use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use tokio::io::AsyncWriteExt;

use anyhow::Context;
use tokio::process::Command;

use crate::utils::{path::expand_home_path, workspace::get_project_meta};

#[derive(Debug, Clone, PartialEq, Eq)]
struct BlacklistedPath {
    path: PathBuf,
    is_directory: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sandbox {
    enabled: bool,
    working_directory: PathBuf,
    project_directory: PathBuf,
    whitelist: Vec<PathBuf>,
    blacklist: Vec<BlacklistedPath>,
    write_protection: Option<Arc<WriteProtection>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct WriteProtection {
    read_only: Vec<PathBuf>,
    write_exceptions: Vec<PathBuf>,
}

impl Sandbox {
    pub fn new(enabled: bool, working_directory: impl Into<PathBuf>) -> anyhow::Result<Self> {
        let (working_directory, project_directory) = resolve_directories(working_directory.into())?;
        if enabled && project_directory.parent().is_none() {
            anyhow::bail!("refusing to make the filesystem root writable");
        }
        Ok(Sandbox {
            enabled,
            working_directory,
            project_directory,
            whitelist: Vec::new(),
            blacklist: Vec::new(),
            write_protection: None,
        })
    }

    pub fn with_path_rules(
        mut self,
        whitelist: Vec<PathBuf>,
        blacklist: Vec<PathBuf>,
    ) -> anyhow::Result<Self> {
        if self.enabled {
            self.whitelist = resolve_whitelist(whitelist)?;
            self.blacklist = resolve_blacklist(blacklist, &self.working_directory)?;
        }
        Ok(self)
    }

    /// Process write protection is enforced after configurable writable mounts.
    pub fn with_write_protection(
        mut self,
        read_only: Vec<PathBuf>,
        write_exceptions: Vec<PathBuf>,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            cfg!(target_os = "linux"),
            "write protection requires Linux bubblewrap"
        );
        anyhow::ensure!(
            self.project_directory.parent().is_some(),
            "refusing to make the filesystem root writable"
        );
        let read_only = resolve_protected_paths(read_only)?;
        let write_exceptions = resolve_protected_paths(write_exceptions)?;
        for writable in &write_exceptions {
            anyhow::ensure!(
                !read_only.iter().any(|root| root.starts_with(writable)),
                "write exception contains a protected root: {}",
                writable.display()
            );
        }
        self.write_protection = Some(Arc::new(WriteProtection {
            read_only,
            write_exceptions,
        }));
        self.enabled = true;
        Ok(self)
    }

    pub(crate) fn ensure_write_allowed(&self, path: &Path) -> anyhow::Result<()> {
        let Some(protection) = &self.write_protection else {
            return Ok(());
        };
        let resolved = resolve_future_path(path)?;
        if protection
            .read_only
            .iter()
            .any(|root| resolved.starts_with(root))
            && !protection
                .write_exceptions
                .iter()
                .any(|root| resolved.starts_with(root))
        {
            anyhow::bail!(
                "path is read-only under process write protection: {}",
                path.display()
            );
        }
        Ok(())
    }

    pub(crate) async fn write_file(
        &self,
        path: &Path,
        content: &[u8],
        create_parents: bool,
    ) -> anyhow::Result<()> {
        if self.write_protection.is_none() {
            if create_parents && let Some(parent) = path.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            return tokio::fs::write(path, content).await.map_err(Into::into);
        }
        self.ensure_write_allowed(path)?;
        let mut command = self.file_write_command(path, create_parents)?;
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command
            .spawn()
            .context("failed to start write-protected file mutation")?;
        let write = child
            .stdin
            .take()
            .context("file mutation stdin is unavailable")?
            .write_all(content)
            .await;
        let output = child.wait_with_output().await?;
        anyhow::ensure!(
            output.status.success(),
            "write-protected file mutation failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
        write.map_err(Into::into)
    }

    fn file_write_command(&self, path: &Path, create_parents: bool) -> anyhow::Result<Command> {
        let parent = path
            .ancestors()
            .skip(1)
            .find(|parent| parent.is_dir())
            .context("file mutation has no existing parent")?;
        anyhow::ensure!(
            parent.parent().is_some(),
            "file mutation cannot make the filesystem root writable"
        );
        let mut sandbox = self.clone();
        sandbox.whitelist.push(std::fs::canonicalize(parent)?);
        let script = if create_parents {
            "mkdir -p -- \"$(dirname -- \"$1\")\" && cat > \"$1\""
        } else {
            "cat > \"$1\""
        };
        let args = vec![
            "-c".into(),
            script.into(),
            "phi-write".into(),
            path.to_string_lossy().into_owned(),
        ];
        Ok(sandbox.wrap_program("bash", &args))
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub(crate) fn unrestricted(&self) -> Self {
        Self {
            enabled: false,
            whitelist: Vec::new(),
            blacklist: Vec::new(),
            write_protection: None,
            ..self.clone()
        }
    }

    pub fn wrap_command(&self, command: &str) -> Command {
        if !self.enabled {
            let mut command_runner = Command::new("bash");
            command_runner.current_dir(&self.working_directory);
            command_runner.arg("-c").arg(command);
            return command_runner;
        }

        let mut command_runner = self.sandbox_command();
        command_runner.args(["bash", "-c", command]);
        command_runner
    }

    pub fn wrap_program(&self, program: &str, args: &[String]) -> Command {
        if !self.enabled {
            let mut command = Command::new(program);
            command.current_dir(&self.working_directory).args(args);
            return command;
        }

        let mut command = self.sandbox_command();
        command.arg(program).args(args);
        command
    }

    /// Overrides process cwd without expanding writable mounts or protection exceptions.
    pub fn wrap_program_in(
        &self,
        program: &str,
        args: &[String],
        working_dir: &Path,
    ) -> anyhow::Result<Command> {
        let mut sandbox = self.clone();
        sandbox.working_directory = std::fs::canonicalize(working_dir).with_context(|| {
            format!(
                "invalid process working directory: {}",
                working_dir.display()
            )
        })?;
        anyhow::ensure!(
            sandbox.working_directory.is_dir(),
            "process working directory is not a directory"
        );
        Ok(sandbox.wrap_program(program, args))
    }

    fn sandbox_command(&self) -> Command {
        let mut command = Command::new("bwrap");
        command.args([
            "--ro-bind",
            "/",
            "/",
            "--proc",
            "/proc",
            "--dev",
            "/dev",
            "--tmpfs",
            "/tmp",
        ]);
        for path in std::iter::once(&self.project_directory).chain(self.whitelist.iter()) {
            command.arg("--bind").arg(path).arg(path);
        }
        if let Some(protection) = &self.write_protection {
            for path in &protection.read_only {
                if path.exists() {
                    command.arg("--ro-bind").arg(path).arg(path);
                } else {
                    command
                        .arg("--tmpfs")
                        .arg(path)
                        .arg("--remount-ro")
                        .arg(path);
                }
            }
            for path in &protection.write_exceptions {
                if std::fs::canonicalize(path).is_ok_and(|resolved| resolved == *path) {
                    command.arg("--bind").arg(path).arg(path);
                }
            }
        }
        for entry in &self.blacklist {
            if entry.is_directory {
                command.arg("--tmpfs");
                command.arg(entry.path.as_os_str());
                command.arg("--remount-ro");
                command.arg(entry.path.as_os_str());
            } else {
                command.args(["--ro-bind", "/dev/null"]);
                command.arg(entry.path.as_os_str());
            }
        }
        command.arg("--chdir");
        command.arg(self.working_directory.as_os_str());
        command.args(["--unshare-all", "--die-with-parent"]);
        command
    }
}

fn resolve_protected_paths(paths: Vec<PathBuf>) -> anyhow::Result<Vec<PathBuf>> {
    let mut resolved = Vec::new();
    for path in paths {
        let path = resolve_future_path(&path)?;
        anyhow::ensure!(
            path.parent().is_some(),
            "cannot protect or exempt the filesystem root"
        );
        if !resolved.contains(&path) {
            resolved.push(path);
        }
    }
    resolved.sort_by_key(|path| path.components().count());
    let mut roots: Vec<PathBuf> = Vec::new();
    for path in resolved {
        if !roots.iter().any(|root| path.starts_with(root)) {
            roots.push(path);
        }
    }
    Ok(roots)
}

fn resolve_future_path(path: &Path) -> anyhow::Result<PathBuf> {
    anyhow::ensure!(
        path.is_absolute(),
        "protected path must be absolute: {}",
        path.display()
    );
    let mut existing = path;
    let mut missing = Vec::new();
    loop {
        match std::fs::canonicalize(existing) {
            Ok(mut resolved) => {
                for component in missing.into_iter().rev() {
                    resolved.push(component);
                }
                return Ok(resolved);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                missing.push(
                    existing
                        .file_name()
                        .context("invalid protected path")?
                        .to_owned(),
                );
                existing = existing
                    .parent()
                    .context("protected path has no existing ancestor")?;
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("invalid protected path: {}", path.display()));
            }
        }
    }
}

fn resolve_listed_path(path: PathBuf, list: &str) -> anyhow::Result<PathBuf> {
    let path = match path.to_str() {
        Some(path) => expand_home_path(path),
        None => path,
    };
    if !path.is_absolute() {
        anyhow::bail!(
            "sandbox {list} path must be absolute or start with ~/: {}",
            path.display()
        );
    }
    let path = std::fs::canonicalize(&path)
        .with_context(|| format!("invalid sandbox {list} path: {}", path.display()))?;
    if !path.is_file() && !path.is_dir() {
        anyhow::bail!(
            "sandbox {list} path must be a file or directory: {}",
            path.display()
        );
    }
    Ok(path)
}

fn resolve_whitelist(paths: Vec<PathBuf>) -> anyhow::Result<Vec<PathBuf>> {
    let mut resolved = Vec::new();
    for path in paths {
        let path = resolve_listed_path(path, "whitelist")?;
        if path.parent().is_none() {
            anyhow::bail!("refusing to whitelist the filesystem root");
        }
        if !resolved.contains(&path) {
            resolved.push(path);
        }
    }
    Ok(resolved)
}

fn resolve_blacklist(
    paths: Vec<PathBuf>,
    working_directory: &std::path::Path,
) -> anyhow::Result<Vec<BlacklistedPath>> {
    let mut resolved = Vec::new();
    for path in paths {
        let path = resolve_listed_path(path, "blacklist")?;
        if working_directory.starts_with(&path) {
            anyhow::bail!(
                "sandbox blacklist path contains the working directory: {}",
                path.display()
            );
        }
        if !resolved.contains(&path) {
            resolved.push(path);
        }
    }
    resolved.sort_by_key(|path| path.components().count());
    let mut blacklist: Vec<BlacklistedPath> = Vec::new();
    for path in resolved {
        if blacklist
            .iter()
            .any(|entry| entry.is_directory && path.starts_with(&entry.path))
        {
            continue;
        }
        blacklist.push(BlacklistedPath {
            is_directory: path.is_dir(),
            path,
        });
    }
    Ok(blacklist)
}

fn resolve_directories(working_directory: PathBuf) -> anyhow::Result<(PathBuf, PathBuf)> {
    let working_directory = std::fs::canonicalize(&working_directory).with_context(|| {
        format!(
            "invalid sandbox working directory: {}",
            working_directory.display()
        )
    })?;
    if !working_directory.is_dir() {
        anyhow::bail!("sandbox working directory is not a directory");
    }
    let project = get_project_meta(&working_directory);
    let project_directory =
        std::fs::canonicalize(&project.root).context("invalid sandbox project directory")?;
    if !working_directory.starts_with(&project_directory) {
        anyhow::bail!("sandbox working directory is outside the project");
    }
    Ok((working_directory, project_directory))
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, process::Command};

    use super::Sandbox;

    #[test]
    fn binds_git_root_without_changing_working_directory() {
        let project = std::env::temp_dir().join(uuid::Uuid::new_v4().to_string());
        let nested = project.join("src/nested");
        std::fs::create_dir_all(&nested).unwrap();
        assert!(
            Command::new("git")
                .arg("init")
                .arg(&project)
                .output()
                .unwrap()
                .status
                .success()
        );

        let sandbox = Sandbox::new(true, &nested).unwrap();

        assert_eq!(sandbox.project_directory, project);
        assert_eq!(sandbox.working_directory, nested);
        std::fs::remove_dir_all(project).unwrap();
    }

    #[test]
    fn rejects_invalid_working_directory() {
        let path = std::env::temp_dir().join(uuid::Uuid::new_v4().to_string());

        assert!(Sandbox::new(true, path).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_writable_filesystem_root() {
        assert!(Sandbox::new(true, "/").is_err());
    }

    #[test]
    fn adds_writable_whitelist_and_masked_blacklist_mounts() {
        let root = std::env::temp_dir().join(uuid::Uuid::new_v4().to_string());
        let project = root.join("project");
        let allowed = root.join("allowed");
        let blocked_directory = project.join("blocked");
        let blocked_file = project.join("secret.txt");
        for path in [&project, &allowed, &blocked_directory] {
            std::fs::create_dir_all(path).unwrap();
        }
        std::fs::write(&blocked_file, "secret").unwrap();
        Command::new("git")
            .arg("init")
            .arg(&project)
            .output()
            .unwrap();

        let sandbox = Sandbox::new(true, &project)
            .unwrap()
            .with_path_rules(
                vec![allowed.clone()],
                vec![blocked_file.clone(), blocked_directory.clone()],
            )
            .unwrap();
        let args = sandbox
            .sandbox_command()
            .as_std()
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();

        assert!(contains_args(
            &args,
            &[
                "--bind",
                allowed.to_str().unwrap(),
                allowed.to_str().unwrap()
            ]
        ));
        assert!(contains_args(
            &args,
            &[
                "--tmpfs",
                blocked_directory.to_str().unwrap(),
                "--remount-ro",
                blocked_directory.to_str().unwrap()
            ]
        ));
        assert!(contains_args(
            &args,
            &["--ro-bind", "/dev/null", blocked_file.to_str().unwrap()]
        ));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn validates_and_expands_path_rules() {
        let project = std::env::temp_dir().join(uuid::Uuid::new_v4().to_string());
        std::fs::create_dir_all(&project).unwrap();
        Command::new("git")
            .arg("init")
            .arg(&project)
            .output()
            .unwrap();

        assert!(
            Sandbox::new(true, &project)
                .unwrap()
                .with_path_rules(vec![PathBuf::from("relative")], Vec::new())
                .is_err()
        );
        assert!(
            Sandbox::new(true, &project)
                .unwrap()
                .with_path_rules(Vec::new(), vec![project.clone()])
                .is_err()
        );
        let sandbox = Sandbox::new(true, &project)
            .unwrap()
            .with_path_rules(vec![PathBuf::from("~")], Vec::new())
            .unwrap();
        assert_eq!(
            sandbox.whitelist,
            [std::fs::canonicalize(dirs::home_dir().unwrap()).unwrap()]
        );
        std::fs::remove_dir_all(project).unwrap();
    }

    #[tokio::test]
    async fn write_protection_cannot_be_overridden_by_disabled_sandbox_or_whitelist() {
        let root = std::env::temp_dir().join(format!("phi-readonly-{}", uuid::Uuid::new_v4()));
        let project = root.join("project");
        let protected = project.join(".phi");
        let memory = protected.join("memory");
        std::fs::create_dir_all(&memory).unwrap();
        let settings = protected.join("settings.json");
        std::fs::write(&settings, "original").unwrap();
        let sandbox = Sandbox::new(false, &project)
            .unwrap()
            .with_write_protection(vec![protected.clone()], vec![memory.clone()])
            .unwrap()
            .with_path_rules(vec![protected.clone()], Vec::new())
            .unwrap();
        assert!(sandbox.is_enabled());
        assert!(sandbox.ensure_write_allowed(&settings).is_err());
        assert!(
            sandbox
                .ensure_write_allowed(&memory.join("note.md"))
                .is_ok()
        );
        let output = sandbox
            .wrap_command("cat .phi/settings.json; printf changed > .phi/settings.json")
            .output()
            .await
            .unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stdout).contains("original"));
        assert_eq!(std::fs::read_to_string(&settings).unwrap(), "original");
        assert!(
            !sandbox
                .wrap_command("mv .phi .phi-moved")
                .output()
                .await
                .unwrap()
                .status
                .success()
        );
        assert!(
            sandbox
                .wrap_command("printf note > .phi/memory/note.md")
                .output()
                .await
                .unwrap()
                .status
                .success()
        );
        sandbox
            .write_file(&memory.join("note.md"), b"tool write", true)
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(memory.join("note.md")).unwrap(),
            "tool write"
        );
        assert!(
            sandbox
                .write_file(&settings, b"tool overwrite", true)
                .await
                .is_err()
        );
        let outside = root.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        sandbox
            .write_file(
                &outside.join("new/note.md"),
                b"approved external write",
                true,
            )
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(outside.join("new/note.md")).unwrap(),
            "approved external write"
        );
        assert!(
            sandbox
                .write_file(&outside.join("missing/note.md"), b"edit", false)
                .await
                .is_err()
        );
        assert!(!outside.join("missing").exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn protects_symlink_aliases_and_missing_config_roots() {
        let root =
            std::env::temp_dir().join(format!("phi-readonly-alias-{}", uuid::Uuid::new_v4()));
        let project = root.join("project");
        let global = root.join("global");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&global).unwrap();
        let settings = global.join("settings.json");
        std::fs::write(&settings, "original").unwrap();
        std::os::unix::fs::symlink(&global, project.join("alias")).unwrap();
        let sandbox = Sandbox::new(false, &project)
            .unwrap()
            .with_write_protection(vec![global.clone(), project.join(".phi")], Vec::new())
            .unwrap()
            .with_path_rules(vec![global.clone()], Vec::new())
            .unwrap();
        assert!(
            sandbox
                .ensure_write_allowed(&project.join("alias/settings.json"))
                .is_err()
        );
        assert!(
            sandbox
                .ensure_write_allowed(&project.join(".phi/new/settings.json"))
                .is_err()
        );
        assert!(
            sandbox
                .write_file(&project.join("alias/settings.json"), b"changed", true)
                .await
                .is_err()
        );
        assert!(
            !sandbox
                .wrap_command("printf changed > alias/settings.json")
                .output()
                .await
                .unwrap()
                .status
                .success()
        );
        assert!(
            !sandbox
                .wrap_command("mkdir -p .phi/new && printf changed > .phi/new/settings.json")
                .output()
                .await
                .unwrap()
                .status
                .success()
        );
        assert_eq!(std::fs::read_to_string(&settings).unwrap(), "original");
        assert!(!project.join(".phi/new/settings.json").exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    fn contains_args(args: &[String], expected: &[&str]) -> bool {
        args.windows(expected.len()).any(|window| {
            window
                .iter()
                .map(String::as_str)
                .eq(expected.iter().copied())
        })
    }
}
