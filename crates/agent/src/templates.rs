use agent_skills::SkillSummary;
use anyhow::Result;
use gpui::SharedString;
use handlebars::Handlebars;
use rust_embed::RustEmbed;
use serde::Serialize;
use std::sync::Arc;

#[derive(RustEmbed)]
#[folder = "src/templates"]
#[include = "*.hbs"]
struct Assets;

pub struct Templates(Handlebars<'static>);

impl Templates {
    pub fn new() -> Arc<Self> {
        let mut handlebars = Handlebars::new();
        handlebars.set_strict_mode(true);
        handlebars.register_helper("contains", Box::new(agent_settings::contains_helper));
        handlebars.register_helper("join", Box::new(agent_settings::join_helper));
        handlebars.register_helper("array", Box::new(agent_settings::ArrayHelper));
        handlebars.register_helper("union", Box::new(agent_settings::SetOpHelper::UNION));
        handlebars.register_helper(
            "intersect",
            Box::new(agent_settings::SetOpHelper::INTERSECT),
        );
        handlebars.register_helper("differ", Box::new(agent_settings::SetOpHelper::DIFFER));
        handlebars.register_embed_templates::<Assets>().unwrap();
        Arc::new(Self(handlebars))
    }
}

pub trait Template: Sized {
    const TEMPLATE_NAME: &'static str;

    fn render(&self, templates: &Templates) -> Result<String>
    where
        Self: Serialize + Sized,
    {
        Ok(templates.0.render(Self::TEMPLATE_NAME, self)?)
    }
}

#[derive(Serialize)]
pub struct SystemPromptTemplateContext<'a> {
    #[serde(flatten)]
    pub project: &'a prompt_store::ProjectContext,
    pub available_tools: Vec<SharedString>,
    pub model_name: Option<String>,
    pub date: String,
    /// Contents of the user-global `~/.config/zed/AGENTS.md` file (or the
    /// platform equivalent), if present and non-empty.
    pub user_agents_md: Option<SharedString>,
    /// Whether agent-run terminal commands are wrapped in an OS-level
    /// sandbox for this thread. When `true` — and the `terminal` tool is
    /// in `available_tools` — the rendered prompt describes the sandbox's
    /// read/write/network rules and the per-command flags the model can
    /// request to relax them. Otherwise the prompt omits the sandbox
    /// section entirely.
    pub sandboxing: bool,
    /// Whether the host is Linux. The writable-temp story differs by
    /// platform (Linux exposes an ephemeral `tmpfs` over `/tmp`; other
    /// platforms provide a persistent per-thread `$TMPDIR`), so the sandbox
    /// section describes the right one rather than advertising a `$TMPDIR`
    /// that doesn't behave as stated.
    pub is_linux: bool,
    /// Whether sandboxed terminal commands run through WSL on Windows.
    pub is_windows: bool,
    pub is_macos: bool,
}

impl Template for SystemPromptTemplateContext<'_> {
    const TEMPLATE_NAME: &'static str = "system_prompt.hbs";
}

/// The built-in system prompt template, used both as the default when no
/// `system_prompt.hbs` override exists and as the content materialized when a
/// user first opens the override from the agent menu.
pub const BUILT_IN_SYSTEM_PROMPT: &str = include_str!("templates/system_prompt.hbs");

/// Renders the system prompt, preferring the user's `system_prompt.hbs`
/// override when one is loaded. A failed user template falls back to the
/// built-in template rather than breaking the session, and the failure is
/// reported back to the global so the host application can show it: a render
/// error that only reached the log would be invisible, leaving the session
/// behaving as if no override existed while the UI still reported the file as
/// loaded.
pub fn render_system_prompt(
    context: &SystemPromptTemplateContext,
    templates: &Templates,
    user_template: Option<&agent_settings::SystemPromptTemplate>,
) -> anyhow::Result<String> {
    if let Some(template) = user_template
        && let Some(source) = template.source()
    {
        match render_user_system_prompt(source, context) {
            Ok(rendered) => return Ok(rendered),
            Err(err) => {
                let message = format!("{err:#}");
                log::error!(
                    "Failed to render user system prompt template {}: {message}",
                    paths::system_prompt_template_file().display()
                );
                template.report_render_error(message);
            }
        }
    }
    context.render(templates)
}

fn render_user_system_prompt(
    source: &agent_settings::SystemPromptTemplateSource,
    context: &SystemPromptTemplateContext,
) -> anyhow::Result<String> {
    let mut partials = (*source.partials).clone();
    // Registered last so the real `AGENTS.md` wins over a user partial that
    // happens to use the same stem.
    partials.insert(
        agent_settings::AGENTS_MD_PARTIAL_NAME.to_string(),
        context
            .user_agents_md
            .as_deref()
            .unwrap_or_default()
            .to_string(),
    );
    agent_settings::render_template(source.source.as_ref(), &partials, context)
}

/// A synthetic session context to dry-render a `system_prompt.hbs` override
/// against, so errors that a template only produces while rendering —
/// unknown variables, unknown helpers, helper arity mismatches — are found
/// when the file changes rather than at the start of the next session turn.
/// Reporting a helper error a turn later than a syntax error in the same file
/// is the inconsistency this exists to remove.
///
/// Owns the [`prompt_store::ProjectContext`] that
/// [`SystemPromptTemplateContext`] borrows.
pub struct SystemPromptProbe {
    project: prompt_store::ProjectContext,
    available_tools: Vec<SharedString>,
}

impl SystemPromptProbe {
    /// Every context value that would otherwise be a coin flip is fixed at
    /// its "most content" setting — sandboxing on, a worktree with a rules
    /// file, a skill, an `AGENTS.md` — so a single render reaches as many
    /// gated sections as possible. Sections gated on a *narrower* context
    /// than this one are not rendered, so a clean probe is not proof that
    /// every branch renders; those still reach the user through
    /// [`render_system_prompt`]'s report.
    pub fn new(available_tools: Vec<SharedString>) -> Self {
        let worktrees = vec![prompt_store::WorktreeContext {
            root_name: "my-project".to_string(),
            abs_path: std::path::Path::new("/path/to/my-project").into(),
            rules_file: Some(prompt_store::RulesFileContext {
                path_in_worktree: util::rel_path::RelPath::from_unix_str("AGENTS.md")
                    .unwrap_or(util::rel_path::RelPath::empty())
                    .into(),
                text: "project rules body".to_string(),
                project_entry_id: 0,
            }),
        }];
        let project =
            prompt_store::ProjectContext::new(worktrees).with_skills(vec![SkillSummary {
                name: "example-skill".to_string(),
                description: "An example skill.".to_string(),
                location: "/path/to/skills/example-skill/SKILL.md".to_string(),
            }]);
        Self {
            project,
            available_tools,
        }
    }

    /// Probes with every built-in tool available, since a template can gate a
    /// section on any of them.
    pub fn with_built_in_tools() -> Self {
        Self::new(
            crate::tools::built_in_tools()
                .map(|tool| SharedString::from(tool.name))
                .collect(),
        )
    }

    pub fn context(&self) -> SystemPromptTemplateContext<'_> {
        SystemPromptTemplateContext {
            project: &self.project,
            available_tools: self.available_tools.clone(),
            model_name: Some("Example Model".to_string()),
            date: chrono::Local::now().format("%Y-%m-%d").to_string(),
            user_agents_md: Some("personal rules body".into()),
            sandboxing: true,
            is_linux: cfg!(target_os = "linux"),
            is_windows: cfg!(target_os = "windows"),
            is_macos: cfg!(target_os = "macos"),
        }
    }

    pub fn render(
        &self,
        source: &agent_settings::SystemPromptTemplateSource,
    ) -> anyhow::Result<String> {
        render_user_system_prompt(source, &self.context())
    }
}

/// Dry-renders a candidate `system_prompt.hbs` the way a session would.
///
/// Handed to the `system_prompt.hbs` watcher in `agent_settings`, which can't
/// build a [`SystemPromptTemplateContext`] itself: the context type lives
/// here, in a crate that depends on it.
pub fn probe_user_system_prompt(
    source: &agent_settings::SystemPromptTemplateSource,
) -> anyhow::Result<()> {
    SystemPromptProbe::with_built_in_tools()
        .render(source)
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_system_prompt_template() {
        let project = prompt_store::ProjectContext::default();
        let template = SystemPromptTemplateContext {
            project: &project,
            available_tools: vec!["echo".into()],
            model_name: Some("test-model".to_string()),
            date: "2026-01-01".to_string(),
            user_agents_md: None,
            sandboxing: false,
            is_linux: false,
            is_windows: false,
            is_macos: false,
        };
        let templates = Templates::new();
        let rendered = template.render(&templates).unwrap();
        assert!(rendered.contains("You are the Zed coding agent"));
        assert!(rendered.contains("Today's Date: 2026-01-01"));
        assert!(rendered.contains("## Fixing Diagnostics"));
        assert!(rendered.contains("test-model"));
    }

    #[test]
    fn test_system_prompt_renders_user_agents_md_before_project_rules() {
        use prompt_store::{ProjectContext, RulesFileContext, WorktreeContext};
        use util::rel_path::RelPath;

        let worktrees = vec![WorktreeContext {
            root_name: "my-project".to_string(),
            abs_path: std::path::Path::new("/tmp/my-project").into(),
            rules_file: Some(RulesFileContext {
                path_in_worktree: RelPath::from_unix_str("AGENTS.md").unwrap().into(),
                text: "project-specific guidance".to_string(),
                project_entry_id: 1,
            }),
        }];
        let project = ProjectContext::new(worktrees);
        let template = SystemPromptTemplateContext {
            project: &project,
            available_tools: vec!["echo".into()],
            model_name: Some("test-model".to_string()),
            date: "2026-01-01".to_string(),
            user_agents_md: Some("always be concise".into()),
            sandboxing: false,
            is_linux: false,
            is_windows: false,
            is_macos: false,
        };
        let templates = Templates::new();
        let rendered = template.render(&templates).unwrap();

        assert!(rendered.contains("### Personal `AGENTS.md`"));
        assert!(rendered.contains("always be concise"));
        assert!(rendered.contains("### Project Rules"));
        assert!(rendered.contains("project-specific guidance"));

        let personal_idx = rendered.find("### Personal `AGENTS.md`").unwrap();
        let project_idx = rendered.find("### Project Rules").unwrap();
        assert!(
            personal_idx < project_idx,
            "personal AGENTS.md should render before project rules so project rules can override it"
        );
    }

    #[test]
    fn test_system_prompt_omits_sandbox_section_when_sandboxing_disabled() {
        let project = prompt_store::ProjectContext::default();
        let template = SystemPromptTemplateContext {
            project: &project,
            available_tools: vec!["echo".into()],
            model_name: Some("test-model".to_string()),
            date: "2026-01-01".to_string(),
            user_agents_md: None,
            sandboxing: false,
            is_linux: false,
            is_windows: false,
            is_macos: false,
        };
        let templates = Templates::new();
        let rendered = template.render(&templates).unwrap();
        assert!(!rendered.contains("## Terminal sandbox"));
        assert!(!rendered.contains("allow_hosts"));
    }

    #[test]
    fn test_system_prompt_renders_sandbox_section_with_worktrees_when_enabled() {
        use prompt_store::{ProjectContext, WorktreeContext};

        let worktrees = vec![
            WorktreeContext {
                root_name: "alpha".to_string(),
                abs_path: std::path::Path::new("/tmp/alpha").into(),
                rules_file: None,
            },
            WorktreeContext {
                root_name: "beta".to_string(),
                abs_path: std::path::Path::new("/tmp/beta").into(),
                rules_file: None,
            },
        ];
        let project = ProjectContext::new(worktrees);
        let template = SystemPromptTemplateContext {
            project: &project,
            available_tools: vec!["echo".into(), "terminal".into()],
            model_name: Some("test-model".to_string()),
            date: "2026-01-01".to_string(),
            user_agents_md: None,
            sandboxing: true,
            is_linux: false,
            is_windows: false,
            is_macos: false,
        };
        let templates = Templates::new();
        let rendered = template.render(&templates).unwrap();

        assert!(rendered.contains("## Terminal sandbox"));
        assert!(rendered.contains("`/tmp/alpha`"));
        assert!(rendered.contains("`/tmp/beta`"));
        assert!(rendered.contains("allow_hosts"));
        assert!(rendered.contains("allow_all_hosts: true"));
        assert!(rendered.contains("fs_write_paths"));
        assert!(rendered.contains("allow_fs_write_all: true"));
        assert!(rendered.contains("unsandboxed: true"));
        assert!(rendered.contains("`.git` directories remain protected"));
        assert!(rendered.contains("Git metadata writes are never grantable inside the sandbox"));
        assert!(rendered.contains("request `unsandboxed: true` with a reason"));
        assert!(rendered.contains("git --no-optional-locks status"));
        assert!(rendered.contains("for the rest of the thread"));
        // macOS tolerates granting a not-yet-existing path, so the
        // existing-directory requirement must not be stated there; the
        // `create_directory` flow is the preferred guidance instead.
        assert!(!rendered.contains("Each path must be an existing directory"));
        assert!(rendered.contains("first create it with the `create_directory` tool"));
    }

    #[test]
    fn test_system_prompt_linux_sandbox_section_omits_tmpdir() {
        use prompt_store::{ProjectContext, WorktreeContext};

        let worktrees = vec![WorktreeContext {
            root_name: "alpha".to_string(),
            abs_path: std::path::Path::new("/tmp/alpha").into(),
            rules_file: None,
        }];
        let project = ProjectContext::new(worktrees);
        let template = SystemPromptTemplateContext {
            project: &project,
            available_tools: vec!["echo".into(), "terminal".into()],
            model_name: Some("test-model".to_string()),
            date: "2026-01-01".to_string(),
            user_agents_md: None,
            sandboxing: true,
            is_linux: true,
            is_windows: false,
            is_macos: false,
        };
        let templates = Templates::new();
        let rendered = template.render(&templates).unwrap();

        assert!(rendered.contains("## Terminal sandbox"));
        // On Linux we must not advertise the special persistent `$TMPDIR`.
        assert!(!rendered.contains("$TMPDIR"));
        assert!(rendered.contains("`/tmp` is writable"));
        assert!(rendered.contains("`/tmp/alpha`"));
        // Linux write grants must already exist (bwrap binds existing paths).
        assert!(rendered.contains("Each path must be an existing directory"));
        assert!(rendered.contains("first create it with the `create_directory` tool"));
    }

    #[test]
    fn test_system_prompt_windows_sandbox_section_rejects_host_specific_network() {
        use prompt_store::{ProjectContext, WorktreeContext};

        let worktrees = vec![WorktreeContext {
            root_name: "alpha".to_string(),
            abs_path: std::path::Path::new("C:/Users/me/project").into(),
            rules_file: None,
        }];
        let project = ProjectContext::new(worktrees);
        let template = SystemPromptTemplateContext {
            project: &project,
            available_tools: vec!["echo".into(), "terminal".into()],
            model_name: Some("test-model".to_string()),
            date: "2026-01-01".to_string(),
            user_agents_md: None,
            sandboxing: true,
            is_linux: false,
            is_windows: true,
            is_macos: false,
        };
        let templates = Templates::new();
        let rendered = template.render(&templates).unwrap();

        assert!(rendered.contains("commands run inside WSL under Bubblewrap"));
        assert!(rendered.contains("Protected Git metadata remains read-only"));
        assert!(rendered.contains("do not use this on Windows"));
        assert!(rendered.contains("such requests are rejected"));
        assert!(rendered.contains("allow_all_hosts: true"));
        assert!(rendered.contains("git --no-optional-locks status"));
        // Out-of-project `create_directory` grants aren't supported on Windows,
        // so the prompt must not recommend that flow; it suggests granting the
        // nearest existing parent instead.
        assert!(rendered.contains("Each path must be an existing directory"));
        assert!(rendered.contains("nearest existing parent directory"));
        assert!(!rendered.contains("first create it with the `create_directory` tool"));
    }

    #[test]
    fn test_system_prompt_sandbox_section_handles_zero_worktrees() {
        let project = prompt_store::ProjectContext::default();
        let template = SystemPromptTemplateContext {
            project: &project,
            available_tools: vec!["echo".into(), "terminal".into()],
            model_name: Some("test-model".to_string()),
            date: "2026-01-01".to_string(),
            user_agents_md: None,
            sandboxing: true,
            is_linux: false,
            is_windows: false,
            is_macos: false,
        };
        let templates = Templates::new();
        let rendered = template.render(&templates).unwrap();

        assert!(rendered.contains("## Terminal sandbox"));
        assert!(rendered.contains("No project directories are currently writable"));
    }

    #[test]
    fn test_system_prompt_omits_sandbox_section_when_terminal_tool_unavailable() {
        // A profile can disable the terminal tool entirely; the prompt must not
        // describe a sandboxed `terminal` tool the model doesn't have.
        let project = prompt_store::ProjectContext::default();
        let template = SystemPromptTemplateContext {
            project: &project,
            available_tools: vec!["echo".into()],
            model_name: Some("test-model".to_string()),
            date: "2026-01-01".to_string(),
            user_agents_md: None,
            sandboxing: true,
            is_linux: false,
            is_windows: false,
            is_macos: false,
        };
        let templates = Templates::new();
        let rendered = template.render(&templates).unwrap();

        assert!(!rendered.contains("## Terminal sandbox"));
        assert!(!rendered.contains("allow_hosts"));
    }

    #[test]
    fn test_system_prompt_omits_user_agents_md_section_when_absent() {
        let project = prompt_store::ProjectContext::default();
        let template = SystemPromptTemplateContext {
            project: &project,
            available_tools: vec!["echo".into()],
            model_name: Some("test-model".to_string()),
            date: "2026-01-01".to_string(),
            user_agents_md: None,
            sandboxing: false,
            is_linux: false,
            is_windows: false,
            is_macos: false,
        };
        let templates = Templates::new();
        let rendered = template.render(&templates).unwrap();
        assert!(!rendered.contains("### Personal `AGENTS.md`"));
    }

    #[test]
    fn test_system_prompt_does_not_render_legacy_zed_rules_section() {
        let project = prompt_store::ProjectContext::default();
        let template = SystemPromptTemplateContext {
            project: &project,
            available_tools: vec!["echo".into()],
            model_name: Some("test-model".to_string()),
            date: "2026-01-01".to_string(),
            user_agents_md: None,
            sandboxing: false,
            is_linux: false,
            is_windows: false,
            is_macos: false,
        };
        let templates = Templates::new();
        let rendered = template.render(&templates).unwrap();

        assert!(!rendered.contains("The user has specified the following rules"));
        assert!(!rendered.contains("Rules title:"));
    }

    /// The agent menu materializes [`BUILT_IN_SYSTEM_PROMPT`] into
    /// `system_prompt.hbs` when a user first opens the override, so a probe
    /// context too thin to render it would reject the override the moment it
    /// was created. Rendering it is what keeps the synthetic context honest
    /// about what a session provides.
    #[test]
    fn probe_renders_the_built_in_system_prompt() {
        let source = agent_settings::SystemPromptTemplateSource {
            source: SharedString::from(BUILT_IN_SYSTEM_PROMPT),
            partials: Arc::new(std::collections::BTreeMap::new()),
        };
        probe_user_system_prompt(&source).expect("the built-in prompt should probe cleanly");
    }

    /// A helper typo used to survive every file change and fail only once a
    /// session rendered the template, a turn after a syntax error in the same
    /// file would have been reported. The probe is what pulls it forward to
    /// the change that introduced it.
    #[test]
    fn probe_rejects_a_template_that_only_fails_while_rendering() {
        let source = agent_settings::SystemPromptTemplateSource {
            source: SharedString::from("{{frobnicate available_tools}}"),
            partials: Arc::new(std::collections::BTreeMap::new()),
        };
        let error =
            probe_user_system_prompt(&source).expect_err("an unknown helper should fail the probe");
        assert!(format!("{error:#}").contains("frobnicate"), "{error:#}");
    }

    /// Renders a `system_prompt.hbs` override exactly the way a session does
    /// and prints the result, so a template can be checked from the CLI
    /// without starting Zed. A render error fails the test with the message a
    /// session would otherwise only write to the log before falling back to
    /// the built-in prompt.
    ///
    /// Skipped by default; run manually with:
    ///
    /// ```sh
    /// cargo test -p agent render_system_prompt_template_file -- --ignored --nocapture
    /// ```
    ///
    /// Renders the real `system_prompt.hbs` override path by default. The
    /// inputs can be pointed elsewhere with:
    ///
    /// - `ZED_SYSTEM_PROMPT_TEMPLATE`: the template file to render. Every
    ///   other `*.hbs` file under its directory is registered as a partial,
    ///   as in the config directory.
    /// - `ZED_SYSTEM_PROMPT_TOOLS`: comma-separated `available_tools`
    ///   (default: every built-in tool).
    ///
    /// Context values that would need one environment variable each come
    /// from [`SystemPromptProbe`], the same synthetic context the watcher
    /// dry-renders against — so this prints what the probe checks.
    #[test]
    #[ignore = "rendering utility, not a test"]
    fn render_system_prompt_template_file() {
        use std::collections::BTreeMap;
        use std::path::{Path, PathBuf};

        /// Mirrors `agent_settings`' partial collection: every `*.hbs` file
        /// under `root` except the entrypoint, named by its relative path
        /// without the extension, `/`-separated on every platform.
        fn collect_partials(
            root: &Path,
            dir: &Path,
            entrypoint: &Path,
            partials: &mut BTreeMap<String, String>,
        ) -> anyhow::Result<()> {
            for entry in std::fs::read_dir(dir)? {
                let path = entry?.path();
                if path.is_dir() {
                    collect_partials(root, &path, entrypoint, partials)?;
                    continue;
                }
                if path == entrypoint {
                    continue;
                }
                let Some(name) = path
                    .strip_prefix(root)
                    .ok()
                    .and_then(|relative| relative.to_str())
                    .map(|relative| relative.replace('\\', "/"))
                    .and_then(|relative| {
                        relative.strip_suffix(".hbs").map(ToString::to_string)
                    })
                    .filter(|name| !name.is_empty())
                else {
                    continue;
                };
                partials.insert(name, std::fs::read_to_string(&path)?);
            }
            Ok(())
        }

        let path = std::env::var("ZED_SYSTEM_PROMPT_TEMPLATE")
            .map(PathBuf::from)
            .unwrap_or_else(|_| paths::system_prompt_template_file().clone());
        let path = std::fs::canonicalize(&path)
            .unwrap_or_else(|err| panic!("failed to resolve {}: {err}", path.display()));
        let template = std::fs::read_to_string(&path)
            .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()));
        let directory = path
            .parent()
            .unwrap_or_else(|| panic!("{} should have a parent", path.display()));

        let mut partials = BTreeMap::new();
        collect_partials(directory, directory, &path, &mut partials)
            .unwrap_or_else(|err| panic!("failed to collect partials: {err}"));

        let available_tools: Vec<SharedString> = match std::env::var("ZED_SYSTEM_PROMPT_TOOLS") {
            Ok(tools) => tools
                .split(',')
                .map(str::trim)
                .filter(|tool| !tool.is_empty())
                .map(SharedString::from)
                .collect(),
            Err(_) => crate::tools::built_in_tools()
                .map(|tool| SharedString::from(tool.name))
                .collect(),
        };

        let probe = SystemPromptProbe::new(available_tools.clone());

        eprintln!("template: {}", path.display());
        eprintln!(
            "partials: {}",
            if partials.is_empty() {
                "(none)".to_string()
            } else {
                partials.keys().cloned().collect::<Vec<_>>().join(", ")
            }
        );
        eprintln!("available_tools: {}", available_tools.join(", "));

        let source = agent_settings::SystemPromptTemplateSource {
            source: SharedString::from(template),
            partials: Arc::new(partials),
        };
        match probe.render(&source) {
            Ok(rendered) => {
                eprintln!("rendered {} bytes\n", rendered.len());
                println!("{rendered}");
            }
            Err(err) => panic!(
                "failed to render {}: {err:#}\n\n\
                 A session hitting this error falls back to the built-in \
                 system prompt.",
                path.display()
            ),
        }
    }
}
