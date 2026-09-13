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
| [ot-measurement](ot-measurement/) | OT → thin-edge | `ot/<protocol>/sample/<point>`, `ot/<protocol>/manifest` (per-signal `meta`) | `m/<group>` measurement |
| [ot-alarm](ot-alarm/) | thin-edge → thin-edge | `m/<group>` | `a/<type>` alarm (hysteresis) |
| [ot-event](ot-event/) | thin-edge → thin-edge | `m/<group>` | `e/<type>` event (on change) |
| [ot-registration](ot-registration/) | OT → thin-edge | `ot/<protocol>/status/link` (trigger + type), `ot/<protocol>/manifest` (descriptor) | `te/device/<device>//` child registration (+ optional `twin/<fragment>`) |
| [ot-parameter-update](ot-parameter-update/) | thin-edge → thin-edge | `cmd/parameter_update/<id>` | `cmd/ot_write_batch/<id>` |
| [ot-parameter-result](ot-parameter-result/) | thin-edge → thin-edge | `cmd/ot_write_batch/<id>` | `cmd/parameter_update/<id>` (the batch's `origin.command`) |
| [ot-parameter-state](ot-parameter-state/) | OT → thin-edge | `manifest` (which points are parameters, and their sets), `sample/<point>`, `cmd/write*/<id>` | `twin/<set>` |

There is no command *bridge* any more (RFC 0006 §7): the connector subscribes to the thin-edge
command topics and drives them itself, so a write is one command on one topic with one state
machine. The only pair left is the parameter bridge, which exists because
`c8y_ParameterUpdate` is a *different* command — one cloud operation carrying a whole set — that
has to be reshaped into one `ot_write_batch`: *update* sends the batch, *result* completes the
operation from it. Two flows rather than one because a flow may not publish to a topic matching
its own input filter (the mapper drops such outputs to prevent loops).

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
| `ot_write_batch` | `write-batch` | `c8y_ParameterUpdate` (device parameters, via `ot-parameter-update`; template owned by tedge-parameter-plugin) |

The `write` verb is implemented by the protocol module; the `set-config`/`define-device`/
`remove-device` management verbs are implemented once by the SDK runtime (it owns the connector
configuration), so every connector supports them. A connector advertises the command types each
device answers on its manifest (contract §8.2) and marks them retained on
`te/device/<device>///cmd/<type>`, so nothing carries a hard-coded list.

Management verbs change one connector instance's configuration, so the instance is named in the
command's `service` field (contract §6.6), defaulting to `tedge-dot-<protocol>` — the default
`service_name` of a connector config. Name it whenever a config sets its own `service_name`,
e.g. when a gateway runs several connectors of one protocol: the device segment cannot say
which instance is meant, because a Cloud Fieldbus operation arrives on the gateway's topic and
the gateway is nobody's configured device.

**Device parameters** (see [RFC 0003](../doc/rfc/0003-parameter-writes.md)): writable points are
parameters. `ot-parameter-state` keeps one retained twin fragment per *parameter set*
(`te/device/<device>///twin/<set>`, keyed by point id) current from the samples (which echo each
point's `access`) and from acknowledged writes. `ot-parameter-update` reshapes a
`parameter_update` command — the command type of the
[tedge-parameter-plugin](https://github.com/thin-edge/tedge-parameter-plugin), whose
`c8y_ParameterUpdate.template` maps the Cumulocity operation onto it — into ONE
`ot_write_batch`, and `ot-parameter-result` completes it through the batch's `origin.command`.
The plugin's own workflow only serves the main device (tedge-agent runs workflows for its own
entity only), so on OT child devices the flows are the sole handler and no second template is
needed: the c8y mapper binds templates per fragment name, so two templates for
`c8y_ParameterUpdate` could never coexist.
A set name is a tenant-wide identifier in the cloud, so it is derived from the **device type**
rather than from the protocol: `<type, else protocol>_<parameter.group, default "control">_parameters`,
e.g. `acme_meter_v2_control_parameters`. Both `group` and `set` accept a list, so one point can
be in several sets and its value is published to each of their fragments; `parameter.set` names
a set outright, and `[connector] parameter_set` forces one name for everything.
The flow derives none of this: the **connector** resolves the names and publishes them on the
device manifest (contract §8.2), the flow reads them there, and
`tedge-dot manifest --format c8y-dtm` renders the same manifests as Cumulocity DTM definitions
for a tenant admin to register — see
[RFC 0005](../doc/rfc/0005-device-types-and-parameter-sets.md).
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
`ot-registration` can additionally publish the connector's device descriptor (the manifest's
`info`) as a digital-twin fragment (`twin_fragment`, e.g. `c8y_ModbusDevice`).

Every static fact about a device — its type, and each point's datatype, access, unit, labels,
free-form `meta` and resolved parameter sets — is published once, retained, on the **device
manifest** `te/device/<device>/ot/<protocol>/manifest` (contract §8.2). The flows keep the last
manifest of each device in the shared mapper state (`ot-manifest:<device>`) and look points up
there, so a sample carries only what changes per read. Offline, `tedge flows test` is fed the
manifest line first, as the broker replays the retained message on a restart.

## Pipeline

```text
 OT device                    tedge-dot (driver)            flows (this dir)            cloud mapper
 ───────────────   reads ──▶  ot/<protocol>/sample/<point> ──▶  ot-measurement ──▶  m/<group>  ──▶  measurement
                             ot/<protocol>/manifest         ──▶  (kept by the flows: per-signal meta, parameter sets, descriptor)
                             ot/<protocol>/status/link      ──▶  ot-registration ─▶ te/device/x// ─▶ child device
                                                                 m/<group> ──▶ ot-alarm ──▶ a/<type> ──▶ alarm

 cloud operation  ──▶  cmd/ot_<verb>/<id>  ──────────────▶ the DRIVER answers it in place (no flow)

 c8y_ParameterUpdate ─▶ cmd/parameter_update/<id> ─▶ ot-parameter-update ─▶ cmd/ot_write_batch/<id> ─▶ driver writes N points
 batch result        ─▶ cmd/ot_write_batch/<id>   ─▶ ot-parameter-result ─▶ cmd/parameter_update/<id> (operation completes)
 samples + write results ─▶ ot-parameter-state ─▶ te/device/<device>///twin/<set> ─▶ Parameters tab
```

## Configure

Each flow with settings ships a `params.toml.template` documenting them. To customise, copy it
to `params.toml` in the same directory and edit. A flow with nothing to configure ships no
template — `ot-parameter-state` reads everything it needs off the device manifest. With the defaults, `ot-measurement` maps every
good numeric point into an `m/<protocol>` measurement whose series is the point id — zero config.

## Test (offline, no broker/device/cloud)

```sh
just test-flows          # runs flows/test-flows.sh (covers modbus and opcua samples)
# or a single case:
echo '[te/device/plc1/ot/modbus/sample/level_f32] {"ts":"2026-05-30T10:00:00.000Z","device":"plc1","protocol":"modbus","point":"level_f32","mode":"typed","datatype":"float32","value":404.17,"quality":"good"}' \
  | tedge flows test --flows-dir ./flows/ot-measurement/
```

## Deploy

The `tedge-dot` packages (both the Rust and the C build) already ship these flows, so on a
packaged install there is nothing to copy. The core pipeline is deployed **active**, into the
Cumulocity mapper's flows directory:

```
/etc/tedge/mappers/c8y/flows/ot-measurement/
/etc/tedge/mappers/c8y/flows/ot-registration/
/etc/tedge/mappers/c8y/flows/ot-parameter-update/
/etc/tedge/mappers/c8y/flows/ot-parameter-result/
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
