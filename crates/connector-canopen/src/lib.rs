//! CANopen connector module (Linux SocketCAN + zencan-client SDO).
// On non-Linux platforms SocketCAN is unavailable; suppress dead-code diagnostics.
#![cfg_attr(not(target_os = "linux"), allow(dead_code, unused_mut))]
//!
//! Implements the [`Connector`](tedge_dot_sdk::Connector) trait against Linux SocketCAN using
//! the `zencan-client` crate for CANopen SDO transfers.
//!
//! The connector is **poll-based**: the SDK runtime calls `read_points()` on a configurable
//! interval per device. CANopen SDO upload (read) is used for all reads; SDO download (write)
//! is used for all writes. NMT and TPDO are out of scope for this version.
//!
//! # Platform support
//!
//! SocketCAN is Linux-only. On non-Linux platforms the connector compiles but all network
//! operations return `ConnectorError::Unsupported`. The config parsing and decode logic are
//! fully cross-platform and unit-tested on all platforms.

mod config;

pub use config::{CanopenConnection, NodeAddress, OdAddress};

use async_trait::async_trait;
use std::collections::HashMap;
use tedge_dot_sdk::{
    Access, Capabilities, CommandRequest, CommandResult, ConfigError, Connector, ConnectorConfig,
    ConnectorError, DataType, DeviceId, LinkReport, LinkStatus, Mode, PointRef,
    Quality, Sample, SampleSink, Transform, Value,
};
use time::OffsetDateTime;

const PROTOCOL: &str = "canopen";

// ─── Internal model ──────────────────────────────────────────────────────────

#[derive(Clone)]
struct CanopenPoint {
    od: OdAddress,
    mode: Mode,
    datatype: Option<DataType>,
    access: Access,
    unit: Option<String>,
    transform: Transform,
}

struct DeviceModel {
    node_id: u8,
    points: HashMap<String, CanopenPoint>,
}

// ─── Public connector ────────────────────────────────────────────────────────

#[derive(Default)]
pub struct CanopenConnector {
    interface: Option<String>,
    devices: HashMap<DeviceId, DeviceModel>,
    #[cfg(target_os = "linux")]
    bus: Option<zencan_client::BusManager<zencan_client::common::SocketCanSender>>,
}

pub fn factory() -> Box<dyn Connector> {
    Box::<CanopenConnector>::default()
}

#[async_trait]
impl Connector for CanopenConnector {
    fn configure(&mut self, config: &ConnectorConfig) -> Result<(), ConfigError> {
        let conn: CanopenConnection =
            serde_json::from_value(config.connection.clone()).map_err(|e| {
                ConfigError::Invalid(format!("connection block: {e}"))
            })?;
        self.interface = Some(conn.interface);
        self.devices.clear();

        for d in &config.devices {
            let addr: NodeAddress =
                serde_json::from_value(d.protocol_address.clone()).map_err(|e| {
                    ConfigError::Invalid(format!("device '{}' protocol_address: {e}", d.name))
                })?;
            addr.validate()
                .map_err(|e| ConfigError::Invalid(format!("device '{}': {e}", d.name)))?;

            let mut points = HashMap::new();
            for p in &d.points {
                let od: OdAddress =
                    serde_json::from_value(p.address.clone()).map_err(|e| {
                        ConfigError::Invalid(format!("point '{}' address: {e}", p.id))
                    })?;

                let mode = p.resolved_mode(d.default_mode);
                if mode == Mode::Typed && p.datatype.is_none() {
                    return Err(ConfigError::Invalid(format!(
                        "point '{}' is typed but has no datatype",
                        p.id
                    )));
                }

                points.insert(
                    p.id.clone(),
                    CanopenPoint {
                        od,
                        mode,
                        datatype: p.datatype,
                        access: Access::parse(p.access.as_deref()),
                        unit: p.unit.clone(),
                        transform: p.transform.unwrap_or_default(),
                    },
                );
            }

            self.devices.insert(
                d.name.clone(),
                DeviceModel {
                    node_id: addr.node_id,
                    points,
                },
            );
        }
        Ok(())
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            protocol: PROTOCOL,
            version: env!("CARGO_PKG_VERSION"),
            modes: vec![Mode::Raw, Mode::Typed],
            datatypes: vec![
                DataType::Bool,
                DataType::Int8,
                DataType::Uint8,
                DataType::Int16,
                DataType::Uint16,
                DataType::Int32,
                DataType::Uint32,
                DataType::Int64,
                DataType::Uint64,
                DataType::Float32,
                DataType::Float64,
                DataType::String,
            ],
            point_kinds: vec!["object".into()],
            command_verbs: vec!["write".into()],
            features: vec!["management".into()],
            subscribe: false,
        }
    }

    async fn connect(&mut self) -> Result<Vec<LinkReport>, ConnectorError> {
        self.platform_connect().await
    }

    async fn read_points(
        &mut self,
        device: &DeviceId,
        points: &[PointRef],
    ) -> Result<Vec<Sample>, ConnectorError> {
        self.platform_read_points(device, points).await
    }

    async fn subscribe(
        &mut self,
        _device: &DeviceId,
        _points: &[PointRef],
        _sink: SampleSink,
    ) -> Result<(), ConnectorError> {
        Err(ConnectorError::Unsupported(
            "CANopen connector does not support push subscribe; use poll mode".into(),
        ))
    }

    async fn execute(
        &mut self,
        device: &DeviceId,
        verb: &str,
        request: &CommandRequest,
    ) -> Result<CommandResult, ConnectorError> {
        if verb != "write" {
            return Err(ConnectorError::Unsupported(verb.to_string()));
        }
        self.platform_execute(device, request).await
    }

    async fn disconnect(&mut self) -> Result<(), ConnectorError> {
        self.platform_disconnect().await
    }
}

// ─── Linux-only SocketCAN + zencan-client implementation ─────────────────────

#[cfg(target_os = "linux")]
impl CanopenConnector {
    async fn platform_connect(
        &mut self,
    ) -> Result<Vec<LinkReport>, ConnectorError> {
        let iface = self
            .interface
            .as_deref()
            .ok_or_else(|| ConnectorError::Transport("not configured".into()))?;

        let (tx, rx) = zencan_client::common::open_socketcan(iface).map_err(|e| {
            ConnectorError::Transport(format!("open SocketCAN {iface}: {e}"))
        })?;

        let mut bus = zencan_client::BusManager::new(tx, rx);

        // Broadcast NMT Start (node 0 = all nodes) to bring nodes into Operational state.
        bus.nmt_start(0).await;

        // Give nodes a moment to start up before probing.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let device_names: Vec<(DeviceId, u8)> = self
            .devices
            .iter()
            .map(|(name, m)| (name.clone(), m.node_id))
            .collect();

        let mut reports = Vec::new();
        for (name, node_id) in device_names {
            // Probe: try to read the identity object (0x1018:0 = number of sub-entries).
            let mut sdo = bus.sdo_client(node_id);
            match sdo.read_u8(0x1018, 0).await {
                Ok(_) => {
                    info!("CANopen node {node_id} on {iface} responded (device {name})");
                    reports.push(LinkReport {
                        device: name,
                        status: LinkStatus::Connected,
                        reason: None,
                        info: Some(serde_json::json!({
                            "interface": iface,
                            "node_id": node_id,
                        })),
                    });
                }
                Err(e) => {
                    warn!("CANopen node {node_id} on {iface} probe failed: {e}");
                    reports.push(LinkReport {
                        device: name,
                        status: LinkStatus::Disconnected,
                        reason: Some(format!("SDO probe failed: {e}")),
                        info: Some(serde_json::json!({
                            "interface": iface,
                            "node_id": node_id,
                        })),
                    });
                }
            }
        }

        self.bus = Some(bus);
        Ok(reports)
    }

    async fn platform_read_points(
        &mut self,
        device: &DeviceId,
        points: &[PointRef],
    ) -> Result<Vec<Sample>, ConnectorError> {
        let dev_model = self
            .devices
            .get(device)
            .ok_or_else(|| ConnectorError::NotConnected(device.clone()))?;
        let node_id = dev_model.node_id;

        let bus = self
            .bus
            .as_mut()
            .ok_or_else(|| ConnectorError::NotConnected(device.clone()))?;

        let ts = OffsetDateTime::now_utc();
        let mut samples = Vec::with_capacity(points.len());

        for pr in points {
            let pt = match dev_model.points.get(&pr.id) {
                Some(p) => p.clone(),
                None => {
                    return Err(ConnectorError::UnknownPoint {
                        device: device.clone(),
                        point: pr.id.clone(),
                    })
                }
            };

            let addr = serde_json::json!({
                "index": pt.od.index,
                "subindex": pt.od.subindex,
            });

            let mut sdo = bus.sdo_client(node_id);
            let raw_bytes = match sdo.upload(pt.od.index, pt.od.subindex).await {
                Ok(b) => b,
                Err(e) => {
                    warn!("SDO upload {device}/{} 0x{:04X}:{:02X} failed: {e}",
                        pr.id, pt.od.index, pt.od.subindex);
                    samples.push(make_bad_sample(device, &pr.id, &e.to_string(), ts, addr));
                    continue;
                }
            };

            let sample = match pt.mode {
                Mode::Raw => Sample {
                    ts,
                    device: device.clone(),
                    protocol: PROTOCOL,
                    point: pr.id.clone(),
                    mode: Mode::Raw,
                    datatype: None,
                    value: None,
                    raw: raw_bytes,
                    raw_group: 1,
                    quality: Quality::Good,
                    unit: pt.unit.clone(),
                    addr,
                    seq: None,
                    error: None,
                },
                Mode::Typed => {
                    let dt = match pt.datatype {
                        Some(d) => d,
                        None => {
                            samples.push(make_bad_sample(
                                device, &pr.id, "typed point missing datatype", ts, addr,
                            ));
                            continue;
                        }
                    };
                    // CANopen is little-endian on the wire.
                    match decode_primitive(&raw_bytes, dt, Endianness::Little, WordOrder::Big) {
                        Ok(value) => {
                            let value = pt.transform.apply(value);
                            Sample {
                                ts,
                                device: device.clone(),
                                protocol: PROTOCOL,
                                point: pr.id.clone(),
                                mode: Mode::Typed,
                                datatype: Some(dt),
                                value: Some(value),
                                raw: raw_bytes,
                                raw_group: 1,
                                quality: Quality::Good,
                                unit: pt.unit.clone(),
                                addr,
                                seq: None,
                                error: None,
                            }
                        }
                        Err(e) => {
                            make_bad_sample(device, &pr.id, &e.to_string(), ts, addr)
                        }
                    }
                }
            };
            samples.push(sample);
        }
        Ok(samples)
    }

    async fn platform_execute(
        &mut self,
        device: &DeviceId,
        request: &CommandRequest,
    ) -> Result<CommandResult, ConnectorError> {
        let pt = self
            .devices
            .get(device)
            .and_then(|d| d.points.get(&request.point))
            .cloned()
            .ok_or_else(|| ConnectorError::UnknownPoint {
                device: device.clone(),
                point: request.point.clone(),
            })?;

        if !pt.access.can_write() {
            return Err(ConnectorError::AccessDenied(request.point.clone()));
        }

        let node_id = self
            .devices
            .get(device)
            .map(|d| d.node_id)
            .ok_or_else(|| ConnectorError::NotConnected(device.clone()))?;

        let bus = self
            .bus
            .as_mut()
            .ok_or_else(|| ConnectorError::NotConnected(device.clone()))?;

        let bytes: Vec<u8> = if let Some(raw_hex) = &request.raw {
            hex_decode(raw_hex)
                .map_err(|e| ConnectorError::Other(format!("invalid raw hex: {e}")))?
        } else if let Some(json_val) = &request.value {
            let dt = pt.datatype.ok_or_else(|| {
                ConnectorError::Other("typed write requires datatype on the point".into())
            })?;
            let sdk_value = json_to_value(json_val, dt)?;
            // CANopen is little-endian on the wire.
            encode_primitive(&sdk_value, dt, Endianness::Little, WordOrder::Big)
                .map_err(|e| ConnectorError::Other(format!("encode failed: {e}")))?
        } else {
            return Err(ConnectorError::Other(
                "write command must supply either 'value' or 'raw'".into(),
            ));
        };

        let mut sdo = bus.sdo_client(node_id);
        sdo.download(pt.od.index, pt.od.subindex, &bytes)
            .await
            .map_err(|e| ConnectorError::Transport(format!("SDO download failed: {e}")))?;

        Ok(CommandResult {
            point: request.point.clone(),
            value: request.value.clone(),
            raw: request.raw.clone(),
        })
    }

    async fn platform_disconnect(&mut self) -> Result<(), ConnectorError> {
        self.bus = None;
        Ok(())
    }
}

// ─── Stub for non-Linux platforms ────────────────────────────────────────────

#[cfg(not(target_os = "linux"))]
impl CanopenConnector {
    async fn platform_connect(
        &mut self,
    ) -> Result<Vec<LinkReport>, ConnectorError> {
        let reports = self
            .devices
            .keys()
            .map(|name| LinkReport {
                device: name.clone(),
                status: LinkStatus::Disconnected,
                reason: Some("SocketCAN is only available on Linux".into()),
                info: None,
            })
            .collect();
        Ok(reports)
    }

    async fn platform_read_points(
        &mut self,
        _device: &DeviceId,
        _points: &[PointRef],
    ) -> Result<Vec<Sample>, ConnectorError> {
        Err(ConnectorError::Unsupported(
            "SocketCAN is only available on Linux".into(),
        ))
    }

    async fn platform_execute(
        &mut self,
        _device: &DeviceId,
        _request: &CommandRequest,
    ) -> Result<CommandResult, ConnectorError> {
        Err(ConnectorError::Unsupported(
            "SocketCAN is only available on Linux".into(),
        ))
    }

    async fn platform_disconnect(&mut self) -> Result<(), ConnectorError> {
        Ok(())
    }
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

fn make_bad_sample(
    device: &DeviceId,
    point_id: &str,
    error: &str,
    ts: OffsetDateTime,
    addr: serde_json::Value,
) -> Sample {
    Sample {
        ts,
        device: device.clone(),
        protocol: PROTOCOL,
        point: point_id.to_string(),
        mode: Mode::Typed,
        datatype: None,
        value: None,
        raw: vec![],
        raw_group: 1,
        quality: Quality::Bad,
        unit: None,
        addr,
        seq: None,
        error: Some(error.to_string()),
    }
}

fn json_to_value(v: &serde_json::Value, dt: DataType) -> Result<Value, ConnectorError> {
    let val = match v {
        serde_json::Value::Bool(b) => Value::Bool(*b),
        serde_json::Value::Number(n) => Value::Number(
            n.as_f64()
                .ok_or_else(|| ConnectorError::Other("number out of f64 range".into()))?,
        ),
        serde_json::Value::String(s) => Value::Text(s.clone()),
        _ => {
            return Err(ConnectorError::Other(format!(
                "unsupported JSON value type for datatype {dt:?}"
            )))
        }
    };
    Ok(val)
}

fn hex_decode(s: &str) -> Result<Vec<u8>, String> {
    let s = s.trim();
    if s.len() % 2 != 0 {
        return Err("odd number of hex digits".into());
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|e| e.to_string()))
        .collect()
}
