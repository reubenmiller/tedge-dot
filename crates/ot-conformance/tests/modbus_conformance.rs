//! The shipped Modbus connector must pass its own conformance suite, end to end: schema
//! examples, all applicable golden vectors, and the full behavioural layer against the
//! built-in simulator and test broker. No external processes, no hardware.

use ot_conformance::{manifest::Manifest, run_suite, Selection};
use std::path::Path;

#[tokio::test(flavor = "multi_thread")]
async fn modbus_connector_is_conformant() {
    let spec = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../connectors/modbus/conformance.toml");
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

    let text = report.render_text();
    println!("{text}");
    assert!(report.conformant(), "\n{text}");

    // sanity: the run actually exercised the interesting checks
    let ids: Vec<String> = report
        .layers
        .iter()
        .flat_map(|l| l.checks.iter().map(|c| c.id.clone()))
        .collect();
    for expected in [
        "B1-capabilities",
        "B2-seq",
        "B4-bad-quality",
        "B5-drop",
        "B5-recovery",
        "B5-transport-drop",
        "B5-transport-recovery",
        "B5-transport-dataflow",
        "B6-write-setpoint_u16",
        "B6-write-setpoint_f32",
        "B6-write-coil_rw",
        "B7-access",
        "B8-hot-reload",
        "B9-manifest",
        "B9-honesty",
        "B10-topics",
    ] {
        assert!(
            ids.iter().any(|id| id == expected),
            "check {expected} was never executed; ran: {ids:?}"
        );
    }
}
