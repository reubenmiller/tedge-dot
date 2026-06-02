#!/bin/sh
# Wait for the simulator and broker to be reachable, then start the connector.
# The connector connects to Modbus devices once at startup, so the simulator must
# be accepting connections before it launches.
set -e

echo "waiting for simulator:502 ..."
until nc -z simulator 502; do sleep 1; done

echo "waiting for broker:1883 ..."
until nc -z broker 1883; do sleep 1; done

echo "starting tedge-dot"
exec /usr/bin/tedge-dot /etc/connector.toml
