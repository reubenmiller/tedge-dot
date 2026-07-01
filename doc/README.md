# Proposal: A thin-edge.io OT Connector Framework

> Status: **Draft for discussion** · Target: thin-edge.io community · Date: 2026-05-30

This directory contains a design proposal to evolve the thin-edge.io **modbus-plugin**
into a general-purpose, community-extensible **OT Connector framework** written in Rust.

The proposal is intentionally split into small, self-contained documents so that each
piece can be reviewed, ratified, and implemented independently — including by AI coding
agents working from the machine-readable specifications.

---

## TL;DR

- **Make the protocol driver dumb.** A connector should only do one thing well: move
  bytes between an OT protocol (Modbus, CAN, BACnet, OPC-UA, …) and MQTT. It performs
  *transport* and *optional decoding* — nothing else.
- **Move transformation into [thin-edge.io flows](https://thin-edge.github.io/thin-edge.io/extend/flows/).**
  Scaling, renaming, units, thin-edge JSON shaping, threshold alarms, aggregation, and
  registration are all expressed as small JavaScript flows that the community can read,
  test (`tedge flows test`), and share — no recompilation required.
- **Define one neutral contract for all OT protocols.** A single *OT Connector Contract*
  (config model + MQTT envelopes + command protocol + capability model) lets every
  protocol connector look the same to the rest of the system.
- **Rust + a shared SDK.** One binary, `tedge-dot`, with protocol modules behind
  cargo feature flags, all built on a shared `tedge-dot-sdk` runtime. Modbus is
  the reference implementation on [`tokio-modbus`](https://github.com/slowtec/tokio-modbus).
- **Built for a community and for AI agents.** Machine-readable schemas, a connector
  template, a conformance test suite, a connector registry, and an RFC process make it
  realistic for third parties — human or AI — to add a new protocol and have it "just work."

---

## Why change?

The current plugin ([`tedge_modbus`](../../tedge_modbus/)) is a capable Python application,
but it concentrates a large amount of responsibility inside one codebase:

| Concern | Where it lives today | Problem |
| --- | --- | --- |
| Modbus transport (TCP/RTU) | `reader/reader.py` | Tightly coupled to mapping logic |
| Bit extraction & datatypes | `reader/mapper.py` | Hard-coded; per-protocol reinvention |
| Endianness (byte + word order) | `reader/mapper.py` | Subtle, hard to test in isolation |
| Scaling `(raw*m*10^d/div)+offset` | `reader/mapper.py` | Requires a code change to adjust |
| Alarms / events / change detection | `reader/mapper.py` | Business logic baked into the driver |
| thin-edge JSON shaping | `reader/mapper.py` | Couples driver to cloud data model |
| Cumulocity operations | `operations/*.py` | C8y-specific glue inside the driver |

Adding a **second** OT protocol (CAN, BACnet, OPC-UA) means re-implementing most of this
stack again. There is no shared contract, no shared test harness, and no obvious place for
a contributor to plug in. The transformation logic — the part domain experts most want to
tweak — is the least accessible part of the system.

thin-edge.io already ships a purpose-built answer for the transformation half of this
problem: **flows**. Flows are sandboxed JavaScript steps that run inside a mapper, are
hot-reloaded without a restart, are testable offline, and are packaged and distributed
through normal software management. This proposal leans into flows for *everything that is
business logic* and reserves Rust for *everything that is protocol plumbing*.

---

## The shape of the solution

```text
        ┌──────────────────────────────────────────────────────────────────────┐
        │                         tedge-dot                            │
        │  (single Rust binary, protocol modules behind cargo feature flags)    │
        │                                                                        │
        │   ┌────────────┐     ┌──────────────────┐      ┌────────────────────┐  │
   OT ──┼──▶│ Transport  │────▶│ Decode (optional)│─────▶│ Publish: canonical │  │
 device │   │ (protocol) │     │ raw │ typed       │      │ "sample" envelope  │  │
        │   └────────────┘     └──────────────────┘      └─────────┬──────────┘  │
        │   ▲ write_point            capability model              │             │
        └───┼─────────────────────────────────────────────────────┼─────────────┘
            │                                                       │ MQTT
   command  │                                                       ▼
   results  │                                        ┌────────────────────────────┐
            │                                        │  thin-edge.io flows (JS)    │
            └────────────────────────────────────────│  scale · rename · units ·  │
                       cmd/write request             │  alarms · events · shape · │
                                                      │  aggregate · register      │
                                                      └─────────────┬──────────────┘
                                                                    ▼
                                                       te/device/<d>///m|e|a/...
                                                       (standard thin-edge data)
```

The connector publishes a small, **protocol-neutral "sample" envelope** for every point it
reads. Flows turn those raw/typed samples into the thin-edge.io data model the cloud
mappers already understand. Writes flow the other way: a `cmd/write` request on MQTT is
validated by the SDK and handed to the protocol module's `write_point`.

---

## Documents in this proposal

Read them roughly in this order:

| # | Document | What it covers |
| --- | --- | --- |
| 1 | [rfc/0001-ot-connector-architecture.md](rfc/0001-ot-connector-architecture.md) | The core architectural decision: dumb driver + flows, single pluggable binary, rationale, alternatives, risks. |
| 2 | [contract/ot-connector-contract.md](contract/ot-connector-contract.md) | **Normative.** The OT Connector Contract: config model, sample envelope, status/health, command protocol, capability model, topic conventions, quality & timestamp rules. |
| 3 | [contract/schemas/](contract/schemas/) | Machine-readable JSON Schemas for config, sample, command and status messages. |
| 4 | [contract/asyncapi.yaml](contract/asyncapi.yaml) | AsyncAPI 3.0 description of every MQTT topic and message. |
| 5 | [sdk/connector-sdk.md](sdk/connector-sdk.md) | The Rust SDK and the `Connector` trait every protocol module implements; what the runtime provides for free. |
| 6 | [connectors/modbus-connector-spec.md](connectors/modbus-connector-spec.md) | **AI-implementable** reference spec for the Modbus connector, including decode rules and acceptance test vectors. |
| 7 | [connectors/_template-connector-spec.md](connectors/_template-connector-spec.md) | A blank protocol spec template, plus capability sketches for CAN, BACnet and OPC-UA. |
| 8 | [flows/](flows/) | Example flow packages that move transformation out of the driver (scaling, alarms, registration). |
| 9 | [conformance/conformance-suite.md](conformance/conformance-suite.md) | The acceptance/conformance suite every connector must pass, and the simulator harness. |
| 10 | [community/community-model.md](community/community-model.md) | Connector registry, contribution guide, RFC process, project template, governance. |
| 11 | [migration/migration-guide.md](migration/migration-guide.md) | How today's `devices.toml` / `modbus.toml` / C8y operations map onto the new world. |
| 12 | [roadmap.md](roadmap.md) | A phased delivery plan from SDK to a community of connectors. |
| 13 | [testing.md](testing.md) | The layered testing strategy: unit, property-based (proptest), fuzzing (cargo-fuzz), integration, simulator e2e, flow and cloud tests. |
| 14 | [rfc/0002-cloud-fieldbus-integration.md](rfc/0002-cloud-fieldbus-integration.md) | Proposal: Cumulocity Cloud Fieldbus device types translated into `define-device` commands; the device stays config-file driven. |

---

## Design principles

1. **One job per layer.** The driver moves bytes; flows make meaning. Neither reaches into
   the other's domain.
2. **Protocol-neutral by default.** Everything above the transport layer speaks the same
   contract, so tooling, tests, and flows are written once and reused everywhere.
3. **Configurable, not hard-coded.** Each point can be emitted `raw` or `typed`; decoding is
   declared in config, never compiled in.
4. **Testable offline.** Connectors ship golden acceptance vectors; flows are validated with
   `tedge flows test`. A contributor can prove correctness without hardware.
5. **Boringly extensible.** Adding a protocol means implementing one trait and passing one
   conformance suite — a task small enough to specify for an AI agent.
6. **Community-owned.** A registry, a template, an RFC process, and a contribution guide make
   the framework a place people can build on, not just consume.

---

## Scope of this proposal

**In scope:** the architecture, the normative contract, machine-readable schemas, the SDK
and trait design, the Modbus reference spec with acceptance vectors, illustrative flow
packages, the conformance suite design, the community model, and a migration path.

**Out of scope (follow-up work):** the actual Rust crates, production flow packages,
packaging/CI, and any dynamic-plugin runtime. Those are described well enough to be
implemented — by humans or AI agents — but are deliberately not built here.
