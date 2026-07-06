# Build and run the J1939 simulator inside a Docker container.
# Build context is the tedge-dot/ project root (see docker-compose.yaml).
#
# Needs CAP_NET_ADMIN (via docker-compose.yaml) and network_mode: host so it can
# create vcan0 in the kernel's default network namespace — the same one the
# connector (poc-c/build/tedge-dot, run by poc-c/ci/smoke.sh) accesses.

FROM python:3.11-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends iproute2 can-utils kmod \
    && rm -rf /var/lib/apt/lists/*

COPY connectors/j1939/sim/requirements.txt /tmp/requirements.txt
RUN pip install --no-cache-dir -r /tmp/requirements.txt

COPY connectors/j1939/sim/simulator.py /simulator.py

# 1. Load the vcan kernel module (may already be loaded on the host).
# 2. Create vcan0 (idempotent — ignore EEXIST).
# 3. Bring the interface up.
# 4. Start the simulator.
CMD ["sh", "-c", \
  "modprobe vcan 2>/dev/null || true \
   && ip link add vcan0 type vcan 2>/dev/null || true \
   && ip link set up vcan0 2>/dev/null || true \
   && python3 /simulator.py"]
