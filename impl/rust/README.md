# tedge-dot — Rust implementation

The default implementation, packaged as **`tedge-dot-rs`**. A cargo workspace:
the SDK (runtime, `Connector` trait, config model, decode helpers), one crate
per protocol module, the conformance harness, and the `tedge-dot` binary.

| Path | Contents |
|---|---|
| [crates/sdk](crates/sdk/) | `tedge-dot-sdk` — runtime, `Connector` trait, config model, decode helpers, golden vectors |
| [crates/connector-*](crates/) | one crate per protocol module (modbus, opcua, canbus, canopen, profibus) |
| [crates/ot-conformance](crates/ot-conformance/) | the contract conformance harness (schema, decode vectors, behavioural checks) |
| [src/main.rs](src/main.rs) | the binary: `run` service plus the `read`/`write`/`manifest` CLI |
| [src/formats/](src/formats/) | renderings of the device manifest for something other than the contract (`--format c8y-dtm`) — here and not in the SDK, so the SDK stays cloud-agnostic |
| [vendor/](vendor/) | patched copies of upstream crates (see the `TEDGE-DOT-PATCH.md` in each) |

The [C implementation](../c/) is its maintained peer: same contract, same
system tests, its own package (`tedge-dot-c`). Differences between the two are
listed in
[impl/c/README.md](../c/README.md#parity-with-the-rust-implementation).

## Building and testing

Everything runs from the **repository root**, where the shared `connectors/`,
`cloud/`, `flows/`, `demo/` and `packaging/` trees live; cargo is pointed here
with `--manifest-path` rather than by changing directory, so the relative paths
the suites and conformance manifests use keep resolving.

```sh
just test                 # unit + integration + property tests
just lint                 # clippy -D warnings
just conformance modbus   # contract conformance suite (no hardware or broker)
just test-e2e modbus      # Dockerised MQTT e2e suite
just build                # cross-compile + package (goreleaser, this package only)

# or directly
cargo test --manifest-path impl/rust/Cargo.toml --workspace
export TEDGE_DOT_POINT_LIBRARY_PATH=demo/points.d   # the demo point lists (a package installs them)
cargo run  --manifest-path impl/rust/Cargo.toml -- read -c demo/config/modbus.toml
```

Protocol modules are cargo features on the binary (`modbus`, `opcua`, `canbus`,
`canbus-fd`, `canopen`, `profibus`); see [Cargo.toml](Cargo.toml). `profibus` is
excluded from the released package because its serial dependency has a native
libudev build script that does not cross-compile with cargo-zigbuild — build it
from source on Linux, or use `tedge-dot-c`, which ships it.

The SDK and the `Connector` trait are specified in
[doc/sdk/connector-sdk.md](../../doc/sdk/connector-sdk.md); the testing strategy
is in [doc/testing.md](../../doc/testing.md).
