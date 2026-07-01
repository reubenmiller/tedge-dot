//! Fuzz the shared primitive decode/encode path: decode must be total over arbitrary wire
//! bytes, and integer values that decode successfully must re-encode to the same buffer.

#![no_main]

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use tedge_dot_sdk::decode::{decode_primitive, encode_primitive, extract_bitfield, Endianness, WordOrder};
use tedge_dot_sdk::model::{DataType, Value};

#[derive(Arbitrary, Debug)]
struct Input {
    bytes: Vec<u8>,
    datatype_tag: u8,
    little_endian: bool,
    little_word: bool,
    start_bit: u32,
    bit_count: u32,
}

fn datatype(tag: u8) -> DataType {
    match tag % 13 {
        0 => DataType::Bool,
        1 => DataType::Int8,
        2 => DataType::Uint8,
        3 => DataType::Int16,
        4 => DataType::Uint16,
        5 => DataType::Int32,
        6 => DataType::Uint32,
        7 => DataType::Int64,
        8 => DataType::Uint64,
        9 => DataType::Float32,
        10 => DataType::Float64,
        11 => DataType::String,
        _ => DataType::Bytes,
    }
}

fuzz_target!(|input: Input| {
    let dt = datatype(input.datatype_tag);
    let end = if input.little_endian { Endianness::Little } else { Endianness::Big };
    let wo = if input.little_word { WordOrder::Little } else { WordOrder::Big };

    let _ = extract_bitfield(&input.bytes, end, wo, input.start_bit % 128, input.bit_count % 128);

    if let Ok(value) = decode_primitive(&input.bytes, dt, end, wo) {
        let reencoded = encode_primitive(&value, dt, end, wo);
        // Integers are exact: a successful decode must round-trip to the identical buffer.
        // (Bool collapses non-zero bytes and floats canonicalize NaN, so only ints assert.)
        let int_as_number = matches!(
            dt,
            DataType::Int8
                | DataType::Uint8
                | DataType::Int16
                | DataType::Uint16
                | DataType::Int32
                | DataType::Uint32
                | DataType::Int64
                | DataType::Uint64
        ) && matches!(value, Value::Number(_) | Value::Text(_));
        if int_as_number {
            assert_eq!(
                reencoded.expect("decoded int must re-encode"),
                input.bytes,
                "int round-trip mismatch for {dt:?}"
            );
        }
    }
});
