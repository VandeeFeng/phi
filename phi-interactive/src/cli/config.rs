use phi_tui::{
    CursorPosition, Frame, InputContext, InputOutcome, KeyInput, MountOptions, RenderArea,
    RenderContext, RuntimeEvent, StyledLine, StyledSegment, TerminalGuard, Theme, ThemeColor,
    TuiRenderer, TuiRuntime, Widget,
};

use crate::resources::{Override, Resource, ResourcePolicy, Scope};

#[derive(Clone, Debug, PartialEq, Eq)]
enum ConfigAction {
    Close,
    Toggle,
}

struct ConfigScreen {
    policy: ResourcePolicy,
    resources: Vec<Resource>,
    scope: Scope,
    query: String,
    selected: usize,
    message: Option<String>,
    project_scope: Option<std::path::PathBuf>,
}

impl ConfigScreen {
    fn visible(&self) -> Vec<usize> {
        let query = self.query.to_lowercase();
        let mut visible: Vec<_> = self
            .resources
            .iter()
            .enumerate()
            .filter(|(_, resource)| self.scope == Scope::Project || resource.scope == Scope::Global)
            .filter(|(_, resource)| {
                resource.name().to_lowercase().contains(&query)
                    || resource
                        .path
                        .to_string_lossy()
                        .to_lowercase()
                        .contains(&query)
                    || resource.kind.label().to_lowercase().contains(&query)
            })
            .map(|(index, _)| index)
            .collect();
        visible.sort_by_cached_key(|index| {
            let resource = &self.resources[*index];
            let builtin = resource.is_builtin();
            (
                builtin,
                builtin && resource.kind == crate::resources::ResourceKind::Skills,
            )
        });
        visible
    }

    fn key(&mut self, key: &KeyInput) -> InputOutcome<ConfigAction> {
        match key {
            KeyInput::Esc | KeyInput::Ctrl('c') => {
                return InputOutcome::Action(ConfigAction::Close);
            }
            KeyInput::Tab if self.project_scope.is_some() => {
                self.scope = if self.scope == Scope::Global {
                    Scope::Project
                } else {
                    Scope::Global
                };
                self.selected = 0;
            }
            KeyInput::Up => self.selected = self.selected.saturating_sub(1),
            KeyInput::Down => {
                self.selected = (self.selected + 1).min(self.visible().len().saturating_sub(1))
            }
            KeyInput::Backspace => {
                phi_tui::pop_grapheme(&mut self.query);
                self.selected = 0;
            }
            KeyInput::Char(' ') | KeyInput::Enter => {
                return InputOutcome::Action(ConfigAction::Toggle);
            }
            KeyInput::Char(character) if !character.is_control() => {
                self.query.push(*character);
                self.selected = 0;
            }
            KeyInput::Paste(text) => {
                self.query
                    .extend(text.chars().filter(|character| !character.is_control()));
                self.selected = 0;
            }
            _ => return InputOutcome::Ignored,
        }
        InputOutcome::Consumed
    }

    fn toggle(&mut self) {
        let visible = self.visible();
        let Some(resource) = visible
            .get(self.selected)
            .map(|index| &self.resources[*index])
        else {
            return;
        };
        let state = self.policy.override_for(resource, self.scope);
        let next = if self.scope == Scope::Global
            && state == Override::Inherit
            && !self.policy.globally_enabled(resource)
        {
            Override::Load
        } else {
            let inherited =
                resource.scope == Scope::Project || self.policy.globally_enabled(resource);
            next_override(self.scope, state, inherited)
        };
        self.message = self
            .policy
            .set(resource, self.scope, next)
            .err()
            .map(|error| phi_tui::text::abbreviate_home_paths(&error.to_string()));
    }

    fn render(&self, context: &RenderContext<'_>) -> Vec<StyledLine> {
        let width = context.area.width;
        let height = context.area.height.unwrap_or(24);
        let home_scope = self.project_scope.is_none();
        let (title, hint, file) = match self.scope {
            Scope::Global => (
                "Global Resources",
                "tab switch mode · space toggle · esc close",
                crate::config::settings::settings_file_path(),
            ),
            Scope::Project => (
                "Project Local Resources",
                "tab switch mode · space cycle inherit/+/- · esc close",
                self.project_scope
                    .as_ref()
                    .expect("project scope is required for project config")
                    .join("settings.json"),
            ),
        };
        let spacing = (width as usize)
            .saturating_sub(
                phi_tui::text::display_width(title) + phi_tui::text::display_width(hint),
            )
            .max(1);
        let path = if self.scope == Scope::Project {
            ".phi/settings.json".to_string()
        } else {
            phi_tui::home_to_tilde(&file)
        };
        let mut lines = vec![
            StyledLine::rich_segments([
                StyledSegment::new(title, ThemeColor::Default).with_bold(true),
                StyledSegment::new(" ".repeat(spacing), ThemeColor::Default),
                StyledSegment::new(hint, ThemeColor::Dim),
            ])
            .fit(width),
            StyledLine::new(
                format!(
                    "{path}{}",
                    if self.scope == Scope::Project {
                        " · inherited global resources are dimmed"
                    } else {
                        ""
                    }
                ),
                ThemeColor::Muted,
            )
            .fit(width),
            StyledLine::new("", ThemeColor::Default),
            StyledLine::new(format!("> {}", self.query), ThemeColor::Default).fit(width),
            StyledLine::new("", ThemeColor::Default),
        ];
        if home_scope {
            lines.insert(
                2,
                StyledLine::new(
                    "Home directory does not provide project-level Phi configuration.",
                    ThemeColor::Warning,
                )
                .fit(width),
            );
        }
        let visible = self.visible();
        let selected = self.selected.min(visible.len().saturating_sub(1));
        let window = height.saturating_sub(12 + usize::from(home_scope)).max(1);
        let start = selected
            .saturating_sub(window / 2)
            .min(visible.len().saturating_sub(window));
        let mut last_group = None;
        let mut last_kind = None;
        for (position, index) in visible.iter().enumerate().skip(start).take(window) {
            let resource = &self.resources[*index];
            let group = resource_group(resource);
            if last_group.as_ref() != Some(&group) {
                lines.push(StyledLine::new(format!("  {group}"), ThemeColor::Accent).fit(width));
                last_group = Some(group);
                last_kind = None;
            }
            if last_kind != Some(resource.kind) {
                lines.push(
                    StyledLine::new(format!("    {}", resource.kind.label()), ThemeColor::Muted)
                        .fit(width),
                );
                last_kind = Some(resource.kind);
            }
            lines.push(self.resource_line(resource, position == selected, width));
        }
        if visible.is_empty() {
            lines.push(StyledLine::new("  No resources found", ThemeColor::Muted));
        }
        if let Some(message) = &self.message {
            lines.push(StyledLine::new(message, ThemeColor::Warning).fit(width));
        }
        lines.push(
            StyledLine::new(
                format!(
                    "  ({}/{})",
                    if visible.is_empty() { 0 } else { selected + 1 },
                    visible.len()
                ),
                ThemeColor::Dim,
            )
            .fit(width),
        );
        lines
    }

    fn resource_line(&self, resource: &Resource, selected: bool, width: u16) -> StyledLine {
        let state = self.policy.override_for(resource, Scope::Project);
        let (marker, marker_color) = match (self.scope, state) {
            (Scope::Project, Override::Load) => ("[+]", ThemeColor::Success),
            (Scope::Project, Override::Unload) => ("[-]", ThemeColor::Warning),
            (Scope::Global, _) if self.policy.globally_enabled(resource) => {
                ("[x]", ThemeColor::Success)
            }
            (Scope::Project, _) if self.policy.enabled(resource) => ("[x]", ThemeColor::Success),
            _ => ("[ ]", ThemeColor::Dim),
        };
        let suffix = if self.scope != Scope::Project {
            ""
        } else {
            match state {
                Override::Load => "  project load",
                Override::Unload => "  project unload",
                Override::Inherit if resource.scope == Scope::Global => "  inherited global",
                Override::Inherit => "",
            }
        };
        let unavailable_reason = self.policy.unavailable_reason(resource, self.scope);
        let suffix = match unavailable_reason {
            Some(reason) => format!("{suffix}  unavailable: {reason}"),
            None => suffix.to_string(),
        };
        let name_color = if self.scope == Scope::Project
            && resource.scope == Scope::Global
            && state == Override::Inherit
        {
            ThemeColor::Dim
        } else {
            ThemeColor::Default
        };
        let cursor = if selected { "->" } else { "  " };
        let mut line = StyledLine::rich_segments([
            StyledSegment::new(format!("{cursor}    "), ThemeColor::Default),
            StyledSegment::new(marker, marker_color).with_bold(selected),
            StyledSegment::new(format!(" {}", resource.name()), name_color).with_bold(selected),
            StyledSegment::new(suffix, ThemeColor::Dim).with_bold(selected),
        ])
        .fit(width);
        if unavailable_reason.is_some() {
            line.fg = ThemeColor::Dim;
            line.bold = selected;
            line.segments.clear();
        }
        line
    }
}

impl Widget for ConfigScreen {
    type Action = ConfigAction;

    fn render(&self, context: &RenderContext<'_>) -> Vec<StyledLine> {
        ConfigScreen::render(self, context)
    }

    fn handle_input(&mut self, key: &KeyInput) -> InputOutcome<Self::Action> {
        self.key(key)
    }

    fn cursor(&self, area: RenderArea) -> Option<CursorPosition> {
        Some(CursorPosition {
            row: if self.project_scope.is_none() { 4 } else { 3 },
            col: (2 + phi_tui::text::display_width(&self.query))
                .min(area.width.saturating_sub(1) as usize) as u16,
        })
    }
}

fn resource_group(resource: &Resource) -> String {
    if resource.is_builtin() {
        return "Built-in".to_string();
    }
    let global = crate::config::settings::phi_path();
    if resource.scope == Scope::Global && resource.path.starts_with(&global) {
        let root = phi_tui::home_to_tilde(global);
        return format!("User ({}/)", root.trim_end_matches('/'));
    }
    if resource.scope == Scope::Project
        && crate::config::settings::project_scope_dir()
            .is_some_and(|dir| resource.path.starts_with(dir))
    {
        return "Project (.phi/)".to_string();
    }
    let root = resource.path.parent().unwrap_or(&resource.path);
    let label = if resource.scope == Scope::Global {
        "User"
    } else {
        "Project"
    };
    format!(
        "{label} ({}/)",
        phi_tui::home_to_tilde(root).trim_end_matches('/')
    )
}

fn next_override(scope: Scope, state: Override, inherited: bool) -> Override {
    match (scope, state, inherited) {
        (Scope::Global, Override::Unload, _) => Override::Load,
        (Scope::Global, _, _) => Override::Unload,
        (Scope::Project, Override::Inherit, true) => Override::Unload,
        (Scope::Project, Override::Inherit, false) => Override::Load,
        (Scope::Project, Override::Unload, true) => Override::Load,
        (Scope::Project, Override::Load, false) => Override::Unload,
        _ => Override::Inherit,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mounted_config_emits_write_intent_and_preserves_draft_on_failure() {
        let directory = std::path::PathBuf::from("/tmp")
            .join(format!("phi-config-widget-{}", uuid::Uuid::new_v4()));
        let project_scope = directory.join(".phi");
        let policy = ResourcePolicy::load_for(&directory).unwrap();
        let resource = Resource {
            path: project_scope.join("prompts/test.md"),
            kind: crate::resources::ResourceKind::Prompts,
            scope: Scope::Project,
        };
        let mut runtime = TuiRuntime::new();
        let handle = runtime.mount_widget(
            ConfigScreen {
                policy,
                resources: vec![resource],
                scope: Scope::Project,
                query: "test".into(),
                selected: 0,
                message: None,
                project_scope: Some(project_scope.clone()),
            },
            MountOptions::after_root(InputContext::Picker).focused(),
        );
        assert_eq!(
            runtime.dispatch_key(KeyInput::Enter),
            InputOutcome::Action(ConfigAction::Toggle)
        );
        let settings = project_scope.join("settings.json");
        assert!(!settings.exists());
        std::fs::create_dir_all(&settings).unwrap();
        runtime.update_widget(handle, ConfigScreen::toggle);
        let screen = runtime.widget(handle).unwrap();
        assert!(screen.message.is_some());
        assert_eq!(screen.query, "test");
        assert_eq!(screen.selected, 0);
        assert_eq!(
            screen
                .policy
                .override_for(&screen.resources[0], Scope::Project),
            Override::Inherit
        );
        std::fs::remove_dir(&settings).unwrap();
        runtime.update_widget(handle, ConfigScreen::toggle);
        assert!(settings.is_file());
        assert!(runtime.widget(handle).unwrap().message.is_none());
        let area = RenderArea {
            width: 28,
            height: Some(12),
        };
        let frame = runtime.compose(area, &Frame::default(), &Theme::default());
        assert_eq!(frame.frame.cursor.unwrap().col, 6);
        assert!(
            frame
                .frame
                .lines
                .iter()
                .all(|line| phi_tui::text::display_width(&line.text) <= 28)
        );
        runtime.update_widget(handle, |screen| {
            screen.project_scope = None;
            screen.scope = Scope::Global;
        });
        runtime.dispatch_key(KeyInput::Tab);
        assert_eq!(runtime.widget(handle).unwrap().scope, Scope::Global);
        assert_eq!(runtime.widget(handle).unwrap().cursor(area).unwrap().row, 4);
        assert_eq!(
            runtime.dispatch_key(KeyInput::Esc),
            InputOutcome::Action(ConfigAction::Close)
        );
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn builtin_section_supports_search_and_project_toggles() {
        let directory = std::path::Path::new("/tmp")
            .join(format!("phi-builtin-screen-{}", uuid::Uuid::new_v4()));
        let policy = ResourcePolicy::load_for(&directory).unwrap();
        let resources = policy
            .discover()
            .into_iter()
            .filter(|resource| resource.builtin_extension().is_some())
            .collect();
        let mut screen = ConfigScreen {
            policy,
            resources,
            scope: Scope::Project,
            query: "builtin:mcp".into(),
            selected: 0,
            message: None,
            project_scope: Some(directory.join(".phi")),
        };
        let visible = screen.visible();
        assert_eq!(visible.len(), 1);
        let inherited = screen.policy.enabled(&screen.resources[visible[0]]);
        screen.toggle();
        assert!(screen.message.is_none());
        assert_ne!(
            screen.policy.enabled(&screen.resources[visible[0]]),
            inherited
        );
        let saved: serde_json::Value =
            serde_json::from_slice(&std::fs::read(directory.join(".phi/settings.json")).unwrap())
                .unwrap();
        assert_eq!(
            saved["extensions"],
            serde_json::json!([if inherited {
                "-builtin:mcp"
            } else {
                "+builtin:mcp"
            }])
        );
        let mut runtime = TuiRuntime::new();
        runtime.mount_widget(
            screen,
            MountOptions::after_root(InputContext::Picker).focused(),
        );
        let frame = runtime.compose(
            RenderArea {
                width: 100,
                height: Some(24),
            },
            &Frame::default(),
            &Theme::default(),
        );
        assert!(
            frame
                .frame
                .lines
                .iter()
                .any(|line| line.text.trim() == "Built-in")
        );
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn builtin_section_groups_extensions_and_skills() {
        let directory = std::path::Path::new("/tmp")
            .join(format!("phi-builtin-skills-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let skill = Resource {
            path: "builtin:skills/memory-init/SKILL.md".into(),
            kind: crate::resources::ResourceKind::Skills,
            scope: Scope::Global,
        };
        assert_eq!(resource_group(&skill), "Built-in");
        let user = Resource {
            path: directory.join("user/SKILL.md"),
            ..skill.clone()
        };
        assert_ne!(resource_group(&user), "Built-in");
        let mut runtime = TuiRuntime::new();
        runtime.mount_widget(
            ConfigScreen {
                policy: ResourcePolicy::load_for(&directory).unwrap(),
                resources: vec![
                    skill,
                    user,
                    Resource {
                        path: phi_sdk::extension::BuiltinExtension::Memory.path().into(),
                        kind: crate::resources::ResourceKind::Extensions,
                        scope: Scope::Global,
                    },
                ],
                scope: Scope::Global,
                query: String::new(),
                selected: 0,
                message: None,
                project_scope: Some(directory.join(".phi")),
            },
            MountOptions::after_root(InputContext::Picker).focused(),
        );
        let frame = runtime.compose(
            RenderArea {
                width: 100,
                height: Some(30),
            },
            &Frame::default(),
            &Theme::default(),
        );
        assert_eq!(
            frame
                .frame
                .lines
                .iter()
                .filter(|line| line.text.trim() == "Built-in")
                .count(),
            1
        );
        let headings: Vec<_> = frame
            .frame
            .lines
            .iter()
            .filter(|line| matches!(line.text.trim(), "Built-in" | "Extensions" | "Skills"))
            .map(|line| (line.text.trim(), line.fg))
            .collect();
        assert_eq!(
            headings,
            vec![
                ("Skills", ThemeColor::Muted),
                ("Built-in", ThemeColor::Accent),
                ("Extensions", ThemeColor::Muted),
                ("Skills", ThemeColor::Muted),
            ]
        );
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn unavailable_skills_keep_their_configured_checkmark() {
        let directory =
            std::path::Path::new("/tmp").join(format!("phi-skill-state-{}", uuid::Uuid::new_v4()));
        let global = serde_json::from_value::<crate::config::Settings>(serde_json::json!({
            "memory": { "enabled": true },
            "extensions": ["-builtin:memory"],
            "skills": ["+builtin:skills/memory-init/SKILL.md"]
        }))
        .unwrap();
        let policy =
            ResourcePolicy::from_settings(&directory, global.into(), Default::default()).unwrap();
        let resource = Resource {
            path: "builtin:skills/memory-init/SKILL.md".into(),
            kind: crate::resources::ResourceKind::Skills,
            scope: Scope::Global,
        };
        let mut screen = ConfigScreen {
            policy,
            resources: vec![resource.clone()],
            scope: Scope::Global,
            query: String::new(),
            selected: 0,
            message: None,
            project_scope: Some(directory.join(".phi")),
        };
        let line = screen.resource_line(&resource, true, 120);
        assert!(line.text.contains("[x] memory-init"));
        assert!(line.text.contains("unavailable: memory disabled"));
        assert_eq!(line.fg, ThemeColor::Dim);
        assert!(line.segments.is_empty());
        screen.scope = Scope::Project;
        screen
            .policy
            .set(&resource, Scope::Project, Override::Load)
            .unwrap();
        let line = screen.resource_line(&resource, true, 120);
        assert!(line.text.contains("[+] memory-init"));
        assert!(line.text.contains("unavailable: memory disabled"));
        assert_eq!(line.fg, ThemeColor::Dim);
        assert!(line.segments.is_empty());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn project_cycles_from_inherited_enabled_and_disabled() {
        for (enabled, first, second) in [
            (true, Override::Unload, Override::Load),
            (false, Override::Load, Override::Unload),
        ] {
            assert_eq!(
                next_override(Scope::Project, Override::Inherit, enabled),
                first
            );
            assert_eq!(next_override(Scope::Project, first, enabled), second);
            assert_eq!(
                next_override(Scope::Project, second, enabled),
                Override::Inherit
            );
        }
    }
}

pub fn run(local: bool) -> anyhow::Result<()> {
    if local && crate::config::settings::project_scope_dir().is_none() {
        anyhow::bail!("project configuration is unavailable in the home directory");
    }
    let working_dir = std::env::current_dir()?;
    let settings_manager = crate::config::SettingsManager::load_for(&working_dir)?;
    let policy = (*settings_manager.resource_policy_for(&working_dir)?).clone();
    let resources = policy.discover();
    let screen = ConfigScreen {
        policy,
        resources,
        scope: if local { Scope::Project } else { Scope::Global },
        query: String::new(),
        selected: 0,
        message: None,
        project_scope: crate::config::settings::project_scope_dir(),
    };
    let _guard = TerminalGuard::enter()?;
    let mut renderer = TuiRenderer::new(Theme::default());
    let mut runtime = TuiRuntime::new();
    let handle = runtime.mount_widget(
        screen,
        MountOptions::after_root(InputContext::Picker).focused(),
    );
    loop {
        runtime.render_if_requested(&mut renderer, &Frame::default(), &Theme::default())?;
        let RuntimeEvent::Key(key) =
            runtime.poll_event_with_timeout(super::screen_input_timeout(&runtime))?
        else {
            continue;
        };
        match runtime.dispatch_key(key) {
            InputOutcome::Action(ConfigAction::Close) => break,
            InputOutcome::Action(ConfigAction::Toggle) => {
                runtime.update_widget(handle, ConfigScreen::toggle);
            }
            _ => {}
        }
    }
    renderer.finish()?;
    Ok(())
}
