use crate::{AgentTool, ContextualAnchor, ToolCallEventStream};
use agent_client_protocol as acp;
use anyhow::{Result, anyhow};
use gpui::{App, Entity, SharedString, Task};
use language::PointUtf16;
use language_model::{LanguageModelProviderId, LanguageModelToolResultContent};
use project::{AgentLocation, Project};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::fmt::Write;
use std::sync::Arc;
use text::ToPointUtf16;

/// Find references for a symbol specified by a structured ContextualAnchor.
///
/// Input JSON: { "contextual_anchor": { ... } }
///
/// Behaviour:
/// - The anchor's `path` is resolved to a project path.
/// - The buffer is opened in-memory.
/// - The `context_str` must match exactly once in the buffer text. If zero or
///   multiple matches are found the tool returns an informative `raw_output`.
/// - The `token` is searched only inside the matched context span. If zero
///   matches are found an error `raw_output` is returned. If multiple matches
///   exist and the anchor's `index` is not provided, an error `raw_output` is
///   returned. If `index` is provided it disambiguates 1-based occurrences.
/// - Once a single token occurrence is selected, we convert its byte offset to
///   a UTF-16 point and call the project's LSP-backed `references` routine.
/// - The resulting locations are returned and also emitted as ACPT locations so
///   the UI/agent can follow them.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct FindReferencesToolInput {
    pub contextual_anchor: ContextualAnchor,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct FindReferencesLocation {
    pub path: String,
    pub start_line: u32,
    pub start_character: u32,
    pub end_line: u32,
    pub end_character: u32,
    #[serde(default)]
    pub excerpt: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct FindReferencesToolOutput {
    pub locations: Vec<FindReferencesLocation>,
}

impl From<FindReferencesToolOutput> for LanguageModelToolResultContent {
    fn from(output: FindReferencesToolOutput) -> Self {
        if output.locations.is_empty() {
            "No references found".into()
        } else {
            let mut out = format!("Found {} references:", output.locations.len());
            for loc in output.locations {
                let _ = write!(
                    &mut out,
                    "\n- {}:{}:{} - {}:{}{}",
                    loc.path,
                    loc.start_line,
                    loc.start_character,
                    loc.end_line,
                    loc.end_character,
                    loc.excerpt
                        .as_ref()
                        .map(|s| format!(": {}", s))
                        .unwrap_or_default()
                );
            }
            out.into()
        }
    }
}

#[derive(Clone, Debug)]
pub struct FindReferencesTool {
    project: Entity<Project>,
}

impl FindReferencesTool {
    pub fn new(project: Entity<Project>) -> Self {
        Self { project }
    }
}

impl AgentTool for FindReferencesTool {
    type Input = FindReferencesToolInput;
    type Output = FindReferencesToolOutput;

    fn name() -> &'static str {
        "find_references"
    }

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Search
    }

    fn initial_title(
        &self,
        _input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        "Find references".into()
    }

    fn run(
        self: Arc<Self>,
        input: Self::Input,
        event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<Self::Output>> {
        let project = self.project.clone();
        // Basic validation early: ensure context contains token (cheap local check).
        if let Err(e) = input.contextual_anchor.validate_basic() {
            // Provide feedback but still return a ready empty result so the tool call "succeeds".
            let msg = format!("Contextual anchor validation failed: {}", e);
            event_stream.update_fields(acp::ToolCallUpdateFields::new().raw_output(json!(msg)));
            return Task::ready(Ok(FindReferencesToolOutput {
                locations: Vec::new(),
            }));
        }

        let target_path = input.contextual_anchor.path.clone();

        // Resolve project path synchronously while we still have &mut App.
        let project_path = match project.read(cx).find_project_path(&target_path, cx) {
            Some(p) => p,
            None => return Task::ready(Err(anyhow!("Path {} not found in project", target_path))),
        };

        cx.spawn(async move |cx| {
            // Open buffer
            let buffer = cx
                .update(|cx| project.update(cx, |project, cx| project.open_buffer(project_path.clone(), cx)))?
                .await?;

            // Helper to emit a single raw_output message to the agent
            let emit_msg = |event_stream: &ToolCallEventStream, msg: String| {
                event_stream.update_fields(acp::ToolCallUpdateFields::new().raw_output(json!(msg)));
            };

            let ca = input.contextual_anchor;

            // Find occurrences of context_str in buffer text (byte offsets)
            let snippet_occurrences = buffer.read_with(cx, |buffer, _| {
                let text = buffer.text();
                text.match_indices(&ca.context_str).map(|(off, _)| off).collect::<Vec<_>>()
            })?;

            if snippet_occurrences.len() != 1 {
                let msg = if snippet_occurrences.is_empty() {
                    "No occurrences of the provided context_str were found".to_string()
                } else {
                    "Multiple occurrences of the provided context_str were found; contextual anchors must be unique".to_string()
                };
                emit_msg(&event_stream, msg.clone());
                dbg!(snippet_occurrences);
                return Ok(FindReferencesToolOutput { locations: Vec::new() });
            }

            let snippet_start_byte = snippet_occurrences[0];
            let snippet_len = ca.context_str.as_bytes().len();
            let snapshot_len = buffer.read_with(cx, |buffer, _| buffer.snapshot().len())?;
            let snippet_end_byte = snippet_start_byte.saturating_add(snippet_len).min(snapshot_len);

            // Find token occurrences in the snippet
            let token = ca.token.clone();
            let token_occurrences_inside = buffer.read_with(cx, |buffer, _| {
                let full = buffer.text();
                let slice = &full[snippet_start_byte..snippet_end_byte];
                slice.match_indices(&token).map(|(rel_off, _)| snippet_start_byte + rel_off).collect::<Vec<_>>()
            })?;

            if token_occurrences_inside.is_empty() {
                let msg = format!("Token `{}` not found inside the provided context_str", token);
                emit_msg(&event_stream, msg.clone());
                dbg!(msg);
                return Ok(FindReferencesToolOutput { locations: Vec::new() });
            }

            if token_occurrences_inside.len() > 1 && ca.index.is_none() {
                let msg = format!("Multiple occurrences of token `{}` found inside context; provide index to disambiguate", token);
                emit_msg(&event_stream, msg.clone());
                dbg!(msg);
                return Ok(FindReferencesToolOutput { locations: Vec::new() });
            }

            let sel0 = ca.index.unwrap_or(1).saturating_sub(1);
            if sel0 >= token_occurrences_inside.len() {
                let msg = format!("selection_index {} out of range: token occurrences inside context = {}", sel0 + 1, token_occurrences_inside.len());
                emit_msg(&event_stream, msg.clone());
                dbg!(msg);
                return Ok(FindReferencesToolOutput { locations: Vec::new() });
            }

            let chosen_byte_offset = token_occurrences_inside[sel0];
            dbg!(chosen_byte_offset);

            // Convert chosen byte offset to a UTF-16 point
            let (row, column_utf16) = buffer.read_with(cx, |buffer, _| {
                let snapshot = buffer.snapshot();
                let start_pt = snapshot.offset_to_point_utf16(chosen_byte_offset);
                let token_byte_len = token.as_bytes().len();
                let end_byte = chosen_byte_offset.saturating_add(token_byte_len).min(snapshot.len());
                let end_pt = snapshot.offset_to_point_utf16(end_byte);
                let column = if start_pt.row == end_pt.row {
                    (start_pt.column + end_pt.column) / 2
                } else {
                    start_pt.column
                };
                (start_pt.row, column)
            })?;

            // Emit a small test snippet and a full snippet to help the agent contextualize the selection.
            let (snippet, start_row, end_row) = buffer.read_with(cx, |buffer, _| {
                let snapshot = buffer.snapshot();
                let start_pt = snapshot.offset_to_point_utf16(chosen_byte_offset);
                let start_row = start_pt.row.saturating_sub(17);
                let end_row = start_pt.row.saturating_add(14);
                let start_anchor = buffer.anchor_before(text::Point::new(start_row, 0));
                let end_anchor = buffer.anchor_before(text::Point::new(end_row.saturating_add(1), 0));
                let s = buffer.text_for_range(start_anchor..end_anchor).collect::<String>();
                let trimmed = s.trim_end_matches(&['\r', '\n'][..]).to_string();
                (trimmed, start_row, end_row)
            })?;

            event_stream.update_fields(acp::ToolCallUpdateFields::new().raw_output(json!({ "snippet_test": true })));
            event_stream.update_fields(acp::ToolCallUpdateFields::new().raw_output(json!({
                "snippet": snippet,
                "snippet_start_line": start_row,
                "snippet_end_line": end_row,
                "selection_index": sel0 + 1
            })));

            let point = PointUtf16 { row, column: column_utf16 };
            dbg!(point);

            // Call into project LSP-based references
            let references_task = project.update(cx, |project, cx| project.references(&buffer, point, cx))?;
            let maybe_locations = references_task.await?;

            // Convert locations to output and ACPT locations
            let (output_locations, acp_locations, maybe_first_agent_location) = if let Some(locs) = maybe_locations {
                cx.update(|cx| {
                    let mut out = Vec::new();
                    let mut acp_locs = Vec::new();
                    let mut first_agent: Option<AgentLocation> = None;
                    for loc in locs {
                        let buf_entity = loc.buffer;
                        let range = loc.range.clone();
                        let buf = buf_entity.read(cx);
                        let start_point = range.start.to_point_utf16(&buf);
                        let end_point = range.end.to_point_utf16(&buf);

                        let excerpt = {
                            let start_anchor = buf.anchor_before(text::Point::new(start_point.row, 0));
                            let next_line_anchor = buf.anchor_before(text::Point::new(start_point.row.saturating_add(1), 0));
                            let s = buf.text_for_range(start_anchor..next_line_anchor).collect::<String>();
                            let trimmed = s.trim_end_matches(&['\r', '\n'][..]).to_string();
                            dbg!(&trimmed);
                            if trimmed.is_empty() { None } else { Some(trimmed) }
                        };

                        let path = project.read(cx).short_full_path_for_project_path(&project_path, cx)
                            .unwrap_or_else(|| target_path.clone());

                        out.push(FindReferencesLocation {
                            path: path.clone(),
                            start_line: start_point.row,
                            start_character: start_point.column,
                            end_line: end_point.row,
                            end_character: end_point.column,
                            excerpt,
                        });

                        let abs_path = project.read(cx).absolute_path(&project_path, cx)
                            .map(|p| p.to_string_lossy().to_string())
                            .unwrap_or_else(|| path.clone());
                        let mut acp_loc = acp::ToolCallLocation::new(&abs_path);
                        acp_loc = acp_loc.line(Some(start_point.row));
                        acp_locs.push(acp_loc);

                        if first_agent.is_none() {
                            first_agent = Some(AgentLocation { buffer: buf_entity.downgrade(), position: range.start });
                        }
                    }
                    Ok::<_, anyhow::Error>((out, acp_locs, first_agent))
                })?
            } else {
                Ok((Vec::new(), Vec::new(), None))
            }?;

            if let Some(agent_loc) = maybe_first_agent_location {
                if let Err(e) = project.update(cx, |project, cx| { project.set_agent_location(Some(agent_loc), cx); Ok::<(), anyhow::Error>(()) }) {
                    log::error!("Failed to schedule set_agent_location: {:#}", e);
                }
            }

            if !acp_locations.is_empty() {
                let mut fields = acp::ToolCallUpdateFields::new();
                fields = fields.locations(acp_locations.into_iter().collect::<Vec<_>>());
                event_stream.update_fields(fields);
            }

            Ok(FindReferencesToolOutput { locations: output_locations })
        })
    }

    fn supports_provider(_provider: &LanguageModelProviderId) -> bool {
        true
    }
}
