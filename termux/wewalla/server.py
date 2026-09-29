"""Stdlib HTTP server: dashboard, JSON state, Server-Sent Events.

Security: bound to loopback by default. Host header is validated (DNS-rebinding), POST needs the
pairing token, and if you bind a non-loopback address every endpoint except /health needs it.
"""
import hmac
import json
import os
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlparse

LOOPBACK = {"127.0.0.1", "localhost", "::1", "[::1]"}


def make_server(runtime, token, host="127.0.0.1", port=8080):
    lan = host not in LOOPBACK
    def load(name):
        with open(os.path.join(os.path.dirname(__file__), name), encoding="utf-8") as fh:
            return fh.read()
    page, map_page, model3d = load("dashboard.html"), load("map.html"), load("model3d.html")
    MAX_BODY = {"/api/v1/map/pose": 262144, "/api/v1/map/planes": 262144, "/api/v1/map/voxels": 524288}

    class H(BaseHTTPRequestHandler):
        server_version = "wewalla"

        def log_message(self, *a):
            pass

        def _host_ok(self):
            h = (self.headers.get("Host") or "").strip().lower()
            if h.startswith("["):                       # [::1]:8080
                name = h.split("]")[0] + "]"
            else:
                name = h.split(":")[0]                  # 127.0.0.1:8080
            return lan or name in LOOPBACK

        def _token_ok(self):
            q = parse_qs(urlparse(self.path).query)
            got = self.headers.get("X-Wewalla-Token") or (q.get("token") or [""])[0]
            return hmac.compare_digest(got.encode(), token.encode())

        def _send(self, code, body, ctype="application/json"):
            b = body if isinstance(body, bytes) else body.encode()
            self.send_response(code)
            self.send_header("Content-Type", ctype)
            self.send_header("Content-Length", str(len(b)))
            self.send_header("Cache-Control", "no-store")
            self.send_header("X-Content-Type-Options", "nosniff")
            self.send_header("Content-Security-Policy", "default-src 'self'; style-src 'unsafe-inline'; script-src 'unsafe-inline'")
            self.end_headers()
            self.wfile.write(b)

        def do_GET(self):
            if not self._host_ok():
                return self._send(403, '{"error":"bad host"}')
            path = urlparse(self.path).path
            if path == "/api/v1/health":
                return self._send(200, json.dumps({"ok": True}))
            if lan and not self._token_ok():
                return self._send(401, '{"error":"token required"}')
            if path == "/":
                return self._send(200, page.replace("__TOKEN__", token), "text/html; charset=utf-8")
            if path == "/map":
                return self._send(200, map_page.replace("__TOKEN__", token), "text/html; charset=utf-8")
            if path == "/3d":
                return self._send(200, model3d.replace("__TOKEN__", token), "text/html; charset=utf-8")
            ms = getattr(runtime, "mapstore", None)
            if path.startswith("/api/v1/map"):
                if ms is None:
                    return self._send(404, '{"error":"mapping not enabled"}')
                if path == "/api/v1/map":
                    return self._send(200, json.dumps(ms.summary()))
                if path == "/api/v1/map/radio":
                    key = (parse_qs(urlparse(self.path).query).get("key") or [""])[0][:80]
                    return self._send(200, json.dumps({"key": key, "cell_m": 0.5, "cells": ms.radio(key)}))
            if path == "/api/v1/state":
                return self._send(200, json.dumps(runtime.state))
            if path == "/api/v1/history":
                return self._send(200, json.dumps(list(runtime.history)))
            if path == "/events":
                return self._sse()
            self._send(404, '{"error":"not found"}')

        def _sse(self):
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Cache-Control", "no-store")
            self.end_headers()
            seq = -1
            try:
                while True:
                    seq2, st = runtime.wait_state(seq, 15)
                    if seq2 == seq:
                        self.wfile.write(b": keepalive\n\n")
                    else:
                        seq = seq2
                        self.wfile.write(("data: %s\n\n" % json.dumps(st)).encode())
                    self.wfile.flush()
            except (BrokenPipeError, ConnectionResetError, OSError):
                return

        def do_POST(self):
            if not self._host_ok():
                return self._send(403, '{"error":"bad host"}')
            if not self._token_ok():
                return self._send(401, '{"error":"token required"}')
            path = urlparse(self.path).path
            try:
                n = max(0, min(int(self.headers.get("Content-Length") or 0), MAX_BODY.get(path, 4096)))
                body = json.loads(self.rfile.read(n) or b"{}")
                if not isinstance(body, dict):
                    raise ValueError
            except ValueError:
                return self._send(400, '{"error":"bad json"}')
            if path == "/api/v1/calibrate":
                try:
                    secs = max(10, min(600, int(body.get("seconds", 60))))
                except (TypeError, ValueError, OverflowError):
                    return self._send(400, '{"error":"seconds must be a number"}')
                runtime.calibrate(secs)
                return self._send(202, json.dumps({"calibrating_s": secs}))
            ms = getattr(runtime, "mapstore", None)
            if path.startswith("/api/v1/map/"):
                if ms is None:
                    return self._send(404, '{"error":"mapping not enabled"}')
                try:
                    if path == "/api/v1/map/pose":
                        rows = body.get("poses")
                        if not isinstance(rows, list):
                            raise ValueError("poses must be a list")
                        return self._send(200, json.dumps({"added": ms.add_poses(rows)}))
                    if path == "/api/v1/map/voxels":
                        rows = body.get("voxels")
                        if not isinstance(rows, list):
                            raise ValueError("voxels must be a list")
                        return self._send(200, json.dumps({"added": ms.add_voxels(rows)}))
                    if path == "/api/v1/map/object":
                        return self._send(200, json.dumps(ms.add_object(body)))
                    if path == "/api/v1/map/object/delete":
                        return self._send(200, json.dumps({"deleted": ms.delete_object(body.get("id"))}))
                    if path == "/api/v1/map/planes":
                        planes = body.get("planes")
                        if not isinstance(planes, list):
                            raise ValueError("planes must be a list")
                        return self._send(200, json.dumps({"stored": ms.set_planes(planes)}))
                    if path == "/api/v1/map/geo":
                        return self._send(200, json.dumps(ms.set_geo(body)))
                    if path == "/api/v1/map/reset":
                        ms.reset()
                        return self._send(200, '{"reset":true}')
                except (KeyError, TypeError, ValueError) as e:
                    return self._send(400, json.dumps({"error": str(e)[:200]}))
            self._send(404, '{"error":"not found"}')

    srv = ThreadingHTTPServer((host, port), H)
    srv.daemon_threads = True
    return srv


def serve_in_thread(srv):
    t = threading.Thread(target=srv.serve_forever, name="http", daemon=True)
    t.start()
    return t
