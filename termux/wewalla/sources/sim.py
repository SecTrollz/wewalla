"""SYNTHETIC data for demos and tests. Everything it emits is labelled sim:*."""
import math
import random
import time

from ..model import G_RSSI, Sample
from .base import Source


class SimSource(Source):
    name = "sim"

    def __init__(self, scenario="cycle", fs=4.0, n_ap=6, seed=1):
        super().__init__()
        self.scenario, self.fs, self.n_ap = scenario, fs, n_ap
        self.rnd = random.Random(seed)
        self.drift = [0.0] * n_ap
        self.walk = [0.0] * n_ap
        self.t_start = None

    def phase(self, t):
        if self.scenario != "cycle":
            return self.scenario
        x = (t - self.t_start) % 150
        return "empty" if x < 60 else ("walk" if x < 80 else "breathe")

    def gen(self, t):
        ph, out = self.phase(t), []
        for i in range(self.n_ap):
            self.drift[i] += self.rnd.gauss(0, 0.02)
            v = -45 - 5 * i + self.drift[i] + self.rnd.gauss(0, 0.4)
            if ph == "walk":
                self.walk[i] = 0.9 * self.walk[i] + self.rnd.gauss(0, 1.6)
                v += self.walk[i]
            elif ph == "breathe":
                v += 0.8 * math.sin(2 * math.pi * 0.25 * t + i)
            out.append(Sample(t, "sim:ap%d" % i, round(v), G_RSSI, True, 2412 + 5 * i))
        return out

    def _start(self):
        self.t_start = time.time()
        self._spawn(self._loop, "sim")

    def _loop(self):
        while not self._stop.is_set():
            for s in self.gen(time.time()):
                self.sink(s)
            self.imu_sink(0.02)
            self._stop.wait(1.0 / self.fs)

    def info(self):
        return {"name": self.name, "scenario": self.scenario, "SYNTHETIC": True}
