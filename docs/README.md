# Development guide

## Repository layout

| Path | Contents |
|---|---|
| `core/moat-common` | Identifiers, checksums, memory alignment, and buffer pools |
| `core/moat-engine` | Sole frame engine, owner-driven I/O, allocation, and recovery |
| `core/moat-server` | Device discovery, routing, session adaptation, and exclusive workers |
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

Start with the [engine guide](../core/moat-engine/README.md) and [migration boundaries](design/engine-migration.md):

1. [Frame codecs](../core/moat-engine/src/frame/mod.rs) and the [segment format](design/engine-segment-format.md).
2. [Pipeline](../core/moat-engine/src/pipeline/mod.rs): requests, buffer ownership, and completion ordering.
3. [Device engine](../core/moat-engine/src/engine/mod.rs): geometry, recovery, segment routing, and rollover.
4. [Application adapter](../core/moat-server/src/storage/mod.rs): exclusive leases, queues/pools, LSNs, and request conversion.
5. [Cache-store worker](../core/moat-cache-store/src/worker.rs): per-key ordering, read coalescing, fences, and shutdown.

The v1 designs and experiments explain historical decisions. The engine and the migration guide define the current format and interfaces.

## Design documents

| Document | Topic |
|---|---|
| [Chunkserver design](design/chunkserver.md) | Overall architecture and goals |
| [Chunkserver audit](design/chunkserver-audit.md) | Design review and constraints |
| [Engine migration guide](design/engine-migration.md) | Current interfaces, transition boundaries, missing capabilities, and rebuild priorities |
| [Historical engine API](design/engine-api.md) | Removed v1 shared-queue design |
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
