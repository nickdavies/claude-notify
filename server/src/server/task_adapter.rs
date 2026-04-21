use std::collections::HashMap;

use chrono::{DateTime, Utc};
use protocol::{
    CreateWorkItemRequest, FireEventRequest, RepoSource, WorkItem, WorkItemState, WorkflowEventKind,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskTriageStatus {
    NeedsWorkflowSelection,
    Ready,
    InProgress,
    Blocked,
    Completed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HumanActionKind {
    ChooseWorkflow,
    Submit,
    Approve,
    RequestChanges,
    Unblock,
    Retry,
    Cancel,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskHumanAction {
    pub kind: HumanActionKind,
    #[serde(default)]
    pub workflow_id: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskToolRecord {
    pub id: Uuid,
    pub title: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub workflow_id: Option<String>,
    pub triage_status: TaskTriageStatus,
    #[serde(default)]
    pub pending_human_action: bool,
    #[serde(default)]
    pub current_human_gate: Option<String>,
    #[serde(default)]
    pub workflow_phase: Option<String>,
    #[serde(default)]
    pub blocked_reason: Option<String>,
    #[serde(default)]
    pub human_gate_state: Option<TaskHumanGateState>,
    #[serde(default)]
    pub valid_next_events: Vec<WorkflowEventKind>,
    #[serde(default)]
    pub allowed_actions: Vec<HumanActionKind>,
    #[serde(default)]
    pub context: serde_json::Value,
    #[serde(default)]
    pub repo: Option<RepoSource>,
    pub updated_at: DateTime<Utc>,
    #[serde(default)]
    pub requested_action: Option<TaskHumanAction>,
    #[serde(default)]
    pub action_revision: u64,
}

#[derive(Debug, Clone)]
pub struct TaskSyncRecord {
    pub external: TaskToolRecord,
}

#[derive(Debug, Clone)]
pub enum AdapterIntent {
    Create {
        task: TaskToolRecord,
    },
    UpdateProjection {
        work_item_id: Uuid,
        task: TaskToolRecord,
    },
    ApplyAction {
        work_item_id: Uuid,
        action: TaskHumanAction,
        action_revision: u64,
    },
}

#[derive(Debug, Clone)]
pub struct TaskProjection {
    pub task: TaskToolRecord,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskChangeType {
    Created,
    Updated,
    Deleted,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskChange {
    pub task_id: String,
    pub change_type: TaskChangeType,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskUpdate {
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub labels: Option<Vec<String>>,
    #[serde(default)]
    pub assignee: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskCreate {
    pub title: String,
    pub description: String,
    pub labels: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskHumanGateState {
    AwaitingHumanAction,
    NeedsWorkflowSelection,
    WorkflowSelected,
    Approved,
    ChangesRequested,
    Unblocked,
    RetryRequested,
}

pub trait TaskAdapter: Send + Sync {
    fn adapter_name(&self) -> &'static str;
    fn pull_tasks(&self) -> anyhow::Result<Vec<TaskSyncRecord>>;
    fn get_task(&self, id: Uuid) -> anyhow::Result<Option<TaskToolRecord>>;
    fn persist_projection(&self, projection: &TaskProjection) -> anyhow::Result<()>;

    fn poll_changes(&self) -> anyhow::Result<Vec<TaskChange>> {
        Err(anyhow::anyhow!("not implemented"))
    }

    fn update_task(&self, _task_id: &str, _update: TaskUpdate) -> anyhow::Result<()> {
        Err(anyhow::anyhow!("not implemented"))
    }

    fn create_task(&self, _create: TaskCreate) -> anyhow::Result<String> {
        Err(anyhow::anyhow!("not implemented"))
    }
}

const NEEDS_TRIAGE_LABEL: &str = "needs-triage";

fn context_has_label(context: &serde_json::Value, label: &str) -> bool {
    context
        .get("labels")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|labels| {
            labels
                .iter()
                .filter_map(serde_json::Value::as_str)
                .any(|value| value == label)
        })
}

fn context_needs_triage_flag(context: &serde_json::Value) -> bool {
    context
        .get("needs_triage")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

pub(crate) fn task_requires_manual_triage(task: &TaskToolRecord) -> bool {
    context_has_label(&task.context, NEEDS_TRIAGE_LABEL) || context_needs_triage_flag(&task.context)
}

pub(crate) fn work_item_requires_manual_triage(item: &WorkItem) -> bool {
    item.labels.iter().any(|label| label == NEEDS_TRIAGE_LABEL)
        || context_has_label(&item.context, NEEDS_TRIAGE_LABEL)
        || context_needs_triage_flag(&item.context)
}

pub fn work_item_from_create(
    req: &CreateWorkItemRequest,
    id: Uuid,
    workflow_id: String,
    current_step: String,
) -> WorkItem {
    let now = Utc::now();
    let title = req
        .context
        .get("title")
        .and_then(serde_json::Value::as_str)
        .map(ToOwned::to_owned);
    let description = req
        .context
        .get("description")
        .and_then(serde_json::Value::as_str)
        .map(ToOwned::to_owned);
    WorkItem {
        id,
        workflow_id,
        external_task_id: Some(id.to_string()),
        external_task_source: Some("file".to_string()),
        title,
        description,
        labels: vec![],
        priority: None,
        session_id: req.session_id.clone(),
        current_step,
        projected_state: None,
        triage_status: None,
        human_gate: false,
        human_gate_state: None,
        blocked_on: None,
        state: WorkItemState::Pending,
        context: req.context.clone(),
        repo: req.repo.clone(),
        parent_work_item_id: None,
        parent_step_id: None,
        child_work_item_id: None,
        created_at: now,
        updated_at: now,
    }
}

pub fn choose_workflow_id(task: &TaskToolRecord, workflow_ids: &[String]) -> Option<String> {
    if let Some(explicit) = &task.workflow_id
        && workflow_ids.iter().any(|id| id == explicit)
    {
        return Some(explicit.clone());
    }

    task.context
        .get("workflow_id")
        .and_then(serde_json::Value::as_str)
        .filter(|wf| workflow_ids.iter().any(|id| id == wf))
        .map(ToOwned::to_owned)
}

pub fn task_from_work_item(item: &WorkItem, existing: Option<&TaskToolRecord>) -> TaskToolRecord {
    let inferred = infer_valid_next_events_for_projection(item);
    task_from_work_item_with_valid_next_events(item, existing, &inferred)
}

pub fn task_from_work_item_with_valid_next_events(
    item: &WorkItem,
    existing: Option<&TaskToolRecord>,
    valid_next_events: &[WorkflowEventKind],
) -> TaskToolRecord {
    let title = existing
        .map(|t| t.title.clone())
        .unwrap_or_else(|| format!("Task {}", item.id));
    let description = existing.and_then(|t| t.description.clone()).or_else(|| {
        item.context
            .get("description")
            .and_then(serde_json::Value::as_str)
            .map(ToOwned::to_owned)
    });
    let action_revision = existing.map(|t| t.action_revision).unwrap_or(0);

    let is_triage = item.workflow_id == "__triage__";
    let title = item.title.clone().unwrap_or(title);
    let description = item.description.clone().or(description);
    let gate_state = current_human_gate_state(item);
    TaskToolRecord {
        id: item.id,
        title,
        description,
        workflow_id: if is_triage {
            existing.and_then(|t| t.workflow_id.clone())
        } else {
            Some(item.workflow_id.clone())
        },
        triage_status: if is_triage {
            TaskTriageStatus::NeedsWorkflowSelection
        } else {
            triage_status_from_item(item)
        },
        pending_human_action: is_triage || is_human_gate(item),
        current_human_gate: if is_triage {
            Some("workflow_selection".to_string())
        } else {
            human_gate_label(item)
        },
        workflow_phase: Some(project_workflow_phase(item)),
        blocked_reason: project_blocked_reason(item),
        human_gate_state: gate_state,
        valid_next_events: valid_next_events.to_vec(),
        allowed_actions: compute_allowed_actions(item, valid_next_events),
        context: item.context.clone(),
        repo: item.repo.clone(),
        updated_at: existing.map(|t| t.updated_at).unwrap_or(item.updated_at),
        requested_action: None,
        action_revision,
    }
}

fn current_human_gate_state(item: &WorkItem) -> Option<TaskHumanGateState> {
    item.human_gate_state
        .as_deref()
        .and_then(parse_human_gate_state)
        .or_else(|| {
            item.context
                .get("task_human_gate_state")
                .cloned()
                .and_then(|v| serde_json::from_value(v).ok())
        })
}

fn parse_human_gate_state(v: &str) -> Option<TaskHumanGateState> {
    serde_json::from_value(serde_json::Value::String(v.to_string())).ok()
}

fn project_workflow_phase(item: &WorkItem) -> String {
    item.projected_state
        .clone()
        .unwrap_or_else(|| item.current_step.clone())
}

fn project_blocked_reason(item: &WorkItem) -> Option<String> {
    if item.workflow_id == "__triage__" {
        if let Some(reason) = item
            .context
            .get("triage_reason")
            .and_then(serde_json::Value::as_str)
        {
            return Some(reason.to_string());
        }
        return Some("workflow selection required".to_string());
    }
    if item.human_gate {
        if let Some(state) = &item.human_gate_state {
            return Some(format!("human gate: {}", state));
        }
        return Some("human gate awaiting action".to_string());
    }
    if item.state == WorkItemState::Failed {
        return Some("workflow step failed".to_string());
    }
    None
}

fn compute_allowed_actions(
    item: &WorkItem,
    valid_next_events: &[WorkflowEventKind],
) -> Vec<HumanActionKind> {
    if matches!(
        item.state,
        WorkItemState::Succeeded | WorkItemState::Cancelled
    ) {
        return vec![];
    }

    let allows = |event: WorkflowEventKind| valid_next_events.contains(&event);
    let requires_manual_triage = work_item_requires_manual_triage(item);

    if item.workflow_id == "__triage__" {
        if requires_manual_triage {
            return if allows(WorkflowEventKind::Cancel) {
                vec![HumanActionKind::Cancel]
            } else {
                vec![]
            };
        }

        let mut actions = Vec::new();
        if item
            .human_gate_state
            .as_deref()
            .is_some_and(|s| s == "workflow_selected")
        {
            if allows(WorkflowEventKind::Submit) {
                actions.push(HumanActionKind::Submit);
            }
        }

        if item.current_step.as_str().eq("triage") && !requires_manual_triage {
            actions.push(HumanActionKind::ChooseWorkflow);
        }

        if allows(WorkflowEventKind::Cancel) {
            actions.push(HumanActionKind::Cancel);
        }
        return actions;
    }

    if item.human_gate {
        let mut actions = Vec::new();
        if allows(WorkflowEventKind::HumanApproved) {
            actions.push(HumanActionKind::Approve);
        }
        if allows(WorkflowEventKind::HumanChangesRequested) {
            actions.push(HumanActionKind::RequestChanges);
        }
        if allows(WorkflowEventKind::HumanUnblocked) {
            actions.push(HumanActionKind::Unblock);
        }
        if allows(WorkflowEventKind::RetryRequested) {
            actions.push(HumanActionKind::Retry);
        }
        if allows(WorkflowEventKind::Cancel) {
            actions.push(HumanActionKind::Cancel);
        }
        return actions;
    }

    match item.state {
        // Submit is currently a compatibility action and not shown by default.
        WorkItemState::Pending | WorkItemState::Running => {
            if allows(WorkflowEventKind::Cancel) {
                vec![HumanActionKind::Cancel]
            } else {
                vec![]
            }
        }
        WorkItemState::Failed => {
            let mut actions = Vec::new();
            if allows(WorkflowEventKind::RetryRequested) {
                actions.push(HumanActionKind::Retry);
            }
            if allows(WorkflowEventKind::Cancel) {
                actions.push(HumanActionKind::Cancel);
            }
            actions
        }
        WorkItemState::Succeeded | WorkItemState::Cancelled => vec![],
    }
}

fn infer_valid_next_events_for_projection(item: &WorkItem) -> Vec<WorkflowEventKind> {
    // Compatibility fallback for contexts/tests that do not have orchestration-provided
    // valid_next_events yet. Keep this broadly aligned with the authoritative server helper,
    // but prefer orchestration::valid_next_events_for_work_item as the source of truth.
    if matches!(
        item.state,
        WorkItemState::Succeeded | WorkItemState::Cancelled
    ) {
        return vec![];
    }

    if item.workflow_id == "__triage__" {
        if work_item_requires_manual_triage(item) {
            return vec![WorkflowEventKind::Cancel];
        }
        let mut events = Vec::new();
        if item
            .human_gate_state
            .as_deref()
            .is_some_and(|s| s == "workflow_selected")
        {
            events.push(WorkflowEventKind::Submit);
        }
        events.push(WorkflowEventKind::Cancel);
        return events;
    }

    if item.human_gate {
        return legacy_human_gate_valid_next_events(item);
    }

    let mut events = Vec::new();
    if item.state == WorkItemState::Failed {
        events.push(WorkflowEventKind::RetryRequested);
    }
    events.push(WorkflowEventKind::Cancel);
    events
}

pub(crate) fn legacy_human_gate_valid_next_events(item: &WorkItem) -> Vec<WorkflowEventKind> {
    let mut events = Vec::new();
    match current_human_gate_state(item) {
        Some(TaskHumanGateState::Approved) => {}
        Some(TaskHumanGateState::ChangesRequested) => {
            events.extend([
                WorkflowEventKind::HumanUnblocked,
                WorkflowEventKind::RetryRequested,
            ]);
        }
        Some(TaskHumanGateState::Unblocked) => {
            events.extend([
                WorkflowEventKind::HumanApproved,
                WorkflowEventKind::HumanChangesRequested,
            ]);
        }
        Some(TaskHumanGateState::RetryRequested) => {
            events.push(WorkflowEventKind::HumanUnblocked);
        }
        _ => {
            events.extend([
                WorkflowEventKind::HumanApproved,
                WorkflowEventKind::HumanChangesRequested,
                WorkflowEventKind::HumanUnblocked,
                WorkflowEventKind::RetryRequested,
            ]);
        }
    }
    events.push(WorkflowEventKind::Cancel);
    events
}

pub fn action_to_fire_event(action: &TaskHumanAction) -> Option<FireEventRequest> {
    let event = match action.kind {
        HumanActionKind::Submit => WorkflowEventKind::Submit,
        HumanActionKind::Approve => WorkflowEventKind::HumanApproved,
        HumanActionKind::RequestChanges => WorkflowEventKind::HumanChangesRequested,
        HumanActionKind::Unblock => WorkflowEventKind::HumanUnblocked,
        HumanActionKind::Retry => WorkflowEventKind::RetryRequested,
        HumanActionKind::Cancel => WorkflowEventKind::Cancel,
        HumanActionKind::ChooseWorkflow => return None,
    };
    Some(FireEventRequest {
        event,
        job_id: None,
        reason: action.reason.clone(),
    })
}

pub fn infer_intents(
    records: Vec<TaskSyncRecord>,
    existing: &HashMap<Uuid, WorkItem>,
    workflow_ids: &[String],
) -> Vec<AdapterIntent> {
    let mut intents = Vec::new();
    for record in records {
        let task = record.external;
        if let Some(item) = existing.get(&task.id) {
            if let Some(action) = &task.requested_action {
                // requested_action is a one-shot intent; dedupe is enforced by action_revision
                // against context.task_adapter.last_action_revision on the orchestration side.
                intents.push(AdapterIntent::ApplyAction {
                    work_item_id: item.id,
                    action: action.clone(),
                    action_revision: task.action_revision,
                });
            } else {
                if task.updated_at <= item.updated_at {
                    continue;
                }
                intents.push(AdapterIntent::UpdateProjection {
                    work_item_id: item.id,
                    task,
                });
            }
            continue;
        }

        let mut task = task;
        if task_requires_manual_triage(&task) || choose_workflow_id(&task, workflow_ids).is_none() {
            task.triage_status = TaskTriageStatus::NeedsWorkflowSelection;
            task.pending_human_action = true;
            task.current_human_gate = Some("workflow_selection".to_string());
            intents.push(AdapterIntent::Create { task });
            continue;
        }
        intents.push(AdapterIntent::Create { task });
    }
    intents
}

fn triage_status_from_item(item: &WorkItem) -> TaskTriageStatus {
    if item.workflow_id == "__triage__" {
        return TaskTriageStatus::NeedsWorkflowSelection;
    }
    match item.state {
        WorkItemState::Pending => TaskTriageStatus::Ready,
        WorkItemState::Running => {
            if is_human_gate(item) {
                TaskTriageStatus::Blocked
            } else {
                TaskTriageStatus::InProgress
            }
        }
        WorkItemState::Succeeded => TaskTriageStatus::Completed,
        WorkItemState::Failed => TaskTriageStatus::Blocked,
        WorkItemState::Cancelled => TaskTriageStatus::Cancelled,
    }
}

fn is_human_gate(item: &WorkItem) -> bool {
    item.human_gate
        || item
            .context
            .get("human_gate")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
}

fn human_gate_label(item: &WorkItem) -> Option<String> {
    if is_human_gate(item) {
        Some(item.current_step.clone())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn choose_workflow_prefers_explicit() {
        let task = TaskToolRecord {
            id: Uuid::new_v4(),
            title: "t".into(),
            description: None,
            workflow_id: Some("wf1".into()),
            triage_status: TaskTriageStatus::NeedsWorkflowSelection,
            pending_human_action: true,
            current_human_gate: None,
            workflow_phase: None,
            blocked_reason: None,
            human_gate_state: None,
            valid_next_events: vec![],
            allowed_actions: vec![],
            context: serde_json::json!({"workflow_id": "wf2"}),
            repo: None,
            updated_at: Utc::now(),
            requested_action: None,
            action_revision: 0,
        };
        assert_eq!(
            choose_workflow_id(&task, &["wf1".into(), "wf2".into()]),
            Some("wf1".into())
        );
    }

    #[test]
    fn infer_intents_marks_missing_workflow_for_triage() {
        let task = TaskToolRecord {
            id: Uuid::new_v4(),
            title: "t".into(),
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
            context: serde_json::json!({}),
            repo: None,
            updated_at: Utc::now(),
            requested_action: None,
            action_revision: 0,
        };

        let intents = infer_intents(
            vec![TaskSyncRecord {
                external: task.clone(),
            }],
            &HashMap::new(),
            &[],
        );
        assert!(matches!(intents[0], AdapterIntent::Create { .. }));
    }

    #[test]
    fn task_projection_prefers_work_item_human_gate_field() {
        let mut item = work_item_from_create(
            &CreateWorkItemRequest {
                workflow_id: "wf".into(),
                session_id: None,
                context: serde_json::json!({"human_gate": false}),
                repo: None,
            },
            Uuid::new_v4(),
            "wf".into(),
            "review".into(),
        );
        item.human_gate = true;
        let task = task_from_work_item(&item, None);
        assert!(task.pending_human_action);
    }

    #[test]
    fn allowed_actions_for_triage_and_human_gate() {
        let mut triage = work_item_from_create(
            &CreateWorkItemRequest {
                workflow_id: "__triage__".into(),
                session_id: None,
                context: serde_json::json!({}),
                repo: None,
            },
            Uuid::new_v4(),
            "__triage__".into(),
            "triage".into(),
        );
        triage.workflow_id = "__triage__".into();
        let triage_task = task_from_work_item(&triage, None);
        assert!(
            triage_task
                .allowed_actions
                .contains(&HumanActionKind::ChooseWorkflow)
        );

        let mut human = triage.clone();
        human.workflow_id = "wf".into();
        human.human_gate = true;
        let human_task = task_from_work_item(&human, None);
        assert!(
            human_task
                .allowed_actions
                .contains(&HumanActionKind::Approve)
        );
        assert!(
            human_task
                .allowed_actions
                .contains(&HumanActionKind::RequestChanges)
        );

        human.human_gate_state = Some("awaiting_human_action".into());
        let awaiting = task_from_work_item(&human, None);
        assert_eq!(
            awaiting.human_gate_state,
            Some(TaskHumanGateState::AwaitingHumanAction)
        );
        assert!(awaiting.allowed_actions.contains(&HumanActionKind::Approve));

        human.human_gate_state = Some("changes_requested".into());
        let narrowed = task_from_work_item(&human, None);
        assert!(!narrowed.allowed_actions.contains(&HumanActionKind::Approve));
        assert!(narrowed.allowed_actions.contains(&HumanActionKind::Unblock));
        assert!(narrowed.allowed_actions.contains(&HumanActionKind::Retry));
    }

    #[test]
    fn pending_non_triage_does_not_offer_submit_by_default() {
        let item = work_item_from_create(
            &CreateWorkItemRequest {
                workflow_id: "wf".into(),
                session_id: None,
                context: serde_json::json!({}),
                repo: None,
            },
            Uuid::new_v4(),
            "wf".into(),
            "build".into(),
        );
        let task = task_from_work_item(&item, None);
        assert!(!task.allowed_actions.contains(&HumanActionKind::Submit));
    }

    #[test]
    fn triage_needs_triage_flag_hides_choose_workflow_until_manual_unblock() {
        let mut item = work_item_from_create(
            &CreateWorkItemRequest {
                workflow_id: "__triage__".into(),
                session_id: None,
                context: serde_json::json!({"needs_triage": true, "triage_reason": "manual triage required"}),
                repo: None,
            },
            Uuid::new_v4(),
            "__triage__".into(),
            "triage".into(),
        );
        item.workflow_id = "__triage__".into();
        let task = task_from_work_item(&item, None);
        assert!(
            !task
                .allowed_actions
                .contains(&HumanActionKind::ChooseWorkflow)
        );
        assert_eq!(
            task.blocked_reason.as_deref(),
            Some("manual triage required")
        );
    }

    #[test]
    fn triage_needs_triage_label_hides_choose_workflow_until_label_removed() {
        let mut item = work_item_from_create(
            &CreateWorkItemRequest {
                workflow_id: "__triage__".into(),
                session_id: None,
                context: serde_json::json!({"labels": ["needs-triage"], "triage_reason": "manual triage required"}),
                repo: None,
            },
            Uuid::new_v4(),
            "__triage__".into(),
            "triage".into(),
        );
        item.workflow_id = "__triage__".into();
        item.labels = vec!["needs-triage".to_string()];

        let task = task_from_work_item(&item, None);
        assert!(
            !task
                .allowed_actions
                .contains(&HumanActionKind::ChooseWorkflow)
        );

        item.labels.clear();
        item.context = serde_json::json!({"labels": []});
        let unblocked = task_from_work_item(&item, None);
        assert!(
            unblocked
                .allowed_actions
                .contains(&HumanActionKind::ChooseWorkflow)
        );
    }

    #[test]
    fn infer_intents_respects_needs_triage_label() {
        let task = TaskToolRecord {
            id: Uuid::new_v4(),
            title: "t".into(),
            description: None,
            workflow_id: Some("wf".into()),
            triage_status: TaskTriageStatus::Ready,
            pending_human_action: false,
            current_human_gate: None,
            workflow_phase: None,
            blocked_reason: None,
            human_gate_state: None,
            valid_next_events: vec![],
            allowed_actions: vec![],
            context: serde_json::json!({"labels": ["needs-triage"]}),
            repo: None,
            updated_at: Utc::now(),
            requested_action: None,
            action_revision: 0,
        };

        let intents = infer_intents(
            vec![TaskSyncRecord { external: task }],
            &HashMap::new(),
            &["wf".into()],
        );
        assert!(matches!(intents[0], AdapterIntent::Create { .. }));
    }

    #[test]
    fn allowed_actions_are_subset_of_valid_next_events() {
        let item = work_item_from_create(
            &CreateWorkItemRequest {
                workflow_id: "wf".into(),
                session_id: None,
                context: serde_json::json!({}),
                repo: None,
            },
            Uuid::new_v4(),
            "wf".into(),
            "review".into(),
        );

        let task = task_from_work_item_with_valid_next_events(
            &item,
            None,
            &[WorkflowEventKind::Cancel, WorkflowEventKind::RetryRequested],
        );

        for action in &task.allowed_actions {
            let mapped = action_to_fire_event(&TaskHumanAction {
                kind: action.clone(),
                workflow_id: None,
                reason: None,
            })
            .map(|req| req.event);
            if let Some(event) = mapped {
                assert!(task.valid_next_events.contains(&event));
            }
        }
    }

    #[test]
    fn fallback_valid_next_events_for_human_gate_are_state_aware() {
        let mut item = work_item_from_create(
            &CreateWorkItemRequest {
                workflow_id: "wf".into(),
                session_id: None,
                context: serde_json::json!({}),
                repo: None,
            },
            Uuid::new_v4(),
            "wf".into(),
            "review".into(),
        );
        item.human_gate = true;
        item.human_gate_state = Some("changes_requested".into());

        let task = task_from_work_item(&item, None);
        assert!(
            task.valid_next_events
                .contains(&WorkflowEventKind::HumanUnblocked)
        );
        assert!(
            task.valid_next_events
                .contains(&WorkflowEventKind::RetryRequested)
        );
        assert!(task.valid_next_events.contains(&WorkflowEventKind::Cancel));
        assert!(
            !task
                .valid_next_events
                .contains(&WorkflowEventKind::HumanApproved)
        );
        assert!(
            !task
                .valid_next_events
                .contains(&WorkflowEventKind::HumanChangesRequested)
        );
    }

    #[test]
    fn work_item_from_create_lifts_title_and_description_from_context() {
        let item = work_item_from_create(
            &CreateWorkItemRequest {
                workflow_id: "wf".into(),
                session_id: None,
                context: serde_json::json!({
                    "title": "Context title",
                    "description": "Context description"
                }),
                repo: None,
            },
            Uuid::new_v4(),
            "wf".into(),
            "build".into(),
        );

        assert_eq!(item.title.as_deref(), Some("Context title"));
        assert_eq!(item.description.as_deref(), Some("Context description"));
    }
}
