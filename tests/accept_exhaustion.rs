//! The accept loop under descriptor exhaustion (ledger #753).
//!
//! The loop used to be `if let Ok(..) = listener.accept().await` inside `loop {}`. At `EMFILE`
//! tokio does not clear the listener's readiness (only `WouldBlock` does), so the next accept
//! fails at once, and the loop spun a core for as long as the process was out of descriptors.
//!
//! Each scenario runs in a CHILD process — this test binary re-executed with one `#[ignore]`d
//! test selected — because it lowers the descriptor limit and then fills every slot, which no
//! other test in the same process could survive. The child reports on stdout; the parent judges.
//!
//! The setup is deterministic, not a race: with every descriptor filled, freeing exactly ONE and
//! connecting a client spends that one on the client's socket before the connection can reach
//! the backlog, so the server's accept finds the connection and no descriptor to put it in.
//!
//! ⚠ The kernels differ at that point, measured 2026-10-10. **Linux** leaves the connection in
//! the backlog, so a loop that retries at once fails at once, forever: that is the spin. **macOS**
//! aborts the connection inside the failed `accept` (the client's socket goes dead: `getpeername`
//! answers `EINVAL`), so the next attempt finds an empty backlog and waits — no spin, but every
//! connection that arrives while the process is full is thrown away, as fast as it arrives. The
//! assertions hold on both; only the Linux run can show the spin, so CI (ubuntu) is where the
//! unfixed loop fails.

#![cfg(unix)]

use std::fs::File;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::os::fd::AsRawFd;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ikigai_core::{EndpointSpace, Fallback, Kernel, Space};
use ikigai_web_server::ceiling::Ceiling;

/// The marker that turns an `#[ignore]`d scenario on. Without it a scenario is a no-op, so a
/// stray `cargo test -- --ignored` cannot lower the limit of a process running other tests.
const CHILD: &str = "IKIGAI_WEB_ACCEPT_EXHAUSTION_CHILD";

/// The descriptor limit a child runs under: small enough to fill quickly, large enough for the
/// runtime and the harness.
const FD_LIMIT: libc::rlim_t = 128;

/// How long the CPU is sampled while the server is out of descriptors.
const SAMPLE: Duration = Duration::from_secs(2);

/// Run one scenario in a child process; return (stdout, stderr). `output()` waits for the
/// child to be REAPED, so its status is final (field guide 9h).
fn run_child(scenario: &str) -> (String, String) {
    let exe = std::env::current_exe().expect("test binary path");
    let out = Command::new(exe)
        .args([
            scenario,
            "--exact",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD, "1")
        .output()
        .expect("spawn the child scenario");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        out.status.success(),
        "child {scenario} failed ({}):\n--- stdout\n{stdout}\n--- stderr\n{stderr}",
        out.status
    );
    (stdout, stderr)
}

/// One `RESULT key=value` line from a child's stdout.
fn result(stdout: &str, key: &str) -> String {
    let prefix = format!("RESULT {key}=");
    stdout
        .lines()
        // The harness may print the test's name on the same line, before it.
        .find_map(|line| line.find(&prefix).map(|at| &line[at + prefix.len()..]))
        .unwrap_or_else(|| panic!("child reported no {key}:\n{stdout}"))
        .to_string()
}

/// Process CPU time (user + system), all threads.
fn cpu_time() -> Duration {
    // SAFETY: getrusage writes one `rusage` into the zeroed struct we hand it.
    let usage = unsafe {
        let mut usage: libc::rusage = std::mem::zeroed();
        assert_eq!(libc::getrusage(libc::RUSAGE_SELF, &mut usage), 0);
        usage
    };
    let tv = |t: libc::timeval| {
        Duration::from_secs(t.tv_sec as u64) + Duration::from_micros(t.tv_usec as u64)
    };
    tv(usage.ru_utime) + tv(usage.ru_stime)
}

fn lower_fd_limit() {
    // SAFETY: get/setrlimit read and write one `rlimit`; lowering the soft limit needs no
    // privilege.
    unsafe {
        let mut limit: libc::rlimit = std::mem::zeroed();
        assert_eq!(libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit), 0);
        limit.rlim_cur = FD_LIMIT.min(limit.rlim_max);
        assert_eq!(libc::setrlimit(libc::RLIMIT_NOFILE, &limit), 0);
    }
}

/// Open `/dev/null` until the process is out of descriptors; return what was opened.
fn fill_descriptors() -> Vec<File> {
    let mut fillers = Vec::new();
    loop {
        match File::open("/dev/null") {
            Ok(file) => fillers.push(file),
            Err(e) if e.raw_os_error() == Some(libc::EMFILE) => return fillers,
            Err(e) => panic!("filling descriptors: {e}"),
        }
    }
}

/// A server over an empty kernel on a loopback port, on its own thread and runtime.
/// Returns where it listens and the listener's descriptor.
fn spawn_server() -> (SocketAddr, i32) {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async move {
            let listener = ikigai_web_server::serve::bind("127.0.0.1:0".parse().unwrap())
                .await
                .unwrap();
            tx.send((listener.local_addr().unwrap(), listener.as_raw_fd()))
                .unwrap();
            let kernel = Kernel::new(Arc::new(Fallback::new(vec![
                Arc::new(EndpointSpace::new()) as Arc<dyn Space>,
            ])));
            ikigai_web_server::serve::serve(Arc::new(kernel), listener, Ceiling::unset()).await
        })
    });
    rx.recv().expect("server bound")
}

/// How many connections wait in the backlog while the server is out of descriptors.
const QUEUED: usize = 16;

/// Put the server at EMFILE with [`QUEUED`] connections in its backlog, each with its request
/// already written. Returns the clients and the descriptors holding the process full.
///
/// One descriptor is freed per client, and the client's socket takes it before its connection
/// exists, so the server never finds a free slot: every accept it attempts is an `EMFILE`.
fn exhaust(addr: SocketAddr) -> (Vec<TcpStream>, Vec<File>) {
    let mut fillers = fill_descriptors();
    let mut clients = Vec::new();
    for _ in 0..QUEUED {
        fillers.pop();
        let mut client = TcpStream::connect(addr).expect("connect with the one free descriptor");
        // macOS may already have aborted this connection (see the module docs), so a failed
        // write is not this test's business.
        let _ = client.write_all(REQUEST);
        clients.push(client);
    }
    (clients, fillers)
}

const REQUEST: &[u8] = b"GET / HTTP/1.1\r\nHost: test\r\n\r\n";

/// Read one response; true when it is HTTP.
fn answered(client: &mut TcpStream) -> bool {
    if client
        .set_read_timeout(Some(Duration::from_secs(5)))
        .is_err()
    {
        return false;
    }
    let mut response = Vec::new();
    let _ = client.read_to_end(&mut response);
    response.starts_with(b"HTTP/1.1 ")
}

#[test]
fn exhaustion_does_not_spin_and_the_door_serves_again_once_descriptors_free_up() {
    let (stdout, stderr) = run_child("child_exhaust_then_recover");
    let busy: f64 = result(&stdout, "cpu_fraction").parse().unwrap();
    assert!(
        busy < 0.25,
        "the accept loop burned {busy:.2} of a core while out of descriptors (spinning)"
    );
    // The scenario must really have put the server at EMFILE, or a quiet CPU proves nothing.
    assert!(
        stderr.contains("accept failing"),
        "the server never reported exhaustion, so the scenario did not reach EMFILE:\n{stderr}"
    );
    assert_eq!(
        result(&stdout, "fresh_served"),
        "true",
        "a new connection was not served once descriptors were free"
    );
    // Linux keeps a connection queued through EMFILE, so the one that was waiting must be
    // answered too. (macOS aborts it at the failed accept: there is nothing left to answer.)
    if cfg!(target_os = "linux") {
        assert_eq!(result(&stdout, "queued_served"), "true");
    }
    assert!(
        stderr.contains("accept recovered"),
        "the episode's end was not reported:\n{stderr}"
    );
}

#[test]
#[ignore = "a child scenario: run by the test above, in its own process"]
fn child_exhaust_then_recover() {
    if std::env::var_os(CHILD).is_none() {
        return;
    }
    lower_fd_limit();
    let (addr, _fd) = spawn_server();
    let (mut clients, fillers) = exhaust(addr);

    std::thread::sleep(Duration::from_millis(300));
    let (cpu0, wall0) = (cpu_time(), Instant::now());
    std::thread::sleep(SAMPLE);
    let busy = (cpu_time() - cpu0).as_secs_f64() / wall0.elapsed().as_secs_f64();
    println!("RESULT cpu_fraction={busy:.3}");

    // Give the descriptors back: a new connection must be served, and on Linux the first one
    // that waited through the episode as well.
    drop(fillers);
    let mut fresh = TcpStream::connect(addr).expect("connect after recovery");
    fresh.write_all(REQUEST).unwrap();
    println!("RESULT fresh_served={}", answered(&mut fresh));
    println!("RESULT queued_served={}", answered(&mut clients[0]));
}
