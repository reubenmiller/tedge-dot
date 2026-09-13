//! Renderings of the device manifest (contract §8.2) for something other than the contract.
//!
//! `tedge-dot manifest` prints the manifest itself; `--format <name>` prints one of these
//! instead. They live in the binary crate, not in the SDK, and they take a *manifest* rather
//! than a configuration (RFC 0006 §8.1). Both halves of that matter:
//!
//! * The SDK is the cloud-agnostic driver — "the connector never talks to the DTM service".
//!   A vendor's JSON-schema dialect has no business inside it, and it was the largest single
//!   piece of code in either SDK.
//! * Rendering from the manifest makes a format a pure function of the message the connector
//!   already publishes, so a new one is a new function with no configuration parsing in it, and
//!   anything else that consumes manifests — another cloud, an MCP server, a commissioning
//!   tool — starts from the same input rather than from `/etc/tedge/plugins/ot`.

pub mod c8y_dtm;

use serde_json::Value;

/// One device's manifest, with the device it belongs to.
///
/// The published manifest does not carry the device name — the topic does — so the CLI, which
/// has no topic, carries it alongside and adds it to the JSON it prints.
pub struct DeviceManifest {
    pub device: String,
    pub manifest: Value,
}

impl DeviceManifest {
    /// The manifest as printed by `--format json`: the published document plus the `device`
    /// name the topic would have carried, so a reader can tell whose manifest it is.
    pub fn to_json(&self) -> Value {
        let mut out = serde_json::Map::new();
        out.insert("device".into(), Value::String(self.device.clone()));
        if let Some(fields) = self.manifest.as_object() {
            for (key, value) in fields {
                out.insert(key.clone(), value.clone());
            }
        }
        Value::Object(out)
    }
}
