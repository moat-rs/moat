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

//! Reclaim one sealed segment while preserving record LSNs.
//!
//! The victim is freed only after relocation writes complete and readers release
//! their pins. A failed scan or relocation keeps the victim available.

use std::sync::atomic::Ordering;

use moat_common::{PAGE_SIZE, PooledBuf, align_up, verify_blocks_with};

use super::{
    Apply, Aux, AuxOp, Outcome, ReclaimReport, Ticket, Writer, header, io_error, record_flags, small_batch_kind,
};
use crate::{
    Error, IoQueue, Result,
    index::Location,
    layout::{BatchKind, RecordHeader, RecordKind, SegmentKind, SegmentState, large_batch_len},
    scan::{BatchStep, next_batch, parse_batch},
};

pub(super) enum ReclaimStage {
    /// Issue the next window read (or finish when the cursor reached the end).
    Read,
    /// A window read is in flight.
    Reading,
    /// A window is being processed; resume at batch `rel`, record `rec`.
    Process {
        buf: PooledBuf,
        len: usize,
        rel: usize,
        rec: usize,
    },
    /// Every record is processed; waiting for the batches that carry the
    /// relocations and the foreground writes the decisions relied on.
    Drain,
    /// Waiting for readers to release the victim.
    Pins,
    /// The free header write is in flight.
    Freeing,
}

pub(super) struct ReclaimJob {
    pub(super) ticket: Ticket,
    pub(super) seg_no: u32,
    pub(super) seq: u64,
    pub(super) kind: SegmentKind,
    pub(super) is_oldest: bool,
    pub(super) end: u64,
    pub(super) cursor: u64,
    pub(super) window: usize,
    pub(super) report: ReclaimReport,
    pub(super) stage: ReclaimStage,
    pub(super) drain_until: u64,
    /// A relocation write failed; the victim must be kept.
    pub(super) failed: bool,
}

impl Writer {
    /// Starts one reclaim pass, relocating live records; the completion reports
    /// [`Outcome::Reclaim`]. `None` if there is no sealed segment to reclaim,
    /// [`Error::Busy`] while a previous pass is still running.
    ///
    /// Relocation needs a free segment for the cold log; callers should
    /// reclaim before the free list is exhausted (keeping at least two free
    /// segments is enough).
    pub fn reclaim(&mut self, q: &mut dyn IoQueue) -> Result<Option<Ticket>> {
        if self.reclaim.is_some() {
            return Err(Error::Busy);
        }
        let Some(seg_no) = self.pick_victim() else {
            return Ok(None);
        };
        let (seq, kind, end) = {
            let segments = &self.shared.segments;
            (segments.seq(seg_no), segments.kind(seg_no), segments.data_end(seg_no))
        };
        let is_oldest = self.oldest_live_seq() == Some(seq);
        // The window is a performance knob; it is bounded by the pool's
        // largest class but never below the largest batch.
        let window = align_up(self.shared.options.scan_window as u64, PAGE_SIZE)
            .min(q.pool().max_class() as u64)
            .max(self.max_batch) as usize;
        let ticket = self.next_ticket();
        self.reclaim = Some(ReclaimJob {
            ticket,
            seg_no,
            seq,
            kind,
            is_oldest,
            end,
            cursor: self.shared.geometry.data_start(),
            window,
            report: ReclaimReport {
                seg_no,
                ..Default::default()
            },
            stage: ReclaimStage::Read,
            drain_until: 0,
            failed: false,
        });
        self.advance_reclaim(q);
        Ok(Some(ticket))
    }

    /// Chooses the sealed segment with the fewest live bytes, breaking ties
    /// by age, that [`Writer::reclaim`] would process next.
    pub fn pick_victim(&self) -> Option<u32> {
        let segments = &self.shared.segments;
        segments
            .iter()
            .filter(|&s| segments.state(s) == SegmentState::Sealed)
            .min_by_key(|&s| (segments.live_bytes(s), segments.seq(s)))
    }

    fn oldest_live_seq(&self) -> Option<u64> {
        let segments = &self.shared.segments;
        segments
            .iter()
            .filter(|&s| segments.state(s) != SegmentState::Free)
            .map(|s| segments.seq(s))
            .min()
    }

    pub(super) fn set_reclaim_stage(&mut self, stage: ReclaimStage) {
        if let Some(job) = &mut self.reclaim {
            job.stage = stage;
        }
    }

    pub(super) fn finish_reclaim(&mut self, result: Result<Outcome>) {
        if let Some(job) = self.reclaim.take() {
            self.complete(job.ticket, result);
        }
    }

    pub(super) fn advance_reclaim(&mut self, q: &mut dyn IoQueue) {
        loop {
            // The stage is taken out while it is worked on; every arm puts a
            // stage back before returning or looping.
            let Some(stage) = self
                .reclaim
                .as_mut()
                .map(|j| std::mem::replace(&mut j.stage, ReclaimStage::Reading))
            else {
                return;
            };
            match stage {
                ReclaimStage::Read => {
                    let (seg_no, cursor, end, window) = {
                        let j = self.reclaim.as_ref().expect("job exists");
                        (j.seg_no, j.cursor, j.end, j.window)
                    };
                    if cursor >= end {
                        // Every record is processed. The batches carrying the
                        // relocations, and the foreground writes whose
                        // existence justified dropping records (a pending
                        // tombstone, a newer version), must be durable before
                        // the victim disappears.
                        if let Err(e) = self.close_all_pending(q) {
                            match e {
                                Error::Busy => {
                                    self.set_reclaim_stage(ReclaimStage::Read);
                                    return;
                                }
                                e => {
                                    self.finish_reclaim(Err(e));
                                    return;
                                }
                            }
                        }
                        let until = self.next_batch_id;
                        let job = self.reclaim.as_mut().expect("job exists");
                        job.drain_until = until;
                        job.stage = ReclaimStage::Drain;
                        continue;
                    }
                    let len = (end - cursor).min(window as u64) as usize;
                    let Some(buf) = q.pool().alloc(len) else {
                        self.set_reclaim_stage(ReclaimStage::Read);
                        return;
                    };
                    let offset = self.shared.geometry.segment_offset(seg_no) + cursor;
                    self.set_reclaim_stage(ReclaimStage::Reading);
                    self.enqueue_aux(
                        q,
                        AuxOp {
                            aux: Aux::ReclaimRead,
                            buf,
                            len,
                            offset,
                            read: true,
                        },
                    );
                    return;
                }
                ReclaimStage::Reading => {
                    self.set_reclaim_stage(ReclaimStage::Reading);
                    return;
                }
                ReclaimStage::Process { buf, len, rel, rec } => match self.process_window(q, buf, len, rel, rec) {
                    Ok(None) => self.set_reclaim_stage(ReclaimStage::Read),
                    Ok(Some(stage)) => {
                        self.set_reclaim_stage(stage);
                        return;
                    }
                    Err(e) => {
                        self.finish_reclaim(Err(e));
                        return;
                    }
                },
                ReclaimStage::Drain => {
                    let (until, failed, seg_no) = {
                        let j = self.reclaim.as_ref().expect("job exists");
                        (j.drain_until, j.failed, j.seg_no)
                    };
                    if !self.oldest_unapplied().is_none_or(|o| o >= until) {
                        self.set_reclaim_stage(ReclaimStage::Drain);
                        return;
                    }
                    if failed {
                        self.finish_reclaim(Err(io_error(format!(
                            "segment {seg_no}: relocation writes failed; segment kept"
                        ))));
                        return;
                    }
                    self.set_reclaim_stage(ReclaimStage::Pins);
                }
                ReclaimStage::Pins => {
                    // Readers that looked the victim up before its entries
                    // were removed hold a pin; the removals are ordered before
                    // this check.
                    std::sync::atomic::fence(Ordering::SeqCst);
                    let (seg_no, kind, seq) = {
                        let j = self.reclaim.as_ref().expect("job exists");
                        (j.seg_no, j.kind, j.seq)
                    };
                    if self.shared.segments.pins(seg_no) > 0 {
                        self.set_reclaim_stage(ReclaimStage::Pins);
                        return;
                    }
                    let header = self.shared.segment_header(seg_no, SegmentState::Free, kind, seq);
                    match self.write_header(q, &header, Aux::FreeHeader { seg_no }) {
                        Ok(()) => self.set_reclaim_stage(ReclaimStage::Freeing),
                        Err(_) => self.set_reclaim_stage(ReclaimStage::Pins),
                    }
                    return;
                }
                ReclaimStage::Freeing => {
                    self.set_reclaim_stage(ReclaimStage::Freeing);
                    return;
                }
            }
        }
    }

    /// Processes the window `buf[..len]` from batch offset `rel`, record
    /// `rec`. Returns the stage to park in when the pool runs dry mid-window,
    /// or `None` when the window is consumed and the cursor advanced.
    fn process_window(
        &mut self,
        q: &mut dyn IoQueue,
        buf: PooledBuf,
        len: usize,
        mut rel: usize,
        mut rec: usize,
    ) -> Result<Option<ReclaimStage>> {
        let (seg_no, seq, cursor, end, is_oldest) = {
            let job = self.reclaim.as_ref().expect("job exists");
            (job.seg_no, job.seq, job.cursor, job.end, job.is_oldest)
        };
        loop {
            match next_batch(&buf[..len], rel, seq, self.max_batch, end - cursor - rel as u64) {
                BatchStep::Batch(batch, batch_len) => {
                    let batch_off = cursor + rel as u64;
                    // A structurally corrupt batch aborts the pass without
                    // freeing anything: live records behind it could be lost.
                    // Live value checksums are checked per record below and
                    // likewise abort the pass on corruption.
                    let records = parse_batch(&buf[rel..rel + batch_len], &batch, false)?;
                    for (i, r) in records.iter().enumerate().skip(rec) {
                        let loc = Location {
                            seg_no,
                            offset: (batch_off + r.offset_in_batch as u64) as u32,
                        };
                        // Reclaim is off the hot path; copying the checksum
                        // array out keeps the re-encoding API simple.
                        let checksums = r.checksums.to_vec();
                        match self.reclaim_record(q, &r.header, &checksums, r.value, loc, is_oldest) {
                            Ok(()) => self.reclaim.as_mut().expect("job exists").report.records += 1,
                            Err(Error::Busy) => {
                                return Ok(Some(ReclaimStage::Process { buf, len, rel, rec: i }));
                            }
                            Err(e) => return Err(e),
                        }
                    }
                    rel += batch_len;
                    rec = 0;
                }
                BatchStep::NeedMore => break,
                BatchStep::End => {
                    // A sealed segment has a known data boundary. Unlike an
                    // active recovery scan, an invalid header before that
                    // boundary is corruption, not an uncommitted tail.
                    if cursor + rel as u64 != end {
                        return Err(Error::corrupt(format!(
                            "segment {seg_no}: invalid batch at {} before data end {end}",
                            cursor + rel as u64
                        )));
                    }
                    self.reclaim.as_mut().expect("job exists").cursor = end;
                    return Ok(None);
                }
            }
        }
        if rel == 0 {
            return Err(Error::corrupt(format!(
                "segment {seg_no}: batch at {cursor} larger than the scan window"
            )));
        }
        let job = self.reclaim.as_mut().expect("job exists");
        job.cursor += rel as u64;
        Ok(None)
    }

    #[allow(clippy::too_many_arguments)]
    fn reclaim_record(
        &mut self,
        q: &mut dyn IoQueue,
        hdr: &RecordHeader,
        checksums: &[u32],
        value: &[u8],
        loc: Location,
        is_oldest: bool,
    ) -> Result<()> {
        match hdr.kind {
            RecordKind::Data => {
                let Some(_) = self.index.get(&hdr.key).filter(|v| v.loc == loc) else {
                    self.reclaim.as_mut().expect("job exists").report.dropped += 1;
                    return Ok(());
                };
                // A corrupt live chunk remains indexed and its segment must
                // not be reused: repair or deletion is an explicit decision.
                if let Err(block) = verify_blocks_with(value, 0, |b| checksums.get(b as usize).copied()) {
                    return Err(Error::corrupt(format!(
                        "chunk {}: checksum block {block} mismatch during reclaim",
                        hdr.key
                    )));
                }
                // Relocated records keep their LSN (see the module docs).
                let apply = Apply::Relocate(loc);
                if hdr.is_large() {
                    let mut large = self.prepare_large(q, hdr.value_len)?;
                    self.ensure_room(q, SegmentKind::Cold, large_batch_len(hdr.value_len), 1)?;
                    large.value_mut().copy_from_slice(value);
                    let flags = record_flags(BatchKind::Large);
                    let new = header(RecordKind::Data, flags, hdr.value_len, hdr.lsn, hdr.key);
                    self.write_large(q, SegmentKind::Cold, &new, checksums, large.buf, apply, None);
                } else {
                    let batch = small_batch_kind(hdr.value_len);
                    self.reserve_small(q, SegmentKind::Cold, batch, value.len())?;
                    let flags = record_flags(batch);
                    let new = header(RecordKind::Data, flags, hdr.value_len, hdr.lsn, hdr.key);
                    self.append_small(q, SegmentKind::Cold, batch, &new, checksums, value, apply, None);
                }
                let report = &mut self.reclaim.as_mut().expect("job exists").report;
                report.relocated += 1;
                report.bytes_relocated += value.len() as u64;
            }
            RecordKind::Tombstone => {
                // A live newer version supersedes the tombstone; and if this is
                // the oldest segment, no older data can exist. Otherwise it is
                // carried forward with its LSN, so a put of the key that is
                // still pending keeps outranking it.
                if self.index.get(&hdr.key).is_some_and(|v| v.lsn > hdr.lsn) || is_oldest {
                    self.reclaim.as_mut().expect("job exists").report.tombstones_dropped += 1;
                } else {
                    self.reserve_small(q, SegmentKind::Cold, BatchKind::Inline, 0)?;
                    let new = header(RecordKind::Tombstone, 0, 0, hdr.lsn, hdr.key);
                    self.append_small(
                        q,
                        SegmentKind::Cold,
                        BatchKind::Inline,
                        &new,
                        &[],
                        &[],
                        Apply::Tombstone,
                        None,
                    );
                    self.reclaim.as_mut().expect("job exists").report.tombstones_relocated += 1;
                }
            }
        }
        Ok(())
    }
}
