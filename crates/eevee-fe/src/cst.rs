//! Driving Verible and navigating its JSON CST.
//!
//! Per the front-end decision (`docs/frontend-decision.md`), we shell out to the
//! prebuilt `verible-verilog-syntax --export_json --printtree` and parse the
//! resulting JSON concrete syntax tree with `serde_json`. This module owns the
//! subprocess and the small set of tree-navigation helpers that mirror the
//! Python reference's `_children` / `_non_null` / tag accessors.
//!
//! The CST node shape is:
//! * branch node: `{ "tag": "kFoo", "children": [ <node|null>, ... ] }`
//! * leaf token:  `{ "tag": "SymbolIdentifier" | "+" | ..., "text": "clk", .. }`
//!
//! `children` arrays contain `null` holes for absent optional slots, so all
//! traversal goes through [`kids`], which skips them.

use std::path::PathBuf;
use std::process::{Command, Stdio};

use serde_json::Value;

/// Errors from running or decoding the Verible front-end.
#[derive(Debug)]
pub enum FeError {
    /// `verible-verilog-syntax` could not be located.
    VeribleNotFound,
    /// The subprocess failed to start or I/O failed.
    Io(std::io::Error),
    /// Verible produced output that was not valid JSON.
    Json(serde_json::Error),
    /// Verible produced JSON without the expected `tree` node (syntax error).
    NoTree { stderr: String },
    /// Verible recovered a tree after a lexical or parse error.
    Syntax {
        phase: String,
        text: String,
        line: usize,
        column: usize,
    },
    /// Conformance mode encountered syntax outside the implemented subset.
    UnsupportedSyntax {
        construct: String,
        line: usize,
        column: usize,
    },
}

impl std::fmt::Display for FeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FeError::VeribleNotFound => write!(
                f,
                "verible-verilog-syntax not found (set VERIBLE_SYNTAX_BIN or place it under verible/)"
            ),
            FeError::Io(e) => write!(f, "verible I/O error: {e}"),
            FeError::Json(e) => write!(f, "verible JSON decode error: {e}"),
            FeError::NoTree { stderr } => {
                write!(f, "verible produced no syntax tree (parse error):\n{stderr}")
            }
            FeError::Syntax {
                phase,
                text,
                line,
                column,
            } => write!(
                f,
                "SystemVerilog {phase} error at {line}:{column} near '{text}'"
            ),
            FeError::UnsupportedSyntax {
                construct,
                line,
                column,
            } => write!(
                f,
                "unsupported SystemVerilog construct {construct} at {line}:{column}"
            ),
        }
    }
}

impl std::error::Error for FeError {}

impl From<std::io::Error> for FeError {
    fn from(e: std::io::Error) -> Self {
        FeError::Io(e)
    }
}
impl From<serde_json::Error> for FeError {
    fn from(e: serde_json::Error) -> Self {
        FeError::Json(e)
    }
}

/// Locate the `verible-verilog-syntax` binary: the `VERIBLE_SYNTAX_BIN`
/// environment variable, then the repo-vendored `verible/` tree, then `PATH`.
pub fn find_verible() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("VERIBLE_SYNTAX_BIN") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
    }
    // Walk up from this crate looking for a vendored `verible/` directory.
    let mut dir = std::env::current_dir().ok();
    while let Some(d) = dir {
        let cand = d.join("verible");
        if cand.is_dir() {
            if let Some(found) = find_in_dir(&cand) {
                return Some(found);
            }
        }
        // also try a sibling `../verible` of the workspace
        dir = d.parent().map(|p| p.to_path_buf());
    }
    None
}

fn find_in_dir(dir: &std::path::Path) -> Option<PathBuf> {
    let walker = std::fs::read_dir(dir).ok()?;
    for entry in walker.flatten() {
        let p = entry.path();
        if p.is_dir() {
            if let Some(f) = find_in_dir(&p) {
                return Some(f);
            }
        } else if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
            if name == "verible-verilog-syntax" || name == "verible-verilog-syntax.exe" {
                return Some(p);
            }
        }
    }
    None
}

/// Parse SystemVerilog `src` and return the root CST node (the `tree`).
pub fn parse_source(src: &str) -> Result<Value, FeError> {
    let exe = find_verible().ok_or(FeError::VeribleNotFound)?;
    parse_source_with(&exe, src)
}

/// Parse source and reject any Verible lexical/parser recovery diagnostics.
pub(crate) fn parse_source_strict(src: &str) -> Result<(Value, Vec<Value>), FeError> {
    let exe = find_verible().ok_or(FeError::VeribleNotFound)?;
    parse_source_with_policy(&exe, src, true)
}

/// Parse using an explicit Verible binary path.
pub fn parse_source_with(exe: &std::path::Path, src: &str) -> Result<Value, FeError> {
    parse_source_with_policy(exe, src, false).map(|(tree, _)| tree)
}

fn parse_source_with_policy(
    exe: &std::path::Path,
    src: &str,
    reject_recovery: bool,
) -> Result<(Value, Vec<Value>), FeError> {
    use std::io::Write;

    let mut command = Command::new(exe);
    command.args(["--export_json", "--printtree"]);
    if reject_recovery {
        command.arg("--printtokens");
    }
    let mut child = command
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    child
        .stdin
        .take()
        .expect("stdin piped")
        .write_all(src.as_bytes())?;
    let out = child.wait_with_output()?;
    let success = out.status.success();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();

    let json: Value = deserialize_deep(out.stdout)?;
    // Output shape: { "<filename or '-'>": { "tree": <node> } }
    let file = json.as_object().and_then(|object| object.values().next());
    if reject_recovery {
        if let Some(error) = file
            .and_then(|value| value.get("errors"))
            .and_then(first_error)
        {
            return Err(FeError::Syntax {
                phase: error
                    .get("phase")
                    .and_then(Value::as_str)
                    .unwrap_or("parse")
                    .to_string(),
                text: error
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                line: error.get("line").and_then(Value::as_u64).unwrap_or(0) as usize + 1,
                column: error.get("column").and_then(Value::as_u64).unwrap_or(0) as usize + 1,
            });
        }
        if !success {
            return Err(FeError::NoTree { stderr });
        }
    }
    let tree = file.and_then(|value| value.get("tree")).cloned();
    let tokens = file
        .and_then(|value| value.get("tokens"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    tree.map(|tree| (tree, tokens))
        .ok_or(FeError::NoTree { stderr })
}

fn first_error(errors: &Value) -> Option<&Value> {
    errors
        .as_array()
        .and_then(|values| values.first())
        .or_else(|| errors.as_object().map(|_| errors))
}

/// Deserialize a (possibly very deeply nested) Verible CST JSON.
///
/// `serde_json` recurses on the call stack and caps nesting at 128 by default;
/// the UVM CST goes far deeper. We disable that limit and parse on a thread
/// with a large stack so the deep recursion has room.
fn deserialize_deep(bytes: Vec<u8>) -> Result<Value, FeError> {
    use serde::Deserialize;
    std::thread::Builder::new()
        .stack_size(1024 * 1024 * 1024)
        .spawn(move || -> Result<Value, serde_json::Error> {
            let mut de = serde_json::Deserializer::from_slice(&bytes);
            de.disable_recursion_limit();
            let v = Value::deserialize(&mut de)?;
            de.end()?;
            Ok(v)
        })
        .expect("spawn JSON parse thread")
        .join()
        .expect("JSON parse thread panicked")
        .map_err(FeError::Json)
}

// ---------------------------------------------------------------------------
// CST navigation helpers (mirror the Python `verible_fe.py` accessors)
// ---------------------------------------------------------------------------

/// The node's `tag` (`""` if missing).
#[inline]
pub fn tag(n: &Value) -> &str {
    n.get("tag").and_then(Value::as_str).unwrap_or("")
}

/// The leaf token's `text` (`""` if missing/branch).
#[inline]
pub fn text(n: &Value) -> &str {
    n.get("text").and_then(Value::as_str).unwrap_or("")
}

/// The operator/keyword string for a leaf: its `text`, or its `tag` when the
/// text is empty (Verible leaves punctuation tags like `"+"` with empty text).
#[inline]
pub fn leaf_op(n: &Value) -> &str {
    let t = text(n);
    if t.is_empty() {
        tag(n)
    } else {
        t
    }
}

/// Iterate the non-null children of a branch node.
pub fn kids(n: &Value) -> impl Iterator<Item = &Value> {
    n.get("children")
        .and_then(Value::as_array)
        .map(|v| v.as_slice())
        .unwrap_or(&[])
        .iter()
        .filter(|c| !c.is_null())
}

/// First direct child whose tag is `t`.
pub fn find<'a>(n: &'a Value, t: &str) -> Option<&'a Value> {
    kids(n).find(|c| tag(c) == t)
}

/// First descendant (depth-first, self excluded) whose tag is `t`.
pub fn find_deep<'a>(n: &'a Value, t: &str) -> Option<&'a Value> {
    for c in kids(n) {
        if tag(c) == t {
            return Some(c);
        }
        if let Some(found) = find_deep(c, t) {
            return Some(found);
        }
    }
    None
}

/// Recursively find the first numeric leaf and parse it as an integer.
/// Handles `TK_DecNumber` (the only radix used by the current subset).
pub fn const_int(n: &Value) -> Option<i64> {
    if tag(n) == "TK_DecNumber" {
        return text(n).replace('_', "").parse().ok();
    }
    for c in kids(n) {
        if let Some(v) = const_int(c) {
            return Some(v);
        }
    }
    None
}
