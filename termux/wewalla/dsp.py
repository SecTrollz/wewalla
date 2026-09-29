"""Pure-stdlib sensing DSP (no numpy: nothing to compile on a phone).

Presence / motion  : per-series window std vs an (empty-room) baseline.
Breathing / heart  : Hann-windowed Goertzel spectrum over the physiological band,
                     averaged across series, accepted only if the peak stands out
                     AND the data is fast enough to support the claim.

Nothing here invents numbers: when the data cannot support a claim the result is
None plus a machine-readable reason.
"""
import math
import statistics
import time
from collections import deque

from .model import G_CSI, G_RSSI, G_RTT

# Floors keep a dead-quiet series from producing huge ratios.
MIN_SD = {G_RSSI: 0.35, G_RTT: 25.0, G_CSI: 0.5}

# Relative trust per signal kind when series vote on motion. HEURISTIC ordering (real CSI >
# ToF ranging > coarse RSSI), not measured accuracy; tune against labelled captures.
GROUP_WEIGHT = {G_CSI: 1.0, G_RTT: 0.6, G_RSSI: 0.35}
MIN_RATE_WEIGHT = 0.1

MOTION_WIN_S = 10.0
# Slow sources (Termux:API calls take ~2 s each on real phones; scans arrive every ~10 s) get a
# longer per-series window so they still have >= 3 samples, instead of dropping to "no_data".
MAX_MOTION_WIN_S = 40.0
WIN_SPACINGS = 3.5
MOTION_FS = 2.0
KEEP_S = 90.0
STALE_S = 60.0


def resample(pts, t0, t1, fs):
    """Cell-mean resample of [(t,v)...] to a uniform grid; holds last value across gaps."""
    n = int((t1 - t0) * fs)
    if n < 2:
        return None
    sums = [0.0] * n
    cnt = [0] * n
    last = None
    for t, v in pts:
        if t < t0:
            last = v
            continue
        if t >= t1:
            break
        i = int((t - t0) * fs)
        if i >= n:
            i = n - 1
        sums[i] += v
        cnt[i] += 1
    out = []
    prev = last
    for i in range(n):
        if cnt[i]:
            prev = sums[i] / cnt[i]
        out.append(prev)
    first = next((x for x in out if x is not None), None)
    if first is None:
        return None
    return [first if x is None else x for x in out]


def weighted_median(pairs):
    """Median of [(value, weight)...]; equals statistics.median when all weights are equal."""
    pairs = sorted((v, w) for v, w in pairs if w > 0)
    if not pairs:
        return None
    half = sum(w for _, w in pairs) / 2.0
    cum = 0.0
    for i, (v, w) in enumerate(pairs):
        cum += w
        if abs(cum - half) < 1e-9 and i + 1 < len(pairs):
            return (v + pairs[i + 1][0]) / 2.0
        if cum > half:
            return v
    return pairs[-1][0]


def series_weight(ser, now):
    """Vote weight: signal-kind trust x how close the fresh-update rate is to the motion grid."""
    rate = min(1.0, ser.update_hz(now) / MOTION_FS)
    return GROUP_WEIGHT.get(ser.group, GROUP_WEIGHT[G_RSSI]) * max(MIN_RATE_WEIGHT, rate)


def _detrend(x):
    n = len(x)
    mx = (n - 1) / 2.0
    my = sum(x) / n
    num = sum((i - mx) * (v - my) for i, v in enumerate(x))
    den = sum((i - mx) ** 2 for i in range(n)) or 1.0
    b = num / den
    return [v - (my + b * (i - mx)) for i, v in enumerate(x)]


def _hann(x):
    n = len(x)
    return [v * (0.5 - 0.5 * math.cos(2 * math.pi * i / (n - 1))) for i, v in enumerate(x)]


def goertzel_power(x, fs, f):
    w = 2 * math.pi * f / fs
    c = 2 * math.cos(w)
    s1 = s2 = 0.0
    for v in x:
        s0 = v + c * s1 - s2
        s2, s1 = s1, s0
    return s1 * s1 + s2 * s2 - c * s1 * s2


def band_spectrum(x, fs, f_lo, f_hi, step):
    x = _hann(_detrend(x))
    freqs, power = [], []
    f = f_lo
    while f <= f_hi + 1e-9:
        freqs.append(f)
        power.append(goertzel_power(x, fs, f))
        f += step
    return freqs, power


def motion_window(pts):
    """Per-series motion window: 10 s, stretched to ~3.5 sample spacings for slow series."""
    if len(pts) < 4:
        return MOTION_WIN_S
    recent = [pts[i][0] for i in range(max(0, len(pts) - 21), len(pts))]
    dt = statistics.median(b - a for a, b in zip(recent, recent[1:]))
    return min(MAX_MOTION_WIN_S, max(MOTION_WIN_S, WIN_SPACINGS * dt))


class Series:
    __slots__ = ("key", "group", "freq", "pts", "fresh_ts")

    def __init__(self, key, group):
        self.key, self.group, self.freq = key, group, 0
        self.pts = deque(maxlen=4000)
        self.fresh_ts = deque(maxlen=4000)

    def update_hz(self, now, span=20.0):
        n = sum(1 for t in self.fresh_ts if t >= now - span)
        return n / span


class Analyzer:
    def __init__(self, calibration=None, on_thr=0.5, off_thr=0.3, hold_s=45.0,
                 phone_motion_thr=0.35, breathing_snr=6.0, heart_snr=6.0,
                 min_breath_hz=1.0, min_heart_hz=15.0, warmup_points=30):
        self.series = {}
        self.calibration = (calibration or {}).get("series", {}) if calibration else {}
        self.on_thr, self.off_thr, self.hold_s = on_thr, off_thr, hold_s
        self.phone_motion_thr = phone_motion_thr
        self.breathing_snr, self.heart_snr = breathing_snr, heart_snr
        self.min_breath_hz, self.min_heart_hz = min_breath_hz, min_heart_hz
        self.warmup_points = warmup_points
        self.phone_motion = 0.0
        self.phone_motion_t = 0.0
        self._sd_hist = {}
        self._last_motion = 0.0
        self.presence = False
        self.presence_since = None
        self._on_count = 0
        self._last_active = 0.0
        self.motion_history = deque(maxlen=300)
        self._cand = {}

    # ---- input -------------------------------------------------------------
    def push(self, s):
        ser = self.series.get(s.key)
        if ser is None:
            ser = self.series[s.key] = Series(s.key, s.group)
        if s.freq:
            ser.freq = s.freq
        ser.pts.append((s.t, s.value))
        if s.fresh:
            ser.fresh_ts.append(s.t)
        while ser.pts and ser.pts[0][0] < s.t - KEEP_S:
            ser.pts.popleft()

    def set_phone_motion(self, value, t=None):
        self.phone_motion = float(value)
        self.phone_motion_t = t if t is not None else time.time()

    # ---- helpers -----------------------------------------------------------
    def phone_moving(self, now):
        return (now - self.phone_motion_t) < 5.0 and self.phone_motion > self.phone_motion_thr

    def _baseline(self, key, group):
        floor = MIN_SD.get(group, 0.5)
        cal = self.calibration.get(key)
        if cal:
            return max(cal["sd"], floor), "calibrated"
        h = self._sd_hist.get(key)
        if h and len(h) >= self.warmup_points:
            v = sorted(h)
            return max(v[int(0.3 * (len(v) - 1))], floor), "adaptive"
        return None, "warming_up"

    def export_calibration(self):
        out = {}
        for key, h in self._sd_hist.items():
            if len(h) >= 10:
                v = sorted(h)
                out[key] = {"sd": v[int(0.8 * (len(v) - 1))],
                            "group": self.series[key].group if key in self.series else G_RSSI}
        return {"created": time.time(), "series": out}

    # ---- main --------------------------------------------------------------
    def analyze(self, now=None):
        now = time.time() if now is None else now
        for k in [k for k, s in self.series.items() if not s.pts or now - s.pts[-1][0] > STALE_S]:
            del self.series[k]

        moving = self.phone_moving(now)
        zs, sds, warming, modes = [], {}, 0, set()
        used = []
        for key, ser in self.series.items():
            win = motion_window(ser.pts)
            grid = resample(ser.pts, now - win, now, MOTION_FS)
            if grid is None or sum(1 for t, _ in ser.pts if t >= now - win) < 3:
                continue
            sd = statistics.pstdev(grid)
            sds[key] = sd
            base, mode = self._baseline(key, ser.group)
            modes.add(mode)
            if not moving and self._last_motion < self.off_thr:
                self._sd_hist.setdefault(key, deque(maxlen=600)).append(sd)
            if base is None:
                warming += 1
                continue
            zs.append((max(0.0, sd / base - 1.0), series_weight(ser, now)))
            used.append(ser)

        motion = None
        state = "ok"
        if moving:
            state = "phone_moving"
        elif not zs:
            state = "warming_up" if warming else "no_data"
        else:
            motion = 1.0 - math.exp(-weighted_median(zs) / 1.5)
            self._last_motion = motion
            self.motion_history.append((now, motion))
            self._update_presence(now, motion)

        breathing = self._vital(now, "breathing", 0.10, 0.50, 4.0, 40.0, 0.01,
                                self.min_breath_hz, self.breathing_snr, None, motion, moving)
        heart = self._vital(now, "heart", 0.80, 2.00, 10.0, 20.0, 0.02,
                            self.min_heart_hz, self.heart_snr, G_CSI, motion, moving)
        if breathing["bpm"] is not None and not moving:
            # A confirmed breathing rhythm is stronger evidence of a still occupant than motion.
            self._last_active = now
            if not self.presence:
                self.presence, self.presence_since = True, now
        baseline = "calibrated" if modes == {"calibrated"} else ("adaptive" if "adaptive" in modes or "calibrated" in modes else "warming_up")
        return {
            "t": now,
            "state": state,
            "presence": None if state in ("warming_up", "no_data") else self.presence,
            "presence_since": self.presence_since,
            "motion": None if motion is None else round(motion, 3),
            "baseline": baseline,
            "confidence": self.confidence(now, state, used, baseline),
            "breathing": breathing,
            "heart_rate": heart,
            "quality": self.quality(now),
        }

    def _update_presence(self, now, motion):
        if motion >= self.on_thr:
            self._on_count += 1
            self._last_active = now
            if self._on_count >= 2 and not self.presence:
                self.presence, self.presence_since = True, now
        elif motion >= self.off_thr:
            self._on_count = 0
            self._last_active = now
        else:
            self._on_count = 0
            if self.presence and now - self._last_active > self.hold_s:
                self.presence, self.presence_since = False, None

    def _vital(self, now, name, f_lo, f_hi, fs, win, step, min_hz, snr_thr, only_group, motion, moving):
        hist = self._cand.setdefault(name, deque(maxlen=3))

        def res(bpm=None, snr=None, reason=None):
            # Persistence gate (like the repo's 3-frame fall debounce): a genuine vital sign
            # holds one frequency; drift-induced spectral peaks wander.
            if bpm is None:
                hist.clear()
            else:
                hist.append(bpm)
                if len(hist) < 3 or max(hist) - min(hist) > 3.0:
                    bpm, reason = None, "confirming"
                else:
                    bpm = round(statistics.median(hist), 1)
            r = {"bpm": bpm, "snr": None if snr is None else round(snr, 2), "reason": reason}
            if name == "heart":
                r["experimental"] = True
            return r

        if moving:
            return res(reason="phone_moving")
        cands = [s for s in self.series.values()
                 if (only_group is None or s.group == only_group) and s.update_hz(now) >= min_hz]
        if not cands:
            why = "unsupported_by_source" if only_group and not any(s.group == only_group for s in self.series.values()) \
                else "insufficient_update_rate"
            return res(reason=why)
        if motion is not None and motion >= 0.5:
            return res(reason="subject_moving")
        span = min((now - s.pts[0][0]) for s in cands)
        if span < 0.75 * win:
            return res(reason="warming_up")

        specs = []
        for s in cands:
            g = resample(s.pts, now - win, now, fs)
            if g is None or statistics.pstdev(g) < 1e-9:
                continue
            _, p = band_spectrum(g, fs, f_lo, f_hi, step)
            med = statistics.median(p) or 1e-12
            specs.append([v / med for v in p])
        if not specs:
            return res(reason="no_signal_variance")
        m = len(specs)
        avg = [sum(col) / m for col in zip(*specs)]
        peak = max(avg)
        idx = avg.index(peak)
        freq = f_lo + idx * step
        med_avg = statistics.median(avg) or 1e-12
        snr = peak / med_avg
        # Null-hypothesis calibration: with ~40 noise bins, max/median is ~5 by chance for
        # one series, so the bar must be higher for few series and relax as more are averaged.
        thr = max(snr_thr, 2.4 * snr_thr / math.sqrt(m))
        # Dominance: a real vital sign towers over the best *unrelated* bump; noise peaks do not.
        guard = max(2, int(round(0.06 / step)))
        rest = [v for j, v in enumerate(avg) if abs(j - idx) > guard]
        dominance = peak / (max(rest) if rest else 1e-12)
        if snr < thr or dominance < 2.0 or idx < 2 or idx > len(avg) - 3:
            return res(snr=snr, reason="no_clear_peak")
        return res(bpm=round(freq * 60.0, 1), snr=snr)

    def confidence(self, now, state, used, baseline):
        """HEURISTIC 0..1 trust score for this reading. It is NOT a measured accuracy: it only
        says how well the inputs match what the detector needs (signal kind, how many series
        voted, their update rate, and whether the baseline is calibrated)."""
        if state != "ok" or not used:
            return {"score": 0.0, "level": "none", "factors": {}, "heuristic": True}
        n = len(used)
        f = {
            "signal": round(sum(GROUP_WEIGHT.get(s.group, GROUP_WEIGHT[G_RSSI]) for s in used) / n, 3),
            "coverage": round(min(1.0, n / 4.0), 3),
            "rate": round(min(1.0, statistics.median(s.update_hz(now) for s in used) / MOTION_FS), 3),
            "baseline": {"calibrated": 1.0, "adaptive": 0.7}.get(baseline, 0.0),
        }
        # Signal kind caps the score (RSSI-only tops out at ~0.59, i.e. never "high"); the other
        # factors combine by geometric mean so one weak factor pulls it down without zeroing it.
        rest = [max(f[k], 1e-3) for k in ("coverage", "rate", "baseline")]
        score = math.sqrt(f["signal"]) * math.prod(rest) ** (1.0 / len(rest))
        level = "high" if score >= 0.66 else ("medium" if score >= 0.33 else "low")
        return {"score": round(score, 3), "level": level, "factors": f, "heuristic": True}

    def quality(self, now):
        groups = {s.group for s in self.series.values()}
        rates = [s.update_hz(now) for s in self.series.values()]
        if G_CSI in groups:
            kind = "real-csi"
        elif G_RTT in groups:
            kind = "derived-rtt+rssi (NOT CSI)"
        elif G_RSSI in groups:
            kind = "derived-rssi (NOT CSI)"
        else:
            kind = "none"
        return {
            "signal_kind": kind,
            "series": len(self.series),
            "update_hz_median": round(statistics.median(rates), 2) if rates else 0.0,
            "update_hz_max": round(max(rates), 2) if rates else 0.0,
            "phone_motion": round(self.phone_motion, 3),
            "phone_moving": self.phone_moving(now),
        }
