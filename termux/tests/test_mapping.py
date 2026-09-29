"""Tests for the AR room map + Wi-Fi radio map (wewalla/mapping.py)."""
import os
import sys
import tempfile
import time
import unittest

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))
from wewalla.mapping import MapStore, CELL_M  # noqa: E402
from wewalla.model import G_CSI, G_RSSI, Sample  # noqa: E402


class TestMapStore(unittest.TestCase):
    def setUp(self):
        self.m = MapStore()

    def walk(self, t0=None):
        t0 = time.time() if t0 is None else t0
        self.t0 = t0
        # a straight AR walk along +x at y=0, one pose per 0.1 s
        for i in range(60):
            self.m.add_poses([[t0 + i * 0.1, i * 0.1, 0.0, 0.0, 0.0]])

    def test_pose_stamps_wifi_into_the_right_cell(self):
        self.walk()
        # sample at t where the phone is near x=3.0 -> cell floor(3.0/0.5)=6
        placed = self.m.on_sample(Sample(self.t0 + 30 * 0.1, "scan:aa", -55.0, G_RSSI, True, 5200, "Home"))
        self.assertTrue(placed)
        cells = {(c["ix"], c["iz"]): c for c in self.m.radio("scan:aa")}
        self.assertIn((6, 0), cells)
        self.assertEqual(cells[(6, 0)]["rssi"], -55.0)
        self.assertEqual(self.m.ap_label["scan:aa"], "Home")

    def test_sample_without_a_pose_is_not_placed(self):
        self.assertFalse(self.m.on_sample(Sample(time.time() - 100000.0, "scan:aa", -55.0, G_RSSI, True)))
        self.assertEqual(self.m.placed, 0)
        self.assertEqual(self.m.unplaced, 1)

    def test_stale_and_csi_samples_are_ignored(self):
        self.walk()
        self.assertFalse(self.m.on_sample(Sample(self.t0 + 3.0, "scan:aa", -55.0, G_RSSI, False)))  # not fresh
        self.assertFalse(self.m.on_sample(Sample(self.t0 + 3.0, "csi:1", 10.0, G_CSI, True)))        # not RSSI
        self.assertEqual(self.m.placed, 0)

    def test_locate_recovers_a_known_spot(self):
        self.walk()
        for i in range(60):
            t = self.t0 + i * 0.1
            self.m.on_sample(Sample(t, "scan:a", -40.0 - i, G_RSSI, True))     # fades along the walk
            self.m.on_sample(Sample(t, "scan:b", -90.0 + i, G_RSSI, True))     # grows along the walk
            self.m.on_sample(Sample(t, "scan:c", -60.0, G_RSSI, True))
        loc = self.m.locate({"scan:a": -40.0 - 10, "scan:b": -90.0 + 10, "scan:c": -60.0})
        self.assertIsNotNone(loc)
        self.assertLess(abs(loc["x"] - 1.0), 1.0)          # near where i≈10 was (x≈1.0 m)
        self.assertGreaterEqual(loc["common_aps"], 3)

    def test_locate_refuses_without_enough_common_aps(self):
        self.walk()
        self.m.on_sample(Sample(self.t0 + 3.0, "scan:a", -50.0, G_RSSI, True))
        self.assertIsNone(self.m.locate({"scan:a": -50.0}))       # only 1 common AP < min 3

    def test_objects_validate_and_delete(self):
        o = self.m.add_object({"kind": "bed", "x": 1.0, "z": 2.0, "label": "master"})
        self.assertEqual(o["id"], 1)
        with self.assertRaises(ValueError):
            self.m.add_object({"kind": "spaceship", "x": 0, "z": 0})
        with self.assertRaises(ValueError):
            self.m.add_object({"kind": "bed", "x": 1e9, "z": 0})       # coord out of range
        self.assertTrue(self.m.delete_object(1))
        self.assertFalse(self.m.delete_object(1))

    def test_voxels_build_a_heightmap(self):
        # a 2 m wall column at (ix=10) and a 0.5 m furniture column at (ix=20), floor at iy=0
        for iy in range(0, 20):
            self.m.add_voxels([[10, iy, 5]] * 3)
        for iy in range(0, 5):
            self.m.add_voxels([[20, iy, 5]] * 3)
        hm = self.m.heightmap()
        kinds = {(c[0], c[1]): c[4] for c in hm["cols"]}
        self.assertEqual(kinds.get((10, 5)), "wall")
        self.assertEqual(kinds.get((20, 5)), "furniture")

    def test_planes_require_valid_polygon_and_orientation(self):
        n = self.m.set_planes([
            {"id": "p1", "orientation": "vertical", "y": 0, "polygon": [[0, 0], [1, 0], [1, 2]]},
            {"id": "p2", "orientation": "sideways", "polygon": [[0, 0], [1, 1], [2, 2]]},   # bad orient
            {"id": "p3", "orientation": "horizontal", "polygon": [[0, 0]]},                  # too few pts
        ])
        self.assertEqual(n, 1)

    def test_geo_anchor_validates(self):
        g = self.m.set_geo({"lat": 36.08, "lon": -79.54, "alt": 165.0, "accuracy": 18.0})
        self.assertAlmostEqual(g["lat"], 36.08)
        with self.assertRaises(ValueError):
            self.m.set_geo({"lat": 200.0, "lon": 0.0})        # latitude out of range

    def test_round_trip_persistence(self):
        with tempfile.TemporaryDirectory() as d:
            path = os.path.join(d, "map.json")
            m = MapStore(path)
            t0 = time.time()
            m.add_poses([[t0, 0.0, 0.0, 0.0, 0.0], [t0 + 0.1, 0.5, 0.0, 0.0, 0.0]])
            m.on_sample(Sample(t0 + 0.1, "scan:a", -50.0, G_RSSI, True, 2412, "Net"))
            m.add_object({"kind": "router", "x": 0.5, "z": 0.0})
            m.add_voxels([[1, 2, 3]])
            m.set_geo({"lat": 1.0, "lon": 2.0})
            m.save()
            m2 = MapStore(path)
            self.assertEqual(len(m2.objects), 1)
            self.assertEqual(m2.geo["lat"], 1.0)
            self.assertEqual(m2.voxels.get((1, 2, 3)), 1)
            self.assertTrue(m2.radio("scan:a"))

    def test_map_file_is_private(self):
        with tempfile.TemporaryDirectory() as d:
            path = os.path.join(d, "map.json")
            m = MapStore(path)
            m.add_poses([[time.time(), 0.0, 0.0, 0.0, 0.0]])
            m.dirty = True
            m.save()
            self.assertEqual(oct(os.stat(path).st_mode & 0o777), "0o600")


if __name__ == "__main__":
    unittest.main()
