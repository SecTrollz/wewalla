#!/usr/bin/env bash
# Pair adb to this phone over Wireless debugging (no PC needed) and lift Android's
# phantom-process limit, which otherwise kills Termux background processes.
set -u
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
. "$HERE/lib.sh"

have adb || { say "Installing android-tools..."; pkg install -y android-tools || exit 1; }
cat <<'TXT'

  1) Connect the phone to WiFi (Wireless debugging needs it).
  2) Settings > Developer options > Wireless debugging > turn ON.
  3) Tap "Pair device with pairing code". Keep that dialog open (use split-screen or a notification reply).
TXT
read -r -p "  Pairing PORT shown in the dialog: " pport
read -r -p "  6-digit pairing CODE: " pcode
adb pair "localhost:${pport}" "${pcode}" || { fail "pairing failed"; exit 1; }
echo
echo "  Now close the dialog. On the main Wireless debugging screen note the IP & PORT"
echo "  (this is a DIFFERENT port from the pairing port)."
read -r -p "  Connect PORT: " cport
adb connect "localhost:${cport}" || { fail "connect failed"; exit 1; }
adb -s "localhost:${cport}" shell "settings put global settings_enable_monitor_phantom_procs false" \
  && ok "phantom process monitor disabled" || warn "could not set settings_enable_monitor_phantom_procs"
adb -s "localhost:${cport}" shell "device_config put activity_manager max_phantom_processes 2147483647" \
  && ok "phantom process limit raised" || warn "device_config not permitted on this Android build (use the Developer options toggle instead)"
adb disconnect >/dev/null 2>&1
info "You can turn Wireless debugging off again now."
