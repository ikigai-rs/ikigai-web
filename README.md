# ikigai-web

Browse an ikigai kernel from a web browser.

A small standalone HTTP server: `GET http://127.0.0.1:8642/{uri}` percent-decodes
`{uri}` (e.g. `/urn:repo:ikigai-core:tree`) and resolves it through an embedded
kernel composed from the machine's **normal config** — the `mount` lines in
`~/.config/ikigai/config.toml`. This process owns no store and configures no
browse roots; everything it serves lives on the mounted peers (typically the dev
server behind `~/.ikigai/dev.sock`).

```
ikigai-web [--port N] [--config PATH]
```

Configuration comes from the config home and flags — never environment
variables. `web.port` in the config sets the port (default 8642); `--port`
overrides.

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
| anything else | 405 + `Allow` |

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

## Mounts

Each config line is `<mode> <prefix>=<target>`, the CLI's grammar:

```toml
mount = "prefer urn:repo:=~/.ikigai/dev.sock"
mount = "prefer urn:annotation:=~/.ikigai/dev.sock"
```

`alias` renames a remote's `urn:` namespace under a local prefix; `override`
forwards IRIs unchanged and connects eagerly (a dead peer is a startup error);
`prefer` connects lazily on first use and retries after failures — an absent
peer is its normal operation (503 under its prefix while asleep). v1 dials
Unix-socket (IPC) targets only; `quic://` and `peer:` targets are a loud
startup error.

## License

MIT OR Apache-2.0, at your option.
