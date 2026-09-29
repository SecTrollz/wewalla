# Wewalla on Termux: UI flow plan

Status: **proposal**, nothing below is implemented yet. It is based on reading the current
`termux/` tree (commit `7438bcd`). Each phase is small enough to review and test on its own.

## 1. What exists today

A user meets **four separate surfaces**, and each one was added on its own without a shared flow.

| Surface | Entry points | Notes |
|---|---|---|
| Install | `./install.sh [--apk]`, `bash setup.sh` (8 steps, Rust build), `wewalla-setup setup` | Two installers with different package lists and goals |
| Commands | `wewalla` (Python: `up run calibrate pair connector doctor`) and `wewalla-setup` (bash: `setup doctor lite server ui ip verify adb-fix patch-installer uninstall`) | Two CLIs, two `doctor`s |
| Web | `/` dashboard, `/map` AR map, `/3d` 3D model, `/observatory` → `ui/observatory.html`; plus `wewalla-setup lite` (:3100) and `wewalla-setup ui` (:3001) | Up to three HTTP servers on three ports |
| Android | Connector APK screen: paste token, 1. Grant permissions, 2. Start sensing, Stop, Battery | Works, but the numbered steps don't match the web or CLI steps |

## 2. Concrete problems

1. **Two CLIs point at each other wrongly.** `doctor.sh` (run by `wewalla-setup doctor`) tells the
   user to run `wewalla setup --only server` and `wewalla lite`. The `wewalla` command has neither
   subcommand, so following the advice fails.
2. **Observatory shows fake data.** The dashboard links to `/observatory`, which loads
   `ui/observatory/js/main.js`. That page looks for a sensing server on `:8765`/`:3000`, finds none under
   the Termux runtime, and **falls back to `DemoDataGenerator`**: synthetic people and poses. From the
   Wewalla dashboard this looks like real output. It conflicts with the rule that WiFi sensing is never
   presented as camera-grade, and it is the most serious item here.
3. **No shared navigation.** Dashboard links: Room map, 3D, Observatory. Map links: ← dashboard. 3D links:
   dashboard, AR map. Each page has its own header, colours (`--bg` differs on 3D), and light-mode
   support (3D has none).
4. **The same room is edited in two places with different words.** `/map` has "Tag here" and
   "New map"; `/3d` has "Place…", "Move", "Delete" and "Clear". Both call `/api/v1/map/object` and
   `/api/v1/map/reset`. "Clear" on 3D wipes the AR scan too, with no warning that it's the same map.
5. **Order of actions is unclear.** Calibrate (dashboard button, 60 s empty room), AR mapping,
   bed placement and furniture placement are all independent buttons. Nothing tells the user which
   to do first or which are optional.
6. **Status is only on the dashboard.** The live/reconnecting dot and signal-kind badge are missing on
   `/map` and `/3d`, so a user mapping a room can't see that sensing stopped.
7. **`lite` duplicates the runtime.** `lite/lite.py` is a second UDP monitor with its own dashboard,
   written for ESP32 nodes. `wewalla run --source adr018` already covers receiving ADR-018 frames.

## 3. Target flow

One command, one server, one set of pages that share a header, and a clear order.

```
 install ──► first run ──► Live ──► (optional) Set up room ──► Live
   │            │                        │
   │            │                        ├─ Calibrate empty room   (60 s)
   │            │                        ├─ Scan room with AR      (/map)
   │            │                        └─ Arrange furniture      (/3d)
   │            └─ wewalla up: token, connector, sources, opens browser
   └─ ./install.sh [--apk]    (setup.sh stays only for the Rust server path)
```

### 3.1 Commands: one `wewalla`

| Keep | Change |
|---|---|
| `wewalla up` | The only command a normal user needs. Unchanged behaviour. |
| `wewalla run / calibrate / pair / connector` | Unchanged (advanced use). |
| `wewalla doctor` | Absorbs the useful checks from `doctor.sh` (ports, wake lock, adb child limits). |
| `wewalla-setup` | Renamed in help text to "Rust sensing-server tools (advanced)". Fix its doctor's advice to say `wewalla-setup setup` / `wewalla-setup lite`. `lite` is marked deprecated in favour of `wewalla run --source adr018`. |

### 3.2 Web: one shell, four tabs

Every page gets the same top bar, rendered from one shared snippet the server injects (no build step):

```
[Wewalla]  Live | Room | 3D | Setup          ● live · connector+bed · derived (not CSI)
```

| Tab | Path | Purpose | Moves here from |
|---|---|---|---|
| **Live** | `/` | Presence, motion, confidence, breathing, heart, motion chart | current dashboard minus the calibrate button |
| **Room** | `/map` | AR scan, Wi-Fi radio map, access points, tagged objects | current map |
| **3D** | `/3d` | View the room; place/move/delete furniture | current 3D, with light-mode tokens and the shared bar |
| **Setup** | `/setup` (new) | Ordered checklist with state: ① connector connected ② phone still ③ calibrated ④ room scanned ⑤ bed placed. Each row links to where you do it. Calibrate button lives here. "New map" (destructive) lives here, once. | new; reads `/api/v1/state` + `/api/v1/map` |

**Observatory:** removed from the Termux nav. `/observatory` stays reachable only with a full-width
banner "DEMO DATA: no CSI sensing server connected" whenever it falls back to the generator (a small
change in `ui/observatory/js/main.js` so the banner also helps the desktop stack).

**Map editing vocabulary** (both Room and 3D): *Add*, *Move*, *Delete*, and a single *Start a new map*
in Setup with a confirm that says what is erased (AR scan, radio map, furniture). Remove *Clear* from 3D.

### 3.3 Android connector

Match its steps to the Setup checklist: *1. Grant permissions*, *2. Start sensing*, then a line
"Open the Setup page in your browser" with the URL. The token paste stays (it's filled automatically by
`wewalla up`).

## 4. Phases

Each phase ends with `python3 -m unittest discover -s tests -t .` from `termux/` passing, plus a manual
check on a phone for anything visual (not verified by CI).

| # | Change | Files | Risk |
|---|---|---|---|
| 1 | **Honesty fix:** drop Observatory from the Termux dashboard; DEMO banner in observatory fallback | `dashboard.html`, `ui/observatory/js/main.js` | low |
| 2 | Fix `doctor.sh` advice to name the right command; deprecate `lite` in help | `doctor.sh`, `wewalla-setup` | low |
| 3 | Shared top bar + status line injected by `server.py` into all three pages; light mode on 3D | `server.py`, 3 html files, a new `shell.html` | medium (touch every page) |
| 4 | `/setup` checklist page; move Calibrate and "New map" there; remove "Clear" from 3D | new `setup.html`, `server.py`, `dashboard.html`, `map.html`, `model3d.html`, tests for the route | medium |
| 5 | Align connector APK wording and show the Setup URL | `MainActivity.java`, `test_apk_build.sh` | low |
| 6 | README: replace command tables with the flow in §3, one "Every time" line | `README.md` | low |

## 5. Decisions needed before building

1. **Observatory:** remove it from Termux entirely, or keep it behind the DEMO banner? (Plan assumes banner + no nav link.)
2. **`lite`:** deprecate now, or keep as-is for people with ESP32 nodes?
3. **Setup tab:** is a checklist page what you want, or would you rather the checklist sit at the top of Live until it's complete?
4. **Scope:** do phases 1–2 first as one small PR, then 3–6? Or all in one?
