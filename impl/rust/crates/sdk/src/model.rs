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

/// The closed set of datatypes a point may declare (§4). Every point declares one: there is
/// no second type system alongside it.
///
/// `bytes` is the raw case — the value is the hex of what was read, and what each connector
/// means by "the bytes" is defined in its own spec (the registers or coils for Modbus, the
/// whole frame payload for CAN, the SDO payload for CANopen, the bytes at `byte_offset` for
/// PROFIBUS-DP, the best-effort variant encoding for OPC UA). A connector that cannot deliver
/// bytes for a point kind refuses `bytes` at configuration time, exactly as it refuses any
/// datatype it does not list in its capabilities.
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

    /// True for the integer datatypes — the ones whose wire value must be a whole number, so a
    /// write inverted through a transform is rounded to nearest before encoding (§4.2).
    pub fn is_integer(self) -> bool {
        matches!(
            self,
            DataType::Int8
                | DataType::Uint8
                | DataType::Int16
                | DataType::Uint16
                | DataType::Int32
                | DataType::Uint32
                | DataType::Int64
                | DataType::Uint64
        )
    }

    /// The inclusive range of wire values this datatype can hold, for the numeric datatypes.
    /// `None` for `bool`, `string` and `bytes`, which a transform never touches.
    ///
    /// The 64-bit bounds are the exact integer limits; they are not representable in `f64`, so
    /// a value near them may round to the bound. That is deliberate: the check exists to catch
    /// a write that is wrong by orders of magnitude, not to police the last bit of an i64.
    pub fn value_range(self) -> Option<(f64, f64)> {
        Some(match self {
            DataType::Int8 => (i8::MIN as f64, i8::MAX as f64),
            DataType::Uint8 => (u8::MIN as f64, u8::MAX as f64),
            DataType::Int16 => (i16::MIN as f64, i16::MAX as f64),
            DataType::Uint16 => (u16::MIN as f64, u16::MAX as f64),
            DataType::Int32 => (i32::MIN as f64, i32::MAX as f64),
            DataType::Uint32 => (u32::MIN as f64, u32::MAX as f64),
            DataType::Int64 => (i64::MIN as f64, i64::MAX as f64),
            DataType::Uint64 => (u64::MIN as f64, u64::MAX as f64),
            DataType::Float32 => (-(f32::MAX as f64), f32::MAX as f64),
            DataType::Float64 => (f64::MIN, f64::MAX),
            DataType::Bool | DataType::String | DataType::Bytes => return None,
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

    /// True when [`Transform::invert`] can undo this transform — that is, when `multiplier` is
    /// not zero. A `multiplier` of `0` maps every value to the same `offset`, so there is no
    /// wire value a write could mean; a writable point declaring one is refused at load (§4.2).
    pub fn is_invertible(&self) -> bool {
        self.multiplier != 0.0
    }

    /// Undo the linear transform: given a value in engineering units — what a sample's `value`
    /// carries, and therefore what a write request carries (§4.2) — return the wire value a
    /// module must encode.
    ///
    /// ```text
    /// wire = (value − offset) × divisor ÷ (multiplier × 10^decimal_shift)
    /// ```
    ///
    /// Booleans and strings pass through, exactly as [`Transform::apply`] leaves them. Returns
    /// `None` when the transform is not invertible, which the loader has already refused for a
    /// writable point.
    pub fn invert(&self, value: Value) -> Option<Value> {
        match value {
            Value::Number(n) => {
                if !self.is_invertible() {
                    return None;
                }
                let divisor = if self.divisor == 0.0 { 1.0 } else { self.divisor };
                Some(Value::Number(
                    (n - self.offset) * divisor / (self.multiplier * 10f64.powi(self.decimal_shift)),
                ))
            }
            other => Some(other),
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

    /// The value as it appears in a sample envelope's `value`, and as the publish gate (§5.4)
    /// compares one reading with the last published one.
    pub fn to_json(&self) -> serde_json::Value {
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
    /// The point's datatype (§4). Always present: `bytes` is the raw case.
    pub datatype: DataType,
    pub value: Option<Value>,
    /// Raw bytes read from the wire; always present.
    pub raw: Vec<u8>,
    /// Number of bytes per hex group when serializing `raw` (2 for 16-bit registers, 1 for coils).
    pub raw_group: usize,
    pub quality: Quality,
    /// The point's declared unit, as the module resolved it. **Not** serialized into the
    /// envelope (§5): a unit is static per point, so it is published once on the device
    /// manifest (§8.2). Kept here because it is part of what a read resolved, and the module
    /// integration tests assert on it.
    pub unit: Option<String>,
    /// Protocol-specific address echo. Serialized only under `[connector] sample_debug`.
    pub addr: serde_json::Value,
    pub seq: Option<u64>,
    /// Required when `quality == Bad`.
    pub error: Option<String>,
}

impl Sample {
    /// Build the JSON sample envelope per the OT Connector Contract §5.
    ///
    /// A sample is a time series row: identity, value, quality — and nothing that is static per
    /// point. Everything a consumer needs to *interpret* the point (its unit, its labels, its
    /// access, its free-form `meta`, and the device's type) is on the device's retained manifest
    /// (§8.2), published once instead of on every read.
    ///
    /// `debug` is `[connector] sample_debug`: it adds `raw` and `addr` back, for the tooling
    /// that inspects the wire (the conformance suite runs with it on). Off by default, because
    /// both are a debugging aid on a message a point publishes thousands of times a day.
    pub fn to_envelope(&self, debug: bool) -> serde_json::Value {
        let mut obj = serde_json::Map::new();
        obj.insert("ts".into(), serde_json::Value::String(format_rfc3339_ms(self.ts)));
        obj.insert("device".into(), serde_json::Value::String(self.device.clone()));
        obj.insert("protocol".into(), serde_json::Value::String(self.protocol.into()));
        obj.insert("point".into(), serde_json::Value::String(self.point.clone()));
        obj.insert(
            "datatype".into(),
            serde_json::to_value(self.datatype).unwrap(),
        );
        // No `value_repr`: `datatype` plus the JSON type of `value` say the same thing, and a
        // consumer that needs to know an int64 was widened to a string reads `datatype`.
        if let Some(v) = &self.value {
            obj.insert("value".into(), v.to_json());
        }
        obj.insert("quality".into(), serde_json::Value::String(self.quality.as_str().into()));
        if let Some(seq) = self.seq {
            obj.insert("seq".into(), serde_json::json!(seq));
        }
        if let Some(err) = &self.error {
            obj.insert("error".into(), serde_json::Value::String(err.clone()));
        }
        if debug {
            obj.insert(
                "raw".into(),
                serde_json::Value::String(hex_grouped(&self.raw, self.raw_group)),
            );
            obj.insert("addr".into(), self.addr.clone());
        }
        serde_json::Value::Object(obj)
    }
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

    fn envelope_sample() -> Sample {
        let ts = OffsetDateTime::from_unix_timestamp_nanos(1_500_000_000_123_456_789).unwrap();
        Sample {
            ts,
            device: "plc-1".into(),
            protocol: "modbus",
            point: "temp".into(),
            datatype: DataType::Uint16,
            value: Some(Value::Number(1.0)),
            raw: vec![0x00, 0x01],
            raw_group: 2,
            quality: Quality::Good,
            unit: Some("°C".into()),
            addr: serde_json::json!({ "table": "holding", "address": 3 }),
            seq: Some(7),
            error: None,
        }
    }

    /// The 0.2 envelope is a time series row: identity, value, quality. Everything static per
    /// point — the unit, the labels, the access, `meta`, the device type — is on the manifest
    /// (§8.2), and `ts_ms`/`value_repr` are gone because `ts` and `datatype` already say it.
    #[test]
    fn envelope_carries_only_what_changes_per_read() {
        let env = envelope_sample().to_envelope(false);
        assert_eq!(env["ts"], serde_json::json!("2017-07-14T02:40:00.123Z"));
        assert_eq!(env["device"], serde_json::json!("plc-1"));
        assert_eq!(env["protocol"], serde_json::json!("modbus"));
        assert_eq!(env["point"], serde_json::json!("temp"));
        assert_eq!(env["datatype"], serde_json::json!("uint16"));
        assert_eq!(env["value"], serde_json::json!(1.0));
        assert_eq!(env["quality"], serde_json::json!("good"));
        assert_eq!(env["seq"], serde_json::json!(7));
        for gone in ["ts_ms", "value_repr", "unit", "type", "access", "meta", "raw", "addr"] {
            assert!(env.get(gone).is_none(), "{gone} must not be in a 0.2 sample");
        }
    }

    /// `sample_debug` puts the wire back on the envelope — and nothing else: it is a debugging
    /// aid, not a way to re-add the static fields the manifest carries.
    #[test]
    fn sample_debug_adds_the_wire_back() {
        let env = envelope_sample().to_envelope(true);
        assert_eq!(env["raw"], serde_json::json!("0001"));
        assert_eq!(env["addr"]["address"], serde_json::json!(3));
        for gone in ["ts_ms", "value_repr", "unit", "type", "access", "meta"] {
            assert!(env.get(gone).is_none(), "{gone} is the manifest's, debug or not");
        }
    }

    /// §4.2: a write is in the same units as a read, so inverting must undo `apply` exactly.
    /// This is the defect the RFC found: the demo's `temp_scaled` read 17.001 °C from register
    /// 17001, an operator edited it to 20, and the module wrote register 20 — the next read
    /// then said 0.02.
    #[test]
    fn invert_undoes_apply() {
        let scaled = Transform {
            multiplier: 1.0,
            divisor: 1.0,
            decimal_shift: -3,
            offset: 0.0,
        };
        assert_eq!(scaled.apply(Value::Number(17001.0)), Value::Number(17.001));
        assert_eq!(scaled.invert(Value::Number(20.0)), Some(Value::Number(20000.0)));

        // Every field at once, and a round trip through both directions.
        let full = Transform {
            multiplier: 2.0,
            divisor: 4.0,
            decimal_shift: 1,
            offset: 7.5,
        };
        for wire in [0.0, 1.0, -3.25, 1234.5] {
            let Value::Number(engineering) = full.apply(Value::Number(wire)) else {
                panic!("a numeric value stays numeric");
            };
            let Some(Value::Number(back)) = full.invert(Value::Number(engineering)) else {
                panic!("an invertible transform inverts");
            };
            assert!((back - wire).abs() < 1e-9, "{wire} -> {engineering} -> {back}");
        }
    }

    /// Booleans and strings are untouched in both directions — the transform never applied to
    /// them — and a zero `divisor` is read as 1 on the way back too.
    #[test]
    fn invert_leaves_non_numbers_and_survives_a_zero_divisor() {
        let t = Transform {
            multiplier: 10.0,
            divisor: 0.0,
            decimal_shift: 0,
            offset: 5.0,
        };
        assert_eq!(t.invert(Value::Bool(true)), Some(Value::Bool(true)));
        assert_eq!(
            t.invert(Value::Text("hi".into())),
            Some(Value::Text("hi".into()))
        );
        // apply: 3 * 10 / 1 + 5 = 35; invert: (35 - 5) * 1 / 10 = 3
        assert_eq!(t.apply(Value::Number(3.0)), Value::Number(35.0));
        assert_eq!(t.invert(Value::Number(35.0)), Some(Value::Number(3.0)));
    }

    /// `multiplier = 0` maps every wire value to `offset`, so no write can mean anything.
    /// The loader refuses such a point when it is writable (§4.2); the model reports it here.
    #[test]
    fn a_zero_multiplier_is_not_invertible() {
        let t = Transform {
            multiplier: 0.0,
            divisor: 1.0,
            decimal_shift: 0,
            offset: 4.0,
        };
        assert!(!t.is_invertible());
        assert_eq!(t.invert(Value::Number(4.0)), None);
        // A non-numeric value still passes through: there was nothing to invert.
        assert_eq!(t.invert(Value::Bool(false)), Some(Value::Bool(false)));
    }

    #[test]
    fn integer_datatypes_know_their_range() {
        assert!(DataType::Uint16.is_integer());
        assert!(!DataType::Float32.is_integer());
        assert!(!DataType::Bool.is_integer());
        assert_eq!(DataType::Uint16.value_range(), Some((0.0, 65535.0)));
        assert_eq!(DataType::Int8.value_range(), Some((-128.0, 127.0)));
        assert_eq!(DataType::Bool.value_range(), None);
        assert_eq!(DataType::Bytes.value_range(), None);
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
