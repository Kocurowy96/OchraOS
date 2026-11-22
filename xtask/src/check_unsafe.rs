// SPDX-License-Identifier: MIT OR Apache-2.0

//! `cargo xtask check-unsafe`: no `unsafe` outside `ochra-frame`.
//!
//! The kernel crate is `#![forbid(unsafe_code)]`, but that lint does not see code produced by
//! macros from other crates, and a new crate could simply forget the attribute. This check is
//! the backstop: it reads the source of every workspace member except the ones listed in
//! [`ALLOWED`] and fails if the keyword `unsafe` appears in code. Comments, strings and
//! character literals are skipped, so "unsafe" in a doc comment or a log message is fine.
//!
//! New crates (e.g. drivers) are workspace members, so they are checked automatically.

use std::path::{Path, PathBuf};

use crate::{Result, root, step};

/// Workspace members that may contain `unsafe`, by directory.
const ALLOWED: &[&str] = &["frame"];

pub fn run() -> Result<()> {
    let members = workspace_members()?;
    let mut checked = Vec::new();
    let mut findings = Vec::new();
    for member in members.iter().filter(|m| !ALLOWED.contains(&m.as_str())) {
        step(&format!("checking {member}/ for unsafe"));
        let mut files = Vec::new();
        rust_files(&root().join(member), &mut files)?;
        for file in files {
            let source = std::fs::read_to_string(&file)?;
            for line in unsafe_lines(&source) {
                let shown = file.strip_prefix(root()).unwrap_or(&file).display();
                findings.push(format!("{shown}:{line}"));
            }
        }
        checked.push(member.as_str());
    }

    if findings.is_empty() {
        step(&format!("no unsafe in {}", checked.join(", ")));
        Ok(())
    } else {
        for finding in &findings {
            eprintln!("    unsafe at {finding}");
        }
        Err("`unsafe` is only allowed in frame/ (CLAUDE.md, rule 3)".into())
    }
}

/// The `members = [...]` list of the root `Cargo.toml`.
fn workspace_members() -> Result<Vec<String>> {
    let manifest = std::fs::read_to_string(root().join("Cargo.toml"))?;
    let start = manifest
        .find("members")
        .and_then(|at| manifest[at..].find('[').map(|bracket| at + bracket + 1))
        .ok_or("no `members = [...]` in Cargo.toml")?;
    let end = manifest[start..]
        .find(']')
        .ok_or("unterminated `members` list in Cargo.toml")?;
    Ok(manifest[start..start + end]
        .split(',')
        .map(|item| item.trim().trim_matches('"').to_string())
        .filter(|item| !item.is_empty())
        .collect())
}

/// All `.rs` files under `dir`, skipping build output.
fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|name| name != "target") {
                rust_files(&path, out)?;
            }
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
    out.sort();
    Ok(())
}

/// Line numbers (1-based) where the identifier `unsafe` appears in code.
fn unsafe_lines(source: &str) -> Vec<usize> {
    let chars: Vec<char> = source.chars().collect();
    let mut lines = Vec::new();
    let mut line = 1;
    let mut i = 0;
    // Counts the newlines in chars[from..to], so skipped comments and strings keep the line
    // numbers right.
    let newlines = |from: usize, to: usize| chars[from..to].iter().filter(|&&c| c == '\n').count();

    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        let start = i;
        if c == '/' && next == Some('/') {
            // Line comment (also `///` and `//!` doc comments).
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
        } else if c == '/' && next == Some('*') {
            // Block comment; they nest in Rust.
            let mut depth = 0;
            while i < chars.len() {
                if chars[i] == '/' && chars.get(i + 1) == Some(&'*') {
                    depth += 1;
                    i += 2;
                } else if chars[i] == '*' && chars.get(i + 1) == Some(&'/') {
                    depth -= 1;
                    i += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    i += 1;
                }
            }
        } else if let Some(end) = raw_string_end(&chars, i) {
            i = end;
        } else if c == '"' {
            i = quoted_end(&chars, i, '"');
        } else if c == '\'' {
            // A character literal ('x', '\n', '\'') or a lifetime ('a, 'static).
            if next == Some('\\') || chars.get(i + 2) == Some(&'\'') {
                i = quoted_end(&chars, i, '\'');
            } else {
                i += 1;
            }
        } else if c.is_alphabetic() || c == '_' {
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            if word == "unsafe" {
                lines.push(line);
            }
            // `b"…"`, `r"…"`, `br#"…"#`: the prefix was read as a word, the string comes next.
            continue;
        } else {
            i += 1;
        }
        line += newlines(start, i);
    }
    lines.dedup();
    lines
}

/// If a raw string (`r"…"`, `r#"…"#`, `br#"…"#`) starts at `i`, returns the index past its end.
fn raw_string_end(chars: &[char], i: usize) -> Option<usize> {
    let mut j = i;
    if chars.get(j) == Some(&'b') {
        j += 1;
    }
    if chars.get(j) != Some(&'r') {
        return None;
    }
    // Must not be the end of a longer identifier, e.g. `var"`.
    if i > 0 && (chars[i - 1].is_alphanumeric() || chars[i - 1] == '_') {
        return None;
    }
    j += 1;
    let hashes = chars[j..].iter().take_while(|&&c| c == '#').count();
    j += hashes;
    if chars.get(j) != Some(&'"') {
        return None;
    }
    j += 1;
    while j < chars.len() {
        if chars[j] == '"'
            && chars[j + 1..]
                .iter()
                .take(hashes)
                .filter(|&&c| c == '#')
                .count()
                == hashes
        {
            return Some(j + 1 + hashes);
        }
        j += 1;
    }
    Some(chars.len())
}

/// Index just past a `"…"` string or `'…'` character literal starting at `i`.
fn quoted_end(chars: &[char], i: usize, quote: char) -> usize {
    let mut j = i + 1;
    while j < chars.len() {
        match chars[j] {
            '\\' => j += 2,
            c if c == quote => return j + 1,
            _ => j += 1,
        }
    }
    chars.len()
}

#[cfg(test)]
mod tests {
    use super::unsafe_lines;

    #[test]
    fn finds_code_only() {
        let source = r##"
// unsafe in a comment
/* unsafe /* nested unsafe */ still comment */
/// unsafe in a doc comment
fn a() { let s = "unsafe"; let r = r#"unsafe "quoted""#; let c = '"'; }
fn b<'a>(x: &'a u8) -> u8 { unsafe { *x } }
fn not_unsafe_ident() { let unsafe_ok = 1; }
#[unsafe(no_mangle)]
"##;
        assert_eq!(unsafe_lines(source), vec![6, 8]);
    }
}
