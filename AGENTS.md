# Working in this repository

`ikigai-web` is a standalone HTTP and SPARQL server that lets a browser read an ikigai kernel
composed from the machine's mounts: an index page, the browse shell and a SPARQL editor (crate
`ikigai-web-server`, binary `ikigai-web`). These rules are for every agent and every person
working here. `CLAUDE.md` is a link to this file.

- **Read [ACCESSIBILITY.md](ACCESSIBILITY.md) before any change to UI or rendered output**: the
  index page, the browse shell and the SPARQL editor (their markup, style and script in
  `src/serve.rs` and `src/sparql.rs`). It lists what has been tested, what has not, and the
  barriers known today.
- Security reports go through the organization's security policy (GitHub private vulnerability
  reporting), never a public issue.
- Every change lands through a pull request with green CI (`.github/workflows/ci.yml`: fmt,
  clippy with warnings denied, tests and docs).
- Never invent testing, tool results or conformance evidence, and never weaken a test to make a
  change pass.
- Use American spelling in everything you write: code comments, docs, commit messages and pull
  requests.
