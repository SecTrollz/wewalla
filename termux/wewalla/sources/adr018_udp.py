"""Real CSI from any hardware that can emit ADR-018 frames over UDP (not just ESP32):
e.g. a Nexmon-CSI Raspberry Pi, an Intel AX200 box, a CSI-patched OpenWrt router, each
behind a tiny converter. Every I/Q subcarrier becomes its own amplitude series.
Frames flagged 'derived' (bit 7) are rejected so derived data can never masquerade as CSI."""
import socket
import time

from .. import adr018
from ..model import G_CSI, Sample
from .base import Source


class Adr018Source(Source):
    name = "adr018"

    def __init__(self, host="0.0.0.0", port=5005):
        super().__init__()
        self.host, self.port = host, port
        self.frames = self.bad = self.derived_rejected = 0
        self.sock = None

    def bind(self):
        s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        s.bind((self.host, self.port))
        s.settimeout(0.5)
        self.sock = s
        self.port = s.getsockname()[1]
        return self.port

    def _start(self):
        if self.sock is None:
            self.bind()
        self._spawn(self._loop, "adr018-rx")

    def stop(self):
        super().stop()
        if self.sock:
            self.sock.close()
            self.sock = None

    def _loop(self):
        while not self._stop.is_set():
            try:
                data, _ = self.sock.recvfrom(4096)
            except socket.timeout:
                continue
            except OSError:
                return
            self.handle(data)

    def handle(self, data, now=None):
        now = time.time() if now is None else now
        try:
            f = adr018.decode(data)
        except ValueError:
            self.bad += 1
            return
        if f is None:
            return
        if f.derived:
            self.derived_rejected += 1
            return
        self.frames += 1
        for ant, sc, amp in f.amplitudes():
            self.sink(Sample(now, "csi:%d:%d:%d" % (f.node_id, ant, sc), amp, G_CSI, True, f.freq_mhz))

    def info(self):
        return {"name": self.name, "listening": "%s:%s" % (self.host, self.port), "frames": self.frames,
                "bad": self.bad, "derived_rejected": self.derived_rejected}
