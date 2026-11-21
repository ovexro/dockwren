//! Command-line entry points other than `serve`.

use super::*;

pub(super) async fn cli_expose(cfg: &config::Config, args: &[String]) -> Result<(), String> {
    let apply = args.iter().any(|a| a == "--yes");
    let replace_dns = args.iter().any(|a| a == "--replace-dns");
    let usage = "usage: dockwren expose <instance> [hostname] [--access EMAIL[,EMAIL...]] [--replace-dns] [--yes]";
    let mut access: Vec<String> = Vec::new();
    let mut pos: Vec<&String> = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--access" {
            let v = it.next().ok_or(usage)?;
            access = v.split(',').map(str::to_string).collect();
            if let Some(bad) = access.iter().find(|e| !public::valid_email(e)) {
                return Err(format!("not an e-mail address: {bad}"));
            }
        } else if !a.starts_with("--") {
            pos.push(a);
        }
    }
    let name = pos.first().ok_or(usage)?;
    if !runner::valid_name(name) {
        return Err("invalid instance name".into());
    }
    let dir = cfg.instances_dir().join(name.as_str());
    if !dir.join("state.toml").is_file() {
        return Err(format!("no instance named {name}"));
    }
    let host = match pos.get(1) {
        Some(h) => {
            public::write(&dir, h)?;
            h.to_string()
        }
        None => public::read(&dir).ok_or("no hostname set: pass one, or set it in the web UI")?,
    };
    let port = public::web_port(&dir).ok_or("this app has no WEB_PORT to publish")?;
    let settings = public::Settings::from_env().ok_or(
        "Cloudflare is not connected: set CLOUDFLARE_API_TOKEN, CLOUDFLARE_ACCOUNT_ID and DOCKWREN_TUNNEL_ID, or paste a token in the panel's Cloudflare tab (and run this with the same DOCKWREN_DATA)",
    )?;
    public::expose(
        &settings,
        &host,
        &public::service_url(port),
        &access,
        apply,
        replace_dns,
        &mut |s| println!("  {s}"),
    )
    .await
}

/// `dockwren unexpose <instance> [--yes]`: remove the tunnel route and DNS record, then forget the hostname.
pub(super) async fn cli_unexpose(cfg: &config::Config, args: &[String]) -> Result<(), String> {
    let apply = args.iter().any(|a| a == "--yes");
    let name = args
        .iter()
        .find(|a| !a.starts_with("--"))
        .ok_or("usage: dockwren unexpose <instance> [--yes]")?;
    if !runner::valid_name(name) {
        return Err("invalid instance name".into());
    }
    let dir = cfg.instances_dir().join(name.as_str());
    if !dir.join("state.toml").is_file() {
        return Err(format!("no instance named {name}"));
    }
    let host = public::read(&dir).ok_or("this instance has no public hostname")?;
    let settings = public::Settings::from_env().ok_or(
        "Cloudflare is not connected: set CLOUDFLARE_API_TOKEN, CLOUDFLARE_ACCOUNT_ID and DOCKWREN_TUNNEL_ID, or paste a token in the panel's Cloudflare tab (and run this with the same DOCKWREN_DATA)",
    )?;
    public::unexpose(&settings, &host, apply, &mut |s| println!("  {s}")).await?;
    if apply {
        public::write(&dir, "")?;
    }
    Ok(())
}

/// Passphrase from `DOCKWREN_PASSPHRASE`, else the first line of stdin.
pub(super) fn read_passphrase() -> Result<String, String> {
    if let Ok(p) = std::env::var("DOCKWREN_PASSPHRASE") {
        return Ok(p);
    }
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .map_err(|e| format!("passphrase: {e}"))?;
    Ok(line.trim_end_matches(['\n', '\r']).to_string())
}

/// `dockwren export FILE` / `dockwren import FILE`: encrypted copy of every app's config.
pub(super) async fn cli_config(
    cfg: &config::Config,
    export: bool,
    args: &[String],
) -> Result<(), String> {
    let [file] = args else {
        return Err(format!(
            "usage: dockwren {} FILE   (passphrase from DOCKWREN_PASSPHRASE or the first line of stdin)",
            if export { "export" } else { "import" }
        ));
    };
    let file = std::path::Path::new(file);
    let pass = read_passphrase()?;
    let root = cfg.instances_dir();
    if export {
        let n = configbundle::export(&root, file, &pass)?;
        println!(
            "{n} app(s) exported to {} (encrypted; it holds your apps' passwords, keep it safe).",
            file.display()
        );
        return Ok(());
    }
    let r = configbundle::import(&root, file, &pass, false)?;
    for n in &r.skipped {
        println!("{n}: already exists here, skipped");
    }
    print_panel(&r);
    for n in &r.created {
        println!("{n}: recreated, starting (empty, data comes from a snapshot)");
        let inst = runner::Instance {
            name: n.clone(),
            dir: root.join(n),
        };
        if let Err(e) = runner::bring_up(&inst, &mut |s| println!("  {s}")).await {
            println!("  {n}: could not start: {e}");
        }
        if let Some(h) = r.hosts.get(n) {
            println!("  was public at {h}: publish again with `dockwren expose {n} {h}`");
        }
        println!(
            "  then restore data: dockwren backup pull {n} (if off-host backups are set up), dockwren restore {n}"
        );
    }
    Ok(())
}

pub(super) fn print_panel(r: &configbundle::Imported) {
    if !r.panel_restored.is_empty() {
        println!(
            "panel settings restored: {} (sign in with your old password and 2FA; restart the panel to apply)",
            r.panel_restored.join(", ")
        );
    }
    if !r.panel_kept.is_empty() {
        println!(
            "panel settings kept as they are on this server: {}",
            r.panel_kept.join(", ")
        );
    }
}

/// One answer for `recover`: env `DOCKWREN_OFFSITE_<KEY>`, else a prompt (hidden for secrets).
pub(super) fn ask(key: &str, label: &str, secret: bool) -> Result<String, String> {
    if let Ok(v) = std::env::var(format!("DOCKWREN_OFFSITE_{key}")) {
        return Ok(v);
    }
    use std::io::{IsTerminal, Write};
    let tty = std::io::stdin().is_terminal();
    eprint!("{label}: ");
    let _ = std::io::stderr().flush();
    let hide = secret
        && tty
        && std::process::Command::new("stty")
            .arg("-echo")
            .status()
            .is_ok_and(|s| s.success());
    let mut line = String::new();
    let r = std::io::stdin().read_line(&mut line);
    if hide {
        let _ = std::process::Command::new("stty").arg("echo").status();
        eprintln!();
    }
    r.map_err(|e| format!("{label}: {e}"))?;
    Ok(line.trim().to_string())
}

/// Off-host target for `recover`: the saved one, or asked for (kind and fields).
pub(super) fn recover_target(off: &offsite::Offsite) -> Result<(), String> {
    if off.get().enabled() {
        println!("using the off-host target already saved on this server");
        return Ok(());
    }
    let mut f = offsite::Form {
        kind: ask("KIND", "Off-host target type (sftp or s3)", false)?.to_lowercase(),
        ..Default::default()
    };
    match f.kind.as_str() {
        "s3" => {
            f.endpoint = ask("ENDPOINT", "S3 endpoint URL (https://...)", false)?;
            f.bucket = ask("BUCKET", "Bucket", false)?;
            f.access_key = ask("ACCESS_KEY", "Access key", true)?;
            f.secret_key = ask("SECRET_KEY", "Secret key", true)?;
        }
        "sftp" => {
            f.host = ask("HOST", "SFTP host", false)?;
            f.port = ask("PORT", "SFTP port (empty = 22)", false)?;
            f.user = ask("USER", "SFTP user", false)?;
            f.path = ask("PATH", "Folder on the server", false)?;
            let file = ask(
                "PRIVATE_KEY_FILE",
                "Path of the SSH private key file",
                false,
            )?;
            f.private_key = std::fs::read_to_string(&file).map_err(|e| format!("{file}: {e}"))?;
        }
        _ => return Err("the target type must be sftp or s3".into()),
    }
    f.password = ask(
        "PASSWORD",
        "Repository password (from your password manager)",
        true,
    )?;
    if f.password.is_empty() {
        return Err("the repository password is needed".into());
    }
    let (c, _) = offsite::Config::from_form(&f, &offsite::Config::default())?;
    off.set(c)
}

/// `dockwren recover`: rebuild a lost server from the off-host target alone.
pub(super) async fn cli_recover(cfg: &config::Config) -> Result<(), String> {
    let off = offsite::Offsite::open(&cfg.data_dir);
    recover_target(&off)?;
    println!("downloading the config export...");
    let bytes = off.pull_config().await?;
    let root = cfg.instances_dir();
    let r = configbundle::import_bytes(&root, &bytes, &off.get().password, true)?;
    for n in &r.skipped {
        println!("{n}: already exists here, left alone");
    }
    print_panel(&r);
    let backups = cfg.data_dir.join("backups");
    let templates = apps::catalog(cfg)?;
    let mut failed = Vec::new();
    let mut empty: Vec<String> = Vec::new();
    for n in &r.created {
        println!("== {n}");
        let inst = runner::Instance {
            name: n.clone(),
            dir: root.join(n),
        };
        let step = async {
            runner::bring_up(&inst, &mut |s| println!("  {s}"))
                .await
                .map_err(|e| format!("could not start: {e}"))?;
            match off.pull(&backups, n, &mut |s| println!("  {s}")).await {
                Ok(_) => {}
                Err(e) if !backup::list(&backups, n).is_empty() => {
                    println!("  pull: {e} (using the snapshots already here)")
                }
                Err(e) if e.contains("no snapshot found") => {
                    println!("  no backup of {n} exists off-host: it stays empty (a fresh app)");
                    empty.push(n.clone());
                    return Ok(());
                }
                Err(e) => return Err(format!("no data to restore: {e}")),
            }
            backup::restore_instance(cfg, &templates, n, None, false, &mut |s| println!("  {s}"))
                .await
                .map(|snap| println!("  restored from {}", snap.display()))
        };
        if let Err(e) = step.await {
            println!("  {n}: {e}");
            failed.push(n.clone());
        } else if let Some(h) = r.hosts.get(n) {
            if empty.contains(n) {
                // A fresh app without its data must not be opened to the internet by itself.
                println!(
                    "  was public at {h}: publish again with `dockwren expose {n} {h} --yes` once it is set up"
                );
            } else {
                public::publish_after_install(&root.join(n), n, h, &mut |s| println!("  {s}"))
                    .await;
            }
        }
    }
    println!(
        "recovered {} of {} app(s); {} started empty (no backup existed)",
        r.created.len() - failed.len() - empty.len(),
        r.created.len(),
        empty.len()
    );
    if failed.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "not recovered: {} (the others are done; retry those with `dockwren backup pull <app>` and `dockwren restore <app>`)",
            failed.join(", ")
        ))
    }
}

/// `dockwren install <app> [KEY=VALUE...]`: resolve, prepare, bring up.
pub(super) async fn cli_install(cfg: &config::Config, args: &[String]) -> Result<(), String> {
    let app = args
        .first()
        .ok_or("usage: dockwren install <app> [KEY=VALUE...]")?;
    let templates = apps::catalog(cfg)?;
    let t = templates
        .iter()
        .find(|t| &t.manifest.app.id == app)
        .ok_or_else(|| format!("unknown app {app}"))?;
    let input = args[1..]
        .iter()
        .map(|a| {
            a.split_once('=')
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .ok_or(format!("expected KEY=VALUE, got {a}"))
        })
        .collect::<Result<std::collections::HashMap<_, _>, _>>()?;
    let root = cfg.instances_dir();
    let claimed = runner::claimed_ports(&root);
    let resolved = install::resolve(t, &input, &claimed, &install::port_is_free)?;
    install::check_lan_ports(t)?;
    let inst = runner::prepare(&root, t, &resolved.env)?;
    println!("instance {} prepared in {}", inst.name, inst.dir.display());
    runner::bring_up(&inst, &mut |step| println!("  {step}"))
        .await
        .map_err(|e| {
            format!(
                "{e}\n(instance kept in {}; `dockwren remove {}` stops it)",
                inst.dir.display(),
                inst.name
            )
        })?;
    let captured =
        runner::run_post_start(&inst, t, &resolved.env, &mut |step| println!("  {step}")).await?;
    if let Some(host) = &resolved.public_host {
        public::publish_after_install(&inst.dir, &inst.name, host, &mut |s| println!("  {s}"))
            .await;
    }
    for key in ["WEB_PORT", "SSH_PORT"] {
        if let Some(p) = resolved.env.get(key) {
            println!("  {key}: 127.0.0.1:{p}");
        }
    }
    for (k, v) in captured.iter().chain(&resolved.revealed) {
        println!("  {k} (shown once): {v}");
    }
    println!("up and healthy");
    Ok(())
}

pub(super) const USAGE: &str = "usage: dockwren [serve | set-password | disable-2fa | status-token [--revoke] | install <app> [KEY=VALUE...] | export FILE | import FILE | recover | backup <instance> | backup pull <instance> | update <instance> | restore <instance> [snapshot | --list] | verify <instance> [snapshot] | restore-test <instance> | scan <instance> | doctor | template add <https-url> <commit> [--yes] | template list | template remove <id> | cleanup <instance> | expose <instance> [hostname] [--access EMAIL] [--yes] | unexpose <instance> [--yes] | remove <instance> | setup-osupdate [--data-dir DIR] | setup-smart [--data-dir DIR] | setup-ups | self-update | --version]";

/// CLI commands work on the panel's data directory; explain the usual permission mistake
/// instead of failing later with a bare "Permission denied".
pub(super) fn check_data_dir(dir: &std::path::Path) {
    let probe = dir.join(".dockwren-probe");
    let ok = std::fs::create_dir_all(dir)
        .and_then(|_| std::fs::write(&probe, b""))
        .and_then(|_| std::fs::remove_file(&probe));
    if let Err(e) = ok {
        eprintln!(
            "cannot use the data directory {} ({e}).\nSet DOCKWREN_DATA to the panel's data directory and run as its owner, for example:\n  sudo -E -u dockwren env DOCKWREN_DATA=/var/lib/dockwren dockwren <command>",
            dir.display()
        );
        std::process::exit(1);
    }
}
