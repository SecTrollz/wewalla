"""AR room mapping + Wi-Fi radio map (pure stdlib).

Geometry comes from ARCore through WebXR in Chrome (map.html): the phone's 6-DoF track, floor/wall
planes and points the user tags (furniture, doors, the router). Every *fresh* Wi-Fi reading is
stamped with where the phone was at that moment, which builds a per-access-point radio map on a
0.5 m floor grid. Later the same map locates the PHONE from Wi-Fi alone (k-nearest fingerprint).

What this does not do: RSSI cannot see furniture or where people are; those come from AR tags
and the room-level presence detector respectively.

Frame: WebXR "local-floor" (metres, y up, floor at y=0). Each AR session has its own origin, so one
map = one session unless you re-anchor (reset starts a new map).
"""
import bisect
import json
import math
import threading
import time

from .model import G_RSSI

CELL_M = 0.5
VOX_M = 0.1                  # depth voxels (ARCore depth API through WebXR)
MAX_VOXELS = 400_000
MAX_VOX_INDEX = 5000         # |index| * VOX_M <= 500 m
POSE_MATCH_S = 0.6           # a Wi-Fi sample must be this close in time to a pose to be placed
MAX_POSES = 200_000
MAX_OBJECTS = 500
MAX_PLANES = 300
MAX_COORD_M = 500.0
KINDS = ("wall", "corner", "door", "window", "bed", "sofa", "table", "desk", "chair", "wardrobe",
         "tv", "router", "phone_spot", "other")


def _num(v, lim=MAX_COORD_M):
    f = float(v)
    if not math.isfinite(f) or abs(f) > lim:
        raise ValueError("coordinate out of range")
    return f


class MapStore:
    def __init__(self, path=None):
        self.path = path
        self.lock = threading.Lock()
        self._reset()
        if path:
            self._load()

    def _reset(self):
        self.created = time.time()
        self.pose_t, self.poses = [], []          # parallel: times (sorted) and (x, y, z, yaw)
        self.objects, self.planes = {}, {}
        self.cells = {}                           # (ix, iz) -> {key: [n, sum, sumsq]}
        self.voxels = {}                          # (ix, iy, iz) at VOX_M -> observation count
        self.ap_label, self.ap_freq = {}, {}
        self.geo = None                           # {lat, lon, alt, accuracy, yaw_offset, t}: AR origin in world coords
        self._next_id = 1
        self.placed = self.unplaced = 0
        self.dirty = False

    # ---- persistence ---------------------------------------------------------------
    def _load(self):
        try:
            with open(self.path) as fh:
                d = json.load(fh)
        except (OSError, ValueError):
            return
        try:
            self.created = float(d.get("created", time.time()))
            for o in d.get("objects", []):
                self.objects[int(o["id"])] = o
            self._next_id = max(self.objects, default=0) + 1
            self.planes = {str(p["id"]): p for p in d.get("planes", [])}
            for c in d.get("cells", []):
                self.cells[(int(c["ix"]), int(c["iz"]))] = {k: list(v) for k, v in c["aps"].items()}
            for t, x, y, z, yaw in d.get("track", []):
                self.pose_t.append(float(t))
                self.poses.append((float(x), float(y), float(z), float(yaw)))
            self.ap_label = d.get("ap_label", {})
            self.geo = d.get("geo")
            for ix, iy, iz, n in d.get("voxels", []):
                self.voxels[(int(ix), int(iy), int(iz))] = int(n)
        except (KeyError, TypeError, ValueError):
            self._reset()

    def save(self):
        if not self.path:
            return
        with self.lock:
            if not self.dirty:
                return
            d = self._export(track_max=20_000)
            self.dirty = False
        import os
        tmp = self.path + ".tmp"
        fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)   # a map of your home: private
        with os.fdopen(fd, "w") as fh:
            json.dump(d, fh)
        os.replace(tmp, self.path)

    def _export(self, track_max):
        step = max(1, len(self.poses) // track_max)
        track = [[round(self.pose_t[i], 2)] + [round(v, 3) for v in self.poses[i]]
                 for i in range(0, len(self.poses), step)]
        return {
            "created": self.created, "cell_m": CELL_M,
            "objects": list(self.objects.values()), "planes": list(self.planes.values()),
            "cells": [{"ix": ix, "iz": iz, "aps": aps} for (ix, iz), aps in self.cells.items()],
            "track": track, "ap_label": self.ap_label,
            "voxels": [[ix, iy, iz, n] for (ix, iy, iz), n in self.voxels.items()],
            "geo": self.geo,
        }

    def reset(self):
        with self.lock:
            self._reset()
            self.dirty = True

    # ---- AR input ------------------------------------------------------------------
    def add_poses(self, rows):
        """rows: [[t_epoch, x, y, z, yaw_rad], ...] in time order (a batch from the browser)."""
        added = 0
        now = time.time()
        with self.lock:
            for r in rows[:5000]:
                try:
                    t = float(r[0])
                    if not math.isfinite(t) or abs(t - now) > 86400:
                        continue
                    p = (_num(r[1]), _num(r[2]), _num(r[3]), _num(r[4], 10.0))
                except (TypeError, ValueError, IndexError):
                    continue
                if self.pose_t and t <= self.pose_t[-1]:
                    continue
                self.pose_t.append(t)
                self.poses.append(p)
                added += 1
            if len(self.poses) > MAX_POSES:
                drop = len(self.poses) - MAX_POSES
                del self.pose_t[:drop], self.poses[:drop]
            self.dirty = self.dirty or bool(added)
        return added

    def add_object(self, o):
        kind = str(o.get("kind", ""))
        if kind not in KINDS:
            raise ValueError("kind must be one of: " + ", ".join(KINDS))
        obj = {"kind": kind, "label": str(o.get("label", ""))[:40],
               "x": _num(o["x"]), "y": _num(o.get("y", 0.0)), "z": _num(o["z"]),
               "yaw": _num(o.get("yaw", 0.0), 10.0), "t": time.time()}
        with self.lock:
            if len(self.objects) >= MAX_OBJECTS:
                raise ValueError("too many objects")
            obj["id"] = self._next_id
            self._next_id += 1
            self.objects[obj["id"]] = obj
            self.dirty = True
        return obj

    def delete_object(self, oid):
        with self.lock:
            gone = self.objects.pop(int(oid), None) is not None
            self.dirty = self.dirty or gone
        return gone

    def set_geo(self, g):
        """Anchor the AR local-floor origin to a GPS fix so the map can be placed on a world map
        (e.g. OpenStreetMap). GPS is ~15 m accurate outdoors and worse indoors: this georeferences
        the WHOLE map coarsely, it does not locate objects to GPS precision."""
        geo = {"lat": _num(g["lat"], 90.0), "lon": _num(g["lon"], 180.0),
               "alt": _num(g.get("alt", 0.0), 1e5), "accuracy": _num(g.get("accuracy", 0.0), 1e5),
               "yaw_offset": _num(g.get("yaw_offset", 0.0), 10.0), "t": time.time()}
        with self.lock:
            self.geo = geo
            self.dirty = True
        return geo

    def add_voxels(self, rows):
        """rows: [[ix, iy, iz], ...] depth hits already quantised to VOX_M by the browser."""
        n = 0
        with self.lock:
            for r in rows[:20000]:
                try:
                    v = (int(r[0]), int(r[1]), int(r[2]))
                except (TypeError, ValueError, IndexError):
                    continue
                if max(abs(c) for c in v) > MAX_VOX_INDEX:
                    continue
                if v not in self.voxels and len(self.voxels) >= MAX_VOXELS:
                    continue
                self.voxels[v] = self.voxels.get(v, 0) + 1
                n += 1
            self.dirty = self.dirty or bool(n)
        return n

    def heightmap(self, min_hits=2):
        """Top-down occupancy from depth voxels: per (ix, iz) column the floor-relative max height and
        how much of the column is filled. Classifies columns as wall / furniture / low / floor.
        Floor height = 5th percentile of all voxel heights (robust to a few below-floor outliers)."""
        with self.lock:
            vox = [(v, n) for v, n in self.voxels.items() if n >= min_hits]
        if not vox:
            return {"vox_m": VOX_M, "floor_iy": None, "cols": []}
        ys = sorted(v[1] for v, _ in vox)
        floor = ys[int(0.05 * (len(ys) - 1))]
        cols = {}
        for (ix, iy, iz), n in vox:
            h = iy - floor
            if h < 1:                              # the floor itself
                continue
            c = cols.setdefault((ix, iz), [0, 0, 0])
            c[0] = max(c[0], h)
            c[1] += 1
            c[2] += n
        out = []
        for (ix, iz), (hmax, filled, hits) in cols.items():
            top = hmax * VOX_M
            kind = "wall" if top >= 1.8 and filled >= 8 else ("furniture" if top >= 0.25 else "low")
            out.append([ix, iz, round(top, 2), filled, kind])
        return {"vox_m": VOX_M, "floor_iy": floor, "cols": out}

    def set_planes(self, planes):
        n = 0
        with self.lock:
            for p in planes[:MAX_PLANES]:
                try:
                    orient = p.get("orientation")
                    if orient not in ("horizontal", "vertical"):
                        continue
                    poly = [[round(_num(a), 3), round(_num(b), 3)] for a, b in p["polygon"][:64]]
                    if len(poly) < 3:
                        continue
                    self.planes[str(p["id"])[:40]] = {"id": str(p["id"])[:40], "orientation": orient,
                                                     "y": round(_num(p.get("y", 0.0)), 3), "polygon": poly}
                    n += 1
                except (KeyError, TypeError, ValueError):
                    continue
            if len(self.planes) > MAX_PLANES:
                for k in list(self.planes)[:len(self.planes) - MAX_PLANES]:
                    del self.planes[k]
            self.dirty = self.dirty or bool(n)
        return n

    # ---- Wi-Fi input ---------------------------------------------------------------
    def pose_at(self, t):
        i = bisect.bisect_left(self.pose_t, t)
        best = None
        for j in (i - 1, i):
            if 0 <= j < len(self.pose_t):
                dt = abs(self.pose_t[j] - t)
                if dt <= POSE_MATCH_S and (best is None or dt < best[0]):
                    best = (dt, self.poses[j])
        return None if best is None else best[1]

    def on_sample(self, s):
        """Place a fresh RSSI sample at the phone's AR position (if an AR track covers that moment)."""
        if s.group != G_RSSI or not s.fresh:
            return False
        with self.lock:
            p = self.pose_at(s.t)
            if p is None:
                self.unplaced += 1
                return False
            cell = self.cells.setdefault((math.floor(p[0] / CELL_M), math.floor(p[2] / CELL_M)), {})
            acc = cell.setdefault(s.key, [0, 0.0, 0.0])
            acc[0] += 1
            acc[1] += s.value
            acc[2] += s.value * s.value
            if s.freq:
                self.ap_freq[s.key] = s.freq
            if s.label:
                self.ap_label[s.key] = s.label
            self.placed += 1
            self.dirty = True
        return True

    # ---- queries ---------------------------------------------------------------------
    def radio(self, key):
        with self.lock:
            out = []
            for (ix, iz), aps in self.cells.items():
                a = aps.get(key)
                if a and a[0]:
                    m = a[1] / a[0]
                    out.append({"ix": ix, "iz": iz, "rssi": round(m, 1), "n": a[0]})
            return out

    def ap_summary(self):
        """Per access point: samples, cells covered, strongest cell and a coarse location ESTIMATE
        (RSSI-weighted centroid of its 5 strongest cells; indoor multipath makes this +-several m)."""
        per = {}
        with self.lock:
            for (ix, iz), aps in self.cells.items():
                for k, (n, sm, _) in aps.items():
                    per.setdefault(k, []).append((sm / n, n, ix, iz))
        out = []
        for k, rows in per.items():
            rows.sort(reverse=True)
            top = rows[:5]
            w = [10 ** (r[0] / 20.0) for r in top]
            sw = sum(w) or 1.0
            ex = sum(wi * (r[2] + 0.5) * CELL_M for wi, r in zip(w, top)) / sw
            ez = sum(wi * (r[3] + 0.5) * CELL_M for wi, r in zip(w, top)) / sw
            out.append({"key": k, "label": self.ap_label.get(k, ""), "freq": self.ap_freq.get(k, 0),
                        "samples": sum(r[1] for r in rows), "cells": len(rows),
                        "max_rssi": round(top[0][0], 1), "est_x": round(ex, 2), "est_z": round(ez, 2),
                        "estimate": "rssi-weighted centroid, +-several m"})
        out.sort(key=lambda a: -a["samples"])
        return out

    def locate(self, current, min_common=3, k=3):
        """Where is the PHONE now? `current` = {key: rssi}. k-NN over the radio map's cell means.
        Returns None when the map cannot support an answer."""
        with self.lock:
            scored = []
            for (ix, iz), aps in self.cells.items():
                common = [key for key in current if key in aps and aps[key][0]]
                if len(common) < min_common:
                    continue
                d = math.sqrt(sum((current[key] - aps[key][1] / aps[key][0]) ** 2 for key in common) / len(common))
                scored.append((d, ix, iz, len(common)))
        if not scored:
            return None
        scored.sort()
        best = scored[:k]
        w = [1.0 / (d + 1.0) for d, *_ in best]
        sw = sum(w)
        x = sum(wi * (b[1] + 0.5) * CELL_M for wi, b in zip(w, best)) / sw
        z = sum(wi * (b[2] + 0.5) * CELL_M for wi, b in zip(w, best)) / sw
        spread = max(math.hypot((b[1] - best[0][1]) * CELL_M, (b[2] - best[0][2]) * CELL_M) for b in best)
        return {"x": round(x, 2), "z": round(z, 2), "rms_db": round(best[0][0], 1), "common_aps": best[0][3],
                "spread_m": round(spread, 2), "cells_considered": len(scored),
                "method": "wifi fingerprint k-NN (phone position only)"}

    def summary(self, track_max=3000):
        with self.lock:
            d = self._export(track_max)
            d.pop("cells")
            d.pop("voxels")
            d["counts"] = {"poses": len(self.poses), "cells": len(self.cells), "objects": len(self.objects),
                           "voxels": len(self.voxels),
                           "planes": len(self.planes), "wifi_placed": self.placed, "wifi_unplaced": self.unplaced}
            d["last_pose_age_s"] = round(time.time() - self.pose_t[-1], 1) if self.pose_t else None
        d["aps"] = self.ap_summary()
        d["heightmap"] = self.heightmap()
        return d
