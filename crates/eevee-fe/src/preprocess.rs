//! A SystemVerilog preprocessor (IEEE 1800-2017 §22), ported from the Python
//! reference's `lang/preprocessor.py`. It runs **before** Verible (text→text)
//! and is the gate to compiling real UVM: it expands `` `include `` (recursive,
//! with include dirs) and `` `define ``/macro references (the `` `uvm_* ``
//! macros), and handles `` `ifdef ``/`` `ifndef ``/`` `elsif ``/`` `else ``/
//! `` `endif `` conditional compilation.
//!
//! It also recognizes-and-strips directives that don't affect our token stream
//! (`` `timescale ``, `` `default_nettype ``, …) and supports `` `" ``
//! (stringify), ` `` ` (token paste), `__FILE__`, and `__LINE__`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

const MAX_EXPAND_DEPTH: usize = 64;
const MAX_INCLUDE_DEPTH: usize = 64;

/// A macro definition: object-like (`params == None`) or function-like.
struct Macro {
    params: Option<Vec<(String, Option<String>)>>,
    body: String,
}

/// Directives that consume their line and emit nothing.
fn is_passthrough(d: &str) -> bool {
    matches!(
        d,
        "timescale"
            | "default_nettype"
            | "celldefine"
            | "endcelldefine"
            | "resetall"
            | "unconnected_drive"
            | "nounconnected_drive"
            | "line"
            | "begin_keywords"
            | "end_keywords"
            | "protect"
            | "endprotect"
            | "protected"
            | "endprotected"
            | "pragma"
    )
}

/// One `` `ifdef ``/`` `ifndef `` conditional level.
struct Cond {
    /// Whether the enclosing context was emitting when this level opened.
    parent_active: bool,
    /// Whether any branch in this if-chain has been taken yet.
    taken: bool,
    /// Whether the current branch is emitting.
    active: bool,
}

/// The preprocessor. One instance per top-level compilation; `` `define ``s and
/// the include stack accumulate across `` `include ``s.
pub struct Preprocessor {
    include_dirs: Vec<PathBuf>,
    macros: HashMap<String, Macro>,
    include_stack: Vec<PathBuf>,
    current_file: String,
    current_line: usize,
}

impl Preprocessor {
    /// Create a preprocessor with the given `+incdir+` search paths.
    pub fn new(include_dirs: Vec<PathBuf>) -> Preprocessor {
        Preprocessor {
            include_dirs,
            macros: HashMap::new(),
            include_stack: Vec::new(),
            current_file: String::new(),
            current_line: 0,
        }
    }

    /// Predefine an object-like macro (like `+define+NAME=VALUE`).
    pub fn define(&mut self, name: &str, body: &str) {
        self.macros.insert(
            name.to_string(),
            Macro {
                params: None,
                body: body.to_string(),
            },
        );
    }

    /// Preprocess a file, expanding its includes relative to its directory.
    pub fn process_file(&mut self, path: &Path) -> std::io::Result<String> {
        let bytes = std::fs::read(path)?;
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let src = path.display().to_string();
        self.include_stack.push(path.to_path_buf());
        let out = self.process(&text, &src);
        self.include_stack.pop();
        Ok(out)
    }

    /// Preprocess a string of source. `source_path` is used for relative
    /// `` `include `` resolution and `__FILE__`.
    pub fn process(&mut self, text: &str, source_path: &str) -> String {
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        let text = join_continuations(&text);
        let text = strip_comments(&text);

        let mut out_lines: Vec<String> = Vec::new();
        let mut cond: Vec<Cond> = Vec::new();
        self.current_file = source_path.to_string();

        for (idx, line) in text.split('\n').enumerate() {
            self.current_line = idx + 1;
            let stripped = line.trim_start();
            if let Some(after) = stripped.strip_prefix('`') {
                let (directive, rest) = split_directive(after);
                // Conditionals are handled regardless of the emitting state.
                match directive.as_str() {
                    "ifdef" | "ifndef" => {
                        let name = first_token(&rest);
                        let parent = emitting(&cond);
                        let defined = self.macros.contains_key(&name);
                        let want = if directive == "ifdef" {
                            defined
                        } else {
                            !defined
                        };
                        cond.push(Cond {
                            parent_active: parent,
                            taken: want,
                            active: parent && want,
                        });
                        out_lines.push(String::new());
                        continue;
                    }
                    "elsif" => {
                        if let Some(top) = cond.last_mut() {
                            let name = first_token(&rest);
                            if !top.taken && top.parent_active && self.macros.contains_key(&name) {
                                top.active = true;
                                top.taken = true;
                            } else {
                                top.active = false;
                            }
                        }
                        out_lines.push(String::new());
                        continue;
                    }
                    "else" => {
                        if let Some(top) = cond.last_mut() {
                            if !top.taken && top.parent_active {
                                top.active = true;
                                top.taken = true;
                            } else {
                                top.active = false;
                            }
                        }
                        out_lines.push(String::new());
                        continue;
                    }
                    "endif" => {
                        cond.pop();
                        out_lines.push(String::new());
                        continue;
                    }
                    _ => {}
                }

                if !emitting(&cond) {
                    out_lines.push(String::new());
                    continue;
                }

                match directive.as_str() {
                    "define" => {
                        self.handle_define(&rest);
                        out_lines.push(String::new());
                    }
                    "undef" => {
                        self.macros.remove(&first_token(&rest));
                        out_lines.push(String::new());
                    }
                    "include" => match parse_include_filename(&rest) {
                        Some(fname) => {
                            let inc = self.do_include(&fname, source_path);
                            out_lines.push(inc);
                            // do_include changes current_file; restore it.
                            self.current_file = source_path.to_string();
                        }
                        None => out_lines.push(String::new()),
                    },
                    d if is_passthrough(d) => out_lines.push(String::new()),
                    _ => {
                        // Unknown directive — treat the line as a macro use.
                        out_lines.push(self.expand_text(line, 0));
                    }
                }
                continue;
            }

            if emitting(&cond) {
                out_lines.push(self.expand_text(line, 0));
            } else {
                out_lines.push(String::new());
            }
        }

        out_lines.join("\n")
    }

    // ---- `define -------------------------------------------------------

    fn handle_define(&mut self, rest: &str) {
        let rest = rest.trim_start();
        let (name, after) = take_ident(rest);
        if name.is_empty() {
            return;
        }
        let mut after = after;
        let mut params = None;
        // A `(` immediately after the name (no space) starts a parameter list.
        if after.starts_with('(') {
            let chars: Vec<char> = after.chars().collect();
            if let Some(end) = find_matching_paren(&chars, 0) {
                let param_src: String = chars[1..end].iter().collect();
                params = Some(parse_params(&param_src));
                after = &after[char_byte(after, end + 1)..];
            }
        }
        let body = after.trim().to_string();
        self.macros.insert(name, Macro { params, body });
    }

    // ---- `include ------------------------------------------------------

    fn do_include(&mut self, fname: &str, source_path: &str) -> String {
        if self.include_stack.len() >= MAX_INCLUDE_DEPTH {
            return format!("// `include depth limit exceeded for {fname}");
        }
        let resolved = match self.resolve_include(fname, source_path) {
            Some(p) => p,
            None => return format!("// `include not found: {fname}"),
        };
        if self.include_stack.iter().any(|p| p == &resolved) {
            return format!("// recursive `include skipped: {fname}");
        }
        let bytes = match std::fs::read(&resolved) {
            Ok(b) => b,
            Err(_) => return format!("// `include read error: {fname}"),
        };
        let inner = String::from_utf8_lossy(&bytes).into_owned();
        let src = resolved.display().to_string();
        self.include_stack.push(resolved);
        let out = self.process(&inner, &src);
        self.include_stack.pop();
        out
    }

    fn resolve_include(&self, fname: &str, source_path: &str) -> Option<PathBuf> {
        // 1) relative to the current file's directory
        if let Some(dir) = Path::new(source_path).parent() {
            let cand = dir.join(fname);
            if cand.is_file() {
                return Some(cand);
            }
        }
        // 2) the include dirs
        for d in &self.include_dirs {
            let cand = d.join(fname);
            if cand.is_file() {
                return Some(cand);
            }
        }
        None
    }

    // ---- macro expansion ----------------------------------------------

    fn expand_text(&self, text: &str, depth: usize) -> String {
        if depth >= MAX_EXPAND_DEPTH {
            return text.to_string();
        }
        let chars: Vec<char> = text.chars().collect();
        let n = chars.len();
        let mut out = String::with_capacity(text.len());
        let mut i = 0;
        let mut in_str = false;
        while i < n {
            let c = chars[i];
            if c == '"' {
                in_str = !in_str;
                out.push(c);
                i += 1;
                continue;
            }
            if in_str {
                out.push(c);
                i += 1;
                continue;
            }
            if c == '`' {
                // `" -> "   ;   `` -> (deleted token paste)
                if i + 1 < n && chars[i + 1] == '"' {
                    out.push('"');
                    i += 2;
                    continue;
                }
                if i + 1 < n && chars[i + 1] == '`' {
                    i += 2;
                    continue;
                }
                if i + 1 < n && is_ident_start(chars[i + 1]) {
                    let start = i + 1;
                    let mut j = start + 1;
                    while j < n && is_ident_cont(chars[j]) {
                        j += 1;
                    }
                    let name: String = chars[start..j].iter().collect();
                    if name == "__FILE__" {
                        // Emit as a string literal; escape backslashes (Windows
                        // paths) and quotes so they survive string unescaping.
                        out.push('"');
                        out.push_str(&self.current_file.replace('\\', "\\\\").replace('"', "\\\""));
                        out.push('"');
                        i = j;
                        continue;
                    }
                    if name == "__LINE__" {
                        out.push_str(&self.current_line.to_string());
                        i = j;
                        continue;
                    }
                    match self.macros.get(&name) {
                        None => {
                            let raw: String = chars[i..j].iter().collect();
                            out.push_str(&raw);
                            i = j;
                        }
                        Some(mac) if mac.params.is_some() => {
                            let mut k = j;
                            while k < n && (chars[k] == ' ' || chars[k] == '\t') {
                                k += 1;
                            }
                            if k < n && chars[k] == '(' {
                                if let Some(close) = find_matching_paren(&chars, k) {
                                    let args_src: String = chars[k + 1..close].iter().collect();
                                    let args = split_top_level(&args_src, ',');
                                    let repl = self.apply_macro(mac, &args);
                                    out.push_str(&self.expand_text(&repl, depth + 1));
                                    i = close + 1;
                                    continue;
                                }
                            }
                            let raw: String = chars[i..j].iter().collect();
                            out.push_str(&raw);
                            i = j;
                        }
                        Some(mac) => {
                            let body = mac.body.clone();
                            out.push_str(&self.expand_text(&body, depth + 1));
                            i = j;
                        }
                    }
                    continue;
                }
                out.push(c);
                i += 1;
                continue;
            }
            out.push(c);
            i += 1;
        }
        out
    }

    fn apply_macro(&self, mac: &Macro, args: &[String]) -> String {
        let params = mac.params.as_deref().unwrap_or(&[]);
        let mut bound: HashMap<&str, String> = HashMap::new();
        for (k, (pname, default)) in params.iter().enumerate() {
            let v = if k < args.len() {
                let a = args[k].trim();
                if a.is_empty() {
                    default.clone().unwrap_or_default()
                } else {
                    a.to_string()
                }
            } else {
                default.clone().unwrap_or_default()
            };
            bound.insert(pname.as_str(), v);
        }
        let mut body = substitute_params(&mac.body, &bound);
        if body.contains("``") {
            body = body.replace("``", "");
        }
        body
    }
}

/// `emitting` if every open conditional level is active (active already folds
/// in the parent state, so checking the top suffices).
fn emitting(cond: &[Cond]) -> bool {
    cond.last().map(|c| c.active).unwrap_or(true)
}

fn is_ident_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_'
}
fn is_ident_cont(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// Take a leading identifier from `s`, returning `(ident, rest)`.
fn take_ident(s: &str) -> (String, &str) {
    let mut end = 0;
    for (k, c) in s.char_indices() {
        if k == 0 {
            if !is_ident_start(c) {
                return (String::new(), s);
            }
        } else if !is_ident_cont(c) {
            break;
        }
        end = k + c.len_utf8();
    }
    (s[..end].to_string(), &s[end..])
}

/// Split `` `directive rest `` into `(directive, rest)`.
fn split_directive(after_tick: &str) -> (String, String) {
    let (ident, rest) = take_ident(after_tick);
    (ident, rest.to_string())
}

fn first_token(s: &str) -> String {
    s.split_whitespace().next().unwrap_or("").to_string()
}

/// Join `` \ ``-newline line continuations (used in `` `define `` bodies).
fn join_continuations(text: &str) -> String {
    text.replace("\\\n", " ")
}

/// Replace comments with spaces (preserving newlines) while respecting strings.
fn strip_comments(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let n = chars.len();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    let mut in_str = false;
    while i < n {
        let c = chars[i];
        if in_str {
            out.push(c);
            if c == '\\' && i + 1 < n {
                out.push(chars[i + 1]);
                i += 2;
                continue;
            }
            if c == '"' {
                in_str = false;
            }
            i += 1;
            continue;
        }
        if c == '"' {
            in_str = true;
            out.push(c);
            i += 1;
            continue;
        }
        if c == '/' && i + 1 < n && chars[i + 1] == '/' {
            while i < n && chars[i] != '\n' {
                out.push(' ');
                i += 1;
            }
            continue;
        }
        if c == '/' && i + 1 < n && chars[i + 1] == '*' {
            out.push(' ');
            out.push(' ');
            i += 2;
            while i < n {
                if chars[i] == '*' && i + 1 < n && chars[i + 1] == '/' {
                    out.push(' ');
                    out.push(' ');
                    i += 2;
                    break;
                }
                out.push(if chars[i] == '\n' { '\n' } else { ' ' });
                i += 1;
            }
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

/// Index of the matching `)` for the `(` at `chars[open]`, or `None`.
fn find_matching_paren(chars: &[char], open: usize) -> Option<usize> {
    let mut depth = 0;
    let mut in_str = false;
    let mut i = open;
    while i < chars.len() {
        let c = chars[i];
        if in_str {
            if c == '\\' && i + 1 < chars.len() {
                i += 2;
                continue;
            }
            if c == '"' {
                in_str = false;
            }
            i += 1;
            continue;
        }
        match c {
            '"' => in_str = true,
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// Byte offset of the `n`-th char (for slicing back into the `&str`).
fn char_byte(s: &str, n: usize) -> usize {
    s.char_indices().nth(n).map(|(b, _)| b).unwrap_or(s.len())
}

fn parse_params(src: &str) -> Vec<(String, Option<String>)> {
    let mut out = Vec::new();
    for chunk in split_top_level(src, ',') {
        let chunk = chunk.trim();
        if chunk.is_empty() {
            continue;
        }
        if let Some((name, default)) = chunk.split_once('=') {
            out.push((name.trim().to_string(), Some(default.trim().to_string())));
        } else {
            out.push((chunk.to_string(), None));
        }
    }
    out
}

fn parse_include_filename(rest: &str) -> Option<String> {
    let rest = rest.trim();
    if let Some(r) = rest.strip_prefix('"') {
        return r.find('"').map(|e| r[..e].to_string());
    }
    if let Some(r) = rest.strip_prefix('<') {
        return r.find('>').map(|e| r[..e].to_string());
    }
    rest.split_whitespace().next().map(|s| s.to_string())
}

/// Whole-identifier substitution of macro parameters in a body.
fn substitute_params(body: &str, bound: &HashMap<&str, String>) -> String {
    if bound.is_empty() {
        return body.to_string();
    }
    let chars: Vec<char> = body.chars().collect();
    let n = chars.len();
    let mut out = String::with_capacity(body.len());
    let mut i = 0;
    while i < n {
        let c = chars[i];
        if is_ident_start(c) {
            let start = i;
            i += 1;
            while i < n && is_ident_cont(chars[i]) {
                i += 1;
            }
            let ident: String = chars[start..i].iter().collect();
            match bound.get(ident.as_str()) {
                Some(v) => out.push_str(v),
                None => out.push_str(&ident),
            }
        } else {
            out.push(c);
            i += 1;
        }
    }
    out
}

/// Split `s` by `sep` at the top level (ignoring nesting and strings).
fn split_top_level(s: &str, sep: char) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut in_str = false;
    let mut buf = String::new();
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if in_str {
            if c == '\\' && i + 1 < chars.len() {
                buf.push(c);
                buf.push(chars[i + 1]);
                i += 2;
                continue;
            }
            if c == '"' {
                in_str = false;
            }
            buf.push(c);
            i += 1;
            continue;
        }
        match c {
            '"' => {
                in_str = true;
                buf.push(c);
            }
            '(' | '[' | '{' => {
                depth += 1;
                buf.push(c);
            }
            ')' | ']' | '}' => {
                depth -= 1;
                buf.push(c);
            }
            _ if c == sep && depth == 0 => {
                out.push(std::mem::take(&mut buf));
            }
            _ => buf.push(c),
        }
        i += 1;
    }
    out.push(buf);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pp(src: &str) -> String {
        Preprocessor::new(vec![]).process(src, "<test>")
    }

    #[test]
    fn object_like_macro() {
        let out = pp("`define W 8\nlogic [`W-1:0] x;");
        assert!(out.contains("logic [8-1:0] x;"), "{out}");
    }

    #[test]
    fn function_like_macro() {
        let out = pp("`define ADD(a, b) ((a) + (b))\nint y = `ADD(3, 4);");
        assert!(out.contains("((3) + (4))"), "{out}");
    }

    #[test]
    fn ifdef_else() {
        let out = pp("`ifdef FOO\nint a;\n`else\nint b;\n`endif");
        assert!(out.contains("int b;") && !out.contains("int a;"), "{out}");
        let out2 = pp("`define FOO\n`ifdef FOO\nint a;\n`else\nint b;\n`endif");
        assert!(
            out2.contains("int a;") && !out2.contains("int b;"),
            "{out2}"
        );
    }

    #[test]
    fn nested_macro_expansion() {
        let out = pp("`define A 1\n`define B (`A + `A)\nint x = `B;");
        assert!(out.contains("(1 + 1)"), "{out}");
    }

    #[test]
    fn elsif_chain_first_branch_wins() {
        let out = pp("`define A\n`ifdef A\nx\n`elsif A\ny\n`else\nz\n`endif");
        assert!(
            out.contains('x') && !out.contains('y') && !out.contains('z'),
            "{out}"
        );
    }

    #[test]
    fn line_continuation_in_define() {
        let out = pp("`define M(a) do_a(a); \\\n do_b(a)\n`M(5)");
        assert!(out.contains("do_a(5);") && out.contains("do_b(5)"), "{out}");
    }

    #[test]
    fn comments_stripped_respecting_strings() {
        let out = pp("string s = \"a // not a comment\"; // real comment");
        assert!(out.contains("\"a // not a comment\""), "{out}");
        assert!(!out.contains("real comment"), "{out}");
    }
}
