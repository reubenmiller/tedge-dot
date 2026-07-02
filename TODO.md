# TODO

## In flight / next


* [ ] Cloud Fieldbus integration — see `doc/rfc/0002-cloud-fieldbus-integration.md`
      (device stays config-file driven; `ot-fieldbus-import` flow translates device types
      into `define-device` commands). Start with increment 1 (modbus).
* [ ] Wire `just fuzz-all` and `just test-properties` into CI (nightly job for fuzzing).
* [ ] Conformance suite implementation (`doc/conformance/conformance-suite.md` is spec'd,
      harness not built yet).
* [ ] Per-point `meta` support for the remaining flows: `ot-alarm` should read thresholds
      from `sample.meta`/measurement context so alarm limits can live next to the signal.
* [ ] File an upstream async-opcua issue: the 0.18 server can strand a monitored-item value
      written within one sampling interval of the previous source timestamp
      (`maybe_enqueue_skipped_value` vs the `notified_monitored_items` drain; see the notes in
      `crates/connector-opcua/tests/subscription_integration.rs`).
* [ ] Wire tenant-gated cloud e2e (`just test-cloud modbus`, incl. the Cloud Fieldbus
      round-trip suite) into CI as a manually-triggered/secret-gated workflow.

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
