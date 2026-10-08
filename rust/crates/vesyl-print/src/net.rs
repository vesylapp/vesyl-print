//! Shared HTTP plumbing: ureq agents that behave like Python's urllib.
//!
//! - **Timeouts like urllib's.** urllib's `timeout=N` bounds each socket
//!   operation: a slow but steady download never times out, and a connection
//!   that goes silent fails after N seconds. Every read and write here waits
//!   at most [`Timeouts::idle`] (a transport wrapper caps ureq's per-phase
//!   deadline), connecting and the response head keep their own budgets, and
//!   the body also has a generous total budget as a backstop.
//! - **Proxies like urllib.** `<scheme>_proxy` (lowercase wins over
//!   uppercase), `no_proxy` matched the way `proxy_bypass_environment` matches
//!   it, `ALL_PROXY` ignored. The proxy is chosen for every connection, so each
//!   redirect hop gets its own decision. https targets tunnel through a plain
//!   `CONNECT` (also for an `https://` proxy URL), http targets send
//!   absolute-form requests to the proxy, and credentials are percent-decoded
//!   and sent only when both user and password are set. ureq's own proxy
//!   support differs on each point, so it is switched off.
//! - **No resolver threads.** Target and proxy names are looked up on the
//!   calling thread; ureq's resolver starts a thread per lookup whenever a
//!   deadline applies, and fails with a panic when it cannot.
//! - **No transparent gzip** (the `gzip` feature is off): urllib never sends
//!   `Accept-Encoding: gzip`, and a server that gzips a `.tar.gz` would break
//!   the artifact checksum.
//!
//! The connector, transport and resolver hooks come from `ureq::unversioned`,
//! which is outside ureq's semver promise. ureq is pinned (3.4.2) in
//! Cargo.lock; an upgrade must re-check them.

use std::fmt;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use ureq::config::Config;
use ureq::http::Uri;
use ureq::tls::{RootCerts, TlsConfig};
use ureq::unversioned::resolver::{DefaultResolver, ResolvedSocketAddrs, Resolver};
use ureq::unversioned::transport::time::Duration as Deadline;
use ureq::unversioned::transport::{
    Buffers, ConnectionDetails, Connector, NextTimeout, RustlsConnector, TcpConnector, Transport,
};
use url::Url;

const LOG: &str = "vesyl-print.net";
/// Redirects followed per request (urllib's `max_redirections`).
const MAX_REDIRECTS: u32 = 10;
/// Cap on a proxy's response head to `CONNECT`.
const MAX_PROXY_HEAD: usize = 16 << 10;

/// Timeouts for one kind of request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timeouts {
    /// TCP connect and TLS handshake, and sending the request.
    pub connect: Duration,
    /// Waiting for the response head.
    pub response: Duration,
    /// Longest wait for any single read or write (Python's per-operation
    /// timeout): a connection that goes silent mid-body fails after this.
    pub idle: Duration,
    /// Total budget for reading the body, a backstop against a server that
    /// trickles forever. Generous: a slow but steady download must finish.
    pub body: Duration,
}

impl Timeouts {
    /// wms-api REST calls (`CloudClient`, Python timeout=30). The body budget
    /// covers a large jobs/pending payload on a slow link.
    pub const API: Timeouts = Timeouts {
        connect: Duration::from_secs(30),
        response: Duration::from_secs(30),
        idle: Duration::from_secs(30),
        body: Duration::from_secs(10 * 60),
    };

    /// Print-job content fetch (`jobs::http_get`, Python timeout=60).
    pub const CONTENT: Timeouts = Timeouts {
        connect: Duration::from_secs(60),
        response: Duration::from_secs(60),
        idle: Duration::from_secs(60),
        body: Duration::from_secs(15 * 60),
    };

    /// OTA release manifest (`update::fetch_manifest`, Python timeout=120).
    pub const MANIFEST: Timeouts = Timeouts {
        connect: Duration::from_secs(60),
        response: Duration::from_secs(120),
        idle: Duration::from_secs(120),
        body: Duration::from_secs(10 * 60),
    };

    /// OTA artifact download (Python timeout=300).
    pub const ARTIFACT: Timeouts = Timeouts {
        connect: Duration::from_secs(60),
        response: Duration::from_secs(300),
        idle: Duration::from_secs(300),
        body: Duration::from_secs(30 * 60),
    };

    /// LAN probes (Zebra HTTP identify): short everything.
    pub fn lan(timeout: Duration) -> Timeouts {
        Timeouts {
            connect: timeout,
            response: timeout,
            idle: timeout,
            body: timeout,
        }
    }
}

/// Redirect policy for an agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Redirects {
    /// Let ureq follow up to 10 redirects (it drops `Authorization` on every
    /// hop). The proxy is chosen again for each hop.
    Follow,
    /// Return 3xx responses to the caller (authenticated API calls handle
    /// same-host redirects themselves so the token is never silently dropped).
    Manual,
}

/// Environment lookup, injectable for tests.
pub type Env<'a> = &'a dyn Fn(&str) -> Option<String>;

/// An owned [`Env`], kept by an agent for its per-connection proxy choice.
type SharedEnv = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

fn real_env(key: &str) -> Option<String> {
    std::env::var(key).ok()
}

/// `<name>` from the environment the way urllib reads it: the lowercase
/// variable wins over the uppercase one; empty values count as unset.
fn proxy_env(env: Env, name: &str) -> Option<String> {
    env(&name.to_lowercase())
        .or_else(|| env(&name.to_uppercase()))
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// urllib `proxy_bypass_environment`: `*` bypasses everything; otherwise each
/// comma-separated entry (leading dots ignored) matches the host or any
/// subdomain of it, case-insensitively, compared against the host alone and
/// against `host:port`.
///
/// Like urllib's `req.host`, `host` is written as in the URL (an IPv6 literal
/// keeps its brackets: `[fd00::10]`) and `port` is a port the URL spells out,
/// never the scheme default: `https://h/` does not match an `h:443` entry.
pub fn bypass_proxy(host: &str, port: Option<u16>, no_proxy: &str) -> bool {
    let no_proxy = no_proxy.trim();
    if no_proxy == "*" {
        return true;
    }
    let host = host.to_lowercase();
    let host_port = port.map(|p| format!("{host}:{p}"));
    no_proxy
        .split(',')
        .map(|e| e.trim().trim_start_matches('.').to_lowercase())
        .filter(|e| !e.is_empty())
        .any(|entry| {
            let matches =
                |candidate: &str| candidate == entry || candidate.ends_with(&format!(".{entry}"));
            matches(&host) || host_port.as_deref().is_some_and(matches)
        })
}

/// The proxy URL for a `scheme` URL to `host` (bracketed IPv6) with an
/// explicit `port`, or `None` for a direct connection.
fn proxy_for_host(scheme: &str, host: &str, port: Option<u16>, env: Env) -> Option<String> {
    let var = match scheme.to_ascii_lowercase().as_str() {
        "http" | "ws" => "http_proxy",
        "https" | "wss" => "https_proxy",
        _ => return None,
    };
    let proxy = proxy_env(env, var)?;
    // urllib only consults no_proxy for a non-empty host.
    if !host.is_empty() {
        if let Some(no_proxy) = proxy_env(env, "no_proxy") {
            if bypass_proxy(host, port, &no_proxy) {
                return None;
            }
        }
    }
    Some(proxy)
}

/// The proxy URL urllib would use for `url`, or `None` for a direct connection.
///
/// `http://` uses `http_proxy`, `https://` uses `https_proxy`. WebSockets
/// follow websocket-client (>= 1.6): `ws://` uses `http_proxy` and `wss://`
/// only `https_proxy`. `no_proxy` is checked as by [`bypass_proxy`].
pub fn proxy_url_for(url: &Url, env: Env) -> Option<String> {
    // host_str() keeps IPv6 brackets; port() is only an explicit port.
    proxy_for_host(
        url.scheme(),
        url.host_str().unwrap_or_default(),
        url.port(),
        env,
    )
}

/// A proxy endpoint for tunneling raw TCP (WebSocket) through `CONNECT`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyTarget {
    pub host: String,
    pub port: u16,
    /// Ready-to-send `Proxy-Authorization` value (`Basic …`), from userinfo.
    pub authorization: Option<String>,
}

/// Parse a proxy URL (`http://user:pass@host:port`, scheme optional) the way
/// websocket-client uses one for its tunnel: a plain HTTP `CONNECT`, also for
/// an `https://` URL; port 80 by default; credentials percent-decoded and
/// sent whenever a user name is given. Other schemes return `None`.
pub fn parse_proxy(proxy: &str) -> Option<ProxyTarget> {
    let parts = ProxyParts::split(proxy);
    if !matches!(parts.scheme.as_deref(), None | Some("http" | "https")) {
        return None;
    }
    let (host, port) = parts.host_and_port()?;
    let authorization = parts.user.filter(|u| !u.is_empty()).map(|user| {
        let mut credentials = percent_decode(user);
        if let Some(password) = parts.password.filter(|p| !p.is_empty()) {
            credentials.push(':');
            credentials.push_str(&percent_decode(password));
        }
        basic_auth(&credentials)
    });
    Some(ProxyTarget {
        host,
        port: port.unwrap_or(80),
        authorization,
    })
}

/// A proxy URL split like urllib's `_parse_proxy`; nothing is decoded yet.
struct ProxyParts<'a> {
    /// Lowercase scheme; `None` for a bare `[user:pass@]host[:port]`.
    scheme: Option<String>,
    user: Option<&'a str>,
    password: Option<&'a str>,
    /// `host[:port]`, an IPv6 host in brackets.
    host_port: &'a str,
}

impl<'a> ProxyParts<'a> {
    fn split(proxy: &'a str) -> ProxyParts<'a> {
        let (scheme, authority) = match proxy.split_once("://") {
            Some((scheme, rest)) if !scheme.is_empty() && !scheme.contains('/') => {
                // The authority ends at the first `/` after the userinfo.
                let from = rest.find('@').unwrap_or(0);
                let end = rest[from..].find('/').map_or(rest.len(), |i| from + i);
                (Some(scheme.to_ascii_lowercase()), &rest[..end])
            }
            _ => (None, proxy),
        };
        // The last `@` ends the userinfo; the first `:` in it ends the user.
        let (userinfo, host_port) = match authority.rsplit_once('@') {
            Some((userinfo, host_port)) => (Some(userinfo), host_port),
            None => (None, authority),
        };
        let (user, password) = match userinfo.map(|u| u.split_once(':').ok_or(u)) {
            Some(Ok((user, password))) => (Some(user), Some(password)),
            Some(Err(user)) => (Some(user), None),
            None => (None, None),
        };
        ProxyParts {
            scheme,
            user,
            password,
            host_port,
        }
    }

    /// Host (unbracketed) and explicit port, split like http.client does.
    fn host_and_port(&self) -> Option<(String, Option<u16>)> {
        let host_port = percent_decode(self.host_port);
        let (host, port) = match (host_port.rfind(':'), host_port.rfind(']')) {
            (Some(colon), bracket) if bracket.is_none_or(|b| colon > b) => {
                let port = &host_port[colon + 1..];
                let port = if port.is_empty() {
                    None
                } else {
                    Some(port.parse::<u16>().ok()?)
                };
                (&host_port[..colon], port)
            }
            _ => (host_port.as_str(), None),
        };
        let host = host
            .strip_prefix('[')
            .and_then(|h| h.strip_suffix(']'))
            .unwrap_or(host);
        (!host.is_empty()).then(|| (host.to_string(), port))
    }
}

/// Python's `urllib.parse.unquote`: `%XX` escapes decoded (as UTF-8, invalid
/// sequences replaced); a `+` stays a plus.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while let Some(&b) = bytes.get(i) {
        let escape = bytes
            .get(i + 1..i + 3)
            .filter(|h| b == b'%' && h.iter().all(u8::is_ascii_hexdigit))
            .and_then(|h| u8::from_str_radix(std::str::from_utf8(h).ok()?, 16).ok());
        match escape {
            Some(v) => {
                out.push(v);
                i += 3;
            }
            None => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn basic_auth(credentials: &str) -> String {
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(credentials)
    )
}

/// A proxy URL safe to log: userinfo (`user:pass@`) replaced by `***@`.
pub fn redact_proxy(proxy: &str) -> String {
    let (scheme, rest) = proxy.split_once("://").map_or(("", proxy), |(s, r)| (s, r));
    let authority_end = rest.find('/').unwrap_or(rest.len());
    let redacted = match rest[..authority_end].rfind('@') {
        Some(at) => format!("***@{}", &rest[at + 1..]),
        None => rest.to_string(),
    };
    if scheme.is_empty() {
        redacted
    } else {
        format!("{scheme}://{redacted}")
    }
}

/// The proxy urllib talks to for one request.
struct ProxyEndpoint {
    /// `http://host:port` of the proxy (what the resolver looks up).
    uri: Uri,
    /// `host:port` for messages.
    addr: String,
    /// `Basic …`, only when the URL has both a user and a password.
    authorization: Option<String>,
}

impl ProxyEndpoint {
    /// The proxy at `proxy` (an `http_proxy` / `https_proxy` value) as urllib
    /// uses it for an https (`tunnel`) or http target. Only http and https
    /// proxy URLs (or a bare `host:port`) are usable; both get plain HTTP.
    fn parse(proxy: &str, tunnel: bool) -> Option<ProxyEndpoint> {
        let parts = ProxyParts::split(proxy);
        let https_proxy = match parts.scheme.as_deref() {
            None | Some("http") => false,
            Some("https") => true,
            Some(_) => return None,
        };
        let (host, port) = parts.host_and_port()?;
        // http.client's default port for the connection class urllib uses.
        let port = port.unwrap_or(if tunnel || https_proxy { 443 } else { 80 });
        let addr = if host.contains(':') {
            format!("[{host}]:{port}")
        } else {
            format!("{host}:{port}")
        };
        let uri = format!("http://{addr}/").parse().ok()?;
        let authorization = match (parts.user, parts.password) {
            (Some(user), Some(password)) if !user.is_empty() && !password.is_empty() => {
                Some(basic_auth(&format!(
                    "{}:{}",
                    percent_decode(user),
                    percent_decode(password)
                )))
            }
            _ => None,
        };
        Some(ProxyEndpoint {
            uri,
            addr,
            authorization,
        })
    }

    /// A TCP connection to the proxy, the name looked up on this thread.
    fn connect(&self, details: &ConnectionDetails) -> Result<Box<dyn Transport>, ureq::Error> {
        let addrs = resolve_now(&self.uri, details.config).map_err(|e| self.context(e))?;
        let to_proxy = ConnectionDetails {
            uri: &self.uri,
            addrs,
            config: details.config,
            request_level: details.request_level,
            resolver: details.resolver,
            now: details.now,
            timeout: details.timeout,
            current_time: details.current_time.clone(),
            run_connector: details.run_connector.clone(),
        };
        let tcp = Connector::<()>::connect(&TcpConnector::default(), &to_proxy, None)
            .map_err(|e| self.context(e))?
            .ok_or(ureq::Error::ConnectionFailed)?;
        Ok(Box::new(tcp))
    }

    /// `e` naming this proxy; timeouts stay timeouts.
    fn context(&self, e: ureq::Error) -> ureq::Error {
        match e {
            ureq::Error::Io(io) => ureq::Error::Io(std::io::Error::new(
                io.kind(),
                format!("proxy {}: {io}", self.addr),
            )),
            ureq::Error::HostNotFound => {
                ureq::Error::ConnectProxyFailed(format!("proxy {}: host not found", self.addr))
            }
            other => other,
        }
    }
}

impl fmt::Debug for ProxyEndpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Never the credentials.
        f.debug_struct("ProxyEndpoint")
            .field("addr", &self.addr)
            .finish_non_exhaustive()
    }
}

/// How one connection reaches its target.
#[derive(Debug)]
enum Route {
    Direct,
    /// https target: `CONNECT` through the proxy, then TLS to the target.
    Tunnel(ProxyEndpoint),
    /// http target: absolute-form requests to the proxy (as urllib sends).
    Forward(ProxyEndpoint),
}

/// The per-connection proxy decision, shared by the connector and resolver.
#[derive(Clone)]
struct Router {
    env: SharedEnv,
}

impl Router {
    /// How to reach `uri` under the current environment. An unusable proxy
    /// URL (another scheme, no host) is ignored with a warning when `warn`.
    fn route(&self, uri: &Uri, warn: bool) -> Route {
        let (Some(scheme), Some(host)) = (uri.scheme_str(), uri.host()) else {
            return Route::Direct;
        };
        // host() keeps IPv6 brackets; port_u16() is only an explicit port.
        let Some(proxy) = proxy_for_host(scheme, host, uri.port_u16(), &*self.env) else {
            return Route::Direct;
        };
        let tunnel = scheme.eq_ignore_ascii_case("https");
        match ProxyEndpoint::parse(&proxy, tunnel) {
            Some(endpoint) if tunnel => Route::Tunnel(endpoint),
            Some(endpoint) => Route::Forward(endpoint),
            None => {
                if warn {
                    log::warn!(
                        target: LOG,
                        "ignoring unusable proxy {} (only http:// and https:// proxies); connecting directly",
                        redact_proxy(&proxy)
                    );
                }
                Route::Direct
            }
        }
    }
}

impl fmt::Debug for Router {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Router").finish_non_exhaustive()
    }
}

/// First link of the connector chain: when the target goes through a proxy,
/// connect to the proxy and either open a `CONNECT` tunnel (TLS to the target
/// follows in ureq's TLS connector) or set up absolute-form requests. Direct
/// targets fall through to ureq's TCP connector.
#[derive(Debug)]
struct ProxyConnector {
    router: Router,
    idle: Duration,
}

impl Connector for ProxyConnector {
    type Out = Box<dyn Transport>;

    fn connect(
        &self,
        details: &ConnectionDetails,
        _chained: Option<()>,
    ) -> Result<Option<Self::Out>, ureq::Error> {
        let (proxy, tunnel) = match self.router.route(details.uri, true) {
            Route::Direct => return Ok(None),
            Route::Tunnel(proxy) => (proxy, true),
            Route::Forward(proxy) => (proxy, false),
        };
        log::debug!(target: LOG, "connecting via proxy {}", proxy.addr);
        let mut transport = proxy.connect(details)?;
        if tunnel {
            let timeout = capped(details.timeout, self.idle);
            open_tunnel(&mut *transport, &proxy, details.uri, timeout)?;
            return Ok(Some(transport));
        }
        Ok(Some(Box::new(ForwardTransport {
            inner: transport,
            origin: origin(details.uri),
            authorization: proxy.authorization,
            head_sent: false,
        })))
    }
}

/// Ask the proxy behind `t` for a tunnel to `target` and wait for its 200.
/// Like urllib's tunnel, `Host` and `Proxy-Authorization` (when configured)
/// are the only headers.
fn open_tunnel(
    t: &mut dyn Transport,
    proxy: &ProxyEndpoint,
    target: &Uri,
    timeout: NextTimeout,
) -> Result<(), ureq::Error> {
    // host() keeps IPv6 brackets.
    let authority = format!(
        "{}:{}",
        target.host().unwrap_or_default(),
        target.port_u16().unwrap_or(443)
    );
    let mut head = format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n");
    if let Some(auth) = &proxy.authorization {
        head.push_str(&format!("Proxy-Authorization: {auth}\r\n"));
    }
    head.push_str("\r\n");
    send_all(t, head.as_bytes(), timeout)?;
    let response = read_head(t, timeout).map_err(|e| match e {
        ureq::Error::ConnectProxyFailed(why) => {
            ureq::Error::ConnectProxyFailed(format!("proxy {}: {why}", proxy.addr))
        }
        other => proxy.context(other),
    })?;
    let status_line = response.lines().next().unwrap_or_default();
    if status_line.split_whitespace().nth(1) != Some("200") {
        return Err(ureq::Error::ConnectProxyFailed(format!(
            "proxy {} refused CONNECT {authority}: {status_line}",
            proxy.addr
        )));
    }
    Ok(())
}

/// Read a response head from `t`, consuming exactly its bytes: anything after
/// it stays buffered for the next layer (TLS).
fn read_head(t: &mut dyn Transport, timeout: NextTimeout) -> Result<String, ureq::Error> {
    loop {
        let input = t.buffers().input();
        if let Some(end) = input.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&input[..end]).into_owned();
            t.buffers().input_consume(end + 4);
            return Ok(head);
        }
        if input.len() >= MAX_PROXY_HEAD {
            return Err(ureq::Error::ConnectProxyFailed(
                "response head too large".into(),
            ));
        }
        if !t.await_input(timeout)? {
            return Err(ureq::Error::ConnectProxyFailed(
                "connection closed before the response head".into(),
            ));
        }
    }
}

/// Write all of `data` through `t`'s output buffer.
fn send_all(
    t: &mut dyn Transport,
    mut data: &[u8],
    timeout: NextTimeout,
) -> Result<(), ureq::Error> {
    while !data.is_empty() {
        let output = t.buffers().output();
        let n = output.len().min(data.len());
        output[..n].copy_from_slice(&data[..n]);
        t.transmit_output(n, timeout)?;
        data = &data[n..];
    }
    Ok(())
}

/// `http://host[:port]` of `uri`, without userinfo: the prefix that turns an
/// origin-form request target into absolute form.
fn origin(uri: &Uri) -> String {
    let host = uri.host().unwrap_or_default();
    match uri.port_u16() {
        Some(port) => format!("http://{host}:{port}"),
        None => format!("http://{host}"),
    }
}

/// A connection to an HTTP proxy for one plain-http request. ureq always
/// writes origin-form (`GET /path`); urllib sends the absolute form (`GET
/// http://host/path`) plus `Proxy-Authorization` to the proxy, so the request
/// line is rewritten on the way out. Single use: never returned to the pool,
/// so every request on it is the first one.
struct ForwardTransport {
    inner: Box<dyn Transport>,
    origin: String,
    authorization: Option<String>,
    head_sent: bool,
}

impl ForwardTransport {
    /// The request head ureq wrote, with an absolute-form target and the
    /// proxy credentials.
    fn rewrite(&self, head: &[u8]) -> Result<Vec<u8>, ureq::Error> {
        let bad = || ureq::Error::Io(std::io::Error::other("unexpected request line"));
        let end = head.windows(2).position(|w| w == b"\r\n").ok_or_else(bad)?;
        let line = std::str::from_utf8(&head[..end]).map_err(|_| bad())?;
        let mut parts = line.splitn(3, ' ');
        let (Some(method), Some(target), Some(version)) =
            (parts.next(), parts.next(), parts.next())
        else {
            return Err(bad());
        };
        let mut out = if target.starts_with('/') {
            format!("{method} {}{target} {version}\r\n", self.origin)
        } else {
            format!("{line}\r\n")
        };
        if let Some(auth) = &self.authorization {
            out.push_str(&format!("Proxy-Authorization: {auth}\r\n"));
        }
        let mut out = out.into_bytes();
        out.extend_from_slice(&head[end + 2..]);
        Ok(out)
    }
}

impl Transport for ForwardTransport {
    fn buffers(&mut self) -> &mut dyn Buffers {
        self.inner.buffers()
    }

    fn transmit_output(&mut self, amount: usize, timeout: NextTimeout) -> Result<(), ureq::Error> {
        if self.head_sent || amount == 0 {
            return self.inner.transmit_output(amount, timeout);
        }
        // The first bytes ureq sends on a connection start with the request line.
        let head = self.inner.buffers().output()[..amount].to_vec();
        let rewritten = self.rewrite(&head)?;
        self.head_sent = true;
        send_all(&mut *self.inner, &rewritten, timeout)
    }

    fn await_input(&mut self, timeout: NextTimeout) -> Result<bool, ureq::Error> {
        self.inner.await_input(timeout)
    }

    fn is_open(&mut self) -> bool {
        !self.head_sent && self.inner.is_open()
    }
}

impl fmt::Debug for ForwardTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Never the credentials.
        f.debug_struct("ForwardTransport")
            .field("inner", &self.inner)
            .field("origin", &self.origin)
            .finish_non_exhaustive()
    }
}

/// `timeout`, but at most `idle`.
fn capped(timeout: NextTimeout, idle: Duration) -> NextTimeout {
    NextTimeout {
        after: timeout.after.min(Deadline::Exact(idle)),
        reason: timeout.reason,
    }
}

/// Last link of the connector chain: wraps every connection in
/// [`IdleTransport`].
#[derive(Debug)]
struct IdleConnector {
    idle: Duration,
}

impl<In: Transport> Connector<In> for IdleConnector {
    type Out = IdleTransport<In>;

    fn connect(
        &self,
        _details: &ConnectionDetails,
        chained: Option<In>,
    ) -> Result<Option<Self::Out>, ureq::Error> {
        Ok(chained.map(|inner| IdleTransport {
            inner,
            idle: self.idle,
        }))
    }
}

/// urllib's per-operation timeout: every read and write waits at most `idle`
/// (ureq's own deadline for the phase still applies when it is sooner). The
/// cap reaches the socket even under TLS: rustls reads through the same
/// timeout. A stalled body therefore fails after `idle`, while a slow body
/// that keeps arriving runs until the body budget.
#[derive(Debug)]
struct IdleTransport<T> {
    inner: T,
    idle: Duration,
}

impl<T: Transport> Transport for IdleTransport<T> {
    fn buffers(&mut self) -> &mut dyn Buffers {
        self.inner.buffers()
    }

    fn transmit_output(&mut self, amount: usize, timeout: NextTimeout) -> Result<(), ureq::Error> {
        self.inner
            .transmit_output(amount, capped(timeout, self.idle))
    }

    fn await_input(&mut self, timeout: NextTimeout) -> Result<bool, ureq::Error> {
        self.inner.await_input(capped(timeout, self.idle))
    }

    fn is_open(&mut self) -> bool {
        self.inner.is_open()
    }

    fn is_tls(&self) -> bool {
        self.inner.is_tls()
    }
}

/// Looks names up on the calling thread, always: ureq's resolver starts a
/// thread per lookup when given a deadline, and the agent loop must not
/// depend on being able to start one. A target reached through a proxy is not
/// looked up at all; the proxy resolves it, as with urllib.
#[derive(Debug)]
struct SyncResolver {
    router: Router,
}

impl Resolver for SyncResolver {
    fn resolve(
        &self,
        uri: &Uri,
        config: &Config,
        _timeout: NextTimeout,
    ) -> Result<ResolvedSocketAddrs, ureq::Error> {
        if matches!(self.router.route(uri, false), Route::Direct) {
            return resolve_now(uri, config);
        }
        // Never dialled: ProxyConnector connects to the proxy instead.
        let mut addrs = self.empty();
        addrs.push(SocketAddr::from(([0, 0, 0, 0], 0)));
        Ok(addrs)
    }
}

/// ureq's resolver without a deadline: it then calls getaddrinfo on this
/// thread instead of starting one.
fn resolve_now(uri: &Uri, config: &Config) -> Result<ResolvedSocketAddrs, ureq::Error> {
    let no_deadline = NextTimeout {
        after: Deadline::NotHappening,
        reason: ureq::Timeout::Resolve,
    };
    DefaultResolver::default().resolve(uri, config, no_deadline)
}

/// A ureq agent with the given timeouts and redirect policy. Status codes are
/// returned, not raised. The proxy is chosen per connection from the
/// environment, so `_url` (once used to pick it) no longer matters.
pub fn agent(_url: &str, timeouts: Timeouts, redirects: Redirects) -> ureq::Agent {
    agent_with_env(timeouts, redirects, Arc::new(real_env))
}

/// [`agent`] reading the proxy variables through `env`.
fn agent_with_env(timeouts: Timeouts, redirects: Redirects, env: SharedEnv) -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .user_agent("vesyl-print-agent")
        // OS trust store, like Python urllib (sites may add a TLS-inspection CA).
        .tls_config(
            TlsConfig::builder()
                .root_certs(RootCerts::PlatformVerifier)
                .build(),
        )
        // ProxyConnector does the proxying; ureq's default reads ALL_PROXY etc.
        .proxy(None)
        .timeout_global(None)
        .timeout_per_call(None)
        .timeout_resolve(None)
        .timeout_connect(Some(timeouts.connect))
        .timeout_send_request(Some(timeouts.connect))
        .timeout_send_body(Some(timeouts.connect))
        .timeout_recv_response(Some(timeouts.response))
        .timeout_recv_body(Some(timeouts.body))
        .max_redirects(match redirects {
            Redirects::Follow => MAX_REDIRECTS,
            Redirects::Manual => 0,
        })
        .build();
    let router = Router { env };
    let connector = ProxyConnector {
        router: router.clone(),
        idle: timeouts.idle,
    }
    .chain(TcpConnector::default())
    .chain(RustlsConnector::default())
    .chain(IdleConnector {
        idle: timeouts.idle,
    });
    ureq::Agent::with_parts(config, connector, SyncResolver { router })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cloud::http_stub::{self, respond};
    use std::collections::HashMap;
    use std::io::{Read, Write};
    use std::sync::Mutex;
    use std::time::Instant;

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + Send + Sync + 'static {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| map.get(k).cloned()
    }

    fn proxy(url: &str, pairs: &[(&str, &str)]) -> Option<String> {
        proxy_url_for(&Url::parse(url).unwrap(), &env_of(pairs))
    }

    fn agent_env(timeouts: Timeouts, redirects: Redirects, pairs: &[(&str, &str)]) -> ureq::Agent {
        agent_with_env(timeouts, redirects, Arc::new(env_of(pairs)))
    }

    fn quick() -> Timeouts {
        Timeouts::lan(Duration::from_secs(5))
    }

    fn get(agent: &ureq::Agent, url: &str) -> Result<(u16, String), ureq::Error> {
        let mut resp = agent.get(url).call()?;
        let status = resp.status().as_u16();
        Ok((status, resp.body_mut().read_to_string()?))
    }

    fn decode_basic(value: &str) -> String {
        let token = value.strip_prefix("Basic ").expect("Basic credentials");
        String::from_utf8(
            base64::engine::general_purpose::STANDARD
                .decode(token)
                .unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn scheme_specific_like_urllib() {
        let env = [
            ("HTTP_PROXY", "http://p:3128"),
            ("ALL_PROXY", "http://all:1"),
        ];
        assert_eq!(proxy("http://x/", &env).as_deref(), Some("http://p:3128"));
        // https does not fall back to http_proxy, and ALL_PROXY is ignored.
        assert_eq!(proxy("https://x/", &env), None);
        assert_eq!(
            proxy("https://x/", &[("HTTPS_PROXY", "http://s:8080")]).as_deref(),
            Some("http://s:8080")
        );
    }

    #[test]
    fn lowercase_wins() {
        let env = [
            ("https_proxy", "http://lower:1"),
            ("HTTPS_PROXY", "http://upper:2"),
        ];
        assert_eq!(proxy("https://x/", &env).as_deref(), Some("http://lower:1"));
    }

    /// N16: websocket-client >= 1.6 reads only https_proxy for wss.
    #[test]
    fn websocket_schemes_follow_websocket_client() {
        assert_eq!(proxy("wss://x/", &[("http_proxy", "http://p:1")]), None);
        assert_eq!(proxy("wss://x/", &[("HTTP_PROXY", "http://p:1")]), None);
        assert_eq!(
            proxy(
                "wss://x/",
                &[("http_proxy", "http://p:1"), ("https_proxy", "http://s:2")]
            )
            .as_deref(),
            Some("http://s:2")
        );
        assert_eq!(
            proxy("ws://x/", &[("http_proxy", "http://p:1")]).as_deref(),
            Some("http://p:1")
        );
        assert_eq!(proxy("ws://x/", &[("https_proxy", "http://s:2")]), None);
    }

    /// N18: expectations from CPython's `proxy_bypass_environment(req.host)`.
    #[test]
    fn no_proxy_rules_match_urllib() {
        let python = "localhost, anotherdomain.com, newdomain.com:1234, .d.o.t";
        let cases: &[(&str, Option<u16>, &str, bool)] = &[
            ("[fd00::10]", Some(3000), "[fd00::10]", true),
            ("[fd00::10]", Some(3000), "[fd00::10]:3000", true),
            ("[fd00::10]", Some(3000), "fd00::10", false),
            ("[fd00::10]", Some(3000), "[fd00::10]:4000", false),
            ("[fd00::10]", None, "[fd00::10]", true),
            ("[fd00::10]", None, "fd00::10", false),
            ("[::1]", Some(8080), "[::1]", true),
            ("[::1]", Some(8080), "::1", false),
            ("h", None, "h:443", false),
            ("h", Some(443), "h:443", true),
            ("h", Some(8443), "h", true),
            ("10.0.0.5", Some(3600), "10.0.0.5:3600", true),
            ("10.0.0.5", None, "10.0.0.5:3600", false),
            ("wms-api.vesyl.dev", None, " .vesyl.dev , other", true),
            ("notvesyl.dev", None, "vesyl.dev", false),
            ("WMS-API.vesyl.dev", None, "VESYL.DEV", true),
            ("newdomain.com", Some(1234), python, true),
            ("www.newdomain.com", Some(1234), python, true),
            ("newdomain.com", None, python, false),
            ("newdomain.com", Some(1235), python, false),
            ("anotherdomain.com", Some(8888), python, true),
            ("foo.d.o.t", None, python, true),
            ("prelocalhost", None, python, false),
            ("newdomain.com", None, "*, anotherdomain.com", false),
            ("anotherdomain.com", None, "*, anotherdomain.com", true),
            ("anything", Some(1), "*", true),
            ("x", None, "", false),
        ];
        for &(host, port, no_proxy, want) in cases {
            assert_eq!(
                bypass_proxy(host, port, no_proxy),
                want,
                "{host} {port:?} vs {no_proxy:?}"
            );
        }

        // Through the URL: brackets kept, only an explicit port counts.
        let v6 = |no_proxy| {
            proxy(
                "http://[fd00::10]:3000/x",
                &[("http_proxy", "http://p:1"), ("no_proxy", no_proxy)],
            )
        };
        assert_eq!(v6("[fd00::10]"), None);
        assert_eq!(v6("[fd00::10]:3000"), None);
        assert!(v6("fd00::10").is_some());
        let https = [("https_proxy", "http://p:1"), ("no_proxy", "h:443")];
        assert!(proxy("https://h/x", &https).is_some());
        let env = [("https_proxy", "http://p:1"), ("no_proxy", "vesyl.dev")];
        assert_eq!(proxy("https://wms-api.vesyl.dev:3600/print", &env), None);
        assert!(proxy("https://example.com/", &env).is_some());

        // The per-connection route reads the raw authority: an explicit
        // default port still counts, as in urllib's req.host.
        let router = Router {
            env: Arc::new(env_of(&[
                ("https_proxy", "http://p:1"),
                ("no_proxy", "h:443,[fd00::10]"),
            ])),
        };
        let route = |uri: &str| router.route(&uri.parse().unwrap(), false);
        assert!(matches!(route("https://h:443/x"), Route::Direct));
        assert!(matches!(route("https://h/x"), Route::Tunnel(_)));
        assert!(matches!(route("https://[fd00::10]:8443/x"), Route::Direct));
        assert!(matches!(route("https://[fd00::11]/x"), Route::Tunnel(_)));
    }

    #[test]
    fn parses_proxy_targets() {
        assert_eq!(
            parse_proxy("http://proxy.lan:3128"),
            Some(ProxyTarget {
                host: "proxy.lan".into(),
                port: 3128,
                authorization: None
            })
        );
        assert_eq!(parse_proxy("proxy.lan").unwrap().port, 80);
        let auth = |p: &str| decode_basic(&parse_proxy(p).unwrap().authorization.unwrap());
        assert_eq!(auth("http://us%40er:p%3Ass@p:8080"), "us@er:p:ss");
        // unquote, not form decoding: a plus stays a plus.
        assert_eq!(auth("http://u:p+w%2Bx@p:8080"), "u:p+w+x");
        // websocket-client: the user alone when there is no password.
        assert_eq!(auth("http://user:@p:1"), "user");
        assert_eq!(auth("http://u:p@ss@p:1/x"), "u:p@ss");
        assert_eq!(parse_proxy("http://:pw@p:1").unwrap().authorization, None);
        assert_eq!(parse_proxy("socks5://p:1080"), None);
        // N20: an https:// proxy URL is a plain HTTP proxy (websocket-client),
        // and an explicit port is kept even when it is a scheme default.
        let https = parse_proxy("https://u%40x:p@proxy.lan:443").unwrap();
        assert_eq!((https.host.as_str(), https.port), ("proxy.lan", 443));
        assert_eq!(decode_basic(&https.authorization.unwrap()), "u@x:p");
        assert_eq!(parse_proxy("https://proxy.lan").unwrap().port, 80);
        let v6 = parse_proxy("http://[fd00::1]:3128").unwrap();
        assert_eq!((v6.host.as_str(), v6.port), ("fd00::1", 3128));
        assert_eq!(parse_proxy("http://"), None);
        assert_eq!(parse_proxy("http://p:notaport"), None);
    }

    /// N20: urllib's `_parse_proxy` + `proxy_open`: credentials decoded and
    /// sent only with both parts; http.client's default ports.
    #[test]
    fn proxy_endpoints_follow_urllib() {
        let endpoint = |p: &str, tunnel: bool| ProxyEndpoint::parse(p, tunnel).unwrap();
        let auth = |p: &str| endpoint(p, true).authorization.map(|a| decode_basic(&a));
        assert_eq!(
            auth("http://dev%40corp:p%3As%25s@proxy:3128").as_deref(),
            Some("dev@corp:p:s%s")
        );
        assert_eq!(auth("http://u:p@ss@p:1/x").as_deref(), Some("u:p@ss"));
        assert_eq!(auth("https://u:p+w@proxy").as_deref(), Some("u:p+w"));
        assert_eq!(auth("http://user@p:1"), None);
        assert_eq!(auth("http://user:@p:1"), None);
        assert_eq!(auth("http://:pw@p:1"), None);

        assert_eq!(endpoint("http://proxy:3128", true).addr, "proxy:3128");
        assert_eq!(endpoint("proxy.lan:3128", false).addr, "proxy.lan:3128");
        // http.client: HTTPSConnection (tunnel) defaults to 443, HTTP to 80.
        assert_eq!(endpoint("http://proxy", true).addr, "proxy:443");
        assert_eq!(endpoint("http://proxy", false).addr, "proxy:80");
        assert_eq!(endpoint("https://proxy", false).addr, "proxy:443");
        assert_eq!(endpoint("http://proxy:80", true).addr, "proxy:80");
        assert_eq!(
            endpoint("http://[fd00::1]:3128", true).addr,
            "[fd00::1]:3128"
        );
        assert!(ProxyEndpoint::parse("socks5://p:1080", true).is_none());
        assert!(ProxyEndpoint::parse("http://", true).is_none());
        // The endpoint never shows its credentials.
        assert!(!format!("{:?}", endpoint("http://u:secret@p:1", true)).contains("secret"));
    }

    #[test]
    fn redacts_proxy_credentials() {
        assert_eq!(
            redact_proxy("http://user:secret@proxy:3128"),
            "http://***@proxy:3128"
        );
        assert_eq!(redact_proxy("user:secret@proxy:3128/x"), "***@proxy:3128/x");
        assert_eq!(redact_proxy("http://proxy:3128"), "http://proxy:3128");
        assert!(!redact_proxy("http://u:p@ss@proxy:1").contains("p@ss"));
    }

    #[test]
    fn unusable_proxies_connect_directly() {
        let target = http_stub::serve(|_, s| respond(s, 200, &[], b"DIRECT"));
        for proxy in ["socks5://127.0.0.1:1", "http://"] {
            let agent = agent_env(quick(), Redirects::Follow, &[("http_proxy", proxy)]);
            assert_eq!(
                get(&agent, &target.base_url).unwrap(),
                (200, "DIRECT".into()),
                "{proxy}"
            );
        }
    }

    /// Headers, then part of the body, then silence (a path that died).
    fn stalled_server() -> http_stub::Stub {
        http_stub::serve(|_, s| {
            let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n{\"jobs\":[");
            let _ = s.flush();
            std::thread::sleep(Duration::from_secs(20));
        })
    }

    fn idle_timeouts(idle: Duration) -> Timeouts {
        Timeouts {
            connect: Duration::from_secs(5),
            response: Duration::from_secs(5),
            idle,
            body: Duration::from_secs(60),
        }
    }

    /// N15: a body that stops arriving fails after the idle timeout, not the
    /// (much longer) body budget, as with urllib's per-operation timeout.
    #[test]
    fn stalled_body_fails_after_the_idle_timeout() {
        let srv = stalled_server();
        let agent = agent_env(
            idle_timeouts(Duration::from_secs(1)),
            Redirects::Follow,
            &[],
        );
        let started = Instant::now();
        let err = get(&agent, &srv.base_url).unwrap_err();
        let took = started.elapsed();
        assert!(
            matches!(err, ureq::Error::Timeout(ureq::Timeout::RecvBody)),
            "{err:?}"
        );
        assert!(took >= Duration::from_millis(900), "{took:?}");
        assert!(took < Duration::from_secs(5), "{took:?}");
    }

    /// N15: the idle timeout is per read: a body that keeps arriving, with
    /// gaps shorter than it, outlives it.
    #[test]
    fn steady_trickle_outlives_the_idle_timeout() {
        let body: String = "x".repeat(40);
        let served = body.clone();
        let srv = http_stub::serve(move |_, s| {
            http_stub::trickle(s, served.as_bytes(), 10, Duration::from_millis(300))
        });
        let agent = agent_env(
            idle_timeouts(Duration::from_secs(1)),
            Redirects::Follow,
            &[],
        );
        let started = Instant::now();
        assert_eq!(get(&agent, &srv.base_url).unwrap(), (200, body));
        assert!(started.elapsed() >= Duration::from_secs(2));
    }

    /// The response head is bounded by the idle timeout too.
    #[test]
    fn silent_server_fails_after_the_idle_timeout() {
        let srv = http_stub::serve(|_, _| std::thread::sleep(Duration::from_secs(20)));
        let agent = agent_env(
            idle_timeouts(Duration::from_secs(1)),
            Redirects::Follow,
            &[],
        );
        let started = Instant::now();
        let err = get(&agent, &srv.base_url).unwrap_err();
        assert!(matches!(err, ureq::Error::Timeout(_)), "{err:?}");
        assert!(started.elapsed() < Duration::from_secs(4));
    }

    fn addr_of(stub: &http_stub::Stub) -> String {
        stub.base_url.trim_start_matches("http://").to_string()
    }

    /// N20(a): an http target goes to the proxy in absolute form with decoded
    /// credentials, as urllib sends it (no CONNECT, which squid refuses for
    /// port 80 by default).
    #[test]
    fn http_targets_use_absolute_form() {
        let proxy = http_stub::serve(|_, s| respond(s, 200, &[], b"LABEL"));
        let proxy_url = format!("http://dev%40corp:p%3As%25s@{}", addr_of(&proxy));
        let agent = agent_env(quick(), Redirects::Follow, &[("http_proxy", &proxy_url)]);
        assert_eq!(
            get(&agent, "http://labels.example.test/l.zpl?x=1").unwrap(),
            (200, "LABEL".into())
        );
        assert_eq!(
            get(&agent, "http://labels.example.test:8080/a").unwrap(),
            (200, "LABEL".into())
        );
        let seen = proxy.requests();
        assert_eq!(seen.len(), 2, "one connection per request");
        assert_eq!(seen[0].method, "GET");
        assert_eq!(seen[0].path, "http://labels.example.test/l.zpl?x=1");
        assert_eq!(seen[0].header("Host"), Some("labels.example.test"));
        assert_eq!(
            decode_basic(seen[0].header("Proxy-Authorization").unwrap()),
            "dev@corp:p:s%s"
        );
        assert_eq!(seen[0].header("User-Agent"), Some("vesyl-print-agent"));
        assert_eq!(seen[1].path, "http://labels.example.test:8080/a");
        assert_eq!(seen[1].header("Host"), Some("labels.example.test:8080"));
    }

    /// A CONNECT proxy that records the request, answers `status` and then
    /// records the first byte sent through the tunnel.
    fn connect_proxy(status: u16) -> (http_stub::Stub, Arc<Mutex<Vec<u8>>>) {
        let tunnelled = Arc::new(Mutex::new(Vec::new()));
        let seen = tunnelled.clone();
        let stub = http_stub::serve(move |req, s| {
            if req.method != "CONNECT" {
                return respond(s, 400, &[], b"");
            }
            let reason = if status == 200 {
                "Connection established"
            } else {
                "Denied"
            };
            let _ = write!(s, "HTTP/1.1 {status} {reason}\r\n\r\n");
            let _ = s.flush();
            if status == 200 {
                let _ = s.set_read_timeout(Some(Duration::from_secs(5)));
                let mut first = [0u8; 1];
                if s.read_exact(&mut first).is_ok() {
                    seen.lock().unwrap().push(first[0]);
                }
            }
        });
        (stub, tunnelled)
    }

    /// N20(b, c): https targets tunnel with a plain CONNECT carrying decoded
    /// credentials, also for an https:// proxy URL, and TLS to the target
    /// then starts inside the tunnel.
    #[test]
    fn https_targets_tunnel_with_decoded_credentials() {
        let (proxy, tunnelled) = connect_proxy(200);
        for scheme in ["http", "https"] {
            let proxy_url = format!("{scheme}://dev%40corp:p%3As%25s@{}", addr_of(&proxy));
            let agent = agent_env(quick(), Redirects::Follow, &[("https_proxy", &proxy_url)]);
            // The stub closes the tunnel after the ClientHello.
            assert!(get(&agent, "https://wms-api.example.test/print/v1/whoami").is_err());
        }
        let seen = proxy.requests();
        assert_eq!(seen.len(), 2);
        for req in &seen {
            assert_eq!(req.method, "CONNECT");
            assert_eq!(req.path, "wms-api.example.test:443");
            assert_eq!(req.header("Host"), Some("wms-api.example.test:443"));
            assert_eq!(
                decode_basic(req.header("Proxy-Authorization").unwrap()),
                "dev@corp:p:s%s"
            );
        }
        // 0x16: a TLS handshake record, sent through the tunnel.
        assert_eq!(*tunnelled.lock().unwrap(), vec![0x16, 0x16]);
    }

    #[test]
    fn refused_connect_is_an_error() {
        let (proxy, _) = connect_proxy(403);
        let proxy_url = format!("http://{}", addr_of(&proxy));
        let agent = agent_env(quick(), Redirects::Follow, &[("https_proxy", &proxy_url)]);
        let err = get(&agent, "https://wms-api.example.test/").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("refused CONNECT wms-api.example.test:443"),
            "{msg}"
        );
        assert!(msg.contains("403"), "{msg}");
        // No credentials configured: none sent.
        assert_eq!(proxy.requests()[0].header("Proxy-Authorization"), None);

        let agent = agent_env(
            quick(),
            Redirects::Follow,
            &[("https_proxy", "http://127.0.0.1:9")],
        );
        let msg = get(&agent, "https://wms-api.example.test/")
            .unwrap_err()
            .to_string();
        assert!(msg.contains("proxy 127.0.0.1:9"), "{msg}");
    }

    /// N17: like urllib, every redirect hop gets its own proxy decision, both
    /// when the first hop is bypassed and when it is proxied.
    #[test]
    fn redirect_hops_choose_their_own_proxy() {
        // localhost is in no_proxy; 127.0.0.1 is not.
        let target = http_stub::serve(|req, s| match req.path.as_str() {
            "/a" => {
                let to = format!("http://127.0.0.1:{}/b", req.port());
                respond(s, 302, &[("Location", &to)], b"")
            }
            _ => respond(s, 200, &[], b"DIRECT"),
        });
        let proxy = http_stub::serve(|req, s| {
            if req.path.ends_with("/a") {
                // Proxied first hop: redirect to a bypassed host.
                let port = req.path.split(':').nth(2).unwrap().trim_end_matches("/a");
                let to = format!("http://localhost:{port}/b");
                respond(s, 302, &[("Location", &to)], b"")
            } else {
                respond(s, 200, &[], b"VIA-PROXY")
            }
        });
        let proxy_url = format!("http://{}", addr_of(&proxy));
        let env = [
            ("http_proxy", proxy_url.as_str()),
            ("no_proxy", "localhost"),
        ];
        let agent = agent_env(quick(), Redirects::Follow, &env);
        let port = target.base_url.rsplit(':').next().unwrap().to_string();

        // Bypassed first hop, proxied second hop.
        let body = get(&agent, &format!("http://localhost:{port}/a")).unwrap();
        assert_eq!(body, (200, "VIA-PROXY".into()));
        let via = proxy.requests();
        assert_eq!(via.len(), 1);
        assert_eq!(via[0].path, format!("http://127.0.0.1:{port}/b"));
        assert_eq!(target.requests().len(), 1);

        // Proxied first hop, bypassed second hop.
        let body = get(&agent, &format!("http://127.0.0.1:{port}/a")).unwrap();
        assert_eq!(body, (200, "DIRECT".into()));
        let via = proxy.requests();
        assert_eq!(via.len(), 2);
        assert_eq!(via[1].path, format!("http://127.0.0.1:{port}/a"));
        let direct = target.requests();
        assert_eq!(direct.len(), 2);
        assert_eq!(direct[1].path, "/b");
    }

    /// N17: a redirect that changes the scheme switches to that scheme's
    /// proxy variable.
    #[test]
    fn redirect_to_https_uses_https_proxy() {
        let (tunnel, _) = connect_proxy(403);
        let plain = http_stub::serve(|_, s| respond(s, 500, &[], b""));
        let target = http_stub::serve(|_, s| {
            respond(
                s,
                302,
                &[("Location", "https://cdn.example.test/a.tar.gz")],
                b"",
            )
        });
        let env = [
            ("http_proxy", format!("http://{}", addr_of(&plain))),
            ("https_proxy", format!("http://{}", addr_of(&tunnel))),
            ("no_proxy", "127.0.0.1".to_string()),
        ];
        let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let agent = agent_env(quick(), Redirects::Follow, &env);
        let msg = get(&agent, &target.base_url).unwrap_err().to_string();
        assert!(
            msg.contains("refused CONNECT cdn.example.test:443"),
            "{msg}"
        );
        assert_eq!(plain.requests().len(), 0);
        assert_eq!(tunnel.requests()[0].path, "cdn.example.test:443");
    }

    #[test]
    fn manual_redirects_are_returned() {
        let srv = http_stub::serve(|_, s| respond(s, 302, &[("Location", "/b")], b""));
        let agent = agent_env(quick(), Redirects::Manual, &[]);
        assert_eq!(get(&agent, &srv.base_url).unwrap().0, 302);
        assert_eq!(srv.requests().len(), 1);
    }

    const NO_THREADS_CHILD: &str = "VESYL_TEST_NET_NO_THREADS";

    /// N19: proxy and target names are resolved on the calling thread. The
    /// requests run in a child process that may not start any thread
    /// (RLIMIT_NPROC=1), where ureq's resolver thread used to panic.
    #[test]
    fn requests_start_no_threads() {
        let target = http_stub::serve(|_, s| respond(s, 200, &[], b"DIRECT"));
        let forward = http_stub::serve(|_, s| respond(s, 200, &[], b"VIA-PROXY"));
        let (tunnel, _) = connect_proxy(200);
        let port = |stub: &http_stub::Stub| stub.base_url.rsplit(':').next().unwrap().to_string();
        let spec = format!("{} {} {}", port(&target), port(&forward), port(&tunnel));
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "net::tests::no_threads_child",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(NO_THREADS_CHILD, spec)
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success() && stdout.contains("1 passed"),
            "child failed: {stdout}{}",
            String::from_utf8_lossy(&out.stderr)
        );
        if stdout.contains("thread limit not enforced") {
            eprintln!("note: thread limit not enforced; no-thread check was vacuous");
        }
        assert_eq!(target.requests().len(), 1);
        assert_eq!(forward.requests()[0].path, "http://example.invalid/x");
        assert_eq!(tunnel.requests()[0].path, "example.invalid:443");
    }

    #[test]
    #[ignore = "child process of requests_start_no_threads"]
    fn no_threads_child() {
        let Ok(spec) = std::env::var(NO_THREADS_CHILD) else {
            return;
        };
        let ports: Vec<&str> = spec.split(' ').collect();
        let limit = libc::rlimit {
            rlim_cur: 1,
            rlim_max: 1,
        };
        // SAFETY: plain syscall on a valid struct.
        assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NPROC, &limit) }, 0);
        // From here on any thread start fails (ureq's resolver thread panics),
        // except for real root, which RLIMIT_NPROC does not bind.
        if std::thread::Builder::new().spawn(|| ()).is_ok() {
            println!("thread limit not enforced (running as root?)");
        }
        let direct = agent_env(quick(), Redirects::Follow, &[]);
        let result = get(&direct, &format!("http://localhost:{}/", ports[0]));
        assert_eq!(result.unwrap(), (200, "DIRECT".into()));

        // A proxy name (looked up) and an IP literal; the target is never
        // looked up locally (example.invalid would not resolve).
        let forward = format!("http://localhost:{}", ports[1]);
        let tunnel = format!("http://127.0.0.1:{}", ports[2]);
        let env = [
            ("http_proxy", forward.as_str()),
            ("https_proxy", tunnel.as_str()),
        ];
        let proxied = agent_env(quick(), Redirects::Follow, &env);
        let result = get(&proxied, "http://example.invalid/x");
        assert_eq!(result.unwrap(), (200, "VIA-PROXY".into()));
        // The tunnel opens, then TLS fails against the stub: an error, no panic.
        assert!(get(&proxied, "https://example.invalid/").is_err());
    }
}
