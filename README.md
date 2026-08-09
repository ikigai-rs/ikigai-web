# ikigai-web

Browse an ikigai kernel from a web browser.

A small standalone HTTP server: `GET http://127.0.0.1:8642/{uri}` percent-decodes
`{uri}` (e.g. `/urn:repo:ikigai-core:tree`) and resolves it through an embedded
kernel composed from the machine's **normal config** — the `mount` lines in
`~/.config/ikigai/config.toml`. This process owns no store and configures no
browse roots; everything it serves lives on the mounted peers (typically the dev
server behind `~/.ikigai/dev.sock`).

```
ikigai-web [--port N] [--config PATH] [--mount LINE ...]
```

Configuration comes from the config home and flags — never environment
variables. `web.port` in the config sets the port (default 8642); `--port`
overrides. `web.mount` config lines (and repeatable `--mount` flags) add
mounts for **this process only** — see [Mounts](#mounts).

## Trust posture (v1)

Binds **127.0.0.1 only**, unconditionally — no flag widens it. The trust model
is *the local owner*, the same posture the dev socket's peer-credential check
takes. Requests resolve under the root capability locally; capability does not
yet cross the IPC wire, so a mounted peer serves under its own authority for a
peercred-authenticated local client — exactly what it grants the CLI.

## The face

| HTTP | kernel |
|------|--------|
| `GET /{uri}` | `Source` — query args pass through as invocation args (`?annotations=include`) |
| `HEAD /{uri}` | `Exists` — 200 (no body) on `true`, 404 on `false` |
| `POST /urn:annotation[:{id}]` | `Sink` — the one write route v1 exposes (annotation minting for the browse overlay) |
| `GET`/`POST /sparql` | `Source` on `urn:sparql:{form}` — the SPARQL face (below) |
| `GET /` | index: browsable repos (via `urn:repo:list`) + the kernel catalog |
| `GET /browse/{uri}` | the htmx shell page hosting the browse family's HTML faces |
| `GET /k/source <iri> [k=v ...]` | the host adapter the faces' `hx-get` affordances target |
| `POST /k/sink urn:annotation…` | the faces' `hx-post` (form fields → args; same single write route) |
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
too; body fields win on collision.

**Errors:** typed kernel errors project to status codes — `NotFound`/
`Unresolved` → 404, `Denied` → 403, bad args → 400, `Unavailable` → 503,
`Timeout` → 504.

**Caching:** `Cache-Control` projects the representation's own `Expiry`
(`Always` → `no-store`, `At` → `max-age`, `Never` → `immutable`). There is
deliberately **no ETag**: the golden-thread validity token is kernel-local by
design (threads never cross a wire mount), so for mounted resources this edge
holds no validity to surface — and a content-hash ETag would fake freshness the
kernel never asserted. When validity crosses the wire, the ETag lands here.

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
mount = "prefer urn:annotation:=~/.ikigai/dev.sock"
```

`web.mount` lines and `--mount` flags use the same grammar and compose after
the shared `mount` lines, for this process only.

`alias` renames a remote's `urn:` namespace under a local prefix; `override`
forwards IRIs unchanged and connects eagerly (a dead peer is a startup error);
`prefer` connects lazily on first use and retries after failures — an absent
peer is its normal operation (503 under its prefix while asleep). v1 dials
Unix-socket (IPC) targets only; `quic://` and `peer:` targets are a loud
startup error.

## License

MIT OR Apache-2.0, at your option.
