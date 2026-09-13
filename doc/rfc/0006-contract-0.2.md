# RFC 0006: Contract 0.2 — a slimmer envelope, a device manifest, and typed signal metadata

Status: proposed — **for review, one decision per section**. Nothing here is implemented.

Revision 2 folds in an independent review of revision 1 against the code. It adds §4 (writes in
engineering units — a pre-existing defect that `range` would have compounded), corrects claims in
§1, §6, §7 and §9 that turned out to be wrong, adds §5.1 (who applies `publish`), and closes with
§11.1, the complete list of what 0.2 knowingly gives up.

| Field | Value |
| --- | --- |
| RFC | 0006 |
| Supersedes | Parts of the [OT Connector Contract](../contract/ot-connector-contract.md) 0.1.0 (§2, §3.1, §4.2, §5, §6, §7) |
| Related | [RFC 0001](0001-ot-connector-architecture.md), [RFC 0003](0003-parameter-writes.md), [RFC 0004](0004-point-libraries.md), [RFC 0005](0005-device-types-and-parameter-sets.md) |
| Prompted by | A review of the contract against the concepts of Anthropic's [Model Hardware Standard](https://www.anthropic.com/news/model-hardware-standard-research-preview) (MHS) |

## How to read this RFC

Every numbered section is an independent proposal with the same shape:

- **Today** — what the 0.1 contract does, with a real example from this repository.
- **Proposed** — the 0.2 replacement, with the same example rewritten.
- **Why** — the argument, and what MHS does where it is relevant.
- **Breaks** — what a user, a flow or an implementation has to change.
- **Feedback** — a markdown checklist to tick (`[x]`) plus a notes line. A section can be
  accepted, amended or rejected on its own;
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
  and live-reload semantics (their *topic* changes in §7);
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

What `bytes` means is defined per connector, and it is exactly what raw mode delivers today, so
nothing raw mode can do is lost (revision 1 said an OPC UA node "has no wire bytes" and would
have rejected `bytes` there; OPC UA supports raw mode today and conformance check B3 runs on it):

| Connector | `bytes` on read | `bytes` on write |
| --- | --- | --- |
| Modbus | the registers or coils read; hex, space-grouped per 16-bit word | written verbatim to `count` registers/coils |
| CAN bus | the **whole payload of the signal's frame** (today's raw mode publishes `payload.to_vec()`) | the frame payload, sent as one frame (today's raw write) |
| CANopen | the SDO payload | the SDO payload |
| PROFIBUS-DP | the bytes at `byte_offset`, to the end of the input image when no length is given (today's raw mode) | the bytes at `byte_offset` |
| OPC UA | the best-effort encoding of the variant that raw mode returns today (`variant_to_value`) | rejected at configuration time: a variant cannot be built from bytes, and today's raw write already fails at runtime for want of a datatype |

The hex grouping rule is the one `raw` uses today (`hex_grouped`, per protocol word), because the
conformance vectors compare against it. A connector that cannot deliver bytes for a point kind
rejects `bytes` for that kind at configuration time, exactly as it rejects any datatype it does
not list in its capabilities; `datatypes` containing `bytes` replaces `modes` containing `raw` as
the signal the conformance suite keys on.

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

**Feedback**

- [ ] accepted
- [ ] accepted with changes
- [ ] rejected

Notes:

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
| `protocol` | yes | yes | kept — a fact about the sample's origin, cheap, and what `ot-measurement` groups by (§6 would remove it from the *topic*, not the payload) |
| `datatype` | when typed | always | kept — one short string; tells a consumer that a `string`-typed `value` is an `int64` outside the safe range (§4.1 of the contract) without a second field |
| `value` | typed + good | good | unchanged; a `bytes` value is the hex string (§1) |
| `mode` | yes | — | removed (§1) |
| `value_repr` | yes | — | removed; `datatype` + JSON type carry the same information |
| `raw`, `addr` | yes | opt-in | `[connector] sample_debug = true` adds both; off by default |
| `ts_ms` | optional | — | removed; `Date.parse(sample.ts)` in a flow |
| `type`, `unit`, `access`, `meta` | yes | — | the device manifest (§3); `type` also stays on the link status, for registration (§3) |

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
- Tooling that inspects `raw`/`addr` sets `sample_debug`. That includes the conformance suite:
  layer 3 checks `mode`, `raw` and `value_repr` on every sample
  ([layer3.rs](../../impl/rust/crates/ot-conformance/src/layer3.rs)), so the harness runs the
  connector with `sample_debug = true` and those checks become `datatype`/`value` checks. Two e2e
  cases go with the fields: the OPC UA `addr.node_id` echo and the CAN `value_repr` check.
- **Custom flows.** The flows in this repository are rewritten for the manifest, but a site's own
  flow that reads `sample.meta.my_threshold` today — the pattern behind the standing goal
  *per-signal logic in flows, declared next to the signal*, and the planned next use of `meta`
  (alarm thresholds for `ot-alarm`) — needs a manifest subscription, a `context.mapper` lookup,
  and a fallback for a sample that arrives before its manifest. That is the real cost of this
  section, and revision 1 understated it ("nothing reads `meta`" is true of the connector, not of
  flows). Two mitigations are proposed with §5: the runtime applying `publish` itself (§5.1),
  which takes the per-signal lookup out of the hot path for the standard flows, and an opt-in
  `[connector] sample_echo = ["publish", "meta"]` for a site that wants its flows stateless and
  pays the bytes knowingly.

**Feedback**

- [ ] accepted
- [ ] accepted with changes
- [ ] rejected

Notes:

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

One retained message per device, published when the device is loaded — before its link status
and before any sample, on the same connection — republished on every configuration change that
affects it (a reload, a management verb, a changed point library), and cleared (empty retained)
when the device is removed (`remove-device`) or switched off (`enabled = false`). The link status
is cleared at the same moments — a change from contract §3.3, which leaves it behind today — so
that nothing retained describes a device that is gone:

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
- `commands` advertises the thin-edge command types the device answers (§7), so the
  registration flow stops hard-coding `ot_write,ot_write_coil,parameter_update`.
- `meta` (§5) is included verbatim when present — the manifest is where free-form tags belong.

What it replaces:

| Removed | Replaced by |
| --- | --- |
| `point_labels` in the capability descriptor | `points.<id>.name/description` |
| `type`, `access`, `unit`, `meta` in every sample | `type`, `points.<id>.*` |
| `info` on the link status | `info` — the link status **keeps `type`** (see Breaks: registration) and is otherwise back to `status`/`since`/`reason` |
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
artefact a future MCP server or agent would read (§10).

### Breaks

- Flows: `ot-measurement` reads `publish` hints and `measurement` naming from the manifest
  instead of `sample.meta` (with §5.1 option A, only the naming); `ot-parameter-state` reads
  `parameter.sets` and `access` from it and drops its derivation code (roughly 120 of its 259
  lines); `ot-registration` takes `info` and `commands` from the manifest.
- **Registration keeps its trigger and its `type` on the link status.** Flow shared state is an
  in-memory map (`LayeredKVStore` in thin-edge's `tedge_flows` crate), so after every mapper
  restart the retained link status and the retained manifest are replayed in whatever order the
  broker chooses, and `ot-registration` registers a device on its first `connected` link and
  never again. Were `type` only in the manifest, a link-first replay would register
  `<protocol>-device` for good. The link status is one retained message per device, not a
  per-sample echo, so keeping `type` on it costs nothing and removes the race. `info` and
  `commands` can arrive late without harm: the twin fragment and the capability markers are
  simply published when the manifest is seen.
- Ordering for samples is a smaller problem than revision 1 made it: samples are not retained, so
  after a mapper restart only *live* samples can race the replayed manifest, and on a connector
  restart the runtime publishes the manifest first on the same connection. In that window
  `ot-measurement` falls back to its flow-wide defaults (as it does today when `meta` is absent),
  which can misname one sample's series; `ot-parameter-state` buffers nothing and simply waits.
  §5.1 option A removes the publish-policy half of the window entirely. See open question 12.2.
- Status schema: `point_labels` and link `info` removed; a new `manifest` definition.

**Feedback**

- [ ] accepted
- [ ] accepted with changes
- [ ] rejected

Notes:

---

## 4. Writes are in engineering units

### Today

Contract §4.2 defines the per-point linear transform for **reads**: the sample's `value` is the
scaled result. It says nothing about writes, and both implementations follow it literally. The
Modbus module applies `transform` when it builds a sample
([lib.rs:618](../../impl/rust/crates/connector-modbus/src/lib.rs)) and encodes a write request's
`value` exactly as it arrives ([lib.rs:494](../../impl/rust/crates/connector-modbus/src/lib.rs));
the OPC UA module does the same. So for a writable point declared like the demo's `temp_scaled`:

```toml
[[point]]
id        = "temp_scaled"
datatype  = "uint16"
access    = "read_write"
address   = { table = "holding", address = 3, count = 1 }
transform = { decimal_shift = -3 }
unit      = "°C"
```

the round trip that RFC 0003 built the Parameters tab on does not close:

```text
register 17001 ──read──▶  { "value": 17.001 }                    twin shows 17.001 °C
operator edits 17.001 → 20 ──write──▶ { "point": "temp_scaled", "value": 20 }
                                      ──encode uint16(20)──▶ register 20
next read ──▶ { "value": 0.02 }
```

Nothing caught it because no suite round-trips a writable point that has a transform: the demo's
writable `temp_u16` is the unscaled twin of the read-only `temp_scaled`, the conformance
configurations' transform points are read-only, and check B6 writes unscaled points. Revision 1
of this RFC would have made it worse: §5 checks `range` in engineering units, and on a write that
would have compared an engineering bound against a wire value.

### Proposed

A write's `value` is in the same units as the sample's `value` — engineering units — for `write`,
`write-batch` and the `tedge-dot write --value` CLI. The runtime inverts the linear transform
before the module sees the value:

```text
wire = (value − offset) × divisor ÷ (multiplier × 10^decimal_shift)
```

```json
{ "status": "init", "point": "temp_scaled", "value": 20 }
```

```text
runtime:   20 → (20 − 0) × 1 ÷ (1 × 10⁻³) = 20000 → the module encodes uint16 20000
result:    { "status": "successful", "point": "temp_scaled", "value": 20 }
next read: { "value": 20 }
```

Rules:

- The inversion happens in the SDK runtime, once, for every protocol, after the `range` check
  (§5, on the engineering value) and before the module's `execute`. A module keeps receiving what
  it receives today: the wire value.
- For an integer datatype the inverted value is rounded to nearest; a value the datatype cannot
  hold after inversion fails the write (`value 70 does not fit uint16 after transform`).
- A writable point whose transform cannot be inverted (`multiplier = 0`) is rejected at
  configuration time: "point temp_scaled is writable but its transform cannot be inverted".
  Contract §4.2 already treats `divisor = 0` as 1.
- `bytes`, `bool` and `string` values are untouched, as the transform never applied to them.
- The result message echoes the engineering value, so `ot-parameter-state`'s optimistic update
  stays in the units the twin displays.

### Why

The contract says the scaled value *is* the value of the point; RFC 0003 says the twin displays
that value and the Parameters tab edits it; RFC 0002 imports Cloud Fieldbus scaling into
`transform`, and Cloud Fieldbus writes are in engineering units. Every layer above the module
already assumes symmetry. Making writes symmetric is a defect fix that happens to be a contract
change, and it is the prerequisite for `range` (§5) meaning one thing on both paths. MHS's
reference file describes "what can be adjusted" in the same terms as "what it measures"; a driver
whose set-temperature took different units from its get-temperature would fail that test.

### Breaks

- A script that today writes wire units to a transformed point gets a different register value.
  None exists in the repository; a site's would have been written around the defect.
- Both SDKs gain the inversion; the modules do not change.
- Conformance B6 and the Modbus/OPC UA e2e suites gain a round trip on a writable point *with* a
  transform, so the defect cannot return.

**Feedback**

In particular: round an inexact integer write to nearest, or refuse it?

- [ ] accepted
- [ ] accepted with changes
- [ ] rejected

Notes:

---

## 5. Typed signal metadata, and a `range` the driver enforces

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

meta = { asset_tag = "B-17", commissioned = "2026-03" }     # yours; the connector never reads it, your flows find it in the manifest
```

| Field | Type | Meaning | Interpreted by |
| --- | --- | --- | --- |
| `range` | `{ min?, max? }` | Engineering-unit bounds: the sample's `value` on read, the request's `value` on write (§4). | **the connector** (below) and the cloud form |
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

**Inheritance.** Across `points_from` libraries and inline overrides, `range`, `publish`,
`measurement` and `parameter` merge **key by key**, exactly as `meta` and `transform` do today
(contract §3.4; `merge_point_table` in [library.rs](../../impl/rust/crates/sdk/src/library.rs)),
so a site can set `publish.deadband` on an inherited point without restating `on_change` and
`min_interval`. Revision 1 left this unspecified, and the default rule for every other field —
replace — would have made such an override silently drop the inherited keys. The scalar forms
`measurement = false` and `parameter = false | true | "<set>"` replace whatever was inherited, as
any scalar does.

### 5.1 Who applies `publish`: the flow, or the runtime?

Today `on_change`, `deadband`, `min_interval` and `debounce` are applied by `ot-measurement`,
which is why that flow needs per-signal metadata in its hot path — and why §2's cost to custom
flows exists. The four settings are protocol-neutral, identical in every deployment, and were
anticipated as connector work from the start: RFC 0001 §7 lists "per-point publish throttling in
the connector" as a mitigation, and the SDK document already lists "backpressure / throttling:
optional per-point minimum publish interval" among the runtime's responsibilities. MHS puts rate
limiting in the driver for the same reason.

**Option A — the runtime applies `publish`** (recommended). A point with `publish.on_change` is
published only when its value changes (by more than `deadband`), never more often than
`min_interval`, and only once it has been stable for `debounce`. Every consumer — the
measurement flow, the parameter twin, a historian, an MCP subscription — then sees one stream
with the policy already applied, and `ot-measurement` needs the manifest only for naming. `bad`
samples are still published (and rate-limited) as contract §5.1 says; `stale` is unaffected. The
runtime already keeps per-point state (`seq`, the last value for `stale`), so the code is small
and lands once per SDK rather than once per flow that wants it. A consumer that needs every raw
read leaves `publish` undeclared on that point.

**Option B — the flows apply it**, as today, reading the manifest. Nothing moves into the
connector; every flow that wants per-signal policy carries the lookup.

Either way `publish` is the same typed table in the same place; the decision is only who reads it.

**Feedback (5.1)**

- [ ] A: the runtime applies `publish`
- [ ] B: the flows apply `publish`

Notes:

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
- With §5.1 option A, `publish` becomes connector behaviour: the sample stream itself is
  filtered, not just the measurements derived from it.

**Feedback**

In particular: enforce `range` in the driver, or keep the driver limit-free and leave it to the cloud form?

- [ ] accepted
- [ ] accepted with changes
- [ ] rejected

Notes:

---

## 6. Drop `<protocol>` from the topic tree

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
te/device/plc1///cmd/ot_write/<id>              (§7 — the thin-edge command topic)
te/device/main/service/tedge-dot-modbus/cmd/ot_set_config/<id>   (§7)
te/device/main/service/tedge-dot-modbus/ot/capabilities
```

`protocol` stays in the sample payload (§2) and in the manifest. Command ownership (contract
§6.5) is unchanged in substance: an instance subscribes to `te/device/+/ot/...` and acts only for
devices its live configuration defines — which is already the rule, and already how two Modbus
instances on one broker stay apart today. The protocol level never did that job.

**This section is now optional.** With §7 accepted, no flow ever needs the protocol to address a
device: commands travel on the thin-edge topic, and the `ot-protocol:<device>` state disappears
with `ot-command-forward`. What remains of this section is a guessable sample topic and one level
less — against a functional loss described under Breaks. Revision 1 argued this section from the
command path; that argument now belongs to §7, and the recommendation below has changed.

### Why

The protocol says *how the connector reaches* a device, not *what the device is*. A flow, a
cloud operation or an operator addresses a device. MHS addresses devices, not buses. Removing
the level makes the topic a consumer subscribes to guessable from the device name alone. The
family of shared flow state and the "cannot write before first read" wrinkle, which revision 1
also credited to this section, are removed by §7 with or without it.

### Breaks

- **One device served by two connectors is legal today, and this section breaks it.** Uniqueness
  is per protocol *and* device — contract §6.5, and the duplicate check in
  [main.rs](../../impl/rust/src/main.rs) is on `(protocol, device)` pairs — so a `plc1` read over
  Modbus and over OPC UA publishes under `ot/modbus/` and `ot/opcua/` and feeds **one** child
  device. Revision 1 claimed 0.1 did not support this; it does. Without the level, the two
  connectors' manifests and link statuses overwrite each other on one topic, and both instances
  own `plc1` for commands. Three ways out:
  1. **Withdraw this section** (recommended): keep `ot/<protocol>/` for samples, link status and
     the manifest. A consumer that wants every manifest subscribes `te/device/+/ot/+/manifest`.
     Nothing else in this RFC depends on the level being gone.
  2. Drop the level and **declare the case unsupported**: device names become unique per broker,
     the second instance to load a taken name refuses it, and a device reached over two protocols
     is two names. Honest, but a removal.
  3. Drop the level from samples only and key the per-connector retained messages by protocol
     (`ot/manifest/<protocol>`, `ot/status/link/<protocol>`). Consistent for nobody; listed for
     completeness.

  Whichever is chosen, §7 carries the ownership rule *device and point in the live configuration*,
  so that two instances sharing a device never both answer one `ot_write`.
- Topic-level filtering by protocol goes with the level: `tedge mqtt sub 'te/+/+/ot/modbus/#'`
  for debugging, and a flow instance scoped to one protocol by its input filter. Both become a
  check on `sample.protocol`.
- Every subscription in flows, e2e suites, conformance manifests and `asyncapi.yaml`.
- The `ot-protocol` field in the child-device registration is dropped; the protocol is in the
  manifest.

**Feedback**

- [ ] withdraw this section (keep the protocol level)
- [ ] drop the level; one device on two protocols becomes unsupported
- [ ] drop the level from samples only; key the retained messages by protocol

Notes:

---

## 7. The runtime answers thin-edge commands directly

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
te/device/<device>///cmd/ot_set_config/<id>              (management: claimed by payload.service, below)
te/device/<device>///cmd/ot_define_device/<id>
te/device/<device>///cmd/ot_remove_device/<id>
te/device/main/service/<service>/cmd/ot_set_config/<id>   (management: addressed to one instance)
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
(about 40 lines), plus a result mirror for that one command type.

**`ot_write_coil` stays, as an alias.** Revision 1 said it could go; that was wrong. The reason
it exists is the c8y mapper, not the flow path: two operation templates cannot share one
`workflow.operation` — the mapper's `get_operation_name_by_workflow_operation` warns "Found more
than one template with the same `workflow.operation` field" and picks the first — so
`c8y_SetCoil` needs a command type of its own for as long as it is a separate Cumulocity
operation. Rather than bake a Cumulocity workaround into the protocol-neutral runtime, the alias
is configuration:

```toml
[connector]
command_aliases = { ot_write_coil = "ot_write" }    # packaged default; delete when c8y_SetCoil goes
```

The runtime subscribes to, answers and advertises (`manifest.commands`) every alias exactly as
the command it stands for.

**Management verbs from the cloud need a claim rule.** A Cloud Fieldbus operation
(`c8y_ModbusConfiguration`, `c8y_ModbusDevice`) is an operation on the *gateway*, so the mapper
delivers it on the gateway's command topic — `te/device/main///cmd/ot_set_config/<id>` — never
on a connector's service topic. Today `ot-command-forward` re-addresses it to the service the
payload names, else to `tedge-dot-<protocol>`. Without that hop, RFC 0002 increments 1 and 2
(shipped, verified live 2026-07-02) would stop working — revision 1 missed this. So the runtime
also subscribes to the device-level management command types, and an instance acts on such a
command **only when the payload's `service` field names it**; a request that names no service is
nobody's and stays at `init`, like a command for an unowned device (contract §6.5). The shims
therefore always name the service — `input.service = "tedge-dot-modbus"` in the
`c8y_ModbusConfiguration` and `c8y_SerialConfiguration` templates, `FIELDBUS_SERVICE` in the
import script as today — and the service-topic form remains for a requester that wants to
address an instance directly. A result is published on the topic its request arrived on, so
`origin.device` is no longer needed to find the entity to complete.

**Ownership is by device *and* point.** An instance acts on `ot_write` only when the device
*and* the named point are in its live configuration (for `ot_write_batch`, every point), and
publishes nothing otherwise. With §6 withdrawn, a device served by two connectors is answered
by the one that has the point.

### Why

Fewer moving parts on the most safety-relevant path. Every hop is a place a retained message
can be left behind, an id prefix can be forgotten, or a correlation field can be dropped — the
`origin` echo rule and the `ot--` prefix are both patches for problems this path created.
Being a thin-edge service that answers thin-edge commands is not cloud coupling; it is what
every other thin-edge plugin does, and it is how tedge-agent will route an operation to the
device without a flow in between. MHS's equivalent is that the driver *is* the thing an agent
calls; nothing sits between them.

One more thing the thin-edge topic brings for free: the c8y mapper clears the command topic
once the operation is done (its operation handler documents the step "and then clear the local
MQTT operation topic"), so the terminal state no longer lingers retained on a connector-side
topic that nobody clears. `ot-parameter-state` consumes results live, as it does today.

This reverses a decision RFC 0003 made deliberately, so it is presented as a choice, not a
correction. The alternative that keeps that decision: keep the 0.1 `ot/<protocol>/cmd/<verb>`
topics and the two flows, and accept that the forward/result pair is the cost of never letting
the connector see a thin-edge topic.

### Breaks

- `ot-command-forward` and `ot-command-result` become one small `ot-parameter-update` flow.
- The Cumulocity shims in [operations/](../../operations/) keep their `ot_<verb>` command types
  (they were chosen to be the thin-edge names already); the management-verb templates gain an
  explicit `input.service`.
- Conformance layer 3 targets the thin-edge topics.
- Command schema: unchanged, apart from the `ot--` convention going away.
- The capability markers (`te/device/<d>///cmd/ot_write` retained `{}`) move from
  `ot-registration` to the runtime, which may publish them before the child-device registration
  exists. Whether the c8y mapper holds such a message for an entity it has not seen yet or drops
  it is open question 12.6; until that is verified, `ot-registration` keeps publishing them from
  `manifest.commands`, which is harmless if both do.

**Feedback**

In particular: is "the connector never speaks a thin-edge topic" a principle worth two flows?

- [ ] accepted
- [ ] accepted with changes
- [ ] rejected

Notes:

---

## 8. `describe` becomes `manifest`, and the Cumulocity renderer leaves the SDK

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

`manifest` carries over everything `describe` *checks* today, not only what it prints: the warning
for a device without a `type` (its sets collide, RFC 0005), the refusal of a parameter id outside
`[A-Za-z0-9_]`, and the forced set name of `--set` — which becomes a configuration key
(`[connector] parameter_set`), because the flow that publishes the twin must agree on it, and
today's `default_set` flow parameter is a second copy of that decision. Rendering a set that
several configurations share *once* — today `describe`'s job across a directory — becomes the
renderer's: it de-duplicates by identifier across the manifests it is given.

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
- `ot-parameter-state` loses its `default_set` parameter; the set names arrive resolved in the
  manifest.

**Feedback**

- [ ] accepted
- [ ] accepted with changes
- [ ] rejected

Notes:

---

## 9. Hold the C implementation at 0.1 until 0.2 has landed

### Today

Two full implementations of a 0.1 contract that this RFC proposes to change in eight places.
Each change lands three times: Rust, C, and the JavaScript flows.

| Component | Rust | C |
| --- | --- | --- |
| runtime | 2727 lines | 2023 lines |
| configuration + point libraries | 2099 lines | 1324 lines |
| parameter descriptor (§8) | 1017 lines | 634 lines |

The C build earns its place (the glibc 2.17 floor, a ~25× smaller binary, PROFIBUS-DP), and
the shared test suites are what keep the two honest. But "we can break things" at alpha is
three times as expensive as it looks, and that cost has already shaped decisions — the
three-way set-naming agreement in §3 is one.

Revision 1 proposed freezing the C *control plane* "at not yet". That misdescribed the
situation: the C runtime implements the management verbs today, [its README](../../impl/c/README.md)
calls the C connectors "fully conformant, hot reload through the management verbs included", and
the Cloud Fieldbus cloud suite runs on C with no `requires:` tag. Taking the verbs out of C would
be a regression of a shipped capability, not a deferral of a pending one.

### Proposed

Three honest options, the first recommended:

| Option | During the 0.2 work | Cost |
| --- | --- | --- |
| **A. Freeze C at 0.1, whole** (recommended) | C is neither ported nor released until 0.2 is ratified and shipped in Rust; the last 0.1 `tedge-dot-c` release stays available. The shared suites move to 0.2 with Rust; C is then ported in **one** pass against a settled contract, and the suites and parity checks resume. | No C release during the transition. A 0.1 C binary cannot run with the 0.2 flows, so the two packages are not interchangeable until the port lands. |
| B. Port in lockstep | Every section lands in Rust, C and JavaScript together, as today. | Three times the work in the phase where iterations are most frequent — the reason this section exists. |
| C. Port the data plane, drop the management verbs from C | What revision 1 proposed. | A regression: `tedge-dot-c` loses cloud reconfiguration it has today, and the Cloud Fieldbus suite must be tagged `requires:management` for C. |

Under A the `requires:<capability>` mechanism is not used for the transition at all: it exists
for capabilities an implementation genuinely lacks, and using it for "not ported yet" would report
half a suite as skipped and call that parity. The freeze lifts when 0.2 is ratified.

### Why

The alternative to a freeze is to pay for every design iteration three times, in the phase where
iterations are most frequent. A whole-implementation freeze is also the only option that keeps
the parity promise honest: under it, either both binaries run the *same* suite at the same
contract version, or C is explicitly at the previous version — never a mix that tags paper over.

### Breaks

- No `tedge-dot-c` release during the transition; a device on the C build stays on 0.1, flows
  included, until the port.
- The parity checks (`just c-describe-parity`, the shared e2e/cloud/conformance runs against C)
  are suspended, not deleted, and resume against the ported build.

**Feedback**

- [ ] accepted
- [ ] accepted with changes
- [ ] rejected

Notes:

---

## 10. Reserved, not built: procedures and an agent surface

Two MHS concepts have no counterpart in tedge-dot. Neither is proposed for 0.2; both are named
here so that 0.2 leaves room for them rather than a shape they would have to fight.

### 10.1 A `call` verb for procedures

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
command model (§7) has the topic.

### 10.2 An MCP server over the manifest

MHS's three access paths are MCP, a CLI, and code. tedge-dot has the CLI and, through flows,
code. With §3 and §7 in place, an MCP server for a thin-edge device is a thin, generic
adapter: every `ot/manifest` is a *resource*, `ot_write` is a *tool* whose argument schema is
generated from `datatype` + `range`, and samples are a subscription. It would need nothing
from the connector that 0.2 does not already publish — which is the test of whether 0.2 is the
right shape.

**Feedback**

- [ ] these are the right reservations
- [ ] something is missing or wrong (see notes)

Notes:

---

## 11. Everything that breaks, on one page

| # | Change | Config | Wire (topics/payloads) | Flows | SDKs |
| --- | --- | --- | --- | --- | --- |
| 1 | `mode` → `datatype = "bytes"` | `mode`, `default_mode` rejected | `mode`, `value_repr`, `raw` write field gone | none | both |
| 2 | slim sample | `sample_debug`, `sample_echo` added | sample loses 9 fields | read manifest instead of sample echoes; custom flows join | both |
| 3 | device manifest | — | new retained `ot/manifest`; capability `point_labels` and link `info` gone; link status cleared with the manifest | `ot-measurement`, `ot-parameter-state`, `ot-registration` | both |
| 4 | writes in engineering units | a writable point's transform must be invertible | `value` in write requests and results is the scaled value | none | both (inversion in the runtime) |
| 5 | typed metadata + `range`; 5.1 `publish` in the runtime | `range`, `publish`, `measurement`, `parameter` are point fields, merged key by key; legacy `meta.*` warned | writes outside `range` fail; with 5.1 A the sample stream is policy-filtered | `ot-measurement`, `ot-parameter-state` read typed fields | both |
| 6 | no protocol level (optional; recommended withdrawn) | device names unique per broker, if accepted | every `ot/<protocol>/` topic, if accepted | every subscription, if accepted | both |
| 7 | thin-edge commands | `command_aliases` added; shims name their service | `ot/cmd/<verb>` topics gone; commands on `///cmd/ot_<verb>` | `ot-command-forward` + `ot-command-result` → `ot-parameter-update` | both |
| 8 | `describe` → `manifest` | `parameter_set` replaces `--set` and the flow's `default_set` | — | `ot-parameter-state` loses `default_set` | DTM code leaves both SDKs; one renderer script |
| 9 | C freeze | — | — | — | C stays at 0.1 until the port |

Suggested sequencing, if everything is accepted: **3 → 2 → 4 → 5 → 1 → 7 → 6 → 8**, with 9 in
force throughout. The manifest first, because §2 and §5 both move things *into* it; §4 before
§5, because `range` is checked in the units §4 defines; §7 and §6 last, because they are the
topic changes every suite subscribes on, and landing them once is cheaper than twice.

Dependencies between the sections:

- §5 (`range`) requires §4: without it, `range` would compare an engineering bound against a
  wire value on every write.
- §2 requires §3 (the fields have to go somewhere) and, for custom flows, either §5.1 option A
  or the `sample_echo` opt-in.
- §8 requires §3 (it prints the manifest); §7 requires §3 only for `manifest.commands`.
- §6 requires nothing and is required by nothing; §7 removes its original motivation.
- §1, §4 and §9 stand alone.

### 11.1 What 0.2 knowingly gives up

The review that produced revision 2 asked what functionality the proposal loses. This is the
complete list; everything not in it is a rename, a relocation or a fix.

| Lost | Section | Deliberate? | Mitigation |
| --- | --- | --- | --- |
| A stateless per-sample custom flow reading `sample.meta` | §2 | yes | manifest join; `sample_echo` opt-in; §5.1 A for the publish policy |
| `value_repr`, `ts_ms`, per-sample `raw` and `addr` | §1, §2 | yes | `datatype` + JSON type; `Date.parse`; `sample_debug` |
| One device served by two connectors | §6 | **only if §6 is accepted** | withdraw §6 (recommended) |
| Topic-level filtering by protocol | §6 | only if §6 is accepted | filter on `sample.protocol` |
| The `ot/<protocol>/cmd/<verb>` topics as a command path outside thin-edge | §7 | yes | the thin-edge topic is the path; the CLI remains for a stopped service |
| `describe` and its `--set`/`--device`/`--compact` flags | §8 | yes | `manifest` + renderer; `parameter_set` |
| A `tedge-dot-c` release during the transition | §9 | yes | the last 0.1 release; a one-pass port after ratification |
| Writing wire units to a transformed point | §4 | yes — it was a defect | `bytes` writes stay verbatim |
| OPC UA raw mode | §1 | **no** — kept as `bytes` | — |
| Cloud reconfiguration on `tedge-dot-c` | §9 | **no** — option A keeps it, at 0.1 | — |

## 12. Open questions

1. **`ts_ms`.** Dropped in §2. The commit that added it recorded no rationale beyond "for
   consumers doing time arithmetic", so the assumption is that flows can `Date.parse` an
   RFC 3339 string cheaply. If a measured cost in the flow runtime says otherwise, it stays.
2. **Manifest before samples.** §3 narrows this to live samples racing a replayed manifest after
   a *mapper* restart. Is "fall back to flow-wide defaults for that sample" acceptable for
   `ot-measurement`'s naming, or should the flow hold a sample until its manifest has been seen?
3. **`range` on reads.** §5 leaves reads untouched. An alternative is a `range: "above" | "below"`
   marker on an out-of-range sample so a flow need not know the limits. Cheap; deferred unless
   wanted.
4. **One device, two protocols** (§6). Legal today; lost only if §6 is accepted. Is the case worth
   keeping? The answer decides §6.
5. **Contract version on the wire.** The manifest carries `"contract": "0.2"`. Should samples
   too, or is the manifest enough for a consumer to know what it is talking to?
6. **Capability markers before registration** (§7). The runtime publishes
   `te/device/<d>///cmd/ot_write` for a device the c8y mapper may not have seen yet. Does the
   mapper hold a message for an unregistered entity until its registration arrives, or drop it?
   Decides whether `ot-registration` keeps publishing the markers.
7. **Rounding on write** (§4). A write of 21.55 to an integer point with `decimal_shift = -1`
   inverts to 215.5. Proposed: round to nearest, and refuse only a value the datatype cannot
   hold. The alternative is to refuse any inexact write.
8. **`publish` and `bad` samples** (§5.1 option A). Does `min_interval` also limit repeated `bad`
   samples, replacing the runtime's separate bad-sample rate limit, or do the two stay
   independent?
