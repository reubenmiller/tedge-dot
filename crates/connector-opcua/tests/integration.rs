//! End-to-end subscription test: run the real `OpcuaConnector` against an in-process
//! `async-opcua` server, subscribe to a couple of variables, mutate them server-side, and
//! assert the pushed samples (values, quality, timestamps, teardown). This exercises the
//! actual wire path (session, subscription, monitored items, data-change notifications).

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
use tokio::net::TcpListener;
use tokio::sync::mpsc;

// Note: must differ from the application URI, which always becomes namespace index 1.
const NAMESPACE_URI: &str = "urn:tedge-dot-opcua-test:nodes";

/// Start an anonymous, security-`None` OPC-UA server on an ephemeral port with two variables
/// (`Temperature`: Double, `Counter`: UInt16) in a custom namespace.
async fn start_server() -> (ServerHandle, Arc<SimpleNodeManager>, u16, u16) {
    // Opt-in wire logging for debugging: RUST_LOG=opcua_client=debug,opcua_server=debug
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
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
