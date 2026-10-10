//! Readiness for an IPC peer a test serves IN-PROCESS: wait until it ANSWERS, never until its
//! socket file exists (ledger [#225](http://localhost:1060/l/default/item/225)).
//!
//! ⚠ The file appearing is not the socket listening. `ikigai_ipc::serve` calls
//! `UnixListener::bind`, which binds (creating the file) and only then listens, so a connect in
//! between is refused with `ECONNREFUSED`, and an eager `override` mount dials at `compose`.
//! Measured on `7da4e90` with the old `socket.exists()` wait, 48 concurrent copies of each test
//! binary: `tests/conformance.rs` failed 112 of 1920 runs and `tests/ceiling.rs` 1 of 960, every
//! one with `mount override …/peer.sock: Connection refused`. So the wait is for a connection
//! that completes the version hello, the same thing `compose` will do next.
//!
//! The same fix in gonk is ikigai-rs/ikigai-gonk PR 110 (`tests/ready/mod.rs`).

use std::path::Path;
use std::time::{Duration, Instant};

/// Wait (bounded) until the `ikigai_ipc` socket at `path` accepts a connection and completes
/// the hello. The connection it made is dropped; the caller dials its own.
pub fn socket(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match ikigai_ipc::connect(path) {
            Ok(_) => return,
            Err(e) if Instant::now() > deadline => {
                panic!("the IPC peer at {} never answered: {e}", path.display())
            }
            Err(_) => std::thread::sleep(Duration::from_millis(10)),
        }
    }
}
