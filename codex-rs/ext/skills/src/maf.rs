use std::sync::Arc;

use serde::Deserialize;
use serde::Serialize;

use crate::HostSkillProvider;
use crate::HostSkillsSnapshot;
use crate::SkillProvider;
use crate::catalog::SkillCatalog;
use crate::provider::SkillListQuery;
use crate::provider::SkillReadContext;
use crate::provider::SkillReadRequest;

pub const PROTOCOL_VERSION: u32 = 1;
pub const MAX_MESSAGE_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_CONTENT_BYTES: usize = 256 * 1024;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Authority {
    pub kind: String,
    pub id: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SnapshotEntry {
    pub authority: Authority,
    pub package_id: String,
    pub resource_id: String,
    pub name: String,
    pub description: String,
    pub short_description: Option<String>,
    pub enabled: bool,
    pub prompt_visible: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Snapshot {
    pub version: u32,
    pub snapshot_id: String,
    pub entries: Vec<SnapshotEntry>,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReadRequest {
    pub version: u32,
    #[serde(rename = "type")]
    pub kind: String,
    pub request_id: String,
    pub snapshot_id: String,
    pub authority: Authority,
    pub package_id: String,
    pub resource_id: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReadResponse {
    pub version: u32,
    #[serde(rename = "type")]
    pub kind: String,
    pub request_id: String,
    pub snapshot_id: String,
    pub contents: Option<String>,
    pub error: Option<String>,
}

pub struct HostSnapshotBridge {
    snapshot: Arc<HostSkillsSnapshot>,
    catalog: SkillCatalog,
    wire: Snapshot,
}

impl HostSnapshotBridge {
    pub async fn new(
        snapshot_id: String,
        snapshot: Arc<HostSkillsSnapshot>,
    ) -> Result<Self, String> {
        if snapshot_id.is_empty() || snapshot_id.len() > 128 {
            return Err("invalid_snapshot_id".into());
        }
        let catalog = HostSkillProvider::new()
            .list(SkillListQuery {
                turn_id: snapshot_id.clone(),
                executor_roots: Vec::new(),
                resolved_executor_roots: Vec::new(),
                host_snapshot: Some(Arc::clone(&snapshot)),
                include_host_skills: true,
                include_bundled_skills: false,
                include_cloud_skills: false,
                mcp_resources: None,
                executor_capability_discovery: None,
            })
            .await
            .map_err(|_| "catalog_unavailable".to_string())?;
        let wire = Snapshot {
            version: PROTOCOL_VERSION,
            snapshot_id,
            entries: catalog
                .entries
                .iter()
                .map(|entry| SnapshotEntry {
                    authority: Authority {
                        kind: entry.authority.kind.to_string(),
                        id: entry.authority.id.clone(),
                    },
                    package_id: entry.id.0.clone(),
                    resource_id: entry.main_prompt.as_str().to_string(),
                    name: entry.name.clone(),
                    description: entry.description.clone(),
                    short_description: entry.short_description.clone(),
                    enabled: entry.enabled,
                    prompt_visible: entry.prompt_visible,
                })
                .collect(),
            warnings: catalog.warnings.clone(),
        };
        if wire.entries.len() > 1024
            || serde_json::to_vec(&wire)
                .map_err(|_| "invalid_catalog")?
                .len()
                > MAX_MESSAGE_BYTES
        {
            return Err("catalog_too_large".into());
        }
        Ok(Self {
            snapshot,
            catalog,
            wire,
        })
    }

    pub fn snapshot(&self) -> &Snapshot {
        &self.wire
    }

    pub async fn read(&self, request: ReadRequest) -> ReadResponse {
        let result = self.read_contents(&request).await;
        let (contents, error) = match result {
            Ok(contents) => (Some(contents), None),
            Err(error) => (None, Some(error)),
        };
        ReadResponse {
            version: PROTOCOL_VERSION,
            kind: "readResult".into(),
            request_id: request.request_id.chars().take(128).collect(),
            snapshot_id: self.wire.snapshot_id.clone(),
            contents,
            error,
        }
    }

    async fn read_contents(&self, request: &ReadRequest) -> Result<String, String> {
        if request.version != PROTOCOL_VERSION
            || request.kind != "read"
            || request.request_id.is_empty()
            || request.request_id.len() > 128
        {
            return Err("invalid_request".into());
        }
        if request.snapshot_id != self.wire.snapshot_id {
            return Err("stale_snapshot".into());
        }
        let entry = self
            .catalog
            .entries
            .iter()
            .find(|entry| {
                entry.enabled
                    && entry.prompt_visible
                    && entry.authority.kind.to_string() == request.authority.kind
                    && entry.authority.id == request.authority.id
                    && entry.id.0 == request.package_id
                    && entry.main_prompt.as_str() == request.resource_id
            })
            .ok_or("resource_not_allowed")?;
        let provider = HostSkillProvider::new();
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            provider.read(SkillReadRequest {
                authority: entry.authority.clone(),
                package: entry.id.clone(),
                resource: entry.main_prompt.clone(),
                context: SkillReadContext::Host {
                    host_snapshot: Some(Arc::clone(&self.snapshot)),
                },
            }),
        )
        .await
        .map_err(|_| "resource_read_timeout")?
        .map_err(|_| "resource_read_failed")?;
        if result.contents.len() > MAX_CONTENT_BYTES {
            return Err("resource_too_large".into());
        }
        Ok(result.contents)
    }
}

#[cfg(test)]
#[path = "maf_tests.rs"]
mod tests;
