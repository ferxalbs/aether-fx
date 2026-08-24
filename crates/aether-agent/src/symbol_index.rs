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

/// An in-memory, bounded collection of lazily supplied symbol files.
#[derive(Clone, Debug)]
pub struct SymbolIndex {
    files: BTreeMap<PathBuf, SymbolFile>,
    max_files: usize,
}

impl Default for SymbolIndex {
    fn default() -> Self {
        Self::new()
    }
}

impl SymbolIndex {
    #[must_use]
    pub fn new() -> Self {
        Self { files: BTreeMap::new(), max_files: MAX_SYMBOL_FILES }
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

    #[must_use]
    pub fn estimated_bytes(&self) -> usize {
        self.files
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
            .sum()
    }

    fn insert(&mut self, file: SymbolFile) {
        if !self.files.contains_key(&file.path) && self.files.len() >= self.max_files {
            let Some(oldest) = self.files.keys().next().cloned() else { return };
            self.files.remove(&oldest);
        }
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
                let name = bounded_name(raw);
                let container = container_name(&scopes);
                let symbol_index = symbols.len();
                symbols.push(Symbol {
                    name: if name.is_empty() { "use".to_owned() } else { name.clone() },
                    kind: SymbolKind::Import,
                    start_line: token.line,
                    end_line: tokens[close].line,
                    container: container.clone(),
                    is_test: false,
                });
                if let Some(container) = container {
                    push_relationship(
                        &mut relationships,
                        SymbolRelation {
                            from: container,
                            to: symbols[symbol_index].name.clone(),
                            kind: SymbolRelationKind::Contains,
                        },
                    );
                }
                push_relationship(
                    &mut relationships,
                    SymbolRelation {
                        from: symbols[symbol_index].name.clone(),
                        to: name,
                        kind: SymbolRelationKind::Imports,
                    },
                );
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
                                from: container,
                                to: name.clone(),
                                kind: SymbolRelationKind::Contains,
                            },
                        );
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
}
