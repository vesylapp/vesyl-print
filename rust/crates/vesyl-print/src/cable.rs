//! Minimal ActionCable client for PrintNodeChannel (/print/cable).
//!
//! Protocol (Rails ActionCable):
//!   server → welcome | ping | confirm_subscription | reject_subscription | message
//!   client → subscribe | unsubscribe | message (perform)
//!
//! Connect with short-lived ticket:  `{cable_url}?token={ws_ticket}`
//!
//! Threading: each client has one socket thread that owns the WebSocket and
//! is its only reader and writer. It sleeps in `poll()` on the socket and a
//! wake-up pipe, so [`ActionCableClient::perform`] from another thread queues
//! the frame, wakes the socket thread and waits (bounded) for the write.
//! Frames sent from the socket thread itself (subscribe on `welcome`,
//! callbacks) are queued and go out as soon as the current frame is handled.

use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::thread::{self, ThreadId};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tungstenite::client::IntoClientRequest;
use tungstenite::handshake::client::Request;
use tungstenite::http::header::ORIGIN;
use tungstenite::http::HeaderValue;
use tungstenite::protocol::WebSocketConfig;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};
use url::{Host, Url};

use crate::net::{self, ProxyTarget};
use crate::util::py_str;
use crate::{BoxError, JsonObject};

const LOG: &str = "vesyl-print.cable";

pub const CHANNEL_NAME: &str = "PrintNodeChannel";

/// Keepalive like websocket-client `run_forever(ping_interval=25, ping_timeout=10)`:
/// a WebSocket ping every PING_INTERVAL, and the session ends when no Pong
/// has come back PING_TIMEOUT later. Only a Pong counts. ActionCable's own
/// `{"type":"ping"}` text frames (every 3 s) and any other frame show that
/// the server still writes, not that it still reads what we send.
const PING_INTERVAL: Duration = Duration::from_secs(25);
const PING_TIMEOUT: Duration = Duration::from_secs(10);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// How long a session may sit in its handshake before the agent replaces it.
pub const HANDSHAKE_GRACE: Duration = Duration::from_secs(30);
/// How long `perform()` from another thread waits for the socket thread to
/// take its frame. After that the frame is dropped unsent and the caller falls
/// back to REST.
const PERFORM_TIMEOUT: Duration = Duration::from_secs(2);
/// Close the session if `confirm_subscription` has not arrived this long after
/// `subscribe` (an exception in the channel's `subscribed` sends nothing).
const SUBSCRIBE_TIMEOUT: Duration = Duration::from_secs(30);
/// Bound on a blocking socket write once connected. A peer that accepts
/// nothing for this long is treated as dead.
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
/// Reads are non-blocking once connected; this only bounds a stray blocking read.
const READ_TIMEOUT: Duration = Duration::from_secs(1);
/// The close frame on teardown is best effort.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(1);
/// Longest `poll()` sleep. Wake-ups normally come from the socket, the
/// wake-up pipe or a timer; this is only a backstop.
const MAX_IDLE: Duration = Duration::from_secs(1);
/// Largest push accepted: the REST pull limit (`cloud.rs` `MAX_RESPONSE_BYTES`).
/// ActionCable sends each message as a single frame, so frames get the same cap.
const MAX_MESSAGE_BYTES: usize = 64 << 20;
/// Cap on a proxy's response to `CONNECT`.
const MAX_PROXY_HEAD: usize = 16 << 10;

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
/// Environment lookup for the proxy variables (injectable for tests).
type EnvFn = Box<dyn Fn(&str) -> Option<String> + Send + Sync>;
#[cfg(test)]
type SendFn = Box<dyn Fn(&str) -> Result<(), BoxError> + Send + Sync>;

/// Knobs fixed for the life of a client (tests shorten the timeouts).
struct Settings {
    env: EnvFn,
    perform_timeout: Duration,
    subscribe_timeout: Duration,
    ping_interval: Duration,
    ping_timeout: Duration,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            env: Box::new(|key| std::env::var(key).ok()),
            perform_timeout: PERFORM_TIMEOUT,
            subscribe_timeout: SUBSCRIBE_TIMEOUT,
            ping_interval: PING_INTERVAL,
            ping_timeout: PING_TIMEOUT,
        }
    }
}

/// Where a frame queued by another thread stands.
#[derive(Default)]
enum ReplyState {
    #[default]
    Queued,
    Writing,
    Done(Result<(), String>),
    /// The caller gave up first; the socket thread must not write it.
    Abandoned,
}

/// One-shot result for a frame queued by a thread other than the socket thread.
#[derive(Default)]
struct Reply {
    state: Mutex<ReplyState>,
    cv: Condvar,
}

impl Reply {
    /// Socket thread: take the frame for writing, unless its caller gave up.
    fn claim(&self) -> bool {
        let mut state = self.state.lock().unwrap();
        let queued = matches!(*state, ReplyState::Queued);
        if queued {
            *state = ReplyState::Writing;
        }
        queued
    }

    fn finish(&self, result: Result<(), String>) {
        let mut state = self.state.lock().unwrap();
        if matches!(*state, ReplyState::Queued | ReplyState::Writing) {
            *state = ReplyState::Done(result);
            self.cv.notify_all();
        }
    }

    /// Caller: wait for the socket thread. A frame still queued after
    /// `timeout` is abandoned (it will never be written), so a REST fallback
    /// cannot race a late cable copy of it. A frame the socket thread is
    /// already writing is waited out; that write is bounded by WRITE_TIMEOUT.
    fn wait(&self, timeout: Duration) -> Result<(), BoxError> {
        let start = Instant::now();
        let cap = timeout + WRITE_TIMEOUT + CLOSE_TIMEOUT;
        let mut state = self.state.lock().unwrap();
        loop {
            let elapsed = start.elapsed();
            let limit = match &*state {
                ReplyState::Done(result) => return result.clone().map_err(Into::into),
                ReplyState::Abandoned => return Err("cable send abandoned".into()),
                ReplyState::Queued if elapsed >= timeout => {
                    *state = ReplyState::Abandoned;
                    return Err(format!("cable busy: frame not sent within {timeout:?}").into());
                }
                ReplyState::Queued => timeout,
                ReplyState::Writing if elapsed >= cap => {
                    return Err("cable write did not finish".into())
                }
                ReplyState::Writing => cap,
            };
            state = self.cv.wait_timeout(state, limit - elapsed).unwrap().0;
        }
    }
}

/// A frame for the socket thread to write.
struct Outgoing {
    raw: String,
    /// `None` for frames queued by the socket thread itself (never awaited).
    reply: Option<Arc<Reply>>,
}

impl Drop for Outgoing {
    /// Dropped unsent (session over): fail the caller now, not at its timeout.
    fn drop(&mut self) {
        if let Some(reply) = &self.reply {
            reply.finish(Err("cable not connected".into()));
        }
    }
}

/// Hand-off from other threads to the socket thread.
struct Outbox {
    frames: mpsc::Sender<Outgoing>,
    /// Write end of the socket thread's wake-up pipe (non-blocking).
    wake: UnixStream,
}

impl Outbox {
    fn wake(&self) {
        // A full pipe already holds a pending wake-up.
        let _ = (&self.wake).write(&[1]);
    }
}

struct Inner {
    url: String,
    identifier: String,
    callbacks: ClientCallbacks,
    settings: Settings,
    stop: Flag,
    connected: Flag,
    subscribed: Flag,
    /// Set by the socket thread on exit.
    finished: Flag,
    /// Open while the socket thread is serving a connection.
    outbox: Mutex<Option<Outbox>>,
    /// When `subscribe` was queued (confirmation deadline).
    subscribe_sent: Mutex<Option<Instant>>,
    thread_id: Mutex<Option<ThreadId>>,
    /// When the current socket thread was spawned (handshake grace period).
    started_at: Mutex<Option<Instant>>,
    #[cfg(test)]
    capture: Mutex<Option<SendFn>>,
}

/// Background WebSocket client for PrintNodeChannel.
///
/// Callbacks run on the socket thread — keep them short or enqueue work.
/// While a callback runs, `perform()` from other threads waits (at most
/// ~2 s, then fails so the caller can use REST). `stop()` never blocks the
/// caller more than its timeout.
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
                settings: Settings::default(),
                stop: Flag::default(),
                connected: Flag::default(),
                subscribed: Flag::default(),
                finished: Flag::default(),
                outbox: Mutex::new(None),
                subscribe_sent: Mutex::new(None),
                thread_id: Mutex::new(None),
                started_at: Mutex::new(None),
                #[cfg(test)]
                capture: Mutex::new(None),
            }),
        }
    }

    pub fn connected(&self) -> bool {
        self.inner.connected.is_set() && !self.inner.stop.is_set()
    }

    pub fn subscribed(&self) -> bool {
        self.inner.subscribed.is_set() && self.connected()
    }

    /// Socket thread alive and not stopped, but no `welcome` yet: the TLS /
    /// WebSocket handshake is still in progress. Only counts for `grace` after
    /// start so a server that never welcomes us still gets replaced.
    pub fn connecting(&self, grace: Duration) -> bool {
        let running = self.inner.thread_id.lock().unwrap().is_some()
            && !self.inner.finished.is_set()
            && !self.inner.stop.is_set();
        let recent = (*self.inner.started_at.lock().unwrap()).is_some_and(|t| t.elapsed() < grace);
        running && recent && !self.inner.connected.is_set()
    }

    /// Spawn the socket thread (no-op if already running).
    pub fn start(&self) -> std::io::Result<()> {
        // Held across the spawn so the new thread always sees its own id
        // (frames it sends must be queued, never awaited).
        let mut thread_id = self.inner.thread_id.lock().unwrap();
        if thread_id.is_some() && !self.inner.finished.is_set() {
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
        *self.inner.subscribe_sent.lock().unwrap() = None;
        let inner = self.inner.clone();
        let handle = thread::Builder::new()
            .name("vesyl-print-cable".into())
            .spawn(move || inner.run())?;
        *thread_id = Some(handle.thread().id());
        *self.inner.started_at.lock().unwrap() = Some(Instant::now());
        Ok(())
    }

    /// Signal the socket to close. Never hangs the agent main loop.
    pub fn stop(&self, timeout: Duration) {
        self.inner.stop.set();
        self.inner.subscribed.clear();
        self.inner.wake();
        if !self.inner.on_socket_thread() && self.inner.thread_id.lock().unwrap().is_some() {
            self.inner.finished.wait(timeout);
        }
        self.inner.connected.clear();
    }

    pub fn wait_subscribed(&self, timeout: Duration) -> bool {
        self.inner.subscribed.wait(timeout)
    }

    /// Invoke a channel action (heartbeat, ack_job, job_status, …).
    /// Null values in `data` are dropped, like Python's `**kwargs` filter.
    ///
    /// From another thread this returns once the frame is written, or an
    /// error after ~2 s if the socket thread is busy (the frame is then never
    /// sent). From the socket thread it only queues the frame.
    pub fn perform(&self, action: &str, data: JsonObject) -> Result<(), BoxError> {
        self.inner.perform(action, data)
    }

    /// Feed one server frame through the protocol handler (tests / replay).
    pub fn handle_text(&self, text: &str) {
        self.inner.handle_text(text);
    }

    #[cfg(test)]
    fn set_sender(&self, f: SendFn) {
        *self.inner.capture.lock().unwrap() = Some(f);
    }

    /// Adjust settings before `start()`.
    #[cfg(test)]
    fn tuned(mut self, f: impl FnOnce(&mut Settings)) -> Self {
        f(&mut Arc::get_mut(&mut self.inner)
            .expect("tune before start")
            .settings);
        self
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

    fn on_socket_thread(&self) -> bool {
        *self.thread_id.lock().unwrap() == Some(thread::current().id())
    }

    /// Hand `frame` to the socket thread, the only writer. Another thread
    /// waits (bounded) for the write. The socket thread itself only queues:
    /// waiting on its own reply would deadlock, and the frame goes out as
    /// soon as the frame being handled returns.
    fn send(&self, frame: &Value) -> Result<(), BoxError> {
        let raw = frame.to_string();
        if self.stop.is_set() {
            return Err("cable not connected".into());
        }
        #[cfg(test)]
        if let Some(capture) = self.capture.lock().unwrap().as_ref() {
            return capture(&raw);
        }
        let reply = (!self.on_socket_thread()).then(Arc::<Reply>::default);
        {
            let outbox = self.outbox.lock().unwrap();
            let outbox = outbox.as_ref().ok_or("cable not connected")?;
            let item = Outgoing {
                raw,
                reply: reply.clone(),
            };
            outbox
                .frames
                .send(item)
                .map_err(|_| "cable not connected")?;
            // The socket thread drains the queue before it next sleeps.
            if reply.is_some() {
                outbox.wake();
            }
        }
        match reply {
            Some(reply) => reply.wait(self.settings.perform_timeout),
            None => Ok(()),
        }
    }

    /// Interrupt the socket thread's `poll()`.
    fn wake(&self) {
        if let Some(outbox) = self.outbox.lock().unwrap().as_ref() {
            outbox.wake();
        }
    }

    /// End the session from the protocol side. `connected()` turns false at
    /// once, so the agent replaces the session.
    fn close_session(&self) {
        self.stop.set();
        self.subscribed.clear();
        self.wake();
    }

    fn subscribe(&self) {
        *self.subscribe_sent.lock().unwrap() = Some(Instant::now());
        let frame = json!({ "command": "subscribe", "identifier": self.identifier });
        if let Err(e) = self.send(&frame) {
            log::error!(target: LOG, "cable subscribe send failed: {e}");
        }
    }

    /// When an unconfirmed subscription gives up.
    fn confirm_deadline(&self) -> Option<Instant> {
        if self.subscribed.is_set() {
            return None;
        }
        (*self.subscribe_sent.lock().unwrap()).map(|t| t + self.settings.subscribe_timeout)
    }

    fn handle_text(&self, text: &str) {
        let Ok(value) = serde_json::from_str::<Value>(text) else {
            log::warn!(target: LOG, "cable non-JSON frame");
            return;
        };
        let Value::Object(mut data) = value else {
            return;
        };

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
                // ActionCable keeps the socket open (and pinging) after a
                // reject; hang up so the agent reconnects instead of sitting
                // connected-but-unsubscribed forever.
                log::error!(target: LOG, "cable subscription rejected — closing");
                self.close_session();
                return;
            }
            _ => {}
        }

        // Moved, not cloned: a print_job may carry tens of MiB of inline content.
        if let Some(Value::Object(msg)) = data.remove("message") {
            if let Some(cb) = &self.callbacks.on_message {
                guarded("cable message", || cb(msg));
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
        if !self.stop.is_set() {
            if let Some(cb) = &self.callbacks.on_disconnected {
                guarded("on_disconnected", || cb());
            }
        }
        self.finished.set();
    }

    fn connect(&self) -> Result<Ws, BoxError> {
        let url = Url::parse(&self.url)?;
        let secure = match url.scheme() {
            "wss" => true,
            "ws" => false,
            other => return Err(format!("unsupported cable url scheme {other:?}").into()),
        };
        let host = url.host().ok_or("cable url missing host")?;
        let port = url
            .port_or_known_default()
            .ok_or("cable url missing port")?;
        if secure && matches!(host, Host::Ipv6(_)) {
            // tungstenite takes the TLS server name from the bracketed URI
            // host, which rustls rejects as an invalid DNS name.
            return Err("wss:// to an IPv6 literal is not supported; use a DNS name".into());
        }
        let stream = match cable_proxy(&url, &*self.settings.env) {
            Some(proxy) => {
                log::info!(target: LOG, "cable connecting via proxy {}:{}", proxy.host, proxy.port);
                open_tunnel(&proxy, &authority(&host, port))?
            }
            // Resolves IPv6 literals unbracketed (host_str() keeps the brackets).
            None => connect_any(&url.socket_addrs(|| None)?)?,
        };
        stream.set_nodelay(true)?;
        stream.set_read_timeout(Some(CONNECT_TIMEOUT))?;
        stream.set_write_timeout(Some(CONNECT_TIMEOUT))?;
        let config = WebSocketConfig::default()
            .max_message_size(Some(MAX_MESSAGE_BYTES))
            .max_frame_size(Some(MAX_MESSAGE_BYTES));
        let (ws, _resp) = tungstenite::client_tls_with_config(
            handshake_request(&url)?,
            stream,
            Some(config),
            None,
        )
        .map_err(|e| format!("handshake failed: {e}"))?;
        let tcp = tcp_of(&ws);
        tcp.set_read_timeout(Some(READ_TIMEOUT))?;
        tcp.set_write_timeout(Some(WRITE_TIMEOUT))?;
        Ok(ws)
    }

    fn connect_and_serve(&self) -> Result<(), BoxError> {
        let mut ws = self.connect()?;
        log::info!(target: LOG, "cable socket open — waiting for welcome");
        let (frames, queued) = mpsc::channel();
        let (wake_tx, wake_rx) = UnixStream::pair()?;
        wake_tx.set_nonblocking(true)?;
        wake_rx.set_nonblocking(true)?;
        *self.outbox.lock().unwrap() = Some(Outbox {
            frames,
            wake: wake_tx,
        });

        let result = self.serve(&mut ws, &queued, &wake_rx);

        // perform() fails fast from here on, and callers still waiting on a
        // queued frame get an error now instead of at their timeout.
        self.connected.clear();
        self.subscribed.clear();
        *self.outbox.lock().unwrap() = None;
        while queued.try_recv().is_ok() {}
        // Best-effort close; never block shutdown on it.
        let _ = tcp_of(&ws).set_write_timeout(Some(CLOSE_TIMEOUT));
        let _ = ws.close(None);
        result
    }

    /// The socket thread's loop: write queued frames, keep the connection
    /// alive, and sleep in `poll()` until the socket has data, a frame is
    /// queued (wake-up pipe) or a timer is due.
    fn serve(
        &self,
        ws: &mut Ws,
        queued: &mpsc::Receiver<Outgoing>,
        wake: &UnixStream,
    ) -> Result<(), BoxError> {
        let (ping_interval, ping_timeout) =
            (self.settings.ping_interval, self.settings.ping_timeout);
        let mut last_ping = Instant::now();
        let mut awaiting_pong: Option<Instant> = None;
        // poll() cannot see frames the handshake (or the previous read)
        // already buffered, so read until the socket runs dry before sleeping.
        let mut maybe_buffered = true;
        while !self.stop.is_set() {
            // Every pass, including right after each handled frame.
            self.write_queued(ws, queued)?;

            let now = Instant::now();
            let mut wake_at = now + MAX_IDLE;
            match awaiting_pong {
                // Judged only once everything received so far has been read:
                // a pong that came in while a callback ran is not missing.
                Some(sent) if now >= sent + ping_timeout && !maybe_buffered => {
                    return Err("ping/pong timed out".into())
                }
                Some(sent) => wake_at = wake_at.min(sent + ping_timeout),
                None if now >= last_ping + ping_interval => {
                    ws.send(Message::Ping(Default::default()))?;
                    last_ping = now;
                    awaiting_pong = Some(now);
                    wake_at = wake_at.min(now + ping_timeout);
                }
                None => wake_at = wake_at.min(last_ping + ping_interval),
            }
            if let Some(deadline) = self.confirm_deadline() {
                if now >= deadline {
                    log::warn!(
                        target: LOG,
                        "cable subscription not confirmed within {:?} — closing",
                        self.settings.subscribe_timeout
                    );
                    self.close_session();
                    break;
                }
                wake_at = wake_at.min(deadline);
            }

            if !maybe_buffered {
                let timeout = wake_at.saturating_duration_since(Instant::now());
                let (readable, woken) =
                    wait_readable(tcp_of(ws).as_raw_fd(), wake.as_raw_fd(), timeout)?;
                if woken {
                    drain_wakeups(wake);
                }
                if !readable {
                    continue;
                }
            }

            match read_nonblocking(ws) {
                // Text (ActionCable's pings included), Ping and Binary frames
                // leave the pong wait alone; see PING_INTERVAL.
                Ok(Message::Text(text)) => {
                    maybe_buffered = true;
                    self.handle_text(text.as_str());
                }
                Ok(Message::Pong(_)) => {
                    maybe_buffered = true;
                    awaiting_pong = None;
                }
                Ok(Message::Close(frame)) => {
                    maybe_buffered = true;
                    log::info!(target: LOG, "cable closed status={:?}", frame.map(|f| f.code));
                }
                Ok(_) => maybe_buffered = true,
                Err(tungstenite::Error::Io(e))
                    if matches!(
                        e.kind(),
                        ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted
                    ) =>
                {
                    maybe_buffered = false;
                    // Send any reply the read queued (pong, close) before sleeping.
                    match ws.flush() {
                        Err(e) if is_closed(&e) => break,
                        other => other?,
                    }
                }
                Err(e) if is_closed(&e) => {
                    log::info!(target: LOG, "cable closed");
                    break;
                }
                Err(tungstenite::Error::Capacity(e)) => {
                    return Err(format!(
                        "cable message over the {} MiB limit: {e}",
                        MAX_MESSAGE_BYTES >> 20
                    )
                    .into())
                }
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }

    /// Write queued frames in order, skipping any whose caller gave up.
    fn write_queued(&self, ws: &mut Ws, queued: &mpsc::Receiver<Outgoing>) -> Result<(), BoxError> {
        while let Ok(mut item) = queued.try_recv() {
            if item.reply.as_ref().is_some_and(|r| !r.claim()) {
                continue;
            }
            let result = ws.send(Message::text(std::mem::take(&mut item.raw)));
            if let Some(reply) = &item.reply {
                reply.finish(result.as_ref().map(|_| ()).map_err(ToString::to_string));
            }
            // A failed or timed-out write leaves the stream unusable.
            result.map_err(|e| format!("cable send failed: {e}"))?;
        }
        Ok(())
    }
}

fn is_closed(e: &tungstenite::Error) -> bool {
    matches!(
        e,
        tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed
    )
}

/// Upgrade request with an `Origin` header like websocket-client sends.
fn handshake_request(url: &Url) -> Result<Request, BoxError> {
    let mut request = url.as_str().into_client_request()?;
    request
        .headers_mut()
        .insert(ORIGIN, HeaderValue::from_str(&origin_for(url))?);
    Ok(request)
}

/// websocket-client's `Origin`: `https://host[:port]` for wss, `http://…` for
/// ws, without the port when it is the scheme default.
fn origin_for(url: &Url) -> String {
    let scheme = if url.scheme() == "wss" {
        "https"
    } else {
        "http"
    };
    // host_str() keeps IPv6 brackets; port() is None for the scheme default.
    let host = url.host_str().unwrap_or_default();
    match url.port() {
        Some(port) => format!("{scheme}://{host}:{port}"),
        None => format!("{scheme}://{host}"),
    }
}

/// `host:port` in authority form (IPv6 bracketed).
fn authority(host: &Host<&str>, port: u16) -> String {
    match host {
        Host::Domain(domain) => format!("{domain}:{port}"),
        Host::Ipv4(ip) => format!("{ip}:{port}"),
        Host::Ipv6(ip) => format!("[{ip}]:{port}"),
    }
}

/// The HTTP proxy to tunnel through, chosen like websocket-client (see
/// [`net::proxy_url_for`]). Loopback targets always connect directly: a
/// proxy cannot reach our loopback, and websocket-client skips localhost too.
fn cable_proxy(url: &Url, env: net::Env) -> Option<ProxyTarget> {
    let loopback = match url.host()? {
        Host::Domain(domain) => domain.eq_ignore_ascii_case("localhost"),
        Host::Ipv4(ip) => ip.is_loopback(),
        Host::Ipv6(ip) => ip.is_loopback(),
    };
    if loopback {
        return None;
    }
    let raw = net::proxy_url_for(url, env)?;
    let proxy = net::parse_proxy(&raw);
    if proxy.is_none() {
        log::warn!(
            target: LOG,
            "cable: ignoring unsupported proxy {} (only http:// and https:// proxies can tunnel); connecting directly",
            without_userinfo(&raw)
        );
    }
    proxy
}

/// A proxy URL safe to log (credentials removed).
fn without_userinfo(proxy: &str) -> String {
    let (scheme, rest) = proxy.split_once("://").unwrap_or(("", proxy));
    let rest = rest.rsplit_once('@').map_or(rest, |(_, host)| host);
    if scheme.is_empty() {
        rest.to_string()
    } else {
        format!("{scheme}://{rest}")
    }
}

fn connect_any(addrs: &[SocketAddr]) -> Result<TcpStream, BoxError> {
    let mut last_err: Option<std::io::Error> = None;
    for addr in addrs {
        match TcpStream::connect_timeout(addr, CONNECT_TIMEOUT) {
            Ok(stream) => return Ok(stream),
            Err(e) => last_err = Some(e),
        }
    }
    Err(last_err
        .map(Into::into)
        .unwrap_or_else(|| "host resolved to no addresses".into()))
}

/// Open a `CONNECT` tunnel to `target` (`host:port`) through an HTTP proxy,
/// like websocket-client. TLS and the WebSocket handshake then run inside it.
fn open_tunnel(proxy: &ProxyTarget, target: &str) -> Result<TcpStream, BoxError> {
    let via = format!("proxy {}:{}", proxy.host, proxy.port);
    let addrs: Vec<SocketAddr> = (proxy.host.as_str(), proxy.port)
        .to_socket_addrs()
        .map_err(|e| format!("{via}: {e}"))?
        .collect();
    let mut stream = connect_any(&addrs).map_err(|e| format!("{via}: {e}"))?;
    stream.set_read_timeout(Some(CONNECT_TIMEOUT))?;
    stream.set_write_timeout(Some(CONNECT_TIMEOUT))?;
    let mut request = format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n");
    if let Some(auth) = &proxy.authorization {
        request.push_str(&format!("Proxy-Authorization: {auth}\r\n"));
    }
    request.push_str("\r\n");
    stream.write_all(request.as_bytes())?;
    let head = read_head(&mut stream).map_err(|e| format!("{via}: {e}"))?;
    let status_line = head.lines().next().unwrap_or_default();
    if status_line.split_whitespace().nth(1) != Some("200") {
        return Err(format!("{via} refused CONNECT {target}: {status_line}").into());
    }
    Ok(stream)
}

/// Read an HTTP response head one byte at a time, so none of the tunnelled
/// bytes that follow it are consumed.
fn read_head(stream: &mut TcpStream) -> Result<String, BoxError> {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() >= MAX_PROXY_HEAD {
            return Err("response head too large".into());
        }
        stream.read_exact(&mut byte).map_err(|e| match e.kind() {
            ErrorKind::UnexpectedEof => "connection closed before the response head".into(),
            _ => BoxError::from(e),
        })?;
        head.push(byte[0]);
    }
    Ok(String::from_utf8_lossy(&head).into_owned())
}

/// One `read()` that never blocks; writes stay blocking (bounded by WRITE_TIMEOUT).
fn read_nonblocking(ws: &mut Ws) -> tungstenite::Result<Message> {
    tcp_of(ws).set_nonblocking(true)?;
    let read = ws.read();
    tcp_of(ws).set_nonblocking(false)?;
    read
}

/// Sleep until `sock` is readable, `wake` is signalled or `timeout` passes.
/// Returns `(sock_ready, woken)`; hang-ups and errors count as ready so the
/// next read reports them.
fn wait_readable(sock: RawFd, wake: RawFd, timeout: Duration) -> std::io::Result<(bool, bool)> {
    let mut fds = [
        libc::pollfd {
            fd: sock,
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            fd: wake,
            events: libc::POLLIN,
            revents: 0,
        },
    ];
    // Round up so a sub-millisecond remainder sleeps instead of spinning.
    let millis = timeout.as_nanos().div_ceil(1_000_000).min(i32::MAX as u128) as libc::c_int;
    // SAFETY: `fds` is a valid array of `fds.len()` pollfd structs for the whole call.
    let rc = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, millis) };
    if rc < 0 {
        let err = std::io::Error::last_os_error();
        return match err.kind() {
            ErrorKind::Interrupted => Ok((false, false)),
            _ => Err(err),
        };
    }
    Ok((fds[0].revents != 0, fds[1].revents != 0))
}

fn drain_wakeups(mut wake: &UnixStream) {
    let mut buf = [0u8; 64];
    while matches!(wake.read(&mut buf), Ok(n) if n > 0) {}
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
    /// How long `start` lingers once the socket thread runs, as if the
    /// starting thread lost the CPU there (tests).
    #[cfg(test)]
    start_pause: Duration,
}

impl PrintCableSession {
    pub fn new(cable_url: &str, get_ticket: TicketFn, handlers: SessionHandlers) -> Self {
        PrintCableSession {
            cable_url: cable_url.trim_end_matches('/').to_string(),
            get_ticket,
            handlers: Arc::new(handlers),
            client: Mutex::new(None),
            lifecycle: Mutex::new(()),
            #[cfg(test)]
            start_pause: Duration::ZERO,
        }
    }

    /// Have `start` linger `pause` after the socket thread starts.
    #[cfg(test)]
    pub(crate) fn with_start_pause(mut self, pause: Duration) -> Self {
        self.start_pause = pause;
        self
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

    /// Handshake still in progress (see [`ActionCableClient::connecting`]).
    pub fn connecting(&self) -> bool {
        self.current()
            .is_some_and(|c| c.connecting(HANDSHAKE_GRACE))
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
        // In place before the socket thread starts: it may subscribe before
        // start() returns, and on_subscribed's perform() needs the client.
        *self.client.lock().unwrap() = Some(client.clone());
        if let Err(e) = client.start() {
            log::warn!(target: LOG, "cable: start failed: {e}");
            self.client.lock().unwrap().take();
            return false;
        }
        #[cfg(test)]
        thread::sleep(self.start_pause);
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

    /// Perform a channel action if subscribed. Returns false otherwise, or if
    /// the frame could not be sent (caller falls back to REST).
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

fn dispatch_message(h: &SessionHandlers, mut msg: JsonObject) {
    match msg.get("type").and_then(Value::as_str) {
        Some("print_job") => match msg.remove("job") {
            Some(Value::Object(job)) => {
                if let Some(cb) = &h.on_print_job {
                    cb(job);
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
    use base64::Engine as _;
    use std::collections::HashMap;
    use std::net::{Shutdown, TcpListener};
    use tungstenite::http::HeaderMap;

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

    fn obj(v: Value) -> JsonObject {
        v.as_object().unwrap().clone()
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
    fn reject_closes_and_disconnect_stops() {
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
        // Rejected sessions hang up so the agent replaces them.
        assert!(!client.connected());

        let client = ActionCableClient::new(
            "wss://example/print/cable?token=t",
            ClientCallbacks::default(),
        );
        let _sent = capture(&client);
        client.handle_text(&fixture("welcome.json"));
        assert!(client.connected());
        client.handle_text(r#"{"type":"disconnect","reason":"unauthorized"}"#);
        assert!(!client.connected());
    }

    #[test]
    fn origin_like_websocket_client() {
        let origin = |url: &str| origin_for(&Url::parse(url).unwrap());
        assert_eq!(
            origin("wss://wms-api.vesyl.dev/print/cable?token=t"),
            "https://wms-api.vesyl.dev"
        );
        assert_eq!(
            origin("wss://wms-api.vesyl.dev:443/print/cable"),
            "https://wms-api.vesyl.dev"
        );
        assert_eq!(
            origin("wss://wms-api.vesyl.dev:8443/print/cable"),
            "https://wms-api.vesyl.dev:8443"
        );
        assert_eq!(origin("ws://dev.lan:80/print/cable"), "http://dev.lan");
        assert_eq!(
            origin("ws://10.0.0.5:3000/print/cable"),
            "http://10.0.0.5:3000"
        );
        assert_eq!(
            origin("ws://[fd00::10]:3000/print/cable"),
            "http://[fd00::10]:3000"
        );
    }

    fn env_of(pairs: &[(&str, &str)]) -> EnvFn {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        Box::new(move |k| map.get(k).cloned())
    }

    #[test]
    fn proxy_choice_follows_websocket_client() {
        let pick = |url: &str, env: &[(&str, &str)]| {
            cable_proxy(&Url::parse(url).unwrap(), &env_of(env))
                .map(|p| format!("{}:{}", p.host, p.port))
        };
        let https = [("https_proxy", "http://proxy.lan:3128")];
        assert_eq!(
            pick("wss://wms-api.vesyl.dev/print/cable", &https).as_deref(),
            Some("proxy.lan:3128")
        );
        // ws:// only looks at http_proxy.
        assert_eq!(pick("ws://wms-api.vesyl.dev/print/cable", &https), None);
        assert_eq!(
            pick(
                "ws://10.0.0.5:3000/print/cable",
                &[("http_proxy", "proxy.lan:8080")]
            )
            .as_deref(),
            Some("proxy.lan:8080")
        );
        let bypassed = [("https_proxy", "http://p:1"), ("no_proxy", ".vesyl.dev")];
        assert_eq!(pick("wss://wms-api.vesyl.dev/print/cable", &bypassed), None);
        for local in [
            "ws://127.0.0.1:3000/x",
            "ws://LOCALHOST:3000/x",
            "ws://[::1]:3000/x",
        ] {
            assert_eq!(
                pick(local, &[("http_proxy", "http://p:1")]),
                None,
                "{local}"
            );
        }
        // An https:// proxy URL tunnels too, through a plain CONNECT.
        let tls_proxy = [("https_proxy", "https://proxy.lan:3129")];
        assert_eq!(
            pick("wss://wms-api.vesyl.dev/print/cable", &tls_proxy).as_deref(),
            Some("proxy.lan:3129")
        );
        let socks = [("https_proxy", "socks5://user:secret@p:1080")];
        assert_eq!(pick("wss://wms-api.vesyl.dev/print/cable", &socks), None);
        assert_eq!(
            without_userinfo("socks5://user:secret@p:1080"),
            "socks5://p:1080"
        );
        assert_eq!(without_userinfo("u:pw@proxy.lan:3128"), "proxy.lan:3128");
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
        sess.dispatch_message(obj(
            json!({"type": "node_config", "node_id": "n1", "name": "Pack 1"}),
        ));
        sess.dispatch_message(obj(json!({"type": "job_canceled", "job_id": "j7"})));
        sess.dispatch_message(obj(json!({"type": "job_canceled"})));
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

    // --- loopback servers ---------------------------------------------------

    type ServerWs = WebSocket<TcpStream>;

    /// Accept one WebSocket client: the socket, request target and headers.
    #[allow(clippy::result_large_err)] // accept_hdr's callback signature is fixed by tungstenite
    fn accept_ws(listener: &TcpListener) -> (ServerWs, String, HeaderMap) {
        let (stream, _) = listener.accept().unwrap();
        let mut seen = None;
        let ws = tungstenite::accept_hdr(
            stream,
            |req: &tungstenite::handshake::server::Request, resp| {
                seen = Some((req.uri().to_string(), req.headers().clone()));
                Ok(resp)
            },
        )
        .unwrap();
        let (target, headers) = seen.unwrap();
        (ws, target, headers)
    }

    /// welcome → expect subscribe → confirm.
    fn welcome_and_confirm(ws: &mut ServerWs) {
        ws.send(Message::text(fixture("welcome.json"))).unwrap();
        let sub: Value = serde_json::from_str(ws.read().unwrap().to_text().unwrap()).unwrap();
        assert_eq!(sub["command"], "subscribe");
        ws.send(Message::text(fixture("confirm_subscription.json")))
            .unwrap();
    }

    /// Behave like Rails once subscribed: an ActionCable ping every `every`,
    /// client text frames to `on_text`, until the client hangs up.
    fn serve_pings(ws: &mut ServerWs, every: Duration, mut on_text: impl FnMut(String)) {
        ws.get_ref()
            .set_read_timeout(Some(Duration::from_millis(5)))
            .unwrap();
        let mut last = Instant::now();
        loop {
            if last.elapsed() >= every {
                let ping = Message::text(r#"{"type":"ping","message":1700000000}"#);
                if ws.send(ping).is_err() {
                    return;
                }
                last = Instant::now();
            }
            match ws.read() {
                Ok(Message::Text(text)) => on_text(text.to_string()),
                Ok(_) => {}
                Err(tungstenite::Error::Io(e))
                    if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
                Err(_) => return,
            }
        }
    }

    fn job_id_of(raw: &str) -> String {
        let frame: Value = serde_json::from_str(raw).unwrap();
        let data: Value = serde_json::from_str(frame["data"].as_str().unwrap()).unwrap();
        py_str(&data["job_id"])
    }

    fn ticket(t: &'static str) -> TicketFn {
        Box::new(move || Ok(obj(json!({ "ticket": t }))))
    }

    fn wait_until(timeout: Duration, f: impl Fn() -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if f() {
                return true;
            }
            thread::sleep(Duration::from_millis(5));
        }
        f()
    }

    /// A session that is still handshaking must not look "dead" to the agent.
    #[test]
    fn connecting_until_welcome_within_grace() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let server = thread::spawn(move || {
            let (mut ws, _, _) = accept_ws(&listener);
            // Never send welcome until released.
            let _ = release_rx.recv_timeout(Duration::from_secs(5));
            let _ = ws.send(Message::text(fixture("welcome.json")));
            // Stay connected until the client goes away.
            while ws.read().is_ok() {}
        });
        let client = ActionCableClient::new(
            &format!("ws://{addr}/print/cable"),
            ClientCallbacks::default(),
        );
        client.start().unwrap();
        assert!(
            wait_until(Duration::from_secs(5), || client
                .connecting(Duration::from_secs(30))),
            "handshaking client counts as connecting"
        );
        assert!(!client.connecting(Duration::ZERO), "grace period expired");
        assert!(!client.connected());
        release_tx.send(()).unwrap();
        assert!(wait_until(Duration::from_secs(5), || client.connected()));
        assert!(
            !client.connecting(Duration::from_secs(30)),
            "welcomed client is connected, not connecting"
        );
        client.stop(Duration::from_secs(2));
        assert!(!client.connecting(Duration::from_secs(30)));
        server.join().unwrap();
    }

    /// End-to-end against a local tungstenite server speaking ActionCable.
    #[test]
    fn live_socket_roundtrip() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = mpsc::channel::<String>();
        let server = thread::spawn(move || {
            let (mut ws, target, headers) = accept_ws(&listener);
            tx.send(target).unwrap();
            tx.send(headers["origin"].to_str().unwrap().to_string())
                .unwrap();
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
            ticket("abc"),
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
        // websocket-client sends Origin; Rails' forgery protection may check it.
        assert_eq!(rx.recv_timeout(wait).unwrap(), format!("http://{addr}"));
        let sub: Value = serde_json::from_str(&rx.recv_timeout(wait).unwrap()).unwrap();
        assert_eq!(sub["command"], "subscribe");
        assert!(sess.wait_subscribed(wait));
        let ack: Value = serde_json::from_str(&rx.recv_timeout(wait).unwrap()).unwrap();
        let inner: Value = serde_json::from_str(ack["data"].as_str().unwrap()).unwrap();
        assert_eq!(inner["action"], "ack_job");
        assert_eq!(inner["job_id"], "job-uuid-1");

        sess.stop();
        assert!(!sess.connected());
        sess_slot.lock().unwrap().take();
        server.join().unwrap();
    }

    /// C00: perform() from the agent's main thread used to queue behind the
    /// socket thread's 250 ms reads and took seconds. The socket thread is now
    /// the only writer and wakes up as soon as a frame is queued.
    #[test]
    fn cross_thread_perform_stays_fast_while_server_pings() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = mpsc::channel::<String>();
        let server = thread::spawn(move || {
            let (mut ws, _, _) = accept_ws(&listener);
            welcome_and_confirm(&mut ws);
            serve_pings(&mut ws, Duration::from_millis(300), |t| {
                let _ = tx.send(t);
            });
        });
        let sess = PrintCableSession::new(
            &format!("ws://{addr}/print/cable"),
            ticket("abc"),
            SessionHandlers::default(),
        );
        assert!(sess.start());
        assert!(sess.wait_subscribed(Duration::from_secs(5)));

        let mut slowest = Duration::ZERO;
        for i in 0..40u64 {
            thread::sleep(Duration::from_millis(i * 37 % 50));
            let t = Instant::now();
            let ok = sess.perform("ack_job", obj(json!({ "job_id": format!("j{i}") })));
            slowest = slowest.max(t.elapsed());
            assert!(ok, "perform {i} failed");
        }
        assert!(
            slowest < Duration::from_millis(250),
            "slowest cross-thread perform took {slowest:?}"
        );
        let ids: Vec<String> = (0..40)
            .map(|_| job_id_of(&rx.recv_timeout(Duration::from_secs(5)).unwrap()))
            .collect();
        let expected: Vec<String> = (0..40).map(|i| format!("j{i}")).collect();
        assert_eq!(ids, expected, "every frame delivered once, in order");

        sess.stop();
        server.join().unwrap();
    }

    /// Callbacks run on the socket thread; their perform() must queue, not
    /// wait on the thread that is running them.
    #[test]
    fn perform_from_a_callback_is_queued_not_awaited() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = mpsc::channel::<String>();
        let server = thread::spawn(move || {
            let (mut ws, _, _) = accept_ws(&listener);
            welcome_and_confirm(&mut ws);
            serve_pings(&mut ws, Duration::from_millis(100), |t| {
                let _ = tx.send(t);
            });
        });
        let slot: Arc<Mutex<Option<Arc<PrintCableSession>>>> = Arc::default();
        let (s2, (took_tx, took_rx)) = (slot.clone(), mpsc::channel());
        let sess = Arc::new(PrintCableSession::new(
            &format!("ws://{addr}/print/cable"),
            ticket("abc"),
            SessionHandlers {
                on_subscribed: Some(Arc::new(move || {
                    let s = s2.lock().unwrap().clone().unwrap();
                    let t = Instant::now();
                    let ok = s.perform("report_printers", obj(json!({ "printers": [] })));
                    took_tx.send((ok, t.elapsed())).unwrap();
                })),
                ..Default::default()
            },
        ));
        *slot.lock().unwrap() = Some(sess.clone());
        assert!(sess.start());

        let (ok, took) = took_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(ok);
        assert!(
            took < Duration::from_millis(100),
            "queued, not awaited: {took:?}"
        );
        let frame: Value =
            serde_json::from_str(&rx.recv_timeout(Duration::from_secs(5)).unwrap()).unwrap();
        let data: Value = serde_json::from_str(frame["data"].as_str().unwrap()).unwrap();
        assert_eq!(data["action"], "report_printers");

        sess.stop();
        slot.lock().unwrap().take();
        server.join().unwrap();
    }

    /// J2: the socket thread can subscribe before start() returns (here
    /// start() lingers 300 ms once the thread runs, as if preempted). The
    /// client used to be stored only after that, so on_subscribed's
    /// report_printers found no client and was dropped.
    #[test]
    fn on_subscribed_can_perform_before_start_returns() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = mpsc::channel::<String>();
        let server = thread::spawn(move || {
            let (mut ws, _, _) = accept_ws(&listener);
            welcome_and_confirm(&mut ws);
            serve_pings(&mut ws, Duration::from_millis(100), |t| {
                let _ = tx.send(t);
            });
        });
        let slot: Arc<Mutex<Option<Arc<PrintCableSession>>>> = Arc::default();
        let (s2, (done_tx, done_rx)) = (slot.clone(), mpsc::channel());
        let sess = Arc::new(
            PrintCableSession::new(
                &format!("ws://{addr}/print/cable"),
                ticket("abc"),
                SessionHandlers {
                    on_subscribed: Some(Arc::new(move || {
                        let s = s2.lock().unwrap().clone().unwrap();
                        let ok = s.perform("report_printers", obj(json!({ "printers": [] })));
                        done_tx.send(ok).unwrap();
                    })),
                    ..Default::default()
                },
            )
            .with_start_pause(Duration::from_millis(300)),
        );
        *slot.lock().unwrap() = Some(sess.clone());
        assert!(sess.start());

        let performed = done_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let frame = rx.recv_timeout(Duration::from_secs(5));
        sess.stop();
        slot.lock().unwrap().take();
        server.join().unwrap();
        assert!(performed, "on_subscribed's perform found no client");
        let frame: Value = serde_json::from_str(&frame.unwrap()).unwrap();
        let data: Value = serde_json::from_str(frame["data"].as_str().unwrap()).unwrap();
        assert_eq!(data["action"], "report_printers");
    }

    /// A perform() that times out while the socket thread is busy is dropped,
    /// never sent late: the caller already fell back to REST, and a late cable
    /// copy (say job_status=printing after delivered) could regress the job.
    #[test]
    fn timed_out_perform_is_never_sent_late() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = mpsc::channel::<String>();
        let server = thread::spawn(move || {
            let (mut ws, _, _) = accept_ws(&listener);
            welcome_and_confirm(&mut ws);
            serve_pings(&mut ws, Duration::from_millis(100), |t| {
                let _ = tx.send(t);
            });
        });
        let client = ActionCableClient::new(
            &format!("ws://{addr}/print/cable"),
            ClientCallbacks {
                // Slow socket-thread work (the agent's on_subscribed runs a CUPS inventory).
                on_subscribed: Some(Arc::new(|| thread::sleep(Duration::from_secs(1)))),
                ..Default::default()
            },
        )
        .tuned(|s| s.perform_timeout = Duration::from_millis(200));
        client.start().unwrap();
        assert!(client.wait_subscribed(Duration::from_secs(5)));

        let t = Instant::now();
        let err = client
            .perform("ack_job", obj(json!({ "job_id": "late" })))
            .expect_err("socket thread is busy");
        assert!(t.elapsed() < Duration::from_secs(1), "{:?}", t.elapsed());
        assert!(err.to_string().contains("busy"), "{err}");

        // Once the callback returns, sends work again.
        assert!(wait_until(Duration::from_secs(3), || client
            .perform("ack_job", obj(json!({ "job_id": "ok" })))
            .is_ok()));
        let first = job_id_of(&rx.recv_timeout(Duration::from_secs(5)).unwrap());
        assert_eq!(first, "ok", "the abandoned frame must not go out");
        assert!(rx.recv_timeout(Duration::from_millis(300)).is_err());

        client.stop(Duration::from_secs(2));
        server.join().unwrap();
    }

    /// Frames still queued when the session ends fail at once instead of
    /// holding the caller for the full timeout.
    #[test]
    fn queued_perform_fails_fast_when_session_ends() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = mpsc::channel::<String>();
        let server = thread::spawn(move || {
            let (mut ws, _, _) = accept_ws(&listener);
            welcome_and_confirm(&mut ws);
            serve_pings(&mut ws, Duration::from_millis(100), |t| {
                let _ = tx.send(t);
            });
        });
        let client = Arc::new(
            ActionCableClient::new(
                &format!("ws://{addr}/print/cable"),
                ClientCallbacks {
                    on_subscribed: Some(Arc::new(|| thread::sleep(Duration::from_secs(1)))),
                    ..Default::default()
                },
            )
            .tuned(|s| s.perform_timeout = Duration::from_secs(10)),
        );
        client.start().unwrap();
        assert!(client.wait_subscribed(Duration::from_secs(5)));

        let c = client.clone();
        let waiter = thread::spawn(move || {
            let t = Instant::now();
            let result = c.perform("ack_job", obj(json!({ "job_id": "j" })));
            (result.is_err(), t.elapsed())
        });
        thread::sleep(Duration::from_millis(100));
        client.stop(Duration::from_secs(3));
        let (failed, took) = waiter.join().unwrap();
        assert!(
            failed,
            "a frame dropped at teardown must not report success"
        );
        assert!(
            took < Duration::from_secs(2),
            "failed at teardown: {took:?}"
        );
        assert!(rx.recv_timeout(Duration::from_millis(200)).is_err());
        server.join().unwrap();
    }

    /// C39: Rails keeps the socket open (and pinging) after reject_subscription.
    #[test]
    fn rejected_subscription_hangs_up() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = mpsc::channel();
        let server = thread::spawn(move || {
            let (mut ws, _, _) = accept_ws(&listener);
            ws.send(Message::text(fixture("welcome.json"))).unwrap();
            ws.read().unwrap();
            let reject =
                json!({ "identifier": channel_identifier(), "type": "reject_subscription" });
            ws.send(Message::text(reject.to_string())).unwrap();
            let t = Instant::now();
            serve_pings(&mut ws, Duration::from_millis(100), |_| {});
            tx.send(t.elapsed()).unwrap();
        });
        let disconnected = Arc::new(Flag::default());
        let d = disconnected.clone();
        let client = ActionCableClient::new(
            &format!("ws://{addr}/print/cable"),
            ClientCallbacks {
                on_disconnected: Some(Arc::new(move || d.set())),
                ..Default::default()
            },
        );
        client.start().unwrap();
        let hung_up = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("client closes the rejected session");
        assert!(hung_up < Duration::from_secs(2), "{hung_up:?}");
        assert!(!client.connected() && !client.subscribed());
        assert!(
            !client.connecting(HANDSHAKE_GRACE),
            "not mistaken for a handshake in progress"
        );
        // Closed on purpose, like a server `disconnect`: no on_disconnected.
        assert!(!disconnected.is_set());
        server.join().unwrap();
    }

    /// C39: no confirm_subscription at all (e.g. an exception in the channel's
    /// `subscribed`) also ends the session after the deadline.
    #[test]
    fn unconfirmed_subscription_times_out() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = mpsc::channel();
        let server = thread::spawn(move || {
            let (mut ws, _, _) = accept_ws(&listener);
            ws.send(Message::text(fixture("welcome.json"))).unwrap();
            ws.read().unwrap();
            let t = Instant::now();
            serve_pings(&mut ws, Duration::from_millis(100), |_| {});
            tx.send(t.elapsed()).unwrap();
        });
        let client = ActionCableClient::new(
            &format!("ws://{addr}/print/cable"),
            ClientCallbacks::default(),
        )
        .tuned(|s| s.subscribe_timeout = Duration::from_millis(300));
        client.start().unwrap();
        let hung_up = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("client gives up on the subscription");
        assert!(
            hung_up >= Duration::from_millis(200) && hung_up < Duration::from_secs(2),
            "{hung_up:?}"
        );
        assert!(!client.connected());
        server.join().unwrap();
    }

    /// N05: a server that stopped reading (so it never answers our WebSocket
    /// ping, nor sees a perform) but still sends ActionCable pings. Every text
    /// frame used to reset the pong wait, so the session stayed "subscribed"
    /// while each perform vanished into the socket buffer instead of falling
    /// back to REST.
    #[test]
    fn server_that_stopped_reading_is_dropped_despite_its_pings() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (done_tx, done_rx) = mpsc::channel::<()>();
        let server = thread::spawn(move || {
            let (mut ws, _, _) = accept_ws(&listener);
            welcome_and_confirm(&mut ws);
            // Never reads again; pings until told to stop or the client is gone.
            let ping = r#"{"type":"ping","message":1700000000}"#;
            while done_rx.recv_timeout(Duration::from_millis(20)).is_err() {
                if ws.send(Message::text(ping)).is_err() {
                    break;
                }
            }
        });
        let disconnected = Arc::new(Flag::default());
        let d = disconnected.clone();
        let client = ActionCableClient::new(
            &format!("ws://{addr}/print/cable"),
            ClientCallbacks {
                on_disconnected: Some(Arc::new(move || d.set())),
                ..Default::default()
            },
        )
        .tuned(|s| {
            s.ping_interval = Duration::from_millis(200);
            s.ping_timeout = Duration::from_millis(300);
        });
        client.start().unwrap();
        assert!(client.wait_subscribed(Duration::from_secs(5)));
        let t = Instant::now();
        // Until the session drops, a perform only has to reach the socket buffer.
        assert!(client
            .perform("job_status", obj(json!({ "job_id": "j1" })))
            .is_ok());
        assert!(
            disconnected.wait(Duration::from_secs(5)),
            "a server that never pongs is dropped"
        );
        // One ping interval plus the timeout: 0.5 s here, at most 35 s by default.
        assert!(t.elapsed() < Duration::from_secs(3), "{:?}", t.elapsed());
        assert!(!client.connected() && !client.subscribed());
        // So the agent's performs fail and it falls back to REST.
        assert!(client
            .perform("job_status", obj(json!({ "job_id": "j1" })))
            .is_err());
        let _ = done_tx.send(());
        server.join().unwrap();
    }

    /// The pong wait is judged only after reading what has already arrived:
    /// a pong that came in while a slow callback ran keeps the session, and a
    /// server that answers every ping stays connected across many of them.
    #[test]
    fn pong_that_arrived_during_a_slow_callback_counts() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (done_tx, done_rx) = mpsc::channel::<()>();
        let server = thread::spawn(move || {
            let (mut ws, _, _) = accept_ws(&listener);
            welcome_and_confirm(&mut ws);
            ws.get_ref()
                .set_read_timeout(Some(Duration::from_millis(5)))
                .unwrap();
            let mut pings = 0;
            while done_rx.try_recv().is_err() {
                match ws.read() {
                    // tungstenite writes the frame we send ahead of the pong it
                    // owes, so the first pong sits behind a slow message.
                    Ok(Message::Ping(_)) => {
                        pings += 1;
                        if pings == 1 {
                            let slow = json!({
                                "identifier": channel_identifier(),
                                "message": {"type": "slow"},
                            });
                            ws.send(Message::text(slow.to_string())).unwrap();
                        }
                    }
                    Ok(_) => {}
                    Err(tungstenite::Error::Io(e))
                        if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
                    Err(_) => break,
                }
            }
            pings
        });
        let disconnected = Arc::new(Flag::default());
        let d = disconnected.clone();
        let client = ActionCableClient::new(
            &format!("ws://{addr}/print/cable"),
            ClientCallbacks {
                // Outlasts the pong timeout while the pong is already buffered.
                on_message: Some(Arc::new(|_| thread::sleep(Duration::from_millis(1500)))),
                on_disconnected: Some(Arc::new(move || d.set())),
                ..Default::default()
            },
        )
        .tuned(|s| {
            s.ping_interval = Duration::from_millis(100);
            s.ping_timeout = Duration::from_millis(800);
        });
        client.start().unwrap();
        assert!(client.wait_subscribed(Duration::from_secs(5)));
        assert!(
            !disconnected.wait(Duration::from_millis(2800)),
            "session dropped"
        );
        assert!(client.subscribed());
        let _ = done_tx.send(());
        client.stop(Duration::from_secs(2));
        let pings = server.join().unwrap();
        assert!(pings >= 4, "only {pings} ping(s) answered");
    }

    /// C21: an IPv6-literal cable URL connects (the bracketed host used to go
    /// to DNS) and sends the same Host/Origin as websocket-client.
    #[test]
    fn ipv6_literal_url_connects() {
        let Ok(listener) = TcpListener::bind("[::1]:0") else {
            eprintln!("skipping: no IPv6 loopback here");
            return;
        };
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = mpsc::channel();
        let server = thread::spawn(move || {
            let (mut ws, _, headers) = accept_ws(&listener);
            tx.send(headers).unwrap();
            welcome_and_confirm(&mut ws);
            while ws.read().is_ok() {}
        });
        let client = ActionCableClient::new(
            &format!("ws://[::1]:{port}/print/cable?token=t"),
            ClientCallbacks::default(),
        );
        client.start().unwrap();
        assert!(client.wait_subscribed(Duration::from_secs(5)));
        let headers = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(headers["host"].to_str().unwrap(), format!("[::1]:{port}"));
        assert_eq!(
            headers["origin"].to_str().unwrap(),
            format!("http://[::1]:{port}")
        );
        client.stop(Duration::from_secs(2));
        server.join().unwrap();
    }

    #[test]
    fn wss_to_ipv6_literal_fails_clearly() {
        let client = ActionCableClient::new(
            "wss://[fd00::10]:3000/print/cable?token=t",
            ClientCallbacks::default(),
        );
        let err = client.inner.connect().expect_err("unsupported").to_string();
        assert!(err.contains("IPv6"), "{err}");
    }

    /// One-shot HTTP CONNECT proxy: reports the request head, answers
    /// `status`, and on 200 relays bytes to `upstream` both ways.
    fn fake_proxy(
        listener: TcpListener,
        status: &'static str,
        upstream: SocketAddr,
        heads: mpsc::Sender<String>,
    ) -> thread::JoinHandle<()> {
        thread::spawn(move || {
            let (mut client, _) = listener.accept().unwrap();
            heads.send(read_head(&mut client).unwrap()).unwrap();
            client
                .write_all(format!("HTTP/1.1 {status}\r\n\r\n").as_bytes())
                .unwrap();
            if !status.starts_with("200") {
                return;
            }
            let mut server = TcpStream::connect(upstream).unwrap();
            let (mut from_client, mut to_server) =
                (client.try_clone().unwrap(), server.try_clone().unwrap());
            let relay = thread::spawn(move || {
                let _ = std::io::copy(&mut from_client, &mut to_server);
                let _ = to_server.shutdown(Shutdown::Write);
            });
            let _ = std::io::copy(&mut server, &mut client);
            let _ = client.shutdown(Shutdown::Write);
            relay.join().unwrap();
        })
    }

    /// C22: like websocket-client, tunnel through http_proxy/https_proxy.
    #[test]
    fn tunnels_through_http_proxy() {
        let ws_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let ws_addr = ws_listener.local_addr().unwrap();
        let proxy_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let proxy_addr = proxy_listener.local_addr().unwrap();
        let (head_tx, head_rx) = mpsc::channel();
        let proxy = fake_proxy(
            proxy_listener,
            "200 Connection established",
            ws_addr,
            head_tx,
        );
        let (hdr_tx, hdr_rx) = mpsc::channel();
        let server = thread::spawn(move || {
            let (mut ws, _, headers) = accept_ws(&ws_listener);
            hdr_tx.send(headers).unwrap();
            welcome_and_confirm(&mut ws);
            while ws.read().is_ok() {}
        });

        // `.invalid` never resolves (RFC 2606): only the tunnel can reach it.
        let port = ws_addr.port();
        let proxy_url = format!("http://dev%40ice:p%3Ass@{proxy_addr}");
        let client = ActionCableClient::new(
            &format!("ws://cable.invalid:{port}/print/cable?token=t"),
            ClientCallbacks::default(),
        )
        .tuned(|s| s.env = env_of(&[("http_proxy", &proxy_url)]));
        client.start().unwrap();
        assert!(
            client.wait_subscribed(Duration::from_secs(5)),
            "subscribed through the tunnel"
        );

        let head = head_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(
            head.starts_with(&format!("CONNECT cable.invalid:{port} HTTP/1.1\r\n")),
            "{head}"
        );
        assert!(
            head.contains(&format!("\r\nHost: cable.invalid:{port}\r\n")),
            "{head}"
        );
        let auth = base64::engine::general_purpose::STANDARD.encode("dev@ice:p:ss");
        assert!(
            head.contains(&format!("\r\nProxy-Authorization: Basic {auth}\r\n")),
            "{head}"
        );
        let headers = hdr_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(
            headers["host"].to_str().unwrap(),
            format!("cable.invalid:{port}")
        );
        assert_eq!(
            headers["origin"].to_str().unwrap(),
            format!("http://cable.invalid:{port}")
        );

        client.stop(Duration::from_secs(2));
        server.join().unwrap();
        proxy.join().unwrap();
    }

    #[test]
    fn proxy_refusal_is_reported() {
        let proxy_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let proxy_addr = proxy_listener.local_addr().unwrap();
        let (head_tx, head_rx) = mpsc::channel();
        let unused: SocketAddr = "127.0.0.1:9".parse().unwrap();
        let proxy = fake_proxy(
            proxy_listener,
            "407 Proxy Authentication Required",
            unused,
            head_tx,
        );
        let client = ActionCableClient::new(
            "wss://cable.invalid/print/cable?token=t",
            ClientCallbacks::default(),
        )
        .tuned(|s| s.env = env_of(&[("https_proxy", &format!("http://{proxy_addr}"))]));
        let err = client.inner.connect().expect_err("refused").to_string();
        assert!(err.contains("407"), "{err}");
        let head = head_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(
            head.starts_with("CONNECT cable.invalid:443 HTTP/1.1\r\n"),
            "{head}"
        );
        assert!(!head.contains("Proxy-Authorization"), "{head}");
        proxy.join().unwrap();
    }

    /// C38: pushes up to the REST limit (64 MiB) arrive; tungstenite's default
    /// 16 MiB frame cap used to drop the connection instead.
    #[test]
    fn accepts_push_over_16_mib() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let size = 17 << 20;
        let server = thread::spawn(move || {
            let (mut ws, _, _) = accept_ws(&listener);
            welcome_and_confirm(&mut ws);
            let push = json!({
                "identifier": channel_identifier(),
                "message": {"type": "print_job", "job": {"id": "big", "raw_base64": "A".repeat(size)}},
            });
            ws.send(Message::text(push.to_string())).unwrap();
            while ws.read().is_ok() {}
        });
        let (tx, rx) = mpsc::channel();
        let client = ActionCableClient::new(
            &format!("ws://{addr}/print/cable"),
            ClientCallbacks {
                on_message: Some(Arc::new(move |m| {
                    let len = m["job"]["raw_base64"].as_str().map_or(0, str::len);
                    tx.send(len).unwrap();
                })),
                ..Default::default()
            },
        );
        client.start().unwrap();
        assert_eq!(rx.recv_timeout(Duration::from_secs(30)).unwrap(), size);
        assert!(client.connected());
        client.stop(Duration::from_secs(2));
        server.join().unwrap();
    }

    /// A frame over the cap is refused from its header, before any payload
    /// is buffered.
    #[test]
    fn oversized_frame_is_refused_at_its_header() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut ws, _, _) = accept_ws(&listener);
            welcome_and_confirm(&mut ws);
            // Text frame header announcing 64 MiB + 1 byte, and no payload.
            let mut header = vec![0x81, 127];
            header.extend_from_slice(&(MAX_MESSAGE_BYTES as u64 + 1).to_be_bytes());
            ws.get_mut().write_all(&header).unwrap();
            while ws.read().is_ok() {}
        });
        let disconnected = Arc::new(Flag::default());
        let d = disconnected.clone();
        let client = ActionCableClient::new(
            &format!("ws://{addr}/print/cable"),
            ClientCallbacks {
                on_disconnected: Some(Arc::new(move || d.set())),
                ..Default::default()
            },
        );
        client.start().unwrap();
        assert!(
            disconnected.wait(Duration::from_secs(5)),
            "dropped at the header"
        );
        assert!(!client.connected());
        server.join().unwrap();
    }
}
