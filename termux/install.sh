#!/data/data/com.termux/files/usr/bin/env bash
# One-shot Termux setup. Safe to re-run.   Usage: ./install.sh [--apk]
#   (also works on plain Debian/Ubuntu for development: it skips `pkg` there)
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BUILD_APK=0; [[ "${1:-}" == "--apk" ]] && BUILD_APK=1

if command -v pkg >/dev/null 2>&1; then
  pkg update -y
  pkg install -y python termux-api
  ((BUILD_APK)) && pkg install -y openjdk-17 aapt apksigner dx ecj zip curl unzip
else
  echo "(not Termux: install python3 yourself; skipping pkg)"
fi

chmod +x "$HERE/bin/wewalla" "$HERE/connector-apk/build.sh"
BIN="${PREFIX:-$HOME/.local}/bin"; mkdir -p "$BIN"
ln -sf "$HERE/bin/wewalla" "$BIN/wewalla"
echo "linked $BIN/wewalla"

if ((BUILD_APK)); then "$HERE/connector-apk/build.sh"; fi

cat <<MSG

Next (in plain Termux, not proot):
  1. Install the *Termux:API app* (same source as Termux), open it once, allow Location, keep it ON.
  2. termux-setup-storage   # once, so the APK can go to Downloads
     cp ~/.wewalla/wewalla-connector.apk ~/storage/downloads/
     then Files -> Download -> wewalla-connector.apk -> Install   (needs --apk build)
  3. Open Wewalla Connector once: tap "1. Grant permissions", allow everything.
  4. wewalla up             # starts everything and opens http://127.0.0.1:8080/
No APK? 'wewalla up' still works via Termux:API only (slower; presence/motion + bed heart/breathing).
MSG
