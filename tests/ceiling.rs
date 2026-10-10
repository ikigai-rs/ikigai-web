//! The capability ceiling, end to end over a real IPC mount (ledger #224, #837).
//!
//! ## The defect this file reproduces
//!
//! On 2026-10-07 the LAN-bound face at 8642 listed the whole work ledger
//! (`GET /urn:iki:ledger:items`) and answered `urn:iki:store:select` with 12,319 ledger quads,
//! to anyone on the network. The mounted peer (gonk) enforces its scopes exactly, per ledger
//! and per graph; the face handed it `Capability::root()` on every request, so there was
//! nothing to enforce against. The off-loopback GET/HEAD gate stopped writes and nothing
//! stopped reads.
//!
//! The peer here is a small stand-in with the same shape: a ledger listing that requires
//! `urn:cap:ledger:read:default`, a browse tree that requires a `urn:cap:browse:read:` grant,
//! a repo-list facade that requires `urn:cap:exec:git`, and a `urn:sparql:select` whose default
//! dataset is the UNION of the graphs the caller's capability may read (gonk's rule), with
//! `graph=` refused for a graph the caller cannot read. Before the ceiling,
//! [`a_lan_face_with_the_browse_only_ceiling_serves_browse_and_not_the_ledger`] failed on its
//! first assertion: the ledger answered 200 with its contents, and `/sparql` listed the ledger
//! graph beside the browse graph.
//!
//! ## What is pinned
//!
//! - **Sealed, per route.** With a ceiling set, no route, header, query parameter or `as=`
//!   reaches anything the ceiling does not grant: `/{uri}`, `HEAD`, `/k/source`, `/sparql`
//!   (GET, POST and the editor page), and the index's repo list.
//! - **Browse still works** under the browse-only ceiling, and `/sparql` over the union reads
//!   the browse graph only.
//! - **Unset off loopback refuses to start**, in the library (`serve` panics before accepting)
//!   and in the binary (exit 1 before binding, naming `web.cap`).
//! - **Unset on loopback is root**, unchanged: the control that proves the fixture can leak.

mod ready;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;

use ikigai_core::{
    ActionSpec, ArgSpec, Capability, Description, EndpointSpace, Error, Exact, Fallback,
    FnEndpoint, Kernel, ReprType, Representation, Space, Verb,
};
use ikigai_web_server::ceiling::{Ceiling, BROWSE_ONLY};
use ikigai_web_server::mounts::{compose, parse_mount_line};

const XSD_STRING: &str = "http://www.w3.org/2001/XMLSchema#string";
const LEDGER_SECRET: &str = "ledger item 1: the whole work ledger";
const REPO_LIST_SECRET: &str = "unmounted-private-repo";
const TREE_BODY: &str = "README.md\nsrc/\n";
const BROWSE_GRAPH: &str = "urn:iki:browse:graph:default";
const LEDGER_GRAPH: &str = "urn:iki:ledger:graph:default";

// ---------------------------------------------------------------------------
// The peer: scopes enforced exactly, the way gonk enforces them.
// ---------------------------------------------------------------------------

fn text(body: impl Into<Vec<u8>>, media: &str) -> Representation {
    Representation::new(ReprType::new(media), body.into())
}

/// `urn:iki:ledger:items` — the work ledger, behind its read token, for both the read and
/// the existence probe (so `HEAD` has something to confirm under root).
fn ledger_items() -> FnEndpoint {
    FnEndpoint::new("ledger-items", |inv| match inv.request.verb {
        Verb::Exists => Ok(text("true", "text/plain")),
        _ => Ok(text(LEDGER_SECRET, "text/plain")),
    })
    .with_description(
        Description::new("ledger-items")
            .action(
                ActionSpec::new(Verb::Source)
                    .output("text/plain")
                    .requires("urn:cap:ledger:read:default"),
            )
            .action(
                ActionSpec::new(Verb::Exists)
                    .output("text/plain")
                    .requires("urn:cap:ledger:read:default"),
            ),
    )
}

/// `urn:repo:demo:tree` — a browse root. Declares the offering `urn:cap:browse:read:*` and
/// checks the root exactly at runtime, as `ikigai-browse` does (the literal `*` scope is
/// browse's all-roots grant).
fn browse_tree() -> FnEndpoint {
    FnEndpoint::new("browse-tree", |inv| {
        let cap = inv.capability;
        if cap.allows("urn:cap:browse:read:demo") || cap.allows("urn:cap:browse:read:*") {
            Ok(text(TREE_BODY, "text/plain"))
        } else {
            Err(Error::Denied("urn:cap:browse:read:demo".into()))
        }
    })
    .with_description(
        Description::new("browse-tree").action(
            ActionSpec::new(Verb::Source)
                .output("text/plain")
                .requires("urn:cap:browse:read:*"),
        ),
    )
}

/// `urn:repo:list` — the facade the index reads, behind an exec token.
fn repo_list() -> FnEndpoint {
    FnEndpoint::new("repo-list", |_inv| {
        Ok(text(format!("{REPO_LIST_SECRET}\t/x\n"), "text/plain"))
    })
    .with_description(
        Description::new("repo-list").action(
            ActionSpec::new(Verb::Source)
                .output("text/plain")
                .requires("urn:cap:exec:git"),
        ),
    )
}

/// `urn:sparql:select` — answers `?g` for every graph in the dataset. The default dataset is
/// the union of the graphs the caller may read; `graph=` names graphs, each of which must be
/// readable or the whole query is Denied (whether or not it exists).
fn sparql_select() -> FnEndpoint {
    FnEndpoint::new("sparql-select", |inv| {
        let cap: &Capability = inv.capability;
        let readable = |g: &str| cap.allows(&format!("urn:cap:store:read:graph:{g}"));
        let named: Option<Vec<String>> = inv.inline_str("graph").ok().map(|g| {
            g.split([',', ' '])
                .filter(|s| !s.is_empty())
                .map(String::from)
                .collect()
        });
        let dataset: Vec<String> = match named {
            Some(graphs) => {
                if let Some(denied) = graphs.iter().find(|g| !readable(g)) {
                    return Err(Error::Denied(format!("urn:cap:store:read:graph:{denied}")));
                }
                graphs
            }
            None => [BROWSE_GRAPH, LEDGER_GRAPH]
                .into_iter()
                .filter(|g| readable(g))
                .map(String::from)
                .collect(),
        };
        let mut out = String::from("?g\n");
        for g in dataset {
            out.push_str(&format!("<{g}>\n"));
        }
        Ok(text(out, "text/tab-separated-values"))
    })
    .with_description(
        Description::new("sparql-select").action(
            ActionSpec::new(Verb::Source)
                .input(ArgSpec::new("query").class(XSD_STRING))
                .input(ArgSpec::new("as").class(XSD_STRING).optional())
                .input(ArgSpec::new("graph").class(XSD_STRING).optional())
                .output("text/tab-separated-values")
                .requires("urn:cap:store:read:graph:*"),
        ),
    )
}

fn peer_kernel() -> Kernel {
    let space = EndpointSpace::new()
        .bind(Exact::new("urn:iki:ledger:items"), ledger_items())
        .bind(Exact::new("urn:repo:demo:tree"), browse_tree())
        .bind(Exact::new("urn:repo:list"), repo_list())
        .bind(Exact::new("urn:sparql:select"), sparql_select());
    Kernel::with_meta_renderer(
        Arc::new(Fallback::new(vec![Arc::new(space) as Arc<dyn Space>])),
        Arc::new(ikigai_vocab::TurtleRenderer),
    )
}

// ---------------------------------------------------------------------------
// The face: composed the way production composes it, over the peer's socket.
// ---------------------------------------------------------------------------

struct Face {
    _dir: tempfile::TempDir,
    addr: std::net::SocketAddr,
}

/// A peer on a scratch socket (under `$TMPDIR`, never the scratchpad: a Unix socket path has
/// a 104-byte budget), and a face mounting it with config-shaped `prefer` lines.
fn composed() -> (tempfile::TempDir, Kernel) {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket = dir.path().join("peer.sock");
    let path = socket.clone();
    let served = peer_kernel();
    std::thread::spawn(move || {
        let _ = ikigai_ipc::serve(served, &path);
    });
    ready::socket(&socket);
    let lines = [
        "prefer urn:iki:ledger:",
        "prefer urn:repo:",
        "prefer urn:sparql:",
    ]
    .iter()
    .map(|m| parse_mount_line(&format!("{m}={}", socket.display())).expect("mount"))
    .collect();
    (dir, compose(lines).expect("compose"))
}

fn spawn_face(bind_ip: &str, ceiling: Ceiling) -> Face {
    let (dir, kernel) = composed();
    let kernel = Arc::new(kernel);
    let addr: std::net::SocketAddr = format!("{bind_ip}:0").parse().expect("addr");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(async move {
            let listener = ikigai_web_server::serve::bind(addr).await.expect("bind");
            tx.send(listener.local_addr().expect("addr").port())
                .expect("send");
            ikigai_web_server::serve::serve(kernel, listener, ceiling).await
        })
    });
    let port = rx.recv().expect("the face reports its port");
    Face {
        _dir: dir,
        addr: format!("127.0.0.1:{port}").parse().expect("loopback addr"),
    }
}

/// The LAN shape: bound to 0.0.0.0 (reached over 127.0.0.1; the posture keys on the BOUND
/// address), under the browse-only ceiling.
fn lan_browse_only() -> Face {
    spawn_face(
        "0.0.0.0",
        Ceiling::scoped(BROWSE_ONLY).expect("a valid ceiling"),
    )
}

/// Send a raw request; return the status and the whole response text (head and body).
fn roundtrip(addr: std::net::SocketAddr, request: &str) -> (u16, String) {
    let mut stream = TcpStream::connect(addr).expect("connect");
    stream.write_all(request.as_bytes()).expect("write");
    let mut response = Vec::new();
    stream.read_to_end(&mut response).expect("read");
    let text = String::from_utf8_lossy(&response).into_owned();
    let status = text
        .split(' ')
        .nth(1)
        .and_then(|code| code.parse().ok())
        .expect("a status line");
    (status, text)
}

fn get(addr: std::net::SocketAddr, path: &str, headers: &str) -> (u16, String) {
    roundtrip(
        addr,
        &format!("GET {path} HTTP/1.1\r\nHost: t\r\n{headers}\r\n"),
    )
}

const TSV: &str = "Accept: text/tab-separated-values\r\n";
const SELECT_G: &str = "SELECT%20%3Fg%20WHERE%20%7B%20GRAPH%20%3Fg%20%7B%7D%20%7D";

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// The control: unset on loopback is root, exactly as before, so this fixture DOES leak the
/// ledger when nothing narrows root. Without it, every refusal below could be a fixture that
/// never served anything.
#[test]
fn unset_on_loopback_is_root_and_the_fixture_serves_everything() {
    let face = spawn_face("127.0.0.1", Ceiling::unset());
    let (status, body) = get(face.addr, "/urn:iki:ledger:items", "");
    assert_eq!(status, 200, "{body}");
    assert!(body.contains(LEDGER_SECRET), "{body}");
    let (status, body) = get(face.addr, &format!("/sparql?query={SELECT_G}"), TSV);
    assert_eq!(status, 200, "{body}");
    assert!(
        body.contains(LEDGER_GRAPH) && body.contains(BROWSE_GRAPH),
        "{body}"
    );
    let (status, _) = roundtrip(
        face.addr,
        "HEAD /urn:iki:ledger:items HTTP/1.1\r\nHost: t\r\n\r\n",
    );
    assert_eq!(status, 200, "root confirms the ledger exists");
    let (status, body) = get(face.addr, "/", "Accept: text/html\r\n");
    assert_eq!(status, 200, "{body}");
    assert!(
        body.contains(REPO_LIST_SECRET),
        "root reads the repo list: {body}"
    );
    assert!(
        body.contains("Capability ceiling") && body.contains("issued as root"),
        "the index states that no ceiling is in force: {body}"
    );
}

/// ★ The reproduction, now the pinned behavior: a LAN face under the browse-only ceiling
/// serves browse and the browse graph, and not the ledger. Before the ceiling the first
/// assertion failed with a 200 carrying the ledger's contents.
#[test]
fn a_lan_face_with_the_browse_only_ceiling_serves_browse_and_not_the_ledger() {
    let face = lan_browse_only();
    let (status, body) = get(face.addr, "/urn:iki:ledger:items", "");
    assert!(
        !body.contains(LEDGER_SECRET),
        "the LAN read the ledger: {body}"
    );
    assert_eq!(status, 403, "{body}");
    assert!(
        body.contains("urn:cap:ledger:read:default"),
        "names the scope: {body}"
    );

    // /sparql over the union: the browse graph only.
    let (status, body) = get(face.addr, &format!("/sparql?query={SELECT_G}"), TSV);
    assert_eq!(status, 200, "{body}");
    assert!(body.contains(BROWSE_GRAPH), "{body}");
    assert!(
        !body.contains(LEDGER_GRAPH),
        "the LAN read the ledger graph: {body}"
    );

    // Browse pages still work.
    let (status, body) = get(face.addr, "/urn:repo:demo:tree", "");
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("README.md"), "{body}");
    let (status, body) = get(face.addr, "/k/source%20urn:repo:demo:tree", "");
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("README.md"), "{body}");
}

/// ★ Sealed: no route, header, query parameter or `as=` reaches what the ceiling does not
/// grant. One request per route that issues to the kernel, each aimed at the ledger.
#[test]
fn no_route_header_or_parameter_reaches_past_the_ceiling() {
    let face = lan_browse_only();
    for (what, request) in [
        (
            "GET /{uri}",
            "GET /urn:iki:ledger:items HTTP/1.1\r\nHost: t\r\n\r\n".to_string(),
        ),
        (
            "GET /{uri} with an explicit as=",
            "GET /urn:iki:ledger:items?as=text/turtle HTTP/1.1\r\nHost: t\r\n\r\n".to_string(),
        ),
        (
            "GET /{uri} with a cap= parameter",
            "GET /urn:iki:ledger:items?cap=root&capability=root HTTP/1.1\r\nHost: t\r\n\r\n"
                .to_string(),
        ),
        (
            "GET /{uri} with credential-shaped headers",
            "GET /urn:iki:ledger:items HTTP/1.1\r\nHost: t\r\nAuthorization: Bearer root\r\n\
             Cookie: cap=root\r\nX-Capability: root\r\n\r\n"
                .to_string(),
        ),
        (
            "GET /k/source",
            "GET /k/source%20urn:iki:ledger:items HTTP/1.1\r\nHost: t\r\n\r\n".to_string(),
        ),
        (
            "GET /k/source with k=v args",
            "GET /k/source%20urn:iki:ledger:items%20cap=root%20as=text/html HTTP/1.1\r\n\
             Host: t\r\n\r\n"
                .to_string(),
        ),
    ] {
        let (status, body) = roundtrip(face.addr, &request);
        assert_eq!(status, 403, "{what}: {body}");
        assert!(
            !body.contains(LEDGER_SECRET),
            "{what} read the ledger: {body}"
        );
    }

    // `/sparql` naming the ledger graph: never the ledger graph in the answer. (Today the
    // face does not forward `graph=` at all, so the query runs over the union, which the
    // ceiling has already narrowed to the browse graph; if it ever starts forwarding it,
    // the peer's per-graph refusal is what must hold, and this still pins the outcome.)
    for (what, request) in [
        (
            "GET /sparql naming the ledger graph",
            format!(
                "GET /sparql?query={SELECT_G}&graph={LEDGER_GRAPH} HTTP/1.1\r\nHost: t\r\n{TSV}\r\n"
            ),
        ),
        ("POST /sparql naming the ledger graph", {
            let body = format!(
                "query={SELECT_G}&graph={}",
                LEDGER_GRAPH.replace(':', "%3A")
            );
            format!(
                "POST /sparql HTTP/1.1\r\nHost: t\r\n{TSV}\
                     Content-Type: application/x-www-form-urlencoded\r\n\
                     Content-Length: {}\r\n\r\n{body}",
                body.len()
            )
        }),
    ] {
        let (_status, body) = roundtrip(face.addr, &request);
        assert!(
            !body.contains(LEDGER_GRAPH),
            "{what} read the ledger graph: {body}"
        );
    }

    // HEAD → Exists, refused the same way (no body to inspect, so the status is the claim;
    // root answers 200 here, see the control test).
    let (status, _) = roundtrip(
        face.addr,
        "HEAD /urn:iki:ledger:items HTTP/1.1\r\nHost: t\r\n\r\n",
    );
    assert_eq!(status, 403, "HEAD confirmed or denied past the ceiling");

    // POST /sparql with a raw query body: the union is the browse graph only.
    let query = "SELECT ?g WHERE { GRAPH ?g {} }";
    let (status, body) = roundtrip(
        face.addr,
        &format!(
            "POST /sparql HTTP/1.1\r\nHost: t\r\n{TSV}Content-Type: application/sparql-query\r\n\
             Content-Length: {}\r\n\r\n{query}",
            query.len()
        ),
    );
    assert_eq!(status, 200, "{body}");
    assert!(
        body.contains(BROWSE_GRAPH) && !body.contains(LEDGER_GRAPH),
        "{body}"
    );

    // The editor page runs the query too, under the same ceiling.
    let (status, body) = get(
        face.addr,
        &format!("/sparql?query={SELECT_G}"),
        "Accept: text/html\r\n",
    );
    assert_eq!(status, 200, "{body}");
    assert!(
        body.contains(BROWSE_GRAPH),
        "the editor ran the query: {body}"
    );
    assert!(
        !body.contains(LEDGER_GRAPH),
        "the editor read the ledger graph: {body}"
    );

    // The index reads `urn:repo:list` under the ceiling, which lacks its exec token, and
    // states the ceiling in force.
    let (status, body) = get(face.addr, "/", "Accept: text/html\r\n");
    assert_eq!(status, 200, "{body}");
    assert!(
        !body.contains(REPO_LIST_SECRET),
        "the index read past the ceiling: {body}"
    );
    for scope in BROWSE_ONLY {
        assert!(body.contains(scope), "the index names the ceiling: {body}");
    }
}

/// Unset off loopback: the library refuses to serve. `serve` panics before it accepts a
/// single connection, so no caller of the library can put root on the network either.
#[test]
fn the_library_refuses_an_unset_ceiling_off_loopback() {
    let (_dir, kernel) = composed();
    let kernel = Arc::new(kernel);
    let outcome = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(async move {
            let listener = ikigai_web_server::serve::bind("0.0.0.0:0".parse().unwrap())
                .await
                .expect("bind");
            ikigai_web_server::serve::serve(kernel, listener, Ceiling::unset()).await
        })
    })
    .join();
    let panic = outcome.expect_err("serve must refuse, not run");
    let message = panic
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| panic.downcast_ref::<&str>().map(|s| s.to_string()))
        .unwrap_or_default();
    assert!(
        message.contains("web.cap"),
        "the refusal names the key: {message}"
    );
}

/// Unset off loopback: the BINARY refuses before it binds, with a message naming `web.cap`
/// and carrying the browse-only example. A scratch config home; nothing live is touched.
#[test]
fn the_binary_refuses_to_start_off_loopback_without_a_ceiling() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = dir.path().join("config.toml");
    std::fs::write(
        &config,
        format!(
            "mount = \"prefer urn:repo:={}\"\nweb.bind = \"0.0.0.0:0\"\n",
            dir.path().join("absent.sock").display()
        ),
    )
    .expect("write config");
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_ikigai-web"))
        .arg("--config")
        .arg(&config)
        .output()
        .expect("run the binary");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "refused: {stderr}");
    assert!(stderr.contains("web.cap"), "{stderr}");
    for scope in BROWSE_ONLY {
        assert!(
            stderr.contains(scope),
            "the example is in the refusal: {stderr}"
        );
    }
    assert!(
        !stderr.contains("serving http://"),
        "it refused before binding: {stderr}"
    );
}

/// And `--help` states the ceiling, so an operator can see what the LAN may read before
/// widening a bind.
#[test]
fn help_names_the_ceiling_and_the_browse_only_example() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_ikigai-web"))
        .arg("--help")
        .output()
        .expect("run the binary");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");
    assert!(stdout.contains("--cap SCOPE"), "{stdout}");
    for scope in BROWSE_ONLY {
        assert!(stdout.contains(scope), "{stdout}");
    }
}
