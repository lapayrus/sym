//! `sym serve`: MCP server over stdio. Newline-delimited JSON-RPC 2.0, synchronous,
//! one request at a time. Each tool call refreshes the index, then wraps a `query` fn.

use std::io::{BufRead, Write};
use std::path::Path;

use anyhow::{Result, anyhow, bail};
use rusqlite::Connection;
use serde_json::{Value, json};

use crate::{db, index, query};

pub fn serve(root: &Path) -> Result<()> {
    let mut conn = db::open(root)?;
    index::index(root, &mut conn)?;
    let mut out = std::io::stdout().lock();
    for line in std::io::stdin().lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if let Some(resp) = handle(root, &mut conn, &line) {
            writeln!(out, "{resp}")?;
            out.flush()?;
        }
    }
    Ok(())
}

/// One JSON-RPC message in, at most one response out (notifications get none).
fn handle(root: &Path, conn: &mut Connection, line: &str) -> Option<Value> {
    let msg: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => return Some(json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": e.to_string()}})),
    };
    let id = msg.get("id")?.clone();
    let params = &msg["params"];
    let result = match msg["method"].as_str().unwrap_or("") {
        "initialize" => Ok(json!({
            // ponytail: echoes the client's version; pin a list once a client needs a newer protocol feature
            "protocolVersion": params["protocolVersion"].as_str().unwrap_or("2025-06-18"),
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "sym", "version": env!("CARGO_PKG_VERSION")},
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({"tools": tools()})),
        "tools/call" => call(root, conn, params),
        m => Err((-32601, format!("method not found: {m}"))),
    };
    Some(match result {
        Ok(r) => json!({"jsonrpc": "2.0", "id": id, "result": r}),
        Err((code, message)) => json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}),
    })
}

fn tools() -> Value {
    let name = json!({"type": "string", "description": "Symbol name; `Parent.name` or `Parent::name` picks one member"});
    let limit = json!({"type": "integer", "description": "Max hits to show"});
    json!([
        {
            "name": "find_def",
            "description": "Where a symbol is defined: `path:line-end kind [in Parent:] signature`, one per line.",
            "inputSchema": {"type": "object", "properties": {"name": name}, "required": ["name"]},
        },
        {
            "name": "find_refs",
            "description": "Where a symbol is referenced, grouped by resolved definition then file, with the enclosing definition of each site. `?` marks an ambiguous target.",
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

fn call(root: &Path, conn: &mut Connection, params: &Value) -> Result<Value, (i64, String)> {
    let name = params["name"].as_str().unwrap_or("");
    if !tools().as_array().unwrap().iter().any(|t| t["name"] == name) {
        return Err((-32602, format!("unknown tool: {name}")));
    }
    // Tool failures go back as content with isError, so the agent can read them.
    let (text, is_error) = match run(root, conn, name, &params["arguments"]) {
        Ok(t) => (t, false),
        Err(e) => (format!("{e:#}"), true),
    };
    Ok(json!({"content": [{"type": "text", "text": text}], "isError": is_error}))
}

fn run(root: &Path, conn: &mut Connection, tool: &str, args: &Value) -> Result<String> {
    let arg = |k: &str| args[k].as_str().ok_or_else(|| anyhow!("missing string argument `{k}`"));
    let limit = |d: usize| args["limit"].as_u64().map_or(d, |n| n as usize);
    // ponytail: O(files) refresh per call (10 ms ripgrep, 175 ms TypeScript); watch mode (phase 5) replaces it
    index::index(root, conn)?;
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
        index::index(&root, &mut conn).unwrap();
        let mut rpc = |s: &str| handle(&root, &mut conn, s);

        let init = rpc(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26"}}"#).unwrap();
        assert_eq!(init["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(rpc(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#), None);
        let list = rpc(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#).unwrap();
        assert_eq!(list["result"]["tools"].as_array().unwrap().len(), 5);

        let text = |v: Value| v["result"]["content"][0]["text"].as_str().unwrap().to_string();
        let def = rpc(r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"find_def","arguments":{"name":"helper"}}}"#);
        assert_eq!(text(def.unwrap()), "a.rs:1-1 function fn helper()\n");
        let calls = rpc(r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"calls","arguments":{"name":"run","direction":"callees"}}}"#);
        assert!(text(calls.unwrap()).contains("helper"));

        let missing = rpc(r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"find_def","arguments":{}}}"#).unwrap();
        assert_eq!(missing["result"]["isError"], true);
        let unknown = rpc(r#"{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"nope"}}"#).unwrap();
        assert_eq!(unknown["error"]["code"], -32602);
        assert_eq!(rpc("{bad").unwrap()["error"]["code"], -32700);
        assert_eq!(rpc(r#"{"jsonrpc":"2.0","id":7,"method":"x"}"#).unwrap()["error"]["code"], -32601);
    }
}
