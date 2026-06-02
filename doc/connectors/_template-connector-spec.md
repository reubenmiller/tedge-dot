# `<protocol>` Connector Spec — Template

> Copy this file to `connectors/<protocol>-connector-spec.md` and fill in every section.
> A spec is "done" when an implementer (human or AI agent) can build the connector from it
> plus the [SDK spec](../sdk/connector-sdk.md) and pass the
> [conformance suite](../conformance/conformance-suite.md). Keep prose minimal and tables
> precise. Anything you leave vague will be implemented inconsistently.

| Field | Value |
| --- | --- |
| Status | Draft / Implementable |
| Protocol id | `<protocol>` |
| Crate | `connector-<protocol>` (feature `<protocol>`) |
| Builds on | `<rust crate(s) used>`, [Connector SDK](../sdk/connector-sdk.md) |
| Implements | [OT Connector Contract](../contract/ot-connector-contract.md) v0.1.0 |

## 1. Scope

State what this connector reads/writes and explicitly that scaling/naming/alarms/shaping are
flow responsibilities. List the protocol operations supported and the underlying crate calls.

| Object kind | Read | Write | Library call |
| --- | --- | --- | --- |
| `<kind>` | yes/no | yes/no | `<call>` |

## 2. Capability descriptor

Provide the exact JSON the connector publishes (contract §7). Set `subscribe` correctly: is
this protocol polled, push, or both?

```json
{
  "protocol": "<protocol>",
  "version": "0.1.0",
  "modes": ["raw", "typed"],
  "datatypes": ["..."],
  "point_kinds": ["..."],
  "command_verbs": ["write"],
  "features": ["polling"],
  "subscribe": false
}
```

## 3. Protocol-specific configuration

Define the shape of the three opaque contract slots and provide a JSON Schema
(`<protocol>.schema.json`).

### 3.1 `connection`
### 3.2 `device.protocol_address`
### 3.3 `point.address`

For each, give a table of fields (name, required, notes) and a TOML example.

## 4. Decoding rules (typed mode)

Specify how raw bytes map to the contract datatypes via the SDK `decode_primitive` helper:

- What is the smallest addressable unit and its native endianness?
- How do `endianness` and `word_order` apply (or not)?
- Datatype → read-size table.
- Any allowed refinement (e.g. bit-fields), declared as a `feature`.

## 5. Read flow

Describe `read_points` (polled) and/or `subscribe` (push): batching strategy, error → `bad`
sample behaviour, what goes in `addr`.

## 6. Write flow

Describe `execute` for verb `write` (and any extra verbs you declare): typed vs raw encoding,
access checks, read-modify-write if needed.

## 7. Status

Describe link-status semantics for this protocol (what counts as connected/degraded).

## 8. Acceptance test vectors

Provide normative vectors: input bytes/frames → expected `Sample`. Cover every advertised
datatype, both word orders if applicable, a bad read, and a write round-trip. These feed the
conformance suite.

## 9. Implementation checklist

A numbered list an agent can follow end-to-end.

---

# Capability sketches for upcoming protocols

These are **not** full specs — they are starting notes to show the contract generalizes and to
seed future spec PRs. Each will graduate to its own file using the template above.

## CAN bus (`canbus`)

| Aspect | Sketch |
| --- | --- |
| Library | `socketcan` (Linux SocketCAN) |
| Nature | **Push** (frames arrive asynchronously) → implements `subscribe`, `subscribe: true`. |
| Device | A CAN interface, e.g. `{ interface = "can0", bitrate = 500000 }`. |
| Point | A signal within a frame: `{ can_id = "0x1A0", start_bit, bit_count, byte_order, value_type }` (DBC-like). |
| Typed | Decode a signal as int/uint/float from a bit range of an 8-byte frame; `endianness` ≈ Intel/Motorola byte order. |
| Raw | Emit the full 8-byte frame payload as hex. |
| Write | `write` verb sends a CAN frame (for points whose `access` allows). |
| Notes | A flow can apply the DBC scale/offset; the connector only extracts the bit-field. Subscription model fits the SDK `subscribe` path directly. |

## BACnet/IP (`bacnet`)

| Aspect | Sketch |
| --- | --- |
| Library | a Rust BACnet stack (e.g. `bacnet-rs`) or FFI to a C stack |
| Nature | Polled (ReadProperty) **and** push (COV subscription) → `subscribe: true`, `features: ["polling","cov"]`. |
| Device | `{ device_instance = 1234, address = "192.168.0.50:47808" }`. |
| Point | An object property: `{ object_type = "analog-input", instance = 3, property = "present-value" }`. |
| Typed | BACnet already carries typed values (Real, Unsigned, Boolean, Enumerated); map to contract datatypes; `endianness`/`word_order` largely N/A. |
| Raw | Emit the encoded APDU value as hex for unusual types. |
| Write | `write` verb → WriteProperty (respecting priority array could be a future verb extension). |
| Notes | Because BACnet is self-describing, `typed` decode is mostly a type map; the flow still does engineering-unit naming and thresholds. |

## OPC-UA (`opcua`)

| Aspect | Sketch |
| --- | --- |
| Library | `opcua` crate (client) |
| Nature | Polled (Read) **and** push (MonitoredItems/Subscriptions) → `subscribe: true`. |
| Device | An OPC-UA server endpoint + security: `{ endpoint = "opc.tcp://host:4840", security_policy, auth }`. |
| Point | A node: `{ node_id = "ns=2;s=Boiler.Temp" }`. |
| Typed | OPC-UA `Variant` is already typed (Double, Int32, Boolean, String…); map to contract datatypes; carry the source timestamp into `ts`; map StatusCode → `quality`. |
| Raw | Emit the encoded variant bytes as hex when the type is exotic. |
| Write | `write` verb → Write service call. |
| Notes | OPC-UA's StatusCode maps cleanly to the contract's `good`/`bad`/`stale` quality. Subscriptions are the natural mode; polling is a fallback. The flow handles renaming and any unit conversion. |

These four sketches share one config model, one sample envelope, one command protocol, and one
conformance harness — which is the entire point of the contract.
