use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use protocol::{
    ReactiveWorkflowDefinition, WorkflowDefinition, WorkflowDocument, WorkflowValidationError,
    WorkflowValidationReport, parse_workflow_toml_document, validate_reactive_workflows,
    validate_workflow_document, validate_workflows,
};

pub struct WorkflowCatalog {
    definitions: HashMap<String, WorkflowDefinition>,
    reactive_definitions: HashMap<String, protocol::ReactiveWorkflowDefinition>,
    validation_errors: Vec<WorkflowValidationError>,
}

impl WorkflowCatalog {
    pub fn empty() -> Self {
        Self {
            definitions: HashMap::new(),
            reactive_definitions: HashMap::new(),
            validation_errors: Vec::new(),
        }
    }

    pub fn load_from_dir(dir: &str) -> anyhow::Result<Self> {
        let path = PathBuf::from(dir);
        if !path.exists() {
            return Ok(Self {
                definitions: HashMap::new(),
                reactive_definitions: HashMap::new(),
                validation_errors: vec![],
            });
        }

        let mut definitions = HashMap::new();
        let mut reactive_definitions = HashMap::new();
        let mut validation_errors = Vec::new();

        for entry in std::fs::read_dir(&path)? {
            let entry = entry?;
            let p = entry.path();
            if !is_toml(&p) {
                continue;
            }

            let content = std::fs::read_to_string(&p)?;
            match parse_workflow_toml_document(&content) {
                Ok(WorkflowDocument::LegacyV1 { workflow: def }) => {
                    let report = validate_workflow_document(&WorkflowDocument::LegacyV1 {
                        workflow: def.clone(),
                    });
                    if report.is_valid() {
                        if definitions.contains_key(&def.id) {
                            validation_errors.push(WorkflowValidationError {
                                workflow_id: def.id.clone(),
                                message: format!("duplicate workflow id '{}'", def.id),
                            });
                        } else {
                            definitions.insert(def.id.clone(), def);
                        }
                    } else {
                        validation_errors.extend(report.errors);
                    }
                }
                Ok(WorkflowDocument::ReactiveV2 { workflow: def }) => {
                    let report = validate_workflow_document(&WorkflowDocument::ReactiveV2 {
                        workflow: def.clone(),
                    });
                    if report.is_valid() {
                        if reactive_definitions.contains_key(&def.id) {
                            validation_errors.push(WorkflowValidationError {
                                workflow_id: def.id.clone(),
                                message: format!("duplicate reactive workflow id '{}'", def.id),
                            });
                        } else {
                            reactive_definitions.insert(def.id.clone(), def);
                        }
                    } else {
                        validation_errors.extend(report.errors);
                    }
                }
                Err(e) => {
                    validation_errors.push(WorkflowValidationError {
                        workflow_id: p
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_else(|| "unknown".to_string()),
                        message: format!("failed to parse TOML: {e}"),
                    });
                }
            }
        }

        loop {
            let defs_vec = definitions.values().cloned().collect::<Vec<_>>();
            let cross_report = validate_workflows(&defs_vec);
            if cross_report.is_valid() {
                break;
            }

            let invalid_ids = cross_report
                .errors
                .iter()
                .map(|err| err.workflow_id.clone())
                .filter(|id| definitions.contains_key(id))
                .collect::<HashSet<_>>();
            validation_errors.extend(cross_report.errors);

            if invalid_ids.is_empty() {
                break;
            }

            for id in invalid_ids {
                definitions.remove(&id);
            }
        }

        let reactive_vec = reactive_definitions.values().cloned().collect::<Vec<_>>();
        let legacy_vec = definitions.values().cloned().collect::<Vec<_>>();
        let reactive_report = validate_reactive_workflows(&reactive_vec, &legacy_vec);
        let invalid_reactive_ids = reactive_report
            .errors
            .iter()
            .map(|err| err.workflow_id.clone())
            .filter(|id| reactive_definitions.contains_key(id))
            .collect::<HashSet<_>>();
        validation_errors.extend(reactive_report.errors);
        for id in invalid_reactive_ids {
            reactive_definitions.remove(&id);
        }

        Ok(Self {
            definitions,
            reactive_definitions,
            validation_errors,
        })
    }

    pub fn get(&self, id: &str) -> Option<&WorkflowDefinition> {
        self.definitions.get(id)
    }

    pub fn get_reactive(&self, id: &str) -> Option<&ReactiveWorkflowDefinition> {
        self.reactive_definitions.get(id)
    }

    pub fn list(&self) -> Vec<WorkflowDefinition> {
        let mut defs = self.definitions.values().cloned().collect::<Vec<_>>();
        defs.sort_by(|a, b| a.id.cmp(&b.id));
        defs
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn list_reactive(&self) -> Vec<ReactiveWorkflowDefinition> {
        let mut defs = self
            .reactive_definitions
            .values()
            .cloned()
            .collect::<Vec<_>>();
        defs.sort_by(|a, b| a.id.cmp(&b.id));
        defs
    }

    pub fn validation_report(&self) -> WorkflowValidationReport {
        WorkflowValidationReport {
            errors: self.validation_errors.clone(),
        }
    }
}

fn is_toml(path: &Path) -> bool {
    path.extension()
        .and_then(|v| v.to_str())
        .map(|ext| ext.eq_ignore_ascii_case("toml"))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_rejects_cross_workflow_child_cycle() {
        let dir = std::env::temp_dir().join(format!("wf-catalog-cycle-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let a = r#"
[workflow]
id = "a"
entry_step = "s"

[[workflow.steps]]
id = "s"
kind = "child_workflow"
executor = "raw"
child_workflow_id = "b"
command = ""
"#;
        let b = r#"
[workflow]
id = "b"
entry_step = "s"

[[workflow.steps]]
id = "s"
kind = "child_workflow"
executor = "raw"
child_workflow_id = "a"
command = ""
"#;
        std::fs::write(dir.join("a.toml"), a).expect("write a");
        std::fs::write(dir.join("b.toml"), b).expect("write b");

        let catalog = WorkflowCatalog::load_from_dir(dir.to_str().unwrap()).expect("load");
        assert!(catalog.list().is_empty());
        assert!(!catalog.validation_report().is_valid());

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn load_cascades_cross_validation_after_removal() {
        let dir = std::env::temp_dir().join(format!("wf-catalog-cascade-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let a = r#"
[workflow]
id = "a"
entry_step = "s"

[[workflow.steps]]
id = "s"
kind = "child_workflow"
executor = "raw"
child_workflow_id = "b"
command = ""
"#;
        let b = r#"
[workflow]
id = "b"
entry_step = "s"

[[workflow.steps]]
id = "s"
kind = "child_workflow"
executor = "raw"
child_workflow_id = "missing"
command = ""
"#;
        std::fs::write(dir.join("a.toml"), a).expect("write a");
        std::fs::write(dir.join("b.toml"), b).expect("write b");

        let catalog = WorkflowCatalog::load_from_dir(dir.to_str().unwrap()).expect("load");
        assert!(catalog.list().is_empty());
        assert!(!catalog.validation_report().is_valid());

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn load_accepts_reactive_v2_workflow() {
        let dir =
            std::env::temp_dir().join(format!("wf-catalog-reactive-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let reactive = r#"
[workflow]
id = "reactive_demo"
schema = "reactive_v2"
initial_state = "draft"

[[workflow.states]]
id = "draft"
kind = "human"

[[workflow.states.on]]
event = "submit"
target = "running"

[[workflow.states]]
id = "running"
kind = "auto"

[[workflow.states.on]]
event = "start"
target = "done"

[[workflow.states]]
id = "done"
kind = "terminal"
"#;
        std::fs::write(dir.join("reactive.toml"), reactive).expect("write reactive");

        let catalog = WorkflowCatalog::load_from_dir(dir.to_str().unwrap()).expect("load");
        assert!(
            catalog
                .list_reactive()
                .iter()
                .any(|w| w.id == "reactive_demo")
        );
        assert!(catalog.validation_report().is_valid());

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn load_rejects_dot_in_legacy_step_id() {
        let dir =
            std::env::temp_dir().join(format!("wf-catalog-dot-step-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let wf = r#"
[workflow]
id = "wf_dot_step"
entry_step = "build.v1"

[[workflow.steps]]
id = "build.v1"
kind = "agent"
executor = "raw"
command = "echo"
args = ["ok"]
"#;
        std::fs::write(dir.join("wf.toml"), wf).expect("write");

        let catalog = WorkflowCatalog::load_from_dir(dir.to_str().unwrap()).expect("load");
        assert!(catalog.list().is_empty());
        assert!(!catalog.validation_report().is_valid());

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn load_rejects_dot_in_reactive_state_id() {
        let dir = std::env::temp_dir().join(format!(
            "wf-catalog-dot-reactive-state-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let wf = r#"
[workflow]
id = "reactive_dot_state"
schema = "reactive_v2"
initial_state = "draft.v1"

[[workflow.states]]
id = "draft.v1"
kind = "human"

[[workflow.states.on]]
event = "submit"
target = "done"

[[workflow.states]]
id = "done"
kind = "terminal"
"#;
        std::fs::write(dir.join("reactive.toml"), wf).expect("write");

        let catalog = WorkflowCatalog::load_from_dir(dir.to_str().unwrap()).expect("load");
        assert!(catalog.list_reactive().is_empty());
        assert!(!catalog.validation_report().is_valid());

        let _ = std::fs::remove_dir_all(dir);
    }
}
