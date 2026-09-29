# Contributing to sym

Thanks for helping. Bug reports, benchmark results from your repos, new languages and fixes are all
welcome.

## Before you start

- **Bugs:** open an issue using the bug template. Include `sym --version`, your OS, and the command
  (or MCP tool call) with its output.
- **Features and larger changes:** open an issue first, so we can agree on the approach before you
  write code. The project deliberately stays small (see "Design rules" below), and some ideas are
  better as a separate tool.
- **Security issues:** don't open a public issue. See [SECURITY.md](SECURITY.md).

## Development setup

You need Rust 1.90 or newer ([rustup](https://rustup.rs)). The C parts (tree-sitter grammars, bundled SQLite)
compile with the system C compiler that Rust already needs.

```sh
git clone https://github.com/lapayrus/sym && cd sym
cargo build
cargo test
cargo run -- --root /some/repo def main      # try it on any repo
```

To try your build as an MCP server in Claude Code, run `cargo build --release`, then
`claude mcp add sym-dev -- /abs/path/to/target/release/sym serve`.

## Where things live

| Path | What it does |
|---|---|
| `src/lang.rs` | languages, tree-sitter queries, extraction of definitions, references and imports |
| `src/db.rs` | SQLite schema and writes |
| `src/index.rs` | parallel walk, change detection, full and scoped (watch-mode) indexing |
| `src/query.rs` | `def`, `refs`, `calls`, `search`, `outline`, and syntactic reference resolution |
| `src/mcp.rs` | `sym serve`: stdio JSON-RPC, tools, file watcher |
| `src/main.rs` | the CLI |
| `docs/PROJECT.md` | design decisions (with "revisit when"), roadmap, known limits, dev log |
| `bench/bench.py` | benchmark against ripgrep |

Read `docs/PROJECT.md` before a non-trivial change. It explains why things are the way they are.

## Design rules

- **Keep it small.** Few files, no abstraction until a second use exists, and no dependency for something
  a few lines can do.
- **Mark deliberate shortcuts** with a comment `// ponytail: <limit>, <upgrade path>`. For example:
  `// ponytail: O(files) walk per CLI call; watch mode avoids it`. The limit and its fix then stay searchable.
- **Output is for agents.** Query output stays plain, line-oriented text: `path:line-end kind sig`,
  grouped by file, with as few tokens as possible. Before changing it, compare tokens and correctness
  with `bench/bench.py`.
- **Freshness is not optional.** No query may answer from a stale index.

## Making a change

1. Fork, then create a branch from `main`.
2. Make the change, with a test. Each module has its tests at the bottom. The smallest test that
   fails when the logic breaks is enough.
3. If you changed what gets extracted or stored (queries in `lang.rs`, the schema in `db.rs`),
   bump `VERSION` in `src/db.rs` so existing indexes rebuild.
4. Run the same checks as CI:

   ```sh
   cargo fmt
   cargo clippy --all-targets -- -D warnings
   cargo test
   ```

5. Update the docs in the same PR:
   - `CHANGELOG.md`: add a line under **Unreleased** for anything users will notice.
   - `docs/PROJECT.md`: update it if you made or reversed a design decision, added or removed a
     known limit, or changed the roadmap. Add a dated line to its Changelog.
   - `README.md`: update it if usage or output changed.
6. Open a PR against `main` and fill in the template. CI runs formatting, clippy, and tests on Linux,
   macOS and Windows, plus a build on the minimum Rust version.

**Commit messages** follow [Conventional Commits](https://www.conventionalcommits.org), for example
`feat(query): ...`, `fix(index): ...`, `perf: ...` or `docs: ...`. PRs are squash-merged, so the PR
title becomes the commit message; give it the same form.

### Adding a language

1. Add the grammar crate to `Cargo.toml`. Check it works with the `tree-sitter` version in use; the
   `all_queries_compile` test catches a mismatch.
2. Add an entry to `LANGS` in `src/lang.rs`: extensions, language, the grammar's `TAGS_QUERY`, and
   an `*_EXTRA` query for imports, type references, and anything the grammar's tags file misses.
3. If needed, teach `family()` which languages can call each other, and `import_match` the new file
   extension.
4. Add a test next to the existing per-language tests in `src/lang.rs`.
5. Bump `VERSION` in `src/db.rs`, and list the language in `README.md` and `CHANGELOG.md`.

### Benchmarks

```sh
cargo build --release
python bench/bench.py target/release/sym "$(command -v rg)" /path/to/repo \
  --symbol someFunction --file path/to/big_file.rs --edit path/to/mid_size_file.rs
```

Compare runs on the same machine in one session only: background load and antivirus scans can double the timings.

## Releasing (maintainers)

1. Set `version` in `Cargo.toml` (semver; the index format is internal, so a `VERSION` bump alone
   is not a breaking change). Run `cargo build` so `Cargo.lock` picks up the new version.
2. In `CHANGELOG.md`, rename **Unreleased** to `## [X.Y.Z] - YYYY-MM-DD`, add a new empty
   **Unreleased** section, and update the compare links at the bottom.
3. Commit (`chore: release vX.Y.Z`), then tag and push:

   ```sh
   git tag vX.Y.Z && git push origin main vX.Y.Z
   ```

4. The `Release` workflow checks that the tag matches `Cargo.toml`. It then builds Linux (x86_64, arm64),
   macOS (arm64, x86_64) and Windows (x86_64) binaries, and publishes a GitHub release with the
   archives, `SHA256SUMS`, and the changelog section as release notes.

## License

sym is dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option. Unless you
explicitly state otherwise, any contribution you intentionally submit for inclusion in sym, as
defined in the Apache-2.0 license, is dual-licensed as above, without any additional terms or conditions.
