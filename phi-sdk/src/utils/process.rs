//! Bounded, cancellable process execution shared by hooks and extensions.

use crate::sandbox::Sandbox;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

const OUTPUT_LIMIT: usize = 1024 * 1024;
const PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(5);

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

pub(crate) fn run_sync(
    command: &mut std::process::Command,
    timeout: Duration,
    cancellation: Option<&tokio_util::sync::CancellationToken>,
) -> std::io::Result<std::process::Output> {
    use std::io::Read;
    use std::process::Stdio;
    use std::time::Instant;

    if cancellation.is_some_and(|token| token.is_cancelled()) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Interrupted,
            "process cancelled",
        ));
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command.spawn()?;
    #[cfg(unix)]
    let process_group = ProcessGroup(Some(child.id()));
    let read_output = |mut reader: Box<dyn Read + Send>| {
        std::thread::spawn(move || {
            let mut output = Vec::new();
            let mut truncated = false;
            let mut buffer = [0_u8; 8192];
            loop {
                let read = reader.read(&mut buffer)?;
                if read == 0 {
                    return if truncated {
                        Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "process output limit exceeded",
                        ))
                    } else {
                        Ok(output)
                    };
                }
                let remaining = OUTPUT_LIMIT.saturating_sub(output.len());
                output.extend_from_slice(&buffer[..read.min(remaining)]);
                truncated |= read > remaining;
            }
        })
    };
    let stdout = read_output(Box::new(child.stdout.take().expect("piped stdout")));
    let stderr = read_output(Box::new(child.stderr.take().expect("piped stderr")));
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None)
                if !cancellation.is_some_and(|token| token.is_cancelled())
                    && started.elapsed() < timeout =>
            {
                std::thread::sleep(PROCESS_POLL_INTERVAL);
            }
            result => {
                let _ = child.kill();
                let _ = child.wait();
                break match result {
                    Err(error) => Err(error),
                    _ if cancellation.is_some_and(|token| token.is_cancelled()) => Err(
                        std::io::Error::new(std::io::ErrorKind::Interrupted, "process cancelled"),
                    ),
                    _ => Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "process timed out",
                    )),
                };
            }
        }
    };
    #[cfg(unix)]
    drop(process_group);
    let join_output = |reader: std::thread::JoinHandle<std::io::Result<Vec<u8>>>| {
        reader
            .join()
            .map_err(|_| std::io::Error::other("process output reader panicked"))?
    };
    let stdout = join_output(stdout)?;
    let stderr = join_output(stderr)?;
    Ok(std::process::Output {
        status: status?,
        stdout,
        stderr,
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

    #[cfg(unix)]
    #[test]
    fn sync_commands_drain_pipes_and_bound_output() {
        const PIPE_OUTPUT_BYTES: usize = 64 * 1024;
        let mut command = std::process::Command::new("bash");
        command.args([
            "-c",
            &format!("printf '%{PIPE_OUTPUT_BYTES}s' ''; printf '%{PIPE_OUTPUT_BYTES}s' '' >&2"),
        ]);
        let output = run_sync(&mut command, Duration::from_secs(2), None).unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout.len(), PIPE_OUTPUT_BYTES);
        assert_eq!(output.stderr.len(), PIPE_OUTPUT_BYTES);
        let mut command = std::process::Command::new("bash");
        command.args(["-c", &format!("printf '%{}s' ''", OUTPUT_LIMIT + 1)]);
        assert_eq!(
            run_sync(&mut command, Duration::from_secs(2), None)
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::InvalidData
        );
    }

    #[cfg(unix)]
    #[test]
    fn sync_interruption_reaps_processes_holding_output_pipes() {
        for expected in [
            std::io::ErrorKind::TimedOut,
            std::io::ErrorKind::Interrupted,
        ] {
            let cancellation = tokio_util::sync::CancellationToken::new();
            let cancel = (expected == std::io::ErrorKind::Interrupted).then(|| {
                let cancellation = cancellation.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(50));
                    cancellation.cancel();
                })
            });
            let mut command = std::process::Command::new("bash");
            command.args(["-c", "sleep 10 & wait"]);
            let started = std::time::Instant::now();
            assert_eq!(
                run_sync(
                    &mut command,
                    Duration::from_millis(500),
                    Some(&cancellation)
                )
                .unwrap_err()
                .kind(),
                expected
            );
            assert!(started.elapsed() < Duration::from_secs(2));
            if let Some(cancel) = cancel {
                cancel.join().unwrap();
            }
        }
    }

    #[test]
    fn cancelled_command_never_spawns() {
        let cancellation = tokio_util::sync::CancellationToken::new();
        cancellation.cancel();
        let mut command = std::process::Command::new("/nonexistent/phi-cancelled-command");
        assert_eq!(
            run_sync(&mut command, Duration::from_secs(1), Some(&cancellation))
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::Interrupted
        );
    }

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
