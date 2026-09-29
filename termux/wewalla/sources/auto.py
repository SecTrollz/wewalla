import time

from .base import Source


class MultiSource(Source):
    """Run several sources side by side."""
    name = "multi"

    def __init__(self, sources):
        super().__init__()
        self.sources = sources

    def _start(self):
        for s in self.sources:
            s.start(self.sink, self.imu_sink)

    def stop(self):
        for s in self.sources:
            s.stop()
        super().stop()

    def info(self):
        return {"name": self.name, "sources": [s.info() for s in self.sources]}


class AutoSource(Source):
    """Prefer the connector APK; fall back to Termux:API polling whenever the APK is silent."""
    name = "auto"

    def __init__(self, connector, termux, extra=None, silence_s=6.0):
        super().__init__()
        self.connector, self.termux, self.extra = connector, termux, extra or []
        self.silence_s = silence_s
        self.fallback_active = False

    def _start(self):
        self.connector.start(self.sink, self.imu_sink)
        for s in self.extra:
            s.start(self.sink, self.imu_sink)
        self._spawn(self._supervise, "auto-supervisor")

    def _supervise(self):
        t0 = time.time()
        while not self._stop.is_set():
            connector_ok = self.connector.alive(self.silence_s)
            want_fallback = (not connector_ok) and (time.time() - t0 > self.silence_s) and self.termux.available()
            if want_fallback and not self.fallback_active:
                self.termux.start(self.sink, self.imu_sink)
                self.fallback_active = True
            elif not want_fallback and self.fallback_active:
                self.termux.stop()
                self.fallback_active = False
            self._stop.wait(1.0)

    def stop(self):
        super().stop()
        for s in [self.connector, self.termux] + self.extra:
            s.stop()

    def info(self):
        return {"name": self.name, "active": "termux-api" if self.fallback_active else
                ("connector" if self.connector.alive() else "waiting"),
                "connector": self.connector.info(), "termux_available": self.termux.available(),
                "termux": self.termux.info() if self.fallback_active else None,
                "extra": [s.info() for s in self.extra]}
