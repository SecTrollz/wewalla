#!/data/data/com.termux/files/usr/bin/bash
# rvcsi device diagnostics. READ-ONLY: changes no settings, enables no monitor mode, sends nothing off the phone.
# `su` will prompt for a root grant.
#
#   bash scripts/diag.sh A   connected to your normal Wi-Fi, hotspot OFF
#   bash scripts/diag.sh B   your hotspot ON (5 GHz) with one idle client connected
#   bash scripts/diag.sh C   driver / Wi-Fi chip / CSI strings / regulatory domain / Bluetooth / sensors
#
# Output goes to ~/rvcsi-diag-<phase>.txt (and the terminal). Attach those files when reporting.
# Prerequisites:  pkg install root-repo; pkg install iw iproute2 binutils termux-api
phase=${1:?usage: diag.sh A|B|C}
exec > >(tee "$HOME/rvcsi-diag-$phase.txt") 2>&1
sec(){ printf '\n===== %s =====\n' "$*"; }
run(){ printf '\n$ %s\n' "$*"; timeout 20 "$@" 2>&1 | head -${N:-60}; }
rs(){ printf '\n# su: %s\n' "$*"; timeout 25 su -c "$*" 2>&1 | head -${N:-60}; }
AW='/^Station/{m=$2} /^[ \t]*signal:/{s=$2; if(!(m in last)||last[m]!=s){chg[m]++; last[m]=s} n[m]++} END{for(m in n) printf "%s snapshots=%d signal_changes=%d last=%s\n", m, n[m], chg[m], last[m]}'
snap(){ su -c "for i in \$(seq 1 100); do iw dev $1 station dump; echo __SNAP__; sleep 0.2; done" > "$2" 2>&1; }

if [ "$phase" = A ]; then
sec identity
for p in ro.product.model ro.product.device ro.build.version.release ro.build.version.sdk ro.hardware ro.board.platform ro.soc.model ro.soc.manufacturer ro.boot.hardware.sku ro.boot.hwc; do printf '%s=%s\n' $p "$(getprop $p)"; done
rs 'id; getenforce'; { time su -c true; } 2>&1 | tail -3
sec client_link
run cat /proc/net/wireless
rs 'ip -br link'; rs 'ip -br addr'; rs 'iw dev'
rs 'iw dev wlan0 link'
rs 'iw dev wlan0 station dump'
run termux-wifi-connectioninfo
sec client_link_sampling_5Hz_20s
t0=$(date +%s.%N); snap wlan0 $HOME/sta-client.txt; t1=$(date +%s.%N)
awk "BEGIN{print \"wall_seconds_for_100_snapshots=\" $t1-$t0}"
awk "$AW" $HOME/sta-client.txt
grep -E 'beacon signal avg|signal avg|tx retries|tx failed|tx bitrate' $HOME/sta-client.txt | sort | uniq -c | sort -rn | head -20
fi

if [ "$phase" = B ]; then
sec ap_discovery
rs 'iw dev'
IF=$(su -c 'iw dev' 2>/dev/null | awk '/Interface/{i=$2} /type AP/{print i; exit}'); echo "AP_IFACE=${IF:-NONE}"
[ -z "$IF" ] && { rs 'ip -br addr'; echo "no AP-type interface visible to iw"; exit 0; }
rs "iw dev $IF info"; rs "iw dev $IF station dump"; rs "ip neigh show dev $IF"
sec sampling_quiet; t0=$(date +%s.%N); snap $IF $HOME/sta-quiet.txt; t1=$(date +%s.%N)
awk "BEGIN{print \"wall_seconds=\" $t1-$t0}"; awk "$AW" $HOME/sta-quiet.txt
sec sampling_with_ping
CIP=$(su -c "ip neigh show dev $IF" | awk '/REACHABLE|STALE|DELAY/{print $1; exit}'); echo "CLIENT_IP=${CIP:-NONE}"
[ -n "$CIP" ] && { ( su -c "ping -c 110 -i 0.2 $CIP" > $HOME/ping.txt 2>&1 & ); snap $IF $HOME/sta-ping.txt; awk "$AW" $HOME/sta-ping.txt; tail -3 $HOME/ping.txt; }
sec raw_sample; head -30 $HOME/sta-quiet.txt
fi

if [ "$phase" = C ]; then
sec regulatory
rs 'iw reg get'
sec iw_capabilities
N=40 rs "iw list | sed -n '/Supported interface modes/,/Band 1/p'"
N=40 rs "iw list | grep -E 'Band [0-9]|VHT Capabilities|HE Iftypes|HE PHY|5[0-9][0-9][0-9] MHz|6[0-9][0-9][0-9] MHz' | head -40"
N=60 rs "iw list | sed -n '/Supported commands/,/WoWLAN/p'"
sec wifi_driver_and_chip
rs 'ls /sys/class/ieee80211'; rs 'cat /sys/class/net/wlan0/device/uevent'; rs 'readlink -f /sys/class/net/wlan0/device'
rs "lsmod | grep -iE 'wlan|wifi|conn|mt7|mt66|mt79|cfg80211|gen4m'"
N=40 rs "dmesg | grep -iE 'wlan|wifi|cfg80211|conninfra|connac|wmt|mt7|mt66|mt79|gen4m' | head -40"
rs 'ls /proc/net/wlan /proc/net/wlan* /sys/kernel/debug/wlan* /sys/module/*wlan*/parameters 2>&1 | head -40'
sec driver_strings_csi_icap
for f in $(su -c "find /vendor /vendor_dlkm /system /odm -name '*wlan*.ko' -o -name '*wifi*.ko' -o -name '*conn*.ko' 2>/dev/null" | head -8); do
  echo "## $f"; su -c "strings $f | grep -ciE 'csi|icap'"; su -c "strings $f | grep -iE 'csi|icap|iwpriv|set_csi|csi_' | sort -u | head -25"
done
sec bluetooth
for p in persist.bluetooth.btsnoopenable ro.bluetooth.library_name ro.vendor.bluetooth.chip; do printf '%s=%s\n' $p "$(getprop $p)"; done
rs 'ls -l /sys/class/bluetooth /dev | grep -iE "hci|bluetooth|btfm|stpbt|bt"'
rs 'ls -ld /data/misc/bluetooth /data/vendor/bluetooth /data/misc/bluedroid 2>&1'
N=40 rs "dumpsys bluetooth_manager | head -40"
run termux-bluetooth 2>&1
sec sensors_for_contact_bcg
run termux-sensor -l
N=25 run termux-sensor -s accelerometer -d 10 -n 100
sec nfc_uwb_features
rs "pm list features | grep -iE 'uwb|nfc|wifi|bluetooth|rtt|aware|softap|passpoint'"
sec softap_concurrency
rs "dumpsys wifi | grep -iE 'concurrency|STA\\+AP|dual|SupportedFeatures|softap' | head -30"
fi
