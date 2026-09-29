import json
import os
import secrets
from pathlib import Path


def home() -> Path:
    p = Path(os.environ.get("WEWALLA_HOME") or Path.home() / ".wewalla")
    p.mkdir(parents=True, exist_ok=True)
    return p


def token(create=True):
    f = home() / "token"
    if f.exists():
        return f.read_text().strip()
    if not create:
        return None
    t = secrets.token_urlsafe(9)
    fd = os.open(str(f), os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(fd, "w") as fh:
        fh.write(t + "\n")
    return t


def load_calibration():
    f = home() / "calibration.json"
    try:
        return json.loads(f.read_text())
    except (OSError, ValueError):
        return None


def save_calibration(cal):
    f = home() / "calibration.json"
    tmp = f.with_suffix(".tmp")
    tmp.write_text(json.dumps(cal, indent=1))
    tmp.replace(f)
    return f
