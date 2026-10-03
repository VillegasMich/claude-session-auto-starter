# claude-session-starter

A small CLI and always-on service that keeps a **Claude 5-hour usage window** running.

Claude subscription plans count usage in rolling **5-hour windows** that start with the first
message you send. If the first message of the day is at 10:00, the window resets at 15:00 — no
matter when you actually hit the limit. This tool checks whether a window is active and, if not,
starts one by sending a single, minimal message through Claude Code. Windows then chain back to back,
so when you start working a window is already running and resets sooner.

**Built for the Claude Pro subscription** (Max works the same way). It only uses your claude.ai
subscription through Claude Code — **no Anthropic API key, no API billing**. It refuses to run if
Claude Code would authenticate with an API key or a third-party provider instead.

It is written in Rust, ships as a Docker image (or a native binary), runs as a systemd service, and
uses the [Claude Code CLI (`claude`)](https://docs.claude.com/en/docs/claude-code) as a hard
dependency to send the message.

See [`docs/`](docs/) for the detailed design.

## How it works

1. **Startup** – verifies that `claude` is installed and logged in to a claude.ai subscription:
   `claude auth status --json` must report `authMethod: "claude.ai"` and a `subscriptionType`
   (e.g. `pro`). See [Authentication](#authentication).
2. **Check** – every `CHECK_INTERVAL_MINUTES` (default 5) it asks: *is a 5-hour window active?*
   - Primary: the same usage data the `/usage` command shows (`five_hour.resets_at`).
   - Fallback: its own record of the last window it started.
3. **Start** – if no window is active (and the current time is inside `ACTIVE_HOURS`), it runs
   `claude -p` once from an empty, isolated directory: no `CLAUDE.md`, no settings, no tools, no
   MCP servers, short system prompt, cheapest model. Just enough to open the window.
4. **Sleep** – until the window resets (`resets_at`), then repeats.

A message sent inside an already active window does not reset it, so the worst case of a wrong
check is a few wasted tokens, never a shifted window.

## Quick start

### With Docker

```bash
claude setup-token                          # once, on a machine with a browser; copy the token
docker build -t claude-session-starter .

docker run -d \
  --name claude-session-starter \
  --restart unless-stopped \
  -e CLAUDE_CODE_OAUTH_TOKEN=sk-ant-oat01-xxxxxxxx \
  -v claude-session-starter-data:/data \
  claude-session-starter
```

### As a systemd service (planned)

Same model as `auto-git-commit-tool`: `scripts/install.sh [docker|native]` writes
`/etc/claude-session-starter/env` (mode 600) and installs a unit that starts on boot.
Details: [`docs/deployment.md`](docs/deployment.md).

## Command line

```text
claude-session-starter [daemon]        # run forever: check, start a window when none is active (default)
claude-session-starter once            # check now, start a window if none is active, exit
claude-session-starter status          # read-only: window active?, resets at, last start, next check
claude-session-starter check           # validate config, `claude` binary and authentication
claude-session-starter start [--force] [--dry-run]
                                       # send the starter message (--force skips the check,
                                       # --dry-run prints the command without running it)
```

## Configuration

All configuration is done through environment variables.

| Variable                  | Default                 | Description                                                         |
| ------------------------- | ----------------------- | ------------------------------------------------------------------- |
| `CLAUDE_CODE_OAUTH_TOKEN` | **required**            | Long-lived subscription token from `claude setup-token`.            |
| `CHECK_INTERVAL_MINUTES`  | `5`                     | How often to check when no window is known to be active.           |
| `ACTIVE_HOURS`            | unset (all day)         | Only start windows inside this range, e.g. `07:00-23:00`.           |
| `TIMEZONE`                | `UTC`                   | IANA zone used for `ACTIVE_HOURS`, e.g. `America/Bogota`.           |
| `DETECTION`               | `auto`                  | `auto`, `oauth-usage` or `local`. See architecture docs.            |
| `STARTER_MODEL`           | `haiku`                 | Model alias passed to `claude --model`.                             |
| `STARTER_PROMPT`          | `hi`                    | The message sent to open the window.                                |
| `DATA_DIR`                | `/data`                 | State file and the isolated Claude config/work directories.         |
| `RUST_LOG`                | `info`                  | Log level.                                                          |

Full details: [`docs/configuration.md`](docs/configuration.md).

## Authentication

Requires a **Claude Pro** (or Max) subscription. API keys are not supported and are rejected at
startup: API usage is billed per token and has no 5-hour window to start.

The container has no browser, so it can't run `claude /login`. Use a long-lived subscription token
instead:

```bash
claude setup-token     # on your machine; prints a token valid for ~1 year
```

Pass it as `CLAUDE_CODE_OAUTH_TOKEN` (env file or Docker secret). Never bake it into the image or
pass it on a command line. It is needed natively too, because the starter runs with an isolated
Claude config directory that cannot see your normal login.

## Documentation

- [`docs/architecture.md`](docs/architecture.md) – components, detection strategies, starter command, scheduling, failure handling
- [`docs/configuration.md`](docs/configuration.md) – every setting in detail
- [`docs/deployment.md`](docs/deployment.md) – Docker image, authentication, systemd
- [`CLAUDE.md`](CLAUDE.md) – guidance for AI coding assistants working in this repo

## Development

```bash
cargo build
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt
```

Running locally requires `claude` on your `PATH` and either a normal `claude` login or
`CLAUDE_CODE_OAUTH_TOKEN` exported:

```bash
DATA_DIR=/tmp/css cargo run -- status
DATA_DIR=/tmp/css cargo run -- start --dry-run
```

## Disclaimer

Not affiliated with Anthropic. The usage endpoint used for detection is the one Claude Code itself
uses for `/usage`; it is not a public API and may change. Make sure automated use fits your plan's
terms.
