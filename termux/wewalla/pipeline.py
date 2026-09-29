import queue
import threading
import time
from collections import Counter, deque

from . import __version__, adr018, config
from .model import G_RSSI, Sample
from .sources.base import Source


class FrameEmitter:
    """Re-emit the derived per-AP amplitudes as ADR-018 frames (flag bit7 = derived, NOT CSI)
    so the upstream Rust sensing-server / tooling can consume this phone as a node."""

    def __init__(self, host, port, node_id=200):
        import socket
        self.addr = (host, port)
        self.node_id = node_id
        self.seq = 0
        self.sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)

    def build(self, analyzer, now):
        items = sorted((k, s) for k, s in analyzer.series.items()
                       if s.group == G_RSSI and s.pts and now - s.pts[-1][0] < 5)[:adr018.MAX_SUBCARRIERS]
        if not items:
            return None
        iq, rssis, freqs = [], [], Counter()
        for _, s in items:
            v = s.pts[-1][1]
            rssis.append(v)
            if s.freq:
                freqs[s.freq] += 1
            iq += [max(0, min(127, round((v + 100) * 2))), 0]  # amplitude = 0.5 dB steps above -100 dBm
        self.seq += 1
        freq = freqs.most_common(1)[0][0] if freqs else 0
        return adr018.encode(self.node_id, 1, len(items), freq, self.seq, max(rssis), -95, iq,
                             flags=adr018.FLAG_DERIVED)

    def send(self, analyzer, now):
        pkt = self.build(analyzer, now)
        if pkt:
            try:
                self.sock.sendto(pkt, self.addr)
            except OSError:
                pass


class Runtime:
    def __init__(self, source: Source, analyzer, emitter=None, recorder=None, alerts=None,
                 tick_s=1.0, emit_hz=5.0, bcg=None, mapstore=None):
        self.source, self.analyzer, self.emitter = source, analyzer, emitter
        self.bcg = bcg
        self.mapstore = mapstore
        self._next_save = 0.0
        self.recorder, self.alerts = recorder, alerts
        self.tick_s, self.emit_s = tick_s, 1.0 / emit_hz
        self.q = queue.Queue(maxsize=50000)
        self.dropped = 0
        self.cond = threading.Condition()
        self.seq = 0
        self.state = {"state": "starting"}
        self.history = deque(maxlen=120)
        self._cal_until = None
        self._stop = threading.Event()
        self._thread = None
        self.started = time.time()

    # -- source callbacks (other threads) ------------------------------------
    def _on_sample(self, s: Sample):
        try:
            self.q.put_nowait(s)
        except queue.Full:
            self.dropped += 1

    def _on_imu(self, v):
        try:
            self.q.put_nowait(("imu", v, time.time()))
        except queue.Full:
            self.dropped += 1

    # -- control -----------------------------------------------------------------
    def start(self):
        self.source.start(self._on_sample, self._on_imu)
        self._thread = threading.Thread(target=self._loop, name="runtime", daemon=True)
        self._thread.start()

    def stop(self):
        self._stop.set()
        if self.mapstore is not None:
            try:
                self.mapstore.save()
            except OSError:
                pass
        self.source.stop()
        if self._thread:
            self._thread.join(timeout=3)
        if self.recorder:
            self.recorder.close()

    def calibrate(self, seconds):
        """Empty-room calibration; result is saved and applied when the window ends."""
        self._cal_until = time.time() + float(seconds)
        return self._cal_until

    @property
    def calibrating(self):
        return self._cal_until is not None

    def wait_state(self, last_seq, timeout=15.0):
        with self.cond:
            if self.seq == last_seq:
                self.cond.wait(timeout)
            return self.seq, self.state

    # -- main loop -------------------------------------------------------------
    def _loop(self):
        next_tick = time.time() + self.tick_s
        next_emit = time.time() + self.emit_s
        while not self._stop.is_set():
            try:
                item = self.q.get(timeout=0.1)
                n = 0
                while True:
                    self._consume(item)
                    n += 1
                    if n >= 2000:
                        break
                    item = self.q.get_nowait()
            except queue.Empty:
                pass
            now = time.time()
            if self.emitter and now >= next_emit:
                self.emitter.send(self.analyzer, now)
                next_emit = now + self.emit_s
            if now >= next_tick:
                self._tick(now)
                next_tick = now + self.tick_s

    def _consume(self, item):
        if isinstance(item, tuple):
            self.analyzer.set_phone_motion(item[1], item[2])
            return
        self.analyzer.push(item)
        if self.mapstore is not None:
            self.mapstore.on_sample(item)
        if self.recorder:
            self.recorder(item)

    def _map_state(self, now):
        ms = self.mapstore
        current = {k: s.pts[-1][1] for k, s in self.analyzer.series.items()
                   if s.group == G_RSSI and s.pts and now - s.pts[-1][0] < 30}
        if now >= self._next_save:
            self._next_save = now + 10.0
            try:
                ms.save()
            except OSError:
                pass
        return {"poses": len(ms.poses), "cells": len(ms.cells), "wifi_placed": ms.placed,
                "ar_live": bool(ms.pose_t) and now - ms.pose_t[-1] < 3.0,
                "phone_estimate": ms.locate(current) if len(current) >= 3 else None}

    def _tick(self, now):
        prev = self.state
        res = self.analyzer.analyze(now)
        if self._cal_until is not None and now >= self._cal_until:
            cal = self.analyzer.export_calibration()
            if cal["series"]:
                config.save_calibration(cal)
                self.analyzer.calibration = cal["series"]
                res["calibration"] = "saved:%d_series" % len(cal["series"])
            else:
                res["calibration"] = "failed:no_data"
            self._cal_until = None
        if self.bcg is not None:
            res["bed"] = self.bcg.analyze(now)
        if self.mapstore is not None:
            res["map"] = self._map_state(now)
        res["calibrating"] = self.calibrating
        res["source"] = self.source.info()
        res["version"] = __version__
        res["uptime_s"] = round(now - self.started, 1)
        res["dropped"] = self.dropped
        res["series_keys"] = sorted(self.analyzer.series)[:64]
        with self.cond:
            self.seq += 1
            self.state = res
            h = {"t": round(now, 1), "motion": res["motion"], "presence": res["presence"]}
            if "bed" in res:
                h["bed_heart"] = res["bed"]["heart"]["bpm"]
                h["bed_breathing"] = res["bed"]["breathing"]["bpm"]
            self.history.append(h)
            self.cond.notify_all()
        if self.alerts:
            try:
                self.alerts.on_state(prev, res)
            except Exception:
                pass
