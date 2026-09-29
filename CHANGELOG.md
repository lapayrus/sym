# Changelog

User-visible changes, newest first. Format: [Keep a Changelog](https://keepachangelog.com/en/1.1.0/);
versions follow [Semantic Versioning](https://semver.org/). Add a line under **Unreleased** in the
same PR as the change.

## [Unreleased]

## [1.0.0] - 2026-09-29

First public release.

### Added

- Tree-sitter symbol index for Rust, Python, JavaScript/JSX, TypeScript/TSX and Go, stored in
  SQLite at `.sym/index.db`: definitions with signatures, line spans and parents, references
  (calls, `new X`, type usages) and imports.
- CLI: `sym index`, `def`, `refs`, `calls [--callees]`, `search` (fuzzy, camelCase initials) and
  `outline`. Every command refreshes the index incrementally before answering.
- `sym serve`: MCP server on stdio with `find_def`, `find_refs`, `calls`, `search` and `outline`.
  A file watcher re-indexes changed files in the background, so edits show up in the next query.
- Syntactic reference resolution: receivers (`self.f()`, `Type::f()`, `searcher.f()` → `Searcher`),
  imports, then same file and directory. References that could mean several definitions are
  listed once under `? one of path:line, ...`.
- `.gitignore`-aware parallel indexing; changed files are detected by `(mtime, size)`.
- `bench/bench.py`: timing and token comparison against ripgrep.

[Unreleased]: https://github.com/lapayrus/sym/compare/v1.0.0...HEAD
[1.0.0]: https://github.com/lapayrus/sym/releases/tag/v1.0.0
