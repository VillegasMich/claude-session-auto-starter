#!/usr/bin/env bash
# Remove the claude-session-starter systemd service.
#
#   scripts/uninstall.sh [--purge]
#
# By default the env file (token), the state and the Docker image/volume are kept, so a reinstall
# picks up where it left off. --purge deletes them too. The token is not revoked, only deleted
# from this machine.
set -euo pipefail

readonly SERVICE=claude-session-starter

purge=false
case ${1:-} in
  "") ;;
  --purge) purge=true ;;
  -h | --help) sed -n '2,8p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
  *) echo "error: unknown argument '$1' (try --help)" >&2; exit 1 ;;
esac

SUDO=()
[[ $EUID -ne 0 ]] && SUDO=(sudo)

"${SUDO[@]}" systemctl disable --now "$SERVICE" 2>/dev/null || true
"${SUDO[@]}" rm -f "/etc/systemd/system/$SERVICE.service"
"${SUDO[@]}" systemctl daemon-reload
echo "==> Service removed"

if [[ $purge == true ]]; then
  # A published image configured with IMAGE in the env file is removed too.
  image=$("${SUDO[@]}" sed -n 's/^IMAGE=//p' "/etc/$SERVICE/env" 2>/dev/null | tail -n 1 || true)
  "${SUDO[@]}" rm -rf "/etc/$SERVICE" "/usr/local/bin/$SERVICE" "/usr/local/lib/$SERVICE" \
    "/var/lib/$SERVICE" "/var/lib/private/$SERVICE"
  if command -v docker >/dev/null 2>&1; then
    "${SUDO[@]}" docker rm --force "$SERVICE" >/dev/null 2>&1 || true
    "${SUDO[@]}" docker volume rm "$SERVICE-data" >/dev/null 2>&1 || true
    "${SUDO[@]}" docker image rm "$SERVICE:latest" ${image:+"$image"} >/dev/null 2>&1 || true
  fi
  echo "==> Purged config, token, state, binaries and Docker image/volume"
fi
