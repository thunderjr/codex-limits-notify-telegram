//! Minimal Telegram Bot API client — one method, `sendMessage`.

use std::time::Duration;

use serde_json::json;

pub struct Telegram {
    token: String,
    chat_id: String,
}

impl Telegram {
    pub fn new(token: String, chat_id: String) -> Self {
        Self { token, chat_id }
    }

    pub fn send(&self, text: &str) -> Result<(), String> {
        let url = format!("https://api.telegram.org/bot{}/sendMessage", self.token);

        let response = ureq::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .post(&url)
            .send_json(json!({
                "chat_id": self.chat_id,
                "text": text,
                "parse_mode": "HTML",
                "disable_web_page_preview": true,
            }));

        match response {
            Ok(_) => Ok(()),
            // Surface Telegram's own description, but never the URL — it
            // carries the bot token.
            Err(ureq::Error::Status(code, res)) => {
                let body = res.into_string().unwrap_or_default();
                let detail = serde_json::from_str::<serde_json::Value>(&body)
                    .ok()
                    .and_then(|v| {
                        v.get("description")
                            .and_then(|d| d.as_str())
                            .map(str::to_string)
                    })
                    .unwrap_or(body);
                Err(format!("telegram sendMessage returned {code}: {detail}"))
            }
            Err(ureq::Error::Transport(t)) => {
                Err(format!("telegram request failed: {}", t.kind()))
            }
        }
    }
}
