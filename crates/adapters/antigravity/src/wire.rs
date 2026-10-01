//! A hand-rolled protobuf *wire-format* reader, and nothing else.
//!
//! Antigravity CLI ships a closed-source Go binary: the schema behind
//! `gen_metadata.data` and `steps.metadata` is never published, so there is no
//! `.proto` to compile and no dependency is justified for reading six varints
//! and three strings. Only the fields named in [`crate::parser`] are ever
//! touched; everything else on the wire is skipped by its wire type, which is
//! what keeps this reader working when Google adds a field.
//!
//! Every entry point is total: a truncated or garbage buffer yields `None`, and
//! the byte offset where the scan gave up, so a caller can report *where* a
//! record stopped making sense instead of just dropping it.

/// One decoded field value. `Group` wire types (3/4) are deprecated and absent
/// from these tables, so a tag carrying one ends the scan rather than risking a
/// desync that would misread every later field.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Value<'a> {
    Varint(u64),
    Bytes(&'a [u8]),
    Fixed64(u64),
    Fixed32(u32),
}

/// Cursor over one message buffer.
#[derive(Debug, Clone)]
pub(crate) struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
    /// First byte of the field that could not be read, once that happens.
    bad: Option<usize>,
}

impl<'a> Reader<'a> {
    pub(crate) fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0, bad: None }
    }

    /// Bytes consumed so far, i.e. the offset a caller reports as the failure
    /// point when [`Reader::next`] returns `None` before the buffer ended.
    pub(crate) fn pos(&self) -> usize {
        self.pos
    }

    /// Whether the scan ended on a field it could not read, and where that field
    /// began. `pos` alone cannot answer this: a truncated tail sits exactly at
    /// the end of the buffer, which looks like a clean stop.
    pub(crate) fn broken_at(&self) -> Option<usize> {
        self.bad
    }

    /// Base-128 varint, capped at the ten bytes a `u64` can need.
    pub(crate) fn varint(&mut self) -> Option<u64> {
        let mut out: u64 = 0;
        let mut shift = 0u32;
        loop {
            let byte = *self.buf.get(self.pos)?;
            self.pos += 1;
            out |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Some(out);
            }
            shift += 7;
            if shift >= 64 {
                return None;
            }
        }
    }

    /// Next `(field_number, value)`, or `None` at end of buffer or on the first
    /// malformed byte.
    pub(crate) fn next(&mut self) -> Option<(u32, Value<'a>)> {
        if self.pos >= self.buf.len() {
            return None;
        }
        let start = self.pos;
        let read = self.field();
        if read.is_none() {
            self.bad = Some(start);
        }
        read
    }

    fn field(&mut self) -> Option<(u32, Value<'a>)> {
        let tag = self.varint()?;
        let field = u32::try_from(tag >> 3).ok()?;
        let value = match (tag & 0x7) as u8 {
            0 => Value::Varint(self.varint()?),
            1 => Value::Fixed64(u64::from_le_bytes(self.take(8)?.try_into().ok()?)),
            2 => {
                let len = usize::try_from(self.varint()?).ok()?;
                let end = self.pos.checked_add(len).filter(|&e| e <= self.buf.len())?;
                let bytes = &self.buf[self.pos..end];
                self.pos = end;
                Value::Bytes(bytes)
            }
            5 => Value::Fixed32(u32::from_le_bytes(self.take(4)?.try_into().ok()?)),
            _ => return None,
        };
        Some((field, value))
    }

    /// Exactly `n` raw bytes, `None` when the buffer is shorter than declared.
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(n).filter(|&e| e <= self.buf.len())?;
        let bytes = &self.buf[self.pos..end];
        self.pos = end;
        Some(bytes)
    }
}

/// First `bytes`-wire value for `field` (sub-messages, strings and raw bytes
/// share this wire type).
pub(crate) fn bytes_field(buf: &[u8], field: u32) -> Option<&[u8]> {
    let mut r = Reader::new(buf);
    while let Some((n, value)) = r.next() {
        if n == field {
            if let Value::Bytes(b) = value {
                return Some(b);
            }
        }
    }
    None
}

/// First varint value for `field`.
pub(crate) fn varint_field(buf: &[u8], field: u32) -> Option<u64> {
    let mut r = Reader::new(buf);
    while let Some((n, value)) = r.next() {
        if n == field {
            if let Value::Varint(v) = value {
                return Some(v);
            }
        }
    }
    None
}

/// First UTF-8 string for `field`. A field that is present but not valid UTF-8
/// is treated as absent rather than lossy-decoded, so a binary payload can never
/// masquerade as a model id.
pub(crate) fn string_field(buf: &[u8], field: u32) -> Option<&str> {
    std::str::from_utf8(bytes_field(buf, field)?).ok()
}

/// Where scanning `buf` gave up — the first byte of the field that could not be
/// read — or `None` when the whole buffer was well formed.
pub(crate) fn broken_at(buf: &[u8]) -> Option<usize> {
    let mut r = Reader::new(buf);
    while r.next().is_some() {}
    r.broken_at()
}

/// Protobuf's canonical encoder, used by the tests and by nothing at runtime:
/// the adapter never writes a wire message.
#[cfg(test)]
pub(crate) fn encode_varint(mut value: u64) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        out.push(if value == 0 { byte } else { byte | 0x80 });
        if value == 0 {
            return out;
        }
    }
}

#[cfg(test)]
pub(crate) fn tag_varint(field: u32, value: u64) -> Vec<u8> {
    let mut out = encode_varint(u64::from(field) << 3);
    out.extend(encode_varint(value));
    out
}

#[cfg(test)]
pub(crate) fn tag_bytes(field: u32, payload: &[u8]) -> Vec<u8> {
    let mut out = encode_varint((u64::from(field) << 3) | 2);
    out.extend(encode_varint(payload.len() as u64));
    out.extend_from_slice(payload);
    out
}

#[cfg(test)]
pub(crate) fn tag_fixed64(field: u32, value: u64) -> Vec<u8> {
    let mut out = encode_varint((u64::from(field) << 3) | 1);
    out.extend(value.to_le_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varints_decode_across_the_continuation_boundary() {
        // One byte, then the multi-byte forms: 300 = 0b100101100 spans two bytes.
        assert_eq!(varint_field(&tag_varint(2, 0), 2), Some(0));
        assert_eq!(varint_field(&tag_varint(2, 127), 2), Some(127));
        assert_eq!(varint_field(&tag_varint(2, 128), 2), Some(128));
        assert_eq!(varint_field(&tag_varint(2, 300), 2), Some(300));
        assert_eq!(varint_field(&tag_varint(2, u64::MAX), 2), Some(u64::MAX));
        assert_eq!(encode_varint(300), vec![0xac, 0x02]);
    }

    #[test]
    fn field_numbers_are_wide_but_wire_types_stay_three_bits() {
        assert_eq!(varint_field(&tag_varint(19, 7), 19), Some(7));
        assert_eq!(varint_field(&tag_varint(1000, 7), 1000), Some(7));
    }

    #[test]
    fn length_delimited_fields_are_skipped_by_length_not_by_content() {
        // A sub-message whose payload is nonsense must still be stepped over so
        // the fields after it stay readable.
        let mut buf = tag_bytes(3, b"junk\xff\x00");
        buf.extend(tag_varint(4, 42));
        assert_eq!(varint_field(&buf, 4), Some(42));
        assert_eq!(bytes_field(&buf, 3), Some(&b"junk\xff\x00"[..]));
    }

    #[test]
    fn nested_field_paths_reach_the_usage_message() {
        // #1 -> #4 -> #2, the exact path a billable row is read through.
        let usage = tag_varint(2, 512);
        let chat = tag_bytes(4, &usage);
        let outer = tag_bytes(1, &chat);
        let chat = bytes_field(&outer, 1).expect("chatModel");
        let usage = bytes_field(chat, 4).expect("usage");
        assert_eq!(varint_field(usage, 2), Some(512));
    }

    #[test]
    fn fixed_width_fields_decode_little_endian() {
        assert_eq!(Reader::new(&tag_fixed64(9, 0x1122)).next(), Some((9, Value::Fixed64(0x1122))));
        let mut r = Reader::new(&[0x4d, 0xef, 0xbe, 0xad, 0xde]);
        assert_eq!(r.next(), Some((9, Value::Fixed32(0xdead_beef))));
        assert_eq!(r.broken_at(), None);
    }

    #[test]
    fn a_truncated_buffer_stops_the_scan_instead_of_reading_past_the_end() {
        let mut buf = tag_varint(1, 7);
        buf.extend([0x12, 0x40]); // says "64 bytes follow"; nothing does
        assert_eq!(varint_field(&buf, 1), Some(7), "the field before the break still reads");
        assert_eq!(bytes_field(&buf, 2), None);
        assert_eq!(broken_at(&buf), Some(2), "the offset of the field with the absent payload");

        // A varint that never terminates, and a tag with no value at all.
        assert_eq!(broken_at(&[0x08, 0x80, 0x80]), Some(0));
        assert_eq!(broken_at(&[0x08]), Some(0));
        assert_eq!(broken_at(&tag_varint(1, 1)), None);
    }

    #[test]
    fn deprecated_group_wire_types_end_the_scan() {
        let buf = [0x0b, 0x01, 0x00, 0x00, 0x00];
        let mut r = Reader::new(&buf);
        assert_eq!(r.next().map(|(f, _)| f), None);
        assert_eq!(r.broken_at(), Some(0), "the unsupported tag is the field that failed");
    }

    #[test]
    fn garbage_bytes_never_panic_and_never_invent_a_field() {
        for n in 0..64u8 {
            let buf: Vec<u8> = (0..n).map(|i| i.wrapping_mul(37).wrapping_add(11)).collect();
            let _ = broken_at(&buf);
            let _ = bytes_field(&buf, 4);
            let _ = varint_field(&buf, 4);
            let _ = string_field(&buf, 4);
        }
        assert_eq!(string_field(&[0xff, 0xff], 1), None);
    }

    #[test]
    fn a_string_field_holding_binary_is_absent_not_garbled() {
        assert_eq!(string_field(&tag_bytes(11, &[0xc3, 0x28]), 11), None);
        assert_eq!(string_field(&tag_bytes(11, b"resp-id"), 11), Some("resp-id"));
    }

    #[test]
    fn the_unset_time_sentinel_survives_the_wire_untouched() {
        // agy 1.1.18 writes `#9.#2` as int64 -1, i.e. u64::MAX on the wire. The
        // reader must hand that value back verbatim for `parser` to reject.
        let buf = tag_varint(2, u64::MAX);
        assert_eq!(varint_field(&buf, 2), Some(u64::MAX));
        assert_eq!(broken_at(&buf), None);
    }
}
