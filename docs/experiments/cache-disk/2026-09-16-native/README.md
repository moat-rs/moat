Native twenty-device methodology
=================================

This comparison drives 20 raw NVMe devices concurrently, using a bounded
64-GiB window on each. It does not fill the devices to capacity. Operational
hostnames, paths, device identifiers, allowlists, and remote-control scripts
are intentionally excluded from public artifacts.

Source and execution
--------------------

- Harness and engines: `78ce9a29b4ddda392b211e76bd647457ebb8a18e`.
- Foyer: `dd46245c45071d1036331e4e2c48e15386017b96`.
- GNU/glibc release build with Rust 1.98.0, target
  `x86_64-unknown-linux-gnu`, and `-C target-cpu=x86-64-v3`.
- Executable SHA-256:
  `276b3af8bb2939b63febf7d00bf843fa5f519429b1e04e05acbe2777d83170c3`.
- Same 20 devices, CPU sets, NUMA placement, queue depths, pool budgets, and
  bounded datasets as the [previous run](../2026-09-16-20disk/README.md).
  The test reserves 20 I/O cores and 40 application cores. No global kernel
  tuning, discard, or full-device preconditioning is performed.
- Exact host, serial/capacity, system-device exclusion, partition, mount,
  holder, signature, open-user, and cooperative-lock checks precede raw writes.

Adapter boundary
----------------

A synchronous workload loop calls the same `Backend` operations: `put`,
`read`, `poll`, `drain`, and `close`. Each device has one persistent caller.
The backend type is selected at startup; native request loops are monomorphized.

V1/v2 own and drive their engine and io_uring queue on the calling I/O thread.
They construct no Tokio runtime and have no per-record channel, task, oneshot,
or completion bridge. Request generation, validation, and pooled-buffer return
stay on that thread. Channels are used only for phase control and aggregate
results. This makes 21 application threads including the coordinator. Linux
may also create io-wq kernel helpers, which appear in the process task list.
The report retains those observed counts instead of calling all tasks application
threads. CPU figures use process `getrusage`; they are not whole-machine CPU usage.

Only foyer constructs a Tokio runtime, with 40 workers on the application
CPU set. One asynchronous driver per disk receives batched requests, calls
HybridCache within the runtime, polls returned read futures, and sends batches
of completions. There is no additional task or oneshot per request in the
adapter. Synchronous callers wait for completions on application cores;
foyer's 20 io_uring threads retain the I/O cores. This gives foyer access to
more CPU resources than the native engines; process CPU costs are included.
The bridge still has batching, scheduling, allocation, and channel costs.

Workload and timing
-------------------

All three use the same deterministic 16-byte keys, initialized values, Xxh3
identity, and weighted rendezvous placement. Identity calculation and routing
happen before timing. Each device reads uniformly from its own assigned keys.
Foyer additionally performs its normal internal full-key hashing and lookup.

| Value | Mean records/device | Total records | Global prefill budget |
|---|---:|---:|---:|
| 100 B | 131,072 | 2,621,440 | 2,560 |
| 1 KiB | 131,072 | 2,621,440 | 2,560 |
| 4 KiB | 130,561 | 2,611,220 | 2,560 |
| 64 KiB | 8,190 | 163,800 | 2,560 |
| 4 MiB | 2,048 | 40,960 | 620 |

The global read concurrency and prefill budgets are divided across devices,
with at most one extra request per device. Read concurrency 160/640/2,560
means exactly 8/32/128 logical requests per device. Queue/pool backpressure
can reduce actual device queue depth; unsubmitted requests are retained.
Latency starts before admission. Accepted requests are drained after the
deadline, and both their operations and drain time enter throughput.

Prefill drains each device's local batch without a cross-device batch barrier.
Timing includes value generation/allocation, copies, checksums, batch drain,
and a final device sync. Formatting, opening, keys, and routing are excluded.
Small engine inputs are contiguous full-key/value envelopes; large prepared
inputs keep the owned key and value separate until copying into the registered
buffer. In-place generation and large-frame batching diagnostics are disabled.
Generation/encoding runs for at most 64 records or about 4 MiB before polling.
A single record can exceed that byte budget. These are bounded prefills,
including very short small-record runs, not sustained write ceilings.

Every inserted key is read and checked before timed reads. Checks include
full key, value length, and both stamped endpoints. V1/v2 read CRC is disabled;
write checksums remain enabled. Foyer retains normal XXHash64 verification,
owned-vector decoding, and possible in-flight read coalescing. Its memory
admission filter rejects entries. These are different exposed read semantics.

Both engines retain registered buffers/files, depth-256 io_uring queues,
2-GiB segment slots, and 512-MiB pools per device with preferred huge pages.
Foyer retains depth-256 io_uring, 16-MiB blocks, two flushers, one reclaimer,
64 index shards, and 512 MiB/device of configured flush buffers. Its read
allocations are separate, so this is not an equal total-memory cap.

Each engine/size has three independently initialized processes and prefills.
The first pass measures 160/640/2,560 clients; two additional passes measure
640 clients. Each level has a two-second warmup and one five-second measured
phase. Engine order rotates across sizes and passes. Thus the headline
640-client medians have three independent samples; the other concurrency
levels have one sample each. All 45 prefills and 75 measured read phases are
retained. Ranges are observed run spread, not confidence intervals.

Reproduction and interpretation
-------------------------------

Use the [harness instructions](../../../../benchmarks/cache-disk/README.md) and an operator-provided
configuration. Source and executable identities above fix the measured code.
The generic analyzer emits per-run CSVs and read summaries; prefill summaries
are medians grouped by engine and size. Run identifiers connect write and read
samples from the same process. Physical activity must be present on all 20
devices in every measured read phase.

The earlier application/Tokio results remain historical comparisons. Native
results also change routing scope, generation placement, concurrency per disk,
copy preparation, and batch barriers. Their gains cannot be attributed solely
to removing a lock or described as changes in either engine implementation.
