//! Device *parameters*: which points a configuration exposes as operator-editable settings,
//! and how to declare them in Cumulocity's Digital Twin Manager (DTM).
//!
//! A parameter is a point whose `access` permits writes, plus any point that opts in through
//! the `parameter` field (`parameter = false` opts a writable point out). Parameters are grouped
//! into **sets**: one set is one twin fragment on the device (published by the
//! `ot-parameter-state` flow with the current values) and one DTM property definition in the
//! tenant. The keys of a set are the point ids, so parameter ids must be plain identifiers
//! (`[A-Za-z0-9_]`).
//!
//! What this module does NOT do is render any of that for a cloud. It decides which points are
//! parameters and what their sets are called; the connector writes the answer onto the device
//! manifest (§8.2), and a renderer — `tedge-dot manifest --format c8y-dtm`, in the binary crate
//! — works from the manifest. That is what keeps the SDK, which "never talks to the DTM
//! service", free of any cloud vendor's schema (RFC 0006 §8.1).
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
//! `parameter.group` names a second set for the same device type (`commissioning` ->
//! `acme_meter_v2_commissioning_parameters`); `parameter.set` bypasses the naming rule
//! entirely and is used verbatim, which is how points of *different* device types can be made to
//! share one set, or an existing tenant identifier can be matched.
//!
//! `parameter` (all optional; either a string naming the set, `true`, `false`, or a table).
//! The bounds are **not** part of it: they are the point's `range` (§5.3), the one table the
//! cloud form renders *and* the connector enforces on write.
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
//! range     = { min = 0, max = 120 }
//! parameter = { title = "Setpoint", order = 1 }
//! # -> set "acme_boiler_v2_control_parameters"
//! ```

use crate::config::{ConnectorConfig, DeviceConfig, PointConfig};
use crate::connector::Access;
use crate::model::DataType;
use serde_json::{Map, Value};

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
/// `[connector] parameter_set`: a single set name for everything that does not name its own,
/// which is the escape hatch for a tenant identifier that predates this rule. It was a CLI flag
/// and a flow parameter that had to agree with each other; it is one configuration key now, and
/// the connector resolves it onto the manifest for both of them (§8.1).
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

    /// Every set a point belongs to, given its `parameter` options.
    ///
    /// `set` and `group` each accept a string or an array of them, so one point can appear in
    /// several sets — operators group signals by what they are *for*, and the same setpoint
    /// belongs on the commissioning screen and the daily-operation one. The point's value is
    /// published to every set's fragment, so the groups stay consistent with each other.
    pub fn sets_of(&self, options: &Map<String, Value>) -> Vec<String> {
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

/// True when `id` can be used verbatim as a fragment key (Cumulocity rejects `.` and `$`).
pub fn is_valid_key(id: &str) -> bool {
    !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// One parameter derived from a configured point.
#[derive(Clone, Debug, PartialEq)]
pub struct Parameter {
    /// Point id (= the key inside the set).
    pub point: String,
    /// Parameter set (twin fragment / DTM identifier).
    pub set: String,
    pub datatype: Option<DataType>,
    pub access: Access,
    pub unit: Option<String>,
    /// The point's own `name`/`description` (§3.1). `parameter.title` and
    /// `parameter.description` override them, so a point can carry a general-purpose
    /// label and still say something different in the parameter UI.
    pub name: Option<String>,
    pub description: Option<String>,
    /// The point's engineering-unit bounds (§5.3), which the cloud form renders as
    /// `minimum`/`maximum` — and which the connector now also enforces on write. In 0.1 these
    /// were `meta.parameter.min`/`max`, trusted by the form and by nothing else.
    pub range: Option<crate::config::Range>,
    /// The `parameter` table (normalized to an object).
    pub options: Map<String, Value>,
}

/// The parameters of one point: one per set it belongs to, and none when it is not a
/// parameter at all. `naming` names the sets for a point that does not give absolute ones.
///
/// A point in several groups yields several [`Parameter`]s — identical but for `set` — which
/// is what puts it in each of those definitions and fragments.
pub fn parameters_of(point: &PointConfig, naming: &SetNaming) -> Vec<Parameter> {
    let access = Access::parse(point.access.as_deref());
    // The typed `parameter` field (§5.2). It was `meta.parameter` in 0.1; `meta` is free-form
    // again, and a stale `meta.parameter` is warned about by the loader rather than read here.
    let options: Option<Map<String, Value>> = match point.parameter.as_ref() {
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
    naming
        .sets_of(&options)
        .into_iter()
        .map(|set| Parameter {
            point: point.id.clone(),
            set,
            datatype: Some(point.datatype),
            access,
            unit: point.unit.clone(),
            name: point.name.clone(),
            description: point.description.clone(),
            range: point.range,
            options: options.clone(),
        })
        .collect()
}

/// Every parameter of every device in the config, in configuration order.
///
/// `[connector] parameter_set`, when the configuration sets it, is one set name for every point
/// that does not give an absolute one. It is read off the configuration rather than passed in:
/// the forcing used to be a CLI flag AND a flow parameter that had to agree with it, and the
/// whole point of §8 is that the decision is written down once.
pub fn parameters(config: &ConnectorConfig) -> Vec<Parameter> {
    let forced = config.connector.parameter_set();
    config
        .devices
        .iter()
        .flat_map(move |device| {
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
fn devices_across(configs: &[ConnectorConfig]) -> impl Iterator<Item = (&ConnectorConfig, &DeviceConfig)> {
    configs
        .iter()
        .flat_map(|config| config.devices.iter().map(move |device| (config, device)))
}

/// Devices that expose parameters without declaring a `type`, so their sets fall back to the
/// protocol — which every other device of every other type on that protocol also falls back to.
/// `manifest` warns about them; it is not an error, because a fleet of one type is fine.
pub fn devices_without_type(config: &ConnectorConfig) -> Vec<String> {
    untyped_devices_across(std::slice::from_ref(config), &config.connector.protocol)
}

/// [`devices_without_type`] over several configurations: the untyped devices of those speaking
/// `protocol`. Per protocol, because the set such a device falls back to — and so everything it
/// collides with — is named after it.
pub fn untyped_devices_across(configs: &[ConnectorConfig], protocol: &str) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for (config, device) in devices_across(configs) {
        let p = config.connector.protocol.as_str();
        if p != protocol || !device.device_type.as_deref().unwrap_or("").is_empty() {
            continue;
        }
        // A configuration that forces one set name derives none, so its devices cannot collide
        // over a name they never use — declaring a type would change nothing for them.
        if config.connector.parameter_set().is_some() {
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
/// Deliberately not reported for an absolute `parameter.set` shared by several device
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
    for (config, device) in devices_across(configs) {
        let protocol = config.connector.protocol.as_str();
        // As in `untyped_devices_across`: a forced set name is not derived from the type, so
        // two types folding to one derived name is not this configuration's problem.
        if config.connector.parameter_set().is_some() {
            continue;
        }
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

/// Parameter ids (and set names) that cannot be used as fragment keys.
pub fn invalid_keys(config: &ConnectorConfig) -> Vec<String> {
    invalid_keys_across(std::slice::from_ref(config))
}

/// [`invalid_keys`] over several configurations, in configuration order.
pub fn invalid_keys_across(configs: &[ConnectorConfig]) -> Vec<String> {
    configs
        .iter()
        .flat_map(parameters)
        .flat_map(|p| {
            let mut bad = Vec::new();
            if !is_valid_key(&p.point) {
                bad.push(format!("point id '{}'", p.point));
            }
            if !is_valid_key(&p.set) {
                bad.push(format!("parameter set '{}'", p.set));
            }
            bad
        })
        .collect()
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
  id = "commission_code"
  datatype = "uint16"
  access = "read_write"
  address = { table = "holding", address = 22, count = 1 }
  parameter = { group = "commissioning" }

  # In two groups at once: operators see it on the daily screen and the
  # commissioning one, and both fragments carry its value.
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

    fn cfg() -> ConnectorConfig {
        toml::from_str(CONFIG).unwrap()
    }

    /// The same configuration with `[connector] parameter_set` set: one name for every point
    /// that does not give an absolute one.
    fn cfg_forcing(set: &str) -> ConnectorConfig {
        toml::from_str(&CONFIG.replace(
            "protocol = \"modbus\"",
            &format!("protocol = \"modbus\"\nparameter_set = \"{set}\""),
        ))
        .unwrap()
    }

    /// A set name is qualified by the *device type*, because that is what decides which points
    /// exist; the protocol is only the fallback for a device that does not declare one. An
    /// absolute `parameter.set` is used verbatim, a `group` names a second set of the same
    /// device type.
    #[test]
    fn parameters_select_writable_and_opted_in_points() {
        let params = parameters(&cfg());
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
                // device types and is the reason `manifest` warns about it.
                ("spare_rw", "modbus_control_parameters"),
            ]
        );
        assert_eq!(params[3].access, Access::Read);
        assert_eq!(devices_without_type(&cfg()), vec!["plc2".to_string()]);
    }

    /// `[connector] parameter_set` forces one name for every point that does not give an
    /// absolute one — the escape hatch for a tenant identifier that predates the naming rule.
    #[test]
    fn forced_set_overrides_the_derived_name() {
        let params = parameters(&cfg_forcing("legacy_params"));
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
                // The forced name collapses a multi-group point to one set, once.
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
            sets(&point("parameter = { group = [\"control\", \"commissioning\"] }")),
            [
                "acme_boiler_v2_control_parameters",
                "acme_boiler_v2_commissioning_parameters"
            ]
        );
        assert_eq!(
            sets(&point("parameter = { set = [\"plant_a\", \"plant_b\"] }")),
            ["plant_a", "plant_b"]
        );
        // An absolute set still wins over the groups.
        assert_eq!(
            sets(&point(
                "parameter = { set = \"pump\", group = [\"a\", \"b\"] }"
            )),
            ["pump"]
        );
        // Group names that fold to the same set name yield one set, not two.
        assert_eq!(
            sets(&point("parameter = { group = [\"a b\", \"a-b\"] }")),
            ["acme_boiler_v2_a_b_parameters"]
        );
        // An empty or unusable list is an absent one: the default group, never no set at all.
        for meta in [
            "parameter = { group = [] }",
            "parameter = { group = [1, true] }",
            "parameter = { set = [] }",
        ] {
            assert_eq!(
                sets(&point(meta)),
                ["acme_boiler_v2_control_parameters"],
                "{meta}"
            );
        }
        // ...and opting out still beats every list.
        assert!(sets(&point("parameter = false")).is_empty());
    }

    /// Two device types that differ only in punctuation fold to one qualifier, so their sets
    /// collide exactly as two protocols' did before this feature — `manifest` says so.
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
        let bad = invalid_keys(&c);
        assert_eq!(bad, vec!["point id 'Boiler.Temp'"]);
        // A forced name is a fragment key too, so it is refused by the same rule.
        assert!(invalid_keys(&cfg_forcing("plant.floor"))
            .iter()
            .all(|b| b.contains("parameter set")));
        // Blank is "unset", not a set called "": an unexpanded variable in a provisioning
        // script must not rename every set to nothing.
        assert!(cfg_forcing("  ").connector.parameter_set().is_none());
        assert_eq!(cfg_forcing(" plant ").connector.parameter_set(), Some("plant"));
    }

    /// Several configurations at once (`manifest -c <dir>`): device types folding together
    /// collide across files, and the untyped-device warning is per protocol.
    #[test]
    fn warnings_span_every_config() {
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

        // Neither file collides on its own; together they do.
        assert!(type_warnings(&modbus).is_empty());
        let warnings = type_warnings_across(&both);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("'acme-boiler-v2', 'acme boiler v2'"), "{}", warnings[0]);

        assert_eq!(untyped_devices_across(&both, "modbus"), ["plc2"]);
        assert_eq!(untyped_devices_across(&both, "opcua"), ["opc1"]);
        assert_eq!(invalid_keys_across(&both), ["point id 'Tank.Level'"]);
    }
}
