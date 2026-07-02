//! The `conformance.toml` manifest a connector ships to declare what it claims.
//!
//! See `doc/conformance/conformance-suite.md` §5. The harness selects the applicable golden
//! vectors and behavioural checks from it and cross-checks it against the live capability
//! descriptor (check B9).

use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub connector: ConnectorClaims,
    #[serde(default)]
    pub simulator: Option<Simulator>,
    #[serde(default)]
    pub harness: Harness,
    /// Directory the manifest was loaded from; relative paths resolve against it.
    #[serde(skip)]
    pub base_dir: PathBuf,
}

/// What the connector claims to support. Must agree with the live capability descriptor.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectorClaims {
    pub protocol: String,
    pub modes: Vec<String>,
    #[serde(default)]
    pub datatypes: Vec<String>,
    #[serde(default)]
    pub point_kinds: Vec<String>,
    #[serde(default)]
    pub verbs: Vec<String>,
    #[serde(default)]
    pub features: Vec<String>,
    #[serde(default)]
    pub subscribe: bool,
}

/// The protocol simulator the behavioural layer runs the connector against.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Simulator {
    /// Built-in simulator id (currently: `modbus-tcp`).
    pub kind: String,
    /// Seed data file (registers/coils/invalid addresses), relative to the manifest.
    pub seed: PathBuf,
    /// Informational: an external simulator image for container-based setups. Unused by the
    /// built-in harness.
    #[serde(default)]
    pub image: Option<String>,
}

/// How the harness hosts the connector under test.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Harness {
    /// Connector configuration template, relative to the manifest. The harness rewrites the
    /// `[mqtt]` section and each device's `protocol_address` to point at the test broker and
    /// simulator before starting the connector.
    #[serde(default)]
    pub config: Option<PathBuf>,
    /// External connector launch command (the rewritten config path is appended as the last
    /// argument). When absent the harness runs the protocol module in-process — bit-identical
    /// to the shipped binary, which links the same module and SDK runtime.
    #[serde(default)]
    pub command: Vec<String>,
}

impl Manifest {
    pub fn load(path: &Path) -> Result<Manifest, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("failed to read manifest '{}': {e}", path.display()))?;
        let mut manifest: Manifest = toml::from_str(&text)
            .map_err(|e| format!("failed to parse manifest '{}': {e}", path.display()))?;
        manifest.base_dir = path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        Ok(manifest)
    }

    /// Resolve a manifest-relative path.
    pub fn resolve(&self, p: &Path) -> PathBuf {
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.base_dir.join(p)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_documented_example() {
        let m: Manifest = toml::from_str(
            r#"
[connector]
protocol  = "modbus"
modes     = ["raw", "typed"]
datatypes = ["bool", "int16", "uint16", "int32", "uint32", "float32", "float64"]
verbs     = ["write"]
features  = ["polling", "bitfield"]
subscribe = false

[simulator]
kind  = "modbus-tcp"
image = "connectors/modbus/sim"
seed  = "conformance/seed/modbus.json"
"#,
        )
        .unwrap();
        assert_eq!(m.connector.protocol, "modbus");
        assert_eq!(m.simulator.as_ref().unwrap().kind, "modbus-tcp");
        assert!(m.harness.command.is_empty());
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let err = toml::from_str::<Manifest>("[connector]\nprotocol = \"x\"\nmodes = []\nbogus = 1\n")
            .unwrap_err();
        assert!(err.to_string().contains("bogus"), "{err}");
    }
}
