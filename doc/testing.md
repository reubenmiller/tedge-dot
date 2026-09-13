# Testing strategy

The framework aims for world-class coverage of the code every protocol shares, plus a
repeatable harness for each protocol's own transport. Testing is layered; each layer catches a
class of bug the others cannot.

| Layer | Where | Catches | Run with |
|---|---|---|---|
| Unit tests | `impl/rust/crates/*/src` (inline `#[cfg(test)]`), `impl/c/tests/` | Known-answer regressions, spec acceptance vectors | `just test` / `just c-test` |
| Property-based tests | `impl/rust/crates/sdk/tests/properties.rs` | Invariant violations across the whole input space | `just test-properties` |
| Fuzzing | `impl/rust/crates/sdk/fuzz/` | Panics/crashes on hostile or malformed input | `just fuzz <target>` |
| Integration tests | `impl/rust/crates/connector-*/tests/` | Protocol framing against an in-process or scripted peer | `just test` |
| Simulator e2e | `connectors/<proto>/` (sim, compose, Robot suite) | Real protocol stacks end to end, both implementations | `just test-e2e <proto>` / `just test-e2e-c <proto>` |
| Flow tests | `flows/test-flows.sh` (`tedge flows test`) | Sample→measurement/alarm/event mapping, offline | `just test-flows` |
| Cloud e2e | `cloud/<proto>/tests/*.robot` | Cumulocity operation round-trips on a live tenant, both implementations | `just test-cloud <proto>` / `just test-cloud-c <proto>` |
| Conformance | `connectors/<proto>/conformance{,-c}.toml` | Contract compliance (schema, decode vectors, behaviour), both implementations | `just conformance <proto>` / `just conformance-c <proto>` |
| Manifest parity | `impl/c/ci/describe-parity.sh` | The two binaries publishing different manifests, or rendering them into different Cumulocity DTM definitions | `just c-manifest-parity` |

### Parity between the two implementations

`tedge-dot` ships as two implementations of one contract (see the root README). They are kept
honest by *sharing* test assets rather than by having parallel suites: the same Robot suites,
the same conformance manifests, the same golden decode vectors, and a `manifest` output
comparison. `IMPL=rust|c` selects which binary the stack is built with.

A capability one implementation genuinely cannot support is the only exception. The test that
covers it is tagged `requires:<capability>` and the runner turns the capabilities an
implementation lacks into `robot --skip requires:<capability>`, so it is reported as SKIPPED
rather than failing or being quietly dropped. The list lives in one place —
`C_MISSING_CAPABILITIES` in the justfile — and is mirrored by the parity table in
`impl/c/README.md`.

Two rules for such a test, documented next to the convention in
`connectors/_shared/stack.resource`: it must *fail* on an implementation lacking the capability
if the tag were removed (otherwise it is not testing the feature), and the capability must be a
real limitation, not a shortcut around a bug. The OPC UA push tests are the worked example —
proving push delivery needed a point on a *static* node, because a rate check cannot tell push
from polling when the runtime hands the connector the same interval for both.

### The suites own their stack and device

Both e2e layers are self-contained: nothing has to be started before a run, and nothing has to
be cleaned up afterwards. Each suite's setup hands its `docker-compose.yaml` to
**DeviceLibrary** (`connectors/_shared/stack.resource`, `cloud/_shared/device.resource`), which

- starts the stack as its own compose project named after a randomly generated device serial
  (isolated network and volumes, so suites never collide and can run in parallel),
- exposes the other services of the stack to the suite (`Execute Command … device_name=${SERIAL}:broker`),
- resolves the broker's *ephemeral* host port (`Get Service Port`) instead of a hardcoded one,
- for the cloud layer, bootstraps the generated device id against the tenant and deletes the
  device and its user again at teardown,
- stops and removes the project when the suite ends.

Put the setup keyword in `Suite Setup` for one stack per file (what the suites do today, and
what the ordered cloud suites need) or in `Test Setup` for a fresh stack per test case.
Consequence for compose files: **no fixed published host ports** (DeviceLibrary rejects them —
they break parallel runs); pin them through the documented env vars when poking at a stack by
hand (`just e2e-up`, `just sim`).

## Property-based tests (proptest)

`impl/rust/crates/sdk/tests/properties.rs` pins the invariants of the shared decode/transform layer —
the layer where a bug corrupts *every* protocol at once:

- encode → decode is the identity for all integer/float datatypes × endianness × word order;
- decode/encode/`parse_duration`/string decode are **total** (never panic, any input);
- 64-bit integers switch from `number` to `string` exactly at the JS safe-integer boundary;
- the linear transform is NaN-free for finite inputs and passes non-numerics through;
- `hex_grouped` raw serialization is lossless;
- `extract_bitfield` agrees with an independently written bit-by-bit reference model.

New shared decode logic must come with properties, not just examples. When a property fails,
proptest shrinks to a minimal counterexample — commit that counterexample as a plain unit test
alongside the fix.

## Fuzzing (cargo-fuzz / libFuzzer)

`impl/rust/crates/sdk/fuzz/` has four targets, runnable with `just fuzz <target> [seconds]` or all
briefly via `just fuzz-all` (requires the nightly toolchain and `cargo install cargo-fuzz`):

- `decode_primitive` — arbitrary wire bytes × datatype × byte orders; asserts integer
  round-trips re-encode to the identical buffer.
- `config_toml` — arbitrary text through the contract config parser and `parse_duration`.
  Configs arrive from hand-edited files *and* remote `set-config` commands, so hostile input
  is a normal operating condition. This target found a real crash on day one: negative/NaN
  durations panicked in `Duration::from_secs_f64` (fixed; regression covered by
  `invalid_durations_are_none_not_panics`).
- `transform` — the full f64 space (NaN, ±inf, subnormals) through `Transform::apply`.
- `sample_envelope` — arbitrary `Sample` contents must always serialize to valid JSON.

Fuzz findings graduate to unit tests: reproduce, fix, then encode the crashing input as a
permanent `#[test]` so the fuzz corpus is not the only memory of the bug.

## Platform-gated code

The SocketCAN connectors (`canbus`, `canopen`) hide their transport behind
`#[cfg(target_os = "linux")]`, so a macOS `cargo build --manifest-path impl/rust/Cargo.toml` silently skips them — Linux-only
compile errors then surface only inside the Docker e2e build. Run `just check-linux` (cross
`cargo check --manifest-path impl/rust/Cargo.toml`) after touching cfg-gated code; it caught the canopen Linux path failing to
compile while the host build was green.

## What a new connector must ship with

1. Unit tests for its address parsing and any protocol-specific decode beyond the SDK.
2. An integration test against an in-process or scripted peer where feasible.
3. A Docker simulator (`connectors/<proto>/sim/`) wired into `demo/docker-compose.yaml`.
4. Acceptance vectors in its spec (`doc/connectors/<proto>-connector-spec.md`).
5. If it adds parsing of external input (files, frames), a fuzz target for that parser.
