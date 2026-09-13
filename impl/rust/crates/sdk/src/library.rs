//! Point libraries: reusable, protocol-scoped point lists a device references instead of
//! inlining (contract §3.4).
//!
//! One device type usually has the same data points on every instance, and only the
//! connection information differs. A **point library** is that point list in its own file,
//! with no connection information at all:
//!
//! ```toml
//! # /usr/share/tedge-dot/points.d/modbus/acme-meter-v2.toml
//! [library]
//! protocol = "modbus"
//!
//! [[point]]
//! id       = "boiler_temp"
//! datatype = "float32"
//! address  = { table = "holding", address = 7, count = 2 }
//! ```
//!
//! and a device names it:
//!
//! ```toml
//! [[device]]
//! name             = "plc-1"
//! protocol_address = { transport = "tcp", host = "192.168.0.10", port = 502, unit_id = 1 }
//! points_from      = ["acme-meter-v2"]
//! ```
//!
//! Resolution happens here, at load time, and produces a [`ConnectorConfig`] whose devices
//! carry fully-expanded point lists. Everything downstream — the runtime, the protocol
//! modules, `describe`, the `read`/`write` CLI — is therefore unchanged by this feature.
//!
//! Note what is *not* expanded: the raw configuration document the runtime keeps for the
//! management verbs (§6.3). It keeps the `points_from` reference, so patching and persisting a
//! config never bakes a library's points into the user's file, and `define-device` can define a
//! device by reference alone.

use crate::config::ConnectorConfig;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use toml::value::Table;
use toml::Value;

/// Site-local point libraries; shadow packaged ones of the same name.
pub const SITE_LIBRARY_DIR: &str = "/etc/tedge/plugins/ot/points.d";
/// Point libraries shipped by packages (the connector's own, or a vendor's device pack).
pub const PACKAGED_LIBRARY_DIR: &str = "/usr/share/tedge-dot/points.d";
/// Colon-separated search-path override, for running from a source checkout.
pub const LIBRARY_PATH_ENV: &str = "TEDGE_DOT_POINT_LIBRARY_PATH";

/// Keys a point library MUST NOT carry: their presence means the file is a connector
/// configuration, which is the mistake worth naming explicitly.
const CONNECTOR_ONLY_KEYS: [&str; 4] = ["connector", "mqtt", "connection", "device"];

/// Point fields that are merged key by key when a later definition overrides an earlier one.
/// Everything else — `address` included — is replaced wholesale, because a partial protocol
/// address is not a meaningful thing to inherit.
const DEEP_MERGED_KEYS: [&str; 2] = ["meta", "transform"];

// The keys a contract-level table may carry (§3.3). Anything else is refused, with the nearest
// known key suggested, so a misspelt setting — `polling_interval` for `poll_interval` — is
// reported instead of silently doing nothing. The protocol-specific objects (`connection`,
// `protocol_address`, `address`) and `meta` are free-form and not checked. The C loader
// (impl/c/sdk/src/config.c `check_keys`) refuses the same keys with the same message.
const TOP_KEYS: &[&str] = &["connector", "mqtt", "connection", "device"];
const CONNECTOR_KEYS: &[&str] = &[
    "protocol",
    "service_name",
    "poll_interval",
    "log_level",
    "operation_timeout",
    "stall_timeout",
    "point_library_path",
];
const MQTT_KEYS: &[&str] = &["host", "port"];
const DEVICE_KEYS: &[&str] = &[
    "name",
    "type",
    "protocol_address",
    "poll_interval",
    "default_mode",
    "points_from",
    "point",
    "enabled",
];
const POINT_KEYS: &[&str] = &[
    "id",
    "mode",
    "datatype",
    "endianness",
    "word_order",
    "poll_interval",
    "address",
    "access",
    "unit",
    "name",
    "description",
    "transform",
    "meta",
    "subscribe",
];
const TRANSFORM_KEYS: &[&str] = &["multiplier", "divisor", "decimal_shift", "offset"];
const LIBRARY_TOP_KEYS: &[&str] = &["library", "point"];
const LIBRARY_KEYS: &[&str] = &["protocol", "type", "description", "version"];

/// Refuse the first key of `table` that is not `known`, naming the table (`place`) and, when one
/// is close enough to be what was meant, the known key it most resembles. A value that is not a
/// table is left to the typed parse, whose message for a wrong shape is the canonical one.
fn check_keys(table: &Value, known: &[&str], place: &str) -> Result<(), String> {
    let Some(table) = table.as_table() else {
        return Ok(());
    };
    match table.keys().find(|key| !known.contains(&key.as_str())) {
        Some(key) => Err(unknown_key(key, known, place)),
        None => Ok(()),
    }
}

/// The message for an unknown key: the nearest known key is suggested when its edit distance is
/// at most a third of the key's length (and at least 2), so `polling_interval` suggests
/// `poll_interval` while an unrelated word suggests nothing.
fn unknown_key(key: &str, known: &[&str], place: &str) -> String {
    let limit = (key.len() / 3).max(2);
    let suggestion = known
        .iter()
        .map(|candidate| (edit_distance(key, candidate), *candidate))
        .min_by_key(|(distance, _)| *distance)
        .filter(|(distance, _)| *distance <= limit)
        .map(|(_, candidate)| format!(" (did you mean '{candidate}'?)"))
        .unwrap_or_default();
    format!("unknown key '{key}' in {place}{suggestion}")
}

/// Levenshtein distance over bytes (as the C loader computes it).
fn edit_distance(a: &str, b: &str) -> usize {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let substitute = prev[j] + usize::from(ca != cb);
            cur[j + 1] = substitute.min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// The keys of a connector configuration, table by table (§3.3). Checked on the document as
/// written, before anything is resolved — a disabled device's keys included, since a misspelt
/// key is a mistake whether or not the device is switched on.
fn check_document(doc: &Value) -> Result<(), String> {
    check_keys(doc, TOP_KEYS, "the top level")?;
    if let Some(connector) = doc.get("connector") {
        check_keys(connector, CONNECTOR_KEYS, "[connector]")?;
    }
    if let Some(mqtt) = doc.get("mqtt") {
        check_keys(mqtt, MQTT_KEYS, "[mqtt]")?;
    }
    for device in doc.get("device").and_then(Value::as_array).into_iter().flatten() {
        let name = device.get("name").and_then(Value::as_str).unwrap_or("<unnamed>");
        check_keys(device, DEVICE_KEYS, &format!("device '{name}'"))?;
        for point in device.get("point").and_then(Value::as_array).into_iter().flatten() {
            check_point_keys(point)?;
        }
    }
    Ok(())
}

/// The keys of one point definition, inline or in a point library, and of its transform.
fn check_point_keys(point: &Value) -> Result<(), String> {
    let id = point.get("id").and_then(Value::as_str).unwrap_or("<unnamed>");
    check_keys(point, POINT_KEYS, &format!("point '{id}'"))?;
    if let Some(transform) = point.get("transform") {
        check_keys(transform, TRANSFORM_KEYS, &format!("the transform of point '{id}'"))?;
    }
    Ok(())
}

/// Load a connector configuration file, resolving every device's point-library references.
///
/// Relative library paths resolve against the configuration file's own directory.
pub fn load(path: &Path) -> Result<ConnectorConfig, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("failed to read config '{}': {e}", path.display()))?;
    resolve(&text, config_base_dir(path))
        .map_err(|e| format!("failed to load config '{}': {e}", path.display()))
}

/// The directory relative library references in `config_path` resolve against.
pub fn config_base_dir(config_path: &Path) -> &Path {
    match config_path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir,
        _ => Path::new("."),
    }
}

/// Parse connector configuration text and expand every device's `points_from` references.
///
/// `base_dir` is what relative library paths resolve against (the configuration file's
/// directory; see [`config_base_dir`]).
pub fn resolve(text: &str, base_dir: &Path) -> Result<ConnectorConfig, String> {
    let mut doc: Value = toml::from_str(text).map_err(|e| format!("failed to parse config: {e}"))?;
    check_document(&doc)?;
    expand(&mut doc, base_dir)?;
    doc.try_into()
        .map_err(|e: toml::de::Error| format!("failed to parse config: {e}"))
}

/// Expand every device's `points_from` references in place, leaving a document whose devices
/// only carry inline points.
fn expand(doc: &mut Value, base_dir: &Path) -> Result<(), String> {
    // A document with no devices, or one so malformed that `[connector] protocol` is missing,
    // has nothing to expand: leave it to the typed parse, whose error message is the canonical
    // one for a broken config.
    let Some(protocol) = doc
        .get("connector")
        .and_then(|c| c.get("protocol"))
        .and_then(Value::as_str)
        .map(str::to_string)
    else {
        return Ok(());
    };
    // Validated even when nothing references a library, so a typo in the field is reported at
    // load instead of waiting for the first device that needs it. The C loader validates at the
    // same point, which is what keeps the two from accepting different files.
    let search_path = search_path(doc, base_dir)?;
    let Some(devices) = doc.get_mut("device").and_then(Value::as_array_mut) else {
        return Ok(());
    };

    // §3.3 requires device names to be unique within a connector. Unchecked, two same-named
    // devices publish over each other on one entity's topics — and they are what makes
    // "was this reference already here" ambiguous for the management guard.
    let mut seen: Vec<&str> = Vec::new();
    let mut normalised: Vec<(usize, String)> = Vec::new();
    let mut disabled: Vec<usize> = Vec::new();
    for (index, device) in devices.iter().enumerate() {
        let Some(name) = device.get("name").and_then(Value::as_str) else {
            continue; // a device without a name is the typed parse's error to report
        };
        if seen.contains(&name) {
            return Err(format!("device '{name}' is defined more than once"));
        }
        seen.push(name);
        // `enabled = false` (§3.3) takes the device out of the configuration before anything else
        // about it is read — its type, its address, its point libraries — so a config can carry a
        // ready-made device switched off, even one naming a library that is not installed yet.
        // Its name still counted above: switching it on must not produce a duplicate.
        match device.get("enabled") {
            None | Some(Value::Boolean(true)) => {}
            Some(Value::Boolean(false)) => {
                tracing::info!(device = %name, "device is disabled (enabled = false); not loaded");
                disabled.push(index);
                continue;
            }
            Some(_) => return Err(format!("device '{name}': enabled must be true or false")),
        }
        // Checked here rather than left to the typed parse, because an empty string would
        // otherwise be accepted as a type and silently behave like an absent one — and the C
        // loader must reject exactly the same files as this one.
        if let Some(declared) = device.get("type") {
            match declared.as_str().map(trim_c) {
                None | Some("") => {
                    return Err(format!("device '{name}': type must be a non-empty string"))
                }
                // Normalised once, here: the type is rendered in three places (the parameter
                // set names, the sample envelope and the link status) which must agree on its
                // exact spelling, so surrounding whitespace goes before anything reads it.
                Some(trimmed) => normalised.push((index, trimmed.to_string())),
            }
        }
    }
    for (index, device_type) in normalised {
        if let Some(table) = devices[index].as_table_mut() {
            table.insert("type".to_string(), Value::String(device_type));
        }
    }
    // Last in, first out, so the indices still to remove stay valid.
    for index in disabled.into_iter().rev() {
        devices.remove(index);
    }

    let mut cache: HashMap<PathBuf, Library> = HashMap::new();
    for device in devices {
        let name = device
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("<unnamed>")
            .to_string();
        let refs = device_refs(device, &name)?;
        if refs.is_empty() {
            continue;
        }

        let mut points: Vec<Value> = Vec::new();
        let mut device_type: Option<String> = None;
        for reference in &refs {
            let path = locate(reference, &protocol, base_dir, &search_path)
                .map_err(|e| format!("device '{name}': {e}"))?;
            let library = match cache.get(&path) {
                Some(library) => library,
                None => {
                    let parsed = read_library(&path, &protocol)?;
                    cache.entry(path.clone()).or_insert(parsed)
                }
            };
            // The device type comes from the *first* library that names one: later references
            // extend a type rather than redefine it (`["acme-meter-v2", "site-extras"]`).
            if device_type.is_none() {
                device_type.clone_from(&library.device_type);
            }
            for point in &library.points {
                merge_point(&mut points, point.clone());
            }
        }
        // Inline points come last: a device's own definition wins over every library it
        // references, which is what makes extending a packaged list possible without editing
        // the packaged file.
        //
        // A `point` key that is present but not an array is a mistake, not an absence: the
        // resolved list is written back over this key, so treating it as absent would silently
        // discard a mistyped `[[device.point]]` (an inline table, say) that the typed parse
        // would otherwise have rejected.
        let inline = match device.get_mut("point") {
            Some(Value::Array(points)) => std::mem::take(points),
            Some(_) => {
                return Err(format!(
                    "device '{name}': point must be an array of tables ([[device.point]])"
                ))
            }
            None => Vec::new(),
        };
        let inline_count = inline.len();
        for point in inline {
            merge_point(&mut points, point);
        }

        tracing::info!(
            device = %name,
            libraries = ?refs,
            points = points.len(),
            inline = inline_count,
            "resolved device points from point libraries"
        );
        let table = device
            .as_table_mut()
            .ok_or_else(|| format!("device '{name}' is not a table"))?;
        // A device that does not declare its own type inherits the library's (§3.1). Written
        // into the expanded document rather than resolved later, so everything reading the
        // typed config — `describe`, the runtime, a connector — sees one resolved type.
        if let Some(device_type) = device_type {
            table
                .entry("type".to_string())
                .or_insert(Value::String(device_type));
        }
        table.insert("point".to_string(), Value::Array(points));
    }
    Ok(())
}

/// The `points_from` references declared by one device.
fn device_refs(device: &Value, name: &str) -> Result<Vec<String>, String> {
    let Some(value) = device.get("points_from") else {
        return Ok(Vec::new());
    };
    let array = value.as_array().ok_or_else(|| {
        format!("device '{name}': points_from must be an array of point-library names or paths")
    })?;
    array
        .iter()
        .map(|entry| {
            entry
                .as_str()
                .map(str::to_string)
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| {
                    format!(
                        "device '{name}': points_from entries must be non-empty strings \
                         (point-library names or paths)"
                    )
                })
        })
        .collect()
}

/// The directories a bare library name is looked up in, most specific first.
///
/// An explicit `point_library_path` replaces the default path entirely (§3.4). It must name at
/// least one directory: an empty or unusable list would leave nothing to resolve names
/// against, which is a configuration mistake rather than a way to express "no libraries" —
/// and reading it as "unset" instead is what made the C loader disagree with this one.
fn search_path(doc: &Value, base_dir: &Path) -> Result<Vec<PathBuf>, String> {
    if let Some(configured) = doc.get("connector").and_then(|c| c.get("point_library_path")) {
        let dirs = configured.as_array().ok_or(
            "[connector] point_library_path must be an array of directories",
        )?;
        if dirs.is_empty() {
            return Err(
                "[connector] point_library_path is empty; name at least one directory or \
                 remove it to use the default path"
                    .to_string(),
            );
        }
        return dirs
            .iter()
            .map(|dir| {
                dir.as_str().map(|dir| absolutise(dir, base_dir)).ok_or_else(|| {
                    "[connector] point_library_path entries must be directory strings".to_string()
                })
            })
            .collect();
    }
    if let Some(env) = std::env::var_os(LIBRARY_PATH_ENV) {
        let env = env.to_string_lossy().into_owned();
        let dirs: Vec<PathBuf> = env
            .split(':')
            .filter(|s| !s.is_empty())
            .map(|dir| absolutise(dir, base_dir))
            .collect();
        if !dirs.is_empty() {
            return Ok(dirs);
        }
    }
    Ok(vec![
        PathBuf::from(SITE_LIBRARY_DIR),
        PathBuf::from(PACKAGED_LIBRARY_DIR),
    ])
}

fn absolutise(dir: &str, base_dir: &Path) -> PathBuf {
    let path = Path::new(dir);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base_dir.join(path)
    }
}

/// True when a `points_from` entry is a path rather than a library name.
pub fn is_path_reference(reference: &str) -> bool {
    reference.ends_with(".toml") || reference.contains('/')
}

/// Reject path references that a management command (§6.3) *introduced*.
///
/// A `points_from` entry is allowed to be a path in a configuration file, which only root or
/// `tedge` can edit. A command is a different trust boundary: anything that can publish on the
/// broker could otherwise name an arbitrary path and read the resolver's verdict — whether the
/// file exists, and, through a parse error, a line of its contents — out of the retained
/// command result. Names are all a discovery mechanism needs, so only names are accepted from
/// there.
///
/// Only what the command changed is judged: the path references a device already had stay
/// legal, so an unrelated `set-config` (or a `remove-device`) on a configuration that uses the
/// path form still works.
pub fn reject_path_references(before: &str, after: &str) -> Result<(), String> {
    let parse = |text: &str| -> Result<Value, String> {
        toml::from_str(text).map_err(|e| format!("failed to parse config: {e}"))
    };
    let existing = path_references(&parse(before)?);
    for (device, reference) in path_references(&parse(after)?) {
        if !existing.contains(&(device.clone(), reference.clone())) {
            return Err(format!(
                "device '{device}': points_from '{reference}' is a path; a management \
                 command may only name a point library, not a path"
            ));
        }
    }
    Ok(())
}

/// Every `(device, reference)` pair in `doc` whose reference is a path.
fn path_references(doc: &Value) -> Vec<(String, String)> {
    let Some(devices) = doc.get("device").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for device in devices {
        let name = device
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("<unnamed>");
        let refs = device
            .get("points_from")
            .and_then(Value::as_array)
            .map(|r| r.as_slice())
            .unwrap_or_default();
        for reference in refs.iter().filter_map(Value::as_str) {
            if is_path_reference(reference) {
                found.push((name.to_string(), reference.to_string()));
            }
        }
    }
    found
}

/// Turn one `points_from` entry into the file it names.
fn locate(
    reference: &str,
    protocol: &str,
    base_dir: &Path,
    search_path: &[PathBuf],
) -> Result<PathBuf, String> {
    if is_path_reference(reference) {
        let path = absolutise(reference, base_dir);
        if !path.is_file() {
            return Err(format!(
                "point library '{reference}' not found at {}",
                path.display()
            ));
        }
        return Ok(path);
    }
    // A bare name is protocol-scoped: libraries carry protocol-specific addressing, so each
    // protocol gets its own subdirectory and the same device name can exist for several.
    let mut tried = Vec::new();
    for dir in search_path {
        let candidate = dir.join(protocol).join(format!("{reference}.toml"));
        if candidate.is_file() {
            return Ok(candidate);
        }
        tried.push(candidate.display().to_string());
    }
    if tried.is_empty() {
        return Err(format!(
            "cannot resolve point library '{reference}': the library search path is empty"
        ));
    }
    Err(format!(
        "unknown point library '{reference}' for protocol '{protocol}' (looked for {})",
        tried.join(", ")
    ))
}

/// The whitespace C's `isspace()` recognises, which is what the C loader trims and rejects
/// with. Rust's own `str::trim` also strips Unicode spaces, so a type containing a non-breaking
/// space would be trimmed here and kept there — one configuration file, two different
/// tenant-wide parameter set names depending on which package is installed.
fn is_c_whitespace(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\x0B' | '\x0C' | '\r')
}

/// `str::trim`, but with the C loader's definition of whitespace.
pub fn trim_c(s: &str) -> &str {
    s.trim_matches(is_c_whitespace)
}

/// One parsed point library: the device type it describes, and its points.
struct Library {
    /// `[library] type`, the device type these points belong to (§3.4). Absent when the
    /// library does not name one — the file name is deliberately not used instead, because
    /// this ends up as a tenant-wide identifier in the cloud (§5.2) and so is worth declaring.
    device_type: Option<String>,
    /// The `[[point]]` entries in declaration order.
    points: Vec<Value>,
}

/// Read and validate one point library, returning its points in declaration order.
fn read_library(path: &Path, protocol: &str) -> Result<Library, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("failed to read point library '{}': {e}", path.display()))?;
    let doc: Value = toml::from_str(&text)
        .map_err(|e| format!("failed to parse point library '{}': {e}", path.display()))?;
    let where_ = path.display();

    let table = doc
        .as_table()
        .ok_or_else(|| format!("point library '{where_}' is not a TOML table"))?;
    if let Some(key) = CONNECTOR_ONLY_KEYS
        .iter()
        .find(|key| table.contains_key(**key))
    {
        return Err(format!(
            "'{where_}' is a connector configuration, not a point library (it has a [{key}] \
             section); a point library holds only [library] and [[point]]"
        ));
    }
    // The known keys (§3.3), after the check above: a connector configuration pointed at by
    // mistake deserves that message rather than "unknown key 'connector'".
    check_keys(&doc, LIBRARY_TOP_KEYS, &format!("point library '{where_}'"))?;
    if let Some(library) = table.get("library") {
        check_keys(library, LIBRARY_KEYS, &format!("[library] of point library '{where_}'"))?;
    }
    let device_type = table
        .get("library")
        .and_then(|l| l.get("type"))
        .map(|t| {
            t.as_str()
                // Normalised like the device's own type: one spelling, everywhere.
                .map(|t| trim_c(t).to_string())
                .filter(|t| !t.is_empty())
                .ok_or_else(|| {
                    format!("point library '{where_}': [library] type must be a non-empty string")
                })
        })
        .transpose()?;
    let declared = table
        .get("library")
        .and_then(|l| l.get("protocol"))
        .and_then(Value::as_str)
        .ok_or_else(|| {
            format!("point library '{where_}' is missing [library] protocol = \"<protocol>\"")
        })?;
    if declared != protocol {
        return Err(format!(
            "point library '{where_}' is for protocol '{declared}', not '{protocol}'"
        ));
    }

    let points = match table.get("point") {
        Some(points) => points.as_array().ok_or_else(|| {
            format!("point library '{where_}': [[point]] must be an array of tables")
        })?,
        None => &Vec::new(),
    };
    // An empty list is as unusable as a missing one, and far more dangerous: a device would
    // resolve to zero points, come up healthy and publish nothing (§3.4). This is what a
    // generated library that found nothing looks like.
    if points.is_empty() {
        return Err(format!(
            "point library '{where_}' declares no [[point]] entries"
        ));
    }

    // Within one library a repeated id is a mistake, not an override: there is no meaningful
    // order to apply and the second definition would silently win.
    let mut seen: Vec<&str> = Vec::new();
    for point in points {
        check_point_keys(point).map_err(|e| format!("point library '{where_}': {e}"))?;
        let id = point
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("point library '{where_}': a point is missing its id"))?;
        if seen.contains(&id) {
            return Err(format!(
                "point library '{where_}' declares point '{id}' twice"
            ));
        }
        seen.push(id);
    }
    Ok(Library {
        device_type,
        points: points.clone(),
    })
}

/// Add `incoming` to `points`, or merge it into the existing point with the same id.
fn merge_point(points: &mut Vec<Value>, incoming: Value) {
    let id = incoming.get("id").and_then(Value::as_str).map(str::to_string);
    let existing = id.and_then(|id| {
        points
            .iter_mut()
            .find(|p| p.get("id").and_then(Value::as_str) == Some(id.as_str()))
    });
    match existing {
        Some(base) => {
            if let (Some(base), Some(over)) = (base.as_table_mut(), incoming.as_table()) {
                merge_point_table(base, over);
            } else {
                *base = incoming;
            }
        }
        None => points.push(incoming),
    }
}

/// Apply an overriding point definition onto an inherited one: `meta` and `transform` merge
/// key by key so a single field can be adjusted, everything else replaces.
fn merge_point_table(base: &mut Table, over: &Table) {
    for (key, value) in over {
        let deep = DEEP_MERGED_KEYS.contains(&key.as_str());
        match (deep, base.get_mut(key), value) {
            (true, Some(Value::Table(base)), Value::Table(over)) => merge_table(base, over),
            _ => {
                base.insert(key.clone(), value.clone());
            }
        }
    }
}

/// Recursive table merge, so `meta.parameter.title` can be set without restating the rest of
/// `meta.parameter`.
fn merge_table(base: &mut Table, over: &Table) {
    for (key, value) in over {
        match (base.get_mut(key), value) {
            (Some(Value::Table(base)), Value::Table(over)) => merge_table(base, over),
            _ => {
                base.insert(key.clone(), value.clone());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// A temp directory that cleans itself up (the SDK has no dev-dependency on tempfile).
    struct Dir(PathBuf);

    impl Dir {
        fn new(tag: &str) -> Dir {
            let base = std::env::temp_dir().join(format!(
                "tdot-library-{tag}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = fs::remove_dir_all(&base);
            fs::create_dir_all(&base).unwrap();
            Dir(base)
        }

        /// Write `contents` to `rel` inside the directory, creating parents.
        fn write(&self, rel: &str, contents: &str) -> PathBuf {
            let path = self.0.join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, contents).unwrap();
            path
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    const LIBRARY: &str = r#"
[library]
protocol = "modbus"

[[point]]
id          = "boiler_temp"
datatype    = "float32"
address     = { table = "holding", address = 7, count = 2 }
unit        = "°C"
name        = "Boiler temp"
description = "Outlet temperature after the heat exchanger"

[[point]]
id       = "pump_run"
datatype = "bool"
access   = "read_write"
address  = { table = "coil", address = 0, count = 1 }
"#;

    /// A connector config whose single device references `refs` and inlines `inline`.
    /// `search` pins `point_library_path`, so no test depends on /etc or the environment.
    fn config_with(search: Option<&Path>, refs: &str, inline: &str) -> String {
        let search = search
            .map(|dir| format!("point_library_path = [\"{}\"]\n", dir.display()))
            .unwrap_or_default();
        format!(
            r#"
[connector]
protocol = "modbus"
{search}
[[device]]
name             = "plc-1"
protocol_address = {{ transport = "tcp", host = "10.0.0.1", port = 502, unit_id = 1 }}
points_from      = [{refs}]
{inline}
"#
        )
    }

    /// A config with no search path of its own (for the environment/default-path tests).
    fn config(refs: &str, inline: &str) -> String {
        config_with(None, refs, inline)
    }

    /// Resolve a single-device config whose libraries live under `dir`.
    fn resolve_in(dir: &Path, refs: &str, inline: &str) -> Result<ConnectorConfig, String> {
        resolve(&config_with(Some(dir), refs, inline), dir)
    }

    #[test]
    fn device_inherits_points_from_a_named_library() {
        let dir = Dir::new("named");
        dir.write("modbus/acme-meter.toml", LIBRARY);

        let cfg = resolve_in(dir.path(), "\"acme-meter\"", "").unwrap();
        let device = &cfg.devices[0];
        assert_eq!(device.points_from, vec!["acme-meter".to_string()]);
        let ids: Vec<&str> = device.points.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, ["boiler_temp", "pump_run"]);
        assert_eq!(device.points[0].unit.as_deref(), Some("°C"));
        assert_eq!(device.points[1].access.as_deref(), Some("read_write"));
    }

    /// A library is the point list of one device *type*, so it is where the type is named
    /// (§3.1): every instance that references it inherits it, and its parameter sets are named
    /// after it rather than after the protocol. A device's own `type` wins, and a second
    /// library extends the type rather than redefining it.
    #[test]
    fn device_inherits_the_type_of_the_first_library_that_names_one() {
        let dir = Dir::new("type");
        dir.write(
            "modbus/acme-meter.toml",
            &LIBRARY.replace("[library]\n", "[library]\ntype = \"acme-meter-v2\"\n"),
        );
        dir.write(
            "modbus/site-extras.toml",
            "[library]\nprotocol = \"modbus\"\ntype = \"site-extras\"\n\n[[point]]\nid = \"spare\"\ndatatype = \"bool\"\naddress = { table = \"coil\", address = 9, count = 1 }\n",
        );
        dir.write("modbus/untyped.toml", LIBRARY);

        let cfg = resolve_in(dir.path(), "\"acme-meter\", \"site-extras\"", "").unwrap();
        assert_eq!(
            cfg.devices[0].device_type.as_deref(),
            Some("acme-meter-v2"),
            "the first library that names a type gives it; later ones extend it"
        );

        // The device's own declaration wins over the library's.
        let text = resolve_in(dir.path(), "\"acme-meter\"", "").unwrap();
        assert_eq!(text.devices[0].device_type.as_deref(), Some("acme-meter-v2"));
        let own = resolve(
            &config_with(Some(dir.path()), "\"acme-meter\"", "").replace(
                "points_from",
                "type             = \"site-special\"\npoints_from",
            ),
            dir.path(),
        )
        .unwrap();
        assert_eq!(own.devices[0].device_type.as_deref(), Some("site-special"));

        // A library that names no type leaves the device without one (the file name is not
        // guessed at: the type ends up as a tenant-wide identifier in the cloud).
        let none = resolve_in(dir.path(), "\"untyped\"", "").unwrap();
        assert_eq!(none.devices[0].device_type, None);
    }

    /// A padded type is normalised at load, so the set names, the sample envelope and the link
    /// status cannot spell it differently from one another (they all read the stored value).
    /// Whitespace means what C's `isspace()` means, so the C loader normalises identically —
    /// a non-breaking space is *not* whitespace and stays part of the name in both.
    #[test]
    fn a_declared_type_is_trimmed_once_at_load() {
        let dir = Dir::new("type-trim");
        dir.write(
            "modbus/acme-meter.toml",
            &LIBRARY.replace("[library]\n", "[library]\ntype = \"  acme-meter-v2 \"\n"),
        );
        let inherited = resolve_in(dir.path(), "\"acme-meter\"", "").unwrap();
        assert_eq!(
            inherited.devices[0].device_type.as_deref(),
            Some("acme-meter-v2")
        );

        let own = resolve(
            &config_with(Some(dir.path()), "\"acme-meter\"", "").replace(
                "points_from",
                "type             = \" site-special \"\npoints_from",
            ),
            dir.path(),
        )
        .unwrap();
        assert_eq!(own.devices[0].device_type.as_deref(), Some("site-special"));

        // A non-breaking space is not whitespace to C's isspace(), so it must survive here too:
        // trimming it would give the two builds different set names for one configuration.
        // Written as the literal character, not a `\u` escape, because the C loader's TOML
        // parser mis-reads `\u00a0acme` (it consumes hex digits greedily) — which is also why
        // the mirrored C test writes the same bytes.
        let nbsp = resolve(
            &config_with(Some(dir.path()), "\"acme-meter\"", "").replace(
                "points_from",
                "type             = \"\u{a0}acme\u{a0}\"\npoints_from",
            ),
            dir.path(),
        )
        .unwrap();
        assert_eq!(
            nbsp.devices[0].device_type.as_deref(),
            Some("\u{a0}acme\u{a0}")
        );
    }

    #[test]
    fn device_type_must_be_a_non_empty_string() {
        let dir = Dir::new("device-type-invalid");
        dir.write("modbus/acme-meter.toml", LIBRARY);
        // An array or a table is *present but unusable*, not absent — the case the C loader's
        // scalar-only presence check used to drop silently. `\u{b}` is a vertical tab, which
        // C counts as whitespace and Rust's own `str::trim` does not.
        for bad in ["\"\"", "\"  \"", "\"\\u000B\"", "7", "true", "[\"acme\"]", "{ a = 1 }"] {
            let text = config_with(Some(dir.path()), "\"acme-meter\"", "").replace(
                "points_from",
                &format!("type             = {bad}\npoints_from"),
            );
            let err = resolve(&text, dir.path()).unwrap_err();
            assert!(err.contains("type must be a non-empty string"), "{bad}: {err}");
        }
    }

    #[test]
    fn library_type_must_be_a_non_empty_string() {
        let dir = Dir::new("type-invalid");
        for bad in ["\"\"", "\"  \"", "\"\\u000B\"", "7", "true", "[\"acme\"]", "{ a = 1 }"] {
            dir.write(
                "modbus/bad.toml",
                &LIBRARY.replace("[library]\n", &format!("[library]\ntype = {bad}\n")),
            );
            let err = resolve_in(dir.path(), "\"bad\"", "").unwrap_err();
            assert!(err.contains("[library] type"), "{bad}: {err}");
        }
    }

    #[test]
    fn library_is_looked_up_under_the_connector_protocol() {
        let dir = Dir::new("protocol-scope");
        // Same library name for two protocols; only the connector's own must be picked up.
        dir.write("modbus/shared-name.toml", LIBRARY);
        dir.write(
            "opcua/shared-name.toml",
            "[library]\nprotocol = \"opcua\"\n\n[[point]]\nid = \"wrong\"\ndatatype = \"bool\"\naddress = { node = \"ns=1;i=1\" }\n",
        );

        let cfg = resolve_in(dir.path(), "\"shared-name\"", "").unwrap();
        let ids: Vec<&str> = cfg.devices[0].points.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, ["boiler_temp", "pump_run"]);
    }

    #[test]
    fn earlier_search_path_entries_shadow_later_ones() {
        let dir = Dir::new("shadow");
        dir.write(
            "site/modbus/acme-meter.toml",
            "[library]\nprotocol = \"modbus\"\n\n[[point]]\nid = \"site_point\"\ndatatype = \"bool\"\naddress = { table = \"coil\", address = 1, count = 1 }\n",
        );
        dir.write("packaged/modbus/acme-meter.toml", LIBRARY);

        let text = format!(
            r#"
[connector]
protocol           = "modbus"
point_library_path = ["{site}", "{packaged}"]

[[device]]
name             = "plc-1"
protocol_address = {{ transport = "tcp", host = "10.0.0.1", port = 502, unit_id = 1 }}
points_from      = ["acme-meter"]
"#,
            site = dir.path().join("site").display(),
            packaged = dir.path().join("packaged").display(),
        );
        let cfg = resolve(&text, dir.path()).unwrap();
        let ids: Vec<&str> = cfg.devices[0].points.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, ["site_point"], "the site copy must shadow the packaged one");
    }

    #[test]
    fn relative_path_references_resolve_against_the_config_directory() {
        let dir = Dir::new("relpath");
        dir.write("lists/meter.toml", LIBRARY);

        let cfg = resolve(&config("\"lists/meter.toml\"", ""), dir.path()).unwrap();
        assert_eq!(cfg.devices[0].points.len(), 2);
    }

    #[test]
    fn absolute_path_references_are_used_verbatim() {
        let dir = Dir::new("abspath");
        let path = dir.write("meter.toml", LIBRARY);

        let cfg = resolve(
            &config(&format!("\"{}\"", path.display()), ""),
            Path::new("/nonexistent"),
        )
        .unwrap();
        assert_eq!(cfg.devices[0].points.len(), 2);
    }

    #[test]
    fn several_libraries_are_applied_in_order() {
        let dir = Dir::new("multi");
        dir.write("modbus/base.toml", LIBRARY);
        dir.write(
            "modbus/extras.toml",
            "[library]\nprotocol = \"modbus\"\n\n[[point]]\nid = \"site_extra\"\ndatatype = \"uint16\"\naddress = { table = \"holding\", address = 99, count = 1 }\n",
        );

        let cfg = resolve_in(dir.path(), "\"base\", \"extras\"", "").unwrap();
        let ids: Vec<&str> = cfg.devices[0].points.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, ["boiler_temp", "pump_run", "site_extra"]);
    }

    #[test]
    fn a_later_library_patches_a_point_it_repeats() {
        let dir = Dir::new("patch");
        dir.write("modbus/base.toml", LIBRARY);
        dir.write(
            "modbus/tweaks.toml",
            "[library]\nprotocol = \"modbus\"\n\n[[point]]\nid = \"boiler_temp\"\npoll_interval = \"30s\"\n",
        );

        let cfg = resolve_in(dir.path(), "\"base\", \"tweaks\"", "").unwrap();
        let points = &cfg.devices[0].points;
        assert_eq!(points.len(), 2, "a patch must not add a second point");
        assert_eq!(points[0].poll_interval.as_deref(), Some("30s"));
        // Untouched fields are inherited, so a patch need not restate the address.
        assert_eq!(points[0].unit.as_deref(), Some("°C"));
        assert_eq!(points[0].address["address"], serde_json::json!(7));
    }

    #[test]
    fn inline_points_extend_and_override_the_libraries() {
        let dir = Dir::new("inline");
        dir.write("modbus/acme-meter.toml", LIBRARY);

        let inline = r#"
  [[device.point]]
  id   = "boiler_temp"
  unit = "K"

  [[device.point]]
  id       = "local_only"
  datatype = "uint16"
  address  = { table = "holding", address = 42, count = 1 }
"#;
        let cfg = resolve_in(dir.path(), "\"acme-meter\"", inline).unwrap();
        let points = &cfg.devices[0].points;
        let ids: Vec<&str> = points.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, ["boiler_temp", "pump_run", "local_only"]);
        assert_eq!(points[0].unit.as_deref(), Some("K"), "inline wins over the library");
        assert_eq!(
            points[0].datatype,
            Some(crate::model::DataType::Float32),
            "and inherits what it does not restate"
        );
    }

    #[test]
    fn meta_and_transform_merge_but_address_replaces() {
        let dir = Dir::new("merge-rules");
        dir.write(
            "modbus/base.toml",
            r#"
[library]
protocol = "modbus"

[[point]]
id        = "flow"
datatype  = "uint16"
address   = { table = "holding", address = 7, count = 1 }
transform = { multiplier = 2, decimal_shift = -3 }
meta      = { on_change = true, deadband = 0.5, parameter = { title = "Flow", min = 0 } }
"#,
        );

        let inline = r#"
  [[device.point]]
  id        = "flow"
  address   = { table = "input", address = 9 }
  transform = { decimal_shift = -1 }
  meta      = { deadband = 2.0, parameter = { max = 100 } }
"#;
        let cfg = resolve_in(dir.path(), "\"base\"", inline).unwrap();
        let point = &cfg.devices[0].points[0];

        // address replaces wholesale: a half-inherited protocol address is not meaningful.
        assert_eq!(point.address, serde_json::json!({ "table": "input", "address": 9 }));
        // transform merges key by key.
        let transform = point.transform.unwrap();
        assert_eq!(transform.multiplier, 2.0);
        assert_eq!(transform.decimal_shift, -1);
        // meta merges recursively.
        let meta = point.meta.as_ref().unwrap();
        assert_eq!(meta["on_change"], serde_json::json!(true));
        assert_eq!(meta["deadband"], serde_json::json!(2.0));
        assert_eq!(meta["parameter"]["title"], serde_json::json!("Flow"));
        assert_eq!(meta["parameter"]["max"], serde_json::json!(100));
    }

    #[test]
    fn devices_without_references_are_untouched() {
        let dir = Dir::new("no-refs");
        let text = r#"
[connector]
protocol = "modbus"

[[device]]
name             = "plc-1"
protocol_address = { transport = "tcp", host = "10.0.0.1", port = 502, unit_id = 1 }

  [[device.point]]
  id       = "only"
  datatype = "uint16"
  address  = { table = "holding", address = 1, count = 1 }
"#;
        let cfg = resolve(text, dir.path()).unwrap();
        assert!(cfg.devices[0].points_from.is_empty());
        assert_eq!(cfg.devices[0].points.len(), 1);
    }

    /// `enabled = false` (§3.3) takes a device out of the configuration before anything else
    /// about it is read. Mirrors `check_disabled_devices` in impl/c/tests/config.c.
    #[test]
    fn a_disabled_device_is_left_out_without_resolving_its_libraries() {
        let dir = Dir::new("disabled");
        let text = format!(
            r#"
[connector]
protocol = "modbus"
point_library_path = ["{}"]

[[device]]
name             = "plc-1"
enabled          = true
protocol_address = {{ transport = "tcp", host = "10.0.0.1", port = 502, unit_id = 1 }}

  [[device.point]]
  id       = "only"
  datatype = "uint16"
  address  = {{ table = "holding", address = 1, count = 1 }}

# Nothing about it is valid beyond its name, and nothing has to be.
[[device]]
name        = "plc-2"
enabled     = false
type        = ""
points_from = ["not-installed"]
"#,
            dir.path().display()
        );
        let cfg = resolve(&text, dir.path()).unwrap();
        let names: Vec<&str> = cfg.devices.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["plc-1"]);
    }

    #[test]
    fn enabled_must_be_a_boolean_and_disabled_names_still_count() {
        let dir = Dir::new("enabled-shape");
        let device =
            |name: &str, extra: &str| format!("[[device]]\nname = \"{name}\"\n{extra}\nprotocol_address = {{ unit_id = 1 }}\n");
        let err = resolve(
            &format!("[connector]\nprotocol = \"modbus\"\n{}", device("plc-1", "enabled = \"no\"")),
            dir.path(),
        )
        .unwrap_err();
        assert!(err.contains("enabled must be true or false"), "{err}");

        // Switching the disabled one on would make two devices of one name.
        let err = resolve(
            &format!(
                "[connector]\nprotocol = \"modbus\"\n{}{}",
                device("plc-1", ""),
                device("plc-1", "enabled = false")
            ),
            dir.path(),
        )
        .unwrap_err();
        assert!(err.contains("defined more than once"), "{err}");
    }

    /// A key the contract does not define is refused (§3.3), naming the table and — when one is
    /// close enough to be what was meant — the known key: `polling_interval` must not be accepted
    /// and silently ignored. Mirrors `check_unknown_keys` in impl/c/tests/config.c.
    #[test]
    fn unknown_keys_are_refused_with_the_key_they_resemble() {
        let dir = Dir::new("unknown-keys");
        let config = |connector: &str, device: &str, point: &str| {
            format!(
                "[connector]\nprotocol = \"modbus\"\n{connector}\n\
                 [[device]]\nname = \"plc-1\"\nprotocol_address = {{ unit_id = 1 }}\n{device}\n\
                 [[device.point]]\nid = \"t\"\ndatatype = \"uint16\"\naddress = {{ address = 1 }}\n{point}\n"
            )
        };
        let refused = |text: String| resolve(&text, dir.path()).unwrap_err();

        assert_eq!(
            refused(config("", "polling_interval = \"10s\"", "")),
            "unknown key 'polling_interval' in device 'plc-1' (did you mean 'poll_interval'?)"
        );
        assert_eq!(
            refused(config("log_levle = \"debug\"", "", "")),
            "unknown key 'log_levle' in [connector] (did you mean 'log_level'?)"
        );
        assert_eq!(
            refused(config("", "", "datatyp = \"uint16\"")),
            "unknown key 'datatyp' in point 't' (did you mean 'datatype'?)"
        );
        assert_eq!(
            refused(config("", "", "transform = { multiplyer = 2 }")),
            "unknown key 'multiplyer' in the transform of point 't' (did you mean 'multiplier'?)"
        );
        assert_eq!(
            refused(format!("{}[mqqt]\nhost = \"broker\"\n", config("", "", ""))),
            "unknown key 'mqqt' in the top level (did you mean 'mqtt'?)"
        );
        // Nothing close enough to be what was meant: no suggestion.
        assert_eq!(
            refused(config("", "colour = \"red\"", "")),
            "unknown key 'colour' in device 'plc-1'"
        );
        // A switched-off device's keys are checked too: a misspelt key is a mistake either way.
        assert_eq!(
            refused(config("", "enabled = false\nenabeld = true", "")),
            "unknown key 'enabeld' in device 'plc-1' (did you mean 'enabled'?)"
        );
        // The free-form objects are not checked.
        resolve(
            &config("[connection]\nwhatever = 1", "", "meta = { anything = 1 }"),
            dir.path(),
        )
        .expect("connection, protocol_address, address and meta are free-form");
    }

    #[test]
    fn unknown_keys_in_a_point_library_are_refused() {
        let dir = Dir::new("unknown-library-keys");
        dir.write(
            "modbus/acme.toml",
            "[library]\nprotocol = \"modbus\"\nvendor = \"acme\"\n\n[[point]]\nid = \"t\"\naddress = {}\n",
        );
        let err = resolve_in(dir.path(), "\"acme\"", "").unwrap_err();
        assert!(err.contains("unknown key 'vendor' in [library] of point library"), "{err}");

        dir.write(
            "modbus/acme.toml",
            "[library]\nprotocol = \"modbus\"\n\n[[point]]\nid = \"t\"\naddress = {}\nunits = \"K\"\n",
        );
        let err = resolve_in(dir.path(), "\"acme\"", "").unwrap_err();
        assert!(err.contains("unknown key 'units' in point 't' (did you mean 'unit'?)"), "{err}");
    }

    #[test]
    fn unknown_library_names_the_places_it_looked() {
        let dir = Dir::new("missing");
        let err = resolve_in(dir.path(), "\"nope\"", "").unwrap_err();
        assert!(err.contains("unknown point library 'nope'"), "{err}");
        assert!(err.contains("modbus/nope.toml"), "{err}");
        assert!(err.contains("device 'plc-1'"), "{err}");
    }

    #[test]
    fn missing_path_reference_is_reported_with_the_path() {
        let dir = Dir::new("missing-path");
        let err = resolve(&config("\"lists/gone.toml\"", ""), dir.path()).unwrap_err();
        assert!(err.contains("lists/gone.toml"), "{err}");
    }

    #[test]
    fn protocol_mismatch_is_rejected() {
        let dir = Dir::new("mismatch");
        dir.write(
            "modbus/wrong.toml",
            "[library]\nprotocol = \"opcua\"\n\n[[point]]\nid = \"x\"\ndatatype = \"bool\"\naddress = { node = \"ns=1;i=1\" }\n",
        );
        let err = resolve_in(dir.path(), "\"wrong\"", "").unwrap_err();
        assert!(err.contains("is for protocol 'opcua', not 'modbus'"), "{err}");
    }

    #[test]
    fn library_without_a_protocol_is_rejected() {
        let dir = Dir::new("no-protocol");
        dir.write(
            "modbus/bare.toml",
            "[[point]]\nid = \"x\"\ndatatype = \"bool\"\naddress = { table = \"coil\", address = 1, count = 1 }\n",
        );
        let err = resolve_in(dir.path(), "\"bare\"", "").unwrap_err();
        assert!(err.contains("missing [library] protocol"), "{err}");
    }

    #[test]
    fn pointing_at_a_connector_config_is_rejected_by_name() {
        let dir = Dir::new("not-a-library");
        dir.write("modbus/oops.toml", &config("", ""));
        let err = resolve_in(dir.path(), "\"oops\"", "").unwrap_err();
        assert!(err.contains("is a connector configuration, not a point library"), "{err}");
    }

    #[test]
    fn library_with_no_points_is_rejected() {
        let dir = Dir::new("empty");
        dir.write("modbus/empty.toml", "[library]\nprotocol = \"modbus\"\n");
        let err = resolve_in(dir.path(), "\"empty\"", "").unwrap_err();
        assert!(err.contains("declares no [[point]] entries"), "{err}");
    }

    #[test]
    fn duplicate_id_within_one_library_is_rejected() {
        let dir = Dir::new("dup");
        dir.write(
            "modbus/dup.toml",
            "[library]\nprotocol = \"modbus\"\n\n[[point]]\nid = \"x\"\ndatatype = \"bool\"\naddress = { table = \"coil\", address = 1, count = 1 }\n\n[[point]]\nid = \"x\"\ndatatype = \"bool\"\naddress = { table = \"coil\", address = 2, count = 1 }\n",
        );
        let err = resolve_in(dir.path(), "\"dup\"", "").unwrap_err();
        assert!(err.contains("declares point 'x' twice"), "{err}");
    }

    #[test]
    fn a_patch_that_never_names_a_base_point_must_still_be_valid_on_its_own() {
        let dir = Dir::new("orphan-patch");
        dir.write(
            "modbus/tweaks.toml",
            "[library]\nprotocol = \"modbus\"\n\n[[point]]\nid = \"stray\"\ndatatype = \"uint16\"\npoll_interval = \"30s\"\n",
        );
        // No base library defines `stray`, so it stays a point with no address: the typed
        // parse must reject it rather than the connector failing later.
        let err = resolve_in(dir.path(), "\"tweaks\"", "").unwrap_err();
        assert!(err.contains("address"), "{err}");
    }

    #[test]
    fn malformed_points_from_is_reported() {
        let dir = Dir::new("bad-refs");
        let text = r#"
[connector]
protocol = "modbus"

[[device]]
name             = "plc-1"
protocol_address = { transport = "tcp", host = "10.0.0.1", port = 502, unit_id = 1 }
points_from      = "acme-meter"
"#;
        let err = resolve(text, dir.path()).unwrap_err();
        assert!(err.contains("points_from must be an array"), "{err}");

        let text = text.replace("\"acme-meter\"", "[42]");
        let err = resolve(&text, dir.path()).unwrap_err();
        assert!(err.contains("non-empty strings"), "{err}");
    }

    #[test]
    fn one_library_shared_by_several_devices_is_read_once_and_applied_to_each() {
        let dir = Dir::new("shared");
        dir.write("modbus/acme-meter.toml", LIBRARY);
        let text = format!(
            r#"
[connector]
protocol           = "modbus"
point_library_path = ["{dir}"]

[[device]]
name             = "plc-1"
protocol_address = {{ transport = "tcp", host = "10.0.0.1", port = 502, unit_id = 1 }}
points_from      = ["acme-meter"]

[[device]]
name             = "plc-2"
protocol_address = {{ transport = "tcp", host = "10.0.0.2", port = 502, unit_id = 1 }}
points_from      = ["acme-meter"]

  [[device.point]]
  id   = "boiler_temp"
  unit = "K"
"#,
            dir = dir.path().display()
        );
        let cfg = resolve(&text, dir.path()).unwrap();
        assert_eq!(cfg.devices[0].points.len(), 2);
        assert_eq!(cfg.devices[1].points.len(), 2);
        // The per-device override must not leak into the other device's copy.
        assert_eq!(cfg.devices[0].points[0].unit.as_deref(), Some("°C"));
        assert_eq!(cfg.devices[1].points[0].unit.as_deref(), Some("K"));
    }

    #[test]
    fn the_environment_provides_the_search_path_when_the_config_does_not() {
        let dir = Dir::new("env");
        dir.write("modbus/acme-meter.toml", LIBRARY);
        // Serialized with the other env-reading test by the guard in `env_lock`.
        let _guard = env_lock();
        std::env::set_var(LIBRARY_PATH_ENV, dir.path());
        let resolved = resolve(&config("\"acme-meter\"", ""), dir.path());
        std::env::remove_var(LIBRARY_PATH_ENV);
        assert_eq!(resolved.unwrap().devices[0].points.len(), 2);
    }

    #[test]
    fn an_explicit_search_path_wins_over_the_environment() {
        let dir = Dir::new("env-override");
        dir.write("wanted/modbus/acme-meter.toml", LIBRARY);
        dir.write(
            "ignored/modbus/acme-meter.toml",
            "[library]\nprotocol = \"modbus\"\n\n[[point]]\nid = \"from_env\"\ndatatype = \"bool\"\naddress = { table = \"coil\", address = 1, count = 1 }\n",
        );
        let text = format!(
            r#"
[connector]
protocol           = "modbus"
point_library_path = ["{}"]

[[device]]
name             = "plc-1"
protocol_address = {{ transport = "tcp", host = "10.0.0.1", port = 502, unit_id = 1 }}
points_from      = ["acme-meter"]
"#,
            dir.path().join("wanted").display()
        );
        let _guard = env_lock();
        std::env::set_var(LIBRARY_PATH_ENV, dir.path().join("ignored"));
        let resolved = resolve(&text, dir.path());
        std::env::remove_var(LIBRARY_PATH_ENV);
        let ids: Vec<String> = resolved
            .unwrap()
            .devices
            .remove(0)
            .points
            .iter()
            .map(|p| p.id.clone())
            .collect();
        assert_eq!(ids, ["boiler_temp", "pump_run"]);
    }

    /// Resolution writes the merged list back over the device's `point` key, so a key that is
    /// present but not an array must be reported rather than treated as absent — otherwise
    /// adding a `points_from` reference silently removes the validation the typed parse does,
    /// and a mistyped inline point disappears without a word.
    #[test]
    fn a_malformed_point_key_is_reported_not_discarded() {
        let dir = Dir::new("bad-point-key");
        dir.write("modbus/acme-meter.toml", LIBRARY);

        for value in ["\"typo\"", "42", "{ id = \"temp\" }"] {
            let text = format!(
                r#"
[connector]
protocol           = "modbus"
point_library_path = ["{dir}"]

[[device]]
name             = "plc-1"
protocol_address = {{ transport = "tcp", host = "10.0.0.1", port = 502, unit_id = 1 }}
points_from      = ["acme-meter"]
point            = {value}
"#,
                dir = dir.path().display()
            );
            let err = resolve(&text, dir.path()).unwrap_err();
            assert!(err.contains("must be an array of tables"), "{value}: {err}");
        }
    }

    /// §3.3: unique within a connector. Two same-named devices would publish over each other
    /// on one entity's topics, and they make "was this reference already here" ambiguous for
    /// the management guard — which is where the two implementations drifted apart.
    /// `name` and `description` (§3.1) are ordinary scalars, so they patch like `unit` does:
    /// a site can relabel a point it inherited without restating its address or its other
    /// label. This is the whole point of putting a description in a shared list.
    #[test]
    fn labels_are_inherited_and_patch_one_at_a_time() {
        let dir = Dir::new("labels");
        dir.write("modbus/acme-meter.toml", LIBRARY);

        // Inherited as declared.
        let cfg = resolve_in(dir.path(), "\"acme-meter\"", "").unwrap();
        let point = &cfg.devices[0].points[0];
        assert_eq!(point.name.as_deref(), Some("Boiler temp"));
        assert_eq!(
            point.description.as_deref(),
            Some("Outlet temperature after the heat exchanger")
        );

        // A site relabels just the short name; the description and everything else stay.
        let inline = r#"
  [[device.point]]
  id   = "boiler_temp"
  name = "Flow temp (site label)"
"#;
        let cfg = resolve_in(dir.path(), "\"acme-meter\"", inline).unwrap();
        let point = &cfg.devices[0].points[0];
        assert_eq!(point.name.as_deref(), Some("Flow temp (site label)"));
        assert_eq!(
            point.description.as_deref(),
            Some("Outlet temperature after the heat exchanger"),
            "patching the name must not drop the inherited description"
        );
        assert_eq!(point.unit.as_deref(), Some("°C"));
        assert_eq!(point.datatype, Some(crate::model::DataType::Float32));
    }

    #[test]
    fn a_repeated_device_name_is_rejected() {
        let dir = Dir::new("dup-device");
        let text = r#"
[connector]
protocol = "modbus"

[[device]]
name             = "plc-1"
protocol_address = { transport = "tcp", host = "10.0.0.1", port = 502, unit_id = 1 }

  [[device.point]]
  id       = "a"
  datatype = "uint16"
  address  = { table = "holding", address = 1, count = 1 }

[[device]]
name             = "plc-1"
protocol_address = { transport = "tcp", host = "10.0.0.2", port = 502, unit_id = 1 }

  [[device.point]]
  id       = "b"
  datatype = "uint16"
  address  = { table = "holding", address = 2, count = 1 }
"#;
        let err = resolve(text, dir.path()).unwrap_err();
        assert!(err.contains("defined more than once"), "{err}");
        assert!(err.contains("plc-1"), "{err}");

        // Distinct names are of course fine.
        let ok = text.replacen(r#"name             = "plc-1""#, r#"name             = "plc-2""#, 1);
        assert_eq!(resolve(&ok, dir.path()).unwrap().devices.len(), 2);
    }

    /// An explicit search path replaces the default, so it has to name somewhere. Reading an
    /// empty list as "unset" instead is what made the C loader resolve a config this one
    /// rejected — the same file starting under one package and failing under the other.
    #[test]
    fn an_explicit_but_empty_search_path_is_a_configuration_error() {
        let dir = Dir::new("empty-path");
        dir.write("modbus/acme-meter.toml", LIBRARY);
        let text = r#"
[connector]
protocol           = "modbus"
point_library_path = []

[[device]]
name             = "plc-1"
protocol_address = { transport = "tcp", host = "10.0.0.1", port = 502, unit_id = 1 }
points_from      = ["acme-meter"]
"#;
        // Set the env var too: falling back to it is precisely the behaviour being ruled out.
        let _guard = env_lock();
        std::env::set_var(LIBRARY_PATH_ENV, dir.path());
        let resolved = resolve(text, dir.path());
        std::env::remove_var(LIBRARY_PATH_ENV);
        let err = resolved.expect_err("an empty search path must not fall back");
        assert!(err.contains("point_library_path is empty"), "{err}");
    }

    #[test]
    fn a_malformed_search_path_is_a_configuration_error() {
        let dir = Dir::new("bad-path");
        for (value, needle) in [
            ("\"/etc/points.d\"", "must be an array"),
            ("[42]", "must be directory strings"),
        ] {
            let text = format!(
                r#"
[connector]
protocol           = "modbus"
point_library_path = {value}

[[device]]
name             = "plc-1"
protocol_address = {{ transport = "tcp", host = "10.0.0.1", port = 502, unit_id = 1 }}
points_from      = ["acme-meter"]
"#
            );
            let err = resolve(&text, dir.path()).unwrap_err();
            assert!(err.contains(needle), "{value}: {err}");
        }
    }

    /// A library that declares an empty list, not a missing one. Left to resolve, the device
    /// would come up healthy with no points and publish nothing — the failure mode §3.4 calls
    /// out, and what a generated library that found nothing looks like.
    #[test]
    fn a_library_with_an_empty_point_list_is_rejected() {
        let dir = Dir::new("empty-points");
        dir.write("modbus/generated.toml", "point = []\n\n[library]\nprotocol = \"modbus\"\n");
        let err = resolve_in(dir.path(), "\"generated\"", "").unwrap_err();
        assert!(err.contains("declares no [[point]] entries"), "{err}");
    }

    /// A configuration that arrived over MQTT may name a library, never a path: otherwise
    /// anything able to publish on the broker could read the resolver's verdict on an
    /// arbitrary path — existence, and through a parse error a line of the file — out of the
    /// retained command result.
    /// The field is validated even by a config that references no library at all: a typo
    /// should be reported at load, and the C loader validates at the same point — when one
    /// was eager and the other lazy, the same file loaded under one package and failed under
    /// the other.
    #[test]
    fn the_search_path_is_validated_even_when_no_device_uses_a_library() {
        let dir = Dir::new("eager");
        let text = r#"
[connector]
protocol           = "modbus"
point_library_path = []

[[device]]
name             = "plc-1"
protocol_address = { transport = "tcp", host = "10.0.0.1", port = 502, unit_id = 1 }

  [[device.point]]
  id       = "inline_only"
  datatype = "uint16"
  address  = { table = "holding", address = 3, count = 1 }
"#;
        let err = resolve(text, dir.path()).unwrap_err();
        assert!(err.contains("point_library_path is empty"), "{err}");

        // ...and a usable one still loads a library-free config.
        let text = text.replace("point_library_path = []", "point_library_path = [\".\"]");
        let cfg = resolve(&text, dir.path()).expect("a usable search path");
        assert_eq!(cfg.devices[0].points.len(), 1);
    }

    #[test]
    fn management_supplied_path_references_are_refused() {
        let with = |reference: &str| {
            format!(
                r#"
[connector]
protocol = "modbus"

[[device]]
name             = "plc-1"
protocol_address = {{ transport = "tcp", host = "10.0.0.1", port = 502, unit_id = 1 }}
points_from      = ["{reference}"]
"#
            )
        };
        let none = "[connector]\nprotocol = \"modbus\"\n";
        for reference in ["./secret/creds.env", "../../etc/shadow", "list.toml", "/etc/hosts"] {
            let err = reject_path_references(none, &with(reference))
                .expect_err("a path reference the command added must be refused");
            assert!(err.contains("is a path"), "{reference}: {err}");
            assert!(err.contains("plc-1"), "{reference}: {err}");
        }
        // A plain name is what discovery actually needs, and stays allowed.
        reject_path_references(none, &with("acme-meter-v2")).expect("a name is fine");
        // As does a config with no references at all.
        reject_path_references(none, none).expect("no devices");
    }

    /// A path reference is legal in a configuration *file* (§3.4). Judging the whole candidate
    /// rather than what the command changed made every management verb — an unrelated
    /// `set-config`, even `remove-device` — fail on such a config.
    #[test]
    fn a_path_reference_already_in_the_config_does_not_block_management() {
        let before = r#"
[connector]
protocol = "modbus"

[[device]]
name = "plc-1"
protocol_address = { transport = "tcp", host = "10.0.0.1", port = 502, unit_id = 1 }
points_from = ["./lists/meter.toml"]
"#;
        // An unrelated set-config: the pre-existing path reference is untouched.
        let after = before.replace("[connector]", "[connector]\npoll_interval = \"5s\"");
        reject_path_references(before, &after)
            .expect("a pre-existing path reference must not block an unrelated command");

        // The same document unchanged (what remove-device on another device looks like).
        reject_path_references(before, before).expect("nothing was introduced");

        // But adding a *new* path reference to that same config is still refused.
        let after = before.replace(
            r#"points_from = ["./lists/meter.toml"]"#,
            r#"points_from = ["./lists/meter.toml", "../../etc/hosts"]"#,
        );
        let err = reject_path_references(before, &after)
            .expect_err("a newly introduced path must still be refused");
        assert!(err.contains("../../etc/hosts"), "{err}");

        // ...as is moving one to a different device.
        let after = format!(
            "{before}\n[[device]]\nname = \"plc-2\"\nprotocol_address = {{ unit_id = 2 }}\n\
             points_from = [\"./lists/meter.toml\"]\n"
        );
        let err = reject_path_references(before, &after)
            .expect_err("the same path on another device is a new reference");
        assert!(err.contains("plc-2"), "{err}");
    }

    /// `set_var` is process-global: every test that sets OR depends on the default value of
    /// `TEDGE_DOT_POINT_LIBRARY_PATH` must hold this. Tests that pin `point_library_path`, or
    /// that only use path references, never read the variable and need no guard.
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn load_resolves_relative_to_the_config_file() {
        let dir = Dir::new("load");
        dir.write("points.d/modbus/acme-meter.toml", LIBRARY);
        let config_path = dir.write(
            "etc/modbus.toml",
            r#"
[connector]
protocol           = "modbus"
point_library_path = ["../points.d"]

[[device]]
name             = "plc-1"
protocol_address = { transport = "tcp", host = "10.0.0.1", port = 502, unit_id = 1 }
points_from      = ["acme-meter"]
"#,
        );
        let cfg = load(&config_path).unwrap();
        assert_eq!(cfg.devices[0].points.len(), 2);
    }

    #[test]
    fn load_reports_the_config_path_on_failure() {
        let dir = Dir::new("load-fail");
        let config_path = dir.write("modbus.toml", &config("\"nope\"", ""));
        let err = load(&config_path).unwrap_err();
        assert!(err.contains("modbus.toml"), "{err}");
    }

    #[test]
    fn path_references_are_told_apart_from_names() {
        assert!(is_path_reference("lists/meter.toml"));
        assert!(is_path_reference("meter.toml"));
        assert!(is_path_reference("/opt/meter"));
        assert!(!is_path_reference("acme-meter-v2"));
        // Exactly ".toml" is a path too. The C check guarded on `n > 5` and classified it as a
        // name, which split the two implementations — and with them the management guard.
        assert!(is_path_reference(".toml"));
        assert!(!is_path_reference("toml"));
        assert!(!is_path_reference(".tom"));
    }

    mod props {
        use super::*;
        use proptest::prelude::*;

        /// Point ids drawn from a small alphabet, so collisions (the interesting case) are
        /// common rather than rare.
        fn ids() -> impl Strategy<Value = Vec<String>> {
            prop::collection::vec(prop_oneof!["a", "b", "c"].prop_map(String::from), 0..6)
        }

        fn library(ids: &[String], unit: &str) -> String {
            let mut text = String::from("[library]\nprotocol = \"modbus\"\n");
            for (i, id) in ids.iter().enumerate() {
                text.push_str(&format!(
                    "\n[[point]]\nid = \"{id}\"\ndatatype = \"uint16\"\nunit = \"{unit}\"\naddress = {{ table = \"holding\", address = {i}, count = 1 }}\n"
                ));
            }
            text
        }

        proptest! {
            /// Whatever the overlap between two libraries, the result has exactly one point per
            /// distinct id, and every repeated id takes the *later* library's value. Getting
            /// this wrong is how a packaged list would silently win over a site override.
            #[test]
            fn later_libraries_win_and_ids_stay_unique(first in ids(), second in ids()) {
                // Duplicates within one library are rejected by design; dedupe each side.
                let dedupe = |v: &Vec<String>| {
                    let mut out: Vec<String> = Vec::new();
                    for id in v {
                        if !out.contains(id) {
                            out.push(id.clone());
                        }
                    }
                    out
                };
                let first = dedupe(&first);
                let second = dedupe(&second);
                prop_assume!(!first.is_empty() && !second.is_empty());

                let dir = Dir::new("prop");
                dir.write("modbus/first.toml", &library(&first, "first"));
                dir.write("modbus/second.toml", &library(&second, "second"));

                let cfg = resolve_in(dir.path(), "\"first\", \"second\"", "").unwrap();
                let points = &cfg.devices[0].points;

                let mut expected = first.clone();
                for id in &second {
                    if !expected.contains(id) {
                        expected.push(id.clone());
                    }
                }
                let got: Vec<String> = points.iter().map(|p| p.id.clone()).collect();
                prop_assert_eq!(&got, &expected, "order must be first-seen, ids unique");

                for point in points {
                    let want = if second.contains(&point.id) { "second" } else { "first" };
                    prop_assert_eq!(point.unit.as_deref(), Some(want));
                }
            }
        }
    }
}
