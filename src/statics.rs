//! Static file serving: safe path resolution, MIME types and ETag/304 caching.
//!
//! Hashed assets are accepted both under `assets/` (modern bundlers) and at
//! the web root (legacy layouts), so a rebuilt bundle is always reachable.

use std::fs::File;
use std::path::{Path, PathBuf};

use crate::http::{Body, Outgoing};

/// Strips the query string / fragment and returns the path portion.
pub fn path_only(url: &str) -> &str {
    url.split(['?', '#']).next().unwrap_or(url)
}

/// Resolve a request path to a safe relative path within the web root.
///
/// Returns `None` for path traversal (`..`), null bytes, percent-encoding or
/// empty paths.
pub fn safe_rel(path: &str) -> Option<String> {
    let p = path_only(path);
    if p.contains('\0') || p.contains('%') {
        return None;
    }
    let trimmed = p.trim_start_matches('/');
    if trimmed.is_empty() || trimmed.contains("..") {
        return None;
    }
    Some(trimmed.to_string())
}

/// Whether a request path looks like a static asset (an extension, or under
/// `/assets/`). Used to decide if a miss should 404 instead of SPA-falling
/// back to `index.html`.
pub fn is_asset_url(path: &str) -> bool {
    let p = path_only(path);
    if p.starts_with("/assets/") {
        return true;
    }
    let name = p.rsplit('/').next().unwrap_or("");
    name.contains('.')
}

/// MIME type for a served path (extension based).
pub fn mime_of(path: &str) -> &'static str {
    let p = path_only(path).to_ascii_lowercase();
    if p.ends_with(".js") || p.ends_with(".mjs") {
        "text/javascript; charset=utf-8"
    } else if p.ends_with(".css") {
        "text/css; charset=utf-8"
    } else if p.ends_with(".html") || p.ends_with(".htm") {
        "text/html; charset=utf-8"
    } else if p.ends_with(".json") || p.ends_with(".webmanifest") {
        "application/json; charset=utf-8"
    } else if p.ends_with(".wasm") {
        "application/wasm"
    } else if p.ends_with(".svg") {
        "image/svg+xml"
    } else if p.ends_with(".png") {
        "image/png"
    } else if p.ends_with(".jpg") || p.ends_with(".jpeg") {
        "image/jpeg"
    } else if p.ends_with(".gif") {
        "image/gif"
    } else if p.ends_with(".webp") {
        "image/webp"
    } else if p.ends_with(".ico") {
        "image/x-icon"
    } else if p.ends_with(".woff2") {
        "font/woff2"
    } else if p.ends_with(".woff") {
        "font/woff"
    } else if p.ends_with(".ttf") {
        "font/ttf"
    } else if p.ends_with(".otf") {
        "font/otf"
    } else if p.ends_with(".map") || p.ends_with(".txt") {
        "text/plain; charset=utf-8"
    } else if p.ends_with(".xml") {
        "application/xml; charset=utf-8"
    } else {
        "application/octet-stream"
    }
}

/// Cache-Control policy for a URL: hashed assets are immutable; HTML is
/// revalidated; everything else gets a short max-age.
pub fn cache_control_for(url_path: &str) -> &'static str {
    let p = path_only(url_path);
    if p.starts_with("/assets/") || p.starts_with("/s/") {
        "public, max-age=31536000, immutable"
    } else if p == "/" || p == "/index.html" || p.ends_with(".html") {
        "public, max-age=0, must-revalidate"
    } else {
        // Everything else (favicon, manifest, root-level JS/CSS, robots.txt…)
        // MUST revalidate on every request to guarantee the user always sees the
        // latest version.  The ETag/304 mechanism avoids re-downloading unchanged
        // content, so there is no performance penalty.
        "public, max-age=0, must-revalidate"
    }
}

fn file_etag(meta: &std::fs::Metadata) -> String {
    let secs = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("\"{:x}-{:x}\"", secs, meta.len())
}

fn etag_match(inm: &str, etag: &str) -> bool {
    inm.split(',').any(|t| {
        let t = t.trim();
        t == "*" || t == etag || t.strip_prefix("W/").unwrap_or(t) == etag
    })
}

/// Static file server rooted at a web directory.
#[derive(Clone, Debug)]
pub struct StaticFiles {
    www: PathBuf,
}

impl StaticFiles {
    pub fn new(www: impl Into<PathBuf>) -> Self {
        Self { www: www.into() }
    }

    /// The web root directory.
    pub fn www(&self) -> &Path {
        &self.www
    }

    /// Resolve a request path to a real file on disk.
    ///
    /// Tries the path as-is first, then the alternate layout: for `/assets/X`
    /// it also tries `X` at the root, and for root paths it also tries them
    /// under `assets/`. Returns `None` for unsafe or missing paths.
    pub fn resolve(&self, url_path: &str) -> Option<PathBuf> {
        let rel = safe_rel(url_path)?;
        let mut p = self.www.join(&rel);
        if p.is_file() {
            return Some(p);
        }
        if let Some(rest) = rel.strip_prefix("assets/") {
            p = self.www.join(rest);
        } else {
            p = self.www.join("assets").join(&rel);
        }
        p.is_file().then_some(p)
    }

    /// Serve a request path (304 via `If-None-Match`, 404 when missing).
    pub fn serve(&self, url_path: &str, inm: Option<&str>) -> Outgoing {
        match self.resolve(url_path) {
            Some(p) => self.serve_existing(&p, url_path, inm),
            None => Outgoing::text(404, "not found", "text/plain; charset=utf-8"),
        }
    }

    /// Serve a known file at `path` (already resolved) for `url_path`.
    pub fn serve_existing(&self, path: &Path, url_path: &str, inm: Option<&str>) -> Outgoing {
        let file = match File::open(path) {
            Ok(f) => f,
            Err(_) => return Outgoing::text(404, "not found", "text/plain; charset=utf-8"),
        };
        let meta = match file.metadata() {
            Ok(m) => m,
            Err(_) => return Outgoing::text(404, "not found", "text/plain; charset=utf-8"),
        };
        let etag = file_etag(&meta);
        let cache = cache_control_for(url_path);
        if let Some(inm) = inm {
            if etag_match(inm, &etag) {
                return Outgoing {
                    status: 304,
                    headers: vec![
                        ("ETag".into(), etag),
                        ("Cache-Control".into(), cache.into()),
                    ],
                    body: Body::Bytes(Vec::new()),
                };
            }
        }
        Outgoing {
            status: 200,
            headers: vec![
                ("Content-Type".into(), mime_of(url_path).into()),
                ("ETag".into(), etag),
                ("Cache-Control".into(), cache.into()),
            ],
            body: Body::File {
                file,
                len: meta.len(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn safe_rel_rejects_traversal_and_nulls() {
        assert!(safe_rel("/index.html").is_some());
        assert!(safe_rel("/a/b/c.js").is_some());
        assert!(safe_rel("/../etc/passwd").is_none());
        assert!(safe_rel("/a/../b").is_none());
        assert!(safe_rel("/a%20b").is_none());
        assert!(safe_rel("/a\0b").is_none());
        assert!(safe_rel("/").is_none());
    }

    #[test]
    fn mime_and_cache_control() {
        assert!(mime_of("/x.js").starts_with("text/javascript"));
        assert!(mime_of("/x.css").starts_with("text/css"));
        assert!(mime_of("/x.wasm") == "application/wasm");
        assert!(cache_control_for("/assets/a-b.js").contains("immutable"));
        assert!(cache_control_for("/index.html").contains("max-age=0"));
        assert!(cache_control_for("/other").contains("max-age=0"));
    }

    #[test]
    fn static_cache_etag_and_304() {
        let dir = std::env::temp_dir().join("vite-rs-cache-test");
        fs::create_dir_all(&dir).unwrap();
        let f = dir.join("app.js");
        fs::write(&f, b"export default 1;").unwrap();

        let sf = StaticFiles::new(&dir);
        let out = sf.serve("/app.js", None);
        assert_eq!(out.status, 200);
        let etag = out
            .headers
            .iter()
            .find(|(k, _)| k == "ETag")
            .map(|(_, v)| v.clone())
            .unwrap();

        let r = sf.serve("/app.js", Some(&etag));
        assert_eq!(r.status, 304);

        let miss = sf.serve("/nope.js", None);
        assert_eq!(miss.status, 404);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn resolve_accepts_assets_and_root_layouts() {
        let dir = std::env::temp_dir().join("vite-rs-layout-test");
        fs::create_dir_all(dir.join("assets")).unwrap();
        fs::write(dir.join("root.js"), b"a").unwrap();
        fs::write(dir.join("assets").join("hashed.js"), b"b").unwrap();

        let sf = StaticFiles::new(&dir);
        assert!(sf.resolve("/root.js").is_some());
        assert!(sf.resolve("/assets/hashed.js").is_some());
        // /assets/X with the file only at the root (legacy layout):
        assert!(sf.resolve("/assets/root.js").is_some());
        // root path with the file only under assets/:
        assert!(sf.resolve("/hashed.js").is_some());
        fs::remove_dir_all(&dir).ok();
    }
}
