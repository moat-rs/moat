#!/usr/bin/env python3
"""Model of open inline pages and unified value extents, not engine code.

Run directly to exercise byte truncation, torn pages, identity checks, recovery
publication, and batching deadlines. No device, queue, or durability is modeled.
"""

from dataclasses import dataclass
import itertools
import struct
import unittest


PAGE = 4096
HEADER = struct.Struct("<8sIIQQIIII16s")
META = struct.Struct("<8sQ16sIIIB19s")
MAGIC = b"MOATEXTA"
RECORD_MAGIC = b"MOATRECA"
INLINE_HEADER = struct.Struct("<8sI4xQQ32s")
INLINE_RECORD = struct.Struct("<8sIIQ16sB7s")
INLINE_MAGIC = b"MOATINLA"
INLINE_RECORD_MAGIC = b"MOATIRCA"
CRC_OFFSET = 8


def make_crc_table():
    table = []
    for value in range(256):
        for _ in range(8):
            value = (value >> 1) ^ (0x82F63B78 if value & 1 else 0)
        table.append(value)
    return tuple(table)


CRC_TABLE = make_crc_table()


def crc32c(data):
    value = 0xFFFFFFFF
    for byte in data:
        value = CRC_TABLE[(value ^ byte) & 255] ^ (value >> 8)
    return value ^ 0xFFFFFFFF


def align(value, boundary):
    return (value + boundary - 1) // boundary * boundary


def header_crc(header):
    copy = bytearray(header)
    copy[CRC_OFFSET:CRC_OFFSET + 4] = bytes(4)
    return crc32c(copy)


def pages(start, length):
    return (start % PAGE + length + PAGE - 1) // PAGE if length else 0


@dataclass(frozen=True)
class Record:
    key: int
    lsn: int
    value: bytes
    tombstone: bool = False
    page_aligned: bool = False


@dataclass(frozen=True)
class Entry:
    record: Record
    value_offset: int
    alignment: int
    extent_start: int
    extent_len: int


class InvalidExtent(ValueError):
    pass


def empty_inline_page(epoch, start):
    page = bytearray(PAGE)
    INLINE_HEADER.pack_into(page, 0, INLINE_MAGIC, 0, epoch, start, bytes(32))
    struct.pack_into("<I", page, CRC_OFFSET, header_crc(page[:INLINE_HEADER.size]))
    return bytes(page)


def inline_record_crc(raw, epoch, page_start, record_start, value_len):
    copy = bytearray(raw)
    checksum_at = INLINE_RECORD.size + value_len
    copy[checksum_at:checksum_at + 4] = bytes(4)
    return crc32c(struct.pack("<QQI", epoch, page_start, record_start) + copy)


def decode_inline_page(page, epoch, start):
    if len(page) != PAGE:
        raise InvalidExtent("short inline page")
    magic, checksum, stored_epoch, stored_start, reserved = INLINE_HEADER.unpack_from(page)
    if (magic != INLINE_MAGIC or checksum != header_crc(page[:INLINE_HEADER.size])
            or reserved != bytes(32) or any(page[12:16])):
        raise InvalidExtent("invalid inline header")
    if (stored_epoch, stored_start) != (epoch, start) or start % PAGE:
        raise InvalidExtent("wrong inline page identity")
    cursor = INLINE_HEADER.size
    entries = []
    while cursor + INLINE_RECORD.size + 4 <= PAGE:
        magic, length, value_len, lsn, key, kind, reserved = INLINE_RECORD.unpack_from(page, cursor)
        if magic != INLINE_RECORD_MAGIC or kind not in (0, 1) or reserved != bytes(7):
            break
        if length != align(INLINE_RECORD.size + value_len + 4, 8) or cursor + length > PAGE or (kind and value_len):
            break
        stored_crc = struct.unpack_from("<I", page, cursor + INLINE_RECORD.size + value_len)[0]
        if stored_crc != inline_record_crc(page[cursor:cursor + length], epoch, start, cursor, value_len):
            break
        value_start = cursor + INLINE_RECORD.size
        record = Record(
            int.from_bytes(key, "little"), lsn,
            bytes(page[value_start:value_start + value_len]), bool(kind),
        )
        entries.append(Entry(record, value_start, 8, start, PAGE))
        cursor += length
    return entries, cursor


def append_inline(page, record, epoch, start):
    """Append to a working RAM image; prior record bytes stay identical."""
    if record.tombstone and record.value:
        raise ValueError("a tombstone has no value")
    _, cursor = decode_inline_page(page, epoch, start)
    length = align(INLINE_RECORD.size + len(record.value) + 4, 8)
    if cursor + length > PAGE:
        raise ValueError("inline page full")
    raw = bytearray(length)
    INLINE_RECORD.pack_into(
        raw, 0, INLINE_RECORD_MAGIC, length, len(record.value), record.lsn,
        record.key.to_bytes(16, "little"), int(record.tombstone), bytes(7),
    )
    raw[INLINE_RECORD.size:INLINE_RECORD.size + len(record.value)] = record.value
    checksum = inline_record_crc(raw, epoch, start, cursor, len(record.value))
    struct.pack_into("<I", raw, INLINE_RECORD.size + len(record.value), checksum)
    result = bytearray(page)
    result[cursor:cursor + length] = raw
    return bytes(result)


def encode_extent(records, epoch, start):
    if not records or start % PAGE:
        raise ValueError("a nonempty extent must start on a page boundary")
    directory_end = HEADER.size + len(records) * META.size
    cursor = directory_end
    placements = []
    for record in records:
        if record.tombstone and record.value:
            raise ValueError("a tombstone has no value")
        candidate = align(cursor, 8)
        boundary = 8
        if record.page_aligned or len(record.value) >= 65536:
            boundary = PAGE
        elif pages(candidate, len(record.value)) > pages(0, len(record.value)):
            boundary = PAGE
        value_offset = align(candidate, boundary)
        placements.append((record, value_offset, boundary))
        cursor = value_offset + len(record.value)
    total = align(cursor, PAGE)
    encoded = bytearray(total)
    for i, (record, value_offset, boundary) in enumerate(placements):
        META.pack_into(
            encoded, HEADER.size + i * META.size,
            RECORD_MAGIC, record.lsn, record.key.to_bytes(16, "little"),
            len(record.value), value_offset, boundary, int(record.tombstone), bytes(19),
        )
        encoded[value_offset:value_offset + len(record.value)] = record.value
    body_crc = crc32c(memoryview(encoded)[HEADER.size:])
    HEADER.pack_into(
        encoded, 0, MAGIC, 0, total, epoch, start, len(records),
        directory_end, cursor, body_crc, bytes(16),
    )
    struct.pack_into("<I", encoded, CRC_OFFSET, header_crc(encoded[:HEADER.size]))
    return bytes(encoded)


def decode_extent(data, epoch, start):
    if len(data) < HEADER.size:
        raise InvalidExtent("short header")
    (magic, checksum, total, stored_epoch, stored_start, count,
     directory_end, used_end, body_crc, reserved) = HEADER.unpack_from(data)
    if magic != MAGIC or checksum != header_crc(data[:HEADER.size]) or reserved != bytes(16):
        raise InvalidExtent("invalid header")
    if stored_epoch != epoch or stored_start != start or start % PAGE:
        raise InvalidExtent("wrong physical identity")
    if total < PAGE or total % PAGE or total > len(data):
        raise InvalidExtent("invalid extent length")
    if not count or directory_end != HEADER.size + count * META.size or not directory_end <= used_end <= total:
        raise InvalidExtent("invalid directory bounds")
    if body_crc != crc32c(data[HEADER.size:total]):
        raise InvalidExtent("incomplete or corrupt body")
    entries = []
    previous_end = directory_end
    for i in range(count):
        magic, lsn, key, size, offset, boundary, kind, reserved = META.unpack_from(data, HEADER.size + i * META.size)
        if magic != RECORD_MAGIC or reserved != bytes(19) or kind not in (0, 1):
            raise InvalidExtent("invalid record metadata")
        if boundary not in (8, PAGE) or offset % boundary:
            raise InvalidExtent("invalid record alignment")
        if offset < previous_end or offset + size > used_end or (kind == 1 and size):
            raise InvalidExtent("invalid record bounds")
        if any(data[previous_end:offset]):
            raise InvalidExtent("nonzero placement padding")
        record = Record(
            int.from_bytes(key, "little"), lsn,
            bytes(data[offset:offset + size]), bool(kind), boundary == PAGE,
        )
        entries.append(Entry(record, offset, boundary, start, total))
        previous_end = offset + size
    if previous_end != used_end or any(data[used_end:total]):
        raise InvalidExtent("invalid final padding")
    return entries, total


@dataclass(frozen=True)
class Recovery:
    index: dict
    entries: tuple
    tail: int
    extents: int


def recover(image, epoch):
    """Recover the valid prefix; never search payload bytes for magic."""
    cursor = PAGE  # The model assumes an already validated segment header.
    latest = {}
    entries = []
    extents = 0
    view = memoryview(image)
    while cursor + HEADER.size <= len(image):
        try:
            if bytes(view[cursor:cursor + 8]) == INLINE_MAGIC:
                decoded, _ = decode_inline_page(view[cursor:cursor + PAGE], epoch, cursor)
                length = PAGE
            else:
                decoded, length = decode_extent(view[cursor:], epoch, cursor)
        except InvalidExtent:
            break
        # No record escapes before the complete extent has validated.
        for entry in decoded:
            old = latest.get(entry.record.key)
            if old is None or old.record.lsn < entry.record.lsn:
                latest[entry.record.key] = entry
        entries.extend(decoded)
        extents += 1
        cursor += length
    index = {key: entry.record.value for key, entry in latest.items() if not entry.record.tombstone}
    return Recovery(index, tuple(entries), cursor, extents)


class Log:
    def __init__(self, capacity=256 * 1024, epoch=7):
        self.image = bytearray([0xA5]) * capacity
        self.epoch = epoch
        self.tail = PAGE

    def plan(self, records):
        encoded = encode_extent(records, self.epoch, self.tail)
        if self.tail + len(encoded) > len(self.image):
            raise ValueError("segment full")
        return self.tail, encoded

    def append(self, records):
        start, encoded = self.plan(records)
        self.image[start:start + len(encoded)] = encoded
        self.tail += len(encoded)
        return start, encoded


@dataclass
class PendingWindow:
    """Scheduling decision only; real worker wakeups are not modeled."""
    limit: int
    delay: int
    size: int = 0
    first_at: int | None = None

    def add(self, size, now):
        if self.first_at is None:
            self.first_at = now
        self.size += size

    def ready(self, now, force=False):
        return self.first_at is not None and (force or self.size >= self.limit or now >= self.first_at + self.delay)


class ProtocolTests(unittest.TestCase):
    def setUp(self):
        self.log = Log()
        self.log.append([Record(1, 1, b"previously persisted value")])

    def test_crc32c_known_vector(self):
        self.assertEqual(crc32c(b"123456789"), 0xE3069283)
        self.assertEqual((HEADER.size, META.size, INLINE_HEADER.size, INLINE_RECORD.size), (64, 64, 64, 48))

    def test_mixed_values_share_one_extent(self):
        records = [Record(2, 2, b"a" * 100), Record(3, 3, b"b" * 4090), Record(4, 4, b"c" * 65536)]
        start, encoded = self.log.append(records)
        entries, length = decode_extent(encoded, self.log.epoch, start)
        self.assertEqual([entry.value_offset for entry in entries], [256, 4096, 8192])
        self.assertEqual(length, 73728)
        self.assertEqual(recover(self.log.image, self.log.epoch).index, {
            1: b"previously persisted value",
            2: records[0].value, 3: records[1].value, 4: records[2].value,
        })

    def test_near_page_values_use_actual_directory_size(self):
        encoded = encode_extent([Record(1, 1, b"a" * 4090), Record(2, 2, b"b" * 4090)], 7, PAGE)
        entries, length = decode_extent(encoded, 7, PAGE)
        self.assertEqual([entry.value_offset for entry in entries], [4096, 8192])
        self.assertEqual(length, 12288)

    def test_no_extra_trailer_page_for_exact_page_value(self):
        encoded = encode_extent([Record(2, 2, b"x" * 65536)], 7, PAGE)
        self.assertEqual(len(encoded), 69632)

    def test_every_byte_truncation_preserves_previous_extent(self):
        start, encoded = self.log.plan([Record(2, 2, b"new value")])
        image = bytearray(self.log.image)
        for cut in range(len(encoded)):
            if cut:
                image[start + cut - 1] = encoded[cut - 1]
            result = recover(image, self.log.epoch)
            self.assertEqual(result.index, {1: b"previously persisted value"}, f"cut={cut}")
            self.assertEqual(result.tail, start)
        image[start:start + len(encoded)] = encoded
        self.assertIn(2, recover(image, self.log.epoch).index)

    def test_all_sector_subsets_of_a_submission(self):
        start, encoded = self.log.plan([Record(2, 2, b"new value")])
        sectors = len(encoded) // 512
        for mask in range(1 << sectors):
            image = bytearray(self.log.image)
            for sector in range(sectors):
                if mask & (1 << sector):
                    begin = sector * 512
                    image[start + begin:start + begin + 512] = encoded[begin:begin + 512]
            result = recover(image, self.log.epoch)
            self.assertIn(1, result.index)
            self.assertEqual(2 in result.index, mask == (1 << sectors) - 1)

    def test_header_last_and_body_last_both_validate_only_when_complete(self):
        start, encoded = self.log.plan([Record(2, 2, b"x" * 9000)])
        for order in itertools.permutations(range(len(encoded) // PAGE)):
            image = bytearray(self.log.image)
            for n, page in enumerate(order):
                begin = page * PAGE
                image[start + begin:start + begin + PAGE] = encoded[begin:begin + PAGE]
                self.assertEqual(2 in recover(image, self.log.epoch).index, n == len(order) - 1)

    def test_complete_records_in_invalid_extent_are_not_published(self):
        start, encoded = self.log.append([Record(2, 2, b"intact"), Record(3, 3, b"damaged")])
        entries, _ = decode_extent(encoded, self.log.epoch, start)
        self.log.image[start + entries[1].value_offset] ^= 1
        self.assertEqual(recover(self.log.image, self.log.epoch).index, {1: b"previously persisted value"})

    def test_stale_incarnation_and_wrong_position_are_rejected(self):
        for epoch, position in [(6, self.log.tail), (7, self.log.tail + PAGE)]:
            encoded = encode_extent([Record(2, 2, b"stale")], epoch, position)
            self.log.image[self.log.tail:self.log.tail + len(encoded)] = encoded
            self.assertNotIn(2, recover(self.log.image, self.log.epoch).index)

    def test_no_magic_search_past_invalid_extent(self):
        start, _ = self.log.append([Record(2, 2, b"bad")])
        self.log.append([Record(3, 3, b"valid later, not a durable-prefix promise")])
        self.log.image[start] ^= 1
        self.assertEqual(recover(self.log.image, self.log.epoch).index, {1: b"previously persisted value"})

    def test_lsn_order_and_tombstones(self):
        self.log.append([Record(1, 20, b"new")])
        self.log.append([Record(1, 10, b"old but physically later")])
        self.assertEqual(recover(self.log.image, self.log.epoch).index[1], b"new")
        self.log.append([Record(1, 30, b"", tombstone=True)])
        self.assertNotIn(1, recover(self.log.image, self.log.epoch).index)

    def test_old_pages_never_change_on_append(self):
        old = bytes(self.log.image[:self.log.tail])
        self.log.append([Record(2, 2, b"another value")])
        self.assertEqual(bytes(self.log.image[:len(old)]), old)

    def test_invalid_lengths_are_bounded_before_body_access(self):
        start, encoded = self.log.plan([Record(2, 2, b"x")])
        for length in [0, 1, PAGE - 1, len(self.log.image) + PAGE, 0xFFFFF000]:
            changed = bytearray(encoded)
            struct.pack_into("<I", changed, 12, length)
            struct.pack_into("<I", changed, CRC_OFFSET, header_crc(changed[:HEADER.size]))
            with self.assertRaises(InvalidExtent):
                decode_extent(changed, self.log.epoch, start)

    def test_bad_geometry_is_rejected_even_with_recomputed_checksums(self):
        start, encoded = self.log.plan([Record(2, 2, b"x")])
        changed = bytearray(encoded)
        struct.pack_into("<I", changed, HEADER.size + 36, 0)  # Value overlaps header.
        struct.pack_into("<I", changed, 44, crc32c(changed[HEADER.size:]))
        struct.pack_into("<I", changed, CRC_OFFSET, header_crc(changed[:HEADER.size]))
        with self.assertRaises(InvalidExtent):
            decode_extent(changed, self.log.epoch, start)

    def test_polls_do_not_extend_deadline_or_force_early_submission(self):
        window = PendingWindow(limit=65536, delay=50)
        self.assertFalse(window.ready(0, force=True))
        window.add(100, now=0)
        for now in range(50):
            self.assertFalse(window.ready(now))
        window.add(100, now=49)
        self.assertTrue(window.ready(50))
        self.assertTrue(window.ready(0, force=True))
        window.add(65536, now=49)
        self.assertTrue(window.ready(49))


class InlineRewriteTests(unittest.TestCase):
    """The positive rewrite tests assume torn writes contain old/new bytes only.

    The negative test deliberately violates that assumption. Passing these
    tests is not evidence that a Device implementation offers that contract.
    """

    def setUp(self):
        self.epoch = 7
        self.start = PAGE
        self.old = append_inline(empty_inline_page(7, PAGE), Record(1, 1, b"a" * 100), 7, PAGE)
        self.new = append_inline(self.old, Record(2, 2, b"b" * 900), 7, PAGE)

    def keys(self, page):
        entries, _ = decode_inline_page(page, self.epoch, self.start)
        return {entry.record.key: entry.record.value for entry in entries}

    def test_every_byte_prefix_of_rewrite_retains_old_record(self):
        _, old_end = decode_inline_page(self.old, 7, PAGE)
        self.assertEqual(self.old[:old_end], self.new[:old_end])
        for cut in range(PAGE + 1):
            page = self.new[:cut] + self.old[cut:]
            keys = self.keys(page)
            self.assertEqual(keys[1], b"a" * 100, f"cut={cut}")
            if 2 in keys:
                self.assertEqual(keys[2], b"b" * 900)

    def test_every_old_new_sector_combination_retains_old_record(self):
        for mask in range(1 << (PAGE // 512)):
            page = b"".join(
                (self.new if mask & (1 << sector) else self.old)[sector * 512:(sector + 1) * 512]
                for sector in range(PAGE // 512)
            )
            self.assertEqual(self.keys(page)[1], b"a" * 100)

    def test_incomplete_inline_suffix_does_not_hide_later_extent(self):
        image = bytearray([0xA5]) * (64 * 1024)
        image[PAGE:2 * PAGE] = self.new[:512] + self.old[512:]
        extent = encode_extent([Record(3, 3, b"large" * 2000)], 7, 2 * PAGE)
        image[2 * PAGE:2 * PAGE + len(extent)] = extent
        recovered = recover(image, 7)
        self.assertEqual(recovered.index, {1: b"a" * 100, 3: b"large" * 2000})

    def test_damage_to_old_bytes_is_not_repaired_by_record_crc(self):
        damaged = bytearray(self.new)
        entries, _ = decode_inline_page(self.old, 7, PAGE)
        damaged[entries[0].value_offset] ^= 1
        self.assertNotIn(1, self.keys(damaged))

    def test_stale_record_tail_is_bound_to_incarnation(self):
        current = bytearray(empty_inline_page(8, PAGE))
        current[INLINE_HEADER.size:] = self.new[INLINE_HEADER.size:]
        self.assertEqual(decode_inline_page(current, 8, PAGE)[0], [])

    def test_overwrite_and_tombstone_append_without_changing_old_bytes(self):
        revised = append_inline(self.old, Record(1, 10, b"replacement"), 7, PAGE)
        deleted = append_inline(revised, Record(1, 11, b"", tombstone=True), 7, PAGE)
        _, old_end = decode_inline_page(self.old, 7, PAGE)
        self.assertEqual(deleted[:old_end], self.old[:old_end])
        image = bytearray(3 * PAGE)
        image[PAGE:2 * PAGE] = deleted
        self.assertEqual(recover(image, 7).index, {})

    def test_page_full_does_not_modify_original_image(self):
        before = self.old
        with self.assertRaises(ValueError):
            append_inline(self.old, Record(2, 2, b"x" * PAGE), 7, PAGE)
        self.assertEqual(self.old, before)


if __name__ == "__main__":
    unittest.main(verbosity=2)
