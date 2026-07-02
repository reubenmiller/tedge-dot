# tedge-dot C proof of concept

A C11 reimplementation of the tedge-dot SDK framework plus the **Modbus**
(libmodbus) and **OPC UA** (open62541) connectors, exploring two questions:

1. how much smaller do the binaries get compared to the Rust implementation?
2. how feasible is a path to microcontrollers?

It speaks the same [OT Connector Contract](../doc/contract/) as the Rust
implementation: same TOML config files (the untouched configs in
[demo/config/](../demo/config/) work as-is), same MQTT topics, same JSON
sample/command envelopes, same decode semantics (validated against the Rust
SDK's golden vectors).

## Layout

| Path | Contents | Rust counterpart |
|---|---|---|
| `sdk/include/tedge_dot/` | public headers: model, config, connector vtable, decode, runtime | `crates/sdk` |
| `sdk/src/` | config loader (tomlc99), decode/encode, envelope builder (cJSON), poll-loop runtime (mosquitto) | `crates/sdk` |
| `connectors/modbus/` | libmodbus connector (TCP + RTU, 4 tables, typed decode, writes) | `crates/connector-modbus` |
| `connectors/opcua/` | open62541 connector (client session, node-id points, typed reads/writes) | `crates/connector-opcua` |
| `src/main.c` | `read` / `write` / `run` CLI | `src/main.rs` |
| `tests/golden.c` | conformance runner for `crates/sdk/conformance/vectors.json` | `tests/golden_vectors.rs` |
| `third_party/tomlc99/` | vendored TOML parser (MIT) | serde/toml |

The Rust `Connector` trait maps to a C vtable (`tdot_connector_t` in
[connector.h](sdk/include/tedge_dot/connector.h)): `configure`,
`connect_device`, `read_point`, `write_point`, `disconnect_device`. Protocol
modules are selected by `tdot_connector_factory(protocol)` and compiled in
behind CMake options (`-DTDOT_MODBUS=ON/OFF`, `-DTDOT_OPCUA=ON/OFF`) —
the C analogue of the cargo feature flags.

## Build & run

Dependencies: cmake, pkg-config, libmodbus, open62541, mosquitto (client lib),
cJSON. On macOS: `brew install libmodbus open62541 mosquitto cjson`.

```sh
cmake -B build -G Ninja        # MinSizeRel by default
cmake --build build

# against the demo simulators (`just sim modbus`, `just sim opcua`):
./build/tedge-dot read  -c ../demo/config/modbus.toml
./build/tedge-dot read  -c ../demo/config/opcua.toml -d opc1 -p temperature --json
./build/tedge-dot write -c ../demo/config/modbus.toml -d plc1 -p coil_rw --value true
./build/tedge-dot run   -c ../demo/config/modbus.toml --output stdout --duration 10s
./build/tedge-dot run   -c ../demo/config/opcua.toml   # publishes to MQTT broker

# conformance
./build/tedge-dot-golden ../crates/sdk/conformance/vectors.json
```

## What was verified (2026-07-02, against the demo simulators)

- **Modbus**: all six demo points read correctly (uint16 17001, scaled 17.001 °C
  with `decimal_shift = -3`, uint32 617001, float32 404.17, coil), Modbus
  exception on `bad_point` → `quality: "bad"` with error reason, register +
  coil write round-trips.
- **OPC UA**: float64/uint32/int32/bool reads, `BadNodeIdUnknown` →
  `quality: "bad"`, int32 + bool write round-trips.
- **MQTT contract**: retained health (`up`/`down` + last-will), retained
  capability descriptor, retained per-device link status, non-retained samples
  on `te/device/<dev>/ot/<protocol>/sample/<point>`, and inbound
  `cmd/write/<id>` commands answered with retained
  `{"status":"successful"|"failed"}` results.
- **Recovery**: killing the simulator flips the link to `disconnected` and
  starts the 1s→60s exponential reconnect backoff.
- **Decode conformance**: all 73 golden vectors pass (every datatype,
  endianness/word-order combination, NaN/±Inf, out-of-JS-safe-range 64-bit →
  string, bitfields, round-trips).

## Size comparison

Apples-to-apples is tricky (the Rust binary is one static binary with five
protocols; the C PoC dynamically links two protocol libraries), but the
totals on this machine:

| | size |
|---|---|
| Rust `tedge-dot` release binary (aarch64-linux, 5 protocols, static) | **12 MB** |
| C PoC binary, modbus + opcua (arm64 macOS, MinSizeRel) | **108 KB** |
| C PoC binary, modbus only | 91 KB |
| + libmodbus.dylib | 75 KB |
| + libopen62541.dylib | 1.7 MB |
| + libmosquitto.dylib | 160 KB |
| + libcjson.dylib | 88 KB |
| **C total (binary + all four libs)** | **≈ 2.1 MB** |

So roughly **6× smaller** all-in, and the app code itself is ~100 KB. On a
distro where libmodbus/mosquitto are already installed (most gateways running
thin-edge.io have libmosquitto), the marginal install is the binary plus
open62541. open62541 can also be compiled with reduced feature sets
(`UA_ENABLE_*=OFF`, amalgamated single-file build) down to a few hundred KB.

## Microcontroller path

What this PoC shows about the MCU question:

- **The contract layer is MCU-ready.** The decode/encode/scaling core and the
  envelope model have no OS dependencies beyond libc (the golden-vector suite
  would run on bare metal). tomlc99 and cJSON are portable C99 with small
  footprints.
- **open62541 explicitly supports MCUs** (it has ports for FreeRTOS+lwIP,
  Zephyr) — the OPC UA connector logic here uses only the high-level client
  API that exists on those ports.
- **The Modbus connector would swap libmodbus for a bare-metal Modbus layer**
  on MCUs (libmodbus assumes POSIX sockets/termios). Modbus framing is simple
  enough that this is a small, well-bounded port.
- **The runtime (poll loop, backoff, scheduler) is a single thread with no
  dynamic task machinery** — it maps naturally onto an RTOS task. The MQTT
  output would use an embedded client (e.g. coreMQTT) behind the same
  `publish()` seam in `runtime.c`.

## PoC scope cuts (vs. the Rust implementation)

- **Typed mode only** — `mode = "raw"` points are not implemented (raw hex is
  still echoed in every envelope).
- **Polling only** — no OPC UA monitored-item push subscriptions
  (`subscribe = false` fallback path is what runs).
- **No OPC UA security** — `security_policy = "None"` only (open62541
  supports Basic256Sha256 etc.; wiring it up is config + cert plumbing).
- **Bitfield extraction** exists in the SDK (`tdot_bitfield_extract`, golden
  vectors pass) but is not wired into the Modbus point config.
- **One config file per process** (the Rust binary supervises all configs in
  `/etc/tedge/plugins/ot/` from one process).
- `stale` quality (last-good cache) and the `set-config` / `define-device`
  management verbs are not implemented.
- Fixed-size buffers cap strings/raw values at 64 bytes.

## Licensing

Everything linked here is copyleft-safe for the intended packaging:
libmodbus (LGPL-2.1+, dynamically linked), open62541 (MPL-2.0),
mosquitto client library (EPL/EDL dual), cJSON (MIT), tomlc99 (MIT, vendored).
