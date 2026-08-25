//! Lazy, bounded repository intelligence for coding-task context.
//!
//! Discovery deliberately uses the Git index as its source of truth. The map stores paths and
//! bounded metadata, never source contents, and can therefore be refreshed without rereading the
//! repository's source files.

use std::{
    collections::BTreeSet,
    fs::{self, File},
    io::{self, Read},
    path::{Component, Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex},
    time::UNIX_EPOCH,
};

use aether_core::BoundedText;
use blake3::Hasher;
use serde_json::Value;
use thiserror::Error;

use crate::symbol_index::{
    MAX_SYMBOL_FILES, MAX_SYMBOL_LOOKUP_RESULTS, SymbolFile, SymbolIndex, SymbolKind,
    SymbolLanguage, SymbolMatch, SymbolRelationshipKind, SymbolRelationshipMatch,
};

pub const DEFAULT_MAX_REPO_FILES: usize = 4_096;
pub const DEFAULT_MAX_REPO_SPECIAL_FILES: usize = 1_024;
pub const DEFAULT_MAX_REPO_MANIFEST_BYTES: usize = 64 * 1024;
pub const DEFAULT_MAX_REPO_INSTRUCTION_BYTES: usize = 16 * 1024;
pub const DEFAULT_MAX_REPO_README_BYTES: usize = 8 * 1024;
pub const DEFAULT_MAX_REPO_MAP_BYTES: usize = 12 * 1024;
pub const DEFAULT_MAX_REPO_MAP_ITEMS: usize = 64;

/// Limits that keep both the retained map and metadata reads bounded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RepoMapLimits {
    pub max_files: usize,
    pub max_special_files: usize,
    pub max_manifest_bytes: usize,
    pub max_instruction_bytes: usize,
    pub max_readme_bytes: usize,
    pub max_map_bytes: usize,
    pub max_map_items: usize,
}

impl Default for RepoMapLimits {
    fn default() -> Self {
        Self {
            max_files: DEFAULT_MAX_REPO_FILES,
            max_special_files: DEFAULT_MAX_REPO_SPECIAL_FILES,
            max_manifest_bytes: DEFAULT_MAX_REPO_MANIFEST_BYTES,
            max_instruction_bytes: DEFAULT_MAX_REPO_INSTRUCTION_BYTES,
            max_readme_bytes: DEFAULT_MAX_REPO_README_BYTES,
            max_map_bytes: DEFAULT_MAX_REPO_MAP_BYTES,
            max_map_items: DEFAULT_MAX_REPO_MAP_ITEMS,
        }
    }
}

impl RepoMapLimits {
    fn normalized(self) -> Self {
        Self {
            max_files: self.max_files.max(1),
            max_special_files: self.max_special_files.max(1),
            max_manifest_bytes: self.max_manifest_bytes.max(1),
            max_instruction_bytes: self.max_instruction_bytes.max(1),
            max_readme_bytes: self.max_readme_bytes.max(1),
            max_map_bytes: self.max_map_bytes.max(1),
            max_map_items: self.max_map_items.max(1),
        }
    }
}

#[derive(Debug, Error)]
pub enum RepoMapError {
    #[error("repository root is not a directory: {0}")]
    InvalidRoot(PathBuf),
    #[error("failed to execute git ls-files: {0}")]
    Git(#[source] io::Error),
    #[error("git ls-files failed: {0}")]
    GitCommand(String),
    #[error("source path is outside the repository: {0}")]
    InvalidSourcePath(PathBuf),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RepoFileKind {
    Manifest,
    Instruction,
    Documentation,
    Test,
    Source,
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoFile {
    pub path: PathBuf,
    pub kind: RepoFileKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ManifestKind {
    Cargo,
    Node,
    Python,
    Go,
    Java,
    Swift,
    DotNet,
    Php,
    Ruby,
    Elixir,
    Other,
}

impl ManifestKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Cargo => "cargo",
            Self::Node => "node",
            Self::Python => "python",
            Self::Go => "go",
            Self::Java => "java",
            Self::Swift => "swift",
            Self::DotNet => ".net",
            Self::Php => "php",
            Self::Ruby => "ruby",
            Self::Elixir => "elixir",
            Self::Other => "other",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ManifestStatus {
    Parsed,
    Malformed,
    Unreadable,
}

impl ManifestStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Parsed => "parsed",
            Self::Malformed => "malformed",
            Self::Unreadable => "unreadable",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ManifestInfo {
    pub path: PathBuf,
    pub kind: ManifestKind,
    pub package_name: Option<String>,
    pub workspace_members: Vec<String>,
    pub status: ManifestStatus,
    pub truncated: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackageInfo {
    pub name: Option<String>,
    pub root: PathBuf,
    pub manifest: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkspaceMember {
    pub declared_by: PathBuf,
    pub pattern: String,
    pub manifest: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DocumentationKind {
    Readme,
    Markdown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DocumentationFile {
    pub path: PathBuf,
    pub kind: DocumentationKind,
    pub preview: Option<String>,
    pub truncated: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScopedInstruction {
    pub path: PathBuf,
    pub scope: PathBuf,
    pub content: String,
    pub truncated: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RepoSelectionKind {
    File,
    Manifest,
    Package,
    SourceRoot,
    Test,
    Documentation,
    Instruction,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoSelection {
    pub path: PathBuf,
    pub kind: RepoSelectionKind,
    pub score: usize,
    pub symbol: Option<RepoSymbolSelection>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoSymbolSelection {
    pub name: String,
    pub kind: SymbolKind,
    pub line: usize,
    pub container: Option<String>,
}

/// Bounded repository metadata produced by [`RepoMap`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoMapSnapshot {
    pub root: PathBuf,
    pub tracked_file_count: usize,
    pub tracked_files: Vec<RepoFile>,
    pub manifests: Vec<ManifestInfo>,
    pub packages: Vec<PackageInfo>,
    pub workspace_members: Vec<WorkspaceMember>,
    pub source_roots: Vec<PathBuf>,
    pub test_paths: Vec<PathBuf>,
    pub documentation: Vec<DocumentationFile>,
    pub instructions: Vec<ScopedInstruction>,
    pub truncated: bool,
}

impl RepoMapSnapshot {
    /// Return instructions in broad-to-specific order; later entries override earlier ones.
    pub fn instructions_for(&self, path: impl AsRef<Path>) -> Vec<&ScopedInstruction> {
        let Some(relative) = relative_path(&self.root, path.as_ref()) else {
            return Vec::new();
        };
        let mut instructions = self
            .instructions
            .iter()
            .filter(|instruction| scope_contains(&instruction.scope, &relative))
            .collect::<Vec<_>>();
        instructions.sort_by(|left, right| {
            scope_depth(&left.scope)
                .cmp(&scope_depth(&right.scope))
                .then_with(|| left.path.cmp(&right.path))
        });
        instructions
    }

    /// Merge applicable instruction bodies in precedence order, bounded for context use.
    pub fn effective_instructions(&self, path: impl AsRef<Path>, max_bytes: usize) -> String {
        let mut output = String::new();
        for instruction in self.instructions_for(path) {
            push_line(&mut output, format!("# {}", display_path(&instruction.path)));
            output.push_str(&instruction.content);
            if !instruction.content.ends_with('\n') {
                output.push('\n');
            }
        }
        BoundedText::new(output, max_bytes).into_string()
    }

    /// Return a bounded, human-readable representation suitable for model context selection.
    pub fn compact(&self, max_bytes: usize) -> String {
        let mut output = String::new();
        push_line(&mut output, format!("tracked files: {}", self.tracked_file_count));
        if self.truncated {
            push_line(&mut output, "tracked file listing truncated".to_owned());
        }
        push_section(
            &mut output,
            "manifests",
            self.manifests.iter().map(|manifest| {
                let package = manifest
                    .package_name
                    .as_deref()
                    .map_or_else(String::new, |name| format!(" package={name}"));
                let workspace = if manifest.workspace_members.is_empty() {
                    String::new()
                } else {
                    format!(" members={}", manifest.workspace_members.len())
                };
                format!(
                    "{} [{} {}{}{}]",
                    display_path(&manifest.path),
                    manifest.kind.as_str(),
                    manifest.status.as_str(),
                    package,
                    workspace
                )
            }),
        );
        push_section(
            &mut output,
            "packages",
            self.packages.iter().map(|package| {
                let name = package.name.as_deref().unwrap_or("<unnamed>");
                format!("{} ({name})", display_path(&package.root))
            }),
        );
        push_section(
            &mut output,
            "workspace members",
            self.workspace_members.iter().map(|member| {
                let manifest = member
                    .manifest
                    .as_ref()
                    .map_or_else(|| "unresolved".to_owned(), |path| display_path(path));
                format!(
                    "{} pattern={} -> {manifest}",
                    display_path(&member.declared_by),
                    member.pattern
                )
            }),
        );
        push_section(
            &mut output,
            "source roots",
            self.source_roots.iter().map(|path| display_path(path)),
        );
        push_section(&mut output, "tests", self.test_paths.iter().map(|path| display_path(path)));
        push_section(
            &mut output,
            "documentation",
            self.documentation.iter().map(|document| {
                let preview = document
                    .preview
                    .as_deref()
                    .map_or_else(String::new, |line| format!(" — {line}"));
                format!("{}{}", display_path(&document.path), preview)
            }),
        );
        push_section(
            &mut output,
            "instructions (broad to specific)",
            self.instructions.iter().map(|instruction| {
                let preview = instruction
                    .content
                    .lines()
                    .map(str::trim)
                    .find(|line| !line.is_empty())
                    .map_or_else(String::new, |line| format!(" — {}", truncate_preview(line)));
                format!(
                    "{} scope={}{}",
                    display_path(&instruction.path),
                    display_scope(&instruction.scope),
                    preview
                )
            }),
        );
        BoundedText::new(output, max_bytes).into_string()
    }

    /// Select the most relevant retained paths using simple path-token scoring.
    pub fn select(&self, query: &str, limit: usize) -> Vec<RepoSelection> {
        let query =
            query.split_whitespace().map(|token| token.to_ascii_lowercase()).collect::<Vec<_>>();
        let mut selections = Vec::new();
        for file in &self.tracked_files {
            add_selection(&mut selections, &file.path, selection_kind(file.kind), &query);
        }
        for manifest in &self.manifests {
            add_selection(&mut selections, &manifest.path, RepoSelectionKind::Manifest, &query);
        }
        for package in &self.packages {
            add_selection(&mut selections, &package.root, RepoSelectionKind::Package, &query);
        }
        for path in &self.source_roots {
            add_selection(&mut selections, path, RepoSelectionKind::SourceRoot, &query);
        }
        for path in &self.test_paths {
            add_selection(&mut selections, path, RepoSelectionKind::Test, &query);
        }
        for document in &self.documentation {
            add_selection(
                &mut selections,
                &document.path,
                RepoSelectionKind::Documentation,
                &query,
            );
        }
        for instruction in &self.instructions {
            add_selection(
                &mut selections,
                &instruction.path,
                RepoSelectionKind::Instruction,
                &query,
            );
        }
        selections.sort_by(|left, right| {
            right.score.cmp(&left.score).then_with(|| left.path.cmp(&right.path))
        });
        selections.truncate(limit);
        selections
    }

    /// Estimate retained heap-backed bytes for diagnostics and benchmarks.
    pub fn estimated_bytes(&self) -> usize {
        let mut bytes = self.root.as_os_str().len();
        bytes += self
            .tracked_files
            .iter()
            .map(|file| file.path.as_os_str().len() + std::mem::size_of::<RepoFile>())
            .sum::<usize>();
        bytes += self
            .manifests
            .iter()
            .map(|manifest| {
                manifest.path.as_os_str().len()
                    + manifest.package_name.as_ref().map_or(0, String::len)
                    + manifest.workspace_members.iter().map(String::len).sum::<usize>()
            })
            .sum::<usize>();
        bytes += self
            .packages
            .iter()
            .map(|package| {
                package.root.as_os_str().len()
                    + package.manifest.as_os_str().len()
                    + package.name.as_ref().map_or(0, String::len)
            })
            .sum::<usize>();
        bytes += self
            .workspace_members
            .iter()
            .map(|member| {
                member.declared_by.as_os_str().len()
                    + member.pattern.len()
                    + member.manifest.as_ref().map_or(0, |path| path.as_os_str().len())
            })
            .sum::<usize>();
        bytes += self
            .source_roots
            .iter()
            .chain(self.test_paths.iter())
            .map(|path| path.as_os_str().len())
            .sum::<usize>();
        bytes += self
            .documentation
            .iter()
            .map(|document| {
                document.path.as_os_str().len() + document.preview.as_ref().map_or(0, String::len)
            })
            .sum::<usize>();
        bytes += self
            .instructions
            .iter()
            .map(|instruction| {
                instruction.path.as_os_str().len()
                    + instruction.scope.as_os_str().len()
                    + instruction.content.len()
            })
            .sum::<usize>();
        bytes
    }
}

/// A lazy repository map. Constructing this value performs no filesystem or Git I/O.
#[derive(Debug)]
pub struct RepoMap {
    root: PathBuf,
    limits: RepoMapLimits,
    cache: Arc<Mutex<Option<CacheEntry>>>,
    symbols: Arc<Mutex<SymbolIndex>>,
}

impl Clone for RepoMap {
    fn clone(&self) -> Self {
        Self {
            root: self.root.clone(),
            limits: self.limits,
            cache: Arc::clone(&self.cache),
            symbols: Arc::clone(&self.symbols),
        }
    }
}

impl RepoMap {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self::with_limits(root, RepoMapLimits::default())
    }

    pub fn with_limits(root: impl Into<PathBuf>, limits: RepoMapLimits) -> Self {
        Self {
            root: root.into(),
            limits: limits.normalized(),
            cache: Arc::new(Mutex::new(None)),
            symbols: Arc::new(Mutex::new(SymbolIndex::new())),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Discover or return the cached map. Source-file contents never participate in invalidation.
    pub fn snapshot(&self) -> Result<Arc<RepoMapSnapshot>, RepoMapError> {
        let mut cache = self.cache.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(entry) = cache.as_ref()
            && entry.is_current(&self.root, self.limits)
        {
            return Ok(Arc::clone(&entry.snapshot));
        }
        drop(cache);

        let inventory = collect_inventory(&self.root, self.limits)?;
        cache = self.cache.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(entry) = cache.as_ref()
            && entry.fingerprint == inventory.fingerprint
        {
            return Ok(Arc::clone(&entry.snapshot));
        }
        let snapshot = Arc::new(build_snapshot(&self.root, &inventory, self.limits));
        *cache = Some(CacheEntry {
            fingerprint: inventory.fingerprint,
            index_path: inventory.index_path,
            index_stamp: inventory.index_stamp,
            special_stamps: inventory.special_stamps,
            snapshot: Arc::clone(&snapshot),
        });
        Ok(snapshot)
    }

    pub fn discover(&self) -> Result<Arc<RepoMapSnapshot>, RepoMapError> {
        self.snapshot()
    }

    pub fn compact(&self, max_bytes: usize) -> Result<String, RepoMapError> {
        Ok(self.snapshot()?.compact(max_bytes.min(self.limits.max_map_bytes)))
    }

    pub fn select(&self, query: &str, limit: usize) -> Result<Vec<RepoSelection>, RepoMapError> {
        let mut selections = self.snapshot()?.select(query, limit);
        let paths = selections
            .iter()
            .filter(|selection| {
                matches!(selection.kind, RepoSelectionKind::File | RepoSelectionKind::Test)
            })
            .map(|selection| selection.path.clone())
            .collect::<Vec<_>>();
        for symbol in self.lookup_symbols(query, &paths, limit)? {
            let selection = selections.iter_mut().find(|selection| selection.path == symbol.path);
            let symbol_selection = RepoSymbolSelection {
                name: symbol.symbol.name.clone(),
                kind: symbol.symbol.kind,
                line: symbol.symbol.start_line,
                container: symbol.symbol.container.clone(),
            };
            if let Some(selection) = selection {
                selection.score = selection.score.saturating_add(symbol.score);
                if selection.symbol.is_none() {
                    selection.symbol = Some(symbol_selection);
                }
            } else {
                selections.push(RepoSelection {
                    path: symbol.path,
                    kind: RepoSelectionKind::File,
                    score: symbol.score,
                    symbol: Some(symbol_selection),
                });
            }
        }
        for relationship in self.lookup_relationships(query, &paths, limit)? {
            let selection =
                selections.iter_mut().find(|selection| selection.path == relationship.path);
            let score = relationship.score.saturating_sub(relationship.ambiguous as usize * 8);
            if let Some(selection) = selection {
                selection.score = selection.score.saturating_add(score);
                if selection.symbol.is_none()
                    && let Some(symbol) = relationship.symbol.as_ref()
                {
                    selection.symbol = Some(RepoSymbolSelection {
                        name: symbol.name.clone(),
                        kind: symbol.kind,
                        line: symbol.start_line,
                        container: symbol.container.clone(),
                    });
                }
            } else {
                let kind = relationship_selection_kind(&relationship);
                selections.push(RepoSelection {
                    path: relationship.path,
                    kind,
                    score,
                    symbol: relationship.symbol.map(|symbol| RepoSymbolSelection {
                        name: symbol.name,
                        kind: symbol.kind,
                        line: symbol.start_line,
                        container: symbol.container,
                    }),
                });
            }
        }
        selections.sort_by(|left, right| {
            right.score.cmp(&left.score).then_with(|| left.path.cmp(&right.path))
        });
        selections.truncate(limit);
        Ok(selections)
    }

    /// Parse and cache one targeted source file without reading unrelated files.
    pub fn symbols_for_file(
        &self,
        path: impl AsRef<Path>,
    ) -> Result<Arc<SymbolFile>, RepoMapError> {
        self.snapshot()?;
        let Some(relative) = relative_path(&self.root, path.as_ref()) else {
            return Err(RepoMapError::InvalidSourcePath(path.as_ref().to_path_buf()));
        };
        if safe_join(&self.root, &relative).is_none() {
            return Err(RepoMapError::InvalidSourcePath(relative));
        }
        if !is_source_file(&relative) {
            self.symbols
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove_file(&relative);
            return Ok(Arc::new(empty_symbol_file(relative)));
        }
        let Some(source) = read_symbol_source(&self.root, &relative) else {
            self.symbols
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove_file(&relative);
            return Ok(Arc::new(empty_symbol_file(relative)));
        };
        let mut index = self.symbols.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(file) = index.file(&relative)
            && file.content_hash == source.content_hash
            && file.truncated == source.truncated
        {
            return Ok(index.file_arc(&relative).expect("indexed symbol file exists"));
        }
        Ok(index.index_file_hashed(
            relative,
            &source.content,
            source.content_hash,
            source.truncated,
        ))
    }

    /// Look up symbols only in the caller-selected files. An empty path list performs no source
    /// reads, which keeps repository-wide discovery lazy by default.
    pub fn lookup_symbols(
        &self,
        query: &str,
        paths: &[PathBuf],
        limit: usize,
    ) -> Result<Vec<SymbolMatch>, RepoMapError> {
        let mut targeted = Vec::new();
        for path in paths.iter().take(MAX_SYMBOL_FILES) {
            let file = self.symbols_for_file(path)?;
            if file.readable && !file.symbols.is_empty() {
                targeted.push(path.clone());
            }
        }
        let index = self.symbols.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        Ok(index.lookup_in_paths(&targeted, query, limit.min(MAX_SYMBOL_LOOKUP_RESULTS)))
    }

    /// Look up symbols in caller-owned UTF-8 paths without cloning a temporary `PathBuf` list.
    pub(crate) fn lookup_symbols_in_paths(
        &self,
        query: &str,
        paths: &[&str],
        limit: usize,
    ) -> Result<Vec<SymbolMatch>, RepoMapError> {
        for path in paths.iter().take(MAX_SYMBOL_FILES) {
            self.symbols_for_file(path)?;
        }
        let index = self.symbols.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        Ok(index.lookup_in_path_refs(
            paths.iter().take(MAX_SYMBOL_FILES).map(|path| Path::new(*path)),
            query,
            limit.min(MAX_SYMBOL_LOOKUP_RESULTS),
        ))
    }

    /// Look up bounded lexical relationships among the caller-selected and already indexed files.
    /// This never walks the workspace beyond `paths`.
    pub fn lookup_relationships(
        &self,
        query: &str,
        paths: &[PathBuf],
        limit: usize,
    ) -> Result<Vec<SymbolRelationshipMatch>, RepoMapError> {
        for path in paths.iter().take(MAX_SYMBOL_FILES) {
            self.symbols_for_file(path)?;
        }
        let index = self.symbols.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        Ok(index.lookup_relationships(query, limit.min(MAX_SYMBOL_LOOKUP_RESULTS)))
    }

    /// Look up relationships in caller-owned UTF-8 paths without a temporary path vector.
    pub(crate) fn lookup_relationships_in_paths(
        &self,
        query: &str,
        paths: &[&str],
        limit: usize,
    ) -> Result<Vec<SymbolRelationshipMatch>, RepoMapError> {
        for path in paths.iter().take(MAX_SYMBOL_FILES) {
            self.symbols_for_file(path)?;
        }
        let index = self.symbols.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        Ok(index.lookup_relationships(query, limit.min(MAX_SYMBOL_LOOKUP_RESULTS)))
    }

    /// Retained symbol heap estimate for diagnostics.
    pub fn estimated_symbol_bytes(&self) -> usize {
        self.symbols.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).estimated_bytes()
    }

    /// Number of source files currently retained by the ephemeral symbol cache.
    pub fn indexed_symbol_files(&self) -> usize {
        self.symbols.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).len()
    }

    /// Drop repository and symbol caches after a command may have changed workspace state.
    pub fn invalidate(&self) {
        *self.cache.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
        self.symbols.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clear();
    }

    pub fn estimated_bytes(&self) -> Result<usize, RepoMapError> {
        Ok(self.snapshot()?.estimated_bytes())
    }
}

impl CacheEntry {
    fn is_current(&self, root: &Path, limits: RepoMapLimits) -> bool {
        if file_stamp(&self.index_path) != self.index_stamp {
            return false;
        }
        self.special_stamps.iter().all(|expected| {
            let current = read_special_file(root, &expected.path, limits);
            special_digest(&current) == expected.digest
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Fingerprint {
    tracked: [u8; 32],
    special: [u8; 32],
}

#[derive(Debug)]
struct CacheEntry {
    fingerprint: Fingerprint,
    index_path: PathBuf,
    index_stamp: Option<FileStamp>,
    special_stamps: Vec<SpecialStamp>,
    snapshot: Arc<RepoMapSnapshot>,
}

struct Inventory {
    paths: Vec<PathBuf>,
    special: Vec<SpecialFile>,
    index_path: PathBuf,
    index_stamp: Option<FileStamp>,
    special_stamps: Vec<SpecialStamp>,
    fingerprint: Fingerprint,
}

struct SpecialFile {
    path: PathBuf,
    content: Vec<u8>,
    truncated: bool,
    readable: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct FileStamp {
    length: u64,
    modified_nanos: u128,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SpecialStamp {
    path: PathBuf,
    digest: [u8; 32],
}

fn collect_inventory(root: &Path, limits: RepoMapLimits) -> Result<Inventory, RepoMapError> {
    if !root.is_dir() {
        return Err(RepoMapError::InvalidRoot(root.to_path_buf()));
    }
    let index_path = git_index_path(root);
    let (paths, index_stamp) = match read_git_index(&index_path) {
        Ok(index) => index,
        Err(_) => (git_ls_files(root)?, file_stamp(&index_path)),
    };
    let mut tracked_hasher = Hasher::new();
    for path in &paths {
        hash_path(&mut tracked_hasher, path);
    }

    let mut special = Vec::new();
    let mut special_stamps = Vec::new();
    let mut special_hasher = Hasher::new();
    for path in &paths {
        if !is_special_file(path) || special.len() >= limits.max_special_files {
            continue;
        }
        let file = read_special_file(root, path, limits);
        hash_path(&mut special_hasher, path);
        let digest = special_digest(&file);
        special_hasher.update(&digest);
        special_stamps.push(SpecialStamp { path: path.clone(), digest });
        special.push(SpecialFile {
            path: path.clone(),
            content: file.content,
            truncated: file.truncated,
            readable: file.readable,
        });
    }

    Ok(Inventory {
        paths,
        special,
        index_path,
        index_stamp,
        special_stamps,
        fingerprint: Fingerprint {
            tracked: *tracked_hasher.finalize().as_bytes(),
            special: *special_hasher.finalize().as_bytes(),
        },
    })
}

fn read_git_index(index_path: &Path) -> io::Result<(Vec<PathBuf>, Option<FileStamp>)> {
    let mut file = File::open(index_path)?;
    let metadata = file.metadata().ok();
    let mut bytes = Vec::with_capacity(metadata.as_ref().map_or(0, fs::Metadata::len) as usize);
    file.read_to_end(&mut bytes)?;
    let paths = parse_git_index(&bytes)?;
    let stamp = metadata.as_ref().map(file_stamp_from_metadata);
    Ok((paths, stamp))
}

fn parse_git_index(bytes: &[u8]) -> io::Result<Vec<PathBuf>> {
    const HEADER_LEN: usize = 12;
    const ENTRY_LEN: usize = 62;
    const EXTENDED_FLAG: u16 = 0x4000;
    const NAME_LEN_MASK: u16 = 0x0fff;
    const DIRECTORY_MODE: u32 = 0o040000;

    if bytes.len() < HEADER_LEN || &bytes[..4] != b"DIRC" {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid git index header"));
    }
    let version = u32::from_be_bytes(bytes[4..8].try_into().expect("fixed index version"));
    if !matches!(version, 2 | 3) {
        return Err(io::Error::new(io::ErrorKind::Unsupported, "unsupported git index version"));
    }
    let count = u32::from_be_bytes(bytes[8..12].try_into().expect("fixed index count")) as usize;
    let mut paths = Vec::with_capacity(count);
    let mut offset = HEADER_LEN;
    for _ in 0..count {
        let entry_start = offset;
        let fixed = bytes.get(offset..offset + ENTRY_LEN).ok_or_else(|| {
            io::Error::new(io::ErrorKind::UnexpectedEof, "truncated git index entry")
        })?;
        let mode = u32::from_be_bytes(fixed[24..28].try_into().expect("fixed index mode"));
        if mode & 0o170000 == DIRECTORY_MODE {
            return Err(io::Error::new(io::ErrorKind::Unsupported, "sparse git index"));
        }
        let flags = u16::from_be_bytes(fixed[60..62].try_into().expect("fixed index flags"));
        offset += ENTRY_LEN;
        if flags & EXTENDED_FLAG != 0 {
            offset = offset.checked_add(2).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "invalid git index entry")
            })?;
        }
        let declared_len = usize::from(flags & NAME_LEN_MASK);
        let path_end = if declared_len < usize::from(NAME_LEN_MASK) {
            offset.checked_add(declared_len).filter(|end| *end < bytes.len()).ok_or_else(|| {
                io::Error::new(io::ErrorKind::UnexpectedEof, "truncated git index path")
            })?
        } else {
            offset
                + bytes[offset..].iter().position(|byte| *byte == 0).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "unterminated path")
                })?
        };
        if bytes[path_end] != 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid git index path"));
        }
        paths.push(PathBuf::from(String::from_utf8_lossy(&bytes[offset..path_end]).into_owned()));
        offset = path_end + 1;
        let entry_len = offset - entry_start;
        offset += (8 - entry_len % 8) % 8;
    }
    while bytes.len().saturating_sub(offset) > 32 {
        let header = bytes.get(offset..offset + 8).ok_or_else(|| {
            io::Error::new(io::ErrorKind::UnexpectedEof, "truncated git index extension")
        })?;
        let size =
            u32::from_be_bytes(header[4..8].try_into().expect("fixed extension size")) as usize;
        if matches!(&header[..4], b"link" | b"sdir") {
            return Err(io::Error::new(io::ErrorKind::Unsupported, "indirect git index"));
        }
        offset =
            offset.checked_add(8 + size).filter(|end| *end <= bytes.len()).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "invalid git index extension")
            })?;
    }
    Ok(paths)
}

fn git_ls_files(root: &Path) -> Result<Vec<PathBuf>, RepoMapError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["ls-files", "--cached", "-z"])
        .output()
        .map_err(RepoMapError::Git)?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(RepoMapError::GitCommand(if stderr.is_empty() {
            "repository is not indexed by git".to_owned()
        } else {
            stderr
        }));
    }
    Ok(output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| PathBuf::from(String::from_utf8_lossy(path).into_owned()))
        .collect())
}

fn git_index_path(root: &Path) -> PathBuf {
    let dot_git = root.join(".git");
    if dot_git.is_file()
        && let Ok(contents) = fs::read_to_string(&dot_git)
        && let Some(git_dir) = contents.trim().strip_prefix("gitdir:")
    {
        let git_dir = PathBuf::from(git_dir.trim());
        return if git_dir.is_absolute() {
            git_dir.join("index")
        } else {
            dot_git.parent().unwrap_or(root).join(git_dir).join("index")
        };
    }
    dot_git.join("index")
}

fn file_stamp(path: &Path) -> Option<FileStamp> {
    let metadata = fs::metadata(path).ok()?;
    Some(file_stamp_from_metadata(&metadata))
}

fn file_stamp_from_metadata(metadata: &fs::Metadata) -> FileStamp {
    let modified_nanos = metadata
        .modified()
        .ok()
        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |duration| duration.as_nanos());
    FileStamp { length: metadata.len(), modified_nanos }
}

fn special_digest(file: &ReadContent) -> [u8; 32] {
    let mut hasher = Hasher::new();
    hasher.update(&(file.content.len() as u64).to_le_bytes());
    hasher.update(&[u8::from(file.truncated), u8::from(file.readable)]);
    hasher.update(&file.content);
    *hasher.finalize().as_bytes()
}

struct ReadContent {
    content: Vec<u8>,
    truncated: bool,
    readable: bool,
}

fn read_special_file(root: &Path, path: &Path, limits: RepoMapLimits) -> ReadContent {
    let Some(full_path) = safe_join(root, path) else {
        return ReadContent { content: Vec::new(), truncated: false, readable: false };
    };
    let Ok(metadata) = fs::symlink_metadata(&full_path) else {
        return ReadContent { content: Vec::new(), truncated: false, readable: false };
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return ReadContent { content: Vec::new(), truncated: false, readable: false };
    }
    let limit = if is_manifest(path) {
        limits.max_manifest_bytes
    } else if is_instruction(path) {
        limits.max_instruction_bytes
    } else {
        limits.max_readme_bytes
    };
    let Ok(mut file) = File::open(full_path) else {
        return ReadContent { content: Vec::new(), truncated: false, readable: false };
    };
    let mut content = Vec::new();
    let mut limited = (&mut file).take(limit as u64 + 1);
    if limited.read_to_end(&mut content).is_err() {
        return ReadContent { content: Vec::new(), truncated: false, readable: false };
    }
    let truncated = content.len() > limit;
    content.truncate(limit);
    ReadContent { content, truncated, readable: true }
}

struct SymbolSource {
    content: String,
    content_hash: [u8; 32],
    truncated: bool,
}

fn read_symbol_source(root: &Path, path: &Path) -> Option<SymbolSource> {
    let full_path = safe_join(root, path)?;
    let metadata = fs::symlink_metadata(&full_path).ok()?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return None;
    }
    let mut file = File::open(full_path).ok()?;
    let mut hasher = Hasher::new();
    let mut retained = Vec::new();
    let mut buffer = [0u8; 8 * 1024];
    let mut total = 0usize;
    loop {
        let read = file.read(&mut buffer).ok()?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        if retained.len() < crate::symbol_index::MAX_SYMBOL_SOURCE_BYTES {
            let available = crate::symbol_index::MAX_SYMBOL_SOURCE_BYTES - retained.len();
            retained.extend_from_slice(&buffer[..read.min(available)]);
        }
        total = total.saturating_add(read);
    }
    Some(SymbolSource {
        content: String::from_utf8_lossy(&retained).into_owned(),
        content_hash: *hasher.finalize().as_bytes(),
        truncated: total > crate::symbol_index::MAX_SYMBOL_SOURCE_BYTES,
    })
}

fn empty_symbol_file(path: PathBuf) -> SymbolFile {
    SymbolFile {
        language: SymbolLanguage::for_path(&path),
        path,
        content_hash: [0; 32],
        symbols: Vec::new(),
        relationships: Vec::new(),
        truncated: false,
        readable: false,
    }
}

fn build_snapshot(root: &Path, inventory: &Inventory, limits: RepoMapLimits) -> RepoMapSnapshot {
    let mut tracked_files = inventory
        .paths
        .iter()
        .take(limits.max_files)
        .map(|path| RepoFile { path: path.clone(), kind: classify_file(path) })
        .collect::<Vec<_>>();
    tracked_files.sort_by(|left, right| left.path.cmp(&right.path));

    let mut manifests = Vec::new();
    let mut documentation = Vec::new();
    let mut instructions = Vec::new();
    for special in &inventory.special {
        if is_manifest(&special.path) {
            manifests.push(parse_manifest(special));
        }
        if is_readme(&special.path) {
            documentation.push(DocumentationFile {
                path: special.path.clone(),
                kind: DocumentationKind::Readme,
                preview: preview(&special.content),
                truncated: special.truncated,
            });
        }
        if is_instruction(&special.path) {
            instructions.push(ScopedInstruction {
                scope: special.path.parent().map_or_else(PathBuf::new, Path::to_path_buf),
                path: special.path.clone(),
                content: String::from_utf8_lossy(&special.content).into_owned(),
                truncated: special.truncated,
            });
        }
    }
    for path in inventory.paths.iter().filter(|path| is_markdown(path) && !is_readme(path)) {
        documentation.push(DocumentationFile {
            path: path.clone(),
            kind: DocumentationKind::Markdown,
            preview: None,
            truncated: false,
        });
    }
    manifests.sort_by(|left, right| left.path.cmp(&right.path));
    documentation.sort_by(|left, right| left.path.cmp(&right.path));
    instructions.sort_by(|left, right| {
        scope_depth(&left.scope)
            .cmp(&scope_depth(&right.scope))
            .then_with(|| left.path.cmp(&right.path))
    });
    manifests.truncate(limits.max_special_files);
    documentation.truncate(limits.max_special_files);
    instructions.truncate(limits.max_special_files);

    let mut packages = manifests
        .iter()
        .filter_map(|manifest| {
            manifest.package_name.as_ref().map(|name| PackageInfo {
                name: Some(name.clone()),
                root: manifest.path.parent().map_or_else(PathBuf::new, Path::to_path_buf),
                manifest: manifest.path.clone(),
            })
        })
        .collect::<Vec<_>>();
    packages.truncate(limits.max_special_files);

    let manifest_paths = manifests.iter().map(|manifest| manifest.path.clone()).collect::<Vec<_>>();
    let mut workspace_members = Vec::new();
    for manifest in &manifests {
        for pattern in &manifest.workspace_members {
            let member_manifest = manifest_paths.iter().find(|candidate| {
                let relative = candidate.parent().unwrap_or(Path::new("."));
                let base = manifest.path.parent().unwrap_or(Path::new("."));
                let member = relative.strip_prefix(base).unwrap_or(relative);
                wildcard_path_matches(pattern, member)
            });
            workspace_members.push(WorkspaceMember {
                declared_by: manifest.path.clone(),
                pattern: pattern.clone(),
                manifest: member_manifest.cloned(),
            });
        }
    }
    workspace_members.truncate(limits.max_special_files);
    for member in &workspace_members {
        if let Some(manifest) = &member.manifest
            && !packages.iter().any(|package| package.manifest == *manifest)
        {
            let name = manifests
                .iter()
                .find(|candidate| candidate.path == *manifest)
                .and_then(|candidate| candidate.package_name.clone());
            packages.push(PackageInfo {
                name,
                root: manifest.parent().map_or_else(PathBuf::new, Path::to_path_buf),
                manifest: manifest.clone(),
            });
        }
    }
    packages.truncate(limits.max_special_files);

    let mut source_roots = BTreeSet::new();
    let mut test_paths = Vec::new();
    for path in &inventory.paths {
        if is_source_file(path) && !is_test_path(path) {
            source_roots.insert(source_root(path));
        }
        if is_test_path(path) {
            test_paths.push(path.clone());
        }
    }
    let source_roots = source_roots.into_iter().take(limits.max_special_files).collect::<Vec<_>>();
    test_paths.sort();
    test_paths.dedup();
    test_paths.truncate(limits.max_special_files);

    RepoMapSnapshot {
        root: root.to_path_buf(),
        tracked_file_count: inventory.paths.len(),
        tracked_files,
        manifests,
        packages,
        workspace_members,
        source_roots,
        test_paths,
        documentation,
        instructions,
        truncated: inventory.paths.len() > limits.max_files,
    }
}

fn parse_manifest(file: &SpecialFile) -> ManifestInfo {
    let kind = manifest_kind(&file.path);
    let text = String::from_utf8_lossy(&file.content);
    let (package_name, workspace_members, status) = match kind {
        ManifestKind::Node | ManifestKind::Php => parse_package_json(&file.content),
        ManifestKind::Go => parse_go_mod(&text),
        ManifestKind::Cargo | ManifestKind::Python | ManifestKind::Ruby | ManifestKind::Elixir => {
            parse_toml_like(&text, kind)
        }
        ManifestKind::Java | ManifestKind::Swift | ManifestKind::DotNet | ManifestKind::Other => (
            None,
            Vec::new(),
            if file.readable { ManifestStatus::Parsed } else { ManifestStatus::Unreadable },
        ),
    };
    ManifestInfo {
        path: file.path.clone(),
        kind,
        package_name,
        workspace_members,
        status: if !file.readable { ManifestStatus::Unreadable } else { status },
        truncated: file.truncated,
    }
}

fn parse_package_json(content: &[u8]) -> (Option<String>, Vec<String>, ManifestStatus) {
    let Ok(value) = serde_json::from_slice::<Value>(content) else {
        return (None, Vec::new(), ManifestStatus::Malformed);
    };
    let package_name = value.get("name").and_then(Value::as_str).map(str::to_owned);
    let workspace_members = value
        .get("workspaces")
        .and_then(|workspaces| {
            workspaces.as_array().or_else(|| workspaces.get("packages").and_then(Value::as_array))
        })
        .map(|items| items.iter().filter_map(Value::as_str).map(str::to_owned).collect::<Vec<_>>())
        .unwrap_or_default();
    (package_name, workspace_members, ManifestStatus::Parsed)
}

fn parse_go_mod(text: &str) -> (Option<String>, Vec<String>, ManifestStatus) {
    let module = text.lines().find_map(|line| line.trim().strip_prefix("module "));
    (
        module.map(str::trim).filter(|value| !value.is_empty()).map(str::to_owned),
        Vec::new(),
        if module.is_some() { ManifestStatus::Parsed } else { ManifestStatus::Malformed },
    )
}

fn parse_toml_like(
    text: &str,
    kind: ManifestKind,
) -> (Option<String>, Vec<String>, ManifestStatus) {
    let mut section = String::new();
    let mut package_name = None;
    let mut members = Vec::new();
    let mut saw_assignment = false;
    let mut saw_malformed = false;
    let mut pending_array: Option<(String, String)> = None;
    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = pending_array.as_mut() {
            value.push_str(line);
            if line.contains(']') {
                let values = parse_array_values(value);
                if values.is_empty() {
                    saw_malformed = true;
                } else if key == "members" {
                    members.extend(values);
                }
                pending_array = None;
            }
            continue;
        }
        if line.starts_with('[') {
            if !line.ends_with(']') {
                saw_malformed = true;
            } else {
                section = line.trim_matches(['[', ']']).to_ascii_lowercase();
            }
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            saw_malformed = true;
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        if value.starts_with('[') && !value.contains(']') {
            pending_array = Some((key.to_owned(), value.to_owned()));
            continue;
        }
        saw_assignment = true;
        if key == "name"
            && ((section == "package")
                || (kind == ManifestKind::Python && section == "project")
                || section == "tool.poetry")
        {
            package_name = parse_quoted(value);
        }
        if key == "members" && section == "workspace" {
            let values = parse_array_values(value);
            if values.is_empty() {
                saw_malformed = true;
            } else {
                members.extend(values);
            }
        }
    }
    if pending_array.is_some() {
        saw_malformed = true;
    }
    let status = if saw_malformed {
        ManifestStatus::Malformed
    } else if saw_assignment || package_name.is_some() || !members.is_empty() {
        ManifestStatus::Parsed
    } else {
        ManifestStatus::Malformed
    };
    (package_name, members, status)
}

fn parse_array_values(value: &str) -> Vec<String> {
    value
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .split(',')
        .filter_map(parse_quoted)
        .collect()
}

fn parse_quoted(value: &str) -> Option<String> {
    let value = value.trim().trim_end_matches(',').trim();
    if value.len() >= 2
        && ((value.starts_with('"') && value.ends_with('"'))
            || (value.starts_with('\'') && value.ends_with('\'')))
    {
        Some(value[1..value.len() - 1].to_owned())
    } else {
        None
    }
}

fn add_selection(
    selections: &mut Vec<RepoSelection>,
    path: &Path,
    kind: RepoSelectionKind,
    query: &[String],
) {
    let score = path_score(path, query);
    if score == 0 || selections.iter().any(|selection| selection.path == path) {
        return;
    }
    selections.push(RepoSelection { path: path.to_path_buf(), kind, score, symbol: None });
}

fn path_score(path: &Path, query: &[String]) -> usize {
    if query.is_empty() {
        return 1;
    }
    let display = display_path(path).to_ascii_lowercase();
    let file_name = path
        .file_name()
        .map_or_else(String::new, |name| name.to_string_lossy().to_ascii_lowercase());
    query.iter().fold(0, |score, token| {
        score
            + if file_name == *token {
                5
            } else if file_name.contains(token) {
                3
            } else if display.contains(token) {
                1
            } else {
                0
            }
    })
}

fn classify_file(path: &Path) -> RepoFileKind {
    if is_manifest(path) {
        RepoFileKind::Manifest
    } else if is_instruction(path) {
        RepoFileKind::Instruction
    } else if is_readme(path) || is_markdown(path) {
        RepoFileKind::Documentation
    } else if is_test_path(path) {
        RepoFileKind::Test
    } else if is_source_file(path) {
        RepoFileKind::Source
    } else {
        RepoFileKind::Other
    }
}

fn selection_kind(kind: RepoFileKind) -> RepoSelectionKind {
    match kind {
        RepoFileKind::Manifest => RepoSelectionKind::Manifest,
        RepoFileKind::Instruction => RepoSelectionKind::Instruction,
        RepoFileKind::Documentation => RepoSelectionKind::Documentation,
        RepoFileKind::Test => RepoSelectionKind::Test,
        RepoFileKind::Source | RepoFileKind::Other => RepoSelectionKind::File,
    }
}

fn relationship_selection_kind(relationship: &SymbolRelationshipMatch) -> RepoSelectionKind {
    if is_test_path(&relationship.path) || relationship.kind == SymbolRelationshipKind::Test {
        RepoSelectionKind::Test
    } else {
        RepoSelectionKind::File
    }
}

fn manifest_kind(path: &Path) -> ManifestKind {
    let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
        return ManifestKind::Other;
    };
    if name.eq_ignore_ascii_case("cargo.toml") {
        ManifestKind::Cargo
    } else if ["package.json", "deno.json", "deno.jsonc"]
        .iter()
        .any(|candidate| name.eq_ignore_ascii_case(candidate))
    {
        ManifestKind::Node
    } else if ["pyproject.toml", "setup.py", "setup.cfg"]
        .iter()
        .any(|candidate| name.eq_ignore_ascii_case(candidate))
    {
        ManifestKind::Python
    } else if name.eq_ignore_ascii_case("go.mod") {
        ManifestKind::Go
    } else if ["pom.xml", "build.gradle", "build.gradle.kts"]
        .iter()
        .any(|candidate| name.eq_ignore_ascii_case(candidate))
    {
        ManifestKind::Java
    } else if name.eq_ignore_ascii_case("package.swift") {
        ManifestKind::Swift
    } else if name.eq_ignore_ascii_case("composer.json") {
        ManifestKind::Php
    } else if name.eq_ignore_ascii_case("gemfile") || ends_with_ignore_ascii_case(name, ".gemspec")
    {
        ManifestKind::Ruby
    } else if name.eq_ignore_ascii_case("mix.exs") {
        ManifestKind::Elixir
    } else if [".csproj", ".fsproj", ".sln"]
        .iter()
        .any(|suffix| ends_with_ignore_ascii_case(name, suffix))
    {
        ManifestKind::DotNet
    } else {
        ManifestKind::Other
    }
}

fn is_manifest(path: &Path) -> bool {
    !matches!(manifest_kind(path), ManifestKind::Other)
}

fn is_instruction(path: &Path) -> bool {
    path.file_name().is_some_and(|name| {
        matches!(
            name.to_string_lossy().to_ascii_uppercase().as_str(),
            "AGENTS.MD" | "CLAUDE.MD" | "CONTRIBUTING.MD"
        )
    })
}

fn is_readme(path: &Path) -> bool {
    path.file_name().is_some_and(|name| name.to_string_lossy().eq_ignore_ascii_case("README.md"))
}

fn is_markdown(path: &Path) -> bool {
    path.extension().is_some_and(|extension| extension.to_string_lossy().eq_ignore_ascii_case("md"))
}

fn is_source_file(path: &Path) -> bool {
    path.extension().and_then(|value| value.to_str()).is_some_and(|extension| {
        [
            "c", "cc", "cpp", "cs", "ex", "exs", "go", "h", "hpp", "java", "js", "jsx", "kt", "m",
            "mm", "php", "py", "rb", "rs", "swift", "ts", "tsx", "zig",
        ]
        .iter()
        .any(|candidate| extension.eq_ignore_ascii_case(candidate))
    })
}

fn is_test_path(path: &Path) -> bool {
    let has_test_directory = path.components().any(|component| {
        matches!(
            component,
            Component::Normal(value)
                if value.to_str().is_some_and(|value| ["test", "tests", "__tests__"]
                    .iter().any(|candidate| value.eq_ignore_ascii_case(candidate)))
        )
    });
    let display = path.to_string_lossy();
    has_test_directory
        || contains_ignore_ascii_case(&display, ".test.")
        || contains_ignore_ascii_case(&display, ".spec.")
        || ends_with_ignore_ascii_case(&display, "_test.rs")
        || ends_with_ignore_ascii_case(&display, "_test.go")
        || path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| starts_with_ignore_ascii_case(name, "test_"))
}

fn source_root(path: &Path) -> PathBuf {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        if matches!(
            component,
            Component::Normal(value)
                if value.to_str().is_some_and(|value| ["src", "lib", "app", "cmd", "internal"]
                    .iter().any(|candidate| value.eq_ignore_ascii_case(candidate)))
        ) {
            return current;
        }
    }
    path.parent().map_or_else(PathBuf::new, Path::to_path_buf)
}

fn is_special_file(path: &Path) -> bool {
    is_manifest(path) || is_instruction(path) || is_readme(path)
}

fn relative_path(root: &Path, path: &Path) -> Option<PathBuf> {
    if path.is_absolute() {
        path.strip_prefix(root).ok().map(Path::to_path_buf)
    } else {
        Some(path.to_path_buf())
    }
}

fn scope_contains(scope: &Path, path: &Path) -> bool {
    scope.as_os_str().is_empty() || path.starts_with(scope)
}

fn scope_depth(scope: &Path) -> usize {
    scope.components().count()
}

fn safe_join(root: &Path, relative: &Path) -> Option<PathBuf> {
    if relative.is_absolute()
        || relative.components().any(|component| component == Component::ParentDir)
    {
        return None;
    }
    Some(root.join(relative))
}

fn wildcard_path_matches(pattern: &str, path: &Path) -> bool {
    let pattern = pattern.trim_matches('/');
    let path = display_path(path);
    if pattern == path {
        return true;
    }
    if let Some(prefix) = pattern.strip_suffix("/*") {
        let remainder = path.strip_prefix(prefix).unwrap_or("").trim_matches('/');
        return !remainder.is_empty() && !remainder.contains('/');
    }
    simple_wildcard_match(pattern, &path)
}

fn simple_wildcard_match(pattern: &str, value: &str) -> bool {
    let mut parts = pattern.split('*');
    let Some(first) = parts.next() else {
        return false;
    };
    if !value.starts_with(first) {
        return false;
    }
    let mut offset = first.len();
    for part in parts {
        let Some(found) = value[offset..].find(part) else {
            return false;
        };
        offset += found + part.len();
    }
    pattern.ends_with('*') || offset == value.len()
}

fn hash_path(hasher: &mut Hasher, path: &Path) {
    hasher.update(path.as_os_str().as_encoded_bytes());
    hasher.update(&[0]);
}

fn starts_with_ignore_ascii_case(value: &str, prefix: &str) -> bool {
    value.get(..prefix.len()).is_some_and(|head| head.eq_ignore_ascii_case(prefix))
}

fn ends_with_ignore_ascii_case(value: &str, suffix: &str) -> bool {
    value
        .len()
        .checked_sub(suffix.len())
        .and_then(|start| value.get(start..))
        .is_some_and(|tail| tail.eq_ignore_ascii_case(suffix))
}

fn contains_ignore_ascii_case(value: &str, needle: &str) -> bool {
    value
        .as_bytes()
        .windows(needle.len())
        .any(|window| window.eq_ignore_ascii_case(needle.as_bytes()))
}

fn preview(content: &[u8]) -> Option<String> {
    String::from_utf8_lossy(content)
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with('#'))
        .map(truncate_preview)
}

fn truncate_preview(value: &str) -> String {
    let mut preview = value.chars().take(160).collect::<String>();
    if value.chars().count() > 160 {
        preview.push('…');
    }
    preview
}

fn display_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn display_scope(scope: &Path) -> String {
    if scope.as_os_str().is_empty() { ".".to_owned() } else { display_path(scope) }
}

fn push_line(output: &mut String, line: String) {
    output.push_str(&line);
    output.push('\n');
}

fn push_section<'a>(output: &mut String, title: &str, lines: impl Iterator<Item = String> + 'a) {
    let lines = lines.collect::<Vec<_>>();
    if lines.is_empty() {
        return;
    }
    push_line(output, format!("{title}:"));
    for line in lines.iter().take(DEFAULT_MAX_REPO_MAP_ITEMS) {
        push_line(output, format!("- {line}"));
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::Path,
        process::Command,
        sync::atomic::{AtomicUsize, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::*;

    static NEXT_TEMP: AtomicUsize = AtomicUsize::new(0);

    struct TempRepo {
        path: PathBuf,
    }

    impl TempRepo {
        fn new() -> Self {
            let sequence = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
            let nanos = SystemTime::now().duration_since(UNIX_EPOCH).expect("clock").as_nanos();
            let path = std::env::temp_dir().join(format!("aether-repo-map-{nanos}-{sequence}"));
            fs::create_dir_all(&path).expect("temp repo");
            git(&path, ["init", "--quiet"]);
            Self { path }
        }

        fn write(&self, path: &str, content: &str) {
            let full = self.path.join(path);
            if let Some(parent) = full.parent() {
                fs::create_dir_all(parent).expect("parent");
            }
            fs::write(full, content).expect("write");
        }

        fn track(&self, paths: &[&str]) {
            let mut command = Command::new("git");
            command.current_dir(&self.path).arg("add").arg("--");
            for path in paths {
                command.arg(path);
            }
            assert!(command.status().expect("git add").success());
        }
    }

    impl Drop for TempRepo {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn git<const N: usize>(root: &Path, args: [&str; N]) {
        assert!(Command::new("git").current_dir(root).args(args).status().expect("git").success());
    }

    #[test]
    fn monorepo_detects_manifests_packages_source_roots_and_tests() {
        let repo = TempRepo::new();
        repo.write("Cargo.toml", "[workspace]\nmembers = [\"crates/*\"]\n");
        repo.write("crates/one/Cargo.toml", "[package]\nname = \"one\"\nversion = \"0.1.0\"\n");
        repo.write("crates/one/src/lib.rs", "pub fn one() {}\n");
        repo.write("crates/one/tests/one.rs", "#[test]\nfn it_works() {}\n");
        repo.write("package.json", "{\"name\":\"root\",\"workspaces\":[\"packages/*\"]}\n");
        repo.write("packages/web/package.json", "{\"name\":\"web\"}\n");
        repo.write("packages/web/src/index.ts", "export const web = true;\n");
        repo.track(&[
            "Cargo.toml",
            "crates/one/Cargo.toml",
            "crates/one/src/lib.rs",
            "crates/one/tests/one.rs",
            "package.json",
            "packages/web/package.json",
            "packages/web/src/index.ts",
        ]);

        let snapshot = RepoMap::new(&repo.path).snapshot().expect("map");
        assert_eq!(snapshot.tracked_file_count, 7);
        assert_eq!(snapshot.manifests.len(), 4);
        assert!(snapshot.packages.iter().any(|package| package.name.as_deref() == Some("one")));
        assert!(snapshot.source_roots.iter().any(|path| path == Path::new("crates/one/src")));
        assert!(
            snapshot.test_paths.iter().any(|path| path == Path::new("crates/one/tests/one.rs"))
        );
        assert!(snapshot.workspace_members.iter().any(|member| member.pattern == "crates/*"));
    }

    #[test]
    fn nested_instructions_use_scope_precedence() {
        let repo = TempRepo::new();
        repo.write("AGENTS.md", "root instructions\n");
        repo.write("CONTRIBUTING.md", "contributing instructions\n");
        repo.write("crates/AGENTS.md", "crate instructions\n");
        repo.write("crates/foo/CLAUDE.md", "foo instructions\n");
        repo.write("crates/foo/src/lib.rs", "pub fn foo() {}\n");
        repo.track(&[
            "AGENTS.md",
            "CONTRIBUTING.md",
            "crates/AGENTS.md",
            "crates/foo/CLAUDE.md",
            "crates/foo/src/lib.rs",
        ]);
        let snapshot = RepoMap::new(&repo.path).snapshot().expect("map");
        let paths = snapshot
            .instructions_for("crates/foo/src/lib.rs")
            .into_iter()
            .map(|instruction| instruction.path.clone())
            .collect::<Vec<_>>();
        assert_eq!(
            paths,
            vec![
                PathBuf::from("AGENTS.md"),
                PathBuf::from("CONTRIBUTING.md"),
                PathBuf::from("crates/AGENTS.md"),
                PathBuf::from("crates/foo/CLAUDE.md")
            ]
        );
        assert_eq!(snapshot.instructions_for("docs/guide.md").len(), 2);
        let effective = snapshot.effective_instructions("crates/foo/src/lib.rs", 512);
        assert!(effective.contains("root instructions"));
        assert!(effective.ends_with("foo instructions\n"));
    }

    #[test]
    fn ignored_and_untracked_files_do_not_enter_or_invalidate_map() {
        let repo = TempRepo::new();
        repo.write(".gitignore", "ignored.txt\n");
        repo.write("src/lib.rs", "pub fn stable() {}\n");
        repo.track(&[".gitignore", "src/lib.rs"]);
        let map = RepoMap::new(&repo.path);
        let first = map.snapshot().expect("map");
        repo.write("ignored.txt", "ignored\n");
        repo.write("untracked.rs", "pub fn no() {}\n");
        let second = map.snapshot().expect("map");
        assert!(second.tracked_files.iter().all(|file| file.path != Path::new("ignored.txt")));
        assert!(second.tracked_files.iter().all(|file| file.path != Path::new("untracked.rs")));
        assert!(Arc::ptr_eq(&first, &second));
    }

    #[test]
    fn unsupported_index_versions_fall_back_to_git() {
        let repo = TempRepo::new();
        git(&repo.path, ["config", "index.version", "4"]);
        repo.write("src/lib.rs", "pub fn stable() {}\n");
        repo.track(&["src/lib.rs"]);

        let snapshot = RepoMap::new(&repo.path).snapshot().expect("map");

        assert_eq!(snapshot.tracked_file_count, 1);
        assert_eq!(snapshot.tracked_files[0].path, Path::new("src/lib.rs"));
    }

    #[test]
    fn malformed_manifest_is_retained_without_failing_discovery() {
        let repo = TempRepo::new();
        repo.write("package.json", "{ not valid json\n");
        repo.write("Cargo.toml", "[package\nname = \"broken\"\n");
        repo.track(&["package.json", "Cargo.toml"]);
        let snapshot = RepoMap::new(&repo.path).snapshot().expect("map");
        assert_eq!(snapshot.manifests.len(), 2);
        assert!(
            snapshot.manifests.iter().all(|manifest| manifest.status == ManifestStatus::Malformed)
        );
    }

    #[test]
    fn large_repository_is_bounded_and_reports_truncation() {
        let repo = TempRepo::new();
        let mut paths = Vec::new();
        for index in 0..1_500 {
            let path = format!("src/generated/file-{index}.rs");
            repo.write(&path, "pub fn generated() {}\n");
            paths.push(path);
        }
        let path_refs = paths.iter().map(String::as_str).collect::<Vec<_>>();
        repo.track(&path_refs);
        let limits = RepoMapLimits { max_files: 128, ..RepoMapLimits::default() };
        let snapshot = RepoMap::with_limits(&repo.path, limits).snapshot().expect("map");
        assert_eq!(snapshot.tracked_file_count, 1_500);
        assert_eq!(snapshot.tracked_files.len(), 128);
        assert!(snapshot.truncated);
        assert!(snapshot.estimated_bytes() < 100_000);
    }

    #[test]
    fn relevant_metadata_changes_invalidate_but_source_changes_do_not() {
        let repo = TempRepo::new();
        repo.write("AGENTS.md", "old\n");
        repo.write("src/lib.rs", "old source\n");
        repo.track(&["AGENTS.md", "src/lib.rs"]);
        let map = RepoMap::new(&repo.path);
        let first = map.snapshot().expect("map");
        repo.write("src/lib.rs", "new source with a different size\n");
        let source_changed = map.snapshot().expect("map");
        assert!(Arc::ptr_eq(&first, &source_changed));
        repo.write("AGENTS.md", "new instructions\n");
        let instruction_changed = map.snapshot().expect("map");
        assert!(!Arc::ptr_eq(&source_changed, &instruction_changed));
        assert_eq!(instruction_changed.instructions[0].content.trim(), "new instructions");
        repo.write("src/new.rs", "pub fn new_file() {}\n");
        repo.track(&["src/new.rs"]);
        let tracked_structure_changed = map.snapshot().expect("map");
        assert!(!Arc::ptr_eq(&instruction_changed, &tracked_structure_changed));
        assert!(
            tracked_structure_changed
                .tracked_files
                .iter()
                .any(|file| file.path == Path::new("src/new.rs"))
        );
    }

    #[test]
    fn compact_output_and_selection_are_bounded() {
        let repo = TempRepo::new();
        repo.write("README.md", "# Project\nA useful repository.\n");
        repo.write("src/feature.rs", "pub fn feature() {}\n");
        repo.write("tests/feature_test.rs", "#[test]\nfn feature() {}\n");
        repo.track(&["README.md", "src/feature.rs", "tests/feature_test.rs"]);
        let snapshot = RepoMap::new(&repo.path).snapshot().expect("map");
        assert!(snapshot.compact(128).len() <= 128);
        assert!(snapshot.select("feature", 2).len() <= 2);
        assert!(
            snapshot
                .select("feature", 2)
                .iter()
                .any(|selection| selection.path == Path::new("src/feature.rs"))
        );
    }

    #[test]
    fn targeted_symbol_lookup_is_lazy_and_invalidates_on_content_hash_change() {
        let repo = TempRepo::new();
        repo.write("src/target.rs", "pub fn target_symbol() {}\n");
        repo.write("src/unrelated.rs", "pub fn unrelated_symbol() {}\n");
        repo.track(&["src/target.rs", "src/unrelated.rs"]);

        let map = RepoMap::new(&repo.path);
        let target = map.symbols_for_file("src/target.rs").expect("target symbols");
        assert!(target.symbols.iter().any(|symbol| symbol.name == "target_symbol"));
        assert_eq!(map.indexed_symbol_files(), 1);
        assert!(map.lookup_symbols("unrelated_symbol", &[], 4).unwrap().is_empty());
        assert_eq!(map.indexed_symbol_files(), 1);

        repo.write("src/target.rs", "pub fn replacement_symbol() {}\n");
        let replacement = map.symbols_for_file("src/target.rs").expect("replacement symbols");
        assert_ne!(target.content_hash, replacement.content_hash);
        assert!(replacement.symbols.iter().any(|symbol| symbol.name == "replacement_symbol"));
        assert!(replacement.symbols.iter().all(|symbol| symbol.name != "target_symbol"));
        fs::remove_file(repo.path.join("src/target.rs")).expect("remove target");
        let missing = map.symbols_for_file("src/target.rs").expect("missing symbols");
        assert!(!missing.readable);
        assert_eq!(map.indexed_symbol_files(), 0);
    }

    #[test]
    fn targeted_relationship_lookup_indexes_only_participants_and_ranks_source_test_edges() {
        let repo = TempRepo::new();
        repo.write("src/widget.rs", "pub struct Widget;\n");
        repo.write("src/use.rs", "use crate::widget::Widget as ImportedWidget;\n");
        repo.write("tests/widget_test.rs", "#[test]\nfn widget() { Widget; }\n");
        repo.write("src/unrelated.rs", "pub fn unrelated() {}\n");
        repo.track(&["src/widget.rs", "src/use.rs", "tests/widget_test.rs", "src/unrelated.rs"]);
        let map = RepoMap::new(&repo.path);
        let paths = vec![
            PathBuf::from("src/widget.rs"),
            PathBuf::from("src/use.rs"),
            PathBuf::from("tests/widget_test.rs"),
        ];
        let matches = map.lookup_relationships("Widget", &paths, 16).expect("relationships");
        assert_eq!(map.indexed_symbol_files(), 3);
        assert_eq!(matches[0].kind, crate::symbol_index::SymbolRelationshipKind::Definition);
        assert_eq!(matches[0].path, Path::new("src/widget.rs"));
        assert!(matches.iter().any(|hit| {
            hit.kind == crate::symbol_index::SymbolRelationshipKind::Dependency
                && hit.path == Path::new("src/use.rs")
        }));
        assert!(matches.iter().any(|hit| {
            hit.kind == crate::symbol_index::SymbolRelationshipKind::Test
                && hit.path == Path::new("tests/widget_test.rs")
        }));
        assert!(!matches.iter().any(|hit| hit.path == Path::new("src/unrelated.rs")));
    }

    #[test]
    fn repo_map_selection_includes_targeted_symbol_metadata() {
        let repo = TempRepo::new();
        repo.write("src/target.rs", "pub fn target_symbol() {}\n");
        repo.track(&["src/target.rs"]);
        let selection = RepoMap::new(&repo.path).select("target", 4).expect("selection");
        assert_eq!(selection[0].path, Path::new("src/target.rs"));
        assert_eq!(
            selection[0].symbol.as_ref().map(|symbol| symbol.name.as_str()),
            Some("target_symbol")
        );
    }
}
