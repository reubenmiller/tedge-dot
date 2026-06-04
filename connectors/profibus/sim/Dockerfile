# PROFIBUS simulator container:
#   1. Creates a virtual serial port pair with socat (ttyPROFIBUS0 / ttyPROFIBUS1)
#   2. Runs the Python DP-V0 slave simulator on ttyPROFIBUS1
#
# ttyPROFIBUS0 is used by the Rust connector container (shared via /tmp/profibus/).
# Both containers mount the same named volume so the socat pseudo-ttys are visible
# to the connector.

FROM python:3.11-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends socat \
    && rm -rf /var/lib/apt/lists/*

COPY connectors/profibus/sim/requirements.txt /tmp/requirements.txt
RUN pip install --no-cache-dir -r /tmp/requirements.txt

COPY connectors/profibus/sim/slave_sim.py /slave_sim.py

# Create the virtual serial pair, then start the slave simulator.
# socat creates symlinks /tmp/profibus/ttyPROFIBUS0 and ttyPROFIBUS1.
CMD ["/bin/sh", "-c", "\
    mkdir -p /tmp/profibus && \
    socat -d -d \
        pty,rawer,echo=0,link=/tmp/profibus/ttyPROFIBUS0 \
        pty,rawer,echo=0,link=/tmp/profibus/ttyPROFIBUS1 & \
    SOCAT_PID=$! && \
    sleep 1 && \
    echo 'socat ptys ready' && \
    python3 /slave_sim.py --port /tmp/profibus/ttyPROFIBUS1 \
        --address 7 --baudrate 19200 --input-bytes 8 --output-bytes 4 ; \
    kill $SOCAT_PID \
"]
