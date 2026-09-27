mod db;
mod index;
mod lang;

use std::path::PathBuf;
use std::time::Instant;

// ponytail: hand-parsed args until the clap CLI lands in phase 3
fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("index") => {
            let root = args.next().map_or_else(|| PathBuf::from("."), PathBuf::from);
            let t = Instant::now();
            let mut conn = db::open(&root)?;
            let s = index::index(&root, &mut conn)?;
            println!(
                "parsed {} unchanged {} removed {} failed {} in {:.1?}",
                s.parsed, s.unchanged, s.removed, s.failed, t.elapsed()
            );
        }
        _ => anyhow::bail!("usage: sym index [root]"),
    }
    Ok(())
}
