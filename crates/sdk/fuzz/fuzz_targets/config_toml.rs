//! Fuzz the contract-level configuration parser: arbitrary text must never panic, only parse
//! or fail. Connector configs are edited by hand and patched remotely via management
//! commands, so hostile/corrupt input is a normal operating condition.

#![no_main]

use libfuzzer_sys::fuzz_target;
use tedge_dot_sdk::config::{parse_duration, ConnectorConfig};

fuzz_target!(|data: &[u8]| {
    if let Ok(text) = std::str::from_utf8(data) {
        let _ = toml::from_str::<ConnectorConfig>(text);
        let _ = parse_duration(text);
    }
});
