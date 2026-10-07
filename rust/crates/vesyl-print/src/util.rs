//! Small helpers shared across modules: Python-ish JSON coercion and durable writes.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

use chrono::{SecondsFormat, Utc};
use serde_json::Value;

/// UTC now as `2026-07-15T20:00:00+00:00` (Python `isoformat()` without micros).
pub fn utc_now_iso() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Secs, false)
}

/// Python truthiness for a JSON value.
pub fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// `bool(obj.get(key))`.
pub fn get_truthy(obj: &serde_json::Map<String, Value>, key: &str) -> bool {
    obj.get(key).is_some_and(truthy)
}

/// Python `str(v)` for scalar JSON values (strings unquoted).
pub fn py_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::Null => "None".into(),
        other => other.to_string(),
    }
}

/// `str(v)` when present and not null, else `None`.
pub fn opt_str(v: Option<&Value>) -> Option<String> {
    match v {
        None | Some(Value::Null) => None,
        Some(v) => Some(py_str(v)),
    }
}

/// `str(obj[key])` when the value is truthy (the `x.get(k) or None` idiom).
pub fn truthy_str(obj: &serde_json::Map<String, Value>, key: &str) -> Option<String> {
    obj.get(key).filter(|v| truthy(v)).map(py_str)
}

/// Python `int(v)`: ints, truncated floats, bools and numeric strings.
pub fn py_int(v: &Value) -> Option<i64> {
    match v {
        Value::Bool(b) => Some(*b as i64),
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f.trunc() as i64)),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// Write `data` to `path` via `path.tmp` + fsync + rename, creating the temp
/// file with `mode`. Optionally fsyncs the parent directory.
pub fn write_durable(path: &Path, data: &[u8], mode: u32, sync_dir: bool) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut tmp_name = path.as_os_str().to_owned();
    tmp_name.push(".tmp");
    let tmp = Path::new(&tmp_name);
    let result = (|| {
        let mut f = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(mode)
            .open(tmp)?;
        f.write_all(data)?;
        f.flush()?;
        f.sync_all()?;
        fs::rename(tmp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(tmp);
        return result;
    }
    if sync_dir {
        if let Some(parent) = path.parent() {
            if let Ok(d) = File::open(parent) {
                let _ = d.sync_all();
            }
        }
    }
    Ok(())
}

pub fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn truthiness_matches_python() {
        assert!(!truthy(&json!(null)));
        assert!(!truthy(&json!("")));
        assert!(!truthy(&json!(0)));
        assert!(!truthy(&json!({})));
        assert!(truthy(&json!("x")));
        assert!(truthy(&json!(1)));
    }

    #[test]
    fn py_int_coerces() {
        assert_eq!(py_int(&json!("15")), Some(15));
        assert_eq!(py_int(&json!(15.9)), Some(15));
        assert_eq!(py_int(&json!(true)), Some(1));
        assert_eq!(py_int(&json!("x")), None);
    }
}
