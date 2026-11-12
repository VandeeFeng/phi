use std::fmt::Write;

use crate::session::discovery::{self, SessionSummary};

const SAVED_SESSION_LIMIT: usize = 10;

pub(crate) fn run() -> anyhow::Result<()> {
    let sessions = discovery::discover_sessions("")?;
    print!("{}", format_sessions(&sessions));
    Ok(())
}

fn format_sessions(sessions: &[SessionSummary]) -> String {
    let mut output = String::from("SAVED SESSIONS\n");
    let visible: Vec<_> = sessions.iter().take(SAVED_SESSION_LIMIT).collect();
    if visible.is_empty() {
        output.push_str("(none)\n");
        return output;
    }
    let width = visible
        .iter()
        .map(|session| session.id.len())
        .max()
        .unwrap_or(2)
        .max(2);
    let _ = writeln!(output, "{:<width$}  {:<16} NAME", "ID", "UPDATED");
    for session in visible {
        let _ = writeln!(
            output,
            "{:<width$}  {:<16} {}",
            session.id,
            crate::interactive::short_time(session.updated_at.as_str()),
            crate::interactive::session_label(session)
        );
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_saved_sessions_with_prompt_labels_and_an_empty_state() {
        let sessions = [SessionSummary {
            id: "saved".into(),
            name: "".into(),
            updated_at: "2026-09-25T09:25:46.108595697+00:00".into(),
            first_user_prompt: Some("Fix the bug\nnow".into()),
        }];
        let output = format_sessions(&sessions);
        assert!(output.contains("saved"));
        assert!(output.contains("Fix the bug now"));
        assert!(output.contains(" NAME\n"));
        assert_eq!(format_sessions(&[]), "SAVED SESSIONS\n(none)\n");
    }

    #[test]
    fn shows_only_ten_recent_saved_sessions() {
        let sessions: Vec<_> = (0..12)
            .map(|index| SessionSummary {
                id: format!("session-{index}").into(),
                name: "".into(),
                updated_at: "2026-09-25T09:25:00Z".into(),
                first_user_prompt: None,
            })
            .collect();
        let output = format_sessions(&sessions);
        assert!(output.contains("session-9"));
        assert!(!output.contains("session-10"));
        assert!(!output.contains("session-11"));
    }
}
