# tedge-dot

[![CI](https://github.com/thin-edge/tedge-dot/actions/workflows/ci.yaml/badge.svg)](https://github.com/thin-edge/tedge-dot/actions/workflows/ci.yaml)

OT protocol connectors for [thin-edge.io](https://thin-edge.io): one `tedge-dot`
binary that moves data between industrial (OT) protocols and the thin-edge.io
MQTT broker.

> **Status: alpha.** The MQTT contract, config format and packaging may still
> change between releases. The `develop` branch carries the contract 0.2 work of
> [RFC 0006](doc/rfc/0006-contract-0.2.md); during it the C implementation is frozen at 0.1
> ([why](impl/c/README.md)).

## Two implementations, one contract

`tedge-dot` exists twice, as two maintained implementations of the same
[OT Connector Contract](doc/contract/), released together as two interchangeable
packages that both install `/usr/bin/tedge-dot`:

| Package | Source | Pick it when |
|---|---|---|
| **`tedge-dot-rs`** | [impl/rust/](impl/rust/) | Default. Richest protocol support, one static binary, no shared-library dependencies. |
| **`tedge-dot-c`** | [impl/c/](impl/c/) | Small or old devices: ~25x smaller, a glibc 2.17 floor (Debian 8 / RHEL 7 era), and it ships the PROFIBUS-DP connector the Rust package omits. |

The two are mutually exclusive — a host installs one or the other — and a
config, a flow or a cloud integration built against one works unchanged against
the other. They run the **same** e2e, cloud and conformance suites, share the
same golden decode vectors, and are checked against each other for `describe`
output; the remaining behavioural differences are listed, and enforced by test
tags, in [impl/c/README.md](impl/c/README.md#parity-with-the-rust-implementation).

Everything outside `impl/` is shared by both: the contract and docs
([doc/](doc/)), the device-side [flows/](flows/), the test stacks and
conformance manifests ([connectors/](connectors/), [cloud/](cloud/)), the
[demo/](demo/) simulators and the [packaging/](packaging/) defaults.

## Design in one paragraph

The connector is a *dumb driver*: it reads/writes an OT protocol, decodes
primitives, applies per-signal scaling/units declared on each point, and
publishes generic `sample`/`cmd`/`status` envelopes on
`te/device/<device>/ot/<protocol>/...`. Everything else — naming, alarms,
child-device registration, cloud operation shaping — lives in protocol-neutral
[thin-edge.io flows](https://thin-edge.github.io/thin-edge.io/extend/flows/)
(small JavaScript modules, hot-reloaded, no recompilation). One neutral
*OT Connector Contract* makes every protocol connector look the same to the
rest of the system. See [doc/](doc/) for the full proposal, RFCs and
machine-readable schemas.

## Protocols

All protocol modules are compiled into the single `tedge-dot` binary (behind
cargo features in the Rust build, CMake options in the C one); each process runs
one protocol, selected by `connector.protocol` in its config file.

| Protocol | Transport | `tedge-dot-rs` | `tedge-dot-c` |
|---|---|---|---|
| Modbus (reference) | TCP + RTU | ✅ | ✅ |
| OPC UA | opc.tcp | ✅ | ✅ |
| CAN bus | Linux SocketCAN + DBC | ✅ | ✅ |
| CANopen | Linux SocketCAN (SDO) | ✅ | ✅ |
| PROFIBUS-DP | serial (Rust) / `tcp://` (C) | ❌ build from source (`--features profibus`, Linux only) | ✅ |

Rust modules: [connector-modbus](impl/rust/crates/connector-modbus/),
[connector-opcua](impl/rust/crates/connector-opcua/),
[connector-canbus](impl/rust/crates/connector-canbus/),
[connector-canopen](impl/rust/crates/connector-canopen/),
[connector-profibus](impl/rust/crates/connector-profibus/). C modules:
[impl/c/connectors/](impl/c/connectors/).

## Install

Grab a `.deb`/`.rpm`/`.apk` (or a plain binary archive) from the
[releases page](https://github.com/thin-edge/tedge-dot/releases). The package
installs:

- `tedge-dot` — the connector binary (also a standalone `read`/`write`/`describe` CLI);
- one default config per protocol in `/etc/tedge/plugins/ot/` (no devices
  configured, so the service starts and idles until you add some);
- `tedge-dot.service` — a single systemd service: one `tedge-dot` process runs
  every configured connector, each in an in-process restart loop;
- the [flows](flows/) that map the connector's generic OT envelopes onto the
  thin-edge data model. The core pipeline (measurement, registration, the two
  command flows, parameter state) lands in `/etc/tedge/mappers/c8y/flows/`
  ready to run — the mapper hot-reloads it, no restart. The opt-in alarm and
  event flows wait in `/usr/share/tedge-dot/flows/` until you copy one over and
  give it a `params.toml`;
- demo configs in `/usr/share/tedge-dot/demo/`, pre-wired to the Docker
  simulators in [demo/](demo/) — see there for the all-protocols demo.

Add `[[device]]` sections to a config (each file documents the syntax), then reload the
service — it applies edited, added and removed configs without restarting:

```sh
sudo systemctl reload tedge-dot    # SIGHUP; a restart works too
tedge mqtt sub 'te/+/+/+/+/m/+'    # watch the measurements arrive
```

A reload re-reads every config file and the point libraries they reference. A connector whose
configuration is unchanged keeps running untouched; one whose file changed applies it in place,
reconnecting its devices while its MQTT session and service health stay up; a file that cannot
be used is logged and its connector keeps the configuration it has. A new file starts a
connector and a removed file stops its connector. A change of `service_name`, `protocol`,
`[mqtt]` or the effective stall timeout (`stall_timeout`, raised to twice `operation_timeout`)
restarts that one connector, and `log_level` needs a service restart. A connector that cannot
start, or cannot restart, is tried again every `TEDGE_DOT_RESTART_DELAY` seconds (default 5) and
on every reload; the service keeps running meanwhile. The same goes for a broker that does not
accept the connection within 10 seconds, such as one still starting at boot: the connector
starts once it does.

## One point list, many devices

A device's point list belongs to its *type*, not to the instance: every ACME meter of the same
firmware has the same registers, and only the address differs. So a point list can live in its
own file — a **point library** — and a device reference it:

```toml
[[device]]
name             = "plc-7"
protocol_address = { transport = "tcp", host = "192.168.0.17", port = 502, unit_id = 1 }
points_from      = ["acme-meter-v2"]     # /usr/share/tedge-dot/points.d/modbus/acme-meter-v2.toml
```

Libraries are looked up as `<dir>/<protocol>/<name>.toml` under
`/etc/tedge/plugins/ot/points.d` (a site's own) and then `/usr/share/tedge-dot/points.d`
(packaged), so a site copy shadows a packaged list of the same name. A device may reference
several and add or adjust points of its own — the later definition of a point id patches the
earlier one — which is how a packaged list gets extended without editing the file a package
upgrade replaces.

Points can also carry a `name` and `description`, so a shared list documents itself once for
every instance that references it — they render into the Cumulocity parameter UI and into the
connector's retained capability descriptor, rather than being echoed on every sample.

A library can also name the **device type** it describes (`[library] type`), which every device
referencing it inherits (a `type` on the `[[device]]` wins). That is what keeps two device types
on the same protocol from colliding in the cloud: parameter set names are derived from it, and
it becomes the thin-edge entity type of the registered child device.

The packages ship a library per demo simulator (`demo-sim`, one for each protocol), so the
quickest way to get data out of a device is to give a `[[device]]` its address and
`points_from = ["demo-sim"]` — no point definitions to type. The demo configs in
[demo/config/](demo/config/) are written exactly that way.

Because only the *reference* is stored, `define-device` can add an instance at runtime from
its address alone, which is what lets your own discovery (mDNS, a subnet scan, an asset
inventory) onboard a known device type without shipping its point list. See
[RFC 0004](doc/rfc/0004-point-libraries.md), the normative
[contract §3.4](doc/contract/ot-connector-contract.md#34-point-libraries), and
the demo configs in [demo/config/](demo/config/) for runnable examples.

## Try it without hardware

Each protocol has a Docker simulator. No broker or cloud needed for a first
poke — the CLI talks to the device directly:

```sh
just sim modbus     # pymodbus simulator on 127.0.0.1:5020
export TEDGE_DOT_POINT_LIBRARY_PATH=demo/points.d   # where the demo point lists live in a checkout
cargo run --manifest-path impl/rust/Cargo.toml -- read -c demo/config/modbus.toml                    # all devices, all readable points
cargo run --manifest-path impl/rust/Cargo.toml -- read -c demo/config/modbus.toml -d plc1 -p 'temp_*' --poll   # keep polling (Ctrl-C stops)
cargo run --manifest-path impl/rust/Cargo.toml -- run  -c demo/config/modbus.toml --output stdout --duration 10s  # sample JSON lines, no broker
```

See [demo/](demo/) for the local exploration guide and the full
all-protocols demo on a real device — both use the same configs in
[demo/config/](demo/config/).

## Writing to devices

Writable points (`access = "read_write"` / `"write"`) are written through retained
thin-edge commands, never through a second protocol session:

```sh
# one point (the contract `write` verb)
tedge mqtt pub -r te/device/plc1/ot/modbus/cmd/write/w1 '{"status":"init","point":"temp_u16","value":4242}'
# several points, in order, one result (the SDK `write-batch` verb)
tedge mqtt pub -r te/device/plc1/ot/modbus/cmd/write-batch/b1 '{"status":"init","writes":[{"point":"temp_u16","value":4242},{"point":"coil_rw","value":true}]}'
```

From Cumulocity, writable points are **device parameters**: the `ot-parameter-state` flow
keeps one twin fragment per parameter set current, and the command flows turn a
`c8y_ParameterUpdate` operation from the device's *Parameters* tab (mapped by the
[tedge-parameter-plugin](https://github.com/thin-edge/tedge-parameter-plugin), which owns that
operation) into one `write-batch`.
A tenant admin declares the sets once with the definitions `tedge-dot describe` prints from
the same configuration — by default every connector config in `/etc/tedge/plugins/ot`, with a
set that several of them share rendered once. A set name is a tenant-wide identifier, so it is derived from the device's
**type** rather than from the protocol — `acme_meter_v2_control_parameters`, not
`modbus_parameters` — which is what lets several device types on one protocol coexist in a
tenant. See [RFC 0003](doc/rfc/0003-parameter-writes.md),
[RFC 0005](doc/rfc/0005-device-types-and-parameter-sets.md), [flows/](flows/) and
[operations/](operations/).

## Repository layout

| Path | Contents |
|---|---|
| [impl/rust/crates/sdk](impl/rust/crates/sdk/) | `tedge-dot-sdk` — runtime, `Connector` trait, config model, decode helpers |
| [impl/rust/crates/connector-*](impl/rust/crates/) | one crate per protocol module |
| [impl/rust/crates/ot-conformance](impl/rust/crates/ot-conformance/) | `ot-conformance` — the connector conformance harness (schema, decode vectors, behavioural checks) |
| [impl/rust/src/](impl/rust/src/) | the Rust `tedge-dot` binary (run service, `read`/`write`/`describe` CLI) |
| [impl/c/](impl/c/) | the C implementation: SDK, connectors, binary, cross-build and packaging |
| [flows/](flows/) | protocol-neutral thin-edge.io flows (sample→measurement, alarms, registration, commands) |
| [operations/](operations/) | Cumulocity operation shims (legacy `c8y_*` operations and `c8y_ParameterUpdate` → generic OT commands) |
| [connectors/](connectors/) | per-protocol e2e test stacks: simulator, Docker compose, Robot suites |
| [cloud/](cloud/) | Cumulocity cloud e2e suites (live tenant) |
| [packaging/](packaging/) | installed default configs, systemd unit, package scripts |
| [demo/points.d/](demo/points.d/) | example point libraries (a device type's point list, packaged on its own) |
| [doc/](doc/) | proposal, RFCs, contract + schemas, connector specs, testing strategy |
| [demo/](demo/) | simulator compose file + demo configs: local CLI exploration and the all-protocols on-device demo |

## Development

Requires Rust (stable) and [just](https://github.com/casey/just);
Docker and Python for the e2e suites.

```sh
just venv                 # one virtualenv for every system test (also used by the editor)
just test                 # Rust unit + integration + property tests
just lint                 # clippy -D warnings
just conformance modbus   # full conformance suite (no hardware/broker needed)
just test-flows           # offline flow tests (tedge flows test)
just test-e2e modbus      # Dockerised MQTT e2e suite for one protocol (the suite starts its own stack)
just test-cloud modbus    # live Cumulocity suite (needs C8Y_* credentials; device created per run)
just fuzz config_toml     # fuzz one SDK target (nightly + cargo-fuzz)
just build                # cross-compile + package the Rust build (goreleaser)
```

The same system suites run against the C implementation — that is how parity is
proven — and it has its own build and packaging recipes:

```sh
just c-test               # build impl/c/ and run its unit tests (shared golden vectors)
just c-describe-parity    # `tedge-dot describe` must agree between the two binaries
just conformance-c modbus # the contract conformance suite against the C build
just test-e2e-c modbus    # the SAME e2e suite, C connector
just test-cloud-c modbus  # the SAME cloud suite, C connector
just c-cross arm64        # cross-compile with zig against the glibc 2.17 floor
just c-package arm64 0.1.0 # deb/rpm/apk for one architecture (nfpm)
```

A test covering a capability one implementation lacks is tagged
`requires:<capability>` and reported as skipped for that implementation rather
than being duplicated or silently passing; see
[connectors/_shared/stack.resource](connectors/_shared/stack.resource) and
`C_MISSING_CAPABILITIES` in the [justfile](justfile).

The testing strategy — what each layer catches and what a new connector must
ship with — is documented in [doc/testing.md](doc/testing.md). Adding a new
protocol is documented in [connectors/README.md](connectors/README.md) and
[doc/connectors/_template-connector-spec.md](doc/connectors/_template-connector-spec.md).

## Releasing

Push a tag (e.g. `v0.1.0`) and the [release workflow](.github/workflows/release.yaml)
builds **both** implementations — `tedge-dot-rs` with goreleaser/cargo-zigbuild,
`tedge-dot-c` with the zig + Debian multiarch image in
[impl/c/cross/](impl/c/cross/) and nfpm — then assembles one GitHub release, one
`SHA256SUMS` over every asset, and one Cloudsmith push. Run the workflow
manually for a snapshot build; it can also refresh the rolling `snapshot`
pre-release.

## License

[Apache-2.0](LICENSE)
