CPU and waiting-path observations
================================

These are separate diagnostic runs of the same executable as the throughput
matrix. Their throughput is excluded from all comparison tables. V1/v2 were
sampled with 100-B, 4-KiB, 64-KiB and 4-MiB values; foyer was sampled with
4-MiB values. Reads use 640 global clients and an eight-second measured phase.

The process was launched under `perf record -e cycles:u -F 99 --clockid mono
--call-graph dwarf,8192 -- BINARY CONFIG.json`, so application workers and I/O
workers inherit sampling. Write and read reports use separate monotonic-time
windows derived from the harness's elapsed time and output markers. An earlier
attach-based attempt missed application threads; those profiles are excluded.
All final reports record zero lost samples. Short write profiles have fewer
samples than read profiles and should be interpreted as hotspot evidence.

[Selected hotspots](profile-hotspots.csv) contain self-cycle percentages, not
wall-clock fractions. Copy/fill groups sum reported leaf sites whose call
chains identify `copy_nonoverlapping` or `write_bytes`; sites below the report's
0.5% cutoff may be omitted. Symbols without sufficient attribution are not
assigned to a category. Percentages are not comparable measures of total work
between implementations and cannot measure time asleep or device waits.

Request coordination limits small-record reads
----------------------------------------------

V2's 4-KiB read profile attributes 24.43% of sampled cycles to
`std::sys::sync::mutex::futex::Mutex::lock_contended` and another 3.71% to
Tokio's `push_remote_task`. Call chains reach the Tokio scheduler's shared
queue from engine-worker one-shot completion delivery. At 64 KiB, the mutex
site accounts for approximately 24% in both v1 and v2.

These locks are in the benchmark's cross-thread request/completion path. They
do not demonstrate a shared index or mutex inside the v2 pipeline. Increasing
client count cannot remove this coordination cost. Batching completion
notification or arranging local execution is a concrete next experiment; it
must be applied to both engine adapters before another comparison.

Writes include substantial allocation and copying
--------------------------------------------------

| Profile | Selected copy sites | Selected fill sites | CRC32C SIMD site |
|---|---:|---:|---:|
| V1, 64 KiB | 60.81% | 15.37% | 0.93% |
| V2, 64 KiB | 56.87% | 21.96% | 0.86% |
| V1, 4 MiB | 63.85% | 17.44% | 13.00% |
| V2, 4 MiB | 64.33% | 17.44% | 12.50% |

The stacks identify generated-value initialization, construction of the
full-key/value envelope, and copying that envelope into the registered I/O
buffer. `PreparedFrame` avoids an additional copy inside v2, but this adapter
still copies the incoming heap value into that prepared buffer. The current
multi-device prefill is consequently not a direct-in-place producer test or
a device-write bandwidth ceiling. Producing data in its final buffer would
remove work that exists in both adapters; the improvement has not been measured.

For 4-KiB writes, `malloc` alone accounts for 9.77% of sampled cycles in v1 and
13.83% in v2. Allocation, wakeup and checksum costs are all visible. These
profiles motivate reusing batching storage and reducing per-record completion
work, but do not isolate a single cause of the remaining 1-KiB/4-KiB regression.
There is no 1-KiB CPU profile in this set. Read CRC is already disabled for both
engines; eliminating it again cannot improve this read path.

Foyer's high-concurrency large-read behavior
-------------------------------------------

Foyer reaches 84.0 GiB/s at 160 clients in the initial sweep. At 640 clients,
independently recreated datasets produce 15.9–26.4 GiB/s. The higher-concurrency
case must not be substituted for its best measured throughput without labeling
the concurrency and preserving both results.

In the complete-process 4-MiB read profile, `Checksummer::checksum64` accounts
for 40.96% of sampled cycles and identified copying sites for 42.46%. Foyer
retains its default integrity checks and decodes into an owned value vector.
V1/v2 skip read CRC and return pooled buffers, so this comparison includes real
integrity and ownership differences.

Independent thread-state snapshots during that profile also observe
`vm_mmap_pgoff`, `__vm_munmap`, `lock_mm_and_find_vma`, and
`__gup_longterm_locked` waits. The pinned foyer source allocates an `IoSliceMut`
for each load; its configured buffer pool is for flushing, not for reads.
These observations point to allocation/mapping and page-pinning pressure in
addition to checksum/copy work. They do not isolate how much of the throughput
collapse comes from each cause. Changing the allocator, pooling/registering
read buffers, and concurrency tuning would require separate controlled runs.
