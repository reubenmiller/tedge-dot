# The OT Connector Contract

| Field | Value |
| --- | --- |
| Status | **Normative draft** |
| Version | 0.1.0 |
| Date | 2026-05-30 |
| Schemas | [schemas/](schemas/) · [asyncapi.yaml](asyncapi.yaml) |

This document defines the **OT Connector Contract**: the protocol-neutral interface every
connector exposes to the rest of thin-edge.io. It is the single source of truth that the
[SDK](../sdk/connector-sdk.md), the [connector specs](../connectors/), the
[flows](../flows/), and the [conformance suite](../conformance/conformance-suite.md) all
build on.

The keywords **MUST**, **MUST NOT**, **SHOULD**, **SHOULD NOT**, and **MAY** are used as in
RFC 2119.

> **This contract is protocol-agnostic.** Nothing in it is specific to Modbus, CAN, BACnet,
> OPC-UA, or any other protocol. Exactly three configuration objects are left opaque for a
> protocol to define — `connection`, `device.protocol_address`, and `point.address` (§3.2) —
> and everything else (topics, sample envelope, datatypes, commands, capabilities, status) is
> identical across protocols. Throughout this document **Modbus is used only as a running
> example** to make the abstract structure concrete; wherever you see Modbus terms such as
> *register*, *coil*, or *unit id*, read them as "this protocol's equivalent."

---

## 1. Concepts and terminology

| Term | Meaning |
| --- | --- |
| **Connector** | A running instance of `tedge-dot` using one protocol module. |
| **Protocol module** | The protocol-specific code (e.g. Modbus) implementing the `Connector` trait. |
| **Device** | A physical/logical field device the connector talks to (a PLC, a meter, an OPC-UA server). Maps to a thin-edge entity. |
| **Point** | A single readable/writable datum on a device (a register, a coil, an OPC-UA node, a CAN signal). |
| **Sample** | The result of reading a point once, published as a *sample envelope*. |
| **Mode** | Per-point output selection: `raw` (bytes) or `typed` (decoded primitive). |
| **Capability** | A declared feature a connector supports (a point kind, a mode, a command verb). |

A connector manages **one or more devices**; each device has **one or more points**. The
connector reads points (by polling and/or subscription), publishes **samples**, accepts
**commands** (e.g. writes), and reports **status**.

## 2. Topic conventions

All topics live under the thin-edge.io entity tree so the local mapper and flows pick them
up naturally. `<device>` is the thin-edge entity id segment for the device
(e.g. `plc-1`); `<protocol>` is the protocol module id (e.g. `modbus`).

| Purpose | Direction | Topic | Retained |
| --- | --- | --- | --- |
| Sample (read result) | connector → broker | `te/device/<device>/ot/<protocol>/sample/<point>` | no |
| Connector status | connector → broker | `te/device/main/service/<service>/status/health` | yes |
| Device link status | connector → broker | `te/device/<device>/ot/<protocol>/status/link` | yes |
| Capability descriptor | connector → broker | `te/device/main/service/<service>/ot/capabilities` | yes |
| Command request | requester → broker | `te/device/<device>/ot/<protocol>/cmd/<verb>/<id>` | yes |
| Command result | connector → broker | `te/device/<device>/ot/<protocol>/cmd/<verb>/<id>` | yes |

Notes:

- `<service>` is the connector service name (default `tedge-dot`).
- Samples MUST NOT be retained; they are time series.
- Status, capability, and command messages MUST be retained so late subscribers and the
  command state machine observe the latest state.
- Command request and result share a topic; the **payload `status` field** carries the
  state-machine transitions defined in §6.

> Flows are responsible for re-publishing samples into the standard
> `te/device/<device>///m|e|a/...` topics. The connector itself MUST NOT publish to the
> `m/`, `e/`, or `a/` channels.

## 3. Configuration model

Configuration is TOML. The top level is split into a **connector** section, a **connection**
section (protocol-specific), and a list of **devices**, each with a list of **points**. The
structure below is **protocol-neutral**: only the three objects marked *protocol-specific*
(`connection`, `device.protocol_address`, `point.address`) change shape from one protocol to
the next (see §3.2). Everything else is identical for Modbus, CAN, BACnet, OPC-UA, and any
future protocol.

```toml
# /etc/tedge/plugins/ot/<protocol>.toml   (protocol-neutral skeleton)

[connector]
protocol      = "<protocol>"    # protocol module id (MUST match a compiled-in module)
service_name  = "tedge-dot"
poll_interval = "2s"            # default poll interval (duration string); per-point override allowed
log_level     = "info"

[mqtt]
host = "127.0.0.1"
port = 1883

# Protocol-specific shared connection defaults. Shape defined by each connector spec.
[connection]
# ...

[[device]]
name     = "<device-name>"      # -> te/device/<device-name>
protocol_address = { } # protocol-specific: how to reach this device. Shape per connector spec.
poll_interval = "2s"            # optional per-device override
default_mode  = "typed"         # optional; default output mode for this device's points

  [[device.point]]
  id       = "<point-id>"       # unique within the device; appears in topics
  mode     = "typed"            # "raw" | "typed" (inherits device.default_mode if omitted)
  datatype = "float32"          # required when mode = "typed"; see §4
  endianness    = "big"         # byte order: "big" | "little" (typed only)
  word_order    = "big"         # multi-word order: "big" | "little" (typed only)
  poll_interval = "1s"          # optional per-point override
  address  = { } # protocol-specific: how to address this point. Shape per connector spec.
  access   = "read"             # "read" | "write" | "read_write" (default "read")
  unit     = "raw"              # optional free-form hint passed through in the sample
  transform = { multiplier = 1, divisor = 1, decimal_shift = 0, offset = 0 } # optional linear scale
```

> **Example (Modbus).** To make the skeleton concrete, here are the same fields populated for
> a Modbus device. Only the three protocol-specific objects differ from the skeleton; a CAN or
> OPC-UA example would fill in those same three slots differently while keeping every other
> field identical.
>
> ```toml
> [connector]
> protocol      = "modbus"
> poll_interval = "2s"
>
> [connection]
> serial = { baudrate = 9600, parity = "N", stopbits = 2, databits = 8 }  # RTU defaults
>
> [[device]]
> name     = "plc-1"
> protocol_address = { transport = "tcp", host = "192.168.0.10", port = 502, unit_id = 1 }
> default_mode = "typed"
>
>   [[device.point]]
>   id       = "boiler_temp"
>   mode     = "typed"
>   datatype = "float32"
>   address  = { table = "holding", address = 7, count = 2 }
>
>   [[device.point]]
>   id       = "run_command"
>   mode     = "raw"
>   access   = "read_write"
>   address  = { table = "coil", address = 0, count = 1 }
> ```

### 3.1 Common (protocol-neutral) point fields

| Field | Type | Required | Notes |
| --- | --- | --- | --- |
| `id` | string | yes | Unique within the device; used in `sample/<point>` and `cmd` topics. |
| `mode` | `"raw"` \| `"typed"` | no | Inherits `device.default_mode`, else `"typed"`. |
| `datatype` | string | when `typed` | One of §4's primitive types. |
| `endianness` | `"big"` \| `"little"` | no | Byte order for `typed`; default `"big"`. |
| `word_order` | `"big"` \| `"little"` | no | Word order for multi-word `typed`; default `"big"`. |
| `poll_interval` | duration string | no | Overrides device/connector default. |
| `access` | `"read"` \| `"write"` \| `"read_write"` | no | Default `"read"`. |
| `unit` | string | no | Opaque hint echoed into the sample for flows. |
| `transform` | object | no | Per-point linear scale `(value*multiplier*10^decimal_shift/divisor)+offset`; see §4.2. |
| `meta` | object | no | Free-form signal metadata echoed verbatim as `meta` in every sample envelope. Never interpreted by the connector; flows read it for per-signal behaviour (e.g. `on_change`, `deadband`, `min_interval`, `debounce`). |
| `subscribe` | boolean | no | Default `true`. `false` keeps the point on the polling schedule even when the connector supports push delivery. |
| `address` | object | yes | **Protocol-specific**; shape defined by the connector spec. |

### 3.2 Protocol-specific fields

`device.protocol_address`, `connection`, and `point.address` are **opaque to the contract**:
their shape is defined by each [connector spec](../connectors/). The contract only requires
that they are objects and that each connector documents and schema-validates them.

### 3.3 Validation rules

- A point with `mode = "typed"` MUST declare a `datatype`.
- A point with `mode = "raw"` MUST NOT be rejected for missing `datatype`; decoding fields
  are ignored.
- `id` MUST be unique within a device; `name` MUST be unique within a connector.
- Duration strings follow the thin-edge convention (`"500ms"`, `"2s"`, `"5m"`).
- Unknown top-level keys SHOULD be rejected; unknown keys inside protocol-specific objects
  are delegated to the connector's own schema.

## 4. Datatypes (typed mode)

In `typed` mode the driver applies **only** primitive decoding. The contract defines this
closed set of primitive datatypes:

| `datatype` | Meaning | JSON `value` type |
| --- | --- | --- |
| `bool` | single boolean (e.g. a Modbus coil/discrete input, a digital signal) | boolean |
| `int8` / `uint8` | 8-bit integer | number |
| `int16` / `uint16` | 16-bit integer | number |
| `int32` / `uint32` | 32-bit integer | number |
| `int64` / `uint64` | 64-bit integer | number (see §4.1) |
| `float32` | IEEE-754 single | number |
| `float64` | IEEE-754 double | number |
| `string` | fixed-length text (encoding declared per connector) | string |
| `bytes` | opaque byte run (always emitted as hex) | string (hex) |

Decoding semantics:

- **Endianness** (`endianness`) selects byte order within the smallest addressable unit.
- **Word order** (`word_order`) selects the order of multi-word reads when a value spans more
  than one of the protocol's native words (for example, two Modbus 16-bit registers forming a
  32-bit value). Protocols whose values are not word-addressed simply ignore this field.
- The driver MUST NOT apply renaming, unit conversion, thresholding, or thin-edge JSON
  shaping. Those are flow responsibilities. The one numeric transform the driver MAY apply is
  the **declared per-point linear transform** (`point.transform`, §4.2): because scaling is an
  intrinsic property of a signal rather than flow logic, it is a contract-level point field whose
  math is owned by the SDK. The driver only invokes the SDK helper; it MUST NOT invent any other
  scaling, offset, or rounding.
- Bit-field extraction (start bit / bit count within a word) MAY be supported by a connector
  as a `typed` refinement and, if so, MUST be declared in that connector's spec. It is the
  one decoding refinement allowed beyond whole-primitive decode, because doing it in JS is
  error-prone.

### 4.1 64-bit integers

`int64`/`uint64` values that exceed JavaScript's safe integer range (`2^53 - 1`) MUST be
emitted as a JSON **string** in `value`, and the connector MUST set `value_repr: "string"`
in the sample (see §5). Flows can then parse with `BigInt`. Values within the safe range
MAY be emitted as numbers with `value_repr: "number"`.

### 4.2 Per-point linear transform

A point MAY declare a `transform` object. The SDK applies it to the decoded **numeric** value
immediately after primitive decode (and after bit-field extraction):

```
out = (value * multiplier * 10^decimal_shift / divisor) + offset
```

| Field | Default | Notes |
| --- | --- | --- |
| `multiplier` | `1` | |
| `divisor` | `1` | A `0` divisor is treated as `1`. |
| `decimal_shift` | `0` | Power-of-ten shift (e.g. `-3` divides by 1000); mirrors the legacy `decimalshiftright`. |
| `offset` | `0` | Added last. |

Rules:

- The transform applies **only** to `number` values. `bool`, `string`, and `bytes` values pass
  through unchanged, and it is a no-op in `raw` mode.
- The scaled value is what the sample's `value`/`value_repr` carry; `raw` always remains the
  unmodified wire bytes.
- The math is owned by the SDK so every connector scales identically. Connectors invoke the SDK
  helper rather than re-implementing it.

## 5. The sample envelope

Every successful or failed read produces exactly one **sample** message on
`te/device/<device>/ot/<protocol>/sample/<point>`. JSON Schema:
[schemas/sample.schema.json](schemas/sample.schema.json).

The envelope is protocol-neutral; only the `addr` object is protocol-specific (it echoes the
native address so flows can route or debug). The example below uses Modbus to make it concrete:

```json
{
  "ts": "2026-05-30T10:00:00.000Z",
  "ts_ms": 1780221600000.0,
  "device": "plc-1",
  "protocol": "modbus",
  "point": "boiler_temp",
  "mode": "typed",
  "datatype": "float32",
  "value": 42.5,
  "value_repr": "number",
  "raw": "422a 0000",
  "quality": "good",
  "unit": "raw",
  "addr": { "table": "holding", "address": 7, "unit_id": 1 },
  "seq": 12407
}
```

| Field | Type | Required | Notes |
| --- | --- | --- | --- |
| `ts` | string (RFC 3339, ms, UTC `Z`) | yes | Read completion time. |
| `ts_ms` | number | no | The same instant as Unix epoch milliseconds (float); the numeric companion to `ts` for consumers doing time arithmetic. |
| `device` | string | yes | thin-edge device entity id segment. |
| `protocol` | string | yes | Protocol module id. |
| `point` | string | yes | Point `id`. |
| `mode` | `"raw"` \| `"typed"` | yes | Echoes the point mode. |
| `datatype` | string | when `typed` | The primitive type decoded. |
| `value` | number \| boolean \| string | when `quality = good` | Decoded value (`typed`) — absent for `raw`. |
| `value_repr` | `"number"` \| `"boolean"` \| `"string"` | when `value` present | Tells flows how to interpret `value`. |
| `raw` | string (hex, space-grouped per word) | yes | The bytes read; always present in both modes. |
| `quality` | `"good"` \| `"bad"` \| `"stale"` | yes | See §5.1. |
| `unit` | string | no | Echo of the point's `unit` hint. |
| `addr` | object | yes | Protocol-specific address echo (for flow routing/debug). |
| `seq` | integer | no | Monotonic per-point counter; helps detect drops. |
| `error` | string | when `quality = bad` | Human-readable failure reason. |
| `meta` | object | no | The point's `meta` table echoed verbatim by the runtime (§3.1); carries per-signal hints for flows. |

### 5.1 Quality semantics

| `quality` | Meaning | `value` present? |
| --- | --- | --- |
| `good` | Read succeeded; value is current. | yes (typed) / `raw` only (raw mode) |
| `bad` | Read failed (timeout, exception, CRC). `error` set. | no |
| `stale` | Last good value re-emitted because a refresh failed but cached data exists. | yes |

- In `raw` mode there is no `value`; the payload is the `raw` hex. `quality` still applies
  (a failed raw read is `bad` with no `raw`, or `raw` omitted).
- A connector MUST publish `bad` samples for failed reads rather than silently dropping
  them, so flows and operators can react. A connector MAY rate-limit repeated `bad` samples.

## 6. Command protocol

Commands let external actors (cloud operations, other flows, operators) act on a connector —
primarily **writing** points. Commands use a request/result state machine on
`te/device/<device>/ot/<protocol>/cmd/<verb>/<id>` (retained). JSON Schema:
[schemas/command.schema.json](schemas/command.schema.json).

### 6.1 State machine

```text
init ──▶ executing ──▶ successful
                   └──▶ failed
```

| `status` | Set by | Meaning |
| --- | --- | --- |
| `init` | requester | New command request, payload includes inputs. |
| `executing` | connector | Connector accepted it and is acting. |
| `successful` | connector | Completed; `result` may carry output. |
| `failed` | connector | Failed; `reason` set. |

The requester publishes the `init` message; the connector transitions it through the
remaining states on the **same topic** (retained). A clearing (empty retained) message ends
the command lifecycle.

### 6.2 The `write` verb (standard)

Request (`status: "init"`):

```json
{
  "status": "init",
  "point": "setpoint",
  "value": 21.5,
  "value_repr": "number"
}
```

- For a `typed`-writable point, `value` is the logical value and the connector encodes it per
  the point's `datatype`/`endianness`/`word_order`.
- For a `raw`-writable point, the request MUST instead provide `raw` (hex) and the connector
  writes those bytes verbatim.
- The connector MUST reject (`failed`) a write to a point whose `access` does not permit it.

Result (`status: "successful"`):

```json
{ "status": "successful", "point": "setpoint", "value": 21.5 }
```

Result (`status: "failed"`) — `reason` is free text; the Modbus wording here is illustrative:

```json
{ "status": "failed", "point": "setpoint", "reason": "modbus exception: illegal data address" }
```

### 6.3 Management verbs (standard, SDK-provided)

Beyond point I/O, every connector needs to be (re)configured at runtime: change a poll
interval, adjust serial parameters, add a device, etc. Rather than inventing a bespoke
operation per protocol (as the legacy Modbus plugin did with `c8y_ModbusConfiguration`,
`c8y_SerialConfiguration`, `c8y_ModbusDevice`, `c8y_Coils`, `c8y_Registers`), the contract
defines a small set of **protocol-neutral management verbs**. They are implemented once in the
SDK runtime — it owns the connector configuration document — so every protocol module gets them
for free without any extra code.

A connector that uses the SDK runtime MUST advertise these verbs (and the `management` feature)
in its capability descriptor (§7). All three follow the same `init → executing →
successful/failed` state machine as `write`, on
`te/device/<device>/ot/<protocol>/cmd/<verb>/<id>`. After a successful management command the
runtime persists the updated configuration to disk and live-reloads the connector (re-validate,
reconnect, reschedule) — no service restart is required.

> Management commands mutate the connector's own configuration; they are typically targeted at
> the connector's main device (`te/device/main/ot/<protocol>/cmd/<verb>/<id>`), but the
> `<device>` segment is not otherwise interpreted by these verbs (the affected device is named in
> the payload).

#### `set-config` — patch connector configuration

Applies a deep-merged patch to one section of the configuration document. Replaces
`c8y_ModbusConfiguration` (poll/transmit rate) and `c8y_SerialConfiguration` (serial parameters).

Request (`status: "init"`):

```json
{
  "status": "init",
  "target": "connector",
  "config": { "poll_interval": "5s", "log_level": "debug" }
}
```

- `target` selects the section to patch: `"connector"`, `"mqtt"`, `"connection"` (shared
  protocol defaults, e.g. `{ "serial": { "baudrate": 19200 } }`), or `"device:<name>"` to patch a
  single device's fields (its `point` list is left untouched unless included).
- `config` is deep-merged into the target section (objects merge recursively; scalars and arrays
  replace).
- The runtime rejects (`failed`) a patch that produces an invalid configuration.

#### `define-device` — add or replace a device

Inserts a device (transport + points) into the configuration, or replaces an existing device with
the same `name`. Replaces `c8y_ModbusDevice` together with the point definitions that
`c8y_Coils`/`c8y_Registers` used to stage. Child-device registration in the cloud is then handled
by the existing registration flow when the device link comes up.

Request (`status: "init"`):

```json
{
  "status": "init",
  "device": {
    "name": "plc-9",
    "protocol_address": { "transport": "tcp", "host": "10.0.0.9", "port": 502, "unit_id": 1 },
    "default_mode": "typed",
    "point": [
      { "id": "temp", "datatype": "float32", "access": "read_write",
        "address": { "table": "holding", "address": 7, "count": 2 } }
    ]
  }
}
```

The `device` object uses the same shape as a `[[device]]` entry in the configuration file (note
the point list key is `point`, matching the file's `[[device.point]]`).

#### `remove-device` — delete a device

Removes the named device (and its points) from the configuration and disconnects it.

```json
{ "status": "init", "device": "plc-9" }
```

#### Other verbs

`write` is the only point-I/O verb a conformant connector MUST support (for writable points), and
SDK-based connectors additionally provide the three management verbs above. Connectors MAY support
further verbs (e.g. `read-now`, `rescan`); any such verb MUST be declared in the capability
descriptor (§7) and documented in the connector spec.

## 7. Capability model

On startup a connector MUST publish a retained **capability descriptor** to
`te/device/main/service/<service>/ot/capabilities`. JSON Schema:
[schemas/status.schema.json](schemas/status.schema.json) (`capabilities` definition).

The fields are protocol-neutral; the **values** describe what a given connector supports. The
example below is the Modbus connector's descriptor — a CAN or OPC-UA connector publishes the
same fields with its own values (and typically `"subscribe": true`):

```json
{
  "protocol": "modbus",
  "version": "0.1.0",
  "modes": ["raw", "typed"],
  "datatypes": ["bool", "int16", "uint16", "int32", "uint32", "float32", "float64"],
  "point_kinds": ["coil", "discrete_input", "holding_register", "input_register"],
  "command_verbs": ["write", "set-config", "define-device", "remove-device"],
  "features": ["polling", "bitfield", "management"],
  "subscribe": false
}
```

| Field | Meaning |
| --- | --- |
| `modes` | Output modes supported. MUST include at least one of `raw`/`typed`. |
| `datatypes` | Subset of §4 the connector can decode in `typed` mode. |
| `point_kinds` | Protocol-specific kinds the connector understands (free strings, documented per spec). |
| `command_verbs` | Verbs accepted on `cmd/<verb>`. MUST include `write` if any point is writable. |
| `features` | Optional capability tags: `polling`, `subscribe`, `bitfield`, `string`, `bulk_read`, … |
| `subscribe` | Whether the connector supports event-driven (push) reads in addition to polling. |

Tooling and the conformance suite use the descriptor to decide which tests apply.

## 8. Status and health

- The connector MUST publish a retained service health message to
  `te/device/main/service/<service>/status/health` with at least
  `{"status":"up"|"down","time":"<rfc3339>"}` on startup and on health changes, following
  the thin-edge service health convention.
- For each device, the connector SHOULD publish a retained link-status message to
  `te/device/<device>/ot/<protocol>/status/link`:

  ```json
  { "status": "connected", "since": "2026-05-30T09:59:00.000Z" }
  ```

  with `status` ∈ `{"connected","disconnected","degraded"}` and an optional `reason`.

## 9. Timestamps, encoding and ordering

- All timestamps MUST be RFC 3339 / ISO 8601, millisecond precision, UTC with a `Z` suffix.
- All payloads MUST be UTF-8 JSON.
- `raw` hex MUST be lowercase or uppercase consistently within a connector; words SHOULD be
  space-separated in protocol-natural width (e.g. per 16-bit register for Modbus).
- Samples for a single point SHOULD be published in read order; the optional `seq` field lets
  consumers detect reordering or loss.

## 10. Versioning

- This contract is versioned (`Version` header). Connectors declare the contract version they
  target in their capability descriptor's `version` (their own version) and SHOULD document
  the contract version they implement in their spec.
- Backward-incompatible changes to topics, required fields, or the command state machine
  require a new contract major version and an [RFC](../community/community-model.md).

## 11. Conformance

A connector is **contract-conformant** when it:

1. publishes valid samples (§5) for every configured point in its declared modes,
2. publishes a valid capability descriptor (§7) and health/status (§8),
3. implements the `write` verb (§6) for all writable points,
4. validates its configuration (§3) and protocol-specific schemas,
5. passes the shared [conformance suite](../conformance/conformance-suite.md), including the
   golden decode vectors for every `typed` datatype it advertises.

See the conformance document for the executable definition of these requirements.
