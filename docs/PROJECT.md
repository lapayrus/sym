# sym — project state

Single source of truth for mission, decisions, roadmap and current state.
Update it in the same change as the code it describes.

## Mission

A local-first, always-fresh symbol index for AI coding agents, shipped as a CLI
("ripgrep for symbols") and an MCP server. It answers "where is X defined",
"who calls Y", "what does file Z contain" in milliseconds, with compact output
that costs agents far fewer tokens than grep or reading whole files.

What sets it apart:
- **Fresh**: incremental re-index. Cost scales with changed files, not repo size.
- **Instant cold start**: persistent SQLite store, no re-embedding.
- **Token-lean output**: line-oriented and grouped by file, with no JSON noise.
- **Zero GPU, zero cloud**: one binary.

Proof is public benchmarks (phase 6): cold index time, one-file update latency,
query latency, and tokens per task versus `rg`.

## Tech decisions

| Decision | Choice | Why | Revisit when |
|---|---|---|---|
| Language | Rust (edition 2024) | tree-sitter, rayon and notify are first-class; single binary | — |
| Parsing | `tree-sitter` 0.27 + `tree-sitter-tags` 0.27 | the grammars ship `tags.scm` (defs and refs for free); one parse per file | — |
| Languages | Rust, Python, JS/JSX, TS/TSX, Go | cover most agent workloads | users ask for more (add grammar + extra query in `lang.rs`) |
| Imports | extra `@reference.import` patterns inside the tags query | same single parse, no second pass | — |
| Signatures | byte scan from def start to body (`{`/`;`, Python `:`), whitespace collapsed, 200 chars | no tree access through tags API; cheap | a bracket inside a string skews one in the wild |
| Storage | SQLite (`rusqlite`, bundled) | point lookups in µs, joins for call graph, transactional per-file updates | `find_def` p99 > 1 ms on a Chromium-size repo → `redb` + in-memory maps |
| Rejected storage | Sled (stalled), RocksDB (heavy C++ build, KV only), DuckDB (OLAP, weak point lookups), GlueSQL (immature) | — | — |
| Call graph | no edge table; the caller is the innermost def whose line span contains the call ref (indexed `(file_id, line)` lookup) | zero resolution pass | — |
| CLI freshness | every CLI command runs an incremental refresh before answering | stale answers are the worst failure for an agent; refresh is 14 ms on ripgrep | big repos (310 ms at 31k files) → use `sym serve` + watch (phases 4–5) |
| Output format | plain text, one line per hit, `path:line-end kind sig`; refs/outline grouped by file | fewest tokens; `line-end` lets an agent read the exact span | an MCP client needs structured output |
| Root discovery | `--root`, else nearest ancestor with `.sym` or `.git`, else cwd | works from any subdir | — |
| Reference resolution | by name only | fast, language-agnostic | noisy names (`new`, `get`): first rank refs in files that import the defining module |
| Change detection | `(mtime, size)` per file | no hashing cost | `git checkout` causes spurious reparses → add blake3 |
| Schema migrations | none: `PRAGMA user_version` mismatch drops and rebuilds the index | it's a cache, and rebuilding is cheap | — |
| SQLite tuning | WAL, `synchronous=NORMAL`, `mmap_size=256MB`, `temp_store=MEMORY`, cached prepared statements, one transaction per run | — | — |
| Index pipeline | parallel walk (`ignore::build_parallel`) → rayon parse → bounded channel (256) → single SQLite writer | parse and write overlap, memory stays bounded | — |
| Query compile | per-language `OnceLock`, compiled on first use | startup cost only for languages present | — |
| File filter | `.gitignore` respected even outside git (`require_git(false)`); hard cap 16 MiB; minified files (avg line > 500 B) stored with no symbols | real sources reach 3 MiB; bundles pollute results | minified heuristic misfires |
| MCP transport | hand-rolled sync stdio JSON-RPC | about 100 lines, no tokio | HTTP transport needed → `rmcp` |
| Fuzzy search | SQL `LIKE '%a%b%'` (case-insensitive subsequence, `_`/`%` escaped) over distinct names, ranked exact > prefix > substring > subsequence, then shorter | no extra deps, 25–42 ms at 31k files | ranking feels off → `nucleo-matcher`; too slow → in-memory name list in `serve` |
| Rejected: Laya (Convai decision model) | not used | a probabilistic classifier, ~140 ms+/call on CPU, 1.7 GB, 512-token context; conflicts with the "fast, exact, light" pitch | — |

## Roadmap

| # | Phase | Done when | Status |
|---|---|---|---|
| 0 | Crate, deps, grammar/ABI compatibility | `cargo build` passes | ✅ done |
| 1 | `lang.rs`: defs/refs/imports + signatures, 5 languages | per-language tests pass | ✅ done |
| 2 | `db.rs` + `index.rs`: SQLite store, full + incremental index | second `sym index` re-parses 0 files | ✅ done |
| 3 | CLI: `def`, `refs`, `search`, `calls`, `outline` (clap) | correct answers on this repo and on ripgrep | ✅ done |
| 4 | `sym serve` MCP server, 5 tools | works via `claude mcp add sym -- sym serve` | ⏳ next |
| 5 | Watch mode (`notify-debouncer-mini`, thread inside `serve`) | an edit shows up in queries in < 100 ms | ⏳ |
| 6 | Benchmarks + README | published numbers vs `rg` | ⏳ |

MCP tools planned: `find_def(name)`, `find_refs(name, limit)`, `search(query)`,
`calls(name, callers|callees)`, `outline(path)`.

## Current state

- `src/lang.rs`: `parse(path, src) -> Option<Parsed { defs, refs, imports }>`, `supported(path)`.
  Static `LANGS` table with lazily compiled tags configs, and one `TagsContext` per thread.
- `src/db.rs`: `open(root)`, `known_files`, `put_file` (delete + insert, cascading), `remove_file`.
  Tables: `files`, `symbols(name, kind, line, end_line, sig)`, `refs(name, kind, line)`, `imports(module)`.
  Indexes on `name`/`module` and on `(file_id, line)`.
- `src/index.rs`: `index(root, conn) -> Stats { parsed, unchanged, removed, failed }`.
- `src/query.rs`: `def`, `refs(limit)`, `search(limit)`, `callers(limit)`, `callees(limit)`, `outline(path)`.
  Each returns ready-to-print text; phase 4's MCP tools should wrap these directly.
- `src/main.rs`: clap CLI: `sym [--root R] index|def NAME|refs NAME|search Q|calls NAME [--callees]|outline PATH`.
  Every command refreshes the index first. `outline` accepts a path relative to cwd, or a suffix such as `db.rs`.
- Index location: `.sym/index.db` at the repo root (gitignored).

Benchmarks (release, Windows, 16 threads, warm OS cache):

| Repo | Files | Cold | Warm (no changes) | One-file edit | DB size |
|---|---|---|---|---|---|
| ripgrep | 110 | 293 ms | 14 ms | 32 ms | 1.5 MB |
| TypeScript (microsoft) | 31,443 | 7.0 s | 310 ms | 1.3 s (3 MiB `checker.ts`) | 63 MB |

Query latency on TypeScript (31k files), excluding the refresh; end-to-end CLI time is about 400 ms, almost all of it the refresh walk:

| Query | Time |
|---|---|
| `def getTypeOfSymbol` | 0.4 ms |
| `refs getTypeOfSymbol` (limit 50) | 2.0 ms |
| `calls getTypeOfSymbol` (262 callers) | 4.6 ms |
| `calls checkSourceFile --callees` | 0.5 ms |
| `search gtos` / `search s` | 25 ms / 42 ms |
| `outline checker.ts` (3 MiB) | 21 ms |

The first-ever cold run on a fresh clone took 49.8 s: the OS/antivirus was scanning
newly written files. Benchmarks must use a warm cache, and should report the cold-cache run separately.

Known limits (also marked `ponytail:` in the code):
- Signature scan is byte-based, not syntax-aware.
- Reference resolution is name-only.
- Minified detection is an average-line-length heuristic.
- Huge files are bound by tree-sitter itself: `checker.ts` (3 MiB) takes 600 ms to parse
  and 820 ms including tags. Fix: incremental re-parse with the old tree, in watch mode (phase 5).
- Warm refresh is O(files) stats (310 ms at 31k files), and every CLI query pays it. Watch mode (phase 5) avoids the walk.
- `calls --callees` lists raw call names, including std/external ones (`push`, `Ok`); they are not resolved to project defs.
- `search` is a LIKE scan over distinct names with a 4-bucket rank (no word-boundary scoring).
- Test-file calls inside anonymous callbacks show as `<top level>` (anonymous functions are not defs).

## Changelog

- 2026-09-28: Phase 3 done. clap CLI with `def`/`refs`/`search`/`calls`/`outline` in `query.rs`; auto-refresh before each query; root discovery. Verified on this repo and ripgrep (`calls search_path` matches all 8 `rg` call sites). Queries run in 0.4–42 ms on TypeScript.
- 2026-09-28: Phase 2 done. SQLite store + incremental index with parallel walk, pipelined parse/write, lazy query compile, and minified-file detection. Cold index of TypeScript went from 9.6 s to 7.0 s; warm refresh from 720 ms to 310 ms.
- 2026-09-27: Phases 0 and 1 done. Added CommonJS `require()` imports and multi-line signatures. Chose SQLite.
