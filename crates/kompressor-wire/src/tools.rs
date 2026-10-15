//! Recovering what a tool output was the output *of*.
//!
//! The engine's read lifecycle can only notice that a file read was invalidated
//! by a later edit if each tool result says which tool ran and on which file.
//! The wire puts that information in the *call*, several messages earlier, and
//! links the two by id. Resolving that link here is what makes the whole
//! feature reachable through a proxy — the Python plugin never sent the
//! metadata, so the classifier it fed had nothing to classify.
//!
//! Every lookup is by exact id. An output whose call is not in this request
//! keeps `None` for both fields: a wrong tool name produces a wrong staleness
//! verdict, which replaces content the model still needs.

use std::collections::BTreeMap;

use serde_json::Value;

/// A tool call seen in the same request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ToolCall {
    /// The tool's name, as the caller spelled it.
    pub(crate) name: String,
    /// The file the call names, when its arguments carry one.
    pub(crate) file_path: Option<String>,
}

/// Calls indexed by the id their output will quote.
pub(crate) type ToolCallIndex = BTreeMap<String, ToolCall>;

/// The file an invocation names, from already-parsed arguments.
///
/// `file_path` is checked before `path` only to be deterministic when a tool
/// declares both; no harness in practice does.
pub(crate) fn file_path_in_arguments(arguments: &Value) -> Option<String> {
    let object = arguments.as_object()?;
    for key in ["file_path", "path"] {
        if let Some(path) = object.get(key).and_then(Value::as_str)
            && !path.is_empty()
        {
            return Some(path.to_string());
        }
    }
    None
}

/// The file an invocation names, from OpenAI's arguments — a JSON *string*
/// rather than an object, and one a model can emit malformed, in which case
/// there is nothing to recover and nothing is guessed.
pub(crate) fn file_path_in_json_arguments(arguments: &str) -> Option<String> {
    let parsed: Value = serde_json::from_str(arguments).ok()?;
    file_path_in_arguments(&parsed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn either_spelling_of_the_path_argument_is_recovered() {
        assert_eq!(
            file_path_in_arguments(&json!({"file_path": "src/app.rs"})),
            Some("src/app.rs".to_string())
        );
        assert_eq!(
            file_path_in_arguments(&json!({"path": "src/app.rs", "limit": 100})),
            Some("src/app.rs".to_string())
        );
    }

    #[test]
    fn arguments_naming_no_file_yield_none() {
        assert_eq!(file_path_in_arguments(&json!({"command": "ls -la"})), None);
        assert_eq!(file_path_in_arguments(&json!({"path": ""})), None);
        assert_eq!(file_path_in_arguments(&json!("not an object")), None);
    }

    #[test]
    fn malformed_json_arguments_yield_none_rather_than_a_guess() {
        assert_eq!(
            file_path_in_json_arguments("{\"path\": \"a.rs\"}"),
            Some("a.rs".to_string())
        );
        assert_eq!(file_path_in_json_arguments("{\"path\": \"a.rs"), None);
    }
}
