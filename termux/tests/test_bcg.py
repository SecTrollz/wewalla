"""Bed-BCG tests. All signals are SYNTHETIC (tests/bcg_synth.py) with the noise floor and resolution
MEASURED on a Motorola Edge 2025; they check the gates and the maths, not real-world accuracy."""
import os
import sys
import unittest

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))
sys.path.insert(0, os.path.dirname(__file__))
from bcg_synth import bed_signal, run  # noqa: E402
from wewalla.bcg import BcgAnalyzer  # noqa: E402


def accepted(res, key="heart"):
    return [r[key]["bpm"] for r in res if r[key]["bpm"] is not None]


class TestBcgHeart(unittest.TestCase):
    def test_recovers_heart_rate_across_range(self):
        for bpm in (48, 62, 90, 130):
            res = run(BcgAnalyzer(), bed_signal(bpm=bpm, dur=60, amp=0.06, seed=bpm), jitter=0.03)[-25:]
            ok = accepted(res)
            self.assertGreaterEqual(len(ok), 18, "bpm %s: only %d/25 accepted" % (bpm, len(ok)))
            self.assertLessEqual(max(abs(v - bpm) for v in ok), 3.0, "bpm %s: %s" % (bpm, ok))

    def test_noise_and_twitches_never_yield_a_heart_rate(self):
        for seed in range(6):
            res = run(BcgAnalyzer(), bed_signal(heart=False, dur=50, seed=500 + seed,
                                                twitch_at=25 if seed % 2 else None))
            self.assertEqual(accepted(res), [], "seed %d" % seed)

    def test_weak_signal_mostly_refuses_instead_of_guessing(self):
        res = run(BcgAnalyzer(), bed_signal(bpm=75, dur=60, amp=0.025, seed=9))[-25:]
        self.assertLessEqual(len(accepted(res)), 5)
        self.assertIn(res[-1]["heart"]["reason"], ("no_clear_rhythm", "confirming", None))

    def test_movement_blocks_until_window_is_clean(self):
        res = run(BcgAnalyzer(), bed_signal(bpm=70, dur=70, amp=0.06, seed=4, twitch_at=30))
        after = res[31:31 + 14]                      # twitch still inside the 16 s window
        self.assertEqual(accepted(after), [])
        self.assertTrue(any(r["heart"]["reason"] in ("settling_after_movement", "subject_moving") for r in after))
        self.assertTrue(accepted(res[-10:]), "should recover once the window is clean")

    def test_slow_stream_is_refused(self):
        res = run(BcgAnalyzer(), bed_signal(bpm=70, dur=40, amp=0.06, fs=20.0, seed=2))
        self.assertEqual(res[-1]["heart"]["reason"], "insufficient_update_rate")

    def test_stale_stream_reports_no_data(self):
        an = BcgAnalyzer()
        run(an, bed_signal(dur=10))
        self.assertEqual(an.analyze(1e6 + 30)["state"], "no_data")

    def test_label_says_contact_not_wifi(self):
        res = run(BcgAnalyzer(), bed_signal(dur=5))
        self.assertIn("NOT Wi-Fi", res[-1]["heart"]["method"])
        self.assertTrue(res[-1]["heart"]["experimental"])


class TestBcgBreathing(unittest.TestCase):
    def test_recovers_breathing_rate(self):
        res = run(BcgAnalyzer(), bed_signal(bpm=65, dur=75, amp=0.05, resp_bpm=15, resp_amp=0.03, seed=3))[-10:]
        ok = accepted(res, "breathing")
        self.assertTrue(ok)
        self.assertLessEqual(max(abs(v - 15) for v in ok), 1.0)

    def test_no_breathing_from_noise(self):
        res = run(BcgAnalyzer(), bed_signal(heart=False, resp_amp=0.0, dur=60, seed=8))
        self.assertEqual(accepted(res, "breathing"), [])


if __name__ == "__main__":
    unittest.main()
