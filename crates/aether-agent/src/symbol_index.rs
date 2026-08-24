//! Bounded, lexical symbol navigation for source files.
//!
//! This module intentionally does not try to be a Rust parser. It masks comments and literals,
//! then uses a small token/state machine to find declarations, imports, test attributes, and
//! containment. That keeps navigation useful on incomplete files and leaves room for additional
//! language parsers without adding a parser dependency.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use blake3::Hasher;

pub const MAX_SYMBOL_SOURCE_BYTES: usize = 512 * 1024;
pub const MAX_SYMBOL_FILES: usize = 128;
pub const MAX_SYMBOLS_PER_FILE: usize = 512;
pub const MAX_SYMBOL_RELATIONSHIPS_PER_FILE: usize = 256;
pub const MAX_SYMBOL_LOOKUP_RESULTS: usize = 128;
pub const MAX_SYMBOL_RELATIONSHIP_FANOUT: usize = 32;
const MAX_SYMBOL_TOKENS: usize = 32 * 1024;
const MAX_SYMBOL_NAME_BYTES: usize = 128;

/// Languages recognized by the navigation layer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SymbolLanguage {
    Rust,
    TypeScript,
    Python,
    Unsupported,
}

impl SymbolLanguage {
    #[must_use]
    pub fn for_path(path: &Path) -> Self {
        match path.extension().and_then(|extension| extension.to_str()) {
            Some("rs") => Self::Rust,
            Some("ts" | "tsx" | "js" | "jsx") => Self::TypeScript,
            Some("py") => Self::Python,
            _ => Self::Unsupported,
        }
    }
}

/// A navigation symbol extracted from one source file.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Symbol {
    pub name: String,
    pub kind: SymbolKind,
    pub start_line: usize,
    pub end_line: usize,
    pub container: Option<String>,
    pub is_test: bool,
}

/// The declaration categories used by repository navigation.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum SymbolKind {
    Function,
    Method,
    Struct,
    Enum,
    Trait,
    TypeAlias,
    Module,
    Test,
    Import,
}

impl SymbolKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Function => "function",
            Self::Method => "method",
            Self::Struct => "struct",
            Self::Enum => "enum",
            Self::Trait => "trait",
            Self::TypeAlias => "type",
            Self::Module => "module",
            Self::Test => "test",
            Self::Import => "import",
        }
    }
}

/// A small relationship between symbols in one indexed file.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SymbolRelation {
    pub from: String,
    pub to: String,
    pub kind: SymbolRelationKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SymbolRelationKind {
    Contains,
    Imports,
    References,
    Implements,
}

/// All bounded symbol data retained for one source file.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SymbolFile {
    pub path: PathBuf,
    pub language: SymbolLanguage,
    pub content_hash: [u8; 32],
    pub symbols: Vec<Symbol>,
    pub relationships: Vec<SymbolRelation>,
    pub truncated: bool,
    pub readable: bool,
}

impl SymbolFile {
    #[must_use]
    pub fn lookup(&self, query: &str, limit: usize) -> Vec<SymbolMatch> {
        lookup_files(std::slice::from_ref(self), query, limit)
    }
}

/// A ranked symbol result with its owning file.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SymbolMatch {
    pub path: PathBuf,
    pub symbol: Symbol,
    pub score: usize,
}

/// The bounded, lexical roles used by cross-file relationship lookup.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum SymbolRelationshipKind {
    Definition,
    Caller,
    Implementation,
    Test,
    Dependency,
    Import,
    Module,
}

/// A ranked cross-file relationship candidate.
///
/// `ambiguous` is true when more than one lexical definition can explain the query. The
/// relationship is intentionally still returned, but callers must not treat it as semantic
/// resolution.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SymbolRelationshipMatch {
    pub path: PathBuf,
    pub symbol: Option<Symbol>,
    pub kind: SymbolRelationshipKind,
    pub score: usize,
    pub ambiguous: bool,
}

/// Backwards-friendly name for a ranked relationship result.
pub type SymbolRelationship = SymbolRelationshipMatch;

#[derive(Clone, Debug, Eq, PartialEq)]
struct RelationshipEvidence {
    path: PathBuf,
    symbol_index: Option<usize>,
    kind: SymbolRelationshipKind,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RelationshipContribution {
    key: String,
    evidence: RelationshipEvidence,
}

#[derive(Clone, Debug, Default)]
struct RelationshipIndex {
    by_key: BTreeMap<String, Vec<RelationshipEvidence>>,
    contributions: BTreeMap<PathBuf, Vec<RelationshipContribution>>,
    source_by_stem: BTreeMap<String, Vec<PathBuf>>,
    test_by_stem: BTreeMap<String, Vec<PathBuf>>,
    stems_by_path: BTreeMap<PathBuf, (bool, String)>,
}

impl RelationshipIndex {
    fn insert_file(&mut self, file: &SymbolFile) {
        self.remove_file(&file.path);
        let mut contributions = Vec::new();
        let test_file =
            is_test_path(&file.path) || file.symbols.iter().any(|symbol| symbol.is_test);
        if let Some(stem) = path_stem(&file.path) {
            let stems = if test_file { &mut self.test_by_stem } else { &mut self.source_by_stem };
            insert_path(stems, stem, file.path.clone());
            self.stems_by_path.insert(file.path.clone(), (test_file, stem.to_owned()));
        }

        for (symbol_index, symbol) in file.symbols.iter().enumerate() {
            if symbol.kind == SymbolKind::Import {
                continue;
            }
            let kind = if symbol.kind == SymbolKind::Module {
                SymbolRelationshipKind::Module
            } else if symbol.is_test {
                SymbolRelationshipKind::Test
            } else {
                SymbolRelationshipKind::Definition
            };
            add_contribution(
                &mut self.by_key,
                &mut contributions,
                symbol_key(&symbol.name),
                RelationshipEvidence {
                    path: file.path.clone(),
                    symbol_index: Some(symbol_index),
                    kind,
                },
            );
            if symbol.kind == SymbolKind::Method
                && let Some(container) = symbol.container.as_deref()
            {
                add_contribution(
                    &mut self.by_key,
                    &mut contributions,
                    symbol_key(container),
                    RelationshipEvidence {
                        path: file.path.clone(),
                        symbol_index: Some(symbol_index),
                        kind: SymbolRelationshipKind::Implementation,
                    },
                );
            }
        }

        for relation in &file.relationships {
            match relation.kind {
                SymbolRelationKind::References => {
                    let owner = file.symbols.iter().position(|symbol| symbol.name == relation.from);
                    let kind =
                        if test_file || owner.is_some_and(|index| file.symbols[index].is_test) {
                            SymbolRelationshipKind::Test
                        } else {
                            SymbolRelationshipKind::Caller
                        };
                    add_contribution(
                        &mut self.by_key,
                        &mut contributions,
                        symbol_key(&relation.to),
                        RelationshipEvidence { path: file.path.clone(), symbol_index: owner, kind },
                    );
                }
                SymbolRelationKind::Implements => {
                    let method =
                        file.symbols.iter().position(|symbol| symbol.name == relation.from);
                    add_contribution(
                        &mut self.by_key,
                        &mut contributions,
                        symbol_key(&relation.to),
                        RelationshipEvidence {
                            path: file.path.clone(),
                            symbol_index: method,
                            kind: SymbolRelationshipKind::Implementation,
                        },
                    );
                }
                SymbolRelationKind::Imports => {
                    let import =
                        file.symbols.iter().position(|symbol| symbol.name == relation.from);
                    let target_key = symbol_key(&relation.to);
                    add_contribution(
                        &mut self.by_key,
                        &mut contributions,
                        target_key.clone(),
                        RelationshipEvidence {
                            path: file.path.clone(),
                            symbol_index: import,
                            kind: SymbolRelationshipKind::Dependency,
                        },
                    );
                    if let Some(last) = relation.to.rsplit("::").find(|part| !part.is_empty()) {
                        let last_key = symbol_key(last);
                        if last_key != target_key {
                            add_contribution(
                                &mut self.by_key,
                                &mut contributions,
                                last_key,
                                RelationshipEvidence {
                                    path: file.path.clone(),
                                    symbol_index: import,
                                    kind: SymbolRelationshipKind::Dependency,
                                },
                            );
                        }
                    }
                    if let Some(import) = import {
                        add_contribution(
                            &mut self.by_key,
                            &mut contributions,
                            symbol_key(&file.symbols[import].name),
                            RelationshipEvidence {
                                path: file.path.clone(),
                                symbol_index: Some(import),
                                kind: SymbolRelationshipKind::Import,
                            },
                        );
                    }
                }
                SymbolRelationKind::Contains => {}
            }
        }
        self.contributions.insert(file.path.clone(), contributions);
    }

    fn remove_file(&mut self, path: &Path) {
        if let Some(contributions) = self.contributions.remove(path) {
            for contribution in contributions {
                remove_evidence(&mut self.by_key, &contribution.key, &contribution.evidence);
            }
        }
        if let Some((test_file, stem)) = self.stems_by_path.remove(path) {
            let stems = if test_file { &mut self.test_by_stem } else { &mut self.source_by_stem };
            remove_path_from_stem(stems, &stem, path);
        }
    }

    fn lookup(
        &self,
        files: &BTreeMap<PathBuf, SymbolFile>,
        query: &str,
        limit: usize,
    ) -> Vec<SymbolRelationshipMatch> {
        let limit = limit.min(MAX_SYMBOL_LOOKUP_RESULTS);
        if limit == 0 || query.trim().is_empty() {
            return Vec::new();
        }
        let keys = query
            .split(|character: char| !character.is_ascii_alphanumeric() && character != '_')
            .filter(|term| term.len() > 1)
            .take(16)
            .map(symbol_key)
            .collect::<Vec<_>>();
        if keys.is_empty() {
            return Vec::new();
        }
        let definition_count = keys
            .iter()
            .flat_map(|key| self.by_key.get(key).into_iter().flatten())
            .filter(|evidence| {
                matches!(
                    evidence.kind,
                    SymbolRelationshipKind::Definition | SymbolRelationshipKind::Module
                )
            })
            .map(|evidence| (&evidence.path, evidence.symbol_index))
            .collect::<std::collections::BTreeSet<_>>()
            .len();
        let mut ranked = Vec::new();
        for (key_index, key) in keys.iter().enumerate() {
            for evidence in self.by_key.get(key).into_iter().flatten() {
                let Some(file) = files.get(&evidence.path) else { continue };
                let symbol =
                    evidence.symbol_index.and_then(|index| file.symbols.get(index)).cloned();
                let mut score = relationship_score(evidence.kind);
                score = score.saturating_sub(key_index.saturating_mul(4));
                if definition_count > 1 {
                    score = score.saturating_sub(8);
                }
                ranked.push(SymbolRelationshipMatch {
                    path: evidence.path.clone(),
                    symbol,
                    kind: evidence.kind,
                    score,
                    ambiguous: definition_count > 1,
                });
            }
        }
        for key in keys {
            let Some(definitions) = self.by_key.get(&key) else { continue };
            for evidence in definitions.iter().filter(|evidence| {
                matches!(
                    evidence.kind,
                    SymbolRelationshipKind::Definition | SymbolRelationshipKind::Module
                )
            }) {
                let Some(stem) = path_stem(&evidence.path) else { continue };
                let Some(tests) = self.test_by_stem.get(stem) else { continue };
                for path in tests.iter().take(MAX_SYMBOL_RELATIONSHIP_FANOUT) {
                    ranked.push(SymbolRelationshipMatch {
                        path: path.clone(),
                        symbol: None,
                        kind: SymbolRelationshipKind::Test,
                        score: relationship_score(SymbolRelationshipKind::Test).saturating_sub(4),
                        ambiguous: definition_count > 1,
                    });
                }
            }
        }
        ranked.sort_unstable_by(|left, right| {
            right
                .score
                .cmp(&left.score)
                .then_with(|| left.path.cmp(&right.path))
                .then_with(|| left.kind.cmp(&right.kind))
                .then_with(|| {
                    left.symbol
                        .as_ref()
                        .map(|symbol| symbol.name.as_str())
                        .cmp(&right.symbol.as_ref().map(|symbol| symbol.name.as_str()))
                })
        });
        ranked.dedup_by(|left, right| {
            left.path == right.path && left.kind == right.kind && left.symbol == right.symbol
        });
        ranked.truncate(limit);
        ranked
    }

    fn estimated_bytes(&self) -> usize {
        self.by_key
            .iter()
            .map(|(key, values)| {
                key.len()
                    + values.iter().map(|value| value.path.as_os_str().len() + 24).sum::<usize>()
            })
            .sum::<usize>()
            + self
                .contributions
                .values()
                .flatten()
                .map(|contribution| {
                    contribution.key.len() + contribution.evidence.path.as_os_str().len() + 24
                })
                .sum::<usize>()
    }

    fn entry_count(&self) -> usize {
        self.by_key.values().map(Vec::len).sum()
    }
}

/// An in-memory, bounded collection of lazily supplied symbol files.
#[derive(Clone, Debug)]
pub struct SymbolIndex {
    files: BTreeMap<PathBuf, SymbolFile>,
    max_files: usize,
    relationship_index: RelationshipIndex,
}

impl Default for SymbolIndex {
    fn default() -> Self {
        Self::new()
    }
}

impl SymbolIndex {
    #[must_use]
    pub fn new() -> Self {
        Self {
            files: BTreeMap::new(),
            max_files: MAX_SYMBOL_FILES,
            relationship_index: RelationshipIndex::default(),
        }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.files.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    #[must_use]
    pub fn symbol_count(&self) -> usize {
        self.files.values().map(|file| file.symbols.len()).sum()
    }

    #[must_use]
    pub fn file(&self, path: &Path) -> Option<&SymbolFile> {
        self.files.get(path)
    }

    /// Parse and replace a file. Re-indexing the same path with changed content replaces the old
    /// entry, which makes content-hash invalidation explicit and deterministic.
    pub fn index_file(&mut self, path: impl Into<PathBuf>, source: &str) -> SymbolFile {
        let path = path.into();
        let file = parse_source(&path, source, false);
        self.insert(file.clone());
        file
    }

    /// Parse a source prefix while retaining a caller-provided truncation marker.
    pub fn index_file_bounded(
        &mut self,
        path: impl Into<PathBuf>,
        source: &str,
        truncated: bool,
    ) -> SymbolFile {
        let path = path.into();
        let file = parse_source(&path, source, truncated);
        self.insert(file.clone());
        file
    }

    pub(crate) fn index_file_hashed(
        &mut self,
        path: impl Into<PathBuf>,
        source: &str,
        content_hash: [u8; 32],
        truncated: bool,
    ) -> SymbolFile {
        let path = path.into();
        let mut file = parse_source(&path, source, truncated);
        file.content_hash = content_hash;
        self.insert(file.clone());
        file
    }

    pub fn remove_file(&mut self, path: &Path) -> Option<SymbolFile> {
        self.relationship_index.remove_file(path);
        self.files.remove(path)
    }

    #[must_use]
    pub fn lookup(&self, query: &str, limit: usize) -> Vec<SymbolMatch> {
        lookup_files(self.files.values(), query, limit)
    }

    #[must_use]
    pub fn lookup_in_paths(
        &self,
        paths: &[PathBuf],
        query: &str,
        limit: usize,
    ) -> Vec<SymbolMatch> {
        lookup_files(paths.iter().filter_map(|path| self.files.get(path)), query, limit)
    }

    /// Rank likely definitions, callers, implementations, tests, and dependencies using only
    /// bounded lexical evidence from files already present in this index.
    #[must_use]
    pub fn lookup_relationships(&self, query: &str, limit: usize) -> Vec<SymbolRelationshipMatch> {
        self.relationship_index.lookup(&self.files, query, limit)
    }

    #[must_use]
    pub fn relationship_count(&self) -> usize {
        self.relationship_index.entry_count()
    }

    #[must_use]
    pub fn estimated_relationship_bytes(&self) -> usize {
        self.relationship_index.estimated_bytes()
    }

    #[must_use]
    pub fn estimated_bytes(&self) -> usize {
        self.relationship_index.estimated_bytes()
            + self
                .files
                .values()
                .map(|file| {
                    file.path.as_os_str().len()
                        + file.symbols.iter().map(symbol_bytes).sum::<usize>()
                        + file
                            .relationships
                            .iter()
                            .map(|relation| relation.from.len() + relation.to.len())
                            .sum::<usize>()
                })
                .sum::<usize>()
    }

    fn insert(&mut self, file: SymbolFile) {
        self.relationship_index.remove_file(&file.path);
        if !self.files.contains_key(&file.path) && self.files.len() >= self.max_files {
            let Some(oldest) = self.files.keys().next().cloned() else { return };
            self.relationship_index.remove_file(&oldest);
            self.files.remove(&oldest);
        }
        self.relationship_index.insert_file(&file);
        self.files.insert(file.path.clone(), file);
    }
}

/// Parse one source file using the language selected from its extension.
#[must_use]
pub fn parse_source(path: &Path, source: &str, truncated: bool) -> SymbolFile {
    let language = SymbolLanguage::for_path(path);
    let content_hash = hash_source(source.as_bytes());
    let (symbols, relationships) = match language {
        SymbolLanguage::Rust => parse_rust_symbols(source),
        SymbolLanguage::TypeScript | SymbolLanguage::Python | SymbolLanguage::Unsupported => {
            (Vec::new(), Vec::new())
        }
    };
    SymbolFile {
        path: path.to_path_buf(),
        language,
        content_hash,
        symbols,
        relationships,
        truncated,
        readable: true,
    }
}

/// Parse Rust source directly. This is useful to callers that already have a bounded read.
#[must_use]
pub fn parse_rust(path: impl Into<PathBuf>, source: &str) -> SymbolFile {
    let path = path.into();
    parse_source(&path, source, false)
}

fn lookup_files<'a>(
    files: impl IntoIterator<Item = &'a SymbolFile>,
    query: &str,
    limit: usize,
) -> Vec<SymbolMatch> {
    let limit = limit.min(MAX_SYMBOL_LOOKUP_RESULTS);
    if limit == 0 || query.trim().is_empty() {
        return Vec::new();
    }
    let terms = query
        .split(|character: char| !character.is_ascii_alphanumeric() && character != '_')
        .filter(|term| term.len() > 1)
        .take(16)
        .collect::<Vec<_>>();
    if terms.is_empty() {
        return Vec::new();
    }
    let mut matches = Vec::new();
    for file in files {
        for symbol in &file.symbols {
            let score = symbol_score(symbol, &file.path, &terms);
            if score > 0 {
                matches.push(SymbolMatch {
                    path: file.path.clone(),
                    symbol: symbol.clone(),
                    score,
                });
            }
        }
    }
    matches.sort_unstable_by(|left, right| {
        right
            .score
            .cmp(&left.score)
            .then_with(|| left.path.cmp(&right.path))
            .then_with(|| left.symbol.start_line.cmp(&right.symbol.start_line))
            .then_with(|| left.symbol.name.cmp(&right.symbol.name))
    });
    matches.truncate(limit);
    matches
}

fn symbol_score(symbol: &Symbol, path: &Path, terms: &[&str]) -> usize {
    let mut score = 0;
    for term in terms {
        if symbol.name.eq_ignore_ascii_case(term) {
            score += 100;
        } else if starts_with_ascii_case_insensitive(&symbol.name, term) {
            score += 70;
        } else if contains_ascii_case_insensitive(&symbol.name, term) {
            score += 50;
        } else if symbol
            .container
            .as_deref()
            .is_some_and(|container| contains_ascii_case_insensitive(container, term))
        {
            score += 25;
        } else if path
            .to_string_lossy()
            .split(['/', '\\', '_', '.', '-'])
            .any(|part| part.eq_ignore_ascii_case(term))
        {
            score += 10;
        }
    }
    if symbol.is_test && terms.iter().any(|term| term.eq_ignore_ascii_case("test")) {
        score += 8;
    }
    score
}

fn starts_with_ascii_case_insensitive(value: &str, prefix: &str) -> bool {
    value.get(..prefix.len()).is_some_and(|head| head.eq_ignore_ascii_case(prefix))
}

fn contains_ascii_case_insensitive(value: &str, needle: &str) -> bool {
    if needle.len() > value.len() {
        return false;
    }
    value
        .as_bytes()
        .windows(needle.len())
        .any(|window| window.eq_ignore_ascii_case(needle.as_bytes()))
}

fn symbol_bytes(symbol: &Symbol) -> usize {
    symbol.name.len() + symbol.container.as_ref().map_or(0, String::len) + 32
}

fn symbol_key(value: &str) -> String {
    value.trim().to_ascii_lowercase()
}

fn relationship_score(kind: SymbolRelationshipKind) -> usize {
    match kind {
        SymbolRelationshipKind::Definition => 120,
        SymbolRelationshipKind::Implementation => 104,
        SymbolRelationshipKind::Caller => 92,
        SymbolRelationshipKind::Test => 84,
        SymbolRelationshipKind::Dependency => 68,
        SymbolRelationshipKind::Import => 56,
        SymbolRelationshipKind::Module => 48,
    }
}

fn add_contribution(
    by_key: &mut BTreeMap<String, Vec<RelationshipEvidence>>,
    contributions: &mut Vec<RelationshipContribution>,
    key: String,
    evidence: RelationshipEvidence,
) {
    if key.is_empty() {
        return;
    }
    let values = by_key.entry(key.clone()).or_default();
    if !values.contains(&evidence) {
        values.push(evidence.clone());
        values.sort_unstable_by(|left, right| {
            left.path
                .cmp(&right.path)
                .then_with(|| left.symbol_index.cmp(&right.symbol_index))
                .then_with(|| left.kind.cmp(&right.kind))
        });
        values.truncate(MAX_SYMBOL_RELATIONSHIP_FANOUT);
    }
    if values.contains(&evidence) {
        contributions.push(RelationshipContribution { key, evidence });
    }
}

fn remove_evidence(
    by_key: &mut BTreeMap<String, Vec<RelationshipEvidence>>,
    key: &str,
    evidence: &RelationshipEvidence,
) {
    let Some(values) = by_key.get_mut(key) else { return };
    values.retain(|value| value != evidence);
    if values.is_empty() {
        by_key.remove(key);
    }
}

fn insert_path(paths: &mut BTreeMap<String, Vec<PathBuf>>, stem: &str, path: PathBuf) {
    let values = paths.entry(stem.to_owned()).or_default();
    if !values.contains(&path) {
        values.push(path);
        values.sort_unstable();
        values.truncate(MAX_SYMBOL_RELATIONSHIP_FANOUT);
    }
}

fn remove_path_from_stem(paths: &mut BTreeMap<String, Vec<PathBuf>>, stem: &str, path: &Path) {
    let Some(values) = paths.get_mut(stem) else { return };
    values.retain(|value| value != path);
    if values.is_empty() {
        paths.remove(stem);
    }
}

fn path_stem(path: &Path) -> Option<&str> {
    path.file_stem().and_then(|stem| stem.to_str()).map(|stem| stem.trim_end_matches("_test"))
}

fn is_test_path(path: &Path) -> bool {
    if path.components().any(|component| component.as_os_str() == "tests") {
        return true;
    }
    path.file_stem()
        .and_then(|stem| stem.to_str())
        .is_some_and(|stem| stem.starts_with("test_") || stem.ends_with("_test"))
}

fn hash_source(source: &[u8]) -> [u8; 32] {
    let mut hasher = Hasher::new();
    hasher.update(source);
    *hasher.finalize().as_bytes()
}

#[derive(Clone, Copy)]
struct Token {
    start: usize,
    end: usize,
    line: usize,
}

#[derive(Clone)]
enum PendingKind {
    Module,
    Function(usize),
    Type(usize),
    Impl(Option<String>),
}

#[derive(Clone)]
struct Scope {
    kind: ScopeKind,
    name: Option<String>,
    symbol_index: Option<usize>,
    is_test: bool,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum ScopeKind {
    Module,
    Impl,
    Function,
    Type,
    Block,
}

fn parse_rust_symbols(source: &str) -> (Vec<Symbol>, Vec<SymbolRelation>) {
    let masked = mask_rust(source);
    let tokens = tokenize(&masked);
    let mut symbols = Vec::new();
    let mut relationships = Vec::new();
    let mut scopes = Vec::new();
    let mut pending = None;
    let mut pending_test = false;
    let mut index = 0;
    while index < tokens.len() {
        let token = tokens[index];
        let word = &masked[token.start..token.end];
        if word == "#"
            && next_text(&tokens, &masked, index + 1) == Some("[")
            && let Some(close) = find_attribute_end(&tokens, &masked, index + 1)
        {
            pending_test |= attribute_is_test(&masked, token.start, tokens[close].end);
            index = close + 1;
            continue;
        }
        if word == "use"
            && let Some(close) = find_semicolon(&tokens, &masked, index + 1)
        {
            if symbols.len() < MAX_SYMBOLS_PER_FILE {
                let raw = source[token.end..tokens[close].start].trim();
                let container = container_name(&scopes);
                for (name, target) in
                    import_entries(raw).into_iter().take(MAX_SYMBOL_RELATIONSHIPS_PER_FILE)
                {
                    if symbols.len() >= MAX_SYMBOLS_PER_FILE {
                        break;
                    }
                    let symbol_index = symbols.len();
                    symbols.push(Symbol {
                        name: if name.is_empty() { "use".to_owned() } else { name },
                        kind: SymbolKind::Import,
                        start_line: token.line,
                        end_line: tokens[close].line,
                        container: container.clone(),
                        is_test: false,
                    });
                    if let Some(container) = container.clone() {
                        push_relationship(
                            &mut relationships,
                            SymbolRelation {
                                from: container.clone(),
                                to: symbols[symbol_index].name.clone(),
                                kind: SymbolRelationKind::Contains,
                            },
                        );
                    }
                    push_relationship(
                        &mut relationships,
                        SymbolRelation {
                            from: symbols[symbol_index].name.clone(),
                            to: target,
                            kind: SymbolRelationKind::Imports,
                        },
                    );
                }
            }
            pending = None;
            pending_test = false;
            index = close + 1;
            continue;
        }
        if let Some(kind) = declaration_kind(word) {
            if let Some(name_index) = next_identifier(&tokens, &masked, index + 1) {
                let name = bounded_name(&masked[tokens[name_index].start..tokens[name_index].end]);
                let test = pending_test || scopes.iter().any(|scope| scope.is_test);
                let kind = match kind {
                    DeclarationKind::Function if test => SymbolKind::Test,
                    DeclarationKind::Function if in_impl(&scopes) => SymbolKind::Method,
                    DeclarationKind::Function => SymbolKind::Function,
                    DeclarationKind::Struct => SymbolKind::Struct,
                    DeclarationKind::Enum => SymbolKind::Enum,
                    DeclarationKind::Trait => SymbolKind::Trait,
                    DeclarationKind::Type => SymbolKind::TypeAlias,
                    DeclarationKind::Module => SymbolKind::Module,
                };
                let container = container_name(&scopes);
                let symbol_index = if symbols.len() < MAX_SYMBOLS_PER_FILE {
                    let symbol_index = symbols.len();
                    symbols.push(Symbol {
                        name: name.clone(),
                        kind,
                        start_line: token.line,
                        end_line: token.line,
                        container: container.clone(),
                        is_test: test,
                    });
                    if let Some(container) = container.clone() {
                        push_relationship(
                            &mut relationships,
                            SymbolRelation {
                                from: container.clone(),
                                to: name.clone(),
                                kind: SymbolRelationKind::Contains,
                            },
                        );
                        if kind == SymbolKind::Method {
                            push_relationship(
                                &mut relationships,
                                SymbolRelation {
                                    from: name.clone(),
                                    to: container.clone(),
                                    kind: SymbolRelationKind::Implements,
                                },
                            );
                        }
                    }
                    Some(symbol_index)
                } else {
                    None
                };
                pending = Some(match declaration_kind(word).expect("matched declaration") {
                    DeclarationKind::Module => PendingKind::Module,
                    DeclarationKind::Function => {
                        PendingKind::Function(symbol_index.unwrap_or(usize::MAX))
                    }
                    DeclarationKind::Struct
                    | DeclarationKind::Enum
                    | DeclarationKind::Trait
                    | DeclarationKind::Type => {
                        PendingKind::Type(symbol_index.unwrap_or(usize::MAX))
                    }
                });
                pending_test = test && matches!(kind, SymbolKind::Module);
                index = name_index;
            }
        } else if word == "impl" {
            pending = Some(PendingKind::Impl(impl_name(&tokens, &masked, index + 1)));
            pending_test = false;
        } else if word == "{" {
            let (kind, name, symbol_index, is_test) = match pending.take() {
                Some(PendingKind::Module) => {
                    let module = symbols.last().filter(|symbol| symbol.kind == SymbolKind::Module);
                    (
                        ScopeKind::Module,
                        module.map(|symbol| symbol.name.clone()),
                        module.map(|_| symbols.len() - 1),
                        pending_test,
                    )
                }
                Some(PendingKind::Function(symbol_index)) => {
                    let symbol = symbols.get(symbol_index).filter(|_| symbol_index != usize::MAX);
                    (
                        ScopeKind::Function,
                        symbol.map(|symbol| symbol.name.clone()),
                        symbol.map(|_| symbol_index),
                        symbol.is_some_and(|symbol| symbol.is_test),
                    )
                }
                Some(PendingKind::Type(symbol_index)) => {
                    let symbol = symbols.get(symbol_index).filter(|_| symbol_index != usize::MAX);
                    (
                        ScopeKind::Type,
                        symbol.map(|symbol| symbol.name.clone()),
                        symbol.map(|_| symbol_index),
                        symbol.is_some_and(|symbol| symbol.is_test),
                    )
                }
                Some(PendingKind::Impl(name)) => (ScopeKind::Impl, name, None, false),
                None => (ScopeKind::Block, None, None, false),
            };
            scopes.push(Scope { kind, name, symbol_index, is_test });
            pending_test = false;
        } else if word == "}" {
            if let Some(scope) = scopes.pop()
                && let Some(symbol_index) = scope.symbol_index
                && let Some(symbol) = symbols.get_mut(symbol_index)
            {
                symbol.end_line = token.line;
            }
            pending = None;
            pending_test = false;
        } else if word == ";" {
            pending = None;
            pending_test = false;
        } else if is_reference_identifier(word)
            && let Some(owner) = reference_owner(&scopes)
        {
            push_relationship(
                &mut relationships,
                SymbolRelation {
                    from: owner,
                    to: bounded_name(word),
                    kind: SymbolRelationKind::References,
                },
            );
        }
        index += 1;
    }
    (symbols, relationships)
}

#[derive(Clone, Copy)]
enum DeclarationKind {
    Function,
    Struct,
    Enum,
    Trait,
    Type,
    Module,
}

fn declaration_kind(word: &str) -> Option<DeclarationKind> {
    Some(match word {
        "fn" => DeclarationKind::Function,
        "struct" => DeclarationKind::Struct,
        "enum" => DeclarationKind::Enum,
        "trait" => DeclarationKind::Trait,
        "type" => DeclarationKind::Type,
        "mod" => DeclarationKind::Module,
        _ => return None,
    })
}

fn next_identifier(tokens: &[Token], source: &str, mut index: usize) -> Option<usize> {
    while index < tokens.len() {
        let text = &source[tokens[index].start..tokens[index].end];
        if is_identifier(text) {
            return Some(index);
        }
        if matches!(text, "{" | ";" | "=") {
            return None;
        }
        index += 1;
    }
    None
}

fn find_attribute_end(tokens: &[Token], source: &str, open: usize) -> Option<usize> {
    let mut depth = 0usize;
    for index in open..tokens.len() {
        match &source[tokens[index].start..tokens[index].end] {
            "[" => depth += 1,
            "]" => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(index);
                }
            }
            _ => {}
        }
    }
    None
}

fn import_entries(raw: &str) -> Vec<(String, String)> {
    let raw = raw.trim();
    let Some(open) = raw.find('{') else {
        return vec![import_entry(raw)];
    };
    let Some(close) = raw.rfind('}') else {
        return vec![import_entry(raw)];
    };
    let prefix = raw[..open].trim_end_matches(":").trim();
    let body = &raw[open + 1..close];
    let mut entries = Vec::new();
    for part in body.split(',').map(str::trim).filter(|part| !part.is_empty()) {
        let item = if part == "self" {
            prefix.to_owned()
        } else if prefix.is_empty() {
            part.to_owned()
        } else {
            format!("{prefix}::{part}")
        };
        entries.push(import_entry(&item));
    }
    if entries.is_empty() { vec![import_entry(raw)] } else { entries }
}

fn import_entry(raw: &str) -> (String, String) {
    let raw = raw.trim();
    let (target, alias) = raw
        .split_once(" as ")
        .map_or((raw, None), |(target, alias)| (target.trim(), Some(alias.trim())));
    let target = bounded_name(target);
    let name = alias.map(bounded_name).unwrap_or_else(|| target.clone());
    (name, target)
}

fn attribute_is_test(source: &str, start: usize, end: usize) -> bool {
    let text = &source[start..end];
    let tokens = tokenize(text);
    tokens.iter().any(|token| {
        let word = &text[token.start..token.end];
        word == "test"
    })
}

fn find_semicolon(tokens: &[Token], source: &str, source_index: usize) -> Option<usize> {
    tokens
        .iter()
        .enumerate()
        .skip(source_index)
        .find_map(|(index, token)| (&source[token.start..token.end] == ";").then_some(index))
}

fn next_text<'a>(tokens: &[Token], source: &'a str, index: usize) -> Option<&'a str> {
    tokens.get(index).map(|token| &source[token.start..token.end])
}

fn impl_name(tokens: &[Token], source: &str, start: usize) -> Option<String> {
    let mut angle_depth = 0usize;
    let mut after_for = false;
    let mut fallback = None;
    for token in tokens.iter().skip(start) {
        let text = &source[token.start..token.end];
        if text == "{" {
            break;
        }
        if text == "<" {
            angle_depth += 1;
            continue;
        }
        if text == ">" {
            angle_depth = angle_depth.saturating_sub(1);
            continue;
        }
        if text == "for" {
            after_for = true;
            continue;
        }
        if is_identifier(text) && !matches!(text, "where" | "unsafe") {
            if after_for && angle_depth == 0 {
                return Some(bounded_name(text));
            }
            if angle_depth == 0 {
                fallback = Some(bounded_name(text));
            }
        }
    }
    fallback
}

fn container_name(scopes: &[Scope]) -> Option<String> {
    let mut names = Vec::new();
    for scope in scopes {
        if matches!(scope.kind, ScopeKind::Module | ScopeKind::Impl)
            && let Some(name) = &scope.name
        {
            names.push(name.as_str());
        }
    }
    (!names.is_empty()).then(|| names.join("::"))
}

fn reference_owner(scopes: &[Scope]) -> Option<String> {
    scopes.iter().rev().find_map(|scope| {
        matches!(scope.kind, ScopeKind::Function).then(|| scope.name.clone()).flatten()
    })
}

fn is_reference_identifier(word: &str) -> bool {
    is_identifier(word)
        && !matches!(
            word,
            "as" | "async"
                | "await"
                | "break"
                | "const"
                | "continue"
                | "crate"
                | "dyn"
                | "else"
                | "enum"
                | "extern"
                | "false"
                | "fn"
                | "for"
                | "if"
                | "impl"
                | "in"
                | "let"
                | "loop"
                | "match"
                | "mod"
                | "move"
                | "mut"
                | "pub"
                | "ref"
                | "return"
                | "self"
                | "Self"
                | "static"
                | "struct"
                | "super"
                | "trait"
                | "true"
                | "type"
                | "unsafe"
                | "use"
                | "where"
                | "while"
        )
}

fn in_impl(scopes: &[Scope]) -> bool {
    scopes.iter().rev().any(|scope| scope.kind == ScopeKind::Impl)
}

fn push_relationship(relationships: &mut Vec<SymbolRelation>, relation: SymbolRelation) {
    if relationships.len() < MAX_SYMBOL_RELATIONSHIPS_PER_FILE {
        relationships.push(relation);
    }
}

fn bounded_name(name: &str) -> String {
    name.trim().chars().take(MAX_SYMBOL_NAME_BYTES).collect()
}

fn is_identifier(text: &str) -> bool {
    let mut characters = text.chars();
    let Some(first) = characters.next() else { return false };
    (first == '_' || first.is_ascii_alphabetic())
        && characters.all(|character| character == '_' || character.is_ascii_alphanumeric())
}

fn mask_rust(source: &str) -> String {
    let bytes = source.as_bytes();
    let mut masked = bytes.to_vec();
    let mut index = 0;
    while index < bytes.len() {
        if let Some(end) = raw_string_end(bytes, index) {
            for byte in &mut masked[index..end] {
                if *byte != b'\n' {
                    *byte = b' ';
                }
            }
            index = end;
            continue;
        }
        if bytes[index] == b'/' && bytes.get(index + 1) == Some(&b'/') {
            masked[index] = b' ';
            if index + 1 < bytes.len() {
                masked[index + 1] = b' ';
            }
            index += 2;
            while index < bytes.len() && bytes[index] != b'\n' {
                masked[index] = b' ';
                index += 1;
            }
            continue;
        }
        if bytes[index] == b'/' && bytes.get(index + 1) == Some(&b'*') {
            let mut depth = 1usize;
            masked[index] = b' ';
            masked[index + 1] = b' ';
            index += 2;
            while index < bytes.len() && depth > 0 {
                if bytes[index] == b'/' && bytes.get(index + 1) == Some(&b'*') {
                    depth += 1;
                    masked[index] = b' ';
                    masked[index + 1] = b' ';
                    index += 2;
                } else if bytes[index] == b'*' && bytes.get(index + 1) == Some(&b'/') {
                    depth = depth.saturating_sub(1);
                    masked[index] = b' ';
                    masked[index + 1] = b' ';
                    index += 2;
                } else {
                    if bytes[index] != b'\n' {
                        masked[index] = b' ';
                    }
                    index += 1;
                }
            }
            continue;
        }
        if bytes[index] == b'\''
            && bytes.get(index + 1).is_some_and(|byte| byte.is_ascii_alphabetic() || *byte == b'_')
        {
            let mut lifetime_end = index + 2;
            while lifetime_end < bytes.len()
                && (bytes[lifetime_end].is_ascii_alphanumeric() || bytes[lifetime_end] == b'_')
            {
                lifetime_end += 1;
            }
            if bytes.get(lifetime_end) != Some(&b'\'') {
                index += 1;
                continue;
            }
        }
        if bytes[index] == b'\'' || bytes[index] == b'"' {
            let quote = bytes[index];
            masked[index] = b' ';
            index += 1;
            while index < bytes.len() {
                let escaped = index > 0 && bytes[index - 1] == b'\\';
                if bytes[index] != b'\n' {
                    masked[index] = b' ';
                }
                let done = bytes[index] == quote && !escaped;
                index += 1;
                if done {
                    break;
                }
            }
            continue;
        }
        index += 1;
    }
    String::from_utf8(masked).unwrap_or_default()
}

fn raw_string_end(bytes: &[u8], start: usize) -> Option<usize> {
    let marker = if bytes.get(start) == Some(&b'r') {
        start
    } else if bytes.get(start) == Some(&b'b') && bytes.get(start + 1) == Some(&b'r') {
        start + 1
    } else {
        return None;
    };
    let mut hashes = 0usize;
    let mut quote = marker + 1;
    while bytes.get(quote) == Some(&b'#') {
        hashes += 1;
        quote += 1;
    }
    if bytes.get(quote) != Some(&b'"') {
        return None;
    }
    let mut cursor = quote + 1;
    while cursor < bytes.len() {
        if bytes[cursor] == b'"'
            && bytes
                .get(cursor + 1..cursor + 1 + hashes)
                .is_some_and(|closing| closing.iter().all(|byte| *byte == b'#'))
        {
            return Some(cursor + hashes + 1);
        }
        cursor += 1;
    }
    Some(bytes.len())
}

fn tokenize(source: &str) -> Vec<Token> {
    let bytes = source.as_bytes();
    let mut tokens = Vec::new();
    let mut index = 0;
    let mut line = 1;
    while index < bytes.len() && tokens.len() < MAX_SYMBOL_TOKENS {
        if bytes[index].is_ascii_whitespace() {
            if bytes[index] == b'\n' {
                line += 1;
            }
            index += 1;
            continue;
        }
        let start = index;
        if bytes[index].is_ascii_alphabetic() || bytes[index] == b'_' {
            index += 1;
            while index < bytes.len()
                && (bytes[index].is_ascii_alphanumeric() || bytes[index] == b'_')
            {
                index += 1;
            }
        } else {
            index += 1;
        }
        tokens.push(Token { start, end: index, line });
    }
    tokens
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(file: &SymbolFile, kind: SymbolKind) -> Vec<&str> {
        file.symbols
            .iter()
            .filter(|symbol| symbol.kind == kind)
            .map(|symbol| symbol.name.as_str())
            .collect()
    }

    #[test]
    fn rust_extracts_functions_methods_types_modules_tests_and_imports() {
        let file = parse_rust(
            "src/lib.rs",
            "use crate::items::Thing;\nmod outer {\n    pub struct Boxed;\n    enum State { Ready }\n    impl Boxed {\n        fn open(&self) {}\n    }\n    #[test]\n    fn opens() {}\n    #[cfg(test)]\n    mod nested { fn inner() {} }\n}\n",
        );
        assert_eq!(names(&file, SymbolKind::Import), vec!["crate::items::Thing"]);
        assert_eq!(names(&file, SymbolKind::Struct), vec!["Boxed"]);
        assert_eq!(names(&file, SymbolKind::Enum), vec!["State"]);
        assert_eq!(names(&file, SymbolKind::Module), vec!["outer", "nested"]);
        assert_eq!(names(&file, SymbolKind::Method), vec!["open"]);
        assert_eq!(names(&file, SymbolKind::Test), vec!["opens", "inner"]);
        assert!(file.relationships.iter().any(|relation| {
            relation.kind == SymbolRelationKind::Contains && relation.to == "open"
        }));
    }

    #[test]
    fn malformed_rust_is_partial_and_does_not_panic() {
        let file = parse_rust("src/broken.rs", "/* unclosed fn ignored\n fn not_seen() {\n");
        assert!(file.symbols.is_empty());
        let file = parse_rust("src/broken.rs", "fn visible( { struct Item {\n fn nested() {");
        assert!(file.symbols.iter().any(|symbol| symbol.name == "visible"));
        assert!(file.symbols.iter().any(|symbol| symbol.name == "Item"));
    }

    #[test]
    fn comments_and_literals_do_not_create_symbols() {
        let file = parse_rust(
            "src/literals.rs",
            "// fn comment() {}\nconst TEXT: &str = r###\"fn fake() {}\"###;\nfn real() {}\n",
        );
        assert_eq!(names(&file, SymbolKind::Function), vec!["real"]);
    }

    #[test]
    fn source_hash_invalidation_replaces_symbols() {
        let mut index = SymbolIndex::new();
        let first = index.index_file("src/lib.rs", "fn old() {}");
        let second = index.index_file("src/lib.rs", "fn new() {}");
        assert_ne!(first.content_hash, second.content_hash);
        assert!(
            index
                .file(Path::new("src/lib.rs"))
                .unwrap()
                .symbols
                .iter()
                .all(|symbol| symbol.name != "old")
        );
        assert_eq!(index.symbol_count(), 1);
    }

    #[test]
    fn large_source_is_bounded() {
        let source = "fn generated() {}\n".repeat(MAX_SYMBOLS_PER_FILE * 2);
        let file = SymbolIndex::new().index_file_bounded(
            "src/large.rs",
            &source[..MAX_SYMBOL_SOURCE_BYTES.min(source.len())],
            true,
        );
        assert!(file.truncated);
        assert!(file.symbols.len() <= MAX_SYMBOLS_PER_FILE);
        assert!(file.relationships.len() <= MAX_SYMBOL_RELATIONSHIPS_PER_FILE);
    }

    #[test]
    fn lookup_is_targeted_and_deterministic() {
        let mut index = SymbolIndex::new();
        index.index_file("src/one.rs", "fn target_one() {}");
        index.index_file("src/two.rs", "fn target_two() {}");
        let matches = index.lookup("target_two", 4);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].path, Path::new("src/two.rs"));
    }

    #[test]
    fn cross_file_relationships_rank_definitions_callers_implementations_tests_and_dependencies() {
        let mut index = SymbolIndex::new();
        index.index_file(
            "src/widget.rs",
            "pub struct Widget;\nimpl Widget { pub fn open(&self) {} }\n",
        );
        index.index_file("src/lib.rs", "mod widget;\n");
        index.index_file(
            "src/use.rs",
            "use crate::widget::Widget as ImportedWidget;\nfn caller() { open(); ImportedWidget; }\n",
        );
        index.index_file(
            "tests/widget_test.rs",
            "use crate::widget::Widget;\n#[test]\nfn widget_behavior() { Widget; }\n",
        );

        let widget = index.lookup_relationships("Widget", 32);
        assert_eq!(widget[0].kind, SymbolRelationshipKind::Definition);
        assert_eq!(widget[0].path, Path::new("src/widget.rs"));
        assert!(widget.iter().any(|hit| {
            hit.kind == SymbolRelationshipKind::Dependency && hit.path == Path::new("src/use.rs")
        }));
        assert!(widget.iter().any(|hit| {
            hit.kind == SymbolRelationshipKind::Implementation
                && hit.path == Path::new("src/widget.rs")
        }));
        assert!(widget.iter().any(|hit| {
            hit.kind == SymbolRelationshipKind::Test
                && hit.path == Path::new("tests/widget_test.rs")
        }));
        assert!(index.lookup_relationships("widget", 8).iter().any(|hit| {
            hit.kind == SymbolRelationshipKind::Module && hit.path == Path::new("src/lib.rs")
        }));

        let callers = index.lookup_relationships("open", 8);
        assert!(callers.iter().any(|hit| {
            hit.kind == SymbolRelationshipKind::Caller && hit.path == Path::new("src/use.rs")
        }));
        let aliases = index.lookup_relationships("ImportedWidget", 8);
        assert!(aliases.iter().any(|hit| {
            hit.kind == SymbolRelationshipKind::Import && hit.path == Path::new("src/use.rs")
        }));
    }

    #[test]
    fn ambiguous_names_are_returned_without_claiming_resolution() {
        let mut index = SymbolIndex::new();
        index.index_file("src/one.rs", "pub fn duplicate() {}");
        index.index_file("src/two.rs", "pub fn duplicate() {}");
        let matches = index.lookup_relationships("duplicate", 8);
        assert_eq!(
            matches.iter().filter(|hit| hit.kind == SymbolRelationshipKind::Definition).count(),
            2
        );
        assert!(matches.iter().all(|hit| hit.ambiguous));
    }

    #[test]
    fn changing_one_participant_invalidates_only_its_relationships() {
        let mut index = SymbolIndex::new();
        index.index_file("src/target.rs", "pub fn target() {}");
        index.index_file("src/caller.rs", "fn caller() { target(); }");
        index.index_file("src/other.rs", "fn other() { target(); }");
        let before = index.relationship_count();
        assert_eq!(
            index
                .lookup_relationships("target", 8)
                .iter()
                .filter(|hit| hit.kind == SymbolRelationshipKind::Caller)
                .count(),
            2
        );
        index.index_file("src/caller.rs", "fn caller() {}");
        let matches = index.lookup_relationships("target", 8);
        assert!(matches.iter().all(|hit| hit.path != Path::new("src/caller.rs")));
        assert!(matches.iter().any(|hit| hit.path == Path::new("src/other.rs")));
        assert!(index.relationship_count() < before);
    }

    #[test]
    fn relationship_fanout_and_memory_are_bounded() {
        let mut index = SymbolIndex::new();
        for file_index in 0..MAX_SYMBOL_RELATIONSHIP_FANOUT * 2 {
            index.index_file(format!("src/duplicate_{file_index}.rs"), "pub fn duplicate() {}");
        }
        let matches = index.lookup_relationships("duplicate", MAX_SYMBOL_LOOKUP_RESULTS);
        assert!(matches.len() <= MAX_SYMBOL_RELATIONSHIP_FANOUT);
        assert!(index.estimated_relationship_bytes() < 64 * 1024);
    }
}
