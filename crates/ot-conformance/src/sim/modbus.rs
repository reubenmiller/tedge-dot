//! Built-in Modbus TCP simulator (`kind = "modbus-tcp"`).
//!
//! A `tokio-modbus` server backed by seeded register/coil tables. Addresses listed under
//! `invalid` answer with a Modbus `IllegalDataAddress` exception — the hardware-free stand-in
//! for a flaky field device (B4). The outage switch makes *every* request fail with
//! `ServerDeviceFailure`, which is how the harness "drops" the device for B5.

use super::proxy::TransportProxy;
use super::{PointData, PointSpec, Simulator};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::future;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tedge_dot_sdk::{DataType, Mode};
use tokio_modbus::server::tcp::{accept_tcp_connection, Server};
use tokio_modbus::{ExceptionCode, Request, Response};
use tracing::debug;

/// Seed file: initial table contents plus addresses whose reads must fail.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Seed {
    /// Free-text description; ignored.
    #[serde(default, rename = "$comment")]
    _comment: Option<String>,
    #[serde(default)]
    holding: HashMap<String, u16>,
    #[serde(default)]
    input: HashMap<String, u16>,
    #[serde(default)]
    coils: HashMap<String, bool>,
    #[serde(default)]
    discrete_inputs: HashMap<String, bool>,
    #[serde(default)]
    invalid: Invalid,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Invalid {
    #[serde(default)]
    holding: Vec<u16>,
    #[serde(default)]
    input: Vec<u16>,
    #[serde(default)]
    coils: Vec<u16>,
    #[serde(default)]
    discrete_inputs: Vec<u16>,
}

#[derive(Debug, Default)]
struct State {
    holding: HashMap<u16, u16>,
    input: HashMap<u16, u16>,
    coils: HashMap<u16, bool>,
    discrete: HashMap<u16, bool>,
    invalid: HashSet<(&'static str, u16)>,
    /// Observed writes: (table, start address, number of values written).
    writes: Vec<(&'static str, u16, usize)>,
}

fn parse_addr_keys<V: Copy>(m: &HashMap<String, V>, what: &str) -> Result<HashMap<u16, V>, String> {
    m.iter()
        .map(|(k, v)| {
            k.parse::<u16>()
                .map(|a| (a, *v))
                .map_err(|_| format!("seed: {what} address '{k}' is not a u16"))
        })
        .collect()
}

impl State {
    fn from_seed(seed: Seed) -> Result<State, String> {
        let mut invalid = HashSet::new();
        for a in seed.invalid.holding {
            invalid.insert(("holding", a));
        }
        for a in seed.invalid.input {
            invalid.insert(("input", a));
        }
        for a in seed.invalid.coils {
            invalid.insert(("coil", a));
        }
        for a in seed.invalid.discrete_inputs {
            invalid.insert(("discrete_input", a));
        }
        Ok(State {
            holding: parse_addr_keys(&seed.holding, "holding")?,
            input: parse_addr_keys(&seed.input, "input")?,
            coils: parse_addr_keys(&seed.coils, "coils")?,
            discrete: parse_addr_keys(&seed.discrete_inputs, "discrete_inputs")?,
            invalid,
            writes: Vec::new(),
        })
    }

    fn read_registers(&self, table: &'static str, addr: u16, cnt: u16) -> Result<Vec<u16>, ExceptionCode> {
        let store = match table {
            "holding" => &self.holding,
            _ => &self.input,
        };
        (0..cnt)
            .map(|i| {
                let a = addr.wrapping_add(i);
                if self.invalid.contains(&(table, a)) {
                    Err(ExceptionCode::IllegalDataAddress)
                } else {
                    Ok(store.get(&a).copied().unwrap_or(0))
                }
            })
            .collect()
    }

    fn read_bits(&self, table: &'static str, addr: u16, cnt: u16) -> Result<Vec<bool>, ExceptionCode> {
        let store = match table {
            "coil" => &self.coils,
            _ => &self.discrete,
        };
        (0..cnt)
            .map(|i| {
                let a = addr.wrapping_add(i);
                if self.invalid.contains(&(table, a)) {
                    Err(ExceptionCode::IllegalDataAddress)
                } else {
                    Ok(store.get(&a).copied().unwrap_or(false))
                }
            })
            .collect()
    }
}

struct Service {
    state: Arc<Mutex<State>>,
    outage: Arc<AtomicBool>,
}

impl tokio_modbus::server::Service for Service {
    type Request = Request<'static>;
    type Response = Response;
    type Exception = ExceptionCode;
    type Future = future::Ready<Result<Response, ExceptionCode>>;

    fn call(&self, req: Self::Request) -> Self::Future {
        future::ready(self.handle(req))
    }
}

impl Service {
    fn handle(&self, req: Request<'static>) -> Result<Response, ExceptionCode> {
        if self.outage.load(Ordering::SeqCst) {
            return Err(ExceptionCode::ServerDeviceFailure);
        }
        let mut state = self.state.lock().unwrap();
        match req {
            Request::ReadHoldingRegisters(addr, cnt) => state
                .read_registers("holding", addr, cnt)
                .map(Response::ReadHoldingRegisters),
            Request::ReadInputRegisters(addr, cnt) => state
                .read_registers("input", addr, cnt)
                .map(Response::ReadInputRegisters),
            Request::ReadCoils(addr, cnt) => {
                state.read_bits("coil", addr, cnt).map(Response::ReadCoils)
            }
            Request::ReadDiscreteInputs(addr, cnt) => state
                .read_bits("discrete_input", addr, cnt)
                .map(Response::ReadDiscreteInputs),
            Request::WriteSingleRegister(addr, word) => {
                state.holding.insert(addr, word);
                state.writes.push(("holding", addr, 1));
                Ok(Response::WriteSingleRegister(addr, word))
            }
            Request::WriteMultipleRegisters(addr, words) => {
                for (i, w) in words.iter().enumerate() {
                    state.holding.insert(addr.wrapping_add(i as u16), *w);
                }
                state.writes.push(("holding", addr, words.len()));
                Ok(Response::WriteMultipleRegisters(addr, words.len() as u16))
            }
            Request::WriteSingleCoil(addr, on) => {
                state.coils.insert(addr, on);
                state.writes.push(("coil", addr, 1));
                Ok(Response::WriteSingleCoil(addr, on))
            }
            Request::WriteMultipleCoils(addr, coils) => {
                for (i, c) in coils.iter().enumerate() {
                    state.coils.insert(addr.wrapping_add(i as u16), *c);
                }
                state.writes.push(("coil", addr, coils.len()));
                Ok(Response::WriteMultipleCoils(addr, coils.len() as u16))
            }
            _ => Err(ExceptionCode::IllegalFunction),
        }
    }
}

/// The point `address` object of the Modbus connector spec (bitfield keys are ignored here;
/// the harness handles bit extraction when computing expected values).
#[derive(Debug, Deserialize)]
struct ModbusAddress {
    #[serde(default = "default_table")]
    table: String,
    address: u16,
    #[serde(default)]
    count: Option<u16>,
}

fn default_table() -> String {
    "holding".to_string()
}

impl ModbusAddress {
    fn parse(value: &serde_json::Value) -> Result<ModbusAddress, String> {
        serde_json::from_value(value.clone()).map_err(|e| format!("modbus point address: {e}"))
    }

    fn is_bit_table(&self) -> bool {
        matches!(self.table.as_str(), "coil" | "discrete_input")
    }

    fn table_key(&self) -> &'static str {
        match self.table.as_str() {
            "coil" => "coil",
            "discrete_input" => "discrete_input",
            "input" => "input",
            _ => "holding",
        }
    }

    /// Register count the connector will read: explicit `count`, else derived from the
    /// datatype (mirrors the Modbus connector spec).
    fn register_count(&self, datatype: Option<DataType>, mode: Mode) -> u16 {
        if let Some(c) = self.count {
            return c;
        }
        match mode {
            Mode::Raw => 1,
            Mode::Typed => datatype
                .and_then(DataType::byte_len)
                .map(|b| (b.div_ceil(2)).max(1) as u16)
                .unwrap_or(1),
        }
    }
}

pub struct ModbusSim {
    state: Arc<Mutex<State>>,
    outage: Arc<AtomicBool>,
    /// The connector talks to the proxy, never to the Modbus server directly, so transport
    /// drops can kill its live TCP session (see [`TransportProxy`]).
    proxy: TransportProxy,
    server_task: tokio::task::JoinHandle<()>,
}

impl Drop for ModbusSim {
    fn drop(&mut self) {
        self.server_task.abort();
    }
}

impl ModbusSim {
    pub async fn start(seed_path: &Path) -> Result<ModbusSim, String> {
        let text = std::fs::read_to_string(seed_path)
            .map_err(|e| format!("failed to read seed '{}': {e}", seed_path.display()))?;
        let seed: Seed = serde_json::from_str(&text)
            .map_err(|e| format!("failed to parse seed '{}': {e}", seed_path.display()))?;
        let state = Arc::new(Mutex::new(State::from_seed(seed)?));
        let outage = Arc::new(AtomicBool::new(false));

        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .map_err(|e| format!("simulator bind failed: {e}"))?;
        let port = listener
            .local_addr()
            .map_err(|e| format!("simulator local_addr: {e}"))?
            .port();

        let service_state = state.clone();
        let service_outage = outage.clone();
        let server_task = tokio::spawn(async move {
            let server = Server::new(listener);
            let new_service = |_addr| {
                Ok(Some(Arc::new(Service {
                    state: service_state.clone(),
                    outage: service_outage.clone(),
                })))
            };
            let on_connected = |stream, addr| async move { accept_tcp_connection(stream, addr, new_service) };
            if let Err(e) = server.serve(&on_connected, |e| debug!("modbus sim: {e}")).await {
                debug!("modbus sim server stopped: {e}");
            }
        });

        let proxy = TransportProxy::start(port).await?;
        Ok(ModbusSim {
            state,
            outage,
            proxy,
            server_task,
        })
    }
}

#[async_trait::async_trait]
impl Simulator for ModbusSim {
    fn port(&self) -> u16 {
        self.proxy.port()
    }

    fn point_data(&self, point: &PointSpec) -> Result<PointData, String> {
        let addr = ModbusAddress::parse(&point.address)?;
        let state = self.state.lock().unwrap();
        if addr.is_bit_table() {
            let cnt = addr.count.unwrap_or(1);
            let bits = state
                .read_bits(addr.table_key(), addr.address, cnt)
                .map_err(|e| format!("seeded invalid: {e:?}"))?;
            Ok(PointData {
                bytes: pack_bits(&bits),
                raw_group: 1,
            })
        } else {
            let cnt = addr.register_count(point.datatype, point.mode);
            let regs = state
                .read_registers(addr.table_key(), addr.address, cnt)
                .map_err(|e| format!("seeded invalid: {e:?}"))?;
            Ok(PointData {
                bytes: regs.iter().flat_map(|r| r.to_be_bytes()).collect(),
                raw_group: 2,
            })
        }
    }

    fn is_invalid(&self, point: &PointSpec) -> bool {
        let Ok(addr) = ModbusAddress::parse(&point.address) else {
            return false;
        };
        let cnt = if addr.is_bit_table() {
            addr.count.unwrap_or(1)
        } else {
            addr.register_count(point.datatype, point.mode)
        };
        let state = self.state.lock().unwrap();
        (0..cnt).any(|i| {
            state
                .invalid
                .contains(&(addr.table_key(), addr.address.wrapping_add(i)))
        })
    }

    fn write_count(&self, point: &PointSpec) -> Result<usize, String> {
        let addr = ModbusAddress::parse(&point.address)?;
        let state = self.state.lock().unwrap();
        Ok(state
            .writes
            .iter()
            .filter(|(table, a, _)| *table == addr.table_key() && *a == addr.address)
            .count())
    }

    fn set_outage(&self, on: bool) {
        self.outage.store(on, Ordering::SeqCst);
    }

    async fn set_transport(&self, up: bool) -> Result<(), String> {
        self.proxy.set_up(up).await
    }

    fn rewrite_protocol_address(&self, address: &mut toml::Value) -> Result<(), String> {
        let table = address
            .as_table_mut()
            .ok_or("device protocol_address is not a table")?;
        table.insert("transport".into(), toml::Value::String("tcp".into()));
        table.insert("host".into(), toml::Value::String("127.0.0.1".into()));
        table.insert("port".into(), toml::Value::Integer(self.proxy.port() as i64));
        table.entry("unit_id").or_insert(toml::Value::Integer(1));
        // serial-only keys would make a tcp address ambiguous
        table.remove("serial_port");
        Ok(())
    }
}

/// LSB-first bit packing, identical to the Modbus connector's raw coil serialization.
fn pack_bits(bits: &[bool]) -> Vec<u8> {
    if bits.is_empty() {
        return vec![0];
    }
    let mut out = vec![0u8; bits.len().div_ceil(8)];
    for (i, b) in bits.iter().enumerate() {
        if *b {
            out[i / 8] |= 1 << (i % 8);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_modbus::client::tcp;
    use tokio_modbus::prelude::{Reader, Writer};
    use tokio_modbus::Slave;

    fn write_seed(name: &str, contents: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("ot-conf-sim-{}-{name}", std::process::id()));
        std::fs::write(&path, contents).unwrap();
        path
    }

    #[tokio::test]
    async fn serves_seeded_data_exceptions_and_writes() {
        let seed = write_seed(
            "basic.json",
            r#"{
                "holding": { "3": 17001, "6": 16938, "7": 0 },
                "coils": { "48": true },
                "invalid": { "holding": [1] }
            }"#,
        );
        let sim = ModbusSim::start(&seed).await.unwrap();
        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], sim.port()));
        let mut ctx = tcp::connect_slave(addr, Slave(1)).await.unwrap();

        // seeded registers (unseeded addresses read as 0)
        let regs = ctx.read_holding_registers(3, 1).await.unwrap().unwrap();
        assert_eq!(regs, vec![17001]);
        let float_regs = ctx.read_holding_registers(6, 2).await.unwrap().unwrap();
        assert_eq!(float_regs, vec![16938, 0]);

        // seeded coil
        let coils = ctx.read_coils(48, 1).await.unwrap().unwrap();
        assert_eq!(coils, vec![true]);

        // invalid address answers with a modbus exception
        let exc = ctx.read_holding_registers(1, 1).await.unwrap();
        assert!(exc.is_err(), "expected exception, got {exc:?}");

        // writes land in the state and are observable
        ctx.write_single_register(3, 999).await.unwrap().unwrap();
        let spec = PointSpec {
            address: serde_json::json!({ "table": "holding", "address": 3, "count": 1 }),
            datatype: Some(DataType::Uint16),
            mode: Mode::Typed,
        };
        assert_eq!(sim.write_count(&spec).unwrap(), 1);
        assert_eq!(sim.point_data(&spec).unwrap().bytes, vec![0x03, 0xe7]);

        // outage: every request fails
        sim.set_outage(true);
        let out = ctx.read_holding_registers(3, 1).await.unwrap();
        assert!(out.is_err(), "expected outage exception, got {out:?}");
        sim.set_outage(false);
        let back = ctx.read_holding_registers(3, 1).await.unwrap().unwrap();
        assert_eq!(back, vec![999]);

        std::fs::remove_file(&seed).ok();
    }

    #[test]
    fn point_data_derives_register_count_from_datatype() {
        let addr = ModbusAddress {
            table: "holding".into(),
            address: 6,
            count: None,
        };
        assert_eq!(addr.register_count(Some(DataType::Float32), Mode::Typed), 2);
        assert_eq!(addr.register_count(Some(DataType::Float64), Mode::Typed), 4);
        assert_eq!(addr.register_count(None, Mode::Raw), 1);
    }
}
