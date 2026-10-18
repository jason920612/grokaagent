//! `submit_result`: how a workflow agent asked for structured output hands
//! it in. The tool's parameters are the caller's schema, so the model fills
//! in the result as the call's arguments. A call that does not fit comes
//! back as an error listing what to fix; an accepted call ends the agent's
//! turn chain (it is the kernel's terminal tool), with the arguments as
//! the final answer.

use serde_json::{json, Value};

use crate::error::Error;
use crate::tools::{ClientTool, ToolCallFut, ToolSpec};

pub const NAME: &str = "submit_result";

pub struct SubmitResultTool {
    schema: Value,
}

impl SubmitResultTool {
    pub fn new(schema: Value) -> Self {
        Self { schema }
    }
}

/// What the agent is told about the tool, appended to its task.
pub fn task_suffix() -> String {
    format!(
        "\n\nWhen you are done, hand in your final result by calling `{NAME}`: its arguments are the result and must match its schema. That call is your answer: anything you write after it is not read."
    )
}

impl ClientTool for SubmitResultTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: NAME.into(),
            description: "Hand in your final result. The arguments ARE the result and must match this tool's schema; a mismatch comes back as an error to fix. An accepted call ends your task.".into(),
            parameters: self.schema.clone(),
        }
    }

    fn call(&self, args: &Value) -> ToolCallFut<'_> {
        let errors = crate::jschema::validate(args, &self.schema);
        let out = if errors.is_empty() {
            Ok(json!({"accepted": true, "result": args}).to_string())
        } else {
            let list: Vec<String> = errors.iter().take(20).map(|e| format!("- {e}")).collect();
            Err(Error::Tool(format!(
                "the result does not match the schema; fix these and call {NAME} again:\n{}",
                list.join("\n")
            )))
        };
        Box::pin(async move { out })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn accepts_a_fitting_result_and_explains_a_misfit() {
        let tool = SubmitResultTool::new(json!({
            "type": "object",
            "properties": {"n": {"type": "integer"}},
            "required": ["n"]
        }));
        let ok: Value = serde_json::from_str(&tool.call(&json!({"n": 2})).await.unwrap()).unwrap();
        assert_eq!(ok, json!({"accepted": true, "result": {"n": 2}}));
        let err = tool.call(&json!({"n": "two"})).await.unwrap_err().to_string();
        assert!(err.contains("$.n: expected integer, got string"), "{err}");
    }
}
