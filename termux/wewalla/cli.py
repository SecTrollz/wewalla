import argparse
import json
import os
import shutil
import subprocess
import sys
import time

from . import __version__, config
from .bcg import BcgAnalyzer
from .dsp import Analyzer
from .mapping import MapStore
from .pipeline import FrameEmitter, Runtime
from .server import make_server, serve_in_thread
from .sources.adr018_udp import Adr018Source
from .sources.auto import AutoSource, MultiSource
from .sources.bed import BedSource
from .sources.connector import DEFAULT_PORT, ConnectorSource
from .sources.replay import Recorder, ReplaySource
from .sources.sim import SimSource
from .sources.termux_api import TermuxApiSource, run_json

PKG = "com.sectrollz.wewalla.connector"


def hostport(s, default_port):
    h, _, p = s.rpartition(":")
    return (h or "127.0.0.1", int(p)) if _ else (s, default_port)


def source_kinds(args):
    return [k.strip() for k in args.source.split(",") if k.strip()]


def build_source(args, tok, bcg=None):
    kinds = source_kinds(args)
    conn = lambda: ConnectorSource(tok, "127.0.0.1", args.connector_port)
    csi = lambda: Adr018Source(args.csi_bind, args.csi_port)
    # the bed source owns the accelerometer stream; a second termux-sensor poller would compete
    tapi = lambda: TermuxApiSource(imu=not args.no_imu and "bed" not in kinds)
    if kinds == ["auto"]:
        return AutoSource(conn(), tapi(), extra=[csi()] if args.with_csi else [])
    made = []
    for k in kinds:
        if k == "connector":
            made.append(conn())
        elif k == "termux":
            made.append(tapi())
        elif k in ("adr018", "csi"):
            made.append(csi())
        elif k == "bed":
            if bcg is None:
                sys.exit("source=bed needs a BCG analyzer (use `wewalla run`)")
            made.append(BedSource(bcg, raw_path=getattr(args, "bed_raw", None)))
        elif k == "sim":
            made.append(SimSource(args.scenario))
        elif k == "replay":
            if not args.replay:
                sys.exit("--replay FILE required for source=replay")
            made.append(ReplaySource(args.replay, args.speed))
        else:
            sys.exit("unknown source %r" % k)
    return made[0] if len(made) == 1 else MultiSource(made)


def cmd_run(args):
    tok = config.token()
    cal = None if args.no_calibration else config.load_calibration()
    an = Analyzer(calibration=cal)
    emitter = None
    if args.emit_udp:
        h, p = hostport(args.emit_udp, 5005)
        emitter = FrameEmitter(h, p)
    alerts = None
    if args.alerts:
        from .alerts import Alerts
        alerts = Alerts(speak=args.speak)
    bcg = BcgAnalyzer() if "bed" in source_kinds(args) else None
    mapstore = MapStore(str(config.home() / "map.json"))
    rt = Runtime(build_source(args, tok, bcg), an, emitter, Recorder(args.record) if args.record else None,
                 alerts, bcg=bcg, mapstore=mapstore)
    h, p = hostport(args.http, 8080)
    srv = make_server(rt, tok, h, p)
    rt.start()
    serve_in_thread(srv)
    if shutil.which("termux-wake-lock") and not args.no_wakelock:
        subprocess.run(["termux-wake-lock"], stderr=subprocess.DEVNULL)
    print("wewalla %s | source=%s | dashboard http://%s:%d/ | room map http://%s:%d/map | calibration=%s" % (
        __version__, args.source, h, srv.server_address[1], h, srv.server_address[1],
        "loaded" if cal else "none (adaptive)"))
    if args.calibrate:
        rt.calibrate(args.calibrate)
        print("calibrating for %ds - keep the room empty" % args.calibrate)
    try:
        while True:
            time.sleep(args.print_every)
            s = rt.state
            q = s.get("quality", {})
            print("%s presence=%s motion=%s breath=%s | %s series=%s %sHz" % (
                time.strftime("%H:%M:%S"), s.get("presence"), s.get("motion"),
                (s.get("breathing") or {}).get("bpm") or (s.get("breathing") or {}).get("reason"),
                q.get("signal_kind"), q.get("series"), q.get("update_hz_median")) + bed_line(s), flush=True)
    except KeyboardInterrupt:
        pass
    finally:
        rt.stop()
        srv.shutdown()
        if shutil.which("termux-wake-unlock") and not args.no_wakelock:
            subprocess.run(["termux-wake-unlock"], stderr=subprocess.DEVNULL)


def bed_line(s):
    b = s.get("bed")
    if not b:
        return ""
    v = lambda o: (o or {}).get("bpm") or (o or {}).get("reason")
    return " | bed %s heart=%s breath=%s %sHz" % (b.get("state"), v(b.get("heart")), v(b.get("breathing")), b.get("fs"))


def cmd_up(args):
    """Everything `run` needs, done in order, saying what worked and what did not."""
    ok = lambda m: print("  [ ok ] " + m, flush=True)
    no = lambda m, hint: print("  [ -- ] %s\n         %s" % (m, hint), flush=True)
    print("wewalla up", flush=True)
    config.token()
    ok("pairing token ready (%s/token)" % config.home())
    installed = bool(shutil.which("pm")) and bool(
        subprocess.run(["pm", "path", PKG], capture_output=True, text=True).stdout.strip())
    started = False
    if installed:
        r = _am("start", "-f", ACTIVITY_FLAGS, "-n", PKG + "/.MainActivity", "--ez", "autostart", "true",
                "--es", "token", config.token(), "--es", "host", "127.0.0.1", "--ei", "port",
                str(args.connector_port), "--es", "modes", "conn,scan,rtt", "--ei", "rate", "8")
        started = r.returncode == 0
        (ok if started else lambda m: no(m, "open the Wewalla Connector app and tap 2. Start sensing"))(
            "connector app started" if started else "connector app did not start: " + (r.stderr.strip()[-120:]))
    else:
        no("connector app not installed: using slower Termux:API polling",
           "better data: connector-apk/build.sh, then open the APK from the Files app")
    wifi = "connector" if installed else "termux"
    kinds = [wifi] + ([] if args.no_bed else ["bed"])
    args.source = args.source or ",".join(kinds)
    ok("sources: %s%s" % (args.source, "" if args.no_bed else
                          "   (bed = phone flat on the mattress beside you for heart/breathing)"))
    url = "http://%s:%d/" % hostport(args.http, 8080)
    if not args.no_browser and shutil.which("termux-open-url"):
        # the server starts within a second; the browser retries on its own if it is early
        subprocess.Popen(["sh", "-c", "sleep 2; termux-open-url %s" % url],
                         stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        ok("opening the dashboard: " + url)
    else:
        ok("dashboard: " + url)
    print("  Ctrl+C stops sensing%s.\n" % (" (the connector app keeps its notification until `wewalla connector stop`)"
                                            if started else ""))
    return cmd_run(args)


def cmd_calibrate(args):
    args.calibrate = args.seconds
    tok = config.token()
    an = Analyzer()
    rt = Runtime(build_source(args, tok), an)
    rt.start()
    print("calibrating %ds - keep the room empty (source=%s)" % (args.seconds, args.source))
    rt.calibrate(args.seconds)
    end = time.time() + args.seconds + 3
    while time.time() < end and rt.calibrating:
        time.sleep(1)
    time.sleep(1.5)
    res = rt.state.get("calibration", "failed:no_data")
    rt.stop()
    print(res)
    return 0 if res.startswith("saved") else 1


def cmd_pair(args):
    tok = config.token()
    print(tok)
    if shutil.which("termux-clipboard-set"):
        subprocess.run(["termux-clipboard-set", tok])
        print("(copied to clipboard - paste it in the Wewalla Connector app)", file=sys.stderr)


# NEW_TASK | CLEAR_TOP | SINGLE_TOP: if the app is already open (e.g. via the installer's "Open"),
# Android would otherwise just bring its task to the front and silently drop our extras (token...).
ACTIVITY_FLAGS = "0x34000000"


def _am(*a):
    # "--user 0": Termux's am otherwise asks for user -2 (current), which Android 14+ refuses from an
    # app uid (SecurityException: INTERACT_ACROSS_USERS) - the launch silently never happens.
    r = subprocess.run(["am", a[0], "--user", "0"] + list(a[1:]), capture_output=True, text=True, timeout=20)
    err = r.stderr.strip()
    if "Exception" in err or "Error" in err:
        print("am failed: " + err.splitlines()[-1], file=sys.stderr)
        r.returncode = r.returncode or 1
    return r


def cmd_connector(args):
    if args.action == "install":
        apk = args.apk or str(config.home() / "wewalla-connector.apk")
        if not os.path.exists(apk):
            sys.exit("no APK at %s - build it with: connector-apk/build.sh" % apk)
        if shutil.which("termux-open"):
            subprocess.run(["termux-open", "--content-type", "application/vnd.android.package-archive", apk])
            print("Android's installer should open. Allow 'Install unknown apps' for Termux if asked.\n"
                  "If it says 'problem parsing the package', copy the APK to shared storage and open it\n"
                  "from the Files app instead:  cp %s ~/storage/downloads/" % apk)
        else:
            print("Install manually:", apk)
    elif args.action == "start":
        r = _am("start", "-f", ACTIVITY_FLAGS, "-n", PKG + "/.MainActivity", "--ez", "autostart", "true", "--es", "token", config.token(),
                "--es", "host", "127.0.0.1", "--ei", "port", str(args.port), "--es", "modes", args.modes,
                "--ei", "rate", str(args.rate))
        print(r.stdout.strip() or r.stderr.strip())
        return r.returncode
    elif args.action == "stop":
        r = _am("start", "-f", ACTIVITY_FLAGS, "-n", PKG + "/.MainActivity", "--ez", "stop", "true")
        print(r.stdout.strip() or r.stderr.strip())
        return r.returncode
    elif args.action == "status":
        r = subprocess.run(["pm", "path", PKG], capture_output=True, text=True)
        print("installed:", bool(r.stdout.strip()))


def cmd_doctor(args):
    ok = True

    def line(good, msg, hint=""):
        nonlocal ok
        ok = ok and good
        print(("[ ok ] " if good else "[FAIL] ") + msg + (("\n       -> " + hint) if hint and not good else ""))

    line(sys.version_info >= (3, 8), "python %s" % sys.version.split()[0], "pkg install python")
    on_termux = "com.termux" in os.environ.get("PREFIX", "") or os.path.exists("/data/data/com.termux")
    line(True, "running on Termux" if on_termux else "not on Termux (fine for development)")
    for b in ("termux-wifi-connectioninfo", "termux-wifi-scaninfo", "termux-sensor", "termux-notification"):
        line(shutil.which(b) is not None, "%s present" % b, "pkg install termux-api")
    d = run_json(["termux-wifi-connectioninfo"], 8) if shutil.which("termux-wifi-connectioninfo") else None
    if isinstance(d, dict) and "rssi" in d:
        line(True, "Termux:API app answers (rssi %s dBm on %s)" % (d["rssi"], d.get("ssid")))
        line(str(d.get("bssid", "")).lower() not in ("", "02:00:00:00:00:00"), "location permission for Termux:API (BSSID visible)",
             "Android settings > Apps > Termux:API > Permissions > Location: allow, and turn Location ON")
    else:
        line(False, "Termux:API app answers", "install the Termux:API *app* (same source as Termux, e.g. F-Droid) and connect to Wi-Fi: %s" % (d,))
    r = subprocess.run(["pm", "path", PKG], capture_output=True, text=True) if shutil.which("pm") else None
    line(bool(r and r.stdout.strip()), "connector APK installed", "build it (connector-apk/build.sh) then: wewalla connector install")
    line(config.token(create=False) is not None, "pairing token exists", "run: wewalla pair")
    line(config.load_calibration() is not None, "empty-room calibration saved", "optional but recommended: wewalla calibrate")
    print("\nall required checks passed" if ok else "\nsome checks failed (see hints)")
    return 0 if ok else 1


def main(argv=None):
    ap = argparse.ArgumentParser(prog="wewalla", description="WiFi sensing on Termux: no root, no ESP32")
    ap.add_argument("--version", action="version", version=__version__)
    sub = ap.add_subparsers(dest="cmd", required=True)

    def common(p):
        p.add_argument("--source", default="auto", help="auto|connector|termux|bed|adr018|sim|replay (comma-separate to combine, "
                            "e.g. connector,bed; bed = phone on the mattress, accelerometer heart/breathing)")
        p.add_argument("--connector-port", type=int, default=DEFAULT_PORT)
        p.add_argument("--bed-raw", metavar="CSV", help="also save raw bed accelerometer t,x,y,z (private body data)")
        p.add_argument("--csi-bind", default="0.0.0.0")
        p.add_argument("--csi-port", type=int, default=5005)
        p.add_argument("--with-csi", action="store_true", help="in auto mode also listen for real ADR-018 CSI")
        p.add_argument("--no-imu", action="store_true")
        p.add_argument("--scenario", default="cycle", help="sim: empty|walk|breathe|cycle")
        p.add_argument("--replay")
        p.add_argument("--speed", type=float, default=1.0)

    def run_opts(r):
        r.add_argument("--http", default="127.0.0.1:8080")
        r.add_argument("--emit-udp", help="host:port to forward derived ADR-018 frames (e.g. the Rust sensing-server)")
        r.add_argument("--record")
        r.add_argument("--alerts", action="store_true")
        r.add_argument("--speak", action="store_true")
        r.add_argument("--calibrate", type=int, default=0)
        r.add_argument("--no-calibration", action="store_true")
        r.add_argument("--no-wakelock", action="store_true")
        r.add_argument("--print-every", type=float, default=5.0)

    r = sub.add_parser("run", help="start sensing + dashboard")
    common(r)
    run_opts(r)
    r.set_defaults(fn=cmd_run)

    u = sub.add_parser("up", help="one step: start the connector app, pick sources, open the dashboard")
    common(u)
    run_opts(u)
    u.add_argument("--no-bed", action="store_true", help="skip the bed accelerometer (heart/breathing)")
    u.add_argument("--no-browser", action="store_true")
    u.set_defaults(fn=cmd_up, source=None)

    c = sub.add_parser("calibrate", help="learn the empty-room baseline")
    common(c)
    c.add_argument("--seconds", type=int, default=60)
    c.set_defaults(fn=cmd_calibrate)

    sub.add_parser("pair", help="print/copy the pairing token").set_defaults(fn=cmd_pair)

    k = sub.add_parser("connector", help="manage the connector APK")
    k.add_argument("action", choices=["install", "start", "stop", "status"])
    k.add_argument("--apk")
    k.add_argument("--port", type=int, default=DEFAULT_PORT)
    k.add_argument("--modes", default="conn,scan,rtt")
    k.add_argument("--rate", type=int, default=8)
    k.set_defaults(fn=cmd_connector)

    sub.add_parser("doctor", help="check the Termux setup").set_defaults(fn=cmd_doctor)

    args = ap.parse_args(argv)
    return args.fn(args) or 0


if __name__ == "__main__":
    sys.exit(main())
