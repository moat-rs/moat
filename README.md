<p align="center">
    <img src="https://raw.githubusercontent.com/moat-rs/moat/main/etc/logo/slogan.svg" />
</p>

# moat

A chunkserver built for machines with many large NVMe drives.

***Work in progress. The on-disk format and APIs are not stable yet.***

moat stores immutable, variable-length chunks (from a few bytes up to a
configurable maximum, 4 MiB by default) on raw NVMe devices, with one
independent log-structured engine per disk, an RDMA data path, and a client
library that stripes arbitrarily large objects across disks and nodes. It is
designed to be correct first, fast second, and as small as those two allow.

The design document lives at [`docs/design/chunkserver.md`](docs/design/chunkserver.md).

## Current status

| Crate | Responsibility | Status |
|---|---|---|
| [`moat-common`](core/moat-common) | Chunk IDs, CRC32C, aligned memory, and buffer pools | Available |
| [`moat-engine`](core/moat-engine) | Frame/segment codecs, owner-driven I/O, allocation, recovery, and indexing | Sole engine; append-only, without physical reclamation or segment reuse |
| [`moat-server`](core/moat-server) | Device discovery, routing, exclusive sessions, and workers | Exclusive engine sessions; no network transport yet |
| [`moat-cache-memory`](core/moat-cache-memory) | Sharded memory cache and replacement policies | Available |
| [`moat-cache-store`](core/moat-cache-store) | Bounded async requests, per-key ordering, and read coalescing | Available |
| [`moat-cache`](core/moat-cache) | Hybrid cache, key identity, catalog, and shared views | Writes stop when append headroom is exhausted |
| `moat-transport`, `moat-client`, `moat-tools` | Networking, clients, and operational tools | Planned |

The v1 implementation and entry points have been removed. The engine does not read the
old disk format; existing v1 data requires a separate migration or rebuild.
The upper layers use a replaceable adapter in
[`moat-server::storage`](core/moat-server/src/storage/mod.rs). See the
[engine migration guide](docs/design/engine-migration.md) for API changes, current
limitations, and future rebuild boundaries.

## Examples

```sh
cargo run -p moat-cache-store --example chunks
cargo run -p moat-cache --example hybrid
cargo run -p moat-cache --example views
cargo run -p moat-engine --example segment_io -- /path/to/new-example.img
```

The first three examples use memory devices. `segment_io` creates a new file
and demonstrates frame-pipeline writes, recovery, and reads. See the
[engine guide](core/moat-engine/README.md) for the complete device API.

Each disk has one owner for its engine, queue, and pool. `Auto` selects
io_uring on Linux and a synchronous queue on other Unix platforms; memory
devices must explicitly select `Sync`. Synchronous I/O runs on the calling
thread for local development and tests. Read CRC verification is disabled
by default and can be enabled with `storage::Options::verify_reads`; writes
and recovery retain checksum validation.

Shared cache views remain readable after eviction, overwrite, and shutdown.
The last holder releases their buffers and credits. Logical deletion and
eviction do not free physical segments; `reclaim` explicitly returns
unsupported. Sustained overwrite workloads require reclamation and reuse.

## Validation and benchmarks

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo bench -p moat-engine --bench frame
```

Linux tests include real io_uring and registered-buffer paths. Allow at least
64 MiB of locked memory for tests; CI configures this limit. Benchmark memory
limits must accommodate the configured pool capacity per disk.

The [engine benchmark](benchmarks/engine/README.md) runs the engine directly;
[disk comparisons](benchmarks/cache-disk/README.md) compare moat with a pinned
foyer revision; [memory comparisons](benchmarks/cache-memory/README.md)
measure resident cache policies. Direct-device tests overwrite the configured
window and require explicitly assigned disposable devices.

The server `node` benchmark accesses only disks owned by its worker, each with
an independent queue and pool. Its historical cross-worker readers, index
prefetching, and precomputed write checksums are removed. Historical results
are not performance guarantees for the current topology. The
[experiment archive](docs/experiments/README.md) retains original revisions,
measurements, profiles, and interpretation limits.

## Development

See the [development guide](docs/README.md) for the repository layout,
suggested reading order, and design document index.

Rust stable (see `rust-version` in `Cargo.toml`). Development tasks follow the
[cargo-xtask](https://github.com/matklad/cargo-xtask) convention and are run
through the `cargo x` alias:

| Command | What it does |
|---|---|
| `cargo x` | The default suite: `tools`, `check`, `test`, `udeps`, `license`, `doc` |
| `cargo x tools [-y]` | Installs the helper tools the other tasks need (`typos`, `taplo`, `cargo-sort`, `cargo-machete`, `cargo-nextest`, `license-eye`) |
| `cargo x check` | Spelling, TOML and Rust formatting (applied in place, nightly rustfmt when available), clippy with warnings denied |
| `cargo x test` | `cargo nextest run` plus doctests |
| `cargo x udeps` | Unused dependencies |
| `cargo x license` | Apache 2.0 header check (`.licenserc.yaml`) |
| `cargo x doc` | `cargo doc` with warnings denied |

`cargo x tools` installs helper binaries under `CARGO_HOME`; when it is unset,
the standard `$HOME/.cargo` location is used. Downloaded archives are kept in a
temporary directory and removed after installation.

Run `cargo x` before opening a pull request; CI runs the same checks.

## License

Licensed under the [Apache License, Version 2.0](LICENSE).
