# The Connector SDK and the `Connector` trait

| Field | Value |
| --- | --- |
| Status | Draft |
| Crate | `tedge-dot-sdk` |
| Binary | `tedge-dot` |
| Implements | [OT Connector Contract](../contract/ot-connector-contract.md) |

This document specifies the Rust SDK that every protocol module builds on. The goal is that a
new protocol is **only** a `Connector` trait implementation plus a config schema — the SDK
provides everything else (MQTT, scheduling, command routing, health, hot-reload,
serialization, and conformance hooks).

---

## 1. Crate layout

```text
tedge-dot/                 # cargo workspace
├── crates/
│   ├── sdk/                         # tedge-dot-sdk (the runtime + trait + types)
│   │   ├── src/
│   │   │   ├── lib.rs
│   │   │   ├── connector.rs         # the Connector trait
│   │   │   ├── model.rs             # Sample, Quality, DataType, Value, PointConfig, ...
│   │   │   ├── runtime.rs           # scheduler, MQTT, command router, health
│   │   │   ├── decode.rs            # shared primitive decode helpers (endianness, IEEE-754)
│   │   │   ├── config.rs            # contract-level config + schema validation
│   │   │   └── registry.rs          # protocol module registration
│   ├── connector-modbus/           # reference protocol module (feature = "modbus")
│   ├── connector-opcua/            # future module (feature = "opcua")
│   └── ...
├── src/main.rs                     # the binary: select module by config, run the runtime
└── Cargo.toml                      # feature flags: modbus, opcua, bacnet, canbus, ...
```

### 1.1 Feature flags

Each protocol module is a separate crate, enabled by a cargo feature on the binary:

```toml
# Cargo.toml (binary)
[features]
default = ["modbus"]
modbus  = ["dep:connector-modbus"]
opcua   = ["dep:connector-opcua"]
bacnet  = ["dep:connector-bacnet"]
canbus  = ["dep:connector-canbus"]
```

A build includes only the protocols it needs (`cargo build --features modbus,opcua`). The
binary selects the active module at runtime from `connector.protocol` in the config and
fails fast if that protocol was not compiled in.

---

## 2. The `Connector` trait

The trait is intentionally small. A polled protocol implements `read_points`; an
event-driven protocol additionally implements `subscribe`. Everything else is optional or
provided by the SDK.

```rust
use async_trait::async_trait;

/// A protocol module. One instance manages the connection(s) for one configured connector.
#[async_trait]
pub trait Connector: Send + Sync {
    /// Validate & parse the protocol-specific parts of the configuration
    /// (connection, device.protocol_address, point.address) into a typed model.
    /// Called once at startup and on every hot-reload.
    fn configure(&mut self, config: &ConnectorConfig) -> Result<(), ConfigError>;

    /// Declare what this connector supports. Drives the capability descriptor and
    /// the conformance suite. Must be cheap and pure.
    fn capabilities(&self) -> Capabilities;

    /// Establish protocol connections to all configured devices.
    /// Per-device success/failure is reported via the returned LinkReport(s).
    async fn connect(&mut self) -> Result<Vec<LinkReport>, ConnectorError>;

    /// Read a batch of points for one device. The SDK calls this on the point/device
    /// schedule. Implementations SHOULD batch contiguous addresses for efficiency.
    /// Returns one Sample per requested point (including `bad`/`stale` samples).
    async fn read_points(
        &mut self,
        device: &DeviceId,
        points: &[PointRef<'_>],
    ) -> Result<Vec<Sample>, ConnectorError>;

    /// OPTIONAL: for event-driven protocols (CAN, OPC-UA subscriptions, BACnet COV).
    /// The implementation pushes Samples into `sink` as values arrive, until cancelled.
    /// The default implementation returns `Unsupported`, which the SDK treats as
    /// "polling only".
    async fn subscribe(
        &mut self,
        device: &DeviceId,
        points: &[PointRef<'_>],
        sink: SampleSink,
    ) -> Result<(), ConnectorError> {
        let _ = (device, points, sink);
        Err(ConnectorError::Unsupported("subscribe"))
    }

    /// OPTIONAL: execute a command verb (default supports nothing).
    /// The SDK routes `cmd/<verb>` requests here after validating the topic/payload.
    /// Implementations MUST honour point `access` and encode `typed` writes per datatype.
    async fn execute(
        &mut self,
        device: &DeviceId,
        verb: &str,
        request: &CommandRequest,
    ) -> Result<CommandResult, ConnectorError> {
        let _ = (device, verb, request);
        Err(ConnectorError::Unsupported(verb.to_string().leak()))
    }

    /// Close connections cleanly. Called on shutdown and before reload.
    async fn disconnect(&mut self) -> Result<(), ConnectorError>;
}
```

### 2.1 Why both `read_points` and `subscribe`

Polled protocols (Modbus) only need `read_points`. Push protocols need `subscribe`. Having
both in the trait from the start means the contract and runtime never have to change when the
first event-driven protocol lands — it just implements the second method. The SDK decides
which to drive based on `capabilities().subscribe` and the point configuration.

---

## 3. Shared model types

These types are SDK-owned so every connector and the conformance suite agree on them. They
serialize to the [sample](../contract/schemas/sample.schema.json) and
[command](../contract/schemas/command.schema.json) schemas.

```rust
pub enum Mode { Raw, Typed }

pub enum DataType {
    Bool,
    Int8, Uint8, Int16, Uint16, Int32, Uint32, Int64, Uint64,
    Float32, Float64,
    StringT, Bytes,
}

pub enum Value {
    Bool(bool),
    Number(f64),
    /// Used for int64/uint64 outside JS safe-integer range, and for string/bytes.
    Text(String),
}

pub enum Quality { Good, Bad, Stale }

pub struct Sample {
    pub ts: OffsetDateTime,
    pub device: DeviceId,
    pub protocol: &'static str,
    pub point: PointId,
    pub mode: Mode,
    pub datatype: Option<DataType>,   // present when typed
    pub value: Option<Value>,         // absent for raw and for bad
    pub raw: Vec<u8>,                 // serialized to space-grouped hex
    pub quality: Quality,
    pub unit: Option<String>,
    pub addr: serde_json::Value,      // protocol-specific echo
    pub seq: Option<u64>,
    pub error: Option<String>,        // required when quality = Bad
}
```

### 3.1 Shared decode helpers (`decode.rs`)

`typed` mode must decode identically across connectors, so primitive decoding lives in the
SDK, not in each module:

```rust
pub fn decode_primitive(
    bytes: &[u8],
    datatype: DataType,
    endianness: Endianness,  // byte order within a word
    word_order: WordOrder,   // order of multi-word reads
) -> Result<Value, DecodeError>;

pub fn encode_primitive(
    value: &Value,
    datatype: DataType,
    endianness: Endianness,
    word_order: WordOrder,
) -> Result<Vec<u8>, DecodeError>;
```

A connector module typically: reads raw words from the wire, then for `typed` points calls
`decode_primitive`. This guarantees the [golden decode vectors](../conformance/conformance-suite.md)
behave the same everywhere. Connectors that need bit-field extraction call a companion
`extract_bitfield` helper and declare the `bitfield` feature.

> The SDK decode helpers are the **only** place IEEE-754 / endianness / word-order logic
> lives. Modules must not re-implement it. This is what makes the contract's promise — "the
> driver only decodes primitives" — uniform and testable.

---

## 4. What the runtime provides for free

The `runtime` module wraps a `Connector` and delivers all contract behaviour so modules stay
tiny:

| Responsibility | Detail |
| --- | --- |
| **MQTT** | Connect to the local broker (`rumqttc`), publish/subscribe, retained semantics, reconnection, QoS. |
| **Scheduling** | Drive `read_points` per point/device `poll_interval`; coalesce due points per device into batched calls. |
| **Subscriptions** | For modules advertising `subscribe`, manage the push lifecycle and forward Samples. |
| **Sample publishing** | Serialize `Sample` to the envelope and publish to `sample/<point>` (non-retained). |
| **Command routing** | Subscribe `cmd/<verb>/+`, validate against the command schema, run the state machine (`init→executing→successful|failed`), call `execute`, publish results (retained). |
| **Management verbs** | Implement `set-config`/`define-device`/`remove-device` (contract §6.3) generically: patch the config document, validate, persist, and live-reload — so no module writes config-mutation code. Augment `capabilities()` with these verbs + the `management` feature. |
| **Capability descriptor** | Build from `capabilities()` (plus the management verbs above) and publish retained on startup. |
| **Health & link status** | Publish retained service health; turn `LinkReport`s into retained `status/link` messages. |
| **Config & hot-reload** | Load + schema-validate config (contract schema + the module's own schema), watch files with `notify`, call `configure`/`connect` on change without a process restart. |
| **Backpressure / throttling** | Optional per-point minimum publish interval and `bad`-sample rate limiting. |
| **Observability** | Structured logging (`tracing`), and a `--once`/dry-run mode used by the conformance harness. |

A module author therefore writes: a config-parsing step, a capability declaration, connect,
read (and/or subscribe), execute, disconnect. Nothing about MQTT topics, retained flags,
JSON shaping, or the command state machine.

### 4.1 CLI: direct read/write (no broker)

The binary also exposes `read` and `write` subcommands that drive the **same** `Connector` code
path the runtime uses, but connect straight to a device and act, then exit — no broker or
running service required. This makes it easy to experiment with a config, verify wiring, or script
one-off writes:

```sh
tedge-dot read  -c modbus.toml                                     # every readable point of every device
tedge-dot read  -c modbus.toml -d plc-1 -p boiler_temp -p setpoint
tedge-dot read  -c modbus.toml -d 'plc-*' -p 'boiler_*' --poll     # keep polling at the configured interval
tedge-dot read  -c modbus.toml --interval 500ms --count 10         # override the interval, stop after 10 polls
tedge-dot write -c modbus.toml -d plc-1 -p setpoint --value 21.5   # typed; bool/number/string inferred
tedge-dot write -c modbus.toml -d plc-1 -p status   --raw 00ff     # raw bytes, verbatim
tedge-dot write -c modbus.toml -p 'setpoint_*' --value 0           # one value to every matching writable point
```

Device (`-d`) and point (`-p`) selectors accept `*`/`?` wildcards and default to every device /
every readable point; wildcard patterns skip points whose `access` does not fit the operation,
while explicitly named points are always attempted. `read`/`write` reuse `configure` →
`connect` → `read_points`/`execute(verb="write")` → `disconnect`, and `--json` prints the
contract sample/result envelopes.

The `run` subcommand accepts the same `-c/--config` flag (files or directories; the positional
form still works), `--duration 10s` to stop after a while, and `--output stdout` to print each
sample envelope as one JSON line instead of publishing to MQTT — handy for piping into `jq` or
capturing a trace; the envelope's `device` field identifies the source. The legacy invocation
`tedge-dot [<config>]` still runs the service (an implicit `run` subcommand), so existing
service units are unaffected. Because a direct CLI session opens its own transport, avoid using
it against a serial device while the service is running (the port cannot be shared); Modbus TCP
generally tolerates a second connection.

---

## 5. Module registration

Modules register themselves behind their feature flag so the binary can instantiate the one
named in config:

```rust
// crates/connector-modbus/src/lib.rs
pub fn factory() -> Box<dyn Connector> { Box::new(ModbusConnector::default()) }

// src/main.rs
fn build_connector(protocol: &str) -> Result<Box<dyn Connector>, FatalError> {
    match protocol {
        #[cfg(feature = "modbus")]
        "modbus" => Ok(connector_modbus::factory()),
        #[cfg(feature = "opcua")]
        "opcua"  => Ok(connector_opcua::factory()),
        other => Err(FatalError::ProtocolNotCompiledIn(other.to_string())),
    }
}
```

---

## 6. Minimal module skeleton

The full job of adding a protocol, reduced to its essentials:

```rust
#[derive(Default)]
pub struct MyConnector { /* parsed config, client handles */ }

#[async_trait]
impl Connector for MyConnector {
    fn configure(&mut self, cfg: &ConnectorConfig) -> Result<(), ConfigError> {
        // parse cfg.connection / device.protocol_address / point.address using serde,
        // validate against my-protocol.schema.json, store typed model.
        Ok(())
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            protocol: "myproto",
            version: env!("CARGO_PKG_VERSION"),
            modes: vec![Mode::Raw, Mode::Typed],
            datatypes: vec![DataType::Uint16, DataType::Float32],
            point_kinds: vec!["channel".into()],   // protocol-specific kind names
            command_verbs: vec!["write".into()],
            features: vec!["polling".into()],
            subscribe: false,
        }
    }

    async fn connect(&mut self) -> Result<Vec<LinkReport>, ConnectorError> { /* ... */ }

    async fn read_points(&mut self, device: &DeviceId, points: &[PointRef<'_>])
        -> Result<Vec<Sample>, ConnectorError>
    {
        // 1. read raw bytes from the wire (batched)
        // 2. for typed points call sdk::decode::decode_primitive(...)
        // 3. build Sample { quality, value, raw, addr, ... }
        Ok(samples)
    }

    async fn execute(&mut self, device: &DeviceId, verb: &str, req: &CommandRequest)
        -> Result<CommandResult, ConnectorError>
    {
        // verb == "write": check access, encode_primitive for typed, write, return result
    }

    async fn disconnect(&mut self) -> Result<(), ConnectorError> { Ok(()) }
}
```

The reference [Modbus connector spec](../connectors/modbus-connector-spec.md) fills this
skeleton in concretely and is detailed enough to be implemented by an AI agent.

---

## 7. Dependencies (suggested)

| Concern | Crate |
| --- | --- |
| Async runtime | `tokio` |
| MQTT | `rumqttc` |
| Modbus (reference) | `tokio-modbus` |
| Config / serde | `serde`, `toml`, `serde_json` |
| Schema validation | `jsonschema` |
| File watching | `notify` |
| Time | `time` (RFC 3339, UTC) |
| Logging | `tracing`, `tracing-subscriber` |
| Trait async | `async-trait` |

These are suggestions, not mandates; the contract and trait are what bind a module, not the
specific crates.
