#!/usr/bin/env bash
# wewalla guided setup for Termux (Android, non-root).
#
#   bash termux/setup.sh            interactive, resumable
#   bash termux/setup.sh --yes      accept defaults
#   bash termux/setup.sh --only server
#   bash termux/setup.sh --force    re-run steps already marked done
#   bash termux/setup.sh --check-only   run the doctor and exit
#
# Steps: preflight, packages, keepalive, verify, server, network, nodes, finish
set -u -o pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
export WW_REPO="$(cd "$HERE/.." && pwd)"
# shellcheck source=lib.sh
. "$HERE/lib.sh"

ONLY=""; FORCE=0
while [ $# -gt 0 ]; do
  case "$1" in
    -y|--yes) WW_YES=1 ;;
    --force) FORCE=1 ;;
    --only) ONLY="${2:-}"; shift ;;
    --check-only) exec bash "$HERE/doctor.sh" ;;
    --profile) warn "--profile is ignored on Termux (desktop profiles need Docker/ESP-IDF/WASM toolchains)"; shift ;;
    -h|--help) sed -n '2,11p' "${BASH_SOURCE[0]}"; exit 0 ;;
    *) warn "ignoring unknown option: $1" ;;
  esac
  shift
done
export WW_YES="${WW_YES:-0}"

CORE_PKGS=(git python python-numpy python-scipy curl openssl ca-certificates termux-api netcat-openbsd coreutils)
BUILD_PKGS=(rust clang make cmake pkg-config libopenblas binutils)

should_run() { [ -z "$ONLY" ] || [ "$ONLY" = "$1" ]; }
skip_done()  { [ "$FORCE" = 0 ] && [ -z "$ONLY" ] && done_step "$1"; }

# ---------------------------------------------------------------- preflight
phase_preflight() {
  step "1/8" "Preflight"
  if ! is_termux; then
    fail "This does not look like Termux. Install Termux from F-Droid or GitHub releases, then re-run."
    info "Desktop Linux/macOS: use ./install.sh instead."
    exit 1
  fi
  ok "Termux ${TERMUX_VERSION:-(version unknown)} ${TERMUX_APK_RELEASE:+(build: $TERMUX_APK_RELEASE)}"
  info "Tip: install Termux and its plugin apps (Termux:API, Termux:Boot) from the SAME source, or they will not work together."

  local arch; arch="$(uname -m)"
  if [ "$arch" = aarch64 ]; then ok "CPU: $arch"; else warn "CPU: $arch (prebuilt binaries are aarch64 only; you would have to build from source)"; fi

  local free; free="$(df -Pm "$HOME" 2>/dev/null | awk 'NR==2{print $4}')"; free="${free:-0}"
  if [ "$free" -ge 3000 ]; then ok "Free storage: ${free} MB"
  elif [ "$free" -ge 1000 ]; then warn "Free storage: ${free} MB (fine for the prebuilt path; a source build needs ~3000+)"
  else fail "Free storage: ${free} MB. Free some space first."; exit 1; fi

  local ram; ram="$(awk '/MemTotal/{print int($2/1024)}' /proc/meminfo 2>/dev/null || echo 0)"
  if [ "${ram:-0}" -ge 6000 ]; then ok "RAM: ${ram} MB"; else warn "RAM: ${ram} MB (source builds may be killed; prefer the prebuilt binary)"; fi

  if ping -c1 -W3 1.1.1.1 >/dev/null 2>&1 || curl -fsS -m 6 -o /dev/null https://github.com; then ok "Internet reachable"
  else warn "No internet detected; package and binary downloads will fail"; fi
}

# ----------------------------------------------------------------- packages
phase_packages() {
  step "2/8" "Packages"
  if skip_done packages; then ok "already done (use --force to repeat)"; return; fi
  say "Refreshing package lists (if this fails, run: termux-change-repo)"
  pkg update -y >/dev/null 2>&1 || warn "pkg update reported problems; continuing"
  local p missing=()
  for p in "${CORE_PKGS[@]}"; do
    pkg_have "$p" && continue
    if pkg_avail "$p"; then missing+=("$p"); else warn "package not found in your mirror: $p"; fi
  done
  if [ "${#missing[@]}" -gt 0 ]; then
    say "Installing: ${missing[*]}"
    pkg install -y "${missing[@]}" || { fail "pkg install failed. Try: termux-change-repo, then re-run."; return 1; }
  fi
  python3 - <<'PY' && ok "python, numpy, scipy import correctly" || warn "numpy/scipy missing: verification step will be skipped"
import numpy, scipy  # noqa
PY
  if confirm "Also install the Rust build toolchain (only needed to compile on the phone; ~1 GB)?" N; then
    local b bm=()
    for b in "${BUILD_PKGS[@]}"; do pkg_have "$b" || { pkg_avail "$b" && bm+=("$b"); }; done
    [ "${#bm[@]}" -gt 0 ] && pkg install -y "${bm[@]}"
  fi
  mark_step packages
}

# ---------------------------------------------------------------- keepalive
phase_keepalive() {
  step "3/8" "Keep-alive (stop Android killing long runs)"
  if have termux-wake-lock; then termux-wake-lock && ok "CPU wake lock acquired (release with: termux-wake-unlock)"; fi
  say "Do these by hand, once:"
  say "  1) Settings > Apps > Termux > Battery > Unrestricted"
  say "  2) Developer options > enable 'Disable child process restrictions' (Android 14+)."
  say "     Not there? Run:  wewalla adb-fix   (pairs adb to this phone over Wireless debugging)"
  info "Without (2), Android may kill Termux background processes mid-run."
}

# ------------------------------------------------------------------- verify
phase_verify() {
  step "4/8" "Verify the signal pipeline (deterministic proof)"
  if skip_done verify; then ok "already passed (use --force to repeat)"; return; fi
  local cmd out
  if   [ -x "$WW_REPO/verify" ]; then cmd=("$WW_REPO/verify")
  elif [ -f "$WW_REPO/archive/v1/data/proof/verify.py" ]; then cmd=(python3 "$WW_REPO/archive/v1/data/proof/verify.py")
  else warn "no verify script found in this checkout; skipping"; return 0; fi
  say "Running: ${cmd[*]} (can take a minute)"
  if out="$(cd "$WW_REPO" && "${cmd[@]}" 2>&1)"; then :; fi
  printf '%s\n' "$out" | tail -n 6 | sed 's/^/    /'
  if printf '%s' "$out" | grep -q 'VERDICT: PASS'; then ok "VERDICT: PASS"; mark_step verify
  else warn "Did not see 'VERDICT: PASS'. Numpy/scipy versions can shift results; the browser monitor still works."; fi
}

# ------------------------------------------------------------------- server
fetch_prebuilt() {
  local tmp rc=1; tmp="$(mktemp -d)"
  say "Downloading prebuilt aarch64 sensing server..."
  if curl -fL --retry 3 -o "$tmp/$WW_ASSET" "$WW_RELEASE_URL/$WW_ASSET" \
     && curl -fL --retry 3 -o "$tmp/$WW_ASSET.sha256" "$WW_RELEASE_URL/$WW_ASSET.sha256"; then
    if (cd "$tmp" && sha256sum -c "$WW_ASSET.sha256" >/dev/null 2>&1); then
      ok "checksum verified"
      tar -xzf "$tmp/$WW_ASSET" -C "$tmp" && install -m 755 "$tmp/sensing-server" "$WW_BIN/sensing-server" && rc=0
    else
      fail "checksum mismatch; refusing to install"
    fi
  fi
  rm -rf "$tmp"
  return $rc
}

build_source() {
  have cargo || { fail "cargo not found. Run: pkg install rust clang make cmake pkg-config libopenblas"; return 1; }
  warn "Compiling on the phone is slow and memory hungry. Keep the screen on and the phone plugged in."
  ( cd "$WW_REPO/v2" && CARGO_BUILD_JOBS="${WW_JOBS:-2}" cargo build --release -p wifi-densepose-sensing-server ) || return 1
  local bin
  bin="$(find "$WW_REPO/v2/target/release" -maxdepth 1 -type f -perm -u+x -name '*sensing-server*' ! -name '*.d' 2>/dev/null | head -n1)"
  [ -n "$bin" ] || { fail "build finished but no sensing-server binary was found"; return 1; }
  install -m 755 "$bin" "$WW_BIN/sensing-server"
}

phase_server() {
  step "5/8" "Sensing server binary"
  if [ -x "$WW_BIN/sensing-server" ] && [ "$FORCE" = 0 ]; then ok "already installed at $WW_BIN/sensing-server"; return; fi
  if fetch_prebuilt; then ok "installed prebuilt server"; mark_step server; return; fi
  warn "No prebuilt binary available yet (the release may not be published)."
  if confirm "Build from source on this phone instead?" N; then
    build_source && { ok "built and installed"; mark_step server; }
  else
    info "Skipping. The browser monitor ('wewalla lite') works without it."
  fi
}

# ------------------------------------------------------------------ network
phase_network() {
  step "6/8" "Network check"
  local ip; ip="$(phone_ip)"
  if [ -n "$ip" ]; then ok "Phone address: $ip"; else warn "Could not determine the phone's address (Termux:API app installed? Location permission granted?)"; fi
  info "If the phone is the hotspot, the address is the hotspot gateway shown in Settings (often 192.168.x.1)."
  if port_free udp "$WW_UDP_PORT"; then ok "UDP $WW_UDP_PORT is free"; else warn "UDP $WW_UDP_PORT is busy (another listener running?)"; fi
  if port_free tcp "$WW_HTTP_PORT"; then ok "TCP $WW_HTTP_PORT is free"; else warn "TCP $WW_HTTP_PORT is busy"; fi
  if python3 "$HERE/lite/lite.py" --selftest "${ip:-127.0.0.1}" >/dev/null 2>&1; then ok "UDP receive self-test passed"; else fail "UDP self-test failed (try: wewalla lite --selftest)"; fi
}

# -------------------------------------------------------------------- nodes
phase_nodes() {
  step "7/8" "Sensor nodes (ESP32-S3)"
  local ip; ip="$(phone_ip)"; ip="${ip:-<PHONE_IP>}"
  say "The phone cannot capture WiFi CSI itself (no root). CSI comes from ESP32-S3 boards"
  say "that stream UDP to this phone. Flash and provision them from a computer, once:"
  printf '\n    python firmware/esp32-csi-node/provision.py --port <SERIAL_PORT> \\\n'
  printf '      --ssid "<YOUR_WIFI>" --password "<PASSWORD>" --target-ip %s\n\n' "$ip"
  say "Give the phone a fixed address (router DHCP reservation) so --target-ip stays valid."
  say "Then run:  wewalla lite   and open the page; nodes appear as packets arrive."
}

# ------------------------------------------------------------------- finish
phase_finish() {
  step "8/8" "Finish"
  if [ -w "$WW_PREFIX/bin" ]; then
    ln -sf "$HERE/wewalla" "$WW_PREFIX/bin/wewalla" && ok "installed command: wewalla"
  fi
  chmod +x "$HERE"/*.sh "$HERE/wewalla" "$HERE/lite/lite.py" 2>/dev/null || true
  printf '\n  %sNext:%s\n' "$C_B" "$C_0"
  say "wewalla doctor        check everything"
  say "wewalla lite          start the browser monitor  (http://127.0.0.1:$WW_HTTP_PORT)"
  say "wewalla lite --rssi   add coarse phone-WiFi RSSI motion hint"
  say "wewalla server --help see the full sensing server options"
  info "Research prototype: not a medical device or safety system. Only sense spaces where everyone present has agreed."
}

for p in preflight packages keepalive verify server network nodes finish; do
  if should_run "$p"; then "phase_$p" || warn "step '$p' did not complete; fix the message above and re-run (finished steps are skipped)"; fi
done
