//! Protocol-specific configuration for the CAN bus connector.
//!
//! Fills the contract's opaque slots: `connection` (empty), `device.protocol_address`,
//! and `point.address`. Signal metadata resolved from the DBC file is kept in
//! [`ResolvedSignal`] after `configure()`.

use serde::Deserialize;
use std::path::PathBuf;

/// Shared `[connection]` block — no global parameters required for SocketCAN.
/// Accepted as `{}` or omitted entirely.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct CanbusConnection {}

/// `device.protocol_address` — the SocketCAN interface and DBC file for this device.
#[derive(Debug, Clone, Deserialize)]
pub struct CanInterface {
    /// SocketCAN interface name, e.g. `"can0"` or `"vcan0"`.
    pub interface: String,
    /// Nominal bit rate in bit/s; logged for diagnostics, not set by the connector.
    #[serde(default)]
    pub bitrate: Option<u32>,
    /// Absolute path to the DBC file that defines messages and signals for this device.
    pub dbc_file: PathBuf,
}

/// `point.address` — DBC message/signal names that identify one CAN signal.
#[derive(Debug, Clone, Deserialize)]
pub struct CanSignalAddress {
    /// Name of the DBC `BO_` message block (e.g. `"ENGINE_STATUS"`).
    pub message_name: String,
    /// Name of the DBC `SG_` signal within that message (e.g. `"RPM"`).
    pub signal_name: String,
}

/// Byte order of a DBC signal (Intel = little-endian, Motorola = big-endian).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CanByteOrder {
    Intel,
    Motorola,
}

/// Value type of a DBC signal after bit extraction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalValueType {
    Unsigned,
    Signed,
    /// IEEE-754 32-bit float (bit_count must be 32).
    Float32,
    /// IEEE-754 64-bit float (bit_count must be 64).
    Float64,
}

/// A fully resolved CAN signal, derived from the DBC at `configure()` time.
#[derive(Debug, Clone)]
pub struct ResolvedSignal {
    /// 11-bit (standard) or 29-bit (extended) CAN identifier.
    pub can_id: u32,
    /// DBC `start_bit`: LSBit position for Intel, MSBit position for Motorola.
    pub start_bit: u16,
    /// Number of bits in the signal.
    pub bit_count: u8,
    pub byte_order: CanByteOrder,
    pub value_type: SignalValueType,
}

/// Load and parse a DBC file, returning the raw [`can_dbc::DBC`] object.
///
/// Returns `Err` if the file cannot be read or is not valid DBC syntax.
pub fn load_dbc(path: &std::path::Path) -> Result<can_dbc::DBC, String> {
    let bytes =
        std::fs::read(path).map_err(|e| format!("cannot read DBC file {}: {e}", path.display()))?;
    can_dbc::DBC::from_slice(&bytes)
        .map_err(|e| format!("failed to parse DBC file {}: {e:?}", path.display()))
}

/// Resolve a DBC message/signal address to a [`ResolvedSignal`].
///
/// Returns `Err` if the message or signal name is not found in the DBC.
pub fn resolve_signal(
    dbc: &can_dbc::DBC,
    message_name: &str,
    signal_name: &str,
) -> Result<ResolvedSignal, String> {
    let message = dbc
        .messages()
        .iter()
        .find(|m| m.message_name() == message_name)
        .ok_or_else(|| format!("DBC message '{message_name}' not found"))?;

    let signal = message
        .signals()
        .iter()
        .find(|s| s.name() == signal_name)
        .ok_or_else(|| {
            format!("DBC signal '{signal_name}' not found in message '{message_name}'")
        })?;

    // can-dbc represents the CAN ID as a raw u32; standard frames use the 11 low bits,
    // extended frames set bit 31 (0x80000000). Strip that flag for the wire ID.
    let raw_id = message.message_id().0;
    let can_id = raw_id & 0x1FFF_FFFF;

    let byte_order = match signal.byte_order() {
        can_dbc::ByteOrder::LittleEndian => CanByteOrder::Intel,
        can_dbc::ByteOrder::BigEndian => CanByteOrder::Motorola,
    };

    let value_type = match signal.value_type() {
        can_dbc::ValueType::Signed => SignalValueType::Signed,
        can_dbc::ValueType::Unsigned => SignalValueType::Unsigned,
    };

    Ok(ResolvedSignal {
        can_id,
        start_bit: signal.start_bit as u16,
        bit_count: signal.signal_size as u8,
        byte_order,
        value_type,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    const SAMPLE_DBC: &str = r#"
VERSION ""

NS_ :

BS_:

BU_:

BO_ 416 ENGINE_STATUS: 8 Vector__XXX
 SG_ RPM : 0|16@1+ (1,0) [0|0] "rpm" Vector__XXX
 SG_ COOLANT_TEMP : 16|8@1- (1,0) [0|0] "degC" Vector__XXX
 SG_ BRAKE_ACTIVE : 24|1@1+ (1,0) [0|0] "" Vector__XXX

"#;

    fn write_dbc(content: &str) -> NamedTempFile {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(content.as_bytes()).unwrap();
        f
    }

    #[test]
    fn load_and_resolve_unsigned_signal() {
        let f = write_dbc(SAMPLE_DBC);
        let dbc = load_dbc(f.path()).unwrap();
        let sig = resolve_signal(&dbc, "ENGINE_STATUS", "RPM").unwrap();
        assert_eq!(sig.can_id, 416);
        assert_eq!(sig.start_bit, 0);
        assert_eq!(sig.bit_count, 16);
        assert_eq!(sig.byte_order, CanByteOrder::Intel);
        assert_eq!(sig.value_type, SignalValueType::Unsigned);
    }

    #[test]
    fn resolve_signed_signal() {
        let f = write_dbc(SAMPLE_DBC);
        let dbc = load_dbc(f.path()).unwrap();
        let sig = resolve_signal(&dbc, "ENGINE_STATUS", "COOLANT_TEMP").unwrap();
        assert_eq!(sig.bit_count, 8);
        assert_eq!(sig.value_type, SignalValueType::Signed);
    }

    #[test]
    fn resolve_bool_signal() {
        let f = write_dbc(SAMPLE_DBC);
        let dbc = load_dbc(f.path()).unwrap();
        let sig = resolve_signal(&dbc, "ENGINE_STATUS", "BRAKE_ACTIVE").unwrap();
        assert_eq!(sig.bit_count, 1);
        assert_eq!(sig.value_type, SignalValueType::Unsigned);
    }

    #[test]
    fn unknown_message_returns_error() {
        let f = write_dbc(SAMPLE_DBC);
        let dbc = load_dbc(f.path()).unwrap();
        assert!(resolve_signal(&dbc, "NO_SUCH_MSG", "RPM").is_err());
    }

    #[test]
    fn unknown_signal_returns_error() {
        let f = write_dbc(SAMPLE_DBC);
        let dbc = load_dbc(f.path()).unwrap();
        assert!(resolve_signal(&dbc, "ENGINE_STATUS", "NO_SUCH_SIG").is_err());
    }

    #[test]
    fn deserialize_can_interface() {
        let v: CanInterface = serde_json::from_str(
            r#"{"interface":"vcan0","bitrate":500000,"dbc_file":"/tmp/test.dbc"}"#,
        )
        .unwrap();
        assert_eq!(v.interface, "vcan0");
        assert_eq!(v.bitrate, Some(500000));
    }

    #[test]
    fn deserialize_signal_address() {
        let v: CanSignalAddress = serde_json::from_str(
            r#"{"message_name":"ENGINE_STATUS","signal_name":"RPM"}"#,
        )
        .unwrap();
        assert_eq!(v.message_name, "ENGINE_STATUS");
        assert_eq!(v.signal_name, "RPM");
    }
}
