// Copyright 2026- Moat Project Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! A single-disk, log-structured chunk engine.
//!
//! One engine instance manages one block device (an NVMe namespace, a file, or
//! an in-memory buffer). It stores immutable, variable-length chunks of up to
//! a configured maximum size, identified by an opaque 128-bit
//! [`ChunkId`](moat_common::ChunkId).
//!
//! # Design in one paragraph
//!
//! The device is divided into fixed-size *segments*. Records are appended to at
//! most two active segments (hot for foreground writes, cold for records
//! relocated by reclaim) in self-describing, checksummed *batches*. When a
//! segment fills up it is sealed with a *footer* listing every record, so
//! recovery reads footers instead of data. An in-memory sharded hash *index*
//! maps every chunk to its newest record. Deletes append *tombstones*. Space is
//! reclaimed one segment at a time by relocating what is still live and freeing
//! the segment; the same mechanism implements cache eviction under a different
//! policy. There is no separate write-ahead log and no embedded key-value
//! store: the log is the only source of truth.
//!
//! # Threads
//!
//! [`open`] returns exactly one [`Writer`] and a cloneable [`Reader`]. All
//! mutations (put, delete, flush, reclaim) go through the writer, which is
//! meant to be driven by one thread per disk. Any number of threads may read
//! concurrently, each through its own [`ReadRing`] created from a reader
//! handle. Every thread that does I/O owns an I/O queue (io_uring on Linux)
//! and a buffer pool of pre-registered, huge-page backed memory; values move
//! between pool buffers and the device without copies.
//!
//! # Example
//!
//! ```
//! use std::sync::Arc;
//! use moat_common::ChunkId;
//! use moat_engine::{FormatOptions, MemDevice, Options, PutOptions, PutOutcome, QueueOptions};
//!
//! let device = Arc::new(MemDevice::new(16 << 20));
//! moat_engine::format(&*device, &FormatOptions { segment_size: 1 << 20, chunk_max: 128 << 10, ..Default::default() })?;
//! let opened = moat_engine::open(device, Options::default())?;
//! let (mut writer, reader) = (opened.writer, opened.reader);
//! let mut ring = reader.ring(&QueueOptions::default())?;
//!
//! let id = ChunkId::from_u128(42);
//! assert!(matches!(writer.put(id, b"hello", PutOptions::default())?, PutOutcome::Written { .. }));
//! writer.flush()?;
//! assert_eq!(ring.get_sync(&id, None)?.as_deref(), Some(&b"hello"[..]));
//! assert!(writer.delete(&id)?);
//! assert!(ring.get_sync(&id, None)?.is_none());
//! # Ok::<(), moat_engine::Error>(())
//! ```

mod codec;
mod device;
mod engine;
mod error;
mod index;
pub mod io;
pub mod layout;
mod options;
mod reader;
mod scan;
mod segments;
mod shared;
#[cfg(target_os = "linux")]
pub mod uring;
mod writer;

pub use device::{Device, FileDevice, MemDevice};
pub use engine::{Opened, RecoveryReport, format, open};
pub use error::{Error, Result};
pub use io::{IoQueue, QueueOptions};
pub use options::{Clock, FormatOptions, ManualClock, Options, SystemClock};
pub use reader::{ChunkData, ChunkStat, ReadCompletion, ReadOutcome, ReadRing, Reader, Usage};
pub use writer::{Completion, LargeValue, Lsn, PutOptions, PutOutcome, ReclaimPolicy, ReclaimReport, Ticket, Writer};
