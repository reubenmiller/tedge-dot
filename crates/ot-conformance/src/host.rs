//! Hosting the connector under test, and rewiring its configuration onto the harness.
//!
//! Two modes:
//! * **in-process** (default): the protocol module compiled into this harness runs under the
//!   real SDK runtime (`runtime::run_until`) — the identical code path the shipped `tedge-dot`
//!   binary uses, without needing that binary on disk.
//! * **external**: the manifest's `[harness] command` is spawned with the rewritten config
//!   path appended, for out-of-tree connectors.

use crate::sim::Simulator;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tedge_dot_sdk::{runtime, Connector, ConnectorConfig};
use tokio::sync::watch;
use tracing::warn;

/// Scratch directory for the rewritten config; removed on drop.
pub struct TempDir(PathBuf);

static TEMP_COUNTER: AtomicUsize = AtomicUsize::new(0);

impl TempDir {
    pub fn new() -> Result<TempDir, String> {
        let dir = std::env::temp_dir().join(format!(
            "ot-conformance-{}-{}",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
        Ok(TempDir(dir))
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Rewrite the connector config template: `[mqtt]` points at the test broker, every device's
/// `protocol_address` points at the simulator. Returns the rewritten path and the parsed
/// config the checks inspect.
pub fn rewrite_config(
    template: &Path,
    out_dir: &Path,
    broker_port: u16,
    sim: &dyn Simulator,
) -> Result<(PathBuf, ConnectorConfig), String> {
    let text = std::fs::read_to_string(template)
        .map_err(|e| format!("failed to read config '{}': {e}", template.display()))?;
    let mut doc: toml::Value = toml::from_str(&text)
        .map_err(|e| format!("failed to parse config '{}': {e}", template.display()))?;

    let root = doc
        .as_table_mut()
        .ok_or("connector config is not a TOML table")?;

    let mut mqtt = toml::value::Table::new();
    mqtt.insert("host".into(), toml::Value::String("127.0.0.1".into()));
    mqtt.insert("port".into(), toml::Value::Integer(broker_port as i64));
    root.insert("mqtt".into(), toml::Value::Table(mqtt));

    if let Some(devices) = root.get_mut("device").and_then(|d| d.as_array_mut()) {
        for device in devices {
            let address = device
                .get_mut("protocol_address")
                .ok_or("device without protocol_address")?;
            sim.rewrite_protocol_address(address)?;
        }
    }

    let rewritten = toml::to_string_pretty(&doc).map_err(|e| format!("re-serialize config: {e}"))?;
    let out = out_dir.join(
        template
            .file_name()
            .unwrap_or_else(|| std::ffi::OsStr::new("connector.toml")),
    );
    std::fs::write(&out, &rewritten).map_err(|e| format!("write {}: {e}", out.display()))?;

    let parsed: ConnectorConfig =
        toml::from_str(&rewritten).map_err(|e| format!("rewritten config is invalid: {e}"))?;
    Ok((out, parsed))
}

/// Select a compiled-in protocol module (mirrors the `tedge-dot` binary's factory).
pub fn build_connector(protocol: &str) -> Result<Box<dyn Connector>, String> {
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
            "protocol '{other}' is not compiled into ot-conformance (enable its cargo \
             feature); for an external binary set `[harness] command` in the manifest"
        )),
    }
}

pub enum Host {
    InProcess {
        shutdown: watch::Sender<bool>,
        task: tokio::task::JoinHandle<Result<(), String>>,
    },
    External {
        child: tokio::process::Child,
    },
}

impl Host {
    /// Start the connector against the rewritten config.
    pub async fn start(
        protocol: &str,
        command: &[String],
        config_path: &Path,
    ) -> Result<Host, String> {
        if command.is_empty() {
            let text = std::fs::read_to_string(config_path)
                .map_err(|e| format!("read {}: {e}", config_path.display()))?;
            let config: ConnectorConfig =
                toml::from_str(&text).map_err(|e| format!("parse config: {e}"))?;
            let connector = build_connector(protocol)?;
            let (shutdown, mut shutdown_rx) = watch::channel(false);
            let path = config_path.to_path_buf();
            let task = tokio::spawn(async move {
                let stop = async move {
                    let _ = shutdown_rx.wait_for(|s| *s).await;
                };
                runtime::run_until(connector, config, path, stop)
                    .await
                    .map_err(|e| e.to_string())
            });
            Ok(Host::InProcess { shutdown, task })
        } else {
            let mut cmd = tokio::process::Command::new(&command[0]);
            cmd.args(&command[1..])
                .arg(config_path)
                .kill_on_drop(true);
            let child = cmd
                .spawn()
                .map_err(|e| format!("failed to spawn '{}': {e}", command[0]))?;
            Ok(Host::External { child })
        }
    }

    /// Stop the connector, giving it a moment to publish its final health status.
    pub async fn stop(self) {
        match self {
            Host::InProcess { shutdown, task } => {
                let _ = shutdown.send(true);
                match tokio::time::timeout(Duration::from_secs(10), task).await {
                    Ok(Ok(Ok(()))) => {}
                    Ok(Ok(Err(e))) => warn!("connector exited with error: {e}"),
                    Ok(Err(join)) => warn!("connector task panicked: {join}"),
                    Err(_) => warn!("connector did not stop within 10s"),
                }
            }
            Host::External { mut child } => {
                let _ = child.kill().await;
            }
        }
    }
}
