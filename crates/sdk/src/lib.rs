//! `tedge-dot-sdk` — the runtime, `Connector` trait, shared model, and decode helpers
//! that every thin-edge.io OT protocol module builds on.
//!
//! See the [OT Connector Contract](../../../doc/proposal/contract/ot-connector-contract.md) and
//! the [Connector SDK spec](../../../doc/proposal/sdk/connector-sdk.md).

pub mod config;
pub mod conformance;
pub mod connector;
pub mod decode;
pub mod model;
pub mod runtime;

pub use config::{parse_duration, ConnectorConfig, DeviceConfig, PointConfig};
pub use connector::{
    Access, Capabilities, CommandRequest, CommandResult, ConfigError, Connector, ConnectorError,
    LinkReport, LinkStatus, PointRef, SampleSink,
};
pub use decode::{decode_primitive, encode_primitive, extract_bitfield, DecodeError, Endianness, WordOrder};
pub use model::{DataType, DeviceId, Mode, PointId, Quality, Sample, Transform, Value};
