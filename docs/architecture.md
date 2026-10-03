# Architecture

## Goal

Run unattended on any machine (inside a Docker container or as a native systemd service) and make
sure a Claude 5-hour usage window is always active — or, with `ACTIVE_HOURS`, always active during
the hours the user cares about — while spending as few tokens as possible to do it.

**Target plan: Claude Pro** (Max behaves the same). Everything goes through the user's claude.ai
subscription via Claude Code. The Anthropic API (API keys, Console billing) and third-party
providers (Bedrock, Vertex, Foundry) are out of scope and actively refused: they have no 5-hour
window, so a message sent through them would cost money and open nothing.

## Background: the 5-hour window

On Claude subscription plans, usage limits are tracked in rolling 5-hour windows. A window starts
with the first message sent when no window is active and ends 5 hours later; usage then resets.
Starting a window early means it resets early, and chaining windows back to back means a fresh one
is already running whenever the user starts working.

Two facts the design relies on:

- A message sent **inside** an active window does not move its reset time. So a false "no window"
  answer only costs one tiny message; it never shifts anything.
- The window is per account, not per machine or per Claude Code session. Messages from the
  desktop app, claude.ai or another machine all count.

## Design principles

- **Subscription only.** Auth is a claude.ai OAuth login/token; never an API key.
- **Single binary + one external tool.** The Rust binary decides; `claude` sends the message.
  Invoked via `std::process::Command`.
- **Minimal starter.** One message, no context loaded, cheapest model, no tools.
- **Check before start.** Prefer an accurate check; fall back to local state; as a last resort,
  start anyway (harmless, see above).
- **Fail loud, keep running.** Transient failures are logged and retried; only config/preflight
  errors exit non-zero.
- **UTC internally.** `TIMEZONE` only affects `ACTIVE_HOURS`.

## Components (planned)

```
src/
├── main.rs        # CLI (clap), logging, signal handlers, dispatch to app
├── app.rs         # wires everything into the commands: daemon, once, status, check, start
├── config.rs      # env var parsing + validation (Secret type for the token)
├── preflight.rs   # `claude --version`, auth present, DATA_DIR layout (work dir is empty)
├── probe.rs       # trait `UsageProbe`: UsageApiProbe, LocalStateProbe, AutoProbe (chain)
├── starter.rs     # trait `Starter`: ClaudeCliStarter builds and runs the minimal `claude -p`
├── state.rs       # trait `StateStore`: JSON state file in DATA_DIR
├── scheduler.rs   # decide next action + bounded sleep loop
├── hours.rs       # ACTIVE_HOURS parsing and "is now inside" in TIMEZONE
├── retry.rs       # exponential backoff
├── clock.rs       # `Clock` trait: SystemClock (sleep interruptible by SIGTERM), fake for tests
└── exec.rs        # Command wrapper: non-interactive, timeout, captured output, env passthrough
```

## Command-line interface

Configuration always comes from environment variables; the CLI only selects what to do.

| Command                        | Purpose                                                                  |
| ------------------------------ | ------------------------------------------------------------------------ |
| `daemon` (default, no args)    | Startup flow, then the scheduler loop forever.                           |
| `once`                         | Startup flow, one check, start a window if none is active, exit.        |
| `status`                       | Read-only: detection result, `resets_at`, last start, next check.        |
| `check`                        | Validate config, `claude` binary and authentication, then exit.          |
| `start [--force] [--dry-run]`  | Send the starter message. `--force` skips the check. `--dry-run` prints the command and environment (token redacted) and exits. |

## Preflight: subscription auth only

Run at startup (and by `check`), with the same isolated environment as the starter:

1. `claude --version` succeeds.
2. Refuse to start if any of these is set in the service's environment: `ANTHROPIC_API_KEY`,
   `ANTHROPIC_AUTH_TOKEN`, `ANTHROPIC_BASE_URL`, `CLAUDE_CODE_USE_BEDROCK`,
   `CLAUDE_CODE_USE_VERTEX`, `CLAUDE_CODE_USE_FOUNDRY`. Claude Code would prefer them over the
   subscription token. (They are also never passed to the child, see the env table below.)
3. `claude auth status --json` must report:

   ```json
   { "loggedIn": true, "authMethod": "claude.ai", "apiProvider": "firstParty", "subscriptionType": "pro" }
   ```

   `loggedIn`, `authMethod == "claude.ai"` and `apiProvider == "firstParty"` are required.
   `subscriptionType` is logged; `pro` and `max` are accepted, anything else (missing, `free`,
   team/enterprise) is a warning — the window behavior is only verified on Pro.

The status output may contain the account email; it is never logged.

## Detecting an active window

`UsageProbe::check()` returns one of:

- `Active { resets_at }` – a window is running and resets at `resets_at` (UTC).
- `Inactive` – no window is running.
- `Unknown` – the probe couldn't tell (error, unsupported auth).

Strategies, selected by `DETECTION`:

### `oauth-usage`

The `/usage` command in Claude Code reads the subscription's usage (the same numbers shown on
claude.ai → Settings → Usage) from Anthropic's OAuth usage endpoint. Despite the host name this is
not the paid API: it is authenticated with the subscription OAuth token and costs nothing. The probe calls it directly instead of scraping the interactive `/usage` screen:

```
GET https://api.anthropic.com/api/oauth/usage
Authorization: Bearer <oauth access token>
anthropic-beta: oauth-2025-04-20
```

The response contains a `five_hour` object with `utilization` and `resets_at`. If `resets_at` is
set and in the future → `Active`; if it is null or in the past → `Inactive`. Any HTTP/parse error →
`Unknown`.

Caveats, to verify during implementation:

- The endpoint is internal and undocumented; the response shape may change. Parse defensively.
- It needs a token with the `user:profile` scope. A normal `claude /login` token has it; a
  `claude setup-token` token may only have `user:inference`. If it returns 401/403, log once and
  treat as `Unknown` (the `auto` chain then falls back).
- Token source: `CLAUDE_CODE_OAUTH_TOKEN`, else `accessToken` from
  `$CLAUDE_CONFIG_DIR/.credentials.json`. Never log either.

Why not run `/usage` itself: it is an interactive TUI screen, not available as structured output
in `claude -p`; parsing it would be fragile and could cost a session.

### `local`

The state file (`DATA_DIR/state.json`) records every window this service started:

```json
{ "last_start": "2026-10-03T07:00:04Z", "resets_at": "2026-10-03T12:00:04Z", "source": "local" }
```

If `now < resets_at` → `Active`, else `Inactive`. It can't see windows started from other
devices, so it may send one redundant message in a window the user already opened — harmless.

### `auto` (default)

`oauth-usage`; if that returns `Unknown`, `local`. If the last known state is also missing,
`Inactive` (start one).

## Starting a window

`ClaudeCliStarter` runs, with `WORK_DIR` as the current directory:

```bash
claude -p "$STARTER_PROMPT" \
  --model "$STARTER_MODEL" \
  --system-prompt "Reply with one word." \
  --tools "" \
  --strict-mcp-config \
  --setting-sources "" \
  --disable-slash-commands \
  --no-session-persistence \
  --output-format json
```

Environment of the child process (everything else is cleared):

| Variable                  | Value                                        | Why                                              |
| ------------------------- | -------------------------------------------- | ------------------------------------------------ |
| `HOME`                    | `DATA_DIR/home`                              | No `~/.claude/CLAUDE.md`, skills, plugins, hooks |
| `CLAUDE_CONFIG_DIR`       | `DATA_DIR/home/.claude`                      | Same, explicit                                   |
| `CLAUDE_CODE_OAUTH_TOKEN` | from config                                  | Subscription auth without a browser              |
| `DISABLE_AUTOUPDATER`     | `1`                                          | Image pins the version                           |
| `PATH`                    | minimal                                      |                                                  |

Because the environment is built from scratch, `ANTHROPIC_API_KEY` and the other variables from
the preflight list can never reach the child.

Rules:

- `WORK_DIR` (`DATA_DIR/work`) must be empty and must not be inside a git repository, so Claude
  Code finds no `CLAUDE.md`, `.claude/`, `.mcp.json` or git context. Preflight creates it and
  refuses to run if it contains anything.
- `--bare` is **not** used: it ignores OAuth and only accepts `ANTHROPIC_API_KEY`, which would bill
  the API instead of opening a subscription window.
- `--max-budget-usd` is not used either; it is meaningless on a subscription.
- `--system-prompt` replaces Claude Code's large default prompt, which is most of the cost.
- Each flag must be confirmed against the installed `claude --help` when implementing (they are
  checked by `check`). If a flag is unsupported, drop it rather than fail, and log a warning.
- Timeout: 2 minutes. Exit code 0 and a JSON result with `is_error: false` → success.

After success, the service records `last_start = now`, `resets_at = now + 5h` (or the value from the
usage endpoint on the next probe, which wins), and goes back to the scheduler.

Open question to verify early on a Pro account: does a message on the cheapest model (`haiku`)
open the same 5-hour window as Sonnet (Pro's default in Claude Code)? Check with `status` before
and after `start --force`. If not, change the `STARTER_MODEL` default to `sonnet`.

## Scheduler

Loop:

1. If outside `ACTIVE_HOURS`: sleep until the range starts.
2. Probe.
   - `Active { resets_at }` → sleep until `resets_at + 30 s` (grace for clock skew).
   - `Inactive` → start a window, then sleep until the new `resets_at + 30 s`.
   - `Unknown` (only when every strategy failed) → sleep `CHECK_INTERVAL_MINUTES`, retry.
3. Repeat.

Sleeps are done in chunks of ≤ 60 s re-checking the wall clock, so host suspend or clock jumps
don't cause a missed window. If a start fails, retry with backoff (1, 2, 4… up to
`CHECK_INTERVAL_MINUTES`).

With `ACTIVE_HOURS=07:00-23:00`, the first window of the day starts at 07:00 and the last one starts
before 23:00 (the window may run past it). No windows are opened outside the range.

**Shutdown:** the first `SIGTERM`/`SIGINT` sets a flag checked by every sleep (≤ 250 ms latency); a
running `claude` child is given a few seconds and then killed. A second signal exits immediately.

## Failure handling

| Failure                              | Behavior                                                       |
| ------------------------------------ | -------------------------------------------------------------- |
| `claude` missing                     | Exit non-zero at startup.                                      |
| No auth (no token, no credentials)   | Exit non-zero at startup.                                      |
| API key / 3rd-party provider set     | Exit non-zero at startup (subscription only).                  |
| `authMethod` is not `claude.ai`      | Exit non-zero at startup.                                      |
| Token expired / revoked              | `claude` fails with an auth error → log as error, retry every `CHECK_INTERVAL_MINUTES`; `check` reports it. |
| Usage endpoint fails (4xx / 5xx)     | `Unknown` → fall back to local state.                          |
| Network down                         | Start fails → backoff retry.                                   |
| `claude` hangs                       | Killed after the timeout, counted as a failed start.           |
| Rate limited (window already full)   | `claude` reports a limit error → treat as `Active`, sleep `CHECK_INTERVAL_MINUTES`. |
| State file corrupt / missing         | Ignored and rewritten.                                         |

## Logging

Structured logs to stdout (`tracing` + `tracing-subscriber`), level via `RUST_LOG`. Every cycle
logs the probe result and source; every start logs duration, model and the new `resets_at`. Token
values are never logged.

## Suggested crates

- `chrono`, `chrono-tz` – UTC time, `ACTIVE_HOURS` in `TIMEZONE`
- `anyhow` / `thiserror` – error handling
- `tracing`, `tracing-subscriber` – logging
- `signal-hook` – SIGTERM/SIGINT handling
- `clap` – subcommands
- `ureq` (rustls) – usage endpoint request
- `serde`, `serde_json` – state file, usage response, `claude` JSON output

Keep dependencies minimal; the service is synchronous (no async runtime).
