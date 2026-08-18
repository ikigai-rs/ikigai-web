//! End-to-end: a real TCP round-trip against a kernel of in-process test
//! endpoints — the verb map, conneg, query passthrough, the POST contract,
//! and the typed-error → status projection.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, OnceLock};

use ikigai_core::{
    ArgRef, Capability, Error, Exact, Fallback, FnEndpoint, Iri, Kernel, ReprType, Representation,
    Request, Space, Verb,
};

/// A tiny multi-face endpoint: Source serves faces by `as`, Exists says true.
fn hello() -> FnEndpoint {
    FnEndpoint::new("hello", |inv| match inv.request.verb {
        Verb::Source => {
            let shout = inv.inline_str("shout").unwrap_or("false") == "true";
            let text = if shout { "HELLO" } else { "hello" };
            match inv.inline_str("as").unwrap_or("text/plain") {
                "text/html" => Ok(Representation::new(
                    ReprType::new("text/html"),
                    format!("<html><body>{text}</body></html>"),
                )),
                "text/turtle" => Ok(Representation::new(
                    ReprType::new("text/turtle"),
                    format!("<urn:hello> <urn:says> \"{text}\" ."),
                )),
                _ => Ok(Representation::new(ReprType::new("text/plain"), text)),
            }
        }
        Verb::Exists => Ok(Representation::new(ReprType::new("text/plain"), "true")),
        _ => Err(Error::Endpoint("hello: Source|Exists only".into())),
    })
}

/// A permanently-cacheable representation — the Cache-Control projection case.
fn pure() -> FnEndpoint {
    FnEndpoint::new("pure", |_inv| {
        Ok(Representation::new(ReprType::new("text/plain"), "42").cacheable())
    })
}

/// Sink target standing in for the annotation family: echoes the args it saw.
fn annotation() -> FnEndpoint {
    FnEndpoint::new("annotation", |inv| {
        if inv.request.verb != Verb::Sink {
            return Err(Error::Endpoint("annotation: Sink only".into()));
        }
        let body = inv.inline_str("body").unwrap_or("<absent>");
        let exact = inv.inline_str("exact").unwrap_or("<absent>");
        let target = inv.inline_str("target").unwrap_or("<absent>");
        let content = inv.inline_str("content").unwrap_or("<absent>");
        Ok(Representation::new(
            ReprType::new("text/plain"),
            format!("body={body} exact={exact} target={target} content={content}"),
        ))
    })
}

/// Stub `urn:sparql:{form}` endpoints: shaped like ikigai-sparql's faces
/// (default = sparql-results+json for select/ask, turtle for construct/
/// describe; `as=` picks CSV/TSV), but canned — the tests here exercise the
/// FACE's contract (routing by form, conneg → `as=`, query passthrough), not
/// query evaluation, which is ikigai-sparql's own tested job.
fn sparql_stub(form: &'static str) -> FnEndpoint {
    FnEndpoint::new(form, move |inv| {
        if inv.request.verb != Verb::Source {
            return Err(Error::Endpoint("sparql: Source only".into()));
        }
        // A face bug that drops the query must fail loudly here.
        let query = inv.inline_str("query").unwrap_or("");
        if query.is_empty() {
            return Err(Error::MissingArgument("query".into()));
        }
        if form == "construct" || form == "describe" {
            return Ok(Representation::new(
                ReprType::new("text/turtle"),
                format!("<urn:x:1> <urn:from> \"{form}\" ."),
            ));
        }
        match inv.inline_str("as").unwrap_or("") {
            "text/csv" => Ok(Representation::new(
                ReprType::new("text/csv"),
                if form == "ask" {
                    "true"
                } else {
                    "s\r\nurn:x:1"
                },
            )),
            "text/tab-separated-values" => Ok(Representation::new(
                ReprType::new("text/tab-separated-values"),
                "?s\t?note\n<urn:x:1>\t\"hi\\nthere\"@en\n",
            )),
            _ => Ok(Representation::new(
                ReprType::new("application/sparql-results+json"),
                format!("{{\"form\":\"{form}\",\"query\":\"{}\"}}", query.len()),
            )),
        }
    })
}

/// The `urn:repo:list` scan's shape: `name\tpath` lines. `beta` is also
/// mounted (a `urn:repo:beta:tree` binding below); `alpha` is scan-only.
fn repo_list() -> FnEndpoint {
    FnEndpoint::new("repo-list", |_inv| {
        Ok(Representation::new(
            ReprType::new("text/plain"),
            "alpha\t/scan/alpha\nbeta\t/scan/beta\n",
        ))
    })
}

fn erroring(kind: &'static str) -> FnEndpoint {
    FnEndpoint::new(kind, move |_inv| -> Result<Representation, Error> {
        Err(match kind {
            "missing" => Error::NotFound("gone fishing".into()),
            "denied" => Error::Denied("not with that capability".into()),
            _ => Error::Unavailable("peer asleep".into()),
        })
    })
}

fn test_kernel() -> Kernel {
    let space = ikigai_core::EndpointSpace::new()
        .bind(Exact::new("urn:hello"), hello())
        .bind(Exact::new("urn:pure"), pure())
        .bind(Exact::new("urn:annotation"), annotation())
        .bind(Exact::new("urn:annotation:abc"), annotation())
        .bind(Exact::new("urn:sparql:select"), sparql_stub("select"))
        .bind(Exact::new("urn:sparql:ask"), sparql_stub("ask"))
        .bind(Exact::new("urn:sparql:construct"), sparql_stub("construct"))
        .bind(Exact::new("urn:sparql:describe"), sparql_stub("describe"))
        .bind(Exact::new("urn:repo:list"), repo_list())
        // Mounted browse roots: `beta` is also in the scan, `folio` is not
        // (the root-living-elsewhere case the index must still surface).
        .bind(Exact::new("urn:repo:beta:tree"), hello())
        .bind(Exact::new("urn:repo:folio:tree"), hello())
        .bind(Exact::new("urn:missing"), erroring("missing"))
        .bind(Exact::new("urn:denied"), erroring("denied"))
        .bind(Exact::new("urn:asleep"), erroring("asleep"));
    Kernel::new(Arc::new(Fallback::new(vec![
        Arc::new(space) as Arc<dyn Space>
    ])))
}

/// Spawn a server on an OS-assigned port at `bind_ip`; return where it landed.
fn spawn(bind_ip: &str) -> std::net::SocketAddr {
    let addr: std::net::SocketAddr = format!("{bind_ip}:0").parse().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async move {
            let listener = ikigai_web_server::serve::bind(addr).await.unwrap();
            tx.send(listener.local_addr().unwrap()).unwrap();
            ikigai_web_server::serve::serve(Arc::new(test_kernel()), listener).await
        })
    });
    rx.recv().unwrap()
}

/// One loopback server for the whole test binary — the LocalOwner posture.
fn server_addr() -> std::net::SocketAddr {
    static ADDR: OnceLock<std::net::SocketAddr> = OnceLock::new();
    *ADDR.get_or_init(|| spawn("127.0.0.1"))
}

/// One non-loopback (0.0.0.0) server — the ReadOnly posture. Tests connect to
/// it via 127.0.0.1; the posture keys on the BOUND address, not the caller's.
fn readonly_addr() -> std::net::SocketAddr {
    static ADDR: OnceLock<std::net::SocketAddr> = OnceLock::new();
    *ADDR.get_or_init(|| {
        let bound = spawn("0.0.0.0");
        format!("127.0.0.1:{}", bound.port()).parse().unwrap()
    })
}

/// Send raw HTTP to `addr`, return (status, headers-lowercased, body).
fn roundtrip_at(
    addr: std::net::SocketAddr,
    request: &str,
) -> (u16, Vec<(String, String)>, Vec<u8>) {
    let mut stream = TcpStream::connect(addr).unwrap();
    stream.write_all(request.as_bytes()).unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).unwrap();
    let head_end = response
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("a complete response head");
    let head = std::str::from_utf8(&response[..head_end]).unwrap();
    let mut lines = head.split("\r\n");
    let status: u16 = lines
        .next()
        .unwrap()
        .split(' ')
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    (status, headers, response[head_end + 4..].to_vec())
}

/// Send raw HTTP to the loopback server.
fn roundtrip(request: &str) -> (u16, Vec<(String, String)>, Vec<u8>) {
    roundtrip_at(server_addr(), request)
}

fn get(path_and_headers: &str) -> (u16, Vec<(String, String)>, Vec<u8>) {
    roundtrip(&format!(
        "GET {path_and_headers} HTTP/1.1\r\nHost: t\r\n\r\n"
    ))
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
}

#[test]
fn get_resolves_source_default_face() {
    let (status, headers, body) = get("/urn:hello");
    assert_eq!(status, 200);
    assert_eq!(
        header(&headers, "content-type"),
        Some("text/plain; charset=utf-8")
    );
    assert_eq!(header(&headers, "cache-control"), Some("no-store"));
    assert_eq!(body, b"hello");
}

#[test]
fn browser_accept_selects_the_html_face() {
    let (status, headers, body) = roundtrip(
        "GET /urn:hello HTTP/1.1\r\nHost: t\r\n\
         Accept: text/html,application/xhtml+xml,*/*;q=0.8\r\n\r\n",
    );
    assert_eq!(status, 200);
    assert_eq!(
        header(&headers, "content-type"),
        Some("text/html; charset=utf-8")
    );
    assert!(String::from_utf8(body).unwrap().contains("<body>hello"));
}

#[test]
fn turtle_via_accept_header() {
    let (status, headers, body) =
        roundtrip("GET /urn:hello HTTP/1.1\r\nHost: t\r\nAccept: text/turtle\r\n\r\n");
    assert_eq!(status, 200);
    assert_eq!(
        header(&headers, "content-type"),
        Some("text/turtle; charset=utf-8")
    );
    assert!(String::from_utf8(body).unwrap().contains("<urn:says>"));
}

#[test]
fn explicit_as_query_arg_wins_over_accept() {
    let (_, headers, _) =
        roundtrip("GET /urn:hello?as=text/turtle HTTP/1.1\r\nHost: t\r\nAccept: text/html\r\n\r\n");
    assert_eq!(
        header(&headers, "content-type"),
        Some("text/turtle; charset=utf-8")
    );
}

#[test]
fn query_args_pass_through_to_the_invocation() {
    let (_, _, body) = get("/urn:hello?shout=true");
    assert_eq!(body, b"HELLO");
}

#[test]
fn percent_encoded_paths_decode() {
    let (status, _, _) = get("/urn%3Ahello");
    assert_eq!(status, 200);
}

/// A kernel-permanent representation is cacheable, NOT url-immutable: the
/// stylesheet is the standing counterexample — a stable URL whose content
/// changes with `a11y.toml`. `immutable` hid every such change from a browser
/// short of a hard reload; `no-cache` + a validator does not.
#[test]
fn cacheable_representations_revalidate_rather_than_promise_immutability() {
    let (_, headers, _) = get("/urn:pure");
    assert_eq!(header(&headers, "cache-control"), Some("public, no-cache"));
    let etag = header(&headers, "etag").expect("a strong ETag");
    assert!(
        etag.starts_with("\"b3:") && etag.ends_with('"'),
        "etag: {etag}"
    );
    assert_eq!(header(&headers, "vary"), Some("Accept"));
}

#[test]
fn a_matching_if_none_match_gets_a_bodyless_304() {
    let (_, headers, body) = get("/urn:pure");
    let etag = header(&headers, "etag").unwrap().to_string();
    assert!(!body.is_empty());

    let (status, headers, body) = roundtrip(&format!(
        "GET /urn:pure HTTP/1.1\r\nHost: t\r\nIf-None-Match: {etag}\r\n\r\n"
    ));
    assert_eq!(status, 304);
    assert!(body.is_empty(), "a 304 carries no body");
    // The 304 still carries the validator and the caching directive — a cache
    // that reuses the stored body needs both.
    assert_eq!(header(&headers, "etag"), Some(etag.as_str()));
    assert_eq!(header(&headers, "cache-control"), Some("public, no-cache"));
    assert_eq!(header(&headers, "vary"), Some("Accept"));
    assert_eq!(header(&headers, "content-length"), None);

    // Weak comparison and `*` are both hits (RFC 9110 §13.1.2).
    assert_eq!(
        roundtrip(&format!(
            "GET /urn:pure HTTP/1.1\r\nHost: t\r\nIf-None-Match: W/{etag}\r\n\r\n"
        ))
        .0,
        304
    );
    assert_eq!(
        roundtrip("GET /urn:pure HTTP/1.1\r\nHost: t\r\nIf-None-Match: *\r\n\r\n").0,
        304
    );
}

#[test]
fn a_stale_if_none_match_gets_the_full_body() {
    let (status, headers, body) = roundtrip(
        "GET /urn:pure HTTP/1.1\r\nHost: t\r\n\
         If-None-Match: \"b3:0000000000000000\"\r\n\r\n",
    );
    assert_eq!(status, 200);
    assert_eq!(body, b"42");
    assert!(header(&headers, "etag").unwrap().starts_with("\"b3:"));
}

/// `content_id` hashes the repr TYPE as well as the bytes, so the two faces of
/// one resource cannot collide — which is what makes `Vary: Accept` safe
/// rather than merely hopeful. Both faces here even carry different bytes; the
/// point is that the tags move together with the face.
#[test]
fn two_faces_of_one_resource_carry_different_etags() {
    let (_, html, _) = roundtrip("GET /urn:hello HTTP/1.1\r\nHost: t\r\nAccept: text/html\r\n\r\n");
    let (_, turtle, _) =
        roundtrip("GET /urn:hello HTTP/1.1\r\nHost: t\r\nAccept: text/turtle\r\n\r\n");
    let html_tag = header(&html, "etag").unwrap();
    let turtle_tag = header(&turtle, "etag").unwrap();
    assert_ne!(html_tag, turtle_tag);
    assert_eq!(header(&html, "vary"), Some("Accept"));
    // And the HTML face's own tag does not validate the Turtle face.
    let (status, _, _) = roundtrip(&format!(
        "GET /urn:hello HTTP/1.1\r\nHost: t\r\nAccept: text/turtle\r\n\
         If-None-Match: {html_tag}\r\n\r\n"
    ));
    assert_eq!(status, 200);
}

/// The stylesheet the browse shell links is fetched through the `/k/` adapter
/// (`<link href="/k/source urn:repo:style">`) — the route the invisible-change
/// bug actually travelled. It revalidates too.
#[test]
fn the_k_adapter_revalidates_like_the_direct_route() {
    let (status, headers, _) = get("/k/source%20urn:pure");
    assert_eq!(status, 200);
    assert_eq!(header(&headers, "cache-control"), Some("public, no-cache"));
    let etag = header(&headers, "etag").unwrap().to_string();
    let (status, _, body) = roundtrip(&format!(
        "GET /k/source%20urn:pure HTTP/1.1\r\nHost: t\r\nIf-None-Match: {etag}\r\n\r\n"
    ));
    assert_eq!(status, 304);
    assert!(body.is_empty());
}

#[test]
fn head_maps_to_exists() {
    let (status, _, body) = roundtrip("HEAD /urn:hello HTTP/1.1\r\nHost: t\r\n\r\n");
    assert_eq!(status, 200);
    assert!(body.is_empty());
    let (status, _, body) = roundtrip("HEAD /urn:nowhere HTTP/1.1\r\nHost: t\r\n\r\n");
    assert_eq!(status, 404);
    assert!(body.is_empty());
}

#[test]
fn typed_errors_surface_as_statuses() {
    assert_eq!(get("/urn:missing").0, 404);
    assert_eq!(get("/urn:denied").0, 403);
    assert_eq!(get("/urn:asleep").0, 503);
    assert_eq!(get("/urn:never-bound").0, 404);
}

#[test]
fn post_form_body_maps_to_sink_args() {
    let form = "target=urn%3Arepo%3Ax%3Afile%3Aa.rs&body=nice+line&exact=let%20x";
    let (status, _, body) = roundtrip(&format!(
        "POST /urn:annotation HTTP/1.1\r\nHost: t\r\n\
         Content-Type: application/x-www-form-urlencoded\r\n\
         Content-Length: {}\r\n\r\n{form}",
        form.len()
    ));
    assert_eq!(status, 200);
    let body = String::from_utf8(body).unwrap();
    assert!(body.contains("body=nice line"), "body was: {body}");
    assert!(body.contains("exact=let x"), "body was: {body}");
    assert!(
        body.contains("target=urn:repo:x:file:a.rs"),
        "body was: {body}"
    );
}

#[test]
fn post_raw_body_arrives_as_piped_content() {
    let note = "a raw note";
    let (status, _, body) = roundtrip(&format!(
        "POST /urn:annotation:abc?target=urn:repo:x:file:a.rs HTTP/1.1\r\nHost: t\r\n\
         Content-Type: text/plain\r\nContent-Length: {}\r\n\r\n{note}",
        note.len()
    ));
    assert_eq!(status, 200);
    let body = String::from_utf8(body).unwrap();
    assert!(body.contains("content=a raw note"), "body was: {body}");
    assert!(
        body.contains("target=urn:repo:x:file:a.rs"),
        "body was: {body}"
    );
}

#[test]
fn post_outside_the_annotation_family_is_refused() {
    let (status, headers, _) =
        roundtrip("POST /urn:hello HTTP/1.1\r\nHost: t\r\nContent-Length: 0\r\n\r\n");
    assert_eq!(status, 405);
    assert_eq!(header(&headers, "allow"), Some("GET, HEAD"));
}

#[test]
fn unsupported_methods_get_405_with_allow() {
    let (status, headers, _) =
        roundtrip("DELETE /urn:hello HTTP/1.1\r\nHost: t\r\nContent-Length: 0\r\n\r\n");
    assert_eq!(status, 405);
    assert_eq!(header(&headers, "allow"), Some("GET, HEAD, POST"));
}

#[test]
fn the_index_lists_the_catalog() {
    let (status, headers, body) = get("/");
    assert_eq!(status, 200);
    assert_eq!(
        header(&headers, "content-type"),
        Some("text/html; charset=utf-8")
    );
    let body = String::from_utf8(body).unwrap();
    assert!(body.contains("urn:hello"), "index was: {body}");
}

/// The repo section is the scan ∪ the catalog's browse roots: rows the
/// catalog backs (a `urn:repo:{name}:tree` binding) wear the badge, scan-only
/// rows render bare, and a mounted root the scan never saw still appears.
#[test]
fn the_index_badges_catalog_backed_browse_roots() {
    let (_, _, body) = get("/");
    let body = String::from_utf8(body).unwrap();
    assert!(
        body.contains("<li><a href=\"/browse/urn:repo:alpha:tree\"><code>alpha</code></a></li>"),
        "scan-only row grew a badge or vanished; index was: {body}"
    );
    assert!(
        body.contains("<code>beta</code></a> <span class=\"index-badge\">browsable</span>"),
        "mounted scan row is unbadged; index was: {body}"
    );
    assert!(
        body.contains("<code>folio</code></a> <span class=\"index-badge\">browsable</span>"),
        "root outside the scan is missing; index was: {body}"
    );
}

#[test]
fn non_urn_paths_are_not_resolvable() {
    let (status, _, _) = get("/etc/passwd");
    assert_eq!(status, 404);
}

/// The face never widens the verb: a GET cannot mutate even a Sink-only
/// endpoint (the annotation echo errors on non-Sink verbs → 500, not 200).
#[test]
fn get_never_sinks() {
    let (status, _, _) = get("/urn:annotation");
    assert_eq!(status, 500);
}

/// The `/k/` host adapter speaks the browse faces' own affordance shape:
/// `hx-get="/k/source <iri> [k=v ...]"` (the browser percent-encodes spaces).
#[test]
fn k_adapter_sources_with_args() {
    let (status, headers, body) = get("/k/source%20urn:hello%20as=text/html%20shout=true");
    assert_eq!(status, 200);
    assert_eq!(
        header(&headers, "content-type"),
        Some("text/html; charset=utf-8")
    );
    assert!(String::from_utf8(body).unwrap().contains("HELLO"));
}

/// `hx-post="/k/sink urn:annotation"` with form fields — the annotate form.
#[test]
fn k_adapter_sinks_annotation_forms() {
    let form = "target=urn%3Arepo%3Ax%3Afile%3Aa.rs&body=note&exact=let";
    let (status, _, body) = roundtrip(&format!(
        "POST /k/sink%20urn:annotation HTTP/1.1\r\nHost: t\r\n\
         Content-Type: application/x-www-form-urlencoded\r\n\
         Content-Length: {}\r\n\r\n{form}",
        form.len()
    ));
    assert_eq!(status, 200);
    let body = String::from_utf8(body).unwrap();
    assert!(body.contains("body=note"), "body was: {body}");
}

/// The adapter never widens the write surface: sink outside the annotation
/// family refuses, and GET cannot sink at all.
#[test]
fn k_adapter_keeps_the_write_surface() {
    let (status, _, _) =
        roundtrip("POST /k/sink%20urn:hello HTTP/1.1\r\nHost: t\r\nContent-Length: 0\r\n\r\n");
    assert_eq!(status, 405);
    let (status, _, _) = get("/k/sink%20urn:annotation");
    assert_eq!(status, 405);
    let (status, _, _) = get("/k/delete%20urn:hello");
    assert_eq!(status, 405);
}

/// `/browse/{uri}` is the htmx shell: it loads htmx and hx-gets the start
/// resource through the adapter.
#[test]
fn browse_shell_hosts_the_faces() {
    let (status, headers, body) = get("/browse/urn:repo:demo:tree");
    assert_eq!(status, 200);
    assert_eq!(
        header(&headers, "content-type"),
        Some("text/html; charset=utf-8")
    );
    let body = String::from_utf8(body).unwrap();
    assert!(body.contains("src=\"/htmx.min.js\""), "shell was: {body}");
    assert!(
        body.contains("hx-get=\"/k/source urn:repo:demo:tree as=text/html\""),
        "shell was: {body}"
    );
    let (status, headers, _) = get("/htmx.min.js");
    assert_eq!(status, 200);
    assert_eq!(
        header(&headers, "content-type"),
        Some("text/javascript; charset=utf-8")
    );
}

// ---------------------------------------------------------------------------
// The /sparql face.
// ---------------------------------------------------------------------------

/// A protocol client (curl, another tool) with no Accept opinion gets the
/// endpoint's default face: sparql-results+json, routed by query form.
#[test]
fn sparql_get_executes_with_the_default_json_face() {
    let query = ikigai_web_server::sparql::urlencode("SELECT ?s WHERE { ?s ?p ?o }");
    let (status, headers, body) = get(&format!("/sparql?query={query}"));
    assert_eq!(status, 200);
    assert_eq!(
        header(&headers, "content-type"),
        Some("application/sparql-results+json")
    );
    let body = String::from_utf8(body).unwrap();
    assert!(body.contains("\"form\":\"select\""), "body was: {body}");
}

/// `Accept: text/csv` becomes `as=text/csv` on the endpoint; ASK routes to
/// `urn:sparql:ask`; CONSTRUCT comes back as Turtle.
#[test]
fn sparql_conneg_selects_faces_and_forms_route() {
    let query = ikigai_web_server::sparql::urlencode("ASK { ?s ?p ?o }");
    let (status, headers, body) = roundtrip(&format!(
        "GET /sparql?query={query} HTTP/1.1\r\nHost: t\r\nAccept: text/csv\r\n\r\n"
    ));
    assert_eq!(status, 200);
    assert_eq!(
        header(&headers, "content-type"),
        Some("text/csv; charset=utf-8")
    );
    assert_eq!(body, b"true");

    let query = ikigai_web_server::sparql::urlencode("CONSTRUCT { ?s ?p ?o } WHERE { ?s ?p ?o }");
    let (status, headers, body) = get(&format!("/sparql?query={query}"));
    assert_eq!(status, 200);
    assert_eq!(
        header(&headers, "content-type"),
        Some("text/turtle; charset=utf-8")
    );
    assert!(String::from_utf8(body).unwrap().contains("construct"));
}

/// An explicit `?as=` wins even over a browser Accept — the editor page's own
/// raw-face links depend on this.
#[test]
fn sparql_explicit_as_wins_over_accept() {
    let query = ikigai_web_server::sparql::urlencode("SELECT ?s WHERE { ?s ?p ?o }");
    let (status, headers, _) = roundtrip(&format!(
        "GET /sparql?query={query}&as=text/csv HTTP/1.1\r\nHost: t\r\nAccept: text/html\r\n\r\n"
    ));
    assert_eq!(status, 200);
    assert_eq!(
        header(&headers, "content-type"),
        Some("text/csv; charset=utf-8")
    );
}

/// POST carries a long query: raw body (`application/sparql-query`) and form
/// body (`query=` field) both execute — as Source, never widening the write
/// surface.
#[test]
fn sparql_post_bodies_carry_the_query() {
    let query = "SELECT ?s WHERE { ?s ?p ?o }";
    let (status, headers, _) = roundtrip(&format!(
        "POST /sparql HTTP/1.1\r\nHost: t\r\n\
         Content-Type: application/sparql-query\r\nContent-Length: {}\r\n\r\n{query}",
        query.len()
    ));
    assert_eq!(status, 200);
    assert_eq!(
        header(&headers, "content-type"),
        Some("application/sparql-results+json")
    );

    let form = format!("query={}", ikigai_web_server::sparql::urlencode(query));
    let (status, _, body) = roundtrip(&format!(
        "POST /sparql HTTP/1.1\r\nHost: t\r\n\
         Content-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\n\r\n{form}",
        form.len()
    ));
    assert_eq!(status, 200);
    assert!(String::from_utf8(body).unwrap().contains("select"));
}

/// Update forms are refused at the face, loudly, before the kernel sees them.
#[test]
fn sparql_update_forms_are_refused() {
    for update in [
        "INSERT DATA { <urn:x> <urn:p> 1 }",
        "DELETE WHERE { ?s ?p ?o }",
    ] {
        let query = ikigai_web_server::sparql::urlencode(update);
        let (status, _, body) = get(&format!("/sparql?query={query}"));
        assert_eq!(status, 400);
        let body = String::from_utf8(body).unwrap();
        assert!(body.contains("read-only"), "body was: {body}");
    }
    // No query at all (non-HTML client) is a 400, not an empty editor.
    let (status, _, _) = get("/sparql?x=1");
    assert_eq!(status, 400);
}

/// A browser gets the editor page: the query prefilled BYTE-EXACTLY in the
/// textarea, results rendered as a table below, and every sample in the
/// sidebar with a click-to-fill link whose href round-trips byte-exactly.
#[test]
fn sparql_editor_page_prefills_and_renders_results() {
    let query = "SELECT ?s WHERE { ?s ?p ?o } # <tag> & \"quotes\"";
    let encoded = ikigai_web_server::sparql::urlencode(query);
    let (status, headers, body) = roundtrip(&format!(
        "GET /sparql?query={encoded} HTTP/1.1\r\nHost: t\r\nAccept: text/html\r\n\r\n"
    ));
    assert_eq!(status, 200);
    assert_eq!(
        header(&headers, "content-type"),
        Some("text/html; charset=utf-8")
    );
    let body = String::from_utf8(body).unwrap();
    // The prefill is the html-escape of the exact query text.
    let escaped = query
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;");
    assert!(body.contains(&escaped), "page was: {body}");
    // Results: the stub's TSV row became a table with a linked urn: IRI and
    // an unescaped literal.
    assert!(body.contains("<th>s</th><th>note</th>"), "page was: {body}");
    assert!(
        body.contains("<a href=\"/urn:x:1\"><code>urn:x:1</code></a>"),
        "page was: {body}"
    );
    assert!(body.contains("hi\nthere"), "page was: {body}");
    // Sidebar: all eight samples, each with a byte-exact click-to-fill href.
    for sample in ikigai_web_server::sparql::samples() {
        assert!(body.contains(sample.title), "missing: {}", sample.title);
        let href = format!(
            "/sparql?query={}",
            ikigai_web_server::sparql::urlencode(&sample.query())
        );
        assert!(body.contains(&href), "missing href for: {}", sample.title);
    }
    // The editor is same-origin: no external URL anywhere in the page.
    assert!(!body.contains("https://cdn"), "page was: {body}");
    assert!(!body.contains("http://cdn"), "page was: {body}");
}

/// An empty editor (no query yet) and a query error both render the page —
/// the editor is the html face's 200 even when the query is wrong.
#[test]
fn sparql_editor_page_handles_empty_and_bad_queries() {
    let (status, _, body) =
        roundtrip("GET /sparql HTTP/1.1\r\nHost: t\r\nAccept: text/html\r\n\r\n");
    assert_eq!(status, 200);
    assert!(String::from_utf8(body).unwrap().contains("<textarea"));

    let query = ikigai_web_server::sparql::urlencode("DROP GRAPH <urn:g>");
    let (status, _, body) = roundtrip(&format!(
        "GET /sparql?query={query} HTTP/1.1\r\nHost: t\r\nAccept: text/html\r\n\r\n"
    ));
    assert_eq!(status, 200);
    let body = String::from_utf8(body).unwrap();
    assert!(body.contains("read-only"), "page was: {body}");
}

/// The face's method surface is GET and POST only.
#[test]
fn sparql_rejects_other_methods() {
    let (status, headers, _) =
        roundtrip("DELETE /sparql HTTP/1.1\r\nHost: t\r\nContent-Length: 0\r\n\r\n");
    assert_eq!(status, 405);
    assert_eq!(header(&headers, "allow"), Some("GET, POST"));
}

// ---------------------------------------------------------------------------
// The read-only posture (non-loopback bind).
// ---------------------------------------------------------------------------

/// Off loopback the write surface is GONE: both spellings of the annotation
/// sink refuse with 403 and a one-line reason, and so does every other
/// non-GET/HEAD method — before any route dispatch.
#[test]
fn readonly_bind_disables_the_write_surface() {
    let addr = readonly_addr();
    let form = "target=urn%3Arepo%3Ax%3Afile%3Aa.rs&body=note&exact=let";
    for path in ["/urn:annotation", "/k/sink%20urn:annotation"] {
        let (status, headers, body) = roundtrip_at(
            addr,
            &format!(
                "POST {path} HTTP/1.1\r\nHost: t\r\n\
                 Content-Type: application/x-www-form-urlencoded\r\n\
                 Content-Length: {}\r\n\r\n{form}",
                form.len()
            ),
        );
        assert_eq!(status, 403, "for {path}");
        assert_eq!(header(&headers, "allow"), Some("GET, HEAD"));
        let body = String::from_utf8(body).unwrap();
        assert!(body.contains("read-only"), "body was: {body}");
        assert!(body.contains("loopback"), "body was: {body}");
    }
    // Other methods refuse at the same gate.
    let (status, _, _) = roundtrip_at(
        addr,
        "DELETE /urn:hello HTTP/1.1\r\nHost: t\r\nContent-Length: 0\r\n\r\n",
    );
    assert_eq!(status, 403);
}

/// The read surface survives untouched: GET (with conneg), HEAD, the /k/
/// adapter's source, and /sparql — including its POST spelling, which carries
/// a QUERY, not a write.
#[test]
fn readonly_bind_still_reads() {
    let addr = readonly_addr();
    let (status, _, body) = roundtrip_at(addr, "GET /urn:hello HTTP/1.1\r\nHost: t\r\n\r\n");
    assert_eq!(status, 200);
    assert_eq!(body, b"hello");
    let (status, _, body) = roundtrip_at(addr, "HEAD /urn:hello HTTP/1.1\r\nHost: t\r\n\r\n");
    assert_eq!(status, 200);
    assert!(body.is_empty());
    let (status, _, _) = roundtrip_at(
        addr,
        "GET /k/source%20urn:hello%20as=text/html HTTP/1.1\r\nHost: t\r\n\r\n",
    );
    assert_eq!(status, 200);
    let query = "SELECT ?s WHERE { ?s ?p ?o }";
    let (status, headers, _) = roundtrip_at(
        addr,
        &format!(
            "POST /sparql HTTP/1.1\r\nHost: t\r\n\
             Content-Type: application/sparql-query\r\nContent-Length: {}\r\n\r\n{query}",
            query.len()
        ),
    );
    assert_eq!(status, 200);
    assert_eq!(
        header(&headers, "content-type"),
        Some("application/sparql-results+json")
    );
    // /sparql stays read-only by its own construction: update forms refuse.
    let update = ikigai_web_server::sparql::urlencode("INSERT DATA { <urn:x> <urn:p> 1 }");
    let (status, _, _) = roundtrip_at(
        addr,
        &format!("GET /sparql?query={update} HTTP/1.1\r\nHost: t\r\n\r\n"),
    );
    assert_eq!(status, 400);
}

/// The shell page stops OFFERING the annotate form off loopback (the faces'
/// markup comes from the mounted peer unchanged — the shell hides the class),
/// and on loopback stays byte-identical to what it always served.
#[test]
fn readonly_shell_hides_the_annotate_form_loopback_does_not() {
    const HIDE: &str = ".browse-annotate{display:none}";
    let (status, _, body) = roundtrip_at(
        readonly_addr(),
        "GET /browse/urn:repo:demo:tree HTTP/1.1\r\nHost: t\r\n\r\n",
    );
    assert_eq!(status, 200);
    assert!(
        String::from_utf8(body).unwrap().contains(HIDE),
        "read-only shell must hide the annotate form"
    );
    let (_, _, body) = get("/browse/urn:repo:demo:tree");
    assert!(
        !String::from_utf8(body).unwrap().contains(HIDE),
        "loopback shell must not hide the annotate form"
    );
}

/// Belt and braces for the test fixtures themselves: the kernel resolves the
/// annotation sink directly too.
#[test]
fn fixture_sanity_direct_issue() {
    let kernel = test_kernel();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let repr = runtime
        .block_on(
            kernel.issue(
                Request::new(Verb::Sink, Iri::parse("urn:annotation").unwrap())
                    .with_arg("body", ArgRef::Inline(b"x".to_vec()))
                    .with_arg("exact", ArgRef::Inline(b"y".to_vec()))
                    .with_arg("target", ArgRef::Inline(b"z".to_vec())),
                &Capability::root(),
            ),
        )
        .unwrap();
    assert!(String::from_utf8(repr.bytes).unwrap().contains("body=x"));
}
