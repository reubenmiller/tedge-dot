# TODO

## In flight / next

* [ ] Contract 0.2 (`doc/rfc/0006-contract-0.2.md`, decisions recorded 2026-09-13) lands on the
      `develop` branch in the RFC's order: §3 manifest → §2 slim envelope → §4 writes in
      engineering units → §5 typed metadata + `range` + runtime `publish` → §1 `mode` →
      `datatype` → §7 thin-edge commands → §8 `manifest --format`. §6 was withdrawn.
* [ ] Port `impl/c/` to contract 0.2 in one pass once it is ratified and shipped in Rust
      (RFC 0006 §9, option A), then re-enable the `c`, `e2e-c` and `packaging-parity` CI jobs
      and resume the parity check, now `just c-manifest-parity`. Three things changed under
      the freeze and are deliberately NOT fixed in `impl/c/`, for that one pass to pick up:
      `packaging/nfpm.yaml` still installs `flows/ot-command-forward/` and
      `flows/ot-command-result/`, renamed to `ot-parameter-update/` and `ot-parameter-result/`
      by §7; `src/main.c` still has `describe` with `--set`/`--compact` instead of `manifest`
      with `--format`; and `ci/describe-parity.sh` keeps its name (the just recipe is already
      `c-manifest-parity`) and still compares `describe` output. The gated-off `c`, `e2e-c`
      and `packaging-parity` jobs mean nothing builds or runs any of it in the meantime.

* [ ] Ship profibus in the `tedge-dot-rs` package: the `profibus` cargo feature is excluded
      from the goreleaser builds because its serial dependency (`serialport` via `profirust`)
      has a native libudev build script that does not cross-compile with cargo-zigbuild.
      Options: disable the libudev feature upstream, vendor a libudev stub per target, or build
      the Linux packages natively per architecture. (`tedge-dot-c` already ships PROFIBUS, over
      `tcp://` only.)

* [ ] Close the remaining C parity gaps listed in `impl/c/README.md`. Each has a capability
      name already wired into the test tagging (`C_MISSING_CAPABILITIES` in the justfile), so
      implementing one means removing it from that list and adding the test that was waiting
      for it:
      - `opcua-security` — open62541 supports `Basic256Sha256` and friends; needs config +
        certificate plumbing, and a secured endpoint in the e2e stack to test against.
      - `canbus-fd` — classic frames only today; the Rust build has a `canbus-fd` feature.
      - `profibus-serial` — the C module speaks `tcp://` only (no serial PHY, no FDL token
        timing), so it cannot yet drive a multi-master RS-485 bus.
      Also: CAN bus push delivery (the C module renders the push-based bus as drain-into-cache
      polling — same samples, worse latency, so it is not tagged), and the 64-byte cap on
      string/raw values (`TDOT_RAW_MAX`).

* [ ] The `.apk` packages carry versions apk-tools rejects, for BOTH implementations and for
      real releases, not just snapshots: `apk version -c` reports `0.0.1-alpha.2` (this
      repository's existing tag format) and `0.0.0_pre.<sha>` (what nfpm derives from the
      snapshot version) as invalid, because apk's grammar allows `_pre1` but not `_pre.<hash>`
      and no bare `-alpha.2`. Valid forms are e.g. `0.0.1_alpha2` or `0.0.0~<sha>`. Fixing it
      means either an apk-specific version override in both packaging configs or a change to
      the tag convention. Nothing in CI installs an apk, which is why it has gone unnoticed —
      a `apk add --allow-untrusted` smoke on the built package would catch it.

* [ ] A simulator hook to delete an OPC UA subscription server-side while leaving the session
      up. It is the one push-failure path neither implementation can be tested against today
      (see the note in `impl/c/README.md`): open62541 reports the client as healthy throughout,
      so a regression there would be silent. `connectors/opcua/sim/` would need an endpoint or
      a method the suite can call.

* [ ] Fuzz the C parsers. The validation policy below requires a fuzz target for anything
      parsing external input; the Rust SDK has four (`just fuzz-all`), the C build has none,
      so its TOML loader (tomlc99) and DBC parser are only covered by the shared golden
      vectors and the e2e suites. libFuzzer via clang would reuse the same corpora.

* [ ] Cloud Fieldbus increments 3 + 4 (see `doc/rfc/0002-cloud-fieldbus-integration.md`;
      increments 1 + 2 shipped and verified live 2026-07-02): generalise the device-type
      translator per protocol, and the export path / UI-placeholder reconciliation (needs a
      tenant-side actor — a device cannot own or delete the UI-created managed object).
* [ ] Conformance suite implementation (`doc/conformance/conformance-suite.md` is spec'd,
      harness not built yet).
* [ ] Per-point `meta` support for the remaining flows: `ot-alarm` should read thresholds
      from `sample.meta`/measurement context so alarm limits can live next to the signal.
* [ ] File the upstream async-opcua issues (drafts ready in
      `doc/upstream/async-opcua-stranded-sample.md` and
      `doc/upstream/async-opcua-null-session-nonce.md`); drop `vendor/async-opcua-crypto`
      and the `[patch.crates-io]` entry once the nonce fix ships.
* [ ] c8y-fieldbus-import deferred items (script header TODOs): alarm/event/status mappings
      (gap G4), RTU serial-port resolution from `[connection.serial]`, signed and
      multi-register bit fields.
* [ ] Device parameters (RFC 0003, prototype implemented): persist the
      last-commanded value of write-only parameters across mapper restarts; derive
      `meta.parameter.set` from the Cloud Fieldbus device type name in `c8y-fieldbus-import`;
      optional Modbus FC16 fast path for `write-batch` on contiguous registers; optional
      `c8y_ParameterUpdate` audit event flow on top of the twin; gateway-level connector
      settings (poll_interval) as a tedge-parameter-plugin set script issuing `set-config`.
* [ ] Legacy write-payload compatibility (gap G2): accept explicit-address
      (`register`/`coil`/`address`/`ipAddress`) and name-based `metrics[]` payloads for
      `c8y_SetRegister`/`c8y_SetCoil`, not only `{point, value}`.

## Connector candidates

* ethercat — https://github.com/ethercrab-rs/ethercrab (MIT/Apache-2.0)
* EtherNet/IP — https://github.com/sergiogallegos/rust-ethernet-ip
* BACnet — spec sketch exists in `doc/connectors/_template-connector-spec.md`
* DNP3 — https://github.com/stepfunc/dnp3 — NOT possible (non-OSS license)

Score each implementation (functionality + maintainability incl. upstream library activity)
before promoting it past experimental.

## Validation policy

* Connectors must be validated with unit + integration tests and e2e simulator tests, and the
  tests must be proven by running them (see `doc/testing.md`).
* Shared SDK decode logic requires property-based tests; parsers of external input require a
  fuzz target.
