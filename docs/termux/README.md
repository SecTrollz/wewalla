# wewalla on Android (Termux, no root)

## What the phone can and cannot do

A non-root phone **cannot capture WiFi CSI from its own radio**. The working setup is:

```
ESP32-S3 node(s)  --UDP-->  phone (Termux)  -->  browser on the same phone
   (the sensors)            (the host)            http://127.0.0.1:3100
```

The phone receives and displays; ESP32-S3 boards do the sensing. Without boards you can still run the
verification proof and the coarse phone-RSSI hint (experimental, not real CSI sensing).

This is a research prototype. It is **not** a medical device or safety system. Pose and vitals accuracy
is limited (see the main README's "Model weights: what's real, what's not"). Only sense spaces where
everyone present has agreed.

## Step by step

1. **Install Termux from F-Droid or the GitHub releases page**, plus Termux:API from the same source.
   Do not mix sources: the apps will not trust each other.
2. In Termux: `pkg update && pkg install -y git`, then
   `git clone https://github.com/SecTrollz/wewalla && cd wewalla`
3. Run `bash termux/setup.sh`. It is resumable; re-run it any time and finished steps are skipped.
   Use `--yes` to accept defaults, `--force` to repeat a step, `--only server` to run one step.
4. Do the two keep-alive settings it prints (Termux battery = Unrestricted; disable child process
   restrictions). If your Android has no such toggle, run `wewalla adb-fix`.
5. Give the phone a fixed address (DHCP reservation on your router), or use the phone's hotspot.
6. Flash and provision each ESP32-S3 from a computer with the command setup prints (it fills in your
   phone's address as `--target-ip`).
7. `wewalla lite`, then open http://127.0.0.1:3100. Nodes appear when packets arrive.
   `wewalla doctor` shows what is wrong if they do not.

Optional: `bash termux/patch-install-sh.sh` makes `./install.sh` hand off to the Termux installer.

## Commands

| Command | Purpose |
|---|---|
| `wewalla setup` | guided install |
| `wewalla doctor` | read-only health check (exit 1 on failure) |
| `wewalla lite [--rssi] [--forward HOST:PORT] [--allow CIDR]` | browser monitor |
| `wewalla server [args]` | run the sensing server (`--help` for its options) |
| `wewalla ui` | serve `./ui` on 127.0.0.1:3001 |
| `wewalla verify` | deterministic pipeline proof (expects `VERDICT: PASS`) |
| `wewalla adb-fix` | lift Android's child-process limit using on-device Wireless debugging |
| `wewalla uninstall` | remove `~/.wewalla` and the command |

Running the real sensing server and `lite` together: both want UDP 5005 and lite's page is on 3100.
Start the server on another UDP port, then `wewalla lite --forward 127.0.0.1:<that port>`.

## Security defaults

- The dashboard binds to `127.0.0.1` and rejects unknown `Host` headers. `--http-bind 0.0.0.0` exposes
  it, without a login, to your network.
- UDP is accepted only from private/LAN addresses unless you pass `--allow`.
- Prebuilt binaries are only installed after a SHA-256 check.

## Troubleshooting

| Symptom | Likely cause and fix |
|---|---|
| `pkg` cannot find packages | `termux-change-repo`, then `pkg update` |
| No phone address | install the Termux:API app and grant it Location permission |
| Page loads, no nodes | node `--target-ip` wrong, different subnet, or router client isolation |
| Termux dies in the background | battery Unrestricted, wake lock, child-process restriction (step 4) |
| `pip install ruview` fails | its wheel targets glibc; Termux is bionic. Build from source with maturin, or use `proot-distro` Debian |
| Build from source is killed | not enough RAM; use the prebuilt binary or `WW_JOBS=1` |

## Not supported on Termux

Docker, ESP-IDF firmware builds, the WASM/`browser` and `field` install profiles, and PyTorch-based
training. Use a computer for those.
