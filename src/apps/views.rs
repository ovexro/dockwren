//! View structs (askama pages) and their builders.

use super::*;

// ---- views -------------------------------------------------------------

pub(super) struct AppView {
    pub(super) id: String,
    pub(super) name: String,
    pub(super) description: String,
    pub(super) https_note: bool,
    /// `url@sha` for a pinned third-party template.
    pub(super) third_party: Option<String>,
}

/// Hidden template that pasted Compose stacks are filed under.
pub(super) const CUSTOM_ID: &str = "custom";

#[derive(Template)]
#[template(path = "stack_form.html")]
pub(super) struct StackForm {
    pub(super) name: String,
    pub(super) compose: String,
    pub(super) env: String,
    pub(super) error: Option<String>,
}

#[derive(Template)]
#[template(path = "catalog.html")]
pub(super) struct AppsPage {
    pub(super) apps: Vec<AppView>,
}

pub(super) struct FieldView {
    pub(super) key: String,
    pub(super) label: String,
    /// text | password | select
    pub(super) input: &'static str,
    pub(super) value: String,
    pub(super) placeholder: String,
    pub(super) help: String,
    pub(super) options: Vec<String>,
    pub(super) required: bool,
}

#[derive(Template)]
#[template(path = "app_form.html")]
pub(super) struct FormPage {
    pub(super) id: String,
    pub(super) name: String,
    pub(super) note: String,
    pub(super) https_note: bool,
    /// "53/udp (DNS)" lines; non-empty means the form asks for confirmation.
    pub(super) lan_ports: Vec<String>,
    pub(super) fields: Vec<FieldView>,
    pub(super) error: Option<String>,
}

#[derive(Template)]
#[template(path = "job.html")]
pub(super) struct JobPage {
    pub(super) id: String,
    pub(super) verb: &'static str,
    pub(super) instance: String,
    pub(super) log: Vec<String>,
    pub(super) running: bool,
    pub(super) error: Option<String>,
    pub(super) secrets: Vec<(String, String)>,
    pub(super) ports: Vec<(String, String)>,
    pub(super) note: String,
}

pub(super) struct InstanceView {
    pub(super) name: String,
    pub(super) app: String,
    pub(super) ports: String,
    pub(super) status: String,
    /// "1 → 2" when the bundled template is newer than the instance.
    pub(super) update: Option<String>,
    /// The user chose to skip that version.
    pub(super) update_skipped: bool,
    /// "Upstream has 28.4.0" when a registry has a newer tag (information only).
    pub(super) upstream: Option<String>,
    pub(super) snapshots: usize,
    pub(super) last_backup: Option<String>,
    /// Epoch of the newest snapshot.
    pub(super) last_backup_at: Option<u64>,
    pub(super) hostname: Option<String>,
    /// Last external check of the public URL: Some(true) reachable, Some(false) down.
    pub(super) public_up: Option<bool>,
    /// "99.9% reachable over 30 days" from the history of external checks.
    pub(super) uptime: Option<String>,
    /// Last image scan: "2 critical, 5 high" (None when clean or not scanned yet).
    pub(super) vulns: Option<String>,
    /// The template has settings that can be changed after install.
    pub(super) editable: bool,
    /// A pasted stack: its Compose file can be edited here.
    pub(super) stack_editable: bool,
    /// Stopped (no containers) and not published: the whole app can be deleted.
    pub(super) deletable: bool,
    /// The template is known, so the main service can be limited.
    pub(super) limitable: bool,
    /// The template has generated passwords that can be shown again.
    pub(super) has_passwords: bool,
    /// Apps this one needs, each with whether it is healthy right now.
    pub(super) needs: Vec<(String, bool)>,
    /// No containers: the app can be started from here.
    pub(super) stopped: bool,
    /// What no longer matches the Compose files (see `drift`); empty when in sync.
    pub(super) drift: Vec<String>,
    /// Its stop/start schedule, when it has one.
    pub(super) power: Option<String>,
    /// Confirmation texts; they name the apps that need this one.
    pub(super) stop_confirm: String,
    pub(super) delete_confirm: String,
    /// Docker logs of this app have no size cap yet.
    pub(super) uncapped: bool,
}

#[derive(Template)]
#[template(path = "instances.html")]
pub(super) struct InstancesPage {
    pub(super) instances: Vec<InstanceView>,
    /// Apps ticked for a bulk action; the list refreshes itself, so the ticks travel with it.
    pub(super) picked: Vec<String>,
    pub(super) error: Option<String>,
    pub(super) note: Option<String>,
}

pub(super) fn form_view(
    t: &AppTemplate,
    values: &HashMap<String, String>,
    error: Option<String>,
) -> FormPage {
    let url_field = t.manifest.app.public_url_field.as_deref();
    let mut fields: Vec<FieldView> = t
        .manifest
        .fields
        .iter()
        .filter(|f| Some(f.key.as_str()) != url_field)
        .map(|f| {
            let bool_or_select = matches!(f.kind, FieldType::Bool | FieldType::Select);
            let options = match f.kind {
                FieldType::Bool => vec!["true".to_string(), "false".to_string()],
                FieldType::Select => f.options.clone(),
                _ => Vec::new(),
            };
            let default = f.default.clone().unwrap_or_default();
            let entered = values.get(&f.key).cloned().unwrap_or_default();
            FieldView {
                key: f.key.clone(),
                label: f.label.clone(),
                input: if bool_or_select {
                    "select"
                } else if f.kind == FieldType::Password {
                    "password"
                } else {
                    "text"
                },
                // Selects need a concrete value; text inputs show the default as a placeholder.
                value: if bool_or_select && entered.is_empty() {
                    default.clone()
                } else {
                    entered
                },
                placeholder: default,
                help: f.help.clone().unwrap_or_default(),
                options,
                required: f.required && f.default.is_none(),
            }
        })
        .collect();
    if url_field.is_some() || t.manifest.app.accepts_public_host {
        let base = "The web address people type in their browser to open this app, for example blog.example.com (the domain must already be in your Cloudflare account). Leave it empty to use the app only on this server.";
        let help = if crate::public::Settings::from_env().is_some() {
            format!("{base} Dockwren will also make it reachable from the internet for you.")
        } else {
            format!(
                "{base} Cloudflare is not connected yet: paste a token in the Cloudflare tab first, then press Publish on the app's Public URL page."
            )
        };
        let at = usize::from(fields.first().is_some_and(|f| f.key == "INSTANCE_NAME"));
        fields.insert(
            at,
            FieldView {
                key: "PUBLIC_HOST".into(),
                label: "Public hostname".into(),
                input: "text",
                value: values.get("PUBLIC_HOST").cloned().unwrap_or_default(),
                placeholder: "blog.example.com".into(),
                help,
                options: Vec::new(),
                required: false,
            },
        );
    }
    FormPage {
        id: t.manifest.app.id.clone(),
        name: t.manifest.app.name.clone(),
        note: t.manifest.app.note.clone().unwrap_or_default(),
        https_note: t.manifest.app.needs_public_https,
        lan_ports: t
            .manifest
            .lan_ports
            .iter()
            .map(|l| format!("{}/{} ({})", l.port, l.protocol, l.label))
            .collect(),
        fields,
        error,
    }
}

pub(super) fn render<T: askama::Template>(page: T) -> Html<String> {
    Html(page.render().expect("template renders"))
}

// ---- handlers ----------------------------------------------------------
