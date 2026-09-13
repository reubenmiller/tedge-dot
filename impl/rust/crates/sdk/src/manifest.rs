//! The retained per-device **manifest** (contract §8.2, RFC 0006 §3): one message per device
//! that tells a consumer what the device is and what its points are — datatype, access, unit,
//! labels, free-form `meta`, and the parameter sets each point belongs to, already resolved —
//! so that a flow never needs the configuration file and a sample never has to repeat any of it.
//!
//! Published when the device is loaded (before its link status and before any sample), again
//! whenever the configuration changes, and cleared when the device is removed or switched off.
//! The topic sits below the connector's `ot/<protocol>/` prefix as a channel of its own: a
//! topic of exactly four segments after `te/` is an entity *registration* topic to thin-edge,
//! and the c8y mapper parses everything it finds there.

use crate::config::{ConnectorConfig, DeviceConfig};
use crate::connector::Access;
use crate::descriptor::{parameters_of, SetNaming};
use serde_json::{json, Map, Value};

/// The contract version a manifest declares.
pub const CONTRACT_VERSION: &str = "0.2";

/// The retained manifest topic of one device.
pub fn topic(device: &str, protocol: &str) -> String {
    format!("te/device/{device}/ot/{protocol}/manifest")
}

/// The manifest of one configured device. `info` is the device descriptor the module reported
/// from `connect` (transport and address details), when it has been seen.
pub fn device_manifest(
    config: &ConnectorConfig,
    device: &DeviceConfig,
    info: Option<&Value>,
) -> Value {
    let protocol = config.connector.protocol.as_str();
    let naming = SetNaming::of(device, protocol, None);
    let mut points = Map::new();
    for point in &device.points {
        let mut entry = Map::new();
        if let Some(datatype) = point.datatype {
            entry.insert("datatype".into(), serde_json::to_value(datatype).unwrap());
        }
        entry.insert(
            "access".into(),
            Value::String(Access::parse(point.access.as_deref()).as_str().into()),
        );
        if let Some(unit) = &point.unit {
            entry.insert("unit".into(), Value::String(unit.clone()));
        }
        if let Some(name) = &point.name {
            entry.insert("name".into(), Value::String(name.clone()));
        }
        if let Some(description) = &point.description {
            entry.insert("description".into(), Value::String(description.clone()));
        }
        if let Some(meta) = &point.meta {
            entry.insert("meta".into(), meta.clone());
        }
        // Resolved once, here: the RFC 0005 naming rule used to be applied by the SDK, the C
        // SDK and the parameter-state flow alike, byte for byte. Now the flow reads the result.
        let mut sets: Vec<String> = Vec::new();
        for parameter in parameters_of(point, &naming) {
            if !sets.contains(&parameter.set) {
                sets.push(parameter.set);
            }
        }
        if !sets.is_empty() {
            entry.insert("parameter".into(), json!({ "sets": sets }));
        }
        points.insert(point.id.clone(), Value::Object(entry));
    }

    let mut manifest = Map::new();
    manifest.insert("contract".into(), Value::String(CONTRACT_VERSION.into()));
    manifest.insert("protocol".into(), Value::String(protocol.into()));
    manifest.insert(
        "service".into(),
        Value::String(config.connector.service_name()),
    );
    if let Some(device_type) = device.device_type.as_deref().filter(|t| !t.is_empty()) {
        manifest.insert("type".into(), Value::String(device_type.into()));
    }
    if let Some(info) = info {
        manifest.insert("info".into(), info.clone());
    }
    manifest.insert("points".into(), Value::Object(points));
    Value::Object(manifest)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> ConnectorConfig {
        toml::from_str(
            r#"
[connector]
protocol = "modbus"

[[device]]
name = "plc1"
type = "acme-meter-v2"
protocol_address = { host = "127.0.0.1" }

  [[device.point]]
  id = "setpoint"
  datatype = "uint16"
  access = "read_write"
  name = "Temperature setpoint"
  description = "Target temperature"
  address = { table = "holding", address = 3, count = 1 }
  meta = { parameter = { group = ["control", "commissioning"], min = 0, max = 30000 } }

  [[device.point]]
  id = "temp"
  datatype = "float32"
  unit = "°C"
  address = { table = "holding", address = 7, count = 2 }
  meta = { on_change = true, deadband = 0.5 }

  [[device.point]]
  id = "hidden_rw"
  datatype = "bool"
  access = "read_write"
  address = { table = "coil", address = 0, count = 1 }
  meta = { parameter = false }

  [[device.point]]
  id = "raw_only"
  mode = "raw"
  address = { table = "holding", address = 9, count = 1 }
"#,
        )
        .unwrap()
    }

    #[test]
    fn topic_sits_below_the_protocol_prefix() {
        // Exactly four segments after `te/` would be a thin-edge registration topic.
        assert_eq!(topic("plc1", "modbus"), "te/device/plc1/ot/modbus/manifest");
    }

    #[test]
    fn manifest_describes_the_device_and_resolves_the_parameter_sets() {
        let cfg = config();
        let manifest = device_manifest(&cfg, &cfg.devices[0], None);
        assert_eq!(manifest["contract"], json!("0.2"));
        assert_eq!(manifest["protocol"], json!("modbus"));
        assert_eq!(manifest["service"], json!("tedge-dot-modbus"));
        assert_eq!(manifest["type"], json!("acme-meter-v2"));
        assert!(manifest.get("info").is_none());

        let points = manifest["points"].as_object().unwrap();
        assert_eq!(points.len(), 4, "every point, keyed by id");

        let setpoint = &points["setpoint"];
        assert_eq!(setpoint["datatype"], json!("uint16"));
        assert_eq!(setpoint["access"], json!("read_write"));
        assert_eq!(setpoint["name"], json!("Temperature setpoint"));
        assert_eq!(setpoint["description"], json!("Target temperature"));
        assert_eq!(setpoint["meta"]["parameter"]["min"], json!(0), "meta is verbatim");
        assert_eq!(
            setpoint["parameter"]["sets"],
            json!(["acme_meter_v2_control_parameters", "acme_meter_v2_commissioning_parameters"]),
            "the RFC 0005 rule applied once, here"
        );

        let temp = &points["temp"];
        assert_eq!(temp["access"], json!("read"));
        assert_eq!(temp["unit"], json!("°C"));
        assert_eq!(temp["meta"], json!({ "on_change": true, "deadband": 0.5 }));
        assert!(temp.get("parameter").is_none(), "a read-only point is no parameter");
        assert!(temp.get("name").is_none(), "only declared fields appear");

        assert!(
            points["hidden_rw"].get("parameter").is_none(),
            "meta.parameter = false opts a writable point out"
        );
        assert!(points["raw_only"].get("datatype").is_none());
    }

    #[test]
    fn manifest_carries_the_descriptor_once_known_and_falls_back_to_the_protocol_set() {
        let mut cfg = config();
        cfg.devices[0].device_type = None;
        let info = json!({ "transport": "tcp", "host": "10.0.0.9", "port": 502 });
        let manifest = device_manifest(&cfg, &cfg.devices[0], Some(&info));
        assert_eq!(manifest["info"], info);
        assert!(manifest.get("type").is_none());
        assert_eq!(
            manifest["points"]["setpoint"]["parameter"]["sets"],
            json!(["modbus_control_parameters", "modbus_commissioning_parameters"])
        );
    }
}
