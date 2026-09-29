#!/usr/bin/env bash
# wewalla doctor: read-only health check for the Termux install. Exit code 1 if anything FAILs.
set -u -o pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
export WW_REPO="$(cd "$HERE/.." && pwd)"
# shellcheck source=lib.sh
. "$HERE/lib.sh"

FAILS=0; WARNS=0
bad()  { fail "$*"; FAILS=$((FAILS+1)); }
meh()  { warn "$*"; WARNS=$((WARNS+1)); }

printf '%swewalla doctor%s\n' "$C_B" "$C_0"

step "env" "Environment"
if is_termux; then ok "Termux ${TERMUX_VERSION:-}"; else bad "not running inside Termux"; fi
[ "$(uname -m)" = aarch64 ] && ok "arch aarch64" || meh "arch $(uname -m): prebuilt binaries are aarch64 only"
free="$(df -Pm "$HOME" 2>/dev/null | awk 'NR==2{print $4}')"; free="${free:-0}"
[ "$free" -ge 1000 ] && ok "free storage ${free} MB" || bad "free storage only ${free} MB"

step "py" "Python stack"
if have python3; then ok "$(python3 --version 2>&1)"; else bad "python3 missing (pkg install python)"; fi
for m in numpy scipy; do
  if python3 -c "import $m" 2>/dev/null; then ok "python module: $m"; else meh "python module missing: $m (pkg install python-$m)"; fi
done

step "api" "Termux:API (optional; needed for phone IP and RSSI)"
if have termux-wifi-connectioninfo; then
  out="$(timeout 8 termux-wifi-connectioninfo 2>/dev/null || true)"
  if printf '%s' "$out" | python3 -c 'import sys,json; d=json.load(sys.stdin); sys.exit(0 if "rssi" in d else 1)' 2>/dev/null; then
    ok "termux-wifi-connectioninfo works"
  else meh "termux-api command found but no data (install the Termux:API app; grant Location permission)"; fi
else meh "termux-api package not installed (pkg install termux-api)"; fi

step "srv" "Sensing server"
if [ -x "$WW_BIN/sensing-server" ]; then
  if timeout 10 "$WW_BIN/sensing-server" --help >/dev/null 2>&1; then ok "sensing-server runs"; else meh "sensing-server present but --help failed"; fi
else meh "sensing-server not installed (wewalla setup --only server). 'wewalla lite' does not need it."; fi

step "net" "Network"
ip="$(phone_ip)"; [ -n "$ip" ] && ok "phone address $ip" || meh "phone address unknown"
port_free udp "$WW_UDP_PORT" && ok "UDP $WW_UDP_PORT free" || meh "UDP $WW_UDP_PORT busy (is 'wewalla lite' already running?)"
port_free tcp "$WW_HTTP_PORT" && ok "TCP $WW_HTTP_PORT free" || meh "TCP $WW_HTTP_PORT busy"
python3 "$HERE/lite/lite.py" --selftest >/dev/null 2>&1 && ok "UDP loopback self-test" || bad "UDP loopback self-test failed"

step "life" "Staying alive"
if [ -f "$WW_STATE" ]; then ok "setup state: $(tr '\n' ' ' < "$WW_STATE")"; else meh "setup has not been run (wewalla setup)"; fi
info "Cannot be checked from inside Termux: battery 'Unrestricted' and 'Disable child process restrictions'."

printf '\n%s%d failure(s), %d warning(s)%s\n' "$C_B" "$FAILS" "$WARNS" "$C_0"
[ "$FAILS" -eq 0 ]
