//! The behavioural layer's test broker + assertion probe, in one.
//!
//! A deliberately minimal MQTT 3.1.1 broker (CONNECT / SUBSCRIBE / PUBLISH QoS 0-1, retained
//! messages, wildcards, last-will) built on the `mqttbytes` v4 codec that rumqttc vendors —
//! the same codec the SDK's client uses, and no external broker dependency. Being in-process
//! buys the harness what a real broker cannot offer: every message is recorded with the
//! *publishing client attached*, so topic-discipline checks (B10) and retain-flag assertions
//! are exact instead of inferred.
//!
//! Deliberate simplifications (fine for a single-run conformance harness): QoS 2 is rejected
//! at the subscription level and unsupported for publishes, sessions are always clean, and
//! delivery to subscribers is QoS 0 (a broker may always grant a lower QoS than requested).

use bytes::BytesMut;
use rumqttc::mqttbytes::v4::{self, ConnAck, ConnectReturnCode, LastWill, Packet, PubAck, Publish, SubAck, SubscribeReasonCode, UnsubAck};
use rumqttc::mqttbytes::QoS;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tracing::debug;

const MAX_PACKET_SIZE: usize = 1024 * 1024;
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// One observed message: what was published, by whom, and how.
#[derive(Debug, Clone)]
pub struct Record {
    pub seq: usize,
    /// The publishing MQTT client id; [`BrokerHandle::publish`] injects as `"harness"`.
    pub client: String,
    pub topic: String,
    pub payload: Vec<u8>,
    pub retain: bool,
}

impl Record {
    pub fn json(&self) -> Result<serde_json::Value, String> {
        serde_json::from_slice(&self.payload)
            .map_err(|e| format!("payload on '{}' is not JSON: {e}", self.topic))
    }
}

struct Session {
    filters: Vec<String>,
    tx: mpsc::UnboundedSender<Publish>,
}

#[derive(Default)]
struct Shared {
    retained: HashMap<String, (String, Vec<u8>)>,
    sessions: HashMap<u64, Session>,
    log: Vec<Record>,
    next_session: u64,
}

impl Shared {
    /// Route one publish: record it, update the retained store, deliver to subscribers.
    fn route(&mut self, client: &str, topic: &str, payload: &[u8], retain: bool) {
        self.log.push(Record {
            seq: self.log.len(),
            client: client.to_string(),
            topic: topic.to_string(),
            payload: payload.to_vec(),
            retain,
        });
        if retain {
            if payload.is_empty() {
                self.retained.remove(topic);
            } else {
                self.retained
                    .insert(topic.to_string(), (client.to_string(), payload.to_vec()));
            }
        }
        for session in self.sessions.values() {
            if session.filters.iter().any(|f| topic_matches(f, topic)) {
                let publish = Publish::new(topic, QoS::AtMostOnce, payload.to_vec());
                let _ = session.tx.send(publish);
            }
        }
    }
}

/// Handle to the running broker; dropping it stops accepting new connections.
pub struct BrokerHandle {
    shared: Arc<Mutex<Shared>>,
    port: u16,
    accept_task: tokio::task::JoinHandle<()>,
}

impl Drop for BrokerHandle {
    fn drop(&mut self) {
        self.accept_task.abort();
    }
}

impl BrokerHandle {
    pub async fn start() -> Result<BrokerHandle, String> {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .map_err(|e| format!("broker bind failed: {e}"))?;
        let port = listener
            .local_addr()
            .map_err(|e| format!("broker local_addr: {e}"))?
            .port();
        let shared = Arc::new(Mutex::new(Shared::default()));
        let accept_shared = shared.clone();
        let accept_task = tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => {
                        let shared = accept_shared.clone();
                        tokio::spawn(async move {
                            if let Err(e) = serve_connection(stream, shared).await {
                                debug!("broker connection ended: {e}");
                            }
                        });
                    }
                    Err(e) => {
                        debug!("broker accept error: {e}");
                        break;
                    }
                }
            }
        });
        Ok(BrokerHandle {
            shared,
            port,
            accept_task,
        })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// Inject a message as the pseudo-client `"harness"` (e.g. a retained `cmd … init`).
    pub fn publish(&self, topic: &str, payload: &[u8], retain: bool) {
        self.shared
            .lock()
            .unwrap()
            .route("harness", topic, payload, retain);
    }

    /// Current end of the observation log; pass to [`Self::wait_for`]/[`Self::records_from`]
    /// to scope an assertion to "messages from now on".
    pub fn mark(&self) -> usize {
        self.shared.lock().unwrap().log.len()
    }

    pub fn records_from(&self, seq: usize) -> Vec<Record> {
        let shared = self.shared.lock().unwrap();
        shared.log.iter().skip(seq).cloned().collect()
    }

    /// The retained payload currently stored for `topic`, if any.
    pub fn retained(&self, topic: &str) -> Option<Vec<u8>> {
        self.shared
            .lock()
            .unwrap()
            .retained
            .get(topic)
            .map(|(_, p)| p.clone())
    }

    /// Wait until a record at/after `from_seq` matches `pred`.
    pub async fn wait_for(
        &self,
        from_seq: usize,
        timeout: Duration,
        what: &str,
        pred: impl Fn(&Record) -> bool,
    ) -> Result<Record, String> {
        let deadline = tokio::time::Instant::now() + timeout;
        let mut scanned = from_seq;
        loop {
            {
                let shared = self.shared.lock().unwrap();
                let start = scanned.min(shared.log.len());
                if let Some(r) = shared.log[start..].iter().find(|r| pred(r)) {
                    return Ok(r.clone());
                }
                scanned = shared.log.len();
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(format!("timed out after {timeout:?} waiting for {what}"));
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }

    /// Collect every record matching `pred` observed at/after `from_seq` within `window`.
    pub async fn collect_for(
        &self,
        from_seq: usize,
        window: Duration,
        pred: impl Fn(&Record) -> bool,
    ) -> Vec<Record> {
        tokio::time::sleep(window).await;
        self.records_from(from_seq)
            .into_iter()
            .filter(|r| pred(r))
            .collect()
    }
}

/// One client connection: decode packets, answer the handshake/pings, route publishes.
async fn serve_connection(
    mut stream: TcpStream,
    shared: Arc<Mutex<Shared>>,
) -> Result<(), String> {
    let mut buf = BytesMut::with_capacity(4096);

    // Handshake: the first packet must be CONNECT.
    let connect = loop {
        match v4::read(&mut buf, MAX_PACKET_SIZE) {
            Ok(Packet::Connect(c)) => break c,
            Ok(other) => return Err(format!("expected CONNECT, got {other:?}")),
            Err(rumqttc::mqttbytes::Error::InsufficientBytes(_)) => {
                if stream
                    .read_buf(&mut buf)
                    .await
                    .map_err(|e| format!("read: {e}"))?
                    == 0
                {
                    return Err("connection closed before CONNECT".into());
                }
            }
            Err(e) => return Err(format!("bad packet: {e}")),
        }
    };
    let client_id = connect.client_id.clone();
    let last_will: Option<LastWill> = connect.last_will.clone();
    write_packet(&mut stream, |b| {
        ConnAck::new(ConnectReturnCode::Success, false).write(b)
    })
    .await?;

    let (tx, mut rx) = mpsc::unbounded_channel::<Publish>();
    let session_id = {
        let mut s = shared.lock().unwrap();
        let id = s.next_session;
        s.next_session += 1;
        s.sessions.insert(
            id,
            Session {
                filters: Vec::new(),
                tx,
            },
        );
        id
    };

    let mut graceful = false;
    let result: Result<(), String> = async {
        loop {
            // Drain complete packets already buffered.
            loop {
                match v4::read(&mut buf, MAX_PACKET_SIZE) {
                    Ok(packet) => {
                        if handle_packet(&mut stream, &shared, session_id, &client_id, packet)
                            .await?
                        {
                            graceful = true;
                            return Ok(());
                        }
                    }
                    Err(rumqttc::mqttbytes::Error::InsufficientBytes(_)) => break,
                    Err(e) => return Err(format!("bad packet from {client_id}: {e}")),
                }
            }
            tokio::select! {
                read = stream.read_buf(&mut buf) => {
                    if read.map_err(|e| format!("read: {e}"))? == 0 {
                        return Ok(()); // peer closed without DISCONNECT -> will fires
                    }
                }
                Some(publish) = rx.recv() => {
                    write_packet(&mut stream, |b| publish.write(b)).await?;
                }
            }
        }
    }
    .await;

    {
        let mut s = shared.lock().unwrap();
        s.sessions.remove(&session_id);
        if !graceful {
            if let Some(will) = last_will {
                s.route(&client_id, &will.topic, &will.message, will.retain);
            }
        }
    }
    result
}

/// Handle one inbound packet. Returns `Ok(true)` on a graceful DISCONNECT.
async fn handle_packet(
    stream: &mut TcpStream,
    shared: &Arc<Mutex<Shared>>,
    session_id: u64,
    client_id: &str,
    packet: Packet,
) -> Result<bool, String> {
    match packet {
        Packet::Publish(p) => {
            if p.qos == QoS::AtLeastOnce {
                write_packet(stream, |b| PubAck::new(p.pkid).write(b)).await?;
            }
            shared
                .lock()
                .unwrap()
                .route(client_id, &p.topic, &p.payload, p.retain);
        }
        Packet::Subscribe(sub) => {
            let mut codes = Vec::new();
            let mut retained_to_send: Vec<(String, Vec<u8>)> = Vec::new();
            {
                let mut s = shared.lock().unwrap();
                for filter in &sub.filters {
                    if filter.qos == QoS::ExactlyOnce {
                        codes.push(SubscribeReasonCode::Failure);
                        continue;
                    }
                    codes.push(SubscribeReasonCode::Success(QoS::AtMostOnce));
                    if let Some(session) = s.sessions.get_mut(&session_id) {
                        session.filters.push(filter.path.clone());
                    }
                    for (topic, (_, payload)) in &s.retained {
                        if topic_matches(&filter.path, topic) {
                            retained_to_send.push((topic.clone(), payload.clone()));
                        }
                    }
                }
            }
            write_packet(stream, |b| SubAck::new(sub.pkid, codes.clone()).write(b)).await?;
            for (topic, payload) in retained_to_send {
                let mut publish = Publish::new(&topic, QoS::AtMostOnce, payload);
                publish.retain = true; // delivered from the retained store
                write_packet(stream, |b| publish.write(b)).await?;
            }
        }
        Packet::Unsubscribe(unsub) => {
            {
                let mut s = shared.lock().unwrap();
                if let Some(session) = s.sessions.get_mut(&session_id) {
                    session.filters.retain(|f| !unsub.topics.contains(f));
                }
            }
            write_packet(stream, |b| UnsubAck::new(unsub.pkid).write(b)).await?;
        }
        Packet::PingReq => {
            write_packet(stream, |b| {
                b.extend_from_slice(&[0xd0, 0x00]); // PINGRESP
                Ok::<usize, rumqttc::mqttbytes::Error>(2)
            })
            .await?;
        }
        Packet::Disconnect => return Ok(true),
        Packet::PubAck(_) | Packet::PubRec(_) | Packet::PubRel(_) | Packet::PubComp(_) => {}
        other => debug!("broker ignoring {other:?}"),
    }
    Ok(false)
}

async fn write_packet<E: std::fmt::Debug>(
    stream: &mut TcpStream,
    encode: impl FnOnce(&mut BytesMut) -> Result<usize, E>,
) -> Result<(), String> {
    let mut out = BytesMut::new();
    encode(&mut out).map_err(|e| format!("encode: {e:?}"))?;
    stream
        .write_all(&out)
        .await
        .map_err(|e| format!("write: {e}"))
}

/// MQTT topic filter matching (`+` one level, `#` trailing multi-level).
pub fn topic_matches(filter: &str, topic: &str) -> bool {
    let mut f = filter.split('/');
    let mut t = topic.split('/');
    loop {
        match (f.next(), t.next()) {
            (Some("#"), _) => return true,
            (Some("+"), Some(_)) => {}
            (Some(fl), Some(tl)) if fl == tl => {}
            (None, None) => return true,
            _ => return false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rumqttc::{AsyncClient, Event, MqttOptions, Packet as ClientPacket, QoS as ClientQoS};

    #[test]
    fn topic_filter_matching() {
        assert!(topic_matches("a/b/c", "a/b/c"));
        assert!(topic_matches("a/+/c", "a/b/c"));
        assert!(topic_matches("a/#", "a/b/c"));
        assert!(topic_matches("#", "a"));
        assert!(!topic_matches("a/b", "a/b/c"));
        assert!(!topic_matches("a/+", "a/b/c"));
        assert!(!topic_matches("a/b/c", "a/b"));
        assert!(topic_matches("te/device/+/ot/modbus/cmd/+/+", "te/device/plc1/ot/modbus/cmd/write/c1"));
    }

    /// End-to-end through a real rumqttc client: retained delivery, attribution, round-trip.
    #[tokio::test]
    async fn client_roundtrip_retained_and_log() {
        let broker = BrokerHandle::start().await.unwrap();

        let mut opts = MqttOptions::new("test-client", "127.0.0.1", broker.port());
        opts.set_keep_alive(Duration::from_secs(5));
        let (client, mut eventloop) = AsyncClient::new(opts, 16);
        let (suback_tx, suback_rx) = tokio::sync::oneshot::channel::<()>();
        let events = tokio::spawn(async move {
            let mut suback_tx = Some(suback_tx);
            let mut seen = Vec::new();
            while let Ok(event) = eventloop.poll().await {
                match event {
                    Event::Incoming(ClientPacket::SubAck(_)) => {
                        if let Some(tx) = suback_tx.take() {
                            let _ = tx.send(());
                        }
                    }
                    Event::Incoming(ClientPacket::Publish(p)) => {
                        seen.push((p.topic.clone(), p.payload.to_vec(), p.retain));
                        if seen.len() == 2 {
                            break;
                        }
                    }
                    _ => {}
                }
            }
            seen
        });

        client
            .publish("t/retained", ClientQoS::AtLeastOnce, true, "kept")
            .await
            .unwrap();
        // wait until the broker recorded it, then subscribe: retained copy must arrive
        broker
            .wait_for(0, Duration::from_secs(5), "retained publish", |r| {
                r.topic == "t/retained"
            })
            .await
            .unwrap();
        client.subscribe("t/#", ClientQoS::AtLeastOnce).await.unwrap();
        // the subscription only exists once the broker SubAcks — publishing a non-retained
        // message before that would race and be (correctly) dropped
        tokio::time::timeout(Duration::from_secs(5), suback_rx)
            .await
            .expect("suback within 5s")
            .unwrap();
        // and a live publish injected by the harness must arrive too
        broker.publish("t/live", b"now", false);

        let seen = tokio::time::timeout(Duration::from_secs(10), events)
            .await
            .expect("both publishes delivered within 10s")
            .unwrap();
        assert!(seen.iter().any(|(t, p, retain)| t == "t/retained" && p == b"kept" && *retain));
        assert!(seen.iter().any(|(t, p, retain)| t == "t/live" && p == b"now" && !*retain));

        let records = broker.records_from(0);
        let retained_rec = records.iter().find(|r| r.topic == "t/retained").unwrap();
        assert_eq!(retained_rec.client, "test-client");
        assert!(retained_rec.retain);
        assert_eq!(broker.retained("t/retained").unwrap(), b"kept");
        client.disconnect().await.ok();
    }
}
