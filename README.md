# ikigai-web

Browse an ikigai kernel from a web browser.

A small standalone HTTP server: `GET http://127.0.0.1:8642/{uri}` percent-decodes
`{uri}` (e.g. `/urn:repo:ikigai-core:tree`) and resolves it through an embedded
kernel composed from the machine's **normal config** — the `mount` lines in
`~/.config/ikigai/config.toml`. This process owns no store and configures no
browse roots; everything it serves lives on the mounted peers (typically the dev
server behind `~/.ikigai/dev.sock`).

```
ikigai-web [--bind IP:PORT | --port N] [--config PATH] [--mount LINE ...]
```

Configuration comes from the config home and flags — never environment
variables. `web.bind` in the config sets the full bind address (`--bind`
overrides); `web.port` is shorthand for `web.bind = "127.0.0.1:{port}"`
(`--port` likewise) — they are one setting spelled two ways, so setting both
is a loud error. Default: `127.0.0.1:8642`. Flags override config wholesale.
`web.mount` config lines (and repeatable `--mount` flags) add mounts for
**this process only** — see [Mounts](#mounts).

## Trust posture

Binds **127.0.0.1 by default**. On loopback the trust model is *the local
owner*, the same posture the dev socket's peer-credential check takes.

Every request this face issues — every verb, every route, the `/k/` adapter
included — is issued under the **root capability**, and there is no flag,
header or config key that makes it anything else. So a declared cap scope is
never the gate here: what protects the one write route is the route allowlist
(two annotation roots, colon-anchored) plus the bind posture below.
`tests/route_gate.rs` pins both halves — the same Sink a capability lacking its
declared scope is refused at the kernel, accepted through the route — so the
day this face starts minting attenuated capabilities (the passkey arc), the
change announces itself.

Note that the capability itself *does* cross the IPC wire now: the client
carries it in `IssueAs` and the peer resolves under it, clamping to its
authenticated principal — `tests/conformance.rs` proves a peer enforcing a
declared scope across a real mount, and a refused write landing nothing. What
is missing is not the transport, it is anything at this edge that would narrow
root before handing it over.

A non-loopback bind (`web.bind = "0.0.0.0:8642"`, or `--bind 0.0.0.0:8642`)
serves **read-only**: one gate ahead of all dispatch refuses everything that
is not GET/HEAD with a 403, so the write surface — the annotation Sink, in
both its `POST /urn:iki:annotation…` and `/k/sink` spellings, and under the
legacy `urn:annotation…` name too — is *gone*, not gated per-route. The
exception is `POST /sparql`, whose body is a query (that face is read-only by
its own construction and rejects update forms itself).
The posture derives from the socket actually bound, inside `serve` itself; the
startup line states it. The browse shell also stops offering the annotate form
off loopback (presentation — the gate is the boundary).

This is deliberately **trust-the-LAN, for demos**: anyone on the network can
read what the mounted peers serve, and there is intentionally no auth theater
in front of that. Real authentication is the passkey → capability-workspace
arc (a WebAuthn login minting a capability-scoped workspace, the
`ikigai-cms-web` lineage); until that lands here, don't bind a kernel with
sensitive mounts beyond loopback.

## The face

| HTTP | kernel |
|------|--------|
| `GET /{uri}` | `Source` — query args pass through as invocation args (`?annotations=include`) |
| `HEAD /{uri}` | `Exists` — 200 (no body) on `true`, 404 on `false` |
| `POST /urn:iki:annotation[:{id}]` | `Sink` — the one write route v1 exposes (annotation minting for the browse overlay). The legacy `POST /urn:annotation[:{id}]` spelling is accepted too — see [The annotation route and the `urn:iki:` window](#the-annotation-route-and-the-urniki-window) |
| `GET`/`POST /sparql` | `Source` on `urn:sparql:{form}` — the SPARQL face (below) |
| `GET /` | index: browsable repos (via `urn:repo:list`) + the kernel catalog |
| `GET /browse/{uri}` | the htmx shell page hosting the browse family's HTML faces |
| `GET /k/source <iri> [k=v ...]` | the host adapter the faces' `hx-get` affordances target |
| `POST /k/sink urn:iki:annotation…` | the faces' `hx-post` (form fields → args, any other body → the piped `content`; same single write route; legacy spelling accepted) |
| anything else | 405 + `Allow` |

Open `http://127.0.0.1:8642/` in a browser and click into a repo: tree →
directories → file faces with syntax highlighting, all htmx swaps through
`/k/source`. htmx is vendored (`assets/htmx.min.js`, same-origin, no CDN — the
`ikigai-cms-web` posture).

**Conneg:** the `Accept` header selects the face via the `as=` argument —
`text/html` → HTML (a browser's default Accept gets HTML), `text/turtle`,
`application/json`, `application/ld+json`, `text/plain`. An explicit `?as=`
wins over the header.

**POST bodies:** `application/x-www-form-urlencoded` fields map to invocation
args (the htmx overlay's shape); any other body arrives as the piped `content`
arg with its `Content-Type` surfaced as `content-type`. Query args pass through
too; body fields win on collision. The `/k/sink` spelling follows the same rule
(it silently dropped non-form bodies until 0.4.0), with one difference that is
a refusal rather than a silence: the adapter's arguments are text, so a
non-UTF-8 payload is a 400 there and rides the direct route instead.

**Errors:** typed kernel errors project to status codes — `NotFound`/
`Unresolved` → 404, `Denied` → 403, bad args → 400, `Unavailable` → 503,
`Timeout` → 504.

**Caching:** `Cache-Control` projects the representation's own `Expiry`
(`Always` → `no-store`, `At` → `max-age`, `Never` → `public, no-cache`).
`Never` is deliberately **not** `immutable`: the kernel means "pure function of
its inputs", HTTP means "the bytes at this URL will never change", and
`urn:repo:style` is a stable URL whose content really does change (`a11y.toml`,
theme, crate version) — `immutable` made correct server-side changes invisible
short of a hard reload.

Conneg'd reads carry a strong **`ETag`** (`Representation::content_id()` —
BLAKE3 over the representation's type and bytes, `"b3:<hex>"`) and honour
`If-None-Match` with a bodyless `304`, plus `Vary: Accept`. The validator is
content-derived, not golden-thread-derived: thread sets are kernel-local and
never cross a wire mount, so a thread-derived tag would be right in-process and
silently degrade over IPC.

## /sparql — the SPARQL face

`GET /sparql?query=…` (or `POST /sparql` with the query as the body — raw
`application/sparql-query` or a form's `query=` field — for long queries),
content-negotiated:

- **`Accept: text/html`** — the editor page: query prefilled and
  syntax-highlighted, results as a table below (SELECT; `urn:*` IRIs link back
  into this server), a boolean (ASK) or Turtle (CONSTRUCT/DESCRIBE), and the
  eight sample queries from the review layer as sidebar links that fill the
  editor on click. Entirely same-origin: the editor is a small inline highlight
  overlay, not a vendored bundle — no external requests, ever.
- **anything else** — the raw result: `application/sparql-results+json` by
  default; `text/csv`, `text/tab-separated-values`, `+xml`, `text/turtle` via
  `Accept` or an explicit `?as=` (which wins). A protocol-ish endpoint other
  tools can point at.

Execution routes by **query form** — the first meaningful token after the
prologue picks `urn:sparql:select` / `:ask` / `:construct` / `:describe` — and
is always `Verb::Source`. The face is **read-only**: update forms
(INSERT/DELETE/…) are rejected loudly before the kernel sees them, so
`POST /sparql` does not widen the write surface.

The `urn:sparql:*` space typically lives on the dev server (its shared live
store: explanations, annotations, review passes). Mount it for this process
only:

```toml
web.mount = "prefer urn:sparql:=~/.ikigai/dev.sock"
```

`web.mount` (not a bare `mount` line) because the key is web-scoped: the CLI
hosts read `mount` and never `web.mount`, so a machine-wide `mount` line would
shadow their **local** sparql spaces — this one cannot. `--mount` is the ad-hoc
flag spelling of the same line.

## Mounts

Each config line is `<mode> <prefix>=<target>`, the CLI's grammar:

```toml
mount = "prefer urn:repo:=~/.ikigai/dev.sock"
# No trailing colon: the annotation prefix must cover the bare mint IRI
# (`urn:annotation`) as well as the slug family (`urn:annotation:{id}`).
mount = "prefer urn:iki:annotation=~/.ikigai/dev.sock"
mount = "prefer urn:annotation=~/.ikigai/dev.sock"
```

`web.mount` lines and `--mount` flags use the same grammar and compose after
the shared `mount` lines, for this process only.

`alias` renames a remote's `urn:` namespace under a local prefix; `override`
forwards IRIs unchanged and connects eagerly (a dead peer is a startup error);
`prefer` connects lazily on first use and retries after failures — an absent
peer is its normal operation (503 under its prefix while asleep). v1 dials
Unix-socket (IPC) targets only; `quic://` and `peer:` targets are a loud
startup error.

## The annotation route and the `urn:iki:` window

`ikigai-browse` 0.3.0 moved the annotation family from `urn:annotation:*` to
`urn:iki:annotation:*`. This server accepts **both** spellings on its one write
route, for the whole transition window.

That is deliberate, and it moves in the **opposite direction** from a mount
line. `post_allowed()` is an HTTP route allowlist that sits **outside the
kernel**: it inspects the raw request path *before* any resolution, so it sees
**the name the caller wrote**. A mount line, by contrast, sits **inside** any
alias table (`Kernel::with_aliases` wraps the root space), so it sees the
**canonical** name — a stale `urn:annotation` mount stops matching the instant
an alias fires, and the alias cannot save it. Route gates widen; mount lines
get rewritten. Getting this backwards breaks one while "fixing" the other.

⚠ **This process installs no alias table, and should not.** It composes
*nothing* locally — the kernel is exactly its mounts — so there are no local
bindings for a table to protect; it could only relocate names away from the
very mount prefixes that route them. The rename lives at the peer that holds
the bindings (the dev server), which installs its own table. What that means
operationally:

- an **old-spelling** request forwards verbatim over IPC and is aliased at the
  peer, so it keeps working with no config change here;
- a **new-spelling** request needs its own `mount` line, as above. Without one
  nothing routes `urn:iki:annotation…` to a peer and it 404s at the empty local
  space — the route gate accepting the name is necessary but not sufficient.

The old entry comes out of the gate when no reachable caller emits the old
spelling any more — in practice, once every host's alias table has dropped its
`urn:annotation:` rule. Until then it is not redundancy to tidy away.

## Conformance — what a walk of the served kernel says

This crate binds **no endpoints**. `mounts::compose` front-composes the config's
`mount` lines and closes the chain with an empty `EndpointSpace`, so the served
kernel is exactly its mounts — and the interesting question is not "does my
module conform" but **does composition preserve conformance**.

`tests/conformance.rs` answers it by running one
[`ikigai-conformance`](https://crates.io/crates/ikigai-conformance) suite over
two kernels built from the *same* endpoints: in-process, and through
`compose` over a real `ikigai-ipc` peer. The delta is what a mount costs.

- **Contracts cross whole.** ArgSpecs, `one_of` faces, declared outputs and
  declared cap scopes all arrive intact — a mounted catalog is a usable
  manifold, not a list of names.
- **Enforcement crosses.** A declared scope is refused at the peer under a
  capability that lacks it, and the refused write lands nothing.
- **Golden threads do not cross.** A representation that is cacheable *and*
  threaded in-process arrives cacheable with an **empty thread set**: thread
  sets are `#[serde(skip)]` and kernel-local. The suite says so in the words the
  recipe uses — "served forever with nothing to cut it" — and this is exactly
  why the ETag here is content-derived rather than thread-derived. Endpoints
  declared *pure* look unaffected, because a pure result was supposed to have no
  thread; the erasure is only visible where there was something to lose.
- **A peer with no Meta renderer has no contract.** `describe` over a mount is a
  `Verb::Meta` round-trip, and it is best-effort: a peer that cannot render one
  answers, and every mounted endpoint collapses to one anonymous, action-less
  description called `remote`. Nothing reports it.
- **An alias mount re-prefixes the peer's kernel operations too**
  (`urn:edge:kernel:catalog`), which carries them past every `urn:kernel:`
  exclusion — the conformance suite's included, so core's operations get walked
  as if they were the module's. An argument for allowlists over denylists; this
  server's write route is an allowlist.

## License

MIT OR Apache-2.0, at your option.
