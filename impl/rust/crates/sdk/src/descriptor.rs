//! Device *parameters*: which points a configuration exposes as operator-editable settings,
//! and how to declare them in Cumulocity's Digital Twin Manager (DTM).
//!
//! A parameter is a point whose `access` permits writes, plus any point that opts in through
//! `meta.parameter` (`meta.parameter = false` opts a writable point out). Parameters are grouped
//! into **sets**: one set is one twin fragment on the device (published by the
//! `ot-parameter-state` flow with the current values) and one DTM property definition in the
//! tenant (rendered by `tedge-dot describe`). The keys of a set are the point ids, unless a point
//! names its own with `meta.parameter.key` (so `firmwareVersion` can be `version` in a `firmware`
//! set: `key = "firmware.version"` names the set and the key at once, while a key without a dot
//! stays in the point's usual set). Keys must be plain identifiers (`[A-Za-z0-9_]`) and unique per set on a device; the
//! capability descriptor lists the points naming one (`parameter_keys`), so the flows know a key
//! before the point samples.
//!
//! ## Naming a set
//!
//! A DTM identifier is **tenant-wide**, so a set name has to be as specific as the points in it.
//! It is therefore qualified by the *device type* (§3.1) — the thing that determines which
//! points exist — and not by the protocol, which says nothing about them:
//!
//! ```text
//! <device type, else the protocol>_<group, default "control">_parameters
//! ```
//!
//! `acme-meter-v2` with the default group gives `acme_meter_v2_control_parameters`; a device
//! with no declared type falls back to `modbus_control_parameters`, which is fine for a fleet of
//! one type and collides for a fleet of several — the reason to declare the type.
//!
//! `meta.parameter.group` names a second set for the same device type (`commissioning` ->
//! `acme_meter_v2_commissioning_parameters`); `meta.parameter.set` bypasses the naming rule
//! entirely and is used verbatim, which is how points of *different* device types can be made to
//! share one set, or an existing tenant identifier can be matched.
//!
//! `meta.parameter` (all optional; either a string naming the set, `true`, or a table):
//!
//! ```toml
//! [[device]]
//! type = "acme-boiler-v2"
//!
//! [[device.point]]
//! id       = "setpoint"
//! datatype = "int16"
//! access   = "read_write"
//! unit     = "°C"
//! meta.parameter = { title = "Setpoint", min = 0, max = 120, order = 1 }
//! # -> set "acme_boiler_v2_control_parameters"
//! ```

use crate::config::{ConnectorConfig, DeviceConfig, PointConfig};
use crate::connector::Access;
use crate::model::DataType;
use serde_json::{json, Map, Value};

pub use crate::library::trim_c;

/// The group a parameter belongs to when it names none.
pub const DEFAULT_GROUP: &str = "control";

/// Every *run* of characters outside `[A-Za-z0-9]` becomes a single `_`, so a device type or
/// group name can be written the way it reads (`acme-meter-v2`) and still be a valid fragment
/// key. A run rather than a character because the C implementation folds bytes and this one
/// folds chars: collapsing runs is what makes them agree on a name with a non-ASCII character
/// in it (one multi-byte char = one run either way).
fn sanitize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_was_sep = false;
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
            last_was_sep = false;
        } else if !last_was_sep {
            out.push('_');
            last_was_sep = true;
        }
    }
    out
}

/// A parameter set name: `<qualifier>_<group>_parameters` (§5.2). Sanitized as a whole, so a
/// qualifier that already ends in a separator does not produce a doubled `_`.
pub fn set_name(qualifier: &str, group: &str) -> String {
    sanitize(&format!("{qualifier}_{group}_parameters"))
}

/// How one device's parameter sets are named.
///
/// Built per device, because the qualifier is the device's own type. `forced` is
/// `tedge-dot describe --set <name>` and the `ot-parameter-state` flow's `default_set`: a
/// single set name for everything that does not name its own, which is the escape hatch for a
/// tenant identifier that predates this rule.
#[derive(Clone, Debug)]
pub struct SetNaming {
    forced: Option<String>,
    qualifier: String,
}

impl SetNaming {
    /// The naming of `device`'s sets: qualified by its declared `type`, else by the protocol.
    pub fn of(device: &DeviceConfig, protocol: &str, forced: Option<&str>) -> Self {
        SetNaming {
            forced: forced.map(String::from),
            // Used verbatim: the loader normalised and validated the declared type, so every
            // renderer (set names, sample envelope, link status) spells it identically.
            qualifier: device
                .device_type
                .clone()
                .filter(|t| !t.is_empty())
                .unwrap_or_else(|| protocol.to_string()),
        }
    }

    /// The set a point in `group` belongs to.
    pub fn set_for(&self, group: Option<&str>) -> String {
        match &self.forced {
            Some(set) => set.clone(),
            None => set_name(&self.qualifier, group.unwrap_or(DEFAULT_GROUP)),
        }
    }

    /// Every set a point belongs to, given its `meta.parameter` options.
    ///
    /// `set` and `group` each accept a string or an array of them, so one point can appear in
    /// several sets — operators group signals by what they are *for*, and the same setpoint
    /// belongs on the commissioning screen and the daily-operation one. The point's value is
    /// published to every set's fragment, so the groups stay consistent with each other.
    pub fn sets_of(&self, options: &Map<String, Value>) -> Vec<String> {
        // A key that names its set (`key = "<set>.<key>"`) is absolute, like `set`, and wins.
        if let Some((set, _)) = options.get("key").and_then(Value::as_str).and_then(dotted_key) {
            return vec![set.to_string()];
        }
        // An absolute `set` bypasses the naming rule entirely and wins over `group`.
        let absolute = names_of(options.get("set"));
        if !absolute.is_empty() {
            return absolute;
        }
        if let Some(forced) = &self.forced {
            return vec![forced.clone()];
        }
        let groups = names_of(options.get("group"));
        if groups.is_empty() {
            return vec![self.set_for(None)];
        }
        // Two group names can fold to one set name ("a b" and "a-b"), so dedupe the result
        // rather than the input: a point must not appear twice in one definition.
        let mut sets: Vec<String> = Vec::with_capacity(groups.len());
        for group in groups {
            let set = set_name(&self.qualifier, &group);
            if !sets.contains(&set) {
                sets.push(set);
            }
        }
        sets
    }
}

/// The names a `set`/`group` option holds: one string, or an array of them. Empty and
/// non-string entries are ignored, so a mistyped entry degrades to the default group rather
/// than inventing a set name out of `null`.
fn names_of(value: Option<&Value>) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    let mut push = |name: &str| {
        if !name.is_empty() && !names.iter().any(|n| n == name) {
            names.push(name.to_string());
        }
    };
    match value {
        Some(Value::String(s)) => push(s),
        Some(Value::Array(items)) => {
            for item in items {
                if let Some(s) = item.as_str() {
                    push(s);
                }
            }
        }
        _ => {}
    }
    names
}

/// A key that names its set too, `<set>.<key>`: both halves, split at its only dot. `None` for a
/// plain key — and for one with more dots or an empty half, which stays a plain key and is
/// refused by [`invalid_keys`]. A dot can only be this separator, because neither a set name nor
/// a key may contain one.
pub fn dotted_key(key: &str) -> Option<(&str, &str)> {
    let (set, name) = key.split_once('.')?;
    (!set.is_empty() && !name.is_empty() && !name.contains('.')).then_some((set, name))
}

/// True when `id` can be used verbatim as a fragment key (Cumulocity rejects `.` and `$`).
pub fn is_valid_key(id: &str) -> bool {
    !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// One parameter derived from a configured point.
#[derive(Clone, Debug, PartialEq)]
pub struct Parameter {
    /// Point id.
    pub point: String,
    /// The key inside the set: `meta.parameter.key`, else the point id. It lets a point keep an
    /// id that is unique on the device and still carry a short, conventional key in its set.
    pub key: String,
    /// Parameter set (twin fragment / DTM identifier).
    pub set: String,
    pub datatype: Option<DataType>,
    pub access: Access,
    pub unit: Option<String>,
    /// The point's own `name`/`description` (§3.1). `meta.parameter.title` and
    /// `meta.parameter.description` override them, so a point can carry a general-purpose
    /// label and still say something different in the parameter UI.
    pub name: Option<String>,
    pub description: Option<String>,
    /// The `meta.parameter` table (normalized to an object).
    pub options: Map<String, Value>,
}

/// The parameters of one point: one per set it belongs to, and none when it is not a
/// parameter at all. `naming` names the sets for a point that does not give absolute ones.
///
/// A point in several groups yields several [`Parameter`]s — identical but for `set` — which
/// is what puts it in each of those definitions and fragments.
pub fn parameters_of(point: &PointConfig, naming: &SetNaming) -> Vec<Parameter> {
    let access = Access::parse(point.access.as_deref());
    let options: Option<Map<String, Value>> = match point.meta.as_ref().and_then(|m| m.get("parameter")) {
        None => None,
        Some(Value::Bool(false)) => return Vec::new(), // explicit opt-out
        Some(Value::Bool(true)) => Some(Map::new()),
        Some(Value::String(set)) => {
            let mut m = Map::new();
            m.insert("set".into(), Value::String(set.clone()));
            Some(m)
        }
        Some(Value::Object(m)) => Some(m.clone()),
        Some(_) => Some(Map::new()),
    };
    if !access.can_write() && options.is_none() {
        return Vec::new();
    }
    let options = options.unwrap_or_default();
    // A string key replaces the id inside the set — its part after the set, for a key naming its
    // set — and anything else is no key. An unusable string is kept as it is, so `invalid_keys`
    // reports it rather than silently falling back to the id.
    let key = match options.get("key") {
        Some(Value::String(key)) => dotted_key(key).map_or(key.as_str(), |(_, name)| name).to_string(),
        _ => point.id.clone(),
    };
    naming
        .sets_of(&options)
        .into_iter()
        .map(|set| Parameter {
            point: point.id.clone(),
            key: key.clone(),
            set,
            datatype: point.datatype,
            access,
            unit: point.unit.clone(),
            name: point.name.clone(),
            description: point.description.clone(),
            options: options.clone(),
        })
        .collect()
}

/// Every parameter of every device in the config, in configuration order. `forced` is the
/// `--set` override: one set name for every point that does not give an absolute one.
pub fn parameters(config: &ConnectorConfig, forced: Option<&str>) -> Vec<Parameter> {
    config
        .devices
        .iter()
        .flat_map(|device| {
            let naming = SetNaming::of(device, &config.connector.protocol, forced);
            device
                .points
                .iter()
                .flat_map(move |p| parameters_of(p, &naming))
        })
        .collect()
}

/// Every device of every configuration, with the protocol that names its fallback sets, in
/// configuration order.
///
/// The `*_across` functions below take several configurations because one service runs every
/// config in its directory, and a DTM identifier is tenant-wide: two files declaring the same
/// device type share its sets, and two types that fold to one name collide no matter which files
/// they are in.
fn devices_across(configs: &[ConnectorConfig]) -> impl Iterator<Item = (&str, &DeviceConfig)> {
    configs.iter().flat_map(|config| {
        config
            .devices
            .iter()
            .map(move |device| (config.connector.protocol.as_str(), device))
    })
}

/// Devices that expose parameters without declaring a `type`, so their sets fall back to the
/// protocol — which every other device of every other type on that protocol also falls back to.
/// `describe` warns about them; it is not an error, because a fleet of one type is fine.
pub fn devices_without_type(config: &ConnectorConfig) -> Vec<String> {
    untyped_devices_across(std::slice::from_ref(config), &config.connector.protocol)
}

/// [`devices_without_type`] over several configurations: the untyped devices of those speaking
/// `protocol`. Per protocol, because the set such a device falls back to — and so everything it
/// collides with — is named after it.
pub fn untyped_devices_across(configs: &[ConnectorConfig], protocol: &str) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for (p, device) in devices_across(configs) {
        if p != protocol || !device.device_type.as_deref().unwrap_or("").is_empty() {
            continue;
        }
        let naming = SetNaming::of(device, p, None);
        let has_parameters = device
            .points
            .iter()
            .any(|point| !parameters_of(point, &naming).is_empty());
        // Named once, even when several files define a device of that name.
        if has_parameters && !names.contains(&device.name) {
            names.push(device.name.clone());
        }
    }
    names
}

/// Warnings about the device types a configuration declares: types that produce the *same* set
/// names, and types that produce no usable name at all.
///
/// The collision case — `"acme meter"` and
/// `"acme-meter"` both give `acme_meter_control_parameters`, so their sets share one
/// tenant-wide definition and the first one rendered silently wins, which is this feature's own
/// failure mode one scope down.
///
/// Types are grouped and displayed by the set name [`set_name`] actually derives for them, so
/// there is no second notion of "qualifier" to drift from the real one — the C implementation
/// compares the same strings. One message per colliding group, in configuration order.
///
/// Deliberately not reported for an absolute `meta.parameter.set` shared by several device
/// types: that is the documented way to share a set on purpose (§5.2).
pub fn type_warnings(config: &ConnectorConfig) -> Vec<String> {
    type_warnings_across(std::slice::from_ref(config))
}

/// [`type_warnings`] over several configurations: a device type in one file collides with a type
/// folding to the same name in another, since the set names are tenant-wide either way.
pub fn type_warnings_across(configs: &[ConnectorConfig]) -> Vec<String> {
    let mut warnings: Vec<String> = Vec::new();
    // representative set name -> the distinct raw types that derive it
    let mut folded: Vec<(String, Vec<String>)> = Vec::new();
    for (protocol, device) in devices_across(configs) {
        let Some(declared) = device.device_type.as_deref().filter(|t| !t.is_empty()) else {
            continue;
        };
        // A type with nothing usable in it (`"日本語"`, `"---"`) folds away entirely and the
        // sets are named `_control_parameters` — which every such type shares, silently.
        if !declared.bytes().any(|b| b.is_ascii_alphanumeric()) {
            warnings.push(format!(
                "warning: device type '{declared}' has no [A-Za-z0-9] character, so its \
                 parameter sets are named '{}' with nothing to tell them apart from another \
                 such type's; name the type in ASCII",
                set_name(declared, DEFAULT_GROUP)
            ));
        }
        // A device with no parameters derives no set, so it cannot collide with anything.
        let naming = SetNaming::of(device, protocol, None);
        if !device.points.iter().any(|p| !parameters_of(p, &naming).is_empty()) {
            continue;
        }
        let key = set_name(declared, DEFAULT_GROUP);
        match folded.iter_mut().find(|(k, _)| *k == key) {
            Some((_, types)) => {
                if !types.iter().any(|t| t == declared) {
                    types.push(declared.to_string());
                }
            }
            None => folded.push((key, vec![declared.to_string()])),
        }
    }
    warnings.extend(folded
        .into_iter()
        .filter(|(_, types)| types.len() > 1)
        .map(|(key, types)| {
            let names: Vec<String> = types.iter().map(|t| format!("'{t}'")).collect();
            format!(
                "warning: device types {} derive the same parameter set names (e.g. '{}'), so \
                 they share one tenant-wide definition and the first one rendered wins; give \
                 them names that differ by more than punctuation",
                names.join(", "),
                key
            )
        }));
    warnings
}

/// Parameter keys (and set names) that cannot be used as fragment keys.
pub fn invalid_keys(config: &ConnectorConfig, forced: Option<&str>) -> Vec<String> {
    invalid_keys_across(std::slice::from_ref(config), forced)
}

/// [`invalid_keys`] over several configurations, in configuration order.
pub fn invalid_keys_across(configs: &[ConnectorConfig], forced: Option<&str>) -> Vec<String> {
    let mut bad = Vec::new();
    for config in configs {
        for device in &config.devices {
            let naming = SetNaming::of(device, &config.connector.protocol, forced);
            for point in &device.points {
                // A point yields one parameter per set, but what is wrong with it is reported
                // once, key first and then its sets — as the C build reports it.
                let mut reported: Vec<String> = Vec::new();
                for p in parameters_of(point, &naming) {
                    let mut items = Vec::new();
                    // A point that names its key is published under the key, so only the key
                    // has to be usable — not the id.
                    if p.key != p.point {
                        if !is_valid_key(&p.key) {
                            items.push(format!("parameter key '{}' of point '{}'", p.key, p.point));
                        }
                    } else if !is_valid_key(&p.point) {
                        items.push(format!("point id '{}'", p.point));
                    }
                    if !is_valid_key(&p.set) {
                        items.push(format!("parameter set '{}'", p.set));
                    }
                    for item in items {
                        if !reported.contains(&item) {
                            reported.push(item.clone());
                            bad.push(item);
                        }
                    }
                }
            }
        }
    }
    bad
}

/// Parameter keys that cannot be rendered:
///
/// * two points of one device with the same key in one set — a fragment holds one value per key;
/// * a point combining `set` with `key` — a key names its set itself (`<set>.<key>`);
/// * a point combining `group` with a key that names its set — the set is then absolute.
pub fn key_conflicts(config: &ConnectorConfig, forced: Option<&str>) -> Vec<String> {
    key_conflicts_across(std::slice::from_ref(config), forced)
}

/// [`key_conflicts`] over several configurations, in configuration order. Same text as the C
/// build's `tdot_param_key_conflicts`.
pub fn key_conflicts_across(configs: &[ConnectorConfig], forced: Option<&str>) -> Vec<String> {
    let mut conflicts = Vec::new();
    for config in configs {
        for device in &config.devices {
            let naming = SetNaming::of(device, &config.connector.protocol, forced);
            // (set, key, the point of this device that has it first)
            let mut seen: Vec<(String, String, String)> = Vec::new();
            for point in &device.points {
                let params = parameters_of(point, &naming);
                if let Some(p) = params.first() {
                    if let Some(key) = p.options.get("key").and_then(Value::as_str) {
                        if p.options.contains_key("set") {
                            conflicts.push(format!(
                                "point '{}' on device '{}' combines \"set\" with \"key\": write the key as '<set>.<key>'",
                                p.point, device.name
                            ));
                        } else if dotted_key(key).is_some() && p.options.contains_key("group") {
                            conflicts.push(format!(
                                "point '{}' on device '{}' combines \"group\" with a key that names its set",
                                p.point, device.name
                            ));
                        }
                    }
                }
                for p in params {
                    match seen.iter().find(|(set, key, _)| *set == p.set && *key == p.key) {
                        Some((_, _, first)) => conflicts.push(format!(
                            "key '{}' of points '{}' and '{}' in set '{}' on device '{}'",
                            p.key, first, p.point, p.set, device.name
                        )),
                        None => seen.push((p.set, p.key, p.point)),
                    }
                }
            }
        }
    }
    conflicts
}

/// Render Cumulocity Digital Twin Manager property definitions — one per parameter set — for
/// every device in the config. The output is the request body of
/// `POST /service/dtm/definitions/properties` (one element at a time), which a tenant admin
/// registers once; the device never talks to the DTM service. Sets that appear on several
/// devices are merged (the identifier is tenant-wide, so devices sharing a set must agree).
pub fn c8y_dtm_definitions(config: &ConnectorConfig, forced_set: Option<&str>) -> Vec<Value> {
    c8y_dtm_definitions_across(std::slice::from_ref(config), forced_set)
}

/// [`c8y_dtm_definitions`] over several configurations — every connector a service runs. A set
/// declared in more than one of them is still ONE definition: keys merge across files (the first
/// definition of a key wins), and the definition is described and tagged with the protocol of
/// the configuration that declared the set first.
pub fn c8y_dtm_definitions_across(
    configs: &[ConnectorConfig],
    forced_set: Option<&str>,
) -> Vec<Value> {
    // ordered (key, property schema)
    type Properties = Vec<(String, Value)>;
    // set -> (protocol of the config declaring it first, its properties)
    let mut sets: Vec<(String, &str, Properties)> = Vec::new();
    for config in configs {
        let protocol = config.connector.protocol.as_str();
        for param in parameters(config, forced_set) {
            let index = match sets.iter().position(|(name, _, _)| *name == param.set) {
                Some(index) => index,
                None => {
                    sets.push((param.set.clone(), protocol, Vec::new()));
                    sets.len() - 1
                }
            };
            let props = &mut sets[index].2;
            if props.iter().any(|(k, _)| *k == param.key) {
                continue; // same key on another device: first definition wins
            }
            let schema = property_schema(&param);
            props.push((param.key.clone(), schema));
        }
    }
    sets.into_iter()
        .map(|(set, protocol, props)| {
            let mut properties = Map::new();
            for (i, (key, mut schema)) in props.into_iter().enumerate() {
                if !schema.as_object().map(|o| o.contains_key("order")).unwrap_or(false) {
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
                        "Writable {protocol} points exposed by tedge-dot (generated from the connector configuration)",
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

/// JSON-schema property for one parameter (type from the datatype, limits from the datatype
/// range, everything else from `meta.parameter`).
pub fn property_schema(param: &Parameter) -> Value {
    let mut schema = Map::new();
    let (ty, min, max): (&str, Option<f64>, Option<f64>) = match param.datatype {
        Some(DataType::Bool) => ("boolean", None, None),
        Some(DataType::Int8) => ("integer", Some(i8::MIN as f64), Some(i8::MAX as f64)),
        Some(DataType::Uint8) => ("integer", Some(0.0), Some(u8::MAX as f64)),
        Some(DataType::Int16) => ("integer", Some(i16::MIN as f64), Some(i16::MAX as f64)),
        Some(DataType::Uint16) => ("integer", Some(0.0), Some(u16::MAX as f64)),
        Some(DataType::Int32) => ("integer", Some(i32::MIN as f64), Some(i32::MAX as f64)),
        Some(DataType::Uint32) => ("integer", Some(0.0), Some(u32::MAX as f64)),
        // 64-bit limits exceed the JS safe range; leave them unbounded.
        Some(DataType::Int64) | Some(DataType::Uint64) => ("integer", None, None),
        Some(DataType::Float32) | Some(DataType::Float64) => ("number", None, None),
        Some(DataType::String) | Some(DataType::Bytes) | None => ("string", None, None),
    };
    schema.insert("type".into(), json!(ty));
    let title = param
        .options
        .get("title")
        .and_then(|t| t.as_str())
        .map(String::from)
        .or_else(|| param.name.clone())
        .unwrap_or_else(|| param.key.clone());
    schema.insert("title".into(), json!(title));
    let mut description = param
        .options
        .get("description")
        .and_then(|d| d.as_str())
        .map(String::from)
        .or_else(|| param.description.clone())
        .unwrap_or_default();
    if let Some(unit) = &param.unit {
        if !description.is_empty() {
            description.push(' ');
        }
        description.push_str(&format!("[{unit}]"));
    }
    if param.access == Access::Write {
        if !description.is_empty() {
            description.push(' ');
        }
        description.push_str("(write-only: shows the last value written)");
    }
    if !description.is_empty() {
        schema.insert("description".into(), json!(description));
    }
    let min = param.options.get("min").and_then(|v| v.as_f64()).or(min);
    let max = param.options.get("max").and_then(|v| v.as_f64()).or(max);
    if let Some(min) = min {
        schema.insert("minimum".into(), json!(min));
    }
    if let Some(max) = max {
        schema.insert("maximum".into(), json!(max));
    }
    for key in ["enum", "default", "order"] {
        if let Some(v) = param.options.get(key) {
            schema.insert(key.into(), v.clone());
        }
    }
    if !param.access.can_write() {
        schema.insert("readOnly".into(), json!(true));
    }
    Value::Object(schema)
}

/// The `point_labels` of the capability descriptor (§7): every configured point that declares
/// a `name` or a `description`, so a consumer can show something friendlier than the point id.
///
/// Points with neither are omitted — their id *is* their label — so a configuration that uses
/// none of this adds nothing to the descriptor. Published once, retained, rather than echoed
/// in every sample: the labels are static, and a sample is a time series.
pub fn point_labels(config: &ConnectorConfig) -> Vec<Value> {
    let mut labels = Vec::new();
    for device in &config.devices {
        for point in &device.points {
            if point.name.is_none() && point.description.is_none() {
                continue;
            }
            let mut entry = Map::new();
            entry.insert("device".into(), json!(device.name));
            entry.insert("point".into(), json!(point.id));
            if let Some(name) = &point.name {
                entry.insert("name".into(), json!(name));
            }
            if let Some(description) = &point.description {
                entry.insert("description".into(), json!(description));
            }
            labels.push(Value::Object(entry));
        }
    }
    labels
}

/// The `parameter_keys` of the capability descriptor (§7): every configured point that names its
/// own key inside its parameter sets (`meta.parameter.key`), with the `set` / `group` that name
/// those sets, exactly as configured.
///
/// A consumer — the `ot-parameter-state` flow — learns from it which point a key belongs to
/// before the point samples: right after a restart (samples are not retained), and at all for a
/// write-only point, which never samples. Points without a key are omitted, since their key is
/// their id, so a configuration that names no key adds nothing to the descriptor.
pub fn parameter_keys(config: &ConnectorConfig) -> Vec<Value> {
    let mut keys = Vec::new();
    for device in &config.devices {
        for point in &device.points {
            let Some(Value::Object(options)) = point.meta.as_ref().and_then(|m| m.get("parameter"))
            else {
                continue;
            };
            let Some(Value::String(key)) = options.get("key") else {
                continue;
            };
            let mut entry = Map::new();
            entry.insert("device".into(), json!(device.name));
            entry.insert("point".into(), json!(point.id));
            entry.insert("key".into(), json!(key));
            for name in ["set", "group"] {
                if let Some(value) = options.get(name) {
                    entry.insert(name.into(), value.clone());
                }
            }
            keys.push(Value::Object(entry));
        }
    }
    keys
}

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
  meta = { parameter = { title = "Temperature setpoint", min = 0, max = 100, order = 7 } }

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
  meta = { parameter = "pump" }

  [[device.point]]
  id = "level_f32"
  datatype = "float32"
  description = "Level in the buffer tank"
  address = { table = "holding", address = 6, count = 2 }

  [[device.point]]
  id = "status_word"
  datatype = "uint16"
  address = { table = "holding", address = 20, count = 1 }
  meta = { parameter = true }

  [[device.point]]
  id = "hidden_rw"
  datatype = "uint16"
  access = "read_write"
  address = { table = "holding", address = 21, count = 1 }
  meta = { parameter = false }

  [[device.point]]
  id = "commission_code"
  datatype = "uint16"
  access = "read_write"
  address = { table = "holding", address = 22, count = 1 }
  meta = { parameter = { group = "commissioning" } }

  # In two groups at once: operators see it on the daily screen and the
  # commissioning one, and both fragments carry its value.
  [[device.point]]
  id = "flow_limit"
  datatype = "uint16"
  access = "read_write"
  address = { table = "holding", address = 23, count = 1 }
  meta = { parameter = { group = ["control", "commissioning"] } }

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

    fn cfg() -> ConnectorConfig {
        toml::from_str(CONFIG).unwrap()
    }

    /// A set name is qualified by the *device type*, because that is what decides which points
    /// exist; the protocol is only the fallback for a device that does not declare one. An
    /// absolute `meta.parameter.set` is used verbatim, a `group` names a second set of the same
    /// device type.
    #[test]
    fn parameters_select_writable_and_opted_in_points() {
        let params = parameters(&cfg(), None);
        let names: Vec<(&str, &str)> = params
            .iter()
            .map(|p| (p.point.as_str(), p.set.as_str()))
            .collect();
        assert_eq!(
            names,
            vec![
                ("temp_u16", "acme_boiler_v2_control_parameters"),
                ("coil_rw", "acme_boiler_v2_control_parameters"),
                ("pump_speed", "pump"),
                ("status_word", "acme_boiler_v2_control_parameters"),
                ("commission_code", "acme_boiler_v2_commissioning_parameters"),
                // One point, two groups -> one parameter per set, in the declared order.
                ("flow_limit", "acme_boiler_v2_control_parameters"),
                ("flow_limit", "acme_boiler_v2_commissioning_parameters"),
                // plc2 declares no type: back to the protocol, which is what collides across
                // device types and is the reason `describe` warns about it.
                ("spare_rw", "modbus_control_parameters"),
            ]
        );
        assert_eq!(params[3].access, Access::Read);
        assert_eq!(devices_without_type(&cfg()), vec!["plc2".to_string()]);
    }

    /// `--set` forces one name for every point that does not give an absolute one — the escape
    /// hatch for a tenant identifier that predates the naming rule.
    #[test]
    fn forced_set_overrides_the_derived_name() {
        let params = parameters(&cfg(), Some("legacy_params"));
        let names: Vec<(&str, &str)> = params
            .iter()
            .map(|p| (p.point.as_str(), p.set.as_str()))
            .collect();
        assert_eq!(
            names,
            vec![
                ("temp_u16", "legacy_params"),
                ("coil_rw", "legacy_params"),
                ("pump_speed", "pump"),
                ("status_word", "legacy_params"),
                ("commission_code", "legacy_params"),
                // --set collapses a multi-group point to the one forced name, once.
                ("flow_limit", "legacy_params"),
                ("spare_rw", "legacy_params"),
            ]
        );
    }

    /// `set` and `group` both accept a list, an absolute `set` still wins, and names that fold
    /// to the same set are not repeated.
    #[test]
    fn a_point_can_belong_to_several_sets() {
        let naming = SetNaming::of(&cfg().devices[0], "modbus", None);
        let sets = |toml: &str| {
            let point: PointConfig = toml::from_str(toml).unwrap();
            parameters_of(&point, &naming)
                .into_iter()
                .map(|p| p.set)
                .collect::<Vec<_>>()
        };
        let point = |meta: &str| {
            format!(
                "id = \"p\"\ndatatype = \"uint16\"\naccess = \"read_write\"\n\
                 address = {{ table = \"holding\", address = 1, count = 1 }}\n{meta}"
            )
        };

        assert_eq!(
            sets(&point("meta = { parameter = { group = [\"control\", \"commissioning\"] } }")),
            [
                "acme_boiler_v2_control_parameters",
                "acme_boiler_v2_commissioning_parameters"
            ]
        );
        assert_eq!(
            sets(&point("meta = { parameter = { set = [\"plant_a\", \"plant_b\"] } }")),
            ["plant_a", "plant_b"]
        );
        // An absolute set still wins over the groups.
        assert_eq!(
            sets(&point(
                "meta = { parameter = { set = \"pump\", group = [\"a\", \"b\"] } }"
            )),
            ["pump"]
        );
        // Group names that fold to the same set name yield one set, not two.
        assert_eq!(
            sets(&point("meta = { parameter = { group = [\"a b\", \"a-b\"] } }")),
            ["acme_boiler_v2_a_b_parameters"]
        );
        // An empty or unusable list is an absent one: the default group, never no set at all.
        for meta in [
            "meta = { parameter = { group = [] } }",
            "meta = { parameter = { group = [1, true] } }",
            "meta = { parameter = { set = [] } }",
        ] {
            assert_eq!(
                sets(&point(meta)),
                ["acme_boiler_v2_control_parameters"],
                "{meta}"
            );
        }
        // ...and opting out still beats every list.
        assert!(sets(&point("meta = { parameter = false }")).is_empty());
    }

    /// Two device types that differ only in punctuation fold to one qualifier, so their sets
    /// collide exactly as two protocols' did before this feature — `describe` says so.
    #[test]
    fn device_type_problems_are_warned_about() {
        let mut c = cfg();
        assert!(type_warnings(&c).is_empty());
        c.devices[1].device_type = Some("acme boiler v2".into()); // vs plc1's "acme-boiler-v2"
        let warnings = type_warnings(&c);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("'acme-boiler-v2', 'acme boiler v2'"), "{}", warnings[0]);
        // Named by the set the two actually derive, not by a separately-computed qualifier —
        // a type ending in punctuation folds differently in the two, which is how the C and
        // Rust messages drifted apart the first time.
        assert!(
            warnings[0].contains("acme_boiler_v2_control_parameters"),
            "{}",
            warnings[0]
        );
        // The same type twice is one device type, not a collision.
        c.devices[1].device_type = Some("acme-boiler-v2".into());
        assert!(type_warnings(&c).is_empty());
        // A trailing separator folds into the same name: 'acme-boiler-v2-' collides too.
        c.devices[1].device_type = Some("acme-boiler-v2-".into());
        assert_eq!(type_warnings(&c).len(), 1);
        // A type containing a comma is one type, not two.
        c.devices[0].device_type = Some("Acme, Inc. Meter".into());
        c.devices[1].device_type = None;
        assert!(type_warnings(&c).is_empty());

        // A type with nothing usable in it folds away entirely, which every such type shares.
        c.devices[0].device_type = Some("日本語".into());
        let warnings = type_warnings(&c);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("no [A-Za-z0-9] character"), "{}", warnings[0]);

        // A device that exposes no parameters derives no set, so it cannot collide.
        let mut none = cfg();
        none.devices[1].device_type = Some("acme boiler v2".into());
        for point in &mut none.devices[1].points {
            point.access = Some("read".into());
            point.meta = None;
        }
        assert!(type_warnings(&none).is_empty());
    }

    #[test]
    fn key_validation_and_set_names() {
        assert!(is_valid_key("ok_id_1"));
        assert!(!is_valid_key("Environment.Temperature"));
        assert!(!is_valid_key(""));
        assert_eq!(set_name("opc-ua", DEFAULT_GROUP), "opc_ua_control_parameters");
        assert_eq!(set_name("ACME meter v2", "pump"), "ACME_meter_v2_pump_parameters");
        // A run of separators (or one multi-byte character) folds to a single `_`, which is
        // what keeps this identical to the C implementation's byte-wise fold.
        assert_eq!(set_name("acme -- v2", "control"), "acme_v2_control_parameters");
        assert_eq!(set_name("wärmezähler", "control"), "w_rmez_hler_control_parameters");
        let mut c = cfg();
        c.devices[0].points[0].id = "Boiler.Temp".into();
        let bad = invalid_keys(&c, None);
        assert_eq!(bad, vec!["point id 'Boiler.Temp'"]);
        assert!(invalid_keys(&cfg(), Some("plant.floor"))
            .iter()
            .all(|b| b.contains("parameter set")));
    }

    /// `meta.parameter.key` names the key a point has inside its sets, so a point keeps an id
    /// that is unique on the device and still carries a conventional key. The definition is
    /// keyed by it (a point without a label is titled by it), a usable key frees the id from the
    /// key rule, and two points of a device sharing a key in a set are refused — a key on a
    /// write-only point is not, since the capability descriptor carries it. Mirrors
    /// `check_parameter_keys` in impl/c/tests/describe.c.
    #[test]
    fn a_point_can_name_its_own_key() {
        const KEYED: &str = r#"
[connector]
protocol = "modbus"

[[device]]
name = "plc1"
type = "zephyr"
protocol_address = { transport = "tcp", host = "127.0.0.1", port = 502, unit_id = 1 }

  [[device.point]]
  id = "firmwareName"
  datatype = "uint16"
  name = "Firmware name"
  address = { table = "holding", address = 1, count = 1 }
  meta = { parameter = { key = "firmware.name" } }

  [[device.point]]
  id = "firmwareVersion"
  datatype = "uint16"
  address = { table = "holding", address = 2, count = 1 }
  meta = { parameter = { key = "firmware.version" } }

  [[device.point]]
  id = "Tank.Level"
  datatype = "uint16"
  address = { table = "holding", address = 5, count = 1 }
  meta = { parameter = { key = "tank_level" } }
"#;
        // Appended: a second point with the key `name` in `firmware`, a key on a write-only
        // point (allowed), an unusable key, and the two option combinations a key refuses.
        const CONFLICTS: &str = r#"
  [[device.point]]
  id = "bootName"
  datatype = "uint16"
  address = { table = "holding", address = 3, count = 1 }
  meta = { parameter = { key = "firmware.name" } }

  [[device.point]]
  id = "update_cmd"
  datatype = "uint16"
  access = "write"
  address = { table = "holding", address = 4, count = 1 }
  meta = { parameter = { key = "firmware.update" } }

  [[device.point]]
  id = "level"
  datatype = "uint16"
  address = { table = "holding", address = 6, count = 1 }
  meta = { parameter = { key = "a.b.c" } }

  [[device.point]]
  id = "mixed_set"
  datatype = "uint16"
  address = { table = "holding", address = 7, count = 1 }
  meta = { parameter = { set = "firmware", key = "mixed" } }

  [[device.point]]
  id = "mixed_group"
  datatype = "uint16"
  address = { table = "holding", address = 8, count = 1 }
  meta = { parameter = { group = "control", key = "info.mixed" } }
"#;
        let config: ConnectorConfig = toml::from_str(KEYED).unwrap();
        assert!(
            invalid_keys(&config, None).is_empty(),
            "a usable key frees the id from the key rule"
        );
        assert!(key_conflicts(&config, None).is_empty());
        let defs = c8y_dtm_definitions(&config, None);
        assert_eq!(defs[0]["identifier"], "firmware");
        let firmware = &defs[0]["jsonSchema"]["properties"];
        assert_eq!(firmware["name"]["title"], "Firmware name");
        assert_eq!(
            firmware["version"]["title"], "version",
            "a point without a label is titled by its key"
        );
        assert!(firmware.get("firmwareName").is_none());
        let control = &defs[1]["jsonSchema"]["properties"];
        assert!(control["tank_level"].is_object());
        assert!(control.get("Tank.Level").is_none());

        let config: ConnectorConfig = toml::from_str(&format!("{KEYED}{CONFLICTS}")).unwrap();
        assert_eq!(
            invalid_keys(&config, None),
            vec!["parameter key 'a.b.c' of point 'level'"]
        );
        assert_eq!(
            key_conflicts(&config, None),
            vec![
                "key 'name' of points 'firmwareName' and 'bootName' in set 'firmware' on device 'plc1'",
                "point 'mixed_set' on device 'plc1' combines \"set\" with \"key\": write the key as '<set>.<key>'",
                "point 'mixed_group' on device 'plc1' combines \"group\" with a key that names its set",
            ]
        );
    }

    #[test]
    fn dtm_definitions_group_by_set_and_render_schema() {
        let defs = c8y_dtm_definitions(&cfg(), None);
        assert_eq!(defs.len(), 4);
        // The two-group point is a property of BOTH its sets, so either screen can edit it.
        assert!(defs[0]["jsonSchema"]["properties"]["flow_limit"].is_object());
        assert!(defs[2]["jsonSchema"]["properties"]["flow_limit"].is_object());
        let main = &defs[0];
        assert_eq!(main["identifier"], "acme_boiler_v2_control_parameters");
        assert_eq!(main["contexts"], json!(["asset", "event", "operation"]));
        let props = &main["jsonSchema"]["properties"];
        assert_eq!(props["temp_u16"]["type"], "integer");
        assert_eq!(props["temp_u16"]["title"], "Temperature setpoint");
        assert_eq!(props["temp_u16"]["minimum"], 0.0);
        assert_eq!(props["temp_u16"]["maximum"], 100.0);
        assert_eq!(props["temp_u16"]["order"], 7);
        // meta.parameter.title wins over the point's `name`...
        assert_eq!(props["temp_u16"]["title"], "Temperature setpoint");
        // ...while the point's `description` is used (no meta.parameter.description here) and
        // still composes with the unit.
        assert_eq!(
            props["temp_u16"]["description"],
            "Outlet temperature after the heat exchanger [°C]"
        );
        // With no meta at all, the point's `name` becomes the title.
        assert_eq!(props["coil_rw"]["title"], "Pump enable");
        assert_eq!(props["coil_rw"]["type"], "boolean");
        assert_eq!(props["coil_rw"]["order"], 2);
        assert_eq!(props["status_word"]["readOnly"], true);
        assert_eq!(props["status_word"]["maximum"], 65535.0);
        assert!(props.get("hidden_rw").is_none());
        assert!(props.get("level_f32").is_none());

        let pump = &defs[1];
        assert_eq!(pump["identifier"], "pump");
        assert_eq!(pump["jsonSchema"]["title"], "Pump");
        let speed = &pump["jsonSchema"]["properties"]["pump_speed"];
        assert_eq!(speed["type"], "number");
        assert!(speed["description"].as_str().unwrap().contains("write-only"));

        // One device type, two sets (the group), and one untyped device on the protocol name.
        assert_eq!(defs[2]["identifier"], "acme_boiler_v2_commissioning_parameters");
        assert_eq!(defs[3]["identifier"], "modbus_control_parameters");
        assert!(defs[3]["jsonSchema"]["properties"]["spare_rw"].is_object());
    }

    /// The capability descriptor's `point_labels` (§7): the labels are static, so they are
    /// published once and retained rather than echoed in every sample. Read-only points get an
    /// entry too — they have no DTM parameter to carry one — and a point with neither label is
    /// omitted, so a configuration using none of this adds nothing to the descriptor.
    #[test]
    fn point_labels_carry_the_human_readable_text() {
        let labels = point_labels(&cfg());
        assert_eq!(
            labels,
            vec![
                json!({
                    "device": "plc1",
                    "point": "temp_u16",
                    "name": "Boiler temp",
                    "description": "Outlet temperature after the heat exchanger",
                }),
                json!({ "device": "plc1", "point": "coil_rw", "name": "Pump enable" }),
                json!({
                    "device": "plc1",
                    "point": "level_f32",
                    "description": "Level in the buffer tank",
                }),
            ],
            "labelled points only, in configuration order, with just the fields they declare"
        );

        // A configuration that labels nothing produces no labels at all, so the descriptor is
        // unchanged for everyone not using the feature.
        let mut bare = cfg();
        for point in &mut bare.devices[0].points {
            point.name = None;
            point.description = None;
        }
        assert!(point_labels(&bare).is_empty());
    }

    /// The capability descriptor's `parameter_keys` (§7): only the points that name their own
    /// key, with the `set` / `group` that name their sets as configured — what the
    /// ot-parameter-state flow needs to claim a key before the point samples.
    #[test]
    fn parameter_keys_list_the_points_naming_a_key() {
        let config: ConnectorConfig = toml::from_str(
            r#"
[connector]
protocol = "modbus"

[[device]]
name = "plc1"
protocol_address = { transport = "tcp", host = "127.0.0.1", port = 502, unit_id = 1 }

  [[device.point]]
  id = "firmwareVersion"
  datatype = "uint16"
  address = { table = "holding", address = 1, count = 1 }
  meta = { parameter = { key = "firmware.version" } }

  [[device.point]]
  id = "valve_cmd"
  datatype = "bool"
  access = "write"
  address = { table = "coil", address = 2, count = 1 }
  meta = { parameter = { group = ["control", "commissioning"], key = "valve" } }

  [[device.point]]
  id = "plain_rw"
  datatype = "uint16"
  access = "read_write"
  address = { table = "holding", address = 3, count = 1 }
  meta = { parameter = { group = "control" } }
"#,
        )
        .unwrap();
        assert_eq!(
            parameter_keys(&config),
            vec![
                json!({ "device": "plc1", "point": "firmwareVersion", "key": "firmware.version" }),
                json!({ "device": "plc1", "point": "valve_cmd", "key": "valve", "group": ["control", "commissioning"] }),
            ]
        );
        assert!(
            parameter_keys(&cfg()).is_empty(),
            "a configuration naming no key adds nothing to the descriptor"
        );
    }

    #[test]
    fn dtm_default_set_override() {
        let defs = c8y_dtm_definitions(&cfg(), Some("plant_settings"));
        assert_eq!(defs[0]["identifier"], "plant_settings");
    }

    /// Several configurations at once (`describe -c <dir>`): a set declared in two files is ONE
    /// definition, device types folding together collide across files, and the untyped-device
    /// warning is per protocol. Mirrors `check_across_configs` in impl/c/tests/describe.c.
    #[test]
    fn definitions_and_warnings_span_every_config() {
        let modbus: ConnectorConfig = toml::from_str(
            r#"
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
  name = "Boiler temp"
  address = { table = "holding", address = 3, count = 1 }

[[device]]
name = "plc2"
protocol_address = { transport = "tcp", host = "127.0.0.1", port = 503, unit_id = 1 }
  [[device.point]]
  id = "spare_rw"
  datatype = "uint16"
  access = "read_write"
  address = { table = "holding", address = 30, count = 1 }
"#,
        )
        .unwrap();
        let opcua: ConnectorConfig = toml::from_str(
            r#"
[connector]
protocol = "opcua"

[[device]]
name = "boiler2"
type = "acme boiler v2"
protocol_address = { endpoint = "opc.tcp://127.0.0.1:4840/" }
  [[device.point]]
  id = "temp_u16"
  datatype = "uint16"
  access = "read_write"
  name = "Second title"
  address = { node_id = "ns=2;s=Temp" }
  [[device.point]]
  id = "Tank.Level"
  datatype = "float32"
  access = "read_write"
  address = { node_id = "ns=2;s=Level" }

[[device]]
name = "opc1"
protocol_address = { endpoint = "opc.tcp://127.0.0.1:4841/" }
  [[device.point]]
  id = "spare_rw"
  datatype = "uint16"
  access = "read_write"
  address = { node_id = "ns=2;s=Spare" }
"#,
        )
        .unwrap();
        let both = [modbus.clone(), opcua];

        // The second file's "acme boiler v2" folds to the first file's "acme-boiler-v2", so the
        // two share one definition (a key they both declare keeps the first file's schema), and
        // each protocol's untyped device has its own fallback set.
        let defs = c8y_dtm_definitions_across(&both, None);
        let ids: Vec<&str> = defs.iter().map(|d| d["identifier"].as_str().unwrap()).collect();
        assert_eq!(
            ids,
            [
                "acme_boiler_v2_control_parameters",
                "modbus_control_parameters",
                "opcua_control_parameters"
            ]
        );
        let shared = &defs[0]["jsonSchema"]["properties"];
        assert_eq!(shared["temp_u16"]["title"], "Boiler temp", "the first definition of a key wins");
        assert!(shared["Tank.Level"].is_object(), "the second file's extra key is merged in");
        assert_eq!(defs[0]["tags"][1], "modbus", "tagged with the protocol that declared it first");
        assert_eq!(defs[2]["tags"][1], "opcua");

        // Neither file collides on its own; together they do.
        assert!(type_warnings(&modbus).is_empty());
        let warnings = type_warnings_across(&both);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("'acme-boiler-v2', 'acme boiler v2'"), "{}", warnings[0]);

        assert_eq!(untyped_devices_across(&both, "modbus"), ["plc2"]);
        assert_eq!(untyped_devices_across(&both, "opcua"), ["opc1"]);
        assert_eq!(invalid_keys_across(&both, None), ["point id 'Tank.Level'"]);
    }
}
