# Configuration

All settings come from environment variables. Invalid values abort startup with a clear error.

## Authentication

### `CLAUDE_CODE_OAUTH_TOKEN` (required)

Long-lived **Claude Pro** (or Max) subscription token created with `claude setup-token` on a
machine with a browser. It is passed to the `claude` child process through its environment and
never logged.

This is the only supported authentication. `ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN`,
`ANTHROPIC_BASE_URL` and the Bedrock/Vertex/Foundry switches are rejected at startup: API usage is
billed separately and has no 5-hour window.

It is required in native mode too: the starter runs with an isolated `CLAUDE_CONFIG_DIR`, so it
can't see your normal `claude` login (by design — that is what keeps your `CLAUDE.md`, plugins
and MCP servers out of it).

## Scheduling

### `CHECK_INTERVAL_MINUTES` (default: `5`)

How often to probe when the window state is unknown, and the upper bound of the retry backoff
after a failed start. `1`–`60`. While a window is known to be active, the service sleeps until it
resets instead.

### `ACTIVE_HOURS` (default: unset = all day)

`HH:MM-HH:MM` range, interpreted in `TIMEZONE`, in which new windows may be started. Example:
`07:00-23:00`. A range crossing midnight (`22:00-06:00`) is allowed. Outside the range the service
only sleeps.

Tip: set the start to roughly when you begin working minus a little, so the first window resets
early in your day.

### `TIMEZONE` (default: `UTC`)

IANA timezone name for `ACTIVE_HOURS`, e.g. `America/Bogota`, `Europe/Madrid`. The container's
`TZ` is ignored.

## Detection

### `DETECTION` (default: `auto`)

| Value         | Meaning                                                                       |
| ------------- | ----------------------------------------------------------------------------- |
| `auto`        | Subscription usage endpoint, falling back to local state.                     |
| `oauth-usage` | Only the subscription usage endpoint; if it fails, wait and retry.            |
| `local`       | Only the state file; can't see windows started on other devices.              |

See [architecture.md](architecture.md#detecting-an-active-window).

## Starter message

### `STARTER_MODEL` (default: `haiku`)

Model alias or full name passed to `claude --model`. The cheapest model that still opens the
window on your plan. Must be a model available to Claude Code on Pro (`haiku` or `sonnet`).

### `STARTER_PROMPT` (default: `hi`)

The message sent. Keep it short; the reply is limited by the system prompt to one word.

## Paths

### `DATA_DIR` (default: `/data`)

Holds everything the service writes:

```text
$DATA_DIR/
├── state.json          # last window started, last probe result
├── home/               # HOME for the claude child process
│   └── .claude/        # CLAUDE_CONFIG_DIR: isolated, no CLAUDE.md, skills, plugins or MCP
└── work/               # WORK_DIR: always empty, cwd of the starter message
```

Mount a volume here in Docker. `work/` must stay empty; startup fails if it isn't.

## Logging

### `RUST_LOG` (default: `info`)

Log filter for `tracing-subscriber` (`error`, `warn`, `info`, `debug`, `trace`).

## Example `.env`

```dotenv
CLAUDE_CODE_OAUTH_TOKEN=sk-ant-oat01-xxxxxxxx
ACTIVE_HOURS=07:00-23:00
TIMEZONE=America/Bogota
```

Never commit a real `.env` file — it is listed in `.gitignore`.
