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
        .bind(Exact::new("urn:missing"), erroring("missing"))
        .bind(Exact::new("urn:denied"), erroring("denied"))
        .bind(Exact::new("urn:asleep"), erroring("asleep"));
    Kernel::new(Arc::new(Fallback::new(vec![
        Arc::new(space) as Arc<dyn Space>
    ])))
}

/// One server for the whole test binary, on an OS-assigned port.
fn server_addr() -> std::net::SocketAddr {
    static ADDR: OnceLock<std::net::SocketAddr> = OnceLock::new();
    *ADDR.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                let listener = ikigai_web::serve::bind(0).await.unwrap();
                tx.send(listener.local_addr().unwrap()).unwrap();
                ikigai_web::serve::serve(Arc::new(test_kernel()), listener).await
            })
        });
        rx.recv().unwrap()
    })
}

/// Send raw HTTP, return (status, headers-lowercased, body).
fn roundtrip(request: &str) -> (u16, Vec<(String, String)>, Vec<u8>) {
    let mut stream = TcpStream::connect(server_addr()).unwrap();
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

#[test]
fn cacheable_representations_project_immutable() {
    let (_, headers, _) = get("/urn:pure");
    assert_eq!(
        header(&headers, "cache-control"),
        Some("public, max-age=31536000, immutable")
    );
    // No ETag until golden-thread validity crosses the wire (see crate doc).
    assert_eq!(header(&headers, "etag"), None);
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
