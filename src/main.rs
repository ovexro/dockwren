mod adopt;
mod alerts;
mod apps;
mod assets;
mod audit;
mod auth;
mod backup;
mod backupwatch;
mod cli;
mod clone;
mod config;
mod configbundle;
mod containers;
mod deliver;
mod direct;
mod diskusage;
mod docker;
mod doctor;
mod drift;
mod hostupdates;
mod install;
mod limits;
mod listeners;
mod logcap;
mod metrics;
mod needs;
mod notify;
mod offsite;
mod orphans;
mod osupdate;
mod pages;
mod power;
mod public;
mod registry;
mod release;
mod restoretest;
mod routes;
mod runner;
mod schedule;
mod selfupdate;
mod signin;
mod smart;
mod stack;
mod stackhistory;
mod statusapi;
mod template;
mod terminal;
mod thirdparty;
mod totp;
mod update;
mod updatepolicy;
mod ups;
mod upssetup;
mod upstream;
mod vulnscan;
mod watchdog;
mod webcheck;

use std::sync::Arc;

use askama::Template;
use auth::SharedAuth;
use axum::{
    Router,
    extract::{FromRef, Path, Query, State},
    http::{HeaderMap, HeaderValue, Request, StatusCode, header},
    middleware::{self, Next, from_fn_with_state},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use docker::{Action, ContainerInfo, DockerClient};

use assets::*;
use cli::*;
use containers::*;
use pages::*;
use routes::*;

#[derive(Clone)]
struct AppState {
    auth: SharedAuth,
    docker: DockerClient,
    apps: apps::Apps,
    metrics: metrics::Shared,
    notify: notify::Shared,
    terminal: Arc<terminal::Switch>,
}

impl FromRef<AppState> for notify::Shared {
    fn from_ref(state: &AppState) -> Self {
        state.notify.clone()
    }
}

impl FromRef<AppState> for metrics::Shared {
    fn from_ref(state: &AppState) -> Self {
        state.metrics.clone()
    }
}

impl FromRef<AppState> for apps::Apps {
    fn from_ref(state: &AppState) -> Self {
        state.apps.clone()
    }
}

impl FromRef<AppState> for SharedAuth {
    fn from_ref(state: &AppState) -> Self {
        state.auth.clone()
    }
}

#[tokio::main]
async fn main() {
    let cfg = config::Config::from_env();
    let cmd = std::env::args().nth(1);
    match cmd.as_deref() {
        Some("--version" | "-V" | "version") => {
            println!("dockwren {}", env!("CARGO_PKG_VERSION"));
            return;
        }
        Some("--help" | "-h" | "help") => {
            println!("{USAGE}");
            return;
        }
        None
        | Some("serve")
        | Some("set-password")
        | Some("disable-2fa")
        | Some("status-token")
        | Some("setup-osupdate")
        | Some("setup-smart")
        | Some("setup-ups")
        | Some("smart-collect")
        | Some("self-update") => {}
        Some(
            "install" | "export" | "import" | "recover" | "backup" | "update" | "restore"
            | "verify" | "restore-test" | "scan" | "template" | "cleanup" | "expose" | "unexpose"
            | "remove",
        ) => check_data_dir(&cfg.data_dir),
        Some(_) => {}
    }
    match cmd.as_deref() {
        None | Some("serve") => {}
        Some("self-update") => {
            let on_start = std::env::args().nth(2).as_deref() == Some("--on-start");
            if let Err(e) = selfupdate::cli(&cfg.data_dir, on_start).await {
                eprintln!("self-update failed: {e}");
                std::process::exit(1);
            }
            return;
        }
        Some(c @ ("setup-smart" | "smart-collect")) => {
            let args: Vec<String> = std::env::args().skip(2).collect();
            let dir = match args.as_slice() {
                [] => cfg.data_dir.clone(),
                [flag, d] if flag == "--data-dir" => std::path::PathBuf::from(d),
                _ => {
                    eprintln!("usage: dockwren {c} [--data-dir DIR]");
                    std::process::exit(2);
                }
            };
            let res = if c == "setup-smart" {
                smart::setup(&dir)
            } else {
                smart::collect(&dir)
            };
            match res {
                Ok(n) => println!("SMART: {n} disk(s) read into {}/smart.json.", dir.display()),
                Err(e) => {
                    eprintln!("{c} failed: {e}");
                    std::process::exit(1);
                }
            }
            return;
        }
        Some("setup-ups") => {
            match upssetup::setup() {
                Ok(lines) => {
                    for l in lines {
                        println!("{l}");
                    }
                }
                Err(e) => {
                    eprintln!("setup-ups failed: {e}");
                    std::process::exit(1);
                }
            }
            return;
        }
        Some("setup-osupdate") => {
            let args: Vec<String> = std::env::args().skip(2).collect();
            let dir = match args.as_slice() {
                [] => cfg.data_dir.clone(),
                [flag, d] if flag == "--data-dir" => std::path::PathBuf::from(d),
                _ => {
                    eprintln!("usage: dockwren setup-osupdate [--data-dir DIR]");
                    std::process::exit(2);
                }
            };
            match osupdate::setup(&dir) {
                Ok(()) => println!(
                    "OS-update helper installed for {}. Turn it on in the panel: Settings, OS updates.",
                    dir.display()
                ),
                Err(e) => {
                    eprintln!("setup-osupdate failed: {e}");
                    std::process::exit(1);
                }
            }
            return;
        }
        Some("disable-2fa") => {
            match auth::disable_two_factor(&cfg.data_dir) {
                Ok(true) => println!("Two-factor sign-in is off. Set it up again in Settings."),
                Ok(false) => println!("Two-factor sign-in was not on."),
                Err(e) => {
                    eprintln!("cannot turn it off in {}: {e}", cfg.data_dir.display());
                    std::process::exit(1);
                }
            }
            return;
        }
        Some("status-token") => {
            let revoke = std::env::args().nth(2).as_deref() == Some("--revoke");
            if revoke {
                match statusapi::revoke(&cfg.data_dir) {
                    Ok(true) => println!("Status token revoked."),
                    Ok(false) => println!("There was no status token."),
                    Err(e) => {
                        eprintln!("cannot revoke: {e}");
                        std::process::exit(1);
                    }
                }
                return;
            }
            let token = statusapi::create(&cfg.data_dir).unwrap_or_else(|e| {
                panic!("cannot write the token in {}: {e}", cfg.data_dir.display())
            });
            println!("New read-only status token (shown once, only its hash is stored):");
            println!("    {token}");
            println!(
                "Use it as: curl -H 'Authorization: Bearer <token>' https://<panel>/api/status"
            );
            println!("It replaces any earlier token. Revoke with: dockwren status-token --revoke");
            return;
        }
        Some("set-password") => {
            let password = auth::set_password(&cfg.data_dir).unwrap_or_else(|e| {
                panic!("cannot set password in {}: {e}", cfg.data_dir.display())
            });
            println!("New admin password (shown once, not stored):");
            println!("    {password}");
            println!("A running server applies it immediately and signs all sessions out.");
            return;
        }
        Some(c @ ("export" | "import")) => {
            let args: Vec<String> = std::env::args().skip(2).collect();
            if let Err(e) = cli_config(&cfg, c == "export", &args).await {
                eprintln!("{c} failed: {e}");
                std::process::exit(1);
            }
            return;
        }
        Some("recover") => {
            if let Err(e) = cli_recover(&cfg).await {
                eprintln!("recover failed: {e}");
                std::process::exit(1);
            }
            return;
        }
        Some("install") => {
            let args: Vec<String> = std::env::args().skip(2).collect();
            if let Err(e) = cli_install(&cfg, &args).await {
                eprintln!("install failed: {e}");
                std::process::exit(1);
            }
            return;
        }
        Some("backup") => {
            let args: Vec<String> = std::env::args().skip(2).collect();
            let (pull, name) = match args.as_slice() {
                [n] => (false, n.clone()),
                [p, n] if p == "pull" => (true, n.clone()),
                _ => (false, String::new()),
            };
            if !runner::valid_name(&name) {
                eprintln!("usage: dockwren backup <instance> | dockwren backup pull <instance>");
                std::process::exit(2);
            }
            let off = offsite::Offsite::open(&cfg.data_dir);
            let backups = cfg.data_dir.join("backups");
            if pull {
                match off.pull(&backups, &name, &mut |s| println!("  {s}")).await {
                    Ok(added) if added.is_empty() => {
                        println!("nothing new: every off-host snapshot is already here")
                    }
                    Ok(added) => println!(
                        "{} snapshot(s) added; install the app if this server does not have it yet, then: dockwren restore {name}",
                        added.len()
                    ),
                    Err(e) => {
                        eprintln!("pull failed: {e}");
                        std::process::exit(1);
                    }
                }
                return;
            }
            let result = async {
                let templates = apps::catalog(&cfg)?;
                let dir =
                    backup::snapshot_instance(&cfg, &templates, &name, &mut |s| println!("  {s}"))
                        .await?;
                off.push(&backups, &name, backup::KEEP, &mut |s| println!("  {s}"))
                    .await
                    .map_err(|e| {
                        format!(
                            "local snapshot saved at {}, off-host upload failed: {e}",
                            dir.display()
                        )
                    })?;
                Ok::<_, String>(dir)
            }
            .await;
            match result {
                Ok(dir) => println!("backup written to {}", dir.display()),
                Err(e) => {
                    eprintln!("backup failed: {e}");
                    std::process::exit(1);
                }
            }
            return;
        }
        Some("update") => {
            let name = std::env::args().nth(2).unwrap_or_default();
            if !runner::valid_name(&name) {
                eprintln!("usage: dockwren update <instance>");
                std::process::exit(2);
            }
            let result = async {
                let templates = apps::catalog(&cfg)?;
                update::update_instance(
                    &cfg.instances_dir(),
                    &cfg.data_dir.join("backups"),
                    &templates,
                    &name,
                    &mut |s| println!("  {s}"),
                )
                .await
            }
            .await;
            match result {
                Ok(update::Outcome::UpToDate) => println!("{name} is already up to date"),
                Ok(update::Outcome::Updated { from, to }) => {
                    println!("{name} updated from template version {from} to {to}, up and healthy")
                }
                Err(e) => {
                    eprintln!("{e}");
                    std::process::exit(1);
                }
            }
            return;
        }
        Some("expose") => {
            let args: Vec<String> = std::env::args().skip(2).collect();
            if let Err(e) = cli_expose(&cfg, &args).await {
                eprintln!("expose failed: {e}");
                std::process::exit(1);
            }
            return;
        }
        Some("unexpose") => {
            let args: Vec<String> = std::env::args().skip(2).collect();
            if let Err(e) = cli_unexpose(&cfg, &args).await {
                eprintln!("unexpose failed: {e}");
                std::process::exit(1);
            }
            return;
        }
        Some("restore") => {
            let args: Vec<String> = std::env::args().skip(2).collect();
            let safety = !args.iter().any(|a| a == "--no-safety-snapshot");
            let list = args.iter().any(|a| a == "--list");
            let pos: Vec<&String> = args.iter().filter(|a| !a.starts_with("--")).collect();
            let name = pos.first().map(|s| s.as_str()).unwrap_or_default();
            if list {
                if !runner::valid_name(name) || pos.len() != 1 || args.len() != 2 {
                    eprintln!("usage: dockwren restore <instance> --list");
                    std::process::exit(2);
                }
                let snaps = backup::list(&cfg.data_dir.join("backups"), name);
                if snaps.is_empty() {
                    println!("no snapshots of {name}");
                }
                for p in snaps {
                    let id = p.file_name().and_then(|n| n.to_str()).unwrap_or_default();
                    let when = std::process::Command::new("date")
                        .args(["-d", &format!("@{id}"), "+%F %T"])
                        .output()
                        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                        .unwrap_or_default();
                    println!("{id}  {when}");
                }
                return;
            }
            if !runner::valid_name(name)
                || pos.len() > 2
                || args.len() > pos.len() + usize::from(!safety)
            {
                eprintln!("usage: dockwren restore <instance> [snapshot] [--no-safety-snapshot]");
                std::process::exit(2);
            }
            let result = async {
                let templates = apps::catalog(&cfg)?;
                backup::restore_instance(
                    &cfg,
                    &templates,
                    name,
                    pos.get(1).map(|s| s.as_str()),
                    safety,
                    &mut |s| println!("  {s}"),
                )
                .await
            }
            .await;
            match result {
                Ok(snap) => println!("restored from {}, up and healthy", snap.display()),
                Err(e) => {
                    eprintln!("restore failed: {e}");
                    std::process::exit(1);
                }
            }
            return;
        }
        Some("verify") => {
            let args: Vec<String> = std::env::args().skip(2).collect();
            let name = args.first().map(|s| s.as_str()).unwrap_or_default();
            if !runner::valid_name(name) || args.len() > 2 {
                eprintln!("usage: dockwren verify <instance> [snapshot]");
                std::process::exit(2);
            }
            let result = async {
                let templates = apps::catalog(&cfg)?;
                backup::verify_instance(
                    &cfg,
                    &templates,
                    name,
                    args.get(1).map(|s| s.as_str()),
                    &mut |s| println!("  {s}"),
                )
                .await
            }
            .await;
            match result {
                Ok(snap) => println!("{} verified: it can be restored", snap.display()),
                Err(e) => {
                    eprintln!("verify failed: {e}");
                    std::process::exit(1);
                }
            }
            return;
        }
        Some("doctor") => {
            let items = doctor::run(&cfg.data_dir).await;
            print!("{}", doctor::render_text(&items));
            if doctor::worst(&items) == doctor::Level::Fail {
                std::process::exit(1);
            }
            return;
        }
        Some("scan") => {
            let name = std::env::args().nth(2).unwrap_or_default();
            if !runner::valid_name(&name) || std::env::args().count() != 3 {
                eprintln!("usage: dockwren scan <instance>");
                std::process::exit(2);
            }
            let inst = runner::Instance {
                dir: cfg.instances_dir().join(&name),
                name: name.clone(),
            };
            if !inst.dir.join("state.toml").is_file() {
                eprintln!("no instance named {name}");
                std::process::exit(1);
            }
            match vulnscan::scan_app(&cfg.data_dir, &inst, &mut |s| println!("  {s}")).await {
                Some(f) => println!("{name}: {}", vulnscan::describe(&f)),
                None => {
                    eprintln!("{name}: no image could be scanned");
                    std::process::exit(1);
                }
            }
            return;
        }
        Some("restore-test") => {
            let name = std::env::args().nth(2).unwrap_or_default();
            if !runner::valid_name(&name) || std::env::args().count() != 3 {
                eprintln!("usage: dockwren restore-test <instance>");
                std::process::exit(2);
            }
            let result = async {
                let templates = apps::catalog(&cfg)?;
                let dir = cfg.instances_dir().join(&name);
                let state = std::fs::read_to_string(dir.join("state.toml"))
                    .map_err(|_| format!("no instance named {name}"))?;
                let tpl = toml::from_str::<toml::Table>(&state)
                    .ok()
                    .and_then(|t| t.get("template")?.as_str().map(String::from))
                    .ok_or("state.toml: no template")?;
                let t = templates
                    .iter()
                    .find(|t| t.manifest.app.id == tpl)
                    .ok_or_else(|| format!("template {tpl} is not available"))?;
                restoretest::run(
                    &cfg.data_dir,
                    &runner::Instance {
                        name: name.clone(),
                        dir,
                    },
                    t,
                    &mut |s| println!("  {s}"),
                )
                .await
            }
            .await;
            match result {
                Ok(Some(s)) => println!("restore test passed (snapshot {s}): it came back healthy"),
                Ok(None) => println!("restore test skipped"),
                Err(e) => {
                    eprintln!("restore test failed: {e}");
                    std::process::exit(1);
                }
            }
            return;
        }
        Some("template") => {
            let args: Vec<String> = std::env::args().skip(2).collect();
            let usage = "usage: dockwren template add <https-url> <full-commit-hash> [--yes] | template list | template remove <id>";
            match args
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .as_slice()
            {
                ["list"] => {
                    for s in thirdparty::list(&cfg.data_dir) {
                        println!("{}  {} @ {}", s.id, s.url, s.sha);
                    }
                }
                ["remove", id] => match thirdparty::remove(&cfg.data_dir, id) {
                    Ok(()) => {
                        println!("removed {id}; restart dockwren to drop it from the catalog")
                    }
                    Err(e) => {
                        eprintln!("{e}");
                        std::process::exit(1);
                    }
                },
                ["add", url, sha, rest @ ..] if rest.is_empty() || rest == ["--yes"] => {
                    let fetched = match thirdparty::fetch(&cfg.data_dir, url, sha).await {
                        Ok(f) => f,
                        Err(e) => {
                            eprintln!("cannot fetch: {e}");
                            std::process::exit(1);
                        }
                    };
                    let t = match thirdparty::review(&fetched, None) {
                        Ok(t) => t,
                        Err(e) => {
                            eprintln!("refused: {e}");
                            std::process::exit(1);
                        }
                    };
                    println!(
                        "== {} ({}) version {}\n{}\n\n== compose.yaml\n{}",
                        t.manifest.app.name,
                        t.manifest.app.id,
                        t.manifest.app.version,
                        t.manifest.app.description,
                        fetched.compose
                    );
                    if rest != ["--yes"] {
                        print!("Add this template? [y/N] ");
                        let _ = std::io::Write::flush(&mut std::io::stdout());
                        let mut line = String::new();
                        let _ = std::io::stdin().read_line(&mut line);
                        if !line.trim().eq_ignore_ascii_case("y") {
                            println!("not added");
                            return;
                        }
                    }
                    match thirdparty::add(&cfg.data_dir, url, sha, &fetched) {
                        Ok(s) => println!("added {}; restart dockwren to load it", s.id),
                        Err(e) => {
                            eprintln!("cannot add: {e}");
                            std::process::exit(1);
                        }
                    }
                }
                _ => {
                    eprintln!("{usage}");
                    std::process::exit(2);
                }
            }
            return;
        }
        Some("cleanup") => {
            let name = std::env::args().nth(2).unwrap_or_default();
            if !runner::valid_name(&name) || std::env::args().count() != 3 {
                eprintln!(
                    "usage: dockwren cleanup <instance>   (removes *.pre-restore-* / *.failed-restore-* leftovers)"
                );
                std::process::exit(2);
            }
            match backup::cleanup_instance(&cfg, &name, &mut |s| println!("  {s}")).await {
                Ok(n) if n.is_empty() => println!("nothing to clean up"),
                Ok(n) => println!(
                    "removed {} leftover director{}",
                    n.len(),
                    if n.len() == 1 { "y" } else { "ies" }
                ),
                Err(e) => {
                    eprintln!("cleanup failed: {e}");
                    std::process::exit(1);
                }
            }
            return;
        }
        Some("remove") => {
            let name = std::env::args().nth(2).unwrap_or_default();
            if !runner::valid_name(&name) {
                eprintln!("usage: dockwren remove <instance>");
                std::process::exit(2);
            }
            let inst = runner::Instance {
                dir: cfg.instances_dir().join(&name),
                name,
            };
            match runner::take_down(&inst).await {
                Ok(()) => println!(
                    "{} stopped and containers removed (data kept in {})",
                    inst.name,
                    inst.dir.display()
                ),
                Err(e) => {
                    eprintln!("remove failed: {e}");
                    std::process::exit(1);
                }
            }
            return;
        }
        Some(other) => {
            eprintln!("unknown command '{other}'\n{USAGE}");
            std::process::exit(2);
        }
    }
    let (_, first_run_password) = auth::load_or_create(&cfg.data_dir)
        .unwrap_or_else(|e| panic!("cannot prepare data dir {}: {e}", cfg.data_dir.display()));
    let auth = Arc::new(
        auth::Auth::open(&cfg.data_dir, cfg.secure_cookie).unwrap_or_else(|e| panic!("{e}")),
    );

    if let Some(password) = first_run_password {
        println!("First run: generated admin password (shown once, not stored):");
        println!("    {password}");
    }

    let docker =
        DockerClient::connect().unwrap_or_else(|e| panic!("cannot set up Docker client: {e}"));

    let listener = tokio::net::TcpListener::bind(&cfg.bind)
        .await
        .unwrap_or_else(|e| panic!("cannot bind {}: {e}", cfg.bind));
    println!("dockwren listening on http://{}", cfg.bind);
    watchdog::spawn(&cfg.bind);
    let templates = apps::catalog(&cfg).unwrap_or_else(|e| panic!("bad template: {e}"));
    audit::init(&cfg.data_dir);
    let notify = notify::shared(&cfg.data_dir);
    let apps = apps::Apps::new(templates, cfg.instances_dir())
        .with_notify(notify.clone())
        .with_auth(auth.clone())
        .with_schedule(Arc::new(schedule::Schedule::open(&cfg.data_dir)))
        .with_offsite(Arc::new(offsite::Offsite::open(&cfg.data_dir)));
    apps.spawn_update_checker();
    apps.spawn_backup_scheduler();
    apps.spawn_update_scheduler();
    apps.spawn_power_scheduler();
    release::spawn_checker(notify.clone());
    upstream::spawn_checker(apps.clone(), notify.clone());
    hostupdates::spawn_checker(notify.clone());
    webcheck::spawn_checker(apps.clone(), notify.clone());
    {
        // Direct HTTPS: make sure the Caddy container matches the instances (also after a restore).
        let (root, data) = (
            apps.root.clone(),
            apps.root.parent().map(|p| p.to_path_buf()),
        );
        tokio::spawn(async move {
            if let Some(data) = data
                && let Err(e) = direct::reconcile(&data, &root).await
            {
                eprintln!("direct HTTPS: {e}");
            }
        });
    }
    vulnscan::spawn_checker(apps.clone(), notify.clone(), cfg.data_dir.clone());
    backupwatch::spawn_checker(notify.clone());
    smart::spawn_checker(notify.clone(), cfg.data_dir.clone());
    ups::spawn_checker(notify.clone());
    signin::init(notify.clone(), &cfg.data_dir);
    let os_control = Arc::new(osupdate::Control::new(Some(&cfg.data_dir)));
    osupdate::spawn_watcher(os_control.clone(), notify.clone());
    deliver::spawn(notify.clone());
    let metrics = metrics::Shared::default();
    let mut disk_paths = vec![cfg.data_dir.display().to_string()];
    disk_paths.extend(cfg.disk_paths.iter().cloned());
    let inode_paths = disk_paths.clone();
    metrics::spawn(
        metrics.clone(),
        docker.clone(),
        disk_paths,
        cfg.data_dir.clone(),
    );
    alerts::spawn(notify.clone(), metrics.clone(), docker.clone(), inode_paths);
    axum::serve(
        listener,
        app(
            auth,
            docker,
            apps,
            metrics,
            notify,
            Arc::new(terminal::Switch::new(cfg.terminal, Some(&cfg.data_dir))),
            os_control,
            Arc::new(selfupdate::Control::new(&cfg.data_dir)),
        )
        .into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .await
    .expect("server error");
}

#[cfg(test)]
mod tests;
