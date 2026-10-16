//! The bytes on disk.
//!
//! A segment is a 16-byte header followed by records:
//!
//! ```text
//! header: "DNSP" | version u8 | 3 zero bytes | segment sequence u64 (LE)
//! record: payload length u32 (LE) | CRC-32 of the payload u32 (LE) | payload
//! ```
//!
//! A record is valid only if its length is between 1 and the maximum, the
//! payload is all there and the CRC matches. The first invalid record of a
//! segment ends it: whatever follows is a torn write or damage, and is cut off
//! when the spool opens.

use std::io::{self, Read};

pub(crate) const MAGIC: [u8; 4] = *b"DNSP";
pub(crate) const VERSION: u8 = 1;
pub(crate) const HEADER_LEN: u64 = 16;
pub(crate) const RECORD_OVERHEAD: u64 = 8;

pub(crate) fn encode_header(sequence: u64) -> [u8; HEADER_LEN as usize] {
    let mut header = [0u8; HEADER_LEN as usize];
    header[..4].copy_from_slice(&MAGIC);
    header[4] = VERSION;
    header[8..].copy_from_slice(&sequence.to_le_bytes());
    header
}

/// The sequence in a header, when the header is a valid one.
pub(crate) fn decode_header(bytes: &[u8]) -> Option<u64> {
    if bytes.len() < HEADER_LEN as usize || bytes[..4] != MAGIC || bytes[4] != VERSION {
        return None;
    }
    if bytes[5..8] != [0, 0, 0] {
        return None;
    }
    Some(u64::from_le_bytes(bytes[8..16].try_into().ok()?))
}

/// Appends a whole record to `out`.
pub(crate) fn encode_record(out: &mut Vec<u8>, payload: &[u8]) {
    out.extend_from_slice(
        &u32::try_from(payload.len())
            .unwrap_or(u32::MAX)
            .to_le_bytes(),
    );
    out.extend_from_slice(&crc32(payload).to_le_bytes());
    out.extend_from_slice(payload);
}

/// What reading one record found.
pub(crate) enum Read1 {
    /// A valid record: its payload and the bytes it took.
    Record(Vec<u8>, u64),
    /// The end of the data: nothing, or a record cut short or damaged.
    End,
}

/// Reads the next record. A record that does not check out is `End`, never an
/// error: the caller decides whether that is a clean end or a torn tail.
pub(crate) fn read_record(reader: &mut impl Read, max_record: u32) -> io::Result<Read1> {
    let mut head = [0u8; RECORD_OVERHEAD as usize];
    if !read_exact_or_end(reader, &mut head)? {
        return Ok(Read1::End);
    }
    let len = u32::from_le_bytes([head[0], head[1], head[2], head[3]]);
    let crc = u32::from_le_bytes([head[4], head[5], head[6], head[7]]);
    if len == 0 || len > max_record {
        return Ok(Read1::End);
    }
    // The length is trusted only up to the maximum, so this allocation is
    // bounded; the checksum then vouches for the contents.
    let mut payload = vec![0u8; len as usize];
    if !read_exact_or_end(reader, &mut payload)? || crc32(&payload) != crc {
        return Ok(Read1::End);
    }
    Ok(Read1::Record(payload, RECORD_OVERHEAD + u64::from(len)))
}

/// Fills `buf`; false when the data ends first.
fn read_exact_or_end(reader: &mut impl Read, buf: &mut [u8]) -> io::Result<bool> {
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..]) {
            Ok(0) => return Ok(false),
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(true)
}

/// CRC-32 (IEEE 802.3, reflected, as used by zlib), byte-at-a-time with a
/// 256-entry table built at compile time.
pub(crate) fn crc32(data: &[u8]) -> u32 {
    const TABLE: [u32; 256] = {
        let mut table = [0u32; 256];
        let mut i = 0;
        while i < 256 {
            let mut c = i as u32;
            let mut bit = 0;
            while bit < 8 {
                c = if c & 1 == 1 {
                    0xEDB8_8320 ^ (c >> 1)
                } else {
                    c >> 1
                };
                bit += 1;
            }
            table[i] = c;
            i += 1;
        }
        table
    };
    let mut crc = !0u32;
    for &byte in data {
        crc = TABLE[((crc ^ u32::from(byte)) & 0xFF) as usize] ^ (crc >> 8);
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_matches_the_standard_check_value() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn a_header_round_trips_and_rejects_damage() {
        let header = encode_header(42);
        assert_eq!(decode_header(&header), Some(42));
        let mut bad = header;
        bad[0] = b'X';
        assert_eq!(decode_header(&bad), None);
        let mut bad = header;
        bad[4] = 9;
        assert_eq!(decode_header(&bad), None);
        let mut bad = header;
        bad[6] = 1;
        assert_eq!(decode_header(&bad), None);
        assert_eq!(decode_header(&header[..10]), None);
    }

    fn encoded(payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        encode_record(&mut out, payload);
        out
    }

    #[test]
    fn a_record_round_trips() {
        let bytes = encoded(b"hello");
        let Read1::Record(payload, taken) = read_record(&mut &bytes[..], 100).unwrap() else {
            panic!("expected a record");
        };
        assert_eq!(payload, b"hello");
        assert_eq!(taken, bytes.len() as u64);
    }

    #[test]
    fn every_truncation_of_a_record_is_an_end() {
        let bytes = encoded(b"hello world");
        for cut in 0..bytes.len() {
            assert!(
                matches!(read_record(&mut &bytes[..cut], 100).unwrap(), Read1::End),
                "{cut}"
            );
        }
    }

    #[test]
    fn every_flipped_bit_is_an_end() {
        let bytes = encoded(b"hello world");
        for index in 0..bytes.len() {
            for bit in 0..8 {
                let mut damaged = bytes.clone();
                damaged[index] ^= 1 << bit;
                assert!(
                    matches!(read_record(&mut &damaged[..], 100).unwrap(), Read1::End),
                    "byte {index} bit {bit}"
                );
            }
        }
    }

    #[test]
    fn a_zero_length_or_oversized_record_is_an_end() {
        assert!(matches!(
            read_record(&mut &[0u8; 64][..], 100).unwrap(),
            Read1::End
        ));
        let bytes = encoded(&[7u8; 50]);
        assert!(matches!(
            read_record(&mut &bytes[..], 49).unwrap(),
            Read1::End
        ));
        assert!(matches!(
            read_record(&mut &bytes[..], 50).unwrap(),
            Read1::Record(..)
        ));
    }

    #[test]
    fn a_huge_claimed_length_does_not_allocate_it() {
        let mut bytes = vec![0xFF; 8];
        bytes.extend_from_slice(b"xx");
        assert!(matches!(
            read_record(&mut &bytes[..], 1024).unwrap(),
            Read1::End
        ));
    }
}
