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
//! - `web.port` — the port to serve on (a `--port` flag overrides).

use std::path::PathBuf;

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
}
