# Escape analysis and block-scoped reclamation (issue #13)

**Decision: not now.** Measured against thread-local young collection (#1), the most region
allocation could save on allocation-heavy loops is the collection time of their temporaries,
11–16% on the worst cases and 4–5% on typical ones. A wrong "does not escape" answer is a
use-after-free, so that ceiling doesn't justify the risk. The larger gains need inlining and
scalar replacement, which is an optimizing compiler, the same territory the JIT experiment (#7)
declined for now.

## Measurements

October 2026, Apple M-series, best of three. GC time from `CATX_GC_STATS=1`.

| Program | Runtime | Minor GC | Major GC | GC share |
| --- | --- | --- | --- | --- |
| list temporary per iteration (3M iterations, see below) | 0.12 s | 13.0 ms | 1.0 ms | 11% |
| same loop without the list | 0.01 s | — | — | — |
| two `P.new` temporaries per call (2M iterations) | 0.09 s | 3.2 ms | 0 | 4% |
| `bench/gen.cx` | 0.45 s | 74.2 ms | 9.1 ms | 18% |
| `bench/trees.cx` | 0.09 s | 4.5 ms | 0.5 ms | 5% |

```
for i in 0...3_000_000 {
  tmp = [i, i + 1, i + 2]          # never escapes the loop body
  s += tmp[0] + tmp.length
}
```

- **Region allocation** (still allocate, but reclaim at block exit) saves at most the GC share
  above, minus the region's own bookkeeping. Young collection already makes short-lived garbage
  cheap: one bump allocation plus a slot visit when its page is swept.
- **Scalar replacement** (no allocation at all) is where the 12× gap between the first two rows
  lies. Most of it is the list operations themselves (`NewList`, indexing, the `length` native
  call), not memory management. Removing them requires inlining calls and constructors and
  keeping fields in registers, which is an optimizing compiler, not an escape analysis.

## What a future attempt should look like

If profiles of real programs show allocation-heavy hot loops dominated by collection:

1. Conservative escape analysis over the checked program. Unknown calls, `Dyn`, natives,
   interface dispatch, closures, channels, `freeze` and `share` all escape their arguments.
2. Region allocation per function frame for non-escaping allocations, with the region scanned
   as roots by both collectors.
3. A differential mode that poisons reclaimed objects and checks every access, run across the
   conformance suite and the differential fuzzer (#20) under `CATX_GC_STRESS`, plus Miri on the
   region allocator.

Adopt only for a clear, measured win over young collection on the benchmarks.
