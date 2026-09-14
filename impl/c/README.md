# tedge-dot — C implementation

A C11 implementation of the tedge-dot SDK framework plus all five connectors —
**Modbus** (libmodbus), **OPC UA** (open62541), **CAN bus** (SocketCAN + a
minimal DBC parser), **CANopen** (expedited SDO client directly over SocketCAN),
and **PROFIBUS-DP** (a minimal built-in DP-V0 class-1 master, TCP transport).

It is a **maintained peer of the [Rust implementation](../rust/)**, not a
prototype: it ships as its own package (`tedge-dot-c`) from the same release,
runs the same system tests, and is held to the same contract. Pick it when the
device is small or old — it is roughly 25x smaller and installs on a glibc 2.17
floor — and pick the Rust build (`tedge-dot-rs`) otherwise.

Both packages install the same `/usr/bin/tedge-dot`, the same systemd unit and
the same `/etc/tedge/plugins/ot/` layout, and they conflict with each other:
a host runs one or the other, and a config, a flow or a cloud integration built
against one works unchanged against the other.

It speaks the same [OT Connector Contract](../../doc/contract/) as the Rust
implementation: same TOML config files (the untouched configs in
[demo/config/](../../demo/config/) work as-is), same MQTT topics, same JSON
sample/command envelopes (including the `access` and device `type` echoes and the
runtime-provided `write-batch` verb that the device-parameter flows rely on, see
[RFC 0003](../../doc/rfc/0003-parameter-writes.md) and
[RFC 0005](../../doc/rfc/0005-device-types-and-parameter-sets.md), whose parameter derivation,
set naming and `tedge-dot describe` rendering this build shares), the SDK management verbs
(`set-config`, `define-device`, `remove-device`: the runtime patches the
config document, validates it with the loader and the module, persists it and
live-reloads — comments are not preserved, tomlc99 being read-only), `raw`
mode and Modbus bit fields, push delivery via OPC UA monitored items, and the
same decode semantics (validated against the Rust SDK's golden vectors).

## Parity with the Rust implementation

This table is the **single source of truth for what differs**:

- everything NOT listed here runs the same suites against both builds — the
  e2e Robot suites, the cloud Cumulocity suites, the contract conformance
  suite, the shared golden decode vectors and `describe` parity;
- a capability listed here is declared in `C_MISSING_CAPABILITIES` in the
  [justfile](../../justfile), and a test covering it is tagged
  `requires:<capability>` and reported as SKIPPED for the build that lacks it
  (the convention is documented in
  [connectors/_shared/stack.resource](../../connectors/_shared/stack.resource)).

**A gap is only *enforced* once a test carries its tag.** Three of the entries
below are documented but not yet tested — nothing exercises OPC UA security,
CAN FD or a PROFIBUS serial PHY in *either* implementation, so skipping them for
the C build is currently a no-op. That is a real hole, and it is kept visible
rather than implied away: `just check-capability-tags` cross-checks the tags
against the declared lists, fails on a tag no list declares (a typo would
otherwise silently run the test everywhere), and prints the capabilities that
are still inert. CI runs it.

| Capability | Rust | C | Notes |
|---|---|---|---|
| Modbus, OPC UA, CAN bus, CANopen | ✅ | ✅ | |
| PROFIBUS-DP | ⚠️ source only | ✅ | The Rust package omits it: its serial dependency has a native libudev build script that does not cross-compile. The C package ships it. |
| Push delivery (`subscribe`) | ✅ | ✅ | OPC UA monitored items. See "Push delivery" below for the CAN bus difference. |
| Point libraries (`points_from`) | ✅ | ✅ | Same search path, protocol scoping and merge rules (contract §3.4). The mirrored loader tests are [`library.rs`](../rust/crates/sdk/src/library.rs) and [`tests/config.c`](tests/config.c); the Modbus e2e suite resolves half its points through a library in both builds. |
| `operation_timeout` | ✅ cancels the call | ✅ bounds the library | See "Liveness" below. |
| `stall_timeout` | ✅ restarts the connector | ⚠️ restarts the process | See "Liveness" below. |
| Reload on `SIGHUP` | ✅ | ✅ | Same rules: a new file starts a connector, a removed one stops it, a change is applied in place and an unusable file leaves the running configuration alone. A connector that cannot start or restart is tried again after `TEDGE_DOT_RESTART_DELAY` and on every reload, while the process keeps running. Two differences:<br>• Unchanged files: Rust compares the resolved configuration, C the documents. A file reordered or respelt to mean the same thing is left alone by Rust but reconnects its devices in C.<br>• With `--output stdout` (no MQTT session): Rust restarts a connector whose file changed, C applies the change in place.<br>[`reload_e2e.robot`](../../connectors/modbus/tests/reload_e2e.robot) runs against both. |
| `opcua-security` | ✅ `Basic256Sha256`, … | ❌ `None` only | open62541 supports the policies; wiring them up is config + certificate plumbing that has not been done. **No test yet** (needs a secured endpoint in the e2e stack). |
| `canbus-fd` | ✅ | ❌ | Classic CAN frames only. **No test yet.** |
| `profibus-serial` | ✅ serial + `tcp://` | ❌ `tcp://` only | No serial PHY and no FDL token timing — fine against a device server or the simulator, not yet for a multi-master RS-485 bus. **No test yet.** |
| CANopen segmented SDO | ❌ | ❌ | Neither implements it; expedited transfers (≤ 4 bytes) only. Not a parity gap. |
| `stale` quality | ❌ | ❌ | Neither emits it; the contract allows it, no last-good cache exists. Not a parity gap. |
| String / raw value length | unbounded | 64 bytes | Fixed buffers (`TDOT_RAW_MAX`). |

### Push delivery

OPC UA points are delivered by monitored items, exactly as in the Rust module:
one subscription per device publishing at the fastest requested point rate, one
monitored item per point, and the point's own `poll_interval` as the sampling
hint. Points opting out with `subscribe = false` stay on the polling schedule,
as does every point of a device whose subscription could not be established.

The C runtime is one thread per config with no async machinery, so where Rust
returns a stream the runtime selects on, the C vtable splits the job in two:
`subscribe_device()` arms the subscription and `drain_subscriptions()` is called
once per loop tick to hand over what arrived. The samples are published from the
runtime loop either way, so a module never touches MQTT.

Push delivery has a failure mode worth naming, because it is invisible by
construction: a subscribed point is off the polling schedule, so if push stops
the device simply goes quiet behind a retained `connected` link. open62541 does
not surface every way that can happen through `UA_Client_run_iterate`, which
keeps returning `GOOD` when a subscription is deleted server-side or dies with
the session. The connector therefore also watches the subscription's
delete/status-change/inactivity callbacks and the session state, and treats any
of them as a dead link so the ordinary reconnect-and-re-subscribe path runs.

The Rust module covers the same failure through the runtime's
`check_subscription` hook, asked on every tick for each device with pushed
points: async-opcua re-establishes a dropped session only a few times before
its event loop ends silently, and never notices a server that stops answering,
so the module records what the event loop reports (connection lost, gave up,
last publish response) and fails the check when the session is down, the
subscription or its monitored items went missing, or no publish response
arrived within the keep-alive window. The e2e suite's push-only device (`opc2`)
exercises this for both builds.

The polling path has a related trap. A synchronous read on a client that is not
fully connected reconnects inside the call and returns the failed attempt's
status, which a fixed list of "connection lost" codes cannot anticipate; the
point then looked merely bad, the link stayed `degraded`, and every point
repeated the blocking reconnect on every tick. `read_point()` therefore also
checks the session state after a failed read: a node-level Bad status arrives
over an activated session, a transport failure does not.

**None of those checks is currently proven by a test**, and it is worth being
precise about why. The OPC UA suite does cover push *recovery* end to end
(`Push Delivery Recovers After The Server Restarts` and `... From A Silent
Server`), but each of those trips an older path: a restart resets the TCP
connection, so `UA_Client_run_iterate` returns non-GOOD; a freeze trips the
client's request timeout on the polled points first. Verified by removing the
new checks — both tests still pass. Provoking the remaining path needs a server
that deletes a subscription while keeping the session up, which the suite's
simulator cannot do (tracked in TODO.md). Until then these checks rest on
open62541's source, not on evidence.

The **CAN bus** module is the one place the two still differ in delivery: the
Rust module pushes frames as they arrive, while this one renders the push-based
bus as drain-into-cache polling (`read_point` waits up to ~1.2 s for the first
broadcast of a frame). The observable samples are the same, which is why the
shared e2e suite passes for both and no capability tag is needed; the difference
is latency under a slow poll interval.

### Liveness

`operation_timeout` is the contract's bound on one protocol-module call
(§8.1). Rust wraps each call in a cancellable timeout. This runtime cannot
cancel a call in flight — a blocking libmodbus/open62541 call owns the thread —
so it pushes the value down into the protocol library's own response timeout
instead (`modbus_set_response_timeout`, open62541's client `timeout`). That is
what actually makes a silent peer return, and it is why the conformance
suite's silent-peer checks (B5) pass for both builds.

`stall_timeout` covers what a response timeout cannot: a call that wedges
*inside* a library. Every poll loop stamps a heartbeat; a watchdog thread
notices when one stops advancing and **exits the process** (exit code 70) so the
service manager restarts it — `packaging/tedge-dot.service` sets
`Restart=always`. Rust cancels and restarts just the wedged connector while its
siblings keep running; the C build takes every connector in the process down
with it, because a pthread blocked in a protocol library cannot be safely
cancelled. Both end with the broker publishing the retained last-will health
`down`. `stall_timeout = "0"` disables the watchdog; a value below twice
`operation_timeout` is raised, so one slow-but-legitimate call cannot cause a
restart loop.

## Tests

The C build is held to the same coverage as the Rust one:

- **unit tests** — `just c-test` (`ctest --test-dir impl/c/build`): the golden decode vectors
  shared with the Rust SDK; the device-parameter/`describe` checks, which assert the same facts
  over the same fixture config as `impl/rust/crates/sdk/src/descriptor.rs`; and the
  config-loader rules the runtime depends on (the liveness bounds and the per-point sampling
  hint, `tests/config.c`);
- **describe parity** — [`ci/describe-parity.sh`](ci/describe-parity.sh) (`just
  c-describe-parity`) renders the Cumulocity DTM definitions of every connector config in the
  repo with both binaries and compares them parsed, so the tenant-side declaration cannot drift
  between the implementations; CI runs it in the `c` job;
- **e2e Robot suites** — the per-protocol stacks under [connectors/](../../connectors/) run
  their Robot suite against the C connector with `just test-e2e-c <proto>` (the stack's
  `connector` service is swapped for [`connectors/_shared/Dockerfile.connector-c`](../../connectors/_shared/Dockerfile.connector-c));
  CI runs it for every protocol (`e2e-c` matrix);
- **cloud Robot suites** — the live Cumulocity harness under [cloud/](../../cloud/) builds this
  implementation into the thin-edge demo image with `just test-cloud-c <proto>` (`IMPL=c`
  selects a connector-install stage that compiles impl/c/ instead of installing the Rust
  package), so child registration, measurements, operations, the Cloud Fieldbus import and the
  device-parameter round-trip are all covered for the C build too;
- **smoke** — [`ci/smoke.sh <proto>`](ci/smoke.sh), a fast broker-and-simulator check
  without Docker for the connector itself (used by the `c` CI job).

- **conformance** — the contract conformance suite (layers 1-3, built-in broker and
  simulators) runs against the C binary as an external connector:
  `just conformance-c modbus|opcua` (manifests `connectors/<proto>/conformance-c.toml`,
  identical claims to the Rust ones); CI runs it in the `c` job. Both
  connectors are fully conformant, hot reload through the management verbs included.

Liveness is covered by `operation_timeout` and `stall_timeout` — see
[Liveness](#liveness) above for how they differ from the Rust runtime's.

Debugging: `TDOT_OPCUA_DEBUG=1` keeps open62541's client handshake log on stdout.

## Packaging & releases

Both implementations are released together by
[`.github/workflows/release.yaml`](../../.github/workflows/release.yaml): a
release tag (`v1.2.3` or the bare `0.0.1-alpha.3` form this repository has used
so far) builds `tedge-dot-rs` (goreleaser) and `tedge-dot-c` (this tree), and one
publish job assembles a single GitHub release, a single `SHA256SUMS` over every
asset and a single Cloudsmith push. A manual run builds both and can refresh the
rolling `snapshot` pre-release.

The C package ships `.deb` / `.rpm` / `.apk` + tarballs for `amd64`, `arm64` and
`armhf`. It carries the same `/usr/bin/tedge-dot`, the same `tedge-dot.service`
and the same `/etc/tedge/plugins/ot/` layout as `tedge-dot-rs`, which is why the
two *conflict* — install one or the other. Same device-side
[flows](../../flows/) too: the core pipeline is deployed active into
`/etc/tedge/mappers/c8y/flows/` and the opt-in alarm/event flows ship in
`/usr/share/tedge-dot/flows/`. It additionally ships the **PROFIBUS** connector,
which the Rust package omits.

Packaging is done with **nfpm** rather than goreleaser: goreleaser's headline
feature is cross-compiling Go/Rust with zig, and it has no notion of a C build
that also links *system* shared libraries. The build itself does use zig — see
below.

### Cross-compilation

Every architecture is cross-compiled on one machine by [cross/](cross/): a
Debian container where **zig supplies the compiler and libc** and **Debian
supplies the target-architecture shared libraries** (`libmodbus`,
`libmosquitto`, `libcjson`) via multiarch. open62541 is not packaged by Debian,
so it is still built from source — cross-compiled with the same toolchain and
statically linked.

The point is not only that one runner builds every architecture. zig lets us
**pin the minimum glibc** (2.17 — Debian 8 / RHEL 7 era), which a native build
fundamentally cannot: a binary built on `ubuntu-24.04` hard-requires glibc 2.39
on the target, which rules out most OT gateways in the field.

```sh
just c-cross-image            # build the toolchain image
just c-cross arm64            # -> impl/c/dist/arm64/tedge-dot
just c-verify arm64           # golden vectors on Debian bullseye (glibc 2.31)
just c-package arm64 0.1.0    # -> impl/c/dist/packages/*.{deb,rpm,apk}
just c-cross-all              # every architecture in C_ARCHS
```

`just c-verify` for a non-native architecture needs binfmt/qemu registered:
`docker run --privileged --rm tonistiigi/binfmt --install all`.

The packaged systemd unit runs `tedge-dot run /etc/tedge/plugins/ot` — a
**directory**, so one process runs every connector config it contains (one
worker thread per file), matching the Rust single-service model.

## Layout

| Path | Contents | Rust counterpart |
|---|---|---|
| `sdk/include/tedge_dot/` | public headers: model, config, connector vtable, decode, runtime | `impl/rust/crates/sdk` |
| `sdk/src/` | config loader (tomlc99), decode/encode, envelope builder (cJSON), poll-loop runtime (mosquitto), device parameters + Cumulocity DTM rendering | `impl/rust/crates/sdk` |
| `connectors/modbus/` | libmodbus connector (TCP + RTU, 4 tables, typed decode, writes) | `impl/rust/crates/connector-modbus` |
| `connectors/opcua/` | open62541 connector (client session, node-id points, typed reads/writes) | `impl/rust/crates/connector-opcua` |
| `connectors/canbus/` | SocketCAN connector + minimal DBC parser (BO_/SG_, Intel & Motorola layouts) | `impl/rust/crates/connector-canbus` |
| `connectors/canopen/` | expedited SDO client over raw SocketCAN (no CANopen library) | `impl/rust/crates/connector-canopen` |
| `connectors/profibus/` | minimal DP-V0 master (Diag→Prm→Cfg→Data_Exchange, bus thread, tcp:// transport) | `impl/rust/crates/connector-profibus` |
| `src/main.c` | `read` / `write` / `run` / `describe` CLI | `src/main.rs` |
| `tests/golden.c` | conformance runner for `impl/rust/crates/sdk/conformance/vectors.json` | `tests/golden_vectors.rs` |
| `tests/describe.c` | device-parameter derivation + DTM rendering checks | `impl/rust/crates/sdk/src/descriptor.rs` tests |
| `ci/describe-parity.sh` | `tedge-dot describe` output compared between the Rust and C binaries | — |
| `ci/smoke.sh` | e2e smoke: connector ⇄ simulator ⇄ broker, per protocol (used by the `c` CI job) | conformance/e2e suites |
| `cross/` | zig + Debian-multiarch cross-compilation image, build and verify scripts | goreleaser/zig |
| `third_party/tomlc99/` | vendored TOML parser (MIT) | serde/toml |

The Rust `Connector` trait maps to a C vtable (`tdot_connector_t` in
[connector.h](sdk/include/tedge_dot/connector.h)): `configure`,
`connect_device`, `read_point`, `write_point`, `disconnect_device`, plus the
optional `device_info` (the link status `info` descriptor, which the c8y
registration flow turns into the `c8y_ModbusDevice` twin fragment). Protocol
modules are selected by `tdot_connector_factory(protocol)` and compiled in
behind CMake options (`-DTDOT_MODBUS=ON/OFF`, `-DTDOT_OPCUA=ON/OFF`) —
the C analogue of the cargo feature flags.

## Build & run

Dependencies: cmake, pkg-config, libmodbus, mosquitto (client lib), cJSON.
open62541 is used from the system when installed, otherwise CMake builds
v1.5.5 from source (client-only feature set) via FetchContent. The CAN
connectors are Linux-only (SocketCAN) and compile-gated automatically.
On macOS: `brew install libmodbus open62541 mosquitto cjson`.

```sh
cmake -B build -G Ninja        # MinSizeRel by default
cmake --build build

# against the demo simulators (`just sim modbus`, `just sim opcua`):
./build/tedge-dot read  -c ../../demo/config/modbus.toml
./build/tedge-dot read  -c ../../demo/config/opcua.toml -d opc1 -p temperature --json
./build/tedge-dot write -c ../../demo/config/modbus.toml -d plc1 -p coil_rw --value true
./build/tedge-dot run   -c ../../demo/config/modbus.toml --output stdout --duration 10s
./build/tedge-dot run   -c ../../demo/config/opcua.toml   # publishes to MQTT broker

# the writable points as Cumulocity DTM property definitions, for a tenant admin to
# register once (needs no device, broker or protocol module):
./build/tedge-dot describe -c ../../demo/config/modbus.toml
./build/tedge-dot describe -c ../../demo/config/modbus.toml -d plc1 --compact
./build/tedge-dot describe -c ../../demo/config    # every config in the directory, sets merged

# tests
./build/tedge-dot-golden ../rust/crates/sdk/conformance/vectors.json
./build/tedge-dot-describe
```

## What was verified (2026-07-02/03, against the demo simulators)

- **Modbus**: all six demo points read correctly (uint16 17001, scaled 17.001 °C
  with `decimal_shift = -3`, uint32 617001, float32 404.17, coil), Modbus
  exception on `bad_point` → `quality: "bad"` with error reason, register +
  coil write round-trips.
- **OPC UA**: float64/uint32/int32/bool reads, `BadNodeIdUnknown` →
  `quality: "bad"`, int32 + bool write round-trips.
- **CAN bus** (Linux/vcan0): RPM 2500, coolant 85, brake true decoded from the
  sim's ENGINE_STATUS broadcasts via the DBC; a write sends the encoded
  read-modify-write frame on the bus. DBC bit-layout logic (Intel + Motorola,
  signed, encode round-trips) covered by a self-test against `sim/test.dbc`.
- **CANopen** (Linux/vcan0): SDO expedited uploads (uint16 1234, int16 -100,
  uint8 1) and a download round-trip (`digital_out` 1→0→1); SDO aborts map to
  `quality: "bad"` with the abort code.
- **PROFIBUS-DP** (TCP): full Diag→Set_Prm→Chk_Cfg→Data_Exchange bring-up
  against the DP-V0 slave sim; seeded inputs decode (0x0A, bit 3, 0x1234,
  100), output-byte write lands in the slave's PI_Q, and back-to-back
  sessions reconnect cleanly.
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
protocols; the C build dynamically links two protocol libraries), but the
totals on this machine:

| | size |
|---|---|
| Rust `tedge-dot` release binary (aarch64-linux, 5 protocols, static) | **12 MB** |
| **C binary, all 5 protocols, open62541 statically linked (x86_64 Linux, MinSizeRel)** | **457 KB** |
| C binary, modbus + opcua, shared open62541 (arm64 macOS) | 108 KB |
| C binary, modbus only | 91 KB |
| + libmodbus.dylib | 75 KB |
| + libmosquitto.dylib | 160 KB |
| + libcjson.dylib | 88 KB |

The headline number: **all five protocols in 457 KB** (open62541 built
client-only and statically linked; libmodbus/mosquitto/cJSON still dynamic,
~320 KB more if counted) — about **25× smaller** than the Rust binary. The
canbus/canopen/profibus connectors add ~40 KB total because their protocol
logic (DBC parsing, SDO framing, DP-V0 master) is implemented directly rather
than pulled in as libraries.

## Microcontroller path

What this implementation shows about the MCU question:

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
- **CANopen and PROFIBUS already have zero library dependencies** — the SDO
  client and the DP-V0 master are plain C over a socket/byte stream, so on an
  MCU only the transport (CAN driver / UART) needs swapping. The DBC parser
  is libc-only.
- **The runtime (poll loop, backoff, scheduler) is a single thread with no
  dynamic task machinery** — it maps naturally onto an RTOS task. The MQTT
  output would use an embedded client (e.g. coreMQTT) behind the same
  `publish()` seam in `runtime.c`.

## Known limitations

The differences from the Rust implementation are listed in the
[parity table](#parity-with-the-rust-implementation) above, which is the source
of truth. Limitations of this build that are NOT parity gaps:

- **CANopen is expedited-SDO only** (values ≤ 4 bytes; segmented transfers
  report a bad sample). The Rust module does not implement segmented transfers
  either.
- **Fixed-size buffers** cap strings and raw values at 64 bytes
  (`TDOT_RAW_MAX`), where the Rust model is unbounded.
- **`stale` quality** (last-good cache) is not implemented — nor is it in the
  Rust build; the contract allows it, neither emits it.

## Licensing

Everything linked here is copyleft-safe for the intended packaging:
libmodbus (LGPL-2.1+, dynamically linked), open62541 (MPL-2.0),
mosquitto client library (EPL/EDL dual), cJSON (MIT), tomlc99 (MIT, vendored).
