//! Read-side queries. Each returns ready-to-print, line-oriented text grouped by file,
//! shared by the CLI and (phase 4) the MCP server.

use std::fmt::Write;

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};

/// Escape `\ % _` for `LIKE ... ESCAPE '\'`.
fn like_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
}

/// Innermost definition in the file whose line span contains `line`.
fn enclosing(conn: &Connection, file_id: i64, line: i64) -> Result<Option<(String, i64)>> {
    Ok(conn
        .prepare_cached(
            "SELECT name, line FROM symbols WHERE file_id = ?1 AND line <= ?2 AND end_line >= ?2
             ORDER BY line DESC LIMIT 1",
        )?
        .query_row(params![file_id, line], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?)
}

/// Append `path:line-end kind sig` for every definition of `name`, at most `limit` lines.
fn defs_into(conn: &Connection, name: &str, limit: usize, out: &mut String) -> Result<usize> {
    let mut stmt = conn.prepare_cached(
        "SELECT f.path, s.line, s.end_line, s.kind, s.sig FROM symbols s JOIN files f ON f.id = s.file_id
         WHERE s.name = ?1 ORDER BY f.path, s.line LIMIT ?2",
    )?;
    let mut rows = stmt.query(params![name, limit as i64])?;
    let mut n = 0;
    while let Some(r) = rows.next()? {
        let (path, line, end, kind, sig): (String, i64, i64, String, String) =
            (r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?);
        writeln!(out, "{path}:{line}-{end} {kind} {sig}")?;
        n += 1;
    }
    Ok(n)
}

pub fn def(conn: &Connection, name: &str) -> Result<String> {
    let mut out = String::new();
    if defs_into(conn, name, usize::MAX >> 1, &mut out)? == 0 {
        writeln!(out, "no definition of `{name}`")?;
    }
    Ok(out)
}

/// Every reference to `name`, grouped by file, each with its enclosing definition.
pub fn refs(conn: &Connection, name: &str, limit: usize) -> Result<String> {
    let mut out = String::new();
    let total: i64 = conn.query_row("SELECT count(*) FROM refs WHERE name = ?1", [name], |r| r.get(0))?;
    let total = total as usize;
    if total == 0 {
        writeln!(out, "no references to `{name}`")?;
        return Ok(out);
    }
    let mut stmt = conn.prepare_cached(
        "SELECT r.file_id, f.path, r.line, r.kind FROM refs r JOIN files f ON f.id = r.file_id
         WHERE r.name = ?1 ORDER BY f.path, r.line LIMIT ?2",
    )?;
    let mut rows = stmt.query(params![name, limit as i64])?;
    let mut last = String::new();
    while let Some(r) = rows.next()? {
        let (file_id, path, line, kind): (i64, String, i64, String) = (r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?);
        if path != last {
            writeln!(out, "{path}")?;
            last = path;
        }
        match enclosing(conn, file_id, line)? {
            Some((caller, _)) => writeln!(out, "  {line} {kind} in {caller}")?,
            None => writeln!(out, "  {line} {kind}")?,
        }
    }
    if total > limit {
        writeln!(out, "... {} more", total - limit)?;
    }
    Ok(out)
}

/// Definitions whose name contains `query` as a case-insensitive subsequence, best first:
/// exact, then prefix, then substring, then subsequence; shorter names first within a rank.
// ponytail: LIKE full scan of distinct names + 4-bucket rank; in-memory name list and
// nucleo-matcher if it gets slow or ranking feels off
pub fn search(conn: &Connection, query: &str, limit: usize) -> Result<String> {
    let pattern: String = query.chars().map(|c| format!("%{}", like_escape(&c.to_string()))).collect::<String>() + "%";
    let mut names: Vec<String> = conn
        .prepare_cached("SELECT DISTINCT name FROM symbols WHERE name LIKE ?1 ESCAPE '\\'")?
        .query_map([pattern], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let q = query.to_lowercase();
    names.sort_by_cached_key(|n| {
        let l = n.to_lowercase();
        let rank = if l == q { 0 } else if l.starts_with(&q) { 1 } else if l.contains(&q) { 2 } else { 3 };
        (rank, n.len(), n.clone())
    });
    let mut out = String::new();
    let mut left = limit;
    for name in &names {
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

/// Who calls `name`: one line per calling definition with its call-site lines.
pub fn callers(conn: &Connection, name: &str, limit: usize) -> Result<String> {
    let mut stmt = conn.prepare_cached(
        "SELECT r.file_id, f.path, r.line FROM refs r JOIN files f ON f.id = r.file_id
         WHERE r.name = ?1 AND r.kind = 'call' ORDER BY f.path, r.line",
    )?;
    let mut rows = stmt.query([name])?;
    // (path, caller, call lines); a caller's calls are contiguous because rows are line-ordered.
    let mut groups: Vec<(String, Option<(String, i64)>, Vec<i64>)> = Vec::new();
    while let Some(r) = rows.next()? {
        let (file_id, path, line): (i64, String, i64) = (r.get(0)?, r.get(1)?, r.get(2)?);
        let caller = enclosing(conn, file_id, line)?;
        match groups.last_mut() {
            Some((p, c, lines)) if *p == path && *c == caller => lines.push(line),
            _ => groups.push((path, caller, vec![line])),
        }
    }
    let mut out = String::new();
    if groups.is_empty() {
        writeln!(out, "no calls to `{name}`")?;
    }
    for (path, caller, lines) in groups.iter().take(limit) {
        let lines = lines.iter().map(i64::to_string).collect::<Vec<_>>().join(", ");
        match caller {
            Some((c, l)) => writeln!(out, "{path}:{l} {c} ({lines})")?,
            None => writeln!(out, "{path} <top level> ({lines})")?,
        }
    }
    if groups.len() > limit {
        writeln!(out, "... {} more callers", groups.len() - limit)?;
    }
    Ok(out)
}

/// What `name` calls: for each definition of it, the distinct call names in its span.
// ponytail: callee names are unresolved (includes std/external calls); resolve against
// symbols if agents need to jump straight to project defs
pub fn callees(conn: &Connection, name: &str, limit: usize) -> Result<String> {
    let mut stmt = conn.prepare_cached(
        "SELECT s.file_id, f.path, s.line, s.end_line FROM symbols s JOIN files f ON f.id = s.file_id
         WHERE s.name = ?1 ORDER BY f.path, s.line LIMIT ?2",
    )?;
    let defs: Vec<(i64, String, i64, i64)> = stmt
        .query_map(params![name, limit as i64], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let mut out = String::new();
    if defs.is_empty() {
        writeln!(out, "no definition of `{name}`")?;
    }
    let mut calls = conn.prepare_cached(
        "SELECT name FROM refs WHERE file_id = ?1 AND line BETWEEN ?2 AND ?3 AND kind = 'call' ORDER BY line",
    )?;
    for (file_id, path, line, end) in defs {
        let mut names: Vec<String> = Vec::new();
        for n in calls.query_map(params![file_id, line, end], |r| r.get(0))? {
            let n = n?;
            if !names.contains(&n) {
                names.push(n);
            }
        }
        writeln!(out, "{path}:{line}-{end} {name}")?;
        writeln!(out, "  {}", if names.is_empty() { "(no calls)".into() } else { names.join(", ") })?;
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
            "fn helper() {}\nfn run() {\n    helper();\n    helper();\n}\nstruct S;\nimpl S {\n    fn go(&self) { helper(); run(); }\n}\n",
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
        assert_eq!(def(&conn, "nope").unwrap(), "no definition of `nope`\n");

        assert_eq!(refs(&conn, "helper", 2).unwrap(), "b.py\n  6 call in m\n  8 call\n... 3 more\n");

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
            "b.py:5 m (6)\nb.py <top level> (8)\nsrc/a.rs:2 run (3, 4)\nsrc/a.rs:8 go (8)\n"
        );
        assert_eq!(callers(&conn, "helper", 1).unwrap(), "b.py:5 m (6)\n... 3 more callers\n");
        assert_eq!(callees(&conn, "go", 50).unwrap(), "src/a.rs:8-8 go\n  helper, run\n");
        assert_eq!(callees(&conn, "helper", 50).unwrap(), "src/a.rs:1-1 helper\n  (no calls)\n");

        assert_eq!(
            outline(&conn, "a.rs").unwrap(),
            "src/a.rs\n  1-1 function fn helper()\n  2-5 function fn run()\n  6-6 class struct S\n  8-8 method fn go(&self)\n"
        );
        assert_eq!(
            outline(&conn, "b.py").unwrap(),
            "b.py\n  1-2 function def helper_two()\n  4-6 class class K\n    5-6 function def m(self)\n"
        );
        assert_eq!(outline(&conn, "x.rs").unwrap(), "not indexed: `x.rs`\n");

        drop(conn);
        fs::remove_dir_all(&root).unwrap();
    }
}
