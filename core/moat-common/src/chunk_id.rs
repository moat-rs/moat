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

//! The chunk identifier.

use std::{
    fmt,
    hash::{BuildHasherDefault, Hash, Hasher},
    str::FromStr,
};

/// The opaque 128-bit identifier of a chunk.
///
/// The chunkserver never interprets the bytes. Upper layers are free to encode
/// whatever they need (object hash, version, stripe index, ...) as long as the
/// identifier is unique for the content it names. A UUID is a natural choice:
/// it is exactly 128 bits, [`ChunkId::from_bytes`] accepts its bytes directly,
/// [`FromStr`] accepts both the hyphenated `8-4-4-4-12` form and 32 plain hex
/// digits, and with the `uuid` feature `From` conversions to and from
/// `uuid::Uuid` are provided.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct ChunkId([u8; 16]);

impl ChunkId {
    /// The number of bytes in a chunk identifier.
    pub const LEN: usize = 16;

    /// Wraps raw bytes as a chunk identifier.
    #[inline]
    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    /// Builds a chunk identifier from a `u128` (big-endian byte order).
    #[inline]
    pub const fn from_u128(value: u128) -> Self {
        Self(value.to_be_bytes())
    }

    /// Returns the raw bytes.
    #[inline]
    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }

    /// Returns the identifier as a `u128` (big-endian byte order).
    #[inline]
    pub const fn to_u128(&self) -> u128 {
        u128::from_be_bytes(self.0)
    }

    /// Returns a well-mixed 64-bit hash of the identifier.
    ///
    /// Identifiers are user supplied and may be poorly distributed (sequential
    /// counters, common prefixes). This mix is what index sharding and disk
    /// placement use, so every consumer sees the same uniform distribution.
    #[inline]
    pub fn mix(&self) -> u64 {
        let lo = u64::from_le_bytes(self.0[..8].try_into().unwrap());
        let hi = u64::from_le_bytes(self.0[8..].try_into().unwrap());
        splitmix64(lo ^ splitmix64(hi ^ 0x9E37_79B9_7F4A_7C15))
    }
}

/// The SplitMix64 finalizer: a cheap, high quality 64-bit bijection.
#[inline]
const fn splitmix64(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

impl Hash for ChunkId {
    #[inline]
    fn hash<H: Hasher>(&self, state: &mut H) {
        state.write_u64(self.mix());
    }
}

impl fmt::Debug for ChunkId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ChunkId({self})")
    }
}

impl fmt::Display for ChunkId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

/// Error returned when parsing a chunk identifier from text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseChunkIdError;

impl fmt::Display for ParseChunkIdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("chunk id must be 32 hexadecimal digits, optionally in UUID 8-4-4-4-12 form")
    }
}

impl std::error::Error for ParseChunkIdError {}

impl FromStr for ChunkId {
    type Err = ParseChunkIdError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.as_bytes();
        // Hyphens are accepted only where the UUID text form places them.
        let digits: Vec<u8> = match s.len() {
            32 => s.to_vec(),
            36 => {
                if [8, 13, 18, 23].iter().any(|&i| s[i] != b'-') {
                    return Err(ParseChunkIdError);
                }
                s.iter().copied().filter(|&c| c != b'-').collect()
            }
            _ => return Err(ParseChunkIdError),
        };
        let mut out = [0u8; 16];
        for (i, pair) in digits.as_chunks::<2>().0.iter().enumerate() {
            let hi = hex_nibble(pair[0]).ok_or(ParseChunkIdError)?;
            let lo = hex_nibble(pair[1]).ok_or(ParseChunkIdError)?;
            out[i] = (hi << 4) | lo;
        }
        Ok(Self(out))
    }
}

fn hex_nibble(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

#[cfg(feature = "uuid")]
impl From<uuid::Uuid> for ChunkId {
    #[inline]
    fn from(uuid: uuid::Uuid) -> Self {
        Self(uuid.into_bytes())
    }
}

#[cfg(feature = "uuid")]
impl From<ChunkId> for uuid::Uuid {
    #[inline]
    fn from(id: ChunkId) -> Self {
        uuid::Uuid::from_bytes(id.0)
    }
}

/// A [`Hasher`] that passes the pre-mixed [`ChunkId::mix`] value through
/// unchanged.
///
/// `ChunkId::hash` already produces a uniformly distributed 64-bit value, so
/// hash maps keyed by chunk identifiers do not need a second mixing pass.
#[derive(Default, Clone, Copy)]
pub struct ChunkIdHasher(u64);

impl Hasher for ChunkIdHasher {
    #[inline]
    fn finish(&self) -> u64 {
        self.0
    }

    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        // Only reached if a key other than `ChunkId` is hashed; fold bytes so
        // the hasher stays correct (if slow) for any key type.
        for chunk in bytes.chunks(8) {
            let mut word = [0u8; 8];
            word[..chunk.len()].copy_from_slice(chunk);
            self.0 = splitmix64(self.0 ^ u64::from_le_bytes(word));
        }
    }

    #[inline]
    fn write_u64(&mut self, value: u64) {
        self.0 = value;
    }
}

/// The [`std::hash::BuildHasher`] to use for maps keyed by [`ChunkId`].
pub type ChunkIdHashBuilder = BuildHasherDefault<ChunkIdHasher>;

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn roundtrip_text() {
        let id = ChunkId::from_u128(0x0123_4567_89ab_cdef_fedc_ba98_7654_3210);
        let text = id.to_string();
        assert_eq!(text, "0123456789abcdeffedcba9876543210");
        assert_eq!(text.parse::<ChunkId>().unwrap(), id);
        assert!("0123".parse::<ChunkId>().is_err());
        assert!("zz23456789abcdeffedcba9876543210".parse::<ChunkId>().is_err());
    }

    #[test]
    fn parses_uuid_text_form() {
        let id: ChunkId = "550e8400-e29b-41d4-A716-446655440000".parse().unwrap();
        assert_eq!(id, ChunkId::from_u128(0x550e_8400_e29b_41d4_a716_4466_5544_0000));
        assert_eq!(id, "550e8400e29b41d4a716446655440000".parse().unwrap());
        // Hyphens only where UUIDs put them.
        assert!("550e8400e2-9b-41d4-a716-446655440000".parse::<ChunkId>().is_err());
        assert!("550e8400-e29b-41d4-a716-4466554400000".parse::<ChunkId>().is_err());
    }

    #[cfg(feature = "uuid")]
    #[test]
    fn uuid_roundtrip() {
        let uuid = uuid::Uuid::from_u128(0x0123_4567_89ab_cdef_fedc_ba98_7654_3210);
        let id = ChunkId::from(uuid);
        assert_eq!(id.as_bytes(), uuid.as_bytes());
        assert_eq!(uuid::Uuid::from(id), uuid);
        assert_eq!(id.to_string(), uuid.simple().to_string());
    }

    #[test]
    fn sequential_ids_mix_well() {
        // Sequential identifiers must not collide in the low bits used for
        // sharding: check the low 6 bits spread over all 64 values.
        let mut buckets = HashSet::new();
        for i in 0..4096u128 {
            buckets.insert(ChunkId::from_u128(i).mix() & 63);
        }
        assert_eq!(buckets.len(), 64);
    }
}
