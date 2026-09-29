//! `sym serve`: MCP server over stdio. Newline-delimited JSON-RPC 2.0, synchronous,
//! one request at a time. A file watcher records changed paths and a background thread
//! re-indexes just those; each tool call first applies anything still pending, then wraps a `query` fn.

use std::collections::HashSet;
use std::io::{BufRead, Write};
use std::path::Path;
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use notify::{EventKind, RecursiveMode, Watcher};
use rusqlite::Connection;
use serde_json::{Value, json};

use crate::{db, index, lang, query};

/// Repo-relative paths changed since the last refresh; `None` = watcher lost track, rescan everything.
type Changed = Arc<Mutex<Option<HashSet<String>>>>;

/// Sent in `initialize`; clients show it to the model as guidance for when to use these tools.
const INSTRUCTIONS: &str = "sym is a symbol index of this repo (Rust, Python, JS/TS, Go), kept up to date with the \
files on disk. Prefer it over grep and over reading whole files to find where a symbol is defined (find_def), \
where it is used (find_refs), who calls a function or what it calls (calls), a symbol from part of its name \
(search), and what a file contains (outline). Results give `path:line-end` spans: read just those lines. \
Use grep for text that isn't a symbol (strings, comments, config).";

/// Quiet time before the background thread re-indexes, so a burst (save, `git checkout`) is one update.
const SETTLE: Duration = Duration::from_millis(20);

pub fn serve(root: &Path) -> Result<()> {
    let changed: Changed = Arc::new(Mutex::new(Some(HashSet::new())));
    let (wake, woken) = channel();
    // Watch before the first index so no edit falls in between.
    let _watcher = watch(root, changed.clone(), wake)?;
    let conn = Arc::new(Mutex::new(db::open(root)?));
    index::index(root, &mut conn.lock().unwrap(), None)?;

    // Re-index as edits land, so the parse cost (~650 ms for a 1.4 MiB file) is usually paid before
    // the agent's next tool call; a call arriving mid-update waits on the lock and still sees it.
    let (bg_root, bg_conn, bg_changed) = (root.to_path_buf(), conn.clone(), changed.clone());
    std::thread::spawn(move || {
        while woken.recv().is_ok() {
            while woken.recv_timeout(SETTLE).is_ok() {}
            if let Err(e) = refresh(&bg_root, &mut bg_conn.lock().unwrap(), &bg_changed) {
                eprintln!("sym: {e:#}");
            }
        }
    });

    let mut out = std::io::stdout().lock();
    for line in std::io::stdin().lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if let Some(resp) = handle(root, &mut conn.lock().unwrap(), &changed, &line) {
            writeln!(out, "{resp}")?;
            out.flush()?;
        }
    }
    Ok(())
}

/// Record OS file events in `changed` and poke `wake` when something was recorded.
fn watch(root: &Path, changed: Changed, wake: Sender<()>) -> Result<notify::RecommendedWatcher> {
    let base = root.to_path_buf();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        let mut c = changed.lock().unwrap();
        let ev = match res {
            Ok(ev) if !ev.need_rescan() => ev,
            _ => {
                *c = None; // event overflow or watcher error
                let _ = wake.send(());
                return;
            }
        };
        if matches!(ev.kind, EventKind::Access(_)) {
            return; // our own reads
        }
        for p in &ev.paths {
            let Some(rel) = index::rel(&base, p) else { continue };
            if matches!(rel.split('/').next(), Some(".sym" | ".git")) {
                continue; // our own db writes, git internals
            }
            let rel = if matches!(p.file_name().and_then(|n| n.to_str()), Some(".gitignore" | ".ignore")) {
                // An ignore-file edit can (un)ignore anything below it.
                rel.rsplit_once('/').map_or("", |(dir, _)| dir).to_string()
            } else if p.is_file() && !lang::supported(p) {
                continue; // build output etc.; keeps the set small
            } else {
                rel
            };
            match c.as_mut() {
                Some(set) if !rel.is_empty() => _ = set.insert(rel),
                _ => *c = None,
            }
            let _ = wake.send(()); // receiver gone = shutting down
        }
    })?;
    watcher.watch(root, RecursiveMode::Recursive)?;
    Ok(watcher)
}

/// Re-index what changed since the last call (the whole tree if the watcher lost track).
/// ponytail: an event the OS hasn't delivered when a query runs is picked up by the next call (latency is ~ms); fine for agents
fn refresh(root: &Path, conn: &mut Connection, changed: &Changed) -> Result<()> {
    let taken = changed.lock().unwrap().replace(HashSet::new());
    let res = match &taken {
        Some(set) if set.is_empty() => return Ok(()),
        scope => index::index(root, conn, scope.as_ref()),
    };
    if res.is_err() {
        *changed.lock().unwrap() = None; // don't lose the changes: rescan next time
    }
    res.map(drop)
}

/// One JSON-RPC message in, at most one response out (notifications get none).
fn handle(root: &Path, conn: &mut Connection, changed: &Changed, line: &str) -> Option<Value> {
    let msg: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => {
            return Some(json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": e.to_string()}}));
        }
    };
    let id = msg.get("id")?.clone();
    let params = &msg["params"];
    let result = match msg["method"].as_str().unwrap_or("") {
        "initialize" => Ok(json!({
            // ponytail: echoes the client's version; pin a list once a client needs a newer protocol feature
            "protocolVersion": params["protocolVersion"].as_str().unwrap_or("2025-06-18"),
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "sym", "version": env!("CARGO_PKG_VERSION")},
            "instructions": INSTRUCTIONS,
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({"tools": tools()})),
        "tools/call" => call(root, conn, changed, params),
        m => Err((-32601, format!("method not found: {m}"))),
    };
    Some(match result {
        Ok(r) => json!({"jsonrpc": "2.0", "id": id, "result": r}),
        Err((code, message)) => json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}),
    })
}

fn tools() -> Value {
    let name =
        json!({"type": "string", "description": "Symbol name; `Parent.name` or `Parent::name` picks one member"});
    let limit = json!({"type": "integer", "description": "Max hits to show"});
    json!([
        {
            "name": "find_def",
            "description": "Where a symbol is defined: `path:line-end kind [in Parent:] signature`, one per line.",
            "inputSchema": {"type": "object", "properties": {"name": name}, "required": ["name"]},
        },
        {
            "name": "find_refs",
            "description": "Where a symbol is referenced, grouped by resolved definition then file, with the enclosing definition of each site. A `? one of path:line, ...` group holds references that could mean several definitions.",
            "inputSchema": {"type": "object", "properties": {"name": name, "limit": limit}, "required": ["name"]},
        },
        {
            "name": "search",
            "description": "Fuzzy-find definitions by name (subsequence, camelCase initials like `gtos` → getTypeOfSymbol).",
            "inputSchema": {"type": "object", "properties": {"query": {"type": "string"}, "limit": limit}, "required": ["query"]},
        },
        {
            "name": "calls",
            "description": "Call graph for a function: who calls it (callers, default) or what it calls (callees).",
            "inputSchema": {"type": "object", "properties": {
                "name": name,
                "direction": {"type": "string", "enum": ["callers", "callees"]},
                "limit": limit,
            }, "required": ["name"]},
        },
        {
            "name": "outline",
            "description": "All definitions in a file, nested, with line spans and signatures. `path` is repo-relative or a unique suffix like `db.rs`.",
            "inputSchema": {"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]},
        },
    ])
}

fn call(root: &Path, conn: &mut Connection, changed: &Changed, params: &Value) -> Result<Value, (i64, String)> {
    let name = params["name"].as_str().unwrap_or("");
    if !tools().as_array().unwrap().iter().any(|t| t["name"] == name) {
        return Err((-32602, format!("unknown tool: {name}")));
    }
    // Tool failures go back as content with isError, so the agent can read them.
    let (text, is_error) = match run(root, conn, changed, name, &params["arguments"]) {
        Ok(t) => (t, false),
        Err(e) => (format!("{e:#}"), true),
    };
    Ok(json!({"content": [{"type": "text", "text": text}], "isError": is_error}))
}

fn run(root: &Path, conn: &mut Connection, changed: &Changed, tool: &str, args: &Value) -> Result<String> {
    let arg = |k: &str| args[k].as_str().ok_or_else(|| anyhow!("missing string argument `{k}`"));
    let limit = |d: usize| args["limit"].as_u64().map_or(d, |n| n as usize);
    refresh(root, conn, changed)?;
    match tool {
        "find_def" => query::def(conn, arg("name")?),
        "find_refs" => query::refs(conn, arg("name")?, limit(50)),
        "search" => query::search(conn, arg("query")?, limit(20)),
        "calls" => match args["direction"].as_str().unwrap_or("callers") {
            "callers" => query::callers(conn, arg("name")?, limit(50)),
            "callees" => query::callees(conn, arg("name")?, limit(50)),
            d => bail!("direction must be `callers` or `callees`, got `{d}`"),
        },
        "outline" => query::outline(conn, &crate::rel_path(root, arg("path")?)),
        _ => unreachable!("checked in call"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn protocol() {
        let root = std::env::temp_dir().join(format!("sym-mcp-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("a.rs"), "fn helper() {}\nfn run() { helper(); }\n").unwrap();
        let mut conn = db::open(&root).unwrap();
        index::index(&root, &mut conn, None).unwrap();
        let changed: Changed = Arc::new(Mutex::new(Some(HashSet::new())));
        let mut rpc = |s: &str| handle(&root, &mut conn, &changed, s);

        let init =
            rpc(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26"}}"#).unwrap();
        assert_eq!(init["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(rpc(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#), None);
        let list = rpc(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#).unwrap();
        assert_eq!(list["result"]["tools"].as_array().unwrap().len(), 5);

        let text = |v: Value| v["result"]["content"][0]["text"].as_str().unwrap().to_string();
        let def = rpc(
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"find_def","arguments":{"name":"helper"}}}"#,
        );
        assert_eq!(text(def.unwrap()), "a.rs:1-1 function fn helper()\n");
        let calls = rpc(
            r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"calls","arguments":{"name":"run","direction":"callees"}}}"#,
        );
        assert!(text(calls.unwrap()).contains("helper"));

        let missing =
            rpc(r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"find_def","arguments":{}}}"#)
                .unwrap();
        assert_eq!(missing["result"]["isError"], true);
        let unknown = rpc(r#"{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"nope"}}"#).unwrap();
        assert_eq!(unknown["error"]["code"], -32602);
        assert_eq!(rpc("{bad").unwrap()["error"]["code"], -32700);
        assert_eq!(rpc(r#"{"jsonrpc":"2.0","id":7,"method":"x"}"#).unwrap()["error"]["code"], -32601);
        drop(conn);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn watch_picks_up_edits() {
        let root = std::env::temp_dir().join(format!("sym-watch-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/a.rs"), "fn old() {}\n").unwrap();
        let changed: Changed = Arc::new(Mutex::new(Some(HashSet::new())));
        let (wake, woken) = channel();
        let _w = watch(&root, changed.clone(), wake).unwrap();
        let mut conn = db::open(&root).unwrap();
        index::index(&root, &mut conn, None).unwrap();
        refresh(&root, &mut conn, &changed).unwrap(); // drain events from the initial writes

        let edit = |f: &dyn Fn()| {
            let t = std::time::Instant::now();
            f();
            while changed.lock().unwrap().as_ref().is_some_and(|s| s.is_empty()) {
                assert!(t.elapsed().as_secs() < 5, "no watch event");
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        };
        edit(&|| fs::write(root.join("src/a.rs"), "fn fresh() {}\n").unwrap());
        assert!(woken.try_recv().is_ok(), "recorded events wake the background thread");
        refresh(&root, &mut conn, &changed).unwrap();
        assert_eq!(query::def(&conn, "fresh").unwrap(), "src/a.rs:1-1 function fn fresh()\n");
        assert!(query::def(&conn, "old").unwrap().starts_with("no definition"));

        // A deleted directory takes its files with it.
        edit(&|| fs::remove_dir_all(root.join("src")).unwrap());
        refresh(&root, &mut conn, &changed).unwrap();
        assert!(query::def(&conn, "fresh").unwrap().starts_with("no definition"));

        drop(conn);
        let _ = fs::remove_dir_all(&root);
    }
}
