//! Property-based tests for the SDK's shared decode/encode, transform, and formatting logic.
//!
//! `typed` mode must decode identically across every connector, so this is the layer where an
//! encoding bug corrupts *all* protocols at once. The properties here complement the normative
//! acceptance vectors in `decode.rs`: instead of checking known answers, they pin down the
//! invariants (round-trips, totality, reference models) across the whole input space.

use proptest::prelude::*;
use tedge_dot_sdk::config::parse_duration;
use tedge_dot_sdk::decode::{decode_primitive, encode_primitive, extract_bitfield, Endianness, WordOrder};
use tedge_dot_sdk::model::{hex_grouped, DataType, Transform, Value};

fn endianness() -> impl Strategy<Value = Endianness> {
    prop_oneof![Just(Endianness::Big), Just(Endianness::Little)]
}

fn word_order() -> impl Strategy<Value = WordOrder> {
    prop_oneof![Just(WordOrder::Big), Just(WordOrder::Little)]
}

fn any_datatype() -> impl Strategy<Value = DataType> {
    prop_oneof![
        Just(DataType::Bool),
        Just(DataType::Int8),
        Just(DataType::Uint8),
        Just(DataType::Int16),
        Just(DataType::Uint16),
        Just(DataType::Int32),
        Just(DataType::Uint32),
        Just(DataType::Int64),
        Just(DataType::Uint64),
        Just(DataType::Float32),
        Just(DataType::Float64),
        Just(DataType::String),
        Just(DataType::Bytes),
    ]
}

/// An integer datatype together with a value that fits it exactly (and stays inside the JS
/// safe-integer range, where decode is required to return `Value::Number`).
fn int_datatype_and_value() -> impl Strategy<Value = (DataType, i64)> {
    prop_oneof![
        (Just(DataType::Int8), (i8::MIN as i64)..=(i8::MAX as i64)),
        (Just(DataType::Uint8), 0i64..=(u8::MAX as i64)),
        (Just(DataType::Int16), (i16::MIN as i64)..=(i16::MAX as i64)),
        (Just(DataType::Uint16), 0i64..=(u16::MAX as i64)),
        (Just(DataType::Int32), (i32::MIN as i64)..=(i32::MAX as i64)),
        (Just(DataType::Uint32), 0i64..=(u32::MAX as i64)),
        (Just(DataType::Int64), -9_007_199_254_740_991i64..=9_007_199_254_740_991i64),
        (Just(DataType::Uint64), 0i64..=9_007_199_254_740_991i64),
    ]
}

proptest! {
    /// encode → decode is the identity for every integer datatype, endianness and word order.
    #[test]
    fn int_encode_decode_roundtrip(
        (datatype, value) in int_datatype_and_value(),
        end in endianness(),
        wo in word_order(),
    ) {
        let bytes = encode_primitive(&Value::Number(value as f64), datatype, end, wo).unwrap();
        prop_assert_eq!(bytes.len(), datatype.byte_len().unwrap());
        let decoded = decode_primitive(&bytes, datatype, end, wo).unwrap();
        prop_assert_eq!(decoded, Value::Number(value as f64));
    }

    /// encode → decode is the identity for finite floats (bit-exact for f32 sources).
    #[test]
    fn float32_encode_decode_roundtrip(v in any::<f32>(), end in endianness(), wo in word_order()) {
        prop_assume!(v.is_finite());
        let bytes = encode_primitive(&Value::Number(v as f64), DataType::Float32, end, wo).unwrap();
        let decoded = decode_primitive(&bytes, DataType::Float32, end, wo).unwrap();
        prop_assert_eq!(decoded, Value::Number(v as f64));
    }

    #[test]
    fn float64_encode_decode_roundtrip(v in any::<f64>(), end in endianness(), wo in word_order()) {
        prop_assume!(v.is_finite());
        let bytes = encode_primitive(&Value::Number(v), DataType::Float64, end, wo).unwrap();
        let decoded = decode_primitive(&bytes, DataType::Float64, end, wo).unwrap();
        prop_assert_eq!(decoded, Value::Number(v));
    }

    /// decode is total: arbitrary bytes with arbitrary parameters never panic — they return a
    /// value or a structured error.
    #[test]
    fn decode_never_panics(
        bytes in proptest::collection::vec(any::<u8>(), 0..64),
        datatype in any_datatype(),
        end in endianness(),
        wo in word_order(),
    ) {
        let _ = decode_primitive(&bytes, datatype, end, wo);
    }

    /// encode is total over arbitrary values (including NaN/infinite numbers and junk strings).
    #[test]
    fn encode_never_panics(
        v in prop_oneof![
            any::<f64>().prop_map(Value::Number),
            any::<bool>().prop_map(Value::Bool),
            ".*".prop_map(Value::Text),
        ],
        datatype in any_datatype(),
        end in endianness(),
        wo in word_order(),
    ) {
        let _ = encode_primitive(&v, datatype, end, wo);
    }

    /// 64-bit integers decode to `Number` inside the JS safe range and `Text` outside it —
    /// never a silently rounded double.
    #[test]
    fn int64_safe_range_boundary(v in any::<i64>(), end in endianness(), wo in word_order()) {
        let bytes = v.to_be_bytes().to_vec();
        // Rebuild the natural buffer for the given orders so decode sees a consistent input.
        let natural = encode_primitive(&Value::Text(v.to_string()), DataType::Int64, end, wo).unwrap();
        prop_assert_eq!(natural.len(), bytes.len());
        let decoded = decode_primitive(&natural, DataType::Int64, end, wo).unwrap();
        const SAFE: i64 = 9_007_199_254_740_991;
        if (-SAFE..=SAFE).contains(&v) {
            prop_assert_eq!(decoded, Value::Number(v as f64));
        } else {
            prop_assert_eq!(decoded, Value::Text(v.to_string()));
        }
    }

    /// The default transform is the identity for every finite number.
    #[test]
    fn transform_default_is_identity(v in any::<f64>()) {
        prop_assume!(v.is_finite());
        prop_assert_eq!(Transform::default().apply(Value::Number(v)), Value::Number(v));
    }

    /// Transform never panics and never manufactures NaN from finite inputs with sane
    /// parameters (the zero-divisor guard is part of the contract).
    #[test]
    fn transform_total_and_nan_free(
        v in -1e12f64..1e12,
        multiplier in -1e6f64..1e6,
        divisor in -1e6f64..1e6,
        decimal_shift in -9i32..=9,
        offset in -1e9f64..1e9,
    ) {
        let t = Transform { multiplier, divisor, decimal_shift, offset };
        match t.apply(Value::Number(v)) {
            Value::Number(out) => prop_assert!(!out.is_nan(), "NaN from finite inputs: {t:?} on {v}"),
            other => prop_assert!(false, "number in, non-number out: {other:?}"),
        }
    }

    /// Booleans and strings pass through any transform untouched.
    #[test]
    fn transform_passes_non_numbers(
        b in any::<bool>(),
        s in ".*",
        multiplier in any::<f64>(),
        offset in any::<f64>(),
    ) {
        let t = Transform { multiplier, divisor: 1.0, decimal_shift: 0, offset };
        prop_assert_eq!(t.apply(Value::Bool(b)), Value::Bool(b));
        prop_assert_eq!(t.apply(Value::Text(s.clone())), Value::Text(s));
    }

    /// hex_grouped is lossless: stripping the spaces and parsing the hex pairs recovers the
    /// original bytes, for every group size.
    #[test]
    fn hex_grouped_roundtrip(bytes in proptest::collection::vec(any::<u8>(), 0..64), group in 0usize..8) {
        let s = hex_grouped(&bytes, group);
        let joined: String = s.split(' ').collect();
        prop_assert_eq!(joined.len(), bytes.len() * 2);
        let parsed: Vec<u8> = (0..joined.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&joined[i..i + 2], 16).unwrap())
            .collect();
        prop_assert_eq!(parsed, bytes);
    }

    /// parse_duration is total over arbitrary strings.
    #[test]
    fn parse_duration_never_panics(s in ".*") {
        let _ = parse_duration(&s);
    }

    /// Well-formed duration strings parse to the exact expected duration.
    #[test]
    fn parse_duration_well_formed(n in 0u64..1_000_000) {
        prop_assert_eq!(parse_duration(&format!("{n}ms")), Some(std::time::Duration::from_millis(n)));
        prop_assert_eq!(parse_duration(&format!("{n}s")), Some(std::time::Duration::from_secs(n)));
    }

    /// extract_bitfield agrees with a bit-by-bit reference model built on the canonical
    /// big-endian buffer.
    #[test]
    fn bitfield_matches_reference(
        bytes in proptest::collection::vec(any::<u8>(), 1..8),
        start_bit in 0u32..64,
        bit_count in 1u32..64,
    ) {
        let got = extract_bitfield(&bytes, Endianness::Big, WordOrder::Big, start_bit, bit_count);
        // Reference: assemble the buffer into an integer (big-endian), then take bits one by
        // one — deliberately naive so a shared shift/mask bug can't hide in both sides.
        let mut acc: u128 = 0;
        for b in &bytes {
            acc = (acc << 8) | (*b as u128);
        }
        let mut expected: u64 = 0;
        for i in 0..bit_count.min(64) {
            let bit = (acc >> (start_bit + i)) & 1;
            expected |= (bit as u64) << i;
        }
        prop_assert_eq!(got, expected, "bytes={:?} start={} count={}", bytes, start_bit, bit_count);
    }

    /// Raw-mode `string` decode is total over arbitrary (possibly invalid UTF-8) bytes.
    #[test]
    fn string_decode_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..64)) {
        let v = decode_primitive(&bytes, DataType::String, Endianness::Big, WordOrder::Big).unwrap();
        prop_assert!(matches!(v, Value::Text(_)));
    }
}
