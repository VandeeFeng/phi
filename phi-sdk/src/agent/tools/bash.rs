use std::path::PathBuf;
use std::process::{ExitStatus, Stdio};

use rig::tool::Tool;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};
use tokio::time::{Duration, timeout};

use crate::core::event::{ToolOutputDetails, ToolResultDetails};

const PIPE_BUFFER_BYTES: usize = 8_192;
const EXIT_STDIO_GRACE: Duration = Duration::from_millis(100);
const MAX_COMMAND_OUTPUT_BYTES: u64 = 5_000_000_000;

use crate::agent::tools::{BashArgs, ToolError, check_bash_perm};
use crate::permission::runtime::PermissionRuntime;
use crate::sandbox::Sandbox;

pub struct BashTool {
    pub permission: Option<PermissionRuntime>,
    pub sandbox: Sandbox,
    dispatcher:
        Option<tokio::sync::mpsc::UnboundedSender<crate::harness::instance::bash::BashRequest>>,
}

impl BashTool {
    pub fn new(permission: Option<PermissionRuntime>, sandbox: Sandbox) -> Self {
        BashTool {
            permission,
            sandbox,
            dispatcher: None,
        }
    }

    pub(crate) fn with_dispatcher(
        mut self,
        dispatcher: Option<
            tokio::sync::mpsc::UnboundedSender<crate::harness::instance::bash::BashRequest>,
        >,
    ) -> Self {
        self.dispatcher = dispatcher;
        self
    }
}

impl Tool for BashTool {
    const NAME: &'static str = "Bash";

    type Error = ToolError;
    type Args = BashArgs;
    type Output = String;

    fn description(&self) -> String {
        "Execute a bash command in the current working directory. Returns combined stdout and stderr, limited to the last 2000 lines or 50KB. Larger output is saved to a temporary file for paged reads. Optionally provide a timeout in seconds. Set run_in_background to return an execution ID and output path immediately; completion is delivered automatically. Do not poll or sleep while waiting for background completion."
            .to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "Bash command to execute" },
                "timeout": { "type": "integer", "description": "Foreground wait in seconds (optional). On expiry the command moves to the background, except sleep commands which stop. For an explicitly background command in an unattended run, sets its runtime limit (default 1800, maximum 7200 seconds).", "minimum": 1 },
                "run_in_background": { "type": "boolean", "description": "Start as a managed background command; default false" }
            },
            "required": ["command"]
        })
    }

    async fn call(
        &self,
        context: &mut rig::tool::ToolContext,
        args: BashArgs,
    ) -> Result<String, ToolError> {
        check_bash_perm(&self.permission, &args.command).await?;

        let command = self.sandbox.wrap_command(&args.command);
        let (text, output) = if let Some(dispatcher) = &self.dispatcher {
            use crate::harness::instance::bash::{BashLifetime, BashMode, BashReply, BashRequest};
            let mut scope = crate::core::interaction::active_interaction_scope();
            scope.parent_tool_call_id =
                crate::core::interaction::active_tool_call_id().map(Into::into);
            let (reply, result) = tokio::sync::oneshot::channel();
            dispatcher
                .send(BashRequest {
                    command,
                    description: args.command,
                    scope,
                    mode: if args.run_in_background.unwrap_or(false) {
                        BashMode::Background
                    } else {
                        BashMode::Foreground
                    },
                    timeout: args.timeout.map(Duration::from_secs),
                    lifetime: BashLifetime::Unattended,
                    reply,
                })
                .map_err(|_| ToolError::Msg("Bash dispatcher is unavailable".into()))?;
            match result.await.map_err(|_| ToolError::Msg("Bash result channel closed".into()))?
                .map_err(ToolError::Msg)? {
                BashReply::Finished(text, output) => (text, output),
                BashReply::Background(execution) => return Ok(serde_json::json!({
                    "status": "background_started",
                    "execution_id": execution.id,
                    "output_path": execution.output_path,
                    "message": "Command continues in the background. Its terminal outcome will arrive automatically."
                }).to_string()),
            }
        } else {
            if args.run_in_background.unwrap_or(false) {
                return Err(ToolError::Msg(
                    "Background Bash requires an AgentInstance".into(),
                ));
            }
            run_command(command, args.timeout).await?
        };
        context.insert_result(ToolResultDetails {
            output: Some(output),
            ..Default::default()
        });
        Ok(text)
    }
}

async fn run_command(
    command: Command,
    timeout_secs: Option<u64>,
) -> Result<(String, ToolOutputDetails), ToolError> {
    run_with_output(command, timeout_secs, OutputAccumulator::new(), None, None).await
}

pub(crate) async fn run_managed_bash(
    command: Command,
    path: PathBuf,
    ready: tokio::sync::oneshot::Sender<()>,
    cancellation: tokio::sync::watch::Receiver<Option<String>>,
    output_changes: tokio::sync::watch::Sender<()>,
) -> Result<(String, ToolOutputDetails), ToolError> {
    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let file = options.open(&path).await?;
    let mut output = OutputAccumulator::new();
    output.file = Some(file);
    output.path = Some(path);
    output.byte_limit = Some(MAX_COMMAND_OUTPUT_BYTES);
    output.changes = Some(output_changes);
    run_with_output(command, None, output, Some(ready), Some(cancellation)).await
}

async fn run_with_output(
    mut command: Command,
    timeout_secs: Option<u64>,
    mut output: OutputAccumulator,
    ready: Option<tokio::sync::oneshot::Sender<()>>,
    mut cancellation: Option<tokio::sync::watch::Receiver<Option<String>>>,
) -> Result<(String, ToolOutputDetails), ToolError> {
    if let Some(reason) = cancellation
        .as_ref()
        .and_then(|receiver| receiver.borrow().clone())
    {
        return Err(ToolError::Msg(reason));
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);

    let mut child = command.spawn()?;
    let guard = ProcessGroupGuard::new(child.id());
    let managed = ready.is_some();
    if let Some(ready) = ready {
        let _ = ready.send(());
    }
    let status = {
        let wait = async {
            let collect = collect_output(&mut child, &mut output);
            if let Some(secs) = timeout_secs {
                timeout(Duration::from_secs(secs), collect)
                    .await
                    .map_err(|_| ToolError::Msg("Command timed out".into()))?
                    .map_err(ToolError::from)
            } else {
                collect.await.map_err(ToolError::from)
            }
        };
        tokio::select! {
            biased;
            reason = async {
                let Some(receiver) = cancellation.as_mut() else {
                    return std::future::pending::<String>().await;
                };
                if receiver.borrow().is_none() { let _ = receiver.changed().await; }
                receiver.borrow().clone().unwrap_or_else(|| "Agent stopped".into())
            } => Err(ToolError::Msg(reason)),
            result = wait => result,
        }
    };
    let status = match status {
        Ok(status) => status,
        Err(error) => {
            drop(guard);
            let _ = child.kill().await;
            return Err(error);
        }
    };
    let exit_code = status.code().unwrap_or_else(|| {
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            128 + status.signal().unwrap_or(0)
        }
        #[cfg(not(unix))]
        {
            -1
        }
    });
    let result = output.finish(exit_code).await?;
    if !managed {
        guard.disarm();
    }
    Ok(result)
}

async fn collect_output(
    child: &mut Child,
    output: &mut OutputAccumulator,
) -> std::io::Result<ExitStatus> {
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| std::io::Error::other("missing stdout pipe"))?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| std::io::Error::other("missing stderr pipe"))?;
    let mut stdout_buffer = [0; PIPE_BUFFER_BYTES];
    let mut stderr_buffer = [0; PIPE_BUFFER_BYTES];
    let mut stdout_pending = Vec::new();
    let mut stderr_pending = Vec::new();
    let mut stdout_open = true;
    let mut stderr_open = true;
    let mut status = None;
    let idle = tokio::time::sleep(EXIT_STDIO_GRACE);
    tokio::pin!(idle);
    while status.is_none() || stdout_open || stderr_open {
        tokio::select! {
            result = stdout.read(&mut stdout_buffer), if stdout_open => {
                let count = result?;
                stdout_open = count != 0;
                output.append_pipe(&stdout_buffer[..count], &mut stdout_pending).await?;
                if status.is_some() && count != 0 {
                    idle.as_mut().reset(tokio::time::Instant::now() + EXIT_STDIO_GRACE);
                }
            }
            result = stderr.read(&mut stderr_buffer), if stderr_open => {
                let count = result?;
                stderr_open = count != 0;
                output.append_pipe(&stderr_buffer[..count], &mut stderr_pending).await?;
                if status.is_some() && count != 0 {
                    idle.as_mut().reset(tokio::time::Instant::now() + EXIT_STDIO_GRACE);
                }
            }
            result = child.wait(), if status.is_none() => {
                status = Some(result?);
                idle.as_mut().reset(tokio::time::Instant::now() + EXIT_STDIO_GRACE);
            }
            _ = &mut idle, if status.is_some() => break,
        }
    }
    output.append(&stdout_pending).await?;
    output.append(&stderr_pending).await?;
    status.ok_or_else(|| std::io::Error::other("process exited without status"))
}

struct ProcessGroupGuard {
    pid: Option<u32>,
}

impl ProcessGroupGuard {
    fn new(pid: Option<u32>) -> Self {
        Self { pid }
    }

    fn disarm(mut self) {
        self.pid = None;
    }
}

#[cfg(unix)]
impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        if let Some(pid) = self.pid
            && pid <= i32::MAX as u32
        {
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
            }
        }
    }
}

#[cfg(not(unix))]
impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {}
}

const MAX_OUTPUT_BYTES: usize = 50 * 1024;
const MAX_OUTPUT_LINES: usize = 2_000;

struct OutputAccumulator {
    tail: Vec<u8>,
    total_bytes: u64,
    newlines: u64,
    last_byte: Option<u8>,
    file: Option<tokio::fs::File>,
    path: Option<PathBuf>,
    byte_limit: Option<u64>,
    changes: Option<tokio::sync::watch::Sender<()>>,
}

impl OutputAccumulator {
    fn new() -> Self {
        Self {
            tail: Vec::new(),
            total_bytes: 0,
            newlines: 0,
            last_byte: None,
            file: None,
            path: None,
            byte_limit: None,
            changes: None,
        }
    }

    async fn append_pipe(&mut self, bytes: &[u8], pending: &mut Vec<u8>) -> std::io::Result<()> {
        // Interleaving pipes must not split a UTF-8 code point.
        pending.extend_from_slice(bytes);
        let mut complete = 0;
        while complete < pending.len() {
            match std::str::from_utf8(&pending[complete..]) {
                Ok(_) => {
                    complete = pending.len();
                    break;
                }
                Err(error) => {
                    complete += error.valid_up_to();
                    let Some(length) = error.error_len() else {
                        break;
                    };
                    complete += length;
                }
            }
        }
        self.append(&pending[..complete]).await?;
        pending.drain(..complete);
        Ok(())
    }

    async fn append(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        if self
            .byte_limit
            .is_some_and(|limit| self.total_bytes + bytes.len() as u64 > limit)
        {
            return Err(std::io::Error::other("Bash output exceeded its disk limit"));
        }
        self.total_bytes += bytes.len() as u64;
        self.newlines += bytes.iter().filter(|byte| **byte == b'\n').count() as u64;
        self.last_byte = bytes.last().copied();
        if self.file.is_none()
            && (self.total_bytes > MAX_OUTPUT_BYTES as u64
                || self.total_lines() > MAX_OUTPUT_LINES as u64)
        {
            self.spill().await?;
        }
        if let Some(file) = &mut self.file {
            file.write_all(bytes).await?;
            if let Some(changes) = &self.changes {
                file.flush().await?;
                changes.send_replace(());
            }
        }
        let kept = bytes.len().min(MAX_OUTPUT_BYTES);
        if bytes.len() >= MAX_OUTPUT_BYTES {
            self.tail.clear();
        } else {
            let removed = (self.tail.len() + kept).saturating_sub(MAX_OUTPUT_BYTES);
            self.tail.drain(..removed);
        }
        self.tail.extend_from_slice(&bytes[bytes.len() - kept..]);
        Ok(())
    }

    async fn spill(&mut self) -> std::io::Result<()> {
        let path = std::env::temp_dir().join(format!("phi-output-{}.log", uuid::Uuid::new_v4()));
        let mut options = tokio::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options.open(&path).await?;
        file.write_all(&self.tail).await?;
        self.path = Some(path);
        self.file = Some(file);
        Ok(())
    }

    fn total_lines(&self) -> u64 {
        self.newlines + u64::from(self.last_byte.is_some_and(|byte| byte != b'\n'))
    }

    async fn finish(mut self, exit_code: i32) -> std::io::Result<(String, ToolOutputDetails)> {
        if self.file.is_none() && String::from_utf8_lossy(&self.tail).len() > MAX_OUTPUT_BYTES {
            self.spill().await?;
        }
        if let Some(file) = &mut self.file {
            file.flush().await?;
        }
        let start = if self.total_bytes > self.tail.len() as u64 {
            self.tail
                .iter()
                .position(|byte| byte & 0xc0 != 0x80)
                .unwrap_or(self.tail.len())
        } else {
            0
        };
        let decoded = String::from_utf8_lossy(&self.tail[start..]);
        let mut lines = decoded
            .split_inclusive('\n')
            .rev()
            .take(MAX_OUTPUT_LINES)
            .collect::<Vec<_>>();
        lines.reverse();
        let mut text = lines.concat();
        if text.len() > MAX_OUTPUT_BYTES {
            let start = text
                .char_indices()
                .find(|(index, _)| text.len() - index <= MAX_OUTPUT_BYTES)
                .map(|(index, _)| index)
                .unwrap_or(text.len());
            text.drain(..start);
        }
        if text.is_empty() {
            text.push_str("(no output)");
        }
        let truncated = self.total_bytes > self.tail.len() as u64
            || self.total_lines() > MAX_OUTPUT_LINES as u64
            || decoded.len() > MAX_OUTPUT_BYTES;
        if truncated && let Some(path) = &self.path {
            text = format!(
                "[Output truncated; full output: {}]\n{text}",
                path.display()
            );
        }
        if exit_code != 0 {
            text.push_str(&format!("\nExit code: {exit_code}"));
        }
        let details = ToolOutputDetails {
            total_bytes: self.total_bytes,
            total_lines: self.total_lines(),
            truncated,
            full_output_path: self.path,
            exit_code,
        };
        Ok((text, details))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::permission::PermissionConfig;
    use crate::permission::checker::PermissionChecker;

    #[cfg(unix)]
    #[tokio::test]
    async fn interleaved_pipes_preserve_split_utf8() {
        let sandbox = Sandbox::new(false, "/tmp").unwrap();
        let (text, details) = run_command(
            sandbox.wrap_command(
                "printf '\\347\\225'; sleep 0.1; printf 'stderr\\n' >&2; sleep 0.1; printf '\\214\\n'",
            ),
            Some(5),
        )
        .await
        .unwrap();
        assert!(text.contains('界'), "{text:?}");
        assert!(text.contains("stderr\n"));
        assert!(!text.contains('\u{fffd}'));
        assert_eq!(details.total_bytes, 11);
        assert_eq!(details.total_lines, 2);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn post_exit_output_drains_until_pipes_are_idle() {
        let sandbox = Sandbox::new(false, "/tmp").unwrap();
        let (text, details) = run_command(
            sandbox.wrap_command(
                "(for ((index=0; index<8; index++)); do printf 'tick\\n'; sleep 0.04; done) &",
            ),
            Some(5),
        )
        .await
        .unwrap();
        assert_eq!(text.lines().count(), 8);
        assert_eq!(details.total_lines, 8);
        let (text, _) = tokio::time::timeout(
            Duration::from_secs(2),
            run_command(sandbox.wrap_command("sleep 10 & printf '%s' \"$!\""), None),
        )
        .await
        .unwrap()
        .unwrap();
        let pid: i32 = text.parse().unwrap();
        unsafe {
            libc::kill(pid, libc::SIGKILL);
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancellation_and_timeout_kill_command_descendants() {
        for cancel in [true, false] {
            let root = std::path::Path::new("/tmp")
                .join(format!("phi-bash-cancel-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&root).unwrap();
            let sandbox = Sandbox::new(false, &root).unwrap();
            let task = tokio::spawn(async move {
                run_command(
                    sandbox.wrap_command(
                        "printf ready > ready; (sleep 1.4; printf leaked > leaked) & wait",
                    ),
                    Some(1),
                )
                .await
            });
            tokio::time::timeout(Duration::from_secs(2), async {
                while !root.join("ready").exists() {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            })
            .await
            .unwrap();
            if cancel {
                task.abort();
                assert!(task.await.unwrap_err().is_cancelled());
                tokio::time::sleep(Duration::from_millis(1_500)).await;
            } else {
                assert!(
                    task.await
                        .unwrap()
                        .unwrap_err()
                        .to_string()
                        .contains("timed out")
                );
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            assert!(!root.join("leaked").exists());
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[tokio::test]
    async fn permission_checks_original_and_final_commands() {
        let config = PermissionConfig::try_from_value(serde_json::json!({
            "bash": {"git push **": "ask", "echo rewritten": "allow", "echo blocked": "deny"}
        }))
        .unwrap();
        let checker = PermissionChecker::new(&config, None);
        let permission = PermissionRuntime::new(Arc::new(Mutex::new(checker)));
        let tool = BashTool::new(
            Some(permission),
            Sandbox::new(false, std::env::temp_dir()).unwrap(),
        );

        let mut context = rig::tool::ToolContext::new();
        let rewritten = tool.call(
            &mut context,
            BashArgs {
                command: "echo rewritten".to_string(),
                timeout: None,
                run_in_background: None,
            },
        );
        let error = crate::core::interaction::with_original_bash_command(
            Some("git push main".to_string()),
            rewritten,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("non-interactive"));

        let error = tool
            .call(
                &mut context,
                BashArgs {
                    command: "echo blocked".to_string(),
                    timeout: None,
                    run_in_background: None,
                },
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("Permission denied"));
    }

    #[tokio::test]
    async fn output_limit_rejects_growth_before_writing_more_bytes() {
        let mut output = OutputAccumulator::new();
        output.byte_limit = Some(4);
        output.append(b"1234").await.unwrap();
        assert!(
            output
                .append(b"5")
                .await
                .unwrap_err()
                .to_string()
                .contains("disk limit")
        );
        assert_eq!(output.total_bytes, 4);
        assert_eq!(output.tail, b"1234");
    }

    #[tokio::test]
    async fn chunk_boundaries_preserve_utf8_and_exact_spilled_bytes() {
        let text = format!("{}\nend\n", "界🙂".repeat(MAX_OUTPUT_BYTES / 7 + 1));
        for chunk_size in [7, 127, 8_192, text.len()] {
            let mut output = OutputAccumulator::new();
            let mut pending = Vec::new();
            for chunk in text.as_bytes().chunks(chunk_size) {
                output.append_pipe(chunk, &mut pending).await.unwrap();
                assert!(pending.len() < 4);
                assert!(output.tail.len() <= MAX_OUTPUT_BYTES);
            }
            output.append(&pending).await.unwrap();
            let (preview, details) = output.finish(0).await.unwrap();
            assert!(preview.ends_with("\nend\n"));
            assert!(!preview.contains('\u{fffd}'));
            assert_eq!(details.total_bytes, text.len() as u64);
            assert_eq!(details.total_lines, 2);
            let path = details.full_output_path.unwrap();
            assert_eq!(tokio::fs::read(&path).await.unwrap(), text.as_bytes());
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                assert_eq!(
                    tokio::fs::metadata(&path)
                        .await
                        .unwrap()
                        .permissions()
                        .mode()
                        & 0o777,
                    0o600
                );
            }
            tokio::fs::remove_file(path).await.unwrap();
        }
    }

    #[tokio::test]
    async fn invalid_utf8_expansion_is_spilled_before_truncation() {
        let mut bytes = vec![0xff; MAX_OUTPUT_BYTES / 2];
        bytes.extend_from_slice(&[0xe7, 0x95]);
        let mut output = OutputAccumulator::new();
        let mut pending = Vec::new();
        output.append_pipe(&bytes, &mut pending).await.unwrap();
        output.append(&pending).await.unwrap();
        let (preview, details) = output.finish(0).await.unwrap();
        assert!(details.truncated);
        assert!(preview.len() < MAX_OUTPUT_BYTES + 200);
        let path = details.full_output_path.unwrap();
        assert_eq!(tokio::fs::read(&path).await.unwrap(), bytes);
        tokio::fs::remove_file(path).await.unwrap();
    }

    #[tokio::test]
    async fn byte_and_line_thresholds_preserve_small_output() {
        for (bytes, truncated) in [
            (vec![b'x'; MAX_OUTPUT_BYTES], false),
            (vec![b'x'; MAX_OUTPUT_BYTES + 1], true),
            (vec![b'\n'; MAX_OUTPUT_LINES], false),
            (vec![b'\n'; MAX_OUTPUT_LINES + 1], true),
        ] {
            let mut output = OutputAccumulator::new();
            output.append(&bytes).await.unwrap();
            let (preview, details) = output.finish(0).await.unwrap();
            assert_eq!(details.truncated, truncated);
            if let Some(path) = details.full_output_path {
                assert_eq!(tokio::fs::read(&path).await.unwrap(), bytes);
                tokio::fs::remove_file(path).await.unwrap();
            } else {
                assert_eq!(preview.as_bytes(), bytes);
            }
        }
    }
}
