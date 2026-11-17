//! "Add a stack": run a Compose file the user pasted, as a panel-managed app.
//!
//! The pasted file is not trusted. It is never linted line by line: `docker compose config`
//! turns it (anchors, short syntax, variables and all) into the exact model Docker would run,
//! and that JSON is checked against an allow-list. Anything not listed is refused with the
//! reason, so a panel login cannot be turned into root on the host by pasting YAML.

use std::collections::BTreeMap;
use std::path::Path;

use serde_json::Value;

/// Pasted compose files above this size are refused.
pub const MAX_COMPOSE_BYTES: usize = 64 * 1024;

/// Words that make `docker compose config` read other files from the host. Refused anywhere in
/// the text (even in a comment): crude on purpose.
const FILE_READERS: &[&str] = &[
    "include:", "extends:", "env_file", "build:", "secrets:", "configs:",
];

const TOP_LEVEL: &[&str] = &["name", "services", "volumes", "networks"];

const SERVICE_KEYS: &[&str] = &[
    "image",
    "command",
    "entrypoint",
    "environment",
    "ports",
    "expose",
    "volumes",
    "tmpfs",
    "depends_on",
    "restart",
    "healthcheck",
    "networks",
    "labels",
    "working_dir",
    "user",
    "hostname",
    "domainname",
    "container_name",
    "stop_grace_period",
    "stop_signal",
    "init",
    "tty",
    "stdin_open",
    "read_only",
    "mem_limit",
    "mem_reservation",
    "memswap_limit",
    "cpus",
    "cpu_shares",
    "pids_limit",
    "shm_size",
    "ulimits",
    "dns",
    "dns_search",
    "extra_hosts",
    "logging",
    "cap_drop",
    "platform",
    "pull_policy",
    "profiles",
    "deploy",
];

/// Why a key is not on the list, for the ones people are most likely to paste.
fn why_refused(key: &str) -> &'static str {
    match key {
        "privileged"
        | "cap_add"
        | "security_opt"
        | "devices"
        | "device_cgroup_rules"
        | "sysctls" => "it gives the container more power over the host",
        "network_mode" | "pid" | "ipc" | "uts" | "userns_mode" | "cgroup" | "cgroup_parent" => {
            "it shares the host's networking or process space"
        }
        "volumes_from" | "runtime" | "gpus" | "group_add" => "it reaches outside the stack",
        _ => "it is not on the list of supported settings",
    }
}

/// Check the model printed by `docker compose config --format json`.
/// `project_dir` is the directory the config was resolved in (relative paths start there).
pub fn lint_model(model: &Value, project_dir: &Path) -> Result<(), String> {
    let top = model
        .as_object()
        .ok_or("the compose model is not an object")?;
    for k in top.keys() {
        if !TOP_LEVEL.contains(&k.as_str()) && !k.starts_with("x-") {
            return Err(format!("top-level \"{k}\" is not supported"));
        }
    }
    let services = top
        .get("services")
        .and_then(Value::as_object)
        .filter(|s| !s.is_empty())
        .ok_or("the file defines no services")?;
    let data_dir = format!("{}/data", project_dir.display());

    for (name, svc) in services {
        let svc = svc
            .as_object()
            .ok_or(format!("service {name}: not a mapping"))?;
        for (k, v) in svc {
            // Compose prints unset optional keys as null.
            if v.is_null() || k.starts_with("x-") {
                continue;
            }
            if !SERVICE_KEYS.contains(&k.as_str()) {
                return Err(format!(
                    "service {name}: \"{k}\" is not allowed ({})",
                    why_refused(k)
                ));
            }
        }
        if svc
            .get("image")
            .and_then(Value::as_str)
            .unwrap_or("")
            .is_empty()
        {
            return Err(format!(
                "service {name}: needs an image (build is not supported)"
            ));
        }
        for p in svc
            .get("ports")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let ip = p.get("host_ip").and_then(Value::as_str).unwrap_or("");
            if ip != "127.0.0.1" {
                let what = p.get("published").and_then(Value::as_str).unwrap_or("?");
                return Err(format!(
                    "service {name}: port {what} must be bound to 127.0.0.1 (write \"127.0.0.1:{what}:{}\"); reach it from outside through a public hostname",
                    p.get("target").map(|t| t.to_string()).unwrap_or_default()
                ));
            }
        }
        for v in svc
            .get("volumes")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let kind = v.get("type").and_then(Value::as_str).unwrap_or("");
            let source = v.get("source").and_then(Value::as_str).unwrap_or("");
            match kind {
                "volume" | "tmpfs" => {}
                "bind" => {
                    if !(source == data_dir || source.starts_with(&format!("{data_dir}/")))
                        || source.contains("..")
                    {
                        return Err(format!(
                            "service {name}: bind mount of {} is not allowed; keep data in a named volume or under ./data (for example ./data/db:/var/lib/db)",
                            display_source(source, project_dir)
                        ));
                    }
                }
                other => {
                    return Err(format!(
                        "service {name}: mount type {other:?} is not allowed"
                    ));
                }
            }
        }
        if let Some(d) = svc.get("deploy").and_then(Value::as_object) {
            for k in d.keys() {
                if !["resources", "replicas", "restart_policy"].contains(&k.as_str()) {
                    return Err(format!("service {name}: deploy.{k} is not supported"));
                }
            }
        }
    }

    for (name, vol) in top
        .get("volumes")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
    {
        let Some(vol) = vol.as_object() else { continue };
        for (k, v) in vol {
            let ok = match k.as_str() {
                "name" | "labels" => true,
                "driver" => v.as_str().is_none_or(|d| d == "local"),
                _ => v.is_null(),
            };
            if !ok {
                return Err(format!(
                    "volume {name}: \"{k}\" is not allowed (external volumes and driver options can reach other data on the host)"
                ));
            }
        }
    }
    for (name, net) in top
        .get("networks")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
    {
        let Some(net) = net.as_object() else { continue };
        for (k, v) in net {
            let ok = match k.as_str() {
                "name" | "labels" | "ipam" | "internal" | "enable_ipv6" => true,
                "driver" => v.as_str().is_none_or(|d| d == "bridge"),
                _ => v.is_null(),
            };
            if !ok {
                return Err(format!(
                    "network {name}: \"{k}\" is not allowed (only plain bridge networks inside this stack)"
                ));
            }
        }
    }
    Ok(())
}

fn display_source(source: &str, project_dir: &Path) -> String {
    source
        .strip_prefix(&format!("{}/", project_dir.display()))
        .map(|rel| format!("./{rel}"))
        .unwrap_or_else(|| source.to_string())
}

/// First host port a stack publishes (always on 127.0.0.1 after [`lint_model`]).
pub fn first_published_port(model: &Value) -> Option<u16> {
    let mut ports: Vec<u16> = model
        .get("services")?
        .as_object()?
        .values()
        .flat_map(|s| {
            s.get("ports")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
        })
        .filter_map(|p| p.get("published")?.as_str()?.parse().ok())
        .collect();
    ports.sort_unstable();
    ports.first().copied()
}

/// Cheap checks on the raw text before Docker reads it.
pub fn precheck(compose: &str) -> Result<(), String> {
    if compose.trim().is_empty() {
        return Err("paste a compose file".into());
    }
    if compose.len() > MAX_COMPOSE_BYTES {
        return Err("the compose file is larger than 64 KiB".into());
    }
    for w in FILE_READERS {
        if compose.contains(w) {
            return Err(format!(
                "\"{w}\" is not supported: it reads other files from the host. Paste one self-contained file with ready-made images"
            ));
        }
    }
    Ok(())
}

/// Parse the optional `.env` text (KEY=VALUE lines, # comments) into variables for Compose.
pub fn parse_env(text: &str) -> Result<BTreeMap<String, String>, String> {
    let mut out = BTreeMap::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (k, v) = line
            .split_once('=')
            .ok_or_else(|| format!("environment line {}: expected KEY=VALUE", n + 1))?;
        let k = k.trim();
        let ok = !k.is_empty()
            && !k.starts_with(|c: char| c.is_ascii_digit())
            && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        if !ok {
            return Err(format!(
                "environment line {}: bad variable name {k:?}",
                n + 1
            ));
        }
        let v = v.trim().trim_matches('"').trim_matches('\'');
        if v.chars().any(char::is_control) {
            return Err(format!(
                "environment line {}: control character in value",
                n + 1
            ));
        }
        if out.insert(k.to_string(), v.to_string()).is_some() {
            return Err(format!("environment: {k} is set twice"));
        }
    }
    Ok(out)
}

/// Resolve `compose` the way Docker will (using `env`) and lint the result. Works in a
/// throw-away directory under `data_dir`. Returns the model on success.
pub async fn check(
    data_dir: &Path,
    compose: &str,
    env: &BTreeMap<String, String>,
) -> Result<Value, String> {
    precheck(compose)?;
    let dir = data_dir.join(format!("stack-check-{}", crate::auth::generate_password()));
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let result = check_in(&dir, data_dir, compose, env).await;
    // `dir` was created just above and holds only the two files written there.
    let _ = std::fs::remove_dir_all(&dir);
    result
}

async fn check_in(
    dir: &Path,
    data_dir: &Path,
    compose: &str,
    env: &BTreeMap<String, String>,
) -> Result<Value, String> {
    std::fs::write(dir.join("compose.yaml"), compose).map_err(|e| e.to_string())?;
    crate::install::write_env(dir, env).map_err(|e| e.to_string())?;
    let out = tokio::process::Command::new("docker")
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("DOCKER_CONFIG", data_dir.join("docker-config"))
        .envs(std::env::var_os("DOCKER_HOST").map(|h| ("DOCKER_HOST", h)))
        .args(["compose", "-p", "dockwren-check", "--project-directory"])
        .arg(dir)
        .arg("--env-file")
        .arg(dir.join(".env"))
        .arg("-f")
        .arg(dir.join("compose.yaml"))
        .args(["config", "--format", "json"])
        .output()
        .await
        .map_err(|e| format!("cannot run docker: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        // Docker's message names the file and line; show it without our scratch path.
        let msg = err.replace(&format!("{}/", dir.display()), "");
        let msg: Vec<&str> = msg
            .lines()
            .filter(|l| !l.trim().is_empty())
            .take(6)
            .collect();
        return Err(format!(
            "Docker does not accept this file: {}",
            msg.join(" ")
        ));
    }
    let canon = dir.canonicalize().map_err(|e| e.to_string())?;
    let model: Value = serde_json::from_slice(&out.stdout)
        .map_err(|e| format!("unreadable compose model: {e}"))?;
    lint_model(&model, &canon)?;
    Ok(model)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const DIR: &str = "/srv/stack-1";

    fn lint(v: Value) -> Result<(), String> {
        lint_model(&v, Path::new(DIR))
    }

    fn svc(extra: Value) -> Value {
        let mut s = json!({"image": "nginx:1", "command": null, "networks": {"default": null}});
        for (k, v) in extra.as_object().unwrap() {
            s[k] = v.clone();
        }
        json!({"name": "x", "services": {"web": s}, "networks": {"default": {"name": "x_default", "ipam": {}}}})
    }

    #[test]
    fn a_plain_stack_passes() {
        let m = svc(json!({
            "ports": [{"mode": "ingress", "host_ip": "127.0.0.1", "target": 80, "published": "8123", "protocol": "tcp"}],
            "volumes": [
                {"type": "bind", "source": "/srv/stack-1/data/db", "target": "/d"},
                {"type": "volume", "source": "named", "target": "/n"}
            ],
            "environment": {"A": "b"},
            "restart": "unless-stopped"
        }));
        assert_eq!(lint(m.clone()), Ok(()));
        assert_eq!(first_published_port(&m), Some(8123));
    }

    #[test]
    fn dangerous_service_settings_are_refused() {
        for (k, v) in [
            ("privileged", json!(true)),
            ("cap_add", json!(["SYS_ADMIN"])),
            ("network_mode", json!("host")),
            ("pid", json!("host")),
            ("devices", json!(["/dev/sda"])),
            ("security_opt", json!(["seccomp=unconfined"])),
            ("volumes_from", json!(["other"])),
            ("userns_mode", json!("host")),
            ("sysctls", json!({"a": "1"})),
            ("build", json!({"context": "."})),
            ("env_file", json!(["/etc/x"])),
        ] {
            let e = lint(svc(json!({ k: v }))).unwrap_err();
            assert!(e.contains(k), "{k}: {e}");
        }
    }

    #[test]
    fn ports_must_stay_on_loopback() {
        let p = |ip: Option<&str>| {
            let mut port =
                json!({"mode": "ingress", "target": 80, "published": "8080", "protocol": "tcp"});
            if let Some(ip) = ip {
                port["host_ip"] = json!(ip);
            }
            lint(svc(json!({"ports": [port]})))
        };
        assert!(p(None).unwrap_err().contains("127.0.0.1:8080:80"));
        assert!(p(Some("0.0.0.0")).is_err());
        assert!(p(Some("203.0.113.5")).is_err());
        assert_eq!(p(Some("127.0.0.1")), Ok(()));
    }

    #[test]
    fn bind_mounts_must_live_under_data() {
        let b = |src: &str| {
            lint(svc(
                json!({"volumes": [{"type": "bind", "source": src, "target": "/x"}]}),
            ))
        };
        for bad in [
            "/etc",
            "/",
            "/var/run/docker.sock",
            "/srv/stack-1",
            "/srv/stack-1/.env",
            "/srv/stack-1/compose.yaml",
            "/srv/stack-1/data/../.env",
            "/srv/stack-1/database",
            "/srv/stack-1-evil/data/x",
        ] {
            assert!(b(bad).is_err(), "{bad}");
        }
        assert_eq!(b("/srv/stack-1/data"), Ok(()));
        assert_eq!(b("/srv/stack-1/data/a/b"), Ok(()));
        assert!(b("/etc").unwrap_err().contains("not allowed"),);
    }

    #[test]
    fn mount_types_are_limited() {
        let t = |ty: &str| {
            lint(svc(
                json!({"volumes": [{"type": ty, "source": "x", "target": "/x"}]}),
            ))
        };
        assert_eq!(t("tmpfs"), Ok(()));
        assert!(t("npipe").is_err());
        assert!(t("image").is_err());
    }

    #[test]
    fn top_level_extras_are_refused() {
        let mut m = svc(json!({}));
        m["secrets"] = json!({"s": {"file": "/etc/shadow"}});
        assert!(lint(m).unwrap_err().contains("secrets"));
        let mut m = svc(json!({}));
        m["configs"] = json!({});
        assert!(lint(m).is_err());
    }

    #[test]
    fn volumes_and_networks_cannot_reach_the_host() {
        let mut m = svc(json!({}));
        m["volumes"] = json!({"v": {"name": "x_v", "driver": "local", "driver_opts": {"type": "none", "o": "bind", "device": "/"}}});
        assert!(lint(m).unwrap_err().contains("driver_opts"));
        let mut m = svc(json!({}));
        m["volumes"] = json!({"v": {"name": "v", "external": true}});
        assert!(lint(m).unwrap_err().contains("external"));
        let mut m = svc(json!({}));
        m["volumes"] = json!({"v": {"name": "x_v", "driver": "nfs"}});
        assert!(lint(m).is_err());
        let mut m = svc(json!({}));
        m["volumes"] = json!({"v": {"name": "x_v"}});
        assert_eq!(lint(m), Ok(()));
        let mut m = svc(json!({}));
        m["networks"]["host"] = json!({"name": "host", "external": true});
        assert!(lint(m).is_err());
        let mut m = svc(json!({}));
        m["networks"]["m"] = json!({"name": "m", "driver": "macvlan"});
        assert!(lint(m).is_err());
    }

    #[test]
    fn an_image_is_required_and_services_must_exist() {
        assert!(lint(svc(json!({"image": ""}))).is_err());
        assert!(lint(json!({"name": "x", "services": {}})).is_err());
        assert!(lint(json!({"name": "x"})).is_err());
        assert!(lint(json!([])).is_err());
    }

    #[test]
    fn deploy_is_limited_to_resources() {
        assert_eq!(
            lint(svc(
                json!({"deploy": {"resources": {"limits": {"cpus": "1"}}}})
            )),
            Ok(())
        );
        assert!(lint(svc(json!({"deploy": {"placement": {}}}))).is_err());
    }

    #[test]
    fn precheck_refuses_file_readers_and_size() {
        assert!(precheck("").is_err());
        assert!(precheck("services:\n  a:\n    image: x:1\n").is_ok());
        for bad in [
            "include:\n - x",
            "    extends:\n      file: x",
            "env_file: .env",
            "build: .",
        ] {
            assert!(precheck(bad).is_err(), "{bad}");
        }
        assert!(precheck(&"a".repeat(MAX_COMPOSE_BYTES + 1)).is_err());
    }

    #[test]
    fn env_text_is_parsed_strictly() {
        let e = parse_env("# c\nA=1\nB = \"two words\"\n\nC=\n").unwrap();
        assert_eq!(e["A"], "1");
        assert_eq!(e["B"], "two words");
        assert_eq!(e["C"], "");
        assert!(parse_env("nope").is_err());
        assert!(parse_env("1A=x").is_err());
        assert!(parse_env("A=1\nA=2").is_err());
        assert!(parse_env("A-B=1").is_err());
    }

    /// Needs a working `docker compose`; run with `cargo test -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn real_docker_resolves_and_lints() {
        let data = std::env::temp_dir().join(format!(
            "dockwren-stack-{}",
            crate::auth::generate_password()
        ));
        std::fs::create_dir_all(&data).unwrap();
        let env = parse_env("TAG=1").unwrap();
        let ok = "x-common: &c\n  restart: unless-stopped\nservices:\n  web:\n    <<: *c\n    image: nginx:${TAG}\n    ports: [\"127.0.0.1:8123:80\"]\n    volumes: [\"./data:/d\", \"cache:/c\"]\nvolumes:\n  cache:\n";
        let m = check(&data, ok, &env).await.unwrap();
        assert_eq!(first_published_port(&m), Some(8123));
        assert_eq!(m["services"]["web"]["image"], "nginx:1");
        for (bad, word) in [
            (
                "services:\n  a:\n    image: x:1\n    privileged: true\n",
                "privileged",
            ),
            (
                "services:\n  a:\n    image: x:1\n    ports: [\"8080:80\"]\n",
                "127.0.0.1",
            ),
            (
                "services:\n  a: &a\n    image: x:1\n  b:\n    <<: *a\n    volumes: [\"/etc:/e\"]\n",
                "bind mount",
            ),
            (
                "services:\n  a:\n    image: x:1\n    volumes: [\"./data/../../x:/e\"]\n",
                "bind mount",
            ),
            (
                "services:\n  a:\n    image: x:1\n    cap_add: [ALL]\n",
                "cap_add",
            ),
            (
                "services:\n  a:\n    image: x:1\n    network_mode: host\n",
                "network_mode",
            ),
            (
                "services:\n  a:\n    image: x:1\n    command: [\"${NOPE:?need NOPE}\"]\n",
                "Docker does not accept",
            ),
            ("services: [", "Docker does not accept"),
            (
                "services:\n  a:\n    image: x:1\n    volumes: [\"vol:/v\"]\nvolumes:\n  vol:\n    driver_opts: {type: none, o: bind, device: /}\n",
                "driver_opts",
            ),
        ] {
            let e = check(&data, bad, &env).await.unwrap_err();
            assert!(e.contains(word), "{bad}: {e}");
        }
        // The scratch directories are gone.
        let left: Vec<_> = std::fs::read_dir(&data).unwrap().flatten().collect();
        assert!(left.is_empty(), "{left:?}");
        std::fs::remove_dir_all(&data).unwrap();
    }
}
