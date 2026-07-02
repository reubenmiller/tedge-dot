# tedge-dot

[![CI](https://github.com/thin-edge/tedge-dot/actions/workflows/ci.yaml/badge.svg)](https://github.com/thin-edge/tedge-dot/actions/workflows/ci.yaml)

OT protocol connectors for [thin-edge.io](https://thin-edge.io): one Rust binary
that moves data between industrial (OT) protocols and the thin-edge.io MQTT
broker.

> **Status: alpha.** The MQTT contract, config format and packaging may still
> change between releases.

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

All protocol modules are compiled into the single `tedge-dot` binary behind
cargo feature flags; each process runs one protocol (selected by
`connector.protocol` in its config file).

| Protocol | Crate | Transport | In released packages |
|---|---|---|---|
| Modbus (reference) | [connector-modbus](crates/connector-modbus/) | TCP + RTU | ✅ |
| OPC UA | [connector-opcua](crates/connector-opcua/) | opc.tcp | ✅ |
| CAN bus | [connector-canbus](crates/connector-canbus/) | Linux SocketCAN + DBC | ✅ |
| CANopen | [connector-canopen](crates/connector-canopen/) | Linux SocketCAN (SDO) | ✅ |
| PROFIBUS-DP | [connector-profibus](crates/connector-profibus/) | serial | ❌ build from source (`--features profibus`, Linux only) |

## Install

Grab a `.deb`/`.rpm`/`.apk` (or a plain binary archive) from the
[releases page](https://github.com/thin-edge/tedge-dot/releases). The package
installs:

- `tedge-dot` — the connector binary (also a standalone `read`/`write` CLI);
- one default config per protocol in `/etc/tedge/plugins/ot/` (no devices
  configured, so the service starts and idles until you add some);
- `tedge-dot.service` — a single systemd service: one `tedge-dot` process runs
  every configured connector, each in an in-process restart loop;
- demo configs in `/usr/share/tedge-dot/demo/`, pre-wired to the Docker
  simulators in [demo/](demo/) — see there for the all-protocols demo.

Add `[[device]]` sections to a config (each file documents the syntax), then:

```sh
sudo systemctl restart tedge-dot
tedge mqtt sub 'te/+/+/+/+/m/+'    # watch the measurements arrive
```

## Try it without hardware

Each protocol has a Docker simulator. No broker or cloud needed for a first
poke — the CLI talks to the device directly:

```sh
just sim modbus     # pymodbus simulator on 127.0.0.1:5020
cargo run -- read -c demo/config/modbus.toml                    # all devices, all readable points
cargo run -- read -c demo/config/modbus.toml -d plc1 -p 'temp_*' --poll   # keep polling (Ctrl-C stops)
cargo run -- run  -c demo/config/modbus.toml --output stdout --duration 10s  # sample JSON lines, no broker
```

See [demo/](demo/) for the local exploration guide and the full
all-protocols demo on a real device — both use the same configs in
[demo/config/](demo/config/).

## Repository layout

| Path | Contents |
|---|---|
| [crates/sdk](crates/sdk/) | `tedge-dot-sdk` — runtime, `Connector` trait, config model, decode helpers |
| [crates/connector-*](crates/) | one crate per protocol module |
| [crates/ot-conformance](crates/ot-conformance/) | `ot-conformance` — the connector conformance harness (schema, decode vectors, behavioural checks) |
| [src/](src/) | the `tedge-dot` binary (run service, `read`/`write` CLI) |
| [flows/](flows/) | protocol-neutral thin-edge.io flows (sample→measurement, alarms, registration, commands) |
| [operations/](operations/) | Cumulocity operation shims (legacy `c8y_*` operations → generic OT commands) |
| [connectors/](connectors/) | per-protocol e2e test stacks: simulator, Docker compose, Robot suites |
| [cloud/](cloud/) | Cumulocity cloud e2e suites (live tenant) |
| [packaging/](packaging/) | installed default configs, systemd unit, package scripts |
| [doc/](doc/) | proposal, RFCs, contract + schemas, connector specs, testing strategy |
| [demo/](demo/) | simulator compose file + demo configs: local CLI exploration and the all-protocols on-device demo |

## Development

Requires Rust (stable) and [just](https://github.com/casey/just);
Docker and Python for the e2e suites.

```sh
just test               # unit + integration + property tests
just lint               # clippy -D warnings
just conformance modbus # full conformance suite (no hardware/broker needed)
just test-flows         # offline flow tests (tedge flows test)
just test-e2e modbus    # Dockerised MQTT e2e suite for one protocol
just fuzz config_toml   # fuzz one SDK target (nightly + cargo-fuzz)
just build              # cross-compile + package everything (goreleaser)
```

The testing strategy — what each layer catches and what a new connector must
ship with — is documented in [doc/testing.md](doc/testing.md). Adding a new
protocol is documented in [connectors/README.md](connectors/README.md) and
[doc/connectors/_template-connector-spec.md](doc/connectors/_template-connector-spec.md).

## Releasing

Push a tag (e.g. `v0.1.0`) and the [release workflow](.github/workflows/release.yaml)
cross-compiles all targets with goreleaser, creates the GitHub release and
publishes the Linux packages. Run the workflow manually for a snapshot build
without releasing.

## License

[Apache-2.0](LICENSE)
