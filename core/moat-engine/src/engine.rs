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

//! Formatting and opening (recovery).

use std::{collections::VecDeque, ops::ControlFlow, sync::Arc};

use moat_common::{AlignedBuf, PAGE_SIZE};

use crate::{
    device::Device,
    error::{Error, Result},
    index::{FLAG_DEAD, Index, IndexValue, Location, flags_from_record},
    layout::{
        Extent, FooterEntry, RecordGeometry, RecordKind, SEGMENT_HEADER_LEN, SUPERBLOCK_A_OFFSET, SUPERBLOCK_B_OFFSET,
        SUPERBLOCK_LEN, SegmentHeader, SegmentKind, SegmentState, Superblock, decode_footer, large_batch_len,
    },
    options::{Clock, FormatOptions, Options, SystemClock},
    reader::Reader,
    scan::{parse_batch, scan_batches_blocking},
    segments::{Geometry, SegmentTable},
    shared::Shared,
    writer::{Writer, seal_segment},
};

/// Formats `device`, destroying any previous contents.
///
/// Writes every segment header as free and both superblock copies. The device
/// must be at least two segments long (the first segment-sized region is
/// reserved for the superblocks).
pub fn format(device: &dyn Device, opts: &FormatOptions) -> Result<()> {
    opts.validate(device.capacity())?;
    let geometry = Geometry::for_device(device.capacity(), opts.segment_size);
    if geometry.segment_count == 0 {
        return Err(Error::InvalidOption("device holds no segments".into()));
    }
    let superblock = Superblock {
        generation: 1,
        disk_uuid: opts.disk_uuid,
        segment_size: opts.segment_size,
        chunk_max: opts.chunk_max,
        segment_count: geometry.segment_count,
        created_at: SystemClock.now_secs(),
    };

    let mut page = AlignedBuf::zeroed(SEGMENT_HEADER_LEN as usize);
    for seg_no in 0..geometry.segment_count {
        SegmentHeader {
            disk_uuid: opts.disk_uuid,
            seg_no,
            state: SegmentState::Free,
            kind: SegmentKind::Hot,
            seq: 0,
            footer_offset: 0,
            footer_len: 0,
            record_count: 0,
        }
        .encode(&mut page);
        device.write_at(&page, geometry.segment_offset(seg_no))?;
    }

    let mut sb = AlignedBuf::zeroed(SUPERBLOCK_LEN);
    superblock.encode(&mut sb);
    device.write_at(&sb, SUPERBLOCK_A_OFFSET)?;
    device.write_at(&sb, SUPERBLOCK_B_OFFSET)?;
    device.sync()?;
    Ok(())
}

/// What recovery found while opening a device.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RecoveryReport {
    /// Segments on the device.
    pub segments: u32,
    /// Segments that were sealed and indexed from their footer.
    pub sealed: u32,
    /// Segments that were active and rebuilt by a forward scan (then sealed).
    pub scanned: u32,
    /// Segments whose header was unreadable or belonged to another format;
    /// treated as free. Non-zero values deserve attention.
    pub unreadable_headers: u32,
    /// Sealed segments whose footer failed validation and were rebuilt by a
    /// forward scan instead (counted in `scanned` as well).
    pub bad_footers: u32,
    /// Records (data and tombstones) seen across footers and scans.
    pub records: u64,
    /// Chunks in the index after recovery.
    pub chunks: usize,
    /// The next LSN the writer will assign.
    pub next_lsn: u64,
}

/// An opened engine.
pub struct Opened {
    /// The single writer.
    pub writer: Writer,
    /// A reader handle; clone it freely.
    pub reader: Reader,
    /// What recovery did.
    pub report: RecoveryReport,
}

/// Opens a formatted device, rebuilding the in-memory index.
///
/// Sealed segments are indexed from their footers; segments left active by a
/// crash are scanned forward, verified record by record, and sealed. Every key
/// resolves to its highest-LSN record, and keys whose newest record is a
/// tombstone are dropped. The cost is proportional to the number of records
/// on the device, not its capacity.
pub fn open(device: Arc<dyn Device>, mut options: Options) -> Result<Opened> {
    options.validate()?;
    let superblock = read_superblock(&*device)?;
    // A pending batch must always fit in a fresh segment together with the
    // segment header and a footer; a quarter of a segment leaves ample room.
    // The batch limit is a power of two so a staging buffer is exactly one
    // pool class, and it must hold at least one maximal small record.
    options.batch_limit = options
        .batch_limit
        .min((superblock.segment_size / 4) as usize)
        .next_power_of_two()
        .max(2 * PAGE_SIZE as usize);
    options.pack_threshold = options.pack_threshold.min((options.batch_limit / 2) as u32);
    // The writer's pool must be able to hold a maximal batch, a staging batch
    // and a scan window.
    let largest = large_batch_len(superblock.chunk_max) as usize;
    let needed = largest
        .max(options.batch_limit)
        .max(options.scan_window)
        .next_power_of_two();
    options.queue.pool.max_class = options.queue.pool.max_class.max(needed);
    let minimum_pool_bytes = needed
        .checked_mul(options.writer_pool_capacity_multiplier)
        .ok_or_else(|| Error::InvalidOption("writer pool capacity exceeds addressable memory".into()))?;
    options.queue.pool.bytes = options.queue.pool.bytes.max(minimum_pool_bytes);
    let geometry = Geometry::for_device(device.capacity(), superblock.segment_size);
    if geometry.segment_count < superblock.segment_count {
        return Err(Error::Unformatted(format!(
            "device holds {} segments but was formatted with {}",
            geometry.segment_count, superblock.segment_count
        )));
    }
    let geometry = Geometry {
        segment_count: superblock.segment_count,
        ..geometry
    };

    let shared = Arc::new(Shared {
        device,
        index: Index::new(options.index_shards),
        segments: SegmentTable::new(geometry.segment_count),
        geometry,
        superblock,
        options,
    });

    let mut report = RecoveryReport {
        segments: geometry.segment_count,
        ..Default::default()
    };
    let mut free = Vec::new();
    let mut actives: Vec<(SegmentHeader, u64, Vec<FooterEntry>)> = Vec::new();
    let mut max_seq = 0u64;
    let mut max_lsn = 0u64;

    for seg_no in 0..geometry.segment_count {
        let header = match shared.read_segment_header(seg_no) {
            Ok(h) if h.disk_uuid == shared.superblock.disk_uuid && h.seg_no == seg_no => h,
            _ => {
                report.unreadable_headers += 1;
                free.push(seg_no);
                continue;
            }
        };
        max_seq = max_seq.max(header.seq);
        if header.state == SegmentState::Free {
            free.push(seg_no);
            continue;
        }
        shared.segments.set(seg_no, header.state, header.kind, header.seq);

        let footer_entries = if header.state == SegmentState::Sealed {
            let footer = shared.read_extent(
                seg_no,
                Extent {
                    start: header.footer_offset,
                    len: header.footer_len,
                },
            )?;
            // A damaged footer is not fatal: the records themselves are still
            // self-describing, so fall back to a scan and write a new footer.
            match decode_footer(&footer, header.seq) {
                Ok(entries) => Some(entries),
                Err(_) => {
                    report.bad_footers += 1;
                    None
                }
            }
        } else {
            None
        };

        match footer_entries {
            Some(entries) => {
                report.sealed += 1;
                for entry in entries {
                    report.records += 1;
                    max_lsn = max_lsn.max(entry.lsn);
                    merge_entry(&shared.index, seg_no, &entry);
                }
            }
            None => {
                report.scanned += 1;
                let (tail, entries) = scan_segment(&shared, seg_no, header.seq)?;
                for entry in &entries {
                    report.records += 1;
                    max_lsn = max_lsn.max(entry.lsn);
                    merge_entry(&shared.index, seg_no, entry);
                }
                actives.push((header, tail, entries));
            }
        }
    }

    // Keys whose newest record is a tombstone are gone.
    shared.index.retain(|_, v| v.flags & FLAG_DEAD == 0);

    // Live bytes follow from the final index, not from the order records
    // were merged in.
    shared.index.for_each(|_, v| {
        shared
            .segments
            .add_live(v.loc.seg_no, RecordGeometry::footprint(v.value_len, v.record_flags()));
    });

    // Seal whatever was active so the tail is described by a footer and the
    // writer starts on fresh segments.
    for (header, tail, entries) in actives {
        seal_segment(&shared, header.seg_no, header.seq, header.kind, tail, &entries)?;
    }

    report.chunks = shared.index.len();
    report.next_lsn = max_lsn + 1;
    free.sort_unstable();
    let queue = shared.device.open_queue(&shared.options.queue)?;
    let writer = Writer::new(shared.clone(), queue, VecDeque::from(free), max_seq + 1, max_lsn + 1);
    let reader = Reader::new(shared);
    Ok(Opened { writer, reader, report })
}

/// Scans a segment forward, verifying every record, and returns the offset at
/// which valid data ends together with a footer entry per record.
///
/// The first torn or corrupt batch ends the recoverable prefix; nothing after
/// it was ever acknowledged (or, for a sealed segment with a bad footer, the
/// remainder is unrecoverable either way).
fn scan_segment(shared: &Shared, seg_no: u32, seq: u64) -> Result<(u64, Vec<FooterEntry>)> {
    let mut entries = Vec::new();
    let tail = scan_batches_blocking(
        shared,
        seg_no,
        seq,
        shared.geometry.data_start(),
        shared.geometry.segment_size,
        |batch_off, batch, bytes| {
            let Ok(records) = parse_batch(bytes, batch, true) else {
                return Ok(ControlFlow::Break(()));
            };
            for rec in records {
                entries.push(FooterEntry {
                    key: rec.header.key,
                    offset: (batch_off + rec.offset_in_batch as u64) as u32,
                    value_off: (batch_off + rec.value_in_batch as u64) as u32,
                    value_len: rec.header.value_len,
                    lsn: rec.header.lsn,
                    crc: rec.checksums.get(0).unwrap_or(0),
                    kind: rec.header.kind,
                    flags: rec.header.flags,
                });
            }
            Ok(ControlFlow::Continue(()))
        },
    )?;
    Ok((tail, entries))
}

/// Merges one recovered record into the index: highest LSN wins, tombstones
/// are kept as dead markers until every record has been seen.
fn merge_entry(index: &Index, seg_no: u32, entry: &FooterEntry) {
    let mut flags = flags_from_record(entry.flags);
    if entry.kind == RecordKind::Tombstone {
        flags |= FLAG_DEAD;
    }
    index.insert_if_newer(
        entry.key,
        IndexValue {
            loc: Location {
                seg_no,
                offset: entry.offset,
            },
            value_off: entry.value_off,
            value_len: entry.value_len,
            flags,
            crc: entry.crc,
            lsn: entry.lsn,
        },
    );
}

fn read_superblock(device: &dyn Device) -> Result<Superblock> {
    let mut buf = AlignedBuf::zeroed(SUPERBLOCK_LEN);
    let mut best: Option<Superblock> = None;
    let mut first_err = None;
    for offset in [SUPERBLOCK_A_OFFSET, SUPERBLOCK_B_OFFSET] {
        device.read_at(&mut buf, offset)?;
        match Superblock::decode(&buf) {
            Ok(sb) => {
                if best.as_ref().is_none_or(|b| sb.generation > b.generation) {
                    best = Some(sb);
                }
            }
            Err(e) => {
                first_err.get_or_insert(e);
            }
        }
    }
    best.ok_or_else(|| first_err.expect("both copies failed"))
}
