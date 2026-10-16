# DSP notes

All stages run per report hop (default 2 s) on the last 30 s of each link, in `f32`, with no
allocation after start-up.

## 1. Uniform resampling

Android delivers irregular samples. For the breathing path each link is box-averaged into
`window_secs × fs_hz` = 30 × 2 = 60 bins ending at "now". Box averaging, not point sampling, is
the anti-alias filter when the source is faster than 2 Hz. Empty bins are bridged linearly; a
window with under 70 % of bins populated is rejected, never interpolated into a result.

A link joins the breathing spectrum only if its measured raw rate is ≥ 2 Hz, the analysis grid rate
(a slower link cannot fill 70 % of the grid; an earlier 4 Hz grid made 2 Hz links silently undetectable,
found by `selftest`). The grid's Nyquist limit is 1 Hz, above the 0.6 Hz band edge. The rate is `(n-1)/span` of the samples actually
received, so a slow Wi-Fi-scan link can never be mistaken for a fast one.

## 2. Spectrum

Linear detrend (least squares over the window), then a **Hann-windowed direct DFT over the
breathing band only**: 0.10–0.60 Hz at 0.01 Hz = 51 bins (6–36 breaths/min). The window and
twiddles are folded into two tables (`51 × 60` floats each, ≈ 24 KB total) built once, so a
step is 102 dot products of length 60. The dot product keeps 8 independent accumulators so LLVM
vectorises it to NEON without fast-math. A full FFT would compute ~60 bins this does not need.

Each link's power is normalised by its own median bin (a noise-floor proxy); links are averaged.

## 3. Detection with a stated false-alarm rate

Under noise only, normalised bin power is roughly exponential, and averaging `L` links gives
`Gamma(L, 1/L)`. The Hann window correlates neighbouring bins (equivalent noise bandwidth 1.5 bins of
width 1/T), so the band holds `N_eff ≈ 0.5 Hz × 30 s / 1.5 = 10` independent bins, and the analytic
peak-to-mean threshold `x` solves

    N_eff · Q(L, L·x) = α        Q(L, y) = e^(-y) Σ_{k<L} y^k / k!

with `α = breath_false_alarm` (default 0.005 per window): `ln(N_eff/α)` ≈ 7.6 for one link, falling as
links are added.

**That formula was measured and found optimistic**: `selftest` showed ≈ 1.5 % false alarms against the
0.5 % target from four links up (the per-link median normalisation makes the null heavier-tailed than
the ideal gamma). So the default configuration instead uses **Monte-Carlo thresholds**
(`dsp::CALIBRATED_THRESHOLDS`, links 1..8 = 6.03, 4.76, 4.03, 3.65, 3.37, 3.15, 2.92, 2.81), the
`1 - α` quantile of the simulated null, worst case over 10/4/2 Hz source rates. They are regenerated with
`rvcsi-node selftest --calibrate --trials 10000` and apply only to the exact default window/band/grid/α;
any other configuration falls back to the analytic value; beyond 8 links the analytic curve is scaled by
the 8-link ratio (extrapolated, untested). Out-of-sample check (3000 fresh trials per cell, different
seeds): 0.33 %–0.67 % false alarms for 1/2/4/8 links at 10/4/2 Hz, enforced in the test suite.

A decision needs 2 of the last 3 hops over threshold, the peak strictly inside the band (an edge peak is
drift leakage), and the reported rate is the median of the last 5 parabolic-interpolated peaks.
See `docs/VALIDATION.md` for what the simulation does **not** establish (colored real-world noise, equal
amplitude on every link).

## 4. Presence

Per link: standard deviation of the linearly detrended raw samples over 20 s (60 s for links slower
than 0.5 Hz), divided by an adaptive noise baseline. The baseline moves down quickly (EWMA 0.2) and
leaks up at 0.05 % per hop, and is floored at the series' quantisation noise (0.29 dB for integer
RSSI). Presence turns on at score ≥ 2.0, off below 1.5, and is held 20 s. Breathing detection also
asserts presence.

`rvcsi-node calibrate` (empty room, ≥ 60 s) stores a per-link baseline ×1.15 in
`~/.rvcsi/baseline.json`; calibrated baselines are **frozen** upward, so a person sitting very still
cannot be slowly absorbed into "normal". Without calibration the baseline adapts online and assumes
the first minutes are representative.

## 5. Limits

* A motionless person produces almost no RSSI variance; presence then rests on breathing detection.
* Periodic interference in 6–36 cycles/min (HVAC, a pet) can look like breathing.
* Integer-dB RSSI hides sub-dB chest motion unless noise dithers it across a quantisation edge,
  which is why fusing links and long windows matter more than any filter.
* The latency of a breathing estimate is the window (30 s) plus 2 hops of agreement.
