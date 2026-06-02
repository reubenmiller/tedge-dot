# OT Connector Conformance Suite

| Field | Value |
| --- | --- |
| Status | Draft |
| Applies to | every connector implementing the [OT Connector Contract](../contract/ot-connector-contract.md) |

The conformance suite is what makes a community of connectors trustworthy. A connector that
passes it behaves identically — at the contract boundary — to every other connector, so
flows, tooling, dashboards, and operators can rely on it. The suite is designed so a
contributor (human or AI agent) can prove a new connector correct **without hardware**.

## 1. Structure

The suite has three layers, run in order:

```text
 ┌────────────────────────────────────────────────────────────────────┐
 │ Layer 1 — Schema conformance (static, no connector running)        │
 │   validate sample/command/status/config payloads vs JSON Schemas    │
 ├────────────────────────────────────────────────────────────────────┤
 │ Layer 2 — Decode conformance (pure functions, no I/O)              │
 │   golden vectors: bytes + datatype + endianness/word_order → value  │
 ├────────────────────────────────────────────────────────────────────┤
 │ Layer 3 — Behavioural conformance (connector ⇄ protocol simulator)  │
 │   run the real connector against a simulator, assert MQTT traffic    │
 └────────────────────────────────────────────────────────────────────┘
```

Layers 1–2 are cheap, deterministic, and the bulk of the value. Layer 3 needs a per-protocol
simulator but reuses one harness.

## 2. Layer 1 — Schema conformance

Every message a connector emits MUST validate against the contract schemas:

- samples → [sample.schema.json](../contract/schemas/sample.schema.json)
- commands → [command.schema.json](../contract/schemas/command.schema.json)
- status/capabilities → [status.schema.json](../contract/schemas/status.schema.json)
- config → [config.schema.json](../contract/schemas/config.schema.json) + the connector's own
  protocol schema

The harness captures published payloads (Layer 3) and validates them, and also validates the
static example payloads embedded in each schema's `examples` array. A connector PR that breaks
the envelope fails here immediately.

## 3. Layer 2 — Decode conformance (golden vectors)

This is the heart of correctness for `typed` mode. Because all connectors decode through the
SDK's `decode_primitive`/`encode_primitive`, the golden vectors live **once** in the SDK and
every connector that advertises a datatype must pass the vectors for it.

Vectors are stored as data (JSON), not code, so they are language-neutral and AI-auditable:

```json
{
  "id": "float32-be-bigword-42_5",
  "datatype": "float32",
  "endianness": "big",
  "word_order": "big",
  "bytes": "422a0000",
  "expect": { "value": 42.5, "value_repr": "number" }
}
```

The Modbus reference vectors in
[modbus-connector-spec §9](../connectors/modbus-connector-spec.md#9-acceptance-test-vectors)
are the canonical seed set. The full SDK vector file MUST include, for each advertised
datatype:

| Case | Why |
| --- | --- |
| nominal value, big endian / big word | baseline |
| signed negative (int types) | two's-complement |
| both `word_order` values (multi-word types) | word swap |
| both `endianness` values (where meaningful) | byte swap |
| min/max representable | range edges |
| IEEE-754 specials for floats (`+0`, `-0`, `inf`, `nan` handling policy) | float edge cases |
| bit-field extraction (if `bitfield` advertised) | masking |
| encode→decode round-trip | write path symmetry |

A connector under test runs each applicable vector through its read path (decode) and, for
writable types, the round-trip through its write path (encode), and asserts the `expect`.

## 4. Layer 3 — Behavioural conformance

Layer 3 runs the **real connector binary** against a **protocol simulator** and a test MQTT
broker, then asserts the MQTT side of the contract.

```text
 ┌───────────────┐   protocol    ┌──────────────────┐   MQTT    ┌──────────────────┐
 │  Protocol     │◀─────────────▶│ tedge-dot│◀────────▶│  test broker +   │
 │  simulator    │   (e.g. TCP)  │ (under test)      │           │  assertion probe │
 └───────────────┘               └──────────────────┘           └──────────────────┘
```

Reusable harness, per-protocol simulator. For Modbus the simulator is the existing
[`images/simulator`](../../../images/simulator/) Modbus server (or a `tokio-modbus` server),
seeded with known register/coil contents.

### 3.1 Required behavioural checks

| # | Check | Pass criterion |
| --- | --- | --- |
| B1 | Startup | Capability descriptor (retained) + service health `up` published. |
| B2 | Sample publishing | For each configured point, a sample on `…/sample/<point>` validating Layer 1, with the value matching the simulator's seeded data (cross-checked via Layer 2 decode). |
| B3 | Modes | A `typed` point yields `value`+`value_repr`; a `raw` point yields `raw` only. |
| B4 | Quality | A simulated read failure yields a `bad` sample with `error`, not a dropped message. |
| B5 | Link status | `status/link` transitions to `connected`; to `disconnected`/`degraded` when the simulator drops. |
| B6 | Write verb | A `cmd/write/<id>` `init` drives `executing`→`successful`; the simulator observes the written value; round-trips through a subsequent read. |
| B7 | Access control | A write to a `read`-only point yields `failed` with a reason and no simulator write. |
| B8 | Hot reload | Editing config (add a point) is picked up without restart; new point starts publishing. |
| B9 | Capability honesty | The connector never emits a datatype/mode/verb it did not advertise. |
| B10 | Topic discipline | The connector publishes only under its contract topics; never to `…/m/`, `…/e/`, `…/a/`. |

### 3.2 Flow integration smoke test

Optionally, the harness pipes captured samples through the reference flows with
`tedge flows test` and asserts the standard measurement/alarm output — proving the
end-to-end driver→flow story for that connector.

## 5. Conformance manifest

Each connector ships a `conformance.toml` declaring what it claims, so the harness selects the
applicable vectors and behavioural checks:

```toml
[connector]
protocol  = "modbus"
modes     = ["raw", "typed"]
datatypes = ["bool", "int16", "uint16", "int32", "uint32", "float32", "float64"]
verbs     = ["write"]
features  = ["polling", "bitfield"]
subscribe = false

[simulator]
kind  = "modbus-tcp"
image = "images/simulator"
seed  = "conformance/seed/modbus.json"
```

The harness cross-checks the manifest against the live capability descriptor (B9): they MUST
agree.

## 6. Running it

```sh
# Layers 1 & 2 — fast, no simulator:
ot-conformance check --spec ./conformance.toml --static

# Layer 3 — with the protocol simulator + a test broker:
ot-conformance check --spec ./conformance.toml --behavioural
```

`ot-conformance` is a small harness binary (part of the workspace). It exits non-zero on any
failure and emits a machine-readable report (JUnit + JSON) suitable for CI and for an AI agent
to consume and self-correct against.

## 7. Definition of "conformant"

A connector is conformant for a release when, on that release:

1. Layer 1 passes for all emitted message types.
2. Layer 2 passes for every datatype in its manifest (read and, where writable, round-trip).
3. Layer 3 checks B1–B10 pass against the declared simulator.
4. The manifest and the live capability descriptor agree.

Only conformant connectors are eligible for the [registry](../community/community-model.md)
"verified" tier.
