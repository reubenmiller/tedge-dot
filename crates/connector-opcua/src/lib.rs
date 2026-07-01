//! OPC-UA connector module.
//!
//! Implements the [`Connector`](tedge_dot_sdk::Connector) trait against
//! [`async-opcua`]. Like the Modbus reference module the driver stays "dumb": it connects, reads
//! node values (polling), writes node values, and pushes data-change notifications
//! (`subscribe`, via one OPC-UA subscription per device with one monitored item per point).
//! All scaling, renaming, units, alarms and thin-edge JSON shaping are handled by
//! thin-edge.io flows.
//!
//! Addressing uses OPC-UA `NodeId`s (textual `ns=2;s=Temperature` or structured
//! `namespace`+`identifier`) instead of Modbus register tables, exercising the contract's opaque
//! address slots with a very different protocol.

mod config;

pub use config::{NodeAddress, OpcuaConnection, OpcuaEndpoint};

use async_trait::async_trait;
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tedge_dot_sdk::{
    Access, Capabilities, CommandRequest, CommandResult, ConfigError, Connector, ConnectorConfig,
    ConnectorError, DataType, DeviceId, LinkReport, LinkStatus, Mode, PointRef, Quality, Sample,
    SampleSink, Transform, Value,
};
use time::OffsetDateTime;

use opcua::client::{ClientBuilder, DataChangeCallback, IdentityToken, Session};
use opcua::crypto::SecurityPolicy;
use opcua::types::{
    AttributeId, DataValue, MessageSecurityMode, MonitoredItemCreateRequest, MonitoringMode,
    MonitoringParameters, NodeId, ReadValueId, StatusCode, TimestampsToReturn, UAString,
    UserTokenPolicy, Variant, WriteValue,
};

const PROTOCOL: &str = "opcua";

/// Sampling-interval fallback for monitored items when a point has no resolved poll interval.
const DEFAULT_SAMPLING_INTERVAL: Duration = Duration::from_millis(500);

/// Largest integer representable exactly as an `f64` (JS `Number.MAX_SAFE_INTEGER`).
const MAX_SAFE_INT: i64 = 9_007_199_254_740_991;

/// A fully-resolved OPC-UA point (node id + decode parameters), built in `configure`.
#[derive(Clone)]
struct OpcuaPoint {
    node_id: NodeId,
    mode: Mode,
    datatype: Option<DataType>,
    access: Access,
    unit: Option<String>,
    transform: Transform,
}

struct DeviceModel {
    endpoint: OpcuaEndpoint,
    points: HashMap<String, OpcuaPoint>,
}

/// A live session and the background task driving its event loop.
struct SessionHandle {
    session: Arc<Session>,
    _event_loop: tokio::task::JoinHandle<StatusCode>,
}

/// A live push subscription for one device: the server-side subscription id plus the forwarder
/// task bridging data-change callbacks into the runtime's sample channel.
struct SubscriptionHandle {
    subscription_id: u32,
    forwarder: tokio::task::JoinHandle<()>,
}

/// The OPC-UA connector. One instance manages all configured OPC-UA servers.
#[derive(Default)]
pub struct OpcuaConnector {
    conn: OpcuaConnection,
    devices: HashMap<String, DeviceModel>,
    sessions: HashMap<String, SessionHandle>,
    subscriptions: HashMap<String, SubscriptionHandle>,
}

/// Factory used by the binary to instantiate the module behind its feature flag.
pub fn factory() -> Box<dyn Connector> {
    Box::<OpcuaConnector>::default()
}

#[async_trait]
impl Connector for OpcuaConnector {
    fn configure(&mut self, config: &ConnectorConfig) -> Result<(), ConfigError> {
        self.conn = serde_json::from_value(config.connection.clone()).unwrap_or_default();
        self.devices.clear();

        for d in &config.devices {
            let endpoint: OpcuaEndpoint = serde_json::from_value(d.protocol_address.clone())
                .map_err(|e| {
                    ConfigError::Invalid(format!("device '{}' protocol_address: {e}", d.name))
                })?;

            let mut points = HashMap::new();
            for p in &d.points {
                let addr: NodeAddress = serde_json::from_value(p.address.clone()).map_err(|e| {
                    ConfigError::Invalid(format!("point '{}' address: {e}", p.id))
                })?;
                let node_id = node_id_from(&addr).map_err(|e| {
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
                    OpcuaPoint {
                        node_id,
                        mode,
                        datatype: p.datatype,
                        access: Access::parse(p.access.as_deref()),
                        unit: p.unit.clone(),
                        transform: p.transform.unwrap_or_default(),
                    },
                );
            }
            self.devices
                .insert(d.name.clone(), DeviceModel { endpoint, points });
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
            point_kinds: vec!["variable".into()],
            command_verbs: vec!["write".into()],
            features: vec!["polling".into(), "subscribe".into()],
            subscribe: true,
        }
    }

    async fn connect(&mut self) -> Result<Vec<LinkReport>, ConnectorError> {
        let names: Vec<String> = self.devices.keys().cloned().collect();
        let mut reports = Vec::new();
        for name in names {
            let endpoint = self.devices[&name].endpoint.clone();
            match connect_device(&self.conn, &endpoint).await {
                Ok(handle) => {
                    self.sessions.insert(name.clone(), handle);
                    reports.push(LinkReport {
                        device: name,
                        status: LinkStatus::Connected,
                        reason: None,
                        info: None,
                    });
                }
                Err(e) => reports.push(LinkReport {
                    device: name,
                    status: LinkStatus::Disconnected,
                    reason: Some(e),
                    info: None,
                }),
            }
        }
        Ok(reports)
    }

    async fn read_points(
        &mut self,
        device: &DeviceId,
        points: &[PointRef],
    ) -> Result<Vec<Sample>, ConnectorError> {
        // Resolve per-point models first, ending the borrow on `self.devices`.
        let models: Vec<(String, Option<OpcuaPoint>)> = match self.devices.get(device) {
            Some(dev) => points
                .iter()
                .map(|p| (p.id.clone(), dev.points.get(&p.id).cloned()))
                .collect(),
            None => points.iter().map(|p| (p.id.clone(), None)).collect(),
        };

        let session = match self.sessions.get(device) {
            Some(h) => h.session.clone(),
            None => {
                return Ok(models
                    .into_iter()
                    .map(|(id, model)| {
                        bad_sample(&id, model.as_ref(), "device not connected")
                    })
                    .collect());
            }
        };

        // Build the read request for the known points (skip unknown ones, reported separately).
        let mut known: Vec<(String, OpcuaPoint)> = Vec::new();
        let mut reads: Vec<ReadValueId> = Vec::new();
        let mut out: Vec<Sample> = Vec::new();
        for (id, model) in models {
            match model {
                Some(m) => {
                    reads.push(ReadValueId {
                        node_id: m.node_id.clone(),
                        attribute_id: AttributeId::Value as u32,
                        index_range: Default::default(),
                        data_encoding: Default::default(),
                    });
                    known.push((id, m));
                }
                None => out.push(bad_sample(&id, None, "unknown point")),
            }
        }

        if !reads.is_empty() {
            match session.read(&reads, TimestampsToReturn::Neither, 0.0).await {
                Ok(values) => {
                    for ((id, model), dv) in known.iter().zip(values) {
                        let mut sample = build_sample(id, model, &dv);
                        // Contract §5: polled samples carry the read-completion time. Servers
                        // may return a (stale) source timestamp even for TimestampsToReturn::
                        // Neither; only the push path reports event time.
                        sample.ts = OffsetDateTime::now_utc();
                        out.push(sample);
                    }
                }
                Err(status) => {
                    for (id, model) in &known {
                        out.push(bad_sample(id, Some(model), &format!("read failed: {status}")));
                    }
                }
            }
        }
        Ok(out)
    }

    async fn subscribe(
        &mut self,
        device: &DeviceId,
        points: &[PointRef],
        sink: SampleSink,
    ) -> Result<(), ConnectorError> {
        if points.is_empty() {
            return Ok(());
        }
        let dev = self
            .devices
            .get(device)
            .ok_or_else(|| ConnectorError::NotConnected(device.clone()))?;

        // Resolve the requested points and their sampling intervals up front.
        let mut items: Vec<(String, OpcuaPoint, Duration)> = Vec::with_capacity(points.len());
        for r in points {
            let model = dev.points.get(&r.id).cloned().ok_or_else(|| {
                ConnectorError::UnknownPoint {
                    device: device.clone(),
                    point: r.id.clone(),
                }
            })?;
            items.push((
                r.id.clone(),
                model,
                r.interval.unwrap_or(DEFAULT_SAMPLING_INTERVAL),
            ));
        }

        let session = self
            .sessions
            .get(device)
            .ok_or_else(|| ConnectorError::NotConnected(device.clone()))?
            .session
            .clone();

        // Notifications are matched back to points by node id (several points may share one).
        let mut by_node: HashMap<NodeId, Vec<(String, OpcuaPoint)>> = HashMap::new();
        for (id, model, _) in &items {
            by_node
                .entry(model.node_id.clone())
                .or_default()
                .push((id.clone(), model.clone()));
        }

        // The data-change callback is synchronous, so it forwards through an unbounded channel
        // to a spawned task that awaits the runtime's (bounded) sink.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Sample>();
        let device_name = device.clone();
        let callback = DataChangeCallback::new(move |dv, item| {
            let node_id = &item.item_to_monitor().node_id;
            if let Some(models) = by_node.get(node_id) {
                for (id, model) in models {
                    let mut sample = build_sample(id, model, &dv);
                    sample.device = device_name.clone();
                    // Ignore send errors: the forwarder has already shut down.
                    let _ = tx.send(sample);
                }
            }
        });

        // One subscription per device, publishing at the fastest requested point rate.
        let publishing_interval = items
            .iter()
            .map(|(_, _, interval)| *interval)
            .min()
            .unwrap_or(DEFAULT_SAMPLING_INTERVAL);
        let subscription_id = session
            .create_subscription(publishing_interval, 60, 20, 0, 0, true, callback)
            .await
            .map_err(|s| ConnectorError::Transport(format!("create_subscription failed: {s}")))?;

        // One monitored item per point, sampled at the point's resolved poll interval.
        let requests: Vec<MonitoredItemCreateRequest> = items
            .iter()
            .map(|(_, model, interval)| {
                MonitoredItemCreateRequest::new(
                    model.node_id.clone().into(),
                    MonitoringMode::Reporting,
                    MonitoringParameters {
                        sampling_interval: interval.as_millis() as f64,
                        queue_size: 1,
                        discard_oldest: true,
                        ..Default::default()
                    },
                )
            })
            .collect();
        let results = match session
            .create_monitored_items(subscription_id, TimestampsToReturn::Both, requests)
            .await
        {
            Ok(results) => results,
            Err(status) => {
                let _ = session.delete_subscription(subscription_id).await;
                return Err(ConnectorError::Transport(format!(
                    "create_monitored_items failed: {status}"
                )));
            }
        };
        // All-or-nothing per device: on any rejected item, drop the subscription so the runtime
        // keeps every point of this device on the polling schedule.
        let failed: Vec<String> = items
            .iter()
            .zip(&results)
            .filter(|(_, r)| !r.result.status_code.is_good())
            .map(|((id, _, _), r)| format!("{id}: {}", r.result.status_code))
            .collect();
        if !failed.is_empty() {
            let _ = session.delete_subscription(subscription_id).await;
            return Err(ConnectorError::Transport(format!(
                "monitored items rejected: {}",
                failed.join(", ")
            )));
        }

        // Forward pushed samples into the runtime sink; exit cleanly when either side closes.
        let forwarder = tokio::spawn(async move {
            while let Some(sample) = rx.recv().await {
                if sink.send(sample).await.is_err() {
                    break; // runtime dropped the sink (shutdown or reload)
                }
            }
        });
        self.subscriptions.insert(
            device.clone(),
            SubscriptionHandle {
                subscription_id,
                forwarder,
            },
        );
        Ok(())
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
        let model = self
            .devices
            .get(device)
            .and_then(|d| d.points.get(&request.point))
            .cloned()
            .ok_or_else(|| ConnectorError::UnknownPoint {
                device: device.clone(),
                point: request.point.clone(),
            })?;

        if !model.access.can_write() {
            return Err(ConnectorError::AccessDenied(request.point.clone()));
        }
        let datatype = model
            .datatype
            .ok_or_else(|| ConnectorError::Decode("write requires a point datatype".into()))?;
        let value = request
            .value
            .as_ref()
            .ok_or_else(|| ConnectorError::Decode("write requires a value".into()))?;
        let variant = build_variant(datatype, value)
            .map_err(ConnectorError::Decode)?;

        let session = self
            .sessions
            .get(device)
            .ok_or_else(|| ConnectorError::NotConnected(device.clone()))?
            .session
            .clone();

        let write = WriteValue {
            node_id: model.node_id.clone(),
            attribute_id: AttributeId::Value as u32,
            index_range: Default::default(),
            value: DataValue {
                value: Some(variant),
                ..Default::default()
            },
        };
        let results = session
            .write(&[write])
            .await
            .map_err(|s| ConnectorError::Transport(format!("write failed: {s}")))?;
        let status = results.first().copied().unwrap_or(StatusCode::Good);
        if !status.is_good() {
            return Err(ConnectorError::Transport(format!("write rejected: {status}")));
        }
        Ok(CommandResult {
            point: request.point.clone(),
            value: request.value.clone(),
            raw: request.raw.clone(),
        })
    }

    async fn disconnect(&mut self) -> Result<(), ConnectorError> {
        // Tear down push subscriptions first: abort the forwarder explicitly so no stale task
        // keeps pushing into an old sink after a config reload re-subscribes, and delete the
        // server-side subscription while the session is still alive.
        for (device, sub) in self.subscriptions.drain() {
            sub.forwarder.abort();
            if let Some(handle) = self.sessions.get(&device) {
                let _ = handle.session.delete_subscription(sub.subscription_id).await;
            }
        }
        for (_, handle) in self.sessions.drain() {
            let _ = handle.session.disconnect().await;
            handle._event_loop.abort();
        }
        Ok(())
    }
}

/// Build a `NodeId` from the configured address (textual or structured).
fn node_id_from(addr: &NodeAddress) -> Result<NodeId, String> {
    if let Some(text) = &addr.node_id {
        return NodeId::from_str(text).map_err(|_| format!("invalid node_id '{text}'"));
    }
    let ns = addr
        .namespace
        .ok_or_else(|| "missing 'node_id' or 'namespace'/'identifier'".to_string())?;
    match &addr.identifier {
        Some(serde_json::Value::String(s)) => Ok(NodeId::new(ns, s.clone())),
        Some(serde_json::Value::Number(n)) => {
            let i = n
                .as_u64()
                .ok_or_else(|| "numeric identifier must be a non-negative integer".to_string())?;
            Ok(NodeId::new(ns, i as u32))
        }
        _ => Err("missing or invalid 'identifier'".to_string()),
    }
}

/// Connect to one OPC-UA server endpoint and wait for the session to activate.
async fn connect_device(
    conn: &OpcuaConnection,
    endpoint: &OpcuaEndpoint,
) -> Result<SessionHandle, String> {
    let mut client = ClientBuilder::new()
        .application_name(conn.application_name.clone())
        .application_uri(conn.application_uri.clone())
        .trust_server_certs(true)
        .create_sample_keypair(false)
        .session_retry_limit(3)
        .client()
        .map_err(|e| format!("client build failed: {e:?}"))?;

    let policy_str = endpoint
        .security_policy
        .as_deref()
        .or(conn.security_policy.as_deref())
        .unwrap_or("None");
    let policy = SecurityPolicy::from_str(policy_str)
        .map_err(|_| format!("unknown security_policy '{policy_str}'"))?;
    let mode = parse_security_mode(
        endpoint
            .security_mode
            .as_deref()
            .or(conn.security_mode.as_deref()),
    );
    let identity = match (&endpoint.user, &endpoint.password) {
        (Some(u), Some(p)) => IdentityToken::UserName(u.clone(), p.clone().into()),
        (Some(u), None) => IdentityToken::UserName(u.clone(), String::new().into()),
        _ => IdentityToken::Anonymous,
    };

    let (session, event_loop) = client
        .connect_to_matching_endpoint(
            (
                endpoint.endpoint.as_str(),
                policy.to_str(),
                mode,
                UserTokenPolicy::anonymous(),
            ),
            identity,
        )
        .await
        .map_err(|e| format!("connect failed: {e}"))?;

    let handle = event_loop.spawn();
    let timeout = Duration::from_secs(conn.connect_timeout_s.max(1));
    match tokio::time::timeout(timeout, session.wait_for_connection()).await {
        Ok(true) => Ok(SessionHandle {
            session,
            _event_loop: handle,
        }),
        Ok(false) => {
            handle.abort();
            Err("session failed to connect".to_string())
        }
        Err(_) => {
            handle.abort();
            Err(format!("timed out after {}s waiting for connection", timeout.as_secs()))
        }
    }
}

fn parse_security_mode(mode: Option<&str>) -> MessageSecurityMode {
    match mode.map(|m| m.to_ascii_lowercase()).as_deref() {
        Some("sign") => MessageSecurityMode::Sign,
        Some("sign_and_encrypt") | Some("signandencrypt") => MessageSecurityMode::SignAndEncrypt,
        _ => MessageSecurityMode::None,
    }
}

/// Convert an OPC-UA `Variant` into the SDK value model plus a best-effort raw byte echo.
fn variant_to_value(v: &Variant) -> Option<(Value, DataType, Vec<u8>)> {
    Some(match v {
        Variant::Boolean(b) => (Value::Bool(*b), DataType::Bool, vec![*b as u8]),
        Variant::SByte(i) => (Value::Number(*i as f64), DataType::Int8, vec![*i as u8]),
        Variant::Byte(u) => (Value::Number(*u as f64), DataType::Uint8, vec![*u]),
        Variant::Int16(i) => (Value::Number(*i as f64), DataType::Int16, i.to_be_bytes().to_vec()),
        Variant::UInt16(u) => {
            (Value::Number(*u as f64), DataType::Uint16, u.to_be_bytes().to_vec())
        }
        Variant::Int32(i) => (Value::Number(*i as f64), DataType::Int32, i.to_be_bytes().to_vec()),
        Variant::UInt32(u) => {
            (Value::Number(*u as f64), DataType::Uint32, u.to_be_bytes().to_vec())
        }
        Variant::Int64(i) => (int64_value(*i), DataType::Int64, i.to_be_bytes().to_vec()),
        Variant::UInt64(u) => (uint64_value(*u), DataType::Uint64, u.to_be_bytes().to_vec()),
        Variant::Float(f) => (Value::Number(*f as f64), DataType::Float32, f.to_be_bytes().to_vec()),
        Variant::Double(d) => (Value::Number(*d), DataType::Float64, d.to_be_bytes().to_vec()),
        Variant::String(s) => {
            let t = s.as_ref().to_string();
            let raw = t.clone().into_bytes();
            (Value::Text(t), DataType::String, raw)
        }
        _ => return None,
    })
}

/// 64-bit signed: keep as a number while exactly representable, else stringify.
fn int64_value(i: i64) -> Value {
    if i.abs() <= MAX_SAFE_INT {
        Value::Number(i as f64)
    } else {
        Value::Text(i.to_string())
    }
}

/// 64-bit unsigned: keep as a number while exactly representable, else stringify.
fn uint64_value(u: u64) -> Value {
    if u <= MAX_SAFE_INT as u64 {
        Value::Number(u as f64)
    } else {
        Value::Text(u.to_string())
    }
}

/// Build an OPC-UA `Variant` for a write, coercing the JSON value to the point's datatype.
fn build_variant(dt: DataType, value: &serde_json::Value) -> Result<Variant, String> {
    let num_err = || format!("value {value} is not valid for datatype {dt:?}");
    Ok(match dt {
        DataType::Bool => Variant::Boolean(value.as_bool().ok_or_else(num_err)?),
        DataType::Int8 => Variant::SByte(value.as_i64().ok_or_else(num_err)? as i8),
        DataType::Uint8 => Variant::Byte(value.as_u64().ok_or_else(num_err)? as u8),
        DataType::Int16 => Variant::Int16(value.as_i64().ok_or_else(num_err)? as i16),
        DataType::Uint16 => Variant::UInt16(value.as_u64().ok_or_else(num_err)? as u16),
        DataType::Int32 => Variant::Int32(value.as_i64().ok_or_else(num_err)? as i32),
        DataType::Uint32 => Variant::UInt32(value.as_u64().ok_or_else(num_err)? as u32),
        DataType::Int64 => Variant::Int64(int_from_json(value).ok_or_else(num_err)?),
        DataType::Uint64 => Variant::UInt64(uint_from_json(value).ok_or_else(num_err)?),
        DataType::Float32 => Variant::Float(value.as_f64().ok_or_else(num_err)? as f32),
        DataType::Float64 => Variant::Double(value.as_f64().ok_or_else(num_err)?),
        DataType::String => Variant::String(UAString::from(
            value.as_str().ok_or_else(num_err)?.to_string(),
        )),
        other => return Err(format!("datatype {other:?} is not writable over OPC-UA")),
    })
}

/// Accept a JSON number or a numeric string for 64-bit integers (outside JS safe range).
fn int_from_json(value: &serde_json::Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_str().and_then(|s| s.parse::<i64>().ok()))
}

fn uint_from_json(value: &serde_json::Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_str().and_then(|s| s.parse::<u64>().ok()))
}

/// Sample timestamp for a `DataValue`: prefer the source timestamp, then the server timestamp,
/// then "now" (polled reads request no timestamps, so they always fall back to "now").
fn data_value_ts(dv: &DataValue) -> OffsetDateTime {
    dv.source_timestamp
        .as_ref()
        .or(dv.server_timestamp.as_ref())
        .and_then(opcua_datetime_to_ts)
        .unwrap_or_else(OffsetDateTime::now_utc)
}

/// Convert an OPC-UA `DateTime` (100 ns ticks since 1601-01-01) to an `OffsetDateTime`.
fn opcua_datetime_to_ts(dt: &opcua::types::DateTime) -> Option<OffsetDateTime> {
    if dt.is_null() {
        return None;
    }
    // Ticks between the OPC-UA epoch (1601-01-01) and the Unix epoch (1970-01-01).
    const UNIX_EPOCH_TICKS: i128 = 116_444_736_000_000_000;
    let nanos = (dt.ticks() as i128 - UNIX_EPOCH_TICKS) * 100;
    OffsetDateTime::from_unix_timestamp_nanos(nanos).ok()
}

/// Build a contract sample from an OPC-UA `DataValue`. Shared by the polling path
/// (`read_points`) and the push path (`subscribe`) so both decode identically.
fn build_sample(id: &str, model: &OpcuaPoint, dv: &DataValue) -> Sample {
    let status = dv.status.unwrap_or(StatusCode::Good);
    if !status.is_good() {
        return bad_sample(id, Some(model), &format!("bad status: {status}"));
    }
    let variant = match &dv.value {
        Some(v) => v,
        None => return bad_sample(id, Some(model), "no value returned"),
    };
    let (value, native_dt, raw) = match variant_to_value(variant) {
        Some(parts) => parts,
        None => return bad_sample(id, Some(model), "unsupported OPC-UA value type"),
    };
    let (out_value, datatype) = match model.mode {
        Mode::Raw => (None, model.datatype),
        Mode::Typed => (
            Some(model.transform.apply(value)),
            Some(model.datatype.unwrap_or(native_dt)),
        ),
    };
    Sample {
        ts: data_value_ts(dv),
        device: String::new(),
        protocol: PROTOCOL,
        point: id.to_string(),
        mode: model.mode,
        datatype,
        value: out_value,
        raw,
        raw_group: 1,
        quality: Quality::Good,
        unit: model.unit.clone(),
        addr: addr_echo(model),
        seq: None,
        error: None,
    }
}

/// Build a `bad` quality sample carrying the error reason.
fn bad_sample(id: &str, model: Option<&OpcuaPoint>, error: &str) -> Sample {
    Sample {
        ts: OffsetDateTime::now_utc(),
        device: String::new(),
        protocol: PROTOCOL,
        point: id.to_string(),
        mode: model.map(|m| m.mode).unwrap_or(Mode::Typed),
        datatype: model.and_then(|m| m.datatype),
        value: None,
        raw: Vec::new(),
        raw_group: 1,
        quality: Quality::Bad,
        unit: model.and_then(|m| m.unit.clone()),
        addr: model.map(addr_echo).unwrap_or(serde_json::Value::Null),
        seq: None,
        error: Some(error.to_string()),
    }
}

/// Echo the node id (textual form) for the sample `addr` field.
fn addr_echo(model: &OpcuaPoint) -> serde_json::Value {
    serde_json::json!({ "node_id": model.node_id.to_string() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_id_textual() {
        let addr = NodeAddress {
            node_id: Some("ns=2;s=Temperature".into()),
            namespace: None,
            identifier: None,
        };
        let nid = node_id_from(&addr).unwrap();
        assert_eq!(nid.namespace, 2);
    }

    #[test]
    fn node_id_structured_numeric() {
        let addr = NodeAddress {
            node_id: None,
            namespace: Some(3),
            identifier: Some(serde_json::json!(1001)),
        };
        let nid = node_id_from(&addr).unwrap();
        assert_eq!(nid.namespace, 3);
    }

    #[test]
    fn variant_number_roundtrip() {
        let (v, dt, raw) = variant_to_value(&Variant::UInt16(17001)).unwrap();
        assert_eq!(v, Value::Number(17001.0));
        assert_eq!(dt, DataType::Uint16);
        assert_eq!(raw, 17001u16.to_be_bytes().to_vec());
    }

    #[test]
    fn variant_bool() {
        let (v, dt, _) = variant_to_value(&Variant::Boolean(true)).unwrap();
        assert_eq!(v, Value::Bool(true));
        assert_eq!(dt, DataType::Bool);
    }

    #[test]
    fn build_variant_float() {
        let v = build_variant(DataType::Float32, &serde_json::json!(404.17)).unwrap();
        assert!(matches!(v, Variant::Float(_)));
    }

    #[test]
    fn build_variant_bool_type_mismatch() {
        assert!(build_variant(DataType::Bool, &serde_json::json!(5)).is_err());
    }

    #[test]
    fn big_uint64_is_text() {
        assert_eq!(uint64_value(u64::MAX), Value::Text(u64::MAX.to_string()));
    }

    #[test]
    fn opcua_datetime_roundtrip() {
        let dt = opcua::types::DateTime::ymd_hms(2026, 7, 1, 12, 30, 45);
        let ts = opcua_datetime_to_ts(&dt).unwrap();
        assert_eq!(ts.year(), 2026);
        assert_eq!(u8::from(ts.month()), 7);
        assert_eq!(ts.day(), 1);
        assert_eq!((ts.hour(), ts.minute(), ts.second()), (12, 30, 45));
    }

    #[test]
    fn opcua_datetime_null_is_none() {
        assert!(opcua_datetime_to_ts(&opcua::types::DateTime::null()).is_none());
    }

    #[test]
    fn data_value_ts_prefers_source_timestamp() {
        let source = opcua::types::DateTime::ymd_hms(2026, 1, 2, 3, 4, 5);
        let server = opcua::types::DateTime::ymd_hms(2026, 6, 7, 8, 9, 10);
        let dv = DataValue {
            source_timestamp: Some(source),
            server_timestamp: Some(server),
            ..Default::default()
        };
        let ts = data_value_ts(&dv);
        assert_eq!((ts.year(), u8::from(ts.month()), ts.day()), (2026, 1, 2));
    }

    #[test]
    fn data_value_ts_falls_back_to_now() {
        let before = OffsetDateTime::now_utc();
        let ts = data_value_ts(&DataValue::default());
        assert!(ts >= before);
    }
}
