//! Bounded, cancellable process execution shared by hooks and extensions.

use crate::sandbox::Sandbox;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

const OUTPUT_LIMIT: usize = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessOutput {
    pub success: bool,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub truncated: bool,
}

pub(crate) async fn run(
    sandbox: &Sandbox,
    program: &str,
    args: &[String],
    input: Option<&str>,
    timeout: Duration,
) -> anyhow::Result<ProcessOutput> {
    let mut command = sandbox.wrap_program(program, args);
    command
        .kill_on_drop(true)
        .stdin(if input.is_some() {
            std::process::Stdio::piped()
        } else {
            std::process::Stdio::null()
        })
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command.spawn()?;
    #[cfg(unix)]
    let _process_group = ProcessGroup(child.id());
    let mut stdin = child.stdin.take();
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("process stdout unavailable"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow::anyhow!("process stderr unavailable"))?;
    let execution = async {
        let write_input = async {
            if let Some(stdin) = stdin.as_mut() {
                // A hook may exit without consuming its event.
                if let Err(error) = stdin.write_all(input.unwrap_or_default().as_bytes()).await {
                    if error.kind() != std::io::ErrorKind::BrokenPipe {
                        return Err(error);
                    }
                }
            }
            drop(stdin.take());
            Ok::<_, std::io::Error>(())
        };
        let ((), status, stdout, stderr) = tokio::try_join!(
            write_input,
            child.wait(),
            read_limited(stdout, OUTPUT_LIMIT),
            read_limited(stderr, OUTPUT_LIMIT),
        )?;
        Ok::<_, std::io::Error>((status, stdout, stderr))
    };
    let (status, stdout, stderr) = match tokio::time::timeout(timeout, execution).await {
        Ok(result) => result?,
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            anyhow::bail!("process timed out after {}ms", timeout.as_millis());
        }
    };
    Ok(ProcessOutput {
        success: status.success(),
        exit_code: status.code(),
        stdout: String::from_utf8_lossy(&stdout.0).into_owned(),
        stderr: String::from_utf8_lossy(&stderr.0).into_owned(),
        truncated: stdout.1 || stderr.1,
    })
}

#[cfg(unix)]
struct ProcessGroup(Option<u32>);

#[cfg(unix)]
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        if let Some(process_id) = self.0 {
            unsafe {
                libc::kill(-(process_id as i32), libc::SIGKILL);
            }
        }
    }
}

async fn read_limited(
    mut reader: impl AsyncRead + Unpin,
    limit: usize,
) -> std::io::Result<(Vec<u8>, bool)> {
    let mut output = Vec::new();
    let mut truncated = false;
    let mut buffer = [0_u8; 8192];
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        let remaining = limit.saturating_sub(output.len());
        output.extend_from_slice(&buffer[..read.min(remaining)]);
        truncated |= read > remaining;
    }
    Ok((output, truncated))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stdin_streams_and_tolerates_early_exit() {
        let sandbox = Sandbox::new(false, "/tmp").unwrap();
        let input = "untrusted '\"; $(exit 2)\n".repeat(10_000);
        for script in ["cat", "exit 2"] {
            let output = run(
                &sandbox,
                "bash",
                &["-c".into(), script.into()],
                Some(&input),
                Duration::from_secs(2),
            )
            .await
            .unwrap();
            if script == "cat" {
                assert!(output.success);
                assert_eq!(output.stdout, input);
            } else {
                assert_eq!(output.exit_code, Some(2));
            }
        }
    }
}
