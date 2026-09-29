#!/usr/bin/env bash
# Idempotently make the top-level ./install.sh hand off to the Termux installer when run inside Termux.
set -eu
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
F="$REPO/install.sh"
[ -f "$F" ] || { echo "install.sh not found in $REPO" >&2; exit 1; }
if grep -q 'WEWALLA_TERMUX_GUARD' "$F"; then echo "install.sh already patched"; exit 0; fi
grep -q '^set -euo pipefail' "$F" || { echo "unexpected install.sh layout; not patching" >&2; exit 1; }
tmp="$(mktemp)"
awk '
  { print }
  !done && /^set -euo pipefail/ {
    print ""
    print "# WEWALLA_TERMUX_GUARD: Android/Termux has no sudo/apt/rustup/Docker; use the dedicated installer."
    print "if [ -n \"${TERMUX_VERSION:-}\" ] || [ -d /data/data/com.termux/files/usr ]; then"
    print "  echo \"Termux detected: running termux/setup.sh\""
    print "  exec bash \"$(cd \"$(dirname \"${BASH_SOURCE[0]}\")\" && pwd)/termux/setup.sh\" \"$@\""
    print "fi"
    done = 1
  }' "$F" > "$tmp"
cat "$tmp" > "$F"; rm -f "$tmp"
echo "patched $F"
