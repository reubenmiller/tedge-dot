//! Cumulocity **Digital Twin Manager** property definitions, rendered from device manifests.
//!
//! One definition per parameter set: the request body of
//! `POST /service/dtm/definitions/properties`, which a tenant admin registers once to make the
//! set editable in the device's "Parameters" tab. The device itself never talks to the DTM
//! service — this is a rendering of what it publishes, produced by a CLI on the side.
//!
//! A set identifier is **tenant-wide**, so a set that several devices (or several connector
//! configurations) share is ONE definition: the keys merge, the first definition of a key wins,
//! and the definition is tagged with the protocol of the manifest that declared the set first.

use super::DeviceManifest;
use serde_json::{json, Map, Value};

/// Render the definitions of every parameter set found in `manifests`, in the order the sets
/// first appear.
pub fn definitions(manifests: &[DeviceManifest]) -> Vec<Value> {
    // ordered (key, property schema)
    type Properties = Vec<(String, Value)>;
    // set -> (protocol of the manifest declaring it first, its properties)
    let mut sets: Vec<(String, String, Properties)> = Vec::new();
    for entry in manifests {
        let protocol = entry.manifest["protocol"].as_str().unwrap_or_default();
        let Some(points) = entry.manifest["points"].as_object() else {
            continue;
        };
        for (id, point) in points {
            // A point with no `parameter` is not one: the connector already applied the
            // access rule and the explicit opt-in/opt-out (§5.2), so there is nothing to
            // decide here.
            let Some(parameter) = point.get("parameter").and_then(Value::as_object) else {
                continue;
            };
            for set in parameter
                .get("sets")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
            {
                let index = match sets.iter().position(|(name, _, _)| name == set) {
                    Some(index) => index,
                    None => {
                        sets.push((set.to_string(), protocol.to_string(), Vec::new()));
                        sets.len() - 1
                    }
                };
                let props = &mut sets[index].2;
                if props.iter().any(|(k, _)| k == id) {
                    continue; // the same key on another device: the first definition wins
                }
                props.push((id.clone(), property_schema(id, point, parameter)));
            }
        }
    }
    sets.into_iter()
        .map(|(set, protocol, props)| {
            let mut properties = Map::new();
            for (i, (key, mut schema)) in props.into_iter().enumerate() {
                // A point that declares no `parameter.order` is ordered by its position in the
                // set, so the form is at least stable rather than arbitrary.
                if !schema
                    .as_object()
                    .map(|o| o.contains_key("order"))
                    .unwrap_or(false)
                {
                    schema["order"] = json!(i + 1);
                }
                properties.insert(key, schema);
            }
            json!({
                "identifier": set,
                "jsonSchema": {
                    "$schema": "http://json-schema.org/draft-07/schema#",
                    "title": title_from_key(&set),
                    "description": format!(
                        "Writable {protocol} points exposed by tedge-dot (generated from the device manifest)",
                    ),
                    "type": "object",
                    "properties": properties,
                },
                "contexts": ["asset", "event", "operation"],
                "tags": ["tedge-dot", protocol],
            })
        })
        .collect()
}

/// The JSON-schema property for one parameter: the type and its natural limits from the
/// manifest's `datatype`, the bounds from its `range`, everything else from `parameter`.
fn property_schema(id: &str, point: &Value, parameter: &Map<String, Value>) -> Value {
    let mut schema = Map::new();
    let empty = Map::new();
    let point = point.as_object().unwrap_or(&empty);
    let (ty, min, max) = json_type(point.get("datatype").and_then(Value::as_str));
    schema.insert("type".into(), json!(ty));

    // `parameter.title` is what the point should be called *as a setting*; the point's own
    // `name` is the general-purpose label, and the id is the last resort.
    let title = string_of(parameter, "title")
        .or_else(|| string_of(point, "name"))
        .unwrap_or_else(|| id.to_string());
    schema.insert("title".into(), json!(title));

    let mut description = string_of(parameter, "description")
        .or_else(|| string_of(point, "description"))
        .unwrap_or_default();
    let mut append = |text: &str| {
        if !description.is_empty() {
            description.push(' ');
        }
        description.push_str(text);
    };
    if let Some(unit) = point.get("unit").and_then(Value::as_str) {
        append(&format!("[{unit}]"));
    }
    let access = point.get("access").and_then(Value::as_str).unwrap_or("read");
    if access == "write" {
        // A write-only point cannot be read back, so the form shows the last value written —
        // say so, rather than let an operator read it as the device's current state.
        append("(write-only: shows the last value written)");
    }
    if !description.is_empty() {
        schema.insert("description".into(), json!(description));
    }

    // The declared `range` (§5.3) narrows the datatype's own bounds. It is the same table the
    // connector enforces on write, so the form and the driver cannot disagree about the limit.
    let range = point.get("range").and_then(Value::as_object);
    let bound = |key: &str| range.and_then(|r| r.get(key)).and_then(Value::as_f64);
    if let Some(min) = bound("min").or(min) {
        schema.insert("minimum".into(), json!(min));
    }
    if let Some(max) = bound("max").or(max) {
        schema.insert("maximum".into(), json!(max));
    }
    for key in ["enum", "default", "order"] {
        if let Some(value) = parameter.get(key) {
            schema.insert(key.into(), value.clone());
        }
    }
    if access == "read" {
        // A read-only point that opted in with `parameter = true`: shown on the tab, not edited.
        schema.insert("readOnly".into(), json!(true));
    }
    Value::Object(schema)
}

/// The JSON-schema type for a contract datatype, with the limits the type itself implies.
fn json_type(datatype: Option<&str>) -> (&'static str, Option<f64>, Option<f64>) {
    match datatype {
        Some("bool") => ("boolean", None, None),
        Some("int8") => ("integer", Some(i8::MIN as f64), Some(i8::MAX as f64)),
        Some("uint8") => ("integer", Some(0.0), Some(u8::MAX as f64)),
        Some("int16") => ("integer", Some(i16::MIN as f64), Some(i16::MAX as f64)),
        Some("uint16") => ("integer", Some(0.0), Some(u16::MAX as f64)),
        Some("int32") => ("integer", Some(i32::MIN as f64), Some(i32::MAX as f64)),
        Some("uint32") => ("integer", Some(0.0), Some(u32::MAX as f64)),
        // 64-bit limits exceed the JS safe range; leave them unbounded.
        Some("int64") | Some("uint64") => ("integer", None, None),
        Some("float32") | Some("float64") => ("number", None, None),
        _ => ("string", None, None),
    }
}

fn string_of(table: &Map<String, Value>, key: &str) -> Option<String> {
    table.get(key).and_then(Value::as_str).map(String::from)
}

/// `acme_boiler_v2_control_parameters` -> `Acme boiler v2 control parameters`: the set's own
/// identifier read back as a heading, since that is the only name it has.
fn title_from_key(key: &str) -> String {
    let mut out = String::new();
    for (i, part) in key.split('_').filter(|p| !p.is_empty()).enumerate() {
        if i > 0 {
            out.push(' ');
        }
        let mut chars = part.chars();
        if let Some(first) = chars.next() {
            if i == 0 {
                out.extend(first.to_uppercase());
            } else {
                out.push(first);
            }
            out.push_str(chars.as_str());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tedge_dot_sdk::ConnectorConfig;

    const CONFIG: &str = r#"
[connector]
protocol = "modbus"

[[device]]
name = "plc1"
type = "acme-boiler-v2"
protocol_address = { transport = "tcp", host = "127.0.0.1", port = 502, unit_id = 1 }

  [[device.point]]
  id = "temp_u16"
  datatype = "uint16"
  access = "read_write"
  unit = "°C"
  name = "Boiler temp"
  description = "Outlet temperature after the heat exchanger"
  address = { table = "holding", address = 3, count = 1 }
  parameter = { title = "Temperature setpoint", order = 7 }
  range     = { min = 0, max = 100 }

  [[device.point]]
  id = "coil_rw"
  datatype = "bool"
  access = "read_write"
  name = "Pump enable"
  address = { table = "coil", address = 48, count = 1 }

  [[device.point]]
  id = "pump_speed"
  datatype = "float32"
  access = "write"
  address = { table = "holding", address = 10, count = 2 }
  parameter = "pump"

  [[device.point]]
  id = "level_f32"
  datatype = "float32"
  description = "Level in the buffer tank"
  address = { table = "holding", address = 6, count = 2 }

  [[device.point]]
  id = "status_word"
  datatype = "uint16"
  address = { table = "holding", address = 20, count = 1 }
  parameter = true

  [[device.point]]
  id = "hidden_rw"
  datatype = "uint16"
  access = "read_write"
  address = { table = "holding", address = 21, count = 1 }
  parameter = false

  [[device.point]]
  id = "flow_limit"
  datatype = "uint16"
  access = "read_write"
  address = { table = "holding", address = 23, count = 1 }
  parameter = { group = ["control", "commissioning"] }

# A device of an undeclared type: its sets fall back to the protocol.
[[device]]
name = "plc2"
protocol_address = { transport = "tcp", host = "127.0.0.1", port = 503, unit_id = 1 }

  [[device.point]]
  id = "spare_rw"
  datatype = "uint16"
  access = "read_write"
  address = { table = "holding", address = 30, count = 1 }
"#;

    /// The manifests a configuration would publish — the renderer's only input.
    fn manifests(toml_text: &str) -> Vec<DeviceManifest> {
        let config: ConnectorConfig = toml::from_str(toml_text).unwrap();
        config
            .devices
            .iter()
            .map(|device| DeviceManifest {
                device: device.name.clone(),
                manifest: tedge_dot_sdk::manifest::device_manifest(&config, device, None, &[]),
            })
            .collect()
    }

    #[test]
    fn definitions_group_by_set_and_render_a_schema_per_point() {
        let defs = definitions(&manifests(CONFIG));
        let ids: Vec<&str> = defs.iter().map(|d| d["identifier"].as_str().unwrap()).collect();
        assert_eq!(
            ids,
            [
                "acme_boiler_v2_control_parameters",
                "acme_boiler_v2_commissioning_parameters",
                "pump",
                "modbus_control_parameters",
            ]
        );
        // The two-group point is a property of BOTH its sets, so either screen can edit it.
        assert!(defs[0]["jsonSchema"]["properties"]["flow_limit"].is_object());
        assert!(defs[1]["jsonSchema"]["properties"]["flow_limit"].is_object());

        let main = &defs[0];
        assert_eq!(main["contexts"], json!(["asset", "event", "operation"]));
        assert_eq!(main["jsonSchema"]["title"], "Acme boiler v2 control parameters");
        assert_eq!(main["tags"], json!(["tedge-dot", "modbus"]));
        let props = &main["jsonSchema"]["properties"];

        assert_eq!(props["temp_u16"]["type"], "integer");
        // `parameter.title` wins over the point's own `name`...
        assert_eq!(props["temp_u16"]["title"], "Temperature setpoint");
        // ...while its `description` is used (there is no parameter.description) and still
        // composes with the unit.
        assert_eq!(
            props["temp_u16"]["description"],
            "Outlet temperature after the heat exchanger [°C]"
        );
        // `range` narrows the datatype's own bounds — the table the connector enforces.
        assert_eq!(props["temp_u16"]["minimum"], 0.0);
        assert_eq!(props["temp_u16"]["maximum"], 100.0);
        assert_eq!(props["temp_u16"]["order"], 7, "the declared order is kept");

        // With no `parameter` table at all, the point's `name` becomes the title, and the order
        // is its position in the configuration — which the manifest resolved.
        assert_eq!(props["coil_rw"]["title"], "Pump enable");
        assert_eq!(props["coil_rw"]["type"], "boolean");
        assert_eq!(props["coil_rw"]["order"], 2);
        assert_eq!(props["flow_limit"]["order"], 5);

        // A read-only point that opted in is shown, not edited; its bounds are the datatype's.
        assert_eq!(props["status_word"]["readOnly"], true);
        assert_eq!(props["status_word"]["maximum"], 65535.0);
        assert!(props.get("hidden_rw").is_none(), "parameter = false opts out");
        assert!(props.get("level_f32").is_none(), "read-only and never opted in");

        // An absolute `parameter.set` is a definition of its own, titled after the identifier.
        let pump = &defs[2];
        assert_eq!(pump["jsonSchema"]["title"], "Pump");
        let speed = &pump["jsonSchema"]["properties"]["pump_speed"];
        assert_eq!(speed["type"], "number");
        assert!(speed["description"].as_str().unwrap().contains("write-only"));

        // The untyped device falls back to the protocol, as its manifest already says.
        assert!(defs[3]["jsonSchema"]["properties"]["spare_rw"].is_object());
    }

    /// `[connector] parameter_set` is applied by the connector, so the renderer never sees the
    /// derived names at all — it reads the sets off the manifest like any other consumer.
    #[test]
    fn a_forced_set_arrives_already_resolved() {
        let forced = CONFIG.replace(
            "protocol = \"modbus\"",
            "protocol = \"modbus\"\nparameter_set = \"plant_settings\"",
        );
        let defs = definitions(&manifests(&forced));
        let ids: Vec<&str> = defs.iter().map(|d| d["identifier"].as_str().unwrap()).collect();
        assert_eq!(ids, ["plant_settings", "pump"], "only the absolute set survives it");
    }

    /// A set is tenant-wide, so two manifests declaring it produce ONE definition: the keys
    /// merge, the first definition of a key wins, and the protocol is the first declarer's.
    #[test]
    fn a_set_shared_by_two_manifests_is_one_definition() {
        let modbus = manifests(
            r#"
[connector]
protocol = "modbus"
[[device]]
name = "plc1"
type = "acme-boiler-v2"
protocol_address = {}
  [[device.point]]
  id = "temp_u16"
  datatype = "uint16"
  access = "read_write"
  name = "Boiler temp"
  address = {}
"#,
        );
        // Another protocol, the same device type once folded: `acme boiler v2` -> the same set.
        let opcua = manifests(
            r#"
[connector]
protocol = "opcua"
[[device]]
name = "boiler2"
type = "acme boiler v2"
protocol_address = {}
  [[device.point]]
  id = "temp_u16"
  datatype = "uint16"
  access = "read_write"
  name = "Second title"
  address = {}
  [[device.point]]
  id = "level"
  datatype = "float32"
  access = "read_write"
  address = {}
"#,
        );
        let both: Vec<DeviceManifest> = modbus.into_iter().chain(opcua).collect();
        let defs = definitions(&both);
        assert_eq!(defs.len(), 1);
        let props = &defs[0]["jsonSchema"]["properties"];
        assert_eq!(props["temp_u16"]["title"], "Boiler temp", "the first definition wins");
        assert!(props["level"].is_object(), "the second manifest's extra key is merged in");
        assert_eq!(
            defs[0]["tags"][1], "modbus",
            "tagged with the protocol that declared the set first"
        );
    }

    /// The renderer is a function of the manifest and nothing else: a manifest with no
    /// parameters renders nothing, and one whose fields it does not recognise renders defaults
    /// rather than panicking.
    #[test]
    fn a_manifest_without_parameters_renders_nothing() {
        assert!(definitions(&[]).is_empty());
        assert!(definitions(&[DeviceManifest {
            device: "plc1".into(),
            manifest: json!({ "contract": "0.2", "protocol": "modbus", "points": {} }),
        }])
        .is_empty());
        let odd = definitions(&[DeviceManifest {
            device: "plc1".into(),
            manifest: json!({ "points": { "p": { "parameter": { "sets": ["s"] } } } }),
        }]);
        assert_eq!(odd[0]["jsonSchema"]["properties"]["p"]["type"], "string");
        assert_eq!(odd[0]["jsonSchema"]["properties"]["p"]["readOnly"], true);
    }
}
