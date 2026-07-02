//! Integration tests for the CAN bus connector.
//!
//! These tests exercise the full DBC-loading → signal-resolution → bit-extraction pipeline
//! using the same `test.dbc` that the e2e simulator uses.  No CAN socket is opened; the
//! tests run on all platforms (including macOS CI).
//!
//! Signal layout under test (ENGINE_STATUS, ID=0x1A0/416):
//!   RPM          u16 Intel bits  0-15   seeded at 2500  (0x09C4)
//!   COOLANT_TEMP i8  Intel bits 16-23   seeded at 85    (0x55)
//!   BRAKE_ACTIVE u1  Intel bit  24      seeded at 1

use connector_canbus::{
    encode_can_signal, extract_can_signal, load_dbc, resolve_signal, CanByteOrder, ResolvedSignal,
    SignalValueType,
};
use std::io::Write;
use tedge_dot_sdk::{Connector, ConnectorConfig};
use tempfile::NamedTempFile;

// Embed the simulator's DBC at compile time so the integration test stays
// in sync with the simulator even without file I/O.
const TEST_DBC: &str =
    include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../connectors/canbus/sim/test.dbc"));

// ─── Helpers ──────────────────────────────────────────────────────────────────

/// Frame bytes matching the simulator's seeded values:
///   RPM=2500, COOLANT_TEMP=85, BRAKE_ACTIVE=1
fn seeded_frame() -> [u8; 8] {
    let mut data = [0u8; 8];
    data[0] = (2500u16 & 0xFF) as u8; // RPM low byte
    data[1] = (2500u16 >> 8) as u8;   // RPM high byte
    data[2] = 85u8;                    // COOLANT_TEMP (positive, no sign extension)
    data[3] = 0x01;                    // BRAKE_ACTIVE bit 0 = 1
    data
}

/// Write the embedded DBC to a temporary file and return `(NamedTempFile, path)`.
fn dbc_tempfile() -> (NamedTempFile, std::path::PathBuf) {
    let mut f = NamedTempFile::new().unwrap();
    f.write_all(TEST_DBC.as_bytes()).unwrap();
    let path = f.path().to_path_buf();
    (f, path)
}

// ─── DBC loading ─────────────────────────────────────────────────────────────

#[test]
fn load_test_dbc_succeeds() {
    let (_f, path) = dbc_tempfile();
    load_dbc(&path).expect("test.dbc should parse without errors");
}

#[test]
fn resolve_rpm_signal() {
    let (_f, path) = dbc_tempfile();
    let dbc = load_dbc(&path).unwrap();
    let sig = resolve_signal(&dbc, "ENGINE_STATUS", "RPM").unwrap();
    assert_eq!(sig.can_id, 416, "CAN ID should be 416 (0x1A0)");
    assert_eq!(sig.start_bit, 0);
    assert_eq!(sig.bit_count, 16);
    assert_eq!(sig.byte_order, CanByteOrder::Intel);
    assert_eq!(sig.value_type, SignalValueType::Unsigned);
}

#[test]
fn resolve_coolant_temp_signal() {
    let (_f, path) = dbc_tempfile();
    let dbc = load_dbc(&path).unwrap();
    let sig = resolve_signal(&dbc, "ENGINE_STATUS", "COOLANT_TEMP").unwrap();
    assert_eq!(sig.can_id, 416);
    assert_eq!(sig.start_bit, 16);
    assert_eq!(sig.bit_count, 8);
    assert_eq!(sig.byte_order, CanByteOrder::Intel);
    assert_eq!(sig.value_type, SignalValueType::Signed);
}

#[test]
fn resolve_brake_active_signal() {
    let (_f, path) = dbc_tempfile();
    let dbc = load_dbc(&path).unwrap();
    let sig = resolve_signal(&dbc, "ENGINE_STATUS", "BRAKE_ACTIVE").unwrap();
    assert_eq!(sig.can_id, 416);
    assert_eq!(sig.start_bit, 24);
    assert_eq!(sig.bit_count, 1);
    assert_eq!(sig.byte_order, CanByteOrder::Intel);
    assert_eq!(sig.value_type, SignalValueType::Unsigned);
}

#[test]
fn resolve_unknown_message_errors() {
    let (_f, path) = dbc_tempfile();
    let dbc = load_dbc(&path).unwrap();
    let err = resolve_signal(&dbc, "NO_SUCH_MSG", "RPM").unwrap_err();
    assert!(err.contains("NO_SUCH_MSG"), "error should name the missing message");
}

#[test]
fn resolve_unknown_signal_errors() {
    let (_f, path) = dbc_tempfile();
    let dbc = load_dbc(&path).unwrap();
    let err = resolve_signal(&dbc, "ENGINE_STATUS", "NO_SUCH_SIGNAL").unwrap_err();
    assert!(err.contains("NO_SUCH_SIGNAL"), "error should name the missing signal");
}

// ─── Signal extraction from seeded frame bytes ───────────────────────────────

#[test]
fn extract_rpm_from_seeded_frame() {
    let (_f, path) = dbc_tempfile();
    let dbc = load_dbc(&path).unwrap();
    let sig = resolve_signal(&dbc, "ENGINE_STATUS", "RPM").unwrap();
    let frame = seeded_frame();
    let raw = extract_can_signal(&frame, &sig);
    assert_eq!(raw, 2500, "RPM should decode to 2500");
}

#[test]
fn extract_coolant_temp_from_seeded_frame() {
    let (_f, path) = dbc_tempfile();
    let dbc = load_dbc(&path).unwrap();
    let sig = resolve_signal(&dbc, "ENGINE_STATUS", "COOLANT_TEMP").unwrap();
    let frame = seeded_frame();
    let raw = extract_can_signal(&frame, &sig);
    // Raw unsigned value; sign extension is applied separately by decode_signal.
    assert_eq!(raw, 85, "COOLANT_TEMP raw bits should be 85");
}

#[test]
fn extract_brake_active_from_seeded_frame() {
    let (_f, path) = dbc_tempfile();
    let dbc = load_dbc(&path).unwrap();
    let sig = resolve_signal(&dbc, "ENGINE_STATUS", "BRAKE_ACTIVE").unwrap();
    let frame = seeded_frame();
    let raw = extract_can_signal(&frame, &sig);
    assert_eq!(raw, 1, "BRAKE_ACTIVE should decode to 1");
}

// ─── Encode → extract roundtrip ──────────────────────────────────────────────

fn make_sig(can_id: u32, start_bit: u16, bit_count: u8, byte_order: CanByteOrder) -> ResolvedSignal {
    ResolvedSignal { can_id, start_bit, bit_count, byte_order, value_type: SignalValueType::Unsigned }
}

#[test]
fn roundtrip_rpm_intel_u16() {
    let sig = make_sig(416, 0, 16, CanByteOrder::Intel);
    let mut payload = [0u8; connector_canbus::CLASSIC_PAYLOAD_LEN];
    encode_can_signal(&mut payload, &sig, 2500);
    assert_eq!(extract_can_signal(&payload, &sig), 2500);
}

#[test]
fn roundtrip_brake_active_intel_u1() {
    let sig = make_sig(416, 24, 1, CanByteOrder::Intel);
    let mut payload = [0u8; connector_canbus::CLASSIC_PAYLOAD_LEN];
    encode_can_signal(&mut payload, &sig, 1);
    assert_eq!(extract_can_signal(&payload, &sig), 1);
    encode_can_signal(&mut payload, &sig, 0);
    assert_eq!(extract_can_signal(&payload, &sig), 0);
}

#[test]
fn roundtrip_motorola_u8() {
    let sig = make_sig(0, 7, 8, CanByteOrder::Motorola);
    let mut payload = [0u8; connector_canbus::CLASSIC_PAYLOAD_LEN];
    encode_can_signal(&mut payload, &sig, 0xAB);
    assert_eq!(extract_can_signal(&payload, &sig), 0xAB);
}

#[test]
fn roundtrip_motorola_u10_span() {
    let sig = make_sig(0, 15, 10, CanByteOrder::Motorola);
    let mut payload = [0u8; connector_canbus::CLASSIC_PAYLOAD_LEN];
    encode_can_signal(&mut payload, &sig, 235);
    assert_eq!(extract_can_signal(&payload, &sig), 235);
}

// ─── Connector configure() + capabilities() ──────────────────────────────────

fn make_config(dbc_path: &std::path::Path) -> ConnectorConfig {
    let toml = format!(
        r#"
[connector]
protocol = "canbus"

[[device]]
name = "engine"
protocol_address = {{ interface = "vcan0", dbc_file = "{}" }}
default_mode = "typed"

  [[device.point]]
  id = "rpm"
  datatype = "uint16"
  access = "read"
  address = {{ message_name = "ENGINE_STATUS", signal_name = "RPM" }}

  [[device.point]]
  id = "brake_active"
  datatype = "bool"
  access = "read_write"
  address = {{ message_name = "ENGINE_STATUS", signal_name = "BRAKE_ACTIVE" }}
"#,
        dbc_path.display()
    );
    toml::from_str(&toml).expect("config TOML is valid")
}

#[test]
fn configure_succeeds_with_valid_dbc_and_points() {
    let (_f, path) = dbc_tempfile();
    let cfg = make_config(&path);
    let mut connector = connector_canbus::CanbusConnector::default();
    connector.configure(&cfg).expect("configure should succeed");
}

#[test]
fn capabilities_protocol_is_canbus() {
    let caps = connector_canbus::CanbusConnector::default().capabilities();
    assert_eq!(caps.protocol, "canbus");
}

#[test]
fn capabilities_includes_subscribe_feature() {
    let caps = connector_canbus::CanbusConnector::default().capabilities();
    assert!(caps.features.iter().any(|f| f == "subscribe"));
}

#[test]
fn capabilities_includes_write_verb() {
    let caps = connector_canbus::CanbusConnector::default().capabilities();
    assert!(caps.command_verbs.iter().any(|v| v == "write"));
}

#[test]
fn configure_fails_for_unknown_signal() {
    let (_f, path) = dbc_tempfile();
    let toml = format!(
        r#"
[connector]
protocol = "canbus"

[[device]]
name = "engine"
protocol_address = {{ interface = "vcan0", dbc_file = "{}" }}

  [[device.point]]
  id = "bad"
  datatype = "uint16"
  address = {{ message_name = "ENGINE_STATUS", signal_name = "DOES_NOT_EXIST" }}
"#,
        path.display()
    );
    let cfg: ConnectorConfig = toml::from_str(&toml).unwrap();
    let mut connector = connector_canbus::CanbusConnector::default();
    assert!(connector.configure(&cfg).is_err(), "should fail for unknown signal");
}
