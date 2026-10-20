//! The door: what this server does about a page on ANOTHER origin, and about content it serves
//! that it did not write (ledger #1795, #1813).
//!
//! Three claims, each witnessed rather than inferred:
//!
//! 1. **A cross-site write is refused before it lands.** A browser attaches nothing that
//!    proves intent to a form POST another site makes, but it cannot lie about `Origin` or
//!    `Sec-Fetch-Site`. On the loopback posture the annotation Sink is live and every request
//!    is root, so a page on `evil.example` that posts to `http://127.0.0.1:8642/k/sink …` is
//!    the whole attack. The witness is the Sink's own record, never the status alone: a door
//!    that issued the write and then answered 403 would pass a status-only test.
//! 2. **A representation this server relays cannot run script as this origin.** The kernel
//!    serves whatever its mounts answer, and a mounted file jail answers `text/html` for a
//!    `.html` file (ikigai-fs maps the extension), so a planted page would otherwise run with
//!    the browse pages' authority: read anything the ceiling reaches, and write.
//! 3. **This server's own pages carry the door's headers**, and nothing on them needs an
//!    inline script or style that the policy would block.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, Mutex, OnceLock};

use ikigai_core::{
    EndpointSpace, Error, Exact, Fallback, FnEndpoint, Kernel, ReprType, Representation, Space,
    Verb,
};
use ikigai_web_server::ceiling::Ceiling;

/// Every value the annotation Sink was asked to write, in order.
type Witness = Arc<Mutex<Vec<String>>>;

/// The annotation family's Sink, remembering what it wrote. A form post carries `body`; a
/// piped one carries `content`; either is recorded.
fn recording_annotation(witness: Witness) -> FnEndpoint {
    FnEndpoint::new("annotation", move |inv| {
        if inv.request.verb != Verb::Sink {
            return Err(Error::Endpoint("annotation: Sink only".into()));
        }
        let wrote = inv
            .inline_str("body")
            .or_else(|_| inv.inline_str("content"))
            .unwrap_or("<empty>")
            .to_string();
        witness.lock().expect("witness").push(wrote.clone());
        Ok(Representation::new(
            ReprType::new("text/plain"),
            format!("wrote {wrote}"),
        ))
    })
}

/// A page planted in a writable file jail: what a mounted ikigai-fs answers for `evil.html`.
const PLANTED: &str = "<!doctype html><title>notes</title><script>\
    fetch('/k/sink urn:iki:annotation',{method:'POST',body:'body=pwned'})</script>";

/// The same, as SVG, which a browser also runs script in when it is navigated to directly.
const PLANTED_SVG: &str = "<svg xmlns=\"http://www.w3.org/2000/svg\">\
    <script>fetch('/k/sink urn:iki:annotation',{method:'POST',body:'body=pwned'})</script></svg>";

fn served(media: &'static str, body: &'static str) -> FnEndpoint {
    FnEndpoint::new("jail", move |_inv| {
        Ok(Representation::new(ReprType::new(media), body))
    })
}

fn kernel(witness: Witness) -> Kernel {
    let space = EndpointSpace::new()
        .bind(
            Exact::new("urn:iki:annotation"),
            recording_annotation(Arc::clone(&witness)),
        )
        .bind(
            Exact::new("urn:iki:annotation:abc"),
            recording_annotation(witness),
        )
        .bind(
            Exact::new("urn:file:jail:evil.html"),
            served("text/html", PLANTED),
        )
        .bind(
            Exact::new("urn:file:jail:evil.svg"),
            served("image/svg+xml", PLANTED_SVG),
        )
        .bind(Exact::new("urn:hello"), served("text/plain", "hello"));
    Kernel::with_meta_renderer(
        Arc::new(Fallback::new(vec![Arc::new(space) as Arc<dyn Space>])),
        Arc::new(ikigai_vocab::TurtleRenderer),
    )
}

struct Server {
    addr: std::net::SocketAddr,
    witness: Witness,
}

/// The loopback posture (the full write surface, every request root), on an OS-assigned port
/// bound IN the server and reported back.
fn server() -> &'static Server {
    static SERVER: OnceLock<Server> = OnceLock::new();
    SERVER.get_or_init(|| {
        let witness: Witness = Arc::default();
        let kernel = Arc::new(kernel(Arc::clone(&witness)));
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("runtime");
            runtime.block_on(async move {
                let listener = ikigai_web_server::serve::bind("127.0.0.1:0".parse().unwrap())
                    .await
                    .expect("bind");
                tx.send(listener.local_addr().expect("local_addr"))
                    .expect("send");
                ikigai_web_server::serve::serve(kernel, listener, Ceiling::unset()).await
            })
        });
        Server {
            addr: rx.recv().expect("the server reports where it bound"),
            witness,
        }
    })
}

/// The witness is shared by every test in this binary, so each write carries a marker of its
/// own and a test asks only whether ITS marker landed.
fn landed(marker: &str) -> bool {
    server()
        .witness
        .lock()
        .expect("witness")
        .iter()
        .any(|w| w == marker)
}

fn roundtrip(request: &str) -> (u16, Vec<(String, String)>, Vec<u8>) {
    let mut stream = TcpStream::connect(server().addr).unwrap();
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

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
}

/// A form post of `body={marker}` to `path`, with `extra` header lines (each ending `\r\n`):
/// exactly what an HTML `<form method=post>` on any site sends.
fn form_post(path: &str, marker: &str, extra: &str) -> (u16, String) {
    let form = format!("body={marker}");
    let (status, _, body) = roundtrip(&format!(
        "POST {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n{extra}\
         Content-Type: application/x-www-form-urlencoded\r\n\
         Content-Length: {}\r\n\r\n{form}",
        form.len()
    ));
    (status, String::from_utf8_lossy(&body).into_owned())
}

/// Both entrances to the write route.
const WRITE_PATHS: [&str; 2] = ["/k/sink%20urn:iki:annotation", "/urn:iki:annotation"];

#[test]
fn a_cross_site_form_post_is_refused_before_it_writes() {
    for (n, path) in WRITE_PATHS.iter().enumerate() {
        let marker = format!("cross-site-{n}");
        let (status, body) = form_post(path, &marker, "Origin: http://evil.example\r\n");
        assert!(
            !landed(&marker),
            "a POST from http://evil.example to {path} WROTE (status {status}, {body})"
        );
        assert_eq!(status, 403, "{path}: {body}");
        assert!(
            body.contains("Origin"),
            "the refusal names its rule: {body}"
        );
    }
}

#[test]
fn every_foreign_or_unprovable_origin_is_refused() {
    let port = server().addr.port();
    let cases = [
        "Origin: null\r\n".to_string(),
        format!("Origin: http://127.0.0.1:{}\r\n", port.wrapping_add(1)),
        format!("Origin: https://127.0.0.1:{port}\r\n"),
        "Sec-Fetch-Site: cross-site\r\n".to_string(),
        // Another port on localhost is the same SITE but not the same origin.
        "Sec-Fetch-Site: same-site\r\n".to_string(),
        // A browser's credentials with nothing saying where the request came from.
        "Cookie: s=1\r\n".to_string(),
        "Authorization: Basic dTpw\r\n".to_string(),
    ];
    for (n, extra) in cases.iter().enumerate() {
        for (p, path) in WRITE_PATHS.iter().enumerate() {
            let marker = format!("foreign-{n}-{p}");
            let (status, body) = form_post(path, &marker, extra);
            assert!(
                !landed(&marker),
                "{extra:?} to {path} WROTE ({status}, {body})"
            );
            assert_eq!(status, 403, "{extra:?} to {path}: {body}");
        }
    }
}

#[test]
fn this_doors_own_origins_and_non_browser_clients_still_write() {
    let port = server().addr.port();
    let cases = [
        format!("Origin: http://127.0.0.1:{port}\r\n"),
        // The loopback bind is reached by name too.
        format!("Origin: http://localhost:{port}\r\n"),
        "Sec-Fetch-Site: same-origin\r\n".to_string(),
        // What a browser says about a request the person started themselves.
        "Sec-Fetch-Site: none\r\n".to_string(),
        // curl, and a script holding its own bearer token: no ambient credential.
        String::new(),
        "Authorization: Bearer t\r\n".to_string(),
    ];
    for (n, extra) in cases.iter().enumerate() {
        for (p, path) in WRITE_PATHS.iter().enumerate() {
            let marker = format!("ours-{n}-{p}");
            let (status, body) = form_post(path, &marker, extra);
            assert_eq!(status, 200, "{extra:?} to {path}: {body}");
            assert!(landed(&marker), "{extra:?} to {path} did not write");
        }
    }
}

/// The CSP value's directives, lowercased and trimmed.
fn directives(csp: &str) -> Vec<String> {
    csp.split(';')
        .map(|d| d.trim().to_ascii_lowercase())
        .filter(|d| !d.is_empty())
        .collect()
}

/// A browser that navigates to a response under this CSP runs no script in it and gives it an
/// opaque origin: `sandbox` with no `allow-scripts` and no `allow-same-origin`.
fn is_sandboxed(headers: &[(String, String)]) -> bool {
    header(headers, "content-security-policy").is_some_and(|csp| {
        directives(csp).iter().any(|d| {
            let mut words = d.split_whitespace();
            words.next() == Some("sandbox")
                && !words.any(|w| w == "allow-scripts" || w == "allow-same-origin")
        })
    })
}

const BROWSER_ACCEPT: &str = "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8";

#[test]
fn a_planted_page_from_a_mounted_jail_cannot_run_as_this_origin() {
    for path in [
        "/urn:file:jail:evil.html",
        "/urn:file:jail:evil.svg",
        "/k/source%20urn:file:jail:evil.html",
    ] {
        let (status, headers, _) = roundtrip(&format!(
            "GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nAccept: {BROWSER_ACCEPT}\r\n\r\n"
        ));
        assert_eq!(status, 200, "{path}");
        assert!(
            is_sandboxed(&headers),
            "{path} is served as {:?} with CSP {:?}: a browser runs its script with this \
             origin's authority",
            header(&headers, "content-type"),
            header(&headers, "content-security-policy"),
        );
        assert_eq!(
            header(&headers, "x-content-type-options"),
            Some("nosniff"),
            "{path}"
        );
    }
}

/// The page policy: the cli ikigai-web library's, whose doctest pins it verbatim.
const PAGE_CSP: &str = ikigai_web::HTML_DOOR_CSP;

#[test]
fn this_servers_own_pages_carry_the_door_headers() {
    for path in ["/", "/browse/urn:hello", "/sparql"] {
        let (status, headers, body) = roundtrip(&format!(
            "GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nAccept: {BROWSER_ACCEPT}\r\n\r\n"
        ));
        assert_eq!(status, 200, "{path}");
        assert_eq!(
            header(&headers, "content-type"),
            Some("text/html; charset=utf-8"),
            "{path}"
        );
        assert_eq!(
            header(&headers, "content-security-policy"),
            Some(PAGE_CSP),
            "{path}"
        );
        assert_eq!(
            header(&headers, "x-content-type-options"),
            Some("nosniff"),
            "{path}"
        );
        assert_eq!(
            header(&headers, "referrer-policy"),
            Some("no-referrer"),
            "{path}"
        );
        assert_eq!(
            header(&headers, "cross-origin-opener-policy"),
            Some("same-origin"),
            "{path}"
        );
        // And the page works under that policy: no inline script or style for it to block.
        let body = String::from_utf8(body).unwrap();
        assert!(!body.contains("<style"), "{path} has an inline <style>");
        assert!(!body.contains(" style=\""), "{path} has a style attribute");
        for (i, _) in body.match_indices("<script") {
            let tag = &body[i..i + body[i..].find('>').unwrap()];
            assert!(
                tag.contains(" src=\""),
                "{path} has an inline script: {tag}"
            );
        }
    }
}
