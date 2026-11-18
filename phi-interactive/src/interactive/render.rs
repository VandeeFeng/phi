use phi_tui::{
    CodingFrame, RenderArea, RenderContext, StyledLine, Theme, ThemeColor, TranscriptRenderRequest,
    Widget,
};

use super::agent::SubagentProjection;
use super::components::conversation::{ConversationSource, ConversationView};
use super::components::footer::SessionFooter;
use super::components::input::{render_editor, render_menu};
use super::components::status;
use super::state::UiState;
use crate::runtime::attachment::AgentAttachment;
use crate::session::Session;

pub(super) async fn render_current_target(
    renderer: &mut phi_tui::TuiRenderer,
    subagents: &mut SubagentProjection,
    state: &mut UiState,
    session: &Session,
    attachment: &AgentAttachment,
) -> anyhow::Result<()> {
    if state.features.extensions {
        let _ = attachment.extension_ui_rendered().await;
    }
    render_frame(renderer, subagents, state, session)
}

pub(super) fn render_frame(
    renderer: &mut phi_tui::TuiRenderer,
    subagents: &mut SubagentProjection,
    state: &mut UiState,
    session: &Session,
) -> anyhow::Result<()> {
    let area = state.runtime.render_area()?;
    let theme = Theme::default();
    let mut target = subagents.render_target(state, session);
    let built = build_ui_frame(&mut target, area, &theme);
    target.paint(built, |runtime, built| {
        renderer.set_history(built.history);
        if built.replace_history {
            renderer.invalidate_history();
        }
        runtime.render_owned_frame(renderer, area, built.frame, &theme)
    })
}

pub(super) struct BuiltUiFrame {
    source: ConversationSource,
    pub(super) frame: phi_tui::Frame,
    pub(super) retire_before: Option<u64>,
    pub(super) first_block: Option<u64>,
    pub(super) history_revision: u64,
    pub(super) replace_history: bool,
    pub(super) history: phi_tui::HistorySnapshot,
}

pub(super) enum RenderTarget<'a> {
    Main {
        state: &'a mut UiState,
        session: &'a Session,
    },
    Subagent {
        conversation: &'a mut ConversationView,
        session: &'a Session,
        compact_enabled: Option<bool>,
        terminal: &'a mut UiState,
    },
}

impl RenderTarget<'_> {
    #[cfg(test)]
    pub(super) fn session(&self) -> &Session {
        match self {
            Self::Main { session, .. } | Self::Subagent { session, .. } => session,
        }
    }

    fn terminal(&mut self) -> &mut UiState {
        match self {
            Self::Main { state, .. } => state,
            Self::Subagent { terminal, .. } => terminal,
        }
    }

    pub(super) fn paint(
        &mut self,
        built: BuiltUiFrame,
        paint: impl FnOnce(
            &mut phi_tui::TuiRuntime<super::components::input::UiAction>,
            BuiltUiFrame,
        ) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.conversation().source() == &built.source,
            "prepared frame belongs to another conversation"
        );
        let source = built.source.clone();
        let (revision, first, retire) = (
            built.history_revision,
            built.first_block,
            built.retire_before,
        );
        paint(&mut self.terminal().runtime, built)?;
        self.conversation()
            .acknowledge_history_render(revision, first, retire);
        self.terminal().painted_conversation = Some(source);
        Ok(())
    }

    pub(super) fn conversation(&mut self) -> &mut ConversationView {
        match self {
            Self::Main { state, .. } => &mut state.conversation,
            Self::Subagent { conversation, .. } => conversation,
        }
    }
}

pub(super) fn build_ui_frame(
    target: &mut RenderTarget<'_>,
    area: RenderArea,
    theme: &Theme,
) -> BuiltUiFrame {
    let mut built = match target {
        RenderTarget::Main { state, session } => build_main_ui_frame(state, session, area, theme),
        RenderTarget::Subagent {
            conversation,
            session,
            compact_enabled,
            terminal,
        } => build_child_ui_frame(
            conversation,
            session,
            *compact_enabled,
            area,
            theme,
            super::workspace::git_branch(terminal, session),
            terminal
                .bash_selector_lines(&RenderContext::new(area, theme))
                .unwrap_or_default(),
        ),
    };
    // The first paint must preserve pre-existing terminal content and scrollback.
    // Only switching an already painted conversation requires history replacement.
    built.replace_history |= target
        .terminal()
        .painted_conversation
        .as_ref()
        .is_some_and(|source| source != &built.source);
    built
}

fn build_main_ui_frame(
    state: &mut UiState,
    session: &Session,
    area: RenderArea,
    theme: &Theme,
) -> BuiltUiFrame {
    let width = area.width;
    let (editor_lines, editor_cursor) =
        render_editor(state, &session.current_switch.to_string(), width, theme);
    let status = status::render(state, &RenderContext::new(area, theme));
    let (menu, menu_cursor) = render_menu(state, width, theme);
    let footer_lines = SessionFooter {
        session,
        git_branch: super::workspace::git_branch(state, session),
        usage: &state.conversation.usage,
        compact_enabled: state.compact_enabled,
        thinking_level: state.active_thinking_level.as_deref(),
        mcp_status: state.mcp_status.as_deref(),
    }
    .render(&RenderContext::width(width, theme));

    finish_ui_frame(
        &mut state.conversation,
        CodingFrame {
            transcript: Vec::new(),
            status,
            editor: editor_lines,
            menu,
            footer: footer_lines,
            editor_cursor,
            menu_cursor,
            transcript_origin: 0,
        },
        area,
    )
}

fn build_child_ui_frame(
    conversation: &mut ConversationView,
    session: &Session,
    compact_enabled: Option<bool>,
    area: RenderArea,
    theme: &Theme,
    git_branch: Option<&str>,
    mut inline_status: Vec<StyledLine>,
) -> BuiltUiFrame {
    let width = area.width;
    let footer_lines = SessionFooter {
        session,
        git_branch,
        usage: &conversation.usage,
        compact_enabled,
        thinking_level: None,
        mcp_status: None,
    }
    .render(&RenderContext::width(width, theme));
    inline_status.extend(conversation.run_status_view().render(width));
    inline_status.push(StyledLine::new(String::new(), ThemeColor::Default).fit(width));
    finish_ui_frame(
        conversation,
        CodingFrame {
            transcript: Vec::new(),
            status: inline_status,
            editor: Vec::new(),
            menu: Vec::new(),
            footer: footer_lines,
            editor_cursor: None,
            menu_cursor: None,
            transcript_origin: 0,
        },
        area,
    )
}

fn finish_ui_frame(
    state: &mut ConversationView,
    mut coding_frame: CodingFrame,
    area: RenderArea,
) -> BuiltUiFrame {
    let width = area.width;
    let row_budget = paint_tail_rows(area.height);
    let transcript = state.materialize(TranscriptRenderRequest {
        width,
        row_budget,
        projection_revision: state.projection_revision,
        theme_revision: 0,
        expansion_key_hint: Some("ctrl+o"),
        text_expansion_row_limit: state
            .blocks_expanded
            .is_none()
            .then(|| area.height.unwrap_or(24).saturating_div(3).max(1)),
    });

    let retire_before = transcript.retire_before;
    coding_frame.transcript = transcript.lines;
    coding_frame.transcript_origin = transcript.logical_origin;
    let frame = coding_frame.compose(width);
    BuiltUiFrame {
        source: state.source().clone(),
        frame,
        retire_before,
        first_block: transcript.first_block,
        history_revision: transcript.history_revision,
        replace_history: transcript.history_revision != state.painted_history_revision,
        history: transcript.history,
    }
}

fn paint_tail_rows(height: Option<usize>) -> usize {
    const MIN_PAINT_ROWS: usize = 128;
    const MAX_PAINT_ROWS: usize = 1_024;
    const PAINT_OVERSCAN: usize = 3;
    height
        .unwrap_or(MIN_PAINT_ROWS)
        .saturating_mul(PAINT_OVERSCAN)
        .clamp(MIN_PAINT_ROWS, MAX_PAINT_ROWS)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn history_frame(state: &mut UiState, height: usize) -> BuiltUiFrame {
        let session = crate::session::new("test", "test", 0, &std::env::current_dir().unwrap());
        build_ui_frame(
            &mut RenderTarget::Main {
                state,
                session: &session,
            },
            RenderArea {
                width: 80,
                height: Some(height),
            },
            &Theme::default(),
        )
    }

    #[test]
    fn agent_mode_hint_is_dim_inline_without_moving_cursor() {
        for input in ["@reviewer ", "@审查 ", "@a-very-long-reviewer-name "] {
            let mut state = UiState::default();
            state.editor.set_text(input);
            let session = crate::session::new("test", "test", 0, &std::env::current_dir().unwrap());
            for width in [80, 18] {
                let baseline = build_main_ui_frame(
                    &mut state,
                    &session,
                    RenderArea {
                        width,
                        height: Some(24),
                    },
                    &Theme::default(),
                );
                state.file_mentions.suggest_mode(input);
                let built = build_main_ui_frame(
                    &mut state,
                    &session,
                    RenderArea {
                        width,
                        height: Some(24),
                    },
                    &Theme::default(),
                );
                assert_eq!(built.frame.cursor, baseline.frame.cursor);
                let line = built
                    .frame
                    .lines
                    .iter()
                    .find(|line| {
                        line.segments
                            .iter()
                            .any(|segment| segment.text().starts_with('['))
                    })
                    .unwrap();
                assert!(line.segments.iter().any(|segment| {
                    segment.text().starts_with('[') && segment.color() == ThemeColor::Dim
                }));
                if width == 80 {
                    assert_eq!(line.text.trim_end(), format!("{input}[-f | -b]"));
                }
                assert_eq!(state.editor.text(), input);
                assert!(render_menu(&state, width, &Theme::default()).0.is_empty());
                state.file_mentions.close();
            }
        }
    }

    #[test]
    fn source_switches_replace_history_and_failed_paints_do_not_acknowledge() {
        let session = crate::session::new("test", "test", 0, &std::env::current_dir().unwrap());
        let mut terminal = UiState::default();
        assert!(!history_frame(&mut terminal, 24).replace_history);
        let mut child = ConversationView::default();
        for view in [&mut terminal.conversation, &mut child] {
            view.transcript_tail
                .set_history_budget(phi_tui::HistoryBudget {
                    terminal_bytes: 10_000,
                    ..Default::default()
                });
            for index in 0..20 {
                view.push_text_block(phi_tui::BlockRole::Assistant, format!("group {index}"));
            }
            view.push_image_block(
                phi_tui::BlockRole::Assistant,
                phi_tui::ImageBlock::fallback("image"),
            );
        }
        let build = |target: &mut RenderTarget<'_>| {
            build_ui_frame(
                target,
                RenderArea {
                    width: 80,
                    height: Some(24),
                },
                &Theme::default(),
            )
        };
        let unpainted_blocks = child.blocks.len();
        let first_child_block = child.blocks[0].id;
        let (main_revision, foreign_frame) = {
            let mut target = RenderTarget::Main {
                state: &mut terminal,
                session: &session,
            };
            let built = build(&mut target);
            assert!(!built.replace_history);
            let revision = built.history_revision;
            target.paint(built, |_, _| Ok(())).unwrap();
            let built = build(&mut target);
            assert!(!built.replace_history);
            (revision, built)
        };
        {
            let mut target = RenderTarget::Subagent {
                conversation: &mut child,
                session: &session,
                compact_enabled: None,
                terminal: &mut terminal,
            };
            assert!(
                target
                    .paint(foreign_frame, |_, _| panic!("wrong-source paint"))
                    .is_err()
            );
            let built = build(&mut target);
            assert_eq!(built.history_revision, main_revision);
            assert!(built.replace_history);
            assert!(built.retire_before.is_some_and(|id| id > first_child_block));
            assert!(
                target
                    .paint(built, |_, _| anyhow::bail!("paint failed"))
                    .is_err()
            );
            assert!(!target.conversation().buffer_initialized);
            assert_eq!(target.conversation().painted_history_revision, 0);
            assert_eq!(target.conversation().blocks.len(), unpainted_blocks);
            let retry = build(&mut target);
            assert!(retry.replace_history);
            target.paint(retry, |_, _| Ok(())).unwrap();
            assert!(target.conversation().blocks.len() < unpainted_blocks);
            assert!(!build(&mut target).replace_history);
        }
        assert_eq!(
            terminal.conversation.painted_history_revision,
            main_revision
        );
        {
            let mut target = RenderTarget::Main {
                state: &mut terminal,
                session: &session,
            };
            let returned = build(&mut target);
            assert!(returned.replace_history);
            target.paint(returned, |_, _| Ok(())).unwrap();
            assert!(!build(&mut target).replace_history);
        }
        child = ConversationView::default();
        child.push_image_block(
            phi_tui::BlockRole::Assistant,
            phi_tui::ImageBlock::fallback("image"),
        );
        let mut target = RenderTarget::Subagent {
            conversation: &mut child,
            session: &session,
            compact_enabled: None,
            terminal: &mut terminal,
        };
        assert!(build(&mut target).replace_history);
    }

    #[test]
    fn resume_notice_stays_at_history_tail_after_idle_recovery() {
        let mut session = crate::session::new("test", "test", 0, &std::env::current_dir().unwrap());
        for index in 0..100 {
            session
                .transcript
                .append_user_message(&format!("saved prompt {index}"));
        }
        let mut state = UiState::default();
        state.rebuild_history(&session);
        state
            .conversation
            .transcript_tail
            .set_history_budget(phi_tui::HistoryBudget {
                terminal_bytes: 10_000,
                ..Default::default()
            });
        state.notify(phi_tui::NotifyKind::Info, "resumed session");
        for _ in 0..3 {
            let built = history_frame(&mut state, 24);
            assert_eq!(
                built
                    .frame
                    .lines
                    .iter()
                    .filter(|line| line.text.contains("resumed session"))
                    .count(),
                1
            );
            RenderTarget::Main {
                state: &mut state,
                session: &session,
            }
            .paint(built, |_, _| Ok(()))
            .unwrap();
            assert!(state.conversation.blocks.len() < 100);
            state.recover_history(&session);
            assert!(state.conversation.blocks.last().unwrap().local_notification);
        }
        session.transcript.append_user_message("next prompt");
        state.recover_history(&session);
        let blocks = &state.conversation.blocks;
        assert!(blocks[blocks.len() - 2].local_notification);
        assert!(!blocks.last().unwrap().local_notification);
    }

    #[test]
    fn child_frame_uses_only_conversation_and_keeps_main_editor_chrome_out() {
        let mut session = crate::session::new("test", "test", 0, &std::env::current_dir().unwrap());
        session.transcript.append_user_message("child prompt");
        let mut conversation = ConversationView::child_from_session(&session, None);
        conversation.project_main_run_started();
        let mut terminal = UiState::default();
        terminal.compact_enabled = Some(true);
        let built = build_ui_frame(
            &mut RenderTarget::Subagent {
                conversation: &mut conversation,
                session: &session,
                compact_enabled: None,
                terminal: &mut terminal,
            },
            RenderArea {
                width: 80,
                height: Some(24),
            },
            &Theme::default(),
        );
        assert!(built.frame.cursor.is_none());
        assert!(
            !built
                .frame
                .lines
                .iter()
                .any(|line| line.text.contains(" (auto)"))
        );
        assert!(
            built
                .frame
                .lines
                .iter()
                .any(|line| line.text.contains("child prompt"))
        );
        assert!(
            built
                .frame
                .lines
                .iter()
                .any(|line| line.text.contains("Cooking..."))
        );
        assert!(
            built
                .frame
                .lines
                .iter()
                .any(|line| line.text.contains("test"))
        );
    }

    #[test]
    fn history_window_survives_resize_and_repeated_expansion() {
        let mut state = UiState::default();
        state
            .conversation
            .transcript_tail
            .set_history_budget(phi_tui::HistoryBudget {
                terminal_bytes: 400_000,
                ..phi_tui::HistoryBudget::default()
            });
        let output = (0..10)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        for _ in 0..250 {
            state.conversation.push_agent_result_block(
                "tool",
                &output,
                phi_tui::BlockStatus::Success,
            );
        }
        let initial = history_frame(&mut state, 24);
        let first = initial.retire_before.expect("retained history");
        state.conversation.retire_transcript_before(first);
        state.conversation.buffer_first_block = Some(first);
        assert!(state.conversation.blocks.len() < 250);
        let resized = history_frame(&mut state, 60);
        assert_eq!(resized.retire_before, Some(first));
        assert_eq!(resized.history_revision, initial.history_revision);
        for _ in 0..4 {
            assert!(state.conversation.toggle_all_collapsible_blocks());
            let built = history_frame(&mut state, 24);
            assert_eq!(built.retire_before, Some(first));
            assert!(built.replace_history);
            RenderTarget::Main {
                state: &mut state,
                session: &crate::session::new("test", "test", 0, &std::env::current_dir().unwrap()),
            }
            .paint(built, |_, _| Ok(()))
            .unwrap();
            assert!(!history_frame(&mut state, 24).replace_history);
        }
        assert_eq!(
            history_frame(&mut state, 24).frame.lines,
            initial.frame.lines
        );
    }

    #[test]
    fn subagent_trace_folds_below_agent_tool_with_aligned_headers() {
        let mut state = UiState::default();
        let tool = state
            .conversation
            .project_tool_block("Agent", r#"{"role":"probe"}"#, None);
        state
            .conversation
            .blocks
            .iter_mut()
            .find(|block| block.id == tool)
            .unwrap()
            .tool_call_id = Some("agent-call".into());
        state
            .conversation
            .push_text_block(phi_tui::BlockRole::Assistant, "main output");
        let parent = state
            .conversation
            .project_subagent_container(Some("agent-call"), "[agent] probe".into());
        state.conversation.project_subagent_status(
            parent,
            "[agent] probe",
            "taskId: task\nworkspace: /tmp/phi",
            phi_tui::BlockStatus::Success,
        );
        state
            .conversation
            .project_subagent_thinking(parent, true, true);
        assert_ne!(parent, tool);
        assert_eq!(state.conversation.blocks[0].id, tool);
        assert_eq!(state.conversation.blocks[1].id, parent);
        state.conversation.project_subagent_tool_started(
            parent,
            "read",
            "Read",
            &serde_json::json!({"path": "AGENTS.md"}),
        );
        state
            .conversation
            .project_subagent_tool_finished(parent, "read", false);
        let initial = history_frame(&mut state, 24).frame.lines;
        assert!(
            initial
                .iter()
                .any(|line| line.text.contains("[agent] probe"))
        );
        assert!(!initial.iter().any(|line| line.text.contains("Read")
            || line.text.contains("taskId")
            || line.text.contains("workspace")));
        assert!(initial.iter().any(|line| line.text.contains("main output")));
        assert!(initial.iter().any(|line| {
            line.text
                .trim_end()
                .ends_with("[agent] probe (ctrl+o to expand)")
        }));
        assert!(
            !initial
                .iter()
                .any(|line| line.text.contains("lines") || line.text.contains("Thinking"))
        );
        assert!(state.conversation.toggle_all_collapsible_blocks());
        let expanded = history_frame(&mut state, 24).frame.lines;
        let tool = expanded
            .iter()
            .find(|line| line.text.contains("Read {AGENTS.md}"))
            .unwrap();
        let metadata = expanded
            .iter()
            .find(|line| line.text.contains("taskId: task"))
            .unwrap();
        assert_eq!(metadata.text.trim(), "└╴ taskId: task");
        let workspace = expanded
            .iter()
            .find(|line| line.text.contains("workspace: /tmp/phi"))
            .unwrap();
        assert_eq!(workspace.text.trim_end(), "      workspace: /tmp/phi");
        assert!(!tool.text.contains("├─"));
        let thinking = expanded
            .iter()
            .find(|line| line.text.contains("∴ Thinking…"))
            .unwrap();
        assert!(!thinking.text.contains("ctrl+o"));
        assert!(
            state
                .conversation
                .project_subagent_thinking(parent, false, false)
        );
        let completed = history_frame(&mut state, 24).frame.lines;
        assert!(
            completed
                .iter()
                .any(|line| line.text.contains("∴ Thinking") && !line.text.contains("Thinking…"))
        );
        assert!(state.conversation.toggle_all_collapsible_blocks());
        assert_eq!(initial, history_frame(&mut state, 24).frame.lines);
    }

    #[test]
    fn subagent_tool_headers_wrap_without_losing_command_text() {
        let mut state = UiState::default();
        let parent = state
            .conversation
            .project_subagent_container(None, "[agent] probe".into());
        let command = format!("{}\nprintf final-command-marker", "pwd; ".repeat(60));
        state.conversation.project_subagent_tool_started(
            parent,
            "bash",
            "Bash",
            &serde_json::json!({"command": command}),
        );
        assert!(state.conversation.toggle_all_collapsible_blocks());
        let lines = history_frame(&mut state, 24).frame.lines;
        assert!(
            lines
                .iter()
                .any(|line| line.text.contains("final-command-marker}"))
        );
        assert!(
            lines
                .iter()
                .filter(|line| line.text.contains("pwd;"))
                .count()
                > 1
        );
        assert!(
            lines
                .iter()
                .all(|line| phi_tui::text::display_width(&line.text) <= 80)
        );
    }

    #[test]
    fn user_prompt_folds_above_one_third_of_terminal_height() {
        let mut state = UiState::default();
        let prompt = (0..9)
            .map(|line| format!("prompt line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        state
            .conversation
            .push_text_block(phi_tui::BlockRole::User, prompt.as_str());

        let collapsed = history_frame(&mut state, 24);
        assert!(
            collapsed
                .frame
                .lines
                .iter()
                .any(|line| line.text.contains("└╴ +6 lines"))
        );
        assert!(
            collapsed
                .frame
                .lines
                .iter()
                .any(|line| line.text.contains("prompt line 2"))
        );
        assert!(
            !collapsed
                .frame
                .lines
                .iter()
                .any(|line| line.text.contains("prompt line 3"))
        );

        let expanded = history_frame(&mut state, 30);
        assert!(
            expanded
                .frame
                .lines
                .iter()
                .any(|line| line.text.contains("prompt line 8"))
        );

        state.conversation.blocks_expanded = Some(true);
        let overridden = history_frame(&mut state, 24);
        assert!(
            overridden
                .frame
                .lines
                .iter()
                .any(|line| line.text.contains("prompt line 8"))
        );
    }
}
