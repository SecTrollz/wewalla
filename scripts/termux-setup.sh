#!/data/data/com.termux/files/usr/bin/sh
# One-shot install on the phone (Termux). Builds natively, installs the UI, writes a config.
# Usage:  sh scripts/termux-setup.sh
set -eu

[ -n "${PREFIX:-}" ] || { echo "run this inside Termux" >&2; exit 1; }
repo=$(cd "$(dirname "$0")/.." && pwd)
share="$PREFIX/share/rvcsi"
state="$HOME/.rvcsi"

pkg install -y rust termux-api mosquitto >/dev/null
# termux-api (the package) is only the CLI; the Termux:API *app* must be installed from
# the same source (F-Droid) as Termux, and location permission granted to it, or Wi-Fi
# commands return placeholders / errors. `rvcsi-node probe` will tell you.

cd "$repo"
RUSTFLAGS="-C target-cpu=native" cargo build --release --locked
install -m 0755 target/release/rvcsi-node "$PREFIX/bin/rvcsi-node"

mkdir -p "$share"
rm -rf "$share/ui"
cp -R ui "$share/ui"

umask 077
mkdir -p "$state"
if [ ! -f "$state/config.json" ]; then
  sed "s#\"ui_dir\": \"ui\"#\"ui_dir\": \"$share/ui\"#" config/edge2025.json > "$state/config.json"
fi

echo
echo "Installed. Next:"
echo "  rvcsi-node probe  --config $state/config.json      # what does this phone expose?"
echo "  rvcsi-node calibrate --config $state/config.json   # empty room, ~90 s"
echo "  termux-wake-lock; rvcsi-node run --config $state/config.json"
echo "  rvcsi-node token --url --config $state/config.json # open this URL once in the phone browser"
