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

- **Card marking for large old containers: not needed.** Remembered sets are empty on every
  benchmark, because writes into old objects between minor collections are rare. Revisit if a
  workload repeatedly mutates a large old list or dict and its minor-collection marks show it.
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
