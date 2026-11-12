use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::Mutex;

use anyhow::Context;

pub(crate) fn subscriber(
    path: Option<&Path>,
    filter: tracing_subscriber::EnvFilter,
) -> anyhow::Result<impl tracing::Subscriber + Send + Sync> {
    let writer: Box<dyn Write + Send> = match path {
        Some(path) => Box::new(
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .mode(0o600)
                .open(path)
                .with_context(|| format!("failed to open diagnostic log {}", path.display()))?,
        ),
        None => Box::new(io::sink()),
    };
    Ok(tracing_subscriber::fmt()
        .with_ansi(false)
        .log_internal_errors(false)
        .with_writer(Mutex::new(writer))
        .with_env_filter(filter)
        .finish())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn diagnostics_append_to_private_file() {
        let directory = std::env::temp_dir().join(format!(
            "phi-diagnostics-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("diagnostics.log");
        for message in ["first diagnostic", "second diagnostic"] {
            let subscriber =
                subscriber(Some(&path), tracing_subscriber::EnvFilter::new("warn")).unwrap();
            tracing::subscriber::with_default(subscriber, || {
                tracing::warn!("{message}");
                tracing::debug!("filtered diagnostic");
            });
        }
        let output = std::fs::read_to_string(&path).unwrap();
        assert_eq!(output.matches("first diagnostic").count(), 1);
        assert_eq!(output.matches("second diagnostic").count(), 1);
        assert!(!output.contains("filtered diagnostic"));
        assert!(!output.contains('\u{1b}'));
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn unavailable_diagnostic_file_is_an_explicit_startup_error() {
        let path = std::env::temp_dir()
            .join(format!("phi-missing-{}", uuid::Uuid::new_v4()))
            .join("diagnostics.log");
        let error = match subscriber(Some(&path), tracing_subscriber::EnvFilter::new("warn")) {
            Ok(_) => panic!("missing diagnostic directory accepted"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("failed to open diagnostic log"));
        assert!(error.to_string().contains(path.to_str().unwrap()));
    }
}
