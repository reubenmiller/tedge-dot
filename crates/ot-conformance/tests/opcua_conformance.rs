//! The shipped OPC UA connector must pass its full conformance suite: schema examples,
//! golden vectors, capability agreement, and the behavioural layer against the embedded
//! async-opcua server + transport proxy. No external processes, no hardware.

use ot_conformance::{manifest::Manifest, run_suite, Selection};
use std::path::Path;

#[tokio::test(flavor = "multi_thread")]
async fn opcua_connector_is_conformant() {
    // Opt-in wire logging for debugging: RUST_LOG=tedge_dot_sdk=debug,ot_conformance=debug
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();
    let spec = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../connectors/opcua/conformance.toml");
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
        "S1-capabilities",
        "B1-capabilities",
        "B2-sample-big_counter", // uint64 above the JS safe range -> string value
        "B2-sample-label",       // string datatype
        "B3-raw",
        "B4-bad-quality",
        "B5-drop",
        "B5-recovery",
        "B5-transport-drop",
        "B5-transport-recovery",
        "B5-transport-dataflow",
        "B6-write-setpoint",
        "B6-write-target_temp",
        "B6-write-enable",
        "B6-write-recipe",
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
