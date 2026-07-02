//! Every builtin golden vector must pass against the SDK decode/encode helpers.
//! This is Layer 2 of the conformance suite, enforced on every `cargo test`.

use tedge_dot_sdk::conformance::{parse_vectors, run_vector, BUILTIN_VECTORS};

#[test]
fn all_builtin_vectors_pass() {
    let vectors = parse_vectors(BUILTIN_VECTORS).expect("builtin vector file parses");
    let failures: Vec<String> = vectors
        .iter()
        .filter_map(|v| run_vector(v).err().map(|e| format!("{}: {e}", v.id)))
        .collect();
    assert!(
        failures.is_empty(),
        "{} golden vector(s) failed:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
