#!/bin/sh
# Wait for the MQTT broker, then start the connector. The simulator's TCP
# endpoint needs no probing here: the connector's tcp:// transport retries the
# initial connect itself (and probing would steal the sim's single-connection
# socat listener).
set -e

echo "waiting for broker:1883 ..."
until nc -z broker 1883; do sleep 1; done

echo "starting tedge-dot (profibus)"
exec /usr/bin/tedge-dot /etc/connector.toml
