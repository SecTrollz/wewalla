#!/usr/bin/env python3
"""wewalla-lite: a dependency-free UDP stream monitor with a browser dashboard.

Runs in Termux without root and without compiling anything (Python stdlib only).

What it does
  * listens for UDP packets from ESP32 sensor nodes and shows, per node, packet rate,
    totals and last-seen time (a "is my node alive and reaching the phone?" tool);
  * optionally relays the raw packets to the real sensing server (--forward);
  * optionally samples the phone's own WiFi RSSI via Termux:API (--rssi) as a coarse,
    experimental motion hint.

What it does NOT do: decode CSI, estimate pose or vitals, or replace the sensing server.
Not a medical device or safety system.
"""
import argparse
import collections
import ipaddress
import json
import secrets
import socket
import statistics
import subprocess
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

MAX_SOURCES = 64          # bounds memory if something floods the port
MAX_SSE_CLIENTS = 8
WINDOW = 5.0              # seconds used for the packets/s figure


class State:
    def __init__(self):
        self.lock = threading.Lock()
        self.sources = {}
        self.total = 0
        self.rejected = 0
        self.dropped = 0
        self.started = time.time()
        self.history = collections.deque(maxlen=120)   # aggregate packets/s, 1 Hz
        self.rssi = collections.deque(maxlen=120)
        self.rssi_on = False
        self.rssi_err = None

    def record(self, ip, n, head):
        now = time.time()
        with self.lock:
            s = self.sources.get(ip)
            if s is None:
                if len(self.sources) >= MAX_SOURCES:
                    self.dropped += 1
                    return
                s = {"packets": 0, "bytes": 0, "last": now, "recent": collections.deque(), "head": ""}
                self.sources[ip] = s
            s["packets"] += 1
            s["bytes"] += n
            s["last"] = now
            s["head"] = head
            r = s["recent"]
            r.append(now)
            while r and r[0] < now - WINDOW:
                r.popleft()
            self.total += 1

    def reject(self):
        with self.lock:
            self.rejected += 1

    def add_rssi(self, v):
        with self.lock:
            self.rssi.append(float(v))
            self.rssi_err = None

    def set_rssi_err(self, msg):
        with self.lock:
            self.rssi_err = msg

    def _pps(self, now):
        total = 0.0
        for s in self.sources.values():
            r = s["recent"]
            while r and r[0] < now - WINDOW:
                r.popleft()
            total += len(r) / WINDOW
        return round(total, 1)

    def sample(self):
        with self.lock:
            self.history.append(self._pps(time.time()))

    def snapshot(self):
        now = time.time()
        with self.lock:
            nodes = []
            for ip, s in sorted(self.sources.items()):
                r = s["recent"]
                while r and r[0] < now - WINDOW:
                    r.popleft()
                nodes.append({"ip": ip, "packets": s["packets"], "bytes": s["bytes"],
                              "pps": round(len(r) / WINDOW, 1), "age": round(now - s["last"], 1),
                              "head": s["head"]})
            vals = list(self.rssi)
            rssi = None
            if self.rssi_on:
                recent = vals[-30:]
                sd = round(statistics.pstdev(recent), 2) if len(recent) >= 5 else None
                rssi = {"now": vals[-1] if vals else None, "stdev": sd, "series": vals[-60:], "error": self.rssi_err}
            return {"uptime": round(now - self.started), "total": self.total, "rejected": self.rejected,
                    "dropped": self.dropped, "pps": self._pps(now), "history": list(self.history)[-60:],
                    "nodes": nodes, "rssi": rssi}


def allowed(ip, nets):
    try:
        addr = ipaddress.ip_address(ip)
    except ValueError:
        return False
    if nets:
        return any(addr in n for n in nets)
    return addr.is_private       # default: LAN/loopback/link-local only, never the open internet


def udp_loop(sock, st, nets, fwd):
    while True:
        try:
            data, (ip, _port) = sock.recvfrom(4096)
        except OSError:
            return
        if not allowed(ip, nets):
            st.reject()
            continue
        st.record(ip, len(data), data[:4].hex())
        if fwd:
            try:
                sock.sendto(data, fwd)
            except OSError:
                pass


def sampler(st):
    while True:
        time.sleep(1)
        st.sample()


def rssi_loop(st, interval):
    while True:
        err = None
        try:
            out = subprocess.run(["termux-wifi-connectioninfo"], capture_output=True, text=True, timeout=10)
            val = json.loads(out.stdout).get("rssi")
            if isinstance(val, (int, float)):
                st.add_rssi(val)
            else:
                err = "no RSSI in response (is the Termux:API app installed and Location permission granted?)"
        except FileNotFoundError:
            err = "termux-api not installed (pkg install termux-api)"
        except Exception as exc:  # noqa: BLE001 - surface any failure to the UI instead of dying
            err = str(exc)[:120]
        if err:
            st.set_rssi_err(err)
        time.sleep(interval)


PAGE = r"""<!doctype html>
<html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>wewalla lite</title>
<style nonce="__N__">
:root{--bg:#f3efe6;--ink:#1c1a17;--mute:#6d675c;--card:#fbf8f1;--line:#d9d2c2;--acc:#0b6b57;--warn:#a4471a}
@media(prefers-color-scheme:dark){:root{--bg:#0f1512;--ink:#e6efe9;--mute:#8aa094;--card:#16201b;--line:#25332b;--acc:#5fe0b7;--warn:#ffb070}}
*{box-sizing:border-box}
body{margin:0;background:var(--bg);color:var(--ink);font:15px/1.45 ui-sans-serif,system-ui,-apple-system,"Segoe UI",sans-serif;padding:env(safe-area-inset-top) 0 env(safe-area-inset-bottom)}
main{max-width:760px;margin:0 auto;padding:18px 14px 40px}
h1{font:600 13px/1 ui-monospace,Menlo,Consolas,monospace;letter-spacing:.14em;text-transform:uppercase;color:var(--mute);margin:6px 0 14px}
.big{display:flex;align-items:baseline;gap:10px;flex-wrap:wrap}
.big b{font:600 54px/1 ui-monospace,Menlo,Consolas,monospace;color:var(--acc)}
.big span{color:var(--mute)}
#rnow{font-size:30px}
.pill{display:inline-block;padding:3px 10px;border:1px solid var(--line);border-radius:99px;font-size:13px;color:var(--mute)}
.pill.live{color:var(--acc);border-color:var(--acc)}
.card{background:var(--card);border:1px solid var(--line);border-radius:10px;padding:12px 14px;margin-top:14px}
.card h2{margin:0 0 8px;font:600 12px/1 ui-monospace,Menlo,Consolas,monospace;letter-spacing:.1em;text-transform:uppercase;color:var(--mute)}
canvas{width:100%;height:64px;display:block}
.scroll{overflow-x:auto}
table{border-collapse:collapse;width:100%;font:13px ui-monospace,Menlo,Consolas,monospace}
th,td{text-align:left;padding:5px 8px 5px 0;white-space:nowrap}
th{color:var(--mute);font-weight:500;border-bottom:1px solid var(--line)}
.stale{color:var(--warn)}
.note{color:var(--mute);font-size:12.5px;margin-top:10px}
</style></head><body><main>
<h1>wewalla lite &middot; raw stream monitor</h1>
<div class="big"><b id="pps">0.0</b><span>packets / s</span><span id="pill" class="pill">waiting for nodes</span></div>
<div class="card"><h2>Aggregate rate (last minute)</h2><canvas id="c1" width="700" height="64"></canvas></div>
<div class="card"><h2>Nodes</h2><div class="scroll"><table><thead><tr><th>address</th><th>pkt/s</th><th>total</th><th>last seen</th><th>hdr</th></tr></thead><tbody id="rows"></tbody></table></div>
<div id="empty" class="note">No packets yet. Point a node's <code>--target-ip</code> at this phone, UDP port <span id="port"></span>.</div></div>
<div class="card" id="rcard" hidden><h2>Phone WiFi RSSI &middot; experimental</h2>
<div class="big"><b id="rnow">-</b><span>dBm</span><span id="rsd" class="pill"></span></div>
<canvas id="c2" width="700" height="64"></canvas>
<div class="note" id="rerr"></div>
<div class="note">Coarse hint only: Android throttles and caches WiFi readings. Not real CSI sensing.</div></div>
<div class="note" id="foot"></div>
<div class="note">Monitors raw UDP only. It does not decode CSI or estimate pose/vitals, and is not a medical or safety device. Sense only spaces where everyone present has agreed.</div>
</main>
<script nonce="__N__">
const $=id=>document.getElementById(id);
function spark(cv,a){const x=cv.getContext('2d'),w=cv.width,h=cv.height;x.clearRect(0,0,w,h);if(!a||a.length<2)return;
 let lo=Math.min(...a),hi=Math.max(...a);if(hi-lo<1){hi=lo+1}
 x.lineWidth=2;x.strokeStyle=getComputedStyle(document.documentElement).getPropertyValue('--acc');x.beginPath();
 a.forEach((v,i)=>{const px=i*(w-4)/(a.length-1)+2,py=h-4-(v-lo)/(hi-lo)*(h-8);i?x.lineTo(px,py):x.moveTo(px,py)});x.stroke()}
function cell(tr,t,cls){const td=document.createElement('td');td.textContent=t;if(cls)td.className=cls;tr.appendChild(td)}
function render(s){
 $('pps').textContent=s.pps.toFixed(1);
 const live=s.nodes.some(n=>n.age<5);const p=$('pill');p.textContent=live?'receiving':(s.nodes.length?'nodes quiet':'waiting for nodes');p.className='pill'+(live?' live':'');
 spark($('c1'),s.history);
 const rows=$('rows');rows.replaceChildren();
 s.nodes.forEach(n=>{const tr=document.createElement('tr');cell(tr,n.ip);cell(tr,n.pps.toFixed(1));cell(tr,String(n.packets));cell(tr,n.age.toFixed(1)+' s ago',n.age>=5?'stale':'');cell(tr,n.head);rows.appendChild(tr)});
 $('empty').hidden=s.nodes.length>0;
 const r=s.rssi;$('rcard').hidden=!r;
 if(r){$('rnow').textContent=r.now===null?'-':r.now;$('rsd').textContent=r.stdev===null?'warming up':('spread '+r.stdev+' dB \u00b7 '+(r.stdev>2?'varying':'steady'));$('rerr').textContent=r.error||'';spark($('c2'),r.series)}
 $('foot').textContent='up '+s.uptime+' s \u00b7 '+s.total+' packets \u00b7 '+s.rejected+' rejected (non-LAN source) \u00b7 '+s.dropped+' dropped (node limit)';
}
fetch('/api/state').then(r=>r.json()).then(s=>{$('port').textContent=s.udp_port||''});
const es=new EventSource('/events');es.onmessage=e=>{const s=JSON.parse(e.data);$('port').textContent=s.udp_port||$('port').textContent;render(s)};
es.onerror=()=>{$('pill').textContent='reconnecting...';$('pill').className='pill'};
</script></body></html>
"""


def make_handler(state, udp_port, nonce, loopback_only, good_hosts):
    page = PAGE.replace("__N__", nonce).encode()
    sse_slots = threading.BoundedSemaphore(MAX_SSE_CLIENTS)

    class Handler(BaseHTTPRequestHandler):
        server_version = "wewalla-lite"

        def log_message(self, *args):
            pass

        def _headers(self, ctype, extra=None):
            self.send_header("Content-Type", ctype)
            self.send_header("Cache-Control", "no-store")
            self.send_header("X-Content-Type-Options", "nosniff")
            self.send_header("Referrer-Policy", "no-referrer")
            self.send_header("Content-Security-Policy",
                             "default-src 'none'; style-src 'nonce-%s'; script-src 'nonce-%s'; "
                             "connect-src 'self'; base-uri 'none'; frame-ancestors 'none'" % (nonce, nonce))
            for k, v in (extra or {}).items():
                self.send_header(k, v)

        def _state(self):
            s = state.snapshot()
            s["udp_port"] = udp_port
            return s

        def do_GET(self):  # noqa: N802
            # Defeats DNS-rebinding when bound to loopback: only accept our own host names.
            if loopback_only and (self.headers.get("Host") or "").lower() not in good_hosts:
                self.send_error(403, "bad host")
                return
            path = self.path.split("?", 1)[0]
            if path in ("/", "/index.html"):
                self.send_response(200)
                self._headers("text/html; charset=utf-8")
                self.send_header("Content-Length", str(len(page)))
                self.end_headers()
                self.wfile.write(page)
            elif path == "/api/state":
                body = json.dumps(self._state()).encode()
                self.send_response(200)
                self._headers("application/json")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)
            elif path == "/healthz":
                self.send_response(200)
                self._headers("text/plain")
                self.send_header("Content-Length", "2")
                self.end_headers()
                self.wfile.write(b"ok")
            elif path == "/events":
                if not sse_slots.acquire(blocking=False):
                    self.send_error(503, "too many viewers")
                    return
                try:
                    self.send_response(200)
                    self._headers("text/event-stream")
                    self.end_headers()
                    while True:
                        self.wfile.write(b"data: " + json.dumps(self._state()).encode() + b"\n\n")
                        self.wfile.flush()
                        time.sleep(1)
                except OSError:
                    pass
                finally:
                    sse_slots.release()
            else:
                self.send_error(404)

    return Handler


def selftest(target):
    st = State()
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    s.bind(("0.0.0.0", 0))
    port = s.getsockname()[1]
    threading.Thread(target=udp_loop, args=(s, st, [], None), daemon=True).start()
    c = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    try:
        c.sendto(b"WWTEST", (target, port))
    except OSError as exc:
        print("selftest: FAIL (%s)" % exc)
        return 1
    for _ in range(30):
        if st.total >= 1:
            print("selftest: PASS (sent to %s:%d, received)" % (target, port))
            return 0
        time.sleep(0.1)
    print("selftest: FAIL (no packet seen; source rejected=%d)" % st.rejected)
    return 1


def parse_hostport(v):
    host, _, port = v.rpartition(":")
    if not host or not port.isdigit():
        raise argparse.ArgumentTypeError("expected HOST:PORT")
    return host, int(port)


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--udp-port", type=int, default=5005)
    ap.add_argument("--udp-bind", default="0.0.0.0")
    ap.add_argument("--http-port", type=int, default=3100)
    ap.add_argument("--http-bind", default="127.0.0.1",
                    help="default 127.0.0.1 (this phone only). 0.0.0.0 exposes the page, with no login, to your LAN")
    ap.add_argument("--allow", action="append", default=[], metavar="CIDR",
                    help="accept UDP only from these networks (repeatable). Default: any private/LAN address")
    ap.add_argument("--forward", type=parse_hostport, metavar="HOST:PORT", help="relay raw packets to another UDP listener")
    ap.add_argument("--rssi", action="store_true", help="sample phone WiFi RSSI via Termux:API (experimental)")
    ap.add_argument("--rssi-interval", type=float, default=2.0)
    ap.add_argument("--selftest", nargs="?", const="127.0.0.1", metavar="IP", help="send a test packet to IP and exit")
    ap.add_argument("--open", action="store_true", help="open the page with termux-open-url")
    a = ap.parse_args()

    if a.selftest:
        return selftest(a.selftest)

    try:
        nets = [ipaddress.ip_network(n, strict=False) for n in a.allow]
    except ValueError as exc:
        ap.error("bad --allow value: %s" % exc)

    st = State()
    st.rssi_on = a.rssi
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    try:
        sock.bind((a.udp_bind, a.udp_port))
    except OSError as exc:
        print("cannot bind UDP %s:%d (%s). Is another listener (or the sensing server) using it?" % (a.udp_bind, a.udp_port, exc))
        return 2

    nonce = secrets.token_urlsafe(12)
    loopback_only = a.http_bind in ("127.0.0.1", "localhost", "::1")
    good = {h % a.http_port for h in ("127.0.0.1:%d", "localhost:%d", "[::1]:%d")}
    handler = make_handler(st, a.udp_port, nonce, loopback_only, good)
    try:
        httpd = ThreadingHTTPServer((a.http_bind, a.http_port), handler)
    except OSError as exc:
        print("cannot bind HTTP %s:%d (%s). Use --http-port to pick another." % (a.http_bind, a.http_port, exc))
        return 2

    threading.Thread(target=udp_loop, args=(sock, st, nets, a.forward), daemon=True).start()
    threading.Thread(target=sampler, args=(st,), daemon=True).start()
    if a.rssi:
        threading.Thread(target=rssi_loop, args=(st, a.rssi_interval), daemon=True).start()

    url = "http://127.0.0.1:%d/" % a.http_port
    print("wewalla-lite")
    print("  UDP listener : %s:%d  (accepting %s)" % (a.udp_bind, a.udp_port, ", ".join(a.allow) or "private/LAN sources only"))
    print("  Dashboard    : %s" % url)
    if not loopback_only:
        print("  WARNING      : dashboard is reachable from your network and has no login")
    if a.forward:
        print("  Forwarding   : raw packets -> %s:%d" % a.forward)
    print("  Ctrl-C to stop")
    if a.open:
        try:
            subprocess.Popen(["termux-open-url", url])
        except OSError:
            print("  (termux-open-url not available; open the URL above in your browser)")
    try:
        httpd.serve_forever()
    except KeyboardInterrupt:
        print("\nstopped")
    return 0


if __name__ == "__main__":
    sys.exit(main())
