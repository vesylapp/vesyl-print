//! Minimal ActionCable client for PrintNodeChannel (/print/cable).
//!
//! Protocol (Rails ActionCable):
//!   server → welcome | ping | confirm_subscription | reject_subscription | message
//!   client → subscribe | unsubscribe | message (perform)
//!
//! Connect with short-lived ticket:  `{cable_url}?token={ws_ticket}`

use std::io::ErrorKind;
use std::net::{TcpStream, ToSocketAddrs};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, ThreadId};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};
use url::Url;

use crate::util::py_str;
use crate::{BoxError, JsonObject};

const LOG: &str = "vesyl-print.cable";

pub const CHANNEL_NAME: &str = "PrintNodeChannel";

/// Keepalive like websocket-client `run_forever(ping_interval=25, ping_timeout=10)`.
const PING_INTERVAL: Duration = Duration::from_secs(25);
const PING_TIMEOUT: Duration = Duration::from_secs(10);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// Read poll granularity: how quickly the socket thread notices `stop()`.
const READ_POLL: Duration = Duration::from_millis(250);

/// Append `?token=ticket` (or replace an existing token) on the cable URL.
pub fn build_cable_connect_url(cable_url: &str, ticket: &str) -> Result<String, url::ParseError> {
    let mut url = Url::parse(cable_url)?;
    let pairs: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(k, _)| k != "token")
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    url.query_pairs_mut()
        .clear()
        .extend_pairs(pairs)
        .append_pair("token", ticket);
    Ok(url.into())
}

pub fn channel_identifier() -> String {
    json!({ "channel": CHANNEL_NAME }).to_string()
}

pub type MessageHandler = Arc<dyn Fn(JsonObject) + Send + Sync>;
pub type EventHandler = Arc<dyn Fn() + Send + Sync>;

/// `threading.Event` equivalent.
#[derive(Default)]
struct Flag {
    set: Mutex<bool>,
    cv: Condvar,
}

impl Flag {
    fn set(&self) {
        *self.set.lock().unwrap() = true;
        self.cv.notify_all();
    }

    fn clear(&self) {
        *self.set.lock().unwrap() = false;
    }

    fn is_set(&self) -> bool {
        *self.set.lock().unwrap()
    }

    fn wait(&self, timeout: Duration) -> bool {
        let guard = self.set.lock().unwrap();
        let (guard, _) = self.cv.wait_timeout_while(guard, timeout, |s| !*s).unwrap();
        *guard
    }
}

/// Run a user callback; a panic is logged instead of killing the socket thread.
fn guarded(what: &str, f: impl FnOnce()) {
    if catch_unwind(AssertUnwindSafe(f)).is_err() {
        log::error!(target: LOG, "{what} handler panicked");
    }
}

#[derive(Default, Clone)]
pub struct ClientCallbacks {
    pub on_message: Option<MessageHandler>,
    pub on_connected: Option<EventHandler>,
    pub on_disconnected: Option<EventHandler>,
    pub on_subscribed: Option<EventHandler>,
}

type Ws = WebSocket<MaybeTlsStream<TcpStream>>;
type SendFn = Box<dyn Fn(&str) -> Result<(), BoxError> + Send + Sync>;

struct Inner {
    url: String,
    identifier: String,
    callbacks: ClientCallbacks,
    stop: Flag,
    connected: Flag,
    subscribed: Flag,
    /// Set by the socket thread on exit.
    finished: Flag,
    sender: Mutex<Option<SendFn>>,
    thread_id: Mutex<Option<ThreadId>>,
}

/// Background WebSocket client for PrintNodeChannel.
///
/// Callbacks run on the socket thread — keep them short or enqueue work.
/// `stop()` never blocks the caller more than its timeout.
pub struct ActionCableClient {
    inner: Arc<Inner>,
}

impl ActionCableClient {
    pub fn new(url: &str, callbacks: ClientCallbacks) -> Self {
        ActionCableClient {
            inner: Arc::new(Inner {
                url: url.to_string(),
                identifier: channel_identifier(),
                callbacks,
                stop: Flag::default(),
                connected: Flag::default(),
                subscribed: Flag::default(),
                finished: Flag::default(),
                sender: Mutex::new(None),
                thread_id: Mutex::new(None),
            }),
        }
    }

    pub fn connected(&self) -> bool {
        self.inner.connected.is_set() && !self.inner.stop.is_set()
    }

    pub fn subscribed(&self) -> bool {
        self.inner.subscribed.is_set() && self.connected()
    }

    /// Spawn the socket thread (no-op if already running).
    pub fn start(&self) -> std::io::Result<()> {
        if self.inner.thread_id.lock().unwrap().is_some() && !self.inner.finished.is_set() {
            return Ok(());
        }
        for f in [
            &self.inner.stop,
            &self.inner.subscribed,
            &self.inner.connected,
            &self.inner.finished,
        ] {
            f.clear();
        }
        let inner = self.inner.clone();
        let handle = thread::Builder::new()
            .name("vesyl-print-cable".into())
            .spawn(move || inner.run())?;
        *self.inner.thread_id.lock().unwrap() = Some(handle.thread().id());
        Ok(())
    }

    /// Signal the socket to close. Never hangs the agent main loop.
    pub fn stop(&self, timeout: Duration) {
        self.inner.stop.set();
        self.inner.subscribed.clear();
        let on_own_thread = *self.inner.thread_id.lock().unwrap() == Some(thread::current().id());
        if !on_own_thread && self.inner.thread_id.lock().unwrap().is_some() {
            self.inner.finished.wait(timeout);
        }
        self.inner.connected.clear();
    }

    pub fn wait_subscribed(&self, timeout: Duration) -> bool {
        self.inner.subscribed.wait(timeout)
    }

    /// Invoke a channel action (heartbeat, ack_job, job_status, …).
    /// Null values in `data` are dropped, like Python's `**kwargs` filter.
    pub fn perform(&self, action: &str, data: JsonObject) -> Result<(), BoxError> {
        self.inner.perform(action, data)
    }

    /// Feed one server frame through the protocol handler (tests / replay).
    pub fn handle_text(&self, text: &str) {
        self.inner.handle_text(text);
    }

    #[cfg(test)]
    fn set_sender(&self, f: SendFn) {
        *self.inner.sender.lock().unwrap() = Some(f);
    }
}

impl Inner {
    fn perform(&self, action: &str, data: JsonObject) -> Result<(), BoxError> {
        let mut payload = JsonObject::new();
        payload.insert("action".into(), json!(action));
        payload.extend(data.into_iter().filter(|(_, v)| !v.is_null()));
        let frame = json!({
            "command": "message",
            "identifier": self.identifier,
            "data": Value::Object(payload).to_string(),
        });
        self.send(&frame)
    }

    fn send(&self, frame: &Value) -> Result<(), BoxError> {
        let raw = frame.to_string();
        let guard = self.sender.lock().unwrap();
        match guard.as_ref() {
            Some(send) if !self.stop.is_set() => send(&raw),
            _ => Err("cable not connected".into()),
        }
    }

    fn subscribe(&self) {
        let frame = json!({ "command": "subscribe", "identifier": self.identifier });
        if let Err(e) = self.send(&frame) {
            log::error!(target: LOG, "cable subscribe send failed: {e}");
        }
    }

    fn handle_text(&self, text: &str) {
        let Ok(value) = serde_json::from_str::<Value>(text) else {
            log::warn!(target: LOG, "cable non-JSON frame");
            return;
        };
        let Value::Object(data) = value else { return };

        match data.get("type").and_then(Value::as_str) {
            Some("welcome") => {
                self.connected.set();
                if let Some(cb) = &self.callbacks.on_connected {
                    guarded("on_connected", || cb());
                }
                self.subscribe();
                return;
            }
            Some("ping") => return,
            Some("disconnect") => {
                log::warn!(
                    target: LOG,
                    "cable disconnect: {}",
                    data.get("reason").map(py_str).unwrap_or_default()
                );
                self.stop.set();
                return;
            }
            Some("confirm_subscription") => {
                if data.get("identifier").and_then(Value::as_str) == Some(self.identifier.as_str())
                {
                    log::info!(target: LOG, "cable subscribed to {CHANNEL_NAME}");
                    self.subscribed.set();
                    if let Some(cb) = &self.callbacks.on_subscribed {
                        guarded("on_subscribed", || cb());
                    }
                }
                return;
            }
            Some("reject_subscription") => {
                log::error!(target: LOG, "cable subscription rejected");
                self.subscribed.clear();
                return;
            }
            _ => {}
        }

        if let Some(Value::Object(msg)) = data.get("message") {
            if let Some(cb) = &self.callbacks.on_message {
                guarded("cable message", || cb(msg.clone()));
            }
        }
    }

    fn run(self: Arc<Self>) {
        if !self.stop.is_set() {
            log::info!(target: LOG, "cable connecting");
            if let Err(e) = self.connect_and_serve() {
                if !self.stop.is_set() {
                    log::warn!(target: LOG, "cable connection error: {e}");
                }
            }
        }
        self.connected.clear();
        self.subscribed.clear();
        *self.sender.lock().unwrap() = None;
        if !self.stop.is_set() {
            if let Some(cb) = &self.callbacks.on_disconnected {
                guarded("on_disconnected", || cb());
            }
        }
        self.finished.set();
    }

    fn connect(&self) -> Result<Ws, BoxError> {
        let url = Url::parse(&self.url)?;
        let host = url.host_str().ok_or("cable url missing host")?;
        let port = url
            .port_or_known_default()
            .ok_or("cable url missing port")?;
        let mut last_err: Option<std::io::Error> = None;
        let mut stream = None;
        for addr in (host, port).to_socket_addrs()? {
            match TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT) {
                Ok(s) => {
                    stream = Some(s);
                    break;
                }
                Err(e) => last_err = Some(e),
            }
        }
        let stream = match stream {
            Some(s) => s,
            None => {
                return Err(last_err
                    .map(Into::into)
                    .unwrap_or_else(|| "no addresses for cable host".into()))
            }
        };
        stream.set_nodelay(true)?;
        stream.set_read_timeout(Some(CONNECT_TIMEOUT))?;
        stream.set_write_timeout(Some(CONNECT_TIMEOUT))?;
        let (ws, _resp) =
            tungstenite::client_tls_with_config(self.url.as_str(), stream, None, None)
                .map_err(|e| format!("handshake failed: {e}"))?;
        // Short read timeout from here on so the loop can notice stop() and pings.
        tcp_of(&ws).set_read_timeout(Some(READ_POLL))?;
        Ok(ws)
    }

    fn connect_and_serve(self: &Arc<Self>) -> Result<(), BoxError> {
        let ws = Arc::new(Mutex::new(self.connect()?));
        log::info!(target: LOG, "cable socket open — waiting for welcome");
        {
            let ws = ws.clone();
            *self.sender.lock().unwrap() = Some(Box::new(move |raw: &str| {
                let mut ws = ws.lock().unwrap();
                ws.send(Message::text(raw)).map_err(Into::into)
            }));
        }

        let mut last_ping = Instant::now();
        let mut awaiting_pong: Option<Instant> = None;
        while !self.stop.is_set() {
            // Hold the lock only for the read; handlers may call perform().
            let read = {
                let mut guard = ws.lock().unwrap();
                let r = guard.read();
                // Flush any queued pong for a server ping.
                let _ = guard.flush();
                r
            };
            match read {
                Ok(Message::Text(text)) => {
                    awaiting_pong = None;
                    self.handle_text(text.as_str());
                }
                Ok(Message::Pong(_)) => awaiting_pong = None,
                Ok(Message::Close(frame)) => {
                    log::info!(target: LOG, "cable closed status={:?}", frame.map(|f| f.code));
                }
                Ok(_) => awaiting_pong = None,
                Err(tungstenite::Error::Io(e))
                    if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
                Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => {
                    log::info!(target: LOG, "cable closed");
                    break;
                }
                Err(e) => return Err(e.into()),
            }

            if let Some(sent) = awaiting_pong {
                if sent.elapsed() >= PING_TIMEOUT {
                    return Err("ping/pong timed out".into());
                }
            } else if last_ping.elapsed() >= PING_INTERVAL {
                ws.lock().unwrap().send(Message::Ping(Default::default()))?;
                last_ping = Instant::now();
                awaiting_pong = Some(last_ping);
            }
        }

        // Best-effort close; never block shutdown on it.
        if let Ok(mut guard) = ws.try_lock() {
            let _ = guard.close(None);
            let _ = guard.flush();
        }
        Ok(())
    }
}

fn tcp_of(ws: &Ws) -> &TcpStream {
    match ws.get_ref() {
        MaybeTlsStream::Plain(s) => s,
        MaybeTlsStream::Rustls(s) => &s.sock,
        _ => unreachable!("only plain and rustls streams are enabled"),
    }
}

/// Handlers for [`PrintCableSession`] message types.
#[derive(Default, Clone)]
pub struct SessionHandlers {
    pub on_print_job: Option<MessageHandler>,
    pub on_revoke: Option<EventHandler>,
    pub on_job_canceled: Option<Arc<dyn Fn(String) + Send + Sync>>,
    /// Warehouse/name push so LCD matches admin without waiting for whoami.
    pub on_node_config: Option<MessageHandler>,
    pub on_subscribed: Option<EventHandler>,
    pub on_disconnected: Option<EventHandler>,
}

pub type TicketFn = Box<dyn Fn() -> Result<JsonObject, BoxError> + Send + Sync>;

/// High-level session: ticket → connect → PrintNodeChannel.
pub struct PrintCableSession {
    cable_url: String,
    get_ticket: TicketFn,
    handlers: Arc<SessionHandlers>,
    /// Held only for brief reads/swaps so handlers can call `perform()` freely.
    client: Mutex<Option<Arc<ActionCableClient>>>,
    /// Serializes start/stop (Python used an RLock around both).
    lifecycle: Mutex<()>,
}

impl PrintCableSession {
    pub fn new(cable_url: &str, get_ticket: TicketFn, handlers: SessionHandlers) -> Self {
        PrintCableSession {
            cable_url: cable_url.trim_end_matches('/').to_string(),
            get_ticket,
            handlers: Arc::new(handlers),
            client: Mutex::new(None),
            lifecycle: Mutex::new(()),
        }
    }

    fn current(&self) -> Option<Arc<ActionCableClient>> {
        self.client.lock().unwrap().clone()
    }

    pub fn subscribed(&self) -> bool {
        self.current().is_some_and(|c| c.subscribed())
    }

    pub fn connected(&self) -> bool {
        self.current().is_some_and(|c| c.connected())
    }

    /// Fetch ticket and start client (non-blocking handshake).
    ///
    /// Returns false if the ticket fails. Does **not** wait for subscription —
    /// poll [`subscribed`](Self::subscribed) or call [`wait_subscribed`](Self::wait_subscribed).
    pub fn start(&self) -> bool {
        let _life = self.lifecycle.lock().unwrap();
        self.stop_unlocked();
        let payload = match (self.get_ticket)() {
            Ok(p) => p,
            Err(e) => {
                log::warn!(target: LOG, "cable: ws_ticket failed: {e}");
                return false;
            }
        };
        let Some(ticket) = payload
            .get("ticket")
            .filter(|t| crate::util::truthy(t))
            .map(py_str)
        else {
            log::warn!(target: LOG, "cable: ws_ticket response missing ticket");
            return false;
        };
        let url = match build_cable_connect_url(&self.cable_url, &ticket) {
            Ok(u) => u,
            Err(e) => {
                log::warn!(target: LOG, "cable: bad cable_url: {e}");
                return false;
            }
        };
        let handlers = self.handlers.clone();
        let client = Arc::new(ActionCableClient::new(
            &url,
            ClientCallbacks {
                on_message: Some(Arc::new(move |m| dispatch_message(&handlers, m))),
                on_connected: None,
                on_subscribed: self.handlers.on_subscribed.clone(),
                on_disconnected: self.handlers.on_disconnected.clone(),
            },
        ));
        if let Err(e) = client.start() {
            log::warn!(target: LOG, "cable: start failed: {e}");
            return false;
        }
        *self.client.lock().unwrap() = Some(client);
        true
    }

    pub fn stop(&self) {
        let _life = self.lifecycle.lock().unwrap();
        self.stop_unlocked();
    }

    fn stop_unlocked(&self) {
        let old = self.client.lock().unwrap().take();
        if let Some(c) = old {
            c.stop(Duration::from_millis(1500));
        }
    }

    /// Perform a channel action if subscribed. Returns false otherwise (caller
    /// falls back to REST).
    pub fn perform(&self, action: &str, data: JsonObject) -> bool {
        let Some(c) = self.current().filter(|c| c.subscribed()) else {
            return false;
        };
        match c.perform(action, data) {
            Ok(()) => true,
            Err(e) => {
                log::warn!(target: LOG, "cable perform {action} failed: {e}");
                false
            }
        }
    }

    pub fn wait_subscribed(&self, timeout: Duration) -> bool {
        self.current().is_some_and(|c| c.wait_subscribed(timeout))
    }

    /// Route one channel message to the session handlers.
    pub fn dispatch_message(&self, msg: JsonObject) {
        dispatch_message(&self.handlers, msg);
    }
}

fn dispatch_message(h: &SessionHandlers, msg: JsonObject) {
    match msg.get("type").and_then(Value::as_str) {
        Some("print_job") => match msg.get("job") {
            Some(Value::Object(job)) => {
                if let Some(cb) = &h.on_print_job {
                    cb(job.clone());
                }
            }
            _ => log::warn!(target: LOG, "print_job message missing job object"),
        },
        Some("job_canceled") => {
            if let (Some(jid), Some(cb)) = (
                msg.get("job_id").filter(|v| crate::util::truthy(v)),
                &h.on_job_canceled,
            ) {
                cb(py_str(jid));
            }
        }
        Some("revoke") => {
            log::warn!(target: LOG, "cable: revoke received");
            if let Some(cb) = &h.on_revoke {
                cb();
            }
        }
        Some("node_config") => {
            if let Some(cb) = &h.on_node_config {
                guarded("node_config", || cb(msg.clone()));
            }
        }
        Some("error") => log::warn!(
            target: LOG,
            "cable error from server: {} {}",
            msg.get("code").map(py_str).unwrap_or_default(),
            msg.get("message").map(py_str).unwrap_or_default()
        ),
        other => log::debug!(target: LOG, "cable unknown message type={other:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    const FIXTURES: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../tests/fixtures/actioncable"
    );

    fn fixture(name: &str) -> String {
        std::fs::read_to_string(format!("{FIXTURES}/{name}")).unwrap()
    }

    fn fixture_message(name: &str) -> JsonObject {
        let v: Value = serde_json::from_str(&fixture(name)).unwrap();
        v["message"].as_object().unwrap().clone()
    }

    #[test]
    fn identifier() {
        assert_eq!(channel_identifier(), r#"{"channel":"PrintNodeChannel"}"#);
    }

    #[test]
    fn connect_url_appends_token() {
        let url =
            build_cable_connect_url("wss://wms-api.vesyl.dev/print/cable", "tic.ket-1").unwrap();
        assert_eq!(url, "wss://wms-api.vesyl.dev/print/cable?token=tic.ket-1");
    }

    #[test]
    fn connect_url_replaces_token() {
        let url =
            build_cable_connect_url("wss://example/print/cable?a=1&token=old", "new").unwrap();
        assert!(url.contains("token=new"));
        assert!(url.contains("a=1"));
        assert!(!url.contains("token=old"));
    }

    type Log = Arc<Mutex<Vec<String>>>;

    fn capture(client: &ActionCableClient) -> Log {
        let sent: Log = Arc::default();
        let s = sent.clone();
        client.set_sender(Box::new(move |raw| {
            s.lock().unwrap().push(raw.to_string());
            Ok(())
        }));
        sent
    }

    #[test]
    fn welcome_subscribe_confirm_message() {
        let events: Log = Arc::default();
        let messages: Arc<Mutex<Vec<JsonObject>>> = Arc::default();
        let (e1, e2, m) = (events.clone(), events.clone(), messages.clone());
        let client = ActionCableClient::new(
            "wss://example/print/cable?token=t",
            ClientCallbacks {
                on_message: Some(Arc::new(move |msg| m.lock().unwrap().push(msg))),
                on_connected: Some(Arc::new(move || {
                    e1.lock().unwrap().push("connected".into())
                })),
                on_subscribed: Some(Arc::new(move || {
                    e2.lock().unwrap().push("subscribed".into())
                })),
                on_disconnected: None,
            },
        );
        let sent = capture(&client);

        client.handle_text(&fixture("welcome.json"));
        assert!(events.lock().unwrap().contains(&"connected".into()));
        let first: Value = serde_json::from_str(&sent.lock().unwrap()[0]).unwrap();
        assert_eq!(first["command"], "subscribe");
        assert_eq!(first["identifier"], r#"{"channel":"PrintNodeChannel"}"#);

        client.handle_text(&fixture("confirm_subscription.json"));
        assert!(client.subscribed());
        assert!(events.lock().unwrap().contains(&"subscribed".into()));

        client.handle_text(&fixture("print_job.json"));
        let msgs = messages.lock().unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["type"], "print_job");
        assert_eq!(msgs[0]["job"]["id"], "job-uuid-1");
    }

    #[test]
    fn perform_frame_shape() {
        let client = ActionCableClient::new(
            "wss://example/print/cable?token=t",
            ClientCallbacks::default(),
        );
        let sent = capture(&client);
        let mut data = JsonObject::new();
        data.insert("job_id".into(), json!("j1"));
        data.insert("message".into(), Value::Null);
        client.perform("ack_job", data).unwrap();
        let frame: Value = serde_json::from_str(&sent.lock().unwrap()[0]).unwrap();
        assert_eq!(frame["command"], "message");
        let inner: Value = serde_json::from_str(frame["data"].as_str().unwrap()).unwrap();
        assert_eq!(inner["action"], "ack_job");
        assert_eq!(inner["job_id"], "j1");
        assert!(inner.get("message").is_none());
    }

    #[test]
    fn perform_without_socket_errors() {
        let client =
            ActionCableClient::new("wss://example/print/cable", ClientCallbacks::default());
        assert!(client.perform("ack_job", JsonObject::new()).is_err());
    }

    #[test]
    fn ping_and_garbage_ignored() {
        let client = ActionCableClient::new(
            "wss://example/print/cable?token=t",
            ClientCallbacks::default(),
        );
        client.handle_text(r#"{"type":"ping","message":123}"#);
        client.handle_text("not json");
        client.handle_text("[1,2]");
    }

    #[test]
    fn reject_and_disconnect() {
        let client = ActionCableClient::new(
            "wss://example/print/cable?token=t",
            ClientCallbacks::default(),
        );
        let _sent = capture(&client);
        client.handle_text(&fixture("welcome.json"));
        client.handle_text(&fixture("confirm_subscription.json"));
        assert!(client.subscribed());
        client.handle_text(r#"{"type":"reject_subscription"}"#);
        assert!(!client.subscribed());
        client.handle_text(r#"{"type":"disconnect","reason":"unauthorized"}"#);
        assert!(!client.connected());
    }

    fn session(handlers: SessionHandlers) -> PrintCableSession {
        PrintCableSession::new(
            "wss://example/print/cable",
            Box::new(|| Ok(json!({"ticket": "t"}).as_object().unwrap().clone())),
            handlers,
        )
    }

    #[test]
    fn dispatch_print_job_and_revoke() {
        let jobs: Arc<Mutex<Vec<JsonObject>>> = Arc::default();
        let revoked: Log = Arc::default();
        let (j, r) = (jobs.clone(), revoked.clone());
        let sess = session(SessionHandlers {
            on_print_job: Some(Arc::new(move |job| j.lock().unwrap().push(job))),
            on_revoke: Some(Arc::new(move || r.lock().unwrap().push("revoked".into()))),
            ..Default::default()
        });
        sess.dispatch_message(fixture_message("print_job.json"));
        assert_eq!(jobs.lock().unwrap()[0]["cups_name"], "Label_1");
        sess.dispatch_message(fixture_message("revoke.json"));
        assert_eq!(revoked.lock().unwrap().len(), 1);
    }

    #[test]
    fn node_config_and_job_canceled() {
        let seen: Log = Arc::default();
        let (a, b) = (seen.clone(), seen.clone());
        let sess = session(SessionHandlers {
            on_node_config: Some(Arc::new(move |m| {
                a.lock().unwrap().push(py_str(&m["name"]))
            })),
            on_job_canceled: Some(Arc::new(move |id| b.lock().unwrap().push(id))),
            ..Default::default()
        });
        sess.dispatch_message(
            json!({"type": "node_config", "node_id": "n1", "name": "Pack 1"})
                .as_object()
                .unwrap()
                .clone(),
        );
        sess.dispatch_message(
            json!({"type": "job_canceled", "job_id": "j7"})
                .as_object()
                .unwrap()
                .clone(),
        );
        sess.dispatch_message(json!({"type": "job_canceled"}).as_object().unwrap().clone());
        assert_eq!(
            *seen.lock().unwrap(),
            vec!["Pack 1".to_string(), "j7".to_string()]
        );
    }

    #[test]
    fn start_fails_cleanly_without_ticket() {
        let sess = PrintCableSession::new(
            "wss://example/print/cable",
            Box::new(|| Err("503".into())),
            SessionHandlers::default(),
        );
        assert!(!sess.start());
        assert!(!sess.subscribed());
        assert!(!sess.perform("heartbeat", JsonObject::new()));
        sess.stop();
    }

    /// End-to-end against a local tungstenite server speaking ActionCable.
    #[test]
    #[allow(clippy::result_large_err)] // accept_hdr's callback signature is fixed by tungstenite
    fn live_socket_roundtrip() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut path = String::new();
            let mut ws = tungstenite::accept_hdr(
                stream,
                |req: &tungstenite::handshake::server::Request, resp| {
                    path = req.uri().to_string();
                    Ok(resp)
                },
            )
            .unwrap();
            tx.send(path).unwrap();
            ws.send(Message::text(fixture("welcome.json"))).unwrap();
            // Expect subscribe.
            let sub = ws.read().unwrap().into_text().unwrap();
            tx.send(sub.to_string()).unwrap();
            ws.send(Message::text(fixture("confirm_subscription.json")))
                .unwrap();
            ws.send(Message::text(fixture("print_job.json"))).unwrap();
            // Expect ack_job perform from the job handler.
            let ack = ws.read().unwrap().into_text().unwrap();
            tx.send(ack.to_string()).unwrap();
            // Wait for the client to close.
            while ws.read().is_ok() {}
        });

        let sess_slot: Arc<Mutex<Option<Arc<PrintCableSession>>>> = Arc::default();
        let slot = sess_slot.clone();
        let sess = Arc::new(PrintCableSession::new(
            &format!("ws://{addr}/print/cable/"),
            Box::new(|| Ok(json!({"ticket": "abc"}).as_object().unwrap().clone())),
            SessionHandlers {
                on_print_job: Some(Arc::new(move |job| {
                    let s = slot.lock().unwrap().clone().unwrap();
                    let mut data = JsonObject::new();
                    data.insert("job_id".into(), job["id"].clone());
                    assert!(s.perform("ack_job", data));
                })),
                ..Default::default()
            },
        ));
        *sess_slot.lock().unwrap() = Some(sess.clone());

        assert!(sess.start());
        let wait = Duration::from_secs(5);
        assert_eq!(rx.recv_timeout(wait).unwrap(), "/print/cable?token=abc");
        let sub: Value = serde_json::from_str(&rx.recv_timeout(wait).unwrap()).unwrap();
        assert_eq!(sub["command"], "subscribe");
        assert!(sess.wait_subscribed(wait));
        let ack: Value = serde_json::from_str(&rx.recv_timeout(wait).unwrap()).unwrap();
        let inner: Value = serde_json::from_str(ack["data"].as_str().unwrap()).unwrap();
        assert_eq!(inner["action"], "ack_job");
        assert_eq!(inner["job_id"], "job-uuid-1");

        sess.stop();
        assert!(!sess.connected());
        server.join().unwrap();
    }
}
