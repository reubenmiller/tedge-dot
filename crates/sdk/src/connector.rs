//! The `Connector` trait every protocol module implements, plus the supporting types the SDK
//! passes in and out of it.

use crate::config::ConnectorConfig;
use crate::decode::{Endianness, WordOrder};
use crate::model::{DataType, DeviceId, Mode, Sample, Transform};
use async_trait::async_trait;
use thiserror::Error;

/// Access permitted on a point.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    Read,
    Write,
    ReadWrite,
}

impl Access {
    pub fn parse(s: Option<&str>) -> Self {
        match s.map(|s| s.to_ascii_lowercase()).as_deref() {
            Some("write") => Access::Write,
            Some("read_write") | Some("readwrite") => Access::ReadWrite,
            _ => Access::Read,
        }
    }

    pub fn can_write(self) -> bool {
        matches!(self, Access::Write | Access::ReadWrite)
    }
}

/// A resolved reference to a point the runtime asks the connector to read. The protocol-specific
/// address is looked up by the connector using `device` + `id`.
#[derive(Clone, Debug)]
pub struct PointRef {
    pub id: String,
    pub mode: Mode,
    pub datatype: Option<DataType>,
    pub endianness: Endianness,
    pub word_order: WordOrder,
    pub access: Access,
    pub unit: Option<String>,
    /// Per-point linear transform applied to the decoded numeric value.
    pub transform: Transform,
}

/// The connector's declared capabilities; drives the capability descriptor and conformance.
#[derive(Clone, Debug)]
pub struct Capabilities {
    pub protocol: &'static str,
    pub version: &'static str,
    pub modes: Vec<Mode>,
    pub datatypes: Vec<DataType>,
    pub point_kinds: Vec<String>,
    pub command_verbs: Vec<String>,
    pub features: Vec<String>,
    pub subscribe: bool,
}

impl Capabilities {
    /// Serialize to the capability descriptor JSON (contract §7).
    pub fn to_json(&self) -> serde_json::Value {
        let modes: Vec<&str> = self
            .modes
            .iter()
            .map(|m| match m {
                Mode::Raw => "raw",
                Mode::Typed => "typed",
            })
            .collect();
        let datatypes: Vec<serde_json::Value> = self
            .datatypes
            .iter()
            .map(|d| serde_json::to_value(d).unwrap())
            .collect();
        serde_json::json!({
            "protocol": self.protocol,
            "version": self.version,
            "modes": modes,
            "datatypes": datatypes,
            "point_kinds": self.point_kinds,
            "command_verbs": self.command_verbs,
            "features": self.features,
            "subscribe": self.subscribe,
        })
    }
}

/// Per-device connection status.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkStatus {
    Connected,
    Disconnected,
    Degraded,
}

impl LinkStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            LinkStatus::Connected => "connected",
            LinkStatus::Disconnected => "disconnected",
            LinkStatus::Degraded => "degraded",
        }
    }
}

#[derive(Clone, Debug)]
pub struct LinkReport {
    pub device: DeviceId,
    pub status: LinkStatus,
    pub reason: Option<String>,
    /// Optional device descriptor (transport/address details) published on the link status topic.
    /// Flows (e.g. ot-registration) can forward this into a digital-twin fragment. `None` omits it.
    pub info: Option<serde_json::Value>,
}

impl LinkReport {
    /// A link report without a device descriptor.
    pub fn new(device: DeviceId, status: LinkStatus, reason: Option<String>) -> Self {
        LinkReport {
            device,
            status,
            reason,
            info: None,
        }
    }
}

/// A parsed command request (contract §6).
#[derive(Clone, Debug)]
pub struct CommandRequest {
    pub point: String,
    pub value: Option<serde_json::Value>,
    pub value_repr: Option<String>,
    pub raw: Option<String>,
}

/// The outcome of a successful command (the runtime wraps it in the result envelope).
#[derive(Clone, Debug, Default)]
pub struct CommandResult {
    pub point: String,
    pub value: Option<serde_json::Value>,
    pub raw: Option<String>,
}

/// Sink for event-driven (`subscribe`) connectors to push samples into.
pub type SampleSink = tokio::sync::mpsc::Sender<Sample>;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("invalid configuration: {0}")]
    Invalid(String),
}

#[derive(Debug, Error)]
pub enum ConnectorError {
    #[error("not connected to device {0}")]
    NotConnected(DeviceId),
    #[error("unknown point {point} on device {device}")]
    UnknownPoint { device: DeviceId, point: String },
    #[error("access denied: point {0} is not writable")]
    AccessDenied(String),
    #[error("transport error: {0}")]
    Transport(String),
    #[error("decode error: {0}")]
    Decode(String),
    #[error("unsupported operation: {0}")]
    Unsupported(String),
    #[error("{0}")]
    Other(String),
}

/// A protocol module. One instance manages the connection(s) for one configured connector.
///
/// Only `Send` is required (not `Sync`): the SDK runtime drives a connector from a single task,
/// so a connector may hold non-`Sync` transport handles (e.g. a `tokio-modbus` `Context`).
#[async_trait]
pub trait Connector: Send {
    /// Validate & parse the protocol-specific parts of the configuration into a typed model.
    fn configure(&mut self, config: &ConnectorConfig) -> Result<(), ConfigError>;

    /// Declare what this connector supports. Must be cheap and pure.
    fn capabilities(&self) -> Capabilities;

    /// Establish protocol connections to all configured devices.
    async fn connect(&mut self) -> Result<Vec<LinkReport>, ConnectorError>;

    /// Read a batch of points for one device. Returns one [`Sample`] per requested point
    /// (including `bad`/`stale` samples).
    async fn read_points(
        &mut self,
        device: &DeviceId,
        points: &[PointRef],
    ) -> Result<Vec<Sample>, ConnectorError>;

    /// OPTIONAL: for event-driven protocols. Default = polling only.
    async fn subscribe(
        &mut self,
        device: &DeviceId,
        points: &[PointRef],
        sink: SampleSink,
    ) -> Result<(), ConnectorError> {
        let _ = (device, points, sink);
        Err(ConnectorError::Unsupported("subscribe".into()))
    }

    /// OPTIONAL: execute a command verb (default supports nothing).
    async fn execute(
        &mut self,
        device: &DeviceId,
        verb: &str,
        request: &CommandRequest,
    ) -> Result<CommandResult, ConnectorError> {
        let _ = (device, request);
        Err(ConnectorError::Unsupported(verb.to_string()))
    }

    /// Close connections cleanly. Called on shutdown and before reload.
    async fn disconnect(&mut self) -> Result<(), ConnectorError>;
}
