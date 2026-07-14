//! Thin tree-sitter layer: file → symbol outline, references, definition lookup.
//! Every entry point returns `Option`; `None` means "no grammar / parse failed" and
//! the caller falls back to the brace/heuristic path. Deterministic, no IO.

use streaming_iterator::StreamingIterator;
use tree_sitter::{Language, Node, Parser, Query, QueryCursor};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Lang {
    Rust,
    Ruby,
    JavaScript,
    TypeScript,
    Tsx,
    Python,
    Go,
}

#[derive(Debug, Clone)]
pub struct Decl {
    pub kind: String,
    pub name: String,
    /// 1-based line range.
    pub start: usize,
    pub end: usize,
    pub signature: String,
}

#[derive(Debug, Clone)]
pub struct Ref {
    pub name: String,
    pub line: usize,
}

const SIG_MAX: usize = 200;

impl Lang {
    fn language(self) -> Language {
        match self {
            Lang::Rust => tree_sitter_rust::LANGUAGE.into(),
            Lang::Ruby => tree_sitter_ruby::LANGUAGE.into(),
            Lang::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
            Lang::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Lang::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
            Lang::Python => tree_sitter_python::LANGUAGE.into(),
            Lang::Go => tree_sitter_go::LANGUAGE.into(),
        }
    }

    fn decls_query(self) -> &'static str {
        match self {
            Lang::Rust => include_str!("queries/rust.decls.scm"),
            Lang::Ruby => include_str!("queries/ruby.decls.scm"),
            Lang::JavaScript => include_str!("queries/javascript.decls.scm"),
            Lang::TypeScript | Lang::Tsx => include_str!("queries/typescript.decls.scm"),
            Lang::Python => include_str!("queries/python.decls.scm"),
            Lang::Go => include_str!("queries/go.decls.scm"),
        }
    }

    fn refs_query(self) -> &'static str {
        match self {
            Lang::Rust => include_str!("queries/rust.refs.scm"),
            Lang::Ruby => include_str!("queries/ruby.refs.scm"),
            Lang::JavaScript => include_str!("queries/javascript.refs.scm"),
            Lang::TypeScript | Lang::Tsx => include_str!("queries/typescript.refs.scm"),
            Lang::Python => include_str!("queries/python.refs.scm"),
            Lang::Go => include_str!("queries/go.refs.scm"),
        }
    }
}

pub fn lang_for_path(path: &str) -> Option<Lang> {
    let name = path.rsplit('/').next().unwrap_or(path);
    let ext = name.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
    Some(match ext {
        "rs" => Lang::Rust,
        "rb" | "rake" | "gemspec" => Lang::Ruby,
        "js" | "jsx" | "mjs" | "cjs" => Lang::JavaScript,
        "ts" | "mts" | "cts" => Lang::TypeScript,
        "tsx" => Lang::Tsx,
        "py" => Lang::Python,
        "go" => Lang::Go,
        _ => return None,
    })
}

fn parse(lang: Lang, src: &str) -> Option<tree_sitter::Tree> {
    let mut parser = Parser::new();
    parser.set_language(&lang.language()).ok()?;
    parser.parse(src, None)
}

fn signature_of(node: Node, bytes: &[u8]) -> String {
    let text = node.utf8_text(bytes).unwrap_or("");
    let first = text.lines().next().unwrap_or("").trim();
    let mut s = first.to_string();
    if s.len() > SIG_MAX {
        let mut end = SIG_MAX;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
        s.push('…');
    }
    s
}

/// Full symbol outline of a file. `None` when no grammar matches or parse fails.
pub fn outline(path: &str, src: &str) -> Option<Vec<Decl>> {
    let lang = lang_for_path(path)?;
    let tree = parse(lang, src)?;
    let language = lang.language();
    let query = Query::new(&language, lang.decls_query()).ok()?;
    let names = query.capture_names();
    let bytes = src.as_bytes();

    let mut cursor = QueryCursor::new();
    let mut it = cursor.matches(&query, tree.root_node(), bytes);
    let mut out = Vec::new();
    while let Some(m) = it.next() {
        let mut kind: Option<&str> = None;
        let mut def_node: Option<Node> = None;
        let mut name = String::new();
        for cap in m.captures {
            let cn = names[cap.index as usize];
            if let Some(k) = cn.strip_prefix("def.") {
                kind = Some(k);
                def_node = Some(cap.node);
            } else if cn == "name" {
                name = cap.node.utf8_text(bytes).unwrap_or("").to_string();
            }
        }
        if let (Some(k), Some(node)) = (kind, def_node) {
            out.push(Decl {
                kind: k.to_string(),
                name,
                start: node.start_position().row + 1,
                end: node.end_position().row + 1,
                signature: signature_of(node, bytes),
            });
        }
    }
    out.sort_by(|a, b| a.start.cmp(&b.start).then(a.end.cmp(&b.end)));
    Some(out)
}

/// Identifiers referenced within `ranges` (1-based, inclusive). Empty `ranges` → whole file.
pub fn references(path: &str, src: &str, ranges: &[(usize, usize)]) -> Option<Vec<Ref>> {
    let lang = lang_for_path(path)?;
    let tree = parse(lang, src)?;
    let language = lang.language();
    let query = Query::new(&language, lang.refs_query()).ok()?;
    let bytes = src.as_bytes();

    let mut cursor = QueryCursor::new();
    let mut it = cursor.matches(&query, tree.root_node(), bytes);
    let mut out = Vec::new();
    while let Some(m) = it.next() {
        for cap in m.captures {
            let line = cap.node.start_position().row + 1;
            if !ranges.is_empty() && !ranges.iter().any(|(lo, hi)| line >= *lo && line <= *hi) {
                continue;
            }
            let name = cap.node.utf8_text(bytes).unwrap_or("");
            if name.is_empty() {
                continue;
            }
            out.push(Ref {
                name: name.to_string(),
                line,
            });
        }
    }
    Some(out)
}

/// First declaration named `name` in the file, if any.
pub fn definition_in(path: &str, src: &str, name: &str) -> Option<Decl> {
    outline(path, src)?.into_iter().find(|d| d.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outline_rust() {
        let src = "pub fn alpha(x: u32) -> u32 {\n    x + 1\n}\n\nstruct Beta {\n    n: u32,\n}\n";
        let o = outline("a.rs", src).unwrap();
        let names: Vec<_> = o.iter().map(|d| (d.kind.as_str(), d.name.as_str())).collect();
        assert!(names.contains(&("fn", "alpha")));
        assert!(names.contains(&("struct", "Beta")));
        let alpha = o.iter().find(|d| d.name == "alpha").unwrap();
        assert_eq!(alpha.start, 1);
        assert!(alpha.signature.contains("fn alpha"));
    }

    #[test]
    fn outline_ruby_nonempty() {
        let src = "class Foo\n  def bar(a)\n    a * 2\n  end\nend\n";
        let o = outline("foo.rb", src).unwrap();
        assert!(o.iter().any(|d| d.kind == "class" && d.name == "Foo"));
        assert!(o.iter().any(|d| d.kind == "method" && d.name == "bar"));
    }

    #[test]
    fn outline_python_nonempty() {
        let src = "def greet(name):\n    return name\n\nclass Thing:\n    pass\n";
        let o = outline("t.py", src).unwrap();
        assert!(o.iter().any(|d| d.kind == "function" && d.name == "greet"));
        assert!(o.iter().any(|d| d.kind == "class" && d.name == "Thing"));
    }

    #[test]
    fn references_rust_in_range() {
        let src = "fn main() {\n    helper();\n    let f: Foo = other();\n}\n";
        let r = references("a.rs", src, &[(2, 3)]).unwrap();
        let names: Vec<_> = r.iter().map(|x| x.name.as_str()).collect();
        assert!(names.contains(&"helper"));
        assert!(names.contains(&"Foo"));
        assert!(names.contains(&"other"));
    }

    #[test]
    fn definition_lookup() {
        let src = "fn one() {}\nfn two() {}\n";
        let d = definition_in("a.rs", src, "two").unwrap();
        assert_eq!(d.kind, "fn");
        assert_eq!(d.start, 2);
    }

    #[test]
    fn outline_typescript() {
        let src = "export function calc(a: number): number {\n  return a;\n}\n\nclass Widget {\n  render() {}\n}\n\ninterface Opts {\n  x: number;\n}\n";
        let o = outline("w.ts", src).unwrap();
        assert!(o.iter().any(|d| d.kind == "function" && d.name == "calc"));
        assert!(o.iter().any(|d| d.kind == "class" && d.name == "Widget"));
        assert!(o.iter().any(|d| d.kind == "interface" && d.name == "Opts"));
    }

    #[test]
    fn outline_javascript_arrow() {
        let src = "const run = () => {\n  return 1;\n};\n\nfunction plain() {}\n";
        let o = outline("s.js", src).unwrap();
        assert!(o.iter().any(|d| d.kind == "function" && d.name == "run"));
        assert!(o.iter().any(|d| d.kind == "function" && d.name == "plain"));
    }

    #[test]
    fn outline_go() {
        let src = "package main\n\nfunc Add(a int) int {\n\treturn a\n}\n\ntype Thing struct {\n\tN int\n}\n";
        let o = outline("m.go", src).unwrap();
        assert!(o.iter().any(|d| d.kind == "function" && d.name == "Add"));
        assert!(o.iter().any(|d| d.kind == "type" && d.name == "Thing"));
    }

    #[test]
    fn no_grammar_returns_none() {
        assert!(lang_for_path("notes.txt").is_none());
        assert!(outline("notes.txt", "hello").is_none());
    }
}
