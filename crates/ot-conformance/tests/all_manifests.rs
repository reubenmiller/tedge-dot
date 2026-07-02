//! Every connector in the repository ships a conformance manifest, and every manifest must
//! pass the suite. Protocols without a built-in simulator run the static layers (schema
//! examples, golden vectors, manifest ↔ compiled capability agreement) and skip behavioural;
//! this still catches capability drift and vector regressions for all of them in CI.

use ot_conformance::report::Status;
use ot_conformance::{manifest::Manifest, run_suite, Selection};
use std::path::{Path, PathBuf};

fn connectors_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../connectors")
}

#[test]
fn every_connector_ships_a_conformance_manifest() {
    let missing: Vec<String> = std::fs::read_dir(connectors_dir())
        .expect("connectors dir")
        .flatten()
        .filter(|e| e.path().is_dir() && !e.file_name().to_string_lossy().starts_with('_'))
        .filter(|e| !e.path().join("conformance.toml").is_file())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        missing.is_empty(),
        "connectors without a conformance.toml: {missing:?}"
    );
}

async fn assert_conformant(protocol: &str) {
    let spec = connectors_dir().join(protocol).join("conformance.toml");
    let manifest = Manifest::load(&spec).expect("manifest loads");
    let report = run_suite(
        &manifest,
        Selection {
            static_layers: true,
            behavioural: true,
        },
        None,
    )
    .await
    .expect("suite runs");
    assert!(report.conformant(), "\n{}", report.render_text());
    // the capability agreement check must have actually run (not skipped) when the module
    // is compiled into the harness
    let caps_check = report
        .layers
        .iter()
        .flat_map(|l| &l.checks)
        .find(|c| c.id == "S1-capabilities")
        .expect("S1-capabilities executed");
    if ot_conformance::host::build_connector(protocol).is_ok() {
        assert_eq!(
            caps_check.status,
            Status::Pass,
            "capability agreement must run for compiled-in protocol {protocol}: {:?}",
            caps_check.detail
        );
    }
}

// modbus and opcua have dedicated full-suite tests (modbus_conformance.rs,
// opcua_conformance.rs) — running them again here would double the behavioural runs.

#[tokio::test(flavor = "multi_thread")]
async fn canbus_manifest_is_conformant() {
    assert_conformant("canbus").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn canopen_manifest_is_conformant() {
    assert_conformant("canopen").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn profibus_manifest_is_conformant() {
    assert_conformant("profibus").await;
}
