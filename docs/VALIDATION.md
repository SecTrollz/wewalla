# Validation: proving it on your phone

Nothing in this repo claims a capability the device has not shown. Three levels of evidence, in order:

| Level | What it shows | Where |
|---|---|---|
| Simulation | the detector's statistics are what the docs say, on white noise | `rvcsi-node selftest`, `cargo test --release` |
| Recording | your device's real data, replayable and shareable | `rvcsi-node record` / `replay` |
| Field test | a PASS/FAIL verdict from *your* room and *your* radio | `rvcsi-node validate` |

Only the field test says anything about your phone. Run it before trusting any output.

## 1. Empty-room control (always first)

```
rvcsi-node validate --mode empty --secs 600
```
Everyone leaves the room. PASS needs presence false alarms <= 5% and breathing false alarms <= 2% of
steady-state windows (the first 40 s are discarded). A FAIL here means real RSSI noise is burstier than the
white noise the thresholds were calibrated on (fading, interference, AP power steps, a pet, a fan): fix that
(placement, calibration with `rvcsi-node calibrate`, more links) before anything else.

## 2. Paced breathing (ground truth)

```
rvcsi-node validate --mode paced --bpm 12 --secs 180 --feet 5
```
Sit still and breathe to a metronome at 12 breaths/min. PASS needs the breathing peak in >= 60% of windows
and the median rate within 1.5 bpm. This is the only test that proves the *rate* is right.

## 3. Range: 40 ft, every direction

```
rvcsi-node validate --mode range --target-ft 40 --bearings 8 --distances 5,10,20,30,40
```
Protocol (the tool prompts for each step; Enter starts the measurement):

1. **Control**: everyone leaves; 100 s of empty room.
2. For each of 8 bearings (N, NE, E, SE, S, SW, W, NW) and each distance (5, 10, 20, 30, 40 ft) from the
   phone: stand there, move naturally in place (shift weight, gesture), 100 s per cell. About 70 minutes
   total; use `--bearings 4` or `--distances 10,40` for a quick pass.

Result: a table per cell, and per bearing the **reach**: the farthest distance where that cell *and every
nearer cell* passed (a hole at 20 ft ends the reach at 10 ft even if 40 ft passes). `presence_target_met`
is true only if the control passed **and** every bearing reaches the target. Breathing coverage is reported
separately (`breathing_target_met`) and never folded into the presence verdict. Exit code is 2 on FAIL.

Rules the measurement follows, so a PASS means something:
* cells use the detector's raw per-window statistics, not its debounced flags (those linger after a person
  leaves and would leak one cell into the next);
* a failed control marks the whole run invalid;
* too little data is a failure, never a pass;
* a run where no source delivers any sample is an error, not an empty room.

Where you place the phone, and where the other end of each link is (your AP, hotspot clients, Bluetooth
devices), decides whether "all directions" is even physically possible: a link only senses people near
the line between its two ends. The coverage table shows weak bearings so you can add a link there.

## Recording and offline analysis

```
rvcsi-node record --out walk.jsonl --secs 600     # raw samples (file mode 0600; refuses to overwrite)
rvcsi-node replay walk.jsonl                      # same detector, deterministic, any machine
```
A recording lets someone else reproduce a result without your phone.

## What the simulation does and does not show

`rvcsi-node selftest` (800 trials per cell, white noise sigma = 1 dB, integer-rounded RSSI; **every link
sees the same breathing amplitude**, which real geometry never gives):

Per-window detection probability (false alarm with no breathing in brackets; target 0.5%):

| source rate | links | 0.4 dB breathing | 0.8 dB breathing |
|---|---|---|---|
| 10 Hz | 1 | 17% (0.6%) | 96% |
| 10 Hz | 4 | 92% (0.4%) | 100% |
| 4 Hz | 1 | 3% (0.5%) | 45% |
| 4 Hz | 4 | 23% (0.4%) | 100% |
| 2 Hz | 1 | 1% (0.3%) | 12% |
| 2 Hz | 4 | 7% (0.5%) | 83% |

Out-of-sample false-alarm rate (3000 fresh trials per cell, never used for calibration): 0.33%-0.67% for
1, 2, 4 and 8 links at 10, 4 and 2 Hz. Enforced by `selftest::tests::calibrated_false_alarm_holds_out_of_sample`.

Caveats that matter:
1. Real RSSI noise is colored and bursty; the white-noise false-alarm rate **does not transfer**. Only the
   empty-room control on your device measures the real rate.
2. Fusion gain is an upper bound: it assumes every link sees the chest equally.
3. Rhythmic motion at 6-36 cycles/min (a fan, a pet, repetitive arm movement) is indistinguishable from
   breathing to this detector.
4. Breathing amplitude of 0.4-0.8 dB at the receiver is an assumption, not a measurement; the paced test
   measures it.
