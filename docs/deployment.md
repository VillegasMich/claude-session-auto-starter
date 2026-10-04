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

## Published image

Each release pushes `<user>/claude-session-starter` to Docker Hub with tags `<version>`
(e.g. `1.2.3`), `<major>.<minor>` and `latest` (not for pre-releases), for `linux/amd64` and
`linux/arm64`. Use it in place of the locally built `claude-session-starter` in the `docker run`
above, or with `IMAGE=` in systemd mode below. How images are built and released:
[repository-setup.md](repository-setup.md).

## systemd

```bash
export CLAUDE_CODE_OAUTH_TOKEN=sk-ant-oat01-...   # optional: otherwise a hidden prompt asks
scripts/install.sh [docker|native] [--reconfigure]
```

Run it as your normal user; it uses `sudo` only for system changes. It:

1. Checks host requirements for the mode, and refuses to run with `ANTHROPIC_API_KEY` or
   `ANTHROPIC_AUTH_TOKEN` set.
2. Reads `CLAUDE_CODE_OAUTH_TOKEN` from the environment (or prompts for it, hidden) and checks it
   looks like a subscription token (`sk-ant-oat01-…`). It never appears on a command line.
3. **docker:** builds the image as `claude-session-starter:latest`, or pulls `IMAGE` when it is
   exported or in the env file (e.g. `IMAGE=<user>/claude-session-starter:1.2.3`).
   **native:** installs `target/release/claude-session-starter` (built with cargo if missing) to
   `/usr/local/bin`, and copies the host's `claude` binary to
   `/usr/local/lib/claude-session-starter/claude`.
4. Writes `/etc/claude-session-starter/env` (root-only, mode 600) with the token and any of
   `ACTIVE_HOURS`, `TIMEZONE`, `DETECTION`, `CHECK_INTERVAL_MINUTES`, `STARTER_MODEL`,
   `STARTER_PROMPT`, `RUST_LOG`, `IMAGE` that are exported. An existing file is kept unless
   `--reconfigure`, except that an exported `IMAGE` replaces the one in it.
5. Installs `/etc/systemd/system/claude-session-starter.service` from
   [`deploy/systemd/`](../deploy/systemd), enables and (re)starts it.

Both units wait for `network-online.target` and use `Restart=always` (60 s apart), so a bad token
or a missing network shows up as repeated failures in the journal rather than a dead service.

| Mode     | Unit runs                                                                  | State              |
| -------- | -------------------------------------------------------------------------- | ------------------ |
| `docker` | `docker run --env-file /etc/claude-session-starter/env -v claude-session-starter-data:/data $IMAGE daemon` | Docker volume `claude-session-starter-data` |
| `native` | `claude-session-starter daemon` with `DynamicUser=yes`, hardened (`ProtectHome`, `ProtectSystem=strict`, …) | `StateDirectory=` `/var/lib/claude-session-starter` (= `DATA_DIR`) |

The native unit never touches a user's own `~/.claude`: `ProtectHome=yes` hides home directories,
which is why it runs its own copy of `claude` (and why that must be the self-contained binary from
the native installer, not the npm package). The copy doesn't auto-update; re-run
`scripts/install.sh native` to pick up a newer `claude`. Don't set `DATA_DIR` or `PATH` in the env
file: they would override the unit's.

```bash
systemctl status claude-session-starter
journalctl -u claude-session-starter -f
docker exec claude-session-starter claude-session-starter status   # docker mode
```

### Upgrading

- **Locally built:** pull the repository and re-run `scripts/install.sh [docker|native]`. The env
  file and state are kept.
- **Published image:** `IMAGE=<user>/claude-session-starter:1.2.3 scripts/install.sh` (pulls it,
  updates `IMAGE` in the env file, restarts).
- **Claude Code in the image:** it is pinned by `CLAUDE_CODE_VERSION` in the `Dockerfile`; a new
  release (or a local rebuild) picks up a new pin.

### Uninstalling

```bash
scripts/uninstall.sh            # stop and remove the unit; keep env file, state, image, volume
scripts/uninstall.sh --purge    # also delete the env file (token), state, binaries, image, volume
```

A purge deletes the token from this machine but does not revoke it; it stays valid until it
expires.

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
