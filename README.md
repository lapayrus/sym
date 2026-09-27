# sym

A symbol index for AI coding agents: "ripgrep for symbols". It answers *where is X defined*,
*who calls X*, *what does this file contain* from a tree-sitter index in SQLite, as a CLI and
as an MCP server. Answers come back in milliseconds, in compact line-oriented text that costs an agent
fewer tokens than grepping or reading whole files.

- **Always fresh.** Every CLI call refreshes the index incrementally first. `sym serve` watches the tree
  and re-indexes changed files in the background, so an edit shows up in the next query.
- **Fast cold start.** The index persists in `.sym/index.db`, and indexing a 31k-file repo takes about 7 s.
- **One binary.** No GPU, no cloud, no language servers.

Languages: Rust, Python, JavaScript/JSX, TypeScript/TSX, Go.

## Install

```sh
cargo install --path .    # Rust 1.88+
```

Add `.sym/` to your `.gitignore`.

## Use from Claude Code (MCP)

```sh
claude mcp add sym -- sym serve
```

The server indexes the repo it starts in: the nearest ancestor of the working directory that has
`.git` or `.sym`. To point it elsewhere, use `sym --root <dir> serve`. It exposes five tools:

| Tool | Answers |
|---|---|
| `find_def(name)` | where `name` is defined, with signature and line span |
| `find_refs(name, limit?)` | every reference, grouped by the definition it resolves to, then by file, with the enclosing function |
| `calls(name, direction?, limit?)` | callers (default) or callees |
| `search(query, limit?)` | fuzzy definition search (`gtos` finds `getTypeOfSymbol`) |
| `outline(path)` | every definition in a file, nested; `path` may be a suffix like `db.rs` |

`Parent.name` or `Parent::name` selects one member, for example `Searcher.search_path`.

## CLI

The CLI has the same queries: `sym def|refs|calls [--callees]|search|outline`, plus `sym index`. Example output on ripgrep's source:

```text
$ sym calls search_path
crates/core/search.rs:342-351 method in SearchWorker: fn search_path(&mut self, path: &Path) -> io::Result<SearchResult>
  crates/core/search.rs:245 search (265)
  crates/core/search.rs:329 search_decompress (331)
crates/core/search.rs:380-412 function fn search_path<M: Matcher, W: WriteColor>( matcher: M, searcher: &mut grep::searcher::Searcher, printer: &mut Printer<W>, path: &Path, ) -> io::Result<SearchResult>
  crates/core/search.rs:342 search_path (347, 349)
crates/searcher/src/searcher/mod.rs:643-657 method in Searcher: pub fn search_path<P, M, S>( &mut self, matcher: M, path: P, write_to: S, ) -> Result<(), S::Error> where P: AsRef<Path>, M: Matcher, S: Sink,
  crates/core/search.rs:380 search_path (389, 397, 405)
  crates/grep/examples/simplegrep.rs:32 search (58)
```

Each definition line reads `path:line-end kind [in Parent:] signature`, and each caller line gives the
calling function and its call-site lines. The three same-named `search_path`s are told apart syntactically:
`self.search_path(..)` inside `SearchWorker` resolves to the method, and a bare call resolves to the free function.
Calls that could mean several definitions are listed once, under `? one of path:line, ...`.

## Benchmarks

Measured on Windows 11 with 16 threads and a warm OS cache, against ripgrep 14.1.1.
Times are wall clock including process start, as medians. Tokens are estimated as bytes / 4. The rg
command is what an agent would run instead:

| Task | rg baseline |
|---|---|
| def | `rg -n '(fn\|def\|function\|func\|class\|…)\s+NAME\b'` |
| refs | `rg -n -w NAME` |
| callers | `rg -n '\bNAME\s*\('` |
| outline | reading the whole file |

To reproduce: `python bench/bench.py <sym> <rg> <repo> --symbol ... --file ... --edit ...`

### microsoft/TypeScript @ 4f5ddae2 (31,443 indexed files, 66,640 in the checkout)

| Cold index | Refresh, no change (CLI) | One-file edit (CLI) | Edit → answer (`serve`) | DB |
|---|---|---|---|---|
| 6.9 s | 192 ms | 259 ms | 58 ms | 66 MB |

| Task | `sym serve` | `sym` CLI | `rg` | ≈ tokens sym / rg |
|---|---|---|---|---|
| def `getTypeOfSymbol` | 0.2 ms | 188 ms | 2307 ms | 391 / 140 |
| refs `getTypeOfSymbol` | 18.7 ms | 226 ms | 2277 ms | 6,440 / 15,182 |
| callers `getTypeOfSymbol` | 18.0 ms | 221 ms | 2268 ms | 8,949 / 14,289 |
| def `checkSourceFile` | 0.2 ms | 196 ms | 2264 ms | 75 / 68 |
| refs `checkSourceFile` | 0.2 ms | 191 ms | 2224 ms | 116 / 1,009 |
| callers `checkSourceFile` | 0.2 ms | 210 ms | 2332 ms | 114 / 114 |
| outline `checker.go` (1.4 MiB) | 9.3 ms | 209 ms | read file | 39,469 / 361,993 |
| outline `program.go` | 6.1 ms | 203 ms | read file | 4,064 / 25,541 |

### BurntSushi/ripgrep @ 3fce3b5 (110 files)

| Cold index | Refresh, no change (CLI) | One-file edit (CLI) | Edit → answer (`serve`) | DB |
|---|---|---|---|---|
| 0.2 s | 19 ms | 50 ms | 19 ms | 2 MB |

| Task | `sym serve` | `sym` CLI | `rg` | ≈ tokens sym / rg |
|---|---|---|---|---|
| def `search_path` | 0.1 ms | 19 ms | 34 ms | 133 / 61 |
| refs `search_path` | 0.2 ms | 17 ms | 31 ms | 215 / 287 |
| callers `search_path` | 0.3 ms | 19 ms | 33 ms | 196 / 195 |
| refs `search_reader` | 6.0 ms | 22 ms | 31 ms | 1,572 / 2,578 |
| callers `search_reader` | 6.2 ms | 25 ms | 31 ms | 2,082 / 2,463 |
| outline `main.rs` | 0.2 ms | 18 ms | read file | 297 / 4,792 |
| outline `searcher/mod.rs` | 0.3 ms | 19 ms | read file | 1,269 / 10,564 |

What the numbers say:
- **Speed.** Through `sym serve`, queries take 0.1–19 ms. rg takes 2.3 s on the large repo because it
  reads every file each time. The CLI adds its own refresh (about 190 ms at 31k files), which is why the server is the
  recommended way to use sym on big repos.
- **Tokens.** Outlines are 6–16× smaller than reading the file. On the large repo, refs take 2.4× to
  8.7× fewer tokens, because rg's matches include comments, strings and imports. Callers range from
  even with rg to 1.6× fewer tokens, and add what rg can't: the calling function and which definition each call hits.
  `def` costs more tokens than rg: it prints the full signature and line span of every definition,
  such as all overloads or `.d.ts` declarations, while the regex misses some (5 lines vs 12 here).

## How it works

A parallel walk (respecting `.gitignore`) feeds rayon parser threads. Each file gets one tree-sitter parse
running the grammar's `tags.scm` plus a few extra patterns, which yields definitions, references and imports.
A single writer stores the results in SQLite. Files are re-parsed only when their `(mtime, size)` changes.
References are resolved at query time with syntactic rules:
- the receiver name (`self.f()`, `Type::f()`, `searcher.f()` → `Searcher`)
- then imports
- then the same file and directory

The caller of a reference is the innermost definition whose line span contains it. See
[docs/PROJECT.md](docs/PROJECT.md) for the design decisions and their trade-offs.

## Limitations

- Resolution is syntactic, not type-based. `x.run()` with several `run` methods and an uninformative
  receiver name ends up in a `? one of ...` group.
- Type usages are only partly indexed as references. In Rust, `refs Searcher` finds no `&mut Searcher` and no
  `Searcher::new()`. Calls, `new X`, and the type positions the TS/Go grammars capture are covered.
- Editing a very large file (MBs) takes about 0.5–1 s to re-index. `sym serve` does that in the background.
- Anonymous callbacks without a name string, such as `arr.map(x => ..)` at top level, have no enclosing definition.

The full list is in [docs/PROJECT.md](docs/PROJECT.md#current-state).
