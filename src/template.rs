//! App templates: `apps/<id>/manifest.toml` + `compose.yaml`.
//! Loading validates the manifest and lints the compose file (see the template spec).

#![allow(dead_code)] // manifest fields are consumed by upcoming UI, post-start and update code

use std::collections::HashSet;
use std::path::Path;

use serde::Deserialize;

const RESERVED_VARS: &[&str] = &["INSTANCE_NAME"];

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub app: App,
    #[serde(default)]
    pub fields: Vec<Field>,
    #[serde(default)]
    pub secrets: Vec<Secret>,
    #[serde(default)]
    pub services: Vec<Service>,
    #[serde(default)]
    pub post_start: Vec<PostStart>,
    /// Commands run once after an update that crosses `to_version`.
    #[serde(default)]
    pub migrations: Vec<Migration>,
    #[serde(default)]
    pub backup: Backup,
    #[serde(default)]
    pub update: Update,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct App {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub version: u32,
    #[serde(default)]
    pub homepage: Option<String>,
    #[serde(default)]
    pub needs_public_https: bool,
    /// Key of the field that holds the app's address. When the installer gives a public
    /// hostname, this field is set to `https://<hostname>` and hidden from the form.
    #[serde(default)]
    pub public_url_field: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FieldType {
    String,
    Password,
    Int,
    Bool,
    Port,
    Url,
    Email,
    Select,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Field {
    pub key: String,
    pub label: String,
    #[serde(rename = "type")]
    pub kind: FieldType,
    #[serde(default)]
    pub default: Option<String>,
    #[serde(default)]
    pub pattern: Option<String>,
    #[serde(default = "yes")]
    pub required: bool,
    #[serde(default)]
    pub help: Option<String>,
    #[serde(default)]
    pub options: Vec<String>,
    #[serde(default)]
    pub min: Option<i64>,
    #[serde(default)]
    pub max: Option<i64>,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Secret {
    pub key: String,
    pub kind: SecretKind,
    #[serde(default = "default_len")]
    pub length: usize,
    #[serde(default)]
    pub reveal: bool,
}

fn default_len() -> usize {
    32
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SecretKind {
    Password,
    Hex,
    Argon2,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Service {
    pub name: String,
    #[serde(default)]
    pub main: bool,
    pub health: Health,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "source", rename_all = "lowercase", deny_unknown_fields)]
pub enum Health {
    Image,
    Panel {
        test: Vec<String>,
        #[serde(default)]
        interval: Option<String>,
        #[serde(default)]
        start_period: Option<String>,
    },
}

/// A command run (via `compose exec`) after updating an instance to a
/// template version `>= to_version`, when the instance was older than that.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Migration {
    pub to_version: u32,
    pub name: String,
    pub service: String,
    pub run: Vec<String>,
    #[serde(default)]
    pub user: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PostStart {
    pub name: String,
    pub service: String,
    pub run: Vec<String>,
    #[serde(default)]
    pub user: Option<String>,
    /// Pull a value out of the command's stdout and show it once (not stored).
    #[serde(default)]
    pub capture: Option<Capture>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Capture {
    pub label: String,
    /// Regex with exactly one capture group, matched against stdout.
    pub regex: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Backup {
    #[serde(default)]
    pub paths: Vec<String>,
    #[serde(default)]
    pub dumps: Vec<Dump>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Dump {
    pub service: String,
    pub run: Vec<String>,
    pub output: String,
    /// Command that reads the dump on stdin to restore it (via `compose exec`).
    #[serde(default)]
    pub restore: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Update {
    #[serde(default)]
    pub image_pins: Vec<String>,
    #[serde(default)]
    pub pre_update: Vec<String>,
    #[serde(default)]
    pub pinned_major: Vec<String>,
}

#[derive(Debug)]
pub struct Template {
    pub manifest: Manifest,
    pub compose: String,
    /// Optional `icon.svg` shown in the catalog and instance list.
    pub icon: Option<String>,
}

/// Parse and validate a template from its two files' contents.
pub fn parse(manifest: &str, compose: &str) -> Result<Template, String> {
    let manifest: Manifest = toml::from_str(manifest).map_err(|e| format!("manifest.toml: {e}"))?;
    validate(&manifest, compose)?;
    Ok(Template {
        manifest,
        compose: compose.to_string(),
        icon: None,
    })
}

/// Load and validate one template directory.
pub fn load(dir: &Path) -> Result<Template, String> {
    let read = |name: &str| {
        std::fs::read_to_string(dir.join(name))
            .map_err(|e| format!("{}: {name}: {e}", dir.display()))
    };
    let mut t = parse(&read("manifest.toml")?, &read("compose.yaml")?)
        .map_err(|e| format!("{}: {e}", dir.display()))?;
    t.icon = std::fs::read_to_string(dir.join("icon.svg")).ok();
    Ok(t)
}

/// Load every template under `root` (one sub-directory each), sorted by id.
pub fn load_all(root: &Path) -> Result<Vec<Template>, String> {
    let mut out = Vec::new();
    let entries = std::fs::read_dir(root).map_err(|e| format!("{}: {e}", root.display()))?;
    for entry in entries {
        let path = entry.map_err(|e| e.to_string())?.path();
        if path.is_dir() {
            out.push(load(&path)?);
        }
    }
    out.sort_by(|a, b| a.manifest.app.id.cmp(&b.manifest.app.id));
    Ok(out)
}

/// Templates shipped inside the binary.
const BUNDLED: &[(&str, &str, &str)] = &[
    (
        include_str!("../apps/gitea/manifest.toml"),
        include_str!("../apps/gitea/compose.yaml"),
        include_str!("../apps/gitea/icon.svg"),
    ),
    (
        include_str!("../apps/vaultwarden/manifest.toml"),
        include_str!("../apps/vaultwarden/compose.yaml"),
        include_str!("../apps/vaultwarden/icon.svg"),
    ),
    (
        include_str!("../apps/uptime-kuma/manifest.toml"),
        include_str!("../apps/uptime-kuma/compose.yaml"),
        include_str!("../apps/uptime-kuma/icon.svg"),
    ),
    (
        include_str!("../apps/miniflux/manifest.toml"),
        include_str!("../apps/miniflux/compose.yaml"),
        include_str!("../apps/miniflux/icon.svg"),
    ),
    (
        include_str!("../apps/memos/manifest.toml"),
        include_str!("../apps/memos/compose.yaml"),
        include_str!("../apps/memos/icon.svg"),
    ),
    (
        include_str!("../apps/paperless-ngx/manifest.toml"),
        include_str!("../apps/paperless-ngx/compose.yaml"),
        include_str!("../apps/paperless-ngx/icon.svg"),
    ),
    (
        include_str!("../apps/wordpress/manifest.toml"),
        include_str!("../apps/wordpress/compose.yaml"),
        include_str!("../apps/wordpress/icon.svg"),
    ),
];

/// Bundled templates plus (optionally) a directory of extra or overriding ones.
pub fn catalog(extra_dir: Option<&Path>) -> Result<Vec<Template>, String> {
    let mut all = BUNDLED
        .iter()
        .map(|(m, c, i)| {
            parse(m, c).map(|mut t| {
                t.icon = Some((*i).to_string());
                t
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    if let Some(dir) = extra_dir.filter(|d| d.is_dir()) {
        for t in load_all(dir)? {
            all.retain(|b| b.manifest.app.id != t.manifest.app.id);
            all.push(t);
        }
    }
    all.sort_by(|a, b| a.manifest.app.id.cmp(&b.manifest.app.id));
    Ok(all)
}

/// All `${NAME}` references in `text`.
fn var_refs(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(i) = rest.find("${") {
        rest = &rest[i + 2..];
        if let Some(j) = rest.find('}') {
            out.push(&rest[..j]);
            rest = &rest[j + 1..];
        }
    }
    out
}

fn valid_key(k: &str) -> bool {
    !k.is_empty()
        && k.chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
        && !k.starts_with(|c: char| c.is_ascii_digit())
}

pub fn validate(m: &Manifest, compose: &str) -> Result<(), String> {
    let id = &m.app.id;
    if id.is_empty()
        || !id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err(format!("app.id {id:?} must match [a-z0-9-]+"));
    }

    if let Some(k) = &m.app.public_url_field
        && !m
            .fields
            .iter()
            .any(|f| &f.key == k && matches!(f.kind, FieldType::Url | FieldType::String))
    {
        return Err(format!(
            "app.public_url_field {k} must name a url or string field"
        ));
    }

    // Names of every variable a template may reference, in definition order.
    let mut known: HashSet<&str> = RESERVED_VARS.iter().copied().collect();
    for f in &m.fields {
        if !valid_key(&f.key) {
            return Err(format!("field key {:?} must be UPPER_SNAKE_CASE", f.key));
        }
        if let Some(d) = &f.default {
            for r in var_refs(d) {
                if !known.contains(r) {
                    return Err(format!(
                        "field {}: default uses unknown or later variable ${{{r}}}",
                        f.key
                    ));
                }
            }
        }
        if f.kind == FieldType::Select && f.options.is_empty() {
            return Err(format!("field {}: select needs options", f.key));
        }
        if f.pattern.is_some() && !matches!(f.kind, FieldType::String | FieldType::Password) {
            return Err(format!(
                "field {}: pattern only applies to string/password",
                f.key
            ));
        }
        // A field may define a reserved variable (the form input for it).
        if !known.insert(&f.key) && !RESERVED_VARS.contains(&f.key.as_str()) {
            return Err(format!("duplicate variable {}", f.key));
        }
    }
    for s in &m.secrets {
        if !valid_key(&s.key) {
            return Err(format!("secret key {:?} must be UPPER_SNAKE_CASE", s.key));
        }
        if s.length < 16 {
            return Err(format!("secret {}: length must be at least 16", s.key));
        }
        if !known.insert(&s.key) {
            return Err(format!("duplicate variable {}", s.key));
        }
    }

    // Services: unique names, exactly one main, panel health needs a test.
    let mut names = HashSet::new();
    for s in &m.services {
        if !names.insert(s.name.as_str()) {
            return Err(format!("duplicate service {}", s.name));
        }
        if let Health::Panel { test, .. } = &s.health
            && test.is_empty()
        {
            return Err(format!("service {}: empty health test", s.name));
        }
    }
    if m.services.iter().filter(|s| s.main).count() != 1 {
        return Err("exactly one service must have main = true".into());
    }
    for h in &m.post_start {
        if !names.contains(h.service.as_str()) {
            return Err(format!(
                "post_start {}: unknown service {}",
                h.name, h.service
            ));
        }
        if h.run.is_empty() {
            return Err(format!("post_start {}: empty command", h.name));
        }
        if let Some(c) = &h.capture {
            let re = regex::Regex::new(&c.regex)
                .map_err(|e| format!("post_start {}: bad capture regex: {e}", h.name))?;
            if re.captures_len() != 2 {
                return Err(format!(
                    "post_start {}: capture regex needs exactly one group",
                    h.name
                ));
            }
        }
    }
    for g in &m.migrations {
        if !names.contains(g.service.as_str()) {
            return Err(format!(
                "migration {}: unknown service {}",
                g.name, g.service
            ));
        }
        if g.run.is_empty() {
            return Err(format!("migration {}: empty command", g.name));
        }
        if g.to_version < 2 || g.to_version > m.app.version {
            return Err(format!(
                "migration {}: to_version {} must be between 2 and the app version {}",
                g.name, g.to_version, m.app.version
            ));
        }
    }
    for d in &m.backup.dumps {
        if !names.contains(d.service.as_str()) {
            return Err(format!("backup dump: unknown service {}", d.service));
        }
        if d.run.is_empty() {
            return Err("backup dump: empty command".into());
        }
        if d.output.is_empty() || d.output.contains('/') || d.output.starts_with('.') {
            return Err(format!(
                "backup dump output {:?} must be a plain file name",
                d.output
            ));
        }
    }
    for p in &m.backup.paths {
        if p.is_empty()
            || p.starts_with('/')
            || p.split('/').any(|c| c.is_empty() || c == ".." || c == ".")
        {
            return Err(format!(
                "backup path {p:?} must be relative and inside the instance"
            ));
        }
    }

    // Variable references in post_start commands and health tests.
    let mut texts: Vec<&str> = Vec::new();
    for h in &m.post_start {
        texts.extend(h.run.iter().map(String::as_str));
    }
    for g in &m.migrations {
        texts.extend(g.run.iter().map(String::as_str));
    }
    for s in &m.services {
        if let Health::Panel { test, .. } = &s.health {
            texts.extend(test.iter().map(String::as_str));
        }
    }
    for t in texts {
        for r in var_refs(t) {
            if !known.contains(r) {
                return Err(format!("unknown variable ${{{r}}} in manifest command"));
            }
        }
    }

    lint_compose(compose, &known, &names)
}

/// Line-based checks on the compose file. Not a YAML parser: the full structure
/// is validated later by `docker compose config`; this rejects the dangerous
/// and sloppy patterns early.
fn lint_compose(
    compose: &str,
    known: &HashSet<&str>,
    services: &HashSet<&str>,
) -> Result<(), String> {
    for r in var_refs(compose) {
        let name = r.split([':', '-', '?', '+']).next().unwrap_or(r);
        if !known.contains(name) {
            return Err(format!("compose.yaml: unknown variable ${{{r}}}"));
        }
    }
    let mut in_ports = false;
    let mut in_volumes = false;
    let mut compose_services = HashSet::new();
    for (n, raw) in compose.lines().enumerate() {
        let line = raw.trim();
        let at = |msg: &str| format!("compose.yaml:{}: {msg}", n + 1);
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        // Two-space-indented keys under `services:` are service names.
        if raw.starts_with("  ") && !raw.starts_with("   ") && line.ends_with(':') {
            compose_services.insert(line.trim_end_matches(':').to_string());
        }
        for bad in [
            "privileged:",
            "network_mode:",
            "docker.sock",
            "pid: host",
            "cap_add:",
        ] {
            if line.contains(bad) {
                return Err(at(&format!("{bad:?} is not allowed in templates")));
            }
        }
        if let Some(img) = line.strip_prefix("image:") {
            let img = img.trim();
            let tag = img
                .rsplit('/')
                .next()
                .unwrap_or(img)
                .split_once(':')
                .map(|(_, t)| t);
            if matches!(tag, None | Some("latest")) {
                return Err(at(&format!(
                    "image {img:?} must have a pinned tag (not latest)"
                )));
            }
        }
        if line == "ports:" {
            in_ports = true;
            in_volumes = false;
            continue;
        }
        if line == "volumes:" {
            in_volumes = true;
            in_ports = false;
            continue;
        }
        if let Some(item) = line.strip_prefix("- ") {
            let item = item.trim().trim_matches('"').trim_matches('\'');
            if in_ports && !item.starts_with("127.0.0.1:") {
                return Err(at("published ports must bind 127.0.0.1"));
            }
            if in_volumes {
                let src = item.split(':').next().unwrap_or("");
                if src.starts_with('/') || src.starts_with('~') || src.contains("..") {
                    return Err(at("bind mounts must be relative to the instance dir"));
                }
            }
        } else {
            in_ports = false;
            in_volumes = false;
        }
        // Literal credentials: any `*PASSWORD*`/`*SECRET*`/`*TOKEN*` env value must be a ${VAR}.
        if let Some((k, v)) = line.split_once(':') {
            let ku = k.to_ascii_uppercase();
            let v = v.trim().trim_matches('"');
            if (ku.contains("PASSWORD")
                || ku.contains("PASSWD")
                || ku.contains("SECRET")
                || ku.contains("TOKEN"))
                && !v.is_empty()
                && !v.starts_with("${")
            {
                return Err(at("secret-looking values must come from ${VAR}"));
            }
        }
    }
    for s in services {
        if !compose_services.contains(*s) {
            return Err(format!(
                "manifest service {s} is not defined in compose.yaml"
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apps() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("apps")
    }

    #[test]
    fn embedded_catalog_matches_apps_dir_and_dir_overrides() {
        let ids: Vec<_> = catalog(None)
            .unwrap()
            .iter()
            .map(|t| t.manifest.app.id.clone())
            .collect();
        assert_eq!(
            ids,
            [
                "gitea",
                "memos",
                "miniflux",
                "paperless-ngx",
                "uptime-kuma",
                "vaultwarden",
                "wordpress"
            ]
        );
        // An on-disk template with the same id replaces the bundled one.
        let c = catalog(Some(&apps())).unwrap();
        assert_eq!(c.len(), 7);
    }

    #[test]
    fn every_bundled_template_has_an_svg_icon() {
        for t in catalog(None).unwrap() {
            let svg = t.icon.unwrap_or_default();
            assert!(svg.starts_with("<svg"), "{} has no icon", t.manifest.app.id);
            assert!(
                !svg.contains("<script"),
                "{} icon has a script",
                t.manifest.app.id
            );
        }
    }

    #[test]
    fn bundled_templates_load() {
        let all = load_all(&apps()).expect("bundled templates are valid");
        let ids: Vec<_> = all.iter().map(|t| t.manifest.app.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "gitea",
                "memos",
                "miniflux",
                "paperless-ngx",
                "uptime-kuma",
                "vaultwarden",
                "wordpress"
            ]
        );
    }

    fn manifest(extra: &str) -> Manifest {
        toml::from_str(&format!(
            "[app]\nid=\"x\"\nname=\"X\"\nversion=1\n[[services]]\nname=\"web\"\nmain=true\nhealth={{source=\"image\"}}\n{extra}"
        ))
        .unwrap()
    }

    const OK: &str =
        "services:\n  web:\n    image: nginx:1.27\n    ports:\n      - \"127.0.0.1:8080:80\"\n";

    #[test]
    fn minimal_template_is_valid() {
        assert_eq!(validate(&manifest(""), OK), Ok(()));
    }

    #[test]
    fn public_url_field_must_name_a_url_or_string_field() {
        let mut m = manifest("");
        m.app.public_url_field = Some("NOPE".into());
        assert!(validate(&m, OK).unwrap_err().contains("public_url_field"));
    }

    #[test]
    fn rejects_unpinned_images() {
        for img in ["nginx", "nginx:latest"] {
            let c = OK.replace("nginx:1.27", img);
            assert!(
                validate(&manifest(""), &c).unwrap_err().contains("pinned"),
                "{img}"
            );
        }
    }

    #[test]
    fn rejects_public_ports_and_dangerous_options() {
        let m = manifest("");
        let cases = [
            OK.replace("127.0.0.1:8080:80", "8080:80"),
            OK.replace("    ports:", "    privileged: true\n    ports:"),
            OK.replace("    ports:", "    network_mode: host\n    ports:"),
            format!("{OK}    volumes:\n      - /var/run/docker.sock:/s\n"),
            format!("{OK}    volumes:\n      - /home:/h\n"),
            format!("{OK}    environment:\n      DB_PASSWORD: hunter2\n"),
        ];
        for c in cases {
            assert!(validate(&m, &c).is_err(), "should reject:\n{c}");
        }
    }

    #[test]
    fn rejects_unknown_variables_and_services() {
        let c = format!("{OK}    environment:\n      A: ${{NOPE}}\n");
        assert!(validate(&manifest(""), &c).unwrap_err().contains("NOPE"));
        let m = manifest("[[post_start]]\nname=\"p\"\nservice=\"ghost\"\nrun=[\"true\"]\n");
        assert!(validate(&m, OK).unwrap_err().contains("ghost"));
    }

    #[test]
    fn rejects_bad_migrations() {
        for bad in [
            "service=\"ghost\"\nrun=[\"true\"]\nto_version=2\n",
            "service=\"web\"\nrun=[]\nto_version=2\n",
            "service=\"web\"\nrun=[\"true\"]\nto_version=1\n",
            "service=\"web\"\nrun=[\"true\"]\nto_version=99\n",
            "service=\"web\"\nrun=[\"${NOPE}\"]\nto_version=2\n",
        ] {
            let mut m = manifest(&format!("[[migrations]]\nname=\"m\"\n{bad}"));
            m.app.version = 3;
            assert!(validate(&m, OK).is_err(), "{bad}");
        }
        let mut m =
            manifest("[[migrations]]\nname=\"m\"\nservice=\"web\"\nrun=[\"true\"]\nto_version=3\n");
        m.app.version = 3;
        assert_eq!(validate(&m, OK), Ok(()));
    }

    #[test]
    fn rejects_bad_capture_regex() {
        for re in ["(", "no group", "(a)(b)"] {
            let m = manifest(&format!(
                "[[post_start]]\nname=\"p\"\nservice=\"web\"\nrun=[\"true\"]\ncapture={{label=\"L\", regex={re:?}}}\n"
            ));
            assert!(validate(&m, OK).is_err(), "{re}");
        }
    }

    #[test]
    fn rejects_bad_backup_paths_and_short_secrets() {
        let m = manifest("[backup]\npaths=[\"../etc\"]\n");
        assert!(validate(&m, OK).is_err());
        let m = manifest("[[secrets]]\nkey=\"S\"\nkind=\"password\"\nlength=8\n");
        assert!(validate(&m, OK).is_err());
    }
}
