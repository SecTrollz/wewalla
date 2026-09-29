"""ADR-018 CSI frame codec (wire-compatible with the RuView Rust parser).

Offset Size Field
0      4    magic 0xC5110001 (LE)
4      1    node id
5      1    antennas (1..4)
6      2    subcarriers (LE u16, <=256)
8      4    frequency MHz (LE u32)
12     4    sequence (LE u32)
16     1    RSSI (i8)
17     1    noise floor (i8)
18     1    PPDU type
19     1    flags  (bit0 bw40, bit2 STBC, bit3 LDPC, bit4 15.4 sync, bit5 sanitized)
20     N    I/Q int8 pairs, antenna-major

We additionally use flags bit7 (0x80) = "derived, NOT real CSI". The upstream
parser ignores unknown flag bits, so this is backward compatible.
"""
import struct
from dataclasses import dataclass, field
from typing import List, Optional

MAGIC = 0xC5110001
SIBLING_MAGICS = {0xC5110000 + n for n in range(2, 8)}
HEADER = struct.Struct("<IBBHIIbbBB")
assert HEADER.size == 20
MAX_SUBCARRIERS = 256
MAX_ANTENNAS = 4
FLAG_DERIVED = 0x80


@dataclass
class Frame:
    node_id: int
    n_ant: int
    n_sc: int
    freq_mhz: int
    seq: int
    rssi: int
    noise: int
    ppdu: int = 0
    flags: int = 0
    iq: List[int] = field(default_factory=list)  # flat [i0,q0,i1,q1,...]

    @property
    def derived(self) -> bool:
        return bool(self.flags & FLAG_DERIVED)

    def amplitudes(self):
        """Yield (antenna, subcarrier, amplitude)."""
        for a in range(self.n_ant):
            for s in range(self.n_sc):
                k = (a * self.n_sc + s) * 2
                i, q = self.iq[k], self.iq[k + 1]
                yield a, s, (i * i + q * q) ** 0.5


def _i8(v) -> int:
    return max(-128, min(127, int(round(v))))


def encode(node_id, n_ant, n_sc, freq_mhz, seq, rssi, noise, iq, ppdu=0, flags=0) -> bytes:
    if not 1 <= n_ant <= MAX_ANTENNAS:
        raise ValueError("antenna count out of range")
    if not 0 <= n_sc <= MAX_SUBCARRIERS:
        raise ValueError("subcarrier count out of range")
    if len(iq) != n_ant * n_sc * 2:
        raise ValueError("iq length mismatch")
    head = HEADER.pack(MAGIC, node_id & 0xFF, n_ant, n_sc, int(freq_mhz) & 0xFFFFFFFF,
                       int(seq) & 0xFFFFFFFF, _i8(rssi), _i8(noise), ppdu & 0xFF, flags & 0xFF)
    return head + bytes((_i8(v) & 0xFF) for v in iq)


def decode(buf: bytes) -> Optional[Frame]:
    """Return a Frame, or None for sibling RuView packets. Raises ValueError on garbage."""
    if len(buf) < 4:
        raise ValueError("short packet")
    magic = struct.unpack_from("<I", buf)[0]
    if magic in SIBLING_MAGICS:
        return None
    if magic != MAGIC:
        raise ValueError("bad magic 0x%08X" % magic)
    if len(buf) < HEADER.size:
        raise ValueError("short header")
    _, node, n_ant, n_sc, freq, seq, rssi, noise, ppdu, flags = HEADER.unpack_from(buf)
    if not 1 <= n_ant <= MAX_ANTENNAS or n_sc > MAX_SUBCARRIERS:
        raise ValueError("invalid antenna/subcarrier count")
    need = HEADER.size + n_ant * n_sc * 2
    if len(buf) < need:
        raise ValueError("truncated iq data")
    iq = [b - 256 if b > 127 else b for b in buf[HEADER.size:need]]
    return Frame(node, n_ant, n_sc, freq, seq, rssi, noise, ppdu, flags, iq)
