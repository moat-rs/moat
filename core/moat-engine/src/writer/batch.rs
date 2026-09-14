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

//! Packed inline and framed batches before they receive a segment offset.

use moat_common::{PAGE_SIZE, PooledBuf, align_up};

use super::{Apply, Lsn, Ticket};
use crate::layout::{
    BATCH_HEADER_LEN, BatchHeader, BatchKind, FooterEntry, RECORD_ALIGN, RecordHeader, record_meta_len,
};

pub(super) struct PendingRecord {
    /// Offsets are batch-relative until `enqueue_batch` fixes the position.
    pub(super) entry: FooterEntry,
    pub(super) apply: Apply,
    pub(super) ticket: Option<Ticket>,
}

/// A packed batch under construction, encoded directly into a pool buffer.
///
/// Inline batches place each header right before its value; framed batches
/// keep headers in a reserved area at the front and page-align every value.
pub(super) struct Pending {
    pub(super) kind: BatchKind,
    pub(super) buf: Option<PooledBuf>,
    /// Bytes used so far: the end of the last record (inline) or of the last
    /// value (framed).
    pub(super) len: usize,
    /// Framed batches: size of the reserved header area, and the position of
    /// the next header within it.
    header_len: usize,
    header_pos: usize,
    pub(super) records: Vec<PendingRecord>,
    first_lsn: Lsn,
}

impl Pending {
    pub(super) fn new(kind: BatchKind) -> Self {
        Self {
            kind,
            buf: None,
            len: BATCH_HEADER_LEN,
            header_len: 0,
            header_pos: BATCH_HEADER_LEN,
            records: Vec::new(),
            first_lsn: 0,
        }
    }

    pub(super) fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Attaches a fresh staging buffer. Framed batches reserve a header area
    /// large enough for the most records that could fit their values (every
    /// framed value is close to a page multiple, so at most one per page).
    pub(super) fn attach(&mut self, buf: PooledBuf) {
        if self.kind == BatchKind::Framed {
            let max_records = buf.capacity() / PAGE_SIZE as usize;
            let meta = record_meta_len(PAGE_SIZE as u32);
            self.header_len = align_up((BATCH_HEADER_LEN + max_records * meta) as u64, PAGE_SIZE) as usize;
            self.header_pos = BATCH_HEADER_LEN;
            self.len = self.header_len;
        }
        self.buf = Some(buf);
    }

    /// Whether a record of `meta` + `value_len` bytes fits in the attached
    /// buffer.
    pub(super) fn fits(&self, meta: usize, value_len: usize) -> bool {
        let Some(buf) = &self.buf else {
            return false;
        };
        match self.kind {
            BatchKind::Framed => {
                self.header_pos + meta <= self.header_len
                    && align_up(self.len as u64, PAGE_SIZE) as usize + value_len <= buf.capacity()
            }
            _ => self.next_position(meta, value_len) + meta + value_len <= buf.capacity(),
        }
    }

    /// Position the next inline record of `meta + value_len` bytes starts at:
    /// 8-byte aligned, but moved to the next page boundary whenever that lets
    /// the record span fewer pages. The gap is zero-filled and recognised by
    /// the scanner.
    fn next_position(&self, meta: usize, value_len: usize) -> usize {
        let pos = align_up(self.len as u64, RECORD_ALIGN) as usize;
        let len = (meta + value_len) as u64;
        let min_pages = len.div_ceil(PAGE_SIZE);
        let spanned = (pos as u64 % PAGE_SIZE + len).div_ceil(PAGE_SIZE);
        if spanned > min_pages {
            align_up(pos as u64, PAGE_SIZE) as usize
        } else {
            pos
        }
    }

    pub(super) fn append(
        &mut self,
        hdr: &RecordHeader,
        checksums: &[u32],
        value: &[u8],
        apply: Apply,
        ticket: Option<Ticket>,
    ) {
        let meta = hdr.meta_len();
        let (hdr_pos, value_pos) = match self.kind {
            BatchKind::Framed => (self.header_pos, align_up(self.len as u64, PAGE_SIZE) as usize),
            _ => {
                let pos = self.next_position(meta, value.len());
                (pos, pos + meta)
            }
        };
        let buf = self.buf.as_mut().expect("pending buffer allocated");
        // Zero whatever lies between the previous record and this one.
        buf[self.len..value_pos.max(self.len)].fill(0);
        hdr.encode(&mut buf[hdr_pos..hdr_pos + meta], checksums);
        buf[value_pos..value_pos + value.len()].copy_from_slice(value);
        if self.records.is_empty() {
            self.first_lsn = hdr.lsn;
        }
        self.len = value_pos + value.len();
        if self.kind == BatchKind::Framed {
            self.header_pos = hdr_pos + meta;
        }
        self.records.push(PendingRecord {
            entry: footer_entry(hdr, checksums, hdr_pos, value_pos),
            apply,
            ticket,
        });
    }

    /// Finishes the batch: zero-fills the unused header area and the tail
    /// padding, encodes the batch header, and returns the batch length.
    pub(super) fn finish(&mut self, seg_seq: u64) -> usize {
        let batch_len = align_up(self.len as u64, PAGE_SIZE) as usize;
        let buf = self.buf.as_mut().expect("pending buffer allocated");
        if self.kind == BatchKind::Framed {
            buf[self.header_pos..self.header_len].fill(0);
        }
        buf[self.len..batch_len].fill(0);
        BatchHeader {
            seg_seq,
            batch_len: batch_len as u32,
            record_count: self.records.len() as u32,
            first_lsn: self.first_lsn,
            kind: self.kind,
            header_len: if self.kind == BatchKind::Framed {
                self.header_len as u32
            } else {
                0
            },
        }
        .encode(&mut buf[..BATCH_HEADER_LEN]);
        batch_len
    }
}

/// The footer entry for a record, with segment offsets still unresolved.
pub(super) fn footer_entry(hdr: &RecordHeader, checksums: &[u32], offset: usize, value_off: usize) -> FooterEntry {
    FooterEntry {
        key: hdr.key,
        offset: offset as u32,
        value_off: value_off as u32,
        value_len: hdr.value_len,
        lsn: hdr.lsn,
        crc: checksums.first().copied().unwrap_or(0),
        kind: hdr.kind,
        flags: hdr.flags,
    }
}
