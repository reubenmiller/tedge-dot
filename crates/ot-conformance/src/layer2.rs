//! Layer 2 — decode conformance (golden vectors).
//!
//! The vectors live in the SDK (`tedge_dot_sdk::conformance`), because every connector decodes
//! through the SDK's `decode_primitive`/`encode_primitive`. The harness runs each vector that
//! is applicable to the connector's manifest: vectors for datatypes the connector does not
//! advertise are skipped (they cannot be reached through it), and bitfield vectors only run
//! when the `bitfield` feature is claimed.

use crate::manifest::Manifest;
use crate::report::Layer;
use tedge_dot_sdk::conformance::{parse_vectors, run_vector, Vector, BUILTIN_VECTORS};

pub fn run(manifest: &Manifest, vectors_override: Option<&str>) -> Result<Layer, String> {
    let mut layer = Layer::new("Layer 2 — decode conformance (golden vectors)");
    let vectors = parse_vectors(vectors_override.unwrap_or(BUILTIN_VECTORS))?;

    let mut applicable = 0;
    for vector in &vectors {
        let id = format!("L2/{}", vector.id);
        let name = describe(vector);
        if let Some(reason) = skip_reason(manifest, vector) {
            layer.skip(&id, &name, reason);
            continue;
        }
        applicable += 1;
        layer.check(&id, &name, run_vector(vector).map(|()| None));
    }

    // A manifest that advertises datatypes with zero vector coverage would silently prove
    // nothing — fail loudly instead.
    for datatype in &manifest.connector.datatypes {
        if !vectors.iter().any(|v| datatype_name(v) == *datatype) {
            layer.fail(
                &format!("L2/coverage-{datatype}"),
                &format!("advertised datatype '{datatype}' has golden vectors"),
                "no vector covers this datatype; add one to crates/sdk/conformance/vectors.json"
                    .into(),
            );
        }
    }
    if applicable == 0 {
        layer.fail(
            "L2/coverage",
            "at least one golden vector applies",
            "the manifest matches no vectors at all".into(),
        );
    }
    Ok(layer)
}

fn describe(v: &Vector) -> String {
    match &v.note {
        Some(note) => format!("{} ({note})", datatype_name(v)),
        None => datatype_name(v),
    }
}

fn skip_reason(manifest: &Manifest, v: &Vector) -> Option<String> {
    let dt = datatype_name(v);
    if v.bitfield.is_some() && !manifest.connector.features.iter().any(|f| f == "bitfield") {
        return Some("connector does not claim the 'bitfield' feature".into());
    }
    if !manifest.connector.datatypes.contains(&dt) {
        return Some(format!("datatype '{dt}' not advertised by the connector"));
    }
    None
}

fn datatype_name(v: &Vector) -> String {
    serde_json::to_value(v.datatype)
        .ok()
        .and_then(|x| x.as_str().map(str::to_string))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::Status;

    fn manifest(datatypes: &[&str], features: &[&str]) -> Manifest {
        let toml = format!(
            "[connector]\nprotocol = \"modbus\"\nmodes = [\"typed\"]\ndatatypes = {:?}\nfeatures = {:?}\n",
            datatypes, features
        );
        toml::from_str(&toml).unwrap()
    }

    #[test]
    fn all_applicable_vectors_pass_for_a_full_manifest() {
        let m = manifest(
            &[
                "bool", "int8", "uint8", "int16", "uint16", "int32", "uint32", "int64", "uint64",
                "float32", "float64", "string", "bytes",
            ],
            &["bitfield"],
        );
        let layer = run(&m, None).unwrap();
        let failures: Vec<_> = layer.checks.iter().filter(|c| c.status == Status::Fail).collect();
        assert!(failures.is_empty(), "{failures:?}");
        assert!(layer.checks.iter().all(|c| c.status != Status::Skip));
    }

    #[test]
    fn unadvertised_datatypes_are_skipped() {
        let m = manifest(&["uint16"], &[]);
        let layer = run(&m, None).unwrap();
        assert!(layer
            .checks
            .iter()
            .any(|c| c.id.contains("float32") && c.status == Status::Skip));
        assert!(layer
            .checks
            .iter()
            .any(|c| c.id.contains("bitfield") && c.status == Status::Skip));
    }

    #[test]
    fn advertised_datatype_without_vectors_fails_coverage() {
        let mut m = manifest(&["uint16"], &[]);
        m.connector.datatypes.push("madeup42".into());
        let layer = run(&m, None).unwrap();
        assert!(layer
            .checks
            .iter()
            .any(|c| c.id == "L2/coverage-madeup42" && c.status == Status::Fail));
    }
}
