//! Shared HTTP plumbing: ureq agents configured like Python's urllib.
//!
//! - **Timeouts per phase.** urllib's `timeout=N` bounds each socket operation,
//!   so a slow but steady download never times out. ureq has no per-read idle
//!   timeout, so connect / response use the Python values and the body gets a
//!   generous total budget instead of one end-to-end deadline.
//! - **Proxies like urllib.** `<scheme>_proxy` (lowercase wins over uppercase),
//!   `no_proxy` suffix matching, `ALL_PROXY` ignored. ureq's own env handling
//!   differs (it prefers `ALL_PROXY` and uses `HTTP_PROXY` for https).
//! - **No resolver threads.** With no resolve deadline ureq resolves DNS
//!   synchronously instead of spawning a thread per request.
//! - **No transparent gzip** (the `gzip` feature is off): urllib never sends
//!   `Accept-Encoding: gzip`, and a server that gzips a `.tar.gz` would break
//!   the artifact checksum.

use std::time::Duration;

use base64::Engine as _;
use ureq::tls::{RootCerts, TlsConfig};
use url::Url;

/// Per-phase timeouts for one kind of request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timeouts {
    /// TCP connect and sending the request (Python's per-operation timeout).
    pub connect: Duration,
    /// Waiting for the response head (Python's per-operation timeout).
    pub response: Duration,
    /// Total budget for reading the body (ureq cannot do a per-read idle timeout).
    pub body: Duration,
}

impl Timeouts {
    /// wms-api REST calls (`CloudClient`, Python timeout=30). The body budget
    /// covers a large jobs/pending payload on a slow link.
    pub const API: Timeouts = Timeouts {
        connect: Duration::from_secs(30),
        response: Duration::from_secs(30),
        body: Duration::from_secs(10 * 60),
    };

    /// Print-job content fetch (`jobs::http_get`, Python timeout=60).
    pub const CONTENT: Timeouts = Timeouts {
        connect: Duration::from_secs(60),
        response: Duration::from_secs(60),
        body: Duration::from_secs(15 * 60),
    };

    /// OTA manifest and artifact download (Python timeout=120 / 300).
    pub const ARTIFACT: Timeouts = Timeouts {
        connect: Duration::from_secs(60),
        response: Duration::from_secs(300),
        body: Duration::from_secs(30 * 60),
    };

    /// LAN probes (Zebra HTTP identify): short everything.
    pub fn lan(timeout: Duration) -> Timeouts {
        Timeouts {
            connect: timeout,
            response: timeout,
            body: timeout,
        }
    }
}

/// Redirect policy for an agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Redirects {
    /// Let ureq follow redirects (it drops `Authorization` on every hop).
    Follow,
    /// Return 3xx responses to the caller (authenticated API calls handle
    /// same-host redirects themselves so the token is never silently dropped).
    Manual,
}

/// Environment lookup, injectable for tests.
pub type Env<'a> = &'a dyn Fn(&str) -> Option<String>;

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
/// subdomain of it, compared case-insensitively against `host` and `host:port`.
pub fn bypass_proxy(host: &str, port: Option<u16>, no_proxy: &str) -> bool {
    let no_proxy = no_proxy.trim();
    if no_proxy == "*" {
        return true;
    }
    let host = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_lowercase();
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

/// The proxy URL urllib would use for `url`, or `None` for a direct connection.
///
/// `http://` uses `http_proxy`, `https://` uses `https_proxy`. For WebSockets
/// (`ws://` / `wss://`) this follows websocket-client: `wss` tries
/// `https_proxy` then `http_proxy`, `ws` uses `http_proxy`.
pub fn proxy_url_for(url: &Url, env: Env) -> Option<String> {
    let names: &[&str] = match url.scheme() {
        "http" => &["http_proxy"],
        "https" => &["https_proxy"],
        "ws" => &["http_proxy"],
        "wss" => &["https_proxy", "http_proxy"],
        _ => return None,
    };
    let proxy = names.iter().find_map(|n| proxy_env(env, n))?;
    if let Some(no_proxy) = proxy_env(env, "no_proxy") {
        let host = url.host_str().unwrap_or_default();
        if bypass_proxy(host, url.port_or_known_default(), &no_proxy) {
            return None;
        }
    }
    Some(proxy)
}

/// A proxy endpoint for tunneling raw TCP (WebSocket) through `CONNECT`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyTarget {
    pub host: String,
    pub port: u16,
    /// Ready-to-send `Proxy-Authorization` value (`Basic …`), from userinfo.
    pub authorization: Option<String>,
}

/// Parse a proxy URL (`http://user:pass@host:port`, scheme optional).
/// Only HTTP proxies are supported for tunneling; other schemes return `None`.
pub fn parse_proxy(proxy: &str) -> Option<ProxyTarget> {
    let with_scheme = if proxy.contains("://") {
        proxy.to_string()
    } else {
        format!("http://{proxy}")
    };
    let url = Url::parse(&with_scheme).ok()?;
    if url.scheme() != "http" {
        return None;
    }
    let host = url
        .host_str()?
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_string();
    let port = url.port().unwrap_or(80);
    let authorization = (!url.username().is_empty()).then(|| {
        let user = percent_decode(url.username());
        let pass = percent_decode(url.password().unwrap_or(""));
        let token = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}"));
        format!("Basic {token}")
    });
    Some(ProxyTarget {
        host,
        port,
        authorization,
    })
}

fn percent_decode(s: &str) -> String {
    url::form_urlencoded::parse(format!("x={s}").as_bytes())
        .next()
        .map(|(_, v)| v.into_owned())
        .unwrap_or_default()
}

/// ureq proxy for `url` per [`proxy_url_for`].
pub fn proxy_for(url: &str) -> Option<ureq::Proxy> {
    let parsed = Url::parse(url).ok()?;
    let proxy = proxy_url_for(&parsed, &real_env)?;
    match ureq::Proxy::new(&proxy) {
        Ok(p) => Some(p),
        Err(e) => {
            log::warn!(target: "vesyl-print.net", "ignoring invalid proxy {proxy:?}: {e}");
            None
        }
    }
}

/// A ureq agent for requests to `url` (used for proxy selection) with the
/// given timeouts and redirect policy. Status codes are returned, not raised.
pub fn agent(url: &str, timeouts: Timeouts, redirects: Redirects) -> ureq::Agent {
    let mut cfg = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .user_agent("vesyl-print-agent")
        // OS trust store, like Python urllib (sites may add a TLS-inspection CA).
        .tls_config(
            TlsConfig::builder()
                .root_certs(RootCerts::PlatformVerifier)
                .build(),
        )
        .timeout_global(None)
        .timeout_per_call(None)
        // No resolve deadline: ureq then resolves synchronously (no thread per request).
        .timeout_resolve(None)
        .timeout_connect(Some(timeouts.connect))
        .timeout_send_request(Some(timeouts.connect))
        .timeout_send_body(Some(timeouts.connect))
        .timeout_recv_response(Some(timeouts.response))
        .timeout_recv_body(Some(timeouts.body))
        .proxy(proxy_for(url));
    if redirects == Redirects::Manual {
        cfg = cfg.max_redirects(0);
    }
    cfg.build().into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| map.get(k).cloned()
    }

    fn proxy(url: &str, pairs: &[(&str, &str)]) -> Option<String> {
        proxy_url_for(&Url::parse(url).unwrap(), &env_of(pairs))
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

    #[test]
    fn websocket_schemes_follow_websocket_client() {
        assert_eq!(
            proxy("wss://x/", &[("http_proxy", "http://p:1")]).as_deref(),
            Some("http://p:1")
        );
        assert_eq!(
            proxy(
                "wss://x/",
                &[("http_proxy", "http://p:1"), ("https_proxy", "http://s:2")]
            )
            .as_deref(),
            Some("http://s:2")
        );
        assert_eq!(proxy("ws://x/", &[("https_proxy", "http://s:2")]), None);
    }

    #[test]
    fn no_proxy_rules() {
        assert!(bypass_proxy("wms-api.vesyl.dev", Some(443), "vesyl.dev"));
        assert!(bypass_proxy(
            "wms-api.vesyl.dev",
            Some(443),
            " .vesyl.dev , other"
        ));
        assert!(bypass_proxy("WMS-API.vesyl.dev", Some(443), "VESYL.DEV"));
        assert!(!bypass_proxy("notvesyl.dev", Some(443), "vesyl.dev"));
        assert!(bypass_proxy("10.0.0.5", Some(3600), "10.0.0.5:3600"));
        assert!(bypass_proxy("anything", None, "*"));
        assert!(!bypass_proxy("x", None, ""));
        let env = [("https_proxy", "http://p:1"), ("no_proxy", "vesyl.dev")];
        assert_eq!(proxy("https://wms-api.vesyl.dev:3600/print", &env), None);
        assert!(proxy("https://example.com/", &env).is_some());
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
        let auth = parse_proxy("http://us%40er:p%3Ass@p:8080")
            .unwrap()
            .authorization
            .unwrap();
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(auth.strip_prefix("Basic ").unwrap())
            .unwrap();
        assert_eq!(decoded, b"us@er:p:ss");
        assert_eq!(parse_proxy("socks5://p:1080"), None);
    }

    #[test]
    fn agent_builds_with_and_without_proxy() {
        let _ = agent(
            "https://wms-api.vesyl.dev/",
            Timeouts::API,
            Redirects::Manual,
        );
        let _ = agent(
            "http://10.0.0.5/",
            Timeouts::lan(Duration::from_secs(2)),
            Redirects::Follow,
        );
    }
}
