# Backend

One Rust server, `botd`. It serves the web app, stores Bot and chat records in SQLite, and runs each Bot on its selected harness.

## How it works

- **Bots** live in SQLite. Each Bot has a harness (`claude`, `codex`, `pi`, or `custom`), an optional model, and a CLI harness session id.
- **Teams** group Bots in the sidebar. A Bot has one optional team assignment in SQLite. The sidebar remembers collapsed sections in the current browser.
- **A message** is queued. Each Bot runs one task at a time, in order. Different Bots run in parallel.
- **Pause** lets the current task finish and blocks new chats, scheduled routines, and delegated tasks. Queued messages stay in history and retire from the execution queue. Resuming accepts future work.
- **A run** invokes the harness, streams its events, and saves each text block as a chat bubble.
- **Live updates** go to the browser over one Server-Sent Events stream: new messages, streaming text, and what the Bot is doing ("Running: ls").
- **Memory** is plain files, re-sent in the system prompt every run. The Bot edits them itself:
  - `DATA_DIR/bots/<id>/MEMORY.md`: one Bot's job, rules, and work log.
  - `DATA_DIR/USER.md`: facts about the user, shared by all Bots.
- **The computer** is `WORKSPACE_DIR`, shared by all Bots, like Grok Bot's shared VM.
- **Routines** are cron schedules. Bots create them with the `botctl` CLI, which works from any harness's shell.
- **Bot to Bot**: a Bot runs `botctl ask "<Bot>" "<message>"`. The other Bot answers in its own chat and the reply prints back. If it takes over 9 minutes, the reply arrives later as a new message.
- **Model and effort** are saved per Bot. The web app shows a faint picker above the input. Unset means the harness default.
- **Choice cards**: a reply ending in a ` ```question ` JSON block becomes a multiple-choice card.
- **Restarts**: queued messages run after a restart. A task that was running is reported as interrupted, not re-run, so it can't repeat an action like sending an email.

## Harness adapters

CLI adapters in `server/src/adapters/` build command lines and map JSON lines to the runner's events. The native custom adapter emits the same text, tool and error events from an HTTP stream.

- **Claude Code**: `claude -p --output-format stream-json --resume <id>`. System prompt via `--append-system-prompt`, fresh every run.
- **Codex**: `codex exec --json`, then `codex exec resume <id>`. Instructions via `-c developer_instructions=...`. Codex only reads them when a session starts, so later memory edits reach it through the memory file.
- **Pi**: `pi --mode json --session-dir <bot dir> --session-id <id>`. System prompt via `--append-system-prompt`.
- **[Custom (OpenAI Compatible)](custom-provider.md)** uses streaming Chat Completions, stores recent conversation turns per Bot, and exposes a subset of SuperBot tools as functions.
- **[Managed MCP services](mcp.md)** use server-side connections and per-Bot grants. CLI harnesses use `botctl mcp`. Custom Bots use function tools when configured.

All three run with permission prompts off (`--dangerously-skip-permissions` and equivalents). The server is the computer, so treat it that way. Approvals are not built yet.

For a new CLI harness, add an adapter file and register it in `get()`, `CLI_NAMES` and `NAMES` in `adapters/mod.rs`.

## Run it

```sh
cargo build --release          # makes target/release/botd and target/release/botctl
cp .env.example .env           # fill it in
set -a; . ./.env; set +a
./target/release/botd
```

Settings are in [.env.example](../.env.example). Keep `botctl` next to `botd`: the server puts that folder on the Bots' PATH.

## Deploying on a Linux VPS

- Keep your server IP, domain, and secrets out of this repo. They go in `.env` on the server.

- Install the harness CLIs you want and log them in **as the user that runs `botd`**: `claude` (then `claude login` or `ANTHROPIC_API_KEY`), `codex` (`codex login` or `OPENAI_API_KEY`), `pi` (provider API key, e.g. `OPENROUTER_API_KEY`). Their env vars pass through to the Bots.
- Run `botd` as a normal user, not root. Claude Code refuses to skip permissions as root.
- Run it under systemd with `Restart=always`. It shuts down cleanly on SIGTERM.
- Put HTTPS in front (e.g. Caddy) and set `AUTH_PASSWORD` and `HOST=0.0.0.0`, or keep `HOST=127.0.0.1` behind the proxy. The browser logs in with any username and that password.
- Proxies must not buffer `/api/events` (it's an SSE stream).
- Set `TZ` to the user's time zone. Routines use it.
- Back up `DATA_DIR`. `WORKSPACE_DIR` is the Bots' scratch and files.
- `GET /healthz` returns `{"ok":true}` without auth.

## API

All JSON. Browser auth: HTTP Basic with `AUTH_PASSWORD`.

- `GET /api/state`: user name, installed harnesses, all Bots and teams
- `GET /api/events`: SSE stream (`message`, `draft`, `status`, `bot`, `botDeleted`, `team`, `teamDeleted`, `resync`)
- `POST /api/bots` `{name, harness?, model?, color?, shape?}`
- `PATCH /api/bots/:id` `{name?, hidden?, harness?, model?, effort?}`, `DELETE /api/bots/:id`
- `POST /api/teams` and `PATCH /api/teams/:id` accept `{name}`. `DELETE /api/teams/:id` returns 409 while Bots are assigned.
- `PUT /api/bots/:id/team` accepts `{teamId}`. Use `null` to move a Bot to Unassigned.
- `GET /api/harnesses/:name/models`: models and reasoning efforts the harness offers. For `custom`, this returns the configured default model.
- `GET|POST /api/bots/:id/messages` `{text, answer?: {messageId, option}}`
- `POST /api/bots/:id/read`, `POST /api/bots/:id/stop`
- `PUT /api/bots/:id/pause` accepts `{paused}`. Paused Bots return 409 for new messages and delegation. A reply to an outstanding screen handoff can finish the active task.
- `GET|POST /api/bots/:id/routines` `{name, cron, instruction}`
- `PATCH|DELETE /api/routines/:id` `{enabled?, name?, cron?, instruction?}`
- `GET /api/bots/:id/screen`: starts the Bot's screen and returns its live view URL
- `POST /api/handoff` `{from, text}`: `botctl handoff`; waits for the user's answer

## Not built yet

Approvals before risky actions, Vault for secrets, per-Bot resource limits, Slack.

Computer use (each Bot's screen, browser, and live view) is in [computer.md](computer.md).
