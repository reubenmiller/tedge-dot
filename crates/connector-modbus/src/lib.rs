//! Reference Modbus connector module (TCP + RTU).
//!
//! Implements the [`Connector`](tedge_dot_sdk::Connector) trait against
//! [`tokio-modbus`]. The driver is intentionally "dumb": it does transport, contiguous reads,
//! and primitive decode via the SDK helpers only. All scaling, renaming, units, alarms and
//! thin-edge JSON shaping are handled by thin-edge.io flows.

mod config;

pub use config::{ModbusAddress, ModbusConnection, ProtocolAddress, SerialDefaults, Table};

use async_trait::async_trait;
use std::collections::HashMap;
use tedge_dot_sdk::{
    decode_primitive, encode_primitive, extract_bitfield, Access, Capabilities, CommandRequest,
    CommandResult, ConfigError, Connector, ConnectorConfig, ConnectorError, DataType, DeviceId,
    Endianness, LinkReport, LinkStatus, Mode, PointRef, Quality, Sample, Transform, Value,
    WordOrder,
};
use time::OffsetDateTime;
use tokio_modbus::client::{rtu, tcp, Context};
use tokio_modbus::prelude::{Reader, Writer};
use tokio_modbus::Slave;
use tokio_serial::SerialStream;

const PROTOCOL: &str = "modbus";

/// A fully-resolved Modbus point (address + decode parameters), built in `configure`.
#[derive(Clone)]
struct ModbusPoint {
    address: ModbusAddress,
    mode: Mode,
    datatype: Option<DataType>,
    endianness: Endianness,
    word_order: WordOrder,
    access: Access,
    unit: Option<String>,
    transform: Transform,
}

struct DeviceModel {
    address: ProtocolAddress,
    points: HashMap<String, ModbusPoint>,
}

/// The Modbus connector. One instance manages all configured Modbus devices.
#[derive(Default)]
pub struct ModbusConnector {
    serial: SerialDefaults,
    devices: HashMap<String, DeviceModel>,
    contexts: HashMap<String, Context>,
}

/// Factory used by the binary to instantiate the module behind its feature flag.
pub fn factory() -> Box<dyn Connector> {
    Box::<ModbusConnector>::default()
}

#[async_trait]
impl Connector for ModbusConnector {
    fn configure(&mut self, config: &ConnectorConfig) -> Result<(), ConfigError> {
        let conn: ModbusConnection =
            serde_json::from_value(config.connection.clone()).unwrap_or_default();
        self.serial = conn.serial;
        self.devices.clear();

        for d in &config.devices {
            let address: ProtocolAddress = serde_json::from_value(d.protocol_address.clone())
                .map_err(|e| {
                    ConfigError::Invalid(format!("device '{}' protocol_address: {e}", d.name))
                })?;

            let mut points = HashMap::new();
            for p in &d.points {
                let addr: ModbusAddress = serde_json::from_value(p.address.clone())
                    .map_err(|e| {
                        ConfigError::Invalid(format!("point '{}' address: {e}", p.id))
                    })?;
                let mode = p.resolved_mode(d.default_mode);
                if mode == Mode::Typed && p.datatype.is_none() && !addr.table.is_bit() {
                    return Err(ConfigError::Invalid(format!(
                        "point '{}' is typed but has no datatype",
                        p.id
                    )));
                }
                if matches!(p.access.as_deref(), Some("write") | Some("read_write"))
                    && !addr.table.is_writable()
                {
                    return Err(ConfigError::Invalid(format!(
                        "point '{}' is writable but table {:?} is read-only",
                        p.id, addr.table
                    )));
                }
                points.insert(
                    p.id.clone(),
                    ModbusPoint {
                        address: addr,
                        mode,
                        datatype: p.datatype,
                        endianness: Endianness::parse(p.endianness.as_deref()),
                        word_order: WordOrder::parse(p.word_order.as_deref()),
                        access: Access::parse(p.access.as_deref()),
                        unit: p.unit.clone(),
                        transform: p.transform.unwrap_or_default(),
                    },
                );
            }
            self.devices
                .insert(d.name.clone(), DeviceModel { address, points });
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
                DataType::Int16,
                DataType::Uint16,
                DataType::Int32,
                DataType::Uint32,
                DataType::Int64,
                DataType::Uint64,
                DataType::Float32,
                DataType::Float64,
            ],
            point_kinds: vec![
                "coil".into(),
                "discrete_input".into(),
                "holding_register".into(),
                "input_register".into(),
            ],
            command_verbs: vec!["write".into()],
            features: vec!["polling".into(), "bitfield".into()],
            subscribe: false,
        }
    }

    async fn connect(&mut self) -> Result<Vec<LinkReport>, ConnectorError> {
        let names: Vec<String> = self.devices.keys().cloned().collect();
        let mut reports = Vec::new();
        for name in names {
            let address = self.devices[&name].address.clone();
            match build_context(&address, &self.serial).await {
                Ok(ctx) => {
                    self.contexts.insert(name.clone(), ctx);
                    reports.push(LinkReport {
                        device: name,
                        status: LinkStatus::Connected,
                        reason: None,
                        info: Some(device_descriptor(&address)),
                    });
                }
                Err(e) => {
                    reports.push(LinkReport {
                        device: name,
                        status: LinkStatus::Disconnected,
                        reason: Some(e),
                        info: Some(device_descriptor(&address)),
                    });
                }
            }
        }
        Ok(reports)
    }

    async fn read_points(
        &mut self,
        device: &DeviceId,
        points: &[PointRef],
    ) -> Result<Vec<Sample>, ConnectorError> {
        // Resolve unit id and per-point models, ending the immutable borrow on `self.devices`
        // before borrowing `self.contexts` mutably.
        let (unit_id, models): (u8, Vec<(String, Option<ModbusPoint>)>) =
            match self.devices.get(device) {
                Some(dev) => (
                    dev.address.unit_id(),
                    points
                        .iter()
                        .map(|p| (p.id.clone(), dev.points.get(&p.id).cloned()))
                        .collect(),
                ),
                None => (0, points.iter().map(|p| (p.id.clone(), None)).collect()),
            };

        let ctx = match self.contexts.get_mut(device) {
            Some(c) => c,
            None => {
                return Ok(models
                    .into_iter()
                    .map(|(id, model)| {
                        let group = model
                            .as_ref()
                            .map(|m| if m.address.table.is_bit() { 1 } else { 2 })
                            .unwrap_or(2);
                        bad_sample(
                            &id,
                            model.as_ref().map(|m| &m.address),
                            unit_id,
                            group,
                            "device not connected",
                        )
                    })
                    .collect());
            }
        };

        let mut out = Vec::with_capacity(models.len());
        for (id, model) in models {
            let model = match model {
                Some(m) => m,
                None => {
                    out.push(bad_sample(&id, None, unit_id, 2, "unknown point"));
                    continue;
                }
            };
            out.push(read_one(ctx, &id, &model, unit_id).await);
        }
        Ok(out)
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

        if !model.access.can_write() || !model.address.table.is_writable() {
            return Err(ConnectorError::AccessDenied(request.point.clone()));
        }

        let ctx = self
            .contexts
            .get_mut(device)
            .ok_or_else(|| ConnectorError::NotConnected(device.clone()))?;

        write_point(ctx, &model, request).await?;
        Ok(CommandResult {
            point: request.point.clone(),
            value: request.value.clone(),
            raw: request.raw.clone(),
        })
    }

    async fn disconnect(&mut self) -> Result<(), ConnectorError> {
        self.contexts.clear();
        Ok(())
    }
}

/// Read a single point and build its sample.
async fn read_one(ctx: &mut Context, id: &str, model: &ModbusPoint, unit_id: u8) -> Sample {
    let addr = &model.address;
    if addr.table.is_bit() {
        let count = addr.count.unwrap_or(1);
        match read_bits(ctx, addr.table, addr.address, count).await {
            Ok(bits) => {
                let raw = pack_bits(&bits);
                let value = match model.mode {
                    Mode::Raw => None,
                    Mode::Typed => Some(Value::Bool(bits.first().copied().unwrap_or(false))),
                };
                good_sample(id, model, unit_id, raw, 1, value)
            }
            Err(e) => bad_sample(id, Some(addr), unit_id, 1, &e),
        }
    } else {
        let count = register_count(addr, model);
        match read_registers(ctx, addr.table, addr.address, count).await {
            Ok(regs) => {
                let bytes: Vec<u8> = regs.iter().flat_map(|r| r.to_be_bytes()).collect();
                match model.mode {
                    Mode::Raw => good_sample(id, model, unit_id, bytes, 2, None),
                    Mode::Typed => {
                        let dt = match model.datatype {
                            Some(dt) => dt,
                            None => {
                                return bad_sample(
                                    id,
                                    Some(addr),
                                    unit_id,
                                    2,
                                    "typed point missing datatype",
                                )
                            }
                        };
                        let value = if let (Some(sb), Some(bc)) =
                            (addr.start_bit, addr.bit_count)
                        {
                            let n = extract_bitfield(
                                &bytes,
                                model.endianness,
                                model.word_order,
                                sb,
                                bc,
                            );
                            Value::Number(n as f64)
                        } else {
                            match decode_primitive(
                                &bytes,
                                dt,
                                model.endianness,
                                model.word_order,
                            ) {
                                Ok(v) => v,
                                Err(e) => {
                                    return bad_sample(
                                        id,
                                        Some(addr),
                                        unit_id,
                                        2,
                                        &format!("decode error: {e}"),
                                    )
                                }
                            }
                        };
                        good_sample(id, model, unit_id, bytes, 2, Some(value))
                    }
                }
            }
            Err(e) => bad_sample(id, Some(addr), unit_id, 2, &e),
        }
    }
}

/// Encode and write a point per the command request.
async fn write_point(
    ctx: &mut Context,
    model: &ModbusPoint,
    request: &CommandRequest,
) -> Result<(), ConnectorError> {
    let addr = &model.address;

    // Coil write.
    if addr.table == Table::Coil {
        let on = if let Some(raw) = &request.raw {
            let bytes = parse_hex(raw)?;
            bytes.first().copied().unwrap_or(0) != 0
        } else {
            match request.value.as_ref().and_then(json_to_value) {
                Some(Value::Bool(b)) => b,
                Some(Value::Number(n)) => n != 0.0,
                _ => return Err(ConnectorError::Decode("coil write needs a boolean".into())),
            }
        };
        return flatten_write(ctx.write_single_coil(addr.address, on).await);
    }

    // Holding register write.
    let regs: Vec<u16> = if let Some(raw) = &request.raw {
        let bytes = parse_hex(raw)?;
        bytes_to_registers(&bytes)
    } else {
        let dt = model
            .datatype
            .ok_or_else(|| ConnectorError::Decode("typed write needs a datatype".into()))?;
        let value = request
            .value
            .as_ref()
            .and_then(json_to_value)
            .ok_or_else(|| ConnectorError::Decode("missing or invalid value".into()))?;

        // Bit-field write: read-modify-write a single register.
        if let (Some(sb), Some(bc)) = (addr.start_bit, addr.bit_count) {
            let current = read_registers(ctx, Table::Holding, addr.address, 1)
                .await
                .map_err(ConnectorError::Transport)?;
            let cur = current.first().copied().unwrap_or(0);
            let field = match &value {
                Value::Number(n) => *n as u64,
                Value::Bool(b) => *b as u64,
                Value::Text(t) => t.parse::<u64>().unwrap_or(0),
            };
            let mask: u32 = ((1u32 << bc) - 1) << sb;
            let merged = (cur as u32 & !mask) | ((field as u32) << sb & mask);
            vec![merged as u16]
        } else {
            let bytes =
                encode_primitive(&value, dt, model.endianness, model.word_order)
                    .map_err(|e| ConnectorError::Decode(e.to_string()))?;
            bytes_to_registers(&bytes)
        }
    };

    if regs.len() == 1 {
        flatten_write(ctx.write_single_register(addr.address, regs[0]).await)
    } else {
        flatten_write(ctx.write_multiple_registers(addr.address, &regs).await)
    }
}

// ---- transport helpers ----

async fn build_context(
    address: &ProtocolAddress,
    serial_defaults: &SerialDefaults,
) -> Result<Context, String> {
    match address {
        ProtocolAddress::Tcp {
            host,
            port,
            unit_id,
        } => {
            let socket = tokio::net::lookup_host((host.as_str(), *port))
                .await
                .map_err(|e| format!("dns lookup '{host}:{port}' failed: {e}"))?
                .next()
                .ok_or_else(|| format!("no address resolved for '{host}:{port}'"))?;
            tcp::connect_slave(socket, Slave(*unit_id))
                .await
                .map_err(|e| format!("tcp connect failed: {e}"))
        }
        ProtocolAddress::Rtu {
            serial_port,
            unit_id,
            baudrate,
            parity,
            stopbits,
            databits,
        } => {
            let baud = baudrate.unwrap_or(serial_defaults.baudrate);
            let parity = parity.clone().unwrap_or_else(|| serial_defaults.parity.clone());
            let stopbits = stopbits.unwrap_or(serial_defaults.stopbits);
            let databits = databits.unwrap_or(serial_defaults.databits);
            let builder = tokio_serial::new(serial_port, baud)
                .parity(parity_from(&parity))
                .stop_bits(stopbits_from(stopbits))
                .data_bits(databits_from(databits));
            let stream = SerialStream::open(&builder)
                .map_err(|e| format!("open serial port '{serial_port}' failed: {e}"))?;
            Ok(rtu::attach_slave(stream, Slave(*unit_id)))
        }
    }
}

async fn read_registers(
    ctx: &mut Context,
    table: Table,
    addr: u16,
    cnt: u16,
) -> Result<Vec<u16>, String> {
    let result = match table {
        Table::Holding => ctx.read_holding_registers(addr, cnt).await,
        Table::Input => ctx.read_input_registers(addr, cnt).await,
        _ => return Err("read_registers called on a bit table".into()),
    };
    match result {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(exc)) => Err(format!("modbus exception: {exc}")),
        Err(e) => Err(format!("modbus error: {e}")),
    }
}

async fn read_bits(
    ctx: &mut Context,
    table: Table,
    addr: u16,
    cnt: u16,
) -> Result<Vec<bool>, String> {
    let result = match table {
        Table::Coil => ctx.read_coils(addr, cnt).await,
        Table::DiscreteInput => ctx.read_discrete_inputs(addr, cnt).await,
        _ => return Err("read_bits called on a register table".into()),
    };
    match result {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(exc)) => Err(format!("modbus exception: {exc}")),
        Err(e) => Err(format!("modbus error: {e}")),
    }
}

fn flatten_write(
    result: Result<Result<(), tokio_modbus::ExceptionCode>, tokio_modbus::Error>,
) -> Result<(), ConnectorError> {
    match result {
        Ok(Ok(())) => Ok(()),
        Ok(Err(exc)) => Err(ConnectorError::Transport(format!("modbus exception: {exc}"))),
        Err(e) => Err(ConnectorError::Transport(format!("modbus error: {e}"))),
    }
}

// ---- sample builders ----

fn good_sample(
    id: &str,
    model: &ModbusPoint,
    unit_id: u8,
    raw: Vec<u8>,
    group: usize,
    value: Option<Value>,
) -> Sample {
    Sample {
        ts: OffsetDateTime::now_utc(),
        device: String::new(), // filled in by the SDK from the topic context if needed
        protocol: PROTOCOL,
        point: id.to_string(),
        mode: model.mode,
        datatype: if model.mode == Mode::Typed {
            model.datatype.or(Some(DataType::Bool))
        } else {
            None
        },
        value: value.map(|v| model.transform.apply(v)),
        raw,
        raw_group: group,
        quality: Quality::Good,
        unit: model.unit.clone(),
        addr: addr_echo(Some(&model.address), unit_id),
        seq: None,
        error: None,
    }
}

fn bad_sample(
    id: &str,
    addr: Option<&ModbusAddress>,
    unit_id: u8,
    group: usize,
    error: &str,
) -> Sample {
    Sample {
        ts: OffsetDateTime::now_utc(),
        device: String::new(),
        protocol: PROTOCOL,
        point: id.to_string(),
        mode: Mode::Typed,
        datatype: None,
        value: None,
        raw: Vec::new(),
        raw_group: group,
        quality: Quality::Bad,
        unit: None,
        addr: addr_echo(addr, unit_id),
        seq: None,
        error: Some(error.to_string()),
    }
}

fn addr_echo(addr: Option<&ModbusAddress>, unit_id: u8) -> serde_json::Value {
    match addr {
        Some(a) => serde_json::json!({
            "table": table_str(a.table),
            "address": a.address,
            "unit_id": unit_id,
        }),
        None => serde_json::json!({ "unit_id": unit_id }),
    }
}

fn table_str(t: Table) -> &'static str {
    match t {
        Table::Coil => "coil",
        Table::DiscreteInput => "discrete_input",
        Table::Holding => "holding",
        Table::Input => "input",
    }
}

/// Build a device descriptor for the link status payload (transport + address details).
/// Flows forward this into a digital-twin fragment (parity with the legacy c8y_ModbusDevice).
fn device_descriptor(address: &ProtocolAddress) -> serde_json::Value {
    match address {
        ProtocolAddress::Tcp {
            host,
            port,
            unit_id,
        } => serde_json::json!({
            "protocol": PROTOCOL,
            "transport": "tcp",
            "host": host,
            "port": port,
            "unit_id": unit_id,
        }),
        ProtocolAddress::Rtu {
            serial_port,
            unit_id,
            baudrate,
            parity,
            stopbits,
            databits,
        } => serde_json::json!({
            "protocol": PROTOCOL,
            "transport": "rtu",
            "serial_port": serial_port,
            "unit_id": unit_id,
            "baudrate": baudrate,
            "parity": parity,
            "stopbits": stopbits,
            "databits": databits,
        }),
    }
}

// ---- small helpers ----

/// Number of 16-bit registers to read for a register point.
fn register_count(addr: &ModbusAddress, model: &ModbusPoint) -> u16 {
    if let Some(c) = addr.count {
        return c;
    }
    match model.mode {
        Mode::Raw => 1,
        Mode::Typed => model
            .datatype
            .and_then(|dt| dt.byte_len())
            .map(|bytes| bytes.div_ceil(2).max(1) as u16)
            .unwrap_or(1),
    }
}

fn bytes_to_registers(bytes: &[u8]) -> Vec<u16> {
    bytes
        .chunks(2)
        .map(|c| {
            let hi = c[0];
            let lo = c.get(1).copied().unwrap_or(0);
            u16::from_be_bytes([hi, lo])
        })
        .collect()
}

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

fn parse_hex(s: &str) -> Result<Vec<u8>, ConnectorError> {
    let cleaned: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    if !cleaned.len().is_multiple_of(2) {
        return Err(ConnectorError::Decode("hex string has odd length".into()));
    }
    (0..cleaned.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&cleaned[i..i + 2], 16)
                .map_err(|e| ConnectorError::Decode(format!("invalid hex: {e}")))
        })
        .collect()
}

fn json_to_value(v: &serde_json::Value) -> Option<Value> {
    match v {
        serde_json::Value::Bool(b) => Some(Value::Bool(*b)),
        serde_json::Value::Number(n) => n.as_f64().map(Value::Number),
        serde_json::Value::String(s) => Some(Value::Text(s.clone())),
        _ => None,
    }
}

fn parity_from(s: &str) -> tokio_serial::Parity {
    match s.to_ascii_uppercase().as_str() {
        "E" => tokio_serial::Parity::Even,
        "O" => tokio_serial::Parity::Odd,
        _ => tokio_serial::Parity::None,
    }
}

fn stopbits_from(n: u8) -> tokio_serial::StopBits {
    if n == 1 {
        tokio_serial::StopBits::One
    } else {
        tokio_serial::StopBits::Two
    }
}

fn databits_from(n: u8) -> tokio_serial::DataBits {
    match n {
        5 => tokio_serial::DataBits::Five,
        6 => tokio_serial::DataBits::Six,
        7 => tokio_serial::DataBits::Seven,
        _ => tokio_serial::DataBits::Eight,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn point(table: Table, datatype: Option<DataType>, mode: Mode) -> ModbusPoint {
        ModbusPoint {
            address: ModbusAddress {
                table,
                address: 0,
                count: None,
                start_bit: None,
                bit_count: None,
            },
            mode,
            datatype,
            endianness: Endianness::Big,
            word_order: WordOrder::Big,
            access: Access::Read,
            unit: None,
            transform: Transform::default(),
        }
    }

    #[test]
    fn register_count_from_datatype() {
        assert_eq!(register_count(&point(Table::Holding, Some(DataType::Uint16), Mode::Typed).address, &point(Table::Holding, Some(DataType::Uint16), Mode::Typed)), 1);
        assert_eq!(register_count(&point(Table::Holding, Some(DataType::Float32), Mode::Typed).address, &point(Table::Holding, Some(DataType::Float32), Mode::Typed)), 2);
        assert_eq!(register_count(&point(Table::Holding, Some(DataType::Float64), Mode::Typed).address, &point(Table::Holding, Some(DataType::Float64), Mode::Typed)), 4);
    }

    #[test]
    fn bytes_to_registers_roundtrip() {
        assert_eq!(bytes_to_registers(&[0x42, 0x2a, 0x00, 0x00]), vec![0x422a, 0x0000]);
    }

    #[test]
    fn hex_parse() {
        assert_eq!(parse_hex("422a 0000").unwrap(), vec![0x42, 0x2a, 0x00, 0x00]);
    }

    #[test]
    fn pack_single_coil() {
        assert_eq!(pack_bits(&[true]), vec![1]);
        assert_eq!(pack_bits(&[false]), vec![0]);
    }
}
