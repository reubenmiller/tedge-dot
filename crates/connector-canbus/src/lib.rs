//! CAN bus connector module (Linux SocketCAN + DBC).
// On non-Linux platforms the SocketCAN helpers are compiled out; suppress the
// resulting dead-code / unused-mut diagnostics so CI stays clean everywhere.
#![cfg_attr(not(target_os = "linux"), allow(dead_code, unused_mut))]
//
//!
//! Implements the [`Connector`](tedge_dot_sdk::Connector) trait against Linux SocketCAN.
//! The driver stays "dumb": it opens a CAN socket, subscribes to frames, extracts signal
//! bit-fields, and decodes them via DBC metadata. All DBC `factor`/`offset` scaling,
//! renaming, units, alarms, and thin-edge JSON shaping are handled by flows.
//!
//! CAN is push-based: the connector implements `subscribe()` and returns `Unsupported` from
//! `read_points()`.
//!
//! # Platform support
//!
//! SocketCAN is Linux-only. On non-Linux platforms the connector compiles but all network
//! operations return `ConnectorError::Unsupported`. The bit-extraction/encoding logic and
//! DBC config parsing are fully cross-platform and unit-tested on all platforms.

mod config;

pub use config::{
    load_dbc, resolve_signal,
    CanByteOrder, CanInterface, CanSignalAddress, CanbusConnection, ResolvedSignal, SignalValueType,
};

use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;
use tedge_dot_sdk::{
    Access, Capabilities, CommandRequest, CommandResult, ConfigError, Connector, ConnectorConfig,
    ConnectorError, DataType, DeviceId, LinkReport, LinkStatus, Mode, PointRef, Quality, Sample,
    SampleSink, Transform, Value,
};
use time::OffsetDateTime;
use tokio::sync::Mutex;
#[cfg(target_os = "linux")]
use tracing::{info, warn};

const PROTOCOL: &str = "canbus";

/// Maximum number of bytes in a classic CAN frame payload.
pub const CLASSIC_PAYLOAD_LEN: usize = 8;

/// Largest integer representable exactly as an f64 (JS Number.MAX_SAFE_INTEGER).
const MAX_SAFE_INT: i64 = 9_007_199_254_740_991;

// ─── Internal model ──────────────────────────────────────────────────────────

#[derive(Clone)]
struct CanPoint {
    signal: ResolvedSignal,
    mode: Mode,
    datatype: Option<DataType>,
    access: Access,
    unit: Option<String>,
    transform: Transform,
}

struct DeviceModel {
    interface: CanInterface,
    points: HashMap<String, CanPoint>,
}

/// Per-device runtime state (last-frame cache + subscribe task handle).
struct DeviceState {
    iface_name: String,
    last_frame: Arc<Mutex<HashMap<u32, [u8; CLASSIC_PAYLOAD_LEN]>>>,
    _task: Option<tokio::task::JoinHandle<()>>,
}

// ─── Public connector ────────────────────────────────────────────────────────

#[derive(Default)]
pub struct CanbusConnector {
    devices: HashMap<String, DeviceModel>,
    state: HashMap<String, DeviceState>,
}

pub fn factory() -> Box<dyn Connector> {
    Box::<CanbusConnector>::default()
}

#[async_trait]
impl Connector for CanbusConnector {
    fn configure(&mut self, config: &ConnectorConfig) -> Result<(), ConfigError> {
        let _: CanbusConnection =
            serde_json::from_value(config.connection.clone()).unwrap_or_default();
        self.devices.clear();

        for d in &config.devices {
            let iface: CanInterface =
                serde_json::from_value(d.protocol_address.clone()).map_err(|e| {
                    ConfigError::Invalid(format!("device '{}' protocol_address: {e}", d.name))
                })?;

            let dbc = config::load_dbc(&iface.dbc_file)
                .map_err(|e| ConfigError::Invalid(format!("device '{}' dbc_file: {e}", d.name)))?;

            let mut points = HashMap::new();
            for p in &d.points {
                let addr: CanSignalAddress =
                    serde_json::from_value(p.address.clone()).map_err(|e| {
                        ConfigError::Invalid(format!("point '{}' address: {e}", p.id))
                    })?;

                let signal =
                    config::resolve_signal(&dbc, &addr.message_name, &addr.signal_name)
                        .map_err(|e| ConfigError::Invalid(format!("point '{}': {e}", p.id)))?;

                let mode = p.resolved_mode(d.default_mode);
                if mode == Mode::Typed && p.datatype.is_none() {
                    return Err(ConfigError::Invalid(format!(
                        "point '{}' is typed but has no datatype",
                        p.id
                    )));
                }

                points.insert(
                    p.id.clone(),
                    CanPoint {
                        signal,
                        mode,
                        datatype: p.datatype,
                        access: Access::parse(p.access.as_deref()),
                        unit: p.unit.clone(),
                        transform: p.transform.unwrap_or_default(),
                    },
                );
            }

            self.devices
                .insert(d.name.clone(), DeviceModel { interface: iface, points });
        }
        Ok(())
    }

    fn capabilities(&self) -> Capabilities {
        let mut features = vec!["subscribe".into(), "management".into()];
        #[cfg(feature = "canbus-fd")]
        features.push("canbus-fd".into());

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
            ],
            point_kinds: vec!["signal".into()],
            command_verbs: vec!["write".into()],
            features,
            subscribe: true,
        }
    }

    async fn connect(&mut self) -> Result<Vec<LinkReport>, ConnectorError> {
        self.platform_connect().await
    }

    async fn read_points(
        &mut self,
        _device: &DeviceId,
        _points: &[PointRef],
    ) -> Result<Vec<Sample>, ConnectorError> {
        Err(ConnectorError::Unsupported(
            "canbus is push-only; use subscribe".into(),
        ))
    }

    async fn subscribe(
        &mut self,
        device: &DeviceId,
        points: &[PointRef],
        sink: SampleSink,
    ) -> Result<(), ConnectorError> {
        self.platform_subscribe(device, points, sink).await
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
        // Abort push loops explicitly: dropping a JoinHandle detaches the task, which would
        // leave the old subscription publishing after a config reload re-subscribes.
        for state in self.state.values_mut() {
            if let Some(task) = state._task.take() {
                task.abort();
            }
        }
        self.state.clear();
        Ok(())
    }
}

// ─── Linux-only SocketCAN implementation ─────────────────────────────────────

#[cfg(target_os = "linux")]
mod linux_impl {
    use super::*;
    use socketcan::{CanFilter, CanFrame, EmbeddedFrame, ExtendedId, Frame, Id, Socket, SocketOptions, StandardId};
    use tracing::debug;

    impl CanbusConnector {
        pub(super) async fn platform_connect(
            &mut self,
        ) -> Result<Vec<LinkReport>, ConnectorError> {
            let names: Vec<String> = self.devices.keys().cloned().collect();
            let mut reports = Vec::new();
            for name in names {
                let iface_name = self.devices[&name].interface.interface.clone();
                match socketcan::CanSocket::open(&iface_name) {
                    Ok(_probe) => {
                        info!("SocketCAN interface {iface_name} available for device {name}");
                        let last_frame = Arc::new(Mutex::new(HashMap::new()));
                        self.state.insert(
                            name.clone(),
                            DeviceState { iface_name: iface_name.clone(), last_frame, _task: None },
                        );
                        reports.push(LinkReport {
                            device: name,
                            status: LinkStatus::Connected,
                            reason: None,
                            info: Some(serde_json::json!({ "interface": iface_name })),
                        });
                    }
                    Err(e) => {
                        warn!("failed to open {iface_name}: {e}");
                        reports.push(LinkReport {
                            device: name,
                            status: LinkStatus::Disconnected,
                            reason: Some(e.to_string()),
                            info: Some(serde_json::json!({ "interface": iface_name })),
                        });
                    }
                }
            }
            Ok(reports)
        }

        pub(super) async fn platform_subscribe(
            &mut self,
            device: &DeviceId,
            points: &[PointRef],
            sink: SampleSink,
        ) -> Result<(), ConnectorError> {
            let dev_model = self
                .devices
                .get(device)
                .ok_or_else(|| ConnectorError::NotConnected(device.clone()))?;

            // Only push the requested points; anything else stays on the caller's schedule.
            let mut id_map: HashMap<u32, Vec<(String, CanPoint)>> = HashMap::new();
            for r in points {
                let pt = dev_model.points.get(&r.id).ok_or_else(|| {
                    ConnectorError::UnknownPoint {
                        device: device.clone(),
                        point: r.id.clone(),
                    }
                })?;
                id_map
                    .entry(pt.signal.can_id)
                    .or_default()
                    .push((r.id.clone(), pt.clone()));
            }

            let state = self
                .state
                .get_mut(device)
                .ok_or_else(|| ConnectorError::NotConnected(device.clone()))?;

            let iface_name = state.iface_name.clone();
            let last_frame = state.last_frame.clone();
            let device_id = device.clone();

            let async_socket = socketcan::tokio::CanSocket::open(&iface_name).map_err(|e| {
                ConnectorError::Transport(format!("subscribe open {iface_name}: {e}"))
            })?;
            let filters: Vec<CanFilter> =
                id_map.keys().map(|&id| CanFilter::new(id, 0x1FFF_FFFF)).collect();
            if let Err(e) = async_socket.set_filters(&filters) {
                warn!("could not set CAN hardware filters on {iface_name}: {e}");
            }

            let task = tokio::spawn(async move {
                subscribe_loop(async_socket, device_id, iface_name, id_map, last_frame, sink).await;
            });
            state._task = Some(task);
            Ok(())
        }

        pub(super) async fn platform_execute(
            &mut self,
            device: &DeviceId,
            request: &CommandRequest,
        ) -> Result<CommandResult, ConnectorError> {
            let point = self
                .devices
                .get(device)
                .and_then(|d| d.points.get(&request.point))
                .cloned()
                .ok_or_else(|| ConnectorError::UnknownPoint {
                    device: device.clone(),
                    point: request.point.clone(),
                })?;

            if !point.access.can_write() {
                return Err(ConnectorError::AccessDenied(request.point.clone()));
            }

            let state = self
                .state
                .get(device)
                .ok_or_else(|| ConnectorError::NotConnected(device.clone()))?;

            let mut payload = {
                let cache = state.last_frame.lock().await;
                cache.get(&point.signal.can_id).copied().unwrap_or([0u8; CLASSIC_PAYLOAD_LEN])
            };

            if let Some(raw_hex) = &request.raw {
                let bytes = hex_decode(raw_hex)
                    .map_err(|e| ConnectorError::Other(format!("invalid raw hex: {e}")))?;
                if bytes.len() > CLASSIC_PAYLOAD_LEN {
                    return Err(ConnectorError::Other(format!(
                        "raw payload {} bytes exceeds classic CAN max {CLASSIC_PAYLOAD_LEN}",
                        bytes.len()
                    )));
                }
                payload[..bytes.len()].copy_from_slice(&bytes);
            } else if let Some(value) = &request.value {
                let dt = point.datatype.ok_or_else(|| {
                    ConnectorError::Other("typed write requires datatype on the point".into())
                })?;
                let bits = value_to_bits(value, dt, &point.signal)?;
                encode_can_signal(&mut payload, &point.signal, bits);
            } else {
                return Err(ConnectorError::Other(
                    "write command must supply either 'value' or 'raw'".into(),
                ));
            }

            let can_id: Id = if point.signal.can_id <= 0x7FF {
                StandardId::new(point.signal.can_id as u16)
                    .map(Id::Standard)
                    .ok_or_else(|| ConnectorError::Other("invalid standard CAN ID".into()))?
            } else {
                ExtendedId::new(point.signal.can_id)
                    .map(Id::Extended)
                    .ok_or_else(|| ConnectorError::Other("invalid extended CAN ID".into()))?
            };
            let frame = CanFrame::new(can_id, &payload)
                .ok_or_else(|| ConnectorError::Other("cannot build CAN frame".into()))?;

            let iface_name = &state.iface_name;
            let write_socket = socketcan::CanSocket::open(iface_name).map_err(|e| {
                ConnectorError::Transport(format!("write open {iface_name}: {e}"))
            })?;
            write_socket.write_frame(&frame).map_err(|e| ConnectorError::Transport(e.to_string()))?;

            Ok(CommandResult {
                point: request.point.clone(),
                value: request.value.clone(),
                raw: request.raw.clone(),
            })
        }
    }

    async fn subscribe_loop(
        socket: socketcan::tokio::CanSocket,
        device: DeviceId,
        iface_name: String,
        id_map: HashMap<u32, Vec<(String, CanPoint)>>,
        last_frame: Arc<Mutex<HashMap<u32, [u8; CLASSIC_PAYLOAD_LEN]>>>,
        sink: SampleSink,
    ) {
        loop {
            match socket.read_frame().await {
                Ok(frame) => {
                    let can_id = frame.raw_id() & 0x1FFF_FFFF;
                    let data = frame.data();
                    let mut payload = [0u8; CLASSIC_PAYLOAD_LEN];
                    let copy_len = data.len().min(CLASSIC_PAYLOAD_LEN);
                    payload[..copy_len].copy_from_slice(&data[..copy_len]);
                    { last_frame.lock().await.insert(can_id, payload); }

                    if let Some(points) = id_map.get(&can_id) {
                        let ts = OffsetDateTime::now_utc();
                        for (pid, pt) in points {
                            let sample = build_sample(&device, pid, pt, &payload, ts);
                            if sink.send(sample).await.is_err() {
                                debug!("sink closed, stopping subscribe loop for {iface_name}");
                                return;
                            }
                        }
                    }
                }
                Err(e) => {
                    warn!("recv error on {iface_name}: {e}");
                    let ts = OffsetDateTime::now_utc();
                    for points in id_map.values() {
                        for (pid, _) in points {
                            let _ = sink.send(make_bad_sample(&device, pid, &e.to_string(), ts)).await;
                        }
                    }
                    return;
                }
            }
        }
    }
}

// ─── Stub for non-Linux platforms ────────────────────────────────────────────

#[cfg(not(target_os = "linux"))]
impl CanbusConnector {
    async fn platform_connect(&mut self) -> Result<Vec<LinkReport>, ConnectorError> {
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

    async fn platform_subscribe(
        &mut self,
        _device: &DeviceId,
        _points: &[PointRef],
        _sink: SampleSink,
    ) -> Result<(), ConnectorError> {
        Err(ConnectorError::Unsupported("SocketCAN is only available on Linux".into()))
    }

    async fn platform_execute(
        &mut self,
        _device: &DeviceId,
        _request: &CommandRequest,
    ) -> Result<CommandResult, ConnectorError> {
        Err(ConnectorError::Unsupported("SocketCAN is only available on Linux".into()))
    }
}

// ─── Sample helpers ──────────────────────────────────────────────────────────

fn build_sample(device: &DeviceId, point_id: &str, pt: &CanPoint, payload: &[u8], ts: OffsetDateTime) -> Sample {
    let addr = serde_json::json!({ "can_id": format!("0x{:X}", pt.signal.can_id) });
    match pt.mode {
        Mode::Raw => Sample {
            ts,
            device: device.clone(),
            protocol: PROTOCOL,
            point: point_id.to_string(),
            mode: Mode::Raw,
            datatype: None,
            value: None,
            raw: payload.to_vec(),
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
                None => return make_bad_sample(device, point_id, "typed point missing datatype", ts),
            };
            let extracted = extract_can_signal(payload, &pt.signal);
            let value = match decode_signal(extracted, dt, &pt.signal) {
                Ok(v) => v,
                Err(e) => return make_bad_sample(device, point_id, &e, ts),
            };
            let value = pt.transform.apply(value);
            Sample {
                ts,
                device: device.clone(),
                protocol: PROTOCOL,
                point: point_id.to_string(),
                mode: Mode::Typed,
                datatype: Some(dt),
                value: Some(value),
                raw: payload.to_vec(),
                raw_group: 1,
                quality: Quality::Good,
                unit: pt.unit.clone(),
                addr,
                seq: None,
                error: None,
            }
        }
    }
}

fn make_bad_sample(device: &DeviceId, point_id: &str, error: &str, ts: OffsetDateTime) -> Sample {
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
        addr: serde_json::Value::Null,
        seq: None,
        error: Some(error.to_string()),
    }
}

// ─── CAN signal bit extraction / encoding (spec §4.1) ────────────────────────

/// Extract a CAN signal from `frame_bytes` using the DBC bit layout.
/// Returns the raw unsigned value; caller must apply type interpretation (§4.2).
pub fn extract_can_signal(frame_bytes: &[u8], signal: &ResolvedSignal) -> u64 {
    match signal.byte_order {
        CanByteOrder::Intel => extract_intel(frame_bytes, signal.start_bit, signal.bit_count),
        CanByteOrder::Motorola => extract_motorola(frame_bytes, signal.start_bit, signal.bit_count),
    }
}

/// Intel (little-endian): `start_bit` is the LSBit position; bit 0 = bit 0 of byte 0.
fn extract_intel(bytes: &[u8], start_bit: u16, bit_count: u8) -> u64 {
    let mut raw = 0u64;
    for (i, &b) in bytes.iter().enumerate().take(8) {
        raw |= (b as u64) << (i * 8);
    }
    let mask = if bit_count >= 64 { u64::MAX } else { (1u64 << bit_count) - 1 };
    (raw >> start_bit) & mask
}

/// Motorola (big-endian): `start_bit` is the MSBit using DBC Motorola numbering.
/// Byte 0: bits 7..0, byte 1: bits 15..8, etc.
/// Bits are traversed from MSBit downward: within a byte, bit N → bit N-1;
/// crossing a byte boundary (bit 8 → bit 7) is handled by simple decrement.
fn extract_motorola(bytes: &[u8], start_bit: u16, bit_count: u8) -> u64 {
    let mut result: u64 = 0;
    let mut current_bit = start_bit as i32;
    for _ in 0..bit_count {
        let byte_idx = (current_bit / 8) as usize;
        let bit_idx = (current_bit % 8) as u32;
        let bit_val = if byte_idx < bytes.len() { (bytes[byte_idx] >> bit_idx) & 1 } else { 0 };
        result = (result << 1) | (bit_val as u64);
        current_bit -= 1;
    }
    result
}

/// Encode `value` into `payload` at the signal's bit position (read-modify-write).
pub fn encode_can_signal(payload: &mut [u8; CLASSIC_PAYLOAD_LEN], signal: &ResolvedSignal, value: u64) {
    match signal.byte_order {
        CanByteOrder::Intel => encode_intel(payload, signal.start_bit, signal.bit_count, value),
        CanByteOrder::Motorola => encode_motorola(payload, signal.start_bit, signal.bit_count, value),
    }
}

fn encode_intel(payload: &mut [u8; CLASSIC_PAYLOAD_LEN], start_bit: u16, bit_count: u8, value: u64) {
    let mask = if bit_count >= 64 { u64::MAX } else { (1u64 << bit_count) - 1 };
    let masked = value & mask;
    for i in 0..bit_count {
        let abs_bit = start_bit + i as u16;
        let byte_idx = (abs_bit / 8) as usize;
        let bit_idx = abs_bit % 8;
        if byte_idx < CLASSIC_PAYLOAD_LEN {
            let bit_val = ((masked >> i) & 1) as u8;
            payload[byte_idx] = (payload[byte_idx] & !(1 << bit_idx)) | (bit_val << bit_idx);
        }
    }
}

fn encode_motorola(payload: &mut [u8; CLASSIC_PAYLOAD_LEN], start_bit: u16, bit_count: u8, value: u64) {
    let mask = if bit_count >= 64 { u64::MAX } else { (1u64 << bit_count) - 1 };
    let masked = value & mask;
    let mut current_bit = start_bit as i32;
    for i in 0..bit_count {
        let bit_val = ((masked >> (bit_count - 1 - i)) & 1) as u8;
        let byte_idx = (current_bit / 8) as usize;
        let bit_idx = (current_bit % 8) as u32;
        if byte_idx < CLASSIC_PAYLOAD_LEN {
            payload[byte_idx] = (payload[byte_idx] & !(1 << bit_idx)) | (bit_val << bit_idx);
        }
        current_bit -= 1;
    }
}

// ─── Value decode / encode helpers ───────────────────────────────────────────

fn decode_signal(raw: u64, dt: DataType, signal: &ResolvedSignal) -> Result<Value, String> {
    match dt {
        DataType::Bool => Ok(Value::Bool(raw != 0)),
        DataType::Uint8 | DataType::Uint16 | DataType::Uint32 => Ok(Value::Number(raw as f64)),
        DataType::Uint64 => {
            if raw > MAX_SAFE_INT as u64 { Ok(Value::Text(raw.to_string())) } else { Ok(Value::Number(raw as f64)) }
        }
        DataType::Int8 | DataType::Int16 | DataType::Int32 | DataType::Int64 => {
            let signed = sign_extend(raw, signal.bit_count);
            if !(-MAX_SAFE_INT..=MAX_SAFE_INT).contains(&signed) {
                Ok(Value::Text(signed.to_string()))
            } else {
                Ok(Value::Number(signed as f64))
            }
        }
        DataType::Float32 => Ok(Value::Number(f32::from_bits(raw as u32) as f64)),
        DataType::Float64 => Ok(Value::Number(f64::from_bits(raw))),
        DataType::String | DataType::Bytes => {
            Err(format!("datatype {dt:?} is not supported for CAN signals"))
        }
    }
}

fn sign_extend(value: u64, bit_count: u8) -> i64 {
    if bit_count == 0 || bit_count >= 64 { return value as i64; }
    let sign_bit = 1u64 << (bit_count - 1);
    if value & sign_bit != 0 { (value | (u64::MAX << bit_count)) as i64 } else { value as i64 }
}

fn value_to_bits(json_value: &serde_json::Value, dt: DataType, signal: &ResolvedSignal) -> Result<u64, ConnectorError> {
    match dt {
        DataType::Bool => match json_value {
            serde_json::Value::Bool(b) => Ok(*b as u64),
            serde_json::Value::Number(n) => Ok(n.as_f64().unwrap_or(0.0) as u64),
            _ => Err(ConnectorError::Other("bool value must be true/false".into())),
        },
        DataType::Float32 => {
            let f = json_value.as_f64().ok_or_else(|| ConnectorError::Other("float32 value must be a number".into()))? as f32;
            Ok(f.to_bits() as u64)
        }
        DataType::Float64 => {
            let f = json_value.as_f64().ok_or_else(|| ConnectorError::Other("float64 value must be a number".into()))?;
            Ok(f.to_bits())
        }
        _ => {
            let n = json_value.as_i64()
                .or_else(|| json_value.as_f64().map(|f| f as i64))
                .ok_or_else(|| ConnectorError::Other("integer value expected".into()))?;
            let mask = if signal.bit_count >= 64 { u64::MAX } else { (1u64 << signal.bit_count) - 1 };
            Ok((n as u64) & mask)
        }
    }
}

fn hex_decode(s: &str) -> Result<Vec<u8>, String> {
    let s = s.trim();
    if !s.len().is_multiple_of(2) { return Err("odd number of hex digits".into()); }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|e| e.to_string())).collect()
}

// ─── Unit tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{CanByteOrder, ResolvedSignal, SignalValueType};

    fn intel_sig(start_bit: u16, bit_count: u8) -> ResolvedSignal {
        ResolvedSignal { can_id: 0x1A0, start_bit, bit_count, byte_order: CanByteOrder::Intel, value_type: SignalValueType::Unsigned }
    }
    fn motorola_sig(start_bit: u16, bit_count: u8) -> ResolvedSignal {
        ResolvedSignal { can_id: 0x1A0, start_bit, bit_count, byte_order: CanByteOrder::Motorola, value_type: SignalValueType::Unsigned }
    }

    #[test]
    fn intel_u8_start0() {
        let bytes = [0xABu8, 0, 0, 0, 0, 0, 0, 0];
        assert_eq!(extract_can_signal(&bytes, &intel_sig(0, 8)), 171);
    }
    #[test]
    fn intel_u8_cross_byte() {
        let bytes = [0xABu8, 0x0C, 0, 0, 0, 0, 0, 0];
        assert_eq!(extract_can_signal(&bytes, &intel_sig(4, 8)), 202);
    }
    #[test]
    fn intel_signed_negative() {
        let bytes = [0xFEu8, 0, 0, 0, 0, 0, 0, 0];
        let raw = extract_can_signal(&bytes, &intel_sig(0, 8));
        assert_eq!(raw, 254);
        assert_eq!(sign_extend(raw, 8), -2);
    }
    #[test]
    fn intel_u16_little_endian() {
        let bytes = [0xD0u8, 0x07, 0, 0, 0, 0, 0, 0];
        assert_eq!(extract_can_signal(&bytes, &intel_sig(0, 16)), 2000);
    }
    #[test]
    fn intel_bool_bit8() {
        let bytes = [0x00u8, 0x01, 0, 0, 0, 0, 0, 0];
        assert_eq!(extract_can_signal(&bytes, &intel_sig(8, 1)), 1);
    }
    #[test]
    fn motorola_u8_single_byte() {
        let bytes = [0xABu8, 0, 0, 0, 0, 0, 0, 0];
        assert_eq!(extract_can_signal(&bytes, &motorola_sig(7, 8)), 0xAB);
    }
    #[test]
    fn motorola_u10_span() {
        let bytes = [0xC0u8, 0x3A, 0, 0, 0, 0, 0, 0];
        assert_eq!(extract_can_signal(&bytes, &motorola_sig(15, 10)), 235);
    }
    #[test]
    fn intel_f32_42() {
        let bytes = [0x00u8, 0x00, 0x28, 0x42, 0, 0, 0, 0];
        let raw = extract_can_signal(&bytes, &intel_sig(0, 32));
        let f = f32::from_bits(raw as u32);
        assert!((f - 42.0f32).abs() < 1e-6);
    }
    #[test]
    fn intel_encode_decode_u16_roundtrip() {
        let sig = intel_sig(0, 16);
        let mut payload = [0u8; CLASSIC_PAYLOAD_LEN];
        encode_can_signal(&mut payload, &sig, 2500);
        assert_eq!(extract_can_signal(&payload, &sig), 2500);
    }
    #[test]
    fn motorola_encode_decode_u8_roundtrip() {
        let sig = motorola_sig(7, 8);
        let mut payload = [0u8; CLASSIC_PAYLOAD_LEN];
        encode_can_signal(&mut payload, &sig, 0xAB);
        assert_eq!(extract_can_signal(&payload, &sig), 0xAB);
    }
    #[test]
    fn decode_bool_true() {
        let sig = intel_sig(0, 1);
        assert_eq!(decode_signal(1, DataType::Bool, &sig).unwrap(), Value::Bool(true));
    }
    #[test]
    fn decode_int8_negative() {
        let sig = intel_sig(0, 8);
        assert_eq!(decode_signal(0xFE, DataType::Int8, &sig).unwrap(), Value::Number(-2.0));
    }
    #[test]
    fn hex_decode_basic() {
        assert_eq!(hex_decode("deadbeef").unwrap(), vec![0xDE, 0xAD, 0xBE, 0xEF]);
        assert!(hex_decode("abc").is_err());
    }
}
