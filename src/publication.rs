use crate::state::{StatePaths, read_json, write_json};
use anyhow::{Context, Result, bail};
use chrono::Utc;
use reqwest::StatusCode;
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::time::Duration;
use uuid::Uuid;

pub const PENDING: &str = "PENDING";
pub const ACKNOWLEDGED: &str = "ACKNOWLEDGED";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PublicationAck {
    pub publication_id: String,
    pub attempt: u64,
    pub project_id: String,
    pub branch: String,
    pub accepted_oid: String,
    pub remote_head: String,
    pub project_generation: u64,
    pub durable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Publication {
    pub publication_id: String,
    pub project_id: String,
    #[serde(default)]
    pub checkout_id: Option<String>,
    pub branch: String,
    pub source_commit_oid: String,
    pub candidate_commit_oid: String,
    #[serde(default)]
    pub expected_remote_oid: Option<String>,
    pub attempt: u64,
    pub state: String,
    pub created_at: String,
    #[serde(default)]
    pub last_attempt_at: Option<String>,
    #[serde(default)]
    pub next_attempt_at: Option<String>,
    #[serde(default)]
    pub last_error: Option<String>,
    #[serde(default)]
    pub ack: Option<PublicationAck>,
}

impl Publication {
    pub fn new(
        project_id: &str,
        checkout_id: Option<String>,
        branch: &str,
        commit_oid: &str,
        expected_remote_oid: Option<String>,
    ) -> Self {
        Self {
            publication_id: format!("pub_{}", &Uuid::new_v4().simple().to_string()[..20]),
            project_id: project_id.into(),
            checkout_id,
            branch: branch.into(),
            source_commit_oid: commit_oid.into(),
            candidate_commit_oid: commit_oid.into(),
            expected_remote_oid,
            attempt: 1,
            state: PENDING.into(),
            created_at: Utc::now().to_rfc3339(),
            last_attempt_at: None,
            next_attempt_at: Some(Utc::now().to_rfc3339()),
            last_error: None,
            ack: None,
        }
    }

    pub fn pending(&self) -> bool {
        self.state != ACKNOWLEDGED
    }

    pub fn due(&self) -> bool {
        self.pending()
            && self
                .next_attempt_at
                .as_deref()
                .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
                .is_none_or(|value| value <= Utc::now())
    }

    pub fn record_failure(&mut self, error: &anyhow::Error, retry: Duration) {
        let now = Utc::now();
        self.state = PENDING.into();
        self.last_attempt_at = Some(now.to_rfc3339());
        self.next_attempt_at = Some(
            (now + chrono::Duration::from_std(retry).unwrap_or(chrono::Duration::minutes(1)))
                .to_rfc3339(),
        );
        self.last_error = Some(format!("{error:#}"));
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicationQueue {
    #[serde(default = "queue_version")]
    pub version: u32,
    #[serde(default)]
    pub publications: Vec<Publication>,
}

fn queue_version() -> u32 {
    1
}

impl PublicationQueue {
    pub fn load(paths: &StatePaths) -> Result<Self> {
        read_json(&queue_path(paths), Self::default())
    }

    pub fn persist(&self, paths: &StatePaths) -> Result<()> {
        write_json(&queue_path(paths), self, 0o600)
    }

    pub fn compact(&mut self) {
        let completed = self
            .publications
            .iter()
            .filter(|value| !value.pending())
            .count();
        if completed <= 256 {
            return;
        }
        let mut remove = completed - 256;
        self.publications.retain(|value| {
            if remove > 0 && !value.pending() {
                remove -= 1;
                false
            } else {
                true
            }
        });
    }
}

impl Default for PublicationQueue {
    fn default() -> Self {
        Self {
            version: queue_version(),
            publications: Vec::new(),
        }
    }
}

pub fn queue_path(paths: &StatePaths) -> PathBuf {
    paths.root.join("publications.json")
}

pub enum ProviderPublication {
    Acknowledged(PublicationAck),
    RemoteAdvanced {
        remote_head: Option<String>,
        project_generation: u64,
    },
}

pub fn acknowledge_with_provider(
    service: &str,
    token: &str,
    publication: &Publication,
) -> Result<ProviderPublication> {
    let url = url::Url::parse(service)?.join(&format!(
        "/v1/projects/{}/publications",
        publication.project_id
    ))?;
    let response = Client::new()
        .post(url)
        .bearer_auth(token)
        .json(&json!({
            "publication_id": publication.publication_id,
            "attempt": publication.attempt,
            "branch": publication.branch,
            "candidate_oid": publication.candidate_commit_oid,
            "expected_remote_oid": publication.expected_remote_oid
        }))
        .send()
        .context("send publication acknowledgement request")?;
    let status = response.status();
    let body: Value = response
        .json()
        .context("provider returned an invalid publication response")?;
    if status.is_success() {
        let ack: PublicationAck = serde_json::from_value(body)?;
        if !ack.durable
            || ack.publication_id != publication.publication_id
            || ack.attempt != publication.attempt
            || ack.project_id != publication.project_id
            || ack.branch != publication.branch
            || ack.accepted_oid != publication.candidate_commit_oid
        {
            bail!("provider returned an invalid durability ACK");
        }
        return Ok(ProviderPublication::Acknowledged(ack));
    }
    if status == StatusCode::CONFLICT
        && body.get("outcome").and_then(Value::as_str) == Some("REMOTE_ADVANCED")
    {
        return Ok(ProviderPublication::RemoteAdvanced {
            remote_head: body
                .get("remote_head")
                .and_then(Value::as_str)
                .map(str::to_owned),
            project_generation: body
                .get("project_generation")
                .and_then(Value::as_u64)
                .unwrap_or_default(),
        });
    }
    bail!(
        "{}",
        body.get("error")
            .and_then(Value::as_str)
            .unwrap_or("provider rejected publication")
    )
}
