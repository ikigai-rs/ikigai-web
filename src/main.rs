//! `ikigai-web` — serve the machine's kernel to a browser.
//!
//! Composition and policy live in the library (`ikigai_web`); this binary reads
//! the config home + flags (never environment variables), composes the kernel
//! from the machine's `mount` lines, and serves. Default bind is loopback (the
//! full v1 surface); a non-loopback bind serves READ-ONLY — see the library's
//! trust-posture doc.

use std::sync::Arc;

const USAGE: &str =
    "usage: ikigai-web [--bind IP:PORT | --port N] [--config PATH] [--mount LINE ...]\n\
 \n\
 Serves the machine's mounted kernel over HTTP. Default bind: 127.0.0.1:8642\n\
 (loopback — the full surface). A NON-loopback bind serves read-only: the\n\
 write surface is disabled, GET/HEAD and /sparql queries only.\n\
 \n\
   --bind IP:PORT address to bind (config: `web.bind`); an IP, not a hostname.\n\
                  e.g. --bind 0.0.0.0:8642 to demo browse + /sparql to the LAN\n\
   --port N       shorthand for --bind 127.0.0.1:N (config: `web.port`).\n\
                  Flags override config wholesale; `web.bind` and `web.port`\n\
                  are one setting spelled two ways — setting both is an error\n\
   --config PATH  config file (default: ~/.config/ikigai/config.toml)\n\
   --mount LINE   additional mount for THIS process only (repeatable; same\n\
                  grammar as a config `mount` line, e.g.\n\
                  'prefer urn:sparql:=~/.ikigai/dev.sock'). The persistent\n\
                  spelling is a `web.mount` line in the config: web-scoped by\n\
                  key, so the CLI hosts never read it and their local spaces\n\
                  are never shadowed machine-wide.\n";

fn main() {
    let mut bind_flag: Option<String> = None;
    let mut port_flag: Option<u16> = None;
    let mut config_flag: Option<String> = None;
    let mut mount_flags: Vec<String> = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--bind" => match args.next() {
                Some(value) => bind_flag = Some(value),
                None => fail("--bind: expected IP:PORT (e.g. 0.0.0.0:8642)"),
            },
            "--port" => {
                let value = args.next().unwrap_or_default();
                match value.parse() {
                    Ok(p) => port_flag = Some(p),
                    Err(_) => fail(&format!("--port: `{value}` is not a port number")),
                }
            }
            "--config" => match args.next() {
                Some(path) => config_flag = Some(path),
                None => fail("--config: expected a path"),
            },
            "--mount" => match args.next() {
                Some(line) => mount_flags.push(line),
                None => fail("--mount: expected '<mode> <prefix>=<target>'"),
            },
            "--help" | "-h" => {
                print!("{USAGE}");
                return;
            }
            other => fail(&format!("unknown flag `{other}`\n\n{USAGE}")),
        }
    }

    let config_path = config_flag
        .map(std::path::PathBuf::from)
        .unwrap_or_else(ikigai_web::config::config_path);
    // Expected-but-unset must stop: a web face over NO mounts serves nothing,
    // and silently starting empty would look exactly like a broken dev server.
    let config_text = match std::fs::read_to_string(&config_path) {
        Ok(text) => text,
        Err(e) => fail(&format!(
            "cannot read config {}: {e}\n(this server composes the machine's `mount` lines; \
             without the config there is nothing to serve)",
            config_path.display()
        )),
    };

    // The machine's shared topology (`mount`), then this process's OWN mounts:
    // `web.mount` config lines (web-scoped by key — the CLI hosts read `mount`,
    // never `web.mount`, so a web-only mount cannot shadow their local spaces
    // machine-wide) and `--mount` flags, in that order.
    let mut mount_values = ikigai_web::config::values_for(&config_text, "mount");
    mount_values.extend(ikigai_web::config::values_for(&config_text, "web.mount"));
    mount_values.extend(mount_flags);
    if mount_values.is_empty() {
        fail(&format!(
            "no `mount`/`web.mount` lines in {} and no --mount flags — nothing to serve",
            config_path.display()
        ));
    }
    let lines: Vec<_> = mount_values
        .iter()
        .map(|value| match ikigai_web::mounts::parse_mount_line(value) {
            Ok(line) => line,
            Err(e) => fail(&e),
        })
        .collect();

    let bind = match ikigai_web::config::resolve_bind(bind_flag.as_deref(), port_flag, &config_text)
    {
        Ok(addr) => addr,
        Err(e) => fail(&e),
    };

    for line in &lines {
        eprintln!("mount: {:?} {} -> {}", line.kind, line.prefix, line.target);
    }
    let kernel = match ikigai_web::mounts::compose(lines) {
        Ok(kernel) => Arc::new(kernel),
        Err(e) => fail(&e),
    };

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    runtime.block_on(async move {
        let listener = match ikigai_web::serve::bind(bind).await {
            Ok(listener) => listener,
            Err(e) => fail(&format!("cannot bind {bind}: {e}")),
        };
        // The posture line states what this bind MEANS, from the socket
        // actually bound ([`serve`] re-derives the same posture internally —
        // if it could not be derived, the server would refuse to start).
        let addr = match listener.local_addr() {
            Ok(addr) => addr,
            Err(e) => fail(&format!("cannot read the bound address: {e}")),
        };
        let posture = match ikigai_web::serve::Posture::of(&listener) {
            Ok(posture) => posture,
            Err(e) => fail(&format!("cannot derive the trust posture: {e}")),
        };
        match posture {
            ikigai_web::serve::Posture::LocalOwner => {
                eprintln!("serving http://{addr}/ (loopback only)")
            }
            ikigai_web::serve::Posture::ReadOnly => eprintln!(
                "serving http://{addr}/ — read-only (non-loopback): the write surface is \
                 disabled; GET/HEAD and /sparql queries only"
            ),
        }
        ikigai_web::serve::serve(kernel, listener).await
    })
}

fn fail(message: &str) -> ! {
    eprintln!("ikigai-web: {message}");
    std::process::exit(1);
}
