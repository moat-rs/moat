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

//! Persistent format, mixed placement, and hostile-input validation.

use moat_common::{AlignedBuf, CHECKSUM_BLOCK_SIZE, ChunkId, Crc32c, crc32c};
use moat_engine_v2::frame::{
    DESCRIPTOR_LEN, Error, FORMAT_VERSION, Frame, FrameBuilder, FrameHeader, FrameLimits, FramePosition, HEADER_LEN,
    MAGIC, Metadata, PreparedFrame, RecordKind,
};

const PAGE: usize = 4096;

fn limits() -> FrameLimits {
    FrameLimits::new(8 << 20, 4 << 20).unwrap()
}
fn position() -> FramePosition {
    FramePosition::new(17, PAGE as u32, 16 << 20).unwrap()
}
fn key(id: u128) -> ChunkId {
    ChunkId::from_u128(id)
}

fn encode(values: &[&[u8]]) -> Vec<u8> {
    let mut builder = FrameBuilder::new(limits());
    for (i, value) in values.iter().enumerate() {
        builder.push(key(i as u128), 1000 - i as u64, value).unwrap();
    }
    let mut bytes = vec![0xcc; builder.encoded_len()];
    builder.encode_into(position(), &mut bytes).unwrap();
    bytes
}

fn put_u32(bytes: &mut [u8], at: usize, value: u32) {
    bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

fn reseal_header(bytes: &mut [u8]) {
    let sum = Crc32c::new()
        .update(&bytes[..12])
        .update(&[0; 4])
        .update(&bytes[16..HEADER_LEN])
        .finalize();
    put_u32(bytes, 12, sum);
}

fn reseal_metadata(bytes: &mut [u8], len: usize) {
    put_u32(bytes, 44, crc32c(&bytes[HEADER_LEN..len]));
    reseal_header(bytes);
}

#[test]
fn mixed_values_use_one_encoding_and_actual_metadata_size() {
    let values = [vec![1; 100], vec![2; 4096], vec![3; 65536], vec![4; 300]];
    let refs: Vec<_> = values.iter().map(Vec::as_slice).collect();
    let bytes = encode(&refs);
    let frame = Frame::decode(&bytes, limits(), position()).unwrap();
    assert_eq!(frame.metadata().header().metadata_len(), 336);
    assert_eq!(bytes.len(), 76 << 10); // Sequential baseline, without gap filling.
    let offsets: Vec<_> = frame
        .metadata()
        .records()
        .map(|r| r.descriptor().value_offset)
        .collect();
    assert_eq!(offsets, [336, 4096, 8192, 73728]);
    for (i, value) in values.iter().enumerate() {
        assert_eq!(frame.value(i as u32), Some(value.as_slice()));
        assert_eq!(
            frame.metadata().record(i as u32).unwrap().descriptor().lsn,
            1000 - i as u64
        );
    }
    assert!(bytes[436..4096].iter().all(|&b| b == 0));
    assert!(bytes[74028..].iter().all(|&b| b == 0));
}

#[test]
fn directory_can_span_pages_and_small_values_share_metadata_page() {
    let values: Vec<_> = (0..128).map(|_| &b"payload"[..]).collect();
    let bytes = encode(&values);
    let frame = Frame::decode(&bytes, limits(), position()).unwrap();
    let header = frame.metadata().header();
    assert_eq!(header.record_count(), 128);
    assert_eq!(header.metadata_len(), 64 + 128 * 68);
    assert_eq!(
        frame.metadata().record(0).unwrap().descriptor().value_offset,
        header.metadata_len() as u32
    );
    assert!(header.metadata_len() > PAGE);
    for i in 0..128 {
        assert_eq!(frame.value(i), Some(&b"payload"[..]));
    }
}

#[test]
fn empty_data_and_tombstones_remain_distinct() {
    let mut builder = FrameBuilder::new(limits());
    builder.push(key(1), 9, &[]).unwrap();
    builder.push_tombstone(key(1), 10).unwrap();
    let mut bytes = vec![0xff; builder.encoded_len()];
    builder.encode_into(position(), &mut bytes).unwrap();
    let frame = Frame::decode(&bytes, limits(), position()).unwrap();
    assert_eq!(bytes.len(), PAGE);
    assert_eq!(frame.value(0), Some(&[][..]));
    assert_eq!(frame.value(1), None);
    assert_eq!(frame.value(2), None);
    for record in frame.metadata().records() {
        let d = record.descriptor();
        assert_eq!(
            (d.value_offset, d.value_len, d.checksum_offset, d.checksum_count),
            (0, 0, 0, 0)
        );
        assert!(record.checksum(0).is_none());
    }
    assert_eq!(
        frame.metadata().record(1).unwrap().descriptor().kind,
        RecordKind::Tombstone
    );
    assert!(bytes[192..].iter().all(|&b| b == 0));
}

#[test]
fn prepared_value_stays_in_place_and_padding_is_initialized() {
    let value_len = (3 * CHECKSUM_BLOCK_SIZE + 13) as u32;
    let len = PreparedFrame::required_len(limits(), value_len).unwrap();
    let mut buffer = AlignedBuf::zeroed(len + PAGE);
    buffer.fill(0xee);
    let base = buffer.as_ptr() as usize;
    let mut prepared = PreparedFrame::new(limits(), value_len, &mut buffer).unwrap();
    let payload_address = prepared.value_mut().as_ptr() as usize;
    assert_eq!(payload_address - base, PAGE);
    assert_eq!(payload_address % PAGE, 0);
    for (i, byte) in prepared.value_mut().iter_mut().enumerate() {
        *byte = (i % 251) as u8;
    }
    let bytes = prepared.finish(position(), key(3), 44).unwrap();
    let frame = Frame::decode(bytes, limits(), position()).unwrap();
    assert_eq!(frame.value(0).unwrap().as_ptr() as usize, payload_address);
    assert_eq!(frame.value(0).unwrap().len(), value_len as usize);
    assert!(
        frame
            .value(0)
            .unwrap()
            .iter()
            .enumerate()
            .all(|(i, &byte)| byte == (i % 251) as u8)
    );
    assert!(
        bytes[frame.metadata().header().metadata_len()..PAGE]
            .iter()
            .all(|&b| b == 0)
    );
    assert!(bytes[PAGE + value_len as usize..].iter().all(|&b| b == 0));
    assert!(buffer[len..].iter().all(|&b| b == 0xee));
}

#[test]
fn prepared_empty_value_has_no_payload_page() {
    let mut buffer = [0xff; PAGE];
    let mut prepared = PreparedFrame::new(limits(), 0, &mut buffer).unwrap();
    assert!(prepared.value_mut().is_empty());
    let bytes = prepared.finish(position(), key(0), 0).unwrap();
    let frame = Frame::decode(bytes, limits(), position()).unwrap();
    assert_eq!(frame.value(0), Some(&[][..]));
    assert!(bytes[128..].iter().all(|&byte| byte == 0));
}

#[test]
fn accepts_reordered_nonoverlapping_values_and_rejects_overlap() {
    let values = [vec![1; 100], vec![2; 4096], vec![3; 65536], vec![4; 300]];
    let refs: Vec<_> = values.iter().map(Vec::as_slice).collect();
    let mut bytes = encode(&refs);
    bytes[440..740].copy_from_slice(&values[3]);
    bytes[73728..].fill(0);
    bytes.truncate(72 << 10);
    put_u32(&mut bytes, 28, (72 << 10) as u32);
    put_u32(&mut bytes, HEADER_LEN + 3 * DESCRIPTOR_LEN + 24, 440);
    reseal_metadata(&mut bytes, 336);
    let frame = Frame::decode(&bytes, limits(), position()).unwrap();
    assert_eq!(frame.value(3), Some(values[3].as_slice()));
    assert_eq!(frame.value(2), Some(values[2].as_slice()));
    put_u32(&mut bytes, HEADER_LEN + 3 * DESCRIPTOR_LEN + 24, 432);
    reseal_metadata(&mut bytes, 336);
    assert_eq!(
        Metadata::decode(&bytes, limits(), position()).unwrap_err(),
        Error::Corrupt("overlapping values")
    );
}

#[test]
fn range_verification_checks_only_complete_requested_blocks() {
    let value: Vec<_> = (0..3 * CHECKSUM_BLOCK_SIZE + 13).map(|i| (i % 251) as u8).collect();
    let bytes = encode(&[&value]);
    let metadata = Metadata::decode(&bytes, limits(), position()).unwrap();
    let record = metadata.record(0).unwrap();
    let block = CHECKSUM_BLOCK_SIZE as u32;
    let range = record.verification_range(block + 5..2 * block + 1).unwrap();
    assert_eq!(range, block..3 * block);
    record
        .verify(range.clone(), &value[range.start as usize..range.end as usize])
        .unwrap();
    let tail = record.verification_range(3 * block + 5..3 * block + 13).unwrap();
    record
        .verify(tail.clone(), &value[tail.start as usize..tail.end as usize])
        .unwrap();
    assert!(record.verify(block + 5..block + 6, &value[5..6]).is_err());
    assert!(record.verify(0..block, &value[..10]).is_err());
    assert!(
        record
            .verification_range(std::ops::Range { start: 10, end: 9 })
            .is_err()
    );
    assert!(record.verification_range(0..u32::MAX).is_err());
    record.verify(5..5, &[]).unwrap();

    let mut damaged = value.clone();
    damaged[0] ^= 1; // Outside this range; a partial read need not fetch block 0.
    record
        .verify(block..2 * block, &damaged[block as usize..2 * block as usize])
        .unwrap();
    damaged[2 * block as usize + 5] ^= 1;
    assert_eq!(
        record.verify(block..3 * block, &damaged[block as usize..3 * block as usize]),
        Err(Error::PayloadChecksum { record: 0, block: 2 })
    );
}

#[test]
fn a_torn_payload_rejects_the_entire_frame() {
    let bytes = encode(&[b"first", &[7; 70000]]);
    let metadata = Metadata::decode(&bytes, limits(), position()).unwrap();
    let descriptor = metadata.record(1).unwrap().descriptor();
    let mut damaged = bytes.clone();
    damaged[descriptor.value_offset as usize + CHECKSUM_BLOCK_SIZE] ^= 1;
    assert!(Metadata::decode(&damaged, limits(), position()).is_ok());
    assert_eq!(
        Frame::decode(&damaged, limits(), position()).unwrap_err(),
        Error::PayloadChecksum { record: 1, block: 1 }
    );
    for len in [0, 1, 63, 64, 127, 128, 199, PAGE, bytes.len() - 1] {
        assert!(
            Frame::decode(&bytes[..len], limits(), position()).is_err(),
            "length {len}"
        );
    }
}

#[test]
fn every_header_and_metadata_byte_is_checksum_protected() {
    let bytes = encode(&[b"test"]);
    let metadata_len = FrameHeader::decode(&bytes, limits(), position())
        .unwrap()
        .metadata_len();
    for at in 0..metadata_len {
        let mut damaged = bytes.clone();
        damaged[at] ^= 1;
        assert!(Metadata::decode(&damaged, limits(), position()).is_err(), "byte {at}");
    }
}

#[test]
fn valid_crcs_do_not_bypass_header_bounds_or_version_checks() {
    let original = encode(&[b"test"]);
    let bad_fields = [
        (24, 2 * PAGE as u32),
        (28, 0),
        (28, 65),
        (28, 16 << 20),
        (32, 0),
        (32, u32::MAX),
        (36, u32::MAX),
        (40, u32::MAX),
        (40, 4096),
    ];
    for (offset, value) in bad_fields {
        let mut bytes = original.clone();
        put_u32(&mut bytes, offset, value);
        reseal_header(&mut bytes);
        assert!(
            FrameHeader::decode(&bytes, limits(), position()).is_err(),
            "field {offset}: {value}"
        );
    }
    let mut bytes = original.clone();
    put_u32(&mut bytes, 8, FORMAT_VERSION + 1);
    reseal_header(&mut bytes);
    assert_eq!(
        FrameHeader::decode(&bytes, limits(), position()).unwrap_err(),
        Error::UnsupportedVersion(FORMAT_VERSION + 1)
    );
    bytes[48] = 1;
    put_u32(&mut bytes, 8, FORMAT_VERSION);
    reseal_header(&mut bytes);
    assert!(FrameHeader::decode(&bytes, limits(), position()).is_err());
    assert!(
        FrameHeader::decode(
            &original,
            limits(),
            FramePosition::new(18, PAGE as u32, 16 << 20).unwrap()
        )
        .is_err()
    );
    assert!(FrameHeader::decode(&original, FrameLimits::new(PAGE as u32, 1).unwrap(), position()).is_ok());
    assert!(Metadata::decode(&original, FrameLimits::new(PAGE as u32, 1).unwrap(), position()).is_err());
    bytes[..8].copy_from_slice(b"MOATBAT1");
    reseal_header(&mut bytes);
    assert!(FrameHeader::decode(&bytes, limits(), position()).is_err());
}

#[test]
fn forged_descriptors_are_rejected_before_payload_access() {
    let original = encode(&[b"test"]);
    let metadata_len = FrameHeader::decode(&original, limits(), position())
        .unwrap()
        .metadata_len();
    let bad_fields = [
        (24, 0),
        (24, 128),
        (24, 137),
        (24, u32::MAX - 7),
        (28, u32::MAX),
        (32, 0),
        (32, u32::MAX),
        (36, 0),
        (36, 2),
        (36, u32::MAX),
    ];
    for (offset, value) in bad_fields {
        let mut bytes = original.clone();
        put_u32(&mut bytes, HEADER_LEN + offset, value);
        reseal_metadata(&mut bytes, metadata_len);
        assert!(
            Metadata::decode(&bytes, limits(), position()).is_err(),
            "field {offset}: {value}"
        );
    }
    for (offset, value) in [(40, 0), (40, 2), (40, 3), (41, 1), (42, 1), (63, 1)] {
        let mut bytes = original.clone();
        bytes[HEADER_LEN + offset] = value;
        reseal_metadata(&mut bytes, metadata_len);
        assert!(Metadata::decode(&bytes, limits(), position()).is_err());
    }
    let mut bytes = encode(&[&[]]);
    put_u32(&mut bytes, HEADER_LEN + 24, 128);
    reseal_metadata(&mut bytes, 128);
    assert!(Metadata::decode(&bytes, limits(), position()).is_err());
}

#[test]
fn failed_admission_and_encoding_preserve_accepted_work() {
    let limits = FrameLimits::new(PAGE as u32, PAGE as u32).unwrap();
    let mut builder = FrameBuilder::new(limits);
    builder.push(key(1), 1, &[7; 100]).unwrap();
    let before = (builder.len(), builder.metadata_len(), builder.encoded_len());
    assert!(builder.push(key(2), 2, &[8; PAGE]).is_err());
    assert_eq!((builder.len(), builder.metadata_len(), builder.encoded_len()), before);
    let mut short = [0xab; 100];
    assert!(builder.encode_into(position(), &mut short).is_err());
    assert_eq!(short, [0xab; 100]);
    let mut bytes = vec![0; builder.encoded_len()];
    builder.encode_into(position(), &mut bytes).unwrap();
    assert_eq!(
        Frame::decode(&bytes, limits, position()).unwrap().value(0),
        Some(&[7; 100][..])
    );
    builder.clear();
    assert!(builder.is_empty());
    assert_eq!(builder.encoded_len(), 0);
    assert!(builder.encode_into(position(), &mut bytes).is_err());
    assert!(PreparedFrame::required_len(limits, PAGE as u32).is_err());
}

#[test]
fn capacity_errors_distinguish_rejection_from_frame_segment_and_buffer_changes() {
    let limits = FrameLimits::new(8192, 4096).unwrap();
    let mut builder = FrameBuilder::new(limits);
    assert_eq!(
        builder.push(key(1), 1, &[7; 4097]),
        Err(Error::ValueTooLarge { len: 4097, max: 4096 })
    );
    assert!(builder.is_empty());

    builder.push(key(1), 1, &[7; 4096]).unwrap();
    assert_eq!(
        builder.push(key(2), 2, &[8; 4096]),
        Err(Error::FrameFull {
            required: 12288,
            limit: 8192
        })
    );
    assert_eq!(builder.len(), 1);

    let mut short = [0xab; 4096];
    assert_eq!(
        builder.encode_into(position(), &mut short),
        Err(Error::BufferTooSmall {
            required: 8192,
            available: 4096
        })
    );
    assert_eq!(short, [0xab; 4096]);

    let mut output = [0xab; 8192];
    let near_end = FramePosition::new(17, 4096, 8192).unwrap();
    assert_eq!(
        builder.encode_into(near_end, &mut output),
        Err(Error::SegmentFull {
            required: 8192,
            available: 4096
        })
    );
    assert_eq!(output, [0xab; 8192]);
    builder.encode_into(position(), &mut output).unwrap();
    assert_eq!(
        Frame::decode(&output, limits, position()).unwrap().value(0),
        Some(&[7; 4096][..])
    );
}

#[test]
fn prepared_errors_preserve_payload_and_report_the_failing_limit() {
    let limits = FrameLimits::new(8192, 4096).unwrap();
    assert_eq!(
        PreparedFrame::required_len(limits, 4097),
        Err(Error::ValueTooLarge { len: 4097, max: 4096 })
    );
    assert_eq!(
        PreparedFrame::required_len(FrameLimits::new(4096, 4096).unwrap(), 4096),
        Err(Error::FrameFull {
            required: 8192,
            limit: 4096
        })
    );
    let mut short = [0xab; 4096];
    assert!(matches!(
        PreparedFrame::new(limits, 4096, &mut short),
        Err(Error::BufferTooSmall {
            required: 8192,
            available: 4096
        })
    ));
    assert_eq!(short, [0xab; 4096]);

    let mut output = [0xab; 8192];
    let mut prepared = PreparedFrame::new(limits, 4096, &mut output).unwrap();
    prepared.value_mut().fill(7);
    let near_end = FramePosition::new(17, 4096, 8192).unwrap();
    assert_eq!(
        prepared.finish(near_end, key(1), 1),
        Err(Error::SegmentFull {
            required: 8192,
            available: 4096
        })
    );
    assert_eq!(output[..4096], [0xab; 4096]);
    assert_eq!(output[4096..], [7; 4096]);
    let prepared = PreparedFrame::new(limits, 4096, &mut output).unwrap();
    let bytes = prepared.finish(position(), key(1), 1).unwrap();
    assert_eq!(
        Frame::decode(bytes, limits, position()).unwrap().value(0),
        Some(&[7; 4096][..])
    );
}

#[test]
fn codec_errors_remain_small_and_thread_safe() {
    fn assert_traits<T: std::error::Error + Send + Sync + 'static>() {}
    assert_traits::<Error>();
    // Expected admission failures travel through the writer's normal path.
    // Keep error storage bounded as diagnostics evolve; this is not an ABI.
    assert!(std::mem::size_of::<Error>() <= 24);
}

#[test]
fn format_bounds_are_independent_of_a_smaller_future_batch_target() {
    let value = vec![9; 2 << 20];
    let bytes = encode(&[&value]);
    assert!(bytes.len() > 1 << 20);
    assert!(Frame::decode(&bytes, limits(), position()).is_ok());
    let small_format = FrameLimits::new(1 << 20, 1 << 20).unwrap();
    assert!(FrameHeader::decode(&bytes, small_format, position()).is_err());
}

#[test]
fn invalid_positions_and_limits_fail_without_allocation() {
    for max in [0, 1, 4095, 4097, u32::MAX] {
        assert!(FrameLimits::new(max, 0).is_err());
    }
    assert!(FrameLimits::new(PAGE as u32, PAGE as u32 + 1).is_err());
    for (offset, len) in [(0, 8192), (1, 8192), (4096, 4096), (8192, 4096), (4096, u32::MAX)] {
        assert!(FramePosition::new(1, offset, len).is_err());
    }
    let mut bytes = encode(&[&[7; 5000]]);
    let before = bytes.clone();
    let mut builder = FrameBuilder::new(limits());
    builder.push(key(1), 1, &[7; 5000]).unwrap();
    let near_end = FramePosition::new(1, 4096, 8192).unwrap();
    assert!(builder.encode_into(near_end, &mut bytes).is_err());
    assert_eq!(bytes, before);
}

#[test]
fn admission_matches_exact_layout_across_alignment_boundaries() {
    // Deterministic randomized coverage, including the final page where the
    // builder's O(1) bound falls back to exact placement. No extra dependency.
    let mut state = 0x723a_b419_0123_4567u64;
    let sizes = [0, 1, 7, 8, 100, 511, 1024, 3964, 4095, 4096, 4097, 65535, 65536, 65537];
    let values: Vec<_> = sizes.iter().map(|&size| vec![7; size]).collect();
    for pages in [1, 2, 3, 8, 17, 32, 64] {
        for _ in 0..40 {
            let max = pages * PAGE as u32;
            let bound = FrameLimits::new(max, max).unwrap();
            let mut builder = FrameBuilder::new(bound);
            let mut accepted: Vec<usize> = Vec::new();
            for ordinal in 0..100 {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                let index = state as usize % sizes.len();
                let mut reference = FrameBuilder::new(limits());
                for &i in &accepted {
                    reference.push(key(i as u128), 0, &values[i]).unwrap();
                }
                reference.push(key(index as u128), ordinal, &values[index]).unwrap();
                let should_fit = reference.encoded_len() <= max as usize;
                assert_eq!(
                    builder.push(key(index as u128), ordinal, &values[index]).is_ok(),
                    should_fit
                );
                if should_fit {
                    accepted.push(index);
                }
            }
            let mut bytes = vec![0xcc; builder.encoded_len()];
            if !bytes.is_empty() {
                builder.encode_into(position(), &mut bytes).unwrap();
                let frame = Frame::decode(&bytes, bound, position()).unwrap();
                assert_eq!(frame.metadata().header().record_count() as usize, accepted.len());
                for (i, &index) in accepted.iter().enumerate() {
                    assert_eq!(frame.value(i as u32), Some(values[index].as_slice()));
                }
            }
        }
    }
}

#[test]
fn header_field_offsets_match_the_persistent_specification() {
    let bytes = encode(&[b"test"]);
    assert_eq!(&bytes[..8], &MAGIC);
    assert_eq!(&bytes[8..12], &2u32.to_le_bytes());
    assert_eq!(&bytes[16..24], &17u64.to_le_bytes());
    assert_eq!(&bytes[24..28], &4096u32.to_le_bytes());
    assert_eq!(&bytes[28..32], &4096u32.to_le_bytes());
    assert_eq!(&bytes[32..36], &1u32.to_le_bytes());
    assert_eq!(&bytes[36..40], &64u32.to_le_bytes());
    assert_eq!(&bytes[40..44], &4u32.to_le_bytes());
    assert!(bytes[48..64].iter().all(|&byte| byte == 0));
    assert_eq!(&bytes[64..80], key(0).as_bytes());
    assert_eq!(&bytes[80..88], &1000u64.to_le_bytes());
    assert_eq!(&bytes[88..92], &136u32.to_le_bytes());
    assert_eq!(&bytes[92..96], &4u32.to_le_bytes());
    assert_eq!(&bytes[96..100], &128u32.to_le_bytes());
    assert_eq!(&bytes[100..104], &1u32.to_le_bytes());
    assert_eq!(bytes[104], 1);
    assert!(bytes[105..128].iter().all(|&byte| byte == 0));
}

#[test]
fn golden_header_and_metadata_match_an_independent_crc32c_encoder() {
    let bytes = encode(&[b"test"]);
    let expected: &[u8] = &[
        0x4d, 0x4f, 0x41, 0x54, 0x46, 0x52, 0x4d, 0x32, 0x02, 0x00, 0x00, 0x00, 0xc4, 0x92, 0xb8, 0x05, 0x11, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x00, 0x00, 0x10, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00,
        0x40, 0x00, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x82, 0xff, 0xde, 0x85, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xe8, 0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x88, 0x00,
        0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x80, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0xc0, 0x72, 0xa0, 0x86,
    ];
    assert_eq!(&bytes[..expected.len()], expected);
}

#[test]
fn metadata_can_be_decoded_without_reading_intervening_payloads() {
    let bytes = encode(&[&[1; 1024][..]; 32]);
    let header = FrameHeader::decode(&bytes[..64], limits(), position()).unwrap();
    assert_eq!(header.metadata_len(), 2240);
    let metadata = Metadata::decode(&bytes[..header.metadata_len()], limits(), position()).unwrap();
    let last = metadata.record(31).unwrap();
    assert_eq!(last.descriptor().value_offset, 34816);
    last.verify(0..1024, &bytes[34816..35840]).unwrap();
    assert!(Frame::decode(&bytes[..header.metadata_len()], limits(), position()).is_err());
}

#[test]
fn checksum_arrays_must_cover_the_area_once_in_directory_order() {
    let original = encode(&[b"one", b"two"]);
    let metadata_len = FrameHeader::decode(&original, limits(), position())
        .unwrap()
        .metadata_len();
    let mut bytes = original.clone();
    put_u32(&mut bytes, HEADER_LEN + DESCRIPTOR_LEN + 32, 192);
    reseal_metadata(&mut bytes, metadata_len);
    assert!(Metadata::decode(&bytes, limits(), position()).is_err());

    let mut bytes = original;
    put_u32(&mut bytes, 40, 12);
    reseal_metadata(&mut bytes, metadata_len + 4);
    assert!(Metadata::decode(&bytes, limits(), position()).is_err());
}
