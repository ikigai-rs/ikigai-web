//! `ikigai-web` — browse an ikigai kernel from a web browser.
//!
//! A small standalone HTTP server that makes kernel resources browsable:
//! `GET http://127.0.0.1:{port}/{uri}` percent-decodes `{uri}` (e.g.
//! `/urn:repo:ikigai-core:tree`) and resolves it through an embedded kernel
//! composed from the machine's NORMAL config — the `mount` lines in
//! `~/.config/ikigai/config.toml`. This process owns no store and configures
//! no browse roots: everything it serves lives on the mounted peers (on a
//! typical machine, gonk behind `~/.ikigai/gonk.sock`; the dev server it
//! replaced was retired on 2026-10-07).
//!
//! ## Trust posture
//!
//! Two settings decide what this face exposes, and they are independent: the
//! BIND (who can connect) and the capability CEILING (what any request may
//! read or write once connected).
//!
//! **The bind.** 127.0.0.1 by default. On loopback the trust model is *the
//! local owner*, the same posture gonk's IPC socket takes with its
//! peer-credential check: anything that can open a loopback connection on this machine is the
//! machine's owner. `web.bind` (config) / `--bind` (flag) widens it for the
//! LAN (`web.bind = "0.0.0.0:8642"`), and off loopback the server is
//! **read-only by construction**, not by per-route discipline: one gate ahead
//! of all dispatch refuses everything that is not GET/HEAD with a 403, so the
//! write surface (the annotation Sink, in both its spellings) is GONE, and no
//! future route can widen it by forgetting a check. The one exception is
//! `POST /sparql`, whose body is a QUERY ([`sparql`] rejects update forms
//! before the kernel sees them). The posture is derived from the socket
//! ACTUALLY bound, inside [`serve::serve`] itself. The browse shell also stops
//! offering the annotate form off loopback (presentation only; the gate is the
//! boundary).
//!
//! **The ceiling** ([`ceiling`]). Read-only is not the same as safe: this face
//! composes the config home's GENERIC `mount` lines, so on a typical machine it
//! fronts the browse family, the persistent store and the work ledger alike.
//! Until ledger #224/#837 every request went out as
//! [`ikigai_core::Capability::root`], and on 2026-10-07 a LAN-bound 8642 listed
//! the whole work ledger and answered `urn:iki:store:select` with 12,319 ledger
//! quads to anyone on the network. Now the face holds ONE capability, chosen
//! at startup from `web.cap` / `--cap`, and every route issues under it
//! (`serve::Face` is the only door to the kernel): `/{uri}`, `HEAD`, the
//! annotation `POST`, both `/k/` commands, `/sparql` and the index. No header,
//! query parameter or `as=` selects a capability, so a careless route cannot
//! reach past it.
//!
//! - Ceiling **unset on loopback**: root, as before (the local owner).
//! - Ceiling **unset off loopback**: the server REFUSES TO START, in the binary
//!   before it binds and in [`serve::serve`] for any library caller. There is
//!   deliberately no spelling for "root on the LAN".
//! - Ceiling **set**: every request, on any bind, holds exactly those scopes.
//!   [`ceiling::BROWSE_ONLY`] is the documented LAN example: browse plus the
//!   browse graph over `/sparql`, no ledger, no store-wide reads, no inference.
//!
//! The capability CROSSES the IPC wire: the mount resolver carries it in
//! `IssueAs`, and the peer resolves under it, clamped to its authenticated
//! principal. `tests/conformance.rs` proves a peer enforcing a declared scope
//! across an `override` mount; `tests/ceiling.rs` proves a peer that checks a
//! scope at RUNTIME (a per-graph SPARQL union, gonk's rule) across the `prefer`
//! mounts a machine config actually uses, which until this arc DROPPED the
//! capability at the wire (see `mounts.rs`, `LazyIpcResolver::issue_as`).
//!
//! What this is not: authentication. Every caller on the LAN gets the same
//! ceiling; there is no per-person capability and no auth theater in front of
//! it (no token in a URL, no password form). Per-person authority is the
//! passkey → capability-workspace arc (a WebAuthn login mints a scoped
//! workspace, the `ikigai-cms-web` lineage, and gonk's HTTP door already does
//! it). Until then, choose the ceiling for the least trusted person on the
//! network.
//!
//! ## Verb map
//!
//! - `GET {uri}` → `Source`
//! - `HEAD {uri}` → `Exists` (200 with no body when the resource reports
//!   `true`, 404 when `false`)
//! - `POST /urn:iki:annotation[:{id}]` → `Sink` — the ONE write route v1
//!   exposes, because the browse annotation overlay needs it. Form-encoded
//!   bodies map to invocation args; any other body arrives as the piped
//!   `content`. The pre-`urn:iki:` spelling `POST /urn:annotation[:{id}]` is
//!   accepted too, for the whole migration window: this route gate sits
//!   OUTSIDE the kernel and sees the CALLER's name, never the canonical one
//!   (see `serve::ANNOTATION_ROOTS` for why, and for what ends the window).
//! - `GET`/`POST /sparql` → `Source` on `urn:sparql:{select|ask|construct|
//!   describe}` — the content-negotiated SPARQL face (see [`sparql`]). POST
//!   here carries a long QUERY, not a write: execution is always `Source`,
//!   update forms are rejected at the face, so the write surface is unchanged.
//! - everything else → 405 with `Allow`.
//!
//! ## The browse host (`/browse/{uri}`, `/k/…`)
//!
//! The browse family's HTML faces are authored against a HOST adapter: their
//! affordances are `hx-get="/k/source <iri> [k=v ...]"` and
//! `hx-post="/k/sink urn:iki:annotation"`. This server IS that host: `/k/` speaks
//! exactly `source` (GET) and `sink` (POST, annotation family only — the same
//! single write route), and `/browse/{uri}` serves the shell page (vendored
//! htmx, `#browse` target, minimal styling) that makes the faces clickable.
//! `GET /` is a courtesy index: the browsable repos (via `urn:repo:list`) and
//! the kernel catalog.
//!
//! ## Conneg
//!
//! The `Accept` header selects the representation face by setting the `as`
//! argument: `text/html` → `as=text/html`, `text/turtle` and JSON likewise. A
//! browser's default `Accept` starts with `text/html`, so browsers get the
//! HTML face. An explicit `?as=` query argument wins over the header. All
//! other query arguments pass through as invocation args
//! (`?annotations=include`).
//!
//! ## Caching — revalidate, don't promise
//!
//! `Cache-Control` projects the representation's own `Expiry`: `Always` →
//! `no-store`, `At(t)` → `max-age`, `Never` → `public, no-cache`.
//!
//! That last one is **not** `immutable`, and the difference is a category
//! error worth naming. Kernel `Expiry::Never` means "a pure function of its
//! inputs — safe to cache and reuse"; it says nothing about the URL. HTTP
//! `immutable` means "the bytes AT THIS URL will never change; do not
//! revalidate". `urn:repo:style` is a stable URL whose content genuinely
//! changes — with `a11y.toml`, with the theme, with the crate version — so
//! `immutable` was a promise it could not keep: a stylesheet change that was
//! demonstrably on the wire stayed invisible in the browser at every normal
//! reload. Correct in the kernel does not survive translation into a different
//! caching model unchanged.
//!
//! Every conneg'd read (`GET /{uri}`, `/k/source`, `/sparql`) therefore carries
//! a **strong `ETag`** — `Representation::content_id()`, BLAKE3 over the
//! representation's type and bytes, rendered `"b3:<hex>"` — and honours
//! `If-None-Match` with a bodyless `304`. The validator is deliberately
//! content-derived rather than golden-thread-derived: thread sets are
//! `#[serde(skip)]` and kernel-local, so they do not cross a wire mount, and
//! this process serves nearly everything from a mounted peer. A thread-derived
//! validator would be right in-process and silently degrade over IPC; bytes are
//! bytes on both sides. When validity DOES cross the wire it can strengthen
//! this (a mounted peer could then answer "unchanged" without re-sending bytes
//! to us), but it is no longer a precondition for an ETag at the edge.
//!
//! `Vary: Accept` rides along, because these faces really do serve different
//! bytes per `Accept` and `content_id` hashes the repr type, so the tags
//! already differ per face. `HEAD` is exempt: it maps to `Exists`, an existence
//! probe with no representation to validate.

pub mod ceiling;
pub mod config;
pub mod mounts;
pub mod serve;
pub mod sparql;
