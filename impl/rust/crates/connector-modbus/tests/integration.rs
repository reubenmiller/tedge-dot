//! End-to-end TCP integration test: run the real `ModbusConnector` against an in-process
//! `tokio-modbus` TCP server with known register/coil contents, and assert the decoded samples
//! and the write round-trip. This exercises the actual wire path (connect, read, write, decode).

use std::collections::HashMap;
use std::future;
use std::sync::{Arc, Mutex};

use connector_modbus::ModbusConnector;
use tedge_dot_sdk::{
    Access, Connector, ConnectorConfig, DataType, Endianness, Mode, PointRef, Quality, Value,
    WordOrder,
};
use tokio::net::TcpListener;
use tokio_modbus::prelude::{ExceptionCode, Request, Response};
use tokio_modbus::server::tcp::{accept_tcp_connection, Server};

#[derive(Clone)]
struct TestService {
    holding: Arc<Mutex<HashMap<u16, u16>>>,
    input: Arc<Mutex<HashMap<u16, u16>>>,
    coils: Arc<Mutex<HashMap<u16, bool>>>,
}

impl tokio_modbus::server::Service for TestService {
    type Request = Request<'static>;
    type Response = Response;
    type Exception = ExceptionCode;
    type Future = future::Ready<Result<Self::Response, Self::Exception>>;

    fn call(&self, req: Self::Request) -> Self::Future {
        let res = match req {
            Request::ReadHoldingRegisters(addr, cnt) => {
                read_regs(&self.holding.lock().unwrap(), addr, cnt)
                    .map(Response::ReadHoldingRegisters)
            }
            Request::ReadInputRegisters(addr, cnt) => {
                read_regs(&self.input.lock().unwrap(), addr, cnt)
                    .map(Response::ReadInputRegisters)
            }
            Request::ReadCoils(addr, cnt) => {
                let map = self.coils.lock().unwrap();
                let mut out = Vec::new();
                for i in 0..cnt {
                    out.push(*map.get(&(addr + i)).unwrap_or(&false));
                }
                Ok(Response::ReadCoils(out))
            }
            Request::WriteSingleRegister(addr, value) => {
                self.holding.lock().unwrap().insert(addr, value);
                Ok(Response::WriteSingleRegister(addr, value))
            }
            Request::WriteMultipleRegisters(addr, values) => {
                let mut map = self.holding.lock().unwrap();
                for (i, v) in values.iter().enumerate() {
                    map.insert(addr + i as u16, *v);
                }
                Ok(Response::WriteMultipleRegisters(addr, values.len() as u16))
            }
            Request::WriteSingleCoil(addr, on) => {
                self.coils.lock().unwrap().insert(addr, on);
                Ok(Response::WriteSingleCoil(addr, on))
            }
            _ => Err(ExceptionCode::IllegalFunction),
        };
        future::ready(res)
    }
}

fn read_regs(map: &HashMap<u16, u16>, addr: u16, cnt: u16) -> Result<Vec<u16>, ExceptionCode> {
    let mut out = vec![0; cnt as usize];
    for i in 0..cnt {
        out[i as usize] = *map.get(&(addr + i)).unwrap_or(&0);
    }
    Ok(out)
}

fn pref(id: &str, mode: Mode, datatype: Option<DataType>, access: Access) -> PointRef {
    PointRef {
        id: id.to_string(),
        mode,
        datatype,
        endianness: Endianness::Big,
        word_order: WordOrder::Big,
        access,
        unit: None,
        transform: Default::default(),
        interval: None,
    }
}

#[tokio::test]
async fn tcp_read_and_write_roundtrip() {
    // Seed the server: holding[7..9] = 42.5 (float32), holding[0] = 0x1234, coil[0] = true.
    let mut holding = HashMap::new();
    holding.insert(0, 0x1234);
    holding.insert(7, 0x422a);
    holding.insert(8, 0x0000);
    holding.insert(10, 0x0000);
    holding.insert(11, 0x0000);
    let mut coils = HashMap::new();
    coils.insert(0u16, true);
    let service = TestService {
        holding: Arc::new(Mutex::new(holding)),
        input: Arc::new(Mutex::new(HashMap::new())),
        coils: Arc::new(Mutex::new(coils)),
    };

    // Start the server on an ephemeral port.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let svc = service.clone();
    tokio::spawn(async move {
        let server = Server::new(listener);
        let new_service = move |_addr| Ok(Some(svc.clone()));
        let on_connected = move |stream, socket_addr| {
            let new_service = new_service.clone();
            async move { accept_tcp_connection(stream, socket_addr, new_service) }
        };
        let on_error = |err| eprintln!("server error: {err}");
        let _ = server.serve(&on_connected, on_error).await;
    });

    // Build the connector config pointing at the test server.
    let toml = format!(
        r#"
        [connector]
        protocol = "modbus"

        [[device]]
        name = "plc-1"
        protocol_address = {{ transport = "tcp", host = "{}", port = {}, unit_id = 1 }}
        default_mode = "typed"

          [[device.point]]
          id = "boiler_temp"
          datatype = "float32"
          address = {{ table = "holding", address = 7, count = 2 }}

          [[device.point]]
          id = "status_word"
          mode = "raw"
          address = {{ table = "holding", address = 0, count = 1 }}

          [[device.point]]
          id = "run_state"
          datatype = "bool"
          address = {{ table = "coil", address = 0, count = 1 }}

          [[device.point]]
          id = "setpoint"
          datatype = "float32"
          access = "read_write"
          address = {{ table = "holding", address = 10, count = 2 }}
        "#,
        addr.ip(),
        addr.port()
    );
    let config: ConnectorConfig = toml::from_str(&toml).unwrap();

    let mut connector = ModbusConnector::default();
    connector.configure(&config).unwrap();
    let reports = connector.connect().await.unwrap();
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].status.as_str(), "connected");

    let device = "plc-1".to_string();

    // Read the three points.
    let points = vec![
        pref("boiler_temp", Mode::Typed, Some(DataType::Float32), Access::Read),
        pref("status_word", Mode::Raw, None, Access::Read),
        pref("run_state", Mode::Typed, Some(DataType::Bool), Access::Read),
    ];
    let samples = connector.read_points(&device, &points).await.unwrap();
    assert_eq!(samples.len(), 3);

    let boiler = samples.iter().find(|s| s.point == "boiler_temp").unwrap();
    assert_eq!(boiler.quality, Quality::Good);
    assert_eq!(boiler.value, Some(Value::Number(42.5)));

    let status = samples.iter().find(|s| s.point == "status_word").unwrap();
    assert_eq!(status.quality, Quality::Good);
    assert!(status.value.is_none());
    // raw envelope hex (grouped per 16-bit register); `raw` is opt-in (§5: sample_debug)
    let env = status.to_envelope(true);
    assert_eq!(env["raw"], "1234");
    assert!(status.to_envelope(false).get("raw").is_none());

    let run = samples.iter().find(|s| s.point == "run_state").unwrap();
    assert_eq!(run.value, Some(Value::Bool(true)));

    // Write a new setpoint (42.5 -> registers 0x422a 0x0000) and read it back.
    let write_req = tedge_dot_sdk::CommandRequest {
        point: "setpoint".to_string(),
        value: Some(serde_json::json!(42.5)),
        value_repr: Some("number".to_string()),
        raw: None,
    };
    connector.execute(&device, "write", &write_req).await.unwrap();

    let readback = connector
        .read_points(
            &device,
            &[pref("setpoint", Mode::Typed, Some(DataType::Float32), Access::ReadWrite)],
        )
        .await
        .unwrap();
    assert_eq!(readback[0].value, Some(Value::Number(42.5)));

    // Writing to a read-only point must be rejected.
    let bad_write = tedge_dot_sdk::CommandRequest {
        point: "status_word".to_string(),
        value: Some(serde_json::json!(1)),
        value_repr: Some("number".to_string()),
        raw: None,
    };
    let err = connector.execute(&device, "write", &bad_write).await;
    assert!(err.is_err(), "write to read-only point should fail");
}

/// Regression: a peer that accepts the connection and then answers nothing must fail the read,
/// not wait forever.
///
/// tokio-modbus has no timeout of its own, so before `connection.request_timeout_s` existed a
/// request to such a peer never returned — which is exactly what a half-open socket looks like
/// after the device (or its container) vanished without sending a RST. The await happened inside
/// the runtime's poll loop, so the whole connector wedged: no samples, no health, no link
/// status, nothing logged, and only a service restart recovered it.
#[tokio::test]
async fn silent_peer_fails_the_read_instead_of_hanging() {
    // Accept connections and hold them open without ever replying.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((stream, _)) = listener.accept().await {
            held.push(stream); // keep the socket open: no response, no close, no reset
        }
    });

    let toml = format!(
        r#"
        [connector]
        protocol = "modbus"

        [connection]
        request_timeout_s = 0.25

        [[device]]
        name = "plc-1"
        protocol_address = {{ transport = "tcp", host = "{}", port = {}, unit_id = 1 }}
        default_mode = "typed"

          [[device.point]]
          id = "first"
          datatype = "uint16"
          address = {{ table = "holding", address = 0, count = 1 }}

          [[device.point]]
          id = "second"
          datatype = "uint16"
          address = {{ table = "holding", address = 1, count = 1 }}
        "#,
        addr.ip(),
        addr.port()
    );
    let config: ConnectorConfig = toml::from_str(&toml).unwrap();
    let mut connector = ModbusConnector::default();
    connector.configure(&config).unwrap();
    // The TCP connect itself succeeds: the peer accepts, it just never answers a request.
    connector.connect().await.unwrap();

    let device = "plc-1".to_string();
    let points = vec![
        pref("first", Mode::Typed, Some(DataType::Uint16), Access::Read),
        pref("second", Mode::Typed, Some(DataType::Uint16), Access::Read),
    ];
    let started = std::time::Instant::now();
    let samples = connector.read_points(&device, &points).await.unwrap();
    let elapsed = started.elapsed();

    // One timeout bounds the batch: the second point is reported without waiting again.
    assert!(
        elapsed < std::time::Duration::from_secs(2),
        "read_points took {elapsed:?}, expected it to give up after ~250ms"
    );
    assert_eq!(samples.len(), 2);
    assert!(
        samples.iter().all(|s| s.quality == Quality::Bad),
        "every point of an unanswered batch must be bad: {samples:?}"
    );
    let first = samples[0].error.as_deref().unwrap_or_default();
    assert!(first.contains("timed out"), "unexpected error: {first}");
    let second = samples[1].error.as_deref().unwrap_or_default();
    assert!(second.contains("skipped"), "unexpected error: {second}");
}
