//! Containers list, log viewer and container actions.

use super::*;

#[derive(Template)]
#[template(path = "containers.html")]
pub(super) struct ContainersTemplate {
    containers: Vec<ContainerInfo>,
    error: Option<String>,
    terminal: bool,
}

pub(super) fn render_containers(
    result: Result<Vec<ContainerInfo>, String>,
    terminal: bool,
) -> String {
    let page = match result {
        Ok(containers) => ContainersTemplate {
            containers,
            error: None,
            terminal,
        },
        Err(e) => ContainersTemplate {
            containers: Vec::new(),
            error: Some(format!("Docker is not reachable: {e}")),
            terminal,
        },
    };
    page.render().expect("containers template renders")
}

pub(super) async fn containers(State(state): State<AppState>) -> Html<String> {
    let result = state.docker.containers().await.map_err(|e| e.to_string());
    Html(render_containers(result, state.terminal.is_on()))
}

#[derive(Template)]
#[template(path = "logs.html")]
pub(super) struct LogsTemplate {
    pub(super) id: String,
    pub(super) text: String,
    pub(super) error: Option<String>,
    pub(super) q: String,
    pub(super) level: String,
    pub(super) since: String,
    /// Lines that matched (only meaningful while a filter is on).
    pub(super) matched: usize,
    pub(super) filtered: bool,
    /// Render only the refreshing part (the filter form stays untouched while typing).
    pub(super) part: bool,
}

#[derive(serde::Deserialize, Default)]
pub(super) struct LogQuery {
    q: Option<String>,
    level: Option<String>,
    since: Option<String>,
    part: Option<String>,
}

/// Parsed, bounded log filter.
pub(super) struct LogFilter {
    q: String,
    level: String,
    since: String,
}

impl LogFilter {
    fn from(q: &LogQuery) -> Self {
        let pick = |v: &Option<String>, ok: &[&str], d: &str| {
            v.as_deref()
                .filter(|x| ok.contains(x))
                .unwrap_or(d)
                .to_string()
        };
        Self {
            q: q.q.as_deref().unwrap_or("").chars().take(100).collect(),
            level: pick(&q.level, &["all", "warn", "error"], "all"),
            since: pick(&q.since, &["all", "15m", "1h", "24h"], "all"),
        }
    }
    fn active(&self) -> bool {
        !self.q.is_empty() || self.level != "all" || self.since != "all"
    }
    /// Unix time to read logs from (0 = no limit).
    fn since_unix(&self) -> i32 {
        let secs = match self.since.as_str() {
            "15m" => 900,
            "1h" => 3600,
            "24h" => 86400,
            _ => return 0,
        };
        (metrics::now_unix().saturating_sub(secs)).min(i32::MAX as u64) as i32
    }
}

/// Lines read from Docker when a filter is on (the plain view shows the last `LOG_LINES`).
pub(super) const LOG_SEARCH_LINES: u32 = 20_000;
/// Most matching lines shown in the page; the download has everything that was read.
pub(super) const LOG_SHOWN_LINES: usize = 500;

pub(super) async fn read_logs(
    docker: &docker::DockerClient,
    id: &str,
    f: &LogFilter,
) -> Result<(String, usize), bollard::errors::Error> {
    if !f.active() {
        let t = docker.logs(id, LOG_LINES).await?;
        let n = t.lines().count();
        return Ok((t, n));
    }
    let raw = docker
        .logs_since(id, LOG_SEARCH_LINES, f.since_unix())
        .await?;
    Ok(docker::filter_log_lines(&raw, &f.q, &f.level))
}

pub(super) async fn container_logs(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<LogQuery>,
) -> Result<Html<String>, StatusCode> {
    if !docker::valid_id(&id) {
        return Err(StatusCode::NOT_FOUND);
    }
    let f = LogFilter::from(&query);
    let (text, matched, error) = match read_logs(&state.docker, &id, &f).await {
        Ok((t, n)) => {
            // Show the newest lines only.
            let skip = n.saturating_sub(LOG_SHOWN_LINES);
            let shown: Vec<&str> = t.lines().skip(skip).collect();
            (shown.join("\n"), n, None)
        }
        Err(e) => (String::new(), 0, Some(format!("Cannot read logs: {e}"))),
    };
    let page = LogsTemplate {
        filtered: f.active(),
        id,
        text,
        error,
        q: f.q,
        level: f.level,
        since: f.since,
        matched,
        part: query.part.is_some(),
    };
    Ok(Html(page.render().expect("logs template renders")))
}

/// The same filter as a plain-text download (everything that was read, not just the page's tail).
pub(super) async fn download_logs(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<LogQuery>,
) -> Result<impl IntoResponse, StatusCode> {
    if !docker::valid_id(&id) {
        return Err(StatusCode::NOT_FOUND);
    }
    let f = LogFilter::from(&query);
    let raw = state
        .docker
        .logs_since(&id, LOG_SEARCH_LINES, f.since_unix())
        .await
        .map_err(|_| StatusCode::BAD_GATEWAY)?;
    let (text, _) = docker::filter_log_lines(&raw, &f.q, &f.level);
    Ok((
        [
            (
                header::CONTENT_TYPE,
                "text/plain; charset=utf-8".to_string(),
            ),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{id}.log\""),
            ),
        ],
        text,
    ))
}

pub(super) async fn close_logs() -> Html<&'static str> {
    Html("")
}

pub(super) const LOG_LINES: u32 = 200;

/// Rejects cross-origin POSTs (defence in depth on top of the SameSite=Strict session cookie).
pub fn same_origin(headers: &HeaderMap) -> bool {
    let Some(origin) = headers.get(header::ORIGIN) else {
        return true; // non-browser client; it still needs the session cookie
    };
    let host = headers.get(header::HOST).and_then(|h| h.to_str().ok());
    let origin_host = origin
        .to_str()
        .ok()
        .and_then(|o| o.split_once("://"))
        .map(|(_, h)| h);
    host.is_some() && host == origin_host
}

pub(super) async fn container_action(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, action)): Path<(String, String)>,
) -> Result<Html<String>, StatusCode> {
    if !same_origin(&headers) {
        return Err(StatusCode::FORBIDDEN);
    }
    let action = Action::parse(&action).ok_or(StatusCode::NOT_FOUND)?;
    if !docker::valid_id(&id) {
        return Err(StatusCode::NOT_FOUND);
    }
    if let Err(e) = state.docker.act(&id, action).await {
        return Ok(Html(render_containers(
            Err(e.to_string()),
            state.terminal.is_on(),
        )));
    }
    Ok(containers(State(state)).await)
}
