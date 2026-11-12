use std::ops::Deref;

use crate::{
    BlockBody, BlockExpansion, BlockRole, BlockStatus, StyledLine, TranscriptBlock,
    TranscriptBlockView,
};

/// A display block whose mutations maintain layout and source invalidation.
/// Identity and collection-wide projection revisions belong to the caller.
///
/// ```compile_fail
/// use phi_tui::{TrackedBlock, TranscriptBlock};
/// let mut block = TrackedBlock::from(TranscriptBlock::text("body"));
/// block.title = "untracked mutation".into();
/// ```
#[derive(Debug, Clone)]
pub struct TrackedBlock {
    block: TranscriptBlock,
    layout_revision: u64,
    content_epoch: u64,
}

impl From<TranscriptBlock> for TrackedBlock {
    fn from(block: TranscriptBlock) -> Self {
        Self {
            block,
            layout_revision: 0,
            content_epoch: 0,
        }
    }
}

impl Deref for TrackedBlock {
    type Target = TranscriptBlock;

    fn deref(&self) -> &Self::Target {
        &self.block
    }
}

impl TrackedBlock {
    pub fn view(&self, id: u64, role: BlockRole, starts_group: bool) -> TranscriptBlockView<'_> {
        TranscriptBlockView {
            id,
            layout_revision: self.layout_revision,
            content_epoch: self.content_epoch,
            role,
            starts_group,
            block: &self.block,
        }
    }

    pub fn set_title(&mut self, title: String) {
        self.block.title = title;
        self.invalidate_layout();
    }

    pub fn set_input(&mut self, input: Option<String>) {
        self.block.input = input;
        self.invalidate_layout();
    }

    pub fn set_status(&mut self, status: BlockStatus) {
        self.block.status = status;
        self.invalidate_layout();
    }

    pub fn set_indent(&mut self, indent: usize) {
        self.block.indent = indent;
        self.invalidate_layout();
    }

    pub fn set_footer(&mut self, footer: Option<StyledLine>) {
        self.block.footer = footer;
        self.invalidate_layout();
    }

    pub fn set_expansion(&mut self, expansion: BlockExpansion) {
        self.block.expansion = expansion;
        self.invalidate_layout();
    }

    pub fn set_expanded(&mut self, expanded: bool) -> bool {
        self.block.set_expanded(expanded)
    }

    pub fn set_body(&mut self, body: BlockBody) {
        self.block.body = body;
        self.content_epoch = self.content_epoch.saturating_add(1);
        self.invalidate_layout();
    }

    pub fn append_text(&mut self, text: &str) {
        if let BlockBody::Text(content) = &mut self.block.body {
            content.append(text);
            self.invalidate_layout();
        } else {
            self.set_body(BlockBody::Text(text.into()));
        }
    }

    fn invalidate_layout(&mut self) {
        self.layout_revision = self.layout_revision.saturating_add(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BlockRole, BlockStatus, ThemeColor, TranscriptRenderRequest, TranscriptTail};

    #[test]
    fn append_after_body_kind_change_replaces_the_incremental_source() {
        let mut block = TrackedBlock::from(TranscriptBlock::output("tool"));
        block.append_text("text");
        let view = block.view(1, BlockRole::Assistant, true);
        assert_eq!(view.content_epoch, 1);
        assert_eq!(view.layout_revision, 1);
        assert!(matches!(&view.block.body, BlockBody::Text(text) if text == "text"));
    }

    #[test]
    fn presentation_changes_invalidate_layout_without_replacing_source() {
        let mut block = TrackedBlock::from(TranscriptBlock::text("body"));
        block.set_title("title".into());
        block.set_input(Some("input".into()));
        block.set_status(BlockStatus::Running);
        block.set_indent(2);
        block.set_footer(Some(StyledLine::new("elapsed", ThemeColor::Dim)));
        let view = block.view(1, BlockRole::Event, true);
        assert_eq!(view.layout_revision, 5);
        assert_eq!(view.content_epoch, 0);
        assert_eq!(view.block.title, "title");
        assert_eq!(view.block.input.as_deref(), Some("input"));
        assert_eq!(view.block.indent, 2);
        assert_eq!(view.block.status, BlockStatus::Running);
        assert_eq!(view.block.footer.as_ref().unwrap().text, "elapsed");
    }

    #[test]
    fn expansion_changes_presentation_but_not_layout_or_source_clocks() {
        let mut block = TrackedBlock::from(TranscriptBlock::output("tool"));
        assert!(block.set_expanded(true));
        assert!(block.set_expanded(false));
        let view = block.view(1, BlockRole::Event, true);
        assert_eq!(view.layout_revision, 0);
        assert_eq!(view.content_epoch, 0);
        assert!(!block.set_expanded(false));
        block.set_expansion(BlockExpansion::Fixed);
        assert!(!block.set_expanded(true));
    }

    #[test]
    fn view_role_and_group_flow_invalidate_retained_layout_without_body_edits() {
        let block = TrackedBlock::from(TranscriptBlock::collapsible_text("prompt"));
        let mut tail = TranscriptTail::default();
        let request = TranscriptRenderRequest {
            width: 40,
            row_budget: 128,
            projection_revision: 0,
            theme_revision: 0,
            expansion_key_hint: None,
            text_expansion_row_limit: None,
        };
        tail.materialize([block.view(3, BlockRole::User, true)].into_iter(), request);
        for (revision, role, starts_group) in [
            (1, BlockRole::Assistant, true),
            (2, BlockRole::Assistant, false),
        ] {
            let request = TranscriptRenderRequest {
                projection_revision: revision,
                ..request
            };
            let actual = tail.materialize([block.view(3, role, starts_group)].into_iter(), request);
            let expected = TranscriptTail::default()
                .materialize([block.view(3, role, starts_group)].into_iter(), request);
            assert_eq!(actual.lines, expected.lines);
        }
    }

    #[test]
    fn existing_tail_reuses_append_and_resets_replaced_markdown() {
        let mut block = TrackedBlock::from(TranscriptBlock::text("old **text**"));
        let mut tail = TranscriptTail::default();
        let request = TranscriptRenderRequest {
            width: 40,
            row_budget: 128,
            projection_revision: 0,
            theme_revision: 0,
            expansion_key_hint: None,
            text_expansion_row_limit: None,
        };
        let first = tail.materialize(
            [block.view(9, BlockRole::Assistant, true)].into_iter(),
            request,
        );
        assert!(first.lines.iter().any(|line| line.text.contains("old")));
        block.append_text(" appended");
        let appended_view = block.view(9, BlockRole::Assistant, true);
        assert_eq!(appended_view.layout_revision, 1);
        assert_eq!(appended_view.content_epoch, 0);
        let appended = tail.materialize(
            [appended_view].into_iter(),
            TranscriptRenderRequest {
                projection_revision: 1,
                ..request
            },
        );
        assert!(
            appended
                .lines
                .iter()
                .any(|line| line.text.contains("appended"))
        );
        block.set_body(BlockBody::Text("new source".into()));
        let replaced_view = block.view(9, BlockRole::Assistant, true);
        assert_eq!(replaced_view.layout_revision, 2);
        assert_eq!(replaced_view.content_epoch, 1);
        let replaced = tail.materialize(
            [replaced_view].into_iter(),
            TranscriptRenderRequest {
                projection_revision: 2,
                ..request
            },
        );
        assert!(
            replaced
                .lines
                .iter()
                .any(|line| line.text.contains("new source"))
        );
        assert!(!replaced.lines.iter().any(|line| line.text.contains("old")));
    }
}
