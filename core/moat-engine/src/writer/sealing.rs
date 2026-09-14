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

//! Segment allocation, sealing, and durability barriers.
//!
//! Sealing drains the segment's batches before writing its footer and sealed
//! header. Barriers wait for the preceding batches and any requested fsync.

use moat_common::AlignedBuf;

use super::{Active, Aux, AuxOp, Outcome, Ticket, Writer, io_error, slot};
use crate::{
    Error, IoQueue, Result,
    layout::{SegmentHeader, SegmentKind, SegmentState, encode_footer, footer_len},
};

pub(super) enum SealStage {
    /// Waiting for the segment's batches to be applied.
    Draining,
    /// Footer pieces being written.
    Footer {
        encoded: AlignedBuf,
        written: usize,
        outstanding: u32,
    },
    /// The sealed header write is in flight.
    Header,
}

pub(super) struct Sealing {
    pub(super) seg: Active,
    pub(super) stage: SealStage,
}

pub(super) enum Fsync {
    NotNeeded,
    /// Batches are applied; the fsync has yet to be enqueued.
    Pending,
    Issued,
    Done,
}

pub(super) struct Barrier {
    pub(super) ticket: Ticket,
    /// Complete once no batch with an id below this is unapplied.
    pub(super) until: u64,
    /// Segments that must leave `sealing` (for `seal`).
    pub(super) seal: Option<Vec<u32>>,
    pub(super) fsync: Fsync,
    pub(super) error: Option<String>,
}

impl Writer {
    /// Barrier: its completion reports [`Outcome::Flush`] once every write
    /// accepted before it is durable (including an `fdatasync` when
    /// [`Options::sync_on_flush`](crate::Options::sync_on_flush) is set), or
    /// the first error among those writes.
    pub fn flush(&mut self, q: &mut dyn IoQueue) -> Result<Ticket> {
        self.close_all_pending(q)?;
        Ok(self.enqueue_barrier(q, None))
    }

    /// `flush`, then seal both active segments so the next open needs no
    /// scan. The completion reports [`Outcome::Seal`].
    ///
    /// The writer allocates fresh segments on the next write, so sealing a
    /// nearly empty segment wastes its remaining space; callers normally leave
    /// sealing to the writer and use this before shutdown or when reclaim must
    /// be able to reach recent data.
    pub fn seal(&mut self, q: &mut dyn IoQueue) -> Result<Ticket> {
        self.close_all_pending(q)?;
        let mut sealed = Vec::new();
        for k in 0..2 {
            if let Some(active) = self.active[k].take() {
                sealed.push(active.seg_no);
                self.park(active);
            }
        }
        Ok(self.enqueue_barrier(q, Some(sealed)))
    }

    fn enqueue_barrier(&mut self, q: &mut dyn IoQueue, seal: Option<Vec<u32>>) -> Ticket {
        let ticket = self.next_ticket();
        self.barriers.push_back(Barrier {
            ticket,
            until: self.next_batch_id,
            seal,
            fsync: if self.shared.options.sync_on_flush {
                Fsync::Pending
            } else {
                Fsync::NotNeeded
            },
            error: None,
        });
        self.push_ready(q);
        ticket
    }

    /// Makes sure the active segment of `kind` has room for a batch of
    /// `batch_len` bytes and `new_records` footer entries, parking the current
    /// one for sealing and allocating a fresh one as needed.
    pub(super) fn ensure_room(
        &mut self,
        q: &mut dyn IoQueue,
        kind: SegmentKind,
        batch_len: u64,
        new_records: usize,
    ) -> Result<()> {
        let k = slot(kind);
        let segment_size = self.shared.superblock.segment_size;
        let fits = self.active[k]
            .as_ref()
            .is_some_and(|a| a.fits(batch_len, new_records, segment_size));
        if fits {
            return Ok(());
        }
        // Allocate first so a failure leaves the current segment in place.
        let fresh = self.allocate(q, kind)?;
        if !fresh.fits(batch_len, new_records, segment_size) {
            // Only reachable if format validation was bypassed; the header
            // write is already queued, so the segment is parked, not reused.
            self.park(fresh);
            return Err(Error::ValueTooLarge {
                len: batch_len,
                max: segment_size,
            });
        }
        if let Some(old) = self.active[k].replace(fresh) {
            self.park(old);
        }
        Ok(())
    }

    /// Takes a free segment, queues its header write and returns it as
    /// active (batches wait until the header has landed).
    fn allocate(&mut self, q: &mut dyn IoQueue, kind: SegmentKind) -> Result<Active> {
        let seg_no = self.free.pop_front().ok_or(Error::NoSpace)?;
        let seq = self.next_seq;
        let header = self.shared.segment_header(seg_no, SegmentState::Active, kind, seq);
        if let Err(e) = self.write_header(q, &header, Aux::OpenHeader { seg_no }) {
            self.free.push_front(seg_no);
            return Err(e);
        }
        self.next_seq += 1;
        self.shared.segments.set(seg_no, SegmentState::Active, kind, seq);
        Ok(Active {
            seg_no,
            seq,
            kind,
            tail: self.shared.geometry.data_start(),
            footer: Vec::new(),
            broken: false,
            opened: false,
            unapplied: 0,
            unapplied_records: 0,
        })
    }

    /// Hands a segment that is no longer written to the sealing state
    /// machine.
    pub(super) fn park(&mut self, seg: Active) {
        self.sealing.push(Sealing {
            seg,
            stage: SealStage::Draining,
        });
    }

    pub(super) fn advance_sealing(&mut self, q: &mut dyn IoQueue) {
        for i in 0..self.sealing.len() {
            self.advance_one_sealing(q, i);
        }
    }

    fn advance_one_sealing(&mut self, q: &mut dyn IoQueue, i: usize) {
        // The stage is taken out while it is worked on so the queue and the
        // pool can be used without holding a borrow into `sealing`.
        loop {
            let stage = std::mem::replace(&mut self.sealing[i].stage, SealStage::Header);
            match stage {
                SealStage::Draining => {
                    let seg = &self.sealing[i].seg;
                    if seg.unapplied > 0 {
                        self.sealing[i].stage = SealStage::Draining;
                        return;
                    }
                    let len = footer_len(seg.footer.len()) as usize;
                    let mut encoded = AlignedBuf::zeroed(len);
                    encode_footer(seg.seq, &seg.footer, &mut encoded);
                    self.sealing[i].stage = SealStage::Footer {
                        encoded,
                        written: 0,
                        outstanding: 0,
                    };
                }
                SealStage::Footer {
                    encoded,
                    mut written,
                    mut outstanding,
                } => {
                    let seg = &self.sealing[i].seg;
                    let (seg_no, tail, kind, seq, count) = (seg.seg_no, seg.tail, seg.kind, seg.seq, seg.footer.len());
                    let base = self.shared.geometry.segment_offset(seg_no);
                    let max = q.pool().max_class();
                    while written < encoded.len() {
                        let piece = (encoded.len() - written).min(max);
                        let Some(mut buf) = q.pool().alloc(piece) else {
                            self.sealing[i].stage = SealStage::Footer {
                                encoded,
                                written,
                                outstanding,
                            };
                            return;
                        };
                        buf[..piece].copy_from_slice(&encoded[written..written + piece]);
                        let op = AuxOp {
                            aux: Aux::Footer { seg_no },
                            buf,
                            len: piece,
                            offset: base + tail + written as u64,
                            read: false,
                        };
                        written += piece;
                        // A deferred piece is still outstanding.
                        outstanding += 1;
                        self.enqueue_aux(q, op);
                    }
                    if outstanding > 0 {
                        self.sealing[i].stage = SealStage::Footer {
                            encoded,
                            written,
                            outstanding,
                        };
                        return;
                    }
                    let header = SegmentHeader {
                        footer_offset: tail,
                        footer_len: encoded.len() as u64,
                        record_count: count as u64,
                        ..self.shared.segment_header(seg_no, SegmentState::Sealed, kind, seq)
                    };
                    match self.write_header(q, &header, Aux::SealHeader { seg_no }) {
                        Ok(()) => {
                            self.sealing[i].stage = SealStage::Header;
                        }
                        Err(_) => {
                            self.sealing[i].stage = SealStage::Footer {
                                encoded,
                                written,
                                outstanding,
                            };
                        }
                    }
                    return;
                }
                SealStage::Header => {
                    self.sealing[i].stage = SealStage::Header;
                    return;
                }
            }
        }
    }

    pub(super) fn advance_barriers(&mut self, q: &mut dyn IoQueue) {
        let oldest = self.oldest_unapplied();
        let mut i = 0;
        while i < self.barriers.len() {
            let ticket = self.barriers[i].ticket;
            let until = self.barriers[i].until;
            let applied = oldest.is_none_or(|o| o >= until);
            if !applied {
                i += 1;
                continue;
            }
            if matches!(self.barriers[i].fsync, Fsync::Pending) {
                let token = self.next_aux_token();
                match q.fsync(self.desc, token) {
                    Ok(()) => {
                        self.aux.insert(token, (Aux::Fsync { ticket }, 0));
                        self.barriers[i].fsync = Fsync::Issued;
                    }
                    Err(_) => {
                        i += 1;
                        continue;
                    }
                }
            }
            if matches!(self.barriers[i].fsync, Fsync::Issued) {
                i += 1;
                continue;
            }
            if let Some(segs) = &self.barriers[i].seal
                && segs.iter().any(|s| self.sealing.iter().any(|x| x.seg.seg_no == *s))
            {
                i += 1;
                continue;
            }
            let b = self.barriers.remove(i).expect("index in range");
            let outcome = if b.seal.is_some() {
                Outcome::Seal
            } else {
                Outcome::Flush
            };
            let result = match b.error {
                Some(e) => Err(io_error(e)),
                None => Ok(outcome),
            };
            self.complete(b.ticket, result);
        }
    }
}
