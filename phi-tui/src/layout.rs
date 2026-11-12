use crate::{Cursor, Frame, StyledLine};

#[derive(Debug, Clone, Default)]
pub struct CodingFrame {
    pub transcript: Vec<StyledLine>,
    pub status: Vec<StyledLine>,
    pub editor: Vec<StyledLine>,
    pub menu: Vec<StyledLine>,
    pub footer: Vec<StyledLine>,
    pub editor_cursor: Option<Cursor>,
    pub transcript_origin: usize,
    pub menu_cursor: Option<Cursor>,
}

impl CodingFrame {
    pub fn compose(mut self, width: u16) -> Frame {
        self.fit(width);
        let mut lines = Vec::new();
        lines.extend(self.transcript);
        lines.extend(self.status);
        let editor_row = lines.len();
        lines.extend(self.editor);
        let menu_row = lines.len();
        lines.extend(self.menu);
        lines.extend(self.footer);

        let cursor = self
            .menu_cursor
            .map(|cursor| Cursor {
                row: menu_row + cursor.row,
                col: cursor.col,
            })
            .or_else(|| {
                self.editor_cursor.map(|cursor| Cursor {
                    row: editor_row + cursor.row,
                    col: cursor.col,
                })
            });

        Frame {
            lines,
            cursor,
            logical_origin: self.transcript_origin,
        }
    }

    fn fit(&mut self, width: u16) {
        for line in self
            .transcript
            .iter_mut()
            .chain(self.status.iter_mut())
            .chain(self.editor.iter_mut())
            .chain(self.menu.iter_mut())
            .chain(self.footer.iter_mut())
        {
            line.fit_mut(width);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ThemeColor;

    fn line(text: &str) -> StyledLine {
        StyledLine::new(text, ThemeColor::Default)
    }

    #[test]
    fn regions_preserve_local_cursors_and_fit_width() {
        for transcript_rows in 0..=3 {
            for status_rows in 0..=3 {
                for width in 0..=20 {
                    for menu_cursor in [None, Some(Cursor { row: 1, col: 0 })] {
                        let frame = CodingFrame {
                            transcript: vec![line("history"); transcript_rows],
                            status: vec![line("status"); status_rows],
                            editor: vec![line("editor"); 2],
                            menu: vec![line("menu"); 2],
                            footer: vec![line("footer")],
                            editor_cursor: Some(Cursor { row: 1, col: 0 }),
                            menu_cursor,
                            transcript_origin: 42,
                        }
                        .compose(width);
                        let editor_row = transcript_rows + status_rows;
                        let expected_row = editor_row + if menu_cursor.is_some() { 3 } else { 1 };
                        assert_eq!(
                            frame.cursor,
                            Some(Cursor {
                                row: expected_row,
                                col: 0
                            })
                        );
                        assert_eq!(frame.logical_origin, 42);
                        assert_eq!(frame.lines.len(), 5 + transcript_rows + status_rows);
                        assert!(frame.lines.iter().all(|line| {
                            crate::text::display_width(&line.text) <= usize::from(width)
                        }));
                    }
                }
            }
        }
    }

    #[test]
    fn keeps_full_transcript_and_offsets_editor_cursor() {
        let frame = CodingFrame {
            transcript: vec![line("one"), line("two"), line("three")],
            status: vec![line("status")],
            editor: vec![line("editor")],
            editor_cursor: Some(Cursor { row: 0, col: 3 }),
            footer: vec![line("footer")],
            ..Default::default()
        }
        .compose(20);

        assert_eq!(
            frame
                .lines
                .iter()
                .map(|line| line.text.trim_end())
                .collect::<Vec<_>>(),
            vec!["one", "two", "three", "status", "editor", "footer"]
        );
        assert_eq!(frame.cursor, Some(Cursor { row: 4, col: 3 }));
    }
}
