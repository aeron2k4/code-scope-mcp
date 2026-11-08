//! Tree-sitter parsing and symbol extraction.
//!
//! Each supported language has a symbol query whose patterns capture a
//! definition node (`@function`, `@class`, ...) and its `@name`. Parsing never
//! fails on syntactically broken input: tree-sitter recovers with `ERROR`
//! nodes, and we report `has_errors` instead of bailing out.

use std::cell::Cell;
use std::ops::ControlFlow;
use std::path::Path;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use streaming_iterator::StreamingIterator;
use tree_sitter::{Node, Parser, Query, QueryCursor, QueryCursorOptions};

#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("unsupported language for file: {0}")]
    UnsupportedLanguage(String),
    #[error("failed to load grammar: {0}")]
    Grammar(#[from] tree_sitter::LanguageError),
    #[error("invalid symbol query for {lang}: {message}")]
    Query { lang: &'static str, message: String },
    #[error("tree-sitter returned no tree")]
    NoTree,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Lang {
    Rust,
    Python,
    JavaScript,
    TypeScript,
    Tsx,
    Go,
    C,
    Cpp,
    Java,
    CSharp,
    GdScript,
}

impl Lang {
    pub const ALL: [Lang; 11] = [
        Lang::Rust,
        Lang::Python,
        Lang::JavaScript,
        Lang::TypeScript,
        Lang::Tsx,
        Lang::Go,
        Lang::C,
        Lang::Cpp,
        Lang::Java,
        Lang::CSharp,
        Lang::GdScript,
    ];

    pub fn from_path(path: &Path) -> Option<Lang> {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        Some(match ext.as_str() {
            "rs" => Lang::Rust,
            "py" | "pyi" => Lang::Python,
            "js" | "jsx" | "mjs" | "cjs" => Lang::JavaScript,
            "ts" | "mts" | "cts" => Lang::TypeScript,
            "tsx" => Lang::Tsx,
            "go" => Lang::Go,
            // `parse_path` also tries C++ on a `.h` file that fails as C.
            "c" | "h" => Lang::C,
            "cc" | "cpp" | "cxx" | "c++" | "hh" | "hpp" | "hxx" | "h++" | "ipp" | "tpp" => {
                Lang::Cpp
            }
            "java" => Lang::Java,
            "cs" => Lang::CSharp,
            "gd" => Lang::GdScript,
            _ => return None,
        })
    }

    /// Position in [`Self::ALL`], for compact storage. A match rather than a
    /// search, so a new language cannot be left without a number.
    pub fn index(self) -> usize {
        match self {
            Lang::Rust => 0,
            Lang::Python => 1,
            Lang::JavaScript => 2,
            Lang::TypeScript => 3,
            Lang::Tsx => 4,
            Lang::Go => 5,
            Lang::C => 6,
            Lang::Cpp => 7,
            Lang::Java => 8,
            Lang::CSharp => 9,
            Lang::GdScript => 10,
        }
    }

    /// The language at `index` in [`Self::ALL`].
    pub fn from_index(index: usize) -> Option<Self> {
        Self::ALL.get(index).copied()
    }

    pub fn name(self) -> &'static str {
        match self {
            Lang::Rust => "rust",
            Lang::Python => "python",
            Lang::JavaScript => "javascript",
            Lang::TypeScript => "typescript",
            Lang::Tsx => "tsx",
            Lang::Go => "go",
            Lang::C => "c",
            Lang::Cpp => "cpp",
            Lang::Java => "java",
            Lang::CSharp => "csharp",
            Lang::GdScript => "gdscript",
        }
    }

    fn grammar(self) -> tree_sitter::Language {
        match self {
            Lang::Rust => tree_sitter_rust::LANGUAGE.into(),
            Lang::Python => tree_sitter_python::LANGUAGE.into(),
            Lang::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
            Lang::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Lang::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
            Lang::Go => tree_sitter_go::LANGUAGE.into(),
            Lang::C => tree_sitter_c::LANGUAGE.into(),
            Lang::Cpp => tree_sitter_cpp::LANGUAGE.into(),
            Lang::Java => tree_sitter_java::LANGUAGE.into(),
            Lang::CSharp => tree_sitter_c_sharp::LANGUAGE.into(),
            Lang::GdScript => tree_sitter_gdscript::LANGUAGE.into(),
        }
    }

    fn query_source(self) -> String {
        match self {
            Lang::Rust => RUST_QUERY.to_string(),
            Lang::Python => PYTHON_QUERY.to_string(),
            Lang::JavaScript => JS_QUERY.to_string(),
            Lang::TypeScript | Lang::Tsx => format!("{TS_EXTRA_QUERY}\n{JS_QUERY}"),
            Lang::Go => GO_QUERY.to_string(),
            Lang::C => format!("{C_FUNCTION_QUERY}\n{C_TYPE_QUERY}"),
            Lang::Cpp => format!("{CPP_QUERY}\n{C_FUNCTION_QUERY}\n{C_TYPE_QUERY}"),
            Lang::Java => JAVA_QUERY.to_string(),
            Lang::CSharp => CSHARP_QUERY.to_string(),
            Lang::GdScript => GDSCRIPT_QUERY.to_string(),
        }
    }

    /// Compiled symbol query, built once per language.
    fn query(self) -> Result<&'static Query, ParseError> {
        static QUERIES: [OnceLock<Result<Query, String>>; Lang::ALL.len()] =
            [const { OnceLock::new() }; Lang::ALL.len()];
        let slot = &QUERIES[self as usize];
        slot.get_or_init(|| {
            Query::new(&self.grammar(), &self.query_source()).map_err(|e| e.to_string())
        })
        .as_ref()
        .map_err(|message| ParseError::Query {
            lang: self.name(),
            message: message.clone(),
        })
    }
}

/// `class_name` names the file's own class but holds none of its members, so
/// it is left out. Signals have no kind of their own; their signature shows
/// `signal`.
const GDSCRIPT_QUERY: &str = r#"
(function_definition name: (_) @name) @function
(class_definition name: (_) @name) @class
(enum_definition name: (_) @name) @enum
(const_statement name: (_) @name) @constant
(signal_statement name: (_) @name) @function
(source (variable_statement name: (_) @name) @variable)
(source (export_variable_statement name: (_) @name) @variable)
(source (onready_variable_statement name: (_) @name) @variable)
(class_body (variable_statement name: (_) @name) @variable)
"#;

const RUST_QUERY: &str = r#"
(function_item name: (_) @name) @function
(function_signature_item name: (_) @name) @function
(struct_item name: (_) @name) @struct
(enum_item name: (_) @name) @enum
(union_item name: (_) @name) @struct
(trait_item name: (_) @name) @trait
(impl_item type: (_) @name) @impl
(mod_item name: (_) @name) @module
(const_item name: (_) @name) @constant
(static_item name: (_) @name) @constant
(type_item name: (_) @name) @type
(macro_definition name: (_) @name) @macro
"#;

const PYTHON_QUERY: &str = r#"
(function_definition name: (_) @name) @function
(class_definition name: (_) @name) @class
(module
  (expression_statement
    (assignment left: (identifier) @name)) @constant
  (#match? @name "^_*[A-Z][A-Z0-9_]*$"))
(module
  (expression_statement
    (assignment left: (identifier) @name)) @variable)
"#;

const JS_QUERY: &str = r#"
(function_declaration name: (_) @name) @function
(generator_function_declaration name: (_) @name) @function
(class_declaration name: (_) @name) @class
(method_definition name: (_) @name) @method
(lexical_declaration
  (variable_declarator
    name: (identifier) @name
    value: [(arrow_function) (function_expression)])) @function
(variable_declaration
  (variable_declarator
    name: (identifier) @name
    value: [(arrow_function) (function_expression)])) @function
; Top-level variables. One holding a function also matches a pattern above,
; which comes first and so wins.
(program
  (lexical_declaration kind: "const"
    (variable_declarator name: (identifier) @name)) @constant)
(program
  (export_statement
    declaration: (lexical_declaration kind: "const"
      (variable_declarator name: (identifier) @name)) @constant))
(program
  (lexical_declaration kind: "let"
    (variable_declarator name: (identifier) @name)) @variable)
(program
  (export_statement
    declaration: (lexical_declaration kind: "let"
      (variable_declarator name: (identifier) @name)) @variable))
(program
  (variable_declaration
    (variable_declarator name: (identifier) @name)) @variable)
(program
  (export_statement
    declaration: (variable_declaration
      (variable_declarator name: (identifier) @name)) @variable))
"#;

const TS_EXTRA_QUERY: &str = r#"
(interface_declaration name: (_) @name) @interface
(type_alias_declaration name: (_) @name) @type
(enum_declaration name: (_) @name) @enum
(abstract_class_declaration name: (_) @name) @class
(function_signature name: (_) @name) @function
(method_signature name: (_) @name) @method
(abstract_method_signature name: (_) @name) @method
(internal_module name: (_) @name) @module
"#;

const GO_QUERY: &str = r#"
(function_declaration name: (_) @name) @function
(method_declaration name: (_) @name) @method
(type_spec name: (_) @name type: (struct_type)) @struct
(type_spec name: (_) @name type: (interface_type)) @interface
(type_spec name: (_) @name) @type
(type_alias name: (_) @name) @type
(source_file (const_declaration (const_spec name: (identifier) @name) @constant))
(source_file (var_declaration (var_spec name: (identifier) @name) @variable))
(source_file
  (var_declaration (var_spec_list (var_spec name: (identifier) @name) @variable)))
"#;

/// C functions and prototypes, shared with C++. The name sits inside the
/// declarator, under at most one pointer for pointer-returning functions.
const C_FUNCTION_QUERY: &str = r#"
(function_definition
  declarator: (function_declarator declarator: (identifier) @name)) @function
(function_definition
  declarator: (pointer_declarator
    declarator: (function_declarator declarator: (identifier) @name))) @function
(declaration
  declarator: (function_declarator declarator: (identifier) @name)) @function
"#;

const C_TYPE_QUERY: &str = r#"
(struct_specifier name: (_) @name body: (_)) @struct
(union_specifier name: (_) @name body: (_)) @struct
(enum_specifier name: (_) @name body: (_)) @enum
(type_definition declarator: (type_identifier) @name) @type
(preproc_def name: (_) @name) @macro
(preproc_function_def name: (_) @name) @macro
"#;

/// C++ additions; `Foo::bar` defined outside its class is named `bar`.
const CPP_QUERY: &str = r#"
(function_definition
  declarator: (function_declarator
    declarator: [(field_identifier) (destructor_name) (operator_name)] @name)) @function
(function_definition
  declarator: (function_declarator
    declarator: (qualified_identifier name: (_) @name))) @function
(function_definition
  declarator: (pointer_declarator
    declarator: (function_declarator
      declarator: (qualified_identifier name: (_) @name)))) @function
(function_definition
  declarator: (reference_declarator
    (function_declarator declarator: [(identifier) (field_identifier)] @name))) @function
(function_definition
  declarator: (reference_declarator
    (function_declarator
      declarator: (qualified_identifier name: (_) @name)))) @function
(field_declaration
  declarator: (function_declarator declarator: (_) @name)) @function
(class_specifier name: (_) @name body: (_)) @class
(namespace_definition name: (_) @name) @module
(alias_declaration name: (_) @name) @type
(concept_definition name: (_) @name) @type
"#;

const JAVA_QUERY: &str = r#"
(class_declaration name: (_) @name) @class
(record_declaration name: (_) @name) @class
(interface_declaration name: (_) @name) @interface
(annotation_type_declaration name: (_) @name) @interface
(enum_declaration name: (_) @name) @enum
(method_declaration name: (_) @name) @method
(constructor_declaration name: (_) @name) @method
"#;

const CSHARP_QUERY: &str = r#"
(class_declaration name: (_) @name) @class
(record_declaration name: (_) @name) @class
(struct_declaration name: (_) @name) @struct
(interface_declaration name: (_) @name) @interface
(enum_declaration name: (_) @name) @enum
(delegate_declaration name: (_) @name) @type
(namespace_declaration name: (_) @name) @module
(file_scoped_namespace_declaration name: (_) @name) @module
(method_declaration name: (_) @name) @method
(constructor_declaration name: (_) @name) @method
(local_function_statement name: (_) @name) @function
"#;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum SymbolKind {
    Function,
    Method,
    Class,
    Struct,
    Enum,
    Interface,
    Trait,
    Impl,
    Module,
    Constant,
    Type,
    Macro,
    Variable,
}

impl SymbolKind {
    /// Every kind, in the order the store writes them.
    pub const ALL: [Self; 13] = [
        Self::Function,
        Self::Method,
        Self::Class,
        Self::Struct,
        Self::Enum,
        Self::Interface,
        Self::Trait,
        Self::Impl,
        Self::Module,
        Self::Constant,
        Self::Type,
        Self::Macro,
        Self::Variable,
    ];

    /// Position in [`Self::ALL`], for compact storage. A match rather than a
    /// search, so a new kind cannot be left without a number.
    pub fn index(self) -> usize {
        match self {
            Self::Function => 0,
            Self::Method => 1,
            Self::Class => 2,
            Self::Struct => 3,
            Self::Enum => 4,
            Self::Interface => 5,
            Self::Trait => 6,
            Self::Impl => 7,
            Self::Module => 8,
            Self::Constant => 9,
            Self::Type => 10,
            Self::Macro => 11,
            Self::Variable => 12,
        }
    }

    /// The kind at `index` in [`Self::ALL`].
    pub fn from_index(index: usize) -> Option<Self> {
        Self::ALL.get(index).copied()
    }
}

impl SymbolKind {
    fn from_capture(name: &str) -> Option<SymbolKind> {
        Some(match name {
            "function" => SymbolKind::Function,
            "method" => SymbolKind::Method,
            "class" => SymbolKind::Class,
            "struct" => SymbolKind::Struct,
            "enum" => SymbolKind::Enum,
            "interface" => SymbolKind::Interface,
            "trait" => SymbolKind::Trait,
            "impl" => SymbolKind::Impl,
            "module" => SymbolKind::Module,
            "constant" => SymbolKind::Constant,
            "type" => SymbolKind::Type,
            "macro" => SymbolKind::Macro,
            "variable" => SymbolKind::Variable,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            SymbolKind::Function => "function",
            SymbolKind::Method => "method",
            SymbolKind::Class => "class",
            SymbolKind::Struct => "struct",
            SymbolKind::Enum => "enum",
            SymbolKind::Interface => "interface",
            SymbolKind::Trait => "trait",
            SymbolKind::Impl => "impl",
            SymbolKind::Module => "module",
            SymbolKind::Constant => "constant",
            SymbolKind::Type => "type",
            SymbolKind::Macro => "macro",
            SymbolKind::Variable => "variable",
        }
    }

    /// Kinds whose nested functions are reported as methods.
    fn is_container(self) -> bool {
        matches!(
            self,
            SymbolKind::Class | SymbolKind::Impl | SymbolKind::Trait | SymbolKind::Interface
        )
    }

    /// Kinds whose body is executable code that the pruner may elide.
    pub fn is_callable(self) -> bool {
        matches!(self, SymbolKind::Function | SymbolKind::Method)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Symbol {
    pub name: String,
    pub kind: SymbolKind,
    /// Qualified name of the enclosing symbol, e.g. `Foo` or `outer::Inner`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// One-line declaration header (everything before the body).
    pub signature: String,
    /// 1-based inclusive line range.
    pub start_line: usize,
    pub end_line: usize,
    pub start_byte: usize,
    pub end_byte: usize,
    /// Byte range of the body block, if the definition has one.
    pub body: Option<(usize, usize)>,
    pub depth: usize,
}

impl Symbol {
    pub fn qualified_name(&self) -> String {
        match &self.parent {
            Some(p) => format!("{p}::{}", self.name),
            None => self.name.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParsedFile {
    pub lang: Lang,
    pub symbols: Vec<Symbol>,
    /// True when tree-sitter had to recover from syntax errors.
    pub has_errors: bool,
    /// True when the symbol query ran out of time, so `symbols` holds only
    /// those found before that.
    pub incomplete: bool,
    /// Start bytes of every identifier token, ascending. Kept so reference
    /// search needs no re-parse; stored as deltas to keep the store compact.
    #[serde(with = "deltas")]
    pub idents: Vec<u32>,
}

/// Serde adapter writing an ascending `Vec<u32>` as successive differences.
mod deltas {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(values: &[u32], s: S) -> Result<S::Ok, S::Error> {
        let mut prev = 0;
        s.collect_seq(values.iter().map(|&v| {
            let d = v.saturating_sub(prev);
            prev = v;
            d
        }))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u32>, D::Error> {
        let mut acc = 0u32;
        Ok(Vec::<u32>::deserialize(d)?
            .into_iter()
            .map(|delta| {
                acc = acc.saturating_add(delta);
                acc
            })
            .collect())
    }
}

pub fn parse_path(path: &Path, source: &str) -> Result<ParsedFile, ParseError> {
    let lang = Lang::from_path(path)
        .ok_or_else(|| ParseError::UnsupportedLanguage(path.display().to_string()))?;
    let parsed = parse_source(lang, source)?;
    let is_header = path
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("h"));
    if !is_header || !parsed.has_errors {
        return Ok(parsed);
    }
    // `.h` does not tell C from C++. A file both grammars reject, for instance
    // over a macro, goes to the one that makes more of it.
    let cpp = parse_source(Lang::Cpp, source)?;
    let better = !cpp.has_errors || cpp.symbols.len() > parsed.symbols.len();
    Ok(if better { cpp } else { parsed })
}

/// Time allowed for the symbol query on one file. Ordinary files need far less,
/// even a 2 MB data table about 6 s, but the query is quadratic on some broken
/// input: 2 MB of `[` would take most of an hour.
const QUERY_BUDGET: Duration = Duration::from_secs(10);

pub fn parse_source(lang: Lang, source: &str) -> Result<ParsedFile, ParseError> {
    let mut parser = Parser::new();
    parser.set_language(&lang.grammar())?;
    let tree = parser.parse(source, None).ok_or(ParseError::NoTree)?;
    let root = tree.root_node();
    let (symbols, incomplete) = extract_symbols(lang, root, source, QUERY_BUDGET)?;
    Ok(ParsedFile {
        lang,
        symbols,
        has_errors: root.has_error(),
        incomplete,
        idents: collect_idents(root),
    })
}

/// Start bytes of identifier leaves (identifier, type_identifier,
/// field_identifier, property_identifier, ..., and GDScript's `name` for the
/// name a definition declares), in source order. Comments and string literals
/// are not identifiers, so they never appear here.
fn collect_idents(root: Node<'_>) -> Vec<u32> {
    let mut out = Vec::new();
    let mut cursor = root.walk();
    'walk: loop {
        let node = cursor.node();
        if node.child_count() == 0
            && (node.kind().ends_with("identifier") || node.kind() == "name")
            && let Ok(start) = u32::try_from(node.start_byte())
        {
            out.push(start);
        }
        if cursor.goto_first_child() || cursor.goto_next_sibling() {
            continue;
        }
        loop {
            if !cursor.goto_parent() {
                break 'walk;
            }
            if cursor.goto_next_sibling() {
                break;
            }
        }
    }
    out
}

/// One identifier token in a source file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Occurrence {
    /// 1-based line.
    pub line: usize,
    /// 0-based byte column.
    pub column: usize,
    pub start_byte: usize,
}

fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

/// Identifier tokens spelled exactly `name`, in source order, looked up in
/// the identifiers recorded at parse time (no re-parse).
pub fn identifier_occurrences(parsed: &ParsedFile, source: &str, name: &str) -> Vec<Occurrence> {
    if name.is_empty() || !source.contains(name) {
        return Vec::new();
    }
    let mut out = Vec::new();
    let (mut line, mut line_start, mut counted) = (1, 0, 0);
    for &start in &parsed.idents {
        let start = start as usize;
        let Some(rest) = source.get(start..) else {
            continue;
        };
        let whole_token = rest
            .strip_prefix(name)
            .is_some_and(|after| !after.starts_with(is_ident_char));
        if !whole_token {
            continue;
        }
        // Idents are ascending, so line numbers are counted incrementally.
        for (i, b) in source.as_bytes()[counted..start].iter().enumerate() {
            if *b == b'\n' {
                line += 1;
                line_start = counted + i + 1;
            }
        }
        counted = start;
        out.push(Occurrence {
            line,
            column: start - line_start,
            start_byte: start,
        });
    }
    out
}

/// Symbols under `root`, and whether `budget` ran out before the query ended.
fn extract_symbols(
    lang: Lang,
    root: Node<'_>,
    source: &str,
    budget: Duration,
) -> Result<(Vec<Symbol>, bool), ParseError> {
    let query = lang.query()?;
    let names = query.capture_names();
    let bytes = source.as_bytes();

    // (pattern_index, symbol) so overlapping patterns can be de-duplicated
    // in favour of the more specific (earlier) one.
    let mut found: Vec<(usize, Symbol)> = Vec::new();
    let started = Instant::now();
    let timed_out = Cell::new(false);
    let mut check_budget = |_: &_| {
        if started.elapsed() < budget {
            return ControlFlow::Continue(());
        }
        timed_out.set(true);
        ControlFlow::Break(())
    };
    let options = QueryCursorOptions::new().progress_callback(&mut check_budget);
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches_with_options(query, root, bytes, options);
    while let Some(m) = matches.next() {
        let mut def: Option<(Node<'_>, SymbolKind)> = None;
        let mut name_node: Option<Node<'_>> = None;
        for cap in m.captures() {
            let cap_name = names[cap.index as usize];
            if cap_name == "name" {
                name_node = Some(cap.node);
            } else if let Some(kind) = SymbolKind::from_capture(cap_name) {
                def = Some((cap.node, kind));
            }
        }
        let (Some((node, kind)), Some(name_node)) = (def, name_node) else {
            continue;
        };
        let name = node_text(name_node, source).to_string();
        if name.is_empty() || is_require(name_node, source) {
            continue;
        }
        let node = own_declarator(node, name_node);
        // GDScript's body begins at the line break after the colon.
        let body = find_body(node).map(|b| {
            let text = node_text(b, source);
            let skipped = text.len() - text.trim_start().len();
            (b.start_byte() + skipped, b.end_byte())
        });
        found.push((
            m.pattern_index,
            Symbol {
                name,
                kind,
                parent: None,
                signature: signature(node, body, source),
                start_line: node.start_position().row + 1,
                end_line: node.end_position().row + 1,
                start_byte: declaration_start(node),
                end_byte: node.end_byte(),
                body,
                depth: 0,
            },
        ));
    }

    // Outer definitions first so the containment stack below works.
    found.sort_by(|(pa, a), (pb, b)| {
        a.start_byte
            .cmp(&b.start_byte)
            .then(b.end_byte.cmp(&a.end_byte))
            .then(pa.cmp(pb))
    });
    found.dedup_by(|(_, b), (_, a)| a.start_byte == b.start_byte && a.end_byte == b.end_byte);

    let mut symbols: Vec<Symbol> = Vec::with_capacity(found.len());
    let mut stack: Vec<usize> = Vec::new();
    for (_, mut sym) in found {
        while let Some(&top) = stack.last() {
            if symbols[top].end_byte >= sym.end_byte {
                break;
            }
            stack.pop();
        }
        if let Some(&top) = stack.last() {
            let parent = &symbols[top];
            if sym.kind == SymbolKind::Function && parent.kind.is_container() {
                sym.kind = SymbolKind::Method;
            }
            sym.parent = Some(parent.qualified_name());
            sym.depth = parent.depth + 1;
        }
        stack.push(symbols.len());
        symbols.push(sym);
    }
    Ok((symbols, timed_out.get()))
}

/// `const fs = require("fs")` is an import, not a definition of the file.
fn is_require(name: Node<'_>, source: &str) -> bool {
    name.parent()
        .filter(|d| d.kind() == "variable_declarator")
        .and_then(|d| d.child_by_field_name("value"))
        .filter(|v| v.kind() == "call_expression")
        .and_then(|call| call.child_by_field_name("function"))
        .is_some_and(|f| node_text(f, source) == "require")
}

fn node_text<'s>(node: Node<'_>, source: &'s str) -> &'s str {
    source.get(node.byte_range()).unwrap_or("")
}

/// In `const a = () => 1, b = () => 2` the declaration holds several functions,
/// so each one is only its declarator: as the whole declaration they would be
/// one symbol, and replacing it would delete the others.
fn own_declarator<'t>(node: Node<'t>, name: Node<'t>) -> Node<'t> {
    let mut cursor = node.walk();
    let declarators = node
        .named_children(&mut cursor)
        .filter(|c| c.kind() == "variable_declarator")
        .count();
    match name.parent() {
        Some(declarator) if declarators > 1 => declarator,
        _ => node,
    }
}

/// Locate the block that holds a definition's implementation.
fn find_body(node: Node<'_>) -> Option<Node<'_>> {
    if let Some(body) = node.child_by_field_name("body") {
        return Some(body);
    }
    if node.kind() == "variable_declarator" {
        return node
            .child_by_field_name("value")?
            .child_by_field_name("body");
    }
    // `const f = () => {...}` keeps the body on the declarator's value.
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .filter(|c| c.kind() == "variable_declarator")
        .find_map(|d| d.child_by_field_name("value"))
        .and_then(|v| v.child_by_field_name("body"))
}

const MAX_SIGNATURE_LEN: usize = 240;

/// Where the header ends: at the body, or before comments right above it.
fn header_end(node: Node<'_>, body_start: usize) -> usize {
    let mut cursor = node.walk();
    let mut end = body_start;
    let before_body: Vec<Node<'_>> = node
        .children(&mut cursor)
        .take_while(|child| child.end_byte() <= body_start)
        .collect();
    for child in before_body.iter().rev() {
        if !child.is_extra() {
            break;
        }
        end = child.start_byte();
    }
    end
}

/// The declaration header: text up to the body, whitespace-collapsed.
fn signature(node: Node<'_>, body: Option<(usize, usize)>, source: &str) -> String {
    let start = node.start_byte();
    let end = match body {
        Some((body_start, _)) => header_end(node, body_start),
        None => node.end_byte(),
    };
    let raw = source.get(start..end).unwrap_or("");
    // Without a body, keep only the first line (e.g. `type Foo = ...`).
    let raw = if body.is_none() {
        raw.lines().next().unwrap_or("")
    } else {
        raw
    };
    let mut sig = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed_len = sig
        .trim_end_matches(|c: char| c == '{' || c == ':' || c == '=' || c.is_whitespace())
        .len();
    sig.truncate(trimmed_len);
    if sig.len() > MAX_SIGNATURE_LEN {
        let mut cut = MAX_SIGNATURE_LEN;
        while !sig.is_char_boundary(cut) {
            cut -= 1;
        }
        sig.truncate(cut);
        sig.push('…');
    }
    sig
}

/// Byte offset of the start of the line holding `byte`.
fn line_start(source: &str, byte: usize) -> usize {
    source[..byte].rfind('\n').map_or(0, |i| i + 1)
}

/// 1-based line number of `byte`.
pub fn line_number(source: &str, byte: usize) -> usize {
    source[..byte].matches('\n').count() + 1
}

/// Where the lines of `sym` begin: the start of its first line, moved up
/// over the doc comments, attributes and decorators directly above it, but
/// never into the lines of a symbol that ends before it. When other code
/// precedes `sym` on its first line, that code is not part of the symbol, so
/// it begins exactly at its own first byte.
pub fn symbol_start(parsed: &ParsedFile, source: &str, sym: &Symbol) -> usize {
    let line = line_start(source, sym.start_byte);
    if !source[line..sym.start_byte].trim().is_empty() {
        return sym.start_byte;
    }
    let floor = parsed
        .symbols
        .iter()
        .filter(|s| s.end_byte <= sym.start_byte)
        .map(|s| s.end_byte)
        .max()
        .map_or(0, |end| {
            source[end..]
                .find('\n')
                .map_or(source.len(), |i| end + i + 1)
        });
    attached_start(parsed.lang, source, line).max(floor)
}

/// The first byte of the declaration around `node`, taking in what the grammar
/// keeps outside the definition but that belongs to it when it shares its
/// first line: `export` and `declare`, Go's `type`, `template <...>`,
/// `extern "C"`, and attributes or decorators written in front of it.
fn declaration_start(node: Node<'_>) -> usize {
    let row = node.start_position().row;
    let mut current = node;
    loop {
        if let Some(prev) = current.prev_named_sibling()
            && matches!(prev.kind(), "attribute_item" | "decorator")
            && prev.start_position().row == row
        {
            current = prev;
            continue;
        }
        let Some(parent) = current.parent() else {
            break;
        };
        let wraps = match parent.kind() {
            "export_statement"
            | "ambient_declaration"
            | "template_declaration"
            | "linkage_specification" => true,
            // `type T int` holds one spec; `type ( ... )` is a group of them.
            "type_declaration" | "const_declaration" | "var_declaration" => {
                parent.child_count() == 2
            }
            _ => false,
        };
        if !wraps || parent.start_position().row != row {
            break;
        }
        current = parent;
    }
    current.start_byte()
}

/// Move `start` (a line start) up over the comment, attribute and decorator
/// lines directly above it, so code inserted there does not split them from
/// the definition they belong to. Attributes and decorators spanning several
/// lines (`#[tool(\n  ...\n)]`, `@route(\n  ...\n)`) count as a whole.
fn attached_start(lang: Lang, source: &str, mut start: usize) -> usize {
    /// How far up a multi-line attribute's opening line is looked for.
    const MAX_ATTRIBUTE_LINES: usize = 30;
    let opens = |text: &str| match lang {
        Lang::Python | Lang::GdScript => text.starts_with('#') || text.starts_with('@'),
        _ => ["//", "/*", "*", "#[", "@", "["]
            .iter()
            .any(|p| text.starts_with(p)),
    };
    while start > 0 {
        let prev = line_start(source, start - 1);
        let text = source[prev..start].trim();
        if text.is_empty() {
            break;
        }
        if opens(text) {
            start = prev;
            continue;
        }
        // The closing line of a multi-line attribute: find where it opens,
        // without crossing a blank line.
        if !(text.ends_with(']') || text.ends_with(')')) {
            break;
        }
        let mut line = prev;
        let mut opening = None;
        for _ in 0..MAX_ATTRIBUTE_LINES {
            if line == 0 {
                break;
            }
            line = line_start(source, line - 1);
            let text = source[line..].lines().next().unwrap_or("").trim();
            if text.is_empty() {
                break;
            }
            if text.starts_with("#[") || text.starts_with('@') || text.starts_with('[') {
                opening = Some(line);
                break;
            }
        }
        match opening {
            Some(line) => start = line,
            None => break,
        }
    }
    start
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn functions_sharing_a_declaration_are_separate_symbols() {
        let source = "const a = () => { 1 }, n = 1, b = function () {};\nconst c = () => 3;\n";
        for lang in [Lang::JavaScript, Lang::TypeScript] {
            let parsed = parse_source(lang, source).unwrap();
            let found: Vec<_> = parsed
                .symbols
                .iter()
                .map(|s| (s.name.as_str(), &source[s.start_byte..s.end_byte]))
                .collect();
            assert_eq!(
                found,
                [
                    ("a", "a = () => { 1 }"),
                    ("n", "n = 1"),
                    ("b", "b = function () {}"),
                    ("c", "const c = () => 3;"),
                ]
            );
            let body = parsed.symbols[0].body.unwrap();
            assert_eq!(&source[body.0..body.1], "{ 1 }");
        }
    }

    fn names(parsed: &ParsedFile) -> Vec<(String, SymbolKind)> {
        parsed
            .symbols
            .iter()
            .map(|s| (s.qualified_name(), s.kind))
            .collect()
    }

    #[test]
    fn all_queries_compile() {
        for lang in Lang::ALL {
            lang.query()
                .unwrap_or_else(|e| panic!("{}: {e}", lang.name()));
        }
    }

    #[test]
    fn index_is_the_position_in_all() {
        for (i, lang) in Lang::ALL.into_iter().enumerate() {
            assert_eq!(lang.index(), i, "{lang:?}");
            assert_eq!(Lang::from_index(i), Some(lang));
        }
        for (i, kind) in SymbolKind::ALL.into_iter().enumerate() {
            assert_eq!(kind.index(), i, "{kind:?}");
            assert_eq!(SymbolKind::from_index(i), Some(kind));
        }
    }

    #[test]
    fn detects_language_from_extension() {
        assert_eq!(Lang::from_path(Path::new("a/b.rs")), Some(Lang::Rust));
        assert_eq!(Lang::from_path(Path::new("x.TSX")), Some(Lang::Tsx));
        assert_eq!(Lang::from_path(Path::new("x.mjs")), Some(Lang::JavaScript));
        assert_eq!(Lang::from_path(Path::new("x.h")), Some(Lang::C));
        assert_eq!(Lang::from_path(Path::new("x.hpp")), Some(Lang::Cpp));
        assert_eq!(Lang::from_path(Path::new("X.java")), Some(Lang::Java));
        assert_eq!(Lang::from_path(Path::new("X.cs")), Some(Lang::CSharp));
        assert_eq!(
            Lang::from_path(Path::new("player.gd")),
            Some(Lang::GdScript)
        );
        assert_eq!(Lang::from_path(Path::new("Makefile")), None);
    }

    #[test]
    fn rust_symbols() {
        let src = r#"
struct Point { x: i32 }
enum Color { Red }
trait Shape { fn area(&self) -> f64; }
impl Point {
    pub fn new(x: i32) -> Self {
        Point { x }
    }
}
mod util {
    pub fn helper() {}
}
const MAX: usize = 3;
macro_rules! m { () => {} }
"#;
        let parsed = parse_source(Lang::Rust, src).unwrap();
        assert!(!parsed.has_errors);
        assert_eq!(
            names(&parsed),
            vec![
                ("Point".into(), SymbolKind::Struct),
                ("Color".into(), SymbolKind::Enum),
                ("Shape".into(), SymbolKind::Trait),
                ("Shape::area".into(), SymbolKind::Method),
                ("Point".into(), SymbolKind::Impl),
                ("Point::new".into(), SymbolKind::Method),
                ("util".into(), SymbolKind::Module),
                ("util::helper".into(), SymbolKind::Function),
                ("MAX".into(), SymbolKind::Constant),
                ("m".into(), SymbolKind::Macro),
            ]
        );
        let new = &parsed.symbols[5];
        assert_eq!(new.signature, "pub fn new(x: i32) -> Self");
        assert_eq!((new.start_line, new.end_line), (6, 8));
        assert!(new.body.is_some());
    }

    #[test]
    fn python_symbols() {
        let src = "class A:\n    def m(self, x):\n        return x\n\ndef f():\n    pass\n";
        let parsed = parse_source(Lang::Python, src).unwrap();
        assert_eq!(
            names(&parsed),
            vec![
                ("A".into(), SymbolKind::Class),
                ("A::m".into(), SymbolKind::Method),
                ("f".into(), SymbolKind::Function),
            ]
        );
        assert_eq!(parsed.symbols[1].signature, "def m(self, x)");
    }

    #[test]
    fn javascript_symbols() {
        let src = "function a() {}\nconst b = (x) => { return x; };\nclass C { run() {} }\n";
        let parsed = parse_source(Lang::JavaScript, src).unwrap();
        assert_eq!(
            names(&parsed),
            vec![
                ("a".into(), SymbolKind::Function),
                ("b".into(), SymbolKind::Function),
                ("C".into(), SymbolKind::Class),
                ("C::run".into(), SymbolKind::Method),
            ]
        );
        assert!(parsed.symbols[1].body.is_some());
    }

    #[test]
    fn typescript_symbols() {
        let src = "interface I { go(): void }\ntype T = number;\nexport class K { m(): void {} }\nenum E { A }\n";
        let parsed = parse_source(Lang::TypeScript, src).unwrap();
        assert_eq!(
            names(&parsed),
            vec![
                ("I".into(), SymbolKind::Interface),
                ("I::go".into(), SymbolKind::Method),
                ("T".into(), SymbolKind::Type),
                ("K".into(), SymbolKind::Class),
                ("K::m".into(), SymbolKind::Method),
                ("E".into(), SymbolKind::Enum),
            ]
        );
    }

    #[test]
    fn go_symbols() {
        let src = "package p\ntype S struct { A int }\ntype R interface { Do() }\ntype N int\nfunc F() {}\nfunc (s *S) M() {}\n";
        let parsed = parse_source(Lang::Go, src).unwrap();
        assert_eq!(
            names(&parsed),
            vec![
                ("S".into(), SymbolKind::Struct),
                ("R".into(), SymbolKind::Interface),
                ("N".into(), SymbolKind::Type),
                ("F".into(), SymbolKind::Function),
                ("M".into(), SymbolKind::Method),
            ]
        );
    }

    #[test]
    fn c_symbols() {
        let src = "#define MAX 10\n#define SQ(x) ((x) * (x))\nstruct point { int x; };\nenum color { RED };\ntypedef unsigned long ulong;\nint add(int a, int b);\nint add(int a, int b) { return a + b; }\nchar *name(void) { return 0; }\n";
        let parsed = parse_source(Lang::C, src).unwrap();
        assert_eq!(
            names(&parsed),
            vec![
                ("MAX".into(), SymbolKind::Macro),
                ("SQ".into(), SymbolKind::Macro),
                ("point".into(), SymbolKind::Struct),
                ("color".into(), SymbolKind::Enum),
                ("ulong".into(), SymbolKind::Type),
                ("add".into(), SymbolKind::Function),
                ("add".into(), SymbolKind::Function),
                ("name".into(), SymbolKind::Function),
            ]
        );
        let add = &parsed.symbols[6];
        assert_eq!(add.signature, "int add(int a, int b)");
        assert!(add.body.is_some());
    }

    #[test]
    fn cpp_symbols() {
        let src = "namespace ns {\nclass Widget {\n public:\n  Widget();\n  ~Widget() {}\n  int size() const { return 0; }\n};\nWidget::Widget() {}\nint &Widget::ref() { return n; }\nusing Id = int;\n}\n";
        let parsed = parse_source(Lang::Cpp, src).unwrap();
        let got: Vec<_> = parsed
            .symbols
            .iter()
            .map(|s| (s.qualified_name(), s.kind))
            .collect();
        assert_eq!(
            got,
            vec![
                ("ns".into(), SymbolKind::Module),
                ("ns::Widget".into(), SymbolKind::Class),
                ("ns::Widget::Widget".into(), SymbolKind::Method),
                ("ns::Widget::~Widget".into(), SymbolKind::Method),
                ("ns::Widget::size".into(), SymbolKind::Method),
                ("ns::Widget".into(), SymbolKind::Function),
                ("ns::ref".into(), SymbolKind::Function),
                ("ns::Id".into(), SymbolKind::Type),
            ]
        );
    }

    #[test]
    fn java_symbols() {
        let src = "package p;\npublic class Shop {\n  public Shop() {}\n  void buy(int n) {}\n  interface Item { int price(); }\n  enum Size { S, M }\n  record Pair(int a, int b) {}\n}\n";
        let parsed = parse_source(Lang::Java, src).unwrap();
        let got: Vec<_> = parsed
            .symbols
            .iter()
            .map(|s| (s.qualified_name(), s.kind))
            .collect();
        assert_eq!(
            got,
            vec![
                ("Shop".into(), SymbolKind::Class),
                ("Shop::Shop".into(), SymbolKind::Method),
                ("Shop::buy".into(), SymbolKind::Method),
                ("Shop::Item".into(), SymbolKind::Interface),
                ("Shop::Item::price".into(), SymbolKind::Method),
                ("Shop::Size".into(), SymbolKind::Enum),
                ("Shop::Pair".into(), SymbolKind::Class),
            ]
        );
    }

    #[test]
    fn csharp_symbols() {
        let src = "namespace App {\n  public struct Point { }\n  interface IShape { double Area(); }\n  public class Circle : IShape {\n    public Circle() { }\n    public double Area() { return 0; }\n  }\n  enum Kind { A }\n  delegate void Handler();\n}\n";
        let parsed = parse_source(Lang::CSharp, src).unwrap();
        let got: Vec<_> = parsed
            .symbols
            .iter()
            .map(|s| (s.qualified_name(), s.kind))
            .collect();
        assert_eq!(
            got,
            vec![
                ("App".into(), SymbolKind::Module),
                ("App::Point".into(), SymbolKind::Struct),
                ("App::IShape".into(), SymbolKind::Interface),
                ("App::IShape::Area".into(), SymbolKind::Method),
                ("App::Circle".into(), SymbolKind::Class),
                ("App::Circle::Circle".into(), SymbolKind::Method),
                ("App::Circle::Area".into(), SymbolKind::Method),
                ("App::Kind".into(), SymbolKind::Enum),
                ("App::Handler".into(), SymbolKind::Type),
            ]
        );
    }

    #[test]
    fn gdscript_symbols() {
        let src = "class_name Player\nextends Node2D\n\nsignal hit(damage: int)\nconst SPEED := 300.0\nenum State { IDLE, RUN }\n@export var health := 10\n\n# Moves the player.\n@rpc\nfunc move(delta: float) -> void:\n\tposition.x += SPEED * delta\n\nclass Inventory:\n\tfunc add(item):\n\t\tpass\n";
        let parsed = parse_source(Lang::GdScript, src).unwrap();
        assert!(!parsed.has_errors);
        assert_eq!(
            names(&parsed),
            vec![
                ("hit".into(), SymbolKind::Function),
                ("SPEED".into(), SymbolKind::Constant),
                ("State".into(), SymbolKind::Enum),
                ("health".into(), SymbolKind::Variable),
                ("move".into(), SymbolKind::Function),
                ("Inventory".into(), SymbolKind::Class),
                ("Inventory::add".into(), SymbolKind::Method),
            ]
        );
        let mv = &parsed.symbols[4];
        assert_eq!(mv.signature, "func move(delta: float) -> void");
        assert!(mv.body.is_some());
        assert_eq!(parsed.symbols[0].signature, "signal hit(damage: int)");
        // The comment and annotation above `move` belong to it.
        let start = symbol_start(&parsed, src, mv);
        assert!(src[start..].starts_with("# Moves"), "{:?}", &src[start..]);
    }

    #[test]
    fn top_level_variables_are_symbols() {
        let js = "const fs = require('fs');\nexport const CARDS = [1, 2];\nlet handlers = {}, f = () => 1;\nvar old = 1;\nfunction g() { const local = 1; }\nconst { a, b } = obj;\n";
        for lang in [Lang::JavaScript, Lang::TypeScript] {
            let parsed = parse_source(lang, js).unwrap();
            assert_eq!(
                names(&parsed),
                vec![
                    ("CARDS".into(), SymbolKind::Constant),
                    ("handlers".into(), SymbolKind::Variable),
                    ("f".into(), SymbolKind::Function),
                    ("old".into(), SymbolKind::Variable),
                    ("g".into(), SymbolKind::Function),
                ],
                "{}",
                lang.name()
            );
            let cards = &parsed.symbols[0];
            assert_eq!(cards.signature, "const CARDS = [1, 2];");
            assert_eq!(
                &js[cards.start_byte..cards.end_byte],
                "export const CARDS = [1, 2];"
            );
        }

        let py = "import os\nMAX_SIZE = 10\n_PRIVATE = 1\ncache: dict = {}\n\ndef f():\n    local = 1\n\nclass A:\n    field = 1\n";
        let parsed = parse_source(Lang::Python, py).unwrap();
        assert_eq!(
            names(&parsed),
            vec![
                ("MAX_SIZE".into(), SymbolKind::Constant),
                ("_PRIVATE".into(), SymbolKind::Constant),
                ("cache".into(), SymbolKind::Variable),
                ("f".into(), SymbolKind::Function),
                ("A".into(), SymbolKind::Class),
            ]
        );

        let go = "package p\nconst Max = 3\nconst (\n\tA = iota\n\tB\n)\nvar Debug = false\nvar (\n\tx int\n)\nfunc f() { const local = 1; var v int }\n";
        let parsed = parse_source(Lang::Go, go).unwrap();
        assert_eq!(
            names(&parsed),
            vec![
                ("Max".into(), SymbolKind::Constant),
                ("A".into(), SymbolKind::Constant),
                ("B".into(), SymbolKind::Constant),
                ("Debug".into(), SymbolKind::Variable),
                ("x".into(), SymbolKind::Variable),
                ("f".into(), SymbolKind::Function),
            ]
        );
        // A lone spec takes its keyword along; one in a group does not.
        let max = &parsed.symbols[0];
        assert_eq!(&go[max.start_byte..max.end_byte], "const Max = 3");
        let a = &parsed.symbols[1];
        assert_eq!(&go[a.start_byte..a.end_byte], "A = iota");
    }

    #[test]
    fn symbol_query_stops_when_out_of_time() {
        let source = format!("function early() {{}}\n{}", "[".repeat(50_000));
        let lang = Lang::JavaScript;
        let mut parser = Parser::new();
        parser.set_language(&lang.grammar()).unwrap();
        let tree = parser.parse(&source, None).unwrap();

        let budget = Duration::from_millis(50);
        let started = Instant::now();
        let (symbols, incomplete) =
            extract_symbols(lang, tree.root_node(), &source, budget).unwrap();
        assert!(incomplete);
        // Generous for a busy CI runner, yet well under the full query, which
        // takes several seconds even without load.
        let elapsed = started.elapsed();
        assert!(elapsed < Duration::from_secs(5), "took {elapsed:?}");
        assert_eq!(symbols[0].name, "early");

        let parsed = parse_source(lang, "function f() {}\n").unwrap();
        assert!(!parsed.incomplete);
    }

    #[test]
    fn signature_stops_before_comments_above_the_body() {
        let src = "def f(a):  # why\n    # more\n    return a\n";
        let parsed = parse_source(Lang::Python, src).unwrap();
        assert_eq!(parsed.symbols[0].signature, "def f(a)");

        let src = "fn g() /* note */ {\n}\n";
        let parsed = parse_source(Lang::Rust, src).unwrap();
        assert_eq!(parsed.symbols[0].signature, "fn g()");

        // A body nested in a child keeps the header in front of it.
        let src = "const h = (a) => {\n  return a;\n};\n";
        let parsed = parse_source(Lang::JavaScript, src).unwrap();
        assert!(
            parsed.symbols[0].signature.contains("(a)"),
            "{:?}",
            parsed.symbols
        );
    }

    #[test]
    fn broken_source_degrades_gracefully() {
        let src = "fn ok() {}\nfn broken( {\nstruct After;\n";
        let parsed = parse_source(Lang::Rust, src).unwrap();
        assert!(parsed.has_errors);
        assert!(parsed.symbols.iter().any(|s| s.name == "ok"));
    }

    #[test]
    fn every_language_survives_broken_input() {
        let cases = [
            (Lang::Rust, "fn good() {}\nimpl X { fn half(&self", "good"),
            (
                Lang::Python,
                "def good():\n    pass\nclass Broken(:\n",
                "good",
            ),
            (Lang::JavaScript, "function good() {}\nclass { (\n", "good"),
            (
                Lang::TypeScript,
                "function good(): void {}\ninterface <\n",
                "good",
            ),
            (
                Lang::Tsx,
                "function good() { return <div />; }\nconst x = <span (\n",
                "good",
            ),
            (Lang::Go, "package p\nfunc good() {}\nfunc (\n", "good"),
            (
                Lang::C,
                "int good(void) { return 0; }\nint bad( {\n",
                "good",
            ),
            (Lang::Cpp, "void good() {}\nclass X { void half(\n", "good"),
            (Lang::Java, "class Good { void m() {} }\nclass {\n", "Good"),
            (
                Lang::CSharp,
                "class Good { void M() {} }\nclass {\n",
                "Good",
            ),
            (Lang::GdScript, "func good():\n\tpass\nfunc (:\n", "good"),
        ];
        for (lang, src, expected) in cases {
            let parsed = parse_source(lang, src).unwrap_or_else(|e| panic!("{}: {e}", lang.name()));
            assert!(parsed.has_errors, "{}: error not flagged", lang.name());
            assert!(
                parsed.symbols.iter().any(|s| s.name == expected),
                "{}: lost `{expected}` in {:?}",
                lang.name(),
                parsed.symbols
            );
        }
        // Degenerate inputs must not panic either.
        for lang in Lang::ALL {
            for src in ["", "\0\0\0", "{{{{{{", "\u{feff}é漢字"] {
                parse_source(lang, src).unwrap();
            }
        }
    }

    #[test]
    fn identifier_occurrences_skip_comments_and_strings() {
        let cases = [
            (
                Lang::Rust,
                "// load here\nfn load() {}\nfn main() { let s = \"load\"; load(); x.load; }\n",
                vec![2, 3, 3],
            ),
            (
                Lang::Python,
                "# load\ndef load():\n    pass\nload()\nobj.load()\ns = 'load'\n",
                vec![2, 4, 5],
            ),
            (
                Lang::JavaScript,
                "// load\nfunction load() {}\nload();\nobj.load();\nconst s = 'load';\n",
                vec![2, 3, 4],
            ),
            (
                Lang::TypeScript,
                "type load = number;\nlet x: load = 1; // load\n",
                vec![1, 2],
            ),
            (
                Lang::Go,
                "package p\n// load\nfunc load() {}\nfunc m() { load(); s := \"load\" }\n",
                vec![3, 4],
            ),
            (
                Lang::C,
                "/* load */\nint load(void);\nint main() { load(); char *s = \"load\"; }\n",
                vec![2, 3],
            ),
            (
                Lang::Cpp,
                "// load\nstruct A { void load(); };\nvoid A::load() {}\nvoid f(A a) { a.load(); }\n",
                vec![2, 3, 4],
            ),
            (
                Lang::Java,
                "class A {\n  // load\n  void load() {}\n  void f() { load(); String s = \"load\"; }\n}\n",
                vec![3, 4],
            ),
            (
                Lang::CSharp,
                "class A {\n  // load\n  void load() {}\n  void F() { load(); var s = \"load\"; }\n}\n",
                vec![3, 4],
            ),
            (
                Lang::GdScript,
                "# load\nfunc load():\n\tpass\nfunc f():\n\tload()\n\tobj.load()\n\tvar s = \"load\"\n",
                vec![2, 5, 6],
            ),
        ];
        for (lang, src, expected) in cases {
            let parsed = parse_source(lang, src).unwrap();
            let lines: Vec<usize> = identifier_occurrences(&parsed, src, "load")
                .iter()
                .map(|o| o.line)
                .collect();
            assert_eq!(lines, expected, "{}", lang.name());
        }
        let src = "fn loader() {}";
        let parsed = parse_source(Lang::Rust, src).unwrap();
        assert!(identifier_occurrences(&parsed, src, "load").is_empty());

        let src = "fn f() {\n    x.load(load);\n}\n";
        let parsed = parse_source(Lang::Rust, src).unwrap();
        let cols: Vec<_> = identifier_occurrences(&parsed, src, "load")
            .iter()
            .map(|o| (o.line, o.column))
            .collect();
        assert_eq!(cols, [(2, 6), (2, 11)]);
    }

    #[test]
    fn idents_round_trip_through_json_as_deltas() {
        let src = "fn a() { b(c) }\n";
        let parsed = parse_source(Lang::Rust, src).unwrap();
        assert_eq!(parsed.idents, [3, 9, 11]);
        let json = serde_json::to_string(&parsed).unwrap();
        assert!(json.contains("\"idents\":[3,6,2]"), "{json}");
        let back: ParsedFile = serde_json::from_str(&json).unwrap();
        assert_eq!(back.idents, parsed.idents);
    }

    #[test]
    fn symbol_start_includes_attached_lines() {
        let start_line = |lang, src: &str, name: &str| {
            let parsed = parse_source(lang, src).unwrap();
            let sym = parsed.symbols.iter().find(|s| s.name == name).unwrap();
            line_number(src, symbol_start(&parsed, src, sym))
        };
        let rust = "fn a() {}\n\n/// Doc.\n#[tool(\n    description = \"x\"\n)]\nfn b() {}\n";
        assert_eq!(start_line(Lang::Rust, rust, "b"), 3);
        let py = "x = f(\n    1,\n)\n\n@route(\n    '/a',\n)\ndef view():\n    pass\n";
        assert_eq!(start_line(Lang::Python, py, "view"), 5);
        // A call ending in `)` right above is code, not an attribute.
        let py = "x = f(\n    1,\n)\ndef g():\n    pass\n";
        assert_eq!(start_line(Lang::Python, py, "g"), 4);
        // Never reaches into the previous symbol, even without a blank line.
        let rust = "#[inline] fn a() {}\n#[cfg(test)]\nfn b() {}\n";
        assert_eq!(start_line(Lang::Rust, rust, "b"), 2);
    }

    #[test]
    fn symbol_start_stops_at_other_code_on_its_line() {
        let start = |lang, src: &str, name: &str| {
            let parsed = parse_source(lang, src).unwrap();
            let sym = parsed.symbols.iter().find(|s| s.name == name).unwrap();
            let start = symbol_start(&parsed, src, sym);
            assert!(start <= sym.end_byte, "{name} starts after its end");
            src[start..].to_string()
        };
        assert_eq!(
            start(Lang::Rust, "struct A; struct B;\n", "B"),
            "struct B;\n"
        );
        assert_eq!(
            start(Lang::JavaScript, "const k = 1; function f() {}\n", "f"),
            "function f() {}\n"
        );
        // Indentation alone is not other code.
        assert_eq!(
            start(Lang::Rust, "impl P {\n    fn m(&self) {}\n}\n", "m"),
            "    fn m(&self) {}\n}\n"
        );
        // Only a prefix on the definition's own line belongs to it.
        assert_eq!(
            start(Lang::Go, "package p\n\ntype (\n\tT int\n)\n", "T"),
            "\tT int\n)\n"
        );
        assert_eq!(
            start(Lang::Go, "package p\n\ntype ( T int; U int )\n", "U"),
            "U int )\n"
        );
    }

    #[test]
    fn headers_fall_back_to_cpp() {
        let path = Path::new("x.h");
        let c = parse_path(path, "struct point { int x; };\nint add(int a, int b);\n").unwrap();
        assert_eq!(c.lang, Lang::C);

        let src = "namespace n {\nclass Foo {\npublic:\n  int bar() { return 1; }\n};\n}\n";
        let cpp = parse_path(path, src).unwrap();
        assert_eq!(cpp.lang, Lang::Cpp);
        assert!(!cpp.has_errors);
        let bar = cpp.symbols.iter().find(|s| s.name == "bar").unwrap();
        assert_eq!(bar.qualified_name(), "n::Foo::bar");

        // Only `.h` is ambiguous.
        assert_eq!(parse_path(Path::new("x.c"), src).unwrap().lang, Lang::C);
    }

    #[test]
    fn unsupported_extension_is_an_error() {
        let err = parse_path(Path::new("notes.txt"), "hi").unwrap_err();
        assert!(matches!(err, ParseError::UnsupportedLanguage(_)));
    }
}
