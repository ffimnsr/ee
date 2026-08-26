//! Provider-facing tool schema compiler.
//!
//! Model adapters consume [`ToolDefinition`] values; [`compile_tool_schema`]
//! turns one definition into the canonical provider-facing JSON schema —
//! name, description, argument schema, side-effect metadata, and dependency
//! metadata — with recursively sorted keys so serialized schemas are stable
//! snapshot targets regardless of how the source schema was built.

use serde_json::{Map, Value};

use crate::tool_dependencies::ToolDataClass;
use crate::tools::ToolDefinition;

/// Compiles one tool definition into its provider-facing JSON schema.
///
/// The emitted schema always carries `name`, `description`, `inputSchema`,
/// `sideEffect`, `sideEffectSubclass`, `requiredCapabilities`, `requires`,
/// and `produces`; object keys are sorted recursively for deterministic
/// serialization.
#[must_use]
pub fn compile_tool_schema(definition: &ToolDefinition) -> Value {
    let mut schema = Map::new();
    schema.insert("name".into(), Value::String(definition.name.clone()));
    schema.insert("description".into(), Value::String(definition.description.clone()));
    schema.insert("inputSchema".into(), canonicalize(&definition.input_schema));
    schema.insert("sideEffect".into(), Value::String(definition.side_effect_class.as_str().into()));
    schema.insert(
        "sideEffectSubclass".into(),
        definition
            .side_effect_subclass
            .map(|subclass| Value::String(subclass.as_str().into()))
            .unwrap_or(Value::Null),
    );
    schema.insert(
        "requiredCapabilities".into(),
        Value::Array(
            definition
                .required_capabilities
                .iter()
                .map(|capability| Value::String(capability.clone()))
                .collect(),
        ),
    );
    schema.insert("requires".into(), data_classes(&definition.dependency.requires));
    schema.insert("produces".into(), data_classes(&definition.dependency.produces));
    Value::Object(schema)
}

/// Compiles every definition, preserving the input order (callers pass
/// registry definitions, which are sorted by name).
#[must_use]
pub fn compile_schemas(definitions: &[ToolDefinition]) -> Vec<Value> {
    definitions.iter().map(compile_tool_schema).collect()
}

/// Validates a compiled schema: non-empty name and description, an object
/// `inputSchema` that declares `required` when it declares `properties`, and
/// a known `sideEffect` value.
pub fn validate_compiled_schema(schema: &Value) -> Result<(), String> {
    let name = schema
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| "schema must include a string name".to_string())?;
    if name.is_empty() {
        return Err("schema name must not be empty".into());
    }
    let description = schema
        .get("description")
        .and_then(Value::as_str)
        .ok_or_else(|| "schema must include a string description".to_string())?;
    if description.is_empty() {
        return Err("schema description must not be empty".into());
    }
    let input = schema
        .get("inputSchema")
        .and_then(Value::as_object)
        .ok_or_else(|| "schema must include an object inputSchema".to_string())?;
    if input.contains_key("properties") {
        match input.get("required") {
            Some(Value::Array(_)) => {}
            Some(_) => return Err("inputSchema required must be an array".into()),
            None => {
                return Err("inputSchema with properties must declare required arguments".into());
            }
        }
    }
    let side_effect = schema
        .get("sideEffect")
        .and_then(Value::as_str)
        .ok_or_else(|| "schema must include a sideEffect string".to_string())?;
    if !matches!(side_effect, "read" | "write" | "execute" | "delegate") {
        return Err(format!("unknown sideEffect class: {side_effect}"));
    }
    Ok(())
}

fn data_classes(classes: &[ToolDataClass]) -> Value {
    Value::Array(classes.iter().map(|class| Value::String(class.as_str().into())).collect())
}

/// Recursively sorts object keys so serialized output is a stable snapshot.
fn canonicalize(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut pairs: Vec<(String, Value)> =
                map.iter().map(|(key, value)| (key.clone(), canonicalize(value))).collect();
            pairs.sort_by(|a, b| a.0.cmp(&b.0));
            let mut sorted = Map::new();
            for (key, value) in pairs {
                sorted.insert(key, value);
            }
            Value::Object(sorted)
        }
        Value::Array(items) => Value::Array(items.iter().map(canonicalize).collect()),
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use ee_agent_protocol::SessionId;
    use serde_json::json;

    use super::*;
    use crate::tools::ToolRegistry;

    fn builtin_schemas() -> Vec<Value> {
        let mut registry = ToolRegistry::new();
        registry.register_builtins(&SessionId::new("s-1")).expect("builtins register");
        compile_schemas(&registry.definitions())
    }

    #[test]
    fn compiled_schemas_validate() {
        for schema in builtin_schemas() {
            validate_compiled_schema(&schema).expect("compiled schema is valid");
        }
    }

    #[test]
    fn validation_rejects_missing_fields() {
        assert!(validate_compiled_schema(&json!({})).is_err());
        assert!(
            validate_compiled_schema(&json!({
                "name": "x",
                "description": "",
                "inputSchema": { "type": "object" },
                "sideEffect": "read",
            }))
            .is_err(),
            "empty description is rejected"
        );
        assert!(
            validate_compiled_schema(&json!({
                "name": "x",
                "description": "d",
                "inputSchema": { "type": "object", "properties": {} },
                "sideEffect": "read",
            }))
            .is_err(),
            "properties without required are rejected"
        );
        assert!(
            validate_compiled_schema(&json!({
                "name": "x",
                "description": "d",
                "inputSchema": { "type": "object" },
                "sideEffect": "teleport",
            }))
            .is_err(),
            "unknown side effect is rejected"
        );
    }

    #[test]
    fn compiled_schema_contains_dependency_metadata() {
        let mut registry = ToolRegistry::new();
        registry.register_builtins(&SessionId::new("s-1")).expect("builtins register");
        let schemas = compile_schemas(&registry.definitions());
        let terminal_output = schemas
            .iter()
            .find(|schema| schema.get("name").and_then(Value::as_str) == Some("terminal_output"))
            .expect("terminal_output is registered");
        assert_eq!(terminal_output.get("requires").expect("requires"), &json!(["terminal_handle"]));
        assert_eq!(terminal_output.get("produces").expect("produces"), &json!(["terminal_output"]));
        assert_eq!(terminal_output.get("sideEffect").expect("sideEffect"), &json!("execute"));
    }

    #[test]
    fn builtin_schemas_snapshot_is_stable() {
        let schemas = builtin_schemas();
        let snapshot = serde_json::to_string_pretty(&schemas).expect("schemas serialize");
        assert_eq!(
            snapshot,
            r#"[
  {
    "name": "ask_user",
    "description": "Asks the user for input through the editor",
    "inputSchema": {
      "properties": {
        "message": {
          "type": "string"
        }
      },
      "required": [
        "message"
      ],
      "type": "object"
    },
    "sideEffect": "read",
    "sideEffectSubclass": null,
    "requiredCapabilities": [],
    "requires": [],
    "produces": [
      "user_input"
    ]
  },
  {
    "name": "cargo_check",
    "description": "Runs focused cargo check through editor terminal",
    "inputSchema": {
      "properties": {
        "path": {
          "type": "string"
        }
      },
      "required": [],
      "type": "object"
    },
    "sideEffect": "execute",
    "sideEffectSubclass": null,
    "requiredCapabilities": [],
    "requires": [],
    "produces": []
  },
  {
    "name": "create_terminal",
    "description": "Creates a terminal running a command",
    "inputSchema": {
      "properties": {
        "command": {
          "type": "string"
        },
        "cwd": {
          "type": "string"
        }
      },
      "required": [
        "command"
      ],
      "type": "object"
    },
    "sideEffect": "execute",
    "sideEffectSubclass": null,
    "requiredCapabilities": [],
    "requires": [],
    "produces": [
      "terminal_handle"
    ]
  },
  {
    "name": "kill_terminal",
    "description": "Kills a terminal without releasing it",
    "inputSchema": {
      "properties": {
        "terminal_id": {
          "type": "string"
        }
      },
      "required": [
        "terminal_id"
      ],
      "type": "object"
    },
    "sideEffect": "execute",
    "sideEffectSubclass": "terminal_kill",
    "requiredCapabilities": [],
    "requires": [
      "terminal_handle"
    ],
    "produces": []
  },
  {
    "name": "read_file",
    "description": "Reads a text file through the editor",
    "inputSchema": {
      "properties": {
        "path": {
          "type": "string"
        }
      },
      "required": [
        "path"
      ],
      "type": "object"
    },
    "sideEffect": "read",
    "sideEffectSubclass": null,
    "requiredCapabilities": [],
    "requires": [],
    "produces": [
      "file_text"
    ]
  },
  {
    "name": "release_terminal",
    "description": "Releases a terminal and its resources",
    "inputSchema": {
      "properties": {
        "terminal_id": {
          "type": "string"
        }
      },
      "required": [
        "terminal_id"
      ],
      "type": "object"
    },
    "sideEffect": "execute",
    "sideEffectSubclass": null,
    "requiredCapabilities": [],
    "requires": [
      "terminal_handle"
    ],
    "produces": []
  },
  {
    "name": "terminal_output",
    "description": "Fetches a terminal's current output",
    "inputSchema": {
      "properties": {
        "terminal_id": {
          "type": "string"
        }
      },
      "required": [
        "terminal_id"
      ],
      "type": "object"
    },
    "sideEffect": "execute",
    "sideEffectSubclass": null,
    "requiredCapabilities": [],
    "requires": [
      "terminal_handle"
    ],
    "produces": [
      "terminal_output"
    ]
  },
  {
    "name": "wait_for_terminal_exit",
    "description": "Waits for a terminal command to exit; timeout_ms triggers kill, output capture, and release.",
    "inputSchema": {
      "properties": {
        "terminal_id": {
          "type": "string"
        },
        "timeout_ms": {
          "type": "integer"
        }
      },
      "required": [
        "terminal_id"
      ],
      "type": "object"
    },
    "sideEffect": "execute",
    "sideEffectSubclass": null,
    "requiredCapabilities": [],
    "requires": [
      "terminal_handle"
    ],
    "produces": [
      "terminal_exit",
      "terminal_output"
    ]
  },
  {
    "name": "write_file",
    "description": "Writes a text file through the editor",
    "inputSchema": {
      "properties": {
        "content": {
          "type": "string"
        },
        "path": {
          "type": "string"
        }
      },
      "required": [
        "path",
        "content"
      ],
      "type": "object"
    },
    "sideEffect": "write",
    "sideEffectSubclass": "overwrite",
    "requiredCapabilities": [],
    "requires": [],
    "produces": [
      "file_text"
    ]
  }
]"#
        );
    }
}
