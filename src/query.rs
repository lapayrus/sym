//! Read-side queries. Each returns ready-to-print, line-oriented text grouped by file,
//! shared by the CLI and (phase 4) the MCP server.

use std::collections::HashMap;
use std::fmt::Write;

use anyhow::Result;
use nucleo_matcher::pattern::{Atom, AtomKind, CaseMatching, Normalization};
use nucleo_matcher::{Config, Matcher};
use rusqlite::{Connection, OptionalExtension, params};

/// Escape `\ % _` for `LIKE ... ESCAPE '\'`.
fn like_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
}

/// `Parent.name` / `Parent::name` → (Some("Parent"), "name"); a plain name → (None, name).
fn split_qualified(q: &str) -> (Option<&str>, &str) {
    match q.rsplit_once("::").or_else(|| q.rsplit_once('.')) {
        Some((p, n)) if !n.is_empty() && !q.contains(' ') => (p.rsplit([':', '.']).next(), n),
        _ => (None, q),
    }
}

/// A definition a reference may point at.
struct Target {
    file_id: i64,
    path: String,
    line: i64,
    end: i64,
    kind: String,
    sig: String,
    parent: Option<String>,
}

impl Target {
    fn header(&self) -> String {
        let Target { path, line, end, kind, sig, .. } = self;
        match &self.parent {
            Some(p) => format!("{path}:{line}-{end} {kind} in {p}: {sig}"),
            None => format!("{path}:{line}-{end} {kind} {sig}"),
        }
    }
}

/// Definitions named `name` (optionally only members of `parent`).
fn targets(conn: &Connection, name: &str, parent: Option<&str>, limit: usize) -> Result<Vec<Target>> {
    let mut stmt = conn.prepare_cached(
        "SELECT s.file_id, f.path, s.line, s.end_line, s.kind, s.sig, s.parent
         FROM symbols s JOIN files f ON f.id = s.file_id
         WHERE s.name = ?1 AND (?2 IS NULL OR s.parent = ?2) ORDER BY f.path, s.line LIMIT ?3",
    )?;
    let rows = stmt.query_map(params![name, parent, limit as i64], |r| {
        Ok(Target {
            file_id: r.get(0)?,
            path: r.get(1)?,
            line: r.get(2)?,
            end: r.get(3)?,
            kind: r.get(4)?,
            sig: r.get(5)?,
            parent: r.get(6)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// For resolution, one definition per (file, parent): TS overload signatures and their
/// implementation are one callable, represented by the widest (the one with the body).
fn merge_overloads(mut t: Vec<Target>) -> Vec<Target> {
    t.sort_by(|a, b| (a.file_id, &a.parent, b.end - b.line).cmp(&(b.file_id, &b.parent, a.end - a.line)));
    t.dedup_by(|b, a| a.file_id == b.file_id && a.parent == b.parent);
    t.sort_by(|a, b| (&a.path, a.line).cmp(&(&b.path, b.line)));
    t
}

/// Definitions for a user query: `name` or `Parent.name`; a qualifier that matches nothing is
/// retried as a whole name (callback names contain dots) and then ignored.
fn lookup(conn: &Connection, query: &str, limit: usize) -> Result<Vec<Target>> {
    let (parent, name) = split_qualified(query);
    if parent.is_some() {
        for (n, p) in [(name, parent), (query, None)] {
            let t = targets(conn, n, p, limit)?;
            if !t.is_empty() {
                return Ok(t);
            }
        }
    }
    targets(conn, name, None, limit)
}

/// The innermost definition around a reference.
struct Caller {
    name: String,
    line: i64,
    parent: Option<String>,
}

fn enclosing(conn: &Connection, file_id: i64, line: i64) -> Result<Option<Caller>> {
    Ok(conn
        .prepare_cached(
            "SELECT name, line, parent FROM symbols WHERE file_id = ?1 AND line <= ?2 AND end_line >= ?2
             ORDER BY line DESC LIMIT 1",
        )?
        .query_row(params![file_id, line], |r| Ok(Caller { name: r.get(0)?, line: r.get(1)?, parent: r.get(2)? }))
        .optional()?)
}

/// One reference: where it is and how it's qualified.
#[derive(Clone)]
struct Site {
    file_id: i64,
    path: String,
    line: i64,
    kind: String,
    qual: Option<String>,
}

const INDEX_FILES: [&str; 5] = ["mod", "index", "lib", "main", "__init__"];

/// Module names a file answers to: its stem, and its directory (Go packages, `mod.rs`, `index.ts`).
fn module_names(path: &str) -> [&str; 2] {
    let mut parts = path.rsplit('/');
    let file = parts.next().unwrap_or("");
    let dir = parts.next().unwrap_or("");
    let stem = file.split('.').next().unwrap_or("");
    let stem = if INDEX_FILES.contains(&stem) { dir } else { stem };
    [stem, dir]
}

/// How specifically `import` names the file at `path`: trailing path segments in common,
/// also trying a Rust import without its last segment (`use crate::db::put_file`) and the file's
/// directory (Go packages). `"../../src/api/async/api"` scores 4 against
/// `packages/x/src/api/async/api.ts` but 1 against `.../api/sync/api.ts`.
fn import_match(import: &str, path: &str) -> usize {
    const EXTS: [&str; 9] = ["js", "jsx", "ts", "tsx", "mjs", "cjs", "rs", "py", "go"];
    let segs = |s: &str| -> Vec<String> {
        let s = match s.rsplit_once('.') {
            Some((head, ext)) if EXTS.contains(&ext) => head.to_string(),
            _ => s.to_string(),
        };
        s.split(['/', '.', ':', '\\']).filter(|p| !p.is_empty()).map(str::to_string).collect()
    };
    let imp = segs(import);
    let mut file = segs(path);
    if file.last().is_some_and(|l| INDEX_FILES.contains(&l.as_str())) {
        file.pop();
    }
    let dir = file[..file.len().saturating_sub(1)].to_vec();
    // Only Rust paths end in an item name; a JS/Go import's last segment is the module itself.
    let imp_short = if import.contains("::") { imp[..imp.len().saturating_sub(1)].to_vec() } else { Vec::new() };
    let common = |a: &[String], b: &[String]| a.iter().rev().zip(b.iter().rev()).take_while(|(x, y)| x == y).count();
    [(&imp, &file), (&imp, &dir), (&imp_short, &file), (&imp_short, &dir)]
        .into_iter()
        .map(|(a, b)| common(a, b))
        .max()
        .unwrap_or(0)
}

/// Keep the items passing `pred`, unless none do.
fn narrow(v: Vec<usize>, pred: impl Fn(usize) -> bool) -> Vec<usize> {
    let n: Vec<usize> = v.iter().copied().filter(|&i| pred(i)).collect();
    if n.is_empty() { v } else { n }
}

/// Resolves references to definitions from syntax alone (no types):
/// - bare `f()` → free definitions (not members), preferring the same file, then imported files;
/// - `self.f()` / `this.f()` → members of the caller's own class/impl;
/// - `Q.f()` / `Q::f()` → members of type `Q`, else of a type whose name ends with `Q`
///   (`searcher` → `Searcher`), else free defs in module `Q`; then a `::` path or capitalized
///   `Q` is external, while a variable resolves to the caller's own type, else every candidate
///   (ambiguous).
// ponytail: syntactic heuristics; wrong when a receiver's name says nothing about its type
// (`x.run()` with several `run` methods → ambiguous). Needs per-language type inference to beat.
struct Resolver<'c> {
    conn: &'c Connection,
    imports: HashMap<i64, Vec<String>>,
}

impl<'c> Resolver<'c> {
    fn new(conn: &'c Connection) -> Self {
        Resolver { conn, imports: HashMap::new() }
    }

    fn imports(&mut self, file_id: i64) -> Result<&[String]> {
        if !self.imports.contains_key(&file_id) {
            let mods = self
                .conn
                .prepare_cached("SELECT module FROM imports WHERE file_id = ?1")?
                .query_map([file_id], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            self.imports.insert(file_id, mods);
        }
        Ok(&self.imports[&file_id])
    }

    /// Indices into `targets` that `site` may refer to; empty = none (external or not indexed).
    fn resolve(&mut self, site: &Site, targets: &[Target]) -> Result<Vec<usize>> {
        let hits = self.candidates(site, targets)?;
        self.tiebreak(site, targets, hits)
    }

    fn candidates(&self, site: &Site, targets: &[Target]) -> Result<Vec<usize>> {
        // A TS file never calls a Go method of the same name.
        let family = crate::lang::family(std::path::Path::new(&site.path));
        let pick = |pred: &dyn Fn(&Target) -> bool| -> Vec<usize> {
            (0..targets.len())
                .filter(|&i| crate::lang::family(std::path::Path::new(&targets[i].path)) == family && pred(&targets[i]))
                .collect()
        };
        let caller_parent = || -> Result<Option<String>> {
            Ok(enclosing(self.conn, site.file_id, site.line)?.and_then(|c| c.parent))
        };
        let Some(q) = site.qual.as_deref() else {
            return Ok(pick(&|t| t.parent.is_none()));
        };
        let last = q.rsplit([':', '.']).next().unwrap_or(q);
        if ["self", "this", "Self", "cls", "super"].contains(&last) {
            let cp = caller_parent()?;
            return Ok(narrow(pick(&|t| t.parent.is_some()), |i| targets[i].parent == cp));
        }
        let exact = pick(&|t| t.parent.as_deref() == Some(last));
        if !exact.is_empty() {
            return Ok(exact);
        }
        let l = last.trim_matches('_').to_lowercase();
        if l.len() >= 3 {
            let named = pick(&|t| t.parent.as_ref().is_some_and(|p| p.to_lowercase().ends_with(&l)));
            if !named.is_empty() {
                return Ok(named);
            }
        }
        let module = pick(&|t| t.parent.is_none() && module_names(&t.path).contains(&last));
        if !module.is_empty() {
            return Ok(module);
        }
        // `Vec::new()`, `React.createElement()`: an explicit type/module that isn't indexed.
        if q.contains("::") || last.starts_with(|c: char| c.is_uppercase()) {
            return Ok(Vec::new());
        }
        let cp = caller_parent()?;
        Ok(narrow(pick(&|_| true), |i| cp.is_some() && targets[i].parent == cp))
    }

    /// Several candidates left: prefer the same file, then the best-matching import, then the
    /// same directory (Go packages, sibling modules).
    fn tiebreak(&mut self, site: &Site, targets: &[Target], hits: Vec<usize>) -> Result<Vec<usize>> {
        if hits.len() < 2 {
            return Ok(hits);
        }
        let hits = narrow(hits, |i| targets[i].file_id == site.file_id);
        if hits.len() < 2 {
            return Ok(hits);
        }
        let imports = self.imports(site.file_id)?;
        let score = |i: usize| imports.iter().map(|m| import_match(m, &targets[i].path)).max().unwrap_or(0);
        let best = hits.iter().map(|&i| score(i)).max().unwrap_or(0);
        let hits = if best > 0 { hits.into_iter().filter(|&i| score(i) == best).collect() } else { hits };
        let dir = |p: &str| p.rsplit_once('/').map_or("", |(d, _)| d).to_string();
        Ok(narrow(hits, |i| dir(&targets[i].path) == dir(&site.path)))
    }
}

/// References to `query` (optionally `kind`), bucketed by the definition they resolve to.
/// Returns (shown targets, per-target sites with an ambiguity flag, unresolved sites).
#[allow(clippy::type_complexity)]
fn resolved_sites(
    conn: &Connection,
    query: &str,
    kind: Option<&str>,
) -> Result<(Vec<Target>, Vec<Vec<(Site, bool)>>, Vec<Site>)> {
    let (parent, name) = split_qualified(query);
    let mut all = merge_overloads(targets(conn, name, None, 1000)?);
    let mut name = name;
    if all.is_empty() && parent.is_some() {
        name = query; // a callback name containing dots
        all = merge_overloads(targets(conn, name, None, 1000)?);
    }
    let mut stmt = conn.prepare_cached(
        "SELECT r.file_id, f.path, r.line, r.kind, r.qual FROM refs r JOIN files f ON f.id = r.file_id
         WHERE r.name = ?1 AND (?2 IS NULL OR r.kind = ?2) ORDER BY f.path, r.line",
    )?;
    let sites = stmt.query_map(params![name, kind], |r| {
        Ok(Site { file_id: r.get(0)?, path: r.get(1)?, line: r.get(2)?, kind: r.get(3)?, qual: r.get(4)? })
    })?;
    let mut res = Resolver::new(conn);
    let mut buckets: Vec<Vec<(Site, bool)>> = (0..all.len()).map(|_| Vec::new()).collect();
    let mut unresolved = Vec::new();
    for site in sites {
        let site = site?;
        let hits = if all.is_empty() { Vec::new() } else { res.resolve(&site, &all)? };
        match hits.as_slice() {
            [] => unresolved.push(site),
            [i] => buckets[*i].push((site, false)),
            many => {
                for &i in many {
                    buckets[i].push((site.clone(), true));
                }
            }
        }
    }
    // A qualified query shows only the matching members; the rest only competed in resolution.
    let keep: Vec<bool> = all.iter().map(|t| name == query || parent.is_none() || t.parent.as_deref() == parent).collect();
    let keep = if keep.contains(&true) { keep } else { vec![true; all.len()] };
    let mut shown = Vec::new();
    let mut shown_buckets = Vec::new();
    for ((t, b), k) in all.into_iter().zip(buckets).zip(keep) {
        if k {
            shown.push(t);
            shown_buckets.push(b);
        }
    }
    if parent.is_some() && name != query {
        unresolved.clear();
    }
    Ok((shown, shown_buckets, unresolved))
}

/// Append `path:line-end kind sig` for every definition of `name`, at most `limit` lines.
fn defs_into(conn: &Connection, name: &str, limit: usize, out: &mut String) -> Result<usize> {
    let t = targets(conn, name, None, limit)?;
    for t in &t {
        writeln!(out, "{}", t.header())?;
    }
    Ok(t.len())
}

pub fn def(conn: &Connection, query: &str) -> Result<String> {
    let mut out = String::new();
    for t in lookup(conn, query, usize::MAX >> 1)? {
        writeln!(out, "{}", t.header())?;
    }
    if out.is_empty() {
        writeln!(out, "no definition of `{query}`")?;
    }
    Ok(out)
}

const UNRESOLVED: &str = "(unresolved: external, or no matching definition indexed)";

/// Every reference to `query`, per definition it resolves to, grouped by file, each with its
/// enclosing definition. `?` marks a reference that could equally mean another definition.
pub fn refs(conn: &Connection, query: &str, limit: usize) -> Result<String> {
    let (targets, buckets, unresolved) = resolved_sites(conn, query, None)?;
    let groups = targets
        .iter()
        .map(Target::header)
        .zip(buckets)
        .chain([(UNRESOLVED.to_string(), unresolved.into_iter().map(|s| (s, false)).collect())]);
    let mut out = String::new();
    let (mut shown, mut total) = (0, 0);
    for (header, sites) in groups {
        total += sites.len();
        if sites.is_empty() || shown >= limit {
            continue;
        }
        writeln!(out, "{header}")?;
        let mut last = String::new();
        for (s, ambiguous) in sites.iter().take(limit - shown) {
            if s.path != last {
                writeln!(out, "  {}", s.path)?;
                last.clone_from(&s.path);
            }
            let mark = if *ambiguous { "?" } else { "" };
            match enclosing(conn, s.file_id, s.line)? {
                Some(c) => writeln!(out, "    {}{mark} {} in {}", s.line, s.kind, c.name)?,
                None => writeln!(out, "    {}{mark} {}", s.line, s.kind)?,
            }
            shown += 1;
        }
    }
    if total == 0 {
        writeln!(out, "no references to `{query}`")?;
    } else if total > shown {
        writeln!(out, "... {} more", total - shown)?;
    }
    Ok(out)
}

/// Word initials of an identifier, lowercased: `getTypeOfSymbol` / `get_type_of_symbol` → `gtos`.
fn initials(name: &str) -> String {
    let mut out = String::new();
    let mut prev = '_';
    for c in name.chars() {
        if c.is_alphanumeric() && (!prev.is_alphanumeric() || (c.is_uppercase() && !prev.is_uppercase())) {
            out.extend(c.to_lowercase());
        }
        prev = c;
    }
    out
}

/// Definitions whose name fuzzy-matches `query`, best first: names whose word initials spell
/// the query (`gtos` → `getTypeOfSymbol`), then nucleo's score (word boundaries, camelCase,
/// consecutive runs), then shorter names. Test/handler callbacks are left out.
// ponytail: LIKE subsequence prefilter scans all distinct names per call; keep the name list
// in memory in `sym serve` if this gets slow
pub fn search(conn: &Connection, query: &str, limit: usize) -> Result<String> {
    let pattern: String = query.chars().map(|c| format!("%{}", like_escape(&c.to_string()))).collect::<String>() + "%";
    let names: Vec<String> = conn
        .prepare_cached("SELECT DISTINCT name FROM symbols WHERE name LIKE ?1 ESCAPE '\\' AND kind != 'callback'")?
        .query_map([pattern], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let atom = Atom::new(query, CaseMatching::Ignore, Normalization::Smart, AtomKind::Fuzzy, false);
    let mut scored = atom.match_list(names, &mut Matcher::new(Config::DEFAULT));
    let q = query.to_lowercase();
    scored.sort_by_cached_key(|(n, score)| (initials(n) != q, std::cmp::Reverse(*score), n.len(), n.clone()));
    let mut out = String::new();
    let mut left = limit;
    for (name, _) in &scored {
        if left == 0 {
            break;
        }
        left -= defs_into(conn, name, left, &mut out)?;
    }
    if out.is_empty() {
        writeln!(out, "no symbols match `{query}`")?;
    }
    Ok(out)
}

/// Who calls `query`: per definition it resolves to, one line per calling definition with its
/// call-site lines (`?` = could equally be a call to another definition of the same name).
pub fn callers(conn: &Connection, query: &str, limit: usize) -> Result<String> {
    let (targets, buckets, unresolved) = resolved_sites(conn, query, Some("call"))?;
    let groups = targets
        .iter()
        .map(Target::header)
        .zip(buckets)
        .chain([(UNRESOLVED.to_string(), unresolved.into_iter().map(|s| (s, false)).collect())]);
    let mut out = String::new();
    let (mut shown, mut total) = (0, 0);
    for (header, sites) in groups {
        total += sites.len();
        if sites.is_empty() || shown >= limit {
            continue;
        }
        writeln!(out, "{header}")?;
        // (path, caller name, caller line, call lines); a caller's calls are contiguous.
        let mut lines: Vec<(&str, Option<Caller>, Vec<String>)> = Vec::new();
        for (s, ambiguous) in sites.iter().take(limit - shown) {
            let caller = enclosing(conn, s.file_id, s.line)?;
            let at = format!("{}{}", s.line, if *ambiguous { "?" } else { "" });
            match lines.last_mut() {
                Some((p, c, ls)) if *p == s.path && c.as_ref().map(|c| c.line) == caller.as_ref().map(|c| c.line) => {
                    ls.push(at)
                }
                _ => lines.push((&s.path, caller, vec![at])),
            }
            shown += 1;
        }
        for (path, caller, ls) in lines {
            match caller {
                Some(c) => writeln!(out, "  {path}:{} {} ({})", c.line, c.name, ls.join(", "))?,
                None => writeln!(out, "  {path} <top level> ({})", ls.join(", "))?,
            }
        }
    }
    if total == 0 {
        writeln!(out, "no calls to `{query}`")?;
    } else if total > shown {
        writeln!(out, "... {} more call sites", total - shown)?;
    }
    Ok(out)
}

/// What `query` calls: for each of its definitions, each distinct callee resolved to its
/// definition (`?` = ambiguous, up to 3 shown), then the unresolved (external) names.
pub fn callees(conn: &Connection, query: &str, limit: usize) -> Result<String> {
    let defs = lookup(conn, query, limit)?;
    let mut out = String::new();
    if defs.is_empty() {
        writeln!(out, "no definition of `{query}`")?;
    }
    let mut res = Resolver::new(conn);
    let mut calls = conn.prepare_cached(
        "SELECT name, qual, line FROM refs WHERE file_id = ?1 AND line BETWEEN ?2 AND ?3 AND kind = 'call' ORDER BY line",
    )?;
    for d in defs {
        writeln!(out, "{}", d.header())?;
        let sites: Vec<(String, Option<String>, i64)> = calls
            .query_map(params![d.file_id, d.line, d.end], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<rusqlite::Result<_>>()?;
        let (mut lines, mut external): (Vec<String>, Vec<String>) = (Vec::new(), Vec::new());
        for (name, qual, line) in sites {
            let site = Site { file_id: d.file_id, path: d.path.clone(), line, kind: "call".into(), qual };
            let cands = merge_overloads(targets(conn, &name, None, 50)?);
            let hits = if cands.is_empty() { Vec::new() } else { res.resolve(&site, &cands)? };
            let at = |i: &usize| format!("{}:{}", cands[*i].path, cands[*i].line);
            let entry = match hits.as_slice() {
                [] => {
                    if !external.contains(&name) {
                        external.push(name);
                    }
                    continue;
                }
                [i] => format!("  {name} {}", at(i)),
                many => {
                    let shown: Vec<String> = many.iter().take(3).map(at).collect();
                    let more = if many.len() > 3 { format!(" +{}", many.len() - 3) } else { String::new() };
                    format!("  {name}? {}{more}", shown.join(", "))
                }
            };
            if !lines.contains(&entry) {
                lines.push(entry);
            }
        }
        for l in &lines {
            writeln!(out, "{l}")?;
        }
        if !external.is_empty() {
            writeln!(out, "  external: {}", external.join(", "))?;
        }
        if lines.is_empty() && external.is_empty() {
            writeln!(out, "  (no calls)")?;
        }
    }
    Ok(out)
}

/// Definitions in the file(s) at `path` (exact, or a `/`-suffix match), nested by span.
pub fn outline(conn: &Connection, path: &str) -> Result<String> {
    let files: Vec<(i64, String)> = conn
        .prepare_cached("SELECT id, path FROM files WHERE path = ?1 OR path LIKE ?2 ESCAPE '\\' ORDER BY path")?
        .query_map(params![path, format!("%/{}", like_escape(path))], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let mut out = String::new();
    if files.is_empty() {
        writeln!(out, "not indexed: `{path}`")?;
    }
    let mut stmt = conn.prepare_cached(
        "SELECT line, end_line, kind, sig, name FROM symbols WHERE file_id = ?1 ORDER BY line, end_line DESC",
    )?;
    for (id, path) in files {
        writeln!(out, "{path}")?;
        let mut open: Vec<i64> = Vec::new(); // end lines of enclosing defs
        let mut rows = stmt.query([id])?;
        while let Some(r) = rows.next()? {
            let (line, end, kind, sig, name): (i64, i64, String, String, String) =
                (r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?);
            while open.last().is_some_and(|&e| e < line) {
                open.pop();
            }
            let indent = "  ".repeat(open.len() + 1);
            writeln!(out, "{indent}{line}-{end} {kind} {}", if sig.is_empty() { name } else { sig })?;
            open.push(end);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{db, index};
    use std::fs;

    #[test]
    fn queries() {
        let root = std::env::temp_dir().join(format!("sym-query-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("src/a.rs"),
            "fn helper() {}\nfn run() {\n    helper();\n    helper();\n}\nstruct S;\nimpl S {\n    fn go(&self) { helper(); run(); self.x(); }\n}\n",
        )
        .unwrap();
        fs::write(
            root.join("b.py"),
            "def helper_two():\n    pass\n\nclass K:\n    def m(self):\n        helper()\n\nhelper()\n",
        )
        .unwrap();
        let mut conn = db::open(&root).unwrap();
        index::index(&root, &mut conn).unwrap();

        assert_eq!(def(&conn, "helper").unwrap(), "src/a.rs:1-1 function fn helper()\n");
        assert_eq!(def(&conn, "S.go").unwrap(), "src/a.rs:8-8 method in S: fn go(&self)\n");
        assert_eq!(def(&conn, "nope").unwrap(), "no definition of `nope`\n");

        assert_eq!(
            refs(&conn, "helper", 2).unwrap(),
            "src/a.rs:1-1 function fn helper()\n  src/a.rs\n    3 call in run\n    4 call in run\n... 3 more\n"
        );

        assert_eq!(
            search(&conn, "help", 20).unwrap(),
            "src/a.rs:1-1 function fn helper()\nb.py:1-2 function def helper_two()\n"
        );
        assert_eq!(search(&conn, "HTWO", 20).unwrap(), "b.py:1-2 function def helper_two()\n");
        // `_` is literal, not a LIKE wildcard.
        assert_eq!(search(&conn, "_", 20).unwrap(), "b.py:1-2 function def helper_two()\n");
        assert_eq!(search(&conn, "help", 1).unwrap(), "src/a.rs:1-1 function fn helper()\n");

        assert_eq!(
            callers(&conn, "helper", 50).unwrap(),
            // Python can't call the Rust `helper`: those calls go to an undefined Python one.
            "src/a.rs:1-1 function fn helper()\n  src/a.rs:2 run (3, 4)\n  src/a.rs:8 go (8)\n\
             (unresolved: external, or no matching definition indexed)\n  b.py:5 m (6)\n  b.py <top level> (8)\n"
        );
        assert_eq!(
            callers(&conn, "helper", 1).unwrap(),
            "src/a.rs:1-1 function fn helper()\n  src/a.rs:2 run (3)\n... 4 more call sites\n"
        );
        assert_eq!(
            callees(&conn, "go", 50).unwrap(),
            "src/a.rs:8-8 method in S: fn go(&self)\n  helper src/a.rs:1\n  run src/a.rs:2\n  external: x\n"
        );
        assert_eq!(callees(&conn, "helper", 50).unwrap(), "src/a.rs:1-1 function fn helper()\n  (no calls)\n");

        assert_eq!(
            outline(&conn, "a.rs").unwrap(),
            "src/a.rs\n  1-1 function fn helper()\n  2-5 function fn run()\n  6-6 class struct S\n  7-9 impl impl S\n    8-8 method fn go(&self)\n"
        );
        assert_eq!(
            outline(&conn, "b.py").unwrap(),
            "b.py\n  1-2 function def helper_two()\n  4-6 class class K\n    5-6 function def m(self)\n"
        );
        assert_eq!(outline(&conn, "x.rs").unwrap(), "not indexed: `x.rs`\n");

        drop(conn);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn helpers() {
        assert_eq!(initials("getTypeOfSymbol"), "gtos");
        assert_eq!(initials("get_type_of_symbol"), "gtos");
        assert_eq!(initials("GoToSelect"), "gts");
        assert_eq!(import_match("../../src/api/async/api", "packages/x/src/api/async/api.ts"), 4);
        assert_eq!(import_match("../../src/api/async/api", "packages/x/src/api/sync/api.ts"), 1);
        assert_eq!(import_match("./util.js", "src/util/index.ts"), 1);
        assert_eq!(import_match("crate::db::put_file", "src/db.rs"), 1);
        assert_eq!(import_match("example.com/x/binder", "tsc/internal/binder/binder.go"), 1);
        assert_eq!(import_match("fmt", "src/db.rs"), 0);
        assert_eq!(import_match("./api.testUtils.ts", "packages/x/src/api/sync/api.ts"), 0);
        assert_eq!(import_match("@x/unstable/async", "packages/x/src/api/async/api.ts"), 1);
        assert_eq!(split_qualified("Db::open"), (Some("Db"), "open"));
        assert_eq!(split_qualified("a.b.C.m"), (Some("C"), "m"));
        assert_eq!(split_qualified("works for a.b"), (None, "works for a.b"));
    }

    /// The ripgrep case that name-only matching got wrong: two `search_path`s plus an
    /// external-looking one, told apart by call syntax.
    #[test]
    fn resolution() {
        let root = std::env::temp_dir().join(format!("sym-resolve-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("src/search.rs"),
            "struct Worker;\nimpl Worker {\n    fn search_path(&self) {\n        search_path();\n    }\n    fn run(&self, searcher: Searcher) {\n        self.search_path();\n        searcher.search_path();\n        opener.open(); Vec::new();\n    }\n}\nfn search_path() {}\n",
        )
        .unwrap();
        fs::write(root.join("src/searcher.rs"), "pub struct Searcher;\nimpl Searcher {\n    pub fn search_path(&self) {} fn new() {}\n}\n")
            .unwrap();
        fs::write(root.join("src/io.rs"), "pub fn open() {}\npub fn open_all() { open(); }\n").unwrap();
        fs::write(root.join("src/fs.rs"), "pub fn open() {}\n").unwrap();
        fs::write(root.join("o.ts"), "function f(a: number): void;\nfunction f(a: string): void;\nfunction f(a: any) {\n}\nf(1);\n")
            .unwrap();
        let mut conn = db::open(&root).unwrap();
        index::index(&root, &mut conn).unwrap();

        // TS overloads resolve as one callable; `def` still lists each signature.
        assert_eq!(callers(&conn, "f", 50).unwrap(), "o.ts:3-4 function function f(a: any)\n  o.ts <top level> (5)\n");
        assert_eq!(def(&conn, "f").unwrap().lines().count(), 3);

        assert_eq!(
            callers(&conn, "search_path", 50).unwrap(),
            "src/search.rs:3-5 method in Worker: fn search_path(&self)\n  src/search.rs:6 run (7)\n\
             src/search.rs:12-12 function fn search_path()\n  src/search.rs:3 search_path (4)\n\
             src/searcher.rs:3-3 method in Searcher: pub fn search_path(&self)\n  src/search.rs:6 run (8)\n"
        );
        // Qualified query: only that member's callers.
        assert_eq!(
            callers(&conn, "Searcher.search_path", 50).unwrap(),
            "src/searcher.rs:3-3 method in Searcher: pub fn search_path(&self)\n  src/search.rs:6 run (8)\n"
        );
        // Bare `open()` in io.rs is io's own; `opener.open()` can't pick between io and fs.
        assert_eq!(
            callers(&conn, "open", 50).unwrap(),
            "src/fs.rs:1-1 function pub fn open()\n  src/search.rs:6 run (9?)\n\
             src/io.rs:1-1 function pub fn open()\n  src/io.rs:2 open_all (2)\n  src/search.rs:6 run (9?)\n"
        );
        assert_eq!(
            callees(&conn, "Worker.run", 50).unwrap(),
            "src/search.rs:6-10 method in Worker: fn run(&self, searcher: Searcher)\n  search_path src/search.rs:3\n  \
             search_path src/searcher.rs:3\n  open? src/fs.rs:1, src/io.rs:1\n  external: new\n"
        );

        drop(conn);
        fs::remove_dir_all(&root).unwrap();
    }
}
