//! Device credentials: load/save with mode 0600. Never log device_token.

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::util::{opt_str, py_str, set_mode, truthy, write_durable};
use crate::JsonObject;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Warehouse {
    pub id: Option<String>,
    pub name: Option<String>,
    pub code: Option<String>,
}

impl Warehouse {
    fn from_json(item: &JsonObject) -> Self {
        Warehouse {
            id: opt_str(item.get("id")),
            name: opt_str(item.get("name")),
            code: opt_str(item.get("code")),
        }
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Credentials {
    pub node_id: String,
    pub device_token: String,
    pub name: Option<String>,
    pub hostname: Option<String>,
    pub organization_id: Option<String>,
    pub organization_name: Option<String>,
    pub organization_slug: Option<String>,
    pub warehouse_id: Option<String>,
    pub warehouse_name: Option<String>,
    pub warehouse_code: Option<String>,
    /// Full list from claim/whoami when the node serves multiple warehouses.
    pub warehouses: Option<Vec<Warehouse>>,
}

// Hand-written so the token can never leak through `{:?}` in a log line.
impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("node_id", &self.node_id)
            .field("device_token", &"<redacted>")
            .field("name", &self.name)
            .field("organization_name", &self.organization_name)
            .field("warehouse_label", &self.warehouse_label())
            .finish_non_exhaustive()
    }
}

impl Credentials {
    /// Safe for status/CLI — never includes device_token.
    pub fn public_dict(&self) -> JsonObject {
        let v = json!({
            "node_id": self.node_id,
            "name": self.name,
            "hostname": self.hostname,
            "organization_name": self.organization_name,
            "warehouse_name": self.warehouse_name,
            "warehouse_code": self.warehouse_code,
            "warehouses": self.warehouses,
            "warehouse_label": self.warehouse_label(),
        });
        match v {
            Value::Object(m) => m,
            _ => unreachable!(),
        }
    }

    /// LCD / status text for warehouse(s).
    ///
    /// - Multiple warehouses with codes → comma-separated codes (e.g. "DFD, MAIN")
    /// - Otherwise prefer the primary name, then code.
    pub fn warehouse_label(&self) -> String {
        let items: &[Warehouse] = self.warehouses.as_deref().unwrap_or(&[]);
        let codes: Vec<&str> = items
            .iter()
            .filter_map(|w| w.code.as_deref().map(str::trim))
            .filter(|c| !c.is_empty())
            .collect();
        if items.len() > 1 && !codes.is_empty() {
            return codes.join(", ");
        }
        if let Some(n) = self.warehouse_name.as_deref().filter(|s| !s.is_empty()) {
            return n.to_string();
        }
        if let Some(c) = self.warehouse_code.as_deref().filter(|s| !s.is_empty()) {
            return c.to_string();
        }
        if let Some(w0) = items.first() {
            return w0
                .name
                .as_deref()
                .filter(|s| !s.is_empty())
                .or(w0.code.as_deref().filter(|s| !s.is_empty()))
                .unwrap_or("—")
                .to_string();
        }
        "—".into()
    }
}

fn nested_str(obj: &JsonObject, key: &str) -> Option<String> {
    opt_str(obj.get(key))
}

/// Accept warehouses[] and/or legacy single warehouse object.
fn parse_warehouses(data: &JsonObject) -> Vec<Warehouse> {
    let out: Vec<Warehouse> = match data.get("warehouses") {
        Some(Value::Array(list)) => list
            .iter()
            .filter_map(Value::as_object)
            .map(Warehouse::from_json)
            .collect(),
        _ => Vec::new(),
    };
    if !out.is_empty() {
        return out;
    }
    if let Some(Value::Object(wh)) = data.get("warehouse") {
        let any = ["id", "name", "code"]
            .iter()
            .any(|k| wh.get(*k).is_some_and(truthy));
        if any {
            return vec![Warehouse::from_json(wh)];
        }
    }
    Vec::new()
}

#[derive(Debug, thiserror::Error)]
#[error("pair response missing node_id")]
pub struct MissingNodeId;

/// Parse claim/enroll/whoami JSON into Credentials.
///
/// claim/enroll include device_token; whoami does not (caller keeps existing).
pub fn credentials_from_pair_response(data: &JsonObject) -> Result<Credentials, MissingNodeId> {
    let node_id = data
        .get("node_id")
        .filter(|v| truthy(v))
        .or_else(|| data.get("id").filter(|v| truthy(v)))
        .ok_or(MissingNodeId)?;
    let token = data
        .get("device_token")
        .filter(|v| truthy(v))
        .map(py_str)
        .unwrap_or_default();
    let empty = JsonObject::new();
    let org = match data.get("organization") {
        Some(Value::Object(o)) => o,
        _ => &empty,
    };
    let warehouses = parse_warehouses(data);
    let primary = warehouses.first().cloned().unwrap_or_default();
    Ok(Credentials {
        node_id: py_str(node_id),
        device_token: token,
        name: nested_str(data, "name"),
        hostname: nested_str(data, "hostname"),
        organization_id: nested_str(org, "id"),
        organization_name: nested_str(org, "name"),
        organization_slug: nested_str(org, "slug"),
        warehouse_id: primary.id,
        warehouse_name: primary.name,
        warehouse_code: primary.code,
        warehouses: (!warehouses.is_empty()).then_some(warehouses),
    })
}

/// Update public fields from whoami without clearing device_token.
pub fn merge_whoami(creds: &Credentials, data: &JsonObject) -> Result<Credentials, MissingNodeId> {
    let mut merged = data.clone();
    merged.insert(
        "device_token".into(),
        Value::String(creds.device_token.clone()),
    );
    let mut updated = credentials_from_pair_response(&merged)?;
    if updated.device_token.is_empty() {
        updated.device_token = creds.device_token.clone();
    }
    Ok(updated)
}

pub fn load_credentials(path: &Path) -> Option<Credentials> {
    if !path.is_file() {
        return None;
    }
    let raw = fs::read_to_string(path).ok()?;
    let Value::Object(data) = serde_json::from_str::<Value>(&raw).ok()? else {
        return None;
    };
    let node_id = data.get("node_id").filter(|v| truthy(v))?;
    let token = data.get("device_token").filter(|v| truthy(v))?;
    let warehouses = match data.get("warehouses") {
        Some(Value::Array(list)) => {
            let ws: Vec<Warehouse> = list
                .iter()
                .filter_map(Value::as_object)
                .map(Warehouse::from_json)
                .collect();
            (!ws.is_empty()).then_some(ws)
        }
        _ => None,
    };
    let s = |k: &str| opt_str(data.get(k));
    Some(Credentials {
        node_id: py_str(node_id),
        device_token: py_str(token),
        name: s("name"),
        hostname: s("hostname"),
        organization_id: s("organization_id"),
        organization_name: s("organization_name"),
        organization_slug: s("organization_slug"),
        warehouse_id: s("warehouse_id"),
        warehouse_name: s("warehouse_name"),
        warehouse_code: s("warehouse_code"),
        warehouses,
    })
}

/// Atomically write credentials.json with mode 0600. Never logs token.
pub fn save_credentials(path: &Path, creds: &Credentials) -> io::Result<()> {
    let mut raw = serde_json::to_string_pretty(creds).map_err(io::Error::other)?;
    raw.push('\n');
    write_durable(path, raw.as_bytes(), 0o600, false)?;
    set_mode(path, 0o600)
}

/// Delete credentials file. Returns true if a file was removed.
pub fn clear_credentials(path: &Path) -> io::Result<bool> {
    if !path.is_file() {
        return Ok(false);
    }
    fs::remove_file(path)?;
    Ok(true)
}

/// Return file mode bits (e.g. 0o600) or None if missing.
pub fn credentials_mode(path: &Path) -> Option<u32> {
    fs::metadata(path)
        .ok()
        .map(|m| m.permissions().mode() & 0o7777)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claim_response() -> JsonObject {
        let v = json!({
            "node_id": "node-uuid-1",
            "device_token": "secret-device-token-do-not-log",
            "name": "Pack station 1",
            "hostname": "vesyl-print-01",
            "status": "offline",
            "warehouse": {"id": "wh-1", "name": "Main Warehouse", "code": "MAIN"},
            "organization": {"id": "org-1", "name": "Acme Corp", "slug": "acme"},
        });
        v.as_object().unwrap().clone()
    }

    #[test]
    fn parse_claim_response() {
        let creds = credentials_from_pair_response(&claim_response()).unwrap();
        assert_eq!(creds.node_id, "node-uuid-1");
        assert_eq!(creds.device_token, "secret-device-token-do-not-log");
        assert_eq!(creds.organization_name.as_deref(), Some("Acme Corp"));
        assert_eq!(creds.warehouse_name.as_deref(), Some("Main Warehouse"));
        assert_eq!(creds.warehouse_code.as_deref(), Some("MAIN"));
        assert_eq!(creds.warehouse_label(), "Main Warehouse");
        assert!(!creds.public_dict().contains_key("device_token"));
        assert!(!format!("{creds:?}").contains("secret-device-token"));
    }

    #[test]
    fn multiple_warehouses_label_uses_codes() {
        let mut data = claim_response();
        data.remove("warehouse");
        data.insert(
            "warehouses".into(),
            json!([
                {"id": "w1", "name": "Desert Fulfillment Depot", "code": "DFD"},
                {"id": "w2", "name": "Main Warehouse", "code": "MAIN"},
            ]),
        );
        let creds = credentials_from_pair_response(&data).unwrap();
        assert_eq!(creds.warehouses.as_ref().unwrap().len(), 2);
        assert_eq!(creds.warehouse_code.as_deref(), Some("DFD"));
        assert_eq!(creds.warehouse_label(), "DFD, MAIN");
    }

    #[test]
    fn warehouses_array_preferred_over_singular() {
        let mut data = claim_response();
        data.insert(
            "warehouse".into(),
            json!({"id": "old", "name": "Legacy", "code": "LEG"}),
        );
        data.insert(
            "warehouses".into(),
            json!([{"id": "w1", "name": "A", "code": "AAA"}, {"id": "w2", "name": "B", "code": "BBB"}]),
        );
        let creds = credentials_from_pair_response(&data).unwrap();
        assert_eq!(creds.warehouse_label(), "AAA, BBB");
    }

    #[test]
    fn missing_node_id_errors() {
        assert!(credentials_from_pair_response(&JsonObject::new()).is_err());
    }

    #[test]
    fn save_mode_0600_and_roundtrip() {
        let td = tempfile::tempdir().unwrap();
        let path = td.path().join("credentials.json");
        let creds = credentials_from_pair_response(&claim_response()).unwrap();
        save_credentials(&path, &creds).unwrap();
        assert_eq!(credentials_mode(&path), Some(0o600));
        let loaded = load_credentials(&path).unwrap();
        assert_eq!(loaded, creds);
    }

    #[test]
    fn loads_python_written_file() {
        // Python asdict() output — the Rust agent must read existing devices' files.
        let td = tempfile::tempdir().unwrap();
        let path = td.path().join("credentials.json");
        fs::write(
            &path,
            r#"{"node_id": "n1", "device_token": "t", "name": null, "hostname": "h",
               "organization_id": null, "organization_name": "Acme", "organization_slug": null,
               "warehouse_id": null, "warehouse_name": null, "warehouse_code": null,
               "warehouses": [{"id": "w1", "name": null, "code": "W1"}]}"#,
        )
        .unwrap();
        let c = load_credentials(&path).unwrap();
        assert_eq!(c.organization_name.as_deref(), Some("Acme"));
        assert_eq!(c.warehouse_label(), "W1");
    }

    #[test]
    fn merge_whoami_keeps_token() {
        let creds = credentials_from_pair_response(&claim_response()).unwrap();
        let who = json!({"node_id": "node-uuid-1", "name": "Renamed"});
        let merged = merge_whoami(&creds, who.as_object().unwrap()).unwrap();
        assert_eq!(merged.device_token, creds.device_token);
        assert_eq!(merged.name.as_deref(), Some("Renamed"));
    }

    #[test]
    fn clear() {
        let td = tempfile::tempdir().unwrap();
        let path = td.path().join("credentials.json");
        let creds = credentials_from_pair_response(&claim_response()).unwrap();
        save_credentials(&path, &creds).unwrap();
        assert!(clear_credentials(&path).unwrap());
        assert!(load_credentials(&path).is_none());
        assert!(!clear_credentials(&path).unwrap());
    }
}
