"""SYNTHETIC bed-BCG generator for tests. Noise floor and resolution are the values MEASURED on a
Motorola Edge 2025 lying flat (sd ~0.01 m/s^2 per axis, 0.00195 m/s^2 steps, 80 Hz stream)."""
import math
import random


def bed_signal(bpm=62.0, dur=60.0, fs=80.0, amp=0.03, noise=0.012, resp_bpm=15.0, resp_amp=0.03,
               seed=0, twitch_at=None, heart=True):
    rnd = random.Random(seed)
    beats, t = [], rnd.uniform(0, 0.5)
    while t < dur:
        beats.append(t)
        t += 60.0 / bpm * rnd.uniform(0.97, 1.03)          # ~3% heart-rate variability
    q = 0.00195
    out, bi = [], 0
    for i in range(int(dur * fs)):
        t = i / fs
        while bi + 1 < len(beats) and beats[bi + 1] <= t:
            bi += 1
        s = 0.0
        if heart and beats and t >= beats[bi]:
            dt = t - beats[bi]
            s = amp * math.exp(-dt / 0.07) * math.sin(2 * math.pi * 8.0 * dt)
        tilt = resp_amp * math.sin(2 * math.pi * resp_bpm / 60.0 * t)
        x = 1.10 + tilt * 0.3 + rnd.gauss(0, noise)
        y = -0.16 + s * 0.8 + rnd.gauss(0, noise)
        z = 9.78 + s + tilt + rnd.gauss(0, noise * 1.5)
        if twitch_at is not None and twitch_at <= t < twitch_at + 1.5:
            x += rnd.gauss(0, 1.5); y += rnd.gauss(0, 1.5); z += rnd.gauss(0, 1.5)
        out.append((t, round(x / q) * q, round(y / q) * q, round(z / q) * q))
    return out


def run(an, samples, t0=1e6, jitter=0.0, seed=1):
    """Push samples with arrival-time jitter (termux-sensor delivers in small bursts); analyze at 1 Hz."""
    rnd = random.Random(seed)
    res, nxt = [], t0 + 1.0
    for t, x, y, z in samples:
        ta = t0 + t + (rnd.uniform(0, jitter) if jitter else 0.0)
        an.push(ta, x, y, z)
        if t0 + t >= nxt:
            res.append(an.analyze(t0 + t))
            nxt += 1.0
    return res
