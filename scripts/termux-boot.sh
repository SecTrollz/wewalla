#!/data/data/com.termux/files/usr/bin/sh
# Install as ~/.termux/boot/rvcsi.sh (requires the Termux:Boot app) to start the node,
# and a local broker, at device boot. Without a wake lock Android suspends the Wi-Fi
# polling within minutes of the screen turning off.
termux-wake-lock
mosquitto -d -c "$PREFIX/etc/mosquitto/mosquitto.conf" 2>/dev/null
exec rvcsi-node run --config "$HOME/.rvcsi/config.json" >>"$HOME/.rvcsi/node.log" 2>&1
