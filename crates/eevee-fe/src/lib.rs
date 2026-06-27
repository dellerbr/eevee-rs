//! Front-end: Verible parsing + CST→AST lowering.
//!
//! [`parse_source`] runs the vendored `verible-verilog-syntax` on a string of
//! SystemVerilog and lowers the resulting JSON CST to the [`eevee_ast`] AST.
//! Verible runs as a subprocess (no FFI) per `docs/frontend-decision.md`; the
//! CST plumbing is in [`cst`] and the lowering map in [`lower`].

#![forbid(unsafe_code)]

pub mod cst;
pub mod lower;
pub mod preprocess;

pub use cst::{find_verible, FeError};
pub use lower::lower_file;
pub use preprocess::Preprocessor;

use eevee_ast::SourceFile;
use std::path::{Path, PathBuf};

/// Parse a string of SystemVerilog into a [`SourceFile`] AST.
pub fn parse_source(src: &str) -> Result<SourceFile, FeError> {
    let tree = cst::parse_source(src)?;
    Ok(lower::lower_file(&tree))
}

/// Preprocess (expand `` `include ``/`` `define ``/macros with the given
/// include dirs) then parse a file into a [`SourceFile`] AST. This is the entry
/// point for compiling real, macro-heavy sources such as UVM.
pub fn parse_file(path: &Path, include_dirs: Vec<PathBuf>) -> Result<SourceFile, FeError> {
    let mut pp = Preprocessor::new(include_dirs);
    let text = pp.process_file(path).map_err(FeError::Io)?;
    let tree = cst::parse_source(&text)?;
    Ok(lower::lower_file(&tree))
}

/// Preprocess a string (with include dirs) then parse it.
pub fn parse_source_with_includes(
    src: &str,
    source_path: &str,
    include_dirs: Vec<PathBuf>,
) -> Result<SourceFile, FeError> {
    let mut pp = Preprocessor::new(include_dirs);
    let text = pp.process(src, source_path);
    let tree = cst::parse_source(&text)?;
    Ok(lower::lower_file(&tree))
}
