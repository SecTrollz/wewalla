#!/usr/bin/env python3
"""
termux_presence.py — Phone-only WiFi RSSI presence sensing for Termux (non-root)

Companion script for the wewalla / RuView WiFi-DensePose project
(https://github.com/SecTrollz/wewalla), adapted for non-root Termux on
Android (built/tested against a Motorola Edge 2025 target).

WHAT THIS IS
  A standalone occupancy/motion detector using only:
    - Termux:API's `termux-wifi-scaninfo` (no root, no ESP32 hardware)
    - Python standard library (no torch/opencv/fastapi — nothing to compile)

  It repeatedly scans visible WiFi networks, tracks RSSI (signal strength)
  variance per BSSID over a sliding window, and flags "presence"/"motion"
  when nearby signals wobble more than a stillness baseline — the same
  underlying principle the wewalla repo's own RSSI-only research uses
  (see examples/research-sota/04-rssi/ in that repo): RSSI retains most of
  its usefulness for simple occupancy/counting, but none of the per-person
  detail (pose, vitals) that requires real CSI from ESP32 hardware.

WHAT THIS IS NOT
  - Not through-wall pose estimation, not breathing/heart rate, not
    per-person tracking. Those genuinely require CSI from ESP32-S3/C6
    boards — there is no way to get that from a stock, non-rooted phone.
  - Not usable while your own hotspot is active on chipsets that can't do
    concurrent AP+STA WiFi scanning (common). Test this yourself — see
    README note printed at startup.

USAGE
  pkg install termux-api python   # one-time setup
  termux-wake-lock                # keep the session alive in background
  python termux_presence.py [--interval 2.0] [--window 20] [--threshold 4.0] [--port 8080]

Then open http://127.0.0.1:8080 in the phone's browser, or from another
device connected to the phone's hotspot at http://<phone-hotspot-ip>:8080
(commonly 192.168.43.1:8080 on Android tethering).
"""

from __future__ import annotations

import argparse
import json
import statistics
import subprocess
import threading
import time
from collections import deque
from dataclasses import dataclass, field
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Deque


# --------------------------------------------------------------------------
# Scanning
# --------------------------------------------------------------------------

def scan_wifi() -> list[dict]:
    """Call termux-wifi-scaninfo and return parsed network list.

    Returns [] on any failure (missing permission, hotspot radio conflict,
    termux-api not installed, etc.) rather than raising — this script is
    meant to keep running and reporting status even when scans fail.
    """
    try:
        result = subprocess.run(
            ["termux-wifi-scaninfo"],
            capture_output=True,
            text=True,
            timeout=10,
        )
        if result.returncode != 0 or not result.stdout.strip():
            return []
        data = json.loads(result.stdout)
        if isinstance(data, list):
            return data
        return []
    except (subprocess.TimeoutExpired, FileNotFoundError, json.JSONDecodeError):
        return []


def extract_rssi(network: dict) -> float | None:
    """termux-wifi-scaninfo field names have varied across versions; check both."""
    for key in ("rssi", "level", "levelDbm"):
        if key in network and isinstance(network[key], (int, float)):
            return float(network[key])
    return None


def network_key(network: dict) -> str:
    bssid = network.get("bssid") or network.get("BSSID")
    ssid = network.get("ssid") or network.get("SSID") or "?"
    return bssid or ssid


# --------------------------------------------------------------------------
# Presence / motion detection
# --------------------------------------------------------------------------

@dataclass
class NetworkTrack:
    history: Deque[float] = field(default_factory=lambda: deque(maxlen=64))

    def push(self, rssi: float) -> None:
        self.history.append(rssi)

    def variance(self) -> float | None:
        if len(self.history) < 5:
            return None
        return statistics.pvariance(self.history)


class PresenceEngine:
    def __init__(self, window: int, threshold: float):
        self.window = window
        self.threshold = threshold
        self.tracks: dict[str, NetworkTrack] = {}
        self.lock = threading.Lock()
        self.last_scan_ok = False
        self.last_scan_time: float | None = None
        self.last_error_hint = ""
        self.presence_history: Deque[int] = deque(maxlen=120)  # last N scan cycles

    def ingest(self, networks: list[dict]) -> None:
        with self.lock:
            self.last_scan_time = time.time()
            self.last_scan_ok = len(networks) > 0
            if not self.last_scan_ok:
                self.last_error_hint = (
                    "Empty scan result. If your hotspot is ON, this chipset "
                    "likely can't scan while also acting as an access point — "
                    "turn hotspot off to sense, or check Termux:API location "
                    "permission."
                )
            else:
                self.last_error_hint = ""

            seen = set()
            for net in networks:
                rssi = extract_rssi(net)
                if rssi is None:
                    continue
                key = network_key(net)
                seen.add(key)
                track = self.tracks.setdefault(key, NetworkTrack(deque(maxlen=self.window)))
                if track.history.maxlen != self.window:
                    track.history = deque(track.history, maxlen=self.window)
                track.push(rssi)

            # Drop tracks for networks no longer visible.
            stale = [k for k in self.tracks if k not in seen]
            for k in stale:
                del self.tracks[k]

            motion_score = self._motion_score()
            self.presence_history.append(1 if motion_score >= self.threshold else 0)

    def _motion_score(self) -> float:
        variances = [t.variance() for t in self.tracks.values()]
        variances = [v for v in variances if v is not None]
        if not variances:
            return 0.0
        return max(variances)

    def snapshot(self) -> dict:
        with self.lock:
            motion_score = self._motion_score()
            present = motion_score >= self.threshold
            recent = list(self.presence_history)
            occupancy_pct = (
                round(100 * sum(recent) / len(recent), 1) if recent else 0.0
            )
            return {
                "ok": self.last_scan_ok,
                "error_hint": self.last_error_hint,
                "last_scan_time": self.last_scan_time,
                "networks_tracked": len(self.tracks),
                "motion_score": round(motion_score, 2),
                "threshold": self.threshold,
                "presence": present,
                "recent_occupancy_pct": occupancy_pct,
                "recent_window": len(recent),
            }


# --------------------------------------------------------------------------
# Web dashboard (stdlib only — no external assets, works fully offline)
# --------------------------------------------------------------------------

PAGE_TEMPLATE = """<!doctype html>
<html>
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>wewalla — Termux RSSI Presence</title>
  <meta http-equiv="refresh" content="3">
  <style>
    body {{ font-family: -apple-system, sans-serif; background: #0b0f14; color: #e6edf3;
           margin: 0; padding: 24px; }}
    h1 {{ font-size: 20px; margin-bottom: 4px; }}
    .sub {{ color: #8b98a5; font-size: 13px; margin-bottom: 24px; }}
    .card {{ background: #161b22; border: 1px solid #30363d; border-radius: 10px;
             padding: 20px; margin-bottom: 16px; }}
    .status {{ font-size: 15px; padding: 10px 16px; border-radius: 8px; display: inline-block; }}
    .present {{ background: #1a3a2a; color: #3fb950; border: 1px solid #3fb950; }}
    .idle {{ background: #21262d; color: #8b98a5; border: 1px solid #30363d; }}
    .warn {{ background: #3a2a1a; color: #d29922; border: 1px solid #d29922; }}
    .metric {{ display: flex; justify-content: space-between; padding: 6px 0;
               border-bottom: 1px solid #21262d; font-size: 14px; }}
    .metric:last-child {{ border-bottom: none; }}
  </style>
</head>
<body>
  <h1>WiFi RSSI Presence — phone-only mode</h1>
  <div class="sub">wewalla / RuView companion — Termux non-root — auto-refreshes every 3s</div>

  <div class="card">
    <div class="status {status_class}">{status_text}</div>
  </div>

  <div class="card">
    <div class="metric"><span>Scan OK</span><span>{ok}</span></div>
    <div class="metric"><span>Networks tracked</span><span>{networks_tracked}</span></div>
    <div class="metric"><span>Motion score</span><span>{motion_score} (threshold {threshold})</span></div>
    <div class="metric"><span>Recent occupancy</span><span>{recent_occupancy_pct}% of last {recent_window} scans</span></div>
    <div class="metric"><span>Last scan</span><span>{last_scan}</span></div>
  </div>

  {error_block}

  <div class="sub">
    This is RSSI-only coarse presence detection — no pose, no vitals, no
    through-wall detail. Full sensing requires ESP32-S3/C6 CSI hardware.
  </div>
</body>
</html>
"""


def render_page(snap: dict) -> str:
    if snap["presence"]:
        status_class, status_text = "present", "● Presence / motion detected"
    elif not snap["ok"]:
        status_class, status_text = "warn", "⚠ Scan not returning data"
    else:
        status_class, status_text = "idle", "○ Idle / no motion"

    last_scan = (
        time.strftime("%H:%M:%S", time.localtime(snap["last_scan_time"]))
        if snap["last_scan_time"]
        else "never"
    )

    error_block = ""
    if snap["error_hint"]:
        error_block = f'<div class="card warn" style="font-size:13px;">{snap["error_hint"]}</div>'

    return PAGE_TEMPLATE.format(
        status_class=status_class,
        status_text=status_text,
        ok=snap["ok"],
        networks_tracked=snap["networks_tracked"],
        motion_score=snap["motion_score"],
        threshold=snap["threshold"],
        recent_occupancy_pct=snap["recent_occupancy_pct"],
        recent_window=snap["recent_window"],
        last_scan=last_scan,
        error_block=error_block,
    )


def make_handler(engine: PresenceEngine):
    class Handler(BaseHTTPRequestHandler):
        def log_message(self, fmt, *args):  # silence default request logging
            pass

        def do_GET(self):
            if self.path == "/api/status":
                body = json.dumps(engine.snapshot()).encode("utf-8")
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)
                return

            body = render_page(engine.snapshot()).encode("utf-8")
            self.send_response(200)
            self.send_header("Content-Type", "text/html; charset=utf-8")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

    return Handler


# --------------------------------------------------------------------------
# Main loop
# --------------------------------------------------------------------------

def scan_loop(engine: PresenceEngine, interval: float, stop_event: threading.Event):
    while not stop_event.is_set():
        networks = scan_wifi()
        engine.ingest(networks)
        stop_event.wait(interval)


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--interval", type=float, default=2.0, help="Seconds between WiFi scans (default 2.0)")
    parser.add_argument("--window", type=int, default=20, help="Rolling samples per network for variance (default 20)")
    parser.add_argument("--threshold", type=float, default=4.0, help="RSSI variance threshold for 'presence' (default 4.0, tune to your environment)")
    parser.add_argument("--port", type=int, default=8080, help="Local dashboard port (default 8080)")
    args = parser.parse_args()

    print("wewalla / RuView — Termux RSSI presence sensor (non-root, phone-only)")
    print(f"  scan interval : {args.interval}s")
    print(f"  window        : {args.window} samples")
    print(f"  threshold     : {args.threshold}")
    print(f"  dashboard     : http://127.0.0.1:{args.port}")
    print()
    print("NOTE: if you turn on your phone's mobile hotspot, scanning may")
    print("stop returning data — many chipsets can't scan WiFi (client mode)")
    print("while simultaneously running as an access point. Test it; if scans")
    print("go empty, that's your answer for this hardware.")
    print()

    engine = PresenceEngine(window=args.window, threshold=args.threshold)
    stop_event = threading.Event()

    scanner_thread = threading.Thread(
        target=scan_loop, args=(engine, args.interval, stop_event), daemon=True
    )
    scanner_thread.start()

    server = ThreadingHTTPServer(("0.0.0.0", args.port), make_handler(engine))
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        stop_event.set()
        server.shutdown()


if __name__ == "__main__":
    main()
