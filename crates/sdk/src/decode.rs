//! Shared primitive decode/encode helpers.
//!
//! `typed` mode must decode identically across all connectors, so the IEEE-754 / endianness /
//! word-order logic lives here and **only** here. See the OT Connector Contract §4 and the
//! Modbus connector spec §9 acceptance vectors.

use crate::model::{DataType, Value};
use thiserror::Error;

/// Byte order within a 16-bit word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Endianness {
    Big,
    Little,
}

/// Order of multiple 16-bit words forming a wider value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WordOrder {
    Big,
    Little,
}

impl Endianness {
    /// Parse from a config string; defaults to `Big` for unknown/empty input.
    pub fn parse(s: Option<&str>) -> Self {
        match s.map(|s| s.to_ascii_lowercase()).as_deref() {
            Some("little") => Endianness::Little,
            _ => Endianness::Big,
        }
    }
}

impl WordOrder {
    pub fn parse(s: Option<&str>) -> Self {
        match s.map(|s| s.to_ascii_lowercase()).as_deref() {
            Some("little") => WordOrder::Little,
            _ => WordOrder::Big,
        }
    }
}

#[derive(Debug, Error)]
pub enum DecodeError {
    #[error("expected {expected} bytes for {datatype:?}, got {actual}")]
    Length {
        datatype: DataType,
        expected: usize,
        actual: usize,
    },
    #[error("datatype {0:?} has no decoded value (raw only)")]
    NoValue(DataType),
    #[error("invalid value for datatype {0:?}")]
    InvalidValue(DataType),
}

/// Reorder a "natural" byte buffer (each 16-bit word serialized big-endian, in word order)
/// into a canonical big-endian byte sequence, applying `word_order` then `endianness`.
///
/// This is its own inverse, so it is used for both decode and encode.
fn reorder(bytes: &[u8], end: Endianness, wo: WordOrder) -> Vec<u8> {
    let mut words: Vec<Vec<u8>> = bytes.chunks(2).map(|c| c.to_vec()).collect();
    if wo == WordOrder::Little {
        words.reverse();
    }
    let mut out = Vec::with_capacity(bytes.len());
    for mut w in words {
        if end == Endianness::Little && w.len() == 2 {
            w.swap(0, 1);
        }
        out.extend(w);
    }
    out
}

/// JS safe-integer bounds (`2^53 - 1`).
const SAFE_INT_MAX: i128 = 9_007_199_254_740_991;
const SAFE_INT_MIN: i128 = -9_007_199_254_740_991;

/// Decode a primitive value from a "natural" byte buffer (words serialized big-endian, in
/// word order). Applies `endianness` and `word_order`, then interprets the datatype.
pub fn decode_primitive(
    bytes: &[u8],
    datatype: DataType,
    endianness: Endianness,
    word_order: WordOrder,
) -> Result<Value, DecodeError> {
    if datatype == DataType::Bytes {
        return Err(DecodeError::NoValue(DataType::Bytes));
    }
    if datatype == DataType::Bool {
        return Ok(Value::Bool(bytes.iter().any(|b| *b != 0)));
    }
    if datatype == DataType::String {
        // ASCII/UTF-8, null-trimmed. String preserves wire order (no reordering).
        let trimmed: Vec<u8> = bytes.iter().copied().take_while(|b| *b != 0).collect();
        let text = String::from_utf8_lossy(&trimmed).to_string();
        return Ok(Value::Text(text));
    }

    let expected = datatype.byte_len().expect("non string/bytes has fixed length");
    if bytes.len() != expected {
        return Err(DecodeError::Length {
            datatype,
            expected,
            actual: bytes.len(),
        });
    }
    let be = reorder(bytes, endianness, word_order);

    let value = match datatype {
        DataType::Int8 => Value::Number(i8::from_be_bytes([be[0]]) as f64),
        DataType::Uint8 => Value::Number(be[0] as f64),
        DataType::Int16 => Value::Number(i16::from_be_bytes([be[0], be[1]]) as f64),
        DataType::Uint16 => Value::Number(u16::from_be_bytes([be[0], be[1]]) as f64),
        DataType::Int32 => {
            Value::Number(i32::from_be_bytes([be[0], be[1], be[2], be[3]]) as f64)
        }
        DataType::Uint32 => {
            Value::Number(u32::from_be_bytes([be[0], be[1], be[2], be[3]]) as f64)
        }
        DataType::Int64 => {
            let v = i64::from_be_bytes(be[..8].try_into().unwrap());
            int_value(v as i128)
        }
        DataType::Uint64 => {
            let v = u64::from_be_bytes(be[..8].try_into().unwrap());
            int_value(v as i128)
        }
        DataType::Float32 => {
            Value::Number(f32::from_be_bytes([be[0], be[1], be[2], be[3]]) as f64)
        }
        DataType::Float64 => Value::Number(f64::from_be_bytes(be[..8].try_into().unwrap())),
        DataType::Bool | DataType::String | DataType::Bytes => unreachable!(),
    };
    Ok(value)
}

fn int_value(v: i128) -> Value {
    if (SAFE_INT_MIN..=SAFE_INT_MAX).contains(&v) {
        Value::Number(v as f64)
    } else {
        Value::Text(v.to_string())
    }
}

/// Encode a primitive value into a "natural" byte buffer (words serialized big-endian, in
/// word order) ready to be split into 16-bit registers. Inverse of [`decode_primitive`].
pub fn encode_primitive(
    value: &Value,
    datatype: DataType,
    endianness: Endianness,
    word_order: WordOrder,
) -> Result<Vec<u8>, DecodeError> {
    let be: Vec<u8> = match datatype {
        DataType::Bool => {
            let b = match value {
                Value::Bool(b) => *b,
                Value::Number(n) => *n != 0.0,
                Value::Text(t) => t == "true" || t == "1",
            };
            return Ok(vec![if b { 1 } else { 0 }]);
        }
        DataType::Int8 => (number(value, datatype)? as i64 as i8).to_be_bytes().to_vec(),
        DataType::Uint8 => (number(value, datatype)? as i64 as u8).to_be_bytes().to_vec(),
        DataType::Int16 => (number(value, datatype)? as i64 as i16).to_be_bytes().to_vec(),
        DataType::Uint16 => (number(value, datatype)? as i64 as u16).to_be_bytes().to_vec(),
        DataType::Int32 => (number(value, datatype)? as i64 as i32).to_be_bytes().to_vec(),
        DataType::Uint32 => (number(value, datatype)? as i64 as u32).to_be_bytes().to_vec(),
        DataType::Int64 => int_from_value(value)?.to_be_bytes().to_vec(),
        DataType::Uint64 => (int_from_value(value)? as u64).to_be_bytes().to_vec(),
        DataType::Float32 => (number(value, datatype)? as f32).to_be_bytes().to_vec(),
        DataType::Float64 => number(value, datatype)?.to_be_bytes().to_vec(),
        DataType::String => {
            let s = match value {
                Value::Text(t) => t.clone(),
                _ => return Err(DecodeError::InvalidValue(datatype)),
            };
            return Ok(s.into_bytes());
        }
        DataType::Bytes => return Err(DecodeError::NoValue(DataType::Bytes)),
    };
    Ok(reorder(&be, endianness, word_order))
}

fn number(value: &Value, datatype: DataType) -> Result<f64, DecodeError> {
    match value {
        Value::Number(n) => Ok(*n),
        Value::Bool(b) => Ok(if *b { 1.0 } else { 0.0 }),
        Value::Text(t) => t.parse::<f64>().map_err(|_| DecodeError::InvalidValue(datatype)),
    }
}

fn int_from_value(value: &Value) -> Result<i64, DecodeError> {
    match value {
        Value::Number(n) => Ok(*n as i64),
        Value::Text(t) => t.parse::<i64>().map_err(|_| DecodeError::InvalidValue(DataType::Int64)),
        Value::Bool(b) => Ok(if *b { 1 } else { 0 }),
    }
}

/// Extract an unsigned bit-field of `bit_count` bits starting at `start_bit` (0-based, LSB of
/// the canonical big-endian buffer) from a register buffer. Used by the optional `bitfield`
/// feature. The buffer is the "natural" big-endian byte serialization of the registers.
pub fn extract_bitfield(
    bytes: &[u8],
    endianness: Endianness,
    word_order: WordOrder,
    start_bit: u32,
    bit_count: u32,
) -> u64 {
    let be = reorder(bytes, endianness, word_order);
    let mut acc: u128 = 0;
    for b in &be {
        acc = (acc << 8) | (*b as u128);
    }
    if bit_count == 0 || bit_count >= 64 {
        return acc as u64;
    }
    let mask: u128 = (1u128 << bit_count) - 1;
    ((acc >> start_bit) & mask) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn regs_to_bytes(regs: &[u16]) -> Vec<u8> {
        regs.iter().flat_map(|r| r.to_be_bytes()).collect()
    }

    // Modbus connector spec §9 acceptance vectors.

    #[test]
    fn v9_1_uint16_be() {
        let b = regs_to_bytes(&[0x1234]);
        let v = decode_primitive(&b, DataType::Uint16, Endianness::Big, WordOrder::Big).unwrap();
        assert_eq!(v, Value::Number(4660.0));
    }

    #[test]
    fn v9_2_int16_negative() {
        let b = regs_to_bytes(&[0xfffe]);
        let v = decode_primitive(&b, DataType::Int16, Endianness::Big, WordOrder::Big).unwrap();
        assert_eq!(v, Value::Number(-2.0));
    }

    #[test]
    fn v9_3_uint16_little_byte_order() {
        // Source bytes 34 12 decoded as little -> 0x1234 = 4660.
        let b = vec![0x34, 0x12];
        let v = decode_primitive(&b, DataType::Uint16, Endianness::Little, WordOrder::Big).unwrap();
        assert_eq!(v, Value::Number(4660.0));
        // ... and as big -> 0x3412 = 13330.
        let v2 = decode_primitive(&b, DataType::Uint16, Endianness::Big, WordOrder::Big).unwrap();
        assert_eq!(v2, Value::Number(13330.0));
    }

    #[test]
    fn v9_4_uint32_big_word() {
        let b = regs_to_bytes(&[0x0001, 0x0002]);
        let v = decode_primitive(&b, DataType::Uint32, Endianness::Big, WordOrder::Big).unwrap();
        assert_eq!(v, Value::Number(65538.0));
    }

    #[test]
    fn v9_5_uint32_little_word() {
        let b = regs_to_bytes(&[0x0002, 0x0001]);
        let v = decode_primitive(&b, DataType::Uint32, Endianness::Big, WordOrder::Little).unwrap();
        assert_eq!(v, Value::Number(65538.0));
    }

    #[test]
    fn v9_6_int32_negative_big_word() {
        let b = regs_to_bytes(&[0xffff, 0xfffe]);
        let v = decode_primitive(&b, DataType::Int32, Endianness::Big, WordOrder::Big).unwrap();
        assert_eq!(v, Value::Number(-2.0));
    }

    #[test]
    fn v9_7_float32_be_big_word() {
        let b = regs_to_bytes(&[0x422a, 0x0000]);
        let v = decode_primitive(&b, DataType::Float32, Endianness::Big, WordOrder::Big).unwrap();
        assert_eq!(v, Value::Number(42.5));
    }

    #[test]
    fn v9_8_float32_little_word() {
        let b = regs_to_bytes(&[0x0000, 0x422a]);
        let v = decode_primitive(&b, DataType::Float32, Endianness::Big, WordOrder::Little).unwrap();
        assert_eq!(v, Value::Number(42.5));
    }

    #[test]
    #[allow(clippy::approx_constant)] // 3.14159 is the normative §9.9 vector, not PI
    fn v9_9_float64_be_big_word() {
        let b = regs_to_bytes(&[0x4009, 0x21f9, 0xf01b, 0x866e]);
        let v = decode_primitive(&b, DataType::Float64, Endianness::Big, WordOrder::Big).unwrap();
        if let Value::Number(n) = v {
            assert!((n - 3.14159).abs() < 1e-9, "got {n}");
        } else {
            panic!("expected number");
        }
    }

    #[test]
    fn v9_13_bitfield_extraction() {
        let b = regs_to_bytes(&[0x01e0]); // 0b0000_0001_1110_0000
        let v = extract_bitfield(&b, Endianness::Big, WordOrder::Big, 5, 4);
        assert_eq!(v, 15);
    }

    #[test]
    fn v9_14_typed_write_roundtrip() {
        let buf = encode_primitive(
            &Value::Number(42.5),
            DataType::Float32,
            Endianness::Big,
            WordOrder::Big,
        )
        .unwrap();
        assert_eq!(buf, regs_to_bytes(&[0x422a, 0x0000]));
        // round-trip back
        let v = decode_primitive(&buf, DataType::Float32, Endianness::Big, WordOrder::Big).unwrap();
        assert_eq!(v, Value::Number(42.5));
    }

    #[test]
    fn uint64_outside_safe_range_is_string() {
        let b = (u64::MAX).to_be_bytes().to_vec();
        let v = decode_primitive(&b, DataType::Uint64, Endianness::Big, WordOrder::Big).unwrap();
        assert_eq!(v, Value::Text(u64::MAX.to_string()));
    }
}
