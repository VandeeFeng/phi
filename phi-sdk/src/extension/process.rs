use std::{
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
    time::Duration,
};

use crate::permission::command::CommandPermissionRequest;
use crate::permission::runtime::PermissionRuntime;
use crate::sandbox::Sandbox;

const DEFAULT_TIMEOUT_MS: u64 = 2_000;
const MAX_TIMEOUT_MS: u64 = 30_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionProcessRequest {
    pub extension: String,
    pub source: String,
    pub context: String,
    pub program: String,
    pub args: Vec<String>,
    pub timeout_ms: u64,
}

pub use crate::utils::process::ProcessOutput as ExtensionProcessOutput;

pub type ExtensionProcessFuture =
    Pin<Box<dyn Future<Output = anyhow::Result<ExtensionProcessOutput>> + Send + 'static>>;

pub trait ExtensionProcessRunner: Send + Sync {
    fn run(&self, request: ExtensionProcessRequest) -> ExtensionProcessFuture;

    /// The host selects Hook execution; request context strings grant no authority.
    fn run_hook(&self, request: ExtensionProcessRequest) -> ExtensionProcessFuture {
        self.run(request)
    }
}

#[derive(Debug, Clone, Default)]
pub struct ExtensionProcessPolicy {
    trusted_roots: Vec<PathBuf>,
}

impl ExtensionProcessPolicy {
    pub fn trust_root(mut self, root: impl Into<PathBuf>) -> Self {
        let root = root.into().canonicalize().unwrap_or_default();
        if !root.as_os_str().is_empty() {
            self.trusted_roots.push(root);
        }
        self
    }

    pub fn trusts(&self, source: &Path) -> bool {
        let source = source
            .canonicalize()
            .unwrap_or_else(|_| source.to_path_buf());
        self.trusted_roots
            .iter()
            .any(|root| source.starts_with(root))
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ExtensionHookPolicy {
    #[default]
    Enabled,
    Disabled,
}

#[derive(Clone, Default)]
pub struct ExtensionRuntimeServices {
    process_runner: Option<Arc<dyn ExtensionProcessRunner>>,
    process_policy: ExtensionProcessPolicy,
    permission: Option<PermissionRuntime>,
    sandbox_effects: bool,
    hook_policy: ExtensionHookPolicy,
}

impl ExtensionRuntimeServices {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn process_runner(mut self, runner: Arc<dyn ExtensionProcessRunner>) -> Self {
        self.process_runner = Some(runner);
        self
    }

    pub fn hook_policy(mut self, policy: ExtensionHookPolicy) -> Self {
        self.hook_policy = policy;
        self
    }

    pub(crate) fn hooks_enabled(&self) -> bool {
        self.hook_policy == ExtensionHookPolicy::Enabled
    }

    pub fn process_policy(mut self, policy: ExtensionProcessPolicy) -> Self {
        self.process_policy = policy;
        self
    }

    pub(crate) fn permission(mut self, permission: PermissionRuntime) -> Self {
        self.permission = Some(permission);
        self
    }

    pub fn sandbox_effects(mut self, enabled: bool) -> Self {
        self.sandbox_effects = enabled;
        self
    }

    pub(crate) fn process_runner_ref(&self) -> Option<&Arc<dyn ExtensionProcessRunner>> {
        self.process_runner.as_ref()
    }

    pub(crate) fn source_trusted(&self, source: &Path) -> bool {
        self.process_policy.trusts(source)
    }

    pub(crate) async fn authorize_process(
        &self,
        extension: &str,
        context: &str,
        program: &str,
        args: &[String],
    ) -> anyhow::Result<()> {
        let permission = self.permission.as_ref().ok_or_else(|| {
            anyhow::anyhow!("extension process permission runtime is unavailable")
        })?;
        let origin = format!("extension {extension} ({context})");
        permission
            .authorize_command(&CommandPermissionRequest::new(program, args).with_origin(&origin))
            .await
    }

    pub(crate) fn effects_sandboxed(&self) -> bool {
        self.sandbox_effects
    }
}

#[derive(Debug, Clone)]
pub struct LocalExtensionProcessRunner {
    sandbox: Sandbox,
}

impl LocalExtensionProcessRunner {
    pub fn new(sandbox: Sandbox) -> Self {
        Self { sandbox }
    }

    fn run_process(
        mut request: ExtensionProcessRequest,
        sandbox: Sandbox,
    ) -> ExtensionProcessFuture {
        request.timeout_ms = match request.timeout_ms {
            0 => DEFAULT_TIMEOUT_MS,
            timeout_ms => timeout_ms.min(MAX_TIMEOUT_MS),
        };
        Box::pin(async move {
            tracing::info!(
                extension = request.extension,
                source = request.source,
                context = request.context,
                program = request.program,
                "running extension process capability"
            );
            crate::utils::process::run(
                &sandbox,
                &request.program,
                &request.args,
                None,
                Duration::from_millis(request.timeout_ms),
            )
            .await
        })
    }
}

impl ExtensionProcessRunner for LocalExtensionProcessRunner {
    fn run(&self, request: ExtensionProcessRequest) -> ExtensionProcessFuture {
        Self::run_process(request, self.sandbox.clone())
    }

    fn run_hook(&self, request: ExtensionProcessRequest) -> ExtensionProcessFuture {
        Self::run_process(request, self.sandbox.unrestricted())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(args: &[&str], timeout_ms: u64) -> ExtensionProcessRequest {
        ExtensionProcessRequest {
            extension: "test".to_string(),
            source: "/tmp/test.lua".to_string(),
            context: "PreToolUse".to_string(),
            program: "bash".to_string(),
            args: args.iter().map(ToString::to_string).collect(),
            timeout_ms,
        }
    }

    #[tokio::test]
    async fn process_authorization_fails_without_permission_runtime() {
        let error = ExtensionRuntimeServices::new()
            .authorize_process("test", "Command", "printf", &[])
            .await
            .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("permission runtime is unavailable")
        );
    }

    #[tokio::test]
    async fn runner_passes_arguments_without_shell_interpolation() {
        let runner =
            LocalExtensionProcessRunner::new(Sandbox::new(false, std::env::temp_dir()).unwrap());
        let output = runner
            .run(ExtensionProcessRequest {
                extension: "test".to_string(),
                source: "/tmp/test.lua".to_string(),
                context: "Command".to_string(),
                program: "printf".to_string(),
                args: vec!["%s".to_string(), "hello; echo unsafe".to_string()],
                timeout_ms: 1_000,
            })
            .await
            .unwrap();

        assert!(output.success);
        assert_eq!(output.stdout, "hello; echo unsafe");
        assert!(output.stderr.is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancelled_process_kills_script_descendants() {
        let root =
            std::path::Path::new("/tmp").join(format!("phi-hook-cancel-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let ready = root.join("ready");
        let orphan = root.join("orphan");
        let runner = LocalExtensionProcessRunner::new(Sandbox::new(false, &root).unwrap());
        let task = tokio::spawn(runner.run_hook(request(
            &["-c", "(sleep 0.3; touch orphan) & touch ready; wait"],
            2_000,
        )));
        tokio::time::timeout(Duration::from_secs(2), async {
            while !ready.exists() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        task.abort();
        let _ = task.await;
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(!orphan.exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn runner_enforces_timeout() {
        let runner =
            LocalExtensionProcessRunner::new(Sandbox::new(false, std::env::temp_dir()).unwrap());

        let error = runner
            .run_hook(request(&["-c", "sleep 1"], 10))
            .await
            .unwrap_err();

        assert!(error.to_string().contains("timed out"));
    }

    #[tokio::test]
    async fn runner_limits_captured_output() {
        let runner =
            LocalExtensionProcessRunner::new(Sandbox::new(false, std::env::temp_dir()).unwrap());

        let output = runner
            .run_hook(request(
                &["-c", "head -c 1100000 /dev/zero | tr '\\0' x"],
                2_000,
            ))
            .await
            .unwrap();

        assert!(output.success);
        assert!(output.truncated);
        assert_eq!(output.stdout.len(), 1024 * 1024);
    }
}
