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
| [ot-command-forward](ot-command-forward/) | thin-edge → OT | `cmd/ot_<verb>/<id>` (incl. `parameter_update`) | `ot/<protocol>/cmd/<verb>/<id>`, or `service/<service>/ot/cmd/<verb>/<id>` for management verbs |
| [ot-command-result](ot-command-result/) | OT → thin-edge | `ot/<protocol>/cmd/<verb>/<id>`, `service/<service>/ot/cmd/<verb>/<id>` | `cmd/ot_<verb>/<id>` (or the `origin.command`) |
| [ot-parameter-state](ot-parameter-state/) | OT → thin-edge | `sample/<point>`, `cmd/write*/<id>`, `status/link` | `twin/<set>` |

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
| `parameter_update` | `write-batch` (reshaped by `ot-command-forward`) | `c8y_ParameterUpdate` (device parameters; template owned by tedge-parameter-plugin) |

The `write` verb is implemented by the protocol module; the `set-config`/`define-device`/
`remove-device` management verbs are implemented once by the SDK runtime (it owns the connector
configuration), so every connector supports them. `ot-command-forward` subscribes to an explicit
allow-list of `ot_*` command types (add a line to its `flow.toml` to support a new verb).

Management verbs change one connector instance's configuration, so `ot-command-forward` sends
them to that instance's service topic (`te/device/main/service/<service>/ot/cmd/<verb>/<id>`,
contract §6.3). The service is the command's `service` field, else `tedge-dot-<protocol>` — the
default `service_name` of a connector config — so name the service whenever the connector's config
sets its own `service_name`, e.g. when a gateway runs several connectors of one protocol. A
command whose `service` is not a plain topic level is **not forwarded**: the flow cannot fail it
(its output would match its own input), so it stays pending.

**Device parameters** (see [RFC 0003](../doc/rfc/0003-parameter-writes.md)): writable points are
parameters. `ot-parameter-state` keeps one retained twin fragment per *parameter set*
(`te/device/<device>///twin/<set>`, keyed by point id) current from the samples (which echo each
point's `access`) and from acknowledged writes. It also drops a point from the twin when the
retained link status no longer lists it (a reload removed it) or its latest sample no longer
names that set, and clears a set left empty: Cumulocity sends the whole fragment back with an
edit, so a stale key would fail every update of the set. `ot-command-forward` reshapes a
`parameter_update` command — the command type of the
[tedge-parameter-plugin](https://github.com/thin-edge/tedge-parameter-plugin), whose
`c8y_ParameterUpdate.template` maps the Cumulocity operation onto it — into ONE connector
`write-batch`, and `ot-command-result` completes it through the batch request's `origin.command`.
The plugin's own workflow only serves the main device (tedge-agent runs workflows for its own
entity only), so on OT child devices the flows are the sole handler and no second template is
needed: the c8y mapper binds templates per fragment name, so two templates for
`c8y_ParameterUpdate` could never coexist.
A set name is a tenant-wide identifier in the cloud, so it is derived from the **device type**
(echoed in every sample and on the retained link status) rather than from the protocol:
`<type, else protocol>_<meta.parameter.group, default "control">_parameters`, e.g.
`acme_meter_v2_control_parameters`. Both `group` and `set` accept a list, so one point can be in
several sets and its value is published to each of their fragments. `meta.parameter.set` still
names a set outright, and the flow's `default_set` param forces one name for everything. `tedge-dot describe` derives the same
names from the same configuration and renders them as Cumulocity DTM definitions for a tenant
admin to register — see [RFC 0005](../doc/rfc/0005-device-types-and-parameter-sets.md).
A parameter is still an ordinary signal otherwise, so by default its samples also become
measurements through `ot-measurement`. To keep its value on the twin fragment only, and not also
as a measurement series, set `meta.measurement = false` on the point: it stays a parameter and
keeps being sampled. `ot-alarm` and `ot-event` work from measurements, so they no longer see
such a point either.

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

 c8y_ParameterUpdate ─▶ cmd/parameter_update/<id> ─▶ ot-command-forward ─▶ ot/<protocol>/cmd/write-batch/<id> ─▶ driver writes N points
 driver result       ─▶ ot/<protocol>/cmd/write-batch/<id> ─▶ ot-command-result ─▶ cmd/parameter_update/<id> (operation completes)
 samples + write results ─▶ ot-parameter-state ─▶ te/device/<device>///twin/<set> ─▶ Parameters tab
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

The `tedge-dot` packages (both the Rust and the C build) already ship these flows, so on a
packaged install there is nothing to copy. The core pipeline is deployed **active**, into the
Cumulocity mapper's flows directory:

```
/etc/tedge/mappers/c8y/flows/ot-measurement/
/etc/tedge/mappers/c8y/flows/ot-registration/
/etc/tedge/mappers/c8y/flows/ot-command-forward/
/etc/tedge/mappers/c8y/flows/ot-command-result/
/etc/tedge/mappers/c8y/flows/ot-parameter-state/
```

`ot-alarm` and `ot-event` only mean something once a threshold or an event type has been chosen
for a specific signal, so they ship inert in `/usr/share/tedge-dot/flows/`. Opt one in by copying
it across and giving it a `params.toml`:

```sh
sudo cp -Ra /usr/share/tedge-dot/flows/ot-alarm /etc/tedge/mappers/c8y/flows/
sudo cp /etc/tedge/mappers/c8y/flows/ot-alarm/params.toml.template \
        /etc/tedge/mappers/c8y/flows/ot-alarm/params.toml
sudo -u tedge $EDITOR /etc/tedge/mappers/c8y/flows/ot-alarm/params.toml
```

Either way the mapper picks the change up and hot-reloads — no restart. Only the flow logic
(`flow.toml`, `main.js`) and the `params.toml.template` are packaged; the `params.toml` you write
next to them is not, so a package upgrade replaces the logic and leaves your settings alone.

From a source checkout, or to target a different mapper, copy the directories yourself:

```sh
sudo cp -Ra flows/ot-measurement /etc/tedge/mappers/c8y/flows/
sudo cp -Ra flows/ot-registration /etc/tedge/mappers/c8y/flows/
sudo cp -Ra flows/ot-parameter-state /etc/tedge/mappers/c8y/flows/
# ...and the others as needed
```

Or package a flow as a `*.tar.gz` and install it via Cumulocity software management using the
`<mapper>/<flow>` name (e.g. `c8y/ot-measurement`), as described in the
[flows guide](https://thin-edge.github.io/thin-edge.io/extend/flows/#installing-flows).
