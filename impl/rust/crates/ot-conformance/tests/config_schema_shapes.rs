//! The config schema must agree with the loader about which configurations are legal.
//!
//! Point libraries (contract §3.4) made this non-trivial: an inline point may now be a
//! *patch* of one a library supplies, carrying only `id`, so the completeness rules
//! (`address` required, typed needs a `datatype`) can only be demanded of a device that
//! inherits nothing. A schema that kept demanding them would reject configurations the
//! connector accepts — and this schema is what tooling validates against.

use ot_conformance::layer1::{Kind, Schemas};

fn device(extra: serde_json::Value) -> serde_json::Value {
    let mut d = serde_json::json!({ "name": "plc-1", "protocol_address": { "unit_id": 1 } });
    let obj = d.as_object_mut().unwrap();
    for (k, v) in extra.as_object().unwrap() {
        obj.insert(k.clone(), v.clone());
    }
    serde_json::json!({ "connector": { "protocol": "modbus" }, "device": [d] })
}

fn accepts(config: serde_json::Value) -> Result<(), String> {
    Schemas::load().unwrap().validate(Kind::Config, &config)
}

#[test]
fn a_device_that_inherits_may_patch_a_point_with_only_an_id() {
    accepts(device(serde_json::json!({
        "points_from": ["acme-meter"],
        "point": [{ "id": "boiler_temp", "unit": "K" }],
    })))
    .expect("a patch of an inherited point is legal");
}

#[test]
fn a_device_may_inherit_and_declare_no_points_of_its_own() {
    accepts(device(serde_json::json!({ "points_from": ["acme-meter"] })))
        .expect("connection information plus a reference is a complete device");
}

#[test]
fn a_self_contained_point_must_still_be_complete() {
    let err = accepts(device(serde_json::json!({
        "point": [{ "id": "boiler_temp" }],
    })))
    .expect_err("nothing supplies the address, so it must be required");
    assert!(err.contains("address"), "{err}");

    let err = accepts(device(serde_json::json!({
        "point": [{ "id": "t", "address": { "a": 1 } }],
    })))
    .expect_err("a point with nothing to inherit needs a datatype (§1: always required)");
    assert!(err.contains("datatype"), "{err}");
}

#[test]
fn a_complete_self_contained_point_is_accepted() {
    accepts(device(serde_json::json!({
        "point": [{ "id": "t", "address": { "a": 1 }, "datatype": "uint16" }],
    })))
    .expect("the pre-existing shape must keep validating");
}

/// A disabled device is not loaded (§3.3), so the loaders accept one carrying nothing but its
/// name — and the schema must not demand more than they do. Every other device still needs its
/// address, and `enabled` must be a boolean.
#[test]
fn a_disabled_device_needs_only_its_name() {
    let config = |device: serde_json::Value| {
        serde_json::json!({ "connector": { "protocol": "modbus" }, "device": [device] })
    };
    accepts(config(serde_json::json!({
        "name": "plc-9", "enabled": false, "point": [{ "id": "patch_only" }],
    })))
    .expect("nothing but the name of a disabled device is read");

    let err = accepts(config(serde_json::json!({ "name": "plc-9", "enabled": true })))
        .expect_err("an enabled device still needs its address");
    assert!(err.contains("protocol_address"), "{err}");

    accepts(device(serde_json::json!({ "enabled": "no" })))
        .expect_err("enabled must be a boolean");
}

#[test]
fn points_from_must_be_an_array_of_non_empty_strings() {
    accepts(device(serde_json::json!({ "points_from": "acme-meter" })))
        .expect_err("a bare string is not a reference list");
    accepts(device(serde_json::json!({ "points_from": [""] })))
        .expect_err("an empty reference names nothing");
}

#[test]
fn the_library_search_path_is_a_list_of_directories() {
    accepts(serde_json::json!({
        "connector": { "protocol": "modbus", "point_library_path": ["/etc/tedge/plugins/ot/points.d"] }
    }))
    .expect("an explicit search path is legal");
    accepts(serde_json::json!({
        "connector": { "protocol": "modbus", "point_library_path": "/etc/tedge/plugins/ot/points.d" }
    }))
    .expect_err("a single directory must still be a list");
}

/// The point-library schema composes config.schema.json's point definition through a `$ref`
/// rather than restating it. If that reference stopped resolving, the schema would accept
/// anything inside `point` and quietly stop being a check at all — so assert on a violation
/// that only the referenced definition can catch.
#[test]
fn the_point_library_schema_validates_points_through_the_config_schema() {
    let schemas = Schemas::load().unwrap();
    let library = |point: serde_json::Value| {
        serde_json::json!({ "library": { "protocol": "modbus" }, "point": [point] })
    };

    schemas
        .validate(
            Kind::PointLibrary,
            &library(serde_json::json!({
                "id": "boiler_temp", "datatype": "float32",
                "address": { "table": "holding", "address": 7, "count": 2 },
            })),
        )
        .expect("a well-formed library");

    let err = schemas
        .validate(
            Kind::PointLibrary,
            &library(serde_json::json!({ "id": "t", "datatype": "not_a_datatype", "address": {} })),
        )
        .expect_err("the referenced point definition must reject an unknown datatype");
    assert!(err.contains("datatype") || err.contains("not_a_datatype"), "{err}");

    let err = schemas
        .validate(
            Kind::PointLibrary,
            &library(serde_json::json!({ "id": "t", "address": {}, "bogus_field": 1 })),
        )
        .expect_err("the referenced point definition forbids unknown fields");
    assert!(err.contains("bogus_field"), "{err}");
}

#[test]
fn a_point_library_must_declare_its_protocol_and_at_least_one_point() {
    let schemas = Schemas::load().unwrap();
    let err = schemas
        .validate(
            Kind::PointLibrary,
            &serde_json::json!({ "point": [{ "id": "t", "address": {} }] }),
        )
        .expect_err("a library with no [library] protocol is not usable");
    assert!(err.contains("library"), "{err}");

    schemas
        .validate(
            Kind::PointLibrary,
            &serde_json::json!({ "library": { "protocol": "modbus" }, "point": [] }),
        )
        .expect_err("an empty library resolves a device to nothing");

    schemas
        .validate(
            Kind::PointLibrary,
            &serde_json::json!({
                "library": { "protocol": "modbus" },
                "point": [{ "id": "t", "address": {} }],
                "connector": { "protocol": "modbus" },
            }),
        )
        .expect_err("a file with a [connector] section is a config, not a library");
}
