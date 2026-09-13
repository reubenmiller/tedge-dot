//! The SDK runtime: wraps a [`Connector`] and delivers all contract behaviour (MQTT,
//! scheduling, command routing, capability descriptor, health & link status) so protocol
//! modules stay tiny.

use crate::config::{parse_duration, ConnectorConfig};
use crate::connector::{
    Access, Capabilities, CommandRequest, Connector, ConnectorError, LinkReport, LinkStatus,
    PointRef, SampleSink,
};
use crate::decode::{Endianness, WordOrder};
use crate::model::{format_rfc3339_ms, Sample};
use rumqttc::{AsyncClient, Event, LastWill, MqttOptions, Packet, QoS};
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use time::OffsetDateTime;
use toml_edit::{ArrayOfTables, DocumentMut, InlineTable, Item, Table, Value as EditValue};
use tracing::{debug, error, info, warn};

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Liveness marker for a connector's loop, shared with whoever supervises it.
///
/// The loop stamps it on every iteration; a supervisor that sees it go stale knows the loop is
/// wedged (a protocol call that never returns) and can cancel and restart the connector — the
/// loop itself cannot do that, since the hang is *inside* it. Monotonic: it measures elapsed
/// time from a shared start instant, so a wall-clock change cannot make a live loop look stuck.
#[derive(Clone, Debug)]
pub struct Progress(Arc<(Instant, AtomicU64)>);

impl Progress {
    pub fn new() -> Self {
        let progress = Progress(Arc::new((Instant::now(), AtomicU64::new(0))));
        progress.mark();
        progress
    }

    /// Record that the loop just made progress.
    pub fn mark(&self) {
        let elapsed = self.0 .0.elapsed().as_millis() as u64;
        self.0 .1.store(elapsed, Ordering::Relaxed);
    }

    /// How long since the last `mark()`.
    pub fn idle(&self) -> Duration {
        let now = self.0 .0.elapsed().as_millis() as u64;
        Duration::from_millis(now.saturating_sub(self.0 .1.load(Ordering::Relaxed)))
    }
}

impl Default for Progress {
    fn default() -> Self {
        Progress::new()
    }
}

/// Bounds the runtime puts on the protocol module. Carried to the few helpers that call it.
#[derive(Clone, Copy, Debug)]
struct Limits {
    /// Upper bound on one protocol-module call (`ConnectorSection::operation_timeout`).
    operation: Duration,
}

impl Limits {
    fn from_config(config: &ConnectorConfig) -> Self {
        let operation = parse_duration(&config.connector.operation_timeout)
            .filter(|d| !d.is_zero())
            .unwrap_or_else(|| {
                warn!(
                    "invalid connector.operation_timeout '{}'; using 30s",
                    config.connector.operation_timeout
                );
                Duration::from_secs(30)
            });
        Limits { operation }
    }
}

/// Run one protocol-module call under the runtime's operation bound.
///
/// A module that hangs instead of failing (a half-open socket answers nothing and never resets)
/// would otherwise block the connector's whole loop: no samples, no health, no link status, and
/// nothing logged, because every one of those is published from that loop. Turning the hang into
/// a transport error lets the existing degraded-link and reconnect-with-backoff handling run.
async fn bounded<T>(
    limits: Limits,
    what: &str,
    call: impl Future<Output = Result<T, ConnectorError>>,
) -> Result<T, ConnectorError> {
    match tokio::time::timeout(limits.operation, call).await {
        Ok(result) => result,
        Err(_) => Err(ConnectorError::Transport(format!(
            "{what} did not return within {}s (operation_timeout)",
            limits.operation.as_secs()
        ))),
    }
}

/// Tracks the last published link status per device so the runtime can publish the
/// contract-required transitions: `degraded` when a whole poll batch fails (e.g. the device
/// dropped mid-run), back to `connected` when reads recover. A device whose initial connect
/// failed stays `disconnected` — failing reads add no information there.
struct LinkTracker {
    protocol: String,
    /// The thin-edge command types every device of this connector answers (§6.6): the
    /// manifest advertises them and the retained capability markers are published for them.
    commands: Vec<String>,
    states: HashMap<String, LinkStatus>,
    /// Last device descriptor seen per device, re-attached to transition reports so the
    /// retained link message keeps carrying it.
    infos: HashMap<String, serde_json::Value>,
}

impl LinkTracker {
    fn new(protocol: &str, commands: Vec<String>) -> Self {
        LinkTracker {
            protocol: protocol.to_string(),
            commands,
            states: HashMap::new(),
            infos: HashMap::new(),
        }
    }

    /// Publish connector-produced link reports (from `connect`) and record their status.
    ///
    /// `config` is the *live* configuration: the device `type` echoed on the status is read
    /// from it at publish time rather than cached, because a management command (§6.3)
    /// republishes the link status as part of applying a reload — a cached map would still
    /// hold the pre-reload types there, and the retained status of a device that a
    /// `define-device` just added would carry no type at all.
    async fn publish_reports(
        &mut self,
        client: &Mqtt,
        reports: &[LinkReport],
        config: &ConnectorConfig,
    ) -> Result<(), BoxError> {
        for report in reports {
            self.states.insert(report.device.clone(), report.status);
            if let Some(info) = &report.info {
                if self.infos.get(&report.device) != Some(info) {
                    self.infos.insert(report.device.clone(), info.clone());
                    // The descriptor lives on the manifest (§8.2), which is republished — before
                    // the link status — the first time a device's `connect` reports it.
                    if let Some(device) = config.devices.iter().find(|d| d.name == report.device) {
                        let payload =
                            crate::manifest::device_manifest(config, device, Some(info), &self.commands);
                        let topic = crate::manifest::topic(&device.name, &self.protocol);
                        publish_retained(client, &topic, payload.to_string()).await?;
                    }
                }
            }
        }
        publish_links(client, &self.protocol, reports, config).await
    }

    /// Publish the retained manifest (§8.2) of every configured device, carrying the device
    /// descriptor already learned from its `connect` report when there is one.
    async fn publish_manifests(
        &self,
        client: &Mqtt,
        config: &ConnectorConfig,
    ) -> Result<(), BoxError> {
        for device in &config.devices {
            let payload = crate::manifest::device_manifest(
                config,
                device,
                self.infos.get(&device.name),
                &self.commands,
            );
            let topic = crate::manifest::topic(&device.name, &self.protocol);
            publish_retained(client, &topic, payload.to_string()).await?;
            // The retained capability markers thin-edge expects for a command type (§6.6).
            // Published with the manifest so a device is announced and answerable in one step.
            for command in &self.commands {
                let marker = crate::commands::capability_topic(&device.name, command);
                publish_retained(client, &marker, "{}".to_string()).await?;
            }
        }
        Ok(())
    }

    /// Clear the retained manifest and link status of devices the configuration no longer
    /// defines — removed, or switched off — so nothing retained describes a device that is gone
    /// (§8.2; the 0.1 contract left the link status behind).
    async fn clear_devices(&mut self, client: &Mqtt, devices: &[String]) -> Result<(), BoxError> {
        for device in devices {
            self.states.remove(device);
            self.infos.remove(device);
            // The capability markers go with the manifest: a marker for a device that is gone
            // would leave a mapper routing operations nothing will ever answer.
            for command in &self.commands {
                let marker = crate::commands::capability_topic(device, command);
                publish_retained(client, &marker, String::new()).await?;
            }
            publish_retained(client, &crate::manifest::topic(device, &self.protocol), String::new())
                .await?;
            let link = format!("te/device/{device}/ot/{}/status/link", self.protocol);
            publish_retained(client, &link, String::new()).await?;
        }
        Ok(())
    }

    /// Record a device descriptor without publishing, so a later transition publish carries
    /// it (used when a reconnect succeeded but the link waits for reads to confirm).
    fn stash_info(&mut self, device: &str, info: Option<serde_json::Value>) {
        if let Some(info) = info {
            self.infos.insert(device.to_string(), info);
        }
    }

    /// Publish a link report only when it changes the recorded status — reconnect attempts
    /// repeat on a backoff schedule and must not re-publish the same retained status.
    async fn publish_if_changed(
        &mut self,
        client: &Mqtt,
        report: &LinkReport,
        config: &ConnectorConfig,
    ) {
        if self.states.get(&report.device) == Some(&report.status) {
            return;
        }
        if let Err(e) = self
            .publish_reports(client, std::slice::from_ref(report), config)
            .await
        {
            warn!(device = %report.device, "failed to publish link transition: {e}");
        }
    }

    /// Record the outcome of one poll batch for `device` (`healthy` = at least one point was
    /// readable) and publish a retained link transition when the status changed.
    async fn note_poll(
        &mut self,
        client: &Mqtt,
        device: &str,
        healthy: bool,
        reason: Option<String>,
        config: &ConnectorConfig,
    ) {
        let current = self.states.get(device).copied();
        let Some(new) = next_link_state(current, healthy) else {
            return;
        };
        info!(%device, status = new.as_str(), "link status changed");
        let report = LinkReport {
            device: device.to_string(),
            status: new,
            reason,
            info: self.infos.get(device).cloned(),
        };
        if let Err(e) = self
            .publish_reports(client, std::slice::from_ref(&report), config)
            .await
        {
            warn!(%device, "failed to publish link transition: {e}");
        }
    }

    /// Publish every recorded status again, unchanged, for a broker that lost its retained
    /// messages. Devices a management command removed are skipped, and the failure `reason`
    /// is not recorded, so a republished status carries none.
    async fn republish(
        &self,
        client: &Mqtt,
        config: &ConnectorConfig,
    ) -> Result<(), BoxError> {
        let mut reports: Vec<LinkReport> = self
            .states
            .iter()
            .filter(|(device, _)| config.devices.iter().any(|d| &d.name == *device))
            .map(|(device, status)| LinkReport {
                device: device.clone(),
                status: *status,
                reason: None,
                info: self.infos.get(device).cloned(),
            })
            .collect();
        reports.sort_by(|a, b| a.device.cmp(&b.device));
        publish_links(client, &self.protocol, &reports, config).await
    }
}

/// Reconnect backoff bounds: first retry after one second, doubling to a one-minute cap.
const RECONNECT_INITIAL: Duration = Duration::from_secs(1);
const RECONNECT_MAX: Duration = Duration::from_secs(60);

/// The delay to wait after a reconnect attempt that did not restore data flow. Pure so the
/// schedule is unit-testable.
fn next_backoff(current: Duration) -> Duration {
    current.saturating_mul(2).min(RECONNECT_MAX)
}

/// One device pending transport recovery: entries are created when a whole poll batch fails,
/// re-armed after every reconnect attempt, and removed only once reads succeed again.
struct ReconnectEntry {
    delay: Duration,
    due: Instant,
}

impl ReconnectEntry {
    fn new() -> Self {
        ReconnectEntry {
            delay: RECONNECT_INITIAL,
            due: Instant::now() + RECONNECT_INITIAL,
        }
    }

    fn re_arm(&mut self) {
        self.delay = next_backoff(self.delay);
        self.due = Instant::now() + self.delay;
    }
}

/// Try to re-establish one unhealthy device. Prefers the connector's per-device
/// [`Connector::reconnect`]; falls back to a full [`Connector::connect`] when unsupported.
///
/// A successful transport reconnect is deliberately NOT published as `connected`: an
/// application-level outage keeps the transport connectable while reads still fail, and
/// publishing `connected` here would make the retained link status flap. The next healthy
/// poll batch publishes the `connected` transition (with the stashed device descriptor);
/// failed attempts publish `disconnected` once via `publish_if_changed`.
/// Returns true when the transport is back, so the caller can re-arm anything that died with
/// the old one (push subscriptions).
async fn attempt_reconnect(
    connector: &mut Box<dyn Connector>,
    client: &Mqtt,
    links: &mut LinkTracker,
    device: &str,
    limits: Limits,
    config: &ConnectorConfig,
) -> bool {
    debug!(%device, "attempting reconnect");
    let reports: Vec<LinkReport> = match bounded(
        limits,
        "reconnect",
        connector.reconnect(&device.to_string()),
    )
    .await
    {
        Ok(report) => vec![report],
        Err(ConnectorError::Unsupported(_)) => match bounded(limits, "connect", connector.connect()).await {
            Ok(reports) => reports,
            Err(e) => {
                warn!(%device, "reconnect (full connect) failed: {e}");
                return false;
            }
        },
        Err(e) => {
            warn!(%device, "reconnect failed: {e}");
            return false;
        }
    };
    let mut restored = false;
    for report in reports {
        if report.status == LinkStatus::Connected {
            if report.device == device {
                restored = true;
            }
            links.stash_info(&report.device, report.info.clone());
        } else {
            links.publish_if_changed(client, &report, config).await;
        }
    }
    restored
}

/// The link state to publish after a poll batch, or `None` when nothing changed. Pure so the
/// transition rules are unit-testable.
fn next_link_state(current: Option<LinkStatus>, healthy: bool) -> Option<LinkStatus> {
    if healthy {
        (current != Some(LinkStatus::Connected)).then_some(LinkStatus::Connected)
    } else {
        match current {
            // never connected: stay disconnected rather than "upgrade" to degraded
            Some(LinkStatus::Disconnected) | None => None,
            Some(LinkStatus::Degraded) => None,
            Some(LinkStatus::Connected) => Some(LinkStatus::Degraded),
        }
    }
}

/// A single scheduled read job for one point on one device.
struct ScheduleEntry {
    device_index: usize,
    point: PointRef,
    interval: Duration,
    next_due: Instant,
}

/// Run the connector under the SDK runtime until the process receives Ctrl-C or SIGTERM.
///
/// `config_path` is the file the typed `config` was loaded from; the runtime keeps the raw
/// document so management commands (§6.3) can patch and persist it.
pub async fn run(
    connector: Box<dyn Connector>,
    config: ConnectorConfig,
    config_path: PathBuf,
) -> Result<(), BoxError> {
    run_until(connector, config, config_path, shutdown_signal()).await
}

/// Resolve when the process is asked to stop: Ctrl-C (all platforms) or SIGTERM (unix, what
/// systemd sends on `systemctl stop`).
pub async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// Run the connector under the SDK runtime until `shutdown` resolves.
///
/// This is the composable variant of [`run`]: a host binary that runs several connectors in
/// one process passes each instance the same shutdown trigger and supervises them itself.
pub async fn run_until(
    connector: Box<dyn Connector>,
    config: ConnectorConfig,
    config_path: PathBuf,
    shutdown: impl std::future::Future<Output = ()> + Send,
) -> Result<(), BoxError> {
    run_until_watched(connector, config, config_path, shutdown, Progress::new()).await
}

/// Same as [`run_until`], but stamping `progress` on every loop iteration so a supervisor can
/// tell a wedged connector from a quiet one and restart it (see [`Progress`]).
pub async fn run_until_watched(
    connector: Box<dyn Connector>,
    config: ConnectorConfig,
    config_path: PathBuf,
    shutdown: impl std::future::Future<Output = ()> + Send,
    progress: Progress,
) -> Result<(), BoxError> {
    let never = Arc::new(tokio::sync::Notify::new());
    run_until_reloadable(connector, config, config_path, shutdown, progress, never)
        .await
        .map(|_| ())
}

/// How long a connector attempt waits for the broker to accept its session before it fails.
const BROKER_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long one publish may wait for the MQTT client to take it: long enough for a busy but
/// connected client, short enough that a connection gone before the event loop noticed (a
/// half-open socket) does not hold up the main loop for long.
const PUBLISH_TIMEOUT: Duration = Duration::from_secs(2);
/// How long a stopping connector gives its final health and DISCONNECT to reach the broker.
const MQTT_FLUSH_TIMEOUT: Duration = Duration::from_secs(2);

/// A spawned task that is cancelled when its handle is dropped. A plain `JoinHandle` detaches the
/// task instead, so whoever gives up on it — a cancelled supervisor, a connector attempt that
/// ended — would leave it running.
pub struct AbortOnDrop<T>(pub tokio::task::JoinHandle<T>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl<T> Future for AbortOnDrop<T> {
    type Output = Result<T, tokio::task::JoinError>;

    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        std::pin::Pin::new(&mut self.0).poll(cx)
    }
}

/// How [`run_until_reloadable`] ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunExit {
    /// `shutdown` resolved.
    Stopped,
    /// A reload found a change the running connector cannot adopt in place — another service
    /// name, protocol, broker or stall timeout: the caller restarts it from the file.
    Restart,
}

/// Same as [`run_until_watched`], and re-reading `config_path` each time `reload` is notified.
/// The host binary notifies it on SIGHUP, so an edited configuration takes effect without
/// restarting the service.
///
/// A reload is applied in place, the way a management command's change is (§6.3): the protocol
/// module is reconfigured, every device reconnected, and the retained capability descriptor and
/// link status republished, while the MQTT session — and with it the service health — stays up.
/// A file that no longer loads, or that the protocol module rejects, is reported and the running
/// configuration kept; a file that resolves to the configuration already running changes nothing.
pub async fn run_until_reloadable(
    mut connector: Box<dyn Connector>,
    mut config: ConnectorConfig,
    config_path: PathBuf,
    shutdown: impl std::future::Future<Output = ()> + Send,
    progress: Progress,
    reload: Arc<tokio::sync::Notify>,
) -> Result<RunExit, BoxError> {
    let mut limits = Limits::from_config(&config);
    let protocol = config.connector.protocol.clone();
    let service = config.connector.service_name();

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
    let caps = effective_capabilities(&connector.capabilities());

    // 2. MQTT setup.
    let health_topic = format!("te/device/main/service/{service}/status/health");
    let cap_topic = format!("te/device/main/service/{service}/ot/capabilities");
    // Device commands for the whole protocol, and management commands for this service (§6).
    // Every instance of the protocol on the broker receives every device command, so the one
    // it acts on is decided per message (`route_command`) against the live configuration —
    // which define-device/remove-device change, so a subscription per device would go stale.
    // The thin-edge command topics (§6, RFC 0006 §7). `+` matches one level including an
    // empty one, but the empty levels are written out so the filter cannot also match a
    // connector-ish topic that happens to have the same segment count.
    let device_cmd_sub = "te/device/+///cmd/+/+".to_string();
    let service_cmd_sub = format!("te/device/main/service/{service}/cmd/+/+");

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
    // Every CONNACK after the first is a reconnect. The session is clean, so the broker has
    // forgotten the command subscriptions (commands would silently never arrive again), and a
    // broker that restarted without persistence every retained message too. The main loop owns
    // what must be restored — the live config and link states — so this task only signals it.
    let reconnected = Arc::new(tokio::sync::Notify::new());
    let reconnected_tx = reconnected.clone();
    // Whether the broker is connected right now. While it is not, the event loop only retries the
    // connection and never reads the client's request queue, so whatever is published fills it
    // and the next `publish().await` blocks — and the main loop with it: no reload, no stop, until
    // the broker is back. The main loop checks this before publishing samples.
    let (online_tx, mut online) = tokio::sync::watch::channel(false);
    let client = Mqtt {
        client,
        online: online.clone(),
    };
    // Ends with this function, however it returns: the process may host other connectors and
    // restart this one, and a leaked event loop would keep its session connected (or keep
    // reconnecting) behind it.
    let mut mqtt_task = AbortOnDrop(tokio::spawn(async move {
        let mut sessions = 0u64;
        loop {
            match eventloop.poll().await {
                Ok(Event::Incoming(Packet::Publish(p))) => {
                    if incoming_tx.send(p).await.is_err() {
                        break; // runtime shut down
                    }
                }
                Ok(Event::Incoming(Packet::ConnAck(_))) => {
                    online_tx.send_replace(true);
                    sessions += 1;
                    if sessions > 1 {
                        info!("reconnected to MQTT broker");
                        reconnected_tx.notify_one();
                    }
                }
                // The clean shutdown's DISCONNECT is written, after everything queued before it
                // (the final health "down"): this session is over.
                Ok(Event::Outgoing(rumqttc::Outgoing::Disconnect)) => break,
                Ok(_) => {}
                Err(rumqttc::ConnectionError::RequestsDone) => break,
                Err(e) => {
                    online_tx.send_replace(false);
                    warn!("mqtt event loop error: {e}; retrying");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }
    }));

    // The broker must accept the session before the connector starts: nothing it publishes could
    // be delivered otherwise, and a wrong host or port would pass for a running connector. As in
    // the C runtime, not connecting fails this attempt, which the host retries on its backoff — or
    // at once on a reload, since the fix is usually in the file. A stop or a reload is honoured
    // while waiting.
    tokio::pin!(shutdown);
    let broker = format!("{}:{}", config.mqtt.host, config.mqtt.port);
    tokio::select! {
        _ = &mut shutdown => return Ok(RunExit::Stopped),
        _ = reload.notified() => {
            info!("reload requested while connecting to the MQTT broker {broker}; restarting");
            return Ok(RunExit::Restart);
        }
        connected = tokio::time::timeout(BROKER_CONNECT_TIMEOUT, online.wait_for(|up| *up)) => {
            if !matches!(connected, Ok(Ok(_))) {
                return Err(format!(
                    "cannot connect to the MQTT broker {broker} within {}s",
                    BROKER_CONNECT_TIMEOUT.as_secs()
                )
                .into());
            }
        }
    }

    // 3. Publish capability descriptor + service health (retained).
    publish_retained(&client, &cap_topic, capability_payload(&caps)).await?;
    publish_health(&client, &health_topic, "up").await?;
    client.subscribe(&device_cmd_sub, QoS::AtLeastOnce).await?;
    client.subscribe(&service_cmd_sub, QoS::AtLeastOnce).await?;
    info!(%protocol, %service, "connector started");

    // 4. Publish every device's manifest (§8.2) — before its link status and before any sample,
    // on this same connection — then connect to the devices and publish their link status.
    let mut links = LinkTracker::new(
        &protocol,
        crate::commands::device_command_types(&caps.command_verbs, &config.connector.command_aliases),
    );
    links.publish_manifests(&client, &config).await?;
    match bounded(limits, "connect", connector.connect()).await {
        Ok(reports) => links.publish_reports(&client, &reports, &config).await?,
        Err(e) => warn!("initial connect failed: {e}"),
    }
    // 5. Set up push delivery for subscribe-capable connectors, then build the polling
    // schedule for everything that is not pushed. The runtime keeps `sample_tx` alive for
    // the whole run so re-subscribing after a config reload reuses the same channel.
    let (sample_tx, mut sample_rx) = tokio::sync::mpsc::channel::<Sample>(256);
    let mut subscribed =
        setup_subscriptions(&mut connector, &config, caps.subscribe, &sample_tx, limits).await;
    let mut schedule = build_schedule(&config, &subscribed);
    let mut sample_debug = config.connector.sample_debug;
    // The per-point publish policy (§5.4) is applied here, once, for every consumer.
    let mut publish_gate = PublishGate::new(&config);
    let mut seq_counters: HashMap<(String, String), u64> = HashMap::new();
    // Devices whose transport needs re-establishing, keyed by device name.
    let mut reconnects: HashMap<String, ReconnectEntry> = HashMap::new();

    // 6. Main loop: poll due points on a tick, route commands from the MQTT event-loop task.
    let mut tick = tokio::time::interval(Duration::from_millis(200));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Set inside the loop: acted on after the iteration (`rearm`), or once the loop ends (`exit`).
    let mut rearm = false;
    let mut exit = RunExit::Stopped;

    loop {
        tokio::select! {
            _ = &mut shutdown => {
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
                    match bounded(limits, "read", connector.read_points(&device, &points)).await {
                        Ok(mut samples) => {
                            for s in samples.iter_mut() {
                                // The runtime owns the device identity for polled reads:
                                // connectors routinely leave `device` empty, and the sample
                                // topic + meta lookup are keyed by the configured name.
                                s.device = device.clone();
                                publish_sample(
                                    &client, &protocol, s, &mut seq_counters,
                                    &mut publish_gate, sample_debug,
                                )
                                .await;
                            }
                            // A batch where every point failed means the device itself is
                            // unreachable (a single bad point keeps the link healthy).
                            if !samples.is_empty() {
                                let healthy =
                                    samples.iter().any(|s| s.quality != crate::model::Quality::Bad);
                                let reason = (!healthy)
                                    .then(|| samples.iter().find_map(|s| s.error.clone()))
                                    .flatten();
                                links.note_poll(&client, &device, healthy, reason, &config).await;
                                if healthy {
                                    reconnects.remove(&device);
                                } else {
                                    reconnects.entry(device.clone()).or_insert_with(ReconnectEntry::new);
                                }
                            }
                        }
                        Err(e) => {
                            warn!(%device, "read_points failed: {e}");
                            links
                                .note_poll(&client, &device, false, Some(e.to_string()), &config)
                                .await;
                            reconnects.entry(device.clone()).or_insert_with(ReconnectEntry::new);
                        }
                    }
                }
                // The loop completed an iteration: samples published, reconnects attempted.
                // A supervisor watching this marker restarts the connector if it stops moving.
                progress.mark();
                // Re-establish unhealthy devices on their backoff schedule. Entries stay
                // until reads succeed: a transport that reconnects while the device still
                // fails (application-level outage) keeps backing off instead of storming.
                let now = Instant::now();
                let due: Vec<String> = reconnects
                    .iter()
                    .filter(|(_, entry)| entry.due <= now)
                    .map(|(device, _)| device.clone())
                    .collect();
                for device in due {
                    let restored =
                        attempt_reconnect(
                            &mut connector, &client, &mut links, &device, limits, &config,
                        )
                            .await;
                    if let Some(entry) = reconnects.get_mut(&device) {
                        entry.re_arm();
                    }
                    // A push subscription dies with the transport it was created on. Without
                    // re-arming it here the device's subscribed points stay OFF the polling
                    // schedule with nothing delivering them -- silent for good, behind a link
                    // that recovers to `connected` on the next healthy poll.
                    if restored && caps.subscribe {
                        if let Some((device_index, device_config)) = config
                            .devices
                            .iter()
                            .enumerate()
                            .find(|(_, d)| d.name == device)
                        {
                            subscribe_device(
                                &mut connector,
                                &config,
                                device_index,
                                device_config,
                                &sample_tx,
                                limits,
                                &mut subscribed,
                            )
                            .await;
                            schedule = build_schedule(&config, &subscribed);
                        }
                    }
                }
            }
            Some(mut sample) = sample_rx.recv() => {
                publish_sample(
                    &client, &protocol, &mut sample, &mut seq_counters,
                    &mut publish_gate, sample_debug,
                )
                .await;
                progress.mark();
            }
            _ = reconnected.notified() => {
                if let Err(e) = restore_mqtt_session(
                    &client, &[&device_cmd_sub, &service_cmd_sub], &health_topic, &cap_topic,
                    &caps, &config, &links,
                ).await {
                    warn!("failed to restore the MQTT session after reconnecting: {e}");
                }
                progress.mark();
            }
            _ = reload.notified() => {
                match reload_from_file(
                    &mut connector, &client, &mut links, &mut config, &mut config_doc,
                    &config_path, limits,
                ).await {
                    Reloaded::Applied => rearm = true,
                    Reloaded::Unchanged | Reloaded::Kept => {}
                    Reloaded::Restart => {
                        exit = RunExit::Restart;
                        break;
                    }
                }
                progress.mark();
            }
            Some(p) = incoming_rx.recv() => {
                match handle_command(
                    &mut connector, &client, &service, &mut links,
                    &mut config, &mut config_doc, &config_path,
                    &p.topic, &p.payload, limits,
                ).await {
                    // A management command changed the config (see below).
                    Ok(true) => rearm = true,
                    Ok(false) => {}
                    Err(e) => warn!("command handling error: {e}"),
                }
                progress.mark();
            }
        }
        // A new configuration was applied — by a management command or a reload: re-establish
        // push delivery (reconnecting dropped the old subscriptions), and rebuild the polling
        // schedule and everything else derived from the configuration.
        if std::mem::take(&mut rearm) {
            limits = Limits::from_config(&config);
            subscribed = setup_subscriptions(
                &mut connector, &config, caps.subscribe, &sample_tx, limits,
            ).await;
            schedule = build_schedule(&config, &subscribed);
            sample_debug = config.connector.sample_debug;
            publish_gate.reload(&config);
            seq_counters.clear();
            // applying the configuration already reconnected every device
            reconnects.clear();
        }
    }

    // 7. Clean shutdown: the final health "down", then a DISCONNECT, given a moment to reach the
    // broker (the event loop ends once the DISCONNECT is written). With the broker unreachable
    // there is nothing to send them over, and the broker publishes the last will instead.
    let _ = bounded(limits, "disconnect", connector.disconnect()).await;
    if client.is_online() {
        let flush = async {
            publish_health(&client, &health_topic, "down").await.ok();
            client.disconnect().await.ok();
            let _ = (&mut mqtt_task).await;
        };
        let _ = tokio::time::timeout(MQTT_FLUSH_TIMEOUT, flush).await;
    }
    Ok(exit)
}

/// Run the connector without a broker until `shutdown` resolves: every sample is printed to
/// stdout as one JSON envelope per line (NDJSON) instead of being published over MQTT. The
/// envelope carries the source `device`, so interleaved output from several connectors (or
/// devices) stays identifiable.
///
/// This powers `tedge-dot run --output stdout` for local exploration and piping into other
/// tools. It reuses the exact scheduling, subscription, seq and meta handling of the MQTT
/// runtime; link transitions are logged via tracing, and the broker-borne features (health,
/// capabilities, commands/management) are simply absent.
pub async fn run_stdout_until(
    mut connector: Box<dyn Connector>,
    config: ConnectorConfig,
    shutdown: impl std::future::Future<Output = ()> + Send,
) -> Result<(), BoxError> {
    let limits = Limits::from_config(&config);
    connector
        .configure(&config)
        .map_err(|e| format!("configure failed: {e}"))?;
    let caps = connector.capabilities();

    match bounded(limits, "connect", connector.connect()).await {
        Ok(reports) => {
            for report in &reports {
                info!(device = %report.device, status = report.status.as_str(),
                    reason = report.reason.as_deref().unwrap_or(""), "link");
            }
        }
        Err(e) => warn!("initial connect failed: {e}"),
    }

    let (sample_tx, mut sample_rx) = tokio::sync::mpsc::channel::<Sample>(256);
    let subscribed =
        setup_subscriptions(&mut connector, &config, caps.subscribe, &sample_tx, limits).await;
    let mut schedule = build_schedule(&config, &subscribed);
    let sample_debug = config.connector.sample_debug;
    let mut publish_gate = PublishGate::new(&config);
    let mut seq_counters: HashMap<(String, String), u64> = HashMap::new();
    let mut reconnects: HashMap<String, ReconnectEntry> = HashMap::new();

    let mut tick = tokio::time::interval(Duration::from_millis(200));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            _ = tick.tick() => {
                let now = Instant::now();
                let mut due: HashMap<usize, Vec<PointRef>> = HashMap::new();
                for entry in schedule.iter_mut() {
                    if entry.next_due <= now {
                        due.entry(entry.device_index).or_default().push(entry.point.clone());
                        entry.next_due = now + entry.interval;
                    }
                }
                for (device_index, points) in due {
                    let device = config.devices[device_index].name.clone();
                    match bounded(limits, "read", connector.read_points(&device, &points)).await {
                        Ok(mut samples) => {
                            for s in samples.iter_mut() {
                                s.device = device.clone();
                                print_sample(s, &mut seq_counters, &mut publish_gate, sample_debug);
                            }
                            let healthy = samples.is_empty()
                                || samples.iter().any(|s| s.quality != crate::model::Quality::Bad);
                            if healthy {
                                reconnects.remove(&device);
                            } else {
                                reconnects.entry(device.clone()).or_insert_with(ReconnectEntry::new);
                            }
                        }
                        Err(e) => {
                            warn!(%device, "read_points failed: {e}");
                            reconnects.entry(device.clone()).or_insert_with(ReconnectEntry::new);
                        }
                    }
                }
                let now = Instant::now();
                let due: Vec<String> = reconnects
                    .iter()
                    .filter(|(_, entry)| entry.due <= now)
                    .map(|(device, _)| device.clone())
                    .collect();
                for device in due {
                    debug!(%device, "attempting reconnect");
                    match connector.reconnect(&device).await {
                        Ok(_) => {}
                        Err(ConnectorError::Unsupported(_)) => {
                            if let Err(e) = connector.connect().await {
                                warn!(%device, "reconnect (full connect) failed: {e}");
                            }
                        }
                        Err(e) => warn!(%device, "reconnect failed: {e}"),
                    }
                    if let Some(entry) = reconnects.get_mut(&device) {
                        entry.re_arm();
                    }
                }
            }
            Some(mut sample) = sample_rx.recv() => {
                print_sample(&mut sample, &mut seq_counters, &mut publish_gate, sample_debug);
            }
        }
    }

    let _ = bounded(limits, "disconnect", connector.disconnect()).await;
    Ok(())
}

/// Stamp the per-point sequence number and print one sample envelope to stdout (one JSON
/// object per line); the stdout counterpart of [`publish_sample`].
fn print_sample(
    sample: &mut Sample,
    seq_counters: &mut HashMap<(String, String), u64>,
    gate: &mut PublishGate,
    debug: bool,
) {
    let counter = seq_counters
        .entry((sample.device.clone(), sample.point.clone()))
        .or_insert(0);
    *counter += 1;
    sample.seq = Some(*counter);
    if !gate.admits(sample) {
        return;
    }
    println!("{}", sample.to_envelope(debug));
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
            let mut point = point_ref(point);
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
    limits: Limits,
) -> HashSet<(usize, String)> {
    let mut subscribed = HashSet::new();
    if !subscribe_capable {
        return subscribed;
    }
    for (device_index, device) in config.devices.iter().enumerate() {
        subscribe_device(
            connector,
            config,
            device_index,
            device,
            sink,
            limits,
            &mut subscribed,
        )
        .await;
    }
    subscribed
}

/// Arm push delivery for ONE device, recording its points in `subscribed` on success and
/// removing them on failure (so they fall back to the polling schedule).
///
/// Separate from [`setup_subscriptions`] because a device that reconnects has to be
/// re-subscribed on its own: its monitored items died with the old session, while its
/// siblings' are still live and must not be created twice.
async fn subscribe_device(
    connector: &mut Box<dyn Connector>,
    config: &ConnectorConfig,
    device_index: usize,
    device: &crate::config::DeviceConfig,
    sink: &SampleSink,
    limits: Limits,
    subscribed: &mut HashSet<(usize, String)>,
) {
    let connector_default = parse_duration(&config.connector.poll_interval)
        .unwrap_or_else(|| Duration::from_secs(2));
    {
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
                let mut r = point_ref(p);
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
            return;
        }
        // Drop any stale entries first: on a re-subscribe these points are currently marked
        // as pushed, and if the call below fails they must go back to being polled rather
        // than stay off the schedule with no subscription behind them.
        for p in &points {
            subscribed.remove(&(device_index, p.id.clone()));
        }
        match bounded(
            limits,
            "subscribe",
            connector.subscribe(&device.name, &points, sink.clone()),
        )
        .await
        {
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
}

/// The declared `type` of one configured device (§3.1), if it has one. Used verbatim: the
/// loader normalised and validated it (`library::expand`), so trimming here — and only here —
/// would make the link status spell the type differently from the samples and the set names.
fn device_type_of<'a>(config: &'a ConnectorConfig, device: &str) -> Option<&'a str> {
    config
        .devices
        .iter()
        .find(|d| d.name == device)
        .and_then(|d| d.device_type.as_deref())
        .filter(|t| !t.is_empty())
}

/// Stamp the per-point sequence number and publish one sample. Shared by the polling loop and
/// the subscription channel so both paths get identical seq/topic handling.
async fn publish_sample(
    client: &Mqtt,
    protocol: &str,
    sample: &mut Sample,
    seq_counters: &mut HashMap<(String, String), u64>,
    gate: &mut PublishGate,
    debug: bool,
) {
    let counter = seq_counters
        .entry((sample.device.clone(), sample.point.clone()))
        .or_insert(0);
    *counter += 1;
    sample.seq = Some(*counter);
    // The per-point publish policy (§5.4). `seq` is stamped first, so a consumer sees the gap
    // a suppressed reading leaves and can tell "nothing changed" from "nothing was read".
    if !gate.admits(sample) {
        return;
    }
    if !client.is_online() {
        // With the broker unreachable a queued sample cannot be sent, and it would take the room
        // the state messages need in the client's request queue (see `publish_retained`). A
        // sample is a reading of the moment: it is dropped, and the gap shows in `seq`.
        return;
    }
    let topic = format!(
        "te/device/{}/ot/{}/sample/{}",
        sample.device, protocol, sample.point
    );
    let payload = sample.to_envelope(debug).to_string();
    let publish = client.publish(&topic, QoS::AtMostOnce, false, payload);
    match tokio::time::timeout(PUBLISH_TIMEOUT, publish).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => error!("failed to publish sample: {e}"),
        // The connection is gone but the event loop has not noticed yet (a half-open socket).
        Err(_) => warn!(
            "dropping a sample on {topic}: the MQTT client did not take it within {}s",
            PUBLISH_TIMEOUT.as_secs()
        ),
    }
}

/// The per-point publish policy of §5.4, applied by the runtime to the sample stream itself.
///
/// The four settings — `on_change`, `deadband`, `min_interval`, `debounce` — are
/// protocol-neutral and identical in every deployment, so they belong below every consumer
/// rather than in each flow that wants them: the measurement flow, the parameter twin, a
/// historian and a future MCP subscription then all see one stream with the policy already
/// applied, and the code lands once per SDK instead of once per flow.
///
/// Only `good` samples are gated. A `bad` sample is still published — contract §5.1 requires
/// it, an operator must see a failing read — and keeps the runtime's own bad-sample rate
/// limit; `stale` is unaffected. A consumer that needs every raw read leaves `publish`
/// undeclared on that point, which is the default.
#[derive(Default)]
struct PublishGate {
    /// The declared policy per `(device, point)`; points that declare none are absent.
    policies: HashMap<(String, String), crate::config::PublishPolicy>,
    /// The last value published, and when, per `(device, point)`.
    last: HashMap<(String, String), (serde_json::Value, OffsetDateTime)>,
    /// A value waiting out its `debounce` period: the candidate and when it was first seen.
    candidate: HashMap<(String, String), (serde_json::Value, OffsetDateTime)>,
}

impl PublishGate {
    fn new(config: &ConnectorConfig) -> Self {
        let mut gate = PublishGate::default();
        gate.reload(config);
        gate
    }

    /// Re-read the policies after a configuration change, dropping the state of points that
    /// are gone. State of a point that survives is kept: a reload is not a reason to republish
    /// a value that has not changed.
    fn reload(&mut self, config: &ConnectorConfig) {
        self.policies.clear();
        for device in &config.devices {
            for point in &device.points {
                if let Some(policy) = &point.publish {
                    if !policy.is_noop() {
                        self.policies
                            .insert((device.name.clone(), point.id.clone()), policy.clone());
                    }
                }
            }
        }
        self.last.retain(|key, _| self.policies.contains_key(key));
        self.candidate.retain(|key, _| self.policies.contains_key(key));
    }

    /// Whether this sample is published. Mirrors the order `ot-measurement` applied in 0.1:
    /// debounce, then change detection, then the rate limit.
    fn admits(&mut self, sample: &Sample) -> bool {
        if sample.quality != crate::model::Quality::Good {
            return true;
        }
        let key = (sample.device.clone(), sample.point.clone());
        let Some(policy) = self.policies.get(&key) else {
            return true;
        };
        let Some(value) = sample.value.as_ref().map(|v| v.to_json()) else {
            return true;
        };
        let now = sample.ts;
        let deadband = policy.deadband.unwrap_or(0.0);

        // Debounce: a changed value is accepted only once it has been observed stable for the
        // period. The first sighting is the candidate and is not published.
        if let Some(debounce) = policy.debounce.as_deref().and_then(parse_duration) {
            match self.candidate.get(&key) {
                Some((candidate, since)) if values_equal(candidate, &value, 0.0) => {
                    if now - *since < debounce {
                        return false; // still settling
                    }
                }
                _ => {
                    self.candidate.insert(key.clone(), (value.clone(), now));
                    return false; // new candidate: wait for it to prove stable
                }
            }
        }

        if let Some((last, last_ts)) = self.last.get(&key) {
            if policy.on_change() && values_equal(last, &value, deadband) {
                return false;
            }
            if let Some(min_interval) = policy.min_interval.as_deref().and_then(parse_duration) {
                if now - *last_ts < min_interval {
                    return false;
                }
            }
        }

        self.last.insert(key, (value, now));
        true
    }
}

/// Whether two sample values count as the same reading. Numbers compare within `deadband`
/// (or exactly, when none is declared); everything else compares by equality, so a `bool` or
/// a `string` point gets change detection too.
fn values_equal(a: &serde_json::Value, b: &serde_json::Value, deadband: f64) -> bool {
    match (a.as_f64(), b.as_f64()) {
        (Some(a), Some(b)) => {
            let min_delta = if deadband > 0.0 { deadband } else { f64::EPSILON };
            (a - b).abs() < min_delta
        }
        _ => a == b,
    }
}

/// Turn a write request's value — which is in **engineering units**, the same units a sample's
/// `value` carries (contract §4.2) — into the wire value a module encodes.
///
/// A read scales the wire value by the point's `transform`; a write inverts it, so the round
/// trip closes: write 20 °C to a point with `decimal_shift = -3` and the next read says 20,
/// not 0.02. The inversion happens here, once, for every protocol and every write path
/// (`write`, `write-batch`, and the `tedge-dot write --value` CLI), so a module keeps
/// receiving exactly what it receives today: the wire value.
///
/// Values a transform never touches — booleans, strings, a `bytes` hex string — pass through,
/// and so does every value of a point whose transform is the identity, which leaves an
/// untransformed point's write byte-for-byte as it was.
///
/// For an integer datatype the inverted value is rounded to nearest (an operator editing a
/// °C twin should not be refused for a value the register cannot express exactly), and a value
/// the datatype could not hold afterwards fails the write rather than wrapping on the wire.
pub fn wire_value(
    point: &crate::config::PointConfig,
    value: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    // The declared engineering-unit bounds (§5.3), checked here — before the transform is
    // inverted and before the device is touched — so the limit means one thing on both paths.
    // A limit is a property of the signal, not of whatever is asking: a script, another flow
    // or a typo in an operation reaches the same check the cloud form renders.
    if let (Some(range), Some(engineering)) = (&point.range, value.as_f64()) {
        if let Some(reason) = range.reject(&point.id, engineering) {
            return Err(reason);
        }
    }
    let transform = point.transform.unwrap_or_default();
    if transform.is_identity() {
        return Ok(value.clone());
    }
    let Some(engineering) = value.as_f64() else {
        // A bool, a string, or a `bytes` hex payload: the transform never applied to it.
        return Ok(value.clone());
    };
    if !transform.is_invertible() {
        // The loader refuses this at configuration time; this is the belt to that's braces.
        return Err(format!(
            "point {} is writable but its transform cannot be inverted (multiplier = 0)",
            point.id
        ));
    }
    let Some(crate::model::Value::Number(mut wire)) =
        transform.invert(crate::model::Value::Number(engineering))
    else {
        return Ok(value.clone());
    };
    let datatype = point.datatype;
    let integral = datatype.is_integer();
    if integral {
        wire = wire.round();
    }
    if let Some((min, max)) = datatype.value_range() {
        if !wire.is_finite() || wire < min || wire > max {
            return Err(format!(
                "value {engineering} does not fit {} after transform (wire value {wire})",
                serde_json::to_value(datatype)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_string))
                    .unwrap_or_default(),
            ));
        }
    }
    // An integer datatype gets a JSON integer, not `12345.0`: a module builds a typed value
    // from this number (OPC UA a variant, Modbus a register), and a fractional JSON number for
    // an integer point is not something a requester would ever send.
    if integral && wire.abs() < 9e15 {
        return Ok(serde_json::json!(wire as i64));
    }
    Ok(serde_json::json!(wire))
}

/// The configured point of a device, for the write paths that must resolve a request's point
/// against the live configuration before touching the device.
fn configured_point<'a>(
    config: &'a ConnectorConfig,
    device: &str,
    point: &str,
) -> Option<&'a crate::config::PointConfig> {
    config
        .devices
        .iter()
        .find(|d| d.name == device)?
        .points
        .iter()
        .find(|p| p.id == point)
}

/// Build a resolved [`PointRef`] from a configured point. Shared by the scheduler and by callers
/// (e.g. a CLI) that drive a connector's `read_points`/`execute` directly.
pub fn point_ref(point: &crate::config::PointConfig) -> PointRef {
    PointRef {
        id: point.id.clone(),
        datatype: point.datatype,
        endianness: Endianness::parse(point.endianness.as_deref()),
        word_order: WordOrder::parse(point.word_order.as_deref()),
        access: Access::parse(point.access.as_deref()),
        unit: point.unit.clone(),
        transform: point.transform.unwrap_or_default(),
        interval: point.poll_interval.as_deref().and_then(parse_duration),
    }
}

/// Who a message on the connector's command subscriptions is for (contract §6).
#[derive(Debug, PartialEq)]
enum CommandRoute<'a> {
    /// `te/device/<device>///cmd/<type>/<id>` for a device this instance's configuration
    /// defines. `verb` is the contract verb the command type resolved to.
    Device { device: &'a str, verb: &'static str },
    /// `te/device/main/service/<service>/cmd/<type>/<id>` for this instance's service.
    Service { verb: &'static str },
    /// Anything else — above all a command for a device this instance does not own, or a
    /// command type it does not answer. Left unanswered: every instance of the protocol on the
    /// broker receives it, and an "unknown device" failure from one that does not own it would
    /// race (and usually beat) the owner's real result.
    Elsewhere,
}

/// Route a thin-edge command topic against the live configuration, so the devices an instance
/// answers for follow define-device/remove-device, and an unknown command type is ignored
/// rather than answered.
fn route_command<'a>(topic: &'a str, service: &str, config: &ConnectorConfig) -> CommandRoute<'a> {
    let aliases = &config.connector.command_aliases;
    let parts: Vec<&'a str> = topic.split('/').collect();
    match parts[..] {
        ["te", "device", device, "", "", "cmd", command_type, _id] => {
            let Some(verb) = crate::commands::verb_of_type(command_type, aliases) else {
                return CommandRoute::Elsewhere;
            };
            // A management verb here is a Cloud Fieldbus operation on the GATEWAY, which is
            // nobody's configured device — so the device segment decides nothing and the
            // payload's `service` is the whole claim (§6.6). Point I/O still needs the device.
            if crate::commands::MANAGEMENT_VERBS.contains(&verb)
                || config.devices.iter().any(|d| d.name == device)
            {
                CommandRoute::Device { device, verb }
            } else {
                CommandRoute::Elsewhere
            }
        }
        ["te", "device", "main", "service", s, "cmd", command_type, _id] if s == service => {
            match crate::commands::verb_of_type(command_type, aliases) {
                Some(verb) => CommandRoute::Service { verb },
                None => CommandRoute::Elsewhere,
            }
        }
        _ => CommandRoute::Elsewhere,
    }
}

/// Whether this instance owns a device-topic command: it must define the device, and the
/// request must name at least one point it has (§6.5).
///
/// The point check is what keeps two connectors that share a device apart — with the protocol
/// still in the sample topics but gone from the command topic, "the device" is no longer enough
/// to say whose command it is.
///
/// *At least one*, not all: a request naming a point nobody has — a typo, a stale cloud
/// definition — would otherwise be answered by nobody and hang at `init` for ever, which is a
/// worse failure than the ambiguity this check guards against. The owner takes it and fails it
/// with a reason instead; for a batch, `handle_write_batch` rejects an unknown point before the
/// first write, so nothing is applied.
///
/// A request naming no point at all (a management verb, or a malformed batch) is left to the
/// device check alone — and, for a management verb, to the `service` claim.
fn owns_request(config: &ConnectorConfig, device: &str, json: &serde_json::Value) -> bool {
    let Some(device_config) = config.devices.iter().find(|d| d.name == device) else {
        return false;
    };
    let has = |point: &str| device_config.points.iter().any(|p| p.id == point);
    if let Some(point) = json.get("point").and_then(|p| p.as_str()) {
        return has(point);
    }
    if let Some(writes) = json.get("writes").and_then(|w| w.as_array()) {
        if writes.is_empty() {
            // A malformed batch touches nothing; the device's owner says so.
            return true;
        }
        return writes
            .iter()
            .any(|w| w.get("point").and_then(|p| p.as_str()).map(has).unwrap_or(false));
    }
    true
}

#[allow(clippy::too_many_arguments)]
async fn handle_command(
    connector: &mut Box<dyn Connector>,
    client: &Mqtt,
    service: &str,
    links: &mut LinkTracker,
    config: &mut ConnectorConfig,
    config_doc: &mut DocumentMut,
    config_path: &Path,
    topic: &str,
    payload: &[u8],
    limits: Limits,
) -> Result<bool, BoxError> {
    let route = route_command(topic, service, config);
    let (device, verb) = match route {
        CommandRoute::Device { device, verb } => (device.to_string(), verb),
        CommandRoute::Service { verb } => (String::new(), verb),
        CommandRoute::Elsewhere => return Ok(false),
    };

    let json: serde_json::Value = match serde_json::from_slice(payload) {
        Ok(v) => v,
        Err(_) => return Ok(false), // empty/clearing message or junk
    };
    let status = json.get("status").and_then(|s| s.as_str()).unwrap_or("");
    if status != "init" {
        return Ok(false); // only act on new requests; ignore our own transitions
    }

    // Management verbs (§6.3) mutate and persist THIS instance's configuration, so exactly one
    // instance may answer one. Two topics carry them:
    //
    //   * the service topic, which names the instance outright;
    //   * a device topic — where a Cloud Fieldbus operation on the gateway arrives, because the
    //     c8y mapper publishes an operation on the entity it belongs to and that is the gateway,
    //     never a connector's service. There the payload's `service` field is the claim: an
    //     instance acts only when it names this one, and a request naming no service is
    //     nobody's and stays at `init`, like a command for an unowned device (§6.5).
    let service_cmd = matches!(route, CommandRoute::Service { .. });
    if is_management_verb(verb) {
        if !service_cmd {
            let claimed = json.get("service").and_then(|s| s.as_str());
            if claimed != Some(service) {
                debug!(
                    %verb,
                    claimed = claimed.unwrap_or("<none>"),
                    "management command on a device topic is not addressed to this service"
                );
                return Ok(false);
            }
        }
        return handle_management(
            connector, client, links, config, config_doc, config_path, topic, verb,
            &json, limits,
        )
        .await;
    }
    // A write verb on the service topic is refused rather than ignored: the topic already names
    // this instance as the only addressee, so the refusal cannot race another instance's answer.
    if service_cmd {
        let reason = format!(
            "'{verb}' is not a service command: only set-config, define-device and \
             remove-device are; a write goes to te/device/<device>///cmd/ot_write/<id>"
        );
        warn!(%verb, "command refused: {reason}");
        let failed = serde_json::json!({ "status": "failed", "reason": reason });
        publish_retained(client, topic, with_origin(failed, json.get("origin")).to_string())
            .await?;
        return Ok(false);
    }

    // Ownership is by device AND point (§6.5). With the protocol gone from the command topic,
    // the device alone no longer says whose command it is: two connectors may serve one device,
    // and the one that has the point is the one that answers. Anything else is left at `init`
    // for the owner, never failed — a failure from a non-owner would overwrite its result.
    if !owns_request(config, &device, &json) {
        debug!(%device, %verb, "command names a point this instance does not have");
        return Ok(false);
    }

    // `write-batch` (§6.4) is implemented once here on top of the module's `write`.
    if verb == "write-batch" {
        handle_write_batch(connector, client, topic, &device, config, &json, limits).await?;
        debug!(%device, %verb, "command handled");
        return Ok(false);
    }

    let point = json
        .get("point")
        .and_then(|p| p.as_str())
        .unwrap_or_default()
        .to_string();
    let origin = json.get("origin");

    // The request's `value` is in engineering units (§4.2). Invert the point's transform here,
    // before the module sees it, so that what an operator edits on the twin is what reaches the
    // device. An unknown point is left to the module, whose "unknown point" error is the
    // canonical one.
    let requested = json.get("value").cloned();
    let wire = match (&requested, configured_point(config, &device, &point)) {
        (Some(value), Some(configured)) => match wire_value(configured, value) {
            Ok(wire) => Some(wire),
            Err(reason) => {
                publish_retained(
                    client,
                    topic,
                    with_origin(
                        serde_json::json!({ "status": "failed", "point": point, "reason": reason }),
                        origin,
                    )
                    .to_string(),
                )
                .await?;
                debug!(%device, %verb, "command rejected before the device was touched");
                return Ok(false);
            }
        },
        _ => requested.clone(),
    };
    let request = CommandRequest {
        point: point.clone(),
        value: wire,
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
        with_origin(
            serde_json::json!({ "status": "executing", "point": point }),
            origin,
        )
        .to_string(),
    )
    .await?;

    match bounded(limits, "write", connector.execute(&device, verb, &request)).await {
        Ok(result) => {
            let mut obj = serde_json::Map::new();
            obj.insert("status".into(), serde_json::Value::String("successful".into()));
            obj.insert("point".into(), serde_json::Value::String(result.point));
            // Echo the value the requester asked for, in engineering units (§4.2): the module
            // reports back the wire value it encoded, and `ot-parameter-state`'s optimistic
            // twin update must stay in the units the twin displays.
            if let Some(v) = requested.or(result.value) {
                obj.insert("value".into(), v);
            }
            if let Some(r) = result.raw {
                obj.insert("raw".into(), serde_json::Value::String(r));
            }
            let payload = with_origin(serde_json::Value::Object(obj), origin);
            publish_retained(client, topic, payload.to_string()).await?;
        }
        Err(e) => {
            publish_retained(
                client,
                topic,
                with_origin(
                    serde_json::json!({
                        "status": "failed",
                        "point": point,
                        "reason": e.to_string()
                    }),
                    origin,
                )
                .to_string(),
            )
            .await?;
        }
    }
    debug!(%device, %verb, "command handled");
    Ok(false)
}

/// Advertise the runtime-provided `write-batch` verb for every module that implements
/// `write` (the runtime executes the batch as a sequence of `write` calls).
/// What the connector actually answers: the module's own capabilities plus the verbs the
/// runtime implements on its behalf (the management verbs, and `write-batch` over `write`).
///
/// Anything that has to say what a connector does WITHOUT running it — `tedge-dot manifest`,
/// which lists a device's command types — must ask this rather than the module, or it will
/// advertise a shorter list than the running service does.
pub fn effective_capabilities(caps: &Capabilities) -> Capabilities {
    let mut caps = caps.clone();
    augment_management_caps(&mut caps);
    augment_batch_caps(&mut caps);
    caps
}

fn augment_batch_caps(caps: &mut Capabilities) {
    if caps.command_verbs.iter().any(|v| v == "write")
        && !caps.command_verbs.iter().any(|v| v == "write-batch")
    {
        caps.command_verbs.push("write-batch".to_string());
    }
}

/// One entry of a `write-batch` request.
#[derive(Clone, Debug, PartialEq)]
pub struct BatchWrite {
    pub point: String,
    pub value: Option<serde_json::Value>,
    pub raw: Option<String>,
}

/// Parse the `writes` array of a `write-batch` request (§6.4). Each entry needs a `point`
/// and either a `value` (typed write) or `raw` (hex bytes); an empty batch is rejected so a
/// malformed request cannot "succeed" without touching the device.
pub fn parse_batch_writes(json: &serde_json::Value) -> Result<Vec<BatchWrite>, String> {
    let writes = json
        .get("writes")
        .and_then(|w| w.as_array())
        .ok_or_else(|| "write-batch request needs a `writes` array".to_string())?;
    if writes.is_empty() {
        return Err("write-batch request has no writes".into());
    }
    let mut out = Vec::with_capacity(writes.len());
    for (i, w) in writes.iter().enumerate() {
        let point = w
            .get("point")
            .and_then(|p| p.as_str())
            .filter(|p| !p.is_empty())
            .ok_or_else(|| format!("writes[{i}] has no `point`"))?
            .to_string();
        let value = w.get("value").cloned().filter(|v| !v.is_null());
        let raw = w.get("raw").and_then(|r| r.as_str()).map(String::from);
        if value.is_none() && raw.is_none() {
            return Err(format!("writes[{i}] ({point}) has neither `value` nor `raw`"));
        }
        out.push(BatchWrite { point, value, raw });
    }
    Ok(out)
}

/// Execute a `write-batch`: the writes run sequentially in request order through the
/// module's `write` verb and stop at the first failure (later points are left untouched).
/// The result carries one entry per attempted write so a requester can tell what was
/// applied before a failure.
#[allow(clippy::too_many_arguments)]
async fn handle_write_batch(
    connector: &mut Box<dyn Connector>,
    client: &Mqtt,
    topic: &str,
    device: &str,
    config: &ConnectorConfig,
    json: &serde_json::Value,
    limits: Limits,
) -> Result<(), BoxError> {
    let origin = json.get("origin");
    let writes = match parse_batch_writes(json) {
        Ok(w) => w,
        Err(reason) => {
            publish_retained(
                client,
                topic,
                with_origin(
                    serde_json::json!({ "status": "failed", "reason": reason, "results": [] }),
                    origin,
                )
                .to_string(),
            )
            .await?;
            return Ok(());
        }
    };
    // Every entry is validated — `range` (§5.3) and then the transform inversion (§4.2) —
    // BEFORE the first write is executed, so an out-of-range value fails the batch with
    // nothing applied. That makes a rejected batch the one failure mode guaranteed to have
    // left the device untouched, which is what lets an operator retry it safely.
    let mut wire_values: Vec<Option<serde_json::Value>> = Vec::with_capacity(writes.len());
    for w in &writes {
        // A point this device does not define fails the batch here, not on the wire: the
        // all-or-nothing guarantee has to cover a typo as much as an out-of-range value.
        if configured_point(config, device, &w.point).is_none() {
            let reason = format!("write to {} rejected: unknown point", w.point);
            publish_retained(
                client,
                topic,
                with_origin(
                    serde_json::json!({ "status": "failed", "reason": reason, "results": [] }),
                    origin,
                )
                .to_string(),
            )
            .await?;
            return Ok(());
        }
        match (&w.value, configured_point(config, device, &w.point)) {
            (Some(value), Some(configured)) => match wire_value(configured, value) {
                Ok(wire) => wire_values.push(Some(wire)),
                Err(reason) => {
                    let reason = format!("write to {} rejected: {reason}", w.point);
                    publish_retained(
                        client,
                        topic,
                        with_origin(
                            serde_json::json!({
                                "status": "failed",
                                "reason": reason,
                                "results": [],
                            }),
                            origin,
                        )
                        .to_string(),
                    )
                    .await?;
                    return Ok(());
                }
            },
            _ => wire_values.push(w.value.clone()),
        }
    }

    let points: Vec<&str> = writes.iter().map(|w| w.point.as_str()).collect();
    publish_retained(
        client,
        topic,
        with_origin(
            serde_json::json!({ "status": "executing", "points": points }),
            origin,
        )
        .to_string(),
    )
    .await?;

    let mut results: Vec<serde_json::Value> = Vec::with_capacity(writes.len());
    let mut failure: Option<String> = None;
    for (w, wire) in writes.iter().zip(wire_values) {
        let request = CommandRequest {
            point: w.point.clone(),
            value: wire,
            value_repr: None,
            raw: w.raw.clone(),
        };
        match bounded(
            limits,
            "write",
            connector.execute(&device.to_string(), "write", &request),
        )
        .await
        {
            Ok(result) => {
                let mut obj = serde_json::Map::new();
                obj.insert("point".into(), serde_json::Value::String(result.point));
                obj.insert("status".into(), serde_json::Value::String("successful".into()));
                // The engineering value the requester asked for, not the wire value (§4.2).
                if let Some(v) = w.value.clone().or(result.value) {
                    obj.insert("value".into(), v);
                }
                if let Some(r) = result.raw {
                    obj.insert("raw".into(), serde_json::Value::String(r));
                }
                results.push(serde_json::Value::Object(obj));
            }
            Err(e) => {
                let reason = format!("write to {} failed: {e}", w.point);
                results.push(serde_json::json!({
                    "point": w.point,
                    "status": "failed",
                    "reason": reason,
                }));
                failure = Some(reason);
                break;
            }
        }
    }
    let payload = with_origin(batch_result(failure, results), origin);
    publish_retained(client, topic, payload.to_string()).await?;
    Ok(())
}

/// Echo the request's `origin` (§6.4) into a transition the connector publishes for that
/// command.
///
/// The command topic is retained and holds exactly ONE message, so `executing` and then the
/// result overwrite the request that carried `origin` — a consumer that starts (or restarts)
/// afterwards replays the terminal state alone. Carrying the correlation data forward is what
/// lets it still tell which parameter set an acknowledged write belongs to, rather than
/// guessing the default one and retaining a fragment under a name no definition matches.
fn with_origin(
    mut payload: serde_json::Value,
    origin: Option<&serde_json::Value>,
) -> serde_json::Value {
    if let (Some(origin), Some(obj)) = (origin, payload.as_object_mut()) {
        obj.insert("origin".into(), origin.clone());
    }
    payload
}

/// Shape the terminal `write-batch` envelope: `successful` with every result, or `failed`
/// with the first failure's reason and the results up to and including it.
pub fn batch_result(failure: Option<String>, results: Vec<serde_json::Value>) -> serde_json::Value {
    match failure {
        None => serde_json::json!({ "status": "successful", "results": results }),
        Some(reason) => serde_json::json!({
            "status": "failed",
            "reason": reason,
            "results": results,
        }),
    }
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

/// The retained capability descriptor payload (§7): the module's declared capabilities plus
/// the verbs the runtime adds. A property of the build alone — what the configuration says
/// about a device is on its manifest (§8.2) — so it is published once and never rebuilt.
fn capability_payload(caps: &Capabilities) -> String {
    caps.to_json().to_string()
}

/// Handle a management command: patch the config document, validate, persist, and live-reload.
/// Returns `Ok(true)` when the configuration changed (so the caller rebuilds the schedule).
#[allow(clippy::too_many_arguments)]
async fn handle_management(
    connector: &mut Box<dyn Connector>,
    client: &Mqtt,
    links: &mut LinkTracker,
    config: &mut ConnectorConfig,
    config_doc: &mut DocumentMut,
    config_path: &Path,
    topic: &str,
    verb: &str,
    json: &serde_json::Value,
    limits: Limits,
) -> Result<bool, BoxError> {
    // Every transition echoes the request's `origin` (§6.4): a bridge completing the command on
    // the entity it was issued for reads it back from the retained result, since the service
    // topic does not name that entity.
    let origin = json.get("origin");
    publish_retained(
        client,
        topic,
        with_origin(serde_json::json!({ "status": "executing" }), origin).to_string(),
    )
    .await?;

    // Build a candidate document and validate it parses into a typed config.
    let candidate = {
        let mut doc = config_doc.clone();
        match apply_management(verb, json, &mut doc) {
            Ok(()) => doc,
            Err(e) => {
                publish_failed(client, topic, &e, origin).await?;
                return Ok(false);
            }
        }
    };
    // Resolve the candidate the same way the loader does, so a device that only references
    // point libraries (§3.4) is validated with its points expanded. The document itself keeps
    // the `points_from` reference: persisting it must never bake a library's points into the
    // user's file.
    let candidate_text = candidate.to_string();
    // A command may name a point library, never a path: see `reject_path_references`. Judged
    // against the document as it stood, so only a reference the command itself introduced is
    // refused — and refused before the resolver runs, so the path is never opened and no
    // filesystem detail reaches the command result.
    if let Err(e) = crate::library::reject_path_references(&config_doc.to_string(), &candidate_text)
    {
        publish_failed(client, topic, &e, origin).await?;
        return Ok(false);
    }
    let new_config: ConnectorConfig = match crate::library::resolve(
        &candidate_text,
        crate::library::config_base_dir(config_path),
    ) {
        Ok(c) => c,
        Err(e) => {
            publish_failed(
                client,
                topic,
                &format!("resulting config is invalid: {e}"),
                origin,
            )
            .await?;
            return Ok(false);
        }
    };

    // Validate against the protocol module before committing.
    if let Err(e) = connector.configure(&new_config) {
        let _ = connector.configure(config); // restore previous good state
        publish_failed(client, topic, &format!("configure failed: {e}"), origin).await?;
        return Ok(false);
    }

    // Persist the new document (best effort: the running state is already updated).
    if let Err(e) = persist_config(config_path, &candidate) {
        warn!("failed to persist config to {}: {e}", config_path.display());
    }
    *config_doc = candidate;
    commit_config(connector, client, links, config, new_config, limits).await?;

    publish_retained(
        client,
        topic,
        with_origin(serde_json::json!({ "status": "successful" }), origin).to_string(),
    )
    .await?;
    info!(%verb, "management command applied");
    Ok(true)
}

/// Install a configuration the protocol module has already accepted (`configure` succeeded):
/// replace the running one, republish the capability descriptor, and reconnect every device with
/// it, republishing their link status. Shared by management commands and reloads.
async fn commit_config(
    connector: &mut Box<dyn Connector>,
    client: &Mqtt,
    links: &mut LinkTracker,
    config: &mut ConnectorConfig,
    new_config: ConnectorConfig,
    limits: Limits,
) -> Result<(), BoxError> {
    // Devices the new configuration no longer defines — removed, or switched off with
    // `enabled = false` — lose their retained manifest and link status (§8.2): nothing retained
    // may describe a device that is gone.
    let gone: Vec<String> = config
        .devices
        .iter()
        .filter(|d| !new_config.devices.iter().any(|n| n.name == d.name))
        .map(|d| d.name.clone())
        .collect();
    *config = new_config;
    links.clear_devices(client, &gone).await?;
    // The manifests describe the configuration, which just changed: republish every device's,
    // so a retained manifest never describes the configuration as it was at startup — points
    // that are gone, none for a device just defined.
    links.publish_manifests(client, config).await?;

    // Reconnect with the new configuration and republish link status.
    let _ = bounded(limits, "disconnect", connector.disconnect()).await;
    match bounded(limits, "connect", connector.connect()).await {
        Ok(reports) => links.publish_reports(client, &reports, config).await?,
        Err(e) => warn!("reconnect after reconfigure failed: {e}"),
    }
    Ok(())
}

/// What a reload ([`run_until_reloadable`]) did.
#[derive(Debug, PartialEq, Eq)]
enum Reloaded {
    /// The file resolves to the configuration already running: nothing was touched.
    Unchanged,
    /// The file could not be used; the running configuration was kept.
    Kept,
    /// The new configuration was applied in place.
    Applied,
    /// The change needs the connector restarted with it (see [`needs_restart`]).
    Restart,
}

/// A change the running connector cannot adopt in place: its MQTT client id, last will and
/// command subscriptions are named after the service and the protocol, the protocol selects the
/// module, the client is connected to one broker, and the host's stall watchdog takes its limit
/// when the connector starts — the effective one ([`ConnectorConfig::stall_limit`]), so a new
/// `operation_timeout` that raises it counts and a respelt `stall_timeout` does not. The C
/// runtime draws the same line (`needs_restart` in runtime.c).
fn needs_restart(running: &ConnectorConfig, new: &ConnectorConfig) -> bool {
    running.connector.protocol != new.connector.protocol
        || running.connector.service_name() != new.connector.service_name()
        || running.mqtt != new.mqtt
        || running.stall_limit() != new.stall_limit()
}

/// Re-read the connector's config file and apply what changed, keeping the running configuration
/// when the file cannot be used. A file that resolves to the running configuration — point
/// libraries included — leaves everything untouched, so a reload meant for another connector's
/// file does not reconnect this one's devices.
#[allow(clippy::too_many_arguments)]
async fn reload_from_file(
    connector: &mut Box<dyn Connector>,
    client: &Mqtt,
    links: &mut LinkTracker,
    config: &mut ConnectorConfig,
    config_doc: &mut DocumentMut,
    config_path: &Path,
    limits: Limits,
) -> Reloaded {
    let path = config_path.display();
    let loaded = std::fs::read_to_string(config_path)
        .map_err(|e| format!("cannot read {path}: {e}"))
        .and_then(|text| {
            let doc = text
                .parse::<DocumentMut>()
                .map_err(|e| format!("{path}: {e}"))?;
            let base_dir = crate::library::config_base_dir(config_path);
            let new_config =
                crate::library::resolve(&text, base_dir).map_err(|e| format!("{path}: {e}"))?;
            Ok((doc, new_config))
        });
    let (doc, new_config) = match loaded {
        Ok(loaded) => loaded,
        Err(e) => {
            error!("reload: {e}; keeping the running configuration");
            return Reloaded::Kept;
        }
    };
    if needs_restart(config, &new_config) {
        info!(
            "reload: {path} changes the service name, protocol, broker or stall timeout; \
             restarting the connector"
        );
        return Reloaded::Restart;
    }
    if new_config == *config {
        // The document is still taken: a later management command patches and persists it, and
        // must start from the file as it is now (its comments, say), not as it was loaded.
        *config_doc = doc;
        info!("reload: {path} is unchanged");
        return Reloaded::Unchanged;
    }
    if let Err(e) = connector.configure(&new_config) {
        let _ = connector.configure(config); // restore the running configuration
        error!("reload: {path}: configure failed: {e}; keeping the running configuration");
        return Reloaded::Kept;
    }
    *config_doc = doc;
    if let Err(e) =
        commit_config(connector, client, links, config, new_config, limits).await
    {
        warn!("reload: {path}: {e}");
    }
    info!("reload: applied {path}");
    Reloaded::Applied
}

async fn publish_failed(
    client: &Mqtt,
    topic: &str,
    reason: &str,
    origin: Option<&serde_json::Value>,
) -> Result<(), BoxError> {
    warn!("management command failed: {reason}");
    let failed = serde_json::json!({ "status": "failed", "reason": reason });
    publish_retained(client, topic, with_origin(failed, origin).to_string()).await
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
    // The service name addresses this connector's management commands (§6.3) and the protocol
    // selects its module: a running instance cannot take either from a command.
    if target == "connector" {
        if let Some(key) = ["service_name", "protocol"]
            .into_iter()
            .find(|key| patch.contains_key(*key))
        {
            return Err(format!(
                "set-config cannot change connector.{key}: edit the configuration file and \
                 restart the connector"
            ));
        }
    }
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
    client: &Mqtt,
    protocol: &str,
    reports: &[LinkReport],
    config: &ConnectorConfig,
) -> Result<(), BoxError> {
    for report in reports {
        let topic = format!("te/device/{}/ot/{}/status/link", report.device, protocol);
        let payload = link_payload(report, config, OffsetDateTime::now_utc());
        publish_retained(client, &topic, payload.to_string()).await?;
    }
    Ok(())
}

/// The retained link-status payload (§8). Pure, and the device `type` is looked up in the
/// configuration it is given, so it always describes the configuration the connector is
/// running right now — including one a management command (§6.3) has just installed.
fn link_payload(
    report: &LinkReport,
    config: &ConnectorConfig,
    now: OffsetDateTime,
) -> serde_json::Value {
    let mut obj = serde_json::Map::new();
    obj.insert(
        "status".into(),
        serde_json::Value::String(report.status.as_str().into()),
    );
    // The device's declared type (§3.1), so the registration flow can use it as the thin-edge
    // entity type and a consumer can name the device's parameter sets (§5.2) before any sample.
    if let Some(device_type) = device_type_of(config, &report.device) {
        obj.insert(
            "type".into(),
            serde_json::Value::String(device_type.to_string()),
        );
    }
    if report.status == LinkStatus::Connected {
        obj.insert("since".into(), serde_json::Value::String(format_rfc3339_ms(now)));
    }
    if let Some(reason) = &report.reason {
        obj.insert("reason".into(), serde_json::Value::String(reason.clone()));
    }
    // The device descriptor (`info`) is on the manifest (§8.2), not here.
    serde_json::Value::Object(obj)
}

/// Restore what a clean MQTT session loses when the broker drops the connection: the command
/// subscriptions, and the retained service health, capability descriptor and link statuses.
async fn restore_mqtt_session(
    client: &Mqtt,
    subscriptions: &[&str],
    health_topic: &str,
    cap_topic: &str,
    caps: &Capabilities,
    config: &ConnectorConfig,
    links: &LinkTracker,
) -> Result<(), BoxError> {
    for filter in subscriptions {
        client.subscribe(*filter, QoS::AtLeastOnce).await?;
    }
    publish_health(client, health_topic, "up").await?;
    publish_retained(client, cap_topic, capability_payload(caps)).await?;
    links.publish_manifests(client, config).await?;
    links.republish(client, config).await
}

async fn publish_health(client: &Mqtt, topic: &str, status: &str) -> Result<(), BoxError> {
    let payload = serde_json::json!({
        "status": status,
        "time": format_rfc3339_ms(OffsetDateTime::now_utc())
    })
    .to_string();
    publish_retained(client, topic, payload).await
}

/// The MQTT client, and whether its broker is connected right now (kept up to date by the event
/// loop task in [`run_until_reloadable`]). It dereferences to the client, so publishing and
/// subscribing read as usual; what it adds is the connection state that decides how a publish
/// may wait.
struct Mqtt {
    client: AsyncClient,
    online: tokio::sync::watch::Receiver<bool>,
}

impl Mqtt {
    fn is_online(&self) -> bool {
        *self.online.borrow()
    }
}

impl std::ops::Deref for Mqtt {
    type Target = AsyncClient;

    fn deref(&self) -> &AsyncClient {
        &self.client
    }
}

/// Publish a retained state message: health, capability descriptor, link status, command status.
///
/// Every one of them goes through here, so none can hold up the main loop. While the broker is
/// unreachable rumqttc does not read its request queue, and an awaited publish would wait for the
/// broker once the queue is full — which one reload of a connector with more devices than the
/// queue has slots is enough for. So, offline, the message is queued if there is room and dropped
/// if not: reconnecting republishes the health, the capability descriptor and every link status
/// (`restore_mqtt_session`). Online, the wait is bounded (see [`PUBLISH_TIMEOUT`]).
async fn publish_retained(client: &Mqtt, topic: &str, payload: String) -> Result<(), BoxError> {
    if !client.is_online() {
        if let Err(e) = client.try_publish(topic, QoS::AtLeastOnce, true, payload) {
            debug!("not publishing {topic} while the MQTT broker is unreachable: {e}");
        }
        return Ok(());
    }
    let publish = client.publish(topic, QoS::AtLeastOnce, true, payload);
    match tokio::time::timeout(PUBLISH_TIMEOUT, publish).await {
        Ok(result) => result.map_err(|e| Box::new(e) as BoxError),
        Err(_) => {
            warn!(
                "publishing {topic} took longer than {}s; the MQTT connection looks lost",
                PUBLISH_TIMEOUT.as_secs()
            );
            Ok(())
        }
    }
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

    /// `define-device` with only connection information and a library reference is what a
    /// discovery script (mDNS and friends) publishes for an instance of a known device type.
    /// The persisted document must keep the reference: baking the library's points into the
    /// user's file would undo the decoupling on the first management command.
    #[test]
    fn define_device_persists_a_point_library_reference_not_its_points() {
        let dir = std::env::temp_dir().join(format!("tdot-rt-library-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("points.d/modbus")).unwrap();
        std::fs::write(
            dir.join("points.d/modbus/acme-meter.toml"),
            "[library]\nprotocol = \"modbus\"\n\n[[point]]\nid = \"from_library\"\n\
             datatype = \"uint16\"\naddress = { table = \"holding\", address = 9, count = 1 }\n",
        )
        .unwrap();

        let mut d = BASE
            .replace(
                "[connector]",
                &format!(
                    "[connector]\npoint_library_path = [\"{}\"]",
                    dir.join("points.d").display()
                ),
            )
            .parse::<DocumentMut>()
            .unwrap();
        apply_management(
            "define-device",
            &serde_json::json!({
                "device": {
                    "name": "plc-2",
                    "protocol_address": { "transport": "tcp", "host": "10.0.0.2", "port": 502, "unit_id": 1 },
                    "points_from": ["acme-meter"],
                }
            }),
            &mut d,
        )
        .expect("apply ok");

        let text = d.to_string();
        assert!(text.contains(r#"points_from = ["acme-meter"]"#), "reference persisted: {text}");
        assert!(
            !text.contains("from_library"),
            "the library's points must NOT be written into the config: {text}"
        );

        // ...and resolving that same document is what the runtime hands the protocol module.
        let cfg = crate::library::resolve(&text, &dir).expect("resolves");
        let plc2 = cfg.devices.iter().find(|d| d.name == "plc-2").expect("plc-2");
        assert_eq!(plc2.points.len(), 1);
        assert_eq!(plc2.points[0].id, "from_library");

        let _ = std::fs::remove_dir_all(&dir);
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

    /// The service name addresses the connector's management commands and the protocol selects
    /// its module, so neither can be changed by a command while the connector runs.
    #[test]
    fn set_config_cannot_change_the_connector_identity() {
        for key in ["service_name", "protocol"] {
            let mut config = serde_json::Map::new();
            config.insert(key.to_string(), "other".into());
            let request = serde_json::json!({ "target": "connector", "config": config });
            let mut d = doc();
            let err = apply_management("set-config", &request, &mut d).unwrap_err();
            assert!(err.contains(key), "{err}");
            assert_eq!(d.to_string(), BASE, "the document must be left untouched");
        }
    }

    /// A reload applies in place what the running connector can adopt, and restarts it only for
    /// what it cannot: its MQTT identity (service name, protocol, broker) and the stall timeout
    /// the host's watchdog took when it started.
    #[test]
    fn reload_restarts_only_for_what_cannot_change_in_place() {
        let running: ConnectorConfig = toml::from_str(BASE).unwrap();
        let edited = |from: &str, to: &str| -> ConnectorConfig {
            assert!(BASE.contains(from), "fixture lacks {from}");
            toml::from_str(&BASE.replace(from, to)).unwrap()
        };
        assert!(!needs_restart(&running, &running.clone()));
        assert!(!needs_restart(&running, &edited("poll_interval = \"2s\"", "poll_interval = \"9s\"")));
        assert!(!needs_restart(&running, &edited("address = 7", "address = 8")));
        assert!(!needs_restart(&running, &edited("log_level = \"info\"", "log_level = \"debug\"")));

        let service = edited("protocol = \"modbus\"", "protocol = \"modbus\"\nservice_name = \"plant\"");
        assert!(needs_restart(&running, &service));
        assert!(needs_restart(&running, &edited("protocol = \"modbus\"", "protocol = \"opcua\"")));
        assert!(needs_restart(&running, &edited("port = 1883", "port = 1884")));
        let stall = edited("log_level = \"info\"", "log_level = \"info\"\nstall_timeout = \"5m\"");
        assert!(needs_restart(&running, &stall));
    }

    /// "Unchanged" is judged on the resolved configuration, so reloading after an edit that does
    /// not change what the connector does — a comment, say — touches nothing, while any real
    /// change does not compare equal.
    #[test]
    fn a_reload_sees_through_edits_that_change_nothing() {
        let running: ConnectorConfig = toml::from_str(BASE).unwrap();
        let commented: ConnectorConfig =
            toml::from_str(&format!("# edited by the operator\n{BASE}")).unwrap();
        assert_eq!(running, commented);
        let moved: ConnectorConfig =
            toml::from_str(&BASE.replace("address = 7", "address = 8")).unwrap();
        assert_ne!(running, moved);
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

    fn route<'a>(topic: &'a str, config: &ConnectorConfig) -> CommandRoute<'a> {
        route_command(topic, "tedge-dot", config)
    }

    /// Every instance receives every device command on the thin-edge topics, and each must act
    /// only on the devices its own configuration defines — plus the management commands
    /// addressed to its own service. Everything else is another instance's to answer.
    #[test]
    fn commands_route_to_the_owning_instance_only() {
        let config: ConnectorConfig = toml::from_str(BASE).unwrap();
        assert_eq!(
            route("te/device/plc-1///cmd/ot_write/1", &config),
            CommandRoute::Device { device: "plc-1", verb: "write" }
        );
        assert_eq!(
            route("te/device/plc-1///cmd/ot_write_batch/1", &config),
            CommandRoute::Device { device: "plc-1", verb: "write-batch" }
        );
        assert_eq!(
            route("te/device/main/service/tedge-dot/cmd/ot_define_device/1", &config),
            CommandRoute::Service { verb: "define-device" }
        );
        // A management verb also arrives on a DEVICE topic — that is where the c8y mapper puts
        // a Cloud Fieldbus operation, because it belongs to the gateway. Whose it is comes from
        // the payload's `service`, not the topic, so routing alone accepts it.
        assert_eq!(
            route("te/device/main///cmd/ot_set_config/1", &config),
            CommandRoute::Device { device: "main", verb: "set-config" },
            "the gateway is nobody's configured device: payload.service is the claim"
        );
        for elsewhere in [
            // a device another instance owns
            "te/device/plc-2///cmd/ot_write/1",
            // another instance's service
            "te/device/main/service/tedge-dot-2/cmd/ot_define_device/1",
            // a command type this connector does not answer (no alias declares it)
            "te/device/plc-1///cmd/ot_write_coil/1",
            "te/device/plc-1///cmd/firmware_update/1",
            // the 0.1 connector-side topics are gone
            "te/device/plc-1/ot/modbus/cmd/write/1",
            // not a command topic, or not exactly one
            "te/device/plc-1///cmd/ot_write",
            "te/device/plc-1///cmd/ot_write/1/2",
            "te/device/plc-1/ot/modbus/sample/temp",
            "te/device/main/service/tedge-dot/ot/capabilities",
        ] {
            assert_eq!(route(elsewhere, &config), CommandRoute::Elsewhere, "{elsewhere}");
        }
    }

    /// An alias (§6.6) is another name for a verb the connector already answers — it exists
    /// because the c8y mapper needs one command type per operation, which is a cloud
    /// constraint, not a protocol one.
    #[test]
    fn a_command_alias_routes_to_the_verb_it_stands_for() {
        let mut config: ConnectorConfig = toml::from_str(BASE).unwrap();
        config
            .connector
            .command_aliases
            .insert("ot_write_coil".into(), "ot_write".into());
        assert_eq!(
            route("te/device/plc-1///cmd/ot_write_coil/1", &config),
            CommandRoute::Device { device: "plc-1", verb: "write" }
        );
    }

    /// Ownership is by device AND point (§6.5): with the protocol gone from the command topic,
    /// the device alone no longer says whose command it is.
    #[test]
    fn ownership_needs_every_point_of_the_request() {
        let config: ConnectorConfig = toml::from_str(BASE).unwrap();
        let point = &config.devices[0].points[0].id.clone();
        assert!(owns_request(&config, "plc-1", &serde_json::json!({ "point": point })));
        assert!(!owns_request(&config, "plc-1", &serde_json::json!({ "point": "nope" })));
        assert!(!owns_request(&config, "plc-2", &serde_json::json!({ "point": point })));
        // A batch is answered only when EVERY point is this instance's: a partial answer would
        // apply some writes and fail the rest.
        assert!(owns_request(
            &config,
            "plc-1",
            &serde_json::json!({ "writes": [{ "point": point }] })
        ));
        // At least one, not all: a batch mixing a known point with a typo is still this
        // instance's to answer — and `handle_write_batch` fails it before the first write, so
        // the operator gets a reason instead of a command that hangs at `init` for ever.
        assert!(owns_request(
            &config,
            "plc-1",
            &serde_json::json!({ "writes": [{ "point": point }, { "point": "nope" }] })
        ));
        assert!(!owns_request(
            &config,
            "plc-1",
            &serde_json::json!({ "writes": [{ "point": "nope" }] })
        ));
        // A malformed batch touches nothing; the device's owner says so rather than nobody.
        assert!(owns_request(&config, "plc-1", &serde_json::json!({ "writes": [] })));
        // A request naming no point (a management verb) is claimed by `service` instead.
        assert!(owns_request(&config, "plc-1", &serde_json::json!({ "target": "connector" })));
    }

    /// Ownership is the live configuration: a device added by define-device routes to this
    /// instance from then on, and a removed one no longer does.
    #[test]
    fn command_routing_follows_the_configuration() {
        let topic = "te/device/plc-9///cmd/ot_write/1";
        let config: ConnectorConfig = toml::from_str(BASE).unwrap();
        assert_eq!(route(topic, &config), CommandRoute::Elsewhere);

        let (_, added) = apply(
            "define-device",
            serde_json::json!({ "device": {
                "name": "plc-9",
                "protocol_address": { "transport": "tcp", "host": "10.0.0.9", "port": 502, "unit_id": 1 },
                "point": [{ "id": "t", "datatype": "uint16",
                            "address": { "table": "holding", "address": 1, "count": 1 } }]
            }}),
        );
        assert_eq!(route(topic, &added), CommandRoute::Device { device: "plc-9", verb: "write" });

        let (_, removed) = apply("remove-device", serde_json::json!({ "device": "plc-1" }));
        let topic = "te/device/plc-1/ot/modbus/cmd/write/1";
        assert_eq!(route(topic, &removed), CommandRoute::Elsewhere);
    }

    #[test]
    fn define_device_appends_new_device() {
        let (_d, cfg) = apply(
            "define-device",
            serde_json::json!({ "device": {
                "name": "plc-9",
                "protocol_address": { "transport": "tcp", "host": "10.0.0.9", "port": 502, "unit_id": 2 },
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

    /// A subscribed point is deliberately OFF the polling schedule -- that is what makes push
    /// delivery push. The corollary is that anything which drops a point from `subscribed`
    /// MUST rebuild the schedule, or the point is delivered by nobody: not polled, and not
    /// pushed either. `subscribe_device` relies on this when it clears a device's entries
    /// before re-arming, so that a failed re-subscribe degrades to polling rather than to
    /// silence.
    #[test]
    fn a_point_is_scheduled_unless_it_is_subscribed() {
        let config: ConnectorConfig = toml::from_str(BASE).unwrap();

        let none = HashSet::new();
        let polled = build_schedule(&config, &none);
        assert_eq!(polled.len(), 1, "an unsubscribed point must be polled");
        assert_eq!(polled[0].point.id, "temp");

        let mut subscribed = HashSet::new();
        subscribed.insert((0usize, "temp".to_string()));
        assert!(
            build_schedule(&config, &subscribed).is_empty(),
            "a subscribed point must not also be polled (it would double-publish)"
        );

        // ...and dropping it from the set puts it straight back on the schedule.
        subscribed.remove(&(0usize, "temp".to_string()));
        assert_eq!(
            build_schedule(&config, &subscribed).len(),
            1,
            "a point that lost its subscription must fall back to polling"
        );
    }

    #[test]
    fn point_meta_is_parsed_and_kept_on_the_point() {
        let cfg: ConnectorConfig = toml::from_str(
            r#"
[connector]
protocol = "modbus"

[[device]]
name = "plc-1"
type = "acme-meter-v2"
protocol_address = { host = "127.0.0.1" }

  [[device.point]]
  id = "temp"
  datatype = "float32"
  address = { table = "holding", address = 7, count = 2 }
  meta = { on_change = true, min_interval = "5s", room = "boiler" }
"#,
        )
        .unwrap();
        // `meta` is parsed and kept on the point — the manifest publishes it (§8.2); a sample
        // does not (§5).
        let meta = cfg.devices[0].points[0].meta.as_ref().unwrap();
        assert_eq!(meta["on_change"], serde_json::json!(true));
        assert_eq!(meta["min_interval"], serde_json::json!("5s"));
        assert_eq!(meta["room"], serde_json::json!("boiler"));
        assert_eq!(device_type_of(&cfg, "plc-1"), Some("acme-meter-v2"));
        assert_eq!(device_type_of(&cfg, "nope"), None);
    }

    /// The retained link status carries the device type from the configuration the connector
    /// is running *now*. The regression this pins: the type used to be cached when the runtime
    /// started, so a `define-device` that added a typed device published its link status with
    /// no type at all — and that registration is retained, so the child device stayed a
    /// generic `<protocol>-device` for the mapper's lifetime.
    #[test]
    fn link_payload_carries_the_device_type_of_the_live_config() {
        const BASE: &str = r#"
[connector]
protocol = "modbus"

[[device]]
name = "plc-1"
type = "acme-meter-v2"
protocol_address = { host = "127.0.0.1" }

  [[device.point]]
  id = "temp"
  datatype = "float32"
  address = { table = "holding", address = 7, count = 2 }
"#;
        let mut config: ConnectorConfig = toml::from_str(BASE).unwrap();
        let report = LinkReport::new("plc-1".to_string(), LinkStatus::Connected, None);
        let now = OffsetDateTime::UNIX_EPOCH;

        let payload = link_payload(&report, &config, now);
        assert_eq!(payload["status"], serde_json::json!("connected"));
        assert_eq!(payload["type"], serde_json::json!("acme-meter-v2"));
        assert!(payload["since"].is_string());

        // A device the configuration does not (yet) know, and one that declares no type.
        let unknown = LinkReport::new("plc-9".to_string(), LinkStatus::Connected, None);
        assert!(link_payload(&unknown, &config, now).get("type").is_none());
        config.devices[0].device_type = None;
        assert!(link_payload(&report, &config, now).get("type").is_none());

        // What `define-device` does: the type of the newly configured device is published the
        // moment the reload republishes the link status, not on the next restart.
        let added: ConnectorConfig = toml::from_str(
            r#"
[connector]
protocol = "modbus"

[[device]]
name = "plc-7"
type = "acme-boiler-v2"
protocol_address = { host = "127.0.0.1" }

  [[device.point]]
  id = "temp"
  datatype = "float32"
  address = { table = "holding", address = 7, count = 2 }
"#,
        )
        .unwrap();
        let new_device = LinkReport::new("plc-7".to_string(), LinkStatus::Connected, None);
        assert_eq!(
            link_payload(&new_device, &added, now)["type"],
            serde_json::json!("acme-boiler-v2")
        );
    }

    #[test]
    fn envelope_echoes_nothing_static_per_point() {
        let sample = Sample {
            ts: OffsetDateTime::UNIX_EPOCH,
            device: "plc-1".into(),
            protocol: "modbus",
            point: "temp".into(),
            datatype: crate::model::DataType::Uint16,
            value: None,
            raw: vec![0x12, 0x34],
            raw_group: 2,
            quality: crate::model::Quality::Good,
            unit: None,
            addr: serde_json::Value::Null,
            seq: None,
            error: None,
        };
        // Nothing static per point is echoed any more: `meta`, `access`, `unit` and the
        // device `type` are on the device manifest (§8.2), published once.
        let env = sample.to_envelope(false);
        for gone in ["meta", "access", "type", "unit", "value_repr", "ts_ms", "raw", "addr"] {
            assert!(env.get(gone).is_none(), "{gone} must not be echoed per sample");
        }
        // `sample_debug` puts back the wire, and only the wire.
        let debug = sample.to_envelope(true);
        assert_eq!(debug["raw"], serde_json::json!("1234"));
        assert!(debug.get("addr").is_some());
        assert!(debug.get("meta").is_none());
    }

    /// §4.2 — the write path is in engineering units. These pin the four rules: invert, leave
    /// an untransformed point alone, round to nearest for an integer datatype, and refuse a
    /// value the datatype cannot hold once inverted.
    #[test]
    fn wire_value_inverts_the_transform() {
        let cfg: ConnectorConfig = toml::from_str(
            r#"
[connector]
protocol = "modbus"

[[device]]
name = "plc-1"
protocol_address = { host = "127.0.0.1" }

  [[device.point]]
  id = "temp_scaled"
  datatype = "uint16"
  access = "read_write"
  address = { table = "holding", address = 3, count = 1 }
  transform = { decimal_shift = -3 }

  [[device.point]]
  id = "plain_u16"
  datatype = "uint16"
  access = "read_write"
  address = { table = "holding", address = 4, count = 1 }

  [[device.point]]
  id = "offset_u8"
  datatype = "uint8"
  access = "read_write"
  address = { table = "holding", address = 5, count = 1 }
  transform = { multiplier = 0.1, offset = 10.0 }

  [[device.point]]
  id = "coil"
  datatype = "bool"
  access = "read_write"
  address = { table = "coil", address = 0, count = 1 }
  transform = { decimal_shift = -3 }
"#,
        )
        .unwrap();
        let point = |id: &str| configured_point(&cfg, "plc-1", id).unwrap();

        // The RFC's example: 20 °C on a point scaled by 10^-3 is register 20000, not 20.
        assert_eq!(
            wire_value(point("temp_scaled"), &serde_json::json!(20)).unwrap(),
            serde_json::json!(20000)
        );
        // An identity transform leaves the request untouched, byte for byte.
        assert_eq!(
            wire_value(point("plain_u16"), &serde_json::json!(4242)).unwrap(),
            serde_json::json!(4242)
        );
        // An integer datatype rounds to nearest: (10.04 - 10) / 0.1 = 0.4 -> 0.
        assert_eq!(
            wire_value(point("offset_u8"), &serde_json::json!(10.04)).unwrap(),
            serde_json::json!(0)
        );
        // ...and (10.16 - 10) / 0.1 = 1.6 -> 2.
        assert_eq!(
            wire_value(point("offset_u8"), &serde_json::json!(10.16)).unwrap(),
            serde_json::json!(2)
        );
        // A value the datatype cannot hold after inversion fails instead of wrapping: uint8
        // tops out at 255, and (46 - 10) / 0.1 = 360.
        let err = wire_value(point("offset_u8"), &serde_json::json!(46)).unwrap_err();
        assert!(err.contains("does not fit uint8 after transform"), "{err}");
        // A boolean has nothing to invert, transform declared or not.
        assert_eq!(
            wire_value(point("coil"), &serde_json::json!(true)).unwrap(),
            serde_json::json!(true)
        );
    }

    /// §5.3 — `range` is enforced by the runtime, on the engineering value, before the
    /// transform is inverted and before the device is touched. The limit is a property of the
    /// signal, so a script or a typo in an operation meets the same check the cloud form shows.
    #[test]
    fn range_is_enforced_on_write() {
        let cfg: ConnectorConfig = toml::from_str(
            r#"
[connector]
protocol = "modbus"

[[device]]
name = "plc-1"
protocol_address = { host = "127.0.0.1" }

  [[device.point]]
  id = "temp_u16"
  datatype = "uint16"
  access = "read_write"
  address = { table = "holding", address = 3, count = 1 }
  range = { min = 0, max = 30000 }

  [[device.point]]
  id = "floor_only"
  datatype = "int16"
  access = "read_write"
  address = { table = "holding", address = 4, count = 1 }
  range = { min = -10 }

  [[device.point]]
  id = "scaled"
  datatype = "uint16"
  access = "read_write"
  address = { table = "holding", address = 5, count = 1 }
  transform = { decimal_shift = -3 }
  range = { min = 0, max = 30 }
"#,
        )
        .unwrap();
        let point = |id: &str| configured_point(&cfg, "plc-1", id).unwrap();

        wire_value(point("temp_u16"), &serde_json::json!(30000)).unwrap();
        let err = wire_value(point("temp_u16"), &serde_json::json!(35000)).unwrap_err();
        assert_eq!(err, "value 35000 outside range [0, 30000] of temp_u16");

        // One-sided bounds leave the other side open.
        wire_value(point("floor_only"), &serde_json::json!(9999)).unwrap();
        let err = wire_value(point("floor_only"), &serde_json::json!(-11)).unwrap_err();
        assert!(err.contains("outside range [-10, ∞)"), "{err}");

        // The bound is in ENGINEERING units, which is what §4.2 made possible: 30 is allowed
        // and becomes wire 30000; 31 is refused even though 31000 fits a uint16 perfectly.
        assert_eq!(
            wire_value(point("scaled"), &serde_json::json!(30)).unwrap(),
            serde_json::json!(30000)
        );
        let err = wire_value(point("scaled"), &serde_json::json!(31)).unwrap_err();
        assert!(err.contains("outside range [0, 30] of scaled"), "{err}");
    }

    /// §5.1 option A — the runtime applies `publish`, so every consumer sees one stream with
    /// the policy already applied instead of each flow carrying the lookup.
    #[test]
    fn the_runtime_applies_the_publish_policy() {
        let cfg: ConnectorConfig = toml::from_str(
            r#"
[connector]
protocol = "modbus"

[[device]]
name = "plc-1"
protocol_address = { host = "127.0.0.1" }

  [[device.point]]
  id = "changes"
  datatype = "uint16"
  address = { table = "holding", address = 1, count = 1 }
  publish = { on_change = true }

  [[device.point]]
  id = "band"
  datatype = "float32"
  address = { table = "holding", address = 2, count = 2 }
  publish = { deadband = 0.5 }

  [[device.point]]
  id = "limited"
  datatype = "uint16"
  address = { table = "holding", address = 4, count = 1 }
  publish = { min_interval = "10s" }

  [[device.point]]
  id = "every_read"
  datatype = "uint16"
  address = { table = "holding", address = 5, count = 1 }
"#,
        )
        .unwrap();
        let mut gate = PublishGate::new(&cfg);
        let at = |secs: i64| OffsetDateTime::UNIX_EPOCH + Duration::from_secs(secs as u64);
        let sample = |point: &str, value: f64, secs: i64| Sample {
            ts: at(secs),
            device: "plc-1".into(),
            protocol: "modbus",
            point: point.into(),
            datatype: crate::model::DataType::Uint16,
            value: Some(crate::model::Value::Number(value)),
            raw: vec![],
            raw_group: 2,
            quality: crate::model::Quality::Good,
            unit: None,
            addr: serde_json::Value::Null,
            seq: None,
            error: None,
        };

        // on_change: the repeat is suppressed, the next different value is not.
        assert!(gate.admits(&sample("changes", 42.0, 0)));
        assert!(!gate.admits(&sample("changes", 42.0, 1)));
        assert!(gate.admits(&sample("changes", 43.0, 2)));

        // deadband: a change smaller than the band is not a change.
        assert!(gate.admits(&sample("band", 100.0, 0)));
        assert!(!gate.admits(&sample("band", 100.4, 1)));
        assert!(gate.admits(&sample("band", 100.6, 2)));

        // min_interval: too soon after the last published reading, even though it changed.
        assert!(gate.admits(&sample("limited", 1.0, 0)));
        assert!(!gate.admits(&sample("limited", 2.0, 5)));
        assert!(gate.admits(&sample("limited", 3.0, 15)));

        // A point that declares no policy is published on every read — the default.
        for secs in 0..3 {
            assert!(gate.admits(&sample("every_read", 7.0, secs)));
        }

        // A `bad` sample is always published (contract §5.1): an operator must see a failing
        // read, and the runtime's own bad-sample rate limit is what bounds it.
        let mut bad = sample("changes", 42.0, 3);
        bad.quality = crate::model::Quality::Bad;
        bad.value = None;
        bad.error = Some("timeout".into());
        assert!(gate.admits(&bad));
        assert!(gate.admits(&bad), "and again: the gate never holds a bad sample back");
    }

    /// `debounce`: a new value is published only once it has stayed stable for the period, and
    /// the first sighting is the candidate rather than a reading.
    #[test]
    fn the_publish_gate_debounces() {
        let cfg: ConnectorConfig = toml::from_str(
            r#"
[connector]
protocol = "modbus"

[[device]]
name = "plc-1"
protocol_address = { host = "127.0.0.1" }

  [[device.point]]
  id = "b1"
  datatype = "uint16"
  address = { table = "holding", address = 1, count = 1 }
  publish = { debounce = "2s" }
"#,
        )
        .unwrap();
        let mut gate = PublishGate::new(&cfg);
        let sample = |value: f64, secs: u64| Sample {
            ts: OffsetDateTime::UNIX_EPOCH + Duration::from_secs(secs),
            device: "plc-1".into(),
            protocol: "modbus",
            point: "b1".into(),
            datatype: crate::model::DataType::Uint16,
            value: Some(crate::model::Value::Number(value)),
            raw: vec![],
            raw_group: 2,
            quality: crate::model::Quality::Good,
            unit: None,
            addr: serde_json::Value::Null,
            seq: None,
            error: None,
        };
        // First sighting: a candidate, not a reading.
        assert!(!gate.admits(&sample(7.0, 0)));
        // Still settling.
        assert!(!gate.admits(&sample(7.0, 1)));
        // Stable for the period: published.
        assert!(gate.admits(&sample(7.0, 3)));
        // A different value starts a new candidate.
        assert!(!gate.admits(&sample(9.0, 4)));
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
    fn reconnect_backoff_doubles_and_caps() {
        assert_eq!(next_backoff(RECONNECT_INITIAL), Duration::from_secs(2));
        assert_eq!(next_backoff(Duration::from_secs(2)), Duration::from_secs(4));
        assert_eq!(next_backoff(Duration::from_secs(40)), RECONNECT_MAX);
        assert_eq!(next_backoff(RECONNECT_MAX), RECONNECT_MAX);
    }

    #[test]
    fn link_transitions_follow_poll_health() {
        use LinkStatus::*;
        // healthy reads (re)connect from any non-connected state
        assert_eq!(next_link_state(Some(Connected), true), None);
        assert_eq!(next_link_state(Some(Degraded), true), Some(Connected));
        assert_eq!(next_link_state(Some(Disconnected), true), Some(Connected));
        assert_eq!(next_link_state(None, true), Some(Connected));
        // a fully-failing batch degrades a connected link, once
        assert_eq!(next_link_state(Some(Connected), false), Some(Degraded));
        assert_eq!(next_link_state(Some(Degraded), false), None);
        // a device that never connected stays disconnected
        assert_eq!(next_link_state(Some(Disconnected), false), None);
        assert_eq!(next_link_state(None, false), None);
    }

    #[test]
    fn batch_writes_parse_typed_and_raw_entries() {
        let json = serde_json::json!({
            "status": "init",
            "writes": [
                { "point": "setpoint", "value": 21.5 },
                { "point": "mask", "raw": "00ff" }
            ]
        });
        let writes = parse_batch_writes(&json).unwrap();
        assert_eq!(writes.len(), 2);
        assert_eq!(writes[0].point, "setpoint");
        assert_eq!(writes[0].value, Some(serde_json::json!(21.5)));
        assert_eq!(writes[1].raw.as_deref(), Some("00ff"));
        assert_eq!(writes[1].value, None);
    }

    #[test]
    fn batch_writes_reject_malformed_requests() {
        let missing = serde_json::json!({ "status": "init" });
        assert!(parse_batch_writes(&missing).unwrap_err().contains("`writes`"));
        let empty = serde_json::json!({ "writes": [] });
        assert!(parse_batch_writes(&empty).unwrap_err().contains("no writes"));
        let no_point = serde_json::json!({ "writes": [{ "value": 1 }] });
        assert!(parse_batch_writes(&no_point).unwrap_err().contains("`point`"));
        let no_value = serde_json::json!({ "writes": [{ "point": "x" }] });
        assert!(parse_batch_writes(&no_value).unwrap_err().contains("neither"));
        let null_value = serde_json::json!({ "writes": [{ "point": "x", "value": null }] });
        assert!(parse_batch_writes(&null_value).is_err());
    }

    #[test]
    fn batch_result_shapes_success_and_failure() {
        let ok = batch_result(None, vec![serde_json::json!({ "point": "a", "status": "successful" })]);
        assert_eq!(ok["status"], "successful");
        assert_eq!(ok["results"].as_array().unwrap().len(), 1);
        let failed = batch_result(
            Some("write to b failed: boom".into()),
            vec![
                serde_json::json!({ "point": "a", "status": "successful" }),
                serde_json::json!({ "point": "b", "status": "failed", "reason": "write to b failed: boom" }),
            ],
        );
        assert_eq!(failed["status"], "failed");
        assert_eq!(failed["reason"], "write to b failed: boom");
        assert_eq!(failed["results"][1]["status"], "failed");
    }

    #[test]
    fn batch_caps_follow_write_support() {
        let mut caps = Capabilities {
            protocol: "x",
            version: "0",
            datatypes: vec![],
            point_kinds: vec![],
            command_verbs: vec!["write".into()],
            features: vec![],
            subscribe: false,
        };
        augment_batch_caps(&mut caps);
        assert!(caps.command_verbs.iter().any(|v| v == "write-batch"));
        augment_batch_caps(&mut caps); // idempotent
        assert_eq!(caps.command_verbs.iter().filter(|v| *v == "write-batch").count(), 1);
        let mut read_only = caps.clone();
        read_only.command_verbs = vec![];
        augment_batch_caps(&mut read_only);
        assert!(read_only.command_verbs.is_empty());
    }

    #[test]
    fn management_caps_are_advertised() {
        let mut caps = Capabilities {
            protocol: "modbus",
            version: "0.0.0",
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
