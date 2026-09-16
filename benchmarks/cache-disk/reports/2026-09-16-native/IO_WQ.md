Why large reads create io-wq workers
===================================

The native adapter removes Tokio from v1/v2, but it does not prevent Linux
from creating `iou-wrk` helpers. In the first 4-MiB process, the maximum
observed task count was 5,141 for v1 and 5,124 for v2, including 21 application
threads. These are whole-process maxima across prefill, verification, and the
concurrency sweep, not runnable-worker counts or measurements specific to
640 clients. See [runtime observations](runtime.csv).

The tested Linux 6.8 device queues report the same limits on all 20 devices:

| Queue limit | Value |
|---|---:|
| `max_sectors_kb` | 128 |
| `max_hw_sectors_kb` | 128 |
| `max_segments` | 33 |
| `max_segment_size` | 4,294,967,295 bytes |

A 4-MiB benchmark value, including its key and aligned read extent, requires
4,100 KiB. Every retained v1/v2 read phase for this size recorded exactly
33 physical reads per logical operation, averaging 124.242 KiB per device
request. The extra aligned page is not what creates the offload: an exact
4-MiB I/O would also exceed the 128-KiB limit.

Linux first attempts direct reads without blocking. In Linux 6.8,
[`bio_split_rw`](https://github.com/torvalds/linux/blob/v6.8/block/blk-merge.c#L308-L315)
rejects a bio requiring splitting with `EAGAIN` when `REQ_NOWAIT` is set.
io_uring can then retry it through io-wq. Large requests therefore still
trigger offload when registered buffers and huge pages are enabled.

Read-only boundary experiment
----------------------------

A separate single-device probe opens the device read-only with `O_DIRECT`.
It uses the existing common buffer pool, one registered 512-MiB arena, a
registered file, `READ_FIXED`, and a depth-256 ring with `SINGLE_ISSUER` and
`DEFER_TASKRUN`, matching the native queues' relevant configuration. It does
not run the engine, modify data, or contribute to the throughput tables.

Each case runs in a fresh process and issues 64 logical reads, one logical
operation at a time. Split operations submit their disjoint subranges together
and wait for all completions before reusing the buffer. The probe checks CQE
lengths/errors, device read counts/bytes, and worker names after each logical
completion. A separate `RWF_NOWAIT` control exposes `EAGAIN` instead of allowing
the kernel to retry the request through io-wq.

| Logical read | Maximum SQE length | Normal read results | Observed io-wq workers | With `RWF_NOWAIT` |
|---|---|---|---:|---|
| 128 KiB | 128 KiB | 64 successful CQEs | 0 | 64 successful CQEs |
| 132 KiB | 132 KiB | 64 successful CQEs | 1 | 64 `EAGAIN` CQEs; no device reads |
| 4,100 KiB | 4,100 KiB | 64 successful CQEs | 1 | 64 `EAGAIN` CQEs; no device reads |
| 4,100 KiB | 128 KiB | 2,112 successful CQEs | 0 | 2,112 successful CQEs |

Each row was repeated with preferred huge pages and with huge pages disabled;
the outcomes were identical. The observed anonymous huge-page backing was
512 MiB and zero, respectively. All successful 4,100-KiB cases transferred
exactly 64 times 4,100 KiB through 2,112 device reads. The split case changes
where requests are divided, not the physical byte count or device-request
count. [All 16 diagnostic cases](io-wq-probe.csv) retain the observations.
These short runs establish the offload boundary, not a throughput improvement.

Relationship to the earlier investigation
----------------------------------------

[Task Failed Successfully: Saturating NIC and Disk Bandwidth](https://blog.mrcroxx.com/posts/task-failed-successfully-saturating-nic-and-disk-bandwidth/)
separates split-related io-wq offload from the eventual TLB bottleneck. Here,
the offload is reproduced at a much smaller request-size limit, independently
of huge-page backing. Both native engines already use fixed buffers/files;
their main runs observed 10 GiB of anonymous huge pages across the pools, and
their measured reads skip CRC and full-value scans. This investigation does
not include hardware dTLB counters and does not establish a TLB bottleneck.

Worker presence alone also does not establish a throughput bottleneck.
[`IORING_OP_FSYNC`](https://github.com/torvalds/linux/blob/v6.8/io_uring/sync.c#L49-L75)
requires blocking execution in this kernel. Prefill writes can exceed the
read size, and idle helpers can remain after the operation that created them.
Consequently, process-wide worker observations from small-record cases cannot
be attributed to their timed reads without phase-specific evidence.

V2 queue implementation
-----------------------

V2 now obtains the block-device byte limit through `BLKSECTGET` and splits
larger operations inside `UringQueue`. SQEs reference disjoint subranges of the
original allocation, with no payload copies or per-subrequest allocations.
Round-robin submission bounds both logical requests and physical SQEs by the
configured depth. Completion aggregation retains the buffer until every part
finishes, preserves short transfers and I/O errors, and emits one logical
completion. The frame format is unchanged. Tests cover depth-one progress,
mixed request sizes, short/error completions, and draining accepted work on drop.

A further single-device read-only check exercises the actual v2 queue with
automatic limit detection, a depth of 256, and 16 concurrent logical reads.
It reads 64 extents of 4,100 KiB per process and verifies every returned byte
against an independent positional read. Both huge-page settings produce
2,112 device reads, 268,697,600 device bytes, and no observed io-wq workers.
The [native queue observations](io-wq-engine-queue.csv) are functional checks,
not throughput measurements. They use a GNU/glibc release build.

The earlier throughput tables still describe their recorded executable and
do not include this implementation. More SQEs/CQEs change CPU work and the
effective device concurrency; a new controlled comparison is needed to quantify
the performance effect. Simply enabling `RWF_NOWAIT` would instead fail
oversized requests, and limiting worker counts would not remove the cause of
the offload. Filesystem work, segment-count limits, and sync may still offload.
