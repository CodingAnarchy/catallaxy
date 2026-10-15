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

### Recorded workload: chain-shaped heaps (issue #178)

A 5M-node linked list of strings, 2M two-element lists and a 1M-entry dict (about 650 MB live,
`chain.cx` below). Its mark phase takes 60–160 ms, and adding markers barely helps: a chain can
be followed by only one marker at a time, and each node costs a cache miss that the next node's
address depends on. In October 2026 on an Apple M-series machine with 10 cores, the mark phase
measured 95–115 ms with one marker and 105–150 ms with ten. The ten-marker figure was taken with
other builds running (load average 15–20); the issue reported 63–93 ms on a quieter machine.

What #178 changed:
- **Direct child.** The first object a trace marks is traced next without going through the
  mark stack (`Tracer::direct`), so following a chain costs no stack traffic.
- **Lock-free polling for work.** Idle markers poll copies of the other markers' stealers,
  refreshed only when a marker joins, instead of taking the `stealers` mutex on every poll.
- **Sharing.** Markers share 32 entries every 16 objects instead of 16 every 32, so wide
  objects spread faster.

On this workload, with 30 extra collections:
- one marker: about 5% fewer instructions per collection, but only about 1.5% fewer cycles,
  because the time is cache misses along the chain;
- ten markers: total mark time fell by about 17% (minimum 5.43 s to 4.48 s over 35
  collections), and total CPU cycles fell by 6%, mostly from less contended idle polling.

The remaining cost is the miss per chain node, which neither parallel marking nor prefetching
can hide: the next address is only known once the node arrives. Only concurrent or incremental
marking, as above, would take this pause off the mutators. If a program with such a heap has a
pause target, start there.

```
class Node {
  value: String
  next: Node?
}
head: Node? = nil
for i in 0...5_000_000 { head = Node.new("s#{i % 1000}", head) }
lists: List[List[Int]] = []
for i in 0...2_000_000 { lists.push([i, i + 1]) }
d: Dict[Int, Int] = [:]
for i in 0...1_000_000 { d[i] = i }
for r in 0...5 { gc() }
```
