//! HTTP client for VESYL print/v1 REST API.

use std::time::Duration;

use serde_json::{json, Value};
use ureq::tls::{RootCerts, TlsConfig};
use ureq::Agent;

use crate::JsonObject;

const LOG: &str = "vesyl-print.cloud";
/// Pending-jobs payloads can carry base64 PDFs; ureq's default cap is 10 MB.
const MAX_RESPONSE_BYTES: u64 = 64 * 1024 * 1024;

/// API or transport error. `status` is the HTTP code or 0 for network failure.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{message}")]
pub struct CloudError {
    pub message: String,
    pub status: u16,
    pub code: Option<String>,
    pub body: Option<Value>,
}

impl CloudError {
    fn new(message: impl Into<String>, status: u16) -> Self {
        CloudError {
            message: message.into(),
            status,
            code: None,
            body: None,
        }
    }

    pub fn unauthorized(&self) -> bool {
        self.status == 401
    }

    pub fn service_disabled(&self) -> bool {
        self.status == 503
    }

    pub fn not_found(&self) -> bool {
        self.status == 404
    }
}

fn parse_error_body(raw: &[u8], status: u16) -> CloudError {
    let mut err = CloudError::new(format!("HTTP {status}"), status);
    match serde_json::from_slice::<Value>(raw) {
        Ok(body) => {
            if let Value::Object(obj) = &body {
                match obj.get("error") {
                    Some(Value::Object(e)) => {
                        err.code = e.get("code").and_then(Value::as_str).map(String::from);
                        if let Some(m) = e.get("message").filter(|m| crate::util::truthy(m)) {
                            err.message = crate::util::py_str(m);
                        }
                    }
                    Some(Value::String(s)) => err.message = s.clone(),
                    _ => {
                        if let Some(m) = obj.get("message").filter(|m| crate::util::truthy(m)) {
                            err.message = crate::util::py_str(m);
                        }
                    }
                }
            }
            err.body = Some(body);
        }
        Err(_) if !raw.is_empty() => {
            err.message = String::from_utf8_lossy(raw).chars().take(200).collect();
        }
        Err(_) => {}
    }
    err
}

/// Percent-encode one path segment (Python `quote(s, safe='')`).
fn quote_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Thin REST client. Callers must never log Authorization headers or tokens.
#[derive(Clone)]
pub struct CloudClient {
    api_base_url: String,
    agent: Agent,
}

impl CloudClient {
    pub fn new(api_base_url: &str) -> Self {
        Self::with_timeout(api_base_url, Duration::from_secs(30))
    }

    pub fn with_timeout(api_base_url: &str, timeout: Duration) -> Self {
        let agent: Agent = Agent::config_builder()
            .timeout_global(Some(timeout))
            // We map 4xx/5xx bodies into CloudError ourselves.
            .http_status_as_error(false)
            .user_agent("vesyl-print-agent")
            // OS trust store, like Python urllib (sites may add a TLS-inspection CA).
            .tls_config(
                TlsConfig::builder()
                    .root_certs(RootCerts::PlatformVerifier)
                    .build(),
            )
            .build()
            .into();
        CloudClient {
            api_base_url: format!("{}/", api_base_url.trim_end_matches('/')),
            agent,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.api_base_url, path.trim_start_matches('/'))
    }

    fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
        token: Option<&str>,
    ) -> Result<JsonObject, CloudError> {
        let url = self.url(path);
        log::debug!(target: LOG, "{method} {path}");
        let auth = token
            .filter(|t| !t.is_empty())
            .map(|t| format!("Bearer {t}"));
        // Always send a body for POST so the server never sees a bodiless request
        // (a GET on /heartbeat yields Rails RoutingError "Not Found").
        let result = match method {
            "GET" => {
                let mut req = self.agent.get(&url).header("Accept", "application/json");
                if let Some(a) = &auth {
                    req = req.header("Authorization", a);
                }
                req.call()
            }
            _ => {
                let empty = json!({});
                let payload = serde_json::to_vec(body.unwrap_or(&empty)).expect("json");
                let mut req = self
                    .agent
                    .post(&url)
                    .header("Accept", "application/json")
                    .header("Content-Type", "application/json");
                if let Some(a) = &auth {
                    req = req.header("Authorization", a);
                }
                req.send(&payload[..])
            }
        };

        let mut resp = result.map_err(|e| match e {
            ureq::Error::Timeout(_) => CloudError::new("request timed out", 0),
            other => CloudError::new(format!("network error: {other}"), 0),
        })?;
        let status = resp.status().as_u16();
        let raw = resp
            .body_mut()
            .with_config()
            .limit(MAX_RESPONSE_BYTES)
            .read_to_vec()
            .map_err(|e| CloudError::new(format!("network error: {e}"), 0))?;

        if status >= 400 {
            return Err(parse_error_body(&raw, status));
        }
        if raw.is_empty() {
            return Ok(JsonObject::new());
        }
        match serde_json::from_slice::<Value>(&raw) {
            Ok(Value::Object(obj)) => Ok(obj),
            Ok(other) => Err(CloudError {
                body: Some(other),
                ..CloudError::new(format!("expected JSON object (HTTP {status})"), status)
            }),
            Err(_) => Err(CloudError::new(
                format!("invalid JSON response (HTTP {status})"),
                status,
            )),
        }
    }

    pub fn claim(
        &self,
        code: &str,
        hostname: &str,
        agent_version: &str,
        platform: &str,
        name: Option<&str>,
    ) -> Result<JsonObject, CloudError> {
        let mut payload = json!({
            "code": code,
            "hostname": hostname,
            "agent_version": agent_version,
            "platform": platform,
        });
        if let Some(n) = name.filter(|n| !n.is_empty()) {
            payload["name"] = json!(n);
        }
        self.request("POST", "print/v1/claim", Some(&payload), None)
    }

    pub fn enroll(
        &self,
        enrollment_token: &str,
        hostname: &str,
        agent_version: &str,
        platform: Option<&str>,
        name: Option<&str>,
    ) -> Result<JsonObject, CloudError> {
        let mut payload = json!({
            "enrollment_token": enrollment_token,
            "hostname": hostname,
            "agent_version": agent_version,
        });
        if let Some(p) = platform.filter(|p| !p.is_empty()) {
            payload["platform"] = json!(p);
        }
        if let Some(n) = name.filter(|n| !n.is_empty()) {
            payload["name"] = json!(n);
        }
        self.request("POST", "print/v1/enroll", Some(&payload), None)
    }

    pub fn whoami(&self, device_token: &str) -> Result<JsonObject, CloudError> {
        self.request("GET", "print/v1/whoami", None, Some(device_token))
    }

    /// POST /print/v1/heartbeat.
    ///
    /// Response may include OTA control fields: `desired_agent_version`, `update_url`.
    pub fn heartbeat(
        &self,
        device_token: &str,
        body: &HeartbeatBody,
    ) -> Result<JsonObject, CloudError> {
        let mut b = JsonObject::new();
        if let Some(v) = &body.agent_version {
            b.insert("agent_version".into(), json!(v));
        }
        if let Some(v) = &body.hostname {
            b.insert("hostname".into(), json!(v));
        }
        if let Some(v) = &body.printers {
            b.insert("printers".into(), Value::Array(v.clone()));
        }
        if let Some(v) = &body.platform {
            b.insert("platform".into(), json!(v));
        }
        if let Some(v) = &body.update {
            b.insert("update".into(), Value::Object(v.clone()));
        }
        self.request(
            "POST",
            "print/v1/heartbeat",
            Some(&Value::Object(b)),
            Some(device_token),
        )
    }

    pub fn ws_ticket(&self, device_token: &str) -> Result<JsonObject, CloudError> {
        self.request(
            "POST",
            "print/v1/ws_ticket",
            Some(&json!({})),
            Some(device_token),
        )
    }

    // --- jobs (pull path) ---------------------------------------------------

    /// GET /print/v1/jobs/pending — eligible jobs, marked sent server-side.
    pub fn pending_jobs(&self, device_token: &str) -> Result<Vec<JsonObject>, CloudError> {
        let data = self.request("GET", "print/v1/jobs/pending", None, Some(device_token))?;
        match data.get("jobs") {
            None | Some(Value::Null) => Ok(Vec::new()),
            Some(Value::Array(items)) => {
                Ok(items.iter().filter_map(Value::as_object).cloned().collect())
            }
            Some(_) => Err(CloudError {
                body: Some(Value::Object(data.clone())),
                ..CloudError::new("jobs/pending: expected jobs array", 200)
            }),
        }
    }

    /// POST /print/v1/jobs/:id/ack — durable receive ACK (after queue fsync).
    pub fn ack_job(&self, device_token: &str, job_id: &str) -> Result<JsonObject, CloudError> {
        let path = format!("print/v1/jobs/{}/ack", quote_segment(job_id));
        self.request("POST", &path, Some(&json!({})), Some(device_token))
    }

    /// POST /print/v1/jobs/:id/status — printing|delivered|printed|error.
    pub fn report_job_status(
        &self,
        device_token: &str,
        job_id: &str,
        status: &str,
        message: Option<&str>,
    ) -> Result<JsonObject, CloudError> {
        let path = format!("print/v1/jobs/{}/status", quote_segment(job_id));
        let mut body = json!({ "status": status });
        if let Some(m) = message {
            body["message"] = json!(m);
        }
        self.request("POST", &path, Some(&body), Some(device_token))
    }
}

/// Optional heartbeat fields; `None` fields are omitted from the request.
#[derive(Debug, Clone, Default)]
pub struct HeartbeatBody {
    pub agent_version: Option<String>,
    pub hostname: Option<String>,
    pub printers: Option<Vec<Value>>,
    pub platform: Option<String>,
    pub update: Option<JsonObject>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::serve;

    #[test]
    fn claim_parses_201() {
        let srv = serve(vec![(
            201,
            r#"{"node_id":"node-uuid-1","device_token":"tok"}"#,
        )]);
        let client = CloudClient::new(&srv.base_url);
        let data = client
            .claim("AB7K2Q9M", "h", "0.3.0", "linux-arm64", None)
            .unwrap();
        assert_eq!(data["node_id"], "node-uuid-1");
        assert_eq!(data["device_token"], "tok");
        let reqs = srv.requests.lock().unwrap();
        assert_eq!(reqs[0].method, "POST");
        assert_eq!(reqs[0].path, "/print/v1/claim");
        assert_eq!(reqs[0].header("User-Agent"), Some("vesyl-print-agent"));
        let body: Value = serde_json::from_slice(&reqs[0].body).unwrap();
        assert_eq!(body["code"], "AB7K2Q9M");
        assert!(body.get("name").is_none());
    }

    #[test]
    fn edge_api_prefix_is_kept() {
        let srv = serve(vec![(200, "{}")]);
        let client = CloudClient::new(&format!("{}/api/", srv.base_url));
        client.whoami("t").unwrap();
        assert_eq!(srv.requests.lock().unwrap()[0].path, "/api/print/v1/whoami");
    }

    #[test]
    fn unauthorized_401() {
        let srv = serve(vec![(
            401,
            r#"{"error":{"code":"unauthorized","message":"bad token"}}"#,
        )]);
        let client = CloudClient::new(&srv.base_url);
        let err = client.whoami("bad-token").unwrap_err();
        assert!(err.unauthorized());
        assert_eq!(err.code.as_deref(), Some("unauthorized"));
        assert_eq!(err.message, "bad token");
        let reqs = srv.requests.lock().unwrap();
        assert_eq!(reqs[0].method, "GET");
        assert_eq!(reqs[0].header("Authorization"), Some("Bearer bad-token"));
    }

    #[test]
    fn error_does_not_embed_token() {
        let secret = "super-secret-token-xyz";
        let srv = serve(vec![(
            401,
            r#"{"error":{"code":"unauthorized","message":"invalid"}}"#,
        )]);
        let client = CloudClient::new(&srv.base_url);
        let err = client
            .heartbeat(secret, &HeartbeatBody::default())
            .unwrap_err();
        assert!(!err.to_string().contains(secret));
        assert!(!format!("{err:?}").contains(secret));
    }

    #[test]
    fn network_error_has_status_zero() {
        // Port 9 on localhost: nothing listening.
        let client = CloudClient::with_timeout("http://127.0.0.1:9", Duration::from_secs(2));
        let err = client.whoami("t").unwrap_err();
        assert_eq!(err.status, 0);
        assert!(err.message.starts_with("network error") || err.message == "request timed out");
    }

    #[test]
    fn non_json_error_body() {
        let srv = serve(vec![(502, "<html>bad gateway</html>")]);
        let err = CloudClient::new(&srv.base_url).whoami("t").unwrap_err();
        assert_eq!(err.status, 502);
        assert_eq!(err.message, "<html>bad gateway</html>");
    }

    #[test]
    fn pending_jobs_parses_list_and_empty() {
        let srv = serve(vec![
            (
                200,
                r#"{"jobs":[{"id":"job-uuid-1","cups_name":"Label_1"}, 5]}"#,
            ),
            (200, r#"{"jobs":[]}"#),
        ]);
        let client = CloudClient::new(&srv.base_url);
        let jobs = client.pending_jobs("tok").unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0]["cups_name"], "Label_1");
        assert!(client.pending_jobs("tok").unwrap().is_empty());
    }

    #[test]
    fn ack_and_state_paths() {
        let srv = serve(vec![(200, r#"{"ok":true}"#), (200, "{}"), (200, "{}")]);
        let client = CloudClient::new(&srv.base_url);
        client.ack_job("tok", "job-uuid-1").unwrap();
        client
            .report_job_status("tok", "job-uuid-1", "delivered", None)
            .unwrap();
        client
            .report_job_status("tok", "job/../x", "error", Some("lp failed"))
            .unwrap();
        let reqs = srv.requests.lock().unwrap();
        assert_eq!(reqs[0].method, "POST");
        assert_eq!(reqs[0].path, "/print/v1/jobs/job-uuid-1/ack");
        assert_eq!(reqs[1].path, "/print/v1/jobs/job-uuid-1/status");
        assert_eq!(reqs[2].path, "/print/v1/jobs/job%2F..%2Fx/status");
        let body: Value = serde_json::from_slice(&reqs[2].body).unwrap();
        assert_eq!(body["status"], "error");
        assert_eq!(body["message"], "lp failed");
    }
}
