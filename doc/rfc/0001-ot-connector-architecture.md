# RFC 0001 — OT Connector Architecture

| Field | Value |
| --- | --- |
| RFC | 0001 |
| Title | OT Connector Architecture: dumb drivers, smart flows |
| Status | Draft |
| Author(s) | thin-edge.io community |
| Created | 2026-05-30 |
| Supersedes | The monolithic Python `tedge_modbus` plugin |
| Related | [OT Connector Contract](../contract/ot-connector-contract.md), [Connector SDK](../sdk/connector-sdk.md) |

## 1. Summary

This RFC proposes a new architecture for connecting Operational Technology (OT) field
protocols — starting with Modbus — to thin-edge.io. It splits the current monolithic
plugin into two cleanly separated halves:

1. A **dumb protocol driver** (`tedge-dot`) written in Rust. It performs transport
   and, optionally, primitive decoding, then publishes a **protocol-neutral sample
   envelope** to MQTT. It performs **no** business transformation.
2. **thin-edge.io flows** (JavaScript) that own all transformation: scaling, renaming,
   units, thin-edge JSON shaping, alarms, events, aggregation, and entity registration.

All protocols share one **OT Connector Contract** (config + MQTT + commands + capabilities),
so the driver, the tooling, the tests, and the flows are written once and reused across
Modbus, CAN, BACnet, OPC-UA, and beyond.

## 2. Motivation

### 2.1 The current design concentrates unrelated concerns

The Python plugin's [`reader/mapper.py`](../../../tedge_modbus/reader/mapper.py) is
simultaneously responsible for protocol decoding, bit math, endianness, scaling, change
detection, alarm/event state machines, and thin-edge JSON generation. The transport layer
in [`reader/reader.py`](../../../tedge_modbus/reader/reader.py) is interwoven with that
mapping. Cumulocity-specific behaviour lives in
[`operations/`](../../../tedge_modbus/operations/).

This has three consequences:

- **High cost to add a protocol.** A new protocol must re-implement the entire stack.
- **Transformation is locked away.** The logic domain experts most want to change (scaling,
  thresholds, naming) requires editing and redeploying the driver.
- **Hard to test in isolation.** Decoding correctness, mapping correctness, and transport
  correctness cannot be exercised separately.

### 2.2 thin-edge.io already solves the transformation half

[Flows](https://thin-edge.github.io/thin-edge.io/extend/flows/) provide a sandboxed,
hot-reloadable, offline-testable JavaScript runtime *inside the mapper*, with first-class
packaging and distribution. They are purpose-built for "convert a custom payload into the
thin-edge format," "raise an alarm on a threshold," and "aggregate to save bandwidth" —
exactly the work currently trapped inside the driver.

### 2.3 We want a community, and AI agents, to extend OT support

The strategic goal is breadth of protocol support. That only happens if adding a protocol is
small, well-specified, and verifiable. A shared contract + SDK + conformance suite turns
"add a protocol" into a bounded task that a contributor — or an AI coding agent working from
a machine-readable spec — can complete and prove correct.

## 3. Goals and non-goals

### Goals

- A protocol-neutral contract usable by Modbus, CAN, BACnet, OPC-UA and similar.
- A driver that is as dumb as practical, with a configurable raw/typed boundary.
- Transformation expressed as flows that are shareable and testable.
- A machine-readable spec set suitable for AI-agent implementation.
- A conformance suite and community process that make third-party connectors realistic.

### Non-goals

- Backward compatibility with the existing plugin (this is a greenfield design; see the
  [migration guide](../migration/migration-guide.md)).
- Building the production Rust crates or flow packages in this proposal.
- A dynamic plugin runtime in v1 (sketched as future work only).

## 4. Architecture

### 4.1 Layered model

```text
   ┌─────────────┐   ┌────────────────────┐   ┌──────────────────────┐
   │  Transport  │──▶│  Decode (optional) │──▶│  Publish sample      │
   │  protocol   │   │  raw │ typed       │   │  envelope to MQTT    │
   └─────────────┘   └────────────────────┘   └──────────────────────┘
          ▲                                              │
          │ write_point                                  ▼  (te/device/<d>/ot/<proto>/...)
   ┌─────────────┐                              ┌──────────────────────┐
   │  Command    │◀── cmd/write request ────────│  thin-edge.io flows  │
   │  router     │──▶ result (init→…→done)      │  (all transformation)│
   └─────────────┘                              └──────────┬───────────┘
                                                            ▼
                                                 te/device/<d>///m|e|a/...
```

- **Transport** is the only protocol-aware layer for reads. It speaks Modbus/CAN/etc.
- **Decode** is optional and declared per point. In `raw` mode the driver emits the bytes it
  read (hex) with no interpretation. In `typed` mode it applies *only* primitive decoding —
  datatype and endianness — to produce a number or boolean. It never scales, renames, or
  thresholds.
- **Publish** wraps the result in the canonical sample envelope and publishes it under a
  neutral source subtopic of the device entity.
- **Flows** subscribe to those samples and produce standard thin-edge measurements, events
  and alarms. This is where all "meaning" is applied.
- **Command router** (in the SDK) turns `cmd/write` requests into `write_point` calls and
  reports results using the thin-edge command state machine.

### 4.2 Where the raw/typed line sits

The driver boundary is **configurable per point**:

| Mode | Driver does | Flow does |
| --- | --- | --- |
| `raw` | read bytes → publish hex + address metadata | decode datatype, endianness, scale, name, shape |
| `typed` | read bytes → decode datatype + endianness → publish scalar/bool | scale, name, units, shape, threshold |

`typed` is the recommended default for simple scalar points because primitive decoding is
fiddly to do correctly in JavaScript (endianness, word order, IEEE-754). `raw` exists for
exotic encodings, bulk capture, or when a flow author wants total control. Crucially, even
in `typed` mode the driver stops at *decoding* — all *business* transformation is a flow.

### 4.3 Single binary, pluggable modules

`tedge-dot` is one binary. Each protocol is a module compiled in behind a cargo
feature flag (`--features modbus`, `--features opcua`, …), all implementing a shared
[`Connector` trait](../sdk/connector-sdk.md) and running on the shared
`tedge-dot-sdk` runtime. This keeps a single, consistent operational surface
(one service, one config layout, one health model) while letting builds include only the
protocols they need. A dynamic-plugin loading path is possible later but is explicitly out
of scope for v1.

### 4.4 Protocol-neutral contract

Everything above transport is described by the [OT Connector Contract](../contract/ot-connector-contract.md):

- a **config model** (connection + devices + points, with protocol-specific sub-objects),
- a **sample envelope** (the read result),
- a **status/health** model,
- a **command protocol** for writes and other verbs,
- a **capability model** so a connector advertises what it can do.

Because the contract is protocol-neutral, a flow that scales and renames Modbus samples
works unchanged on OPC-UA samples; a dashboard or test harness written against the contract
works for every connector.

### 4.5 Event-driven as well as polled

Some protocols (CAN, OPC-UA subscriptions, BACnet COV) push data rather than being polled.
The `Connector` trait therefore exposes **both** `read_points` (for polled protocols) and
`subscribe` (for event-driven protocols) from day one, so the contract does not have to
break when the second protocol arrives.

## 5. Worked example (Modbus)

1. **Config** declares a holding register point in `typed` mode as `float32`, big-endian
   word order.
2. The **Modbus module** reads two 16-bit registers via `tokio-modbus`, decodes them to an
   `f32`, and publishes:

   ```json
   {
     "ts": "2026-05-30T10:00:00.000Z",
     "device": "plc-1",
     "protocol": "modbus",
     "point": "boiler_temp_raw",
     "mode": "typed",
     "datatype": "float32",
     "value": 42.5,
     "raw": "422a 0000",
     "quality": "good",
     "addr": { "table": "holding", "address": 7, "unit_id": 1 }
   }
   ```

3. A **scaling flow** multiplies by a calibration factor and renames it into a thin-edge
   measurement on `te/device/plc-1///m/environment`.
4. A **threshold flow** raises `te/device/plc-1///a/boiler_overheat` when the scaled value
   crosses a configurable limit, with hysteresis.

No Rust was changed to add calibration or the alarm; both are flows the user can edit and
re-test with `tedge flows test`.

## 6. Alternatives considered

| Alternative | Why not |
| --- | --- |
| **Keep Python, refactor internally** | Doesn't address the core coupling, doesn't unlock flows, and Python deployment footprint (interpreter + libs) is heavy for constrained gateways. |
| **One binary per protocol (no shared SDK)** | Duplicates the runtime, config, health and command code per protocol; inconsistent operational surface; no shared conformance leverage. |
| **Keep transformation in the driver, add a config DSL** | Re-invents a worse version of flows; still requires redeploying the driver to change logic; no offline test story comparable to `tedge flows test`. |
| **Pure `raw` driver only** | Pushes endianness/IEEE-754 decoding into JavaScript for every user; error-prone and repetitive. The configurable `typed` mode keeps the easy 90% easy. |
| **Dynamic plugins in v1** | Adds ABI/stability complexity before there is a second protocol to justify it; feature flags deliver modularity now. |

## 7. Risks and mitigations

| Risk | Mitigation |
| --- | --- |
| Flow performance for high-rate points | Keep `typed` decoding in Rust; support aggregation flows; allow per-point publish throttling in the connector. |
| Contract churn as new protocols arrive | Ratify the contract via the [RFC process](../community/community-model.md); version it; include `subscribe` and capability model up front. |
| Fragmented community connectors | A [conformance suite](../conformance/conformance-suite.md) + [registry](../community/community-model.md) gate quality and discoverability. |
| Migration effort for existing users | Provide a [migration guide](../migration/migration-guide.md) and tooling that maps `devices.toml` to new config + starter flows. |
| Loss of C8y operations behaviour | Re-express operations as command-contract verbs + flows; document the mapping in migration. |

## 8. Adoption and rollout

Delivery is phased (see the [roadmap](../roadmap.md)): SDK + contract + Modbus `typed`
first; then `raw` mode, the command/write path, and the reference flows; then the
conformance suite and registry; then community protocols. Each phase is independently
useful and independently shippable.

## 9. Open questions

1. **Source topic namespace.** This proposal uses `te/device/<device>/ot/<protocol>/...` so
   the local mapper and flows pick samples up naturally. An alternative neutral prefix
   (e.g. `src/ot/...`) is possible; the contract isolates this choice in one place.
2. **MQTT client crate.** The SDK assumes `rumqttc` (pure Rust). `paho-mqtt` is an
   alternative if a feature gap appears.
3. **Per-point vs per-device default mode.** The contract allows both; the default
   (`typed`) is set at the point level with device-level inheritance.
4. **Command verbs beyond `write`.** Whether to standardise verbs like `read-now`,
   `rescan`, or protocol-specific verbs in v1 or defer to capability extensions.

These do not block ratification; they are tracked against the contract document.
