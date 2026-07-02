//! Layer 1 — schema conformance.
//!
//! The contract schemas are embedded so the harness is self-contained. Statically, every
//! example payload embedded in a schema's `examples` array must validate against it; at run
//! time (layer 3) every captured payload is validated through [`Schemas::validate`].

use crate::report::Layer;
use jsonschema::Validator;

/// Which contract schema a payload must validate against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Sample,
    Command,
    Status,
    Config,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Sample => "sample",
            Kind::Command => "command",
            Kind::Status => "status",
            Kind::Config => "config",
        }
    }
}

const SOURCES: [(Kind, &str); 4] = [
    (Kind::Sample, include_str!("../../../doc/contract/schemas/sample.schema.json")),
    (Kind::Command, include_str!("../../../doc/contract/schemas/command.schema.json")),
    (Kind::Status, include_str!("../../../doc/contract/schemas/status.schema.json")),
    (Kind::Config, include_str!("../../../doc/contract/schemas/config.schema.json")),
];

pub struct Schemas {
    compiled: Vec<(Kind, serde_json::Value, Validator)>,
}

impl Schemas {
    pub fn load() -> Result<Schemas, String> {
        let mut compiled = Vec::new();
        for (kind, source) in SOURCES {
            let schema: serde_json::Value = serde_json::from_str(source)
                .map_err(|e| format!("{} schema is not valid JSON: {e}", kind.name()))?;
            let validator = jsonschema::validator_for(&schema)
                .map_err(|e| format!("{} schema does not compile: {e}", kind.name()))?;
            compiled.push((kind, schema, validator));
        }
        Ok(Schemas { compiled })
    }

    /// Validate a payload against one contract schema. `Ok(())` or every violation, joined.
    pub fn validate(&self, kind: Kind, payload: &serde_json::Value) -> Result<(), String> {
        let (_, _, validator) = self
            .compiled
            .iter()
            .find(|(k, _, _)| *k == kind)
            .expect("all kinds compiled");
        let errors: Vec<String> = validator
            .iter_errors(payload)
            .map(|e| format!("{} (at instance path '{}')", e, e.instance_path()))
            .collect();
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }

    /// Validate a raw payload (bytes off the wire) against one contract schema.
    pub fn validate_bytes(&self, kind: Kind, payload: &[u8]) -> Result<(), String> {
        let value: serde_json::Value =
            serde_json::from_slice(payload).map_err(|e| format!("payload is not JSON: {e}"))?;
        self.validate(kind, &value)
    }

    /// Layer 1 static pass: every example embedded in every schema validates against it.
    pub fn run_static(&self) -> Layer {
        let mut layer = Layer::new("Layer 1 — schema conformance (static examples)");
        for (kind, schema, _) in &self.compiled {
            let examples = schema
                .get("examples")
                .and_then(|e| e.as_array())
                .cloned()
                .unwrap_or_default();
            if examples.is_empty() {
                layer.fail(
                    &format!("L1/{}", kind.name()),
                    &format!("{} schema has embedded examples", kind.name()),
                    "schema declares no 'examples' array".into(),
                );
                continue;
            }
            for (i, example) in examples.iter().enumerate() {
                let id = format!("L1/{}-example-{i}", kind.name());
                let name = format!("{} schema example {i} validates", kind.name());
                layer.check(&id, &name, self.validate(*kind, example).map(|()| None));
            }
        }
        layer
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_embedded_examples_validate() {
        let schemas = Schemas::load().unwrap();
        let layer = schemas.run_static();
        let failures: Vec<_> = layer
            .checks
            .iter()
            .filter(|c| c.status == crate::report::Status::Fail)
            .collect();
        assert!(failures.is_empty(), "{failures:?}");
        assert!(layer.checks.len() >= 10, "expected all examples covered");
    }

    #[test]
    fn a_contract_violation_is_reported() {
        let schemas = Schemas::load().unwrap();
        // typed+good sample without value/value_repr violates the sample schema
        let bad = serde_json::json!({
            "ts": "2026-05-30T10:00:00.000Z",
            "device": "plc-1",
            "protocol": "modbus",
            "point": "p",
            "mode": "typed",
            "datatype": "uint16",
            "raw": "1234",
            "quality": "good",
            "addr": {}
        });
        assert!(schemas.validate(Kind::Sample, &bad).is_err());
    }

    #[test]
    fn link_status_with_info_descriptor_validates() {
        let schemas = Schemas::load().unwrap();
        let link = serde_json::json!({
            "status": "connected",
            "since": "2026-05-30T09:59:00.000Z",
            "info": { "protocol": "modbus", "transport": "tcp", "host": "10.0.0.9" }
        });
        schemas.validate(Kind::Status, &link).unwrap();
    }
}
