# Wewalla on Termux — no ESP32, no root

WiFi sensing that runs entirely on a stock Android phone inside Termux. The whole
project (Python runtime + Android connector APK) is built and run from Termux with
plain command-line tools: no Gradle, no PC, no compiled dependencies.

```
 Android (no root)                                   Termux
┌───────────────────────────┐   JSON / UDP loopback ┌─────────────────────────────┐
│ Wewalla Connector APK     │ ────────────────────► │ wewalla (pure Python)       │
│  conn  RSSI (connected AP)│   127.0.0.1:5077      │  sources → analyzer → state │
│  scan  RSSI (all APs)     │   token-authenticated │  presence / motion /        │
│  rtt   Wi-Fi RTT ranging  │                       │  breathing (gated)          │
│  imu   phone-motion gate  │   Termux:API CLI      │  HTTP dashboard + SSE       │
└───────────────────────────┘ ────────────────────► │  ADR-018 re-emit (optional) │
   fallback: termux-wifi-*    (no APK needed)       └──────────────┬──────────────┘
                                                                   │ UDP :5005 (optional)
              real CSI from other hardware (ADR-018 over UDP) ─────┘   Rust sensing-server
```

## Read this first: what this can and cannot do

**Stock Android does not expose CSI.** There is no API for it; extracting it needs root plus
patched Wi-Fi firmware (Nexmon) on a few chipsets. Since root is off the table, the phone-native
sources produce **derived** signals — RSSI, per-AP scan levels and 802.11mc RTT distances — not channel
state information. Everything they emit is labelled `derived-… (NOT CSI)`, and frames re-emitted as
ADR-018 carry flag bit 7 (`0x80`) so nothing downstream can mistake them for real CSI.

| Capability | Phone-native (derived) | Real CSI source (`adr018`) |
|---|---|---|
| Presence / motion | Yes, with calibration | Yes |
| Breathing rate | Only if the data updates ≥ ~1 Hz (RTT mode, some phones). Otherwise it reports `insufficient_update_rate` instead of guessing | Yes (≥ 1 Hz) |
| Heart rate | **Never** (`unsupported_by_source`) | Experimental, needs ≥ 15 Hz |
| Through-wall / pose | No | Out of scope here (use the Rust stack) |

Why so limited: Android refreshes connected-AP RSSI every ~1–3 s, throttles scans (turn off
*Developer options → Wi-Fi scan throttling* to help), and RTT only works against 802.11mc APs
(many phones and few home routers). The analyzer measures the *real* update rate and refuses
vitals it cannot support.

**Weighting and the confidence score.** Motion is a *weighted* median across series: each series
votes with `kind weight × update-rate factor` (CSI 1.0, RTT 0.6, RSSI 0.35; the rate factor is
fresh-update rate ÷ 2 Hz, floored at 0.1), so one fast real-CSI stream is not drowned out by many
slow scan entries. `state.confidence` (0–1, `low`/`medium`/`high`, shown on the dashboard) combines
signal kind (a ceiling: RSSI-only never reaches `high`), series count, update rate and baseline
(calibrated/adaptive). Both the weights and the
score are **heuristics** (`CLAIMED`, not `MEASURED`): they say how well the inputs suit the detector,
**not** how often it is right. Tune `GROUP_WEIGHT` in `wewalla/dsp.py` against labelled captures.

Measured on **synthetic** data only (`SYNTHETIC` per the repo's claim rules; not real-phone accuracy):
breathing false alarms on noise-only input were 0–1.3% in normal regimes and ~6% under extreme random-walk
drift; a 0.4 dB tone at 9–24 BPM was recovered in 100% of trials. Real-world numbers must be measured on
your phone and your room. **Nothing here has been run on a physical Android device by the author of this
patch** (see *Verification status*).

**Real CSI without an ESP32:** point any ADR-018 producer at UDP `:5005` — e.g. a Raspberry Pi with
Nexmon-CSI, an Intel AX200 Linux box or a CSI-patched router, each behind a small converter that
writes the frame format documented in `wewalla/adr018.py`. Run `wewalla run --source adr018`
(or `--with-csi` in auto mode). Frames flagged as derived are rejected on that input.

## Quick start (in Termux)

Run everything in **plain Termux** (the normal Termux shell, prompt `~ $`), not inside a
`proot-distro` Linux: `wewalla` needs Termux:API and Android's `am`/`pm`, and your settings live in
Termux's `~/.wewalla/` (pairing token, signing key, calibration).

**One-time setup**

```sh
pkg install git && git clone https://github.com/SecTrollz/wewalla && cd wewalla/termux
./install.sh --apk        # python, termux-api, build tools; builds the connector APK; links `wewalla`
cp ~/.wewalla/wewalla-connector.apk ~/storage/downloads/   # needs `termux-setup-storage` once
```

1. Install the **Termux:API app** (same source as Termux, e.g. F-Droid), open it once, allow
   *Location*, keep Location ON.
2. Install the connector: open **Files → Download → wewalla-connector.apk → Install**.
   (Opening the installer from Termux can fail on Android 16 with *problem parsing the package*.)
3. First time only: open **Wewalla Connector**, tap *1. Grant permissions* and allow everything.

**Every time**

```sh
wewalla up            # starts the connector app, picks sources, opens the dashboard
```

It prints a checklist of what is running and why anything is not, then status lines every 5 s.
The dashboard is <http://127.0.0.1:8080/>. Ctrl+C stops sensing; `wewalla connector stop` also stops
the app. For heart/breathing, lay the phone **flat on the mattress beside your chest** and lie still:
readings appear after ~20 s (heart) and ~40 s (breathing). `wewalla up --no-bed` skips that.

Optional: `wewalla calibrate --source connector` (leave the room for 60 s) improves presence.
`wewalla doctor` checks the whole setup. Everything `up` does can be done by hand with
`wewalla run --source connector,bed` (see *Commands*). Try it without a phone: `wewalla run --source sim`.

Keep the phone **still** (propped up, not in hand): the accelerometer gate freezes analysis while it moves.
Run `termux-wake-lock` (done automatically by `run`) and exempt Termux and the connector from battery optimisation.

## Commands

| Command | Purpose |
|---|---|
| `wewalla run [--source auto\|connector\|termux\|adr018\|sim\|replay[,…]]` | sensing + dashboard + JSON API |
| `--emit-udp HOST:PORT` | forward derived ADR-018 frames (e.g. to the Rust `sensing-server`) |
| `--record FILE` / `--source replay --replay FILE` | capture / replay sample streams |
| `--alerts [--speak]` | `termux-notification` / vibrate / TTS on presence changes |
| `wewalla calibrate [--seconds N]` | empty-room baseline |
| `wewalla pair`, `wewalla connector install\|start\|stop\|status`, `wewalla doctor` | setup helpers |

HTTP: `GET /api/v1/state`, `/api/v1/history`, `/events` (SSE), `/api/v1/health`, `POST /api/v1/calibrate`.

## Security model

* Connector → Termux datagrams carry a random pairing token (`~/.wewalla/token`, mode 0600) and
  are dropped without it, because loopback is shared with every app on the phone.
* The service is not exported; sensing can only be started through the visible launcher activity.
* HTTP binds to loopback by default; `Host` is validated (anti DNS-rebinding); `POST` needs the token.
  Binding elsewhere (`--http 0.0.0.0:8080`) requires the token for everything except `/health`.
  The pairing token travels in plaintext HTTP there — use only on a network you trust.
* The APK is signed with a local key generated on your device (`~/.wewalla/debug.keystore`). Keep it to
  update the app in place. The only external download is `android.jar`, pinned by SHA-256.
* Location permission is required by Android for Wi-Fi scan/RTT. Nothing leaves the device unless you
  choose `--emit-udp` or `--http` on a non-loopback address.

## Tests

```sh
python3 -m unittest discover -s tests -t .   # 26 tests: codec, DSP, sockets, fake termux-* binaries, HTTP, e2e
tests/test_apk_build.sh                      # builds the APK and checks manifest facts Android 14+ enforces
```

## Verification status (honest)

Verified on a Linux dev box with the same tool families Termux ships: full unit/integration suite, the
APK compiles against the real Android 34 SDK stubs, dexes, packages, signs (v2/v3) and its manifest was
inspected. **Not verified on a real device:** APK installation and runtime behaviour, actual RSSI/RTT
rates on your phone, the `aapt2`/`d8` build branches (the `aapt`/`dx` branch is the tested one), and
Termux package names/versions drifting. If a step fails, `wewalla doctor` and `connector-apk/build.sh`
print what is missing. Please report real-device findings.

## Room map (AR + Wi-Fi)

`wewalla up` (or `run`) also serves a room map at <http://127.0.0.1:8080/map>. Open it in **Chrome**
on the phone and tap **Start AR mapping**, then walk slowly around the room pointing the camera at
walls, floor and furniture. It uses ARCore through WebXR:

- **Geometry (MEASURED by the camera):** the 6-DoF track (your path), detected planes, and a depth
  point cloud voxelised to 0.1 m. Columns are classified wall / furniture / low by height. Aim the
  centre circle at something and tap **Tag here** to label a bed, sofa, router, door, etc.
- **Wi-Fi radio map:** every fresh RSSI reading is stamped with where the phone was at that instant,
  building a per-access-point signal map on a 0.5 m grid ("signal objects"). This also lets the map
  estimate where the **phone** is from Wi-Fi alone (k-nearest fingerprint) once you have walked a bit.
- **GPS anchor:** one location fix georeferences the whole map (coarsely, ~15 m) so it can be placed
  against a world map such as OpenStreetMap. GPS does not locate objects to that precision.
- **People:** shown at **room level only** (the presence border), not as positioned bodies. Deriving
  body position or pose needs real CSI (see below), which this phone's Wi-Fi chip does not expose.
- **Heart/breathing** of a person on the bed come from the bed accelerometer (contact), and are drawn
  on the bed marker when the `bed` source is running.

Depth and plane detection depend on the device's ARCore support; the page reports what it actually
got. The whole map is stored privately on the phone at `~/.wewalla/map.json` (mode 600) and never
leaves it. **New map** starts a fresh AR origin (one map = one continuous AR session).

## Relationship to the upstream stack

The Rust workspace in `../v2` (the heavy `sensing-server`, ML crates) is untouched and remains
the path for real CSI, pose and the ESP32 firmware, which stays in-tree but is now optional. It needs Rust 1.89 and a large dependency
tree, so it is not part of the Termux build. This runtime speaks its ADR-018 wire format, so
`--emit-udp <host>:5005` lets a phone act as a (derived-data) node for it.
