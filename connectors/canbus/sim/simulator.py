"""CAN bus simulator for the e2e test harness.

Sends periodic CAN frames on a virtual CAN interface (vcan0) to simulate a
real CAN bus device.  The frames match the signals declared in test.dbc:

  ENGINE_STATUS (ID 0x1A0 = 416):
    RPM          u16 Intel  bits  0-15   = 2500
    COOLANT_TEMP i8  Intel  bits 16-23   = 85
    BRAKE_ACTIVE u1  Intel  bit  24      = 1

Byte layout (little-endian):
  byte 0 = 0xC4  (RPM low byte)
  byte 1 = 0x09  (RPM high byte)
  byte 2 = 0x55  (COOLANT_TEMP = 85)
  byte 3 = 0x01  (BRAKE_ACTIVE = 1, remaining bits = 0)
  bytes 4-7 = 0x00

Run via:
  python3 simulator.py

The script sets up vcan0 itself when the vcan kernel module is available.
"""

import can
import signal
import sys
import logging
import time

logging.basicConfig(
    level=logging.INFO, format="%(asctime)s - %(name)s - %(levelname)s - %(message)s"
)
log = logging.getLogger("CAN Simulator")

ENGINE_STATUS_ID = 0x1A0  # decimal 416
INTERVAL_S = 0.1  # 10 Hz

running = True


def make_engine_status(rpm: int, coolant_temp: int, brake_active: bool) -> bytes:
    """Pack ENGINE_STATUS payload using Intel (little-endian) byte order."""
    data = bytearray(8)
    # RPM: u16 @ bits 0-15 (bytes 0-1, LE)
    data[0] = rpm & 0xFF
    data[1] = (rpm >> 8) & 0xFF
    # COOLANT_TEMP: i8 @ bits 16-23 (byte 2, two's-complement)
    data[2] = coolant_temp & 0xFF
    # BRAKE_ACTIVE: u1 @ bit 24 (byte 3, bit 0)
    data[3] = 0x01 if brake_active else 0x00
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

    log.info(
        "Sending ENGINE_STATUS (ID=0x%03X) at %.0f Hz", ENGINE_STATUS_ID, 1 / INTERVAL_S
    )
    while running:
        payload = make_engine_status(rpm=2500, coolant_temp=85, brake_active=True)
        msg = can.Message(
            arbitration_id=ENGINE_STATUS_ID, data=payload, is_extended_id=False
        )
        try:
            bus.send(msg)
        except can.CanError as exc:
            log.error("CAN send error: %s", exc)
        time.sleep(INTERVAL_S)

    bus.shutdown()
    log.info("Simulator stopped.")


if __name__ == "__main__":
    main()
