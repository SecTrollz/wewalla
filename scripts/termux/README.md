# Termux (non-root, phone-only) — RSSI presence mode

Runs a coarse WiFi-RSSI occupancy/motion detector directly on an Android
phone in Termux, with no root and no ESP32 hardware. This is a real,
supported mode of this project — and also its ceiling without CSI hardware.

## What this mode is

RSSI (received signal strength) is what every WiFi chip reports natively,
with no special driver support. It carries real information about
occupancy and motion (this repo's own RSSI-only research puts it at
retaining ~95% of full-CSI accuracy for simple occupancy/counting — see
`examples/research-sota/04-rssi/`), but essentially none of the detail
needed for pose, vitals, or through-wall imaging, which need per-subcarrier
CSI.

## What this mode is not

It is **not** a phone-only replacement for the ESP32 CSI pipeline. No
non-rooted Android phone can export CSI from its own WiFi radio — this is
a hardware/driver limitation, not something fixable in this repo's code.
The pose/vitals visualizations elsewhere in this project either come from
real ESP32 CSI data, or — when no sensing server is connected — from the
documented synthetic simulation fallback (see main `README.md`, Quick
start Option 1, and `ui/mobile/README.md`'s "Offline Capable" row). That
fallback is for UI development/demos, not a claim of phone-only sensing.

## Setup

```bash
pkg install termux-api python
# Grant Termux:API the Location permission in Android settings (required
# by Android for WiFi scan results since Android 8), and make sure system
# Location services are on.
termux-wake-lock
python scripts/termux/termux_presence.py
```

Open `http://127.0.0.1:8080` on the phone. See the script's own docstring
and `--help` for tuning (`--interval`, `--window`, `--threshold`, `--port`).

### Hotspot note

If your phone's mobile hotspot is on, WiFi scanning may return nothing —
many chipsets can't do client-mode scanning and AP-mode hotspot
broadcasting at the same time. Sense with the hotspot off; turn it on
afterward only if you want another device to load the dashboard at your
phone's hotspot IP (typically `192.168.43.1:8080`).

## Files

- `termux_presence.py` — the sensor + built-in dashboard, stdlib-only, no
  torch/opencv/fastapi/sqlalchemy — those are not realistically buildable
  in Termux non-root and are not needed for this mode.
