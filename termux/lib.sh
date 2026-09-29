#!/usr/bin/env bash
# shellcheck shell=bash
# Shared helpers for the wewalla Termux tooling. Source this file; do not run it.

WW_HOME="${WEWALLA_HOME:-$HOME/.wewalla}"
WW_STATE="$WW_HOME/state"
WW_BIN="$WW_HOME/bin"
WW_PREFIX="${PREFIX:-/data/data/com.termux/files/usr}"
WW_REPO="${WW_REPO:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
WW_RELEASE_URL="${WEWALLA_RELEASE_URL:-https://github.com/SecTrollz/wewalla/releases/download/termux-latest}"
WW_ASSET="wewalla-sensing-server-aarch64-linux-android.tar.gz"
WW_UDP_PORT="${WEWALLA_UDP_PORT:-5005}"
WW_HTTP_PORT="${WEWALLA_HTTP_PORT:-3100}"
mkdir -p "$WW_HOME" "$WW_BIN"

if [ -t 1 ]; then
  C_R=$'\033[31m'; C_G=$'\033[32m'; C_Y=$'\033[33m'; C_C=$'\033[36m'; C_B=$'\033[1m'; C_D=$'\033[2m'; C_0=$'\033[0m'
else
  C_R=''; C_G=''; C_Y=''; C_C=''; C_B=''; C_D=''; C_0=''
fi

step()  { printf '\n%s[%s]%s %s%s%s\n' "$C_C" "$1" "$C_0" "$C_B" "$2" "$C_0"; }
ok()    { printf '  %sOK%s    %s\n' "$C_G" "$C_0" "$*"; }
warn()  { printf '  %sWARN%s  %s\n' "$C_Y" "$C_0" "$*"; }
fail()  { printf '  %sFAIL%s  %s\n' "$C_R" "$C_0" "$*"; }
info()  { printf '  %s%s%s\n' "$C_D" "$*" "$C_0"; }
say()   { printf '  %s\n' "$*"; }

have()       { command -v "$1" >/dev/null 2>&1; }
is_termux()  { [ -n "${TERMUX_VERSION:-}" ] || [ -d /data/data/com.termux/files/usr ]; }
done_step()  { [ -f "$WW_STATE" ] && grep -qx "$1" "$WW_STATE"; }
mark_step()  { done_step "$1" || echo "$1" >> "$WW_STATE"; }

# confirm "question" [Y|N]  -> returns 0 for yes. Honors WW_YES=1 and non-interactive stdin.
confirm() {
  local q="$1" d="${2:-Y}" a
  [ "${WW_YES:-0}" = 1 ] && return 0
  if [ ! -t 0 ]; then [ "$d" = Y ]; return; fi
  read -r -p "  ? $q [$d] " a || true
  a="${a:-$d}"
  case "$a" in [Yy]*) return 0 ;; *) return 1 ;; esac
}

pkg_have()  { dpkg -s "$1" >/dev/null 2>&1; }
pkg_avail() { apt-cache show "$1" >/dev/null 2>&1; }

# port_free udp|tcp PORT
port_free() {
  python3 - "$1" "$2" <<'PY'
import socket, sys
kind = socket.SOCK_DGRAM if sys.argv[1] == "udp" else socket.SOCK_STREAM
s = socket.socket(socket.AF_INET, kind)
try:
    s.bind(("0.0.0.0", int(sys.argv[2])))
except OSError:
    sys.exit(1)
PY
}

# phone_ip: best-effort LAN address of this phone (prints nothing if unknown).
phone_ip() {
  local ip=""
  if have termux-wifi-connectioninfo; then
    ip="$(timeout 8 termux-wifi-connectioninfo 2>/dev/null \
      | python3 -c 'import sys,json; print(json.load(sys.stdin).get("ip",""))' 2>/dev/null || true)"
  fi
  case "$ip" in ""|null|0.0.0.0) ip="" ;; esac
  if [ -z "$ip" ]; then
    # Fallback: ask the kernel which local address would route outward. Sends no packet.
    ip="$(python3 - <<'PY' 2>/dev/null || true
import socket
s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
try:
    s.connect(("192.0.2.1", 9))
    print(s.getsockname()[0])
except OSError:
    pass
PY
)"
  fi
  printf '%s' "$ip"
}
