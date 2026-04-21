use std::collections::{HashMap, HashSet};
use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::response::sse::{Event, KeepAlive, Sse};
use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use protocol::{
    AgentOutput, BlockReason, CanonicalRuntimeSpec, CreateWorkItemRequest, EventSource,
    FireEventRequest, HeartbeatRequest, Job, JobState, JobUpdateRequest, MergePolicy,
    NetworkResourceRequest, PlannedEnvironmentTier, PollJobRequest, PollJobResponse,
    ReactiveTerminalOutcome, RegisterWorkerRequest, RepoSource, WorkItem, WorkItemState,
    WorkerInfo, WorkerState, WorkflowDefinition, WorkflowDocument, WorkflowEvent,
    WorkflowEventKind, WorkflowEventPayload, WorkflowStepKind, canonical_environment_for_execution,
    canonical_permissions_for_environment, canonical_runtime_spec_for_job,
    canonical_runtime_spec_for_job_effective, default_approval_rules_for_tier,
    legacy_network_label_for_request, network_request_from_legacy_label,
    project_runtime_spec_into_job_context, validate_agent_output_next_event,
    workflow_event_is_externally_fireable,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use tokio::process::Command;
use tokio::sync::OnceCell;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;
use uuid::Uuid;

#[cfg(test)]
use super::task_adapter::task_from_work_item;
use super::task_adapter::{
    AdapterIntent, HumanActionKind, TaskAdapter, TaskHumanGateState, TaskProjection,
    TaskToolRecord, TaskTriageStatus, action_to_fire_event, choose_workflow_id, infer_intents,
    legacy_human_gate_valid_next_events, task_from_work_item_with_valid_next_events,
    task_requires_manual_triage, work_item_from_create, work_item_requires_manual_triage,
};
#[cfg(test)]
use super::task_files::TaskFileStore;
use super::workflows::WorkflowCatalog;
use crate::error::AppError;

const LEASE_TTL_SECS: i64 = 120;
const STALE_WORKER_SECS: u64 = 90;
const CONTEXT_JOB_OUTPUTS_KEY: &str = "job_outputs";
const CONTEXT_JOB_RESULTS_KEY: &str = "job_results";
const CONTEXT_JOB_ARTIFACT_PATHS_KEY: &str = "job_artifact_paths";
const CONTEXT_JOB_ARTIFACT_METADATA_KEY: &str = "job_artifact_metadata";
const CONTEXT_JOB_ARTIFACT_DATA_KEY: &str = "job_artifact_data";
const CONTEXT_WORKFLOW_KEY: &str = "workflow";
const CONTEXT_WORKFLOW_INPUTS_KEY: &str = "inputs";
const CONTEXT_VALID_NEXT_EVENTS_KEY: &str = "valid_next_events";
const CONTEXT_WORK_ITEM_KEY: &str = "work_item";
const CONTEXT_JOB_EXECUTION_KIND_KEY: &str = "job_execution_kind";
const CONTEXT_MANAGED_OPERATION_KEY: &str = "managed_operation";
const CONTEXT_REPO_CATALOG_KEY: &str = "repo_catalog";
const CONTEXT_REPO_CATALOG_HINT_KEY: &str = "repo_catalog_hint";
const SIMPLE_CODE_CHANGE_WORKFLOW_ID: &str = "simple-code-change";
const SIMPLE_CODE_CHANGE_TRIAGE_STATE_ID: &str = "triage_investigation";
const SIMPLE_CODE_CHANGE_REPO_CATALOG_HINT: &str = "- https://github.com/nickdavies/agent-hub-poc-test.git — proof-path repository for validating simple-code-change repo discovery";
pub(crate) const CONTEXT_GITHUB_PR_STATE_KEY: &str = "github_pr_state";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum GithubPrLifecycleState {
    Open,
    Closed,
    Merged,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct GithubPrState {
    pub url: String,
    pub number: u64,
    #[serde(default)]
    pub head_branch: Option<String>,
    pub state: GithubPrLifecycleState,
    #[serde(default)]
    pub approval_count: u32,
    #[serde(default)]
    pub approved: bool,
    #[serde(default)]
    pub passing_checks: Vec<String>,
    #[serde(default)]
    pub checks_passing: bool,
    #[serde(default)]
    pub changes_requested: bool,
    #[serde(default)]
    pub merge_policy: Option<MergePolicy>,
    #[serde(default)]
    pub merge_attempted: bool,
    pub mergeable: bool,
    pub last_updated: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub(crate) struct MergeReadiness {
    pub ready: bool,
    pub is_open: bool,
    pub approved: bool,
    pub checks_passing: bool,
    pub no_changes_requested: bool,
}

#[derive(Debug, Clone, Copy)]
enum DbTable {
    WorkItems,
    Jobs,
}

impl DbTable {
    fn as_str(self) -> &'static str {
        match self {
            Self::WorkItems => "work_items",
            Self::Jobs => "jobs",
        }
    }
}

#[derive(Debug)]
enum JobTransitionError {
    InvalidTransition { from: JobState, to: JobState },
}

#[derive(Debug)]
enum InterpolationError {
    UnterminatedPlaceholder,
    MissingPath(String),
    NonScalarValue(String),
}

#[derive(Debug, Clone, Copy)]
enum EventIngress {
    ExternalPublic,
    TrustedInternal,
}

impl EventIngress {
    fn label(self) -> &'static str {
        match self {
            Self::ExternalPublic => "external-public",
            Self::TrustedInternal => "trusted-internal",
        }
    }
}

impl std::fmt::Display for InterpolationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnterminatedPlaceholder => write!(f, "unterminated placeholder in template"),
            Self::MissingPath(path) => write!(f, "missing interpolation path: {path}"),
            Self::NonScalarValue(path) => write!(f, "interpolation path is non-scalar: {path}"),
        }
    }
}

impl std::error::Error for InterpolationError {}

impl std::fmt::Display for JobTransitionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidTransition { from, to } => {
                write!(f, "invalid job state transition: {from} -> {to}")
            }
        }
    }
}

#[derive(Clone)]
pub struct OrchestrationService {
    pub workflows: Arc<WorkflowCatalog>,
    pub task_adapter: Arc<dyn TaskAdapter>,
    db_path: Arc<PathBuf>,
    init_once: Arc<OnceCell<()>>,
}

impl OrchestrationService {
    pub fn new(
        workflows: Arc<WorkflowCatalog>,
        task_adapter: Arc<dyn TaskAdapter>,
        db_path: String,
    ) -> Self {
        Self {
            workflows,
            task_adapter,
            db_path: Arc::new(PathBuf::from(db_path)),
            init_once: Arc::new(OnceCell::new()),
        }
    }

    pub async fn init(&self) -> anyhow::Result<()> {
        self.init_once
            .get_or_try_init(|| async {
                let path = Arc::clone(&self.db_path);
                tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
                    if let Some(parent) = path.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    let conn = Connection::open(path.as_path())?;
                    conn.execute_batch(
                        r#"
                    CREATE TABLE IF NOT EXISTS work_items (
                        id TEXT PRIMARY KEY,
                        workflow_id TEXT NOT NULL,
                        session_id TEXT,
                        current_step TEXT NOT NULL,
                        blocked_on TEXT,
                        state TEXT NOT NULL,
                        context_json TEXT NOT NULL,
                        repo_json TEXT,
                        parent_work_item_id TEXT,
                        parent_step_id TEXT,
                        child_work_item_id TEXT,
                        created_at TEXT NOT NULL,
                        updated_at TEXT NOT NULL
                    );
                    CREATE TABLE IF NOT EXISTS jobs (
                        id TEXT PRIMARY KEY,
                        work_item_id TEXT NOT NULL,
                        step_id TEXT NOT NULL,
                        state TEXT NOT NULL,
                        assigned_worker_id TEXT,
                        execution_mode TEXT NOT NULL,
                        container_image TEXT,
                        command TEXT NOT NULL,
                        args_json TEXT NOT NULL,
                        env_json TEXT NOT NULL,
                        prompt_json TEXT,
                        context_json TEXT NOT NULL,
                        repo_json TEXT,
                        timeout_secs INTEGER,
                        transcript_dir TEXT,
                        result_json TEXT,
                        lease_expires_at TEXT,
                        runtime_spec_json TEXT,
                        created_at TEXT NOT NULL,
                        updated_at TEXT NOT NULL
                    );
                    CREATE INDEX IF NOT EXISTS idx_jobs_state ON jobs(state);
                    CREATE TABLE IF NOT EXISTS workers (
                        id TEXT PRIMARY KEY,
                        hostname TEXT NOT NULL,
                        state TEXT NOT NULL,
                        supported_modes_json TEXT NOT NULL,
                        last_heartbeat_at TEXT NOT NULL
                    );
                    "#,
                    )?;
                    if !column_exists(&conn, DbTable::Jobs, "container_image")? {
                        conn.execute("ALTER TABLE jobs ADD COLUMN container_image TEXT", [])?;
                    }
                    if !column_exists(&conn, DbTable::Jobs, "result_json")? {
                        conn.execute("ALTER TABLE jobs ADD COLUMN result_json TEXT", [])?;
                    }
                    if !column_exists(&conn, DbTable::WorkItems, "repo_json")? {
                        conn.execute("ALTER TABLE work_items ADD COLUMN repo_json TEXT", [])?;
                    }
                    if !column_exists(&conn, DbTable::WorkItems, "parent_work_item_id")? {
                        conn.execute(
                            "ALTER TABLE work_items ADD COLUMN parent_work_item_id TEXT",
                            [],
                        )?;
                    }
                    if !column_exists(&conn, DbTable::WorkItems, "parent_step_id")? {
                        conn.execute("ALTER TABLE work_items ADD COLUMN parent_step_id TEXT", [])?;
                    }
                    if !column_exists(&conn, DbTable::WorkItems, "child_work_item_id")? {
                        conn.execute(
                            "ALTER TABLE work_items ADD COLUMN child_work_item_id TEXT",
                            [],
                        )?;
                    }
                    if !column_exists(&conn, DbTable::Jobs, "repo_json")? {
                        conn.execute("ALTER TABLE jobs ADD COLUMN repo_json TEXT", [])?;
                    }
                    if !column_exists(&conn, DbTable::Jobs, "lease_expires_at")? {
                        conn.execute("ALTER TABLE jobs ADD COLUMN lease_expires_at TEXT", [])?;
                    }
                    if !column_exists(&conn, DbTable::Jobs, "timeout_secs")? {
                        conn.execute("ALTER TABLE jobs ADD COLUMN timeout_secs INTEGER", [])?;
                    }
                    if !column_exists(&conn, DbTable::Jobs, "max_retries")? {
                        conn.execute("ALTER TABLE jobs ADD COLUMN max_retries INTEGER", [])?;
                    }
                    if !column_exists(&conn, DbTable::Jobs, "attempt")? {
                        conn.execute("ALTER TABLE jobs ADD COLUMN attempt INTEGER", [])?;
                    }
                    if !column_exists(&conn, DbTable::Jobs, "runtime_spec_json")? {
                        conn.execute("ALTER TABLE jobs ADD COLUMN runtime_spec_json TEXT", [])?;
                    }
                    if !column_exists(&conn, DbTable::WorkItems, "external_task_id")? {
                        conn.execute(
                            "ALTER TABLE work_items ADD COLUMN external_task_id TEXT",
                            [],
                        )?;
                    }
                    if !column_exists(&conn, DbTable::WorkItems, "external_task_source")? {
                        conn.execute(
                            "ALTER TABLE work_items ADD COLUMN external_task_source TEXT",
                            [],
                        )?;
                    }
                    if !column_exists(&conn, DbTable::WorkItems, "title")? {
                        conn.execute("ALTER TABLE work_items ADD COLUMN title TEXT", [])?;
                    }
                    if !column_exists(&conn, DbTable::WorkItems, "description")? {
                        conn.execute("ALTER TABLE work_items ADD COLUMN description TEXT", [])?;
                    }
                    if !column_exists(&conn, DbTable::WorkItems, "labels_json")? {
                        conn.execute("ALTER TABLE work_items ADD COLUMN labels_json TEXT", [])?;
                    }
                    if !column_exists(&conn, DbTable::WorkItems, "priority")? {
                        conn.execute("ALTER TABLE work_items ADD COLUMN priority TEXT", [])?;
                    }
                    if !column_exists(&conn, DbTable::WorkItems, "projected_state")? {
                        conn.execute("ALTER TABLE work_items ADD COLUMN projected_state TEXT", [])?;
                    }
                    if !column_exists(&conn, DbTable::WorkItems, "triage_status")? {
                        conn.execute("ALTER TABLE work_items ADD COLUMN triage_status TEXT", [])?;
                    }
                    if !column_exists(&conn, DbTable::WorkItems, "human_gate")? {
                        conn.execute("ALTER TABLE work_items ADD COLUMN human_gate INTEGER", [])?;
                    }
                    if !column_exists(&conn, DbTable::WorkItems, "human_gate_state")? {
                        conn.execute(
                            "ALTER TABLE work_items ADD COLUMN human_gate_state TEXT",
                            [],
                        )?;
                    }
                    if !column_exists(&conn, DbTable::WorkItems, "blocked_on")? {
                        conn.execute("ALTER TABLE work_items ADD COLUMN blocked_on TEXT", [])?;
                    }
                    Ok(())
                })
                .await??;
                Ok::<(), anyhow::Error>(())
            })
            .await?;

        self.sync_tasks_from_adapter().await?;
        Ok(())
    }

    pub async fn create_work_item(&self, req: CreateWorkItemRequest) -> Result<WorkItem, AppError> {
        self.ensure_ready().await?;
        let initial_state = if let Some(workflow) = self.workflows.get(&req.workflow_id) {
            workflow.entry_step.clone()
        } else if let Some(reactive) = self.workflows.get_reactive(&req.workflow_id) {
            reactive.initial_state.clone()
        } else if req.workflow_id == "__triage__" {
            "triage".to_string()
        } else {
            return Err(AppError::WorkflowNotFound(req.workflow_id.clone()));
        };
        let item =
            work_item_from_create(&req, Uuid::new_v4(), req.workflow_id.clone(), initial_state);
        let mut item = item;
        ensure_repo_catalog_hint_context(&mut item);
        item.projected_state = Some(project_workflow_state(&item));
        self.insert_work_item(&item).await.map_err(map_db)?;
        self.persist_task_projection(&item, None)
            .await
            .map_err(map_config)?;
        let mut item = item;
        let created = self
            .ensure_pending_for_current_state(&mut item)
            .await
            .map_err(map_db)?;
        if created {
            item = self
                .get_work_item(item.id)
                .await
                .map_err(map_db)?
                .ok_or_else(|| AppError::WorkItemNotFound(item.id.to_string()))?;
        }
        Ok(item)
    }

    pub async fn list_work_items(&self) -> Result<Vec<WorkItem>, AppError> {
        self.ensure_ready().await?;
        self.select_work_items().await.map_err(map_db)
    }

    pub async fn list_jobs(&self) -> Result<Vec<Job>, AppError> {
        self.ensure_ready().await?;
        self.select_jobs().await.map_err(map_db)
    }

    pub(crate) async fn list_jobs_with_transcripts(&self) -> anyhow::Result<Vec<Job>> {
        self.ensure_ready().await?;
        self.select_jobs_with_transcripts().await
    }

    pub async fn list_jobs_for_work_item(&self, work_item_id: Uuid) -> Result<Vec<Job>, AppError> {
        self.ensure_ready().await?;
        self.select_jobs_for_work_item(work_item_id)
            .await
            .map_err(map_db)
    }

    pub async fn register_worker(
        &self,
        req: RegisterWorkerRequest,
    ) -> Result<WorkerInfo, AppError> {
        self.ensure_ready().await?;
        let worker = WorkerInfo {
            id: req.id,
            hostname: req.hostname,
            state: WorkerState::Idle,
            supported_modes: req.supported_modes,
            last_heartbeat_at: Utc::now(),
        };
        self.upsert_worker(&worker).await.map_err(map_db)?;
        Ok(worker)
    }

    pub async fn heartbeat(
        &self,
        worker_id: String,
        req: HeartbeatRequest,
    ) -> Result<WorkerInfo, AppError> {
        self.ensure_ready().await?;
        let mut worker = self
            .get_worker(&worker_id)
            .await
            .map_err(map_db)?
            .ok_or_else(|| AppError::WorkerNotFound(worker_id.clone()))?;
        worker.state = req.state;
        worker.last_heartbeat_at = Utc::now();
        self.upsert_worker(&worker).await.map_err(map_db)?;
        Ok(worker)
    }

    pub async fn list_workers(&self) -> Result<Vec<WorkerInfo>, AppError> {
        self.ensure_ready().await?;
        self.select_workers().await.map_err(map_db)
    }

    pub async fn work_item_state_counts(&self) -> Result<HashMap<String, usize>, AppError> {
        self.ensure_ready().await?;
        self.select_work_item_state_counts().await.map_err(map_db)
    }

    pub async fn job_state_counts(&self) -> Result<HashMap<String, usize>, AppError> {
        self.ensure_ready().await?;
        self.select_job_state_counts().await.map_err(map_db)
    }

    pub async fn worker_state_counts(&self) -> Result<HashMap<String, usize>, AppError> {
        self.ensure_ready().await?;
        self.select_worker_state_counts().await.map_err(map_db)
    }

    pub async fn list_recent_terminal_jobs(&self, limit: usize) -> Result<Vec<Job>, AppError> {
        self.ensure_ready().await?;
        self.select_recent_terminal_jobs(limit)
            .await
            .map_err(map_db)
    }

    pub async fn list_human_gate_work_items(
        &self,
        limit: usize,
    ) -> Result<Vec<WorkItem>, AppError> {
        self.ensure_ready().await?;
        self.select_human_gate_work_items(limit)
            .await
            .map_err(map_db)
    }

    pub async fn list_triage_work_items(&self, limit: usize) -> Result<Vec<WorkItem>, AppError> {
        self.ensure_ready().await?;
        self.select_triage_work_items(limit).await.map_err(map_db)
    }

    pub async fn poll_job(&self, req: PollJobRequest) -> Result<PollJobResponse, AppError> {
        self.ensure_ready().await?;
        self.reconcile_stale_workers(Duration::from_secs(STALE_WORKER_SECS))
            .await?;
        let mut worker = self
            .get_worker(&req.worker_id)
            .await
            .map_err(map_db)?
            .ok_or_else(|| AppError::WorkerNotFound(req.worker_id.clone()))?;

        if let Some(job) = self
            .assign_next_job(req.worker_id.clone(), worker.supported_modes.clone())
            .await
            .map_err(map_db)?
        {
            worker.state = WorkerState::Busy;
            worker.last_heartbeat_at = Utc::now();
            self.upsert_worker(&worker).await.map_err(map_db)?;
            return Ok(PollJobResponse { job: Some(job) });
        }

        worker.state = WorkerState::Idle;
        worker.last_heartbeat_at = Utc::now();
        self.upsert_worker(&worker).await.map_err(map_db)?;
        Ok(PollJobResponse { job: None })
    }

    pub async fn update_job(&self, job_id: Uuid, req: JobUpdateRequest) -> Result<Job, AppError> {
        self.ensure_ready().await?;
        let mut job = self
            .get_job(job_id)
            .await
            .map_err(map_db)?
            .ok_or_else(|| AppError::JobNotFound(job_id.to_string()))?;

        if job.assigned_worker_id.as_deref() != Some(req.worker_id.as_str()) {
            return Err(AppError::JobOwnership(format!(
                "worker '{}' does not own job '{}'",
                req.worker_id, job_id
            )));
        }

        if job.state != req.state {
            validate_job_transition(job.state.clone(), req.state.clone())
                .map_err(|e| AppError::WorkflowInvalid(e.to_string()))?;
        } else if matches!(req.state, JobState::Running) {
            // Allow running->running updates as lease renewals from the executing worker.
        }

        job.state = req.state.clone();
        if matches!(job.state, JobState::Running) {
            job.lease_expires_at = Some(Utc::now() + chrono::TimeDelta::seconds(LEASE_TTL_SECS));
        }
        if let Some(dir) = req.transcript_dir {
            job.transcript_dir = Some(dir);
        }
        if let Some(result) = req.result {
            job.result = Some(result);
        }
        if matches!(
            job.state,
            JobState::Succeeded | JobState::Failed | JobState::Cancelled
        ) {
            job.lease_expires_at = None;
        }
        job.updated_at = Utc::now();
        self.update_job_row(&job).await.map_err(map_db)?;

        if matches!(
            job.state,
            JobState::Succeeded | JobState::Failed | JobState::Cancelled
        ) {
            self.advance_workflow_for_job(&job).await?;
            if let Some(worker_id) = job.assigned_worker_id.clone()
                && let Some(mut worker) = self.get_worker(&worker_id).await.map_err(map_db)?
            {
                worker.state = WorkerState::Idle;
                worker.last_heartbeat_at = Utc::now();
                self.upsert_worker(&worker).await.map_err(map_db)?;
            }
        }

        Ok(job)
    }

    async fn fire_event_internal(
        &self,
        work_item_id: Uuid,
        req: FireEventRequest,
        source: EventSource,
    ) -> Result<WorkItem, AppError> {
        let workflow_event = WorkflowEvent {
            work_item_id: work_item_id.to_string(),
            event_type: req.event.clone(),
            payload: WorkflowEventPayload::None,
            source,
            timestamp: Utc::now(),
        };
        tracing::info!(workflow_event = ?workflow_event, "workflow event received");

        let mut item = self
            .get_work_item(work_item_id)
            .await
            .map_err(map_db)?
            .ok_or_else(|| AppError::WorkItemNotFound(work_item_id.to_string()))?;
        if item.workflow_id == "__triage__" && matches!(req.event, WorkflowEventKind::Submit) {
            return Err(AppError::WorkflowInvalid(
                "submit requested while task is still in triage".to_string(),
            ));
        }
        if let Some(workflow) = self.workflows.get(&item.workflow_id).cloned() {
            let step = workflow
                .steps
                .iter()
                .find(|s| s.id == item.current_step)
                .ok_or_else(|| {
                    AppError::WorkflowInvalid(format!("missing step '{}'", item.current_step))
                })?;

            match req.event {
                WorkflowEventKind::Start | WorkflowEventKind::Submit => {
                    if item.state == WorkItemState::Pending {
                        item.state = WorkItemState::Running;
                    }
                    self.ensure_pending_job_for_step(&item, &workflow)
                        .await
                        .map_err(map_db)?;
                }
                WorkflowEventKind::JobSucceeded => {
                    if let Some(next) = &step.on_success {
                        item.current_step = next.clone();
                        item.state = WorkItemState::Running;
                        self.ensure_pending_job_for_step(&item, &workflow)
                            .await
                            .map_err(map_db)?;
                    } else {
                        item.state = WorkItemState::Succeeded;
                    }
                }
                WorkflowEventKind::JobFailed => {
                    if let Some(next) = &step.on_failure {
                        item.current_step = next.clone();
                        item.state = WorkItemState::Running;
                        self.ensure_pending_job_for_step(&item, &workflow)
                            .await
                            .map_err(map_db)?;
                    } else {
                        item.state = WorkItemState::Failed;
                    }
                }
                WorkflowEventKind::HumanApproved => {
                    if let Some(next) = &step.on_success {
                        item.current_step = next.clone();
                        item.state = WorkItemState::Running;
                        self.ensure_pending_job_for_step(&item, &workflow)
                            .await
                            .map_err(map_db)?;
                    } else {
                        item.state = WorkItemState::Succeeded;
                    }
                }
                WorkflowEventKind::HumanChangesRequested
                | WorkflowEventKind::HumanUnblocked
                | WorkflowEventKind::RetryRequested => {
                    if let Some(next) = &step.on_failure {
                        item.current_step = next.clone();
                        item.state = WorkItemState::Running;
                        self.ensure_pending_job_for_step(&item, &workflow)
                            .await
                            .map_err(map_db)?;
                    } else {
                        item.state = WorkItemState::Failed;
                    }
                }
                WorkflowEventKind::AllChildrenCompleted => {
                    item.child_work_item_id = None;
                    if let Some(next) = &step.on_success {
                        item.current_step = next.clone();
                        item.state = WorkItemState::Running;
                        self.ensure_pending_job_for_step(&item, &workflow)
                            .await
                            .map_err(map_db)?;
                    } else {
                        item.state = WorkItemState::Succeeded;
                    }
                }
                WorkflowEventKind::ChildFailed => {
                    item.child_work_item_id = None;
                    if let Some(next) = &step.on_failure {
                        item.current_step = next.clone();
                        item.state = WorkItemState::Running;
                        self.ensure_pending_job_for_step(&item, &workflow)
                            .await
                            .map_err(map_db)?;
                    } else {
                        item.state = WorkItemState::Failed;
                    }
                }
                WorkflowEventKind::NeedsTriage => {
                    item.state = WorkItemState::Pending;
                }
                WorkflowEventKind::WorkflowChosen => {
                    if item.state == WorkItemState::Pending {
                        item.state = WorkItemState::Running;
                    }
                }
                event @ (WorkflowEventKind::TaskCreated
                | WorkflowEventKind::TaskUpdated
                | WorkflowEventKind::CommentAdded
                | WorkflowEventKind::LabelChanged
                | WorkflowEventKind::PrCreated
                | WorkflowEventKind::PrApproved
                | WorkflowEventKind::PrMerged
                | WorkflowEventKind::ChecksPassed
                | WorkflowEventKind::ChecksFailed
                | WorkflowEventKind::Timeout) => {
                    return Err(AppError::WorkflowInvalid(format!(
                        "event '{}' is not supported for legacy workflow '{}'",
                        workflow_event_kind_name(&event),
                        item.workflow_id
                    )));
                }
                WorkflowEventKind::Cancel => {
                    item.state = WorkItemState::Cancelled;
                    self.cancel_active_jobs_for_work_item(item.id)
                        .await
                        .map_err(map_db)?;
                    if let Some(child_id) = item.child_work_item_id {
                        self.cancel_work_item(child_id).await.map_err(map_db)?;
                    }
                }
            }
        } else if self.workflows.get_reactive(&item.workflow_id).is_some() {
            self.fire_reactive_event(&mut item, req.event).await?;
        } else {
            return Err(AppError::WorkflowNotFound(item.workflow_id.clone()));
        }

        item.updated_at = Utc::now();
        item.blocked_on = block_reason_for_state_name(&item.current_step);
        item.projected_state = Some(project_workflow_state(&item));
        self.update_work_item(&item).await.map_err(map_db)?;
        self.persist_task_projection(&item, None)
            .await
            .map_err(map_config)?;
        Ok(item)
    }

    async fn guard_event_request_for_ingress(
        &self,
        work_item_id: Uuid,
        requested_event: WorkflowEventKind,
        ingress: EventIngress,
    ) -> Result<(), AppError> {
        let item = self
            .get_work_item(work_item_id)
            .await
            .map_err(map_db)?
            .ok_or_else(|| AppError::WorkItemNotFound(work_item_id.to_string()))?;
        let valid_next_events = match ingress {
            EventIngress::ExternalPublic => self.valid_next_events_for_work_item(&item),
            EventIngress::TrustedInternal => self.valid_next_events_for_work_item_internal(&item),
        };
        if valid_next_events.contains(&requested_event) {
            return Ok(());
        }

        let allowed = valid_next_events
            .into_iter()
            .map(|event| format!("\"{}\"", workflow_event_kind_name(&event)))
            .collect::<Vec<_>>()
            .join(", ");
        Err(AppError::WorkflowInvalid(format!(
            "event '{}' is not currently valid for work item {} via {} ingress; valid_next_events=[{}]",
            workflow_event_kind_name(&requested_event),
            work_item_id,
            ingress.label(),
            allowed
        )))
    }

    pub async fn fire_event(
        &self,
        work_item_id: Uuid,
        req: FireEventRequest,
    ) -> Result<WorkItem, AppError> {
        self.ensure_ready().await?;
        self.guard_event_request_for_ingress(
            work_item_id,
            req.event.clone(),
            EventIngress::ExternalPublic,
        )
        .await?;
        self.fire_event_internal(work_item_id, req, EventSource::Manual)
            .await
    }

    pub async fn fire_event_trusted_internal(
        &self,
        work_item_id: Uuid,
        req: FireEventRequest,
    ) -> Result<WorkItem, AppError> {
        self.ensure_ready().await?;
        self.fire_event_trusted_internal_ready(work_item_id, req)
            .await
    }

    pub async fn fire_event_trusted_internal_with_source(
        &self,
        work_item_id: Uuid,
        req: FireEventRequest,
        source: EventSource,
    ) -> Result<WorkItem, AppError> {
        self.ensure_ready().await?;
        self.fire_event_trusted_internal_ready_with_source(work_item_id, req, source)
            .await
    }

    async fn fire_event_trusted_internal_ready(
        &self,
        work_item_id: Uuid,
        req: FireEventRequest,
    ) -> Result<WorkItem, AppError> {
        self.fire_event_trusted_internal_ready_with_source(work_item_id, req, EventSource::System)
            .await
    }

    async fn fire_event_trusted_internal_ready_with_source(
        &self,
        work_item_id: Uuid,
        req: FireEventRequest,
        source: EventSource,
    ) -> Result<WorkItem, AppError> {
        self.guard_event_request_for_ingress(
            work_item_id,
            req.event.clone(),
            EventIngress::TrustedInternal,
        )
        .await?;
        self.fire_event_internal(work_item_id, req, source).await
    }

    pub async fn dispatch_actionable(&self) -> Result<usize, AppError> {
        self.ensure_ready().await?;
        let items = self.select_work_items().await.map_err(map_db)?;
        let mut created = 0usize;
        for item in items {
            if !matches!(item.state, WorkItemState::Pending | WorkItemState::Running) {
                continue;
            }
            let workflow = match self.workflows.get(&item.workflow_id) {
                Some(w) => w.clone(),
                None => {
                    if self.workflows.get_reactive(&item.workflow_id).is_some() {
                        let mut item = item;
                        let created_for_item = self
                            .ensure_pending_for_current_state(&mut item)
                            .await
                            .map_err(map_db)?;
                        if created_for_item {
                            created += 1;
                        }
                    }
                    continue;
                }
            };
            if self.has_active_job(item.id).await.map_err(map_db)? {
                continue;
            }
            self.ensure_pending_job_for_step(&item, &workflow)
                .await
                .map_err(map_db)?;
            created += 1;
        }
        Ok(created)
    }

    pub async fn maintenance_tick(&self) -> Result<MaintenanceStats, AppError> {
        self.ensure_ready().await?;
        self.sync_tasks_from_adapter().await?;
        let requeued = self
            .reconcile_stale_workers(Duration::from_secs(STALE_WORKER_SECS))
            .await?;
        let lease_requeued = self.requeue_expired_leases().await.map_err(map_db)?;
        let created_jobs = self.dispatch_actionable().await?;
        Ok(MaintenanceStats {
            requeued_jobs: requeued,
            created_jobs,
            lease_requeued_jobs: lease_requeued,
        })
    }

    pub fn list_workflows(&self) -> Vec<WorkflowDefinition> {
        self.workflows.list()
    }

    pub fn list_workflow_documents(&self) -> Vec<WorkflowDocument> {
        let mut docs = self
            .workflows
            .list()
            .into_iter()
            .map(|workflow| WorkflowDocument::LegacyV1 { workflow })
            .collect::<Vec<_>>();
        docs.extend(
            self.workflows
                .list_reactive()
                .into_iter()
                .map(|workflow| WorkflowDocument::ReactiveV2 { workflow }),
        );
        docs.sort_by(|a, b| workflow_document_id(a).cmp(workflow_document_id(b)));
        docs
    }

    pub fn workflow_validation_report(&self) -> protocol::WorkflowValidationReport {
        self.workflows.validation_report()
    }

    pub async fn choose_workflow(
        &self,
        work_item_id: Uuid,
        workflow_id: String,
    ) -> Result<WorkItem, AppError> {
        self.ensure_ready().await?;
        let mut item = self
            .get_work_item(work_item_id)
            .await
            .map_err(map_db)?
            .ok_or_else(|| AppError::WorkItemNotFound(work_item_id.to_string()))?;
        if item.workflow_id != "__triage__" {
            return Err(AppError::WorkflowInvalid(
                "choose_workflow is only valid for triage work items".to_string(),
            ));
        }
        if work_item_requires_manual_triage(&item) {
            return Err(AppError::WorkflowInvalid(
                "cannot choose workflow while needs-triage is active".to_string(),
            ));
        }

        let initial_state = if let Some(workflow) = self.workflows.get(&workflow_id) {
            workflow.entry_step.clone()
        } else if let Some(workflow) = self.workflows.get_reactive(&workflow_id) {
            workflow.initial_state.clone()
        } else {
            return Err(AppError::WorkflowNotFound(workflow_id));
        };

        item.workflow_id = workflow_id;
        item.current_step = initial_state;
        item.state = WorkItemState::Pending;
        item.human_gate = false;
        item.triage_status = Some("selected".to_string());
        item.human_gate_state = Some("workflow_selected".to_string());
        item.blocked_on = block_reason_for_state_name(&item.current_step);
        item.context = mark_human_gate_state(item.context, TaskHumanGateState::WorkflowSelected);
        item.updated_at = Utc::now();
        item.projected_state = Some(project_workflow_state(&item));

        self.update_work_item(&item).await.map_err(map_db)?;
        let _ = self
            .ensure_pending_for_current_state(&mut item)
            .await
            .map_err(map_db)?;
        self.get_work_item(item.id)
            .await
            .map_err(map_db)?
            .ok_or_else(|| AppError::WorkItemNotFound(item.id.to_string()))
    }

    async fn ensure_ready(&self) -> Result<(), AppError> {
        self.init().await.map_err(map_db)
    }

    async fn sync_tasks_from_adapter(&self) -> Result<(), AppError> {
        tracing::trace!(
            adapter = self.task_adapter.adapter_name(),
            "syncing external tasks"
        );
        let adapter = Arc::clone(&self.task_adapter);
        let tasks = tokio::task::spawn_blocking(move || adapter.pull_tasks())
            .await
            .map_err(|e| AppError::Config(format!("adapter task pull join error: {e}")))?
            .map_err(map_config)?;
        let existing_items = self
            .select_work_items()
            .await
            .map_err(map_db)?
            .into_iter()
            .map(|i| (i.id, i))
            .collect::<HashMap<_, _>>();
        let mut workflow_ids = self
            .workflows
            .list()
            .into_iter()
            .map(|w| w.id)
            .collect::<Vec<_>>();
        workflow_ids.extend(self.workflows.list_reactive().into_iter().map(|w| w.id));

        let intents = infer_intents(tasks, &existing_items, &workflow_ids);
        for intent in intents {
            match intent {
                AdapterIntent::Create { task } => {
                    let workflow_id = match pick_workflow_for_task(&task, &workflow_ids) {
                        WorkflowSelectionDecision::Selected(workflow_id) => workflow_id,
                        WorkflowSelectionDecision::NeedsWorkflowSelection {
                            reason,
                            manual_triage_required,
                        } => {
                            let req = CreateWorkItemRequest {
                                workflow_id: "__triage__".to_string(),
                                session_id: None,
                                context: task.context.clone(),
                                repo: task.repo.clone(),
                            };
                            let mut item = work_item_from_create(
                                &req,
                                task.id,
                                "__triage__".to_string(),
                                "triage".to_string(),
                            );
                            item.context = merge_task_metadata(&item.context, &task);
                            item.context = mark_human_gate_state(
                                item.context,
                                TaskHumanGateState::NeedsWorkflowSelection,
                            );
                            item = apply_task_fields(item, &task);
                            if let Some(obj) = item.context.as_object_mut() {
                                obj.insert(
                                    "triage_reason".to_string(),
                                    serde_json::Value::String(reason),
                                );
                                if manual_triage_required {
                                    obj.insert(
                                        "task_human_gate_state".to_string(),
                                        serde_json::to_value(
                                            TaskHumanGateState::AwaitingHumanAction,
                                        )
                                        .unwrap_or(serde_json::Value::Null),
                                    );
                                }
                            }
                            if manual_triage_required {
                                item.human_gate_state = Some("awaiting_human_action".to_string());
                            }
                            self.insert_work_item(&item).await.map_err(map_db)?;
                            self.persist_task_projection(&item, Some(&task))
                                .await
                                .map_err(map_config)?;
                            continue;
                        }
                    };
                    let initial_state = if let Some(workflow) = self.workflows.get(&workflow_id) {
                        workflow.entry_step.clone()
                    } else if let Some(reactive) = self.workflows.get_reactive(&workflow_id) {
                        reactive.initial_state.clone()
                    } else {
                        continue;
                    };

                    let req = CreateWorkItemRequest {
                        workflow_id: workflow_id.clone(),
                        session_id: None,
                        context: task.context.clone(),
                        repo: task.repo.clone(),
                    };
                    let mut item = work_item_from_create(&req, task.id, workflow_id, initial_state);
                    item.context = merge_task_metadata(&item.context, &task);
                    item = apply_task_fields(item, &task);
                    ensure_repo_catalog_hint_context(&mut item);
                    self.insert_work_item(&item).await.map_err(map_db)?;
                    let _ = self
                        .ensure_pending_for_current_state(&mut item)
                        .await
                        .map_err(map_db)?;
                    self.persist_task_projection(&item, Some(&task))
                        .await
                        .map_err(map_config)?;
                }
                AdapterIntent::UpdateProjection { work_item_id, task } => {
                    let Some(mut item) = self.get_work_item(work_item_id).await.map_err(map_db)?
                    else {
                        continue;
                    };
                    item.context = task.context.clone();
                    item.context = merge_task_metadata(&item.context, &task);
                    item = apply_task_fields(item, &task);

                    if item.workflow_id == "__triage__" {
                        match pick_workflow_for_task(&task, &workflow_ids) {
                            WorkflowSelectionDecision::Selected(workflow_id)
                                if !work_item_requires_manual_triage(&item) =>
                            {
                                let initial_state =
                                    if let Some(workflow) = self.workflows.get(&workflow_id) {
                                        workflow.entry_step.clone()
                                    } else if let Some(reactive) =
                                        self.workflows.get_reactive(&workflow_id)
                                    {
                                        reactive.initial_state.clone()
                                    } else {
                                        continue;
                                    };
                                item.workflow_id = workflow_id;
                                item.current_step = initial_state;
                                item.state = WorkItemState::Pending;
                                item.human_gate = false;
                                item.triage_status = Some("selected".to_string());
                                item.human_gate_state = Some("workflow_selected".to_string());
                                item.context = mark_human_gate_state(
                                    item.context,
                                    TaskHumanGateState::WorkflowSelected,
                                );
                                let _ = self
                                    .ensure_pending_for_current_state(&mut item)
                                    .await
                                    .map_err(map_db)?;
                            }
                            WorkflowSelectionDecision::NeedsWorkflowSelection {
                                reason,
                                manual_triage_required,
                            } => {
                                if let Some(obj) = item.context.as_object_mut() {
                                    obj.insert(
                                        "triage_reason".to_string(),
                                        serde_json::Value::String(reason),
                                    );
                                    if manual_triage_required {
                                        obj.insert(
                                            "task_human_gate_state".to_string(),
                                            serde_json::to_value(
                                                TaskHumanGateState::AwaitingHumanAction,
                                            )
                                            .unwrap_or(serde_json::Value::Null),
                                        );
                                        item.human_gate_state =
                                            Some("awaiting_human_action".to_string());
                                    }
                                }
                            }
                            _ => {}
                        }
                    }

                    item.updated_at = task.updated_at;
                    item.blocked_on = block_reason_for_state_name(&item.current_step);
                    self.update_work_item(&item).await.map_err(map_db)?;
                    self.persist_task_projection(&item, Some(&task))
                        .await
                        .map_err(map_config)?;
                }
                AdapterIntent::ApplyAction {
                    work_item_id,
                    action,
                    action_revision,
                } => {
                    let Some(mut item) = self.get_work_item(work_item_id).await.map_err(map_db)?
                    else {
                        continue;
                    };
                    let last_applied = item
                        .context
                        .get("task_adapter")
                        .and_then(|v| v.get("last_action_revision"))
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or(0);
                    if action_revision <= last_applied {
                        continue;
                    }

                    if matches!(action.kind, HumanActionKind::ChooseWorkflow) {
                        if let Some(workflow_id) = action.workflow_id.clone() {
                            if item.workflow_id == "__triage__" {
                                if work_item_requires_manual_triage(&item) {
                                    tracing::warn!(
                                        work_item_id = %item.id,
                                        workflow_id = %workflow_id,
                                        "choose_workflow action ignored while needs-triage is active"
                                    );
                                    continue;
                                }
                                let initial_state = if let Some(w) =
                                    self.workflows.get(&workflow_id)
                                {
                                    w.entry_step.clone()
                                } else if let Some(w) = self.workflows.get_reactive(&workflow_id) {
                                    w.initial_state.clone()
                                } else {
                                    continue;
                                };
                                item.workflow_id = workflow_id.clone();
                                item.current_step = initial_state;
                                item.state = WorkItemState::Pending;
                                item.human_gate = false;
                                item.triage_status = Some("selected".to_string());
                                item.human_gate_state = Some("workflow_selected".to_string());
                                item.context = mark_human_gate_state(
                                    item.context,
                                    TaskHumanGateState::WorkflowSelected,
                                );
                                let _ = self
                                    .ensure_pending_for_current_state(&mut item)
                                    .await
                                    .map_err(map_db)?;
                            } else if item.context.as_object_mut().is_some() {
                                tracing::debug!(
                                    work_item_id = %item.id,
                                    workflow_id = %workflow_id,
                                    "ignoring choose_workflow for non-triage work item"
                                );
                            }
                        }
                    } else if let Some(req) = action_to_fire_event(&action) {
                        if matches!(action.kind, HumanActionKind::Submit)
                            && item.workflow_id == "__triage__"
                            && work_item_requires_manual_triage(&item)
                        {
                            tracing::warn!(
                                work_item_id = %item.id,
                                "submit action ignored for triage item without selected workflow"
                            );
                            continue;
                        }
                        if matches!(
                            action.kind,
                            HumanActionKind::Approve
                                | HumanActionKind::RequestChanges
                                | HumanActionKind::Unblock
                                | HumanActionKind::Retry
                        ) {
                            let state = match action.kind {
                                HumanActionKind::Approve => TaskHumanGateState::Approved,
                                HumanActionKind::RequestChanges => {
                                    TaskHumanGateState::ChangesRequested
                                }
                                HumanActionKind::Unblock => TaskHumanGateState::Unblocked,
                                HumanActionKind::Retry => TaskHumanGateState::RetryRequested,
                                _ => TaskHumanGateState::WorkflowSelected,
                            };
                            item.human_gate_state = Some(task_human_gate_state_name(&state));
                            item.context = mark_human_gate_state(item.context, state);
                        }
                        // Adapter actions are interpreted internal intents from the trusted
                        // task projection path. Route through trusted ingress validation so
                        // system/internal events remain available without using public ingress.
                        item = self
                            .fire_event_trusted_internal_ready(work_item_id, req)
                            .await?;
                    }

                    item.context = merge_task_action_revision(item.context, action_revision);
                    item.updated_at = Utc::now();
                    item.blocked_on = block_reason_for_state_name(&item.current_step);
                    self.update_work_item(&item).await.map_err(map_db)?;
                    let adapter = Arc::clone(&self.task_adapter);
                    let item_id = item.id;
                    let existing = tokio::task::spawn_blocking(move || adapter.get_task(item_id))
                        .await
                        .map_err(|e| {
                            AppError::Config(format!("adapter task lookup join error: {e}"))
                        })?
                        .map_err(map_config)?;
                    self.persist_task_projection(&item, existing.as_ref())
                        .await
                        .map_err(map_config)?;
                }
            }
        }
        Ok(())
    }

    async fn persist_task_projection(
        &self,
        item: &WorkItem,
        existing: Option<&TaskToolRecord>,
    ) -> anyhow::Result<()> {
        let valid_next_events = self.valid_next_events_for_work_item(item);
        let task = task_from_work_item_with_valid_next_events(item, existing, &valid_next_events);
        let adapter = Arc::clone(&self.task_adapter);
        tokio::task::spawn_blocking(move || adapter.persist_projection(&TaskProjection { task }))
            .await
            .map_err(|e| anyhow::anyhow!("adapter persist projection join error: {e}"))?
    }

    pub(crate) fn valid_next_events_for_work_item(
        &self,
        item: &WorkItem,
    ) -> Vec<WorkflowEventKind> {
        self.valid_next_events_for_work_item_internal(item)
            .into_iter()
            .filter(workflow_event_is_externally_fireable)
            .collect()
    }

    fn projected_valid_next_events_for_job_context(
        &self,
        item: &WorkItem,
    ) -> Vec<WorkflowEventKind> {
        if let Some(reactive) = self.workflows.get_reactive(&item.workflow_id)
            && let Some(state) = reactive
                .states
                .iter()
                .find(|state| state.id == item.current_step)
            && matches!(
                state.kind(),
                protocol::ReactiveStateKind::Agent
                    | protocol::ReactiveStateKind::Command
                    | protocol::ReactiveStateKind::Managed
            )
        {
            return self.valid_next_events_for_work_item_internal(item);
        }

        self.valid_next_events_for_work_item(item)
    }

    pub(crate) fn github_pr_state_for_work_item(&self, item: &WorkItem) -> Option<GithubPrState> {
        github_pr_state_from_context(&item.context)
    }

    pub(crate) fn check_merge_readiness(&self, item: &WorkItem) -> Option<MergeReadiness> {
        self.github_pr_state_for_work_item(item)
            .as_ref()
            .map(merge_readiness_from_pr_state)
    }

    fn reactive_state_auto_merge_enabled(&self, item: &WorkItem) -> bool {
        self.workflows
            .get_reactive(&item.workflow_id)
            .and_then(|workflow| {
                workflow
                    .states
                    .iter()
                    .find(|state| state.id == item.current_step)
            })
            .map(|state| state.auto_merge)
            .unwrap_or(false)
    }

    /// Attempt to merge a PR when readiness conditions are met.
    /// This is deterministic — no agent involvement.
    /// Returns Ok(true) if merge was triggered, Ok(false) if not ready or already merged.
    pub(crate) async fn try_auto_merge(&self, item: &mut WorkItem) -> anyhow::Result<bool> {
        let Some(readiness) = self.check_merge_readiness(item) else {
            return Ok(false);
        };
        if !readiness.ready {
            return Ok(false);
        }

        let Some(mut pr_state) = self.github_pr_state_for_work_item(item) else {
            return Ok(false);
        };
        if matches!(pr_state.state, GithubPrLifecycleState::Merged) || pr_state.merge_attempted {
            return Ok(false);
        }

        eprintln!(
            "[info] deterministic auto-merge attempt: work_item={} pr_url={}",
            item.id, pr_state.url
        );
        let output = match Command::new("gh")
            .arg("pr")
            .arg("merge")
            .arg(&pr_state.url)
            .arg("--merge")
            .arg("--auto")
            .output()
            .await
        {
            Ok(output) => output,
            Err(error) => {
                eprintln!(
                    "[info] deterministic auto-merge failed to execute gh: work_item={} pr_url={} error={error}",
                    item.id, pr_state.url
                );
                return Ok(false);
            }
        };

        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        if !output.status.success() {
            eprintln!(
                "[info] deterministic auto-merge command failed: work_item={} pr_url={} status={} stdout='{}' stderr='{}'",
                item.id, pr_state.url, output.status, stdout, stderr
            );
            return Ok(false);
        }

        eprintln!(
            "[info] deterministic auto-merge command succeeded: work_item={} pr_url={} stdout='{}' stderr='{}'",
            item.id, pr_state.url, stdout, stderr
        );

        pr_state.merge_attempted = true;
        pr_state.state = GithubPrLifecycleState::Merged;
        pr_state.mergeable = false;
        pr_state.last_updated = Utc::now();
        if !item.context.is_object() {
            item.context = serde_json::Value::Object(serde_json::Map::new());
        }
        if let Some(obj) = item.context.as_object_mut()
            && let Ok(value) = serde_json::to_value(pr_state)
        {
            obj.insert(CONTEXT_GITHUB_PR_STATE_KEY.to_string(), value);
        }
        item.updated_at = Utc::now();
        self.update_work_item(item).await?;
        self.persist_task_projection(item, None).await?;

        if let Err(error) = self
            .fire_event_trusted_internal(
                item.id,
                FireEventRequest {
                    event: WorkflowEventKind::PrMerged,
                    job_id: None,
                    reason: Some("deterministic auto-merge completed".to_string()),
                },
            )
            .await
            .map(|updated| {
                *item = updated;
            })
        {
            eprintln!(
                "[info] deterministic auto-merge fired merge command but could not fire PrMerged: work_item={} error={error}",
                item.id
            );
        }

        Ok(true)
    }

    fn valid_next_events_for_work_item_internal(&self, item: &WorkItem) -> Vec<WorkflowEventKind> {
        if matches!(
            item.state,
            WorkItemState::Succeeded | WorkItemState::Cancelled
        ) {
            return Vec::new();
        }
        // Failed is intentionally not treated as terminal here: trusted/internal callers may still
        // drive retry-oriented progression semantics from that state.

        if item.workflow_id == "__triage__" {
            return triage_valid_next_events(item);
        }

        if let Some(reactive) = self.workflows.get_reactive(&item.workflow_id)
            && let Some(state) = reactive.states.iter().find(|s| s.id == item.current_step)
        {
            let mut out = state
                .on
                .iter()
                .map(|transition| transition.event.kind())
                .collect::<Vec<_>>();
            if !matches!(state.kind(), protocol::ReactiveStateKind::Terminal) {
                push_unique_event(&mut out, WorkflowEventKind::Cancel);
            }
            return out;
        }

        let Some(workflow) = self.workflows.get(&item.workflow_id) else {
            return Vec::new();
        };
        let Some(step) = workflow.steps.iter().find(|s| s.id == item.current_step) else {
            return Vec::new();
        };

        let mut out = Vec::new();
        if item.state == WorkItemState::Pending {
            push_unique_event(&mut out, WorkflowEventKind::Start);
            push_unique_event(&mut out, WorkflowEventKind::Submit);
        }
        if item.state == WorkItemState::Failed {
            push_unique_event(&mut out, WorkflowEventKind::RetryRequested);
        }
        if item.human_gate {
            // Human-gate semantics fully determine the externally valid event set for this legacy
            // item, so return the narrowed gate-state-aware contract directly.
            out = legacy_human_gate_valid_next_events(item);
            return out;
        }
        if step.kind == WorkflowStepKind::ChildWorkflow {
            push_unique_event(&mut out, WorkflowEventKind::AllChildrenCompleted);
            push_unique_event(&mut out, WorkflowEventKind::ChildFailed);
        } else {
            push_unique_event(&mut out, WorkflowEventKind::JobSucceeded);
            push_unique_event(&mut out, WorkflowEventKind::JobFailed);
        }
        push_unique_event(&mut out, WorkflowEventKind::Cancel);
        out
    }

    async fn reconcile_stale_workers(&self, stale_after: Duration) -> Result<usize, AppError> {
        let workers = self.select_workers().await.map_err(map_db)?;
        let mut total_requeued = 0usize;
        let now = Utc::now();
        for mut worker in workers {
            let Ok(elapsed) = (now - worker.last_heartbeat_at).to_std() else {
                continue;
            };
            if elapsed < stale_after {
                continue;
            }
            let was_offline = worker.state == WorkerState::Offline;
            if worker.state != WorkerState::Offline {
                worker.state = WorkerState::Offline;
            }
            let requeued = self
                .requeue_jobs_for_worker(&worker.id)
                .await
                .map_err(map_db)?;
            total_requeued += requeued;
            if requeued > 0 || !was_offline {
                self.upsert_worker(&worker).await.map_err(map_db)?;
            }
        }
        Ok(total_requeued)
    }

    async fn requeue_jobs_for_worker(&self, worker_id: &str) -> anyhow::Result<usize> {
        let path = Arc::clone(&self.db_path);
        let worker_id = worker_id.to_string();
        tokio::task::spawn_blocking(move || -> anyhow::Result<usize> {
            let conn = Connection::open(path.as_path())?;
            let count = conn.execute(
                "UPDATE jobs SET state = ?1, assigned_worker_id = NULL, lease_expires_at = NULL, updated_at = ?2 WHERE assigned_worker_id = ?3 AND state IN (?4, ?5)",
                params![
                    JobState::Pending.to_string(),
                    Utc::now().to_rfc3339(),
                    worker_id,
                    JobState::Assigned.to_string(),
                    JobState::Running.to_string(),
                ],
            )?;
            Ok(count)
        })
        .await?
    }

    async fn requeue_expired_leases(&self) -> anyhow::Result<usize> {
        let path = Arc::clone(&self.db_path);
        tokio::task::spawn_blocking(move || -> anyhow::Result<usize> {
            let conn = Connection::open(path.as_path())?;
            let now = Utc::now().to_rfc3339();
            let count = conn.execute(
                "UPDATE jobs SET state = ?1, assigned_worker_id = NULL, lease_expires_at = NULL, updated_at = ?2 WHERE state IN (?3, ?4) AND lease_expires_at IS NOT NULL AND lease_expires_at < ?5",
                params![
                    JobState::Pending.to_string(),
                    now,
                    JobState::Assigned.to_string(),
                    JobState::Running.to_string(),
                    now,
                ],
            )?;
            Ok(count)
        })
        .await?
    }

    async fn advance_workflow_for_job(&self, job: &Job) -> Result<(), AppError> {
        let mut item = self
            .get_work_item(job.work_item_id)
            .await
            .map_err(map_db)?
            .ok_or_else(|| AppError::WorkItemNotFound(job.work_item_id.to_string()))?;
        if self.workflows.get_reactive(&item.workflow_id).is_some() {
            self.merge_job_output_into_context(&mut item, job)
                .map_err(AppError::WorkflowInvalid)?;

            let repo_discovery_failed = requires_repo_discovery(&item)
                && !item.repo.as_ref().is_some_and(is_valid_repo_source);
            if repo_discovery_failed {
                tracing::warn!(
                    work_item_id = %item.id,
                    workflow_id = %item.workflow_id,
                    state = %item.current_step,
                    "repo discovery missing or invalid; forcing triage failure event"
                );
            }

            let valid_next_events = self.valid_next_events_for_work_item_internal(&item);
            let agent_next_event = reactive_next_event_from_agent_output(job, &valid_next_events)
                .inspect_err(|error| {
                    tracing::warn!(
                        work_item_id = %item.id,
                        job_id = %job.id,
                        workflow_id = %item.workflow_id,
                        state = %item.current_step,
                        "{error}"
                    );
                })
                .ok()
                .flatten();

            let default_event = match job.state {
                JobState::Succeeded => WorkflowEventKind::JobSucceeded,
                JobState::Failed | JobState::Cancelled => {
                    // Cancelled jobs currently collapse onto the reactive failure lane; reactive
                    // transition tables only distinguish job success vs non-success in this MVP.
                    if job.state == JobState::Failed && job.attempt < job.max_retries {
                        self.retry_job(job).await.map_err(map_db)?;
                        item.state = WorkItemState::Running;
                        item.updated_at = Utc::now();
                        item.blocked_on = block_reason_for_state_name(&item.current_step);
                        self.update_work_item(&item).await.map_err(map_db)?;
                        self.persist_task_projection(&item, None)
                            .await
                            .map_err(map_config)?;
                        return Ok(());
                    }
                    WorkflowEventKind::JobFailed
                }
                JobState::Pending | JobState::Assigned | JobState::Running => return Ok(()),
            };
            let ev = if repo_discovery_failed {
                WorkflowEventKind::JobFailed
            } else {
                agent_next_event.unwrap_or(default_event)
            };
            self.fire_reactive_event(&mut item, ev).await?;
            item.updated_at = Utc::now();
            item.blocked_on = block_reason_for_state_name(&item.current_step);
            item.projected_state = Some(project_workflow_state(&item));
            self.update_work_item(&item).await.map_err(map_db)?;
            self.persist_task_projection(&item, None)
                .await
                .map_err(map_config)?;
            self.try_progress_parent(item.id).await?;
            return Ok(());
        }

        let workflow = self
            .workflows
            .get(&item.workflow_id)
            .ok_or_else(|| AppError::WorkflowNotFound(item.workflow_id.clone()))?
            .clone();
        let step = workflow
            .steps
            .iter()
            .find(|s| s.id == item.current_step)
            .ok_or_else(|| {
                AppError::WorkflowInvalid(format!("missing step '{}'", item.current_step))
            })?;

        if step.id != job.step_id {
            return Err(AppError::WorkflowInvalid(format!(
                "job step '{}' does not match work item step '{}'",
                job.step_id, item.current_step
            )));
        }

        self.merge_job_output_into_context(&mut item, job)
            .map_err(AppError::WorkflowInvalid)?;

        match job.state {
            JobState::Succeeded => {
                if let Some(next) = &step.on_success {
                    item.current_step = next.clone();
                    item.state = WorkItemState::Running;
                    self.ensure_pending_job_for_step(&item, &workflow)
                        .await
                        .map_err(map_db)?;
                } else {
                    item.state = WorkItemState::Succeeded;
                }
            }
            JobState::Failed | JobState::Cancelled => {
                if job.state == JobState::Failed && job.attempt < job.max_retries {
                    self.retry_job(job).await.map_err(map_db)?;
                    item.state = WorkItemState::Running;
                    item.updated_at = Utc::now();
                    item.blocked_on = block_reason_for_state_name(&item.current_step);
                    self.update_work_item(&item).await.map_err(map_db)?;
                    self.persist_task_projection(&item, None)
                        .await
                        .map_err(map_config)?;
                    return Ok(());
                }
                if let Some(next) = &step.on_failure {
                    item.current_step = next.clone();
                    item.state = WorkItemState::Running;
                    self.ensure_pending_job_for_step(&item, &workflow)
                        .await
                        .map_err(map_db)?;
                } else {
                    item.state = WorkItemState::Failed;
                }
            }
            JobState::Pending | JobState::Assigned | JobState::Running => return Ok(()),
        }

        item.updated_at = Utc::now();
        item.blocked_on = block_reason_for_state_name(&item.current_step);
        item.projected_state = Some(project_workflow_state(&item));
        self.update_work_item(&item).await.map_err(map_db)?;
        self.persist_task_projection(&item, None)
            .await
            .map_err(map_config)?;
        self.try_progress_parent(item.id).await?;
        Ok(())
    }

    async fn ensure_pending_for_current_state(&self, item: &mut WorkItem) -> anyhow::Result<bool> {
        if let Some(workflow) = self.workflows.get(&item.workflow_id) {
            self.ensure_pending_job_for_step(item, workflow).await?;
            return Ok(true);
        }

        let Some(reactive) = self.workflows.get_reactive(&item.workflow_id) else {
            return Ok(false);
        };

        let mut created_job = false;
        let mut auto_hops = 0usize;
        loop {
            let state = reactive
                .states
                .iter()
                .find(|s| s.id == item.current_step)
                .ok_or_else(|| anyhow::anyhow!("missing reactive state {}", item.current_step))?;

            match state.kind() {
                protocol::ReactiveStateKind::Terminal => {
                    let terminal_outcome = match &state.config {
                        protocol::ReactiveStateConfig::Terminal(config) => {
                            config.terminal_outcome.as_ref()
                        }
                        _ => None,
                    };
                    item.state = reactive_terminal_work_item_state(terminal_outcome);
                    item.human_gate = false;
                    item.blocked_on = None;
                    item.blocked_on = block_reason_for_state_name(&state.id);
                    break;
                }
                protocol::ReactiveStateKind::Human => {
                    item.state = WorkItemState::Running;
                    item.human_gate = true;
                    item.blocked_on = block_reason_for_state_name(&state.id);
                    if item.human_gate_state.is_none() {
                        item.human_gate_state = Some("awaiting_human_action".to_string());
                    }
                    break;
                }
                protocol::ReactiveStateKind::Auto => {
                    item.human_gate = false;
                    item.blocked_on = None;
                    item.blocked_on = block_reason_for_state_name(&state.id);
                    let child_workflow_id = match &state.config {
                        protocol::ReactiveStateConfig::Auto(config) => config
                            .child_workflow
                            .as_deref()
                            .map(str::trim)
                            .filter(|v| !v.is_empty()),
                        _ => None,
                    };
                    if let Some(child_workflow_id) = child_workflow_id {
                        if item.child_work_item_id.is_none() {
                            let (child_workflow_id, child_initial_step) =
                                if let Some(child_workflow) = self.workflows.get(child_workflow_id)
                                {
                                    (child_workflow.id.clone(), child_workflow.entry_step.clone())
                                } else if let Some(child_workflow) =
                                    self.workflows.get_reactive(child_workflow_id)
                                {
                                    (
                                        child_workflow.id.clone(),
                                        child_workflow.initial_state.clone(),
                                    )
                                } else {
                                    return Err(anyhow::anyhow!(
                                        "unknown child workflow {child_workflow_id}"
                                    ));
                                };
                            let now = Utc::now();
                            let child_id = Uuid::new_v4();
                            let mut child = WorkItem {
                                id: child_id,
                                workflow_id: child_workflow_id,
                                external_task_id: None,
                                external_task_source: None,
                                title: item.title.clone(),
                                description: item.description.clone(),
                                labels: item.labels.clone(),
                                priority: item.priority.clone(),
                                session_id: item.session_id.clone(),
                                current_step: child_initial_step,
                                projected_state: Some("pending".to_string()),
                                triage_status: item.triage_status.clone(),
                                human_gate: item.human_gate,
                                human_gate_state: item.human_gate_state.clone(),
                                blocked_on: None,
                                state: WorkItemState::Pending,
                                context: item.context.clone(),
                                repo: item.repo.clone(),
                                parent_work_item_id: Some(item.id),
                                parent_step_id: Some(state.id.clone()),
                                child_work_item_id: None,
                                created_at: now,
                                updated_at: now,
                            };
                            self.insert_work_item(&child).await?;
                            self.persist_task_projection(&child, None).await?;
                            Box::pin(self.ensure_pending_for_current_state(&mut child)).await?;

                            item.child_work_item_id = Some(child_id);
                        }
                        item.state = WorkItemState::Running;
                        break;
                    }
                    auto_hops = auto_hops.saturating_add(1);
                    if auto_hops > 32 {
                        return Err(anyhow::anyhow!(
                            "reactive auto state transition depth exceeded"
                        ));
                    }
                    if let Some(next) = state
                        .on
                        .iter()
                        .find(|t| t.event.kind() == protocol::ReactiveEventKind::Start)
                    {
                        item.current_step = next.target.clone();
                        item.state = WorkItemState::Running;
                        continue;
                    }
                    return Err(anyhow::anyhow!(
                        "reactive auto state '{}' missing 'start' transition",
                        state.id
                    ));
                }
                protocol::ReactiveStateKind::Agent
                | protocol::ReactiveStateKind::Command
                | protocol::ReactiveStateKind::Managed => {
                    item.human_gate = false;
                    item.blocked_on = block_reason_for_state_name(&state.id);
                    if !self.has_pending_job_for_step(item.id, &state.id).await? {
                        let prompt_context = interpolation_context_for_work_item(item, &state.id);
                        let prompt = state
                            .config
                            .prompt()
                            .as_ref()
                            .map(
                                |prompt| -> Result<protocol::PromptRef, InterpolationError> {
                                    Ok(protocol::PromptRef {
                                        system: prompt
                                            .system
                                            .as_deref()
                                            .map(|system| {
                                                interpolate_template(system, &prompt_context)
                                            })
                                            .transpose()?,
                                        user: interpolate_template(&prompt.user, &prompt_context)?,
                                    })
                                },
                            )
                            .transpose()?;
                        let (command, args, env, managed_operation) = match &state.config {
                            protocol::ReactiveStateConfig::Agent(_) => {
                                (String::new(), Vec::new(), HashMap::new(), None)
                            }
                            protocol::ReactiveStateConfig::Command(config) => {
                                let command = interpolate_template(&config.command, &item.context)?;
                                let args = config
                                    .args
                                    .iter()
                                    .map(|a| interpolate_template(a, &item.context))
                                    .collect::<Result<Vec<_>, _>>()?;
                                let env = config
                                    .env
                                    .iter()
                                    .map(|(k, v)| {
                                        Ok((k.clone(), interpolate_template(v, &item.context)?))
                                    })
                                    .collect::<Result<HashMap<_, _>, InterpolationError>>()?;
                                (command, args, env, None)
                            }
                            protocol::ReactiveStateConfig::Managed(config) => (
                                String::new(),
                                Vec::new(),
                                HashMap::new(),
                                Some(config.operation.clone()),
                            ),
                            _ => {
                                return Err(anyhow::anyhow!(
                                    "non-executable reactive state '{}' scheduled as executable",
                                    state.id
                                ));
                            }
                        };
                        let now = Utc::now();
                        let execution = state.config.execution();
                        let timeout_secs = execution
                            .and_then(|config| config.timeout)
                            .or(execution.and_then(|config| config.timeout_secs));
                        let job = Job {
                            id: Uuid::new_v4(),
                            work_item_id: item.id,
                            step_id: state.id.clone(),
                            state: JobState::Pending,
                            assigned_worker_id: None,
                            execution_mode: execution
                                .and_then(|config| config.executor.clone())
                                .clone()
                                .unwrap_or(protocol::ExecutionMode::Raw),
                            container_image: execution
                                .and_then(|config| config.container_image.clone()),
                            command,
                            args,
                            env,
                            prompt,
                            context: item.context.clone(),
                            repo: item.repo.clone(),
                            timeout_secs,
                            max_retries: execution.map(|config| config.max_retries).unwrap_or(0),
                            attempt: 0,
                            transcript_dir: None,
                            result: None,
                            lease_expires_at: None,
                            created_at: now,
                            updated_at: now,
                        };
                        let runtime = runtime_spec_for_reactive_agent_state(state, item, &job);
                        let mut job = job;
                        job.execution_mode = runtime.environment.execution_mode.clone();
                        job.container_image = runtime.environment.container_image.clone();
                        job.timeout_secs = runtime.environment.timeout_secs;
                        project_reactive_execution_markers_into_job_context(
                            &mut job.context,
                            state.kind(),
                            managed_operation.as_deref(),
                        );
                        let valid_next_events =
                            self.projected_valid_next_events_for_job_context(item);
                        project_workflow_inputs_into_job_context(
                            &mut job.context,
                            &valid_next_events,
                            item,
                            &item.context,
                        );
                        project_runtime_spec_into_job_context(&mut job.context, &runtime);
                        self.insert_job(&job).await?;
                        created_job = true;
                    }
                    item.state = WorkItemState::Running;
                    break;
                }
            }
        }

        item.updated_at = Utc::now();
        item.projected_state = Some(project_workflow_state(item));
        self.update_work_item(item).await?;
        self.persist_task_projection(item, None).await?;
        Ok(created_job)
    }

    async fn fire_reactive_event(
        &self,
        item: &mut WorkItem,
        event: WorkflowEventKind,
    ) -> Result<(), AppError> {
        let reactive = self
            .workflows
            .get_reactive(&item.workflow_id)
            .ok_or_else(|| AppError::WorkflowNotFound(item.workflow_id.clone()))?
            .clone();
        let state = reactive
            .states
            .iter()
            .find(|s| s.id == item.current_step)
            .ok_or_else(|| AppError::WorkflowInvalid("reactive state missing".into()))?;

        if matches!(event, WorkflowEventKind::Cancel) {
            item.state = WorkItemState::Cancelled;
            self.cancel_active_jobs_for_work_item(item.id)
                .await
                .map_err(map_db)?;
            Box::pin(self.try_progress_parent(item.id)).await?;
            return Ok(());
        }

        let Some(transition) = state.on.iter().find(|t| t.event.kind() == event) else {
            if matches!(state.kind(), protocol::ReactiveStateKind::Terminal) {
                return Ok(());
            }
            return Err(AppError::WorkflowInvalid(format!(
                "reactive state '{}' has no transition for event '{}'",
                state.id, event
            )));
        };
        item.current_step = transition.target.clone();
        self.ensure_pending_for_current_state(item)
            .await
            .map_err(map_db)?;
        Ok(())
    }

    async fn try_progress_parent(&self, child_id: Uuid) -> Result<(), AppError> {
        let child = match self.get_work_item(child_id).await.map_err(map_db)? {
            Some(c) => c,
            None => return Ok(()),
        };
        let Some(parent_id) = child.parent_work_item_id else {
            return Ok(());
        };
        let Some(parent_step_id) = child.parent_step_id.clone() else {
            return Ok(());
        };

        if !matches!(
            child.state,
            WorkItemState::Succeeded | WorkItemState::Failed | WorkItemState::Cancelled
        ) {
            return Ok(());
        }

        let mut parent = self
            .get_work_item(parent_id)
            .await
            .map_err(map_db)?
            .ok_or_else(|| AppError::WorkItemNotFound(parent_id.to_string()))?;
        if self.workflows.get_reactive(&parent.workflow_id).is_some() {
            let children = self
                .select_children_for_parent_step(parent.id, &parent_step_id)
                .await
                .map_err(map_db)?;
            if children.is_empty() {
                return Ok(());
            }
            if !children.iter().all(|candidate| {
                matches!(
                    candidate.state,
                    WorkItemState::Succeeded | WorkItemState::Failed | WorkItemState::Cancelled
                )
            }) {
                return Ok(());
            }

            for candidate in &children {
                merge_child_result_into_parent_context(&mut parent, candidate)
                    .map_err(AppError::WorkflowInvalid)?;
            }
            parent.child_work_item_id = None;

            let event = if children
                .iter()
                .any(|candidate| !matches!(candidate.state, WorkItemState::Succeeded))
            {
                WorkflowEventKind::ChildFailed
            } else {
                WorkflowEventKind::AllChildrenCompleted
            };
            Box::pin(self.fire_reactive_event(&mut parent, event)).await?;
            parent.updated_at = Utc::now();
            parent.blocked_on = block_reason_for_state_name(&parent.current_step);
            parent.projected_state = Some(project_workflow_state(&parent));
            self.update_work_item(&parent).await.map_err(map_db)?;
            self.persist_task_projection(&parent, None)
                .await
                .map_err(map_config)?;
            return Ok(());
        }
        let workflow = self
            .workflows
            .get(&parent.workflow_id)
            .ok_or_else(|| AppError::WorkflowNotFound(parent.workflow_id.clone()))?
            .clone();
        let step = workflow
            .steps
            .iter()
            .find(|s| s.id == parent_step_id)
            .ok_or_else(|| AppError::WorkflowInvalid("parent step missing".into()))?;
        merge_child_result_into_parent_context(&mut parent, &child)
            .map_err(AppError::WorkflowInvalid)?;

        match child.state {
            WorkItemState::Succeeded => {
                if let Some(next) = &step.on_success {
                    parent.current_step = next.clone();
                    parent.state = WorkItemState::Running;
                    parent.child_work_item_id = None;
                    self.ensure_pending_job_for_step(&parent, &workflow)
                        .await
                        .map_err(map_db)?;
                } else {
                    parent.state = WorkItemState::Succeeded;
                    parent.child_work_item_id = None;
                }
            }
            WorkItemState::Failed | WorkItemState::Cancelled => {
                if let Some(next) = &step.on_failure {
                    parent.current_step = next.clone();
                    parent.state = WorkItemState::Running;
                    parent.child_work_item_id = None;
                    self.ensure_pending_job_for_step(&parent, &workflow)
                        .await
                        .map_err(map_db)?;
                } else {
                    parent.state = WorkItemState::Failed;
                    parent.child_work_item_id = None;
                }
            }
            _ => {}
        }

        parent.updated_at = Utc::now();
        parent.blocked_on = block_reason_for_state_name(&parent.current_step);
        parent.projected_state = Some(project_workflow_state(&parent));
        self.update_work_item(&parent).await.map_err(map_db)?;
        self.persist_task_projection(&parent, None)
            .await
            .map_err(map_config)?;
        Ok(())
    }

    async fn cancel_work_item(&self, id: Uuid) -> anyhow::Result<()> {
        if let Some(mut item) = self.get_work_item(id).await? {
            item.state = WorkItemState::Cancelled;
            item.updated_at = Utc::now();
            item.blocked_on = block_reason_for_state_name(&item.current_step);
            item.projected_state = Some(project_workflow_state(&item));
            self.update_work_item(&item).await?;
            let _ = self.cancel_active_jobs_for_work_item(id).await?;
            let _ = self.persist_task_projection(&item, None).await;
        }
        Ok(())
    }

    async fn retry_job(&self, previous: &Job) -> anyhow::Result<()> {
        let mut next = previous.clone();
        next.id = Uuid::new_v4();
        next.state = JobState::Pending;
        next.assigned_worker_id = None;
        next.transcript_dir = None;
        next.result = None;
        next.lease_expires_at = None;
        next.attempt = previous.attempt.saturating_add(1);
        next.created_at = Utc::now();
        next.updated_at = next.created_at;
        if let Some(item) = self.get_work_item(previous.work_item_id).await? {
            let valid_next_events = self.projected_valid_next_events_for_job_context(&item);
            project_workflow_inputs_into_job_context(
                &mut next.context,
                &valid_next_events,
                &item,
                &item.context,
            );
        }
        self.insert_job(&next).await
    }

    fn merge_job_output_into_context(&self, item: &mut WorkItem, job: &Job) -> Result<(), String> {
        let raw_output_json = job.result.as_ref().and_then(|r| r.output_json.clone());
        if let Some(raw_output) = raw_output_json.as_ref()
            && let Ok(parsed) = serde_json::from_value::<AgentOutput>(raw_output.clone())
            && let Some(repo) = parsed.repo
        {
            if is_valid_repo_source(&repo) {
                item.repo = Some(repo);
            } else {
                tracing::warn!(
                    work_item_id = %item.id,
                    repo_url = %repo.repo_url,
                    "ignoring invalid repo discovered in agent output"
                );
            }
        }
        let output = raw_output_json.unwrap_or(serde_json::Value::Null);
        let result_json = job
            .result
            .as_ref()
            .map(serde_json::to_value)
            .transpose()
            .map_err(|e| format!("failed to serialize job result: {e}"))?
            .unwrap_or(serde_json::Value::Null);

        if !item.context.is_object() {
            item.context = serde_json::Value::Object(serde_json::Map::new());
        }
        let ctx_obj = item
            .context
            .as_object_mut()
            .ok_or_else(|| "failed to normalize work item context object".to_string())?;

        let outputs = ctx_obj
            .entry(CONTEXT_JOB_OUTPUTS_KEY.to_string())
            .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));

        let outputs_obj = outputs
            .as_object_mut()
            .ok_or_else(|| format!("context.{CONTEXT_JOB_OUTPUTS_KEY} must be a JSON object"))?;

        outputs_obj.insert(job.step_id.clone(), output);

        let results = ctx_obj
            .entry(CONTEXT_JOB_RESULTS_KEY.to_string())
            .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
        let results_obj = results
            .as_object_mut()
            .ok_or_else(|| format!("context.{CONTEXT_JOB_RESULTS_KEY} must be a JSON object"))?;
        results_obj.insert(job.step_id.clone(), result_json);

        let step_artifact_paths = job
            .result
            .as_ref()
            .map(job_structured_artifact_paths)
            .unwrap_or_default();
        let step_artifact_metadata = job
            .result
            .as_ref()
            .map(job_structured_artifact_metadata)
            .unwrap_or_default();
        let step_artifact_data = job
            .result
            .as_ref()
            .map(job_structured_artifact_data)
            .unwrap_or_default();
        let artifact_paths = ctx_obj
            .entry(CONTEXT_JOB_ARTIFACT_PATHS_KEY.to_string())
            .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
        let artifact_paths_obj = artifact_paths.as_object_mut().ok_or_else(|| {
            format!("context.{CONTEXT_JOB_ARTIFACT_PATHS_KEY} must be a JSON object")
        })?;
        if step_artifact_paths.is_empty() {
            artifact_paths_obj.remove(&job.step_id);
        } else {
            artifact_paths_obj.insert(
                job.step_id.clone(),
                serde_json::Value::Object(step_artifact_paths),
            );
        }

        let artifact_metadata = ctx_obj
            .entry(CONTEXT_JOB_ARTIFACT_METADATA_KEY.to_string())
            .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
        let artifact_metadata_obj = artifact_metadata.as_object_mut().ok_or_else(|| {
            format!("context.{CONTEXT_JOB_ARTIFACT_METADATA_KEY} must be a JSON object")
        })?;
        if step_artifact_metadata.is_empty() {
            artifact_metadata_obj.remove(&job.step_id);
        } else {
            artifact_metadata_obj.insert(
                job.step_id.clone(),
                serde_json::Value::Object(step_artifact_metadata),
            );
        }

        let artifact_data = ctx_obj
            .entry(CONTEXT_JOB_ARTIFACT_DATA_KEY.to_string())
            .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
        let artifact_data_obj = artifact_data.as_object_mut().ok_or_else(|| {
            format!("context.{CONTEXT_JOB_ARTIFACT_DATA_KEY} must be a JSON object")
        })?;
        if step_artifact_data.is_empty() {
            artifact_data_obj.remove(&job.step_id);
        } else {
            artifact_data_obj.insert(
                job.step_id.clone(),
                serde_json::Value::Object(step_artifact_data),
            );
        }

        Ok(())
    }

    async fn has_active_job(&self, work_item_id: Uuid) -> anyhow::Result<bool> {
        let path = Arc::clone(&self.db_path);
        tokio::task::spawn_blocking(move || -> anyhow::Result<bool> {
            let conn = Connection::open(path.as_path())?;
            let exists: Option<String> = conn
                .query_row(
                    "SELECT id FROM jobs WHERE work_item_id = ?1 AND state IN (?2, ?3, ?4) LIMIT 1",
                    params![
                        work_item_id.to_string(),
                        JobState::Pending.to_string(),
                        JobState::Assigned.to_string(),
                        JobState::Running.to_string(),
                    ],
                    |row| row.get(0),
                )
                .optional()?;
            Ok(exists.is_some())
        })
        .await?
    }

    async fn cancel_active_jobs_for_work_item(&self, work_item_id: Uuid) -> anyhow::Result<usize> {
        let path = Arc::clone(&self.db_path);
        tokio::task::spawn_blocking(move || -> anyhow::Result<usize> {
            let conn = Connection::open(path.as_path())?;
            let count = conn.execute(
                "UPDATE jobs SET state = ?1, lease_expires_at = NULL, updated_at = ?2 WHERE work_item_id = ?3 AND state IN (?4, ?5, ?6)",
                params![
                    JobState::Cancelled.to_string(),
                    Utc::now().to_rfc3339(),
                    work_item_id.to_string(),
                    JobState::Pending.to_string(),
                    JobState::Assigned.to_string(),
                    JobState::Running.to_string(),
                ],
            )?;
            Ok(count)
        })
        .await?
    }

    async fn ensure_pending_job_for_step(
        &self,
        item: &WorkItem,
        workflow: &WorkflowDefinition,
    ) -> anyhow::Result<()> {
        let step = workflow
            .steps
            .iter()
            .find(|s| s.id == item.current_step)
            .ok_or_else(|| anyhow::anyhow!("missing step {}", item.current_step))?;

        if self.has_pending_job_for_step(item.id, &step.id).await? {
            return Ok(());
        }

        if step.kind == WorkflowStepKind::ChildWorkflow {
            let child_workflow_id = step
                .child_workflow_id
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("missing child_workflow_id for step {}", step.id))?;
            if item.child_work_item_id.is_some() {
                return Ok(());
            }
            let child_workflow = self
                .workflows
                .get(child_workflow_id)
                .ok_or_else(|| anyhow::anyhow!("unknown child workflow {child_workflow_id}"))?
                .clone();
            // Belt-and-suspenders guard: catalog validation should reject nested child workflows,
            // but we keep this runtime check to avoid silently stalling if invalid data slips through.
            if child_workflow
                .steps
                .iter()
                .any(|s| s.kind == WorkflowStepKind::ChildWorkflow)
            {
                return Err(anyhow::anyhow!(
                    "nested child workflows are not supported for child workflow '{}'",
                    child_workflow_id
                ));
            }
            let now = Utc::now();
            let child_id = Uuid::new_v4();
            let child = WorkItem {
                id: child_id,
                workflow_id: child_workflow.id.clone(),
                external_task_id: None,
                external_task_source: None,
                title: item.title.clone(),
                description: item.description.clone(),
                labels: item.labels.clone(),
                priority: item.priority.clone(),
                session_id: item.session_id.clone(),
                current_step: child_workflow.entry_step.clone(),
                projected_state: Some("pending".to_string()),
                triage_status: item.triage_status.clone(),
                human_gate: item.human_gate,
                human_gate_state: item.human_gate_state.clone(),
                blocked_on: None,
                state: WorkItemState::Pending,
                context: item.context.clone(),
                repo: item.repo.clone(),
                parent_work_item_id: Some(item.id),
                parent_step_id: Some(step.id.clone()),
                child_work_item_id: None,
                created_at: now,
                updated_at: now,
            };
            self.insert_work_item(&child).await?;
            self.persist_task_projection(&child, None)
                .await
                .map_err(map_config)?;

            let mut parent = item.clone();
            parent.state = WorkItemState::Running;
            parent.child_work_item_id = Some(child_id);
            parent.updated_at = now;
            parent.blocked_on = block_reason_for_state_name(&parent.current_step);
            self.update_work_item(&parent).await?;
            self.persist_task_projection(&parent, None)
                .await
                .map_err(map_config)?;

            let child_step = child_workflow
                .steps
                .iter()
                .find(|s| s.id == child.current_step)
                .ok_or_else(|| anyhow::anyhow!("missing child entry step"))?;
            if child_step.kind == WorkflowStepKind::Agent {
                let command = interpolate_template(&child_step.command, &child.context)?;
                let args = child_step
                    .args
                    .iter()
                    .map(|a| interpolate_template(a, &child.context))
                    .collect::<Result<Vec<_>, _>>()?;
                let env = child_step
                    .env
                    .iter()
                    .map(|(k, v)| Ok((k.clone(), interpolate_template(v, &child.context)?)))
                    .collect::<Result<HashMap<_, _>, InterpolationError>>()?;
                let job = Job {
                    id: Uuid::new_v4(),
                    work_item_id: child.id,
                    step_id: child_step.id.clone(),
                    state: JobState::Pending,
                    assigned_worker_id: None,
                    execution_mode: child_step.executor.clone(),
                    container_image: child_step.container_image.clone(),
                    command,
                    args,
                    env,
                    prompt: child_step.prompt.clone(),
                    context: child.context.clone(),
                    repo: child.repo.clone(),
                    timeout_secs: child_step.timeout_secs,
                    max_retries: child_step.max_retries,
                    attempt: 0,
                    transcript_dir: None,
                    result: None,
                    lease_expires_at: None,
                    created_at: now,
                    updated_at: now,
                };
                let runtime = canonical_runtime_spec_for_job(&job);
                let mut job = job;
                let valid_next_events = self.projected_valid_next_events_for_job_context(&child);
                project_workflow_inputs_into_job_context(
                    &mut job.context,
                    &valid_next_events,
                    &child,
                    &child.context,
                );
                project_runtime_spec_into_job_context(&mut job.context, &runtime);
                self.insert_job(&job).await?;
            }

            return Ok(());
        }

        let command = interpolate_template(&step.command, &item.context)?;
        let args = step
            .args
            .iter()
            .map(|a| interpolate_template(a, &item.context))
            .collect::<Result<Vec<_>, _>>()?;
        let env = step
            .env
            .iter()
            .map(|(k, v)| Ok((k.clone(), interpolate_template(v, &item.context)?)))
            .collect::<Result<HashMap<_, _>, InterpolationError>>()?;

        let now = Utc::now();
        let job = Job {
            id: Uuid::new_v4(),
            work_item_id: item.id,
            step_id: step.id.clone(),
            state: JobState::Pending,
            assigned_worker_id: None,
            execution_mode: step.executor.clone(),
            container_image: step.container_image.clone(),
            command,
            args,
            env,
            prompt: step.prompt.clone(),
            context: item.context.clone(),
            repo: item.repo.clone(),
            timeout_secs: step.timeout_secs,
            max_retries: step.max_retries,
            attempt: 0,
            transcript_dir: None,
            result: None,
            lease_expires_at: None,
            created_at: now,
            updated_at: now,
        };
        let runtime = canonical_runtime_spec_for_job(&job);
        let mut job = job;
        let valid_next_events = self.projected_valid_next_events_for_job_context(item);
        project_workflow_inputs_into_job_context(
            &mut job.context,
            &valid_next_events,
            item,
            &item.context,
        );
        project_runtime_spec_into_job_context(&mut job.context, &runtime);
        self.insert_job(&job).await
    }

    async fn has_pending_job_for_step(
        &self,
        work_item_id: Uuid,
        step_id: &str,
    ) -> anyhow::Result<bool> {
        let path = Arc::clone(&self.db_path);
        let step_id = step_id.to_string();
        tokio::task::spawn_blocking(move || -> anyhow::Result<bool> {
            let conn = Connection::open(path.as_path())?;
            let exists: Option<String> = conn
                .query_row(
                    "SELECT id FROM jobs WHERE work_item_id = ?1 AND step_id = ?2 AND state IN (?3, ?4, ?5) LIMIT 1",
                    params![
                        work_item_id.to_string(),
                        step_id,
                        JobState::Pending.to_string(),
                        JobState::Assigned.to_string(),
                        JobState::Running.to_string(),
                    ],
                    |row| row.get(0),
                )
                .optional()?;
            Ok(exists.is_some())
        })
        .await?
    }

    async fn insert_work_item(&self, item: &WorkItem) -> anyhow::Result<()> {
        let path = Arc::clone(&self.db_path);
        let item = item.clone();
        tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            let conn = Connection::open(path.as_path())?;
            conn.execute(
                "INSERT INTO work_items (id, workflow_id, external_task_id, external_task_source, title, description, labels_json, priority, session_id, current_step, projected_state, triage_status, human_gate, human_gate_state, blocked_on, state, context_json, repo_json, parent_work_item_id, parent_step_id, child_work_item_id, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23)",
                params![
                    item.id.to_string(),
                    item.workflow_id,
                    item.external_task_id,
                    item.external_task_source,
                    item.title,
                    item.description,
                    serde_json::to_string(&item.labels)?,
                    item.priority,
                    item.session_id.map(|s| s.to_string()),
                    item.current_step,
                    item.projected_state,
                    item.triage_status,
                    if item.human_gate { 1 } else { 0 },
                    item.human_gate_state,
                    item.blocked_on
                        .as_ref()
                        .map(serde_json::to_string)
                        .transpose()?,
                    item.state.to_string(),
                    serde_json::to_string(&item.context)?,
                    item.repo.as_ref().map(serde_json::to_string).transpose()?,
                    item.parent_work_item_id.map(|v| v.to_string()),
                    item.parent_step_id,
                    item.child_work_item_id.map(|v| v.to_string()),
                    item.created_at.to_rfc3339(),
                    item.updated_at.to_rfc3339(),
                ],
            )?;
            Ok(())
        })
        .await?
    }

    async fn update_work_item(&self, item: &WorkItem) -> anyhow::Result<()> {
        let path = Arc::clone(&self.db_path);
        let item = item.clone();
        tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            let conn = Connection::open(path.as_path())?;
            conn.execute(
                "UPDATE work_items SET workflow_id = ?2, external_task_id = ?3, external_task_source = ?4, title = ?5, description = ?6, labels_json = ?7, priority = ?8, current_step = ?9, projected_state = ?10, triage_status = ?11, human_gate = ?12, human_gate_state = ?13, blocked_on = ?14, state = ?15, context_json = ?16, repo_json = ?17, parent_work_item_id = ?18, parent_step_id = ?19, child_work_item_id = ?20, updated_at = ?21 WHERE id = ?1",
                params![
                    item.id.to_string(),
                    item.workflow_id,
                    item.external_task_id,
                    item.external_task_source,
                    item.title,
                    item.description,
                    serde_json::to_string(&item.labels)?,
                    item.priority,
                    item.current_step,
                    item.projected_state,
                    item.triage_status,
                    if item.human_gate { 1 } else { 0 },
                    item.human_gate_state,
                    item.blocked_on
                        .as_ref()
                        .map(serde_json::to_string)
                        .transpose()?,
                    item.state.to_string(),
                    serde_json::to_string(&item.context)?,
                    item.repo.as_ref().map(serde_json::to_string).transpose()?,
                    item.parent_work_item_id.map(|v| v.to_string()),
                    item.parent_step_id,
                    item.child_work_item_id.map(|v| v.to_string()),
                    item.updated_at.to_rfc3339(),
                ],
            )?;
            Ok(())
        })
        .await?
    }

    pub(crate) async fn get_work_item(&self, id: Uuid) -> anyhow::Result<Option<WorkItem>> {
        let path = Arc::clone(&self.db_path);
        tokio::task::spawn_blocking(move || -> anyhow::Result<Option<WorkItem>> {
            let conn = Connection::open(path.as_path())?;
            let mut stmt = conn.prepare(
                "SELECT id, workflow_id, external_task_id, external_task_source, title, description, labels_json, priority, session_id, current_step, projected_state, triage_status, human_gate, human_gate_state, blocked_on, state, context_json, repo_json, parent_work_item_id, parent_step_id, child_work_item_id, created_at, updated_at FROM work_items WHERE id = ?1",
            )?;
            let item = stmt
                .query_row([id.to_string()], row_to_work_item)
                .optional()?;
            Ok(item)
        })
        .await?
    }

    async fn select_work_items(&self) -> anyhow::Result<Vec<WorkItem>> {
        let path = Arc::clone(&self.db_path);
        tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<WorkItem>> {
            let conn = Connection::open(path.as_path())?;
            let mut stmt = conn.prepare(
                "SELECT id, workflow_id, external_task_id, external_task_source, title, description, labels_json, priority, session_id, current_step, projected_state, triage_status, human_gate, human_gate_state, blocked_on, state, context_json, repo_json, parent_work_item_id, parent_step_id, child_work_item_id, created_at, updated_at FROM work_items ORDER BY created_at ASC",
            )?;
            let rows = stmt.query_map([], row_to_work_item)?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            Ok(out)
        })
        .await?
    }

    async fn select_children_for_parent_step(
        &self,
        parent_id: Uuid,
        parent_step_id: &str,
    ) -> anyhow::Result<Vec<WorkItem>> {
        let path = Arc::clone(&self.db_path);
        let parent_step_id = parent_step_id.to_string();
        tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<WorkItem>> {
            let conn = Connection::open(path.as_path())?;
            let mut stmt = conn.prepare(
                "SELECT id, workflow_id, external_task_id, external_task_source, title, description, labels_json, priority, session_id, current_step, projected_state, triage_status, human_gate, human_gate_state, blocked_on, state, context_json, repo_json, parent_work_item_id, parent_step_id, child_work_item_id, created_at, updated_at FROM work_items WHERE parent_work_item_id = ?1 AND parent_step_id = ?2 ORDER BY created_at ASC",
            )?;
            let rows =
                stmt.query_map(params![parent_id.to_string(), parent_step_id], row_to_work_item)?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            Ok(out)
        })
        .await?
    }

    async fn select_work_item_state_counts(&self) -> anyhow::Result<HashMap<String, usize>> {
        let path = Arc::clone(&self.db_path);
        tokio::task::spawn_blocking(move || -> anyhow::Result<HashMap<String, usize>> {
            let conn = Connection::open(path.as_path())?;
            let mut stmt = conn.prepare("SELECT state, COUNT(*) FROM work_items GROUP BY state")?;
            let rows = stmt.query_map([], |row| {
                let state: String = row.get(0)?;
                let count: i64 = row.get(1)?;
                Ok((state, count))
            })?;
            let mut out = HashMap::new();
            for row in rows {
                let (state, count) = row?;
                out.insert(state, usize::try_from(count).unwrap_or_default());
            }
            Ok(out)
        })
        .await?
    }

    async fn insert_job(&self, job: &Job) -> anyhow::Result<()> {
        let path = Arc::clone(&self.db_path);
        let mut job = job.clone();
        let runtime = canonical_runtime_spec_for_job_effective(&job);
        project_runtime_spec_into_job_context(&mut job.context, &runtime);
        tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            let conn = Connection::open(path.as_path())?;
            conn.execute(
                "INSERT INTO jobs (id, work_item_id, step_id, state, assigned_worker_id, execution_mode, container_image, command, args_json, env_json, prompt_json, context_json, repo_json, timeout_secs, max_retries, attempt, transcript_dir, result_json, lease_expires_at, created_at, updated_at, runtime_spec_json) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22)",
                params![
                    job.id.to_string(),
                    job.work_item_id.to_string(),
                    job.step_id,
                    job.state.to_string(),
                    job.assigned_worker_id,
                    job.execution_mode.to_string(),
                    job.container_image,
                    job.command,
                    serde_json::to_string(&job.args)?,
                    serde_json::to_string(&job.env)?,
                    job.prompt.as_ref().map(serde_json::to_string).transpose()?,
                    serde_json::to_string(&job.context)?,
                    job.repo.as_ref().map(serde_json::to_string).transpose()?,
                    job.timeout_secs,
                    job.max_retries,
                    job.attempt,
                    job.transcript_dir,
                    job.result.as_ref().map(serde_json::to_string).transpose()?,
                    job.lease_expires_at.map(|t| t.to_rfc3339()),
                    job.created_at.to_rfc3339(),
                    job.updated_at.to_rfc3339(),
                    serde_json::to_string(&runtime)?,
                ],
            )?;
            Ok(())
        })
        .await?
    }

    async fn select_jobs(&self) -> anyhow::Result<Vec<Job>> {
        let path = Arc::clone(&self.db_path);
        tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<Job>> {
            let conn = Connection::open(path.as_path())?;
            let mut stmt = conn.prepare(
                "SELECT id, work_item_id, step_id, state, assigned_worker_id, execution_mode, container_image, command, args_json, env_json, prompt_json, context_json, repo_json, timeout_secs, max_retries, attempt, transcript_dir, result_json, lease_expires_at, created_at, updated_at, runtime_spec_json FROM jobs ORDER BY created_at ASC",
            )?;
            let rows = stmt.query_map([], row_to_job)?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            Ok(out)
        })
        .await?
    }

    async fn select_jobs_with_transcripts(&self) -> anyhow::Result<Vec<Job>> {
        let path = Arc::clone(&self.db_path);
        tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<Job>> {
            let conn = Connection::open(path.as_path())?;
            let mut stmt = conn.prepare(
                "SELECT id, work_item_id, step_id, state, assigned_worker_id, execution_mode, container_image, command, args_json, env_json, prompt_json, context_json, repo_json, timeout_secs, max_retries, attempt, transcript_dir, result_json, lease_expires_at, created_at, updated_at, runtime_spec_json FROM jobs WHERE transcript_dir IS NOT NULL ORDER BY created_at DESC",
            )?;
            let rows = stmt.query_map([], row_to_job)?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            Ok(out)
        })
        .await?
    }

    async fn select_job_state_counts(&self) -> anyhow::Result<HashMap<String, usize>> {
        let path = Arc::clone(&self.db_path);
        tokio::task::spawn_blocking(move || -> anyhow::Result<HashMap<String, usize>> {
            let conn = Connection::open(path.as_path())?;
            let mut stmt = conn.prepare("SELECT state, COUNT(*) FROM jobs GROUP BY state")?;
            let rows = stmt.query_map([], |row| {
                let state: String = row.get(0)?;
                let count: i64 = row.get(1)?;
                Ok((state, count))
            })?;
            let mut out = HashMap::new();
            for row in rows {
                let (state, count) = row?;
                out.insert(state, usize::try_from(count).unwrap_or_default());
            }
            Ok(out)
        })
        .await?
    }

    async fn select_recent_terminal_jobs(&self, limit: usize) -> anyhow::Result<Vec<Job>> {
        let path = Arc::clone(&self.db_path);
        tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<Job>> {
            let conn = Connection::open(path.as_path())?;
            let mut stmt = conn.prepare(
                "SELECT id, work_item_id, step_id, state, assigned_worker_id, execution_mode, container_image, command, args_json, env_json, prompt_json, context_json, repo_json, timeout_secs, max_retries, attempt, transcript_dir, result_json, lease_expires_at, created_at, updated_at, runtime_spec_json FROM jobs WHERE state IN (?1, ?2) ORDER BY updated_at DESC LIMIT ?3",
            )?;
            let rows = stmt.query_map(
                params![
                    JobState::Succeeded.to_string(),
                    JobState::Failed.to_string(),
                    i64::try_from(limit).unwrap_or(i64::MAX),
                ],
                row_to_job,
            )?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            Ok(out)
        })
        .await?
    }

    async fn select_jobs_for_work_item(&self, work_item_id: Uuid) -> anyhow::Result<Vec<Job>> {
        let path = Arc::clone(&self.db_path);
        tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<Job>> {
            let conn = Connection::open(path.as_path())?;
            let mut stmt = conn.prepare(
                "SELECT id, work_item_id, step_id, state, assigned_worker_id, execution_mode, container_image, command, args_json, env_json, prompt_json, context_json, repo_json, timeout_secs, max_retries, attempt, transcript_dir, result_json, lease_expires_at, created_at, updated_at, runtime_spec_json FROM jobs WHERE work_item_id = ?1 ORDER BY created_at ASC",
            )?;
            let rows = stmt.query_map([work_item_id.to_string()], row_to_job)?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            Ok(out)
        })
        .await?
    }

    pub(crate) async fn get_job(&self, id: Uuid) -> anyhow::Result<Option<Job>> {
        let path = Arc::clone(&self.db_path);
        tokio::task::spawn_blocking(move || -> anyhow::Result<Option<Job>> {
            let conn = Connection::open(path.as_path())?;
            let mut stmt = conn.prepare(
                "SELECT id, work_item_id, step_id, state, assigned_worker_id, execution_mode, container_image, command, args_json, env_json, prompt_json, context_json, repo_json, timeout_secs, max_retries, attempt, transcript_dir, result_json, lease_expires_at, created_at, updated_at, runtime_spec_json FROM jobs WHERE id = ?1",
            )?;
            let job = stmt.query_row([id.to_string()], row_to_job).optional()?;
            Ok(job)
        })
        .await?
    }

    async fn assign_next_job(
        &self,
        worker_id: String,
        supported_modes: Vec<protocol::ExecutionMode>,
    ) -> anyhow::Result<Option<Job>> {
        let path = Arc::clone(&self.db_path);
        tokio::task::spawn_blocking(move || -> anyhow::Result<Option<Job>> {
            let conn = Connection::open(path.as_path())?;
            conn.execute_batch("BEGIN IMMEDIATE TRANSACTION")?;
            let mut stmt = conn.prepare(
                "SELECT id, work_item_id, step_id, state, assigned_worker_id, execution_mode, container_image, command, args_json, env_json, prompt_json, context_json, repo_json, timeout_secs, max_retries, attempt, transcript_dir, result_json, lease_expires_at, created_at, updated_at, runtime_spec_json FROM jobs WHERE state = ?1 ORDER BY created_at ASC",
            )?;
            let rows = stmt.query_map([JobState::Pending.to_string()], row_to_job)?;
            for row in rows {
                let mut job = row?;
                let runtime_environment = canonical_runtime_spec_for_job_effective(&job).environment;
                if !supported_modes.contains(&runtime_environment.execution_mode) {
                    continue;
                }
                job.execution_mode = runtime_environment.execution_mode;
                job.container_image = runtime_environment.container_image;
                job.timeout_secs = runtime_environment.timeout_secs;
                job.state = JobState::Assigned;
                job.assigned_worker_id = Some(worker_id.clone());
                job.lease_expires_at =
                    Some(Utc::now() + chrono::TimeDelta::seconds(LEASE_TTL_SECS));
                job.updated_at = Utc::now();
                let updated = conn.execute(
                    "UPDATE jobs SET state = ?2, assigned_worker_id = ?3, lease_expires_at = ?4, updated_at = ?5 WHERE id = ?1 AND state = ?6",
                    params![
                        job.id.to_string(),
                        job.state.to_string(),
                        job.assigned_worker_id,
                        job.lease_expires_at.map(|t| t.to_rfc3339()),
                        job.updated_at.to_rfc3339(),
                        JobState::Pending.to_string(),
                    ],
                )?;
                if updated == 1 {
                    conn.execute_batch("COMMIT")?;
                    return Ok(Some(job));
                }
            }
            conn.execute_batch("COMMIT")?;
            Ok(None)
        })
        .await?
    }

    async fn update_job_row(&self, job: &Job) -> anyhow::Result<()> {
        let path = Arc::clone(&self.db_path);
        let job = job.clone();
        tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            let conn = Connection::open(path.as_path())?;
            conn.execute(
                "UPDATE jobs SET state = ?2, assigned_worker_id = ?3, transcript_dir = ?4, result_json = ?5, lease_expires_at = ?6, updated_at = ?7 WHERE id = ?1",
                params![
                    job.id.to_string(),
                    job.state.to_string(),
                    job.assigned_worker_id,
                    job.transcript_dir,
                    job.result.as_ref().map(serde_json::to_string).transpose()?,
                    job.lease_expires_at.map(|t| t.to_rfc3339()),
                    job.updated_at.to_rfc3339(),
                ],
            )?;
            Ok(())
        })
        .await?
    }

    async fn upsert_worker(&self, worker: &WorkerInfo) -> anyhow::Result<()> {
        let path = Arc::clone(&self.db_path);
        let worker = worker.clone();
        tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            let conn = Connection::open(path.as_path())?;
            conn.execute(
                "INSERT INTO workers (id, hostname, state, supported_modes_json, last_heartbeat_at) VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(id) DO UPDATE SET hostname = excluded.hostname, state = excluded.state, supported_modes_json = excluded.supported_modes_json, last_heartbeat_at = excluded.last_heartbeat_at",
                params![
                    worker.id,
                    worker.hostname,
                    worker.state.to_string(),
                    serde_json::to_string(&worker.supported_modes)?,
                    worker.last_heartbeat_at.to_rfc3339(),
                ],
            )?;
            Ok(())
        })
        .await?
    }

    async fn get_worker(&self, worker_id: &str) -> anyhow::Result<Option<WorkerInfo>> {
        let path = Arc::clone(&self.db_path);
        let worker_id = worker_id.to_string();
        tokio::task::spawn_blocking(move || -> anyhow::Result<Option<WorkerInfo>> {
            let conn = Connection::open(path.as_path())?;
            let mut stmt = conn.prepare(
                "SELECT id, hostname, state, supported_modes_json, last_heartbeat_at FROM workers WHERE id = ?1",
            )?;
            let worker = stmt
                .query_row([worker_id], |row| {
                    Ok(WorkerInfo {
                        id: row.get(0)?,
                        hostname: row.get(1)?,
                        state: row.get::<_, String>(2)?.parse().map_err(invalid_enum)?,
                        supported_modes: serde_json::from_str(&row.get::<_, String>(3)?)
                            .map_err(invalid_json)?,
                        last_heartbeat_at: parse_datetime(&row.get::<_, String>(4)?)?,
                    })
                })
                .optional()?;
            Ok(worker)
        })
        .await?
    }

    async fn select_workers(&self) -> anyhow::Result<Vec<WorkerInfo>> {
        let path = Arc::clone(&self.db_path);
        tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<WorkerInfo>> {
            let conn = Connection::open(path.as_path())?;
            let mut stmt = conn.prepare(
                "SELECT id, hostname, state, supported_modes_json, last_heartbeat_at FROM workers ORDER BY id ASC",
            )?;
            let rows = stmt.query_map([], |row| {
                Ok(WorkerInfo {
                    id: row.get(0)?,
                    hostname: row.get(1)?,
                    state: row.get::<_, String>(2)?.parse().map_err(invalid_enum)?,
                    supported_modes: serde_json::from_str(&row.get::<_, String>(3)?)
                        .map_err(invalid_json)?,
                    last_heartbeat_at: parse_datetime(&row.get::<_, String>(4)?)?,
                })
            })?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            Ok(out)
        })
        .await?
    }

    async fn select_worker_state_counts(&self) -> anyhow::Result<HashMap<String, usize>> {
        let path = Arc::clone(&self.db_path);
        tokio::task::spawn_blocking(move || -> anyhow::Result<HashMap<String, usize>> {
            let conn = Connection::open(path.as_path())?;
            let mut stmt = conn.prepare("SELECT state, COUNT(*) FROM workers GROUP BY state")?;
            let rows = stmt.query_map([], |row| {
                let state: String = row.get(0)?;
                let count: i64 = row.get(1)?;
                Ok((state, count))
            })?;
            let mut out = HashMap::new();
            for row in rows {
                let (state, count) = row?;
                out.insert(state, usize::try_from(count).unwrap_or_default());
            }
            Ok(out)
        })
        .await?
    }

    async fn select_human_gate_work_items(&self, limit: usize) -> anyhow::Result<Vec<WorkItem>> {
        let path = Arc::clone(&self.db_path);
        tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<WorkItem>> {
            let conn = Connection::open(path.as_path())?;
            let mut stmt = conn.prepare(
                "SELECT id, workflow_id, external_task_id, external_task_source, title, description, labels_json, priority, session_id, current_step, projected_state, triage_status, human_gate, human_gate_state, blocked_on, state, context_json, repo_json, parent_work_item_id, parent_step_id, child_work_item_id, created_at, updated_at FROM work_items WHERE human_gate = 1 ORDER BY updated_at DESC LIMIT ?1",
            )?;
            let rows = stmt.query_map([i64::try_from(limit).unwrap_or(i64::MAX)], row_to_work_item)?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            Ok(out)
        })
        .await?
    }

    async fn select_triage_work_items(&self, limit: usize) -> anyhow::Result<Vec<WorkItem>> {
        let path = Arc::clone(&self.db_path);
        tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<WorkItem>> {
            let conn = Connection::open(path.as_path())?;
            let mut stmt = conn.prepare(
                "SELECT id, workflow_id, external_task_id, external_task_source, title, description, labels_json, priority, session_id, current_step, projected_state, triage_status, human_gate, human_gate_state, blocked_on, state, context_json, repo_json, parent_work_item_id, parent_step_id, child_work_item_id, created_at, updated_at FROM work_items WHERE triage_status IS NOT NULL AND triage_status != '' AND triage_status != 'ready' ORDER BY updated_at DESC LIMIT ?1",
            )?;
            let rows = stmt.query_map([i64::try_from(limit).unwrap_or(i64::MAX)], row_to_work_item)?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            Ok(out)
        })
        .await?
    }
}

fn ensure_repo_catalog_hint_context(item: &mut WorkItem) {
    if item.workflow_id != SIMPLE_CODE_CHANGE_WORKFLOW_ID {
        return;
    }

    if !item.context.is_object() {
        item.context = serde_json::json!({});
    }
    let Some(context) = item.context.as_object_mut() else {
        return;
    };

    context
        .entry(CONTEXT_REPO_CATALOG_HINT_KEY.to_string())
        .or_insert_with(|| {
            serde_json::Value::String(SIMPLE_CODE_CHANGE_REPO_CATALOG_HINT.to_string())
        });
    context
        .entry(CONTEXT_REPO_CATALOG_KEY.to_string())
        .or_insert_with(|| {
            serde_json::json!([
                {
                    "repo_url": "https://github.com/nickdavies/agent-hub-poc-test.git",
                    "description": "proof-path repository for validating simple-code-change repo discovery"
                }
            ])
        });
}

fn requires_repo_discovery(item: &WorkItem) -> bool {
    item.workflow_id == SIMPLE_CODE_CHANGE_WORKFLOW_ID
        && item.current_step == SIMPLE_CODE_CHANGE_TRIAGE_STATE_ID
}

fn is_valid_repo_source(repo: &protocol::RepoSource) -> bool {
    let repo_url = repo.repo_url.trim();
    let base_ref = repo.base_ref.trim();
    !repo_url.is_empty() && !is_placeholder_repo_url(repo_url) && !base_ref.is_empty()
}

fn is_placeholder_repo_url(repo_url: &str) -> bool {
    let normalized = repo_url.trim().to_ascii_lowercase();
    matches!(
        normalized.as_str(),
        "unknown"
            | "n/a"
            | "none"
            | "null"
            | "placeholder"
            | "repo"
            | "repo_url"
            | "todo"
            | "tbd"
            | "https://github.com/org/repo"
            | "https://example/repo"
            | "https://example/repo.git"
    )
}

#[derive(Debug, Clone, Copy)]
pub struct MaintenanceStats {
    pub requeued_jobs: usize,
    pub created_jobs: usize,
    pub lease_requeued_jobs: usize,
}

fn map_db(e: anyhow::Error) -> AppError {
    AppError::Config(format!("db operation failed: {e}"))
}

fn map_config(e: anyhow::Error) -> AppError {
    AppError::Config(format!("orchestration adapter/config failed: {e}"))
}

fn validate_job_transition(from: JobState, to: JobState) -> Result<(), JobTransitionError> {
    let valid = match from {
        JobState::Pending => matches!(to, JobState::Assigned | JobState::Cancelled),
        JobState::Assigned => matches!(to, JobState::Running | JobState::Cancelled),
        JobState::Running => matches!(
            to,
            JobState::Succeeded | JobState::Failed | JobState::Cancelled
        ),
        JobState::Succeeded | JobState::Failed | JobState::Cancelled => false,
    };
    if valid {
        Ok(())
    } else {
        Err(JobTransitionError::InvalidTransition { from, to })
    }
}

fn interpolate_template(
    template: &str,
    context: &serde_json::Value,
) -> Result<String, InterpolationError> {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;

    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after
            .find("}}")
            .ok_or(InterpolationError::UnterminatedPlaceholder)?;
        let path = after[..end].trim();
        let value = resolve_context_path(context, path)
            .ok_or_else(|| InterpolationError::MissingPath(path.to_string()))?;
        let scalar = match value {
            serde_json::Value::String(v) => v.clone(),
            serde_json::Value::Bool(v) => v.to_string(),
            serde_json::Value::Number(v) => v.to_string(),
            serde_json::Value::Null => String::new(),
            _ => return Err(InterpolationError::NonScalarValue(path.to_string())),
        };
        out.push_str(&scalar);
        rest = &after[end + 2..];
    }

    out.push_str(rest);
    Ok(out)
}

fn interpolation_context_for_work_item(item: &WorkItem, state_name: &str) -> serde_json::Value {
    let mut context = item
        .context
        .as_object()
        .cloned()
        .unwrap_or_else(serde_json::Map::new);

    if item.workflow_id == SIMPLE_CODE_CHANGE_WORKFLOW_ID {
        context
            .entry(CONTEXT_REPO_CATALOG_HINT_KEY.to_string())
            .or_insert_with(|| {
                serde_json::Value::String(SIMPLE_CODE_CHANGE_REPO_CATALOG_HINT.to_string())
            });
        context
            .entry(CONTEXT_REPO_CATALOG_KEY.to_string())
            .or_insert_with(|| {
                serde_json::json!([
                    {
                        "repo_url": "https://github.com/nickdavies/agent-hub-poc-test.git",
                        "description": "proof-path repository for validating simple-code-change repo discovery"
                    }
                ])
            });
    }

    context.insert(
        "title".to_string(),
        serde_json::Value::String(item.title.clone().unwrap_or_default()),
    );
    context.insert(
        "description".to_string(),
        serde_json::Value::String(item.description.clone().unwrap_or_default()),
    );
    context.insert(
        "repo".to_string(),
        serde_json::Value::String(repo_name_for_interpolation(item.repo.as_ref())),
    );
    context.insert(
        "branch".to_string(),
        serde_json::Value::String(
            item.repo
                .as_ref()
                .and_then(|repo| {
                    repo.branch_name
                        .clone()
                        .or_else(|| Some(repo.base_ref.clone()))
                })
                .unwrap_or_default(),
        ),
    );
    context.insert(
        "work_item_id".to_string(),
        serde_json::Value::String(item.id.to_string()),
    );
    context.insert(
        "state".to_string(),
        serde_json::Value::String(state_name.to_string()),
    );

    serde_json::Value::Object(context)
}

fn repo_name_for_interpolation(repo: Option<&protocol::RepoSource>) -> String {
    let Some(repo) = repo else {
        return String::new();
    };

    let trimmed = repo.repo_url.trim_end_matches('/');
    trimmed
        .rsplit_once('/')
        .map(|(_, tail)| tail)
        .or_else(|| trimmed.rsplit_once(':').map(|(_, tail)| tail))
        .unwrap_or(trimmed)
        .trim_end_matches(".git")
        .to_string()
}

fn resolve_context_path<'a>(
    ctx: &'a serde_json::Value,
    path: &str,
) -> Option<&'a serde_json::Value> {
    let mut current = ctx;
    for part in path.split('.') {
        if part.is_empty() {
            return None;
        }
        if let Ok(idx) = part.parse::<usize>() {
            current = current.as_array()?.get(idx)?;
        } else {
            current = current.as_object()?.get(part)?;
        }
    }
    Some(current)
}

fn merge_child_result_into_parent_context(
    parent: &mut WorkItem,
    child: &WorkItem,
) -> Result<(), String> {
    if !parent.context.is_object() {
        parent.context = serde_json::Value::Object(serde_json::Map::new());
    }
    let obj = parent
        .context
        .as_object_mut()
        .ok_or_else(|| "failed to normalize parent context object".to_string())?;

    let entry = obj
        .entry("child_results".to_string())
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    let map = entry
        .as_object_mut()
        .ok_or_else(|| "context.child_results must be an object".to_string())?;

    let key = child
        .parent_step_id
        .clone()
        .unwrap_or_else(|| "unknown".to_string());
    map.insert(
        key,
        serde_json::json!({
            "child_work_item_id": child.id,
            "state": child.state,
            "workflow_id": child.workflow_id,
            "job_outputs": child.context.get(CONTEXT_JOB_OUTPUTS_KEY).cloned().unwrap_or(serde_json::Value::Null),
            "job_results": child.context.get(CONTEXT_JOB_RESULTS_KEY).cloned().unwrap_or(serde_json::Value::Null),
            "job_artifact_paths": child.context.get(CONTEXT_JOB_ARTIFACT_PATHS_KEY).cloned().unwrap_or(serde_json::Value::Null),
            "job_artifact_metadata": child.context.get(CONTEXT_JOB_ARTIFACT_METADATA_KEY).cloned().unwrap_or(serde_json::Value::Null),
            "job_artifact_data": child.context.get(CONTEXT_JOB_ARTIFACT_DATA_KEY).cloned().unwrap_or(serde_json::Value::Null),
        }),
    );
    Ok(())
}

fn job_structured_artifact_paths(
    result: &protocol::JobResult,
) -> serde_json::Map<String, serde_json::Value> {
    let mut out = serde_json::Map::new();
    project_legacy_artifact_summary_paths(&result.artifacts, &mut out);
    for artifact in &result.artifacts.structured_transcript_artifacts {
        out.insert(
            artifact.key.clone(),
            serde_json::Value::String(artifact.path.clone()),
        );
    }
    out
}

fn job_structured_artifact_metadata(
    result: &protocol::JobResult,
) -> serde_json::Map<String, serde_json::Value> {
    let mut out = serde_json::Map::new();
    project_legacy_artifact_summary_metadata(&result.artifacts, &mut out);
    for artifact in &result.artifacts.structured_transcript_artifacts {
        let mut artifact_obj = serde_json::Map::new();
        artifact_obj.insert(
            "path".to_string(),
            serde_json::Value::String(artifact.path.clone()),
        );
        if let Some(bytes) = artifact.bytes {
            artifact_obj.insert("bytes".to_string(), serde_json::Value::Number(bytes.into()));
        }
        if let Some(schema) = &artifact.schema {
            artifact_obj.insert(
                "schema".to_string(),
                serde_json::Value::String(schema.clone()),
            );
        }
        if let Some(record_count) = artifact.record_count {
            artifact_obj.insert(
                "record_count".to_string(),
                serde_json::Value::Number(record_count.into()),
            );
        }
        out.insert(
            artifact.key.clone(),
            serde_json::Value::Object(artifact_obj),
        );
    }
    out
}

fn job_structured_artifact_data(
    result: &protocol::JobResult,
) -> serde_json::Map<String, serde_json::Value> {
    let mut out = serde_json::Map::new();
    for artifact in &result.artifacts.structured_transcript_artifacts {
        if let Some(inline) = structured_artifact_inline_data(artifact) {
            out.insert(artifact.key.clone(), inline);
        }
    }
    out
}

fn non_empty_artifact_path(path: Option<&str>) -> Option<String> {
    path.and_then(|v| {
        let trimmed = v.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

fn project_legacy_artifact_summary_paths(
    artifacts: &protocol::ArtifactSummary,
    out: &mut serde_json::Map<String, serde_json::Value>,
) {
    let entries = [
        (
            "stdout_log",
            non_empty_artifact_path(Some(artifacts.stdout_path.as_str())),
        ),
        (
            "stderr_log",
            non_empty_artifact_path(Some(artifacts.stderr_path.as_str())),
        ),
        (
            "output_json",
            non_empty_artifact_path(artifacts.output_path.as_deref()),
        ),
        (
            "transcript_dir",
            non_empty_artifact_path(artifacts.transcript_dir.as_deref()),
        ),
    ];

    for (key, path) in entries {
        let Some(path) = path else {
            continue;
        };
        out.insert(key.to_string(), serde_json::Value::String(path));
    }
}

fn project_legacy_artifact_summary_metadata(
    artifacts: &protocol::ArtifactSummary,
    out: &mut serde_json::Map<String, serde_json::Value>,
) {
    let entries = [
        (
            "stdout_log",
            non_empty_artifact_path(Some(artifacts.stdout_path.as_str())),
            artifacts.stdout_bytes,
        ),
        (
            "stderr_log",
            non_empty_artifact_path(Some(artifacts.stderr_path.as_str())),
            artifacts.stderr_bytes,
        ),
        (
            "output_json",
            non_empty_artifact_path(artifacts.output_path.as_deref()),
            artifacts.output_bytes,
        ),
        (
            "transcript_dir",
            non_empty_artifact_path(artifacts.transcript_dir.as_deref()),
            None,
        ),
    ];

    for (key, path, bytes) in entries {
        let Some(path) = path else {
            continue;
        };
        let mut artifact_obj = serde_json::Map::new();
        artifact_obj.insert("path".to_string(), serde_json::Value::String(path));
        if let Some(bytes) = bytes {
            artifact_obj.insert("bytes".to_string(), serde_json::Value::Number(bytes.into()));
        }
        out.insert(key.to_string(), serde_json::Value::Object(artifact_obj));
    }
}

fn structured_artifact_inline_data(
    artifact: &protocol::StructuredTranscriptArtifact,
) -> Option<serde_json::Value> {
    match artifact.key.as_str() {
        "transcript_jsonl" => artifact.inline_json.clone(),
        "agent_context"
            if artifact.schema.as_deref() == Some(protocol::AGENT_CONTEXT_SCHEMA_NAME) =>
        {
            artifact.inline_json.clone()
        }
        "run_json" if artifact.schema.as_deref() == Some(protocol::JOB_RUN_SCHEMA_NAME) => {
            artifact.inline_json.clone()
        }
        "job_events" if artifact.schema.as_deref() == Some(protocol::JOB_EVENT_SCHEMA_NAME) => {
            artifact.inline_json.clone()
        }
        _ => None,
    }
}

fn merge_task_metadata(context: &serde_json::Value, task: &TaskToolRecord) -> serde_json::Value {
    let mut out = if context.is_object() {
        context.clone()
    } else {
        serde_json::json!({})
    };
    if let Some(obj) = out.as_object_mut() {
        obj.insert(
            "task_title".to_string(),
            serde_json::Value::String(task.title.clone()),
        );
        if let Some(description) = &task.description {
            obj.insert(
                "task_description".to_string(),
                serde_json::Value::String(description.clone()),
            );
        }
    }
    out
}

fn merge_task_action_revision(
    context: serde_json::Value,
    action_revision: u64,
) -> serde_json::Value {
    let mut ctx = if context.is_object() {
        context
    } else {
        serde_json::json!({})
    };
    let obj = ctx.as_object_mut().expect("context normalized to object");
    let adapter_entry = obj
        .entry("task_adapter".to_string())
        .or_insert_with(|| serde_json::json!({}));
    if let Some(adapter_obj) = adapter_entry.as_object_mut() {
        adapter_obj.insert(
            "last_action_revision".to_string(),
            serde_json::Value::Number(action_revision.into()),
        );
    }
    ctx
}

fn clone_context_object_field_or_empty(
    context: &serde_json::Value,
    key: &str,
) -> serde_json::Value {
    context
        .as_object()
        .and_then(|obj| obj.get(key))
        .and_then(serde_json::Value::as_object)
        .cloned()
        .map(serde_json::Value::Object)
        .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new()))
}

fn project_workflow_inputs_into_job_context(
    context: &mut serde_json::Value,
    valid_next_events: &[WorkflowEventKind],
    item: &WorkItem,
    work_item_context: &serde_json::Value,
) {
    if !context.is_object() {
        *context = serde_json::Value::Object(serde_json::Map::new());
    }
    let Some(ctx_obj) = context.as_object_mut() else {
        return;
    };

    let workflow_entry = ctx_obj
        .entry(CONTEXT_WORKFLOW_KEY.to_string())
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    if !workflow_entry.is_object() {
        *workflow_entry = serde_json::Value::Object(serde_json::Map::new());
    }
    let Some(workflow_obj) = workflow_entry.as_object_mut() else {
        return;
    };

    let inputs_entry = workflow_obj
        .entry(CONTEXT_WORKFLOW_INPUTS_KEY.to_string())
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    if !inputs_entry.is_object() {
        *inputs_entry = serde_json::Value::Object(serde_json::Map::new());
    }
    let Some(inputs_obj) = inputs_entry.as_object_mut() else {
        return;
    };

    let valid_event_names = valid_next_events
        .iter()
        .map(workflow_event_kind_name)
        .map(serde_json::Value::String)
        .collect::<Vec<_>>();
    inputs_obj.insert(
        CONTEXT_VALID_NEXT_EVENTS_KEY.to_string(),
        serde_json::Value::Array(valid_event_names),
    );
    inputs_obj.insert(
        CONTEXT_JOB_ARTIFACT_PATHS_KEY.to_string(),
        clone_context_object_field_or_empty(work_item_context, CONTEXT_JOB_ARTIFACT_PATHS_KEY),
    );
    inputs_obj.insert(
        CONTEXT_JOB_ARTIFACT_METADATA_KEY.to_string(),
        clone_context_object_field_or_empty(work_item_context, CONTEXT_JOB_ARTIFACT_METADATA_KEY),
    );

    ctx_obj.insert(
        CONTEXT_WORK_ITEM_KEY.to_string(),
        serde_json::json!({
            "id": item.id,
            "title": &item.title,
            "description": &item.description,
            "labels": &item.labels,
            "priority": &item.priority,
            "workflow_state": &item.current_step,
            "workflow_id": &item.workflow_id,
            "repo_catalog": work_item_context
                .as_object()
                .and_then(|obj| obj.get(CONTEXT_REPO_CATALOG_KEY))
                .cloned()
                .unwrap_or_else(|| serde_json::Value::Array(Vec::new())),
        }),
    );
}

fn repo_source_from_repo_catalog_entry(entry: &serde_json::Value) -> Option<RepoSource> {
    let obj = entry.as_object()?;
    let repo_url = obj
        .get("repo_url")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())?
        .to_string();
    let base_ref = obj
        .get("base_ref")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("origin/main")
        .to_string();
    let branch_name = obj
        .get("branch_name")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned);
    Some(RepoSource {
        repo_url,
        base_ref,
        branch_name,
    })
}

fn repo_catalog_candidates_from_context(context: &serde_json::Value) -> Vec<RepoSource> {
    context
        .as_object()
        .and_then(|obj| obj.get(CONTEXT_REPO_CATALOG_KEY))
        .and_then(serde_json::Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(repo_source_from_repo_catalog_entry)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
}

fn merged_reactive_candidate_repos(
    execution: Option<&protocol::ReactiveExecutionConfig>,
    context: &serde_json::Value,
) -> Vec<RepoSource> {
    let mut merged = Vec::new();
    if let Some(configured) = execution.map(|cfg| cfg.candidate_repos.clone()) {
        merged.extend(configured);
    }
    merged.extend(repo_catalog_candidates_from_context(context));

    let mut seen = HashSet::new();
    merged
        .into_iter()
        .filter(|repo| {
            let key = repo.repo_url.trim().to_string();
            !key.is_empty() && seen.insert(key)
        })
        .collect()
}

fn project_reactive_execution_markers_into_job_context(
    context: &mut serde_json::Value,
    state_kind: protocol::ReactiveStateKind,
    managed_operation: Option<&str>,
) {
    if !context.is_object() {
        *context = serde_json::json!({});
    }
    let Some(ctx_obj) = context.as_object_mut() else {
        return;
    };

    let workflow_entry = ctx_obj
        .entry(CONTEXT_WORKFLOW_KEY.to_string())
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    if !workflow_entry.is_object() {
        *workflow_entry = serde_json::Value::Object(serde_json::Map::new());
    }
    let Some(workflow_obj) = workflow_entry.as_object_mut() else {
        return;
    };

    workflow_obj.insert(
        CONTEXT_JOB_EXECUTION_KIND_KEY.to_string(),
        serde_json::Value::String(reactive_state_kind_name(state_kind)),
    );
    match managed_operation {
        Some(operation) if !operation.trim().is_empty() => {
            workflow_obj.insert(
                CONTEXT_MANAGED_OPERATION_KEY.to_string(),
                serde_json::Value::String(operation.to_string()),
            );
        }
        _ => {
            workflow_obj.remove(CONTEXT_MANAGED_OPERATION_KEY);
        }
    }
}

fn runtime_spec_for_reactive_agent_state(
    state: &protocol::ReactiveWorkflowState,
    item: &WorkItem,
    job: &Job,
) -> CanonicalRuntimeSpec {
    let execution = state.config.execution();
    let execution_mode = match execution.and_then(|config| config.environment_tier.clone()) {
        Some(PlannedEnvironmentTier::Docker) => protocol::ExecutionMode::Docker,
        Some(PlannedEnvironmentTier::Raw) => protocol::ExecutionMode::Raw,
        Some(PlannedEnvironmentTier::Kubernetes) => job.execution_mode.clone(),
        None => execution
            .and_then(|config| config.executor.clone())
            .unwrap_or_else(|| job.execution_mode.clone()),
    };
    let timeout_secs = execution
        .and_then(|config| config.timeout)
        .or(execution.and_then(|config| config.timeout_secs))
        .or(job.timeout_secs);
    let mut environment = canonical_environment_for_execution(
        execution_mode,
        execution
            .and_then(|config| config.container_image.clone())
            .or_else(|| job.container_image.clone()),
        timeout_secs,
    );
    if let Some(environment_tier) = execution.and_then(|config| config.environment_tier.clone()) {
        environment.tier = environment_tier.clone();
    }
    environment.network_resources = execution
        .and_then(|config| config.network_resources.clone())
        .or_else(|| {
            network_request_from_legacy_label(
                execution.and_then(|config| config.network.as_deref()),
            )
        });
    environment.network = legacy_network_label_for_request(environment.network_resources.as_ref());
    environment.toolchains = execution
        .and_then(|config| config.toolchains.clone())
        .unwrap_or_default();

    let mut permissions = canonical_permissions_for_environment(&environment);
    if let Some(request) = &environment.network_resources {
        permissions.can_access_network = !matches!(request, NetworkResourceRequest::None);
    }

    let mut runtime = CanonicalRuntimeSpec {
        runtime_version: "v2".to_string(),
        environment,
        permissions,
        approval_rules: None,
        repos: Vec::new(),
    };
    runtime.approval_rules = execution
        .and_then(|config| config.approval_rules.clone())
        .or_else(|| {
            Some(default_approval_rules_for_tier(
                &runtime.environment.tier,
                runtime.environment.network.as_deref(),
            ))
        });
    if let Some(repo) = &job.repo {
        runtime.repos.push(repo.into());
    }
    let mut seen_repo_urls = runtime
        .repos
        .iter()
        .map(|repo| repo.repo_url.trim().to_string())
        .filter(|repo_url| !repo_url.is_empty())
        .collect::<HashSet<_>>();
    for candidate in merged_reactive_candidate_repos(execution, &item.context) {
        let repo_url = candidate.repo_url.trim().to_string();
        if repo_url.is_empty() || !seen_repo_urls.insert(repo_url.clone()) {
            continue;
        }
        runtime.repos.push(protocol::PlannedRepoClaim {
            repo_url,
            base_ref: candidate.base_ref,
            read_only: true,
            target_branch: candidate.branch_name.clone(),
            claim_type: "read_only".to_string(),
            branch_name: candidate.branch_name,
        });
    }
    runtime
}

fn task_human_gate_state_name(state: &TaskHumanGateState) -> String {
    serde_json::to_value(state)
        .ok()
        .and_then(|v| v.as_str().map(ToOwned::to_owned))
        .unwrap_or_else(|| "unknown".to_string())
}

fn triage_valid_next_events(item: &WorkItem) -> Vec<WorkflowEventKind> {
    let mut out = Vec::new();
    if work_item_requires_manual_triage(item) {
        push_unique_event(&mut out, WorkflowEventKind::Cancel);
        return out;
    }

    // choose_workflow is a dedicated path; triage does not expose workflow_chosen via fire_event.
    if item
        .human_gate_state
        .as_deref()
        .is_some_and(|s| s == "workflow_selected")
    {
        push_unique_event(&mut out, WorkflowEventKind::Submit);
    }
    push_unique_event(&mut out, WorkflowEventKind::Cancel);
    out
}

fn push_unique_event(events: &mut Vec<WorkflowEventKind>, event: WorkflowEventKind) {
    if !events.contains(&event) {
        events.push(event);
    }
}

fn reactive_terminal_work_item_state(outcome: Option<&ReactiveTerminalOutcome>) -> WorkItemState {
    match outcome.unwrap_or(&ReactiveTerminalOutcome::Succeeded) {
        ReactiveTerminalOutcome::Succeeded => WorkItemState::Succeeded,
        ReactiveTerminalOutcome::Failed => WorkItemState::Failed,
    }
}

fn workflow_event_kind_name(event: &WorkflowEventKind) -> String {
    serde_json::to_value(event)
        .ok()
        .and_then(|v| v.as_str().map(ToOwned::to_owned))
        .unwrap_or_else(|| format!("{event:?}"))
}

fn reactive_state_kind_name(kind: protocol::ReactiveStateKind) -> String {
    serde_json::to_value(kind)
        .ok()
        .and_then(|v| v.as_str().map(ToOwned::to_owned))
        .unwrap_or_else(|| "unknown".to_string())
}

fn block_reason_for_state_name(state_name: &str) -> Option<BlockReason> {
    let lower = state_name.to_ascii_lowercase();
    if lower.contains("human") || lower.contains("review") {
        Some(BlockReason::HumanReview)
    } else if lower.contains("blocked") {
        Some(BlockReason::Dependency)
    } else if lower.contains("triage") {
        Some(BlockReason::Clarification)
    } else {
        None
    }
}

fn reactive_next_event_from_agent_output(
    job: &Job,
    valid_next_events: &[WorkflowEventKind],
) -> Result<Option<WorkflowEventKind>, String> {
    let Some(raw_output_json) = job
        .result
        .as_ref()
        .and_then(|result| result.output_json.as_ref())
    else {
        return Ok(None);
    };

    let parsed = match serde_json::from_value::<AgentOutput>(raw_output_json.clone()) {
        Ok(parsed) => parsed,
        Err(_) => return Ok(None),
    };
    let Some(next_event) = parsed.next_event.as_deref() else {
        return Ok(None);
    };

    let valid_event_names = valid_next_events
        .iter()
        .map(workflow_event_kind_name)
        .collect::<Vec<_>>();
    validate_agent_output_next_event(&parsed, &valid_event_names)?;

    next_event
        .parse::<WorkflowEventKind>()
        .map(Some)
        .map_err(|_| {
            format!(
                "next_event '{}' could not be parsed as a workflow event kind",
                next_event
            )
        })
}

fn mark_human_gate_state(
    context: serde_json::Value,
    state: TaskHumanGateState,
) -> serde_json::Value {
    let mut ctx = if context.is_object() {
        context
    } else {
        serde_json::json!({})
    };
    let obj = ctx.as_object_mut().expect("context normalized to object");
    obj.insert(
        "task_human_gate_state".to_string(),
        serde_json::to_value(state).unwrap_or(serde_json::Value::Null),
    );
    ctx
}

fn project_workflow_state(item: &WorkItem) -> String {
    if item.workflow_id == "__triage__" {
        return "triage".to_string();
    }
    match item.state {
        WorkItemState::Pending => "pending".to_string(),
        WorkItemState::Running => {
            if item.human_gate {
                "human_gate".to_string()
            } else {
                "running".to_string()
            }
        }
        WorkItemState::Succeeded => "completed".to_string(),
        WorkItemState::Failed => "failed".to_string(),
        WorkItemState::Cancelled => "cancelled".to_string(),
    }
}

fn apply_task_fields(mut item: WorkItem, task: &TaskToolRecord) -> WorkItem {
    item.external_task_id = Some(task.id.to_string());
    item.external_task_source = Some("file".to_string());
    item.title = Some(task.title.clone());
    item.description = task.description.clone();
    item.labels = task
        .context
        .get("labels")
        .and_then(serde_json::Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(serde_json::Value::as_str)
                .map(ToOwned::to_owned)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    item.priority = task
        .context
        .get("priority")
        .and_then(serde_json::Value::as_str)
        .map(ToOwned::to_owned);
    item.human_gate = task.pending_human_action;
    item.human_gate_state = task
        .human_gate_state
        .as_ref()
        .map(task_human_gate_state_name);
    item.triage_status = Some(
        match task.triage_status {
            TaskTriageStatus::NeedsWorkflowSelection => "needs_workflow_selection",
            TaskTriageStatus::Ready => "ready",
            TaskTriageStatus::InProgress => "in_progress",
            TaskTriageStatus::Blocked => "blocked",
            TaskTriageStatus::Completed => "completed",
            TaskTriageStatus::Cancelled => "cancelled",
        }
        .to_string(),
    );
    item.projected_state = Some(project_workflow_state(&item));
    item
}

#[derive(Debug, Deserialize)]
struct WorkflowRoutingRules {
    #[serde(default)]
    default_workflow_id: Option<String>,
    #[serde(default)]
    by_priority: HashMap<String, String>,
    #[serde(default)]
    by_label: HashMap<String, String>,
}

enum WorkflowSelectionDecision {
    Selected(String),
    NeedsWorkflowSelection {
        reason: String,
        manual_triage_required: bool,
    },
}

fn pick_workflow_for_task(
    task: &TaskToolRecord,
    workflow_ids: &[String],
) -> WorkflowSelectionDecision {
    if task_requires_manual_triage(task) {
        return WorkflowSelectionDecision::NeedsWorkflowSelection {
            reason: "manual triage required".to_string(),
            manual_triage_required: true,
        };
    }

    if let Some(explicit) = choose_workflow_id(task, workflow_ids) {
        return WorkflowSelectionDecision::Selected(explicit);
    }

    if let Some(rules) = task
        .context
        .get("workflow_rules")
        .cloned()
        .and_then(|v| serde_json::from_value::<WorkflowRoutingRules>(v).ok())
    {
        let mut matches: HashMap<String, Vec<String>> = HashMap::new();
        let mut unknown_refs = Vec::new();

        if let Some(priority) = task
            .context
            .get("priority")
            .and_then(serde_json::Value::as_str)
            && let Some(workflow_id) = rules.by_priority.get(priority)
        {
            if workflow_ids.iter().any(|id| id == workflow_id) {
                matches
                    .entry(workflow_id.clone())
                    .or_default()
                    .push(format!("priority:{priority}"));
            } else {
                unknown_refs.push(format!("priority:{priority}->{workflow_id}"));
            }
        }

        if let Some(labels) = task
            .context
            .get("labels")
            .and_then(serde_json::Value::as_array)
        {
            for label in labels.iter().filter_map(serde_json::Value::as_str) {
                if let Some(workflow_id) = rules.by_label.get(label) {
                    if workflow_ids.iter().any(|id| id == workflow_id) {
                        matches
                            .entry(workflow_id.clone())
                            .or_default()
                            .push(format!("label:{label}"));
                    } else {
                        unknown_refs.push(format!("label:{label}->{workflow_id}"));
                    }
                }
            }
        }

        if matches.len() == 1
            && let Some(workflow_id) = matches.keys().next().cloned()
        {
            return WorkflowSelectionDecision::Selected(workflow_id);
        }
        if matches.len() > 1 {
            let mut candidates = matches.keys().cloned().collect::<Vec<_>>();
            candidates.sort();
            return WorkflowSelectionDecision::NeedsWorkflowSelection {
                reason: format!(
                    "multiple workflow rules matched; choose one of: {}",
                    candidates.join(", ")
                ),
                manual_triage_required: false,
            };
        }

        if let Some(default_workflow_id) = rules.default_workflow_id {
            if workflow_ids.iter().any(|id| id == &default_workflow_id) {
                return WorkflowSelectionDecision::Selected(default_workflow_id);
            }
            unknown_refs.push(format!("default->{default_workflow_id}"));
        }

        if !unknown_refs.is_empty() {
            unknown_refs.sort();
            return WorkflowSelectionDecision::NeedsWorkflowSelection {
                reason: format!(
                    "workflow_rules referenced unknown workflow ids: {}",
                    unknown_refs.join(", ")
                ),
                manual_triage_required: false,
            };
        }
    }

    if let Some(hint) = task
        .context
        .get("workflow_hint")
        .and_then(serde_json::Value::as_str)
        && workflow_ids.iter().any(|id| id == hint)
    {
        return WorkflowSelectionDecision::Selected(hint.to_string());
    }

    WorkflowSelectionDecision::NeedsWorkflowSelection {
        reason: "no deterministic workflow rule matched".to_string(),
        manual_triage_required: false,
    }
}

fn row_to_work_item(row: &rusqlite::Row<'_>) -> rusqlite::Result<WorkItem> {
    let id: String = row.get(0)?;
    let labels_json: Option<String> = row.get(6)?;
    let session_id: Option<String> = row.get(8)?;
    let blocked_on_json: Option<String> = row.get(14)?;
    let state: String = row.get(15)?;
    let context_json: String = row.get(16)?;
    let repo_json: Option<String> = row.get(17)?;
    let parent_work_item_id: Option<String> = row.get(18)?;
    let parent_step_id: Option<String> = row.get(19)?;
    let child_work_item_id: Option<String> = row.get(20)?;
    let created_at: String = row.get(21)?;
    let updated_at: String = row.get(22)?;

    Ok(WorkItem {
        id: Uuid::parse_str(&id).map_err(invalid_uuid)?,
        workflow_id: row.get(1)?,
        external_task_id: row.get(2)?,
        external_task_source: row.get(3)?,
        title: row.get(4)?,
        description: row.get(5)?,
        labels: labels_json
            .as_deref()
            .map(serde_json::from_str)
            .transpose()
            .map_err(invalid_json)?
            .unwrap_or_default(),
        priority: row.get(7)?,
        session_id: session_id.map(protocol::SessionId::new),
        current_step: row.get(9)?,
        projected_state: row.get(10)?,
        triage_status: row.get(11)?,
        human_gate: row.get::<_, Option<i64>>(12)?.unwrap_or(0) != 0,
        human_gate_state: row.get(13)?,
        blocked_on: blocked_on_json
            .as_deref()
            .map(serde_json::from_str)
            .transpose()
            .map_err(invalid_json)?,
        state: state.parse().map_err(invalid_enum)?,
        context: serde_json::from_str(&context_json).map_err(invalid_json)?,
        repo: repo_json
            .as_deref()
            .map(serde_json::from_str)
            .transpose()
            .map_err(invalid_json)?,
        parent_work_item_id: parent_work_item_id
            .as_deref()
            .map(Uuid::parse_str)
            .transpose()
            .map_err(invalid_uuid)?,
        parent_step_id,
        child_work_item_id: child_work_item_id
            .as_deref()
            .map(Uuid::parse_str)
            .transpose()
            .map_err(invalid_uuid)?,
        created_at: parse_datetime(&created_at)?,
        updated_at: parse_datetime(&updated_at)?,
    })
}

fn row_to_job(row: &rusqlite::Row<'_>) -> rusqlite::Result<Job> {
    let id: String = row.get(0)?;
    let work_item_id: String = row.get(1)?;
    let state: String = row.get(3)?;
    let mode: String = row.get(5)?;
    let args_json: String = row.get(8)?;
    let env_json: String = row.get(9)?;
    let prompt_json: Option<String> = row.get(10)?;
    let context_json: String = row.get(11)?;
    let repo_json: Option<String> = row.get(12)?;
    let timeout_secs: Option<u64> = row.get(13)?;
    let max_retries: Option<u32> = row.get(14)?;
    let attempt: Option<u32> = row.get(15)?;
    let result_json: Option<String> = row.get(17)?;
    let lease_expires_at: Option<String> = row.get(18)?;
    let created_at: String = row.get(19)?;
    let updated_at: String = row.get(20)?;
    let runtime_spec_json: Option<String> = row.get(21)?;

    let mut job = Job {
        id: Uuid::parse_str(&id).map_err(invalid_uuid)?,
        work_item_id: Uuid::parse_str(&work_item_id).map_err(invalid_uuid)?,
        step_id: row.get(2)?,
        state: state.parse().map_err(invalid_enum)?,
        assigned_worker_id: row.get(4)?,
        execution_mode: mode.parse().map_err(invalid_enum)?,
        container_image: row.get(6)?,
        command: row.get(7)?,
        args: serde_json::from_str(&args_json).map_err(invalid_json)?,
        env: serde_json::from_str(&env_json).map_err(invalid_json)?,
        prompt: prompt_json
            .as_deref()
            .map(serde_json::from_str)
            .transpose()
            .map_err(invalid_json)?,
        context: serde_json::from_str(&context_json).map_err(invalid_json)?,
        repo: repo_json
            .as_deref()
            .map(serde_json::from_str)
            .transpose()
            .map_err(invalid_json)?,
        timeout_secs,
        max_retries: max_retries.unwrap_or(0),
        attempt: attempt.unwrap_or(0),
        transcript_dir: row.get(16)?,
        result: result_json
            .as_deref()
            .map(serde_json::from_str)
            .transpose()
            .map_err(invalid_json)?,
        lease_expires_at: lease_expires_at
            .as_deref()
            .map(parse_datetime)
            .transpose()?,
        created_at: parse_datetime(&created_at)?,
        updated_at: parse_datetime(&updated_at)?,
    };

    if let Some(spec_json) = runtime_spec_json {
        let runtime: CanonicalRuntimeSpec =
            serde_json::from_str(&spec_json).map_err(invalid_json)?;
        // Compatibility boundary: Job still exposes legacy runtime fields only.
        // We intentionally project canonical environment back into those fields,
        // while canonical permissions remain persisted in runtime_spec_json.
        job.execution_mode = runtime.environment.execution_mode;
        job.container_image = runtime.environment.container_image;
        job.timeout_secs = runtime.environment.timeout_secs;
    }

    Ok(job)
}

fn parse_datetime(v: &str) -> rusqlite::Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(v)
        .map(|d| d.with_timezone(&Utc))
        .map_err(invalid_datetime)
}

fn invalid_uuid(e: uuid::Error) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
}

fn invalid_datetime(e: chrono::ParseError) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
}

fn invalid_json(e: serde_json::Error) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
}

fn invalid_enum(e: impl std::error::Error + Send + Sync + 'static) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
}

fn column_exists(conn: &Connection, table: DbTable, column: &str) -> anyhow::Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({})", table.as_str()))?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
    for row in rows {
        if row? == column {
            return Ok(true);
        }
    }
    Ok(false)
}

pub async fn create_work_item<N: super::notifier::Notifier>(
    State(state): State<super::AppState<N>>,
    Json(req): Json<CreateWorkItemRequest>,
) -> Result<Json<WorkItem>, AppError> {
    Ok(Json(state.orchestration.create_work_item(req).await?))
}

pub async fn list_work_items<N: super::notifier::Notifier>(
    State(state): State<super::AppState<N>>,
) -> Result<Json<Vec<WorkItem>>, AppError> {
    Ok(Json(state.orchestration.list_work_items().await?))
}

pub async fn fire_work_item_event<N: super::notifier::Notifier>(
    State(state): State<super::AppState<N>>,
    Path(id): Path<Uuid>,
    Json(req): Json<FireEventRequest>,
) -> Result<Json<WorkItem>, AppError> {
    Ok(Json(state.orchestration.fire_event(id, req).await?))
}

pub async fn list_jobs<N: super::notifier::Notifier>(
    State(state): State<super::AppState<N>>,
) -> Result<Json<Vec<Job>>, AppError> {
    Ok(Json(state.orchestration.list_jobs().await?))
}

pub async fn update_job<N: super::notifier::Notifier>(
    State(state): State<super::AppState<N>>,
    Path(id): Path<Uuid>,
    Json(req): Json<JobUpdateRequest>,
) -> Result<Json<Job>, AppError> {
    let job = state.orchestration.update_job(id, req).await?;
    if matches!(
        job.state,
        JobState::Succeeded | JobState::Failed | JobState::Cancelled
    ) {
        state.job_events.mark_terminal(job.id).await;
        state.job_events.push(terminal_job_event(&job)).await;
    }
    Ok(Json(job))
}

pub async fn job_events_sse<N: super::notifier::Notifier>(
    State(state): State<super::AppState<N>>,
    Path(id): Path<Uuid>,
) -> Result<Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>>, AppError> {
    let job = state
        .orchestration
        .get_job(id)
        .await
        .map_err(map_db)?
        .ok_or_else(|| AppError::JobNotFound(id.to_string()))?;
    let (replay, rx) = state.job_events.subscribe(id).await;
    let replay_stream = tokio_stream::iter(
        replay
            .into_iter()
            .map(|ev| Ok::<Event, Infallible>(to_sse_event(&ev))),
    );
    let done_items = if is_terminal_job_state(&job.state) {
        vec![Ok::<Event, Infallible>(done_sse_event(&job))]
    } else {
        Vec::new()
    };
    let done_stream = tokio_stream::iter(done_items);

    let live_stream = BroadcastStream::new(rx).filter_map(|msg| match msg {
        Ok(ev) => Some(Ok::<Event, Infallible>(to_sse_event(&ev))),
        Err(tokio_stream::wrappers::errors::BroadcastStreamRecvError::Lagged(n)) => {
            Some(Ok::<Event, Infallible>(
                Event::default().event("events_lagged").data(n.to_string()),
            ))
        }
    });
    Ok(
        Sse::new(replay_stream.chain(done_stream).chain(live_stream))
            .keep_alive(KeepAlive::default()),
    )
}

fn to_sse_event(ev: &protocol::JobEvent) -> Event {
    let data = serde_json::to_string(ev).unwrap_or_else(|_| "{}".to_string());
    let event_name = if ev.stream.as_deref() == Some("terminal") {
        "done"
    } else {
        match ev.kind {
            protocol::JobEventKind::System => "system",
            protocol::JobEventKind::Stdout => "stdout",
            protocol::JobEventKind::Stderr => "stderr",
        }
    };
    Event::default()
        .event(event_name)
        .id(ev.at.timestamp_millis().to_string())
        .data(data)
}

fn done_sse_event(job: &Job) -> Event {
    Event::default().event("done").data(
        serde_json::json!({
            "job_id": job.id,
            "state": job.state,
        })
        .to_string(),
    )
}

fn is_terminal_job_state(state: &JobState) -> bool {
    matches!(
        state,
        JobState::Succeeded | JobState::Failed | JobState::Cancelled
    )
}

pub fn terminal_job_event(job: &Job) -> protocol::JobEvent {
    protocol::JobEvent {
        job_id: job.id,
        worker_id: job.assigned_worker_id.clone().unwrap_or_default(),
        at: Utc::now(),
        kind: protocol::JobEventKind::System,
        stream: Some("terminal".to_string()),
        message: serde_json::json!({
            "state": job.state,
        })
        .to_string(),
    }
}

pub async fn register_worker<N: super::notifier::Notifier>(
    State(state): State<super::AppState<N>>,
    Json(req): Json<RegisterWorkerRequest>,
) -> Result<Json<WorkerInfo>, AppError> {
    Ok(Json(state.orchestration.register_worker(req).await?))
}

pub async fn heartbeat_worker<N: super::notifier::Notifier>(
    State(state): State<super::AppState<N>>,
    Path(id): Path<String>,
    Json(req): Json<HeartbeatRequest>,
) -> Result<Json<WorkerInfo>, AppError> {
    Ok(Json(state.orchestration.heartbeat(id, req).await?))
}

pub async fn list_workers<N: super::notifier::Notifier>(
    State(state): State<super::AppState<N>>,
) -> Result<Json<Vec<WorkerInfo>>, AppError> {
    Ok(Json(state.orchestration.list_workers().await?))
}

pub async fn poll_job<N: super::notifier::Notifier>(
    State(state): State<super::AppState<N>>,
    Json(req): Json<PollJobRequest>,
) -> Result<Json<PollJobResponse>, AppError> {
    Ok(Json(state.orchestration.poll_job(req).await?))
}

pub async fn dispatch<N: super::notifier::Notifier>(
    State(state): State<super::AppState<N>>,
) -> Result<Json<DispatchResponse>, AppError> {
    let created = state.orchestration.dispatch_actionable().await?;
    Ok(Json(DispatchResponse {
        created_jobs: created,
    }))
}

#[derive(Debug, serde::Serialize)]
pub struct DispatchResponse {
    pub created_jobs: usize,
}

pub async fn list_workflows<N: super::notifier::Notifier>(
    State(state): State<super::AppState<N>>,
) -> Json<Vec<WorkflowDefinition>> {
    Json(state.orchestration.list_workflows())
}

pub async fn list_workflow_documents<N: super::notifier::Notifier>(
    State(state): State<super::AppState<N>>,
) -> Json<Vec<WorkflowDocument>> {
    Json(state.orchestration.list_workflow_documents())
}

pub async fn validate_workflows<N: super::notifier::Notifier>(
    State(state): State<super::AppState<N>>,
) -> Json<protocol::WorkflowValidationReport> {
    Json(state.orchestration.workflow_validation_report())
}

#[derive(Debug, Deserialize)]
pub struct GithubWebhookPayload {
    pub action: Option<String>,
    #[serde(default)]
    pub pull_request: Option<GithubPullRequestWebhook>,
    #[serde(default)]
    pub review: Option<GithubReviewWebhook>,
    #[serde(default)]
    pub check_suite: Option<GithubCheckSuiteWebhook>,
    #[serde(default)]
    pub check_run: Option<GithubCheckRunWebhook>,
}

#[derive(Debug, Deserialize)]
pub struct GithubPullRequestWebhook {
    pub html_url: String,
    #[serde(default)]
    pub number: Option<u64>,
    #[serde(default)]
    pub merged: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct GithubReviewWebhook {
    #[serde(default)]
    pub state: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct GithubCheckSuiteWebhook {
    #[serde(default)]
    pub conclusion: Option<String>,
    #[serde(default)]
    pub pull_requests: Vec<GithubPullRequestRefWebhook>,
    #[serde(default)]
    pub head_branch: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct GithubCheckRunWebhook {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub conclusion: Option<String>,
    #[serde(default)]
    pub pull_requests: Vec<GithubPullRequestRefWebhook>,
    #[serde(default)]
    pub head_branch: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct GithubPullRequestRefWebhook {
    pub url: String,
}

#[derive(Debug, Serialize)]
pub struct GithubWebhookIngestResponse {
    pub matched_jobs: usize,
    pub fired_events: usize,
    pub mapped_event: Option<WorkflowEventKind>,
}

fn verify_github_signature(secret: &str, body: &[u8], signature_header: &str) -> bool {
    let Some(signature_hex) = signature_header.strip_prefix("sha256=") else {
        return false;
    };

    let Ok(signature) = hex::decode(signature_hex) else {
        return false;
    };

    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(secret.as_bytes()) else {
        return false;
    };
    mac.update(body);

    mac.verify_slice(&signature).is_ok()
}

fn parse_pr_number_from_url(url: &str) -> Option<u64> {
    url.rsplit('/').next()?.parse::<u64>().ok()
}

fn github_pr_state_from_context(context: &serde_json::Value) -> Option<GithubPrState> {
    context
        .get(CONTEXT_GITHUB_PR_STATE_KEY)
        .cloned()
        .and_then(|value| serde_json::from_value::<GithubPrState>(value).ok())
}

fn github_pr_state_default(url: String, number: u64, now: DateTime<Utc>) -> GithubPrState {
    GithubPrState {
        url,
        number,
        head_branch: None,
        state: GithubPrLifecycleState::Open,
        approval_count: 0,
        approved: false,
        passing_checks: Vec::new(),
        checks_passing: false,
        changes_requested: false,
        merge_policy: None,
        merge_attempted: false,
        mergeable: false,
        last_updated: now,
    }
}

fn merge_readiness_from_pr_state(pr_state: &GithubPrState) -> MergeReadiness {
    let is_open = matches!(pr_state.state, GithubPrLifecycleState::Open);
    let (approved, checks_passing) = if let Some(policy) = pr_state.merge_policy.as_ref() {
        let approved = pr_state.approval_count >= policy.required_approvals;
        let checks_passing = policy.required_checks.iter().all(|required| {
            pr_state
                .passing_checks
                .iter()
                .any(|passing| passing == required)
        });
        (approved, checks_passing)
    } else {
        (pr_state.approved, pr_state.checks_passing)
    };
    let no_changes_requested = !pr_state.changes_requested;
    let ready = is_open && approved && checks_passing && no_changes_requested;

    MergeReadiness {
        ready,
        is_open,
        approved,
        checks_passing,
        no_changes_requested,
    }
}

fn update_github_pr_state_in_context(
    context: &mut serde_json::Value,
    pr_url: &str,
    pr_number: Option<u64>,
    head_branch: Option<&str>,
    event: Option<&WorkflowEventKind>,
    check_name: Option<&str>,
    pr_closed_unmerged: bool,
    now: DateTime<Utc>,
) {
    let default_number = pr_number
        .or_else(|| parse_pr_number_from_url(pr_url))
        .unwrap_or(0);
    let mut pr_state = github_pr_state_from_context(context)
        .unwrap_or_else(|| github_pr_state_default(pr_url.to_string(), default_number, now));

    pr_state.url = pr_url.to_string();
    if let Some(number) = pr_number.or_else(|| parse_pr_number_from_url(pr_url)) {
        pr_state.number = number;
    }
    if let Some(branch) = head_branch
        .map(str::trim)
        .filter(|branch| !branch.is_empty())
    {
        pr_state.head_branch = Some(branch.to_string());
    }

    match event {
        Some(WorkflowEventKind::PrCreated) => {
            pr_state.state = GithubPrLifecycleState::Open;
            pr_state.approval_count = 0;
            pr_state.approved = false;
            pr_state.passing_checks.clear();
            pr_state.checks_passing = false;
            pr_state.changes_requested = false;
            pr_state.merge_attempted = false;
        }
        Some(WorkflowEventKind::PrApproved) => {
            pr_state.approval_count = pr_state.approval_count.saturating_add(1);
            pr_state.approved = true;
            pr_state.changes_requested = false;
        }
        Some(WorkflowEventKind::HumanChangesRequested) => {
            pr_state.changes_requested = true;
            pr_state.approval_count = 0;
            pr_state.approved = false;
        }
        Some(WorkflowEventKind::ChecksPassed) => {
            pr_state.checks_passing = true;
            if let Some(name) = check_name {
                let name = name.trim();
                if !name.is_empty()
                    && !pr_state
                        .passing_checks
                        .iter()
                        .any(|existing| existing == name)
                {
                    pr_state.passing_checks.push(name.to_string());
                }
            }
        }
        Some(WorkflowEventKind::ChecksFailed) => {
            pr_state.checks_passing = false;
            if let Some(name) = check_name {
                pr_state
                    .passing_checks
                    .retain(|existing| existing != name.trim());
            }
        }
        Some(WorkflowEventKind::PrMerged) => {
            pr_state.state = GithubPrLifecycleState::Merged;
            pr_state.merge_attempted = true;
        }
        _ => {}
    }
    if pr_closed_unmerged {
        pr_state.state = GithubPrLifecycleState::Closed;
    }

    let readiness = merge_readiness_from_pr_state(&pr_state);
    pr_state.mergeable = readiness.ready;
    pr_state.last_updated = now;

    if !context.is_object() {
        *context = serde_json::Value::Object(serde_json::Map::new());
    }
    if let Some(ctx_obj) = context.as_object_mut()
        && let Ok(value) = serde_json::to_value(pr_state)
    {
        ctx_obj.insert(CONTEXT_GITHUB_PR_STATE_KEY.to_string(), value);
    }
}

pub async fn github_webhook<N: super::notifier::Notifier>(
    State(state): State<super::AppState<N>>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<axum::response::Response, AppError> {
    if let Some(secret) = state.config.github_webhook_secret.as_deref() {
        let Some(signature_header) = headers
            .get("x-hub-signature-256")
            .and_then(|value| value.to_str().ok())
        else {
            return Ok(StatusCode::UNAUTHORIZED.into_response());
        };

        if !verify_github_signature(secret, &body, signature_header) {
            return Ok(StatusCode::UNAUTHORIZED.into_response());
        }
    }

    let payload: GithubWebhookPayload = match serde_json::from_slice(&body) {
        Ok(payload) => payload,
        Err(_) => return Ok(StatusCode::UNPROCESSABLE_ENTITY.into_response()),
    };

    let event_name = headers
        .get("x-github-event")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    let action = payload.action.as_deref().unwrap_or_default();
    let mapped_event = map_github_webhook_to_workflow_event(&headers, &payload);
    let webhook_pr = github_webhook_pr_reference(event_name, &payload);
    let pr_closed_unmerged = event_name == "pull_request"
        && action == "closed"
        && !payload
            .pull_request
            .as_ref()
            .and_then(|pr| pr.merged)
            .unwrap_or(false);
    let check_name = payload
        .check_run
        .as_ref()
        .and_then(|check| check.name.as_deref());
    if webhook_pr.pr_url.is_none()
        && webhook_pr.pr_number.is_none()
        && webhook_pr.head_branch.is_none()
    {
        return Ok(Json(GithubWebhookIngestResponse {
            matched_jobs: 0,
            fired_events: 0,
            mapped_event: None,
        })
        .into_response());
    }
    if mapped_event.is_none() && !pr_closed_unmerged {
        return Ok(Json(GithubWebhookIngestResponse {
            matched_jobs: 0,
            fired_events: 0,
            mapped_event: None,
        })
        .into_response());
    }

    let items = state.orchestration.list_work_items().await?;
    let matched = items
        .into_iter()
        .filter(|item| {
            let webhook_matches_item = item_matches_github_webhook(item, &webhook_pr);
            !matches!(
                item.state,
                WorkItemState::Succeeded | WorkItemState::Failed | WorkItemState::Cancelled
            ) && webhook_matches_item
        })
        .collect::<Vec<_>>();

    let mut fired = 0usize;
    for item in &matched {
        let readiness_before = state
            .orchestration
            .check_merge_readiness(item)
            .map(|readiness| readiness.ready)
            .unwrap_or(false);
        let mut updated_item = item.clone();
        let pr_url = webhook_pr
            .pr_url
            .clone()
            .or_else(|| item_matching_pr_url(item, webhook_pr.pr_number))
            .unwrap_or_default();
        let pr_number = webhook_pr
            .pr_number
            .or_else(|| parse_pr_number_from_url(&pr_url));
        update_github_pr_state_in_context(
            &mut updated_item.context,
            &pr_url,
            pr_number,
            webhook_pr.head_branch.as_deref(),
            mapped_event.as_ref(),
            check_name,
            pr_closed_unmerged,
            Utc::now(),
        );
        updated_item.updated_at = Utc::now();
        state
            .orchestration
            .update_work_item(&updated_item)
            .await
            .map_err(map_db)?;
        state
            .orchestration
            .persist_task_projection(&updated_item, None)
            .await
            .map_err(map_config)?;

        let readiness_after = state
            .orchestration
            .check_merge_readiness(&updated_item)
            .map(|readiness| readiness.ready)
            .unwrap_or(false);
        if state
            .orchestration
            .reactive_state_auto_merge_enabled(&updated_item)
            && !readiness_before
            && readiness_after
        {
            if let Err(error) = state.orchestration.try_auto_merge(&mut updated_item).await {
                eprintln!(
                    "[info] deterministic auto-merge attempt errored: work_item={} error={error:#}",
                    updated_item.id
                );
            }
        }

        let mut candidates = Vec::new();
        if let Some(event) = mapped_event.as_ref() {
            candidates.push(event.clone());
            match event {
                WorkflowEventKind::PrMerged => candidates.push(WorkflowEventKind::JobSucceeded),
                WorkflowEventKind::PrApproved => candidates.push(WorkflowEventKind::HumanApproved),
                WorkflowEventKind::ChecksPassed => candidates.push(WorkflowEventKind::JobSucceeded),
                WorkflowEventKind::ChecksFailed => candidates.push(WorkflowEventKind::JobFailed),
                _ => {}
            }
        }

        for candidate in candidates {
            if state
                .orchestration
                .fire_event_trusted_internal_with_source(
                    item.id,
                    FireEventRequest {
                        event: candidate,
                        job_id: None,
                        reason: Some("mapped from GitHub webhook".to_string()),
                    },
                    EventSource::Webhook,
                )
                .await
                .is_ok()
            {
                fired = fired.saturating_add(1);
                break;
            }
        }
    }

    Ok(Json(GithubWebhookIngestResponse {
        matched_jobs: matched.len(),
        fired_events: fired,
        mapped_event,
    })
    .into_response())
}

fn map_github_webhook_to_workflow_event(
    headers: &HeaderMap,
    payload: &GithubWebhookPayload,
) -> Option<WorkflowEventKind> {
    let event_name = headers
        .get("x-github-event")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    let action = payload.action.as_deref().unwrap_or_default();

    if event_name == "pull_request" {
        if matches!(action, "opened" | "reopened") {
            return Some(WorkflowEventKind::PrCreated);
        }
        if action == "closed"
            && payload
                .pull_request
                .as_ref()
                .and_then(|pr| pr.merged)
                .unwrap_or(false)
        {
            return Some(WorkflowEventKind::PrMerged);
        }
    }

    if event_name == "pull_request_review" && action == "submitted" {
        match payload.review.as_ref().and_then(|r| r.state.as_deref()) {
            Some("approved") => return Some(WorkflowEventKind::PrApproved),
            Some("changes_requested") => return Some(WorkflowEventKind::HumanChangesRequested),
            _ => {}
        }
    }

    if event_name == "check_suite" && action == "completed" {
        match payload
            .check_suite
            .as_ref()
            .and_then(|c| c.conclusion.as_deref())
        {
            Some("success") => return Some(WorkflowEventKind::ChecksPassed),
            Some("failure" | "timed_out" | "cancelled") => {
                return Some(WorkflowEventKind::ChecksFailed);
            }
            _ => {}
        }
    }

    if event_name == "check_run" && action == "completed" {
        match payload
            .check_run
            .as_ref()
            .and_then(|c| c.conclusion.as_deref())
        {
            Some("success") => return Some(WorkflowEventKind::ChecksPassed),
            Some("failure" | "timed_out" | "cancelled") => {
                return Some(WorkflowEventKind::ChecksFailed);
            }
            _ => {}
        }
    }
    None
}

#[derive(Debug, Clone, Default)]
struct GithubWebhookPrReference {
    pr_url: Option<String>,
    pr_number: Option<u64>,
    head_branch: Option<String>,
}

fn github_webhook_pr_reference(
    event_name: &str,
    payload: &GithubWebhookPayload,
) -> GithubWebhookPrReference {
    let top_level_pr_url = payload.pull_request.as_ref().map(|pr| pr.html_url.clone());
    let top_level_pr_number = payload.pull_request.as_ref().and_then(|pr| pr.number);

    let check_suite_pr_url = payload
        .check_suite
        .as_ref()
        .and_then(|suite| suite.pull_requests.first())
        .map(|pr| pr.url.clone());
    let check_run_pr_url = payload
        .check_run
        .as_ref()
        .and_then(|check| check.pull_requests.first())
        .map(|pr| pr.url.clone());

    let pr_url = top_level_pr_url
        .or_else(|| check_suite_pr_url.clone())
        .or_else(|| check_run_pr_url.clone());
    let pr_number = top_level_pr_number
        .or_else(|| pr_url.as_deref().and_then(parse_pr_number_from_url))
        .or_else(|| {
            check_suite_pr_url
                .as_deref()
                .and_then(parse_pr_number_from_url)
        })
        .or_else(|| {
            check_run_pr_url
                .as_deref()
                .and_then(parse_pr_number_from_url)
        });
    let head_branch = if event_name == "check_suite" {
        payload
            .check_suite
            .as_ref()
            .and_then(|suite| suite.head_branch.clone())
    } else if event_name == "check_run" {
        payload
            .check_run
            .as_ref()
            .and_then(|check| check.head_branch.clone())
    } else {
        None
    };

    GithubWebhookPrReference {
        pr_url,
        pr_number,
        head_branch,
    }
}

fn item_github_pr_urls(item: &WorkItem) -> impl Iterator<Item = &str> {
    let pr_state_url = item
        .context
        .get(CONTEXT_GITHUB_PR_STATE_KEY)
        .and_then(serde_json::Value::as_object)
        .and_then(|state| state.get("url"))
        .and_then(serde_json::Value::as_str);

    std::iter::once(pr_state_url).flatten().chain(
        item.context
            .get(CONTEXT_JOB_RESULTS_KEY)
            .and_then(serde_json::Value::as_object)
            .into_iter()
            .flat_map(|results| results.values())
            .filter_map(|result| {
                result
                    .get("github_pr")
                    .and_then(|pr| pr.get("url"))
                    .and_then(serde_json::Value::as_str)
            }),
    )
}

fn item_github_head_branches(item: &WorkItem) -> impl Iterator<Item = &str> {
    let pr_state_head_branch = item
        .context
        .get(CONTEXT_GITHUB_PR_STATE_KEY)
        .and_then(serde_json::Value::as_object)
        .and_then(|state| state.get("head_branch"))
        .and_then(serde_json::Value::as_str);

    std::iter::once(pr_state_head_branch).flatten().chain(
        item.context
            .get(CONTEXT_JOB_RESULTS_KEY)
            .and_then(serde_json::Value::as_object)
            .into_iter()
            .flat_map(|results| results.values())
            .flat_map(|result| {
                let pr_head_branch = result
                    .get("github_pr")
                    .and_then(|pr| pr.get("head_ref_name"))
                    .and_then(serde_json::Value::as_str);
                let repo_target_branch = result
                    .get("repo")
                    .and_then(|repo| repo.get("target_branch"))
                    .and_then(serde_json::Value::as_str);
                let repo_branch = result
                    .get("repo")
                    .and_then(|repo| repo.get("branch"))
                    .and_then(serde_json::Value::as_str);

                [pr_head_branch, repo_target_branch, repo_branch]
                    .into_iter()
                    .flatten()
            }),
    )
}

fn item_matching_pr_url(item: &WorkItem, pr_number: Option<u64>) -> Option<String> {
    item_github_pr_urls(item)
        .find(|url| {
            pr_number
                .map(|number| parse_pr_number_from_url(url) == Some(number))
                .unwrap_or(true)
        })
        .map(ToOwned::to_owned)
}

fn item_matches_github_webhook(item: &WorkItem, webhook_pr: &GithubWebhookPrReference) -> bool {
    let pr_url_match = webhook_pr.pr_url.as_deref().is_some_and(|incoming_url| {
        item_github_pr_urls(item).any(|url| {
            url == incoming_url
                || webhook_pr
                    .pr_number
                    .is_some_and(|number| parse_pr_number_from_url(url) == Some(number))
        })
    });

    let pr_number_match = webhook_pr.pr_number.is_some_and(|number| {
        item_github_pr_urls(item).any(|url| parse_pr_number_from_url(url) == Some(number))
    });

    let branch_match = webhook_pr.head_branch.as_deref().is_some_and(|branch| {
        let repo_match = item
            .repo
            .as_ref()
            .map(|repo| {
                repo.branch_name.as_deref() == Some(branch) || repo.base_ref.as_str() == branch
            })
            .unwrap_or(false);
        repo_match || item_github_head_branches(item).any(|head_branch| head_branch == branch)
    });

    pr_url_match || pr_number_match || branch_match
}

fn workflow_document_id(doc: &WorkflowDocument) -> &str {
    match doc {
        WorkflowDocument::LegacyV1 { workflow } => workflow.id.as_str(),
        WorkflowDocument::ReactiveV2 { workflow } => workflow.id.as_str(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::task_adapter::TaskTriageStatus;
    use axum::http::HeaderValue;
    use axum::http::StatusCode;
    use axum::{Router, routing::get};
    use protocol::JobResult;
    use tower::ServiceExt;

    fn workflow_catalog() -> Arc<WorkflowCatalog> {
        let tmp = std::env::temp_dir().join(format!("wf-catalog-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).expect("mkdir");
        let wf = r#"
[workflow]
id = "wf"
entry_step = "build"

[[workflow.steps]]
id = "build"
kind = "agent"
executor = "raw"
command = "echo"
args = ["hello"]
on_success = "test"

[[workflow.steps]]
id = "test"
kind = "agent"
executor = "docker"
container_image = "alpine:3.20"
command = "echo"
args = ["ok"]
"#;
        std::fs::write(tmp.join("wf.toml"), wf).expect("write wf");
        Arc::new(WorkflowCatalog::load_from_dir(tmp.to_str().unwrap()).expect("load"))
    }

    fn reactive_workflow_catalog() -> Arc<WorkflowCatalog> {
        let tmp = std::env::temp_dir().join(format!("wf-catalog-reactive-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).expect("mkdir");
        let wf = r#"
[workflow]
id = "reactive_wf"
schema = "reactive_v2"
initial_state = "bootstrap"

[[workflow.states]]
id = "bootstrap"
kind = "auto"

[[workflow.states.on]]
event = "start"
target = "run_agent"

[[workflow.states]]
id = "run_agent"
kind = "agent"
executor = "raw"
prompt = { user = "run agent" }

[[workflow.states.on]]
event = "job_succeeded"
target = "review"

[[workflow.states.on]]
event = "job_failed"
target = "review"

[[workflow.states]]
id = "review"
kind = "human"

[[workflow.states.on]]
event = "human_approved"
target = "done"

[[workflow.states.on]]
event = "human_changes_requested"
target = "run_agent"

[[workflow.states]]
id = "done"
kind = "terminal"
"#;
        std::fs::write(tmp.join("reactive.toml"), wf).expect("write reactive wf");
        Arc::new(WorkflowCatalog::load_from_dir(tmp.to_str().unwrap()).expect("load"))
    }

    fn reactive_candidate_repo_catalog() -> Arc<WorkflowCatalog> {
        let tmp =
            std::env::temp_dir().join(format!("wf-catalog-reactive-candidates-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).expect("mkdir");
        let wf = r#"
[workflow]
id = "reactive_candidates"
schema = "reactive_v2"
initial_state = "bootstrap"

[[workflow.states]]
id = "bootstrap"
kind = "auto"

[[workflow.states.on]]
event = "start"
target = "run_agent"

[[workflow.states]]
id = "run_agent"
kind = "agent"
executor = "raw"
prompt = { user = "triage" }
candidate_repos = [
  { repo_url = "https://github.com/reddit/agent-hub", base_ref = "origin/main" },
  { repo_url = "https://github.com/reddit/opencode", base_ref = "origin/main" }
]

[[workflow.states.on]]
event = "job_succeeded"
target = "done"

[[workflow.states.on]]
event = "job_failed"
target = "done"

[[workflow.states]]
id = "done"
kind = "terminal"
"#;
        std::fs::write(tmp.join("reactive_candidates.toml"), wf).expect("write reactive wf");
        Arc::new(WorkflowCatalog::load_from_dir(tmp.to_str().unwrap()).expect("load"))
    }

    fn simple_code_change_catalog() -> Arc<WorkflowCatalog> {
        let tmp = std::env::temp_dir().join(format!("wf-catalog-simple-code-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).expect("mkdir");
        let wf = r#"
[workflow]
id = "simple-code-change"
schema = "reactive_v2"
initial_state = "triage_investigation"

[[workflow.states]]
id = "triage_investigation"
kind = "agent"
executor = "raw"
prompt = { user = "triage {{repo_catalog_hint}}" }

[[workflow.states.on]]
event = "job_succeeded"
target = "await_triage_approval"

[[workflow.states.on]]
event = "job_failed"
target = "triage_failed"

[[workflow.states]]
id = "triage_failed"
kind = "human"

[[workflow.states.on]]
event = "human_changes_requested"
target = "triage_investigation"

[[workflow.states]]
id = "await_triage_approval"
kind = "human"

[[workflow.states.on]]
event = "human_approved"
target = "done"

[[workflow.states.on]]
event = "human_changes_requested"
target = "triage_investigation"

[[workflow.states]]
id = "done"
kind = "terminal"
"#;
        std::fs::write(tmp.join("simple_code_change.toml"), wf).expect("write simple wf");
        Arc::new(WorkflowCatalog::load_from_dir(tmp.to_str().unwrap()).expect("load"))
    }

    fn reactive_no_start_catalog() -> Arc<WorkflowCatalog> {
        let tmp = std::env::temp_dir().join(format!("wf-catalog-reactive-ns-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).expect("mkdir");
        let wf = r#"
[workflow]
id = "reactive_no_start"
schema = "reactive_v2"
initial_state = "bootstrap"

[[workflow.states]]
id = "bootstrap"
kind = "auto"

[[workflow.states.on]]
event = "noop"
target = "bootstrap"
"#;
        std::fs::write(tmp.join("reactive.toml"), wf).expect("write reactive wf");
        Arc::new(WorkflowCatalog::load_from_dir(tmp.to_str().unwrap()).expect("load"))
    }

    fn reactive_submit_catalog() -> Arc<WorkflowCatalog> {
        let tmp =
            std::env::temp_dir().join(format!("wf-catalog-reactive-submit-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).expect("mkdir");
        let wf = r#"
[workflow]
id = "reactive_submit"
schema = "reactive_v2"
initial_state = "draft"

[[workflow.states]]
id = "draft"
kind = "human"

[[workflow.states.on]]
event = "submit"
target = "done"

[[workflow.states]]
id = "done"
kind = "terminal"
terminal_outcome = "failed"
"#;
        std::fs::write(tmp.join("reactive.toml"), wf).expect("write reactive wf");
        Arc::new(WorkflowCatalog::load_from_dir(tmp.to_str().unwrap()).expect("load"))
    }

    fn reactive_managed_catalog() -> Arc<WorkflowCatalog> {
        let tmp =
            std::env::temp_dir().join(format!("wf-catalog-reactive-managed-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).expect("mkdir");
        let wf = r#"
[workflow]
id = "reactive_managed"
schema = "reactive_v2"
initial_state = "bootstrap"

[[workflow.states]]
id = "bootstrap"
kind = "auto"

[[workflow.states.on]]
event = "start"
target = "run_managed"

[[workflow.states]]
id = "run_managed"
kind = "managed"
operation = "open_pr"

[[workflow.states.on]]
event = "job_succeeded"
target = "done"

[[workflow.states.on]]
event = "job_failed"
target = "done"

[[workflow.states]]
id = "done"
kind = "terminal"
"#;
        std::fs::write(tmp.join("reactive.toml"), wf).expect("write reactive wf");
        Arc::new(WorkflowCatalog::load_from_dir(tmp.to_str().unwrap()).expect("load"))
    }

    fn reactive_child_only_catalog() -> Arc<WorkflowCatalog> {
        let tmp =
            std::env::temp_dir().join(format!("wf-catalog-child-reactive-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).expect("mkdir");
        let parent = r#"
[workflow]
id = "wf"
entry_step = "build"

[[workflow.steps]]
id = "build"
kind = "agent"
executor = "raw"
command = "echo"
args = ["parent"]
on_success = "after"

[[workflow.steps]]
id = "after"
kind = "agent"
executor = "raw"
command = "echo"
args = ["after"]
"#;
        let child = r#"
[workflow]
id = "child_reactive"
schema = "reactive_v2"
initial_state = "bootstrap"

[[workflow.states]]
id = "bootstrap"
kind = "auto"

[[workflow.states.on]]
event = "start"
target = "run"

[[workflow.states]]
id = "run"
kind = "command"
executor = "raw"
command = "echo"
args = ["child"]

[[workflow.states.on]]
event = "job_succeeded"
target = "done"

[[workflow.states]]
id = "done"
kind = "terminal"
"#;
        std::fs::write(tmp.join("parent.toml"), parent).expect("write parent");
        std::fs::write(tmp.join("child.toml"), child).expect("write child");
        Arc::new(WorkflowCatalog::load_from_dir(tmp.to_str().unwrap()).expect("load"))
    }

    fn reactive_parent_child_catalog() -> Arc<WorkflowCatalog> {
        let tmp = std::env::temp_dir().join(format!(
            "wf-catalog-reactive-parent-child-{}",
            Uuid::new_v4()
        ));
        std::fs::create_dir_all(&tmp).expect("mkdir");
        let parent = r#"
[workflow]
id = "reactive_parent"
schema = "reactive_v2"
initial_state = "spawn"

[[workflow.states]]
id = "spawn"
kind = "auto"
action = "spawn_children"
child_workflow = "child_legacy"

[[workflow.states.on]]
event = "all_children_completed"
target = "done"

[[workflow.states.on]]
event = "child_failed"
target = "blocked"

[[workflow.states]]
id = "done"
kind = "terminal"
terminal_outcome = "succeeded"

[[workflow.states]]
id = "blocked"
kind = "terminal"
terminal_outcome = "failed"
"#;
        let child = r#"
[workflow]
id = "child_legacy"
entry_step = "run"

[[workflow.steps]]
id = "run"
kind = "agent"
executor = "raw"
command = "echo"
args = ["child"]
"#;
        std::fs::write(tmp.join("parent.toml"), parent).expect("write parent");
        std::fs::write(tmp.join("child.toml"), child).expect("write child");
        Arc::new(WorkflowCatalog::load_from_dir(tmp.to_str().unwrap()).expect("load"))
    }

    fn multi_workflow_catalog() -> Arc<WorkflowCatalog> {
        let tmp = std::env::temp_dir().join(format!("wf-catalog-multi-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).expect("mkdir");
        let wf_a = r#"
[workflow]
id = "wf_alpha"
entry_step = "run"

[[workflow.steps]]
id = "run"
kind = "agent"
executor = "raw"
command = "echo"
args = ["alpha"]
"#;
        let wf_b = r#"
[workflow]
id = "wf_beta"
entry_step = "run"

[[workflow.steps]]
id = "run"
kind = "agent"
executor = "raw"
command = "echo"
args = ["beta"]
"#;
        std::fs::write(tmp.join("wf_alpha.toml"), wf_a).expect("write alpha");
        std::fs::write(tmp.join("wf_beta.toml"), wf_b).expect("write beta");
        Arc::new(WorkflowCatalog::load_from_dir(tmp.to_str().unwrap()).expect("load"))
    }

    #[tokio::test]
    async fn successful_job_advances_workflow() {
        let db = std::env::temp_dir().join(format!("orch-{}.sqlite3", Uuid::new_v4()));
        let tasks = std::env::temp_dir().join(format!("tasks-{}", Uuid::new_v4()));
        let svc = OrchestrationService::new(
            workflow_catalog(),
            Arc::new(TaskFileStore::new(tasks.clone())) as Arc<dyn TaskAdapter>,
            db.to_string_lossy().into_owned(),
        );
        svc.init().await.expect("init");

        let item = svc
            .create_work_item(CreateWorkItemRequest {
                workflow_id: "wf".into(),
                session_id: None,
                context: serde_json::json!({}),
                repo: None,
            })
            .await
            .expect("create item");

        svc.register_worker(RegisterWorkerRequest {
            id: "w1".into(),
            hostname: "host".into(),
            supported_modes: vec![
                protocol::ExecutionMode::Raw,
                protocol::ExecutionMode::Docker,
            ],
        })
        .await
        .expect("reg worker");

        let job = svc
            .poll_job(PollJobRequest {
                worker_id: "w1".into(),
            })
            .await
            .expect("poll")
            .job
            .expect("job assigned");

        svc.update_job(
            job.id,
            JobUpdateRequest {
                worker_id: "w1".into(),
                state: JobState::Running,
                transcript_dir: None,
                result: None,
            },
        )
        .await
        .expect("running");

        svc.update_job(
            job.id,
            JobUpdateRequest {
                worker_id: "w1".into(),
                state: JobState::Succeeded,
                transcript_dir: Some("/tmp/x".into()),
                result: Some(JobResult {
                    outcome: protocol::ExecutionOutcome::Succeeded,
                    termination_reason: protocol::TerminationReason::ExitCode,
                    exit_code: Some(0),
                    timing: protocol::ExecutionTiming {
                        started_at: Utc::now(),
                        finished_at: Utc::now(),
                        duration_ms: 1,
                    },
                    artifacts: protocol::ArtifactSummary {
                        stdout_path: "/tmp/x/stdout.log".into(),
                        stderr_path: "/tmp/x/stderr.log".into(),
                        output_path: None,
                        transcript_dir: Some("/tmp/x".into()),
                        stdout_bytes: None,
                        stderr_bytes: None,
                        output_bytes: None,
                        structured_transcript_artifacts: vec![
                            protocol::StructuredTranscriptArtifact {
                                key: "agent_context".into(),
                                path: "/tmp/x/.agent-context.json".into(),
                                bytes: Some(120),
                                schema: Some(protocol::AGENT_CONTEXT_SCHEMA_NAME.into()),
                                record_count: None,
                                inline_json: Some(serde_json::json!({
                                    "schema": protocol::AGENT_CONTEXT_SCHEMA_NAME,
                                    "job": {
                                        "step_id": "build"
                                    }
                                })),
                            },
                            protocol::StructuredTranscriptArtifact {
                                key: "run_json".into(),
                                path: "/tmp/x/run.json".into(),
                                bytes: Some(99),
                                schema: Some(protocol::JOB_RUN_SCHEMA_NAME.into()),
                                record_count: None,
                                inline_json: Some(serde_json::json!({
                                    "schema": protocol::JOB_RUN_SCHEMA_NAME,
                                    "mode": "docker",
                                    "workspace": "/workspace"
                                })),
                            },
                            protocol::StructuredTranscriptArtifact {
                                key: "transcript_jsonl".into(),
                                path: "/tmp/x/transcript.jsonl".into(),
                                bytes: Some(456),
                                schema: None,
                                record_count: Some(3),
                                inline_json: Some(serde_json::json!([
                                    "{\"event\":\"start\"}",
                                    "{\"event\":\"done\"}"
                                ])),
                            },
                            protocol::StructuredTranscriptArtifact {
                                key: "job_events".into(),
                                path: "/tmp/x/job-events.jsonl".into(),
                                bytes: Some(789),
                                schema: Some(protocol::JOB_EVENT_SCHEMA_NAME.into()),
                                record_count: Some(12),
                                inline_json: Some(serde_json::json!([
                                    {
                                        "job_id": uuid::Uuid::new_v4(),
                                        "worker_id": "w1",
                                        "at": Utc::now(),
                                        "kind": "system",
                                        "message": "phase 1"
                                    },
                                    {
                                        "job_id": uuid::Uuid::new_v4(),
                                        "worker_id": "w1",
                                        "at": Utc::now(),
                                        "kind": "stdout",
                                        "stream": "stdout",
                                        "message": "phase 2"
                                    }
                                ])),
                            },
                        ],
                    },
                    output_json: Some(serde_json::json!({"build_ok": true})),
                    repo: None,
                    github_pr: None,
                    error_message: None,
                }),
            },
        )
        .await
        .expect("success");

        let updated = svc
            .list_work_items()
            .await
            .expect("items")
            .into_iter()
            .find(|w| w.id == item.id)
            .expect("item exists");
        assert_eq!(updated.current_step, "test");
        assert_eq!(updated.state, WorkItemState::Running);

        let jobs = svc.list_jobs().await.expect("jobs");
        assert!(jobs.iter().any(|j| j.step_id == "test"));
        assert_eq!(
            jobs.iter()
                .find(|j| j.step_id == "build")
                .unwrap()
                .timeout_secs,
            None
        );

        let updated = svc
            .get_work_item(item.id)
            .await
            .expect("get item")
            .expect("exists");
        assert_eq!(
            updated.context["job_outputs"]["build"],
            serde_json::json!({"build_ok": true})
        );
        assert_eq!(
            updated.context["job_results"]["build"]["outcome"],
            serde_json::json!("succeeded")
        );
        assert_eq!(
            updated.context["job_results"]["build"]["termination_reason"],
            serde_json::json!("exit_code")
        );
        assert_eq!(
            updated.context["job_artifact_paths"]["build"]["agent_context"],
            serde_json::json!("/tmp/x/.agent-context.json")
        );
        assert_eq!(
            updated.context["job_artifact_paths"]["build"]["stdout_log"],
            serde_json::json!("/tmp/x/stdout.log")
        );
        assert_eq!(
            updated.context["job_artifact_paths"]["build"]["stderr_log"],
            serde_json::json!("/tmp/x/stderr.log")
        );
        assert_eq!(
            updated.context["job_artifact_paths"]["build"]["transcript_dir"],
            serde_json::json!("/tmp/x")
        );
        assert!(
            updated.context["job_artifact_paths"]["build"]
                .get("output_json")
                .is_none()
        );
        assert_eq!(
            updated.context["job_artifact_paths"]["build"]["transcript_jsonl"],
            serde_json::json!("/tmp/x/transcript.jsonl")
        );
        assert_eq!(
            updated.context["job_artifact_paths"]["build"]["job_events"],
            serde_json::json!("/tmp/x/job-events.jsonl")
        );
        assert_eq!(
            updated.context["job_artifact_metadata"]["build"]["agent_context"]["path"],
            serde_json::json!("/tmp/x/.agent-context.json")
        );
        assert_eq!(
            updated.context["job_artifact_metadata"]["build"]["stdout_log"]["path"],
            serde_json::json!("/tmp/x/stdout.log")
        );
        assert_eq!(
            updated.context["job_artifact_metadata"]["build"]["stderr_log"]["path"],
            serde_json::json!("/tmp/x/stderr.log")
        );
        assert_eq!(
            updated.context["job_artifact_metadata"]["build"]["transcript_dir"]["path"],
            serde_json::json!("/tmp/x")
        );
        assert!(
            updated.context["job_artifact_metadata"]["build"]
                .get("output_json")
                .is_none()
        );
        assert_eq!(
            updated.context["job_artifact_metadata"]["build"]["agent_context"]["schema"],
            serde_json::json!(protocol::AGENT_CONTEXT_SCHEMA_NAME)
        );
        assert_eq!(
            updated.context["job_artifact_metadata"]["build"]["run_json"]["schema"],
            serde_json::json!(protocol::JOB_RUN_SCHEMA_NAME)
        );
        assert_eq!(
            updated.context["job_artifact_metadata"]["build"]["transcript_jsonl"]["record_count"],
            serde_json::json!(3)
        );
        assert_eq!(
            updated.context["job_artifact_metadata"]["build"]["job_events"]["schema"],
            serde_json::json!(protocol::JOB_EVENT_SCHEMA_NAME)
        );
        assert_eq!(
            updated.context["job_artifact_metadata"]["build"]["job_events"]["record_count"],
            serde_json::json!(12)
        );
        assert_eq!(
            updated.context["job_artifact_data"]["build"]["agent_context"]["schema"],
            serde_json::json!(protocol::AGENT_CONTEXT_SCHEMA_NAME)
        );
        assert_eq!(
            updated.context["job_artifact_data"]["build"]["run_json"]["schema"],
            serde_json::json!(protocol::JOB_RUN_SCHEMA_NAME)
        );
        assert_eq!(
            updated.context["job_artifact_data"]["build"]["transcript_jsonl"]
                .as_array()
                .map(Vec::len),
            Some(2)
        );
        assert_eq!(
            updated.context["job_artifact_data"]["build"]["transcript_jsonl"][0],
            serde_json::json!("{\"event\":\"start\"}")
        );
        assert_eq!(
            updated.context["job_artifact_data"]["build"]["job_events"]
                .as_array()
                .map(Vec::len),
            Some(2)
        );
        assert_eq!(
            updated.context["job_artifact_data"]["build"]["job_events"][0]["kind"],
            serde_json::json!("system")
        );
        assert!(
            updated.context["job_artifact_data"]["build"]
                .get("stdout_log")
                .is_none()
        );
        assert!(
            updated.context["job_artifact_data"]["build"]
                .get("stderr_log")
                .is_none()
        );
        assert!(
            updated.context["job_artifact_data"]["build"]
                .get("output_json")
                .is_none()
        );
        assert!(
            updated.context["job_artifact_data"]["build"]
                .get("transcript_dir")
                .is_none()
        );

        let next_job = jobs
            .iter()
            .find(|j| j.step_id == "test")
            .expect("next step job");
        assert_eq!(
            next_job.context["workflow"]["inputs"]["valid_next_events"],
            serde_json::json!(["cancel"])
        );
        assert_eq!(
            next_job.context["workflow"]["inputs"]["job_artifact_paths"]["build"]["agent_context"],
            serde_json::json!("/tmp/x/.agent-context.json")
        );
        assert_eq!(
            next_job.context["workflow"]["inputs"]["job_artifact_metadata"]["build"]["agent_context"]
                ["schema"],
            serde_json::json!(protocol::AGENT_CONTEXT_SCHEMA_NAME)
        );
        assert_eq!(
            next_job.context["work_item"]["id"],
            serde_json::json!(item.id)
        );
        assert_eq!(
            next_job.context["work_item"]["workflow_id"],
            serde_json::json!(item.workflow_id)
        );
        assert_eq!(
            next_job.context["work_item"]["workflow_state"],
            serde_json::json!("test")
        );
        assert!(
            next_job.context["workflow"]["inputs"]
                .get("job_artifact_data")
                .is_none()
        );

        let _ = std::fs::remove_file(db);
        let _ = std::fs::remove_dir_all(tasks);
    }

    #[tokio::test]
    async fn initial_job_context_includes_workflow_inputs_contract() {
        let db = std::env::temp_dir().join(format!("orch-{}.sqlite3", Uuid::new_v4()));
        let tasks = std::env::temp_dir().join(format!("tasks-{}", Uuid::new_v4()));
        let svc = OrchestrationService::new(
            workflow_catalog(),
            Arc::new(TaskFileStore::new(tasks.clone())) as Arc<dyn TaskAdapter>,
            db.to_string_lossy().into_owned(),
        );
        svc.init().await.expect("init");

        let item = svc
            .create_work_item(CreateWorkItemRequest {
                workflow_id: "wf".into(),
                session_id: None,
                context: serde_json::json!({}),
                repo: None,
            })
            .await
            .expect("create item");

        let initial_job = svc
            .list_jobs_for_work_item(item.id)
            .await
            .expect("jobs")
            .into_iter()
            .find(|j| j.step_id == "build")
            .expect("build job");

        assert_eq!(
            initial_job.context["workflow"]["inputs"]["valid_next_events"],
            serde_json::json!(["submit", "cancel"])
        );
        assert_eq!(
            initial_job.context["workflow"]["inputs"]["job_artifact_paths"],
            serde_json::json!({})
        );
        assert_eq!(
            initial_job.context["workflow"]["inputs"]["job_artifact_metadata"],
            serde_json::json!({})
        );
        assert_eq!(
            initial_job.context["work_item"]["id"],
            serde_json::json!(item.id)
        );
        assert_eq!(
            initial_job.context["work_item"]["workflow_id"],
            serde_json::json!(item.workflow_id)
        );
        assert_eq!(
            initial_job.context["work_item"]["workflow_state"],
            serde_json::json!("build")
        );
        assert!(
            initial_job.context["workflow"]["inputs"]
                .get("job_artifact_data")
                .is_none()
        );

        let _ = std::fs::remove_file(db);
        let _ = std::fs::remove_dir_all(tasks);
    }

    #[tokio::test]
    async fn sse_events_endpoint_replays_buffered_events() {
        let db = std::env::temp_dir().join(format!("orch-{}.sqlite3", Uuid::new_v4()));
        let tasks = std::env::temp_dir().join(format!("tasks-{}", Uuid::new_v4()));
        let svc = OrchestrationService::new(
            workflow_catalog(),
            Arc::new(TaskFileStore::new(tasks.clone())) as Arc<dyn TaskAdapter>,
            db.to_string_lossy().into_owned(),
        );
        svc.init().await.expect("init");

        let job_events = crate::server::job_events::JobEventHub::new();

        let item = svc
            .create_work_item(CreateWorkItemRequest {
                workflow_id: "wf".into(),
                session_id: None,
                context: serde_json::json!({}),
                repo: None,
            })
            .await
            .expect("create");
        let job = svc
            .list_jobs()
            .await
            .expect("jobs")
            .into_iter()
            .find(|j| j.work_item_id == item.id)
            .expect("job");

        job_events
            .push(protocol::JobEvent {
                job_id: job.id,
                worker_id: "w1".into(),
                at: Utc::now(),
                kind: protocol::JobEventKind::System,
                stream: None,
                message: "hello".into(),
            })
            .await;

        let state = super::super::AppState {
            config: Arc::new(super::super::config::ServerConfig {
                auth_mode: super::super::config::AuthMode::None,
                tokens: vec![],
                listen_addr: "127.0.0.1:0".into(),
                presence_ttl_secs: 120,
                session_ttl_secs: 120,
                notification_delay_secs: 0,
                approval_mode: super::super::config::ApprovalFeatureMode::Disabled,
                base_url: None,
                default_approval_mode: super::super::sessions::SessionApprovalMode::Remote,
                workflows_dir: "workflows".into(),
                task_dir: "tasks".into(),
                task_adapter: "file".into(),
                vikunja_base_url: None,
                vikunja_token: None,
                vikunja_project_id: None,
                vikunja_label_prefix: None,
                orchestration_db_path: ":memory:".into(),
                grpc_listen_addr: None,
                github_webhook_secret: None,
            }),
            presence: Arc::new(super::super::presence::Presence::new(120)),
            sessions: Arc::new(super::super::sessions::SessionRegistry::new(120)),
            notifier: Arc::new(super::super::notifier::NullNotifier),
            notify_config: Arc::new(tokio::sync::RwLock::new(
                protocol::NotifyConfig::with_delay(0),
            )),
            pending: Arc::new(super::super::hooks::PendingNotifications::new()),
            approvals: Arc::new(super::super::approvals::ApprovalRegistry::new()),
            questions: Arc::new(super::super::questions::QuestionRegistry::new()),
            orchestration: Arc::new(svc.clone()),
            job_events: Arc::new(job_events),
            oauth: Arc::new(None),
        };

        let app = Router::new()
            .route(
                "/api/v1/orchestration/jobs/{id}/events",
                get(job_events_sse::<super::super::notifier::NullNotifier>),
            )
            .with_state(state);

        let req = axum::http::Request::builder()
            .method("GET")
            .uri(format!("/api/v1/orchestration/jobs/{}/events", job.id))
            .body(axum::body::Body::empty())
            .expect("request");
        let resp = app.oneshot(req).await.expect("response");
        assert_eq!(resp.status(), StatusCode::OK);

        let _ = std::fs::remove_file(db);
        let _ = std::fs::remove_dir_all(tasks);
    }

    #[tokio::test]
    async fn empty_structured_transcript_artifacts_still_project_legacy_artifact_summary_aliases() {
        let db = std::env::temp_dir().join(format!("orch-{}.sqlite3", Uuid::new_v4()));
        let tasks = std::env::temp_dir().join(format!("tasks-{}", Uuid::new_v4()));
        let svc = OrchestrationService::new(
            workflow_catalog(),
            Arc::new(TaskFileStore::new(tasks.clone())) as Arc<dyn TaskAdapter>,
            db.to_string_lossy().into_owned(),
        );
        svc.init().await.expect("init");

        let item = svc
            .create_work_item(CreateWorkItemRequest {
                workflow_id: "wf".into(),
                session_id: None,
                context: serde_json::json!({}),
                repo: None,
            })
            .await
            .expect("create item");

        svc.register_worker(RegisterWorkerRequest {
            id: "w1".into(),
            hostname: "host".into(),
            supported_modes: vec![protocol::ExecutionMode::Raw],
        })
        .await
        .expect("reg worker");

        let job = svc
            .poll_job(PollJobRequest {
                worker_id: "w1".into(),
            })
            .await
            .expect("poll")
            .job
            .expect("job assigned");

        svc.update_job(
            job.id,
            JobUpdateRequest {
                worker_id: "w1".into(),
                state: JobState::Running,
                transcript_dir: None,
                result: None,
            },
        )
        .await
        .expect("running");

        svc.update_job(
            job.id,
            JobUpdateRequest {
                worker_id: "w1".into(),
                state: JobState::Succeeded,
                transcript_dir: Some("/tmp/x".into()),
                result: Some(JobResult {
                    outcome: protocol::ExecutionOutcome::Succeeded,
                    termination_reason: protocol::TerminationReason::ExitCode,
                    exit_code: Some(0),
                    timing: protocol::ExecutionTiming {
                        started_at: Utc::now(),
                        finished_at: Utc::now(),
                        duration_ms: 1,
                    },
                    artifacts: protocol::ArtifactSummary {
                        stdout_path: "/tmp/x/stdout.log".into(),
                        stderr_path: "/tmp/x/stderr.log".into(),
                        output_path: None,
                        transcript_dir: Some("/tmp/x".into()),
                        stdout_bytes: None,
                        stderr_bytes: None,
                        output_bytes: None,
                        structured_transcript_artifacts: Vec::new(),
                    },
                    output_json: Some(serde_json::json!({"build_ok": true})),
                    repo: None,
                    github_pr: None,
                    error_message: None,
                }),
            },
        )
        .await
        .expect("success");

        let updated = svc
            .get_work_item(item.id)
            .await
            .expect("get item")
            .expect("exists");
        let step_alias = updated
            .context
            .get("job_artifact_paths")
            .and_then(serde_json::Value::as_object)
            .and_then(|v| v.get("build"));
        assert_eq!(
            step_alias
                .and_then(serde_json::Value::as_object)
                .and_then(|v| v.get("stdout_log")),
            Some(&serde_json::json!("/tmp/x/stdout.log"))
        );
        assert_eq!(
            updated.context["job_artifact_paths"]["build"]["stderr_log"],
            serde_json::json!("/tmp/x/stderr.log")
        );
        assert_eq!(
            updated.context["job_artifact_paths"]["build"]["transcript_dir"],
            serde_json::json!("/tmp/x")
        );
        assert!(
            updated.context["job_artifact_paths"]["build"]
                .get("output_json")
                .is_none()
        );
        assert!(
            updated.context["job_artifact_paths"]["build"]
                .get("agent_context")
                .is_none()
        );
        let metadata_alias = updated
            .context
            .get("job_artifact_metadata")
            .and_then(serde_json::Value::as_object)
            .and_then(|v| v.get("build"));
        assert_eq!(
            metadata_alias
                .and_then(serde_json::Value::as_object)
                .and_then(|v| v.get("stdout_log"))
                .and_then(serde_json::Value::as_object)
                .and_then(|v| v.get("path")),
            Some(&serde_json::json!("/tmp/x/stdout.log"))
        );
        assert_eq!(
            updated.context["job_artifact_metadata"]["build"]["transcript_dir"]["path"],
            serde_json::json!("/tmp/x")
        );
        assert!(
            updated.context["job_artifact_metadata"]["build"]
                .get("output_json")
                .is_none()
        );

        let _ = std::fs::remove_file(db);
        let _ = std::fs::remove_dir_all(tasks);
    }

    #[tokio::test]
    async fn update_job_terminal_marks_event_buffer_terminal() {
        let db = std::env::temp_dir().join(format!("orch-{}.sqlite3", Uuid::new_v4()));
        let tasks = std::env::temp_dir().join(format!("tasks-{}", Uuid::new_v4()));
        let svc = OrchestrationService::new(
            workflow_catalog(),
            Arc::new(TaskFileStore::new(tasks.clone())) as Arc<dyn TaskAdapter>,
            db.to_string_lossy().into_owned(),
        );
        svc.init().await.expect("init");

        let job_events = crate::server::job_events::JobEventHub::new();

        let item = svc
            .create_work_item(CreateWorkItemRequest {
                workflow_id: "wf".into(),
                session_id: None,
                context: serde_json::json!({}),
                repo: None,
            })
            .await
            .expect("create");

        svc.register_worker(RegisterWorkerRequest {
            id: "w1".into(),
            hostname: "host".into(),
            supported_modes: vec![protocol::ExecutionMode::Raw],
        })
        .await
        .expect("reg");

        let job = svc
            .poll_job(PollJobRequest {
                worker_id: "w1".into(),
            })
            .await
            .expect("poll")
            .job
            .expect("job");

        let state = super::super::AppState {
            config: Arc::new(super::super::config::ServerConfig {
                auth_mode: super::super::config::AuthMode::None,
                tokens: vec![],
                listen_addr: "127.0.0.1:0".into(),
                presence_ttl_secs: 120,
                session_ttl_secs: 120,
                notification_delay_secs: 0,
                approval_mode: super::super::config::ApprovalFeatureMode::Disabled,
                base_url: None,
                default_approval_mode: super::super::sessions::SessionApprovalMode::Remote,
                workflows_dir: "workflows".into(),
                task_dir: "tasks".into(),
                task_adapter: "file".into(),
                vikunja_base_url: None,
                vikunja_token: None,
                vikunja_project_id: None,
                vikunja_label_prefix: None,
                orchestration_db_path: ":memory:".into(),
                grpc_listen_addr: None,
                github_webhook_secret: None,
            }),
            presence: Arc::new(super::super::presence::Presence::new(120)),
            sessions: Arc::new(super::super::sessions::SessionRegistry::new(120)),
            notifier: Arc::new(super::super::notifier::NullNotifier),
            notify_config: Arc::new(tokio::sync::RwLock::new(
                protocol::NotifyConfig::with_delay(0),
            )),
            pending: Arc::new(super::super::hooks::PendingNotifications::new()),
            approvals: Arc::new(super::super::approvals::ApprovalRegistry::new()),
            questions: Arc::new(super::super::questions::QuestionRegistry::new()),
            orchestration: Arc::new(svc.clone()),
            job_events: Arc::new(job_events.clone()),
            oauth: Arc::new(None),
        };

        let req = JobUpdateRequest {
            worker_id: "w1".into(),
            state: JobState::Running,
            transcript_dir: Some("/tmp/t".into()),
            result: None,
        };

        let _ = update_job::<super::super::notifier::NullNotifier>(
            State(state.clone()),
            Path(job.id),
            Json(req),
        )
        .await
        .expect("update");

        let req = JobUpdateRequest {
            worker_id: "w1".into(),
            state: JobState::Succeeded,
            transcript_dir: Some("/tmp/t".into()),
            result: None,
        };

        let _ = update_job::<super::super::notifier::NullNotifier>(
            State(state),
            Path(job.id),
            Json(req),
        )
        .await
        .expect("update");

        // Marked terminal entries should eventually be cleanable.
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        let _ = job_events.cleanup_terminal_expired().await;

        let _ = std::fs::remove_file(db);
        let _ = std::fs::remove_dir_all(tasks);
        let _ = item;
    }

    #[test]
    fn terminal_event_maps_to_done_sse_event_name() {
        let job = Job {
            id: Uuid::new_v4(),
            work_item_id: Uuid::new_v4(),
            step_id: "build".into(),
            state: JobState::Succeeded,
            assigned_worker_id: Some("w1".into()),
            execution_mode: protocol::ExecutionMode::Raw,
            container_image: None,
            command: "echo".into(),
            args: vec![],
            env: HashMap::new(),
            prompt: None,
            context: serde_json::json!({}),
            repo: None,
            timeout_secs: None,
            max_retries: 0,
            attempt: 0,
            transcript_dir: None,
            result: None,
            lease_expires_at: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        let terminal = terminal_job_event(&job);
        let evt = to_sse_event(&terminal);
        let rendered = format!("{:?}", evt);
        assert!(rendered.contains("event: done"));
    }

    #[test]
    fn maps_github_webhook_to_workflow_event() {
        let mut headers = HeaderMap::new();
        headers.insert("x-github-event", HeaderValue::from_static("pull_request"));
        let payload = GithubWebhookPayload {
            action: Some("closed".into()),
            pull_request: Some(GithubPullRequestWebhook {
                html_url: "https://github.com/reddit/agent-hub/pull/1".into(),
                number: None,
                merged: Some(true),
            }),
            review: None,
            check_suite: None,
            check_run: None,
        };
        assert_eq!(
            map_github_webhook_to_workflow_event(&headers, &payload),
            Some(WorkflowEventKind::PrMerged)
        );

        let opened = GithubWebhookPayload {
            action: Some("opened".into()),
            pull_request: Some(GithubPullRequestWebhook {
                html_url: "https://github.com/reddit/agent-hub/pull/2".into(),
                number: None,
                merged: None,
            }),
            review: None,
            check_suite: None,
            check_run: None,
        };
        assert_eq!(
            map_github_webhook_to_workflow_event(&headers, &opened),
            Some(WorkflowEventKind::PrCreated)
        );
    }

    #[test]
    fn maps_github_review_webhook_to_human_events() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-github-event",
            HeaderValue::from_static("pull_request_review"),
        );

        let approved = GithubWebhookPayload {
            action: Some("submitted".into()),
            pull_request: Some(GithubPullRequestWebhook {
                html_url: "https://github.com/reddit/agent-hub/pull/1".into(),
                number: None,
                merged: None,
            }),
            review: Some(GithubReviewWebhook {
                state: Some("approved".into()),
            }),
            check_suite: None,
            check_run: None,
        };
        assert_eq!(
            map_github_webhook_to_workflow_event(&headers, &approved),
            Some(WorkflowEventKind::PrApproved)
        );

        let changes = GithubWebhookPayload {
            action: Some("submitted".into()),
            pull_request: Some(GithubPullRequestWebhook {
                html_url: "https://github.com/reddit/agent-hub/pull/1".into(),
                number: None,
                merged: None,
            }),
            review: Some(GithubReviewWebhook {
                state: Some("changes_requested".into()),
            }),
            check_suite: None,
            check_run: None,
        };
        assert_eq!(
            map_github_webhook_to_workflow_event(&headers, &changes),
            Some(WorkflowEventKind::HumanChangesRequested)
        );
    }

    #[test]
    fn maps_github_checks_webhooks_to_checks_events() {
        let mut headers = HeaderMap::new();
        headers.insert("x-github-event", HeaderValue::from_static("check_suite"));
        let check_suite_payload = GithubWebhookPayload {
            action: Some("completed".into()),
            pull_request: None,
            review: None,
            check_suite: Some(GithubCheckSuiteWebhook {
                conclusion: Some("success".into()),
                pull_requests: vec![],
                head_branch: None,
            }),
            check_run: None,
        };
        assert_eq!(
            map_github_webhook_to_workflow_event(&headers, &check_suite_payload),
            Some(WorkflowEventKind::ChecksPassed)
        );

        let check_suite_failed = GithubWebhookPayload {
            action: Some("completed".into()),
            pull_request: None,
            review: None,
            check_suite: Some(GithubCheckSuiteWebhook {
                conclusion: Some("failure".into()),
                pull_requests: vec![],
                head_branch: None,
            }),
            check_run: None,
        };
        assert_eq!(
            map_github_webhook_to_workflow_event(&headers, &check_suite_failed),
            Some(WorkflowEventKind::ChecksFailed)
        );

        let mut headers = HeaderMap::new();
        headers.insert("x-github-event", HeaderValue::from_static("check_run"));
        let check_run_payload = GithubWebhookPayload {
            action: Some("completed".into()),
            pull_request: None,
            review: None,
            check_suite: None,
            check_run: Some(GithubCheckRunWebhook {
                name: None,
                conclusion: Some("success".into()),
                pull_requests: vec![],
                head_branch: None,
            }),
        };
        assert_eq!(
            map_github_webhook_to_workflow_event(&headers, &check_run_payload),
            Some(WorkflowEventKind::ChecksPassed)
        );

        let check_run_cancelled = GithubWebhookPayload {
            action: Some("completed".into()),
            pull_request: None,
            review: None,
            check_suite: None,
            check_run: Some(GithubCheckRunWebhook {
                name: None,
                conclusion: Some("cancelled".into()),
                pull_requests: vec![],
                head_branch: None,
            }),
        };
        assert_eq!(
            map_github_webhook_to_workflow_event(&headers, &check_run_cancelled),
            Some(WorkflowEventKind::ChecksFailed)
        );
    }

    #[test]
    fn github_webhook_pr_reference_supports_checks_payload_shapes() {
        let suite_payload = GithubWebhookPayload {
            action: Some("completed".into()),
            pull_request: None,
            review: None,
            check_suite: Some(GithubCheckSuiteWebhook {
                conclusion: Some("success".into()),
                pull_requests: vec![GithubPullRequestRefWebhook {
                    url: "https://github.com/reddit/agent-hub/pull/123".into(),
                }],
                head_branch: Some("feature/test".into()),
            }),
            check_run: None,
        };
        let suite_ref = github_webhook_pr_reference("check_suite", &suite_payload);
        assert_eq!(
            suite_ref.pr_url.as_deref(),
            Some("https://github.com/reddit/agent-hub/pull/123")
        );
        assert_eq!(suite_ref.pr_number, Some(123));
        assert_eq!(suite_ref.head_branch.as_deref(), Some("feature/test"));

        let run_payload = GithubWebhookPayload {
            action: Some("completed".into()),
            pull_request: None,
            review: None,
            check_suite: None,
            check_run: Some(GithubCheckRunWebhook {
                name: Some("ci".into()),
                conclusion: Some("failure".into()),
                pull_requests: vec![GithubPullRequestRefWebhook {
                    url: "https://github.com/reddit/agent-hub/pull/124".into(),
                }],
                head_branch: Some("feature/run".into()),
            }),
        };
        let run_ref = github_webhook_pr_reference("check_run", &run_payload);
        assert_eq!(
            run_ref.pr_url.as_deref(),
            Some("https://github.com/reddit/agent-hub/pull/124")
        );
        assert_eq!(run_ref.pr_number, Some(124));
        assert_eq!(run_ref.head_branch.as_deref(), Some("feature/run"));
    }

    #[test]
    fn updates_pr_state_context_and_merge_readiness_deterministically() {
        let mut context = serde_json::json!({});

        update_github_pr_state_in_context(
            &mut context,
            "https://github.com/reddit/agent-hub/pull/42",
            Some(42),
            None,
            Some(&WorkflowEventKind::PrCreated),
            None,
            false,
            Utc::now(),
        );

        let state = github_pr_state_from_context(&context).expect("state persisted");
        assert_eq!(state.number, 42);
        assert!(matches!(state.state, GithubPrLifecycleState::Open));
        assert!(!state.approved);
        assert!(!state.checks_passing);
        assert!(!state.changes_requested);
        assert!(!state.mergeable);

        update_github_pr_state_in_context(
            &mut context,
            "https://github.com/reddit/agent-hub/pull/42",
            Some(42),
            None,
            Some(&WorkflowEventKind::PrApproved),
            None,
            false,
            Utc::now(),
        );
        let approved = github_pr_state_from_context(&context).expect("approved state");
        assert!(approved.approved);
        assert!(!approved.changes_requested);
        assert!(!approved.mergeable);

        update_github_pr_state_in_context(
            &mut context,
            "https://github.com/reddit/agent-hub/pull/42",
            Some(42),
            None,
            Some(&WorkflowEventKind::ChecksPassed),
            None,
            false,
            Utc::now(),
        );
        let ready = github_pr_state_from_context(&context).expect("ready state");
        assert!(ready.mergeable);

        update_github_pr_state_in_context(
            &mut context,
            "https://github.com/reddit/agent-hub/pull/42",
            Some(42),
            None,
            Some(&WorkflowEventKind::HumanChangesRequested),
            None,
            false,
            Utc::now(),
        );
        let blocked = github_pr_state_from_context(&context).expect("blocked state");
        assert!(!blocked.approved);
        assert!(blocked.changes_requested);
        assert!(!blocked.mergeable);

        update_github_pr_state_in_context(
            &mut context,
            "https://github.com/reddit/agent-hub/pull/42",
            Some(42),
            None,
            Some(&WorkflowEventKind::PrMerged),
            None,
            false,
            Utc::now(),
        );
        let merged = github_pr_state_from_context(&context).expect("merged state");
        assert!(matches!(merged.state, GithubPrLifecycleState::Merged));
        assert!(!merged.mergeable);
    }

    #[test]
    fn merge_readiness_respects_merge_policy_when_present() {
        let pr_state = GithubPrState {
            url: "https://github.com/reddit/agent-hub/pull/99".to_string(),
            number: 99,
            head_branch: None,
            state: GithubPrLifecycleState::Open,
            approval_count: 1,
            approved: true,
            passing_checks: vec!["lint".to_string()],
            checks_passing: true,
            changes_requested: false,
            merge_policy: Some(MergePolicy {
                required_checks: vec!["lint".to_string(), "test".to_string()],
                required_approvals: 2,
                dismiss_stale_reviews: false,
                require_up_to_date: false,
            }),
            merge_attempted: false,
            mergeable: false,
            last_updated: Utc::now(),
        };

        let readiness = merge_readiness_from_pr_state(&pr_state);
        assert!(!readiness.ready);
        assert!(!readiness.approved);
        assert!(!readiness.checks_passing);
    }

    #[test]
    fn parse_pr_number_from_url_extracts_terminal_segment() {
        assert_eq!(
            parse_pr_number_from_url("https://github.com/reddit/agent-hub/pull/123"),
            Some(123)
        );
        assert_eq!(
            parse_pr_number_from_url("https://github.com/reddit/agent-hub/pull/not-a-number"),
            None
        );
    }

    #[test]
    fn closed_unmerged_pr_sets_closed_state_without_mapped_event() {
        let mut context = serde_json::json!({});
        update_github_pr_state_in_context(
            &mut context,
            "https://github.com/reddit/agent-hub/pull/77",
            Some(77),
            None,
            None,
            None,
            true,
            Utc::now(),
        );
        let state = github_pr_state_from_context(&context).expect("state persisted");
        assert!(matches!(state.state, GithubPrLifecycleState::Closed));
        assert!(!state.mergeable);
    }

    #[test]
    fn item_github_pr_urls_includes_github_pr_state_url() {
        let item = WorkItem {
            id: Uuid::new_v4(),
            workflow_id: "wf".to_string(),
            external_task_id: None,
            external_task_source: None,
            title: None,
            description: None,
            labels: vec![],
            priority: None,
            session_id: None,
            current_step: "build".to_string(),
            projected_state: None,
            triage_status: None,
            human_gate: false,
            human_gate_state: None,
            blocked_on: None,
            state: WorkItemState::Running,
            context: serde_json::json!({
                "github_pr_state": {
                    "url": "https://github.com/reddit/agent-hub/pull/222"
                },
                "job_results": {
                    "create_pr": {
                        "github_pr": {
                            "url": "https://github.com/reddit/agent-hub/pull/111"
                        }
                    }
                }
            }),
            repo: None,
            parent_work_item_id: None,
            parent_step_id: None,
            child_work_item_id: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        let urls = item_github_pr_urls(&item).collect::<Vec<_>>();
        assert!(urls.contains(&"https://github.com/reddit/agent-hub/pull/222"));
        assert!(urls.contains(&"https://github.com/reddit/agent-hub/pull/111"));
    }

    #[test]
    fn item_matches_github_webhook_matches_branch_from_pr_state() {
        let item = WorkItem {
            id: Uuid::new_v4(),
            workflow_id: "wf".to_string(),
            external_task_id: None,
            external_task_source: None,
            title: None,
            description: None,
            labels: vec![],
            priority: None,
            session_id: None,
            current_step: "build".to_string(),
            projected_state: None,
            triage_status: None,
            human_gate: false,
            human_gate_state: None,
            blocked_on: None,
            state: WorkItemState::Running,
            context: serde_json::json!({
                "github_pr_state": {
                    "head_branch": "feature/from-pr-state"
                }
            }),
            repo: None,
            parent_work_item_id: None,
            parent_step_id: None,
            child_work_item_id: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        let webhook_pr = GithubWebhookPrReference {
            pr_url: None,
            pr_number: None,
            head_branch: Some("feature/from-pr-state".to_string()),
        };

        assert!(item_matches_github_webhook(&item, &webhook_pr));
    }

    #[test]
    fn item_matches_github_webhook_matches_branch_from_job_result_repo_metadata() {
        let item = WorkItem {
            id: Uuid::new_v4(),
            workflow_id: "wf".to_string(),
            external_task_id: None,
            external_task_source: None,
            title: None,
            description: None,
            labels: vec![],
            priority: None,
            session_id: None,
            current_step: "build".to_string(),
            projected_state: None,
            triage_status: None,
            human_gate: false,
            human_gate_state: None,
            blocked_on: None,
            state: WorkItemState::Running,
            context: serde_json::json!({
                "job_results": {
                    "build": {
                        "repo": {
                            "target_branch": "feature/from-job-result"
                        }
                    }
                }
            }),
            repo: None,
            parent_work_item_id: None,
            parent_step_id: None,
            child_work_item_id: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        let webhook_pr = GithubWebhookPrReference {
            pr_url: None,
            pr_number: None,
            head_branch: Some("feature/from-job-result".to_string()),
        };

        assert!(item_matches_github_webhook(&item, &webhook_pr));
    }

    #[test]
    fn verifies_github_signature_from_known_test_vector() {
        let secret = "It's a Secret to Everybody";
        let body = b"Hello, World!";
        let signature = "sha256=757107ea0eb2509fc211221cce984b8a37570b6d7586c22c46f4379c8b043e17";

        assert!(verify_github_signature(secret, body, signature));
    }

    #[tokio::test]
    async fn stale_worker_requeues_assigned_job() {
        let db = std::env::temp_dir().join(format!("orch-{}.sqlite3", Uuid::new_v4()));
        let tasks = std::env::temp_dir().join(format!("tasks-{}", Uuid::new_v4()));
        let svc = OrchestrationService::new(
            workflow_catalog(),
            Arc::new(TaskFileStore::new(tasks.clone())) as Arc<dyn TaskAdapter>,
            db.to_string_lossy().into_owned(),
        );
        svc.init().await.expect("init");

        svc.create_work_item(CreateWorkItemRequest {
            workflow_id: "wf".into(),
            session_id: None,
            context: serde_json::json!({}),
            repo: None,
        })
        .await
        .expect("create");

        svc.register_worker(RegisterWorkerRequest {
            id: "stale".into(),
            hostname: "host".into(),
            supported_modes: vec![protocol::ExecutionMode::Raw],
        })
        .await
        .expect("reg");

        let job = svc
            .poll_job(PollJobRequest {
                worker_id: "stale".into(),
            })
            .await
            .expect("poll")
            .job
            .expect("assigned");

        let mut stale_worker = svc
            .get_worker("stale")
            .await
            .expect("get worker")
            .expect("exists");
        stale_worker.last_heartbeat_at = Utc::now() - chrono::TimeDelta::seconds(500);
        svc.upsert_worker(&stale_worker).await.expect("backdate");

        let requeued = svc
            .reconcile_stale_workers(Duration::from_secs(30))
            .await
            .expect("reconcile");
        assert!(requeued >= 1);

        let refreshed = svc
            .get_job(job.id)
            .await
            .expect("get job")
            .expect("job exists");
        assert_eq!(refreshed.state, JobState::Pending);
        assert!(refreshed.assigned_worker_id.is_none());

        let _ = std::fs::remove_file(db);
        let _ = std::fs::remove_dir_all(tasks);
    }

    #[tokio::test]
    async fn reactive_workflow_executes_agent_human_loop_to_terminal() {
        let db = std::env::temp_dir().join(format!("orch-{}.sqlite3", Uuid::new_v4()));
        let tasks = std::env::temp_dir().join(format!("tasks-{}", Uuid::new_v4()));
        let svc = OrchestrationService::new(
            reactive_workflow_catalog(),
            Arc::new(TaskFileStore::new(tasks.clone())) as Arc<dyn TaskAdapter>,
            db.to_string_lossy().into_owned(),
        );
        svc.init().await.expect("init");

        let item = svc
            .create_work_item(CreateWorkItemRequest {
                workflow_id: "reactive_wf".into(),
                session_id: None,
                context: serde_json::json!({}),
                repo: None,
            })
            .await
            .expect("create");

        svc.register_worker(RegisterWorkerRequest {
            id: "w1".into(),
            hostname: "h".into(),
            supported_modes: vec![protocol::ExecutionMode::Raw],
        })
        .await
        .expect("worker");

        let first_job = svc
            .poll_job(PollJobRequest {
                worker_id: "w1".into(),
            })
            .await
            .expect("poll")
            .job
            .expect("job");
        assert_eq!(first_job.step_id, "run_agent");

        svc.update_job(
            first_job.id,
            JobUpdateRequest {
                worker_id: "w1".into(),
                state: JobState::Running,
                transcript_dir: None,
                result: None,
            },
        )
        .await
        .expect("running first");

        svc.update_job(
            first_job.id,
            JobUpdateRequest {
                worker_id: "w1".into(),
                state: JobState::Succeeded,
                transcript_dir: None,
                result: None,
            },
        )
        .await
        .expect("complete first");

        let review = svc
            .get_work_item(item.id)
            .await
            .expect("get")
            .expect("exists");
        assert_eq!(review.current_step, "review");
        assert_eq!(review.state, WorkItemState::Running);

        svc.fire_event(
            item.id,
            FireEventRequest {
                event: WorkflowEventKind::HumanChangesRequested,
                job_id: None,
                reason: Some("changes requested".into()),
            },
        )
        .await
        .expect("loop back to agent");

        let second_job = svc
            .poll_job(PollJobRequest {
                worker_id: "w1".into(),
            })
            .await
            .expect("poll")
            .job
            .expect("second job");
        assert_eq!(second_job.step_id, "run_agent");

        svc.update_job(
            second_job.id,
            JobUpdateRequest {
                worker_id: "w1".into(),
                state: JobState::Running,
                transcript_dir: None,
                result: None,
            },
        )
        .await
        .expect("running second");

        svc.update_job(
            second_job.id,
            JobUpdateRequest {
                worker_id: "w1".into(),
                state: JobState::Succeeded,
                transcript_dir: None,
                result: None,
            },
        )
        .await
        .expect("complete second");

        svc.fire_event(
            item.id,
            FireEventRequest {
                event: WorkflowEventKind::HumanApproved,
                job_id: None,
                reason: Some("approved".into()),
            },
        )
        .await
        .expect("advance to terminal");

        let done = svc
            .get_work_item(item.id)
            .await
            .expect("get done")
            .expect("exists");
        assert_eq!(done.current_step, "done");
        assert_eq!(done.state, WorkItemState::Succeeded);

        let _ = std::fs::remove_file(db);
        let _ = std::fs::remove_dir_all(tasks);
    }

    #[tokio::test]
    async fn reactive_terminal_defaults_to_succeeded_when_outcome_missing() {
        let db = std::env::temp_dir().join(format!("orch-{}.sqlite3", Uuid::new_v4()));
        let tasks = std::env::temp_dir().join(format!("tasks-{}", Uuid::new_v4()));
        let svc = OrchestrationService::new(
            reactive_workflow_catalog(),
            Arc::new(TaskFileStore::new(tasks.clone())) as Arc<dyn TaskAdapter>,
            db.to_string_lossy().into_owned(),
        );
        svc.init().await.expect("init");

        let item = svc
            .create_work_item(CreateWorkItemRequest {
                workflow_id: "reactive_wf".into(),
                session_id: None,
                context: serde_json::json!({}),
                repo: None,
            })
            .await
            .expect("create");

        let after_create = svc
            .get_work_item(item.id)
            .await
            .expect("get")
            .expect("exists");
        assert_eq!(after_create.current_step, "run_agent");

        svc.register_worker(RegisterWorkerRequest {
            id: "w-default-terminal".into(),
            hostname: "h".into(),
            supported_modes: vec![protocol::ExecutionMode::Raw],
        })
        .await
        .expect("worker");

        let job = svc
            .poll_job(PollJobRequest {
                worker_id: "w-default-terminal".into(),
            })
            .await
            .expect("poll")
            .job
            .expect("job");

        svc.update_job(
            job.id,
            JobUpdateRequest {
                worker_id: "w-default-terminal".into(),
                state: JobState::Running,
                transcript_dir: None,
                result: None,
            },
        )
        .await
        .expect("running");

        svc.update_job(
            job.id,
            JobUpdateRequest {
                worker_id: "w-default-terminal".into(),
                state: JobState::Succeeded,
                transcript_dir: None,
                result: None,
            },
        )
        .await
        .expect("succeeded");

        svc.fire_event(
            item.id,
            FireEventRequest {
                event: WorkflowEventKind::HumanApproved,
                job_id: None,
                reason: Some("approve".into()),
            },
        )
        .await
        .expect("approve");

        let done = svc
            .get_work_item(item.id)
            .await
            .expect("get done")
            .expect("exists");
        assert_eq!(done.current_step, "done");
        assert_eq!(done.state, WorkItemState::Succeeded);

        let _ = std::fs::remove_file(db);
        let _ = std::fs::remove_dir_all(tasks);
    }

    #[tokio::test]
    async fn reactive_unmatched_non_terminal_event_errors() {
        let db = std::env::temp_dir().join(format!("orch-{}.sqlite3", Uuid::new_v4()));
        let tasks = std::env::temp_dir().join(format!("tasks-{}", Uuid::new_v4()));
        let svc = OrchestrationService::new(
            reactive_workflow_catalog(),
            Arc::new(TaskFileStore::new(tasks.clone())) as Arc<dyn TaskAdapter>,
            db.to_string_lossy().into_owned(),
        );
        svc.init().await.expect("init");

        let item = svc
            .create_work_item(CreateWorkItemRequest {
                workflow_id: "reactive_wf".into(),
                session_id: None,
                context: serde_json::json!({}),
                repo: None,
            })
            .await
            .expect("create");

        let err = svc
            .fire_event(
                item.id,
                FireEventRequest {
                    event: WorkflowEventKind::Start,
                    job_id: None,
                    reason: None,
                },
            )
            .await
            .expect_err("should fail on unmatched event");
        assert!(matches!(err, AppError::WorkflowInvalid(_)));

        let _ = std::fs::remove_file(db);
        let _ = std::fs::remove_dir_all(tasks);
    }

    #[tokio::test]
    async fn reactive_terminal_state_rejects_unmatched_late_external_events() {
        let db = std::env::temp_dir().join(format!("orch-{}.sqlite3", Uuid::new_v4()));
        let tasks = std::env::temp_dir().join(format!("tasks-{}", Uuid::new_v4()));
        let svc = OrchestrationService::new(
            reactive_submit_catalog(),
            Arc::new(TaskFileStore::new(tasks.clone())) as Arc<dyn TaskAdapter>,
            db.to_string_lossy().into_owned(),
        );
        svc.init().await.expect("init");

        let item = svc
            .create_work_item(CreateWorkItemRequest {
                workflow_id: "reactive_submit".into(),
                session_id: None,
                context: serde_json::json!({}),
                repo: None,
            })
            .await
            .expect("create");

        svc.fire_event(
            item.id,
            FireEventRequest {
                event: WorkflowEventKind::Submit,
                job_id: None,
                reason: Some("human submitted".into()),
            },
        )
        .await
        .expect("submit transitions workflow");

        let done_before = svc
            .get_work_item(item.id)
            .await
            .expect("get done")
            .expect("exists");
        assert_eq!(done_before.current_step, "done");
        assert_eq!(done_before.state, WorkItemState::Failed);

        let err = svc
            .fire_event(
                item.id,
                FireEventRequest {
                    event: WorkflowEventKind::JobFailed,
                    job_id: None,
                    reason: Some("late event".into()),
                },
            )
            .await
            .expect_err("late terminal event should be rejected");
        assert!(matches!(err, AppError::WorkflowInvalid(_)));
        let msg = err.to_string();
        assert!(msg.contains("event 'job_failed' is not currently valid"));
        assert!(msg.contains("valid_next_events=[]"));

        let done_after = svc
            .get_work_item(item.id)
            .await
            .expect("get done after")
            .expect("exists");
        assert_eq!(done_after.current_step, "done");
        assert_eq!(done_after.state, WorkItemState::Failed);

        let _ = std::fs::remove_file(db);
        let _ = std::fs::remove_dir_all(tasks);
    }

    #[tokio::test]
    async fn reactive_submit_event_transitions_human_to_terminal() {
        let db = std::env::temp_dir().join(format!("orch-{}.sqlite3", Uuid::new_v4()));
        let tasks = std::env::temp_dir().join(format!("tasks-{}", Uuid::new_v4()));
        let svc = OrchestrationService::new(
            reactive_submit_catalog(),
            Arc::new(TaskFileStore::new(tasks.clone())) as Arc<dyn TaskAdapter>,
            db.to_string_lossy().into_owned(),
        );
        svc.init().await.expect("init");

        let item = svc
            .create_work_item(CreateWorkItemRequest {
                workflow_id: "reactive_submit".into(),
                session_id: None,
                context: serde_json::json!({}),
                repo: None,
            })
            .await
            .expect("create");

        let before = svc
            .get_work_item(item.id)
            .await
            .expect("get")
            .expect("exists");
        assert_eq!(before.current_step, "draft");
        assert_eq!(before.state, WorkItemState::Running);

        svc.fire_event(
            item.id,
            FireEventRequest {
                event: WorkflowEventKind::Submit,
                job_id: None,
                reason: Some("human submitted".into()),
            },
        )
        .await
        .expect("submit transitions workflow");

        let done = svc
            .get_work_item(item.id)
            .await
            .expect("get done")
            .expect("exists");
        assert_eq!(done.current_step, "done");
        assert_eq!(done.state, WorkItemState::Failed);

        let _ = std::fs::remove_file(db);
        let _ = std::fs::remove_dir_all(tasks);
    }

    #[tokio::test]
    async fn reactive_auto_without_start_rejected_by_catalog_validation() {
        let db = std::env::temp_dir().join(format!("orch-{}.sqlite3", Uuid::new_v4()));
        let tasks = std::env::temp_dir().join(format!("tasks-{}", Uuid::new_v4()));
        let svc = OrchestrationService::new(
            reactive_no_start_catalog(),
            Arc::new(TaskFileStore::new(tasks.clone())) as Arc<dyn TaskAdapter>,
            db.to_string_lossy().into_owned(),
        );
        svc.init().await.expect("init");

        let err = svc
            .create_work_item(CreateWorkItemRequest {
                workflow_id: "reactive_no_start".into(),
                session_id: None,
                context: serde_json::json!({}),
                repo: None,
            })
            .await
            .expect_err("should fail");
        assert!(matches!(err, AppError::WorkflowNotFound(_)));

        let _ = std::fs::remove_file(db);
        let _ = std::fs::remove_dir_all(tasks);
    }

    #[tokio::test]
    async fn reactive_child_completion_progresses_legacy_parent() {
        let db = std::env::temp_dir().join(format!("orch-{}.sqlite3", Uuid::new_v4()));
        let tasks = std::env::temp_dir().join(format!("tasks-{}", Uuid::new_v4()));
        let svc = OrchestrationService::new(
            reactive_child_only_catalog(),
            Arc::new(TaskFileStore::new(tasks.clone())) as Arc<dyn TaskAdapter>,
            db.to_string_lossy().into_owned(),
        );
        svc.init().await.expect("init");

        let now = Utc::now();
        let parent_id = Uuid::new_v4();
        let child_id = Uuid::new_v4();

        let parent = WorkItem {
            id: parent_id,
            workflow_id: "wf".into(),
            external_task_id: None,
            external_task_source: None,
            title: None,
            description: None,
            labels: vec![],
            priority: None,
            session_id: None,
            current_step: "build".into(),
            projected_state: Some("running".into()),
            triage_status: None,
            human_gate: false,
            human_gate_state: None,
            blocked_on: None,
            state: WorkItemState::Running,
            context: serde_json::json!({}),
            repo: None,
            parent_work_item_id: None,
            parent_step_id: None,
            child_work_item_id: Some(child_id),
            created_at: now,
            updated_at: now,
        };
        svc.insert_work_item(&parent).await.expect("insert parent");

        let mut child = WorkItem {
            id: child_id,
            workflow_id: "child_reactive".into(),
            external_task_id: None,
            external_task_source: None,
            title: None,
            description: None,
            labels: vec![],
            priority: None,
            session_id: None,
            current_step: "bootstrap".into(),
            projected_state: Some("pending".into()),
            triage_status: None,
            human_gate: false,
            human_gate_state: None,
            blocked_on: None,
            state: WorkItemState::Pending,
            context: serde_json::json!({}),
            repo: None,
            parent_work_item_id: Some(parent_id),
            parent_step_id: Some("build".into()),
            child_work_item_id: None,
            created_at: now,
            updated_at: now,
        };
        svc.insert_work_item(&child).await.expect("insert child");
        let _ = svc
            .ensure_pending_for_current_state(&mut child)
            .await
            .expect("prepare child reactive state");

        svc.register_worker(RegisterWorkerRequest {
            id: "w1".into(),
            hostname: "h".into(),
            supported_modes: vec![protocol::ExecutionMode::Raw],
        })
        .await
        .expect("worker");

        let child_job = svc
            .poll_job(PollJobRequest {
                worker_id: "w1".into(),
            })
            .await
            .expect("poll")
            .job
            .expect("child job");

        svc.update_job(
            child_job.id,
            JobUpdateRequest {
                worker_id: "w1".into(),
                state: JobState::Running,
                transcript_dir: None,
                result: None,
            },
        )
        .await
        .expect("running");

        svc.update_job(
            child_job.id,
            JobUpdateRequest {
                worker_id: "w1".into(),
                state: JobState::Succeeded,
                transcript_dir: None,
                result: None,
            },
        )
        .await
        .expect("child success");

        let parent_after = svc
            .get_work_item(parent_id)
            .await
            .expect("get parent")
            .expect("exists");
        assert_eq!(parent_after.current_step, "after");
        assert!(parent_after.child_work_item_id.is_none());

        let _ = std::fs::remove_file(db);
        let _ = std::fs::remove_dir_all(tasks);
    }

    #[tokio::test]
    async fn reactive_parent_child_progresses_on_child_success() {
        let db = std::env::temp_dir().join(format!("orch-{}.sqlite3", Uuid::new_v4()));
        let tasks = std::env::temp_dir().join(format!("tasks-{}", Uuid::new_v4()));
        let svc = OrchestrationService::new(
            reactive_parent_child_catalog(),
            Arc::new(TaskFileStore::new(tasks.clone())) as Arc<dyn TaskAdapter>,
            db.to_string_lossy().into_owned(),
        );
        svc.init().await.expect("init");

        let parent = svc
            .create_work_item(CreateWorkItemRequest {
                workflow_id: "reactive_parent".into(),
                session_id: None,
                context: serde_json::json!({"task": "x"}),
                repo: None,
            })
            .await
            .expect("create parent");

        let items = svc.list_work_items().await.expect("items");
        let parent_now = items
            .iter()
            .find(|i| i.id == parent.id)
            .expect("parent exists");
        let child_item = items
            .iter()
            .find(|i| {
                i.parent_work_item_id == Some(parent.id)
                    && i.parent_step_id.as_deref() == Some("spawn")
            })
            .expect("child exists")
            .clone();
        assert_eq!(parent_now.child_work_item_id, Some(child_item.id));
        assert_eq!(child_item.workflow_id, "child_legacy");

        svc.register_worker(RegisterWorkerRequest {
            id: "w1".into(),
            hostname: "h".into(),
            supported_modes: vec![protocol::ExecutionMode::Raw],
        })
        .await
        .expect("worker");

        let child_job = svc
            .poll_job(PollJobRequest {
                worker_id: "w1".into(),
            })
            .await
            .expect("poll")
            .job
            .expect("child job");
        assert_eq!(child_job.work_item_id, child_item.id);

        svc.update_job(
            child_job.id,
            JobUpdateRequest {
                worker_id: "w1".into(),
                state: JobState::Running,
                transcript_dir: None,
                result: None,
            },
        )
        .await
        .expect("running");

        svc.update_job(
            child_job.id,
            JobUpdateRequest {
                worker_id: "w1".into(),
                state: JobState::Succeeded,
                transcript_dir: None,
                result: None,
            },
        )
        .await
        .expect("child success");

        let parent_after = svc
            .get_work_item(parent.id)
            .await
            .expect("get parent")
            .expect("exists");
        assert_eq!(parent_after.state, WorkItemState::Succeeded);
        assert_eq!(parent_after.current_step, "done");
        assert!(parent_after.child_work_item_id.is_none());
    }

    #[tokio::test]
    async fn reactive_parent_child_progresses_to_failure_on_child_failure() {
        let db = std::env::temp_dir().join(format!("orch-{}.sqlite3", Uuid::new_v4()));
        let tasks = std::env::temp_dir().join(format!("tasks-{}", Uuid::new_v4()));
        let svc = OrchestrationService::new(
            reactive_parent_child_catalog(),
            Arc::new(TaskFileStore::new(tasks.clone())) as Arc<dyn TaskAdapter>,
            db.to_string_lossy().into_owned(),
        );
        svc.init().await.expect("init");

        let parent = svc
            .create_work_item(CreateWorkItemRequest {
                workflow_id: "reactive_parent".into(),
                session_id: None,
                context: serde_json::json!({}),
                repo: None,
            })
            .await
            .expect("create parent");

        let child_item = svc
            .list_work_items()
            .await
            .expect("items")
            .into_iter()
            .find(|i| {
                i.parent_work_item_id == Some(parent.id)
                    && i.parent_step_id.as_deref() == Some("spawn")
            })
            .expect("child exists");

        svc.register_worker(RegisterWorkerRequest {
            id: "w1".into(),
            hostname: "h".into(),
            supported_modes: vec![protocol::ExecutionMode::Raw],
        })
        .await
        .expect("worker");

        let child_job = svc
            .poll_job(PollJobRequest {
                worker_id: "w1".into(),
            })
            .await
            .expect("poll")
            .job
            .expect("child job");
        assert_eq!(child_job.work_item_id, child_item.id);

        svc.update_job(
            child_job.id,
            JobUpdateRequest {
                worker_id: "w1".into(),
                state: JobState::Running,
                transcript_dir: None,
                result: None,
            },
        )
        .await
        .expect("running");

        svc.update_job(
            child_job.id,
            JobUpdateRequest {
                worker_id: "w1".into(),
                state: JobState::Failed,
                transcript_dir: None,
                result: None,
            },
        )
        .await
        .expect("child failed");

        let parent_after = svc
            .get_work_item(parent.id)
            .await
            .expect("get parent")
            .expect("exists");
        assert_eq!(parent_after.state, WorkItemState::Failed);
        assert_eq!(parent_after.current_step, "blocked");
        assert!(parent_after.child_work_item_id.is_none());

        let _ = std::fs::remove_file(db);
        let _ = std::fs::remove_dir_all(tasks);
    }

    #[tokio::test]
    async fn lease_expiry_requeues_running_job() {
        let db = std::env::temp_dir().join(format!("orch-{}.sqlite3", Uuid::new_v4()));
        let tasks = std::env::temp_dir().join(format!("tasks-{}", Uuid::new_v4()));
        let svc = OrchestrationService::new(
            workflow_catalog(),
            Arc::new(TaskFileStore::new(tasks.clone())) as Arc<dyn TaskAdapter>,
            db.to_string_lossy().into_owned(),
        );
        svc.init().await.expect("init");

        svc.create_work_item(CreateWorkItemRequest {
            workflow_id: "wf".into(),
            session_id: None,
            context: serde_json::json!({}),
            repo: None,
        })
        .await
        .expect("create");

        svc.register_worker(RegisterWorkerRequest {
            id: "w1".into(),
            hostname: "h".into(),
            supported_modes: vec![protocol::ExecutionMode::Raw],
        })
        .await
        .expect("worker");

        let mut job = svc
            .poll_job(PollJobRequest {
                worker_id: "w1".into(),
            })
            .await
            .expect("poll")
            .job
            .expect("job");
        job.state = JobState::Running;
        job.lease_expires_at = Some(Utc::now() - chrono::TimeDelta::seconds(5));
        svc.update_job_row(&job).await.expect("set expired lease");

        let requeued = svc.requeue_expired_leases().await.expect("requeue");
        assert_eq!(requeued, 1);

        let refreshed = svc.get_job(job.id).await.expect("get").expect("exists");
        assert_eq!(refreshed.state, JobState::Pending);
        assert!(refreshed.assigned_worker_id.is_none());
        assert!(refreshed.lease_expires_at.is_none());

        let _ = std::fs::remove_file(db);
        let _ = std::fs::remove_dir_all(tasks);
    }

    #[tokio::test]
    async fn cancel_event_cancels_active_jobs() {
        let db = std::env::temp_dir().join(format!("orch-{}.sqlite3", Uuid::new_v4()));
        let tasks = std::env::temp_dir().join(format!("tasks-{}", Uuid::new_v4()));
        let svc = OrchestrationService::new(
            workflow_catalog(),
            Arc::new(TaskFileStore::new(tasks.clone())) as Arc<dyn TaskAdapter>,
            db.to_string_lossy().into_owned(),
        );
        svc.init().await.expect("init");

        let item = svc
            .create_work_item(CreateWorkItemRequest {
                workflow_id: "wf".into(),
                session_id: None,
                context: serde_json::json!({}),
                repo: None,
            })
            .await
            .expect("create");

        let mut jobs = svc.list_jobs().await.expect("jobs");
        assert_eq!(jobs.len(), 1);
        jobs[0].state = JobState::Running;
        jobs[0].assigned_worker_id = Some("w1".into());
        svc.update_job_row(&jobs[0]).await.expect("update running");

        let _ = svc
            .fire_event(
                item.id,
                FireEventRequest {
                    event: WorkflowEventKind::Cancel,
                    job_id: None,
                    reason: None,
                },
            )
            .await
            .expect("cancel");

        let refreshed = svc
            .get_job(jobs[0].id)
            .await
            .expect("get job")
            .expect("exists");
        assert_eq!(refreshed.state, JobState::Cancelled);

        let _ = std::fs::remove_file(db);
        let _ = std::fs::remove_dir_all(tasks);
    }

    #[tokio::test]
    async fn running_to_running_renews_lease() {
        let db = std::env::temp_dir().join(format!("orch-{}.sqlite3", Uuid::new_v4()));
        let tasks = std::env::temp_dir().join(format!("tasks-{}", Uuid::new_v4()));
        let svc = OrchestrationService::new(
            workflow_catalog(),
            Arc::new(TaskFileStore::new(tasks.clone())) as Arc<dyn TaskAdapter>,
            db.to_string_lossy().into_owned(),
        );
        svc.init().await.expect("init");

        svc.create_work_item(CreateWorkItemRequest {
            workflow_id: "wf".into(),
            session_id: None,
            context: serde_json::json!({}),
            repo: None,
        })
        .await
        .expect("create");

        svc.register_worker(RegisterWorkerRequest {
            id: "w1".into(),
            hostname: "h".into(),
            supported_modes: vec![protocol::ExecutionMode::Raw],
        })
        .await
        .expect("worker");

        let job = svc
            .poll_job(PollJobRequest {
                worker_id: "w1".into(),
            })
            .await
            .expect("poll")
            .job
            .expect("job");

        let running = svc
            .update_job(
                job.id,
                JobUpdateRequest {
                    worker_id: "w1".into(),
                    state: JobState::Running,
                    transcript_dir: None,
                    result: None,
                },
            )
            .await
            .expect("set running");
        let first_lease = running.lease_expires_at.expect("lease set");

        tokio::time::sleep(Duration::from_millis(10)).await;

        let renewed = svc
            .update_job(
                job.id,
                JobUpdateRequest {
                    worker_id: "w1".into(),
                    state: JobState::Running,
                    transcript_dir: None,
                    result: None,
                },
            )
            .await
            .expect("renew");
        let second_lease = renewed.lease_expires_at.expect("lease set");
        assert!(second_lease > first_lease);

        let _ = std::fs::remove_file(db);
        let _ = std::fs::remove_dir_all(tasks);
    }

    #[tokio::test]
    async fn retries_failed_step_before_on_failure() {
        let tmp = std::env::temp_dir().join(format!("wf-retry-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).expect("mkdir");
        let wf = r#"
[workflow]
id = "wf_retry"
entry_step = "build"

[[workflow.steps]]
id = "build"
kind = "agent"
executor = "raw"
command = "echo"
args = ["x"]
max_retries = 1
on_failure = "failstep"

[[workflow.steps]]
id = "failstep"
kind = "agent"
executor = "raw"
command = "echo"
args = ["f"]
"#;
        std::fs::write(tmp.join("wf.toml"), wf).expect("write");

        let db = std::env::temp_dir().join(format!("orch-{}.sqlite3", Uuid::new_v4()));
        let tasks = std::env::temp_dir().join(format!("tasks-{}", Uuid::new_v4()));
        let svc = OrchestrationService::new(
            Arc::new(WorkflowCatalog::load_from_dir(tmp.to_str().unwrap()).expect("catalog")),
            Arc::new(TaskFileStore::new(tasks.clone())) as Arc<dyn TaskAdapter>,
            db.to_string_lossy().into_owned(),
        );
        svc.init().await.expect("init");

        svc.create_work_item(CreateWorkItemRequest {
            workflow_id: "wf_retry".into(),
            session_id: None,
            context: serde_json::json!({}),
            repo: None,
        })
        .await
        .expect("create");

        svc.register_worker(RegisterWorkerRequest {
            id: "w1".into(),
            hostname: "h".into(),
            supported_modes: vec![protocol::ExecutionMode::Raw],
        })
        .await
        .expect("worker");

        let first = svc
            .poll_job(PollJobRequest {
                worker_id: "w1".into(),
            })
            .await
            .expect("poll")
            .job
            .expect("job");

        svc.update_job(
            first.id,
            JobUpdateRequest {
                worker_id: "w1".into(),
                state: JobState::Running,
                transcript_dir: None,
                result: None,
            },
        )
        .await
        .expect("running");

        svc.update_job(
            first.id,
            JobUpdateRequest {
                worker_id: "w1".into(),
                state: JobState::Failed,
                transcript_dir: None,
                result: None,
            },
        )
        .await
        .expect("failed");

        let jobs = svc.list_jobs().await.expect("jobs");
        assert!(jobs.iter().any(|j| j.step_id == "build" && j.attempt == 1));

        let _ = std::fs::remove_file(db);
        let _ = std::fs::remove_dir_all(tasks);
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[tokio::test]
    async fn interpolation_materializes_job_inputs() {
        let tmp = std::env::temp_dir().join(format!("wf-interp-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).expect("mkdir");
        let wf = r#"
[workflow]
id = "wf_interp"
entry_step = "s1"

[[workflow.steps]]
id = "s1"
kind = "agent"
executor = "raw"
command = "echo"
args = ["{{task.name}}", "{{task.count}}"]

[workflow.steps.env]
TASK_NAME = "{{task.name}}"
"#;
        std::fs::write(tmp.join("wf.toml"), wf).expect("write");

        let db = std::env::temp_dir().join(format!("orch-{}.sqlite3", Uuid::new_v4()));
        let tasks = std::env::temp_dir().join(format!("tasks-{}", Uuid::new_v4()));
        let svc = OrchestrationService::new(
            Arc::new(WorkflowCatalog::load_from_dir(tmp.to_str().unwrap()).expect("catalog")),
            Arc::new(TaskFileStore::new(tasks.clone())) as Arc<dyn TaskAdapter>,
            db.to_string_lossy().into_owned(),
        );
        svc.init().await.expect("init");

        svc.create_work_item(CreateWorkItemRequest {
            workflow_id: "wf_interp".into(),
            session_id: None,
            context: serde_json::json!({"task": {"name": "demo", "count": 2}}),
            repo: None,
        })
        .await
        .expect("create");

        let jobs = svc.list_jobs().await.expect("jobs");
        let job = jobs.iter().find(|j| j.step_id == "s1").expect("job");
        assert_eq!(job.args, vec!["demo".to_string(), "2".to_string()]);
        assert_eq!(job.env.get("TASK_NAME").cloned(), Some("demo".to_string()));

        let _ = std::fs::remove_file(db);
        let _ = std::fs::remove_dir_all(tasks);
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[tokio::test]
    async fn parent_waits_for_single_child_and_advances_on_child_success() {
        let tmp = std::env::temp_dir().join(format!("wf-child-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).expect("mkdir");
        let wf = r#"
[workflow]
id = "parent"
entry_step = "spawn"

[[workflow.steps]]
id = "spawn"
kind = "child_workflow"
executor = "raw"
child_workflow_id = "child"
command = ""
on_success = "after"

[[workflow.steps]]
id = "after"
kind = "agent"
executor = "raw"
command = "echo"
args = ["done"]
"#;
        let child = r#"
[workflow]
id = "child"
entry_step = "run"

[[workflow.steps]]
id = "run"
kind = "agent"
executor = "raw"
command = "echo"
args = ["child"]
"#;
        std::fs::write(tmp.join("parent.toml"), wf).expect("write parent");
        std::fs::write(tmp.join("child.toml"), child).expect("write child");

        let db = std::env::temp_dir().join(format!("orch-{}.sqlite3", Uuid::new_v4()));
        let tasks = std::env::temp_dir().join(format!("tasks-{}", Uuid::new_v4()));
        let svc = OrchestrationService::new(
            Arc::new(WorkflowCatalog::load_from_dir(tmp.to_str().unwrap()).expect("catalog")),
            Arc::new(TaskFileStore::new(tasks.clone())) as Arc<dyn TaskAdapter>,
            db.to_string_lossy().into_owned(),
        );
        svc.init().await.expect("init");

        let parent = svc
            .create_work_item(CreateWorkItemRequest {
                workflow_id: "parent".into(),
                session_id: None,
                context: serde_json::json!({"task": "x"}),
                repo: None,
            })
            .await
            .expect("create parent");

        let items = svc.list_work_items().await.expect("items");
        let parent_now = items
            .iter()
            .find(|i| i.id == parent.id)
            .expect("parent exists");
        let child_item = items
            .iter()
            .find(|i| i.parent_work_item_id == Some(parent.id))
            .expect("child exists")
            .clone();
        assert_eq!(parent_now.child_work_item_id, Some(child_item.id));

        svc.register_worker(RegisterWorkerRequest {
            id: "w1".into(),
            hostname: "h".into(),
            supported_modes: vec![protocol::ExecutionMode::Raw],
        })
        .await
        .expect("worker");

        let child_job = svc
            .poll_job(PollJobRequest {
                worker_id: "w1".into(),
            })
            .await
            .expect("poll")
            .job
            .expect("child job");
        assert_eq!(child_job.work_item_id, child_item.id);

        svc.update_job(
            child_job.id,
            JobUpdateRequest {
                worker_id: "w1".into(),
                state: JobState::Running,
                transcript_dir: None,
                result: None,
            },
        )
        .await
        .expect("running");

        svc.update_job(
            child_job.id,
            JobUpdateRequest {
                worker_id: "w1".into(),
                state: JobState::Succeeded,
                transcript_dir: None,
                result: Some(JobResult {
                    outcome: protocol::ExecutionOutcome::Succeeded,
                    termination_reason: protocol::TerminationReason::ExitCode,
                    exit_code: Some(0),
                    timing: protocol::ExecutionTiming {
                        started_at: Utc::now(),
                        finished_at: Utc::now(),
                        duration_ms: 1,
                    },
                    artifacts: protocol::ArtifactSummary {
                        stdout_path: "a".into(),
                        stderr_path: "b".into(),
                        output_path: None,
                        transcript_dir: None,
                        stdout_bytes: None,
                        stderr_bytes: None,
                        output_bytes: None,
                        structured_transcript_artifacts: vec![
                            protocol::StructuredTranscriptArtifact {
                                key: "agent_context".into(),
                                path: "/tmp/child/.agent-context.json".into(),
                                bytes: Some(42),
                                schema: Some(protocol::AGENT_CONTEXT_SCHEMA_NAME.into()),
                                record_count: None,
                                inline_json: Some(serde_json::json!({
                                    "schema": protocol::AGENT_CONTEXT_SCHEMA_NAME,
                                    "job": {
                                        "step_id": "run"
                                    }
                                })),
                            },
                            protocol::StructuredTranscriptArtifact {
                                key: "run_json".into(),
                                path: "/tmp/child/run.json".into(),
                                bytes: Some(64),
                                schema: Some(protocol::JOB_RUN_SCHEMA_NAME.into()),
                                record_count: None,
                                inline_json: Some(serde_json::json!({
                                    "schema": protocol::JOB_RUN_SCHEMA_NAME,
                                    "workspace": "/workspace"
                                })),
                            },
                        ],
                    },
                    output_json: Some(serde_json::json!({"child_ok": true})),
                    repo: None,
                    github_pr: None,
                    error_message: None,
                }),
            },
        )
        .await
        .expect("child success");

        let parent_after = svc
            .get_work_item(parent.id)
            .await
            .expect("get parent")
            .expect("exists");
        assert_eq!(parent_after.current_step, "after");
        assert!(parent_after.context["child_results"]["spawn"].is_object());
        assert_eq!(
            parent_after.context["child_results"]["spawn"]["job_artifact_paths"]["run"]["agent_context"],
            serde_json::json!("/tmp/child/.agent-context.json")
        );
        assert_eq!(
            parent_after.context["child_results"]["spawn"]["job_artifact_metadata"]["run"]["agent_context"]
                ["schema"],
            serde_json::json!(protocol::AGENT_CONTEXT_SCHEMA_NAME)
        );
        assert_eq!(
            parent_after.context["child_results"]["spawn"]["job_artifact_metadata"]["run"]["run_json"]
                ["schema"],
            serde_json::json!(protocol::JOB_RUN_SCHEMA_NAME)
        );
        assert_eq!(
            parent_after.context["child_results"]["spawn"]["job_artifact_data"]["run"]["agent_context"]
                ["schema"],
            serde_json::json!(protocol::AGENT_CONTEXT_SCHEMA_NAME)
        );
        assert_eq!(
            parent_after.context["child_results"]["spawn"]["job_artifact_data"]["run"]["run_json"]
                ["schema"],
            serde_json::json!(protocol::JOB_RUN_SCHEMA_NAME)
        );

        let _ = std::fs::remove_file(db);
        let _ = std::fs::remove_dir_all(tasks);
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[tokio::test]
    async fn child_success_without_on_success_marks_parent_succeeded_via_system_event() {
        let tmp = std::env::temp_dir().join(format!("wf-child-terminal-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).expect("mkdir");
        let parent = r#"
[workflow]
id = "parent_terminal"
entry_step = "spawn"

[[workflow.steps]]
id = "spawn"
kind = "child_workflow"
executor = "raw"
child_workflow_id = "child"
command = ""
"#;
        let child = r#"
[workflow]
id = "child"
entry_step = "run"

[[workflow.steps]]
id = "run"
kind = "agent"
executor = "raw"
command = "echo"
args = ["child"]
"#;
        std::fs::write(tmp.join("parent.toml"), parent).expect("write parent");
        std::fs::write(tmp.join("child.toml"), child).expect("write child");

        let db = std::env::temp_dir().join(format!("orch-{}.sqlite3", Uuid::new_v4()));
        let tasks = std::env::temp_dir().join(format!("tasks-{}", Uuid::new_v4()));
        let svc = OrchestrationService::new(
            Arc::new(WorkflowCatalog::load_from_dir(tmp.to_str().unwrap()).expect("catalog")),
            Arc::new(TaskFileStore::new(tasks.clone())) as Arc<dyn TaskAdapter>,
            db.to_string_lossy().into_owned(),
        );
        svc.init().await.expect("init");

        let parent = svc
            .create_work_item(CreateWorkItemRequest {
                workflow_id: "parent_terminal".into(),
                session_id: None,
                context: serde_json::json!({}),
                repo: None,
            })
            .await
            .expect("create parent");

        svc.register_worker(RegisterWorkerRequest {
            id: "w1".into(),
            hostname: "h".into(),
            supported_modes: vec![protocol::ExecutionMode::Raw],
        })
        .await
        .expect("worker");

        let child_job = svc
            .poll_job(PollJobRequest {
                worker_id: "w1".into(),
            })
            .await
            .expect("poll")
            .job
            .expect("child job");

        svc.update_job(
            child_job.id,
            JobUpdateRequest {
                worker_id: "w1".into(),
                state: JobState::Running,
                transcript_dir: None,
                result: None,
            },
        )
        .await
        .expect("running");

        svc.update_job(
            child_job.id,
            JobUpdateRequest {
                worker_id: "w1".into(),
                state: JobState::Succeeded,
                transcript_dir: None,
                result: None,
            },
        )
        .await
        .expect("child success");

        let parent_after = svc
            .get_work_item(parent.id)
            .await
            .expect("get parent")
            .expect("exists");
        assert_eq!(parent_after.state, WorkItemState::Succeeded);
        assert!(parent_after.child_work_item_id.is_none());

        let _ = std::fs::remove_file(db);
        let _ = std::fs::remove_dir_all(tasks);
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[tokio::test]
    async fn sync_updates_existing_item_only_when_adapter_is_newer() {
        let db = std::env::temp_dir().join(format!("orch-{}.sqlite3", Uuid::new_v4()));
        let tasks = std::env::temp_dir().join(format!("tasks-{}", Uuid::new_v4()));
        let adapter = Arc::new(TaskFileStore::new(tasks.clone())) as Arc<dyn TaskAdapter>;
        let svc = OrchestrationService::new(
            workflow_catalog(),
            Arc::clone(&adapter),
            db.to_string_lossy().into_owned(),
        );
        svc.init().await.expect("init");

        let item = svc
            .create_work_item(CreateWorkItemRequest {
                workflow_id: "wf".into(),
                session_id: None,
                context: serde_json::json!({"v": 1}),
                repo: None,
            })
            .await
            .expect("create");

        let mut external = task_from_work_item(&item, None);
        external.context = serde_json::json!({"v": 2});
        external.updated_at = item.updated_at + chrono::TimeDelta::seconds(5);
        adapter
            .persist_projection(&TaskProjection {
                task: external.clone(),
            })
            .expect("write newer external");

        svc.sync_tasks_from_adapter().await.expect("sync newer");
        let refreshed = svc
            .get_work_item(item.id)
            .await
            .expect("get")
            .expect("exists");
        assert_eq!(refreshed.context["v"], serde_json::json!(2));

        external.context = serde_json::json!({"v": 3});
        external.updated_at = refreshed.updated_at - chrono::TimeDelta::seconds(10);
        adapter
            .persist_projection(&TaskProjection { task: external })
            .expect("write stale external");

        svc.sync_tasks_from_adapter().await.expect("sync stale");
        let refreshed_again = svc
            .get_work_item(item.id)
            .await
            .expect("get")
            .expect("exists");
        assert_eq!(refreshed_again.context["v"], serde_json::json!(2));

        let _ = std::fs::remove_file(db);
        let _ = std::fs::remove_dir_all(tasks);
    }

    #[tokio::test]
    async fn persists_runtime_spec_json_for_docker_job() {
        let db = std::env::temp_dir().join(format!("orch-{}.sqlite3", Uuid::new_v4()));
        let tasks = std::env::temp_dir().join(format!("tasks-{}", Uuid::new_v4()));
        let svc = OrchestrationService::new(
            workflow_catalog(),
            Arc::new(TaskFileStore::new(tasks.clone())) as Arc<dyn TaskAdapter>,
            db.to_string_lossy().into_owned(),
        );
        svc.init().await.expect("init");

        let now = Utc::now();
        let job = Job {
            id: Uuid::new_v4(),
            work_item_id: Uuid::new_v4(),
            step_id: "s1".into(),
            state: JobState::Pending,
            assigned_worker_id: None,
            execution_mode: protocol::ExecutionMode::Docker,
            container_image: Some("alpine:3.20".into()),
            command: "echo".into(),
            args: vec!["hi".into()],
            env: HashMap::new(),
            prompt: None,
            context: serde_json::json!({}),
            repo: None,
            timeout_secs: Some(60),
            max_retries: 0,
            attempt: 0,
            transcript_dir: None,
            result: None,
            lease_expires_at: None,
            created_at: now,
            updated_at: now,
        };

        svc.insert_job(&job).await.expect("insert job");
        let loaded = svc.get_job(job.id).await.expect("get job").expect("exists");
        assert_eq!(loaded.execution_mode, protocol::ExecutionMode::Docker);
        assert_eq!(loaded.container_image.as_deref(), Some("alpine:3.20"));
        assert_eq!(loaded.timeout_secs, Some(60));

        let conn = Connection::open(db.as_path()).expect("open sqlite");
        let spec_json: String = conn
            .query_row(
                "SELECT runtime_spec_json FROM jobs WHERE id = ?1",
                [job.id.to_string()],
                |row| row.get(0),
            )
            .expect("runtime spec row");
        let spec: CanonicalRuntimeSpec =
            serde_json::from_str(&spec_json).expect("parse runtime spec json");
        assert_eq!(
            spec.environment.tier,
            protocol::PlannedEnvironmentTier::Docker
        );
        assert_eq!(
            spec.permissions.tier,
            protocol::PlannedPermissionTier::DockerDefault
        );
        assert!(!spec.permissions.can_access_network);

        let _ = std::fs::remove_file(db);
        let _ = std::fs::remove_dir_all(tasks);
    }

    #[tokio::test]
    async fn insert_job_prefers_runtime_spec_from_context_when_present() {
        let db = std::env::temp_dir().join(format!("orch-{}.sqlite3", Uuid::new_v4()));
        let tasks = std::env::temp_dir().join(format!("tasks-{}", Uuid::new_v4()));
        let svc = OrchestrationService::new(
            workflow_catalog(),
            Arc::new(TaskFileStore::new(tasks.clone())) as Arc<dyn TaskAdapter>,
            db.to_string_lossy().into_owned(),
        );
        svc.init().await.expect("init");

        let now = Utc::now();
        let mut job = Job {
            id: Uuid::new_v4(),
            work_item_id: Uuid::new_v4(),
            step_id: "s1".into(),
            state: JobState::Pending,
            assigned_worker_id: None,
            execution_mode: protocol::ExecutionMode::Raw,
            container_image: None,
            command: "echo".into(),
            args: vec!["hi".into()],
            env: HashMap::new(),
            prompt: None,
            context: serde_json::json!({}),
            repo: None,
            timeout_secs: Some(10),
            max_retries: 0,
            attempt: 0,
            transcript_dir: None,
            result: None,
            lease_expires_at: None,
            created_at: now,
            updated_at: now,
        };
        let runtime = CanonicalRuntimeSpec {
            runtime_version: "v2".to_string(),
            environment: protocol::PlannedEnvironmentSpec {
                tier: protocol::PlannedEnvironmentTier::Docker,
                execution_mode: protocol::ExecutionMode::Docker,
                container_image: Some("alpine:3.20".into()),
                timeout_secs: Some(99),
                memory_limit: None,
                cpu_limit: None,
                network: None,
                network_resources: None,
                filesystem: None,
                file_mounts: vec![],
                toolchains: vec![],
            },
            permissions: protocol::PlannedPermissionSet {
                tier: protocol::PlannedPermissionTier::DockerDefault,
                can_read_workspace: true,
                can_write_workspace: true,
                can_run_shell: true,
                can_access_network: false,
                auto_approve_tools: vec![],
                escalate_tools: vec![],
                kubernetes_access: None,
                aws_access: None,
            },
            approval_rules: None,
            repos: vec![],
        };
        project_runtime_spec_into_job_context(&mut job.context, &runtime);

        svc.insert_job(&job).await.expect("insert job");

        let conn = Connection::open(db.as_path()).expect("open sqlite");
        let spec_json: String = conn
            .query_row(
                "SELECT runtime_spec_json FROM jobs WHERE id = ?1",
                [job.id.to_string()],
                |row| row.get(0),
            )
            .expect("runtime spec row");
        let persisted: CanonicalRuntimeSpec =
            serde_json::from_str(&spec_json).expect("parse runtime spec json");
        assert_eq!(persisted, runtime);

        let _ = std::fs::remove_file(db);
        let _ = std::fs::remove_dir_all(tasks);
    }

    #[tokio::test]
    async fn assign_next_job_uses_runtime_spec_execution_mode_from_context() {
        let db = std::env::temp_dir().join(format!("orch-{}.sqlite3", Uuid::new_v4()));
        let tasks = std::env::temp_dir().join(format!("tasks-{}", Uuid::new_v4()));
        let svc = OrchestrationService::new(
            workflow_catalog(),
            Arc::new(TaskFileStore::new(tasks.clone())) as Arc<dyn TaskAdapter>,
            db.to_string_lossy().into_owned(),
        );
        svc.init().await.expect("init");

        let now = Utc::now();
        let mut job = Job {
            id: Uuid::new_v4(),
            work_item_id: Uuid::new_v4(),
            step_id: "s1".into(),
            state: JobState::Pending,
            assigned_worker_id: None,
            execution_mode: protocol::ExecutionMode::Raw,
            container_image: None,
            command: "echo".into(),
            args: vec!["hi".into()],
            env: HashMap::new(),
            prompt: None,
            context: serde_json::json!({}),
            repo: None,
            timeout_secs: Some(12),
            max_retries: 0,
            attempt: 0,
            transcript_dir: None,
            result: None,
            lease_expires_at: None,
            created_at: now,
            updated_at: now,
        };
        let runtime = CanonicalRuntimeSpec {
            runtime_version: "v2".to_string(),
            environment: protocol::PlannedEnvironmentSpec {
                tier: protocol::PlannedEnvironmentTier::Docker,
                execution_mode: protocol::ExecutionMode::Docker,
                container_image: Some("alpine:3.20".into()),
                timeout_secs: Some(99),
                memory_limit: None,
                cpu_limit: None,
                network: None,
                network_resources: None,
                filesystem: None,
                file_mounts: vec![],
                toolchains: vec![],
            },
            permissions: protocol::PlannedPermissionSet {
                tier: protocol::PlannedPermissionTier::DockerDefault,
                can_read_workspace: true,
                can_write_workspace: true,
                can_run_shell: true,
                can_access_network: false,
                auto_approve_tools: vec![],
                escalate_tools: vec![],
                kubernetes_access: None,
                aws_access: None,
            },
            approval_rules: None,
            repos: vec![],
        };
        project_runtime_spec_into_job_context(&mut job.context, &runtime);

        svc.insert_job(&job).await.expect("insert");

        svc.register_worker(RegisterWorkerRequest {
            id: "docker-worker".into(),
            hostname: "h".into(),
            supported_modes: vec![protocol::ExecutionMode::Docker],
        })
        .await
        .expect("register worker");

        let assigned = svc
            .poll_job(PollJobRequest {
                worker_id: "docker-worker".into(),
            })
            .await
            .expect("poll")
            .job
            .expect("assigned job");

        assert_eq!(assigned.execution_mode, protocol::ExecutionMode::Docker);
        assert_eq!(assigned.container_image.as_deref(), Some("alpine:3.20"));
        assert_eq!(assigned.timeout_secs, Some(99));

        let _ = std::fs::remove_file(db);
        let _ = std::fs::remove_dir_all(tasks);
    }

    #[tokio::test]
    async fn adapter_sync_keeps_task_without_workflow_in_triage() {
        let db = std::env::temp_dir().join(format!("orch-{}.sqlite3", Uuid::new_v4()));
        let tasks = std::env::temp_dir().join(format!("tasks-{}", Uuid::new_v4()));
        let adapter = Arc::new(TaskFileStore::new(tasks.clone())) as Arc<dyn TaskAdapter>;
        let svc = OrchestrationService::new(
            workflow_catalog(),
            Arc::clone(&adapter),
            db.to_string_lossy().into_owned(),
        );
        svc.init().await.expect("init");

        let task = TaskToolRecord {
            id: Uuid::new_v4(),
            title: "triage me".into(),
            description: Some("no workflow yet".into()),
            workflow_id: None,
            triage_status: TaskTriageStatus::Ready,
            pending_human_action: false,
            current_human_gate: None,
            workflow_phase: None,
            blocked_reason: None,
            human_gate_state: None,
            valid_next_events: vec![],
            allowed_actions: vec![],
            context: serde_json::json!({}),
            repo: None,
            updated_at: Utc::now(),
            requested_action: None,
            action_revision: 0,
        };
        adapter
            .persist_projection(&TaskProjection { task: task.clone() })
            .expect("write task");

        svc.sync_tasks_from_adapter().await.expect("sync");
        let items = svc.list_work_items().await.expect("items");
        assert!(items.into_iter().any(|i| i.id == task.id));

        let pulled = adapter.pull_tasks().expect("pull after sync");
        let triaged = pulled
            .into_iter()
            .find(|r| r.external.id == task.id)
            .expect("task exists");
        assert!(matches!(
            triaged.external.triage_status,
            TaskTriageStatus::NeedsWorkflowSelection | TaskTriageStatus::Blocked
        ));
        assert!(matches!(
            triaged.external.workflow_phase.as_deref(),
            Some("triage") | Some("human_gate")
        ));
        assert!(
            triaged
                .external
                .allowed_actions
                .contains(&HumanActionKind::ChooseWorkflow)
        );

        let _ = std::fs::remove_file(db);
        let _ = std::fs::remove_dir_all(tasks);
    }

    #[tokio::test]
    async fn deterministic_workflow_rules_select_single_matching_workflow() {
        let db = std::env::temp_dir().join(format!("orch-{}.sqlite3", Uuid::new_v4()));
        let tasks = std::env::temp_dir().join(format!("tasks-{}", Uuid::new_v4()));
        let adapter = Arc::new(TaskFileStore::new(tasks.clone())) as Arc<dyn TaskAdapter>;
        let svc = OrchestrationService::new(
            multi_workflow_catalog(),
            Arc::clone(&adapter),
            db.to_string_lossy().into_owned(),
        );
        svc.init().await.expect("init");

        let task = TaskToolRecord {
            id: Uuid::new_v4(),
            title: "route me".into(),
            description: None,
            workflow_id: None,
            triage_status: TaskTriageStatus::Ready,
            pending_human_action: false,
            current_human_gate: None,
            workflow_phase: None,
            blocked_reason: None,
            human_gate_state: None,
            valid_next_events: vec![],
            allowed_actions: vec![],
            context: serde_json::json!({
                "priority": "high",
                "labels": ["ops"],
                "workflow_rules": {
                    "by_priority": {"high": "wf_alpha"},
                    "by_label": {"ops": "wf_alpha"}
                }
            }),
            repo: None,
            updated_at: Utc::now(),
            requested_action: None,
            action_revision: 0,
        };
        adapter
            .persist_projection(&TaskProjection { task: task.clone() })
            .expect("persist task");

        svc.sync_tasks_from_adapter().await.expect("sync");
        let item = svc
            .get_work_item(task.id)
            .await
            .expect("get")
            .expect("exists");
        assert_eq!(item.workflow_id, "wf_alpha");
        assert_ne!(item.workflow_id, "__triage__");

        let _ = std::fs::remove_file(db);
        let _ = std::fs::remove_dir_all(tasks);
    }

    #[tokio::test]
    async fn ambiguous_workflow_rules_route_to_triage_with_explicit_reason() {
        let db = std::env::temp_dir().join(format!("orch-{}.sqlite3", Uuid::new_v4()));
        let tasks = std::env::temp_dir().join(format!("tasks-{}", Uuid::new_v4()));
        let adapter = Arc::new(TaskFileStore::new(tasks.clone())) as Arc<dyn TaskAdapter>;
        let svc = OrchestrationService::new(
            multi_workflow_catalog(),
            Arc::clone(&adapter),
            db.to_string_lossy().into_owned(),
        );
        svc.init().await.expect("init");

        let task = TaskToolRecord {
            id: Uuid::new_v4(),
            title: "ambiguous".into(),
            description: None,
            workflow_id: None,
            triage_status: TaskTriageStatus::Ready,
            pending_human_action: false,
            current_human_gate: None,
            workflow_phase: None,
            blocked_reason: None,
            human_gate_state: None,
            valid_next_events: vec![],
            allowed_actions: vec![],
            context: serde_json::json!({
                "priority": "high",
                "labels": ["ops"],
                "workflow_rules": {
                    "by_priority": {"high": "wf_alpha"},
                    "by_label": {"ops": "wf_beta"}
                }
            }),
            repo: None,
            updated_at: Utc::now(),
            requested_action: None,
            action_revision: 0,
        };
        adapter
            .persist_projection(&TaskProjection { task: task.clone() })
            .expect("persist task");

        svc.sync_tasks_from_adapter().await.expect("sync");
        let item = svc
            .get_work_item(task.id)
            .await
            .expect("get")
            .expect("exists");
        assert_eq!(item.workflow_id, "__triage__");
        assert_eq!(
            item.context
                .get("triage_reason")
                .and_then(serde_json::Value::as_str),
            Some("multiple workflow rules matched; choose one of: wf_alpha, wf_beta")
        );

        let projected = adapter.pull_tasks().expect("pull tasks");
        let rec = projected
            .into_iter()
            .find(|r| r.external.id == task.id)
            .expect("projected task");
        assert!(
            rec.external
                .allowed_actions
                .contains(&HumanActionKind::ChooseWorkflow)
        );

        let _ = std::fs::remove_file(db);
        let _ = std::fs::remove_dir_all(tasks);
    }

    #[tokio::test]
    async fn adapter_sync_applies_approve_action_to_work_item() {
        let db = std::env::temp_dir().join(format!("orch-{}.sqlite3", Uuid::new_v4()));
        let tasks = std::env::temp_dir().join(format!("tasks-{}", Uuid::new_v4()));
        let adapter = Arc::new(TaskFileStore::new(tasks.clone())) as Arc<dyn TaskAdapter>;
        let svc = OrchestrationService::new(
            reactive_workflow_catalog(),
            Arc::clone(&adapter),
            db.to_string_lossy().into_owned(),
        );
        svc.init().await.expect("init");

        let mut item = svc
            .create_work_item(CreateWorkItemRequest {
                workflow_id: "reactive_wf".into(),
                session_id: None,
                context: serde_json::json!({"human_gate": true}),
                repo: None,
            })
            .await
            .expect("create");

        // Move to human gate state before applying human approve action.
        svc.fire_event_trusted_internal(
            item.id,
            FireEventRequest {
                event: WorkflowEventKind::JobSucceeded,
                job_id: None,
                reason: Some("agent done".into()),
            },
        )
        .await
        .expect("move to review");
        item = svc
            .get_work_item(item.id)
            .await
            .expect("get")
            .expect("exists");

        let mut task = task_from_work_item(&item, None);
        task.requested_action = Some(crate::server::task_adapter::TaskHumanAction {
            kind: HumanActionKind::Approve,
            workflow_id: None,
            reason: Some("approved from task tool".into()),
        });
        task.action_revision = 1;
        task.updated_at = Utc::now() + chrono::TimeDelta::seconds(2);
        adapter
            .persist_projection(&TaskProjection { task })
            .expect("persist action");

        svc.sync_tasks_from_adapter().await.expect("sync action");
        let updated = svc
            .get_work_item(item.id)
            .await
            .expect("get")
            .expect("exists");
        assert_ne!(updated.current_step, item.current_step);
        assert_eq!(updated.current_step, "done");
        assert!(
            !updated.human_gate,
            "human_gate should be cleared on terminal state entry"
        );

        let _ = std::fs::remove_file(db);
        let _ = std::fs::remove_dir_all(tasks);
    }

    #[tokio::test]
    async fn triage_choose_workflow_then_submit_starts_executable_path() {
        let db = std::env::temp_dir().join(format!("orch-{}.sqlite3", Uuid::new_v4()));
        let tasks = std::env::temp_dir().join(format!("tasks-{}", Uuid::new_v4()));
        let adapter = Arc::new(TaskFileStore::new(tasks.clone())) as Arc<dyn TaskAdapter>;
        let svc = OrchestrationService::new(
            workflow_catalog(),
            Arc::clone(&adapter),
            db.to_string_lossy().into_owned(),
        );
        svc.init().await.expect("init");

        let triage_task = TaskToolRecord {
            id: Uuid::new_v4(),
            title: "needs selection".into(),
            description: Some("pick workflow".into()),
            workflow_id: None,
            triage_status: TaskTriageStatus::NeedsWorkflowSelection,
            pending_human_action: true,
            current_human_gate: Some("workflow_selection".into()),
            workflow_phase: Some("triage".into()),
            blocked_reason: Some("workflow selection required".into()),
            human_gate_state: Some(TaskHumanGateState::NeedsWorkflowSelection),
            valid_next_events: vec![],
            allowed_actions: vec![HumanActionKind::ChooseWorkflow, HumanActionKind::Cancel],
            context: serde_json::json!({}),
            repo: None,
            updated_at: Utc::now(),
            requested_action: None,
            action_revision: 0,
        };
        adapter
            .persist_projection(&TaskProjection {
                task: triage_task.clone(),
            })
            .expect("persist triage task");

        svc.sync_tasks_from_adapter().await.expect("initial sync");

        let mut choose = triage_task.clone();
        choose.requested_action = Some(crate::server::task_adapter::TaskHumanAction {
            kind: HumanActionKind::ChooseWorkflow,
            workflow_id: Some("wf".into()),
            reason: Some("selected".into()),
        });
        choose.action_revision = 1;
        choose.updated_at = Utc::now() + chrono::TimeDelta::seconds(1);
        adapter
            .persist_projection(&TaskProjection { task: choose })
            .expect("persist choose workflow action");

        svc.sync_tasks_from_adapter().await.expect("choose sync");
        let selected = svc
            .get_work_item(triage_task.id)
            .await
            .expect("get")
            .expect("exists");
        assert_eq!(selected.workflow_id, "wf");
        assert_ne!(selected.current_step, "triage");
        assert!(
            !svc.valid_next_events_for_work_item(&selected)
                .contains(&WorkflowEventKind::WorkflowChosen)
        );

        let mut submit = task_from_work_item(&selected, None);
        submit.requested_action = Some(crate::server::task_adapter::TaskHumanAction {
            kind: HumanActionKind::Submit,
            workflow_id: None,
            reason: Some("start it".into()),
        });
        submit.action_revision = 2;
        submit.updated_at = Utc::now() + chrono::TimeDelta::seconds(2);
        adapter
            .persist_projection(&TaskProjection { task: submit })
            .expect("persist submit action");

        svc.sync_tasks_from_adapter().await.expect("submit sync");
        let jobs = svc.list_jobs().await.expect("jobs");
        assert!(jobs.iter().any(|j| j.work_item_id == triage_task.id));

        let _ = std::fs::remove_file(db);
        let _ = std::fs::remove_dir_all(tasks);
    }

    #[tokio::test]
    async fn submit_on_triage_item_returns_workflow_invalid() {
        let db = std::env::temp_dir().join(format!("orch-{}.sqlite3", Uuid::new_v4()));
        let tasks = std::env::temp_dir().join(format!("tasks-{}", Uuid::new_v4()));
        let adapter = Arc::new(TaskFileStore::new(tasks.clone())) as Arc<dyn TaskAdapter>;
        let svc = OrchestrationService::new(
            workflow_catalog(),
            Arc::clone(&adapter),
            db.to_string_lossy().into_owned(),
        );
        svc.init().await.expect("init");

        let triage_task = TaskToolRecord {
            id: Uuid::new_v4(),
            title: "needs selection".into(),
            description: Some("pick workflow".into()),
            workflow_id: None,
            triage_status: TaskTriageStatus::NeedsWorkflowSelection,
            pending_human_action: true,
            current_human_gate: Some("workflow_selection".into()),
            workflow_phase: Some("triage".into()),
            blocked_reason: Some("workflow selection required".into()),
            human_gate_state: Some(TaskHumanGateState::NeedsWorkflowSelection),
            valid_next_events: vec![],
            allowed_actions: vec![HumanActionKind::ChooseWorkflow, HumanActionKind::Cancel],
            context: serde_json::json!({}),
            repo: None,
            updated_at: Utc::now(),
            requested_action: None,
            action_revision: 0,
        };
        adapter
            .persist_projection(&TaskProjection {
                task: triage_task.clone(),
            })
            .expect("persist triage task");
        svc.sync_tasks_from_adapter().await.expect("initial sync");

        let mut submit = triage_task.clone();
        submit.requested_action = Some(crate::server::task_adapter::TaskHumanAction {
            kind: HumanActionKind::Submit,
            workflow_id: None,
            reason: Some("start without workflow".into()),
        });
        submit.action_revision = 1;
        submit.updated_at = Utc::now() + chrono::TimeDelta::seconds(1);
        adapter
            .persist_projection(&TaskProjection { task: submit })
            .expect("persist submit");

        let err = svc
            .sync_tasks_from_adapter()
            .await
            .expect_err("submit on triage should fail");
        assert!(matches!(err, AppError::WorkflowInvalid(_)));

        let _ = std::fs::remove_file(db);
        let _ = std::fs::remove_dir_all(tasks);
    }

    #[tokio::test]
    async fn work_item_persists_first_class_task_fields() {
        let db = std::env::temp_dir().join(format!("orch-{}.sqlite3", Uuid::new_v4()));
        let tasks = std::env::temp_dir().join(format!("tasks-{}", Uuid::new_v4()));
        let adapter = Arc::new(TaskFileStore::new(tasks.clone())) as Arc<dyn TaskAdapter>;
        let svc = OrchestrationService::new(
            workflow_catalog(),
            Arc::clone(&adapter),
            db.to_string_lossy().into_owned(),
        );
        svc.init().await.expect("init");

        let task = TaskToolRecord {
            id: Uuid::new_v4(),
            title: "First class title".into(),
            description: Some("desc".into()),
            workflow_id: Some("wf".into()),
            triage_status: TaskTriageStatus::Ready,
            pending_human_action: false,
            current_human_gate: None,
            workflow_phase: None,
            blocked_reason: None,
            human_gate_state: None,
            valid_next_events: vec![],
            allowed_actions: vec![],
            context: serde_json::json!({"labels": ["ops", "mvp"], "priority": "high"}),
            repo: None,
            updated_at: Utc::now(),
            requested_action: None,
            action_revision: 0,
        };
        adapter
            .persist_projection(&TaskProjection { task: task.clone() })
            .expect("persist task");

        svc.sync_tasks_from_adapter().await.expect("sync");
        let item = svc
            .get_work_item(task.id)
            .await
            .expect("get")
            .expect("exists");
        assert_eq!(item.title.as_deref(), Some("First class title"));
        assert_eq!(item.description.as_deref(), Some("desc"));
        assert_eq!(item.priority.as_deref(), Some("high"));
        assert_eq!(item.labels, vec!["ops".to_string(), "mvp".to_string()]);
        assert_eq!(item.external_task_source.as_deref(), Some("file"));
        assert!(item.projected_state.is_some());

        let _ = std::fs::remove_file(db);
        let _ = std::fs::remove_dir_all(tasks);
    }

    #[tokio::test]
    async fn reactive_human_state_sets_human_gate_flag() {
        let db = std::env::temp_dir().join(format!("orch-{}.sqlite3", Uuid::new_v4()));
        let tasks = std::env::temp_dir().join(format!("tasks-{}", Uuid::new_v4()));
        let adapter = Arc::new(TaskFileStore::new(tasks.clone())) as Arc<dyn TaskAdapter>;
        let svc = OrchestrationService::new(
            reactive_workflow_catalog(),
            Arc::clone(&adapter),
            db.to_string_lossy().into_owned(),
        );
        svc.init().await.expect("init");

        let item = svc
            .create_work_item(CreateWorkItemRequest {
                workflow_id: "reactive_wf".into(),
                session_id: None,
                context: serde_json::json!({}),
                repo: None,
            })
            .await
            .expect("create");

        svc.fire_event_trusted_internal(
            item.id,
            FireEventRequest {
                event: WorkflowEventKind::JobSucceeded,
                job_id: None,
                reason: Some("agent done".into()),
            },
        )
        .await
        .expect("to human state");

        let review = svc
            .get_work_item(item.id)
            .await
            .expect("get")
            .expect("exists");
        assert_eq!(review.current_step, "review");
        assert!(review.human_gate);

        let _ = std::fs::remove_file(db);
        let _ = std::fs::remove_dir_all(tasks);
    }

    #[tokio::test]
    async fn github_webhook_matches_pr_from_terminal_originating_job_context() {
        let db = std::env::temp_dir().join(format!("orch-{}.sqlite3", Uuid::new_v4()));
        let tasks = std::env::temp_dir().join(format!("tasks-{}", Uuid::new_v4()));
        let adapter = Arc::new(TaskFileStore::new(tasks.clone())) as Arc<dyn TaskAdapter>;
        let svc = OrchestrationService::new(
            workflow_catalog(),
            Arc::clone(&adapter),
            db.to_string_lossy().into_owned(),
        );
        svc.init().await.expect("init");

        let mut item = svc
            .create_work_item(CreateWorkItemRequest {
                workflow_id: "wf".into(),
                session_id: None,
                context: serde_json::json!({
                    "job_results": {
                        "create_pr": {
                            "github_pr": {
                                "url": "https://github.com/reddit/agent-hub/pull/42"
                            }
                        }
                    }
                }),
                repo: None,
            })
            .await
            .expect("create");
        item.state = WorkItemState::Running;
        svc.update_work_item(&item).await.expect("update item");

        let mut headers = HeaderMap::new();
        headers.insert("x-github-event", HeaderValue::from_static("pull_request"));

        let state = super::super::AppState {
            config: Arc::new(super::super::config::ServerConfig {
                auth_mode: super::super::config::AuthMode::None,
                tokens: vec![],
                listen_addr: "127.0.0.1:0".into(),
                presence_ttl_secs: 120,
                session_ttl_secs: 120,
                notification_delay_secs: 0,
                approval_mode: super::super::config::ApprovalFeatureMode::Disabled,
                base_url: None,
                default_approval_mode: super::super::sessions::SessionApprovalMode::Remote,
                workflows_dir: "workflows".into(),
                task_dir: "tasks".into(),
                task_adapter: "file".into(),
                vikunja_base_url: None,
                vikunja_token: None,
                vikunja_project_id: None,
                vikunja_label_prefix: None,
                orchestration_db_path: ":memory:".into(),
                grpc_listen_addr: None,
                github_webhook_secret: None,
            }),
            presence: Arc::new(super::super::presence::Presence::new(120)),
            sessions: Arc::new(super::super::sessions::SessionRegistry::new(120)),
            notifier: Arc::new(super::super::notifier::NullNotifier),
            notify_config: Arc::new(tokio::sync::RwLock::new(
                protocol::NotifyConfig::with_delay(0),
            )),
            pending: Arc::new(super::super::hooks::PendingNotifications::new()),
            approvals: Arc::new(super::super::approvals::ApprovalRegistry::new()),
            questions: Arc::new(super::super::questions::QuestionRegistry::new()),
            orchestration: Arc::new(svc.clone()),
            job_events: Arc::new(crate::server::job_events::JobEventHub::new()),
            oauth: Arc::new(None),
        };

        let resp = github_webhook::<super::super::notifier::NullNotifier>(
            State(state),
            headers,
            axum::body::Bytes::from(
                r#"{"action":"closed","pull_request":{"html_url":"https://github.com/reddit/agent-hub/pull/42","merged":true}}"#,
            ),
        )
        .await
        .expect("webhook");
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("read body");
        let json: serde_json::Value = serde_json::from_slice(&body).expect("json body");
        assert_eq!(json.get("fired_events"), Some(&serde_json::json!(1)));

        let _ = std::fs::remove_file(db);
        let _ = std::fs::remove_dir_all(tasks);
    }

    #[tokio::test]
    async fn github_webhook_skips_terminal_items_even_if_pr_matches() {
        let db = std::env::temp_dir().join(format!("orch-{}.sqlite3", Uuid::new_v4()));
        let tasks = std::env::temp_dir().join(format!("tasks-{}", Uuid::new_v4()));
        let adapter = Arc::new(TaskFileStore::new(tasks.clone())) as Arc<dyn TaskAdapter>;
        let svc = OrchestrationService::new(
            workflow_catalog(),
            Arc::clone(&adapter),
            db.to_string_lossy().into_owned(),
        );
        svc.init().await.expect("init");

        let mut item = svc
            .create_work_item(CreateWorkItemRequest {
                workflow_id: "wf".into(),
                session_id: None,
                context: serde_json::json!({
                    "job_results": {
                        "create_pr": {
                            "github_pr": {
                                "url": "https://github.com/reddit/agent-hub/pull/43"
                            }
                        }
                    }
                }),
                repo: None,
            })
            .await
            .expect("create");
        item.state = WorkItemState::Succeeded;
        svc.update_work_item(&item).await.expect("update item");

        let mut headers = HeaderMap::new();
        headers.insert("x-github-event", HeaderValue::from_static("pull_request"));

        let state = super::super::AppState {
            config: Arc::new(super::super::config::ServerConfig {
                auth_mode: super::super::config::AuthMode::None,
                tokens: vec![],
                listen_addr: "127.0.0.1:0".into(),
                presence_ttl_secs: 120,
                session_ttl_secs: 120,
                notification_delay_secs: 0,
                approval_mode: super::super::config::ApprovalFeatureMode::Disabled,
                base_url: None,
                default_approval_mode: super::super::sessions::SessionApprovalMode::Remote,
                workflows_dir: "workflows".into(),
                task_dir: "tasks".into(),
                task_adapter: "file".into(),
                vikunja_base_url: None,
                vikunja_token: None,
                vikunja_project_id: None,
                vikunja_label_prefix: None,
                orchestration_db_path: ":memory:".into(),
                grpc_listen_addr: None,
                github_webhook_secret: None,
            }),
            presence: Arc::new(super::super::presence::Presence::new(120)),
            sessions: Arc::new(super::super::sessions::SessionRegistry::new(120)),
            notifier: Arc::new(super::super::notifier::NullNotifier),
            notify_config: Arc::new(tokio::sync::RwLock::new(
                protocol::NotifyConfig::with_delay(0),
            )),
            pending: Arc::new(super::super::hooks::PendingNotifications::new()),
            approvals: Arc::new(super::super::approvals::ApprovalRegistry::new()),
            questions: Arc::new(super::super::questions::QuestionRegistry::new()),
            orchestration: Arc::new(svc.clone()),
            job_events: Arc::new(crate::server::job_events::JobEventHub::new()),
            oauth: Arc::new(None),
        };

        let resp = github_webhook::<super::super::notifier::NullNotifier>(
            State(state),
            headers,
            axum::body::Bytes::from(
                r#"{"action":"closed","pull_request":{"html_url":"https://github.com/reddit/agent-hub/pull/43","merged":true}}"#,
            ),
        )
        .await
        .expect("webhook");
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("read body");
        let json: serde_json::Value = serde_json::from_slice(&body).expect("json body");
        assert_eq!(json.get("matched_jobs"), Some(&serde_json::json!(0)));
        assert_eq!(json.get("fired_events"), Some(&serde_json::json!(0)));

        let _ = std::fs::remove_file(db);
        let _ = std::fs::remove_dir_all(tasks);
    }

    #[tokio::test]
    async fn triage_needs_triage_flag_persists_manual_triage_reason() {
        let db = std::env::temp_dir().join(format!("orch-{}.sqlite3", Uuid::new_v4()));
        let tasks = std::env::temp_dir().join(format!("tasks-{}", Uuid::new_v4()));
        let adapter = Arc::new(TaskFileStore::new(tasks.clone())) as Arc<dyn TaskAdapter>;
        let svc = OrchestrationService::new(
            workflow_catalog(),
            Arc::clone(&adapter),
            db.to_string_lossy().into_owned(),
        );
        svc.init().await.expect("init");

        let triage_task = TaskToolRecord {
            id: Uuid::new_v4(),
            title: "manual triage".into(),
            description: Some("needs manual triage".into()),
            workflow_id: None,
            triage_status: TaskTriageStatus::NeedsWorkflowSelection,
            pending_human_action: true,
            current_human_gate: Some("workflow_selection".into()),
            workflow_phase: Some("triage".into()),
            blocked_reason: Some("manual triage required".into()),
            human_gate_state: Some(TaskHumanGateState::NeedsWorkflowSelection),
            valid_next_events: vec![],
            allowed_actions: vec![HumanActionKind::Cancel],
            context: serde_json::json!({"needs_triage": true, "triage_reason": "manual triage required"}),
            repo: None,
            updated_at: Utc::now(),
            requested_action: None,
            action_revision: 0,
        };
        adapter
            .persist_projection(&TaskProjection {
                task: triage_task.clone(),
            })
            .expect("persist triage task");

        svc.sync_tasks_from_adapter().await.expect("sync");
        let projected = adapter.pull_tasks().expect("pull tasks");
        let rec = projected
            .into_iter()
            .find(|r| r.external.id == triage_task.id)
            .expect("task exists");
        assert_eq!(
            rec.external.blocked_reason.as_deref(),
            Some("manual triage required")
        );
        assert!(
            !rec.external
                .allowed_actions
                .contains(&HumanActionKind::ChooseWorkflow)
        );

        let _ = std::fs::remove_file(db);
        let _ = std::fs::remove_dir_all(tasks);
    }

    #[tokio::test]
    async fn triage_needs_triage_label_is_manual_gate_authority() {
        let db = std::env::temp_dir().join(format!("orch-{}.sqlite3", Uuid::new_v4()));
        let tasks = std::env::temp_dir().join(format!("tasks-{}", Uuid::new_v4()));
        let adapter = Arc::new(TaskFileStore::new(tasks.clone())) as Arc<dyn TaskAdapter>;
        let svc = OrchestrationService::new(
            workflow_catalog(),
            Arc::clone(&adapter),
            db.to_string_lossy().into_owned(),
        );
        svc.init().await.expect("init");

        let triage_task = TaskToolRecord {
            id: Uuid::new_v4(),
            title: "manual triage by label".into(),
            description: Some("blocked by needs-triage label".into()),
            workflow_id: Some("wf".into()),
            triage_status: TaskTriageStatus::NeedsWorkflowSelection,
            pending_human_action: true,
            current_human_gate: Some("workflow_selection".into()),
            workflow_phase: Some("triage".into()),
            blocked_reason: Some("manual triage required".into()),
            human_gate_state: Some(TaskHumanGateState::NeedsWorkflowSelection),
            valid_next_events: vec![],
            allowed_actions: vec![HumanActionKind::Cancel],
            context: serde_json::json!({"labels": ["needs-triage"], "triage_reason": "manual triage required"}),
            repo: None,
            updated_at: Utc::now(),
            requested_action: None,
            action_revision: 0,
        };
        adapter
            .persist_projection(&TaskProjection {
                task: triage_task.clone(),
            })
            .expect("persist triage task");

        svc.sync_tasks_from_adapter().await.expect("sync");

        let triage_item = svc
            .get_work_item(triage_task.id)
            .await
            .expect("get")
            .expect("exists");
        assert_eq!(triage_item.workflow_id, "__triage__");

        let triage_events = svc.valid_next_events_for_work_item(&triage_item);
        assert_eq!(triage_events, vec![WorkflowEventKind::Cancel]);

        let choose_err = svc
            .choose_workflow(triage_item.id, "wf".to_string())
            .await
            .expect_err("manual triage gate should block choose_workflow");
        assert!(matches!(choose_err, AppError::WorkflowInvalid(_)));
        assert!(
            choose_err
                .to_string()
                .contains("cannot choose workflow while needs-triage is active")
        );

        let projected = adapter.pull_tasks().expect("pull tasks");
        let rec = projected
            .into_iter()
            .find(|r| r.external.id == triage_task.id)
            .expect("projected task");
        assert!(
            !rec.external
                .allowed_actions
                .contains(&HumanActionKind::ChooseWorkflow)
        );

        let _ = std::fs::remove_file(db);
        let _ = std::fs::remove_dir_all(tasks);
    }

    #[tokio::test]
    async fn adapter_choose_workflow_action_is_blocked_while_needs_triage_label_active() {
        let db = std::env::temp_dir().join(format!("orch-{}.sqlite3", Uuid::new_v4()));
        let tasks = std::env::temp_dir().join(format!("tasks-{}", Uuid::new_v4()));
        let adapter = Arc::new(TaskFileStore::new(tasks.clone())) as Arc<dyn TaskAdapter>;
        let svc = OrchestrationService::new(
            workflow_catalog(),
            Arc::clone(&adapter),
            db.to_string_lossy().into_owned(),
        );
        svc.init().await.expect("init");

        let task_id = Uuid::new_v4();
        let triage_task = TaskToolRecord {
            id: task_id,
            title: "manual triage choose action blocked".into(),
            description: None,
            workflow_id: Some("wf".into()),
            triage_status: TaskTriageStatus::NeedsWorkflowSelection,
            pending_human_action: true,
            current_human_gate: Some("workflow_selection".into()),
            workflow_phase: Some("triage".into()),
            blocked_reason: Some("manual triage required".into()),
            human_gate_state: Some(TaskHumanGateState::NeedsWorkflowSelection),
            valid_next_events: vec![],
            allowed_actions: vec![HumanActionKind::Cancel],
            context: serde_json::json!({"labels": ["needs-triage"]}),
            repo: None,
            updated_at: Utc::now(),
            requested_action: None,
            action_revision: 0,
        };
        adapter
            .persist_projection(&TaskProjection {
                task: triage_task.clone(),
            })
            .expect("persist initial");
        svc.sync_tasks_from_adapter().await.expect("sync initial");

        let action_task = TaskToolRecord {
            updated_at: triage_task.updated_at + chrono::TimeDelta::seconds(1),
            requested_action: Some(crate::server::task_adapter::TaskHumanAction {
                kind: HumanActionKind::ChooseWorkflow,
                workflow_id: Some("wf".into()),
                reason: None,
            }),
            action_revision: 1,
            ..triage_task
        };
        adapter
            .persist_projection(&TaskProjection { task: action_task })
            .expect("persist action task");
        svc.sync_tasks_from_adapter()
            .await
            .expect("sync action task");

        let item = svc
            .get_work_item(task_id)
            .await
            .expect("get item")
            .expect("exists");
        assert_eq!(item.workflow_id, "__triage__");

        let _ = std::fs::remove_file(db);
        let _ = std::fs::remove_dir_all(tasks);
    }

    #[tokio::test]
    async fn adapter_update_projection_removing_needs_triage_label_unlocks_workflow_selection() {
        let db = std::env::temp_dir().join(format!("orch-{}.sqlite3", Uuid::new_v4()));
        let tasks = std::env::temp_dir().join(format!("tasks-{}", Uuid::new_v4()));
        let adapter = Arc::new(TaskFileStore::new(tasks.clone())) as Arc<dyn TaskAdapter>;
        let svc = OrchestrationService::new(
            workflow_catalog(),
            Arc::clone(&adapter),
            db.to_string_lossy().into_owned(),
        );
        svc.init().await.expect("init");

        let base_time = Utc::now();
        let triage_task_id = Uuid::new_v4();
        let triage_task = TaskToolRecord {
            id: triage_task_id,
            title: "unlock after label removed".into(),
            description: Some("starts blocked by label".into()),
            workflow_id: Some("wf".into()),
            triage_status: TaskTriageStatus::NeedsWorkflowSelection,
            pending_human_action: true,
            current_human_gate: Some("workflow_selection".into()),
            workflow_phase: Some("triage".into()),
            blocked_reason: Some("manual triage required".into()),
            human_gate_state: Some(TaskHumanGateState::NeedsWorkflowSelection),
            valid_next_events: vec![],
            allowed_actions: vec![HumanActionKind::Cancel],
            context: serde_json::json!({"labels": ["needs-triage"], "triage_reason": "manual triage required"}),
            repo: None,
            updated_at: base_time,
            requested_action: None,
            action_revision: 0,
        };
        adapter
            .persist_projection(&TaskProjection {
                task: triage_task.clone(),
            })
            .expect("persist triage task");
        svc.sync_tasks_from_adapter().await.expect("sync blocked");

        let blocked = svc
            .get_work_item(triage_task_id)
            .await
            .expect("get")
            .expect("exists");
        assert_eq!(blocked.workflow_id, "__triage__");

        let unlocked_task = TaskToolRecord {
            context: serde_json::json!({"labels": []}),
            updated_at: base_time + chrono::TimeDelta::seconds(5),
            ..triage_task
        };
        adapter
            .persist_projection(&TaskProjection {
                task: unlocked_task,
            })
            .expect("persist unlocked task");
        svc.sync_tasks_from_adapter().await.expect("sync unlocked");

        let unlocked = svc
            .get_work_item(triage_task_id)
            .await
            .expect("get")
            .expect("exists");
        assert_eq!(unlocked.workflow_id, "wf");
        assert_ne!(unlocked.current_step, "triage");
        let jobs = svc
            .list_jobs_for_work_item(triage_task_id)
            .await
            .expect("jobs");
        assert!(!jobs.is_empty());

        let _ = std::fs::remove_file(db);
        let _ = std::fs::remove_dir_all(tasks);
    }

    #[tokio::test]
    async fn valid_next_events_for_reactive_item_follow_state_transitions() {
        let db = std::env::temp_dir().join(format!("orch-{}.sqlite3", Uuid::new_v4()));
        let tasks = std::env::temp_dir().join(format!("tasks-{}", Uuid::new_v4()));
        let adapter = Arc::new(TaskFileStore::new(tasks.clone())) as Arc<dyn TaskAdapter>;
        let svc = OrchestrationService::new(
            reactive_workflow_catalog(),
            Arc::clone(&adapter),
            db.to_string_lossy().into_owned(),
        );
        svc.init().await.expect("init");

        let mut item = svc
            .create_work_item(CreateWorkItemRequest {
                workflow_id: "reactive_wf".into(),
                session_id: None,
                context: serde_json::json!({}),
                repo: None,
            })
            .await
            .expect("create");

        let initial_events = svc.valid_next_events_for_work_item(&item);
        assert!(initial_events.contains(&WorkflowEventKind::Cancel));
        assert!(!initial_events.contains(&WorkflowEventKind::HumanApproved));
        assert!(!initial_events.contains(&WorkflowEventKind::JobSucceeded));
        assert!(!initial_events.contains(&WorkflowEventKind::JobFailed));

        let initial_internal = svc.valid_next_events_for_work_item_internal(&item);
        assert!(initial_internal.contains(&WorkflowEventKind::JobSucceeded));
        assert!(initial_internal.contains(&WorkflowEventKind::JobFailed));
        assert!(initial_internal.contains(&WorkflowEventKind::Cancel));

        svc.fire_event_trusted_internal(
            item.id,
            FireEventRequest {
                event: WorkflowEventKind::JobSucceeded,
                job_id: None,
                reason: Some("advance to review".into()),
            },
        )
        .await
        .expect("fire");
        item = svc
            .get_work_item(item.id)
            .await
            .expect("get")
            .expect("exists");
        let review_events = svc.valid_next_events_for_work_item(&item);
        assert!(review_events.contains(&WorkflowEventKind::HumanApproved));
        assert!(review_events.contains(&WorkflowEventKind::HumanChangesRequested));
        assert!(review_events.contains(&WorkflowEventKind::Cancel));
        assert!(!review_events.contains(&WorkflowEventKind::JobSucceeded));

        let review_internal = svc.valid_next_events_for_work_item_internal(&item);
        assert!(review_internal.contains(&WorkflowEventKind::HumanApproved));
        assert!(review_internal.contains(&WorkflowEventKind::HumanChangesRequested));
        assert!(review_internal.contains(&WorkflowEventKind::Cancel));

        let _ = std::fs::remove_file(db);
        let _ = std::fs::remove_dir_all(tasks);
    }

    #[tokio::test]
    async fn reactive_agent_job_context_projects_internal_transition_events() {
        let db = std::env::temp_dir().join(format!("orch-{}.sqlite3", Uuid::new_v4()));
        let tasks = std::env::temp_dir().join(format!("tasks-{}", Uuid::new_v4()));
        let svc = OrchestrationService::new(
            reactive_workflow_catalog(),
            Arc::new(TaskFileStore::new(tasks.clone())) as Arc<dyn TaskAdapter>,
            db.to_string_lossy().into_owned(),
        );
        svc.init().await.expect("init");

        let item = svc
            .create_work_item(CreateWorkItemRequest {
                workflow_id: "reactive_wf".into(),
                session_id: None,
                context: serde_json::json!({}),
                repo: None,
            })
            .await
            .expect("create item");

        let job = svc
            .list_jobs_for_work_item(item.id)
            .await
            .expect("jobs")
            .into_iter()
            .find(|job| job.step_id == "run_agent")
            .expect("run_agent job");

        assert_eq!(
            job.context["workflow"]["inputs"]["valid_next_events"],
            serde_json::json!(["job_succeeded", "job_failed", "cancel"])
        );

        let _ = std::fs::remove_file(db);
        let _ = std::fs::remove_dir_all(tasks);
    }

    #[tokio::test]
    async fn reactive_runtime_spec_includes_workflow_and_context_candidate_repos() {
        let db = std::env::temp_dir().join(format!("orch-{}.sqlite3", Uuid::new_v4()));
        let tasks = std::env::temp_dir().join(format!("tasks-{}", Uuid::new_v4()));
        let svc = OrchestrationService::new(
            reactive_candidate_repo_catalog(),
            Arc::new(TaskFileStore::new(tasks.clone())) as Arc<dyn TaskAdapter>,
            db.to_string_lossy().into_owned(),
        );
        svc.init().await.expect("init");

        let item = svc
            .create_work_item(CreateWorkItemRequest {
                workflow_id: "reactive_candidates".into(),
                session_id: None,
                context: serde_json::json!({
                    "repo_catalog": [
                        { "repo_url": "https://github.com/reddit/opencode", "base_ref": "origin/main" },
                        { "repo_url": "https://github.com/reddit/Dippy", "base_ref": "origin/main" }
                    ]
                }),
                repo: Some(protocol::RepoSource {
                    repo_url: "https://github.com/reddit/agent-hub".to_string(),
                    base_ref: "origin/main".to_string(),
                    branch_name: Some("agent-hub/test-branch".to_string()),
                }),
            })
            .await
            .expect("create item");

        let job = svc
            .list_jobs_for_work_item(item.id)
            .await
            .expect("jobs")
            .into_iter()
            .find(|job| job.step_id == "run_agent")
            .expect("run_agent job");
        let runtime = canonical_runtime_spec_for_job_effective(&job);
        let repo_urls = runtime
            .repos
            .iter()
            .map(|repo| repo.repo_url.clone())
            .collect::<Vec<_>>();

        assert_eq!(repo_urls.len(), 3);
        assert_eq!(repo_urls[0], "https://github.com/reddit/agent-hub");
        assert_eq!(repo_urls[1], "https://github.com/reddit/opencode");
        assert_eq!(repo_urls[2], "https://github.com/reddit/Dippy");
        assert!(!runtime.repos[0].read_only);
        assert!(runtime.repos[1].read_only);
        assert!(runtime.repos[2].read_only);

        let _ = std::fs::remove_file(db);
        let _ = std::fs::remove_dir_all(tasks);
    }

    #[tokio::test]
    async fn reactive_runtime_spec_candidate_repos_dedupes_by_repo_url() {
        let db = std::env::temp_dir().join(format!("orch-{}.sqlite3", Uuid::new_v4()));
        let tasks = std::env::temp_dir().join(format!("tasks-{}", Uuid::new_v4()));
        let svc = OrchestrationService::new(
            reactive_candidate_repo_catalog(),
            Arc::new(TaskFileStore::new(tasks.clone())) as Arc<dyn TaskAdapter>,
            db.to_string_lossy().into_owned(),
        );
        svc.init().await.expect("init");

        let item = svc
            .create_work_item(CreateWorkItemRequest {
                workflow_id: "reactive_candidates".into(),
                session_id: None,
                context: serde_json::json!({
                    "repo_catalog": [
                        { "repo_url": "https://github.com/reddit/opencode", "base_ref": "origin/main" },
                        { "repo_url": "https://github.com/reddit/opencode", "base_ref": "origin/release" },
                        { "repo_url": "https://github.com/reddit/Dippy", "base_ref": "origin/main" },
                        { "repo_url": "https://github.com/reddit/Dippy", "base_ref": "origin/main" }
                    ]
                }),
                repo: None,
            })
            .await
            .expect("create item");

        let job = svc
            .list_jobs_for_work_item(item.id)
            .await
            .expect("jobs")
            .into_iter()
            .find(|job| job.step_id == "run_agent")
            .expect("run_agent job");
        let runtime = canonical_runtime_spec_for_job_effective(&job);
        let repo_urls = runtime
            .repos
            .iter()
            .map(|repo| repo.repo_url.clone())
            .collect::<Vec<_>>();

        assert_eq!(repo_urls.len(), 3);
        assert_eq!(repo_urls[0], "https://github.com/reddit/agent-hub");
        assert_eq!(repo_urls[1], "https://github.com/reddit/opencode");
        assert_eq!(repo_urls[2], "https://github.com/reddit/Dippy");

        let _ = std::fs::remove_file(db);
        let _ = std::fs::remove_dir_all(tasks);
    }

    #[tokio::test]
    async fn reactive_agent_output_repo_is_promoted_to_work_item_repo() {
        let db = std::env::temp_dir().join(format!("orch-{}.sqlite3", Uuid::new_v4()));
        let tasks = std::env::temp_dir().join(format!("tasks-{}", Uuid::new_v4()));
        let svc = OrchestrationService::new(
            reactive_workflow_catalog(),
            Arc::new(TaskFileStore::new(tasks.clone())) as Arc<dyn TaskAdapter>,
            db.to_string_lossy().into_owned(),
        );
        svc.init().await.expect("init");

        let item = svc
            .create_work_item(CreateWorkItemRequest {
                workflow_id: "reactive_wf".into(),
                session_id: None,
                context: serde_json::json!({}),
                repo: None,
            })
            .await
            .expect("create item");

        svc.register_worker(RegisterWorkerRequest {
            id: "w1".into(),
            hostname: "host".into(),
            supported_modes: vec![protocol::ExecutionMode::Raw],
        })
        .await
        .expect("register");

        let job = svc
            .poll_job(PollJobRequest {
                worker_id: "w1".into(),
            })
            .await
            .expect("poll")
            .job
            .expect("assigned job");

        svc.update_job(
            job.id,
            JobUpdateRequest {
                worker_id: "w1".into(),
                state: JobState::Running,
                transcript_dir: None,
                result: None,
            },
        )
        .await
        .expect("start job");

        svc.update_job(
            job.id,
            JobUpdateRequest {
                worker_id: "w1".into(),
                state: JobState::Succeeded,
                transcript_dir: Some("/tmp/x".into()),
                result: Some(JobResult {
                    outcome: protocol::ExecutionOutcome::Succeeded,
                    termination_reason: protocol::TerminationReason::ExitCode,
                    exit_code: Some(0),
                    timing: protocol::ExecutionTiming {
                        started_at: Utc::now(),
                        finished_at: Utc::now(),
                        duration_ms: 1,
                    },
                    artifacts: protocol::ArtifactSummary {
                        stdout_path: "/tmp/x/stdout.log".into(),
                        stderr_path: "/tmp/x/stderr.log".into(),
                        output_path: None,
                        transcript_dir: Some("/tmp/x".into()),
                        stdout_bytes: None,
                        stderr_bytes: None,
                        output_bytes: None,
                        structured_transcript_artifacts: vec![],
                    },
                    output_json: Some(serde_json::json!({
                        "next_event": "job_succeeded",
                        "repo": {
                            "repo_url": "https://github.com/reddit/agent-hub",
                            "base_ref": "origin/main",
                            "branch_name": "feature/discovered"
                        }
                    })),
                    repo: None,
                    github_pr: None,
                    error_message: None,
                }),
            },
        )
        .await
        .expect("finish job");

        let updated = svc
            .get_work_item(item.id)
            .await
            .expect("get")
            .expect("exists");
        let repo = updated.repo.expect("repo promoted");
        assert_eq!(repo.repo_url, "https://github.com/reddit/agent-hub");
        assert_eq!(repo.base_ref, "origin/main");
        assert_eq!(repo.branch_name.as_deref(), Some("feature/discovered"));

        let _ = std::fs::remove_file(db);
        let _ = std::fs::remove_dir_all(tasks);
    }

    #[tokio::test]
    async fn reactive_agent_output_invalid_repo_is_not_promoted_to_work_item_repo() {
        let db = std::env::temp_dir().join(format!("orch-{}.sqlite3", Uuid::new_v4()));
        let tasks = std::env::temp_dir().join(format!("tasks-{}", Uuid::new_v4()));
        let svc = OrchestrationService::new(
            reactive_workflow_catalog(),
            Arc::new(TaskFileStore::new(tasks.clone())) as Arc<dyn TaskAdapter>,
            db.to_string_lossy().into_owned(),
        );
        svc.init().await.expect("init");

        let item = svc
            .create_work_item(CreateWorkItemRequest {
                workflow_id: "reactive_wf".into(),
                session_id: None,
                context: serde_json::json!({}),
                repo: None,
            })
            .await
            .expect("create item");

        svc.register_worker(RegisterWorkerRequest {
            id: "w1".into(),
            hostname: "host".into(),
            supported_modes: vec![protocol::ExecutionMode::Raw],
        })
        .await
        .expect("register");

        let job = svc
            .poll_job(PollJobRequest {
                worker_id: "w1".into(),
            })
            .await
            .expect("poll")
            .job
            .expect("assigned job");

        svc.update_job(
            job.id,
            JobUpdateRequest {
                worker_id: "w1".into(),
                state: JobState::Running,
                transcript_dir: None,
                result: None,
            },
        )
        .await
        .expect("start job");

        svc.update_job(
            job.id,
            JobUpdateRequest {
                worker_id: "w1".into(),
                state: JobState::Succeeded,
                transcript_dir: Some("/tmp/x".into()),
                result: Some(JobResult {
                    outcome: protocol::ExecutionOutcome::Succeeded,
                    termination_reason: protocol::TerminationReason::ExitCode,
                    exit_code: Some(0),
                    timing: protocol::ExecutionTiming {
                        started_at: Utc::now(),
                        finished_at: Utc::now(),
                        duration_ms: 1,
                    },
                    artifacts: protocol::ArtifactSummary {
                        stdout_path: "/tmp/x/stdout.log".into(),
                        stderr_path: "/tmp/x/stderr.log".into(),
                        output_path: None,
                        transcript_dir: Some("/tmp/x".into()),
                        stdout_bytes: None,
                        stderr_bytes: None,
                        output_bytes: None,
                        structured_transcript_artifacts: vec![],
                    },
                    output_json: Some(serde_json::json!({
                        "next_event": "job_succeeded",
                        "repo": {
                            "repo_url": "unknown",
                            "base_ref": "origin/main"
                        }
                    })),
                    repo: None,
                    github_pr: None,
                    error_message: None,
                }),
            },
        )
        .await
        .expect("finish job");

        let updated = svc
            .get_work_item(item.id)
            .await
            .expect("get")
            .expect("exists");
        assert!(updated.repo.is_none());
        assert_eq!(updated.current_step, "review");

        let _ = std::fs::remove_file(db);
        let _ = std::fs::remove_dir_all(tasks);
    }

    #[tokio::test]
    async fn simple_code_change_triage_includes_repo_catalog_hint_context() {
        let db = std::env::temp_dir().join(format!("orch-{}.sqlite3", Uuid::new_v4()));
        let tasks = std::env::temp_dir().join(format!("tasks-{}", Uuid::new_v4()));
        let svc = OrchestrationService::new(
            simple_code_change_catalog(),
            Arc::new(TaskFileStore::new(tasks.clone())) as Arc<dyn TaskAdapter>,
            db.to_string_lossy().into_owned(),
        );
        svc.init().await.expect("init");

        let item = svc
            .create_work_item(CreateWorkItemRequest {
                workflow_id: "simple-code-change".into(),
                session_id: None,
                context: serde_json::json!({}),
                repo: None,
            })
            .await
            .expect("create item");

        let hint = item
            .context
            .get("repo_catalog_hint")
            .and_then(serde_json::Value::as_str)
            .expect("repo catalog hint present");
        assert!(hint.contains("https://github.com/nickdavies/agent-hub-poc-test.git"));

        let catalog = item
            .context
            .get("repo_catalog")
            .and_then(serde_json::Value::as_array)
            .expect("repo catalog present");
        assert_eq!(catalog.len(), 1);
        assert_eq!(
            catalog[0]["repo_url"],
            serde_json::json!("https://github.com/nickdavies/agent-hub-poc-test.git")
        );

        let _ = std::fs::remove_file(db);
        let _ = std::fs::remove_dir_all(tasks);
    }

    #[test]
    fn simple_code_change_interpolation_context_includes_repo_catalog_hint_when_missing() {
        let item = WorkItem {
            id: Uuid::new_v4(),
            workflow_id: SIMPLE_CODE_CHANGE_WORKFLOW_ID.to_string(),
            external_task_id: None,
            external_task_source: None,
            title: Some("Repo-less work item".to_string()),
            description: Some("Needs repo discovery".to_string()),
            labels: vec![],
            priority: None,
            session_id: None,
            current_step: SIMPLE_CODE_CHANGE_TRIAGE_STATE_ID.to_string(),
            projected_state: None,
            triage_status: None,
            human_gate: false,
            human_gate_state: None,
            blocked_on: None,
            state: WorkItemState::Pending,
            context: serde_json::json!({}),
            repo: None,
            parent_work_item_id: None,
            parent_step_id: None,
            child_work_item_id: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        let prompt_context = interpolation_context_for_work_item(&item, &item.current_step);
        let rendered = interpolate_template("triage {{repo_catalog_hint}}", &prompt_context)
            .expect("repo catalog hint should interpolate");

        assert!(rendered.contains("https://github.com/nickdavies/agent-hub-poc-test.git"));
    }

    #[tokio::test]
    async fn simple_code_change_triage_repo_discovery_failure_routes_to_human_state() {
        let db = std::env::temp_dir().join(format!("orch-{}.sqlite3", Uuid::new_v4()));
        let tasks = std::env::temp_dir().join(format!("tasks-{}", Uuid::new_v4()));
        let svc = OrchestrationService::new(
            simple_code_change_catalog(),
            Arc::new(TaskFileStore::new(tasks.clone())) as Arc<dyn TaskAdapter>,
            db.to_string_lossy().into_owned(),
        );
        svc.init().await.expect("init");

        let item = svc
            .create_work_item(CreateWorkItemRequest {
                workflow_id: "simple-code-change".into(),
                session_id: None,
                context: serde_json::json!({}),
                repo: None,
            })
            .await
            .expect("create item");

        svc.register_worker(RegisterWorkerRequest {
            id: "w1".into(),
            hostname: "host".into(),
            supported_modes: vec![protocol::ExecutionMode::Raw],
        })
        .await
        .expect("register");

        let job = svc
            .poll_job(PollJobRequest {
                worker_id: "w1".into(),
            })
            .await
            .expect("poll")
            .job
            .expect("assigned job");
        assert_eq!(job.step_id, "triage_investigation");

        svc.update_job(
            job.id,
            JobUpdateRequest {
                worker_id: "w1".into(),
                state: JobState::Running,
                transcript_dir: None,
                result: None,
            },
        )
        .await
        .expect("start job");

        svc.update_job(
            job.id,
            JobUpdateRequest {
                worker_id: "w1".into(),
                state: JobState::Succeeded,
                transcript_dir: Some("/tmp/x".into()),
                result: Some(JobResult {
                    outcome: protocol::ExecutionOutcome::Succeeded,
                    termination_reason: protocol::TerminationReason::ExitCode,
                    exit_code: Some(0),
                    timing: protocol::ExecutionTiming {
                        started_at: Utc::now(),
                        finished_at: Utc::now(),
                        duration_ms: 1,
                    },
                    artifacts: protocol::ArtifactSummary {
                        stdout_path: "/tmp/x/stdout.log".into(),
                        stderr_path: "/tmp/x/stderr.log".into(),
                        output_path: None,
                        transcript_dir: Some("/tmp/x".into()),
                        stdout_bytes: None,
                        stderr_bytes: None,
                        output_bytes: None,
                        structured_transcript_artifacts: vec![],
                    },
                    output_json: Some(serde_json::json!({
                        "next_event": "job_succeeded",
                        "repo": {
                            "repo_url": "unknown",
                            "base_ref": "origin/main"
                        }
                    })),
                    repo: None,
                    github_pr: None,
                    error_message: None,
                }),
            },
        )
        .await
        .expect("finish job");

        let updated = svc
            .get_work_item(item.id)
            .await
            .expect("get")
            .expect("exists");
        assert_eq!(updated.current_step, "triage_failed");
        assert!(updated.human_gate);
        assert!(updated.repo.is_none());

        let _ = std::fs::remove_file(db);
        let _ = std::fs::remove_dir_all(tasks);
    }

    #[tokio::test]
    async fn reactive_managed_job_context_projects_internal_transition_events() {
        let db = std::env::temp_dir().join(format!("orch-{}.sqlite3", Uuid::new_v4()));
        let tasks = std::env::temp_dir().join(format!("tasks-{}", Uuid::new_v4()));
        let svc = OrchestrationService::new(
            reactive_managed_catalog(),
            Arc::new(TaskFileStore::new(tasks.clone())) as Arc<dyn TaskAdapter>,
            db.to_string_lossy().into_owned(),
        );
        svc.init().await.expect("init");

        let item = svc
            .create_work_item(CreateWorkItemRequest {
                workflow_id: "reactive_managed".into(),
                session_id: None,
                context: serde_json::json!({}),
                repo: None,
            })
            .await
            .expect("create item");

        let job = svc
            .list_jobs_for_work_item(item.id)
            .await
            .expect("jobs")
            .into_iter()
            .find(|job| job.step_id == "run_managed")
            .expect("run_managed job");

        assert_eq!(
            job.context["workflow"]["inputs"]["valid_next_events"],
            serde_json::json!(["job_succeeded", "job_failed", "cancel"])
        );

        let _ = std::fs::remove_file(db);
        let _ = std::fs::remove_dir_all(tasks);
    }

    #[tokio::test]
    async fn reactive_command_job_context_projects_internal_transition_events() {
        let db = std::env::temp_dir().join(format!("orch-{}.sqlite3", Uuid::new_v4()));
        let tasks = std::env::temp_dir().join(format!("tasks-{}", Uuid::new_v4()));
        let svc = OrchestrationService::new(
            reactive_child_only_catalog(),
            Arc::new(TaskFileStore::new(tasks.clone())) as Arc<dyn TaskAdapter>,
            db.to_string_lossy().into_owned(),
        );
        svc.init().await.expect("init");

        let item = svc
            .create_work_item(CreateWorkItemRequest {
                workflow_id: "child_reactive".into(),
                session_id: None,
                context: serde_json::json!({}),
                repo: None,
            })
            .await
            .expect("create item");

        let job = svc
            .list_jobs_for_work_item(item.id)
            .await
            .expect("jobs")
            .into_iter()
            .find(|job| job.step_id == "run")
            .expect("run command job");

        assert_eq!(
            job.context["workflow"]["inputs"]["valid_next_events"],
            serde_json::json!(["job_succeeded", "cancel"])
        );

        let _ = std::fs::remove_file(db);
        let _ = std::fs::remove_dir_all(tasks);
    }

    #[tokio::test]
    async fn valid_next_events_for_failed_legacy_item_include_retry_and_cancel() {
        let db = std::env::temp_dir().join(format!("orch-{}.sqlite3", Uuid::new_v4()));
        let tasks = std::env::temp_dir().join(format!("tasks-{}", Uuid::new_v4()));
        let svc = OrchestrationService::new(
            workflow_catalog(),
            Arc::new(TaskFileStore::new(tasks.clone())) as Arc<dyn TaskAdapter>,
            db.to_string_lossy().into_owned(),
        );
        svc.init().await.expect("init");

        let item = svc
            .create_work_item(CreateWorkItemRequest {
                workflow_id: "wf".into(),
                session_id: None,
                context: serde_json::json!({}),
                repo: None,
            })
            .await
            .expect("create item");

        svc.register_worker(RegisterWorkerRequest {
            id: "w1".into(),
            hostname: "host".into(),
            supported_modes: vec![protocol::ExecutionMode::Raw],
        })
        .await
        .expect("register");

        let job = svc
            .poll_job(PollJobRequest {
                worker_id: "w1".into(),
            })
            .await
            .expect("poll")
            .job
            .expect("assigned job");

        svc.update_job(
            job.id,
            JobUpdateRequest {
                worker_id: "w1".into(),
                state: JobState::Running,
                transcript_dir: None,
                result: None,
            },
        )
        .await
        .expect("start job");

        svc.update_job(
            job.id,
            JobUpdateRequest {
                worker_id: "w1".into(),
                state: JobState::Failed,
                transcript_dir: Some("/tmp/fail".into()),
                result: Some(JobResult {
                    outcome: protocol::ExecutionOutcome::Failed,
                    termination_reason: protocol::TerminationReason::ExitCode,
                    exit_code: Some(1),
                    timing: protocol::ExecutionTiming {
                        started_at: Utc::now(),
                        finished_at: Utc::now(),
                        duration_ms: 1,
                    },
                    artifacts: protocol::ArtifactSummary {
                        stdout_path: "/tmp/fail/stdout.log".into(),
                        stderr_path: "/tmp/fail/stderr.log".into(),
                        output_path: None,
                        transcript_dir: Some("/tmp/fail".into()),
                        stdout_bytes: None,
                        stderr_bytes: None,
                        output_bytes: None,
                        structured_transcript_artifacts: Vec::new(),
                    },
                    output_json: None,
                    repo: None,
                    github_pr: None,
                    error_message: Some("boom".into()),
                }),
            },
        )
        .await
        .expect("fail job");

        let failed_item = svc
            .get_work_item(item.id)
            .await
            .expect("get item")
            .expect("exists");
        assert_eq!(failed_item.state, WorkItemState::Failed);

        let events = svc.valid_next_events_for_work_item(&failed_item);
        assert!(events.contains(&WorkflowEventKind::RetryRequested));
        assert!(events.contains(&WorkflowEventKind::Cancel));

        let _ = std::fs::remove_file(db);
        let _ = std::fs::remove_dir_all(tasks);
    }

    #[tokio::test]
    async fn valid_next_events_for_legacy_human_gate_are_state_aware() {
        let db = std::env::temp_dir().join(format!("orch-{}.sqlite3", Uuid::new_v4()));
        let tasks = std::env::temp_dir().join(format!("tasks-{}", Uuid::new_v4()));
        let svc = OrchestrationService::new(
            workflow_catalog(),
            Arc::new(TaskFileStore::new(tasks.clone())) as Arc<dyn TaskAdapter>,
            db.to_string_lossy().into_owned(),
        );
        svc.init().await.expect("init");

        let mut item = svc
            .create_work_item(CreateWorkItemRequest {
                workflow_id: "wf".into(),
                session_id: None,
                context: serde_json::json!({}),
                repo: None,
            })
            .await
            .expect("create item");
        item.human_gate = true;
        item.human_gate_state = Some("changes_requested".into());

        let events = svc.valid_next_events_for_work_item(&item);
        assert!(events.contains(&WorkflowEventKind::HumanUnblocked));
        assert!(events.contains(&WorkflowEventKind::RetryRequested));
        assert!(events.contains(&WorkflowEventKind::Cancel));
        assert!(!events.contains(&WorkflowEventKind::HumanApproved));
        assert!(!events.contains(&WorkflowEventKind::HumanChangesRequested));

        let _ = std::fs::remove_file(db);
        let _ = std::fs::remove_dir_all(tasks);
    }

    #[tokio::test]
    async fn fire_event_rejects_external_events_not_in_valid_next_events() {
        let db = std::env::temp_dir().join(format!("orch-{}.sqlite3", Uuid::new_v4()));
        let tasks = std::env::temp_dir().join(format!("tasks-{}", Uuid::new_v4()));
        let svc = OrchestrationService::new(
            workflow_catalog(),
            Arc::new(TaskFileStore::new(tasks.clone())) as Arc<dyn TaskAdapter>,
            db.to_string_lossy().into_owned(),
        );
        svc.init().await.expect("init");

        let item = svc
            .create_work_item(CreateWorkItemRequest {
                workflow_id: "wf".into(),
                session_id: None,
                context: serde_json::json!({}),
                repo: None,
            })
            .await
            .expect("create item");

        let err = svc
            .fire_event(
                item.id,
                FireEventRequest {
                    event: WorkflowEventKind::HumanApproved,
                    job_id: None,
                    reason: Some("should reject".into()),
                },
            )
            .await
            .expect_err("event should be rejected by ingress guard");
        assert!(matches!(err, AppError::WorkflowInvalid(_)));
        let msg = err.to_string();
        assert!(msg.contains("event 'human_approved' is not currently valid"));
        assert!(msg.contains("submit"));
        assert!(msg.contains("cancel"));
        assert!(!msg.contains("job_succeeded"));

        let err = svc
            .fire_event(
                item.id,
                FireEventRequest {
                    event: WorkflowEventKind::JobSucceeded,
                    job_id: None,
                    reason: Some("system event via public ingress should reject".into()),
                },
            )
            .await
            .expect_err("system event should be rejected on public ingress");
        assert!(matches!(err, AppError::WorkflowInvalid(_)));

        let progressed = svc
            .fire_event_trusted_internal(
                item.id,
                FireEventRequest {
                    event: WorkflowEventKind::JobSucceeded,
                    job_id: None,
                    reason: Some("trusted internal progression".into()),
                },
            )
            .await
            .expect("trusted internal should allow system progression");
        assert_eq!(progressed.current_step, "test");

        let triage = svc
            .create_work_item(CreateWorkItemRequest {
                workflow_id: "__triage__".into(),
                session_id: None,
                context: serde_json::json!({}),
                repo: None,
            })
            .await
            .expect("create triage item");
        let triage_events = svc.valid_next_events_for_work_item(&triage);
        assert!(!triage_events.contains(&WorkflowEventKind::WorkflowChosen));

        let _ = std::fs::remove_file(db);
        let _ = std::fs::remove_dir_all(tasks);
    }

    #[tokio::test]
    async fn triage_workflow_chosen_is_not_fireable_via_public_or_trusted_event_ingress() {
        let db = std::env::temp_dir().join(format!("orch-{}.sqlite3", Uuid::new_v4()));
        let tasks = std::env::temp_dir().join(format!("tasks-{}", Uuid::new_v4()));
        let svc = OrchestrationService::new(
            workflow_catalog(),
            Arc::new(TaskFileStore::new(tasks.clone())) as Arc<dyn TaskAdapter>,
            db.to_string_lossy().into_owned(),
        );
        svc.init().await.expect("init");

        let triage = svc
            .create_work_item(CreateWorkItemRequest {
                workflow_id: "__triage__".into(),
                session_id: None,
                context: serde_json::json!({}),
                repo: None,
            })
            .await
            .expect("create triage");

        let public_err = svc
            .fire_event(
                triage.id,
                FireEventRequest {
                    event: WorkflowEventKind::WorkflowChosen,
                    job_id: None,
                    reason: Some("should reject".into()),
                },
            )
            .await
            .expect_err("public ingress must reject workflow_chosen");
        assert!(matches!(public_err, AppError::WorkflowInvalid(_)));

        let trusted_err = svc
            .fire_event_trusted_internal(
                triage.id,
                FireEventRequest {
                    event: WorkflowEventKind::WorkflowChosen,
                    job_id: None,
                    reason: Some("still reject; use choose_workflow".into()),
                },
            )
            .await
            .expect_err("trusted ingress should also reject triage workflow_chosen");
        assert!(matches!(trusted_err, AppError::WorkflowInvalid(_)));

        let selected = svc
            .choose_workflow(triage.id, "wf".to_string())
            .await
            .expect("choose_workflow should remain dedicated path");
        assert_eq!(selected.workflow_id, "wf");

        let _ = std::fs::remove_file(db);
        let _ = std::fs::remove_dir_all(tasks);
    }
}
