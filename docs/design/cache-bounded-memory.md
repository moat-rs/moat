# Cache design for bounded memory and large disks

English | [简体中文](cache-bounded-memory.zh-CN.md)

Status: proposal, 2026-10-08. This document does not describe implemented behavior
or measured performance. It builds on unpublished packed-cache and generic
segment-reclaim prototypes reviewed alongside this proposal. References below
to the current catalog, reclaim behavior, or API inventory describe those
prototypes, not functionality already merged into `main`. They are not included
in this documentation-only change. An earlier September 20 packed-cache
benchmark predates this proposal and cannot validate it.

## Decision and scope

Use a deterministic home bucket as the authoritative directory for every key.
Store small records inline and store larger records in immutable extents, with
their descriptors in the same home bucket. Keep only a budgeted subset of bucket
contents and hot descriptors in RAM. Size transitions update one authoritative
directory instead of requiring a persistent per-key routing map.

This replaces the mandatory in-memory per-KV catalog. It does not automatically
eliminate the Chunk Engine's per-chunk index. There are two delivery stages:

1. Implement and measure buckets over existing chunk operations, admitting only
   a disk population whose complete engine index fits the memory budget.
2. If the required disk population does not fit, add a generic compact or paged
   engine index and checkpoint/replay support. A cache-only change cannot promise
   arbitrary disk capacity, small fixed RAM, and one physical read on every miss.

The cache owns size placement, admission and logical eviction. The engine owns
physical addressing, crash recovery and segment reuse. No cache policy moves
into the engine. The resident cache remains independently budgeted.

## Goals and explicit tradeoffs

- Configure a process memory envelope including recovery and maintenance peaks.
- Avoid reconstructing all KV locations before serving requests.
- Give inline hits one bucket read after physical-index resolution; allow a cold
  external hit to read a bucket followed by its value.
- Preserve full-key comparison, read-after-completed-write behavior, and explicit
  flush durability. Do not silently weaken these contracts for speed.
- Keep steady overwrite/delete workloads bounded in metadata and disk history.

RAM cache hits need no disk access. Physical request count additionally depends
on engine verification, alignment, queue splitting and physical-index residency.
One Cache get or one Store get is not a one-device-I/O guarantee.

Bucket-local eviction replaces exact disk-wide per-KV FIFO/SIEVE. This changes
hit behavior under skew and must be an explicit cache policy/format choice.
There is no claim of equal hit ratio to the current cache.

## Why packing alone is insufficient

The current catalog retains both `slots: Map<Slot>` and `chunks: Map<Pack>`, whose
packs retain entry directories. The engine independently retains chunk locations,
key traversal entries, physical occurrence counts, and segment accounting.
Packing reduces engine record count but leaves cache metadata proportional to
the number of logical KVs. The current directory byte budget is not an RSS cap.

For the proposed design, define:

- `Nb`: number of allocated home buckets, including empty retained buckets.
- `Ne`: indexed external extents, including not-yet-collected extents.
- `Nt`: indexed tombstones and control records.
- `I`: measured effective engine RAM per indexed ID, including table capacity and
  traversal metadata at the configured occupancy.
- `H`: fixed RAM for resident data, bucket cache, descriptor cache, I/O pools,
  in-flight operations, locks, recovery and maintenance scratch.

The first approximation is `M = H + I * (Nb + Ne + Nt) + segment_metadata`.
Allocator slack, resizing peaks and fragmentation require separate headroom.
Measure `I`; `size_of(Location)` alone is not sufficient.

For inline bytes `Ds`, bucket size `B`, and useful occupancy `u`, bucket count is
approximately `Ds / (u * B)`. This is optimistic: directory-only records, skew,
headers and fragmentation can reduce useful occupancy. Illustrative planning
with `I = 128 B`, `u = 0.75`, and 100 TiB of inline payload gives:

| Bucket bytes | Approximate engine index RAM alone |
|---|---:|
| 4 KiB | 4.17 TiB |
| 16 KiB | 1.04 TiB |
| 64 KiB | 266.7 GiB |
| 256 KiB | 66.7 GiB |

These are arithmetic estimates, not measured engine costs. With only 32 GiB
available for this index, 16-KiB buckets cover about 3 TiB of inline payload;
64-KiB buckets cover about 12 TiB, before other index entries and headroom.

External data has different scaling. At 4-MiB extents, 80% useful occupancy and
the same assumed `I`, 100 TiB of payload costs about 4 GiB in extent index RAM,
PLUS its home-bucket directory and all other budgets. A 4-MiB target is legal
only when record headers and directories also fit the configured chunk maximum.

For an illustrative all-external workload with average 16-KiB values and 96-B
directory records (including keys/properties), 100 TiB contains about 6.71 billion
entries and 600 GiB of logical directory bytes. At 75% bucket occupancy and
16-KiB buckets, their engine map costs another 6.25 GiB at the assumed `I`.
The bucket-plus-extent map is then about 10.25 GiB before reserves and `H`.
Long keys, smaller values or lower packing occupancy can change this dramatically.

An exact cold lookup must obtain a physical address somewhere. Keeping the map
in RAM costs capacity; paging it adds I/O; deterministic physical slots require
a different storage allocation/recovery design. Larger buckets reduce map size
by increasing read bytes, update bytes and collision domains.

## Persistent routing and bucket layout

Persist a namespace manifest containing format version, hash seed, bucket count,
device topology, allowed bucket sizes, limits, and ID allocation epochs. Compute
`bucket_number = H(namespace, full_key) mod bucket_count`; derive its stable
ChunkId from a domain-separated namespace and bucket number. Missing chunks
represent empty, never-written buckets. An explicitly emptied bucket is written
as a valid empty bucket image rather than deleted on every churn cycle.

The fixed bucket number never depends on value size. Bucket count and disk
topology are immutable within a namespace generation in the first version.
Changing either requires an explicit rebuild; online rehash is deferred.
Bucket placement uses the existing Store placement rule. Extents may land on
another disk: barriers and capacity accounting must include both owners. Do not
assume that hashing different IDs places them on the same disk.

A bucket image contains namespace/format identity, bucket number, image revision,
length, checksum and a variable-length record directory. Each record stores a
full key, logical version, properties, and either:

- the inline value; or
- an external descriptor: extent ID, offset, length, and expected record identity.

Check the complete identity, lengths and checksum before returning a value.
The external record repeats its key and logical version. A fingerprint match is
only a search accelerator, never proof of equality. Derive an explicit key and
properties length limit from bucket geometry; reject entries whose directory
record cannot fit even without an inline value. Do not silently reduce today's
supported limits during migration.

## Size adaptation without a per-key routing map

Start experiments with bucket allocation classes of 4, 16 and 64 KiB. Each
bucket is still a single chunk; its current length comes from the engine index.
All classes accept variable-length records. These are bucket allocation sizes,
not padded per-entry slots. A stable bucket ID makes growth and shrinkage an
ordinary whole-chunk replacement.

Start with a 1-KiB encoded-record inline threshold and sweep 256 B through 4 KiB.
These are experimental starting points, not chosen production defaults.
Include key, properties and headers in all thresholds.

On insertion, choose inline or external placement from encoded size and the
current policy. Grow a bucket only after reserving disk and write-memory credits;
otherwise evict locally or reject according to admission policy. Do not spill
records into an unbounded chain of overflow buckets. Shrink sparsely occupied
buckets during bounded maintenance, with hysteresis to avoid oscillation.

Pack external records into bounded 1-4 MiB immutable extents, clamped to engine
geometry. Separate a small number of size/observed-update-rate streams if
measurements show less copying during collection. Cap active builders globally;
do not allocate one full builder for every bucket or every possible size.

A slow controller may adjust inline thresholds, allocation-class credits and
coalescing delays using read bytes, write bytes, cache hit rates, bucket occupancy,
eviction rate and queue latency. Apply changes to new writes first. Existing
records remain readable from their home bucket; migrate only with bounded work.
Allocation-class changes do not change the hash function or bucket count.

Increasing a bucket's bytes does not reduce index cardinality. If the engine
index budget is the limiting factor, stop allocating new bucket IDs or reject
the capacity configuration; a placement controller cannot hide that limit.

## Read path and bounded hot state

| State | Data-path reads, before engine-map misses and queue splitting |
|---|---|
| Resident value hit | 0 |
| Cached bucket, inline record | 0 |
| Cold bucket, inline record | 1 bucket read |
| Valid hot external descriptor | 1 extent range read |
| Cold external record | 1 bucket read, then 1 extent range read |
| Unknown key, no trusted negative summary | 1 bucket read |

Use separately budgeted bucket and descriptor caches. A descriptor is valid only
while its associated bucket-coherence token remains valid. Every bucket mutation
invalidates or replaces the token before publishing the new image; discarded
tokens invalidate all their cached descriptors. Engine compaction preserves
extent IDs, while cache relocation explicitly changes descriptors.

Reads register bounded references to external extents while holding the bucket
admission lock, before source deletion can be admitted. A concurrent logical
update may overlap an already admitted read, but a completed update must not be
followed by a new read of its old descriptor. Reader pins and returned buffers
are charged; admission fails or backpressures before pins exhaust memory.

Do not require an all-keys Bloom filter in RAM. At 10 bits per key, ten billion
keys already cost about 11.6 GiB. Optional negative summaries must have an explicit
budget. After restart a summary is UNKNOWN until validated against the exact
current bucket image. UNKNOWN forces a lookup; an old filter must never suppress
a newer key. A missing/corrupt optional summary is not missing application data.

The first implementation should use native verified reads. A later direct-read
mode requires independently verified bucket/record envelopes and a documented
corruption model. Even a 4-KiB bucket may span two physical pages if the engine
places its value at an unaligned offset; measure read extents instead of assuming
one aligned page. Do not turn off integrity checks merely to meet an I/O slogan.

## Mutation, visibility and crash ordering

Serialize read-modify-write operations by bucket using a fixed-size striped
coordinator. Operations for different keys in one bucket must not overwrite
each other's updates. Bound queued jobs and bytes globally. Coalesce compatible
mutations already admitted to the same bucket with a bounded delay; uniform
random writes across billions of buckets offer little coalescing opportunity.

Inline update/delete needs one replacement bucket image. The engine resolves
images by its version order. A persisted removal is represented by absence in
the newest complete image, including an empty image. Never rebuild KV state by
replaying arbitrary older bucket payloads.

External insertion or replacement uses a reference-publication protocol:

1. Allocate a never-reused extent identity and write the complete extent.
2. Persist its data with a successful barrier on every participating extent disk.
3. Under bucket serialization, publish images containing the new descriptors.
4. Complete visibility replies after bucket writes complete. An explicit Cache
   flush subsequently persists these bucket images before reporting durability.

Batch step 2 across many extents to amortize synchronization. It remains a real
barrier before publishing references, even if the caller has not requested a
flush. A crash can expose an unflushed bucket image; that image must not point to
an extent that never became durable. Mere write completion is insufficient.

Transitioning inline-to-external or external-to-inline changes the same bucket
record. There are no two competing size-specific key directories. Old external
bytes are collected only after the replacing/removing bucket image is durable.
Deletion need not synchronously reclaim those bytes.

Reserve ID sequence ranges durably before use and abandon unused IDs after a
crash. Include a namespace epoch and type tag; never reuse an orphan's identity.
Allocate logical record versions from an equally crash-safe monotonic sequence;
relocation preserves them, while a new logical mutation receives a new version.
The manifest update protocol needs checksummed generations and copy-before-root
ordering. A malformed committed root is an error, not permission to reformat.

## Collection without a full per-KV RAM catalog

Keep bounded per-extent summaries or page them through a fixed cache. Summaries
help choose candidates but are not authoritative proof that an extent is dead.
Scan one candidate's on-disk record directory, group entries by home bucket, and
compare each against the current bucket record under its coordinator. Stream
this work within scratch limits; do not collect all live descriptors in RAM.

Copy survivors to new extents, persist them, then conditionally publish new
descriptors only if key/version/source still match. Persist all changed buckets,
wait for readers of the old extent, and only then delete the source extent.
Once sealed, no foreground write may create a new reference to an old extent;
this invariant makes completed per-record checks meaningful. Cache relocation
and foreground mutation use the same serialization and pin protocol.

Crashes before publication leave orphan destination data. Crashes after partial
publication leave both extents, with buckets deciding which record is current.
Do not automatically promote orphan records into the directory. A resumable
background sweep can test extent records against their home buckets and reclaim
unreferenced records. A maintenance journal may accelerate this sweep but must
not be the only way to find leaked data. Budget orphan space and stop admitting
writes if cleanup cannot keep up.

Cache collection and generic engine reclaim are distinct write amplifiers.
Measure both. The engine must have guaranteed relocation reserve, and cache
maintenance must have reserved output space and memory. Foreground admission
stops before consuming either reserve. A no-progress pass returns pressure;
it must not loop indefinitely or delete a still-referenced live extent.

## Recovery and readiness

There are two separate recovery costs. Removing the cache's KV scan only solves
the second one:

1. **Engine recovery:** current code visits physical slots, restores sealed
   footer metadata, and scans active/unusable-footer allocations. It rebuilds
   the physical chunk index and occurrence counts before serving data. Repeated
   bucket overwrites can leave many historical records in unretired segments.
2. **Cache recovery:** after the engine is ready, read the namespace manifest,
   validate topology/format/budgets and serve bucket lookups lazily. No mandatory
   enumeration or payload read of every bucket, extent, or logical key.

Current `Store::new` materializes the full live inventory, and starts/waits for
owners sequentially. Add an open-without-inventory path and a bounded streaming
inventory interface, plus bounded parallel device opening. The engine already
has a batch visitor that can support the adapter. Keep GC from invalidating a
cursor while a bounded inventory step is in progress, or restart safely.

Report time to first usable disk, all disks ready, cold serving, and warm hit
rate separately. Do not label an empty replacement cache as recovered.
Recovery of a corrupt authoritative bucket returns a defined corruption error;
it is not silently treated as an ordinary miss.

For fast recovery at large capacity, generic engine checkpoints must eventually
include chunk locations/tombstones, generation and LSN watermarks, occurrence
counts, segment live-byte accounting, and retirement/reuse state. A location-only
checkpoint is insufficient for safe reclaim. Changes after a checkpoint include
relocation and retirement, not merely foreground writes.

Publish a checksummed checkpoint root only after checkpoint data is durable;
retain replay dependencies until the new root is durable. Bound replay bytes
with a persisted progress limit and throttle mutations when checkpointing falls
behind. Validate referenced segment generations to prevent reuse/ABA errors.
This is a separate engine design prerequisite, not an existing API capability.

An eager checkpoint still reads and allocates the whole index. For a paged index,
persist a small root plus an immutable/checksummed directory, replay a bounded
delta, and load index pages through an explicit RAM budget. A cold map miss adds
metadata I/O. Background warming must share the same budget and I/O limiter.

## Memory and capacity admission

The configuration planner must account for:

| Budget | Includes |
|---|---|
| Engine | Live IDs, tombstones, traversal, segment arrays, checkpoint/replay state |
| Resident data | Entries, metadata and externally retained versions |
| Bucket/descriptor cache | Allocations, table capacity and coherence tokens |
| I/O | Per-disk pools, registered memory, queued writes and held read buffers |
| Foreground staging | Extent builders, bucket images, waiters and completion state |
| Maintenance/recovery | Source/destination buffers, streamed directories and replay |
| Headroom | Allocator slack, temporary table growth and runtime overhead |

First-version admission fixes a maximum indexed-ID population including a
tombstone/orphan reserve. Reserve credits before allocation and release them
only after the engine has actually forgotten the ID. Engine index limits must
be configured consistently; the current default of 1,048,576 entries is not a
large-disk capacity plan.

Use pre-sized structures or account for old-plus-new allocations during resizing.
All stages acquire both operation and byte credits. Share references to charged
buffers without double-counting, but keep their charge until the last holder
releases them. Public handles cannot create unbounded uncharged allocations.
An overall RSS target needs measured headroom; logical byte counters alone are
not a hard RSS guarantee.

At startup, emit a capacity estimate for the declared size/key distribution and
a worst-case entry-count bound. Validate it during operation. If the requested
capacity and memory cannot coexist, return a configuration error or an explicit
lower usable-capacity offer; do not silently exceed RAM or imply full disk use.

## Alternatives and recommendation

| Design | Memory | Cold reads | Mutation/recovery implications |
|---|---|---|---|
| Current packed catalog | Per KV plus per chunk | Usually one data extent | Existing implementation; eager KV directory recovery |
| Home buckets over current engine | Per bucket/extent plus bounded hot state | Inline bucket; external bucket then value | Minimal engine semantics changes; engine map still limits capacity |
| Home buckets with paged engine map | Bounded map cache plus roots/delta | May add map-page reads | Generic engine work; bounded-replay recovery can avoid eager index load |
| Fixed-address bucket page arena | Arithmetic addressing, bounded hot state | One bucket extent | Separate generic page storage, atomic replacement/WAL and recovery design required |

Recommend the second option for a measured prototype, with the third as the
target when capacity arithmetic rejects the existing map. Compact dense engine
tables may bridge moderate deployments but remain proportional to bucket count.
Fixed-address pages resemble BigHash more closely and deserve consideration if
single-I/O cold small reads are mandatory at an index size that cannot fit RAM.
They are not implemented by calling `Store::put` on a stable ChunkId: that still
uses an append log and a RAM physical-address map.

Do not group hundreds of independently mutable buckets into one large chunk
merely to shrink the index. The current engine cannot update a range: every
change rewrites the whole chunk. Likewise, persisting small deltas without a
bounded lookup and consolidation design trades index savings for read chains
and recovery debt.

## Measurements and delivery gates

1. Measure effective engine bytes/ID, growth peaks, physical bucket read/write
   sizes, clean/unclean recovery bytes and replay rate. Build a calculator from
   measured values and declared capacity/memory targets.
2. Prototype a fixed bucket count with static inline threshold over existing
   engine APIs. Remove the mandatory KV catalog and startup inventory; implement
   coherence, publication barriers, empty images and bounded external collection.
3. Add bucket size adaptation and a few external streams only after the static
   design passes crash and sustained-churn tests. Compare against the simpler
   configuration using identical RAM, CPU, disk and durability budgets.
4. Implement generic index/checkpoint changes only with separate failure-model
   review and measurable capacity/recovery benefit. Re-run end-to-end tests.

The benchmark matrix must include 100 B through 4 MiB, real key/property lengths,
tiny-heavy and byte-heavy mixtures, uniform/Zipf access, hot/cold phase changes,
overwrite/delete churn, size transitions on the same key, negative lookups,
near-full disks, and collection running concurrently with foreground traffic.
Use identical traces/seeds and report object and byte hit rates as well as speed.

Compare the current packed cache, static buckets, adaptive buckets, and, where
practical, a pinned CacheLib Navy baseline. Align semantics explicitly: do not
equate CacheLib buffering acknowledgements with Moat flush durability. Separate
native integrity verification from any cache-envelope-only experiment.

Run enough written bytes to turn over usable capacity several times and show
stable live bytes, history bytes, RAM and free segments. Measure physical reads
per operation, device bytes, write amplification including both collectors,
CPU, RSS, pinned memory, p50/p99/p99.9, backlog and reserve exhaustion. Two short
fill/read runs are not evidence of steady-state behavior.

Recovery cases include clean close, process kill, crash at each publication/
retirement boundary, missing optional summaries, corrupt committed metadata,
checkpoint failure, and bounded orphan accumulation. Track first-service and
full-readiness time, bytes read, peak RAM and foreground latency during warming.
Acceptance requires no stale-value resurrection, no freed referenced extent,
bounded resource use, and explicit backpressure under overload. Performance and
recovery SLO numbers remain deployment inputs, not unmeasured promises.

## References

- [CacheLib Navy overview](https://cachelib.org/docs/Cache_Library_Architecture_Guide/navy_overview/): size-based engine selection and memory tradeoffs.
- [CacheLib small object cache](https://cachelib.org/docs/Cache_Library_Architecture_Guide/small_object_cache/): deterministic buckets, checksums, FIFO and optional filters.
- [CacheLib large object cache](https://cachelib.org/docs/Cache_Library_Architecture_Guide/large_object_cache/): indexed append regions and persistence.
- [Engine guide](../../core/moat-engine/README.md) and [resident-cache ownership](cache-memory.md): merged implementation background; proposed and prototype extensions are described explicitly above.
