//! Two-tier tool instructions: the guidance tier.
//!
//! Each tool's model-facing documentation is split into a non-overridable API
//! contract authored in code (the schema structure, and the descriptions the
//! tool's own doc comments generate) and overridable guidance (principles,
//! examples, pitfalls) authored as Handlebars files. Guidance covers both the
//! tool description and individual input-schema nodes, so a parameter's
//! description can be extended without touching Rust.
//!
//! # File tree
//!
//! Guidance for tool `<name>` lives under `tool_guidance/<name>/`:
//!
//! ```text
//! tool_guidance/edit_file/
//!   &self.hbs          extends the tool's own description
//!   path.hbs           extends `properties.path`'s description
//!   edits.hbs          extends `properties.edits`'s description
//!   $edits/            descends into `properties.edits`
//!     items.hbs        extends `properties.edits.items`'s description
//!     $items/          descends into `properties.edits.items`
//!       old_text.hbs   extends `properties.edits.items.properties.old_text`
//!       new_text.hbs   extends `properties.edits.items.properties.new_text`
//! ```
//!
//! A `$`-prefixed directory descends into the schema node it names; a plain
//! `<name>.hbs` file extends the schema node named `<name>` — a property, or
//! the `items` keyword inside an array node. `&self` is only meaningful at the
//! tool root, where it extends the tool's own description. `$` is required so a
//! schema descent is distinguishable from an organizational directory whose
//! files are partials (`shared/tips.hbs` → `{{> shared/tips}}`).
//!
//! Guidance is *appended* to whatever the tool authored in code, never
//! replacing it: the API contract stays a developer's non-overridable baseline.
//!
//! Tool and parameter names are conventionally `[A-Za-z0-9_-]`. Names
//! containing `&` or `$` are unsupported; their files are ignored with a log
//! warning rather than being silently reinterpreted.
//!
//! Guidance is rendered through the same Handlebars engine and session context
//! as the system prompt ([`agent_settings::render_rules_template`]). A built-in
//! default embedded from `src/tool_guidance/` is shadowed — skills-style — by a
//! same-named file in the user-global `tool_guidance` config directory. A
//! tool's guidance reaches the model only when the tool itself is available.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use agent_settings::RulesTemplateContext;
use fs::Fs;
use futures::StreamExt as _;
use gpui::{App, BorrowAppContext, Global, Task};
use rust_embed::RustEmbed;
use serde_json::Value;
use util::ResultExt as _;

/// Built-in default guidance, embedded from `src/tool_guidance/**/*.hbs`.
///
/// Both the flat `*.hbs` files and the `**/*.hbs` tree are embedded, so the
/// legacy per-tool files keep working while defaults move into directories.
#[derive(RustEmbed)]
#[folder = "src/tool_guidance"]
#[include = "*.hbs"]
#[include = "**/*.hbs"]
struct BuiltinGuidance;

/// Built-in guidance files: relative path without the extension, `/`-separated
/// (`edit_file/&self`, `edit_file/$edits/items`) → content.
static BUILTIN_GUIDANCE: LazyLock<BTreeMap<String, String>> = LazyLock::new(|| {
    let mut files = BTreeMap::new();
    for path in BuiltinGuidance::iter() {
        let Some(name) = path.strip_suffix(".hbs") else {
            continue;
        };
        if let Some(content) = BuiltinGuidance::get(&path)
            .and_then(|content| String::from_utf8(content.data.into_owned()).log_err())
        {
            files.insert(name.to_string(), content);
        }
    }
    files
});

/// The built-in guidance extending a tool's own description, if it has any.
///
/// Prefers the tree form `tool_guidance/<tool>/&self.hbs`; the flat form
/// `tool_guidance/<tool>.hbs` is accepted as a legacy alias so existing
/// defaults keep working while they are moved into directories.
pub fn builtin_guidance(tool_name: &str) -> Option<&'static str> {
    BUILTIN_GUIDANCE
        .get(&format!("{tool_name}/&self"))
        .or_else(|| BUILTIN_GUIDANCE.get(tool_name))
        .map(String::as_str)
}

/// Every built-in guidance file for `tool_name`, as `(path relative to the
/// `tool_guidance` directory including `.hbs`, content)`, for materializing the
/// tool's whole tree in the UI. The flat legacy file is surfaced under its tree
/// path.
pub fn builtin_files(tool_name: &str) -> Vec<(String, &'static str)> {
    let prefix = format!("{tool_name}/");
    let mut files: Vec<(String, &'static str)> = BUILTIN_GUIDANCE
        .iter()
        .filter(|(name, _)| name.starts_with(&prefix))
        .map(|(name, content)| (format!("{name}.hbs"), content.as_str()))
        .collect();
    if let Some(content) = BUILTIN_GUIDANCE.get(tool_name) {
        files.push((format!("{tool_name}/&self.hbs"), content.as_str()));
    }
    files
}

/// The content written to `tool_guidance/<tool>/&self.hbs` when the user
/// materializes the tool's tree and it has no built-in guidance.
pub fn default_tool_guidance_stub(tool_name: &str) -> String {
    format!(
        "{{{{!--\n\
         Guidance for the `{tool_name}` tool, appended to the tool's model-facing\n\
         description whenever `{tool_name}` is available in the session. Text is\n\
         emitted verbatim; handlebars comments like this one are stripped and never reach\n\
         the model.\n\
         \n\
         This tool has no built-in default guidance — replace this comment with your own.\n\
         Add `<param>.hbs` files here to extend an input parameter's description, and\n\
         `$<param>/…` directories to reach nested nodes (see the README).\n\
         Context variables: available_tools, model_name, date, is_windows, is_linux,\n\
         is_macos, sandboxing. Gate sections with {{{{#if (contains available_tools 'x')}}}}…{{{{/if}}}}.\n\
         Other guidance files here are importable as partials by relative path\n\
         (`shared/tips.hbs` → `{{{{> shared/tips}}}}`, `/`-separated on every platform).\n\
         --}}}}\n"
    )
}

/// The default tree materialized in the UI for `tool_name`: every embedded
/// guidance file, plus a placeholder template for each input-schema node that
/// has no default yet, so parameters are editable out of the box. Paths are
/// relative to the `tool_guidance` directory and include the `.hbs` extension.
pub fn default_tool_guidance_files(tool_name: &str) -> Vec<(String, String)> {
    let mut files: BTreeMap<String, String> = builtin_files(tool_name)
        .into_iter()
        .map(|(path, content)| (path, content.to_string()))
        .collect();
    if let Some(schema) = built_in_tool_schema(tool_name) {
        collect_parameter_stubs(tool_name, "", &schema, &mut files);
    }
    files
        .entry(format!("{tool_name}/&self.hbs"))
        .or_insert_with(|| default_tool_guidance_stub(tool_name));
    files.into_iter().collect()
}

/// The normalized input schema of a built-in tool, if `tool_name` names one.
/// Context-server (MCP) tools are not scaffolded, since their schemas are only
/// known at runtime.
fn built_in_tool_schema(tool_name: &str) -> Option<Value> {
    use language_model::LanguageModelRequestToolInput;

    crate::tools::built_in_tools()
        .find(|tool| tool.name == tool_name)
        .and_then(|tool| match tool.input {
            LanguageModelRequestToolInput::Function { input_schema, .. } => Some(input_schema),
            LanguageModelRequestToolInput::Custom { .. } => None,
        })
}

/// Inserts a placeholder template for every `properties.<name>` and `items`
/// node reachable from `schema`, placed under `dir` (relative to the tool
/// directory, no leading slash). Existing entries are never overwritten, so an
/// embedded default always wins over a scaffold.
fn collect_parameter_stubs(
    tool_name: &str,
    dir: &str,
    schema: &Value,
    files: &mut BTreeMap<String, String>,
) {
    if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
        for (name, child) in properties {
            files
                .entry(format!(
                    "{tool_name}/{}",
                    relative_path(dir, &format!("{name}.hbs"))
                ))
                .or_insert_with(|| parameter_guidance_stub(tool_name, name));
            if has_subschema(child) {
                collect_parameter_stubs(
                    tool_name,
                    &relative_path(dir, &format!("${name}")),
                    child,
                    files,
                );
            }
        }
    }
    if let Some(items) = schema.get("items") {
        files
            .entry(format!("{tool_name}/{}", relative_path(dir, "items.hbs")))
            .or_insert_with(|| parameter_guidance_stub(tool_name, "items"));
        if has_subschema(items) {
            collect_parameter_stubs(tool_name, &relative_path(dir, "$items"), items, files);
        }
    }
}

/// Whether a schema node has children worth descending into for scaffolding.
fn has_subschema(node: &Value) -> bool {
    node.get("properties")
        .and_then(Value::as_object)
        .is_some_and(|properties| !properties.is_empty())
        || node.get("items").is_some()
}

fn relative_path(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_string()
    } else {
        format!("{dir}/{name}")
    }
}

/// The placeholder written to a `<param>.hbs` scaffold: a comment the user
/// replaces, naming the input it extends.
fn parameter_guidance_stub(tool_name: &str, node: &str) -> String {
    format!(
        "{{{{!--\n\
         Guidance for the `{node}` input of the `{tool_name}` tool. Appended to that\n\
         input's model-facing description. Text is emitted verbatim; handlebars comments\n\
         like this one are stripped and never reach the model. Replace this comment, or\n\
         clear the file to add nothing. Nested inputs live in `$<param>` subdirectories\n\
         (see the README).\n\
         --}}}}\n"
    )
}

/// Where a guidance file's rendered text is appended.
#[derive(Debug, Clone, PartialEq, Eq)]
enum DocTarget {
    /// The tool's own description.
    ToolGuidance,
    /// The `description` of the input-schema node at this JSON pointer, which
    /// only ever walks `properties` and `items`.
    Schema(String),
}

/// Resolves a tool-relative guidance path (with the `&`/`$` markers, no
/// extension) to its target. Returns `None` for reserved or malformed paths.
fn parse_target(rest: &str) -> Option<DocTarget> {
    if rest == "&self" {
        return Some(DocTarget::ToolGuidance);
    }
    if rest.is_empty() {
        return None;
    }

    let segments: Vec<&str> = rest.split('/').collect();
    let mut pointer = String::new();
    for (index, raw) in segments.iter().enumerate() {
        let segment = *raw;
        let is_last = index + 1 == segments.len();
        let name = match segment.strip_prefix('$') {
            // A `$` segment descends, so it can never be the final component.
            Some(name) => {
                if is_last {
                    return None;
                }
                name
            }
            // A plain segment names a file, so it must be the final component.
            None => {
                if !is_last {
                    return None;
                }
                segment
            }
        };
        if name.is_empty() || name == "&self" || name.contains('&') || name.contains('$') {
            return None;
        }
        if name == "items" {
            pointer.push_str("/items");
        } else {
            pointer.push_str("/properties/");
            pointer.push_str(name);
        }
    }
    Some(DocTarget::Schema(pointer))
}

/// How a tool's guidance override relates to the built-in default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolGuidanceOverrideState {
    /// No user override file.
    Absent,
    /// Every user override file's content equals the built-in default.
    Default,
    /// Some user override file exists and differs from the built-in default
    /// (or there is no built-in default to compare against).
    Overridden,
}

/// The user-global tool guidance overrides, kept up to date by a watcher.
pub struct ToolGuidanceStore {
    /// Relative path without extension, `/`-separated → content.
    user_files: BTreeMap<String, String>,
    _watcher: Task<()>,
}

impl Global for ToolGuidanceStore {}

impl ToolGuidanceStore {
    pub fn global(cx: &App) -> Option<&Self> {
        cx.try_global::<ToolGuidanceStore>()
    }

    /// Built-in defaults shadowed by the user's overrides of the same path.
    fn merged_files(&self) -> BTreeMap<String, String> {
        let mut files = BUILTIN_GUIDANCE.clone();
        files.extend(
            self.user_files
                .iter()
                .map(|(name, content)| (name.clone(), content.clone())),
        );
        files
    }

    pub fn override_state(&self, tool_name: &str) -> ToolGuidanceOverrideState {
        let prefix = format!("{tool_name}/");
        let mut any = false;
        for (name, content) in &self.user_files {
            if name != tool_name && !name.starts_with(&prefix) {
                continue;
            }
            any = true;
            match BUILTIN_GUIDANCE.get(name) {
                Some(default) if content.trim() == default.trim() => {}
                _ => return ToolGuidanceOverrideState::Overridden,
            }
        }
        if any {
            ToolGuidanceOverrideState::Default
        } else {
            ToolGuidanceOverrideState::Absent
        }
    }

    /// Renders every guidance file for `tool_name` and appends it to the tool's
    /// description and to the addressed input-schema node descriptions.
    ///
    /// A render failure or a path that resolves to a node the schema does not
    /// have is logged and skipped rather than failing the request build.
    pub fn apply(
        &self,
        tool_name: &str,
        description: &mut String,
        schema: &mut Value,
        context: &RulesTemplateContext,
    ) {
        apply_guidance_files(
            &self.merged_files(),
            tool_name,
            description,
            schema,
            context,
        );
    }
}

/// Applies only the built-in default guidance, without user overrides — what a
/// session with no `tool_guidance` overrides sends to the model.
#[cfg(test)]
pub(crate) fn apply_builtin_guidance(
    tool_name: &str,
    description: &mut String,
    schema: &mut Value,
    context: &RulesTemplateContext,
) {
    apply_guidance_files(&BUILTIN_GUIDANCE, tool_name, description, schema, context);
}

fn apply_guidance_files(
    files: &BTreeMap<String, String>,
    tool_name: &str,
    description: &mut String,
    schema: &mut Value,
    context: &RulesTemplateContext,
) {
    let prefix = format!("{tool_name}/");
    for (key, source) in files.iter() {
        let target = if key == tool_name {
            // Legacy flat file: `tool_guidance/<tool>.hbs`.
            DocTarget::ToolGuidance
        } else if let Some(rest) = key.strip_prefix(&prefix) {
            match parse_target(rest) {
                Some(target) => target,
                None => {
                    log::warn!("ignoring unresolvable tool guidance path `{key}`");
                    continue;
                }
            }
        } else {
            continue;
        };

        let rendered = match agent_settings::render_rules_template(source, files, context) {
            Ok(rendered) => rendered,
            Err(err) => {
                log::error!("Failed to render tool guidance for `{key}`: {err:#}");
                continue;
            }
        };
        let rendered = rendered.trim();
        if rendered.is_empty() {
            continue;
        }

        match target {
            DocTarget::ToolGuidance => append_section(description, rendered),
            DocTarget::Schema(pointer) => {
                let Some(Value::Object(node)) = schema.pointer_mut(&pointer) else {
                    log::warn!("tool guidance `{key}` targets absent schema node `{pointer}`");
                    continue;
                };
                append_description(node, rendered);
            }
        }
    }
}

fn append_section(description: &mut String, section: &str) {
    if !description.is_empty() {
        description.push_str("\n\n");
    }
    description.push_str(section);
}

fn append_description(node: &mut serde_json::Map<String, Value>, section: &str) {
    let merged = match node.get("description").and_then(Value::as_str) {
        Some(existing) if !existing.is_empty() => format!("{existing}\n\n{section}"),
        _ => section.to_string(),
    };
    node.insert("description".to_string(), Value::String(merged));
}

/// Initialize the tool guidance store by scanning the user-global
/// `tool_guidance` directory for overrides, keeping it up to date as override
/// files change.
pub(crate) fn init(fs: Arc<dyn Fs>, cx: &mut App) {
    if cx.has_global::<ToolGuidanceStore>() {
        return;
    }
    let watcher = spawn_watcher(fs, cx);
    cx.set_global(ToolGuidanceStore {
        user_files: BTreeMap::new(),
        _watcher: watcher,
    });
}

fn spawn_watcher(fs: Arc<dyn Fs>, cx: &mut App) -> Task<()> {
    let guidance_dir = paths::tool_guidance_dir().clone();

    cx.spawn(async move |cx| {
        // `events` holds a watcher reference, so registrations outlive this
        // handle; we keep it to register newly discovered subdirectories —
        // directory watches are not recursive on Linux. `FsWatcher` polls for
        // the directory to appear if it doesn't exist yet.
        let (events, watcher) = fs.watch(&guidance_dir, Duration::from_millis(100)).await;
        futures::pin_mut!(events);

        let (mut user_files, mut scanned_dirs) =
            load_user_overrides(fs.as_ref(), &guidance_dir).await;
        loop {
            for dir in &scanned_dirs {
                watcher.add(dir).log_err();
            }
            cx.update(|cx| {
                cx.update_global::<ToolGuidanceStore, _>(|store, _| {
                    store.user_files = user_files.clone();
                });
            });

            if events.next().await.is_none() {
                // Watcher ended; nothing more to do.
                return;
            }
            (user_files, scanned_dirs) = load_user_overrides(fs.as_ref(), &guidance_dir).await;
        }
    })
}

/// Every `*.hbs` file under the guidance directory, named by its relative path
/// without the extension, `/`-separated on every platform (handlebars template
/// syntax cannot contain `\`). Also returns every directory that was scanned,
/// so the watcher can register them. `read_dir_items` recurses, so nested
/// `<tool>/<param>.hbs` files are loaded too.
async fn load_user_overrides(
    fs: &dyn Fs,
    guidance_dir: &Path,
) -> (BTreeMap<String, String>, Vec<PathBuf>) {
    let mut files = BTreeMap::new();
    let mut dirs = Vec::new();
    let Ok(items) = fs::read_dir_items(fs, guidance_dir).await else {
        return (files, dirs);
    };
    for (path, is_dir) in items {
        if is_dir {
            dirs.push(path);
            continue;
        }
        let Ok(relative) = path.strip_prefix(guidance_dir) else {
            continue;
        };
        let Some(name) = relative.to_str().map(|name| name.replace('\\', "/")) else {
            continue;
        };
        let Some(name) = name.strip_suffix(".hbs") else {
            continue;
        };
        if name.is_empty() {
            continue;
        }
        match fs.load(&path).await {
            Ok(content) => {
                files.insert(name.to_string(), content);
            }
            Err(err) => {
                log::warn!("Failed to load tool guidance {}: {err:#}", path.display());
            }
        }
    }
    (files, dirs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema_target(rest: &str) -> String {
        match parse_target(rest) {
            Some(DocTarget::Schema(pointer)) => pointer,
            other => panic!("expected a schema target for `{rest}`, got {other:?}"),
        }
    }

    #[test]
    fn resolves_the_documented_tree() {
        assert_eq!(parse_target("&self"), Some(DocTarget::ToolGuidance));
        assert_eq!(schema_target("path"), "/properties/path");
        assert_eq!(schema_target("edits"), "/properties/edits");
        assert_eq!(schema_target("$edits/items"), "/properties/edits/items");
        assert_eq!(
            schema_target("$edits/$items/old_text"),
            "/properties/edits/items/properties/old_text"
        );
    }

    #[test]
    fn rejects_reserved_and_malformed_paths() {
        // `&self` only extends the tool description, never a schema node.
        assert_eq!(parse_target("$edits/&self"), None);
        // A `$` segment must descend into something.
        assert_eq!(parse_target("$edits"), None);
        // A plain segment must be a file (the final component).
        assert_eq!(parse_target("edits/items"), None);
        // Names containing reserved characters are unsupported.
        assert_eq!(parse_target("weird&name"), None);
        assert_eq!(parse_target("weird$name"), None);
    }

    #[test]
    fn default_files_scaffold_every_parameter() {
        let files: BTreeMap<String, String> = default_tool_guidance_files("edit_file")
            .into_iter()
            .collect();

        // The tool description comes from the embedded default...
        assert!(files.contains_key("edit_file/&self.hbs"));
        // ...and every input-schema node is covered, nested array items too: the
        // embedded default where one exists, a comment-only scaffold otherwise.
        for path in [
            "edit_file/path.hbs",
            "edit_file/edits.hbs",
            "edit_file/$edits/items.hbs",
            "edit_file/$edits/$items/old_text.hbs",
            "edit_file/$edits/$items/new_text.hbs",
        ] {
            let content = files.get(path).unwrap_or_else(|| {
                panic!(
                    "missing `{path}`; got {:?}",
                    files.keys().collect::<Vec<_>>()
                )
            });
            let key = path.strip_suffix(".hbs").expect("a .hbs path");
            if let Some(default) = BUILTIN_GUIDANCE.get(key) {
                assert_eq!(content, default, "`{path}` should be the embedded default");
            } else {
                assert!(
                    content.starts_with("{{!--"),
                    "`{path}` should be a comment-only stub: {content}"
                );
            }
        }
    }

    #[test]
    fn scaffolds_fill_only_nodes_without_defaults() {
        let schema = serde_json::json!({
            "properties": {
                "curated": {},
                "uncurated": {},
            }
        });
        let mut files = BTreeMap::new();
        files.insert(
            "some_tool/curated.hbs".to_string(),
            "Curated guidance.".to_string(),
        );
        collect_parameter_stubs("some_tool", "", &schema, &mut files);
        // An existing entry — e.g. an embedded default — always wins over a scaffold.
        assert_eq!(files["some_tool/curated.hbs"], "Curated guidance.");
        assert!(files["some_tool/uncurated.hbs"].starts_with("{{!--"));
    }

    #[test]
    fn built_in_guidance_files_all_render() {
        let context = RulesTemplateContext {
            available_tools: &[],
            model_name: None,
            date: "1970-01-01",
            is_linux: false,
            is_windows: false,
            is_macos: false,
            sandboxing: false,
        };
        // Registering every file as a partial (including `&self` and `$node`
        // paths) must not fail, and every template must compile and render.
        for (key, source) in BUILTIN_GUIDANCE.iter() {
            agent_settings::render_rules_template(source, &BUILTIN_GUIDANCE, &context)
                .unwrap_or_else(|err| panic!("`{key}` failed to render: {err:#}"));
        }
    }

    /// Scaffolding utility: writes a placeholder `*.hbs` for every built-in
    /// tool and input-schema node into `src/tool_guidance/`, where the files
    /// become embedded defaults on the next build and give developers a
    /// concrete place to add guidance. Existing files are left untouched, so
    /// this is safe to re-run after a new tool is added.
    ///
    /// Skipped by default; run manually with:
    ///
    /// ```sh
    /// cargo test -p agent scaffold_builtin_tool_docs -- --ignored
    /// ```
    #[test]
    #[ignore = "scaffolding utility, not a test"]
    fn scaffold_builtin_tool_docs() {
        let out_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/tool_guidance");
        for tool_name in crate::ALL_TOOL_NAMES {
            for (relative, content) in default_tool_guidance_files(tool_name) {
                let path = out_dir.join(&relative);
                if path.exists() {
                    continue;
                }
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).unwrap_or_else(|err| {
                        panic!("failed to create {}: {err}", parent.display())
                    });
                }
                std::fs::write(&path, content)
                    .unwrap_or_else(|err| panic!("failed to write {relative}: {err}"));
            }
        }
    }

    /// Extraction utility for the contract/guidance tier split: dumps each
    /// built-in tool's current code-authored descriptions into
    /// `src/tool_guidance/<tool>/…`, where the files become embedded built-in
    /// guidance defaults on the next build. Run `scaffold_builtin_tool_docs`
    /// first to create the tree; this overwrites the parameter scaffolds with
    /// the current prose.
    ///
    /// Skipped by default; run manually with:
    ///
    /// ```sh
    /// cargo test -p agent extract_builtin_tool_docs -- --ignored
    /// ```
    ///
    /// The dump is a starting point for curation, not the split itself:
    /// deciding what stays in the code-authored contract versus what moves to
    /// the guidance file is a per-tool editorial pass, and doc text containing
    /// `{{` must be escaped for Handlebars. Until a tool's doc comments are
    /// slimmed to their contract, extracting them verbatim would send the same
    /// text twice (contract description + guidance section). An existing
    /// `&self.hbs` is left untouched, since it holds curated guidance rather
    /// than a raw doc dump.
    #[test]
    #[ignore = "extraction utility, not a test"]
    fn extract_builtin_tool_docs() {
        use language_model::LanguageModelRequestToolInput;

        fn write(path: &Path, text: &str) -> std::io::Result<()> {
            if text.trim().is_empty() {
                return Ok(());
            }
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(path, text)
        }

        fn dump_schema_docs(dir: &Path, schema: &Value) -> std::io::Result<()> {
            if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
                for (name, child) in properties {
                    if let Some(description) = child.get("description").and_then(Value::as_str) {
                        write(&dir.join(format!("{name}.hbs")), description)?;
                    }
                    if child.get("properties").is_some() || child.get("items").is_some() {
                        dump_schema_docs(&dir.join(format!("${name}")), child)?;
                    }
                }
            }
            if let Some(items) = schema.get("items") {
                if let Some(description) = items.get("description").and_then(Value::as_str) {
                    write(&dir.join("items.hbs"), description)?;
                }
                if items.get("properties").is_some() || items.get("items").is_some() {
                    dump_schema_docs(&dir.join("$items"), items)?;
                }
            }
            Ok(())
        }

        let out_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/tool_guidance");
        for tool in crate::tools::built_in_tools() {
            let tool_dir = out_dir.join(&tool.name);
            // Only seed the tool description when there is no guidance yet: an
            // existing `&self.hbs` holds curated guidance that a raw doc dump
            // must not clobber.
            let self_path = tool_dir.join("&self.hbs");
            if !self_path.exists() {
                write(&self_path, &tool.description)
                    .unwrap_or_else(|err| panic!("failed to write {}: {err}", self_path.display()));
            }
            let LanguageModelRequestToolInput::Function { input_schema, .. } = &tool.input else {
                continue;
            };
            dump_schema_docs(&tool_dir, input_schema).unwrap_or_else(|err| {
                panic!("failed to dump schema docs for {}: {err}", tool.name)
            });
        }
    }
}
