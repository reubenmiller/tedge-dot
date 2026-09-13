//! thin-edge command types (contract §6, RFC 0006 §7).
//!
//! `ot_write` is not a cloud concept and not a thin-edge core concept: it is a command type
//! *this project defines*, whose payload is the contract's own write request and whose state
//! machine is the contract's own. So the runtime subscribes to the thin-edge command topics and
//! drives them itself, rather than having a pair of flows move a message between two topics
//! that mean the same thing.
//!
//! ```text
//! te/device/<device>///cmd/ot_write/<id>              a write to one point
//! te/device/<device>///cmd/ot_write_batch/<id>        a batch of writes
//! te/device/<device>///cmd/ot_set_config/<id>         management, claimed by payload.service
//! te/device/main/service/<service>/cmd/ot_set_config/<id>   management, addressed directly
//! ```
//!
//! A command type maps to one contract verb. Aliases (`[connector] command_aliases`) add more
//! names for a verb the connector already implements — the Cumulocity mapper needs one command
//! type per operation, and that is a cloud constraint, not a protocol one.

use std::collections::BTreeMap;

/// The thin-edge command type for each contract verb (§6).
const VERB_TYPES: [(&str, &str); 5] = [
    ("write", "ot_write"),
    ("write-batch", "ot_write_batch"),
    ("set-config", "ot_set_config"),
    ("define-device", "ot_define_device"),
    ("remove-device", "ot_remove_device"),
];

/// The management verbs, which change this instance's configuration (§6.3).
pub const MANAGEMENT_VERBS: [&str; 3] = ["set-config", "define-device", "remove-device"];

/// The thin-edge command type a contract verb is published and answered as.
pub fn type_of_verb(verb: &str) -> Option<&'static str> {
    VERB_TYPES
        .iter()
        .find(|(v, _)| *v == verb)
        .map(|(_, t)| *t)
}

/// The contract verb a thin-edge command type stands for, following `command_aliases` first.
///
/// An alias is resolved exactly once: it names a *canonical* command type, not another alias,
/// so a configuration cannot build a cycle the runtime would have to detect.
pub fn verb_of_type(command_type: &str, aliases: &BTreeMap<String, String>) -> Option<&'static str> {
    let canonical = aliases
        .get(command_type)
        .map(String::as_str)
        .unwrap_or(command_type);
    VERB_TYPES
        .iter()
        .find(|(_, t)| *t == canonical)
        .map(|(v, _)| *v)
}

/// Every thin-edge command type this connector answers on a *device* topic: the write verbs it
/// implements, plus every alias of one. Management types are not here — they are answered on a
/// device topic too (§6.6), but they are a property of the service, not of the device.
///
/// This is what `manifest.commands` advertises and what the retained capability markers are
/// published for, so a mapper and an operator see the same list.
pub fn device_command_types(
    module_verbs: &[String],
    aliases: &BTreeMap<String, String>,
) -> Vec<String> {
    let mut types: Vec<String> = Vec::new();
    let mut push = |t: &str| {
        if !types.iter().any(|x| x == t) {
            types.push(t.to_string());
        }
    };
    for verb in module_verbs {
        if MANAGEMENT_VERBS.contains(&verb.as_str()) {
            continue;
        }
        if let Some(t) = type_of_verb(verb) {
            push(t);
        }
    }
    // An alias is advertised only when the verb it stands for is one this connector answers.
    for (alias, canonical) in aliases {
        let Some(verb) = VERB_TYPES.iter().find(|(_, t)| t == canonical).map(|(v, _)| *v) else {
            continue;
        };
        if MANAGEMENT_VERBS.contains(&verb) {
            continue;
        }
        if module_verbs.iter().any(|v| v == verb) {
            push(alias);
        }
    }
    types
}

/// The retained capability marker topic thin-edge expects for one command type on a device.
pub fn capability_topic(device: &str, command_type: &str) -> String {
    format!("te/device/{device}///cmd/{command_type}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn aliases() -> BTreeMap<String, String> {
        BTreeMap::from([("ot_write_coil".to_string(), "ot_write".to_string())])
    }

    #[test]
    fn verbs_and_types_round_trip() {
        assert_eq!(type_of_verb("write"), Some("ot_write"));
        assert_eq!(type_of_verb("write-batch"), Some("ot_write_batch"));
        assert_eq!(type_of_verb("set-config"), Some("ot_set_config"));
        assert_eq!(type_of_verb("nope"), None);

        let none = BTreeMap::new();
        assert_eq!(verb_of_type("ot_write", &none), Some("write"));
        assert_eq!(verb_of_type("ot_remove_device", &none), Some("remove-device"));
        assert_eq!(verb_of_type("ot_write_coil", &none), None, "not an alias here");
        // ...and with the packaged alias it is the same verb as ot_write.
        assert_eq!(verb_of_type("ot_write_coil", &aliases()), Some("write"));
    }

    /// The manifest advertises what a device actually answers: the write types, plus aliases of
    /// them, and never a management type (those belong to the service).
    #[test]
    fn device_command_types_cover_the_write_verbs_and_their_aliases() {
        let verbs: Vec<String> = ["write", "write-batch", "set-config"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            device_command_types(&verbs, &aliases()),
            ["ot_write", "ot_write_batch", "ot_write_coil"]
        );

        // An alias of a verb the connector does NOT implement is not advertised: a marker for a
        // command nothing answers would leave an operation hanging at `init`.
        let read_only: Vec<String> = vec![];
        assert!(device_command_types(&read_only, &aliases()).is_empty());
    }

    #[test]
    fn capability_marker_is_the_thin_edge_command_topic() {
        assert_eq!(
            capability_topic("plc1", "ot_write"),
            "te/device/plc1///cmd/ot_write"
        );
    }
}
