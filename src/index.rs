use std::fs::Metadata;
use std::path::Path;
use std::sync::mpsc;
use std::thread;
use std::time::UNIX_EPOCH;

use anyhow::Result;
use ignore::WalkState;
use rayon::prelude::*;
use rusqlite::Connection;

use crate::{db, lang};

/// Memory guard only; real sources reach several MiB (TypeScript's checker.ts is 3 MiB).
const MAX_FILE_BYTES: u64 = 16 << 20;
/// Parsed files buffered ahead of the SQLite writer.
const CHANNEL: usize = 256;

#[derive(Debug, Default, PartialEq)]
pub struct Stats {
    pub parsed: usize,
    pub unchanged: usize,
    pub removed: usize,
    pub failed: usize,
}

fn mtime(meta: &Metadata) -> i64 {
    meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_nanos() as i64)
}

/// Bring the index up to date with `root`: parse new/changed files, drop deleted ones.
/// Change detection is `(mtime, size)`; everything happens in one transaction.
pub fn index(root: &Path, conn: &mut Connection) -> Result<Stats> {
    let mut known = db::known_files(conn)?;
    let mut stats = Stats::default();
    let mut todo = Vec::new();

    for (rel, stamp) in walk(root) {
        match known.remove(&rel) {
            Some(old) if old == stamp => stats.unchanged += 1,
            _ => todo.push((rel, stamp)),
        }
    }

    let tx = conn.transaction()?;
    // Whatever is left in `known` was not seen on disk (deleted, now ignored, or too big).
    for path in known.keys() {
        db::remove_file(&tx, path)?;
    }
    stats.removed = known.len();

    // Parse on the rayon pool while this thread writes; the bounded channel caps memory.
    let (send, recv) = mpsc::sync_channel(CHANNEL);
    thread::scope(|s| -> Result<()> {
        s.spawn(|| {
            todo.par_iter().for_each_with(send, |send, (rel, stamp)| {
                let res = std::fs::read(root.join(rel)).map_err(Into::into).and_then(|src| lang::parse(Path::new(rel), &src));
                let _ = send.send((rel, stamp, res)); // receiver gone = writer failed; just drain
            })
        });
        for (rel, &(mtime, size), res) in recv {
            match res {
                Ok(Some(p)) => {
                    db::put_file(&tx, rel, mtime, size, &p)?;
                    stats.parsed += 1;
                }
                Ok(None) => {}
                // Old rows (if any) stay; the stale stamp makes the next run retry.
                Err(e) => {
                    eprintln!("sym: {rel}: {e}");
                    stats.failed += 1;
                }
            }
        }
        Ok(())
    })?;
    tx.commit()?;
    Ok(stats)
}

/// Supported, not-ignored source files under `root` as (relative path, (mtime, size)).
fn walk(root: &Path) -> Vec<(String, (i64, i64))> {
    let (send, recv) = mpsc::channel();
    // At a repo root, ignore files above it don't apply (git semantics), and checking every
    // ancestor level per entry cost ~50 ms of a 230 ms refresh on TypeScript.
    let parents = !root.join(".git").exists();
    ignore::WalkBuilder::new(root).require_git(false).parents(parents).build_parallel().run(|| {
        let send = send.clone();
        Box::new(move |entry| {
            let Ok(entry) = entry.inspect_err(|e| eprintln!("sym: {e}")) else { return WalkState::Continue };
            let path = entry.path();
            if entry.file_type().is_some_and(|t| t.is_file())
                && lang::supported(path)
                && let Ok(meta) = entry.metadata()
                && meta.len() <= MAX_FILE_BYTES
                && let Ok(rel) = path.strip_prefix(root)
            {
                let rel = rel.to_string_lossy().replace('\\', "/");
                let _ = send.send((rel, (mtime(&meta), meta.len() as i64)));
            }
            WalkState::Continue
        })
    });
    drop(send);
    recv.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn count(conn: &Connection, sql: &str) -> i64 {
        conn.query_row(sql, [], |r| r.get(0)).unwrap()
    }

    #[test]
    fn incremental() {
        let root = std::env::temp_dir().join(format!("sym-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/a.rs"), "fn a() { b(); }\n").unwrap();
        fs::write(root.join("b.py"), "import os\ndef b():\n    pass\n").unwrap();
        fs::write(root.join("notes.txt"), "not code").unwrap();
        fs::write(root.join(".gitignore"), "gen/\n").unwrap();
        fs::create_dir_all(root.join("gen")).unwrap();
        fs::write(root.join("gen/x.rs"), "fn ignored() {}\n").unwrap();
        fs::write(root.join("app.min.js"), "function q(){r()}".repeat(400)).unwrap();

        let mut conn = db::open(&root).unwrap();
        let s = |parsed, unchanged, removed| Stats { parsed, unchanged, removed, failed: 0 };

        assert_eq!(index(&root, &mut conn).unwrap(), s(3, 0, 0));
        // Minified file: only names in its first 1000 columns (see lang::MAX_NAME_COLUMN).
        assert_eq!(count(&conn, "SELECT count(*) FROM symbols"), 2 + 59);
        assert_eq!(count(&conn, "SELECT count(*) FROM refs WHERE name = 'b'"), 1);
        assert_eq!(count(&conn, "SELECT count(*) FROM imports WHERE module = 'os'"), 1);
        assert_eq!(count(&conn, "SELECT count(*) FROM files WHERE path = 'src/a.rs'"), 1);

        // Nothing changed: nothing re-parsed.
        assert_eq!(index(&root, &mut conn).unwrap(), s(0, 3, 0));

        // Edit one file (size changes, so mtime granularity can't hide it), delete the other.
        fs::write(root.join("src/a.rs"), "fn a2() {}\nfn a3() {}\n").unwrap();
        fs::remove_file(root.join("b.py")).unwrap();
        assert_eq!(index(&root, &mut conn).unwrap(), s(1, 1, 1));
        assert_eq!(count(&conn, "SELECT count(*) FROM symbols"), 2 + 59);
        assert_eq!(count(&conn, "SELECT count(*) FROM symbols WHERE name = 'a'"), 0);
        assert_eq!(count(&conn, "SELECT count(*) FROM refs WHERE name != 'r'"), 0);
        assert_eq!(count(&conn, "SELECT count(*) FROM imports"), 0);

        // Reopening keeps the index (schema version matches).
        drop(conn);
        let mut conn = db::open(&root).unwrap();
        assert_eq!(index(&root, &mut conn).unwrap(), s(0, 2, 0));

        drop(conn);
        fs::remove_dir_all(&root).unwrap();
    }
}
