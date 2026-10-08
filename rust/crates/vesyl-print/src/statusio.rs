//! Agent ↔ LCD status file (JSON under state_dir).

use std::fs;
use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::util::{opt_str, utc_now_iso, write_durable};
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
    /// The file's other top-level keys: [`read_status`] keeps them here and
    /// [`write_status`] writes them back, so a read-modify-write keeps what
    /// it does not know (keys a newer agent added, after a rollback). The
    /// fields above win over a key of the same name.
    pub extra: JsonObject,
}

/// The top-level keys [`AgentStatus`] has fields for.
const SCHEMA_KEYS: &[&str] = &[
    "pairing",
    "cloud",
    "node_id",
    "name",
    "organization_name",
    "warehouse_name",
    "last_heartbeat_at",
    "last_error",
    "agent_version",
    "updated_at",
];

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
            d.entry(k.clone()).or_insert_with(|| v.clone());
        }
        d
    }
}

pub fn write_status(path: &Path, status: &mut AgentStatus) -> io::Result<()> {
    status.updated_at = Some(utc_now_iso());
    let mut raw = serde_json::to_string_pretty(&status.to_dict()).map_err(io::Error::other)?;
    raw.push('\n');
    // Atomic write so LCD never reads a partial file. 0600 like Python's
    // mkstemp; a root CLI run leaves it owned by the service user.
    write_durable(path, raw.as_bytes(), 0o600, false)
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
    let mut st = AgentStatus {
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
    };
    st.extra = data
        .into_iter()
        .filter(|(k, _)| !SCHEMA_KEYS.contains(&k.as_str()))
        .collect();
    Some(st)
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
        assert_eq!(loaded.extra, st.extra);
    }

    /// J7: read_status dropped every key it has no field for, so a
    /// read-modify-write (the revoked heartbeat) lost them: keys a newer
    /// agent wrote, after a rollback. They come back in `extra`, schema keys
    /// never do, and a field wins over an extra of the same name.
    #[test]
    fn unknown_keys_survive_a_read_modify_write() {
        let td = tempfile::tempdir().unwrap();
        let path = td.path().join("status.json");
        fs::write(
            &path,
            r#"{"pairing": "revoked", "cloud": "online", "x": 1, "printers": ["Zebra"]}"#,
        )
        .unwrap();
        let mut st = read_status(&path).unwrap();
        assert_eq!(st.pairing, PairingState::Revoked);
        assert_eq!(
            Value::Object(st.extra.clone()),
            json!({"x": 1, "printers": ["Zebra"]})
        );
        st.cloud = CloudState::Offline;
        write_status(&path, &mut st).unwrap();
        let raw: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(raw["x"], 1);
        assert_eq!(raw["printers"], json!(["Zebra"]));
        assert_eq!(raw["cloud"], "offline");
        assert_eq!(raw["pairing"], "revoked");

        st.extra.insert("cloud".into(), json!("online"));
        assert_eq!(st.to_dict()["cloud"], "offline");
    }

    #[test]
    fn status_is_private_and_leaves_no_temp_files() {
        use std::os::unix::fs::PermissionsExt;
        let td = tempfile::tempdir().unwrap();
        let path = td.path().join("status.json");
        for _ in 0..2 {
            write_status(&path, &mut AgentStatus::default()).unwrap();
        }
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let names: Vec<_> = fs::read_dir(td.path()).unwrap().collect();
        assert_eq!(names.len(), 1, "temp file left behind");
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
