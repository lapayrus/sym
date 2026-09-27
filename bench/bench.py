"""Benchmark sym against rg on one repo; prints markdown tables for the README.

usage: python bench/bench.py SYM RG REPO --symbol NAME... --file PATH... --edit PATH

  --symbol  names to look up (def / refs / callers)
  --file    files to outline (baseline: reading the whole file)
  --edit    a mid-size source file to append to for the edit-latency runs (restored afterwards)

Tokens are estimated as output bytes / 4. Times are wall clock (process start included), medians
after one warm-up run, warm OS cache. The rg baselines are what an agent would run instead; they
answer less (no enclosing function, no resolution to a definition, comments and strings included).
"""
import argparse, json, os, platform, shutil, statistics, subprocess, sys, time

DEF_RE = r"\b(fn|def|function|func|class|interface|type|struct|enum|trait)\s+(\([^)]*\)\s*)?{}\b"
PROBE = {".rs": "fn {}() {{}}", ".go": "func {}() {{}}", ".py": "def {}(): pass"}  # else JS/TS


def run(cmd, cwd):
    t = time.perf_counter()
    # stdin closed: with a readable stdin, rg searches it instead of the directory.
    out = subprocess.run(cmd, cwd=cwd, capture_output=True, stdin=subprocess.DEVNULL).stdout
    return (time.perf_counter() - t) * 1000, out


def median(f, n=5):
    f()  # warm-up, not counted
    return statistics.median(f() for _ in range(n))


def tok(b):
    return f"{len(b) // 4:,}"


def lines(b):
    return len(b.splitlines())


class Serve:
    """`sym serve` over stdio; call() returns (ms, text)."""

    def __init__(self, sym, repo):
        self.p = subprocess.Popen([sym, "--root", repo, "serve"], stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
        self.call("find_def", name="_warmup_")

    def call(self, tool, **args):
        msg = {"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": tool, "arguments": args}}
        t = time.perf_counter()
        self.p.stdin.write(json.dumps(msg) + "\n")
        self.p.stdin.flush()
        text = json.loads(self.p.stdout.readline())["result"]["content"][0]["text"]
        return (time.perf_counter() - t) * 1000, text

    def close(self):
        self.p.stdin.close()
        self.p.wait()


def main():
    sys.stdout.reconfigure(encoding="utf-8")
    ap = argparse.ArgumentParser()
    ap.add_argument("sym")
    ap.add_argument("rg")
    ap.add_argument("repo")
    ap.add_argument("--symbol", nargs="+", required=True)
    ap.add_argument("--file", nargs="+", required=True)
    ap.add_argument("--edit", required=True)
    a = ap.parse_args()
    sym, rg, repo = os.path.abspath(a.sym), os.path.abspath(a.rg), a.repo

    ver = lambda exe: subprocess.run([exe, "--version"], capture_output=True, text=True).stdout.splitlines()[0]
    print(f"## {os.path.basename(os.path.normpath(repo))}\n")
    print(f"{ver(sym)}, {ver(rg)}, {platform.system()} {platform.release()}, {os.cpu_count()} threads\n")

    # Indexing.
    def cold():
        shutil.rmtree(os.path.join(repo, ".sym"), ignore_errors=True)
        return run([sym, "index"], repo)[0]

    cold_ms = median(cold, 3)
    time.sleep(3)  # a freshly written DB gets scanned by the OS/antivirus; don't bill that to the warm runs
    stats = run([sym, "index"], repo)[1].decode().split()
    files = int(stats[1]) + int(stats[3])  # "parsed P unchanged U ..."
    warm_ms = median(lambda: run([sym, "index"], repo)[0])
    edit_path = os.path.join(repo, a.edit)
    orig = open(edit_path, "rb").read()

    def edit_cli():
        try:
            open(edit_path, "wb").write(orig + b"\n")
            return run([sym, "index"], repo)[0]
        finally:
            open(edit_path, "wb").write(orig)
            run([sym, "index"], repo)

    edit_ms = median(edit_cli, 3)
    db_mb = os.path.getsize(os.path.join(repo, ".sym", "index.db")) / 1e6

    srv = Serve(sym, repo)
    probe = PROBE.get(os.path.splitext(a.edit)[1], "function {}() {{}}")

    def edit_serve(i=[0]):
        i[0] += 1
        name = f"symBenchProbe{i[0]}"
        try:
            open(edit_path, "wb").write(orig + ("\n" + probe.format(name) + "\n").encode())
            ms, text = srv.call("find_def", name=name)
            assert name in text, text
            return ms
        finally:
            open(edit_path, "wb").write(orig)

    edit_srv_ms = median(edit_serve)
    time.sleep(0.5)  # let the restore be re-indexed

    print("| Files | Cold index | Refresh, no change (CLI) | One-file edit (CLI) | Edit → answer (`serve`) | DB |")
    print("|---|---|---|---|---|---|")
    print(f"| {files:,} | {cold_ms / 1000:.1f} s | {warm_ms:.0f} ms | {edit_ms:.0f} ms | {edit_srv_ms:.0f} ms | {db_mb:.0f} MB |\n")

    # Queries. CLI time includes the refresh every CLI call does; `serve` time is a warm MCP tool call.
    print("| Task | `sym` CLI | `sym serve` | `rg` | lines sym / rg | ≈ tokens sym / rg |")
    print("|---|---|---|---|---|---|")
    # No hit limit, so sym's output is complete like rg's (by default sym shows 50 and says how many more).
    all_ = 1_000_000
    for name in a.symbol:
        tasks = [
            (f"def `{name}`", ["def", name], ("find_def", {"name": name}), ["-n", DEF_RE.format(name)]),
            (f"refs `{name}`", ["refs", name, "--limit", str(all_)], ("find_refs", {"name": name, "limit": all_}), ["-n", "-w", name]),
            (f"callers `{name}`", ["calls", name, "--limit", str(all_)], ("calls", {"name": name, "limit": all_}), ["-n", rf"\b{name}\s*\("]),
        ]
        for label, cli, (tool, args), rg_args in tasks:
            cli_ms = median(lambda: run([sym, *cli], repo)[0])
            srv_ms = median(lambda: srv.call(tool, **args)[0])
            out = srv.call(tool, **args)[1].encode()
            rg_ms = median(lambda: run([rg, *rg_args], repo)[0])
            rg_out = run([rg, *rg_args], repo)[1]
            print(f"| {label} | {cli_ms:.0f} ms | {srv_ms:.1f} ms | {rg_ms:.0f} ms | {lines(out)} / {lines(rg_out)} | {tok(out)} / {tok(rg_out)} |")
    for path in a.file:
        cli_ms = median(lambda: run([sym, "outline", path], repo)[0])
        srv_ms = median(lambda: srv.call("outline", path=path)[0])
        out = srv.call("outline", path=path)[1].encode()
        src = open(os.path.join(repo, path), "rb").read()
        print(f"| outline `{os.path.basename(path)}` (rg: read file) | {cli_ms:.0f} ms | {srv_ms:.1f} ms | — | {lines(out)} / {lines(src)} | {tok(out)} / {tok(src)} |")
    srv.close()
    print()


if __name__ == "__main__":
    main()
