//! The SDK runtime: wraps a [`Connector`] and delivers all contract behaviour (MQTT,
//! scheduling, command routing, capability descriptor, health & link status) so protocol
//! modules stay tiny.

use crate::config::{parse_duration, ConnectorConfig};
use crate::connector::{
    Access, Capabilities, CommandRequest, Connector, ConnectorError, LinkReport, LinkStatus,
    PointRef, SampleSink,
};
use crate::decode::{Endianness, WordOrder};
use crate::model::{format_rfc3339_ms, Mode, Sample};
use rumqttc::{AsyncClient, Event, LastWill, MqttOptions, Packet, QoS};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use time::OffsetDateTime;
use toml_edit::{ArrayOfTables, DocumentMut, InlineTable, Item, Table, Value as EditValue};
use tracing::{debug, error, info, warn};

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// A single scheduled read job for one point on one device.
struct ScheduleEntry {
    device_index: usize,
    point: PointRef,
    interval: Duration,
    next_due: Instant,
}

/// Run the connector under the SDK runtime until cancelled (Ctrl-C).
///
/// `config_path` is the file the typed `config` was loaded from; the runtime keeps the raw
/// document so management commands (§6.3) can patch and persist it.
pub async fn run(
    mut connector: Box<dyn Connector>,
    mut config: ConnectorConfig,
    config_path: PathBuf,
) -> Result<(), BoxError> {
    let protocol = config.connector.protocol.clone();
    let service = config.connector.service_name.clone();

    // Keep the raw configuration document so management commands can patch & persist it
    // (preserving comments/formatting via toml_edit).
    let mut config_doc: DocumentMut = std::fs::read_to_string(&config_path)
        .ok()
        .and_then(|t| t.parse::<DocumentMut>().ok())
        .unwrap_or_default();

    // 1. Configure the protocol module with the parsed config.
    connector
        .configure(&config)
        .map_err(|e| format!("configure failed: {e}"))?;
    let mut caps = connector.capabilities();
    augment_management_caps(&mut caps);

    // 2. MQTT setup.
    let health_topic = format!("te/device/main/service/{service}/status/health");
    let cap_topic = format!("te/device/main/service/{service}/ot/capabilities");
    let cmd_sub = format!("te/device/+/ot/{protocol}/cmd/+/+");

    let mut opts = MqttOptions::new(
        format!("{service}-{protocol}"),
        config.mqtt.host.clone(),
        config.mqtt.port,
    );
    opts.set_keep_alive(Duration::from_secs(30));
    let down_payload = serde_json::json!({
        "status": "down",
        "time": format_rfc3339_ms(OffsetDateTime::now_utc())
    })
    .to_string();
    opts.set_last_will(LastWill::new(
        health_topic.clone(),
        down_payload,
        QoS::AtLeastOnce,
        true,
    ));

    let (client, mut eventloop) = AsyncClient::new(opts, 32);

    // Drive the MQTT event loop from its own task, forwarding incoming publishes to the main
    // loop. The event loop MUST NOT share a select loop with publishing: while the broker is
    // unreachable the client's request queue fills, `publish().await` then blocks the shared
    // loop, the event loop stops being polled, and the connector wedges permanently — even
    // after the broker comes back. (Observed when the connector service started before the
    // broker.) A dedicated task keeps draining the queue no matter what the main loop awaits.
    let (incoming_tx, mut incoming_rx) = tokio::sync::mpsc::channel::<rumqttc::Publish>(32);
    tokio::spawn(async move {
        loop {
            match eventloop.poll().await {
                Ok(Event::Incoming(Packet::Publish(p))) => {
                    if incoming_tx.send(p).await.is_err() {
                        break; // runtime shut down
                    }
                }
                Ok(_) => {}
                Err(e) => {
                    warn!("mqtt event loop error: {e}; retrying");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }
    });

    // 3. Publish capability descriptor + service health (retained).
    publish_retained(&client, &cap_topic, caps.to_json().to_string()).await?;
    publish_health(&client, &health_topic, "up").await?;
    client.subscribe(&cmd_sub, QoS::AtLeastOnce).await?;
    info!(%protocol, %service, "connector started");

    // 4. Connect to devices and publish link status.
    match connector.connect().await {
        Ok(reports) => publish_links(&client, &protocol, &reports).await?,
        Err(e) => warn!("initial connect failed: {e}"),
    }

    // 5. Set up push delivery for subscribe-capable connectors, then build the polling
    // schedule for everything that is not pushed. The runtime keeps `sample_tx` alive for
    // the whole run so re-subscribing after a config reload reuses the same channel.
    let (sample_tx, mut sample_rx) = tokio::sync::mpsc::channel::<Sample>(256);
    let mut subscribed =
        setup_subscriptions(&mut connector, &config, caps.subscribe, &sample_tx).await;
    let mut schedule = build_schedule(&config, &subscribed);
    let mut meta_index = build_meta_index(&config);
    let mut seq_counters: HashMap<(String, String), u64> = HashMap::new();

    // 6. Main loop: poll due points on a tick, route commands from the MQTT event-loop task.
    let mut tick = tokio::time::interval(Duration::from_millis(200));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                info!("shutdown requested");
                break;
            }
            _ = tick.tick() => {
                let now = Instant::now();
                // Gather due points grouped by device.
                let mut due: HashMap<usize, Vec<PointRef>> = HashMap::new();
                for entry in schedule.iter_mut() {
                    if entry.next_due <= now {
                        due.entry(entry.device_index).or_default().push(entry.point.clone());
                        entry.next_due = now + entry.interval;
                    }
                }
                for (device_index, points) in due {
                    let device = config.devices[device_index].name.clone();
                    match connector.read_points(&device, &points).await {
                        Ok(mut samples) => {
                            for s in samples.iter_mut() {
                                // The runtime owns the device identity for polled reads:
                                // connectors routinely leave `device` empty, and the sample
                                // topic + meta lookup are keyed by the configured name.
                                s.device = device.clone();
                                publish_sample(&client, &protocol, s, &mut seq_counters, &meta_index)
                                    .await;
                            }
                        }
                        Err(e) => warn!(%device, "read_points failed: {e}"),
                    }
                }
            }
            Some(mut sample) = sample_rx.recv() => {
                publish_sample(&client, &protocol, &mut sample, &mut seq_counters, &meta_index)
                    .await;
            }
            Some(p) = incoming_rx.recv() => {
                match handle_command(
                    &mut connector, &client, &protocol,
                    &mut config, &mut config_doc, &config_path,
                    &p.topic, &p.payload,
                ).await {
                    // A management command changed the config: re-establish push
                    // delivery (the reload disconnected the old subscriptions) and
                    // rebuild the polling schedule.
                    Ok(true) => {
                        subscribed = setup_subscriptions(
                            &mut connector, &config, caps.subscribe, &sample_tx,
                        ).await;
                        schedule = build_schedule(&config, &subscribed);
                        meta_index = build_meta_index(&config);
                        seq_counters.clear();
                    }
                    Ok(false) => {}
                    Err(e) => warn!("command handling error: {e}"),
                }
            }
        }
    }

    // 7. Clean shutdown.
    let _ = connector.disconnect().await;
    publish_health(&client, &health_topic, "down").await.ok();
    Ok(())
}

/// Build the polling schedule, skipping points that are delivered by subscription.
fn build_schedule(
    config: &ConnectorConfig,
    subscribed: &HashSet<(usize, String)>,
) -> Vec<ScheduleEntry> {
    let connector_default = parse_duration(&config.connector.poll_interval)
        .unwrap_or_else(|| Duration::from_secs(2));
    let now = Instant::now();
    let mut schedule = Vec::new();
    for (device_index, device) in config.devices.iter().enumerate() {
        let device_default = device
            .poll_interval
            .as_deref()
            .and_then(parse_duration)
            .unwrap_or(connector_default);
        for point in &device.points {
            if subscribed.contains(&(device_index, point.id.clone())) {
                continue;
            }
            let interval = point
                .poll_interval
                .as_deref()
                .and_then(parse_duration)
                .unwrap_or(device_default);
            let mut point = point_ref(point, device.default_mode);
            point.interval = Some(interval);
            schedule.push(ScheduleEntry {
                device_index,
                point,
                interval,
                next_due: now,
            });
        }
    }
    schedule
}

/// Ask a subscribe-capable connector for push delivery, device by device. Points configured
/// with `subscribe = false` are excluded and stay on the polling schedule, as does every point
/// of a device whose `subscribe()` call does not succeed. Returns the set of
/// `(device_index, point_id)` now delivered via push.
async fn setup_subscriptions(
    connector: &mut Box<dyn Connector>,
    config: &ConnectorConfig,
    subscribe_capable: bool,
    sink: &SampleSink,
) -> HashSet<(usize, String)> {
    let mut subscribed = HashSet::new();
    if !subscribe_capable {
        return subscribed;
    }
    let connector_default = parse_duration(&config.connector.poll_interval)
        .unwrap_or_else(|| Duration::from_secs(2));
    for (device_index, device) in config.devices.iter().enumerate() {
        let device_default = device
            .poll_interval
            .as_deref()
            .and_then(parse_duration)
            .unwrap_or(connector_default);
        let points: Vec<PointRef> = device
            .points
            .iter()
            .filter(|p| p.subscribe.unwrap_or(true))
            .map(|p| {
                let mut r = point_ref(p, device.default_mode);
                r.interval = Some(
                    p.poll_interval
                        .as_deref()
                        .and_then(parse_duration)
                        .unwrap_or(device_default),
                );
                r
            })
            .collect();
        if points.is_empty() {
            continue;
        }
        match connector.subscribe(&device.name, &points, sink.clone()).await {
            Ok(()) => {
                info!(device = %device.name, points = points.len(), "subscribed (push delivery)");
                for p in &points {
                    subscribed.insert((device_index, p.id.clone()));
                }
            }
            Err(ConnectorError::Unsupported(_)) => {
                debug!(device = %device.name, "subscribe unsupported; polling");
            }
            Err(e) => {
                warn!(device = %device.name, "subscribe failed: {e}; falling back to polling");
            }
        }
    }
    subscribed
}

/// Per-point `meta` lookup, keyed by `(device name, point id)`; injected into every published
/// sample envelope so flows can apply per-signal behaviour without their own config.
fn build_meta_index(config: &ConnectorConfig) -> HashMap<(String, String), serde_json::Value> {
    let mut index = HashMap::new();
    for device in &config.devices {
        for point in &device.points {
            if let Some(meta) = &point.meta {
                index.insert((device.name.clone(), point.id.clone()), meta.clone());
            }
        }
    }
    index
}

/// The sample envelope as published: the contract envelope plus the point's `meta`, if any.
fn envelope_with_meta(
    sample: &Sample,
    meta_index: &HashMap<(String, String), serde_json::Value>,
) -> serde_json::Value {
    let mut envelope = sample.to_envelope();
    if let Some(meta) = meta_index.get(&(sample.device.clone(), sample.point.clone())) {
        envelope["meta"] = meta.clone();
    }
    envelope
}

/// Stamp the per-point sequence number and publish one sample. Shared by the polling loop and
/// the subscription channel so both paths get identical seq/meta/topic handling.
async fn publish_sample(
    client: &AsyncClient,
    protocol: &str,
    sample: &mut Sample,
    seq_counters: &mut HashMap<(String, String), u64>,
    meta_index: &HashMap<(String, String), serde_json::Value>,
) {
    let counter = seq_counters
        .entry((sample.device.clone(), sample.point.clone()))
        .or_insert(0);
    *counter += 1;
    sample.seq = Some(*counter);
    let topic = format!(
        "te/device/{}/ot/{}/sample/{}",
        sample.device, protocol, sample.point
    );
    let payload = envelope_with_meta(sample, meta_index).to_string();
    if let Err(e) = client.publish(&topic, QoS::AtMostOnce, false, payload).await {
        error!("failed to publish sample: {e}");
    }
}

/// Build a resolved [`PointRef`] from a configured point. Shared by the scheduler and by callers
/// (e.g. a CLI) that drive a connector's `read_points`/`execute` directly.
pub fn point_ref(point: &crate::config::PointConfig, device_default: Option<Mode>) -> PointRef {
    PointRef {
        id: point.id.clone(),
        mode: point.resolved_mode(device_default),
        datatype: point.datatype,
        endianness: Endianness::parse(point.endianness.as_deref()),
        word_order: WordOrder::parse(point.word_order.as_deref()),
        access: Access::parse(point.access.as_deref()),
        unit: point.unit.clone(),
        transform: point.transform.unwrap_or_default(),
        interval: point.poll_interval.as_deref().and_then(parse_duration),
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_command(
    connector: &mut Box<dyn Connector>,
    client: &AsyncClient,
    protocol: &str,
    config: &mut ConnectorConfig,
    config_doc: &mut DocumentMut,
    config_path: &Path,
    topic: &str,
    payload: &[u8],
) -> Result<bool, BoxError> {    // Expect te/device/<device>/ot/<protocol>/cmd/<verb>/<id>
    let parts: Vec<&str> = topic.split('/').collect();
    if parts.len() != 8
        || parts[0] != "te"
        || parts[1] != "device"
        || parts[3] != "ot"
        || parts[4] != protocol
        || parts[5] != "cmd"
    {
        return Ok(false);
    }
    let device = parts[2].to_string();
    let verb = parts[6];

    let json: serde_json::Value = match serde_json::from_slice(payload) {
        Ok(v) => v,
        Err(_) => return Ok(false), // empty/clearing message or junk
    };
    let status = json.get("status").and_then(|s| s.as_str()).unwrap_or("");
    if status != "init" {
        return Ok(false); // only act on new requests; ignore our own transitions
    }

    // Management verbs (§6.3) are handled generically by the runtime; they mutate and persist
    // the connector configuration, then live-reload the protocol module.
    if is_management_verb(verb) {
        return handle_management(
            connector, client, protocol, config, config_doc, config_path, topic, verb, &json,
        )
        .await;
    }

    let point = json
        .get("point")
        .and_then(|p| p.as_str())
        .unwrap_or_default()
        .to_string();
    let request = CommandRequest {
        point: point.clone(),
        value: json.get("value").cloned(),
        value_repr: json
            .get("value_repr")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        raw: json.get("raw").and_then(|v| v.as_str()).map(|s| s.to_string()),
    };

    // executing
    publish_retained(
        client,
        topic,
        serde_json::json!({ "status": "executing", "point": point }).to_string(),
    )
    .await?;

    match connector.execute(&device, verb, &request).await {
        Ok(result) => {
            let mut obj = serde_json::Map::new();
            obj.insert("status".into(), serde_json::Value::String("successful".into()));
            obj.insert("point".into(), serde_json::Value::String(result.point));
            if let Some(v) = result.value {
                obj.insert("value".into(), v);
            }
            if let Some(r) = result.raw {
                obj.insert("raw".into(), serde_json::Value::String(r));
            }
            publish_retained(client, topic, serde_json::Value::Object(obj).to_string()).await?;
        }
        Err(e) => {
            publish_retained(
                client,
                topic,
                serde_json::json!({
                    "status": "failed",
                    "point": point,
                    "reason": e.to_string()
                })
                .to_string(),
            )
            .await?;
        }
    }
    debug!(%device, %verb, "command handled");
    Ok(false)
}

/// The protocol-neutral management verbs the SDK runtime implements for every connector.
fn is_management_verb(verb: &str) -> bool {
    matches!(verb, "set-config" | "define-device" | "remove-device")
}

/// Advertise the SDK-provided management verbs in the connector's capability descriptor.
fn augment_management_caps(caps: &mut Capabilities) {
    for verb in ["set-config", "define-device", "remove-device"] {
        if !caps.command_verbs.iter().any(|v| v == verb) {
            caps.command_verbs.push(verb.to_string());
        }
    }
    if !caps.features.iter().any(|f| f == "management") {
        caps.features.push("management".to_string());
    }
}

/// Handle a management command: patch the config document, validate, persist, and live-reload.
/// Returns `Ok(true)` when the configuration changed (so the caller rebuilds the schedule).
#[allow(clippy::too_many_arguments)]
async fn handle_management(
    connector: &mut Box<dyn Connector>,
    client: &AsyncClient,
    protocol: &str,
    config: &mut ConnectorConfig,
    config_doc: &mut DocumentMut,
    config_path: &Path,
    topic: &str,
    verb: &str,
    json: &serde_json::Value,
) -> Result<bool, BoxError> {
    publish_retained(
        client,
        topic,
        serde_json::json!({ "status": "executing" }).to_string(),
    )
    .await?;

    // Build a candidate document and validate it parses into a typed config.
    let candidate = {
        let mut doc = config_doc.clone();
        match apply_management(verb, json, &mut doc) {
            Ok(()) => doc,
            Err(e) => {
                publish_failed(client, topic, &e).await?;
                return Ok(false);
            }
        }
    };
    let new_config: ConnectorConfig = match toml::from_str(&candidate.to_string()) {
        Ok(c) => c,
        Err(e) => {
            publish_failed(client, topic, &format!("resulting config is invalid: {e}")).await?;
            return Ok(false);
        }
    };

    // Validate against the protocol module before committing.
    if let Err(e) = connector.configure(&new_config) {
        let _ = connector.configure(config); // restore previous good state
        publish_failed(client, topic, &format!("configure failed: {e}")).await?;
        return Ok(false);
    }

    // Persist the new document (best effort: the running state is already updated).
    if let Err(e) = persist_config(config_path, &candidate) {
        warn!("failed to persist config to {}: {e}", config_path.display());
    }
    *config_doc = candidate;
    *config = new_config;

    // Reconnect with the new configuration and republish link status.
    let _ = connector.disconnect().await;
    match connector.connect().await {
        Ok(reports) => publish_links(client, protocol, &reports).await?,
        Err(e) => warn!("reconnect after reconfigure failed: {e}"),
    }

    publish_retained(
        client,
        topic,
        serde_json::json!({ "status": "successful" }).to_string(),
    )
    .await?;
    info!(%verb, "management command applied");
    Ok(true)
}

async fn publish_failed(client: &AsyncClient, topic: &str, reason: &str) -> Result<(), BoxError> {
    warn!("management command failed: {reason}");
    publish_retained(
        client,
        topic,
        serde_json::json!({ "status": "failed", "reason": reason }).to_string(),
    )
    .await
}

/// Dispatch a management verb onto the configuration document.
fn apply_management(
    verb: &str,
    json: &serde_json::Value,
    doc: &mut DocumentMut,
) -> Result<(), String> {
    match verb {
        "set-config" => apply_set_config(json, doc),
        "define-device" => apply_define_device(json, doc),
        "remove-device" => apply_remove_device(json, doc),
        other => Err(format!("unsupported management verb '{other}'")),
    }
}

/// `set-config`: deep-merge `config` into the section named by `target`.
fn apply_set_config(json: &serde_json::Value, doc: &mut DocumentMut) -> Result<(), String> {
    let target = json
        .get("target")
        .and_then(|t| t.as_str())
        .ok_or("set-config requires a 'target'")?;
    let patch = json
        .get("config")
        .and_then(|c| c.as_object())
        .ok_or("set-config requires a 'config' object")?;
    let root = doc.as_table_mut();

    if let Some(name) = target.strip_prefix("device:") {
        let devices = root
            .get_mut("device")
            .and_then(Item::as_array_of_tables_mut)
            .ok_or("no devices are configured")?;
        let table = (0..devices.len())
            .find(|&i| {
                devices
                    .get(i)
                    .and_then(|t| t.get("name"))
                    .and_then(|v| v.as_str())
                    == Some(name)
            })
            .and_then(|i| devices.get_mut(i))
            .ok_or_else(|| format!("device '{name}' not found"))?;
        merge_object_into_table(table, patch)
    } else if matches!(target, "connector" | "mqtt" | "connection") {
        let item = root
            .entry(target)
            .or_insert_with(|| Item::Table(Table::new()));
        let table = item
            .as_table_mut()
            .ok_or_else(|| format!("config section '{target}' is not a table"))?;
        merge_object_into_table(table, patch)
    } else {
        Err(format!(
            "unknown set-config target '{target}' (expected connector, mqtt, connection or device:<name>)"
        ))
    }
}

/// `define-device`: insert or replace a `[[device]]` entry by name.
fn apply_define_device(json: &serde_json::Value, doc: &mut DocumentMut) -> Result<(), String> {
    let device = json
        .get("device")
        .and_then(|d| d.as_object())
        .ok_or("define-device requires a 'device' object")?;
    let name = device
        .get("name")
        .and_then(|n| n.as_str())
        .ok_or("device requires a 'name'")?
        .to_string();
    let new_table = json_object_to_table(device)?;

    let devices = doc
        .as_table_mut()
        .entry("device")
        .or_insert_with(|| Item::ArrayOfTables(ArrayOfTables::new()))
        .as_array_of_tables_mut()
        .ok_or("'device' is not an array of tables")?;

    let existing = (0..devices.len()).find(|&i| {
        devices
            .get(i)
            .and_then(|t| t.get("name"))
            .and_then(|v| v.as_str())
            == Some(name.as_str())
    });
    match existing.and_then(|i| devices.get_mut(i)) {
        Some(slot) => *slot = new_table,
        None => devices.push(new_table),
    }
    Ok(())
}

/// `remove-device`: delete the named `[[device]]` entry.
fn apply_remove_device(json: &serde_json::Value, doc: &mut DocumentMut) -> Result<(), String> {
    let name = json
        .get("device")
        .and_then(|d| d.as_str())
        .ok_or("remove-device requires a 'device' name string")?;
    let devices = doc
        .as_table_mut()
        .get_mut("device")
        .and_then(Item::as_array_of_tables_mut)
        .ok_or("no devices are configured")?;
    let index = (0..devices.len()).find(|&i| {
        devices
            .get(i)
            .and_then(|t| t.get("name"))
            .and_then(|v| v.as_str())
            == Some(name)
    });
    match index {
        Some(i) => {
            devices.remove(i);
            Ok(())
        }
        None => Err(format!("device '{name}' not found")),
    }
}

/// Deep-merge a JSON object into a toml_edit table: nested objects merge into existing standard
/// sub-tables, otherwise (absent / inline / scalar) the key is replaced.
fn merge_object_into_table(
    table: &mut Table,
    patch: &serde_json::Map<String, serde_json::Value>,
) -> Result<(), String> {
    for (key, value) in patch {
        match value {
            serde_json::Value::Object(obj) => match table.get_mut(key) {
                Some(item) if item.is_table() => {
                    merge_object_into_table(item.as_table_mut().unwrap(), obj)?;
                }
                _ => {
                    table.insert(
                        key,
                        Item::Value(EditValue::InlineTable(json_object_to_inline(obj)?)),
                    );
                }
            },
            _ => {
                table.insert(key, Item::Value(json_value_to_edit(value)?));
            }
        }
    }
    Ok(())
}

/// Convert a JSON object into a standard toml_edit table; nested object arrays become
/// arrays-of-tables (e.g. `point`), nested objects become inline tables (e.g. `protocol_address`).
fn json_object_to_table(
    obj: &serde_json::Map<String, serde_json::Value>,
) -> Result<Table, String> {
    let mut table = Table::new();
    for (key, value) in obj {
        match value {
            serde_json::Value::Array(items)
                if !items.is_empty() && items.iter().all(serde_json::Value::is_object) =>
            {
                let mut aot = ArrayOfTables::new();
                for item in items {
                    aot.push(json_object_to_table(item.as_object().unwrap())?);
                }
                table.insert(key, Item::ArrayOfTables(aot));
            }
            _ => {
                table.insert(key, Item::Value(json_value_to_edit(value)?));
            }
        }
    }
    Ok(table)
}

fn json_object_to_inline(
    obj: &serde_json::Map<String, serde_json::Value>,
) -> Result<InlineTable, String> {
    let mut inline = InlineTable::new();
    for (key, value) in obj {
        inline.insert(key, json_value_to_edit(value)?);
    }
    Ok(inline)
}

fn json_value_to_edit(value: &serde_json::Value) -> Result<EditValue, String> {
    Ok(match value {
        serde_json::Value::Null => return Err("null values are not allowed in config".into()),
        serde_json::Value::Bool(b) => EditValue::from(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                EditValue::from(i)
            } else if let Some(f) = n.as_f64() {
                EditValue::from(f)
            } else {
                return Err(format!("unsupported number: {n}"));
            }
        }
        serde_json::Value::String(s) => EditValue::from(s.as_str()),
        serde_json::Value::Array(items) => {
            let mut array = toml_edit::Array::new();
            for item in items {
                array.push(json_value_to_edit(item)?);
            }
            EditValue::Array(array)
        }
        serde_json::Value::Object(obj) => EditValue::InlineTable(json_object_to_inline(obj)?),
    })
}

fn persist_config(path: &Path, doc: &DocumentMut) -> Result<(), String> {
    let text = doc.to_string();
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, text.as_bytes()).map_err(|e| format!("write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("rename to {}: {e}", path.display()))?;
    Ok(())
}

async fn publish_links(
    client: &AsyncClient,
    protocol: &str,
    reports: &[LinkReport],
) -> Result<(), BoxError> {
    for report in reports {
        let topic = format!("te/device/{}/ot/{}/status/link", report.device, protocol);
        let mut obj = serde_json::Map::new();
        obj.insert(
            "status".into(),
            serde_json::Value::String(report.status.as_str().into()),
        );
        if report.status == LinkStatus::Connected {
            obj.insert(
                "since".into(),
                serde_json::Value::String(format_rfc3339_ms(OffsetDateTime::now_utc())),
            );
        }
        if let Some(reason) = &report.reason {
            obj.insert("reason".into(), serde_json::Value::String(reason.clone()));
        }
        if let Some(info) = &report.info {
            obj.insert("info".into(), info.clone());
        }
        publish_retained(client, &topic, serde_json::Value::Object(obj).to_string()).await?;
    }
    Ok(())
}

async fn publish_health(client: &AsyncClient, topic: &str, status: &str) -> Result<(), BoxError> {
    let payload = serde_json::json!({
        "status": status,
        "time": format_rfc3339_ms(OffsetDateTime::now_utc())
    })
    .to_string();
    publish_retained(client, topic, payload).await
}

async fn publish_retained(
    client: &AsyncClient,
    topic: &str,
    payload: String,
) -> Result<(), BoxError> {
    client
        .publish(topic, QoS::AtLeastOnce, true, payload)
        .await
        .map_err(|e| Box::new(e) as BoxError)
}

/// Resolve the effective output mode of a point ignoring device default; small helper used by
/// modules that want the same logic without the SDK config types.
pub fn resolve_mode(mode: Option<Mode>, device_default: Option<Mode>) -> Mode {
    mode.or(device_default).unwrap_or(Mode::Typed)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = r#"
[connector]
protocol = "modbus"
poll_interval = "2s"
log_level = "info"

[mqtt]
host = "127.0.0.1"
port = 1883

[connection.serial]
baudrate = 9600
parity = "N"
stopbits = 2
databits = 8

[[device]]
name = "plc-1"
protocol_address = { transport = "tcp", host = "127.0.0.1", port = 502, unit_id = 1 }
default_mode = "typed"

  [[device.point]]
  id = "temp"
  datatype = "float32"
  address = { table = "holding", address = 7, count = 2 }
"#;

    fn doc() -> DocumentMut {
        BASE.parse::<DocumentMut>().unwrap()
    }

    /// Apply a verb and return the resulting typed config (asserting it stays valid).
    fn apply(verb: &str, json: serde_json::Value) -> (DocumentMut, ConnectorConfig) {
        let mut d = doc();
        apply_management(verb, &json, &mut d).expect("apply ok");
        let cfg: ConnectorConfig = toml::from_str(&d.to_string()).expect("valid config");
        (d, cfg)
    }

    #[test]
    fn set_config_patches_connector_section() {
        let (_d, cfg) = apply(
            "set-config",
            serde_json::json!({ "target": "connector", "config": { "poll_interval": "5s" } }),
        );
        assert_eq!(cfg.connector.poll_interval, "5s");
        // unrelated fields preserved
        assert_eq!(cfg.connector.log_level, "info");
    }

    #[test]
    fn set_config_deep_merges_serial_defaults() {
        let (d, _cfg) = apply(
            "set-config",
            serde_json::json!({ "target": "connection", "config": { "serial": { "baudrate": 19200 } } }),
        );
        let text = d.to_string();
        assert!(text.contains("baudrate = 19200"), "baudrate patched: {text}");
        // sibling serial keys are retained by the deep merge
        assert!(text.contains("parity"), "parity retained: {text}");
    }

    #[test]
    fn set_config_patches_named_device() {
        let (_d, cfg) = apply(
            "set-config",
            serde_json::json!({ "target": "device:plc-1", "config": { "poll_interval": "10s" } }),
        );
        let dev = cfg.devices.iter().find(|d| d.name == "plc-1").unwrap();
        assert_eq!(dev.poll_interval.as_deref(), Some("10s"));
        // existing points untouched
        assert_eq!(dev.points.len(), 1);
    }

    #[test]
    fn set_config_unknown_target_rejected() {
        let mut d = doc();
        let err = apply_management(
            "set-config",
            &serde_json::json!({ "target": "bogus", "config": {} }),
            &mut d,
        )
        .unwrap_err();
        assert!(err.contains("unknown set-config target"), "{err}");
    }

    #[test]
    fn define_device_appends_new_device() {
        let (_d, cfg) = apply(
            "define-device",
            serde_json::json!({ "device": {
                "name": "plc-9",
                "protocol_address": { "transport": "tcp", "host": "10.0.0.9", "port": 502, "unit_id": 2 },
                "default_mode": "typed",
                "point": [
                    { "id": "level", "datatype": "uint16", "address": { "table": "holding", "address": 1, "count": 1 } }
                ]
            }}),
        );
        assert_eq!(cfg.devices.len(), 2);
        let dev = cfg.devices.iter().find(|d| d.name == "plc-9").unwrap();
        assert_eq!(dev.points.len(), 1);
        assert_eq!(dev.points[0].id, "level");
    }

    #[test]
    fn define_device_replaces_existing_by_name() {
        let (_d, cfg) = apply(
            "define-device",
            serde_json::json!({ "device": {
                "name": "plc-1",
                "protocol_address": { "transport": "tcp", "host": "1.2.3.4", "port": 502, "unit_id": 1 },
                "point": [
                    { "id": "a", "datatype": "int16", "address": { "table": "holding", "address": 0, "count": 1 } },
                    { "id": "b", "datatype": "int16", "address": { "table": "holding", "address": 1, "count": 1 } }
                ]
            }}),
        );
        assert_eq!(cfg.devices.len(), 1, "replaced, not appended");
        assert_eq!(cfg.devices[0].points.len(), 2);
    }

    #[test]
    fn remove_device_deletes_entry() {
        let (_d, cfg) = apply("remove-device", serde_json::json!({ "device": "plc-1" }));
        assert!(cfg.devices.is_empty());
    }

    #[test]
    fn remove_unknown_device_rejected() {
        let mut d = doc();
        let err =
            apply_management("remove-device", &serde_json::json!({ "device": "nope" }), &mut d)
                .unwrap_err();
        assert!(err.contains("not found"), "{err}");
    }

    #[test]
    fn point_meta_parsed_and_indexed() {
        let cfg: ConnectorConfig = toml::from_str(
            r#"
[connector]
protocol = "modbus"

[[device]]
name = "plc-1"
protocol_address = { host = "127.0.0.1" }

  [[device.point]]
  id = "temp"
  datatype = "float32"
  address = { table = "holding", address = 7, count = 2 }
  meta = { on_change = true, min_interval = "5s", room = "boiler" }
"#,
        )
        .unwrap();
        let index = build_meta_index(&cfg);
        let meta = index.get(&("plc-1".to_string(), "temp".to_string())).unwrap();
        assert_eq!(meta["on_change"], serde_json::json!(true));
        assert_eq!(meta["min_interval"], serde_json::json!("5s"));
        assert_eq!(meta["room"], serde_json::json!("boiler"));
    }

    #[test]
    fn envelope_carries_point_meta() {
        let sample = Sample {
            ts: OffsetDateTime::UNIX_EPOCH,
            device: "plc-1".into(),
            protocol: "modbus",
            point: "temp".into(),
            mode: Mode::Typed,
            datatype: None,
            value: None,
            raw: vec![0x12, 0x34],
            raw_group: 2,
            quality: crate::model::Quality::Good,
            unit: None,
            addr: serde_json::Value::Null,
            seq: None,
            error: None,
        };
        let mut index = HashMap::new();
        index.insert(
            ("plc-1".to_string(), "temp".to_string()),
            serde_json::json!({ "on_change": true }),
        );
        let env = envelope_with_meta(&sample, &index);
        assert_eq!(env["meta"]["on_change"], serde_json::json!(true));
        // a sample without indexed meta has no meta key
        let env2 = envelope_with_meta(&sample, &HashMap::new());
        assert!(env2.get("meta").is_none());
    }

    #[test]
    fn schedule_skips_subscribed_points() {
        let cfg: ConnectorConfig = toml::from_str(BASE).unwrap();
        let none = HashSet::new();
        assert_eq!(build_schedule(&cfg, &none).len(), 1);
        let mut subscribed = HashSet::new();
        subscribed.insert((0usize, "temp".to_string()));
        assert_eq!(build_schedule(&cfg, &subscribed).len(), 0);
    }

    #[test]
    fn schedule_resolves_point_interval() {
        let cfg: ConnectorConfig = toml::from_str(BASE).unwrap();
        let schedule = build_schedule(&cfg, &HashSet::new());
        // connector poll_interval = "2s" flows into the resolved PointRef interval
        assert_eq!(schedule[0].point.interval, Some(Duration::from_secs(2)));
    }

    #[test]
    fn management_caps_are_advertised() {
        let mut caps = Capabilities {
            protocol: "modbus",
            version: "0.0.0",
            modes: vec![],
            datatypes: vec![],
            point_kinds: vec![],
            command_verbs: vec!["write".into()],
            features: vec!["polling".into()],
            subscribe: false,
        };
        augment_management_caps(&mut caps);
        for verb in ["write", "set-config", "define-device", "remove-device"] {
            assert!(caps.command_verbs.iter().any(|v| v == verb), "missing {verb}");
        }
        assert!(caps.features.iter().any(|f| f == "management"));
    }
}
