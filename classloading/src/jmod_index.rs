// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! A JMOD's entry index read from its ZIP central directory only.
//!
//! zr z8/fm (2026-10-13), `CRATONVM_JMOD_LAZY_INDEX` (default ON, `=0` kill).
//!
//! `zip::ZipArchive::new` (zip 2.4.2) reads the **local header** of every entry
//! while it builds its index (`central_header_to_zip_file` calls
//! `find_data_start` for each record, `zip-2.4.2/src/read.rs:1259`), so opening
//! a mapped archive faults in roughly one page per entry. For the 70 JMODs a
//! JDK that ships `jmods/` puts on the boot class path that is close to every
//! page of every file: ~47 MB of a Hello world's working set (z7/rs,
//! `docs/known-issues/footprint/boot-rss-by-owner-20261013.md`). HotSpot
//! never reads a JMOD at run time.
//!
//! This index reads the end-of-central-directory record and the central
//! directory, which sit together at the end of the file, and nothing else. A
//! local header is read only when its entry is served
//! ([`JmodCentralIndex::locate`]). Lookups answer exactly what the `ZipArchive`
//! path answered, by construction:
//!
//! - names are deduplicated the way `ZipArchive` deduplicates them (an
//!   `IndexMap` insert: the first occurrence keeps its position, the last one's
//!   record wins), so `names` is the list `by_index_raw(0..len)` produced;
//! - the served bytes are the same: a stored entry is the `size()` bytes at the
//!   data start, a deflated one is inflated and CRC-checked as zip's
//!   `Crc32Reader` checks it (the caller does that part, `class_path.rs`).
//!
//! Anything this reader does not model -- ZIP64, encryption, a method other
//! than stored or deflated, a non-ASCII name, a Unicode-path / ZIP64 / AES
//! extra field, a malformed extra field, a prefixed archive, a multi-disk or
//! commented end record that does not close the file -- makes [`parse`]
//! answer `None`, and the caller keeps the `ZipArchive` path for that JMOD.
//!
//! One residual, by design: a JMOD with a corrupt local header used to fail
//! `ZipArchive::new` and was dropped from the class path whole; here only the
//! damaged entry fails, when it is served.
//!
//! [`parse`]: JmodCentralIndex::parse

use rustc_hash::FxHashMap;

const EOCD_SIG: u32 = 0x0605_4b50;
const EOCD_LEN: usize = 22;
const ZIP64_LOCATOR_SIG: u32 = 0x0706_4b50;
const ZIP64_LOCATOR_LEN: usize = 20;
const CDH_SIG: u32 = 0x0201_4b50;
const CDH_LEN: usize = 46;
const LFH_SIG: u32 = 0x0403_4b50;
const LFH_LEN: usize = 30;
const METHOD_STORED: u16 = 0;
const METHOD_DEFLATED: u16 = 8;

/// Extra-field ids that change what `ZipArchive` would report for an entry
/// (its sizes, its name, or how it is decrypted). Any of them sends the whole
/// JMOD back to the `ZipArchive` path.
const EXTRA_ZIP64: u16 = 0x0001;
const EXTRA_AES: u16 = 0x9901;
const EXTRA_UNICODE_PATH: u16 = 0x7075;

/// One central-directory record, as much of it as serving the entry needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CdRecord {
    header_start: u32,
    compressed: u32,
    uncompressed: u32,
    crc32: u32,
    stored: bool,
}

/// Where an entry's bytes are, once its local header has been read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct JmodEntryLocation {
    /// Offset of the first data byte, relative to the ZIP start.
    pub data_start: usize,
    /// `compressed_size` from the central directory.
    pub compressed: usize,
    /// `size()` from the central directory.
    pub uncompressed: u64,
    pub crc32: u32,
    pub stored: bool,
}

/// The central-directory index of one JMOD's ZIP payload.
pub(crate) struct JmodCentralIndex {
    records: Vec<CdRecord>,
    // addr-keyed: no -- keyed by entry NAME, not by an address.
    by_name: FxHashMap<Box<str>, u32>,
    dir_start: u64,
}

fn u16_at(b: &[u8], at: usize) -> Option<u16> {
    let s = b.get(at..at.checked_add(2)?)?;
    Some(u16::from_le_bytes([s[0], s[1]]))
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    let s = b.get(at..at.checked_add(4)?)?;
    Some(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

/// The end-of-central-directory record's offset: the one whose comment ends
/// exactly at the end of the data. `None` when there is none.
fn find_eocd(zip: &[u8]) -> Option<usize> {
    if zip.len() < EOCD_LEN {
        return None;
    }
    let last = zip.len() - EOCD_LEN;
    let first = last.saturating_sub(u16::MAX as usize);
    let mut pos = last;
    loop {
        if u32_at(zip, pos)? == EOCD_SIG {
            let comment = u16_at(zip, pos + 20)? as usize;
            if pos + EOCD_LEN + comment == zip.len() {
                return Some(pos);
            }
        }
        if pos == first {
            return None;
        }
        pos -= 1;
    }
}

/// `true` when the extra field is well-formed TLV and names no id this reader
/// does not model.
fn extra_field_is_plain(extra: &[u8]) -> bool {
    let mut at = 0usize;
    while at < extra.len() {
        let (Some(id), Some(len)) = (u16_at(extra, at), u16_at(extra, at + 2)) else {
            return false;
        };
        if matches!(id, EXTRA_ZIP64 | EXTRA_AES | EXTRA_UNICODE_PATH) {
            return false;
        }
        at += 4 + len as usize;
    }
    at == extra.len()
}

impl JmodCentralIndex {
    /// Index `zip` (the JMOD's bytes after its 4-byte `JM` header) from the
    /// central directory. Returns the index and the deduplicated entry names
    /// in first-occurrence order, or `None` when the archive needs the
    /// `ZipArchive` path (see the module docs).
    ///
    /// Touches only the end record and the central directory.
    pub(crate) fn parse(zip: &[u8]) -> Option<(Self, Vec<String>)> {
        let eocd = find_eocd(zip)?;
        if eocd >= ZIP64_LOCATOR_LEN && u32_at(zip, eocd - ZIP64_LOCATOR_LEN)? == ZIP64_LOCATOR_SIG
        {
            return None;
        }
        let disk = u16_at(zip, eocd + 4)?;
        let cd_disk = u16_at(zip, eocd + 6)?;
        let on_disk = u16_at(zip, eocd + 8)?;
        let total = u16_at(zip, eocd + 10)?;
        let cd_size = u32_at(zip, eocd + 12)?;
        let cd_offset = u32_at(zip, eocd + 16)?;
        if disk != 0 || cd_disk != 0 || on_disk != total || total == u16::MAX {
            return None;
        }
        if cd_size == u32::MAX || cd_offset == u32::MAX {
            return None;
        }
        // A prefixed archive (the directory is not where the end record says)
        // is zip's `ArchiveOffset::Detect` business, not ours.
        let cd_start = cd_offset as usize;
        if cd_start.checked_add(cd_size as usize)? != eocd {
            return None;
        }

        let total = total as usize;
        let mut records: Vec<CdRecord> = Vec::with_capacity(total);
        // addr-keyed: no -- keyed by entry NAME.
        let mut by_name: FxHashMap<Box<str>, u32> =
            FxHashMap::with_capacity_and_hasher(total, Default::default());
        let mut names: Vec<String> = Vec::with_capacity(total);
        let mut at = cd_start;
        for _ in 0..total {
            if u32_at(zip, at)? != CDH_SIG {
                return None;
            }
            let flags = u16_at(zip, at + 8)?;
            let method = u16_at(zip, at + 10)?;
            let crc32 = u32_at(zip, at + 16)?;
            let compressed = u32_at(zip, at + 20)?;
            let uncompressed = u32_at(zip, at + 24)?;
            let name_len = u16_at(zip, at + 28)? as usize;
            let extra_len = u16_at(zip, at + 30)? as usize;
            let comment_len = u16_at(zip, at + 32)? as usize;
            let disk_start = u16_at(zip, at + 34)?;
            let header_start = u32_at(zip, at + 42)?;
            // Encrypted (bit 0) or strongly encrypted (bit 6).
            if flags & 0x0041 != 0 {
                return None;
            }
            if method != METHOD_STORED && method != METHOD_DEFLATED {
                return None;
            }
            if compressed == u32::MAX || uncompressed == u32::MAX || header_start == u32::MAX {
                return None;
            }
            if disk_start != 0 {
                return None;
            }
            // `ZipArchive::new` refuses this archive whole; so do we, and the
            // caller's `ZipArchive` path then reports it as it always did.
            if u64::from(header_start) >= cd_offset as u64 {
                return None;
            }
            let name_at = at + CDH_LEN;
            let extra_at = name_at + name_len;
            let next = extra_at.checked_add(extra_len)?.checked_add(comment_len)?;
            if next > eocd {
                return None;
            }
            let raw_name = zip.get(name_at..extra_at)?;
            if !raw_name.is_ascii() {
                return None;
            }
            if !extra_field_is_plain(zip.get(extra_at..extra_at + extra_len)?) {
                return None;
            }
            // ASCII is valid UTF-8 and reads the same in CP437.
            let name = std::str::from_utf8(raw_name).ok()?;
            let record = CdRecord {
                header_start,
                compressed,
                uncompressed,
                crc32,
                stored: method == METHOD_STORED,
            };
            match by_name.get(name) {
                Some(&slot) => records[slot as usize] = record,
                None => {
                    let slot = u32::try_from(records.len()).ok()?;
                    records.push(record);
                    by_name.insert(name.into(), slot);
                    names.push(name.to_string());
                }
            }
            at = next;
        }
        records.shrink_to_fit();
        Some((
            Self {
                records,
                by_name,
                dir_start: cd_offset as u64,
            },
            names,
        ))
    }

    /// Number of distinct entry names (what `ZipArchive::len` answered).
    pub(crate) fn len(&self) -> usize {
        self.records.len()
    }

    /// Is `name` an entry? No local header is read: the `ZipArchive` path
    /// had read and checked every one at open time, so a name it held was
    /// always servable.
    pub(crate) fn contains(&self, name: &str) -> bool {
        self.by_name.contains_key(name)
    }

    /// Read `name`'s local header and say where its data is. `None` when the
    /// name is absent, the header is not a local header, or the data would
    /// start past the central directory or run past the end of `zip` -- the
    /// cases in which zip's `by_name` (or its open) failed.
    pub(crate) fn locate(&self, zip: &[u8], name: &str) -> Option<JmodEntryLocation> {
        let rec = self.records[*self.by_name.get(name)? as usize];
        let header = rec.header_start as usize;
        if u32_at(zip, header)? != LFH_SIG {
            return None;
        }
        let name_len = u16_at(zip, header + 26)? as usize;
        let extra_len = u16_at(zip, header + 28)? as usize;
        let data_start = header + LFH_LEN + name_len + extra_len;
        if data_start as u64 > self.dir_start {
            return None;
        }
        let compressed = rec.compressed as usize;
        if data_start.checked_add(compressed)? > zip.len() {
            return None;
        }
        Some(JmodEntryLocation {
            data_start,
            compressed,
            uncompressed: u64::from(rec.uncompressed),
            crc32: rec.crc32,
            stored: rec.stored,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal ZIP writer: stored entries only, optional extra fields, so
    /// the parser's refusals can be driven byte by byte.
    fn zip_with(entries: &[(&str, &[u8], &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut cd = Vec::new();
        for (name, data, extra) in entries {
            let off = out.len() as u32;
            let crc = crc32fast::hash(data);
            out.extend_from_slice(&LFH_SIG.to_le_bytes());
            out.extend_from_slice(&[20, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
            out.extend_from_slice(&crc.to_le_bytes());
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(&(name.len() as u16).to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(data);
            cd.extend_from_slice(&CDH_SIG.to_le_bytes());
            cd.extend_from_slice(&[20, 0, 20, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
            cd.extend_from_slice(&crc.to_le_bytes());
            cd.extend_from_slice(&(data.len() as u32).to_le_bytes());
            cd.extend_from_slice(&(data.len() as u32).to_le_bytes());
            cd.extend_from_slice(&(name.len() as u16).to_le_bytes());
            cd.extend_from_slice(&(extra.len() as u16).to_le_bytes());
            // comment length, disk start, internal and external attributes
            cd.extend_from_slice(&[0u8; 10]);
            cd.extend_from_slice(&off.to_le_bytes());
            cd.extend_from_slice(name.as_bytes());
            cd.extend_from_slice(extra);
        }
        let cd_off = out.len() as u32;
        out.extend_from_slice(&cd);
        out.extend_from_slice(&EOCD_SIG.to_le_bytes());
        out.extend_from_slice(&[0, 0, 0, 0]);
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        out.extend_from_slice(&(cd.len() as u32).to_le_bytes());
        out.extend_from_slice(&cd_off.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out
    }

    #[test]
    fn zr8_fm_index_matches_zip_archive_names_and_locations() {
        let bytes = zip_with(&[
            ("classes/a/A.class", b"AAAA", b""),
            ("classes/b/B.class", b"BBBBBB", b""),
            ("lib/x.so", b"xx", b""),
        ]);
        let (idx, names) = JmodCentralIndex::parse(&bytes).expect("plain archive parses");
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes.clone())).unwrap();
        let zip_names: Vec<String> = (0..zip.len())
            .map(|i| zip.by_index_raw(i).unwrap().name().to_string())
            .collect();
        assert_eq!(names, zip_names);
        assert_eq!(idx.len(), zip.len());
        for n in &zip_names {
            let loc = idx.locate(&bytes, n).expect("present entry locates");
            let e = zip.by_name(n).unwrap();
            assert_eq!(loc.data_start as u64, e.data_start());
            assert_eq!(loc.uncompressed, e.size());
            assert_eq!(loc.compressed as u64, e.compressed_size());
            assert_eq!(loc.crc32, e.crc32());
            assert!(loc.stored);
        }
        assert!(!idx.contains("classes/c/C.class"));
        assert!(idx.locate(&bytes, "classes/c/C.class").is_none());
    }

    #[test]
    fn zr8_fm_duplicate_names_keep_first_position_and_last_record() {
        let bytes = zip_with(&[
            ("dup", b"first", b""),
            ("other", b"o", b""),
            ("dup", b"second!", b""),
        ]);
        let (idx, names) = JmodCentralIndex::parse(&bytes).unwrap();
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes.clone())).unwrap();
        let zip_names: Vec<String> = (0..zip.len())
            .map(|i| zip.by_index_raw(i).unwrap().name().to_string())
            .collect();
        assert_eq!(names, zip_names);
        assert_eq!(names, vec!["dup".to_string(), "other".to_string()]);
        let loc = idx.locate(&bytes, "dup").unwrap();
        let e = zip.by_name("dup").unwrap();
        assert_eq!(loc.data_start as u64, e.data_start());
        assert_eq!(loc.uncompressed, 7);
    }

    #[test]
    fn zr8_fm_refuses_what_it_does_not_model() {
        // A ZIP64 extra field on one entry: the whole archive goes back.
        let zip64 = zip_with(&[("a", b"a", &[0x01, 0x00, 0x00, 0x00])]);
        assert!(JmodCentralIndex::parse(&zip64).is_none());
        // A Unicode-path extra field.
        let upath = zip_with(&[("a", b"a", &[0x75, 0x70, 0x00, 0x00])]);
        assert!(JmodCentralIndex::parse(&upath).is_none());
        // A malformed extra field (declares more bytes than it has).
        let bad = zip_with(&[("a", b"a", &[0x55, 0x54, 0x09, 0x00, 0x01])]);
        assert!(JmodCentralIndex::parse(&bad).is_none());
        // A plain extended-timestamp field is fine.
        let ts = zip_with(&[("a", b"a", &[0x55, 0x54, 0x01, 0x00, 0x00])]);
        assert!(JmodCentralIndex::parse(&ts).is_some());
        // A non-ASCII name.
        let utf = zip_with(&[("caf\u{e9}", b"a", b"")]);
        assert!(JmodCentralIndex::parse(&utf).is_none());
        // A prefixed archive.
        let mut prefixed = vec![0u8; 7];
        prefixed.extend_from_slice(&zip_with(&[("a", b"a", b"")]));
        assert!(JmodCentralIndex::parse(&prefixed).is_none());
        // Not a ZIP at all.
        assert!(JmodCentralIndex::parse(b"not a zip").is_none());
    }

    #[test]
    fn zr8_fm_a_damaged_local_header_fails_only_its_entry() {
        let mut bytes = zip_with(&[("a", b"aaaa", b""), ("b", b"bbbb", b"")]);
        let (idx, _) = JmodCentralIndex::parse(&bytes).unwrap();
        // Break entry "a"'s local header signature.
        bytes[0] ^= 0xFF;
        assert!(idx.contains("a"));
        assert!(idx.locate(&bytes, "a").is_none());
        assert!(idx.locate(&bytes, "b").is_some());
    }
}
