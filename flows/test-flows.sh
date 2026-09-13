#!/usr/bin/env bash
# Validate every flow with `tedge flows test` (offline: no broker, no device, no cloud).
# Each case pipes a sample/measurement/command into a flow and checks the output topic+payload.
set -uo pipefail

cd "$(dirname "$0")"

pass=0
fail=0

# check <name> <flows-dir> <stdin> <expected-substring>
check() {
  local name="$1" dir="$2" input="$3" expect="$4"
  local out
  out="$(printf '%s\n' "$input" | tedge flows test --flows-dir "$dir" 2>/dev/null)"
  if [[ "$out" == *"$expect"* ]]; then
    echo "ok   - $name"
    pass=$((pass + 1))
  else
    echo "FAIL - $name"
    echo "       expected to contain: $expect"
    echo "       got:                 $out"
    fail=$((fail + 1))
  fi
}

# check_absent <name> <flows-dir> <stdin> <must-be-present> <must-be-absent>
# Asserts suppression: the output contains the first substring but NOT the second.
check_absent() {
  local name="$1" dir="$2" input="$3" present="$4" absent="$5"
  local out
  out="$(printf '%s\n' "$input" | tedge flows test --flows-dir "$dir" 2>/dev/null)"
  if [[ "$out" == *"$present"* && "$out" != *"$absent"* ]]; then
    echo "ok   - $name"
    pass=$((pass + 1))
  else
    echo "FAIL - $name"
    echo "       expected to contain: $present"
    echo "       and NOT contain:     $absent"
    echo "       got:                 $out"
    fail=$((fail + 1))
  fi
}

# check_empty <name> <flows-dir> <stdin>
check_empty() {
  local name="$1" dir="$2" input="$3"
  local out
  out="$(printf '%s\n' "$input" | tedge flows test --flows-dir "$dir" 2>/dev/null)"
  if [[ -z "$out" ]]; then
    echo "ok   - $name"
    pass=$((pass + 1))
  else
    echo "FAIL - $name (expected no output)"
    echo "       got: $out"
    fail=$((fail + 1))
  fi
}

# Build a temporary copy of a flow with overridden params so non-default config can be tested.
# Starts from the flow's params.toml.template (so every referenced param stays defined) and
# replaces the given "key = value" override lines. Echoes the temp dir; caller must `rm -rf`.
flow_with_params() {
  local src="$1" overrides="$2" tmp key
  tmp="$(mktemp -d)"
  cp "$src"/*.js "$src"/flow.toml "$tmp"/
  cp "$src/params.toml.template" "$tmp/params.toml"
  while IFS= read -r line; do
    [[ -z "$line" ]] && continue
    key="${line%%=*}"
    key="$(printf '%s' "$key" | tr -d '[:space:]')"
    sed -i.bak "/^[[:space:]]*${key}[[:space:]]*=/d" "$tmp/params.toml" && rm -f "$tmp/params.toml.bak"
    printf '%s\n' "$line" >> "$tmp/params.toml"
  done <<< "$overrides"
  printf '%s' "$tmp"
}

# check_params <name> <flow-src> <params> <stdin> <expected-substring> [extra tedge flags...]
check_params() {
  local name="$1" src="$2" params="$3" input="$4" expect="$5"
  shift 5
  local tmp out
  tmp="$(flow_with_params "$src" "$params")"
  out="$(printf '%s\n' "$input" | tedge flows test --flows-dir "$tmp" "$@" 2>/dev/null)"
  rm -rf "$tmp"
  if [[ "$out" == *"$expect"* ]]; then
    echo "ok   - $name"
    pass=$((pass + 1))
  else
    echo "FAIL - $name"
    echo "       expected to contain: $expect"
    echo "       got:                 $out"
    fail=$((fail + 1))
  fi
}

# check_multi <name> <flows (space-separated)> <stdin> <expected-substring> [--absent <substring>]
# Runs several flows together in one mapper-like flows dir (each with its template params), so
# cross-flow state through context.mapper is exercised the way a deployed mapper runs them.
check_multi() {
  local name="$1" flows="$2" input="$3" expect="$4" absent="${6:-}"
  local tmp out f
  tmp="$(mktemp -d)"
  for f in $flows; do
    mkdir -p "$tmp/$f"
    cp "$f"/*.js "$f"/flow.toml "$tmp/$f/"
    cp "$f/params.toml.template" "$tmp/$f/params.toml"
  done
  out="$(printf '%s\n' "$input" | tedge flows test --flows-dir "$tmp" 2>/dev/null)"
  rm -rf "$tmp"
  if [[ "$out" == *"$expect"* && ( -z "$absent" || "$out" != *"$absent"* ) ]]; then
    echo "ok   - $name"
    pass=$((pass + 1))
  else
    echo "FAIL - $name"
    echo "       expected to contain: $expect"
    [[ -n "$absent" ]] && echo "       and NOT contain:     $absent"
    echo "       got:                 $out"
    fail=$((fail + 1))
  fi
}

S='{"ts":"2026-05-30T10:00:00.000Z","device":"plc1","protocol":"modbus","point":"level_f32","mode":"typed","datatype":"float32","value":404.17,"quality":"good"}'
SBAD='{"ts":"2026-05-30T10:00:00.000Z","device":"plc1","protocol":"modbus","point":"level_f32","mode":"typed","datatype":"float32","quality":"bad","error":"timeout"}'
SBOOL='{"ts":"2026-05-30T10:00:00.000Z","device":"plc1","protocol":"modbus","point":"coil_rw","mode":"typed","datatype":"bool","value":true,"quality":"good"}'
# Same contract envelope from a different protocol: the group is derived from sample.protocol.
SOPCUA='{"ts":"2026-05-30T10:00:00.000Z","device":"opc1","protocol":"opcua","point":"temperature","mode":"typed","datatype":"float32","value":21.5,"quality":"good"}'

# --- ot-measurement (OT sample -> thin-edge measurement) ---
check "measurement: modbus float -> m/modbus" ot-measurement \
  "[te/device/plc1/ot/modbus/sample/level_f32] $S" \
  '[te/device/plc1///m/modbus] {"modbus":{"level_f32":404.17},"time":"2026-05-30T10:00:00.000Z"}'
check "measurement: opcua float -> m/opcua (generic)" ot-measurement \
  "[te/device/opc1/ot/opcua/sample/temperature] $SOPCUA" \
  '[te/device/opc1///m/opcua] {"opcua":{"temperature":21.5},"time":"2026-05-30T10:00:00.000Z"}'
check "measurement: bool coil -> 1" ot-measurement \
  "[te/device/plc1/ot/modbus/sample/coil_rw] $SBOOL" \
  '{"modbus":{"coil_rw":1}'
check_empty "measurement: bad quality dropped" ot-measurement \
  "[te/device/plc1/ot/modbus/sample/level_f32] $SBAD"

# --- device manifests (contract §8.2) ---
# The connector publishes each device's static facts once, retained: type, and per point the
# datatype/access/unit/labels, the typed signal metadata of §5 (`range`, `publish`,
# `measurement`), the free-form `meta`, and the parameter sets it resolved. The flows keep the
# last manifest per device in context.mapper, so a test feeds it first, exactly as the broker
# replays the retained message before any live sample.
#
# `publish` is on the manifest for a consumer to read, but no flow applies it: the connector
# runtime does (contract §5.4), so the stream a flow sees is already filtered.
MANIFEST_TOPIC='te/device/plc1/ot/modbus/manifest'
# plc1, no declared type: sets fall back to the protocol name.
MF_PLC1='{"contract":"0.2","protocol":"modbus","service":"tedge-dot-modbus","points":{
 "temp_u16":{"datatype":"uint16","access":"read_write","range":{"min":0,"max":30000},"parameter":{"sets":["modbus_control_parameters"]}},
 "level_f32":{"datatype":"float32","access":"read"},
 "status_word":{"datatype":"uint16","access":"read","parameter":{"sets":["modbus_control_parameters"]}},
 "pump_speed":{"datatype":"float32","access":"read_write","parameter":{"sets":["pump"]}},
 "hidden_rw":{"datatype":"uint16","access":"read_write"},
 "valve_cmd":{"datatype":"bool","access":"write","parameter":{"sets":["modbus_control_parameters"]}},
 "setpoint":{"datatype":"uint16","access":"read_write","measurement":false,"parameter":{"sets":["modbus_control_parameters"]}},
 "m1":{"datatype":"uint16","access":"read","publish":{"on_change":true}},
 "Foo.Bar":{"datatype":"uint16","access":"read","measurement":{"group":"Environment","series":"Temperature"}},
 "no_optout":{"datatype":"uint16","access":"read_write","measurement":"false","parameter":{"sets":["modbus_control_parameters"]}},
 "temperature":{"datatype":"uint16","access":"read","unit":"°C","meta":{"asset_tag":"B-17"},"measurement":{"group":"Environment","series":"Temperature"}}
}}'
# The same device declaring a type: the sets are qualified by it (RFC 0005), and two of the
# points belong to a second group / to absolute sets.
MF_TYPED='{"contract":"0.2","protocol":"modbus","service":"tedge-dot-modbus","type":"acme-boiler-v2","points":{
 "temp_u16":{"datatype":"uint16","access":"read_write","parameter":{"sets":["acme_boiler_v2_control_parameters"]}},
 "valve_cmd":{"datatype":"bool","access":"write","parameter":{"sets":["acme_boiler_v2_commissioning_parameters"]}},
 "commission_code":{"datatype":"uint16","access":"read_write","parameter":{"sets":["acme_boiler_v2_commissioning_parameters"]}},
 "flow_limit":{"datatype":"uint16","access":"read_write","parameter":{"sets":["acme_boiler_v2_control_parameters","acme_boiler_v2_commissioning_parameters"]}},
 "shared":{"datatype":"uint16","access":"read_write","parameter":{"sets":["plant_a","plant_b"]}},
 "hidden_rw":{"datatype":"uint16","access":"read_write","meta":{"parameter":false}},
 "bad_set":{"datatype":"uint16","access":"read_write","parameter":{"sets":["a/b","#"]}}
}}'
MF_OPC1='{"contract":"0.2","protocol":"opcua","service":"tedge-dot-opcua","points":{"setpoint":{"datatype":"int32","access":"read_write","parameter":{"sets":["opcua_control_parameters"]}}}}'
# `tedge flows test` reads one `[topic] payload` per line, so the fixtures above are folded.
MF_PLC1="$(printf '%s' "$MF_PLC1" | tr -d '\n')"
MF_TYPED="$(printf '%s' "$MF_TYPED" | tr -d '\n')"
MF="[$MANIFEST_TOPIC] $MF_PLC1"
MFT="[$MANIFEST_TOPIC] $MF_TYPED"
MFO="[te/device/opc1/ot/opcua/manifest] $MF_OPC1"

# A sample carries nothing static any more; the typed naming comes from the manifest.
# A point's `publish` policy is NOT applied here: the connector runtime applies it to the
# stream (contract §5.4), so two identical readings of a point with `publish.on_change` reach
# this flow only if the connector let them through — and then this flow forwards both, because
# its own on_change param is off.
SM1='{"ts":"2026-05-30T10:00:00.000Z","device":"plc1","protocol":"modbus","point":"m1","datatype":"uint16","value":42,"quality":"good"}'
SM2='{"ts":"2026-05-30T10:00:07.000Z","device":"plc1","protocol":"modbus","point":"m1","datatype":"uint16","value":42,"quality":"good"}'
check "measurement: a point's publish policy is the connector's job, not this flow's" ot-measurement \
  "$MF"$'\n'"[te/device/plc1/ot/modbus/sample/m1] $SM1"$'\n'"[te/device/plc1/ot/modbus/sample/m1] $SM2" \
  '"time":"2026-05-30T10:00:07.000Z"'
STEMP='{"ts":"2026-05-30T10:00:00.000Z","device":"plc1","protocol":"modbus","point":"temperature","datatype":"uint16","value":17.001,"quality":"good"}'
check "measurement: the manifest's typed measurement names group/series" ot-measurement \
  "$MF"$'\n'"[te/device/plc1/ot/modbus/sample/temperature] $STEMP" \
  '[te/device/plc1///m/Environment] {"Environment":{"Temperature":17.001},"time":"2026-05-30T10:00:00.000Z"}'
# A 0.1 sample that still echoes `meta` is ignored: the manifest is the only source of truth,
# and `meta` is free-form again — a site's own tags, never the naming.
SMOLD='{"ts":"2026-05-30T10:00:00.000Z","device":"plc1","protocol":"modbus","point":"temperature","datatype":"uint16","value":1,"quality":"good","meta":{"measurement":{"group":"Old","series":"Way"}}}'
check "measurement: meta echoed in a 0.1 sample is ignored; the manifest names the series" ot-measurement \
  "$MF"$'\n'"[te/device/plc1/ot/modbus/sample/temperature] $SMOLD" \
  '[te/device/plc1///m/Environment] {"Environment":{"Temperature":1}'
# A cleared manifest (the device was removed) falls back to the flow-wide defaults.
check "measurement: a cleared manifest falls back to the defaults" ot-measurement \
  "$MF"$'\n'"[$MANIFEST_TOPIC] "$'\n'"[te/device/plc1/ot/modbus/sample/temperature] $STEMP" \
  '[te/device/plc1///m/modbus] {"modbus":{"temperature":17.001}'
# The manifest line alone produces nothing.
check_empty "measurement: a manifest alone publishes nothing" ot-measurement "$MF"

# --- ot-measurement extended config (on_change / point_separator / combine) ---
# Scaling is applied by the connector (per-point transform), so the sample already carries the
# final value; the flow passes it through unchanged.
SINT='{"ts":"2026-05-30T10:00:00.000Z","device":"plc1","protocol":"modbus","point":"temp_u16","mode":"typed","datatype":"uint16","value":17001,"quality":"good"}'

check_params "measurement: passes connector-scaled value through" ot-measurement \
  'include_boolean = "true"' \
  "[te/device/plc1/ot/modbus/sample/temp_u16] $SINT" \
  '[te/device/plc1///m/modbus] {"modbus":{"temp_u16":17001},"time":"2026-05-30T10:00:00.000Z"}'

# point_separator: a dotted point id remaps the signal to group/series without per-point config.
SDOTTED='{"ts":"2026-05-30T10:00:00.000Z","device":"plc1","protocol":"modbus","point":"Environment.Temperature","mode":"typed","datatype":"uint16","value":17001,"quality":"good"}'
check_params "measurement: point_separator remaps signal to group.series" ot-measurement \
  'point_separator = "."' \
  "[te/device/plc1/ot/modbus/sample/Environment.Temperature] $SDOTTED" \
  '[te/device/plc1///m/Environment] {"Environment":{"Temperature":17001},"time":"2026-05-30T10:00:00.000Z"}'

# on_change: same value twice -> only one emission (the first); assert the second is suppressed.
check_params "measurement: on_change suppresses unchanged" ot-measurement \
  'on_change = "true"' \
  "$(printf '[te/device/plc1/ot/modbus/sample/temp_u16] %s\n[te/device/plc1/ot/modbus/sample/temp_u16] %s' "$SINT" "$SINT")" \
  '"temp_u16":17001'

# --- ot-measurement per-signal naming (from the device manifest) ---
# The per-signal PUBLISH POLICY (`publish`: on_change / deadband / min_interval / debounce) is
# no longer this flow's job: the connector runtime applies it to the sample stream itself
# (contract §5.4, RFC 0006 §5.1), so it is covered by the SDK's own tests
# (`the_runtime_applies_the_publish_policy`, `the_publish_gate_debounces`) and by the
# conformance suite, not here. What stays per signal in this flow is the NAMING, which is the
# point's typed `measurement` field on the manifest. The flow-wide params of the same names are
# exercised by the `check_params` cases above.

# The typed `measurement` wins over the point_separator convention (per-signal beats flow-wide).
MMDOT='{"ts":"2026-05-30T10:00:00.000Z","device":"plc1","protocol":"modbus","point":"Foo.Bar","mode":"typed","datatype":"uint16","value":1,"quality":"good"}'
mmtmp="$(flow_with_params ot-measurement 'point_separator = "."')"
mmout="$(printf '%s\n%s\n' "$MF" "[te/device/plc1/ot/modbus/sample/Foo.Bar] $MMDOT" | tedge flows test --flows-dir "$mmtmp" 2>/dev/null)"
rm -rf "$mmtmp"
if [[ "$mmout" == *'{"Environment":{"Temperature":1}'* && "$mmout" != *'"Foo"'* ]]; then
  echo "ok   - measurement: the typed measurement wins over point_separator"
  pass=$((pass + 1))
else
  echo "FAIL - measurement: the typed measurement wins over point_separator"
  echo "       got: $mmout"
  fail=$((fail + 1))
fi

# measurement = false: the signal stays off the measurements entirely — a parameter whose
# value belongs on its twin fragment only. Without it (the default), a parameter is published both
# ways, and a naming table (above) still publishes.
# The three points differ only in the manifest: `setpoint` carries meta.measurement = false,
# `no_optout` the string "false", and `temp_u16` no measurement meta at all.
MOFF='{"ts":"2026-05-30T10:00:00.000Z","device":"plc1","protocol":"modbus","point":"setpoint","mode":"typed","datatype":"uint16","value":55,"quality":"good"}'
MOFFSTR='{"ts":"2026-05-30T10:00:00.000Z","device":"plc1","protocol":"modbus","point":"no_optout","mode":"typed","datatype":"uint16","value":55,"quality":"good"}'
MPARAM='{"ts":"2026-05-30T10:00:00.000Z","device":"plc1","protocol":"modbus","point":"temp_u16","mode":"typed","datatype":"uint16","value":55,"quality":"good"}'
check_empty "measurement: measurement = false keeps the signal off the measurements" ot-measurement \
  "$MF"$'\n'"[te/device/plc1/ot/modbus/sample/setpoint] $MOFF"
# Only the boolean opts out, as for parameter = false: a string is not a switch.
check "measurement: measurement = \"false\" (a string) does not opt out" ot-measurement \
  "$MF"$'\n'"[te/device/plc1/ot/modbus/sample/no_optout] $MOFFSTR" \
  '[te/device/plc1///m/modbus] {"modbus":{"no_optout":55}'
check "measurement: a parameter is still a measurement by default" ot-measurement \
  "$MF"$'\n'"[te/device/plc1/ot/modbus/sample/temp_u16] $MPARAM" \
  '[te/device/plc1///m/modbus] {"modbus":{"temp_u16":55},"time":"2026-05-30T10:00:00.000Z"}'
# The opt-out is for measurements only: ot-parameter-state still puts the value on the twin.
check_multi "measurement: an opted-out parameter still reaches its twin fragment" \
  "ot-measurement ot-parameter-state" \
  "$MF"$'\n'"[te/device/plc1/ot/modbus/sample/setpoint] $MOFF" \
  '[te/device/plc1///twin/modbus_control_parameters] {"setpoint":55}' \
  --absent '///m/'

# combine: two series of one device merged into a single measurement, flushed on interval.
SLVL='{"ts":"2026-05-30T10:00:00.000Z","device":"plc1","protocol":"modbus","point":"level_f32","mode":"typed","datatype":"float32","value":404.17,"quality":"good"}'
check_params "measurement: combine merges series on interval" ot-measurement \
  "$(printf 'combine = "true"\ncombine_interval = "1s"')" \
  "$(printf '[te/device/plc1/ot/modbus/sample/temp_u16] %s\n[te/device/plc1/ot/modbus/sample/level_f32] %s' "$SINT" "$SLVL")" \
  '[te/device/plc1///m/modbus] {"modbus":{"level_f32":404.17,"temp_u16":17001}' \
  --final-on-interval
# ...and an opted-out signal never reaches the combine buffer, so the flush leaves it out.
check_params "measurement: combine leaves an opted-out signal out" ot-measurement \
  "$(printf 'combine = "true"\ncombine_interval = "1s"')" \
  "$(printf '%s\n[te/device/plc1/ot/modbus/sample/temp_u16] %s\n[te/device/plc1/ot/modbus/sample/setpoint] %s' "$MF" "$SINT" "$MOFF")" \
  '[te/device/plc1///m/modbus] {"modbus":{"temp_u16":17001},"time"' \
  --final-on-interval

# --- ot-event (measurement -> event on value change) ---
check "event: emits on first value" ot-event \
  '[te/device/plc1///m/modbus] {"modbus":{"value":5},"time":"2026-05-30T10:00:00.000Z"}' \
  '[te/device/plc1///e/ot_event] {"text":"OT value changed","time":"2026-05-30T10:00:00.000Z"}'
check "event: opcua measurement raises (generic)" ot-event \
  '[te/device/opc1///m/opcua] {"opcua":{"value":9},"time":"2026-05-30T10:00:00.000Z"}' \
  '[te/device/opc1///e/ot_event]'
# Same value twice -> a single event (the second is suppressed as unchanged).
check "event: change-detection fires once for repeats" ot-event \
  "$(printf '[te/device/plc1///m/modbus] {"modbus":{"value":5},"time":"t1"}\n[te/device/plc1///m/modbus] {"modbus":{"value":5},"time":"t2"}')" \
  '"time":"t1"'

# --- ot-alarm (measurement -> alarm, hysteresis; group taken from topic) ---
check "alarm: modbus measurement raises" ot-alarm \
  '[te/device/plc1///m/modbus] {"modbus":{"temp_u16":80},"time":"2026-05-30T10:00:00.000Z"}' \
  '[te/device/plc1///a/ot_overrange] {"severity":"major"'
check "alarm: opcua measurement raises (generic)" ot-alarm \
  '[te/device/opc1///m/opcua] {"opcua":{"temp_u16":80},"time":"2026-05-30T10:00:00.000Z"}' \
  '[te/device/opc1///a/ot_overrange] {"severity":"major"'
check_empty "alarm: below threshold, never raised" ot-alarm \
  '[te/device/plc1///m/modbus] {"modbus":{"temp_u16":60},"time":"2026-05-30T10:00:00.000Z"}'

# --- ot-registration (link -> child-device registration; type from the connector, else protocol) ---
check "registration: declared device type becomes the entity type" ot-registration \
  '[te/device/plc1/ot/modbus/status/link] {"status":"connected","type":"acme-boiler-v2"}' \
  '[te/device/plc1//] {"@type":"child-device","name":"plc1","type":"acme-boiler-v2","ot-protocol":"modbus"}'
check "registration: modbus link -> modbus-device" ot-registration \
  '[te/device/plc1/ot/modbus/status/link] {"status":"connected"}' \
  '[te/device/plc1//] {"@type":"child-device","name":"plc1","type":"modbus-device","ot-protocol":"modbus"}'
check "registration: opcua link -> opcua-device (generic)" ot-registration \
  '[te/device/opc1/ot/opcua/status/link] {"status":"connected"}' \
  '[te/device/opc1//] {"@type":"child-device","name":"opc1","type":"opcua-device","ot-protocol":"opcua"}'
check_empty "registration: disconnected ignored" ot-registration \
  '[te/device/plc1/ot/modbus/status/link] {"status":"disconnected"}'
check_params "registration: publishes the twin fragment from the manifest's info" ot-registration \
  'twin_fragment = "c8y_ModbusDevice"' \
  '[te/device/plc1/ot/modbus/manifest] {"contract":"0.2","protocol":"modbus","service":"tedge-dot-modbus","info":{"protocol":"modbus","transport":"tcp","host":"127.0.0.1","port":502,"unit_id":1},"points":{}}' \
  '[te/device/plc1///twin/c8y_ModbusDevice] {"protocol":"modbus","transport":"tcp","host":"127.0.0.1","port":502,"unit_id":1}'
check_empty "registration: a manifest without info publishes no twin" ot-registration \
  '[te/device/plc1/ot/modbus/manifest] {"contract":"0.2","protocol":"modbus","service":"tedge-dot-modbus","points":{}}'
check_empty "registration: a manifest never registers a device by itself" ot-registration \
  '[te/device/plc1/ot/modbus/manifest] {"contract":"0.2","protocol":"modbus","service":"tedge-dot-modbus","type":"acme-boiler-v2","info":{"host":"x"},"points":{}}'

# --- ot-parameter-state (manifest + samples + write results -> twin parameter sets) ---
# The manifest says which points are parameters and which sets they belong to (resolved by the
# connector); samples and write results carry only values.
ST='{"ts":"2026-05-30T10:00:00.000Z","device":"plc1","protocol":"modbus","point":"temp_u16","datatype":"uint16","value":17001,"quality":"good"}'
SL='{"ts":"2026-05-30T10:00:00.000Z","device":"plc1","protocol":"modbus","point":"level_f32","datatype":"float32","value":1.5,"quality":"good"}'
SW='{"ts":"2026-05-30T10:00:00.000Z","device":"plc1","protocol":"modbus","point":"status_word","datatype":"uint16","value":7,"quality":"good"}'
SP='{"ts":"2026-05-30T10:00:00.000Z","device":"plc1","protocol":"modbus","point":"pump_speed","datatype":"float32","value":10.5,"quality":"good"}'
SH='{"ts":"2026-05-30T10:00:00.000Z","device":"plc1","protocol":"modbus","point":"hidden_rw","datatype":"uint16","value":1,"quality":"good"}'
STBAD='{"ts":"2026-05-30T10:00:00.000Z","device":"plc1","protocol":"modbus","point":"temp_u16","datatype":"uint16","quality":"bad","error":"timeout"}'
check "parameter-state: writable point sample -> twin set keyed by point id" ot-parameter-state \
  "$MF"$'\n'"[te/device/plc1/ot/modbus/sample/temp_u16] $ST" \
  '[te/device/plc1///twin/modbus_control_parameters] {"temp_u16":17001}'
check_empty "parameter-state: read-only point ignored" ot-parameter-state \
  "$MF"$'\n'"[te/device/plc1/ot/modbus/sample/level_f32] $SL"
SUNKNOWN='{"ts":"2026-05-30T10:00:00.000Z","device":"plc1","protocol":"modbus","point":"unknown","datatype":"uint16","value":1,"quality":"good"}'
check_empty "parameter-state: a point the manifest does not list is ignored" ot-parameter-state \
  "$MF"$'\n'"[te/device/plc1/ot/modbus/sample/unknown] $SUNKNOWN"
check_empty "parameter-state: bad-quality sample ignored" ot-parameter-state \
  "$MF"$'\n'"[te/device/plc1/ot/modbus/sample/temp_u16] $STBAD"
check_empty "parameter-state: a sample before the manifest is left alone" ot-parameter-state \
  "[te/device/plc1/ot/modbus/sample/temp_u16] $ST"
check_empty "parameter-state: a manifest alone publishes nothing" ot-parameter-state "$MF"
check_absent "parameter-state: unchanged value republishes nothing (single twin)" ot-parameter-state \
  "$MF"$'\n'"[te/device/plc1/ot/modbus/sample/temp_u16] $ST"$'\n'"[te/device/plc1/ot/modbus/sample/temp_u16] $ST" \
  '{"temp_u16":17001}' \
  '{"temp_u16":17001}
[te/device/plc1///twin/modbus_control_parameters] {"temp_u16":17001}'
check "parameter-state: an absolute set from the manifest" ot-parameter-state \
  "$MF"$'\n'"[te/device/plc1/ot/modbus/sample/pump_speed] $SP" \
  '[te/device/plc1///twin/pump] {"pump_speed":10.5}'
check "parameter-state: opted-in read-only point is displayed" ot-parameter-state \
  "$MF"$'\n'"[te/device/plc1/ot/modbus/sample/status_word] $SW" \
  '[te/device/plc1///twin/modbus_control_parameters] {"status_word":7}'
check_empty "parameter-state: an opted-out writable point (no sets on the manifest) stays out" ot-parameter-state \
  "$MF"$'\n'"[te/device/plc1/ot/modbus/sample/hidden_rw] $SH"
check "parameter-state: opted-out point stays out after a write" ot-parameter-state \
  "$MF"$'\n'"[te/device/plc1/ot/modbus/sample/temp_u16] $ST"$'\n'"[te/device/plc1/ot/modbus/cmd/write/w1] {\"status\":\"successful\",\"point\":\"hidden_rw\",\"value\":2}" \
  '[te/device/plc1///twin/modbus_control_parameters] {"temp_u16":17001}'
check "parameter-state: write-only point takes the last acknowledged batch write" ot-parameter-state \
  "$MF"$'\n''[te/device/plc1/ot/modbus/cmd/write-batch/ot--1] {"status":"successful","results":[{"point":"valve_cmd","status":"successful","value":true}]}' \
  '[te/device/plc1///twin/modbus_control_parameters] {"valve_cmd":true}'
check "parameter-state: single write result updates a read/write point optimistically" ot-parameter-state \
  "$MF"$'\n'"[te/device/plc1/ot/modbus/sample/temp_u16] $ST"$'\n'"[te/device/plc1/ot/modbus/cmd/write/abc] {\"status\":\"successful\",\"point\":\"temp_u16\",\"value\":4242}" \
  '[te/device/plc1///twin/modbus_control_parameters] {"temp_u16":4242}'
check "parameter-state: a written point lands in the set the manifest gives it" ot-parameter-state \
  "$MF"$'\n'"[te/device/plc1/ot/modbus/cmd/write/abc] {\"status\":\"successful\",\"point\":\"pump_speed\",\"value\":12}" \
  '[te/device/plc1///twin/pump] {"pump_speed":12}'
check_empty "parameter-state: failed write leaves the twin alone" ot-parameter-state \
  "$MF"$'\n''[te/device/plc1/ot/modbus/cmd/write/abc] {"status":"failed","point":"temp_u16","reason":"boom"}'
check_empty "parameter-state: a write result before the manifest is left alone" ot-parameter-state \
  '[te/device/plc1/ot/modbus/cmd/write/abc] {"status":"successful","point":"temp_u16","value":4242}'
check_params "parameter-state: default_set param renames every set" ot-parameter-state \
  'default_set = "plc_settings"' \
  "$MF"$'\n'"[te/device/plc1/ot/modbus/sample/pump_speed] $SP" \
  '[te/device/plc1///twin/plc_settings] {"pump_speed":10.5}'
# An unusable default_set (`/` would publish outside te/<device>///twin/) publishes nowhere.
dstmp="$(flow_with_params ot-parameter-state 'default_set = "a/b"')"
dsout="$(printf '%s\n' "$MF"$'\n'"[te/device/plc1/ot/modbus/sample/temp_u16] $ST" | tedge flows test --flows-dir "$dstmp" 2>/dev/null)"
rm -rf "$dstmp"
if [[ -z "$dsout" ]]; then
  echo "ok   - parameter-state: an unusable default_set publishes nowhere"
  pass=$((pass + 1))
else
  echo "FAIL - parameter-state: an unusable default_set publishes nowhere"
  echo "       got: $dsout"
  fail=$((fail + 1))
fi
check "parameter-state: opcua samples -> opcua_control_parameters (generic)" ot-parameter-state \
  "$MFO"$'\n''[te/device/opc1/ot/opcua/sample/setpoint] {"device":"opc1","protocol":"opcua","point":"setpoint","datatype":"int32","value":42,"quality":"good"}' \
  '[te/device/opc1///twin/opcua_control_parameters] {"setpoint":42}'
# A DTM identifier is tenant-wide, so the connector qualifies the set by the *device type* on
# the manifest — the flow publishes exactly the name `tedge-dot describe` renders.
check "parameter-state: the device type qualifies the set name (from the manifest)" ot-parameter-state \
  "$MFT"$'\n'"[te/device/plc1/ot/modbus/sample/temp_u16] $ST" \
  '[te/device/plc1///twin/acme_boiler_v2_control_parameters] {"temp_u16":17001}'
SGROUP='{"ts":"2026-05-30T10:00:00.000Z","device":"plc1","protocol":"modbus","point":"commission_code","datatype":"uint16","value":3,"quality":"good"}'
check "parameter-state: a second group of the same type" ot-parameter-state \
  "$MFT"$'\n'"[te/device/plc1/ot/modbus/sample/commission_code] $SGROUP" \
  '[te/device/plc1///twin/acme_boiler_v2_commissioning_parameters] {"commission_code":3}'
# A write-only point never samples: the manifest is what names its set — including a
# non-default group, which used to need the request's origin.set.
check "parameter-state: the manifest names the set of a write-only point" ot-parameter-state \
  "$MFT"$'\n''[te/device/plc1/ot/modbus/cmd/write-batch/ot--1] {"status":"successful","results":[{"point":"valve_cmd","status":"successful","value":true}]}' \
  '[te/device/plc1///twin/acme_boiler_v2_commissioning_parameters] {"valve_cmd":true}'
# A republished manifest (a reload changed the type) moves the points to the new sets.
check "parameter-state: a republished manifest renames the sets" ot-parameter-state \
  "$MF"$'\n'"$MFT"$'\n'"[te/device/plc1/ot/modbus/sample/temp_u16] $ST" \
  '[te/device/plc1///twin/acme_boiler_v2_control_parameters] {"temp_u16":17001}'
# A point can be in SEVERAL sets: its value must reach every fragment.
SMULTI='{"ts":"2026-05-30T10:00:00.000Z","device":"plc1","protocol":"modbus","point":"flow_limit","datatype":"uint16","value":42,"quality":"good"}'
check "parameter-state: a point in two groups updates both fragments" ot-parameter-state \
  "$MFT"$'\n'"[te/device/plc1/ot/modbus/sample/flow_limit] $SMULTI" \
  '[te/device/plc1///twin/acme_boiler_v2_control_parameters] {"flow_limit":42}'
check "parameter-state: ...and the second fragment carries it too" ot-parameter-state \
  "$MFT"$'\n'"[te/device/plc1/ot/modbus/sample/flow_limit] $SMULTI" \
  '[te/device/plc1///twin/acme_boiler_v2_commissioning_parameters] {"flow_limit":42}'
check "parameter-state: a write to a multi-group point updates every fragment" ot-parameter-state \
  "$MFT"$'\n'"[te/device/plc1/ot/modbus/sample/flow_limit] $SMULTI"$'\n'"[te/device/plc1/ot/modbus/cmd/write/w9] {\"status\":\"successful\",\"point\":\"flow_limit\",\"value\":7}" \
  '[te/device/plc1///twin/acme_boiler_v2_commissioning_parameters] {"flow_limit":7}'
SSHARED='{"ts":"2026-05-30T10:00:00.000Z","device":"plc1","protocol":"modbus","point":"shared","datatype":"uint16","value":5,"quality":"good"}'
check "parameter-state: an absolute set list reaches each set" ot-parameter-state \
  "$MFT"$'\n'"[te/device/plc1/ot/modbus/sample/shared] $SSHARED" \
  '[te/device/plc1///twin/plant_b] {"shared":5}'
# A replayed result alone (a mapper restarted between request and result) still lands in the
# right set: the manifest is retained too, so it is replayed with it.
RESONLY='{"status":"successful","results":[{"point":"valve_cmd","status":"successful","value":true}],"origin":{"command":"parameter_update","set":"acme_boiler_v2_commissioning_parameters","parameters":{"valve_cmd":true}}}'
check "parameter-state: a replayed result alone still names the set (manifest replayed too)" ot-parameter-state \
  "$MFT"$'\n'"[te/device/plc1/ot/modbus/cmd/write-batch/ot--9] $RESONLY" \
  '[te/device/plc1///twin/acme_boiler_v2_commissioning_parameters] {"valve_cmd":true}'
# An opted-out point stays out even when a request names a set for it: the manifest decides.
check_empty "parameter-state: origin.set cannot resurrect an opted-out point" ot-parameter-state \
  "$MFT"$'\n'"[te/device/plc1/ot/modbus/cmd/write-batch/ot--8] {\"status\":\"successful\",\"results\":[{\"point\":\"hidden_rw\",\"status\":\"successful\",\"value\":2}],\"origin\":{\"command\":\"parameter_update\",\"set\":\"acme_boiler_v2_control_parameters\"}}"
# A set name becomes a twin fragment key AND a topic segment. The connector refuses to publish an
# unusable one, and the flow checks again: `#`/`+` would be an illegal PUBLISH topic and `/`
# would publish outside te/<device>///twin/.
SBADSET='{"ts":"2026-05-30T10:00:00.000Z","device":"plc1","protocol":"modbus","point":"bad_set","datatype":"uint16","value":1,"quality":"good"}'
check_empty "parameter-state: unusable set names on a manifest cannot reach the topic" ot-parameter-state \
  "$MFT"$'\n'"[te/device/plc1/ot/modbus/sample/bad_set] $SBADSET"
# A cleared manifest (the device was removed): later values go nowhere.
check_empty "parameter-state: a cleared manifest stops the updates" ot-parameter-state \
  "$MF"$'\n'"[$MANIFEST_TOPIC] "$'\n'"[te/device/plc1/ot/modbus/sample/temp_u16] $ST"

# --- ot-command-forward: parameter_update -> write-batch ---
C8YOP='{"status":"init","operation":{"deviceId":"123","c8y_ParameterUpdate":{},"c8y_ParameterUpdate_acme_boiler_v2_control_parameters":{},"acme_boiler_v2_control_parameters":{"temp_u16":4242,"coil_rw":true}},"c8y-mapper":{"on_fragment":"c8y_ParameterUpdate","output":null}}'
check "command-forward: c8y parameter update -> one write-batch with origin + mapper metadata" ot-command-forward \
  "[te/device/plc1///cmd/parameter_update/c8y-mapper-1] $C8YOP" \
  '[te/device/plc1/ot/modbus/cmd/write-batch/ot--c8y-mapper-1] {"status":"init","writes":[{"point":"temp_u16","value":4242},{"point":"coil_rw","value":true}],"origin":{"command":"parameter_update","set":"acme_boiler_v2_control_parameters","parameters":{"temp_u16":4242,"coil_rw":true}},"c8y-mapper":{"on_fragment":"c8y_ParameterUpdate","output":null}}'
check "command-forward: direct parameter update shape" ot-command-forward \
  '[te/device/plc1///cmd/parameter_update/x1] {"status":"init","set":"pump","parameters":{"pump_speed":12}}' \
  '[te/device/plc1/ot/modbus/cmd/write-batch/ot--x1] {"status":"init","writes":[{"point":"pump_speed","value":12}],"origin":{"command":"parameter_update","set":"pump","parameters":{"pump_speed":12}}}'
check_params "command-forward: protocol recorded by ot-parameter-state wins over params" ot-command-forward '' \
  '[te/device/opc1///cmd/parameter_update/x1] {"status":"init","set":"opcua_control_parameters","parameters":{"setpoint":7}}' \
  '[te/device/opc1/ot/opcua/cmd/write-batch/ot--x1]' \
  --context '{"ot-protocol:opc1":"opcua"}'
check "command-forward: unintelligible parameter update forwarded as an empty batch with the error noted" ot-command-forward \
  '[te/device/plc1///cmd/parameter_update/x2] {"status":"init","operation":{"c8y_ParameterUpdate":{}}}' \
  '"writes":[],"origin":{"command":"parameter_update","set":null,"parameters":null,"error":"c8y_ParameterUpdate operation names no parameter set"}'

# The connector echoes `origin` into its results (§6.4), so a terminal result replayed on its
# own — a mapper restarted between the request and the result, its in-memory cache gone — still
# completes the command type the requester asked for. Without this the result is mirrored onto
# ot_write_batch and the Cumulocity operation waits on parameter_update forever.
check "command-result: a replayed result alone routes by the echoed origin" ot-command-result \
  '[te/device/plc1/ot/modbus/cmd/write-batch/ot--c8y-mapper-7] {"status":"successful","results":[{"point":"temp_u16","status":"successful","value":4242}],"origin":{"command":"parameter_update","set":"acme_boiler_v2_control_parameters"}}' \
  '[te/device/plc1///cmd/parameter_update/c8y-mapper-7] {'

# --- ot-command-result: origin.command routes reshaped commands back ---
BINIT='{"status":"init","writes":[{"point":"temp_u16","value":4242}],"origin":{"command":"parameter_update","set":"acme_boiler_v2_control_parameters","parameters":{"temp_u16":4242}},"c8y-mapper":{"on_fragment":"c8y_ParameterUpdate","output":null}}'
BRESULT="[te/device/plc1/ot/modbus/cmd/write-batch/ot--c8y-mapper-1] $BINIT"$'\n'"[te/device/plc1/ot/modbus/cmd/write-batch/ot--c8y-mapper-1] {\"status\":\"successful\",\"results\":[{\"point\":\"temp_u16\",\"status\":\"successful\",\"value\":4242}]}"
check "command-result: batch result completes the originating command type" ot-command-result "$BRESULT" \
  '[te/device/plc1///cmd/parameter_update/c8y-mapper-1] {'
check "command-result: batch result keeps the c8y-mapper metadata" ot-command-result "$BRESULT" \
  '"c8y-mapper":{"on_fragment":"c8y_ParameterUpdate","output":null}'
check "command-result: batch result carries status + per-point results" ot-command-result "$BRESULT" \
  '"status":"successful","results":[{"point":"temp_u16","status":"successful","value":4242}]}'
check_absent "command-result: batch request body (writes) is not echoed" ot-command-result "$BRESULT" \
  '"status":"successful"' '"writes"'
check "command-result: failed batch combines the origin note and the connector reason" ot-command-result \
  '[te/device/plc1/ot/modbus/cmd/write-batch/ot--x2] {"status":"init","writes":[],"origin":{"command":"parameter_update","error":"no parameter set"}}'$'\n''[te/device/plc1/ot/modbus/cmd/write-batch/ot--x2] {"status":"failed","reason":"write-batch request has no writes","results":[]}' \
  '[te/device/plc1///cmd/parameter_update/x2] {"origin":{"command":"parameter_update","error":"no parameter set"},"status":"failed","reason":"no parameter set; write-batch request has no writes","results":[]}'
check "command-result: batch without origin mirrors as ot_write_batch" ot-command-result \
  '[te/device/plc1/ot/modbus/cmd/write-batch/ot--b1] {"status":"successful","results":[]}' \
  '[te/device/plc1///cmd/ot_write_batch/b1] {"status":"successful","results":[]}'
check "registration: advertises the parameter_update capability" ot-registration \
  '[te/device/plc1/ot/modbus/status/link] {"status":"connected"}' \
  '[te/device/plc1///cmd/parameter_update] {}'

# --- the parameter bridge in one mapper: state records the protocol, forward uses it, result completes, state updates the twin ---
CHAIN="[te/device/opc1/ot/opcua/manifest] $MF_OPC1
[te/device/opc1/ot/opcua/sample/setpoint] {\"device\":\"opc1\",\"protocol\":\"opcua\",\"point\":\"setpoint\",\"mode\":\"typed\",\"datatype\":\"int32\",\"value\":0,\"quality\":\"good\"}
[te/device/opc1///cmd/parameter_update/c8y-mapper-9] {\"status\":\"init\",\"operation\":{\"c8y_ParameterUpdate\":{},\"c8y_ParameterUpdate_opcua_control_parameters\":{},\"opcua_control_parameters\":{\"setpoint\":42}},\"c8y-mapper\":{\"on_fragment\":\"c8y_ParameterUpdate\",\"output\":null}}
[te/device/opc1/ot/opcua/cmd/write-batch/ot--c8y-mapper-9] {\"status\":\"init\",\"writes\":[{\"point\":\"setpoint\",\"value\":42}],\"origin\":{\"command\":\"parameter_update\",\"set\":\"opcua_control_parameters\",\"parameters\":{\"setpoint\":42}},\"c8y-mapper\":{\"on_fragment\":\"c8y_ParameterUpdate\",\"output\":null}}
[te/device/opc1/ot/opcua/cmd/write-batch/ot--c8y-mapper-9] {\"status\":\"successful\",\"results\":[{\"point\":\"setpoint\",\"status\":\"successful\",\"value\":42}]}"
check_multi "parameter bridge: forward targets the protocol the state flow recorded (no params needed)" \
  "ot-parameter-state ot-command-forward ot-command-result" "$CHAIN" \
  '[te/device/opc1/ot/opcua/cmd/write-batch/ot--c8y-mapper-9] {"status":"init","writes":[{"point":"setpoint","value":42}]'
check_multi "parameter bridge: result completes the cloud-bound command" \
  "ot-parameter-state ot-command-forward ot-command-result" "$CHAIN" \
  '[te/device/opc1///cmd/parameter_update/c8y-mapper-9] {'
check_multi "parameter bridge: completed command keeps the mapper metadata and result" \
  "ot-parameter-state ot-command-forward ot-command-result" "$CHAIN" \
  '"status":"successful","results":[{"point":"setpoint","status":"successful","value":42}]}'
check_multi "parameter bridge: acknowledged write updates the twin, no ot_write_batch echo" \
  "ot-parameter-state ot-command-forward ot-command-result" "$CHAIN" \
  '[te/device/opc1///twin/opcua_control_parameters] {"setpoint":42}' --absent 'ot_write_batch'

# --- ot-command-forward (thin-edge cmd -> connector write) ---
check "command-forward: init forwarded" ot-command-forward \
  '[te/device/plc1///cmd/ot_write/abc] {"status":"init","point":"coil_rw","value":true}' \
  '[te/device/plc1/ot/modbus/cmd/write/ot--abc] {"status":"init","point":"coil_rw","value":true}'
check_empty "command-forward: non-init ignored" ot-command-forward \
  '[te/device/plc1///cmd/ot_write/abc] {"status":"successful","point":"coil_rw"}'
# Management verbs change one connector instance's configuration, so they go to its service topic
# (contract §6.3) — the named `service`, else the packaged tedge-dot-<protocol> — with the entity
# the command was issued on recorded in origin.device for ot-command-result.
check "command-forward: set-config init forwarded to the default service" ot-command-forward \
  '[te/device/main///cmd/ot_set_config/cfg1] {"status":"init","target":"connector","config":{"poll_interval":"5s"}}' \
  '[te/device/main/service/tedge-dot-modbus/ot/cmd/set-config/ot--cfg1] {"status":"init","target":"connector","config":{"poll_interval":"5s"},"origin":{"device":"main"}}'
check "command-forward: define-device init forwarded to the service it names" ot-command-forward \
  '[te/device/main///cmd/ot_define_device/d1] {"status":"init","service":"plant-a","device":{"name":"plc-9"}}' \
  '[te/device/main/service/plant-a/ot/cmd/define-device/ot--d1] {"status":"init","device":{"name":"plc-9"},"origin":{"device":"main"}}'
check "command-forward: remove-device keeps the requester's origin and adds the entity" ot-command-forward \
  '[te/device/gw1///cmd/ot_remove_device/r1] {"status":"init","device":"plc-9","origin":{"ticket":7}}' \
  '[te/device/main/service/tedge-dot-modbus/ot/cmd/remove-device/ot--r1] {"status":"init","device":"plc-9","origin":{"ticket":7,"device":"gw1"}}'
check_empty "command-forward: a service that is not a topic level is not forwarded" ot-command-forward \
  '[te/device/main///cmd/ot_define_device/d2] {"status":"init","service":"+","device":{"name":"plc-9"}}'
# The default follows the protocol the command targets: the one ot-parameter-state recorded for
# the entity, else params.protocol.
check_params "command-forward: management command naming no service goes to tedge-dot-<recorded protocol>" ot-command-forward '' \
  '[te/device/gw1///cmd/ot_set_config/cfg2] {"status":"init","target":"connector","config":{"poll_interval":"5s"}}' \
  '[te/device/main/service/tedge-dot-opcua/ot/cmd/set-config/ot--cfg2]' \
  --context '{"ot-protocol:gw1":"opcua"}'
check_empty "command-forward: non-ot command ignored" ot-command-forward \
  '[te/device/plc1///cmd/restart/abc] {"status":"init"}'

# --- ot-command-result (connector result -> thin-edge cmd) ---
check "command-result: successful mirrored" ot-command-result \
  '[te/device/plc1/ot/modbus/cmd/write/abc] {"status":"successful","point":"coil_rw","value":true}' \
  '[te/device/plc1///cmd/ot_write/abc]'
check "command-result: opcua result mirrored (generic)" ot-command-result \
  '[te/device/opc1/ot/opcua/cmd/write/xyz] {"status":"successful","point":"setpoint","value":42}' \
  '[te/device/opc1///cmd/ot_write/xyz]'
check "command-result: set-config result on a service topic mirrored onto main" ot-command-result \
  '[te/device/main/service/tedge-dot-modbus/ot/cmd/set-config/ot--cfg1] {"status":"successful"}' \
  '[te/device/main///cmd/ot_set_config/cfg1]'
check "command-result: management result completes the command on the echoed origin.device" ot-command-result \
  '[te/device/main/service/plant-a/ot/cmd/remove-device/ot--r1] {"status":"successful","origin":{"device":"gw1"}}' \
  '[te/device/gw1///cmd/ot_remove_device/r1]'
check "command-result: an origin.device that is not a topic level falls back to main" ot-command-result \
  '[te/device/main/service/plant-a/ot/cmd/remove-device/ot--r2] {"status":"successful","origin":{"device":"#"}}' \
  '[te/device/main///cmd/ot_remove_device/r2]'
check_empty "command-result: init not mirrored (no loop)" ot-command-result \
  '[te/device/plc1/ot/modbus/cmd/write/ot--abc] {"status":"init","point":"coil_rw","value":true}'
check "command-result: c8y-mapper metadata preserved in result" ot-command-result \
  $'[te/device/plc1/ot/modbus/cmd/write/ot--abc] {"status":"init","point":"coil_rw","value":true,"c8y-mapper":{"on_fragment":"c8y_SetCoil","output":null}}\n[te/device/plc1/ot/modbus/cmd/write/ot--abc] {"status":"successful","point":"coil_rw","value":true}' \
  '"c8y-mapper":{"on_fragment":"c8y_SetCoil","output":null}'

echo
echo "flows: $pass passed, $fail failed"
[[ "$fail" -eq 0 ]]
