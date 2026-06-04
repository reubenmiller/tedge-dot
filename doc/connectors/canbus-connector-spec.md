# CAN Bus Connector Spec

| Field | Value |
| --- | --- |
| Status | Implementable draft |
| Protocol id | `canbus` |
| Crate | `connector-canbus` (feature `canbus`) |
| Builds on | [`socketcan`](https://crates.io/crates/socketcan) v3 (Linux SocketCAN), [`can-dbc`](https://crates.io/crates/can-dbc) v4, [Connector SDK](../sdk/connector-sdk.md) |
| Implements | [OT Connector Contract](../contract/ot-connector-contract.md) v0.1.0 |

This spec is written to be precise enough for a developer or AI agent to implement from this
document plus the SDK spec, and verify correctness against the [acceptance vectors](#9-acceptance-test-vectors)
without hardware.

---

## 1. Scope

The CAN bus connector reads signals from CAN frames via Linux SocketCAN and writes CAN frames
for command verbs. Signals are addressed by their name in an external
[DBC](https://docs.vector.com/vctools/dbc_editor/dbc_spec.html) file; the connector extracts
raw bit-fields and decodes them to typed values.

Apart from the per-point linear `transform` (applied by the connector per contract §4.2), all
scaling, renaming, units, alarms, events, and thin-edge JSON shaping are **out of scope** and
handled by [flows](../flows/). This is deliberate: DBC files often carry `factor`/`offset`
entries — those MUST be applied in a flow or via `point.transform`, never automatically by the
connector.

CAN is **push-based** (frames arrive asynchronously from the kernel ring buffer). The connector
implements `subscribe()` rather than `read_points()`. `read_points()` returns
`ConnectorError::Unsupported`.

**Optional CAN FD support** is gated behind the `canbus-fd` Cargo feature. When enabled:
- Frames up to 64 bytes are accepted.
- The `"canbus-fd"` string is included in the `features` array of the capability descriptor.
- The DBC file format must use extended CANDB++ FD message definitions for FD signals.

Supported operations via SocketCAN:

| Operation | Supported | Notes |
| --- | --- | --- |
| Receive CAN frame | yes | Async via `SocketCAN::recv_frame()` |
| Send CAN frame | yes | `write` verb |
| Receive CAN FD frame | feature `canbus-fd` | `CanFdSocket` |
| Filter by CAN ID | yes | Kernel-level `HwFilter` per subscribed message |

---

## 2. Capability descriptor

The connector MUST publish:

```json
{
  "protocol": "canbus",
  "version": "0.1.0",
  "modes": ["raw", "typed"],
  "datatypes": ["bool", "uint8", "int8", "uint16", "int16", "uint32", "int32", "uint64", "int64", "float32", "float64"],
  "point_kinds": ["signal"],
  "command_verbs": ["write", "set-config", "define-device", "remove-device"],
  "features": ["subscribe", "management"],
  "subscribe": true
}
```

When compiled with feature `canbus-fd`:

```json
{
  "features": ["subscribe", "management", "canbus-fd"],
  ...
}
```

`subscribe` is `true` because CAN is push-based. The SDK runtime adds the management verbs and
the `management` feature to the list returned by `capabilities()` — the connector itself only
declares `write`.

---

## 3. Protocol-specific configuration

These objects fill the contract's opaque slots. They MUST be schema-validated by the
`canbus.schema.json` shipped with this spec.

### 3.1 `connection`

No global connection parameters are required for SocketCAN. The `connection` block MUST be
accepted as an empty object `{}` or omitted.

### 3.2 `device.protocol_address`

```toml
protocol_address = { interface = "can0", bitrate = 500000, dbc_file = "/etc/tedge/vehicle.dbc" }
```

| Field | Required | Notes |
| --- | --- | --- |
| `interface` | yes | SocketCAN interface name (e.g. `"can0"`, `"vcan0"`). |
| `bitrate` | no | Nominal bit rate in bit/s; logged for diagnostics, not set by the connector (use `ip link set can0 type can bitrate 500000` before starting). |
| `dbc_file` | yes | Absolute path to the DBC file defining messages and signals for this device. |

### 3.3 `point.address`

```toml
address = { message_name = "ENGINE_STATUS", signal_name = "RPM" }
```

| Field | Required | Notes |
| --- | --- | --- |
| `message_name` | yes | Name of the DBC `BO_` message block (e.g. `"ENGINE_STATUS"`). |
| `signal_name` | yes | Name of the DBC `SG_` signal within that message (e.g. `"RPM"`). |

The connector resolves these names at `configure()` time: if the DBC file does not contain the
named message or signal, `configure()` returns `ConfigError::Invalid`. The resolved CAN ID,
`start_bit`, `bit_count`, `byte_order`, and value type (unsigned/signed/float) are stored in
memory and used on every received frame.

---

## 4. Decoding rules (typed mode)

### 4.1 CAN signal bit extraction

CAN signals are packed into an 8-byte (classic) or up to 64-byte (FD) frame payload using one
of two byte orders described in DBC files:

**Intel byte order (little-endian):**

- `start_bit` addresses the LSBit of the signal in the frame. Bit numbering: bit 0 is bit 0 of
  byte 0 (least-significant bit of the first byte).
- The signal occupies `bit_count` contiguous bits counted upward from `start_bit`.
- Byte boundaries are crossed naturally: bit 7 is the MSBit of byte 0, bit 8 is the LSBit of
  byte 1, etc.
- Extraction algorithm:
  1. Flatten the frame payload to a 64-bit integer in **little-endian** byte order (byte 0 = bits 0–7).
  2. Right-shift by `start_bit`, mask with `(1 << bit_count) - 1`.

**Motorola byte order (big-endian):**

- `start_bit` addresses the MSBit of the signal. DBC Motorola convention numbers bits as:
  bit 7 = MSBit of byte 0, bit 0 = LSBit of byte 0, bit 15 = MSBit of byte 1, etc.
  (each byte's bits are numbered 7..0, then the pattern repeats for byte 1 starting at 15..8).
- The signal occupies `bit_count` bits, with `start_bit` as the MSBit, counting down through
  lower bit positions.
- Extraction algorithm:
  1. Determine MSBit (`start_bit`) and compute the LSBit position by following Motorola bit
     counting: `lsb_byte = msb_byte + floor(bit_count / 8)`, adjusting for partial bytes.
  2. Collect `bit_count` bits from the frame, MSBit first, building the result in big-endian
     order.
  3. See §9 acceptance vectors for worked examples.

This bit extraction logic lives in the `extract_can_signal(frame_bytes, start_bit, bit_count, byte_order) -> u64`
private function in `crates/connector-canbus/src/lib.rs`. It is **not** the SDK's
`extract_bitfield` helper (which uses Modbus register-word-order semantics).

### 4.2 Datatype interpretation after extraction

Once the raw unsigned `u64` is extracted:

| DBC value type | Connector action |
| --- | --- |
| Unsigned | Interpret as `u64`, return `Value::Number(n as f64)`. |
| Signed | Sign-extend from `bit_count` bits to `i64`, return `Value::Number(n as f64)`. |
| Float (IEEE-754, 32-bit) | Reinterpret extracted `u32` bits via `f32::from_bits`, return `Value::Number(n as f64)`. |
| Float (IEEE-754, 64-bit) | Reinterpret extracted `u64` bits via `f64::from_bits`. |

After extraction and type interpretation the SDK `Transform::apply()` is called for the
per-point `transform` (multiplier/divisor/offset). The DBC `factor` and `offset` fields are
**not applied** by the connector.

`int64`/`uint64` values outside the JS safe-integer range MUST be emitted as `Value::Text`
with `value_repr: "string"` (contract §4.1).

### 4.3 Raw mode

In `raw` mode the full frame payload (8 bytes classic / up to 64 bytes FD) is emitted as a
lowercase hex string. No `value` is set. The `datatype` field is absent. The `addr` field
echoes the CAN ID: `{ "can_id": "0x1A0" }`.

### 4.4 Boolean signals

Signals with `bit_count = 1` and datatype `bool` are decoded as `Value::Bool(extracted != 0)`.

---

## 5. Subscribe flow (`subscribe`)

The connector implements `subscribe()` (not `read_points()`).

1. Group the requested points by DBC message (CAN ID).
2. Open a `CanSocket` on the device's `interface`.
3. Apply kernel-level `HwFilter`s for the set of subscribed CAN IDs to reduce CPU load.
4. Spawn a `tokio::task` per device that loops:
   a. `socket.recv_frame().await` — blocks until a frame arrives.
   b. Look up which points subscribe to this frame's CAN ID.
   c. For each matching point:
      - Call `extract_can_signal(frame.data(), start_bit, bit_count, byte_order)`.
      - In `typed` mode: interpret as datatype, apply `transform`, build `Sample { quality: Good, value, raw, addr: {"can_id": "0xNNN"}, ... }`.
      - In `raw` mode: emit full payload as hex, no `value`.
   d. Push each `Sample` to the `SampleSink`.
5. On `io::Error` from `recv_frame`: emit a `bad` sample with `error` for each subscribed point, update link status to `Degraded`, break.

The connector MUST NOT drop frames silently; bad-quality samples are emitted so flows/operators
can react.

---

## 6. Write flow (`execute`, verb `write`)

For `cmd/write/<id>` with `status: "init"`:

1. Resolve the target point by `request.point` within the device.
2. Reject (`failed`) if the point's `access` is `read`.
3. **Typed write:** decode `request.value` into the signal's bit representation using `encode_can_signal(value, start_bit, bit_count, byte_order)`.
4. **Raw write:** take `request.raw` (hex string, must be exactly 8 bytes for classic / ≤ 64 bytes FD), send the frame verbatim.
5. If the target message contains multiple signals (common in CAN), perform a **read-modify-write**:
   - Read the most-recently-received frame payload for this CAN ID from an in-memory cache (populated by the subscribe loop). If no cached frame exists, use a zero-filled payload.
   - Overwrite the signal's bit-field in the cached payload with the new value.
   - Send the modified frame.
6. Call `socket.write_frame()` with the CAN ID and resulting payload.
7. Return `successful` with the written `value`/`raw`, or `failed` with `reason`.

> **Atomicity note:** Read-modify-write is not atomic on SocketCAN. Callers SHOULD avoid
> writing to messages with multiple writable signals from concurrent command requests.

---

## 7. Status

- Publish `te/device/<device>/ot/canbus/status/link` retained:
  - `connected` once the SocketCAN socket opens successfully.
  - `disconnected` if `CanSocket::open()` fails.
  - `degraded` if `recv_frame()` returns an `io::Error` while subscribed.
- Publish service health on `te/device/main/service/tedge-dot/status/health`.

---

## 8. Byte-layout worked examples

### 8.1 Intel (little-endian) signal, `start_bit=0, bit_count=8`

Frame bytes: `[0xAB, 0xCD, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]`

Flatten to little-endian u64: bit 0..7 = `0xAB`, bit 8..15 = `0xCD`, ...

Shift right by `start_bit=0`, mask `0xFF` → `0xAB = 171`.

### 8.2 Intel signal, `start_bit=4, bit_count=8`

Frame bytes: `[0xAB, 0x0C, ...]`

Bits 0–7 = `0xAB`, bits 8–15 = `0x0C`.
As u16: `0x0CAB`. Right-shift by 4: `0x0CA`. Mask `0xFF`: `0xCA = 202`.

### 8.3 Motorola (big-endian) signal, `start_bit=7, bit_count=8`

`start_bit=7` is the MSBit (bit 7 of byte 0). An 8-bit Motorola signal starting at bit 7
occupies bits 7..0 of byte 0.

Frame byte 0: `0xAB` → extracted value = `0xAB = 171`.

### 8.4 Motorola signal spanning two bytes, `start_bit=15, bit_count=10`

`start_bit=15` = MSBit of byte 1.
The 10 bits run: bit 15 (MSB of byte 1), ..., bit 8 (LSB of byte 1), bit 7 (MSB of byte 0), bit 6.

Frame bytes: `[0xC0, 0x3A, ...]`
Byte 0 = `0xC0` (bits 7..0). Byte 1 = `0x3A` (bits 15..8).

Bits [15,14,13,12,11,10,9,8] from byte 1 = `0x3A` = `0011 1010`.
Bits [7,6] from byte 0 = `1100 0000` → upper two bits = `11`.

10-bit result: `0b00 1110 1011` (concatenated MSBit first) = `0x0EB = 235`.

Wait — let me redo carefully:

bit 15 (MSBit of result) = bit 15 of frame = bit 7 of byte 1 = `(0x3A >> 7) & 1` = `0`
bit 14 = bit 6 of byte 1 = `(0x3A >> 6) & 1` = `0`
...
bit 8  = bit 0 of byte 1 = `0x3A & 1` = `0`

Hmm, `0x3A = 0011 1010`, so byte 1 bits [7..0] = `0,0,1,1,1,0,1,0`.

bits [15..8] (8 bits) = `0x3A` = `0011 1010`
bits [7,6] (2 bits from byte 0) = `(0xC0 >> 6) & 3` = `3` = `11`

10-bit result MSBit-first: `00 1110 1011` = `0x0EB` = 235.

---

## 9. Acceptance test vectors

These vectors are **normative** for the CAN bus connector.

Format: `bytes` is the 8-byte classic CAN frame payload as lowercase hex (no spaces).
`start_bit`, `bit_count`, `byte_order` match the DBC signal definition.
`expect_raw_u64` is the unsigned bit-extracted value before type interpretation.

### 9.1 Intel unsigned, `start_bit=0, bit_count=8`

```json
{
  "id": "canbus-intel-u8-0x0-8",
  "frame_bytes": "ab00000000000000",
  "byte_order": "intel",
  "start_bit": 0,
  "bit_count": 8,
  "value_type": "unsigned",
  "expect": { "value": 171, "value_repr": "number" }
}
```

### 9.2 Intel unsigned, `start_bit=4, bit_count=8` (crosses byte boundary)

```json
{
  "id": "canbus-intel-u8-cross-byte",
  "frame_bytes": "ab0c000000000000",
  "byte_order": "intel",
  "start_bit": 4,
  "bit_count": 8,
  "value_type": "unsigned",
  "expect": { "value": 202, "value_repr": "number" }
}
```

- Explanation: bits 4..11 of the payload. Byte 0 bits [7:4] = `0x0A`, byte 1 bits [3:0] = `0x0C`. Assembled = `0xCA = 202`.

### 9.3 Intel signed negative, `start_bit=0, bit_count=8`

```json
{
  "id": "canbus-intel-i8-negative",
  "frame_bytes": "fe00000000000000",
  "byte_order": "intel",
  "start_bit": 0,
  "bit_count": 8,
  "value_type": "signed",
  "expect": { "value": -2, "value_repr": "number" }
}
```

- `0xFE` extracted unsigned = 254; sign-extended from 8 bits = -2.

### 9.4 Intel 16-bit, `start_bit=0, bit_count=16`

```json
{
  "id": "canbus-intel-u16",
  "frame_bytes": "d007000000000000",
  "byte_order": "intel",
  "start_bit": 0,
  "bit_count": 16,
  "value_type": "unsigned",
  "expect": { "value": 2000, "value_repr": "number" }
}
```

- Little-endian: byte 0 = `0xD0`, byte 1 = `0x07` → `0x07D0 = 2000`.

### 9.5 Intel boolean, `start_bit=8, bit_count=1`

```json
{
  "id": "canbus-intel-bool-true",
  "frame_bytes": "00010000000000000",
  "byte_order": "intel",
  "start_bit": 8,
  "bit_count": 1,
  "value_type": "unsigned",
  "datatype": "bool",
  "expect": { "value": true }
}
```

### 9.6 Motorola unsigned, `start_bit=7, bit_count=8` (single byte)

```json
{
  "id": "canbus-motorola-u8-byte0",
  "frame_bytes": "ab00000000000000",
  "byte_order": "motorola",
  "start_bit": 7,
  "bit_count": 8,
  "value_type": "unsigned",
  "expect": { "value": 171, "value_repr": "number" }
}
```

### 9.7 Motorola unsigned, `start_bit=15, bit_count=10` (spans two bytes)

```json
{
  "id": "canbus-motorola-u10-span",
  "frame_bytes": "c03a000000000000",
  "byte_order": "motorola",
  "start_bit": 15,
  "bit_count": 10,
  "value_type": "unsigned",
  "expect": { "value": 235, "value_repr": "number" }
}
```

- See §8.4 for worked derivation.

### 9.8 Intel float32

```json
{
  "id": "canbus-intel-f32",
  "frame_bytes": "0000284200000000",
  "byte_order": "intel",
  "start_bit": 0,
  "bit_count": 32,
  "value_type": "float32",
  "expect": { "value": 42.0, "value_repr": "number" }
}
```

- `0x42280000` in little-endian bytes = `00 00 28 42`. `f32::from_bits(0x42280000) = 42.0`.

### 9.9 Raw mode — full frame payload

```json
{
  "id": "canbus-raw-frame",
  "frame_bytes": "deadbeef01020304",
  "mode": "raw",
  "expect": { "raw": "deadbeef01020304", "value": null }
}
```

### 9.10 Bad quality — socket error

When `recv_frame()` returns an `io::Error`, the connector MUST emit:

```json
{
  "quality": "bad",
  "error": "<io error message>",
  "raw": ""
}
```

### 9.11 Typed write round-trip

- Point: `{ access: read_write, datatype: uint16, address: { message_name: "ENGINE_STATUS", signal_name: "RPM" } }`, Intel byte order, `start_bit=0, bit_count=16`.
- Command: `{ status: "init", point: "engine_rpm", value: 2500, value_repr: "number" }`
- Expected frame payload bytes: `c4090000...` (`0x09C4 = 2500` in little-endian).
- Expected result: `{ status: "successful", point: "engine_rpm", value: 2500 }`.

### 9.12 Access control — write to read-only point

- Point configured with `access = "read"`.
- Command: write request.
- Expected: connector returns `{ status: "failed", reason: "access denied: ..." }`.

---

## 10. Protocol config schema

A `canbus.schema.json` MUST validate the three protocol-specific objects:

- `protocol_address.interface` — non-empty string.
- `protocol_address.dbc_file` — non-empty string (absolute path recommended).
- `point.address.message_name` — non-empty string.
- `point.address.signal_name` — non-empty string.
- Writable `access` only permitted for signals resolvable to writable CAN frames.

---

## 11. Implementation checklist (for an agent)

1. Parse `protocol_address` (interface, dbc_file) and `point.address` (message_name, signal_name) in `configure()` (§3).
2. Load and parse the DBC file using `can-dbc`; resolve each signal to `(can_id, start_bit, bit_count, byte_order, value_type)`; reject unknown names with `ConfigError::Invalid`.
3. Implement `capabilities()` exactly as §2 (`subscribe: true`).
4. Implement `connect()`: `CanSocket::open(interface)`, apply `HwFilter`s for all subscribed CAN IDs, return `LinkReport { Connected }` or `{ Disconnected }`.
5. Implement `subscribe()`: spawn per-device task, `recv_frame` loop, `extract_can_signal`, `decode`, push to `SampleSink`; emit `bad` on socket error (§5).
6. Implement `read_points()` returning `ConnectorError::Unsupported` (CAN is push-only).
7. Implement `execute()` for verb `write`: read-modify-write cached frame, `encode_can_signal`, `socket.write_frame()` (§6).
8. Emit `bad` samples on socket errors; never drop events silently.
9. Gate CAN FD frame handling behind `#[cfg(feature = "canbus-fd")]`.
10. Pass every vector in §9 via unit tests.
