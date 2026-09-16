# Development guide

## Repository layout

| Path | Contents |
|---|---|
| `core/moat-common` | Identifiers, checksums, memory alignment, and buffer pools |
| `core/moat-engine` | Single-disk log storage, indexing, recovery, and reclaim |
| `core/moat-engine-v2` | Independent unified-frame engine with a single-owner I/O pipeline |
| `core/moat-server` | Disk discovery, placement, and workers |
| `core/moat-cache-memory` | Resident cache and eviction policies |
| `core/moat-cache-store` | Async engine adapter, request budgets, and read coalescing |
| `core/moat-cache` | Memory and disk cache coordination, key identity, and data views |
| `benchmarks` | Active comparison drivers and reproduction instructions |
| `xtask` | Development checks invoked through `cargo x` |
| `docs/design` | Designs, API contracts, and implementation audits |
| `docs/experiments` | Archived optimization decisions, measurements, profiles, and complete numeric samples |
| `etc/logo` | Project logo assets |

Start with each crate's `src/lib.rs` for its public API. Its `examples` show
usage, `tests` cover behavior across modules, and `benches` contain benchmarks
for that crate. Comparison projects under `benchmarks` have separate manifests
and run instructions; they are not included in the root workspace tests.

## Reading the storage engine

For the new implementation, start with the [v2 crate guide](../core/moat-engine-v2/README.md)
and [experiment archive](experiments/README.md). The sequence below describes
the existing engine used by current consumers.

1. [Engine API](../core/moat-engine/src/lib.rs) and
   [local example](../core/moat-engine/examples/local.rs): how queues, engines,
   and read/write pipelines fit together.
2. [Disk layout](../core/moat-engine/src/layout.rs) and
   [open and recovery](../core/moat-engine/src/engine.rs): how persisted data
   maps to in-memory state.
3. [Write path](../core/moat-engine/src/writer.rs): request admission, submission
   ordering, and completion handling.
4. [Read path](../core/moat-engine/src/reader.rs) and
   [index](../core/moat-engine/src/index.rs): record lookup, data reads, and
   concurrent access constraints.
5. [Engine integration tests](../core/moat-engine/tests/engine.rs): edge cases
   covered by crash injection, out-of-order completions, reclaim, and model tests.

The write implementation is split by responsibility, with state owned by a
single `Writer`:

| File | Responsibility |
|---|---|
| [writer.rs](../core/moat-engine/src/writer.rs) | Write admission, batch submission, index updates, and auxiliary I/O completion dispatch |
| [writer/types.rs](../core/moat-engine/src/writer/types.rs) | Write options, tickets, completions, and large-value buffers |
| [writer/batch.rs](../core/moat-engine/src/writer/batch.rs) | Small-record packing and inline/framed batch encoding |
| [writer/sealing.rs](../core/moat-engine/src/writer/sealing.rs) | Segment allocation, sealing, and durability barriers |
| [writer/reclaim.rs](../core/moat-engine/src/writer/reclaim.rs) | Victim selection, window scanning, and live-record relocation |

## Design documents

| Document | Topic |
|---|---|
| [Chunkserver design](design/chunkserver.md) | Overall architecture and goals |
| [Chunkserver audit](design/chunkserver-audit.md) | Design review and constraints |
| [Engine API](design/engine-api.md) | Queue and read/write pipeline interfaces |
| [Unified Frame layout proposal](design/engine-frame-layout.md) | Immutable frames, mixed-value placement, read/write paths, recovery, and tradeoffs |
| [Earlier engine write layout proposal](design/engine-write-layout.md) | Open inline pages, unified value extents, and a recovery validation model |
| [Memory cache](design/cache-memory.md) | Resident cache, shared handles, and eviction policies |
| [Disk cache adapter](design/cache-store.md) | Async requests, budgets, and read coalescing |
| [Hybrid cache](design/cache-hybrid.md) | Stable key identity, disk catalog, and conditional population |
| [Cache views](design/cache-views.md) | Zero-copy views and retention limits |
| [Cache implementation plan](design/cache-implementation.md) | Implementation stages and acceptance criteria |

## Validating changes

Run `cargo x` from the repository root for the full development checks; see the
[project README](../README.md#development) for individual commands. For Rust
changes, start with:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

On Linux, engine tests exercise file-backed io_uring paths and require enough
locked memory. See the project README for limits and platform-specific setup.
