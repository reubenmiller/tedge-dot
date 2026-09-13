# Cumulocity operation shims for the OT connector

These files translate the legacy Cumulocity operations the Python Modbus plugin shipped (under
the repo-root [`operations/`](../../operations/)) into the **generic OT command model** of the
new connector. They are the Cumulocity-specific glue: each one maps a cloud operation onto a
protocol-neutral `ot_<verb>` thin-edge command. The [`ot-command-forward`](../flows/) flow then
bridges that command to the connector's `cmd/<verb>` topic, the connector (or the SDK runtime, for
management verbs) acts on it, and [`ot-command-result`](../flows/) mirrors the result back so the
cloud operation completes.

Management verbs (`set-config`, `define-device`, `remove-device`) change one connector instance's
configuration, so they go to that instance's service topic
(`te/device/main/service/<service>/ot/cmd/<verb>/<id>`, contract §6.3) rather than a device topic.
`ot-command-forward` uses the command's `service` field, else `tedge-dot-<protocol>` — the default
`service_name` of a connector config. When the connector's config sets another `service_name`
(e.g. several configurations of one protocol), name the service: `c8y-fieldbus-import` sets it
from `FIELDBUS_SERVICE`; the `c8y_ModbusConfiguration`/`c8y_SerialConfiguration` templates
always use the default.

```text
 c8y operation ─▶ c8y-mapper ─▶ cmd/ot_<verb> ─▶ ot-command-forward ─▶ ot/<protocol>/cmd/<verb> ─▶ connector
                                       ▲                                                                │
 operation SUCCESSFUL ◀── c8y-mapper ──┴──────────────── ot-command-result ◀───────────────────────────┘
```

## Mapping

| Legacy operation | Shim file | Generic command | Verb | Handled by |
| --- | --- | --- | --- | --- |
| `c8y_SetRegister` | [`c8y_SetRegister`](c8y_SetRegister) | `ot_write` | `write` | protocol module |
| `c8y_SetCoil` | [`c8y_SetCoil`](c8y_SetCoil) | `ot_write_coil` | `write-coil` | protocol module |
| `c8y_ModbusConfiguration` | [`c8y_ModbusConfiguration`](c8y_ModbusConfiguration) | `ot_set_config` | `set-config` | SDK runtime |
| `c8y_SerialConfiguration` | [`c8y_SerialConfiguration`](c8y_SerialConfiguration) | `ot_set_config` | `set-config` | SDK runtime |
| `c8y_ModbusDevice` (+ `c8y_Coils`/`c8y_Registers`) | [`c8y_ModbusDevice`](c8y_ModbusDevice) → [`c8y-fieldbus-import`](c8y-fieldbus-import) | `ot_define_device` | `define-device` | SDK runtime |
| `c8y_ParameterUpdate` (device parameters) | *none here* — the [tedge-parameter-plugin](https://github.com/thin-edge/tedge-parameter-plugin)'s `c8y_ParameterUpdate.template` | `parameter_update` | `write-batch` (reshaped by `ot-command-forward`) | SDK runtime |

`c8y_Coils` and `c8y_Registers` no longer have standalone shims: the legacy operations only staged
point definitions in TOML that `c8y_ModbusDevice` later assembled. In the generic model the points
travel inside the `ot_define_device` payload (the `device.point[]` array), so defining a device and
its points is a single operation.

The generic verbs are defined in the [OT connector contract §6](../../doc/proposal/contract/ot-connector-contract.md)
and implemented once in the SDK runtime (management verbs) or the protocol module (`write`), so the
same shims work for any SDK-based connector — only the operation fragment names are Cumulocity- and
Modbus-specific.

## Payload shapes

The shims build the generic command input from the cloud operation payload. The generic model is
**point-name based** (it references connector config point ids), not raw protocol coordinates, so
the operation payloads carry logical fields:

```jsonc
// c8y_SetRegister / c8y_SetCoil
{ "c8y_SetRegister": { "point": "boiler_setpoint", "value": 21.5 } }
{ "c8y_SetCoil":     { "point": "pump_run",        "value": true  } }

// c8y_ModbusConfiguration  (transmitRate is now a flow concern; only pollingRate maps)
{ "c8y_ModbusConfiguration": { "pollingRate": "5s" } }

// c8y_SerialConfiguration
{ "c8y_SerialConfiguration": { "baudRate": 19200, "stopBits": 1, "parity": "N", "dataBits": 8 } }

// c8y_ModbusDevice, shape 1: the stock Cloud Fieldbus UI payload. The c8y-fieldbus-import
// script creates the child's external identity, fetches the c8y_ModbusDeviceType MO named by
// `type` via the mapper's c8y proxy, and translates its c8y_Registers/c8y_Coils into points.
{ "c8y_ModbusDevice": {
    "protocol": "TCP", "address": 1, "ipAddress": "10.0.0.9",
    "id": "<child MO id>", "name": "plc-9",
    "type": "/inventory/managedObjects/<c8y_ModbusDeviceType MO id>"
} }

// c8y_ModbusDevice, shape 2: a device shaped like a [[device]] config entry (passed through)
{ "c8y_ModbusDevice": { "device": {
    "name": "plc-9",
    "protocol_address": { "transport": "tcp", "host": "10.0.0.9", "port": 502, "unit_id": 1 },
    "default_mode": "typed",
    "point": [
      { "id": "temp", "datatype": "float32", "access": "read_write",
        "address": { "table": "holding", "address": 7, "count": 2 } }
    ]
} } }
```

`c8y_ModbusDevice` is a `command` shim (the mapper executes
[`c8y-fieldbus-import`](c8y-fieldbus-import), legacy-plugin style) rather than an
`[exec.workflow]` mapping: the Cloud Fieldbus path needs HTTP against the mapper's local c8y
proxy, which the tedge flows JS runtime cannot do (no `fetch`/`XMLHttpRequest` — see the
status update in [RFC 0002](../doc/rfc/0002-cloud-fieldbus-integration.md)). The register
translation rules (datatype, transform, `meta.measurement` naming, coils) are documented in
the script header and unit-tested offline by
[`cloud/modbus/tests/test_fieldbus_import.sh`](../cloud/modbus/tests/test_fieldbus_import.sh).

```jsonc
// c8y_ParameterUpdate — sent by the Cumulocity "Parameters" tab for one parameter set. The set
// name (<set>) is the Digital Twin Manager identifier; the whole operation is passed to the
// parameter_update command as `operation` and ot-command-forward turns it into one write-batch.
{ "c8y_ParameterUpdate": {}, "c8y_ParameterUpdate_acme_meter_v2_control_parameters": {},
  "acme_meter_v2_control_parameters": { "temp_u16": 4343, "coil_rw": true } }
```

Device parameters are protocol-neutral and need no operation file in this repository: the
c8y mapper binds `.template` files **by fragment name**, so only one template may exist for
`c8y_ParameterUpdate`, and that one belongs to the
[tedge-parameter-plugin](https://github.com/thin-edge/tedge-parameter-plugin) (install it
alongside tedge-dot). Its template maps the operation onto the `parameter_update` command with
the whole operation as `operation`. `ot-registration` advertises `parameter_update` on every OT
child device (a retained `{}` on `te/device/<device>///cmd/parameter_update`), the mapper
symlinks the plugin's template for the child, and the OT flows handle the resulting command —
nothing else does, because tedge-agent only runs workflows for its own entity. On the main
device the plugin's own workflow and parameter-set scripts keep working untouched. The
Parameters tab only renders sets that have a Digital Twin Manager property definition, which a
tenant admin registers once (the device never calls the DTM service — device users lack the
roles anyway).
`tedge-dot describe` prints exactly those definitions from the connector configs — by default
every config in `/etc/tedge/plugins/ot`, the directory the service runs, with a set shared by
several of them rendered once (`-c <file-or-dir>`, repeatable, narrows or widens that):

```sh
# One definition per line, and the DTM service takes one per request — a configuration that
# groups its parameters (§5.2) renders several. `</dev/null` matters: without it c8y reads the
# loop's stdin as its own input pipeline and the remaining definitions are never registered.
defs=$(mktemp) one=$(mktemp)
trap 'rm -f "$defs" "$one"' EXIT
tedge-dot describe --compact > "$defs"
# Read from a file, not a pipe: a `while` on the right of a pipe runs in a subshell, where
# `exit 1` would abort only the loop and leave the script reporting success.
while read -r definition; do
    printf '%s' "$definition" > "$one"
    C8Y_SETTINGS_CI=true c8y api POST /service/dtm/definitions/properties --data "@$one" </dev/null ||
        exit 1   # stop at the first rejected definition rather than reporting only the last
done < "$defs"
```

(or create it by hand in the DTM UI: identifier = the set name, one property per point id).

A DTM identifier is **tenant-wide**, so the set name is derived from the device *type* the
configuration declares — `<type>_<group>_parameters`, e.g. `acme_meter_v2_control_parameters`
(contract §5.2). Give every device a `type` (on the `[[device]]`, or once in the point library
it references): without one the sets fall back to `<protocol>_control_parameters`, which every
other device type on that protocol also falls back to, and the definitions would overwrite each
other in the tenant. `describe` warns when a device exposes parameters without a type.

Renaming a set leaves the old twin fragment retained under its old name (and mirrored into the
managed object), so clear it once per device after changing a name:

```sh
tedge mqtt pub -r -q 1 'te/device/plc1///twin/modbus_parameters' ''
```

> The legacy operations carried raw register/coil addresses and per-register scaling. Those now
> live in connector config (point `address`) and flows (scaling), so the cloud-facing operation
> only needs the logical point id and value. Adapt the `input.*` jq expressions in each shim if
> your Cumulocity operation templates use different field names.

## Deploy

Copy the shim files into the device's Cumulocity operations directory and the bridge flows into the
c8y mapper. Operations that target the OT child devices (`c8y_SetRegister`, `c8y_SetCoil`)
must be installed as **templates** (`.template` suffix): the mapper instantiates a template per
child device that advertises the matching `ot_*` command capability, which `ot-registration`
publishes. Gateway-level operations (`c8y_ModbusDevice`, `c8y_ModbusConfiguration`,
`c8y_SerialConfiguration`) are installed as plain files so they land in the main device's
supported operations. Device parameters need the tedge-parameter-plugin package, nothing from
here ([`cloud/modbus/Dockerfile.tedge`](../cloud/modbus/Dockerfile.tedge) is the tested layout):

```sh
sudo apt-get install tedge-parameter-plugin   # thin-edge community repo: owns c8y_ParameterUpdate
sudo cp operations/c8y_SetRegister /etc/tedge/operations/c8y/c8y_SetRegister.template
sudo cp operations/c8y_SetCoil     /etc/tedge/operations/c8y/c8y_SetCoil.template
sudo cp operations/c8y_ModbusDevice operations/c8y_ModbusConfiguration operations/c8y_SerialConfiguration /etc/tedge/operations/c8y/
sudo install -m 0755 operations/c8y-fieldbus-import /usr/bin/c8y-fieldbus-import  # needs jq + curl
# The three flows below are already in place on a packaged install; from a
# source checkout, copy them yourself (see ../flows/README.md):
sudo cp -Ra flows/ot-command-forward flows/ot-command-result /etc/tedge/mappers/c8y/flows/
sudo cp -Ra flows/ot-parameter-state /etc/tedge/mappers/c8y/flows/
```

Set each flow's `params.toml` `protocol` (forward) / `command_prefix` (result) if you are not using
the defaults (`modbus` / `ot_`).
