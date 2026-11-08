//! Compact binary form of a parsed file, for the on-disk store.
//!
//! JSON writes every symbol number as decimal digits and repeats the field
//! names, so a file with thousands of identifiers spends most of its bytes on
//! separators and offsets. This form writes numbers as varints and stores the
//! repeated strings (parents, signatures) once, which is about 40% of the JSON
//! size on Go and Python sources.
//!
//! The format is private to the store: a mismatch is a miss, never a wrong
//! result, because the store is named after the format's version.

use crate::ast::parser::{Lang, ParsedFile, Symbol, SymbolKind};

/// Why bytes in the store could not be read back. The store treats any of
/// these as a miss; they only show up in logs.
#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    #[error("stored parse is truncated")]
    Truncated,
    #[error("stored parse has an unknown {what}")]
    Unknown { what: &'static str },
    #[error("stored parse is not text")]
    NotText,
}

type Result<T> = std::result::Result<T, DecodeError>;

/// Encode `parsed` for the store.
pub fn encode(parsed: &ParsedFile) -> Vec<u8> {
    let mut strings = Strings::default();
    // Index 0 is the empty string, the parent of a top-level symbol.
    strings.put(&mut Vec::new(), "");
    let mut body = Vec::new();
    put(&mut body, parsed.symbols.len());
    for sym in &parsed.symbols {
        strings.put(&mut body, &sym.name);
        put(&mut body, sym.kind.index());
        match &sym.parent {
            Some(parent) => strings.put(&mut body, parent),
            None => put(&mut body, 0),
        }
        strings.put(&mut body, &sym.signature);
        for n in [
            sym.start_line,
            sym.end_line,
            sym.start_byte,
            sym.end_byte,
            sym.depth,
        ] {
            put(&mut body, n);
        }
        // A start of 0 counts as no body: a body opens after the definition's
        // header, so it never starts at the first byte of the file.
        match sym.body {
            Some((start, end)) => {
                put(&mut body, start + 1);
                put(&mut body, end);
            }
            None => {
                put(&mut body, 0);
                put(&mut body, 0);
            }
        }
    }
    put(&mut body, parsed.idents.len());
    let mut previous = 0usize;
    for &start in &parsed.idents {
        put(&mut body, (start as usize).saturating_sub(previous));
        previous = start as usize;
    }

    // Each string is written after its length, since a name or signature may
    // hold any byte, NUL included.
    let table_bytes: usize = strings.table.iter().map(|s| s.len() + 2).sum();
    let mut out = Vec::with_capacity(table_bytes + body.len() + 8);
    put(&mut out, strings.table.len());
    for string in &strings.table {
        put(&mut out, string.len());
        out.extend_from_slice(string);
    }
    out.extend_from_slice(&body);
    out
}

/// Decode bytes written by [`encode`].
pub fn decode(bytes: &[u8], lang: Lang, has_errors: bool, incomplete: bool) -> Result<ParsedFile> {
    let mut at = 0;
    let count = take(&mut at, bytes)?;
    // Index 0 of the table is the empty string, which stands for a symbol
    // without a parent.
    let mut table: Vec<&str> = Vec::with_capacity(count.min(1024));
    for _ in 0..count {
        let len = take(&mut at, bytes)?;
        let raw = at
            .checked_add(len)
            .and_then(|end| bytes.get(at..end))
            .ok_or(DecodeError::Truncated)?;
        table.push(std::str::from_utf8(raw).map_err(|_| DecodeError::NotText)?);
        at += len;
    }
    let string = |at: &mut usize| -> Result<&str> {
        let index = take(at, bytes)?;
        table
            .get(index)
            .copied()
            .ok_or(DecodeError::Unknown { what: "string" })
    };

    let count = take(&mut at, bytes)?;
    let mut symbols = Vec::with_capacity(count.min(4096));
    for _ in 0..count {
        let name = string(&mut at)?.to_string();
        let kind = SymbolKind::from_index(take(&mut at, bytes)?).ok_or(DecodeError::Unknown {
            what: "symbol kind",
        })?;
        let parent = string(&mut at)?;
        let parent = (!parent.is_empty()).then(|| parent.to_string());
        let signature = string(&mut at)?.to_string();
        let numbers: Vec<usize> = (0..5)
            .map(|_| take(&mut at, bytes))
            .collect::<Result<_>>()?;
        let body_start = take(&mut at, bytes)?;
        let body_end = take(&mut at, bytes)?;
        symbols.push(Symbol {
            name,
            kind,
            parent,
            signature,
            start_line: numbers[0],
            end_line: numbers[1],
            start_byte: numbers[2],
            end_byte: numbers[3],
            depth: numbers[4],
            body: if body_start > 0 {
                Some((body_start - 1, body_end))
            } else {
                None
            },
        });
    }

    let count = take(&mut at, bytes)?;
    let mut idents = Vec::with_capacity(count.min(1 << 16));
    let mut start = 0usize;
    for _ in 0..count {
        start = start.saturating_add(take(&mut at, bytes)?);
        let start = u32::try_from(start).map_err(|_| DecodeError::Unknown { what: "offset" })?;
        idents.push(start);
    }
    Ok(ParsedFile {
        lang,
        symbols,
        has_errors,
        incomplete,
        idents,
    })
}

/// The string table of one file, built as the symbols are written.
#[derive(Default)]
struct Strings {
    table: Vec<Vec<u8>>,
    /// The empty string is always index 0, for a symbol without a parent.
    seen: std::collections::HashMap<Vec<u8>, usize>,
}

impl Strings {
    fn put(&mut self, out: &mut Vec<u8>, text: &str) {
        let index = match self.seen.get(text.as_bytes()) {
            Some(&index) => index,
            None => {
                let index = self.table.len();
                let bytes = text.as_bytes().to_vec();
                self.seen.insert(bytes.clone(), index);
                self.table.push(bytes);
                index
            }
        };
        put(out, index);
    }
}

fn put(out: &mut Vec<u8>, mut value: usize) {
    while value >= 0x80 {
        out.push((value as u8) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn take(at: &mut usize, bytes: &[u8]) -> Result<usize> {
    let mut value = 0usize;
    let mut shift = 0;
    loop {
        let byte = *bytes.get(*at).ok_or(DecodeError::Truncated)?;
        *at += 1;
        value |= ((byte & 0x7f) as usize)
            .checked_shl(shift)
            .ok_or(DecodeError::Truncated)?;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
        shift += 7;
        if shift > usize::BITS {
            return Err(DecodeError::Truncated);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::parser;

    const SOURCE: &str =
        "pub struct Cfg;\n\nimpl Cfg {\n    pub fn load() -> Self {\n        Cfg\n    }\n}\n";

    #[test]
    fn round_trips_a_parsed_file() {
        let parsed = parser::parse_path(std::path::Path::new("a.rs"), SOURCE).unwrap();
        let bytes = encode(&parsed);
        let back = decode(&bytes, parsed.lang, parsed.has_errors, parsed.incomplete).unwrap();
        assert_eq!(back.symbols, parsed.symbols);
        assert_eq!(back.idents, parsed.idents);
        assert_eq!(back.lang, parsed.lang);
    }

    #[test]
    fn an_empty_parent_is_none_not_the_empty_name() {
        let parsed = parser::parse_source(Lang::Rust, "fn a() {}\n").unwrap();
        let bytes = encode(&parsed);
        let back = decode(&bytes, parsed.lang, false, false).unwrap();
        assert_eq!(back.symbols[0].parent, None);
        assert_eq!(back.symbols[0].name, "a");
    }

    #[test]
    fn a_nul_inside_a_signature_survives() {
        let mut parsed = parser::parse_path(std::path::Path::new("a.rs"), SOURCE).unwrap();
        parsed.symbols[0].signature = "fn f(sep = \"\0\")".to_string();
        parsed.symbols[0].name = "\0".to_string();
        let bytes = encode(&parsed);
        let back = decode(&bytes, parsed.lang, false, false).unwrap();
        assert_eq!(back.symbols, parsed.symbols);
        assert_eq!(back.idents, parsed.idents);
    }

    #[test]
    fn damaged_bytes_are_an_error() {
        let parsed = parser::parse_path(std::path::Path::new("a.rs"), SOURCE).unwrap();
        let bytes = encode(&parsed);
        assert!(decode(&bytes[..bytes.len() / 2], parsed.lang, false, false).is_err());
        assert!(decode(&[0xff; 8], parsed.lang, false, false).is_err());
    }
}
