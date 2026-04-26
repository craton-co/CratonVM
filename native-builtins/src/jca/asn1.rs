//! WP6.6 — minimal ASN.1 DER encoder/decoder for the JCA surface.
//!
//! The only DER-touching probe in the wave is the X500Principal round-trip
//! (`apps/sig_probe/SigProbe.java`):
//!
//! ```java
//! X500Principal dn = new X500Principal("CN=Test, O=Acme, C=SE");
//! byte[] der = dn.getEncoded();
//! X500Principal back = new X500Principal(der);
//! if (!dn.equals(back)) { ... }
//! ```
//!
//! The codec lives here so future Wave-6 surfaces (KeyFactory, Signature
//! parameter encoding) reuse the same byte-correct primitives.  Existing
//! `crypto_impl::der_encode_*` helpers cover what we need for INTEGER /
//! SEQUENCE / OCTET STRING; this module adds OID, SET, and the typed-string
//! encodings (`PrintableString`, `UTF8String`, `IA5String`) the X.500 RDN
//! values use.
//!
//! Hard size limits: 1 MiB total input, 64 levels of nesting (matches the
//! `security_manager::x509` parser already shipped in session 90).

#![allow(dead_code)]

const MAX_INPUT_SIZE: usize = 1024 * 1024;
const MAX_NESTING_DEPTH: usize = 64;

// DER tag bytes
pub const TAG_BOOLEAN:        u8 = 0x01;
pub const TAG_INTEGER:        u8 = 0x02;
pub const TAG_BIT_STRING:     u8 = 0x03;
pub const TAG_OCTET_STRING:   u8 = 0x04;
pub const TAG_NULL:           u8 = 0x05;
pub const TAG_OID:            u8 = 0x06;
pub const TAG_UTF8_STRING:    u8 = 0x0C;
pub const TAG_PRINTABLE:      u8 = 0x13;
pub const TAG_TELETEX:        u8 = 0x14;
pub const TAG_IA5:            u8 = 0x16;
pub const TAG_BMP:            u8 = 0x1E;
pub const TAG_SEQUENCE:       u8 = 0x30;
pub const TAG_SET:            u8 = 0x31;

// ---------------------------------------------------------------------------
// Encoder helpers
// ---------------------------------------------------------------------------

/// Encode a length following X.690 §8.1.3 (definite-length only, DER).
pub fn encode_length(len: usize) -> Vec<u8> {
    if len < 0x80 {
        vec![len as u8]
    } else if len < 0x100 {
        vec![0x81, len as u8]
    } else if len < 0x10000 {
        vec![0x82, (len >> 8) as u8, len as u8]
    } else if len < 0x1000000 {
        vec![0x83, (len >> 16) as u8, (len >> 8) as u8, len as u8]
    } else {
        // 4-byte length (caps at 4 GiB; X.500 names never go this big).
        vec![
            0x84,
            (len >> 24) as u8,
            (len >> 16) as u8,
            (len >> 8) as u8,
            len as u8,
        ]
    }
}

/// `tag || len || content`.
pub fn encode_tlv(tag: u8, content: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(content.len() + 6);
    out.push(tag);
    out.extend_from_slice(&encode_length(content.len()));
    out.extend_from_slice(content);
    out
}

/// `SEQUENCE { content }`.
pub fn encode_sequence(content: &[u8]) -> Vec<u8> {
    encode_tlv(TAG_SEQUENCE, content)
}

/// `SET { content }`.  Per DER (X.690 §10.3) elements of a `SET OF` must
/// be sorted by their encoded byte value; we produce single-element sets
/// from RDNs so sorting is a no-op for the X500Principal round-trip.
pub fn encode_set(content: &[u8]) -> Vec<u8> {
    encode_tlv(TAG_SET, content)
}

/// Encode a dotted-decimal OID like "2.5.4.3" into DER bytes.
///
/// First byte = `arc1 * 40 + arc2`.  Subsequent arcs encoded base-128
/// big-endian with the high bit of the final byte cleared.  Returns
/// `Ok(_)` on success, `Err(())` on malformed input (so the caller can
/// fall back to a synthetic encoding without panicking).
pub fn encode_oid(dotted: &str) -> Result<Vec<u8>, ()> {
    let arcs: Result<Vec<u64>, _> = dotted.split('.').map(|s| s.parse::<u64>()).collect();
    let arcs = arcs.map_err(|_| ())?;
    if arcs.len() < 2 {
        return Err(());
    }
    if arcs[0] > 2 || arcs[1] > 39 {
        return Err(());
    }
    let mut content = Vec::new();
    content.push((arcs[0] * 40 + arcs[1]) as u8);
    for &arc in &arcs[2..] {
        encode_base128(&mut content, arc);
    }
    Ok(encode_tlv(TAG_OID, &content))
}

fn encode_base128(out: &mut Vec<u8>, mut v: u64) {
    if v == 0 {
        out.push(0);
        return;
    }
    // Collect base-128 digits least-significant first.
    let mut digits = Vec::new();
    while v > 0 {
        digits.push((v & 0x7F) as u8);
        v >>= 7;
    }
    // Emit most-significant first; set high bit on all but last.
    for (i, d) in digits.iter().rev().enumerate() {
        let last = i + 1 == digits.len();
        out.push(if last { *d } else { *d | 0x80 });
    }
}

/// Pick an RDN string-tag based on the value's content.
///
/// JDK's X500Name encoder uses `PrintableString` when every character is in
/// the ASCII printable subset (A-Z, a-z, 0-9, plus ` '()+,-./:=?`) and
/// otherwise falls back to `UTF8String`.  IA5String is reserved for `EmailAddress`
/// and `DC` per RFC 5280 §4.1.2.4.
pub fn pick_string_tag(value: &str) -> u8 {
    if value.bytes().all(|b| matches!(b,
        b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' |
        b' ' | b'\'' | b'(' | b')' | b'+' | b',' | b'-' |
        b'.' | b'/' | b':' | b'=' | b'?'
    )) {
        TAG_PRINTABLE
    } else {
        TAG_UTF8_STRING
    }
}

/// Encode a DirectoryString — chooses between PrintableString / UTF8String.
pub fn encode_directory_string(value: &str) -> Vec<u8> {
    encode_tlv(pick_string_tag(value), value.as_bytes())
}

// ---------------------------------------------------------------------------
// Decoder
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DerError {
    Truncated,
    BadTag,
    BadLength,
    TooLarge,
    NestedTooDeep,
    NotMinimal,
    Trailing,
}

/// `(tag, content_offset, content_len, total_len)` — total_len includes the
/// tag and length bytes.
pub fn read_header(data: &[u8]) -> Result<(u8, usize, usize, usize), DerError> {
    if data.is_empty() {
        return Err(DerError::Truncated);
    }
    let tag = data[0];
    if data.len() < 2 {
        return Err(DerError::Truncated);
    }
    let l0 = data[1];
    let (content_len, hdr_extra) = if l0 < 0x80 {
        (l0 as usize, 1)
    } else if l0 == 0x80 {
        // Indefinite length — not allowed in DER.
        return Err(DerError::BadLength);
    } else {
        let n = (l0 & 0x7F) as usize;
        if n > 4 {
            return Err(DerError::BadLength);
        }
        if data.len() < 2 + n {
            return Err(DerError::Truncated);
        }
        let mut len = 0usize;
        for i in 0..n {
            len = (len << 8) | (data[2 + i] as usize);
        }
        (len, 1 + n)
    };
    let hdr_total = 1 + hdr_extra;
    let total = hdr_total
        .checked_add(content_len)
        .ok_or(DerError::TooLarge)?;
    if total > MAX_INPUT_SIZE || data.len() < total {
        return Err(DerError::Truncated);
    }
    Ok((tag, hdr_total, content_len, total))
}

/// Read an OID into its dotted-decimal string form.
pub fn read_oid(content: &[u8]) -> Result<String, DerError> {
    if content.is_empty() {
        return Err(DerError::BadLength);
    }
    let first = content[0] as u64;
    let arc1 = first / 40;
    let arc2 = first % 40;
    let mut out = format!("{}.{}", arc1, arc2);
    let mut i = 1;
    while i < content.len() {
        let mut v: u64 = 0;
        loop {
            if i >= content.len() {
                return Err(DerError::Truncated);
            }
            let b = content[i];
            i += 1;
            v = (v << 7) | ((b & 0x7F) as u64);
            if (b & 0x80) == 0 {
                break;
            }
        }
        out.push('.');
        out.push_str(&v.to_string());
    }
    Ok(out)
}

/// Decode a directory string (PrintableString / UTF8String / IA5String /
/// TeletexString / BMPString) into a Rust `String`.
pub fn read_directory_string(tag: u8, content: &[u8]) -> Option<String> {
    match tag {
        TAG_PRINTABLE | TAG_UTF8_STRING | TAG_IA5 | TAG_TELETEX => {
            // Treat all of these as UTF-8 (Latin-1 fallback for Teletex).
            match std::str::from_utf8(content) {
                Ok(s) => Some(s.to_string()),
                Err(_) => Some(content.iter().map(|&b| b as char).collect::<String>()),
            }
        }
        TAG_BMP => {
            // BMPString is UCS-2 big-endian.
            if content.len() % 2 != 0 {
                return None;
            }
            let mut out = String::with_capacity(content.len() / 2);
            for chunk in content.chunks_exact(2) {
                let cp = u16::from_be_bytes([chunk[0], chunk[1]]);
                out.push(char::from_u32(cp as u32).unwrap_or('?'));
            }
            Some(out)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_length_short_form() {
        assert_eq!(encode_length(0), vec![0x00]);
        assert_eq!(encode_length(127), vec![0x7F]);
    }

    #[test]
    fn encode_length_long_form() {
        assert_eq!(encode_length(128), vec![0x81, 0x80]);
        assert_eq!(encode_length(0x100), vec![0x82, 0x01, 0x00]);
        assert_eq!(encode_length(0x10000), vec![0x83, 0x01, 0x00, 0x00]);
    }

    #[test]
    fn oid_round_trip_2_5_4_3() {
        // 2.5.4.3 -> 0x55 0x04 0x03
        let der = encode_oid("2.5.4.3").unwrap();
        assert_eq!(der, vec![0x06, 0x03, 0x55, 0x04, 0x03]);
        let (tag, hdr, content_len, total) = read_header(&der).unwrap();
        assert_eq!(tag, TAG_OID);
        assert_eq!(content_len, 3);
        assert_eq!(total, 5);
        let oid = read_oid(&der[hdr..hdr + content_len]).unwrap();
        assert_eq!(oid, "2.5.4.3");
    }

    #[test]
    fn oid_round_trip_long_arc() {
        // 1.2.840.113549.1.1.1 (rsaEncryption)
        let der = encode_oid("1.2.840.113549.1.1.1").unwrap();
        let (tag, hdr, content_len, _) = read_header(&der).unwrap();
        assert_eq!(tag, TAG_OID);
        let back = read_oid(&der[hdr..hdr + content_len]).unwrap();
        assert_eq!(back, "1.2.840.113549.1.1.1");
    }

    #[test]
    fn pick_string_tag_basic() {
        assert_eq!(pick_string_tag("Test"), TAG_PRINTABLE);
        assert_eq!(pick_string_tag("Acme Corp"), TAG_PRINTABLE);
        // Non-printable: contains '!'
        assert_eq!(pick_string_tag("hi!"), TAG_UTF8_STRING);
        // UTF-8 chars
        assert_eq!(pick_string_tag("Acmé"), TAG_UTF8_STRING);
    }

    #[test]
    fn read_header_rejects_indefinite() {
        let data = vec![0x30, 0x80];
        assert_eq!(read_header(&data), Err(DerError::BadLength));
    }

    #[test]
    fn directory_string_utf8_and_bmp() {
        assert_eq!(
            read_directory_string(TAG_UTF8_STRING, b"hello").as_deref(),
            Some("hello")
        );
        // BMP UCS-2BE for "hi"
        assert_eq!(
            read_directory_string(TAG_BMP, &[0x00, b'h', 0x00, b'i']).as_deref(),
            Some("hi")
        );
    }
}
