//! PROFIBUS-DP connector module.
//!
//! Implements the [`Connector`](tedge_dot_sdk::Connector) trait using the
//! [`profirust`] crate as the underlying PROFIBUS stack.
//!
//! # Architecture
//!
//! PROFIBUS requires a tight, deterministic polling loop (`fdl.poll(now, phy, dp_master)`)
//! that cannot be run inside a Tokio task — cooperative scheduling introduces latency
//! spikes that violate FDL timing constraints.  This connector therefore spawns a
//! dedicated OS thread (the "bus thread") for the polling loop.  The thread communicates
//! with the async `Connector` methods through an `Arc<Mutex<SharedBusState>>`.
//!
//! ```text
//! ┌─────────────────────────────────────────┐
//! │  tokio runtime (SDK runtime::run)        │
//! │  ProfibusConnector                       │
//! │    read_points() ──────────────────────► │   Arc<Mutex<SharedBusState>>
//! │    execute()     ──────────────────────► │◄──────────────────────┐
//! │    connect()     spawns bus thread ─────►│                       │
//! └─────────────────────────────────────────┘             ┌──────────┴─────────┐
//!                                                          │  Bus thread         │
//!                                                          │  fdl.poll() loop    │
//!                                                          │  copies PI_I/PI_Q   │
//!                                                          └────────────────────┘
//! ```

mod config;

pub use config::{BusConnection, Direction, PeripheralAddress, PointAddress};

use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tedge_dot_sdk::{
    decode_primitive, encode_primitive, extract_bitfield, Access, Capabilities, CommandRequest,
    CommandResult, ConfigError, Connector, ConnectorConfig, ConnectorError, DataType, DeviceId,
    Endianness, LinkReport, LinkStatus, Mode, PointRef, Quality, Sample, Transform, Value,
    WordOrder,
};
use time::OffsetDateTime;
use tracing::{debug, error, info, warn};

const PROTOCOL: &str = "profibus";

// ── per-point internal model ─────────────────────────────────────────────────

#[derive(Clone)]
struct ProfibusPoint {
    address: PointAddress,
    mode: Mode,
    datatype: Option<DataType>,
    endianness: Endianness,
    word_order: WordOrder,
    access: Access,
    unit: Option<String>,
    transform: Transform,
}

struct DeviceModel {
    address: PeripheralAddress,
    points: HashMap<String, ProfibusPoint>,
}

// ── shared state between bus thread and async connector ──────────────────────

#[derive(Clone, Default)]
enum PeripheralLinkState {
    #[default]
    Unknown,
    Connected,
    Fault(String),
}

#[derive(Default)]
struct PeripheralState {
    /// Latest PI_I snapshot (input bytes from peripheral).
    inputs: Vec<u8>,
    /// Pending PI_Q (output bytes to write on the next cycle).
    outputs: Vec<u8>,
    link: PeripheralLinkState,
}

struct SharedBusState {
    peripherals: HashMap<String, PeripheralState>,
    /// Number of DP cycles completed. Used for initial-ready detection.
    cycle_count: u64,
}

// ── configuration handed to the bus thread ───────────────────────────────────

struct DeviceBusConfig {
    name: String,
    station_address: u8,
    ident_number: u16,
    max_tsdr: u16,
    config_bytes: Vec<u8>,
    param_bytes: Vec<u8>,
    input_bytes: usize,
    output_bytes: usize,
}

// ── connector struct ─────────────────────────────────────────────────────────

/// The PROFIBUS-DP connector.  One instance manages all configured peripherals
/// on a single RS-485 bus.
#[derive(Default)]
pub struct ProfibusConnector {
    connection: BusConnection,
    devices: HashMap<String, DeviceModel>,
    shared_state: Option<Arc<Mutex<SharedBusState>>>,
    bus_thread: Option<std::thread::JoinHandle<()>>,
    stop_flag: Option<Arc<AtomicBool>>,
}

/// Factory used by the binary to instantiate the module behind its feature flag.
pub fn factory() -> Box<dyn Connector> {
    Box::<ProfibusConnector>::default()
}

#[async_trait]
impl Connector for ProfibusConnector {
    fn configure(&mut self, config: &ConnectorConfig) -> Result<(), ConfigError> {
        let conn: BusConnection = serde_json::from_value(config.connection.clone())
            .map_err(|e| ConfigError::Invalid(format!("connection: {e}")))?;
        self.connection = conn;
        self.devices.clear();

        for d in &config.devices {
            let addr: PeripheralAddress =
                serde_json::from_value(d.protocol_address.clone()).map_err(|e| {
                    ConfigError::Invalid(format!(
                        "device '{}' protocol_address: {e}",
                        d.name
                    ))
                })?;

            if addr.station_address == 0 || addr.station_address > 125 {
                return Err(ConfigError::Invalid(format!(
                    "device '{}' station_address {} is out of range (1–125)",
                    d.name, addr.station_address
                )));
            }

            let mut points = HashMap::new();
            for p in &d.points {
                let point_addr: PointAddress =
                    serde_json::from_value(p.address.clone()).map_err(|e| {
                        ConfigError::Invalid(format!("point '{}' address: {e}", p.id))
                    })?;

                let mode = p.resolved_mode(d.default_mode);
                if mode == Mode::Typed && p.datatype.is_none() && point_addr.bit_offset.is_none() {
                    return Err(ConfigError::Invalid(format!(
                        "point '{}' is typed but has no datatype",
                        p.id
                    )));
                }
                let access = Access::parse(p.access.as_deref());
                if access.can_write() && point_addr.is_input() {
                    return Err(ConfigError::Invalid(format!(
                        "point '{}' is writable but direction is 'input'",
                        p.id
                    )));
                }
                points.insert(
                    p.id.clone(),
                    ProfibusPoint {
                        address: point_addr,
                        mode,
                        datatype: p.datatype,
                        endianness: Endianness::parse(p.endianness.as_deref()),
                        word_order: WordOrder::parse(p.word_order.as_deref()),
                        access,
                        unit: p.unit.clone(),
                        transform: p.transform.unwrap_or_default(),
                    },
                );
            }
            self.devices
                .insert(d.name.clone(), DeviceModel { address: addr, points });
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
                DataType::Float32,
            ],
            point_kinds: vec!["input".into(), "output".into()],
            command_verbs: vec!["write".into()],
            features: vec!["polling".into(), "bitfield".into()],
            subscribe: false,
        }
    }

    async fn connect(&mut self) -> Result<Vec<LinkReport>, ConnectorError> {
        self.stop_bus_thread();

        let device_configs: Vec<DeviceBusConfig> = self
            .devices
            .iter()
            .map(|(name, dev)| DeviceBusConfig {
                name: name.clone(),
                station_address: dev.address.station_address,
                ident_number: dev.address.ident_number,
                max_tsdr: dev.address.max_tsdr,
                config_bytes: dev.address.config_bytes.clone(),
                param_bytes: dev.address.param_bytes.clone(),
                input_bytes: dev.address.input_bytes,
                output_bytes: dev.address.output_bytes,
            })
            .collect();

        // Initialise shared state with zeroed I/O buffers.
        let mut peripherals = HashMap::new();
        for dc in &device_configs {
            peripherals.insert(
                dc.name.clone(),
                PeripheralState {
                    inputs: vec![0u8; dc.input_bytes],
                    outputs: vec![0u8; dc.output_bytes],
                    link: PeripheralLinkState::Unknown,
                },
            );
        }
        let shared = Arc::new(Mutex::new(SharedBusState {
            peripherals,
            cycle_count: 0,
        }));
        let stop_flag = Arc::new(AtomicBool::new(false));

        let shared_clone = Arc::clone(&shared);
        let stop_clone = Arc::clone(&stop_flag);
        let connection = self.connection.clone();

        // One-shot channel: bus thread sends initial link reports after first cycle.
        let (ready_tx, ready_rx) =
            std::sync::mpsc::channel::<Result<Vec<LinkReport>, String>>();

        let handle = std::thread::Builder::new()
            .name("profibus-bus".to_string())
            .spawn(move || {
                bus_thread_main(device_configs, connection, shared_clone, stop_clone, ready_tx);
            })
            .map_err(|e| {
                ConnectorError::Transport(format!("failed to spawn bus thread: {e}"))
            })?;

        self.shared_state = Some(Arc::clone(&shared));
        self.bus_thread = Some(handle);
        self.stop_flag = Some(stop_flag);

        // Wait up to 30 s for the bus thread to complete its first DP cycle.
        let reports = tokio::task::spawn_blocking(move || {
            ready_rx
                .recv_timeout(std::time::Duration::from_secs(30))
                .unwrap_or_else(|_| {
                    Err("bus did not complete a cycle within 30 s (check port and wiring)".into())
                })
        })
        .await
        .map_err(|e| ConnectorError::Transport(format!("bus readiness wait failed: {e}")))?;

        match reports {
            Ok(r) => Ok(r),
            Err(msg) => Ok(self.all_disconnected_reports(&msg)),
        }
    }

    async fn read_points(
        &mut self,
        device: &DeviceId,
        points: &[PointRef],
    ) -> Result<Vec<Sample>, ConnectorError> {
        let models: Vec<(String, Option<ProfibusPoint>)> = match self.devices.get(device) {
            Some(dev) => points
                .iter()
                .map(|p| (p.id.clone(), dev.points.get(&p.id).cloned()))
                .collect(),
            None => {
                return Ok(points
                    .iter()
                    .map(|p| bad_sample(&p.id, "unknown device", None, None))
                    .collect());
            }
        };

        let shared = self.shared_state.as_ref().map(Arc::clone);

        let mut out = Vec::with_capacity(models.len());
        for (id, model) in models {
            let model = match model {
                Some(m) => m,
                None => {
                    out.push(bad_sample(&id, "unknown point", None, None));
                    continue;
                }
            };

            if !model.address.is_input() {
                out.push(bad_sample(&id, "point direction is output", None, None));
                continue;
            }

            let shared = match &shared {
                Some(s) => s,
                None => {
                    out.push(bad_sample(&id, "not connected", None, None));
                    continue;
                }
            };

            let state = shared.lock().unwrap();
            let ps = match state.peripherals.get(device) {
                Some(p) => p,
                None => {
                    out.push(bad_sample(&id, "device not in shared state", None, None));
                    continue;
                }
            };

            if !matches!(ps.link, PeripheralLinkState::Connected) {
                let reason = match &ps.link {
                    PeripheralLinkState::Fault(msg) => format!("peripheral fault: {msg}"),
                    PeripheralLinkState::Unknown => "peripheral not yet in data exchange".into(),
                    PeripheralLinkState::Connected => unreachable!(),
                };
                out.push(bad_sample(&id, &reason, None, None));
                continue;
            }

            out.push(decode_point(&id, &model, &ps.inputs));
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

        if !model.access.can_write() {
            return Err(ConnectorError::AccessDenied(request.point.clone()));
        }
        if model.address.is_input() {
            return Err(ConnectorError::AccessDenied(format!(
                "point '{}' direction is input",
                request.point
            )));
        }

        let shared = self
            .shared_state
            .as_ref()
            .ok_or_else(|| ConnectorError::NotConnected(device.clone()))?;

        let mut state = shared.lock().unwrap();
        let pstate = state
            .peripherals
            .get_mut(device)
            .ok_or_else(|| ConnectorError::NotConnected(device.clone()))?;

        encode_point(&model, &mut pstate.outputs, request)
            .map_err(ConnectorError::Decode)?;

        Ok(CommandResult {
            point: request.point.clone(),
            value: request.value.clone(),
            raw: request.raw.clone(),
        })
    }

    async fn disconnect(&mut self) -> Result<(), ConnectorError> {
        self.stop_bus_thread();
        self.shared_state = None;
        Ok(())
    }
}

impl ProfibusConnector {
    fn stop_bus_thread(&mut self) {
        if let Some(flag) = &self.stop_flag {
            flag.store(true, Ordering::Relaxed);
        }
        if let Some(handle) = self.bus_thread.take() {
            if let Err(e) = handle.join() {
                error!("bus thread panicked: {e:?}");
            }
        }
        self.stop_flag = None;
    }

    fn all_disconnected_reports(&self, reason: &str) -> Vec<LinkReport> {
        self.devices
            .keys()
            .map(|name| {
                LinkReport::new(name.clone(), LinkStatus::Disconnected, Some(reason.to_string()))
            })
            .collect()
    }
}

// ── bus thread ───────────────────────────────────────────────────────────────

/// Heap-allocated, leaked I/O / config slices for a single peripheral.
///
/// # Safety
/// Each field is a raw pointer to a `Box<[u8]>` that has been leaked.
/// The pointers are valid for the lifetime of the bus thread. They are
/// exclusively owned by the bus thread — no other code aliases them.
/// They are freed by reconstructing the `Box` when the thread exits.
struct LeakedBufs {
    ibuf: *mut [u8],
    obuf: *mut [u8],
    cfg: *mut [u8],
    prm: *mut [u8],
}

// SAFETY: LeakedBufs is only ever created and used on the bus thread.
unsafe impl Send for LeakedBufs {}

// ── PHY dispatch ─────────────────────────────────────────────────────────────

/// Enum-dispatch PHY so we can use `SerialPortPhy` (production, real RS-485),
/// `TcpPhy` (serial-over-TCP device servers and the containerised simulator) or
/// `PtyPhy` (test/development, socat virtual serial pair) without `dyn` dispatch
/// (which profirust's `ProfibusPhy` trait doesn't support).
enum AnyPhy {
    Serial(profirust::phy::SerialPortPhy),
    Tcp(TcpPhy),
    Pty(PtyPhy),
}

impl profirust::phy::ProfibusPhy for AnyPhy {
    fn poll_transmission(&mut self, now: profirust::time::Instant) -> bool {
        match self {
            Self::Serial(p) => p.poll_transmission(now),
            Self::Tcp(p) => p.poll_transmission(now),
            Self::Pty(p) => p.poll_transmission(now),
        }
    }
    fn transmit_data<F, R>(&mut self, now: profirust::time::Instant, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> (usize, R),
    {
        match self {
            Self::Serial(p) => p.transmit_data(now, f),
            Self::Tcp(p) => p.transmit_data(now, f),
            Self::Pty(p) => p.transmit_data(now, f),
        }
    }
    fn receive_data<F, R>(&mut self, now: profirust::time::Instant, f: F) -> R
    where
        F: FnOnce(&[u8]) -> (usize, R),
    {
        match self {
            Self::Serial(p) => p.receive_data(now, f),
            Self::Tcp(p) => p.receive_data(now, f),
            Self::Pty(p) => p.receive_data(now, f),
        }
    }
}

/// Select the appropriate PHY for `port`:
/// * `tcp://host:port` — a TCP byte stream (serial device server / simulator).
/// * Paths under `/dev/pts/`, `/tmp/tty`, or containing `PROFIBUS` in the
///   name — ptys (no TIOCGSERIAL).
/// * Anything else — a real serial device.
fn open_phy(port: &str, baudrate: profirust::Baudrate) -> AnyPhy {
    if let Some(addr) = port.strip_prefix("tcp://") {
        debug!(addr, "opening port as tcp byte stream");
        AnyPhy::Tcp(TcpPhy::new(addr))
    } else if is_pty_path(port) {
        debug!(port, "opening port as pty (skipping TIOCGSERIAL)");
        AnyPhy::Pty(PtyPhy::new(port))
    } else {
        AnyPhy::Serial(profirust::phy::SerialPortPhy::new(port, baudrate))
    }
}

fn is_pty_path(port: &str) -> bool {
    port.contains("PROFIBUS")
        || port.starts_with("/dev/pts/")
        || port.starts_with("/tmp/tty")
}

// ── PtyPhy ────────────────────────────────────────────────────────────────────

/// A minimal `ProfibusPhy` implementation that works with Linux pty devices
/// (socat virtual serial pairs).  It uses raw `libc` I/O so it doesn't call
/// `TIOCGSERIAL`/`TIOCSSERIAL` (which ptys don't support).
///
/// PTYs are full-duplex and writes complete synchronously, so this phy returns
/// `false` from `poll_transmission` immediately — transmission is always done.
struct PtyPhy {
    fd: libc::c_int,
    rx_buf: Vec<u8>,
}

impl PtyPhy {
    fn new(path: &str) -> Self {
        use std::ffi::CString;
        let cpath = CString::new(path).expect("port path contains NUL");
        // SAFETY: standard libc open() call.
        let fd = unsafe {
            libc::open(
                cpath.as_ptr(),
                libc::O_RDWR | libc::O_NOCTTY | libc::O_NONBLOCK,
            )
        };
        if fd < 0 {
            panic!(
                "PtyPhy: failed to open '{}': {}",
                path,
                std::io::Error::last_os_error()
            );
        }

        // Set raw mode so there's no input/output processing.
        // SAFETY: termios struct is fully initialised by tcgetattr.
        unsafe {
            let mut tty: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(fd, &mut tty) == 0 {
                libc::cfmakeraw(&mut tty);
                // 19200 bps — ignored by ptys but keeps the struct consistent.
                libc::cfsetispeed(&mut tty, libc::B19200);
                libc::cfsetospeed(&mut tty, libc::B19200);
                libc::tcsetattr(fd, libc::TCSANOW, &tty);
            }
        }

        Self {
            fd,
            rx_buf: Vec::with_capacity(512),
        }
    }
}

impl Drop for PtyPhy {
    fn drop(&mut self) {
        // SAFETY: fd is valid until drop.
        unsafe { libc::close(self.fd) };
    }
}

impl profirust::phy::ProfibusPhy for PtyPhy {
    fn poll_transmission(&mut self, _now: profirust::time::Instant) -> bool {
        // Writes to a pty complete synchronously in transmit_data, so there
        // is never an ongoing transmission from this phy's perspective.
        false
    }

    fn transmit_data<F, R>(&mut self, _now: profirust::time::Instant, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> (usize, R),
    {
        let mut buf = [0u8; 512];
        let (len, r) = f(&mut buf);
        if len > 0 {
            // SAFETY: buf is valid; fd is open.
            unsafe {
                libc::write(self.fd, buf.as_ptr() as *const libc::c_void, len);
            }
        }
        r
    }

    fn receive_data<F, R>(&mut self, _now: profirust::time::Instant, f: F) -> R
    where
        F: FnOnce(&[u8]) -> (usize, R),
    {
        // Drain whatever is available without blocking.
        let mut tmp = [0u8; 512];
        // SAFETY: buf is valid; fd is open; O_NONBLOCK so returns -1/EAGAIN if empty.
        let n = unsafe {
            libc::read(self.fd, tmp.as_mut_ptr() as *mut libc::c_void, tmp.len())
        };
        if n > 0 {
            self.rx_buf.extend_from_slice(&tmp[..n as usize]);
        }
        let (consumed, r) = f(&self.rx_buf);
        if consumed > 0 {
            self.rx_buf.drain(..consumed.min(self.rx_buf.len()));
        }
        r
    }
}

// ── TcpPhy ────────────────────────────────────────────────────────────────────

/// A `ProfibusPhy` over a TCP byte stream (`port = "tcp://host:port"`): serial
/// device servers (RS-485 ⇄ TCP gateways) and the containerised slave
/// simulator, with no pty or socat bridge in between.
///
/// Like a pty the stream is full-duplex with no line-level transmission timing,
/// so `poll_transmission` reports "done" immediately. A dropped connection is
/// re-established lazily from the bus loop with a backoff; while disconnected,
/// frames are silently lost and the FDL layer's own retries/link watchdog
/// surface the outage.
struct TcpPhy {
    addr: String,
    stream: Option<std::net::TcpStream>,
    rx_buf: Vec<u8>,
    next_attempt: std::time::Instant,
}

impl TcpPhy {
    const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);
    const RECONNECT_DELAY: std::time::Duration = std::time::Duration::from_secs(2);
    /// TCP endpoints (device servers, containerised sims) may come up after the
    /// connector; retry the initial connect for this long before giving up.
    const INITIAL_RETRY_WINDOW: std::time::Duration = std::time::Duration::from_secs(10);

    /// Panics when the initial connect keeps failing — like the other phys,
    /// `bus_thread_main`'s `catch_unwind` turns this into a clean connect error
    /// report (and `connect()` allows the bus 30 s to become ready).
    fn new(addr: &str) -> Self {
        let deadline = std::time::Instant::now() + Self::INITIAL_RETRY_WINDOW;
        loop {
            match Self::open(addr) {
                Ok(stream) => {
                    return Self {
                        addr: addr.to_string(),
                        stream: Some(stream),
                        rx_buf: Vec::with_capacity(512),
                        next_attempt: std::time::Instant::now(),
                    }
                }
                Err(e) if std::time::Instant::now() < deadline => {
                    debug!(addr, "tcp phy connect failed: {e}; retrying");
                    std::thread::sleep(std::time::Duration::from_millis(500));
                }
                Err(e) => panic!("TcpPhy: failed to connect to '{addr}': {e}"),
            }
        }
    }

    fn open(addr: &str) -> std::io::Result<std::net::TcpStream> {
        use std::net::ToSocketAddrs;
        let sock_addr = addr.to_socket_addrs()?.next().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "address did not resolve")
        })?;
        let stream = std::net::TcpStream::connect_timeout(&sock_addr, Self::CONNECT_TIMEOUT)?;
        // The bus loop must never block on the socket, and PROFIBUS frames are
        // latency-sensitive request/response exchanges: disable Nagle.
        stream.set_nonblocking(true)?;
        stream.set_nodelay(true)?;
        Ok(stream)
    }

    /// The live stream, reconnecting lazily (at most every `RECONNECT_DELAY`)
    /// after a drop.
    fn stream(&mut self) -> Option<&mut std::net::TcpStream> {
        if self.stream.is_none() {
            let now = std::time::Instant::now();
            if now < self.next_attempt {
                return None;
            }
            self.next_attempt = now + Self::RECONNECT_DELAY;
            match Self::open(&self.addr) {
                Ok(stream) => {
                    info!(addr = %self.addr, "tcp phy reconnected");
                    self.stream = Some(stream);
                }
                Err(e) => {
                    debug!(addr = %self.addr, "tcp phy reconnect failed: {e}");
                    return None;
                }
            }
        }
        self.stream.as_mut()
    }

    fn drop_stream(&mut self, why: &std::io::Error) {
        warn!(addr = %self.addr, "tcp phy connection lost: {why}; reconnecting");
        self.stream = None;
        self.next_attempt = std::time::Instant::now() + Self::RECONNECT_DELAY;
    }
}

impl profirust::phy::ProfibusPhy for TcpPhy {
    fn poll_transmission(&mut self, _now: profirust::time::Instant) -> bool {
        // Writes complete synchronously in transmit_data (into the kernel socket
        // buffer), so there is never an ongoing transmission from this phy's view.
        false
    }

    fn transmit_data<F, R>(&mut self, _now: profirust::time::Instant, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> (usize, R),
    {
        use std::io::Write;
        let mut buf = [0u8; 512];
        let (len, r) = f(&mut buf);
        if len > 0 {
            if let Some(stream) = self.stream() {
                match stream.write_all(&buf[..len]) {
                    Ok(()) => {}
                    // A full socket buffer means the peer stopped reading; the frame
                    // is lost and the FDL layer retries. Keep the connection.
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        warn!(addr = %self.addr, "tcp phy send buffer full; frame dropped");
                    }
                    Err(e) => self.drop_stream(&e),
                }
            }
        }
        r
    }

    fn receive_data<F, R>(&mut self, _now: profirust::time::Instant, f: F) -> R
    where
        F: FnOnce(&[u8]) -> (usize, R),
    {
        use std::io::Read;
        // Drain whatever is available without blocking.
        let mut tmp = [0u8; 512];
        if let Some(stream) = self.stream() {
            match stream.read(&mut tmp) {
                Ok(0) => self.drop_stream(&std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "peer closed the connection",
                )),
                Ok(n) => self.rx_buf.extend_from_slice(&tmp[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => self.drop_stream(&e),
            }
        }
        let (consumed, r) = f(&self.rx_buf);
        if consumed > 0 {
            self.rx_buf.drain(..consumed.min(self.rx_buf.len()));
        }
        r
    }
}

fn bus_thread_main(
    devices: Vec<DeviceBusConfig>,
    connection: BusConnection,
    shared: Arc<Mutex<SharedBusState>>,
    stop: Arc<AtomicBool>,
    ready_tx: std::sync::mpsc::Sender<Result<Vec<LinkReport>, String>>,
) {
use profirust::{dp, fdl};

    // Wrap the entire setup in catch_unwind so profirust panics (e.g. from
    // build_verified or a bad serial port) are surfaced cleanly.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // ── 1. Leak heap buffers for 'static lifetime ─────────────────────
        //
        // profirust's Peripheral::new takes &'dp mut [u8] I/O buffers and
        // &'dp [u8] config/param slices.  DpMaster<'dp, _> ties everything
        // to the same lifetime.  By leaking the Vecs we obtain 'static slices,
        // which avoids self-referential struct complexity.  We reconstruct and
        // drop the Boxes when the thread exits.
        let mut leaked: Vec<LeakedBufs> = devices
            .iter()
            .map(|d| LeakedBufs {
                ibuf: Box::into_raw(vec![0u8; d.input_bytes].into_boxed_slice()),
                obuf: Box::into_raw(vec![0u8; d.output_bytes].into_boxed_slice()),
                cfg: Box::into_raw(d.config_bytes.clone().into_boxed_slice()),
                prm: Box::into_raw(d.param_bytes.clone().into_boxed_slice()),
            })
            .collect();

        // ── 2. Build DpMaster and add peripherals ─────────────────────────
        let mut dp_master = dp::DpMaster::new(Vec::new());
        let mut handles: Vec<(String, dp::PeripheralHandle)> = Vec::new();

        for (i, dev) in devices.iter().enumerate() {
            // SAFETY: Pointers are valid for the duration of this closure.
            // No other code aliases these pointers.
            let (ibuf, obuf, cfg_bytes, prm_bytes): (
                &'static mut [u8],
                &'static mut [u8],
                &'static [u8],
                &'static [u8],
            ) = unsafe {
                (
                    &mut *leaked[i].ibuf,
                    &mut *leaked[i].obuf,
                    &*leaked[i].cfg,
                    &*leaked[i].prm,
                )
            };

            let opts = dp::PeripheralOptions {
                ident_number: dev.ident_number,
                max_tsdr: dev.max_tsdr,
                // Always supply slices (even empty) so profirust sends Set_Prm and
                // Chk_Cfg.  With None the DP state machine stalls in WaitForParam/
                // WaitForConfig indefinitely.
                config: Some(cfg_bytes),
                user_parameters: Some(prm_bytes),
                ..Default::default()
            };
            let h = dp_master.add(dp::Peripheral::new(dev.station_address, opts, ibuf, obuf));
            handles.push((dev.name.clone(), h));
        }

        // ── 3. Build FDL layer ────────────────────────────────────────────
        let baudrate = connection.profirust_baudrate();
        let params = fdl::ParametersBuilder::new(connection.master_address, baudrate)
            .slot_bits(connection.slot_bits)
            .build_verified(&dp_master);
        let mut fdl = fdl::FdlActiveStation::new(params);

        // ── 4. Open PHY ───────────────────────────────────────────────────
        let mut phy = open_phy(&connection.port, fdl.parameters().baudrate);

        // ── 5. Go online ──────────────────────────────────────────────────
        fdl.set_online();
        dp_master.enter_operate();

        info!(
            port = connection.port,
            baudrate = connection.baudrate,
            master_address = connection.master_address,
            peripherals = handles.len(),
            "PROFIBUS bus thread online"
        );

        // ── 6. Main bus loop ──────────────────────────────────────────────
        let mut ready_tx_opt: Option<std::sync::mpsc::Sender<Result<Vec<LinkReport>, String>>> =
            Some(ready_tx);

        loop {
            if stop.load(Ordering::Relaxed) {
                info!("bus thread received stop signal, exiting");
                break;
            }

            let now = profirust::time::Instant::now();
            fdl.poll(now, &mut phy, &mut dp_master);
            let events = dp_master.take_last_events();

            if events.cycle_completed {
                let mut state = shared.lock().unwrap();
                state.cycle_count += 1;

                let mut reports: Vec<LinkReport> = Vec::new();

                for (name, h) in &handles {
                    let peripheral = dp_master.get_mut(*h);

                    // Copy current inputs into shared state.
                    let pi_i: Vec<u8> = peripheral.pi_i().to_vec();

                    if let Some(ps) = state.peripherals.get_mut(name) {
                        debug!(device = name, inputs = ?pi_i, "cycle_completed: pi_i snapshot");
                        ps.inputs = pi_i;

                        // Mark peripheral connected once it is in data exchange.
                        // profirust marks a peripheral as "running" (DATA_EXCHANGE) after
                        // successful Set_Prm + Chk_Cfg.  Until then, inputs are zeroed.
                        // We consider the bus itself "connected" after the first DP cycle.
                        ps.link = PeripheralLinkState::Connected;

                        if ready_tx_opt.is_some() {
                            reports.push(LinkReport {
                                device: name.clone(),
                                status: LinkStatus::Connected,
                                reason: None,
                                info: Some(serde_json::json!({
                                    "protocol": PROTOCOL,
                                    "station_address": devices
                                        .iter()
                                        .find(|d| &d.name == name)
                                        .map(|d| d.station_address)
                                        .unwrap_or(0),
                                })),
                            });
                        }

                        // Apply pending outputs to PI_Q on every cycle.
                        let pending = ps.outputs.clone();
                        drop(state);

                        let pi_q = peripheral.pi_q_mut();
                        let len = pi_q.len().min(pending.len());
                        if len > 0 {
                            pi_q[..len].copy_from_slice(&pending[..len]);
                        }

                        state = shared.lock().unwrap();
                    }
                }

                // Signal readiness after the first completed cycle.
                if let Some(tx) = ready_tx_opt.take() {
                    debug!("first DP cycle complete, signalling readiness");
                    let _ = tx.send(Ok(reports));
                }
            }
        }

        // ── 7. Cleanup ────────────────────────────────────────────────────
        drop(dp_master);
        let _ = fdl;
        drop(phy);

        // SAFETY: Reconstruct Boxes from the raw pointers to free the heap.
        // dp_master (and thus the 'static borrows into these allocations) has
        // been dropped above, so no dangling references remain.
        unsafe {
            for lb in leaked {
                drop(Box::from_raw(lb.ibuf));
                drop(Box::from_raw(lb.obuf));
                drop(Box::from_raw(lb.cfg));
                drop(Box::from_raw(lb.prm));
            }
        }

        info!("PROFIBUS bus thread stopped");
    }));

    if let Err(e) = result {
        let msg = if let Some(s) = e.downcast_ref::<String>() {
            s.clone()
        } else if let Some(s) = e.downcast_ref::<&str>() {
            s.to_string()
        } else {
            "unknown panic".to_string()
        };
        error!("PROFIBUS bus thread panicked: {msg}");
        // best-effort: tell connect() that we failed if it is still waiting
        // (the ready_tx may have already been consumed, which is fine)
        shared
            .lock()
            .unwrap()
            .peripherals
            .values_mut()
            .for_each(|ps| ps.link = PeripheralLinkState::Fault(msg.clone()));
    }
}

// ── decode / encode helpers ───────────────────────────────────────────────────

fn decode_point(id: &str, model: &ProfibusPoint, buffer: &[u8]) -> Sample {
    let addr = &model.address;
    let start = addr.byte_offset;

    // Bit-level extraction (boolean / bitfield).
    if let Some(bit_off) = addr.bit_offset {
        let bit_cnt = addr.bit_count.unwrap_or(1);
        if start >= buffer.len() {
            return bad_sample(id, "byte_offset out of range for input buffer", Some(addr), None);
        }
        let raw = vec![buffer[start]];
        return match model.mode {
            Mode::Raw => good_sample(id, model, raw, None),
            Mode::Typed => {
                let n = extract_bitfield(&raw, model.endianness, model.word_order, bit_off, bit_cnt);
                let value = if bit_cnt == 1 {
                    Value::Bool(n != 0)
                } else {
                    Value::Number(n as f64)
                };
                good_sample(id, model, raw, Some(value))
            }
        };
    }

    // Byte-level extraction.
    let dt = match model.datatype {
        Some(d) => d,
        None if model.mode == Mode::Raw => {
            // raw mode with no datatype — emit the whole remaining buffer slice
            let raw = buffer[start..].to_vec();
            return good_sample(id, model, raw, None);
        }
        None => {
            return bad_sample(id, "typed point has no datatype", Some(addr), None);
        }
    };

    let byte_len = match dt.byte_len() {
        Some(n) => n,
        None => {
            return bad_sample(id, "unsupported variable-length datatype", Some(addr), None);
        }
    };

    let end = start + byte_len;
    if end > buffer.len() {
        return bad_sample(
            id,
            &format!(
                "point needs bytes {start}..{end} but buffer is only {} bytes",
                buffer.len()
            ),
            Some(addr),
            None,
        );
    }

    let bytes = buffer[start..end].to_vec();
    match model.mode {
        Mode::Raw => good_sample(id, model, bytes, None),
        Mode::Typed => {
            match decode_primitive(&bytes, dt, model.endianness, model.word_order) {
                Ok(value) => good_sample(id, model, bytes, Some(value)),
                Err(e) => bad_sample(
                    id,
                    &format!("decode error: {e}"),
                    Some(addr),
                    None,
                ),
            }
        }
    }
}

fn encode_point(
    model: &ProfibusPoint,
    buffer: &mut Vec<u8>,
    request: &CommandRequest,
) -> Result<(), String> {
    let addr = &model.address;
    let start = addr.byte_offset;

    if let Some(raw_hex) = &request.raw {
        let bytes = parse_hex(raw_hex)?;
        write_bytes_into(buffer, start, &bytes);
        return Ok(());
    }

    let value = request
        .value
        .as_ref()
        .and_then(json_to_value)
        .ok_or_else(|| "missing or invalid value".to_string())?;

    // Bit-level write.
    if let Some(bit_off) = addr.bit_offset {
        let bit_cnt = addr.bit_count.unwrap_or(1);
        if start >= buffer.len() {
            return Err(format!("byte_offset {start} out of range for output buffer"));
        }
        let field = match &value {
            Value::Bool(b) => *b as u64,
            Value::Number(n) => *n as u64,
            Value::Text(t) => t.parse::<u64>().unwrap_or(0),
        };
        let mask: u8 = (((1u64 << bit_cnt) - 1) << bit_off) as u8;
        buffer[start] = (buffer[start] & !mask) | ((field as u8) << bit_off & mask);
        return Ok(());
    }

    // Byte-level write.
    let dt = model
        .datatype
        .ok_or_else(|| "typed write requires a datatype".to_string())?;
    let bytes = encode_primitive(&value, dt, model.endianness, model.word_order)
        .map_err(|e| e.to_string())?;
    write_bytes_into(buffer, start, &bytes);
    Ok(())
}

fn write_bytes_into(buffer: &mut Vec<u8>, start: usize, bytes: &[u8]) {
    let end = start + bytes.len();
    if end > buffer.len() {
        buffer.resize(end, 0);
    }
    buffer[start..end].copy_from_slice(bytes);
}

// ── sample builders ───────────────────────────────────────────────────────────

fn good_sample(id: &str, model: &ProfibusPoint, raw: Vec<u8>, value: Option<Value>) -> Sample {
    Sample {
        ts: OffsetDateTime::now_utc(),
        device: String::new(),
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
        raw_group: 2,
        quality: Quality::Good,
        unit: model.unit.clone(),
        addr: addr_echo(Some(&model.address)),
        seq: None,
        error: None,
    }
}

fn bad_sample(id: &str, error: &str, addr: Option<&PointAddress>, unit: Option<String>) -> Sample {
    Sample {
        ts: OffsetDateTime::now_utc(),
        device: String::new(),
        protocol: PROTOCOL,
        point: id.to_string(),
        mode: Mode::Typed,
        datatype: None,
        value: None,
        raw: Vec::new(),
        raw_group: 2,
        quality: Quality::Bad,
        unit,
        addr: addr_echo(addr),
        seq: None,
        error: Some(error.to_string()),
    }
}

fn addr_echo(addr: Option<&PointAddress>) -> serde_json::Value {
    match addr {
        Some(a) => {
            let mut obj = serde_json::json!({
                "direction": match a.direction {
                    Direction::Input  => "input",
                    Direction::Output => "output",
                },
                "byte_offset": a.byte_offset,
            });
            if let Some(b) = a.bit_offset {
                obj["bit_offset"] = b.into();
            }
            obj
        }
        None => serde_json::Value::Null,
    }
}

// ── small utilities ───────────────────────────────────────────────────────────

fn json_to_value(v: &serde_json::Value) -> Option<Value> {
    match v {
        serde_json::Value::Bool(b) => Some(Value::Bool(*b)),
        serde_json::Value::Number(n) => n.as_f64().map(Value::Number),
        serde_json::Value::String(s) => Some(Value::Text(s.clone())),
        _ => None,
    }
}

fn parse_hex(s: &str) -> Result<Vec<u8>, String> {
    let s = s.trim().replace(' ', "");
    if !s.len().is_multiple_of(2) {
        return Err(format!("odd-length hex string: '{s}'"));
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|e| format!("invalid hex: {e}")))
        .collect()
}

// ── test helpers (exposed only under #[cfg(test)] / integration test builds) ──

#[doc(hidden)]
pub mod __test_helpers {
    use super::*;

    /// Call `decode_point` with an ad-hoc `ProfibusPoint` and return the sample.
    pub fn decode_test(
        id: &str,
        datatype: DataType,
        byte_offset: usize,
        bit_offset: Option<u32>,
        bit_count: Option<u32>,
        buffer: &[u8],
    ) -> Sample {
        let model = ProfibusPoint {
            address: PointAddress {
                direction: Direction::Input,
                byte_offset,
                bit_offset,
                bit_count,
            },
            mode: Mode::Typed,
            datatype: Some(datatype),
            endianness: Endianness::Big,
            word_order: WordOrder::Big,
            access: Access::Read,
            unit: None,
            transform: Transform::default(),
        };
        decode_point(id, &model, buffer)
    }

    /// Call `encode_point` with an ad-hoc output model and return the modified buffer.
    pub fn encode_test(
        datatype: DataType,
        byte_offset: usize,
        bit_offset: Option<u32>,
        bit_count: Option<u32>,
        value: serde_json::Value,
        buffer: &mut Vec<u8>,
    ) {
        let model = ProfibusPoint {
            address: PointAddress {
                direction: Direction::Output,
                byte_offset,
                bit_offset,
                bit_count,
            },
            mode: Mode::Typed,
            datatype: Some(datatype),
            endianness: Endianness::Big,
            word_order: WordOrder::Big,
            access: Access::Write,
            unit: None,
            transform: Transform::default(),
        };
        let request = CommandRequest {
            point: "test".to_string(),
            value: Some(value),
            value_repr: None,
            raw: None,
        };
        encode_point(&model, buffer, &request).expect("encode failed");
    }

    /// Encode using raw hex bytes.
    pub fn encode_hex_test(byte_offset: usize, hex: &str, buffer: &mut Vec<u8>) {
        let model = ProfibusPoint {
            address: PointAddress {
                direction: Direction::Output,
                byte_offset,
                bit_offset: None,
                bit_count: None,
            },
            mode: Mode::Raw,
            datatype: None,
            endianness: Endianness::Big,
            word_order: WordOrder::Big,
            access: Access::Write,
            unit: None,
            transform: Transform::default(),
        };
        let request = CommandRequest {
            point: "test".to_string(),
            value: None,
            value_repr: None,
            raw: Some(hex.to_string()),
        };
        encode_point(&model, buffer, &request).expect("encode failed");
    }
}

#[cfg(test)]
mod tcp_phy_tests {
    use super::*;
    use profirust::phy::ProfibusPhy;
    use std::io::{Read, Write};

    fn now() -> profirust::time::Instant {
        profirust::time::Instant::ZERO
    }

    /// Full round-trip through a live socket: transmit reaches the peer, the
    /// peer's reply comes back through receive_data's buffering.
    #[test]
    fn tcp_phy_round_trips_bytes() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let mut buf = [0u8; 5];
            sock.read_exact(&mut buf).unwrap();
            assert_eq!(&buf, b"hello");
            sock.write_all(b"world").unwrap();
            sock // keep the connection open until the test is done
        });

        let mut phy = TcpPhy::new(&addr.to_string());
        phy.transmit_data(now(), |buf| {
            buf[..5].copy_from_slice(b"hello");
            (5, ())
        });
        assert!(!phy.poll_transmission(now()), "tcp writes complete synchronously");

        // Poll until the reply has arrived (nonblocking reads may need a few tries).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut got = Vec::new();
        while got.len() < 5 && std::time::Instant::now() < deadline {
            phy.receive_data(now(), |data| {
                if data.len() >= 5 {
                    got.extend_from_slice(data);
                    (data.len(), ())
                } else {
                    (0, ()) // leave partial frames buffered
                }
            });
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(got, b"world");
        drop(server.join().unwrap());
    }

    /// A closed peer must not panic the bus thread: the phy drops the stream
    /// and keeps answering with an empty receive buffer until it reconnects.
    #[test]
    fn tcp_phy_survives_peer_disconnect() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (sock, _) = listener.accept().unwrap();
            drop(sock); // immediately close
        });

        let mut phy = TcpPhy::new(&addr.to_string());
        server.join().unwrap();

        // The peer's close() has returned, but its FIN reaches this socket through the
        // kernel asynchronously — a nonblocking read may see WouldBlock a few times before
        // it sees EOF. Poll until the phy notices the disconnect (a fixed iteration count
        // here flakes under parallel test load); every poll must stay panic-free and empty.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while phy.stream.is_some() && std::time::Instant::now() < deadline {
            let seen = phy.receive_data(now(), |data| (data.len(), data.len()));
            assert_eq!(seen, 0, "no data must surface from a closed peer");
            phy.transmit_data(now(), |buf| {
                buf[0] = 0xAA;
                (1, ())
            });
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(phy.stream.is_none(), "stream dropped after disconnect");

        // Repeated polls on the dropped stream: no panic, no data, no premature reconnect
        // (the listener is gone and the backoff hasn't elapsed).
        for _ in 0..3 {
            let seen = phy.receive_data(now(), |data| (data.len(), data.len()));
            assert_eq!(seen, 0);
            phy.transmit_data(now(), |buf| {
                buf[0] = 0xAA;
                (1, ())
            });
        }
        assert!(phy.stream.is_none(), "stream stays down until the backoff elapses");
    }

    #[test]
    fn open_phy_dispatches_tcp_scheme() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let phy = open_phy(&format!("tcp://{addr}"), profirust::Baudrate::B19200);
        assert!(matches!(phy, AnyPhy::Tcp(_)));
    }
}
