# Roadmap

A phased delivery plan. Each milestone is independently useful and independently shippable, so
value lands early and the contract is proven before the community is invited in.

## Milestone M1 — Contract + SDK + Modbus `typed` (foundations)

**Goal:** a Rust connector that polls Modbus and publishes typed samples, proving the contract.

- Ratify the [OT Connector Contract](contract/ot-connector-contract.md) v0.1.0 via
  [RFC 0001](rfc/0001-ot-connector-architecture.md).
- Build `tedge-dot-sdk`: model types, `decode_primitive`, runtime (MQTT via `rumqttc`,
  scheduler, sample publishing, config + schema validation, health/link, hot-reload).
- Implement `connector-modbus` read path (`tokio-modbus`, TCP+RTU, batching, `typed` decode).
- Land the SDK **golden decode vectors** (Layer 2 conformance) seeded from the
  [Modbus spec §9](connectors/modbus-connector-spec.md#9-acceptance-test-vectors).
- Package the single binary (nfpm + systemd), reusing the existing repo's packaging patterns.

**Exit criteria:** Modbus `typed` samples flow to MQTT; Layer 1 + Layer 2 conformance pass.

## Milestone M2 — `raw` mode, writes, and reference flows (feature-complete driver)

**Goal:** full driver/flow split usable end-to-end, replacing the Python plugin's capabilities.

- Add `raw` output mode to the SDK + Modbus connector.
- Implement the **command contract** (contract §6) and the Modbus `write` verb
  (typed/raw/bit-field, access control).
- Ship the reference flows: [modbus-scaling](flows/modbus-scaling/),
  [modbus-alarm](flows/modbus-alarm/), [device-registration](flows/device-registration/),
  plus an event flow.
- Behavioural conformance (Layer 3) against the
  [Modbus simulator](../../images/simulator/): checks B1–B10.
- Write the C8y operation shims that translate `c8y_SetRegister`/`c8y_SetCoil` into `cmd/write`.

**Exit criteria:** a device can be read, scaled, alarmed, registered, and written entirely via
connector + flows; full conformance suite green for Modbus.

## Milestone M3 — Migration, conformance harness, and registry (community-ready)

**Goal:** make it safe to switch, and easy for others to contribute.

- Ship `tedge-ot-migrate` per the [migration guide](migration/migration-guide.md).
- Release the standalone `ot-conformance` harness (static + behavioural) with JUnit/JSON output.
- Stand up the [connector registry](community/community-model.md) with the Modbus entry at the
  `verified` tier.
- Publish the `cargo-generate` connector template and the flow template.
- Write `CONTRIBUTING.md` and formalize the RFC process.

**Exit criteria:** an external contributor can scaffold, implement, and self-verify a connector
without core-team help; existing users have a documented, tested cut-over.

## Milestone M4 — Second protocol via the community (proof of generality)

**Goal:** validate that the contract generalizes beyond Modbus.

- Promote one capability sketch (recommended: **OPC-UA** or **CAN**) to a full spec using the
  [template](connectors/_template-connector-spec.md).
- Implement it as a `connector-<protocol>` crate — ideally as a community/AI-assisted PR — and
  exercise the **`subscribe`** (event-driven) path end-to-end.
- Add its simulator + conformance manifest; reach `verified` in the registry.
- Fold any lessons into a contract v0.2.0 RFC (additive where possible).

**Exit criteria:** two `verified` connectors of different natures (polled + push) share one
contract, SDK, conformance harness, and flow library — demonstrating the framework's purpose.

## Beyond M4 (opportunistic)

- More protocols: BACnet, additional fieldbuses.
- A dynamic-plugin loading path (deferred from v1) if demand justifies the ABI cost.
- A small library of reusable transformation flows (DBC scaling, unit conversion, aggregation)
  shared across protocols.
- Deeper thin-edge integration: connector + flow packages installable as one unit via software
  management.

## Sequencing rationale

- **Contract first.** Everything keys off the contract; ratifying it early prevents rework.
- **Typed before raw.** `typed` covers the common case and exercises the decode helpers and
  conformance vectors that everything else depends on.
- **Driver split proven before community.** The reference flows + conformance suite must exist
  before inviting external connectors, so quality is enforced by tooling, not review effort.
- **Generality proven last.** A second, event-driven protocol is the real test that the
  abstraction holds — done once the supporting machinery (template, harness, registry) is in
  place to absorb it cheaply.
