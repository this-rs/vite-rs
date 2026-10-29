//! End-to-end tests: a real server on an ephemeral port, exercised over TCP.
//!
//! These spin up `http::serve` in a background thread and talk to it with raw
//! `TcpStream` requests (no external HTTP client).

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

use vite_rs::dev::DevState;
use vite_rs::headers::DEFAULT_SECURITY_HEADERS;
use vite_rs::http::{self, Outgoing};
use vite_rs::statics::StaticFiles;

fn test_port(offset: u16) -> u16 {
    let pid = std::process::id() as u16;
    20000 + (pid % 15000) + offset
}

/// One raw HTTP request with `Connection: close` so the server ends the body.
fn http(addr: &str, method: &str, path: &str, body: Option<&str>) -> (u16, String) {
    let mut raw = format!("{method} {path} HTTP/1.1\r\nHost: test\r\nConnection: close\r\n");
    if let Some(b) = body {
        raw.push_str(&format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            b.len()
        ));
    }
    raw.push_str("\r\n");
    if let Some(b) = body {
        raw.push_str(b);
    }
    let mut s = TcpStream::connect(addr).expect("connect");
    s.set_read_timeout(Some(Duration::from_secs(8))).unwrap();
    s.write_all(raw.as_bytes()).expect("write");
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).expect("read");
    let text = String::from_utf8_lossy(&buf).into_owned();
    let status: u16 = text
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    (status, text)
}

fn wait_ready(addr: &str) {
    for _ in 0..50 {
        if TcpStream::connect(addr).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("server did not come up on {addr}");
}

/// A full production + dev handler, mirroring `examples/basic.rs`.
fn handler_for(statics: StaticFiles, dev: std::sync::Arc<DevState>) -> impl Fn(vite_rs::http::Request) -> Outgoing {
    move |req| {
        if let Some(out) = dev.handle(&req) {
            return out;
        }
        let mut out = match statics.resolve(&req.url) {
            Some(p) => statics.serve_existing(&p, &req.url, req.header("if-none-match")),
            None => {
                if vite_rs::statics::is_asset_url(&req.url) {
                    Outgoing::text(404, "not found", "text/plain; charset=utf-8")
                } else {
                    statics.serve("/index.html", req.header("if-none-match"))
                }
            }
        };
        dev.inject_html(&mut out, &[]);
        out
    }
}

fn spawn_server(port: u16, www: &std::path::Path, src: Option<PathBuf>, rebuild: Option<String>) -> std::sync::Arc<DevState> {
    let statics = StaticFiles::new(www);
    let dev = DevState::new(www)
        .dev(true)
        .watch(src) // Option<PathBuf> is IntoIterator (0 or 1 dirs)
        .rebuild(rebuild)
        .poll_ms(250)
        .log_path(std::env::temp_dir().join(format!("vite-rs-it-{port}.log")))
        .start()
        .expect("dev start");
    let dev2 = dev.clone();
    let addr = format!("127.0.0.1:{port}");
    let listen = addr.clone();
    std::thread::spawn(move || {
        http::serve(&listen, handler_for(statics, dev2), DEFAULT_SECURITY_HEADERS)
            .expect("serve");
    });
    wait_ready(&addr);
    dev
}

#[test]
fn end_to_end_serving_dev_and_bridge() {
    let port = test_port(1);
    let root = std::env::temp_dir().join(format!("vite-rs-it-e2e-{port}"));
    let www = root.join("www");
    std::fs::create_dir_all(www.join("assets")).unwrap();
    std::fs::write(www.join("index.html"), "<html><head><title>it</title></head><body>hi</body></html>").unwrap();
    std::fs::write(www.join("assets").join("app-h1.js"), "export default 1;").unwrap();
    let dev = spawn_server(port, &www, None, None);
    let addr = format!("127.0.0.1:{port}");

    // 1. index with injected dev snippet + security headers
    let (s, body) = http(&addr, "GET", "/", None);
    assert_eq!(s, 200, "{body}");
    assert!(body.contains("__VITE_RS_DEV"), "snippet injected");
    assert!(body.to_lowercase().contains("x-content-type-options: nosniff"), "{body}");

    // 2. SPA fallback for non-asset misses
    let (s, body) = http(&addr, "GET", "/some/route", None);
    assert_eq!(s, 200, "{body}");
    assert!(body.contains("<title>it</title>"), "index.html served for SPA route");

    // 3. assets resolve from assets/ (modern layout)
    let (s, body) = http(&addr, "GET", "/assets/app-h1.js", None);
    assert_eq!(s, 200, "{body}");
    assert!(body.contains("ETag"), "etag present");
    let etag = body
        .lines()
        .find(|l| l.to_ascii_lowercase().starts_with("etag:"))
        .unwrap()
        .split_once(':')
        .unwrap()
        .1
        .trim()
        .to_string();

    // 4. If-None-Match -> 304
    let raw = format!("GET /assets/app-h1.js HTTP/1.1\r\nHost: test\r\nIf-None-Match: {etag}\r\nConnection: close\r\n\r\n");
    let mut s = TcpStream::connect(&addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    s.write_all(format!("GET /assets/app-h1.js HTTP/1.1\r\nHost: test\r\nIf-None-Match: {etag}\r\nConnection: close\r\n\r\n").as_bytes()).unwrap();
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).unwrap();
    let text = String::from_utf8_lossy(&buf);
    assert!(text.starts_with("HTTP/1.1 304"), "{text}");

    // 5. asset miss -> 404
    let (s, _) = http(&addr, "GET", "/assets/missing.js", None);
    assert_eq!(s, 404);

    // 6. dev endpoints
    let (s, body) = http(&addr, "GET", "/__dev/version", None);
    assert_eq!(s, 200);
    assert!(body.contains("\"v\":"), "{body}");

    // 7. log batch POST
    let (s, _) = http(&addr, "POST", "/__dev/log", Some("[{\"kind\":\"console\",\"level\":\"warn\",\"args\":[\"x\"]},{\"kind\":\"route\",\"url\":\"/y\"}]"));
    assert_eq!(s, 204);
    let log = std::fs::read_to_string(std::env::temp_dir().join(format!("vite-rs-it-{port}.log"))).unwrap_or_default();
    assert!(log.contains("\"warn\""), "{log}");
    assert!(log.contains("\"route\""), "{log}");

    // 8. command bridge round trip over real HTTP
    let (s, body) = http(&addr, "POST", "/__dev/command", Some(r#"{"code":"document.title"}"#));
    assert_eq!(s, 200, "{body}");
    let id: String = body
        .lines()
        .last()
        .and_then(|l| {
            let v: serde_json::Value = serde_json::from_str(l).ok()?;
            v.get("id").and_then(|i| i.as_str()).map(String::from)
        })
        .expect("command id");
    let (s, body) = http(&addr, "GET", "/__dev/command/next", None);
    assert_eq!(s, 200);
    assert!(body.contains("document.title"), "{body}");
    let (s, _) = http(
        &addr,
        "POST",
        "/__dev/command/result",
        Some(&format!(r#"{{"id":"{id}","ok":true,"result":"It Page"}}"#)),
    );
    assert_eq!(s, 204);
    let (s, body) = http(&addr, "GET", &format!("/__dev/command/result?id={id}"), None);
    assert_eq!(s, 200);
    assert!(body.contains("It Page"), "{body}");
    let _ = dev;

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn watcher_rebuilds_on_src_change() {
    let port = test_port(2);
    let root = std::env::temp_dir().join(format!("vite-rs-it-watch-{port}"));
    let www = root.join("www");
    let src = root.join("src");
    std::fs::create_dir_all(&www).unwrap();
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(www.join("index.html"), "<html>ok</html>").unwrap();
    std::fs::write(src.join("app.js"), "export const a = 1;").unwrap();
    let marker = root.join("built.marker");
    let dev = spawn_server(
        port,
        &www,
        Some(src.clone()),
        Some(format!("touch {}", marker.display())),
    );
    let addr = format!("127.0.0.1:{port}");
    let (s, body) = http(&addr, "GET", "/__dev/version", None);
    assert_eq!(s, 200);
    let v0: u64 = body
        .lines()
        .last()
        .and_then(|l| {
            let v: serde_json::Value = serde_json::from_str(l).ok()?;
            v.get("v").and_then(|x| x.as_u64())
        })
        .expect("version");
    // touch a source file -> rebuild command runs -> version bumps
    std::fs::write(src.join("app.js"), "export const a = 2;").unwrap();
    let mut bumped = false;
    for _ in 0..30 {
        std::thread::sleep(Duration::from_millis(200));
        if marker.exists() {
            bumped = true;
            break;
        }
    }
    assert!(bumped, "rebuild command did not run");
    let (_, body) = http(&addr, "GET", "/__dev/version", None);
    assert!(
        body.contains(&format!("\"v\":{}", v0 + 1)) || body.contains(&format!("\"v\":{}", v0 + 2)),
        "version should have bumped from {v0}: {body}"
    );
    let _ = dev;
    std::fs::remove_dir_all(&root).ok();
}
