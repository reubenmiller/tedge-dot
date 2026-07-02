#!/usr/bin/env bash
# Offline unit test for the Cloud Fieldbus -> define-device translation
# (operations/c8y-fieldbus-import --translate). No broker, no device, no cloud — it feeds a
# canned c8y_ModbusDeviceType managed object through the jq translation and asserts the
# emitted contract `device` object. Requires bash + jq (same as the shim itself).
#
# Run directly:  ./cloud/modbus/tests/test_fieldbus_import.sh
set -uo pipefail

cd "$(dirname "$0")/../../.." || exit 1
SHIM=./operations/c8y-fieldbus-import

pass=0
fail=0

# assert <name> <jq-expression that must be true against $DEVICE>
assert() {
  local name="$1" expr="$2"
  if jq -e "$expr" >/dev/null 2>&1 <<<"$DEVICE"; then
    echo "ok   - $name"
    pass=$((pass + 1))
  else
    echo "FAIL - $name"
    echo "       expression: $expr"
    echo "       device:     $DEVICE"
    fail=$((fail + 1))
  fi
}

# Canned c8y_ModbusDeviceType managed object, covering:
#   - a plain uint16 holding register with scaling + measurementMapping (the fieldbus_c8y.robot
#     round-trip register),
#   - a signed 32-bit input register with multiplier and decimal shift ("offset"),
#   - a sub-register bit field (Cloud Fieldbus MSB-based startBit),
#   - an unsupported layout (24 bits, unaligned) that must be skipped, not crash,
#   - a writable coil with measurementMapping and a read-only (input) coil.
TYPE_MO='{
  "id": "321",
  "name": "tedge-dot-sim-type",
  "type": "c8y_ModbusDeviceType",
  "c8y_IsDeviceType": {},
  "c8y_ModbusDeviceType": { "protocol": "TCP" },
  "c8y_Registers": [
    { "number": 3, "startBit": 0, "noBits": 16, "signed": false,
      "multiplier": 1, "divisor": 1000, "offset": 0, "input": false,
      "name": "temperature", "unit": "°C",
      "measurementMapping": { "type": "modbus", "series": "temperature" } },
    { "number": 4, "startBit": 0, "noBits": 32, "signed": true,
      "multiplier": 3, "divisor": 1, "offset": 2, "input": true,
      "name": "energy", "unit": "kWh" },
    { "number": 10, "startBit": 12, "noBits": 4, "signed": false,
      "multiplier": 1, "divisor": 1, "offset": 0, "input": false,
      "name": "status_bits" },
    { "number": 20, "startBit": 8, "noBits": 24, "signed": false, "input": false,
      "name": "unsupported_24bit" }
  ],
  "c8y_Coils": [
    { "number": 48, "input": false, "name": "pump_run",
      "measurementMapping": { "type": "modbus", "series": "pump" } },
    { "number": 49, "input": true, "name": "door_open" }
  ]
}'

DEVICE="$(printf '%s' "$TYPE_MO" | "$SHIM" --translate fieldbus1 TCP simulator 1 2>stderr.tmp)"
STDERR="$(cat stderr.tmp)"
rm -f stderr.tmp

# --- device shell -------------------------------------------------------------------------
assert "device name from the operation payload" '.name == "fieldbus1"'
assert "tcp protocol_address from protocol/ipAddress/address" \
  '.protocol_address == {transport: "tcp", host: "simulator", port: 502, unit_id: 1}'
assert "default_mode is typed" '.default_mode == "typed"'
assert "unsupported register skipped, everything else kept (3 regs + 2 coils)" \
  '.point | length == 5'
assert "no point emitted for the unsupported layout" \
  '[.point[].id] | index("unsupported_24bit") == null'

# --- plain uint16 holding register --------------------------------------------------------
assert "register name -> point id" '.point[0].id == "temperature"'
assert "16-bit unsigned -> uint16" '.point[0].datatype == "uint16"'
assert "number/input -> holding table address" \
  '.point[0].address == {table: "holding", address: 3, count: 1}'
assert "holding register is writable (c8y_SetRegister parity)" \
  '.point[0].access == "read_write"'
assert "unit carried over" '.point[0].unit == "°C"'
assert "divisor-only scaling -> transform.divisor" '.point[0].transform == {divisor: 1000}'
assert "measurementMapping -> meta.measurement group/series" \
  '.point[0].meta == {measurement: {group: "modbus", series: "temperature"}}'

# --- signed 32-bit input register with scaling --------------------------------------------
assert "32-bit signed -> int32 over two registers" \
  '.point[1].datatype == "int32" and .point[1].address.count == 2'
assert "input flag -> input register table" '.point[1].address.table == "input"'
assert "input register is not writable" '.point[1] | has("access") | not'
assert "c8y offset is a decimal shift, not transform.offset (legacy 10^offset semantics)" \
  '.point[1].transform == {multiplier: 3, decimal_shift: 2}'
assert "no measurementMapping -> no meta" '.point[1] | has("meta") | not'

# --- sub-register bit field ----------------------------------------------------------------
assert "bit field -> start_bit/bit_count (MSB-based startBit converted to LSB)" \
  '.point[2].address == {table: "holding", address: 10, count: 1, start_bit: 0, bit_count: 4}'
assert "bit field decodes via uint16 register" '.point[2].datatype == "uint16"'

# --- coils ----------------------------------------------------------------------------------
assert "writable coil -> coil table, bool, read_write" \
  '.point[3] == {id: "pump_run", datatype: "bool",
                 address: {table: "coil", address: 48, count: 1}, access: "read_write",
                 meta: {measurement: {group: "modbus", series: "pump"}}}'
assert "input coil -> discrete_input, read-only" \
  '.point[4] == {id: "door_open", datatype: "bool",
                 address: {table: "discrete_input", address: 49, count: 1}}'

# --- warnings -------------------------------------------------------------------------------
if [[ "$STDERR" == *"unsupported_24bit"* ]]; then
  echo "ok   - skipped register is reported on stderr"
  pass=$((pass + 1))
else
  echo "FAIL - skipped register is reported on stderr"
  echo "       stderr: $STDERR"
  fail=$((fail + 1))
fi

# --- RTU fallback ----------------------------------------------------------------------------
DEVICE="$(printf '%s' '{"c8y_Registers": []}' |
  FIELDBUS_SERIAL_PORT=/dev/ttyS1 "$SHIM" --translate plc7 RTU '' 7 2>/dev/null)"
assert "non-TCP protocol -> rtu protocol_address with serial port + unit id" \
  '.protocol_address == {transport: "rtu", serial_port: "/dev/ttyS1", unit_id: 7}'
assert "empty type -> device with no points (still a valid define-device)" '.point == []'

echo
echo "fieldbus import translation: $pass passed, $fail failed"
[[ $fail -eq 0 ]]
