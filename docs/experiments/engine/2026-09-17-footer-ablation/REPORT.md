# Footer recovery ablation and PR cleanup

The initial PR added 9,565 lines, including 7,814 lines of fill progress.
Remove the full-device runner and historical baseline attachments from this PR;
retain the format change, fault coverage, and this compact decision record.
The previous material remains available in commit `e1e24b3`.

## Performance decisions

The control is `e1e24b3`. Each treatment removes one optimization. Positive
percentages mean removing it increased elapsed time. Results are medians of three
paired comparisons; brackets give the observed range, not a confidence interval.
[Pair summaries](pairs.csv) retain all comparisons, including unfavorable results.

| Removed optimization | Workload | Time change | Decision |
| --- | --- | ---: | --- |
| Tail-page reuse | Small footer | +6.99% [+6.68, +7.01] | Keep: removal adds one read and allocation per segment, and 50% more segment metadata bytes. |
| Tail-page reuse | Large footer | -5.56% [-6.39, -4.96] | Keep to reduce read bytes; no cached-latency improvement is claimed. Removal rereads 4 KiB per segment. |
| Route capacity reservation | Small footer | -13.12% [-15.15, -12.95] | Remove the up-front reservation and extra constructor parameter. |
| Route capacity reservation | Large footer | -2.08% [-5.02, -0.73] | Same decision; reservation did not demonstrate a speed benefit. |
| Route capacity reservation | Sparse device | +0.76% [+0.01, +1.45] | Remove: timing difference is small, while reservation requests 1,048,320 unnecessary bytes in this case. |
| Unordered metadata reservation | Ordered metadata | -0.08% [-0.26, +0.01] | Keep: this path remains allocation-free. |
| Unordered metadata reservation | Unordered metadata | +67.29% [+67.10, +76.76] | Keep: removal introduces eight reallocations for 1,024 values. |

Small-footer recovery reads 134,225,920 bytes in 32,770 application calls; without
reuse it reads 201,334,784 bytes in 49,154 calls and allocates 16,384 extra buffers.
Large-footer recovery reads 41,951,232 bytes in 6,146 calls; without reuse it reads
50,339,840 bytes in the same call count. These are application counters, not
block-device statistics.

The unordered decoder requests one 8,192-byte range vector. Without reservation,
one allocation plus eight reallocations cumulatively request 16,352 bytes.
Full-device route growth now performs twelve reallocations in the small workload;
that tradeoff avoids reserving for unused slots and was faster in these runs.
Allocation byte totals are cumulative requests, not RSS or peak live memory.

The final implementation was built separately after removing the route parameter
and compared with fresh controls in three further blocks of seven samples:

| Workload | Final elapsed-time change |
| --- | ---: |
| Small footer | -13.79% [-14.55, -12.87] |
| Large footer | -1.50% [-4.11, -1.38] |
| Sparse device | -1.14% [-1.24, +2.08] |

## Method and limits

- Recovery fixtures use fresh sparse temporary files, 64 KiB slots, one sealed
  frame per allocated slot, one-byte values, distinct keys, and a 32 KiB frame
  limit. Small: 16,384 allocated slots with one record each and 4 KiB footers.
  Large: 2,048 allocated slots with 180 records each and 16 KiB footers.
  Sparse: 16,384 slots, only one allocated, containing one record.
- Metadata fixtures contain 1,024 eight-byte values. The unordered case reverses
  value offsets and repairs metadata/header CRCs while preserving nonoverlap.
  Each timing sample averages 10,000 validated decodes.
- Release binaries run serially on one pinned CPU. Recovery measures only
  `Engine::open`, excluding fixture creation, queue setup, and engine destruction.
  Each process performs an allocation-counting warmup and a second warmup before
  five timing samples. Metadata allocation counting is separate from timing.
  Allocation counters are disabled during timing. Record and allocation counts
  are asserted for every recovered fixture.
- Each block brackets treatments with controls, using the mean of the two
  control medians as its denominator. Treatment order is shuffled with a fixed
  seed. These are buffered, cache-resident file and CPU measurements. They do not
  predict full-device O_DIRECT latency, concurrent recovery, or physical I/O ops.
- The final comparison changes only route reservation. No explanation involving
  allocator placement or CPU caches is established by these results.

## Safety decisions

Allocation validation and generation matching remain correctness requirements.
In isolated mutations, bypassing allocation validation fails
`valid_footer_cannot_hide_a_corrupt_allocation_header`; accepting a stale footer
fails `older_footer_is_ignored_after_allocation_generation_changes`.
Neither mutation is retained. Trailer/full-footer checksums, persistence ordering,
sealed scan boundaries, and fault tests remain intact.
