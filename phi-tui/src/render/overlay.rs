use crate::{Overlay, OverlayOptions, StyledLine, resolve_overlay_frame};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverlayLayer {
    pub lines: Vec<StyledLine>,
    pub options: OverlayOptions,
}

pub fn render_overlay_layers<'a>(
    layers: impl IntoIterator<Item = &'a OverlayLayer>,
    width: u16,
    height: usize,
) -> Vec<Overlay<'a>> {
    layers
        .into_iter()
        .map(|layer| {
            let frame = resolve_overlay_frame(layer.options, width, height, layer.lines.len());
            let lines = frame.max_height.map_or(layer.lines.as_slice(), |max| {
                &layer.lines[..layer.lines.len().min(max)]
            });
            frame.as_overlay(lines)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{OverlaySize, ThemeColor};

    #[test]
    fn clips_overlay_lines_to_resolved_max_height() {
        let layer = OverlayLayer {
            lines: (0..5)
                .map(|index| StyledLine::new(index.to_string(), ThemeColor::Default))
                .collect(),
            options: OverlayOptions {
                max_height: Some(OverlaySize::Cells(2)),
                ..OverlayOptions::default()
            },
        };

        let overlays = render_overlay_layers([&layer], 40, 10);

        assert_eq!(overlays[0].lines.len(), 2);
    }
}
