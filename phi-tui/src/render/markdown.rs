use std::borrow::Cow;
use std::collections::VecDeque;

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, LinkType, Options, Parser, Tag, TagEnd};
use unicode_segmentation::UnicodeSegmentation;

use crate::terminal::hyperlinks_supported;
use crate::text::{display_width, wrap};
use crate::{StyledLine, StyledSegment, ThemeColor};

pub fn render_markdown(text: &str, width: u16, color: ThemeColor) -> Vec<StyledLine> {
    MarkdownRenderer::new(width as usize, color, None, hyperlinks_supported())
        .render(text)
        .lines
}

pub(crate) fn render_markdown_tail(
    text: &str,
    width: u16,
    color: ThemeColor,
    row_budget: usize,
) -> MarkdownTail {
    MarkdownRenderer::new(
        width as usize,
        color,
        Some(row_budget.max(1)),
        hyperlinks_supported(),
    )
    .render(text)
}

#[derive(Debug, Clone, Default)]
pub(crate) struct IncrementalMarkdownState {
    stable_offset: usize,
    stable_rows: usize,
    stable_tail: Vec<StyledLine>,
    row_capacity: usize,
    last_text_len: usize,
    last_row_budget: usize,
    last_tail: MarkdownTail,
}

impl IncrementalMarkdownState {
    pub(crate) fn render_tail(
        &mut self,
        text: &str,
        width: u16,
        color: ThemeColor,
        row_budget: usize,
    ) -> MarkdownTail {
        let row_budget = row_budget.max(1);
        if self.stable_offset > text.len() || row_budget > self.row_capacity {
            *self = Self::default();
        }
        if text.len() == self.last_text_len && row_budget <= self.last_row_budget {
            return retain_markdown_tail(self.last_tail.clone(), row_budget);
        }
        let pending_source = &text[self.stable_offset..];
        let mut pending = render_markdown_tail(pending_source.trim(), width, color, row_budget);
        if pending.reference_sensitive && self.stable_offset > 0 {
            *self = Self::default();
            pending = render_markdown_tail(text.trim(), width, color, row_budget);
        } else if !pending.reference_sensitive
            && let Some(boundary) = pending.last_top_level_block_start
        {
            self.extend_prefix(&pending_source[..boundary], width, color, row_budget);
            self.stable_offset += boundary;
            pending =
                render_markdown_tail(pending_source[boundary..].trim(), width, color, row_budget);
        }
        self.row_capacity = self.row_capacity.max(row_budget);
        let has_mutable_tail = !text[self.stable_offset..].trim().is_empty();
        let tail = self.combine_tail(pending, has_mutable_tail, row_budget);
        self.last_text_len = text.len();
        self.last_row_budget = row_budget;
        self.last_tail = tail.clone();
        tail
    }

    fn extend_prefix(&mut self, text: &str, width: u16, color: ThemeColor, row_budget: usize) {
        let rendered = render_markdown(text.trim(), width, color);
        if !self.stable_tail.is_empty() && !rendered.is_empty() {
            self.stable_tail
                .push(StyledLine::new(String::new(), ThemeColor::Default));
            self.stable_rows += 1;
        }
        self.stable_rows += rendered.len();
        self.stable_tail.extend(rendered);
        retain_last_rows(&mut self.stable_tail, row_budget);
    }

    fn combine_tail(
        &self,
        mut tail: MarkdownTail,
        has_mutable_tail: bool,
        row_budget: usize,
    ) -> MarkdownTail {
        let separator_rows = usize::from(self.stable_rows > 0 && has_mutable_tail);
        let total_rows = self.stable_rows + separator_rows + tail.omitted_rows + tail.lines.len();
        let mut lines = self.stable_tail.clone();
        if separator_rows > 0 {
            lines.push(StyledLine::new(String::new(), ThemeColor::Default));
        }
        lines.append(&mut tail.lines);
        retain_last_rows(&mut lines, row_budget);
        MarkdownTail {
            omitted_rows: total_rows.saturating_sub(lines.len()),
            lines,
            ..MarkdownTail::default()
        }
    }
}

fn retain_last_rows(lines: &mut Vec<StyledLine>, row_budget: usize) {
    let excess = lines.len().saturating_sub(row_budget);
    if excess > 0 {
        lines.drain(..excess);
    }
}

fn retain_markdown_tail(mut tail: MarkdownTail, row_budget: usize) -> MarkdownTail {
    let excess = tail.lines.len().saturating_sub(row_budget);
    if excess > 0 {
        tail.lines.drain(..excess);
        tail.omitted_rows = tail.omitted_rows.saturating_add(excess);
    }
    tail
}

#[derive(Debug, Clone, Default)]
pub(crate) struct MarkdownTail {
    pub lines: Vec<StyledLine>,
    pub omitted_rows: usize,
    last_top_level_block_start: Option<usize>,
    reference_sensitive: bool,
}

#[derive(Clone, Debug)]
struct Run {
    text: String,
    color: ThemeColor,
    bold: bool,
    italic: bool,
    underline: bool,
    strikethrough: bool,
    link_uri: Option<String>,
}

impl Run {
    fn text(text: impl Into<String>, color: ThemeColor) -> Self {
        Self {
            text: text.into(),
            color,
            bold: false,
            italic: false,
            underline: false,
            strikethrough: false,
            link_uri: None,
        }
    }
}

#[derive(Clone, Debug, Default)]
struct InlineStyle {
    bold: bool,
    italic: bool,
    underline: bool,
    strikethrough: bool,
    link: bool,
    link_url: Option<String>,
    link_text: String,
}

#[derive(Clone, Copy, Debug)]
struct ListState {
    next: Option<u64>,
}

#[derive(Default)]
struct TableState {
    rows: Vec<Vec<TableCell>>,
    row: Vec<TableCell>,
    cell: Vec<Run>,
}

#[derive(Default)]
struct TableCell {
    runs: Vec<Run>,
}

impl TableCell {
    fn text(&self) -> String {
        self.runs.iter().map(|run| run.text.as_str()).collect()
    }
}

struct MarkdownRenderer {
    width: usize,
    color: ThemeColor,
    lines: VecDeque<StyledLine>,
    paragraph: Vec<Run>,
    inline: InlineStyle,
    list_stack: Vec<ListState>,
    quote_depth: usize,
    in_code_block: bool,
    code_lang: String,
    pending_heading: Option<HeadingLevel>,
    table: Option<TableState>,
    line_budget: Option<usize>,
    omitted_rows: usize,
    hyperlinks: bool,
}

fn phi_markdown_options() -> Options {
    Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES | Options::ENABLE_TASKLISTS
}

impl MarkdownRenderer {
    fn new(width: usize, color: ThemeColor, line_budget: Option<usize>, hyperlinks: bool) -> Self {
        Self {
            width: width.max(1),
            color,
            lines: VecDeque::new(),
            paragraph: Vec::new(),
            inline: InlineStyle::default(),
            list_stack: Vec::new(),
            quote_depth: 0,
            in_code_block: false,
            code_lang: String::new(),
            pending_heading: None,
            table: None,
            line_budget,
            omitted_rows: 0,
            hyperlinks,
        }
    }

    fn render(&mut self, text: &str) -> MarkdownTail {
        let parser = Parser::new_ext(text, phi_markdown_options());
        let mut reference_sensitive = parser.reference_definitions().iter().next().is_some();
        let mut depth = 0usize;
        let mut block_count = 0usize;
        let mut last_block_start = 0usize;
        for (event, range) in parser.into_offset_iter() {
            reference_sensitive |= self.reference_sensitive_event(&event);
            if depth == 0 && matches!(&event, Event::Start(_) | Event::Rule | Event::Html(_)) {
                block_count += 1;
                last_block_start = range.start;
            }
            match &event {
                Event::Start(_) => depth += 1,
                Event::End(_) => depth = depth.saturating_sub(1),
                _ => {}
            }
            self.handle_event(event);
        }
        self.flush_paragraph(false);
        self.trim_trailing_blank();
        self.enforce_line_budget();
        MarkdownTail {
            lines: std::mem::take(&mut self.lines).into(),
            omitted_rows: self.omitted_rows,
            last_top_level_block_start: (block_count > 1).then_some(last_block_start),
            reference_sensitive,
        }
    }

    fn enforce_line_budget(&mut self) {
        if let Some(line_budget) = self.line_budget {
            while self.lines.len() > line_budget {
                self.lines.pop_front();
                self.omitted_rows += 1;
            }
        }
    }

    fn reference_sensitive_event(&self, event: &Event<'_>) -> bool {
        match event {
            Event::Text(text) => !self.in_code_block && text.contains('['),
            Event::Start(Tag::Link { link_type, .. }) => matches!(
                link_type,
                LinkType::Reference | LinkType::Collapsed | LinkType::Shortcut
            ),
            _ => false,
        }
    }

    fn push_line(&mut self, line: StyledLine) {
        self.lines.push_back(line);
        if self
            .line_budget
            .is_some_and(|line_budget| self.lines.len() > line_budget.saturating_add(1))
        {
            self.lines.pop_front();
            self.omitted_rows += 1;
        }
    }

    fn handle_event(&mut self, event: Event<'_>) {
        match event {
            Event::Start(tag) => self.start_tag(tag),
            Event::End(tag) => self.end_tag(tag),
            Event::Text(text) | Event::Html(text) | Event::InlineHtml(text) => {
                self.push_text(&text)
            }
            Event::Code(text) => self.push_inline_code(&text),
            Event::TaskListMarker(checked) => self.push_text(if checked { "[x] " } else { "[ ] " }),
            Event::SoftBreak => self.push_text(" "),
            Event::HardBreak => self.flush_paragraph(false),
            Event::Rule => self.push_rule(),
            _ => {}
        }
    }

    fn start_tag(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph | Tag::HtmlBlock => {}
            Tag::Heading { level, .. } => self.start_heading(level),
            Tag::Strong => self.inline.bold = true,
            Tag::Emphasis => self.inline.italic = true,
            Tag::Strikethrough => self.inline.strikethrough = true,
            Tag::Link { dest_url, .. } => self.start_link(dest_url.as_ref()),
            Tag::List(start) => self.list_stack.push(ListState { next: start }),
            Tag::Item => self.start_list_item(),
            Tag::BlockQuote(_) => {
                self.flush_paragraph(false);
                self.quote_depth += 1;
            }
            Tag::CodeBlock(kind) => self.start_code_block(kind),
            Tag::Table(_) => {
                self.flush_paragraph(false);
                self.table = Some(TableState::default());
            }
            Tag::TableRow => {
                if let Some(table) = &mut self.table {
                    table.row.clear();
                }
            }
            Tag::TableCell => {
                if let Some(table) = &mut self.table {
                    table.cell.clear();
                }
            }
            Tag::TableHead => {}
            _ => {}
        }
    }

    fn end_tag(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph | TagEnd::HtmlBlock => self.flush_paragraph(true),
            TagEnd::Heading(_) => {
                self.flush_paragraph(true);
                self.pending_heading = None;
            }
            TagEnd::Strong => self.inline.bold = false,
            TagEnd::Emphasis => self.inline.italic = false,
            TagEnd::Strikethrough => self.inline.strikethrough = false,
            TagEnd::Link => self.end_link(),
            TagEnd::List(_) => {
                self.list_stack.pop();
                self.push_blank();
            }
            TagEnd::Item => self.flush_paragraph(false),
            TagEnd::BlockQuote(_) => {
                self.flush_paragraph(false);
                self.quote_depth = self.quote_depth.saturating_sub(1);
                self.push_blank();
            }
            TagEnd::CodeBlock => self.end_code_block(),
            TagEnd::Table => self.end_table(),
            TagEnd::TableRow | TagEnd::TableHead => self.end_table_row(),
            TagEnd::TableCell => self.end_table_cell(),
            _ => {}
        }
    }

    fn push_text(&mut self, text: &str) {
        let normalized = if text.contains('\t') {
            Cow::Owned(text.replace('\t', "   "))
        } else {
            Cow::Borrowed(text)
        };
        self.push_normalized_text(&normalized);
    }

    fn push_normalized_text(&mut self, text: &str) {
        if self.push_table_run(text, self.current_color()) {
            return;
        }
        if self.in_code_block {
            for line in text.split('\n') {
                self.push_line(StyledLine::new(
                    format!("  {line}"),
                    ThemeColor::MdCodeBlock,
                ));
            }
            return;
        }
        if self.inline.link {
            self.inline.link_text.push_str(text);
        }

        let mut run = styled_run(
            text,
            self.current_color(),
            &self.inline,
            self.current_link_uri(),
        );
        run.bold |= self.pending_heading.is_some();
        run.italic |= self.quote_depth > 0;
        run.underline |= matches!(self.pending_heading, Some(HeadingLevel::H1));
        self.paragraph.push(run);
    }

    fn push_inline_code(&mut self, text: &str) {
        if self.push_table_run(text, ThemeColor::MdCode) {
            return;
        }
        if self.inline.link {
            self.inline.link_text.push_str(text);
        }

        let mut run = styled_run(
            text,
            ThemeColor::MdCode,
            &self.inline,
            self.current_link_uri(),
        );
        run.italic |= self.quote_depth > 0;
        self.paragraph.push(run);
    }

    fn push_table_run(&mut self, text: &str, color: ThemeColor) -> bool {
        if self.table.is_none() {
            return false;
        }
        if self.inline.link {
            self.inline.link_text.push_str(text);
        }
        let run = styled_run(text, color, &self.inline, self.current_link_uri());
        if let Some(table) = &mut self.table {
            table.cell.push(run);
        }
        true
    }

    fn current_link_uri(&self) -> Option<String> {
        if self.hyperlinks {
            self.inline.link_url.clone()
        } else {
            None
        }
    }

    fn current_color(&self) -> ThemeColor {
        if self.pending_heading.is_some() {
            ThemeColor::MdHeading
        } else if self.inline.link {
            ThemeColor::MdLink
        } else if self.quote_depth > 0 {
            ThemeColor::MdQuote
        } else {
            self.color
        }
    }

    fn start_link(&mut self, dest_url: &str) {
        self.inline.link = true;
        self.inline.underline = true;
        self.inline.link_url = Some(dest_url.to_string());
        self.inline.link_text.clear();
    }

    fn end_link(&mut self) {
        self.inline.link = false;
        self.inline.underline = false;
        if let Some(url) = self.inline.link_url.take()
            && !self.hyperlinks
        {
            let href = url.strip_prefix("mailto:").unwrap_or(&url);
            if self.inline.link_text != url && self.inline.link_text != href {
                let suffix = Run::text(format!(" ({url})"), ThemeColor::MdLinkUrl);
                if let Some(table) = &mut self.table {
                    table.cell.push(suffix);
                } else {
                    self.paragraph.push(suffix);
                }
            }
        }
        self.inline.link_text.clear();
    }

    fn start_heading(&mut self, level: HeadingLevel) {
        self.flush_paragraph(false);
        self.pending_heading = Some(level);
        if matches!(
            level,
            HeadingLevel::H3 | HeadingLevel::H4 | HeadingLevel::H5 | HeadingLevel::H6
        ) {
            let mut run = Run::text(
                format!("{} ", "#".repeat(level as usize)),
                ThemeColor::MdHeading,
            );
            run.bold = true;
            self.paragraph.push(run);
        }
    }

    fn start_list_item(&mut self) {
        let depth = self.list_stack.len().saturating_sub(1);
        let prefix = match self
            .list_stack
            .last_mut()
            .and_then(|list| list.next.as_mut())
        {
            Some(next) => {
                let prefix = format!("{next}. ");
                *next += 1;
                prefix
            }
            None => "- ".to_string(),
        };
        self.paragraph.push(Run::text(
            format!("{}{}", "  ".repeat(depth), prefix),
            ThemeColor::MdListBullet,
        ));
    }

    fn start_code_block(&mut self, kind: CodeBlockKind<'_>) {
        self.flush_paragraph(false);
        self.in_code_block = true;
        self.code_lang = match kind {
            CodeBlockKind::Fenced(lang) => lang.to_string(),
            CodeBlockKind::Indented => String::new(),
        };
        self.push_line(StyledLine::new(
            format!("```{}", self.code_lang),
            ThemeColor::MdCodeBlockBorder,
        ));
    }

    fn end_code_block(&mut self) {
        self.in_code_block = false;
        if self.lines.back().is_some_and(|line| line.text == "  ") {
            self.lines.pop_back();
        }
        self.push_line(StyledLine::new("```", ThemeColor::MdCodeBlockBorder));
        self.push_blank();
    }

    fn end_table_cell(&mut self) {
        if let Some(table) = &mut self.table {
            table.row.push(TableCell {
                runs: trim_table_cell_runs(std::mem::take(&mut table.cell)),
            });
        }
    }

    fn end_table_row(&mut self) {
        if let Some(table) = &mut self.table
            && !table.row.is_empty()
        {
            table.rows.push(std::mem::take(&mut table.row));
        }
    }

    fn end_table(&mut self) {
        if let Some(table) = self.table.take() {
            self.render_table(table.rows);
            self.push_blank();
        }
    }

    fn render_table(&mut self, rows: Vec<Vec<TableCell>>) {
        if rows.is_empty() {
            return;
        }
        let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
        let widths = table_widths(&rows, columns, self.width);
        self.push_line(StyledLine::new(
            table_border('┌', '┬', '┐', &widths),
            ThemeColor::MdCodeBlockBorder,
        ));
        for (row_index, row) in rows.iter().enumerate() {
            for line in table_row_lines(row, &widths, self.color) {
                self.push_line(line);
            }
            let is_header = row_index == 0;
            let is_last = row_index + 1 == rows.len();
            if is_header && !is_last {
                self.push_line(StyledLine::new(
                    table_border('├', '┼', '┤', &widths),
                    ThemeColor::MdCodeBlockBorder,
                ));
            }
        }
        self.push_line(StyledLine::new(
            table_border('└', '┴', '┘', &widths),
            ThemeColor::MdCodeBlockBorder,
        ));
    }

    fn flush_paragraph(&mut self, spacing: bool) {
        if self.paragraph.iter().all(|run| run.text.trim().is_empty()) {
            self.paragraph.clear();
            if spacing {
                self.push_blank();
            }
            return;
        }

        let prefix = self.quote_prefix();
        let continuation = self.list_continuation_indent();
        let plain = self
            .paragraph
            .iter()
            .map(|run| run.text.as_str())
            .collect::<String>();
        let text = plain.trim_end();
        let wrap_width = self.width.saturating_sub(display_width(&prefix)).max(1);
        let mut cursor = 0;
        for (index, line) in wrap(text, wrap_width).into_iter().enumerate() {
            let start = text[cursor..]
                .find(&line)
                .map(|offset| cursor + offset)
                .unwrap_or(cursor);
            let end = start + line.len();
            let line_prefix = if index == 0 {
                prefix.clone()
            } else {
                format!("{prefix}{continuation}")
            };
            self.push_line(self.styled_wrapped_line(&line_prefix, &line, start..end));
            cursor = end;
        }
        self.paragraph.clear();
        if spacing {
            self.push_blank();
        }
    }

    fn styled_wrapped_line(
        &self,
        prefix: &str,
        line: &str,
        range: std::ops::Range<usize>,
    ) -> StyledLine {
        let styled = styled_run_range(&self.paragraph, range, self.color);
        let mut styled = if styled.text.is_empty() {
            StyledLine::new(line.to_string(), self.color)
        } else {
            styled
        };
        if !prefix.is_empty() {
            styled = styled.prepend_segment(prefix, ThemeColor::MdQuoteBorder);
        }
        styled
    }

    fn push_rule(&mut self) {
        self.flush_paragraph(false);
        self.push_line(StyledLine::new(
            "─".repeat(self.width.min(80)),
            ThemeColor::MdHr,
        ));
        self.push_blank();
    }

    fn push_blank(&mut self) {
        if !self
            .lines
            .back()
            .is_some_and(|line| line.text.trim().is_empty())
        {
            self.push_line(StyledLine::new(String::new(), ThemeColor::Default));
        }
    }

    fn trim_trailing_blank(&mut self) {
        while self
            .lines
            .back()
            .is_some_and(|line| line.text.trim().is_empty())
        {
            self.lines.pop_back();
        }
    }

    fn quote_prefix(&self) -> String {
        if self.quote_depth == 0 {
            String::new()
        } else {
            format!("{}│ ", "  ".repeat(self.quote_depth.saturating_sub(1)))
        }
    }

    fn list_continuation_indent(&self) -> String {
        self.paragraph
            .first()
            .filter(|run| run.color == ThemeColor::MdListBullet)
            .filter(|run| is_list_marker(&run.text))
            .map(|run| " ".repeat(display_width(&run.text)))
            .unwrap_or_default()
    }
}

fn is_list_marker(text: &str) -> bool {
    let trimmed = text.trim_start();
    trimmed == "- "
        || trimmed
            .strip_suffix(". ")
            .is_some_and(|number| number.chars().all(|c| c.is_ascii_digit()))
}

fn table_widths(rows: &[Vec<TableCell>], columns: usize, max_width: usize) -> Vec<usize> {
    let border_width = columns.saturating_mul(3) + 1;
    let available = max_width.saturating_sub(border_width).max(columns);
    let mut widths = vec![1; columns];
    for row in rows {
        for (index, cell) in row.iter().enumerate() {
            widths[index] = widths[index].max(display_width(&cell.text()));
        }
    }
    shrink_widths(widths, available)
}

fn shrink_widths(mut widths: Vec<usize>, available: usize) -> Vec<usize> {
    while widths.iter().sum::<usize>() > available {
        if let Some((index, width)) = widths.iter().enumerate().max_by_key(|(_, width)| *width) {
            if *width <= 1 {
                break;
            }
            widths[index] -= 1;
        }
    }
    widths
}

fn table_border(left: char, join: char, right: char, widths: &[usize]) -> String {
    format!(
        "{left}─{}─{right}",
        widths
            .iter()
            .map(|width| "─".repeat(*width))
            .collect::<Vec<_>>()
            .join(&format!("─{join}─"))
    )
}

fn table_row_lines(row: &[TableCell], widths: &[usize], cell_color: ThemeColor) -> Vec<StyledLine> {
    let cells = widths
        .iter()
        .enumerate()
        .map(|(index, width)| {
            row.get(index)
                .map(|cell| table_cell_lines(cell, *width, cell_color))
                .unwrap_or_else(|| {
                    vec![StyledLine::rich_segments([StyledSegment::new(
                        " ".repeat(*width),
                        cell_color,
                    )])]
                })
        })
        .collect::<Vec<_>>();
    let height = cells.iter().map(Vec::len).max().unwrap_or(1);
    (0..height)
        .map(|line_index| table_row_line(&cells, widths, line_index, cell_color))
        .collect()
}

fn table_cell_lines(cell: &TableCell, width: usize, cell_color: ThemeColor) -> Vec<StyledLine> {
    let text = cell.text();
    let mut cursor = 0;
    wrap(&text, width)
        .into_iter()
        .map(|line| {
            let start = text[cursor..]
                .find(&line)
                .map(|offset| cursor + offset)
                .unwrap_or(cursor);
            let end = start + line.len();
            cursor = end;
            let mut styled = styled_run_range(&cell.runs, start..end, cell_color);
            let padding = " ".repeat(width.saturating_sub(display_width(&styled.text)));
            if !padding.is_empty() {
                styled.text.push_str(&padding);
                styled
                    .segments
                    .push(StyledSegment::new(padding, cell_color));
            }
            styled
        })
        .collect()
}

fn table_row_line(
    cells: &[Vec<StyledLine>],
    widths: &[usize],
    line_index: usize,
    cell_color: ThemeColor,
) -> StyledLine {
    let mut segments = vec![StyledSegment::new("│ ", ThemeColor::MdCodeBlockBorder)];
    for (index, lines) in cells.iter().enumerate() {
        let line = lines.get(line_index);
        if let Some(line) = line {
            segments.extend(line.segments.iter().cloned());
        } else {
            segments.push(StyledSegment::new(" ".repeat(widths[index]), cell_color));
        }
        let border = if index + 1 == cells.len() {
            " │"
        } else {
            " │ "
        };
        segments.push(StyledSegment::new(border, ThemeColor::MdCodeBlockBorder));
    }
    StyledLine::rich_segments(segments)
}

fn styled_run(
    text: impl Into<String>,
    color: ThemeColor,
    inline: &InlineStyle,
    link_uri: Option<String>,
) -> Run {
    let mut run = Run::text(text, color);
    run.bold = inline.bold;
    run.italic = inline.italic;
    run.underline = inline.underline;
    run.strikethrough = inline.strikethrough;
    run.link_uri = link_uri;
    run
}

fn styled_run_range(
    runs: &[Run],
    range: std::ops::Range<usize>,
    default_color: ThemeColor,
) -> StyledLine {
    let mut offset = 0;
    let segments = runs
        .iter()
        .filter_map(|run| {
            let run_start = offset;
            offset += run.text.len();
            let text = slice_run_for_range(&run.text, run_start, &range);
            (!text.is_empty()).then(|| {
                StyledSegment::new(text, run.color)
                    .with_bold(run.bold)
                    .with_italic(run.italic)
                    .with_underline(run.underline)
                    .with_strikethrough(run.strikethrough)
                    .with_link_uri(run.link_uri.clone())
            })
        })
        .collect::<Vec<_>>();
    if segments.is_empty() {
        StyledLine::new("", default_color)
    } else {
        StyledLine::rich_segments(segments)
    }
}

fn trim_table_cell_runs(mut runs: Vec<Run>) -> Vec<Run> {
    while runs.last().is_some_and(|run| run.text.trim().is_empty()) {
        runs.pop();
    }
    let leading_empty = runs
        .iter()
        .position(|run| !run.text.trim().is_empty())
        .unwrap_or(0);
    runs.drain(..leading_empty);

    if let Some(first) = runs.first_mut() {
        let whitespace = first.text.len() - first.text.trim_start().len();
        first.text.drain(..whitespace);
    }
    if let Some(last) = runs.last_mut() {
        last.text.truncate(last.text.trim_end().len());
    }
    runs
}

fn slice_run_for_range(text: &str, run_start: usize, range: &std::ops::Range<usize>) -> String {
    let start = range.start.saturating_sub(run_start).min(text.len());
    let end = range.end.saturating_sub(run_start).min(text.len());
    text.get(start..end)
        .map(str::to_string)
        .unwrap_or_else(|| slice_run_graphemes(text, run_start, range))
}

fn slice_run_graphemes(text: &str, run_start: usize, range: &std::ops::Range<usize>) -> String {
    text.grapheme_indices(true)
        .filter_map(|(index, grapheme)| {
            let start = run_start + index;
            let end = start + grapheme.len();
            (start >= range.start && end <= range.end).then_some(grapheme)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_hyperlinks_across_wrapped_text_and_tables() {
        let url = "https://example.com/full/destination";
        let wrapped = MarkdownRenderer::new(5, ThemeColor::Default, None, true)
            .render(&format!("[click here]({url})"))
            .lines;
        let table = MarkdownRenderer::new(40, ThemeColor::Default, None, true)
            .render(&format!("| Name |\n| --- |\n| [click]({url}) |"))
            .lines;

        assert_eq!(line_texts(&wrapped), ["click", "here"]);
        assert!(!line_texts(&table).join("\n").contains(url));
        for lines in [&wrapped, &table] {
            assert!(
                lines
                    .iter()
                    .flat_map(|line| &line.segments)
                    .filter(|segment| segment.text().contains("click") || segment.text() == "here")
                    .all(|segment| segment.link_uri() == Some(url))
            );
        }
    }

    #[test]
    fn renders_visible_destinations_without_hyperlink_support() {
        let url = "https://example.com/destination";
        for source in [
            format!("[click]({url})"),
            format!("| Name |\n| --- |\n| [click]({url}) |"),
        ] {
            let lines = MarkdownRenderer::new(60, ThemeColor::Default, None, false)
                .render(&source)
                .lines;

            assert!(
                line_texts(&lines)
                    .join("\n")
                    .contains(&format!("click ({url})"))
            );
            assert!(
                lines
                    .iter()
                    .flat_map(|line| &line.segments)
                    .all(|segment| segment.link_uri().is_none())
            );
        }
    }

    #[test]
    fn renders_table_box() {
        let lines = render_markdown(
            "| A | B |\n| - | - |\n| one | two |",
            80,
            ThemeColor::Default,
        );

        assert_eq!(lines[0].text, "┌─────┬─────┐");
        assert_eq!(lines[1].text, "│ A   │ B   │");
        assert_eq!(lines[2].text, "├─────┼─────┤");
        assert_eq!(lines[3].text, "│ one │ two │");
        assert_eq!(lines[4].text, "└─────┴─────┘");
    }

    #[test]
    fn indents_wrapped_list_continuation() {
        let lines = render_markdown("- hello world", 9, ThemeColor::Default);

        assert_eq!(lines[0].text, "- hello");
        assert_eq!(lines[1].text, "  world");
    }

    #[test]
    fn preserves_styles_across_unicode_wrapping() {
        let lines = render_markdown("plain **éééé** tail", 8, ThemeColor::Default);

        assert_eq!(line_texts(&lines), ["plain", "éééé", "tail"]);
        assert!(lines[1].segments.iter().all(StyledSegment::is_bold));
    }

    #[test]
    fn incremental_subset_matches_full_rendering() {
        assert_streamed_matches_full(&[
            "intro",
            "intro\n\n# heading",
            "intro\n\n# heading\n\n- one\n- two",
            "intro\n\n# heading\n\n- one\n- two\n\n> quote",
            "intro\n\n# heading\n\n- one\n- two\n\n> quote\n\n```rust\nlet x = 1;\n```",
            "intro\n\n# heading\n\n- one\n- two\n\n> quote\n\n```rust\nlet x = 1;\n```\n\n| A | B |\n| - | - |\n| 1 | 2 |",
        ]);
    }

    #[test]
    fn bounded_incremental_subset_matches_full_tail() {
        let mut state = IncrementalMarkdownState::default();
        let mut source = String::new();
        for index in 0..20 {
            if !source.is_empty() {
                source.push_str("\n\n");
            }
            source.push_str(&format!("paragraph {index}"));
            let streamed = state.render_tail(&source, 40, ThemeColor::Default, 5);
            let full = render_markdown_tail(&source, 40, ThemeColor::Default, 5);
            assert_eq!(line_texts(&streamed.lines), line_texts(&full.lines));
            assert_eq!(streamed.omitted_rows, full.omitted_rows);
        }
    }

    #[test]
    fn brackets_inside_fences_are_not_reference_sensitive() {
        let text = "```rust\nvalue[index]\n```\n\nafter";
        let rendered = render_markdown_tail(text, 40, ThemeColor::Default, 20);

        assert!(!rendered.reference_sensitive);
        assert_eq!(
            rendered.last_top_level_block_start,
            Some(text.find("after").unwrap())
        );
    }

    #[test]
    fn reference_links_reset_prior_stable_sections() {
        let mut state = IncrementalMarkdownState::default();
        let first = "intro\n\nnext";
        let second = "intro\n\nnext\n\nread [the docs][docs]";
        let final_source = "intro\n\nnext\n\nread [the docs][docs]\n\n[docs]: https://example.com";

        state.render_tail(first, 80, ThemeColor::Default, 40);
        assert!(state.stable_offset > 0);
        for source in [second, final_source] {
            let streamed = state.render_tail(source, 80, ThemeColor::Default, 40);
            let full = render_markdown_tail(source, 80, ThemeColor::Default, 40);
            assert_eq!(line_texts(&streamed.lines), line_texts(&full.lines));
            assert_eq!(state.stable_offset, 0);
        }
    }

    #[test]
    fn phi_markdown_enables_only_rendered_extensions() {
        let options = phi_markdown_options();

        assert!(options.contains(Options::ENABLE_STRIKETHROUGH));
        assert!(options.contains(Options::ENABLE_TABLES));
        assert!(options.contains(Options::ENABLE_TASKLISTS));
        assert!(!options.contains(Options::ENABLE_FOOTNOTES));
    }

    fn assert_streamed_matches_full(snapshots: &[&str]) {
        let mut state = IncrementalMarkdownState::default();
        for source in snapshots {
            let streamed = state.render_tail(source, 80, ThemeColor::Default, 200);
            let full = render_markdown_tail(source, 80, ThemeColor::Default, 200);
            assert_eq!(line_texts(&streamed.lines), line_texts(&full.lines));
            assert_eq!(streamed.omitted_rows, full.omitted_rows);
        }
    }

    fn line_texts(lines: &[StyledLine]) -> Vec<&str> {
        lines.iter().map(|line| line.text.as_str()).collect()
    }
}
