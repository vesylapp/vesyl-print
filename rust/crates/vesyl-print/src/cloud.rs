//! HTTP client for VESYL print/v1 REST API.
//!
//! Transport comes from [`crate::net`] (urllib-style timeouts and proxy
//! rules). Redirects are handled here rather than by ureq: ureq drops
//! `Authorization` on every hop, and the unauthenticated follow-up's 401 would
//! make the agent delete its credentials. A URL in an error message is
//! [`net::redact_url`]'s: no userinfo, query or fragment.

use std::collections::HashMap;
use std::time::Duration;

use serde_json::{json, Value};
use ureq::http::{header, Response};
use ureq::{Agent, Body};
use url::Url;

use crate::net::{self, Redirects, Timeouts};
use crate::JsonObject;

const LOG: &str = "vesyl-print.cloud";
/// Pending-jobs payloads can carry base64 PDFs; ureq's default cap is 10 MB.
const MAX_RESPONSE_BYTES: u64 = 64 * 1024 * 1024;
/// Distinct same-host GET redirect targets followed per request, as urllib
/// follows them (`max_redirections`).
const MAX_REDIRECTS: usize = net::MAX_REDIRECTS as usize;
/// Times one redirect target is followed per request (urllib's
/// `max_repeats`): a redirect loop stops after five requests.
const MAX_REPEATS: usize = 4;

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

fn transport_error(e: ureq::Error) -> CloudError {
    match e {
        ureq::Error::Timeout(_) => CloudError::new("request timed out", 0),
        other => CloudError::new(format!("network error: {other}"), 0),
    }
}

/// Redirect codes urllib's `HTTPRedirectHandler` follows.
fn is_redirect(status: u16) -> bool {
    matches!(status, 301 | 302 | 303 | 307 | 308)
}

fn location(resp: &Response<Body>) -> Option<&str> {
    resp.headers()
        .get(header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|l| !l.is_empty())
}

/// Whether the device token may follow a redirect from `from` to `to`: same
/// host, and either the same scheme and port or an http→https upgrade on the
/// default ports. Another host or port, or a downgrade, never gets the token.
/// Nor does a redirect that adds or changes userinfo (`user:pass@`): its
/// target wants other credentials, and ureq would send the token instead.
fn same_host_redirect(from: &Url, to: &Url) -> bool {
    if from.host_str().is_none() || from.host_str() != to.host_str() {
        return false;
    }
    if (from.username(), from.password()) != (to.username(), to.password()) {
        return false;
    }
    match (from.scheme(), to.scheme()) {
        (a, b) if a == b => from.port_or_known_default() == to.port_or_known_default(),
        ("http", "https") => from.port().is_none() && to.port().is_none(),
        _ => false,
    }
}

/// Where a GET to `current` should be re-sent, if `resp` is a redirect the
/// token may follow.
fn follow_target(current: &Url, resp: &Response<Body>) -> Option<Url> {
    if !is_redirect(resp.status().as_u16()) {
        return None;
    }
    let mut to = current.join(location(resp)?).ok()?;
    to.set_fragment(None);
    same_host_redirect(current, &to).then_some(to)
}

/// A 3xx we did not follow, for the request to `at`. Its status stays 3xx so
/// it can never read as a rejected token (401), whatever the redirect target
/// would have answered. The target is named as [`net::redact_url`] shows it.
fn redirect_error(at: &Url, resp: &Response<Body>) -> CloudError {
    let status = resp.status().as_u16();
    match location(resp).map(|loc| at.join(loc)) {
        Some(Ok(to)) => CloudError::new(
            format!("unexpected redirect to {}", net::redact_url(to.as_str())),
            status,
        ),
        Some(Err(_)) => CloudError::new(format!("unexpected HTTP {status} redirect"), status),
        None => CloudError::new(format!("unexpected HTTP {status}"), status),
    }
}

/// Thin REST client. Callers must never log Authorization headers or tokens.
#[derive(Clone)]
pub struct CloudClient {
    api_base_url: String,
    agent: Agent,
}

impl CloudClient {
    pub fn new(api_base_url: &str) -> Self {
        Self::with_timeouts(api_base_url, Timeouts::API)
    }

    /// Like urllib's `timeout`, `timeout` bounds connecting, the response
    /// head and every single read, so a connection that goes silent fails
    /// after `timeout` while a slow but steady body completes (within the
    /// API body budget).
    pub fn with_timeout(api_base_url: &str, timeout: Duration) -> Self {
        Self::with_timeouts(
            api_base_url,
            Timeouts {
                connect: timeout,
                response: timeout,
                idle: timeout,
                body: Timeouts::API.body.max(timeout),
            },
        )
    }

    pub fn with_timeouts(api_base_url: &str, timeouts: Timeouts) -> Self {
        let api_base_url = format!("{}/", api_base_url.trim_end_matches('/'));
        let agent = net::agent(&api_base_url, timeouts, Redirects::Manual);
        CloudClient {
            api_base_url,
            agent,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.api_base_url, path.trim_start_matches('/'))
    }

    /// GET `url`, re-sending the headers (token included) across same-host
    /// redirects as urllib does, within urllib's limits: 10 distinct
    /// targets, each at most 4 times. Any other 3xx comes back unfollowed,
    /// with the URL that answered it.
    fn get(&self, url: Url, auth: Option<&str>) -> Result<(Url, Response<Body>), CloudError> {
        let mut current = url;
        // Times each redirect target was followed (urllib's redirect_dict).
        let mut visited: HashMap<String, usize> = HashMap::new();
        loop {
            // The agent picks the proxy per connection, so an http→https hop
            // gets https_proxy without a new agent.
            let mut req = self
                .agent
                .get(current.as_str())
                .header("Accept", "application/json");
            if let Some(a) = auth {
                req = req.header("Authorization", a);
            }
            let resp = req.call().map_err(transport_error)?;
            let Some(next) = follow_target(&current, &resp) else {
                return Ok((current, resp));
            };
            let status = resp.status().as_u16();
            let seen = visited.get(next.as_str()).copied().unwrap_or(0);
            if seen >= MAX_REPEATS || visited.len() >= MAX_REDIRECTS {
                return Err(CloudError::new(
                    format!(
                        "too many redirects (last to {})",
                        net::redact_url(next.as_str())
                    ),
                    status,
                ));
            }
            visited.insert(next.to_string(), seen + 1);
            log::debug!(target: LOG, "following HTTP {status} redirect to {}", next.path());
            current = next;
        }
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
        let target = Url::parse(&url).map_err(|e| {
            let shown = net::redact_url(&url);
            CloudError::new(format!("network error: invalid URL {shown:?}: {e}"), 0)
        })?;
        let auth = token
            .filter(|t| !t.is_empty())
            .map(|t| format!("Bearer {t}"));
        // Always send a body for POST so the server never sees a bodiless request
        // (a GET on /heartbeat yields Rails RoutingError "Not Found").
        let (at, mut resp) = match method {
            "GET" => self.get(target, auth.as_deref())?,
            _ => {
                let empty = json!({});
                let payload = serde_json::to_vec(body.unwrap_or(&empty)).expect("json");
                let mut req = self
                    .agent
                    .post(target.as_str())
                    .header("Accept", "application/json")
                    .header("Content-Type", "application/json");
                if let Some(a) = &auth {
                    req = req.header("Authorization", a);
                }
                (target, req.send(&payload[..]).map_err(transport_error)?)
            }
        };

        let status = resp.status().as_u16();
        // POSTs are never re-sent, and GETs only within the same host (above).
        if (300..400).contains(&status) {
            return Err(redirect_error(&at, &resp));
        }
        let raw = resp
            .body_mut()
            .with_config()
            .limit(MAX_RESPONSE_BYTES)
            .read_to_vec()
            .map_err(transport_error)?;

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

/// Loopback HTTP server for tests that need full control of the response
/// (redirects, extra headers, slow or stalled bodies); `testutil::serve` only
/// sends canned JSON. Shared with the update and CLI tests.
#[cfg(test)]
pub(crate) mod http_stub {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::{Arc, Mutex};
    use std::thread;

    #[derive(Debug, Clone)]
    pub struct Request {
        pub method: String,
        /// Request target as sent (`/path`, or `host:port` for CONNECT).
        pub path: String,
        pub headers: Vec<(String, String)>,
        pub body: Vec<u8>,
    }

    impl Request {
        pub fn header(&self, name: &str) -> Option<&str> {
            self.headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.as_str())
        }

        /// Port from the `Host` header (to build same- or cross-host URLs).
        pub fn port(&self) -> String {
            self.header("Host")
                .and_then(|h| h.rsplit(':').next())
                .unwrap_or_default()
                .to_string()
        }
    }

    pub struct Stub {
        pub base_url: String,
        requests: Arc<Mutex<Vec<Request>>>,
    }

    impl Stub {
        pub fn requests(&self) -> Vec<Request> {
            self.requests.lock().unwrap().clone()
        }
    }

    /// Serve every connection on its own thread; `handler` writes the raw
    /// response. Requests are recorded before the handler runs.
    pub fn serve(handler: impl Fn(&Request, &mut TcpStream) + Send + Sync + 'static) -> Stub {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let (rec, handler) = (requests.clone(), Arc::new(handler));
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let (rec, handler) = (rec.clone(), handler.clone());
                thread::spawn(move || {
                    if let Some(req) = read_request(&stream) {
                        rec.lock().unwrap().push(req.clone());
                        handler(&req, &mut stream);
                    }
                });
            }
        });
        Stub { base_url, requests }
    }

    fn read_request(stream: &TcpStream) -> Option<Request> {
        let mut reader = BufReader::new(stream.try_clone().ok()?);
        let mut line = String::new();
        reader.read_line(&mut line).ok()?;
        let mut parts = line.split_whitespace();
        let method = parts.next()?.to_string();
        let path = parts.next().unwrap_or_default().to_string();
        let mut headers = Vec::new();
        let mut len = 0usize;
        loop {
            let mut h = String::new();
            if reader.read_line(&mut h).ok()? == 0 {
                break;
            }
            let h = h.trim_end();
            if h.is_empty() {
                break;
            }
            if let Some((k, v)) = h.split_once(':') {
                let (k, v) = (k.trim().to_string(), v.trim().to_string());
                if k.eq_ignore_ascii_case("content-length") {
                    len = v.parse().unwrap_or(0);
                }
                headers.push((k, v));
            }
        }
        let mut body = vec![0u8; len];
        reader.read_exact(&mut body).ok()?;
        Some(Request {
            method,
            path,
            headers,
            body,
        })
    }

    /// Write a complete response (`Content-Length`, `Connection: close`).
    pub fn respond(stream: &mut TcpStream, status: u16, headers: &[(&str, &str)], body: &[u8]) {
        let mut head = format!(
            "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n",
            body.len()
        );
        for (k, v) in headers {
            head.push_str(&format!("{k}: {v}\r\n"));
        }
        head.push_str("\r\n");
        let _ = stream.write_all(head.as_bytes());
        let _ = stream.write_all(body);
        let _ = stream.flush();
    }

    /// Send the head and then `body` in `chunks` pieces, `gap` apart.
    pub fn trickle(stream: &mut TcpStream, body: &[u8], chunks: usize, gap: std::time::Duration) {
        let _ = write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let _ = stream.flush();
        for chunk in body.chunks(body.len().div_ceil(chunks.max(1)).max(1)) {
            thread::sleep(gap);
            let _ = stream.write_all(chunk);
            let _ = stream.flush();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::http_stub::{self, respond};
    use super::*;
    use crate::testutil::serve;
    use std::time::Instant;

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

    /// 401 unless the request carries the token — what wms-api does.
    fn whoami_v2(req: &http_stub::Request, s: &mut std::net::TcpStream) {
        if req.header("Authorization") == Some("Bearer tok") {
            respond(s, 200, &[], br#"{"node_id":"n1"}"#);
        } else {
            respond(s, 401, &[], br#"{"error":{"code":"unauthorized"}}"#);
        }
    }

    #[test]
    fn same_host_redirect_resends_token() {
        for status in [301u16, 302, 303, 307, 308] {
            let srv = http_stub::serve(move |req, s| match req.path.as_str() {
                "/print/v1/whoami" => {
                    respond(s, status, &[("Location", "/v2/print/v1/whoami")], b"")
                }
                _ => whoami_v2(req, s),
            });
            let who = CloudClient::new(&srv.base_url).whoami("tok").unwrap();
            assert_eq!(who["node_id"], "n1", "HTTP {status}");
            let reqs = srv.requests();
            assert_eq!(reqs.len(), 2);
            assert_eq!(reqs[1].path, "/v2/print/v1/whoami");
            assert_eq!(reqs[1].header("Authorization"), Some("Bearer tok"));
            assert_eq!(reqs[1].header("Accept"), Some("application/json"));
        }
    }

    #[test]
    fn absolute_same_host_redirect_is_followed() {
        let srv = http_stub::serve(|req, s| match req.path.as_str() {
            "/print/v1/jobs/pending" => {
                let to = format!("http://127.0.0.1:{}/v2/jobs?x=1#frag", req.port());
                respond(s, 302, &[("Location", &to)], b"")
            }
            _ if req.header("Authorization") == Some("Bearer tok") => {
                respond(s, 200, &[], br#"{"jobs":[{"id":"j1"}]}"#)
            }
            _ => respond(s, 401, &[], b""),
        });
        let jobs = CloudClient::new(&srv.base_url).pending_jobs("tok").unwrap();
        assert_eq!(jobs[0]["id"], "j1");
        assert_eq!(srv.requests()[1].path, "/v2/jobs?x=1");
    }

    #[test]
    fn cross_host_redirect_is_an_error_not_a_401() {
        // "localhost" and port 1 are other origins: the token must not follow.
        // (localhost reaches this same stub, so a follow would be recorded.)
        for target in [
            "http://localhost:{port}/v2/print/v1/whoami",
            "http://127.0.0.1:1/x",
        ] {
            let srv = http_stub::serve(move |req, s| match req.path.as_str() {
                "/print/v1/whoami" => {
                    let to = target.replace("{port}", &req.port());
                    respond(s, 302, &[("Location", &to)], b"")
                }
                _ => whoami_v2(req, s),
            });
            let err = CloudClient::new(&srv.base_url).whoami("tok").unwrap_err();
            assert_eq!(err.status, 302, "{target}");
            assert!(!err.unauthorized());
            assert!(
                err.message.starts_with("unexpected redirect to http://"),
                "{err}"
            );
            assert_eq!(srv.requests().len(), 1, "{target} was followed");
        }
    }

    #[test]
    fn post_redirect_is_not_followed() {
        let srv = http_stub::serve(|req, s| match req.path.as_str() {
            "/print/v1/heartbeat" => {
                respond(s, 307, &[("Location", "/v2/print/v1/heartbeat")], b"")
            }
            _ => whoami_v2(req, s),
        });
        let err = CloudClient::new(&srv.base_url)
            .heartbeat("tok", &HeartbeatBody::default())
            .unwrap_err();
        assert_eq!(err.status, 307);
        assert!(!err.unauthorized());
        assert_eq!(
            err.message,
            format!(
                "unexpected redirect to {}/v2/print/v1/heartbeat",
                srv.base_url
            )
        );
        let reqs = srv.requests();
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].body, b"{}");
    }

    #[test]
    fn redirect_loop_and_missing_location_are_errors() {
        let srv = http_stub::serve(|req, s| match req.path.as_str() {
            "/print/v1/whoami" => respond(s, 302, &[("Location", "/loop")], b""),
            "/loop" => respond(s, 302, &[("Location", "/loop")], b""),
            _ => respond(s, 302, &[], b""),
        });
        let client = CloudClient::new(&srv.base_url);
        let err = client.whoami("tok").unwrap_err();
        assert_eq!(err.status, 302);
        assert!(err.message.starts_with("too many redirects"), "{err}");
        // urllib's loop detection: one target is followed 4 times.
        assert_eq!(srv.requests().len(), 1 + MAX_REPEATS);

        let err = client.ack_job("tok", "j1").unwrap_err();
        assert_eq!(
            (err.status, err.message.as_str()),
            (302, "unexpected HTTP 302")
        );
    }

    /// A stub that redirects /print/v1/whoami → /r1 → … → /r`hops` (each
    /// with a signed-looking query) and answers the last one 200, only with
    /// the token.
    fn redirect_chain(hops: usize) -> http_stub::Stub {
        http_stub::serve(move |req, s| {
            let path = req.path.split('?').next().unwrap_or_default();
            let at: usize = path.strip_prefix("/r").map_or(0, |n| n.parse().unwrap());
            if at < hops {
                let to = format!("/r{}?sig=secret-{}", at + 1, at + 1);
                respond(s, 302, &[("Location", &to)], b"")
            } else {
                whoami_v2(req, s)
            }
        })
    }

    /// urllib follows 10 redirects and refuses the 11th; so does the
    /// same-host loop, with the token on every hop.
    #[test]
    fn same_host_redirect_chain_follows_ten_hops() {
        let srv = redirect_chain(10);
        let who = CloudClient::new(&srv.base_url).whoami("tok").unwrap();
        assert_eq!(who["node_id"], "n1");
        let reqs = srv.requests();
        assert_eq!(reqs.len(), 11);
        assert!(reqs
            .iter()
            .all(|r| r.header("Authorization") == Some("Bearer tok")));

        let srv = redirect_chain(11);
        let err = CloudClient::new(&srv.base_url).whoami("tok").unwrap_err();
        assert_eq!(err.status, 302);
        assert!(!err.unauthorized());
        assert_eq!(
            err.message,
            format!("too many redirects (last to {}/r11)", srv.base_url)
        );
        assert_eq!(srv.requests().len(), 11);
    }

    /// No redirect error names a URL's userinfo, query or fragment: those
    /// can carry credentials (a presigned URL's signature, a portal's
    /// session), and the message goes to the journal, status.json and the
    /// LCD's stats page.
    #[test]
    fn redirect_errors_omit_query_and_userinfo() {
        let leaks = |msg: &str| {
            ["sekrit", "pw", "abc", "secret", "?", "#"]
                .iter()
                .any(|s| msg.contains(s))
        };
        // Cross-host GET.
        let srv = http_stub::serve(|_, s| {
            let to = "https://user:pw@bucket.example.test/obj?X-Amz-Signature=sekrit#frag";
            respond(s, 302, &[("Location", to)], b"")
        });
        let err = CloudClient::new(&srv.base_url).whoami("tok").unwrap_err();
        assert_eq!(err.status, 302);
        assert_eq!(
            err.message,
            "unexpected redirect to https://bucket.example.test/obj"
        );
        // POST, to a relative Location.
        let srv =
            http_stub::serve(|_, s| respond(s, 307, &[("Location", "/v2/hb?token=abc")], b""));
        let err = CloudClient::new(&srv.base_url)
            .heartbeat("tok", &HeartbeatBody::default())
            .unwrap_err();
        assert!(
            err.message.ends_with("/v2/hb") && !leaks(&err.message),
            "{err}"
        );
        // A same-host loop.
        let srv =
            http_stub::serve(|_, s| respond(s, 302, &[("Location", "/loop?sig=secret")], b""));
        let err = CloudClient::new(&srv.base_url).whoami("tok").unwrap_err();
        assert!(err.message.starts_with("too many redirects"), "{err}");
        assert!(!leaks(&err.message), "{err}");
        // Same host, but with userinfo: not followed, and not shown.
        let srv = http_stub::serve(|req, s| {
            let to = format!("http://u:pw@127.0.0.1:{}/v2?k=abc", req.port());
            respond(s, 302, &[("Location", &to)], b"")
        });
        let err = CloudClient::new(&srv.base_url).whoami("tok").unwrap_err();
        assert_eq!(err.status, 302);
        assert!(
            err.message.ends_with("/v2") && !leaks(&err.message),
            "{err}"
        );
        assert_eq!(srv.requests().len(), 1, "followed");
    }

    #[test]
    fn redirect_rules() {
        let ok =
            |a: &str, b: &str| same_host_redirect(&Url::parse(a).unwrap(), &Url::parse(b).unwrap());
        assert!(ok("https://api.example/print/", "https://API.example/v2/"));
        assert!(ok("http://api.example/", "https://api.example/"));
        assert!(ok("http://api.example:8080/", "http://api.example:8080/v2"));
        assert!(!ok("https://api.example/", "http://api.example/"));
        assert!(!ok("https://api.example/", "https://other.example/"));
        assert!(!ok("https://api.example/", "https://api.example:8443/"));
        assert!(!ok("http://api.example:8080/", "https://api.example/"));
        assert!(!ok("http://api.example/", "ftp://api.example/"));
        // Userinfo: never added or changed by a redirect.
        assert!(!ok("https://api.example/", "https://u:pw@api.example/v2"));
        assert!(!ok("https://api.example/", "https://u@api.example/v2"));
        assert!(!ok(
            "https://u:a@api.example/",
            "https://u:b@api.example/v2"
        ));
        assert!(ok("https://u:a@api.example/", "https://u:a@api.example/v2"));
    }

    #[test]
    fn slow_steady_body_outlives_the_per_phase_timeout() {
        // 2.5 s of body against a 1 s timeout: urllib's timeout is per socket
        // operation, so urllib completes this; an end-to-end deadline did not.
        let body = r#"{"jobs":[{"id":"j1"},{"id":"j2"},{"id":"j3"}]}"#;
        let srv = http_stub::serve(move |_, s| {
            http_stub::trickle(s, body.as_bytes(), 10, Duration::from_millis(250))
        });
        let started = Instant::now();
        let jobs = CloudClient::with_timeout(&srv.base_url, Duration::from_secs(1))
            .pending_jobs("tok")
            .unwrap();
        assert_eq!(jobs.len(), 3);
        assert!(started.elapsed() >= Duration::from_secs(2));
    }

    #[test]
    fn stalled_body_times_out() {
        let srv = http_stub::serve(|_, s| {
            use std::io::Write;
            let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n{\"jobs\"");
            let _ = s.flush();
            std::thread::sleep(Duration::from_secs(3));
        });
        let timeouts = Timeouts {
            connect: Duration::from_secs(1),
            response: Duration::from_secs(1),
            idle: Duration::from_secs(1),
            body: Duration::from_millis(500),
        };
        let started = Instant::now();
        let err = CloudClient::with_timeouts(&srv.base_url, timeouts)
            .pending_jobs("tok")
            .unwrap_err();
        assert_eq!((err.status, err.message.as_str()), (0, "request timed out"));
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    /// N15: a connection that dies after the headers fails after the client
    /// timeout (urllib's per-read timeout), not the 10-minute body budget.
    #[test]
    fn stalled_body_fails_after_the_client_timeout() {
        let srv = http_stub::serve(|_, s| {
            use std::io::Write;
            let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n{\"jobs\"");
            let _ = s.flush();
            std::thread::sleep(Duration::from_secs(20));
        });
        let started = Instant::now();
        let err = CloudClient::with_timeout(&srv.base_url, Duration::from_secs(1))
            .pending_jobs("tok")
            .unwrap_err();
        let took = started.elapsed();
        assert_eq!((err.status, err.message.as_str()), (0, "request timed out"));
        assert!(took >= Duration::from_millis(900), "{took:?}");
        assert!(took < Duration::from_secs(5), "{took:?}");
        // The API defaults keep urllib's 30 s per read.
        assert_eq!(Timeouts::API.idle, Duration::from_secs(30));
    }

    const PROXY_CHILD_BASE: &str = "VESYL_TEST_PROXY_CHILD_BASE";

    /// Proxy variables must follow urllib's rules. They are process-global, so
    /// each case runs `proxy_env_child` in a child copy of this test binary.
    #[test]
    fn proxy_env_rules_match_urllib() {
        let api = http_stub::serve(|_, s| respond(s, 200, &[], br#"{"node_id":"n1"}"#));
        let proxy = http_stub::serve(|_, s| respond(s, 502, &[], b""));
        let run = |base: &str, vars: &[(&str, &str)]| {
            let mut cmd = std::process::Command::new(std::env::current_exe().unwrap());
            cmd.args([
                "--exact",
                "cloud::tests::proxy_env_child",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ]);
            for k in [
                "http_proxy",
                "HTTP_PROXY",
                "https_proxy",
                "HTTPS_PROXY",
                "all_proxy",
                "ALL_PROXY",
                "no_proxy",
                "NO_PROXY",
                "REQUEST_METHOD",
            ] {
                cmd.env_remove(k);
            }
            let out = cmd
                .env(PROXY_CHILD_BASE, base)
                .envs(vars.iter().copied())
                .output()
                .unwrap();
            let stdout = String::from_utf8_lossy(&out.stdout);
            assert!(
                out.status.success() && stdout.contains("1 passed"),
                "child failed: {stdout}{}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        let p = proxy.base_url.as_str();

        // NO_PROXY entries are trimmed (ureq kept the leading space).
        run(
            &api.base_url,
            &[("http_proxy", p), ("NO_PROXY", "example.com, 127.0.0.1")],
        );
        assert_eq!((api.requests().len(), proxy.requests().len()), (1, 0));
        // ALL_PROXY is ignored, as urllib ignores it.
        run(&api.base_url, &[("ALL_PROXY", p)]);
        assert_eq!((api.requests().len(), proxy.requests().len()), (2, 0));
        // HTTP_PROXY does not apply to https.
        run("https://127.0.0.1:9", &[("HTTP_PROXY", p)]);
        assert_eq!(proxy.requests().len(), 0);
        // Without a bypass, http_proxy does apply to http, in absolute form
        // like urllib (not CONNECT).
        run(&api.base_url, &[("http_proxy", p)]);
        let via = proxy.requests();
        assert_eq!(via.len(), 1);
        assert_eq!(via[0].method, "GET");
        assert_eq!(via[0].path, format!("{}/print/v1/whoami", api.base_url));
        assert_eq!(api.requests().len(), 2);
        // https tunnels with a plain CONNECT, also for an https:// proxy URL,
        // with the credentials percent-decoded (N20).
        let authed = p.replace("http://", "https://dev%40corp:p%3As%25s@");
        run(
            "https://wms-api.example.test",
            &[("https_proxy", authed.as_str())],
        );
        let via = proxy.requests();
        assert_eq!(via.len(), 2);
        assert_eq!(
            (via[1].method.as_str(), via[1].path.as_str()),
            ("CONNECT", "wms-api.example.test:443")
        );
        let auth = via[1].header("Proxy-Authorization").unwrap();
        let decoded = base64::Engine::decode(
            &base64::engine::general_purpose::STANDARD,
            auth.trim_start_matches("Basic "),
        )
        .unwrap();
        assert_eq!(decoded, b"dev@corp:p:s%s");
    }

    #[test]
    #[ignore = "child process of proxy_env_rules_match_urllib"]
    fn proxy_env_child() {
        let Ok(base) = std::env::var(PROXY_CHILD_BASE) else {
            return;
        };
        let result = CloudClient::with_timeout(&base, Duration::from_secs(5)).whoami("tok");
        println!("whoami via {base}: {result:?}");
    }
}
