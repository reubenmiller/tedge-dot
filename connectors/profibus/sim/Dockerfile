# PROFIBUS-DP slave simulator container.
#
# The Python DP-V0 slave needs a serial device, and PTYs cannot cross the
# container boundary — so the slave's serial line is exposed as a TCP byte
# stream on port 9200 instead:
#
#   socat builds a persistent PTY pair    ttySlave <-> ttyBridge
#   the slave simulator opens ttySlave
#   a second socat bridges ttyBridge <-> TCP-LISTEN:9200 (auto-reconnecting)
#
# The connector speaks serial-over-TCP natively (port = "tcp://<host>:9200"),
# so nothing else is needed on the client side.

FROM python:3.11-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends socat \
    && rm -rf /var/lib/apt/lists/*

COPY connectors/profibus/sim/requirements.txt /tmp/requirements.txt
RUN pip install --no-cache-dir -r /tmp/requirements.txt

COPY connectors/profibus/sim/slave_sim.py /slave_sim.py

EXPOSE 9200

CMD ["/bin/sh", "-c", "\
    set -e && \
    mkdir -p /tmp/profibus && \
    socat pty,rawer,echo=0,b19200,link=/tmp/profibus/ttySlave \
          pty,rawer,echo=0,b19200,link=/tmp/profibus/ttyBridge & \
    sleep 1 && \
    ( while true; do \
        socat FILE:/tmp/profibus/ttyBridge,rawer,b19200 \
              TCP-LISTEN:9200,reuseaddr,nodelay 2>/dev/null || true; \
        sleep 1; \
      done ) & \
    echo 'profibus-sim: slave on ttySlave, serial-over-tcp bridge on :9200' && \
    exec python3 /slave_sim.py \
        --port /tmp/profibus/ttySlave \
        --address 7 --baudrate 19200 \
        --input-bytes 8 --output-bytes 4 \
"]
