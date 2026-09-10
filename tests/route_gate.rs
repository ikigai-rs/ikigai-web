//! The annotation write route, witnessed by SIDE EFFECT rather than by status code.
//!
//! `tests/http.rs` already pins what each refused shape ANSWERS — 405 outside the annotation
//! family, 405 across the colon anchor, 403 off loopback. What no status code can say is
//! whether the refusal happened BEFORE the write. A route that issued the `Sink` and then
//! answered 403 would pass every one of those tests; a gate is only a gate if nothing
//! downstream ran.
//!
//! So this file gives the annotation endpoint a ledger and reads it. That is the witness
//! conformance PENDING #125 asks for and the one `ikigai-web-demo`'s `tests/k_adapter.rs`
//! settled on for the same reason: a refusal inside `invoke` — and, here, a refusal in front
//! of the kernel entirely — leaves NO trace event, because core records an invocation only
//! after `invoke` returns `Ok`. The disk (there) and a shared vector (here) are the only
//! things that can be asked.
//!
//! ## ★ And the finding this file exists to pin
//!
//! `ikigai-web-demo`'s adapter runs each step under a SESSION capability, so a step naming a
//! gated resource is refused at the kernel's floor. **This face has no such capability.**
//! Every request `serve.rs` issues — `get`, `head`, `post`, both `/k/` commands — is issued
//! under `Capability::root()`, and there is no parameter, header or config key by which it
//! could be anything else. So on the annotation route the kernel's floor is not merely
//! unreached, it is *unreachable*: the same Sink that a capability lacking
//! `urn:cap:iki:annotate` is refused at the kernel is ACCEPTED through the route.
//!
//! [`the_face_issues_under_root_so_a_declared_scope_is_never_the_gate`] pins both halves of
//! that, because it is a design fact and not an accident: what protects this server's one
//! write route is the ROUTE ALLOW-LIST (two annotation roots, colon-anchored) and the BIND
//! POSTURE (off loopback the write surface is gone), which is exactly why the crate docs call
//! loopback's trust model "the local owner" and say plainly that real authentication here is
//! the passkey → capability-workspace arc. If that arc lands and this face starts minting
//! attenuated capabilities, this test is where the change announces itself.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, Mutex};

use ikigai_core::{
    ActionSpec, ArgRef, ArgSpec, Capability, Description, EndpointSpace, Error, Exact, Fallback,
    FnEndpoint, Iri, Kernel, ReprType, Representation, Request, Space, Verb,
};

/// The scope the annotation endpoint declares — and therefore, per the recipe, enforces.
const CAP_ANNOTATE: &str = "urn:cap:iki:annotate";
const XSD_STRING: &str = "http://www.w3.org/2001/XMLSchema#string";

/// Everything the annotation Sink was actually asked to write, in order.
type Witness = Arc<Mutex<Vec<String>>>;

/// The annotation family, as a conformant endpoint that REMEMBERS: `content` on the Sink
/// (pipeline citizenship), a declared cap scope, typed inputs, a declared output — and every
/// firing appended to `witness`, so "nothing was written" is a thing a test can read.
fn recording_annotation(witness: Witness) -> FnEndpoint {
    FnEndpoint::new("annotation", move |inv| {
        if inv.request.verb != Verb::Sink {
            return Err(Error::Endpoint("annotation: Sink only".into()));
        }
        let content = inv.inline_str("content")?.to_string();
        witness.lock().expect("witness").push(content.clone());
        Ok(Representation::new(
            ReprType::new("text/plain"),
            format!("wrote {content}"),
        ))
    })
    .with_description(
        Description::new("annotation")
            .title("Mint an annotation")
            .action(
                ActionSpec::new(Verb::Sink)
                    .summary("Mint an annotation from the piped content")
                    .input(
                        ArgSpec::new("content")
                            .class(XSD_STRING)
                            .summary("the annotation body"),
                    )
                    .output("text/plain")
                    .requires(CAP_ANNOTATE),
            ),
    )
}

/// A kernel binding the annotation family under both spellings (the transition window this
/// server's route gate exists to serve) plus one unrelated resource to POST at.
fn witnessed_kernel(witness: Witness) -> Kernel {
    let space = EndpointSpace::new()
        .bind(
            Exact::new("urn:iki:annotation"),
            recording_annotation(Arc::clone(&witness)),
        )
        .bind(
            Exact::new("urn:iki:annotation:abc"),
            recording_annotation(Arc::clone(&witness)),
        )
        .bind(
            Exact::new("urn:annotation"),
            recording_annotation(Arc::clone(&witness)),
        )
        // Deliberately NOT in the annotation family, and deliberately a Sink that would
        // record if it were ever reached: the route gate is the only thing stopping it.
        .bind(
            Exact::new("urn:iki:annotationx"),
            recording_annotation(Arc::clone(&witness)),
        )
        .bind(Exact::new("urn:hello"), recording_annotation(witness));
    Kernel::new(Arc::new(Fallback::new(vec![
        Arc::new(space) as Arc<dyn Space>
    ])))
}

/// A running server plus the witness its kernel writes into.
struct Server {
    addr: std::net::SocketAddr,
    witness: Witness,
}

fn spawn(bind_ip: &str) -> Server {
    let witness: Witness = Arc::default();
    let kernel = Arc::new(witnessed_kernel(Arc::clone(&witness)));
    let addr: std::net::SocketAddr = format!("{bind_ip}:0").parse().expect("a bind address");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(async move {
            let listener = ikigai_web_server::serve::bind(addr).await.expect("bind");
            tx.send(listener.local_addr().expect("local_addr"))
                .expect("send");
            ikigai_web_server::serve::serve(kernel, listener).await
        })
    });
    Server {
        addr: rx.recv().expect("the server reports where it bound"),
        witness,
    }
}

/// A loopback server — [`ikigai_web_server::serve::Posture::LocalOwner`], the full surface.
///
/// One PER TEST, deliberately, where `tests/http.rs` shares a single server through a
/// `OnceLock`. That file asserts status codes, which are per-request; this one asserts a
/// LEDGER, which is per-server — and cargo runs tests in parallel threads, so a shared
/// witness would carry another test's writes and every assertion here would be about the
/// interleaving. The cost is a socket per test; the alternative is a flaky witness.
fn loopback() -> Server {
    spawn("127.0.0.1")
}

/// A non-loopback server — `Posture::ReadOnly`. Reached over 127.0.0.1; the posture keys on
/// the BOUND address, not the caller's.
fn readonly() -> Server {
    let bound = spawn("0.0.0.0");
    Server {
        addr: format!("127.0.0.1:{}", bound.addr.port())
            .parse()
            .expect("loopback address"),
        witness: bound.witness,
    }
}

fn roundtrip(addr: std::net::SocketAddr, request: &str) -> (u16, String) {
    let mut stream = TcpStream::connect(addr).expect("connect");
    stream.write_all(request.as_bytes()).expect("write");
    let mut response = Vec::new();
    stream.read_to_end(&mut response).expect("read");
    let head_end = response
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("a complete response head");
    let head = std::str::from_utf8(&response[..head_end]).expect("UTF-8 head");
    let status = head
        .split("\r\n")
        .next()
        .and_then(|line| line.split(' ').nth(1))
        .and_then(|code| code.parse().ok())
        .expect("a status line");
    (
        status,
        String::from_utf8_lossy(&response[head_end + 4..]).into_owned(),
    )
}

/// POST `body` as `text/plain` — so it arrives as the piped `content` arg, which is what the
/// annotation Sink declares and reads.
fn post(addr: std::net::SocketAddr, path: &str, body: &str) -> (u16, String) {
    roundtrip(
        addr,
        &format!(
            "POST {path} HTTP/1.1\r\nHost: t\r\nContent-Type: text/plain\r\n\
             Content-Length: {}\r\n\r\n{body}",
            body.len()
        ),
    )
}

fn wrote(server: &Server) -> Vec<String> {
    server.witness.lock().expect("witness").clone()
}

/// The witness works: a permitted write through the real route lands exactly once, under both
/// spellings and through both entrances (`POST /{uri}` and the `/k/sink` adapter).
///
/// This has to come first. A witness that cannot fire proves nothing about the refusals, and
/// "no write was recorded" is only evidence once a write has been recorded.
#[test]
fn a_permitted_write_lands_once_through_every_entrance() {
    let server = &loopback();
    let before = wrote(server).len();
    for (path, mark) in [
        ("/urn:iki:annotation", "new-spelling"),
        ("/urn:annotation", "legacy-spelling"),
        ("/urn:iki:annotation:abc", "slug"),
        ("/k/sink%20urn:iki:annotation", "adapter"),
    ] {
        let (status, body) = post(server.addr, path, mark);
        assert_eq!(status, 200, "for {path}: {body}");
        assert!(body.contains(mark), "for {path}: {body}");
    }
    let after = wrote(server);
    assert_eq!(
        &after[before..],
        [
            "new-spelling".to_string(),
            "legacy-spelling".to_string(),
            "slug".to_string(),
            "adapter".to_string()
        ],
        "each permitted write landed exactly once, in order"
    );
}

/// ★ Every refused shape writes NOTHING. The refusal is in front of the kernel, not behind
/// it — which is what "the write surface is GONE, not gated" has to mean to be worth saying.
///
/// The shapes, and which gate each one meets:
///
/// - `POST /urn:hello` — outside the annotation family: the route allow-list.
/// - `POST /urn:iki:annotationx` — inside the annotation PREFIX but across the colon anchor:
///   the allow-list again, and this is the shape a `starts_with` that forgot the colon would
///   have let through. The IRI is BOUND to a recording endpoint here on purpose, so if the
///   anchor ever slips the witness says so instead of a 404 hiding it.
/// - `GET /urn:iki:annotation` — the verb map: GET is `Source`, never `Sink`.
/// - `POST /urn:iki:annotation` at a non-loopback bind — the posture gate, ahead of all
///   dispatch.
/// - `POST /k/sink urn:hello` — the adapter honors the same allow-list as the direct route,
///   which is the point of there being ONE list.
#[test]
fn every_refusal_happens_in_front_of_the_kernel() {
    let server = &loopback();
    // Land one write first, so an empty tail is a fact about these calls and not about a
    // witness that never worked.
    post(server.addr, "/urn:iki:annotation", "sentinel");
    let before = wrote(server).len();
    const BODY: &str = "nope";
    for (request, expected, why) in [
        ("POST /urn:hello", 405u16, "outside the annotation family"),
        ("POST /urn:iki:annotationx", 405, "across the colon anchor"),
        ("POST /k/sink%20urn:hello", 405, "the adapter's allow-list"),
        (
            "DELETE /urn:iki:annotation",
            405,
            "Delete is not a verb this face maps",
        ),
    ] {
        let (status, _) = roundtrip(
            server.addr,
            &format!(
                "{request} HTTP/1.1\r\nHost: t\r\nContent-Type: text/plain\r\n\
                 Content-Length: {}\r\n\r\n{BODY}",
                BODY.len()
            ),
        );
        assert_eq!(status, expected, "{why}");
    }
    // GET on the annotation IRI is a Source, and this endpoint has none: an endpoint error,
    // never a write.
    let (status, _) = roundtrip(
        server.addr,
        "GET /urn:iki:annotation HTTP/1.1\r\nHost: t\r\n\r\n",
    );
    assert_eq!(
        status, 500,
        "GET is Source; the annotation endpoint has none"
    );
    assert_eq!(
        wrote(server).len(),
        before,
        "not one of the refused shapes reached the Sink: {:?}",
        &wrote(server)[before..]
    );

    // The posture gate, on its own server: off loopback the write surface is gone, under
    // every spelling and both entrances — and its witness stays empty from the first byte.
    let ro = &readonly();
    for path in [
        "/urn:iki:annotation",
        "/urn:annotation",
        "/urn:iki:annotation:abc",
        "/k/sink%20urn:iki:annotation",
        "/k/sink%20urn:annotation",
    ] {
        let (status, body) = post(ro.addr, path, "nope");
        assert_eq!(status, 403, "for {path}: {body}");
        assert!(body.contains("read-only"), "for {path}: {body}");
    }
    assert!(
        wrote(ro).is_empty(),
        "a read-only bind has never written anything: {:?}",
        wrote(ro)
    );
}

/// ★ **The face issues under `Capability::root()`, so a declared scope is never the gate
/// here** — stated as a test because it is the load-bearing difference between this server
/// and every host that runs steps under a session capability.
///
/// Both halves, against the same kernel and the same endpoint:
///
/// - At the KERNEL, the annotation Sink declares `urn:cap:iki:annotate`, so a capability
///   holding no grant under it is refused with a typed `Error::Denied` naming the scope,
///   before the endpoint runs — the floor, and the witness is untouched (the shape
///   `ikigai-web-demo`'s `k_adapter.rs` pins for its own gated steps).
/// - Through the ROUTE, the identical write is ACCEPTED, because `serve.rs` issues every
///   request under root. The capability is not attenuated, weakened, or derived from
///   anything about the request; there is nothing to lack.
///
/// So the annotation route's protection is the allow-list plus the bind posture, and this
/// server must not be bound beyond loopback with mounts it does not want the network to
/// write to — which is precisely what the crate's trust-posture doc says, now with a test
/// that fails if either half stops being true.
#[test]
fn the_face_issues_under_root_so_a_declared_scope_is_never_the_gate() {
    // The kernel half: the same endpoint, the same request, minus root.
    let witness: Witness = Arc::default();
    let kernel = witnessed_kernel(Arc::clone(&witness));
    let spec = kernel
        .describe(&Iri::parse("urn:iki:annotation".to_string()).expect("iri"))
        .expect("the annotation endpoint describes itself")
        .action_specs()
        .into_iter()
        .find(|a| a.verb == Verb::Sink)
        .expect("Sink is declared");
    assert_eq!(
        spec.requires,
        vec![CAP_ANNOTATE.to_string()],
        "the endpoint declares the scope it enforces"
    );
    let write = || {
        Request::new(
            Verb::Sink,
            Iri::parse("urn:iki:annotation".to_string()).expect("iri"),
        )
        .with_arg("content", ArgRef::Inline(b"unauthorized".to_vec()))
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    match runtime.block_on(kernel.issue(write(), &Capability::scoped(Vec::<String>::new()))) {
        Err(Error::Denied(detail)) => assert!(
            detail.contains(CAP_ANNOTATE),
            "the refusal names the declared scope: {detail}"
        ),
        other => panic!("under no grants: Denied, got {other:?}"),
    }
    assert!(
        witness.lock().expect("witness").is_empty(),
        "the floor refused before `invoke`, so nothing was written — and core traces no \
         event for it, which is why the witness is the vector and not the tracer"
    );
    // The same capability that WOULD be refused is never formed at the HTTP face.
    runtime
        .block_on(kernel.issue(write(), &Capability::root()))
        .expect("root holds every scope");
    assert_eq!(
        witness.lock().expect("witness").as_slice(),
        ["unauthorized".to_string()],
        "under root the identical write lands"
    );

    // The route half, on the live server: accepted, because root is what it issues under.
    let server = &loopback();
    let before = wrote(server).len();
    let (status, body) = post(server.addr, "/urn:iki:annotation", "no-capability-required");
    assert_eq!(
        status, 200,
        "the route issues under root: a write no capability was presented for is accepted"
    );
    assert!(body.contains("no-capability-required"), "{body}");
    assert_eq!(
        wrote(server)[before..],
        ["no-capability-required".to_string()],
        "and it really landed"
    );
}
