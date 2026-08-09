//! `ikigai-web` — serve the machine's kernel to a browser, loopback-only.
//!
//! Composition and policy live in the library (`ikigai_web`); this binary reads
//! the config home + flags (never environment variables), composes the kernel
//! from the machine's `mount` lines, and serves.

use std::sync::Arc;

const DEFAULT_PORT: u16 = 8642;

const USAGE: &str = "usage: ikigai-web [--port N] [--config PATH]\n\
 \n\
 Serves the machine's mounted kernel over HTTP on 127.0.0.1 (loopback only).\n\
 \n\
   --port N       port to listen on (default: `web.port` in the config, else 8642)\n\
   --config PATH  config file (default: ~/.config/ikigai/config.toml)\n";

fn main() {
    let mut port_flag: Option<u16> = None;
    let mut config_flag: Option<String> = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
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

    let mount_values = ikigai_web::config::values_for(&config_text, "mount");
    if mount_values.is_empty() {
        fail(&format!(
            "no `mount` lines in {} — nothing to serve",
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

    let port = port_flag
        .or_else(|| {
            ikigai_web::config::value_for(&config_text, "web.port").map(|v| match v.parse() {
                Ok(p) => p,
                Err(_) => fail(&format!("web.port: `{v}` is not a port number")),
            })
        })
        .unwrap_or(DEFAULT_PORT);

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
        let listener = match ikigai_web::serve::bind(port).await {
            Ok(listener) => listener,
            Err(e) => fail(&format!("cannot bind 127.0.0.1:{port}: {e}")),
        };
        eprintln!("serving http://127.0.0.1:{port}/ (loopback only)");
        ikigai_web::serve::serve(kernel, listener).await
    })
}

fn fail(message: &str) -> ! {
    eprintln!("ikigai-web: {message}");
    std::process::exit(1);
}
