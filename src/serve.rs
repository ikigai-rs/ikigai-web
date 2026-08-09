//! The HTTP face: parse a request, resolve it through the kernel, project the
//! result — status from the typed error, media type from the representation,
//! `Cache-Control` from its expiry.
//!
//! Hand-rolled HTTP/1.1 over Tokio (the `ikigai-cms-web` / workspace
//! `ikigai-web` precedent): one request per connection, `Connection: close`,
//! bounded header and body reads. Small enough to audit; a loopback-only
//! browse face needs nothing more.

use std::sync::Arc;
use std::time::Duration;

use ikigai_core::{ArgRef, Capability, Error, Expiry, Iri, Kernel, Request, Verb};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Largest accepted request head (request line + headers).
const MAX_HEAD: usize = 64 * 1024;
/// Largest accepted body (annotations are small).
const MAX_BODY: usize = 1024 * 1024;
/// How long a connection may take to deliver its request.
const READ_BUDGET: Duration = Duration::from_secs(30);

/// What the bind address implies about who is on the other end — and therefore
/// which surface this face offers.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Posture {
    /// Loopback bind: the local owner (the same trust the dev socket's
    /// peer-credential check extends). The full v1 surface, including the one
    /// write route (annotation minting).
    LocalOwner,
    /// Non-loopback bind: anyone on the network. The write surface is GONE,
    /// not gated — every request that is not GET/HEAD is refused before
    /// dispatch, except `POST /sparql`, whose body is a QUERY (that face is
    /// read-only by its own construction; see [`crate::sparql`]).
    ReadOnly,
}

impl Posture {
    /// The posture the listener's ACTUAL bound address earns. Derived from the
    /// socket, not the config string — the config is intent, the socket is
    /// truth.
    pub fn of(listener: &TcpListener) -> std::io::Result<Posture> {
        Ok(if listener.local_addr()?.ip().is_loopback() {
            Posture::LocalOwner
        } else {
            Posture::ReadOnly
        })
    }
}

/// Bind the listener. Separate from [`serve`] so a caller (and the tests) can
/// learn the bound address BEFORE requests race the accept loop. The default
/// address is loopback; a non-loopback address puts [`serve`] in
/// [`Posture::ReadOnly`] — see the crate doc's trust posture.
pub async fn bind(addr: std::net::SocketAddr) -> std::io::Result<TcpListener> {
    TcpListener::bind(addr).await
}

/// Accept forever, one task per connection.
///
/// The posture is derived HERE, from the listener itself — not passed in — so
/// a non-loopback listener with a live write surface cannot be constructed:
/// there is no parameter to get wrong. If the socket cannot even report its
/// address, the server refuses to start rather than guess.
pub async fn serve(kernel: Arc<Kernel>, listener: TcpListener) -> ! {
    let posture = Posture::of(&listener)
        .expect("refusing to serve: cannot read the bound address to derive the trust posture");
    loop {
        if let Ok((stream, _peer)) = listener.accept().await {
            let kernel = Arc::clone(&kernel);
            tokio::spawn(async move {
                let _ = handle(kernel, stream, posture).await;
            });
        }
    }
}

/// One connection: read a request (bounded, within a time budget), respond,
/// close.
async fn handle(
    kernel: Arc<Kernel>,
    mut stream: TcpStream,
    posture: Posture,
) -> std::io::Result<()> {
    let parsed = tokio::time::timeout(READ_BUDGET, read_request(&mut stream)).await;
    let resp = match parsed {
        Ok(Ok(req)) => respond(&kernel, req, posture).await,
        Ok(Err(status)) => error_resp(status, "malformed request"),
        Err(_elapsed) => error_resp(408, "request read timed out"),
    };
    write_response(&mut stream, resp).await
}

/// A parsed request — just what the browse face needs.
pub(crate) struct HttpRequest {
    pub(crate) method: String,
    /// Percent-DECODED path, no query.
    pub(crate) path: String,
    /// Percent-decoded query pairs, in order.
    pub(crate) query: Vec<(String, String)>,
    /// Lowercased header names.
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) body: Vec<u8>,
}

impl HttpRequest {
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

/// The response under assembly.
pub(crate) struct Resp {
    pub(crate) status: u16,
    /// (name, value) — `Content-Length`, `Connection` and the standard hygiene
    /// headers are added at write time.
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) body: Vec<u8>,
    /// HEAD and 304-family responses send headers only.
    pub(crate) suppress_body: bool,
}

pub(crate) fn error_resp(status: u16, detail: &str) -> Resp {
    Resp {
        status,
        headers: vec![
            (
                "Content-Type".to_string(),
                "text/plain; charset=utf-8".to_string(),
            ),
            ("Cache-Control".to_string(), "no-store".to_string()),
        ],
        body: format!("{detail}\n").into_bytes(),
        suppress_body: false,
    }
}

/// Read and parse one HTTP/1.1 request from the stream. `Err(status)` is the
/// HTTP status the failure deserves.
async fn read_request(stream: &mut TcpStream) -> Result<HttpRequest, u16> {
    let mut buf: Vec<u8> = Vec::with_capacity(1024);
    let head_end = loop {
        if let Some(pos) = find_head_end(&buf) {
            break pos;
        }
        if buf.len() > MAX_HEAD {
            return Err(431u16);
        }
        let mut chunk = [0u8; 4096];
        let n = stream.read(&mut chunk).await.map_err(|_| 400u16)?;
        if n == 0 {
            return Err(400u16);
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = std::str::from_utf8(&buf[..head_end]).map_err(|_| 400u16)?;
    let mut lines = head.split("\r\n");
    let request_line = lines.next().ok_or(400u16)?;
    let mut parts = request_line.split(' ');
    let method = parts.next().ok_or(400u16)?.to_string();
    let target = parts.next().ok_or(400u16)?;
    let (raw_path, raw_query) = match target.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (target, None),
    };
    let path = percent_decode(raw_path, false).ok_or(400u16)?;
    let query = raw_query.map(parse_query).unwrap_or_default();
    let mut headers = Vec::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
        }
    }
    let content_length: usize = headers
        .iter()
        .find(|(k, _)| k == "content-length")
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
    if content_length > MAX_BODY {
        return Err(413u16);
    }
    let mut body = buf[head_end + 4..].to_vec();
    while body.len() < content_length {
        let mut chunk = [0u8; 8192];
        let n = stream.read(&mut chunk).await.map_err(|_| 400u16)?;
        if n == 0 {
            return Err(400u16);
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(content_length);
    Ok(HttpRequest {
        method,
        path,
        query,
        headers,
        body,
    })
}

fn find_head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

/// Decode `%XX` escapes; with `plus_is_space`, `+` decodes to a space (the
/// form/query convention). `None` on a bad escape or non-UTF-8 result.
pub fn percent_decode(s: &str, plus_is_space: bool) -> Option<String> {
    let mut out: Vec<u8> = Vec::with_capacity(s.len());
    let mut bytes = s.bytes();
    while let Some(b) = bytes.next() {
        match b {
            b'%' => {
                let hi = bytes.next()?;
                let lo = bytes.next()?;
                let hex = |c: u8| (c as char).to_digit(16);
                out.push((hex(hi)? * 16 + hex(lo)?) as u8);
            }
            b'+' if plus_is_space => out.push(b' '),
            other => out.push(other),
        }
    }
    String::from_utf8(out).ok()
}

/// Split a query (or form body) into decoded pairs; pairs that fail to decode
/// are dropped rather than misread.
pub(crate) fn parse_query(raw: &str) -> Vec<(String, String)> {
    raw.split('&')
        .filter(|piece| !piece.is_empty())
        .filter_map(|piece| {
            let (k, v) = piece.split_once('=').unwrap_or((piece, ""));
            Some((percent_decode(k, true)?, percent_decode(v, true)?))
        })
        .filter(|(k, _)| !k.is_empty())
        .collect()
}

/// The `as=` media type the `Accept` header asks for, if any. First recognized
/// type in header order wins; `*/*` (and anything unrecognized) means "the
/// endpoint's default face". A browser's default `Accept` leads with
/// `text/html`, which is exactly the face it should get.
pub fn accept_face(accept: &str) -> Option<&'static str> {
    for item in accept.split(',') {
        let media = item.split(';').next().unwrap_or("").trim();
        match media {
            "text/html" | "application/xhtml+xml" => return Some("text/html"),
            "text/turtle" => return Some("text/turtle"),
            "application/ld+json" => return Some("application/ld+json"),
            "application/json" => return Some("application/json"),
            "text/plain" => return Some("text/plain"),
            _ => continue,
        }
    }
    None
}

/// Project a representation's expiry onto `Cache-Control`, honestly:
/// uncacheable → `no-store`; a deadline → `max-age`; permanent → `immutable`.
/// There is no ETag until golden-thread validity crosses the wire (crate doc).
pub fn cache_control(expiry: Expiry) -> String {
    match expiry {
        Expiry::Always => "no-store".to_string(),
        Expiry::At(deadline) => {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            let secs = deadline.as_millis().saturating_sub(now) / 1000;
            format!("max-age={secs}")
        }
        Expiry::Never => "public, max-age=31536000, immutable".to_string(),
    }
}

/// Map a kernel error onto the status its meaning deserves.
pub fn status_of(error: &Error) -> u16 {
    match error {
        Error::Unresolved(_) | Error::NotFound(_) => 404,
        Error::Denied(_) => 403,
        Error::MissingArgument(_) | Error::InvalidArgument { .. } => 400,
        Error::Unavailable(_) => 503,
        Error::Timeout(_) => 504,
        _ => 500,
    }
}

/// The one write route v1 exposes: annotation minting (`urn:annotation`) and
/// slug-addressed annotation writes (`urn:annotation:{id}`).
fn post_allowed(uri: &str) -> bool {
    uri == "urn:annotation" || uri.starts_with("urn:annotation:")
}

/// Dispatch one parsed request against the kernel.
async fn respond(kernel: &Kernel, req: HttpRequest, posture: Posture) -> Resp {
    // The read-only gate — ONE choke point, before any route parsing, so no
    // later route (today's or a future one) can widen the surface by
    // forgetting a check. `POST /sparql` passes: its body is a query, and that
    // face rejects update forms itself before the kernel ever sees them.
    if posture == Posture::ReadOnly
        && req.method != "GET"
        && req.method != "HEAD"
        && !(req.method == "POST" && req.path == "/sparql")
    {
        let mut resp = error_resp(
            403,
            "read-only: this server is bound beyond loopback, so the write surface \
             is disabled (GET/HEAD, plus /sparql queries)",
        );
        resp.headers
            .push(("Allow".to_string(), "GET, HEAD".to_string()));
        return resp;
    }
    if req.path == "/" {
        return index(kernel).await;
    }
    if req.path == "/htmx.min.js" {
        return htmx_js();
    }
    if req.path == "/sparql" {
        return crate::sparql::respond(kernel, &req).await;
    }
    if let Some(command) = req.path.strip_prefix("/k/") {
        return k_command(kernel, &req, command.to_string()).await;
    }
    if let Some(start) = req.path.strip_prefix("/browse/") {
        return browse_shell(start, posture);
    }
    let uri = &req.path[1..]; // drop the leading `/`
    let Ok(target) = Iri::parse(uri.to_string()) else {
        return error_resp(404, &format!("`{uri}` is not a resolvable IRI"));
    };
    if !uri.starts_with("urn:") {
        return error_resp(
            404,
            &format!("`{uri}`: this face serves urn:* resources (e.g. /urn:repo:...)"),
        );
    }
    match req.method.as_str() {
        "GET" => get(kernel, &req, target).await,
        "HEAD" => head(kernel, target).await,
        "POST" => post(kernel, &req, target).await,
        _ => {
            let mut resp = error_resp(405, "method not supported");
            resp.headers
                .push(("Allow".to_string(), "GET, HEAD, POST".to_string()));
            resp
        }
    }
}

/// GET → Source. Query args pass through as invocation args; the `Accept`
/// header (or an explicit `?as=`) selects the face.
async fn get(kernel: &Kernel, req: &HttpRequest, target: Iri) -> Resp {
    let mut request = Request::new(Verb::Source, target);
    let mut explicit_as = false;
    for (k, v) in &req.query {
        explicit_as |= k == "as";
        request = request.with_arg(k.clone(), ArgRef::Inline(v.clone().into_bytes()));
    }
    if !explicit_as {
        if let Some(face) = req.header("accept").and_then(accept_face) {
            request = request.with_arg("as", ArgRef::Inline(face.as_bytes().to_vec()));
        }
    }
    match kernel.issue(request, &Capability::root()).await {
        Ok(repr) => {
            let headers = vec![
                ("Content-Type".to_string(), content_type(&repr.repr_type)),
                ("Cache-Control".to_string(), cache_control(repr.expiry)),
            ];
            Resp {
                status: 200,
                headers,
                body: repr.bytes,
                suppress_body: false,
            }
        }
        Err(e) => error_resp(status_of(&e), &e.to_string()),
    }
}

/// HEAD → Exists: 200 (no body) when the resource reports `true`, else 404.
/// This is the ROC-honest map — an existence probe, not a body-less GET.
async fn head(kernel: &Kernel, target: Iri) -> Resp {
    let request = Request::new(Verb::Exists, target);
    let mut resp = match kernel.issue(request, &Capability::root()).await {
        Ok(repr) => {
            if repr.bytes.trim_ascii() == b"true" {
                error_resp(200, "")
            } else {
                error_resp(404, "")
            }
        }
        Err(e) => error_resp(status_of(&e), ""),
    };
    resp.body.clear();
    resp.suppress_body = true;
    resp
}

/// POST → Sink, on the annotation family only. A form-encoded body maps to
/// invocation args (the htmx overlay's shape); any other body is the piped
/// `content`, with its Content-Type surfaced as the `content-type` arg. Query
/// args pass through too; body fields win on collision.
async fn post(kernel: &Kernel, req: &HttpRequest, target: Iri) -> Resp {
    let uri = target.as_str();
    if !post_allowed(uri) {
        let mut resp = error_resp(
            405,
            "v1 accepts POST only for annotation minting (/urn:annotation)",
        );
        resp.headers
            .push(("Allow".to_string(), "GET, HEAD".to_string()));
        return resp;
    }
    let mut args: Vec<(String, Vec<u8>)> = req
        .query
        .iter()
        .map(|(k, v)| (k.clone(), v.clone().into_bytes()))
        .collect();
    let content_type_hdr = req.header("content-type").unwrap_or("");
    if content_type_hdr.starts_with("application/x-www-form-urlencoded") {
        let Ok(body) = std::str::from_utf8(&req.body) else {
            return error_resp(400, "form body is not UTF-8");
        };
        for (k, v) in parse_query(body) {
            args.retain(|(name, _)| name != &k);
            args.push((k, v.into_bytes()));
        }
    } else if !req.body.is_empty() {
        args.retain(|(name, _)| name != "content");
        args.push(("content".to_string(), req.body.clone()));
        if !content_type_hdr.is_empty() {
            args.retain(|(name, _)| name != "content-type");
            args.push((
                "content-type".to_string(),
                content_type_hdr.as_bytes().to_vec(),
            ));
        }
    }
    let mut request = Request::new(Verb::Sink, target);
    for (k, v) in args {
        request = request.with_arg(k, ArgRef::Inline(v));
    }
    match kernel.issue(request, &Capability::root()).await {
        Ok(repr) => Resp {
            status: 200,
            headers: vec![
                ("Content-Type".to_string(), content_type(&repr.repr_type)),
                ("Cache-Control".to_string(), "no-store".to_string()),
            ],
            body: repr.bytes,
            suppress_body: false,
        },
        Err(e) => error_resp(status_of(&e), &e.to_string()),
    }
}

/// The `/k/` HOST ADAPTER the browse family's HTML faces are authored against:
/// every affordance they emit is `hx-get="/k/source <iri> [k=v ...]"` or
/// `hx-post="/k/sink urn:annotation"` (see `ikigai-browse`, which calls this
/// "the HOST's /k/ adapter"). The command is REPL-ish but deliberately tiny:
/// `source` (GET) and `sink` (POST, annotation family only — the same one
/// write route as POST `/urn:annotation…`). Nothing else; the adapter never
/// widens the face's verb surface.
async fn k_command(kernel: &Kernel, req: &HttpRequest, command: String) -> Resp {
    let mut tokens = command.split_whitespace();
    let (Some(verb_word), Some(iri)) = (tokens.next(), tokens.next()) else {
        return error_resp(400, "expected /k/<source|sink> <iri> [k=v ...]");
    };
    let Ok(target) = Iri::parse(iri.to_string()) else {
        return error_resp(400, &format!("`{iri}` is not a resolvable IRI"));
    };
    let mut args: Vec<(String, String)> = Vec::new();
    for token in tokens {
        let Some((k, v)) = token.split_once('=') else {
            return error_resp(400, &format!("`{token}`: command args are k=v"));
        };
        args.push((k.to_string(), v.to_string()));
    }
    match (req.method.as_str(), verb_word) {
        ("GET", "source") => {
            let mut request = Request::new(Verb::Source, target);
            for (k, v) in args {
                request = request.with_arg(k, ArgRef::Inline(v.into_bytes()));
            }
            match kernel.issue(request, &Capability::root()).await {
                Ok(repr) => Resp {
                    status: 200,
                    headers: vec![
                        ("Content-Type".to_string(), content_type(&repr.repr_type)),
                        ("Cache-Control".to_string(), cache_control(repr.expiry)),
                    ],
                    body: repr.bytes,
                    suppress_body: false,
                },
                Err(e) => error_resp(status_of(&e), &e.to_string()),
            }
        }
        ("POST", "sink") => {
            if !post_allowed(target.as_str()) {
                return error_resp(
                    405,
                    "v1 accepts sink only for the annotation family (urn:annotation…)",
                );
            }
            // The command's own k=v args, then the form fields (which win).
            let mut merged: Vec<(String, String)> = args;
            let content_type_hdr = req.header("content-type").unwrap_or("");
            if content_type_hdr.starts_with("application/x-www-form-urlencoded") {
                let Ok(body) = std::str::from_utf8(&req.body) else {
                    return error_resp(400, "form body is not UTF-8");
                };
                for (k, v) in parse_query(body) {
                    merged.retain(|(name, _)| name != &k);
                    merged.push((k, v));
                }
            }
            let mut request = Request::new(Verb::Sink, target);
            for (k, v) in merged {
                request = request.with_arg(k, ArgRef::Inline(v.into_bytes()));
            }
            match kernel.issue(request, &Capability::root()).await {
                Ok(repr) => Resp {
                    status: 200,
                    headers: vec![
                        ("Content-Type".to_string(), content_type(&repr.repr_type)),
                        ("Cache-Control".to_string(), "no-store".to_string()),
                    ],
                    body: repr.bytes,
                    suppress_body: false,
                },
                Err(e) => error_resp(status_of(&e), &e.to_string()),
            }
        }
        _ => error_resp(
            405,
            "the /k/ adapter speaks GET source and POST sink (annotation family) only",
        ),
    }
}

/// The vendored htmx the shell page loads — same-origin, no CDN, the
/// `ikigai-cms-web` posture (its `dist/htmx.min.js`, copied verbatim).
fn htmx_js() -> Resp {
    Resp {
        status: 200,
        headers: vec![
            (
                "Content-Type".to_string(),
                "text/javascript; charset=utf-8".to_string(),
            ),
            (
                "Cache-Control".to_string(),
                "public, max-age=86400".to_string(),
            ),
        ],
        body: include_bytes!("../assets/htmx.min.js").to_vec(),
        suppress_body: false,
    }
}

/// `GET /browse/{uri}` — the HOST PAGE for the browse family's htmx faces: a
/// shell that loads htmx, styles the `browse-*` classes, and `hx-get`s the
/// starting resource into `#browse` on load. Everything after the first paint
/// is the faces' own affordances through the `/k/` adapter.
fn browse_shell(start: &str, posture: Posture) -> Resp {
    if Iri::parse(start.to_string()).is_err() || !start.starts_with("urn:") {
        return error_resp(404, &format!("`{start}` is not a browsable urn:* IRI"));
    }
    // Read-only: don't OFFER the annotate form the gate would 403. The form
    // markup comes from the mounted peer's faces, which have no adapter-level
    // flag to omit it (`chrome=embed` strips crumbs only) — so the shell,
    // whose job is dressing the faces, undresses this one affordance. The
    // ENFORCEMENT is the gate in [`respond`]; this is honesty of presentation,
    // not the security boundary.
    let readonly_css = match posture {
        Posture::ReadOnly => ".browse-annotate{display:none}",
        Posture::LocalOwner => "",
    };
    let start = html_escape(start);
    let body = format!(
        "<!doctype html><html><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <title>{start}</title>\
         <script src=\"/htmx.min.js\"></script>\
         <style>{BROWSE_CSS}{readonly_css}</style></head><body>\
         <main id=\"browse\" hx-get=\"/k/source {start} as=text/html\" \
         hx-trigger=\"load\" hx-swap=\"innerHTML\">loading {start}…</main>\
         </body></html>\n"
    );
    Resp {
        status: 200,
        headers: vec![
            (
                "Content-Type".to_string(),
                "text/html; charset=utf-8".to_string(),
            ),
            ("Cache-Control".to_string(), "no-store".to_string()),
        ],
        body: body.into_bytes(),
        suppress_body: false,
    }
}

/// Just enough style for the faces' `browse-*` classes to read comfortably —
/// responsive, and legible in light and dark. The faces own their markup; the
/// shell only dresses it.
const BROWSE_CSS: &str = "\
 :root{color-scheme:light dark}\
 body{margin:0;font:15px/1.5 -apple-system,system-ui,sans-serif;\
   max-width:60rem;padding:1rem;margin-inline:auto}\
 button{background:none;border:none;padding:0;font:inherit;\
   color:light-dark(#0b57d0,#8ab4f8);cursor:pointer}\
 button:hover{text-decoration:underline}\
 .browse-entries{list-style:none;padding-left:0}\
 .browse-entries li{padding:.1rem 0}\
 .browse-crumbs{margin-bottom:.75rem}\
 .browse-actions{display:flex;flex-wrap:wrap;gap:.9rem;margin-bottom:.75rem}\
 .index-badge{font-size:.72em;padding:.05em .5em;margin-left:.35rem;\
   border:1px solid light-dark(#1e7e34,#7bd88f);border-radius:999px;\
   color:light-dark(#1e7e34,#7bd88f);vertical-align:.1em;white-space:nowrap}\
 .browse-sep{opacity:.5;margin:0 .25rem}\
 .browse-size{opacity:.6;font-size:.85em;margin-left:.5rem}\
 pre,code{font:13px/1.45 ui-monospace,SFMono-Regular,Menlo,monospace;\
   overflow-x:auto}\
 .browse-code,.browse-pr-diff,pre:has(> code){color-scheme:light;\
   background:#fff;color:#24292e;padding:.6rem .8rem;border-radius:6px}\
 .browse-annotate{margin:1rem 0;display:grid;gap:.4rem;max-width:32rem}\
 .browse-annotate input,.browse-annotate textarea{font:inherit;padding:.3rem}\
";

/// The browse-root set the CATALOG asserts: each mounted root contributes a
/// `urn:repo:{name}:tree` row to `kernel.entries()`, so the names extracted
/// here are the repos `/browse/…` can actually serve — including roots that
/// live outside the scan directory and so never appear in `urn:repo:list`.
fn browse_roots(entries: &[ikigai_core::SpaceEntry]) -> std::collections::BTreeSet<String> {
    entries
        .iter()
        .filter_map(|entry| {
            let name = entry
                .pattern
                .strip_prefix("urn:repo:")?
                .strip_suffix(":tree")?;
            // A root NAME, not a deeper pattern: `urn:repo:branch` (no name),
            // `…:tree:{path}` (suffix mismatch) and any templated segment are
            // all someone else's rows.
            (!name.is_empty() && !name.contains(':') && !name.contains('{'))
                .then(|| name.to_string())
        })
        .collect()
}

/// `GET /` — a courtesy index: the repos (the `urn:repo:list` scan ∪ the
/// catalog's browse roots — the scan is what's on disk, the roots are what's
/// actually mounted, and neither contains the other) and the kernel's catalog
/// as links, so Safari lands somewhere useful. Derived entirely from
/// resolution and `kernel.entries()` (the catalog IS the machine-legible
/// face); an asleep peer simply contributes nothing.
async fn index(kernel: &Kernel) -> Resp {
    let mut entries = kernel.entries().unwrap_or_default();
    entries.sort_by(|a, b| a.pattern.cmp(&b.pattern));
    let roots = browse_roots(&entries);
    // Best-effort: the repo list is itself a resource; no peer, no scan names.
    let mut names: Vec<String> = roots.iter().cloned().collect();
    if let Ok(target) = Iri::parse("urn:repo:list") {
        if let Ok(repr) = kernel
            .issue(Request::new(Verb::Source, target), &Capability::root())
            .await
        {
            for line in String::from_utf8_lossy(&repr.bytes).lines() {
                if let Some(name) = line.split('\t').next().filter(|n| !n.is_empty()) {
                    names.push(name.to_string());
                }
            }
        }
    }
    names.sort();
    names.dedup();
    let mut repos = String::new();
    for name in &names {
        // The badge marks the rows the catalog BACKS — an unmounted scan row
        // renders as before (the link is a courtesy; resolution says 404).
        let badge = if roots.contains(name) {
            " <span class=\"index-badge\">browsable</span>"
        } else {
            ""
        };
        let name = html_escape(name);
        repos.push_str(&format!(
            "<li><a href=\"/browse/urn:repo:{name}:tree\"><code>{name}</code></a>{badge}</li>\n"
        ));
    }
    let repos = if repos.is_empty() {
        String::new()
    } else {
        format!("<h2>Browse a repo</h2><ul>\n{repos}</ul>")
    };
    let mut rows = String::new();
    for entry in &entries {
        let pattern = html_escape(&entry.pattern);
        // A repo tree opens in the htmx shell (clickable browse); everything
        // else resolves directly.
        let href = if entry.pattern.starts_with("urn:repo:") && entry.pattern.ends_with(":tree") {
            format!("/browse/{pattern}")
        } else {
            format!("/{pattern}")
        };
        rows.push_str(&format!(
            "<li><a href=\"{href}\"><code>{pattern}</code></a> <small>{}</small></li>\n",
            html_escape(&entry.endpoint)
        ));
    }
    let body = format!(
        "<!doctype html><html><head><meta charset=\"utf-8\">\
         <title>ikigai</title><style>{BROWSE_CSS}</style></head><body>\
         <h1>ikigai</h1>\
         <p>Resources this kernel serves. Open any <code>urn:*</code> as a path, \
         e.g. <code>/urn:repo:ikigai-core:tree</code>.</p>\
         <h2>Query</h2>\
         <p><a href=\"/sparql\">SPARQL editor</a> \u{2014} the shared RDF store \
         (explanations, annotations, review passes), queryable.</p>\
         {repos}\
         <h2>Catalog</h2><ul>\n{rows}</ul></body></html>\n"
    );
    Resp {
        status: 200,
        headers: vec![
            (
                "Content-Type".to_string(),
                "text/html; charset=utf-8".to_string(),
            ),
            ("Cache-Control".to_string(), "no-store".to_string()),
        ],
        body: body.into_bytes(),
        suppress_body: false,
    }
}

pub(crate) fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// The `Content-Type` header for a representation: its canonical media type,
/// with `charset=utf-8` added to bare `text/*` types so browsers decode the
/// UTF-8 the kernel speaks.
pub fn content_type(repr_type: &ikigai_core::ReprType) -> String {
    let canonical = repr_type.canonical();
    if repr_type.media_type.starts_with("text/") && !canonical.contains("charset") {
        format!("{}; charset=utf-8", repr_type.media_type)
    } else {
        canonical
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        413 => "Payload Too Large",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "",
    }
}

async fn write_response(stream: &mut TcpStream, resp: Resp) -> std::io::Result<()> {
    let mut head = format!("HTTP/1.1 {} {}\r\n", resp.status, reason(resp.status));
    for (name, value) in &resp.headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str(&format!("Content-Length: {}\r\n", resp.body.len()));
    head.push_str("X-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n");
    stream.write_all(head.as_bytes()).await?;
    if !resp.suppress_body {
        stream.write_all(&resp.body).await?;
    }
    stream.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browser_accept_gets_the_html_face() {
        assert_eq!(
            accept_face("text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8"),
            Some("text/html")
        );
        assert_eq!(accept_face("text/turtle"), Some("text/turtle"));
        assert_eq!(accept_face("application/json"), Some("application/json"));
        // curl's default asks for anything → the endpoint's default face.
        assert_eq!(accept_face("*/*"), None);
        assert_eq!(accept_face("image/png"), None);
    }

    #[test]
    fn percent_decoding_round_trips() {
        assert_eq!(
            percent_decode("/urn:text:grep%20x", false).as_deref(),
            Some("/urn:text:grep x")
        );
        // `+` is a space only in query/form context.
        assert_eq!(percent_decode("a+b", false).as_deref(), Some("a+b"));
        assert_eq!(percent_decode("a+b", true).as_deref(), Some("a b"));
        assert_eq!(percent_decode("bad%zz", false), None);
    }

    #[test]
    fn expiry_projects_to_cache_control() {
        assert_eq!(cache_control(Expiry::Always), "no-store");
        assert_eq!(
            cache_control(Expiry::Never),
            "public, max-age=31536000, immutable"
        );
        let soon = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
            + 60_000;
        let header = cache_control(Expiry::At(ikigai_core::Time::from_millis(soon)));
        assert!(header.starts_with("max-age=5") || header.starts_with("max-age=60"));
    }

    #[test]
    fn typed_errors_map_to_their_statuses() {
        assert_eq!(status_of(&Error::NotFound("x".into())), 404);
        assert_eq!(status_of(&Error::Denied("x".into())), 403);
        assert_eq!(status_of(&Error::Unavailable("x".into())), 503);
        assert_eq!(status_of(&Error::Timeout("x".into())), 504);
        assert_eq!(status_of(&Error::MissingArgument("x".into())), 400);
        assert_eq!(status_of(&Error::Endpoint("x".into())), 500);
    }

    #[test]
    fn browse_roots_are_exactly_the_root_tree_patterns() {
        let entries = vec![
            ikigai_core::SpaceEntry::new("urn:repo:folio:tree", "tree"),
            // Deeper and templated patterns under the same root: not names.
            ikigai_core::SpaceEntry::new("urn:repo:folio:tree:{path}", "tree"),
            ikigai_core::SpaceEntry::new("urn:repo:{name}:tree", "tree"),
            ikigai_core::SpaceEntry::new("urn:repo:x:sub:tree", "tree"),
            // Root-less repo endpoints and unrelated bindings: not names.
            ikigai_core::SpaceEntry::new("urn:repo:branch", "branch"),
            ikigai_core::SpaceEntry::new("urn:hello", "hello"),
        ];
        let roots: Vec<String> = browse_roots(&entries).into_iter().collect();
        assert_eq!(roots, vec!["folio".to_string()]);
    }

    #[test]
    fn the_write_surface_is_exactly_the_annotation_family() {
        assert!(post_allowed("urn:annotation"));
        assert!(post_allowed("urn:annotation:abc-123"));
        assert!(!post_allowed("urn:annotationx"));
        assert!(!post_allowed("urn:repo:ikigai-core:tree"));
        assert!(!post_allowed("urn:kernel:cut"));
    }
}
