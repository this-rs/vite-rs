# vite-rs

A tiny, dependency-light (**httparse + serde_json**) Rust library for hosting a
static SPA with a **Vite-like dev experience** — built to be the best
front-end server for an **AI agent**:

- **Hot reload** — the injected page script polls `/__dev/version` and
  `location.reload()`s when the served build changes.
- **Rebuild on change** — a polling file watcher runs your bundler command
  when watched source dirs change, bumps the reload version on success, and
  logs build failures with the real error (tail of the output).
- **AI command bridge** — an agent can queue arbitrary JavaScript, have it
  executed inside the real browser, and read back the JSON result:
  `POST /__dev/command` → browser runs it → `GET /__dev/command/result?id=…`.
  This is the equivalent of DevTools Console + evaluate, remotely.
- **Console capture** — `log/info/warn/error/debug/trace` with
  JSON-serializable arguments and stack traces, posted to `/__dev/log`.
- **Network capture** — every `fetch`/`XHR` with status, content-type, timing
  and the **initiator stack** (which code path triggered the request).
- **Window errors** — `window.onerror` / `unhandledrejection` with stacks.
- **SPA route events** — `pushState`/`replaceState`/`popstate`/hashchange.
- **ETag/304 caching** and MIME detection, hashed assets accepted both under
  `assets/` and at the web root.

## Debugging from an agent (the bridge)

The injected script (dev mode) turns the browser into a queryable runtime.
Everything lands in the dev log (NDJSON, also echoed to stderr):

```bash
# watch the browser
tail -f /tmp/myapp-dev.log

# run JS in the browser and read the result
curl -s -X POST localhost:8080/__dev/command \
  -H 'content-type: application/json' -d '{"code":"document.title"}'
# → {"id":"1"}
curl -s localhost:8080/__dev/command/next      # the browser polls this
curl -s 'localhost:8080/__dev/command/result?id=1'
# → {"id":"1","ok":true,"result":"My App"}
```

Useful one-liners the agent can evaluate:

```js
// page state
({title: document.title, url: location.href, ready: document.readyState,
  scripts: document.scripts.length, inputs: document.querySelectorAll('input').length})
// resources (network waterfall)
performance.getEntriesByType('resource').map(e => ({n: e.name.slice(0,120), d: Math.round(e.duration)}))
// storage
({local: Object.keys(localStorage), session: Object.keys(sessionStorage)})
// a DOM subtree as text
document.querySelector('#app').innerText.slice(0, 500)
```

The bridge is disabled by default in untrusted deployments — see
`DevState::commands(false)`.

## Dev endpoints

| Endpoint | Purpose |
|---|---|
| `GET /__dev` | dev info + log file path |
| `GET /__dev/version` | `{"v":N,"rebuilding":bool}` — polled by the injected script |
| `POST /__dev/log` | event capture: console, errors, network, routes (single event or JSON array batch) |
| `GET /__dev/log` | read the dev log |
| `POST /__dev/command` | queue JS for the browser (`{"code":…}` → `{"id":…}`) |
| `GET /__dev/command/next` | polled by the browser; pops the next queued command |
| `POST /__dev/command/result` | the browser posts `{"id", "ok", "result"}` |
| `GET /__dev/command/result?id=…` | the agent reads the result back |

Logs are also written to stderr (non-panicking — safe under a killed pipe).

## License

MIT
