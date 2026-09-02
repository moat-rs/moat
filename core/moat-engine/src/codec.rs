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

//! Little-endian fixed-layout encoding helpers.
//!
//! On-disk structures are encoded field by field into fixed-size byte arrays.
//! This keeps the wire format independent of Rust struct layout and avoids any
//! unsafe transmutes.

use moat_common::crc32c;

/// Writes fixed-width little-endian fields into a byte slice.
pub(crate) struct Writer<'a> {
    buf: &'a mut [u8],
    pos: usize,
}

impl<'a> Writer<'a> {
    pub(crate) fn new(buf: &'a mut [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    pub(crate) fn position(&self) -> usize {
        self.pos
    }

    pub(crate) fn u8(&mut self, v: u8) -> &mut Self {
        self.bytes(&[v])
    }

    pub(crate) fn u16(&mut self, v: u16) -> &mut Self {
        self.bytes(&v.to_le_bytes())
    }

    pub(crate) fn u32(&mut self, v: u32) -> &mut Self {
        self.bytes(&v.to_le_bytes())
    }

    pub(crate) fn u64(&mut self, v: u64) -> &mut Self {
        self.bytes(&v.to_le_bytes())
    }

    pub(crate) fn bytes(&mut self, v: &[u8]) -> &mut Self {
        self.buf[self.pos..self.pos + v.len()].copy_from_slice(v);
        self.pos += v.len();
        self
    }

    /// Skips `n` bytes, leaving them untouched (callers pre-zero buffers).
    pub(crate) fn skip(&mut self, n: usize) -> &mut Self {
        self.pos += n;
        self
    }
}

/// Reads fixed-width little-endian fields from a byte slice.
pub(crate) struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub(crate) fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    pub(crate) fn u8(&mut self) -> u8 {
        let v = self.buf[self.pos];
        self.pos += 1;
        v
    }

    pub(crate) fn u32(&mut self) -> u32 {
        u32::from_le_bytes(self.array())
    }

    pub(crate) fn u64(&mut self) -> u64 {
        u64::from_le_bytes(self.array())
    }

    pub(crate) fn array<const N: usize>(&mut self) -> [u8; N] {
        let out: [u8; N] = self.buf[self.pos..self.pos + N]
            .try_into()
            .expect("bounds checked by slice");
        self.pos += N;
        out
    }

    pub(crate) fn skip(&mut self, n: usize) -> &mut Self {
        self.pos += n;
        self
    }
}

/// Computes the CRC32C of everything in `buf` after the 4-byte CRC field at
/// `crc_offset`.
///
/// Every on-disk structure starts with a magic value followed by its CRC. The
/// magic is checked by comparison, so the CRC only needs to protect what
/// follows it: one contiguous slice, nothing to mask out.
fn checksum_since(buf: &[u8], crc_offset: usize) -> u32 {
    crc32c(&buf[crc_offset + 4..])
}

/// Computes the checksum of the bytes after `crc_offset` and stores it in the
/// CRC field there.
pub(crate) fn seal(buf: &mut [u8], crc_offset: usize) {
    let crc = checksum_since(buf, crc_offset);
    buf[crc_offset..crc_offset + 4].copy_from_slice(&crc.to_le_bytes());
}

/// Checks that the CRC field at `crc_offset` matches the bytes after it.
pub(crate) fn verify(buf: &[u8], crc_offset: usize) -> bool {
    let stored = u32::from_le_bytes(buf[crc_offset..crc_offset + 4].try_into().expect("4 bytes"));
    stored == checksum_since(buf, crc_offset)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seal_and_verify() {
        let mut buf = vec![0u8; 64];
        Writer::new(&mut buf).u64(1).u32(0).u32(0xdead_beef).bytes(b"hello");
        seal(&mut buf, 8);
        assert!(verify(&buf, 8));
        buf[20] ^= 1;
        assert!(!verify(&buf, 8));
        buf[20] ^= 1;
        assert!(verify(&buf, 8));
        // Bytes before the CRC field (the magic) are not covered; callers
        // compare them directly.
        buf[0] ^= 1;
        assert!(verify(&buf, 8));
        buf[0] ^= 1;
        // Padding after the payload is covered.
        buf[63] ^= 1;
        assert!(!verify(&buf, 8));

        let mut r = Reader::new(&buf);
        assert_eq!(r.u64(), 1);
        let _crc = r.u32();
        assert_eq!(r.u32(), 0xdead_beef);
        assert_eq!(&r.array::<5>(), b"hello");
    }
}
