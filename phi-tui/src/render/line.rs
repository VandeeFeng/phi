use crate::text::{display_width, fit, truncate};
use crate::{TerminalImage, ThemeColor};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StyledSegment {
    text: String,
    fg: ThemeColor,
    bold: bool,
    italic: bool,
    underline: bool,
    strikethrough: bool,
    // URI preserves non-web targets such as mailto: and file:.
    link_uri: Option<String>,
}

impl StyledSegment {
    pub fn new(text: impl Into<String>, fg: ThemeColor) -> Self {
        Self {
            text: text.into(),
            fg,
            bold: false,
            italic: false,
            underline: false,
            strikethrough: false,
            link_uri: None,
        }
    }

    pub fn with_bold(mut self, bold: bool) -> Self {
        self.bold = bold;
        self
    }

    pub fn with_italic(mut self, italic: bool) -> Self {
        self.italic = italic;
        self
    }

    pub fn with_underline(mut self, underline: bool) -> Self {
        self.underline = underline;
        self
    }

    pub fn with_strikethrough(mut self, strikethrough: bool) -> Self {
        self.strikethrough = strikethrough;
        self
    }

    #[inline]
    pub fn text(&self) -> &str {
        &self.text
    }

    pub(crate) fn heap_capacity(&self) -> usize {
        self.text.capacity()
            + self
                .link_uri
                .as_ref()
                .map_or(0, |link_uri| link_uri.capacity())
    }

    #[inline]
    pub fn color(&self) -> ThemeColor {
        self.fg
    }

    #[inline]
    pub fn is_bold(&self) -> bool {
        self.bold
    }

    #[inline]
    pub fn is_italic(&self) -> bool {
        self.italic
    }

    #[inline]
    pub fn is_underlined(&self) -> bool {
        self.underline
    }

    #[inline]
    pub fn is_strikethrough(&self) -> bool {
        self.strikethrough
    }

    pub(crate) fn with_link_uri(mut self, link_uri: Option<String>) -> Self {
        self.link_uri = link_uri;
        self
    }

    #[inline]
    pub(crate) fn link_uri(&self) -> Option<&str> {
        self.link_uri.as_deref()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StyledLine {
    pub text: String,
    pub fg: ThemeColor,
    pub bold: bool,
    pub bg: Option<ThemeColor>,
    pub segments: Vec<StyledSegment>,
    pub image: Option<TerminalImage>,
    pub(crate) image_owner: Option<u32>,
}

impl StyledLine {
    pub fn new(text: impl Into<String>, fg: ThemeColor) -> Self {
        Self {
            text: text.into(),
            fg,
            bold: false,
            bg: None,
            segments: Vec::new(),
            image: None,
            image_owner: None,
        }
    }

    pub fn image(image: TerminalImage) -> Self {
        Self {
            image: Some(image),
            ..Self::new(String::new(), ThemeColor::Default)
        }
    }

    pub fn segments(segments: impl IntoIterator<Item = (impl Into<String>, ThemeColor)>) -> Self {
        Self::rich_segments(
            segments
                .into_iter()
                .map(|(text, fg)| StyledSegment::new(text, fg)),
        )
    }

    pub fn rich_segments(segments: impl IntoIterator<Item = StyledSegment>) -> Self {
        let segments = segments.into_iter().collect::<Vec<_>>();
        let text = segments
            .iter()
            .map(|segment| segment.text.as_str())
            .collect();
        Self {
            text,
            fg: ThemeColor::Default,
            bold: false,
            bg: None,
            segments,
            image: None,
            image_owner: None,
        }
    }

    pub fn bold(mut self) -> Self {
        self.bold = true;
        self
    }

    pub fn bg(mut self, bg: ThemeColor) -> Self {
        self.bg = Some(bg);
        self
    }

    pub fn prepend_segment(self, text: impl Into<String>, fg: ThemeColor) -> Self {
        self.prepend_styled_segment(text, fg, false, false, false)
    }

    pub fn prepend_styled_segment(
        mut self,
        text: impl Into<String>,
        fg: ThemeColor,
        bold: bool,
        italic: bool,
        underline: bool,
    ) -> Self {
        let prefix_text = text.into();
        let original_text = self.text.clone();
        self.text = format!("{prefix_text}{original_text}");
        let prefix = StyledSegment::new(prefix_text, fg)
            .with_bold(bold)
            .with_italic(italic)
            .with_underline(underline);
        if self.segments.is_empty() {
            self.segments = vec![
                prefix,
                StyledSegment::new(original_text, self.fg).with_bold(self.bold),
            ];
        } else {
            self.segments.insert(0, prefix);
        }
        self
    }

    pub fn fit(mut self, width: u16) -> Self {
        self.fit_mut(width);
        self
    }

    pub fn fit_mut(&mut self, width: u16) {
        let width = width as usize;
        self.text = if self.image.is_some() {
            truncate(&self.text, width)
        } else {
            fit(&self.text, width)
        };
        if self.segments.is_empty() {
            return;
        }

        let mut used = 0;
        let mut fitted = Vec::new();
        for mut segment in std::mem::take(&mut self.segments) {
            if used >= width {
                break;
            }
            segment.text = truncate(&segment.text, width - used);
            used += display_width(&segment.text);
            if !segment.text.is_empty() {
                fitted.push(segment);
            }
        }
        if used < width && self.image.is_none() {
            fitted.push(StyledSegment::new(
                " ".repeat(width - used),
                ThemeColor::Default,
            ));
        }
        self.segments = fitted;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_truncates_styled_segments() {
        let line =
            StyledLine::segments([("ab", ThemeColor::Accent), ("cde", ThemeColor::Dim)]).fit(4);

        assert_eq!(line.text, "abcd");
        assert_eq!(line.segments[0].text, "ab");
        assert_eq!(line.segments[1].text, "cd");
    }
}
