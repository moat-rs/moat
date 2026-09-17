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

use moat_common::ChunkId;

use super::{Pending, Pipeline, Rejected, Result, Ticket, Write, index};
use crate::{
    frame::{FrameBuilder, FrameLimits, FramePosition, Metadata, PreparedFrame},
    io::{Buffer, Operation, Queue},
    segment::SegmentBuilder,
};

impl<Q: Queue> Pipeline<Q> {
    /// Encodes and submits one frame without staging a second payload copy.
    ///
    /// The caller may refill/reuse its builder after admission. On rejection it
    /// retains its original buffer and borrowed values. Caller-supplied LSNs must
    /// uniquely identify logical versions; equal LSNs keep the first occurrence.
    pub fn write(
        &mut self,
        frame: &FrameBuilder<'_>,
        buffer: impl Into<Buffer>,
    ) -> std::result::Result<Ticket, Rejected<Buffer>> {
        self.submit_frame(buffer.into(), |segment, _, buffer| {
            let position = segment.position(frame.encoded_len(), frame.metadata_len())?;
            frame.encode_into(position, buffer)?;
            Ok(position)
        })
    }

    /// Finalizes and submits an already filled prepared value without copying it.
    ///
    /// Fill `PreparedFrame::new(limits, value_len, &mut buffer)?.value_mut()`
    /// before calling this method. The pipeline writes metadata and checksums,
    /// then transfers ownership of that same allocation to its I/O queue.
    /// Rejection retains the buffer and the prepared payload for retry.
    pub fn write_prepared(
        &mut self,
        key: ChunkId,
        lsn: u64,
        value_len: u32,
        buffer: impl Into<Buffer>,
    ) -> std::result::Result<Ticket, Rejected<Buffer>> {
        self.submit_frame(buffer.into(), |segment, limits, buffer| {
            let len = PreparedFrame::required_len(limits, value_len)?;
            let frame = PreparedFrame::new(limits, value_len, buffer)?;
            let position = segment.position(len, frame.metadata_len())?;
            frame.finish(position, key, lsn)?;
            Ok(position)
        })
    }

    fn submit_frame(
        &mut self,
        mut buffer: Buffer,
        encode: impl FnOnce(&SegmentBuilder, FrameLimits, &mut Buffer) -> Result<FramePosition>,
    ) -> std::result::Result<Ticket, Rejected<Buffer>> {
        let prepare = |this: &mut Self, buffer: &mut Buffer| -> Result<(Write, u64, usize)> {
            let ticket = this.admission(true)?;
            let segment = this.segment.as_mut().expect("writable pipeline");
            let position = encode(segment, this.limits, buffer)?;
            let metadata = Metadata::decode(buffer, this.limits, position)?;
            let len = metadata.header().frame_len();
            let entries = index::entries(metadata, this.current as u32).collect();
            segment.append(metadata)?;
            Ok((
                Write {
                    ticket,
                    entries,
                    completed: None,
                },
                this.extents[this.current].base + position.offset() as u64,
                len,
            ))
        };
        let (write, offset, len) = match prepare(self, &mut buffer) {
            Ok(prepared) => prepared,
            Err(error) => return Err(Rejected { error, input: buffer }),
        };
        let ticket = write.ticket;
        let slot = self.take_slot(Pending::Write(write));
        self.writes.push_back(slot);
        self.submit(slot, Operation::Write, offset, len, Some(buffer));
        Ok(ticket)
    }
}
