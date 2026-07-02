//! `tedge-dot` binary entry point.
//!
//! Two modes of operation:
//!
//! * `run` (the default) — discover connector configurations (files and/or directories of
//!   `*.toml`), and run every connector concurrently in this one process: each config gets its
//!   own protocol module + SDK runtime instance, supervised with an in-process restart loop.
//!   Samples go to the MQTT broker by default, or to stdout as JSON lines (`--output stdout`).
//! * `read` / `write` — connect directly to configured devices and read or write points, then
//!   exit. Devices and points accept `*`/`?` wildcards, and `read` can keep polling
//!   (`--poll` / `--interval` / `--count`). These need no broker or running connector; they
//!   reuse the exact same protocol module code path the runtime uses, which makes them handy
//!   for experimenting and debugging.

use clap::{Args, Parser, Subcommand, ValueEnum};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};
use tedge_dot_sdk::{
    model::hex_grouped, parse_duration, runtime, Access, CommandRequest, Connector,
    ConnectorConfig, DeviceConfig, LinkStatus, PointConfig, Quality, Sample, Value,
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

/// Where the `run` command publishes samples.
#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Output {
    /// Publish to the MQTT broker configured in each connector config.
    Mqtt,
    /// Print each sample envelope as one JSON line on stdout (no broker needed). The
    /// envelope's `device` field identifies the source device.
    Stdout,
}

#[derive(Args)]
struct RunArgs {
    /// Connector configuration files and/or directories to scan for `*.toml` configs.
    /// Every config found runs as its own connector within this process, each in a
    /// restart loop (backoff: $TEDGE_DOT_RESTART_DELAY seconds, default 5).
    configs: Vec<String>,
    /// Connector configuration file or directory (same as the positional argument; repeat
    /// for several). Defaults to /etc/tedge/plugins/ot when neither is given.
    #[arg(short, long = "config", value_name = "PATH")]
    config: Vec<String>,
    /// Where samples go: the configured MQTT broker, or stdout as JSON lines.
    #[arg(short, long, value_enum, default_value_t = Output::Mqtt)]
    output: Output,
    /// Stop after this long (e.g. "500ms", "10s", "5m", "1h"); default: run until
    /// Ctrl-C/SIGTERM.
    #[arg(long, value_name = "DURATION", value_parser = parse_cli_duration)]
    duration: Option<Duration>,
}

#[derive(Args)]
struct ReadArgs {
    /// Path to the connector configuration file.
    #[arg(short, long, default_value = DEFAULT_CONFIG)]
    config: String,
    /// Device name or wildcard pattern (`*` matches any run of characters, `?` one).
    /// Default: every device in the config.
    #[arg(short, long, default_value = "*")]
    device: String,
    /// Point id or wildcard pattern; repeat to read several. Default: every readable point
    /// (wildcard patterns skip write-only points; explicitly named points are always read).
    #[arg(short, long = "point", value_name = "POINT")]
    points: Vec<String>,
    /// Keep polling instead of reading once, at each device's configured poll interval.
    #[arg(long)]
    poll: bool,
    /// Poll at this interval (e.g. "500ms", "10s", "5m", "1h") instead of the configured
    /// one. Implies --poll.
    #[arg(long, value_name = "DURATION", value_parser = parse_cli_duration)]
    interval: Option<Duration>,
    /// Stop after this many polls per device. Implies --poll.
    #[arg(long, value_name = "N")]
    count: Option<u64>,
    /// Print the raw JSON sample envelope(s) instead of a friendly summary.
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct WriteArgs {
    /// Path to the connector configuration file.
    #[arg(short, long, default_value = DEFAULT_CONFIG)]
    config: String,
    /// Device name or wildcard pattern (`*` matches any run of characters, `?` one).
    /// Default: every device in the config that has a matching point.
    #[arg(short, long, default_value = "*")]
    device: String,
    /// Point id or wildcard pattern; repeat to write the value to several points (wildcard
    /// patterns select writable points only; explicitly named points are always attempted).
    #[arg(short, long = "point", value_name = "POINT", required = true)]
    points: Vec<String>,
    /// Logical value for a typed write (parsed as bool, number, or string).
    #[arg(short, long, conflicts_with = "raw", required_unless_present = "raw")]
    value: Option<String>,
    /// Hex bytes for a raw write (e.g. "00ff"), written to the wire verbatim.
    #[arg(long, conflicts_with = "value")]
    raw: Option<String>,
    /// Print raw JSON result envelopes instead of a friendly summary.
    #[arg(long)]
    json: bool,
}

/// clap value parser for human-readable durations ("500ms", "10s", "5m", "1h").
fn parse_cli_duration(s: &str) -> Result<Duration, String> {
    parse_duration(s).ok_or_else(|| {
        format!("invalid duration '{s}' (expected e.g. \"500ms\", \"10s\", \"5m\", \"1h\")")
    })
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

/// Combine the positional config paths with the `--config` flag values, falling back to the
/// default config directory when neither is given.
fn combined_config_args(args: &RunArgs) -> Vec<String> {
    let mut paths = args.configs.clone();
    paths.extend(args.config.iter().cloned());
    if paths.is_empty() {
        paths.push(DEFAULT_CONFIG_DIR.to_string());
    }
    paths
}

/// Resolve when the service should stop: the shutdown signal (Ctrl-C / SIGTERM), or the
/// `--duration` deadline when one was given.
async fn shutdown_or_deadline(duration: Option<Duration>) {
    match duration {
        Some(d) => {
            tokio::select! {
                _ = runtime::shutdown_signal() => {}
                _ = tokio::time::sleep(d) => info!("--duration elapsed"),
            }
        }
        None => runtime::shutdown_signal().await,
    }
}

/// Run every discovered connector concurrently in this process (long-lived service).
async fn run(args: RunArgs) -> ExitCode {
    let config_args = combined_config_args(&args);
    let configs = match discover_configs(&config_args) {
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
    // In stdout output mode, stdout carries the sample stream — keep logs on stderr.
    let builder = tracing_subscriber::fmt().with_env_filter(filter);
    if args.output == Output::Stdout {
        builder.with_writer(std::io::stderr).init();
    } else {
        builder.init();
    }

    if configs.is_empty() {
        warn!(
            "no connector configs found in {:?} — idle; add configs and restart the service",
            config_args
        );
        shutdown_or_deadline(args.duration).await;
        return ExitCode::SUCCESS;
    }

    warn_duplicate_service_names(&configs);
    let restart_delay = restart_delay_from_env();

    // One shutdown trigger shared by every connector: flipped on Ctrl-C / SIGTERM (or when
    // --duration elapses).
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let mut tasks = tokio::task::JoinSet::new();
    for path in configs {
        let name = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        let span = tracing::info_span!("connector", %name);
        tasks.spawn(
            supervise(path, args.output, restart_delay, shutdown_rx.clone()).instrument(span),
        );
    }

    shutdown_or_deadline(args.duration).await;
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
    output: Output,
    restart_delay: Duration,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    loop {
        info!("starting connector ({})", path.display());
        // Run each attempt on its own task so a panicking protocol module is contained and
        // restarted like any other failure instead of taking the whole service down.
        let attempt = tokio::spawn(
            run_one(path.clone(), output, shutdown.clone()).in_current_span(),
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
    output: Output,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> Result<(), String> {
    let config = load_config(&path.display().to_string())?;
    let connector = build_connector(&config.connector.protocol)?;
    let shutdown_fut = async move {
        let _ = shutdown.wait_for(|stop| *stop).await;
    };
    match output {
        Output::Mqtt => runtime::run_until(connector, config, path, shutdown_fut)
            .await
            .map_err(|e| e.to_string()),
        Output::Stdout => runtime::run_stdout_until(connector, config, shutdown_fut)
            .await
            .map_err(|e| e.to_string()),
    }
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

/// Shell-style wildcard match: `*` matches any run of characters, `?` exactly one.
fn wildcard_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    // Backtrack point: the most recent `*` and the text position it is currently matched up to.
    let (mut star, mut star_ti) = (None, 0usize);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            star_ti = ti;
            pi += 1;
        } else if let Some(sp) = star {
            // Let the last `*` swallow one more character and retry.
            pi = sp + 1;
            star_ti += 1;
            ti = star_ti;
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&c| c == '*')
}

/// True when a selector is a wildcard pattern rather than a literal name.
fn is_pattern(s: &str) -> bool {
    s.contains('*') || s.contains('?')
}

/// The points selected on one device.
#[derive(Debug)]
struct Target<'a> {
    device: &'a DeviceConfig,
    points: Vec<&'a PointConfig>,
}

/// Resolve device and point selectors (literal names or wildcard patterns) against the config.
///
/// Wildcard point patterns only pick up points whose access fits the operation — readable
/// points for reads, writable for writes — so `-p '*'` never trips over a write-only or
/// read-only point. Explicitly named points are always taken; the connector enforces access
/// and reports the violation. Every selector must match at least one point somewhere.
fn resolve_targets<'a>(
    config: &'a ConnectorConfig,
    device_pattern: &str,
    point_patterns: &[String],
    for_write: bool,
) -> Result<Vec<Target<'a>>, String> {
    let devices: Vec<&DeviceConfig> = config
        .devices
        .iter()
        .filter(|d| wildcard_match(device_pattern, &d.name))
        .collect();
    if devices.is_empty() {
        let names: Vec<&str> = config.devices.iter().map(|d| d.name.as_str()).collect();
        return Err(format!(
            "no device matches '{device_pattern}' (available: {})",
            if names.is_empty() {
                "none".to_string()
            } else {
                names.join(", ")
            }
        ));
    }

    let mut hits = vec![0usize; point_patterns.len()];
    let mut targets = Vec::new();
    for device in devices {
        let mut points: Vec<&PointConfig> = Vec::new();
        for (i, pattern) in point_patterns.iter().enumerate() {
            for point in &device.points {
                if !wildcard_match(pattern, &point.id) {
                    continue;
                }
                if is_pattern(pattern) {
                    let access = Access::parse(point.access.as_deref());
                    let fits = if for_write {
                        access.can_write()
                    } else {
                        access != Access::Write
                    };
                    if !fits {
                        continue;
                    }
                }
                hits[i] += 1;
                if !points.iter().any(|p| p.id == point.id) {
                    points.push(point);
                }
            }
        }
        if !points.is_empty() {
            targets.push(Target { device, points });
        }
    }

    let kind = if for_write { "writable" } else { "readable" };
    for (i, pattern) in point_patterns.iter().enumerate() {
        if hits[i] == 0 {
            return Err(format!(
                "no {kind} point matches '{pattern}' on the selected device(s)"
            ));
        }
    }
    Ok(targets)
}

/// Connect the connector and split the target devices into reachable ones and failures.
/// Unreachable devices are reported on stderr; it is an error when none are reachable.
async fn connect_targets<'a>(
    connector: &mut Box<dyn Connector>,
    targets: Vec<Target<'a>>,
) -> Result<(Vec<Target<'a>>, usize), String> {
    let reports = connector
        .connect()
        .await
        .map_err(|e| format!("connect failed: {e}"))?;
    let mut reachable = Vec::new();
    let mut failed = 0usize;
    for target in targets {
        let down = reports.iter().find(|r| {
            r.device == target.device.name && r.status != LinkStatus::Connected
        });
        match down {
            Some(report) => {
                failed += 1;
                eprintln!(
                    "warning: could not connect to device '{}': {}",
                    target.device.name,
                    report.reason.as_deref().unwrap_or("link not connected")
                );
            }
            None => reachable.push(target),
        }
    }
    if reachable.is_empty() {
        return Err("could not connect to any selected device".into());
    }
    Ok((reachable, failed))
}

/// One device's polling job for `read`: the resolved points and the poll cadence.
struct ReadJob {
    device: String,
    refs: Vec<tedge_dot_sdk::PointRef>,
    interval: Duration,
    next_due: Instant,
    remaining: Option<u64>,
}

/// The poll interval for one device: the `--interval` override, else the device's
/// `poll_interval`, else the connector's.
fn effective_interval(
    config: &ConnectorConfig,
    device: &DeviceConfig,
    over: Option<Duration>,
) -> Duration {
    over.or_else(|| device.poll_interval.as_deref().and_then(parse_duration))
        .or_else(|| parse_duration(&config.connector.poll_interval))
        .unwrap_or(Duration::from_secs(2))
}

/// Read points directly from one or more devices, once or on a polling loop.
async fn cmd_read(args: ReadArgs) -> Result<(), String> {
    init_cli_logging();
    let config = load_config(&args.config)?;
    let patterns = if args.points.is_empty() {
        vec!["*".to_string()]
    } else {
        args.points.clone()
    };
    let targets = resolve_targets(&config, &args.device, &patterns, false)?;
    let poll_mode = args.poll || args.interval.is_some() || args.count.is_some();

    let mut connector = build_connector(&config.connector.protocol)?;
    connector
        .configure(&config)
        .map_err(|e| format!("invalid configuration: {e}"))?;
    let (targets, failed_devices) = connect_targets(&mut connector, targets).await?;

    let now = Instant::now();
    let mut jobs: Vec<ReadJob> = targets
        .iter()
        .map(|t| ReadJob {
            device: t.device.name.clone(),
            refs: t
                .points
                .iter()
                .map(|p| runtime::point_ref(p, t.device.default_mode))
                .collect(),
            interval: effective_interval(&config, t.device, args.interval),
            next_due: now,
            remaining: if poll_mode { args.count } else { Some(1) },
        })
        .collect();

    let result = read_loop(&mut connector, &mut jobs, args.json, poll_mode).await;
    let _ = connector.disconnect().await;
    let saw_bad = result?;

    // One-shot reads keep their strict exit code: any unreachable device or bad-quality
    // point fails the command. A polling session (often ended by Ctrl-C) exits cleanly.
    if !poll_mode {
        if failed_devices > 0 {
            return Err("could not connect to every selected device".into());
        }
        if saw_bad {
            return Err("one or more points returned bad quality".into());
        }
    }
    Ok(())
}

/// Drive the read jobs until every job's count is exhausted or Ctrl-C. Returns whether any
/// sample came back with bad quality.
async fn read_loop(
    connector: &mut Box<dyn Connector>,
    jobs: &mut Vec<ReadJob>,
    json: bool,
    poll_mode: bool,
) -> Result<bool, String> {
    let mut saw_bad = false;
    loop {
        jobs.retain(|j| j.remaining != Some(0));
        let Some(next_due) = jobs.iter().map(|j| j.next_due).min() else {
            return Ok(saw_bad);
        };
        if poll_mode {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => return Ok(saw_bad),
                _ = tokio::time::sleep_until(next_due.into()) => {}
            }
        }
        let now = Instant::now();
        for job in jobs.iter_mut().filter(|j| j.next_due <= now) {
            match connector.read_points(&job.device, &job.refs).await {
                Ok(mut samples) => {
                    for sample in samples.iter_mut() {
                        sample.device = job.device.clone();
                        saw_bad |= sample.quality == Quality::Bad;
                        if json {
                            println!("{}", sample.to_envelope());
                        } else {
                            println!("{}", format_sample(&job.device, sample));
                        }
                    }
                }
                Err(e) if poll_mode => {
                    // Keep the watch session alive: report, try to re-establish the device,
                    // and let the next round read again (the interval is the backoff).
                    eprintln!("error: read from '{}' failed: {e}", job.device);
                    if connector.reconnect(&job.device).await.is_err() {
                        // per-device reconnect unsupported (or failed); a later full read
                        // may still recover via the transport's own reconnect
                    }
                }
                Err(e) => return Err(format!("read failed: {e}")),
            }
            job.next_due = now + job.interval;
            if let Some(remaining) = &mut job.remaining {
                *remaining -= 1;
            }
        }
    }
}

/// Write a value to one or more points, across one or more devices.
async fn cmd_write(args: WriteArgs) -> Result<(), String> {
    init_cli_logging();
    let config = load_config(&args.config)?;
    let targets = resolve_targets(&config, &args.device, &args.points, true)?;

    let mut connector = build_connector(&config.connector.protocol)?;
    connector
        .configure(&config)
        .map_err(|e| format!("invalid configuration: {e}"))?;
    let (targets, failed_devices) = connect_targets(&mut connector, targets).await?;

    let mut failures = failed_devices;
    for target in &targets {
        for point in &target.points {
            let request = if let Some(raw) = &args.raw {
                CommandRequest {
                    point: point.id.clone(),
                    value: None,
                    value_repr: None,
                    raw: Some(raw.clone()),
                }
            } else {
                let (value, repr) = parse_cli_value(args.value.as_deref().unwrap_or_default());
                CommandRequest {
                    point: point.id.clone(),
                    value: Some(value),
                    value_repr: Some(repr.to_string()),
                    raw: None,
                }
            };
            match connector.execute(&target.device.name, "write", &request).await {
                Ok(result) => print_write_result(&target.device.name, &result, args.json),
                Err(e) => {
                    failures += 1;
                    eprintln!(
                        "error: write to {}/{} failed: {e}",
                        target.device.name, point.id
                    );
                }
            }
        }
    }
    let _ = connector.disconnect().await;

    if failures > 0 {
        return Err(format!("{failures} write(s) failed"));
    }
    Ok(())
}

/// Print one successful write result (JSON envelope or friendly line). The `device` field
/// identifies the target when several devices matched.
fn print_write_result(device: &str, result: &tedge_dot_sdk::CommandResult, json: bool) {
    if json {
        let mut obj = serde_json::Map::new();
        obj.insert("status".into(), "successful".into());
        obj.insert("device".into(), device.into());
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
        println!("ok: wrote {what} to {device}/{}", result.point);
    }
}

/// Load and parse a connector configuration file.
fn load_config(path: &str) -> Result<ConnectorConfig, String> {
    let text =
        std::fs::read_to_string(path).map_err(|e| format!("failed to read config '{path}': {e}"))?;
    toml::from_str(&text).map_err(|e| format!("failed to parse config '{path}': {e}"))
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
    fn wildcard_matching() {
        assert!(wildcard_match("*", "anything"));
        assert!(wildcard_match("*", ""));
        assert!(wildcard_match("temp_u16", "temp_u16"));
        assert!(!wildcard_match("temp_u16", "temp_u32"));
        assert!(wildcard_match("temp_*", "temp_u16"));
        assert!(wildcard_match("*_f32", "level_f32"));
        assert!(wildcard_match("*temp*", "boiler_temp_raw"));
        assert!(!wildcard_match("temp_*", "level_f32"));
        assert!(wildcard_match("temp_u1?", "temp_u16"));
        assert!(!wildcard_match("temp_u1?", "temp_u1"));
        assert!(wildcard_match("a*b*c", "a-x-b-y-c"));
        assert!(!wildcard_match("a*b*c", "a-x-b-y"));
        assert!(!wildcard_match("", "x"));
        assert!(wildcard_match("", ""));
    }

    fn selection_config() -> ConnectorConfig {
        toml::from_str(
            r#"
[connector]
protocol = "modbus"
poll_interval = "2s"

[[device]]
name = "plc1"
protocol_address = { host = "127.0.0.1" }
poll_interval = "5s"

  [[device.point]]
  id = "temp_u16"
  access = "read_write"
  address = { table = "holding", address = 3 }

  [[device.point]]
  id = "temp_scaled"
  address = { table = "holding", address = 3 }

  [[device.point]]
  id = "cmd_only"
  access = "write"
  address = { table = "coil", address = 1 }

[[device]]
name = "plc2"
protocol_address = { host = "127.0.0.2" }

  [[device.point]]
  id = "level_f32"
  address = { table = "holding", address = 6 }
"#,
        )
        .unwrap()
    }

    #[test]
    fn resolve_targets_default_wildcards_cover_all_readable_points() {
        let config = selection_config();
        let targets = resolve_targets(&config, "*", &["*".to_string()], false).unwrap();
        assert_eq!(targets.len(), 2);
        let plc1: Vec<&str> = targets[0].points.iter().map(|p| p.id.as_str()).collect();
        // the write-only point is skipped by the wildcard
        assert_eq!(plc1, vec!["temp_u16", "temp_scaled"]);
        assert_eq!(targets[1].device.name, "plc2");
    }

    #[test]
    fn resolve_targets_wildcard_selects_writable_points_for_write() {
        let config = selection_config();
        let targets = resolve_targets(&config, "plc1", &["*".to_string()], true).unwrap();
        assert_eq!(targets.len(), 1);
        let ids: Vec<&str> = targets[0].points.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, vec!["temp_u16", "cmd_only"]);
    }

    #[test]
    fn resolve_targets_explicit_point_bypasses_access_filter() {
        let config = selection_config();
        // explicitly-named read-only point is attempted for write (connector will reject)
        let targets =
            resolve_targets(&config, "plc1", &["temp_scaled".to_string()], true).unwrap();
        assert_eq!(targets[0].points[0].id, "temp_scaled");
    }

    #[test]
    fn resolve_targets_dedups_overlapping_patterns() {
        let config = selection_config();
        let patterns = vec!["temp_*".to_string(), "temp_u16".to_string()];
        let targets = resolve_targets(&config, "plc1", &patterns, false).unwrap();
        let ids: Vec<&str> = targets[0].points.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, vec!["temp_u16", "temp_scaled"]);
    }

    #[test]
    fn resolve_targets_rejects_unknown_device_and_point() {
        let config = selection_config();
        let err = resolve_targets(&config, "nope", &["*".to_string()], false).unwrap_err();
        assert!(err.contains("no device matches 'nope'"), "{err}");
        assert!(err.contains("plc1, plc2"), "{err}");

        let err =
            resolve_targets(&config, "*", &["missing".to_string()], false).unwrap_err();
        assert!(err.contains("no readable point matches 'missing'"), "{err}");
    }

    #[test]
    fn resolve_targets_point_pattern_may_miss_some_devices() {
        let config = selection_config();
        // matches only on plc2; plc1 contributes no target but that's fine
        let targets = resolve_targets(&config, "*", &["level_*".to_string()], false).unwrap();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].device.name, "plc2");
    }

    #[test]
    fn effective_interval_prefers_override_then_device_then_connector() {
        let config = selection_config();
        let plc1 = &config.devices[0]; // poll_interval = "5s"
        let plc2 = &config.devices[1]; // none -> connector "2s"
        assert_eq!(
            effective_interval(&config, plc1, Some(Duration::from_millis(100))),
            Duration::from_millis(100)
        );
        assert_eq!(effective_interval(&config, plc1, None), Duration::from_secs(5));
        assert_eq!(effective_interval(&config, plc2, None), Duration::from_secs(2));
    }

    #[test]
    fn run_config_args_merge_positional_and_flag() {
        let args = RunArgs {
            configs: vec!["a.toml".into()],
            config: vec!["b.toml".into()],
            output: Output::Mqtt,
            duration: None,
        };
        assert_eq!(combined_config_args(&args), vec!["a.toml", "b.toml"]);

        let args = RunArgs {
            configs: vec![],
            config: vec![],
            output: Output::Mqtt,
            duration: None,
        };
        assert_eq!(combined_config_args(&args), vec![DEFAULT_CONFIG_DIR]);
    }

    #[test]
    fn cli_duration_parses_human_readable() {
        assert_eq!(parse_cli_duration("500ms"), Ok(Duration::from_millis(500)));
        assert_eq!(parse_cli_duration("10s"), Ok(Duration::from_secs(10)));
        assert_eq!(parse_cli_duration("5m"), Ok(Duration::from_secs(300)));
        assert_eq!(parse_cli_duration("1h"), Ok(Duration::from_secs(3600)));
        assert!(parse_cli_duration("nope").is_err());
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
