//! Compose the served kernel from the machine's `mount` config lines.
//!
//! Each line is `<mode> <prefix>=<target>`, the same grammar the CLI's
//! `config_mounts()` reads (`mount = "prefer urn:repo:=~/.ikigai/dev.sock"`):
//!
//! - `alias` (or `mount`) — the prefix is a LOCAL name for the remote's `urn:`
//!   namespace (stripped and re-prefixed on the way through).
//! - `override` — the IRI forwards unchanged; the remote genuinely serves that
//!   namespace. Connected eagerly: you named the peer because you want it, and
//!   a silent no-op is the failure worth being loud about.
//! - `prefer` — like `override`, but the peer may legitimately be absent
//!   (an on-demand LaunchAgent, a sleeping workstation). Connected lazily, on
//!   first use, retrying on every use after a failure — so "when it's around"
//!   means *now*, not *at startup*. While it is absent, requests under its
//!   prefix surface the transient `Unavailable` (→ 503 at the HTTP face).
//!
//! v1 dials **Unix-socket (IPC) targets only** — `quic://` and `peer:` targets
//! are a startup error, not a silent skip, so a config this server cannot honor
//! is impossible to mistake for one it did. This process composes NOTHING
//! local: the kernel is exactly its mounts (plus an empty local space), which
//! is why `prefer` and `override` front-compose identically here — there is no
//! local binding to fall back to or shadow.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use ikigai_core::{Fallback, Kernel, Space};
use ikigai_resolve::{MountedRemote, Resolver};

/// How a mount relates the local namespace to the remote one. Mirrors the CLI
/// flag grammar (`--mount` / `--override` / `--prefer`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MountKind {
    Alias,
    Override,
    Prefer,
}

/// One parsed `mount =` config line.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct MountLine {
    pub kind: MountKind,
    /// The IRI prefix the mount claims, e.g. `urn:repo:`.
    pub prefix: String,
    /// The dial target: an absolute Unix-socket path (after `~` expansion).
    pub target: String,
}

/// Parse one `mount =` value: `<mode> <prefix>=<target>`.
///
/// A trailing third token (the QUIC cert-dir the CLI grammar allows) is
/// rejected here along with `quic://` and `peer:` targets — v1 is IPC-only.
pub fn parse_mount_line(line: &str) -> Result<MountLine, String> {
    let mut parts = line.split_whitespace();
    let mode = parts
        .next()
        .ok_or_else(|| format!("mount `{line}`: expected <mode> <prefix>=<target>"))?;
    let kind = match mode {
        "alias" | "mount" => MountKind::Alias,
        "override" => MountKind::Override,
        "prefer" => MountKind::Prefer,
        other => {
            return Err(format!(
                "mount `{line}`: unknown mode `{other}` (alias | override | prefer)"
            ))
        }
    };
    let spec = parts
        .next()
        .ok_or_else(|| format!("mount `{line}`: expected <prefix>=<target>"))?;
    let (prefix, target) = spec
        .split_once('=')
        .ok_or_else(|| format!("mount `{line}`: expected <prefix>=<target>, got `{spec}`"))?;
    if let Some(extra) = parts.next() {
        return Err(format!(
            "mount `{line}`: unexpected `{extra}` — v1 dials Unix-socket targets only \
             (no QUIC cert-dir)"
        ));
    }
    if target.starts_with("quic://") || target.starts_with("peer:") {
        return Err(format!(
            "mount `{line}`: v1 dials Unix-socket (IPC) targets only; `{target}` needs the \
             CLI's QUIC machinery"
        ));
    }
    let target = expand_home(target);
    if !target.starts_with('/') {
        return Err(format!(
            "mount `{line}`: target `{target}` is not an absolute Unix-socket path"
        ));
    }
    Ok(MountLine {
        kind,
        prefix: prefix.to_string(),
        target,
    })
}

/// `~/x` → `$HOME/x`. A config file is hand-written, and `~` is what a person types.
fn expand_home(path: &str) -> String {
    match path.strip_prefix("~/") {
        Some(rest) => PathBuf::from(std::env::var("HOME").unwrap_or_default())
            .join(rest)
            .display()
            .to_string(),
        None => path.to_string(),
    }
}

/// Build the served kernel from parsed mount lines.
///
/// Override and prefer mounts front-compose (most-specific prefix first, so a
/// whole-IRI mount beats a namespace mount); alias mounts follow; an empty
/// local space closes the chain. Alias and override targets connect NOW and a
/// refusal is a startup error; prefer targets connect on first use.
pub fn compose(lines: Vec<MountLine>) -> Result<Kernel, String> {
    let mut fronting: Vec<(String, Arc<dyn Resolver>)> = Vec::new();
    let mut aliases: Vec<Arc<dyn Space>> = Vec::new();
    for line in lines {
        match line.kind {
            MountKind::Alias => {
                let resolver = connect(&line.target, ikigai_ipc::HelloMode::Alias)
                    .map_err(|e| format!("mount alias {}: {e}", line.target))?;
                aliases.push(Arc::new(MountedRemote::new(
                    resolver,
                    line.prefix,
                    line.target,
                )));
            }
            MountKind::Override => {
                let resolver = connect(&line.target, ikigai_ipc::HelloMode::Verbatim)
                    .map_err(|e| format!("mount override {}: {e}", line.target))?;
                fronting.push((line.prefix, resolver));
            }
            MountKind::Prefer => {
                let resolver: Arc<dyn Resolver> = Arc::new(LazyIpcResolver {
                    target: line.target,
                    inner: Mutex::new(None),
                });
                fronting.push((line.prefix, resolver));
            }
        }
    }
    // Most specific prefix wins regardless of declaration order — a whole IRI
    // is simply the most specific prefix there is.
    fronting.sort_by_key(|(prefix, _)| std::cmp::Reverse(prefix.len()));
    let mut ordered: Vec<Arc<dyn Space>> = Vec::new();
    for (prefix, resolver) in fronting {
        let origin = resolver.transport();
        ordered.push(Arc::new(MountedRemote::overriding(
            resolver, prefix, origin,
        )));
    }
    ordered.extend(aliases);
    // The empty local space — this process serves its mounts and nothing else.
    ordered.push(Arc::new(ikigai_core::EndpointSpace::new()));
    Ok(Kernel::new(Arc::new(Fallback::new(ordered))))
}

/// Dial an IPC peer and box it as a mountable resolver.
fn connect(target: &str, mode: ikigai_ipc::HelloMode) -> std::io::Result<Arc<dyn Resolver>> {
    let resolver = ikigai_ipc::connect_as(Path::new(target), mode)?;
    Ok(Arc::new(resolver))
}

/// A prefer-mount resolver that dials on FIRST USE and re-tries on every use
/// after a failure (the CLI's `LazyResolver`, IPC-only). A failed dial surfaces
/// as the transient [`ikigai_core::Error::Unavailable`] it is — 503 at the HTTP
/// face. Once connected, the resolver is kept; the transport re-establishes its
/// own connection on later transport errors.
struct LazyIpcResolver {
    target: String,
    inner: Mutex<Option<Arc<dyn Resolver>>>,
}

impl LazyIpcResolver {
    fn get(&self) -> Result<Arc<dyn Resolver>, ikigai_core::Error> {
        if let Some(resolver) = self.inner.lock().unwrap().clone() {
            return Ok(resolver);
        }
        // A prefer mount forwards IRIs unchanged, so it speaks verbatim.
        let resolver = connect(&self.target, ikigai_ipc::HelloMode::Verbatim)
            .map_err(|e| ikigai_core::Error::Unavailable(format!("{}: {e}", self.target)))?;
        *self.inner.lock().unwrap() = Some(Arc::clone(&resolver));
        Ok(resolver)
    }
}

impl Resolver for LazyIpcResolver {
    fn issue(
        &self,
        request: ikigai_core::Request,
    ) -> Result<(ikigai_core::Representation, ikigai_resolve::CacheStatus), ikigai_core::Error>
    {
        self.get()?.issue(request)
    }

    fn is_cached(
        &self,
        request: &ikigai_core::Request,
        capability: &ikigai_core::Capability,
    ) -> bool {
        // An unreachable peer has nothing cached, and probing must not dial.
        match self.inner.lock().unwrap().clone() {
            Some(resolver) => resolver.is_cached(request, capability),
            None => false,
        }
    }

    fn entries(&self) -> Option<Vec<ikigai_core::SpaceEntry>> {
        // Dial if we never have: an enumeration deserves the truth, and a Unix
        // socket refusal is instant (no negative cache needed at IPC speeds).
        self.get().ok()?.entries()
    }

    fn transport(&self) -> String {
        match self.inner.lock().unwrap().clone() {
            Some(resolver) => resolver.transport(),
            None => format!("{} · not connected", self.target),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_three_modes_parse() {
        let line = parse_mount_line("prefer urn:repo:=/x/dev.sock").unwrap();
        assert_eq!(line.kind, MountKind::Prefer);
        assert_eq!(line.prefix, "urn:repo:");
        assert_eq!(line.target, "/x/dev.sock");
        assert_eq!(
            parse_mount_line("override urn:py:=/x/py.sock")
                .unwrap()
                .kind,
            MountKind::Override
        );
        assert_eq!(
            parse_mount_line("alias urn:cal:=/x/cal.sock").unwrap().kind,
            MountKind::Alias
        );
        // `mount` is the flagless spelling of alias.
        assert_eq!(
            parse_mount_line("mount urn:cal:=/x/cal.sock").unwrap().kind,
            MountKind::Alias
        );
    }

    #[test]
    fn tilde_expands_to_home() {
        let home = std::env::var("HOME").unwrap();
        let line = parse_mount_line("prefer urn:repo:=~/.ikigai/dev.sock").unwrap();
        assert_eq!(line.target, format!("{home}/.ikigai/dev.sock"));
    }

    #[test]
    fn unsupported_targets_fail_loudly_at_parse_time() {
        assert!(parse_mount_line("prefer urn:llm:=peer:plasma").is_err());
        assert!(parse_mount_line("alias urn:cal:=quic://bug.local:4433").is_err());
        assert!(parse_mount_line("alias urn:cal:=quic://x:1 ~/.config/ikigai/quic-x").is_err());
        assert!(parse_mount_line("prefer urn:repo:=relative/path.sock").is_err());
    }

    #[test]
    fn malformed_lines_name_themselves() {
        for bad in ["", "prefer", "prefer urn:x:", "teleport urn:x:=/y"] {
            let err = parse_mount_line(bad).unwrap_err();
            assert!(err.starts_with("mount `"), "err was: {err}");
        }
    }

    #[test]
    fn prefer_mounts_compose_without_dialing() {
        // A prefer target that does not exist must not fail composition —
        // absence at startup is its normal operation.
        let kernel = compose(vec![parse_mount_line(
            "prefer urn:repo:=/nonexistent/dev.sock",
        )
        .unwrap()])
        .unwrap();
        // The mount is there (resolution will 503 on use), the kernel is real.
        drop(kernel);
    }

    #[test]
    fn eager_mounts_refuse_a_dead_target() {
        let result = compose(vec![parse_mount_line(
            "override urn:repo:=/nonexistent/dev.sock",
        )
        .unwrap()]);
        match result {
            Ok(_) => panic!("a dead eager target must refuse composition"),
            Err(err) => assert!(err.contains("/nonexistent/dev.sock"), "err was: {err}"),
        }
    }
}
