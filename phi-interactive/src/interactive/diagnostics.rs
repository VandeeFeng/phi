use std::io::{BufRead, IsTerminal, Write};

use crate::runtime::attachment::AgentCapabilities;

fn format_resource_diagnostic(diagnostic: &crate::resources::ResourceDiagnostic) -> String {
    use crate::resources::DiagnosticResource;
    let title = match diagnostic.resource {
        DiagnosticResource::Prompt => "Prompt conflicts",
        DiagnosticResource::Skill => "Skill conflicts",
    };
    crate::resources::format_diagnostic(title, &diagnostic.path, &diagnostic.message)
}

pub(super) async fn report_resource_diagnostics(
    capabilities: &AgentCapabilities,
) -> anyhow::Result<()> {
    let mut messages = capabilities.agent_diagnostics.clone();
    messages.extend(
        capabilities
            .prompt_diagnostics
            .iter()
            .map(format_resource_diagnostic),
    );
    if messages.is_empty() {
        return Ok(());
    }
    tokio::task::spawn_blocking(move || {
        let stdin = std::io::stdin();
        let stderr = std::io::stderr();
        let acknowledge = stdin.is_terminal() && stderr.is_terminal();
        present_diagnostics(
            &mut stdin.lock(),
            &mut stderr.lock(),
            &messages,
            acknowledge,
        )
    })
    .await??;
    Ok(())
}

fn present_diagnostics(
    input: &mut impl BufRead,
    output: &mut impl Write,
    messages: &[String],
    acknowledge: bool,
) -> std::io::Result<()> {
    for message in messages {
        writeln!(output, "{message}")?;
    }
    if acknowledge {
        writeln!(
            output,
            "Invalid resources were skipped. Press Enter to continue."
        )?;
        output.flush()?;
        let mut answer = String::new();
        input.read_line(&mut answer)?;
        if !answer.ends_with('\n') {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "startup diagnostics were not acknowledged",
            ));
        }
    }
    output.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_diagnostics_use_declared_categories_and_indented_reasons() {
        use crate::resources::{DiagnosticResource, ResourceDiagnostic};

        for (resource, title) in [
            (DiagnosticResource::Prompt, "Prompt"),
            (DiagnosticResource::Skill, "Skill"),
        ] {
            for path in ["/tmp/example.md", "/tmp/path with spaces/SKILL.md"] {
                let diagnostic = ResourceDiagnostic {
                    resource,
                    path: path.into(),
                    message: "invalid YAML\noriginal location".into(),
                };
                assert_eq!(
                    format_resource_diagnostic(&diagnostic),
                    format!("[{title} conflicts]\n  {path}\n  invalid YAML\n  original location")
                );
            }
        }
    }

    #[test]
    fn resource_diagnostics_are_visible_and_acknowledged_before_startup_continues() {
        use std::io::Cursor;

        let messages = vec![
            "[Prompt conflicts]\n  /host/project/.phi/prompts/bad.md\n    allowed-tools: invalid value at line 3 column 16"
                .into(),
        ];
        let mut output = Vec::new();
        present_diagnostics(&mut Cursor::new(b"\n"), &mut output, &messages, true).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(output.starts_with(&messages[0]));
        assert!(output.contains("Press Enter to continue"));
        let error = present_diagnostics(&mut Cursor::new(b""), &mut Vec::new(), &messages, true)
            .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::UnexpectedEof);
        let mut output = Vec::new();
        present_diagnostics(&mut Cursor::new(b""), &mut output, &messages, false).unwrap();
        assert_eq!(
            String::from_utf8(output).unwrap(),
            format!("{}\n", messages[0])
        );
    }
}
