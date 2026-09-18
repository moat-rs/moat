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

//! Segment metadata, footer reservation, and frame-by-frame recovery.
//!
//! The caller chooses physical segments and supplies I/O. Encoding a sealed
//! header does not persist it: all frame writes and the footer must be durable
//! before that header is written and made durable. A failed write abandons the
//! allocated tail; a recovered active segment must be sealed before reuse.

mod builder;
mod error;
mod footer;
mod header;
mod recovery;

pub(crate) use builder::FooterEncoder;
pub use builder::SegmentBuilder;
pub use error::{Error, Result};
pub(crate) use footer::FooterValidator;
pub use footer::{Footer, FooterTrailer};
pub use header::{SegmentHeader, SegmentId};
pub use recovery::Scanner;

/// Segment and footer format version, independent of the frame version.
pub const FORMAT_VERSION: u32 = 1;
/// Identification bytes for the one-page segment header.
pub const HEADER_MAGIC: [u8; 8] = *b"MOATSEG1";
/// Identification bytes for a sealed segment's metadata footer.
pub const FOOTER_MAGIC: [u8; 8] = *b"MOATFTR1";
/// Fixed trailer at the end of a page-rounded footer.
pub const FOOTER_TRAILER_LEN: usize = 64;

const MIN_FRAME_METADATA_LEN: u64 = (crate::frame::HEADER_LEN + crate::frame::DESCRIPTOR_LEN) as u64;

pub(super) fn footer_len(metadata_len: u64) -> u64 {
    moat_common::align_up(FOOTER_TRAILER_LEN as u64 + metadata_len, moat_common::PAGE_SIZE)
}
