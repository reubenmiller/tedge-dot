//! Integration tests for the CANopen connector.
//!
//! These tests exercise the full configure → capabilities → error path pipeline
//! without requiring a CAN socket. They run on all platforms including macOS CI.

use connector_canopen::{CanopenConnection, CanopenConnector, NodeAddress, OdAddress};
use tedge_dot_sdk::{Connector, ConnectorConfig};

// ─── Config helpers ───────────────────────────────────────────────────────────

fn minimal_config(node_id: u8) -> ConnectorConfig {
    let toml = format!(
        r#"
[connector]
protocol = "canopen"
service_name = "tedge-dot"

[connection]
interface = "vcan0"

[[device]]
name = "plc1"
protocol_address = {{ node_id = {node_id} }}
default_mode = "typed"

  [[device.point]]
  id = "analog_in"
  datatype = "uint16"
  access = "read"
  address = {{ index = 0x2000, subindex = 0 }}

  [[device.point]]
  id = "temperature"
  datatype = "int16"
  access = "read"
  address = {{ index = 0x2001, subindex = 0 }}

  [[device.point]]
  id = "digital_out"
  datatype = "uint8"
  access = "read_write"
  address = {{ index = 0x2002, subindex = 0 }}
"#,
        node_id = node_id
    );
    toml::from_str(&toml).expect("valid test config")
}

fn make_connector() -> CanopenConnector {
    CanopenConnector::default()
}

// ─── Config struct tests ──────────────────────────────────────────────────────

#[test]
fn deserialize_canopen_connection() {
    let v: CanopenConnection = serde_json::from_str(r#"{"interface":"vcan0"}"#).unwrap();
    assert_eq!(v.interface, "vcan0");
}

#[test]
fn deserialize_node_address() {
    let v: NodeAddress = serde_json::from_str(r#"{"node_id":42}"#).unwrap();
    assert_eq!(v.node_id, 42);
}

#[test]
fn node_address_zero_invalid() {
    let v = NodeAddress { node_id: 0 };
    assert!(v.validate().is_err());
}

#[test]
fn node_address_128_invalid() {
    let v = NodeAddress { node_id: 128 };
    assert!(v.validate().is_err());
}

#[test]
fn node_address_127_valid() {
    let v = NodeAddress { node_id: 127 };
    assert!(v.validate().is_ok());
}

#[test]
fn deserialize_od_address() {
    let v: OdAddress = serde_json::from_str(r#"{"index":8192,"subindex":1}"#).unwrap();
    assert_eq!(v.index, 0x2000);
    assert_eq!(v.subindex, 1);
}

#[test]
fn od_address_subindex_defaults_to_zero() {
    let v: OdAddress = serde_json::from_str(r#"{"index":8192}"#).unwrap();
    assert_eq!(v.subindex, 0);
}

// ─── configure() tests ───────────────────────────────────────────────────────

#[test]
fn configure_succeeds_with_valid_config() {
    let mut c = make_connector();
    let cfg = minimal_config(1);
    assert!(c.configure(&cfg).is_ok());
}

#[test]
fn configure_rejects_node_id_zero() {
    let mut c = make_connector();
    let cfg = minimal_config(0);
    let err = c.configure(&cfg).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("node_id") || msg.contains("out of range"), "unexpected: {msg}");
}

#[test]
fn configure_rejects_node_id_128() {
    let mut c = make_connector();
    let cfg = minimal_config(128);
    let err = c.configure(&cfg).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("node_id") || msg.contains("out of range"), "unexpected: {msg}");
}

#[test]
fn configure_rejects_missing_connection_interface() {
    let mut c = make_connector();
    let toml = r#"
[connector]
protocol = "canopen"
service_name = "tedge-dot"

[[device]]
name = "plc1"
protocol_address = { node_id = 1 }
"#;
    // connection block is missing — default is `{}` which lacks `interface`
    let cfg: ConnectorConfig = toml::from_str(toml).unwrap();
    assert!(c.configure(&cfg).is_err());
}

#[test]
fn configure_rejects_typed_point_without_datatype() {
    let toml = r#"
[connector]
protocol = "canopen"

[connection]
interface = "vcan0"

[[device]]
name = "plc1"
protocol_address = { node_id = 1 }
default_mode = "typed"

  [[device.point]]
  id = "mystery"
  access = "read"
  address = { index = 0x2000, subindex = 0 }
"#;
    let cfg: ConnectorConfig = toml::from_str(toml).unwrap();
    let mut c = make_connector();
    assert!(c.configure(&cfg).is_err());
}

#[test]
fn configure_accepts_raw_point_without_datatype() {
    let toml = r#"
[connector]
protocol = "canopen"

[connection]
interface = "vcan0"

[[device]]
name = "plc1"
protocol_address = { node_id = 1 }
default_mode = "raw"

  [[device.point]]
  id = "raw_obj"
  access = "read"
  address = { index = 0x2000, subindex = 0 }
"#;
    let cfg: ConnectorConfig = toml::from_str(toml).unwrap();
    let mut c = make_connector();
    assert!(c.configure(&cfg).is_ok());
}

// ─── capabilities() tests ────────────────────────────────────────────────────

#[test]
fn capabilities_protocol_is_canopen() {
    let c = make_connector();
    assert_eq!(c.capabilities().protocol, "canopen");
}

#[test]
fn capabilities_subscribe_is_false() {
    let c = make_connector();
    assert!(!c.capabilities().subscribe);
}

#[test]
fn capabilities_includes_write_verb() {
    let c = make_connector();
    assert!(c.capabilities().command_verbs.contains(&"write".to_string()));
}

#[test]
fn capabilities_includes_common_datatypes() {
    let c = make_connector();
    let caps = c.capabilities();
    use tedge_dot_sdk::DataType;
    for dt in [DataType::Bool, DataType::Uint16, DataType::Int16, DataType::Float32] {
        assert!(caps.datatypes.contains(&dt), "missing datatype {dt:?}");
    }
}

#[test]
fn capabilities_to_json_is_valid() {
    let c = make_connector();
    let json = c.capabilities().to_json();
    assert_eq!(json["protocol"], "canopen");
    assert_eq!(json["subscribe"], false);
    assert!(json["command_verbs"].as_array().unwrap().contains(&serde_json::json!("write")));
}

// ─── configure idempotency ────────────────────────────────────────────────────

#[test]
fn reconfigure_replaces_previous_state() {
    let mut c = make_connector();
    // First configure with node 1
    c.configure(&minimal_config(1)).unwrap();
    // Re-configure with node 5 — should succeed cleanly
    c.configure(&minimal_config(5)).unwrap();
    // Verify capabilities are still correct after reconfigure
    assert_eq!(c.capabilities().protocol, "canopen");
}
