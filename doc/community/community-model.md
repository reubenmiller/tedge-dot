# Community model

The strategic goal of this proposal is not just a Rust Modbus driver — it is a **community**
that adds and maintains OT protocol connectors for thin-edge.io. This document describes the
pieces that make that realistic: how connectors are discovered, contributed, reviewed,
templated, and governed, and how the contract itself evolves.

## 1. What a contributor actually has to do

Because of the [contract](../contract/ot-connector-contract.md), [SDK](../sdk/connector-sdk.md),
and [conformance suite](../conformance/conformance-suite.md), adding a protocol is a bounded,
well-specified task:

1. Write a connector spec from the [template](../connectors/_template-connector-spec.md)
   (config shapes, decode rules, acceptance vectors).
2. Implement the `Connector` trait in a `connector-<protocol>` crate.
3. Provide a `conformance.toml` and a protocol simulator seed.
4. Pass the conformance suite.
5. Submit to the registry.

Steps 1–4 are precise enough to hand to an AI coding agent and verify automatically. That is
the whole design intent: **specification + conformance = AI-implementable + community-scalable.**

## 2. Connector registry

A lightweight, declarative registry makes connectors discoverable and signals quality.

### 2.1 Registry entry format

The registry is a directory of TOML entries (e.g. in a `tedge-ot-connectors` repo):

```toml
# registry/modbus.toml
[connector]
protocol    = "modbus"
name        = "Modbus TCP/RTU"
description = "Read/write Modbus coils, discrete inputs, holding and input registers."
repository  = "https://github.com/thin-edge/tedge-dot"
crate       = "connector-modbus"
maintainers = ["@thin-edge"]
license     = "Apache-2.0"

[conformance]
contract_version = "0.1.0"
tier             = "verified"        # "verified" | "community" | "experimental"
last_verified    = "2026-05-30"
report           = "https://.../conformance/modbus-0.1.0.json"

[capabilities]
modes      = ["raw", "typed"]
datatypes  = ["bool", "int16", "uint16", "int32", "uint32", "float32", "float64"]
verbs      = ["write"]
subscribe  = false
```

### 2.2 Quality tiers

| Tier | Meaning |
| --- | --- |
| `verified` | Passes the full conformance suite in CI; maintained; recommended. |
| `community` | Passes static (Layer 1–2) conformance; behavioural tests may be partial. |
| `experimental` | Early/incomplete; use at your own risk. |

Tiers are earned by **evidence** (a conformance report), not by reputation — which keeps the
bar objective and lets AI-authored connectors qualify on equal footing.

## 3. Project template

A `cargo-generate` template (`tedge-dot-template`) scaffolds a new connector crate so
contributors start from a working, conformance-wired skeleton:

```sh
cargo generate thin-edge/tedge-dot-template --name connector-acme
```

The template produces:

```text
connector-acme/
├── Cargo.toml                 # depends on tedge-dot-sdk
├── src/lib.rs                 # Connector trait skeleton (configure/capabilities/connect/read/execute)
├── schema/acme.schema.json    # protocol config schema stub
├── spec/acme-connector-spec.md# filled-in copy of the spec template
├── conformance.toml           # manifest stub
└── tests/conformance.rs       # wires the SDK conformance harness
```

A companion **flow template** (`tedge generate flow`, or a `cargo-generate` flow template)
scaffolds a transformation flow package (`flow.toml`, `main.js`, `params.toml.template`,
`TEST.md`) so the transformation half is equally easy to contribute.

## 4. Contribution guide (outline)

The repo's `CONTRIBUTING.md` should cover:

- **Scope rule:** drivers stay dumb; transformation goes in flows. The only numeric transform a
  connector applies is the declared per-point `transform` (contract §4.2, math owned by the SDK).
  PRs that put naming, alarms, or ad-hoc scaling logic in a connector are sent back.
- **Spec-first:** a connector PR opens with (or links) its spec and acceptance vectors.
- **Conformance-gated:** CI runs `ot-conformance`; no merge without a passing report.
- **Schema discipline:** any change to a contract schema requires an RFC (see §5).
- **Docs:** every connector ships a spec; every flow ships a `TEST.md`.
- **AI-assisted PRs welcome:** clearly labelled, held to the identical conformance bar.

## 5. RFC process for the contract

The contract is shared infrastructure, so it changes deliberately:

```text
 idea ─▶ RFC PR (rfc/NNNN-title.md) ─▶ discussion ─▶ accepted/rejected ─▶ implemented ─▶ shipped
```

- RFCs live in [rfc/](../rfc/) using the format of
  [0001-ot-connector-architecture.md](../rfc/0001-ot-connector-architecture.md).
- **Breaking** changes to topics, required envelope fields, the command state machine, or the
  datatype set require an RFC and a contract major-version bump.
- **Additive** changes (new optional fields, new capability tags, new verbs) can be minor and
  fast-tracked, but still land as a short RFC for the record.
- Each connector declares the `contract_version` it targets; the registry surfaces mismatches.

## 6. Versioning and compatibility

- The contract is semver-versioned independently of any connector.
- Connectors are versioned independently and declare their target contract version.
- The SDK exposes the contract version it implements; a connector built against an older
  contract still works as long as the major versions match.
- Flows are decoupled: because they consume the stable envelope, they survive connector
  upgrades.

## 7. Governance

- A small **maintainer group** owns the contract, SDK, and conformance harness.
- Connector crates may have **independent maintainers** listed in their registry entry;
  ownership is per-connector, lowering the bar to contribute without diluting core quality.
- Decisions on the contract go through the RFC process; everything else is normal PR review.
- A connector that goes unmaintained and falls out of conformance is demoted from `verified`
  to `community`/`experimental` rather than silently breaking users.

## 8. Why this fosters real community

- **Low, objective bar to entry:** a template + a spec + a conformance report. No tribal
  knowledge required.
- **Separation of concerns:** protocol experts write drivers; domain experts write flows;
  neither blocks the other.
- **AI leverage:** the same artifacts that help humans (precise specs, golden vectors,
  automated conformance) let AI agents contribute and self-verify.
- **Trust by evidence:** tiers and conformance reports replace gatekeeping with measurement.
- **Durable contributions:** flows and connectors are decoupled and independently versioned,
  so contributions keep working as the ecosystem grows.
