"""Receiver for the Wewalla Connector APK (JSON datagrams on 127.0.0.1).

Datagram schema (v1), all carry {"v":1,"tok":<pairing token>}:
  {"t":"hello","dev":..,"sdk":..,"rtt":bool,"modes":[..],"sent":n}
  {"t":"obs","o":[{"k":"conn|scan|rtt","id":mac,"f":MHz,"r":dBm,
                   "u":freshness_token (scan), "d":mm,"s":mm_sd,"n":ok_measurements}]}
  {"t":"imu","m":accel_magnitude_std}
Datagrams with a wrong token are dropped (localhost is shared with other apps).
"""
import hmac
import json
import socket
import time

from ..model import G_RSSI, G_RTT, Sample
from .base import Source

DEFAULT_PORT = 5077


class ConnectorSource(Source):
    name = "connector"

    def __init__(self, token, host="127.0.0.1", port=DEFAULT_PORT):
        super().__init__()
        self.token, self.host, self.port = token, host, port
        self.last_hello = None
        self.last_rx = 0.0
        self.rx = 0
        self.rejected = 0
        self._last = {}
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
        self._spawn(self._loop, "connector-rx")

    def stop(self):
        super().stop()
        if self.sock:
            self.sock.close()
            self.sock = None

    def alive(self, within=6.0):
        return (time.time() - self.last_rx) < within

    def _loop(self):
        while not self._stop.is_set():
            try:
                data, _ = self.sock.recvfrom(65535)
            except socket.timeout:
                continue
            except OSError:
                return
            self.handle(data)

    def handle(self, data, now=None):
        now = time.time() if now is None else now
        try:
            m = json.loads(data.decode("utf-8"))
            tok = str(m.get("tok", ""))
            if m.get("v") != 1 or not hmac.compare_digest(tok.encode(), self.token.encode()):
                raise ValueError("auth")
            kind = m["t"]
        except Exception:
            self.rejected += 1
            return False
        self.rx += 1
        self.last_rx = now
        if kind == "hello":
            self.last_hello = {k: m.get(k) for k in ("dev", "sdk", "rtt", "modes", "sent")}
            self.last_hello["t"] = now
        elif kind == "imu":
            self.imu_sink(float(m.get("m", 0.0)))
        elif kind == "obs":
            for o in m.get("o", []):
                self._obs(o, now)
        return True

    def _obs(self, o, now):
        try:
            k, mac = o["k"], str(o["id"]).lower()
            freq = int(o.get("f", 0))
            if k in ("conn", "scan"):
                rssi = float(o["r"])
                if rssi <= -127 or rssi >= 0:
                    return
                key = "%s:%s" % (k, mac)
                # freshness: scan results carry the AP's own timestamp; conn is "changed value"
                token = o.get("u") if k == "scan" else rssi
                fresh = self._last.get(key) != token
                self._last[key] = token
                self.sink(Sample(now, key, rssi, G_RSSI, fresh, freq))
            elif k == "rtt":
                sd = o.get("s")
                if sd is not None and float(sd) > 3000:  # >3 m std-dev: junk ranging result
                    return
                self.sink(Sample(now, "rtt:%s:d" % mac, float(o["d"]), G_RTT, True, freq))
                if o.get("r") is not None and -127 < float(o["r"]) < 0:
                    self.sink(Sample(now, "rtt:%s:r" % mac, float(o["r"]), G_RSSI, True, freq))
        except (KeyError, TypeError, ValueError):
            self.rejected += 1

    def info(self):
        return {"name": self.name, "listening": "%s:%s" % (self.host, self.port), "rx": self.rx,
                "rejected": self.rejected, "alive": self.alive(), "device": self.last_hello}
