# thin-edge.io flows for OT connectors

These flows convert between the **OT connector format** (the connector's `sample`/`cmd`/`status`
envelopes on `te/device/<device>/ot/<protocol>/...`) and the **thin-edge.io data model**
(`m`/`a`/`cmd` and entity registration on `te/device/<device>///...`, see the
[MQTT API](https://thin-edge.github.io/thin-edge.io/references/mqtt-api/)).

They are **protocol-neutral**: every flow consumes the generic OT Connector Contract envelopes, so
the *same flow set* maps `modbus`, `opcua`, or any other connector built on the SDK. The
measurement group, child-device type and alarm group are derived from the connector's
`protocol`/topic, so a new protocol needs no new flows.

The connector is a "dumb" driver: it reads/writes the OT protocol, decodes primitives and applies
the per-signal properties declared on each point (linear scaling and engineering unit). All
naming, alarms, registration and operation shaping live here, in
[thin-edge.io flows](https://thin-edge.github.io/thin-edge.io/extend/flows/) — small JavaScript
modules that run inside a mapper and are hot-reloaded without restarts.

## The flows

| Flow | Direction | Reads | Emits |
| --- | --- | --- | --- |
| [ot-measurement](ot-measurement/) | OT → thin-edge | `ot/<protocol>/sample/<point>` | `m/<group>` measurement |
| [ot-alarm](ot-alarm/) | thin-edge → thin-edge | `m/<group>` | `a/<type>` alarm (hysteresis) |
| [ot-event](ot-event/) | thin-edge → thin-edge | `m/<group>` | `e/<type>` event (on change) |
| [ot-registration](ot-registration/) | OT → thin-edge | `ot/<protocol>/status/link` | `te/device/<device>//` child registration (+ optional `twin/<fragment>`) |
| [ot-command-forward](ot-command-forward/) | thin-edge → OT | `cmd/ot_<verb>/<id>` | `ot/<protocol>/cmd/<verb>/<id>` |
| [ot-command-result](ot-command-result/) | OT → thin-edge | `ot/<protocol>/cmd/<verb>/<id>` | `cmd/ot_<verb>/<id>` |

The two `ot-command-*` flows form a bidirectional, **verb-neutral** bridge: *forward* turns a
thin-edge command into a connector command request; *result* mirrors the connector's `executing` →
`successful`/`failed` transitions back so the thin-edge command (and any bound cloud operation)
completes. They are split into two flows because a single flow may not both consume and produce
on its own input topics (the mapper drops such outputs to prevent loops).

The thin-edge command type maps to a connector verb by dropping the `ot_` prefix and turning `_`
into `-`. The verbs cover the legacy Cumulocity operations (see the
[migration guide](../../doc/proposal/migration/migration-guide.md)):

| thin-edge command | connector verb | replaces (legacy operation) |
| --- | --- | --- |
| `ot_write` | `write` | `c8y_SetRegister` |
| `ot_write_coil` | `write-coil` | `c8y_SetCoil` |
| `ot_set_config` | `set-config` | `c8y_ModbusConfiguration`, `c8y_SerialConfiguration` |
| `ot_define_device` | `define-device` | `c8y_ModbusDevice`, `c8y_Coils`, `c8y_Registers` |
| `ot_remove_device` | `remove-device` | — |

The `write` verb is implemented by the protocol module; the `set-config`/`define-device`/
`remove-device` management verbs are implemented once by the SDK runtime (it owns the connector
configuration), so every connector supports them. `ot-command-forward` subscribes to an explicit
allow-list of `ot_*` command types (add a line to its `flow.toml` to support a new verb).

By default `ot-measurement` names the measurement group after the sample's `protocol`
(`m/modbus`, `m/opcua`, ...), `ot-registration` types the child device as `<protocol>-device`,
and `ot-alarm`/`ot-event` follow whatever `m/<group>` they are fed. Override any of these via each
flow's `params.toml`.

To remap individual signals to specific groups/series with a single flow instance, name the
connector points with a separator and set `point_separator` (e.g. `"."`): the point id
`Environment.Temperature` then becomes group `Environment`, series `Temperature`. Explicit
`group`/`series` still win, and an empty `point_separator` (the default) leaves dotted ids
untouched. For per-signal shaping beyond this convention, run one filtered instance per signal
(set `point`) or copy the flow and customise `main.js`.

`ot-measurement` also covers the legacy register mapping options: publish-on-change
(`on_change`) and batching a device's series into one measurement (`combine` + `combine_interval`).
Linear scaling
(`multiplier`/`divisor`/`decimal_shift`/`offset`) is a per-point property declared on the
connector point (applied by the SDK), so the sample already carries the scaled value.
`ot-registration` can additionally publish the connector's device descriptor as a digital-twin
fragment (`twin_fragment`, e.g. `c8y_ModbusDevice`).

## Pipeline

```text
 OT device                    tedge-dot (driver)            flows (this dir)            cloud mapper
 ───────────────   reads ──▶  ot/<protocol>/sample/<point> ──▶  ot-measurement ──▶  m/<group>  ──▶  measurement
                             ot/<protocol>/status/link      ──▶  ot-registration ─▶ te/device/x// ─▶ child device
                                                                 m/<group> ──▶ ot-alarm ──▶ a/<type> ──▶ alarm

 cloud operation  ──▶  cmd/ot_<verb>/<id>  ──▶ ot-command-forward ──▶ ot/<protocol>/cmd/<verb>/<id> ──▶ driver acts
 driver result    ──▶  ot/<protocol>/cmd/<verb>/<id> ─▶ ot-command-result ──▶ cmd/ot_<verb>/<id> (operation completes)
```

## Configure

Each flow ships a `params.toml.template` documenting its settings. To customise, copy it to
`params.toml` in the same directory and edit. With the defaults, `ot-measurement` maps every
good numeric point into an `m/<protocol>` measurement whose series is the point id — zero config.

## Test (offline, no broker/device/cloud)

```sh
just test-flows          # runs flows/test-flows.sh (covers modbus and opcua samples)
# or a single case:
echo '[te/device/plc1/ot/modbus/sample/level_f32] {"ts":"2026-05-30T10:00:00.000Z","device":"plc1","protocol":"modbus","point":"level_f32","mode":"typed","datatype":"float32","value":404.17,"value_repr":"number","raw":"43ca 15c3","quality":"good","addr":{}}' \
  | tedge flows test --flows-dir ./flows/ot-measurement/
```

## Deploy

Copy the flow directories into a mapper's flows folder; they are picked up and hot-reloaded:

```sh
sudo cp -Ra flows/ot-measurement /etc/tedge/mappers/c8y/flows/
sudo cp -Ra flows/ot-registration /etc/tedge/mappers/c8y/flows/
# ...and the others as needed
```

Or package a flow as a `*.tar.gz` and install it via Cumulocity software management using the
`<mapper>/<flow>` name (e.g. `c8y/ot-measurement`), as described in the
[flows guide](https://thin-edge.github.io/thin-edge.io/extend/flows/#installing-flows).
