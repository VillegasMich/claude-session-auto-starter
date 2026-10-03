# CLAUDE.md

Guidance for Claude Code (and other AI assistants) working in this repository.

## What this project is

A Rust CLI that also runs as an always-on service (Docker image or native binary under systemd).
Its only job: keep a **Claude 5-hour usage window** open. Claude subscription limits are counted in
rolling 5-hour windows that start with the first message. When no window is active, the service
starts one by sending a single minimal message with `claude -p` from an empty, isolated working
directory, so a window is already running when the user sits down to work.

**Target: Claude Pro subscription** (Max works the same). Claude + Claude Code only — no Anthropic
API usage.

Specs live in `README.md` and `docs/`. **Treat `docs/architecture.md` as the source of truth** for
behavior; update it in the same change when behavior changes. Sibling project with the same shape
(CLI + daemon + Docker + systemd): `../auto-git-commit-tool` — reuse its patterns.

## Commands

```bash
cargo build                    # build
cargo test                     # unit tests (no network, no real `claude` calls)
cargo clippy --all-targets -- -D warnings   # lint (must pass)
cargo fmt                      # format (must be clean)
docker build -t claude-session-starter .
cargo run -- status            # read-only: is a window active, when does it reset
cargo run -- once              # start a window now if none is active, then exit
```

## Hard rules

- **`claude` (Claude Code CLI) is a hard dependency**, invoked via `std::process::Command`. Do not
  call the Messages API directly to start a window: the window must be opened by the user's
  subscription, exactly as Claude Code does. Fail at startup if `claude` is missing.
- **Subscription only, never the API.** Auth is a claude.ai OAuth token (`claude setup-token` →
  `CLAUDE_CODE_OAUTH_TOKEN`). Never add support for `ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN`,
  `--bare`, Bedrock/Vertex/Foundry, or an Anthropic SDK crate. Preflight refuses to start if any of
  those variables is set and requires `claude auth status --json` to report
  `authMethod: "claude.ai"`. The child `claude` gets a from-scratch environment so an API key can
  never leak into it. The OAuth usage endpoint (`api.anthropic.com/api/oauth/usage`) is allowed:
  it is subscription data, not billed API usage.
- **The starter message must cost as few tokens as possible.** It always runs:
  - in `WORK_DIR`, an empty directory owned by the service (never this repo, never a user
    project), so no `CLAUDE.md`, `.claude/` or `.mcp.json` is picked up;
  - with an isolated `HOME`/`CLAUDE_CONFIG_DIR`, so no user-level `CLAUDE.md`, memory, skills,
    plugins, hooks or MCP servers load;
  - with the flags listed in `docs/architecture.md#starting-a-window` (no tools, no MCP, short
    system prompt, cheapest model, no session persistence).
  Any change to that command must keep it minimal; never add context to it.
- **Never start a second window on purpose.** Check first (see detection strategies in
  `docs/architecture.md`). A message sent inside an active window is harmless (it does not reset
  the window) but is wasted usage, so it must only happen as a fallback when detection is
  impossible.
- **Never log or print `CLAUDE_CODE_OAUTH_TOKEN`** or the contents of `.credentials.json`, and
  never pass a token on a command line (it leaks via `ps`). Pass it to child processes through
  the environment only. Wrap secrets in a type whose `Debug` is redacted.
- **Don't crash on transient failures** (network, 5xx, `claude` exiting non-zero once). Log,
  retry with backoff, continue. Only config/preflight errors exit non-zero.
- **Synchronous code.** No async runtime; `std::thread::sleep` in bounded chunks (≤ 60 s,
  interruptible by signals) is sufficient.
- Handle `SIGTERM` cleanly — in the container `tini` is PID 1 and forwards it to the binary.
- Times: store and compare in UTC (`chrono::Utc`). Only `ACTIVE_HOURS` is interpreted in
  `TIMEZONE`.

## Conventions

- Rust edition 2024. Errors: `anyhow` at the top level, `thiserror` for module error types if needed.
- Logging: `tracing`, level via `RUST_LOG`, to stdout.
- Config only from environment variables (see `docs/configuration.md`). Add new settings there, in
  the README table and in `.env.example` (once it exists).
- Keep external interactions behind thin traits so the scheduler is unit tested with fakes:
  `Clock` (time + sleep), `UsageProbe` (is a window active / when does it reset), `Starter`
  (send the starter message), `StateStore` (last known window).
- Pure logic worth testing: next-check computation, `ACTIVE_HOURS` parsing and matching, config
  validation, parsing of the usage response and of `claude --output-format json` output.
- Commit messages: Conventional Commits, validated against commitlint `@commitlint/config-conventional`.

## Commit message recommendation (required after every change)

At the end of **every** response that modifies files, recommend a commit message. Do not commit
unless explicitly asked — only suggest.

1. Inspect what is not yet staged/committed: `git status --short`, `git diff`, and untracked files
   (`git ls-files --others --exclude-standard`). Base the message on these changes only.
2. Follow commitlint `config-conventional`: header `type(scope?): subject`, max 100 characters,
   type one of `build`, `chore`, `ci`, `docs`, `feat`, `fix`, `perf`, `refactor`, `revert`,
   `style`, `test`; imperative lower-case subject, no trailing period; optional body (why, lines
   ≤ 100 chars) and footer separated by blank lines.
3. If changes are unrelated, suggest splitting them into several commits with the files for each.
4. Present it in a code block, ready to copy.

## Testing notes

- Unit tests never call the real `claude` binary or the network. Use the trait fakes.
- Manual end-to-end check: `cargo run -- status`, then `cargo run -- once` (this really sends one
  minimal message on the configured account).
- `cargo run -- start --dry-run` prints the exact `claude` command and environment (token
  redacted) without running it.
