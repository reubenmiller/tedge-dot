# Build the CANopen simulator container.
# Sets up vcan0, installs python-canopen, and runs the SDO server node.

FROM python:3.11-slim-bookworm

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        can-utils \
        iproute2 \
        kmod \
    && rm -rf /var/lib/apt/lists/*

COPY connectors/canopen/sim/requirements.txt /requirements.txt
RUN pip install --no-cache-dir -r /requirements.txt

COPY connectors/canopen/sim/simulator.py /simulator.py

# Create vcan0, load the vcan module, and start the simulator.
CMD set -e; \
    modprobe vcan || true; \
    ip link add dev vcan0 type vcan 2>/dev/null || true; \
    ip link set up vcan0; \
    echo "vcan0 is up"; \
    python3 /simulator.py
