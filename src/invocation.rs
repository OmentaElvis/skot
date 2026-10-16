//! The command lines advice and error messages show, resolved from the real command tree.
//!
//! A remedy that says `skot patch push` has to start saying that the day `push` moves
//! under `patch`, and every such string in the crate has to move with it. A constant per
//! command would not help: constants go stale in exactly the way they were meant to
//! prevent, one edit at a time. So [`skot`] looks the command up in the same tree clap
//! parses with, and moving a command updates every message at once.
//!
//! The tests here read this crate's sources. A hand-written `skot <command>` string
//! literal anywhere but this file fails, and any name handed to [`skot`] that the tree
//! does not have fails, so neither mistake can survive a change to the surface.

use std::collections::HashMap;
use std::sync::OnceLock;

use clap::CommandFactory;

use crate::cli::Cli;

/// The full command line for a subcommand: `skot patch push` for `push`, `skot build`
/// for `build`.
///
/// Panics on an unknown name, which is a bug in the caller; the source scan in the
/// tests below checks every call site against the tree, so it cannot reach a user.
pub fn skot(name: &str) -> String {
    let path = paths()
        .get(name)
        .unwrap_or_else(|| panic!("`{name}` is not a subcommand of skot"));
    format!("skot {}", path.join(" "))
}

/// The full command line with arguments: `skot patch edit-save 0001-a.patch`.
pub fn skot_args(name: &str, args: &str) -> String {
    format!("{} {}", skot(name), args)
}

/// Every subcommand name (and alias) to the path from the binary to it.
///
/// Walked from [`Cli::command`], the derive that parsing itself uses, once per process.
/// clap only requires a name to be unique among its siblings, so two commands under
/// different parents may share one (`series drop` next to `patch drop`). The bare-name
/// map cannot hold both, so the first registration in command-tree order wins and stays
/// deterministic; a hint that needs the other one passes the parent (`skot_args("series",
/// "drop <name>")`), which walks this same tree and cannot go stale.
fn paths() -> &'static HashMap<String, Vec<String>> {
    static PATHS: OnceLock<HashMap<String, Vec<String>>> = OnceLock::new();
    PATHS.get_or_init(|| {
        let mut map = HashMap::new();
        walk(&Cli::command(), &[], &mut map);
        map
    })
}

fn walk(cmd: &clap::Command, prefix: &[String], map: &mut HashMap<String, Vec<String>>) {
    // clap's own rule: a name is free as long as no *sibling* holds it. The assertion
    // here is what turns a future collision into an immediate failure rather than a
    // parse error a user hits.
    let mut siblings = std::collections::HashSet::new();
    for sub in cmd.get_subcommands() {
        if sub.is_hide_set() {
            continue;
        }
        for key in std::iter::once(sub.get_name()).chain(sub.get_all_aliases()) {
            assert!(
                siblings.insert(key.to_string()),
                "two commands under `{}` are both called `{key}`",
                prefix.join(" ")
            );
        }
    }
    for sub in cmd.get_subcommands() {
        if sub.is_hide_set() {
            continue;
        }
        let mut path = prefix.to_vec();
        path.push(sub.get_name().to_string());
        for key in std::iter::once(sub.get_name()).chain(sub.get_all_aliases()) {
            let key = key.to_string();
            map.entry(key).or_insert_with(|| path.clone());
        }
        walk(sub, &path, map);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_paths_resolve() {
        assert_eq!(skot("build"), "skot build");
        assert_eq!(skot("init"), "skot init");
        assert_eq!(skot("push"), "skot patch push");
        assert_eq!(skot("list"), "skot patch list");
        assert_eq!(skot("ls"), "skot patch list");
        assert_eq!(skot("new"), "skot patch new");
        assert_eq!(skot("drop"), "skot patch drop");
        assert_eq!(
            skot_args("edit-save", "0001-a.patch"),
            "skot patch edit-save 0001-a.patch"
        );
    }

    #[test]
    #[should_panic(expected = "is not a subcommand")]
    fn an_unknown_command_name_panics() {
        let name = String::from("frobnicate");
        // scan: ignore — this call is the one that is meant to fail
        skot(&name);
    }

    /// Every `skot("...")` and `skot_args("...", ...)` call in the crate names a
    /// command that exists, and passes a literal as the name: a literal is what lets
    /// this test read the name at all. This file is skipped because the definition and
    /// its pass-through live here; a call site anywhere else must be literal, or carry
    /// `scan: ignore` on its line to say why not.
    #[test]
    fn every_skot_call_names_a_real_command() {
        for path in source_files() {
            if path.ends_with("invocation.rs") {
                continue;
            }
            let text = std::fs::read_to_string(&path).expect("source is readable");
            let mut at = 0;
            while let Some(found) = find_call(&text, at) {
                at = found + 1;
                let Some(name) = string_argument(&text, found) else {
                    assert!(
                        ignore_marker(&text, found),
                        "{}: the command name passed to skot()/skot_args() must be a \
                         string literal",
                        path.display()
                    );
                    continue;
                };
                assert!(
                    paths().contains_key(&name),
                    "{}: skot(\"{name}\") does not name a command",
                    path.display()
                );
            }
        }
    }

    /// No `skot <command>` string literal outside this file. Prose that merely mentions
    /// the tool ("skot never deletes patches/") passes; a literal that spells out a
    /// command must go through [`skot`], so it moves when the command does.
    #[test]
    fn no_command_is_spelled_out_by_hand() {
        let here = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src")
            .join("invocation.rs");
        for path in source_files() {
            if path == here {
                continue;
            }
            let text = std::fs::read_to_string(&path).expect("source is readable");
            for (line, literal) in literals(&text) {
                if let Some(word) = command_after_skot(&literal) {
                    assert!(
                        !paths().contains_key(&word),
                        "{}:{line}: the string {literal:?} spells out `{word}` by hand; \
                         build it with skot()/skot_args() so it follows the command",
                        path.display()
                    );
                }
            }
        }
    }

    /// Index of the next call to `skot` or `skot_args`: a `skot` that is not part of a
    /// longer name or identifier and is followed by `(`.
    fn find_call(text: &str, from: usize) -> Option<usize> {
        let mut at = from;
        loop {
            let found = text[at..].find("skot")? + at;
            let name_len = if text[found..].starts_with("skot_args") {
                "skot_args".len()
            } else {
                "skot".len()
            };
            let previous = text[..found].chars().next_back();
            let in_word = previous.is_some_and(|c| c.is_alphanumeric() || c == '_');
            if !in_word && text[found + name_len..].starts_with('(') {
                return Some(found);
            }
            at = found + name_len;
        }
    }

    /// Whether the call's line says `scan: ignore`: the one way out of the literal
    /// rule, for the call that is testing the failure mode itself.
    fn ignore_marker(text: &str, at: usize) -> bool {
        let line_start = text[..at].rfind('\n').map_or(0, |i| i + 1);
        let line_end = text[at..].find('\n').map_or(text.len(), |i| at + i);
        text[line_start..line_end].contains("scan: ignore")
    }

    /// The string literal starting after the call's `(`, parsed with escapes.
    fn string_argument(text: &str, call_at: usize) -> Option<String> {
        let open = text[call_at..].find('(')? + call_at;
        let mut at = open + 1;
        while text[at..].starts_with(char::is_whitespace) {
            at += 1;
        }
        let bytes = text.as_bytes();
        if bytes.get(at) != Some(&b'"') {
            return None;
        }
        at += 1;
        let mut out = String::new();
        while let Some(&byte) = bytes.get(at) {
            match byte {
                b'"' => return Some(out),
                b'\\' => {
                    at += 1;
                    let escaped = bytes.get(at)?;
                    out.push(match escaped {
                        b'n' => '\n',
                        b't' => '\t',
                        b'r' => '\r',
                        b'0' => '\0',
                        other => *other as char,
                    });
                    at += 1;
                }
                _ => {
                    let ch = text[at..].chars().next()?;
                    out.push(ch);
                    at += ch.len_utf8();
                }
            }
        }
        None
    }

    /// The command word in a literal like ``skot patch push``, if the word after
    /// `skot ` is one of ours.
    fn command_after_skot(literal: &str) -> Option<String> {
        let at = literal.find("skot ")? + "skot ".len();
        let word: String = literal[at..]
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
            .collect();
        paths().contains_key(&word).then_some(word)
    }

    /// Every string literal in the sources, with the line it starts on. Comments and
    /// char literals are skipped so a quote inside them cannot open a string.
    fn literals(src: &str) -> Vec<(usize, String)> {
        let bytes = src.as_bytes();
        let mut out = Vec::new();
        let mut i = 0;
        while i < bytes.len() {
            match bytes[i] {
                b'/' if bytes.get(i + 1) == Some(&b'/') => {
                    while i < bytes.len() && bytes[i] != b'\n' {
                        i += 1;
                    }
                }
                b'/' if bytes.get(i + 1) == Some(&b'*') => {
                    let mut depth = 1;
                    i += 2;
                    while i < bytes.len() && depth > 0 {
                        match (bytes[i], bytes.get(i + 1)) {
                            (b'/', Some(b'*')) => {
                                depth += 1;
                                i += 2;
                            }
                            (b'*', Some(b'/')) => {
                                depth -= 1;
                                i += 2;
                            }
                            _ => i += 1,
                        }
                    }
                }
                b'r' | b'b' if raw_string_start(bytes, i).is_some() => {
                    let (hashes, start) = raw_string_start(bytes, i).unwrap();
                    i = start;
                    while i < bytes.len() {
                        if bytes[i] == b'"'
                            && bytes[i + 1..]
                                .iter()
                                .take(hashes)
                                .filter(|&&c| c == b'#')
                                .count()
                                == hashes
                        {
                            let end = i;
                            i += 1 + hashes;
                            out.push((line_of(src, start), src[start..end].to_string()));
                            break;
                        }
                        i += 1;
                    }
                }
                b'"' => {
                    let start = i + 1;
                    i = start;
                    while i < bytes.len() {
                        match bytes[i] {
                            b'"' => {
                                out.push((line_of(src, start), src[start..i].to_string()));
                                i += 1;
                                break;
                            }
                            b'\\' => i += 2,
                            _ => i += 1,
                        }
                    }
                }
                b'\'' => {
                    // A char literal can hide a quote ('"'), a lifetime cannot; only
                    // take the closing quote when this really is a char literal.
                    let j = i + 1;
                    let is_char = if bytes.get(j) == Some(&b'\\') {
                        bytes.get(j + 2) == Some(&b'\'')
                    } else {
                        bytes.get(j + 1) == Some(&b'\'')
                    };
                    if is_char {
                        i = j + 2;
                    } else {
                        i = j;
                    }
                }
                _ => i += 1,
            }
        }
        out
    }

    /// For `r"`, `r#"`, `br"`, `br#"`: the hash count and where the content starts.
    fn raw_string_start(bytes: &[u8], at: usize) -> Option<(usize, usize)> {
        let mut i = at;
        if bytes.get(i) == Some(&b'b') {
            i += 1;
        }
        if bytes.get(i) != Some(&b'r') {
            return None;
        }
        i += 1;
        let hashes_start = i;
        while bytes.get(i) == Some(&b'#') {
            i += 1;
        }
        let hashes = i - hashes_start;
        (bytes.get(i) == Some(&b'"')).then_some((hashes, i + 1))
    }

    fn line_of(src: &str, offset: usize) -> usize {
        src[..offset].bytes().filter(|&b| b == b'\n').count() + 1
    }

    /// Every `.rs` file under `src/`, in a stable order.
    fn source_files() -> Vec<std::path::PathBuf> {
        fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            for entry in std::fs::read_dir(dir).expect("src is readable") {
                let path = entry.expect("src entry").path();
                if path.is_dir() {
                    walk(&path, out);
                } else if path.extension().is_some_and(|ext| ext == "rs") {
                    out.push(path);
                }
            }
        }
        let mut out = Vec::new();
        walk(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
            &mut out,
        );
        out.sort();
        out
    }
}
