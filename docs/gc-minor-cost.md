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

## Card bits for large buffers (issue #144)

A buffer of at least 4,096 slots (`CARD_MIN`) also has a card table in front of its range
word: one bit per 128 slots (`CARD`), 4 KB for a 4M-slot list. Its range word is the constant
`CARDED`, which covers no slot, so every recorded store (a young value into an old container)
leaves the inline check and marks its card in the out-of-line barrier, with one or two bit
sets. A minor collection scans only the dirty cards, merged into runs, and clears them.
Smaller buffers keep the single range. Readers still never touch the metadata.

- **Recording** is unchanged otherwise. Numbers and old objects record nothing, and a young
  container records nothing. A new buffer starts with every card dirty. Growing copies the
  cards, or the range when a small buffer grows past `CARD_MIN`. `replace` and in-place
  reorders mark what they rewrote.
- **Cost on the store path.** Storing a number or an old object is unchanged. A young store
  into an old large list was already a call whenever the range had to widen; now it always
  is (in `widen_written`, a load and an or). Marking the card inline in `ListObj::note_store`
  instead made the interpreter's list-set path larger: integer stores went up about 3% in
  instructions, so the card is marked out of line.

Measured October 2026 (Apple M-series, `CATX_GC_THREADS=0`, instructions and cycles from
`time -l`, minimum of three runs). The machine was shared with other builds, so cycles are
noisy and instructions are the steadier figure:

| Program | Instructions | Cycles | Minor mark time (remembered) |
| --- | --- | --- | --- |
| `sparse2`: 300k young stores at random indices of an old 4M-item list, much garbage in between | −33.8% | −30.6% | 260 → 133 ms |
| `sparse`: the same with less garbage per store | −21.8% | −23.2% | 207 → 147 ms |
| `keepset`: 3M overwrites cycling through a 200k-item list | −0.5% | −1.3% | 12.9 → 10.7 ms |
| `setyoung`: 5M young stores into an old 100k-item list | +0.9% | +0.1% | 19.0 → 17.3 ms |
| `keep`: 3M small lists pushed into one list | +0.5% | +1.7% | 10.6 → 10.7 ms |
| `keepdict`: 1.5M `d[i] = [i, i + 1]` | +1.9% | −2.4% | 7.4 → 7.6 ms |
| `setidx` / `setold`: 20M integer stores, young / old list | +0.0% / −0.0% | +0.4% / −0.3% | — |
| `push` / `pushold`: 20M integer pushes | −0.0% / −0.0% | −0.7% / −1.7% | — |
| `dict`: 10× 500k integer inserts | −0.4% | +4.7% | — |

`bench/*.cx` wall times were within noise. Writes scattered over a large old container now
cost about what they touched. Appending and cycling writes, where the single range was
already tight, cost about the same as before.
