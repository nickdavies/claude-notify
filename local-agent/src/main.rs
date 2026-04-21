use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use chrono::Utc;
use clap::{Parser, ValueEnum};
use protocol::{
    AGENT_CONTEXT_SCHEMA_NAME, AGENT_CONTEXT_SCHEMA_VERSION, AgentContext, AgentContextArtifacts,
    AgentContextJob, AgentContextPaths, AgentContextRepos, AgentContextWorkflow, AgentOutput,
    ArtifactSummary, ExecutionMode, ExecutionOutcome, ExecutionTiming, GithubPullRequestMetadata,
    HeartbeatRequest, JOB_RUN_SCHEMA_NAME, Job, JobEvent, JobEventKind, JobResult, JobState,
    JobUpdateRequest, NetworkResourceKind, NetworkResourceRequest, PollJobRequest, PollJobResponse,
    RegisterWorkerRequest, RepoExecutionMetadata, Secret, StructuredTranscriptArtifact,
    TerminationReason, WorkerState, canonical_runtime_spec_for_job,
    validate_agent_output_next_event,
};
use tokio::io::AsyncWriteExt;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, BufReader};
use tokio::sync::Mutex;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::metadata::MetadataValue;
use tonic::transport::Channel;

mod egress;
mod egress_config;

use egress::{EgressManager, llm_provider_from_env};
use egress_config::{
    LocalAgentConfig, ManagedRunnerKind, ManagedRunnerSpec, load_local_agent_config,
};

pub mod pb {
    tonic::include_proto!("worker");
}

#[derive(Parser)]
#[command(name = "local-agent", version)]
struct Cli {
    #[arg(
        long,
        env = "AGENT_HUB_SERVER",
        default_value = "http://localhost:8080"
    )]
    server: String,

    #[arg(long, env = "AGENT_HUB_TOKEN")]
    token: Option<String>,

    #[arg(long)]
    worker_id: String,

    #[arg(long, default_value = "localhost")]
    hostname: String,

    #[arg(long, value_enum, default_values_t = vec![ModeArg::Raw, ModeArg::Docker])]
    mode: Vec<ModeArg>,

    #[arg(long, default_value = "./local-agent-data")]
    data_dir: PathBuf,

    /// Optional local-agent JSON config override (default: ~/.config/agent-hub/local-agent.json)
    #[arg(long)]
    local_agent_config: Option<PathBuf>,

    #[arg(long, default_value_t = 3)]
    poll_interval_secs: u64,

    #[arg(long, default_value_t = 168)]
    transcript_retention_hours: u64,

    #[arg(long, value_enum, default_value_t = TransportKind::Http)]
    transport: TransportKind,

    /// gRPC endpoint, e.g. http://127.0.0.1:50051
    #[arg(long, env = "AGENT_HUB_GRPC", default_value = "http://127.0.0.1:50051")]
    grpc_endpoint: String,
}

#[derive(Clone, ValueEnum)]
enum ModeArg {
    Raw,
    Docker,
}

#[derive(Clone, ValueEnum)]
enum TransportKind {
    Http,
    Grpc,
}

impl From<ModeArg> for ExecutionMode {
    fn from(value: ModeArg) -> Self {
        match value {
            ModeArg::Raw => ExecutionMode::Raw,
            ModeArg::Docker => ExecutionMode::Docker,
        }
    }
}

#[derive(Clone)]
enum Client {
    Http(HttpClient),
    Grpc(GrpcClient),
}

#[derive(Clone)]
struct HttpClient {
    http: reqwest::Client,
    base: String,
    token: Option<Secret>,
}

#[derive(Clone)]
struct GrpcClient {
    client: pb::worker_lifecycle_client::WorkerLifecycleClient<Channel>,
    token: Option<Secret>,
}

#[derive(Clone)]
struct JobEventPublisher {
    worker_id: String,
    tx: mpsc::Sender<pb::JobEventChunk>,
}

#[derive(Clone)]
struct JobEventSink {
    worker_id: String,
    writer: Arc<Mutex<JobEventTranscriptWriter>>,
    publisher: Option<JobEventPublisher>,
}

struct JobEventTranscriptWriter {
    file: tokio::fs::File,
}

impl JobEventPublisher {
    async fn emit(
        &self,
        job_id: uuid::Uuid,
        at: chrono::DateTime<Utc>,
        kind: pb::JobEventKind,
        message: String,
    ) {
        let _ = self
            .tx
            .send(pb::JobEventChunk {
                worker_id: self.worker_id.clone(),
                job_id: job_id.to_string(),
                at: Some(ts_to_pb(at)),
                kind: kind as i32,
                stream: job_event_stream_name(kind).map(ToOwned::to_owned),
                message,
            })
            .await;
    }
}

impl JobEventSink {
    async fn emit(&self, job_id: uuid::Uuid, kind: pb::JobEventKind, message: String) {
        let at = Utc::now();
        let event = JobEvent {
            job_id,
            worker_id: self.worker_id.clone(),
            at,
            kind: protocol_job_event_kind(kind),
            stream: job_event_stream_name(kind).map(ToOwned::to_owned),
            message: message.clone(),
        };
        {
            let mut writer = self.writer.lock().await;
            if let Err(error) = writer.write_event(&event).await {
                eprintln!(
                    "failed to append local job event transcript for {}: {error:#}",
                    event.job_id
                );
            }
        }

        if let Some(publisher) = &self.publisher {
            publisher.emit(job_id, at, kind, message).await;
        }
    }
}

impl JobEventTranscriptWriter {
    async fn create(path: &Path) -> anyhow::Result<Self> {
        let file = tokio::fs::File::create(path)
            .await
            .with_context(|| format!("create {}", path.display()))?;
        Ok(Self { file })
    }

    async fn write_event(&mut self, event: &JobEvent) -> anyhow::Result<()> {
        let mut line = serde_json::to_vec(event)?;
        line.push(b'\n');
        self.file.write_all(&line).await?;
        self.file.flush().await?;
        Ok(())
    }
}

fn protocol_job_event_kind(kind: pb::JobEventKind) -> JobEventKind {
    match kind {
        pb::JobEventKind::JobEventStdout => JobEventKind::Stdout,
        pb::JobEventKind::JobEventStderr => JobEventKind::Stderr,
        pb::JobEventKind::JobEventSystem | pb::JobEventKind::JobEventUnspecified => {
            JobEventKind::System
        }
    }
}

fn job_event_stream_name(kind: pb::JobEventKind) -> Option<&'static str> {
    match kind {
        pb::JobEventKind::JobEventStdout => Some("stdout"),
        pb::JobEventKind::JobEventStderr => Some("stderr"),
        _ => None,
    }
}

async fn start_grpc_event_stream(
    mut client: pb::worker_lifecycle_client::WorkerLifecycleClient<Channel>,
    token: Option<Secret>,
) -> anyhow::Result<mpsc::Sender<pb::JobEventChunk>> {
    let (tx, rx) = mpsc::channel::<pb::JobEventChunk>(512);
    let req = apply_grpc_bearer(token.as_ref(), tonic::Request::new(ReceiverStream::new(rx)))?;
    tokio::spawn(async move {
        if let Err(error) = client.stream_job_events(req).await {
            eprintln!("gRPC event stream ended: {error}");
        }
    });
    Ok(tx)
}

fn apply_grpc_bearer<T>(
    token: Option<&Secret>,
    mut request: tonic::Request<T>,
) -> anyhow::Result<tonic::Request<T>> {
    if let Some(token) = token {
        let value = MetadataValue::try_from(format!("Bearer {}", token.expose()))
            .context("invalid AGENT_HUB_TOKEN for gRPC metadata")?;
        request.metadata_mut().insert("authorization", value);
    }
    Ok(request)
}

fn execution_mode_to_pb(v: ExecutionMode) -> i32 {
    match v {
        ExecutionMode::Raw => pb::ExecutionMode::Raw as i32,
        ExecutionMode::Docker => pb::ExecutionMode::Docker as i32,
    }
}

fn worker_state_to_pb(v: WorkerState) -> i32 {
    match v {
        WorkerState::Idle => pb::WorkerState::Idle as i32,
        WorkerState::Busy => pb::WorkerState::Busy as i32,
        WorkerState::Offline => pb::WorkerState::Offline as i32,
    }
}

fn job_state_to_pb(v: JobState) -> i32 {
    match v {
        JobState::Pending => pb::JobState::Pending as i32,
        JobState::Assigned => pb::JobState::Assigned as i32,
        JobState::Running => pb::JobState::Running as i32,
        JobState::Succeeded => pb::JobState::Succeeded as i32,
        JobState::Failed => pb::JobState::Failed as i32,
        JobState::Cancelled => pb::JobState::Cancelled as i32,
    }
}

fn execution_mode_from_pb(v: i32) -> anyhow::Result<ExecutionMode> {
    Ok(
        match pb::ExecutionMode::try_from(v).unwrap_or(pb::ExecutionMode::Unspecified) {
            pb::ExecutionMode::Raw => ExecutionMode::Raw,
            pb::ExecutionMode::Docker => ExecutionMode::Docker,
            pb::ExecutionMode::Unspecified => anyhow::bail!("execution mode unspecified"),
        },
    )
}

fn job_state_from_pb(v: i32) -> anyhow::Result<JobState> {
    Ok(
        match pb::JobState::try_from(v).unwrap_or(pb::JobState::JobUnspecified) {
            pb::JobState::Pending => JobState::Pending,
            pb::JobState::Assigned => JobState::Assigned,
            pb::JobState::Running => JobState::Running,
            pb::JobState::Succeeded => JobState::Succeeded,
            pb::JobState::Failed => JobState::Failed,
            pb::JobState::Cancelled => JobState::Cancelled,
            pb::JobState::JobUnspecified => anyhow::bail!("job state unspecified"),
        },
    )
}

fn execution_outcome_to_pb(v: ExecutionOutcome) -> i32 {
    match v {
        ExecutionOutcome::Succeeded => pb::ExecutionOutcome::OutcomeSucceeded as i32,
        ExecutionOutcome::Failed => pb::ExecutionOutcome::OutcomeFailed as i32,
        ExecutionOutcome::TimedOut => pb::ExecutionOutcome::OutcomeTimedOut as i32,
        ExecutionOutcome::Cancelled => pb::ExecutionOutcome::OutcomeCancelled as i32,
    }
}

fn execution_outcome_from_pb(v: i32) -> anyhow::Result<ExecutionOutcome> {
    Ok(
        match pb::ExecutionOutcome::try_from(v).unwrap_or(pb::ExecutionOutcome::OutcomeUnspecified)
        {
            pb::ExecutionOutcome::OutcomeSucceeded => ExecutionOutcome::Succeeded,
            pb::ExecutionOutcome::OutcomeFailed => ExecutionOutcome::Failed,
            pb::ExecutionOutcome::OutcomeTimedOut => ExecutionOutcome::TimedOut,
            pb::ExecutionOutcome::OutcomeCancelled => ExecutionOutcome::Cancelled,
            pb::ExecutionOutcome::OutcomeUnspecified => {
                anyhow::bail!("execution outcome unspecified")
            }
        },
    )
}

fn termination_reason_to_pb(v: TerminationReason) -> i32 {
    match v {
        TerminationReason::ExitCode => pb::TerminationReason::ReasonExitCode as i32,
        TerminationReason::Timeout => pb::TerminationReason::ReasonTimeout as i32,
        TerminationReason::Cancelled => pb::TerminationReason::ReasonCancelled as i32,
        TerminationReason::WorkerError => pb::TerminationReason::ReasonWorkerError as i32,
    }
}

fn termination_reason_from_pb(v: i32) -> anyhow::Result<TerminationReason> {
    Ok(
        match pb::TerminationReason::try_from(v).unwrap_or(pb::TerminationReason::ReasonUnspecified)
        {
            pb::TerminationReason::ReasonExitCode => TerminationReason::ExitCode,
            pb::TerminationReason::ReasonTimeout => TerminationReason::Timeout,
            pb::TerminationReason::ReasonCancelled => TerminationReason::Cancelled,
            pb::TerminationReason::ReasonWorkerError => TerminationReason::WorkerError,
            pb::TerminationReason::ReasonUnspecified => {
                anyhow::bail!("termination reason unspecified")
            }
        },
    )
}

fn ts_to_pb(v: chrono::DateTime<Utc>) -> prost_types::Timestamp {
    prost_types::Timestamp {
        seconds: v.timestamp(),
        nanos: v.timestamp_subsec_nanos() as i32,
    }
}

fn ts_from_pb(v: prost_types::Timestamp) -> anyhow::Result<chrono::DateTime<Utc>> {
    chrono::DateTime::from_timestamp(v.seconds, v.nanos as u32)
        .ok_or_else(|| anyhow::anyhow!("invalid timestamp"))
}

fn json_to_pb(v: serde_json::Value) -> prost_types::Value {
    match v {
        serde_json::Value::Null => prost_types::Value {
            kind: Some(prost_types::value::Kind::NullValue(0)),
        },
        serde_json::Value::Bool(b) => prost_types::Value {
            kind: Some(prost_types::value::Kind::BoolValue(b)),
        },
        serde_json::Value::Number(n) => prost_types::Value {
            kind: Some(prost_types::value::Kind::NumberValue(
                n.as_f64().unwrap_or_default(),
            )),
        },
        serde_json::Value::String(s) => prost_types::Value {
            kind: Some(prost_types::value::Kind::StringValue(s)),
        },
        serde_json::Value::Array(arr) => prost_types::Value {
            kind: Some(prost_types::value::Kind::ListValue(
                prost_types::ListValue {
                    values: arr.into_iter().map(json_to_pb).collect(),
                },
            )),
        },
        serde_json::Value::Object(obj) => prost_types::Value {
            kind: Some(prost_types::value::Kind::StructValue(prost_types::Struct {
                fields: obj.into_iter().map(|(k, v)| (k, json_to_pb(v))).collect(),
            })),
        },
    }
}

fn json_from_pb(v: prost_types::Value) -> anyhow::Result<serde_json::Value> {
    Ok(match v.kind {
        None => serde_json::Value::Null,
        Some(prost_types::value::Kind::NullValue(_)) => serde_json::Value::Null,
        Some(prost_types::value::Kind::BoolValue(b)) => serde_json::Value::Bool(b),
        Some(prost_types::value::Kind::NumberValue(n)) => serde_json::json!(n),
        Some(prost_types::value::Kind::StringValue(s)) => serde_json::Value::String(s),
        Some(prost_types::value::Kind::ListValue(list)) => serde_json::Value::Array(
            list.values
                .into_iter()
                .map(json_from_pb)
                .collect::<anyhow::Result<Vec<_>>>()?,
        ),
        Some(prost_types::value::Kind::StructValue(s)) => serde_json::Value::Object(
            s.fields
                .into_iter()
                .map(|(k, v)| Ok((k, json_from_pb(v)?)))
                .collect::<anyhow::Result<serde_json::Map<_, _>>>()?,
        ),
    })
}

#[cfg(test)]
fn prompt_to_pb(v: protocol::PromptRef) -> pb::PromptRef {
    pb::PromptRef {
        system: v.system,
        user: v.user,
    }
}

fn prompt_from_pb(v: pb::PromptRef) -> protocol::PromptRef {
    protocol::PromptRef {
        system: v.system,
        user: v.user,
    }
}

#[cfg(test)]
fn repo_to_pb(v: protocol::RepoSource) -> pb::RepoSource {
    pb::RepoSource {
        repo_url: v.repo_url,
        base_ref: v.base_ref,
        branch_name: v.branch_name,
    }
}

fn repo_from_pb(v: pb::RepoSource) -> protocol::RepoSource {
    protocol::RepoSource {
        repo_url: v.repo_url,
        base_ref: v.base_ref,
        branch_name: v.branch_name,
    }
}

fn artifact_to_pb(v: ArtifactSummary) -> pb::ArtifactSummary {
    pb::ArtifactSummary {
        stdout_path: v.stdout_path,
        stderr_path: v.stderr_path,
        output_path: v.output_path,
        transcript_dir: v.transcript_dir,
        stdout_bytes: v.stdout_bytes,
        stderr_bytes: v.stderr_bytes,
        output_bytes: v.output_bytes,
        structured_transcript_artifacts: v
            .structured_transcript_artifacts
            .into_iter()
            .map(structured_artifact_to_pb)
            .collect(),
    }
}

fn artifact_from_pb(v: pb::ArtifactSummary) -> ArtifactSummary {
    ArtifactSummary {
        stdout_path: v.stdout_path,
        stderr_path: v.stderr_path,
        output_path: v.output_path,
        transcript_dir: v.transcript_dir,
        stdout_bytes: v.stdout_bytes,
        stderr_bytes: v.stderr_bytes,
        output_bytes: v.output_bytes,
        structured_transcript_artifacts: v
            .structured_transcript_artifacts
            .into_iter()
            .map(structured_artifact_from_pb)
            .collect(),
    }
}

fn structured_artifact_to_pb(v: StructuredTranscriptArtifact) -> pb::StructuredTranscriptArtifact {
    pb::StructuredTranscriptArtifact {
        key: v.key,
        path: v.path,
        bytes: v.bytes,
        schema: v.schema,
        record_count: v.record_count,
        inline_json: v.inline_json.map(json_to_pb),
    }
}

fn structured_artifact_from_pb(
    v: pb::StructuredTranscriptArtifact,
) -> StructuredTranscriptArtifact {
    StructuredTranscriptArtifact {
        key: v.key,
        path: v.path,
        bytes: v.bytes,
        schema: v.schema,
        record_count: v.record_count,
        inline_json: v.inline_json.map(json_from_pb).transpose().ok().flatten(),
    }
}

fn timing_to_pb(v: ExecutionTiming) -> pb::ExecutionTiming {
    pb::ExecutionTiming {
        started_at: Some(ts_to_pb(v.started_at)),
        finished_at: Some(ts_to_pb(v.finished_at)),
        duration_ms: v.duration_ms,
    }
}

fn timing_from_pb(v: pb::ExecutionTiming) -> anyhow::Result<ExecutionTiming> {
    Ok(ExecutionTiming {
        started_at: ts_from_pb(v.started_at.context("missing started_at")?)?,
        finished_at: ts_from_pb(v.finished_at.context("missing finished_at")?)?,
        duration_ms: v.duration_ms,
    })
}

fn repo_meta_to_pb(v: RepoExecutionMetadata) -> pb::RepoExecutionMetadata {
    pb::RepoExecutionMetadata {
        base_ref: v.base_ref,
        target_branch: v.target_branch,
        branch: v.branch,
        head_sha: v.head_sha,
        is_dirty: v.is_dirty,
        changed_files: v.changed_files,
        untracked_files: v.untracked_files,
    }
}

fn repo_meta_from_pb(v: pb::RepoExecutionMetadata) -> RepoExecutionMetadata {
    RepoExecutionMetadata {
        base_ref: v.base_ref,
        target_branch: v.target_branch,
        branch: v.branch,
        head_sha: v.head_sha,
        is_dirty: v.is_dirty,
        changed_files: v.changed_files,
        untracked_files: v.untracked_files,
    }
}

fn github_pr_to_pb(v: GithubPullRequestMetadata) -> pb::GithubPullRequestMetadata {
    pb::GithubPullRequestMetadata {
        number: v.number,
        url: v.url,
        state: v.state,
        is_draft: v.is_draft,
        head_ref_name: v.head_ref_name,
        base_ref_name: v.base_ref_name,
    }
}

fn github_pr_from_pb(v: pb::GithubPullRequestMetadata) -> GithubPullRequestMetadata {
    GithubPullRequestMetadata {
        number: v.number,
        url: v.url,
        state: v.state,
        is_draft: v.is_draft,
        head_ref_name: v.head_ref_name,
        base_ref_name: v.base_ref_name,
    }
}

fn job_result_to_pb(v: JobResult) -> pb::JobResult {
    pb::JobResult {
        outcome: execution_outcome_to_pb(v.outcome),
        termination_reason: termination_reason_to_pb(v.termination_reason),
        exit_code: v.exit_code,
        timing: Some(timing_to_pb(v.timing)),
        artifacts: Some(artifact_to_pb(v.artifacts)),
        output_json: v.output_json.map(json_to_pb),
        repo: v.repo.map(repo_meta_to_pb),
        github_pr: v.github_pr.map(github_pr_to_pb),
        error_message: v.error_message,
    }
}

fn job_result_from_pb(v: pb::JobResult) -> anyhow::Result<JobResult> {
    Ok(JobResult {
        outcome: execution_outcome_from_pb(v.outcome)?,
        termination_reason: termination_reason_from_pb(v.termination_reason)?,
        exit_code: v.exit_code,
        timing: timing_from_pb(v.timing.context("missing timing")?)?,
        artifacts: artifact_from_pb(v.artifacts.context("missing artifacts")?),
        output_json: v.output_json.map(json_from_pb).transpose()?,
        repo: v.repo.map(repo_meta_from_pb),
        github_pr: v.github_pr.map(github_pr_from_pb),
        error_message: v.error_message,
    })
}

fn job_from_pb(v: pb::Job) -> anyhow::Result<Job> {
    Ok(Job {
        id: uuid::Uuid::parse_str(&v.id).context("invalid job id")?,
        work_item_id: uuid::Uuid::parse_str(&v.work_item_id).context("invalid work_item_id")?,
        step_id: v.step_id,
        state: job_state_from_pb(v.state)?,
        assigned_worker_id: v.assigned_worker_id,
        execution_mode: execution_mode_from_pb(v.execution_mode)?,
        container_image: v.container_image,
        command: v.command,
        args: v.args,
        env: v.env,
        prompt: v.prompt.map(prompt_from_pb),
        context: json_from_pb(v.context.context("missing context")?)?,
        repo: v.repo.map(repo_from_pb),
        timeout_secs: v.timeout_secs,
        max_retries: v.max_retries,
        attempt: v.attempt,
        transcript_dir: v.transcript_dir,
        result: v.result.map(job_result_from_pb).transpose()?,
        lease_expires_at: v.lease_expires_at.map(ts_from_pb).transpose()?,
        created_at: ts_from_pb(v.created_at.context("missing created_at")?)?,
        updated_at: ts_from_pb(v.updated_at.context("missing updated_at")?)?,
    })
}

#[cfg(test)]
fn job_to_pb(v: Job) -> pb::Job {
    pb::Job {
        id: v.id.to_string(),
        work_item_id: v.work_item_id.to_string(),
        step_id: v.step_id,
        state: job_state_to_pb(v.state),
        assigned_worker_id: v.assigned_worker_id,
        execution_mode: execution_mode_to_pb(v.execution_mode),
        container_image: v.container_image,
        command: v.command,
        args: v.args,
        env: v.env,
        prompt: v.prompt.map(prompt_to_pb),
        context: Some(json_to_pb(v.context)),
        repo: v.repo.map(repo_to_pb),
        timeout_secs: v.timeout_secs,
        max_retries: v.max_retries,
        attempt: v.attempt,
        transcript_dir: v.transcript_dir,
        result: v.result.map(job_result_to_pb),
        lease_expires_at: v.lease_expires_at.map(ts_to_pb),
        created_at: Some(ts_to_pb(v.created_at)),
        updated_at: Some(ts_to_pb(v.updated_at)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn heartbeat_shutdown_watch_signal_is_sticky() {
        let (hb_stop_tx, mut hb_stop_rx) = tokio::sync::watch::channel(false);

        hb_stop_tx
            .send(true)
            .expect("watch receiver should accept shutdown signal");

        let changed = tokio::time::timeout(Duration::from_millis(50), hb_stop_rx.changed())
            .await
            .expect("shutdown signal should already be visible");
        assert!(changed.is_ok(), "watch channel should remain open");
        assert!(*hb_stop_rx.borrow(), "shutdown value should be true");
    }

    #[test]
    fn grpc_job_mapping_roundtrip() {
        let now = Utc::now();
        let mut env = std::collections::HashMap::new();
        env.insert("K".to_string(), "V".to_string());
        let job = Job {
            id: uuid::Uuid::new_v4(),
            work_item_id: uuid::Uuid::new_v4(),
            step_id: "step1".into(),
            state: JobState::Assigned,
            assigned_worker_id: Some("w1".into()),
            execution_mode: ExecutionMode::Docker,
            container_image: Some("alpine:3.20".into()),
            command: "echo".into(),
            args: vec!["hi".into()],
            env,
            prompt: Some(protocol::PromptRef {
                system: None,
                user: "u".into(),
            }),
            context: serde_json::json!({"x":1}),
            repo: Some(protocol::RepoSource {
                repo_url: "https://example/repo.git".into(),
                base_ref: "origin/main".into(),
                branch_name: Some("agent-hub/abc".into()),
            }),
            timeout_secs: Some(10),
            max_retries: 2,
            attempt: 1,
            transcript_dir: Some("/tmp/t".into()),
            result: None,
            lease_expires_at: Some(now),
            created_at: now,
            updated_at: now,
        };

        let pb = job_to_pb(job.clone());
        let decoded = job_from_pb(pb).expect("decode");
        assert_eq!(decoded.id, job.id);
        assert_eq!(decoded.execution_mode, job.execution_mode);
        assert_eq!(decoded.max_retries, 2);
    }

    #[test]
    fn parse_github_https_repo_slug() {
        assert_eq!(
            normalize_github_repo_slug("https://github.com/reddit/agent-hub.git"),
            Some("reddit/agent-hub".to_string())
        );
    }

    #[test]
    fn parse_github_ssh_repo_slug() {
        assert_eq!(
            normalize_github_repo_slug("git@github.com:reddit/agent-hub.git"),
            Some("reddit/agent-hub".to_string())
        );
    }

    #[test]
    fn parse_github_pr_contract_from_output_json() {
        let parsed = parse_github_pr_from_output(&serde_json::json!({
            "github_pr": {
                "number": 123,
                "url": "https://github.com/reddit/agent-hub/pull/123",
                "state": "open",
                "is_draft": false,
                "head_ref_name": "agent-hub/test",
                "base_ref_name": "main"
            }
        }))
        .expect("should parse github_pr contract");
        assert_eq!(parsed.number, 123);
        assert_eq!(parsed.state.as_deref(), Some("open"));
        assert_eq!(parsed.base_ref_name.as_deref(), Some("main"));
    }

    #[test]
    fn collect_structured_transcript_artifacts_marks_agent_context_and_optional_jsonl() {
        let root =
            std::env::temp_dir().join(format!("structured-artifacts-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).expect("mkdir root");
        let agent_context_path = root.join(".agent-context.json");
        let transcript_jsonl_path = root.join("transcript.jsonl");
        let job_events_path = root.join("job-events.jsonl");
        let run_json_path = root.join("run.json");

        std::fs::write(&agent_context_path, "{\"schema\":\"agent_context_v1\"}\n")
            .expect("write agent context");
        let artifacts = collect_structured_transcript_artifacts(
            &agent_context_path,
            &run_json_path,
            &transcript_jsonl_path,
            &job_events_path,
        );
        assert_eq!(artifacts.len(), 1);
        assert_eq!(artifacts[0].key, "agent_context");
        assert!(artifacts[0].bytes.is_some());
        assert_eq!(artifacts[0].schema.as_deref(), Some("agent_context_v1"));
        assert_eq!(artifacts[0].record_count, None);
        assert_eq!(
            artifacts[0]
                .inline_json
                .as_ref()
                .and_then(|v| v.get("schema"))
                .and_then(serde_json::Value::as_str),
            Some("agent_context_v1")
        );

        std::fs::write(&transcript_jsonl_path, "{\"event\":\"x\"}\n").expect("write jsonl");
        let artifacts = collect_structured_transcript_artifacts(
            &agent_context_path,
            &run_json_path,
            &transcript_jsonl_path,
            &job_events_path,
        );
        assert_eq!(artifacts.len(), 2);
        let agent_context = artifacts
            .iter()
            .find(|a| a.key == "agent_context")
            .expect("agent_context artifact");
        assert_eq!(agent_context.schema.as_deref(), Some("agent_context_v1"));
        assert_eq!(agent_context.record_count, None);

        let transcript_jsonl = artifacts
            .iter()
            .find(|a| a.key == "transcript_jsonl")
            .expect("transcript_jsonl artifact");
        assert_eq!(transcript_jsonl.schema, None);
        assert_eq!(transcript_jsonl.record_count, Some(1));
        let inline = transcript_jsonl
            .inline_json
            .as_ref()
            .and_then(serde_json::Value::as_array)
            .expect("transcript_jsonl inline tail array");
        assert_eq!(inline.len(), 1);
        assert_eq!(inline[0].as_str(), Some("{\"event\":\"x\"}"));

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn collect_structured_transcript_artifacts_skips_inline_for_unknown_agent_context_schema() {
        let root = std::env::temp_dir().join(format!(
            "structured-artifacts-schema-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).expect("mkdir root");
        let agent_context_path = root.join(".agent-context.json");
        let transcript_jsonl_path = root.join("transcript.jsonl");
        let job_events_path = root.join("job-events.jsonl");
        let run_json_path = root.join("run.json");

        std::fs::write(&agent_context_path, "{\"schema\":\"agent_context_v2\"}\n")
            .expect("write agent context");
        let artifacts = collect_structured_transcript_artifacts(
            &agent_context_path,
            &run_json_path,
            &transcript_jsonl_path,
            &job_events_path,
        );
        assert_eq!(artifacts.len(), 1);
        assert_eq!(artifacts[0].key, "agent_context");
        assert!(artifacts[0].inline_json.is_none());

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn collect_structured_transcript_artifacts_includes_job_events_bounded_inline_tail() {
        let root = std::env::temp_dir().join(format!(
            "structured-artifacts-job-events-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).expect("mkdir root");
        let agent_context_path = root.join(".agent-context.json");
        let transcript_jsonl_path = root.join("transcript.jsonl");
        let job_events_path = root.join("job-events.jsonl");
        let run_json_path = root.join("run.json");

        let first_job_id = uuid::Uuid::new_v4();
        let second_job_id = uuid::Uuid::new_v4();
        let payload = format!(
            "{{\"job_id\":\"{first_job_id}\",\"worker_id\":\"w1\",\"at\":\"{}\",\"kind\":\"system\",\"message\":\"first\"}}\n{{\"job_id\":\"{second_job_id}\",\"worker_id\":\"w1\",\"at\":\"{}\",\"kind\":\"stdout\",\"stream\":\"stdout\",\"message\":\"second\"}}\n",
            Utc::now().to_rfc3339(),
            Utc::now().to_rfc3339(),
        );
        std::fs::write(&job_events_path, payload).expect("write job events");

        let artifacts = collect_structured_transcript_artifacts(
            &agent_context_path,
            &run_json_path,
            &transcript_jsonl_path,
            &job_events_path,
        );
        assert_eq!(artifacts.len(), 1);
        let job_events = artifacts.first().expect("job_events artifact");
        assert_eq!(job_events.key, "job_events");
        assert_eq!(
            job_events.schema.as_deref(),
            Some(protocol::JOB_EVENT_SCHEMA_NAME)
        );
        assert_eq!(job_events.bytes, file_len_u64(&job_events_path));
        assert_eq!(job_events.record_count, Some(2));
        let inline = job_events
            .inline_json
            .as_ref()
            .and_then(serde_json::Value::as_array)
            .expect("job_events inline array");
        assert_eq!(inline.len(), 2);
        assert_eq!(
            inline[0].get("job_id").and_then(serde_json::Value::as_str),
            Some(first_job_id.to_string().as_str())
        );
        assert_eq!(
            inline[1].get("kind").and_then(serde_json::Value::as_str),
            Some("stdout")
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn collect_structured_transcript_artifacts_job_events_inline_tail_skips_malformed_lines() {
        let root = std::env::temp_dir().join(format!(
            "structured-artifacts-job-events-malformed-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).expect("mkdir root");
        let agent_context_path = root.join(".agent-context.json");
        let transcript_jsonl_path = root.join("transcript.jsonl");
        let job_events_path = root.join("job-events.jsonl");
        let run_json_path = root.join("run.json");

        let valid_event_a = serde_json::json!({
            "job_id": uuid::Uuid::new_v4(),
            "worker_id": "w1",
            "at": Utc::now(),
            "kind": "system",
            "message": "ok-a"
        });
        let valid_event_b = serde_json::json!({
            "job_id": uuid::Uuid::new_v4(),
            "worker_id": "w1",
            "at": Utc::now(),
            "kind": "stderr",
            "stream": "stderr",
            "message": "ok-b"
        });
        let payload = format!(
            "{}\nnot-json\n{}\n{{\"job_id\":\"{}\"}}\n",
            serde_json::to_string(&valid_event_a).expect("serialize event a"),
            serde_json::to_string(&valid_event_b).expect("serialize event b"),
            uuid::Uuid::new_v4(),
        );
        std::fs::write(&job_events_path, payload).expect("write job events");

        let artifacts = collect_structured_transcript_artifacts(
            &agent_context_path,
            &run_json_path,
            &transcript_jsonl_path,
            &job_events_path,
        );
        let job_events = artifacts.first().expect("job_events artifact");
        assert_eq!(job_events.record_count, Some(4));
        let inline = job_events
            .inline_json
            .as_ref()
            .and_then(serde_json::Value::as_array)
            .expect("job_events inline array");
        assert_eq!(inline.len(), 2);
        assert_eq!(
            inline[0].get("message").and_then(serde_json::Value::as_str),
            Some("ok-a")
        );
        assert_eq!(
            inline[1].get("message").and_then(serde_json::Value::as_str),
            Some("ok-b")
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn inline_job_events_json_tail_respects_total_byte_budget() {
        let root = std::env::temp_dir().join(format!(
            "structured-artifacts-job-events-budget-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).expect("mkdir root");
        let job_events_path = root.join("job-events.jsonl");

        let mk_event = |message: &str| {
            serde_json::json!({
                "job_id": uuid::Uuid::new_v4(),
                "worker_id": "w1",
                "at": Utc::now(),
                "kind": "system",
                "message": message,
            })
        };
        let payload = format!(
            "{}\n{}\n{}\n",
            serde_json::to_string(&mk_event("small-a")).expect("serialize a"),
            serde_json::to_string(&mk_event(&"x".repeat(2048))).expect("serialize large"),
            serde_json::to_string(&mk_event("small-b")).expect("serialize b"),
        );
        std::fs::write(&job_events_path, payload).expect("write job events");

        let inline = inline_job_events_json_tail(&job_events_path, 20, 512)
            .and_then(|v| v.as_array().cloned())
            .expect("inline tail array");
        assert_eq!(inline.len(), 1);
        assert_eq!(
            inline[0].get("message").and_then(serde_json::Value::as_str),
            Some("small-b")
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn inline_transcript_jsonl_tail_respects_record_and_byte_budgets() {
        let root = std::env::temp_dir().join(format!(
            "structured-artifacts-transcript-inline-budget-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).expect("mkdir root");
        let transcript_jsonl_path = root.join("transcript.jsonl");

        std::fs::write(
            &transcript_jsonl_path,
            format!("line-a\n{}\nline-b\nline-c\n", "x".repeat(2048)),
        )
        .expect("write transcript jsonl");

        let inline = inline_transcript_jsonl_tail(&transcript_jsonl_path, 2, 64)
            .and_then(|v| v.as_array().cloned())
            .expect("inline tail array");
        assert_eq!(inline.len(), 2);
        assert_eq!(inline[0].as_str(), Some("line-b"));
        assert_eq!(inline[1].as_str(), Some("line-c"));

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn collect_structured_transcript_artifacts_includes_run_json_with_schema_gated_inline() {
        let root = std::env::temp_dir().join(format!(
            "structured-artifacts-run-json-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).expect("mkdir root");
        let agent_context_path = root.join(".agent-context.json");
        let run_json_path = root.join("run.json");
        let transcript_jsonl_path = root.join("transcript.jsonl");
        let job_events_path = root.join("job-events.jsonl");

        std::fs::write(
            &run_json_path,
            serde_json::to_vec(&serde_json::json!({
                "schema": protocol::JOB_RUN_SCHEMA_NAME,
                "job_id": uuid::Uuid::new_v4(),
                "workspace": "/workspace"
            }))
            .expect("serialize run payload"),
        )
        .expect("write run json");

        let artifacts = collect_structured_transcript_artifacts(
            &agent_context_path,
            &run_json_path,
            &transcript_jsonl_path,
            &job_events_path,
        );
        assert_eq!(artifacts.len(), 1);
        let run_json = artifacts.first().expect("run_json artifact");
        assert_eq!(run_json.key, "run_json");
        assert_eq!(
            run_json.schema.as_deref(),
            Some(protocol::JOB_RUN_SCHEMA_NAME)
        );
        assert_eq!(run_json.bytes, file_len_u64(&run_json_path));
        assert_eq!(
            run_json
                .inline_json
                .as_ref()
                .and_then(|v| v.get("schema"))
                .and_then(serde_json::Value::as_str),
            Some(protocol::JOB_RUN_SCHEMA_NAME)
        );

        std::fs::write(
            &run_json_path,
            serde_json::to_vec(&serde_json::json!({
                "schema": "job_run_v2",
                "job_id": uuid::Uuid::new_v4()
            }))
            .expect("serialize unknown run payload"),
        )
        .expect("overwrite run json");
        let artifacts = collect_structured_transcript_artifacts(
            &agent_context_path,
            &run_json_path,
            &transcript_jsonl_path,
            &job_events_path,
        );
        let run_json = artifacts.first().expect("run_json artifact");
        assert!(run_json.inline_json.is_none());

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn run_raw_wait_strategy_does_not_depend_on_second_command_spawn() {
        // Regression guard for no-timeout branch: command must execute once.
        let rt = tokio::runtime::Runtime::new().expect("rt");
        rt.block_on(async {
            let root = std::env::temp_dir().join(format!("run-raw-once-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&root).expect("mkdir root");
            let workspace = root.join("workspace");
            std::fs::create_dir_all(&workspace).expect("mkdir workspace");
            let transcript = root.join("transcript");
            std::fs::create_dir_all(&transcript).expect("mkdir transcript");

            let counter = workspace.join("count.txt");
            let script = format!(
                "if [ -f \"{}\" ]; then n=$(cat \"{}\"); else n=0; fi; n=$((n+1)); echo $n > \"{}\"",
                counter.display(),
                counter.display(),
                counter.display()
            );

            let now = Utc::now();
            let job = Job {
                id: uuid::Uuid::new_v4(),
                work_item_id: uuid::Uuid::new_v4(),
                step_id: "s".into(),
                state: JobState::Assigned,
                assigned_worker_id: Some("w1".into()),
                execution_mode: ExecutionMode::Raw,
                container_image: None,
                command: "sh".into(),
                args: vec!["-lc".into(), script],
                env: std::collections::HashMap::new(),
                prompt: None,
                context: serde_json::json!({}),
                repo: None,
                timeout_secs: None,
                max_retries: 0,
                attempt: 0,
                transcript_dir: None,
                result: None,
                lease_expires_at: None,
                created_at: now,
                updated_at: now,
            };

            let status = run_raw(
                &job,
                &workspace,
                &transcript.join("transcript.jsonl"),
                &transcript.join("stdout.log"),
                &transcript.join("stderr.log"),
                &std::collections::HashMap::new(),
                None,
                None,
            )
            .await
            .expect("run_raw");

            assert!(matches!(status, ExecutionStatus::Exited(0)));
            let n = std::fs::read_to_string(&counter).expect("counter exists");
            assert_eq!(n.trim(), "1");

            let _ = std::fs::remove_dir_all(root);
        });
    }

    #[test]
    fn branch_and_worktree_are_attempt_scoped() {
        let now = Utc::now();
        let job = Job {
            id: uuid::Uuid::new_v4(),
            work_item_id: uuid::Uuid::new_v4(),
            step_id: "step".into(),
            state: JobState::Pending,
            assigned_worker_id: None,
            execution_mode: ExecutionMode::Raw,
            container_image: None,
            command: "echo".into(),
            args: vec![],
            env: std::collections::HashMap::new(),
            prompt: None,
            context: serde_json::json!({}),
            repo: None,
            timeout_secs: None,
            max_retries: 0,
            attempt: 2,
            transcript_dir: None,
            result: None,
            lease_expires_at: None,
            created_at: now,
            updated_at: now,
        };

        let branch = branch_for_job_attempt(&job, Some("agent-hub/manual".to_string()));
        assert!(branch.contains("agent-hub/manual"));
        assert!(branch.contains("-a2"));

        let root = std::env::temp_dir().join(format!("wt-root-{}", uuid::Uuid::new_v4()));
        let wt = worktree_dir_for_job_attempt(&root, &job);
        let wt_str = wt.to_string_lossy();
        assert!(wt_str.contains(&job.work_item_id.to_string()));
        assert!(wt_str.contains("-a2"));
    }

    #[test]
    fn preferred_checkout_ref_from_job_results_prefers_matching_base_ref_head_sha() {
        let repo = protocol::RepoSource {
            repo_url: "https://example/repo.git".to_string(),
            base_ref: "origin/main".to_string(),
            branch_name: None,
        };

        let context = serde_json::json!({
            "job_results": {
                "impl": {
                    "outcome": "succeeded",
                    "repo": {
                        "base_ref": "origin/main",
                        "target_branch": "agent-hub/feature-a",
                        "head_sha": "abc123"
                    }
                },
                "other": {
                    "outcome": "succeeded",
                    "repo": {
                        "base_ref": "origin/release",
                        "head_sha": "def456"
                    }
                }
            }
        });

        let selected = preferred_checkout_ref_from_job_results(&context, &repo);
        assert_eq!(selected.as_deref(), Some("abc123"));
    }

    #[test]
    fn preferred_checkout_ref_from_job_results_falls_back_to_any_successful_target_branch() {
        let repo = protocol::RepoSource {
            repo_url: "https://example/repo.git".to_string(),
            base_ref: "origin/main".to_string(),
            branch_name: None,
        };

        let context = serde_json::json!({
            "job_results": {
                "impl": {
                    "outcome": "succeeded",
                    "repo": {
                        "base_ref": "origin/release",
                        "target_branch": "agent-hub/feature-b"
                    }
                },
                "failed_step": {
                    "outcome": "failed",
                    "repo": {
                        "head_sha": "not-used"
                    }
                }
            }
        });

        let selected = preferred_checkout_ref_from_job_results(&context, &repo);
        assert_eq!(selected.as_deref(), Some("agent-hub/feature-b"));
    }

    #[test]
    fn checkout_ref_for_repo_job_falls_back_to_repo_base_ref() {
        let now = Utc::now();
        let job = Job {
            id: uuid::Uuid::new_v4(),
            work_item_id: uuid::Uuid::new_v4(),
            step_id: "step".into(),
            state: JobState::Pending,
            assigned_worker_id: None,
            execution_mode: ExecutionMode::Raw,
            container_image: None,
            command: "echo".into(),
            args: vec![],
            env: std::collections::HashMap::new(),
            prompt: None,
            context: serde_json::json!({"job_results": {"impl": {"outcome": "failed"}}}),
            repo: None,
            timeout_secs: None,
            max_retries: 0,
            attempt: 0,
            transcript_dir: None,
            result: None,
            lease_expires_at: None,
            created_at: now,
            updated_at: now,
        };
        let repo = protocol::RepoSource {
            repo_url: "https://example/repo.git".to_string(),
            base_ref: "origin/main".to_string(),
            branch_name: None,
        };

        let selected = checkout_ref_for_repo_job(&job, &repo);
        assert_eq!(selected, "origin/main");
    }

    #[test]
    fn local_agent_egress_plan_maps_llm_only_to_llm_tier() {
        let runtime = protocol::CanonicalRuntimeSpec {
            runtime_version: "v2".to_string(),
            environment: protocol::PlannedEnvironmentSpec {
                tier: protocol::PlannedEnvironmentTier::Docker,
                execution_mode: ExecutionMode::Docker,
                container_image: None,
                timeout_secs: None,
                memory_limit: None,
                cpu_limit: None,
                network: None,
                network_resources: Some(NetworkResourceRequest::Resources(vec![
                    NetworkResourceKind::Llm,
                ])),
                filesystem: None,
                file_mounts: vec![],
                toolchains: vec![],
            },
            permissions: protocol::PlannedPermissionSet {
                tier: protocol::PlannedPermissionTier::DockerDefault,
                can_read_workspace: true,
                can_write_workspace: true,
                can_run_shell: true,
                can_access_network: true,
                auto_approve_tools: vec![],
                escalate_tools: vec![],
                kubernetes_access: None,
                aws_access: None,
            },
            approval_rules: None,
            repos: vec![],
        };

        let plan = resolve_local_agent_egress_plan(&runtime).expect("egress plan");
        assert_eq!(plan.tier, Some(LocalAgentEgressTier::Llm));
        assert!(!plan.network_none);
    }

    #[test]
    fn local_agent_egress_plan_maps_build_resources_to_build_tier() {
        let runtime = protocol::CanonicalRuntimeSpec {
            runtime_version: "v2".to_string(),
            environment: protocol::PlannedEnvironmentSpec {
                tier: protocol::PlannedEnvironmentTier::Docker,
                execution_mode: ExecutionMode::Docker,
                container_image: None,
                timeout_secs: None,
                memory_limit: None,
                cpu_limit: None,
                network: None,
                network_resources: Some(NetworkResourceRequest::Resources(vec![
                    NetworkResourceKind::Github,
                    NetworkResourceKind::CratesRegistry,
                ])),
                filesystem: None,
                file_mounts: vec![],
                toolchains: vec![],
            },
            permissions: protocol::PlannedPermissionSet {
                tier: protocol::PlannedPermissionTier::DockerDefault,
                can_read_workspace: true,
                can_write_workspace: true,
                can_run_shell: true,
                can_access_network: true,
                auto_approve_tools: vec![],
                escalate_tools: vec![],
                kubernetes_access: None,
                aws_access: None,
            },
            approval_rules: None,
            repos: vec![],
        };

        let plan = resolve_local_agent_egress_plan(&runtime).expect("egress plan");
        assert_eq!(plan.tier, Some(LocalAgentEgressTier::Build));
        assert!(!plan.network_none);
    }

    #[test]
    fn local_agent_egress_plan_rejects_unsupported_resources() {
        let runtime = protocol::CanonicalRuntimeSpec {
            runtime_version: "v2".to_string(),
            environment: protocol::PlannedEnvironmentSpec {
                tier: protocol::PlannedEnvironmentTier::Docker,
                execution_mode: ExecutionMode::Docker,
                container_image: None,
                timeout_secs: None,
                memory_limit: None,
                cpu_limit: None,
                network: None,
                network_resources: Some(NetworkResourceRequest::Resources(vec![
                    NetworkResourceKind::Llm,
                    NetworkResourceKind::Github,
                    NetworkResourceKind::GoRegistry,
                    NetworkResourceKind::CratesRegistry,
                ])),
                filesystem: None,
                file_mounts: vec![],
                toolchains: vec![],
            },
            permissions: protocol::PlannedPermissionSet {
                tier: protocol::PlannedPermissionTier::DockerDefault,
                can_read_workspace: true,
                can_write_workspace: true,
                can_run_shell: true,
                can_access_network: true,
                auto_approve_tools: vec![],
                escalate_tools: vec![],
                kubernetes_access: None,
                aws_access: None,
            },
            approval_rules: None,
            repos: vec![],
        };

        let plan = resolve_local_agent_egress_plan(&runtime).expect("egress plan");
        assert_eq!(plan.tier, Some(LocalAgentEgressTier::Build));
        assert!(!plan.network_none);

        let full_runtime = protocol::CanonicalRuntimeSpec {
            runtime_version: "v2".to_string(),
            environment: protocol::PlannedEnvironmentSpec {
                tier: protocol::PlannedEnvironmentTier::Docker,
                execution_mode: ExecutionMode::Docker,
                container_image: None,
                timeout_secs: None,
                memory_limit: None,
                cpu_limit: None,
                network: None,
                network_resources: Some(NetworkResourceRequest::Full),
                filesystem: None,
                file_mounts: vec![],
                toolchains: vec![],
            },
            permissions: protocol::PlannedPermissionSet {
                tier: protocol::PlannedPermissionTier::DockerDefault,
                can_read_workspace: true,
                can_write_workspace: true,
                can_run_shell: true,
                can_access_network: true,
                auto_approve_tools: vec![],
                escalate_tools: vec![],
                kubernetes_access: None,
                aws_access: None,
            },
            approval_rules: None,
            repos: vec![],
        };

        let full_plan = resolve_local_agent_egress_plan(&full_runtime).expect("full egress plan");
        assert_eq!(full_plan.tier, Some(LocalAgentEgressTier::Full));
        assert!(!full_plan.network_none);

        let invalid_runtime = protocol::CanonicalRuntimeSpec {
            runtime_version: "v2".to_string(),
            environment: protocol::PlannedEnvironmentSpec {
                tier: protocol::PlannedEnvironmentTier::Docker,
                execution_mode: ExecutionMode::Docker,
                container_image: None,
                timeout_secs: None,
                memory_limit: None,
                cpu_limit: None,
                network: Some("internet".to_string()),
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
                can_access_network: true,
                auto_approve_tools: vec![],
                escalate_tools: vec![],
                kubernetes_access: None,
                aws_access: None,
            },
            approval_rules: None,
            repos: vec![],
        };
        assert!(resolve_local_agent_egress_plan(&invalid_runtime).is_err());
    }

    #[test]
    fn mirror_refresh_stamp_gates_fetch_frequency() {
        let root = std::env::temp_dir().join(format!("mirror-stamp-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).expect("mkdir");

        assert!(should_refresh_mirror(&root).expect("initial refresh check"));
        write_mirror_refresh_timestamp(&root).expect("write stamp");
        assert!(!should_refresh_mirror(&root).expect("gated refresh check"));

        let stale_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
            .saturating_sub(MIRROR_REFRESH_MIN_SECS + 5);
        std::fs::write(mirror_refresh_stamp_path(&root), stale_secs.to_string())
            .expect("write stale stamp");
        assert!(should_refresh_mirror(&root).expect("stale refresh check"));

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn cleanup_workspace_deletes_attempt_branch_best_effort() {
        let rt = tokio::runtime::Runtime::new().expect("rt");
        rt.block_on(async {
            let root =
                std::env::temp_dir().join(format!("cleanup-branch-{}", uuid::Uuid::new_v4()));
            let mirror = root.join("mirror");
            let source = root.join("source");
            let worktrees_root = root.join("worktrees");
            std::fs::create_dir_all(&source).expect("mkdir source");
            std::fs::create_dir_all(&worktrees_root).expect("mkdir worktrees");

            let init = std::process::Command::new("git")
                .arg("init")
                .arg(&source)
                .status()
                .expect("git init source");
            assert!(init.success());

            let cfg_name = std::process::Command::new("git")
                .arg("-C")
                .arg(&source)
                .arg("config")
                .arg("user.name")
                .arg("Agent Hub")
                .status()
                .expect("git config name");
            assert!(cfg_name.success());
            let cfg_email = std::process::Command::new("git")
                .arg("-C")
                .arg(&source)
                .arg("config")
                .arg("user.email")
                .arg("agent-hub@example.com")
                .status()
                .expect("git config email");
            assert!(cfg_email.success());

            std::fs::write(source.join("README.md"), "hello\n").expect("write readme");
            let add = std::process::Command::new("git")
                .arg("-C")
                .arg(&source)
                .arg("add")
                .arg("README.md")
                .status()
                .expect("git add");
            assert!(add.success());
            let commit = std::process::Command::new("git")
                .arg("-C")
                .arg(&source)
                .arg("commit")
                .arg("-m")
                .arg("init")
                .status()
                .expect("git commit");
            assert!(commit.success());

            let clone = std::process::Command::new("git")
                .arg("clone")
                .arg(&source)
                .arg(&mirror)
                .status()
                .expect("git clone mirror");
            assert!(clone.success());

            let now = Utc::now();
            let job = Job {
                id: uuid::Uuid::new_v4(),
                work_item_id: uuid::Uuid::new_v4(),
                step_id: "s".into(),
                state: JobState::Assigned,
                assigned_worker_id: Some("w1".into()),
                execution_mode: ExecutionMode::Raw,
                container_image: None,
                command: "echo".into(),
                args: vec!["ok".into()],
                env: std::collections::HashMap::new(),
                prompt: None,
                context: serde_json::json!({}),
                repo: Some(protocol::RepoSource {
                    repo_url: source.to_string_lossy().to_string(),
                    base_ref: "HEAD".into(),
                    branch_name: Some("agent-hub/manual".into()),
                }),
                timeout_secs: None,
                max_retries: 0,
                attempt: 1,
                transcript_dir: None,
                result: None,
                lease_expires_at: None,
                created_at: now,
                updated_at: now,
            };

            let workspace_dir = worktree_dir_for_job_attempt(&worktrees_root, &job);
            let attempt_branch = branch_for_job_attempt(&job, Some("agent-hub/manual".into()));
            let add_wt = std::process::Command::new("git")
                .arg("-C")
                .arg(&mirror)
                .arg("worktree")
                .arg("add")
                .arg("-B")
                .arg(&attempt_branch)
                .arg(&workspace_dir)
                .arg("HEAD")
                .status()
                .expect("git worktree add");
            assert!(add_wt.success());

            cleanup_workspace(
                &job,
                &PreparedWorkspace {
                    workspace_dir: workspace_dir.clone(),
                    mirror_dir: Some(mirror.clone()),
                    attempt_branch: Some(attempt_branch.clone()),
                    additional_repo_mounts: Vec::new(),
                },
                None,
            )
            .await
            .expect("cleanup");

            let branch_list = std::process::Command::new("git")
                .arg("-C")
                .arg(&mirror)
                .arg("branch")
                .arg("--list")
                .arg(&attempt_branch)
                .output()
                .expect("git branch list");
            assert!(branch_list.status.success());
            let listed = String::from_utf8(branch_list.stdout).expect("utf8 branch output");
            assert!(listed.trim().is_empty());

            let _ = std::fs::remove_dir_all(root);
        });
    }

    #[test]
    fn runtime_spec_from_context_drives_execution_mode_and_timeout() {
        let runtime = protocol::CanonicalRuntimeSpec {
            runtime_version: "v2".to_string(),
            environment: protocol::PlannedEnvironmentSpec {
                tier: protocol::PlannedEnvironmentTier::Docker,
                execution_mode: ExecutionMode::Docker,
                container_image: Some("alpine:3.20".into()),
                timeout_secs: Some(77),
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
        let mut ctx = serde_json::json!({});
        protocol::project_runtime_spec_into_job_context(&mut ctx, &runtime);

        let parsed = protocol::canonical_runtime_spec_for_job_effective(&Job {
            id: uuid::Uuid::new_v4(),
            work_item_id: uuid::Uuid::new_v4(),
            step_id: "s".into(),
            state: JobState::Pending,
            assigned_worker_id: None,
            execution_mode: ExecutionMode::Raw,
            container_image: None,
            command: "echo".into(),
            args: vec![],
            env: std::collections::HashMap::new(),
            prompt: None,
            context: ctx,
            repo: None,
            timeout_secs: None,
            max_retries: 0,
            attempt: 0,
            transcript_dir: None,
            result: None,
            lease_expires_at: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        });
        assert_eq!(parsed.environment.execution_mode, ExecutionMode::Docker);
        assert_eq!(parsed.environment.timeout_secs, Some(77));
        assert_eq!(parsed.permissions.can_access_network, false);
    }

    #[test]
    fn write_agent_context_file_includes_standard_worker_local_paths() {
        let root =
            std::env::temp_dir().join(format!("agent-context-paths-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).expect("mkdir root");

        let transcript_dir = root.join("transcript");
        std::fs::create_dir_all(&transcript_dir).expect("mkdir transcript");
        let workspace_dir = root.join("workspace");
        std::fs::create_dir_all(&workspace_dir).expect("mkdir workspace");

        let now = Utc::now();
        let job = Job {
            id: uuid::Uuid::new_v4(),
            work_item_id: uuid::Uuid::new_v4(),
            step_id: "step".into(),
            state: JobState::Assigned,
            assigned_worker_id: Some("w1".into()),
            execution_mode: ExecutionMode::Raw,
            container_image: None,
            command: "echo".into(),
            args: vec!["ok".into()],
            env: std::collections::HashMap::new(),
            prompt: None,
            context: serde_json::json!({}),
            repo: None,
            timeout_secs: Some(30),
            max_retries: 1,
            attempt: 0,
            transcript_dir: None,
            result: None,
            lease_expires_at: None,
            created_at: now,
            updated_at: now,
        };

        let runtime = protocol::CanonicalRuntimeSpec {
            runtime_version: "v2".to_string(),
            environment: protocol::PlannedEnvironmentSpec {
                tier: protocol::PlannedEnvironmentTier::Raw,
                execution_mode: ExecutionMode::Raw,
                container_image: None,
                timeout_secs: Some(30),
                memory_limit: None,
                cpu_limit: None,
                network: None,
                network_resources: None,
                filesystem: None,
                file_mounts: vec![],
                toolchains: vec![],
            },
            permissions: protocol::PlannedPermissionSet {
                tier: protocol::PlannedPermissionTier::RawCompatible,
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

        let agent_context_path = transcript_dir.join(".agent-context.json");
        write_agent_context_file(
            &agent_context_path,
            &job,
            &transcript_dir.to_string_lossy(),
            &workspace_dir,
            &runtime,
            &[],
        )
        .expect("write agent context");

        let parsed: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&agent_context_path).expect("read context"))
                .expect("parse context");
        let paths = parsed
            .get("paths")
            .and_then(serde_json::Value::as_object)
            .expect("paths object");

        assert_eq!(
            paths
                .get("job_events_jsonl")
                .and_then(serde_json::Value::as_str),
            Some(
                transcript_dir
                    .join("job-events.jsonl")
                    .to_string_lossy()
                    .as_ref()
            )
        );
        assert_eq!(
            paths.get("stdout_log").and_then(serde_json::Value::as_str),
            Some(transcript_dir.join("stdout.log").to_string_lossy().as_ref())
        );
        assert_eq!(
            paths.get("stderr_log").and_then(serde_json::Value::as_str),
            Some(transcript_dir.join("stderr.log").to_string_lossy().as_ref())
        );
        assert_eq!(
            paths.get("run_json").and_then(serde_json::Value::as_str),
            Some(transcript_dir.join("run.json").to_string_lossy().as_ref())
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn write_agent_context_file_uses_container_paths_for_docker_jobs() {
        let root = std::env::temp_dir().join(format!(
            "agent-context-docker-paths-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).expect("mkdir root");

        let transcript_dir = root.join("transcript");
        std::fs::create_dir_all(&transcript_dir).expect("mkdir transcript");
        let workspace_dir = root.join("workspace");
        std::fs::create_dir_all(&workspace_dir).expect("mkdir workspace");

        let now = Utc::now();
        let job = Job {
            id: uuid::Uuid::new_v4(),
            work_item_id: uuid::Uuid::new_v4(),
            step_id: "step".into(),
            state: JobState::Assigned,
            assigned_worker_id: Some("w1".into()),
            execution_mode: ExecutionMode::Docker,
            container_image: Some("alpine:3.20".into()),
            command: "echo".into(),
            args: vec!["ok".into()],
            env: std::collections::HashMap::new(),
            prompt: None,
            context: serde_json::json!({}),
            repo: None,
            timeout_secs: Some(30),
            max_retries: 1,
            attempt: 0,
            transcript_dir: None,
            result: None,
            lease_expires_at: None,
            created_at: now,
            updated_at: now,
        };

        let runtime = protocol::CanonicalRuntimeSpec {
            runtime_version: "v2".to_string(),
            environment: protocol::PlannedEnvironmentSpec {
                tier: protocol::PlannedEnvironmentTier::Docker,
                execution_mode: ExecutionMode::Docker,
                container_image: Some("alpine:3.20".into()),
                timeout_secs: Some(30),
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

        let agent_context_path = transcript_dir.join(".agent-context.json");
        write_agent_context_file(
            &agent_context_path,
            &job,
            &transcript_dir.to_string_lossy(),
            &workspace_dir,
            &runtime,
            &[],
        )
        .expect("write agent context");

        let parsed: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&agent_context_path).expect("read context"))
                .expect("parse context");
        let paths = parsed
            .get("paths")
            .and_then(serde_json::Value::as_object)
            .expect("paths object");

        assert_eq!(
            paths
                .get("workspace_dir")
                .and_then(serde_json::Value::as_str),
            Some("/workspace")
        );
        assert_eq!(
            paths
                .get("transcript_dir")
                .and_then(serde_json::Value::as_str),
            Some("/transcript")
        );
        assert_eq!(
            paths
                .get("job_events_jsonl")
                .and_then(serde_json::Value::as_str),
            Some("/transcript/job-events.jsonl")
        );
        assert_eq!(
            paths.get("stdout_log").and_then(serde_json::Value::as_str),
            Some("/transcript/stdout.log")
        );
        assert_eq!(
            paths.get("stderr_log").and_then(serde_json::Value::as_str),
            Some("/transcript/stderr.log")
        );
        assert_eq!(
            paths.get("run_json").and_then(serde_json::Value::as_str),
            Some("/transcript/run.json")
        );

        let workflow_inputs = parsed
            .get("workflow")
            .and_then(|v| v.get("inputs"))
            .and_then(serde_json::Value::as_object)
            .expect("workflow.inputs object");
        assert_eq!(
            workflow_inputs
                .get("valid_next_events")
                .and_then(serde_json::Value::as_array)
                .map(Vec::len),
            Some(0)
        );
        assert!(
            workflow_inputs
                .get("job_artifact_paths")
                .and_then(serde_json::Value::as_object)
                .is_some()
        );
        assert!(
            workflow_inputs
                .get("job_artifact_metadata")
                .and_then(serde_json::Value::as_object)
                .is_some()
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn write_agent_context_file_includes_workflow_inputs_from_job_context_projection() {
        let root = std::env::temp_dir().join(format!(
            "agent-context-workflow-inputs-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).expect("mkdir root");

        let transcript_dir = root.join("transcript");
        std::fs::create_dir_all(&transcript_dir).expect("mkdir transcript");
        let workspace_dir = root.join("workspace");
        std::fs::create_dir_all(&workspace_dir).expect("mkdir workspace");

        let now = Utc::now();
        let job = Job {
            id: uuid::Uuid::new_v4(),
            work_item_id: uuid::Uuid::new_v4(),
            step_id: "step".into(),
            state: JobState::Assigned,
            assigned_worker_id: Some("w1".into()),
            execution_mode: ExecutionMode::Raw,
            container_image: None,
            command: "echo".into(),
            args: vec!["ok".into()],
            env: std::collections::HashMap::new(),
            prompt: None,
            context: serde_json::json!({
                "work_item": {
                    "id": "w-123",
                    "title": "Ship context projection",
                    "description": "Carry work-item summary into local agent context",
                    "labels": ["reactive", "phase-1b"],
                    "priority": "P1",
                    "workflow_state": "build",
                    "workflow_id": "wf"
                },
                "workflow": {
                    "inputs": {
                        "valid_next_events": ["submit", "cancel"],
                        "job_artifact_paths": {
                            "build": {
                                "agent_context": "/tmp/x/.agent-context.json"
                            }
                        },
                        "job_artifact_metadata": {
                            "build": {
                                "agent_context": {
                                    "path": "/tmp/x/.agent-context.json",
                                    "schema": protocol::AGENT_CONTEXT_SCHEMA_NAME
                                }
                            }
                        },
                        "job_artifact_data": {
                            "build": {
                                "agent_context": {"schema": "agent_context_v1"}
                            }
                        }
                    }
                }
            }),
            repo: None,
            timeout_secs: Some(30),
            max_retries: 1,
            attempt: 0,
            transcript_dir: None,
            result: None,
            lease_expires_at: None,
            created_at: now,
            updated_at: now,
        };

        let runtime = protocol::CanonicalRuntimeSpec {
            runtime_version: "v2".to_string(),
            environment: protocol::PlannedEnvironmentSpec {
                tier: protocol::PlannedEnvironmentTier::Raw,
                execution_mode: ExecutionMode::Raw,
                container_image: None,
                timeout_secs: Some(30),
                memory_limit: None,
                cpu_limit: None,
                network: None,
                network_resources: None,
                filesystem: None,
                file_mounts: vec![],
                toolchains: vec![],
            },
            permissions: protocol::PlannedPermissionSet {
                tier: protocol::PlannedPermissionTier::RawCompatible,
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

        let agent_context_path = transcript_dir.join(".agent-context.json");
        write_agent_context_file(
            &agent_context_path,
            &job,
            &transcript_dir.to_string_lossy(),
            &workspace_dir,
            &runtime,
            &[],
        )
        .expect("write agent context");

        let parsed: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&agent_context_path).expect("read context"))
                .expect("parse context");

        assert_eq!(
            parsed["workflow"]["inputs"]["valid_next_events"],
            serde_json::json!(["submit", "cancel"])
        );
        assert_eq!(
            parsed["workflow"]["inputs"]["job_artifact_paths"]["build"]["agent_context"],
            serde_json::json!("/tmp/x/.agent-context.json")
        );
        assert_eq!(
            parsed["workflow"]["inputs"]["job_artifact_metadata"]["build"]["agent_context"]["schema"],
            serde_json::json!(protocol::AGENT_CONTEXT_SCHEMA_NAME)
        );
        assert_eq!(
            parsed["workflow"]["valid_next_events"],
            serde_json::json!(["submit", "cancel"])
        );
        assert_eq!(parsed["work_item"]["id"], serde_json::json!("w-123"));
        assert_eq!(
            parsed["work_item"]["title"],
            serde_json::json!("Ship context projection")
        );
        assert_eq!(
            parsed["work_item"]["workflow_state"],
            serde_json::json!("build")
        );
        assert_eq!(
            parsed["artifacts"]["paths"]["build"]["agent_context"],
            serde_json::json!("/tmp/x/.agent-context.json")
        );
        assert_eq!(parsed["repos"]["primary"], serde_json::Value::Null);
        assert_eq!(parsed["repos"]["additional"], serde_json::json!([]));
        assert!(
            parsed["workflow"]["inputs"]
                .get("job_artifact_data")
                .is_none()
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn build_run_json_payload_uses_effective_runtime_and_normalized_docker_paths() {
        let now = Utc::now();
        let job = Job {
            id: uuid::Uuid::new_v4(),
            work_item_id: uuid::Uuid::new_v4(),
            step_id: "step".into(),
            state: JobState::Assigned,
            assigned_worker_id: Some("w1".into()),
            execution_mode: ExecutionMode::Raw,
            container_image: Some("legacy:image".into()),
            command: "echo".into(),
            args: vec!["ok".into()],
            env: std::collections::HashMap::new(),
            prompt: None,
            context: serde_json::json!({}),
            repo: None,
            timeout_secs: Some(10),
            max_retries: 1,
            attempt: 0,
            transcript_dir: None,
            result: None,
            lease_expires_at: None,
            created_at: now,
            updated_at: now,
        };

        let runtime = protocol::CanonicalRuntimeSpec {
            runtime_version: "v2".to_string(),
            environment: protocol::PlannedEnvironmentSpec {
                tier: protocol::PlannedEnvironmentTier::Docker,
                execution_mode: ExecutionMode::Docker,
                container_image: Some("runtime:image".into()),
                timeout_secs: Some(77),
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

        let payload = build_run_json_payload(
            &job,
            &runtime,
            &ExecutionOutcome::Succeeded,
            &TerminationReason::ExitCode,
            Some(0),
            "/workspace",
            "/transcript",
        );

        assert_eq!(
            payload.get("schema").and_then(serde_json::Value::as_str),
            Some(protocol::JOB_RUN_SCHEMA_NAME)
        );
        assert_eq!(
            payload.get("mode").and_then(serde_json::Value::as_str),
            Some("docker")
        );
        assert_eq!(
            payload
                .get("container_image")
                .and_then(serde_json::Value::as_str),
            Some("runtime:image")
        );
        assert_eq!(
            payload
                .get("timeout_secs")
                .and_then(serde_json::Value::as_u64),
            Some(77)
        );
        assert_eq!(
            payload.get("workspace").and_then(serde_json::Value::as_str),
            Some("/workspace")
        );
        assert_eq!(
            payload
                .get("transcript_dir")
                .and_then(serde_json::Value::as_str),
            Some("/transcript")
        );
    }
}

impl HttpClient {
    fn new(base: String, token: Option<Secret>) -> Self {
        Self {
            http: reqwest::Client::new(),
            base,
            token,
        }
    }

    fn auth(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if let Some(token) = &self.token {
            req.bearer_auth(token.expose())
        } else {
            req
        }
    }
}

impl Client {
    async fn register_worker(&self, req: &RegisterWorkerRequest) -> anyhow::Result<()> {
        match self {
            Client::Http(client) => {
                let url = format!("{}/api/v1/orchestration/workers", client.base);
                let resp = client.auth(client.http.post(url)).json(req).send().await?;
                if !resp.status().is_success() {
                    anyhow::bail!("register failed: {}", resp.status());
                }
                Ok(())
            }
            Client::Grpc(client) => {
                let mut inner = client.client.clone();
                let req = apply_grpc_bearer(
                    client.token.as_ref(),
                    tonic::Request::new(pb::RegisterWorkerRequest {
                        worker_id: req.id.clone(),
                        hostname: req.hostname.clone(),
                        supported_modes: req
                            .supported_modes
                            .iter()
                            .cloned()
                            .map(execution_mode_to_pb)
                            .collect(),
                    }),
                )?;
                inner.register_worker(req).await?;
                Ok(())
            }
        }
    }

    async fn heartbeat(&self, id: &str, state: WorkerState) -> anyhow::Result<()> {
        match self {
            Client::Http(client) => {
                let url = format!(
                    "{}/api/v1/orchestration/workers/{id}/heartbeat",
                    client.base
                );
                let resp = client
                    .auth(client.http.post(url))
                    .json(&HeartbeatRequest { state })
                    .send()
                    .await?;
                if !resp.status().is_success() {
                    anyhow::bail!("heartbeat failed: {}", resp.status());
                }
                Ok(())
            }
            Client::Grpc(client) => {
                let mut inner = client.client.clone();
                let req = apply_grpc_bearer(
                    client.token.as_ref(),
                    tonic::Request::new(pb::HeartbeatWorkerRequest {
                        worker_id: id.to_string(),
                        state: worker_state_to_pb(state),
                    }),
                )?;
                inner.heartbeat_worker(req).await?;
                Ok(())
            }
        }
    }

    async fn poll(&self, worker_id: &str) -> anyhow::Result<Option<Job>> {
        match self {
            Client::Http(client) => {
                let url = format!("{}/api/v1/orchestration/poll", client.base);
                let resp = client
                    .auth(client.http.post(url))
                    .json(&PollJobRequest {
                        worker_id: worker_id.to_string(),
                    })
                    .send()
                    .await?;
                if !resp.status().is_success() {
                    anyhow::bail!("poll failed: {}", resp.status());
                }
                let parsed: PollJobResponse = resp.json().await?;
                Ok(parsed.job)
            }
            Client::Grpc(client) => {
                let mut inner = client.client.clone();
                let req = apply_grpc_bearer(
                    client.token.as_ref(),
                    tonic::Request::new(pb::PollJobRequest {
                        worker_id: worker_id.to_string(),
                    }),
                )?;
                let resp = inner.poll_job(req).await?.into_inner();
                Ok(resp.job.map(job_from_pb).transpose()?)
            }
        }
    }

    async fn update_job(
        &self,
        worker_id: &str,
        job_id: uuid::Uuid,
        state: JobState,
        transcript_dir: Option<String>,
        result: Option<JobResult>,
    ) -> anyhow::Result<()> {
        match self {
            Client::Http(client) => {
                let url = format!("{}/api/v1/orchestration/jobs/{job_id}", client.base);
                let resp = client
                    .auth(client.http.put(url))
                    .json(&JobUpdateRequest {
                        worker_id: worker_id.to_string(),
                        state,
                        transcript_dir,
                        result,
                    })
                    .send()
                    .await?;
                if !resp.status().is_success() {
                    anyhow::bail!("update job failed: {}", resp.status());
                }
                Ok(())
            }
            Client::Grpc(client) => {
                let mut inner = client.client.clone();
                let req = apply_grpc_bearer(
                    client.token.as_ref(),
                    tonic::Request::new(pb::UpdateJobRequest {
                        worker_id: worker_id.to_string(),
                        job_id: job_id.to_string(),
                        state: job_state_to_pb(state),
                        transcript_dir,
                        result: result.map(job_result_to_pb),
                    }),
                )?;
                inner.update_job(req).await?;
                Ok(())
            }
        }
    }

    async fn dispatch(&self) -> anyhow::Result<()> {
        match self {
            Client::Http(client) => {
                let url = format!("{}/api/v1/orchestration/dispatch", client.base);
                let resp = client.auth(client.http.post(url)).send().await?;
                if !resp.status().is_success() {
                    anyhow::bail!("dispatch failed: {}", resp.status());
                }
                Ok(())
            }
            Client::Grpc(_) => Ok(()),
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let modes = cli
        .mode
        .into_iter()
        .map(ExecutionMode::from)
        .collect::<Vec<_>>();

    let client = match cli.transport {
        TransportKind::Http => Client::Http(HttpClient::new(
            cli.server.trim_end_matches('/').to_string(),
            cli.token.map(Secret::new),
        )),
        TransportKind::Grpc => {
            let channel = tonic::transport::Endpoint::new(cli.grpc_endpoint.clone())?
                .connect()
                .await
                .context("failed to connect gRPC endpoint")?;
            Client::Grpc(GrpcClient {
                client: pb::worker_lifecycle_client::WorkerLifecycleClient::new(channel),
                token: cli.token.map(Secret::new),
            })
        }
    };

    let event_publisher = match &client {
        Client::Grpc(grpc) => {
            let tx = start_grpc_event_stream(grpc.client.clone(), grpc.token.clone()).await?;
            Some(JobEventPublisher {
                worker_id: cli.worker_id.clone(),
                tx,
            })
        }
        Client::Http(_) => None,
    };

    let client = Arc::new(client);

    std::fs::create_dir_all(&cli.data_dir)?;
    let local_agent_config = load_local_agent_config(cli.local_agent_config.as_deref())
        .context("failed to load local-agent config")?;
    let egress_manager = EgressManager::new(&cli.worker_id, &cli.data_dir, local_agent_config)
        .context("failed to initialize local-agent egress manager")?;
    let repos_root = cli.data_dir.join("repos");
    std::fs::create_dir_all(&repos_root)?;
    let worktrees_root = cli.data_dir.join("worktrees");
    std::fs::create_dir_all(&worktrees_root)?;
    let transcripts_root = cli.data_dir.join("transcripts");
    std::fs::create_dir_all(&transcripts_root)?;

    if let Err(error) =
        cleanup_old_transcripts(&transcripts_root, cli.transcript_retention_hours).await
    {
        eprintln!("failed to cleanup old transcripts at startup: {error:#}");
    }
    let mut last_transcript_cleanup = Instant::now();

    client
        .register_worker(&RegisterWorkerRequest {
            id: cli.worker_id.clone(),
            hostname: cli.hostname,
            supported_modes: modes,
        })
        .await
        .context("worker registration failed")?;

    loop {
        if last_transcript_cleanup.elapsed()
            >= Duration::from_secs(TRANSCRIPT_CLEANUP_INTERVAL_SECS)
        {
            match cleanup_old_transcripts(&transcripts_root, cli.transcript_retention_hours).await {
                Ok(removed) => {
                    if removed > 0 {
                        eprintln!("info: removed {removed} expired transcript director(ies)");
                    }
                }
                Err(error) => {
                    eprintln!("failed to cleanup old transcripts: {error:#}");
                }
            }
            last_transcript_cleanup = Instant::now();
        }

        if let Err(error) = client.heartbeat(&cli.worker_id, WorkerState::Idle).await {
            eprintln!("worker heartbeat failed: {error}");
        }
        let _ = client.dispatch().await;

        let polled_job = match client.poll(&cli.worker_id).await {
            Ok(job) => job,
            Err(error) => {
                eprintln!("poll failed: {error}");
                tokio::time::sleep(Duration::from_secs(cli.poll_interval_secs)).await;
                continue;
            }
        };

        if let Some(job) = polled_job {
            if let Err(error) = client
                .update_job(&cli.worker_id, job.id, JobState::Running, None, None)
                .await
            {
                eprintln!("failed to report running job {}: {error}", job.id);
            }

            let hb_client = Arc::clone(&client);
            let hb_worker_id = cli.worker_id.clone();
            let hb_job_id = job.id;
            let (hb_stop_tx, mut hb_stop_rx) = tokio::sync::watch::channel(false);
            let heartbeat_task = tokio::spawn(async move {
                let mut interval = tokio::time::interval(Duration::from_secs(JOB_HEARTBEAT_SECS));
                loop {
                    tokio::select! {
                        _ = hb_stop_rx.changed() => break,
                        _ = interval.tick() => {
                            if let Err(error) = hb_client.heartbeat(&hb_worker_id, WorkerState::Busy).await {
                                eprintln!("busy heartbeat failed for job {hb_job_id}: {error}");
                            }
                            if let Err(error) = hb_client
                                .update_job(&hb_worker_id, hb_job_id, JobState::Running, None, None)
                                .await
                            {
                                eprintln!("running lease renewal failed for job {hb_job_id}: {error}");
                            }
                        }
                    }
                }
            });

            let transcript_dir = cli
                .data_dir
                .join("transcripts")
                .join(job.id.to_string())
                .to_string_lossy()
                .to_string();

            let result = run_job(
                &job,
                &cli.worker_id,
                &transcript_dir,
                &repos_root,
                &worktrees_root,
                &egress_manager,
                event_publisher.as_ref(),
            )
            .await;

            let _ = hb_stop_tx.send(true);
            let _ = heartbeat_task.await;

            match result {
                Ok(job_result) if matches!(job_result.outcome, ExecutionOutcome::Succeeded) => {
                    if let Err(error) = client
                        .update_job(
                            &cli.worker_id,
                            job.id,
                            JobState::Succeeded,
                            Some(transcript_dir),
                            Some(job_result),
                        )
                        .await
                    {
                        eprintln!("failed to report successful job {}: {error}", job.id);
                    }
                }
                Ok(job_result) => {
                    if let Err(error) = client
                        .update_job(
                            &cli.worker_id,
                            job.id,
                            JobState::Failed,
                            Some(transcript_dir),
                            Some(job_result),
                        )
                        .await
                    {
                        eprintln!("failed to report failed job {}: {error}", job.id);
                    }
                }
                Err(error) => {
                    eprintln!("job {} execution failed: {error}", job.id);
                    let stdout_path = PathBuf::from(&transcript_dir).join("stdout.log");
                    let stderr_path = PathBuf::from(&transcript_dir).join("stderr.log");
                    let now = Utc::now();
                    let infra_result = JobResult {
                        outcome: ExecutionOutcome::Failed,
                        termination_reason: TerminationReason::WorkerError,
                        exit_code: None,
                        timing: ExecutionTiming {
                            started_at: now,
                            finished_at: now,
                            duration_ms: 0,
                        },
                        artifacts: ArtifactSummary {
                            stdout_path: stdout_path.to_string_lossy().to_string(),
                            stderr_path: stderr_path.to_string_lossy().to_string(),
                            output_path: None,
                            transcript_dir: Some(transcript_dir.clone()),
                            stdout_bytes: file_len_u64(&stdout_path),
                            stderr_bytes: file_len_u64(&stderr_path),
                            output_bytes: None,
                            structured_transcript_artifacts: Vec::new(),
                        },
                        output_json: None,
                        repo: None,
                        github_pr: None,
                        error_message: Some(format!("{error:#}")),
                    };
                    if let Err(update_error) = client
                        .update_job(
                            &cli.worker_id,
                            job.id,
                            JobState::Failed,
                            Some(transcript_dir),
                            Some(infra_result),
                        )
                        .await
                    {
                        eprintln!("failed to report failed job {}: {update_error}", job.id);
                    }
                }
            }

            if let Err(error) = client.heartbeat(&cli.worker_id, WorkerState::Idle).await {
                eprintln!("worker heartbeat failed after job {}: {error}", job.id);
            }
        }

        tokio::time::sleep(Duration::from_secs(cli.poll_interval_secs)).await;
    }
}

async fn run_job(
    job: &Job,
    worker_id: &str,
    transcript_dir: &str,
    repos_root: &Path,
    worktrees_root: &Path,
    egress_manager: &EgressManager,
    event_publisher: Option<&JobEventPublisher>,
) -> anyhow::Result<JobResult> {
    std::fs::create_dir_all(transcript_dir)?;

    let local_agent_config = egress_manager.config();
    let execution_kind = job_execution_kind_from_context(&job.context);
    let managed_operation = managed_operation_from_job_context(&job.context);
    let resolved_runner = resolve_runner_spec_for_job(
        job,
        execution_kind,
        managed_operation.as_deref(),
        local_agent_config,
    )?;

    let (runtime_spec, runtime_spec_source) = resolve_runtime_spec_and_source(job);
    let mut runtime_spec = runtime_spec;
    if runtime_spec.environment.container_image.is_none() {
        runtime_spec.environment.container_image = resolved_runner.container_image.clone();
    }
    runtime_spec
        .environment
        .file_mounts
        .extend(resolved_runner.file_mounts.clone());
    let local_egress_plan = resolve_local_agent_egress_plan(&runtime_spec)?;
    let runtime_source_label = runtime_spec_source.label();
    match runtime_spec_source {
        RuntimeSpecSource::CanonicalContext => {
            eprintln!(
                "info: job {} using {} (version={}, mode={}, timeout_secs={:?}, network={:?}, mounts={}, toolchains={})",
                job.id,
                runtime_source_label,
                runtime_spec.runtime_version,
                runtime_spec.environment.execution_mode,
                runtime_spec.environment.timeout_secs,
                runtime_spec.environment.network,
                runtime_spec.environment.file_mounts.len(),
                runtime_spec.environment.toolchains.len()
            );
        }
        RuntimeSpecSource::LegacyJobFields => {
            eprintln!(
                "WARNING: job {} falling back to {} (mode={}, timeout_secs={:?}, execution_kind={})",
                job.id,
                runtime_source_label,
                runtime_spec.environment.execution_mode,
                runtime_spec.environment.timeout_secs,
                execution_kind.as_str()
            );
        }
    }

    let job_events_path = PathBuf::from(transcript_dir).join("job-events.jsonl");
    let job_event_sink = JobEventSink {
        worker_id: event_publisher
            .map(|p| p.worker_id.clone())
            .unwrap_or_else(|| worker_id.to_string()),
        writer: Arc::new(Mutex::new(
            JobEventTranscriptWriter::create(&job_events_path).await?,
        )),
        publisher: event_publisher.cloned(),
    };

    job_event_sink
        .emit(
            job.id,
            pb::JobEventKind::JobEventSystem,
            "workspace preparation started".to_string(),
        )
        .await;

    let prepared_workspace =
        prepare_workspace(job, &runtime_spec, repos_root, worktrees_root).await?;
    let workspace_dir = prepared_workspace.workspace_dir.clone();

    job_event_sink
        .emit(
            job.id,
            pb::JobEventKind::JobEventSystem,
            format!("workspace prepared at {}", workspace_dir.to_string_lossy()),
        )
        .await;

    let mut env = HashMap::new();
    for (k, v) in &job.env {
        env.insert(k.clone(), v.clone());
    }

    let prompt_dir = PathBuf::from(transcript_dir).join("prompt");
    std::fs::create_dir_all(&prompt_dir)?;
    if let Some(prompt) = &job.prompt {
        if let Some(system) = &prompt.system {
            std::fs::write(prompt_dir.join("system.txt"), system)?;
        }
        std::fs::write(prompt_dir.join("user.txt"), &prompt.user)?;
    }
    std::fs::write(
        PathBuf::from(transcript_dir).join("context.json"),
        serde_json::to_vec_pretty(&job.context)?,
    )?;

    let prompt_system_path = prompt_dir.join("system.txt");
    let prompt_user_path = prompt_dir.join("user.txt");
    let context_path = PathBuf::from(transcript_dir).join("context.json");
    let agent_context_path = PathBuf::from(transcript_dir).join(".agent-context.json");
    let worktree_agent_context_path = workspace_dir.join(".agent-context.json");
    let transcript_jsonl_path = PathBuf::from(transcript_dir).join("transcript.jsonl");
    let output_path = PathBuf::from(transcript_dir).join("output.json");
    let stdout_path = PathBuf::from(transcript_dir).join("stdout.log");
    let stderr_path = PathBuf::from(transcript_dir).join("stderr.log");
    let run_json_path = PathBuf::from(transcript_dir).join("run.json");
    let work_item_context = work_item_contract_from_job_context(&job.context);

    env.insert(
        "AGENT_WORKSPACE_DIR".to_string(),
        workspace_dir.to_string_lossy().to_string(),
    );
    env.insert(
        "AGENT_TRANSCRIPT_DIR".to_string(),
        transcript_dir.to_string(),
    );
    if let Some(repo) = &job.repo {
        if let Some(repo_slug) = normalize_github_repo_slug(&repo.repo_url) {
            env.insert("AGENT_REPO_SLUG".to_string(), repo_slug);
        }
        env.insert("AGENT_REPO_URL".to_string(), repo.repo_url.clone());
        env.insert("AGENT_BASE_REF".to_string(), repo.base_ref.clone());
        let target_branch = branch_for_job_attempt(job, repo.branch_name.clone());
        env.insert("AGENT_TARGET_BRANCH".to_string(), target_branch);
    }
    env.insert(
        "AGENT_CONTEXT_PATH".to_string(),
        context_path.to_string_lossy().to_string(),
    );
    env.insert(
        "AGENT_AGENT_CONTEXT_PATH".to_string(),
        agent_context_path.to_string_lossy().to_string(),
    );
    env.insert(
        "AGENT_TRANSCRIPT_JSONL_PATH".to_string(),
        transcript_jsonl_path.to_string_lossy().to_string(),
    );
    env.insert(
        "AGENT_JOB_EVENTS_JSONL_PATH".to_string(),
        job_events_path.to_string_lossy().to_string(),
    );
    env.insert(
        "AGENT_OUTPUT_PATH".to_string(),
        output_path.to_string_lossy().to_string(),
    );
    env.insert(
        "AGENT_STDOUT_LOG_PATH".to_string(),
        stdout_path.to_string_lossy().to_string(),
    );
    env.insert(
        "AGENT_STDERR_LOG_PATH".to_string(),
        stderr_path.to_string_lossy().to_string(),
    );
    env.insert(
        "AGENT_RUN_JSON_PATH".to_string(),
        run_json_path.to_string_lossy().to_string(),
    );
    env.insert(
        "AGENT_PROMPT_USER_PATH".to_string(),
        prompt_user_path.to_string_lossy().to_string(),
    );
    env.insert(
        "AGENT_PROMPT_SYSTEM_PATH".to_string(),
        prompt_system_path.to_string_lossy().to_string(),
    );
    env.insert(
        "AGENT_CAN_ACCESS_NETWORK".to_string(),
        runtime_spec.permissions.can_access_network.to_string(),
    );
    if let Some(tier) = local_egress_plan.tier {
        env.insert(
            "AGENT_LOCAL_EGRESS_TIER".to_string(),
            tier.as_str().to_string(),
        );
    }
    env.insert(
        "AGENT_PERMISSION_TIER".to_string(),
        planned_permission_tier_name(&runtime_spec.permissions.tier).to_string(),
    );
    env.insert(
        "AGENT_REPO_COUNT".to_string(),
        runtime_spec.repos.len().to_string(),
    );
    if let Some(approval_rules) = &runtime_spec.approval_rules
        && let Ok(value) = serde_json::to_string(approval_rules)
    {
        env.insert("AGENT_APPROVAL_RULES".to_string(), value);
    }
    if let Some(work_item_title) = work_item_context
        .get("title")
        .and_then(serde_json::Value::as_str)
    {
        env.insert(
            "AGENT_HUB_WORK_ITEM_TITLE".to_string(),
            work_item_title.to_string(),
        );
    }
    if let Some(work_item_workflow_state) = work_item_context
        .get("workflow_state")
        .and_then(serde_json::Value::as_str)
    {
        env.insert(
            "AGENT_HUB_WORK_ITEM_WORKFLOW_STATE".to_string(),
            work_item_workflow_state.to_string(),
        );
    }
    // NOTE: permission-related env vars remain advisory metadata for in-agent behavior,
    // while runtime boundary enforcement is active for Docker execution.

    if let Err(error) = write_agent_context_file(
        &agent_context_path,
        job,
        transcript_dir,
        &workspace_dir,
        &runtime_spec,
        &prepared_workspace.additional_repo_mounts,
    ) {
        job_event_sink
            .emit(
                job.id,
                pb::JobEventKind::JobEventSystem,
                format!("warning: failed to write .agent-context.json: {error:#}"),
            )
            .await;
    } else if let Err(error) = std::fs::copy(&agent_context_path, &worktree_agent_context_path) {
        job_event_sink
            .emit(
                job.id,
                pb::JobEventKind::JobEventSystem,
                format!("warning: failed to write worktree .agent-context.json: {error:#}"),
            )
            .await;
    }

    let resolved_job = Job {
        command: resolved_runner.command.clone(),
        args: resolved_runner.args.clone(),
        container_image: runtime_spec.environment.container_image.clone(),
        ..job.clone()
    };

    let mut execution_result = async {
        let started_at = Utc::now();
        job_event_sink
            .emit(
                job.id,
                pb::JobEventKind::JobEventSystem,
                format!(
                    "command start: {} {}",
                    resolved_runner.command,
                    resolved_runner.args.join(" ")
                ),
            )
            .await;
        job_event_sink
            .emit(
                job.id,
                pb::JobEventKind::JobEventSystem,
                format!(
                    "runtime source: {} (mode={}, timeout_secs={:?}, network={:?})",
                    runtime_source_label,
                    runtime_spec.environment.execution_mode,
                    runtime_spec.environment.timeout_secs,
                    runtime_spec.environment.network
                ),
            )
            .await;
        let execution_status = match runtime_spec.environment.execution_mode {
            ExecutionMode::Raw => {
                if matches!(local_egress_plan.tier, Some(LocalAgentEgressTier::Llm | LocalAgentEgressTier::Build))
                {
                    anyhow::bail!(
                        "restricted egress tiers are only supported for Docker execution mode; raw mode cannot enforce restricted egress"
                    );
                }
                if !runtime_spec.permissions.can_access_network {
                    let warning = "WARNING: network restriction is not enforced in raw mode";
                    eprintln!("{warning}");
                    job_event_sink
                        .emit(job.id, pb::JobEventKind::JobEventSystem, warning.to_string())
                        .await;
                }
                if !runtime_spec.permissions.can_write_workspace {
                    let warning =
                        "WARNING: workspace write restriction is not enforced in raw mode";
                    eprintln!("{warning}");
                    job_event_sink
                        .emit(job.id, pb::JobEventKind::JobEventSystem, warning.to_string())
                        .await;
                }
                run_raw(
                    &resolved_job,
                    &workspace_dir,
                    &transcript_jsonl_path,
                    &stdout_path,
                    &stderr_path,
                    &env,
                    runtime_spec.environment.timeout_secs,
                    Some(&job_event_sink),
                )
                .await?
            }
            ExecutionMode::Docker => {
                match &resolved_runner.kind {
                    ResolvedRunnerKind::Opencode(opencode) => {
                        run_docker_opencode_with_validation_retries(
                            &resolved_job,
                            opencode,
                            &workspace_dir,
                            transcript_dir,
                            &transcript_jsonl_path,
                            &stdout_path,
                            &stderr_path,
                            &output_path,
                            &env,
                            runtime_spec.environment.timeout_secs,
                            runtime_spec.environment.container_image.clone(),
                            &runtime_spec,
                            &local_egress_plan,
                            egress_manager,
                            prepared_workspace.mirror_dir.as_deref(),
                            &prepared_workspace.additional_repo_mounts,
                            workflow_valid_next_events_from_job_context(&job.context)
                                .unwrap_or_default(),
                            work_item_context.clone(),
                            Some(&job_event_sink),
                        )
                        .await?
                    }
                    ResolvedRunnerKind::Generic => {
                        run_docker(
                            &resolved_job,
                            &workspace_dir,
                            transcript_dir,
                            &transcript_jsonl_path,
                            &stdout_path,
                            &stderr_path,
                            &env,
                            runtime_spec.environment.timeout_secs,
                            runtime_spec.environment.container_image.clone(),
                            &runtime_spec,
                            &local_egress_plan,
                            egress_manager,
                            prepared_workspace.mirror_dir.as_deref(),
                            &prepared_workspace.additional_repo_mounts,
                            Some(&job_event_sink),
                        )
                        .await?
                    }
                }
            }
        };
        let finished_at = Utc::now();
        let duration_ms = (finished_at - started_at)
            .to_std()
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);

        let (outcome, termination_reason, exit_code) = match execution_status {
            ExecutionStatus::Exited(code) => {
                if code == 0 {
                    (
                        ExecutionOutcome::Succeeded,
                        TerminationReason::ExitCode,
                        Some(code),
                    )
                } else {
                    (
                        ExecutionOutcome::Failed,
                        TerminationReason::ExitCode,
                        Some(code),
                    )
                }
            }
            ExecutionStatus::TimedOut => {
                job_event_sink
                    .emit(
                        job.id,
                        pb::JobEventKind::JobEventSystem,
                        "execution timed out".to_string(),
                    )
                    .await;
                (ExecutionOutcome::TimedOut, TerminationReason::Timeout, None)
            }
        };

        let output_json = if output_path.exists() {
            job_event_sink
                .emit(
                    job.id,
                    pb::JobEventKind::JobEventSystem,
                    format!("output.json detected at {}", output_path.to_string_lossy()),
                )
                .await;
            let bytes = std::fs::read(&output_path)?;
            let raw_output_json: serde_json::Value =
                serde_json::from_slice(&bytes).context("invalid output.json")?;

            if let Ok(typed_output) = serde_json::from_value::<AgentOutput>(raw_output_json.clone())
                && let Some(valid_next_events) =
                    workflow_valid_next_events_from_job_context(&job.context)
                && let Err(validation_error) =
                    validate_agent_output_next_event(&typed_output, &valid_next_events)
            {
                let message = format!(
                    "warning: output.json next_event validation failed for job {}: {}",
                    job.id, validation_error
                );
                eprintln!("{message}");
                job_event_sink
                    .emit(job.id, pb::JobEventKind::JobEventSystem, message)
                    .await;
            }

            Some(raw_output_json)
        } else {
            None
        };

        let github_pr = output_json.as_ref().and_then(parse_github_pr_from_output);

        let target_branch = job
            .repo
            .as_ref()
            .map(|r| branch_for_job_attempt(job, r.branch_name.clone()));
        let repo_meta =
            capture_repo_metadata(&workspace_dir, job.repo.as_ref(), target_branch).await;

        let (workspace_dir_display, transcript_dir_display) =
            display_paths_for_runtime(&workspace_dir, transcript_dir, &runtime_spec);
        let mut meta = tokio::fs::File::create(&run_json_path).await?;
        let payload = build_run_json_payload(
            job,
            &runtime_spec,
            &outcome,
            &termination_reason,
            exit_code,
            &workspace_dir_display,
            &transcript_dir_display,
        );
        meta.write_all(serde_json::to_string_pretty(&payload)?.as_bytes())
            .await?;

        Ok::<JobResult, anyhow::Error>(JobResult {
            outcome,
            termination_reason,
            exit_code,
            timing: ExecutionTiming {
                started_at,
                finished_at,
                duration_ms,
            },
            artifacts: ArtifactSummary {
                stdout_path: stdout_path.to_string_lossy().to_string(),
                stderr_path: stderr_path.to_string_lossy().to_string(),
                output_path: if output_path.exists() {
                    Some(output_path.to_string_lossy().to_string())
                } else {
                    None
                },
                transcript_dir: Some(transcript_dir.to_string()),
                stdout_bytes: file_len_u64(&stdout_path),
                stderr_bytes: file_len_u64(&stderr_path),
                output_bytes: file_len_u64(&output_path),
                structured_transcript_artifacts: Vec::new(),
            },
            output_json,
            repo: repo_meta,
            github_pr,
            error_message: None,
        })
    }
    .await;

    let _ = std::fs::remove_file(&worktree_agent_context_path);

    if let Err(error) = cleanup_workspace(job, &prepared_workspace, Some(&job_event_sink)).await {
        job_event_sink
            .emit(
                job.id,
                pb::JobEventKind::JobEventSystem,
                format!("workspace cleanup warning: {error:#}"),
            )
            .await;
    }

    if let Ok(job_result) = &mut execution_result {
        job_result.artifacts.structured_transcript_artifacts =
            collect_structured_transcript_artifacts(
                &agent_context_path,
                &run_json_path,
                &transcript_jsonl_path,
                &job_events_path,
            );
    }

    execution_result
}

#[derive(Debug, Clone, Copy)]
enum ExecutionStatus {
    Exited(i32),
    TimedOut,
}

#[derive(Debug, Clone, Copy)]
enum RuntimeSpecSource {
    CanonicalContext,
    LegacyJobFields,
}

impl RuntimeSpecSource {
    fn label(self) -> &'static str {
        match self {
            Self::CanonicalContext => "canonical-runtime-spec",
            Self::LegacyJobFields => "legacy-job-fields-fallback",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JobExecutionKind {
    Agent,
    Command,
    Managed,
}

impl JobExecutionKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::Command => "command",
            Self::Managed => "managed",
        }
    }
}

#[derive(Debug, Clone)]
struct ResolvedRunnerSpec {
    kind: ResolvedRunnerKind,
    command: String,
    args: Vec<String>,
    container_image: Option<String>,
    file_mounts: Vec<protocol::orchestration::PlannedFileMount>,
}

#[derive(Debug, Clone)]
enum ResolvedRunnerKind {
    Generic,
    Opencode(OpencodeManagedSpec),
}

#[derive(Debug, Clone)]
struct OpencodeManagedSpec {
    max_validation_attempts: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LocalAgentEgressTier {
    Llm,
    Build,
    Full,
}

impl LocalAgentEgressTier {
    fn as_str(self) -> &'static str {
        match self {
            Self::Llm => "llm",
            Self::Build => "build",
            Self::Full => "full",
        }
    }
}

#[derive(Debug, Clone)]
struct LocalAgentEgressPlan {
    network_none: bool,
    tier: Option<LocalAgentEgressTier>,
}

fn resolve_local_agent_egress_plan(
    runtime_spec: &protocol::CanonicalRuntimeSpec,
) -> anyhow::Result<LocalAgentEgressPlan> {
    if runtime_spec.environment.network_resources.is_none()
        && let Some(raw_network) = runtime_spec.environment.network.as_deref()
        && !matches!(raw_network, "none" | "restricted" | "full")
    {
        anyhow::bail!(
            "legacy network label '{}' is unsupported; expected one of: none, restricted, full",
            raw_network
        );
    }

    let request = runtime_spec
        .environment
        .network_resources
        .clone()
        .or_else(|| {
            protocol::network_request_from_legacy_label(runtime_spec.environment.network.as_deref())
        });

    let Some(request) = request else {
        return Ok(LocalAgentEgressPlan {
            network_none: !runtime_spec.permissions.can_access_network,
            tier: None,
        });
    };

    let tier = match request {
        NetworkResourceRequest::None => None,
        NetworkResourceRequest::Full => Some(LocalAgentEgressTier::Full),
        NetworkResourceRequest::Resources(resources) => {
            let mut deduped = resources;
            deduped.sort_by(|a, b| a.to_string().cmp(&b.to_string()));
            deduped.dedup();

            if deduped.is_empty() {
                None
            } else if deduped
                .iter()
                .all(|resource| matches!(resource, NetworkResourceKind::Llm))
            {
                Some(LocalAgentEgressTier::Llm)
            } else if deduped.iter().all(|resource| {
                matches!(
                    resource,
                    NetworkResourceKind::Llm
                        | NetworkResourceKind::Github
                        | NetworkResourceKind::GoRegistry
                        | NetworkResourceKind::CratesRegistry
                )
            }) {
                Some(LocalAgentEgressTier::Build)
            } else {
                anyhow::bail!(
                    "requested network resources are unsupported by local-agent tiers: {:?}",
                    deduped
                );
            }
        }
    };

    Ok(LocalAgentEgressPlan {
        network_none: tier.is_none(),
        tier,
    })
}

fn resolve_runtime_spec_and_source(
    job: &Job,
) -> (protocol::CanonicalRuntimeSpec, RuntimeSpecSource) {
    if let Some(runtime_spec) = protocol::runtime_spec_from_job_context(&job.context) {
        if runtime_spec.runtime_version == "v2" {
            return (runtime_spec, RuntimeSpecSource::CanonicalContext);
        }
        let mut fallback = canonical_runtime_spec_for_job(job);
        fallback.runtime_version = runtime_spec.runtime_version;
        merge_missing_repo_claims(&mut fallback.repos, runtime_spec.repos);
        (fallback, RuntimeSpecSource::LegacyJobFields)
    } else {
        let mut fallback = canonical_runtime_spec_for_job(job);
        merge_missing_repo_claims(
            &mut fallback.repos,
            repo_claims_from_runtime_spec_context_value(&job.context),
        );
        (fallback, RuntimeSpecSource::LegacyJobFields)
    }
}

fn merge_missing_repo_claims(
    destination: &mut Vec<protocol::PlannedRepoClaim>,
    candidates: Vec<protocol::PlannedRepoClaim>,
) {
    if candidates.is_empty() {
        return;
    }

    let mut seen = destination
        .iter()
        .map(|claim| claim.repo_url.trim().to_string())
        .filter(|repo_url| !repo_url.is_empty())
        .collect::<HashSet<_>>();

    for claim in candidates {
        let repo_url = claim.repo_url.trim().to_string();
        if repo_url.is_empty() || !seen.insert(repo_url) {
            continue;
        }
        destination.push(claim);
    }
}

fn repo_claims_from_runtime_spec_context_value(
    context: &serde_json::Value,
) -> Vec<protocol::PlannedRepoClaim> {
    context
        .as_object()
        .and_then(|obj| obj.get("canonical_runtime_spec"))
        .and_then(serde_json::Value::as_object)
        .and_then(|runtime| runtime.get("repos"))
        .and_then(serde_json::Value::as_array)
        .map(|repos| {
            repos
                .iter()
                .filter_map(repo_claim_from_runtime_context_entry)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
}

fn repo_claim_from_runtime_context_entry(
    entry: &serde_json::Value,
) -> Option<protocol::PlannedRepoClaim> {
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
    let read_only = obj
        .get("read_only")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(true);
    let target_branch = obj
        .get("target_branch")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned);
    let claim_type = obj
        .get("claim_type")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| {
            if read_only {
                "read_only".to_string()
            } else {
                "workspace".to_string()
            }
        });
    let branch_name = obj
        .get("branch_name")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned);

    Some(protocol::PlannedRepoClaim {
        repo_url,
        base_ref,
        read_only,
        target_branch,
        claim_type,
        branch_name,
    })
}

fn job_execution_kind_from_context(context: &serde_json::Value) -> JobExecutionKind {
    let raw = context
        .get("workflow")
        .and_then(|workflow| workflow.get("job_execution_kind"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("command");
    match raw {
        "agent" => JobExecutionKind::Agent,
        "managed" => JobExecutionKind::Managed,
        _ => JobExecutionKind::Command,
    }
}

fn managed_operation_from_job_context(context: &serde_json::Value) -> Option<String> {
    context
        .get("workflow")
        .and_then(|workflow| workflow.get("managed_operation"))
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn resolve_runner_spec_for_job(
    job: &Job,
    execution_kind: JobExecutionKind,
    managed_operation: Option<&str>,
    config: &LocalAgentConfig,
) -> anyhow::Result<ResolvedRunnerSpec> {
    match execution_kind {
        JobExecutionKind::Command => Ok(ResolvedRunnerSpec {
            kind: ResolvedRunnerKind::Generic,
            command: job.command.clone(),
            args: job.args.clone(),
            container_image: None,
            file_mounts: Vec::new(),
        }),
        JobExecutionKind::Agent => {
            resolved_runner_spec_from_config(&config.default_agent, Some("default_agent"))
        }
        JobExecutionKind::Managed => {
            let operation = managed_operation.ok_or_else(|| {
                anyhow::anyhow!(
                    "managed job {} missing workflow.managed_operation context",
                    job.id
                )
            })?;
            let spec = config
                .managed_runner_for_operation(operation)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "managed job {} references unknown managed operation '{}'",
                        job.id,
                        operation
                    )
                })?;
            resolved_runner_spec_from_config(spec, Some(operation))
        }
    }
}

fn resolved_runner_spec_from_config(
    spec: &ManagedRunnerSpec,
    label: Option<&str>,
) -> anyhow::Result<ResolvedRunnerSpec> {
    if spec.command.trim().is_empty() {
        let target = label.unwrap_or("managed runner");
        anyhow::bail!("{target} command must not be empty");
    }

    Ok(ResolvedRunnerSpec {
        kind: resolved_runner_kind_from_spec(spec),
        command: spec.command.clone(),
        args: spec.args.clone(),
        container_image: spec.container_image.clone(),
        file_mounts: spec
            .file_mounts
            .iter()
            .map(|mount| protocol::orchestration::PlannedFileMount {
                host_path: mount.host_path.clone(),
                container_path: mount.container_path.clone(),
                read_only: mount.read_only,
            })
            .collect(),
    })
}

fn resolved_runner_kind_from_spec(spec: &ManagedRunnerSpec) -> ResolvedRunnerKind {
    let is_opencode = match spec.kind {
        ManagedRunnerKind::Opencode => true,
        ManagedRunnerKind::Generic => runner_spec_looks_like_opencode(spec),
    };

    if is_opencode {
        ResolvedRunnerKind::Opencode(OpencodeManagedSpec {
            max_validation_attempts: spec.opencode.max_validation_attempts,
        })
    } else {
        ResolvedRunnerKind::Generic
    }
}

fn runner_spec_looks_like_opencode(spec: &ManagedRunnerSpec) -> bool {
    if spec.command.trim() == "opencode" {
        return true;
    }
    spec.args
        .iter()
        .any(|arg| arg.contains("opencode run") || arg.contains("opencode\trun"))
}

#[derive(Debug, Clone)]
struct PreparedWorkspace {
    workspace_dir: PathBuf,
    mirror_dir: Option<PathBuf>,
    attempt_branch: Option<String>,
    additional_repo_mounts: Vec<AdditionalRepoMount>,
}

#[derive(Debug, Clone)]
struct AdditionalRepoMount {
    repo_url: String,
    host_path: PathBuf,
    container_path: String,
    writable: bool,
}

async fn prepare_workspace(
    job: &Job,
    runtime_spec: &protocol::CanonicalRuntimeSpec,
    repos_root: &Path,
    worktrees_root: &Path,
) -> anyhow::Result<PreparedWorkspace> {
    let Some(repo) = &job.repo else {
        let workspace_dir = worktree_dir_for_job_attempt(worktrees_root, job);
        std::fs::create_dir_all(&workspace_dir)?;
        ensure_git_workspace(&workspace_dir).await?;
        let additional_repo_mounts =
            prepare_additional_repo_mounts(job, runtime_spec, repos_root).await?;
        return Ok(PreparedWorkspace {
            workspace_dir,
            mirror_dir: None,
            attempt_branch: None,
            additional_repo_mounts,
        });
    };

    std::fs::create_dir_all(repos_root)?;
    std::fs::create_dir_all(worktrees_root)?;

    let repo_key = safe_repo_key(&repo.repo_url);
    let mirror_dir = repos_root.join(repo_key);
    if !mirror_dir.join(".git").exists() {
        run_status(
            tokio::process::Command::new("git")
                .arg("clone")
                .arg("--origin")
                .arg("origin")
                .arg(repo.repo_url.as_str())
                .arg(&mirror_dir),
            "git clone failed",
        )
        .await?;
    }

    refresh_mirror_if_needed(&mirror_dir).await?;

    run_status(
        tokio::process::Command::new("git")
            .arg("-C")
            .arg(&mirror_dir)
            .arg("worktree")
            .arg("prune"),
        "git worktree prune failed",
    )
    .await?;

    let worktree_dir = worktree_dir_for_job_attempt(worktrees_root, job);
    if worktree_dir.exists() {
        let _ = std::fs::remove_dir_all(&worktree_dir);
    }

    let target_branch = branch_for_job_attempt(job, repo.branch_name.clone());
    let checkout_ref = checkout_ref_for_repo_job(job, repo);

    run_status(
        tokio::process::Command::new("git")
            .arg("-C")
            .arg(&mirror_dir)
            .arg("worktree")
            .arg("add")
            .arg("-B")
            .arg(&target_branch)
            .arg(&worktree_dir)
            .arg(&checkout_ref),
        "git worktree add failed",
    )
    .await?;

    let additional_repo_mounts =
        prepare_additional_repo_mounts(job, runtime_spec, repos_root).await?;

    Ok(PreparedWorkspace {
        workspace_dir: worktree_dir,
        mirror_dir: Some(mirror_dir),
        attempt_branch: Some(target_branch),
        additional_repo_mounts,
    })
}

async fn ensure_repo_mirror(repo_url: &str, repos_root: &Path) -> anyhow::Result<PathBuf> {
    std::fs::create_dir_all(repos_root)?;

    let repo_key = safe_repo_key(repo_url);
    let mirror_dir = repos_root.join(repo_key);
    if !mirror_dir.join(".git").exists() {
        run_status(
            tokio::process::Command::new("git")
                .arg("clone")
                .arg("--origin")
                .arg("origin")
                .arg(repo_url)
                .arg(&mirror_dir),
            "git clone failed",
        )
        .await?;
    }

    refresh_mirror_if_needed(&mirror_dir).await?;
    Ok(mirror_dir)
}

async fn prepare_additional_repo_mounts(
    job: &Job,
    runtime_spec: &protocol::CanonicalRuntimeSpec,
    repos_root: &Path,
) -> anyhow::Result<Vec<AdditionalRepoMount>> {
    let additional_claims = additional_repo_claims_for_mounts(job, runtime_spec);
    if additional_claims.is_empty() {
        return Ok(Vec::new());
    }

    let mut mounts = Vec::new();
    for claim in additional_claims {
        let requested_writable = !claim.read_only;
        if requested_writable {
            eprintln!(
                "WARNING: job {} requested writable additional repo '{}'; mounting read-only",
                job.id, claim.repo_url
            );
        }
        let mirror_dir = ensure_repo_mirror(&claim.repo_url, repos_root).await?;
        mounts.push(AdditionalRepoMount {
            repo_url: claim.repo_url.clone(),
            host_path: mirror_dir,
            container_path: additional_repo_container_path(&claim.repo_url),
            writable: false,
        });
    }

    Ok(mounts)
}

fn additional_repo_claims_for_mounts<'a>(
    job: &Job,
    runtime_spec: &'a protocol::CanonicalRuntimeSpec,
) -> Vec<&'a protocol::PlannedRepoClaim> {
    let primary_repo_url = job
        .repo
        .as_ref()
        .map(|repo| repo.repo_url.trim())
        .filter(|repo_url| !repo_url.is_empty());

    runtime_spec
        .repos
        .iter()
        .filter(|claim| {
            let claim_url = claim.repo_url.trim();
            !claim_url.is_empty() && primary_repo_url != Some(claim_url)
        })
        .collect()
}

async fn refresh_mirror_if_needed(mirror_dir: &Path) -> anyhow::Result<()> {
    if !should_refresh_mirror(mirror_dir)? {
        return Ok(());
    }

    run_status(
        tokio::process::Command::new("git")
            .arg("-C")
            .arg(mirror_dir)
            .arg("fetch")
            .arg("--prune")
            .arg("origin"),
        "git fetch failed",
    )
    .await?;

    write_mirror_refresh_timestamp(mirror_dir)?;
    Ok(())
}

fn should_refresh_mirror(mirror_dir: &Path) -> anyhow::Result<bool> {
    let stamp_path = mirror_refresh_stamp_path(mirror_dir);
    let now = std::time::SystemTime::now();
    let now_secs = now
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let Ok(raw) = std::fs::read_to_string(stamp_path) else {
        return Ok(true);
    };
    let last_secs = raw.trim().parse::<u64>().unwrap_or(0);
    Ok(now_secs.saturating_sub(last_secs) >= MIRROR_REFRESH_MIN_SECS)
}

fn write_mirror_refresh_timestamp(mirror_dir: &Path) -> anyhow::Result<()> {
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let stamp_path = mirror_refresh_stamp_path(mirror_dir);
    if let Some(parent) = stamp_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(stamp_path, now_secs.to_string())?;
    Ok(())
}

fn mirror_refresh_stamp_path(mirror_dir: &Path) -> PathBuf {
    mirror_dir.join(".git").join("agent_hub_last_fetch")
}

async fn cleanup_workspace(
    job: &Job,
    prepared: &PreparedWorkspace,
    events: Option<&JobEventSink>,
) -> anyhow::Result<()> {
    if let Some(mirror_dir) = &prepared.mirror_dir {
        run_status(
            tokio::process::Command::new("git")
                .arg("-C")
                .arg(mirror_dir)
                .arg("worktree")
                .arg("remove")
                .arg("--force")
                .arg(&prepared.workspace_dir),
            "git worktree remove failed",
        )
        .await?;
        run_status(
            tokio::process::Command::new("git")
                .arg("-C")
                .arg(mirror_dir)
                .arg("worktree")
                .arg("prune"),
            "git worktree prune failed",
        )
        .await?;

        if let Some(attempt_branch) = &prepared.attempt_branch {
            match tokio::process::Command::new("git")
                .arg("-C")
                .arg(mirror_dir)
                .arg("branch")
                .arg("-D")
                .arg(attempt_branch)
                .status()
                .await
            {
                Ok(status) if status.success() => {}
                Ok(status) => {
                    if let Some(events) = events {
                        events
                            .emit(
                                job.id,
                                pb::JobEventKind::JobEventSystem,
                                format!(
                                    "cleanup warning: failed to delete attempt branch '{}' ({status})",
                                    attempt_branch
                                ),
                            )
                            .await;
                    }
                }
                Err(error) => {
                    if let Some(events) = events {
                        events
                            .emit(
                                job.id,
                                pb::JobEventKind::JobEventSystem,
                                format!(
                                    "cleanup warning: branch delete command failed for '{}' ({error})",
                                    attempt_branch
                                ),
                            )
                            .await;
                    }
                }
            }
        }
    } else if prepared.workspace_dir.exists() {
        std::fs::remove_dir_all(&prepared.workspace_dir)?;
    }

    if let Some(events) = events {
        events
            .emit(
                job.id,
                pb::JobEventKind::JobEventSystem,
                format!(
                    "workspace cleaned: {}",
                    prepared.workspace_dir.to_string_lossy()
                ),
            )
            .await;
    }
    Ok(())
}

async fn cleanup_old_transcripts(
    transcripts_dir: &Path,
    retention_hours: u64,
) -> anyhow::Result<usize> {
    if retention_hours == 0 {
        return Ok(0);
    }
    if !transcripts_dir.exists() {
        return Ok(0);
    }

    let cutoff = std::time::SystemTime::now()
        .checked_sub(Duration::from_secs(retention_hours.saturating_mul(60 * 60)))
        .unwrap_or(std::time::UNIX_EPOCH);

    let mut removed = 0usize;
    for entry in std::fs::read_dir(transcripts_dir)
        .with_context(|| format!("read transcripts dir {}", transcripts_dir.display()))?
    {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                eprintln!(
                    "failed to read transcript directory entry under {}: {error}",
                    transcripts_dir.display()
                );
                continue;
            }
        };
        let path = entry.path();

        let metadata = match entry.metadata() {
            Ok(metadata) => metadata,
            Err(error) => {
                eprintln!(
                    "failed to read transcript metadata {}: {error}",
                    path.display()
                );
                continue;
            }
        };
        if !metadata.is_dir() {
            continue;
        }

        let modified_at = match metadata.modified() {
            Ok(modified_at) => modified_at,
            Err(error) => {
                eprintln!(
                    "failed to read transcript mtime for {}: {error}",
                    path.display()
                );
                continue;
            }
        };

        if modified_at > cutoff {
            continue;
        }

        match std::fs::remove_dir_all(&path) {
            Ok(()) => {
                removed = removed.saturating_add(1);
                eprintln!(
                    "info: removed expired transcript directory {}",
                    path.display()
                );
            }
            Err(error) => {
                eprintln!(
                    "failed to remove expired transcript directory {}: {error}",
                    path.display()
                );
            }
        }
    }

    Ok(removed)
}

fn safe_repo_key(repo_url: &str) -> String {
    let mut out = String::with_capacity(repo_url.len());
    for ch in repo_url.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    out
}

fn default_branch_name(work_item_id: uuid::Uuid) -> String {
    format!("agent-hub/{}", work_item_id.simple())
}

fn checkout_ref_for_repo_job(job: &Job, repo: &protocol::RepoSource) -> String {
    preferred_checkout_ref_from_job_results(&job.context, repo).unwrap_or_else(|| repo.base_ref.clone())
}

fn preferred_checkout_ref_from_job_results(
    context: &serde_json::Value,
    repo: &protocol::RepoSource,
) -> Option<String> {
    let results = context.get("job_results")?.as_object()?;

    let mut base_ref_match: Option<String> = None;
    let mut any_succeeded_ref: Option<String> = None;

    for result in results.values() {
        if result
            .get("outcome")
            .and_then(serde_json::Value::as_str)
            != Some("succeeded")
        {
            continue;
        }
        let Some(repo_meta) = result.get("repo").and_then(serde_json::Value::as_object) else {
            continue;
        };

        let candidate_ref = repo_meta
            .get("head_sha")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
            .or_else(|| {
                repo_meta
                    .get("target_branch")
                    .and_then(serde_json::Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(ToOwned::to_owned)
            });

        let Some(candidate_ref) = candidate_ref else {
            continue;
        };

        if any_succeeded_ref.is_none() {
            any_succeeded_ref = Some(candidate_ref.clone());
        }

        let base_ref_matches = repo_meta
            .get("base_ref")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            == Some(repo.base_ref.as_str());
        if base_ref_matches {
            base_ref_match = Some(candidate_ref);
        }
    }

    base_ref_match.or(any_succeeded_ref)
}

fn branch_for_job_attempt(job: &Job, configured_branch: Option<String>) -> String {
    let base = configured_branch.unwrap_or_else(|| default_branch_name(job.work_item_id));
    format!("{base}--j{}-a{}", job.id.simple(), job.attempt)
}

fn worktree_dir_for_job_attempt(worktrees_root: &Path, job: &Job) -> PathBuf {
    worktrees_root
        .join(job.work_item_id.to_string())
        .join(format!("{}-a{}", job.id.simple(), job.attempt))
}

async fn run_status(cmd: &mut tokio::process::Command, context: &str) -> anyhow::Result<()> {
    let status = cmd.status().await?;
    if status.success() {
        Ok(())
    } else {
        anyhow::bail!("{context}: {status}")
    }
}

async fn run_raw(
    job: &Job,
    workspace_dir: &Path,
    transcript_jsonl_path: &Path,
    stdout_path: &Path,
    stderr_path: &Path,
    env: &HashMap<String, String>,
    timeout_secs: Option<u64>,
    events: Option<&JobEventSink>,
) -> anyhow::Result<ExecutionStatus> {
    run_raw_with_mode(
        job,
        workspace_dir,
        transcript_jsonl_path,
        stdout_path,
        stderr_path,
        env,
        timeout_secs,
        events,
        false,
    )
    .await
}

async fn run_raw_append(
    job: &Job,
    workspace_dir: &Path,
    transcript_jsonl_path: &Path,
    stdout_path: &Path,
    stderr_path: &Path,
    env: &HashMap<String, String>,
    timeout_secs: Option<u64>,
    events: Option<&JobEventSink>,
) -> anyhow::Result<ExecutionStatus> {
    run_raw_with_mode(
        job,
        workspace_dir,
        transcript_jsonl_path,
        stdout_path,
        stderr_path,
        env,
        timeout_secs,
        events,
        true,
    )
    .await
}

async fn run_raw_with_mode(
    job: &Job,
    workspace_dir: &Path,
    transcript_jsonl_path: &Path,
    stdout_path: &Path,
    stderr_path: &Path,
    env: &HashMap<String, String>,
    timeout_secs: Option<u64>,
    events: Option<&JobEventSink>,
    append: bool,
) -> anyhow::Result<ExecutionStatus> {
    let stdout_std = if append {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(stdout_path)?
    } else {
        std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(stdout_path)?
    };
    let stderr_std = if append {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(stderr_path)?
    } else {
        std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(stderr_path)?
    };
    let transcript_std = if append {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(transcript_jsonl_path)?
    } else {
        std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(transcript_jsonl_path)?
    };

    let stdout_file = tokio::io::BufWriter::new(tokio::fs::File::from_std(stdout_std));
    let stderr_file = tokio::io::BufWriter::new(tokio::fs::File::from_std(stderr_std));
    let transcript_writer = Arc::new(Mutex::new(tokio::io::BufWriter::new(
        tokio::fs::File::from_std(transcript_std),
    )));

    let mut cmd = tokio::process::Command::new(&job.command);
    cmd.args(&job.args)
        .current_dir(workspace_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in env {
        cmd.env(k, v);
    }

    let mut child = cmd.spawn()?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("failed to capture child stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow::anyhow!("failed to capture child stderr"))?;

    let stdout_events = events.cloned();
    let stdout_job_id = job.id;
    let stdout_transcript_writer = Arc::clone(&transcript_writer);
    let stdout_task = tokio::spawn(async move {
        pump_process_stream(
            stdout,
            stdout_file,
            stdout_transcript_writer,
            "stdout",
            tokio::io::stdout(),
            stdout_events,
            stdout_job_id,
            pb::JobEventKind::JobEventStdout,
        )
        .await
    });

    let stderr_events = events.cloned();
    let stderr_job_id = job.id;
    let stderr_transcript_writer = Arc::clone(&transcript_writer);
    let stderr_task = tokio::spawn(async move {
        pump_process_stream(
            stderr,
            stderr_file,
            stderr_transcript_writer,
            "stderr",
            tokio::io::stderr(),
            stderr_events,
            stderr_job_id,
            pb::JobEventKind::JobEventStderr,
        )
        .await
    });

    let status = if let Some(timeout_secs) = timeout_secs {
        match tokio::time::timeout(Duration::from_secs(timeout_secs), child.wait()).await {
            Ok(status) => status?,
            Err(_) => {
                if let Some(events) = events {
                    events
                        .emit(
                            job.id,
                            pb::JobEventKind::JobEventSystem,
                            format!("timeout after {timeout_secs}s"),
                        )
                        .await;
                }
                let _ = child.kill().await;
                let _ = child.wait().await;
                return Ok(ExecutionStatus::TimedOut);
            }
        }
    } else {
        child.wait().await?
    };

    let _ = stdout_task.await;
    let _ = stderr_task.await;

    Ok(ExecutionStatus::Exited(status.code().unwrap_or(1)))
}

async fn pump_process_stream<R>(
    reader: R,
    mut log_file: tokio::io::BufWriter<tokio::fs::File>,
    transcript_writer: Arc<Mutex<tokio::io::BufWriter<tokio::fs::File>>>,
    stream: &'static str,
    mut parent_out: impl AsyncWrite + Unpin,
    events: Option<JobEventSink>,
    job_id: uuid::Uuid,
    kind: pb::JobEventKind,
) -> anyhow::Result<()>
where
    R: AsyncRead + Unpin,
{
    let mut lines = BufReader::new(reader).lines();
    while let Some(line) = lines.next_line().await? {
        log_file.write_all(line.as_bytes()).await?;
        log_file.write_all(b"\n").await?;
        log_file.flush().await?;

        parent_out.write_all(line.as_bytes()).await?;
        parent_out.write_all(b"\n").await?;
        parent_out.flush().await?;

        let record = serde_json::json!({
            "timestamp": Utc::now().to_rfc3339(),
            "stream": stream,
            "line": line,
        });
        let mut encoded = serde_json::to_vec(&record)?;
        encoded.push(b'\n');
        {
            let mut writer = transcript_writer.lock().await;
            writer.write_all(&encoded).await?;
            writer.flush().await?;
        }

        if let Some(events) = &events {
            let message = record
                .get("line")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string();
            events.emit(job_id, kind, message).await;
        }
    }
    Ok(())
}

async fn run_docker(
    job: &Job,
    workspace_dir: &Path,
    transcript_dir: &str,
    transcript_jsonl_path: &Path,
    stdout_path: &Path,
    stderr_path: &Path,
    env: &HashMap<String, String>,
    timeout_secs: Option<u64>,
    runtime_container_image: Option<String>,
    runtime_spec: &protocol::CanonicalRuntimeSpec,
    local_egress_plan: &LocalAgentEgressPlan,
    egress_manager: &EgressManager,
    primary_repo_mirror_dir: Option<&Path>,
    additional_repo_mounts: &[AdditionalRepoMount],
    events: Option<&JobEventSink>,
) -> anyhow::Result<ExecutionStatus> {
    let mut docker_env = env.clone();
    docker_env.insert("AGENT_WORKSPACE_DIR".to_string(), "/workspace".to_string());
    docker_env.insert(
        "AGENT_TRANSCRIPT_DIR".to_string(),
        "/transcript".to_string(),
    );
    docker_env.insert(
        "AGENT_CONTEXT_PATH".to_string(),
        "/transcript/context.json".to_string(),
    );
    docker_env.insert(
        "AGENT_AGENT_CONTEXT_PATH".to_string(),
        "/transcript/.agent-context.json".to_string(),
    );
    docker_env.insert(
        "AGENT_TRANSCRIPT_JSONL_PATH".to_string(),
        "/transcript/transcript.jsonl".to_string(),
    );
    docker_env.insert(
        "AGENT_JOB_EVENTS_JSONL_PATH".to_string(),
        "/transcript/job-events.jsonl".to_string(),
    );
    docker_env.insert(
        "AGENT_OUTPUT_PATH".to_string(),
        "/transcript/output.json".to_string(),
    );
    docker_env.insert(
        "AGENT_STDOUT_LOG_PATH".to_string(),
        "/transcript/stdout.log".to_string(),
    );
    docker_env.insert(
        "AGENT_STDERR_LOG_PATH".to_string(),
        "/transcript/stderr.log".to_string(),
    );
    docker_env.insert(
        "AGENT_RUN_JSON_PATH".to_string(),
        "/transcript/run.json".to_string(),
    );
    docker_env.insert(
        "AGENT_PROMPT_USER_PATH".to_string(),
        "/transcript/prompt/user.txt".to_string(),
    );
    docker_env.insert(
        "AGENT_PROMPT_SYSTEM_PATH".to_string(),
        "/transcript/prompt/system.txt".to_string(),
    );
    if job.repo.is_some() {
        append_git_config_env(&mut docker_env, "safe.directory", "/workspace");
        append_git_identity_env_defaults(&mut docker_env);
    }

    let mut args = vec!["run".to_string(), "--rm".to_string()];

    args.push("--security-opt=no-new-privileges".to_string());

    let memory_limit = runtime_spec
        .environment
        .memory_limit
        .as_deref()
        .unwrap_or(DEFAULT_DOCKER_MEMORY_LIMIT);
    let cpu_limit = runtime_spec
        .environment
        .cpu_limit
        .as_deref()
        .unwrap_or(DEFAULT_DOCKER_CPU_LIMIT);
    args.push(format!("--memory={memory_limit}"));
    args.push(format!("--cpus={cpu_limit}"));

    let mut docker_network_none = local_egress_plan.network_none;
    let restricted_tier = match local_egress_plan.tier {
        Some(LocalAgentEgressTier::Llm) | Some(LocalAgentEgressTier::Build) => {
            local_egress_plan.tier
        }
        _ => None,
    };
    if !runtime_spec.permissions.can_access_network && !docker_network_none {
        let warning =
            "WARNING: forcing --network=none because can_access_network is false in permissions";
        eprintln!("{warning}");
        if let Some(events) = events {
            events
                .emit(
                    job.id,
                    pb::JobEventKind::JobEventSystem,
                    warning.to_string(),
                )
                .await;
        }
        docker_network_none = true;
    }
    if docker_network_none {
        args.push("--network=none".to_string());
    } else if let Some(tier) = restricted_tier {
        for mount in &runtime_spec.environment.file_mounts {
            if is_restricted_breakout_mount(mount) {
                anyhow::bail!(
                    "restricted egress tier '{}' rejects breakout mount '{}:{}'",
                    tier.as_str(),
                    mount.host_path,
                    mount.container_path
                );
            }
        }

        let llm_provider = llm_provider_from_env(env);
        let restricted_runtime = egress_manager
            .prepare_restricted_tier(tier, llm_provider.as_deref())
            .await
            .with_context(|| {
                format!(
                    "failed preparing restricted egress tier '{}' for job {}",
                    tier.as_str(),
                    job.id
                )
            })?;
        args.push(format!("--network={}", restricted_runtime.network_name));

        for key in [
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "http_proxy",
            "https_proxy",
            "all_proxy",
        ] {
            docker_env.remove(key);
        }
        docker_env.insert(
            "HTTP_PROXY".to_string(),
            restricted_runtime.proxy_url.clone(),
        );
        docker_env.insert(
            "HTTPS_PROXY".to_string(),
            restricted_runtime.proxy_url.clone(),
        );
        docker_env.insert("NO_PROXY".to_string(), restricted_runtime.no_proxy.clone());
        docker_env.insert(
            "http_proxy".to_string(),
            restricted_runtime.proxy_url.clone(),
        );
        docker_env.insert(
            "https_proxy".to_string(),
            restricted_runtime.proxy_url.clone(),
        );
        docker_env.insert("no_proxy".to_string(), restricted_runtime.no_proxy.clone());
        docker_env.insert(
            "AGENT_RESTRICTED_EGRESS_HOSTS".to_string(),
            restricted_runtime.tier_hosts.join(","),
        );

        if matches!(tier, LocalAgentEgressTier::Build) {
            docker_env
                .entry("GOPROXY".to_string())
                .or_insert_with(|| "https://proxy.golang.org".to_string());
            docker_env
                .entry("GOSUMDB".to_string())
                .or_insert_with(|| "sum.golang.org".to_string());
            docker_env
                .entry("CARGO_REGISTRIES_CRATES_IO_PROTOCOL".to_string())
                .or_insert_with(|| "sparse".to_string());
        }
    }
    if let Some(tier) = local_egress_plan.tier
        && let Some(events) = events
    {
        events
            .emit(
                job.id,
                pb::JobEventKind::JobEventSystem,
                format!(
                    "local-agent egress tier resolved to '{}': MVP policy",
                    tier.as_str()
                ),
            )
            .await;
    }

    let workspace_mount_mode = if runtime_spec.permissions.can_write_workspace {
        "rw"
    } else {
        "ro"
    };
    if !runtime_spec.permissions.can_write_workspace {
        args.push("--read-only".to_string());
        args.push("--tmpfs".to_string());
        args.push("/tmp:rw,size=1g".to_string());
    }

    args.extend([
        "-v".to_string(),
        format!(
            "{}:/workspace:{workspace_mount_mode}",
            workspace_dir.to_string_lossy()
        ),
        "-v".to_string(),
        format!("{}:/transcript", transcript_dir),
        "-w".to_string(),
        "/workspace".to_string(),
    ]);

    if let Some(mirror_dir) = primary_repo_mirror_dir {
        let mirror_git_dir = mirror_dir.join(".git");
        args.push("-v".to_string());
        args.push(format!(
            "{}:{}:{workspace_mount_mode}",
            mirror_git_dir.to_string_lossy(),
            mirror_git_dir.to_string_lossy()
        ));
    }

    for mount in additional_repo_mounts {
        args.push("-v".to_string());
        args.push(format!(
            "{}:{}:ro",
            mount.host_path.to_string_lossy(),
            mount.container_path
        ));
    }

    append_gh_config_mount_for_toolchains(
        &mut args,
        &mut docker_env,
        &runtime_spec.environment.toolchains,
    );

    for mount in &runtime_spec.environment.file_mounts {
        let mut spec = format!("{}:{}", mount.host_path, mount.container_path);
        if mount.read_only {
            spec.push_str(":ro");
        }
        args.push("-v".to_string());
        args.push(spec);
    }
    for (k, v) in &docker_env {
        args.push("-e".to_string());
        args.push(format!("{k}={v}"));
    }
    args.push("--entrypoint".to_string());
    args.push(job.command.clone());
    args.push(
        runtime_container_image
            .clone()
            .unwrap_or_else(|| "alpine:3.20".to_string()),
    );
    args.extend(job.args.clone());

    let docker_job = Job {
        args,
        command: "docker".to_string(),
        ..job.clone()
    };
    run_raw(
        &docker_job,
        workspace_dir,
        transcript_jsonl_path,
        stdout_path,
        stderr_path,
        &HashMap::new(),
        timeout_secs,
        events,
    )
    .await
}

#[derive(Debug, Clone)]
struct DockerRuntimeInvocation {
    command: String,
    args: Vec<String>,
    env: HashMap<String, String>,
    timeout_secs: Option<u64>,
    workspace_dir: PathBuf,
    transcript_jsonl_path: PathBuf,
    stdout_path: PathBuf,
    stderr_path: PathBuf,
}

async fn run_docker_opencode_with_validation_retries(
    job: &Job,
    opencode: &OpencodeManagedSpec,
    workspace_dir: &Path,
    transcript_dir: &str,
    transcript_jsonl_path: &Path,
    stdout_path: &Path,
    stderr_path: &Path,
    output_path: &Path,
    env: &HashMap<String, String>,
    timeout_secs: Option<u64>,
    runtime_container_image: Option<String>,
    runtime_spec: &protocol::CanonicalRuntimeSpec,
    local_egress_plan: &LocalAgentEgressPlan,
    egress_manager: &EgressManager,
    primary_repo_mirror_dir: Option<&Path>,
    additional_repo_mounts: &[AdditionalRepoMount],
    valid_next_events: Vec<String>,
    work_item_context: serde_json::Value,
    events: Option<&JobEventSink>,
) -> anyhow::Result<ExecutionStatus> {
    let invocation = build_docker_runtime_invocation(
        job,
        workspace_dir,
        transcript_dir,
        transcript_jsonl_path,
        stdout_path,
        stderr_path,
        env,
        timeout_secs,
        runtime_container_image,
        runtime_spec,
        local_egress_plan,
        egress_manager,
        primary_repo_mirror_dir,
        additional_repo_mounts,
        events,
    )
    .await?;

    run_opencode_validation_loop(
        job,
        opencode,
        &invocation,
        output_path,
        &valid_next_events,
        &work_item_context,
        events,
    )
    .await
}

async fn run_opencode_validation_loop(
    job: &Job,
    opencode: &OpencodeManagedSpec,
    invocation: &DockerRuntimeInvocation,
    output_path: &Path,
    valid_next_events: &[String],
    work_item_context: &serde_json::Value,
    events: Option<&JobEventSink>,
) -> anyhow::Result<ExecutionStatus> {
    let max_attempts = opencode.max_validation_attempts.max(1);
    let _ = std::fs::remove_file(output_path);

    let container_name = format!("agent-hub-opencode-{}", job.id.simple());
    let mut attempt: u32 = 1;
    let mut continuation_prompt: Option<String> = None;
    let shell_prelude = opencode_shell_prelude(job);

    spawn_opencode_runtime_container(invocation, &container_name).await?;

    loop {
        if let Some(events) = events {
            events
                .emit(
                    job.id,
                    pb::JobEventKind::JobEventSystem,
                    format!(
                        "opencode validation attempt {attempt}/{max_attempts} ({})",
                        if continuation_prompt.is_none() {
                            "run"
                        } else {
                            "run --continue"
                        }
                    ),
                )
                .await;
        }

        let status = run_docker_opencode_attempt(
            job,
            invocation,
            &container_name,
            continuation_prompt.as_deref(),
            &shell_prelude,
            attempt > 1,
            events,
        )
        .await?;

        if !matches!(status, ExecutionStatus::Exited(0)) {
            if let Err(error) = cleanup_opencode_container(&container_name).await {
                eprintln!(
                    "warning: failed to cleanup opencode container {}: {error:#}",
                    container_name
                );
            }
            return Ok(status);
        }

        match validate_opencode_output(output_path, valid_next_events, work_item_context) {
            Ok(()) => {
                if let Some(events) = events {
                    events
                        .emit(
                            job.id,
                            pb::JobEventKind::JobEventSystem,
                            format!("opencode output validation passed on attempt {attempt}"),
                        )
                        .await;
                }
                if let Err(error) = cleanup_opencode_container(&container_name).await {
                    eprintln!(
                        "warning: failed to cleanup opencode container {}: {error:#}",
                        container_name
                    );
                }
                return Ok(status);
            }
            Err(validation_error) => {
                if attempt >= max_attempts {
                    if let Some(events) = events {
                        events
                            .emit(
                                job.id,
                                pb::JobEventKind::JobEventSystem,
                                format!(
                                    "opencode output validation failed after {attempt} attempts: {validation_error}"
                                ),
                            )
                            .await;
                    }
                    if let Err(error) = cleanup_opencode_container(&container_name).await {
                        eprintln!(
                            "warning: failed to cleanup opencode container {}: {error:#}",
                            container_name
                        );
                    }
                    anyhow::bail!(
                        "opencode validation failed after {attempt} attempts: {validation_error}"
                    );
                }

                if let Some(events) = events {
                    events
                        .emit(
                            job.id,
                            pb::JobEventKind::JobEventSystem,
                            format!(
                                "opencode output validation failed (attempt {attempt}/{max_attempts}): {validation_error}; retrying with run --continue"
                            ),
                        )
                        .await;
                }
                continuation_prompt = Some(build_opencode_correction_prompt(
                    valid_next_events,
                    &validation_error.to_string(),
                ));
                attempt = attempt.saturating_add(1);
            }
        }
    }
}

async fn run_docker_opencode_attempt(
    job: &Job,
    invocation: &DockerRuntimeInvocation,
    container_name: &str,
    continuation_prompt: Option<&str>,
    shell_prelude: &str,
    append_logs: bool,
    events: Option<&JobEventSink>,
) -> anyhow::Result<ExecutionStatus> {
    let opencode_cmd = if let Some(prompt) = continuation_prompt {
        build_opencode_continue_script(shell_prelude, prompt)
    } else {
        String::new()
    };

    let docker_job = Job {
        command: invocation.command.clone(),
        args: if continuation_prompt.is_some() {
            vec![
                "exec".to_string(),
                container_name.to_string(),
                "sh".to_string(),
                "-lc".to_string(),
                opencode_cmd,
            ]
        } else {
            let mut exec_args = vec!["exec".to_string(), container_name.to_string()];
            exec_args.push(job.command.clone());
            exec_args.extend(job.args.clone());
            exec_args
        },
        ..job.clone()
    };

    if append_logs {
        run_raw_append(
            &docker_job,
            &invocation.workspace_dir,
            &invocation.transcript_jsonl_path,
            &invocation.stdout_path,
            &invocation.stderr_path,
            &invocation.env,
            invocation.timeout_secs,
            events,
        )
        .await
    } else {
        run_raw(
            &docker_job,
            &invocation.workspace_dir,
            &invocation.transcript_jsonl_path,
            &invocation.stdout_path,
            &invocation.stderr_path,
            &invocation.env,
            invocation.timeout_secs,
            events,
        )
        .await
    }
}

async fn spawn_opencode_runtime_container(
    invocation: &DockerRuntimeInvocation,
    container_name: &str,
) -> anyhow::Result<()> {
    let _ = tokio::process::Command::new("docker")
        .arg("rm")
        .arg("-f")
        .arg(container_name)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;

    let run_args = build_opencode_spawn_run_args(&invocation.args, container_name)?;

    let mut cmd = tokio::process::Command::new(&invocation.command);
    cmd.args(&run_args).current_dir(&invocation.workspace_dir);
    for (k, v) in &invocation.env {
        cmd.env(k, v);
    }

    let status = cmd.status().await?;
    if status.success() {
        Ok(())
    } else {
        anyhow::bail!(
            "failed to start opencode runtime container {}: {}",
            container_name,
            status
        )
    }
}

fn build_opencode_spawn_run_args(
    invocation_args: &[String],
    container_name: &str,
) -> anyhow::Result<Vec<String>> {
    let entry_idx = invocation_args
        .iter()
        .position(|arg| arg == "--entrypoint")
        .context("docker invocation missing --entrypoint")?;
    let image_idx = entry_idx.saturating_add(2);
    if image_idx >= invocation_args.len() {
        anyhow::bail!("docker invocation missing image argument");
    }

    let mut run_args = invocation_args[..entry_idx].to_vec();
    run_args.retain(|arg| arg != "--rm");
    run_args.push("--name".to_string());
    run_args.push(container_name.to_string());
    run_args.push("-d".to_string());
    run_args.push("--entrypoint".to_string());
    run_args.push("sh".to_string());
    run_args.push(invocation_args[image_idx].clone());
    run_args.push("-lc".to_string());
    run_args.push("while true; do sleep 600; done".to_string());
    Ok(run_args)
}

fn opencode_shell_prelude(job: &Job) -> String {
    if job.command != "sh" {
        return String::new();
    }
    if job.args.first().map(String::as_str) != Some("-lc") {
        return String::new();
    }
    let Some(script) = job.args.get(1) else {
        return String::new();
    };
    let marker = "if [ -f \"$AGENT_PROMPT_USER_PATH\" ]";
    let Some((prefix, _)) = script.split_once(marker) else {
        return String::new();
    };
    prefix.trim().trim_end_matches(';').trim().to_string()
}

fn build_opencode_continue_script(shell_prelude: &str, prompt: &str) -> String {
    let mut script = String::new();
    if !shell_prelude.is_empty() {
        script.push_str(shell_prelude);
        script.push_str("; ");
    }
    script.push_str("cat <<'AGENT_HUB_RETRY_PROMPT' >/tmp/agent-hub-retry-prompt.txt\n");
    script.push_str(prompt);
    script.push_str(
        "\nAGENT_HUB_RETRY_PROMPT\nopencode run --continue < /tmp/agent-hub-retry-prompt.txt",
    );
    script
}

fn validate_opencode_output(
    output_path: &Path,
    valid_next_events: &[String],
    work_item_context: &serde_json::Value,
) -> anyhow::Result<()> {
    if !output_path.exists() {
        anyhow::bail!("output.json missing")
    }

    let bytes = std::fs::read(output_path).context("read output.json")?;
    let raw_output: serde_json::Value =
        serde_json::from_slice(&bytes).context("output.json parse failed")?;
    let typed_output: AgentOutput = serde_json::from_value(raw_output.clone())
        .context("output.json is not valid AgentOutput")?;

    validate_agent_output_next_event(&typed_output, valid_next_events)
        .map_err(anyhow::Error::msg)?;

    validate_simple_code_change_repo_rules(work_item_context, &typed_output)?;
    Ok(())
}

fn validate_simple_code_change_repo_rules(
    work_item_context: &serde_json::Value,
    output: &AgentOutput,
) -> anyhow::Result<()> {
    let workflow_id = work_item_context
        .get("workflow_id")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let workflow_state = work_item_context
        .get("workflow_state")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    if workflow_id != "simple-code-change" || workflow_state != "triage_investigation" {
        return Ok(());
    }

    let next_event = output.next_event.as_deref().unwrap_or_default();
    if next_event != "job_succeeded" {
        return Ok(());
    }

    let repo = output
        .repo
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("repo is required for simple-code-change triage success"))?;

    let repo_url = repo.repo_url.trim();
    let base_ref = repo.base_ref.trim();
    if repo_url.is_empty() || repo_url.contains("placeholder") || repo_url.contains("example") {
        anyhow::bail!("repo.repo_url must be a concrete repository URL for triage success");
    }
    if base_ref.is_empty() || base_ref.contains("placeholder") {
        anyhow::bail!("repo.base_ref must be concrete for triage success");
    }

    let candidate_repo_urls = work_item_context
        .get("repo_catalog")
        .and_then(serde_json::Value::as_array)
        .map(|catalog| {
            catalog
                .iter()
                .filter_map(serde_json::Value::as_object)
                .filter_map(|entry| entry.get("repo_url"))
                .filter_map(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToOwned::to_owned)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if !candidate_repo_urls.is_empty() && !candidate_repo_urls.iter().any(|url| url == repo_url) {
        anyhow::bail!(
            "repo.repo_url '{}' must match one of work_item.repo_catalog candidates",
            repo_url
        );
    }

    Ok(())
}

fn build_opencode_correction_prompt(
    valid_next_events: &[String],
    validation_error: &str,
) -> String {
    format!(
        "Your previous output.json failed validation in local-agent.\n\nValidation error:\n{validation_error}\n\nYou must write a corrected /transcript/output.json that:\n1) parses as AgentOutput JSON\n2) sets next_event to one of: {:?}\n3) if next_event is job_succeeded for simple-code-change triage, include a concrete repo object with non-empty repo_url and base_ref.\n\nOnly fix output.json and avoid unrelated changes.",
        valid_next_events
    )
}

async fn cleanup_opencode_container(container_name: &str) -> anyhow::Result<()> {
    let mut cmd = tokio::process::Command::new("docker");
    cmd.arg("rm").arg("-f").arg(container_name);
    let status = cmd.status().await?;
    if status.success() {
        Ok(())
    } else {
        anyhow::bail!("docker rm -f {} failed with {}", container_name, status)
    }
}

async fn build_docker_runtime_invocation(
    job: &Job,
    workspace_dir: &Path,
    transcript_dir: &str,
    transcript_jsonl_path: &Path,
    stdout_path: &Path,
    stderr_path: &Path,
    env: &HashMap<String, String>,
    timeout_secs: Option<u64>,
    runtime_container_image: Option<String>,
    runtime_spec: &protocol::CanonicalRuntimeSpec,
    local_egress_plan: &LocalAgentEgressPlan,
    egress_manager: &EgressManager,
    primary_repo_mirror_dir: Option<&Path>,
    additional_repo_mounts: &[AdditionalRepoMount],
    events: Option<&JobEventSink>,
) -> anyhow::Result<DockerRuntimeInvocation> {
    let mut docker_env = env.clone();
    docker_env.insert("AGENT_WORKSPACE_DIR".to_string(), "/workspace".to_string());
    docker_env.insert(
        "AGENT_TRANSCRIPT_DIR".to_string(),
        "/transcript".to_string(),
    );
    docker_env.insert(
        "AGENT_CONTEXT_PATH".to_string(),
        "/transcript/context.json".to_string(),
    );
    docker_env.insert(
        "AGENT_AGENT_CONTEXT_PATH".to_string(),
        "/transcript/.agent-context.json".to_string(),
    );
    docker_env.insert(
        "AGENT_TRANSCRIPT_JSONL_PATH".to_string(),
        "/transcript/transcript.jsonl".to_string(),
    );
    docker_env.insert(
        "AGENT_JOB_EVENTS_JSONL_PATH".to_string(),
        "/transcript/job-events.jsonl".to_string(),
    );
    docker_env.insert(
        "AGENT_OUTPUT_PATH".to_string(),
        "/transcript/output.json".to_string(),
    );
    docker_env.insert(
        "AGENT_STDOUT_LOG_PATH".to_string(),
        "/transcript/stdout.log".to_string(),
    );
    docker_env.insert(
        "AGENT_STDERR_LOG_PATH".to_string(),
        "/transcript/stderr.log".to_string(),
    );
    docker_env.insert(
        "AGENT_RUN_JSON_PATH".to_string(),
        "/transcript/run.json".to_string(),
    );
    docker_env.insert(
        "AGENT_PROMPT_USER_PATH".to_string(),
        "/transcript/prompt/user.txt".to_string(),
    );
    docker_env.insert(
        "AGENT_PROMPT_SYSTEM_PATH".to_string(),
        "/transcript/prompt/system.txt".to_string(),
    );
    if job.repo.is_some() {
        append_git_config_env(&mut docker_env, "safe.directory", "/workspace");
        append_git_identity_env_defaults(&mut docker_env);
    }

    let mut args = vec!["run".to_string(), "--rm".to_string()];

    args.push("--security-opt=no-new-privileges".to_string());

    let memory_limit = runtime_spec
        .environment
        .memory_limit
        .as_deref()
        .unwrap_or(DEFAULT_DOCKER_MEMORY_LIMIT);
    let cpu_limit = runtime_spec
        .environment
        .cpu_limit
        .as_deref()
        .unwrap_or(DEFAULT_DOCKER_CPU_LIMIT);
    args.push(format!("--memory={memory_limit}"));
    args.push(format!("--cpus={cpu_limit}"));

    let mut docker_network_none = local_egress_plan.network_none;
    let restricted_tier = match local_egress_plan.tier {
        Some(LocalAgentEgressTier::Llm) | Some(LocalAgentEgressTier::Build) => {
            local_egress_plan.tier
        }
        _ => None,
    };
    if !runtime_spec.permissions.can_access_network && !docker_network_none {
        let warning =
            "WARNING: forcing --network=none because can_access_network is false in permissions";
        eprintln!("{warning}");
        if let Some(events) = events {
            events
                .emit(
                    job.id,
                    pb::JobEventKind::JobEventSystem,
                    warning.to_string(),
                )
                .await;
        }
        docker_network_none = true;
    }
    if docker_network_none {
        args.push("--network=none".to_string());
    } else if let Some(tier) = restricted_tier {
        for mount in &runtime_spec.environment.file_mounts {
            if is_restricted_breakout_mount(mount) {
                anyhow::bail!(
                    "restricted egress tier '{}' rejects breakout mount '{}:{}'",
                    tier.as_str(),
                    mount.host_path,
                    mount.container_path
                );
            }
        }

        let llm_provider = llm_provider_from_env(env);
        let restricted_runtime = egress_manager
            .prepare_restricted_tier(tier, llm_provider.as_deref())
            .await
            .with_context(|| {
                format!(
                    "failed preparing restricted egress tier '{}' for job {}",
                    tier.as_str(),
                    job.id
                )
            })?;
        args.push(format!("--network={}", restricted_runtime.network_name));

        for key in [
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "http_proxy",
            "https_proxy",
            "all_proxy",
        ] {
            docker_env.remove(key);
        }
        docker_env.insert(
            "HTTP_PROXY".to_string(),
            restricted_runtime.proxy_url.clone(),
        );
        docker_env.insert(
            "HTTPS_PROXY".to_string(),
            restricted_runtime.proxy_url.clone(),
        );
        docker_env.insert("NO_PROXY".to_string(), restricted_runtime.no_proxy.clone());
        docker_env.insert(
            "http_proxy".to_string(),
            restricted_runtime.proxy_url.clone(),
        );
        docker_env.insert(
            "https_proxy".to_string(),
            restricted_runtime.proxy_url.clone(),
        );
        docker_env.insert("no_proxy".to_string(), restricted_runtime.no_proxy.clone());
        docker_env.insert(
            "AGENT_RESTRICTED_EGRESS_HOSTS".to_string(),
            restricted_runtime.tier_hosts.join(","),
        );

        if matches!(tier, LocalAgentEgressTier::Build) {
            docker_env
                .entry("GOPROXY".to_string())
                .or_insert_with(|| "https://proxy.golang.org".to_string());
            docker_env
                .entry("GOSUMDB".to_string())
                .or_insert_with(|| "sum.golang.org".to_string());
            docker_env
                .entry("CARGO_REGISTRIES_CRATES_IO_PROTOCOL".to_string())
                .or_insert_with(|| "sparse".to_string());
        }
    }
    if let Some(tier) = local_egress_plan.tier
        && let Some(events) = events
    {
        events
            .emit(
                job.id,
                pb::JobEventKind::JobEventSystem,
                format!(
                    "local-agent egress tier resolved to '{}': MVP policy",
                    tier.as_str()
                ),
            )
            .await;
    }

    let workspace_mount_mode = if runtime_spec.permissions.can_write_workspace {
        "rw"
    } else {
        "ro"
    };
    if !runtime_spec.permissions.can_write_workspace {
        args.push("--read-only".to_string());
        args.push("--tmpfs".to_string());
        args.push("/tmp:rw,size=1g".to_string());
    }

    args.extend([
        "-v".to_string(),
        format!(
            "{}:/workspace:{workspace_mount_mode}",
            workspace_dir.to_string_lossy()
        ),
        "-v".to_string(),
        format!("{}:/transcript", transcript_dir),
        "-w".to_string(),
        "/workspace".to_string(),
    ]);

    if let Some(mirror_dir) = primary_repo_mirror_dir {
        let mirror_git_dir = mirror_dir.join(".git");
        args.push("-v".to_string());
        args.push(format!(
            "{}:{}:{workspace_mount_mode}",
            mirror_git_dir.to_string_lossy(),
            mirror_git_dir.to_string_lossy()
        ));
    }

    for mount in additional_repo_mounts {
        args.push("-v".to_string());
        args.push(format!(
            "{}:{}:ro",
            mount.host_path.to_string_lossy(),
            mount.container_path
        ));
    }

    append_gh_config_mount_for_toolchains(
        &mut args,
        &mut docker_env,
        &runtime_spec.environment.toolchains,
    );

    for mount in &runtime_spec.environment.file_mounts {
        let mut spec = format!("{}:{}", mount.host_path, mount.container_path);
        if mount.read_only {
            spec.push_str(":ro");
        }
        args.push("-v".to_string());
        args.push(spec);
    }
    for (k, v) in &docker_env {
        args.push("-e".to_string());
        args.push(format!("{k}={v}"));
    }
    args.push("--entrypoint".to_string());
    args.push(job.command.clone());
    args.push(
        runtime_container_image
            .clone()
            .unwrap_or_else(|| "alpine:3.20".to_string()),
    );
    args.extend(job.args.clone());

    Ok(DockerRuntimeInvocation {
        command: "docker".to_string(),
        args,
        env: HashMap::new(),
        timeout_secs,
        workspace_dir: workspace_dir.to_path_buf(),
        transcript_jsonl_path: transcript_jsonl_path.to_path_buf(),
        stdout_path: stdout_path.to_path_buf(),
        stderr_path: stderr_path.to_path_buf(),
    })
}

fn append_git_config_env(env: &mut HashMap<String, String>, key: &str, value: &str) {
    let count = env
        .get("GIT_CONFIG_COUNT")
        .and_then(|raw| raw.parse::<usize>().ok())
        .unwrap_or(0);
    env.insert(format!("GIT_CONFIG_KEY_{count}"), key.to_string());
    env.insert(format!("GIT_CONFIG_VALUE_{count}"), value.to_string());
    env.insert("GIT_CONFIG_COUNT".to_string(), (count + 1).to_string());
}

const GH_CONFIG_CONTAINER_DIR: &str = "/gh-config";

fn append_gh_config_mount_for_toolchains(
    args: &mut Vec<String>,
    docker_env: &mut HashMap<String, String>,
    toolchains: &[String],
) {
    if let Some(host_gh_config_dir) = resolve_gh_config_mount_source(
        toolchains,
        std::env::var_os("GH_CONFIG_DIR").as_deref(),
        std::env::var_os("HOME").as_deref(),
    ) {
        args.push("-v".to_string());
        args.push(format!(
            "{}:{GH_CONFIG_CONTAINER_DIR}:ro",
            host_gh_config_dir.to_string_lossy()
        ));
        docker_env.insert(
            "GH_CONFIG_DIR".to_string(),
            GH_CONFIG_CONTAINER_DIR.to_string(),
        );
    }
}

fn resolve_gh_config_mount_source(
    toolchains: &[String],
    gh_config_dir: Option<&std::ffi::OsStr>,
    home_dir: Option<&std::ffi::OsStr>,
) -> Option<PathBuf> {
    if !toolchains.iter().any(|toolchain| toolchain == "gh") {
        return None;
    }

    let candidate = if let Some(explicit) = gh_config_dir {
        PathBuf::from(explicit)
    } else {
        PathBuf::from(home_dir?).join(".config/gh")
    };

    if candidate.is_dir() {
        Some(candidate)
    } else {
        None
    }
}

fn append_git_identity_env_defaults(env: &mut HashMap<String, String>) {
    for (key, value) in [
        ("GIT_AUTHOR_NAME", "Agent Hub"),
        ("GIT_AUTHOR_EMAIL", "agent-hub@example.com"),
        ("GIT_COMMITTER_NAME", "Agent Hub"),
        ("GIT_COMMITTER_EMAIL", "agent-hub@example.com"),
    ] {
        env.entry(key.to_string()).or_insert_with(|| value.to_string());
    }
}

fn is_restricted_breakout_mount(mount: &protocol::orchestration::PlannedFileMount) -> bool {
    let host = normalize_mount_path_for_policy(&mount.host_path);
    let container = normalize_mount_path_for_policy(&mount.container_path);

    let dangerous_exact = [
        "/",
        "/proc",
        "/sys",
        "/dev",
        "/run/netns",
        "/var/run/docker.sock",
        "/run/docker.sock",
    ];
    if dangerous_exact
        .iter()
        .any(|path| host == *path || container == *path)
    {
        return true;
    }

    let dangerous_prefixes = [
        "/proc/",
        "/sys/",
        "/dev/",
        "/run/netns/",
        "/var/lib/docker/",
        "/var/run/docker.sock/",
        "/run/docker.sock/",
    ];
    if dangerous_prefixes
        .iter()
        .any(|prefix| host.starts_with(prefix) || container.starts_with(prefix))
    {
        return true;
    }

    host.contains("containerd.sock")
        || container.contains("containerd.sock")
        || host.contains("crio.sock")
        || container.contains("crio.sock")
}

fn normalize_mount_path_for_policy(path: &str) -> String {
    let normalized = path.trim().to_ascii_lowercase();
    if normalized == "/" {
        return normalized;
    }
    normalized.trim_end_matches('/').to_string()
}

#[cfg(test)]
mod restricted_mount_tests {
    use super::{
        append_gh_config_mount_for_toolchains, append_git_config_env,
        append_git_identity_env_defaults, is_restricted_breakout_mount,
        resolve_gh_config_mount_source,
    };
    use std::collections::HashMap;

    #[test]
    fn restricted_mount_rejects_obvious_breakout_paths() {
        let dangerous = [
            ("/var/run/docker.sock", "/var/run/docker.sock"),
            ("/", "/host"),
            ("/proc", "/host-proc"),
            ("/sys", "/host-sys"),
            ("/dev", "/host-dev"),
            ("/run/netns", "/netns"),
        ];

        for (host, container) in dangerous {
            let mount = protocol::orchestration::PlannedFileMount {
                host_path: host.to_string(),
                container_path: container.to_string(),
                read_only: true,
            };
            assert!(
                is_restricted_breakout_mount(&mount),
                "{} -> {}",
                host,
                container
            );
        }
    }

    #[test]
    fn restricted_mount_allows_normal_project_mounts() {
        let mount = protocol::orchestration::PlannedFileMount {
            host_path: "/home/user/project/.cargo".to_string(),
            container_path: "/workspace/.cargo".to_string(),
            read_only: false,
        };
        assert!(!is_restricted_breakout_mount(&mount));
    }

    #[test]
    fn append_git_config_env_sets_first_entry() {
        let mut env = HashMap::new();
        append_git_config_env(&mut env, "safe.directory", "/workspace");

        assert_eq!(env.get("GIT_CONFIG_COUNT").map(String::as_str), Some("1"));
        assert_eq!(
            env.get("GIT_CONFIG_KEY_0").map(String::as_str),
            Some("safe.directory")
        );
        assert_eq!(
            env.get("GIT_CONFIG_VALUE_0").map(String::as_str),
            Some("/workspace")
        );
    }

    #[test]
    fn append_git_config_env_appends_to_existing_entries() {
        let mut env = HashMap::from([
            ("GIT_CONFIG_COUNT".to_string(), "1".to_string()),
            ("GIT_CONFIG_KEY_0".to_string(), "user.email".to_string()),
            (
                "GIT_CONFIG_VALUE_0".to_string(),
                "agent@example.com".to_string(),
            ),
        ]);

        append_git_config_env(&mut env, "safe.directory", "/workspace");

        assert_eq!(env.get("GIT_CONFIG_COUNT").map(String::as_str), Some("2"));
        assert_eq!(
            env.get("GIT_CONFIG_KEY_1").map(String::as_str),
            Some("safe.directory")
        );
        assert_eq!(
            env.get("GIT_CONFIG_VALUE_1").map(String::as_str),
            Some("/workspace")
        );
    }

    #[test]
    fn append_git_identity_env_defaults_sets_all_values() {
        let mut env = HashMap::new();

        append_git_identity_env_defaults(&mut env);

        assert_eq!(
            env.get("GIT_AUTHOR_NAME").map(String::as_str),
            Some("Agent Hub")
        );
        assert_eq!(
            env.get("GIT_AUTHOR_EMAIL").map(String::as_str),
            Some("agent-hub@example.com")
        );
        assert_eq!(
            env.get("GIT_COMMITTER_NAME").map(String::as_str),
            Some("Agent Hub")
        );
        assert_eq!(
            env.get("GIT_COMMITTER_EMAIL").map(String::as_str),
            Some("agent-hub@example.com")
        );
    }

    #[test]
    fn append_git_identity_env_defaults_preserves_existing_values() {
        let mut env = HashMap::from([
            ("GIT_AUTHOR_NAME".to_string(), "Existing Author".to_string()),
            (
                "GIT_COMMITTER_EMAIL".to_string(),
                "existing@example.com".to_string(),
            ),
        ]);

        append_git_identity_env_defaults(&mut env);

        assert_eq!(
            env.get("GIT_AUTHOR_NAME").map(String::as_str),
            Some("Existing Author")
        );
        assert_eq!(
            env.get("GIT_AUTHOR_EMAIL").map(String::as_str),
            Some("agent-hub@example.com")
        );
        assert_eq!(
            env.get("GIT_COMMITTER_NAME").map(String::as_str),
            Some("Agent Hub")
        );
        assert_eq!(
            env.get("GIT_COMMITTER_EMAIL").map(String::as_str),
            Some("existing@example.com")
        );
    }

    #[test]
    fn resolve_gh_config_mount_source_prefers_explicit_dir() {
        let temp_root = std::env::temp_dir().join(format!("local-agent-gh-test-{}", uuid::Uuid::new_v4()));
        let explicit = temp_root.join("explicit-gh");
        let home_dir = temp_root.join("home");
        let fallback = home_dir.join(".config/gh");
        std::fs::create_dir_all(&explicit).expect("create explicit dir");
        std::fs::create_dir_all(&fallback).expect("create fallback dir");

        let resolved = resolve_gh_config_mount_source(
            &["gh".to_string()],
            Some(explicit.as_os_str()),
            Some(home_dir.as_os_str()),
        );

        assert_eq!(resolved, Some(explicit.clone()));
        let _ = std::fs::remove_dir_all(temp_root);
    }

    #[test]
    fn resolve_gh_config_mount_source_falls_back_to_home_config() {
        let temp_root = std::env::temp_dir().join(format!("local-agent-gh-test-{}", uuid::Uuid::new_v4()));
        let home_dir = temp_root.join("home");
        let gh_dir = home_dir.join(".config/gh");
        std::fs::create_dir_all(&gh_dir).expect("create gh dir");

        let resolved =
            resolve_gh_config_mount_source(&["gh".to_string()], None, Some(home_dir.as_os_str()));

        assert_eq!(resolved, Some(gh_dir));
        let _ = std::fs::remove_dir_all(temp_root);
    }

    #[test]
    fn resolve_gh_config_mount_source_requires_gh_toolchain() {
        let temp_root = std::env::temp_dir().join(format!("local-agent-gh-test-{}", uuid::Uuid::new_v4()));
        let explicit = temp_root.join("explicit-gh");
        std::fs::create_dir_all(&explicit).expect("create explicit dir");

        let resolved = resolve_gh_config_mount_source(
            &["git".to_string()],
            Some(explicit.as_os_str()),
            Some(temp_root.as_os_str()),
        );

        assert!(resolved.is_none());
        let _ = std::fs::remove_dir_all(temp_root);
    }

    #[test]
    fn append_gh_config_mount_for_toolchains_noops_without_gh_toolchain() {
        let mut args = Vec::new();
        let mut env = HashMap::new();
        append_gh_config_mount_for_toolchains(&mut args, &mut env, &["git".to_string()]);

        assert!(args.is_empty());
        assert!(!env.contains_key("GH_CONFIG_DIR"));
    }
}

fn file_len_u64(path: &Path) -> Option<u64> {
    std::fs::metadata(path).ok().map(|m| m.len())
}

fn file_line_count_u64(path: &Path) -> Option<u64> {
    use std::io::BufRead;

    let file = std::fs::File::open(path).ok()?;
    let reader = std::io::BufReader::new(file);
    Some(reader.lines().count() as u64)
}

fn write_agent_context_file(
    agent_context_path: &Path,
    job: &Job,
    transcript_dir: &str,
    workspace_dir: &Path,
    runtime_spec: &protocol::CanonicalRuntimeSpec,
    additional_repo_mounts: &[AdditionalRepoMount],
) -> anyhow::Result<()> {
    let (workspace_dir_display, transcript_dir_display) =
        display_paths_for_runtime(workspace_dir, transcript_dir, runtime_spec);
    let transcript_root = PathBuf::from(&transcript_dir_display);
    let workflow_inputs = workflow_inputs_contract_from_job_context(&job.context);
    let work_item = work_item_contract_from_job_context(&job.context);
    let workflow_valid_next_events = workflow_inputs
        .as_object()
        .and_then(|obj| obj.get("valid_next_events"))
        .cloned()
        .unwrap_or_else(|| serde_json::Value::Array(Vec::new()));
    let artifact_paths = workflow_inputs
        .as_object()
        .and_then(|obj| obj.get("job_artifact_paths"))
        .cloned()
        .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new()));
    let artifact_metadata = workflow_inputs
        .as_object()
        .and_then(|obj| obj.get("job_artifact_metadata"))
        .cloned()
        .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new()));
    let valid_next_events = workflow_valid_next_events
        .as_array()
        .map(|events| {
            events
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(ToOwned::to_owned)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let repo_value = serde_json::to_value(&job.repo)?;
    let additional_repo_values = additional_repo_mounts
        .iter()
        .map(|mount| {
            serde_json::json!({
                "url": mount.repo_url,
                "path": mount.container_path,
                "writable": mount.writable,
            })
        })
        .collect::<Vec<_>>();
    let execution_mode = serde_json::to_value(&runtime_spec.environment.execution_mode)?
        .as_str()
        .unwrap_or_default()
        .to_string();
    let primary_repo_value = job.repo.as_ref().map(|repo| {
        serde_json::json!({
            "url": repo.repo_url,
            "path": workspace_dir_display,
            "writable": true,
        })
    });
    let payload = AgentContext {
        schema: AGENT_CONTEXT_SCHEMA_VERSION.to_string(),
        job: AgentContextJob {
            id: job.id,
            work_item_id: Some(job.work_item_id),
            step_id: job.step_id.clone(),
            attempt: job.attempt,
            max_retries: job.max_retries,
            execution_mode,
            timeout_secs: runtime_spec.environment.timeout_secs,
            container_image: runtime_spec.environment.container_image.clone(),
        },
        paths: AgentContextPaths {
            workspace_dir: workspace_dir_display,
            transcript_dir: transcript_dir_display,
            context_json: transcript_root
                .join("context.json")
                .to_string_lossy()
                .to_string(),
            agent_context_json: transcript_root
                .join(".agent-context.json")
                .to_string_lossy()
                .to_string(),
            output_json: transcript_root
                .join("output.json")
                .to_string_lossy()
                .to_string(),
            run_json: transcript_root
                .join("run.json")
                .to_string_lossy()
                .to_string(),
            transcript_jsonl: transcript_root
                .join("transcript.jsonl")
                .to_string_lossy()
                .to_string(),
            job_events_jsonl: transcript_root
                .join("job-events.jsonl")
                .to_string_lossy()
                .to_string(),
            stdout_log: transcript_root
                .join("stdout.log")
                .to_string_lossy()
                .to_string(),
            stderr_log: transcript_root
                .join("stderr.log")
                .to_string_lossy()
                .to_string(),
            prompt_user: transcript_root
                .join("prompt")
                .join("user.txt")
                .to_string_lossy()
                .to_string(),
            prompt_system: transcript_root
                .join("prompt")
                .join("system.txt")
                .to_string_lossy()
                .to_string(),
        },
        work_item: Some(work_item),
        workflow: AgentContextWorkflow {
            inputs: workflow_inputs,
            valid_next_events,
        },
        artifacts: AgentContextArtifacts {
            paths: artifact_paths,
            metadata: artifact_metadata,
        },
        repo: Some(repo_value.clone()),
        repos: AgentContextRepos {
            primary: primary_repo_value,
            additional: additional_repo_values,
        },
        permissions: Some(runtime_spec.permissions.clone()),
        approval_rules: runtime_spec.approval_rules.clone(),
        generated_at: Utc::now(),
        advisory: true,
    };
    std::fs::write(agent_context_path, serde_json::to_vec_pretty(&payload)?)?;
    Ok(())
}

fn collect_structured_transcript_artifacts(
    agent_context_path: &Path,
    run_json_path: &Path,
    transcript_jsonl_path: &Path,
    job_events_path: &Path,
) -> Vec<StructuredTranscriptArtifact> {
    let mut artifacts = Vec::new();

    if agent_context_path.exists() {
        let agent_context_inline = inline_agent_context_json(agent_context_path);
        let agent_context_schema = agent_context_inline
            .as_ref()
            .and_then(|v| v.get("schema"))
            .and_then(serde_json::Value::as_str)
            .map(ToOwned::to_owned);
        artifacts.push(StructuredTranscriptArtifact {
            key: "agent_context".to_string(),
            path: agent_context_path.to_string_lossy().to_string(),
            bytes: file_len_u64(agent_context_path),
            schema: agent_context_schema,
            record_count: None,
            inline_json: agent_context_inline,
        });
    }

    if run_json_path.exists() {
        let run_json_inline = inline_run_json(run_json_path);
        let run_json_schema = run_json_inline
            .as_ref()
            .and_then(|v| v.get("schema"))
            .and_then(serde_json::Value::as_str)
            .map(ToOwned::to_owned);
        artifacts.push(StructuredTranscriptArtifact {
            key: "run_json".to_string(),
            path: run_json_path.to_string_lossy().to_string(),
            bytes: file_len_u64(run_json_path),
            schema: run_json_schema,
            record_count: None,
            inline_json: run_json_inline,
        });
    }

    if transcript_jsonl_path.exists() {
        let transcript_jsonl_inline = inline_transcript_jsonl_tail(
            transcript_jsonl_path,
            TRANSCRIPT_JSONL_INLINE_TAIL_LIMIT,
            TRANSCRIPT_JSONL_INLINE_MAX_BYTES,
        );
        artifacts.push(StructuredTranscriptArtifact {
            key: "transcript_jsonl".to_string(),
            path: transcript_jsonl_path.to_string_lossy().to_string(),
            bytes: file_len_u64(transcript_jsonl_path),
            // transcript_jsonl is intentionally schema-free in this thin slice; bounded raw-line
            // array previews are used instead of a typed transcript contract.
            schema: None,
            record_count: file_line_count_u64(transcript_jsonl_path),
            inline_json: transcript_jsonl_inline,
        });
    }

    if job_events_path.exists() {
        let job_events_inline = inline_job_events_json_tail(
            job_events_path,
            JOB_EVENTS_INLINE_TAIL_LIMIT,
            JOB_EVENTS_INLINE_MAX_BYTES,
        );
        artifacts.push(StructuredTranscriptArtifact {
            key: "job_events".to_string(),
            path: job_events_path.to_string_lossy().to_string(),
            bytes: file_len_u64(job_events_path),
            schema: Some(protocol::JOB_EVENT_SCHEMA_NAME.to_string()),
            record_count: file_line_count_u64(job_events_path),
            inline_json: job_events_inline,
        });
    }

    artifacts
}

fn inline_agent_context_json(path: &Path) -> Option<serde_json::Value> {
    let bytes = std::fs::read(path).ok()?;
    let parsed = serde_json::from_slice::<serde_json::Value>(&bytes).ok()?;
    if parsed
        .get("schema")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        == AGENT_CONTEXT_SCHEMA_NAME
    {
        Some(parsed)
    } else {
        None
    }
}

fn inline_run_json(path: &Path) -> Option<serde_json::Value> {
    let bytes = std::fs::read(path).ok()?;
    let parsed = serde_json::from_slice::<serde_json::Value>(&bytes).ok()?;
    if parsed
        .get("schema")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        == JOB_RUN_SCHEMA_NAME
    {
        Some(parsed)
    } else {
        None
    }
}

fn inline_job_events_json_tail(
    path: &Path,
    max_records: usize,
    max_total_bytes: usize,
) -> Option<serde_json::Value> {
    use std::io::BufRead;

    if max_records == 0 || max_total_bytes == 0 {
        return Some(serde_json::Value::Array(Vec::new()));
    }

    let file = std::fs::File::open(path).ok()?;
    let reader = std::io::BufReader::new(file);
    let mut tail: VecDeque<(serde_json::Value, usize)> = VecDeque::with_capacity(max_records);
    let mut total_bytes = 0usize;

    for line in reader.lines() {
        let Ok(line) = line else {
            continue;
        };
        let Ok(event) = serde_json::from_str::<protocol::JobEvent>(&line) else {
            continue;
        };
        let Ok(value) = serde_json::to_value(event) else {
            continue;
        };
        let Ok(serialized) = serde_json::to_vec(&value) else {
            continue;
        };
        let value_bytes = serialized.len();

        if value_bytes > max_total_bytes {
            tail.clear();
            total_bytes = 0;
            continue;
        }

        if tail.len() == max_records {
            if let Some((_, evicted_bytes)) = tail.pop_front() {
                total_bytes = total_bytes.saturating_sub(evicted_bytes);
            }
        }
        tail.push_back((value, value_bytes));
        total_bytes += value_bytes;

        while total_bytes > max_total_bytes {
            let Some((_, evicted_bytes)) = tail.pop_front() else {
                break;
            };
            total_bytes = total_bytes.saturating_sub(evicted_bytes);
        }
    }

    Some(serde_json::Value::Array(
        tail.into_iter().map(|(value, _)| value).collect(),
    ))
}

fn inline_transcript_jsonl_tail(
    path: &Path,
    max_records: usize,
    max_total_bytes: usize,
) -> Option<serde_json::Value> {
    use std::io::BufRead;

    if max_records == 0 || max_total_bytes == 0 {
        return Some(serde_json::Value::Array(Vec::new()));
    }

    let file = std::fs::File::open(path).ok()?;
    let reader = std::io::BufReader::new(file);
    let mut tail: VecDeque<(String, usize)> = VecDeque::with_capacity(max_records);
    let mut total_bytes = 0usize;

    for line in reader.lines() {
        let Ok(line) = line else {
            continue;
        };
        let line_bytes = line.len();

        if line_bytes > max_total_bytes {
            tail.clear();
            total_bytes = 0;
            continue;
        }

        if tail.len() == max_records {
            if let Some((_, evicted_bytes)) = tail.pop_front() {
                total_bytes = total_bytes.saturating_sub(evicted_bytes);
            }
        }
        tail.push_back((line, line_bytes));
        total_bytes += line_bytes;

        while total_bytes > max_total_bytes {
            let Some((_, evicted_bytes)) = tail.pop_front() else {
                break;
            };
            total_bytes = total_bytes.saturating_sub(evicted_bytes);
        }
    }

    Some(serde_json::Value::Array(
        tail.into_iter()
            .map(|(line, _)| serde_json::Value::String(line))
            .collect(),
    ))
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

fn workflow_inputs_contract_from_job_context(job_context: &serde_json::Value) -> serde_json::Value {
    let workflow_inputs = job_context
        .as_object()
        .and_then(|obj| obj.get("workflow"))
        .and_then(serde_json::Value::as_object)
        .and_then(|obj| obj.get("inputs"))
        .cloned();

    let valid_next_events = workflow_inputs
        .as_ref()
        .and_then(serde_json::Value::as_object)
        .and_then(|obj| obj.get("valid_next_events"))
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default();

    let job_artifact_paths = workflow_inputs
        .as_ref()
        .and_then(serde_json::Value::as_object)
        .and_then(|obj| obj.get("job_artifact_paths"))
        .and_then(serde_json::Value::as_object)
        .cloned()
        .map(serde_json::Value::Object)
        .unwrap_or_else(|| clone_context_object_field_or_empty(job_context, "job_artifact_paths"));

    let job_artifact_metadata = workflow_inputs
        .as_ref()
        .and_then(serde_json::Value::as_object)
        .and_then(|obj| obj.get("job_artifact_metadata"))
        .and_then(serde_json::Value::as_object)
        .cloned()
        .map(serde_json::Value::Object)
        .unwrap_or_else(|| {
            clone_context_object_field_or_empty(job_context, "job_artifact_metadata")
        });

    serde_json::json!({
        "valid_next_events": valid_next_events,
        "job_artifact_paths": job_artifact_paths,
        "job_artifact_metadata": job_artifact_metadata,
    })
}

fn workflow_valid_next_events_from_job_context(
    job_context: &serde_json::Value,
) -> Option<Vec<String>> {
    job_context
        .as_object()
        .and_then(|obj| obj.get("workflow"))
        .and_then(serde_json::Value::as_object)
        .and_then(|obj| obj.get("inputs"))
        .and_then(serde_json::Value::as_object)
        .and_then(|obj| obj.get("valid_next_events"))
        .and_then(serde_json::Value::as_array)
        .map(|events| {
            events
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(ToOwned::to_owned)
                .collect::<Vec<_>>()
        })
}

fn work_item_contract_from_job_context(job_context: &serde_json::Value) -> serde_json::Value {
    let work_item = job_context
        .as_object()
        .and_then(|obj| obj.get("work_item"))
        .and_then(serde_json::Value::as_object);

    serde_json::json!({
        "id": work_item
            .and_then(|obj| obj.get("id"))
            .cloned()
            .unwrap_or(serde_json::Value::Null),
        "title": work_item
            .and_then(|obj| obj.get("title"))
            .cloned()
            .unwrap_or(serde_json::Value::Null),
        "description": work_item
            .and_then(|obj| obj.get("description"))
            .cloned()
            .unwrap_or(serde_json::Value::Null),
        "labels": work_item
            .and_then(|obj| obj.get("labels"))
            .and_then(serde_json::Value::as_array)
            .cloned()
            .map(serde_json::Value::Array)
            .unwrap_or_else(|| serde_json::Value::Array(Vec::new())),
        "priority": work_item
            .and_then(|obj| obj.get("priority"))
            .cloned()
            .unwrap_or(serde_json::Value::Null),
        "workflow_state": work_item
            .and_then(|obj| obj.get("workflow_state"))
            .cloned()
            .unwrap_or(serde_json::Value::Null),
        "workflow_id": work_item
            .and_then(|obj| obj.get("workflow_id"))
            .cloned()
            .unwrap_or(serde_json::Value::Null),
        "repo_catalog": work_item
            .and_then(|obj| obj.get("repo_catalog"))
            .and_then(serde_json::Value::as_array)
            .cloned()
            .map(serde_json::Value::Array)
            .unwrap_or_else(|| serde_json::Value::Array(Vec::new())),
    })
}

fn display_paths_for_runtime(
    workspace_dir: &Path,
    transcript_dir: &str,
    runtime_spec: &protocol::CanonicalRuntimeSpec,
) -> (String, String) {
    if matches!(
        runtime_spec.environment.execution_mode,
        ExecutionMode::Docker
    ) {
        ("/workspace".to_string(), "/transcript".to_string())
    } else {
        (
            workspace_dir.to_string_lossy().to_string(),
            transcript_dir.to_string(),
        )
    }
}

fn build_run_json_payload(
    job: &Job,
    runtime_spec: &protocol::CanonicalRuntimeSpec,
    outcome: &ExecutionOutcome,
    termination_reason: &TerminationReason,
    exit_code: Option<i32>,
    workspace_display: &str,
    transcript_dir_display: &str,
) -> serde_json::Value {
    serde_json::json!({
        "schema": JOB_RUN_SCHEMA_NAME,
        "job_id": job.id,
        "work_item_id": job.work_item_id,
        "attempt": job.attempt,
        "mode": &runtime_spec.environment.execution_mode,
        "timeout_secs": runtime_spec.environment.timeout_secs,
        "container_image": &runtime_spec.environment.container_image,
        "outcome": outcome,
        "termination_reason": termination_reason,
        "exit_code": exit_code,
        "workspace": workspace_display,
        "transcript_dir": transcript_dir_display,
        "at": Utc::now(),
    })
}

fn planned_permission_tier_name(tier: &protocol::PlannedPermissionTier) -> &'static str {
    match tier {
        protocol::PlannedPermissionTier::DockerDefault => "docker_default",
        protocol::PlannedPermissionTier::RawCompatible => "raw_compatible",
        protocol::PlannedPermissionTier::KubernetesStub => "kubernetes_stub",
    }
}

fn normalize_github_repo_slug(repo_url: &str) -> Option<String> {
    let trimmed = repo_url.trim();
    let trimmed = trimmed.strip_suffix(".git").unwrap_or(trimmed);

    if let Some(rest) = trimmed.strip_prefix("https://github.com/") {
        let mut parts = rest.split('/').filter(|p| !p.is_empty());
        let owner = parts.next()?;
        let repo = parts.next()?;
        return Some(format!("{owner}/{repo}"));
    }

    if let Some(rest) = trimmed.strip_prefix("git@github.com:") {
        let mut parts = rest.split('/').filter(|p| !p.is_empty());
        let owner = parts.next()?;
        let repo = parts.next()?;
        return Some(format!("{owner}/{repo}"));
    }

    None
}

fn repo_name_from_url(repo_url: &str) -> String {
    let trimmed = repo_url.trim_end_matches('/');
    let candidate = trimmed
        .rsplit_once('/')
        .map(|(_, tail)| tail)
        .or_else(|| trimmed.rsplit_once(':').map(|(_, tail)| tail))
        .unwrap_or(trimmed)
        .trim_end_matches(".git");
    let sanitized = candidate
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.' {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    if sanitized.is_empty() {
        "repo".to_string()
    } else {
        sanitized
    }
}

fn additional_repo_container_path(repo_url: &str) -> String {
    format!("/repos/{}", repo_name_from_url(repo_url))
}

#[cfg(test)]
mod multi_repo_tests {
    use super::*;

    #[test]
    fn repo_name_from_url_handles_https_and_ssh() {
        assert_eq!(
            repo_name_from_url("https://github.com/reddit/runbooks.git"),
            "runbooks"
        );
        assert_eq!(
            repo_name_from_url("git@github.com:reddit/agent-hub.git"),
            "agent-hub"
        );
    }

    #[test]
    fn additional_repo_claims_include_candidates_when_primary_repo_missing() {
        let job = Job {
            id: uuid::Uuid::new_v4(),
            work_item_id: uuid::Uuid::new_v4(),
            step_id: "step".to_string(),
            state: JobState::Assigned,
            assigned_worker_id: None,
            execution_mode: ExecutionMode::Docker,
            container_image: None,
            command: "echo".to_string(),
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
        let runtime_spec = protocol::CanonicalRuntimeSpec {
            runtime_version: "v2".to_string(),
            environment: protocol::PlannedEnvironmentSpec {
                tier: protocol::PlannedEnvironmentTier::Docker,
                execution_mode: ExecutionMode::Docker,
                container_image: None,
                timeout_secs: None,
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
                can_access_network: true,
                auto_approve_tools: vec![],
                escalate_tools: vec![],
                kubernetes_access: None,
                aws_access: None,
            },
            approval_rules: None,
            repos: vec![protocol::PlannedRepoClaim {
                repo_url: "https://github.com/reddit/opencode.git".to_string(),
                base_ref: "origin/main".to_string(),
                read_only: true,
                target_branch: None,
                claim_type: "read_only".to_string(),
                branch_name: None,
            }],
        };

        let claims = additional_repo_claims_for_mounts(&job, &runtime_spec);
        assert_eq!(claims.len(), 1);
        assert_eq!(claims[0].repo_url, "https://github.com/reddit/opencode.git");
    }

    #[test]
    fn additional_repo_claims_exclude_primary_repo_url_regardless_of_position() {
        let primary_url = "https://github.com/reddit/agent-hub.git";
        let candidate_url = "https://github.com/reddit/opencode.git";
        let job = Job {
            id: uuid::Uuid::new_v4(),
            work_item_id: uuid::Uuid::new_v4(),
            step_id: "step".to_string(),
            state: JobState::Assigned,
            assigned_worker_id: None,
            execution_mode: ExecutionMode::Docker,
            container_image: None,
            command: "echo".to_string(),
            args: vec![],
            env: HashMap::new(),
            prompt: None,
            context: serde_json::json!({}),
            repo: Some(protocol::RepoSource {
                repo_url: primary_url.to_string(),
                base_ref: "origin/main".to_string(),
                branch_name: None,
            }),
            timeout_secs: None,
            max_retries: 0,
            attempt: 0,
            transcript_dir: None,
            result: None,
            lease_expires_at: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        let runtime_spec = protocol::CanonicalRuntimeSpec {
            runtime_version: "v2".to_string(),
            environment: protocol::PlannedEnvironmentSpec {
                tier: protocol::PlannedEnvironmentTier::Docker,
                execution_mode: ExecutionMode::Docker,
                container_image: None,
                timeout_secs: None,
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
                can_access_network: true,
                auto_approve_tools: vec![],
                escalate_tools: vec![],
                kubernetes_access: None,
                aws_access: None,
            },
            approval_rules: None,
            repos: vec![
                protocol::PlannedRepoClaim {
                    repo_url: candidate_url.to_string(),
                    base_ref: "origin/main".to_string(),
                    read_only: true,
                    target_branch: None,
                    claim_type: "read_only".to_string(),
                    branch_name: None,
                },
                protocol::PlannedRepoClaim {
                    repo_url: primary_url.to_string(),
                    base_ref: "origin/main".to_string(),
                    read_only: false,
                    target_branch: Some("agent-hub/test".to_string()),
                    claim_type: "workspace".to_string(),
                    branch_name: Some("agent-hub/test".to_string()),
                },
            ],
        };

        let claims = additional_repo_claims_for_mounts(&job, &runtime_spec);
        assert_eq!(claims.len(), 1);
        assert_eq!(claims[0].repo_url, candidate_url);
    }

    #[test]
    fn resolve_runtime_spec_falls_back_and_keeps_repo_claims_from_context() {
        let runtime = protocol::CanonicalRuntimeSpec {
            runtime_version: "v1".to_string(),
            environment: protocol::PlannedEnvironmentSpec {
                tier: protocol::PlannedEnvironmentTier::Docker,
                execution_mode: ExecutionMode::Docker,
                container_image: None,
                timeout_secs: None,
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
                can_access_network: true,
                auto_approve_tools: vec![],
                escalate_tools: vec![],
                kubernetes_access: None,
                aws_access: None,
            },
            approval_rules: None,
            repos: vec![protocol::PlannedRepoClaim {
                repo_url: "https://github.com/nickdavies/agent-hub-poc-test.git".to_string(),
                base_ref: "origin/main".to_string(),
                read_only: true,
                target_branch: None,
                claim_type: "read_only".to_string(),
                branch_name: None,
            }],
        };
        let mut context = serde_json::json!({});
        protocol::project_runtime_spec_into_job_context(&mut context, &runtime);

        let job = Job {
            id: uuid::Uuid::new_v4(),
            work_item_id: uuid::Uuid::new_v4(),
            step_id: "step".to_string(),
            state: JobState::Assigned,
            assigned_worker_id: None,
            execution_mode: ExecutionMode::Raw,
            container_image: None,
            command: "echo".to_string(),
            args: vec![],
            env: HashMap::new(),
            prompt: None,
            context,
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

        let (resolved, source) = resolve_runtime_spec_and_source(&job);
        assert!(matches!(source, RuntimeSpecSource::LegacyJobFields));
        assert_eq!(resolved.repos.len(), 1);
        assert_eq!(
            resolved.repos[0].repo_url,
            "https://github.com/nickdavies/agent-hub-poc-test.git"
        );
    }

    #[test]
    fn build_opencode_spawn_run_args_preserves_repo_mounts() {
        let invocation_args = vec![
            "run".to_string(),
            "--rm".to_string(),
            "-v".to_string(),
            "/host/work:/workspace:rw".to_string(),
            "-v".to_string(),
            "/host/repo:/repos/agent-hub-poc-test:ro".to_string(),
            "--entrypoint".to_string(),
            "sh".to_string(),
            "local/opencode:managed".to_string(),
            "-lc".to_string(),
            "opencode run".to_string(),
        ];

        let run_args =
            build_opencode_spawn_run_args(&invocation_args, "agent-hub-opencode-test").expect("args");

        let repo_mount = "/host/repo:/repos/agent-hub-poc-test:ro".to_string();
        assert!(run_args.iter().any(|arg| arg == &repo_mount));
        assert!(!run_args.iter().any(|arg| arg == "--rm"));
        assert_eq!(run_args[0], "run");
        assert!(run_args.iter().any(|arg| arg == "--name"));
        assert!(run_args.iter().any(|arg| arg == "agent-hub-opencode-test"));
        assert!(run_args.iter().any(|arg| arg == "-d"));
    }
}

#[cfg(test)]
mod simple_code_change_validation_tests {
    use super::*;

    #[test]
    fn triage_success_allows_any_concrete_repo_when_repo_catalog_missing() {
        let work_item_context = serde_json::json!({
            "workflow_id": "simple-code-change",
            "workflow_state": "triage_investigation"
        });
        let output = AgentOutput {
            next_event: Some("job_succeeded".to_string()),
            repo: Some(protocol::RepoSource {
                repo_url: "https://github.com/reddit/agent-hub".to_string(),
                base_ref: "origin/main".to_string(),
                branch_name: None,
            }),
            ..Default::default()
        };

        validate_simple_code_change_repo_rules(&work_item_context, &output)
            .expect("should pass without repo_catalog constraint");
    }

    #[test]
    fn triage_success_requires_repo_to_match_repo_catalog_when_present() {
        let work_item_context = serde_json::json!({
            "workflow_id": "simple-code-change",
            "workflow_state": "triage_investigation",
            "repo_catalog": [
                { "repo_url": "https://github.com/reddit/Dippy", "base_ref": "origin/main" },
                { "repo_url": "https://github.com/reddit/opencode", "base_ref": "origin/main" }
            ]
        });
        let output = AgentOutput {
            next_event: Some("job_succeeded".to_string()),
            repo: Some(protocol::RepoSource {
                repo_url: "https://github.com/reddit/agent-hub".to_string(),
                base_ref: "origin/main".to_string(),
                branch_name: None,
            }),
            ..Default::default()
        };

        let err = validate_simple_code_change_repo_rules(&work_item_context, &output)
            .expect_err("mismatched repo_url should fail");
        assert!(
            err.to_string()
                .contains("must match one of work_item.repo_catalog"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn triage_success_accepts_repo_present_in_repo_catalog() {
        let work_item_context = serde_json::json!({
            "workflow_id": "simple-code-change",
            "workflow_state": "triage_investigation",
            "repo_catalog": [
                { "repo_url": "https://github.com/reddit/Dippy", "base_ref": "origin/main" },
                { "repo_url": "https://github.com/reddit/opencode", "base_ref": "origin/main" }
            ]
        });
        let output = AgentOutput {
            next_event: Some("job_succeeded".to_string()),
            repo: Some(protocol::RepoSource {
                repo_url: "https://github.com/reddit/opencode".to_string(),
                base_ref: "origin/main".to_string(),
                branch_name: None,
            }),
            ..Default::default()
        };

        validate_simple_code_change_repo_rules(&work_item_context, &output)
            .expect("matching repo_url should pass");
    }
}

fn parse_github_pr_from_output(
    output_json: &serde_json::Value,
) -> Option<GithubPullRequestMetadata> {
    let pr = output_json.get("github_pr")?;
    let number = pr.get("number")?.as_u64()?;
    let url = pr.get("url")?.as_str()?.to_string();
    if url.trim().is_empty() {
        return None;
    }

    Some(GithubPullRequestMetadata {
        number,
        url,
        state: pr
            .get("state")
            .and_then(serde_json::Value::as_str)
            .map(ToOwned::to_owned),
        is_draft: pr.get("is_draft").and_then(serde_json::Value::as_bool),
        head_ref_name: pr
            .get("head_ref_name")
            .and_then(serde_json::Value::as_str)
            .map(ToOwned::to_owned),
        base_ref_name: pr
            .get("base_ref_name")
            .and_then(serde_json::Value::as_str)
            .map(ToOwned::to_owned),
    })
}

async fn capture_repo_metadata(
    workspace_dir: &Path,
    repo: Option<&protocol::RepoSource>,
    target_branch: Option<String>,
) -> Option<RepoExecutionMetadata> {
    if !workspace_dir.join(".git").exists() {
        return None;
    }

    let head_sha = git_output(workspace_dir, &["rev-parse", "HEAD"]).await;
    let branch_raw = git_output(workspace_dir, &["rev-parse", "--abbrev-ref", "HEAD"]).await;
    let branch = branch_raw
        .as_deref()
        .map(str::trim)
        .filter(|b| !b.is_empty() && *b != "HEAD")
        .map(ToOwned::to_owned);
    let status_porcelain = git_output(workspace_dir, &["status", "--porcelain"]).await;

    let mut changed_files: u32 = 0;
    let mut untracked_files: u32 = 0;
    let mut is_dirty = false;
    if let Some(status) = &status_porcelain {
        for line in status.lines() {
            if line.trim().is_empty() {
                continue;
            }
            is_dirty = true;
            if line.starts_with("??") {
                untracked_files = untracked_files.saturating_add(1);
            } else {
                changed_files = changed_files.saturating_add(1);
            }
        }
    }

    Some(RepoExecutionMetadata {
        base_ref: repo.map(|r| r.base_ref.clone()),
        target_branch,
        branch,
        head_sha,
        is_dirty,
        changed_files,
        untracked_files,
    })
}

async fn git_output(workspace_dir: &Path, args: &[&str]) -> Option<String> {
    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(workspace_dir)
        .args(args)
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let trimmed = text.trim().to_string();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

async fn ensure_git_workspace(workspace_dir: &Path) -> anyhow::Result<()> {
    let git_dir = workspace_dir.join(".git");
    if git_dir.exists() {
        return Ok(());
    }
    let status = tokio::process::Command::new("git")
        .arg("init")
        .arg(workspace_dir)
        .status()
        .await?;
    if !status.success() {
        anyhow::bail!("git init failed");
    }
    Ok(())
}

const JOB_HEARTBEAT_SECS: u64 = 20;
const MIRROR_REFRESH_MIN_SECS: u64 = 30;
const TRANSCRIPT_CLEANUP_INTERVAL_SECS: u64 = 60 * 60;
const TRANSCRIPT_JSONL_INLINE_TAIL_LIMIT: usize = 20;
const TRANSCRIPT_JSONL_INLINE_MAX_BYTES: usize = 64 * 1024;
const JOB_EVENTS_INLINE_TAIL_LIMIT: usize = 20;
const JOB_EVENTS_INLINE_MAX_BYTES: usize = 64 * 1024;
const DEFAULT_DOCKER_MEMORY_LIMIT: &str = "8g";
const DEFAULT_DOCKER_CPU_LIMIT: &str = "4";
