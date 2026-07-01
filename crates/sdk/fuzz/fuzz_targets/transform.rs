//! Fuzz the per-point linear transform over the full f64 space (including NaN, infinities and
//! subnormals): apply() must be total and must keep non-numeric values untouched.

#![no_main]

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use tedge_dot_sdk::model::{Transform, Value};

#[derive(Arbitrary, Debug)]
struct Input {
    value: f64,
    text: String,
    flag: bool,
    multiplier: f64,
    divisor: f64,
    decimal_shift: i32,
    offset: f64,
}

fuzz_target!(|input: Input| {
    let t = Transform {
        multiplier: input.multiplier,
        divisor: input.divisor,
        decimal_shift: input.decimal_shift,
        offset: input.offset,
    };
    let _ = t.apply(Value::Number(input.value));
    assert_eq!(t.apply(Value::Bool(input.flag)), Value::Bool(input.flag));
    assert_eq!(
        t.apply(Value::Text(input.text.clone())),
        Value::Text(input.text)
    );
});
