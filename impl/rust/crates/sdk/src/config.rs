//! Contract-level configuration model (protocol-neutral). The protocol-specific objects
//! (`connection`, `device.protocol_address`, `point.address`) are kept as raw JSON values and
//! parsed by the connector module in `configure`.

use crate::model::{DataType, Transform};
use serde::Deserialize;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ConnectorConfig {
    pub connector: ConnectorSection,
    #[serde(default)]
    pub mqtt: MqttSection,
    /// Protocol-specific shared connection defaults (opaque to the contract).
    #[serde(default)]
    pub connection: serde_json::Value,
    #[serde(rename = "device", default)]
    pub devices: Vec<DeviceConfig>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ConnectorSection {
    pub protocol: String,
    /// The configured service name; read it through [`ConnectorSection::service_name`], which
    /// applies the default.
    #[serde(rename = "service_name", default)]
    pub(crate) configured_service_name: Option<String>,
    #[serde(default = "default_poll_interval")]
    pub poll_interval: String,
    #[serde(default = "default_log_level")]
    pub log_level: String,
    /// Upper bound on a single protocol-module call (read batch, write, connect, subscribe).
    /// A module that never returns — what a half-open TCP socket produces: no answer, no
    /// error, no RST — would otherwise block the connector's loop forever, stopping samples,
    /// health and link status with nothing logged. The runtime turns the bound into an
    /// ordinary transport error, so the usual degraded-link and reconnect handling applies.
    #[serde(default = "default_operation_timeout")]
    pub operation_timeout: String,
    /// How long the connector's loop may make no progress before it is considered wedged and
    /// restarted (the supervisor cancels and re-runs it; the MQTT last will marks the service
    /// down so the cloud sees the outage). `"0"` disables the watchdog.
    #[serde(default = "default_stall_timeout")]
    pub stall_timeout: String,
    /// Put the wire back on every sample envelope: `raw` (the bytes read, hex) and `addr` (the
    /// protocol-specific address). Off by default — both are static or debugging detail on a
    /// message a point publishes thousands of times a day (§5) — and switched on by the tooling
    /// that inspects the wire, above all the conformance suite.
    #[serde(default)]
    pub sample_debug: bool,
    /// Extra thin-edge command types this connector answers, each mapping to one it already
    /// implements: `command_aliases = { ot_write_coil = "ot_write" }` (§6.6).
    ///
    /// The alias exists for the Cumulocity mapper, not for the protocol: two operation
    /// templates cannot share one `workflow.operation` — the mapper picks the first and warns
    /// — so `c8y_SetCoil` needs a command type of its own for as long as it is a separate
    /// operation. Keeping that in configuration is what stops a cloud workaround from being
    /// baked into a protocol-neutral runtime.
    #[serde(default)]
    pub command_aliases: std::collections::BTreeMap<String, String>,
    /// One parameter set name for every point that does not name an absolute one, instead of
    /// the derived `<device type, else protocol>_<group>_parameters` (RFC 0005, §5.2).
    ///
    /// This was `describe --set` plus the `ot-parameter-state` flow's `default_set`: the same
    /// decision written down twice, in two places that had to agree or the twin fragment and
    /// the registered definition would carry different names. The connector resolves the sets
    /// once, onto the manifest, so the flow and the CLI both read the answer instead of
    /// recomputing it.
    ///
    /// Blank is "unset", as an empty `default_set` was: an unexpanded variable in a
    /// provisioning script must not force every point into a nameless set.
    #[serde(rename = "parameter_set", default)]
    pub(crate) configured_parameter_set: Option<String>,
    /// Directories searched for the point libraries devices name in `points_from`
    /// ([`crate::library`]). Unset means the built-in path: the site directory
    /// `/etc/tedge/plugins/ot/points.d` first, then the packaged
    /// `/usr/share/tedge-dot/points.d`. Relative entries resolve against the configuration
    /// file's own directory.
    #[serde(default)]
    pub point_library_path: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct MqttSection {
    #[serde(default = "default_mqtt_host")]
    pub host: String,
    #[serde(default = "default_mqtt_port")]
    pub port: u16,
}

impl Default for MqttSection {
    fn default() -> Self {
        MqttSection {
            host: default_mqtt_host(),
            port: default_mqtt_port(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct DeviceConfig {
    pub name: String,
    /// Protocol-specific device address (opaque to the contract).
    pub protocol_address: serde_json::Value,
    /// The *device type* this instance is one of (§3.1): what its point list describes, not
    /// where it is. Declared here, or inherited from the first point library the device
    /// references (§3.4) — a library is the point list of one device type, so it is the
    /// natural place to name it.
    ///
    /// It qualifies the names of the device's parameter sets (§5.2), which are tenant-wide
    /// identifiers in the cloud: two device types on the same protocol have different points
    /// and so must not share a set name. It is also echoed in samples and link status, so the
    /// registration flow can use it as the thin-edge entity type.
    #[serde(rename = "type", default)]
    pub device_type: Option<String>,
    #[serde(default)]
    pub poll_interval: Option<String>,
    /// Point libraries this device inherits its points from, in order (§3.4). Names are
    /// resolved against the library search path; entries containing `/` or ending in `.toml`
    /// are paths, relative ones against the configuration file's directory. Resolution
    /// happens in [`crate::library`] when the configuration is loaded, so by the time a
    /// connector sees this config `points` already holds the fully-resolved list and this
    /// field is only a record of where it came from.
    #[serde(default)]
    pub points_from: Vec<String>,
    #[serde(rename = "point", default)]
    pub points: Vec<PointConfig>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct PointConfig {
    pub id: String,
    /// The point's datatype (§4) — required. `bytes` is the raw case, so a point needs no
    /// second field to say what kind of thing it is.
    pub datatype: DataType,
    #[serde(default)]
    pub endianness: Option<String>,
    #[serde(default)]
    pub word_order: Option<String>,
    #[serde(default)]
    pub poll_interval: Option<String>,
    /// Protocol-specific point address (opaque to the contract).
    pub address: serde_json::Value,
    #[serde(default)]
    pub access: Option<String>,
    #[serde(default)]
    pub unit: Option<String>,
    /// Short human-readable label for this signal, for where a name is displayed instead of the
    /// `id` (which is a topic segment and a fragment key, so it stays a plain identifier).
    /// Feeds a parameter's DTM title and the device manifest (§8.2).
    #[serde(default)]
    pub name: Option<String>,
    /// Longer human-readable explanation of what this signal is. Feeds a parameter's DTM
    /// description and the device manifest (§8.2). Neither this nor
    /// `name` is echoed per sample: they are static, so they are published once, retained.
    #[serde(default)]
    pub description: Option<String>,
    /// Optional per-point linear transform applied by the connector after decode.
    #[serde(default)]
    pub transform: Option<Transform>,
    /// Engineering-unit bounds of the signal (§5.3). Enforced by the runtime on **write**: a
    /// value outside them fails before the device is touched. Reads are not altered — a
    /// reading outside `range` is still `good`, because the device really says so.
    #[serde(default)]
    pub range: Option<Range>,
    /// Per-signal publish policy (§5.4), applied by the runtime to this point's sample stream:
    /// publish only on change, outside a deadband, no more often than `min_interval`, and only
    /// once the value has been stable for `debounce`.
    #[serde(default)]
    pub publish: Option<PublishPolicy>,
    /// Where the signal lands as a measurement (§5.5): `false` to keep it out of the
    /// measurements altogether, or a table with `group` / `series`. Kept as a raw value
    /// because both a boolean and a table are legal; the loader validates the table's keys.
    #[serde(default)]
    pub measurement: Option<serde_json::Value>,
    /// Exposure of the signal as an operator-editable setting (§5.2): `false`, `true`, a string
    /// naming the set, or a table (`group`, `set`, `title`, `description`, `enum`, `default`,
    /// `order`). Kept as a raw value because all four shapes are legal; the loader validates
    /// the table's keys, and `min`/`max` now live in `range`.
    #[serde(default)]
    pub parameter: Option<serde_json::Value>,
    /// Free-form signal metadata — *yours*. Published verbatim on the device manifest (§8.2)
    /// so a site's own flow can find it; never interpreted by the connector or the runtime.
    ///
    /// The conventions that used to live here — `on_change`, `deadband`, `min_interval`,
    /// `debounce`, `measurement` and `parameter` — are the typed fields above since 0.2, and
    /// the loader warns for one release when it finds them here (§5).
    #[serde(default)]
    pub meta: Option<serde_json::Value>,
    /// Set to `false` to keep this point on the polling schedule even when the connector
    /// supports push delivery (`subscribe`). Defaults to push when available.
    #[serde(default)]
    pub subscribe: Option<bool>,
}

/// Engineering-unit bounds of a signal (§5.3). Either end may be omitted, which leaves that
/// side unbounded — a setpoint with a floor and no ceiling is an ordinary thing to declare.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Range {
    #[serde(default)]
    pub min: Option<f64>,
    #[serde(default)]
    pub max: Option<f64>,
}

impl Range {
    /// Why `value` is not allowed, or `None` when it is. The message names the point, so it
    /// reads as a reason on a failed command.
    pub fn reject(&self, point: &str, value: f64) -> Option<String> {
        let below = self.min.is_some_and(|min| value < min);
        let above = self.max.is_some_and(|max| value > max);
        if !below && !above {
            return None;
        }
        let bounds = match (self.min, self.max) {
            (Some(min), Some(max)) => format!("[{min}, {max}]"),
            (Some(min), None) => format!("[{min}, ∞)"),
            (None, Some(max)) => format!("(-∞, {max}]"),
            (None, None) => return None,
        };
        Some(format!("value {value} outside range {bounds} of {point}"))
    }
}

/// Per-signal publish policy (§5.4). Every field is optional; an undeclared one means the
/// point is published on every read, which is the 0.1 behaviour and the default.
#[derive(Debug, Clone, PartialEq, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct PublishPolicy {
    /// Publish only when the value differs from the last published one.
    #[serde(default)]
    pub on_change: Option<bool>,
    /// How much a numeric value must differ to count as changed. Implies `on_change`.
    #[serde(default)]
    pub deadband: Option<f64>,
    /// Never publish more often than this (duration string).
    #[serde(default)]
    pub min_interval: Option<String>,
    /// Publish a new value only once it has been stable for this long. Implies `on_change`.
    #[serde(default)]
    pub debounce: Option<String>,
}

impl PublishPolicy {
    /// True when the policy asks for nothing, so the point is published on every read.
    pub fn is_noop(&self) -> bool {
        self.deadband.unwrap_or(0.0) <= 0.0
            && !self.on_change.unwrap_or(false)
            && self.min_interval.as_deref().and_then(parse_duration).is_none()
            && self.debounce.as_deref().and_then(parse_duration).is_none()
    }

    /// Change detection is on when it is asked for directly, or implied by a deadband or a
    /// debounce — both of which are meaningless without it.
    pub fn on_change(&self) -> bool {
        self.on_change.unwrap_or(false)
            || self.deadband.unwrap_or(0.0) > 0.0
            || self.debounce.as_deref().and_then(parse_duration).is_some()
    }
}

impl ConnectorSection {
    /// The connector's service name: `service_name` as configured, else `tedge-dot-<protocol>`.
    ///
    /// The default carries the protocol because connectors of different protocols run from one
    /// configuration directory and must not share a service (health, capability descriptor and
    /// the management command topic, contract §6.3, all hang off it).
    pub fn service_name(&self) -> String {
        self.configured_service_name
            .clone()
            .unwrap_or_else(|| format!("tedge-dot-{}", self.protocol))
    }

    /// The configured `parameter_set`, or `None` when it is absent or blank.
    ///
    /// Trimmed with the C definition of whitespace, the one both loaders share, so an
    /// unexpanded `parameter_set = "$PARAM_SET"` in a provisioning script does not become a
    /// set called `"$PARAM_SET"` and a padded one does not become a different set from the
    /// same name written without the padding.
    pub fn parameter_set(&self) -> Option<&str> {
        self.configured_parameter_set
            .as_deref()
            .map(crate::library::trim_c)
            .filter(|s| !s.is_empty())
    }
}
fn default_poll_interval() -> String {
    "2s".to_string()
}
fn default_log_level() -> String {
    "info".to_string()
}
fn default_operation_timeout() -> String {
    "30s".to_string()
}
fn default_stall_timeout() -> String {
    "120s".to_string()
}
fn default_mqtt_host() -> String {
    "127.0.0.1".to_string()
}
fn default_mqtt_port() -> u16 {
    1883
}

/// Parse a thin-edge duration string (`"500ms"`, `"2s"`, `"5m"`). Falls back to seconds for a
/// bare number. Negative, NaN and overflowing values yield `None` — config values arrive from
/// hand-edited files and remote `set-config` commands, so this must never panic.
pub fn parse_duration(s: &str) -> Option<Duration> {
    let s = s.trim();
    if let Some(rest) = s.strip_suffix("ms") {
        return rest.trim().parse::<u64>().ok().map(Duration::from_millis);
    }
    let (rest, scale) = if let Some(rest) = s.strip_suffix('s') {
        (rest, 1.0)
    } else if let Some(rest) = s.strip_suffix('m') {
        (rest, 60.0)
    } else if let Some(rest) = s.strip_suffix('h') {
        (rest, 3600.0)
    } else {
        (s, 1.0)
    };
    let secs = rest.trim().parse::<f64>().ok()? * scale;
    Duration::try_from_secs_f64(secs).ok()
}

impl ConnectorConfig {
    /// The stall watchdog's limit for this connector: `[connector] stall_timeout` (120s when it
    /// does not parse), zero when disabled with `"0"`, and never less than twice
    /// `operation_timeout` (30s when it does not parse), so one slow but legitimate call is not
    /// read as a hang. The C loader derives `stall_timeout_s` the same way.
    pub fn stall_limit(&self) -> Duration {
        let configured =
            parse_duration(&self.connector.stall_timeout).unwrap_or(Duration::from_secs(120));
        if configured.is_zero() {
            return Duration::ZERO;
        }
        let operation =
            parse_duration(&self.connector.operation_timeout).unwrap_or(Duration::from_secs(30));
        configured.max(operation.saturating_mul(2))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeout_defaults_are_sane_and_overridable() {
        let cfg: ConnectorConfig = toml::from_str(
            r#"
[connector]
protocol = "modbus"
"#,
        )
        .unwrap();
        assert_eq!(parse_duration(&cfg.connector.operation_timeout), Some(Duration::from_secs(30)));
        assert_eq!(parse_duration(&cfg.connector.stall_timeout), Some(Duration::from_secs(120)));

        let cfg: ConnectorConfig = toml::from_str(
            r#"
[connector]
protocol = "modbus"
operation_timeout = "3s"
stall_timeout = "0"
"#,
        )
        .unwrap();
        assert_eq!(parse_duration(&cfg.connector.operation_timeout), Some(Duration::from_secs(3)));
        // "0" disables the watchdog
        assert_eq!(parse_duration(&cfg.connector.stall_timeout), Some(Duration::ZERO));
    }

    /// The service name addresses the connector's management commands (contract §6.3), and the
    /// flows default to `tedge-dot-<protocol>` when a command names none: the connector default
    /// must be that same name, whatever the protocol.
    #[test]
    fn service_name_defaults_to_the_protocol_service() {
        let cfg: ConnectorConfig = toml::from_str("[connector]\nprotocol = \"opcua\"\n").unwrap();
        assert_eq!(cfg.connector.service_name(), "tedge-dot-opcua");

        let cfg: ConnectorConfig =
            toml::from_str("[connector]\nprotocol = \"opcua\"\nservice_name = \"plant-a\"\n")
                .unwrap();
        assert_eq!(cfg.connector.service_name(), "plant-a");
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("500ms"), Some(Duration::from_millis(500)));
        assert_eq!(parse_duration("2s"), Some(Duration::from_secs(2)));
        assert_eq!(parse_duration("5m"), Some(Duration::from_secs(300)));
        assert_eq!(parse_duration("2h"), Some(Duration::from_secs(7200)));
        assert_eq!(parse_duration("3"), Some(Duration::from_secs(3)));
    }

    /// Found by the `config_toml` fuzz target: negative/NaN/overflowing durations used to
    /// panic in `Duration::from_secs_f64`.
    #[test]
    fn invalid_durations_are_none_not_panics() {
        assert_eq!(parse_duration("-66"), None);
        assert_eq!(parse_duration("-5s"), None);
        assert_eq!(parse_duration("NaN"), None);
        assert_eq!(parse_duration("inf"), None);
        assert_eq!(parse_duration("1e300h"), None);
        assert_eq!(parse_duration(""), None);
        assert_eq!(parse_duration("abc"), None);
    }
}
