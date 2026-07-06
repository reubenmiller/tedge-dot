"""SAE J1939 simulator for the e2e test harness.

Broadcasts periodic J1939 frames (extended 29-bit IDs) on a virtual CAN
interface (vcan0), matching the signals declared in j1939.dbc and read by the
demo config (demo/config/j1939.toml):

  EEC1 (PGN 61444, id 0x0CF00400, SA 0x00):
    EngineSpeed  u16 Intel bits 24-39  raw 8000  → ×0.125 = 1000 rpm
  ET1  (PGN 65262, id 0x18FEEE00, SA 0x00):
    EngineCoolantTemperature u8 byte 0  raw 125  → (−40 offset) = 85 °C

Run via:  python3 simulator.py
The container sets up vcan0 itself when the vcan kernel module is available.
"""

import logging
import signal
import time

import can

logging.basicConfig(
    level=logging.INFO, format="%(asctime)s - %(name)s - %(levelname)s - %(message)s"
)
log = logging.getLogger("J1939 Simulator")

EEC1_ID = 0x0CF00400  # PGN 61444, source address 0x00
ET1_ID = 0x18FEEE00  # PGN 65262, source address 0x00
DM1_ID = 0x18FECA00  # PGN 65226, source address 0x00 (active diagnostics)
INTERVAL_S = 0.1  # 10 Hz

running = True


def make_dm1(spn: int, fmi: int, occurrence: int) -> bytes:
    """DM1 single-frame, one active DTC. Bytes 0-1 = lamp status (amber warning
    on here), bytes 2-5 = the DTC (J1939-73 packing), 6-7 reserved."""
    data = bytearray(8)
    data[0] = 0x04  # SAE amber warning lamp on
    data[1] = 0x00
    data[2] = spn & 0xFF
    data[3] = (spn >> 8) & 0xFF
    data[4] = ((spn >> 11) & 0xE0) | (fmi & 0x1F)
    data[5] = occurrence & 0x7F  # SPN conversion method 0
    data[6] = 0xFF
    data[7] = 0xFF
    return bytes(data)


def make_eec1(engine_speed_raw: int) -> bytes:
    """EEC1: EngineSpeed (SPN 190) u16 little-endian at bytes 4-5 (0-indexed 3-4)."""
    data = bytearray(b"\xff" * 8)  # 0xFF = "not available" per J1939
    data[3] = engine_speed_raw & 0xFF
    data[4] = (engine_speed_raw >> 8) & 0xFF
    return bytes(data)


def make_et1(coolant_raw: int) -> bytes:
    """ET1: EngineCoolantTemperature (SPN 110) u8 at byte 1 (0-indexed 0)."""
    data = bytearray(b"\xff" * 8)
    data[0] = coolant_raw & 0xFF
    return bytes(data)


def main() -> None:
    log.info("Opening socketcan channel vcan0 ...")
    bus = can.Bus(interface="socketcan", channel="vcan0", receive_own_messages=False)

    def shutdown(sig, frame):
        global running
        log.info("Shutting down ...")
        running = False

    signal.signal(signal.SIGTERM, shutdown)
    signal.signal(signal.SIGINT, shutdown)

    eec1 = make_eec1(engine_speed_raw=8000)  # 1000 rpm
    et1 = make_et1(coolant_raw=125)  # 85 °C after -40 offset
    dm1 = make_dm1(spn=100, fmi=1, occurrence=2)  # e.g. low oil pressure, 2 occurrences
    log.info("Broadcasting EEC1 (61444) + ET1 (65262) + DM1 (65226) at %.0f Hz", 1 / INTERVAL_S)
    while running:
        for arb_id, payload in ((EEC1_ID, eec1), (ET1_ID, et1), (DM1_ID, dm1)):
            try:
                bus.send(can.Message(arbitration_id=arb_id, data=payload, is_extended_id=True))
            except can.CanError as exc:
                log.error("CAN send error: %s", exc)
        time.sleep(INTERVAL_S)

    bus.shutdown()
    log.info("Simulator stopped.")


if __name__ == "__main__":
    main()
