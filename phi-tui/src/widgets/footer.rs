use std::path::Path;
use std::process::Command;

use crate::text::{display_width, truncate};
use crate::{RenderContext, StyledLine, StyledSegment, ThemeColor, Widget};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FooterAlign {
    Left,
    Right,
}

#[derive(Debug, Clone)]
pub struct FooterSection {
    pub text: StyledLine,
    pub align: FooterAlign,
}

#[derive(Debug, Clone)]
pub struct FooterLine {
    pub sections: Vec<FooterSection>,
}

#[derive(Debug, Clone, Default)]
pub struct Footer {
    pub lines: Vec<FooterLine>,
}

impl Widget for Footer {
    type Action = ();

    fn render(&self, context: &RenderContext<'_>) -> Vec<StyledLine> {
        let area = context.area;
        let width = area.width;
        let width = width as usize;
        self.lines
            .iter()
            .map(|line| render_line(line, width))
            .collect()
    }
}

pub fn current_git_branch(path: impl AsRef<Path>) -> Option<String> {
    let output = Command::new("git")
        .args(["-C", path.as_ref().to_str()?, "branch", "--show-current"])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    clean_git_branch_output(&output.stdout)
}

pub fn path_with_git_branch(path: &str, branch: Option<&str>) -> String {
    match branch.filter(|branch| !branch.is_empty()) {
        Some(branch) => format!("{path} ({branch})"),
        None => path.to_string(),
    }
}

pub fn model_with_thinking_level(
    provider: &str,
    model: &str,
    thinking_level: Option<&str>,
) -> String {
    let model_text = format!("({provider}) {model}");
    match thinking_level.filter(|level| !level.is_empty()) {
        Some(level) => format!("{model_text} • {level}"),
        None => model_text,
    }
}

fn clean_git_branch_output(output: &[u8]) -> Option<String> {
    let branch = String::from_utf8_lossy(output).trim().to_string();
    (!branch.is_empty()).then_some(branch)
}

fn render_line(line: &FooterLine, width: usize) -> StyledLine {
    let left = join_sections(line, FooterAlign::Left);
    let right = join_sections(line, FooterAlign::Right);
    let min_padding = 2;
    let left_width = display_width(&truncate(&left.text, width));
    let available_right = width.saturating_sub(left_width + min_padding);
    if right.text.is_empty() || available_right == 0 {
        return left.fit(width as u16);
    }

    let right_width = display_width(&truncate(&right.text, available_right));
    let padding = width - left_width - right_width;
    let mut segments = left.fit(left_width as u16).segments;
    segments.push(StyledSegment::new(" ".repeat(padding), ThemeColor::Dim));
    segments.extend(right.fit(right_width as u16).segments);
    StyledLine::rich_segments(segments).fit(width as u16)
}

fn join_sections(line: &FooterLine, align: FooterAlign) -> StyledLine {
    let mut segments = Vec::new();
    for section in line
        .sections
        .iter()
        .filter(|section| section.align == align && !section.text.text.is_empty())
    {
        if !segments.is_empty() {
            segments.push(StyledSegment::new("  ", ThemeColor::Dim));
        }
        if section.text.segments.is_empty() {
            segments.push(StyledSegment::new(&section.text.text, section.text.fg));
        } else {
            segments.extend(section.text.segments.iter().cloned());
        }
    }
    StyledLine::rich_segments(segments)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncates_right_section_when_space_is_limited() {
        let footer = Footer {
            lines: vec![FooterLine {
                sections: vec![
                    FooterSection {
                        text: StyledLine::new("abcdef", ThemeColor::Dim),
                        align: FooterAlign::Left,
                    },
                    FooterSection {
                        text: StyledLine::new("uvwxyz", ThemeColor::Dim),
                        align: FooterAlign::Right,
                    },
                ],
            }],
        };

        let lines = footer.render(&RenderContext::new(
            crate::RenderArea::width(10),
            &crate::Theme::default(),
        ));

        assert_eq!(lines[0].text, "abcdef  uv");
    }
}
