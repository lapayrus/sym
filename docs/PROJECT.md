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
| Parsing | `tree-sitter` 0.27 `Query`/`QueryCursor` running each grammar's `tags.scm` + our extras; `tree-sitter-tags` dropped | same queries, but we keep the syntax tree: syntax-aware signatures, parents, qualifiers, and (phase 5) incremental re-parse. One tag per name node: defs beat refs, then earliest pattern | — |
| Languages | Rust, Python, JS/JSX, TS/TSX, Go | cover most agent workloads | users ask for more (add grammar + extra query in `lang.rs`) |
| Imports | extra `@reference.import` patterns inside the tags query | same single parse, no second pass | — |
| Signatures | def text up to its `body` field (or the body of the function in its `value`/`right`); body-less defs scan tokens to a top-level `{`/`;` (Python `:`) or past `=>`. Strings/comments are atomic, strings > 40 B print as `"..."`; 200 chars max | syntax-aware, no per-language code | a grammar with no `body` field gives odd headers |
| Callback defs | `f("name", () => ..)` (JS/TS) and `t.Run("name", func..)` (Go) are `callback` defs named by the string | calls in test blocks/handlers get an enclosing def; outline shows the test tree | too noisy → restrict to known test/route callee names |
| Rust impls | `impl X` / `impl T for X` are `impl` defs named `X` | methods get a parent, outline nests them | — |
| Storage | SQLite (`rusqlite`, bundled) | point lookups in µs, joins for call graph, transactional per-file updates | `find_def` p99 > 1 ms on a Chromium-size repo → `redb` + in-memory maps |
| Rejected storage | Sled (stalled), RocksDB (heavy C++ build, KV only), DuckDB (OLAP, weak point lookups), GlueSQL (immature) | — | — |
| Call graph | no edge table; the caller is the innermost def whose line span contains the call ref (indexed `(file_id, line)` lookup) | zero resolution pass | — |
| CLI freshness | every CLI command runs an incremental refresh before answering | stale answers are the worst failure for an agent; refresh is 10 ms on ripgrep | big repos (175 ms at 31k files) → use `sym serve` + watch (phases 4–5) |
| Output format | plain text, one line per hit, `path:line-end kind [in Parent:] sig`; refs/calls grouped per resolved definition, then by file; `?` = ambiguous | fewest tokens; `line-end` lets an agent read the exact span | an MCP client needs structured output |
| Root discovery | `--root`, else nearest ancestor with `.sym` or `.git`, else cwd | works from any subdir | — |
| Reference resolution | syntactic, at query time (`query.rs` `Resolver`). Defs store `parent` (enclosing class/interface/impl, Go receiver), refs store `qual` (receiver/qualifier). Same language family only. Bare `f()` → free defs; `self.f()` → caller's own type; `Q.f()` → type `Q`, else a type ending in `Q` (`searcher` → `Searcher`), else module `Q`; capitalized/`::` `Q` with no match → external. Ties: same file → best import path match → same dir. TS overloads merge into one target | no types needed, one pass per query; fixed ripgrep's two `search_path`s and all 388 `getTypeOfSymbol` call sites on TypeScript | a receiver name says nothing about its type (`x.run()` with many `run` methods stays `?`) → per-language type inference |
| Qualified queries | `def`/`refs`/`calls` accept `Parent.name` / `Parent::name` | pick one member | — |
| Change detection | `(mtime, size)` per file | no hashing cost | `git checkout` causes spurious reparses → add blake3 |
| Schema migrations | none: `PRAGMA user_version` mismatch drops and rebuilds the index | it's a cache, and rebuilding is cheap | — |
| SQLite tuning | WAL, `synchronous=NORMAL`, `mmap_size=256MB`, `temp_store=MEMORY`, cached prepared statements, one transaction per run | — | — |
| Index pipeline | parallel walk (`ignore::build_parallel`) → rayon parse → bounded channel (256) → single SQLite writer | parse and write overlap, memory stays bounded | — |
| Query compile | per-language `OnceLock`, compiled on first use | startup cost only for languages present | — |
| File filter | `.gitignore` respected even outside git (`require_git(false)`); ignore files above a repo root skipped (`parents(false)` when root has `.git`); hard cap 16 MiB | real sources reach 3 MiB; parent lookup cost 50 ms/refresh on TypeScript | — |
| Minified code | every file is parsed; symbols whose name starts past column 1000 are dropped | no whole-file heuristic to misfire (a normal file with one huge data line keeps its symbols) | the ≤ 1000-column junk from a bundle's first line bothers anyone |
| MCP transport | hand-rolled sync stdio JSON-RPC | about 100 lines, no tokio | HTTP transport needed → `rmcp` |
| Fuzzy search | SQL `LIKE '%a%b%'` prefilter (case-insensitive subsequence, `_`/`%` escaped) over distinct non-callback names, then ranked: exact word initials (`gtos` → `getTypeOfSymbol`) → `nucleo-matcher` score → shorter | boundary/camelCase-aware ranking, 24–59 ms at 31k files | too slow → in-memory name list in `serve` |
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

- `src/lang.rs`: `parse(path, src) -> Option<Parsed { defs, refs, imports }>`, `supported(path)`, `family(path)`.
  Static `LANGS` table with lazily compiled `Query`s, and one `(Parser, QueryCursor)` per thread.
  `Def { name, kind, line, end_line, sig, parent }`, `Ref { name, kind, line, qual }`.
- `src/db.rs`: `open(root)`, `known_files`, `put_file` (delete + insert, cascading), `remove_file`. Schema version 2.
  Tables: `files`, `symbols(name, kind, line, end_line, sig, parent)`, `refs(name, kind, line, qual)`, `imports(module)`.
  Indexes on `name`/`module` and on `(file_id, line)`.
- `src/index.rs`: `index(root, conn) -> Stats { parsed, unchanged, removed, failed }`.
- `src/query.rs`: `def`, `refs(limit)`, `search(limit)`, `callers(limit)`, `callees(limit)`, `outline(path)`, plus the
  `Resolver`. Each returns ready-to-print text; phase 4's MCP tools should wrap these directly.
  `callees` lists resolved callees with locations, then `external: ...`.
- `src/main.rs`: clap CLI: `sym [--root R] index|def NAME|refs NAME|search Q|calls NAME [--callees]|outline PATH`.
  Every command refreshes the index first. `outline` accepts a path relative to cwd, or a suffix such as `db.rs`.
- Index location: `.sym/index.db` at the repo root (gitignored).

Benchmarks (release, Windows, 16 threads, warm OS cache):

| Repo | Files | Cold | Warm (no changes) | One-file edit | DB size |
|---|---|---|---|---|---|
| ripgrep | 110 | 125 ms | 10 ms | 26 ms | 1.6 MB |
| TypeScript (microsoft) | 31,443 | 5.9 s | 175 ms | 1.1 s (3 MiB `checker.ts`) | 63 MB |

Warm refresh breakdown on TypeScript (66,684 files in 653 dirs): directory walk ≈ 70 ms, root `.gitignore`
matching ≈ 50 ms, global excludes ≈ 35 ms, `known_files` ≈ 17 ms, DB open 2 ms.

Query latency on TypeScript (31k files), excluding the refresh (end-to-end CLI time adds it):

| Query | Time |
|---|---|
| `def getTypeOfSymbol` | 0.4 ms |
| `refs getTypeOfSymbol` (resolves all ~500 refs) | 18 ms |
| `calls getTypeOfSymbol` (388 call sites, 4 defs) | 16 ms |
| `calls checkSourceFile --callees` | 5 ms |
| `search gtos` / `search s` | 24 ms / 59 ms |
| `outline checker.ts` (3 MiB) | 11 ms |

The first-ever cold run on a fresh clone took 49.8 s: the OS/antivirus was scanning
newly written files. Benchmarks must use a warm cache, and should report the cold-cache run separately.

Known limits (also marked `ponytail:` in the code):
- Resolution is syntactic, not type-based: `x.run()` with several project `run` methods and an
  uninformative receiver name is shown under each with `?`.
- Minified files: symbols in the first 1000 columns of a minified line still get indexed (bounded junk).
- Huge files are bound by tree-sitter itself: `checker.ts` (3 MiB) takes ~1 s to re-index after an edit.
  Fix: incremental re-parse with the retained old tree, in watch mode (phase 5). Needs a long-lived process;
  a CLI call can't keep trees.
- Warm refresh is O(files) (175 ms at 31k files), and every CLI query pays it. The walk itself is the floor
  (≈ 70 ms raw `read_dir` on Windows); only watch mode (phase 5) removes it.
- `refs`/`calls` resolve every reference of the name (18 ms for ~500 refs); a name with 100k refs costs
  proportionally. Fine until measured otherwise.
- Anonymous callbacks without a string argument (`setTimeout(() => ..)`, `arr.map(x => ..)` at top level) still
  have no enclosing def (`<top level>`).

## Changelog

- 2026-09-28: Fixed phase 3 known limits before phase 4. Replaced `tree-sitter-tags` with direct `Query` use (keeps the tree); syntax-aware signatures; `parent`/`qual` columns (schema v2) and a syntactic resolver for refs/calls/callees; TS overloads merged; JS/Go callback defs; Rust `impl` defs; column-based minified filter replaces the whole-file heuristic; nucleo + initials ranking in `search`; `parents(false)` at repo roots. TypeScript: cold 7.0 → 5.9 s, warm 310 → 175 ms, `calls getTypeOfSymbol` 0 ambiguous of 388.
- 2026-09-28: Phase 3 done. clap CLI with `def`/`refs`/`search`/`calls`/`outline` in `query.rs`; auto-refresh before each query; root discovery. Verified on this repo and ripgrep (`calls search_path` matches all 8 `rg` call sites). Queries run in 0.4–42 ms on TypeScript.
- 2026-09-28: Phase 2 done. SQLite store + incremental index with parallel walk, pipelined parse/write, lazy query compile, and minified-file detection. Cold index of TypeScript went from 9.6 s to 7.0 s; warm refresh from 720 ms to 310 ms.
- 2026-09-27: Phases 0 and 1 done. Added CommonJS `require()` imports and multi-line signatures. Chose SQLite.
