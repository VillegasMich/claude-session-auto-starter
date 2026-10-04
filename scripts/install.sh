#!/usr/bin/env bash
# Install claude-session-starter as a systemd service.
#
#   scripts/install.sh [docker|native] [--reconfigure]
#
#   docker  (default) build the Docker image and run it from systemd, or pull IMAGE if it is set
#           (exported, or in the env file) to a published image, e.g.
#           <user>/claude-session-starter:1.2.3. Host requirements: docker, systemd.
#   native  run a binary built on this machine (target/release, built with cargo if missing) with
#           a private copy of the host's `claude`. Host requirements: systemd, `claude` installed
#           with the native installer (+ cargo if the binary isn't built yet).
#
# Run it as your normal user: it uses sudo only for system changes. The token comes from
# $CLAUDE_CODE_OAUTH_TOKEN or, if unset, a hidden prompt (create one with `claude setup-token`);
# it only travels through variables, stdin and the root-only env file, never argv.
#
# Settings: any of ACTIVE_HOURS, TIMEZONE, DETECTION, CHECK_INTERVAL_MINUTES, STARTER_MODEL,
# STARTER_PROMPT, RUST_LOG, IMAGE exported when running this script are written to
# /etc/claude-session-starter/env. An existing env file is kept unless --reconfigure is given,
# except that an exported IMAGE replaces the one in it.
#
# Upgrade: re-run it (rebuilds the image / binary and recopies `claude`). To a published release:
# set IMAGE in the env file (or export it) and re-run.
set -euo pipefail

readonly SERVICE=claude-session-starter
readonly LOCAL_IMAGE=claude-session-starter:latest
readonly ENV_DIR=/etc/claude-session-starter
readonly ENV_FILE=$ENV_DIR/env
readonly UNIT_FILE=/etc/systemd/system/$SERVICE.service
readonly BIN_DEST=/usr/local/bin/$SERVICE
# Native mode: the service's own `claude`, outside any home directory (ProtectHome=yes).
readonly CLAUDE_DIR=/usr/local/lib/$SERVICE
readonly SETTINGS=(ACTIVE_HOURS TIMEZONE DETECTION CHECK_INTERVAL_MINUTES STARTER_MODEL
  STARTER_PROMPT RUST_LOG IMAGE)

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
readonly ROOT

log() { printf '\033[1m==>\033[0m %s\n' "$*"; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }
need() { command -v "$1" >/dev/null 2>&1 || die "'$1' is required but not installed${2:+ ($2)}"; }

mode=docker
reconfigure=false
for arg in "$@"; do
  case $arg in
    docker | native) mode=$arg ;;
    --reconfigure) reconfigure=true ;;
    -h | --help) sed -n '2,23p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) die "unknown argument '$arg' (try --help)" ;;
  esac
done

SUDO=()
if [[ $EUID -ne 0 ]]; then
  need sudo
  SUDO=(sudo)
fi

# The service refuses to start with any of these set; don't let them reach the env file either.
for name in ANTHROPIC_API_KEY ANTHROPIC_AUTH_TOKEN; do
  [[ -z ${!name:-} ]] || die "$name is set; this service only uses a Claude subscription token, unset it"
done

# --- Dependencies ----------------------------------------------------------------------------
need systemctl "systemd is required to run the service"
if [[ $mode == docker ]]; then
  need docker "https://docs.docker.com/engine/install/"
  DOCKER=(docker)
  if ! docker info >/dev/null 2>&1; then
    DOCKER=("${SUDO[@]}" docker)
    "${DOCKER[@]}" info >/dev/null 2>&1 || die "cannot talk to the Docker daemon; is it running?"
  fi
else
  need claude "install Claude Code: curl -fsSL https://claude.ai/install.sh | bash"
  claude_src=$(readlink -f "$(command -v claude)")
  # The npm package is a script needing node; the service needs the self-contained binary.
  [[ $(head -c 4 "$claude_src" | od -An -c | tr -d ' ') == '177ELF' ]] \
    || die "$claude_src is not a native binary; install Claude Code with the native installer (https://claude.ai/install.sh) or use docker mode"
fi

# --- Token -----------------------------------------------------------------------------------
# The env dir is root-only (0700), so read and test the env file with sudo.
keep_env=false
if "${SUDO[@]}" test -f "$ENV_FILE" && [[ $reconfigure == false ]]; then keep_env=true; fi

if [[ $keep_env == false ]]; then
  token=${CLAUDE_CODE_OAUTH_TOKEN:-}
  if [[ -z $token && -t 0 ]]; then
    read -r -s -p "Claude OAuth token (from 'claude setup-token'): " token
    echo
  fi
  [[ -n $token ]] || die "CLAUDE_CODE_OAUTH_TOKEN is not set; create one with 'claude setup-token' and export it"
  [[ ! $token =~ [[:space:]] ]] || die "the token contains whitespace; paste it again"
  [[ $token == sk-ant-oat01-* ]] \
    || die "not a subscription token (expected sk-ant-oat01-...); create one with 'claude setup-token'"
fi

# --- Build / install the program -------------------------------------------------------------
if [[ $mode == docker ]]; then
  # Same precedence as the unit: exported IMAGE (written to the env file below), then the env
  # file, then the locally built image.
  image=${IMAGE:-}
  if [[ -z $image && $keep_env == true ]]; then
    image=$("${SUDO[@]}" sed -n 's/^IMAGE=//p' "$ENV_FILE" | tail -n 1)
  fi
  image=${image:-$LOCAL_IMAGE}
  if [[ $image == "$LOCAL_IMAGE" ]]; then
    log "Building Docker image $image"
    "${DOCKER[@]}" build --tag "$image" "$ROOT"
  else
    log "Pulling Docker image $image"
    "${DOCKER[@]}" pull "$image" || die "cannot pull '$image'; check IMAGE"
  fi
else
  bin=$ROOT/target/release/$SERVICE
  if [[ ! -x $bin ]]; then
    need cargo "the binary is not built yet; install Rust from https://rustup.rs"
    log "Building release binary"
    (cd "$ROOT" && cargo build --release --locked)
  fi
  log "Installing $bin -> $BIN_DEST"
  "${SUDO[@]}" install -m 0755 "$bin" "$BIN_DEST"
  log "Installing $claude_src -> $CLAUDE_DIR/claude ($("$claude_src" --version 2>/dev/null || echo 'unknown version'))"
  "${SUDO[@]}" install -D -m 0755 "$claude_src" "$CLAUDE_DIR/claude"
fi

# --- Environment file ------------------------------------------------------------------------
if [[ $keep_env == true ]]; then
  log "Keeping existing $ENV_FILE (use --reconfigure to rewrite it)"
  if [[ -n ${IMAGE:-} ]]; then
    log "Setting IMAGE=$IMAGE in $ENV_FILE"
    "${SUDO[@]}" sed -i '/^IMAGE=/d' "$ENV_FILE"
    printf 'IMAGE=%s\n' "$IMAGE" | "${SUDO[@]}" tee -a "$ENV_FILE" >/dev/null
  fi
else
  log "Writing $ENV_FILE (mode 600, root only)"
  {
    printf '# claude-session-starter settings. See docs/configuration.md.\n'
    printf 'CLAUDE_CODE_OAUTH_TOKEN=%s\n' "$token"
    for name in "${SETTINGS[@]}"; do
      if [[ -n ${!name:-} ]]; then printf '%s=%s\n' "$name" "${!name}"; fi
    done
  } | "${SUDO[@]}" sh -c "umask 077 && mkdir -p '$ENV_DIR' && cat > '$ENV_FILE'"
fi

# --- systemd unit ----------------------------------------------------------------------------
log "Installing $UNIT_FILE ($mode mode)"
sed -e "s|@DOCKER@|$(command -v docker || true)|g" -e "s|@BIN@|$BIN_DEST|g" \
  -e "s|@CLAUDE_DIR@|$CLAUDE_DIR|g" \
  "$ROOT/deploy/systemd/$mode.service" | "${SUDO[@]}" tee "$UNIT_FILE" >/dev/null

"${SUDO[@]}" systemctl daemon-reload
"${SUDO[@]}" systemctl enable "$SERVICE" >/dev/null
"${SUDO[@]}" systemctl restart "$SERVICE"

log "Done. $SERVICE is running and will start on boot."
echo "    Logs:    journalctl -u $SERVICE -f"
if [[ $mode == docker ]]; then
  echo "    Image:   $image"
  echo "    Status:  ${DOCKER[*]} exec $SERVICE $SERVICE status"
fi
echo "    Remove:  scripts/uninstall.sh [--purge]"
