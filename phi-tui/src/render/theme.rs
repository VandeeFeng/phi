use std::collections::HashMap;

use crossterm::style::Color;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ThemeColor {
    Default,
    Text,
    Accent,
    Title,
    Output,
    Border,
    BorderAccent,
    BorderMuted,
    Active,
    Running,
    Success,
    Error,
    Warning,
    Muted,
    Dim,
    MdHeading,
    MdLink,
    MdLinkUrl,
    MdCode,
    MdCodeBlock,
    MdCodeBlockBorder,
    MdQuote,
    MdQuoteBorder,
    MdHr,
    MdListBullet,
    SelectedBg,
}

#[derive(Debug, Clone)]
pub struct Theme {
    colors: HashMap<ThemeColor, Color>,
}

impl Theme {
    pub fn dark() -> Self {
        Self::from_colors([
            (ThemeColor::Default, Color::Reset),
            (ThemeColor::Text, rgb(0xD1, 0xD5, 0xDB)),
            (ThemeColor::Accent, rgb(0xB0, 0xC4, 0xDE)),
            (ThemeColor::Title, rgb(0xD1, 0xD5, 0xDB)),
            (ThemeColor::Output, rgb(0x80, 0x80, 0x80)),
            (ThemeColor::Border, rgb(0x64, 0x95, 0xED)),
            (ThemeColor::BorderAccent, rgb(0xB0, 0xC4, 0xDE)),
            (ThemeColor::BorderMuted, rgb(0x50, 0x50, 0x50)),
            (ThemeColor::Active, rgb(0x81, 0xA2, 0xBE)),
            (ThemeColor::Success, rgb(0x86, 0xAF, 0x80)),
            (ThemeColor::Error, rgb(0xCD, 0x5C, 0x5C)),
            (ThemeColor::Warning, rgb(0xE8, 0xAE, 0x5B)),
            (ThemeColor::Muted, rgb(0x80, 0x80, 0x80)),
            (ThemeColor::Dim, rgb(0x66, 0x66, 0x66)),
            (ThemeColor::MdHeading, rgb(0xF0, 0xC6, 0x74)),
            (ThemeColor::MdLink, rgb(0x81, 0xA2, 0xBE)),
            (ThemeColor::MdLinkUrl, rgb(0x66, 0x66, 0x66)),
            (ThemeColor::MdCode, rgb(0xB0, 0xC4, 0xDE)),
            (ThemeColor::MdCodeBlock, rgb(0x86, 0xAF, 0x80)),
            (ThemeColor::MdCodeBlockBorder, rgb(0x80, 0x80, 0x80)),
            (ThemeColor::MdQuote, rgb(0x80, 0x80, 0x80)),
            (ThemeColor::MdQuoteBorder, rgb(0x80, 0x80, 0x80)),
            (ThemeColor::MdHr, rgb(0x80, 0x80, 0x80)),
            (ThemeColor::MdListBullet, rgb(0xB0, 0xC4, 0xDE)),
            (ThemeColor::SelectedBg, rgb(0x2A, 0x2A, 0x2A)),
        ])
    }

    pub fn light() -> Self {
        Self::from_colors([
            (ThemeColor::Default, Color::Reset),
            (ThemeColor::Text, Color::Black),
            (ThemeColor::Accent, Color::Blue),
            (ThemeColor::Title, Color::Black),
            (ThemeColor::Output, Color::DarkGrey),
            (ThemeColor::Border, Color::Grey),
            (ThemeColor::BorderAccent, Color::Blue),
            (ThemeColor::BorderMuted, Color::DarkGrey),
            (ThemeColor::Active, Color::DarkYellow),
            (ThemeColor::Success, Color::Green),
            (ThemeColor::Error, Color::Red),
            (ThemeColor::Warning, Color::DarkYellow),
            (ThemeColor::Muted, Color::DarkGrey),
            (ThemeColor::Dim, Color::Grey),
            (ThemeColor::MdHeading, Color::DarkYellow),
            (ThemeColor::MdLink, Color::Blue),
            (ThemeColor::MdLinkUrl, Color::Grey),
            (ThemeColor::MdCode, Color::DarkCyan),
            (ThemeColor::MdCodeBlock, Color::Green),
            (ThemeColor::MdCodeBlockBorder, Color::DarkGrey),
            (ThemeColor::MdQuote, Color::DarkGrey),
            (ThemeColor::MdQuoteBorder, Color::DarkGrey),
            (ThemeColor::MdHr, Color::DarkGrey),
            (ThemeColor::MdListBullet, Color::Green),
            (ThemeColor::SelectedBg, Color::Grey),
        ])
    }

    pub fn from_colors(colors: impl IntoIterator<Item = (ThemeColor, Color)>) -> Self {
        Self {
            colors: colors.into_iter().collect(),
        }
    }

    pub fn color(&self, token: ThemeColor) -> Color {
        self.colors
            .get(&token)
            .or_else(|| {
                if token == ThemeColor::Running {
                    self.colors.get(&ThemeColor::Warning)
                } else {
                    None
                }
            })
            .copied()
            .unwrap_or(Color::Reset)
    }

    pub fn set_color(&mut self, token: ThemeColor, color: Color) {
        self.colors.insert(token, color);
    }

    pub fn with_color(mut self, token: ThemeColor, color: Color) -> Self {
        self.set_color(token, color);
        self
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self::dark()
    }
}

fn rgb(r: u8, g: u8, b: u8) -> Color {
    Color::Rgb { r, g, b }
}
