#!/bin/sh
# Wait for the OPC-UA simulator and broker to be reachable, then start the connector.
# The connector opens its OPC-UA session at startup, so the simulator must be accepting
# connections before it launches.
set -e

echo "waiting for simulator:4840 ..."
until nc -z simulator 4840; do sleep 1; done

echo "waiting for broker:1883 ..."
until nc -z broker 1883; do sleep 1; done

echo "starting tedge-dot (opcua)"
exec /usr/bin/tedge-dot /etc/connector.toml
