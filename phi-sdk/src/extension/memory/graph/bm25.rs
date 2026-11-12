use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex};

use bm25::{Document, SearchEngine, SearchEngineBuilder, Tokenizer};
use jieba_rs::Jieba;

use crate::extension::memory::memory_core;

#[derive(Debug, Clone)]
pub struct Bm25MemorySource<S> {
    pub file_path: PathBuf,
    pub scope: S,
}

#[derive(Debug, Clone)]
pub struct Bm25MemoryResult<S> {
    pub path: PathBuf,
    pub scope: S,
    pub score: f64,
}

#[derive(Debug, Clone)]
struct JiebaTokenizer {
    jieba: Arc<Jieba>,
}

impl Default for JiebaTokenizer {
    fn default() -> Self {
        Self {
            jieba: {
                static JIEBA: LazyLock<Arc<Jieba>> = LazyLock::new(|| Arc::new(Jieba::new()));
                JIEBA.clone()
            },
        }
    }
}

impl Tokenizer for JiebaTokenizer {
    fn tokenize(&self, input_text: &str) -> Vec<String> {
        self.jieba
            .cut(input_text, true)
            .into_iter()
            .map(|token| token.word.to_lowercase())
            .filter(|token| !token.trim().is_empty())
            .collect()
    }
}

type MemoryFieldEngine = SearchEngine<String, u32, JiebaTokenizer>;

#[derive(Default)]
pub struct Bm25MemorySearch {
    cache: Mutex<Option<MemorySearchIndex>>,
}

struct MemorySearchIndex {
    docs: Vec<MemoryFileFields>,
    fields: Vec<(f64, MemoryFieldEngine)>,
}

impl Bm25MemorySearch {
    pub fn search<S: Clone>(
        &self,
        sources: &[Bm25MemorySource<S>],
        query: &str,
        limit: usize,
    ) -> Vec<Bm25MemoryResult<S>> {
        if query.trim().is_empty() || limit == 0 {
            return Vec::new();
        }
        let docs = sources
            .iter()
            .filter_map(|source| memory_file_fields(&source.file_path).ok())
            .collect::<Vec<_>>();
        let mut cache = self.cache.lock().unwrap_or_else(|error| error.into_inner());
        let index = match cache.as_mut() {
            Some(index) if index.docs == docs => index,
            _ => cache.insert(MemorySearchIndex::new(docs)),
        };
        let mut scores = HashMap::<String, f64>::new();
        for (boost, engine) in &index.fields {
            for result in engine.search(query, limit) {
                *scores.entry(result.document.id).or_default() += f64::from(result.score) * boost;
            }
        }
        let source_map = sources
            .iter()
            .map(|source| (source.file_path.display().to_string(), source))
            .collect::<HashMap<_, _>>();
        let mut ranked = scores.into_iter().collect::<Vec<_>>();
        ranked.sort_by(|left, right| {
            right
                .1
                .partial_cmp(&left.1)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        ranked
            .into_iter()
            .take(limit)
            .filter_map(|(id, score)| {
                source_map.get(&id).map(|source| Bm25MemoryResult {
                    path: source.file_path.clone(),
                    scope: source.scope.clone(),
                    score,
                })
            })
            .collect()
    }
}

impl MemorySearchIndex {
    fn new(docs: Vec<MemoryFileFields>) -> Self {
        let tokenizer = JiebaTokenizer::default();
        let fields = [
            (
                3.0,
                build_memory_field(&docs, tokenizer.clone(), |doc| doc.title.clone()),
            ),
            (
                2.0,
                build_memory_field(&docs, tokenizer.clone(), |doc| doc.tags.clone()),
            ),
            (
                2.0,
                build_memory_field(&docs, tokenizer.clone(), |doc| doc.description.clone()),
            ),
            (
                1.0,
                build_memory_field(&docs, tokenizer, |doc| doc.content.clone()),
            ),
        ]
        .into_iter()
        .filter_map(|(boost, engine)| engine.map(|engine| (boost, engine)))
        .collect();
        Self { docs, fields }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct MemoryFileFields {
    id: String,
    title: String,
    tags: String,
    description: String,
    content: String,
}

fn memory_file_fields(file_path: &std::path::Path) -> anyhow::Result<MemoryFileFields> {
    let memory = memory_core::read_memory_file(file_path)?;
    let title = file_path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or_default()
        .to_string();
    Ok(MemoryFileFields {
        id: file_path.display().to_string(),
        title,
        tags: memory.frontmatter.tags.join(" "),
        description: memory.frontmatter.description,
        content: memory.content,
    })
}

fn build_memory_field(
    docs: &[MemoryFileFields],
    tokenizer: JiebaTokenizer,
    field: impl Fn(&MemoryFileFields) -> String,
) -> Option<MemoryFieldEngine> {
    let field_docs = docs
        .iter()
        .filter_map(|doc| {
            let content = field(doc);
            (!content.trim().is_empty()).then(|| Document::new(doc.id.clone(), content))
        })
        .collect::<Vec<_>>();
    (!field_docs.is_empty())
        .then(|| SearchEngineBuilder::with_tokenizer_and_documents(tokenizer, field_docs).build())
}

#[cfg(test)]
mod tests {
    use super::*;

    proptest::proptest! {
        #![proptest_config(proptest::test_runner::Config::with_cases(16))]
        #[test]
        fn cached_search_matches_fresh_after_source_changes(
            changes in proptest::collection::vec((0_usize..4, 0_usize..4), 1..16)
        ) {
            let directory = PathBuf::from("/tmp").join(format!("phi-bm25-changes-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&directory).unwrap();
            let search = Bm25MemorySearch::default();
            for (file, change) in changes {
                let path = directory.join(format!("memory-{file}.md"));
                match change {
                    0 => { let _ = std::fs::remove_file(&path); }
                    1 => std::fs::write(&path, "needle").unwrap(),
                    2 => std::fs::write(&path, "absent").unwrap(),
                    _ => std::fs::write(&path, "---\ndescription: needle\ntags: [检索]\n---\n中文记忆").unwrap(),
                }
                let sources = memory_core::list_memory_files(&directory).unwrap().into_iter()
                    .map(|file_path| Bm25MemorySource { file_path, scope: change }).collect::<Vec<_>>();
                for selected in [&sources[..], &sources[..sources.len().min(1)]] {
                    for query in ["needle", "记忆", "检索", "absent"] {
                        let normalize = |results: Vec<Bm25MemoryResult<usize>>| {
                            let mut values = results.into_iter().map(|result| (result.path, result.scope, result.score)).collect::<Vec<_>>();
                            values.sort_by(|left, right| left.0.cmp(&right.0));
                            values
                        };
                        let actual = normalize(search.search(selected, query, 20));
                        let expected = normalize(Bm25MemorySearch::default().search(selected, query, 20));
                        proptest::prop_assert_eq!(actual, expected);
                    }
                }
            }
            std::fs::remove_dir_all(directory).unwrap();
        }
    }

    #[test]
    fn memory_fields_keep_independent_weights_and_mixed_language_search() {
        let directory = PathBuf::from("/tmp").join(format!("phi-bm25-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let fields = [
            ("architecture.md", "---\ndescription: other\n---\nother"),
            (
                "tags.md",
                "---\ndescription: other\ntags: [architecture]\n---\nother",
            ),
            (
                "description.md",
                "---\ndescription: architecture\n---\nother",
            ),
            ("body.md", "architecture"),
            ("mixed.md", "中文记忆检索 architecture"),
        ];
        let sources = fields.map(|(name, content)| {
            let file_path = directory.join(name);
            std::fs::write(&file_path, content).unwrap();
            Bm25MemorySource {
                file_path,
                scope: (),
            }
        });
        let search = Bm25MemorySearch::default();
        let results = search.search(&sources, "architecture", 20);
        let score = |name| {
            results
                .iter()
                .find(|result| result.path.ends_with(name))
                .unwrap()
                .score
        };
        let field_score = |contents: &[&str], target: usize| {
            let documents = contents
                .iter()
                .enumerate()
                .map(|(id, text)| Document::new(id, *text))
                .collect::<Vec<_>>();
            let engine: SearchEngine<usize, u32, JiebaTokenizer> =
                SearchEngineBuilder::with_tokenizer_and_documents(
                    JiebaTokenizer::default(),
                    documents,
                )
                .build();
            f64::from(
                engine
                    .search("architecture", 20)
                    .into_iter()
                    .find(|result| result.document.id == target)
                    .unwrap()
                    .score,
            )
        };
        assert_eq!(
            score("architecture.md"),
            3.0 * field_score(&["architecture", "tags", "description", "body", "mixed"], 0)
        );
        assert_eq!(score("tags.md"), 2.0 * field_score(&["architecture"], 0));
        assert_eq!(
            score("description.md"),
            2.0 * field_score(&["other", "other", "architecture"], 2)
        );
        assert_eq!(
            score("body.md"),
            field_score(
                &[
                    "other",
                    "other",
                    "other",
                    "architecture",
                    "中文记忆检索 architecture"
                ],
                3
            )
        );
        let snapshot = search.cache.lock().unwrap().as_ref().unwrap().docs.as_ptr();
        let chinese = search.search(&sources, "记忆检索", 20);
        assert_eq!(
            snapshot,
            search.cache.lock().unwrap().as_ref().unwrap().docs.as_ptr()
        );
        assert_eq!(chinese.len(), 1);
        assert!(chinese[0].path.ends_with("mixed.md"));
        assert!(search.search(&sources, " ", 20).is_empty());
        assert!(search.search(&sources, "architecture", 0).is_empty());
        std::fs::remove_dir_all(directory).unwrap();
    }
}
