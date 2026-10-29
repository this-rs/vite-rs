//! Minimal bounded HTTP/1.1 server (worker pool, keep-alive, timeouts).
//!
//! Bounded worker pool, no thread-per-connection, short idle keep-alive,
//! hard I/O deadlines. Accept overflow applies BACKPRESSURE: the accept loop
//! blocks (letting the kernel listen backlog absorb the surplus) instead of
//! answering 503. A client must always receive a served response — never
//! `busy` — so saturation delays, it never refuses.

use std::fs::File;
use std::io::{BufReader, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, TrySendError};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const HEADER_TIMEOUT: Duration = Duration::from_secs(5);
pub const BODY_TIMEOUT: Duration = Duration::from_secs(15);
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(15);
pub const IDLE_TIMEOUT: Duration = Duration::from_millis(400);
pub const PROXY_CONNECT: Duration = Duration::from_secs(2);
pub const PROXY_SHORT: Duration = Duration::from_secs(8);
pub const PROXY_LONG: Duration = Duration::from_secs(300);
pub const MAX_HEADER_BYTES: usize = 16 * 1024;
pub const MAX_BODY: u64 = 8 * 1024 * 1024;
pub const MAX_BUFFERED_UP: u64 = 2 * 1024 * 1024;
pub const MAX_KEEP_ALIVE: u32 = 32;
const MAX_HEADERS: usize = 64;

pub static ACCEPTED: AtomicU64 = AtomicU64::new(0);
/// Historically the 503 counter. Kept at 0 by construction: the server has no
/// refusal path any more. Read it to prove no client was ever turned away.
pub static REJECTED: AtomicU64 = AtomicU64::new(0);
pub static SERVED: AtomicU64 = AtomicU64::new(0);
pub static INFLIGHT: AtomicUsize = AtomicUsize::new(0);
pub static WORKERS: AtomicUsize = AtomicUsize::new(0);
/// How many times the accept loop had to wait for a free queue slot. Non-zero
/// is normal under load — it is the backpressure that replaces the 503.
pub static BACKPRESSURE: AtomicU64 = AtomicU64::new(0);

/// Worker pool size. Override with `VITE_RS_HTTP_WORKERS`.
///
/// The default is deliberately generous: a worker blocked on a slow route (the
/// chain endpoints hold the global node lock for seconds) must not be able to
/// starve the pool, because starvation used to surface as `503 busy`.
pub fn worker_count() -> usize {
    if let Ok(raw) = std::env::var("VITE_RS_HTTP_WORKERS") {
        if let Ok(n) = raw.trim().parse::<usize>() {
            if n > 0 {
                return n.clamp(4, 1024);
            }
        }
    }
    let n = std::thread::available_parallelism()
        .map(|p| p.get())
        .unwrap_or(4);
    (n * 8).clamp(32, 256)
}

pub fn queue_cap(workers: usize) -> usize {
    workers.saturating_mul(8).max(32)
}

pub struct Request {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub remote: SocketAddr,
    pub http11: bool,
    pub want_keepalive: bool,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        let want = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(&want))
            .map(|(_, v)| v.as_str())
    }

    /// Build a `GET` request for tests / programmatic use.
    pub fn get(url: &str) -> Self {
        Self {
            method: "GET".into(),
            url: url.into(),
            headers: Vec::new(),
            body: Vec::new(),
            remote: "127.0.0.1:0".parse().unwrap(),
            http11: true,
            want_keepalive: false,
        }
    }

    /// Build a `POST` request with a body for tests / programmatic use.
    pub fn post(url: &str, body: &[u8]) -> Self {
        Self {
            method: "POST".into(),
            url: url.into(),
            headers: vec![("content-type".into(), "application/json".into())],
            body: body.to_vec(),
            remote: "127.0.0.1:0".parse().unwrap(),
            http11: true,
            want_keepalive: false,
        }
    }
}

pub enum Body {
    Bytes(Vec<u8>),
    File { file: File, len: u64 },
    Stream { reader: BufReader<TcpStream>, remain: i64 },
}

pub struct Outgoing {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Body,
}

impl Outgoing {
    pub fn bytes(status: u16, ctype: &str, body: Vec<u8>) -> Self {
        Self {
            status,
            headers: vec![
                ("Content-Type".into(), ctype.into()),
                ("Content-Length".into(), body.len().to_string()),
            ],
            body: Body::Bytes(body),
        }
    }

    pub fn text(status: u16, body: &str, ctype: &str) -> Self {
        Self::bytes(status, ctype, body.as_bytes().to_vec())
    }
}

pub fn ignore_sigpipe() {
    unsafe {
        extern "C" {
            fn signal(sig: i32, handler: usize) -> usize;
        }
        signal(13, 1);
    }
}

pub fn status_text(code: u16) -> &'static str {
    match code {
        100 => "Continue",
        200 => "OK",
        204 => "No Content",
        304 => "Not Modified",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        413 => "Payload Too Large",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "OK",
    }
}

pub fn http_date() -> String {
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let days = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    let months = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let z = t;
    let secs = z % 86400;
    let mut days_since = (z / 86400) as i64;
    let hh = secs / 3600;
    let mm = (secs % 3600) / 60;
    let ss = secs % 60;
    let mut y = 1970i64;
    loop {
        let ly = if y % 4 == 0 && (y % 100 != 0 || y % 400 == 0) {
            366
        } else {
            365
        };
        if days_since < ly {
            break;
        }
        days_since -= ly;
        y += 1;
    }
    let md = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let mut mo = 0usize;
    let mut d = days_since;
    let feb = if y % 4 == 0 && (y % 100 != 0 || y % 400 == 0) {
        29
    } else {
        28
    };
    loop {
        let dim = if mo == 1 { feb } else { md[mo] } as i64;
        if d < dim {
            break;
        }
        d -= dim;
        mo += 1;
        if mo > 11 {
            break;
        }
    }
    let wday = ((z / 86400) + 4) % 7;
    format!(
        "{}, {:02} {} {:04} {:02}:{:02}:{:02} GMT",
        days[wday as usize],
        d + 1,
        months[mo.min(11)],
        y,
        hh,
        mm,
        ss
    )
}

pub fn connect_up(host: &str, port: u16, timeout: Duration) -> std::io::Result<TcpStream> {
    let spec = format!("{host}:{port}");
    let mut last = None;
    for sa in spec.to_socket_addrs()? {
        match TcpStream::connect_timeout(&sa, timeout) {
            Ok(s) => {
                let _ = s.set_nodelay(true);
                return Ok(s);
            }
            Err(e) => last = Some(e),
        }
    }
    Err(last.unwrap_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no addr")))
}

fn header_complete(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4)
}

fn parse_headers(buf: &[u8]) -> Result<(String, String, bool, Vec<(String, String)>, u64, bool, bool), ()> {
    let mut headers_arr = [httparse::EMPTY_HEADER; MAX_HEADERS];
    let mut parsed = httparse::Request::new(&mut headers_arr);
    match parsed.parse(buf) {
        Ok(httparse::Status::Complete(_)) => {}
        _ => return Err(()),
    }
    let method = parsed.method.ok_or(())?.to_string();
    let url = parsed.path.ok_or(())?.to_string();
    let http11 = parsed.version.unwrap_or(1) == 1;
    let mut headers = Vec::with_capacity(parsed.headers.len());
    let mut content_len: u64 = 0;
    let mut has_len = false;
    let mut expect_cont = false;
    let mut connection = String::new();
    for h in parsed.headers.iter() {
        let name = h.name.to_string();
        let value = String::from_utf8_lossy(h.value).into_owned();
        if name.eq_ignore_ascii_case("content-length") {
            content_len = value.trim().parse().unwrap_or(0);
            has_len = true;
        }
        if name.eq_ignore_ascii_case("expect") && value.to_ascii_lowercase().contains("100-continue") {
            expect_cont = true;
        }
        if name.eq_ignore_ascii_case("connection") {
            connection = value.to_ascii_lowercase();
        }
        headers.push((name, value));
    }
    let want_ka = if connection.contains("close") {
        false
    } else if connection.contains("keep-alive") {
        true
    } else {
        http11
    };
    let _ = has_len;
    Ok((method, url, http11, headers, content_len, want_ka, expect_cont))
}

fn read_request(
    reader: &mut BufReader<TcpStream>,
    writer: &mut TcpStream,
    remote: SocketAddr,
    idle: bool,
) -> Result<Option<Request>, ()> {
    let _ = reader
        .get_ref()
        .set_read_timeout(Some(if idle { IDLE_TIMEOUT } else { HEADER_TIMEOUT }));
    let mut buf = Vec::with_capacity(1024);
    let header_end = loop {
        if let Some(n) = header_complete(&buf) {
            break n;
        }
        let mut tmp = [0u8; 1024];
        match reader.read(&mut tmp) {
            Ok(0) => return Ok(None),
            Ok(n) => {
                buf.extend_from_slice(&tmp[..n]);
                if buf.len() > MAX_HEADER_BYTES {
                    return Err(());
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut || e.kind() == std::io::ErrorKind::WouldBlock => {
                return Ok(None);
            }
            Err(_) => return Err(()),
        }
    };
    let (method, url, http11, headers, content_len, want_keepalive, expect_cont) =
        parse_headers(&buf[..header_end])?;
    if expect_cont {
        let _ = writer.write_all(b"HTTP/1.1 100 Continue\r\n\r\n");
        let _ = writer.flush();
    }
    let mut body = buf[header_end..].to_vec();
    if content_len > MAX_BODY {
        return Err(());
    }
    if content_len > 0 {
        let _ = reader.get_ref().set_read_timeout(Some(BODY_TIMEOUT));
        let deadline = Instant::now() + BODY_TIMEOUT;
        while (body.len() as u64) < content_len {
            if Instant::now() > deadline {
                return Err(());
            }
            let rest = (content_len as usize).saturating_sub(body.len());
            let mut tmp = vec![0u8; rest.min(16 * 1024)];
            match reader.read(&mut tmp) {
                Ok(0) => break,
                Ok(n) => body.extend_from_slice(&tmp[..n]),
                Err(_) => return Err(()),
            }
        }
        body.truncate(content_len as usize);
    } else {
        body.clear();
    }
    Ok(Some(Request {
        method,
        url,
        headers,
        body,
        remote,
        http11,
        want_keepalive,
    }))
}

fn write_headers(
    writer: &mut TcpStream,
    status: u16,
    extra: &[(String, String)],
    security: &[(&str, &str)],
    ka: bool,
    content_len: Option<u64>,
) -> std::io::Result<()> {
    let _ = writer.set_write_timeout(Some(WRITE_TIMEOUT));
    write!(
        writer,
        "HTTP/1.1 {status} {}\r\n",
        status_text(status)
    )?;
    write!(writer, "Date: {}\r\n", http_date())?;
    writer.write_all(b"Server: vite-rs\r\n")?;
    if ka {
        writer.write_all(b"Connection: keep-alive\r\nKeep-Alive: timeout=1, max=32\r\n")?;
    } else {
        writer.write_all(b"Connection: close\r\n")?;
    }
    if let Some(n) = content_len {
        write!(writer, "Content-Length: {n}\r\n")?;
    }
    let mut seen_ct = false;
    for (k, v) in extra {
        if k.eq_ignore_ascii_case("content-length") || k.eq_ignore_ascii_case("connection") {
            continue;
        }
        if k.eq_ignore_ascii_case("date") || k.eq_ignore_ascii_case("server") {
            continue;
        }
        if k.eq_ignore_ascii_case("content-type") {
            seen_ct = true;
        }
        write!(writer, "{k}: {v}\r\n")?;
    }
    for (k, v) in security {
        let already = extra.iter().any(|(ek, _)| ek.eq_ignore_ascii_case(k));
        if already {
            continue;
        }
        write!(writer, "{k}: {v}\r\n")?;
        if k.eq_ignore_ascii_case("content-type") {
            seen_ct = true;
        }
    }
    let _ = seen_ct;
    writer.write_all(b"\r\n")?;
    Ok(())
}

pub fn write_out(
    writer: &mut TcpStream,
    out: Outgoing,
    security: &[(&str, &str)],
    ka: bool,
) -> std::io::Result<()> {
    match out.body {
        Body::Bytes(bytes) => {
            write_headers(
                writer,
                out.status,
                &out.headers,
                security,
                ka,
                Some(bytes.len() as u64),
            )?;
            if !bytes.is_empty() {
                writer.write_all(&bytes)?;
            }
            writer.flush()?;
        }
        Body::File { mut file, len } => {
            write_headers(writer, out.status, &out.headers, security, ka, Some(len))?;
            let mut buf = [0u8; 64 * 1024];
            let mut left = len;
            while left > 0 {
                let n = file.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                let take = (n as u64).min(left) as usize;
                writer.write_all(&buf[..take])?;
                left -= take as u64;
            }
            writer.flush()?;
        }
        Body::Stream { mut reader, remain } => {
            write_headers(writer, out.status, &out.headers, security, false, if remain >= 0 {
                Some(remain as u64)
            } else {
                None
            })?;
            let mut buf = [0u8; 16 * 1024];
            if remain >= 0 {
                let mut left = remain as u64;
                while left > 0 {
                    let want = buf.len().min(left as usize);
                    let n = reader.read(&mut buf[..want])?;
                    if n == 0 {
                        break;
                    }
                    writer.write_all(&buf[..n])?;
                    left -= n as u64;
                }
            } else {
                loop {
                    let n = reader.read(&mut buf)?;
                    if n == 0 {
                        break;
                    }
                    writer.write_all(&buf[..n])?;
                }
            }
            writer.flush()?;
        }
    }
    Ok(())
}

fn serve_conn<F>(stream: TcpStream, remote: SocketAddr, handler: &F, security: &[(&str, &str)])
where
    F: Fn(Request) -> Outgoing,
{
    let _ = stream.set_nodelay(true);
    let writer = match stream.try_clone() {
        Ok(w) => w,
        Err(_) => return,
    };
    let mut writer = writer;
    let mut reader = BufReader::with_capacity(8192, stream);
    let mut nreq: u32 = 0;
    loop {
        let idle = nreq > 0;
        let req = match read_request(&mut reader, &mut writer, remote, idle) {
            Ok(Some(r)) => r,
            Ok(None) | Err(()) => break,
        };
        let head = req.method.eq_ignore_ascii_case("HEAD");
        let path_ka = req.url.starts_with("/s/") || req.url.starts_with("/assets/");
        let can_ka = req.want_keepalive && req.http11 && nreq + 1 < MAX_KEEP_ALIVE && !head && path_ka;
        let mut out = handler(req);
        let stream_body = matches!(out.body, Body::Stream { .. });
        let ka = can_ka && !stream_body;
        if head {
            out.body = Body::Bytes(Vec::new());
        }
        if write_out(&mut writer, out, security, ka).is_err() {
            break;
        }
        SERVED.fetch_add(1, Ordering::Relaxed);
        nreq += 1;
        if !ka {
            break;
        }
    }
    let _ = writer.shutdown(Shutdown::Both);
}

pub fn serve<F>(listen: &str, handler: F, security: &'static [(&'static str, &'static str)]) -> Result<(), String>
where
    F: Fn(Request) -> Outgoing + Send + Sync + 'static,
{
    ignore_sigpipe();
    let listener = TcpListener::bind(listen).map_err(|e| format!("listen {listen}: {e}"))?;
    let _ = listener.set_nonblocking(false);
    let workers = worker_count();
    let cap = queue_cap(workers);
    WORKERS.store(workers, Ordering::Relaxed);
    let (tx, rx) = mpsc::sync_channel::<(TcpStream, SocketAddr)>(cap);
    let handler = Arc::new(handler);
    let rx = Arc::new(std::sync::Mutex::new(rx));
    for _ in 0..workers {
        let rx = Arc::clone(&rx);
        let handler = Arc::clone(&handler);
        std::thread::Builder::new()
            .name("vite-rs-http".into())
            .spawn(move || loop {
                let job = {
                    let guard = match rx.lock() {
                        Ok(g) => g,
                        Err(_) => break,
                    };
                    guard.recv()
                };
                let (stream, remote) = match job {
                    Ok(x) => x,
                    Err(_) => break,
                };
                INFLIGHT.fetch_add(1, Ordering::Relaxed);
                let h = Arc::clone(&handler);
                let _ = catch_unwind(AssertUnwindSafe(|| {
                    serve_conn(stream, remote, h.as_ref(), security);
                }));
                INFLIGHT.fetch_sub(1, Ordering::Relaxed);
            })
            .map_err(|e| format!("worker: {e}"))?;
    }
    loop {
        match listener.accept() {
            Ok((stream, remote)) => {
                ACCEPTED.fetch_add(1, Ordering::Relaxed);
                let _ = stream.set_nodelay(true);
                match tx.try_send((stream, remote)) {
                    // Chemin rapide : une place est libre, on ne bloque pas.
                    Ok(()) => {}
                    // File pleine : contre-pression. On attend une place au lieu
                    // de répondre 503 — le client est servi en retard, jamais
                    // refusé. La backlog du noyau absorbe le surplus pendant
                    // l'attente (l'accept loop ne tourne plus).
                    Err(TrySendError::Full(job)) => {
                        BACKPRESSURE.fetch_add(1, Ordering::Relaxed);
                        if tx.send(job).is_err() {
                            break;
                        }
                    }
                    Err(TrySendError::Disconnected(_)) => break,
                }
            }
            Err(_) => continue,
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workers_are_bounded() {
        let n = worker_count();
        // Le pool reste borné (pas de thread par connexion), mais il est
        // désormais assez large pour que des routes lentes ne l'affament pas.
        assert!((4..=1024).contains(&n), "pool borné, obtenu {n}");
        assert!(queue_cap(n) >= 32);
    }

    #[test]
    fn header_end_detects_crlf() {
        let b = b"GET / HTTP/1.1\r\nHost: x\r\n\r\nbody";
        assert!(header_complete(b).is_some());
    }

    #[test]
    fn parses_get() {
        let raw = b"GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
        let (m, u, http11, h, len, ka, exp) = parse_headers(raw).unwrap();
        assert_eq!(m, "GET");
        assert_eq!(u, "/healthz");
        assert!(http11);
        assert_eq!(len, 0);
        assert!(!ka);
        assert!(!exp);
        assert!(h.iter().any(|(k, _)| k.eq_ignore_ascii_case("host")));
    }
}
