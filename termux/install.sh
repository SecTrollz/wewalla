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

Next:
  1. Install the *Termux:API app* (same source as Termux, e.g. F-Droid), open it once, and grant
     Location permission; keep Location switched ON.
  2. wewalla doctor
  3. wewalla connector install      # then tap Install in Android's dialog   (needs --apk build)
  4. wewalla pair                   # copies the pairing token
  5. wewalla connector start        # grants permissions on first run, then returns to Termux
  6. wewalla calibrate              # leave the room empty for 60 s
  7. wewalla run                    # dashboard: http://127.0.0.1:8080/
No APK yet? 'wewalla run' still works via Termux:API only (slower, presence/motion only).
MSG
