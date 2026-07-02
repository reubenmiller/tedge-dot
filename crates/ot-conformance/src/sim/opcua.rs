//! Built-in OPC UA simulator (`kind = "opcua"`).
//!
//! An embedded `async-opcua` server (anonymous, security `None`) whose variables are backed
//! by the simulator's own state through read/write callbacks — so the harness has exact
//! ground truth (current values, write counters, seeded bad-status nodes) without poking at
//! the server's address space. A [`TransportProxy`] fronts the server, and the server
//! *advertises the proxy's port* in its endpoints: OPC UA clients (re)connect to the
//! advertised endpoint URL, so without this the session would bypass the proxy and
//! transport-drop checks (B5) would test nothing.

use super::proxy::TransportProxy;
use super::{PointData, PointSpec, Simulator};
use opcua::nodes::VariableBuilder;
use opcua::server::diagnostics::NamespaceMetadata;
use opcua::server::node_manager::memory::{simple_node_manager, SimpleNodeManager};
use opcua::server::{ServerBuilder, ServerHandle};
use opcua::types::{DataTypeId, DataValue, DateTime, NodeId, ObjectId, StatusCode, UAString, Variant};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tedge_dot_sdk::DataType;
use tracing::debug;

const NAMESPACE_URI: &str = "urn:tedge-dot-conformance:opcua";
/// The index the custom namespace lands on: the application URI is always namespace 1, and
/// the simulator registers exactly one namespace after it. Connector configs address nodes
/// as `{ namespace = 2, identifier = "..." }`.
const NAMESPACE_INDEX: u16 = 2;

/// Seed file: the variables the server exposes.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Seed {
    /// Free-text description; ignored.
    #[serde(default, rename = "$comment")]
    _comment: Option<String>,
    variables: Vec<SeedVariable>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SeedVariable {
    /// String node identifier within the simulator namespace.
    identifier: String,
    datatype: DataType,
    value: serde_json::Value,
    #[serde(default)]
    writable: bool,
    /// Reads of this node answer with `BadSensorFailure` (behavioural check B4).
    #[serde(default)]
    bad_status: bool,
}

struct VarState {
    variant: Variant,
    bad_status: bool,
    writes: usize,
}

type SharedState = Arc<Mutex<HashMap<String, VarState>>>;

pub struct OpcuaSim {
    state: SharedState,
    outage: Arc<AtomicBool>,
    proxy: TransportProxy,
    handle: ServerHandle,
}

impl Drop for OpcuaSim {
    fn drop(&mut self) {
        self.handle.cancel();
    }
}

impl OpcuaSim {
    pub async fn start(seed_path: &Path) -> Result<OpcuaSim, String> {
        let text = std::fs::read_to_string(seed_path)
            .map_err(|e| format!("failed to read seed '{}': {e}", seed_path.display()))?;
        let seed: Seed = serde_json::from_str(&text)
            .map_err(|e| format!("failed to parse seed '{}': {e}", seed_path.display()))?;

        let mut state = HashMap::new();
        for var in &seed.variables {
            state.insert(
                var.identifier.clone(),
                VarState {
                    variant: variant_from_seed(var.datatype, &var.value)?,
                    bad_status: var.bad_status,
                    writes: 0,
                },
            );
        }
        let state: SharedState = Arc::new(Mutex::new(state));
        let outage = Arc::new(AtomicBool::new(false));

        // The server listens on an internal port; the proxy in front owns the port the
        // connector sees AND the port the server advertises.
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .map_err(|e| format!("opcua sim bind failed: {e}"))?;
        let internal_port = listener
            .local_addr()
            .map_err(|e| format!("opcua sim local_addr: {e}"))?
            .port();
        let proxy = TransportProxy::start(internal_port).await?;

        let (server, handle) = ServerBuilder::new_anonymous("tedge-dot-conformance")
            .application_uri("urn:tedge-dot-conformance")
            .product_uri("urn:tedge-dot-conformance")
            .host("127.0.0.1")
            .port(proxy.port())
            .discovery_urls(vec![format!("opc.tcp://127.0.0.1:{}", proxy.port())])
            .with_node_manager(simple_node_manager(
                NamespaceMetadata {
                    namespace_uri: NAMESPACE_URI.to_owned(),
                    ..Default::default()
                },
                "simple",
            ))
            .build()
            .map_err(|e| format!("opcua sim server build failed: {e}"))?;

        let nm = handle
            .node_managers()
            .get_of_type::<SimpleNodeManager>()
            .ok_or("opcua sim: no SimpleNodeManager")?;
        let ns = handle
            .get_namespace_index(NAMESPACE_URI)
            .ok_or("opcua sim: namespace not registered")?;
        if ns != NAMESPACE_INDEX {
            return Err(format!(
                "opcua sim: namespace landed on index {ns}, expected {NAMESPACE_INDEX} \
                 (connector configs hardcode it)"
            ));
        }

        for var in &seed.variables {
            let node = NodeId::new(ns, var.identifier.as_str());
            {
                let mut space = nm.address_space().write();
                let mut builder = VariableBuilder::new(&node, &var.identifier, &var.identifier)
                    .data_type(data_type_id(var.datatype))
                    .value(variant_from_seed(var.datatype, &var.value)?)
                    .organized_by(ObjectId::ObjectsFolder);
                if var.writable {
                    builder = builder.writable();
                }
                builder.insert(&mut *space);
            }

            // Serve reads from the simulator state (with outage / bad-status overrides)...
            let read_state = state.clone();
            let read_outage = outage.clone();
            let read_id = var.identifier.clone();
            nm.inner().add_read_callback(node.clone(), move |_, _, _| {
                if read_outage.load(Ordering::SeqCst) {
                    return Err(StatusCode::BadInternalError);
                }
                let state = read_state.lock().unwrap();
                let var = state.get(&read_id).ok_or(StatusCode::BadNodeIdUnknown)?;
                if var.bad_status {
                    return Err(StatusCode::BadSensorFailure);
                }
                Ok(DataValue {
                    value: Some(var.variant.clone()),
                    status: Some(StatusCode::Good),
                    source_timestamp: Some(DateTime::now()),
                    server_timestamp: Some(DateTime::now()),
                    ..Default::default()
                })
            });

            // ...and record writes into it (the callback replaces the default node store, so
            // the simulator is the single source of truth). Access levels are enforced by
            // the server before the callback runs.
            if var.writable {
                let write_state = state.clone();
                let write_id = var.identifier.clone();
                nm.inner().add_write_callback(node.clone(), move |dv, _| {
                    let Some(variant) = dv.value else {
                        return StatusCode::BadNothingToDo;
                    };
                    let mut state = write_state.lock().unwrap();
                    let Some(var) = state.get_mut(&write_id) else {
                        return StatusCode::BadNodeIdUnknown;
                    };
                    var.variant = variant;
                    var.writes += 1;
                    StatusCode::Good
                });
            }
        }

        tokio::spawn(async move {
            if let Err(e) = server.run_with(listener).await {
                debug!("opcua sim server stopped: {e}");
            }
        });

        Ok(OpcuaSim {
            state,
            outage,
            proxy,
            handle,
        })
    }

    fn lookup<T>(
        &self,
        point: &PointSpec,
        f: impl FnOnce(&VarState) -> Result<T, String>,
    ) -> Result<T, String> {
        let identifier = identifier_from(&point.address)?;
        let state = self.state.lock().unwrap();
        let var = state
            .get(&identifier)
            .ok_or_else(|| format!("no seeded variable '{identifier}'"))?;
        f(var)
    }
}

#[async_trait::async_trait]
impl Simulator for OpcuaSim {
    fn port(&self) -> u16 {
        self.proxy.port()
    }

    fn point_data(&self, point: &PointSpec) -> Result<PointData, String> {
        self.lookup(point, |var| {
            if var.bad_status {
                return Err("seeded bad-status variable".into());
            }
            Ok(PointData {
                bytes: variant_raw_bytes(&var.variant)?,
                raw_group: 1,
            })
        })
    }

    fn is_invalid(&self, point: &PointSpec) -> bool {
        self.lookup(point, |var| Ok(var.bad_status)).unwrap_or(false)
    }

    fn write_count(&self, point: &PointSpec) -> Result<usize, String> {
        self.lookup(point, |var| Ok(var.writes))
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
        table.insert(
            "endpoint".into(),
            toml::Value::String(format!("opc.tcp://127.0.0.1:{}", self.proxy.port())),
        );
        Ok(())
    }
}

/// The point's node identifier within the simulator namespace, from its `address` object.
fn identifier_from(address: &serde_json::Value) -> Result<String, String> {
    if let Some(ns) = address.get("namespace").and_then(|n| n.as_u64()) {
        if ns != NAMESPACE_INDEX as u64 {
            return Err(format!(
                "point addresses namespace {ns}; the simulator serves namespace {NAMESPACE_INDEX}"
            ));
        }
    }
    address
        .get("identifier")
        .and_then(|i| i.as_str())
        .map(str::to_string)
        .ok_or_else(|| format!("opcua point address needs a string 'identifier': {address}"))
}

fn data_type_id(dt: DataType) -> DataTypeId {
    match dt {
        DataType::Bool => DataTypeId::Boolean,
        DataType::Int8 => DataTypeId::SByte,
        DataType::Uint8 => DataTypeId::Byte,
        DataType::Int16 => DataTypeId::Int16,
        DataType::Uint16 => DataTypeId::UInt16,
        DataType::Int32 => DataTypeId::Int32,
        DataType::Uint32 => DataTypeId::UInt32,
        DataType::Int64 => DataTypeId::Int64,
        DataType::Uint64 => DataTypeId::UInt64,
        DataType::Float32 => DataTypeId::Float,
        DataType::Float64 => DataTypeId::Double,
        DataType::String | DataType::Bytes => DataTypeId::String,
    }
}

/// Build the seeded `Variant` for a variable (mirrors the connector's write coercion).
fn variant_from_seed(dt: DataType, value: &serde_json::Value) -> Result<Variant, String> {
    let err = || format!("seed value {value} is not valid for datatype {dt:?}");
    Ok(match dt {
        DataType::Bool => Variant::Boolean(value.as_bool().ok_or_else(err)?),
        DataType::Int8 => Variant::SByte(value.as_i64().ok_or_else(err)? as i8),
        DataType::Uint8 => Variant::Byte(value.as_u64().ok_or_else(err)? as u8),
        DataType::Int16 => Variant::Int16(value.as_i64().ok_or_else(err)? as i16),
        DataType::Uint16 => Variant::UInt16(value.as_u64().ok_or_else(err)? as u16),
        DataType::Int32 => Variant::Int32(value.as_i64().ok_or_else(err)? as i32),
        DataType::Uint32 => Variant::UInt32(value.as_u64().ok_or_else(err)? as u32),
        DataType::Int64 => Variant::Int64(value.as_i64().ok_or_else(err)?),
        DataType::Uint64 => Variant::UInt64(value.as_u64().ok_or_else(err)?),
        DataType::Float32 => Variant::Float(value.as_f64().ok_or_else(err)? as f32),
        DataType::Float64 => Variant::Double(value.as_f64().ok_or_else(err)?),
        DataType::String => {
            Variant::String(UAString::from(value.as_str().ok_or_else(err)?.to_string()))
        }
        DataType::Bytes => return Err("datatype 'bytes' is not seedable over OPC UA".into()),
    })
}

/// The raw byte echo the connector reports for a variant (big-endian value bytes — must stay
/// in lockstep with `connector-opcua`'s `variant_to_value`).
fn variant_raw_bytes(v: &Variant) -> Result<Vec<u8>, String> {
    Ok(match v {
        Variant::Boolean(b) => vec![*b as u8],
        Variant::SByte(i) => vec![*i as u8],
        Variant::Byte(u) => vec![*u],
        Variant::Int16(i) => i.to_be_bytes().to_vec(),
        Variant::UInt16(u) => u.to_be_bytes().to_vec(),
        Variant::Int32(i) => i.to_be_bytes().to_vec(),
        Variant::UInt32(u) => u.to_be_bytes().to_vec(),
        Variant::Int64(i) => i.to_be_bytes().to_vec(),
        Variant::UInt64(u) => u.to_be_bytes().to_vec(),
        Variant::Float(f) => f.to_be_bytes().to_vec(),
        Variant::Double(d) => d.to_be_bytes().to_vec(),
        Variant::String(s) => s.as_ref().to_string().into_bytes(),
        other => return Err(format!("unsupported variant {other:?}")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tedge_dot_sdk::Mode;

    fn write_seed(name: &str, contents: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("ot-conf-opcua-{}-{name}", std::process::id()));
        std::fs::write(&path, contents).unwrap();
        path
    }

    fn spec(identifier: &str, datatype: DataType) -> PointSpec {
        PointSpec {
            address: serde_json::json!({ "namespace": 2, "identifier": identifier }),
            datatype: Some(datatype),
            mode: Mode::Typed,
        }
    }

    #[tokio::test]
    async fn serves_seeded_variables_and_ground_truth() {
        let seed = write_seed(
            "basic.json",
            r#"{
                "variables": [
                    { "identifier": "Temperature", "datatype": "float64", "value": 21.5 },
                    { "identifier": "Broken", "datatype": "uint16", "value": 0, "bad_status": true }
                ]
            }"#,
        );
        let sim = OpcuaSim::start(&seed).await.unwrap();

        let temp = sim.point_data(&spec("Temperature", DataType::Float64)).unwrap();
        assert_eq!(temp.bytes, 21.5f64.to_be_bytes().to_vec());
        assert_eq!(temp.raw_group, 1);
        assert!(!sim.is_invalid(&spec("Temperature", DataType::Float64)));
        assert!(sim.is_invalid(&spec("Broken", DataType::Uint16)));
        assert_eq!(sim.write_count(&spec("Temperature", DataType::Float64)).unwrap(), 0);

        std::fs::remove_file(&seed).ok();
    }
}
