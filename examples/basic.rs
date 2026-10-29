//! Minimal example: serve `./public` with hot reload + error reporting.
//!
//! ```sh
//! mkdir -p public src
//! echo '<!doctype html><title>vite-rs</title><body>hello' > public/index.html
//! echo 'export const x = 1;' > src/app.js
//! cargo run --example basic
//! # open http://127.0.0.1:8080
//! ```

use std::path::PathBuf;

use vite_rs::dev::DevState;
use vite_rs::headers::DEFAULT_SECURITY_HEADERS;
use vite_rs::http::{self, Outgoing};
use vite_rs::statics::StaticFiles;

fn main() -> Result<(), String> {
    let www = PathBuf::from("public");
    let statics = StaticFiles::new(&www);
    let dev = DevState::new(&www)
        .dev(true)
        .watch(vec![PathBuf::from("src")])
        .rebuild(Some(
            "echo '[example] rebuild triggered'; cp -r src/* public/".into(),
        ))
        .rebuild_cwd(".")
        .log_path("/tmp/vite-rs-example.log")
        .start()?;

    println!("vite-rs example on http://127.0.0.1:8080 (log: /tmp/vite-rs-example.log)");
    let listen = std::env::var("PORT").unwrap_or_else(|_| "127.0.0.1:8080".into());
    http::serve(&listen, move |req| {
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
    }, DEFAULT_SECURITY_HEADERS)
}
