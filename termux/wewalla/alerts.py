"""Optional local alerts through Termux:API (notification / vibrate / speech)."""
import shutil
import subprocess
import time


class Alerts:
    def __init__(self, vibrate=True, speak=False, min_gap_s=30.0):
        self.vibrate, self.speak, self.min_gap_s = vibrate, speak, min_gap_s
        self._last = 0.0
        self.sent = []  # for tests / status

    def _run(self, *cmd):
        if shutil.which(cmd[0]):
            try:
                subprocess.Popen(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            except OSError:
                pass

    def on_state(self, prev, cur):
        p0, p1 = prev.get("presence"), cur.get("presence")
        if p1 is None or p0 == p1 or (p0 is None and p1 is False):
            return
        now = time.time()
        if now - self._last < self.min_gap_s:
            return
        self._last = now
        msg = "Presence detected" if p1 else "Room clear"
        kind = cur.get("quality", {}).get("signal_kind", "")
        self.sent.append(msg)
        self._run("termux-notification", "--id", "wewalla", "--title", "Wewalla", "--content",
                  "%s (%s)" % (msg, kind))
        if self.vibrate and p1:
            self._run("termux-vibrate", "-d", "200")
        if self.speak:
            self._run("termux-tts-speak", msg)
