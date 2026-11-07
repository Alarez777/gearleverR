//! Small HTTP server for update tests.
//!
//! It handles one request per connection, supports byte ranges, and can be
//! told to ignore `Range`, fail the first range request, or trickle a body so
//! a test can cancel a download in progress.

use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// One path served by [`TestServer`].
#[derive(Clone, Debug)]
pub(crate) struct Route {
    body: Vec<u8>,
    ignore_range: bool,
    fail_first_range: bool,
    slow: bool,
}

impl Route {
    pub(crate) fn new(body: impl Into<Vec<u8>>) -> Self {
        Self {
            body: body.into(),
            ignore_range: false,
            fail_first_range: false,
            slow: false,
        }
    }

    pub(crate) fn ignore_range(mut self) -> Self {
        self.ignore_range = true;
        self
    }

    pub(crate) fn fail_first_range(mut self) -> Self {
        self.fail_first_range = true;
        self
    }

    pub(crate) fn slow(mut self) -> Self {
        self.slow = true;
        self
    }
}

/// One request observed by [`TestServer`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Request {
    /// Request path without the query string.
    pub(crate) path: String,
    /// Inclusive byte range, when the request carried one.
    pub(crate) range: Option<(u64, u64)>,
}

/// A local HTTP server bound to an ephemeral port.
pub(crate) struct TestServer {
    address: SocketAddr,
    requests: Arc<Mutex<Vec<Request>>>,
    range_bytes: Arc<AtomicU64>,
    range_seen: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl TestServer {
    pub(crate) fn start(routes: Vec<(&str, Route)>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let routes: HashMap<String, Route> = routes
            .into_iter()
            .map(|(path, route)| (path.to_owned(), route))
            .collect();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let range_bytes = Arc::new(AtomicU64::new(0));
        let range_seen = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));

        let handle = {
            let requests = Arc::clone(&requests);
            let range_bytes = Arc::clone(&range_bytes);
            let range_seen = Arc::clone(&range_seen);
            let stop = Arc::clone(&stop);
            thread::spawn(move || {
                let mut failed: HashSet<String> = HashSet::new();
                while !stop.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((stream, _)) => serve(
                            stream,
                            &routes,
                            &mut failed,
                            &requests,
                            &range_bytes,
                            &range_seen,
                        ),
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(2));
                        }
                        Err(_) => break,
                    }
                }
            })
        };

        Self {
            address,
            requests,
            range_bytes,
            range_seen,
            stop,
            handle: Some(handle),
        }
    }

    pub(crate) fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.address)
    }

    pub(crate) fn requests(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
    }

    pub(crate) fn range_requests(&self) -> usize {
        self.requests()
            .iter()
            .filter(|request| request.range.is_some())
            .count()
    }

    pub(crate) fn range_bytes(&self) -> u64 {
        self.range_bytes.load(Ordering::Relaxed)
    }

    pub(crate) fn range_seen(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.range_seen)
    }

    pub(crate) fn full_requests(&self, path: &str) -> usize {
        self.requests()
            .iter()
            .filter(|request| request.path == path && request.range.is_none())
            .count()
    }

    pub(crate) fn request_count(&self, path: &str) -> usize {
        self.requests()
            .iter()
            .filter(|request| request.path == path)
            .count()
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn serve(
    mut stream: TcpStream,
    routes: &HashMap<String, Route>,
    failed: &mut HashSet<String>,
    requests: &Mutex<Vec<Request>>,
    range_bytes: &AtomicU64,
    range_seen: &AtomicBool,
) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let Some(headers) = read_headers(&mut stream) else {
        return;
    };

    let Some(path) = headers
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .map(|path| path.split('?').next().unwrap_or(path))
    else {
        let _ = write_status(&mut stream, 400, &[], b"");
        return;
    };

    let range = headers.lines().find_map(parse_range);
    requests.lock().unwrap().push(Request {
        path: path.to_owned(),
        range,
    });

    let Some(route) = routes.get(path) else {
        let _ = write_status(&mut stream, 404, &[], b"not found");
        return;
    };

    if let Some((start, end)) = range {
        range_seen.store(true, Ordering::Relaxed);
        if route.fail_first_range && failed.insert(path.to_owned()) {
            let _ = write_status(&mut stream, 503, &[], b"try again");
            return;
        }

        if route.ignore_range {
            let _ = write_response(
                &mut stream,
                200,
                &[("Accept-Ranges", "none")],
                &route.body,
                route.slow,
            );
            return;
        }

        let length = u64::try_from(route.body.len()).unwrap();
        let start = start.min(length.saturating_sub(1));
        let end = end.min(length.saturating_sub(1));
        if start >= length || start > end {
            let _ = write_status(&mut stream, 416, &[], b"");
            return;
        }
        let first = usize::try_from(start).unwrap();
        let last = usize::try_from(end).unwrap();
        let slice = &route.body[first..=last];
        range_bytes.fetch_add(u64::try_from(slice.len()).unwrap(), Ordering::Relaxed);
        let content_range = format!("bytes {start}-{end}/{length}");
        let headers = [
            ("Content-Range", content_range.as_str()),
            ("Accept-Ranges", "bytes"),
        ];
        let _ = write_response(&mut stream, 206, &headers, slice, route.slow);
        return;
    }

    let _ = write_response(
        &mut stream,
        200,
        &[("Accept-Ranges", "bytes")],
        &route.body,
        route.slow,
    );
}

fn read_headers(stream: &mut TcpStream) -> Option<String> {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 1024];
    loop {
        let read = stream.read(&mut chunk).ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
            return String::from_utf8(buffer).ok();
        }
        if buffer.len() > 64 * 1024 {
            return None;
        }
    }
}

fn parse_range(line: &str) -> Option<(u64, u64)> {
    let (name, value) = line.split_once(':')?;
    if !name.eq_ignore_ascii_case("range") {
        return None;
    }
    let value = value.trim().strip_prefix("bytes=")?;
    let (start, end) = value.split_once('-')?;
    let start = start.parse().ok()?;
    let end = if end.is_empty() {
        u64::MAX
    } else {
        end.parse().ok()?
    };
    Some((start, end))
}

fn write_status(
    stream: &mut TcpStream,
    status: u16,
    headers: &[(&str, &str)],
    body: &[u8],
) -> std::io::Result<()> {
    write_response(stream, status, headers, body, false)
}

#[expect(
    clippy::fn_params_excessive_bools,
    reason = "the test server needs a slow-body switch"
)]
fn write_response(
    stream: &mut TcpStream,
    status: u16,
    headers: &[(&str, &str)],
    body: &[u8],
    slow: bool,
) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        206 => "Partial Content",
        400 => "Bad Request",
        404 => "Not Found",
        416 => "Range Not Satisfiable",
        503 => "Service Unavailable",
        _ => "Unknown",
    };
    let mut response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (name, value) in headers {
        response.push_str(name);
        response.push_str(": ");
        response.push_str(value);
        response.push_str("\r\n");
    }
    response.push_str("\r\n");
    stream.write_all(response.as_bytes())?;

    if slow {
        for chunk in body.chunks(8 * 1024) {
            stream.write_all(chunk)?;
            stream.flush()?;
            thread::sleep(Duration::from_millis(10));
        }
    } else {
        stream.write_all(body)?;
    }
    stream.flush()
}
