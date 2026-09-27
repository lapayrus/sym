use std::cell::RefCell;
use std::cmp::Reverse;
use std::collections::BTreeMap;
use std::ops::Range;
use std::path::Path;
use std::sync::OnceLock;

use anyhow::{Context, Result};
use tree_sitter::{Node, Parser, Query, QueryCursor, StreamingIterator};

// Extra patterns appended after each grammar's own tags.scm. Imports ride along as
// `reference.import` so one parse yields defs, refs and imports. `definition.callback` names
// anonymous callbacks by their string argument (`it("works", () => ..)`, `t.Run("sub", func..)`),
// so calls inside test blocks and handlers get an enclosing definition.
const RUST_EXTRA: &str = r#"
(call_expression function: (scoped_identifier name: (identifier) @name)) @reference.call
(const_item name: (identifier) @name) @definition.constant
(static_item name: (identifier) @name) @definition.constant
(function_signature_item name: (identifier) @name) @definition.method
(use_declaration argument: (_) @name) @reference.import
(impl_item type: [
  (type_identifier) @name
  (generic_type type: (type_identifier) @name)
  (scoped_type_identifier name: (type_identifier) @name)
]) @definition.impl
"#;
const PYTHON_EXTRA: &str = r#"
(import_statement name: [(dotted_name) @name (aliased_import name: (dotted_name) @name)]) @reference.import
(import_from_statement module_name: (_) @name) @reference.import
"#;
const JS_EXTRA: &str = r#"
(import_statement source: (string (string_fragment) @name)) @reference.import
(export_statement source: (string (string_fragment) @name)) @reference.import
((call_expression function: (identifier) arguments: (arguments . (string (string_fragment) @name))) @reference.import
  (#match? @reference.import "^require\\s*\\("))
(call_expression arguments: (arguments . (string (string_fragment) @name) [(arrow_function) (function_expression)])) @definition.callback
"#;
const GO_EXTRA: &str = r#"
(import_spec path: (_) @name) @reference.import
(call_expression arguments: (argument_list . (interpreted_string_literal) @name . (func_literal))) @definition.callback
"#;

/// Definitions whose members get `parent` set (methods, fields of a type).
const CONTAINERS: [&str; 3] = ["class", "interface", "impl"];

/// Names past this column are minified/generated code, never something a human looks up.
// ponytail: fixed column cutoff; the first ~1000 bytes of a minified line still yield junk symbols
const MAX_NAME_COLUMN: usize = 1000;

struct Lang {
    exts: &'static [&'static str],
    language: fn() -> tree_sitter::Language,
    queries: &'static [&'static str],
    /// Compiled on first use: query compilation is the bulk of startup cost.
    cfg: OnceLock<Cfg>,
}

struct Cfg {
    query: Query,
    name: u32,
    /// Per capture index: `(is_definition, kind)` for `definition.*` / `reference.*` captures.
    tags: Vec<Option<(bool, String)>>,
}

impl Lang {
    fn cfg(&self) -> &Cfg {
        self.cfg.get_or_init(|| {
            let query = Query::new(&(self.language)(), &self.queries.concat())
                .unwrap_or_else(|e| panic!("bad tags query for {:?}: {e}", self.exts));
            let names = query.capture_names();
            let tags = names
                .iter()
                .map(|n| {
                    let def = n.strip_prefix("definition.").map(|k| (true, k));
                    def.or_else(|| n.strip_prefix("reference.").map(|k| (false, k))).map(|(d, k)| (d, k.to_string()))
                })
                .collect();
            let name = names.iter().position(|&n| n == "name").expect("query has no @name") as u32;
            Cfg { query, name, tags }
        })
    }
}

use tree_sitter_javascript as js;
use tree_sitter_typescript as ts;

const fn lang(
    exts: &'static [&'static str],
    language: fn() -> tree_sitter::Language,
    queries: &'static [&'static str],
) -> Lang {
    Lang { exts, language, queries, cfg: OnceLock::new() }
}

static LANGS: [Lang; 6] = [
    lang(&["rs"], || tree_sitter_rust::LANGUAGE.into(), &[tree_sitter_rust::TAGS_QUERY, RUST_EXTRA]),
    lang(&["py", "pyi"], || tree_sitter_python::LANGUAGE.into(), &[tree_sitter_python::TAGS_QUERY, PYTHON_EXTRA]),
    lang(&["js", "mjs", "cjs", "jsx"], || js::LANGUAGE.into(), &[js::TAGS_QUERY, JS_EXTRA]),
    // TS tags.scm only holds TS-specific patterns; the JS ones apply on top.
    lang(&["ts", "mts", "cts"], || ts::LANGUAGE_TYPESCRIPT.into(), &[js::TAGS_QUERY, ts::TAGS_QUERY, JS_EXTRA]),
    lang(&["tsx"], || ts::LANGUAGE_TSX.into(), &[js::TAGS_QUERY, ts::TAGS_QUERY, JS_EXTRA]),
    lang(&["go"], || tree_sitter_go::LANGUAGE.into(), &[tree_sitter_go::TAGS_QUERY, GO_EXTRA]),
];

thread_local! {
    static CTX: RefCell<(Parser, QueryCursor)> = RefCell::new((Parser::new(), QueryCursor::new()));
}

#[derive(Debug)]
pub struct Def {
    pub name: String,
    pub kind: &'static str,
    pub line: u32,
    pub end_line: u32,
    pub sig: String,
    /// Enclosing class/interface/impl (Go: receiver type), if this is a member.
    pub parent: Option<String>,
}

#[derive(Debug)]
pub struct Ref {
    pub name: String,
    pub kind: &'static str,
    pub line: u32,
    /// Receiver/qualifier: `x` in `x.f()`, `Db` in `Db::open()`, `*` for a complex expression;
    /// `None` for a bare name.
    pub qual: Option<String>,
}

#[derive(Debug, Default)]
pub struct Parsed {
    pub defs: Vec<Def>,
    pub refs: Vec<Ref>,
    pub imports: Vec<String>,
}

fn find(path: &Path) -> Option<&'static Lang> {
    let ext = path.extension()?.to_str()?;
    LANGS.iter().find(|l| l.exts.contains(&ext))
}

pub fn supported(path: &Path) -> bool {
    find(path).is_some()
}

/// Languages that can call each other (JS and TS share one); `None` if unsupported.
pub fn family(path: &Path) -> Option<&'static str> {
    let first = find(path)?.exts[0];
    Some(if matches!(first, "ts" | "tsx") { "js" } else { first })
}

fn text<'a>(n: Node, src: &'a [u8]) -> std::borrow::Cow<'a, str> {
    String::from_utf8_lossy(&src[n.byte_range()])
}

/// Definition header: its text up to the body (the `body` field, or the body of the function in
/// its `value`/`right`). Without one, tokens are scanned to the first top-level `{`/`;`
/// (Python `:`) or just past `=>`. Strings and comments are single tokens, so brackets inside
/// them don't count, and long strings print as `"..."`. Whitespace collapsed, 200 chars max.
fn signature(def: Node, src: &[u8], python: bool) -> String {
    let body = def.child_by_field_name("body").or_else(|| {
        ["value", "right"].into_iter().find_map(|f| def.child_by_field_name(f)?.child_by_field_name("body"))
    });
    let (stop, elide) = match body {
        Some(b) => (b.start_byte(), Vec::new()),
        None => scan_header(def, python),
    };
    let mut raw = String::new();
    let mut at = def.start_byte();
    for r in elide {
        raw += &String::from_utf8_lossy(&src[at..r.start]);
        raw += "\"...\"";
        at = r.end;
    }
    raw += &String::from_utf8_lossy(&src[at..stop.max(at)]);
    let sig = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    let sig = if python { sig.trim_end_matches(':') } else { &sig };
    sig.chars().take(200).collect()
}

/// End of a body-less definition's header, and the long strings inside it.
fn scan_header(def: Node, python: bool) -> (usize, Vec<Range<usize>>) {
    let mut depth = 0i32;
    let mut elide = Vec::new();
    let mut c = def.walk();
    loop {
        let n = c.node();
        let atomic = n.kind().contains("string") || n.kind().contains("comment");
        if atomic {
            if n.byte_range().len() > 40 {
                elide.push(n.byte_range());
            }
        } else if n.child_count() == 0 && !n.is_named() {
            match n.kind() {
                "(" | "[" => depth += 1,
                ")" | "]" | "}" => depth -= 1,
                "{" | ";" if depth == 0 && !python => return (n.start_byte(), elide),
                "{" => depth += 1,
                ":" if depth == 0 && python => return (n.start_byte(), elide),
                "=>" if depth == 0 => return (n.end_byte(), elide),
                _ => {}
            }
        }
        if !atomic && c.goto_first_child() {
            continue;
        }
        while !c.goto_next_sibling() {
            if !c.goto_parent() {
                return (def.end_byte(), elide);
            }
        }
    }
}

/// Receiver/qualifier of the reference named by `name` (see [`Ref::qual`]).
fn qualifier(name: Node, src: &[u8]) -> Option<String> {
    let p = name.parent()?;
    let q = ["object", "value", "operand", "path"].into_iter().find_map(|f| p.child_by_field_name(f))?;
    if q.id() == name.id() {
        return None;
    }
    let t = text(q, src);
    let t = t.strip_suffix("()").unwrap_or(&t);
    let simple = !t.is_empty() && t.bytes().all(|b| b.is_ascii_alphanumeric() || b"_$.:".contains(&b));
    Some(if simple { t.to_string() } else { "*".into() })
}

fn first_of_kind<'t>(n: Node<'t>, kind: &str) -> Option<Node<'t>> {
    if n.kind() == kind {
        return Some(n);
    }
    let mut c = n.walk();
    let children: Vec<_> = n.named_children(&mut c).collect();
    children.into_iter().find_map(|ch| first_of_kind(ch, kind))
}

/// Extract symbols from one file. `None` if the language is unsupported.
pub fn parse(path: &Path, src: &[u8]) -> Result<Option<Parsed>> {
    let Some(lang) = find(path) else { return Ok(None) };
    let cfg = lang.cfg();
    let python = lang.exts[0] == "py";
    CTX.with_borrow_mut(|(parser, cursor)| {
        parser.set_language(&(lang.language)())?;
        let tree = parser.parse(src, None).context("parser gave up")?;

        // One tag per name node: definitions beat references, then the earliest pattern wins.
        let mut tags = BTreeMap::new();
        let mut matches = cursor.matches(&cfg.query, tree.root_node(), src);
        while let Some(m) = matches.next() {
            let name = m.captures().iter().find(|c| c.index == cfg.name);
            let tag = m.captures().iter().find_map(|c| Some((c.node, cfg.tags[c.index as usize].as_ref()?)));
            let (Some(name), Some((node, (is_def, kind)))) = (name, tag) else { continue };
            let rank = (!is_def, m.pattern_index);
            let key = (name.node.start_byte(), name.node.end_byte());
            let entry = tags.entry(key).or_insert((rank, name.node, node, *is_def, kind.as_str()));
            if rank < entry.0 {
                *entry = (rank, name.node, node, *is_def, kind.as_str());
            }
        }

        let mut out = Parsed::default();
        let mut spans = Vec::new(); // byte span of each def, for parent nesting
        for (_, name, node, is_def, kind) in tags.into_values() {
            let pos = name.start_position();
            if pos.column > MAX_NAME_COLUMN {
                continue;
            }
            let line = pos.row as u32 + 1;
            let name_text = text(name, src);
            if is_def {
                // `type X struct{}`: show the `type` keyword unless it's a grouped `type (...)`.
                let node = match node.parent() {
                    Some(p) if node.kind() == "type_spec" && p.named_child_count() == 1 => p,
                    _ => node,
                };
                let name = name_text.trim_matches('"').to_string();
                let sig = match (kind, node.child_by_field_name("function")) {
                    ("callback", Some(f)) => format!("{}(\"{name}\")", text(f, src)),
                    _ => signature(node, src, python),
                };
                let parent = (node.kind() == "method_declaration")
                    .then(|| node.child_by_field_name("receiver").and_then(|r| first_of_kind(r, "type_identifier")))
                    .flatten()
                    .map(|t| text(t, src).into_owned());
                let end_line = node.end_position().row as u32 + 1;
                out.defs.push(Def { name, kind, line, end_line, sig, parent });
                spans.push(node.byte_range());
            } else if kind == "import" {
                out.imports.push(name_text.trim_matches(['"', '`']).to_string());
            } else {
                // `new ns.Foo()` captures the whole path as the name.
                let (qual, name) = match name_text.rsplit_once('.') {
                    Some((q, n)) => (Some(q.to_string()), n.to_string()),
                    None => (qualifier(name, src), name_text.into_owned()),
                };
                out.refs.push(Ref { name, kind, line, qual });
            }
        }

        // Parent = innermost enclosing def, if it is a container (a method's nested helper is free).
        let mut order: Vec<usize> = (0..spans.len()).collect();
        order.sort_by_key(|&i| (spans[i].start, Reverse(spans[i].end)));
        let mut stack: Vec<usize> = Vec::new();
        for i in order {
            while let Some(&top) = stack.last()
                && spans[top].end <= spans[i].start
            {
                stack.pop();
            }
            if out.defs[i].parent.is_none()
                && let Some(&top) = stack.last()
                && CONTAINERS.contains(&out.defs[top].kind)
            {
                out.defs[i].parent = Some(out.defs[top].name.clone());
            }
            stack.push(i);
        }
        Ok(Some(out))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(file: &str, src: &str) -> Parsed {
        parse(Path::new(file), src.as_bytes()).unwrap().unwrap()
    }
    fn defs(p: &Parsed) -> Vec<(&str, &str, u32, u32)> {
        p.defs.iter().map(|d| (d.name.as_str(), d.kind, d.line, d.end_line)).collect()
    }
    fn def<'a>(p: &'a Parsed, name: &str) -> &'a Def {
        p.defs.iter().find(|d| d.name == name).unwrap()
    }
    fn sig<'a>(p: &'a Parsed, name: &str) -> &'a str {
        &def(p, name).sig
    }
    fn parent<'a>(p: &'a Parsed, name: &str) -> Option<&'a str> {
        def(p, name).parent.as_deref()
    }
    fn refs(p: &Parsed) -> Vec<(&str, u32)> {
        p.refs.iter().filter(|r| r.kind == "call").map(|r| (r.name.as_str(), r.line)).collect()
    }
    fn quals(p: &Parsed) -> Vec<(&str, Option<&str>)> {
        p.refs.iter().filter(|r| r.kind == "call").map(|r| (r.name.as_str(), r.qual.as_deref())).collect()
    }

    #[test]
    fn all_queries_compile() {
        for l in &LANGS {
            l.cfg();
        }
    }

    #[test]
    fn rust() {
        let r = p("a.rs", "use std::path::Path;\nconst MAX: u32 = 3;\nstruct Db;\nimpl Db {\n    fn open() -> Self {\n        helper();\n        Db\n    }\n}\nfn helper() { Db::open(); x.run(); }\ntrait T { fn sig(&self); }\npub fn long<T>(\n    a: u32,\n    b: [u8; 2],\n) -> T\nwhere\n    T: Default,\n{\n    T::default()\n}\nconst S: &str = \"{\";\nconst Q: &str = \"a long string literal that is not worth showing\";\nimpl<W> fmt::Display for Wrap<W> {}\n");
        assert_eq!(r.imports, ["std::path::Path"]);
        let d = defs(&r);
        assert!(d.contains(&("MAX", "constant", 2, 2)));
        assert!(d.contains(&("Db", "class", 3, 3)));
        assert!(d.contains(&("Db", "impl", 4, 9)));
        assert!(d.contains(&("open", "method", 5, 8)));
        assert!(d.contains(&("helper", "function", 10, 10)));
        assert!(d.contains(&("sig", "method", 11, 11)));
        assert_eq!(refs(&r), [("helper", 6), ("open", 10), ("run", 10), ("default", 19)]);
        assert_eq!(quals(&r), [("helper", None), ("open", Some("Db")), ("run", Some("x")), ("default", Some("T"))]);
        assert!(d.contains(&("long", "function", 12, 20)));
        assert_eq!(sig(&r, "open"), "fn open() -> Self");
        assert_eq!(sig(&r, "sig"), "fn sig(&self)");
        assert_eq!(sig(&r, "MAX"), "const MAX: u32 = 3");
        assert_eq!(sig(&r, "long"), "pub fn long<T>( a: u32, b: [u8; 2], ) -> T where T: Default,");
        assert_eq!(sig(&r, "S"), "const S: &str = \"{\""); // bracket inside a string doesn't end it
        assert_eq!(sig(&r, "Q"), "const Q: &str = \"...\"");
        assert_eq!(sig(&r, "Wrap"), "impl<W> fmt::Display for Wrap<W>");
        assert_eq!((parent(&r, "open"), parent(&r, "sig"), parent(&r, "helper")), (Some("Db"), Some("T"), None));
    }

    #[test]
    fn python() {
        let r = p("a.py", "import os, a.b as c\nfrom .x import y\nclass K:\n    def m(self):\n        os.path.join()\n        def inner(): pass\n\ndef f(\n    a: dict = {\"k\": 1},\n) -> list[int]:\n    K().m()\n    self.m()\n");
        assert_eq!(r.imports, ["os", "a.b", ".x"]);
        let d = defs(&r);
        assert!(d.contains(&("K", "class", 3, 6)));
        assert!(d.contains(&("m", "function", 4, 6)));
        assert!(d.contains(&("f", "function", 8, 12)));
        assert_eq!(refs(&r), [("join", 5), ("K", 11), ("m", 11), ("m", 12)]);
        assert_eq!(quals(&r), [("join", Some("os.path")), ("K", None), ("m", Some("K")), ("m", Some("self"))]);
        assert_eq!(sig(&r, "K"), "class K");
        assert_eq!(sig(&r, "f"), "def f( a: dict = {\"k\": 1}, ) -> list[int]");
        assert_eq!((parent(&r, "m"), parent(&r, "inner"), parent(&r, "f")), (Some("K"), None, None));
    }

    #[test]
    fn javascript() {
        let r = p("a.js", "import x from './x';\nconst fs = require('fs');\nclass A { run() { go(); } }\nconst go = () => fs.read();\ndescribe(\"suite\", () => {\n  it('works', async () => { new ns.Thing(); });\n});\n");
        assert_eq!(r.imports, ["./x", "fs"]);
        assert_eq!(sig(&r, "go"), "go = () =>");
        assert_eq!(sig(&r, "run"), "run()");
        assert_eq!(sig(&r, "works"), "it(\"works\")");
        let d = defs(&r);
        assert!(d.contains(&("A", "class", 3, 3)));
        assert!(d.contains(&("run", "method", 3, 3)));
        assert!(d.contains(&("go", "function", 4, 4)));
        assert!(d.contains(&("suite", "callback", 5, 7)));
        assert!(d.contains(&("works", "callback", 6, 6)));
        assert_eq!(parent(&r, "run"), Some("A"));
        assert_eq!(refs(&r), [("go", 3), ("read", 4), ("describe", 5), ("it", 6)]);
        let thing = r.refs.iter().find(|r| r.kind == "class").unwrap();
        assert_eq!((thing.name.as_str(), thing.qual.as_deref()), ("Thing", Some("ns")));
    }

    #[test]
    fn typescript() {
        for file in ["a.ts", "a.tsx"] {
            let r = p(file, "import { y } from \"./y\";\ninterface I { m(): void }\nexport function f(a: I): number {\n  return y(a);\n}\n");
            assert_eq!(r.imports, ["./y"]);
            let d = defs(&r);
            assert!(d.contains(&("I", "interface", 2, 2)), "{file}");
            assert!(d.contains(&("m", "method", 2, 2)), "{file}");
            assert!(d.contains(&("f", "function", 3, 5)), "{file}");
            assert_eq!(refs(&r), [("y", 4)]);
            assert_eq!(sig(&r, "f"), "function f(a: I): number");
            assert_eq!(sig(&r, "m"), "m(): void");
            assert_eq!(parent(&r, "m"), Some("I"));
        }
    }

    #[test]
    fn go() {
        let r = p("a.go", "package main\n\nimport (\n\t\"fmt\"\n\tx \"example.com/x\"\n)\n\ntype S struct{}\n\nfunc (s *S) M() { fmt.Println() }\n\nfunc main() {\n\tS{}.M()\n\tt.Run(\"sub\", func(t *testing.T) {})\n}\n");
        assert_eq!(r.imports, ["fmt", "example.com/x"]);
        let d = defs(&r);
        assert!(d.contains(&("S", "type", 8, 8)));
        assert!(d.contains(&("M", "method", 10, 10)));
        assert!(d.contains(&("main", "function", 12, 15)));
        assert!(d.contains(&("sub", "callback", 14, 14)));
        assert_eq!(refs(&r), [("Println", 10), ("M", 13), ("Run", 14)]);
        assert_eq!(quals(&r)[0], ("Println", Some("fmt")));
        assert_eq!(sig(&r, "M"), "func (s *S) M()");
        assert_eq!(sig(&r, "S"), "type S struct");
        assert_eq!(sig(&r, "sub"), "t.Run(\"sub\")");
        assert_eq!(parent(&r, "M"), Some("S"));
    }

    #[test]
    fn minified_columns_dropped() {
        let r = p("app.min.js", &"function q(){r()}".repeat(400));
        // Names start at columns 9 + 17k (defs) and 13 + 17k (calls); only those <= 1000 are kept.
        assert_eq!((r.defs.len(), r.refs.len()), (59, 59));
        // A normal file with one huge data line keeps its symbols.
        let src = format!("const DATA = [{}];\nfunction f() {{ g(); }}\n", "1,".repeat(5000));
        assert_eq!(defs(&p("data.js", &src)), [("f", "function", 2, 2)]);
    }

    #[test]
    fn unsupported() {
        assert!(parse(Path::new("a.txt"), b"hi").unwrap().is_none());
    }
}
