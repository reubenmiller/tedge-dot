//! Fuzz the sample envelope serializer: any Sample a connector can construct must serialize
//! to valid JSON without panicking, whatever bytes, ids or units it carries.

#![no_main]

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use tedge_dot_sdk::model::{DataType, Mode, Quality, Sample, Value};
use time::OffsetDateTime;

#[derive(Arbitrary, Debug)]
struct Input {
    unix_ts: i64,
    device: String,
    point: String,
    typed: bool,
    value_tag: u8,
    number: f64,
    text: String,
    flag: bool,
    raw: Vec<u8>,
    raw_group: usize,
    quality_tag: u8,
    unit: Option<String>,
    seq: Option<u64>,
    error: Option<String>,
}

fuzz_target!(|input: Input| {
    let ts = OffsetDateTime::from_unix_timestamp(input.unix_ts.rem_euclid(253_402_300_799))
        .unwrap_or(OffsetDateTime::UNIX_EPOCH);
    let value = match input.value_tag % 4 {
        0 => None,
        1 => Some(Value::Number(input.number)),
        2 => Some(Value::Bool(input.flag)),
        _ => Some(Value::Text(input.text.clone())),
    };
    let sample = Sample {
        ts,
        device: input.device,
        protocol: "fuzz",
        point: input.point,
        mode: if input.typed { Mode::Typed } else { Mode::Raw },
        datatype: Some(DataType::Float64),
        value,
        raw: input.raw,
        raw_group: input.raw_group,
        quality: match input.quality_tag % 3 {
            0 => Quality::Good,
            1 => Quality::Bad,
            _ => Quality::Stale,
        },
        unit: input.unit,
        addr: serde_json::json!({ "text": input.text }),
        seq: input.seq,
        error: input.error,
    };
    let envelope = sample.to_envelope();
    // The envelope must always be serializable JSON.
    let _ = envelope.to_string();
});
