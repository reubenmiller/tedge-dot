#!/bin/sh
# Wait for the MQTT broker then start the connector.
# vcan0 must already be up (created by the simulator container which starts first).
set -e

# Ensure vcan0 exists (the simulator may have created it in the host netns).
# If already present the ip commands are idempotent.
modprobe vcan 2>/dev/null || true
ip link add vcan0 type vcan 2>/dev/null || true
ip link set up vcan0 2>/dev/null || true

echo "waiting for broker:13883 ..."
until nc -z localhost 13883; do sleep 1; done

echo "starting tedge-dot"
exec /usr/bin/tedge-dot /etc/connector.toml
