//! Front-end: Verible parsing + CST→AST lowering.
//!
//! [`parse_source`] runs the vendored `verible-verilog-syntax` on a string of
//! SystemVerilog and lowers the resulting JSON CST to the [`eevee_ast`] AST.
//! Verible runs as a subprocess (no FFI) per `docs/frontend-decision.md`; the
//! CST plumbing is in [`cst`] and the lowering map in [`lower`].

#![forbid(unsafe_code)]

mod conformance;
pub mod cst;
pub mod lower;
pub mod preprocess;

pub use cst::{find_verible, FeError};
pub use lower::lower_file;
pub use preprocess::Preprocessor;

use eevee_ast::SourceFile;
use std::path::{Path, PathBuf};

/// Whether parsing may retain legacy best-effort fallbacks or must enforce the
/// currently validated fail-closed subset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseMode {
    Permissive,
    Conformance,
}

/// Parse a string of SystemVerilog into a [`SourceFile`] AST.
pub fn parse_source(src: &str) -> Result<SourceFile, FeError> {
    parse_source_with_mode(src, ParseMode::Permissive)
}

/// Parse source in fail-closed conformance mode.
pub fn parse_source_conformant(src: &str) -> Result<SourceFile, FeError> {
    parse_source_with_mode(src, ParseMode::Conformance)
}

/// Parse source with an explicit fallback policy.
pub fn parse_source_with_mode(src: &str, mode: ParseMode) -> Result<SourceFile, FeError> {
    parse_preprocessed_source(src, mode)
}

/// Preprocess (expand `` `include ``/`` `define ``/macros with the given
/// include dirs) then parse a file into a [`SourceFile`] AST. This is the entry
/// point for compiling real, macro-heavy sources such as UVM.
pub fn parse_file(path: &Path, include_dirs: Vec<PathBuf>) -> Result<SourceFile, FeError> {
    parse_file_with_mode(path, include_dirs, ParseMode::Permissive)
}

/// Preprocess and parse a file with an explicit fallback policy.
pub fn parse_file_with_mode(
    path: &Path,
    include_dirs: Vec<PathBuf>,
    mode: ParseMode,
) -> Result<SourceFile, FeError> {
    let mut pp = Preprocessor::new(include_dirs);
    let text = pp.process_file(path).map_err(FeError::Io)?;
    parse_preprocessed_source(&text, mode)
}

/// Preprocess a string (with include dirs) then parse it.
pub fn parse_source_with_includes(
    src: &str,
    source_path: &str,
    include_dirs: Vec<PathBuf>,
) -> Result<SourceFile, FeError> {
    parse_source_with_includes_and_mode(src, source_path, include_dirs, ParseMode::Permissive)
}

/// Preprocess and parse source with an explicit fallback policy.
pub fn parse_source_with_includes_and_mode(
    src: &str,
    source_path: &str,
    include_dirs: Vec<PathBuf>,
    mode: ParseMode,
) -> Result<SourceFile, FeError> {
    let mut pp = Preprocessor::new(include_dirs);
    let text = pp.process(src, source_path);
    parse_preprocessed_source(&text, mode)
}

fn parse_preprocessed_source(source: &str, mode: ParseMode) -> Result<SourceFile, FeError> {
    let (tree, tokens) = match mode {
        ParseMode::Permissive if contains_strength_keyword(source) => {
            cst::parse_source_with_tokens(source)?
        }
        ParseMode::Permissive => (cst::parse_source(source)?, Vec::new()),
        ParseMode::Conformance => cst::parse_source_strict(source)?,
    };
    let strengths = if mode == ParseMode::Conformance {
        conformance::validate(&tree, &tokens, source)?
    } else if tokens.is_empty() {
        Default::default()
    } else {
        conformance::strength_annotations(&tree, &tokens, source)
            .map(|(annotations, _)| annotations)
            .unwrap_or_default()
    };
    Ok(lower::lower_file_with_strengths(&tree, &strengths))
}

fn contains_strength_keyword(source: &str) -> bool {
    let bytes = source.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'/' if bytes.get(index + 1) == Some(&b'/') => {
                index += 2;
                while index < bytes.len() && bytes[index] != b'\n' {
                    index += 1;
                }
            }
            b'/' if bytes.get(index + 1) == Some(&b'*') => {
                index += 2;
                while index + 1 < bytes.len() && !(bytes[index] == b'*' && bytes[index + 1] == b'/')
                {
                    index += 1;
                }
                index = (index + 2).min(bytes.len());
            }
            b'"' => {
                index += 1;
                while index < bytes.len() {
                    if bytes[index] == b'\\' {
                        index = (index + 2).min(bytes.len());
                    } else if bytes[index] == b'"' {
                        index += 1;
                        break;
                    } else {
                        index += 1;
                    }
                }
            }
            b'\\' => {
                while index < bytes.len() && !bytes[index].is_ascii_whitespace() {
                    index += 1;
                }
            }
            byte if byte.is_ascii_alphabetic() || matches!(byte, b'_' | b'$') => {
                let start = index;
                index += 1;
                while index < bytes.len()
                    && (bytes[index].is_ascii_alphanumeric() || matches!(bytes[index], b'_' | b'$'))
                {
                    index += 1;
                }
                if matches!(
                    &source[start..index],
                    "supply0"
                        | "supply1"
                        | "strong0"
                        | "strong1"
                        | "pull0"
                        | "pull1"
                        | "weak0"
                        | "weak1"
                        | "highz0"
                        | "highz1"
                ) {
                    return true;
                }
            }
            _ => index += 1,
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::contains_strength_keyword;

    #[test]
    fn strength_keyword_scan_ignores_non_syntax_text() {
        assert!(contains_strength_keyword(
            "assign (strong1, pull0) result = source;"
        ));
        assert!(!contains_strength_keyword(
            "$display(\"strong1 is too large\"); // pull0\n/* weak1 */"
        ));
        assert!(!contains_strength_keyword("logic UVM_STRONG1; \\highz0 ;"));
    }
}
