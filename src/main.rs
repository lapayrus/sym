mod lang;

use std::path::Path;

// ponytail: phase-1 debug entry (`sym <file>` dumps its tags); clap CLI replaces this in phase 3
fn main() -> anyhow::Result<()> {
    let arg = std::env::args().nth(1).ok_or_else(|| anyhow::anyhow!("usage: sym <file>"))?;
    let path = Path::new(&arg);
    match lang::parse(path, &std::fs::read(path)?)? {
        Some(p) => println!("{p:#?}"),
        None => eprintln!("unsupported file type"),
    }
    Ok(())
}
