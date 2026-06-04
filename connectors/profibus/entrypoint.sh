#!/bin/sh
# Wait for the PROFIBUS slave simulator and MQTT broker to be reachable,
# then start the connector.
#
# The slave simulator exposes a virtual serial pty at /tmp/profibus/ttyPROFIBUS0
# which is shared via the "profibus-ptys" Docker volume.
set -e

SERIAL_PTY="/tmp/profibus/ttyPROFIBUS0"

echo "waiting for serial pty ${SERIAL_PTY} ..."
until [ -e "${SERIAL_PTY}" ]; do sleep 1; done

echo "waiting for broker:1883 ..."
until nc -z broker 1883; do sleep 1; done

echo "starting tedge-dot (profibus)"
exec /usr/bin/tedge-dot /etc/connector.toml
