"""Bed source: streams the raw accelerometer (termux-sensor, ~80 Hz MEASURED on a Motorola Edge
2025) into a BcgAnalyzer for heart/breathing rate while the phone lies on the mattress.

Contact sensing through the bed, NOT Wi-Fi. It also feeds the phone-motion gate, so it can be
combined with a Wi-Fi source: `--source connector,bed`. Do not combine it with `termux`, whose
own accelerometer polling would compete for the same termux-sensor session.
"""
import json
import math
import os
import subprocess
import time

from .base import Source
from .termux_api import find_accelerometer


class BedSource(Source):
    name = "bed"

    def __init__(self, bcg, delay_ms=10, exe_prefix="", raw_path=None):
        super().__init__()
        self.bcg, self.delay_ms = bcg, delay_ms
        self.raw_path = raw_path          # optional local CSV of t,x,y,z (body data: keep it private)
        self.exe = lambda n: exe_prefix + n
        self.sensor = None
        self.readings = self.errors = self.restarts = 0
        self.last_error = None
        self._proc = None

    def _start(self):
        self._spawn(self._loop, "bed-accel")
        self._spawn(self._motion_loop, "bed-motion")

    def stop(self):
        self._stop.set()
        p = self._proc
        if p and p.poll() is None:
            p.kill()
        subprocess.run([self.exe("termux-sensor"), "-c"], capture_output=True, timeout=10)
        super().stop()

    # -- parsing (separate from I/O so tests can feed canned output) -------------
    @staticmethod
    def parse_stream(lines):
        """Yield [x, y, z] from termux-sensor's stream of pretty-printed JSON objects."""
        dec, buf, depth = json.JSONDecoder(), [], 0
        for line in lines:
            depth += line.count("{") - line.count("}")
            buf.append(line)
            if depth > 0 or not "".join(buf).strip():
                continue
            text, buf, depth = "".join(buf).strip(), [], 0
            try:
                obj, _ = dec.raw_decode(text)
            except ValueError:
                continue
            for v in obj.values() if isinstance(obj, dict) else ():
                vals = v.get("values") if isinstance(v, dict) else None
                if vals and len(vals) >= 3:
                    try:
                        yield [float(vals[0]), float(vals[1]), float(vals[2])]
                    except (TypeError, ValueError):
                        pass

    # -- loops -------------------------------------------------------------------
    def _loop(self):
        self.sensor = find_accelerometer(self.exe)
        if not self.sensor:
            self.last_error = "no accelerometer found by termux-sensor -l"
            return
        raw = None
        if self.raw_path:
            fd = os.open(self.raw_path, os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o600)
            raw = os.fdopen(fd, "a", buffering=1 << 16)
        while not self._stop.is_set():
            try:
                self._proc = subprocess.Popen(
                    [self.exe("termux-sensor"), "-s", self.sensor, "-d", str(self.delay_ms)],
                    stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True, bufsize=1)
                for x, y, z in self.parse_stream(self._proc.stdout):
                    if self._stop.is_set():
                        break
                    self.readings += 1
                    t = time.time()
                    self.bcg.push(t, x, y, z)
                    if raw:
                        raw.write("%.4f,%.5f,%.5f,%.5f\n" % (t, x, y, z))
            except OSError as e:
                self.errors += 1
                self.last_error = str(e)[:120]
            finally:
                if self._proc and self._proc.poll() is None:
                    self._proc.kill()
            if not self._stop.is_set():                 # stream ended: back off, then restart it
                self.restarts += 1
                self._stop.wait(2.0)
        if raw:
            raw.close()

    def _motion_loop(self):
        while not self._stop.wait(0.5):
            if self.readings:
                # scale high-passed |a| (m/s^2) to the Wi-Fi analyzer's phone-motion units (~ std of |a|)
                self.imu_sink(math.sqrt(2.0) * self.bcg.motion())

    def info(self):
        return {"name": self.name, "sensor": self.sensor, "readings": self.readings,
                "fs": None if self.bcg.fs is None else round(self.bcg.fs, 1),
                "restarts": self.restarts, "errors": self.errors, "last_error": self.last_error}
