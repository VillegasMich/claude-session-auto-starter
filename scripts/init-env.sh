#!/usr/bin/env bash
# Create ./.env for local development (`cargo run`), filled from what this machine already has.
#
#   scripts/init-env.sh [--force] [--non-interactive]
#
# Every setting in .env.example is filled from the first source that has a value:
#   1. the main checkout's .env ($ORCA_ROOT_PATH, else the first `git worktree list` entry), so
#      a new worktree reuses the token and settings of the main one;
#   2. the variable exported in the current shell;
#   3. a value derived from this machine:
#        TIMEZONE  the system timezone (timedatectl, /etc/localtime)
#        DATA_DIR  ${XDG_DATA_HOME:-~/.local/share}/claude-session-starter, outside any repo and
#                  shared by all worktrees (one state file, so they never start a second window)
#   4. CLAUDE_CODE_OAUTH_TOKEN only: a hidden prompt when run from a terminal. It can't be
#      produced non-interactively: create it with `claude setup-token` (opens a browser).
# Settings without a value keep their .env.example line (commented default). Settings in the
# main .env that .env.example doesn't list are appended as they are.
#
# An existing .env is kept (exit 0, so it is safe as the Orca setup hook in orca.yaml) unless
# --force is given; its values then win over the main checkout's and it is saved as .env.bak.
# --non-interactive never prompts. Values are never printed, only where each one came from.
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
readonly ROOT
readonly TEMPLATE=$ROOT/.env.example
readonly TARGET=$ROOT/.env
readonly TOKEN_KEY=CLAUDE_CODE_OAUTH_TOKEN

log() { printf '\033[1m==>\033[0m %s\n' "$*"; }
warn() { printf 'warning: %s\n' "$*" >&2; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

force=false
interactive=true
for arg in "$@"; do
  case $arg in
    --force) force=true ;;
    --non-interactive) interactive=false ;;
    -h | --help) sed -n '2,22p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) die "unknown argument '$arg' (try --help)" ;;
  esac
done

[[ -f $TEMPLATE ]] || die "$TEMPLATE not found"
if [[ -f $TARGET && $force == false ]]; then
  log ".env already exists, keeping it (use --force to regenerate)"
  exit 0
fi

# The checkout whose .env is reused: the main worktree, or this one's own .env with --force.
main_checkout() {
  if [[ -n ${ORCA_ROOT_PATH:-} ]]; then
    printf '%s\n' "$ORCA_ROOT_PATH"
  else
    git -C "$ROOT" worktree list --porcelain 2>/dev/null | sed -n '1s/^worktree //p'
  fi
}
source_env=
source_label=
if [[ -f $TARGET ]]; then
  source_env=$TARGET
  source_label="previous .env"
else
  main=$(main_checkout)
  if [[ -n $main && $main != "$ROOT" && -f $main/.env ]]; then
    source_env=$main/.env
    source_label="main checkout .env"
    log "reusing $source_env"
  fi
fi

# KEY=value lines of the source .env, last one wins like dotenvy. The raw value (quotes included)
# is kept, so it round-trips unchanged.
declare -A from_file=()
file_keys=()
if [[ -n $source_env ]]; then
  while IFS= read -r line || [[ -n $line ]]; do
    if [[ $line =~ ^[[:space:]]*(export[[:space:]]+)?([A-Za-z_][A-Za-z0-9_]*)=(.*)$ ]]; then
      key=${BASH_REMATCH[2]}
      [[ -v from_file[$key] ]] || file_keys+=("$key")
      from_file[$key]=${BASH_REMATCH[3]}
    fi
  done <"$source_env"
fi

system_timezone() {
  local tz=
  if command -v timedatectl >/dev/null 2>&1; then
    tz=$(timedatectl show -p Timezone --value 2>/dev/null || true)
  fi
  if [[ -z $tz && -L /etc/localtime ]]; then
    tz=$(readlink /etc/localtime | sed -n 's|.*/zoneinfo/||p')
  fi
  if [[ -z $tz && -f /etc/timezone ]]; then
    tz=$(head -n1 /etc/timezone)
  fi
  printf '%s\n' "$tz"
}

# Sets `value` and `origin` for a key; returns 1 if no source has a value.
resolve() {
  local key=$1
  value=
  origin=
  if [[ -n ${from_file[$key]:-} ]]; then
    value=${from_file[$key]}
    origin=$source_label
  elif [[ -n ${!key:-} ]]; then
    value=${!key}
    origin="exported variable"
  else
    case $key in
      TIMEZONE) value=$(system_timezone); origin="system timezone" ;;
      DATA_DIR) value=${XDG_DATA_HOME:-$HOME/.local/share}/claude-session-starter; origin=default ;;
    esac
  fi
  [[ -n $value ]]
}

prompt_token() {
  [[ $interactive == true && -t 0 ]] || return 1
  printf '%s' "$TOKEN_KEY (from 'claude setup-token', input hidden, Enter to skip): " >&2
  local token
  IFS= read -rs token || true
  printf '\n' >&2
  [[ -n $token ]] || return 1
  [[ $token == sk-ant-oat* ]] || { warn "not a subscription token (must start with sk-ant-oat), skipped"; return 1; }
  value=$token
  origin=prompt
}

tmp=$(mktemp "$ROOT/.env.XXXXXX")
trap 'rm -f "$tmp"' EXIT
chmod 600 "$tmp"

declare -A done_keys=()
report=()
token_set=false
# The template comes in on fd 3 so that stdin stays free for the token prompt.
while IFS= read -r -u 3 line || [[ -n $line ]]; do
  if [[ $line =~ ^#?([A-Z][A-Z0-9_]*)=(.*)$ && ! -v done_keys[${BASH_REMATCH[1]}] ]]; then
    key=${BASH_REMATCH[1]}
    done_keys[$key]=1
    if resolve "$key" || { [[ $key == "$TOKEN_KEY" ]] && prompt_token; }; then
      printf '%s=%s\n' "$key" "$value" >>"$tmp"
      report+=("$(printf '%-24s %s' "$key" "$origin")")
      [[ $key == "$TOKEN_KEY" ]] && token_set=true
      continue
    fi
    # Never keep the example's placeholder token: an empty one fails with a clear error.
    if [[ $key == "$TOKEN_KEY" ]]; then
      printf '%s=\n' "$key" >>"$tmp"
      continue
    fi
  fi
  printf '%s\n' "$line" >>"$tmp"
done 3<"$TEMPLATE"

extra=()
for key in "${file_keys[@]}"; do
  [[ -v done_keys[$key] || -z ${from_file[$key]} ]] || extra+=("$key")
done
if ((${#extra[@]})); then
  printf '\n# Not in .env.example, copied from %s\n' "$source_env" >>"$tmp"
  for key in "${extra[@]}"; do
    printf '%s=%s\n' "$key" "${from_file[$key]}" >>"$tmp"
    report+=("$(printf '%-24s %s' "$key" "$source_label")")
  done
fi

if [[ -f $TARGET ]]; then
  cp -p "$TARGET" "$TARGET.bak"
  log "previous .env saved as .env.bak"
fi
mv "$tmp" "$TARGET"
trap - EXIT

log "wrote $TARGET"
for entry in "${report[@]}"; do
  printf '    %s\n' "$entry"
done
if [[ $token_set == false ]]; then
  warn "$TOKEN_KEY is empty: create one with \`claude setup-token\`, then set it in .env" \
    "or re-run with --force from a terminal"
fi
