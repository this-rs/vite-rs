//! # vite-rs
//!
//! A tiny, dependency-light (httparse only), **Vite-like dev server library**
//! for hosting a static SPA from Rust. It serves hashed assets with ETag/304
//! caching and, in dev mode, gives you the same loop a bundler dev server
//! does:
//!
//! * **Hot reload** — a file watcher bumps a version when served content
//!   changes; an injected script polls `/__dev/version` and reloads the page.
//! * **Rebuild on change** — when a watched source dir changes, an optional
//!   shell command (e.g. your bundler) runs; the version bumps on success and
//!   failures are logged with the build output tail.
//! * **Client error reporting** — `window.onerror`, `unhandledrejection` and
//!   `console.error` are captured and posted to `/__dev/log`, landing in a log
//!   file you can tail while developing.
//!
//! # Example
//!
//! ```no_run
//! use std::path::PathBuf;
//! use vite_rs::dev::DevState;
//! use vite_rs::http::{self, Outgoing};
//! use vite_rs::statics::StaticFiles;
//! use vite_rs::headers::DEFAULT_SECURITY_HEADERS;
//!
//! let www = PathBuf::from("dist");
//! let statics = StaticFiles::new(&www);
//! let dev = DevState::new(&www)
//!     .dev(true)
//!     .watch(vec![PathBuf::from("src")])
//!     .rebuild(Some("npm run build".into()))
//!     .log_path("/tmp/myapp-dev.log")
//!     .start()
//!     .expect("watcher");
//!
//! http::serve("0.0.0.0:8080", move |req| {
//!     if let Some(out) = dev.handle(&req) {
//!         return out;
//!     }
//!     let mut out = statics.serve(&req.url, req.header("if-none-match"));
//!     dev.inject_html(&mut out, &[]);
//!     out
//! }, DEFAULT_SECURITY_HEADERS)
//! .expect("serve");
//! ```
//!
//! SPA fallback (unknown non-asset paths → `index.html`) and custom routing
//! are left to the caller; see `examples/basic.rs` for a complete
//! production + dev handler.

pub mod dev;
pub mod headers;
pub mod http;
pub mod statics;
