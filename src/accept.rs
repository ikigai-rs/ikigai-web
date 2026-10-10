//! What an accept error means for the loop that saw it (ledger #753).
//!
//! ⚠ **A THIRD COPY, on purpose and under protest.** The table and the backoff below are copied
//! from ikigai-cli's `ikigai-web` LIBRARY (`crates/ikigai-web/src/accept.rs`, cli PR #398, ledger
//! #720), which has a twin in `ikigai-ipc` (`crates/ikigai-ipc/src/accept.rs`). Neither exports
//! it: both are `pub(crate)`, and the library's `serve_with_listener` drives the library's OWN
//! HTTP handler, not this server's, so there was nothing to depend on. Ledger #753 asks for a
//! shared home (a small crate, or a `pub` module of the library) — when one exists, delete this
//! file and take it. Until then: change all three or none.
//!
//! The loop used to be `if let Ok(..) = listener.accept().await` inside `loop {}`. At `EMFILE`
//! tokio does not clear the listener's readiness (only `WouldBlock` does), and Linux leaves the
//! connection in the backlog, so every retry failed at once and the loop spun a core for as long
//! as the process was out of descriptors (`tests/accept_exhaustion.rs`). Accept errors are not
//! one thing, and only one kind of them says the listener is finished:
//!
//! - **Per-connection** ([`Fault::Skip`]): the failure belongs to one would-be connection — it
//!   was aborted or reset before we took it, or Linux handed us a network error that was
//!   pending on it (`accept(2)`: "treat them like EAGAIN by retrying"). Take the next one at
//!   once. Not logged: a client can cause these at will, so logging each one is a log-flooding
//!   lever handed to anyone who can reach the port.
//! - **Exhaustion** ([`Fault::BackOff`]): the process or the system is out of something
//!   (`EMFILE`, `ENFILE`, `ENOBUFS`, `ENOMEM`). Retrying immediately fails again, so wait —
//!   bounded, doubling from 10ms to 1s — and retry. Anything this table does not recognize is
//!   treated the same way: a door that keeps trying, and says so, beats a door that exits on an
//!   errno nobody anticipated.
//! - **Fatal** ([`Fault::Fatal`]): the listener itself is unusable (`EBADF`, `ENOTSOCK`,
//!   `EINVAL` — not listening, `EFAULT`). Retrying would back off forever against a dead
//!   socket and hide that the door is gone, so these alone end the loop.
//!
//! The library's module docs carry the comparison with axum's `serve` this table was matched
//! against; it is not repeated here.

use std::io;
use std::time::Duration;

/// The first wait after an exhaustion error.
pub(crate) const BACKOFF_FLOOR: Duration = Duration::from_millis(10);
/// The longest wait between retries during one exhaustion episode.
pub(crate) const BACKOFF_CEILING: Duration = Duration::from_secs(1);

/// What the accept loop does about one error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Fault {
    /// One connection's problem: take the next connection at once.
    Skip,
    /// Out of a resource, or unrecognized: wait, then retry.
    BackOff,
    /// The listener cannot accept again: end the loop with this error.
    Fatal,
}

/// Classify one accept error. See the module docs for the table and why.
pub(crate) fn classify(e: &io::Error) -> Fault {
    #[cfg(unix)]
    if let Some(code) = e.raw_os_error() {
        return match code {
            libc::EBADF | libc::ENOTSOCK | libc::EINVAL | libc::EFAULT => Fault::Fatal,
            libc::EMFILE | libc::ENFILE | libc::ENOBUFS | libc::ENOMEM => Fault::BackOff,
            libc::ECONNABORTED
            | libc::ECONNRESET
            | libc::ECONNREFUSED
            | libc::EINTR
            | libc::EAGAIN
            | libc::EPROTO
            | libc::EPERM
            | libc::ETIMEDOUT
            | libc::ENETDOWN
            | libc::ENETUNREACH
            | libc::EHOSTDOWN
            | libc::EHOSTUNREACH
            | libc::ENOPROTOOPT
            | libc::EOPNOTSUPP => Fault::Skip,
            _ => Fault::BackOff,
        };
    }
    match e.kind() {
        io::ErrorKind::ConnectionAborted
        | io::ErrorKind::ConnectionReset
        | io::ErrorKind::ConnectionRefused
        | io::ErrorKind::Interrupted
        | io::ErrorKind::WouldBlock => Fault::Skip,
        _ => Fault::BackOff,
    }
}

/// One exhaustion episode's state: the next wait, and what to say when it ends.
///
/// An episode starts at the first [`Fault::BackOff`] after a success and ends at the next
/// successful accept. It logs exactly twice — at its start and at its end — however many
/// retries it takes, so the log says THAT the door struggled and for how long, not every
/// attempt.
#[derive(Debug, Default)]
pub(crate) struct Backoff {
    /// The next wait; `None` outside an episode.
    next: Option<Duration>,
    /// Failed accepts in this episode.
    failures: u32,
    /// Total time waited in this episode (summed waits, not a clock reading).
    waited: Duration,
}

impl Backoff {
    /// Record an exhaustion error and return how long to wait before retrying.
    pub(crate) fn failed(&mut self, e: &io::Error) -> Duration {
        let wait = match self.next {
            None => {
                eprintln!(
                    "ikigai-web: accept failing ({e}); backing off up to {}ms and retrying",
                    BACKOFF_CEILING.as_millis()
                );
                BACKOFF_FLOOR
            }
            Some(wait) => wait,
        };
        self.next = Some((wait * 2).min(BACKOFF_CEILING));
        self.failures += 1;
        self.waited += wait;
        wait
    }

    /// Record a successful accept, closing the episode if one was open.
    pub(crate) fn succeeded(&mut self) {
        if self.next.take().is_some() {
            eprintln!(
                "ikigai-web: accept recovered after {} failed attempts (~{}ms backed off)",
                self.failures,
                self.waited.as_millis()
            );
            self.failures = 0;
            self.waited = Duration::ZERO;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn exhaustion_backs_off_a_dead_listener_is_fatal_one_connection_is_skipped() {
        let os = io::Error::from_raw_os_error;
        for code in [libc::EMFILE, libc::ENFILE, libc::ENOBUFS, libc::ENOMEM] {
            assert_eq!(classify(&os(code)), Fault::BackOff, "errno {code}");
        }
        for code in [libc::EBADF, libc::ENOTSOCK, libc::EINVAL, libc::EFAULT] {
            assert_eq!(classify(&os(code)), Fault::Fatal, "errno {code}");
        }
        for code in [
            libc::ECONNABORTED,
            libc::ECONNRESET,
            libc::EPROTO,
            libc::EINTR,
            libc::ENETUNREACH,
        ] {
            assert_eq!(classify(&os(code)), Fault::Skip, "errno {code}");
        }
        // An errno nobody listed keeps the door trying rather than ending it.
        assert_eq!(classify(&os(libc::EIO)), Fault::BackOff);
    }

    #[test]
    fn an_error_without_an_errno_is_classified_by_kind() {
        let kind = |k| io::Error::new(k, "synthetic");
        assert_eq!(
            classify(&kind(io::ErrorKind::ConnectionAborted)),
            Fault::Skip
        );
        assert_eq!(classify(&kind(io::ErrorKind::Other)), Fault::BackOff);
    }

    #[test]
    fn the_backoff_doubles_from_the_floor_to_the_ceiling_and_resets_on_success() {
        let e = io::Error::other("exhausted");
        let mut backoff = Backoff::default();
        let waits: Vec<u128> = (0..10).map(|_| backoff.failed(&e).as_millis()).collect();
        assert_eq!(waits, [10, 20, 40, 80, 160, 320, 640, 1000, 1000, 1000]);
        backoff.succeeded();
        assert_eq!(
            backoff.failed(&e),
            BACKOFF_FLOOR,
            "a new episode starts at the floor"
        );
    }
}
