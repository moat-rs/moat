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

//! Segment format, allocation accounting, and recovery fault cases.

use moat_common::{ChunkId, Crc32c, PAGE_SIZE};
use moat_engine::{
    frame::{self, FrameBuilder, FrameLimits, FramePosition, Metadata, RecordKind},
    segment::{Error, Footer, FooterTrailer, Scanner, SegmentBuilder, SegmentHeader, SegmentId},
};

const PAGE: usize = PAGE_SIZE as usize;
const CAPACITY: u32 = 64 * 1024;

fn id() -> SegmentId {
    SegmentId {
        device_id: [0x37; 16],
        segment_no: 9,
        sequence: 123,
    }
}
fn limits() -> FrameLimits {
    FrameLimits::new(16 * 1024, 8 * 1024).unwrap()
}
fn active(capacity: u32) -> SegmentHeader {
    SegmentHeader::new(id(), capacity).unwrap()
}
fn decode_header(bytes: &[u8], capacity: u32) -> Result<SegmentHeader, Error> {
    SegmentHeader::decode(bytes, id().device_id, id().segment_no, capacity)
}
fn put_u32(bytes: &mut [u8], at: usize, value: u32) {
    bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
}
fn crc(bytes: &[u8]) -> u32 {
    Crc32c::new()
        .update(&bytes[..12])
        .update(&[0; 4])
        .update(&bytes[16..])
        .finalize()
}
fn reseal(bytes: &mut [u8]) {
    put_u32(bytes, 12, crc(bytes));
}

fn reseal_footer(bytes: &mut [u8]) {
    let at = bytes.len() - 64;
    put_u32(&mut bytes[at..], 12, 0);
    put_u32(&mut bytes[at..], 60, 0);
    let sum = Crc32c::new().update(bytes).finalize();
    put_u32(&mut bytes[at..], 60, sum);
    reseal(&mut bytes[at..]);
}

fn append(builder: &mut SegmentBuilder, disk: &mut [u8], records: &[(u128, u64, Option<&[u8]>)]) -> u32 {
    let mut frame = FrameBuilder::new(limits());
    for &(key, lsn, value) in records {
        match value {
            Some(value) => frame.push(ChunkId::from_u128(key), lsn, value).unwrap(),
            None => frame.push_tombstone(ChunkId::from_u128(key), lsn).unwrap(),
        }
    }
    let position = builder.position(frame.encoded_len(), frame.metadata_len()).unwrap();
    let bytes = &mut disk[position.offset() as usize..][..frame.encoded_len()];
    frame.encode_into(position, bytes).unwrap();
    builder
        .append(Metadata::decode(bytes, limits(), position).unwrap())
        .unwrap();
    position.offset()
}

fn fixture() -> (SegmentHeader, SegmentBuilder, Vec<u8>, u32) {
    let header = active(CAPACITY);
    let mut disk = vec![0; CAPACITY as usize];
    header.encode_into(&mut disk).unwrap();
    let mut builder = SegmentBuilder::new(header).unwrap();
    append(&mut builder, &mut disk, &[(1, 20, Some(b"old")), (2, 10, Some(b""))]);
    let last = append(&mut builder, &mut disk, &[(1, 30, None), (3, 15, Some(b"new"))]);
    (header, builder, disk, last)
}

fn seal(builder: &mut SegmentBuilder, disk: &mut [u8]) -> SegmentHeader {
    let at = disk.len() - builder.footer_len();
    builder.seal_into(&mut disk[at..]).unwrap()
}

fn scan(header: SegmentHeader, disk: &[u8]) -> Result<Vec<(u128, u64, RecordKind)>, Error> {
    let mut scanner = Scanner::new(header, limits());
    let mut records = Vec::new();
    while let Some(position) = scanner.position() {
        let Some(frame) = scanner.next_frame(&disk[position.offset() as usize..])? else {
            break;
        };
        records.extend(frame.metadata().records().map(|r| {
            let d = r.descriptor();
            (d.key.to_u128(), d.lsn, d.kind)
        }));
    }
    Ok(records)
}

#[test]
fn segment_header_roundtrips_and_binds_device_number_and_geometry() {
    let header = active(CAPACITY);
    let mut bytes = vec![0xaa; PAGE + 10];
    header.encode_into(&mut bytes).unwrap();
    assert_eq!(decode_header(&bytes, CAPACITY).unwrap(), header);
    assert!(bytes[68..PAGE].iter().all(|&b| b == 0));
    assert_eq!(&bytes[PAGE..], &[0xaa; 10]);
    assert!(SegmentHeader::decode(&bytes, [0; 16], id().segment_no, CAPACITY).is_err());
    assert!(SegmentHeader::decode(&bytes, id().device_id, 10, CAPACITY).is_err());
    assert!(decode_header(&bytes, CAPACITY + PAGE as u32).is_err());
    assert_eq!(header.footer_range(), None);
}

#[test]
fn every_header_byte_is_checksum_protected() {
    let mut bytes = vec![0; PAGE];
    active(CAPACITY).encode_into(&mut bytes).unwrap();
    for at in 0..PAGE {
        bytes[at] ^= 1;
        assert!(decode_header(&bytes, CAPACITY).is_err(), "byte {at}");
        bytes[at] ^= 1;
    }
}

#[test]
fn valid_crc_does_not_bypass_segment_state_and_reserved_fields() {
    let mut original = vec![0; PAGE];
    active(CAPACITY).encode_into(&mut original).unwrap();
    for (at, value) in [
        (40, 0),
        (48, 2),
        (52, PAGE as u32),
        (56, PAGE as u32),
        (60, 1),
        (64, 128),
        (68, 1),
    ] {
        let mut bytes = original.clone();
        put_u32(&mut bytes, at, value);
        reseal(&mut bytes);
        assert!(decode_header(&bytes, CAPACITY).is_err(), "field {at}");
    }
    put_u32(&mut original, 8, 99);
    reseal(&mut original);
    assert!(matches!(
        decode_header(&original, CAPACITY),
        Err(Error::UnsupportedVersion(99))
    ));
}

#[test]
fn invalid_and_extreme_geometry_fail_without_large_allocations() {
    assert!(SegmentHeader::new(SegmentId { sequence: 0, ..id() }, CAPACITY).is_err());
    for capacity in [0, PAGE as u32, CAPACITY + 1, u32::MAX] {
        assert!(SegmentHeader::new(id(), capacity).is_err());
    }
    let capacity = u32::MAX - (PAGE as u32 - 1);
    let header = active(capacity);
    let builder = SegmentBuilder::new(header).unwrap();
    assert!(
        matches!(builder.position(capacity as usize, 128), Err(Error::Full { required, .. }) if required > u32::MAX as u64)
    );
    let mut short = vec![0x55; PAGE - 1];
    assert!(matches!(
        header.encode_into(&mut short),
        Err(Error::BufferTooSmall { .. })
    ));
    assert!(short.iter().all(|&b| b == 0x55));
}

#[test]
fn footer_reservation_accounts_for_the_next_page_before_submission() {
    let capacity = 4 * PAGE as u32;
    let mut disk = vec![0; capacity as usize];
    let mut builder = SegmentBuilder::new(active(capacity)).unwrap();
    let records: Vec<_> = (0..60).map(|key| (key, key as u64, None)).collect();
    append(&mut builder, &mut disk, &records);
    assert_eq!(builder.footer_len(), PAGE);
    assert!(builder.position(PAGE, 128).is_ok());
    // The data frame fits, but these two descriptors grow the footer to 8 KiB.
    assert!(
        matches!(builder.position(PAGE, 192), Err(Error::Full { required, capacity: cap }) if required == 5 * PAGE as u64 && cap == capacity)
    );
    assert_eq!(builder.data_end(), 2 * PAGE as u32);
    assert_eq!(builder.footer_len(), PAGE);
}

#[test]
fn admission_rejects_wrong_position_and_preserves_allocated_tail() {
    let mut builder = SegmentBuilder::new(active(CAPACITY)).unwrap();
    let mut frame = FrameBuilder::new(limits());
    frame.push(ChunkId::from_u128(1), 1, b"value").unwrap();
    for (sequence, offset) in [(id().sequence + 1, PAGE as u32), (id().sequence, 2 * PAGE as u32)] {
        let position = FramePosition::new(sequence, offset, CAPACITY).unwrap();
        let mut bytes = vec![0; frame.encoded_len()];
        frame.encode_into(position, &mut bytes).unwrap();
        assert!(matches!(
            builder.append(Metadata::decode(&bytes, limits(), position).unwrap()),
            Err(Error::InvalidArgument(_))
        ));
        assert_eq!(builder.data_end(), PAGE as u32);
    }
    for (frame_len, metadata_len) in [(0, 128), (PAGE + 1, 128), (PAGE, 0), (PAGE, 129), (PAGE, PAGE + 4)] {
        assert!(matches!(
            builder.position(frame_len, metadata_len),
            Err(Error::InvalidArgument(_))
        ));
    }
}

#[test]
fn short_seal_output_is_retryable_and_sealing_stops_admission() {
    let (_, mut builder, mut disk, _) = fixture();
    let mut short = vec![0xab; builder.footer_len() - 1];
    assert!(matches!(
        builder.seal_into(&mut short),
        Err(Error::BufferTooSmall { .. })
    ));
    assert!(short.iter().all(|&b| b == 0xab));
    assert!(builder.position(PAGE, 128).is_ok());
    let header = seal(&mut builder, &mut disk);
    assert_eq!(decode_header(&disk, CAPACITY).unwrap(), active(CAPACITY));
    assert_eq!(FooterTrailer::decode(&disk, CAPACITY).unwrap().header(), header);
    assert!(header.encode_into(&mut disk).is_err());
    assert!(matches!(builder.position(PAGE, 128), Err(Error::Sealed)));
    assert!(matches!(builder.seal_into(&mut disk), Err(Error::Sealed)));
    assert!(matches!(SegmentBuilder::new(header), Err(Error::Sealed)));
}

#[test]
fn footer_reuses_metadata_and_preserves_tombstones_empty_values_and_lsn_order() {
    let (_, mut builder, mut disk, _) = fixture();
    let header = seal(&mut builder, &mut disk);
    let range = header.footer_range().unwrap();
    let footer = Footer::decode(&disk[range.start as usize..range.end as usize], header, limits()).unwrap();
    let mut frames = footer.frames();
    assert_eq!(frames.len(), 2);
    let first = frames.next().unwrap();
    assert_eq!(first.as_bytes(), &disk[PAGE..PAGE + first.as_bytes().len()]);
    assert_eq!(first.record(1).unwrap().descriptor().kind, RecordKind::Data);
    assert_eq!(first.record(1).unwrap().descriptor().value_len, 0);
    let second = frames.next().unwrap();
    assert_eq!(second.record(0).unwrap().descriptor().kind, RecordKind::Tombstone);
    assert_eq!(second.record(0).unwrap().descriptor().lsn, 30);
    assert_eq!(second.record(1).unwrap().descriptor().lsn, 15);
    assert_eq!(frames.len(), 0);
    assert_eq!(scan(header, &disk).unwrap().len(), 4);
}

#[test]
fn empty_segments_can_seal_and_recover_without_frames() {
    let mut builder = SegmentBuilder::new(active(2 * PAGE as u32)).unwrap();
    let mut disk = vec![0; 2 * PAGE];
    let header = seal(&mut builder, &mut disk);
    assert!(header.is_sealed());
    assert_eq!(
        Footer::decode(&disk[PAGE..], header, limits()).unwrap().frames().len(),
        0
    );
    let mut scanner = Scanner::new(header, limits());
    assert!(scanner.position().is_none());
    assert!(scanner.next_frame(&[]).unwrap().is_none());
    assert_eq!(scanner.data_end(), PAGE as u32);
}

#[test]
fn active_recovery_accepts_only_the_complete_frame_prefix() {
    let (header, builder, disk, _) = fixture();
    let mut scanner = Scanner::new(header, limits());
    while let Some(position) = scanner.position() {
        if scanner
            .next_frame(&disk[position.offset() as usize..])
            .unwrap()
            .is_none()
        {
            break;
        }
    }
    assert_eq!(scanner.data_end(), builder.data_end());
    assert!(scanner.tail_error().is_some());
    assert!(scanner.position().is_none());
    assert!(scanner.next_frame(&disk[PAGE..]).unwrap().is_none());
}

#[test]
fn torn_second_frame_hides_its_tombstone_and_does_not_search_later_magic() {
    let (header, mut builder, mut disk, second) = fixture();
    append(&mut builder, &mut disk, &[(99, 99, Some(b"later valid frame"))]);
    let position = FramePosition::new(id().sequence, second, CAPACITY).unwrap();
    let metadata = Metadata::decode(&disk[second as usize..], limits(), position).unwrap();
    let value = metadata.record(1).unwrap().descriptor().value_offset;
    disk[(second + value) as usize] ^= 1;
    let records = scan(header, &disk).unwrap();
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].1, 20);
    assert!(records.iter().all(|r| r.2 == RecordKind::Data));
}

#[test]
fn sealed_damage_is_an_error_even_when_footer_fallback_is_possible() {
    let (_, mut builder, mut disk, second) = fixture();
    let header = seal(&mut builder, &mut disk);
    let footer_at = header.footer_range().unwrap().start as usize;
    disk[footer_at + 70] ^= 1;
    assert!(Footer::decode(&disk[footer_at..], header, limits()).is_err());
    assert_eq!(scan(header, &disk).unwrap().len(), 4);
    disk[second as usize + 12] ^= 1;
    assert!(matches!(scan(header, &disk), Err(Error::Frame { offset, .. }) if offset == second));
}

#[test]
fn footer_without_its_trailer_does_not_claim_a_committed_seal() {
    let (header, mut builder, mut disk, _) = fixture();
    let at = disk.len() - builder.footer_len();
    let sealed = builder.seal_into(&mut disk[at..]).unwrap();
    let end = disk.len();
    disk[end - 64..].fill(0);
    assert_eq!(decode_header(&disk, CAPACITY).unwrap(), header);
    assert!(FooterTrailer::decode(&disk, CAPACITY).is_err());
    assert!(Footer::decode(&disk[at..], sealed, limits()).is_err());
    assert_eq!(scan(header, &disk).unwrap().len(), 4);
}

#[test]
fn recycled_segment_rejects_old_frames_and_old_footer() {
    let (_, mut builder, mut disk, _) = fixture();
    let old_header = seal(&mut builder, &mut disk);
    let new_header = SegmentHeader::new(
        SegmentId {
            sequence: id().sequence + 1,
            ..id()
        },
        CAPACITY,
    )
    .unwrap();
    new_header.encode_into(&mut disk).unwrap();
    let recovered = decode_header(&disk, CAPACITY).unwrap();
    assert!(scan(recovered, &disk).unwrap().is_empty());
    let mut new_builder = SegmentBuilder::new(new_header).unwrap();
    let mut new_disk = vec![0; CAPACITY as usize];
    append(&mut new_builder, &mut new_disk, &[(1, 40, Some(b"x"))]);
    let sealed = seal(&mut new_builder, &mut new_disk);
    let old_at = old_header.footer_range().unwrap().start as usize;
    assert!(Footer::decode(&disk[old_at..], sealed, limits()).is_err());
}

#[test]
fn footer_is_not_a_substitute_for_payload_verification() {
    let (_, mut builder, mut disk, _) = fixture();
    let header = seal(&mut builder, &mut disk);
    let position = FramePosition::new(id().sequence, PAGE as u32, CAPACITY).unwrap();
    let d = Metadata::decode(&disk[PAGE..], limits(), position)
        .unwrap()
        .record(0)
        .unwrap()
        .descriptor();
    disk[PAGE + d.value_offset as usize] ^= 1;
    let at = header.footer_range().unwrap().start as usize;
    let footer = Footer::decode(&disk[at..], header, limits()).unwrap();
    let record = footer.frames().next().unwrap().record(0).unwrap();
    assert!(
        record
            .verify(
                0..d.value_len,
                &disk[PAGE + d.value_offset as usize..][..d.value_len as usize]
            )
            .is_err()
    );
    assert!(scan(header, &disk).is_err());
}

#[test]
fn truncated_input_never_becomes_a_successful_sealed_prefix() {
    let (active_header, mut builder, mut disk, second) = fixture();
    let sealed = seal(&mut builder, &mut disk);
    for cut in [0, 1, PAGE - 1] {
        assert!(decode_header(&disk[..cut], CAPACITY).is_err());
    }
    for cut in [second as usize, second as usize + 10, second as usize + PAGE - 1] {
        assert_eq!(scan(active_header, &disk[..cut]).unwrap().len(), 2);
        assert!(matches!(scan(sealed, &disk[..cut]), Err(Error::Frame { offset, .. }) if offset == second));
    }
    let range = sealed.footer_range().unwrap();
    assert!(matches!(
        Footer::decode(&disk[range.start as usize..range.end as usize - 1], sealed, limits()),
        Err(Error::Truncated { .. })
    ));
}

#[test]
fn unsupported_frame_version_is_not_silently_truncated_during_recovery() {
    let (header, _, mut disk, _) = fixture();
    put_u32(&mut disk[PAGE..], 8, 99);
    reseal(&mut disk[PAGE..PAGE + frame::HEADER_LEN]);
    assert!(matches!(
        scan(header, &disk),
        Err(Error::Frame {
            source: frame::Error::UnsupportedVersion(99),
            ..
        })
    ));
}

#[test]
fn forged_footer_cannot_skip_reorder_or_cross_the_sealed_boundary() {
    let (_, mut builder, mut disk, _) = fixture();
    let header = seal(&mut builder, &mut disk);
    let range = header.footer_range().unwrap();
    let original = disk[range.start as usize..range.end as usize].to_vec();
    for (at, value) in [(32, 10), (36, 4 * PAGE as u32), (48, 1), (52, 0), (56, 0)] {
        let mut bytes = original.clone();
        let tail = bytes.len() - 64;
        put_u32(&mut bytes[tail..], at, value);
        reseal_footer(&mut bytes);
        assert!(Footer::decode(&bytes, header, limits()).is_err(), "field {at}");
    }
    let mut bytes = original.clone();
    put_u32(&mut bytes, 24, 2 * PAGE as u32);
    reseal(&mut bytes[..frame::HEADER_LEN]);
    reseal_footer(&mut bytes);
    assert!(Footer::decode(&bytes, header, limits()).is_err());
    let mut bytes = original.clone();
    let padding = bytes.len() - 65;
    bytes[padding] = 1;
    reseal_footer(&mut bytes);
    assert!(Footer::decode(&bytes, header, limits()).is_err());
}

#[test]
fn forged_sealed_summary_cannot_turn_missing_frames_into_a_clean_end() {
    let (_, mut builder, mut disk, _) = fixture();
    seal(&mut builder, &mut disk);
    let at = disk.len() - 64;
    put_u32(&mut disk[at..], 48, 1);
    reseal(&mut disk[at..]);
    let header = FooterTrailer::decode(&disk, CAPACITY).unwrap().header();
    assert!(scan(header, &disk).is_err());
}

#[test]
fn independent_crc_vectors_fix_header_and_footer_wire_fields() {
    // Constants come from a separate bitwise CRC32C encoder (0x82f63b78).
    let mut page = vec![0; PAGE];
    active(CAPACITY).encode_into(&mut page).unwrap();
    assert_eq!(&page[..12], b"MOATSEG1\x01\x00\x00\x00");
    assert_eq!(&page[12..16], &0xe0fa814u32.to_le_bytes());
    assert_eq!(&page[32..40], &[9, 0, 0, 0, 0, 0, 1, 0]);
    assert_eq!(&page[40..48], &123u64.to_le_bytes());
    let mut builder = SegmentBuilder::new(active(CAPACITY)).unwrap();
    let mut footer = vec![0; PAGE];
    let header = builder.seal_into(&mut footer).unwrap();
    let trailer = &footer[PAGE - 64..];
    assert_eq!(&trailer[..12], b"MOATFTR1\x01\x00\x00\x00");
    assert_eq!(&trailer[12..16], &0xcb370fdeu32.to_le_bytes());
    assert_eq!(&trailer[60..64], &0xbbad2e77u32.to_le_bytes());
    assert_eq!(FooterTrailer::decode(&footer, CAPACITY).unwrap().header(), header);
    assert_eq!(header.footer_range().unwrap(), CAPACITY - PAGE as u32..CAPACITY);
}

#[test]
fn every_footer_byte_is_checksum_protected() {
    let (_, mut builder, mut disk, _) = fixture();
    let header = seal(&mut builder, &mut disk);
    let range = header.footer_range().unwrap();
    let mut bytes = disk[range.start as usize..range.end as usize].to_vec();
    for at in 0..bytes.len() {
        bytes[at] ^= 1;
        assert!(Footer::decode(&bytes, header, limits()).is_err(), "byte {at}");
        bytes[at] ^= 1;
    }
}

#[test]
fn forged_sealed_geometry_is_bounded_before_footer_access() {
    let (_, mut builder, mut disk, _) = fixture();
    seal(&mut builder, &mut disk);
    for (at, value) in [
        (36, 0),
        (36, PAGE as u32 + 1),
        (36, CAPACITY),
        (56, u32::MAX),
        (48, u32::MAX),
        (52, u32::MAX),
        (52, 0),
    ] {
        let mut bytes = disk[disk.len() - 64..].to_vec();
        put_u32(&mut bytes, at, value);
        reseal(&mut bytes);
        assert!(FooterTrailer::decode(&bytes, CAPACITY).is_err(), "field {at}");
    }
    for len in 0..64 {
        assert!(FooterTrailer::decode(&disk[..len], CAPACITY).is_err());
    }
}

#[test]
fn valid_frame_that_consumes_footer_space_is_not_recovered() {
    let capacity = 3 * PAGE as u32;
    let header = active(capacity);
    let mut disk = vec![0; capacity as usize];
    let mut frame = FrameBuilder::new(limits());
    let value = vec![5; PAGE];
    frame.push(ChunkId::from_u128(1), 1, &value).unwrap();
    let position = FramePosition::new(id().sequence, PAGE as u32, capacity).unwrap();
    frame.encode_into(position, &mut disk[PAGE..]).unwrap();
    let mut scanner = Scanner::new(header, limits());
    assert!(scanner.next_frame(&disk[PAGE..]).unwrap().is_none());
    assert!(matches!(scanner.tail_error(), Some(frame::Error::Corrupt(_))));
    assert_eq!(scanner.data_end(), PAGE as u32);
}

#[test]
fn recovered_prefix_can_build_a_footer_without_rewriting_its_frames() {
    let (header, _, mut disk, second) = fixture();
    disk[second as usize + 12] ^= 1;
    let before = disk[PAGE..second as usize].to_vec();
    let mut scanner = Scanner::new(header, limits());
    let mut summary = SegmentBuilder::new(header).unwrap();
    while let Some(position) = scanner.position() {
        let Some(frame) = scanner.next_frame(&disk[position.offset() as usize..]).unwrap() else {
            break;
        };
        summary.append(frame.metadata()).unwrap();
    }
    assert_eq!(summary.data_end(), scanner.data_end());
    let sealed = seal(&mut summary, &mut disk);
    assert_eq!(&disk[PAGE..second as usize], before.as_slice());
    let at = sealed.footer_range().unwrap().start as usize;
    let footer = Footer::decode(&disk[at..], sealed, limits()).unwrap();
    assert_eq!(footer.frames().len(), 1);
    assert_eq!(
        scan(sealed, &disk).unwrap(),
        vec![(1, 20, RecordKind::Data), (2, 10, RecordKind::Data)]
    );
}
