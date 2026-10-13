# Concurrent or incremental marking (issue #25)

**Decision: not now.** Major pauses are already short: parallel stop-the-world marking on helper
threads (#2) peaks at 25 ms with 890 MB live on a 10-core machine, and thread-local young
collection (#1) keeps major collections rare. No workload in the corpus or benchmarks needs
shorter pauses.

## What it would take

Snapshot-at-the-beginning (SATB) marking: the collector stops the world only to scan roots;
mutators run while marking proceeds, and every overwrite of a heap pointer during marking first
records the old value, so that everything reachable at the snapshot gets marked. New objects are
allocated black.

Places a pointer can be deleted, each of which would need the barrier:

| Site | Where |
| --- | --- |
| Instance field stores | `SetField` fast path (`interp.rs`), `InstanceObj::set` in natives |
| List element stores, pops, truncation, buffer swaps | `ListObj::{set, pop, replace, modify}` |
| Dict value stores, removals, rebuilds, shape changes | `DictTable` writers |
| Cell stores (captured variables) | `SetCell` |
| Channel queues (a received value leaves the queue) | `chan_recv`, `try_recv`, `close` |
| `Mutex` and `Atomic` swaps | `Mutex.set`, `update`; `Atomic.set`, `swap`, `compare_and_set`, `update` |
| Thread and fiber results (taken by `join`) | `ThreadOutcome`, fiber `_result` and the pinned table |
| Global stores | `SetGlobal` |
| Handle and extra-root stacks of natives | `HandleScope`, `extra_roots` truncation |

It would also need to interact with thread-local minor collections running during a major mark,
and with the lazy sweeper, plus Loom models for the barrier and the mark-termination handshake.

## Measured barrier cost

A throwaway build added the barrier's fast path (one relaxed load of a global "marking" flag on
every write through `check_write` and every `SetField`). Median cycles over five runs, against
the same build without it:

| Benchmark | Without | With |
| --- | --- | --- |
| `nbody.cx` | 850M | 846M |
| `trees.cx` | 426M | 416M |
| `gen.cx` | 2175M | 2160M |
| `words.cx` | 278M | 281M |

All within noise. The cost of concurrent marking is correctness surface, not mutator speed.

## When to revisit

When a workload with a stated pause target cannot meet it with parallel marking. For example,
an interactive or latency-bound server whose live heap makes major pauses exceed its budget.
Record the workload and the target in this file first, then build the barrier in the sites above.
