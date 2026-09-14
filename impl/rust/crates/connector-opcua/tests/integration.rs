//! End-to-end subscription test: run the real `OpcuaConnector` against an in-process
//! `async-opcua` server, subscribe to a couple of variables, mutate them server-side, and
//! assert the pushed samples (values, quality, timestamps, teardown). This exercises the
//! actual wire path (session, subscription, monitored items, data-change notifications).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use connector_opcua::OpcuaConnector;
use opcua::nodes::VariableBuilder;
use opcua::server::diagnostics::NamespaceMetadata;
use opcua::server::node_manager::memory::{simple_node_manager, SimpleNodeManager};
use opcua::server::{ServerBuilder, ServerHandle};
use opcua::types::{DataTypeId, DataValue, DateTime, NodeId, ObjectId, StatusCode, Variant};
use tedge_dot_sdk::{
    Access, Connector, ConnectorConfig, DataType, Endianness, Mode, PointRef, Quality, Sample,
    Value, WordOrder,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

// Note: must differ from the application URI, which always becomes namespace index 1.
const NAMESPACE_URI: &str = "urn:tedge-dot-opcua-test:nodes";

/// Start an anonymous, security-`None` OPC-UA server on an ephemeral port with two variables
/// (`Temperature`: Double, `Counter`: UInt16) in a custom namespace.
async fn start_server() -> (ServerHandle, Arc<SimpleNodeManager>, u16, u16) {
    start_server_on(0).await
}

/// [`start_server`] on a given port (0 = ephemeral), so a test can bring a stopped server back
/// where the connector expects it.
async fn start_server_on(port: u16) -> (ServerHandle, Arc<SimpleNodeManager>, u16, u16) {
    // Opt-in wire logging for debugging: RUST_LOG=opcua_client=debug,opcua_server=debug
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();
    let listener = TcpListener::bind(("127.0.0.1", port)).await.unwrap();
    let port = listener.local_addr().unwrap().port();

    let (server, handle) = ServerBuilder::new_anonymous("tedge-dot-test")
        .application_uri("urn:tedge-dot-opcua-test")
        .product_uri("urn:tedge-dot-opcua-test")
        .host("127.0.0.1")
        .port(port)
        .discovery_urls(vec![format!("opc.tcp://127.0.0.1:{port}")])
        .with_node_manager(simple_node_manager(
            NamespaceMetadata {
                namespace_uri: NAMESPACE_URI.to_owned(),
                ..Default::default()
            },
            "simple",
        ))
        .build()
        .unwrap();

    let nm = handle
        .node_managers()
        .get_of_type::<SimpleNodeManager>()
        .unwrap();
    let ns = handle.get_namespace_index(NAMESPACE_URI).unwrap();

    {
        let mut space = nm.address_space().write();
        VariableBuilder::new(&NodeId::new(ns, "Temperature"), "Temperature", "Temperature")
            .data_type(DataTypeId::Double)
            .value(21.5f64)
            .organized_by(ObjectId::ObjectsFolder)
            .insert(&mut *space);
        VariableBuilder::new(&NodeId::new(ns, "Counter"), "Counter", "Counter")
            .data_type(DataTypeId::UInt16)
            .value(0u16)
            .organized_by(ObjectId::ObjectsFolder)
            .insert(&mut *space);
    }

    tokio::spawn(server.run_with(listener));
    (handle, nm, ns, port)
}

fn pref(id: &str, datatype: DataType, interval_ms: u64) -> PointRef {
    PointRef {
        id: id.to_string(),
        mode: Mode::Typed,
        datatype: Some(datatype),
        endianness: Endianness::Big,
        word_order: WordOrder::Big,
        access: Access::Read,
        unit: None,
        transform: Default::default(),
        interval: Some(Duration::from_millis(interval_ms)),
    }
}

/// Receive samples until `pred` matches (skipping e.g. initial-value notifications).
/// `what` names the expected sample in the timeout panic message.
async fn wait_for_sample(
    rx: &mut mpsc::Receiver<Sample>,
    what: &str,
    pred: impl Fn(&Sample) -> bool,
) -> Sample {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let sample = rx.recv().await.expect("sample channel closed");
            if pred(&sample) {
                return sample;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for sample: {what}"))
}

#[tokio::test]
async fn subscription_pushes_data_changes() {
    let (handle, nm, ns, port) = start_server().await;

    // Build the connector config pointing at the test server.
    let toml = format!(
        r#"
        [connector]
        protocol = "opcua"

        [[device]]
        name = "plc-1"
        protocol_address = {{ endpoint = "opc.tcp://127.0.0.1:{port}" }}
        default_mode = "typed"

          [[device.point]]
          id = "temperature"
          datatype = "float64"
          unit = "°C"
          address = {{ namespace = {ns}, identifier = "Temperature" }}

          [[device.point]]
          id = "counter"
          datatype = "uint16"
          address = {{ namespace = {ns}, identifier = "Counter" }}
        "#
    );
    let config: ConnectorConfig = toml::from_str(&toml).unwrap();

    let mut connector = OpcuaConnector::default();
    connector.configure(&config).unwrap();
    assert!(connector.capabilities().subscribe);

    let reports = connector.connect().await.unwrap();
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].status.as_str(), "connected", "{:?}", reports[0].reason);

    let device = "plc-1".to_string();
    let points = vec![
        pref("temperature", DataType::Float64, 100),
        pref("counter", DataType::Uint16, 100),
    ];
    let (tx, mut rx) = mpsc::channel::<Sample>(64);
    connector.subscribe(&device, &points, tx).await.unwrap();

    // Monitored items push their current value on creation.
    let initial = wait_for_sample(&mut rx, "initial temperature value", |s| s.point == "temperature").await;
    assert_eq!(initial.device, "plc-1");
    assert_eq!(initial.quality, Quality::Good);
    assert_eq!(initial.value, Some(Value::Number(21.5)));
    assert_eq!(initial.unit.as_deref(), Some("°C"));
    assert_eq!(initial.addr["node_id"], format!("ns={ns};s=Temperature"));
    assert!(initial.seq.is_none(), "connector must not stamp seq");

    // Space server-side writes more than one sampling interval (100 ms) apart. The async-opcua
    // 0.18 server defers a value written less than one sampling interval after the previous
    // source timestamp (`sample_skipped_data_value`), and a sub-millisecond race in the deferred
    // flush can strand it forever, wedging the test on slow/contended CI hosts. Notifications
    // arrive on the server's 100 ms publish tick, so reacting to a sample immediately with a
    // write lands exactly on that knife edge unless we wait out the interval first.
    let sampling_gap = Duration::from_millis(150);

    // Mutate the variables server-side and expect pushed samples with the new values.
    //
    // Mutations are strictly sequential: write one value, await its sample, then write the
    // next. `wait_for_sample` discards non-matching samples while it scans, and the server
    // batches concurrent changes into one notification in arbitrary (hash-map) order — two
    // outstanding expected samples would let the first wait swallow the second one.
    tokio::time::sleep(sampling_gap).await;
    let temp_node = NodeId::new(ns, "Temperature");
    let counter_node = NodeId::new(ns, "Counter");
    nm.set_value(
        handle.subscriptions(),
        &temp_node,
        None,
        DataValue::new_now(42.5f64),
    )
    .unwrap();
    let temp = wait_for_sample(&mut rx, "temperature update 42.5", |s| {
        s.point == "temperature" && s.value == Some(Value::Number(42.5))
    })
    .await;
    assert_eq!(temp.quality, Quality::Good);
    assert_eq!(temp.datatype, Some(DataType::Float64));
    assert_eq!(temp.raw, 42.5f64.to_be_bytes().to_vec());
    // ts must come from the server's source timestamp, i.e. be a plausible recent instant.
    let age = time::OffsetDateTime::now_utc() - temp.ts;
    assert!(age.whole_seconds() >= 0 && age.whole_seconds() < 30, "stale ts: {}", temp.ts);

    tokio::time::sleep(sampling_gap).await;
    nm.set_value(
        handle.subscriptions(),
        &counter_node,
        None,
        DataValue::new_now(7u16),
    )
    .unwrap();
    let counter = wait_for_sample(&mut rx, "counter update 7", |s| {
        s.point == "counter" && s.value == Some(Value::Number(7.0))
    })
    .await;
    assert_eq!(counter.quality, Quality::Good);
    assert_eq!(counter.datatype, Some(DataType::Uint16));

    // A bad status code on the server must surface as a `bad` quality sample.
    tokio::time::sleep(sampling_gap).await;
    nm.set_value(
        handle.subscriptions(),
        &temp_node,
        None,
        DataValue {
            value: Some(Variant::Double(0.0)),
            status: Some(StatusCode::BadSensorFailure),
            source_timestamp: Some(DateTime::now()),
            server_timestamp: Some(DateTime::now()),
            ..Default::default()
        },
    )
    .unwrap();
    let bad = wait_for_sample(&mut rx, "bad-status temperature", |s| {
        s.point == "temperature" && s.quality == Quality::Bad
    })
    .await;
    assert!(bad.error.as_deref().unwrap_or_default().contains("bad status"));

    // Polling must keep working alongside the subscription (poll fallback / one-shot reads).
    let polled = connector.read_points(&device, &points).await.unwrap();
    assert_eq!(polled.len(), 2);
    let polled_counter = polled.iter().find(|s| s.point == "counter").unwrap();
    assert_eq!(polled_counter.value, Some(Value::Number(7.0)));

    // disconnect() must tear the subscription down: further server-side changes must not
    // reach the (now closed) sink.
    connector.disconnect().await.unwrap();
    nm.set_value(
        handle.subscriptions(),
        &counter_node,
        None,
        DataValue::new_now(99u16),
    )
    .unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    while let Ok(s) = rx.try_recv() {
        assert_ne!(
            s.value,
            Some(Value::Number(99.0)),
            "received a sample pushed after disconnect"
        );
    }

    handle.cancel();
}

#[tokio::test]
async fn subscribe_unknown_point_is_rejected() {
    let (handle, _nm, ns, port) = start_server().await;

    let toml = format!(
        r#"
        [connector]
        protocol = "opcua"

        [[device]]
        name = "plc-1"
        protocol_address = {{ endpoint = "opc.tcp://127.0.0.1:{port}" }}
        default_mode = "typed"

          [[device.point]]
          id = "temperature"
          datatype = "float64"
          address = {{ namespace = {ns}, identifier = "Temperature" }}
        "#
    );
    let config: ConnectorConfig = toml::from_str(&toml).unwrap();

    let mut connector = OpcuaConnector::default();
    connector.configure(&config).unwrap();
    connector.connect().await.unwrap();

    let (tx, _rx) = mpsc::channel::<Sample>(8);
    let err = connector
        .subscribe(
            &"plc-1".to_string(),
            &[pref("no_such_point", DataType::Float64, 100)],
            tx,
        )
        .await;
    assert!(err.is_err(), "unknown point must fail the subscribe call");

    connector.disconnect().await.unwrap();
    handle.cancel();
}

/// A one-device config with a single `temperature` point, for the recovery tests. The short
/// request timeout keeps the keep-alive window (and so the silent-server test) short.
fn recovery_config(port: u16, ns: u16) -> ConnectorConfig {
    toml::from_str(&format!(
        r#"
        [connector]
        protocol = "opcua"

        [connection]
        request_timeout_s = 1

        [[device]]
        name = "plc-1"
        protocol_address = {{ endpoint = "opc.tcp://127.0.0.1:{port}" }}
        default_mode = "typed"

          [[device.point]]
          id = "temperature"
          datatype = "float64"
          address = {{ namespace = {ns}, identifier = "Temperature" }}
        "#
    ))
    .unwrap()
}

/// Poll `check_subscription` until it fails with a reason containing `expected`.
async fn wait_for_check_failure(
    connector: &mut OpcuaConnector,
    device: &str,
    expected: &str,
    within: Duration,
) -> String {
    let deadline = tokio::time::Instant::now() + within;
    let mut last = String::from("never checked");
    while tokio::time::Instant::now() < deadline {
        match connector.check_subscription(&device.to_string()).await {
            Err(e) if e.to_string().contains(expected) => return e.to_string(),
            Err(e) => last = e.to_string(),
            Ok(()) => last = "live".into(),
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("check_subscription did not report '{expected}' within {within:?} (last: {last})");
}

/// Connect, subscribe the `temperature` point and wait for its initial value.
async fn connect_and_subscribe(
    connector: &mut OpcuaConnector,
    tx: &mpsc::Sender<Sample>,
    rx: &mut mpsc::Receiver<Sample>,
) {
    let reports = connector.connect().await.unwrap();
    assert_eq!(reports[0].status.as_str(), "connected", "{:?}", reports[0].reason);
    subscribe_temperature(connector, tx, rx).await;
}

async fn subscribe_temperature(
    connector: &mut OpcuaConnector,
    tx: &mpsc::Sender<Sample>,
    rx: &mut mpsc::Receiver<Sample>,
) {
    let points = [pref("temperature", DataType::Float64, 100)];
    connector
        .subscribe(&"plc-1".to_string(), &points, tx.clone())
        .await
        .unwrap();
    wait_for_sample(rx, "initial temperature value", |s| {
        s.point == "temperature" && s.quality == Quality::Good
    })
    .await;
    connector
        .check_subscription(&"plc-1".to_string())
        .await
        .expect("a fresh subscription is live");
}

/// The reported failure. A device whose points are all pushed makes no reads, so a server outage
/// longer than the client's own session retries went unnoticed: the client's event loop ended,
/// the link stayed `connected` and the device never sent another sample. The check has to
/// report the outage -- including once the client has given up, which is exactly where nothing
/// noticed before -- and reconnecting and re-subscribing must bring push back.
#[tokio::test]
async fn check_subscription_reports_an_outage_and_reconnect_restores_push() {
    let (handle, _nm, ns, port) = start_server().await;
    let mut connector = OpcuaConnector::default();
    connector.configure(&recovery_config(port, ns)).unwrap();
    let (tx, mut rx) = mpsc::channel::<Sample>(64);
    connect_and_subscribe(&mut connector, &tx, &mut rx).await;
    let device = "plc-1".to_string();

    handle.cancel();
    wait_for_check_failure(&mut connector, &device, "", Duration::from_secs(10)).await;
    // The client retries three times (1 s, 2 s, 4 s apart), then its event loop ends.
    wait_for_check_failure(&mut connector, &device, "gave up", Duration::from_secs(30)).await;

    let (handle, nm, ns, _) = start_server_on(port).await;
    let report = connector.reconnect(&device).await.unwrap();
    assert_eq!(report.status.as_str(), "connected", "{:?}", report.reason);
    while rx.try_recv().is_ok() {}
    subscribe_temperature(&mut connector, &tx, &mut rx).await;

    // Push really flows again, beyond the initial value of a new monitored item.
    tokio::time::sleep(Duration::from_millis(150)).await;
    nm.set_value(
        handle.subscriptions(),
        &NodeId::new(ns, "Temperature"),
        None,
        DataValue::new_now(55.5f64),
    )
    .unwrap();
    wait_for_sample(&mut rx, "temperature update after the restart", |s| {
        s.point == "temperature" && s.value == Some(Value::Number(55.5))
    })
    .await;

    connector.disconnect().await.unwrap();
    handle.cancel();
}

/// A server that comes straight back, well inside the client's own session retries. async-opcua
/// reconnects by itself, but push delivery does not resume: against this server no data change
/// and no publish response arrives again (and against the e2e python-asyncua simulator, whose
/// log shows the client re-creating the subscription, no data change arrived either). Nothing
/// fails, so without the check a push-only device stays silent behind a `connected` link even
/// though its session reconnected. The check must still report it -- here as missing publish
/// responses -- which is what makes the runtime replace the session.
#[tokio::test]
async fn check_subscription_reports_push_the_client_did_not_restore() {
    let (handle, _nm, ns, port) = start_server().await;
    let mut connector = OpcuaConnector::default();
    connector.configure(&recovery_config(port, ns)).unwrap();
    let (tx, mut rx) = mpsc::channel::<Sample>(64);
    connect_and_subscribe(&mut connector, &tx, &mut rx).await;
    let device = "plc-1".to_string();

    handle.cancel();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let (handle, _nm, _, _) = start_server_on(port).await;

    // No reconnect() here: only what the client does on its own.
    wait_for_check_failure(&mut connector, &device, "no publish response", Duration::from_secs(40))
        .await;

    connector.disconnect().await.unwrap();
    handle.cancel();
}

/// Forward bytes between two sockets, holding them while `stalled` is set.
async fn forward(mut from: OwnedReadHalf, mut to: OwnedWriteHalf, stalled: Arc<AtomicBool>) {
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        let n = match from.read(&mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        while stalled.load(Ordering::Relaxed) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        if to.write_all(&buf[..n]).await.is_err() {
            return;
        }
    }
}

/// A TCP proxy in front of `upstream` that can stop forwarding while keeping every connection
/// open: a server that is still there but answers nothing (a frozen process, a half-open link).
/// Returns the proxy port and its stall switch.
async fn start_stall_proxy(upstream: u16) -> (u16, Arc<AtomicBool>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let stalled = Arc::new(AtomicBool::new(false));
    let flag = stalled.clone();
    tokio::spawn(async move {
        while let Ok((client, _)) = listener.accept().await {
            let Ok(server) = TcpStream::connect(("127.0.0.1", upstream)).await else {
                continue;
            };
            let (client_rx, client_tx) = client.into_split();
            let (server_rx, server_tx) = server.into_split();
            tokio::spawn(forward(client_rx, server_tx, flag.clone()));
            tokio::spawn(forward(server_rx, client_tx, flag.clone()));
        }
    });
    (port, stalled)
}

/// A server that stops answering but keeps the connection open raises no error in the client
/// (failed keep-alives are not counted by default), and a push-only device makes no reads that
/// could time out instead. The check has to notice that publish responses stopped.
#[tokio::test]
async fn check_subscription_reports_a_server_that_stops_answering() {
    let (handle, _nm, ns, port) = start_server().await;
    let (proxy_port, stalled) = start_stall_proxy(port).await;
    let mut connector = OpcuaConnector::default();
    connector.configure(&recovery_config(proxy_port, ns)).unwrap();
    let (tx, mut rx) = mpsc::channel::<Sample>(64);
    connect_and_subscribe(&mut connector, &tx, &mut rx).await;
    let device = "plc-1".to_string();

    stalled.store(true, Ordering::Relaxed);
    // The window is the revised publishing interval x keep-alive count, plus the 1 s request
    // timeout: a few seconds at the 100 ms interval requested here.
    wait_for_check_failure(&mut connector, &device, "no publish response", Duration::from_secs(30))
        .await;

    stalled.store(false, Ordering::Relaxed);
    let report = connector.reconnect(&device).await.unwrap();
    assert_eq!(report.status.as_str(), "connected", "{:?}", report.reason);
    while rx.try_recv().is_ok() {}
    subscribe_temperature(&mut connector, &tx, &mut rx).await;

    connector.disconnect().await.unwrap();
    handle.cancel();
}
