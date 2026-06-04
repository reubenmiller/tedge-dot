//! Protocol-specific configuration for the CANopen connector.
//!
//! Fills the contract's opaque slots: `connection` (global CAN interface),
//! `device.protocol_address` (CANopen node ID), and `point.address` (OD index + subindex).

use serde::Deserialize;

/// Shared `[connection]` block — the SocketCAN interface name used by all nodes on this bus.
#[derive(Debug, Clone, Deserialize)]
pub struct CanopenConnection {
    /// SocketCAN interface name, e.g. `"can0"` or `"vcan0"`.
    pub interface: String,
}

/// `device.protocol_address` — the CANopen node ID (1–127) for this device.
#[derive(Debug, Clone, Deserialize)]
pub struct NodeAddress {
    /// CANopen node ID in the range 1–127.
    pub node_id: u8,
}

impl NodeAddress {
    pub fn validate(&self) -> Result<(), String> {
        if self.node_id == 0 || self.node_id > 127 {
            return Err(format!(
                "node_id {} is out of range (must be 1–127)",
                self.node_id
            ));
        }
        Ok(())
    }
}

/// `point.address` — the Object Dictionary (OD) address for one data point.
///
/// The `index` is the 16-bit OD index; `subindex` is the 8-bit sub-entry.
/// Most simple objects use `subindex = 0`.
#[derive(Debug, Clone, Deserialize)]
pub struct OdAddress {
    /// OD index (0x0001–0xFFFF).
    pub index: u16,
    /// OD sub-index (0x00–0xFF).
    #[serde(default)]
    pub subindex: u8,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_canopen_connection() {
        let v: CanopenConnection =
            serde_json::from_str(r#"{"interface":"vcan0"}"#).unwrap();
        assert_eq!(v.interface, "vcan0");
    }

    #[test]
    fn deserialize_node_address() {
        let v: NodeAddress = serde_json::from_str(r#"{"node_id":1}"#).unwrap();
        assert_eq!(v.node_id, 1);
    }

    #[test]
    fn node_address_zero_is_invalid() {
        let v = NodeAddress { node_id: 0 };
        assert!(v.validate().is_err());
    }

    #[test]
    fn node_address_128_is_invalid() {
        let v = NodeAddress { node_id: 128 };
        assert!(v.validate().is_err());
    }

    #[test]
    fn node_address_127_is_valid() {
        let v = NodeAddress { node_id: 127 };
        assert!(v.validate().is_ok());
    }

    #[test]
    fn deserialize_od_address() {
        let v: OdAddress =
            serde_json::from_str(r#"{"index":8192,"subindex":0}"#).unwrap();
        assert_eq!(v.index, 0x2000);
        assert_eq!(v.subindex, 0);
    }

    #[test]
    fn deserialize_od_address_subindex_defaults_to_zero() {
        let v: OdAddress = serde_json::from_str(r#"{"index":8192}"#).unwrap();
        assert_eq!(v.subindex, 0);
    }
}
