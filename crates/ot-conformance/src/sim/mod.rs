//! Protocol simulators for the behavioural layer.
//!
//! One trait, one built-in implementation per protocol. A simulator is seeded from the
//! manifest's `[simulator] seed` file, serves the real protocol on a loopback port, and gives
//! the harness ground truth: the bytes a point should read, whether an address is seeded
//! invalid, which writes arrived, and an outage switch for the link-drop check (B5).

#[cfg(feature = "modbus")]
pub mod modbus;
#[cfg(feature = "opcua")]
pub mod opcua;
pub mod proxy;

use tedge_dot_sdk::{DataType, Mode};

/// A point the harness asks the simulator about, resolved from the connector configuration.
#[derive(Debug, Clone)]
pub struct PointSpec {
    /// The point's protocol-specific `address` object (opaque to the contract).
    pub address: serde_json::Value,
    pub datatype: Option<DataType>,
    pub mode: Mode,
}

/// Ground-truth data for one point, as the connector should read it right now.
#[derive(Debug, Clone)]
pub struct PointData {
    /// The "natural" wire bytes (registers serialized big-endian, in word order).
    pub bytes: Vec<u8>,
    /// Hex grouping width the contract expects in the sample `raw` field.
    pub raw_group: usize,
}

#[async_trait::async_trait]
pub trait Simulator: Send + Sync {
    /// Loopback port the simulator listens on.
    fn port(&self) -> u16;

    /// The bytes the connector should currently read for this point (reflects writes).
    fn point_data(&self, point: &PointSpec) -> Result<PointData, String>;

    /// True when reads of this point are seeded to fail (behavioural check B4).
    fn is_invalid(&self, point: &PointSpec) -> bool;

    /// Number of protocol writes observed at this point's address (B6/B7).
    fn write_count(&self, point: &PointSpec) -> Result<usize, String>;

    /// Application-level outage: while on, every request fails but the transport stays
    /// connectable (B5, first half).
    fn set_outage(&self, on: bool);

    /// Transport-level state: `false` closes the listener and kills live sessions — the
    /// connector's TCP connection actually dies; `true` restores it on the same port (B5,
    /// second half — exercises the runtime's reconnect-with-backoff).
    async fn set_transport(&self, up: bool) -> Result<(), String>;

    /// Rewrite a device's `protocol_address` (TOML) to point at this simulator.
    fn rewrite_protocol_address(&self, address: &mut toml::Value) -> Result<(), String>;
}

/// Simulator kinds this harness build can run.
pub fn supported_kinds() -> &'static [&'static str] {
    &[
        #[cfg(feature = "modbus")]
        "modbus-tcp",
        #[cfg(feature = "opcua")]
        "opcua",
    ]
}

/// Instantiate the built-in simulator named by the manifest.
pub async fn build(kind: &str, seed_path: &std::path::Path) -> Result<Box<dyn Simulator>, String> {
    match kind {
        #[cfg(feature = "modbus")]
        "modbus-tcp" => Ok(Box::new(modbus::ModbusSim::start(seed_path).await?)),
        #[cfg(feature = "opcua")]
        "opcua" => Ok(Box::new(opcua::OpcuaSim::start(seed_path).await?)),
        other => Err(format!(
            "no built-in simulator for kind '{other}' (compiled-in: {})",
            supported_kinds().join(", ")
        )),
    }
}
