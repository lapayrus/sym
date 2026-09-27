use std::cell::RefCell;
use std::path::Path;
use std::sync::LazyLock;

use anyhow::Result;
use tree_sitter_tags::{TagsConfiguration, TagsContext};

// Extra patterns appended after each grammar's own tags.scm. Earlier patterns win
// on the same name node, so these only fill gaps. Imports ride along as `reference.import`
// tags so one parse yields defs, refs and imports.
const RUST_EXTRA: &str = r#"
(call_expression function: (scoped_identifier name: (identifier) @name)) @reference.call
(const_item name: (identifier) @name) @definition.constant
(static_item name: (identifier) @name) @definition.constant
(function_signature_item name: (identifier) @name) @definition.method
(use_declaration argument: (_) @name) @reference.import
"#;
const PYTHON_EXTRA: &str = r#"
(import_statement name: [(dotted_name) @name (aliased_import name: (dotted_name) @name)]) @reference.import
(import_from_statement module_name: (_) @name) @reference.import
"#;
const JS_EXTRA: &str = r#"
(import_statement source: (string (string_fragment) @name)) @reference.import
(export_statement source: (string (string_fragment) @name)) @reference.import
"#;
// ponytail: CommonJS require() imports not indexed; tags queries reject the extra capture a
// #eq? on the callee needs. Add by scanning `require` call refs' first arg if CJS repos matter.
const GO_EXTRA: &str = r#"
(import_spec path: (_) @name) @reference.import
"#;

struct Lang {
    exts: &'static [&'static str],
    cfg: TagsConfiguration,
}

fn lang(exts: &'static [&'static str], language: tree_sitter::Language, queries: &[&str]) -> Lang {
    let cfg = TagsConfiguration::new(language, &queries.concat(), "")
        .unwrap_or_else(|e| panic!("bad tags query for {exts:?}: {e}"));
    Lang { exts, cfg }
}

static LANGS: LazyLock<Vec<Lang>> = LazyLock::new(|| {
    use tree_sitter_javascript as js;
    use tree_sitter_typescript as ts;
    vec![
        lang(&["rs"], tree_sitter_rust::LANGUAGE.into(), &[tree_sitter_rust::TAGS_QUERY, RUST_EXTRA]),
        lang(&["py", "pyi"], tree_sitter_python::LANGUAGE.into(), &[tree_sitter_python::TAGS_QUERY, PYTHON_EXTRA]),
        lang(&["js", "mjs", "cjs", "jsx"], js::LANGUAGE.into(), &[js::TAGS_QUERY, JS_EXTRA]),
        // TS tags.scm only holds TS-specific patterns; the JS ones apply on top.
        lang(&["ts", "mts", "cts"], ts::LANGUAGE_TYPESCRIPT.into(), &[js::TAGS_QUERY, ts::TAGS_QUERY, JS_EXTRA]),
        lang(&["tsx"], ts::LANGUAGE_TSX.into(), &[js::TAGS_QUERY, ts::TAGS_QUERY, JS_EXTRA]),
        lang(&["go"], tree_sitter_go::LANGUAGE.into(), &[tree_sitter_go::TAGS_QUERY, GO_EXTRA]),
    ]
});

thread_local! {
    static CTX: RefCell<TagsContext> = RefCell::new(TagsContext::new());
}

#[derive(Debug)]
pub struct Def {
    pub name: String,
    pub kind: &'static str,
    pub line: u32,
    pub end_line: u32,
    pub sig: String,
}

#[derive(Debug)]
pub struct Ref {
    pub name: String,
    pub kind: &'static str,
    pub line: u32,
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

/// Extract symbols from one file. `None` if the language is unsupported.
pub fn parse(path: &Path, src: &[u8]) -> Result<Option<Parsed>> {
    let Some(lang) = find(path) else { return Ok(None) };
    let newlines: Vec<usize> = src.iter().enumerate().filter(|(_, b)| **b == b'\n').map(|(i, _)| i).collect();
    let line_of = |byte: usize| newlines.partition_point(|&nl| nl < byte) as u32 + 1;
    let text = |r: std::ops::Range<usize>| String::from_utf8_lossy(&src[r]).into_owned();

    let mut out = Parsed::default();
    CTX.with_borrow_mut(|ctx| -> Result<()> {
        let (tags, _has_errors) = ctx.generate_tags(&lang.cfg, src, None)?;
        for tag in tags {
            let tag = tag?;
            let kind = lang.cfg.syntax_type_name(tag.syntax_type_id);
            let name = text(tag.name_range.clone());
            let line = tag.span.start.row as u32 + 1;
            if tag.is_definition {
                // ponytail: signature is the name's line only; multi-line signatures get cut
                let sig: String = text(tag.line_range.clone()).trim().chars().take(160).collect();
                out.defs.push(Def { name, kind, line, end_line: line_of(tag.range.end), sig });
            } else if kind == "import" {
                out.imports.push(name.trim_matches(['"', '`']).to_string());
            } else {
                out.refs.push(Ref { name, kind, line });
            }
        }
        Ok(())
    })?;
    Ok(Some(out))
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
    fn refs(p: &Parsed) -> Vec<(&str, u32)> {
        p.refs.iter().filter(|r| r.kind == "call").map(|r| (r.name.as_str(), r.line)).collect()
    }

    #[test]
    fn all_queries_compile() {
        assert_eq!(LANGS.len(), 6);
    }

    #[test]
    fn rust() {
        let r = p("a.rs", "use std::path::Path;\nconst MAX: u32 = 3;\nstruct Db;\nimpl Db {\n    fn open() -> Self {\n        helper();\n        Db\n    }\n}\nfn helper() { Db::open(); x.run(); }\ntrait T { fn sig(&self); }\n");
        assert_eq!(r.imports, ["std::path::Path"]);
        let d = defs(&r);
        assert!(d.contains(&("MAX", "constant", 2, 2)));
        assert!(d.contains(&("Db", "class", 3, 3)));
        assert!(d.contains(&("open", "method", 5, 8)));
        assert!(d.contains(&("helper", "function", 10, 10)));
        assert!(d.contains(&("sig", "method", 11, 11)));
        assert_eq!(refs(&r), [("helper", 6), ("open", 10), ("run", 10)]);
        assert_eq!(r.defs.iter().find(|d| d.name == "open").unwrap().sig, "fn open() -> Self {");
    }

    #[test]
    fn python() {
        let r = p("a.py", "import os, a.b as c\nfrom .x import y\nclass K:\n    def m(self):\n        os.path.join()\n\ndef f():\n    K().m()\n");
        assert_eq!(r.imports, ["os", "a.b", ".x"]);
        let d = defs(&r);
        assert!(d.contains(&("K", "class", 3, 5)));
        assert!(d.contains(&("m", "function", 4, 5)));
        assert!(d.contains(&("f", "function", 7, 8)));
        assert_eq!(refs(&r), [("join", 5), ("K", 8), ("m", 8)]);
    }

    #[test]
    fn javascript() {
        let r = p("a.js", "import x from './x';\nconst fs = require('fs');\nclass A { run() { go(); } }\nconst go = () => fs.read();\n");
        assert_eq!(r.imports, ["./x"]);
        let d = defs(&r);
        assert!(d.contains(&("A", "class", 3, 3)));
        assert!(d.contains(&("run", "method", 3, 3)));
        assert!(d.contains(&("go", "function", 4, 4)));
        assert_eq!(refs(&r), [("go", 3), ("read", 4)]);
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
        }
    }

    #[test]
    fn go() {
        let r = p("a.go", "package main\n\nimport (\n\t\"fmt\"\n\tx \"example.com/x\"\n)\n\ntype S struct{}\n\nfunc (s S) M() { fmt.Println() }\n\nfunc main() {\n\tS{}.M()\n}\n");
        assert_eq!(r.imports, ["fmt", "example.com/x"]);
        let d = defs(&r);
        assert!(d.contains(&("S", "type", 8, 8)));
        assert!(d.contains(&("M", "method", 10, 10)));
        assert!(d.contains(&("main", "function", 12, 14)));
        assert_eq!(refs(&r), [("Println", 10), ("M", 13)]);
    }

    #[test]
    fn unsupported() {
        assert!(parse(Path::new("a.txt"), b"hi").unwrap().is_none());
    }
}
