use std::io;

use crossterm::cursor::{Hide, Show};
use crossterm::event::{
    DisableBracketedPaste, DisableFocusChange, EnableBracketedPaste, EnableFocusChange,
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::style::Print;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};

pub struct TerminalGuard {
    input: InputModes,
}

#[derive(Default)]
struct InputModes {
    active: bool,
    keyboard_pushed: bool,
}

impl TerminalGuard {
    pub fn enter() -> anyhow::Result<Self> {
        enable_raw_mode()?;
        let mut guard = Self {
            input: InputModes::default(),
        };
        guard.input.enable(&mut io::stdout())?;
        Ok(guard)
    }

    /// Restores input reporting while keeping raw mode active until final cleanup.
    pub fn stop_input(&mut self) -> anyhow::Result<()> {
        self.input.stop(&mut io::stdout())?;
        Ok(())
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = self.stop_input();
        let _ = execute!(io::stdout(), Show);
        // Buffered protocol bytes belong to the TUI, not the shell that follows it.
        let _ = super::event::discard_pending_input();
        let _ = disable_raw_mode();
    }
}

impl InputModes {
    fn enable(&mut self, output: &mut impl io::Write) -> io::Result<()> {
        let flags = KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
            | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
            | KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES;
        self.active = true;
        self.keyboard_pushed = execute!(output, PushKeyboardEnhancementFlags(flags)).is_ok();
        execute!(
            output,
            EnableBracketedPaste,
            EnableFocusChange,
            Hide,
            Print("\x1b[?u\x1b[>4;2m\x1b[?12l"),
        )
    }

    fn stop(&mut self, output: &mut impl io::Write) -> io::Result<()> {
        if !self.active {
            return Ok(());
        }
        execute!(
            output,
            DisableFocusChange,
            DisableBracketedPaste,
            Print("\x1b[>4;0m")
        )?;
        if self.keyboard_pushed {
            execute!(output, PopKeyboardEnhancementFlags)?;
            self.keyboard_pushed = false;
        }
        self.active = false;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyboard_modes_are_balanced_and_stop_is_idempotent() {
        let mut modes = InputModes::default();
        let mut output = Vec::new();
        modes.enable(&mut output).unwrap();
        modes.stop(&mut output).unwrap();
        let stopped = output.clone();
        modes.stop(&mut output).unwrap();
        assert_eq!(output, stopped);
        let encoded = String::from_utf8(output).unwrap();
        assert!(encoded.starts_with("\x1b[>13u"));
        assert_eq!(encoded.matches("\x1b[<1u").count(), 1);
        assert!(!encoded.contains("\x1b[<u"));
    }

    #[test]
    fn partial_startup_keeps_enough_state_to_restore_keyboard_modes() {
        let mut modes = InputModes::default();
        let mut limited_output = io::Cursor::new([0_u8; b"\x1b[>13u".len()]);
        assert!(modes.enable(&mut limited_output).is_err());
        assert!(modes.active);
        assert!(modes.keyboard_pushed);
        let mut cleanup = Vec::new();
        modes.stop(&mut cleanup).unwrap();
        assert!(String::from_utf8(cleanup).unwrap().contains("\x1b[<1u"));
        assert!(!modes.active);
    }

    #[test]
    fn failed_keyboard_push_does_not_pop_the_callers_modes() {
        let mut modes = InputModes {
            active: true,
            keyboard_pushed: false,
        };
        let mut output = Vec::new();
        modes.stop(&mut output).unwrap();
        let encoded = String::from_utf8(output).unwrap();
        assert!(!encoded.contains("\x1b[<"));
        assert!(encoded.contains("\x1b[>4;0m"));
    }
}
