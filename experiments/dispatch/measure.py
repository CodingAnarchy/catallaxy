"""Runs the dispatch experiment and prints medians. Uses perf counters on Linux when available,
`time -l` counters on macOS, and wall time otherwise."""
import platform, re, shutil, statistics, subprocess, sys, time

BIN = sys.argv[1] if len(sys.argv) > 1 else "./target/release/dispatch-experiment"
RUNS = 7
perf = shutil.which("perf") if platform.system() == "Linux" else None

def once(style, prog):
    if perf:
        r = subprocess.run([perf, "stat", "-x,", "-e", "cycles,instructions,branch-misses", BIN, style, prog],
                           capture_output=True, text=True)
        vals = {}
        for line in r.stderr.splitlines():
            parts = line.split(",")
            if len(parts) > 2 and parts[0].strip().isdigit():
                vals[parts[2].split(":")[0]] = int(parts[0])
        if "cycles" in vals:
            return vals
    if platform.system() == "Darwin":
        r = subprocess.run(["/usr/bin/time", "-l", BIN, style, prog], capture_output=True, text=True)
        return {
            "cycles": int(re.search(r"(\d+)\s+cycles elapsed", r.stderr).group(1)),
            "instructions": int(re.search(r"(\d+)\s+instructions retired", r.stderr).group(1)),
        }
    t = time.perf_counter()
    subprocess.run([BIN, style, prog], capture_output=True, check=True)
    return {"wall_ms": (time.perf_counter() - t) * 1000}

print(f"{platform.machine()} {platform.system()} {platform.processor()}")
for prog in ["loop", "fib"]:
    for style in ["switch", "threaded"]:
        runs = [once(style, prog) for _ in range(RUNS)]
        med = {k: statistics.median(r[k] for r in runs) for k in runs[0]}
        print(f"{prog:5} {style:9} " + "  ".join(
            f"{k} {v / 1e6:.1f}M" if k != "wall_ms" else f"{k} {v:.1f}" for k, v in med.items()))
