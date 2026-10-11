//! `cvd serve` — a tiny local HTTP server exposing the fleet's state as JSON.
//!
//! A static browser dashboard polls these endpoints live, turning `cvd` into the fleet's live hub.
//! Point `--web <dir>` at the `web/` frontend and `cvd serve` *also* hosts that dashboard from `/`,
//! so a single command is a complete, self-contained hub (API at `/api/*`, UI everywhere else). We
//! deliberately use a lightweight *synchronous* server ([`tiny_http`]) with a small worker-thread
//! pool rather than an async stack — the workload is a handful of pollers, and simplicity wins.
//!
//! The transcript corpus is sensitive (it can contain secrets), so the API is locked to local
//! callers: the `Host` header must name this machine (defeats DNS rebinding), and CORS is echoed
//! only for an allow-list of local origins (the served dashboard, `localhost`/`127.0.0.1` dev
//! servers, and the Tauri app) — never `*`. An optional bearer token (`--token` / `$CVD_TOKEN`)
//! additionally gates every `/api/*` request; `OPTIONS` preflight is answered with a 204.

use anyhow::Result;
use cv_core::Harness;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tiny_http::{Header, Method, Request, Response, Server};

/// What one request handler needs to know beyond the request itself.
struct Ctx {
    /// The configured bind host, accepted in `Host`/`Origin` checks alongside the loopback names.
    bind_host: String,
    web_root: Option<PathBuf>,
    /// When set, every `/api/*` request must carry `Authorization: Bearer <token>`.
    token: Option<String>,
}

/// Run the HTTP server until the process is killed. Never returns under normal operation.
///
/// When `web_root` is `Some(dir)`, any request that doesn't match an `/api/*` route is served as a
/// static file from `dir` (with `index.html` for `/`), so the same process hosts both the JSON API
/// and the browser dashboard.
pub fn run(host: &str, port: u16, web_root: Option<PathBuf>, token: Option<String>) -> Result<()> {
    let server = Server::http((host, port)).map_err(|e| anyhow::anyhow!("failed to bind {host}:{port}: {e}"))?;
    // `--port 0` binds an ephemeral port; the banner below must carry the REAL bound port so
    // callers (test harnesses, scripts) can parse the address instead of racing to guess a free
    // one. For a fixed port this is the same number.
    let port = server.server_addr().to_ip().map_or(port, |a| a.port());
    // Canonicalize the web root once so per-request path-traversal checks are cheap and robust.
    let web_root = web_root.and_then(|d| match d.canonicalize() {
        Ok(c) => Some(c),
        Err(e) => {
            eprintln!("cvd serve: --web {} is unreadable ({e}); serving API only", d.display());
            None
        }
    });
    match &web_root {
        Some(dir) => eprintln!(
            "cvd serve on http://{host}:{port} — dashboard at /, API at /api/* (web root {})",
            dir.display()
        ),
        None => eprintln!("cvd serve on http://{host}:{port} — API at /api/* (no --web; JSON only)"),
    }

    let server = Arc::new(server);
    let ctx = Arc::new(Ctx {
        bind_host: host.to_string(),
        web_root,
        token,
    });
    let mut workers = Vec::new();
    // A handful of workers is plenty for dashboard polling; they share the listener.
    for _ in 0..4 {
        let server = Arc::clone(&server);
        let ctx = Arc::clone(&ctx);
        workers.push(std::thread::spawn(move || {
            for request in server.incoming_requests() {
                // A panicking handler must cost one response, not a worker for the process's life.
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handle(request, &ctx)));
                if outcome.is_err() {
                    eprintln!("cvd serve: request handler panicked; worker continuing");
                }
            }
        }));
    }
    for w in workers {
        // If a worker panics we keep serving on the others, but join here so `run` blocks forever.
        let _ = w.join();
    }
    Ok(())
}

/// Handle one request: any error becomes a JSON error response.
fn handle(request: Request, ctx: &Ctx) {
    // DNS-rebinding guard: a hostile page can point its own domain at 127.0.0.1 and fetch us
    // same-origin, but it can't forge the Host header — reject anything that isn't our own name.
    if !host_allowed(header_value(&request, "Host").as_deref(), &ctx.bind_host) {
        let _ = request.respond(json_response(403, &json!({"error": "forbidden host"}), None, &[]));
        return;
    }
    // Echoed on API responses only when the Origin is on the local allow-list; never `*`.
    let origin = allowed_origin(header_value(&request, "Origin").as_deref(), &ctx.bind_host);

    let method = request.method().clone();
    let raw_url = request.url().to_string();

    // CORS preflight: a 204, CORS-tagged only for allowed origins. Unauthenticated by design —
    // browsers strip credentials from preflights, and it discloses nothing.
    if method == Method::Options {
        let _ = request.respond(no_content(origin.as_deref()));
        return;
    }

    // Split off the query string, then normalise the path (strip trailing slash, decode segments).
    let (path, query) = match raw_url.split_once('?') {
        Some((p, q)) => (p, q),
        None => (raw_url.as_str(), ""),
    };
    let trimmed = path.trim_end_matches('/');
    let segments: Vec<String> = trimmed
        .split('/')
        .filter(|s| !s.is_empty())
        .map(cv_core::api::decode)
        .collect();

    // Only GET (besides the OPTIONS handled above) is supported.
    if method != Method::Get {
        let _ = request.respond(json_response(
            405,
            &json!({"error": "method not allowed"}),
            origin.as_deref(),
            &[],
        ));
        return;
    }

    // Anything outside `/api/*` is a static-dashboard request when a web root is configured.
    let is_api = segments.first().map(|s| s == "api").unwrap_or(false);
    if !is_api {
        if let Some(root) = &ctx.web_root {
            let _ = request.respond(static_file(root, &segments));
            return;
        }
    }

    if let Some(token) = &ctx.token {
        let authed = header_value(&request, "Authorization")
            .and_then(|v| v.strip_prefix("Bearer ").map(str::to_string))
            .is_some_and(|t| t == *token);
        if !authed {
            let _ = request.respond(json_response(
                401,
                &json!({"error": "missing or invalid bearer token"}),
                origin.as_deref(),
                &[],
            ));
            return;
        }
    }

    let (status, body, extra) = route(&segments, query);
    let _ = request.respond(json_response(status, &body, origin.as_deref(), &extra));
}

/// First value of a (case-insensitively named) request header, if present.
fn header_value(request: &Request, name: &'static str) -> Option<String> {
    request
        .headers()
        .iter()
        .find(|h| h.field.equiv(name))
        .map(|h| h.value.as_str().to_string())
}

/// The hostname part of a `Host` header or URL authority: strips a `:port`, unbrackets IPv6.
fn host_name(authority: &str) -> &str {
    match authority.strip_prefix('[') {
        Some(rest) => rest.split(']').next().unwrap_or(rest),
        None => authority.rsplit_once(':').map(|(name, _)| name).unwrap_or(authority),
    }
}

/// A name this server answers to: the loopback names, or the configured bind host.
fn is_local_name(name: &str, bind_host: &str) -> bool {
    name == "127.0.0.1" || name == "::1" || name.eq_ignore_ascii_case("localhost") || name == bind_host
}

/// Whether a configured bind host is a loopback name/address.
pub fn is_loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .trim_matches(['[', ']'])
            .parse::<std::net::IpAddr>()
            .map(|ip| ip.is_loopback())
            .unwrap_or(false)
}

/// Whether the `Host` header names this machine. An explicitly non-loopback bind is an opt-in to
/// remote clients whose Host we can't predict, so the check only bites on loopback binds.
fn host_allowed(host_header: Option<&str>, bind_host: &str) -> bool {
    if !is_loopback_host(bind_host) {
        return true;
    }
    host_header.is_some_and(|h| is_local_name(host_name(h), bind_host))
}

/// The specific `Origin` to echo in `Access-Control-Allow-Origin`, if it's on the allow-list:
/// the Tauri app's origins, or an http(s) origin on a local name (any port — a dev dashboard on
/// e.g. `http://localhost:5173` is same-machine).
fn allowed_origin(origin: Option<&str>, bind_host: &str) -> Option<String> {
    let o = origin?;
    if o == "tauri://localhost" || o == "http://tauri.localhost" {
        return Some(o.to_string());
    }
    let authority = o.strip_prefix("http://").or_else(|| o.strip_prefix("https://"))?;
    is_local_name(host_name(authority), bind_host).then(|| o.to_string())
}

/// A reply: status, JSON body, and any response headers beyond the standard set. Only `/api/search`
/// uses the third element today (to say which door answered — see [`search`]).
type Reply = (u16, Value, Vec<(&'static str, String)>);

/// Route a decoded path + raw query string. Never panics.
fn route(segments: &[String], query: &str) -> Reply {
    let parts: Vec<&str> = segments.iter().map(|s| s.as_str()).collect();
    // Routes that carry extra response headers answer here; everything else is header-free.
    if let ["api", "search"] = parts.as_slice() {
        return search(query);
    }
    if let ["api", "stats"] = parts.as_slice() {
        let (status, body) = stats(query);
        return (status, body, Vec::new());
    }
    // Every other noun is the one read API every door maps onto.
    let (status, body) = cv_core::api::route_json(&parts, query);
    (status, body, Vec::new())
}

// --- search & stats: the two things the CLI could do and the dashboard could not ------------

/// `cv search`'s default `--limit`. The doors agree on defaults too, or a caller gets a different
/// number of rows depending on which one it asked.
const SEARCH_LIMIT: usize = 20;

/// Resolve the query calculus's one external predicate the core can't (`text:`) against the tantivy
/// index — the daemon's copy of what `cv -q` does, and the only part of `-q` evaluation that isn't
/// shared code (`cv_core::query::filter_refs` is the rest).
fn text_sets(q: &cv_core::SessionQuery) -> cv_core::query::TextSets {
    use cv_core::query::{FieldId, TextSets};
    if q.needles(FieldId::Text).is_empty() {
        return TextSets::empty();
    }
    if !cv_search::default_tantivy_dir().exists() {
        eprintln!("cvd serve: text: needs a full-text index — run `cv index` (treating text: as no match)");
        return TextSets::resolve(q, |_| std::collections::HashSet::new());
    }
    TextSets::resolve(q, |n| {
        // A generous cap: we want the full matching set, not a top-K ranking.
        cv_search::text_search(None, n, 100_000)
            .map(|hits| hits.into_iter().map(|h| h.id).collect())
            .unwrap_or_default()
    })
}

/// `GET /api/stats?q=<query-calculus>` — corpus-wide fleet statistics: exactly what `cv stats
/// --json` prints (`sessions, messages, by_harness, top_cwds, earliest_created, latest_updated`),
/// computed by [`cv_core::stats::CorpusStats`] so the two doors cannot drift.
///
/// Why this exists: the dashboard's Stats view used to accumulate its totals from whichever
/// sessions the user had happened to click, and had to confess it ("…reflect the 1 opened session
/// — 6,872 not yet loaded"). The numbers are catalog-cheap for the whole fleet; there was never a
/// reason to guess at them.
///
/// The optional `q` is cv's query calculus (`cv schema`), the same string `cv stats -q` takes; a
/// parse error is a 400 quoting the core's own message.
fn stats(query: &str) -> (u16, Value) {
    let params = Query::parse(query);
    let q = match params.get("q") {
        Some(s) => match cv_core::SessionQuery::parse(&s) {
            Ok(q) => Some(q),
            // The core points at the reference by its 0.10 name; re-point it like the CLI does.
            Err(e) => return err(400, &e.replace("`cv query`", "`cv schema`")),
        },
        None => None,
    };
    let mut refs = cv_core::sessions();
    if let Some(q) = &q {
        cv_core::query::filter_refs(&mut refs, q, &text_sets(q));
    }
    ok(cv_core::stats::CorpusStats::compute(&refs).to_json())
}

/// `GET /api/search?q=&limit=&harness=&cwd=&semantic=1` — full-text (or `semantic=1`) search over
/// the whole corpus, as the JSON array of rows `cv search --json` emits: the §3 session row plus
/// `score`, `snippet` and the sub-agent provenance trio, shaped by [`cv_core::rows::SearchRow`].
///
/// Why this exists: the dashboard's search box could only filter the session stubs it had already
/// downloaded, so it could not find a message it had not fetched — while `cv search` answers over
/// the whole archive instantly off the tantivy index `cv index` maintains. Same index, same rows,
/// through the other door.
///
/// **Degradation, and saying so.** With no index (or an unreadable one) this does not error — it
/// takes the same head-capped live scan `cv search` falls back to ([`cv_core::scan::live_search`]),
/// which is slower and only sees each session's capped head. Which path answered rides on
/// `X-Cv-Search-Source` (`index` | `semantic` | `live`), with a human sentence in `X-Cv-Search-Note`
/// whenever the answer is degraded or truncated, so the UI can tell the user rather than silently
/// showing a worse result. Both headers are CORS-exposed. The body stays exactly the CLI's array.
fn search(query: &str) -> Reply {
    let params = Query::parse(query);
    // An empty query would mean "every session", which is `/api/sessions`' job; searching for
    // nothing is a caller mistake, not an empty result set.
    let Some(q) = params.get("q") else {
        let (s, v) = err(400, "missing q parameter");
        return (s, v, Vec::new());
    };
    // Validate harness up front so a typo is a clear 400 rather than silently empty.
    let want = match params.get("harness") {
        Some(h) => match Harness::parse(&h) {
            Some(h) => Some(h),
            None => {
                let (s, v) = err(400, &format!("unknown harness {h:?}"));
                return (s, v, Vec::new());
            }
        },
        None => None,
    };
    let limit = params
        .get("limit")
        .and_then(|l| l.parse::<usize>().ok())
        .unwrap_or(SEARCH_LIMIT);
    let cwd = params.get("cwd");
    let semantic = params
        .get("semantic")
        .is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true"));

    // Same fetch-then-filter shape as `cv search`: ask the index for 4× the wanted rows so the
    // harness filter has something to cut into, then take `limit`.
    let fetch = limit.saturating_mul(4);
    let (hits, source, mut note) = if semantic {
        match cv_search::semantic_search(None, &q, fetch) {
            Ok(hits) => (hits, "semantic", None),
            Err(e) => {
                // A live scan cannot stand in for semantic ranking, so this is an error, exactly
                // as `cv search --semantic` exits non-zero rather than quietly changing mode.
                let (s, v) = err(
                    500,
                    &format!("semantic search failed (run `cv index --semantic` first?): {e:#}"),
                );
                return (s, v, Vec::new());
            }
        }
    } else if cv_search::default_tantivy_dir().exists() {
        // The index is authoritative when present — an empty result means "no match", never
        // "fall back to a live scan".
        match cv_search::text_search(None, &q, fetch) {
            Ok(hits) => (hits, "index", None),
            Err(e) => (
                Vec::new(),
                "live",
                Some(format!("tantivy index unavailable ({e:#}); scanned live")),
            ),
        }
    } else {
        (
            Vec::new(),
            "live",
            Some("no index yet — scanned live; run `cv index` for instant, full-transcript search".into()),
        )
    };

    let mut rows: Vec<Value> = if source == "live" {
        match cv_core::scan::live_search(&q, want, limit) {
            Ok(found) => {
                if found.truncated {
                    note = Some(format!(
                        "{} (stopped at {limit} hits; raise limit for more)",
                        note.unwrap_or_default()
                    ));
                }
                found
                    .hits
                    .iter()
                    .map(|h| cv_core::rows::live_search_row(&h.session, &h.title, &h.snippet))
                    .collect()
            }
            Err(e) => {
                let (s, v) = err(500, &format!("live scan failed: {e:#}"));
                return (s, v, Vec::new());
            }
        }
    } else {
        hits.iter()
            .filter(|h| want.is_none_or(|w| h.harness == w.as_str()))
            .map(|h| {
                cv_core::rows::SearchRow {
                    id: &h.id,
                    harness: &h.harness,
                    cwd: h.cwd.as_deref(),
                    title: h.title.as_deref(),
                    created_at: h.created_at,
                    updated_at: h.updated_at,
                    score: Some(h.score),
                    snippet: &h.snippet,
                    agent_id: h.agent_id.as_deref(),
                    parent_id: h.parent_id.as_deref(),
                    workflow: h.workflow.as_deref(),
                }
                .to_json()
            })
            .collect()
    };
    // `cwd` is the daemon's own filter (the CLI has no `search --cwd`): a substring of the row's
    // cwd as the caller will see it, matching `/api/sessions?cwd=`. A row with no cwd never matches.
    if let Some(needle) = &cwd {
        rows.retain(|r| r["cwd"].as_str().is_some_and(|c| c.contains(needle.as_str())));
    }
    rows.truncate(limit);

    // The index is authoritative, so an empty result means "no match" — but the two reasons a real
    // conversation is unfindable are both about the index, not the query, and `cv search` says so
    // on a miss. Same note here, and only on a miss (the staleness check costs a metadata-only
    // discovery sweep, which is not something to pay on every hit).
    if source == "index" && rows.is_empty() {
        note = index_miss_note();
    }

    let mut headers = vec![("X-Cv-Search-Source", source.to_string())];
    if let Some(n) = note {
        headers.push(("X-Cv-Search-Note", header_safe(&n)));
    }
    (200, json!(rows), headers)
}

/// Why an indexed search might have come back empty for something that really was said — the same
/// two hints `cv search` prints on a miss: the index has fallen behind the newest session, and/or
/// it holds no sub-agent transcripts (so nothing an Agent/Workflow lane discussed is searchable).
fn index_miss_note() -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if let Some(days) = index_days_behind() {
        parts.push(format!(
            "the index is ~{days} day(s) behind the newest session — run `cv index`"
        ));
    }
    if !cv_search::fts::has_subagent_docs(&cv_search::default_tantivy_dir()) {
        parts.push(
            "sub-agent transcripts aren't searched yet — rebuild with `cv index --subagents` to \
             include the Agent/Workflow lanes"
                .into(),
        );
    }
    (!parts.is_empty()).then(|| parts.join("; "))
}

/// Whole days the FTS index lags the newest session file on disk, when ≥ 1: one stored-field scan
/// of the index plus a metadata-only discovery sweep (a stat per session, no parsing).
fn index_days_behind() -> Option<u64> {
    const DAY_NS: i64 = 86_400 * 1_000_000_000;
    let indexed = cv_search::fts::newest_indexed_mtime(&cv_search::default_tantivy_dir())?;
    let newest_on_disk = cv_core::discover_all()
        .iter()
        .map(|r| cv_core::offsets::file_sig(&r.path).0)
        .max()?;
    let days = newest_on_disk.saturating_sub(indexed) / DAY_NS;
    (days >= 1).then_some(days as u64)
}

/// A header-safe rendering of a note: header values are a byte channel with no escaping, so keep
/// them printable ASCII on one line (`tiny_http` refuses anything else, which would drop the note).
fn header_safe(s: &str) -> String {
    s.chars()
        .map(|c| if (' '..='~').contains(&c) { c } else { ' ' })
        .collect::<String>()
        .trim()
        .to_string()
}

// --- small helpers ---------------------------------------------------------

use cv_core::api::Query;

fn ok(value: Value) -> (u16, Value) {
    (200, value)
}

/// Serve a static file from the configured web root for a non-`/api` request.
///
/// `segments` is the already-decoded path (so `["web", "components", "cv-forest.js"]`). An empty
/// path maps to `index.html`. The resolved path is canonicalized and must stay inside the (already
/// canonical) root — any traversal (`..`, symlink escape, absolute injection) yields a 403. A
/// missing file falls back to `index.html` (so client-side deep links work), and only a missing
/// index is a 404. The `Content-Type` is inferred from the extension; static assets are not
/// CORS-tagged (same-origin) — only the JSON API is.
fn static_file(root: &Path, segments: &[String]) -> Response<std::io::Cursor<Vec<u8>>> {
    let index = root.join("index.html");
    let resolved = if segments.is_empty() {
        index.clone()
    } else {
        // Reject obviously hostile components before touching the filesystem.
        if segments.iter().any(|s| s == ".." || s.contains('\0')) {
            return text_response(403, "text/plain; charset=utf-8", b"forbidden".to_vec());
        }
        let mut p = root.to_path_buf();
        for s in segments {
            p.push(s);
        }
        p
    };

    // A deep link is a ROUTE and falls back to the SPA index; a missing ASSET is a 404. Falling
    // back for an asset hands the browser `index.html` under the asset's name, and for a module
    // script that surfaces as "Expected a JavaScript-or-Wasm module script but the server
    // responded with a MIME type of text/html" — which reads like a server misconfiguration
    // rather than the truth, that the file is not there. (`/pkg/cv_web.js`, the optional wasm
    // bundle, is not built unless you run wasm-pack.)
    let is_asset = resolved
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| !e.eq_ignore_ascii_case("html"));

    let serve_path = match resolved.canonicalize() {
        Ok(c) if c.starts_with(root) && c.is_file() => c,
        _ if is_asset => return text_response(404, "text/plain; charset=utf-8", b"not found".to_vec()),
        _ => match index.canonicalize() {
            Ok(c) if c.starts_with(root) && c.is_file() => c,
            _ => return text_response(404, "text/plain; charset=utf-8", b"not found".to_vec()),
        },
    };

    match std::fs::read(&serve_path) {
        Ok(bytes) => text_response(200, content_type_for(&serve_path), bytes),
        Err(_) => text_response(404, "text/plain; charset=utf-8", b"not found".to_vec()),
    }
}

/// Guess a `Content-Type` from a file extension — enough for the dashboard's asset set.
fn content_type_for(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("html") => "text/html; charset=utf-8",
        Some("js") | Some("mjs") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json") => "application/json; charset=utf-8",
        Some("wasm") => "application/wasm",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("ico") => "image/x-icon",
        Some("woff2") => "font/woff2",
        Some("woff") => "font/woff",
        Some("map") => "application/json; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// A raw-bytes response with a `Content-Type` and no CORS headers (static assets are same-origin).
fn text_response(status: u16, content_type: &str, body: Vec<u8>) -> Response<std::io::Cursor<Vec<u8>>> {
    let mut resp = Response::from_data(body).with_status_code(status);
    resp.add_header(header("Content-Type", content_type));
    resp
}

fn err(status: u16, msg: &str) -> (u16, Value) {
    (status, json!({ "error": msg }))
}

/// Build a JSON response, CORS-tagged for `origin` when one was allowed, plus any route-specific
/// headers (see [`EXPOSED_HEADERS`] — a header a cross-origin dashboard cannot read is not a
/// channel).
fn json_response(
    status: u16,
    body: &Value,
    origin: Option<&str>,
    extra: &[(&'static str, String)],
) -> Response<std::io::Cursor<Vec<u8>>> {
    let data = serde_json::to_vec(body).unwrap_or_else(|_| b"{\"error\":\"serialize failed\"}".to_vec());
    let mut resp = Response::from_data(data).with_status_code(status);
    for h in cors_headers(origin) {
        resp.add_header(h);
    }
    for (field, value) in extra {
        resp.add_header(header(field, value));
    }
    resp.add_header(header("Content-Type", "application/json"));
    resp
}

/// A 204 No Content, CORS-tagged for `origin` when one was allowed — for `OPTIONS` preflight.
fn no_content(origin: Option<&str>) -> Response<std::io::Empty> {
    let mut resp = Response::empty(204);
    for h in cors_headers(origin) {
        resp.add_header(h);
    }
    resp
}

/// Response headers a cross-origin caller is allowed to READ. A browser hides every non-safelisted
/// response header from `fetch` unless it is named here, so `/api/search`'s "this answer is
/// degraded" signal would be invisible to a dashboard on a dev server without it.
const EXPOSED_HEADERS: &str = "X-Cv-Search-Source, X-Cv-Search-Note";

/// CORS headers echoing the one allowed `origin` — an unlisted origin gets none (the browser then
/// refuses to share the response with it). `Vary: Origin` keeps caches from mixing the two.
fn cors_headers(origin: Option<&str>) -> Vec<Header> {
    let mut headers = vec![header("Vary", "Origin")];
    if let Some(o) = origin {
        headers.push(header("Access-Control-Allow-Origin", o));
        headers.push(header("Access-Control-Allow-Methods", "GET, OPTIONS"));
        headers.push(header("Access-Control-Allow-Headers", "Authorization, Content-Type"));
        headers.push(header("Access-Control-Expose-Headers", EXPOSED_HEADERS));
    }
    headers
}

fn header(field: &str, value: &str) -> Header {
    // These are all static, valid headers; the parse cannot fail in practice.
    Header::from_bytes(field.as_bytes(), value.as_bytes())
        .unwrap_or_else(|_| Header::from_bytes(&b"X-Invalid"[..], &b"1"[..]).unwrap())
}
