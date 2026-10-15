#!/usr/bin/env bash
# Remove the kompressor systemd USER service.
#
#   scripts/service/uninstall.sh            # stop, disable, remove the unit
#   scripts/service/uninstall.sh --purge    # also remove the config and state
#
# Without --purge this leaves the configuration and the fragment store alone.
# Deleting the fragment store is not a tidy-up: a handle the model still holds
# stops resolving, so a conversation in flight loses what was elided from it.
set -euo pipefail

UNIT_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
CONF_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/kompressor"
STATE="${XDG_STATE_HOME:-$HOME/.local/state}/kompressor"
UNIT="$UNIT_DIR/kompressor.service"

purge=0
for arg in "$@"; do
  case "$arg" in
    --purge)   purge=1 ;;
    -h|--help) sed -n '2,10p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown argument: $arg" >&2; exit 2 ;;
  esac
done

if systemctl --user list-unit-files kompressor.service >/dev/null 2>&1; then
  systemctl --user disable --now kompressor.service >/dev/null 2>&1 || true
  echo "stopped and disabled kompressor.service"
fi

if [ -e "$UNIT" ]; then
  rm -f "$UNIT"
  echo "removed $UNIT"
fi
systemctl --user daemon-reload
systemctl --user reset-failed kompressor.service >/dev/null 2>&1 || true

if [ "$purge" -eq 1 ]; then
  rm -rf "$CONF_DIR" "$STATE"
  echo "purged $CONF_DIR and $STATE"
else
  echo
  echo "kept your configuration and data:"
  echo "  config  $CONF_DIR"
  echo "  state   $STATE"
  echo "pass --purge to remove them too."
fi
