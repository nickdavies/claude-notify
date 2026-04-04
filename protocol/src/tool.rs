//! Canonical tool identity used across the system.
//!
//! `Tool` is the normalized representation of a tool name. It serializes to/from
//! the Claude Code PascalCase name (the canonical format). Unknown tools serialize
//! as their raw string — the system passes them through without special handling.

use std::fmt;

use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Canonical tool identity used across the system.
///
/// Known variants map 1:1 to Claude Code's PascalCase tool names.
/// `Unknown(String)` captures MCP tools, future tools, or anything not yet
/// in the enum — they round-trip through serde as their raw string.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Tool {
    // File read
    Read,
    Grep,
    Glob,
    SemanticSearch,

    // File write
    Write,
    StrReplace,
    Delete,
    Edit,
    MultiEdit,
    EditNotebook,
    NotebookEdit,

    // Shell
    Bash,

    // Agent / meta
    Task,
    TodoWrite,

    // Other
    WebFetch,

    // Extensibility — MCP tools, future tools, etc.
    Unknown(String),
}

impl Tool {
    /// The canonical string name (Claude Code PascalCase format).
    pub fn as_str(&self) -> &str {
        match self {
            Tool::Read => "Read",
            Tool::Grep => "Grep",
            Tool::Glob => "Glob",
            Tool::SemanticSearch => "SemanticSearch",
            Tool::Write => "Write",
            Tool::StrReplace => "StrReplace",
            Tool::Delete => "Delete",
            Tool::Edit => "Edit",
            Tool::MultiEdit => "MultiEdit",
            Tool::EditNotebook => "EditNotebook",
            Tool::NotebookEdit => "NotebookEdit",
            Tool::Bash => "Bash",
            Tool::Task => "Task",
            Tool::TodoWrite => "TodoWrite",
            Tool::WebFetch => "WebFetch",
            Tool::Unknown(s) => s.as_str(),
        }
    }

    /// Parse a canonical string into a `Tool`.
    fn from_canonical(s: &str) -> Tool {
        match s {
            "Read" => Tool::Read,
            "Grep" => Tool::Grep,
            "Glob" => Tool::Glob,
            "SemanticSearch" => Tool::SemanticSearch,
            "Write" => Tool::Write,
            "StrReplace" => Tool::StrReplace,
            "Delete" => Tool::Delete,
            "Edit" => Tool::Edit,
            "MultiEdit" => Tool::MultiEdit,
            "EditNotebook" => Tool::EditNotebook,
            "NotebookEdit" => Tool::NotebookEdit,
            "Bash" => Tool::Bash,
            "Task" => Tool::Task,
            "TodoWrite" => Tool::TodoWrite,
            "WebFetch" => Tool::WebFetch,
            other => Tool::Unknown(other.to_owned()),
        }
    }
}

impl fmt::Display for Tool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl AsRef<str> for Tool {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

// --- Serde ---

impl Serialize for Tool {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for Tool {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Ok(Tool::from_canonical(&s))
    }
}

// --- JsonSchema ---
//
// schemars 1.x doesn't derive cleanly for untagged-style enums with data variants,
// so we implement manually. The schema says "any string" with known values documented.

impl JsonSchema for Tool {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "Tool".into()
    }

    fn json_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "string",
            "description": "Canonical tool name. Known values: Read, Grep, Glob, SemanticSearch, Write, StrReplace, Delete, Edit, MultiEdit, EditNotebook, NotebookEdit, Bash, Task, TodoWrite, WebFetch. Arbitrary strings are accepted for unknown/MCP tools."
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_tool_roundtrip() {
        let tool = Tool::Bash;
        let json = serde_json::to_string(&tool).unwrap();
        assert_eq!(json, r#""Bash""#);
        let back: Tool = serde_json::from_str(&json).unwrap();
        assert_eq!(back, Tool::Bash);
    }

    #[test]
    fn unknown_tool_roundtrip() {
        let tool = Tool::Unknown("my_mcp_tool".to_owned());
        let json = serde_json::to_string(&tool).unwrap();
        assert_eq!(json, r#""my_mcp_tool""#);
        let back: Tool = serde_json::from_str(&json).unwrap();
        assert_eq!(back, Tool::Unknown("my_mcp_tool".to_owned()));
    }

    #[test]
    fn display_known() {
        assert_eq!(Tool::SemanticSearch.to_string(), "SemanticSearch");
    }

    #[test]
    fn display_unknown() {
        assert_eq!(Tool::Unknown("custom".to_owned()).to_string(), "custom");
    }

    #[test]
    fn as_str_matches_display() {
        for tool in [
            Tool::Read,
            Tool::Grep,
            Tool::Glob,
            Tool::Write,
            Tool::Bash,
            Tool::Task,
            Tool::TodoWrite,
            Tool::Unknown("x".to_owned()),
        ] {
            assert_eq!(tool.as_str(), tool.to_string());
        }
    }

    #[test]
    fn all_known_variants_deserialize() {
        let names = [
            "Read",
            "Grep",
            "Glob",
            "SemanticSearch",
            "Write",
            "StrReplace",
            "Delete",
            "Edit",
            "MultiEdit",
            "EditNotebook",
            "NotebookEdit",
            "Bash",
            "Task",
            "TodoWrite",
            "WebFetch",
        ];
        for name in names {
            let json = format!("\"{name}\"");
            let tool: Tool = serde_json::from_str(&json).unwrap();
            assert!(
                !matches!(tool, Tool::Unknown(_)),
                "{name} deserialized as Unknown"
            );
            assert_eq!(tool.as_str(), name);
        }
    }
}
