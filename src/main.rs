//! Umbra: a tiny self-hosted status board for the devices in your home.
//!
//! One native binary: scheduler, ping/TCP checks, SQLite, web server and HTML.
//! Ring and Blink run as short-lived Python helpers (see `integrations`).

mod alerts;
mod config;
mod db;
mod demo;
mod host;
mod integrations;
mod live;
mod net;
mod scheduler;
mod secrets;
mod state;
mod stats;
mod util;
mod web;

#[cfg(test)]
mod tests;

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

const USAGE: &str = "umbra - a tiny status board for your home

USAGE:
    umbra [COMMAND] [--config PATH]

COMMANDS:
    run              Start the dashboard (default)
    demo             Try the UI with a simulated home: no network traffic,
                     no password, listens on 127.0.0.1 only (umbra-demo.toml)
    set-password     Set or change the sign-in password
    check-config     Validate umbra.toml and exit
    print-config     Print the effective config as TOML
    version          Print the version

The config path defaults to ./umbra.toml if present, otherwise
/etc/umbra/umbra.toml (Linux) or %ProgramData%\\Umbra\\umbra.toml (Windows).
";

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let mut cmd = String::from("run");
    let mut flag: Option<PathBuf> = None;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--config" | "-c" => flag = args.next().map(PathBuf::from),
            "-h" | "--help" | "help" => {
                print!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            other if !other.starts_with('-') => cmd = other.to_string(),
            other => {
                eprintln!("unknown option {other}\n\n{USAGE}");
                return ExitCode::from(2);
            }
        }
    }
    let path = if cmd == "demo" { flag.unwrap_or_else(|| demo::PATH.into()) } else { config::resolve_path(flag) };
    let result = match cmd.as_str() {
        "run" => run(path, false),
        "demo" => {
            if path.exists() {
                run(path, true)
            } else {
                demo::config().save(&path).and_then(|()| run(path, true))
            }
        }
        "set-password" => set_password(path),
        "has-password" => config::Config::load(&path).and_then(|c| open_db(&c)).and_then(|db| {
            if db.meta("password_hash").is_some() { Ok(()) } else { Err("no password set".into()) }
        }),
        "check-config" =>config::Config::load(&path).map(|c| println!("{} is valid ({} devices).", path.display(), c.devices.len())),
        "print-config" => config::Config::load(&path).and_then(|c| toml::to_string_pretty(&c).map_err(|e| e.to_string())).map(|s| print!("{s}")),
        "version" => {
            println!("umbra {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        other => Err(format!("unknown command \"{other}\"\n\n{USAGE}")),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("umbra: {e}");
            ExitCode::FAILURE
        }
    }
}

fn open_db(cfg: &config::Config) -> Result<db::Db, String> {
    std::fs::create_dir_all(&cfg.server.data_dir).map_err(|e| format!("creating {}: {e}", cfg.server.data_dir.display()))?;
    db::Db::open(&cfg.server.data_dir.join("umbra.db")).map_err(|e| format!("opening the database: {e}"))
}

fn set_password(path: PathBuf) -> Result<(), String> {
    use std::io::IsTerminal;
    let cfg = config::Config::load(&path)?;
    let db = open_db(&cfg)?;
    let pw = if std::io::stdin().is_terminal() {
        let pw = rpassword::prompt_password("New password (10+ characters): ").map_err(|e| e.to_string())?;
        let again = rpassword::prompt_password("Again: ").map_err(|e| e.to_string())?;
        if pw != again {
            return Err("the passwords didn't match".into());
        }
        pw
    } else {
        // Piped in (installers, scripts): one line from stdin.
        let mut line = String::new();
        std::io::stdin().read_line(&mut line).map_err(|e| e.to_string())?;
        // Some shells (Windows PowerShell) prepend a byte-order mark when piping.
        line.trim_start_matches('\u{feff}').trim_end_matches(['\r', '\n']).to_string()
    };
    web::auth::set_password(&db, &pw)?;
    println!("Password saved. Existing sessions were signed out.");
    Ok(())
}

fn run(path: PathBuf, demo: bool) -> Result<(), String> {
    let mut cfg = config::Config::load(&path)?;
    if demo {
        // The demo has no password, so it never listens beyond this machine.
        let port = cfg.server.bind.rsplit(':').next().unwrap_or("8770").to_string();
        cfg.server.bind = format!("127.0.0.1:{port}");
    }
    let db = open_db(&cfg)?;
    let secrets = if demo { secrets::Secrets::open_file(&cfg.server.data_dir)? } else { secrets::Secrets::open(&cfg.server.data_dir)? };
    if demo {
        demo::backfill(&db, &cfg);
    }
    // One thread for everything; pings borrow a few short-lived blocking threads.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(8)
        .thread_keep_alive(std::time::Duration::from_secs(5))
        .build()
        .map_err(|e| e.to_string())?;
    rt.block_on(async move {
        let app = Arc::new(state::App::new(cfg, path.clone(), db, secrets, demo));
        info!("umbra {} starting, config {}", env!("CARGO_PKG_VERSION"), path.display());
        if demo {
            info!("demo mode: simulated devices, nothing leaves this machine, open http://{}", app.cfg().server.bind);
        }
        info!("secrets: {}", app.secrets.description);
        if !demo && !web::auth::has_password(&app) {
            warn!("no password set yet: run `umbra set-password --config {}` as the service user", path.display());
        }
        integrations::load_state(&app);
        scheduler::restart(&app);
        tokio::spawn(scheduler::flush_loop(app.clone()));
        tokio::spawn(scheduler::maintenance_loop(app.clone()));
        tokio::spawn(host::power_loop(app.clone()));
        tokio::spawn(alerts::sender_loop(app.clone()));
        tokio::spawn(stats::sampler_loop(app.clone()));
        tokio::spawn(integrations::poll_loop(app.clone(), "ring"));
        tokio::spawn(integrations::poll_loop(app.clone(), "blink"));
        tokio::spawn(live::reaper_loop(app.clone()));
        let r = web::serve(app.clone()).await;
        app.flush_checks();
        r
    })
}
