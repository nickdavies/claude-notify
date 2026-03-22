# claude-notify

> **Experimental** — This project is under active development. APIs, configuration, and behavior may change without notice.

A notification server for [Claude Code](https://docs.anthropic.com/en/docs/claude-code). It receives webhook events from Claude Code sessions (permission prompts, task completions), gates them against your physical presence, and forwards notifications via [Pushover](https://pushover.net/).

## Why

Claude Code can run autonomously for long stretches, then block on a permission prompt or finish a task while you're away from your desk. The built-in notification hook runs a local shell script — fine for simple cases, but it can't know whether you're actually sitting at your computer.

claude-notify solves this by running as a server (designed for k8s, works anywhere) that:

- **Gates notifications on presence** — a motion sensor (or any HTTP client) posts your presence state; notifications are suppressed while you're present
- **Tracks sessions** — auto-registers Claude Code sessions on first hook contact, with per-session notification control
- **Exposes MCP tools** — Claude itself can query and configure its own notification preferences mid-session
- **Supports multiple clients** — any number of Claude Code instances across machines can report to the same server

## Architecture

```
Claude Code  ──HTTP hooks──▶  claude-notify  ──Pushover API──▶  Phone
     │                             ▲
     ├──MCP (HTTP)──▶  /mcp       │
     │                             │
Motion sensor  ────────────────────┘  POST /presence
```

Single binary (`claude-notify serve`), single process. REST API and MCP protocol are served together.

## Setup

### Requirements

- Rust 1.80+ (or Docker)
- A [Pushover](https://pushover.net/) account (API token + user key)

### Environment variables

| Variable | Required | Default | Description |
|----------|----------|---------|-------------|
| `PUSHOVER_TOKEN` | yes | — | Pushover API token |
| `PUSHOVER_USER` | yes | — | Pushover user key |
| `CLAUDE_NOTIFY_TOKENS` | yes | — | Auth tokens (see [Authentication](#authentication)) |
| `LISTEN_ADDR` | no | `0.0.0.0:8080` | Bind address |
| `PRESENCE_TTL` | no | `120` | Seconds before presence degrades to `away` |
| `SESSION_TTL` | no | `7200` | Seconds before idle sessions are evicted |

### Run locally

```sh
export PUSHOVER_TOKEN="..."
export PUSHOVER_USER="..."
export CLAUDE_NOTIFY_TOKENS="desktop:some-secret-token"

cargo run -- serve
```

### Run with Docker

```sh
docker build -t claude-notify .
docker run -p 8080:8080 \
  -e PUSHOVER_TOKEN="..." \
  -e PUSHOVER_USER="..." \
  -e CLAUDE_NOTIFY_TOKENS="desktop:some-secret-token" \
  claude-notify
```

### Configure Claude Code

Add HTTP hooks to `~/.claude/settings.json`:

```json
{
  "hooks": {
    "Stop": [
      {
        "hooks": [
          {
            "type": "http",
            "url": "https://your-server/hooks/stop",
            "timeout": 5,
            "headers": { "Authorization": "Bearer $CLAUDE_NOTIFY_TOKEN" },
            "allowedEnvVars": ["CLAUDE_NOTIFY_TOKEN"]
          }
        ]
      }
    ],
    "Notification": [
      {
        "matcher": "permission_prompt",
        "hooks": [
          {
            "type": "http",
            "url": "https://your-server/hooks/notification",
            "timeout": 5,
            "headers": { "Authorization": "Bearer $CLAUDE_NOTIFY_TOKEN" },
            "allowedEnvVars": ["CLAUDE_NOTIFY_TOKEN"]
          }
        ]
      }
    ],
    "SessionEnd": [
      {
        "hooks": [
          {
            "type": "http",
            "url": "https://your-server/hooks/session-end",
            "timeout": 5,
            "headers": { "Authorization": "Bearer $CLAUDE_NOTIFY_TOKEN" },
            "allowedEnvVars": ["CLAUDE_NOTIFY_TOKEN"]
          }
        ]
      }
    ]
  }
}
```

Set `CLAUDE_NOTIFY_TOKEN` in your shell profile to match one of the tokens in `CLAUDE_NOTIFY_TOKENS`.

### Enable MCP tools (optional)

Create `~/.claude/.mcp.json`:

```json
{
  "mcpServers": {
    "claude-notify": {
      "type": "http",
      "url": "https://your-server/mcp",
      "headers": {
        "Authorization": "Bearer ${CLAUDE_NOTIFY_TOKEN}"
      }
    }
  }
}
```

This gives Claude access to three tools:

| Tool | Description |
|------|-------------|
| `configure_session` | Toggle stop/permission notifications for a session |
| `list_sessions` | List all active sessions and their config |
| `get_status` | Show presence state, global config, session count |

## API

### Hook endpoints

| Method | Path | Description |
|--------|------|-------------|
| POST | `/hooks/stop` | Agent stopped. Notifies if away, then deregisters session. |
| POST | `/hooks/notification` | Permission prompt. Notifies if away. |
| POST | `/hooks/session-end` | Deregisters session. |

Hook payloads should include `session_id` and `cwd`. Sessions are auto-registered on first contact.

### Management endpoints

| Method | Path | Description |
|--------|------|-------------|
| POST | `/presence` | Update presence state (`present`, `idle`, `away`) |
| GET | `/sessions` | List active sessions |
| PUT | `/sessions/{id}` | Update per-session notification config |
| GET | `/config` | Read global config + presence state |
| PUT | `/config` | Toggle global notification types |
| GET | `/health` | Liveness/readiness (no auth) |
| POST | `/mcp` | MCP Streamable HTTP transport |

All endpoints except `/health` require a valid Bearer token.

## Authentication

There is no token issuing mechanism — you generate tokens yourself and configure both the server and clients with them. Any sufficiently random string works. Use a different token per client so you can revoke or identify them independently.

Generate tokens with `openssl`:

```sh
openssl rand -hex 32
```

Then set `CLAUDE_NOTIFY_TOKENS` as a comma-separated list of `label:token` pairs:

```sh
export CLAUDE_NOTIFY_TOKENS="desktop:$(openssl rand -hex 32),sensor:$(openssl rand -hex 32)"
```

The label is an arbitrary name for logging — it identifies which client made a request. If you omit the label (just a bare token), the token itself is used as the identifier in logs.

Give each client its corresponding token:

- **Claude Code** — set `CLAUDE_NOTIFY_TOKEN` in your shell profile to the `desktop` token
- **Motion sensor** — configure it with the `sensor` token
- **Other machines** — add more `label:token` entries as needed

All endpoints except `/health` require a valid `Authorization: Bearer <token>` header. Invalid or missing tokens return 401.

## Presence

Presence has three states: `present`, `idle`, `away`. The server starts in `away`.

Notifications are only sent when presence is **not** `present`. If no presence update is received within `PRESENCE_TTL` seconds (default 120), the state automatically degrades to `away`.

Post presence from a motion sensor, cron job, or anything that can make an HTTP request:

```sh
curl -X POST https://your-server/presence \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"state": "present"}'
```

## Notification gating

A notification fires only when all three conditions are true:

1. Presence is not `present`
2. The notification type is enabled globally (`GET/PUT /config`)
3. The notification type is enabled for that session (`PUT /sessions/{id}`)

Both global and per-session config default to all notifications enabled.
