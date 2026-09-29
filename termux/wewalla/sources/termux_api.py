"""Fallback source using only the Termux:API command-line tools (no APK needed).

Honest limits: every termux-* call round-trips through an Android broadcast. MEASURED on a
Motorola Edge 2025 / Android 16: median 1.95 s per call (1.26-3.76 s, tests/hw_measure.py), and
Android refreshes connected-AP RSSI only every ~1-3 s, so this source can support presence/motion
but almost never breathing. The connector APK is the better source.
"""
import json
import math
import shutil
import statistics
import subprocess
import time

from ..model import G_RSSI, Sample
from .base import Source


def run_json(cmd, timeout=6.0):
    """Run a termux-* command and return parsed JSON (or None). Handles concatenated objects."""
    try:
        p = subprocess.run(cmd, capture_output=True, text=True, timeout=timeout)
    except (OSError, subprocess.TimeoutExpired):
        return None
    out = p.stdout.strip()
    if not out:
        return None
    dec, i, objs = json.JSONDecoder(), 0, []
    while i < len(out):
        while i < len(out) and out[i].isspace():
            i += 1
        if i >= len(out):
            break
        try:
            o, j = dec.raw_decode(out, i)
        except ValueError:
            return None
        objs.append(o)
        i = j
    return objs[0] if len(objs) == 1 else objs


_ACCEL_SKIP = ("linear", "uncal", "gravity", "rot", "orient", "tilt", "step", "motion", "gesture")


def pick_accelerometer(names):
    """Choose the raw accelerometer from `termux-sensor -l`. Names are vendor-specific
    (e.g. "icm45621_acc" on a Motorola Edge 2025, where "accelerometer" matches nothing)."""
    raw = [n for n in names if isinstance(n, str) and not any(k in n.lower() for k in _ACCEL_SKIP)]
    for pred in (lambda n: "accelerometer" in n.lower(), lambda n: n.lower().endswith("_acc"),
                 lambda n: "accel" in n.lower(), lambda n: "acc" in n.lower()):
        hits = [n for n in raw if pred(n)]
        if hits:
            return hits[0]
    return None


def find_accelerometer(exe=lambda n: n):
    d = run_json([exe("termux-sensor"), "-l"], 10)
    names = d.get("sensors") if isinstance(d, dict) else None
    return pick_accelerometer(names or [])


class TermuxApiSource(Source):
    name = "termux-api"

    def __init__(self, poll_s=0.4, scan_s=8.0, imu=True, exe_prefix=""):
        super().__init__()
        self.poll_s, self.scan_s, self.use_imu = poll_s, scan_s, imu
        self.exe = lambda n: exe_prefix + n
        self._last = {}
        self.calls = self.errors = 0
        self.last_error = None

    def available(self):
        return shutil.which(self.exe("termux-wifi-connectioninfo")) is not None

    def _start(self):
        self._spawn(self._conn_loop, "termux-conn")
        self._spawn(self._scan_loop, "termux-scan")
        if self.use_imu and shutil.which(self.exe("termux-sensor")):
            self._spawn(self._imu_loop, "termux-imu")

    # -- parsing (separate from I/O so tests can feed canned JSON) ---------------
    def ingest_conn(self, d, now):
        if not isinstance(d, dict) or "rssi" not in d:
            self.last_error = str(d.get("error") if isinstance(d, dict) else d)[:120]
            self.errors += 1
            return
        rssi = float(d["rssi"])
        if rssi <= -127 or rssi >= 0:
            return
        mac = str(d.get("bssid") or "connected").lower()
        key = "conn:" + mac
        fresh = self._last.get(key) != rssi
        self._last[key] = rssi
        self.sink(Sample(now, key, rssi, G_RSSI, fresh, int(d.get("frequency_mhz") or 0)))

    def ingest_scan(self, lst, now):
        if not isinstance(lst, list):
            self.errors += 1
            return
        for ap in lst:
            try:
                key = "scan:" + str(ap["bssid"]).lower()
                rssi = float(ap["rssi"])
                tok = ap.get("timestamp", now)
                fresh = self._last.get(key) != tok
                self._last[key] = tok
                self.sink(Sample(now, key, rssi, G_RSSI, fresh, int(ap.get("frequency_mhz") or 0)))
            except (KeyError, TypeError, ValueError):
                continue

    @staticmethod
    def imu_std(objs):
        mags = []
        for o in objs if isinstance(objs, list) else [objs]:
            if not isinstance(o, dict):
                continue
            for v in o.values():
                vals = v.get("values") if isinstance(v, dict) else None
                if vals and len(vals) >= 3:
                    mags.append(math.sqrt(sum(float(x) ** 2 for x in vals[:3])))
        return statistics.pstdev(mags) if len(mags) >= 3 else None

    # -- loops -----------------------------------------------------------------
    def _conn_loop(self):
        while not self._stop.is_set():
            self.calls += 1
            self.ingest_conn(run_json([self.exe("termux-wifi-connectioninfo")]), time.time())
            self._stop.wait(self.poll_s)

    def _scan_loop(self):
        while not self._stop.is_set():
            self.calls += 1
            self.ingest_scan(run_json([self.exe("termux-wifi-scaninfo")], 15), time.time())
            self._stop.wait(self.scan_s)

    def _imu_loop(self):
        acc = find_accelerometer(self.exe)
        if not acc:
            self.last_error = "no accelerometer found by termux-sensor -l"
            return
        while not self._stop.is_set():
            v = self.imu_std(run_json([self.exe("termux-sensor"), "-s", acc, "-d", "100", "-n", "10"], 8))
            if v is not None:
                self.imu_sink(v)
            self._stop.wait(0.5)

    def info(self):
        return {"name": self.name, "calls": self.calls, "errors": self.errors, "last_error": self.last_error}
