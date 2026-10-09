//! Test-only HTTP stub: serves canned responses and records requests.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct Recorded {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Recorded {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// A loopback stub from [`serve`]. Dropping it closes its port and checks
/// that the test sent what it provisioned (see [`serve`]).
pub struct StubServer {
    pub base_url: String,
    /// Every request, in order (unexpected ones too).
    pub requests: Arc<Mutex<Vec<Recorded>>>,
    responses: usize,
    served: Arc<AtomicUsize>,
    /// Requests past the canned responses, and requests it could not read.
    unexpected: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl StubServer {
    /// How many of the canned responses went out.
    pub fn served(&self) -> usize {
        self.served.load(Ordering::SeqCst)
    }
}

impl Drop for StubServer {
    /// Close the port, then (unless the test is failing already) fail on a
    /// request it had no response for, or a response left unused once a
    /// request came. A stub no request reached is not checked: a test may
    /// set one up only to show that nothing is sent.
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        if thread::panicking() {
            return;
        }
        let unexpected = self.unexpected.lock().unwrap();
        assert!(
            unexpected.is_empty(),
            "stub {}: {} request(s) beyond its {} response(s): {unexpected:?}",
            self.base_url,
            unexpected.len(),
            self.responses
        );
        let served = self.served();
        assert!(
            served == 0 || served == self.responses,
            "stub {}: served {served}/{} responses",
            self.base_url,
            self.responses
        );
    }
}

/// Read one request from `stream`.
fn read_request(stream: &TcpStream) -> io::Result<Recorded> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "no request"));
    }
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let path = parts.next().unwrap_or_default().to_string();
    let mut headers = Vec::new();
    let mut len = 0usize;
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h)? == 0 {
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
    reader.read_exact(&mut body)?;
    Ok(Recorded {
        method,
        path,
        headers,
        body,
    })
}

/// Serve `responses` (status, body) in order, one per connection.
///
/// A request past them gets a 500 that names it, not a refused
/// connection, and fails the test when the stub is dropped; so does a
/// response left unused (see [`StubServer`]'s drop).
pub fn serve(responses: Vec<(u16, &'static str)>) -> StubServer {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    // Polled, so a drop can stop the thread and close the port.
    listener.set_nonblocking(true).unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    let requests: Arc<Mutex<Vec<Recorded>>> = Arc::default();
    let served: Arc<AtomicUsize> = Arc::default();
    let unexpected: Arc<Mutex<Vec<String>>> = Arc::default();
    let stop = Arc::new(AtomicBool::new(false));
    let count = responses.len();
    let (rec, n, odd, s) = (
        requests.clone(),
        served.clone(),
        unexpected.clone(),
        stop.clone(),
    );
    let thread = thread::spawn(move || {
        let mut canned = responses.into_iter();
        while !s.load(Ordering::SeqCst) {
            let Ok((stream, _)) = listener.accept() else {
                thread::sleep(Duration::from_millis(5));
                continue;
            };
            let _ = stream.set_nonblocking(false);
            // A client that never finishes its request cannot hang the drop.
            let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
            let req = match read_request(&stream) {
                Ok(r) => r,
                Err(e) => {
                    odd.lock().unwrap().push(format!("unreadable request: {e}"));
                    continue;
                }
            };
            let (status, body) = match canned.next() {
                Some((status, body)) => {
                    n.fetch_add(1, Ordering::SeqCst);
                    (status, body.to_string())
                }
                None => {
                    let what = format!("{} {}", req.method, req.path);
                    let mut odd = odd.lock().unwrap();
                    odd.push(what.clone());
                    (
                        500,
                        format!("stub: unexpected request #{}: {what}", odd.len()),
                    )
                }
            };
            rec.lock().unwrap().push(req);
            let mut stream = stream;
            let _ = write!(
                stream,
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    StubServer {
        base_url,
        requests,
        responses: count,
        served,
        unexpected,
        stop,
        thread: Some(thread),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;

    /// GET `path` from the stub at `base_url`: (status line, body).
    fn get(base_url: &str, path: &str) -> (String, String) {
        let addr = base_url.trim_start_matches("http://");
        let mut stream = TcpStream::connect(addr).unwrap();
        write!(stream, "GET {path} HTTP/1.1\r\nHost: {addr}\r\n\r\n").unwrap();
        let mut reply = String::new();
        stream.read_to_string(&mut reply).unwrap();
        let (head, body) = reply.split_once("\r\n\r\n").unwrap();
        (head.lines().next().unwrap().to_string(), body.to_string())
    }

    fn addr_of(base_url: &str) -> SocketAddr {
        base_url.trim_start_matches("http://").parse().unwrap()
    }

    /// J9: a stub with a response left over used to keep a thread blocked
    /// in accept(), and its port, for the rest of the run; the miscount
    /// went unnoticed.
    #[test]
    #[should_panic(expected = "served 1/2 responses")]
    fn an_unused_response_fails_the_test() {
        let srv = serve(vec![(200, "{}"), (200, "{}")]);
        assert_eq!(get(&srv.base_url, "/a").0, "HTTP/1.1 200 X");
    }

    /// One request too many got "connection refused", a transport error far
    /// from the miscount. It now gets a 500 naming it, and the drop fails.
    #[test]
    #[should_panic(expected = "1 request(s) beyond its 1 response(s)")]
    fn an_extra_request_is_named_and_fails_the_test() {
        let srv = serve(vec![(201, r#"{"ok":true}"#)]);
        assert_eq!(
            get(&srv.base_url, "/first"),
            ("HTTP/1.1 201 X".to_string(), r#"{"ok":true}"#.to_string())
        );
        assert_eq!(
            get(&srv.base_url, "/second"),
            (
                "HTTP/1.1 500 X".to_string(),
                "stub: unexpected request #1: GET /second".to_string()
            )
        );
        assert_eq!(srv.requests.lock().unwrap().len(), 2);
    }

    /// Dropped, a stub releases its port at once, used or not (an unused
    /// response kept its thread in accept(), holding the port).
    #[test]
    fn drop_closes_the_port() {
        for used in [false, true] {
            let srv = serve(vec![(200, "{}")]);
            let addr = addr_of(&srv.base_url);
            if used {
                get(&srv.base_url, "/a");
            }
            drop(srv);
            // Only a closed listener lets another one bind its port.
            TcpListener::bind(addr).unwrap_or_else(|e| panic!("{addr} still taken: {e}"));
        }
    }

    /// A stub no request reached passes: a test shows with one that
    /// nothing is sent.
    #[test]
    fn an_untouched_stub_passes() {
        let srv = serve(vec![(201, "{}")]);
        assert_eq!(srv.served(), 0);
    }
}
