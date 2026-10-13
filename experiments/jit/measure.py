"""Runs the JIT experiment and prints medians of RUNS runs as Markdown tables.

Run from anywhere after `cargo build --release` (workspace) and
`cargo build --release --manifest-path experiments/jit/Cargo.toml`.
On macOS, whole-process counters come from `/usr/bin/time -l`; elsewhere only wall time is shown.
"""
import os, platform, re, statistics, subprocess, sys, time

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
CATX = os.path.join(ROOT, "target", "release", "catx")
HARNESS = os.path.join(ROOT, "experiments", "jit", "target", "release", "jit-experiment")
RUNS = int(os.environ.get("RUNS", "7"))
DARWIN = platform.system() == "Darwin"

CASES = [
    ("fib.cx", ["run", "bench/fib.cx"]),
    ("loop.cx", ["run", "bench/loop.cx"]),
    ("collatz.cx", ["run", "experiments/jit/cases/collatz.cx"]),
    ("fib(35)", ["call", "bench/fib.cx", "fib", "35"]),
    ("parallel.cx work(1)", ["call", "bench/parallel.cx", "work", "1"]),
]
CONFIGS = [
    ("catx", None),
    ("interp", ["interp"]),
    ("jit poll=call", ["--poll", "call", "jit"]),
    ("jit poll=load", ["--poll", "load", "jit"]),
    ("jit poll=none", ["--poll", "none", "jit"]),
]


def once(cfg, case):
    if cfg is None:
        if case[0] != "run":
            return None
        argv = [CATX] + case
    else:
        argv = [HARNESS] + cfg + case
    if DARWIN:
        argv = ["/usr/bin/time", "-l"] + argv
    t = time.perf_counter()
    r = subprocess.run(argv, capture_output=True, text=True, cwd=ROOT)
    wall = (time.perf_counter() - t) * 1000
    if r.returncode != 0:
        sys.exit(f"{argv} failed:\n{r.stderr}")
    out = {"stdout": r.stdout, "wall_ms": wall}
    m = re.search(r"exec: ([\d.]+) ms", r.stderr)
    if m:
        out["exec_ms"] = float(m.group(1))
    m = re.search(r"jit: analysis .* total ([\d.]+) us", r.stderr)
    if m:
        out["compile_us"] = float(m.group(1))
    if DARWIN:
        out["instr_M"] = int(re.search(r"(\d+)\s+instructions retired", r.stderr).group(1)) / 1e6
        out["cycles_M"] = int(re.search(r"(\d+)\s+cycles elapsed", r.stderr).group(1)) / 1e6
    out["stderr"] = r.stderr
    return out


def fmt(v, nd=1):
    return "" if v is None else f"{v:,.{nd}f}"


print(f"{platform.machine()} {platform.system()} {platform.platform()}, median of {RUNS} runs\n")
cols = ["wall_ms", "exec_ms", "instr_M", "cycles_M", "compile_us"] if DARWIN else ["wall_ms", "exec_ms", "compile_us"]
print("| Program | Config | " + " | ".join(cols) + " | exec speedup |")
print("| --- | --- |" + " --- |" * (len(cols) + 1))
compile_lines = {}
for label, case in CASES:
    expected = None
    base_exec = None
    for name, cfg in CONFIGS:
        runs = [once(cfg, case) for _ in range(RUNS)]
        if runs[0] is None:
            continue
        for r in runs:
            if expected is None:
                expected = r["stdout"]
            elif r["stdout"] != expected:
                sys.exit(f"{label} {name}: output differs:\n{r['stdout']!r}\nvs\n{expected!r}")
        med = {k: statistics.median(r[k] for r in runs) if k in runs[0] else None for k in cols}
        if name == "interp":
            base_exec = med["exec_ms"]
        speed = f"{base_exec / med['exec_ms']:.1f}x" if base_exec and med["exec_ms"] else ""
        print(f"| {label} | {name} | " + " | ".join(fmt(med[k]) for k in cols) + f" | {speed} |")
        if name == "jit poll=call":
            compile_lines[label] = [l for l in runs[0]["stderr"].splitlines() if l.startswith("jit:")]
    print(f"<!-- {label}: output {expected.strip()!r} identical in all configs -->")

print("\nCompile times (one run, poll=call):\n")
for label, lines in compile_lines.items():
    print(f"{label}:")
    for l in lines:
        print(f"    {l}")
