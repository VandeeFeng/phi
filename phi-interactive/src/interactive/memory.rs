use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::commands::{MemoryCheckOptions, MemoryScope};

use phi_sdk::extension::memory::{
    MemoryConfig, MemorySettings, TapeAnchor, TapeQuery, TapeService, get_global_memory_dir,
    get_memory_dir, get_memory_repo_dir, is_memory_initialized, list_memory_files,
    settings_from_config, tape_gate_enabled,
};

#[derive(Debug, Clone)]
pub(crate) enum MemoryNotice {
    Warning { message: String },
    Info { message: String },
}

#[derive(Clone)]
pub(crate) struct MemoryAdministration {
    configured: bool,
    memory: MemoryConfig,
    working_dir: PathBuf,
}

impl MemoryAdministration {
    pub(crate) fn new(memory: Option<&MemoryConfig>, startup_dir: &std::path::Path) -> Self {
        Self {
            configured: memory.is_some(),
            memory: memory.cloned().unwrap_or_default(),
            working_dir: startup_dir.to_path_buf(),
        }
    }

    pub(crate) fn check(&self, options: &MemoryCheckOptions) -> Vec<MemoryNotice> {
        if !self.memory.enabled {
            return vec![MemoryNotice::Warning {
                message: "memory is disabled in settings".into(),
            }];
        }
        let settings = settings_from_config(&self.memory);
        let memory_dir = get_memory_dir(&settings, &self.working_dir);
        let repo_dir = get_memory_repo_dir(&settings);
        let project = phi_sdk::workspace::get_project_meta(&self.working_dir);
        let git_status = std::process::Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(&repo_dir)
            .output()
            .ok();
        let is_dirty = git_status
            .as_ref()
            .filter(|output| output.status.success())
            .is_some_and(|output| !output.stdout.is_empty());
        let repo_status = if git_status
            .as_ref()
            .is_some_and(|output| output.status.success())
        {
            if is_dirty {
                "Uncommitted changes"
            } else {
                "Clean"
            }
        } else if repo_dir.exists() {
            "Not a git repo"
        } else {
            "Not cloned"
        };
        if !is_memory_initialized(&memory_dir) {
            return vec![MemoryNotice::Info {
                message: format!(
                    "Memory: {} | Status: Not initialized | Repo: {repo_status} | Use /memory-init to set up | Path: {}",
                    project.name,
                    memory_dir.display()
                ),
            }];
        }
        let project_file_count = list_memory_files(&memory_dir)
            .map(|files| files.len())
            .unwrap_or_default();
        let message = format!(
            "Memory: {} | Repo: {repo_status} | Files: {project_file_count} | Path: {}",
            project.name,
            memory_dir.display()
        );
        let mut notices = vec![if is_dirty {
            MemoryNotice::Warning { message }
        } else {
            MemoryNotice::Info { message }
        }];
        let global = options.scope == MemoryScope::Global;
        let memory_scope = if global {
            get_global_memory_dir(&settings)
        } else {
            Some(memory_dir)
        };
        let Some(memory_scope) = memory_scope.filter(|path| path.exists()) else {
            notices.push(MemoryNotice::Warning {
                message: format!(
                    "Memory {} directory is not configured or does not exist.",
                    if global { "global" } else { "project" }
                ),
            });
            return notices;
        };
        notices.push(MemoryNotice::Info {
            message: render_memory_tree(&memory_scope, options.max_tree_lines),
        });
        notices
    }

    pub(crate) fn rewind_annotations(&self, session_id: &str) -> RewindAnnotations {
        if !self.configured {
            return RewindAnnotations::default();
        }
        let settings = settings_from_config(&self.memory);
        let label_prefix = settings
            .tape
            .as_ref()
            .and_then(|tape| tape.anchor.as_ref())
            .and_then(|anchor| anchor.label_prefix.clone())
            .unwrap_or_default();
        let anchors = self
            .load_rewind_anchors(&settings, session_id)
            .unwrap_or_default();
        RewindAnnotations {
            anchors,
            label_prefix,
        }
    }

    fn load_rewind_anchors(
        &self,
        settings: &MemorySettings,
        session_id: &str,
    ) -> Option<Vec<TapeAnchor>> {
        if !tape_gate_enabled(settings, &self.working_dir) {
            return None;
        }
        let project = phi_sdk::workspace::get_project_meta(&self.working_dir);
        let tape = TapeService::create(
            get_memory_dir(settings, &self.working_dir),
            &project.name,
            session_id,
            self.working_dir.display().to_string(),
        )
        .ok()?;
        tape.search(TapeQuery {
            session_id: Some(session_id.into()),
            ..Default::default()
        })
        .ok()
    }

    pub(crate) fn refresh(&self) -> MemoryNotice {
        MemoryNotice::Warning {
            message: if self.memory.enabled {
                "No memory files found to refresh"
            } else {
                "memory is disabled in settings"
            }
            .into(),
        }
    }

    pub(crate) fn review(&self, limit: usize) -> anyhow::Result<Option<TapeReview>> {
        if !self.memory.tape_enabled() {
            return Ok(None);
        }
        let settings = settings_from_config(&self.memory);
        anyhow::ensure!(
            tape_gate_enabled(&settings, &self.working_dir),
            "Tape runtime is unavailable."
        );
        let project = phi_sdk::workspace::get_project_meta(&self.working_dir);
        let tape = TapeService::create(
            get_memory_dir(&settings, &self.working_dir),
            &project.name,
            "",
            self.working_dir.display().to_string(),
        )
        .map_err(|_| anyhow::anyhow!("Tape runtime is unavailable."))?;
        let anchors = tape
            .search(TapeQuery {
                limit: Some(limit),
                ..Default::default()
            })
            .map_err(|error| anyhow::anyhow!("memory review failed: {error}"))?;
        Ok(Some(TapeReview::new(anchors, tape)))
    }
}

fn render_memory_tree(memory_dir: &std::path::Path, max_lines: usize) -> String {
    let max_lines = max_lines.max(1);
    if let Ok(output) = std::process::Command::new("tree")
        .args(["-L", "3", "-I", "node_modules", "--noreport"])
        .arg(memory_dir)
        .output()
    {
        if !output.status.success() {
            return find_memory_dirs(memory_dir);
        }
        let tree = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let lines = tree.lines().collect::<Vec<_>>();
        if lines.len() > max_lines {
            return format!("{}\n...", lines[..max_lines].join("\n"));
        }
        return tree;
    }
    find_memory_dirs(memory_dir)
}

fn find_memory_dirs(memory_dir: &std::path::Path) -> String {
    std::process::Command::new("find")
        .arg(memory_dir)
        .args(["-type", "d", "-not", "-path", "*/node_modules/*"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|output| !output.is_empty())
        .unwrap_or_else(|| "Unable to generate directory tree.".to_string())
}

#[derive(Default)]
pub(crate) struct RewindAnnotations {
    pub anchors: Vec<TapeAnchor>,
    pub label_prefix: String,
}

#[derive(Clone)]
pub(crate) struct TapeReview(Arc<Mutex<OpenedTapeReview>>);

struct OpenedTapeReview {
    tape: TapeService,
    anchors: Vec<TapeAnchor>,
}

impl TapeReview {
    pub(crate) fn new(mut anchors: Vec<TapeAnchor>, tape: TapeService) -> Self {
        anchors.sort_by(|left, right| right.timestamp.cmp(&left.timestamp));
        Self(Arc::new(Mutex::new(OpenedTapeReview { tape, anchors })))
    }

    pub(crate) fn snapshot(&self) -> Vec<TapeAnchor> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .anchors
            .clone()
    }

    pub(crate) fn delete(&self, anchor_id: &str) -> anyhow::Result<Vec<TapeAnchor>> {
        let mut review = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        anyhow::ensure!(
            review.tape.delete_anchor(anchor_id)?,
            "memory anchor not found"
        );
        review.anchors.retain(|anchor| anchor.id != anchor_id);
        Ok(review.anchors.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use phi_sdk::extension::memory::TapeAnchorType;

    fn administration(root: &std::path::Path) -> MemoryAdministration {
        MemoryAdministration {
            configured: true,
            memory: serde_json::from_value(serde_json::json!({
                "memoryDir": {"localPath": root.join("memory"), "globalMemory": "global"},
                "tape": {"enabled": true, "onlyGit": false, "anchor": {"labelPrefix": "startup/"}}
            }))
            .unwrap(),
            working_dir: root.join("project"),
        }
    }

    #[test]
    fn memory_check_preserves_initialization_global_and_dirty_notices() {
        let root = std::env::temp_dir().join(format!("phi-memory-admin-{}", uuid::Uuid::new_v4()));
        let admin = administration(&root);
        let options = MemoryCheckOptions {
            scope: MemoryScope::Global,
            max_tree_lines: 1,
        };
        std::fs::create_dir_all(&admin.working_dir).unwrap();
        let notices = admin.check(&options);
        assert!(matches!(&notices[..], [MemoryNotice::Info { message }]
            if message.contains("Not initialized") && message.contains("Not cloned")));
        let settings = settings_from_config(&admin.memory);
        let memory_dir = get_memory_dir(&settings, &admin.working_dir);
        std::fs::create_dir_all(memory_dir.join("core")).unwrap();
        let notices = admin.check(&options);
        assert!(matches!(&notices[1], MemoryNotice::Warning { message }
            if message == "Memory global directory is not configured or does not exist."));
        std::fs::create_dir_all(get_global_memory_dir(&settings).unwrap()).unwrap();
        let repo = get_memory_repo_dir(&settings);
        assert!(
            std::process::Command::new("git")
                .args(["init", "-q"])
                .arg(&repo)
                .status()
                .unwrap()
                .success()
        );
        std::fs::write(repo.join("dirty.md"), "fixture").unwrap();
        let notices = admin.check(&options);
        assert!(matches!(&notices[0], MemoryNotice::Warning { message }
            if message.contains("Uncommitted changes") && message.contains(&memory_dir.display().to_string())));
        assert!(matches!(&notices[1], MemoryNotice::Info { message }
            if message.contains("global")));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn opened_review_keeps_loaded_subset_and_scope_across_settings_changes() {
        const CHILD_FLAG: &str = "PHI_TEST_MEMORY_REVIEW";
        if std::env::var_os(CHILD_FLAG).is_none() {
            let root = std::env::temp_dir()
                .join(format!("phi-memory-review-child-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&root).unwrap();
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "interactive::memory::tests::opened_review_keeps_loaded_subset_and_scope_across_settings_changes", "--nocapture"])
                .env(CHILD_FLAG, "1").env("PHI_SESSION_DIR", root.join("sessions"))
                .status().unwrap();
            std::fs::remove_dir_all(root).unwrap();
            assert!(status.success());
            return;
        }
        let root = std::env::temp_dir().join(format!("phi-memory-admin-{}", uuid::Uuid::new_v4()));
        let mut admin = administration(&root);
        std::fs::create_dir_all(&admin.working_dir).unwrap();
        let settings = settings_from_config(&admin.memory);
        let tape = TapeService::create(
            get_memory_dir(&settings, &admin.working_dir),
            "project",
            "",
            admin.working_dir.display().to_string(),
        )
        .unwrap();
        let mut session =
            crate::session::new("local", "test", 1_000, &std::env::current_dir().unwrap());
        session.working_dir = admin.working_dir.display().to_string().into();
        session.transcript.append_user_message("saved history");
        session.transcript.append_assistant_message(
            None,
            None,
            Vec::new(),
            "local".into(),
            phi_sdk::providers::ProviderApi::OpenAiCompletions,
            "test".into(),
            Default::default(),
            "stop".into(),
        );
        crate::session::storage::save_session(&session).unwrap();
        for name in ["first", "second", "third"] {
            tape.record_anchor_at_entry(
                session.id.as_str(),
                "entry",
                TapeAnchor::new(name, TapeAnchorType::Normal),
            )
            .unwrap();
        }
        let annotations = admin.rewind_annotations(session.id.as_str());
        assert_eq!(annotations.label_prefix, "startup/");
        assert_eq!(annotations.anchors.len(), 3);
        assert!(
            annotations
                .anchors
                .iter()
                .all(|anchor| anchor.session_id == session.id)
        );
        assert!(
            admin
                .rewind_annotations("another-session")
                .anchors
                .is_empty()
        );
        assert!(
            MemoryAdministration::new(None, &std::env::current_dir().unwrap())
                .rewind_annotations(session.id.as_str())
                .label_prefix
                .is_empty()
        );
        assert_eq!(tape.search(Default::default()).unwrap().len(), 3);
        let review = admin.review(1).unwrap().unwrap();
        assert_eq!(review.snapshot().len(), 1);
        let retained_id = review.snapshot()[0].id.to_string();
        admin.memory.enabled = false;
        admin.working_dir = root.join("other-agent");
        assert!(admin.review(1).unwrap().is_none());
        assert!(matches!(admin.refresh(), MemoryNotice::Warning { message }
            if message == "memory is disabled in settings"));
        assert!(review.delete("missing").is_err());
        assert_eq!(review.snapshot()[0].id, retained_id);
        assert!(review.delete(&retained_id).unwrap().is_empty());
        let remaining = tape.search(Default::default()).unwrap();
        assert_eq!(remaining.len(), 2);
        assert!(remaining.iter().all(|anchor| anchor.id != retained_id));
        assert!(!root.join("other-agent").exists());
        std::fs::write(root.join("blocked"), "fixture").unwrap();
        admin.memory.memory_dir.as_mut().unwrap().local_path =
            Some(root.join("blocked").display().to_string());
        let failed_read = admin.rewind_annotations(session.id.as_str());
        assert!(failed_read.anchors.is_empty());
        assert_eq!(failed_read.label_prefix, "startup/");
        std::fs::remove_dir_all(root).unwrap();
    }
}
