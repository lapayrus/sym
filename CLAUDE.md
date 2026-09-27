# sym

A tree-sitter symbol index for AI agents (CLI + MCP server), written in Rust.

## Start of every session
Read `docs/PROJECT.md` before doing anything. It holds the mission, tech decisions,
roadmap/phase status, current state and known limits.

## Keep docs/PROJECT.md current
Update it in the same change as the code whenever:
- a phase starts or finishes (Roadmap status + Current state)
- a tech decision is made or reversed (Tech decisions table, including "revisit when")
- a known limit or `ponytail:` shortcut is added or removed (Known limits)
- anything else a fresh session would need to continue the work

Add a dated one-line entry to its Changelog for each update.

## Conventions
- `cargo test` must pass before committing.
- Keep it small: few files, no speculative abstractions; mark deliberate shortcuts with `// ponytail: <ceiling>, <upgrade path>`.
