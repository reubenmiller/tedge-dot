//! `tedge-dot` binary entry point.
//!
//! Two modes of operation:
//!
//! * `run` (the default) — load the connector configuration, select the protocol module named in
//!   `connector.protocol`, and run it under the SDK runtime (the long-lived MQTT-driven service).
//! * `read` / `write` — connect directly to a configured device and read or write a point once,
//!   then exit. These need no broker or running connector; they reuse the exact same protocol
//!   module code path the runtime uses, which makes them handy for experimenting and debugging.

use clap::{Args, Parser, Subcommand};
use std::process::ExitCode;
use tedge_dot_sdk::{
    model::hex_grouped, runtime, CommandRequest, Connector, ConnectorConfig, DeviceConfig,
    LinkStatus, Quality, Sample, Value,
};
use tracing_subscriber::EnvFilter;

const DEFAULT_CONFIG: &str = "/etc/tedge/plugins/ot/modbus.toml";

/// thin-edge.io OT protocol connector.
#[derive(Parser)]
#[command(name = "tedge-dot", version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the connector service (default when invoked with just a config path).
    Run(RunArgs),
    /// Read one or more points directly from a device, then exit (no broker required).
    Read(ReadArgs),
    /// Write a value to a point directly on a device, then exit (no broker required).
    Write(WriteArgs),
}

#[derive(Args)]
struct RunArgs {
    /// Path to the connector configuration file.
    #[arg(default_value = DEFAULT_CONFIG)]
    config: String,
}

#[derive(Args)]
struct ReadArgs {
    /// Path to the connector configuration file.
    #[arg(short, long, default_value = DEFAULT_CONFIG)]
    config: String,
    /// Device name (as configured under `[[device]]`).
    #[arg(short, long)]
    device: String,
    /// Point id to read. Repeat to read several points in one batch.
    #[arg(short, long = "point", required = true)]
    points: Vec<String>,
    /// Print the raw JSON sample envelope(s) instead of a friendly summary.
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct WriteArgs {
    /// Path to the connector configuration file.
    #[arg(short, long, default_value = DEFAULT_CONFIG)]
    config: String,
    /// Device name (as configured under `[[device]]`).
    #[arg(short, long)]
    device: String,
    /// Point id to write.
    #[arg(short, long)]
    point: String,
    /// Logical value for a typed write (parsed as bool, number, or string).
    #[arg(short, long, conflicts_with = "raw", required_unless_present = "raw")]
    value: Option<String>,
    /// Hex bytes for a raw write (e.g. "00ff"), written to the wire verbatim.
    #[arg(long, conflicts_with = "value")]
    raw: Option<String>,
    /// Print the raw JSON result envelope instead of a friendly summary.
    #[arg(long)]
    json: bool,
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse_from(normalized_args());
    match cli.command {
        Command::Run(args) => run(args).await,
        Command::Read(args) => report(cmd_read(args).await),
        Command::Write(args) => report(cmd_write(args).await),
    }
}

/// Turn a command `Result` into a process exit code, printing any error to stderr.
fn report(result: Result<(), String>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Preserve the legacy invocation `tedge-dot [<config>]` (which runs the service) by
/// injecting the `run` subcommand when the first argument is neither a known subcommand nor a
/// flag (`-h`/`--help`/`-V`/`--version`).
fn normalized_args() -> Vec<String> {
    let mut args: Vec<String> = std::env::args().collect();
    const SUBCOMMANDS: &[&str] = &["run", "read", "write", "help"];
    let needs_run = match args.get(1) {
        None => true,
        Some(a) => !(SUBCOMMANDS.contains(&a.as_str()) || a.starts_with('-')),
    };
    if needs_run {
        args.insert(1, "run".to_string());
    }
    args
}

/// Run the connector under the SDK runtime (long-lived service).
async fn run(args: RunArgs) -> ExitCode {
    let config = match load_config(&args.config) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };

    let filter = match EnvFilter::try_from_default_env() {
        Ok(f) => f,
        Err(_) => silence_opcua_cert_noise(EnvFilter::new(&config.connector.log_level)),
    };
    tracing_subscriber::fmt().with_env_filter(filter).init();

    let connector = match build_connector(&config.connector.protocol) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };

    if let Err(e) = runtime::run(connector, config, args.config.into()).await {
        eprintln!("connector exited with error: {e}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

/// Read one or more points directly from a device.
async fn cmd_read(args: ReadArgs) -> Result<(), String> {
    init_cli_logging();
    let config = load_config(&args.config)?;
    let device = find_device(&config, &args.device)?;

    // Resolve the requested points against the device config.
    let mut refs = Vec::with_capacity(args.points.len());
    for id in &args.points {
        let point = device
            .points
            .iter()
            .find(|p| &p.id == id)
            .ok_or_else(|| format!("point '{id}' not found on device '{}'", args.device))?;
        refs.push(runtime::point_ref(point, device.default_mode));
    }

    let mut connector = build_connector(&config.connector.protocol)?;
    connector
        .configure(&config)
        .map_err(|e| format!("invalid configuration: {e}"))?;
    connect_device(&mut connector, &args.device).await?;

    let samples = connector
        .read_points(&args.device, &refs)
        .await
        .map_err(|e| format!("read failed: {e}"));
    let _ = connector.disconnect().await;
    let samples = samples?;

    for sample in &samples {
        if args.json {
            println!("{}", sample.to_envelope());
        } else {
            println!("{}", format_sample(&args.device, sample));
        }
    }

    // Non-zero exit if any point read back as bad.
    if samples.iter().any(|s| s.quality == Quality::Bad) {
        return Err("one or more points returned bad quality".into());
    }
    Ok(())
}

/// Write a value to a point directly on a device.
async fn cmd_write(args: WriteArgs) -> Result<(), String> {
    init_cli_logging();
    let config = load_config(&args.config)?;
    let device = find_device(&config, &args.device)?;
    if !device.points.iter().any(|p| p.id == args.point) {
        return Err(format!(
            "point '{}' not found on device '{}'",
            args.point, args.device
        ));
    }

    let request = if let Some(raw) = &args.raw {
        CommandRequest {
            point: args.point.clone(),
            value: None,
            value_repr: None,
            raw: Some(raw.clone()),
        }
    } else {
        let (value, repr) = parse_cli_value(args.value.as_deref().unwrap_or_default());
        CommandRequest {
            point: args.point.clone(),
            value: Some(value),
            value_repr: Some(repr.to_string()),
            raw: None,
        }
    };

    let mut connector = build_connector(&config.connector.protocol)?;
    connector
        .configure(&config)
        .map_err(|e| format!("invalid configuration: {e}"))?;
    connect_device(&mut connector, &args.device).await?;

    let result = connector
        .execute(&args.device, "write", &request)
        .await
        .map_err(|e| format!("write failed: {e}"));
    let _ = connector.disconnect().await;
    let result = result?;

    if args.json {
        let mut obj = serde_json::Map::new();
        obj.insert("status".into(), "successful".into());
        obj.insert("point".into(), result.point.clone().into());
        if let Some(v) = &result.value {
            obj.insert("value".into(), v.clone());
        }
        if let Some(r) = &result.raw {
            obj.insert("raw".into(), r.clone().into());
        }
        println!("{}", serde_json::Value::Object(obj));
    } else {
        let what = result
            .value
            .as_ref()
            .map(|v| v.to_string())
            .or_else(|| result.raw.clone())
            .unwrap_or_default();
        println!("ok: wrote {what} to {}/{}", args.device, result.point);
    }
    Ok(())
}

/// Load and parse a connector configuration file.
fn load_config(path: &str) -> Result<ConnectorConfig, String> {
    let text =
        std::fs::read_to_string(path).map_err(|e| format!("failed to read config '{path}': {e}"))?;
    toml::from_str(&text).map_err(|e| format!("failed to parse config '{path}': {e}"))
}

/// Locate a configured device by name.
fn find_device<'a>(config: &'a ConnectorConfig, name: &str) -> Result<&'a DeviceConfig, String> {
    config
        .devices
        .iter()
        .find(|d| d.name == name)
        .ok_or_else(|| {
            let names: Vec<&str> = config.devices.iter().map(|d| d.name.as_str()).collect();
            format!(
                "device '{name}' not found in config (available: {})",
                if names.is_empty() {
                    "none".to_string()
                } else {
                    names.join(", ")
                }
            )
        })
}

/// Connect the connector and verify the target device's link came up.
async fn connect_device(connector: &mut Box<dyn Connector>, device: &str) -> Result<(), String> {
    let reports = connector
        .connect()
        .await
        .map_err(|e| format!("connect failed: {e}"))?;
    if let Some(report) = reports.iter().find(|r| r.device == device) {
        if report.status != LinkStatus::Connected {
            return Err(format!(
                "could not connect to device '{device}': {}",
                report.reason.as_deref().unwrap_or("link not connected")
            ));
        }
    }
    Ok(())
}

/// Parse a CLI `--value` string into a JSON value and its `value_repr` tag, inferring the type:
/// `true`/`false` → boolean, numeric → number, otherwise string.
fn parse_cli_value(s: &str) -> (serde_json::Value, &'static str) {
    match s.to_ascii_lowercase().as_str() {
        "true" => return (serde_json::Value::Bool(true), "boolean"),
        "false" => return (serde_json::Value::Bool(false), "boolean"),
        _ => {}
    }
    if let Ok(i) = s.parse::<i64>() {
        return (serde_json::json!(i), "number");
    }
    if let Ok(f) = s.parse::<f64>() {
        return (serde_json::json!(f), "number");
    }
    (serde_json::Value::String(s.to_string()), "string")
}

/// Render a sample as a friendly one-line summary.
fn format_sample(device: &str, sample: &Sample) -> String {
    let value = match &sample.value {
        Some(Value::Bool(b)) => b.to_string(),
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::Text(t)) => t.clone(),
        None => "<raw>".to_string(),
    };
    let quality = match sample.quality {
        Quality::Good => "good",
        Quality::Bad => "bad",
        Quality::Stale => "stale",
    };
    let raw = hex_grouped(&sample.raw, sample.raw_group);
    let mut line = format!("{device}/{} = {value} ({quality}) raw={raw}", sample.point);
    if let Some(err) = &sample.error {
        line.push_str(&format!(" error={err}"));
    }
    line
}

/// CLI commands write their result to stdout; keep tracing quiet (warnings only) unless the user
/// raises it via `RUST_LOG`, so transport warnings still surface without cluttering output.
fn init_cli_logging() {
    let filter = match EnvFilter::try_from_default_env() {
        Ok(f) => f,
        Err(_) => silence_opcua_cert_noise(EnvFilter::new("warn")),
    };
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}

/// Silence async-opcua's spurious certificate / secure-channel ERROR logs.
///
/// For `security_policy = "None"` no application-instance certificate is required, but the library
/// still tries to load one from `pki/` and logs the missing cert/key at ERROR on every connect.
/// These directives drop that noise from the default filter; setting `RUST_LOG` bypasses this
/// entirely and shows everything.
fn silence_opcua_cert_noise(mut filter: EnvFilter) -> EnvFilter {
    const DIRECTIVES: &[&str] = &[
        "opcua_client::config=error",
        "opcua_crypto=off",
        "opcua_core::comms::secure_channel=off",
        "opcua_client::session::client=off",
        "opcua_client::session::event_loop=error",
    ];
    for directive in DIRECTIVES {
        if let Ok(parsed) = directive.parse() {
            filter = filter.add_directive(parsed);
        }
    }
    filter
}

/// Select the compiled-in protocol module by id. Fails fast if a protocol was not built.
fn build_connector(protocol: &str) -> Result<Box<dyn Connector>, String> {
    match protocol {
        #[cfg(feature = "modbus")]
        "modbus" => Ok(connector_modbus::factory()),
        #[cfg(feature = "opcua")]
        "opcua" => Ok(connector_opcua::factory()),
        #[cfg(feature = "canbus")]
        "canbus" => Ok(connector_canbus::factory()),
        #[cfg(feature = "canopen")]
        "canopen" => Ok(connector_canopen::factory()),
        #[cfg(feature = "profibus")]
        "profibus" => Ok(connector_profibus::factory()),
        other => Err(format!(
            "protocol '{other}' is not compiled in (enable its cargo feature)"
        )),
    }
}
