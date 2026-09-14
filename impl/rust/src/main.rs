//! `tedge-dot` binary entry point.
//!
//! Two modes of operation:
//!
//! * `run` (the default) — discover connector configurations (files and/or directories of
//!   `*.toml`), and run every connector concurrently in this one process: each config gets its
//!   own protocol module + SDK runtime instance, supervised with an in-process restart loop.
//!   Samples go to the MQTT broker by default, or to stdout as JSON lines (`--output stdout`).
//! * `describe` — render the Cumulocity Digital Twin Manager property definitions that declare
//!   a configuration's writable points as editable device parameters (for a tenant admin to
//!   register; the device itself never talks to the DTM service).
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

/// The release version `--version` reports. The release build stamps it from the tag via
/// TEDGE_DOT_VERSION (.goreleaser.yaml); other builds fall back to the crate version. This is
/// not the contract version the connectors publish in their capability descriptors.
const VERSION: &str = match option_env!("TEDGE_DOT_VERSION") {
    Some(v) => v,
    None => env!("CARGO_PKG_VERSION"),
};

/// thin-edge.io OT protocol connector.
#[derive(Parser)]
#[command(name = "tedge-dot", version = VERSION, about, long_about = None)]
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
    /// Render the Cumulocity DTM property definitions for a configuration's parameter sets
    /// (no device or broker required).
    Describe(DescribeArgs),
}

/// Output format of `describe`.
#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum DescribeFormat {
    /// Cumulocity Digital Twin Manager property definitions, one per parameter set
    /// (writable points grouped by meta.parameter.set). A tenant admin posts each element
    /// to POST /service/dtm/definitions/properties once to make the set editable in the
    /// device "Parameters" tab.
    C8yDtm,
}

#[derive(Args)]
struct DescribeArgs {
    /// Connector configuration files and/or directories to scan for `*.toml` configs, as for
    /// `run`. The definitions cover every config found, with a set shared by several of them
    /// rendered once.
    configs: Vec<String>,
    /// Connector configuration file or directory (same as the positional argument; repeat
    /// for several). Defaults to /etc/tedge/plugins/ot when neither is given.
    #[arg(short, long = "config", value_name = "PATH")]
    config: Vec<String>,
    /// What to print.
    #[arg(short, long, value_enum, default_value_t = DescribeFormat::C8yDtm)]
    format: DescribeFormat,
    /// Device name or wildcard pattern to restrict the output to; it must match at least one
    /// device. Default: every device.
    #[arg(short, long)]
    device: Option<String>,
    /// One parameter set for every point that does not name an absolute one, instead of the
    /// derived <type-or-protocol>_<group>_parameters. Must match the ot-parameter-state flow
    /// setting.
    #[arg(long, value_name = "NAME")]
    set: Option<String>,
    /// Print compact JSON (one document per line) instead of pretty-printed.
    #[arg(long)]
    compact: bool,
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
        Command::Describe(args) => report(cmd_describe(args)),
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
    const SUBCOMMANDS: &[&str] = &["run", "read", "write", "describe", "help"];
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
/// default config directory when neither is given. Shared by `run` and `describe`.
fn combined_config_args(positional: &[String], flagged: &[String]) -> Vec<String> {
    let mut paths = positional.to_vec();
    paths.extend(flagged.iter().cloned());
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
///
/// SIGHUP reloads: the config paths are discovered again, a connector starts for each new file
/// and stops for each file that is gone, and every other one re-reads its own file and applies
/// what changed (see `runtime::run_until_reloadable`).
async fn run(args: RunArgs) -> ExitCode {
    // First of all, so a SIGHUP that arrives while the service is still starting is a reload
    // request rather than the signal's default action, which terminates the process.
    let mut hangups = hangup_signals();
    let config_args = combined_config_args(&args.configs, &args.config);
    let configs = match discover_configs(&config_args)
        .and_then(|configs| require_rereadable(&configs).map(|()| configs))
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {e}");
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
            "no connector configs found in {:?} — idle; add configs, then reload (SIGHUP) or \
             restart the service",
            config_args
        );
    }

    warn_duplicate_service_names(&configs);
    warn_duplicate_devices(&configs);
    let mut connectors = Connectors::new(args.output, restart_delay_from_env());
    for path in configs {
        connectors.start(path);
    }

    // Ctrl-C / SIGTERM, or --duration elapsing, stops every connector.
    let stop = shutdown_or_deadline(args.duration);
    tokio::pin!(stop);
    loop {
        tokio::select! {
            _ = &mut stop => break,
            Some(()) = next_hangup(&mut hangups) => {
                info!("SIGHUP: reloading the connector configs");
                match discover_configs(&config_args) {
                    Ok(configs) => {
                        warn_duplicate_service_names(&configs);
                        warn_duplicate_devices(&configs);
                        // Stopping the removed connectors can take up to STOP_GRACE; a shutdown
                        // must not wait behind it (`connectors` keeps what is still stopping).
                        tokio::select! {
                            _ = connectors.reconcile(configs) => {}
                            _ = &mut stop => break,
                        }
                    }
                    Err(e) => error!("reload failed: {e}; the running connectors are unchanged"),
                }
            }
        }
    }
    info!("shutdown requested; stopping all connectors");
    connectors.stop_all().await;
    ExitCode::SUCCESS
}

/// SIGHUP, the conventional "reload your configuration" signal (what `systemctl reload` sends
/// through the unit's `ExecReload`), where it can be listened for.
#[cfg(unix)]
type Hangups = Option<tokio::signal::unix::Signal>;
#[cfg(not(unix))]
type Hangups = ();

#[cfg(unix)]
fn hangup_signals() -> Hangups {
    use tokio::signal::unix::{signal, SignalKind};
    signal(SignalKind::hangup())
        .map_err(|e| eprintln!("warning: cannot listen for SIGHUP, so reloads are unavailable: {e}"))
        .ok()
}

#[cfg(not(unix))]
fn hangup_signals() -> Hangups {}

/// Resolves on the next SIGHUP; never, where SIGHUP cannot be listened for.
async fn next_hangup(hangups: &mut Hangups) -> Option<()> {
    #[cfg(unix)]
    if let Some(signal) = hangups {
        return signal.recv().await;
    }
    #[cfg(not(unix))]
    let _ = hangups;
    std::future::pending().await
}

/// How long stopping connectors may take to disconnect and publish their final health status.
const STOP_GRACE: Duration = Duration::from_secs(10);

/// The connectors this process runs: one supervisor task per config file, by its path.
struct Connectors {
    output: Output,
    restart_delay: Duration,
    running: std::collections::BTreeMap<PathBuf, Supervised>,
    /// Supervisors asked to stop that have not finished yet. Kept here, not in a local, so a
    /// shutdown that interrupts a reload still waits for them.
    stopping: Vec<tokio::task::JoinHandle<()>>,
}

/// One config file's supervisor, and the handles that steer it.
struct Supervised {
    stop: tokio::sync::watch::Sender<bool>,
    reload: std::sync::Arc<tokio::sync::Notify>,
    task: tokio::task::JoinHandle<()>,
}

impl Connectors {
    fn new(output: Output, restart_delay: Duration) -> Self {
        Connectors {
            output,
            restart_delay,
            running: std::collections::BTreeMap::new(),
            stopping: Vec::new(),
        }
    }

    fn start(&mut self, path: PathBuf) {
        let name = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        let span = tracing::info_span!("connector", %name);
        let (stop, stopped) = tokio::sync::watch::channel(false);
        let reload = std::sync::Arc::new(tokio::sync::Notify::new());
        let task = tokio::spawn(
            supervise(path.clone(), self.output, self.restart_delay, stopped, reload.clone())
                .instrument(span),
        );
        self.running.insert(path, Supervised { stop, reload, task });
    }

    /// Bring the running connectors in line with `configs`, the paths discovered again on a
    /// reload: stop the connectors whose file is gone, ask each remaining one to re-read its
    /// file, and start one for every new file.
    async fn reconcile(&mut self, configs: Vec<PathBuf>) {
        let gone: Vec<PathBuf> = self
            .running
            .keys()
            .filter(|path| !configs.contains(*path))
            .cloned()
            .collect();
        for path in gone {
            if let Some(connector) = self.running.remove(&path) {
                info!("{} is gone; stopping its connector", path.display());
                let _ = connector.stop.send(true);
                self.stopping.push(connector.task);
            }
        }
        // Before starting anything: a new file may carry a removed one's service name, and the
        // old connector's final health "down" must not land after the new one's "up".
        self.finish_stopping().await;
        for path in configs {
            match self.running.get(&path) {
                // A permit is kept when the supervisor is not waiting right now (it is between
                // attempts, say), so the request is never lost.
                Some(connector) => connector.reload.notify_one(),
                None => {
                    info!("new config {}; starting its connector", path.display());
                    self.start(path);
                }
            }
        }
    }

    async fn stop_all(mut self) {
        for connector in std::mem::take(&mut self.running).into_values() {
            let _ = connector.stop.send(true);
            self.stopping.push(connector.task);
        }
        self.finish_stopping().await;
    }

    /// Wait for the stopping connectors to disconnect and publish their final health, for up to
    /// [`STOP_GRACE`], then cancel whatever is left and wait for the cancellation to take effect
    /// (a cancelled supervisor cancels its attempt, see [`runtime::AbortOnDrop`]). Cancel-safe: a task
    /// leaves `stopping` only once it has finished, so a later call picks up where this one was
    /// interrupted.
    async fn finish_stopping(&mut self) {
        let deadline = tokio::time::Instant::now() + STOP_GRACE;
        while let Some(task) = self.stopping.last_mut() {
            if tokio::time::timeout_at(deadline, task).await.is_err() {
                break;
            }
            self.stopping.pop();
        }
        if self.stopping.is_empty() {
            return;
        }
        warn!("some connectors did not stop in time; cancelling them");
        for task in &self.stopping {
            task.abort();
        }
        while let Some(task) = self.stopping.last_mut() {
            let _ = task.await;
            self.stopping.pop();
        }
    }
}

/// Supervise one connector: (re)load its config, run it under the SDK runtime, and restart it
/// with a backoff when it fails — the config file is re-read on every attempt, so fixing a bad
/// config is picked up without restarting the service, and a reload (SIGHUP) cuts the backoff
/// short.
async fn supervise(
    path: PathBuf,
    output: Output,
    restart_delay: Duration,
    mut stop: tokio::sync::watch::Receiver<bool>,
    reload: std::sync::Arc<tokio::sync::Notify>,
) {
    loop {
        info!("starting connector ({})", path.display());
        // Run each attempt on its own task so a panicking protocol module is contained and
        // restarted like any other failure instead of taking the whole service down.
        // Held through `AbortOnDrop`, so a supervisor cancelled because it did not stop in time
        // (`Connectors::finish_stopping`) cancels its attempt too instead of detaching it.
        let attempt = runtime::AbortOnDrop(tokio::spawn(
            run_one(path.clone(), output, stop.clone(), reload.clone()).in_current_span(),
        ))
        .await
        .unwrap_or_else(|join_err| Err(format!("connector task panicked: {join_err}")));

        if *stop.borrow() {
            break;
        }
        match attempt {
            Ok(runtime::RunExit::Stopped) => break, // clean stop
            // A reload changed what the running connector cannot adopt: start over with it.
            Ok(runtime::RunExit::Restart) => continue,
            Err(e) => error!(
                "connector failed: {e}; restarting in {}s, or on reload",
                restart_delay.as_secs()
            ),
        }
        tokio::select! {
            _ = tokio::time::sleep(restart_delay) => {}
            _ = reload.notified() => info!("reload requested; restarting the connector now"),
            _ = stop.changed() => break,
        }
    }
}

/// One connector attempt: parse the config, build the protocol module, and run it until it
/// fails, is stopped, or a reload finds a change it has to be restarted for.
async fn run_one(
    path: PathBuf,
    output: Output,
    mut stop: tokio::sync::watch::Receiver<bool>,
    reload: std::sync::Arc<tokio::sync::Notify>,
) -> Result<runtime::RunExit, String> {
    let config = load_config(&path.display().to_string())?;
    let connector = build_connector(&config.connector.protocol)?;
    let stall = stall_timeout(&config);
    let stop_fut = async move {
        let _ = stop.wait_for(|stop| *stop).await;
    };
    match output {
        Output::Mqtt => {
            // Race the connector against a liveness watchdog. The runtime bounds every single
            // protocol call, but a module can still wedge in ways a timeout does not cover (a
            // driver looping internally, a lock never released); the loop cannot rescue itself
            // then, since the hang is inside it. Losing the race cancels the connector —
            // dropping the hung call and the MQTT client, so the broker publishes the retained
            // last-will health "down" — and returns an error, which the supervisor restarts.
            let progress = runtime::Progress::new();
            let watchdog = stall_watchdog(progress.clone(), stall);
            tokio::select! {
                result = runtime::run_until_reloadable(
                    connector, config, path, stop_fut, progress, reload,
                ) => result.map_err(|e| e.to_string()),
                reason = watchdog => Err(reason),
            }
        }
        // No broker session to keep up here, so a changed file simply restarts the connector
        // with it; an unchanged or unusable one leaves it running, as in the MQTT runtime.
        Output::Stdout => {
            let running = config.clone();
            let run = runtime::run_stdout_until(connector, config, stop_fut);
            tokio::pin!(run);
            loop {
                tokio::select! {
                    result = &mut run => {
                        return result
                            .map(|()| runtime::RunExit::Stopped)
                            .map_err(|e| e.to_string());
                    }
                    _ = reload.notified() => match load_config(&path.display().to_string()) {
                        Ok(new) if new == running => {
                            info!("reload: {} is unchanged", path.display());
                        }
                        Ok(_) => return Ok(runtime::RunExit::Restart),
                        Err(e) => error!("reload: {e}; keeping the running configuration"),
                    },
                }
            }
        }
    }
}

/// The watchdog period for one connector: `[connector] stall_timeout`, `"0"` to disable.
///
/// It must be longer than the per-call bound, otherwise a single slow-but-legitimate call (a
/// large batch on a slow serial line) would be read as a hang and restart the connector in a
/// loop; a too-small value is raised rather than honoured.
fn stall_timeout(config: &ConnectorConfig) -> Duration {
    let limit = config.stall_limit();
    match parse_duration(&config.connector.stall_timeout) {
        None => warn!(
            "invalid connector.stall_timeout '{}'; using {}s",
            config.connector.stall_timeout,
            limit.as_secs()
        ),
        Some(configured) if configured.is_zero() => {
            info!("stall watchdog disabled (connector.stall_timeout = 0)")
        }
        Some(configured) if configured < limit => warn!(
            "connector.stall_timeout ({}s) is not longer than operation_timeout ({}s); using {}s",
            configured.as_secs(),
            (limit / 2).as_secs(),
            limit.as_secs()
        ),
        Some(_) => {}
    }
    limit
}

/// Resolves with a reason once the connector's loop has made no progress for `limit`.
/// Never resolves when the watchdog is disabled.
async fn stall_watchdog(progress: runtime::Progress, limit: Duration) -> String {
    if limit.is_zero() {
        loop {
            tokio::time::sleep(Duration::from_secs(3600)).await;
        }
    }
    let period = (limit / 4)
        .max(Duration::from_millis(500))
        .min(Duration::from_secs(10));
    loop {
        tokio::time::sleep(period).await;
        let idle = progress.idle();
        if idle >= limit {
            return format!(
                "no progress for {}s (connector.stall_timeout {}s): a protocol call is not \
                 returning, restarting the connector",
                idle.as_secs(),
                limit.as_secs()
            );
        }
    }
}

/// Expand config path arguments into concrete config files: directories contribute their `*.toml`
/// regular files (sorted), anything else that exists is taken as-is (a file, or a pipe such as
/// `-c <(generate-config)`), and a file named twice is kept once.
///
/// "Named twice" is judged by where the path resolves to, so the same file under two spellings
/// (`dir` and `dir//a.toml`, or a symlink) is one config; the spelling it was first named by is
/// what gets used and reported. `impl/c/src/main.c` (`collect_configs`) applies the same rules,
/// which `describe-parity.sh` pins.
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
        } else if path.exists() {
            found.push(path.to_path_buf());
        } else {
            return Err(format!("config path '{arg}' does not exist"));
        }
    }
    let mut seen = std::collections::HashSet::new();
    found.retain(|p| seen.insert(std::fs::canonicalize(p).unwrap_or_else(|_| p.clone())));
    Ok(found)
}

/// `run` reads a connector's config again whenever it restarts the connector, which a pipe
/// (`-c <(generate-config)`) cannot provide a second time: the first read would empty it and every
/// restart after that fail. So `run` takes files and directories only; `describe`, which reads
/// each config once, takes pipes too. The C build refuses the same paths.
fn require_rereadable(configs: &[PathBuf]) -> Result<(), String> {
    match configs.iter().find(|path| !path.is_file()) {
        Some(path) => Err(format!(
            "config path '{}' is not a regular file; `run` re-reads its configs, so it needs \
             files or directories",
            path.display()
        )),
        None => Ok(()),
    }
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
        .filter_map(|p| connector_section(p))
        .map(|c| c.log_level)
        .max_by_key(|l| verbosity(l))
        .unwrap_or_else(|| "info".to_string())
}

/// Parse only the `[connector]` section of a config file, skipping point-library resolution
/// (§3.4). The log level and the service name do not depend on a device's points, and a config
/// whose library reference does not resolve should still contribute them — the real load in
/// `run_one` is what reports that error, once, with the connector's own span.
///
/// Only that section is deserialized, so no device can hide it: a device switched off with
/// nothing but its name (§3.3) is valid, but would fail a typed parse of the whole file.
fn connector_section(path: &Path) -> Option<tedge_dot_sdk::config::ConnectorSection> {
    let text = std::fs::read_to_string(path).ok()?;
    let doc: toml::Value = toml::from_str(&text).ok()?;
    doc.get("connector")?.clone().try_into().ok()
}

/// Two configs sharing a `service_name` fight over the same MQTT client id and health topic;
/// call it out loudly instead of letting the connectors steal each other's session.
fn warn_duplicate_service_names(configs: &[PathBuf]) {
    let mut by_service: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    for path in configs {
        if let Some(connector) = connector_section(path) {
            by_service
                .entry(connector.service_name())
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

/// Two configs of one protocol defining the same device both own it: both act on its commands
/// (§6), racing each other's results, and both poll it.
fn warn_duplicate_devices(configs: &[PathBuf]) {
    for ((protocol, device), paths) in duplicate_devices(configs) {
        warn!(
            "configs {} all define {protocol} device '{device}'; define each device in one \
             config only or every one of them will answer its commands",
            paths.join(", ")
        );
    }
}

/// The `(protocol, device)` pairs defined by more than one config, with those configs.
fn duplicate_devices(configs: &[PathBuf]) -> Vec<((String, String), Vec<String>)> {
    let mut owners: std::collections::BTreeMap<(String, String), Vec<String>> =
        std::collections::BTreeMap::new();
    for path in configs {
        let Some(doc) = std::fs::read_to_string(path)
            .ok()
            .and_then(|text| toml::from_str::<toml::Value>(&text).ok())
        else {
            continue;
        };
        let Some(protocol) = doc
            .get("connector")
            .and_then(|c| c.get("protocol"))
            .and_then(toml::Value::as_str)
        else {
            continue;
        };
        let path = path.display().to_string();
        // Read raw rather than as a typed config: a device switched off with nothing but its name
        // (§3.3) is valid but would fail a typed parse, hiding the file's other devices — and a
        // disabled device owns nothing, so it cannot be a duplicate.
        let devices = doc.get("device").and_then(toml::Value::as_array);
        for device in devices.into_iter().flatten() {
            // Only a device the loader would keep: `enabled` absent or true.
            if !device.get("enabled").is_none_or(|v| v.as_bool() == Some(true)) {
                continue;
            }
            let Some(name) = device.get("name").and_then(toml::Value::as_str) else {
                continue;
            };
            let paths = owners
                .entry((protocol.to_string(), name.to_string()))
                .or_default();
            if paths.last() != Some(&path) {
                paths.push(path.clone());
            }
        }
    }
    owners.into_iter().filter(|(_, paths)| paths.len() > 1).collect()
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

/// Print the Cumulocity DTM definitions derived from every connector configuration found.
///
/// One service runs every config in its directory and a DTM identifier is tenant-wide, so the
/// definitions — and the warnings about them — are computed across all of the configs rather
/// than per file: a set declared in several files is rendered once.
fn cmd_describe(args: DescribeArgs) -> Result<(), String> {
    let paths = combined_config_args(&args.configs, &args.config);
    let files = discover_configs(&paths)?;
    if files.is_empty() {
        return Err(format!(
            "no connector configs (*.toml) found in {}",
            paths.join(", ")
        ));
    }
    let mut configs = files
        .iter()
        .map(|path| load_config(&path.display().to_string()))
        .collect::<Result<Vec<_>, _>>()?;
    // A blank `--set` means "none given", as an empty `default_set` does in the flow: an unset
    // variable in a provisioning script (`--set "$PARAM_SET"`) must not force every point into
    // a nameless set. The C build applies the same rule.
    let forced = args
        .set
        .as_deref()
        .map(tedge_dot_sdk::descriptor::trim_c)
        .filter(|s| !s.is_empty());
    // A pattern that was given must match a device somewhere — `*` included, as in the C build:
    // what decides is whether `-d` was given, not what it says.
    if let Some(pattern) = &args.device {
        for config in &mut configs {
            config.devices.retain(|d| wildcard_match(pattern, &d.name));
        }
        if configs.iter().all(|config| config.devices.is_empty()) {
            return Err(format!("no device matches '{pattern}'"));
        }
    }
    // Parameter ids become fragment keys on the device twin, so they must be plain identifiers.
    let bad = tedge_dot_sdk::descriptor::invalid_keys_across(&configs, forced);
    if !bad.is_empty() {
        return Err(format!(
            "parameter keys must match [A-Za-z0-9_]: {}",
            bad.join(", ")
        ));
    }
    // A fragment holds one value per key, and a key naming its set leaves no room for `set` or
    // `group`. Worded like the C build (impl/c/src/main.c).
    let conflicts = tedge_dot_sdk::descriptor::key_conflicts_across(&configs, forced);
    if !conflicts.is_empty() {
        return Err(format!("conflicting parameter keys: {}", conflicts.join(", ")));
    }
    // A DTM identifier is tenant-wide, so a set named after the protocol is shared with every
    // other device type that speaks it. Declaring the device type is what keeps them apart.
    if forced.is_none() {
        // Worded and shaped exactly like the C build's warning (impl/c/src/main.c): the two
        // CLIs are meant to be interchangeable, and `describe-parity.sh` compares stderr.
        for warning in tedge_dot_sdk::descriptor::type_warnings_across(&configs) {
            eprintln!("{warning}");
        }
        // One untyped-device warning per protocol, in the order the protocols first appear:
        // such a device's sets are named after its protocol, so that is what it collides with.
        let mut protocols: Vec<&str> = Vec::new();
        for config in &configs {
            if !protocols.contains(&config.connector.protocol.as_str()) {
                protocols.push(&config.connector.protocol);
            }
        }
        for protocol in protocols {
            let untyped = tedge_dot_sdk::descriptor::untyped_devices_across(&configs, protocol);
            if !untyped.is_empty() {
                eprintln!(
                    "warning: device(s) {} declare no `type`, so their parameter sets are named \
                     after the protocol ('{protocol}_...') and collide with every other \
                     {protocol} device type in the tenant; set `type` on the device or in its \
                     point library",
                    untyped.join(", "),
                );
            }
        }
    }
    let docs: Vec<serde_json::Value> = match args.format {
        DescribeFormat::C8yDtm => {
            tedge_dot_sdk::descriptor::c8y_dtm_definitions_across(&configs, forced)
        }
    };
    if args.compact {
        for doc in &docs {
            println!("{doc}");
        }
    } else {
        let out = serde_json::to_string_pretty(&docs).map_err(|e| e.to_string())?;
        println!("{out}");
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

/// Load and parse a connector configuration file, resolving the point libraries its devices
/// reference (`points_from`, contract §3.4).
fn load_config(path: &str) -> Result<ConnectorConfig, String> {
    tedge_dot_sdk::library::load(Path::new(path))
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

    fn config_with(connector_section: &str) -> ConnectorConfig {
        toml::from_str(&format!("[connector]\nprotocol = \"modbus\"\n{connector_section}")).unwrap()
    }

    /// A device two configs of one protocol define would have its commands answered by both
    /// instances; the same name under different protocols is two different devices.
    #[test]
    fn duplicate_devices_are_found_per_protocol() {
        let dir = std::env::temp_dir().join(format!("tdot-dup-devices-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let config = |protocol: &str, service: &str, devices: &[&str]| {
            let mut text =
                format!("[connector]\nprotocol = \"{protocol}\"\nservice_name = \"{service}\"\n");
            for device in devices {
                text.push_str(&format!("[[device]]\nname = \"{device}\"\nprotocol_address = {{}}\n"));
            }
            text
        };
        let a = write(&dir, "a.toml", &config("modbus", "a", &["plc-1", "plc-2"]));
        let b = write(&dir, "b.toml", &config("modbus", "b", &["plc-2"]));
        let c = write(&dir, "c.toml", &config("opcua", "c", &["plc-1"]));
        // A disabled definition (§3.3) owns nothing, so it duplicates nothing — and one carrying
        // nothing but its name must not hide the enabled devices of its file (plc-2 here).
        let d = write(
            &dir,
            "d.toml",
            "[connector]\nprotocol = \"modbus\"\nservice_name = \"d\"\n\
             [[device]]\nname = \"plc-1\"\nenabled = false\n\
             [[device]]\nname = \"plc-2\"\nprotocol_address = {}\n",
        );

        let duplicates = duplicate_devices(&[a.clone(), b.clone(), c, d.clone()]);
        assert_eq!(
            duplicates,
            vec![(
                ("modbus".to_string(), "plc-2".to_string()),
                vec![
                    a.display().to_string(),
                    b.display().to_string(),
                    d.display().to_string()
                ]
            )]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stall_timeout_defaults_and_is_disabled_by_zero() {
        assert_eq!(stall_timeout(&config_with("")), Duration::from_secs(120));
        assert_eq!(
            stall_timeout(&config_with("stall_timeout = \"0\"\n")),
            Duration::ZERO
        );
        assert_eq!(
            stall_timeout(&config_with("stall_timeout = \"5m\"\n")),
            Duration::from_secs(300)
        );
    }

    /// A watchdog shorter than the per-call bound would read one slow-but-legitimate call as a
    /// hang and restart the connector in a loop, so it is raised to twice the call bound.
    #[test]
    fn stall_timeout_is_raised_above_the_operation_bound() {
        let config = config_with("operation_timeout = \"30s\"\nstall_timeout = \"10s\"\n");
        assert_eq!(stall_timeout(&config), Duration::from_secs(60));
        // A value that already clears the floor is honoured as configured.
        let config = config_with("operation_timeout = \"5s\"\nstall_timeout = \"20s\"\n");
        assert_eq!(stall_timeout(&config), Duration::from_secs(20));
    }

    /// The watchdog must stay silent while the loop is alive, and report once it stops.
    /// Real time (not tokio's paused clock): the liveness marker is measured with
    /// `std::time::Instant`, which a paused clock does not advance.
    #[tokio::test]
    async fn stall_watchdog_fires_only_once_progress_stops() {
        let limit = Duration::from_millis(300);
        let progress = runtime::Progress::new();

        let alive = {
            let progress = progress.clone();
            tokio::spawn(async move {
                loop {
                    progress.mark();
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            })
        };
        let silent = tokio::time::timeout(
            Duration::from_millis(1500),
            stall_watchdog(progress.clone(), limit),
        )
        .await;
        assert!(silent.is_err(), "watchdog fired while the loop was alive");

        alive.abort();
        let reason = tokio::time::timeout(
            Duration::from_secs(5),
            stall_watchdog(progress, limit),
        )
        .await
        .expect("watchdog did not fire after progress stopped");
        assert!(reason.contains("no progress"), "unexpected reason: {reason}");
    }

    /// A connector abandoned because it did not stop in time must really stop: cancelling its
    /// supervisor cancels the attempt the supervisor awaits, rather than detaching it.
    #[tokio::test]
    async fn cancelling_a_supervisor_cancels_its_attempt() {
        let (started_tx, started) = tokio::sync::oneshot::channel();
        let (alive_tx, mut alive) = tokio::sync::mpsc::channel::<()>(1);
        let supervisor = tokio::spawn(async move {
            let _ = runtime::AbortOnDrop(tokio::spawn(async move {
                let _alive = alive_tx;
                let _ = started_tx.send(());
                std::future::pending::<()>().await
            }))
            .await;
        });
        started.await.expect("the attempt did not start");
        supervisor.abort();
        let ended = tokio::time::timeout(Duration::from_secs(2), alive.recv()).await;
        assert!(
            matches!(ended, Ok(None)),
            "the attempt kept running after its supervisor was cancelled"
        );
    }

    /// Disabled means disabled: it must never resolve.
    #[tokio::test]
    async fn stall_watchdog_disabled_never_fires() {
        let progress = runtime::Progress::new();
        let result = tokio::time::timeout(
            Duration::from_millis(300),
            stall_watchdog(progress, Duration::ZERO),
        )
        .await;
        assert!(result.is_err(), "disabled watchdog must never resolve");
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

    /// One file is one config however it is spelled (judged by where the path resolves to), a
    /// hidden file named just `.toml` has no extension and is not a config, and a path that is
    /// not a regular file — a pipe, `-c <(generate-config)` — is still taken. The C build's
    /// `collect_configs` follows the same rules (pinned by `describe-parity.sh`).
    #[test]
    fn discover_configs_judges_files_not_spellings() {
        let dir = std::env::temp_dir().join(format!("tedge-dot-spelling-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        write(&dir, "a.toml", "");
        write(&dir, ".toml", "");

        let spelled = format!("{}//a.toml", dir.display());
        let found = discover_configs(&[
            spelled.clone(),
            dir.display().to_string(),
            format!("{}/./a.toml", dir.display()),
        ])
        .unwrap();
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].as_os_str(), spelled.as_str(), "kept as first named");

        #[cfg(unix)]
        assert_eq!(
            discover_configs(&["/dev/null".into()]).unwrap(),
            vec![PathBuf::from("/dev/null")]
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// `run` re-reads its configs, so it refuses a path it could only read once (a pipe); a file
    /// and a symlink to one are fine.
    #[test]
    fn run_refuses_a_config_it_cannot_read_again() {
        let dir = std::env::temp_dir().join(format!("tedge-dot-rereadable-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = write(&dir, "a.toml", "");
        assert_eq!(require_rereadable(std::slice::from_ref(&file)), Ok(()));
        #[cfg(unix)]
        {
            let link = dir.join("link.toml");
            std::os::unix::fs::symlink(&file, &link).unwrap();
            assert_eq!(require_rereadable(&[link]), Ok(()));
            let err = require_rereadable(&[PathBuf::from("/dev/null")]).unwrap_err();
            assert!(err.contains("not a regular file"), "{err}");
        }
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
        assert_eq!(
            combined_config_args(&args.configs, &args.config),
            vec!["a.toml", "b.toml"]
        );
        assert_eq!(combined_config_args(&[], &[]), vec![DEFAULT_CONFIG_DIR]);
    }

    /// `describe` takes the same paths as `run` and defaults to the same directory: the one the
    /// packaged service runs, so its definitions cover every connector of that service.
    #[test]
    fn describe_takes_the_config_paths_run_does() {
        let cli = Cli::parse_from(["tedge-dot", "describe", "a.toml", "-c", "dir", "-c", "b.toml"]);
        let Command::Describe(args) = cli.command else {
            panic!("not parsed as describe");
        };
        assert_eq!(
            combined_config_args(&args.configs, &args.config),
            vec!["a.toml", "dir", "b.toml"]
        );

        let cli = Cli::parse_from(["tedge-dot", "describe"]);
        let Command::Describe(args) = cli.command else {
            panic!("not parsed as describe");
        };
        assert_eq!(
            combined_config_args(&args.configs, &args.config),
            vec![DEFAULT_CONFIG_DIR]
        );
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
            // A device switched off with nothing but its name (§3.3) is valid, and must not hide
            // the connector settings of its file.
            "[connector]\nprotocol = \"opcua\"\nlog_level = \"debug\"\n\
             [[device]]\nname = \"off\"\nenabled = false\n",
        );

        assert_eq!(pick_log_level(std::slice::from_ref(&info)), "info");
        assert_eq!(pick_log_level(&[info, debug]), "debug");
        assert_eq!(pick_log_level(&[]), "info");

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
