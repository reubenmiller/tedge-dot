# Cumulocity operation shims for the OT connector

These files translate the legacy Cumulocity operations the Python Modbus plugin shipped (under
the repo-root [`operations/`](../../operations/)) into the **generic OT command model** of the
new connector. They are the Cumulocity-specific glue: each one maps a cloud operation onto a
protocol-neutral `ot_<verb>` thin-edge command. The [`ot-command-forward`](../flows/) flow then
bridges that command to the connector's `cmd/<verb>` topic, the connector (or the SDK runtime, for
management verbs) acts on it, and [`ot-command-result`](../flows/) mirrors the result back so the
cloud operation completes.

```text
 c8y operation ─▶ c8y-mapper ─▶ cmd/ot_<verb> ─▶ ot-command-forward ─▶ ot/<protocol>/cmd/<verb> ─▶ connector
                                       ▲                                                                │
 operation SUCCESSFUL ◀── c8y-mapper ──┴──────────────── ot-command-result ◀───────────────────────────┘
```

## Mapping

| Legacy operation | Shim file | Generic command | Verb | Handled by |
| --- | --- | --- | --- | --- |
| `c8y_SetRegister` | [`c8y_SetRegister`](c8y_SetRegister) | `ot_write` | `write` | protocol module |
| `c8y_SetCoil` | [`c8y_SetCoil`](c8y_SetCoil) | `ot_write` | `write` | protocol module |
| `c8y_ModbusConfiguration` | [`c8y_ModbusConfiguration`](c8y_ModbusConfiguration) | `ot_set_config` | `set-config` | SDK runtime |
| `c8y_SerialConfiguration` | [`c8y_SerialConfiguration`](c8y_SerialConfiguration) | `ot_set_config` | `set-config` | SDK runtime |
| `c8y_ModbusDevice` (+ `c8y_Coils`/`c8y_Registers`) | [`c8y_ModbusDevice`](c8y_ModbusDevice) | `ot_define_device` | `define-device` | SDK runtime |

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

// c8y_ModbusDevice  (device shaped like a [[device]] config entry)
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

> The legacy operations carried raw register/coil addresses and per-register scaling. Those now
> live in connector config (point `address`) and flows (scaling), so the cloud-facing operation
> only needs the logical point id and value. Adapt the `input.*` jq expressions in each shim if
> your Cumulocity operation templates use different field names.

## Deploy

Copy the shim files into the device's Cumulocity operations directory and the bridge flows into the
c8y mapper:

```sh
sudo cp operations/c8y_* /etc/tedge/operations/c8y/
sudo cp -Ra flows/ot-command-forward flows/ot-command-result /etc/tedge/mappers/c8y/flows/
```

Set each flow's `params.toml` `protocol` (forward) / `command_prefix` (result) if you are not using
the defaults (`modbus` / `ot_`).
