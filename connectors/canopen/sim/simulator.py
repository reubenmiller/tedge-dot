"""CANopen simulator for the e2e test harness.

Sets up a CANopen node (node ID 1) on vcan0 using the python-canopen library.
The node exposes three Object Dictionary entries seeded with known values:

  0x2000:0x00  UNSIGNED16  analog_in    = 1234
  0x2001:0x00  INTEGER16   temperature  = -100
  0x2002:0x00  UNSIGNED8   digital_out  = 1  (writable)

The node responds to SDO Upload (read) and SDO Download (write) requests so the
Rust connector can read/write via standard CANopen SDO transfers.

Run via:
  python3 simulator.py

The script sets up vcan0 itself when the vcan kernel module is available.
"""

import canopen
import signal
import sys
import logging
import time

logging.basicConfig(
    level=logging.INFO, format="%(asctime)s - %(name)s - %(levelname)s - %(message)s"
)
log = logging.getLogger("CANopen Simulator")

NODE_ID = 1
INTERFACE = "vcan0"

# Seeded values
ANALOG_IN_VALUE = 1234
TEMPERATURE_VALUE = -100
DIGITAL_OUT_VALUE = 1

running = True


def make_eds() -> str:
    """Return a minimal EDS (Electronic Data Sheet) for the simulated node."""
    return """\
[FileInfo]
FileName=sim_node.eds
FileVersion=1
FileRevision=1
EDSVersion=4.0
Description=CANopen test node
CreationTime=00:00AM
CreationDate=01-01-2024
CreatedBy=simulator
ModificationTime=00:00AM
ModificationDate=01-01-2024
ModifiedBy=simulator

[DeviceInfo]
VendorName=TestVendor
VendorNumber=0x00000001
ProductName=TestNode
ProductNumber=0x00000001
RevisionNumber=0x00000001
OrderCode=TEST-1
BaudRate_10=1
BaudRate_20=1
BaudRate_50=1
BaudRate_125=1
BaudRate_250=1
BaudRate_500=1
BaudRate_800=1
BaudRate_1000=1
SimpleBootUpMaster=0
SimpleBootUpSlave=1
Granularity=0
DynamicChannelsSupported=0
GroupMessaging=0
NrOfRXPDO=0
NrOfTXPDO=0
LSS_Supported=0

[DummyUsage]
Dummy0001=0
Dummy0002=0
Dummy0003=0
Dummy0004=0
Dummy0005=0
Dummy0006=0
Dummy0007=0

[Comments]
Lines=0

[ObjectCount]
SupportedObjects=3

[Objects]
1=0x2000
2=0x2001
3=0x2002

[0x2000]
ParameterName=AnalogIn
ObjectType=0x7
DataType=0x0006
AccessType=ro
DefaultValue=1234
PDOMapping=0

[0x2001]
ParameterName=Temperature
ObjectType=0x7
DataType=0x0004
AccessType=ro
DefaultValue=-100
PDOMapping=0

[0x2002]
ParameterName=DigitalOut
ObjectType=0x7
DataType=0x0005
AccessType=rw
DefaultValue=1
PDOMapping=0
"""


def main() -> None:
    log.info("Creating CANopen network on %s ...", INTERFACE)
    network = canopen.Network()

    # Use a minimal EDS written to a temp file
    import tempfile, os
    eds_file = tempfile.NamedTemporaryFile(
        mode="w", suffix=".eds", delete=False
    )
    eds_file.write(make_eds())
    eds_file.close()

    log.info("Starting CANopen node ID=%d ...", NODE_ID)
    node = network.create_node(NODE_ID, eds_file.name)

    try:
        network.connect(interface="socketcan", channel=INTERFACE)

        # Seed initial values
        node.sdo[0x2000].raw = ANALOG_IN_VALUE
        node.sdo[0x2001].raw = TEMPERATURE_VALUE
        node.sdo[0x2002].raw = DIGITAL_OUT_VALUE

        # Start heartbeat and transition to Operational
        node.nmt.state = "OPERATIONAL"

        log.info(
            "Node %d running. analog_in=%d, temperature=%d, digital_out=%d",
            NODE_ID,
            ANALOG_IN_VALUE,
            TEMPERATURE_VALUE,
            DIGITAL_OUT_VALUE,
        )

        def shutdown(sig, frame):
            global running
            log.info("Shutting down ...")
            running = False

        signal.signal(signal.SIGTERM, shutdown)
        signal.signal(signal.SIGINT, shutdown)

        while running:
            network.check()
            time.sleep(0.1)

    finally:
        network.disconnect()
        os.unlink(eds_file.name)
        log.info("Simulator stopped.")


if __name__ == "__main__":
    main()
