//! `/sparql` — a content-negotiated SPARQL face over the kernel's
//! `urn:sparql:*` endpoints.
//!
//! `GET /sparql?query=…` (and `POST /sparql` with the query as the body, for
//! long queries) executes the query through the kernel and projects the result
//! by `Accept`:
//!
//! - `text/html` → the editor page: query prefilled and syntax-highlighted,
//!   results rendered as a table below, the sample queries as sidebar links
//!   that fill the editor on click. Entirely same-origin: the highlighter is a
//!   small inline overlay (a `<pre>` behind a transparent-text `<textarea>`),
//!   not a vendored editor — YASGUI/YASQE's dist is ~1MB of minified JS, an
//!   order of magnitude more un-auditable code than this whole server, so v1
//!   takes the textarea. No external requests, ever.
//! - anything else → the raw result: `application/sparql-results+json` by
//!   default, `text/csv` / `text/tab-separated-values` / `+xml` on request
//!   (via `Accept` or an explicit `?as=`, which wins), `text/turtle` for
//!   CONSTRUCT/DESCRIBE — a protocol-ish endpoint other tools can point at.
//!
//! Execution routes by QUERY FORM: the first meaningful token after the
//! prologue picks `urn:sparql:select` / `:ask` / `:construct` / `:describe`.
//! The face is READ-ONLY — update forms (INSERT/DELETE/…) are rejected loudly
//! at the face, before the kernel ever sees them, and execution is always
//! `Verb::Source`, so `POST /sparql` widens the verb surface not at all (the
//! POST body is a query, not a write; SPARQL's own protocol does the same).

use ikigai_core::{ArgRef, Capability, Iri, Kernel, Request, Verb};

use crate::serve::{
    cache_control, content_type, error_resp, html_escape, status_of, HttpRequest, Resp,
};

/// Dispatch `/sparql`. GET executes `?query=`; POST takes the query from a
/// form body (`query=` field) or a raw body (`application/sparql-query`, or
/// any non-form body — the bytes ARE the query).
pub(crate) async fn respond(kernel: &Kernel, req: &HttpRequest) -> Resp {
    if req.method != "GET" && req.method != "POST" {
        let mut resp = error_resp(405, "the /sparql face speaks GET and POST");
        resp.headers
            .push(("Allow".to_string(), "GET, POST".to_string()));
        return resp;
    }
    let mut args: Vec<(String, String)> = req.query.clone();
    if req.method == "POST" {
        let content_type_hdr = req.header("content-type").unwrap_or("");
        if content_type_hdr.starts_with("application/x-www-form-urlencoded") {
            let Ok(body) = std::str::from_utf8(&req.body) else {
                return error_resp(400, "form body is not UTF-8");
            };
            for (k, v) in crate::serve::parse_query(body) {
                args.retain(|(name, _)| name != &k);
                args.push((k, v));
            }
        } else if !req.body.is_empty() {
            let Ok(body) = std::str::from_utf8(&req.body) else {
                return error_resp(400, "query body is not UTF-8");
            };
            args.retain(|(name, _)| name != "query");
            args.push(("query".to_string(), body.to_string()));
        }
    }
    // Form submission normalizes newlines to CRLF; the CR is a transport
    // artifact, not query text — strip it so re-submissions stay stable.
    let query = args
        .iter()
        .find(|(k, _)| k == "query")
        .map(|(_, v)| v.replace("\r\n", "\n"));
    let query = query.as_deref().filter(|q| !q.trim().is_empty());
    let explicit_as = args.iter().find(|(k, _)| k == "as").map(|(_, v)| v.clone());
    let accepted = req.header("accept").and_then(accept_result_type);
    // The editor page is the html face — but an explicit `as=` always wins
    // (the page's own JSON/CSV links carry one).
    if explicit_as.is_none() && accepted == Some("text/html") {
        return editor_page(kernel, query).await;
    }
    let Some(query) = query else {
        return error_resp(400, "missing `query=` (or a POST body carrying the query)");
    };
    let form = match query_form(query) {
        Ok(form) => form,
        Err(reason) => return error_resp(400, &reason),
    };
    let as_arg = explicit_as.or_else(|| accepted.map(str::to_string));
    match execute(kernel, form, query, as_arg.as_deref()).await {
        Ok(repr) => Resp {
            status: 200,
            headers: vec![
                ("Content-Type".to_string(), content_type(&repr.repr_type)),
                ("Cache-Control".to_string(), cache_control(repr.expiry)),
            ],
            body: repr.bytes,
            suppress_body: false,
        },
        Err(e) => error_resp(status_of(&e), &e.to_string()),
    }
}

/// Issue the query through the kernel as `Verb::Source` on `urn:sparql:{form}`.
async fn execute(
    kernel: &Kernel,
    form: &str,
    query: &str,
    as_arg: Option<&str>,
) -> Result<ikigai_core::Representation, ikigai_core::Error> {
    let target = Iri::parse(format!("urn:sparql:{form}"))
        .map_err(|e| ikigai_core::Error::Endpoint(format!("urn:sparql:{form}: {e}")))?;
    let mut request = Request::new(Verb::Source, target)
        .with_arg("query", ArgRef::Inline(query.as_bytes().to_vec()));
    if let Some(media) = as_arg {
        request = request.with_arg("as", ArgRef::Inline(media.as_bytes().to_vec()));
    }
    kernel.issue(request, &Capability::root()).await
}

/// The result media type an `Accept` header asks for — the /sparql-specific
/// palette (the generic route's `accept_face` doesn't know the SPARQL result
/// types). First recognized item wins; `*/*` means "the endpoint's default".
fn accept_result_type(accept: &str) -> Option<&'static str> {
    for item in accept.split(',') {
        let media = item.split(';').next().unwrap_or("").trim();
        match media {
            "text/html" | "application/xhtml+xml" => return Some("text/html"),
            "application/sparql-results+json" | "application/json" => {
                return Some("application/sparql-results+json")
            }
            "application/sparql-results+xml" => return Some("application/sparql-results+xml"),
            "text/csv" => return Some("text/csv"),
            "text/tab-separated-values" => return Some("text/tab-separated-values"),
            "text/turtle" => return Some("text/turtle"),
            "application/n-triples" => return Some("application/n-triples"),
            _ => continue,
        }
    }
    None
}

/// The query's form — which `urn:sparql:*` endpoint runs it. Read forms only:
/// update forms are refused HERE, loudly, so the read-only posture is the
/// face's own guarantee, not a property of whatever happens to be mounted.
pub fn query_form(query: &str) -> Result<&'static str, String> {
    let tokens = tokenize(query);
    let mut tokens = tokens.iter();
    while let Some(token) = tokens.next() {
        match token.to_ascii_uppercase().as_str() {
            // Prologue: `PREFIX pname: <iri>` / `BASE <iri>` — skip and keep looking.
            "PREFIX" => {
                tokens.next();
                tokens.next();
            }
            "BASE" => {
                tokens.next();
            }
            "SELECT" => return Ok("select"),
            "ASK" => return Ok("ask"),
            "CONSTRUCT" => return Ok("construct"),
            "DESCRIBE" => return Ok("describe"),
            up @ ("INSERT" | "DELETE" | "LOAD" | "CLEAR" | "CREATE" | "DROP" | "COPY" | "MOVE"
            | "ADD" | "WITH") => {
                return Err(format!(
                    "`{up}` is a SPARQL Update form — /sparql is read-only \
                     (SELECT, ASK, CONSTRUCT, DESCRIBE)"
                ))
            }
            _ => {
                return Err(format!(
                    "`{token}`: expected a query form (SELECT, ASK, CONSTRUCT, DESCRIBE) \
                     after the prologue"
                ))
            }
        }
    }
    Err("empty query".to_string())
}

/// Split a query into prologue-grade tokens: whitespace-separated words, with
/// `#` comments skipped, `<…>` IRIs kept whole (an IRI may contain `#` — the
/// vocabulary namespace does), and `"…"` strings kept whole (one may contain
/// anything). Only the leading tokens are ever examined, but tokenizing the
/// whole string keeps the function honest about comments anywhere.
fn tokenize(query: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut chars = query.chars().peekable();
    while let Some(&c) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
        } else if c == '#' {
            for c in chars.by_ref() {
                if c == '\n' {
                    break;
                }
            }
        } else if c == '<' {
            let mut token = String::new();
            for c in chars.by_ref() {
                token.push(c);
                if c == '>' {
                    break;
                }
            }
            tokens.push(token);
        } else if c == '"' {
            let mut token = String::new();
            token.push(chars.next().expect("peeked"));
            let mut escaped = false;
            for c in chars.by_ref() {
                token.push(c);
                if escaped {
                    escaped = false;
                } else if c == '\\' {
                    escaped = true;
                } else if c == '"' {
                    break;
                }
            }
            tokens.push(token);
        } else {
            let mut token = String::new();
            while let Some(&c) = chars.peek() {
                if c.is_whitespace() || c == '#' || c == '<' || c == '"' {
                    break;
                }
                token.push(c);
                chars.next();
            }
            tokens.push(token);
        }
    }
    tokens
}

// ---------------------------------------------------------------------------
// The sample queries — the Elsevier class demo, embedded byte-exact from
// ikigai-devtools claude/class/review-queries.md (titles and one-line
// descriptions included). Each query carries the doc's shared prefix block so
// a sidebar click yields a runnable query.
// ---------------------------------------------------------------------------

/// The prefix block `review-queries.md` states once for all its examples.
const PREFIXES: &str = "PREFIX oa:  <http://www.w3.org/ns/oa#>\n\
                        PREFIX dct: <http://purl.org/dc/terms/>\n\
                        PREFIX ik:  <https://ikigai-rs.dev/ns#>\n\
                        PREFIX prov: <http://www.w3.org/ns/prov#>\n";

/// One sample query: a title, a one-line description, and the query body
/// (prefixes not included — [`Sample::query`] prepends the shared block).
pub struct Sample {
    pub title: &'static str,
    pub blurb: &'static str,
    body: &'static str,
}

impl Sample {
    /// The full runnable query — the shared prefix block, a blank line, the body.
    pub fn query(&self) -> String {
        format!("{PREFIXES}\n{}", self.body)
    }
}

/// The eight worked examples, in the doc's order.
pub fn samples() -> &'static [Sample] {
    const SAMPLES: &[Sample] = &[
        Sample {
            title: "The triage queue",
            blurb: "Everything the model flagged that no human has looked at, sorted by exposure.",
            body: r#"SELECT ?file (COUNT(?a) AS ?findings) WHERE {
  ?a dct:creator ?model ; ik:annotates ?file .
  FILTER NOT EXISTS {
    ?h ik:annotates ?file .
    FILTER NOT EXISTS { ?h dct:creator ?c }
  }
} GROUP BY ?file ORDER BY DESC(?findings)"#,
        },
        Sample {
            title: "System awareness in one query",
            blurb: "What a file IS (the explanation) joined with what the reviewer FLAGGED.",
            body: r#"SELECT ?summary ?note WHERE {
  ?e ik:about <urn:repo:ikigai-emacs:file:ikigai.el> ;
     ik:explanation ?summary .
  ?a ik:annotates <urn:repo:ikigai-emacs:file:ikigai.el> ;
     dct:creator ?m ; oa:bodyValue ?note .
}"#,
        },
        Sample {
            title: "Machine vs human",
            blurb: "Who is doing the reviewing, and how much — unbound ?creator = human notes.",
            body: r#"SELECT ?creator (COUNT(?a) AS ?notes) WHERE {
  ?a a oa:Annotation .
  OPTIONAL { ?a dct:creator ?creator }
} GROUP BY ?creator"#,
        },
        Sample {
            title: "Stale reviews",
            blurb: "Findings whose code moved: re-anchored survivors vs orphaned notes.",
            body: r#"SELECT ?file ?note ?state WHERE {
  ?a dct:creator ?m ; ik:annotates ?file ; oa:bodyValue ?note .
  { ?a ik:orphaned true . BIND("orphaned" AS ?state) }
  UNION
  { ?a ik:reanchored true . BIND("reanchored" AS ?state) }
}"#,
        },
        Sample {
            title: "Review coverage",
            blurb: "Explained files vs review passes per repo — the \"are we even looking\" dashboard.",
            body: r#"SELECT ?repo (COUNT(DISTINCT ?e) AS ?explained)
             (COUNT(DISTINCT ?pass) AS ?review_passes) WHERE {
  { ?e ik:about ?f ; ik:repo ?repo . }
  UNION
  { ?pass prov:used ?f2 ; ik:repo ?repo . }
} GROUP BY ?repo"#,
        },
        Sample {
            title: "Pass archaeology",
            blurb: "What review-v1 said about THIS exact version of the file — addressable forever.",
            body: "SELECT ?tag ?note WHERE {\n  ?a prov:wasGeneratedBy ?pass ; oa:bodyValue ?note .\n  ?pass ik:versionTag ?tag ;\n        ik:contentHash \"sha256:cc28e5\u{2026}\" .   # the content version\n}",
        },
        Sample {
            title: "The PR conversation",
            blurb: "Humans and machines on one pull request, each finding anchored to its diff line.",
            body: r#"SELECT ?who ?note ?quote WHERE {
  ?a ik:annotates <urn:repo:ikigai-browse:pr:11> ;
     oa:bodyValue ?note .
  OPTIONAL { ?a dct:creator ?who }
  OPTIONAL { ?a oa:hasSelector ?s . ?s oa:exact ?quote }
}"#,
        },
        Sample {
            title: "Reviewed PRs vs unreviewed",
            blurb: "Which PRs the system has machine-reviewed, and which merely exist.",
            body: r#"SELECT ?pr (COUNT(?a) AS ?findings) WHERE {
  ?a ik:annotates ?pr ; dct:creator ?m .
  FILTER(CONTAINS(STR(?pr), ":pr:"))
} GROUP BY ?pr"#,
        },
    ];
    SAMPLES
}

/// Percent-encode a query-string VALUE: unreserved characters pass, everything
/// else (including `&`, `=`, `+`, newlines) encodes per byte — so a sidebar
/// href round-trips the sample byte-exactly through [`crate::serve`]'s decoder.
pub fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// The editor page (the text/html face).
// ---------------------------------------------------------------------------

/// Render the editor page: sidebar of samples, the query prefilled (if any),
/// and — when a query is present — its results, executed server-side and
/// rendered as a table (SELECT), a boolean (ASK), or Turtle (CONSTRUCT/
/// DESCRIBE). The page itself is always 200: an editor showing a query error
/// is a successfully rendered editor.
async fn editor_page(kernel: &Kernel, query: Option<&str>) -> Resp {
    let results = match query {
        None => String::new(),
        Some(q) => match query_form(q) {
            Err(reason) => error_box(&reason),
            Ok(form) => {
                // Faces the server can render: TSV → table, CSV boolean → badge,
                // Turtle → code block.
                let as_arg = match form {
                    "select" => Some("text/tab-separated-values"),
                    "ask" => Some("text/csv"),
                    _ => None,
                };
                match execute(kernel, form, q, as_arg).await {
                    Err(e) => error_box(&format!("{} ({})", e, status_of(&e))),
                    Ok(repr) => {
                        let text = String::from_utf8_lossy(&repr.bytes);
                        match form {
                            "select" => tsv_table(&text),
                            "ask" => format!("<p class=\"ask\">{}</p>", html_escape(text.trim())),
                            _ => format!("<pre class=\"turtle\">{}</pre>", html_escape(&text)),
                        }
                    }
                }
            }
        },
    };
    let mut sidebar = String::new();
    for sample in samples() {
        sidebar.push_str(&format!(
            "<li><a href=\"/sparql?query={}\"><strong>{}</strong><small>{}</small></a></li>\n",
            urlencode(&sample.query()),
            html_escape(sample.title),
            html_escape(sample.blurb),
        ));
    }
    // A real prefill, not a placeholder: ghost text looks like a default query
    // but submits nothing, so "Run" on a fresh page silently returned no rows.
    let query_text = query.unwrap_or("SELECT ?s ?p ?o WHERE { ?s ?p ?o } LIMIT 10");
    // The HTML parser eats one newline right after <textarea>; re-add it so a
    // query that genuinely starts with a blank line survives the round trip.
    let textarea = if query_text.starts_with('\n') {
        format!("\n{}", html_escape(query_text))
    } else {
        html_escape(query_text)
    };
    let faces = if query_text.trim().is_empty() {
        String::new()
    } else {
        let encoded = urlencode(query_text);
        format!(
            "<span class=\"faces\">raw: \
             <a href=\"/sparql?query={encoded}&amp;as=application/sparql-results%2Bjson\">JSON</a> \
             <a href=\"/sparql?query={encoded}&amp;as=text/csv\">CSV</a></span>"
        )
    };
    let body = format!(
        "<!doctype html><html><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <title>SPARQL \u{2014} ikigai</title><style>{SPARQL_CSS}</style></head><body>\
         <header><h1>SPARQL</h1>\
         <p>Query the machine's shared RDF store \u{2014} read-only \
         (SELECT, ASK, CONSTRUCT, DESCRIBE). <a href=\"/\">catalog</a></p></header>\
         <div class=\"layout\">\
         <main><form method=\"GET\" action=\"/sparql\">\
         <div class=\"editor\"><pre id=\"hl\" aria-hidden=\"true\"></pre>\
         <textarea id=\"q\" name=\"query\" spellcheck=\"false\">{textarea}</textarea></div>\
         <div class=\"run\"><button type=\"submit\">Run</button>\
         <span class=\"hint\">\u{2318}\u{23ce}</span>{faces}</div>\
         </form>{results}</main>\
         <aside><h2>Sample queries</h2>\
         <p class=\"aside-note\">The review layer, queried \u{2014} click to load.</p>\
         <ul class=\"samples\">\n{sidebar}</ul></aside>\
         </div>\
         <script>{EDITOR_JS}</script></body></html>\n"
    );
    Resp {
        status: 200,
        headers: vec![
            (
                "Content-Type".to_string(),
                "text/html; charset=utf-8".to_string(),
            ),
            ("Cache-Control".to_string(), "no-store".to_string()),
        ],
        body: body.into_bytes(),
        suppress_body: false,
    }
}

fn error_box(reason: &str) -> String {
    format!("<div class=\"error\">{}</div>", html_escape(reason))
}

/// Render SPARQL TSV results as an HTML table. The TSV format (SPARQL 1.1
/// Query Results TSV) writes one variable row (`?x\t?y`) then one row per
/// solution, terms in Turtle-ish encoding: `<iri>`, `"literal"@lang`,
/// numbers plain, unbound empty.
fn tsv_table(tsv: &str) -> String {
    let mut lines = tsv.lines();
    let Some(header) = lines.next() else {
        return error_box("empty result");
    };
    let mut out = String::from("<table class=\"results\"><thead><tr>");
    for var in header.split('\t') {
        out.push_str(&format!(
            "<th>{}</th>",
            html_escape(var.trim_start_matches('?'))
        ));
    }
    out.push_str("</tr></thead><tbody>");
    let mut rows = 0usize;
    for line in lines {
        rows += 1;
        out.push_str("<tr>");
        for term in line.split('\t') {
            out.push_str(&format!("<td>{}</td>", term_html(term)));
        }
        out.push_str("</tr>");
    }
    out.push_str(&format!(
        "</tbody></table><p class=\"count\">{rows} row{}</p>",
        if rows == 1 { "" } else { "s" }
    ));
    out
}

/// One TSV term as display HTML: IRIs become links when the kernel can resolve
/// them (`urn:*` — this server's whole address space), literals lose their
/// transport escapes, everything else (numbers, booleans, unbound) shows as-is.
fn term_html(term: &str) -> String {
    if term.is_empty() {
        return String::new();
    }
    if let Some(iri) = term.strip_prefix('<').and_then(|t| t.strip_suffix('>')) {
        return if iri.starts_with("urn:") {
            format!(
                "<a href=\"/{}\"><code>{}</code></a>",
                html_escape(iri),
                html_escape(iri)
            )
        } else {
            format!("<code>{}</code>", html_escape(iri))
        };
    }
    if term.starts_with('"') {
        let (content, suffix) = split_literal(term);
        let mut cell = html_escape(&unescape_tsv(content));
        if !suffix.is_empty() {
            cell.push_str(&format!(" <small>{}</small>", html_escape(suffix)));
        }
        return cell;
    }
    html_escape(term)
}

/// Split `"content"@lang` / `"content"^^<dt>` into (content, suffix). The
/// closing quote is found respecting backslash escapes.
fn split_literal(term: &str) -> (&str, &str) {
    let bytes = term.as_bytes();
    let mut i = 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'"' => return (&term[1..i], &term[i + 1..]),
            _ => i += 1,
        }
    }
    (&term[1..], "")
}

/// Undo the SPARQL-TSV/N-Triples string escapes (`\t \n \r \" \\ \uXXXX
/// \UXXXXXXXX`). Malformed escapes pass through raw — display code never errors.
fn unescape_tsv(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('t') => out.push('\t'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('"') => out.push('"'),
            Some('\\') => out.push('\\'),
            Some(u @ ('u' | 'U')) => {
                let len = if u == 'u' { 4 } else { 8 };
                let hex: String = chars.by_ref().take(len).collect();
                match u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                    Some(decoded) => out.push(decoded),
                    None => {
                        out.push('\\');
                        out.push(u);
                        out.push_str(&hex);
                    }
                }
            }
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// The page style — same posture as the browse shell: responsive, legible in
/// light and dark, no external anything.
const SPARQL_CSS: &str = "\
 :root{color-scheme:light dark}\
 body{margin:0;font:15px/1.5 -apple-system,system-ui,sans-serif;\
   max-width:75rem;padding:1rem;margin-inline:auto}\
 header p{opacity:.75;margin-top:-.5rem}\
 a{color:light-dark(#0b57d0,#8ab4f8)}\
 .layout{display:grid;grid-template-columns:minmax(0,1fr) 18rem;gap:1.5rem}\
 @media(max-width:52rem){.layout{grid-template-columns:minmax(0,1fr)}}\
 .editor{position:relative;border:1px solid light-dark(#ccc,#444);\
   border-radius:6px;overflow:hidden;background:light-dark(#fff,#1c1c1e)}\
 #hl,#q{margin:0;box-sizing:border-box;width:100%;min-height:15rem;\
   font:13px/1.5 ui-monospace,SFMono-Regular,Menlo,monospace;\
   padding:.7rem .9rem;white-space:pre-wrap;word-break:break-word}\
 #hl{position:absolute;inset:0;overflow:hidden;pointer-events:none}\
 #q{position:relative;display:block;background:transparent;color:transparent;\
   caret-color:light-dark(#000,#eee);border:none;outline:none;resize:vertical}\
 .k{color:light-dark(#8f288f,#d38ad3);font-weight:600}\
 .v{color:light-dark(#0b57d0,#8ab4f8)}\
 .i{color:light-dark(#106a49,#6fcf97)}\
 .s{color:light-dark(#a03d00,#f1a06c)}\
 .c{color:light-dark(#777,#888);font-style:italic}\
 .run{display:flex;align-items:center;gap:.75rem;margin:.6rem 0 1rem}\
 .run button{font:inherit;padding:.35rem 1.1rem;border-radius:6px;\
   border:1px solid light-dark(#ccc,#555);cursor:pointer;\
   background:light-dark(#0b57d0,#8ab4f8);color:light-dark(#fff,#111)}\
 .hint{opacity:.5;font-size:.85em}\
 .faces{margin-left:auto;font-size:.9em;opacity:.85}\
 .faces a{margin-left:.4rem}\
 aside h2{font-size:1em;margin-bottom:.25rem}\
 .aside-note{opacity:.6;font-size:.85em;margin-top:0}\
 .samples{list-style:none;padding:0;margin:0}\
 .samples li{margin:.35rem 0}\
 .samples a{display:block;text-decoration:none;padding:.45rem .6rem;\
   border:1px solid light-dark(#e2e2e2,#3a3a3c);border-radius:6px}\
 .samples a:hover{border-color:light-dark(#0b57d0,#8ab4f8)}\
 .samples strong{display:block;font-size:.92em}\
 .samples small{opacity:.7;line-height:1.3;display:block}\
 .results{border-collapse:collapse;width:100%;font-size:.92em}\
 .results th,.results td{text-align:left;vertical-align:top;\
   padding:.35rem .6rem;border-bottom:1px solid light-dark(#e2e2e2,#3a3a3c)}\
 .results th{border-bottom-width:2px}\
 .results td{white-space:pre-wrap;word-break:break-word}\
 .results code{font-size:.92em}\
 .count{opacity:.6;font-size:.85em}\
 .ask{font-size:1.3em;font-weight:600}\
 .turtle{background:light-dark(#f6f8fa,#1c1c1e);padding:.6rem .8rem;\
   border-radius:6px;overflow-x:auto;\
   font:13px/1.45 ui-monospace,SFMono-Regular,Menlo,monospace}\
 .error{border:1px solid light-dark(#d93025,#f28b82);border-radius:6px;\
   color:light-dark(#a50e0e,#f28b82);padding:.6rem .8rem;margin:.5rem 0}\
";

/// The highlight overlay: paint the textarea's text into the `<pre>` behind it
/// (comments, strings, IRIs, variables, keywords), keep scroll in sync, and
/// submit on Cmd/Ctrl+Enter. This IS the "editor" — 30 lines instead of a
/// vendored megabyte.
const EDITOR_JS: &str = "\
 (function(){\
 var q=document.getElementById('q'),hl=document.getElementById('hl');\
 var KW='SELECT|ASK|CONSTRUCT|DESCRIBE|WHERE|PREFIX|BASE|FILTER|OPTIONAL|UNION|MINUS|GRAPH|SERVICE|ORDER|GROUP|BY|HAVING|LIMIT|OFFSET|DISTINCT|REDUCED|AS|BIND|VALUES|NOT|EXISTS|IN|FROM|NAMED|COUNT|SUM|AVG|MIN|MAX|SAMPLE|GROUP_CONCAT|STR|CONTAINS|REGEX|LANG|DATATYPE|BOUND|IRI|URI|BNODE|CONCAT|UCASE|LCASE|STRSTARTS|STRENDS|YEAR|MONTH|DAY|NOW|COALESCE|IF|true|false|a';\
 var re=new RegExp('(#[^\\n]*)|(\"(?:[^\"\\\\\\\\]|\\\\\\\\.)*\")|(<[^>\\\\s]*>)|([?$][A-Za-z_][A-Za-z0-9_]*)|\\\\b('+KW+')\\\\b','g');\
 function esc(s){return s.replace(/&/g,'&amp;').replace(/</g,'&lt;')}\
 function paint(){\
 var s=q.value,out='',last=0,m;re.lastIndex=0;\
 while((m=re.exec(s))){\
 out+=esc(s.slice(last,m.index));\
 var cls=m[1]?'c':m[2]?'s':m[3]?'i':m[4]?'v':'k';\
 out+='<span class=\"'+cls+'\">'+esc(m[0])+'<\\/span>';\
 last=m.index+m[0].length;\
 }\
 out+=esc(s.slice(last));\
 hl.innerHTML=out+'\\n';\
 }\
 function sync(){hl.scrollTop=q.scrollTop;hl.scrollLeft=q.scrollLeft}\
 q.addEventListener('input',paint);\
 q.addEventListener('scroll',sync);\
 q.addEventListener('keydown',function(e){\
 if((e.metaKey||e.ctrlKey)&&e.key==='Enter'){e.preventDefault();q.form.submit()}\
 });\
 paint();\
 })();";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_read_forms_route_by_first_meaningful_token() {
        assert_eq!(query_form("SELECT ?s WHERE { ?s ?p ?o }"), Ok("select"));
        assert_eq!(query_form("ask { ?s ?p ?o }"), Ok("ask"));
        assert_eq!(
            query_form("CONSTRUCT { ?s ?p ?o } WHERE { ?s ?p ?o }"),
            Ok("construct")
        );
        assert_eq!(query_form("DESCRIBE <urn:x>"), Ok("describe"));
    }

    #[test]
    fn the_prologue_is_skipped_including_hash_in_iris() {
        // The `#` inside the IRI must not read as a comment.
        let q = "# a leading comment\n\
                 PREFIX ik: <https://ikigai-rs.dev/ns#>\n\
                 BASE <http://example.org/base#frag>\n\
                 SELECT ?s WHERE { ?s a ik:Endpoint }";
        assert_eq!(query_form(q), Ok("select"));
    }

    #[test]
    fn update_forms_are_rejected_loudly() {
        for update in [
            "INSERT DATA { <urn:x> <urn:p> 1 }",
            "DELETE WHERE { ?s ?p ?o }",
            "PREFIX x: <urn:x#>\nWITH <urn:g> DELETE { ?s ?p ?o } WHERE { ?s ?p ?o }",
            "DROP GRAPH <urn:g>",
            "load <http://example.org/data>",
        ] {
            let err = query_form(update).unwrap_err();
            assert!(err.contains("read-only"), "err was: {err}");
        }
    }

    #[test]
    fn junk_and_empty_queries_name_themselves() {
        assert!(query_form("").is_err());
        assert!(query_form("   # only a comment\n").is_err());
        assert!(query_form("FROBNICATE ?x")
            .unwrap_err()
            .contains("FROBNICATE"));
    }

    #[test]
    fn all_eight_samples_are_read_forms() {
        assert_eq!(samples().len(), 8);
        for sample in samples() {
            assert_eq!(
                query_form(&sample.query()),
                Ok("select"),
                "{}",
                sample.title
            );
        }
    }

    #[test]
    fn urlencode_round_trips_through_the_query_decoder() {
        for sample in samples() {
            let encoded = urlencode(&sample.query());
            assert_eq!(
                crate::serve::percent_decode(&encoded, true).as_deref(),
                Some(sample.query().as_str()),
                "{}",
                sample.title
            );
        }
        // `+` must encode (the decoder reads bare `+` as space in query context).
        assert_eq!(urlencode("a+b c"), "a%2Bb%20c");
    }

    #[test]
    fn tsv_terms_render_for_humans() {
        // urn: IRIs link back into this server's own address space.
        assert_eq!(
            term_html("<urn:annotation:x>"),
            "<a href=\"/urn:annotation:x\"><code>urn:annotation:x</code></a>"
        );
        assert_eq!(term_html("<http://ex/y>"), "<code>http://ex/y</code>");
        // Literals lose transport escapes; language tags stay visible.
        assert_eq!(term_html(r#""two\nlines""#), "two\nlines");
        assert_eq!(term_html(r#""hi"@en"#), "hi <small>@en</small>");
        assert_eq!(term_html(r#""q\"uote""#), "q&quot;uote");
        assert_eq!(term_html(r#""\u00e9""#), "\u{e9}");
        // Numbers (Turtle shorthand) and unbound cells pass through.
        assert_eq!(term_html("42"), "42");
        assert_eq!(term_html(""), "");
    }

    #[test]
    fn tsv_tables_carry_headers_rows_and_count() {
        let html = tsv_table("?s\t?n\n<urn:x:1>\t\"note\"\n<urn:x:2>\t3\n");
        assert!(html.contains("<th>s</th><th>n</th>"), "html: {html}");
        assert!(html.contains("urn:x:1"), "html: {html}");
        assert!(html.contains("<td>3</td>"), "html: {html}");
        assert!(html.contains("2 rows"), "html: {html}");
    }

    #[test]
    fn accept_maps_to_sparql_result_types() {
        assert_eq!(
            accept_result_type("text/html,application/xhtml+xml,*/*;q=0.8"),
            Some("text/html")
        );
        assert_eq!(
            accept_result_type("application/sparql-results+json"),
            Some("application/sparql-results+json")
        );
        assert_eq!(
            accept_result_type("application/json"),
            Some("application/sparql-results+json")
        );
        assert_eq!(accept_result_type("text/csv"), Some("text/csv"));
        assert_eq!(accept_result_type("text/turtle"), Some("text/turtle"));
        // curl's default: no opinion → the endpoint's default face.
        assert_eq!(accept_result_type("*/*"), None);
    }
}
