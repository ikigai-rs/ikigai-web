//! Configuration, read from the config home — never from environment variables.
//!
//! The same file every other ikigai process on the machine reads:
//! `$XDG_CONFIG_HOME/ikigai/config.toml` (falling back to
//! `~/.config/ikigai/config.toml`), with the same minimal `key = "value"`
//! line grammar the embedded host uses (`ikigai-embedded/src/config.rs`) —
//! dotted keys are ordinary keys, `#` comments and blank lines are skipped,
//! quotes are trimmed. Kept grammar-identical so a `mount` line means the same
//! thing to the CLI, the daemon, and this server.
//!
//! Keys this server reads:
//! - `mount` (repeatable) — the machine's topology; see [`crate::mounts`].
//! - `web.mount` (repeatable) — mounts for THIS process only, same grammar.
//!   Web-scoped by key: the CLI hosts read `mount` and never `web.mount`, so
//!   a web-only mount (e.g. `web.mount = "prefer urn:sparql:=~/.ikigai/
//!   dev.sock"` for the /sparql face) cannot shadow their local spaces
//!   machine-wide. A repeatable `--mount` flag is the ad-hoc spelling.
//! - `web.bind` — the full `IP:PORT` to bind (a `--bind` flag overrides).
//!   Binding beyond loopback makes the server READ-ONLY; see
//!   [`crate::serve::Posture`].
//! - `web.port` — shorthand for `web.bind = "127.0.0.1:{port}"` (a `--port`
//!   flag overrides). `web.bind` and `web.port` are ONE setting spelled two
//!   ways: setting both is a loud error, not a precedence puzzle.

use std::net::SocketAddr;
use std::path::PathBuf;

/// The port the server binds when neither flags nor config say otherwise.
pub const DEFAULT_PORT: u16 = 8642;

/// `$XDG_CONFIG_HOME/ikigai/config.toml`, or `~/.config/ikigai/config.toml`
/// when `XDG_CONFIG_HOME` is unset. (The XDG variable is the platform's
/// config-home convention, not an ikigai setting channel.)
pub fn config_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var_os("HOME").map_or_else(|| PathBuf::from("."), PathBuf::from);
            home.join(".config")
        });
    base.join("ikigai").join("config.toml")
}

/// Parse a bind address: a full `IP:PORT` socket address, nothing looser.
/// An IP, not a hostname — the bind is a listening posture, and a posture
/// should not depend on what a resolver says today.
pub fn parse_bind(s: &str) -> Result<SocketAddr, String> {
    s.parse().map_err(|_| {
        format!("bind `{s}`: expected IP:PORT (e.g. 0.0.0.0:8642, 127.0.0.1:8642, [::1]:8642)")
    })
}

/// The effective bind address, from flags and config.
///
/// Precedence is flags-over-config WHOLESALE: a `--bind`/`--port` flag decides
/// the whole address and no config key is consulted (so `--port 9000` against
/// a `web.bind = "0.0.0.0:8642"` config serves loopback:9000 — the flag's
/// spelling has always meant loopback, and surprise here fails toward LESS
/// exposure, never more). Within each level, `bind` and `port` are one setting
/// spelled two ways — both at once is a loud error. A contradictory config is
/// refused even when a flag would shadow it: a config that cannot mean one
/// thing should never be half-read.
pub fn resolve_bind(
    bind_flag: Option<&str>,
    port_flag: Option<u16>,
    config_text: &str,
) -> Result<SocketAddr, String> {
    if bind_flag.is_some() && port_flag.is_some() {
        return Err("--bind and --port conflict — --bind names the full IP:PORT".to_string());
    }
    let bind_key = value_for(config_text, "web.bind");
    let port_key = value_for(config_text, "web.port");
    if bind_key.is_some() && port_key.is_some() {
        return Err(
            "config sets both `web.bind` and `web.port` — they are one setting spelled \
             two ways; keep `web.bind` (it names the full IP:PORT) and drop `web.port`"
                .to_string(),
        );
    }
    if let Some(s) = bind_flag {
        return parse_bind(s);
    }
    if let Some(port) = port_flag {
        return Ok(SocketAddr::from(([127, 0, 0, 1], port)));
    }
    if let Some(s) = bind_key {
        return parse_bind(&s).map_err(|e| format!("web.{e}"));
    }
    if let Some(v) = port_key {
        let port: u16 = v
            .parse()
            .map_err(|_| format!("web.port: `{v}` is not a port number"))?;
        return Ok(SocketAddr::from(([127, 0, 0, 1], port)));
    }
    Ok(SocketAddr::from(([127, 0, 0, 1], DEFAULT_PORT)))
}

/// The first `key = value` line for `key` in `text`, trimmed and unquoted.
pub fn value_for(text: &str, key: &str) -> Option<String> {
    lines(text)
        .find(|(name, _)| *name == key)
        .map(|(_, value)| value)
}

/// Every `key = value` line for `key` in `text`, in file order — for settings
/// that legitimately repeat (`mount`: a machine has as many mounts as peers).
pub fn values_for(text: &str, key: &str) -> Vec<String> {
    lines(text)
        .filter(|(name, _)| *name == key)
        .map(|(_, value)| value)
        .collect()
}

/// The meaningful `name = value` lines of `text`: comments and blanks skipped,
/// names and values trimmed, surrounding quotes stripped from values.
fn lines(text: &str) -> impl Iterator<Item = (&str, String)> + '_ {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(|line| line.split_once('='))
        .map(|(name, value)| {
            (
                name.trim(),
                value.trim().trim_matches(['"', '\'']).trim().to_string(),
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEXT: &str = "# a comment\n\
                        lisp.timeout = 300\n\
                        \n\
                        mount = \"prefer urn:py:=/x/py.sock\"\n\
                        mount = \"prefer urn:repo:=/x/dev.sock\"\n\
                        web.port = '9999'\n";

    #[test]
    fn first_value_is_found_trimmed_and_unquoted() {
        assert_eq!(value_for(TEXT, "web.port").as_deref(), Some("9999"));
        assert_eq!(value_for(TEXT, "lisp.timeout").as_deref(), Some("300"));
        assert_eq!(value_for(TEXT, "absent"), None);
    }

    #[test]
    fn repeated_keys_surface_in_file_order() {
        assert_eq!(
            values_for(TEXT, "mount"),
            vec![
                "prefer urn:py:=/x/py.sock".to_string(),
                "prefer urn:repo:=/x/dev.sock".to_string(),
            ]
        );
    }

    #[test]
    fn comments_and_blanks_are_skipped() {
        assert_eq!(value_for("# mount = \"x\"\n", "mount"), None);
        assert!(values_for("", "mount").is_empty());
    }

    fn addr(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn bind_resolution_walks_flags_then_config_then_default() {
        // The flag decides the whole address; config keys are not consulted.
        assert_eq!(
            resolve_bind(Some("0.0.0.0:9001"), None, "web.port = \"7\"\n"),
            Ok(addr("0.0.0.0:9001"))
        );
        // --port is loopback shorthand, even against a non-loopback web.bind
        // (flags-over-config wholesale; fails toward LESS exposure).
        assert_eq!(
            resolve_bind(None, Some(9002), "web.bind = \"0.0.0.0:8642\"\n"),
            Ok(addr("127.0.0.1:9002"))
        );
        assert_eq!(
            resolve_bind(None, None, "web.bind = \"0.0.0.0:8642\"\n"),
            Ok(addr("0.0.0.0:8642"))
        );
        assert_eq!(
            resolve_bind(None, None, "web.port = '9999'\n"),
            Ok(addr("127.0.0.1:9999"))
        );
        assert_eq!(resolve_bind(None, None, ""), Ok(addr("127.0.0.1:8642")));
        // IPv6 spells with brackets, like every other socket address.
        assert_eq!(
            resolve_bind(Some("[::1]:8642"), None, ""),
            Ok(addr("[::1]:8642"))
        );
    }

    #[test]
    fn garbage_binds_fail_loud() {
        for bad in [
            "nonsense",
            "0.0.0.0",        // no port
            ":8642",          // no IP
            "localhost:8642", // a hostname is not an IP
            "0.0.0.0:notaport",
            "300.1.1.1:80",
        ] {
            let err = resolve_bind(Some(bad), None, "").unwrap_err();
            assert!(err.contains("expected IP:PORT"), "for `{bad}`: {err}");
        }
        // The config spelling names its key in the error.
        let err = resolve_bind(None, None, "web.bind = \"nonsense\"\n").unwrap_err();
        assert!(err.starts_with("web.bind"), "err was: {err}");
        let err = resolve_bind(None, None, "web.port = \"nope\"\n").unwrap_err();
        assert!(err.starts_with("web.port"), "err was: {err}");
    }

    #[test]
    fn conflicting_spellings_are_refused_not_ranked() {
        assert!(resolve_bind(Some("127.0.0.1:1"), Some(2), "").is_err());
        // A contradictory config refuses even under a shadowing flag.
        let both = "web.bind = \"0.0.0.0:8642\"\nweb.port = \"9999\"\n";
        assert!(resolve_bind(None, None, both).is_err());
        assert!(resolve_bind(Some("127.0.0.1:1"), None, both).is_err());
    }
}
