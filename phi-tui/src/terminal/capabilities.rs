use std::process::Command;
use std::sync::OnceLock;

static HYPERLINKS_SUPPORTED: OnceLock<bool> = OnceLock::new();

pub(crate) fn hyperlinks_supported() -> bool {
    *HYPERLINKS_SUPPORTED.get_or_init(|| {
        detect_hyperlinks(&TerminalEnvironment::from_process(), probe_tmux_hyperlinks)
    })
}

#[derive(Default)]
struct TerminalEnvironment {
    override_value: Option<bool>,
    term_program: String,
    term: String,
    tmux: bool,
    kitty: bool,
    ghostty: bool,
    wezterm: bool,
    warp: bool,
    iterm: bool,
    windows_terminal: bool,
}

impl TerminalEnvironment {
    fn from_process() -> Self {
        Self {
            override_value: parse_override(std::env::var("PHI_HYPERLINKS").ok().as_deref()),
            term_program: env_lower("TERM_PROGRAM"),
            term: env_lower("TERM"),
            tmux: std::env::var_os("TMUX").is_some(),
            kitty: std::env::var_os("KITTY_WINDOW_ID").is_some(),
            ghostty: std::env::var_os("GHOSTTY_RESOURCES_DIR").is_some(),
            wezterm: std::env::var_os("WEZTERM_PANE").is_some(),
            warp: std::env::var_os("WARP_SESSION_ID").is_some()
                || std::env::var_os("WARP_TERMINAL_SESSION_UUID").is_some(),
            iterm: std::env::var_os("ITERM_SESSION_ID").is_some(),
            windows_terminal: std::env::var_os("WT_SESSION").is_some(),
        }
    }
}

fn detect_hyperlinks(environment: &TerminalEnvironment, tmux_probe: impl FnOnce() -> bool) -> bool {
    if let Some(supported) = environment.override_value {
        return supported;
    }
    if environment.tmux || environment.term.starts_with("tmux") {
        return tmux_probe();
    }
    if environment.term.starts_with("screen") {
        return false;
    }
    known_hyperlink_terminal(environment)
}

fn known_hyperlink_terminal(environment: &TerminalEnvironment) -> bool {
    environment.kitty
        || environment.ghostty
        || environment.wezterm
        || environment.warp
        || environment.iterm
        || environment.windows_terminal
        || environment.term.contains("ghostty")
        || matches!(
            environment.term_program.as_str(),
            "kitty"
                | "ghostty"
                | "wezterm"
                | "warpterminal"
                | "iterm.app"
                | "alacritty"
                | "vscode"
                | "zed"
        )
}

fn probe_tmux_hyperlinks() -> bool {
    Command::new("tmux")
        .args(["display-message", "-p", "#{client_termfeatures}"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .is_some_and(|features| {
            features
                .split(',')
                .any(|feature| feature.trim() == "hyperlinks")
        })
}

fn parse_override(value: Option<&str>) -> Option<bool> {
    match value {
        Some("1") => Some(true),
        Some("0") => Some(false),
        _ => None,
    }
}

fn env_lower(name: &str) -> String {
    std::env::var(name).unwrap_or_default().to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_known_and_unknown_terminals() {
        let known = TerminalEnvironment {
            term_program: "ghostty".to_string(),
            ..TerminalEnvironment::default()
        };

        assert!(detect_hyperlinks(&known, || false));
        assert!(!detect_hyperlinks(&TerminalEnvironment::default(), || true));
    }

    #[test]
    fn explicit_override_takes_precedence() {
        let environment = TerminalEnvironment {
            override_value: Some(false),
            term_program: "ghostty".to_string(),
            ..TerminalEnvironment::default()
        };

        assert!(!detect_hyperlinks(&environment, || true));
    }

    #[test]
    fn tmux_requires_forwarding_support() {
        let environment = TerminalEnvironment {
            tmux: true,
            ..TerminalEnvironment::default()
        };

        assert!(detect_hyperlinks(&environment, || true));
        assert!(!detect_hyperlinks(&environment, || false));
    }
}
