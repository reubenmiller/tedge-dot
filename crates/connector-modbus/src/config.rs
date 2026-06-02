//! Protocol-specific configuration for the Modbus connector. These structs fill the contract's
//! opaque slots: `connection`, `device.protocol_address`, and `point.address`.

use serde::Deserialize;

/// Shared `[connection]` defaults (RTU serial settings).
#[derive(Debug, Clone, Deserialize, Default)]
pub struct ModbusConnection {
    #[serde(default)]
    pub serial: SerialDefaults,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SerialDefaults {
    #[serde(default = "default_baudrate")]
    pub baudrate: u32,
    #[serde(default = "default_parity")]
    pub parity: String,
    #[serde(default = "default_stopbits")]
    pub stopbits: u8,
    #[serde(default = "default_databits")]
    pub databits: u8,
}

impl Default for SerialDefaults {
    fn default() -> Self {
        SerialDefaults {
            baudrate: default_baudrate(),
            parity: default_parity(),
            stopbits: default_stopbits(),
            databits: default_databits(),
        }
    }
}

/// `device.protocol_address` — how to reach a Modbus device over TCP or RTU.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "transport", rename_all = "lowercase")]
pub enum ProtocolAddress {
    Tcp {
        host: String,
        #[serde(default = "default_tcp_port")]
        port: u16,
        unit_id: u8,
    },
    Rtu {
        serial_port: String,
        unit_id: u8,
        #[serde(default)]
        baudrate: Option<u32>,
        #[serde(default)]
        parity: Option<String>,
        #[serde(default)]
        stopbits: Option<u8>,
        #[serde(default)]
        databits: Option<u8>,
    },
}

impl ProtocolAddress {
    pub fn unit_id(&self) -> u8 {
        match self {
            ProtocolAddress::Tcp { unit_id, .. } => *unit_id,
            ProtocolAddress::Rtu { unit_id, .. } => *unit_id,
        }
    }
}

/// The four standard Modbus data tables.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Table {
    Coil,
    #[serde(rename = "discrete_input")]
    DiscreteInput,
    Holding,
    Input,
}

impl Table {
    /// Whether this table is bit-addressed (coils / discrete inputs) rather than 16-bit registers.
    pub fn is_bit(self) -> bool {
        matches!(self, Table::Coil | Table::DiscreteInput)
    }

    /// Whether this table can be written.
    pub fn is_writable(self) -> bool {
        matches!(self, Table::Coil | Table::Holding)
    }
}

/// `point.address` — how to address a Modbus point.
#[derive(Debug, Clone, Deserialize)]
pub struct ModbusAddress {
    pub table: Table,
    pub address: u16,
    #[serde(default)]
    pub count: Option<u16>,
    #[serde(default)]
    pub start_bit: Option<u32>,
    #[serde(default)]
    pub bit_count: Option<u32>,
}

fn default_baudrate() -> u32 {
    9600
}
fn default_parity() -> String {
    "N".to_string()
}
fn default_stopbits() -> u8 {
    2
}
fn default_databits() -> u8 {
    8
}
fn default_tcp_port() -> u16 {
    502
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_tcp_protocol_address() {
        let v = serde_json::json!({
            "transport": "tcp", "host": "192.168.0.10", "port": 502, "unit_id": 1
        });
        let pa: ProtocolAddress = serde_json::from_value(v).unwrap();
        assert!(matches!(pa, ProtocolAddress::Tcp { .. }));
        assert_eq!(pa.unit_id(), 1);
    }

    #[test]
    fn parse_rtu_protocol_address() {
        let v = serde_json::json!({
            "transport": "rtu", "serial_port": "/dev/ttyRS485", "unit_id": 2,
            "baudrate": 19200, "parity": "N", "stopbits": 1, "databits": 8
        });
        let pa: ProtocolAddress = serde_json::from_value(v).unwrap();
        match pa {
            ProtocolAddress::Rtu { serial_port, baudrate, .. } => {
                assert_eq!(serial_port, "/dev/ttyRS485");
                assert_eq!(baudrate, Some(19200));
            }
            _ => panic!("expected RTU"),
        }
    }

    #[test]
    fn parse_point_address() {
        let v = serde_json::json!({ "table": "holding", "address": 7, "count": 2 });
        let a: ModbusAddress = serde_json::from_value(v).unwrap();
        assert_eq!(a.table, Table::Holding);
        assert_eq!(a.address, 7);
        assert_eq!(a.count, Some(2));
    }

    #[test]
    fn discrete_input_renamed() {
        let v = serde_json::json!({ "table": "discrete_input", "address": 0 });
        let a: ModbusAddress = serde_json::from_value(v).unwrap();
        assert_eq!(a.table, Table::DiscreteInput);
    }
}
