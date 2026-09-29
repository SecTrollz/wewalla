"""wewalla-termux: WiFi sensing that runs on stock Termux (no root, no ESP32).

Sources
  * connector  - our Android connector APK (foreground service: RSSI / scan / RTT / IMU)
  * termux     - Termux:API polling (no APK needed; slower)
  * adr018     - real CSI from any external hardware speaking ADR-018 over UDP
  * sim/replay - synthetic / recorded data for testing

Honesty rule (mirrors the repo's CLAUDE.md): data from connector/termux sources is
DERIVED (RSSI / RTT), never CSI, and is labelled as such everywhere it is emitted.
"""
__version__ = "0.1.0"
