# Testing strategy

The framework aims for world-class coverage of the code every protocol shares, plus a
repeatable harness for each protocol's own transport. Testing is layered; each layer catches a
class of bug the others cannot.

| Layer | Where | Catches | Run with |
|---|---|---|---|
| Unit tests | `crates/*/src` (inline `#[cfg(test)]`) | Known-answer regressions, spec acceptance vectors | `just test` |
| Property-based tests | `crates/sdk/tests/properties.rs` | Invariant violations across the whole input space | `just test-properties` |
| Fuzzing | `crates/sdk/fuzz/` | Panics/crashes on hostile or malformed input | `just fuzz <target>` |
| Integration tests | `crates/connector-*/tests/` | Protocol framing against an in-process or scripted peer | `just test` |
| Simulator e2e | `connectors/<proto>/sim/`, `docker-compose.simulators.yaml` | Real protocol stacks end to end | `just test-e2e <proto>` |
| Flow tests | `flows/test-flows.sh` (`tedge flows test`) | Sample→measurement/alarm/event mapping, offline | `just test-flows` |
| Cloud e2e | `cloud/<proto>/tests/*.robot` | Cumulocity operation round-trips on a live tenant | Robot Framework |

## Property-based tests (proptest)

`crates/sdk/tests/properties.rs` pins the invariants of the shared decode/transform layer —
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

`crates/sdk/fuzz/` has four targets, runnable with `just fuzz <target> [seconds]` or all
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
`#[cfg(target_os = "linux")]`, so a macOS `cargo build` silently skips them — Linux-only
compile errors then surface only inside the Docker e2e build. Run `just check-linux` (cross
`cargo check`) after touching cfg-gated code; it caught the canopen Linux path failing to
compile while the host build was green.

## What a new connector must ship with

1. Unit tests for its address parsing and any protocol-specific decode beyond the SDK.
2. An integration test against an in-process or scripted peer where feasible.
3. A Docker simulator (`connectors/<proto>/sim/`) wired into `docker-compose.simulators.yaml`.
4. Acceptance vectors in its spec (`doc/connectors/<proto>-connector-spec.md`).
5. If it adds parsing of external input (files, frames), a fuzz target for that parser.
