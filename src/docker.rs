//! Thin wrapper over the Docker Engine API (bollard). Only what the UI needs.

use bollard::{
    Docker,
    models::ContainerSummary,
    query_parameters::{ListContainersOptionsBuilder, LogsOptionsBuilder, StatsOptionsBuilder},
};
use futures_util::StreamExt;

/// Upper bound for log text sent to the browser (the newest bytes are kept).
const MAX_LOG_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerInfo {
    /// Short (12 character) container id.
    pub id: String,
    pub name: String,
    pub image: String,
    /// Lifecycle state: running, exited, paused, ...
    pub state: String,
    /// Human readable status, e.g. "Up 3 hours".
    pub status: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Start,
    Stop,
    Restart,
}

impl Action {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "start" => Some(Self::Start),
            "stop" => Some(Self::Stop),
            "restart" => Some(Self::Restart),
            _ => None,
        }
    }
}

/// Container ids are hex; anything else (names, paths) is refused before it reaches the API URL.
pub fn valid_id(id: &str) -> bool {
    (12..=64).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_hexdigit())
}

#[derive(Clone)]
pub struct DockerClient {
    inner: Docker,
}

impl DockerClient {
    /// Connects to the local Docker socket (honours `DOCKER_HOST`).
    pub fn connect() -> Result<Self, bollard::errors::Error> {
        Ok(Self {
            inner: Docker::connect_with_defaults()?,
        })
    }

    /// Starts, stops or restarts one container. `id` must pass [`valid_id`].
    pub async fn act(&self, id: &str, action: Action) -> Result<(), bollard::errors::Error> {
        match action {
            Action::Start => self.inner.start_container(id, None).await,
            Action::Stop => self.inner.stop_container(id, None).await,
            Action::Restart => self.inner.restart_container(id, None).await,
        }
    }

    /// Returns the last `tail` log lines (stdout and stderr). `id` must pass [`valid_id`].
    pub async fn logs(&self, id: &str, tail: u32) -> Result<String, bollard::errors::Error> {
        let options = LogsOptionsBuilder::default()
            .stdout(true)
            .stderr(true)
            .tail(&tail.to_string())
            .build();
        let mut stream = self.inner.logs(id, Some(options));
        let mut out = String::new();
        while let Some(chunk) = stream.next().await {
            out.push_str(&chunk?.to_string());
        }
        Ok(limit_tail(out, MAX_LOG_BYTES))
    }

    /// Lists all containers, running or not, sorted by name.
    pub async fn containers(&self) -> Result<Vec<ContainerInfo>, bollard::errors::Error> {
        let options = ListContainersOptionsBuilder::default().all(true).build();
        let mut list: Vec<ContainerInfo> = self
            .inner
            .list_containers(Some(options))
            .await?
            .into_iter()
            .map(ContainerInfo::from)
            .collect();
        list.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(list)
    }
}

/// Current resource use of one running container.
#[derive(Debug, Clone, PartialEq)]
pub struct ContainerUsage {
    pub name: String,
    /// CPU in percent of one core (can exceed 100 on multi-core use).
    pub cpu_pct: f64,
    /// Memory in use, without reclaimable page cache.
    pub mem_bytes: u64,
}

/// CPU percent from two consecutive counters (Docker's own formula).
pub fn cpu_percent(cpu: u64, pre_cpu: u64, sys: u64, pre_sys: u64, cpus: u32) -> f64 {
    let (dc, ds) = (cpu.saturating_sub(pre_cpu), sys.saturating_sub(pre_sys));
    if ds == 0 {
        return 0.0;
    }
    dc as f64 / ds as f64 * f64::from(cpus.max(1)) * 100.0
}

impl DockerClient {
    /// One stats reading per running container (each takes about a second, so they run together).
    pub async fn usage(&self) -> Result<Vec<ContainerUsage>, bollard::errors::Error> {
        let running = ListContainersOptionsBuilder::default().build();
        let ids: Vec<(String, String)> = self
            .inner
            .list_containers(Some(running))
            .await?
            .into_iter()
            .filter_map(|c| {
                let name = c.names?.into_iter().next()?.trim_start_matches('/').to_string();
                Some((c.id?, name))
            })
            .collect();
        let reads = ids.into_iter().map(|(id, name)| async move {
            let options = StatsOptionsBuilder::default().stream(false).build();
            let r = self.inner.stats(&id, Some(options)).next().await?.ok()?;
            let cpu = r.cpu_stats.as_ref()?;
            let pre = r.precpu_stats.as_ref()?;
            let cpu_pct = cpu_percent(
                cpu.cpu_usage.as_ref()?.total_usage?,
                pre.cpu_usage.as_ref().and_then(|u| u.total_usage).unwrap_or(0),
                cpu.system_cpu_usage?,
                pre.system_cpu_usage.unwrap_or(0),
                cpu.online_cpus.unwrap_or(1),
            );
            let mem = r.memory_stats.as_ref()?;
            let cache = mem
                .stats
                .as_ref()
                .and_then(|s| s.get("inactive_file").or_else(|| s.get("cache")))
                .copied()
                .unwrap_or(0);
            Some(ContainerUsage {
                name,
                cpu_pct,
                mem_bytes: mem.usage?.saturating_sub(cache),
            })
        });
        let mut out: Vec<ContainerUsage> = futures_util::future::join_all(reads)
            .await
            .into_iter()
            .flatten()
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }
}

/// Keeps at most `max` bytes from the end of `s`, cutting on a character boundary.
fn limit_tail(s: String, max: usize) -> String {
    if s.len() <= max {
        return s;
    }
    let mut start = s.len() - max;
    while !s.is_char_boundary(start) {
        start += 1;
    }
    s[start..].to_string()
}

impl From<ContainerSummary> for ContainerInfo {
    fn from(c: ContainerSummary) -> Self {
        let id = c.id.unwrap_or_default();
        let name = c
            .names
            .and_then(|n| n.into_iter().next())
            .map(|n| n.trim_start_matches('/').to_string())
            .unwrap_or_default();
        Self {
            id: id.chars().take(12).collect(),
            name,
            image: c.image.unwrap_or_default(),
            state: c.state.map(|s| s.to_string()).unwrap_or_default(),
            status: c.status.unwrap_or_default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bollard::models::ContainerSummaryStateEnum;

    #[test]
    fn maps_summary_to_info() {
        let summary = ContainerSummary {
            id: Some("0123456789abcdef0123".into()),
            names: Some(vec!["/web".into()]),
            image: Some("nginx:1".into()),
            state: Some(ContainerSummaryStateEnum::RUNNING),
            status: Some("Up 3 hours".into()),
            ..Default::default()
        };
        assert_eq!(
            ContainerInfo::from(summary),
            ContainerInfo {
                id: "0123456789ab".into(),
                name: "web".into(),
                image: "nginx:1".into(),
                state: "running".into(),
                status: "Up 3 hours".into(),
            }
        );
    }

    #[test]
    fn limit_tail_keeps_the_end_on_a_char_boundary() {
        assert_eq!(limit_tail("abcdef".into(), 10), "abcdef");
        assert_eq!(limit_tail("abcdef".into(), 3), "def");
        assert_eq!(limit_tail("aéé".into(), 3), "é"); // cut would split a 2-byte char
    }

    #[test]
    fn parses_actions_and_validates_ids() {
        assert_eq!(Action::parse("stop"), Some(Action::Stop));
        assert_eq!(Action::parse("rm"), None);
        assert!(valid_id("0123456789ab"));
        assert!(!valid_id("web"));
        assert!(!valid_id("../../images/json"));
        assert!(!valid_id("0123456789ab/stop"));
    }

    #[test]
    fn cpu_percent_follows_dockers_formula() {
        assert_eq!(cpu_percent(200, 100, 2000, 1000, 4), 40.0);
        assert_eq!(cpu_percent(5, 5, 10, 10, 4), 0.0); // no system delta
        assert_eq!(cpu_percent(1, 9, 20, 10, 2), 0.0); // counter went backwards
    }

    #[test]
    fn missing_fields_become_empty() {
        let info = ContainerInfo::from(ContainerSummary::default());
        assert_eq!(info.id, "");
        assert_eq!(info.name, "");
        assert_eq!(info.state, "");
    }

    /// Talks to the real daemon; skipped when the socket is not reachable.
    #[tokio::test]
    async fn lists_containers_from_local_daemon() {
        let Ok(client) = DockerClient::connect() else {
            return;
        };
        match client.containers().await {
            Ok(list) => assert!(list.windows(2).all(|w| w[0].name <= w[1].name)),
            Err(e) => eprintln!("skipping, daemon not reachable: {e}"),
        }
    }
}
