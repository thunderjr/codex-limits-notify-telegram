//! Last-seen usage per limit window, persisted as JSON.
//!
//! Written atomically (temp file + rename) so a crash or a kill mid-write
//! can never leave a truncated state file behind.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct State {
    /// `"<limitId>::<slot>"` -> last observed `usedPercent`.
    #[serde(default)]
    pub last_used_percent: BTreeMap<String, f64>,
    /// Unix seconds of the last successful poll.
    #[serde(default)]
    pub last_poll_at: Option<i64>,
    /// History of full-refill notifications we've already sent.
    #[serde(default)]
    pub notified: Vec<NotifyRecord>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct NotifyRecord {
    pub key: String,
    pub label: String,
    pub at: i64,
    pub from_used_percent: f64,
}

const HISTORY_CAP: usize = 200;

impl State {
    pub fn load(path: &Path) -> Self {
        let Ok(raw) = fs::read_to_string(path) else {
            return Self::default(); // first run
        };
        match serde_json::from_str(&raw) {
            Ok(state) => state,
            Err(e) => {
                // Corrupt state must not wedge the watcher; start clean and
                // keep the bad file around for inspection.
                eprintln!("warn: unreadable state at {}: {e} — starting fresh", path.display());
                let _ = fs::rename(path, path.with_extension("json.corrupt"));
                Self::default()
            }
        }
    }

    pub fn record_notification(&mut self, record: NotifyRecord) {
        self.notified.push(record);
        if self.notified.len() > HISTORY_CAP {
            let excess = self.notified.len() - HISTORY_CAP;
            self.notified.drain(0..excess);
        }
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
        }

        let body = serde_json::to_vec_pretty(self).map_err(|e| format!("serialize state: {e}"))?;
        let tmp: PathBuf = path.with_extension("json.tmp");

        {
            let mut f = fs::File::create(&tmp)
                .map_err(|e| format!("cannot create {}: {e}", tmp.display()))?;
            f.write_all(&body)
                .map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
            f.sync_all().map_err(|e| format!("cannot fsync state: {e}"))?;
        }

        fs::rename(&tmp, path).map_err(|e| format!("cannot commit state file: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_disk() {
        let dir = std::env::temp_dir().join(format!("clt-state-{}", std::process::id()));
        let path = dir.join("state.json");

        let mut state = State::default();
        state.last_used_percent.insert("codex::primary".into(), 73.5);
        state.last_poll_at = Some(1_786_000_000);
        state.record_notification(NotifyRecord {
            key: "codex::primary".into(),
            label: "weekly".into(),
            at: 1_786_000_000,
            from_used_percent: 100.0,
        });
        state.save(&path).unwrap();

        let loaded = State::load(&path);
        assert_eq!(loaded.last_used_percent["codex::primary"], 73.5);
        assert_eq!(loaded.last_poll_at, Some(1_786_000_000));
        assert_eq!(loaded.notified.len(), 1);
        assert_eq!(loaded.notified[0].label, "weekly");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_file_yields_default() {
        let state = State::load(Path::new("/nonexistent/clt/state.json"));
        assert!(state.last_used_percent.is_empty());
        assert!(state.last_poll_at.is_none());
    }

    #[test]
    fn history_is_capped() {
        let mut state = State::default();
        for i in 0..(HISTORY_CAP + 50) {
            state.record_notification(NotifyRecord {
                key: "codex::primary".into(),
                label: "weekly".into(),
                at: i as i64,
                from_used_percent: 100.0,
            });
        }
        assert_eq!(state.notified.len(), HISTORY_CAP);
        // Oldest entries dropped, newest kept.
        assert_eq!(state.notified.last().unwrap().at, (HISTORY_CAP + 49) as i64);
    }
}
