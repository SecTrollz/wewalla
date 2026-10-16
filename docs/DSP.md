# DSP notes

All stages run per report hop (default 2 s) on the last 30 s of each link, in `f32`, with no
allocation after start-up.

## 1. Uniform resampling

Android delivers irregular samples. For the breathing path each link is box-averaged into
`window_secs × fs_hz` = 30 × 4 = 120 bins ending at "now". Box averaging, not point sampling, is
the anti-alias filter when the source is faster than 4 Hz. Empty bins are bridged linearly; a
window with under 70 % of bins populated is rejected, never interpolated into a result.

A link joins the breathing spectrum only if its measured raw rate is ≥ 2 Hz (the band tops out at
0.6 Hz; 2 Hz leaves headroom over Nyquist). The rate is `(n-1)/span` of the samples actually
received, so a slow Wi-Fi-scan link can never be mistaken for a fast one.

## 2. Spectrum

Linear detrend (least squares over the window), then a **Hann-windowed direct DFT over the
breathing band only**: 0.10–0.60 Hz at 0.01 Hz = 51 bins (6–36 breaths/min). The window and
twiddles are folded into two tables (`51 × 120` floats each, ≈ 49 KB total) built once, so a
step is 102 dot products of length 120. The dot product keeps 8 independent accumulators so LLVM
vectorises it to NEON without fast-math. A full FFT would compute ~60 bins this does not need.

Each link's power is normalised by its own median bin (a noise-floor proxy); links are averaged.

## 3. Detection with a stated false-alarm rate

Under noise only, normalised bin power is ≈ exponential. Averaging `L` links gives
`Gamma(L, 1/L)`. The Hann window correlates neighbouring bins; its equivalent noise bandwidth is
1.5 bins of width 1/T, so the band holds `N_eff ≈ 0.5 Hz × 30 s / 1.5 = 10` independent bins. The
peak-to-mean threshold `x` solves

    N_eff · Q(L, L·x) = α        Q(L, y) = e^(-y) Σ_{k<L} y^k / k!

with `α = breath_false_alarm` (default 0.005 per window). For `L = 1` this is `ln(N_eff/α)` ≈ 7.6;
it falls as links are added (≈ 4.0 for three links), which is how fusion buys sensitivity.
A decision needs 2 of the last 3 hops over threshold, the peak strictly inside the band (an edge
peak is drift leakage), and the reported rate is the median of the last 5 parabolic-interpolated
peaks. The test suite measures the empirical false-alarm rate on white noise.

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
