import json, os, socket, stat, subprocess, sys, tempfile, textwrap, threading, time, unittest, urllib.request, urllib.error
ROOT = os.path.join(os.path.dirname(__file__), "..")
sys.path.insert(0, ROOT)
os.environ["WEWALLA_HOME"] = tempfile.mkdtemp(prefix="wew-home-")

from wewalla import adr018, config
from wewalla.dsp import Analyzer
from wewalla.model import Sample
from wewalla.pipeline import FrameEmitter, Runtime
from wewalla.server import make_server, serve_in_thread
from wewalla.sources.adr018_udp import Adr018Source
from wewalla.sources.auto import AutoSource
from wewalla.sources.connector import ConnectorSource
from wewalla.sources.sim import SimSource
from wewalla.sources.termux_api import TermuxApiSource

TOK = "tok-123"


def udp_send(port, obj):
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    s.sendto(json.dumps(obj).encode(), ("127.0.0.1", port))
    s.close()


def wait(cond, timeout=5.0):
    end = time.time() + timeout
    while time.time() < end:
        if cond():
            return True
        time.sleep(0.02)
    return False


class TestConnector(unittest.TestCase):
    def setUp(self):
        self.got, self.imu = [], []
        self.src = ConnectorSource(TOK, port=0)
        self.src.bind()
        self.src.start(self.got.append, self.imu.append)

    def tearDown(self):
        self.src.stop()

    def test_obs_hello_imu_over_real_udp(self):
        p = self.src.port
        udp_send(p, {"v": 1, "tok": TOK, "t": "hello", "dev": "Pixel", "sdk": 34, "rtt": True, "modes": ["conn"]})
        udp_send(p, {"v": 1, "tok": TOK, "t": "obs", "o": [
            {"k": "conn", "id": "AA:BB:CC:DD:EE:01", "f": 5180, "r": -52},
            {"k": "scan", "id": "aa:bb:cc:dd:ee:02", "f": 2437, "r": -61, "u": 1000},
            {"k": "rtt", "id": "aa:bb:cc:dd:ee:03", "d": 4210, "s": 120, "r": -58, "n": 8},
            {"k": "rtt", "id": "aa:bb:cc:dd:ee:04", "d": 100, "s": 9000, "r": -58}]})   # junk sd -> dropped
        udp_send(p, {"v": 1, "tok": TOK, "t": "imu", "m": 0.9})
        self.assertTrue(wait(lambda: len(self.got) >= 4 and self.imu))
        keys = {s.key: s for s in self.got}
        self.assertIn("conn:aa:bb:cc:dd:ee:01", keys)          # normalised to lowercase
        self.assertEqual(keys["rtt:aa:bb:cc:dd:ee:03:d"].group, "rtt")
        self.assertEqual(keys["rtt:aa:bb:cc:dd:ee:03:d"].value, 4210.0)
        self.assertNotIn("rtt:aa:bb:cc:dd:ee:04:d", keys)
        self.assertEqual(self.imu, [0.9])
        self.assertEqual(self.src.last_hello["dev"], "Pixel")
        self.assertTrue(self.src.alive())

    def test_wrong_token_and_garbage_rejected(self):
        p = self.src.port
        udp_send(p, {"v": 1, "tok": "evil", "t": "obs", "o": [{"k": "conn", "id": "x", "r": -50}]})
        udp_send(p, {"v": 2, "tok": TOK, "t": "obs", "o": []})
        s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM); s.sendto(b"\xff\xfe not json", ("127.0.0.1", p)); s.close()
        self.assertTrue(wait(lambda: self.src.rejected >= 3))
        self.assertEqual(self.got, [])
        self.assertFalse(self.src.alive())

    def test_scan_freshness_dedup(self):
        p = self.src.port
        for u in (500, 500, 900):
            udp_send(p, {"v": 1, "tok": TOK, "t": "obs", "o": [{"k": "scan", "id": "aa:aa:aa:aa:aa:aa", "r": -60, "u": u}]})
            time.sleep(0.05)
        self.assertTrue(wait(lambda: len(self.got) == 3))
        self.assertEqual([s.fresh for s in self.got], [True, False, True])


FAKE_CONN = '#!/bin/sh\necho \'{"bssid":"AA:BB:CC:00:00:01","frequency_mhz":5200,"rssi":%d,"ssid":"x","link_speed_mbps":433}\'\n'
FAKE_SCAN = '#!/bin/sh\necho \'[{"bssid":"AA:BB:CC:00:00:02","frequency_mhz":2412,"rssi":-63,"timestamp":777},{"bssid":"AA:BB:CC:00:00:03","frequency_mhz":5180,"rssi":-70,"timestamp":778}]\'\n'
# Mirrors a real Motorola Edge 2025: vendor sensor names, "accelerometer" matches nothing,
# -n N prints N readings, no -n streams until killed; -c is a no-op.
FAKE_SENSOR = textwrap.dedent('''#!/bin/sh
case "$1" in
  -l) printf '{\\n  "sensors": [\\n    "icm45621_acc",\\n    "linear_acc",\\n    "icm45621_gyro"\\n  ]\\n}\\n'; exit 0 ;;
  -c) exit 0 ;;
esac
[ "$2" = "icm45621_acc" ] || { echo "No valid sensors were registered!"; exit 0; }
n=-1; [ "$5" = "-n" ] && n=$6
i=0
while [ $n -lt 0 ] || [ $i -lt $n ]; do
  for v in 9.8 9.9 9.7 9.85 9.75; do
    printf '{\\n  "icm45621_acc": {\\n    "values": [0.1, 0.2, %s]\\n  }\\n}\\n' $v
    i=$((i+1)); [ $n -ge 0 ] && [ $i -ge $n ] && exit 0
  done
  [ $n -lt 0 ] && sleep 0.05
done
''')


class TestTermuxApi(unittest.TestCase):
    def setUp(self):
        self.bin = tempfile.mkdtemp(prefix="fakebin-")
        for name, body in (("termux-wifi-connectioninfo", FAKE_CONN % -55), ("termux-wifi-scaninfo", FAKE_SCAN),
                           ("termux-sensor", FAKE_SENSOR)):
            p = os.path.join(self.bin, name)
            with open(p, "w") as fh:
                fh.write(body)
            os.chmod(p, 0o755)
        self.old = os.environ["PATH"]
        os.environ["PATH"] = self.bin + os.pathsep + self.old

    def tearDown(self):
        os.environ["PATH"] = self.old

    def test_polls_real_subprocesses(self):
        got, imu = [], []
        src = TermuxApiSource(poll_s=0.05, scan_s=0.2)
        self.assertTrue(src.available())
        src.start(got.append, imu.append)
        try:
            self.assertTrue(wait(lambda: any(s.key.startswith("scan:") for s in got) and sum(s.key.startswith("conn:") for s in got) >= 3 and imu, 8))
        finally:
            src.stop()
        conn = [s for s in got if s.key == "conn:aa:bb:cc:00:00:01"]
        self.assertEqual(conn[0].value, -55.0)
        self.assertTrue(conn[0].fresh)
        self.assertFalse(conn[1].fresh)          # identical RSSI re-delivered -> stale, not counted as an update
        self.assertEqual(conn[0].freq, 5200)
        self.assertGreaterEqual(len([s for s in got if s.key == "scan:aa:bb:cc:00:00:02"]), 1)
        self.assertGreater(imu[0], 0)             # accel magnitude std from concatenated pretty-printed JSON

    def test_error_payloads_do_not_crash(self):
        src = TermuxApiSource()
        got = []
        src.sink = got.append
        src.ingest_conn({"error": "Location permission not granted"}, 1.0)
        src.ingest_conn({"rssi": -127}, 1.0)
        src.ingest_scan({"nope": 1}, 1.0)
        src.ingest_scan([{"bad": 1}, {"bssid": "A", "rssi": "-70"}], 1.0)
        self.assertEqual(len(got), 1)
        self.assertEqual(src.errors, 2)
        self.assertIn("Location", src.last_error)

    def test_auto_falls_back_then_yields_to_connector(self):
        got = []
        conn = ConnectorSource(TOK, port=0)
        conn.bind()
        auto = AutoSource(conn, TermuxApiSource(poll_s=0.05, scan_s=0.3, imu=False), silence_s=0.4)
        auto.start(got.append)
        try:
            self.assertTrue(wait(lambda: auto.fallback_active, 5), "should fall back to Termux:API")
            stop_hb = threading.Event()
            def heartbeat():                       # what the APK does: a hello every couple of seconds + data
                while not stop_hb.is_set():
                    udp_send(conn.port, {"v": 1, "tok": TOK, "t": "hello"})
                    stop_hb.wait(0.1)
            threading.Thread(target=heartbeat, daemon=True).start()
            try:
                self.assertTrue(wait(lambda: not auto.fallback_active, 6), "should yield to the connector")
            finally:
                stop_hb.set()
            self.assertTrue(wait(lambda: auto.fallback_active, 6), "should fall back again when the connector goes silent")
        finally:
            auto.stop()


class TestBedSource(unittest.TestCase):
    def setUp(self):
        self.bin = tempfile.mkdtemp(prefix="fakebin-")
        p = os.path.join(self.bin, "termux-sensor")
        with open(p, "w") as fh:
            fh.write(FAKE_SENSOR)
        os.chmod(p, 0o755)
        self.old = os.environ["PATH"]
        os.environ["PATH"] = self.bin + os.pathsep + self.old

    def tearDown(self):
        os.environ["PATH"] = self.old

    def test_parse_stream_handles_pretty_json_and_garbage(self):
        from wewalla.sources.bed import BedSource
        lines = ['{\n', '  "icm45621_acc": {\n', '    "values": [1, 2, 3]\n', '  }\n', '}\n',
                 'No valid sensors were registered!\n', '{"a": {"values": [4, 5, 6]}}\n', '{"b": {"values": [1]}}\n']
        self.assertEqual(list(BedSource.parse_stream(lines)), [[1.0, 2.0, 3.0], [4.0, 5.0, 6.0]])

    def test_streams_vendor_named_accelerometer_into_bcg(self):
        from wewalla.bcg import BcgAnalyzer
        from wewalla.sources.bed import BedSource
        bcg, imu = BcgAnalyzer(), []
        src = BedSource(bcg)
        src.start(lambda s: None, imu.append)
        try:
            self.assertTrue(wait(lambda: src.readings >= 60 and imu, 10))
            self.assertEqual(src.info()["sensor"], "icm45621_acc")   # resolved, not "accelerometer"
            self.assertIsNotNone(bcg.fs)
        finally:
            src.stop()
        self.assertTrue(src._proc is None or src._proc.poll() is not None)


class TestEmitAndCsi(unittest.TestCase):
    def test_emit_is_valid_adr018_and_flagged_derived(self):
        rx = socket.socket(socket.AF_INET, socket.SOCK_DGRAM); rx.bind(("127.0.0.1", 0)); rx.settimeout(2)
        a = Analyzer(); now = time.time()
        for i in range(8):
            a.push(Sample(now, "scan:%02d" % i, -40 - 3 * i, "rssi", True, 2437))
        a.push(Sample(now, "rtt:x:d", 3000.0, "rtt"))     # rtt series must not leak into CSI frame
        FrameEmitter("127.0.0.1", rx.getsockname()[1]).send(a, now)
        f = adr018.decode(rx.recv(2048))
        self.assertEqual((f.n_ant, f.n_sc, f.freq_mhz), (1, 8, 2437))
        self.assertTrue(f.derived)
        self.assertEqual(f.iq[0], (-40 + 100) * 2)
        self.assertEqual(f.rssi, -40)

    def test_adr018_ingest_real_csi_and_rejects_derived(self):
        got = []
        src = Adr018Source("127.0.0.1", 0); src.bind(); src.start(got.append)
        try:
            tx = socket.socket(socket.AF_INET, socket.SOCK_DGRAM); p = src.port
            tx.sendto(adr018.encode(3, 2, 4, 2437, 1, -50, -92, [10, 0, 0, 20, 30, 40, 5, 5] * 2), ("127.0.0.1", p))
            tx.sendto(adr018.encode(3, 1, 2, 2437, 2, -50, -92, [1, 1, 1, 1], flags=adr018.FLAG_DERIVED), ("127.0.0.1", p))
            tx.sendto(bytes([0x03, 0, 0x11, 0xC5]) + b"\0" * 40, ("127.0.0.1", p))    # sibling packet: ignored
            tx.sendto(b"garbage!", ("127.0.0.1", p))
            self.assertTrue(wait(lambda: src.bad >= 1 and src.derived_rejected >= 1 and len(got) >= 8))
        finally:
            src.stop()
        self.assertEqual(len(got), 8)
        self.assertEqual({s.group for s in got}, {"csi"})
        self.assertAlmostEqual(next(s.value for s in got if s.key == "csi:3:0:2"), 50.0)  # |30+40j|


class TestServer(unittest.TestCase):
    def setUp(self):
        self.rt = Runtime(SimSource("walk", fs=8), Analyzer(), tick_s=0.2)
        self.rt.start()
        self.srv = make_server(self.rt, TOK, "127.0.0.1", 0)
        serve_in_thread(self.srv)
        self.base = "http://127.0.0.1:%d" % self.srv.server_address[1]

    def tearDown(self):
        self.srv.shutdown(); self.srv.server_close(); self.rt.stop()

    def req(self, path, method="GET", headers=None, data=None):
        r = urllib.request.Request(self.base + path, method=method, headers=headers or {}, data=data)
        try:
            with urllib.request.urlopen(r, timeout=5) as resp:
                return resp.status, resp.read(), resp.headers
        except urllib.error.HTTPError as e:
            return e.code, e.read(), e.headers

    def test_state_dashboard_health(self):
        self.assertTrue(wait(lambda: self.rt.seq >= 2))
        c, b, _ = self.req("/api/v1/state")
        st = json.loads(b)
        self.assertEqual(c, 200)
        self.assertIn("NOT CSI", st["quality"]["signal_kind"])
        self.assertIn("breathing", st)
        c, b, h = self.req("/")
        self.assertEqual(c, 200)
        self.assertIn(TOK.encode(), b)                       # token injected for same-origin JS
        self.assertNotIn(b"__TOKEN__", b)
        self.assertEqual(h["X-Content-Type-Options"], "nosniff")
        self.assertEqual(self.req("/api/v1/health")[0], 200)
        self.assertEqual(self.req("/nope")[0], 404)

    def test_post_requires_token_and_host_is_checked(self):
        body = b'{"seconds": 10}'
        self.assertEqual(self.req("/api/v1/calibrate", "POST", data=body)[0], 401)
        self.assertEqual(self.req("/api/v1/calibrate", "POST", {"X-Wewalla-Token": "wrong"}, body)[0], 401)
        self.assertEqual(self.req("/api/v1/state", headers={"Host": "evil.example.com"})[0], 403)  # DNS rebinding
        c, b, _ = self.req("/api/v1/calibrate", "POST", {"X-Wewalla-Token": TOK}, body)
        self.assertEqual(c, 202)
        self.assertTrue(self.rt.calibrating)
        self.assertEqual(self.req("/api/v1/calibrate", "POST", {"X-Wewalla-Token": TOK}, b"{oops")[0], 400)
        for bad in (b'{"seconds": "x"}', b'{"seconds": null}', b'[1]'):
            self.assertEqual(self.req("/api/v1/calibrate", "POST", {"X-Wewalla-Token": TOK}, bad)[0], 400)

    def test_sse_streams_state(self):
        s = socket.create_connection(("127.0.0.1", self.srv.server_address[1]), timeout=5)
        s.sendall(b"GET /events HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
        buf = b""
        end = time.time() + 6
        while b'"quality"' not in buf and time.time() < end:   # first event may be the pre-tick stub
            buf += s.recv(4096)
        s.close()
        self.assertIn(b"text/event-stream", buf)
        self.assertIn(b'"quality"', buf)


class TestEndToEndProcess(unittest.TestCase):
    def test_wewalla_run_sim_and_calibrate(self):
        home = tempfile.mkdtemp(prefix="wew-e2e-")
        env = dict(os.environ, WEWALLA_HOME=home)
        s = socket.socket(); s.bind(("127.0.0.1", 0)); port = s.getsockname()[1]; s.close()
        p = subprocess.Popen([sys.executable, "-m", "wewalla", "run", "--source", "sim", "--scenario", "walk",
                              "--http", "127.0.0.1:%d" % port, "--print-every", "1", "--no-wakelock"],
                             cwd=ROOT, env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
        try:
            state = None
            for _ in range(60):
                time.sleep(0.5)
                try:
                    state = json.load(urllib.request.urlopen("http://127.0.0.1:%d/api/v1/state" % port, timeout=2))
                    if state.get("quality"):
                        break
                except OSError:
                    pass
            self.assertIsNotNone(state)
            self.assertEqual(state["source"]["SYNTHETIC"], True)
        finally:
            p.terminate()
            out = p.communicate(timeout=10)[0]
        self.assertIn("dashboard http://127.0.0.1:%d/" % port, out)
        tok = open(os.path.join(home, "token")).read().strip()
        self.assertGreaterEqual(len(tok), 10)
        self.assertEqual(stat.S_IMODE(os.stat(os.path.join(home, "token")).st_mode), 0o600)


if __name__ == "__main__":
    unittest.main()


class TestRecordReplay(unittest.TestCase):
    def test_roundtrip(self):
        from wewalla.sources.replay import Recorder, ReplaySource
        path = os.path.join(tempfile.mkdtemp(), "rec.jsonl")
        r = Recorder(path)
        for i in range(5):
            r(Sample(100.0 + i * 0.05, "k%d" % (i % 2), -50.0 - i, "rssi", i % 2 == 0, 2412))
        r.close()
        got = []
        src = ReplaySource(path, speed=50)
        src.start(got.append)
        try:
            self.assertTrue(wait(lambda: len(got) == 5 and src.done))
        finally:
            src.stop()
        self.assertEqual([s.value for s in got], [-50.0, -51.0, -52.0, -53.0, -54.0])
        self.assertEqual([s.fresh for s in got], [True, False, True, False, True])
