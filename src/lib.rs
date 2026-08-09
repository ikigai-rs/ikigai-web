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
//! ## Trust posture (v1)
//!
//! The server binds **127.0.0.1 only**, unconditionally — there is no flag to
//! widen it. The trust model is *the local owner*, the same posture the dev
//! socket's peer-credential check takes: anything that can open a loopback
//! connection on this machine is the machine's owner. Requests resolve under
//! the root capability locally; capability does not yet cross the IPC wire, so
//! the mounted peer serves under its own authority for a peercred-authenticated
//! local client — exactly what it grants the CLI. Widening the bind or adding
//! an authenticating capability door (the multi-tenant seam) is deliberately a
//! later slice, not a config knob one typo away.
//!
//! ## Verb map
//!
//! - `GET {uri}` → `Source`
//! - `HEAD {uri}` → `Exists` (200 with no body when the resource reports
//!   `true`, 404 when `false`)
//! - `POST /urn:annotation[:{id}]` → `Sink` — the ONE write route v1 exposes,
//!   because the browse annotation overlay needs it. Form-encoded bodies map
//!   to invocation args; any other body arrives as the piped `content`.
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
