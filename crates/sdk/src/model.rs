//! Shared model types for the OT Connector Contract.
//!
//! These types are owned by the SDK so every connector and the conformance suite agree on
//! them. They serialize to the contract's `sample` and `command` envelopes.

use serde::{Deserialize, Serialize};
use time::macros::format_description;
use time::OffsetDateTime;

/// thin-edge entity id segment for a device (e.g. `plc-1`).
pub type DeviceId = String;
/// Point id, unique within a device.
pub type PointId = String;

/// Per-point output selection.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Emit the raw bytes only (no decoded value).
    Raw,
    /// Emit a decoded primitive value.
    Typed,
}

/// The closed set of primitive datatypes the contract supports in `typed` mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DataType {
    Bool,
    Int8,
    Uint8,
    Int16,
    Uint16,
    Int32,
    Uint32,
    Int64,
    Uint64,
    Float32,
    Float64,
    String,
    Bytes,
}

impl DataType {
    /// Number of 8-bit bytes a value of this datatype occupies (excluding `string`/`bytes`,
    /// which are variable length and return `None`).
    pub fn byte_len(self) -> Option<usize> {
        Some(match self {
            DataType::Bool | DataType::Int8 | DataType::Uint8 => 1,
            DataType::Int16 | DataType::Uint16 => 2,
            DataType::Int32 | DataType::Uint32 | DataType::Float32 => 4,
            DataType::Int64 | DataType::Uint64 | DataType::Float64 => 8,
            DataType::String | DataType::Bytes => return None,
        })
    }
}

/// A decoded value. `Number` covers all integer and float types within the JS safe range;
/// `Text` is used for 64-bit integers outside the safe range and for `string`.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Bool(bool),
    Number(f64),
    Text(String),
}

/// A per-point linear transform applied to a decoded numeric value:
/// `(value * multiplier * 10^decimal_shift / divisor) + offset`.
///
/// Scaling is an intrinsic property of a signal (point), not of a downstream flow, so the
/// contract carries it as a point field and the SDK owns the math. Connectors apply it via
/// [`Transform::apply`] right after primitive decode; non-numeric values pass through unchanged.
#[derive(Clone, Copy, Debug, PartialEq, Deserialize)]
#[serde(default)]
pub struct Transform {
    pub multiplier: f64,
    pub divisor: f64,
    pub decimal_shift: i32,
    pub offset: f64,
}

impl Default for Transform {
    fn default() -> Self {
        Transform {
            multiplier: 1.0,
            divisor: 1.0,
            decimal_shift: 0,
            offset: 0.0,
        }
    }
}

impl Transform {
    /// True when the transform leaves every numeric value unchanged (the identity transform).
    pub fn is_identity(&self) -> bool {
        self.multiplier == 1.0 && self.divisor == 1.0 && self.decimal_shift == 0 && self.offset == 0.0
    }

    /// Apply the linear transform to a decoded value. Numeric values are scaled; booleans and
    /// strings are returned unchanged (scaling them is meaningless). A zero `divisor` is treated
    /// as `1` to avoid producing `NaN`/`Inf`.
    pub fn apply(&self, value: Value) -> Value {
        match value {
            Value::Number(n) => {
                let divisor = if self.divisor == 0.0 { 1.0 } else { self.divisor };
                Value::Number((n * self.multiplier * 10f64.powi(self.decimal_shift)) / divisor + self.offset)
            }
            other => other,
        }
    }
}

impl Value {
    /// The `value_repr` tag the contract requires alongside `value`.
    pub fn repr(&self) -> &'static str {
        match self {
            Value::Bool(_) => "boolean",
            Value::Number(_) => "number",
            Value::Text(_) => "string",
        }
    }

    fn to_json(&self) -> serde_json::Value {
        match self {
            Value::Bool(b) => serde_json::Value::Bool(*b),
            Value::Number(n) => serde_json::json!(n),
            Value::Text(t) => serde_json::Value::String(t.clone()),
        }
    }
}

/// Read quality.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Quality {
    Good,
    Bad,
    Stale,
}

impl Quality {
    fn as_str(self) -> &'static str {
        match self {
            Quality::Good => "good",
            Quality::Bad => "bad",
            Quality::Stale => "stale",
        }
    }
}

/// One read result, serialized to the contract sample envelope.
#[derive(Clone, Debug)]
pub struct Sample {
    pub ts: OffsetDateTime,
    pub device: DeviceId,
    pub protocol: &'static str,
    pub point: PointId,
    pub mode: Mode,
    pub datatype: Option<DataType>,
    pub value: Option<Value>,
    /// Raw bytes read from the wire; always present.
    pub raw: Vec<u8>,
    /// Number of bytes per hex group when serializing `raw` (2 for 16-bit registers, 1 for coils).
    pub raw_group: usize,
    pub quality: Quality,
    pub unit: Option<String>,
    /// Protocol-specific address echo.
    pub addr: serde_json::Value,
    pub seq: Option<u64>,
    /// Required when `quality == Bad`.
    pub error: Option<String>,
}

impl Sample {
    /// Build the JSON sample envelope per the OT Connector Contract §5.
    pub fn to_envelope(&self) -> serde_json::Value {
        let mut obj = serde_json::Map::new();
        obj.insert("ts".into(), serde_json::Value::String(format_rfc3339_ms(self.ts)));
        obj.insert("ts_ms".into(), serde_json::json!(unix_ms(self.ts)));
        obj.insert("device".into(), serde_json::Value::String(self.device.clone()));
        obj.insert("protocol".into(), serde_json::Value::String(self.protocol.into()));
        obj.insert("point".into(), serde_json::Value::String(self.point.clone()));
        obj.insert(
            "mode".into(),
            serde_json::Value::String(match self.mode {
                Mode::Raw => "raw".into(),
                Mode::Typed => "typed".into(),
            }),
        );
        if let Some(dt) = self.datatype {
            obj.insert("datatype".into(), serde_json::to_value(dt).unwrap());
        }
        if let Some(v) = &self.value {
            obj.insert("value".into(), v.to_json());
            obj.insert("value_repr".into(), serde_json::Value::String(v.repr().into()));
        }
        obj.insert(
            "raw".into(),
            serde_json::Value::String(hex_grouped(&self.raw, self.raw_group)),
        );
        obj.insert("quality".into(), serde_json::Value::String(self.quality.as_str().into()));
        if let Some(u) = &self.unit {
            obj.insert("unit".into(), serde_json::Value::String(u.clone()));
        }
        obj.insert("addr".into(), self.addr.clone());
        if let Some(seq) = self.seq {
            obj.insert("seq".into(), serde_json::json!(seq));
        }
        if let Some(err) = &self.error {
            obj.insert("error".into(), serde_json::Value::String(err.clone()));
        }
        serde_json::Value::Object(obj)
    }
}

/// Unix epoch milliseconds as a float (sub-millisecond precision preserved), the numeric
/// companion to the RFC 3339 `ts` — consumers doing time math get a number instead of a
/// string to parse.
pub fn unix_ms(ts: OffsetDateTime) -> f64 {
    ts.unix_timestamp_nanos() as f64 / 1e6
}

/// Format an `OffsetDateTime` as RFC 3339, millisecond precision, UTC `Z`.
pub fn format_rfc3339_ms(ts: OffsetDateTime) -> String {
    let fmt = format_description!(
        "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z"
    );
    ts.to_offset(time::UtcOffset::UTC)
        .format(fmt)
        .unwrap_or_else(|_| "1970-01-01T00:00:00.000Z".to_string())
}

/// Format bytes as lowercase hex, grouped every `group` bytes with a single space.
pub fn hex_grouped(bytes: &[u8], group: usize) -> String {
    let group = group.max(1);
    let mut out = String::new();
    for (i, chunk) in bytes.chunks(group).enumerate() {
        if i > 0 {
            out.push(' ');
        }
        for b in chunk {
            out.push_str(&format!("{:02x}", b));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transform_default_is_identity() {
        let t = Transform::default();
        assert!(t.is_identity());
        assert_eq!(t.apply(Value::Number(42.5)), Value::Number(42.5));
    }

    #[test]
    fn transform_linear_scale() {
        // (1000 * 1 * 10^-3 / 1) + 0 = 1.0
        let t = Transform {
            multiplier: 1.0,
            divisor: 1.0,
            decimal_shift: -3,
            offset: 0.0,
        };
        assert_eq!(t.apply(Value::Number(1000.0)), Value::Number(1.0));
    }

    #[test]
    fn transform_multiplier_divisor_offset() {
        // (50 * 2 / 4) + 10 = 35
        let t = Transform {
            multiplier: 2.0,
            divisor: 4.0,
            decimal_shift: 0,
            offset: 10.0,
        };
        assert_eq!(t.apply(Value::Number(50.0)), Value::Number(35.0));
    }

    #[test]
    fn transform_zero_divisor_is_safe() {
        let t = Transform {
            multiplier: 1.0,
            divisor: 0.0,
            decimal_shift: 0,
            offset: 0.0,
        };
        assert_eq!(t.apply(Value::Number(7.0)), Value::Number(7.0));
    }

    #[test]
    fn transform_leaves_non_numbers_unchanged() {
        let t = Transform {
            multiplier: 10.0,
            divisor: 1.0,
            decimal_shift: 0,
            offset: 5.0,
        };
        assert_eq!(t.apply(Value::Bool(true)), Value::Bool(true));
        assert_eq!(t.apply(Value::Text("hi".into())), Value::Text("hi".into()));
    }

    #[test]
    fn envelope_carries_both_timestamp_forms() {
        let ts = OffsetDateTime::from_unix_timestamp_nanos(1_500_000_000_123_456_789).unwrap();
        let sample = Sample {
            ts,
            device: "plc-1".into(),
            protocol: "modbus",
            point: "temp".into(),
            mode: Mode::Typed,
            datatype: Some(DataType::Uint16),
            value: Some(Value::Number(1.0)),
            raw: vec![0x00, 0x01],
            raw_group: 2,
            quality: Quality::Good,
            unit: None,
            addr: serde_json::Value::Null,
            seq: None,
            error: None,
        };
        let env = sample.to_envelope();
        assert_eq!(env["ts"], serde_json::json!("2017-07-14T02:40:00.123Z"));
        // ts_ms is the same instant as unix epoch milliseconds (float, sub-ms preserved)
        assert!((env["ts_ms"].as_f64().unwrap() - 1_500_000_000_123.456_8).abs() < 1e-3);
    }

    #[test]
    fn transform_parsed_from_partial_toml() {
        let t: Transform = toml::from_str("multiplier = 2.5").unwrap();
        assert_eq!(t.multiplier, 2.5);
        assert_eq!(t.divisor, 1.0);
        assert_eq!(t.decimal_shift, 0);
        assert_eq!(t.offset, 0.0);
    }
}
