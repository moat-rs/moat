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

//! End-to-end tests of the engine on an in-memory device.
//!
//! Segments are kept tiny (1 MiB) so that a handful of writes exercises
//! rollover, sealing, reclaim and recovery.

use std::{collections::HashMap, sync::Arc};

use moat_common::{ChunkId, HugePages, PoolOptions};
use moat_engine::{
    Error, FormatOptions, ManualClock, MemDevice, Opened, Options, PutOptions, PutOutcome, QueueOptions, ReadRing,
    Reader, ReclaimPolicy,
};

const SEGMENT: u64 = 1 << 20;
const CHUNK_MAX: u32 = 128 << 10;
const FILE_CHUNK_MAX: u32 = 64 << 10;

fn format_options() -> FormatOptions {
    FormatOptions {
        segment_size: SEGMENT,
        chunk_max: CHUNK_MAX,
        disk_uuid: [0xab; 16],
    }
}

/// A small pool keeps test memory modest; plain pages avoid depending on the
/// host's huge page configuration.
fn queue_options() -> QueueOptions {
    QueueOptions {
        depth: 64,
        pool: PoolOptions {
            bytes: 16 << 20,
            max_class: 1 << 20,
            huge_pages: HugePages::Disabled,
        },
        force_sync: false,
    }
}

fn options() -> Options {
    Options {
        queue: queue_options(),
        ..Default::default()
    }
}

/// Keep the registered pools used by the file-backed test below the
/// locked-memory limit of standard hosted CI runners. The 128 KiB class still
/// accommodates the test format's 64 KiB maximum chunk plus its metadata.
fn file_queue_options() -> QueueOptions {
    QueueOptions {
        depth: 64,
        pool: PoolOptions {
            bytes: 1 << 20,
            max_class: 128 << 10,
            huge_pages: HugePages::Disabled,
        },
        force_sync: false,
    }
}

fn file_options() -> Options {
    Options {
        batch_limit: 128 << 10,
        scan_window: 128 << 10,
        queue: file_queue_options(),
        ..Default::default()
    }
}

fn file_format_options() -> FormatOptions {
    FormatOptions {
        chunk_max: FILE_CHUNK_MAX,
        ..format_options()
    }
}

fn new_device(segments: u64) -> Arc<MemDevice> {
    let device = Arc::new(MemDevice::new(SEGMENT * (segments + 1)));
    moat_engine::format(&*device, &format_options()).unwrap();
    device
}

fn open(device: &Arc<MemDevice>) -> Opened {
    moat_engine::open(device.clone(), options()).unwrap()
}

fn open_with(device: &Arc<MemDevice>, options: Options) -> Opened {
    moat_engine::open(device.clone(), options).unwrap()
}

fn open_ring(reader: &Reader) -> ReadRing {
    reader.ring(&queue_options()).unwrap()
}

fn id(n: u128) -> ChunkId {
    ChunkId::from_u128(n)
}

/// Reads a whole chunk into an owned vector for comparisons.
fn read(ring: &mut ReadRing, id: &ChunkId) -> Option<Vec<u8>> {
    ring.get_sync(id, None).unwrap().map(|d| d.to_vec())
}

/// A deterministic value for `(key, version)` whose every byte can be checked.
fn value_for(key: u128, version: u32, len: usize) -> Vec<u8> {
    // Keys that differ in any bit must yield different streams (an earlier
    // `seed | 1` made neighbouring keys collide).
    let seed = (key as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ ((version as u64) << 40) ^ 0x5bd1_e995;
    let mut x = seed ^ (seed >> 29);
    if x == 0 {
        x = 1;
    }
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 24) as u8
        })
        .collect()
}

struct XorShift(u64);

impl XorShift {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// Value length distribution that covers inline-sized, packed and large records.
fn random_len(rng: &mut XorShift) -> usize {
    match rng.below(10) {
        0 => 0,
        1..=4 => rng.below(4096) as usize,
        5..=7 => rng.below(64 << 10) as usize,
        _ => (64 << 10) + rng.below((CHUNK_MAX as u64) - (64 << 10) + 1) as usize,
    }
}

#[test]
fn put_get_roundtrip_across_sizes() {
    let device = new_device(16);
    let Opened { mut writer, reader, .. } = open(&device);
    let mut ring = open_ring(&reader);

    let sizes = [
        0usize,
        1,
        100,
        4095,
        4096,
        4097,
        65535,
        65536,
        65537,
        100_000,
        CHUNK_MAX as usize,
    ];
    for (i, &len) in sizes.iter().enumerate() {
        let value = value_for(i as u128, 0, len);
        assert!(matches!(
            writer.put(id(i as u128), &value, PutOptions::default()).unwrap(),
            PutOutcome::Written { .. }
        ));
    }
    writer.flush().unwrap();

    for (i, &len) in sizes.iter().enumerate() {
        let got = read(&mut ring, &id(i as u128)).expect("present");
        assert_eq!(got, value_for(i as u128, 0, len), "size {len}");
        assert_eq!(reader.stat(&id(i as u128)).unwrap().len as usize, len);
    }
    assert!(read(&mut ring, &id(999)).is_none());
    assert!(reader.stat(&id(999)).is_none());

    let too_big = vec![0u8; CHUNK_MAX as usize + 1];
    assert!(matches!(
        writer.put(id(1000), &too_big, PutOptions::default()),
        Err(Error::ValueTooLarge { .. })
    ));
}

#[test]
fn zero_copy_large_put() {
    let device = new_device(4);
    let Opened { mut writer, reader, .. } = open(&device);
    let mut ring = open_ring(&reader);
    let value = value_for(3, 0, 100_000);

    let mut large = writer.prepare_large(value.len() as u32).unwrap();
    large.value_mut().copy_from_slice(&value);
    let sums = moat_common::block_checksums(&value);
    let outcome = writer
        .put_large(id(3), large, Some(&sums), PutOptions::default())
        .unwrap();
    assert!(matches!(outcome, PutOutcome::Written { .. }));
    writer.flush().unwrap();
    assert_eq!(read(&mut ring, &id(3)).unwrap(), value);

    // Wrong checksum count is rejected before anything is written.
    let large = writer.prepare_large(value.len() as u32).unwrap();
    assert!(matches!(
        writer.put_large(id(4), large, Some(&sums[..1]), PutOptions::default()),
        Err(Error::InvalidOption(_))
    ));
    // Values below the pack threshold are not large.
    assert!(matches!(writer.prepare_large(100), Err(Error::InvalidOption(_))));

    // The returned data is a view into a pool buffer, usable without copying.
    let data = ring.get_sync(&id(3), Some(10..20)).unwrap().unwrap();
    assert_eq!(&*data, &value[10..20]);
    let (buf, range) = data.into_raw();
    assert_eq!(&buf[range], &value[10..20]);
}

#[test]
fn async_tickets_and_completions() {
    let device = new_device(8);
    let Opened { mut writer, reader, .. } = open(&device);
    let mut ring = open_ring(&reader);
    let mut tickets = HashMap::new();
    for i in 0..40u128 {
        let v = value_for(i, 0, if i % 4 == 0 { 70_000 } else { 3_000 });
        if let PutOutcome::Written { ticket, lsn } = writer.put(id(i), &v, PutOptions::default()).unwrap() {
            tickets.insert(ticket, (i, lsn));
        }
    }
    // Small records are still pending: not visible yet.
    assert!(reader.stat(&id(1)).is_none());
    let mut done = Vec::new();
    writer.submit().unwrap();
    // Nothing forces the pending batch out but a flush or filling it up; use
    // seal_active's flush and then drain completions through poll.
    writer.flush().unwrap();
    // flush discarded completions; write another set and observe them via poll.
    let mut tickets2 = HashMap::new();
    for i in 100..110u128 {
        let v = value_for(i, 0, 70_000);
        if let PutOutcome::Written { ticket, lsn } = writer.put(id(i), &v, PutOptions::default()).unwrap() {
            tickets2.insert(ticket, (i, lsn));
        }
    }
    while !tickets2.is_empty() {
        writer.poll(&mut done, true).unwrap();
        for c in done.drain(..) {
            let (i, lsn) = tickets2.remove(&c.ticket).expect("known ticket");
            assert_eq!(c.lsn, lsn);
            c.result.unwrap();
            assert_eq!(reader.stat(&id(i)).unwrap().lsn, lsn);
        }
    }
    for (_, (i, _)) in tickets {
        assert!(read(&mut ring, &id(i)).is_some());
    }
}

#[test]
fn out_of_order_completions_are_applied_in_order() {
    let device = new_device(12);
    device.set_reverse_completions(true);
    let Opened { mut writer, reader, .. } = open(&device);
    let mut ring = open_ring(&reader);
    let overwrite = PutOptions {
        overwrite: true,
        ..Default::default()
    };
    let mut expected = HashMap::new();
    let mut rng = XorShift(0x5151);
    // Many overwrites of a small key set with mixed sizes: large records are
    // submitted immediately while small ones ride in packed batches, so the
    // same key's versions complete in scrambled order.
    for round in 0..30u32 {
        for k in 0..12u128 {
            let v = value_for(k, round, random_len(&mut rng));
            writer.put(id(k), &v, overwrite).unwrap();
            expected.insert(k, v);
        }
    }
    writer.flush().unwrap();
    for (k, v) in &expected {
        assert_eq!(read(&mut ring, &id(*k)).unwrap(), *v, "key {k}");
    }
    drop(writer);
    let Opened { reader, .. } = open(&device);
    let mut ring = open_ring(&reader);
    for (k, v) in &expected {
        assert_eq!(read(&mut ring, &id(*k)).unwrap(), *v, "key {k} after reopen");
    }
}

#[test]
fn write_failure_truncates_segment_and_continues() {
    let device = new_device(8);
    let Opened { mut writer, reader, .. } = open(&device);
    let mut ring = open_ring(&reader);
    let mut ok_keys = Vec::new();
    for i in 0..3u128 {
        writer
            .put(id(i), &value_for(i, 0, 70_000), PutOptions::default())
            .unwrap();
        ok_keys.push(i);
    }
    writer.flush().unwrap();

    // The hot segment is segment 0 at device offset SEGMENT; fail every write
    // into its second half.
    device.fail_writes_in(Some(SEGMENT + SEGMENT / 2..2 * SEGMENT));
    let mut outcomes = HashMap::new();
    for i in 10..30u128 {
        if let PutOutcome::Written { ticket, .. } = writer
            .put(id(i), &value_for(i, 0, 70_000), PutOptions::default())
            .unwrap()
        {
            outcomes.insert(ticket, i);
        }
    }
    let mut done = Vec::new();
    let (mut succeeded, mut failed) = (Vec::new(), Vec::new());
    while !outcomes.is_empty() {
        writer.poll(&mut done, true).unwrap();
        for c in done.drain(..) {
            let i = outcomes.remove(&c.ticket).unwrap();
            match c.result {
                Ok(()) => succeeded.push(i),
                Err(Error::Io(_)) => failed.push(i),
                Err(e) => panic!("unexpected {e}"),
            }
        }
    }
    assert!(!failed.is_empty(), "the fault must have hit");
    device.fail_writes_in(None);

    // Writing continues on a fresh segment.
    for i in 100..105u128 {
        writer
            .put(id(i), &value_for(i, 0, 70_000), PutOptions::default())
            .unwrap();
    }
    writer.flush().unwrap();

    let check = |ring: &mut ReadRing, reader: &Reader| {
        for i in ok_keys.iter().chain(succeeded.iter()).copied().chain(100..105u128) {
            assert_eq!(read(ring, &id(i)).unwrap(), value_for(i, 0, 70_000), "key {i}");
        }
        for i in &failed {
            assert!(!reader.contains(&id(*i)), "failed key {i} must not be indexed");
        }
    };
    check(&mut ring, &reader);
    drop(writer);
    let Opened { reader, report, .. } = open(&device);
    let mut ring = open_ring(&reader);
    check(&mut ring, &reader);
    assert_eq!(report.chunks, ok_keys.len() + succeeded.len() + 5);
}

#[test]
fn lsn_is_monotonic_and_reported() {
    let device = new_device(4);
    let Opened { mut writer, .. } = open(&device);
    let mut last = 0;
    for i in 0..50u128 {
        let PutOutcome::Written { lsn, .. } = writer.put(id(i), b"x", PutOptions::default()).unwrap() else {
            panic!("expected write");
        };
        assert!(lsn > last);
        last = lsn;
    }
    assert_eq!(writer.next_lsn(), last + 1);
}

#[test]
fn range_reads_verify_only_touched_blocks() {
    let device = new_device(8);
    let Opened { mut writer, reader, .. } = open(&device);
    let mut ring = open_ring(&reader);
    let value = value_for(7, 0, 100_000);
    writer.put(id(7), &value, PutOptions::default()).unwrap();
    writer.flush().unwrap();

    let mut get = |r: std::ops::Range<u64>| ring.get_sync(&id(7), Some(r)).unwrap().unwrap().to_vec();
    assert_eq!(get(0..10), value[0..10]);
    assert_eq!(get(65530..65540), value[65530..65540]);
    assert_eq!(get(99_990..200_000), value[99_990..]);
    assert_eq!(get(200_000..300_000), Vec::<u8>::new());
    assert_eq!(get(0..100_000), value);

    let value_small = value_for(8, 0, 1000);
    writer.put(id(8), &value_small, PutOptions::default()).unwrap();
    writer.flush().unwrap();
    assert_eq!(
        &*ring.get_sync(&id(8), Some(10..20)).unwrap().unwrap(),
        &value_small[10..20]
    );
}

#[test]
fn overwrite_semantics() {
    let device = new_device(4);
    let Opened { mut writer, reader, .. } = open(&device);
    let mut ring = open_ring(&reader);
    let v0 = value_for(1, 0, 3000);
    let v1 = value_for(1, 1, 90_000);
    let v2 = value_for(1, 2, 10);

    let PutOutcome::Written { lsn: lsn0, .. } = writer.put(id(1), &v0, PutOptions::default()).unwrap() else {
        panic!()
    };
    // A duplicate is rejected even while the first put is still pending.
    assert_eq!(
        writer.put(id(1), &v1, PutOptions::default()).unwrap(),
        PutOutcome::Exists
    );
    writer.flush().unwrap();
    assert_eq!(
        writer.put(id(1), &v1, PutOptions::default()).unwrap(),
        PutOutcome::Exists
    );
    assert_eq!(read(&mut ring, &id(1)).unwrap(), v0);

    let overwrite = PutOptions {
        overwrite: true,
        ..Default::default()
    };
    let PutOutcome::Written { lsn: lsn1, .. } = writer.put(id(1), &v1, overwrite).unwrap() else {
        panic!()
    };
    assert!(lsn1 > lsn0);
    // A large record is submitted immediately but only visible once applied.
    writer.flush().unwrap();
    assert_eq!(read(&mut ring, &id(1)).unwrap(), v1);
    assert_eq!(reader.stat(&id(1)).unwrap().lsn, lsn1);

    // A small overwrite following a large one: the pending small record has a
    // higher LSN and must win once flushed.
    writer.put(id(1), &v2, overwrite).unwrap();
    writer.flush().unwrap();
    assert_eq!(read(&mut ring, &id(1)).unwrap(), v2);

    // The reverse: a small pending record followed by a large one. The large
    // record reaches the device first but has the higher LSN and must win.
    let v3 = value_for(1, 3, 20);
    let v4 = value_for(1, 4, 70_000);
    writer.put(id(1), &v3, overwrite).unwrap();
    writer.put(id(1), &v4, overwrite).unwrap();
    writer.flush().unwrap();
    assert_eq!(read(&mut ring, &id(1)).unwrap(), v4);
}

#[test]
fn delete_is_durable_across_reopen() {
    let device = new_device(4);
    {
        let Opened { mut writer, reader, .. } = open(&device);
        let mut ring = open_ring(&reader);
        writer.put(id(1), b"a", PutOptions::default()).unwrap();
        writer.put(id(2), b"b", PutOptions::default()).unwrap();
        writer.flush().unwrap();
        assert!(writer.delete(&id(1)).unwrap());
        assert!(!writer.delete(&id(1)).unwrap());
        assert!(!writer.delete(&id(3)).unwrap());
        assert!(read(&mut ring, &id(1)).is_none());
        assert_eq!(read(&mut ring, &id(2)).unwrap(), b"b");
        // Deliberately no close: recovery must find the tombstone by scanning.
    }
    let Opened { reader, report, .. } = open(&device);
    let mut ring = open_ring(&reader);
    assert_eq!(report.chunks, 1);
    assert!(read(&mut ring, &id(1)).is_none());
    assert_eq!(read(&mut ring, &id(2)).unwrap(), b"b");
}

#[test]
fn delete_after_pending_put_of_same_key() {
    let device = new_device(4);
    let Opened { mut writer, reader, .. } = open(&device);
    let mut ring = open_ring(&reader);
    writer.put(id(1), b"pending", PutOptions::default()).unwrap();
    assert!(writer.delete(&id(1)).unwrap());
    assert!(read(&mut ring, &id(1)).is_none());
    drop(writer);
    let Opened { reader, .. } = open(&device);
    let mut ring = open_ring(&reader);
    assert!(read(&mut ring, &id(1)).is_none());
}

#[test]
fn recovery_from_footers_and_scan() {
    let device = new_device(16);
    let mut rng = XorShift(0x1234);
    let mut expected = HashMap::new();
    {
        let Opened { mut writer, .. } = open(&device);
        for i in 0..400u128 {
            let len = random_len(&mut rng);
            let v = value_for(i, 0, len);
            writer.put(id(i), &v, PutOptions::default()).unwrap();
            expected.insert(i, v);
        }
        writer.flush().unwrap();
        // Drop without close: the hot segment stays active.
    }
    let Opened { reader, report, writer } = open(&device);
    let mut ring = open_ring(&reader);
    assert!(report.scanned >= 1, "an active segment must have been scanned");
    assert!(report.sealed >= 1, "earlier segments must have been sealed");
    assert_eq!(report.chunks, 400);
    for (i, v) in &expected {
        assert_eq!(read(&mut ring, &id(*i)).unwrap(), *v);
    }
    writer.close().unwrap();

    let Opened { reader, report, .. } = open(&device);
    let mut ring = open_ring(&reader);
    assert_eq!(report.scanned, 0, "close seals everything");
    assert_eq!(report.chunks, 400);
    for (i, v) in &expected {
        assert_eq!(read(&mut ring, &id(*i)).unwrap(), *v);
    }
}

/// Simulates a crash in the middle of the last writes: bytes written after a
/// known-good point are damaged (flipped or zeroed). Acknowledged data before
/// the point must survive; damaged records must read as missing or corrupt,
/// never as wrong bytes.
#[test]
fn torn_tail_never_returns_wrong_data() {
    for mode in ["flip", "zero"] {
        let device = new_device(8);
        let mut rng = XorShift(0x77);
        let mut safe = HashMap::new();
        let mut unsafe_keys = HashMap::new();
        {
            let Opened { mut writer, .. } = open(&device);
            for i in 0..60u128 {
                let v = value_for(i, 0, random_len(&mut rng));
                writer.put(id(i), &v, PutOptions::default()).unwrap();
                safe.insert(i, v);
            }
            writer.flush().unwrap();
            let before = device.with_data(|d| d.to_vec());
            for i in 100..130u128 {
                let v = value_for(i, 0, random_len(&mut rng));
                writer.put(id(i), &v, PutOptions::default()).unwrap();
                unsafe_keys.insert(i, v);
            }
            writer.flush().unwrap();
            // Damage every page that changed after the safe point.
            device.with_data_mut(|after| {
                let mut damaged = 0;
                for (page, (a, b)) in after.chunks_mut(4096).zip(before.chunks(4096)).enumerate() {
                    if a != b {
                        // Segment header pages are written atomically by real
                        // devices (single logical block); do not damage them.
                        if (page as u64 * 4096).is_multiple_of(SEGMENT) {
                            continue;
                        }
                        // Leave every other changed page intact so some later
                        // records survive and some do not.
                        if page % 2 == 0 {
                            continue;
                        }
                        damaged += 1;
                        match mode {
                            "flip" => a[100] ^= 0xff,
                            _ => a.fill(0),
                        }
                    }
                }
                assert!(damaged > 0);
            });
        }
        let Opened { reader, .. } = open(&device);
        let mut ring = open_ring(&reader);
        for (i, v) in &safe {
            assert_eq!(read(&mut ring, &id(*i)).unwrap(), *v, "mode {mode} key {i}");
        }
        let mut survived = 0;
        for (i, v) in &unsafe_keys {
            match ring.get_sync(&id(*i), None) {
                Ok(Some(got)) => {
                    assert_eq!(&*got, &v[..], "mode {mode} key {i}");
                    survived += 1;
                }
                Ok(None) | Err(Error::Corrupt(_)) => {}
                Err(e) => panic!("unexpected error {e}"),
            }
        }
        assert!(survived < unsafe_keys.len(), "damage must have removed something");
    }
}

#[test]
fn bit_rot_in_sealed_value_is_detected() {
    let device = new_device(4);
    let Opened { mut writer, reader, .. } = open(&device);
    let mut ring = open_ring(&reader);
    let v = value_for(5, 0, 90_000);
    writer.put(id(5), &v, PutOptions::default()).unwrap();
    writer.close().unwrap();
    let before = device.with_data(|d| d.to_vec());
    // Find the value on disk and flip one byte deep inside it.
    let pos = before
        .windows(64)
        .position(|w| w == &v[1000..1064])
        .expect("value on disk");
    device.with_data_mut(|d| d[pos + 70_000] ^= 1);

    // Block 0 (first 64 KiB) is intact; a range inside it still reads.
    assert_eq!(&*ring.get_sync(&id(5), Some(0..100)).unwrap().unwrap(), &v[0..100]);
    // The damaged block is refused and the entry is dropped from the index.
    assert!(matches!(ring.get_sync(&id(5), None), Err(Error::Corrupt(_))));
    assert!(ring.get_sync(&id(5), None).unwrap().is_none());
}

#[test]
fn corrupt_footer_falls_back_to_scan() {
    let device = new_device(6);
    let mut rng = XorShift(0x42);
    let mut expected = HashMap::new();
    {
        let Opened { mut writer, .. } = open(&device);
        for i in 0..40u128 {
            let v = value_for(i, 0, random_len(&mut rng));
            writer.put(id(i), &v, PutOptions::default()).unwrap();
            expected.insert(i, v);
        }
        writer.close().unwrap();
    }
    // Damage the first footer: it sits right after the data of segment 0, so
    // locate it by its magic.
    let magic = b"MOATFOT1";
    device.with_data_mut(|d| {
        let pos = d.windows(8).position(|w| w == magic).expect("footer present");
        d[pos + 100] ^= 0xff;
    });
    let Opened { reader, report, .. } = open(&device);
    let mut ring = open_ring(&reader);
    assert_eq!(report.bad_footers, 1);
    assert!(report.scanned >= 1);
    for (i, v) in &expected {
        assert_eq!(read(&mut ring, &id(*i)).unwrap(), *v);
    }
    // The rescanned segment was re-sealed with a fresh footer.
    let Opened { report, .. } = open(&device);
    assert_eq!(report.bad_footers, 0);
    assert_eq!(report.scanned, 0);
}

#[test]
fn reclaim_storage_keeps_every_live_chunk() {
    let device = new_device(20);
    let Opened { mut writer, reader, .. } = open(&device);
    let mut ring = open_ring(&reader);
    let mut rng = XorShift(0xbeef);
    let mut model: HashMap<u128, Vec<u8>> = HashMap::new();
    let overwrite = PutOptions {
        overwrite: true,
        ..Default::default()
    };

    for round in 0..6u32 {
        for i in 0..150u128 {
            match rng.below(4) {
                0 if model.contains_key(&i) => {
                    writer.delete(&id(i)).unwrap();
                    model.remove(&i);
                }
                _ => {
                    let v = value_for(i, round, random_len(&mut rng));
                    writer.put(id(i), &v, overwrite).unwrap();
                    model.insert(i, v);
                }
            }
        }
        writer.flush().unwrap();
        while writer.free_segments() < 6 {
            let report = writer.reclaim(ReclaimPolicy::Storage).unwrap().expect("victim");
            assert_eq!(
                report.records,
                report.relocated + report.dropped + report.tombstones_relocated + report.tombstones_dropped
            );
        }
    }
    // A few extra passes exercise the tombstone rules on already-compacted
    // segments (relocated records and forwarded tombstones).
    for _ in 0..8 {
        if writer.free_segments() < 3 || writer.reclaim(ReclaimPolicy::Storage).unwrap().is_none() {
            break;
        }
    }

    let check = |ring: &mut ReadRing| {
        for i in 0..150u128 {
            assert_eq!(read(ring, &id(i)), model.get(&i).cloned(), "key {i}");
        }
    };
    check(&mut ring);
    let usage = reader.usage();
    assert_eq!(usage.chunks, model.len());

    drop(writer);
    let Opened { reader, .. } = open(&device);
    let mut ring = open_ring(&reader);
    check(&mut ring);
}

#[test]
fn reclaim_cache_evicts_oldest_and_reinserts_accessed() {
    let device = new_device(12);
    let Opened { mut writer, reader, .. } = open(&device);
    let mut ring = open_ring(&reader);
    // Fill several segments with 48 KiB values (packed, ~20 per segment).
    for i in 0..100u128 {
        writer
            .put(id(i), &value_for(i, 0, 48 << 10), PutOptions::default())
            .unwrap();
    }
    writer.seal_active().unwrap();
    let free_before = writer.free_segments();

    // Touch the even keys among the oldest records.
    for i in (0..20u128).step_by(2) {
        read(&mut ring, &id(i)).unwrap();
    }

    let report = writer
        .reclaim(ReclaimPolicy::Cache {
            reinsert_accessed: true,
        })
        .unwrap()
        .unwrap();
    assert!(report.relocated > 0 && report.dropped > 0);
    assert_eq!(
        writer.free_segments(),
        free_before + 1 - u32::from(report.relocated > 0)
    );
    for i in 0..report.records as u128 {
        let present = reader.contains(&id(i));
        assert_eq!(present, i % 2 == 0, "key {i}");
    }

    // Pure FIFO: the next oldest segment is dropped wholesale.
    let victim = writer
        .pick_victim(ReclaimPolicy::Cache {
            reinsert_accessed: false,
        })
        .unwrap();
    let report = writer
        .reclaim(ReclaimPolicy::Cache {
            reinsert_accessed: false,
        })
        .unwrap()
        .unwrap();
    assert_eq!(report.seg_no, victim);
    assert_eq!(report.relocated, 0);
    assert!(report.dropped > 0);
}

#[test]
fn expiry_hides_and_reclaim_drops() {
    let clock = Arc::new(ManualClock::new(1_000));
    let device = new_device(6);
    let Opened { mut writer, reader, .. } = open_with(
        &device,
        Options {
            clock: clock.clone(),
            ..options()
        },
    );
    let mut ring = open_ring(&reader);
    writer
        .put(
            id(1),
            b"ephemeral",
            PutOptions {
                expire_at: 1_100,
                ..Default::default()
            },
        )
        .unwrap();
    writer.put(id(2), b"forever", PutOptions::default()).unwrap();
    writer.flush().unwrap();
    assert!(read(&mut ring, &id(1)).is_some());
    clock.set(1_100);
    assert!(read(&mut ring, &id(1)).is_none());
    assert!(read(&mut ring, &id(2)).is_some());

    writer.seal_active().unwrap();
    let report = writer.reclaim(ReclaimPolicy::Storage).unwrap().unwrap();
    assert_eq!(report.dropped, 1);
    assert_eq!(report.relocated, 1);
    assert!(!reader.contains(&id(1)));
    assert!(read(&mut ring, &id(2)).is_some());
}

#[test]
fn no_space_is_reported_not_panicked() {
    let device = new_device(2);
    let Opened { mut writer, .. } = open(&device);
    let big = vec![1u8; CHUNK_MAX as usize];
    let mut written = 0;
    loop {
        match writer.put(id(written), &big, PutOptions::default()) {
            Ok(PutOutcome::Written { .. }) => written += 1,
            Err(Error::NoSpace) => break,
            other => panic!("unexpected {other:?}"),
        }
    }
    assert!(written >= 14, "two segments of 1 MiB hold at least 14 x 128 KiB");
}

#[test]
fn open_rejects_unformatted_and_foreign_devices() {
    let blank = Arc::new(MemDevice::new(SEGMENT * 4));
    assert!(matches!(
        moat_engine::open(blank, options()),
        Err(Error::Unformatted(_))
    ));

    // Segments formatted under a different uuid are treated as free, not as data.
    let device = new_device(4);
    {
        let Opened { mut writer, .. } = open(&device);
        writer.put(id(1), b"old", PutOptions::default()).unwrap();
        writer.close().unwrap();
    }
    let mut opts = format_options();
    opts.disk_uuid = [0xcd; 16];
    // Rewrite only the superblocks with the new uuid: every segment header now
    // carries a foreign uuid.
    let mut sb = moat_common::AlignedBuf::zeroed(4096);
    moat_engine::layout::Superblock {
        generation: 5,
        disk_uuid: opts.disk_uuid,
        segment_size: SEGMENT,
        chunk_max: CHUNK_MAX,
        segment_count: 4,
        created_at: 0,
    }
    .encode(&mut sb);
    use moat_engine::Device;
    device.write_at(&sb, 0).unwrap();
    device.write_at(&sb, 4096).unwrap();
    let Opened { reader, report, .. } = open(&device);
    assert_eq!(report.unreadable_headers, 4);
    assert_eq!(report.chunks, 0);
    assert!(!reader.contains(&id(1)));
}

/// Random operations against a reference model, with periodic flushes,
/// reclaim passes and reopen cycles. Every observable read must agree with the
/// model.
#[test]
fn randomized_against_model() {
    let device = new_device(20);
    let mut rng = XorShift(0x9e37_79b9);
    let mut model: HashMap<u128, (u32, Vec<u8>)> = HashMap::new();
    let mut versions: HashMap<u128, u32> = HashMap::new();
    let mut opened = open(&device);
    let mut ring = open_ring(&opened.reader);
    let keys = 300u64;

    for step in 0..6_000u32 {
        let k = rng.below(keys) as u128;
        match rng.below(100) {
            0..=54 => {
                let version = versions.entry(k).or_insert(0);
                *version += 1;
                let v = value_for(k, *version, random_len(&mut rng));
                let outcome = opened
                    .writer
                    .put(
                        id(k),
                        &v,
                        PutOptions {
                            overwrite: true,
                            ..Default::default()
                        },
                    )
                    .unwrap();
                assert!(matches!(outcome, PutOutcome::Written { .. }));
                model.insert(k, (*version, v));
            }
            55..=69 => {
                let existed = opened.writer.delete(&id(k)).unwrap();
                assert_eq!(existed, model.remove(&k).is_some(), "delete {k} at step {step}");
            }
            70..=89 => {
                // Reads see everything flushed; flush first so the model applies.
                opened.writer.flush().unwrap();
                let got = read(&mut ring, &id(k));
                assert_eq!(got, model.get(&k).map(|(_, v)| v.clone()), "get {k} at step {step}");
            }
            90..=95 => {
                opened.writer.flush().unwrap();
                if opened.writer.free_segments() < 6 {
                    opened.writer.reclaim(ReclaimPolicy::Storage).unwrap();
                }
            }
            _ => {
                // Crash-free reopen: drop the writer without closing half the time.
                drop(ring);
                if rng.below(2) == 0 {
                    let Opened { writer, .. } = opened;
                    writer.close().unwrap();
                } else {
                    opened.writer.flush().unwrap();
                    drop(opened);
                }
                opened = open(&device);
                ring = open_ring(&opened.reader);
            }
        }
        if opened.writer.free_segments() < 3 {
            opened.writer.flush().unwrap();
            for _ in 0..32 {
                if opened.writer.free_segments() >= 6
                    || opened.writer.reclaim(ReclaimPolicy::Storage).unwrap().is_none()
                {
                    break;
                }
            }
        }
    }

    opened.writer.flush().unwrap();
    for k in 0..keys as u128 {
        let got = read(&mut ring, &id(k));
        assert_eq!(got, model.get(&k).map(|(_, v)| v.clone()), "final {k}");
    }
    assert_eq!(opened.reader.usage().chunks, model.len());
}

#[test]
fn concurrent_readers_never_see_torn_values() {
    use std::{
        sync::atomic::{AtomicBool, Ordering},
        thread,
    };

    let device = new_device(16);
    let Opened { mut writer, reader, .. } = open(&device);
    let keys = 64u128;
    // Values are 40 KiB so every segment holds ~25 records and reclaim churns.
    let len = 40 << 10;
    for k in 0..keys {
        writer.put(id(k), &value_for(k, 0, len), PutOptions::default()).unwrap();
    }
    writer.flush().unwrap();

    let stop = Arc::new(AtomicBool::new(false));
    let readers: Vec<_> = (0..4)
        .map(|t| {
            let reader = reader.clone();
            let stop = stop.clone();
            thread::spawn(move || {
                let mut ring = open_ring(&reader);
                let mut rng = XorShift(0x100 + t);
                let mut reads = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    let k = rng.below(keys as u64) as u128;
                    match ring.get_sync(&id(k), None).unwrap() {
                        Some(v) => {
                            // Any version is acceptable, but it must be a whole one.
                            let ok = (0..64u32).any(|ver| *v == value_for(k, ver, len)[..]);
                            assert!(ok, "torn or foreign value for key {k}");
                            reads += 1;
                        }
                        None => panic!("key {k} vanished"),
                    }
                }
                reads
            })
        })
        .collect();

    let overwrite = PutOptions {
        overwrite: true,
        ..Default::default()
    };
    for round in 1..40u32 {
        for k in 0..keys {
            writer.put(id(k), &value_for(k, round, len), overwrite).unwrap();
        }
        writer.flush().unwrap();
        for _ in 0..32 {
            if writer.free_segments() >= 6 {
                break;
            }
            writer.reclaim(ReclaimPolicy::Storage).unwrap().unwrap();
        }
    }
    stop.store(true, Ordering::Relaxed);
    let total: u64 = readers.into_iter().map(|h| h.join().unwrap()).sum();
    assert!(total > 0);
}

/// The same engine on a real file, with `O_DIRECT` and io_uring where the
/// platform supports them (tmpfs does not support `O_DIRECT`; the test then
/// falls back to buffered I/O).
#[test]
fn file_device_roundtrip_with_direct_io() {
    use moat_engine::FileDevice;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("disk.img");
    let len = SEGMENT * 5;
    let device = match FileDevice::create(&path, len, true) {
        Ok(d) => d,
        Err(_) => FileDevice::create(&path, len, false).unwrap(),
    };
    let device: Arc<FileDevice> = Arc::new(device);
    moat_engine::format(&*device, &file_format_options()).unwrap();

    let mut rng = XorShift(0xfeed);
    let mut expected = HashMap::new();
    {
        let Opened { mut writer, reader, .. } = moat_engine::open(device.clone(), file_options()).unwrap();
        let mut ring = reader.ring(&file_queue_options()).unwrap();
        for i in 0..64u128 {
            let len = random_len(&mut rng).min(FILE_CHUNK_MAX as usize);
            let v = value_for(i, 0, len);
            writer.put(id(i), &v, PutOptions::default()).unwrap();
            expected.insert(i, v);
        }
        writer.flush().unwrap();
        for (i, v) in &expected {
            assert_eq!(read(&mut ring, &id(*i)).unwrap(), *v);
        }
        writer.close().unwrap();
    }
    let reopened = Arc::new(FileDevice::open(&path, false).unwrap());
    let Opened { reader, report, .. } = moat_engine::open(reopened, file_options()).unwrap();
    let mut ring = reader.ring(&file_queue_options()).unwrap();
    assert_eq!(report.chunks, 64);
    for (i, v) in &expected {
        assert_eq!(read(&mut ring, &id(*i)).unwrap(), *v);
    }
}

/// Values that are (just under) a page multiple are stored framed: header in
/// the batch's header area, value page aligned, verified from the index CRC.
#[test]
fn framed_records_roundtrip_recover_and_reclaim() {
    let device = new_device(16);
    let Opened { mut writer, reader, .. } = open(&device);
    let mut ring = open_ring(&reader);
    let sizes = [4096usize, 4029, 8192, 12288, 16384 - 10, 65536 - 68];
    let mut expected = HashMap::new();
    for i in 0..300u128 {
        let len = sizes[i as usize % sizes.len()];
        let v = value_for(i, 0, len);
        writer.put(id(i), &v, PutOptions::default()).unwrap();
        expected.insert(i, v);
    }
    // An expiring page-sized value must not be framed (its expiry lives in the
    // header); it still reads back correctly.
    let clockless = value_for(1000, 0, 4096);
    writer
        .put(
            id(1000),
            &clockless,
            PutOptions {
                expire_at: u64::MAX,
                ..Default::default()
            },
        )
        .unwrap();
    writer.flush().unwrap();
    for (i, v) in &expected {
        assert_eq!(read(&mut ring, &id(*i)).unwrap(), *v, "key {i}");
    }
    assert_eq!(read(&mut ring, &id(1000)).unwrap(), clockless);
    // Range reads inside a framed value.
    let got = ring.get_sync(&id(0), Some(100..300)).unwrap().unwrap();
    assert_eq!(&*got, &expected[&0][100..300]);

    // Recovery from a mix of footers (sealed) and scan (active).
    drop(writer);
    let Opened { mut writer, reader, .. } = open(&device);
    let mut ring = open_ring(&reader);
    for (i, v) in &expected {
        assert_eq!(read(&mut ring, &id(*i)).unwrap(), *v, "key {i} after reopen");
    }

    // Reclaim relocates framed records as framed records.
    writer.seal_active().unwrap();
    let mut relocated = 0;
    for _ in 0..4 {
        if let Some(report) = writer.reclaim(ReclaimPolicy::Storage).unwrap() {
            relocated += report.relocated;
        }
    }
    assert!(relocated > 0);
    for (i, v) in &expected {
        assert_eq!(read(&mut ring, &id(*i)).unwrap(), *v, "key {i} after reclaim");
    }
    writer.close().unwrap();
    let Opened { reader, report, .. } = open(&device);
    let mut ring = open_ring(&reader);
    assert_eq!(report.chunks, expected.len() + 1);
    for (i, v) in &expected {
        assert_eq!(read(&mut ring, &id(*i)).unwrap(), *v, "key {i} after second reopen");
    }
}

#[test]
fn framed_corruption_is_caught_with_and_without_header() {
    for strict in [false, true] {
        let device = new_device(4);
        let Opened { mut writer, reader, .. } = open_with(
            &device,
            Options {
                verify_header_on_read: strict,
                ..options()
            },
        );
        let mut ring = open_ring(&reader);
        let v = value_for(9, 0, 4096);
        writer.put(id(9), &v, PutOptions::default()).unwrap();
        writer.close().unwrap();
        let pos = device
            .with_data(|d| d.windows(64).position(|w| w == &v[2000..2064]))
            .expect("value on disk");
        // The value starts on a page boundary: framed layout.
        assert_eq!((pos as u64 - 2000) % 4096, 0);
        device.with_data_mut(|d| d[pos + 100] ^= 1);
        assert!(
            matches!(ring.get_sync(&id(9), None), Err(Error::Corrupt(_))),
            "strict={strict}"
        );
        assert!(ring.get_sync(&id(9), None).unwrap().is_none());
    }
}

/// Every small record reads in the minimum number of pages its length
/// allows: verified from the physical placement the index reports.
#[test]
fn small_records_never_straddle_unnecessarily() {
    let device = new_device(24);
    let Opened { mut writer, reader, .. } = open(&device);
    let mut rng = XorShift(0x3333);
    let mut lens = Vec::new();
    for i in 0..500u128 {
        let len = (rng.below(60 << 10) as usize).max(16);
        writer.put(id(i), &value_for(i, 0, len), PutOptions::default()).unwrap();
        lens.push(len);
    }
    writer.flush().unwrap();
    for (i, &len) in lens.iter().enumerate() {
        let stat = reader.stat(&id(i as u128)).unwrap();
        let meta = 68u64; // header + one block checksum
        let value_off = stat.value_offset as u64;
        let spanned_value = (value_off % 4096 + len as u64).div_ceil(4096);
        let min_pages = (len as u64).div_ceil(4096);
        let page_multiple = len % 4096 == 0 || len % 4096 > 4096 - meta as usize;
        assert_eq!(stat.framed, page_multiple, "len {len}");
        if stat.framed {
            assert_eq!(value_off % 4096, 0, "framed value must be page aligned");
            assert_eq!(spanned_value, min_pages);
        } else {
            // Header and value are read together: the whole inline record must
            // span the minimum number of pages its total length allows.
            let hdr_off = value_off - meta;
            let spanned = (hdr_off % 4096 + meta + len as u64).div_ceil(4096);
            assert_eq!(spanned, (meta + len as u64).div_ceil(4096), "len {len} at {hdr_off}");
        }
    }
}
