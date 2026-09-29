"""Bed source: streams the raw accelerometer (termux-sensor, ~80 Hz MEASURED on a Motorola Edge
2025) into a BcgAnalyzer for heart/breathing rate while the phone lies on the mattress.

Contact sensing through the bed, NOT Wi-Fi. It also feeds the phone-motion gate, so it can be
combined with a Wi-Fi source: `--source connector,bed`. Do not combine it with `termux`, whose
own accelerometer polling would compete for the same termux-sensor session.
"""
import json
import math
import os
import signal
import subprocess
import time

from .base import Source
from .termux_api import find_accelerometer


STALL_S = 8.0   # no readings this long -> reset the sensor session and restart the stream


def kill_tree(p):
    """termux-sensor is a shell script whose child holds the stdout pipe: killing only the script
    leaves the child streaming forever (orphans that starve later runs). Kill the whole group."""
    if p is None or p.poll() is not None:
        return
    try:
        os.killpg(p.pid, signal.SIGKILL)
    except (ProcessLookupError, PermissionError, OSError):
        p.kill()
    try:
        p.wait(timeout=5)
    except subprocess.TimeoutExpired:
        pass


class BedSource(Source):
    name = "bed"

    def __init__(self, bcg, delay_ms=10, exe_prefix="", raw_path=None):
        super().__init__()
        self.bcg, self.delay_ms = bcg, delay_ms
        self.raw_path = raw_path          # optional local CSV of t,x,y,z (body data: keep it private)
        self.exe = lambda n: exe_prefix + n
        self.sensor = None
        self.readings = self.errors = self.restarts = self.stalls = 0
        self.last_error = None
        self._proc = None

    def _start(self):
        self._spawn(self._loop, "bed-accel")
        self._spawn(self._motion_loop, "bed-motion")
        self._spawn(self._watchdog, "bed-watchdog")

    def _reset_sensor(self):
        # Releases every Termux:API sensor listener, including ones left behind by a crashed or
        # killed earlier run, which otherwise keep the accelerometer and starve this stream.
        try:
            subprocess.run([self.exe("termux-sensor"), "-c"], capture_output=True, timeout=10)
        except (OSError, subprocess.TimeoutExpired):
            pass

    def _watchdog(self):
        last, since = -1, time.time()
        while not self._stop.wait(1.0):
            if self.readings != last:
                last, since = self.readings, time.time()
                continue
            p = self._proc
            if p is not None and p.poll() is None and time.time() - since > STALL_S:
                self.stalls += 1
                self.last_error = ("no accelerometer data for %ds (another app or an old session may hold "
                                   "the sensor); reset and restarted" % STALL_S)
                self._reset_sensor()
                kill_tree(p)
                since = time.time()

    def stop(self):
        self._stop.set()
        kill_tree(self._proc)
        self._reset_sensor()
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
        self._reset_sensor()
        raw = None
        if self.raw_path:
            fd = os.open(self.raw_path, os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o600)
            raw = os.fdopen(fd, "a", buffering=1 << 16)
        while not self._stop.is_set():
            try:
                self._proc = subprocess.Popen(
                    [self.exe("termux-sensor"), "-s", self.sensor, "-d", str(self.delay_ms)],
                    stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True, bufsize=1,
                    start_new_session=True)
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
                kill_tree(self._proc)
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
                "restarts": self.restarts, "stalls": self.stalls, "errors": self.errors,
                "last_error": self.last_error}
