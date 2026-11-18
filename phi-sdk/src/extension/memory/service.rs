use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use compact_str::CompactString;

use super::config::MemoryConfig;
use super::graph::MemoryGraph;
use super::graph::{
    MemoryGraphRecord, MemoryGraphSources, MemoryGraphStore, current_source_state,
    memory_graph_sources, reconcile_memory_graph,
};
use super::memory_core;
use super::tape::TapeService;

const GRAPH_WARMUP_MAX_ATTEMPTS: usize = 3;

struct CachedMemoryGraph {
    fingerprint: u64,
    graph: MemoryGraphRecord,
}

#[derive(Clone, Copy)]
enum MaterializationPolicy {
    BestEffort,
    Required,
}

impl MaterializationPolicy {
    fn apply<T>(self, result: anyhow::Result<T>, action: &str) -> anyhow::Result<()> {
        match (self, result) {
            (_, Ok(_)) => Ok(()),
            (Self::Required, Err(error)) => Err(error),
            (Self::BestEffort, Err(error)) => {
                tracing::warn!("failed to {action}: {error}");
                Ok(())
            }
        }
    }
}

pub struct MemoryService {
    memory_project_dir: PathBuf,
    graph_store: MemoryGraphStore,
    graph_sources: MemoryGraphSources,
    graph_cache: Mutex<Option<CachedMemoryGraph>>,
    tape: Option<Arc<TapeService>>,
}

impl MemoryService {
    pub fn new(memory_project_dir: impl Into<PathBuf>, project_name: &str) -> anyhow::Result<Self> {
        Self::with_sources(
            memory_project_dir,
            project_name,
            memory_graph_sources(project_name),
        )
    }

    fn with_sources(
        memory_project_dir: impl Into<PathBuf>,
        project_name: &str,
        graph_sources: MemoryGraphSources,
    ) -> anyhow::Result<Self> {
        let memory_project_dir = memory_project_dir.into();
        let graph_store = MemoryGraphStore::new(&memory_project_dir, project_name)?;
        Ok(Self {
            graph_store,
            graph_sources,
            memory_project_dir,
            graph_cache: Mutex::new(None),
            tape: None,
        })
    }

    pub fn create(
        config: &MemoryConfig,
        session_id: impl Into<CompactString>,
        working_dir: impl Into<PathBuf>,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(config.enabled, "memory is disabled");
        let working_dir = working_dir.into();
        let settings = memory_core::settings_from_config(config);
        let workspace = crate::utils::workspace::WorkspaceMeta::discover(&working_dir, None)?;
        let memory_project_dir = memory_core::get_memory_repo_dir(&settings).join(workspace.name());
        let mut service = Self::new(&memory_project_dir, workspace.name())?;
        if config.tape_enabled() && memory_core::tape_gate_enabled(&settings, &working_dir) {
            service.tape = Some(Arc::new(TapeService::create(
                &memory_project_dir,
                workspace.name(),
                session_id,
                working_dir.display().to_string(),
            )?));
        }
        Ok(service)
    }

    pub fn repository_is_dirty(settings: &super::types::MemorySettings) -> anyhow::Result<bool> {
        use super::memory_git::{LocalMemoryGit, MemoryGit};

        let directory = memory_core::get_memory_repo_dir(settings);
        let workspace = crate::utils::workspace::WorkspaceMeta::discover(&directory, None)?;
        anyhow::ensure!(
            workspace.git_dir().is_some()
                && directory.canonicalize()? == workspace.root.canonicalize()?,
            "memory directory is not a repository root"
        );
        let status = LocalMemoryGit::new(&directory, None).status()?;
        anyhow::ensure!(
            status.success,
            "memory repository status failed: {}",
            status.stdout
        );
        Ok(!status.stdout.is_empty())
    }

    pub fn tape(&self) -> Option<&Arc<TapeService>> {
        self.tape.as_ref()
    }

    pub fn reconcile(&self) -> anyhow::Result<MemoryGraphRecord> {
        self.load_graph(MaterializationPolicy::BestEffort)
    }

    pub fn graph(&self) -> anyhow::Result<MemoryGraph> {
        Ok(MemoryGraph::from_record(
            &self.load_graph(MaterializationPolicy::BestEffort)?,
        ))
    }

    pub fn refresh(&self) -> anyhow::Result<MemoryGraphRecord> {
        *self
            .graph_cache
            .lock()
            .map_err(|_| anyhow::anyhow!("memory graph cache lock poisoned"))? = None;
        self.load_graph(MaterializationPolicy::Required)
    }

    pub fn memory_project_dir(&self) -> &Path {
        &self.memory_project_dir
    }

    fn load_graph(
        &self,
        materialization: MaterializationPolicy,
    ) -> anyhow::Result<MemoryGraphRecord> {
        if let Some(tape) = &self.tape {
            return tape.with_canonical_lock(|| self.load_graph_unlocked(materialization));
        }
        self.load_graph_unlocked(materialization)
    }

    fn load_graph_unlocked(
        &self,
        materialization: MaterializationPolicy,
    ) -> anyhow::Result<MemoryGraphRecord> {
        let source_state = current_source_state(&self.memory_project_dir, &self.graph_sources)?;
        let store_fingerprint = self.store_fingerprint(materialization)?;
        let fingerprint = graph_fingerprint(&source_state, store_fingerprint);
        let mut cache = self
            .graph_cache
            .lock()
            .map_err(|_| anyhow::anyhow!("memory graph cache lock poisoned"))?;
        if let Some(cached) = cache
            .as_ref()
            .filter(|cached| cached.fingerprint == fingerprint)
        {
            return Ok(cached.graph.clone());
        }
        let graph = self.load_or_rebuild(source_state, store_fingerprint, materialization)?;
        let fingerprint = graph_fingerprint(
            &graph.source_state,
            self.store_fingerprint(materialization)?,
        );
        *cache = Some(CachedMemoryGraph {
            fingerprint,
            graph: graph.clone(),
        });
        Ok(graph)
    }

    fn store_fingerprint(&self, materialization: MaterializationPolicy) -> anyhow::Result<u64> {
        match self.graph_store.fingerprint() {
            Ok(fingerprint) => Ok(fingerprint),
            Err(error) if matches!(materialization, MaterializationPolicy::BestEffort) => {
                tracing::warn!("failed to fingerprint materialized graph: {error}");
                Ok(0)
            }
            Err(error) => Err(error),
        }
    }

    fn load_or_rebuild(
        &self,
        source_state: super::graph::MemoryGraphSourceState,
        store_fingerprint: u64,
        materialization: MaterializationPolicy,
    ) -> anyhow::Result<MemoryGraphRecord> {
        if let Ok(graph) = self.graph_store.load()
            && graph.sources == self.graph_sources
            && graph.source_state == source_state
        {
            if graph.nodes.is_empty() {
                materialization.apply(
                    self.graph_store.remove_if_unchanged(store_fingerprint),
                    "remove empty materialized graph",
                )?;
            }
            return Ok(graph);
        }
        let graph = reconcile_memory_graph(self.graph_sources.clone(), &self.memory_project_dir)?;
        let result = if graph.nodes.is_empty() {
            self.graph_store
                .remove_if_unchanged(store_fingerprint)
                .map(|_| ())
        } else {
            self.graph_store.save_if_changed(&graph).map(|_| ())
        };
        materialization.apply(result, "persist materialized graph")?;
        Ok(graph)
    }
}

pub(crate) fn warm_memory_graph(
    memory_dir: &Path,
    project_name: &str,
) -> anyhow::Result<MemoryGraph> {
    let mut last_error = None;
    for _ in 0..GRAPH_WARMUP_MAX_ATTEMPTS {
        match MemoryService::new(memory_dir, project_name).and_then(|service| service.graph()) {
            Ok(graph) => return Ok(graph),
            Err(error) => last_error = Some(error),
        }
    }
    let error = last_error.unwrap_or_else(|| anyhow::anyhow!("memory graph warmup failed"));
    Err(anyhow::anyhow!(
        "memory graph warmup failed after {GRAPH_WARMUP_MAX_ATTEMPTS} attempts: {error}"
    ))
}

fn graph_fingerprint(
    source_state: &super::graph::MemoryGraphSourceState,
    graph_fingerprint: u64,
) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    source_state.hash(&mut hasher);
    graph_fingerprint.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extension::memory::config::MemoryConfig;
    use crate::extension::memory::graph::{
        MemoryGraphNode, MemoryGraphRecord, memory_graph_sources,
    };
    use crate::extension::memory::tape::{TapeAnchor, TapeAnchorType, TapeConfig};
    use crate::extension::memory::types::MemoryDirSettings;

    fn temp_dir() -> PathBuf {
        std::env::temp_dir().join(format!("phi-sdk-memory-service-{}", uuid::Uuid::new_v4()))
    }

    fn memory_file(directory: &Path, name: &str) {
        std::fs::create_dir_all(directory.join("core")).unwrap();
        std::fs::write(
            directory.join(format!("core/{name}.md")),
            format!("---\ndescription: {name}\n---\nMemory"),
        )
        .unwrap();
    }

    fn anchor_file(directory: &Path) {
        std::fs::create_dir_all(directory).unwrap();
        let mut anchor = TapeAnchor::new("design", TapeAnchorType::Normal);
        anchor.session_id = "session".into();
        anchor.session_entry_id = "entry".into();
        std::fs::write(
            directory.join("project__anchors.jsonl"),
            format!("{}\n", serde_json::to_string(&anchor).unwrap()),
        )
        .unwrap();
    }

    fn isolated_config(memory_root: &Path, tape_enabled: bool) -> MemoryConfig {
        MemoryConfig {
            memory_dir: Some(MemoryDirSettings {
                local_path: Some(memory_root.display().to_string()),
                ..Default::default()
            }),
            tape: Some(TapeConfig {
                enabled: Some(tape_enabled),
                only_git: Some(false),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn repository_status_requires_its_own_root_and_tracks_changes() {
        let directory =
            PathBuf::from("/tmp").join(format!("phi-memory-status-{}", uuid::Uuid::new_v4()));
        let mut settings = memory_core::settings_from_config(&isolated_config(&directory, false));
        assert!(MemoryService::repository_is_dirty(&settings).is_err());
        std::fs::create_dir_all(&directory).unwrap();
        assert!(MemoryService::repository_is_dirty(&settings).is_err());
        let output = crate::utils::workspace::git_command(&directory, &["init", "--quiet"])
            .output()
            .unwrap();
        assert!(output.status.success());
        assert!(!MemoryService::repository_is_dirty(&settings).unwrap());
        std::fs::write(directory.join("memory.md"), "memory\n").unwrap();
        assert!(MemoryService::repository_is_dirty(&settings).unwrap());
        std::fs::remove_file(directory.join("memory.md")).unwrap();
        assert!(!MemoryService::repository_is_dirty(&settings).unwrap());
        let nested = directory.join("nested");
        std::fs::create_dir(&nested).unwrap();
        settings.memory_dir.as_mut().unwrap().local_path = Some(nested.display().to_string());
        assert!(MemoryService::repository_is_dirty(&settings).is_err());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn workspace_scopes_lazy_memory_and_tape_persistence() {
        let directory = temp_dir();
        let plain = directory.join("plain");
        let main = directory.join("main");
        let linked = directory.join("linked");
        for root in [&plain, &main] {
            std::fs::create_dir_all(root.join("src")).unwrap();
        }
        for arguments in [
            vec!["init", "--quiet", "--initial-branch=main"],
            vec![
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@localhost",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--quiet",
                "--allow-empty",
                "-m",
                "base",
            ],
            vec![
                "worktree",
                "add",
                "--quiet",
                "-b",
                "linked",
                linked.to_str().unwrap(),
            ],
        ] {
            let output = crate::utils::workspace::git_command(&main, &arguments)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        std::fs::create_dir_all(linked.join("src")).unwrap();
        for (root, project_name, only_git, workspace_id) in [
            (
                &plain,
                "src",
                false,
                Some(format!("directory:{}", plain.join("src").display())),
            ),
            (&plain, "src", true, None),
            (&main, "main", true, Some("git:main".to_owned())),
            (
                &linked,
                "linked",
                true,
                Some("git:worktrees/linked".to_owned()),
            ),
        ] {
            for tape_enabled in [false, true] {
                let memory_root = directory.join("memory");
                let mut config = isolated_config(&memory_root, tape_enabled);
                config.tape.as_mut().unwrap().only_git = Some(only_git);
                let working_dir = root.join("src");
                let project_memory = memory_root.join(project_name);
                let service = MemoryService::create(&config, "session", &working_dir).unwrap();
                assert_eq!(service.memory_project_dir(), project_memory);
                assert_eq!(
                    memory_core::get_memory_dir(
                        &memory_core::settings_from_config(&config),
                        &working_dir
                    )
                    .unwrap(),
                    project_memory
                );
                assert!(service.reconcile().unwrap().nodes.is_empty());
                assert!(service.refresh().unwrap().nodes.is_empty());
                assert!(!memory_root.exists());
                assert_eq!(
                    service.tape().is_some(),
                    tape_enabled && workspace_id.is_some()
                );
                if let Some(tape) = service.tape() {
                    let anchor = tape
                        .create_anchor_without_transcript("design", TapeAnchorType::Normal, None)
                        .unwrap();
                    assert_eq!(
                        anchor.meta.as_ref().unwrap().workspace.as_deref(),
                        workspace_id.as_deref()
                    );
                    assert_eq!(anchor.session_id, "session");
                    assert_eq!(anchor.meta.as_ref().unwrap().git.is_some(), root != &plain);
                    assert!(project_memory.is_dir());
                    let reopened = MemoryService::create(&config, "session", &working_dir).unwrap();
                    let persisted = reopened
                        .tape()
                        .unwrap()
                        .get_anchor_store()
                        .get_all_anchors()
                        .unwrap();
                    assert_eq!(
                        serde_json::to_value(persisted).unwrap(),
                        serde_json::to_value([anchor]).unwrap()
                    );
                    std::fs::remove_dir_all(&memory_root).unwrap();
                }
            }
        }
        let memory_root = directory.join("memory");
        let config = isolated_config(&memory_root, true);
        std::fs::write(plain.join("src/.git"), "gitdir: missing\n").unwrap();
        for working_dir in [plain.join("src"), directory.join("missing")] {
            assert!(MemoryService::create(&config, "session", &working_dir).is_err());
        }
        assert!(!memory_root.exists());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn empty_source_set_removes_materialized_graph() {
        let directory = temp_dir();
        let service = MemoryService::new(&directory, "project").unwrap();

        assert!(!directory.join("project__graph.json").exists());
        assert!(service.reconcile().unwrap().nodes.is_empty());
        assert!(!directory.join("project__graph.json").exists());
        service
            .graph_store
            .save_if_changed(&MemoryGraphRecord::new(memory_graph_sources("project")))
            .unwrap();

        service.refresh().unwrap();
        assert!(!directory.join("project__graph.json").exists());

        memory_file(&directory, "design");
        anchor_file(&directory);
        assert!(service.reconcile().unwrap().nodes.len() > 1);
        std::fs::remove_file(directory.join("project__anchors.jsonl")).unwrap();
        let graph = service.refresh().unwrap();
        assert_eq!(graph.nodes.len(), 1);
        assert!(
            graph
                .nodes
                .iter()
                .all(|node| matches!(node, MemoryGraphNode::MemoryFile { .. }))
        );
        assert!(graph.edges.is_empty());

        std::fs::remove_dir_all(directory.join("core")).unwrap();
        assert!(service.refresh().unwrap().nodes.is_empty());
        assert!(!directory.join("project__graph.json").exists());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn malformed_anchors_preserve_last_materialized_graph() {
        let directory = temp_dir();
        memory_file(&directory, "design");
        anchor_file(&directory);
        let service = MemoryService::new(&directory, "project").unwrap();
        let expected = service.reconcile().unwrap();
        let graph_path = directory.join("project__graph.json");
        let persisted = std::fs::read(&graph_path).unwrap();
        let anchor_path = directory.join("project__anchors.jsonl");
        let valid = std::fs::read_to_string(&anchor_path).unwrap();
        std::fs::write(&anchor_path, format!("{valid}{{invalid}}\n")).unwrap();

        assert!(service.reconcile().is_err());
        assert!(service.refresh().is_err());
        assert_eq!(std::fs::read(&graph_path).unwrap(), persisted);
        std::fs::write(&anchor_path, valid).unwrap();
        assert_eq!(service.reconcile().unwrap(), expected);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn materializes_each_canonical_source_independently() {
        let cases: &[fn(&Path)] = &[|directory| memory_file(directory, "design"), anchor_file];
        for setup in cases {
            let directory = temp_dir();
            setup(&directory);
            let graph = MemoryService::new(&directory, "project")
                .unwrap()
                .reconcile()
                .unwrap();

            assert!(!graph.nodes.is_empty());
            assert!(directory.join("project__graph.json").exists());
            std::fs::remove_dir_all(directory).unwrap();
        }
    }

    #[test]
    fn tape_disabled_still_projects_existing_sources_lazily() {
        let directory = temp_dir();
        let project_root = directory.join("work/project");
        let memory_root = directory.join("memory");
        let project_memory = memory_root.join("project");
        std::fs::create_dir_all(&project_root).unwrap();
        memory_file(&project_memory, "design");
        anchor_file(&project_memory);
        let config = MemoryConfig {
            memory_dir: Some(MemoryDirSettings {
                local_path: Some(memory_root.to_string_lossy().to_string()),
                ..Default::default()
            }),
            tape: Some(TapeConfig {
                enabled: Some(false),
                ..Default::default()
            }),
            ..Default::default()
        };

        let service = MemoryService::create(&config, "session", &project_root).unwrap();
        assert!(service.tape().is_none());
        assert!(!project_memory.join("project__graph.json").exists());

        let graph = service.reconcile().unwrap();
        assert!(
            graph
                .nodes
                .iter()
                .any(|node| matches!(node, MemoryGraphNode::MemoryFile { .. }))
        );
        assert!(
            graph
                .nodes
                .iter()
                .any(|node| matches!(node, MemoryGraphNode::Anchor { .. }))
        );
        assert!(project_memory.join("project__graph.json").exists());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn refreshes_when_a_memory_source_changes() {
        let directory = temp_dir();
        memory_file(&directory, "design");
        let service = MemoryService::new(&directory, "project").unwrap();

        assert_eq!(service.reconcile().unwrap().nodes.len(), 1);
        memory_file(&directory, "architecture");
        assert_eq!(service.reconcile().unwrap().nodes.len(), 2);
        assert!(directory.join("project__graph.json").exists());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn retrieval_ignores_materialization_failure_but_refresh_is_strict() {
        let directory = temp_dir();
        memory_file(&directory, "design");
        std::fs::create_dir_all(directory.join("project__graph.json")).unwrap();
        let service = MemoryService::new(&directory, "project").unwrap();

        assert_eq!(service.graph().unwrap().nodes().count(), 1);
        assert!(service.refresh().is_err());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn recovers_materialized_graph_after_cache_warmup() {
        let directory = temp_dir();
        memory_file(&directory, "design");
        let service = MemoryService::new(&directory, "project").unwrap();
        let graph_path = directory.join("project__graph.json");
        service.reconcile().unwrap();

        std::fs::remove_file(&graph_path).unwrap();
        service.reconcile().unwrap();
        assert!(graph_path.exists());

        std::fs::write(&graph_path, "not json").unwrap();
        assert_eq!(service.reconcile().unwrap().nodes.len(), 1);
        std::fs::remove_dir_all(directory).unwrap();
    }
}
