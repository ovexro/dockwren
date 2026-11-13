//! Thin wrapper over the Docker Engine API (bollard). Only what the UI needs.

use bollard::{Docker, models::ContainerSummary, query_parameters::ListContainersOptionsBuilder};

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
