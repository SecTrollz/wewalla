"""Bed ballistocardiography (BCG): heart and breathing rate from the phone's accelerometer
while the phone lies on the mattress near the sleeper. Pure stdlib, streaming.

This is CONTACT sensing through the bed, not Wi-Fi sensing. The label travels with every result.

Heart     : each heartbeat recoils the body; the mattress passes a 4-15 Hz burst to the phone.
            Band-pass each axis -> summed energy -> 5 Hz envelope -> autocorrelation over
            40-150 BPM lags. Accepted only if the rhythm is clearly periodic, the body is still,
            and three consecutive estimates agree.
Breathing : the chest tilts the mattress slowly; low-pass the accelerometer, project it on the
            gravity direction and look for a dominant 0.1-0.5 Hz peak (same gate as dsp._vital).

As everywhere in wewalla: when the data cannot support a number the result is None + a reason.
"""
import math
import statistics
import threading
import time
from collections import deque

from .dsp import band_spectrum

METHOD = "bcg-accel (bed contact, NOT Wi-Fi)"

ENV_FS = 40.0            # decimated envelope rate (40 Hz: ~0.1 BPM lag steps near 60 BPM)
ENV_LP = 5.0             # envelope smoothing; well above the 2.5 Hz beat rate at 150 BPM
RESP_FS = 10.0           # decimated respiration rate
HEART_WIN_S = 16.0
RESP_WIN_S = 40.0
BPM_LO, BPM_HI = 40.0, 150.0
MIN_FS = 40.0            # below this the 4-15 Hz band is not observable
# A jolt this large (high-passed |a|, m/s^2; ~20x the MEASURED noise floor) marks body movement.
# Readings stay blocked until the jolt has left the analysis window, or its filter ringing
# looks like a rhythm (this was the only false-alarm source in the synthetic sweep).
MOVE_SPIKE = 0.3


class Biquad:
    """RBJ-cookbook biquad (direct form I)."""
    __slots__ = ("b0", "b1", "b2", "a1", "a2", "x1", "x2", "y1", "y2")

    def __init__(self, kind, f0, fs, q=0.7071):
        if not 0 < f0 < 0.45 * fs:
            raise ValueError("biquad f0=%.2f Hz is not representable at fs=%.1f Hz" % (f0, fs))
        w = 2 * math.pi * f0 / fs
        c, alpha = math.cos(w), math.sin(w) / (2 * q)
        if kind == "lp":
            b0, b1, b2 = (1 - c) / 2, 1 - c, (1 - c) / 2
        else:
            b0, b1, b2 = (1 + c) / 2, -(1 + c), (1 + c) / 2
        a0 = 1 + alpha
        self.b0, self.b1, self.b2 = b0 / a0, b1 / a0, b2 / a0
        self.a1, self.a2 = -2 * c / a0, (1 - alpha) / a0
        self.x1 = self.x2 = self.y1 = self.y2 = 0.0

    def __call__(self, x):
        y = self.b0 * x + self.b1 * self.x1 + self.b2 * self.x2 - self.a1 * self.y1 - self.a2 * self.y2
        self.x2, self.x1, self.y2, self.y1 = self.x1, x, self.y1, y
        return y


def chain(*fs):
    def run(x):
        for f in fs:
            x = f(x)
        return x
    return run


def autocorr(x, lag_lo, lag_hi):
    n = len(x)
    m = sum(x) / n
    d = [v - m for v in x]
    den = sum(v * v for v in d) or 1e-12
    return {k: sum(d[i] * d[i + k] for i in range(n - k)) / den for k in range(lag_lo, min(lag_hi, n - 1) + 1)}


class BcgAnalyzer:
    def __init__(self, periodicity_thr=0.35, move_thr=0.25, agree_bpm=5.0):
        self.periodicity_thr, self.move_thr, self.agree_bpm = periodicity_thr, move_thr, agree_bpm
        self.lock = threading.Lock()
        self.arrivals = deque(maxlen=2000)
        self.fs = None
        self._filters = None
        self._n = 0
        # envelopes: one per axis + their sum; the heart estimate uses whichever is most periodic
        # (the heartbeat recoil lands on 1-2 axes depending on how the phone lies)
        self.env = [deque(maxlen=int(HEART_WIN_S * ENV_FS)) for _ in range(4)]
        self.resp = deque(maxlen=int(RESP_WIN_S * RESP_FS))
        self.hp_mag = deque(maxlen=200)           # recent high-passed |a| for the movement gate
        self._hist = {"heart": deque(maxlen=3), "breathing": deque(maxlen=3)}
        self.last_t = 0.0
        self.last_move_t = -1e18

    # ---- input ---------------------------------------------------------------
    def _design(self, fs):
        self.fs = fs
        heart_ok = fs >= MIN_FS          # the 4-15 Hz band only exists above ~30 Hz sampling
        bp = lambda: chain(Biquad("hp", 4.0, fs), Biquad("hp", 4.0, fs),
                           Biquad("lp", 15.0, fs), Biquad("lp", 15.0, fs))
        self._filters = {
            "bp": [bp(), bp(), bp()] if heart_ok else None,
            "env": [chain(Biquad("lp", ENV_LP, fs), Biquad("lp", ENV_LP, fs)) for _ in range(4)] if heart_ok else None,
            "resp": [chain(Biquad("lp", 0.7, fs), Biquad("lp", 0.7, fs)) for _ in range(3)],
            "mag": Biquad("hp", 1.0, fs),
        }
        for d in self.env:
            d.clear()
        self.resp.clear()
        self._n = 0

    def _rate(self):
        if len(self.arrivals) < 50:
            return None
        span = self.arrivals[-1] - self.arrivals[0]
        return (len(self.arrivals) - 1) / span if span > 0 else None

    def push(self, t, x, y, z):
        with self.lock:
            self.arrivals.append(t)
            self.last_t = t
            fs = self._rate()
            if fs is None:
                return
            if self.fs is None or abs(fs - self.fs) / self.fs > 0.15:
                self._design(fs)
            f = self._filters
            self._n += 1
            env = None
            if f["bp"]:
                e = [bp(v) ** 2 for bp, v in zip(f["bp"], (x, y, z))]
                env = [lp(v) for lp, v in zip(f["env"], e + [sum(e)])]
            r = [lp(v) for lp, v in zip(f["resp"], (x, y, z))]
            hp = abs(f["mag"](math.sqrt(x * x + y * y + z * z)))
            self.hp_mag.append(hp)
            if hp > MOVE_SPIKE and self._n > 2 * self.fs:     # skip the filter's start-up transient
                self.last_move_t = t
            if env and self._n % max(1, round(self.fs / ENV_FS)) == 0:
                for d, v in zip(self.env, env):
                    d.append(v)
            if self._n % max(1, round(self.fs / RESP_FS)) == 0:
                self.resp.append(r)

    def motion(self):
        """Recent movement level (m/s^2, high-passed |a| std-ish) for the phone_moving gate."""
        with self.lock:
            return statistics.fmean(self.hp_mag) if self.hp_mag else 0.0

    # ---- output --------------------------------------------------------------
    def _res(self, name, bpm=None, reason=None, **extra):
        h = self._hist[name]
        if bpm is None:
            h.clear()
        else:
            h.append(bpm)
            if len(h) < 3 or max(h) - min(h) > self.agree_bpm:
                bpm, reason = None, "confirming"
            else:
                bpm = round(statistics.median(h), 1)
        out = {"bpm": bpm, "reason": reason, "method": METHOD}
        out.update(extra)
        if name == "heart":
            out["experimental"] = True
        return out

    def analyze(self, now=None):
        now = time.time() if now is None else now
        with self.lock:
            fs, env, resp = self.fs, [list(d) for d in self.env], list(self.resp)
            moving = statistics.fmean(self.hp_mag) if self.hp_mag else 0.0
            stale = now - self.last_t > 3.0
            since_move = now - self.last_move_t
        base = {"fs": None if fs is None else round(fs, 1), "motion": round(moving, 4)}
        if stale or fs is None:
            why = "no_data" if stale else "warming_up"
            return dict(base, state=why, heart=self._res("heart", reason=why),
                        breathing=self._res("breathing", reason=why))
        if fs < MIN_FS:
            return dict(base, state="ok", heart=self._res("heart", reason="insufficient_update_rate"),
                        breathing=self._breathing(resp))
        if moving > self.move_thr:
            return dict(base, state="subject_moving", heart=self._res("heart", reason="subject_moving"),
                        breathing=self._res("breathing", reason="subject_moving"))
        heart = self._heart(env) if since_move > HEART_WIN_S else self._res("heart", reason="settling_after_movement")
        breathing = self._breathing(resp) if since_move > RESP_WIN_S else self._res("breathing", reason="settling_after_movement")
        return dict(base, state="ok" if since_move > HEART_WIN_S else "settling", heart=heart, breathing=breathing)

    @staticmethod
    def _periodicity(env):
        """(score, lag, ac) of the heart-range rhythm in one envelope, or (None, None, ac).

        A steady beat correlates almost equally at T, 2T, 3T..., so "highest peak" is a coin toss
        between the true rate and half of it. Rule: take the SHORTEST lag scoring within 20% of the
        best (then step down to T/2 while a clear half-lag peak exists), then require the rhythm to repeat (ac at 2T still >= half of ac at T); a noise bump
        does not repeat. The score is the weaker of the two, so a fluke cannot pass on one lag."""
        # scan a little past both ends so a rhythm right at 40/150 BPM is still a local maximum
        lo, hi = int(ENV_FS * 60 / (BPM_HI * 1.1)), int(math.ceil(ENV_FS * 60 / (BPM_LO * 0.9)))
        ac = autocorr(env, lo, 2 * hi + 2)
        peaks = [k for k in range(lo + 1, hi) if ac[k] >= ac[k - 1] and ac[k] >= ac[k + 1]]
        if not peaks:
            return None, None, ac
        top = max(ac[k] for k in peaks)
        if top <= 0:
            return top, None, ac
        k = min(k for k in peaks if ac[k] >= 0.8 * top)
        # octave check: a clear peak at exactly half the lag means we locked onto 2T. For a true
        # rhythm at T, ac(T/2) is strongly negative (beats are not there), so noise cannot fake it.
        while True:
            half = [j for j in peaks if abs(j - k / 2.0) <= 2 and ac[j] >= 0.5 * ac[k]]
            if not half:
                break
            k = max(half, key=lambda j: ac[j])
        repeat = max(ac.get(2 * k + d, -1.0) for d in (-1, 0, 1))
        return min(ac[k], 2.0 * repeat), k, ac

    def _heart(self, envs):
        if len(envs[3]) < 0.9 * HEART_WIN_S * ENV_FS:
            return self._res("heart", reason="warming_up")
        tot = envs[3]
        if max(tot) > 40 * (statistics.fmean(tot) or 1e-12):   # one twitch dominates the window
            return self._res("heart", reason="subject_moving")
        best = (None, None, None, None)
        for axis, env in zip(("x", "y", "z", "sum"), envs):
            p, k, ac = self._periodicity(env)
            if p is not None and (best[0] is None or p > best[0]):
                best = (p, k, ac, axis)
        p, k, ac, axis = best
        if p is None or k is None or p < self.periodicity_thr:
            return self._res("heart", reason="no_clear_rhythm", periodicity=None if p is None else round(p, 2))
        a, b, c = ac[k - 1], ac[k], ac[k + 1]
        den = a - 2 * b + c
        kk = k + (0.5 * (a - c) / den if den else 0.0)
        bpm = 60.0 * ENV_FS / kk
        if not BPM_LO <= bpm <= BPM_HI:
            return self._res("heart", reason="out_of_range", periodicity=round(p, 2))
        return self._res("heart", bpm=round(bpm, 1), periodicity=round(p, 2), axis=axis)

    def _breathing(self, resp):
        if len(resp) < 0.9 * RESP_WIN_S * RESP_FS:
            return self._res("breathing", reason="warming_up")
        g = [statistics.fmean(r[i] for r in resp) for i in range(3)]
        gn = math.sqrt(sum(v * v for v in g)) or 1.0
        sig = [sum(r[i] * g[i] for i in range(3)) / gn for r in resp]
        freqs, p = band_spectrum(sig, RESP_FS, 0.1, 0.5, 0.01)
        med = statistics.median(p) or 1e-12
        idx = max(range(len(p)), key=p.__getitem__)
        snr = p[idx] / med
        rest = [v for j, v in enumerate(p) if abs(j - idx) > 6]
        dominance = p[idx] / (max(rest) if rest else 1e-12)
        if snr < 8.0 or dominance < 2.0 or idx < 2 or idx > len(p) - 3:
            return self._res("breathing", reason="no_clear_rhythm", snr=round(snr, 2))
        return self._res("breathing", bpm=round(freqs[idx] * 60.0, 1), snr=round(snr, 2))
