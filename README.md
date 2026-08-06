# codex-limits-telegram

Watches Codex CLI rate limits and sends a Telegram message the moment a limit
refills to **100% available**. Polls every 10 minutes, remembers the last
reading, and notifies only on an `any → 100%` transition.

## How it works

Each poll spawns `codex app-server --stdio`, performs the JSONL init handshake,
and calls `account/rateLimits/read` with `params: null`:

```
→ {"jsonrpc":"2.0","id":0,"method":"initialize","params":{"clientInfo":{…}}}
← {"id":0,"result":{…}}
→ {"jsonrpc":"2.0","method":"initialized","params":{}}
→ {"jsonrpc":"2.0","id":1,"method":"account/rateLimits/read","params":null}
← {"id":1,"result":{"rateLimits":{…},"rateLimitsByLimitId":{…}}}
```

The API reports **`usedPercent`**, so "back to 100%" means `usedPercent == 0`.

Every entry in `rateLimitsByLimitId` is tracked independently, and both the
`primary` and `secondary` usage windows of each, keyed as `<limitId>::<slot>`.
Unknown limit IDs are retained rather than dropped, so a new model tier starts
being watched automatically. Window names come from `limitName` when the server
sends one, otherwise they're inferred from `windowDurationMins`
(`10080 → weekly`, `1440 → daily`, `300 → 5h`, …).

OAuth files under `~/.codex` are never read — the app-server owns auth.

A fresh app-server per poll (rather than one long-lived process) keeps the
watcher self-healing: a wedged or upgraded-out-from-under-us app-server can't
poison later polls.

## Notification rule

| Previous | Now | Notify? |
|---|---|---|
| 100% used | 0% used | ✅ yes — the case you want |
| 12% used | 0% used | ✅ yes |
| 0% used | 0% used | ❌ no — no repeat pings every 10 min |
| *(first run)* | 0% used | ❌ no — baseline only |
| 40% used | 55% used | ❌ no |
| 100% used | 3% used | ❌ no — refilled, but not fully |

If the Telegram send fails, that limit's saved value is **not** advanced, so the
notification is retried on the next poll instead of being lost.

## Configuration

Read from the environment, falling back to `./.env` (real env wins, so systemd
can override):

| Variable | Required | Default |
|---|---|---|
| `TELEGRAM_BOT_TOKEN` | yes | — |
| `TELEGRAM_CHAT_ID` | yes | — |
| `POLL_INTERVAL_SECS` | no | `600` (10 min) |
| `CODEX_BIN` | no | `codex` |
| `STATE_PATH` | no | `$XDG_STATE_HOME/codex-limits-telegram/state.json` |

State is written atomically (temp file + rename), so a crash mid-write can't
leave a truncated file. A corrupt state file is moved aside and a fresh
baseline is taken rather than wedging the watcher.

## Usage

```sh
cargo build --release

./target/release/codex-limits-telegram --test-telegram   # verify bot wiring
./target/release/codex-limits-telegram --once            # one poll, then exit
./target/release/codex-limits-telegram                   # run the 10m loop
```

## Run under systemd --user

```sh
mkdir -p ~/.config/systemd/user
ln -sf ~/projects/codex-limits-telegram/codex-limits-telegram.service \
       ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now codex-limits-telegram
journalctl --user -u codex-limits-telegram -f
```

To keep it running while you're logged out:

```sh
sudo loginctl enable-linger "$USER"
```

## Troubleshooting

**`chat not found`** — a bot cannot start a conversation with you. Open the bot
in Telegram and send `/start` (or any message) once, then retry. To confirm the
ID Telegram actually sees:

```sh
set -a; . ./.env; set +a
curl -s "https://api.telegram.org/bot$TELEGRAM_BOT_TOKEN/getUpdates" \
  | grep -o '"chat":{"id":[-0-9]*'
```

For a group, add the bot to the group and use the group's negative ID.
