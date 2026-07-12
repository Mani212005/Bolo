#!/usr/bin/env bash
# Registers GNOME custom keybindings for Bolo:
#   toggle (default Ctrl+Space), pause (default Alt+P),
#   insert-last (default Alt+I).
# Append-safe and re-runnable: slots are reused by name, other custom
# keybindings are preserved.
# Usage: install-hotkey.sh [toggle-binding] [pause-binding] [insert-binding]
set -euo pipefail

TOGGLE_BINDING="${1:-<Control>space}"
PAUSE_BINDING="${2:-<Alt>p}"
INSERT_BINDING="${3:-<Alt>i}"
BOLO="$(cd "$(dirname "$0")/.." && pwd)/target/release/bolo"
[ -x "$BOLO" ] || { echo "error: $BOLO not built (run: cargo build --release)"; exit 1; }

SCHEMA=org.gnome.settings-daemon.plugins.media-keys
KEY_PATH_BASE=/org/gnome/settings-daemon/plugins/media-keys/custom-keybindings

register() {
    local name="$1" command="$2" binding="$3"
    local current slot="" i p existing
    current=$(gsettings get $SCHEMA custom-keybindings)
    for i in $(seq 0 31); do
        p="$KEY_PATH_BASE/custom$i/"
        existing=$(gsettings get $SCHEMA.custom-keybinding:$p name 2>/dev/null || echo "''")
        if [ "$existing" = "'$name'" ]; then slot=$p; break; fi
        if [[ "$current" != *"$p"* ]] && [ -z "$slot" ]; then slot=$p; fi
    done
    [ -n "$slot" ] || { echo "error: no free custom keybinding slot"; exit 1; }

    gsettings set $SCHEMA.custom-keybinding:$slot name "$name"
    gsettings set $SCHEMA.custom-keybinding:$slot command "$command"
    gsettings set $SCHEMA.custom-keybinding:$slot binding "$binding"

    if [[ "$current" != *"$slot"* ]]; then
        if [ "$current" = "@as []" ] || [ "$current" = "[]" ]; then
            gsettings set $SCHEMA custom-keybindings "['$slot']"
        else
            gsettings set $SCHEMA custom-keybindings "${current%]*}, '$slot']"
        fi
    fi
    echo "installed: $binding -> $command (slot $slot)"
}

register "Bolo toggle" "$BOLO toggle" "$TOGGLE_BINDING"
register "Bolo pause"  "$BOLO pause"  "$PAUSE_BINDING"
register "Bolo insert" "$BOLO insert-last" "$INSERT_BINDING"
echo "verify:    gsettings get $SCHEMA custom-keybindings"
