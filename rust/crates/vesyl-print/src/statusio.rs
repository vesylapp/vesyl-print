//! Agent ↔ LCD status file (JSON under state_dir).

use std::fs;
use std::io::{self, Write};
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::util::{opt_str, utc_now_iso};
use crate::JsonObject;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PairingState {
    #[default]
    Unpaired,
    Paired,
    Revoked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CloudState {
    #[default]
    Unknown,
    Online,
    Offline,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct AgentStatus {
    pub pairing: PairingState,
    pub cloud: CloudState,
    pub node_id: Option<String>,
    pub name: Option<String>,
    pub organization_name: Option<String>,
    pub warehouse_name: Option<String>,
    pub last_heartbeat_at: Option<String>,
    pub last_error: Option<String>,
    pub agent_version: Option<String>,
    pub updated_at: Option<String>,
    /// Merged into the top level of the JSON file (printers, jobs, update, …).
    pub extra: JsonObject,
}

impl AgentStatus {
    pub fn to_dict(&self) -> JsonObject {
        let mut d = JsonObject::new();
        let s = |v: &Option<String>| v.clone().map(Value::String).unwrap_or(Value::Null);
        d.insert(
            "pairing".into(),
            serde_json::to_value(self.pairing).unwrap(),
        );
        d.insert("cloud".into(), serde_json::to_value(self.cloud).unwrap());
        d.insert("node_id".into(), s(&self.node_id));
        d.insert("name".into(), s(&self.name));
        d.insert("organization_name".into(), s(&self.organization_name));
        d.insert("warehouse_name".into(), s(&self.warehouse_name));
        d.insert("last_heartbeat_at".into(), s(&self.last_heartbeat_at));
        d.insert("last_error".into(), s(&self.last_error));
        d.insert("agent_version".into(), s(&self.agent_version));
        d.insert("updated_at".into(), s(&self.updated_at));
        for (k, v) in &self.extra {
            d.insert(k.clone(), v.clone());
        }
        d
    }
}

pub fn write_status(path: &Path, status: &mut AgentStatus) -> io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(dir)?;
    status.updated_at = Some(utc_now_iso());
    let mut raw = serde_json::to_string_pretty(&status.to_dict()).map_err(io::Error::other)?;
    raw.push('\n');
    // Atomic write so LCD never reads a partial file.
    let mut tmp = tempfile::Builder::new()
        .prefix(".status.")
        .suffix(".tmp")
        .tempfile_in(dir)?;
    tmp.write_all(raw.as_bytes())?;
    tmp.flush()?;
    tmp.as_file().sync_all()?;
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

pub fn read_status(path: &Path) -> Option<AgentStatus> {
    if !path.is_file() {
        return None;
    }
    let raw = fs::read_to_string(path).ok()?;
    let Value::Object(data) = serde_json::from_str::<Value>(&raw).ok()? else {
        return None;
    };
    let pairing = data
        .get("pairing")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();
    let cloud = data
        .get("cloud")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();
    let s = |k: &str| opt_str(data.get(k));
    Some(AgentStatus {
        pairing,
        cloud,
        node_id: s("node_id"),
        name: s("name"),
        organization_name: s("organization_name"),
        warehouse_name: s("warehouse_name"),
        last_heartbeat_at: s("last_heartbeat_at"),
        last_error: s("last_error"),
        agent_version: s("agent_version"),
        updated_at: s("updated_at"),
        extra: JsonObject::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn roundtrip() {
        let td = tempfile::tempdir().unwrap();
        let path = td.path().join("status.json");
        let mut st = AgentStatus {
            pairing: PairingState::Paired,
            cloud: CloudState::Online,
            organization_name: Some("Acme".into()),
            ..Default::default()
        };
        st.extra.insert("printers".into(), json!(["Zebra"]));
        write_status(&path, &mut st).unwrap();

        let raw: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(raw["pairing"], "paired");
        assert_eq!(raw["printers"], json!(["Zebra"]));

        let loaded = read_status(&path).unwrap();
        assert_eq!(loaded.pairing, PairingState::Paired);
        assert_eq!(loaded.organization_name.as_deref(), Some("Acme"));
        assert!(loaded.updated_at.is_some());
    }

    #[test]
    fn unknown_enum_values_fall_back() {
        let td = tempfile::tempdir().unwrap();
        let path = td.path().join("status.json");
        fs::write(&path, r#"{"pairing": "weird", "cloud": 3}"#).unwrap();
        let st = read_status(&path).unwrap();
        assert_eq!(st.pairing, PairingState::Unpaired);
        assert_eq!(st.cloud, CloudState::Unknown);
    }
}
