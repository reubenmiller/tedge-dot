# Modbus Connector — Reference Implementation Spec

| Field | Value |
| --- | --- |
| Status | Implementable draft |
| Protocol id | `modbus` |
| Crate | `connector-modbus` (feature `modbus`) |
| Builds on | [`tokio-modbus`](https://github.com/slowtec/tokio-modbus), [Connector SDK](../sdk/connector-sdk.md) |
| Implements | [OT Connector Contract](../contract/ot-connector-contract.md) v0.1.0 |

This is the **reference connector**. It is written to be precise enough that a competent
developer — or an AI coding agent — can implement it from this document plus the SDK spec,
and verify correctness against the [acceptance vectors](#9-acceptance-test-vectors) without
hardware. Wherever the contract leaves something protocol-specific, this document fills it
in.

---

## 1. Scope

The Modbus connector reads and writes the four standard Modbus data tables over TCP or RTU,
and publishes [sample envelopes](../contract/ot-connector-contract.md#5-the-sample-envelope).
Apart from the declared per-point linear transform (`point.transform`, contract §4.2, applied via
the SDK), all renaming, units, alarms, events, and thin-edge JSON shaping are **out of scope**
and handled by [flows](../flows/).

Supported Modbus operations (via `tokio-modbus`):

| Table | Read | Write | tokio-modbus call |
| --- | --- | --- | --- |
| Coil | yes | yes | `read_coils`, `write_single_coil` / `write_multiple_coils` |
| Discrete input | yes | no | `read_discrete_inputs` |
| Holding register | yes | yes | `read_holding_registers`, `write_single_register` / `write_multiple_registers` |
| Input register | yes | no | `read_input_registers` |

## 2. Capability descriptor

The connector MUST publish:

```json
{
  "protocol": "modbus",
  "version": "0.1.0",
  "modes": ["raw", "typed"],
  "datatypes": ["bool", "int16", "uint16", "int32", "uint32", "int64", "uint64", "float32", "float64"],
  "point_kinds": ["coil", "discrete_input", "holding_register", "input_register"],
  "command_verbs": ["write", "set-config", "define-device", "remove-device"],
  "features": ["polling", "bitfield", "management"],
  "subscribe": false
}
```

Modbus is strictly polled, so `subscribe` is `false` and the connector only implements
`read_points` (not `subscribe`). The module itself declares only `write`; the SDK runtime adds the
`set-config`/`define-device`/`remove-device` management verbs (and the `management` feature) it
implements on every connector's behalf (contract §6.3).

## 3. Protocol-specific configuration

These objects fill the contract's opaque slots. They MUST be schema-validated by
[modbus.schema.json](#10-protocol-config-schema).

### 3.1 `connection` (shared defaults)

```toml
[connection.serial]     # RTU defaults, used when a device omits them
baudrate = 9600
parity   = "N"          # "N" | "E" | "O"
stopbits = 2            # 1 | 2
databits = 8            # 7 | 8
```

### 3.2 `device.protocol_address`

TCP:

```toml
protocol_address = { transport = "tcp", host = "192.168.0.10", port = 502, unit_id = 1 }
```

RTU:

```toml
protocol_address = { transport = "rtu", serial_port = "/dev/ttyRS485", unit_id = 1,
                     baudrate = 19200, parity = "N", stopbits = 1, databits = 8 }
```

| Field | Required | Notes |
| --- | --- | --- |
| `transport` | yes | `"tcp"` or `"rtu"`. |
| `unit_id` | yes | Modbus slave/unit id (0–247). |
| `host`, `port` | tcp | `port` default `502`. |
| `serial_port` | rtu | e.g. `/dev/ttyRS485`. |
| `baudrate`/`parity`/`stopbits`/`databits` | rtu | Override `connection.serial`. |

### 3.3 `point.address`

```toml
address = { table = "holding", address = 7, count = 2, start_bit = 0, bit_count = 16 }
```

| Field | Required | Notes |
| --- | --- | --- |
| `table` | yes | `"coil"`, `"discrete_input"`, `"holding"`, `"input"`. |
| `address` | yes | 0-based register/coil address. |
| `count` | typed/raw | Number of registers (holding/input) or coils to read. For `typed`, MUST be large enough for the datatype (see §4.2). Defaults: 1 for coils/bool, derived from datatype otherwise. |
| `start_bit` | no | For `bitfield` extraction within registers (0-based). |
| `bit_count` | no | Width of the bit-field. Requires `bitfield` feature. |

## 4. Decoding rules (typed mode)

In `typed` mode the connector reads raw 16-bit registers (or coil bits), then calls the SDK
helper `decode_primitive(bytes, datatype, endianness, word_order)`. The connector itself does
**no** arithmetic beyond assembling bytes.

### 4.1 Byte and word order

- A Modbus register is 16 bits and transmitted big-endian on the wire. `tokio-modbus` returns
  each register as a `u16` already in host order.
- **`endianness`** (`big` default) controls byte order *within each 16-bit register* when the
  connector serializes registers to a byte buffer for decoding. `big` = `[hi, lo]`,
  `little` = `[lo, hi]`.
- **`word_order`** (`big` default) controls the order of multiple registers for 32/64-bit
  values. `big` = first register is most-significant word; `little` = first register is
  least-significant word.

### 4.2 Datatype → register count

| `datatype` | Registers (`count`) | Coils |
| --- | --- | --- |
| `bool` | — | 1 |
| `int16` / `uint16` | 1 | — |
| `int32` / `uint32` | 2 | — |
| `int64` / `uint64` | 4 | — |
| `float32` | 2 | — |
| `float64` | 4 | — |
| `string` | `ceil(len/2)` | — |
| `bytes` | `count` | — |

For `bool`, the connector reads from `coil` or `discrete_input` and maps `1→true`, `0→false`.

### 4.3 64-bit and string representation

- `int64`/`uint64` outside the JS safe-integer range MUST be emitted as a string `value` with
  `value_repr: "string"` (per contract §4.1).
- `string` is decoded using the connector's declared text encoding (default ASCII/UTF-8,
  null-trimmed); emitted as `value` with `value_repr: "string"`.
- `bytes` always emits no `value`; the data is the `raw` hex.

### 4.4 Bit-fields (optional `bitfield` feature)

When `start_bit`/`bit_count` are present, the connector extracts that bit-field from the
assembled buffer (after endianness/word-order) and decodes it as an unsigned integer of
`bit_count` width. This is the only arithmetic the connector performs and exists because
bit extraction in JavaScript is error-prone.

## 5. Read flow (`read_points`)

1. Group the due points for the device by `table` and into **contiguous address ranges** to
   minimize Modbus transactions (mirrors today's `_build_query_model` batching).
2. Issue the batched `tokio-modbus` reads for each range.
3. For each point:
   - `raw` mode → build `raw` hex from the bytes; `quality = good`; no `value`.
   - `typed` mode → assemble bytes per `endianness`/`word_order`, call `decode_primitive`,
     set `value` + `value_repr` + `datatype`; `quality = good`.
   - On a Modbus exception/timeout/CRC error for a range → emit `bad` samples for the
     affected points with `error` set and no `value`.
4. Echo the address into `addr`: `{ "table": ..., "address": ..., "unit_id": ... }`.
5. Return one `Sample` per requested point.

The connector MUST NOT drop failed reads silently; it emits `bad` samples so flows/operators
can react. Repeated identical `bad` samples MAY be rate-limited by the SDK.

## 6. Write flow (`execute`, verb `write`)

For `cmd/write/<id>` with `status: "init"`:

1. Resolve the target point by `request.point` within the device.
2. Reject (`failed`) if the point's `access` is `read`.
3. **Typed write:** encode `request.value` using `encode_primitive(value, datatype, endianness,
   word_order)`, then:
   - holding register, 1 register → `write_single_register`;
   - holding register, >1 register → `write_multiple_registers`;
   - coil → `write_single_coil` (value must be `bool`/0|1).
4. **Raw write:** take `request.raw` (hex), split into registers/coil bits, write verbatim.
5. **Bit-field write:** read-modify-write — read the current register(s), replace the
   `start_bit..start_bit+bit_count` field with the supplied value, write back. (Mirrors
   today's `compute_masked_value`.)
6. Return `successful` with the written `value`/`raw`, or `failed` with `reason`.

## 7. Status

- Publish `te/device/<device>/ot/modbus/status/link` retained: `connected` once a device
  responds, `disconnected` on transport loss, `degraded` if some reads fail persistently.
- Publish service health on `te/device/main/service/tedge-dot/status/health`.

## 8. Mapping from the legacy plugin

For reviewers familiar with the Python plugin, this is where each old responsibility goes:

| Legacy (Python) | New home |
| --- | --- |
| `reader.py` transport, batching | Modbus connector `read_points` |
| `mapper.py` datatype/endianness decode | SDK `decode_primitive` (driven by config) |
| `mapper.py` scaling `(raw*m*10^d/div)+offset` | **Connector** (per-point `transform`, contract §4.2) |
| `mapper.py` alarm/event state machine | **Flow** ([modbus-alarm](../flows/modbus-alarm/)) |
| `mapper.py` thin-edge JSON `templatestring` | **Flow** (ot-measurement `template`) |
| `mapper.py` combinemeasurements | **Flow** (aggregation/grouping) |
| `operations/set_register.py`, `set_coil.py` | `execute` verb `write` |
| `operations/c8y_*` cloud glue | **Flow** + thin-edge operations (see migration guide) |
| `devices.toml` / `modbus.toml` | New connector config (contract §3) |

## 9. Acceptance test vectors

These vectors are **normative** for the Modbus connector and feed directly into the
[conformance suite](../conformance/conformance-suite.md). Each gives the registers/coils as
returned by `tokio-modbus`, the point config, and the exact expected `Sample` fields. Hex is
lowercase, words space-separated per 16-bit register.

> All float/integer vectors below were computed with IEEE-754 / two's-complement and are
> reproducible with `struct` in Python or `f32::from_bits` in Rust.

### 9.1 `uint16`, big-endian

- Registers: `[0x1234]`
- Point: `{ mode: typed, datatype: uint16, endianness: big, address: { table: holding, address: 0, count: 1 } }`
- Expect: `value = 4660`, `value_repr = "number"`, `raw = "1234"`, `quality = "good"`.

### 9.2 `int16`, signed negative

- Registers: `[0xfffe]`
- Point: `{ mode: typed, datatype: int16, endianness: big }`
- Expect: `value = -2`, `raw = "fffe"`.

### 9.3 `uint16`, little byte order

- Wire bytes for the register assembled little-endian: register `0x1234` with
  `endianness: little` serializes to bytes `34 12`.
- Registers: `[0x1234]`, `endianness: little`
- Expect: `value = 4660` only if the source bytes were `34 12`; i.e. with `endianness:
  little` a register transmitted as bytes `34 12` decodes to `0x1234 = 4660`. `raw = "1234"`.

  > Rationale vector: bytes `[0x34,0x12]` decode as `little`→`0x1234=4660`, as `big`→`0x3412=13330`.

### 9.4 `uint32`, big word order

- Registers: `[0x0001, 0x0002]`
- Point: `{ datatype: uint32, endianness: big, word_order: big, address: { count: 2 } }`
- Expect: `value = 65538` (`0x00010002`), `raw = "0001 0002"`.

### 9.5 `uint32`, little word order

- Registers: `[0x0002, 0x0001]`
- Point: `{ datatype: uint32, word_order: little }`
- Expect: `value = 65538` (first register is least-significant word), `raw = "0002 0001"`.

### 9.6 `int32`, signed negative, big word order

- Registers: `[0xffff, 0xfffe]`
- Point: `{ datatype: int32, word_order: big }`
- Expect: `value = -2` (`0xfffffffe`), `raw = "ffff fffe"`.

### 9.7 `float32`, big-endian, big word order

- Registers: `[0x422a, 0x0000]`
- Point: `{ datatype: float32, endianness: big, word_order: big }`
- Expect: `value = 42.5`, `raw = "422a 0000"`.

### 9.8 `float32`, little word order

- Registers: `[0x0000, 0x422a]`
- Point: `{ datatype: float32, word_order: little }`
- Expect: `value = 42.5` (words swapped), `raw = "0000 422a"`.

### 9.9 `float64`, big-endian, big word order

- Registers: `[0x4009, 0x21f9, 0xf01b, 0x866e]`
- Point: `{ datatype: float64, address: { count: 4 } }`
- Expect: `value = 3.14159`, `raw = "4009 21f9 f01b 866e"`.

### 9.10 `bool` from a coil

- Coil read returns `true`.
- Point: `{ datatype: bool, address: { table: coil, address: 0, count: 1 } }`
- Expect: `value = true`, `value_repr = "boolean"`, `raw = "01"`, `quality = "good"`.

### 9.11 `raw` mode holding register

- Registers: `[0x1234]`
- Point: `{ mode: raw, address: { table: holding, address: 0, count: 1 } }`
- Expect: no `value`, `raw = "1234"`, `quality = "good"`.

### 9.12 Bad read

- Modbus exception (e.g. *gateway target device failed to respond*) for the range.
- Point: any typed point.
- Expect: `quality = "bad"`, `error` non-empty, no `value`.

### 9.13 Bit-field extraction (optional)

- Register: `[0b0000_0001_1110_0000]` = `0x01e0`
- Point: `{ datatype: uint16, address: { address: 0, count: 1, start_bit: 5, bit_count: 4 } }`
- Extract bits 5..9 → `0b1111 = 15`.
- Expect: `value = 15`, `raw = "01e0"`.

### 9.14 Typed write round-trip

- Point: `{ access: read_write, datatype: float32, word_order: big, address: { table: holding, address: 10, count: 2 } }`
- Command: `{ status: "init", point: "setpoint", value: 42.5, value_repr: "number" }`
- Expect the connector to write registers `[0x422a, 0x0000]` and return
  `{ status: "successful", point: "setpoint", value: 42.5 }`.

## 10. Protocol config schema

A `modbus.schema.json` MUST validate the protocol-specific objects (`connection`,
`device.protocol_address`, `point.address`). It is published alongside this spec and
referenced by the SDK's config validation step. Key constraints:

- `protocol_address.transport` ∈ `{tcp, rtu}` with conditional required fields per §3.2.
- `point.address.table` ∈ `{coil, discrete_input, holding, input}`.
- `address.address` ≥ 0; `count` ≥ 1; `start_bit`/`bit_count` only valid for register tables.
- Writable `access` only permitted for `coil` and `holding` tables.

> Authoring note: the JSON Schema file is intentionally left to implementation, but the
> constraints above are normative and tested by the conformance suite.

## 11. Implementation checklist (for an agent)

1. Parse and validate `connection` / `protocol_address` / `address` (§3, §10).
2. Implement `capabilities()` exactly as §2.
3. Implement `connect()` using `tokio-modbus` TCP/RTU; report `LinkReport` per device.
4. Implement `read_points()` with contiguous-range batching (§5) and SDK `decode_primitive`.
5. Implement `execute()` for verb `write`, typed/raw/bit-field (§6).
6. Emit `bad` samples on errors (§5.4, §9.12); never drop reads.
7. Pass every vector in §9 via the conformance harness.
