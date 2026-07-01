//! Integration tests for the PROFIBUS-DP connector.
//!
//! These tests exercise the connector's `configure()`, `read_points()`, and
//! `execute()` logic without requiring real hardware or a serial port.
//! They build a connector in a "not connected" state (no bus thread) and verify:
//!   * Config validation (valid + error cases)
//!   * `read_points()` bad-quality behaviour when the bus is down
//!   * `execute()` error handling
//!   * `decode_point` byte/bit extraction logic via the connector's internal path
//!
//! Full end-to-end tests (with a real or simulated PROFIBUS slave) live in
//! `connectors/profibus/tests/`.

use connector_profibus::ProfibusConnector;
use tedge_dot_sdk::{
    Access, Connector, ConnectorConfig, DataType, Endianness, Mode, PointRef, Quality, Transform,
    WordOrder,
};

// ── helpers ───────────────────────────────────────────────────────────────────

/// Minimal TOML connector config string for tests.
fn cfg_toml(extra_device: &str) -> String {
    format!(
        r#"
[connector]
protocol = "profibus"
service_name = "test"
poll_interval = "1s"

[mqtt]
host = "127.0.0.1"
port = 1883

[connection]
port = "/dev/null"
baudrate = 19200
master_address = 2
slot_bits = 300

{extra_device}
"#
    )
}

fn single_device_config() -> ConnectorConfig {
    let toml = cfg_toml(
        r#"
[[device]]
name = "remote_io"
protocol_address = { station_address = 7, ident_number = 0, input_bytes = 8, output_bytes = 4 }

  [[device.point]]
  id        = "di_byte0"
  datatype  = "uint8"
  access    = "read"
  address   = { direction = "input", byte_offset = 0 }

  [[device.point]]
  id        = "di_bit3"
  datatype  = "bool"
  access    = "read"
  address   = { direction = "input", byte_offset = 1, bit_offset = 3, bit_count = 1 }

  [[device.point]]
  id        = "ai_word"
  datatype  = "uint16"
  access    = "read"
  address   = { direction = "input", byte_offset = 2 }

  [[device.point]]
  id        = "do_byte0"
  datatype  = "uint8"
  access    = "write"
  address   = { direction = "output", byte_offset = 0 }
"#,
    );
    toml::from_str(&toml).expect("config parse failed")
}

fn make_connector() -> ProfibusConnector {
    let mut c = ProfibusConnector::default();
    c.configure(&single_device_config()).expect("configure failed");
    c
}

fn point_ref(id: &str, dt: DataType) -> PointRef {
    PointRef {
        id: id.to_string(),
        mode: Mode::Typed,
        datatype: Some(dt),
        endianness: Endianness::Big,
        word_order: WordOrder::Big,
        access: Access::Read,
        unit: None,
        transform: Transform::default(),
        interval: None,
    }
}

// ── configure() tests ─────────────────────────────────────────────────────────

#[test]
fn configure_valid() {
    let mut c = ProfibusConnector::default();
    c.configure(&single_device_config()).unwrap();
}

#[test]
fn configure_rejects_invalid_station_address() {
    let toml = cfg_toml(
        r#"
[[device]]
name = "oob"
protocol_address = { station_address = 200, input_bytes = 2, output_bytes = 0 }
"#,
    );
    let config: ConnectorConfig = toml::from_str(&toml).unwrap();
    let mut c = ProfibusConnector::default();
    assert!(c.configure(&config).is_err());
}

#[test]
fn configure_rejects_typed_point_without_datatype() {
    let toml = cfg_toml(
        r#"
[[device]]
name = "d"
protocol_address = { station_address = 5, input_bytes = 4, output_bytes = 0 }

  [[device.point]]
  id      = "no_dt"
  access  = "read"
  address = { direction = "input", byte_offset = 0 }
"#,
    );
    let config: ConnectorConfig = toml::from_str(&toml).unwrap();
    let mut c = ProfibusConnector::default();
    // default mode is typed; no datatype and no bit_offset → error
    assert!(c.configure(&config).is_err());
}

#[test]
fn configure_rejects_writable_input_point() {
    let toml = cfg_toml(
        r#"
[[device]]
name = "d"
protocol_address = { station_address = 5, input_bytes = 4, output_bytes = 0 }

  [[device.point]]
  id       = "conflict"
  datatype = "uint8"
  access   = "read_write"
  address  = { direction = "input", byte_offset = 0 }
"#,
    );
    let config: ConnectorConfig = toml::from_str(&toml).unwrap();
    let mut c = ProfibusConnector::default();
    assert!(c.configure(&config).is_err());
}

// ── capabilities() tests ─────────────────────────────────────────────────────

#[test]
fn capabilities_protocol_name() {
    let c = make_connector();
    assert_eq!(c.capabilities().protocol, "profibus");
}

#[test]
fn capabilities_includes_write_verb() {
    let c = make_connector();
    let caps = c.capabilities();
    assert!(caps.command_verbs.iter().any(|v| v == "write"));
}

// ── read_points() when not connected ─────────────────────────────────────────

#[tokio::test]
async fn read_returns_bad_quality_when_not_connected() {
    let mut c = make_connector();
    let refs = vec![point_ref("di_byte0", DataType::Uint8)];
    let samples = c.read_points(&"remote_io".to_string(), &refs).await.unwrap();
    assert_eq!(samples.len(), 1);
    assert_eq!(samples[0].quality, Quality::Bad);
    assert!(samples[0].error.is_some());
}

#[tokio::test]
async fn read_returns_bad_for_unknown_device() {
    let mut c = make_connector();
    let refs = vec![point_ref("di_byte0", DataType::Uint8)];
    let samples = c
        .read_points(&"no_such_device".to_string(), &refs)
        .await
        .unwrap();
    assert_eq!(samples[0].quality, Quality::Bad);
}

#[tokio::test]
async fn read_returns_bad_for_unknown_point() {
    let mut c = make_connector();
    let refs = vec![point_ref("does_not_exist", DataType::Uint8)];
    let samples = c.read_points(&"remote_io".to_string(), &refs).await.unwrap();
    assert_eq!(samples[0].quality, Quality::Bad);
}

#[tokio::test]
async fn read_returns_bad_for_output_point() {
    let mut c = make_connector();
    let refs = vec![PointRef {
        id: "do_byte0".to_string(),
        mode: Mode::Typed,
        datatype: Some(DataType::Uint8),
        endianness: Endianness::Big,
        word_order: WordOrder::Big,
        access: Access::Write,
        unit: None,
        transform: Transform::default(),
        interval: None,
    }];
    let samples = c.read_points(&"remote_io".to_string(), &refs).await.unwrap();
    assert_eq!(samples[0].quality, Quality::Bad);
    assert!(
        samples[0]
            .error
            .as_deref()
            .unwrap_or("")
            .contains("output"),
        "error should mention 'output'"
    );
}

// ── execute() error handling ──────────────────────────────────────────────────

#[tokio::test]
async fn execute_rejects_unknown_verb() {
    let mut c = make_connector();
    let req = tedge_dot_sdk::CommandRequest {
        point: "do_byte0".to_string(),
        value: Some(serde_json::json!(42)),
        value_repr: None,
        raw: None,
    };
    let result = c.execute(&"remote_io".to_string(), "unsupported", &req).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn execute_rejects_unknown_point() {
    let mut c = make_connector();
    let req = tedge_dot_sdk::CommandRequest {
        point: "phantom".to_string(),
        value: Some(serde_json::json!(1)),
        value_repr: None,
        raw: None,
    };
    let result = c.execute(&"remote_io".to_string(), "write", &req).await;
    assert!(matches!(result, Err(tedge_dot_sdk::ConnectorError::UnknownPoint { .. })));
}

#[tokio::test]
async fn execute_rejects_read_only_point() {
    let mut c = make_connector();
    let req = tedge_dot_sdk::CommandRequest {
        point: "di_byte0".to_string(),
        value: Some(serde_json::json!(1)),
        value_repr: None,
        raw: None,
    };
    let result = c.execute(&"remote_io".to_string(), "write", &req).await;
    assert!(matches!(result, Err(tedge_dot_sdk::ConnectorError::AccessDenied(_))));
}

#[tokio::test]
async fn execute_returns_not_connected_when_no_bus() {
    let mut c = make_connector();
    let req = tedge_dot_sdk::CommandRequest {
        point: "do_byte0".to_string(),
        value: Some(serde_json::json!(0xFF)),
        value_repr: None,
        raw: None,
    };
    let result = c.execute(&"remote_io".to_string(), "write", &req).await;
    assert!(matches!(result, Err(tedge_dot_sdk::ConnectorError::NotConnected(_))));
}

// ── decode/encode via shared-state manipulation ───────────────────────────────
//
// We bypass the bus thread by injecting a fake SharedBusState directly.
// This tests the decode/encode logic in isolation.

use connector_profibus::__test_helpers::*;

#[test]
fn decode_uint8_from_buffer() {
    // buffer[0] = 0x42 → uint8 = 66
    let buf = vec![0x42u8, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
    let sample = decode_test("di_byte0", DataType::Uint8, 0, None, None, &buf);
    assert_eq!(sample.quality, Quality::Good);
    assert_eq!(sample.value, Some(tedge_dot_sdk::Value::Number(66.0)));
}

#[test]
fn decode_uint16_big_endian() {
    // buffer[2..4] = 0x01, 0x00 → uint16 BE = 256
    let mut buf = vec![0u8; 8];
    buf[2] = 0x01;
    buf[3] = 0x00;
    let sample = decode_test("ai_word", DataType::Uint16, 2, None, None, &buf);
    assert_eq!(sample.quality, Quality::Good);
    assert_eq!(sample.value, Some(tedge_dot_sdk::Value::Number(256.0)));
}

#[test]
fn decode_bit_false() {
    // buffer[1] = 0b0000_0000 → bit 3 = 0 → false
    let buf = vec![0u8; 8];
    let sample = decode_test("di_bit3", DataType::Bool, 1, Some(3), Some(1), &buf);
    assert_eq!(sample.quality, Quality::Good);
    assert_eq!(sample.value, Some(tedge_dot_sdk::Value::Bool(false)));
}

#[test]
fn decode_bit_true() {
    // buffer[1] = 0b0000_1000 → bit 3 = 1 → true
    let mut buf = vec![0u8; 8];
    buf[1] = 0b0000_1000;
    let sample = decode_test("di_bit3", DataType::Bool, 1, Some(3), Some(1), &buf);
    assert_eq!(sample.quality, Quality::Good);
    assert_eq!(sample.value, Some(tedge_dot_sdk::Value::Bool(true)));
}

#[test]
fn decode_out_of_bounds_returns_bad() {
    let buf = vec![0u8; 1]; // only 1 byte, uint16 needs 2
    let sample = decode_test("ai_word", DataType::Uint16, 0, None, None, &buf);
    assert_eq!(sample.quality, Quality::Bad);
}

#[test]
fn encode_uint8_writes_byte() {
    let mut buf = vec![0u8; 4];
    encode_test(DataType::Uint8, 0, None, None, serde_json::json!(0xAB), &mut buf);
    assert_eq!(buf[0], 0xAB);
}

#[test]
fn encode_bit_sets_correct_bit() {
    let mut buf = vec![0b0000_0000u8; 4];
    encode_test(DataType::Bool, 0, Some(2), Some(1), serde_json::json!(true), &mut buf);
    assert_eq!(buf[0], 0b0000_0100, "bit 2 should be set");
}

#[test]
fn encode_bit_clears_bit() {
    let mut buf = vec![0b1111_1111u8; 4];
    encode_test(DataType::Bool, 0, Some(5), Some(1), serde_json::json!(false), &mut buf);
    assert_eq!(buf[0], 0b1101_1111, "bit 5 should be cleared");
}

#[test]
fn encode_hex_raw() {
    let mut buf = vec![0u8; 4];
    encode_hex_test(0, "DEAD", &mut buf);
    assert_eq!(&buf[0..2], &[0xDE, 0xAD]);
}
