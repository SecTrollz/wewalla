"""Record / replay of Sample streams as JSON lines (for calibration, tests, bug reports)."""
import json
import threading
import time

from ..model import Sample
from .base import Source


class Recorder:
    def __init__(self, path):
        self.f = open(path, "a", buffering=1)
        self.lock = threading.Lock()

    def __call__(self, s):
        row = {"t": round(s.t, 3), "k": s.key, "v": s.value, "g": s.group, "f": s.fresh, "q": s.freq}
        with self.lock:
            self.f.write(json.dumps(row, separators=(",", ":")) + "\n")

    def close(self):
        self.f.close()


class ReplaySource(Source):
    name = "replay"

    def __init__(self, path, speed=1.0):
        super().__init__()
        self.path, self.speed = path, speed
        self.done = False

    def _start(self):
        self._spawn(self._loop, "replay")

    def _loop(self):
        t_first = wall0 = None
        with open(self.path) as f:
            for line in f:
                if self._stop.is_set():
                    return
                try:
                    r = json.loads(line)
                except ValueError:
                    continue
                if t_first is None:
                    t_first, wall0 = r["t"], time.time()
                delay = wall0 + (r["t"] - t_first) / self.speed - time.time()
                if delay > 0 and self._stop.wait(delay):
                    return
                self.sink(Sample(time.time(), r["k"], r["v"], r.get("g", "rssi"), r.get("f", True), r.get("q", 0)))
        self.done = True

    def info(self):
        return {"name": self.name, "file": self.path, "done": self.done}
