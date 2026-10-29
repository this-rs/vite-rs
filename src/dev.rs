//! Vite-like dev mode: file watching, rebuild-on-change, browser reload and
//! client error reporting.
//!
//! The [`DevState`] is cheap to construct in production too (it is inert when
//! dev mode is off). In dev mode it spawns a polling watcher that:
//!
//! * bumps a reload `version` whenever served content changes,
//! * runs a `rebuild` command (e.g. a bundler) when a watched source dir
//!   changes, then bumps the version on success and logs failures,
//! * exposes `__dev` endpoints (`/__dev/version`, `/__dev/log`) that the
//!   injected [`DEV_SNIPPET`] polls for reloads and posts browser errors to.

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::http::{Body, Outgoing, Request};

/// Injected into every served HTML page in dev mode. It turns the browser into
/// an observable, drivable agent for an AI:
///
/// * **console capture** — `log/info/warn/error/debug/trace` with
///   JSON-serializable arguments and stack traces,
/// * **window errors** — `error` and `unhandledrejection` with stacks,
/// * **network capture** — `fetch`/`XHR` with status, timing and the
///   *initiator* stack (the code path that triggered the request),
/// * **SPA route events** — `pushState`/`replaceState`/`popstate`/hashchange,
/// * **hot reload** — polls `/__dev/version` and reloads on change,
/// * **AI command bridge** — polls `/__dev/command/next`, runs queued JS with
///   `new Function`, and posts the JSON-serializable result to
///   `/__dev/command/result`.
pub const DEV_SNIPPET: &str = r#"<script>
window.__VITE_RS_DEV=1;
(function(){
  function send(ev){
    try{ev.t=Math.round(performance.now());fetch("/__dev/log",{method:"POST",headers:{"content-type":"application/json"},body:JSON.stringify(ev),keepalive:true});}catch(e){}
  }
  function safe(v,d){
    d=d||0;
    try{
      if(v===null)return null;
      var t=typeof v;
      if(t==="undefined")return "[undefined]";
      if(t==="function")return "[fn "+(v.name||"anon")+"]";
      if(t==="number"||t==="boolean")return v;
      if(t==="string")return v.length>500?v.slice(0,500)+"…":v;
      if(t==="symbol")return String(v);
      if(v instanceof Error)return {name:v.name,message:v.message,stack:v.stack};
      if(typeof Promise!=="undefined"&&v instanceof Promise)return "[Promise]";
      if(Array.isArray(v)){
        if(d>2)return "[Array("+v.length+")]";
        var a=[];for(var i=0;i<Math.min(v.length,20);i++){try{a.push(safe(v[i],d+1));}catch(x){a.push("[?]");}}
        return a;
      }
      if(d>2)return "[Object]";
      var o={};var ks=Object.keys(v).slice(0,20);
      for(var i=0;i<ks.length;i++){try{o[ks[i]]=safe(v[ks[i]],d+1);}catch(x){o[ks[i]]="[?]";}}
      return o;
    }catch(e){return String(v);}
  }
  function stackOf(){try{return new Error().stack;}catch(e){return "";}}
  // ---- console capture (all levels) ----
  (function(){
    var orig={};
    ["log","info","warn","error","debug","trace"].forEach(function(lv){
      orig[lv]=console[lv]?console[lv].bind(console):function(){};
      console[lv]=function(){
        try{send({kind:"console",level:lv,args:Array.prototype.slice.call(arguments).map(function(a){return safe(a,0);}),stack:lv==="trace"?stackOf():undefined,href:location.href});}catch(e){}
        return orig[lv].apply(console,arguments);
      };
    });
  })();
  // ---- window errors ----
  window.addEventListener("error",function(e){
    send({kind:"error",message:String(e.message||e),source:e.filename,line:e.lineno,col:e.colno,stack:e.error&&e.error.stack,href:location.href});
  });
  window.addEventListener("unhandledrejection",function(e){
    var r=e.reason;
    send({kind:"unhandledrejection",message:String(r&&r.message||r),stack:r&&r.stack,href:location.href});
  });
  // ---- network capture (fetch + XHR, with initiator stacks) ----
  (function(){
    var origFetch=window.fetch;
    if(origFetch&&!window.__VITE_RS_NET){
      window.__VITE_RS_NET=1;
      window.fetch=function(input,init){
        var url=(typeof input==="string")?input:(input&&input.url)||String(input);
        var t0=performance.now();var stack=stackOf();var done=false;
        var p=origFetch.apply(this,arguments);
        function fin(ev){if(done)return;done=true;ev.kind="network";ev.op="fetch";ev.url=url;ev.ms=Math.round(performance.now()-t0);ev.stack=stack;ev.href=location.href;send(ev);}
        p.then(function(r){var ct="";try{ct=r.headers.get("content-type")||"";}catch(e){}fin({status:r.status,ok:r.ok,ct:ct});},function(e){fin({error:String(e&&e.message||e)});});
        return p;
      };
    }
    var origOpen=XMLHttpRequest.prototype.open;
    var origSend=XMLHttpRequest.prototype.send;
    if(!window.__VITE_RS_XHR){
      window.__VITE_RS_XHR=1;
      XMLHttpRequest.prototype.open=function(m,u){this.__vrs={method:m,url:u,stack:stackOf(),t0:0};return origOpen.apply(this,arguments);};
      XMLHttpRequest.prototype.send=function(){
        if(this.__vrs){this.__vrs.t0=performance.now();var self=this;
          this.addEventListener("loadend",function(){
            var r=self.__vrs;if(!r)return;
            send({kind:"network",op:"xhr",method:r.method,url:r.url,status:self.status,ms:Math.round(performance.now()-r.t0),stack:r.stack,href:location.href});
          });
        }
        return origSend.apply(this,arguments);
      };
    }
  })();
  // ---- SPA route changes ----
  (function(){
    function route(){send({kind:"route",url:location.pathname+location.search,href:location.href});}
    var ps=history.pushState;history.pushState=function(){var r=ps.apply(this,arguments);route();return r;};
    var rs=history.replaceState;history.replaceState=function(){var r=rs.apply(this,arguments);route();return r;};
    window.addEventListener("popstate",route);
    window.addEventListener("hashchange",route);
  })();
  // ---- hot reload poller ----
  (function(){
    var last=null;
    setInterval(function(){
      fetch("/__dev/version",{cache:"no-store"}).then(function(r){return r.json();}).then(function(j){
        if(j.rebuilding)return;
        if(last===null){last=j.v;return;}
        if(j.v!==last){last=j.v;location.reload();}
      }).catch(function(){});
    },700);
  })();
  // ---- AI command bridge ----
  (function(){
    if(!window.__VITE_RS_CMD){
      window.__VITE_RS_CMD=1;
      setInterval(function(){
        fetch("/__dev/command/next",{cache:"no-store"}).then(function(r){return r.json();}).then(function(c){
          if(!c||!c.id||c.code===undefined)return;
          var id=c.id;var done=false;
          function post(ok,result){
            if(done)return;done=true;
            try{fetch("/__dev/command/result",{method:"POST",headers:{"content-type":"application/json"},body:JSON.stringify({id:id,ok:ok,result:result}),keepalive:true});}catch(e){}
          }
          try{
            var fn=new Function(c.code);
            var v=fn();
            if(v&&typeof v.then==="function"){
              v.then(function(r){post(true,safe(r,0));},function(e){post(false,String(e&&e.message||e)+"\n"+(e&&e.stack||""));});
            }else{post(true,safe(v,0));}
          }catch(e){post(false,String(e&&e.message||e)+"\n"+(e&&e.stack||""));}
        }).catch(function(){});
      },400);
    }
  })();
})();
</script>"#;

/// Dev state shared between the watcher thread and HTTP handlers.
///
/// In dev mode it also powers an **AI debugging bridge**:
///
/// * `POST /__dev/command` queues JavaScript for the browser
///   (`{"code": "..."}` → `{"id": "..."}`),
/// * the injected snippet polls `GET /__dev/command/next`, runs the code with
///   `new Function` and posts the JSON-serializable result back,
/// * `GET /__dev/command/result?id=...` returns that result.
///
/// Combined with full console capture, network capture (fetch/XHR with
/// initiator stacks) and SPA route events, an agent can inspect and debug
/// everything the browser does — without a DevTools session.
pub struct DevState {
    dev: bool,
    version: AtomicU64,
    rebuilding: AtomicBool,
    www: PathBuf,
    watch: Vec<PathBuf>,
    rebuild_cmd: Option<String>,
    rebuild_cwd: PathBuf,
    poll_ms: u64,
    log_path: PathBuf,
    commands_enabled: bool,
    cmd_seq: AtomicU64,
    commands: Mutex<VecDeque<(String, String)>>,
    results: Mutex<HashMap<String, String>>,
}

impl std::fmt::Debug for DevState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DevState")
            .field("dev", &self.dev)
            .field("version", &self.version.load(Ordering::Relaxed))
            .field("rebuilding", &self.rebuilding.load(Ordering::Relaxed))
            .field("www", &self.www)
            .field("watch", &self.watch)
            .field("rebuild_cmd", &self.rebuild_cmd)
            .field("poll_ms", &self.poll_ms)
            .field("log_path", &self.log_path)
            .finish()
    }
}

impl DevState {
    /// New dev state rooted at the served `www` dir. Inert (no watcher, no
    /// endpoints) until [`DevState::dev`] is enabled.
    pub fn new(www: impl Into<PathBuf>) -> Self {
        let www = www.into();
        Self {
            dev: false,
            version: AtomicU64::new(0),
            rebuilding: AtomicBool::new(false),
            rebuild_cwd: www
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| www.clone()),
            www,
            watch: Vec::new(),
            rebuild_cmd: None,
            poll_ms: 500,
            log_path: std::env::temp_dir().join("vite-rs.log"),
            commands_enabled: true,
            cmd_seq: AtomicU64::new(0),
            commands: Mutex::new(VecDeque::new()),
            results: Mutex::new(HashMap::new()),
        }
    }

    /// Enable / disable dev mode. Default: off (production).
    pub fn dev(mut self, on: bool) -> Self {
        self.dev = on;
        self
    }

    /// Directories watched for changes (in addition to the served `www`).
    pub fn watch(mut self, dirs: impl IntoIterator<Item = PathBuf>) -> Self {
        self.watch = dirs.into_iter().collect();
        self.watch.sort();
        self.watch.dedup();
        self
    }

    /// Shell command run when a watched source dir changes (e.g. a bundler).
    /// The command runs with cwd = [`DevState::rebuild_cwd`].
    pub fn rebuild(mut self, cmd: Option<String>) -> Self {
        self.rebuild_cmd = cmd;
        self
    }

    /// Working directory for the rebuild command. Defaults to the parent of
    /// the served `www` dir; set it to the project root when `www` is
    /// `<root>/dist/static`.
    pub fn rebuild_cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.rebuild_cwd = cwd.into();
        self
    }

    /// Watcher poll interval in ms. Default: 500.
    pub fn poll_ms(mut self, ms: u64) -> Self {
        self.poll_ms = ms.max(200);
        self
    }

    /// File where dev events and client errors are appended. Default:
    /// `$TMPDIR/vite-rs.log`.
    pub fn log_path(mut self, p: impl Into<PathBuf>) -> Self {
        self.log_path = p.into();
        self
    }

    /// Enable / disable the AI command bridge (`/__dev/command*`). Default:
    /// enabled in dev mode. Disable it for untrusted environments — it can
    /// execute arbitrary JavaScript in connected browsers.
    pub fn commands(mut self, on: bool) -> Self {
        self.commands_enabled = on;
        self
    }

    /// Whether dev mode is enabled.
    pub fn is_dev(&self) -> bool {
        self.dev
    }

    /// Current reload version (bumped on every content change / rebuild).
    pub fn version(&self) -> u64 {
        self.version.load(Ordering::Relaxed)
    }

    /// Whether a rebuild is currently running.
    pub fn rebuilding(&self) -> bool {
        self.rebuilding.load(Ordering::Relaxed)
    }

    /// Append a line to the dev log (stderr + log file). Never panics, even
    /// when stderr is a closed pipe.
    pub fn append_log(&self, line: &str) {
        let _ = writeln!(std::io::stderr(), "[vite-rs] {line}");
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log_path)
        {
            let _ = writeln!(f, "{line}");
        }
    }

    /// Spawn the watcher thread and return the shared state.
    ///
    /// Consumes `self` (the shared state is wrapped in an `Arc`). No-op
    /// (returns the plain state) unless dev mode is enabled.
    pub fn start(self) -> Result<Arc<Self>, String> {
        if !self.dev {
            return Ok(Arc::new(self));
        }
        let state = Arc::new(self);
        let st = Arc::clone(&state);
        std::thread::Builder::new()
            .name("vite-rs-watch".into())
            .spawn(move || watcher_loop(st))
            .map_err(|e| format!("dev watcher spawn: {e}"))?;
        Ok(state)
    }

    /// Handle `__dev` requests (`/__dev`, `/__dev/version`, `/__dev/log`).
    /// Returns `None` when the path is not a dev endpoint or dev is off.
    pub fn handle(&self, req: &Request) -> Option<Outgoing> {
        if !self.dev {
            return None;
        }
        let p = crate::statics::path_only(&req.url);
        if !(p == "/__dev" || p.starts_with("/__dev/")) {
            return None;
        }
        if p == "/__dev/version" || p == "/__dev/version/" {
            let body = format!(
                "{{\"v\":{},\"rebuilding\":{}}}",
                self.version.load(Ordering::Relaxed),
                self.rebuilding.load(Ordering::Relaxed)
            );
            return Some(Outgoing::bytes(
                200,
                "application/json; charset=utf-8",
                body.into_bytes(),
            ));
        }
        if p == "/__dev" || p == "/__dev/" {
            let body = format!(
                "{{\"ok\":true,\"dev\":true,\"log\":\"{}\"}}",
                self.log_path.display()
            );
            return Some(Outgoing::bytes(
                200,
                "application/json; charset=utf-8",
                body.into_bytes(),
            ));
        }
        if p == "/__dev/log" || p == "/__dev/log/" {
            if req.method.eq_ignore_ascii_case("POST")
                || req.method.eq_ignore_ascii_case("PUT")
            {
                let raw = String::from_utf8_lossy(&req.body);
                let trimmed = raw.trim();
                // Batch support: a JSON array of events (the snippet may flush
                // several captured events in one POST).
                if trimmed.starts_with('[') {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) {
                        if let Some(arr) = v.as_array() {
                            for item in arr {
                                self.append_log(&item.to_string());
                            }
                            return Some(Outgoing {
                                status: 204,
                                headers: vec![("Cache-Control".into(), "no-store".into())],
                                body: Body::Bytes(Vec::new()),
                            });
                        }
                    }
                }
                let line = trimmed.replace('\n', " ");
                if !line.is_empty() {
                    self.append_log(&line);
                }
                return Some(Outgoing {
                    status: 204,
                    headers: vec![("Cache-Control".into(), "no-store".into())],
                    body: Body::Bytes(Vec::new()),
                });
            }
            let body = std::fs::read_to_string(&self.log_path).unwrap_or_default();
            return Some(Outgoing::bytes(
                200,
                "text/plain; charset=utf-8",
                body.into_bytes(),
            ));
        }
        if p == "/__dev/command" || p == "/__dev/command/" {
            return self.handle_command(req);
        }
        if p == "/__dev/command/next" || p == "/__dev/command/next/" {
            return self.handle_command_next(req);
        }
        if p == "/__dev/command/result" || p == "/__dev/command/result/" {
            return self.handle_command_result(req);
        }
        Some(Outgoing::text(404, "not found", "text/plain; charset=utf-8"))
    }

    /// `POST /__dev/command` — queue JavaScript for the browser.
    fn handle_command(&self, req: &Request) -> Option<Outgoing> {
        if !self.commands_enabled {
            return Some(Outgoing::text(
                403,
                "command bridge disabled",
                "text/plain; charset=utf-8",
            ));
        }
        if !req.method.eq_ignore_ascii_case("POST") {
            return Some(Outgoing::text(405, "POST only", "text/plain; charset=utf-8"));
        }
        let raw = String::from_utf8_lossy(&req.body);
        let code = serde_json::from_str::<serde_json::Value>(&raw)
            .ok()
            .and_then(|v| v.get("code").and_then(|c| c.as_str()).map(String::from));
        let Some(code) = code else {
            return Some(Outgoing::text(
                400,
                "{\"error\":\"missing 'code'\"}",
                "application/json; charset=utf-8",
            ));
        };
        let id = self.cmd_seq.fetch_add(1, Ordering::Relaxed) + 1;
        self.commands
            .lock()
            .unwrap()
            .push_back((id.to_string(), code));
        let body = format!("{{\"id\":\"{id}\"}}");
        Some(Outgoing::bytes(
            200,
            "application/json; charset=utf-8",
            body.into_bytes(),
        ))
    }

    /// `GET /__dev/command/next` — pop the next queued command for a browser.
    fn handle_command_next(&self, _req: &Request) -> Option<Outgoing> {
        if !self.commands_enabled {
            return Some(Outgoing::text(
                403,
                "command bridge disabled",
                "text/plain; charset=utf-8",
            ));
        }
        let next = self.commands.lock().unwrap().pop_front();
        let body = match next {
            Some((id, code)) => {
                serde_json::json!({ "id": id, "code": code }).to_string()
            }
            None => "{}".to_string(),
        };
        Some(Outgoing::bytes(
            200,
            "application/json; charset=utf-8",
            body.into_bytes(),
        ))
    }

    /// `POST /__dev/command/result` (store a browser result) and
    /// `GET /__dev/command/result?id=...` (fetch it back).
    fn handle_command_result(&self, req: &Request) -> Option<Outgoing> {
        if !self.commands_enabled {
            return Some(Outgoing::text(
                403,
                "command bridge disabled",
                "text/plain; charset=utf-8",
            ));
        }
        if req.method.eq_ignore_ascii_case("POST") {
            let raw = String::from_utf8_lossy(&req.body);
            // The snippet posts {"id":..., "ok":..., "result":...}; store it
            // verbatim so the agent can read it back byte-for-byte.
            let id = serde_json::from_str::<serde_json::Value>(&raw)
                .ok()
                .and_then(|v| v.get("id").and_then(|i| i.as_str()).map(String::from));
            if let Some(id) = id {
                self.results.lock().unwrap().insert(id, raw.into_owned());
            }
            return Some(Outgoing {
                status: 204,
                headers: vec![("Cache-Control".into(), "no-store".into())],
                body: Body::Bytes(Vec::new()),
            });
        }
        let id = req
            .url
            .split('?')
            .nth(1)
            .unwrap_or("")
            .split('&')
            .find_map(|kv| kv.strip_prefix("id="))
            .map(|v| v.to_string())
            .unwrap_or_default();
        let body = match self.results.lock().unwrap().get(&id) {
            Some(res) => res.clone(),
            None => serde_json::json!({ "pending": true }).to_string(),
        };
        Some(Outgoing::bytes(
            200,
            "application/json; charset=utf-8",
            body.into_bytes(),
        ))
    }

    /// Inject the dev snippet (and caller snippets) into a 200 HTML response.
    ///
    /// `extra` is a list of `(marker, html)` pairs, injected only when the
    /// marker is not already present in the page (e.g. a "native runtime"
    /// flag script). The dev snippet is injected when dev mode is on.
    pub fn inject_html(&self, out: &mut Outgoing, extra: &[(&str, &str)]) {
        self.inject_html_inner(out, extra, true)
    }

    /// Inject only the `extra` snippets, never the dev-reload snippet. Used by
    /// SSR-hydratable pages: the dev script inserted into `<head>` shifts the
    /// DOM and breaks React hydration of the TanStack stream.
    pub fn inject_html_no_dev(&self, out: &mut Outgoing, extra: &[(&str, &str)]) {
        self.inject_html_inner(out, extra, false)
    }

    fn inject_html_inner(&self, out: &mut Outgoing, extra: &[(&str, &str)], with_dev: bool) {
        if out.status != 200 {
            return;
        }
        let mut bytes = match &mut out.body {
            Body::Bytes(b) => std::mem::take(b),
            Body::File { file, .. } => {
                // The handle may share its file offset with a clone the caller
                // already read (host.rs probes the html via try_clone), so seek
                // back to the start before reading the body.
                let _ = std::io::Seek::seek(file, std::io::SeekFrom::Start(0));
                let mut v = Vec::new();
                let _ = file.read_to_end(&mut v);
                v
            }
            Body::Stream { .. } => return,
        };
        match String::from_utf8(bytes) {
            Ok(mut s) => {
                let mut add = String::new();
                for (marker, html) in extra {
                    if !s.contains(marker) {
                        add.push_str(html);
                    }
                }
                if with_dev && self.dev && !s.contains("__VITE_RS_DEV") {
                    add.push_str(DEV_SNIPPET);
                }
                if !add.is_empty() {
                    if let Some(i) = s.find("<head>") {
                        s.insert_str(i + 6, &add);
                    } else {
                        s.insert_str(0, &add);
                    }
                }
                bytes = s.into_bytes();
            }
            Err(e) => bytes = e.into_bytes(),
        }
        out.headers
            .retain(|(k, _)| !k.eq_ignore_ascii_case("content-length"));
        out.headers
            .push(("Content-Length".into(), bytes.len().to_string()));
        out.headers
            .push(("Cache-Control".into(), "no-store".into()));
        out.body = Body::Bytes(bytes);
    }
}

fn snapshot_dir(root: &Path) -> Vec<(String, u128, u64)> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for e in rd.flatten() {
                let p = e.path();
                if let Ok(meta) = e.metadata() {
                    if meta.is_dir() {
                        stack.push(p);
                    } else {
                        let rel = p
                            .strip_prefix(root)
                            .unwrap_or(&p)
                            .display()
                            .to_string();
                        let mtime = meta
                            .modified()
                            .ok()
                            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                            .map(|d| d.as_nanos())
                            .unwrap_or(0);
                        out.push((rel, mtime, meta.len()));
                    }
                }
            }
        }
    }
    out.sort();
    out
}

fn snap_diff(a: &[(String, u128, u64)], b: &[(String, u128, u64)]) -> String {
    let mut out: Vec<String> = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        match a[i].0.cmp(&b[j].0) {
            std::cmp::Ordering::Less => {
                out.push(format!("-{}", a[i].0));
                i += 1;
            }
            std::cmp::Ordering::Greater => {
                out.push(format!("+{}", b[j].0));
                j += 1;
            }
            std::cmp::Ordering::Equal => {
                if a[i].1 != b[j].1 || a[i].2 != b[j].2 {
                    out.push(format!("~{}", b[j].0));
                }
                i += 1;
                j += 1;
            }
        }
    }
    while i < a.len() {
        out.push(format!("-{}", a[i].0));
        i += 1;
    }
    while j < b.len() {
        out.push(format!("+{}", b[j].0));
        j += 1;
    }
    if out.len() > 6 {
        out.truncate(6);
        out.push("...".into());
    }
    out.join(", ")
}

fn watcher_loop(state: Arc<DevState>) {
    let poll = std::time::Duration::from_millis(state.poll_ms.max(200));
    let mut www_snap = snapshot_dir(&state.www);
    type DirSnap = Vec<(String, u128, u64)>;
    let mut src_snaps: Vec<(PathBuf, DirSnap)> = state
        .watch
        .iter()
        .map(|d| (d.clone(), snapshot_dir(d)))
        .collect();
    state.append_log(&format!(
        "[dev-watch] watching www={} ({} files) src={:?} poll={}ms rebuild={:?}",
        state.www.display(),
        www_snap.len(),
        state.watch,
        state.poll_ms,
        state.rebuild_cmd
    ));
    loop {
        std::thread::sleep(poll);
        let now = snapshot_dir(&state.www);
        if now != www_snap {
            let changed = snap_diff(&www_snap, &now);
            www_snap = now;
            if !state.rebuilding.load(Ordering::Relaxed) {
                state.version.fetch_add(1, Ordering::Relaxed);
            }
            state.append_log(&format!(
                "[dev-watch] www changed: {} -> reload v{}",
                changed,
                state.version.load(Ordering::Relaxed)
            ));
        }
        // One rebuild per poll tick, on the first changed source dir.
        let changed = src_snaps.iter_mut().find_map(|(dir, snap)| {
            let now = snapshot_dir(dir);
            if *snap == now {
                return None;
            }
            let d = snap_diff(snap, &now);
            *snap = now;
            Some((dir.clone(), d))
        });
        let Some((sr, changed)) = changed else { continue };
        state.rebuilding.store(true, Ordering::Relaxed);
        match &state.rebuild_cmd {
            Some(cmd) => {
                state.append_log(&format!(
                    "[dev-watch] {} changed: {} — rebuilding: {cmd}",
                    sr.display(),
                    changed
                ));
                let started = std::time::Instant::now();
                let res = std::process::Command::new("sh")
                    .arg("-c")
                    .arg(cmd)
                    .current_dir(&state.rebuild_cwd)
                    .output();
                match res {
                    Ok(o) if o.status.success() => {
                        state.version.fetch_add(1, Ordering::Relaxed);
                        www_snap = snapshot_dir(&state.www); // avoid double bump
                        state.append_log(&format!(
                            "[dev-watch] rebuild OK ({}ms) -> reload v{}",
                            started.elapsed().as_millis(),
                            state.version.load(Ordering::Relaxed)
                        ));
                    }
                    Ok(o) => {
                        // Real errors are at the END of the build output.
                        let mut combined = String::from_utf8_lossy(&o.stdout).into_owned();
                        combined.push_str(&String::from_utf8_lossy(&o.stderr));
                        let tail: String = combined
                            .chars()
                            .rev()
                            .take(2000)
                            .collect::<String>()
                            .chars()
                            .rev()
                            .collect();
                        www_snap = snapshot_dir(&state.www);
                        state.append_log(&format!(
                            "[dev-watch] REBUILD FAILED ({}) — output tail:\n{}",
                            o.status, tail
                        ));
                    }
                    Err(e) => {
                        state.append_log(&format!("[dev-watch] rebuild spawn error: {e}"));
                    }
                }
            }
            None => {
                state.append_log(&format!(
                    "[dev-watch] {} changed: {} (no rebuild command configured)",
                    sr.display(),
                    changed
                ));
            }
        }
        state.rebuilding.store(false, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_endpoint_json() {
        let state = DevState::new(std::env::temp_dir().join("vite-rs-dev-test"));
        let dev = state.dev(true);
        let req = Request::get("/__dev/version");
        let out = dev.handle(&req).expect("dev endpoint");
        assert_eq!(out.status, 200);
        let body = match &out.body {
            Body::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
            _ => String::new(),
        };
        assert!(body.contains("\"v\":0"), "{body}");
        assert!(body.contains("\"rebuilding\":false"), "{body}");
    }

    #[test]
    fn log_post_appends_and_get_reads() {
        let log = std::env::temp_dir().join("vite-rs-log-test.ndjson");
        let _ = std::fs::remove_file(&log);
        let dev = DevState::new(std::env::temp_dir().join("vite-rs-dev-test"))
            .dev(true)
            .log_path(&log);
        let req = Request::post("/__dev/log", br#"{"kind":"error","message":"boom"}"#);
        let out = dev.handle(&req).expect("log post");
        assert_eq!(out.status, 204);
        let body = std::fs::read_to_string(&log).unwrap_or_default();
        assert!(body.contains("boom"), "{body}");
        let get = Request::get("/__dev/log");
        let out = dev.handle(&get).expect("log get");
        assert_eq!(out.status, 200);
        std::fs::remove_file(&log).ok();
    }

    #[test]
    fn dev_endpoints_disabled_in_prod() {
        let dev = DevState::new(std::env::temp_dir().join("vite-rs-dev-test"));
        assert!(dev.handle(&Request::get("/__dev/version")).is_none());
    }

    #[test]
    fn command_bridge_round_trip() {
        let dev = DevState::new(std::env::temp_dir().join("vite-rs-dev-test")).dev(true);
        // queue
        let req = Request::post("/__dev/command", br#"{"code":"document.title"}"#);
        let out = dev.handle(&req).expect("command queued");
        assert_eq!(out.status, 200);
        let body = match &out.body {
            Body::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
            _ => String::new(),
        };
        assert!(body.contains("\"id\""), "{body}");
        // browser picks it up
        let next = dev.handle(&Request::get("/__dev/command/next")).expect("next");
        let nb = match &next.body {
            Body::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
            _ => String::new(),
        };
        assert!(nb.contains("document.title"), "{nb}");
        assert!(nb.contains("\"id\":"), "id should exist");
        // nothing else queued
        let empty = dev.handle(&Request::get("/__dev/command/next")).expect("empty");
        let eb = match &empty.body {
            Body::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
            _ => String::new(),
        };
        assert_eq!(eb, "{}");
        // browser posts the result
        let res = dev
            .handle(&Request::post(
                "/__dev/command/result",
                br#"{"id":"1","ok":true,"result":"My Page"}"#,
            ))
            .expect("result stored");
        assert_eq!(res.status, 204);
        let got = dev
            .handle(&Request::get("/__dev/command/result?id=1"))
            .expect("result read");
        let gb = match &got.body {
            Body::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
            _ => String::new(),
        };
        assert!(gb.contains("\"ok\":true"), "{gb}");
        assert!(gb.contains("My Page"), "{gb}");
        // unknown id -> pending
        let pend = dev
            .handle(&Request::get("/__dev/command/result?id=zz"))
            .expect("pending");
        let pb = match &pend.body {
            Body::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
            _ => String::new(),
        };
        assert!(pb.contains("\"pending\":true"), "{pb}");
    }

    #[test]
    fn command_bridge_can_be_disabled() {
        let dev = DevState::new(std::env::temp_dir().join("vite-rs-dev-test"))
            .dev(true)
            .commands(false);
        let out = dev
            .handle(&Request::post("/__dev/command", br#"{"code":"1+1"}"#))
            .expect("disabled");
        assert_eq!(out.status, 403);
    }

    #[test]
    fn log_batch_array_is_split() {
        let log = std::env::temp_dir().join("vite-rs-batch-test.ndjson");
        let _ = std::fs::remove_file(&log);
        let dev = DevState::new(std::env::temp_dir().join("vite-rs-dev-test"))
            .dev(true)
            .log_path(&log);
        let req = Request::post(
            "/__dev/log",
            br#"[{"kind":"console","level":"warn","args":["a"]},{"kind":"route","url":"/x"}]"#,
        );
        let out = dev.handle(&req).expect("batch");
        assert_eq!(out.status, 204);
        let body = std::fs::read_to_string(&log).unwrap_or_default();
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 2, "{body}");
        assert!(lines[0].contains("\"warn\""), "{body}");
        assert!(lines[1].contains("\"route\""), "{body}");
        std::fs::remove_file(&log).ok();
    }
}
