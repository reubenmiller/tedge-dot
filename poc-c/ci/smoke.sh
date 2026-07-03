#!/usr/bin/env bash
# End-to-end smoke test for the C PoC connector against a protocol simulator
# and a live MQTT broker.
#
#   poc-c/ci/smoke.sh <modbus|opcua|canbus|canopen|profibus>
#
# Expects (CI provides all of these):
#   * the protocol simulator running (docker compose -f
#     connectors/<proto>/docker-compose.yaml up -d simulator, plus vcan0 for
#     the SocketCAN protocols);
#   * an MQTT broker on 127.0.0.1:1883;
#   * poc-c/build/tedge-dot built;
#   * mosquitto_sub + jq on PATH.
#
# Asserts that a known seeded point arrives with quality "good" and the
# expected value, and (where the demo config has a writable point) that an
# MQTT write command round-trips with status "successful".
set -euo pipefail

proto=${1:?usage: smoke.sh <protocol>}
repo=$(cd "$(dirname "$0")/../.." && pwd)
bin="$repo/poc-c/build/tedge-dot"
workdir=$(mktemp -d)
connector_pid=""
trap 'kill $connector_pid 2>/dev/null || true; rm -rf "$workdir"' EXIT

config="$workdir/$proto.toml"
cp "$repo/demo/config/$proto.toml" "$config"

# Per-protocol: expected point + value, optional write point/value, config fixups.
write_point="" write_value="" write_device=""
case "$proto" in
modbus)
    point=temp_u16 expect=17001 device=plc1
    write_point=coil_rw write_value=true write_device=plc1
    ;;
opcua)
    point=temperature expect=21.5 device=opc1
    write_point=setpoint write_value=41 write_device=opc1
    ;;
canbus)
    point=rpm expect=2500 device=engine
    # demo config points at the installed demo path; use the repo copy
    sed -i.bak "s|/usr/share/tedge-dot/demo/can/test.dbc|$repo/connectors/canbus/sim/test.dbc|" "$config"
    ;;
canopen)
    point=analog_in expect=1234 device=plc1
    write_point=digital_out write_value=1 write_device=plc1
    ;;
profibus)
    point=ai0_raw expect=4660 device=remote_io
    write_point=do_byte0 write_value=5 write_device=remote_io
    ;;
*)
    echo "unknown protocol: $proto" >&2
    exit 2
    ;;
esac

echo "== $proto: starting connector (30s window)"
"$bin" run "$config" --duration 30s 2>"$workdir/connector.log" &
connector_pid=$!

echo "== waiting for a good sample on te/device/$device/ot/$proto/sample/$point"
sample=$(mosquitto_sub -h 127.0.0.1 -W 25 -C 1 \
    -t "te/device/$device/ot/$proto/sample/$point" || true)
if [ -z "$sample" ]; then
    echo "FAIL: no sample received; connector log:" >&2
    cat "$workdir/connector.log" >&2
    exit 1
fi
echo "sample: $sample"
quality=$(jq -r .quality <<<"$sample")
value=$(jq -r .value <<<"$sample")
if [ "$quality" != "good" ] || [ "$value" != "$expect" ]; then
    echo "FAIL: expected quality=good value=$expect, got quality=$quality value=$value" >&2
    exit 1
fi
echo "OK: $point = $value (good)"

if [ -n "$write_point" ]; then
    cmd_topic="te/device/$write_device/ot/$proto/cmd/write/smoke-1"
    echo "== write round-trip on $cmd_topic"
    mosquitto_pub -h 127.0.0.1 -t "$cmd_topic" -r \
        -m "{\"status\":\"init\",\"point\":\"$write_point\",\"value\":$write_value}"
    result=$(mosquitto_sub -h 127.0.0.1 -W 15 -t "$cmd_topic" | \
        jq -c --unbuffered 'select(.status != "init")' | head -1 || true)
    echo "result: $result"
    if [ "$(jq -r .status <<<"$result")" != "successful" ]; then
        echo "FAIL: write command did not succeed; connector log:" >&2
        cat "$workdir/connector.log" >&2
        exit 1
    fi
    echo "OK: write $write_point = $write_value successful"
    # clear the retained command so reruns start clean
    mosquitto_pub -h 127.0.0.1 -t "$cmd_topic" -r -n
fi

echo "== $proto smoke passed"
