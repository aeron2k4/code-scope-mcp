//! Tree-sitter parsing and symbol extraction.
//!
//! Each supported language has a symbol query whose patterns capture a
//! definition node (`@function`, `@class`, ...) and its `@name`. Parsing never
//! fails on syntactically broken input: tree-sitter recovers with `ERROR`
//! nodes, and we report `has_errors` instead of bailing out.

use std::path::Path;
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};
use streaming_iterator::StreamingIterator;
use tree_sitter::{Node, Parser, Query, QueryCursor};

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
}

impl Lang {
    pub const ALL: [Lang; 6] = [
        Lang::Rust,
        Lang::Python,
        Lang::JavaScript,
        Lang::TypeScript,
        Lang::Tsx,
        Lang::Go,
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
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Lang::Rust => "rust",
            Lang::Python => "python",
            Lang::JavaScript => "javascript",
            Lang::TypeScript => "typescript",
            Lang::Tsx => "tsx",
            Lang::Go => "go",
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
        }
    }

    fn query_source(self) -> String {
        match self {
            Lang::Rust => RUST_QUERY.to_string(),
            Lang::Python => PYTHON_QUERY.to_string(),
            Lang::JavaScript => JS_QUERY.to_string(),
            Lang::TypeScript | Lang::Tsx => format!("{TS_EXTRA_QUERY}\n{JS_QUERY}"),
            Lang::Go => GO_QUERY.to_string(),
        }
    }

    /// Compiled symbol query, built once per language.
    fn query(self) -> Result<&'static Query, ParseError> {
        static QUERIES: [OnceLock<Result<Query, String>>; 6] = [const { OnceLock::new() }; 6];
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
}

pub fn parse_path(path: &Path, source: &str) -> Result<ParsedFile, ParseError> {
    let lang = Lang::from_path(path)
        .ok_or_else(|| ParseError::UnsupportedLanguage(path.display().to_string()))?;
    parse_source(lang, source)
}

pub fn parse_source(lang: Lang, source: &str) -> Result<ParsedFile, ParseError> {
    let mut parser = Parser::new();
    parser.set_language(&lang.grammar())?;
    let tree = parser.parse(source, None).ok_or(ParseError::NoTree)?;
    let root = tree.root_node();
    let symbols = extract_symbols(lang, root, source)?;
    Ok(ParsedFile {
        lang,
        symbols,
        has_errors: root.has_error(),
    })
}

fn extract_symbols(lang: Lang, root: Node<'_>, source: &str) -> Result<Vec<Symbol>, ParseError> {
    let query = lang.query()?;
    let names = query.capture_names();
    let bytes = source.as_bytes();

    // (pattern_index, symbol) so overlapping patterns can be de-duplicated
    // in favour of the more specific (earlier) one.
    let mut found: Vec<(usize, Symbol)> = Vec::new();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(query, root, bytes);
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
        if name.is_empty() {
            continue;
        }
        let body = find_body(node).map(|b| (b.start_byte(), b.end_byte()));
        found.push((
            m.pattern_index,
            Symbol {
                name,
                kind,
                parent: None,
                signature: signature(node, body, source),
                start_line: node.start_position().row + 1,
                end_line: node.end_position().row + 1,
                start_byte: node.start_byte(),
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
    Ok(symbols)
}

fn node_text<'s>(node: Node<'_>, source: &'s str) -> &'s str {
    source.get(node.byte_range()).unwrap_or("")
}

/// Locate the block that holds a definition's implementation.
fn find_body(node: Node<'_>) -> Option<Node<'_>> {
    if let Some(body) = node.child_by_field_name("body") {
        return Some(body);
    }
    // `const f = () => {...}` keeps the body on the declarator's value.
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .filter(|c| c.kind() == "variable_declarator")
        .find_map(|d| d.child_by_field_name("value"))
        .and_then(|v| v.child_by_field_name("body"))
}

const MAX_SIGNATURE_LEN: usize = 240;

/// The declaration header: text up to the body, whitespace-collapsed.
fn signature(node: Node<'_>, body: Option<(usize, usize)>, source: &str) -> String {
    let start = node.start_byte();
    let end = match body {
        Some((body_start, _)) => body_start,
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn detects_language_from_extension() {
        assert_eq!(Lang::from_path(Path::new("a/b.rs")), Some(Lang::Rust));
        assert_eq!(Lang::from_path(Path::new("x.TSX")), Some(Lang::Tsx));
        assert_eq!(Lang::from_path(Path::new("x.mjs")), Some(Lang::JavaScript));
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
    fn unsupported_extension_is_an_error() {
        let err = parse_path(Path::new("notes.txt"), "hi").unwrap_err();
        assert!(matches!(err, ParseError::UnsupportedLanguage(_)));
    }
}
