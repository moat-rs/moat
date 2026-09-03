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

//! Page alignment helpers.
//!
//! Every on-disk structure and every direct I/O in moat is aligned to
//! [`PAGE_SIZE`]. Helpers here are generic over the alignment so they can also
//! be used for the 8-byte record alignment inside packed batches.

/// The I/O and layout alignment unit: one 4 KiB page, which is also the logical
/// block size of every NVMe device moat targets.
pub const PAGE_SIZE: u64 = 4096;

/// Rounds `value` up to the next multiple of `align`.
///
/// `align` must be a power of two.
#[inline]
pub const fn align_up(value: u64, align: u64) -> u64 {
    debug_assert!(align.is_power_of_two());
    (value + align - 1) & !(align - 1)
}

/// Rounds `value` down to the previous multiple of `align`.
///
/// `align` must be a power of two.
#[inline]
pub const fn align_down(value: u64, align: u64) -> u64 {
    debug_assert!(align.is_power_of_two());
    value & !(align - 1)
}

/// Returns whether `value` is a multiple of `align`.
///
/// `align` must be a power of two.
#[inline]
pub const fn is_aligned(value: u64, align: u64) -> bool {
    debug_assert!(align.is_power_of_two());
    value & (align - 1) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rounding() {
        assert_eq!(align_up(0, 4096), 0);
        assert_eq!(align_up(1, 4096), 4096);
        assert_eq!(align_up(4096, 4096), 4096);
        assert_eq!(align_up(4097, 4096), 8192);
        assert_eq!(align_down(4097, 4096), 4096);
        assert_eq!(align_down(4095, 4096), 0);
        assert!(is_aligned(8192, 4096));
        assert!(!is_aligned(8193, 4096));
        assert_eq!(align_up(13, 8), 16);
    }
}
