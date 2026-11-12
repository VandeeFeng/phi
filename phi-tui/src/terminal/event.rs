use std::{str::FromStr, time::Duration};

use crossterm::event::{self as term_event, Event, KeyCode, KeyEventKind, KeyModifiers};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyInput {
    None,
    Resize,
    FocusGained,
    FocusLost,
    Char(char),
    Ctrl(char),
    Paste(String),
    Enter,
    ShiftEnter,
    AltEnter,
    Esc,
    Tab,
    Backspace,
    Up,
    Down,
    Left,
    Right,
    CtrlUp,
    CtrlDown,
    CtrlLeft,
    CtrlRight,
    Unknown,
}

pub(super) fn discard_pending_input() -> std::io::Result<()> {
    let drained = discard_pending_events();
    #[cfg(unix)]
    flush_terminal_input()?;
    drained
}

fn discard_pending_events() -> std::io::Result<()> {
    const MAX_PENDING_EVENTS: usize = 256;
    for _ in 0..MAX_PENDING_EVENTS {
        if !term_event::poll(Duration::ZERO)? {
            break;
        }
        let _ = term_event::read()?;
    }
    Ok(())
}

#[cfg(unix)]
fn flush_terminal_input() -> std::io::Result<()> {
    use rustix::termios::{QueueSelector, tcflush};
    use std::io::IsTerminal;

    if std::io::stdin().is_terminal() {
        tcflush(std::io::stdin(), QueueSelector::IFlush)?;
    } else {
        tcflush(std::fs::File::open("/dev/tty")?, QueueSelector::IFlush)?;
    }
    Ok(())
}

pub(crate) const DEFAULT_POLL_TIMEOUT: Duration = Duration::from_millis(50);

pub fn poll_key_input() -> anyhow::Result<KeyInput> {
    poll_key_input_with_timeout(DEFAULT_POLL_TIMEOUT)
}

/// A zero timeout checks for input without waiting.
pub fn poll_key_input_with_timeout(timeout: Duration) -> anyhow::Result<KeyInput> {
    if !term_event::poll(timeout)? {
        return Ok(KeyInput::None);
    }
    Ok(normalize_event(term_event::read()?))
}

fn normalize_event(event: Event) -> KeyInput {
    let Event::Key(key) = event else {
        return match event {
            Event::Paste(text) => KeyInput::Paste(text),
            Event::Resize(_, _) => KeyInput::Resize,
            Event::FocusGained => KeyInput::FocusGained,
            Event::FocusLost => KeyInput::FocusLost,
            _ => KeyInput::None,
        };
    };

    if key.kind != KeyEventKind::Press {
        return KeyInput::None;
    }

    if matches!(key.code, KeyCode::Char('\n')) {
        return KeyInput::ShiftEnter;
    }

    if key.modifiers.contains(KeyModifiers::CONTROL)
        && let KeyCode::Char(c) = key.code
    {
        if c == 'j' {
            return KeyInput::ShiftEnter;
        }
        return KeyInput::Ctrl(c);
    }

    match key.code {
        KeyCode::Up if key.modifiers.contains(KeyModifiers::CONTROL) => KeyInput::CtrlUp,
        KeyCode::Down if key.modifiers.contains(KeyModifiers::CONTROL) => KeyInput::CtrlDown,
        KeyCode::Left if key.modifiers.contains(KeyModifiers::CONTROL) => KeyInput::CtrlLeft,
        KeyCode::Right if key.modifiers.contains(KeyModifiers::CONTROL) => KeyInput::CtrlRight,
        KeyCode::Up => KeyInput::Up,
        KeyCode::Down => KeyInput::Down,
        KeyCode::Left => KeyInput::Left,
        KeyCode::Right => KeyInput::Right,
        KeyCode::Esc => KeyInput::Esc,
        KeyCode::Tab => KeyInput::Tab,
        KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => KeyInput::ShiftEnter,
        KeyCode::Enter if key.modifiers.contains(KeyModifiers::ALT) => KeyInput::AltEnter,
        KeyCode::Enter => KeyInput::Enter,
        KeyCode::Backspace => KeyInput::Backspace,
        KeyCode::Char(c) => KeyInput::Char(c),
        _ => KeyInput::Unknown,
    }
}

impl FromStr for KeyInput {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let value = value.trim().to_ascii_lowercase();
        let key = match value.as_str() {
            "enter" => Self::Enter,
            "shift+enter" => Self::ShiftEnter,
            "alt+enter" => Self::AltEnter,
            "esc" => Self::Esc,
            "tab" => Self::Tab,
            "backspace" => Self::Backspace,
            "up" => Self::Up,
            "down" => Self::Down,
            "left" => Self::Left,
            "right" => Self::Right,
            "ctrl+up" => Self::CtrlUp,
            "ctrl+down" => Self::CtrlDown,
            "ctrl+left" => Self::CtrlLeft,
            "ctrl+right" => Self::CtrlRight,
            value if value.starts_with("ctrl+") => {
                let mut characters = value[5..].chars();
                let character = characters
                    .next()
                    .filter(|_| characters.next().is_none())
                    .ok_or_else(|| format!("unsupported keybinding: {value}"))?;
                Self::Ctrl(character)
            }
            value => {
                let mut characters = value.chars();
                let character = characters
                    .next()
                    .filter(|_| characters.next().is_none())
                    .ok_or_else(|| format!("unsupported keybinding: {value}"))?;
                Self::Char(character)
            }
        };
        Ok(key)
    }
}

impl KeyInput {
    pub fn inserts_editor_newline(&self) -> bool {
        matches!(self, Self::ShiftEnter)
    }

    pub fn clears_text_input(&self, input: &mut String) -> bool {
        if !matches!(self, Self::Ctrl('c')) || input.is_empty() {
            return false;
        }

        input.clear();
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_terminal_events_without_changing_key_policy() {
        use crossterm::event::KeyEvent;
        let cases = [
            (Event::Resize(80, 24), KeyInput::Resize),
            (Event::FocusGained, KeyInput::FocusGained),
            (Event::FocusLost, KeyInput::FocusLost),
            (
                Event::Paste("a\r\nb".into()),
                KeyInput::Paste("a\r\nb".into()),
            ),
            (
                Event::Key(KeyEvent::new(KeyCode::Char('界'), KeyModifiers::NONE)),
                KeyInput::Char('界'),
            ),
            (
                Event::Key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL)),
                KeyInput::ShiftEnter,
            ),
            (
                Event::Key(KeyEvent::new(KeyCode::Char('\n'), KeyModifiers::NONE)),
                KeyInput::ShiftEnter,
            ),
            (
                Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT)),
                KeyInput::AltEnter,
            ),
            (
                Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT)),
                KeyInput::ShiftEnter,
            ),
            (
                Event::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::CONTROL)),
                KeyInput::CtrlUp,
            ),
            (
                Event::Key(KeyEvent::new(KeyCode::Left, KeyModifiers::CONTROL)),
                KeyInput::CtrlLeft,
            ),
            (
                Event::Key(KeyEvent::new(KeyCode::Right, KeyModifiers::CONTROL)),
                KeyInput::CtrlRight,
            ),
            (
                Event::Key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE)),
                KeyInput::Left,
            ),
            (
                Event::Key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE)),
                KeyInput::Right,
            ),
            (
                Event::Key(KeyEvent::new_with_kind(
                    KeyCode::Char('a'),
                    KeyModifiers::NONE,
                    KeyEventKind::Release,
                )),
                KeyInput::None,
            ),
            (
                Event::Key(KeyEvent::new_with_kind(
                    KeyCode::Char('a'),
                    KeyModifiers::NONE,
                    KeyEventKind::Repeat,
                )),
                KeyInput::None,
            ),
        ];
        for (event, expected) in cases {
            assert_eq!(normalize_event(event), expected);
        }
    }

    #[test]
    fn parses_extension_keybinding_names() {
        assert_eq!("ctrl+E".parse::<KeyInput>().unwrap(), KeyInput::Ctrl('e'));
        assert_eq!(
            "shift+enter".parse::<KeyInput>().unwrap(),
            KeyInput::ShiftEnter
        );
        assert_eq!("alt+enter".parse::<KeyInput>().unwrap(), KeyInput::AltEnter);
        assert_eq!("ctrl+left".parse::<KeyInput>().unwrap(), KeyInput::CtrlLeft);
        assert_eq!(
            "ctrl+right".parse::<KeyInput>().unwrap(),
            KeyInput::CtrlRight
        );
        assert!("ctrl+enter".parse::<KeyInput>().is_err());
    }
}
