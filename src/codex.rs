//! Talks to `codex app-server --stdio` over JSONL and reads rate limits.
//!
//! Never touches the OAuth files under `~/.codex` — the app-server owns auth.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{json, Value};

const RPC_TIMEOUT: Duration = Duration::from_secs(45);

/// One usage window of one limit ID, flattened into a single observation.
#[derive(Debug, Clone)]
pub struct Observation {
    pub limit_id: String,
    /// `primary` / `secondary` — which window slot this came from.
    pub slot: &'static str,
    /// Server-provided name when present, else inferred from the duration.
    pub label: String,
    pub used_percent: f64,
    pub window_duration_mins: Option<u64>,
    pub resets_at: Option<i64>,
}

impl Observation {
    /// Stable key for state tracking: survives renames and unknown IDs.
    pub fn key(&self) -> String {
        format!("{}::{}", self.limit_id, self.slot)
    }

    pub fn remaining_percent(&self) -> f64 {
        (100.0 - self.used_percent).clamp(0.0, 100.0)
    }

    /// A limit is "full" when nothing of it has been consumed.
    pub fn is_full(&self) -> bool {
        self.used_percent <= f64::EPSILON
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Window {
    used_percent: Option<f64>,
    window_duration_mins: Option<u64>,
    resets_at: Option<i64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Limit {
    limit_id: Option<String>,
    limit_name: Option<String>,
    primary: Option<Window>,
    secondary: Option<Window>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RateLimitsResult {
    rate_limits: Option<Limit>,
    #[serde(default)]
    rate_limits_by_limit_id: BTreeMap<String, Limit>,
}

/// Infer a human name from the window duration when the server sends none.
fn infer_name(mins: Option<u64>) -> String {
    match mins {
        Some(m) => match m {
            0 => "instant".to_string(),
            1..=90 => format!("{m}m"),
            91..=1439 if m % 60 == 0 => format!("{}h", m / 60),
            91..=1439 => format!("{}h{}m", m / 60, m % 60),
            1440 => "daily".to_string(),
            10080 => "weekly".to_string(),
            40320..=44640 => "monthly".to_string(),
            _ if m % 1440 == 0 => format!("{}d", m / 1440),
            _ => format!("{}h", m / 60),
        },
        None => "unknown-window".to_string(),
    }
}

fn flatten(id: &str, limit: &Limit, out: &mut Vec<Observation>) {
    let limit_id = limit.limit_id.clone().unwrap_or_else(|| id.to_string());

    for (slot, window) in [("primary", &limit.primary), ("secondary", &limit.secondary)] {
        let Some(w) = window else { continue };
        // A window with no usage figure tells us nothing; skip rather than guess.
        let Some(used_percent) = w.used_percent else {
            continue;
        };

        let label = match limit.limit_name.as_deref() {
            Some(name) if !name.is_empty() => name.to_string(),
            _ => infer_name(w.window_duration_mins),
        };

        out.push(Observation {
            limit_id: limit_id.clone(),
            slot,
            label,
            used_percent,
            window_duration_mins: w.window_duration_mins,
            resets_at: w.resets_at,
        });
    }
}

/// Spawn an app-server, do the JSONL init handshake, read rate limits, exit.
///
/// A fresh child per poll keeps the watcher self-healing: a wedged or
/// upgraded-out-from-under-us app-server can never poison later polls.
pub fn read_rate_limits(codex_bin: &str) -> Result<Vec<Observation>, String> {
    let mut child = Command::new(codex_bin)
        .args(["app-server", "--stdio"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("cannot spawn `{codex_bin} app-server --stdio`: {e}"))?;

    let result = handshake_and_read(&mut child);

    // Always reap the child, even when the exchange failed midway.
    let _ = child.kill();
    let _ = child.wait();

    result
}

fn handshake_and_read(child: &mut Child) -> Result<Vec<Observation>, String> {
    let mut stdin = child.stdin.take().ok_or("app-server stdin unavailable")?;
    let stdout = child.stdout.take().ok_or("app-server stdout unavailable")?;

    // Read lines off-thread so a silent app-server can't hang us forever.
    let (tx, rx) = mpsc::channel::<String>();
    thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });

    let send = |stdin: &mut dyn Write, msg: Value| -> Result<(), String> {
        writeln!(stdin, "{msg}").map_err(|e| format!("write to app-server failed: {e}"))?;
        stdin
            .flush()
            .map_err(|e| format!("flush to app-server failed: {e}"))
    };

    // Await the response with the given id, ignoring interleaved notifications.
    let await_response = |id: i64| -> Result<Value, String> {
        loop {
            let line = rx
                .recv_timeout(RPC_TIMEOUT)
                .map_err(|_| format!("timed out waiting for response to request {id}"))?;

            let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                continue; // not JSON — banner or stray output
            };
            if msg.get("id").and_then(Value::as_i64) != Some(id) {
                continue; // a notification, or someone else's reply
            }
            if let Some(err) = msg.get("error") {
                return Err(format!("app-server error on request {id}: {err}"));
            }
            return msg
                .get("result")
                .cloned()
                .ok_or_else(|| format!("response {id} had no result"));
        }
    };

    send(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "id": 0,
            "method": "initialize",
            "params": {
                "clientInfo": {
                    "name": "codex-limits-telegram",
                    "title": "codex-limits-telegram",
                    "version": env!("CARGO_PKG_VERSION"),
                }
            }
        }),
    )?;
    await_response(0)?;

    send(
        &mut stdin,
        json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }),
    )?;

    send(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "account/rateLimits/read",
            "params": null,
        }),
    )?;
    let result = await_response(1)?;

    let parsed: RateLimitsResult = serde_json::from_value(result)
        .map_err(|e| format!("unexpected account/rateLimits/read shape: {e}"))?;

    let mut observations = Vec::new();
    for (id, limit) in &parsed.rate_limits_by_limit_id {
        flatten(id, limit, &mut observations);
    }
    // Older servers only populate the singular field.
    if observations.is_empty() {
        if let Some(limit) = &parsed.rate_limits {
            flatten("codex", limit, &mut observations);
        }
    }

    if observations.is_empty() {
        return Err("app-server returned no usable rate-limit windows".to_string());
    }

    observations.sort_by_key(|o| o.key());
    Ok(observations)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_inferred_from_duration() {
        assert_eq!(infer_name(Some(10080)), "weekly");
        assert_eq!(infer_name(Some(1440)), "daily");
        assert_eq!(infer_name(Some(300)), "5h");
        assert_eq!(infer_name(Some(30)), "30m");
        assert_eq!(infer_name(None), "unknown-window");
    }

    #[test]
    fn parses_live_response_shape() {
        // Verbatim result payload from codex-cli 0.144.5.
        let raw = r#"{
          "rateLimits": {"limitId":"codex","limitName":null,
            "primary":{"usedPercent":100,"windowDurationMins":10080,"resetsAt":1786160279},
            "secondary":null},
          "rateLimitsByLimitId": {
            "codex_bengalfox":{"limitId":"codex_bengalfox","limitName":"GPT-5.3-Codex-Spark",
              "primary":{"usedPercent":0,"windowDurationMins":10080,"resetsAt":1786655372},
              "secondary":null},
            "codex":{"limitId":"codex","limitName":null,
              "primary":{"usedPercent":100,"windowDurationMins":10080,"resetsAt":1786160279},
              "secondary":null}
          }
        }"#;

        let parsed: RateLimitsResult = serde_json::from_str(raw).unwrap();
        let mut obs = Vec::new();
        for (id, limit) in &parsed.rate_limits_by_limit_id {
            flatten(id, limit, &mut obs);
        }
        obs.sort_by_key(|o| o.key());

        assert_eq!(obs.len(), 2);
        assert_eq!(obs[0].key(), "codex::primary");
        assert_eq!(obs[0].label, "weekly"); // limitName was null -> inferred
        assert!(!obs[0].is_full());
        assert_eq!(obs[0].remaining_percent(), 0.0);

        assert_eq!(obs[1].limit_id, "codex_bengalfox");
        assert_eq!(obs[1].label, "GPT-5.3-Codex-Spark");
        assert!(obs[1].is_full());
    }

    #[test]
    fn unknown_limit_ids_are_retained() {
        let raw = r#"{"rateLimitsByLimitId":{"brand_new_thing":{
            "primary":{"usedPercent":42.5,"windowDurationMins":180}}}}"#;
        let parsed: RateLimitsResult = serde_json::from_str(raw).unwrap();
        let mut obs = Vec::new();
        for (id, limit) in &parsed.rate_limits_by_limit_id {
            flatten(id, limit, &mut obs);
        }
        assert_eq!(obs.len(), 1);
        assert_eq!(obs[0].limit_id, "brand_new_thing");
        assert_eq!(obs[0].label, "3h");
    }
}
