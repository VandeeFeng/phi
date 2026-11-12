use phi_tui::{
    Footer, FooterAlign, FooterLine, FooterSection, RenderContext, StyledLine, ThemeColor, Widget,
    home_to_tilde, model_with_thinking_level, path_with_git_branch,
};

use super::conversation::UsageView;
use crate::session::Session;

const FOOTER_MIN_HEIGHT: usize = 3;

pub(in crate::interactive) struct SessionFooter<'a> {
    pub(in crate::interactive) session: &'a Session,
    pub(in crate::interactive) git_branch: Option<&'a str>,
    pub(in crate::interactive) usage: &'a UsageView,
    pub(in crate::interactive) compact_enabled: Option<bool>,
    pub(in crate::interactive) thinking_level: Option<&'a str>,
    pub(in crate::interactive) mcp_status: Option<&'a str>,
}

impl Widget for SessionFooter<'_> {
    type Action = ();

    fn render(&self, context: &RenderContext<'_>) -> Vec<StyledLine> {
        let mcp_status = self.mcp_status.filter(|status| !status.trim().is_empty());
        let metrics = footer_metrics(self.session, self.usage, self.compact_enabled);
        let model = model_with_thinking_level(
            self.session.provider.as_str(),
            self.session.model.as_str(),
            self.thinking_level,
        );
        let footer = Footer {
            lines: footer_lines(
                &footer_left(self.session, self.git_branch),
                &metrics,
                &model,
                mcp_status,
            ),
        };
        let mut lines = footer.render(context);
        let height = FOOTER_MIN_HEIGHT + usize::from(mcp_status.is_some());
        lines.resize_with(height, || {
            StyledLine::new(String::new(), ThemeColor::Default).fit(context.area.width)
        });
        lines
    }
}

fn footer_lines(
    path: &str,
    metrics: &StyledLine,
    model: &str,
    mcp_status: Option<&str>,
) -> Vec<FooterLine> {
    let mut lines = vec![
        FooterLine {
            sections: vec![FooterSection {
                text: StyledLine::new(path, ThemeColor::Dim),
                align: FooterAlign::Left,
            }],
        },
        FooterLine {
            sections: vec![
                FooterSection {
                    text: metrics.clone(),
                    align: FooterAlign::Left,
                },
                FooterSection {
                    text: StyledLine::new(model, ThemeColor::Dim),
                    align: FooterAlign::Right,
                },
            ],
        },
    ];

    if let Some(mcp_status) = mcp_status {
        lines.push(FooterLine {
            sections: vec![FooterSection {
                text: StyledLine::new(mcp_status, ThemeColor::Dim),
                align: FooterAlign::Left,
            }],
        });
    }

    lines
}

fn footer_left(session: &Session, git_branch: Option<&str>) -> String {
    let mut pwd = path_with_git_branch(&home_to_tilde(session.working_dir.as_str()), git_branch);
    if !session.name.is_empty() {
        pwd = format!("{pwd} • {}", session.name);
    }
    pwd
}

fn footer_metrics(
    session: &Session,
    usage: &UsageView,
    compact_enabled: Option<bool>,
) -> StyledLine {
    const CONTEXT_WARNING_PERCENT: f64 = 70.0;
    const CONTEXT_ERROR_PERCENT: f64 = 90.0;

    let mut context_color = ThemeColor::Dim;
    let auto = if compact_enabled == Some(true) {
        " (auto)"
    } else {
        ""
    };
    let context = if session.context_window == 0 {
        format!("?/?{auto}")
    } else {
        let percent = session.total_estimated_tokens as f64 * 100.0 / session.context_window as f64;
        context_color = if percent > CONTEXT_ERROR_PERCENT {
            ThemeColor::Error
        } else if percent > CONTEXT_WARNING_PERCENT {
            ThemeColor::Warning
        } else {
            ThemeColor::Dim
        };
        format!(
            "{percent:.1}%/{}{auto}",
            format_tokens(session.context_window)
        )
    };
    let tokens = if usage.input > 0 || usage.output > 0 {
        format!(
            "↑{} ↓{}",
            format_tokens(usage.input),
            format_tokens(usage.output)
        )
    } else {
        String::new()
    };
    let cache_read = if usage.cache_read > 0 {
        format!(" R{}", format_tokens(usage.cache_read))
    } else {
        String::new()
    };
    let subscription = matches!(session.provider.as_str(), "chatgpt" | "openai-codex");
    let cost = if usage.cost > 0.0 || subscription {
        format!(
            " ${:.3}{}",
            usage.cost,
            if subscription { " (sub)" } else { "" }
        )
    } else {
        String::new()
    };
    let metrics = format!("{tokens}{cache_read}{cost}")
        .trim_start()
        .to_string();
    let prefix = if metrics.is_empty() {
        metrics
    } else {
        format!("{metrics} ")
    };
    StyledLine::segments([(prefix, ThemeColor::Dim), (context, context_color)])
}

fn format_tokens(count: u64) -> String {
    if count < 1_000 {
        count.to_string()
    } else if count < 10_000 {
        format!("{:.1}k", count as f64 / 1_000.0)
    } else if count < 1_000_000 {
        format!("{}k", count.saturating_add(500) / 1_000)
    } else if count < 10_000_000 {
        format!("{:.1}M", count as f64 / 1_000_000.0)
    } else {
        format!("{}M", count.saturating_add(500_000) / 1_000_000)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use phi_tui::{Theme, text::display_width};

    #[test]
    fn footer_uses_the_selected_session_directory() {
        let mut session =
            crate::session::new("test", "test", 128_000, &std::env::current_dir().unwrap());
        session.working_dir = "/tmp/phi-selected-workspace".into();
        session.name = "selected".into();
        assert_eq!(
            footer_left(&session, None),
            "/tmp/phi-selected-workspace • selected"
        );
    }

    #[test]
    fn footer_context_colors_follow_usage_thresholds() {
        let mut session =
            crate::session::new("test", "test", 100_000, &std::env::current_dir().unwrap());
        let usage = UsageView {
            input: 1_000,
            ..UsageView::default()
        };
        for (tokens, color) in [
            (0, ThemeColor::Dim),
            (70_000, ThemeColor::Dim),
            (70_001, ThemeColor::Warning),
            (90_000, ThemeColor::Warning),
            (90_001, ThemeColor::Error),
            (110_000, ThemeColor::Error),
        ] {
            session.total_estimated_tokens = tokens;
            let metrics = footer_metrics(&session, &usage, Some(true));
            let footer = Footer {
                lines: footer_lines("path", &metrics, "model", None),
            };
            for width in [16, 80] {
                let lines = footer.render(&RenderContext::width(width, &Theme::default()));
                let segments = &lines[1].segments;
                assert_eq!(segments[0].color(), ThemeColor::Dim);
                assert_eq!(segments[1].color(), color, "tokens={tokens}, width={width}");
                assert!(segments[1].text().contains('%'));
            }
        }
        session.context_window = 0;
        let metrics = footer_metrics(&session, &usage, None);
        assert!(metrics.text.ends_with("?/?"));
        assert!(
            metrics
                .segments
                .iter()
                .all(|segment| segment.color() == ThemeColor::Dim)
        );
    }

    #[test]
    fn footer_auto_label_uses_only_committed_runtime_fact() {
        let session =
            crate::session::new("test", "test", 128_000, &std::env::current_dir().unwrap());
        let usage = UsageView::default();
        assert!(
            footer_metrics(&session, &usage, Some(true))
                .text
                .contains(" (auto)")
        );
        for fact in [Some(false), None] {
            assert!(
                !footer_metrics(&session, &usage, fact)
                    .text
                    .contains(" (auto)")
            );
        }
    }

    #[test]
    fn footer_preserves_layout_across_widths_and_mcp_status() {
        let mut session =
            crate::session::new("test", "test", 128_000, &std::env::current_dir().unwrap());
        session.name = "named session".into();
        let usage = UsageView {
            input: 1_000,
            output: 500,
            cache_read: 200,
            cost: 0.125,
        };
        let theme = Theme::default();
        for width in [0, 1, 16, 80, 160] {
            for mcp_status in [None, Some(""), Some("  "), Some("MCP connected")] {
                let footer = SessionFooter {
                    session: &session,
                    git_branch: None,
                    usage: &usage,
                    compact_enabled: Some(true),
                    thinking_level: Some("high"),
                    mcp_status,
                };
                let lines = footer.render(&RenderContext::width(width, &theme));
                let has_mcp = mcp_status.is_some_and(|status| !status.trim().is_empty());
                assert_eq!(lines.len(), if has_mcp { 4 } else { 3 });
                assert!(
                    lines
                        .iter()
                        .all(|line| display_width(&line.text) <= width as usize)
                );
                assert!(lines.last().unwrap().text.trim().is_empty());
                if width == 160 {
                    assert!(
                        lines[1]
                            .text
                            .contains("↑1.0k ↓500 R200 $0.125 0.0%/128k (auto)")
                    );
                    assert!(lines[1].text.contains("(test) test • high"));
                    if has_mcp {
                        assert!(lines[2].text.starts_with("MCP connected"));
                    }
                }
            }
        }
    }
}
