use agent_client_protocol::schema::v1 as acp;
use anyhow::Result;
use gpui::{App, SharedString, Task};
use language_model::LanguageModelToolResultContent;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::rc::Rc;
use std::sync::Arc;

use crate::{AgentTool, SiblingThreadRequest, ThreadEnvironment, ToolCallEventStream, ToolInput};

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub struct CreateThreadToolInput {
    pub title: String,
    pub prompt: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default)]
    pub use_new_worktree: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_ref: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CreateThreadToolOutput {
    Success {
        title: String,
        agent_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        /// A non-fatal heads-up about the created thread (e.g., the project's
        /// worktree layout was unusual and the new worktree may not match
        /// expectations). Present only when there's something to flag.
        #[serde(skip_serializing_if = "Option::is_none")]
        warning: Option<String>,
    },
    Error {
        error: String,
    },
}

impl From<CreateThreadToolOutput> for LanguageModelToolResultContent {
    fn from(output: CreateThreadToolOutput) -> Self {
        serde_json::to_string(&output)
            .unwrap_or_else(|e| format!("Failed to serialize create_thread output: {e}"))
            .into()
    }
}

pub struct CreateThreadTool {
    environment: Rc<dyn ThreadEnvironment>,
}

impl CreateThreadTool {
    pub fn new(environment: Rc<dyn ThreadEnvironment>) -> Self {
        Self { environment }
    }
}

impl AgentTool for CreateThreadTool {
    type Input = CreateThreadToolInput;
    type Output = CreateThreadToolOutput;

    const NAME: &'static str = "create_thread";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Other
    }

    fn initial_title(
        &self,
        input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        match input {
            Ok(i) => format!("Create thread: {}", i.title).into(),
            Err(value) => value
                .get("title")
                .and_then(|v| v.as_str())
                .map(|s| format!("Create thread: {s}").into())
                .unwrap_or_else(|| "Create thread".into()),
        }
    }

    fn run(
        self: Arc<Self>,
        input: ToolInput<Self::Input>,
        _event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<Self::Output, Self::Output>> {
        cx.spawn(async move |cx| {
            let input = input
                .recv()
                .await
                .map_err(|e| CreateThreadToolOutput::Error {
                    error: format!("Failed to receive tool input: {e}"),
                })?;

            let title: SharedString = input.title.clone().into();
            let request = SiblingThreadRequest {
                title: title.clone(),
                prompt: input.prompt,
                agent_id: input.agent,
                model: input.model,
                use_new_worktree: input.use_new_worktree,
                worktree_name: input.worktree_name,
                base_ref: input.base_ref,
            };

            let task = self.environment.create_sibling_thread(request, cx);
            match task.await {
                Ok(info) => Ok(CreateThreadToolOutput::Success {
                    title: info.title.to_string(),
                    agent_id: info.agent_id,
                    model: info.model,
                    warning: info.warning,
                }),
                Err(error) => Err(CreateThreadToolOutput::Error {
                    error: error.to_string(),
                }),
            }
        })
    }
}
