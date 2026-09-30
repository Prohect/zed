use std::fmt::Write;
use std::sync::Arc;

use agent_client_protocol::schema::v1 as acp;
use collections::HashSet;
use gpui::{App, Entity, SharedString, Task};
use project::Project;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::symbol_locator::SymbolLocator;
use crate::{AgentTool, ProjectScope, ToolCallEventStream, ToolInput};

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct RenameToolInput {
    pub symbol: SymbolLocator,
    pub new_name: String,
}

pub struct RenameTool {
    project: Entity<Project>,
    scope: ProjectScope,
}

impl RenameTool {
    pub fn new(project: Entity<Project>, scope: ProjectScope) -> Self {
        Self { project, scope }
    }
}

impl AgentTool for RenameTool {
    type Input = RenameToolInput;
    type Output = String;

    const NAME: &'static str = "rename_symbol";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Other
    }

    fn initial_title(
        &self,
        input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        if let Ok(input) = input {
            format!(
                "Rename `{}` to `{}`",
                input.symbol.symbol_name, input.new_name
            )
            .into()
        } else {
            "Rename symbol".into()
        }
    }

    fn run(
        self: Arc<Self>,
        input: ToolInput<Self::Input>,
        _event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<String, String>> {
        let project = self.project.clone();
        let scope = self.scope.clone();
        cx.spawn(async move |cx| {
            let input = input
                .recv()
                .await
                .map_err(|e| format!("Failed to receive tool input: {e}"))?;

            project.read_with(cx, |project, cx| {
                scope
                    .resolve_project_path(project, &input.symbol.file_path, cx)
                    .ok_or_else(|| {
                        format!(
                            "Path '{}' isn't in this project or is outside the session's workspace scope.",
                            input.symbol.file_path
                        )
                    })
            })?;

            let resolved = input.symbol.resolve(&project, cx).await?;

            let rename_task = project.update(cx, |project, cx| {
                project.perform_rename(
                    resolved.buffer.clone(),
                    resolved.position,
                    input.new_name.clone(),
                    None,
                    cx,
                )
            });

            let transaction = rename_task
                .await
                .map_err(|e| format!("Rename failed: {e}"))?;

            if transaction.0.is_empty() {
                return Ok(format!(
                    "No changes were made. The language server could not rename '{}'.",
                    input.symbol.symbol_name
                ));
            }

            let buffers = transaction.0.keys().cloned().collect::<HashSet<_>>();
            project
                .update(cx, |project, cx| project.save_buffers(buffers, cx))
                .await
                .map_err(|e| format!("Rename succeeded, but failed to save renamed files: {e}"))?;

            let mut output = format!(
                "Renamed `{}` to `{}` in {} file(s):\n",
                input.symbol.symbol_name,
                input.new_name,
                transaction.0.len()
            );

            for (buffer, _) in &transaction.0 {
                buffer.read_with(cx, |buffer, cx| {
                    let path = buffer
                        .file()
                        .map(|f| f.full_path(cx).display().to_string())
                        .unwrap_or_else(|| "<untitled>".to_string());
                    writeln!(output, "- {path}").ok();
                });
            }

            Ok(output)
        })
    }
}
