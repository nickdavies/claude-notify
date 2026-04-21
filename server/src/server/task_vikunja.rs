use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::thread;
use std::time::Duration;

use anyhow::Context;
use chrono::{DateTime, Utc};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize};
use uuid::Uuid;

fn deserialize_null_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    let opt = Option::deserialize(deserializer)?;
    Ok(opt.unwrap_or_default())
}

use super::task_adapter::{
    HumanActionKind, TaskAdapter, TaskChange, TaskChangeType, TaskCreate, TaskHumanAction,
    TaskProjection, TaskSyncRecord, TaskToolRecord, TaskTriageStatus, TaskUpdate,
};

const VIKUNJA_UUID_NAMESPACE: Uuid = Uuid::from_bytes([
    0x76, 0x69, 0x6b, 0x75, 0x6e, 0x6a, 0x61, 0x2d, 0x74, 0x61, 0x73, 0x6b, 0x2d, 0x6e, 0x73, 0x00,
]);

fn vikunja_task_uuid(base_url: &str, task_id: i64) -> Uuid {
    let key = format!("vikunja:{base_url}:task:{task_id}");
    Uuid::new_v5(&VIKUNJA_UUID_NAMESPACE, key.as_bytes())
}

#[derive(Debug, Deserialize)]
struct VikunjaTask {
    id: i64,
    title: String,
    description: String,
    done: bool,
    priority: i64,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    labels: Vec<VikunjaLabel>,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    assignees: Vec<VikunjaUser>,
    created: String,
    updated: String,
    project_id: i64,
    index: i64,
    identifier: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
struct VikunjaLabel {
    id: i64,
    title: String,
}

#[derive(Debug, Deserialize)]
struct VikunjaUser {
    id: i64,
    username: String,
}

#[derive(Debug, Deserialize)]
struct VikunjaComment {
    id: i64,
    comment: String,
    author: VikunjaUser,
    created: String,
    updated: String,
}

#[derive(Debug, Deserialize)]
struct VikunjaView {
    id: i64,
    title: String,
}

#[derive(Debug, Deserialize, Default)]
struct AgentHubMetadata {
    workflow_id: Option<String>,
    repo: Option<AgentHubRepoMeta>,
}

#[derive(Debug, Deserialize)]
struct AgentHubRepoMeta {
    repo_url: String,
    base_ref: Option<String>,
    branch_name: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct AgentHubMetadataEnvelope {
    #[serde(default)]
    agent_hub: AgentHubMetadata,
}

fn parse_description_metadata(description: &str) -> (String, Option<AgentHubMetadata>) {
    let Some((body, metadata)) = description.rsplit_once("\n---\n") else {
        return (description.to_string(), None);
    };

    let body = body.trim_end().to_string();
    let metadata = metadata.trim();
    if metadata.is_empty() {
        return (body, None);
    }

    let parsed = match serde_yaml::from_str::<AgentHubMetadataEnvelope>(metadata) {
        Ok(doc) => {
            if doc.agent_hub.workflow_id.is_some() || doc.agent_hub.repo.is_some() {
                Some(doc.agent_hub)
            } else {
                None
            }
        }
        Err(err) => {
            tracing::warn!(error = ?err, "failed to parse task description metadata yaml");
            None
        }
    };

    (body, parsed)
}

fn rest_as_reason(parts: &[&str], from: usize) -> Option<String> {
    if parts.len() > from {
        Some(parts[from..].join(" "))
    } else {
        None
    }
}

fn parse_comment_action(comment: &str) -> Option<TaskHumanAction> {
    let line = comment.lines().find_map(|line| {
        let trimmed = line.trim();
        if trimmed
            .get(..3)
            .map(|prefix| prefix.eq_ignore_ascii_case("/ah"))
            .unwrap_or(false)
        {
            Some(trimmed)
        } else {
            None
        }
    })?;

    let command = line.get(3..)?.trim();
    let parts: Vec<&str> = line.trim().get(3..)?.trim().split_whitespace().collect();
    if command.is_empty() {
        return None;
    }

    match parts.first()?.to_lowercase().as_str() {
        "workflow" => Some(TaskHumanAction {
            kind: HumanActionKind::ChooseWorkflow,
            workflow_id: parts.get(1).map(|s| s.to_string()),
            reason: None,
        }),
        "submit" => Some(TaskHumanAction {
            kind: HumanActionKind::Submit,
            workflow_id: None,
            reason: rest_as_reason(&parts, 1),
        }),
        "approve" => Some(TaskHumanAction {
            kind: HumanActionKind::Approve,
            workflow_id: None,
            reason: rest_as_reason(&parts, 1),
        }),
        "request_changes" => Some(TaskHumanAction {
            kind: HumanActionKind::RequestChanges,
            workflow_id: None,
            reason: rest_as_reason(&parts, 1),
        }),
        "unblock" => Some(TaskHumanAction {
            kind: HumanActionKind::Unblock,
            workflow_id: None,
            reason: rest_as_reason(&parts, 1),
        }),
        "retry" => Some(TaskHumanAction {
            kind: HumanActionKind::Retry,
            workflow_id: None,
            reason: rest_as_reason(&parts, 1),
        }),
        "cancel" => Some(TaskHumanAction {
            kind: HumanActionKind::Cancel,
            workflow_id: None,
            reason: rest_as_reason(&parts, 1),
        }),
        _ => None,
    }
}

#[derive(Debug, Serialize)]
struct VikunjaCreateTaskRequest {
    title: String,
    description: String,
}

#[derive(Debug, Serialize)]
struct VikunjaCommentCreateRequest {
    comment: String,
}

#[derive(Debug, Serialize)]
struct VikunjaAddLabelRequest {
    label_id: i64,
}

#[derive(Debug, Serialize)]
struct VikunjaCreateLabelRequest {
    title: String,
}

#[derive(Debug, Serialize)]
struct VikunjaTaskDoneRequest {
    done: bool,
}

#[derive(Debug, Deserialize)]
struct VikunjaTaskCreated {
    id: i64,
}

pub struct VikunjaAdapter {
    client: reqwest::blocking::Client,
    base_url: String,
    token: String,
    project_id: i64,
    view_id: Mutex<Option<i64>>,
    label_prefix: String,
    task_id_cache: Mutex<HashMap<Uuid, i64>>,
    poll_known: Mutex<HashMap<String, DateTime<Utc>>>,
}

impl VikunjaAdapter {
    const RETRY_BACKOFFS: [Duration; 3] = [
        Duration::from_millis(100),
        Duration::from_millis(500),
        Duration::from_millis(2000),
    ];

    pub fn new(
        base_url: String,
        token: String,
        project_id: i64,
        label_prefix: Option<String>,
    ) -> Self {
        let normalized = if base_url.trim().is_empty() {
            "http://localhost:3456".to_string()
        } else {
            base_url.trim_end_matches('/').to_string()
        };

        Self {
            client: std::thread::spawn(|| reqwest::blocking::Client::new())
                .join()
                .expect("Failed to create HTTP client"),
            base_url: normalized,
            token,
            project_id,
            view_id: Mutex::new(None),
            label_prefix: label_prefix.unwrap_or_else(|| "ah:".to_string()),
            task_id_cache: Mutex::new(HashMap::new()),
            poll_known: Mutex::new(HashMap::new()),
        }
    }

    fn api_url(&self, path: &str) -> String {
        let trimmed = path.trim_start_matches('/');
        format!("{}/api/v1/{trimmed}", self.base_url)
    }

    fn api_get<T: DeserializeOwned>(&self, path: &str) -> anyhow::Result<T> {
        let url = self.api_url(path);
        let response = self.send_with_retry("GET", path, &url, || {
            self.client.get(&url).bearer_auth(&self.token).send()
        })?;
        Self::decode_response(response, "GET", path, &url)
    }

    fn api_put<B: Serialize, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> anyhow::Result<T> {
        let url = self.api_url(path);
        let response = self.send_with_retry("PUT", path, &url, || {
            self.client
                .put(&url)
                .bearer_auth(&self.token)
                .json(body)
                .send()
        })?;
        Self::decode_response(response, "PUT", path, &url)
    }

    fn api_post<B: Serialize, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> anyhow::Result<T> {
        let url = self.api_url(path);
        let response = self.send_with_retry("POST", path, &url, || {
            self.client
                .post(&url)
                .bearer_auth(&self.token)
                .json(body)
                .send()
        })?;
        Self::decode_response(response, "POST", path, &url)
    }

    fn api_delete(&self, path: &str) -> anyhow::Result<()> {
        let url = self.api_url(path);
        let response = self.send_with_retry("DELETE", path, &url, || {
            self.client.delete(&url).bearer_auth(&self.token).send()
        })?;

        if response.status().is_success() {
            return Ok(());
        }

        let status = response.status();
        let body = response.text().unwrap_or_default();
        Err(anyhow::anyhow!(
            "DELETE path {path} ({url}) failed with status {status}: {body}"
        ))
    }

    fn send_with_retry<F>(
        &self,
        method: &str,
        path: &str,
        url: &str,
        mut send: F,
    ) -> anyhow::Result<reqwest::blocking::Response>
    where
        F: FnMut() -> Result<reqwest::blocking::Response, reqwest::Error>,
    {
        for (attempt, backoff) in Self::RETRY_BACKOFFS.iter().enumerate() {
            match send() {
                Ok(response) if response.status().is_server_error() => {
                    tracing::warn!(
                        method,
                        path,
                        url,
                        status = %response.status(),
                        retry_attempt = attempt + 1,
                        "request returned 5xx, retrying"
                    );
                    thread::sleep(*backoff);
                }
                Ok(response) => return Ok(response),
                Err(error) if error.is_connect() || error.is_timeout() => {
                    tracing::warn!(
                        method,
                        path,
                        url,
                        retry_attempt = attempt + 1,
                        error = ?error,
                        "request failed due to connection issue, retrying"
                    );
                    thread::sleep(*backoff);
                }
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("{method} request failed for path {path} ({url})")
                    });
                }
            }
        }

        let response = send().with_context(|| {
            format!("{method} request failed for path {path} ({url}) after retries")
        })?;

        if response.status().is_server_error() {
            let status = response.status();
            let body = response.text().unwrap_or_default();
            return Err(anyhow::anyhow!(
                "{method} path {path} ({url}) failed after retries with status {status}: {body}"
            ));
        }

        Ok(response)
    }

    fn decode_response<T: DeserializeOwned>(
        response: reqwest::blocking::Response,
        method: &str,
        path: &str,
        url: &str,
    ) -> anyhow::Result<T> {
        let status = response.status();
        if !status.is_success() {
            let body = response.text().unwrap_or_default();
            return Err(anyhow::anyhow!(
                "{method} path {path} ({url}) failed with status {status}: {body}"
            ));
        }

        response.json::<T>().with_context(|| {
            format!(
                "failed to decode response body for {method} path {path} ({url}), status {status}"
            )
        })
    }

    fn resolve_view_id(&self) -> anyhow::Result<i64> {
        let mut guard = self
            .view_id
            .lock()
            .map_err(|_| anyhow::anyhow!("vikunja view id lock poisoned"))?;
        if let Some(view_id) = *guard {
            return Ok(view_id);
        }

        let views: Vec<VikunjaView> =
            self.api_get(&format!("projects/{}/views", self.project_id))?;
        let first = views
            .first()
            .ok_or_else(|| anyhow::anyhow!("project {} has no views", self.project_id))?;
        *guard = Some(first.id);
        Ok(first.id)
    }

    fn get_task_comments(&self, task_id: i64) -> anyhow::Result<Vec<VikunjaComment>> {
        self.api_get(&format!("tasks/{task_id}/comments"))
    }

    fn get_or_create_label(&self, title: &str) -> anyhow::Result<i64> {
        let labels: Vec<VikunjaLabel> = self.api_get("labels")?;
        if let Some(existing) = labels.into_iter().find(|l| l.title == title) {
            return Ok(existing.id);
        }

        let created: VikunjaLabel = self.api_put(
            "labels",
            &VikunjaCreateLabelRequest {
                title: title.to_string(),
            },
        )?;
        Ok(created.id)
    }

    fn set_task_labels(
        &self,
        task_id: i64,
        desired: &[String],
        current: &[VikunjaLabel],
    ) -> anyhow::Result<()> {
        let desired_set = desired.iter().cloned().collect::<HashSet<_>>();
        let current_map = current
            .iter()
            .map(|label| (label.title.clone(), label.id))
            .collect::<HashMap<_, _>>();

        for title in desired_set.difference(&current_map.keys().cloned().collect()) {
            let label_id = self.get_or_create_label(title)?;
            let _: serde_json::Value = self.api_put(
                &format!("tasks/{task_id}/labels"),
                &VikunjaAddLabelRequest { label_id },
            )?;
        }

        for label in current {
            if !desired_set.contains(&label.title) {
                self.api_delete(&format!("tasks/{task_id}/labels/{}", label.id))?;
            }
        }

        Ok(())
    }

    fn vikunja_task_to_record(
        &self,
        task: &VikunjaTask,
        comments: &[VikunjaComment],
    ) -> TaskToolRecord {
        let uuid = vikunja_task_uuid(&self.base_url, task.id);
        let (clean_desc, metadata) = parse_description_metadata(&task.description);
        let labels: Vec<String> = task.labels.iter().map(|l| l.title.clone()).collect();

        let requested_action = comments
            .iter()
            .rev()
            .find_map(|c| parse_comment_action(&c.comment));
        let action_revision = comments
            .iter()
            .rev()
            .find(|c| parse_comment_action(&c.comment).is_some())
            .map(|c| c.id as u64)
            .unwrap_or(0);

        let context = serde_json::json!({
            "vikunja_task_id": task.id,
            "vikunja_project_id": task.project_id,
            "vikunja_index": task.index,
            "vikunja_identifier": task.identifier,
            "labels": labels,
            "assignees": task.assignees.iter().map(|a| &a.username).collect::<Vec<_>>(),
            "done": task.done,
            "priority": task.priority,
            "vikunja_created": task.created,
            "vikunja_updated": task.updated,
        });

        let workflow_id = metadata.as_ref().and_then(|m| m.workflow_id.clone());
        let repo = metadata
            .as_ref()
            .and_then(|m| m.repo.as_ref())
            .map(|r| protocol::RepoSource {
                repo_url: r.repo_url.clone(),
                base_ref: r
                    .base_ref
                    .clone()
                    .unwrap_or_else(|| "origin/main".to_string()),
                branch_name: r.branch_name.clone(),
            });

        let needs_triage = labels.iter().any(|l| l == "needs-triage");
        let triage_status = if needs_triage || workflow_id.is_none() {
            TaskTriageStatus::NeedsWorkflowSelection
        } else {
            TaskTriageStatus::Ready
        };

        TaskToolRecord {
            id: uuid,
            title: task.title.clone(),
            description: Some(clean_desc),
            workflow_id,
            triage_status,
            pending_human_action: needs_triage,
            current_human_gate: None,
            workflow_phase: None,
            blocked_reason: None,
            human_gate_state: None,
            valid_next_events: vec![],
            allowed_actions: vec![],
            context,
            repo,
            updated_at: chrono::DateTime::parse_from_rfc3339(&task.updated)
                .map(|dt| dt.with_timezone(&chrono::Utc))
                .unwrap_or_else(|_| chrono::Utc::now()),
            requested_action,
            action_revision,
        }
    }

    fn cache_task_id(&self, task_uuid: Uuid, vikunja_task_id: i64) -> anyhow::Result<()> {
        let mut cache = self
            .task_id_cache
            .lock()
            .map_err(|_| anyhow::anyhow!("vikunja task id cache lock poisoned"))?;
        cache.insert(task_uuid, vikunja_task_id);
        Ok(())
    }

    fn extract_vikunja_task_id(context: &serde_json::Value) -> Option<i64> {
        context
            .get("vikunja_task_id")
            .and_then(serde_json::Value::as_i64)
    }

    fn resolve_task_numeric_id(&self, task_id: &str) -> anyhow::Result<i64> {
        if let Ok(numeric) = task_id.parse::<i64>() {
            return Ok(numeric);
        }

        if let Ok(uuid) = Uuid::parse_str(task_id)
            && let Ok(cache) = self.task_id_cache.lock()
            && let Some(id) = cache.get(&uuid)
        {
            return Ok(*id);
        }

        let records = self.pull_tasks()?;
        let found = records
            .into_iter()
            .find(|r| r.external.id.to_string() == task_id)
            .and_then(|r| Self::extract_vikunja_task_id(&r.external.context));
        found.ok_or_else(|| anyhow::anyhow!("unable to resolve vikunja task id for {task_id}"))
    }

    fn projection_comment(&self, task: &TaskToolRecord) -> String {
        let state_info = self.projection_state_info(task);
        format!("agent-hub projection update\n\n{state_info}")
    }

    fn projection_state_info(&self, task: &TaskToolRecord) -> String {
        let triage_status = serde_json::to_value(&task.triage_status)
            .ok()
            .and_then(|v| v.as_str().map(ToOwned::to_owned))
            .unwrap_or_else(|| "unknown".to_string());

        let phase = task
            .workflow_phase
            .clone()
            .unwrap_or_else(|| "unknown".to_string());

        format!(
            "- phase: {phase}\n- triage_status: {triage_status}\n- pending_human_action: {}\n- blocked_reason: {}",
            task.pending_human_action,
            task.blocked_reason
                .clone()
                .unwrap_or_else(|| "none".to_string())
        )
    }
}

impl TaskAdapter for VikunjaAdapter {
    fn adapter_name(&self) -> &'static str {
        "vikunja"
    }

    fn pull_tasks(&self) -> anyhow::Result<Vec<TaskSyncRecord>> {
        let view_id = self.resolve_view_id()?;
        let per_page = 200;
        let mut page = 1;
        let mut tasks = Vec::new();
        loop {
            let mut page_tasks: Vec<VikunjaTask> = self.api_get(&format!(
                "projects/{}/views/{view_id}/tasks?page={page}&per_page={per_page}",
                self.project_id
            ))?;
            let page_len = page_tasks.len();
            tasks.append(&mut page_tasks);

            if page_len < per_page {
                break;
            }
            page += 1;
        }

        let mut records = Vec::with_capacity(tasks.len());
        for task in tasks {
            let comments = self.get_task_comments(task.id)?;
            let record = self.vikunja_task_to_record(&task, &comments);
            self.cache_task_id(record.id, task.id)?;
            records.push(TaskSyncRecord { external: record });
        }
        Ok(records)
    }

    fn get_task(&self, id: Uuid) -> anyhow::Result<Option<TaskToolRecord>> {
        let tasks = self.pull_tasks()?;
        Ok(tasks
            .into_iter()
            .map(|t| t.external)
            .find(|task| task.id == id))
    }

    fn persist_projection(&self, projection: &TaskProjection) -> anyhow::Result<()> {
        let task_id = Self::extract_vikunja_task_id(&projection.task.context)
            .ok_or_else(|| anyhow::anyhow!("missing vikunja_task_id in task context"))?;

        let state_info = self.projection_state_info(&projection.task);
        let comments = self.get_task_comments(task_id)?;
        let should_add_comment = comments
            .last()
            .map(|comment| !comment.comment.contains(&state_info))
            .unwrap_or(true);

        if should_add_comment {
            let _: serde_json::Value = self.api_put(
                &format!("tasks/{task_id}/comments"),
                &VikunjaCommentCreateRequest {
                    comment: self.projection_comment(&projection.task),
                },
            )?;
        }

        let current_labels: Vec<VikunjaLabel> = self.api_get(&format!("tasks/{task_id}/labels"))?;
        let state_prefix = format!("{}state:", self.label_prefix);
        let mut desired = current_labels
            .iter()
            .map(|label| label.title.clone())
            .filter(|title| !title.starts_with(&state_prefix))
            .collect::<Vec<_>>();

        let phase = projection
            .task
            .workflow_phase
            .clone()
            .unwrap_or_else(|| "unknown".to_string());
        desired.push(format!("{}state:{phase}", self.label_prefix));
        self.set_task_labels(task_id, &desired, &current_labels)?;

        let terminal_comment = match projection.task.workflow_phase.as_deref() {
            Some("completed") => Some("✅ Task completed — work item reached terminal state"),
            Some("failed") => Some("❌ Task failed — work item reached terminal state"),
            Some("cancelled") => Some("❌ Task cancelled"),
            _ => match projection.task.triage_status {
                TaskTriageStatus::Completed => {
                    Some("✅ Task completed — work item reached terminal state")
                }
                TaskTriageStatus::Cancelled => Some("❌ Task cancelled"),
                _ => None,
            },
        };

        if let Some(terminal_comment) = terminal_comment {
            let already_done = projection
                .task
                .context
                .get("done")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);

            if !already_done {
                let _: serde_json::Value = self.api_post(
                    &format!("tasks/{task_id}"),
                    &VikunjaTaskDoneRequest { done: true },
                )?;
            }

            let has_terminal_comment = comments
                .iter()
                .any(|comment| comment.comment.contains(terminal_comment));

            if !has_terminal_comment {
                let _: serde_json::Value = self.api_put(
                    &format!("tasks/{task_id}/comments"),
                    &VikunjaCommentCreateRequest {
                        comment: terminal_comment.to_string(),
                    },
                )?;
            }
        }

        Ok(())
    }

    fn update_task(&self, task_id: &str, update: TaskUpdate) -> anyhow::Result<()> {
        let vikunja_task_id = self.resolve_task_numeric_id(task_id)?;

        if let Some(labels) = update.labels {
            let current_labels: Vec<VikunjaLabel> =
                self.api_get(&format!("tasks/{vikunja_task_id}/labels"))?;
            self.set_task_labels(vikunja_task_id, &labels, &current_labels)?;
        }

        if let Some(status) = update.status {
            let _: serde_json::Value = self.api_put(
                &format!("tasks/{vikunja_task_id}/comments"),
                &VikunjaCommentCreateRequest {
                    comment: format!("agent-hub status updated to: {status}"),
                },
            )?;
        }

        Ok(())
    }

    fn create_task(&self, create: TaskCreate) -> anyhow::Result<String> {
        let created: VikunjaTaskCreated = self.api_put(
            &format!("projects/{}/tasks", self.project_id),
            &VikunjaCreateTaskRequest {
                title: create.title,
                description: create.description,
            },
        )?;

        if !create.labels.is_empty() {
            let current_labels: Vec<VikunjaLabel> =
                self.api_get(&format!("tasks/{}/labels", created.id))?;
            self.set_task_labels(created.id, &create.labels, &current_labels)?;
        }

        let uuid = vikunja_task_uuid(&self.base_url, created.id);
        self.cache_task_id(uuid, created.id)?;
        Ok(uuid.to_string())
    }

    fn poll_changes(&self) -> anyhow::Result<Vec<TaskChange>> {
        let current_records = self.pull_tasks()?;
        let mut known = self
            .poll_known
            .lock()
            .map_err(|_| anyhow::anyhow!("vikunja poll index lock poisoned"))?;

        let mut current = HashMap::new();
        let mut changes = Vec::new();

        for record in &current_records {
            let task_id = record.external.id.to_string();
            current.insert(task_id.clone(), record.external.updated_at);

            match known.get(&task_id) {
                None => changes.push(TaskChange {
                    task_id,
                    change_type: TaskChangeType::Created,
                }),
                Some(previous) if record.external.updated_at > *previous => {
                    changes.push(TaskChange {
                        task_id,
                        change_type: TaskChangeType::Updated,
                    })
                }
                _ => {}
            }
        }

        for task_id in known.keys() {
            if !current.contains_key(task_id) {
                changes.push(TaskChange {
                    task_id: task_id.clone(),
                    change_type: TaskChangeType::Deleted,
                });
            }
        }

        *known = current;
        Ok(changes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_vikunja_task_uuid_is_stable() {
        let first = vikunja_task_uuid("http://localhost:3456", 123);
        let second = vikunja_task_uuid("http://localhost:3456", 123);
        let different = vikunja_task_uuid("http://localhost:3456", 124);

        assert_eq!(first, second);
        assert_ne!(first, different);
    }

    #[test]
    fn test_parse_description_metadata() {
        let input = "Task description\n---\nagent_hub:\n  workflow_id: test_wf\n";
        let (desc, metadata) = parse_description_metadata(input);

        assert_eq!(desc, "Task description");
        let metadata = metadata.expect("metadata should parse");
        assert_eq!(metadata.workflow_id.as_deref(), Some("test_wf"));
    }

    #[test]
    fn test_parse_description_no_metadata() {
        let (desc, metadata) = parse_description_metadata("plain description");
        assert_eq!(desc, "plain description");
        assert!(metadata.is_none());
    }

    #[test]
    fn test_parse_comment_action_approve() {
        let action = parse_comment_action("/ah approve some reason").expect("action expected");
        assert_eq!(action.kind, HumanActionKind::Approve);
        assert_eq!(action.reason.as_deref(), Some("some reason"));
        assert!(action.workflow_id.is_none());
    }

    #[test]
    fn test_parse_comment_action_workflow() {
        let action = parse_comment_action("/ah workflow my_wf").expect("action expected");
        assert_eq!(action.kind, HumanActionKind::ChooseWorkflow);
        assert_eq!(action.workflow_id.as_deref(), Some("my_wf"));
        assert!(action.reason.is_none());
    }

    #[test]
    fn test_parse_comment_action_unknown() {
        assert!(parse_comment_action("/ah unknown").is_none());
    }

    #[test]
    fn test_parse_comment_action_not_first_line() {
        let comment = "first line\n/ah approve looks good";
        let action = parse_comment_action(comment).expect("action expected");
        assert_eq!(action.kind, HumanActionKind::Approve);
        assert_eq!(action.reason.as_deref(), Some("looks good"));
    }

    #[test]
    fn test_parse_description_metadata_uses_last_separator() {
        let input =
            "Task description\n---\nnot: metadata\n---\nagent_hub:\n  workflow_id: test_wf\n";
        let (desc, metadata) = parse_description_metadata(input);

        assert_eq!(desc, "Task description\n---\nnot: metadata");
        let metadata = metadata.expect("metadata should parse");
        assert_eq!(metadata.workflow_id.as_deref(), Some("test_wf"));
    }

    #[test]
    fn test_parse_description_metadata_empty_block() {
        let input = "Task description\n---\n   \n";
        let (desc, metadata) = parse_description_metadata(input);

        assert_eq!(desc, "Task description");
        assert!(metadata.is_none());
    }

    #[test]
    fn test_parse_comment_action_with_whitespace_and_case_insensitive_command() {
        let action =
            parse_comment_action("  /ah    ApPrOvE   looks   good ").expect("action expected");
        assert_eq!(action.kind, HumanActionKind::Approve);
        assert_eq!(action.reason.as_deref(), Some("looks good"));
    }
}
