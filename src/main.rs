mod db;
mod index;
mod lang;
mod mcp;
mod query;

use std::path::{Path, PathBuf};
use std::time::Instant;

use clap::{Parser, Subcommand};

/// Symbol index for AI coding agents: ripgrep for symbols.
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// Repo root [default: nearest ancestor containing .sym or .git, else the current dir]
    #[arg(long, global = true)]
    root: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Build or refresh the index and print stats
    Index,
    /// Where NAME is defined
    Def { name: String },
    /// Where NAME is referenced, grouped by file, with the enclosing definition
    Refs {
        name: String,
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// Fuzzy-find definitions by name
    Search {
        query: String,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Who calls NAME (or, with --callees, what NAME calls)
    Calls {
        name: String,
        #[arg(long)]
        callees: bool,
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// Definitions in a file, nested (PATH may be a suffix like `db.rs`)
    Outline { path: String },
    /// Run as an MCP server on stdio (`claude mcp add sym -- sym serve`)
    Serve,
}

fn find_root() -> PathBuf {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let found = cwd.ancestors().find(|d| d.join(".sym").is_dir() || d.join(".git").exists());
    found.unwrap_or(&cwd).to_path_buf()
}

/// `path` as the index stores it: relative to `root`, `/`-separated.
fn rel_path(root: &Path, path: &str) -> String {
    let rel = std::fs::canonicalize(path)
        .ok()
        .and_then(|p| Some(p.strip_prefix(std::fs::canonicalize(root).ok()?).ok()?.to_path_buf()));
    let s = rel.map_or_else(|| path.to_string(), |p| p.to_string_lossy().into_owned());
    s.replace('\\', "/").trim_start_matches("./").to_string()
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let root = cli.root.unwrap_or_else(find_root);
    if let Cmd::Serve = cli.cmd {
        return mcp::serve(&root);
    }
    let t = Instant::now();
    let mut conn = db::open(&root)?;
    // Every query refreshes first so answers are never stale.
    // ponytail: O(files) stat walk per CLI call (~14 ms ripgrep, ~310 ms TypeScript); `sym serve` (watch mode) avoids it
    let s = index::index(&root, &mut conn, None)?;
    let out = match cli.cmd {
        Cmd::Index => format!(
            "parsed {} unchanged {} removed {} failed {} in {:.1?}\n",
            s.parsed,
            s.unchanged,
            s.removed,
            s.failed,
            t.elapsed()
        ),
        Cmd::Def { name } => query::def(&conn, &name)?,
        Cmd::Refs { name, limit } => query::refs(&conn, &name, limit)?,
        Cmd::Search { query, limit } => query::search(&conn, &query, limit)?,
        Cmd::Calls { name, callees: false, limit } => query::callers(&conn, &name, limit)?,
        Cmd::Calls { name, callees: true, limit } => query::callees(&conn, &name, limit)?,
        Cmd::Outline { path } => query::outline(&conn, &rel_path(&root, &path))?,
        Cmd::Serve => unreachable!("handled above"),
    };
    print!("{out}");
    Ok(())
}
