#!/usr/bin/env python3
"""Real-device measurement run (reproducer for MEASURED claims in termux/README.md).

Starts `wewalla run --source <src>` on this phone, polls /api/v1/state, and prints a JSON
report of what the hardware actually delivered: update rates, series counts, how the
states/vitals gates behaved, confidence, dropped samples and whether the process survived.

It measures pipeline behaviour and data rates on real hardware. It does NOT measure detection
accuracy: that needs ground-truth labels (who was in the room, when), which this script has no
way to know. Usage:  python3 tests/hw_measure.py [--source termux] [--seconds 180] [--home ~/.wewalla]
"""
import argparse
import collections
import json
import os
import socket
import statistics
import subprocess
import sys
import tempfile
import time
import urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))


def free_port():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    p = s.getsockname()[1]
    s.close()
    return p


def bed_summary(beds):
    if not beds:
        return None
    hb = [b["heart"]["bpm"] for b in beds if b["heart"]["bpm"] is not None]
    br = [b["breathing"]["bpm"] for b in beds if b["breathing"]["bpm"] is not None]
    fs = [b["fs"] for b in beds if b.get("fs")]
    return {
        "polls": len(beds),
        "fs_median": statistics.median(fs) if fs else None,
        "states": collections.Counter(b.get("state") for b in beds).most_common(),
        "heart_accepted": len(hb),
        "heart_values": [min(hb), statistics.median(hb), max(hb)] if hb else None,
        "heart_reasons": collections.Counter(str(b["heart"]["reason"]) for b in beds).most_common(),
        "breathing_accepted": len(br),
        "breathing_values": [min(br), statistics.median(br), max(br)] if br else None,
        "breathing_reasons": collections.Counter(str(b["breathing"]["reason"]) for b in beds).most_common(),
        "timeline": [(round(b.get("fs") or 0), b["heart"]["bpm"], b["breathing"]["bpm"]) for b in beds][-40:],
    }


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--source", default="termux")
    ap.add_argument("--seconds", type=float, default=180)
    ap.add_argument("--poll", type=float, default=2.0)
    ap.add_argument("--bed-raw", help="with --source bed: save raw accelerometer CSV here (private)")
    ap.add_argument("--home", help="WEWALLA_HOME to use (default: a throwaway dir). Use your real one for "
                                   "--source connector so the pairing token matches the app.")
    a = ap.parse_args()

    port = free_port()
    home = a.home or tempfile.mkdtemp(prefix="wewalla-hw-")
    env = dict(os.environ, WEWALLA_HOME=home)
    cmd = [os.path.join(HERE, "..", "bin", "wewalla"), "run", "--source", a.source,
           "--http", "127.0.0.1:%d" % port, "--no-wakelock"] + (["--bed-raw", a.bed_raw] if a.bed_raw else [])
    proc = subprocess.Popen(cmd, env=env, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True)
    url = "http://127.0.0.1:%d/api/v1/state" % port
    t0 = time.time()
    polls, errors = [], 0
    try:
        while time.time() - t0 < a.seconds:
            time.sleep(a.poll)
            if proc.poll() is not None:
                break
            try:
                polls.append(json.load(urllib.request.urlopen(url, timeout=5)))
            except OSError:
                errors += 1
    finally:
        alive = proc.poll() is None
        proc.terminate()
        try:
            _, err = proc.communicate(timeout=10)
        except subprocess.TimeoutExpired:
            proc.kill()
            _, err = proc.communicate()

    q = [p.get("quality") or {} for p in polls]
    conf = [p["confidence"]["score"] for p in polls if p.get("confidence", {}).get("level") not in (None, "none")]
    motion = [p["motion"] for p in polls if p.get("motion") is not None]
    report = {
        "source": a.source,
        "requested_s": a.seconds,
        "ran_s": round(time.time() - t0, 1),
        "process_alive_at_end": alive,
        "stderr_tail": (err or "").strip().splitlines()[-5:],
        "polls_ok": len(polls),
        "poll_errors": errors,
        "signal_kind": collections.Counter(x.get("signal_kind") for x in q).most_common(),
        "series_max": max((x.get("series", 0) for x in q), default=0),
        "update_hz_median": statistics.median([x.get("update_hz_median", 0) for x in q]) if q else None,
        "update_hz_max": max((x.get("update_hz_max", 0) for x in q), default=None),
        "states": collections.Counter(p.get("state") for p in polls).most_common(),
        "presence": collections.Counter(str(p.get("presence")) for p in polls).most_common(),
        "motion_median": statistics.median(motion) if motion else None,
        "motion_max": max(motion) if motion else None,
        "breathing_reasons": collections.Counter(str((p.get("breathing") or {}).get("reason")) for p in polls).most_common(),
        "heart_reasons": collections.Counter(str((p.get("heart_rate") or {}).get("reason")) for p in polls).most_common(),
        "confidence_median": statistics.median(conf) if conf else None,
        "confidence_levels": collections.Counter(p.get("confidence", {}).get("level") for p in polls).most_common(),
        "dropped_max": max((p.get("dropped", 0) for p in polls), default=0),
        "last_source_info": polls[-1].get("source") if polls else None,
        "bed": bed_summary([p.get("bed") for p in polls if p.get("bed")]),
        "measured_at": time.strftime("%Y-%m-%dT%H:%M:%S%z"),
    }
    print(json.dumps(report, indent=1, default=str))
    return 0 if alive and polls else 1


if __name__ == "__main__":
    sys.exit(main())
