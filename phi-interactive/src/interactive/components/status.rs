use phi_tui::{RenderContext, StyledLine, ThemeColor};

use super::input::render_queued_submissions;
use crate::interactive::state::UiState;

const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub(in crate::interactive) fn render(
    state: &UiState,
    context: &RenderContext<'_>,
) -> Vec<StyledLine> {
    let width = context.area.width;
    let mut lines = state.tasks.progress_lines(width);
    lines.extend(state.workflows.lines(width));
    if let Some(interaction) = state.interaction_lines(context) {
        lines.extend(interaction);
    } else if let Some(selector) = state.bash_selector_lines(context) {
        lines.extend(selector);
    }
    lines.extend(render_queued_submissions(state, width));
    lines.extend(state.conversation.run_status_view().render(width));
    lines.push(StyledLine::new(String::new(), ThemeColor::Default).fit(width));
    lines
}

#[derive(Debug, Clone, Copy)]
pub(in crate::interactive) struct RunStatusView {
    pub(in crate::interactive) active: bool,
    pub(in crate::interactive) frame: usize,
}

impl RunStatusView {
    pub(in crate::interactive) fn render(&self, width: u16) -> Vec<StyledLine> {
        if !self.active {
            return Vec::new();
        }
        vec![
            StyledLine::new(String::new(), ThemeColor::Default).fit(width),
            StyledLine::segments([
                (
                    SPINNER_FRAMES[self.frame % SPINNER_FRAMES.len()],
                    ThemeColor::Accent,
                ),
                (" ", ThemeColor::Default),
                ("Cooking...", ThemeColor::Muted),
            ])
            .fit(width),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interactive::state::QueuedSubmissionView;
    use phi_sdk::turn::SubmissionDispatch;
    use phi_tui::{NotifyKind, Theme, text::display_width};

    #[test]
    fn status_combines_queue_and_run_without_history_notices() {
        let mut state = UiState::default();
        state.notify(NotifyKind::Info, "history notice");
        let theme = Theme::default();
        let idle = render(&state, &RenderContext::width(80, &theme));
        assert_eq!(idle.len(), 1);
        assert!(idle[0].text.trim().is_empty());
        state.conversation.project_main_run_started();

        for (id, dispatch) in [
            ("steer", SubmissionDispatch::Steer),
            ("follow-up", SubmissionDispatch::FollowUp),
        ] {
            state.queued_submissions.push_back(QueuedSubmissionView {
                submission_id: id.into(),
                prompt: "accepted prompt".into(),
                display: "queued display".into(),
                editor_prompt: "editor draft".into(),
                dispatch,
            });
        }
        for width in 0..=80 {
            let lines = render(&state, &RenderContext::width(width, &theme));
            assert_eq!(lines.len(), 6);
            assert!(
                lines
                    .iter()
                    .all(|line| display_width(&line.text) <= usize::from(width))
            );
            assert!(
                !lines
                    .iter()
                    .any(|line| line.text.contains("history notice"))
            );
            if width == 80 {
                assert_eq!(lines[1].text.trim_end(), "[steer queued] queued display");
                assert_eq!(
                    lines[2].text.trim_end(),
                    "[follow-up queued] queued display"
                );
                assert!(lines[4].text.contains("Cooking..."));
            }
        }
        assert_eq!(state.queued_submissions.len(), 2);
        assert!(state.conversation.blocks.last().unwrap().local_notification);
    }

    #[test]
    fn status_is_hidden_when_idle_and_bounded_when_active() {
        for frame in [0, 1, SPINNER_FRAMES.len(), usize::MAX] {
            for width in 0..=80 {
                let idle = RunStatusView {
                    active: false,
                    frame,
                };
                assert!(idle.render(width).is_empty());
                let active = RunStatusView {
                    active: true,
                    frame,
                };
                let lines = active.render(width);
                assert_eq!(lines.len(), 2);
                assert!(
                    lines
                        .iter()
                        .all(|line| display_width(&line.text) <= usize::from(width))
                );
                if width == 80 {
                    assert_eq!(
                        lines[1].text.trim_end(),
                        format!(
                            "{} Cooking...",
                            SPINNER_FRAMES[frame % SPINNER_FRAMES.len()]
                        )
                    );
                }
            }
        }
    }
}
