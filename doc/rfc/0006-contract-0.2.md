# RFC 0006: Contract 0.2 — a slimmer envelope, a device manifest, and typed signal metadata

Status: proposed — **for review, one decision per section**. Nothing here is implemented.

| Field | Value |
| --- | --- |
| RFC | 0006 |
| Supersedes | Parts of the [OT Connector Contract](../contract/ot-connector-contract.md) 0.1.0 (§2, §3.1, §5, §6, §7) |
| Related | [RFC 0001](0001-ot-connector-architecture.md), [RFC 0003](0003-parameter-writes.md), [RFC 0004](0004-point-libraries.md), [RFC 0005](0005-device-types-and-parameter-sets.md) |
| Prompted by | A review of the contract against the concepts of Anthropic's [Model Hardware Standard](https://www.anthropic.com/news/model-hardware-standard-research-preview) (MHS) |

## How to read this RFC

Every numbered section is an independent proposal with the same shape:

- **Today** — what the 0.1 contract does, with a real example from this repository.
- **Proposed** — the 0.2 replacement, with the same example rewritten.
- **Why** — the argument, and what MHS does where it is relevant.
- **Breaks** — what a user, a flow or an implementation has to change.
- **Feedback** — a line to fill in. A section can be accepted, amended or rejected on its own;
  the dependencies between them are listed in §11.

The comparison with MHS is against what Anthropic and the press have described, not a
published schema — the MHS specification is a closed research preview (September 2026). What
is public: a driver exposing `read`/`write` plus discovery; a per-device *reference file* (a
manifest) of **states** (what the device measures, what can be adjusted) and **procedures**;
safety limits enforced *in the driver*, below the agent; natural-language *tags* compiled into
that reference file; and three access paths — MCP, a CLI, and code. Its consumer is an agent
reasoning over the manifest. tedge-dot's consumer is a flow and a cloud mapper running
unattended. The audiences differ; the device-facing half overlaps almost entirely.

## 0. What this RFC does not change

For the avoidance of doubt, these stay exactly as they are, and each was found to be in good
shape by the same review:

- the driver/flow split of [RFC 0001](0001-ot-connector-architecture.md): the connector
  decodes primitives and applies the declared per-point transform, and *nothing else*;
- point libraries and device types ([RFC 0004](0004-point-libraries.md),
  [RFC 0005](0005-device-types-and-parameter-sets.md)) — this is exactly MHS's "one reference
  file per device type", already done well;
- primitive decoding in the SDK, the golden vectors, and the conformance layering;
- the `read`/`write` CLI driving the same code path as the service (MHS's "CLI path");
- the management verbs `set-config`, `define-device`, `remove-device` and their persistence
  and live-reload semantics (their *topic* changes in §6);
- the liveness model (`operation_timeout`, `stall_timeout`);
- the command state machine `init → executing → successful | failed`.

---

## 1. Collapse `mode` into `datatype`

### Today

A point has a `mode` (`raw` or `typed`) *and*, when typed, a `datatype`. Raw mode publishes
hex and no value; writes to a raw point carry `raw` instead of `value`.

```toml
[[device.point]]
id      = "run_command"
mode    = "raw"
access  = "read_write"
address = { table = "coil", address = 0, count = 1 }

[[device.point]]
id       = "boiler_temp"
mode     = "typed"
datatype = "float32"
address  = { table = "holding", address = 7, count = 2 }
```

```json
{ "point": "run_command", "mode": "raw", "raw": "0001", "quality": "good", "addr": { "table": "coil", "address": 0, "unit_id": 1 } }
```

```json
{ "status": "init", "point": "run_command", "raw": "0001" }
```

Raw mode is used by two conformance configurations
([connectors/modbus/conformance/connector.toml](../../connectors/modbus/conformance/connector.toml),
[connectors/opcua/conformance/connector.toml](../../connectors/opcua/conformance/connector.toml))
and by nothing else in the repository: no demo, no packaged default, no e2e or cloud suite.

### Proposed

`datatype` is always required. `bytes` *is* the raw case: the value is the hex string, read and
written like any other value. `mode`, `device.default_mode`, the `raw` write field and the
`modes` capability disappear.

```toml
[[device.point]]
id       = "run_command"
datatype = "bytes"
access   = "read_write"
address  = { table = "coil", address = 0, count = 1 }

[[device.point]]
id       = "boiler_temp"
datatype = "float32"
address  = { table = "holding", address = 7, count = 2 }
```

```json
{ "point": "run_command", "datatype": "bytes", "value": "0001", "quality": "good" }
```

```json
{ "status": "init", "point": "run_command", "value": "0001" }
```

A connector that cannot deliver undecoded bytes for a point kind (an OPC UA node has no wire
bytes) rejects `bytes` for that kind at configuration time, exactly as it rejects any datatype
it does not list in its capabilities.

### Why

Two type systems for one point cost a field on every point, `default_mode` on every device,
four conditional rules in `sample.schema.json`, two write-request shapes, and a `modes` list
in the capability descriptor — for a feature nothing uses. `bytes` already exists as a datatype;
"raw" is the same thing with a different spelling. MHS has one notion, *read a state*, with a
type; so should the contract.

### Breaks

- Configuration: `mode` and `default_mode` are rejected as unknown keys (§3.3 already makes
  that loud). Existing configs carry neither except the two conformance files.
- Sample and command schemas: `mode` and `raw` (as a request field) removed; see §2.
- Both SDKs, the conformance suite (the raw checks become `bytes` checks).

> **Feedback:** ☐ accept ☐ accept with changes ☐ reject — notes:

---

## 2. Slim the sample envelope

### Today

A good sample for the demo's `temp_scaled` point, with every field the 0.1 runtime sets:

```json
{
  "ts": "2026-09-13T10:00:00.000Z",
  "ts_ms": 1789380000000.0,
  "device": "plc1",
  "type": "modbus-demo-sim",
  "protocol": "modbus",
  "point": "temp_scaled",
  "mode": "typed",
  "datatype": "uint16",
  "value": 17.001,
  "value_repr": "number",
  "raw": "4269",
  "quality": "good",
  "unit": "°C",
  "access": "read",
  "addr": { "table": "holding", "address": 3, "unit_id": 1 },
  "seq": 12407,
  "meta": { "on_change": true, "deadband": 0.5, "min_interval": "10s" }
}
```

Seventeen fields. Of those, `type`, `unit`, `access`, `meta`, `addr` and (per §1) `mode` are
**static per point**: they say the same thing on every one of the thousands of samples a point
publishes in a day. `value_repr` restates what JSON already says about `value`. `raw` is a
debugging aid. `ts_ms` is `ts` again. They are all there so that a flow never has to know the
configuration — a goal §3 meets differently.

### Proposed

A sample is a time series row: identity, value, quality.

```json
{
  "ts": "2026-09-13T10:00:00.000Z",
  "device": "plc1",
  "protocol": "modbus",
  "point": "temp_scaled",
  "datatype": "uint16",
  "value": 17.001,
  "quality": "good",
  "seq": 12407
}
```

A failed read:

```json
{
  "ts": "2026-09-13T10:00:02.000Z",
  "device": "plc1",
  "protocol": "modbus",
  "point": "bad_point",
  "datatype": "uint16",
  "quality": "bad",
  "error": "modbus exception: illegal data address"
}
```

| Field | 0.1 | 0.2 | Where it went |
| --- | --- | --- | --- |
| `ts`, `device`, `point`, `quality`, `seq`, `error` | yes | yes | unchanged |
| `protocol` | yes | yes | kept — a fact about the sample's origin, cheap, and what `ot-measurement` groups by (§5 removes it from the *topic*, not the payload) |
| `datatype` | when typed | always | kept — one short string; tells a consumer that a `string`-typed `value` is an `int64` outside the safe range (§4.1 of the contract) without a second field |
| `value` | typed + good | good | unchanged; a `bytes` value is the hex string (§1) |
| `mode` | yes | — | removed (§1) |
| `value_repr` | yes | — | removed; `datatype` + JSON type carry the same information |
| `raw`, `addr` | yes | opt-in | `[connector] sample_debug = true` adds both; off by default |
| `ts_ms` | optional | — | removed; `Date.parse(sample.ts)` in a flow |
| `type`, `unit`, `access`, `meta` | yes | — | the device manifest (§3) |

`stale` quality is kept unchanged.

### Why

The envelope's stated design goal was "flows never need the configuration file". The 0.1
answer was to echo the configuration in every sample; every new static fact (RFC 0003 added
`access`, RFC 0005 added `type`) grew the envelope again, and `meta` made the growth unbounded.
The 0.2 answer is §3: publish the configuration once, retained, per device. The sample then
shrinks to what changes per read, which is also what every downstream time-series consumer
(the mapper, a historian, a dashboard) actually wants.

MHS makes the same split: the *reference file* describes the device once; reads return values.

### Breaks

- Every flow that reads `sample.meta`, `sample.type`, `sample.access` or `sample.unit`
  (`ot-measurement`, `ot-parameter-state`, `ot-registration` via link status) reads the
  manifest instead (§3 lists the changes).
- `sample.schema.json` loses five properties and all four `allOf` rules.
- Tooling that inspects `raw`/`addr` sets `sample_debug`.

> **Feedback:** ☐ accept ☐ accept with changes ☐ reject — notes:

---

## 3. A retained device manifest

### Today

The static description of a device is scattered across four retained or repeated places, each
added to solve one consumer's need:

| Fact | Published where | Consumer |
| --- | --- | --- |
| point `name`/`description` | `point_labels` in the **capability descriptor** (one per connector, all devices mixed) | cloud UI, `describe` |
| device `type` | every **sample** and the **link status** | `ot-parameter-state`, `ot-registration` |
| point `access` | every **sample** | `ot-parameter-state` |
| point `meta` (parameter sets, publish hints) | every **sample** | `ot-measurement`, `ot-parameter-state` |
| point `unit` | every **sample** | nobody today (the c8y mapper drops units) |
| device descriptor (`info`) | the **link status** | `ot-registration` (twin fragment) |

And because the *set* a parameter belongs to can only be learned from a sample, the flows keep
four kinds of shared state to remember what they have seen
(`ot-protocol:<device>`, `ot-device-type:<device>`, `ot-parameter-set:<device>:<point>`,
`ot-parameter-values:<device>:<set>` in [ot-parameter-state](../../flows/ot-parameter-state/main.js)),
and write-only parameters "land in the default set (no sample ever tells us its meta)".

### Proposed

One retained message per device, published when the device is loaded, republished on every
configuration change that affects it, and cleared (empty retained) by `remove-device`:

```text
te/device/<device>/ot/manifest        (retained)
```

```json
{
  "contract": "0.2",
  "protocol": "modbus",
  "service": "tedge-dot-modbus",
  "type": "modbus-demo-sim",
  "info": { "transport": "tcp", "host": "127.0.0.1", "port": 5020, "unit_id": 1 },
  "points": {
    "temp_u16": {
      "datatype": "uint16",
      "access": "read_write",
      "name": "Temperature setpoint",
      "description": "Target temperature for the boiler loop",
      "range": { "min": 0, "max": 30000 },
      "parameter": { "sets": ["modbus_demo_sim_control_parameters"] }
    },
    "temp_scaled": {
      "datatype": "uint16",
      "access": "read",
      "unit": "°C",
      "name": "Boiler temperature",
      "description": "Measured boiler loop temperature, scaled to °C",
      "publish": { "on_change": true, "deadband": 0.5, "min_interval": "10s" }
    },
    "coil_rw": {
      "datatype": "bool",
      "access": "read_write",
      "name": "Pump enable",
      "parameter": { "sets": ["modbus_demo_sim_control_parameters", "modbus_demo_sim_commissioning_parameters"] }
    }
  },
  "commands": ["ot_write", "ot_write_batch"]
}
```

Notes on the shape:

- `points` is keyed by id — a flow looks a point up, it does not scan a list.
- `parameter.sets` is **already resolved**: the SDK applies the RFC 0005 naming rule once and
  publishes the result. The flow no longer re-derives set names, so the byte-for-byte agreement
  that today's [ot-parameter-state](../../flows/ot-parameter-state/main.js) comment demands of
  three implementations in three languages is required of one.
- `address` is not in the manifest by default (protocol-specific, and the point of the contract
  is that consumers do not need it); `sample_debug = true` includes it, as for `addr` in §2.
- `commands` advertises the thin-edge command types the device answers (§6), so the
  registration flow stops hard-coding `ot_write,ot_write_coil,parameter_update`.
- `meta` (§4) is included verbatim when present — the manifest is where free-form tags belong.

What it replaces:

| Removed | Replaced by |
| --- | --- |
| `point_labels` in the capability descriptor | `points.<id>.name/description` |
| `type`, `access`, `unit`, `meta` in every sample | `type`, `points.<id>.*` |
| `type` and `info` on the link status | `type`, `info` (link status is back to `status`/`since`/`reason`) |
| the four `ot-*:` shared-state families in the flows | `context.mapper.get("manifest:<device>")`, set once from the manifest |
| the capability descriptor being republished after `define-device` | only the affected device's manifest is republished |

The **capability descriptor** stays, and goes back to describing the *build* only
(`protocol`, `version`, `datatypes`, `point_kinds`, `command_verbs`, `features`, `subscribe`),
which is what §7 of the contract originally said it was.

### Why

This is MHS's central idea, and the one tedge-dot is missing: *a reference file per device
that tells a consumer what it can measure, what it can change, and what limits apply.* Every
static echo added in 0.1 was a partial manifest smuggled into a time-series message. Publishing
it once is cheaper on the wire (two hundred labelled points cost ten kilobytes once, not per
sample), simpler for flows (one lookup instead of state reconstruction), and it is the exact
artefact a future MCP server or agent would read (§9).

### Breaks

- Flows: `ot-measurement` reads `publish` hints from the manifest instead of `sample.meta`;
  `ot-parameter-state` reads `parameter.sets` and `access` from it and drops its derivation
  code (roughly 120 of its 259 lines); `ot-registration` subscribes to the manifest instead of
  the link status for `type` and `info`, and advertises `commands` from it.
- Ordering: retained messages on different topics arrive in no defined order after a mapper
  restart, so a flow may see a sample before the manifest. `ot-measurement` falls back to its
  flow-wide defaults for that sample (it does today when `meta` is absent); `ot-parameter-state`
  buffers nothing and simply waits — a parameter value it cannot place is published on the next
  sample after the manifest arrives. See open question 11.2.
- Status schema: `point_labels`, link `type` and `info` removed; a new `manifest` definition.

> **Feedback:** ☐ accept ☐ accept with changes ☐ reject — notes:

---

## 4. Typed signal metadata, and a `range` the driver enforces

### Today

`meta` is defined as "free-form … never interpreted by the connector". In practice it carries
ten keys with fixed meanings, read by flows and by both SDKs' `describe`:

```toml
[[point]]
id       = "temp_u16"
datatype = "uint16"
access   = "read_write"
address  = { table = "holding", address = 3, count = 1 }
meta     = { on_change = true, deadband = 0.5, min_interval = "10s", debounce = "2s",
             measurement = { group = "Environment", series = "Temperature" },
             parameter = { min = 0, max = 30000, title = "Setpoint", group = ["control", "commissioning"] } }
```

| Key | Read by | Validated by |
| --- | --- | --- |
| `on_change`, `deadband`, `min_interval`, `debounce` | `ot-measurement` | nobody — `deadbnad = 0.5` is silently a no-op |
| `measurement.group`, `measurement.series`, `measurement = false` | `ot-measurement` | nobody |
| `parameter` / `parameter = false` / `parameter.{group,set,title,description,min,max,enum,default,order}` | `ot-parameter-state`, `descriptor.rs`, `descriptor.c` | partially, in `describe` only |

Contract §3.3 rejects `polling_interval` for `poll_interval` with a "did you mean" — and
accepts `meta = { on_chnage = true }` without a word. And `min`/`max` are rendered into the
cloud's edit form and then *trusted*: the connector writes whatever arrives.

### Proposed

The conventions become first-class point fields with the same validation as every other key.
`meta` returns to being free-form, is no longer echoed anywhere but the manifest, and is what
MHS calls *tags*.

```toml
[[point]]
id       = "temp_u16"
datatype = "uint16"
access   = "read_write"
address  = { table = "holding", address = 3, count = 1 }

range       = { min = 0, max = 30000 }
publish     = { on_change = true, deadband = 0.5, min_interval = "10s", debounce = "2s" }
measurement = { group = "Environment", series = "Temperature" }     # or: measurement = false
parameter   = { group = ["control", "commissioning"], title = "Setpoint" }  # or: parameter = false

meta = { asset_tag = "B-17", commissioned = "2026-03" }     # yours; nothing reads it
```

| Field | Type | Meaning | Interpreted by |
| --- | --- | --- | --- |
| `range` | `{ min?, max? }` | Engineering-unit bounds of the value **after** `transform`. | **the connector** (below) and the cloud form |
| `publish` | `{ on_change?, deadband?, min_interval?, debounce? }` | Per-signal publish policy. | flows (`ot-measurement`), unchanged semantics |
| `measurement` | `false` or `{ group?, series? }` | Where the signal lands as a measurement. | flows (`ot-measurement`), unchanged semantics |
| `parameter` | `false`, `true`, a string, or `{ group?, set?, title?, description?, enum?, default?, order? }` | Exposure as an operator-editable setting. `min`/`max` move to `range`. | the SDK (manifest `parameter.sets`, `describe`), flows read the resolved result |

**`range` is enforced on write.** A `write` (or a `write-batch` entry) whose value falls
outside the point's `range` fails *before* the device is touched:

```json
{ "status": "init", "point": "temp_u16", "value": 35000 }
```

```json
{ "status": "failed", "point": "temp_u16",
  "reason": "value 35000 outside range [0, 30000] of temp_u16" }
```

In a batch, the check runs for **every** entry before the first write is executed, so an
out-of-range value fails the batch with nothing applied — the only case where a batch failure
is guaranteed to have touched nothing. Reads are *not* altered: a read outside `range` is still
`quality: "good"` (the device really says that), and alarming on it is a flow's job as today.

The check happens in the SDK runtime, once, for every protocol; a module never sees an
out-of-range write. `range` has no effect on a point whose datatype is not numeric, and is
rejected on one at configuration time.

### Why

Two arguments, one for each half.

*Typing the conventions:* the contract's own validation policy ("a misspelt setting would
otherwise be accepted and do nothing") is right, and `meta` is the one place it is not applied
— precisely the place where a site's per-signal behaviour lives. Naming these fields also ends
the pretence that the connector does not interpret `meta`: it already does, in two SDKs.

*Enforcing `range`:* this is the one place MHS is conceptually ahead of tedge-dot. MHS puts
safety limits "in the driver rather than in the prompt", below whatever is asking. In 0.1 the
limit lives in a cloud form, and a write that bypasses the form (a script, another flow, a
typo in an operation) reaches the device. RFC 0001's "dumb driver" rule is the reason it was
left out, but that rule already bent once for `transform`, with the same argument that applies
here: a limit is a property of the signal, not flow logic, and belongs next to the datatype.

### Breaks

- Configuration: `meta.on_change` and friends are still *accepted* (meta is free-form) but no
  longer *do anything* — a silent behaviour change. Mitigation: for one release the loader warns
  when `meta` contains any of the ten legacy keys, naming the field it moved to. The Cloud
  Fieldbus import script writes the new fields directly.
- `meta.parameter.min/max` → `range.min/max` (the import script and `describe` follow).
- Writes that used to succeed and now fail: only those outside a declared `range`, which was
  always the intent of declaring one.
- Both SDKs gain the range check; `descriptor.*` read typed fields instead of walking `meta`.

> **Feedback:** ☐ accept ☐ accept with changes ☐ reject — notes (in particular: enforce `range`
> in the driver, or keep the driver limit-free and leave it to the cloud form?):

---

## 5. Drop `<protocol>` from the topic tree

### Today

```text
te/device/plc1/ot/modbus/sample/temp_scaled
te/device/plc1/ot/modbus/status/link
te/device/plc1/ot/modbus/cmd/write/<id>
te/device/main/service/tedge-dot-modbus/ot/cmd/set-config/<id>
te/device/main/service/tedge-dot-modbus/ot/capabilities
```

Because the protocol is a topic level, anything that wants to *send* to a device must know it.
That is why [ot-parameter-state](../../flows/ot-parameter-state/main.js) records
`ot-protocol:<device>` from every sample, why [ot-command-forward](../../flows/ot-command-forward/main.js)
reads it back and falls back to a `params.protocol` setting, why
[ot-registration](../../flows/ot-registration/main.js) writes an `ot-protocol` field into the
child-device registration, and why a device can only be written after it has been read at
least once since the mapper started.

### Proposed

```text
te/device/plc1/ot/manifest                     (§3)
te/device/plc1/ot/sample/temp_scaled
te/device/plc1/ot/status/link
te/device/plc1///cmd/ot_write/<id>              (§6 — the thin-edge command topic)
te/device/main/service/tedge-dot-modbus/cmd/ot_set_config/<id>   (§6)
te/device/main/service/tedge-dot-modbus/ot/capabilities
```

`protocol` stays in the sample payload (§2) and in the manifest. Command ownership (contract
§6.5) is unchanged in substance: an instance subscribes to `te/device/+/ot/...` and acts only for
devices its live configuration defines — which is already the rule, and already how two Modbus
instances on one broker stay apart today. The protocol level never did that job.

### Why

The protocol says *how the connector reaches* a device, not *what the device is*. A flow, a
cloud operation or an operator addresses a device. MHS addresses devices, not buses. Removing
the level deletes one family of shared flow state and the "cannot write before first read"
wrinkle, and makes the topic a consumer subscribes to guessable from the device name alone.

### Breaks

- **Device names become unique per broker, not per protocol.** Today `plc1` may exist under
  both `ot/modbus/` and `ot/opcua/`. In 0.2 the second instance to load a name already owned
  refuses it (the runtime already warns about this within a protocol). In practice a device
  *is* one thing; two connectors talking to it over two protocols is a real but rare case, and
  it is served by two names (`plc1-modbus`, `plc1-opcua`) or, better, by one device whose points
  come from two connectors — which 0.2 does not support and 0.1 did not either.
- Every subscription in flows, e2e suites, conformance manifests and `asyncapi.yaml`.
- The `ot-protocol` field in the child-device registration is dropped; the protocol is in the
  manifest.

> **Feedback:** ☐ accept ☐ accept with changes ☐ reject — notes:

---

## 6. The runtime answers thin-edge commands directly

### Today

A cloud write travels four hops through two flows, and the connector's own command topic
duplicates the thin-edge one with the *same* state machine:

```text
c8y_SetRegister ─▶ c8y mapper ─▶ te/device/plc1///cmd/ot_write/<id>        {"status":"init","point":"setpoint","value":21.5}
                                          │
                                 ot-command-forward  (records origin, prefixes the id with "ot--")
                                          ▼
                                 te/device/plc1/ot/modbus/cmd/write/ot--<id>   {"status":"init", ...}
                                          │
                                       connector  (init → executing → successful)
                                          ▼
                                 ot-command-result  (strips "ot--", re-attaches "c8y-mapper"/"origin")
                                          ▼
                                 te/device/plc1///cmd/ot_write/<id>        {"status":"successful", ...}
```

The contract forbids the connector from touching thin-edge topics (RFC 0003 Q2: "the
connector stays ignorant of thin-edge command types"). But `ot_write` is not a cloud concept
and not a thin-edge core concept: it is a command type *this project defines*, whose payload
is the contract's own `write` request, whose state machine is the contract's own. The two
flows exist to move a message between two topics that mean the same thing.

The same applies to the management verbs, which live on a bespoke
`…/service/<svc>/ot/cmd/<verb>/<id>` topic next to the standard thin-edge service command topic.

### Proposed

The runtime subscribes to the thin-edge command topics and drives them itself:

```text
te/device/<device>///cmd/ot_write/<id>
te/device/<device>///cmd/ot_write_batch/<id>
te/device/main/service/<service>/cmd/ot_set_config/<id>
te/device/main/service/<service>/cmd/ot_define_device/<id>
te/device/main/service/<service>/cmd/ot_remove_device/<id>
```

Payloads are unchanged. The runtime also publishes the capability markers thin-edge expects
(`te/device/<device>///cmd/ot_write` retained `{}`), which today `ot-registration` publishes
from a hard-coded list. The connector-side `ot/<protocol>/cmd/<verb>/<id>` topics are removed.

```text
c8y_SetRegister ─▶ c8y mapper ─▶ te/device/plc1///cmd/ot_write/<id>  {"status":"init","point":"setpoint","value":21.5}
                                          │
                                       connector  (init → executing → successful, same topic)
```

Fields the runtime does not know (`c8y-mapper`, `origin`, …) are carried into every
transition unchanged — the rule 0.1 already has for `origin`, generalised.

What remains a flow: the **`parameter_update`** bridge. Its request is the
tedge-parameter-plugin's shape (a `c8y_ParameterUpdate` operation with a set fragment), which
*is* a cloud shape; reshaping it into an `ot_write_batch` stays exactly the kind of work flows
are for. That flow shrinks to the `parameterBatch` half of today's `ot-command-forward`
(about 40 lines), plus a result mirror for that one command type. `ot_write_coil` disappears:
it was an alias of `write` "kept separate to work around the one-operation-per-command-type
limit" of the mapper, and with the connector answering `ot_write` natively a second
`c8y_SetCoil` template can map to the same command type.

### Why

Fewer moving parts on the most safety-relevant path. Every hop is a place a retained message
can be left behind, an id prefix can be forgotten, or a correlation field can be dropped — the
`origin` echo rule and the `ot--` prefix are both patches for problems this path created.
Being a thin-edge service that answers thin-edge commands is not cloud coupling; it is what
every other thin-edge plugin does, and it is how tedge-agent will route an operation to the
device without a flow in between. MHS's equivalent is that the driver *is* the thing an agent
calls; nothing sits between them.

This reverses a decision RFC 0003 made deliberately, so it is presented as a choice, not a
correction. The alternative that keeps that decision: keep §5's `ot/cmd/<verb>` topics and the
two flows, and accept that the forward/result pair is the cost of never letting the connector
see a thin-edge topic.

### Breaks

- `ot-command-forward` and `ot-command-result` become one small `ot-parameter-update` flow.
- The Cumulocity shims in [operations/](../../operations/) keep their `ot_<verb>` command types
  (they were chosen to be the thin-edge names already) — the `c8y_SetCoil` shim changes its
  command type from `ot_write_coil` to `ot_write`.
- Conformance layer 3 targets the thin-edge topics.
- Command schema: unchanged, apart from the `ot--` convention going away.

> **Feedback:** ☐ accept ☐ accept with changes ☐ reject — notes (in particular: is "the
> connector never speaks a thin-edge topic" a principle worth two flows?):

---

## 7. `describe` becomes `manifest`, and the Cumulocity renderer leaves the SDK

### Today

`tedge-dot describe` renders **Cumulocity Digital Twin Manager property definitions** — a JSON
schema per parameter set — from the configuration. The renderer is
[descriptor.rs](../../impl/rust/crates/sdk/src/descriptor.rs) (1017 lines) and
[descriptor.c](../../impl/c/sdk/src/descriptor.c) (634 lines), inside the SDK crate whose
README says the connector "never talks to the DTM service" and is "cloud-agnostic".
A parity script checks that the two renderers produce identical DTM JSON.

```sh
tedge-dot describe --compact
```

```json
{"identifier":"modbus_demo_sim_control_parameters","jsonSchema":{"$schema":"http://json-schema.org/draft-07/schema#","title":"Modbus demo sim control parameters","description":"Writable modbus points exposed by tedge-dot (generated from the connector configuration)","type":"object","properties":{"temp_u16":{"type":"integer","title":"Temperature setpoint","description":"Target temperature for the boiler loop","minimum":0,"maximum":30000,"order":1},"coil_rw":{"type":"boolean","title":"Pump enable","order":2}}},"contexts":["asset","event","operation"],"tags":["tedge-dot","modbus"]}
```

### Proposed

The CLI prints the manifest of §3 — the same JSON the service publishes — and nothing
cloud-specific:

```sh
tedge-dot manifest                     # every device of every config in /etc/tedge/plugins/ot
tedge-dot manifest -c modbus.toml -d plc1
```

```json
{ "device": "plc1", "contract": "0.2", "protocol": "modbus", "type": "modbus-demo-sim",
  "points": { "temp_u16": { "datatype": "uint16", "access": "read_write", "range": { "min": 0, "max": 30000 },
                            "name": "Temperature setpoint", "parameter": { "sets": ["modbus_demo_sim_control_parameters"] } },
              "coil_rw":  { "datatype": "bool", "access": "read_write", "name": "Pump enable",
                            "parameter": { "sets": ["modbus_demo_sim_control_parameters", "modbus_demo_sim_commissioning_parameters"] } } } }
```

A separate, single-implementation renderer turns manifests into DTM definitions:

```sh
tedge-dot manifest | tedge-dot-c8y-dtm > definitions.jsonl     # one definition per line, as today
```

`tedge-dot-c8y-dtm` lives under [operations/](../../operations/) next to the other Cumulocity
glue, is written once (a ~100-line script in the language the shims already use), and is
tested against manifests, not against configurations.

### Why

The DTM rendering is the largest single piece of code in either SDK and the only piece that
knows a cloud vendor's schema. Moving it out shrinks both SDKs, removes a cloud dependency from
the "cloud-agnostic driver", and turns the parity test into the simpler statement *both
implementations publish byte-identical manifests*. The manifest is also the artefact that any
other cloud, an MCP server, or a commissioning tool would consume — MHS's reference file is
model-agnostic for the same reason.

### Breaks

- `tedge-dot describe` and its flags (`--set`, `--device`, `--compact`) are removed; the
  README's registration snippet pipes `manifest` through the renderer instead.
- `impl/c/ci/describe-parity.sh` compares manifests.
- `just c-describe-parity` → `just c-manifest-parity`.

> **Feedback:** ☐ accept ☐ accept with changes ☐ reject — notes:

---

## 8. Hold the C implementation at a declared subset until 0.2 has landed

### Today

Two full implementations of a 0.1 contract that this RFC proposes to change in seven places.
Each change lands three times: Rust, C, and the JavaScript flows.

| Component | Rust | C |
| --- | --- | --- |
| runtime | 2727 lines | 2023 lines |
| configuration + point libraries | 2099 lines | 1324 lines |
| parameter descriptor (§7) | 1017 lines | 634 lines |

The C build earns its place (the glibc 2.17 floor, a ~25× smaller binary, PROFIBUS-DP), and
the shared test suites are what keep the two honest. But "we can break things" at alpha is
three times as expensive as it looks, and that cost has already shaped decisions — the
three-way set-naming agreement in §3 is one.

### Proposed

For the duration of the 0.2 work, the C implementation tracks the **data plane** of 0.2 and
freezes the **control plane** at "not yet":

| Area | C implementation during 0.2 |
| --- | --- |
| samples, link status, health, manifest (§2, §3, §5) | tracks 0.2 — this is the part a small device needs |
| `datatype = "bytes"` (§1), `range` on write (§4), `ot_write` / `ot_write_batch` (§6) | tracks 0.2 |
| management verbs (`ot_set_config`, `ot_define_device`, `ot_remove_device`) | **frozen out**; tests tagged `requires:management` (the mechanism in `C_MISSING_CAPABILITIES` already exists) |
| DTM rendering (§7) | **removed** (the renderer is shared) |

The freeze lifts when the 0.2 contract is ratified and the Rust implementation has shipped it.

### Why

The parity mechanism (`requires:<capability>` tags, one list in the justfile) was built for
exactly this: a capability one implementation lacks is reported as skipped, not duplicated and
not silently passing. Using it for a *phase* rather than a permanent gap keeps the C build
releasable and small while the contract moves, and concentrates the churn where the design is
being worked out.

### Breaks

- A `tedge-dot-c` install cannot be reconfigured from the cloud until the freeze lifts; it is
  configured by file, which is its primary mode on the devices it targets anyway.

> **Feedback:** ☐ accept ☐ accept with changes ☐ reject — notes:

---

## 9. Reserved, not built: procedures and an agent surface

Two MHS concepts have no counterpart in tedge-dot. Neither is proposed for 0.2; both are named
here so that 0.2 leaves room for them rather than a shape they would have to fight.

### 9.1 A `call` verb for procedures

MHS separates **states** (read/write a value) from **procedures** (do something: "aspirate",
"home the arm"). tedge-dot has only writes; today a procedure is a `write-batch` that happens
to trigger something. That is fine for a PLC. It stops being fine at OPC UA method calls,
CANopen NMT commands, or a PROFIBUS diagnostic request — all of which exist in protocols
already supported.

The reserved shape, declared in the manifest and invoked as a thin-edge command:

```json
"procedures": { "start_cycle": { "args": { "recipe": "uint8" }, "description": "Start one production cycle" } }
```

```json
{ "status": "init", "procedure": "start_cycle", "args": { "recipe": 3 } }      →  te/device/plc1///cmd/ot_call/<id>
```

Nothing in 0.2 conflicts with adding this later; the manifest (§3) has the slot and the
command model (§6) has the topic.

### 9.2 An MCP server over the manifest

MHS's three access paths are MCP, a CLI, and code. tedge-dot has the CLI and, through flows,
code. With §3 and §6 in place, an MCP server for a thin-edge device is a thin, generic
adapter: every `ot/manifest` is a *resource*, `ot_write` is a *tool* whose argument schema is
generated from `datatype` + `range`, and samples are a subscription. It would need nothing
from the connector that 0.2 does not already publish — which is the test of whether 0.2 is the
right shape.

> **Feedback:** ☐ agree these are the right reservations ☐ notes:

---

## 10. Everything that breaks, on one page

| # | Change | Config | Wire (topics/payloads) | Flows | SDKs |
| --- | --- | --- | --- | --- | --- |
| 1 | `mode` → `datatype = "bytes"` | `mode`, `default_mode` rejected | `mode`, `value_repr`, `raw` write field gone | none | both |
| 2 | slim sample | `sample_debug` added | sample loses 9 fields | read manifest instead of sample echoes | both |
| 3 | device manifest | — | new retained `ot/manifest`; capability `point_labels` and link `type`/`info` gone | `ot-measurement`, `ot-parameter-state`, `ot-registration` | both |
| 4 | typed metadata + `range` | `range`, `publish`, `measurement`, `parameter` are point fields; legacy `meta.*` warned | writes outside `range` fail | `ot-measurement`, `ot-parameter-state` read typed fields | both (range check in runtime) |
| 5 | no protocol level | device names unique per broker | every `ot/<protocol>/` topic | every subscription | both |
| 6 | thin-edge commands | — | `ot/cmd/<verb>` topics gone; commands on `///cmd/ot_<verb>` | `ot-command-forward` + `ot-command-result` → `ot-parameter-update` | both |
| 7 | `describe` → `manifest` | — | — | — | DTM code leaves both SDKs; one renderer script |
| 8 | C freeze | — | — | — | C skips management verbs until 0.2 ships |

Suggested sequencing, if everything is accepted: **3 → 2 → 4 → 1 → 5 → 6 → 7**, with 8 in
force throughout. The manifest first, because §2 and §4 both move things *into* it; §5 and
§6 last, because they are the topic changes every suite subscribes on, and landing them once
is cheaper than twice.

## 11. Open questions

1. **`ts_ms`.** Dropped in §2 on the assumption that flows can `Date.parse` an RFC 3339 string
   cheaply. If a measured cost in the flow runtime says otherwise, it stays.
2. **Manifest before samples.** Retained delivery order across topics is undefined. Is
   "fall back to flow-wide defaults for a sample whose manifest has not arrived" acceptable for
   `ot-measurement`, or should the runtime delay a device's first samples until its manifest is
   published (it publishes both, so it can order them; a *mapper* restart is the case it cannot)?
3. **`range` on reads.** §4 leaves reads untouched. An alternative is a `range: "above" | "below"`
   marker on an out-of-range sample so a flow need not know the limits. Cheap; deferred unless
   wanted.
4. **One device, two protocols** (§5). Named as unsupported. Is there a real deployment that
   needs it before 1.0?
5. **Contract version on the wire.** The manifest carries `"contract": "0.2"`. Should samples
   too, or is the manifest enough for a consumer to know what it is talking to?
