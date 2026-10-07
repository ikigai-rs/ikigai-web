//! `ikigai-conformance` over the kernel this server actually serves.
//!
//! ## This crate binds ZERO endpoints, and that is the point
//!
//! Every other adopter runs the suite over its own module. This one has no module:
//! [`ikigai_web_server::mounts::compose`] front-composes the machine's `mount` lines and
//! closes the chain with an EMPTY `EndpointSpace`, so the served kernel is *exactly* its
//! mounts. There is no local binding to walk, and [`the_served_kernel_binds_nothing_of_its_own`]
//! pins that: a future local endpoint fails this file until someone classifies it.
//!
//! So the conformance question here is not "does my module conform" but **"does composition
//! preserve conformance"** — and it is answered by running ONE suite over TWO kernels built
//! from the SAME endpoints:
//!
//! - `peer_kernel()` in-process, and
//! - `mounts::compose(["override urn:demo:=<socket>"])` over a real
//!   [`ikigai_ipc::serve`] peer serving that same kernel.
//!
//! The delta between the two reports is what the mount costs. It is not zero, but since
//! core 0.1.73 the reports cannot show it: [`the_mount_costs_exactly_the_golden_threads`]
//! pins the one thing it is by hand.
//!
//! ## The fixture module
//!
//! Three endpoints, authored to the recipe so that every finding through the mount is the
//! MOUNT's and not the fixture's (the suite walks fixture endpoints as module endpoints —
//! conformance PENDING #17): typed inputs with XSD classes, declared outputs, declared cap
//! scopes on the verbs that enforce them, `content` on the Sink, a skolemized Turtle face
//! over well-known terms, and cacheability that says what it means (`demo-upper` is pure,
//! `demo-roster` carries a thread).
//!
//! `demo-ledger` doubles as the **side-effect witness**: it appends to a shared vector, so a
//! refusal can be proved to have written nothing without a trace event — the shape
//! conformance PENDING #125 asks for, and the shape this server's write route needs
//! (`tests/route_gate.rs`).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use ikigai_conformance::{Report, Suite};
use ikigai_core::{
    ActionSpec, ArgRef, ArgSpec, Capability, Description, EndpointSpace, Error, Exact, Expiry,
    Fallback, FnEndpoint, Iri, Kernel, ReprType, Representation, Request, Space, Verb,
};
use ikigai_web_server::mounts::{compose, parse_mount_line};

/// The XSD datatype every input here carries: the type the wire actually holds.
const XSD_STRING: &str = "http://www.w3.org/2001/XMLSchema#string";
/// The cap scopes `demo-ledger` declares — and, being declared, enforces.
const CAP_WRITE: &str = "urn:cap:demo:write";
const CAP_READ: &str = "urn:cap:demo:read";

/// What a `demo-ledger` Sink appended, in order — the witness a refusal leaves nothing in.
type Ledger = Arc<Mutex<Vec<String>>>;

// ---------------------------------------------------------------------------
// The fixture module: three endpoints, authored to the recipe.
// ---------------------------------------------------------------------------

/// `urn:demo:upper` — a pure function of its inputs, marked `.cacheable()` and meaning it.
fn upper() -> FnEndpoint {
    FnEndpoint::new("demo-upper", |inv| {
        let input = inv.inline_str("in")?;
        Ok(Representation::new(ReprType::new("text/plain"), input.to_uppercase()).cacheable())
    })
    .with_description(
        Description::new("demo-upper")
            .title("Upper-case a string")
            .action(
                ActionSpec::new(Verb::Source)
                    .summary("Upper-case `in`")
                    .input(ArgSpec::new("in").class(XSD_STRING).summary("the string"))
                    .output("text/plain"),
            ),
    )
}

/// The golden thread `demo-roster` hangs its cacheable representation on. A real module
/// would cut it from a watcher; here nothing cuts it, which is exactly the property the
/// mount comparison is about — the thread's PRESENCE is what does or does not cross.
const ROSTER_THREAD: &str = "urn:demo:roster:thread";

/// `urn:demo:roster` — two faces (`text/plain`, `text/turtle`), both declared, selected by
/// an `as` input whose `one_of` IS the module's list of faces (conformance PENDING #79).
/// Cacheable and threaded: the honest spelling for derived state something can cut.
fn roster() -> FnEndpoint {
    FnEndpoint::new("demo-roster", |inv| {
        let face = inv.inline_str("as").unwrap_or("text/plain");
        let repr = match face {
            "text/turtle" => Representation::new(
                ReprType::new("text/turtle"),
                // Skolemized: a stable IRI per row, never a blank node or a counter.
                "<urn:demo:roster:ada> <http://purl.org/dc/terms/title> \"Ada\" .\n",
            ),
            "text/plain" => Representation::new(ReprType::new("text/plain"), "Ada\n"),
            other => {
                return Err(Error::InvalidArgument {
                    name: "as".into(),
                    detail: format!("`{other}` is not a face this endpoint serves"),
                })
            }
        };
        Ok(repr.cacheable().depends_on(ROSTER_THREAD))
    })
    .with_description(
        Description::new("demo-roster")
            .title("The roster, in two faces")
            .action(
                ActionSpec::new(Verb::Source)
                    .summary("Serve the roster")
                    .input(
                        ArgSpec::new("as")
                            .class(XSD_STRING)
                            .summary("the representation face")
                            .one_of(["text/plain", "text/turtle"])
                            .default_value("text/plain")
                            .optional(),
                    )
                    .output("text/plain")
                    .output("text/turtle"),
            ),
    )
}

/// `urn:demo:ledger` — a Sink that reads its payload from `content` (pipeline citizenship)
/// and a Source that reads it back, each declaring the scope it enforces. Every Sink
/// firing lands in `ledger`, so "nothing was written" is provable without a trace event.
fn ledger_endpoint(ledger: Ledger) -> FnEndpoint {
    FnEndpoint::new("demo-ledger", move |inv| match inv.request.verb {
        Verb::Sink => {
            let entry = inv.inline_str("content")?.to_string();
            ledger.lock().expect("ledger").push(entry);
            Ok(Representation::new(
                ReprType::new("text/plain"),
                "written\n",
            ))
        }
        Verb::Source => Ok(Representation::new(
            ReprType::new("text/plain"),
            ledger.lock().expect("ledger").join("\n"),
        )),
        other => Err(Error::Endpoint(format!("demo-ledger: no {other:?}"))),
    })
    .with_description(
        Description::new("demo-ledger")
            .title("An append-only ledger")
            .action(
                ActionSpec::new(Verb::Sink)
                    .summary("Append an entry")
                    .input(
                        ArgSpec::new("content")
                            .class(XSD_STRING)
                            .summary("the entry to append"),
                    )
                    .output("text/plain")
                    .requires(CAP_WRITE),
            )
            .action(
                ActionSpec::new(Verb::Source)
                    .summary("Read the ledger back")
                    .output("text/plain")
                    .requires(CAP_READ),
            ),
    )
}

/// The fixture module as a kernel — the thing a peer serves and this process mounts.
///
/// Built with the same `TurtleRenderer` [`compose`] injects, and for a reason worth stating:
/// a peer with NO Meta renderer answers the `Verb::Meta` round-trip
/// `ikigai_resolve::ForwardingEndpoint::describe` makes with an error, and that describe is
/// **best-effort** — it falls back to `Description::new("remote")` and says nothing. Every
/// mounted endpoint then presents one anonymous, action-less contract: the walk collapses to
/// a single id called `remote`, and a manifold that has lost every ArgSpec looks merely
/// small rather than broken. A peer without a renderer is a peer without a contract.
fn peer_kernel(ledger: Ledger) -> Kernel {
    let space = EndpointSpace::new()
        .bind(Exact::new("urn:demo:upper"), upper())
        .bind(Exact::new("urn:demo:roster"), roster())
        .bind(Exact::new("urn:demo:ledger"), ledger_endpoint(ledger));
    Kernel::with_meta_renderer(
        Arc::new(Fallback::new(vec![Arc::new(space) as Arc<dyn Space>])),
        Arc::new(ikigai_vocab::TurtleRenderer),
    )
}

/// The ids the fixture module binds — the whole catalog on both sides of the wire.
const FIXTURE_IDS: &[&str] = &["demo-upper", "demo-roster", "demo-ledger"];

/// The suite, declared once and run over both kernels: the same claims, so a difference in
/// the reports is a difference in the KERNEL, never in what was asked of it.
fn suite() -> Suite {
    Suite::new()
        .pure("demo-upper")
        .cacheable("demo-upper")
        .cacheable("demo-roster")
}

// ---------------------------------------------------------------------------
// The peer: a real Unix-socket kernel server, the thing `compose` dials.
// ---------------------------------------------------------------------------

/// A composed kernel over a live IPC peer, with the peer's ledger to witness writes.
struct Mounted {
    _dir: tempfile::TempDir,
    kernel: Kernel,
    /// The ledger the PEER's endpoint writes into (this process holds no endpoint at all).
    ledger: Ledger,
}

/// Stand up an IPC peer serving [`peer_kernel`], then build the served kernel the way
/// production does — `mounts::compose` over a config-shaped `mount` line.
fn mounted(mode: &str) -> Mounted {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket = dir.path().join("peer.sock");
    let ledger: Ledger = Arc::default();
    let served = peer_kernel(Arc::clone(&ledger));
    let path = socket.clone();
    std::thread::spawn(move || {
        // Serves until the process ends; the tempdir outlives every test that uses it.
        let _ = ikigai_ipc::serve(served, &path);
    });
    wait_for_socket(&socket);
    let line = format!("{mode} urn:demo:={}", socket.display());
    let kernel = compose(vec![parse_mount_line(&line).expect("a valid mount line")])
        .unwrap_or_else(|e| panic!("compose `{line}`: {e}"));
    Mounted {
        _dir: dir,
        kernel,
        ledger,
    }
}

/// The peer binds on its own thread; `compose` of an eager mount dials immediately, so wait
/// for the socket to exist rather than racing it.
fn wait_for_socket(socket: &Path) {
    for _ in 0..600 {
        if socket.exists() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    panic!("the IPC peer never bound {}", socket.display());
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn iri(s: &str) -> Iri {
    Iri::parse(s.to_string()).unwrap_or_else(|e| panic!("`{s}` is a valid IRI: {e}"))
}

fn request(verb: Verb, target: &str, args: &[(&str, &str)]) -> Request {
    let mut request = Request::new(verb, iri(target));
    for (name, value) in args {
        request = request.with_arg(*name, ArgRef::Inline(value.as_bytes().to_vec()));
    }
    request
}

fn issue(
    kernel: &Kernel,
    request: Request,
    capability: &Capability,
) -> Result<Representation, Error> {
    futures::executor::block_on(kernel.issue(request, capability))
}

fn text(repr: &Representation) -> String {
    String::from_utf8(repr.bytes.clone()).expect("UTF-8")
}

fn no_grants() -> Capability {
    Capability::scoped(Vec::<String>::new())
}

/// Every description id the kernel's catalog names (kernel ops excluded, as the walk does).
fn walked_ids(kernel: &Kernel) -> BTreeSet<String> {
    kernel
        .entries()
        .expect("an enumerable root")
        .iter()
        .filter(|e| !e.pattern.starts_with("urn:kernel:"))
        .map(|e| {
            kernel
                .describe_pattern(&e.pattern)
                .unwrap_or_else(|| panic!("`{}` describes itself", e.pattern))
                .id
        })
        .collect()
}

/// A report's findings as `id CHECK` pairs — the shape a delta is legible in.
fn finding_keys(report: &Report) -> BTreeSet<String> {
    report
        .findings
        .iter()
        .map(|f| format!("{} {:?}", f.endpoint, f.check))
        .collect()
}

// ---------------------------------------------------------------------------
// The tests
// ---------------------------------------------------------------------------

/// **This crate contributes no endpoints.** `compose` with no mount lines yields a kernel
/// whose catalog is empty, and with one mount line yields exactly the peer's catalog: the
/// local `EndpointSpace` at the end of the chain is empty by construction and nothing in
/// this crate binds into it.
///
/// The pin matters because it is the premise of every other line in this file. If a future
/// version serves something of its own — a health resource, an index face, a badges face —
/// it appears here first, and whoever adds it has to decide, in this file, whether it is
/// cacheable over a working tree (it is not: an index over a live tree is LIVE unless a
/// thread the HOST actually cuts exists, and core PENDING §18 says thread names are
/// host-relative, so this process must not mint a thread no host keeps).
#[test]
fn the_served_kernel_binds_nothing_of_its_own() {
    let empty = compose(Vec::new()).expect("an empty composition is a kernel");
    assert_eq!(
        walked_ids(&empty),
        BTreeSet::new(),
        "this crate binds no endpoints; the served kernel is exactly its mounts"
    );
    let mounted = mounted("override");
    let expected: BTreeSet<String> = FIXTURE_IDS.iter().map(|s| s.to_string()).collect();
    assert_eq!(
        walked_ids(&mounted.kernel),
        expected,
        "with one mount, the catalog is the peer's catalog and nothing else"
    );
}

/// ★ **The composed kernel can render its own catalog** — the regression this arc's one
/// source change buys.
///
/// `urn:kernel:catalog` renders every bound endpoint's `describe()` into one Turtle graph
/// through the kernel's Meta renderer, and core injects none: a `Kernel::new` host has no
/// catalog, only a flat `endpoint error: no Meta renderer configured`. That is what this
/// server shipped, and `GET /urn:kernel:catalog` answered **500** on the running daemon —
/// on a link `serve::index` emits itself, since the index lists every `kernel.entries()` row
/// and kernel operations are rows.
///
/// It survived because nothing here ever asked: the HTTP face maps GET to `Source` and HEAD
/// to `Exists` and never issues `Verb::Meta`; `tests/http.rs` builds its kernels itself
/// rather than through [`compose`], so the one function that had the defect was not under
/// test; and `Kernel::describe()` reads `Endpoint::describe()` directly and needs no
/// renderer, so every introspection path a test WOULD have reached for worked fine. The
/// catalog was the only caller, and it is the one a machine reads.
///
/// The catalog also proves the mount composes into it: the peer's endpoints are described in
/// this process's catalog, each through a `Verb::Meta` round-trip over the wire.
#[test]
fn the_composed_kernel_renders_its_own_catalog() {
    let mounted = mounted("override");
    let catalog = issue(
        &mounted.kernel,
        request(Verb::Source, "urn:kernel:catalog", &[]),
        &Capability::root(),
    )
    .expect("a composed kernel renders its catalog");
    let turtle = text(&catalog);
    for id in FIXTURE_IDS {
        assert!(
            turtle.contains(id),
            "the mounted peer's `{id}` is described in this process's catalog:\n{turtle}"
        );
    }
    assert!(
        turtle.contains("ik:Endpoint"),
        "and it is the catalog graph:\n{turtle}"
    );
    // An empty composition — no mounts at all — still has a catalog rather than an error.
    let empty = compose(Vec::new()).expect("kernel");
    issue(
        &empty,
        request(Verb::Source, "urn:kernel:catalog", &[]),
        &Capability::root(),
    )
    .expect("a mount-free kernel renders an (almost empty) catalog");
}

/// The reference run: the fixture module, in-process, is clean. Every finding the mounted
/// run reports is therefore composition's, not the fixture's — the control this file needs
/// before it can attribute anything (the suite walks fixture endpoints as module endpoints,
/// conformance PENDING #17, so an unconformant fixture would pollute the comparison).
#[test]
fn the_fixture_module_conforms_in_process() {
    let ledger: Ledger = Arc::default();
    let kernel = peer_kernel(Arc::clone(&ledger));
    let report = suite().run_blocking(&kernel);
    eprintln!("--- in-process ---\n{report}");
    assert!(
        report.is_clean(),
        "the reference module must be clean:\n{report}"
    );
    assert_eq!(report.endpoints, FIXTURE_IDS.len());
    assert_eq!(
        report.checks.skipped().count(),
        0,
        "every check runs:\n{report}"
    );
    // The walk fired the Sink once, through the pipeline probe: the witness works.
    assert_eq!(
        ledger.lock().expect("ledger").len(),
        1,
        "PIPELINE fired demo-ledger's Sink exactly once"
    );
}

/// ★ **The mount costs exactly the golden threads — and since core 0.1.73 the suite can no
/// longer see it.** The same endpoints, the same suite, through `compose` over a live IPC
/// peer. `demo-roster`'s in-process representation is cacheable and hangs from
/// [`ROSTER_THREAD`]; over the wire that thread is gone, because `Representation::threads`
/// is `#[serde(skip)]` and does not cross. That is the standing hole this crate's ETag design
/// was built around (see the crate docs: the validator is content-derived precisely because
/// "thread sets are `#[serde(skip)]` and kernel-local, so they do not cross a wire mount").
///
/// Up to core 0.1.72 the suite named it — "cacheable with an empty golden-thread set" — and
/// this test pinned that red line. Core 0.1.73 (hole A, ledger #512) made the kernel hang
/// every cacheable Source/Exists answer from its own canonical target, so the mounted
/// representation now arrives with ONE thread, `urn:demo:roster`, the local name. The set is
/// no longer empty, so the suite's CACHEABLE check passes, and the report over the mount is
/// as clean as the one in-process.
///
/// **The hole is narrower, not closed.** The local target thread is cut by a Sink or Delete
/// made THROUGH this kernel on that name. It is not cut by anything the peer does: a cut of
/// `ROSTER_THREAD` on the peer, or a write that reaches the peer by another route, leaves
/// this kernel serving the old roster until something local cuts it. So the pins below are
/// the mechanism, by hand, because the walk can no longer carry the statement.
#[test]
fn the_mount_costs_exactly_the_golden_threads() {
    let ledger: Ledger = Arc::default();
    let in_process = suite().run_blocking(&peer_kernel(Arc::clone(&ledger)));
    let mounted = mounted("override");
    let over_the_wire = suite().run_blocking(&mounted.kernel);
    eprintln!("--- over an IPC mount ---\n{over_the_wire}");

    assert!(in_process.is_clean(), "control:\n{in_process}");
    assert!(
        over_the_wire.is_clean(),
        "the kernel's target thread masks the erasure from the walk:\n{over_the_wire}"
    );
    // The same walk, same counts: composition hides no endpoint and skips no check.
    assert_eq!(over_the_wire.endpoints, in_process.endpoints);
    assert_eq!(over_the_wire.actions, in_process.actions);
    assert_eq!(over_the_wire.checks.skipped().count(), 0);

    // The mechanism, by hand: the representation is still marked cacheable, the peer's
    // declared thread is gone, and what remains is the thread the LOCAL kernel added.
    let threads_of = |repr: &Representation| -> BTreeSet<String> {
        repr.threads().iter().map(|t| t.to_string()).collect()
    };
    let direct = issue(
        &peer_kernel(Arc::default()),
        request(Verb::Source, "urn:demo:roster", &[]),
        &Capability::root(),
    )
    .expect("in-process");
    assert_eq!(direct.expiry, Expiry::Never);
    assert_eq!(
        threads_of(&direct),
        BTreeSet::from([ROSTER_THREAD.to_string(), "urn:demo:roster".to_string()]),
        "in-process: the declared thread, and the target thread the kernel adds"
    );
    let through = issue(
        &mounted.kernel,
        request(Verb::Source, "urn:demo:roster", &[]),
        &Capability::root(),
    )
    .expect("over the mount");
    assert_eq!(through.bytes, direct.bytes, "the same bytes cross");
    assert_eq!(through.expiry, Expiry::Never, "the expiry crosses");
    assert_eq!(
        threads_of(&through),
        BTreeSet::from(["urn:demo:roster".to_string()]),
        "the declared thread does not cross; only the local target thread remains"
    );
}

/// Composition preserves the CONTRACT: an endpoint's typed self-description survives the
/// wire whole. `ForwardingEndpoint::describe` round-trips a `Verb::Meta` request in the JSON
/// face, so the ArgSpecs, the `one_of` faces, the declared outputs and the declared cap
/// scopes a caller sees through the mount are the peer's own — which is what makes the
/// mounted catalog a usable manifold rather than a list of names.
///
/// This is the half of the story the previous test does not tell: descriptions cross,
/// representations cross, threads do not.
#[test]
fn the_contract_crosses_the_wire_whole() {
    let ledger: Ledger = Arc::default();
    let local = peer_kernel(ledger);
    let mounted = mounted("override");
    for target in ["urn:demo:upper", "urn:demo:roster", "urn:demo:ledger"] {
        let here = local.describe(&iri(target)).expect("described locally");
        let there = mounted
            .kernel
            .describe(&iri(target))
            .expect("described over the mount");
        assert_eq!(there.id, here.id, "{target}");
        assert_eq!(
            there.action_specs().len(),
            here.action_specs().len(),
            "{target}: every action crosses"
        );
        for (there, here) in there.action_specs().iter().zip(here.action_specs().iter()) {
            assert_eq!(there.verb, here.verb, "{target}");
            assert_eq!(
                there.requires, here.requires,
                "{target}: declared caps cross"
            );
            assert_eq!(
                there.outputs, here.outputs,
                "{target}: declared faces cross"
            );
            let names = |a: &ActionSpec| {
                a.inputs
                    .iter()
                    .map(|i| (i.name.clone(), i.class.clone(), i.one_of.clone()))
                    .collect::<Vec<_>>()
            };
            assert_eq!(names(there), names(here), "{target}: typed inputs cross");
        }
    }
}

/// Declared = enforced, across the mount, with the witness the trace cannot give.
///
/// `demo-ledger`'s Sink declares `urn:cap:demo:write`. Under a capability holding no grants
/// the kernel refuses it BEFORE the endpoint runs — the floor, and the suite's ENFORCED check
/// proves that much on both kernels. What ENFORCED cannot prove is that nothing HAPPENED
/// (conformance PENDING #46 and #125: a refusal inside `invoke` leaves no trace event at
/// all), so the ledger is read before and after: a refused write appends nothing, in-process
/// and over the wire alike.
///
/// The capability really does cross: `IpcResolver::issue_as` carries it and the server
/// resolves under it. That is worth pinning here rather than assuming, because the HTTP
/// face's capability CEILING (`tests/ceiling.rs`) is only as good as this crossing. This
/// test uses an `override` mount, which holds the `IpcResolver` directly; the `prefer`
/// mounts a machine config uses go through `mounts::LazyIpcResolver`, which dropped the
/// capability until 2026-10-07, and `tests/ceiling.rs` pins that path with a scope the
/// peer checks at runtime.
#[test]
fn a_declared_scope_is_enforced_across_the_mount_and_the_refusal_writes_nothing() {
    let mounted = mounted("override");
    let before = mounted.ledger.lock().expect("ledger").len();
    match issue(
        &mounted.kernel,
        request(Verb::Sink, "urn:demo:ledger", &[("content", "nope")]),
        &no_grants(),
    ) {
        Err(Error::Denied(detail)) => assert!(
            detail.contains(CAP_WRITE),
            "the refusal names the declared scope: {detail}"
        ),
        other => panic!("under no grants: Denied, got {other:?}"),
    }
    assert_eq!(
        mounted.ledger.lock().expect("ledger").len(),
        before,
        "a refused write appended nothing"
    );
    // And the same call under a capability that DOES hold the scope lands, on the peer's
    // side of the wire — the ledger this process cannot reach except through the mount.
    let granted = Capability::scoped([CAP_WRITE, CAP_READ]);
    issue(
        &mounted.kernel,
        request(Verb::Sink, "urn:demo:ledger", &[("content", "landed")]),
        &granted,
    )
    .expect("the granted write lands");
    assert_eq!(
        mounted
            .ledger
            .lock()
            .expect("ledger")
            .last()
            .map(String::as_str),
        Some("landed"),
        "the write crossed the wire and landed on the peer"
    );
    let read = issue(
        &mounted.kernel,
        request(Verb::Source, "urn:demo:ledger", &[]),
        &granted,
    )
    .expect("the granted read lands");
    assert!(text(&read).contains("landed"), "{}", text(&read));
}

/// **Declared outputs are the media types served, per declared face** — by hand, because
/// 0.1.0's OUTPUTS check compares only the minimal call and only RDF faces (conformance
/// PENDING #11/#31/#79). `demo-roster`'s `as` input carries `one_of`, which IS the module's
/// own list of faces, so every value is resolved and its bare media type required to be
/// declared — through the MOUNT, since that is the kernel this crate serves. A face selected
/// only by `as=` is exactly the case a minimal-inputs probe never sees.
///
/// And the other direction: every declared output is served by some value, so a declaration
/// cannot name a face the endpoint does not have.
#[test]
fn declared_outputs_are_the_media_types_served_through_the_mount() {
    let mounted = mounted("override");
    let kernel = &mounted.kernel;
    let description = kernel.describe(&iri("urn:demo:roster")).expect("described");
    let spec = description
        .action_specs()
        .into_iter()
        .find(|a| a.verb == Verb::Source)
        .expect("Source is declared");
    let declared: BTreeSet<String> = spec
        .outputs
        .iter()
        .map(|o| ikigai_conformance::rdf::bare_media_type(o))
        .collect();
    assert!(!declared.is_empty(), "the action declares an output");
    let faces = &spec
        .inputs
        .iter()
        .find(|i| i.name == "as")
        .expect("`as` is a declared input")
        .one_of;
    assert!(!faces.is_empty(), "`as` enumerates the faces it serves");
    let mut served = BTreeSet::new();
    for face in faces {
        let repr = issue(
            kernel,
            request(Verb::Source, "urn:demo:roster", &[("as", face)]),
            &Capability::root(),
        )
        .unwrap_or_else(|e| panic!("as={face}: {e}"));
        let got = ikigai_conformance::rdf::bare_media_type(&repr.repr_type.media_type);
        assert!(
            declared.contains(&got),
            "as={face} served `{got}`, declared {declared:?}"
        );
        served.insert(got);
    }
    assert_eq!(served, declared, "every declared face is actually served");
    // A value outside `one_of` is a typed refusal, not a silently-defaulted face
    // (conformance PENDING #118/#124: a class or an enum that is declared but not held).
    match issue(
        kernel,
        request(
            Verb::Source,
            "urn:demo:roster",
            &[("as", "application/pdf")],
        ),
        &Capability::root(),
    ) {
        Err(Error::InvalidArgument { name, .. }) => assert_eq!(name, "as"),
        other => panic!("an undeclared face is InvalidArgument(\"as\"), got {other:?}"),
    }
}

/// **A required input is actually required** — the REQUIRED-IS-REQUIRED check conformance
/// PENDING #49/#99 proposes and 0.1.0 does not have: drop each required by-value input from
/// the call and expect `MissingArgument`. Run through the mount, so it also says that a
/// missing argument is refused with its TYPE intact after a wire round-trip rather than
/// flattening to a generic endpoint error.
#[test]
fn dropping_a_required_input_is_a_typed_refusal_through_the_mount() {
    let mounted = mounted("override");
    match issue(
        &mounted.kernel,
        request(Verb::Source, "urn:demo:upper", &[]),
        &Capability::root(),
    ) {
        Err(Error::MissingArgument(name)) => assert_eq!(name, "in"),
        other => panic!("`in` is required: expected MissingArgument, got {other:?}"),
    }
    // `demo-ledger`'s Sink likewise — and the witness says the firing never happened.
    let before = mounted.ledger.lock().expect("ledger").len();
    match issue(
        &mounted.kernel,
        request(Verb::Sink, "urn:demo:ledger", &[]),
        &Capability::scoped([CAP_WRITE]),
    ) {
        Err(Error::MissingArgument(name)) => assert_eq!(name, "content"),
        other => panic!("`content` is required: expected MissingArgument, got {other:?}"),
    }
    assert_eq!(
        mounted.ledger.lock().expect("ledger").len(),
        before,
        "a refused Sink appended nothing"
    );
    // The optional input really is optional: the roster resolves with no arguments at all.
    issue(
        &mounted.kernel,
        request(Verb::Source, "urn:demo:roster", &[]),
        &Capability::root(),
    )
    .expect("`as` is optional and defaults");
}

/// ★ **An alias mount re-prefixes the peer's KERNEL OPERATIONS, and that carries them past
/// every `urn:kernel:` exclusion in the ecosystem.**
///
/// An alias is a local NAME for the remote's `urn:` namespace, so the whole remote catalog
/// arrives re-prefixed — `urn:kernel:catalog` included, as `urn:edge:kernel:catalog`. Two
/// consequences, both of them found here rather than reasoned about:
///
/// 1. The conformance suite skips core's own operations unless `include_kernel_ops()` asks,
///    and it recognizes them by their name. Under an alias it does not recognize them, so a
///    walk of a module reached through an alias mount reports CORE's findings as the
///    module's: eleven of them here, on three operations, and every one is real (core's
///    `kernel-actions`, `kernel-cut` and `kernel-validate` declare inputs with no `class`,
///    and `kernel-validate` does not resolve with the minimal inputs its ArgSpecs allow).
///    Core 0.1.74 hung `kernel-catalog` and `kernel-actions` from the bindings thread, which
///    retired their two CACHEABLE findings. A module author
///    who mounted a peer this way would be handed another crate's homework with no marker
///    saying so. Reported for the conformance PENDING; the ids are pinned below so the day
///    core types those ArgSpecs, this test says so.
/// 2. Every `urn:kernel:` FILTER anywhere is name-based, so this is not one crate's problem.
///    `serve::index` filters nothing and simply lists what it is given; a *hypothetical*
///    write route matching on `urn:kernel:` would not match `urn:edge:kernel:cut` either.
///    This server is safe for a different reason — its one write route is an allow-list of
///    two annotation roots, not a deny-list — which is the argument for allow-lists.
///
/// What the alias does NOT cost: the module's own findings are identical to the override
/// mount's (none — see [`the_mount_costs_exactly_the_golden_threads`] for why the thread
/// erasure no longer shows in a walk), and the local names resolve.
#[test]
fn an_alias_mount_re_prefixes_the_peers_kernel_operations_too() {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket: PathBuf = dir.path().join("alias.sock");
    let ledger: Ledger = Arc::default();
    let served = peer_kernel(Arc::clone(&ledger));
    let path = socket.clone();
    std::thread::spawn(move || {
        let _ = ikigai_ipc::serve(served, &path);
    });
    wait_for_socket(&socket);
    let kernel = compose(vec![parse_mount_line(&format!(
        "alias urn:edge:={}",
        socket.display()
    ))
    .expect("a valid mount line")])
    .expect("compose");

    let patterns: BTreeSet<String> = kernel
        .entries()
        .expect("enumerable")
        .into_iter()
        .map(|e| e.pattern)
        // ★ An alias mount re-prefixes the PEER's kernel operations too, so they arrive as
        // `urn:edge:kernel:catalog` and a `urn:kernel:` prefix test does not see them. The
        // conformance suite excludes kernel ops the same way, which means a module walked
        // through an alias mount has core's operations walked as if they were the module's.
        .filter(|p| !p.contains("kernel:"))
        .collect();
    assert_eq!(
        patterns,
        BTreeSet::from([
            "urn:edge:demo:upper".to_string(),
            "urn:edge:demo:roster".to_string(),
            "urn:edge:demo:ledger".to_string(),
        ]),
        "an alias surfaces the peer's catalog under the LOCAL prefix"
    );
    assert_eq!(
        text(
            &issue(
                &kernel,
                request(Verb::Source, "urn:edge:demo:upper", &[("in", "ada")]),
                &Capability::root()
            )
            .expect("resolves under the local name")
        ),
        "ADA"
    );
    let report = suite().run_blocking(&kernel);
    eprintln!("--- over an alias mount ---\n{report}");
    let (mine, core_s): (BTreeSet<String>, BTreeSet<String>) = finding_keys(&report)
        .into_iter()
        .partition(|key| key.starts_with("demo-"));
    assert_eq!(
        mine,
        BTreeSet::new(),
        "the module's own findings are the override mount's, unchanged:\n{report}"
    );
    assert_eq!(
        core_s,
        BTreeSet::from([
            "kernel-actions ArgSpecs".to_string(),
            "kernel-cut ArgSpecs".to_string(),
            "kernel-validate ArgSpecs".to_string(),
            "kernel-validate SkolemRdf".to_string(),
        ]),
        "core's own operations, walked because the alias renamed them out of every \
         `urn:kernel:` exclusion — none of these are this crate's or the module's:\n{report}"
    );
    // And they really are core's, reached under the local name: the peer's catalog answers
    // at `urn:edge:kernel:catalog`, which is the mechanism, not a coincidence of ids.
    let catalog = issue(
        &kernel,
        request(Verb::Source, "urn:edge:kernel:catalog", &[]),
        &Capability::root(),
    )
    .expect("the peer's catalog, under the local prefix");
    assert!(
        text(&catalog).contains("demo-upper"),
        "it is the peer's catalog: {}",
        text(&catalog)
    );
}
