//! Layer 2 of the OT Connector conformance suite: golden decode/encode vectors.
//!
//! The vectors live once, here in the SDK (`crates/sdk/conformance/vectors.json`), because all
//! connectors decode through [`decode_primitive`]/[`encode_primitive`] — passing them proves
//! `typed` mode correct for every connector at once. They are stored as data (JSON), not code,
//! so they stay language-neutral and auditable. See
//! `doc/conformance/conformance-suite.md` §3 for the required coverage.

use crate::decode::{decode_primitive, encode_primitive, extract_bitfield, Endianness, WordOrder};
use crate::model::{DataType, Value};
use serde::Deserialize;

/// The checked-in golden vector file, embedded so harness binaries are self-contained.
pub const BUILTIN_VECTORS: &str = include_str!("../conformance/vectors.json");

#[derive(Debug, Deserialize)]
struct VectorFile {
    vectors: Vec<Vector>,
}

/// One golden vector: a byte buffer plus decode parameters and the expected outcome.
#[derive(Debug, Clone, Deserialize)]
pub struct Vector {
    pub id: String,
    pub datatype: DataType,
    #[serde(default)]
    pub endianness: Option<String>,
    #[serde(default)]
    pub word_order: Option<String>,
    /// The "natural" wire buffer (16-bit words serialized big-endian, in word order), hex.
    pub bytes: String,
    /// When set, the vector exercises [`extract_bitfield`] instead of [`decode_primitive`].
    #[serde(default)]
    pub bitfield: Option<Bitfield>,
    pub expect: Expect,
    /// When true, `encode_primitive(expect.value)` must reproduce `bytes` (write-path symmetry).
    #[serde(default)]
    pub roundtrip: bool,
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Bitfield {
    pub start_bit: u32,
    pub bit_count: u32,
}

/// Expected outcome: a value + repr, an IEEE-754 special, or a decode error.
#[derive(Debug, Clone, Deserialize)]
pub struct Expect {
    #[serde(default)]
    pub value: Option<serde_json::Value>,
    #[serde(default)]
    pub value_repr: Option<String>,
    /// `"nan"`, `"+inf"` or `"-inf"` — values JSON cannot carry literally.
    #[serde(default)]
    pub special: Option<String>,
    #[serde(default)]
    pub error: bool,
}

/// Parse a vector file (the embedded [`BUILTIN_VECTORS`] or an external override).
pub fn parse_vectors(json: &str) -> Result<Vec<Vector>, String> {
    let file: VectorFile =
        serde_json::from_str(json).map_err(|e| format!("invalid vector file: {e}"))?;
    Ok(file.vectors)
}

/// Run one vector against the SDK decode/encode helpers. `Ok(())` means the vector passed.
pub fn run_vector(v: &Vector) -> Result<(), String> {
    let bytes = parse_hex(&v.bytes)?;
    let end = Endianness::parse(v.endianness.as_deref());
    let wo = WordOrder::parse(v.word_order.as_deref());

    if let Some(bf) = &v.bitfield {
        let got = extract_bitfield(&bytes, end, wo, bf.start_bit, bf.bit_count);
        let want = v
            .expect
            .value
            .as_ref()
            .and_then(|x| x.as_u64())
            .ok_or("bitfield vector needs an unsigned integer 'expect.value'")?;
        if got != want {
            return Err(format!("bitfield: expected {want}, got {got}"));
        }
        return Ok(());
    }

    let decoded = decode_primitive(&bytes, v.datatype, end, wo);

    if v.expect.error {
        return match decoded {
            Err(_) => Ok(()),
            Ok(got) => Err(format!("expected a decode error, got {got:?}")),
        };
    }
    let decoded = decoded.map_err(|e| format!("decode failed: {e}"))?;

    let expected = expected_value(&v.expect)?;
    check_value(&decoded, &expected)?;
    if let Some(repr) = &v.expect.value_repr {
        if decoded.repr() != repr {
            return Err(format!(
                "value_repr: expected '{repr}', got '{}'",
                decoded.repr()
            ));
        }
    }

    if v.roundtrip {
        let encoded = encode_primitive(&expected, v.datatype, end, wo)
            .map_err(|e| format!("encode failed: {e}"))?;
        if encoded != bytes {
            return Err(format!(
                "round-trip: expected bytes {}, got {}",
                v.bytes,
                crate::model::hex_grouped(&encoded, encoded.len().max(1))
            ));
        }
    }
    Ok(())
}

/// Build the expected [`Value`] from the vector's `expect` clause.
fn expected_value(expect: &Expect) -> Result<Value, String> {
    if let Some(special) = &expect.special {
        let n = match special.as_str() {
            "nan" => f64::NAN,
            "+inf" => f64::INFINITY,
            "-inf" => f64::NEG_INFINITY,
            other => return Err(format!("unknown special '{other}'")),
        };
        return Ok(Value::Number(n));
    }
    match &expect.value {
        Some(serde_json::Value::Bool(b)) => Ok(Value::Bool(*b)),
        Some(serde_json::Value::Number(n)) => n
            .as_f64()
            .map(Value::Number)
            .ok_or_else(|| "non-f64 number in expect.value".into()),
        Some(serde_json::Value::String(s)) => Ok(Value::Text(s.clone())),
        other => Err(format!("unsupported expect.value: {other:?}")),
    }
}

/// Compare a decoded value against the expectation. Numbers compare with a tight relative
/// tolerance (the vectors are exact bit patterns, but this keeps the check robust across
/// JSON printing); NaN equals NaN.
fn check_value(got: &Value, want: &Value) -> Result<(), String> {
    let ok = match (got, want) {
        (Value::Bool(g), Value::Bool(w)) => g == w,
        (Value::Text(g), Value::Text(w)) => g == w,
        (Value::Number(g), Value::Number(w)) => {
            (g.is_nan() && w.is_nan()) || *g == *w || {
                let scale = g.abs().max(w.abs()).max(1.0);
                (g - w).abs() <= 1e-12 * scale
            }
        }
        _ => false,
    };
    if ok {
        Ok(())
    } else {
        Err(format!("value: expected {want:?}, got {got:?}"))
    }
}

fn parse_hex(s: &str) -> Result<Vec<u8>, String> {
    let cleaned: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    if !cleaned.len().is_multiple_of(2) {
        return Err("hex string has odd length".into());
    }
    (0..cleaned.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&cleaned[i..i + 2], 16).map_err(|e| format!("invalid hex: {e}"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_vectors_parse() {
        let vectors = parse_vectors(BUILTIN_VECTORS).unwrap();
        assert!(vectors.len() >= 70, "got {}", vectors.len());
    }

    #[test]
    fn vector_ids_are_unique() {
        let vectors = parse_vectors(BUILTIN_VECTORS).unwrap();
        let mut ids: Vec<&str> = vectors.iter().map(|v| v.id.as_str()).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), vectors.len());
    }

    #[test]
    fn every_datatype_is_covered() {
        let vectors = parse_vectors(BUILTIN_VECTORS).unwrap();
        for dt in [
            DataType::Bool,
            DataType::Int8,
            DataType::Uint8,
            DataType::Int16,
            DataType::Uint16,
            DataType::Int32,
            DataType::Uint32,
            DataType::Int64,
            DataType::Uint64,
            DataType::Float32,
            DataType::Float64,
            DataType::String,
            DataType::Bytes,
        ] {
            assert!(
                vectors.iter().any(|v| v.datatype == dt),
                "no vector for {dt:?}"
            );
        }
    }

    #[test]
    fn a_failing_vector_is_reported() {
        let mut v = parse_vectors(BUILTIN_VECTORS)
            .unwrap()
            .into_iter()
            .find(|v| v.id == "uint16-nominal-be")
            .unwrap();
        v.expect.value = Some(serde_json::json!(1));
        assert!(run_vector(&v).is_err());
    }
}
