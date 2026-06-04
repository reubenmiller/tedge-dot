#!/bin/sh
# Combined e2e entrypoint: creates a virtual serial port pair, starts the
# PROFIBUS slave simulator on one end, then starts the connector on the other.
set -e

PTY0="/tmp/ttyPROFIBUS0"
PTY1="/tmp/ttyPROFIBUS1"

echo "creating virtual serial port pair ..."
socat -d -d \
    pty,rawer,echo=0,link=${PTY0} \
    pty,rawer,echo=0,link=${PTY1} &
SOCAT_PID=$!

# Wait for both ptys to appear
until [ -e "${PTY0}" ] && [ -e "${PTY1}" ]; do sleep 0.2; done
echo "ptys ready: ${PTY0} <-> ${PTY1}"

# Start the Python PROFIBUS slave simulator on PTY1 (background)
python3 /slave_sim.py \
    --port "${PTY1}" \
    --address 7 \
    --baudrate 19200 \
    --input-bytes 8 \
    --output-bytes 4 &
SIM_PID=$!
echo "slave simulator started (pid ${SIM_PID})"

# Wait for broker
echo "waiting for broker:1883 ..."
until nc -z broker 1883; do sleep 1; done

# Update the connector config to use the in-container pty path
sed -i "s|/dev/ttyPROFIBUS0|${PTY0}|g" /etc/connector.toml

echo "starting tedge-dot (profibus) on ${PTY0}"
exec /usr/bin/tedge-dot /etc/connector.toml
