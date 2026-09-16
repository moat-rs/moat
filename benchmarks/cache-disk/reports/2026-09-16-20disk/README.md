Twenty-device comparison methodology
===================================

This run compares foyer with the current v1 and v2 engines on **20 concurrent
raw NVMe devices**. It does not fill the devices to capacity. It supersedes
neither the historical full-cache comparison nor the separate single-device
complete-fill experiment: those measure different integration layers and scopes.

Source and execution
--------------------

- Harness and engine source: `a446e72d7d9b5989ba378623ecffece7fead939e`.
- Foyer: pinned `dd46245c45071d1036331e4e2c48e15386017b96`.
- Release build: `x86_64-unknown-linux-gnu`, glibc, `-C target-cpu=x86-64-v3`.
- Executable SHA-256: `3e54d638c074e807124afd41a501f79143e15e7b512fdbde407c6c8f2752fc26`.
- One AMD EPYC 9A85 socket, 96 physical cores / 192 hardware threads, two NUMA
  nodes; Linux 6.8. The test assigns 40 physical application cores and 20
  separate physical I/O cores, one per device. I/O workers are placed on the
  device's NUMA node. The coordinator shares the first application core, fixing
  first-touch placement of the generated keys. Application work may cross NUMA nodes.
- Twenty 30.7-TB NVMe devices, each restricted to its first 64 GiB. System
  devices are excluded. The run uses no discard, full-device preconditioning,
  filesystem cache, or global kernel tuning.
- Exact host, capacity and serial checks, mount/partition/holder/signature
  checks, open-user checks, and cooperative device locks precede raw writes.
  Machine-specific operational inputs are intentionally not published.

Matched workload
----------------

Each process uses all 20 devices. All engines receive the same deterministic
16-byte keys, generated values, identity hash, weighted rendezvous placement,
CPU assignments, window sizes, and logical client counts. Memory admission is
disabled for foyer. V1/v2 have no resident value cache. Every inserted key is
read back, with its length and value endpoints checked, before measurement.
V1/v2 additionally compare the full stored key on every read; foyer performs
its normal key lookup and verification. Timed reads choose keys uniformly.

| Value | Records per device (mean) | Total records | Prefill batch |
|---|---:|---:|---:|
| 100 B | 131,072 | 2,621,440 | 2,560 |
| 1 KiB | 131,072 | 2,621,440 | 2,560 |
| 4 KiB | 130,561 | 2,611,220 | 2,560 |
| 64 KiB | 8,190 | 163,800 | 2,560 |
| 4 MiB | 2,048 | 40,960 | 620 |

Record counts per device are expected means, not exact placement quotas. The
4-MiB dataset totals 160 GiB of values; smaller workloads are bounded by the
record-count and approximately 512-MiB-per-device sizing rule. Values are
uniform within each run; there is no mixed-size or overwrite/churn workload.

Global concurrency is 160, 640, or 2,560 clients (mean 8, 32, or 128 per device).
These are application requests, not measured NVMe queue depths. Each level has
a two-second warmup and three five-second measured phases. Outstanding requests
are drained before reporting elapsed time. Reports retain each sample and all
concurrency levels. Every measured phase must show physical reads on every
one of the 20 devices. Kernel sector counts use 512 bytes per sector.

For each size, the engine order rotates: foyer/v1/v2, v1/v2/foyer,
v2/foyer/v1, foyer/v1/v2, v1/v2/foyer. Each engine/size has three independent
prefill measurements. Two supplementary
passes recreate and verify the dataset, rotating the engine order again; their
read phases recheck 640 clients for five seconds after a two-second warmup.
The headline 640-client result is the median across three independently
initialized datasets: the median of the initial three phases, then one phase
from each additional dataset. Other concurrency levels use the initial three
phases only. Both the original sweep and independent rechecks are published.
These are short measurements,
not independent long-duration device steady states or confidence intervals.

Implementation differences
--------------------------

V1/v2 use the same benchmark-only request adapter: one owning worker per disk,
request channels and one-shot replies, identical full-key/value envelopes, and
zero-copy pooled read results. This bypasses `moat-cache` coordination,
admission, eviction and reclamation; **v1 is not the historical `moat-cache`
benchmark mode**. V2 remains independent of the old engine in production code.

Both engines use 2-GiB segment slots, depth-256 io_uring queues, registered
buffers and files, and a 512-MiB buffer pool per device with preferred huge
pages. Foyer uses its normal HybridCache path, depth-256 io_uring, 16-MiB
blocks, two flushers, one reclaimer, 64 index shards, and a configured 512-MiB
flush-buffer budget per device. Foyer allocates read buffers separately; this
is not a cap on its total read memory. It is not the same allocator, resident
allocation, or memory bound as the engine pool. No compression is enabled.

Engine reads use `verify = false`; foyer retains normal XXHash64 checks and
owned-vector decoding. Both engine write paths retain their checksums. Foyer
may coalesce concurrent requests for the same key; the engine adapter does
not. Consequently this is a comparison of the exposed paths and their costs,
not a checksum-normalized or ownership-normalized engine microbenchmark.

The pinned foyer builder combines `O_DIRECT` and `O_NOATIME`. The harness
sets `O_DIRECT` on the device's shared descriptor through a zero-length
partition before cache initialization, avoiding the ownership requirement of
`O_NOATIME`. No data-path implementation in foyer is patched.

Each prefill batch uses at most 40 application tasks, with concurrent record
futures inside each task; it does not spawn one Tokio task per record.
Prefill throughput includes value generation, allocation, copies, routing,
checksums, completion delivery, per-batch drain barriers and a final device
sync. Formatting and opening are excluded. The reduced 4-MiB batch keeps
foyer's asynchronous insertion within its buffer budget; complete read-back
verification prevents rejected inserts from inflating results. No per-write
fsync guarantee or sustained device-write ceiling is implied.

An earlier diagnostic attempt left the coordinator unpinned. Its results are
excluded from this matrix because key-memory placement could vary between
processes. All three implementations were rerun after fixing the affinity.
