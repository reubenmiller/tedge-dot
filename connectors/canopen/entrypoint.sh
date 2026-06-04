#!/bin/sh
set -e

# Load the vcan kernel module (idempotent on failure — may already be loaded).
modprobe vcan || true

# Create vcan0 if it does not already exist (the simulator may have done this).
ip link add dev vcan0 type vcan 2>/dev/null || true
ip link set up vcan0

echo "vcan0 is up"

# Wait for the MQTT broker to be reachable.
echo "Waiting for broker at localhost:13883 ..."
until nc -z localhost 13883; do
    sleep 0.5
done
echo "Broker is ready"

# Start the connector.
exec tedge-dot run /etc/connector.toml
