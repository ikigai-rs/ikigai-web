//! `ikigai-web` — browse an ikigai kernel from a web browser.
//!
//! A small standalone HTTP server that makes kernel resources browsable:
//! `GET http://127.0.0.1:{port}/{uri}` percent-decodes `{uri}` (e.g.
//! `/urn:repo:ikigai-core:tree`) and resolves it through an embedded kernel
//! composed from the machine's NORMAL config — the `mount` lines in
//! `~/.config/ikigai/config.toml`. This process owns no store and configures
//! no browse roots: everything it serves lives on the mounted peers (on a
//! typical machine, the dev server behind `~/.ikigai/dev.sock`).
//!
//! ## Trust posture
//!
//! The server binds **127.0.0.1 by default**. On loopback the trust model is
//! *the local owner*, the same posture the dev socket's peer-credential check
//! takes: anything that can open a loopback connection on this machine is the
//! machine's owner. Requests resolve under the root capability locally;
//! capability does not yet cross the IPC wire, so the mounted peer serves
//! under its own authority for a peercred-authenticated local client — exactly
//! what it grants the CLI.
//!
//! `web.bind` (config) / `--bind` (flag) can widen the bind for a LAN demo
//! (`web.bind = "0.0.0.0:8642"`) — and off loopback the server is **read-only
//! by construction**, not by per-route discipline: one gate ahead of all
//! dispatch refuses everything that is not GET/HEAD with a 403, so the write
//! surface (the annotation Sink, in both its spellings) is GONE, and no
//! future route can widen it by forgetting a check. The one exception is
//! `POST /sparql`, whose body is a QUERY — that face is read-only by its own
//! construction ([`sparql`] rejects update forms before the kernel sees
//! them). The posture is derived from the socket ACTUALLY bound, inside
//! [`serve::serve`] itself — there is no parameter by which a non-loopback
//! listener could start with a live write surface. The browse shell also
//! stops offering the annotate form off loopback (presentation only; the
//! gate is the boundary).
//!
//! This is deliberately **trust-the-LAN, for demos**: anyone on the network
//! can read whatever the mounted peers serve, and there is intentionally no
//! auth theater in front of that (no token in a URL, no password form — a
//! decoration that suggests a boundary it doesn't enforce is worse than the
//! honest posture line at startup). Real authentication here is the passkey →
//! capability-workspace arc (a WebAuthn login mints a capability-scoped
//! workspace, the `ikigai-cms-web` lineage); until that lands, do not bind a
//! kernel with sensitive mounts beyond loopback.
//!
//! ## Verb map
//!
//! - `GET {uri}` → `Source`
//! - `HEAD {uri}` → `Exists` (200 with no body when the resource reports
//!   `true`, 404 when `false`)
//! - `POST /urn:annotation[:{id}]` → `Sink` — the ONE write route v1 exposes,
//!   because the browse annotation overlay needs it. Form-encoded bodies map
//!   to invocation args; any other body arrives as the piped `content`.
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
//! `hx-post="/k/sink urn:annotation"`. This server IS that host: `/k/` speaks
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
//! ## Caching — honest, and therefore thin (v1)
//!
//! `Cache-Control` projects the representation's own `Expiry`: `Always` →
//! `no-store`, `At(t)` → `max-age`, `Never` → `immutable`. There is **no
//! ETag**: the golden-thread validity token (a representation's thread set +
//! the kernel's cut generations) is kernel-local by design — threads are
//! `#[serde(skip)]` and never cross a wire mount — so for resources served
//! over the machine's mounts this edge holds no validity to surface, and a
//! content-hash ETag would fake freshness the kernel never asserted. When
//! validity crosses the wire, the ETag lands here.

pub mod config;
pub mod mounts;
pub mod serve;
pub mod sparql;
