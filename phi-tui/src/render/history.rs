use std::sync::Arc;

use unicode_segmentation::UnicodeSegmentation;

use crate::StyledLine;

const GHOSTTY_REFERENCE_BYTES: usize = 10_000_000;
const HISTORY_CAPACITY_PERCENT: usize = 80;
// Ghostty v1.3.0 cells/rows are u64; the other costs are conservative reservations.
const CELL_BYTES: usize = 8;
const ROW_BYTES: usize = 8;
const STYLE_ALLOWANCE_BYTES: usize = 64;
const GRAPHEME_METADATA_BYTES: usize = 32;
const CODEPOINT_BYTES: usize = 4;
const GRAPHEME_CHUNK_CODEPOINTS: usize = 4;
const ALLOCATION_ALLOWANCE_FACTOR: usize = 2;
const EVICTION_RESERVE_PERCENT: usize = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HistoryBudget {
    pub terminal_bytes: usize,
    pub resident_bytes: usize,
    pub image_bytes: usize,
}

impl Default for HistoryBudget {
    fn default() -> Self {
        Self {
            terminal_bytes: GHOSTTY_REFERENCE_BYTES * HISTORY_CAPACITY_PERCENT / 100,
            resident_bytes: 8 * 1024 * 1024,
            image_bytes: 8 * 1024 * 1024,
        }
    }
}

impl HistoryBudget {
    pub(crate) fn fits(self, footprint: HistoryFootprint) -> bool {
        footprint.terminal_bytes <= self.terminal_bytes
            && footprint.resident_bytes <= self.resident_bytes
            && footprint.image_bytes <= self.image_bytes
    }

    pub(crate) fn eviction_target(self) -> Self {
        let retain = |bytes: usize| bytes.saturating_sub(bytes / 100 * EVICTION_RESERVE_PERCENT);
        Self {
            terminal_bytes: retain(self.terminal_bytes),
            resident_bytes: retain(self.resident_bytes),
            image_bytes: retain(self.image_bytes),
        }
    }

    pub(crate) fn measurement_rows(self, width: u16) -> usize {
        self.terminal_bytes / minimum_row_bytes(width).max(1)
    }
}

/// Reservations for the retained window, not measurements of terminal or process RSS.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HistoryFootprint {
    pub terminal_bytes: usize,
    pub resident_bytes: usize,
    pub image_bytes: usize,
}

impl HistoryFootprint {
    pub(crate) fn add(self, other: Self) -> Self {
        Self {
            terminal_bytes: self.terminal_bytes.saturating_add(other.terminal_bytes),
            resident_bytes: self.resident_bytes.saturating_add(other.resident_bytes),
            image_bytes: self.image_bytes.saturating_add(other.image_bytes),
        }
    }

    pub(crate) fn subtract(self, other: Self) -> Self {
        Self {
            terminal_bytes: self.terminal_bytes.saturating_sub(other.terminal_bytes),
            resident_bytes: self.resident_bytes.saturating_sub(other.resident_bytes),
            image_bytes: self.image_bytes.saturating_sub(other.image_bytes),
        }
    }

    pub(crate) fn maximum(self, other: Self) -> Self {
        Self {
            terminal_bytes: self.terminal_bytes.max(other.terminal_bytes),
            resident_bytes: self.resident_bytes.max(other.resident_bytes),
            image_bytes: self.image_bytes.max(other.image_bytes),
        }
    }
}

/// Immutable retained history; cloning a frame shares payloads rather than copying them.
#[derive(Debug, Clone, Default)]
pub struct HistorySnapshot {
    chunks: Arc<[Arc<[StyledLine]>]>,
    rows: usize,
    image_ids: Arc<[u32]>,
}

impl HistorySnapshot {
    pub(crate) fn new(chunks: Vec<Arc<[StyledLine]>>, image_ids: Vec<u32>) -> Self {
        let rows = chunks.iter().map(|chunk| chunk.len()).sum();
        Self {
            chunks: chunks.into(),
            rows,
            image_ids: image_ids.into(),
        }
    }

    pub(crate) fn image_ids(&self) -> &[u32] {
        &self.image_ids
    }

    pub(crate) fn chunks(&self) -> &[Arc<[StyledLine]>] {
        &self.chunks
    }

    pub(crate) fn rows(&self) -> usize {
        self.rows
    }

    pub(crate) fn tail(&self, row_budget: usize) -> Vec<StyledLine> {
        let mut lines = self
            .chunks
            .iter()
            .rev()
            .flat_map(|chunk| chunk.iter().rev())
            .take(row_budget)
            .cloned()
            .collect::<Vec<_>>();
        lines.reverse();
        lines
    }
}

pub(crate) fn minimum_row_bytes(width: u16) -> usize {
    (usize::from(width) * CELL_BYTES + ROW_BYTES) * ALLOCATION_ALLOWANCE_FACTOR
}

pub(crate) fn line_footprint(line: &StyledLine, width: u16) -> HistoryFootprint {
    let styles = line
        .segments
        .len()
        .max(1)
        .saturating_mul(STYLE_ALLOWANCE_BYTES);
    let graphemes = line.text.graphemes(true).map(grapheme_bytes).sum::<usize>();
    let (rows, image_bytes) = image_footprint(line, width);
    let terminal_bytes = minimum_row_bytes(width)
        .saturating_mul(rows)
        .saturating_add((styles + graphemes).saturating_mul(ALLOCATION_ALLOWANCE_FACTOR));
    HistoryFootprint {
        terminal_bytes,
        resident_bytes: resident_line_bytes(line),
        image_bytes,
    }
}

fn grapheme_bytes(grapheme: &str) -> usize {
    let codepoints = grapheme.chars().count();
    if codepoints <= 1 {
        return 0;
    }
    codepoints.div_ceil(GRAPHEME_CHUNK_CODEPOINTS) * GRAPHEME_CHUNK_CODEPOINTS * CODEPOINT_BYTES
        + GRAPHEME_METADATA_BYTES
}

fn image_footprint(line: &StyledLine, width: u16) -> (usize, usize) {
    let Some(image) = &line.image else {
        return (1, 0);
    };
    let available = width
        .saturating_sub(crate::text::display_width(&line.text) as u16)
        .max(1);
    let (columns, rows) = image.dimensions();
    let decoded = (columns as usize)
        .saturating_mul(rows as usize)
        .saturating_mul(4);
    (
        image.cell_size(available).rows as usize,
        decoded.saturating_add(image.png_base64().len()),
    )
}

fn resident_line_bytes(line: &StyledLine) -> usize {
    std::mem::size_of::<StyledLine>()
        + line.text.capacity()
        + line.segments.capacity() * std::mem::size_of::<crate::StyledSegment>()
        + line
            .segments
            .iter()
            .map(|segment| segment.heap_capacity())
            .sum::<usize>()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{StyledSegment, ThemeColor};

    #[test]
    fn default_budget_is_eighty_percent_of_ghostty_1_3() {
        assert_eq!(HistoryBudget::default().terminal_bytes, 8_000_000);
        assert_eq!(
            HistoryBudget::default().eviction_target().terminal_bytes,
            7_200_000
        );
    }

    #[test]
    fn equal_row_counts_have_different_capacity_costs() {
        let plain = StyledLine::new("abc", ThemeColor::Default);
        let complex = StyledLine::new("a\u{301}b\u{301}c\u{301}", ThemeColor::Default);
        let styled = StyledLine::segments([("a", ThemeColor::Success), ("bc", ThemeColor::Error)]);
        assert!(
            line_footprint(&complex, 80).terminal_bytes > line_footprint(&plain, 80).terminal_bytes
        );
        assert!(
            line_footprint(&styled, 80).terminal_bytes > line_footprint(&plain, 80).terminal_bytes
        );
        assert!(
            line_footprint(&plain, 160).terminal_bytes > line_footprint(&plain, 80).terminal_bytes
        );
    }

    #[test]
    fn linked_segment_destination_counts_toward_resident_bytes() {
        let plain = StyledLine::rich_segments([StyledSegment::new("label", ThemeColor::Default)]);
        let linked = StyledLine::rich_segments([StyledSegment::new("label", ThemeColor::Default)
            .with_link_uri(Some("https://example.com/destination".to_string()))]);

        assert!(
            line_footprint(&linked, 80).resident_bytes > line_footprint(&plain, 80).resident_bytes
        );
    }

    #[test]
    fn snapshots_share_history_and_clone_only_the_paint_tail() {
        let snapshot = HistorySnapshot::new(
            vec![vec![StyledLine::new("line", ThemeColor::Default); 10_000].into()],
            Vec::new(),
        );
        let cloned = snapshot.clone();
        assert!(Arc::ptr_eq(&snapshot.chunks, &cloned.chunks));
        assert_eq!(cloned.tail(24).len(), 24);
    }
}
