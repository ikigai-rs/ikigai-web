//! A Conflict raised on the FAR side of an IPC mount answers 409 here — and only
//! because this server's wire speaks v8.
//!
//! `tests/http.rs` pins the local half: an endpoint in this process returns
//! `Error::Conflict` and the face answers 409. This file pins the half that went
//! through the wire and used to be lost there. Before `ikigai-ipc`/`ikigai-resolve`
//! 0.1.30 this server spoke wire v7, which has no `Conflict` variant, so a v8
//! daemon downgraded the error to the untyped `Endpoint("conflict: …")` a v7
//! client can decode — and `serve::status_of` maps `Endpoint` to 500.
//!
//! Two peers, the same endpoints, the same requests, the same HTTP face:
//!
//! - **v8**: a real [`ikigai_ipc::serve`] kernel server, mounted with the
//!   production [`compose`]. The conflict crosses typed and answers 409 with the
//!   bare `conflict: …` message as the body.
//! - **v7**: the same server behind a hello shim that speaks wire v7 exactly as
//!   `ikigai-ipc` 0.1.29 did. It answers 500, and the message survives as an
//!   endpoint error. That is the behavior the v8 pin replaces, stated so that a
//!   change to it is a decision rather than an accident.
//!
//! ## Why the v7 peer is a shim, not the real 0.1.29
//!
//! A real v7 server cannot join this test binary: `ikigai-ipc` 0.1.29 and 0.1.30
//! are both `^0.1`, so cargo unifies them to one version per graph. Renaming the
//! dependency does not help. What is v7-specific on a Unix socket is small and
//! public, though: the hello exchange, and `Reply::for_peer(7)` on every reply.
//! The shim does the first by hand, matching 0.1.29's `handle_connection`
//! (answer 7 to any hello, close unless the client offered 7), and gets the
//! second for free by dialing the real v8 server AT v7, so the real server's own
//! downgrade code produces the bytes. Every call and reply frame is relayed
//! untouched. What it cannot catch is a v7 SERVER bug that 0.1.30 does not
//! share — which is not what this file is about.
//!
//! The client half is the real one throughout. Against the shim, `ikigai-ipc`
//! 0.1.30 offers v8, is answered 7 and closed, redials at 7 and is served there:
//! the per-connection negotiation the v8 release introduced, exercised end to end.

mod ready;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ikigai_core::{
    EndpointSpace, Error, Exact, Fallback, FnEndpoint, Kernel, Representation, Space,
};
use ikigai_web_server::mounts::{compose, parse_mount_line};
use ikigai_wire::{decode_hello, read_frame, write_hello, Hello, HelloMode};

/// The message the far side refuses with — the whole point of a conflict is WHAT
/// state refused, so every pin below checks it arrives intact.
const TAKEN: &str = "1,1 is taken — X played there";

/// The fixture peer: one read and one write that each refuse on state.
///
/// The write lives under `urn:iki:annotation:` because that is the ONE family
/// this server's POST route admits (`serve::ANNOTATION_ROOTS`); a write anywhere
/// else is refused at the route before it reaches a mount, and tests nothing.
fn peer_kernel() -> Kernel {
    let refuses = || {
        FnEndpoint::new("taken", |_inv| -> Result<Representation, Error> {
            Err(Error::Conflict(TAKEN.into()))
        })
    };
    let space = EndpointSpace::new()
        .bind(Exact::new("urn:game:board"), refuses())
        .bind(Exact::new("urn:iki:annotation:taken"), refuses());
    // A renderer, so the mount's best-effort describe round-trip has a contract
    // to fetch (see `tests/conformance.rs`, `peer_kernel`). Not load-bearing for
    // the statuses; it keeps the peer shaped like a real daemon.
    Kernel::with_meta_renderer(
        Arc::new(Fallback::new(vec![Arc::new(space) as Arc<dyn Space>])),
        Arc::new(ikigai_vocab::TurtleRenderer),
    )
}

/// A real v8 kernel server over [`peer_kernel`], on `socket`.
fn serve_v8(socket: &Path) {
    let path = socket.to_path_buf();
    std::thread::spawn(move || {
        // Serves until the process ends; the tempdir outlives the test.
        let _ = ikigai_ipc::serve(peer_kernel(), &path);
    });
    ready::socket(socket);
}

/// A wire-v7 kernel server on `socket`: a hello shim in front of the v8 server
/// at `upstream`. See the module docs for what it reproduces and what it cannot.
fn serve_v7(socket: &Path, upstream: PathBuf) {
    let listener = UnixListener::bind(socket).expect("bind the v7 shim");
    std::thread::spawn(move || {
        for client in listener.incoming() {
            let Ok(client) = client else { return };
            let upstream = upstream.clone();
            std::thread::spawn(move || relay_as_v7(client, &upstream));
        }
    });
}

/// One v7 connection, as `ikigai-ipc` 0.1.29's `handle_connection` ran it.
fn relay_as_v7(mut client: UnixStream, upstream: &Path) {
    let Ok(first) = read_frame(&mut client) else {
        return;
    };
    let Some(offered) = decode_hello(&first) else {
        return; // a pre-hello client; v7 refused those too
    };
    // A v7 server answered EVERY hello with 7 and closed on anything else. That
    // answer is what tells a v8 client to redial at 7.
    let answer = Hello {
        version: 7,
        mode: HelloMode::Verbatim,
    };
    if write_hello(&mut client, &answer).is_err() || offered.version != 7 {
        return;
    }
    // Dial the real server AT v7, so its `Reply::for_peer(7)` does the downgrade.
    let Ok(mut server) = UnixStream::connect(upstream) else {
        return;
    };
    let hello = Hello {
        version: 7,
        mode: offered.mode,
    };
    if write_hello(&mut server, &hello).is_err() {
        return;
    }
    match read_frame(&mut server).ok().and_then(|p| decode_hello(&p)) {
        Some(Hello { version: 7, .. }) => {}
        other => panic!("the v8 server must serve a v7 hello at 7, answered {other:?}"),
    }
    // From here on the frames are postcard `Call`/`Reply` at v7: relay them.
    let (Ok(mut client_rx), Ok(mut server_tx)) = (client.try_clone(), server.try_clone()) else {
        return;
    };
    let upward = std::thread::spawn(move || {
        let _ = std::io::copy(&mut client_rx, &mut server_tx);
        let _ = server_tx.shutdown(std::net::Shutdown::Write);
    });
    let _ = std::io::copy(&mut server, &mut client);
    let _ = client.shutdown(std::net::Shutdown::Write);
    let _ = upward.join();
}

/// The served kernel, composed the way production composes it: `override`
/// mounts over the peer at `socket`, one per namespace the fixture binds.
fn mounted_kernel(socket: &Path) -> Kernel {
    let lines = ["urn:game:", "urn:iki:annotation:"]
        .iter()
        .map(|prefix| {
            parse_mount_line(&format!("override {prefix}={}", socket.display()))
                .expect("a valid mount line")
        })
        .collect();
    compose(lines).unwrap_or_else(|e| panic!("compose over {}: {e}", socket.display()))
}

/// Serve `kernel` over HTTP on an OS-assigned loopback port (the LocalOwner
/// posture, so the POST route is live).
fn spawn_http(kernel: Kernel) -> std::net::SocketAddr {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async move {
            let listener = ikigai_web_server::serve::bind("127.0.0.1:0".parse().unwrap())
                .await
                .unwrap();
            tx.send(listener.local_addr().unwrap()).unwrap();
            ikigai_web_server::serve::serve(
                Arc::new(kernel),
                listener,
                ikigai_web_server::ceiling::Ceiling::unset(),
            )
            .await
        })
    });
    rx.recv().unwrap()
}

/// Send raw HTTP, return (status, body).
fn roundtrip(addr: std::net::SocketAddr, request: &str) -> (u16, String) {
    let mut stream = TcpStream::connect(addr).unwrap();
    stream.write_all(request.as_bytes()).unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).unwrap();
    let head_end = response
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("a complete response head");
    let status = std::str::from_utf8(&response[..head_end])
        .unwrap()
        .split(' ')
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    let body = String::from_utf8(response[head_end + 4..].to_vec()).unwrap();
    (status, body)
}

fn get(addr: std::net::SocketAddr) -> (u16, String) {
    roundtrip(addr, "GET /urn:game:board HTTP/1.1\r\nHost: t\r\n\r\n")
}

fn head(addr: std::net::SocketAddr) -> (u16, String) {
    roundtrip(addr, "HEAD /urn:game:board HTTP/1.1\r\nHost: t\r\n\r\n")
}

fn post(addr: std::net::SocketAddr) -> (u16, String) {
    let form = "body=mine";
    roundtrip(
        addr,
        &format!(
            "POST /urn:iki:annotation:taken HTTP/1.1\r\nHost: t\r\n\
             Content-Type: application/x-www-form-urlencoded\r\n\
             Content-Length: {}\r\n\r\n{form}",
            form.len()
        ),
    )
}

/// ★ The pin this release exists for: a remote Conflict is a 409, on every route
/// that can carry one, and the body is the typed error's own `conflict: …` — not
/// an endpoint error that happens to contain the word.
#[test]
fn a_conflict_from_a_v8_peer_answers_409() {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket = dir.path().join("v8.sock");
    serve_v8(&socket);
    let addr = spawn_http(mounted_kernel(&socket));

    let expected = format!("conflict: {TAKEN}\n");
    let (status, body) = get(addr);
    assert_eq!(status, 409, "GET across the mount; body was: {body}");
    assert_eq!(
        body, expected,
        "the typed error's own message, nothing wrapped"
    );

    let (status, body) = post(addr);
    assert_eq!(status, 409, "POST across the mount; body was: {body}");
    assert_eq!(body, expected);

    let (status, body) = head(addr);
    assert_eq!(status, 409, "HEAD across the mount");
    assert!(body.is_empty(), "HEAD carries no body: {body}");
}

/// The behavior v8 replaced, pinned against a v7 peer: the same Conflict arrives
/// downgraded to `Endpoint("conflict: …")`, answers 500, and keeps its message.
/// If this ever answers 409, something is parsing messages — which the wire's
/// typed taxonomy exists to make unnecessary — and that is worth knowing.
#[test]
fn a_conflict_from_a_v7_peer_still_answers_500() {
    let dir = tempfile::tempdir().expect("tempdir");
    let upstream = dir.path().join("v8.sock");
    let socket = dir.path().join("v7.sock");
    serve_v8(&upstream);
    serve_v7(&socket, upstream);
    let addr = spawn_http(mounted_kernel(&socket));

    for (route, (status, body)) in [("GET", get(addr)), ("POST", post(addr))] {
        assert_eq!(status, 500, "{route} across a v7 mount; body was: {body}");
        assert!(
            body.contains(&format!("conflict: {TAKEN}")),
            "{route}: the message survives the downgrade: {body}"
        );
        assert_ne!(
            body,
            format!("conflict: {TAKEN}\n"),
            "{route}: over v7 it is an endpoint error, not the typed conflict"
        );
    }
    assert_eq!(head(addr).0, 500, "HEAD across a v7 mount");
}
