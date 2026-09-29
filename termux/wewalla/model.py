from dataclasses import dataclass

# Sample groups. "rssi"/"rtt" are DERIVED signals; only "csi" is real channel state info.
G_RSSI, G_RTT, G_CSI = "rssi", "rtt", "csi"


@dataclass
class Sample:
    t: float            # arrival time, epoch seconds
    key: str            # stable series id, e.g. "scan:aa:bb:cc:dd:ee:ff"
    value: float        # dBm (rssi), millimetres (rtt) or amplitude (csi)
    group: str = G_RSSI
    fresh: bool = True  # False when the source re-delivered a cached value
    freq: int = 0       # MHz, 0 = unknown
