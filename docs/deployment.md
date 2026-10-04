# Deployment

The service is designed to run on an always-on Linux machine (home server, VPS, Raspberry Pi,
NAS…), started on boot. Two modes, same as `auto-git-commit-tool`:

| Mode               | What runs                                      | Host requirements          |
| ------------------ | ---------------------------------------------- | -------------------------- |
| `docker` (default) | `docker run … claude-session-starter`          | systemd (optional), Docker |
| `native`           | the binary under systemd                       | systemd, `claude`, (cargo) |

## Authentication

```bash
claude setup-token
```

Run it once on a machine with a browser, logged in with your **Claude Pro** (or Max) account —
not an API/Console account. It prints a long-lived OAuth
token (about one year). Store it as `CLAUDE_CODE_OAUTH_TOKEN` in an env file with mode `600`, or a
Docker secret. Don't bake it into the image or pass it with `-e` on a shared machine's shell
history.

Avoid mounting your own `~/.claude/.credentials.json` into the container: its refresh token
rotates, and two clients refreshing the same login can log one of them out.

When the token expires the starter fails with an auth error, visible in the logs and in
`claude-session-starter check`. Create a new one and restart.

## Docker image

Multi-stage build:

1. **Builder** – `rust:1-slim-trixie`, `cargo build --release --locked` (dependencies cached in
   their own layer).
2. **Runtime** – `debian:trixie-slim` with:
   - `ca-certificates`, `tini`
   - Claude Code, installed with the native installer at a pinned version
     (`ARG CLAUDE_CODE_VERSION`), with `DISABLE_AUTOUPDATER=1`. Upgrade by rebuilding.
   - the compiled binary, run as non-root user `app` (uid 1000) under `tini`
     (`ENTRYPOINT tini -- claude-session-starter`, default `CMD daemon`)
   - `ENV DATA_DIR=/data`, `VOLUME /data`

The image contains no `CLAUDE.md`, no settings and no project files: the starter runs in the empty
`/data/work` with `HOME=/data/home`.

## Running

```bash
docker run -d \
  --name claude-session-starter \
  --restart unless-stopped \
  --env-file .env \
  -v claude-session-starter-data:/data \
  claude-session-starter

docker logs -f claude-session-starter
docker exec claude-session-starter claude-session-starter status
```

`--restart unless-stopped` makes it survive reboots. For a proper host service use systemd below.

## systemd (planned)

`scripts/install.sh [docker|native]`, mirroring `auto-git-commit-tool`:

1. Checks host requirements for the mode.
2. Reads `CLAUDE_CODE_OAUTH_TOKEN` from the environment (or prompts for it).
3. Builds the image, or installs the release binary to `/usr/local/bin`.
4. Writes `/etc/claude-session-starter/env` (root-only, mode 600). Kept unless `--reconfigure`.
5. Installs `/etc/systemd/system/claude-session-starter.service`, enables and (re)starts it.

Units wait for `network-online.target` and use `Restart=always`. The native unit uses
`DynamicUser=yes` with `StateDirectory=claude-session-starter` as `DATA_DIR`, so it never touches
the user's own `~/.claude`.

```bash
systemctl status claude-session-starter
journalctl -u claude-session-starter -f
```

## Running without Docker

Requirements: Rust toolchain, `claude` on `PATH`.

```bash
cargo build --release
export DATA_DIR=$HOME/.local/share/claude-session-starter
./target/release/claude-session-starter check
./target/release/claude-session-starter status
./target/release/claude-session-starter once
./target/release/claude-session-starter           # daemon
```
