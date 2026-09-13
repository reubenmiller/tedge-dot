//! `tedge-dot-sdk` — the runtime, `Connector` trait, shared model, and decode helpers
//! that every thin-edge.io OT protocol module builds on.
//!
//! See the [OT Connector Contract](../../../doc/proposal/contract/ot-connector-contract.md) and
//! the [Connector SDK spec](../../../doc/proposal/sdk/connector-sdk.md).

pub mod config;
pub mod conformance;
pub mod commands;
pub mod connector;
pub mod decode;
pub mod descriptor;
pub mod library;
pub mod manifest;
pub mod model;
pub mod runtime;

pub use config::{parse_duration, ConnectorConfig, DeviceConfig, PointConfig};
pub use library::{load as load_config, resolve as resolve_config};
pub use manifest::CONTRACT_VERSION;
pub use connector::{
    Access, Capabilities, CommandRequest, CommandResult, ConfigError, Connector, ConnectorError,
    LinkReport, LinkStatus, PointRef, SampleSink,
};
pub use descriptor::{parameters, parameters_of, Parameter};
pub use decode::{decode_primitive, encode_primitive, extract_bitfield, DecodeError, Endianness, WordOrder};
pub use model::{DataType, DeviceId, PointId, Quality, Sample, Transform, Value};
