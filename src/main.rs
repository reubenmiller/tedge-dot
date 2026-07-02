//! `tedge-dot` binary entry point.
//!
//! Two modes of operation:
//!
//! * `run` (the default) — discover connector configurations (files and/or directories of
//!   `*.toml`), and run every connector concurrently in this one process: each config gets its
//!   own protocol module + SDK runtime instance, supervised with an in-process restart loop.
//! * `read` / `write` — connect directly to a configured device and read or write a point once,
//!   then exit. These need no broker or running connector; they reuse the exact same protocol
//!   module code path the runtime uses, which makes them handy for experimenting and debugging.

use clap::{Args, Parser, Subcommand};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;
use tedge_dot_sdk::{
    model::hex_grouped, runtime, CommandRequest, Connector, ConnectorConfig, DeviceConfig,
    LinkStatus, Quality, Sample, Value,
};
use tracing::{error, info, warn, Instrument};
use tracing_subscriber::EnvFilter;

const DEFAULT_CONFIG: &str = "/etc/tedge/plugins/ot/modbus.toml";
const DEFAULT_CONFIG_DIR: &str = "/etc/tedge/plugins/ot";
const DEFAULT_RESTART_DELAY_SECS: u64 = 5;

/// thin-edge.io OT protocol connector.
#[derive(Parser)]
#[command(name = "tedge-dot", version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the connector service (default when invoked with just config paths).
    Run(RunArgs),
    /// Read one or more points directly from a device, then exit (no broker required).
    Read(ReadArgs),
    /// Write a value to a point directly on a device, then exit (no broker required).
    Write(WriteArgs),
}

#[derive(Args)]
struct RunArgs {
    /// Connector configuration files and/or directories to scan for `*.toml` configs.
    /// Every config found runs as its own connector within this process, each in a
    /// restart loop (backoff: $TEDGE_DOT_RESTART_DELAY seconds, default 5).
    #[arg(default_value = DEFAULT_CONFIG_DIR)]
    configs: Vec<String>,
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

/// Run every discovered connector concurrently in this process (long-lived service).
async fn run(args: RunArgs) -> ExitCode {
    let configs = match discover_configs(&args.configs) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };

    let filter = match EnvFilter::try_from_default_env() {
        Ok(f) => f,
        Err(_) => silence_opcua_cert_noise(EnvFilter::new(pick_log_level(&configs))),
    };
    tracing_subscriber::fmt().with_env_filter(filter).init();

    if configs.is_empty() {
        warn!(
            "no connector configs found in {:?} — idle; add configs and restart the service",
            args.configs
        );
        runtime::shutdown_signal().await;
        return ExitCode::SUCCESS;
    }

    warn_duplicate_service_names(&configs);
    let restart_delay = restart_delay_from_env();

    // One shutdown trigger shared by every connector: flipped on Ctrl-C / SIGTERM.
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let mut tasks = tokio::task::JoinSet::new();
    for path in configs {
        let name = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        let span = tracing::info_span!("connector", %name);
        tasks.spawn(supervise(path, restart_delay, shutdown_rx.clone()).instrument(span));
    }

    runtime::shutdown_signal().await;
    info!("shutdown requested; stopping all connectors");
    let _ = shutdown_tx.send(true);

    // Give the connectors a moment to disconnect and publish their final health status.
    if tokio::time::timeout(Duration::from_secs(10), async {
        while tasks.join_next().await.is_some() {}
    })
    .await
    .is_err()
    {
        warn!("some connectors did not stop in time; exiting anyway");
    }
    ExitCode::SUCCESS
}

/// Supervise one connector: (re)load its config, run it under the SDK runtime, and restart it
/// with a backoff when it fails — the config file is re-read on every attempt, so fixing a bad
/// config is picked up without restarting the service.
async fn supervise(
    path: PathBuf,
    restart_delay: Duration,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    loop {
        info!("starting connector ({})", path.display());
        // Run each attempt on its own task so a panicking protocol module is contained and
        // restarted like any other failure instead of taking the whole service down.
        let attempt = tokio::spawn(
            run_one(path.clone(), shutdown.clone()).in_current_span(),
        )
        .await
        .unwrap_or_else(|join_err| Err(format!("connector task panicked: {join_err}")));

        if *shutdown.borrow() {
            break;
        }
        match attempt {
            Ok(()) => break, // clean stop
            Err(e) => error!(
                "connector failed: {e}; restarting in {}s",
                restart_delay.as_secs()
            ),
        }
        tokio::select! {
            _ = tokio::time::sleep(restart_delay) => {}
            _ = shutdown.changed() => break,
        }
    }
}

/// One connector attempt: parse the config, build the protocol module, run it until it fails
/// or the shared shutdown trigger fires.
async fn run_one(
    path: PathBuf,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> Result<(), String> {
    let config = load_config(&path.display().to_string())?;
    let connector = build_connector(&config.connector.protocol)?;
    let shutdown_fut = async move {
        let _ = shutdown.wait_for(|stop| *stop).await;
    };
    runtime::run_until(connector, config, path, shutdown_fut)
        .await
        .map_err(|e| e.to_string())
}

/// Expand the `run` arguments into concrete config files: directories contribute their `*.toml`
/// entries (sorted), files are taken as-is, and duplicates are dropped.
fn discover_configs(args: &[String]) -> Result<Vec<PathBuf>, String> {
    let mut found = Vec::new();
    for arg in args {
        let path = Path::new(arg);
        if path.is_dir() {
            let mut entries: Vec<PathBuf> = std::fs::read_dir(path)
                .map_err(|e| format!("failed to read config directory '{arg}': {e}"))?
                .flatten()
                .map(|entry| entry.path())
                .filter(|p| p.is_file() && p.extension().is_some_and(|ext| ext == "toml"))
                .collect();
            entries.sort();
            found.extend(entries);
        } else if path.is_file() {
            found.push(path.to_path_buf());
        } else {
            return Err(format!("config path '{arg}' does not exist"));
        }
    }
    let mut seen = std::collections::HashSet::new();
    found.retain(|p| seen.insert(p.clone()));
    Ok(found)
}

/// Default log filter for the service: the most verbose `connector.log_level` across the
/// parseable configs, so a single-config invocation keeps its configured level exactly.
/// `RUST_LOG` overrides this entirely.
fn pick_log_level(configs: &[PathBuf]) -> String {
    fn verbosity(level: &str) -> u8 {
        match level {
            "trace" => 4,
            "debug" => 3,
            "info" => 2,
            "warn" => 1,
            "error" => 0,
            _ => 2,
        }
    }
    configs
        .iter()
        .filter_map(|p| load_config(&p.display().to_string()).ok())
        .map(|c| c.connector.log_level)
        .max_by_key(|l| verbosity(l))
        .unwrap_or_else(|| "info".to_string())
}

/// Two configs sharing a `service_name` fight over the same MQTT client id and health topic;
/// call it out loudly instead of letting the connectors steal each other's session.
fn warn_duplicate_service_names(configs: &[PathBuf]) {
    let mut by_service: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    for path in configs {
        if let Ok(config) = load_config(&path.display().to_string()) {
            by_service
                .entry(config.connector.service_name)
                .or_default()
                .push(path.display().to_string());
        }
    }
    for (service, paths) in by_service {
        if paths.len() > 1 {
            warn!(
                "configs {} share service_name '{service}'; give each connector a unique \
                 service_name or they will steal each other's MQTT session",
                paths.join(", ")
            );
        }
    }
}

/// Per-connector restart backoff, tunable via TEDGE_DOT_RESTART_DELAY (seconds).
fn restart_delay_from_env() -> Duration {
    std::env::var("TEDGE_DOT_RESTART_DELAY")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(DEFAULT_RESTART_DELAY_SECS))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, contents: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, contents).unwrap();
        path
    }

    #[test]
    fn discover_configs_expands_directories_sorted_and_dedups() {
        let dir = std::env::temp_dir().join(format!("tedge-dot-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        write(&dir, "b.toml", "");
        let a = write(&dir, "a.toml", "");
        write(&dir, "ignored.txt", "");

        // a directory plus one of its own files: expanded, sorted, deduplicated
        let found =
            discover_configs(&[dir.display().to_string(), a.display().to_string()]).unwrap();
        let names: Vec<_> = found
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["a.toml", "b.toml"]);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn discover_configs_rejects_missing_paths() {
        let err = discover_configs(&["/nonexistent/tedge-dot".into()]).unwrap_err();
        assert!(err.contains("does not exist"), "{err}");
    }

    #[test]
    fn pick_log_level_takes_most_verbose() {
        let dir = std::env::temp_dir().join(format!("tedge-dot-loglevel-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let info = write(
            &dir,
            "info.toml",
            "[connector]\nprotocol = \"modbus\"\nlog_level = \"info\"\n",
        );
        let debug = write(
            &dir,
            "debug.toml",
            "[connector]\nprotocol = \"opcua\"\nlog_level = \"debug\"\n",
        );

        assert_eq!(pick_log_level(std::slice::from_ref(&info)), "info");
        assert_eq!(pick_log_level(&[info, debug]), "debug");
        assert_eq!(pick_log_level(&[]), "info");

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
