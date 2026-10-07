//! The capability ceiling: the most any request through this face may hold.
//!
//! This face fronts whatever the config home mounts, and on a typical machine that is
//! the WHOLE topology: the browse family, the persistent store, the work ledger. Until
//! the ceiling existed every request went out as [`Capability::root`], so a mounted peer
//! that enforces scopes exactly (gonk does, per graph and per ledger) had nothing to
//! enforce against: on 2026-10-07 a LAN-bound 8642 listed the whole work ledger and
//! answered `urn:iki:store:select` with 12,319 ledger quads to anyone on the network.
//! The off-loopback GET/HEAD gate stopped writes; nothing stopped reads.
//!
//! So the face now holds ONE capability, chosen at startup, and every route issues
//! under it (`serve::Face`): `/{uri}`, `HEAD`, the annotation `POST`, both `/k/`
//! commands, `/sparql` (GET, POST and the editor page) and the index's repo list. There
//! is no per-route capability and no header, query parameter or `as=` that selects one,
//! so a route cannot reach past the ceiling by being written carelessly. The ceiling is
//! the face's whole personal space, narrowed from root before anything is issued.
//!
//! Spelled `web.cap` in the config home (repeatable, ONE scope per line, like `mount`)
//! or `--cap` (repeatable); the flags replace the config lines wholesale, as `--bind`
//! replaces `web.bind`. See [`crate::config::resolve_ceiling`].
//!
//! - **Unset on loopback:** root, exactly as before. Loopback's trust model is the local
//!   owner, the same as the dev socket's peer-credential check.
//! - **Unset off loopback: the server refuses to start** ([`Ceiling::admits`]). A LAN
//!   face with no ceiling is the defect measured above, and failing loud names the fix
//!   (`web.cap`) at the moment someone widens the bind, rather than serving root to the
//!   network until somebody notices. There is deliberately no spelling for "root, on
//!   purpose, on the LAN".
//! - **Set:** every request, on any bind, holds exactly these scopes and nothing more.

use std::collections::BTreeSet;

use ikigai_core::Capability;

use crate::serve::Posture;

/// The browse-only ceiling: what a LAN face needs to give today's dev-server browse
/// parity through gonk, and nothing else.
///
/// - `urn:cap:browse:read:*` — every browse root's tree, files, git state, annotations
///   and archive listing. `ikigai-browse` reads this literal scope as the all-roots
///   grant; `urn:cap:browse:read:{root}` names one root instead.
/// - `urn:cap:store:read:graph:urn:iki:browse:graph:default` — the browse graph's quads,
///   through `urn:iki:store:graph-*` and gonk's `urn:sparql:*` forms (which declare the
///   offering `urn:cap:store:read:graph:*` and then check each graph exactly), so
///   `/sparql` over the union reads the browse graph and no other.
///
/// It is gonk's `--browse read` role, scope for scope. It carries no ledger token, no
/// broad `urn:cap:store:read`, no `urn:cap:annotate` and no `urn:cap:net:*` (so nothing
/// through it spends inference).
pub const BROWSE_ONLY: [&str; 2] = [
    "urn:cap:browse:read:*",
    "urn:cap:store:read:graph:urn:iki:browse:graph:default",
];

/// The capability ceiling in force. Built once at startup and handed to
/// [`crate::serve::serve`]; there is no way to change it while the server runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ceiling {
    /// `None` = unset (root, loopback only). `Some` = exactly these scopes.
    scopes: Option<BTreeSet<String>>,
}

impl Ceiling {
    /// No ceiling: requests go out as root. [`Ceiling::admits`] refuses it off loopback.
    pub fn unset() -> Self {
        Ceiling { scopes: None }
    }

    /// A ceiling of exactly `scopes`. Each must be a `urn:cap:` IRI with no whitespace
    /// in it, and there must be at least one: a ceiling that cannot mean one thing is
    /// refused rather than read as something narrower or wider than intended.
    pub fn scoped<I, S>(scopes: I) -> Result<Self, String>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut set = BTreeSet::new();
        for scope in scopes {
            let scope = scope.into();
            if scope.is_empty() {
                return Err("an empty scope: name a `urn:cap:` IRI".to_string());
            }
            if scope.chars().any(char::is_whitespace) {
                return Err(format!(
                    "`{scope}` holds whitespace: give one scope per `web.cap` line \
                     (or per `--cap` flag)"
                ));
            }
            if !scope.starts_with("urn:cap:") {
                return Err(format!("`{scope}` is not a `urn:cap:` scope"));
            }
            set.insert(scope);
        }
        if set.is_empty() {
            return Err("a ceiling with no scopes: name at least one `urn:cap:` scope".into());
        }
        Ok(Ceiling { scopes: Some(set) })
    }

    /// The ceiling's scopes, or `None` when unset (root).
    pub fn scopes(&self) -> Option<&BTreeSet<String>> {
        self.scopes.as_ref()
    }

    /// The one capability every request through this face is issued under.
    pub fn capability(&self) -> Capability {
        match &self.scopes {
            None => Capability::root(),
            Some(scopes) => Capability::scoped(scopes.iter().cloned()),
        }
    }

    /// Whether this ceiling may serve under `posture`. Off loopback an unset ceiling is
    /// refused, with a message naming the key and the browse-only example.
    pub fn admits(&self, posture: Posture) -> Result<(), String> {
        if posture == Posture::ReadOnly && self.scopes.is_none() {
            return Err(refusal());
        }
        Ok(())
    }

    /// One line for the startup banner and the index page.
    pub fn summary(&self) -> String {
        match &self.scopes {
            None => "none: every request is issued as root (loopback, the local owner)".into(),
            Some(scopes) => {
                let list: Vec<&str> = scopes.iter().map(String::as_str).collect();
                format!(
                    "{} (every request holds these and nothing more)",
                    list.join(" ")
                )
            }
        }
    }
}

/// The off-loopback refusal: what is wrong, the key that fixes it, and the example.
fn refusal() -> String {
    let lines: Vec<String> = BROWSE_ONLY
        .iter()
        .map(|scope| format!("  web.cap = \"{scope}\""))
        .collect();
    format!(
        "refusing to serve beyond loopback with no capability ceiling: every request \
         would be issued as root, so the network could read everything the mounted peers \
         serve (the work ledger and the whole store included). Set `web.cap` in the config \
         (one scope per line) or pass `--cap SCOPE` (repeatable). The browse-only ceiling:\n{}",
        lines.join("\n")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unset_is_root_and_is_refused_off_loopback_only() {
        let unset = Ceiling::unset();
        assert!(unset.capability().is_root());
        assert_eq!(unset.admits(Posture::LocalOwner), Ok(()));
        let err = unset.admits(Posture::ReadOnly).unwrap_err();
        assert!(err.contains("web.cap"), "{err}");
        for scope in BROWSE_ONLY {
            assert!(
                err.contains(scope),
                "the refusal carries the example: {err}"
            );
        }
    }

    #[test]
    fn a_set_ceiling_is_exactly_its_scopes_on_any_bind() {
        let ceiling = Ceiling::scoped(BROWSE_ONLY).expect("valid");
        let cap = ceiling.capability();
        assert!(!cap.is_root());
        assert_eq!(
            cap.scopes().map(|s| s.iter().cloned().collect::<Vec<_>>()),
            Some(BROWSE_ONLY.iter().map(|s| s.to_string()).collect()),
        );
        assert!(!cap.allows("urn:cap:ledger:read:default"));
        assert!(!cap.allows("urn:cap:store:read"));
        assert_eq!(ceiling.admits(Posture::ReadOnly), Ok(()));
        assert_eq!(ceiling.admits(Posture::LocalOwner), Ok(()));
    }

    #[test]
    fn a_ceiling_that_cannot_mean_one_thing_is_refused() {
        assert!(Ceiling::scoped(Vec::<String>::new()).is_err());
        assert!(Ceiling::scoped([""]).is_err());
        assert!(Ceiling::scoped(["root"]).is_err());
        assert!(Ceiling::scoped(["urn:browse:read"]).is_err());
        let err = Ceiling::scoped(["urn:cap:a urn:cap:b"]).unwrap_err();
        assert!(err.contains("one scope per"), "{err}");
    }
}
