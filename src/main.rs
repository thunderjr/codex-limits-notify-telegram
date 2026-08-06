//! Watches Codex CLI rate limits and pings Telegram when one refills to 100%.
//!
//! Polls `codex app-server --stdio` every 10 minutes, persists the last
//! `usedPercent` per limit window, and notifies only on an
//! `anything -> 100% available` transition.

mod codex;
mod state;
mod telegram;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use codex::Observation;
use state::{NotifyRecord, State};
use telegram::Telegram;

const DEFAULT_INTERVAL_SECS: u64 = 600; // 10 minutes
/// Backoff after a failed poll so a flaky app-server doesn't wait a full cycle.
const RETRY_SECS: u64 = 60;

struct Config {
    token: String,
    chat_id: String,
    codex_bin: String,
    state_path: PathBuf,
    interval: Duration,
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Parse a `.env` file: `KEY=value`, `export KEY=value`, `#` comments,
/// optional surrounding quotes. Values are never logged.
fn load_dotenv(path: &PathBuf) -> BTreeMap<String, String> {
    let mut vars = BTreeMap::new();
    let Ok(raw) = std::fs::read_to_string(path) else {
        return vars;
    };

    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };

        let value = value.trim();
        let value = value
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
            .unwrap_or(value);

        vars.insert(key.trim().to_string(), value.to_string());
    }
    vars
}

impl Config {
    fn load() -> Result<Self, String> {
        let dotenv = load_dotenv(&PathBuf::from(".env"));
        // Real environment wins over `.env`, so systemd can override.
        let get = |key: &str| {
            std::env::var(key)
                .ok()
                .filter(|v| !v.is_empty())
                .or_else(|| dotenv.get(key).cloned().filter(|v| !v.is_empty()))
        };

        let token = get("TELEGRAM_BOT_TOKEN")
            .ok_or("TELEGRAM_BOT_TOKEN is not set (checked environment and ./.env)")?;
        let chat_id = get("TELEGRAM_CHAT_ID")
            .ok_or("TELEGRAM_CHAT_ID is not set (checked environment and ./.env)")?;

        let state_path = get("STATE_PATH").map(PathBuf::from).unwrap_or_else(|| {
            let base = get("XDG_STATE_HOME")
                .map(PathBuf::from)
                .or_else(|| get("HOME").map(|h| PathBuf::from(h).join(".local/state")))
                .unwrap_or_else(|| PathBuf::from("."));
            base.join("codex-limits-telegram/state.json")
        });

        let interval = get("POLL_INTERVAL_SECS")
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|s| *s > 0)
            .unwrap_or(DEFAULT_INTERVAL_SECS);

        Ok(Self {
            token,
            chat_id,
            codex_bin: get("CODEX_BIN").unwrap_or_else(|| "codex".to_string()),
            state_path,
            interval: Duration::from_secs(interval),
        })
    }
}

fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// "6d 4h" / "3h 12m" / "less than a minute"
fn human_duration(secs: i64) -> String {
    if secs <= 60 {
        return "less than a minute".to_string();
    }
    let (d, h, m) = (secs / 86400, (secs % 86400) / 3600, (secs % 3600) / 60);
    match (d, h) {
        (0, 0) => format!("{m}m"),
        (0, _) => format!("{h}h {m}m"),
        _ => format!("{d}d {h}h"),
    }
}

fn refill_message(obs: &Observation, previous_used: f64) -> String {
    let mut msg = format!(
        "✅ <b>Codex limit back to 100%</b>\n\n\
         <b>Limit:</b> {}\n\
         <b>Window:</b> {}\n\
         <b>Was:</b> {:.0}% used → <b>now:</b> 0% used (100% available)",
        escape_html(&obs.limit_id),
        escape_html(&obs.label),
        previous_used,
    );

    if let Some(mins) = obs.window_duration_mins {
        msg.push_str(&format!("\n<b>Window length:</b> {mins} min"));
    }
    if let Some(resets_at) = obs.resets_at {
        let remaining = resets_at - now();
        if remaining > 0 {
            msg.push_str(&format!(
                "\n<b>Next reset:</b> in {}",
                human_duration(remaining)
            ));
        }
    }
    msg
}

/// Compare a fresh poll against saved state, notify on refills, save.
///
/// Returns the number of notifications sent.
fn reconcile(
    observations: &[Observation],
    state: &mut State,
    bot: &Telegram,
    config: &Config,
) -> Result<usize, String> {
    let first_run = state.last_used_percent.is_empty();
    let mut sent = 0;

    for obs in observations {
        let key = obs.key();
        let previous = state.last_used_percent.get(&key).copied();

        // Notify only on a genuine transition into "full". A first sighting is
        // a baseline, not a change — otherwise every fresh install pings.
        let refilled = match previous {
            Some(prev) => prev > f64::EPSILON && obs.is_full(),
            None => false,
        };

        if refilled {
            let prev = previous.unwrap_or(0.0);
            match bot.send(&refill_message(obs, prev)) {
                Ok(()) => {
                    println!("notified: {key} refilled ({prev:.0}% used -> 0%)");
                    state.record_notification(NotifyRecord {
                        key: key.clone(),
                        label: obs.label.clone(),
                        at: now(),
                        from_used_percent: prev,
                    });
                    sent += 1;
                }
                Err(e) => {
                    // Don't advance this key's state — retry the notification
                    // on the next poll instead of losing it silently.
                    eprintln!("error: telegram notify failed for {key}: {e}");
                    continue;
                }
            }
        }

        state.last_used_percent.insert(key, obs.used_percent);
    }

    state.last_poll_at = Some(now());
    state.save(&config.state_path)?;

    if first_run {
        println!(
            "baseline recorded for {} limit window(s); no notifications on first run",
            observations.len()
        );
    }
    Ok(sent)
}

fn poll_once(config: &Config, bot: &Telegram) -> Result<(), String> {
    let observations = codex::read_rate_limits(&config.codex_bin)?;

    for obs in &observations {
        println!(
            "  {} [{}] {:.0}% used / {:.0}% available",
            obs.limit_id,
            obs.label,
            obs.used_percent,
            obs.remaining_percent(),
        );
    }

    let mut state = State::load(&config.state_path);
    reconcile(&observations, &mut state, bot, config)?;
    Ok(())
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let once = args.iter().any(|a| a == "--once");
    let test_telegram = args.iter().any(|a| a == "--test-telegram");

    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!(
            "codex-limits-telegram — notify on Codex limit refills\n\n\
             USAGE:\n  codex-limits-telegram [--once] [--test-telegram]\n\n\
             FLAGS:\n\
             \x20 --once            poll a single time and exit\n\
             \x20 --test-telegram   send a test message and exit\n\n\
             ENV (or ./.env):\n\
             \x20 TELEGRAM_BOT_TOKEN, TELEGRAM_CHAT_ID   required\n\
             \x20 POLL_INTERVAL_SECS                     default {DEFAULT_INTERVAL_SECS}\n\
             \x20 CODEX_BIN                              default \"codex\"\n\
             \x20 STATE_PATH                             default $XDG_STATE_HOME/codex-limits-telegram/state.json"
        );
        return;
    }

    let config = match Config::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    };
    let bot = Telegram::new(config.token.clone(), config.chat_id.clone());

    if test_telegram {
        match bot.send("🔔 codex-limits-telegram test message — the bot is wired up.") {
            Ok(()) => println!("test message sent"),
            Err(e) => {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
        return;
    }

    println!("state file: {}", config.state_path.display());

    if once {
        if let Err(e) = poll_once(&config, &bot) {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
        return;
    }

    println!("polling every {}s (Ctrl-C to stop)", config.interval.as_secs());
    loop {
        // A failed poll is never fatal — this runs unattended for weeks.
        let wait = match poll_once(&config, &bot) {
            Ok(()) => config.interval,
            Err(e) => {
                eprintln!("error: poll failed: {e}");
                Duration::from_secs(RETRY_SECS.min(config.interval.as_secs()))
            }
        };
        std::thread::sleep(wait);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dotenv_parsing_handles_quotes_exports_and_comments() {
        let dir = std::env::temp_dir().join(format!("clt-env-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(".env");
        std::fs::write(
            &path,
            "# a comment\nTELEGRAM_BOT_TOKEN=123:abc\nexport TELEGRAM_CHAT_ID=\"-100999\"\n\
             QUOTED='single'\nEMPTY=\n",
        )
        .unwrap();

        let vars = load_dotenv(&path);
        assert_eq!(vars["TELEGRAM_BOT_TOKEN"], "123:abc");
        assert_eq!(vars["TELEGRAM_CHAT_ID"], "-100999");
        assert_eq!(vars["QUOTED"], "single");
        assert_eq!(vars["EMPTY"], "");
        assert!(!vars.contains_key("# a comment"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn human_duration_reads_naturally() {
        assert_eq!(human_duration(30), "less than a minute");
        assert_eq!(human_duration(900), "15m");
        assert_eq!(human_duration(11_520), "3h 12m");
        assert_eq!(human_duration(533_000), "6d 4h");
    }

    #[test]
    fn message_escapes_html_from_server_fields() {
        let obs = Observation {
            limit_id: "a<b>&c".into(),
            slot: "primary",
            label: "<weekly>".into(),
            used_percent: 0.0,
            window_duration_mins: Some(10080),
            resets_at: None,
        };
        let msg = refill_message(&obs, 100.0);
        assert!(msg.contains("a&lt;b&gt;&amp;c"));
        assert!(msg.contains("&lt;weekly&gt;"));
        assert!(!msg.contains("<weekly>"));
    }

    // --- transition logic -------------------------------------------------
    // Mirrors `reconcile`'s decision so the rule is pinned by tests without
    // needing a live app-server or network.
    fn should_notify(previous: Option<f64>, used_now: f64) -> bool {
        match previous {
            Some(prev) => prev > f64::EPSILON && used_now <= f64::EPSILON,
            None => false,
        }
    }

    #[test]
    fn notifies_only_on_transition_into_full() {
        // exhausted -> refilled: the case we care about
        assert!(should_notify(Some(100.0), 0.0));
        // partially used -> refilled
        assert!(should_notify(Some(12.5), 0.0));

        // already full, stays full: no repeat pings every 10 minutes
        assert!(!should_notify(Some(0.0), 0.0));
        // first sighting is a baseline, not a change
        assert!(!should_notify(None, 0.0));
        // still being consumed
        assert!(!should_notify(Some(40.0), 55.0));
        // refilled but not fully
        assert!(!should_notify(Some(100.0), 3.0));
    }

    #[test]
    fn full_cycle_notifies_exactly_once() {
        // Simulated week: burn down, reset, idle at full.
        let readings = [50.0, 80.0, 100.0, 100.0, 0.0, 0.0, 0.0, 20.0, 0.0];
        let mut previous: Option<f64> = None;
        let mut notifications = 0;

        for used in readings {
            if should_notify(previous, used) {
                notifications += 1;
            }
            previous = Some(used);
        }
        // Once at the reset after 100%, once after the later 20% dip.
        assert_eq!(notifications, 2);
    }
}
