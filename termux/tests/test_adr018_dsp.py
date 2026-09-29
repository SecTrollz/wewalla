import math, random, sys, unittest, os
sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))
from wewalla import adr018
import statistics
from wewalla.dsp import Analyzer, resample, weighted_median
from wewalla.model import Sample


class TestAdr018(unittest.TestCase):
    def test_roundtrip_and_layout(self):
        iq = [1, -2, 3, -4, 127, -128]
        raw = adr018.encode(7, 1, 3, 2437, 42, -55, -95, iq, flags=adr018.FLAG_DERIVED)
        self.assertEqual(raw[:4], bytes([0x01, 0x00, 0x11, 0xC5]))  # 0xC5110001 little-endian
        self.assertEqual(len(raw), 20 + 6)
        f = adr018.decode(raw)
        self.assertEqual((f.node_id, f.n_ant, f.n_sc, f.freq_mhz, f.seq, f.rssi, f.noise), (7, 1, 3, 2437, 42, -55, -95))
        self.assertEqual(f.iq, iq)
        self.assertTrue(f.derived)

    def test_rejects_garbage_and_skips_siblings(self):
        with self.assertRaises(ValueError):
            adr018.decode(b"\x00" * 30)
        with self.assertRaises(ValueError):
            adr018.decode(adr018.encode(1, 1, 4, 2412, 0, -50, -90, [0] * 8)[:-1])  # truncated
        self.assertIsNone(adr018.decode(bytes([0x02, 0x00, 0x11, 0xC5]) + b"\x00" * 28))  # vitals sibling
        with self.assertRaises(ValueError):
            adr018.encode(1, 5, 4, 2412, 0, -50, -90, [0] * 40)  # 5 antennas

    def test_clamps_iq(self):
        f = adr018.decode(adr018.encode(1, 1, 1, 2412, 0, -200, -90, [999, -999]))
        self.assertEqual((f.iq, f.rssi), ([127, -128], -128))


def feed(a, scenario, seed=0, fs=4.0, dur=120, n_ap=6, tone=0.25, amp=0.8, t0=1e6):
    rnd = random.Random(seed)
    drift, walk, t, out = [0.0] * n_ap, [0.0] * n_ap, t0, []
    nxt = t0 + 1
    while t < t0 + dur:
        for i in range(n_ap):
            drift[i] += rnd.gauss(0, 0.02)
            v = -45 - 5 * i + drift[i] + rnd.gauss(0, 0.4)
            if scenario == "walk" and t > t0 + 60:
                walk[i] = 0.9 * walk[i] + rnd.gauss(0, 1.6)
                v += walk[i]
            if scenario == "breathe" and t > t0 + 50:
                v += amp * math.sin(2 * math.pi * tone * t + i)
            a.push(Sample(t, "sim:%d" % i, round(v)))
        t += 1 / fs
        if t >= nxt:
            out.append(a.analyze(t))
            nxt += 1
    return out


class TestDsp(unittest.TestCase):
    def test_resample_holds_and_averages(self):
        g = resample([(0.1, 1.0), (0.4, 3.0), (2.5, 9.0)], 0.0, 4.0, 1.0)
        self.assertEqual(g, [2.0, 2.0, 9.0, 9.0])

    def test_empty_room_no_presence(self):
        for seed in range(3):
            out = feed(Analyzer(), "empty", seed)
            self.assertFalse(any(r["presence"] for r in out), seed)
            self.assertIsNone(out[-1]["breathing"]["bpm"])

    def test_warmup_reports_none_not_false(self):
        out = feed(Analyzer(), "empty", 0, dur=5)
        self.assertIn(out[-1]["state"], ("warming_up", "no_data"))
        self.assertIsNone(out[-1]["presence"])

    def test_walking_detected(self):
        out = feed(Analyzer(), "walk", 1)
        self.assertTrue(out[-1]["presence"])
        self.assertGreater(out[-1]["motion"], 0.8)
        self.assertEqual(out[-1]["breathing"]["reason"], "subject_moving")

    def test_breathing_rate_recovered(self):
        for tone in (0.15, 0.25, 0.4):
            out = feed(Analyzer(), "breathe", 3, tone=tone)
            b = out[-1]["breathing"]
            self.assertIsNotNone(b["bpm"], tone)
            self.assertAlmostEqual(b["bpm"], tone * 60, delta=2.0)
            self.assertTrue(out[-1]["presence"])  # confirmed breathing implies presence

    def test_noise_never_yields_breathing(self):
        hits = sum(any(r["breathing"]["bpm"] for r in feed(Analyzer(), "empty", s, dur=90)[-10:]) for s in range(30))
        self.assertLessEqual(hits, 2)  # <= ~7% worst-case over 30 seeds (measured ~1% typical)

    def test_slow_source_refuses_vitals(self):
        a, t = Analyzer(), 1e6
        rnd = random.Random(1)
        for k in range(80):  # one sample per 3 s: ~0.33 Hz, the Android RSSI reality
            t = 1e6 + 3 * k
            for i in range(4):
                a.push(Sample(t, "s%d" % i, -50 + round(rnd.gauss(0, 1)) + 3 * math.sin(2 * math.pi * 0.25 * t)))
        r = a.analyze(t)
        self.assertIsNone(r["breathing"]["bpm"])
        self.assertEqual(r["breathing"]["reason"], "insufficient_update_rate")
        self.assertEqual(r["heart_rate"]["reason"], "unsupported_by_source")

    def test_phone_motion_freezes_analysis(self):
        a = Analyzer()
        out = feed(a, "empty", 0, dur=60)
        t = out[-1]["t"]
        a.set_phone_motion(2.0, t)
        r = a.analyze(t + 0.5)
        self.assertEqual(r["state"], "phone_moving")
        self.assertEqual(r["breathing"]["reason"], "phone_moving")

    def test_calibration_roundtrip(self):
        a = Analyzer()
        feed(a, "empty", 0, dur=90)
        cal = a.export_calibration()
        self.assertEqual(len(cal["series"]), 6)
        b = Analyzer(calibration=cal)
        out = feed(b, "empty", 5, dur=30, t0=2e6)
        self.assertEqual(out[-1]["baseline"], "calibrated")
        self.assertFalse(out[-1]["presence"])

    def test_signal_kind_labels(self):
        a = Analyzer()
        feed(a, "empty", 0, dur=20)
        self.assertIn("NOT CSI", a.analyze(1e6 + 20)["quality"]["signal_kind"])
        a.push(Sample(1e6 + 20, "csi:1:0:0", 30.0, "csi"))
        self.assertEqual(a.analyze(1e6 + 20)["quality"]["signal_kind"], "real-csi")



class TestWeighting(unittest.TestCase):
    def test_weighted_median_matches_plain_median_when_equal(self):
        rnd = random.Random(1)
        for n in range(1, 9):
            xs = [rnd.random() for _ in range(n)]
            self.assertAlmostEqual(weighted_median([(x, 1.0) for x in xs]), statistics.median(xs))
        self.assertIsNone(weighted_median([]))
        self.assertEqual(weighted_median([(0.0, 1.0), (5.0, 3.0)]), 5.0)

    def test_confidence_is_none_while_warming_and_rises_with_calibration(self):
        a = Analyzer()
        out = feed(a, "empty", 0, dur=40)
        self.assertEqual(out[0]["confidence"]["level"], "none")
        adaptive = out[-1]["confidence"]
        self.assertTrue(adaptive["heuristic"])
        self.assertEqual(adaptive["factors"]["baseline"], 0.7)
        a.calibration = a.export_calibration()["series"]
        cal = a.analyze(1e6 + 40)["confidence"]
        self.assertGreater(cal["score"], adaptive["score"])
        self.assertLessEqual(cal["score"], 1.0)
        self.assertNotEqual(cal["level"], "high")   # derived RSSI alone is never high confidence

    def test_fast_csi_outvotes_many_stale_rssi(self):
        a = Analyzer(warmup_points=5)
        rnd, t0 = random.Random(3), 1e6
        t = t0
        while t < t0 + 60:
            moving = t > t0 + 40
            for i in range(6):                      # slow RSSI: one fresh sample per 5 s, quiet
                a.push(Sample(t, "rssi:%d" % i, -50 + rnd.gauss(0, 0.3), fresh=int(t * 4) % 20 == 0))
            a.push(Sample(t, "csi:1:0:0", 30 + rnd.gauss(0, 0.5) + (rnd.gauss(0, 8) if moving else 0), "csi"))
            if int(t * 4) % 4 == 0:
                last = a.analyze(t)
            t += 0.25
        self.assertGreater(last["motion"], 0.5)


class TestSlowSources(unittest.TestCase):
    def test_slow_series_still_analysed(self):
        # Replays the MEASURED Termux:API profile of a Motorola Edge 2025 (tests/hw_measure.py):
        # calls take 1.3-3.8 s, ~4% time out after 6 s, scan results land every ~10 s.
        a = Analyzer(warmup_points=5)
        rnd, t, t_scan, out = random.Random(5), 1e6, 1e6, []
        while t < 1e6 + 600:
            t += rnd.uniform(1.3, 3.8) + (6.0 if rnd.random() < 0.04 else 0.0)
            a.push(Sample(t, "conn:x", -60 + rnd.gauss(0, 1)))
            if t >= t_scan:
                t_scan = t + 8.0 + rnd.uniform(1.5, 2.0)
                for i in range(6):
                    a.push(Sample(t, "scan:%d" % i, -70 + rnd.gauss(0, 1)))
            out.append(a.analyze(t))
        tail = out[len(out) // 2:]
        self.assertEqual(sum(r["state"] == "no_data" for r in tail), 0)
        # the scan series must actually vote, not just exist
        self.assertTrue(all(r["confidence"]["factors"]["coverage"] == 1.0 for r in tail if r["state"] == "ok"))

    def test_fast_series_keep_10s_window(self):
        from wewalla.dsp import motion_window, MOTION_WIN_S
        self.assertEqual(motion_window([(i * 0.25, 0) for i in range(100)]), MOTION_WIN_S)
        self.assertEqual(motion_window([(i * 60.0, 0) for i in range(10)]), 40.0)


if __name__ == "__main__":
    unittest.main()
