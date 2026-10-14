# Minor-collection cost (issue #23)

Measured October 2026 on an Apple M-series machine with `CATX_GC_TRACE=1`, which now prints the
phases of every minor collection, and `time -l` cycle counts.

## Where the time goes

| Benchmark | Minors | Mark | Sweep | Remembered objects per minor |
| --- | --- | --- | --- | --- |
| `bench/gen.cx` (1M-node live tree + 2M short-lived lists) | 129 | 12.6 ms | 118 ms | 0 |
| `bench/trees.cx` | 19 | 2.5 ms | 3.1 ms | 0 |
| `bench/bigheap.cx` (8M live objects) | 118 | 37 ms | 54 ms | 0 |

Sweeping dominates when most of the nursery dies. It visits every slot of every page filled
since the last minor collection, about 8.5 ns a slot, and is memory-bound: the pages stream
through more memory than the caches hold.

## Options evaluated

- **Card marking for large old containers: done as written ranges (issue #120).** Remembered
  sets are empty on these benchmarks, but a program that keeps pushing new objects into one big
  old list (3M small lists, as in issue #120) rescanned the whole list in every minor
  collection. See "Written ranges" below.
- **Cheaper finalizers: no.** A per-thread cache of small list buffers, so that dying lists
  don't return them to `malloc` one by one, cut `gen`'s sweep from 118 to 111 ms, about 2.5%
  of cycles. Removed as not worth its complexity.
- **Nursery size: helps churn-heavy code, costs memory; not adopted by default.**

  | Nursery | `gen` sweep |
  | --- | --- |
  | 1 MB | 218 ms |
  | 4 MB (default) | 158 ms |
  | 16 MB | 108 ms |
  | 64 MB | 52 ms |

  An adaptive per-thread nursery grew when survival was low and shrank when it was high.

  | Total budget | `gen` cycles | Memory cost |
  | --- | --- | --- |
  | 32 MB | −6% to −13% | `bigheap` +70 MB, `gen` +35 MB, `trees` +16 MB |
  | 16 MB | about −3% (within noise) | +4 to +30 MB |

  `trees` and `bigheap` didn't improve either way, so the default stays a fixed 4 MB.
  `CATX_GC_NURSERY=<bytes>` sets it for workloads that allocate heavily and keep little.

## If minor collections become a bottleneck

The structural fix is to avoid visiting dead objects. That means keeping allocations with
finalizers on their own list, recording per page whether anything in it survived (one bit per
page, set during marking), and resetting pages with no survivors and no old objects as a whole
instead of slot by slot. It needs an address-to-page lookup during marking, so it is a
substantial change to the heap; it would cut `gen`'s sweep roughly in proportion to the fraction
of pages that die entirely.

## Written ranges (issue #120)

A minor collection scans each object in the remembered set. For a list or dict it used to scan
every slot, so a large old container written between collections cost its full size each time.
Now each out-of-line list buffer and dict entry buffer starts with one more word: the range of
slots that may hold a young reference, recorded by the writer. A minor collection scans only that
range and then empties it, because everything it found is old afterwards.

- **Recording.** A store records its slot only if the stored value is a young object and the
  container is old. A new buffer's range covers every slot, so a container is scanned whole the
  first time a minor collection meets it; growing keeps the range (the items keep their indices),
  and a replace or reorder covers what it rewrote. Inline list buffers (at most 7 items) are
  always scanned whole.
- **Cost.** Storing a number or an old object costs one compare more than before, and storing a
  young object into a young container two header-bit tests. Storing a young object into an old
  container also loads and compares the range word, and widens it if needed.
- **Ownership.** Minor collections skip remembered objects their thread no longer owns: one that
  was shared, frozen or handed to another thread had its young referents tenured first.
  Handing an object to another thread clears its remembered bit, so the receiver's write
  barrier adds it to its own remembered set. (Before this change a receiver could miss it.)

| Program (min of 7) | Minor GC time before → after | Wall time before → after |
| --- | --- | --- |
| 3M `[i, i + 1]` pushed into one list | 99 → 17 ms (45 minors) | 0.349 → 0.267 s |
| 1.5M `d[i] = [i, i + 1]` | 32 → 10 ms (20 minors) | 0.185 → 0.159 s |
| 3M overwrites cycling through a 200k-item list | 26 → 20 ms (48 minors) | 0.138 → 0.131 s |
| `bench/gen.cx` | 62–70 → 55 ms (145 minors) | 0.411 → 0.400 s |

Overwrites spread over the whole list widen the range to most of it, so they gain little;
per-block card bits would do better there. `gen` gains because `Page::sweep_young` is no longer
inlined into the minor collection (its loop's speed varied by 40% with unrelated code changes).
