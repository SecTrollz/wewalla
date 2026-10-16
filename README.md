# rvcsi: Wi-Fi sensing node for a phone

A single self-contained binary that turns an Android phone (built for the Motorola Edge 2025
under Termux) into a local presence / breathing sensor. Nothing leaves the device or the LAN
you point it at: no cloud, no account, no telemetry, no CDN.

```
 link sources (threads)         sliding-window DSP          local outputs
 ┌──────────────────────┐      ┌───────────────────┐      ┌────────────────────────────┐
 │ /proc/net/wireless   │      │ per-link ring     │      │ UDP JSON datagrams         │
 │ iw / su iw           │─────▶│ presence (σ/base) │─────▶│ MQTT + Home Assistant disc.│
 │ termux-wifi-*        │      │ breathing (DFT,   │      │ HTTP/WS/SSE + the web UI   │
 │ udp_ingest (RTT/CSI) │      │  multi-link fuse) │      │  (3D room model, dashboard)│
 └──────────────────────┘      └───────────────────┘      └────────────────────────────┘
```

## What this phone can and cannot give you

Stated plainly, because the physics depends on it. The probe command reports what *your* unit
exposes: `rvcsi-node probe`.

| Observable | Stock Android / Termux | Used for |
|---|---|---|
| RSSI of the connected AP | yes (`/proc/net/wireless` if SELinux allows, else Termux:API) at 2-10 Hz | presence **and** breathing |
| RSSI of neighbouring APs | yes, but Android throttles scans: ~one list per several seconds | presence only |
| Wi-Fi RTT (802.11mc) | platform API exists; **Termux:API does not expose it** | needs a companion app → `udp_ingest` |
| CSI / beamforming feedback | **not exposed** by the stock driver | needs a rooted/custom-firmware bridge → `udp_ingest` |

Consequences you should plan around:

* Breathing from RSSI is **marginal**. RSSI is integer-quantised (noise floor ≈ 0.29 dB) and a
  chest moves the link by well under 1 dB. The detector integrates over a 30 s window and fuses
  links, and its false-alarm rate is fixed by construction (see `docs/DSP.md`), but expect it to
  work at close range with a still subject, not across a house. A source with real amplitude
  resolution (RTT, CSI) over `udp_ingest` is the path to better numbers.
* The sensing geometry is **phone ↔ AP**. There is one physical link for the connected AP; extra
  scan-based links add presence coverage, not breathing.
* Nothing is fabricated. There is no pose, no node position, no synthetic signal field. A
  `synthetic` source exists for bring-up and is labelled `simulated` on every surface.

## Install (on the phone)

1. Install **Termux**, **Termux:API** and (optional) **Termux:Boot** from F-Droid (same source for
   all, or the signatures will not match). Grant the Termux:API app *Location* permission.
2. In Termux: `pkg install git`, clone this repo, then `sh scripts/termux-setup.sh`.
3. `rvcsi-node probe --config ~/.rvcsi/config.json`, then `calibrate` in an empty room,
   then `termux-wake-lock; rvcsi-node run --config ~/.rvcsi/config.json`.
4. Open the URL printed by `rvcsi-node token --url` once in the phone's browser to sign in.

Disable battery optimisation for Termux or Android will suspend polling with the screen off.

## Outputs

**UDP**: one JSON report per hop (default 2 s) to each `udp.targets` entry; link detail is dropped
rather than fragmenting the datagram.

**MQTT + Home Assistant**: publishes retained discovery configs for 9 entities
(presence, breathing detected, breathing rate, confidence, presence score, link motion, sample
rate, active links, mode) under `homeassistant/…`, state on `rvcsi/<node>/state`, availability with
a last-will on `rvcsi/<node>/status`. Run a local broker (`mosquitto` is in Termux) or point
`mqtt.host` at the broker on your LAN. The client is MQTT 3.1.1 QoS 0, reconnects with backoff, and
re-announces discovery on every connect.

**Web** (`http://127.0.0.1:8765` by default): the retained dashboard, observatory, and the 3D room
model (`/model3d`: place furniture, watch presence/motion on the holographic floor).

## Security model

* HTTP binds loopback unless `http.allow_lan=true`. There is **no TLS**; on a LAN anything on
  the path can read the token and traffic. Use loopback or a trusted network.
* Everything except static UI, `/health*` and `/login` requires the node token (256-bit, in
  `~/.rvcsi/token`, mode 0600) via `Authorization: Bearer` or an HttpOnly, SameSite=Strict cookie.
  WebSockets may use a single-use 30 s ticket. Failed logins are throttled.
* `Host` must be an IP literal or `localhost` (DNS-rebinding defence); `Origin` must match on
  POST and WebSocket upgrades; POST bodies must be `application/json`.
* Hard limits on header/body size, request time, connection count, WebSocket frame size, UDP
  datagram size and ingest rate. Paths are canonicalised and confined to the UI directory.
* `#![forbid(unsafe_code)]`. Dependencies: `serde`, `serde_json` only (plus their transitive
  crates); `Cargo.lock` is committed and CI builds with `--locked`.

Known limit: Android may kill Termux. MQTT's last-will marks the node `offline`; the node does not
trap signals (no `unsafe`/libc), so a clean `offline` is published only on normal shutdown.

## Configuration

`config/edge2025.json` is the template; unknown keys are errors. `"sources": []` means "probe and
choose". Full DSP knobs live under `dsp` (see `docs/DSP.md`). `rvcsi-node check-config` validates.

## Development

```
cargo test --release        # 94 tests: DSP statistics, parsers, MQTT vs a mock broker, HTTP/WS end-to-end
cargo clippy --release --all-targets -- -D warnings
cargo check --release --target aarch64-linux-android
node --test ui/sw.test.mjs ui/services/ws-ticket.test.mjs ui/services/websocket.service.test.mjs
rvcsi-node bench --links 8  # DSP cost
```

Measured on an x86-64 dev host (not the phone): 8 links at 10 Hz, one report step p50 ≈ 41 µs,
p99 ≈ 63 µs; ingest ≈ 170 ns/sample. Run `bench` on the device for real numbers; build there with
`RUSTFLAGS="-C target-cpu=native"` (the setup script does). Cross-compiling needs an NDK linker
(see `.cargo/config.toml`); CI type-checks the Android target but does not link or run it.

## Layout

```
src/dsp.rs      windowing, detrending, band-limited DFT tables, fused-link false-alarm threshold
src/engine.rs   per-link state, presence + breathing decisions, calibration baselines
src/ingest/     Android link backends, bounded process exec, UDP bridge, synthetic source
src/export/     UDP, MQTT 3.1.1 client, Home Assistant discovery
src/web/        strict HTTP/1.1, WebSocket, SSE, auth, map store, static server
src/node.rs     orchestration + calibration;  src/bin/rvcsi-node.rs  CLI
ui/             retained web UI and 3D room model (three.js vendored in ui/vendor)
```

## UI notes

The retained UI still contains tabs from a pose-estimation product (pose, training, model
management, `viz.html`'s demo humanoid). This node has no pose backend: those panels show
"offline", and `viz.html` falls back to its own client-side demo labelled **MOCK DATA**. Nothing in
the node feeds them.

MIT licensed (see `LICENSE`); `ui/vendor/` carries its own licences.
