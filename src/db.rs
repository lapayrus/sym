use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;
use rusqlite::{Connection, Transaction, params};

use crate::lang::Parsed;

/// Bump on any schema change: an index with another version is dropped and rebuilt.
const VERSION: i32 = 2;

const SCHEMA: &str = "
CREATE TABLE files(
    id    INTEGER PRIMARY KEY,
    path  TEXT NOT NULL UNIQUE,
    mtime INTEGER NOT NULL,
    size  INTEGER NOT NULL
);
CREATE TABLE symbols(
    file_id  INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    name     TEXT NOT NULL,
    kind     TEXT NOT NULL,
    line     INTEGER NOT NULL,
    end_line INTEGER NOT NULL,
    sig      TEXT NOT NULL,
    parent   TEXT -- enclosing class/interface/impl, NULL for free definitions
);
CREATE TABLE refs(
    file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    name    TEXT NOT NULL,
    kind    TEXT NOT NULL,
    line    INTEGER NOT NULL,
    qual    TEXT -- receiver/qualifier (`x` in `x.f()`), NULL for a bare name
);
CREATE TABLE imports(
    file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    module  TEXT NOT NULL
);
CREATE INDEX symbols_name ON symbols(name);
CREATE INDEX symbols_file ON symbols(file_id, line);
CREATE INDEX refs_name ON refs(name);
CREATE INDEX refs_file ON refs(file_id, line);
CREATE INDEX imports_module ON imports(module);
CREATE INDEX imports_file ON imports(file_id);
";

/// Open (creating if needed) `<root>/.sym/index.db`.
pub fn open(root: &Path) -> Result<Connection> {
    let dir = root.join(".sym");
    std::fs::create_dir_all(&dir)?;
    let conn = Connection::open(dir.join("index.db"))?;
    conn.execute_batch(
        "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA foreign_keys=ON;
         PRAGMA temp_store=MEMORY; PRAGMA mmap_size=268435456;",
    )?;
    let version: i32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version != VERSION {
        conn.execute_batch(&format!(
            "BEGIN;
             DROP TABLE IF EXISTS imports; DROP TABLE IF EXISTS refs;
             DROP TABLE IF EXISTS symbols; DROP TABLE IF EXISTS files;
             {SCHEMA}
             PRAGMA user_version={VERSION};
             COMMIT;"
        ))?;
    }
    Ok(conn)
}

/// path -> (mtime, size) for every indexed file.
pub fn known_files(conn: &Connection) -> Result<HashMap<String, (i64, i64)>> {
    let mut stmt = conn.prepare("SELECT path, mtime, size FROM files")?;
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, (r.get(1)?, r.get(2)?))))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Like `known_files`, but only paths in `[prefix, prefix + '0')`: `prefix` itself, files under
/// `prefix/`, and siblings such as `prefix.rs` (`-`, `.` sort before `/`) that the caller filters out.
pub fn known_under(conn: &Connection, prefix: &str) -> Result<HashMap<String, (i64, i64)>> {
    let mut stmt = conn.prepare_cached("SELECT path, mtime, size FROM files WHERE path >= ?1 AND path < ?1 || '0'")?;
    let rows = stmt.query_map([prefix], |r| Ok((r.get(0)?, (r.get(1)?, r.get(2)?))))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn remove_file(tx: &Transaction, path: &str) -> Result<()> {
    tx.prepare_cached("DELETE FROM files WHERE path = ?1")?.execute([path])?;
    Ok(())
}

/// Replace everything stored for `path` with `p`.
pub fn put_file(tx: &Transaction, path: &str, mtime: i64, size: i64, p: &Parsed) -> Result<()> {
    remove_file(tx, path)?;
    tx.prepare_cached("INSERT INTO files(path, mtime, size) VALUES (?1, ?2, ?3)")?
        .execute(params![path, mtime, size])?;
    let id = tx.last_insert_rowid();
    let mut s = tx.prepare_cached("INSERT INTO symbols VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)")?;
    for d in &p.defs {
        s.execute(params![id, d.name, d.kind, d.line, d.end_line, d.sig, d.parent])?;
    }
    let mut s = tx.prepare_cached("INSERT INTO refs VALUES (?1, ?2, ?3, ?4, ?5)")?;
    for r in &p.refs {
        s.execute(params![id, r.name, r.kind, r.line, r.qual])?;
    }
    let mut s = tx.prepare_cached("INSERT INTO imports VALUES (?1, ?2)")?;
    for m in &p.imports {
        s.execute(params![id, m])?;
    }
    Ok(())
}
