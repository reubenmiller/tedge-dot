//! `ot-conformance` — the conformance harness for thin-edge.io OT connectors.
//!
//! Three layers (see `doc/conformance/conformance-suite.md`):
//! 1. schema conformance — payloads vs the contract JSON Schemas,
//! 2. decode conformance — the SDK's golden decode/encode vectors,
//! 3. behavioural conformance — the real connector against a protocol simulator and a test
//!    broker, checks B1–B10.

pub mod broker;
pub mod host;
pub mod layer1;
pub mod layer2;
pub mod layer3;
pub mod manifest;
pub mod report;
pub mod sim;

use manifest::Manifest;
use report::{Layer, Report};

/// Which layers to execute.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    /// Layers 1–2: schema examples + golden vectors. No simulator needed.
    pub static_layers: bool,
    /// Layer 3: connector ⇄ simulator ⇄ broker.
    pub behavioural: bool,
}

/// Run the selected layers and collect a report. IO-free apart from the behavioural layer.
pub async fn run_suite(
    manifest: &Manifest,
    selection: Selection,
    vectors_override: Option<&str>,
) -> Result<Report, String> {
    let schemas = layer1::Schemas::load()?;
    let mut report = Report::new(&manifest.connector.protocol);

    if selection.static_layers {
        report.layers.push(schemas.run_static());
        report.layers.push(layer2::run(manifest, vectors_override)?);
        report.layers.push(static_capability_agreement(manifest));
    }
    if selection.behavioural {
        match behavioural_skip_reason(manifest) {
            None => report.layers.extend(layer3::run(manifest, &schemas).await?),
            Some(reason) => {
                // Not every protocol has a built-in simulator yet; skipping keeps the static
                // layers and capability check meaningful for those connectors without failing.
                let mut layer = Layer::new("Layer 3 — behavioural conformance");
                layer.skip(
                    "B-behavioural",
                    "behavioural checks (connector ⇄ simulator ⇄ broker)",
                    reason,
                );
                report.layers.push(layer);
            }
        }
    }
    Ok(report)
}

/// Why the behavioural layer cannot run for this manifest, if it cannot. A declared
/// simulator kind the harness has no built-in implementation for (e.g. the Dockerised vcan
/// stacks) is a skip, not a failure — the static layers still gate the connector.
fn behavioural_skip_reason(manifest: &Manifest) -> Option<String> {
    let Some(simulator) = &manifest.simulator else {
        return Some("the manifest declares no [simulator]; add one to run checks B1-B10".into());
    };
    if !sim::supported_kinds().contains(&simulator.kind.as_str()) {
        return Some(format!(
            "no built-in simulator for kind '{}' (compiled-in: {}); behavioural coverage \
             comes from the protocol's e2e suite instead",
            simulator.kind,
            sim::supported_kinds().join(", ")
        ));
    }
    if manifest.harness.config.is_none() {
        return Some("the manifest declares no `[harness] config` connector configuration".into());
    }
    None
}

/// B9 without a broker: build the protocol module in-process (when it is compiled into the
/// harness), take its raw capability descriptor, apply the SDK management augmentation, and
/// require agreement with the manifest. Catches capability drift for every connector — even
/// those without a behavioural simulator yet.
fn static_capability_agreement(manifest: &Manifest) -> Layer {
    let mut layer = Layer::new("Manifest — capability agreement (static)");
    let id = "S1-capabilities";
    let name = "manifest agrees with the compiled module's capability descriptor";
    match host::build_connector(&manifest.connector.protocol) {
        Ok(connector) => {
            let mut caps = connector.capabilities().to_json();
            layer3::augment_caps(&mut caps);
            let mismatches = layer3::manifest_caps_mismatches(manifest, &caps);
            if mismatches.is_empty() {
                layer.pass(id, name, None);
            } else {
                layer.fail(id, name, mismatches.join("\n"));
            }
        }
        Err(reason) => layer.skip(id, name, reason),
    }
    layer
}
