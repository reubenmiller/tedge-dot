//! Protocol-specific configuration for the OPC-UA connector. These structs fill the contract's
//! opaque slots: `connection`, `device.protocol_address`, and `point.address`.
//!
//! They are plain serde types; conversion to the `async-opcua` runtime types (e.g. `NodeId`) is
//! done in [`crate`].

use serde::Deserialize;

/// Shared `[connection]` defaults for all OPC-UA devices.
#[derive(Debug, Clone, Deserialize)]
pub struct OpcuaConnection {
    #[serde(default = "default_app_name")]
    pub application_name: String,
    #[serde(default = "default_app_uri")]
    pub application_uri: String,
    /// Default security policy (`None`, `Basic256Sha256`, ...). Per-device value wins.
    #[serde(default)]
    pub security_policy: Option<String>,
    /// Default message security mode (`none`, `sign`, `sign_and_encrypt`). Per-device value wins.
    #[serde(default)]
    pub security_mode: Option<String>,
    /// Seconds to wait for a session to activate before declaring the link down.
    #[serde(default = "default_connect_timeout")]
    pub connect_timeout_s: u64,
    /// Seconds to wait for a read/write service call. Bounds a request stuck behind a dead
    /// transport (the client queues requests while it tries to resurrect the session), so a
    /// dropped connection surfaces as bad samples instead of stalling the poll loop.
    #[serde(default = "default_request_timeout")]
    pub request_timeout_s: u64,
}

impl Default for OpcuaConnection {
    fn default() -> Self {
        OpcuaConnection {
            application_name: default_app_name(),
            application_uri: default_app_uri(),
            security_policy: None,
            security_mode: None,
            connect_timeout_s: default_connect_timeout(),
            request_timeout_s: default_request_timeout(),
        }
    }
}

/// `device.protocol_address` — how to reach one OPC-UA server endpoint.
#[derive(Debug, Clone, Deserialize)]
pub struct OpcuaEndpoint {
    /// e.g. `opc.tcp://plc.example.com:4840/`.
    pub endpoint: String,
    #[serde(default)]
    pub security_policy: Option<String>,
    #[serde(default)]
    pub security_mode: Option<String>,
    /// Optional username/password identity. Anonymous when omitted.
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
}

/// `point.address` — how to address one OPC-UA node. Either give the standard textual
/// `node_id` (`ns=2;s=Temperature`, `ns=3;i=1001`) or the structured `namespace` + `identifier`.
#[derive(Debug, Clone, Deserialize)]
pub struct NodeAddress {
    #[serde(default)]
    pub node_id: Option<String>,
    #[serde(default)]
    pub namespace: Option<u16>,
    /// String identifier (`s=`) or numeric identifier (`i=`); a JSON string or number.
    #[serde(default)]
    pub identifier: Option<serde_json::Value>,
}

fn default_app_name() -> String {
    "tedge-dot".to_string()
}
fn default_app_uri() -> String {
    "urn:tedge-dot".to_string()
}
fn default_connect_timeout() -> u64 {
    15
}
fn default_request_timeout() -> u64 {
    5
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_endpoint() {
        let v = serde_json::json!({
            "endpoint": "opc.tcp://localhost:4840/",
            "security_policy": "None",
            "security_mode": "none"
        });
        let e: OpcuaEndpoint = serde_json::from_value(v).unwrap();
        assert_eq!(e.endpoint, "opc.tcp://localhost:4840/");
        assert_eq!(e.security_policy.as_deref(), Some("None"));
        assert!(e.user.is_none());
    }

    #[test]
    fn parse_node_address_textual() {
        let v = serde_json::json!({ "node_id": "ns=2;s=Temperature" });
        let a: NodeAddress = serde_json::from_value(v).unwrap();
        assert_eq!(a.node_id.as_deref(), Some("ns=2;s=Temperature"));
    }

    #[test]
    fn parse_node_address_structured_string() {
        let v = serde_json::json!({ "namespace": 2, "identifier": "Temperature" });
        let a: NodeAddress = serde_json::from_value(v).unwrap();
        assert_eq!(a.namespace, Some(2));
        assert_eq!(a.identifier.unwrap().as_str(), Some("Temperature"));
    }

    #[test]
    fn parse_node_address_structured_numeric() {
        let v = serde_json::json!({ "namespace": 3, "identifier": 1001 });
        let a: NodeAddress = serde_json::from_value(v).unwrap();
        assert_eq!(a.namespace, Some(3));
        assert_eq!(a.identifier.unwrap().as_u64(), Some(1001));
    }
}
