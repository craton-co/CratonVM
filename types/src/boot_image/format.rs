//! On-disk layout of a boot image, and the validating parser both the loader
//! and the tests use.
//!
//! All integers are little-endian. The file is:
//!
//! ```text
//! +--------------------------------------------------------------+ 0
//! | header (HEADER_SIZE = 160 bytes)                             |
//! | key blob            (key_off,  key_len)                      |
//! | rw-region table     (rw_off,   rw_count  * 16)               |
//! | segment table       (seg_off,  seg_count * 112)              |
//! | diff-run table      (run_off,  run_count * 16)               |
//! | diff bytes          (diff_off, diff_len)                     |
//! | strings             (strings_off, strings_len)  -- file paths |
//! +-------------------------------------------------- meta_len --+
//! | zero padding to a page boundary                              |
//! +-------------------------------------------- meta_mapped_len -+
//! | segment payloads, each at a page-aligned file offset,        |
//! | each a whole number of pages                                 |
//! +---------------------------------------------------- file_len -+
//! ```
//!
//! Everything in `[0, meta_len)` is covered by one checksum (the checksum
//! field itself excluded), so a torn or bit-flipped header, key, segment table
//! or data diff is refused before anything is mapped. Payloads carry their own
//! hash, verified only on request ([`super::MapOptions::verify_payload`]):
//! verifying reads every page, which is exactly the cost lazy mapping avoids.
//!
//! Header fields (byte offset: type name):
//!
//! ```text
//!   0: [u8;8] magic "CRVMIMG\0"      80: u64 seg_off
//!   8: u32 version                   88: u64 rw_off
//!  12: u32 header_size (160)         96: u64 run_off
//!  16: u32 page_size                104: u64 diff_off
//!  20: u32 seg_count                112: u64 diff_len
//!  24: u32 run_count                120: u64 strings_off
//!  28: u32 rw_count                 128: u64 strings_len
//!  32: u64 meta_len                 136: u64 load_base (capture; informational)
//!  40: u64 meta_mapped_len          144: u64 checksum
//!  48: u64 file_len                 152: u64 flags (0)
//!  56: u64 root
//!  64: u64 key_off
//!  72: u64 key_len
//! ```
//!
//! Segment entry (112 bytes):
//!
//! ```text
//!   0: u64 addr          48: u64 payload_hash    96: i64 src mtime_sec
//!   8: u64 len           56: u64 path_off        104: u32 src mtime_nsec
//!  16: u64 file_off      64: u64 src_offset      108: u32 flags
//!  24: u64 payload_len   72: u64 src dev
//!  32: u32 prot          80: u64 src inode
//!  36: u32 kind          88: u64 src size
//!  40: u32 source (0 = anonymous, payload in image; 1 = file mapping)
//!  44: u32 path_len
//! 108: u32 flags (SEG_SHARED: map the source file MAP_SHARED;
//!                 SEG_HUGETLB: was MAP_HUGETLB at capture, restored as
//!                 ordinary pages from the image)
//! ```
//!
//! An anonymous segment maps `[addr, addr+payload_len)` from the image at
//! `file_off`, and `[addr+payload_len, addr+len)` anonymous zero. A file
//! segment maps `[addr, addr+len)` from the source file at `src_offset`.
//! Segments are stored in ascending, non-overlapping address order.
//!
//! RW-region entry: `u64 offset_from_load_base, u64 len`. Diff-run entry:
//! `u64 offset_from_load_base, u64 len`; the bytes of run *i* follow those of
//! run *i-1* in the diff-bytes section. Runs are ascending, non-overlapping,
//! and each lies inside one RW region.

use super::hash::hash64;
use super::key::StatIdentity;
use std::ops::Range;

pub const MAGIC: [u8; 8] = *b"CRVMIMG\0";
pub const FORMAT_VERSION: u32 = 1;
pub const HEADER_SIZE: usize = 160;
pub const SEG_ENTRY_SIZE: usize = 112;
pub const RW_ENTRY_SIZE: usize = 16;
pub const RUN_ENTRY_SIZE: usize = 16;
pub(crate) const CHECKSUM_AT: usize = 144;
pub(crate) const CHECKSUM_SEED: u64 = 0x4352_564D_494D_4731;

pub const SOURCE_ANON: u32 = 0;
pub const SOURCE_FILE: u32 = 1;

/// [`SegEntry::flags`]: map the source file `MAP_SHARED` (else `MAP_PRIVATE`).
pub const SEG_SHARED: u32 = 1;
/// [`SegEntry::flags`]: the segment was `MAP_HUGETLB` at capture.
pub const SEG_HUGETLB: u32 = 2;

/// Size sanity limits. A header whose fields exceed them is refused as
/// corrupt; the writer refuses to produce one.
pub mod limits {
    pub const MAX_SEGMENTS: usize = 4096;
    pub const MAX_RW_REGIONS: usize = 8;
    pub const MAX_KEY_BYTES: u64 = 1 << 20;
    pub const MAX_DIFF_RUNS: usize = 1 << 20;
    pub const MAX_DIFF_BYTES: u64 = 256 << 20;
    pub const MAX_STRINGS_BYTES: u64 = 1 << 20;
    pub const MAX_META_BYTES: u64 = 512 << 20;
    pub const MAX_FILE_BYTES: u64 = 64 << 30;
    /// Highest mappable user address on x86_64 with 4-level paging.
    pub const MAX_USER_ADDR: u64 = 1 << 47;
}

/// Why an image was not used. Every variant means "cold boot instead"; the
/// variant only says why, for the diagnostic line.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    #[error("boot images are not supported on this platform")]
    Unsupported,
    #[error("no image at the expected path")]
    NotFound,
    #[error("i/o error during {op}: {msg}")]
    Io { op: &'static str, msg: String },
    #[error("image file too small ({0} bytes)")]
    TooSmall(u64),
    #[error("bad magic")]
    BadMagic,
    #[error("unsupported format version {0} (expected {FORMAT_VERSION})")]
    BadVersion(u32),
    #[error("header/table checksum mismatch")]
    ChecksumMismatch,
    #[error("corrupt image: {0}")]
    Corrupt(&'static str),
    #[error("image exceeds size limit: {0}")]
    TooLarge(&'static str),
    #[error("page size differs: image {image}, process {process}")]
    PageSize { image: u64, process: u64 },
    #[error("key mismatch on `{field}`")]
    KeyMismatch { field: String },
    #[error("file-backed segment source changed: {path}")]
    SourceChanged { path: String },
    #[error("launcher binary writable segment differs from the image: {0}")]
    BinaryLayout(&'static str),
    #[error("address range {addr:#x}+{len:#x} is already mapped")]
    AddressBusy { addr: u64, len: u64 },
    #[error("mmap at {addr:#x} failed (errno {errno})")]
    MapFailed { addr: u64, errno: i32 },
    #[error("payload hash mismatch in segment {0}")]
    PayloadHash(usize),
}

impl Refusal {
    pub(crate) fn io(op: &'static str, e: &std::io::Error) -> Refusal {
        if e.kind() == std::io::ErrorKind::NotFound {
            return Refusal::NotFound;
        }
        Refusal::Io {
            op,
            msg: e.to_string(),
        }
    }
}

/// One writable region of the launcher binary, as an offset from its load base.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RwRegionRecord {
    pub offset: u64,
    pub len: u64,
}

/// One changed run of the binary's writable segment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffRun {
    /// Offset from the binary's load base.
    pub offset: u64,
    pub bytes: Vec<u8>,
}

/// The binary's writable-segment change set between P0 and P1.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DataDiff {
    /// Load base at capture (recorded for diagnostics; the key pins it).
    pub load_base: u64,
    /// The writable regions the runs were computed over.
    pub regions: Vec<RwRegionRecord>,
    /// Ascending, non-overlapping runs, each inside one of `regions`.
    pub runs: Vec<DiffRun>,
}

/// Bytes closer than this are folded into one run, trading a few unchanged
/// bytes for fewer table entries.
const MERGE_GAP: usize = 16;

impl DataDiff {
    pub fn total_bytes(&self) -> u64 {
        self.runs.iter().map(|r| r.bytes.len() as u64).sum()
    }

    /// Append the runs where `after` differs from `before` for one region that
    /// starts `region_off` bytes past the load base. Bytes whose offset falls
    /// in any of `excludes` (offsets from the load base) are never included
    /// and never bridged by a merged run.
    pub fn compute_region(
        region_off: u64,
        before: &[u8],
        after: &[u8],
        excludes: &[Range<u64>],
        out: &mut Vec<DiffRun>,
    ) {
        assert_eq!(before.len(), after.len());
        let n = before.len();
        let excluded = |i: usize| {
            let o = region_off + i as u64;
            excludes.iter().any(|r| r.contains(&o))
        };
        let differs = |i: usize| before[i] != after[i] && !excluded(i);
        let mut i = 0;
        while i < n {
            if i + 8 <= n && before[i..i + 8] == after[i..i + 8] {
                i += 8;
                continue;
            }
            if !differs(i) {
                i += 1;
                continue;
            }
            let start = i;
            let mut last = i;
            let mut j = i + 1;
            while j < n && j - last <= MERGE_GAP {
                if excluded(j) {
                    break;
                }
                if before[j] != after[j] {
                    last = j;
                }
                j += 1;
            }
            out.push(DiffRun {
                offset: region_off + start as u64,
                bytes: after[start..=last].to_vec(),
            });
            i = last + 1;
        }
    }
}

/// A decoded segment-table entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SegEntry {
    pub addr: u64,
    pub len: u64,
    pub file_off: u64,
    pub payload_len: u64,
    pub prot: u32,
    pub kind: u32,
    pub source: u32,
    pub path_len: u32,
    pub payload_hash: u64,
    pub path_off: u64,
    pub src_offset: u64,
    pub src_identity: StatIdentity,
    pub flags: u32,
}

impl SegEntry {
    pub(crate) fn encode(&self, out: &mut Vec<u8>) {
        let start = out.len();
        put_u64(out, self.addr);
        put_u64(out, self.len);
        put_u64(out, self.file_off);
        put_u64(out, self.payload_len);
        put_u32(out, self.prot);
        put_u32(out, self.kind);
        put_u32(out, self.source);
        put_u32(out, self.path_len);
        put_u64(out, self.payload_hash);
        put_u64(out, self.path_off);
        put_u64(out, self.src_offset);
        put_u64(out, self.src_identity.dev);
        put_u64(out, self.src_identity.ino);
        put_u64(out, self.src_identity.size);
        put_u64(out, self.src_identity.mtime_sec as u64);
        put_u32(out, self.src_identity.mtime_nsec);
        put_u32(out, self.flags);
        debug_assert_eq!(out.len() - start, SEG_ENTRY_SIZE);
    }

    fn decode(b: &[u8]) -> SegEntry {
        SegEntry {
            addr: rd_u64(b, 0),
            len: rd_u64(b, 8),
            file_off: rd_u64(b, 16),
            payload_len: rd_u64(b, 24),
            prot: rd_u32(b, 32),
            kind: rd_u32(b, 36),
            source: rd_u32(b, 40),
            path_len: rd_u32(b, 44),
            payload_hash: rd_u64(b, 48),
            path_off: rd_u64(b, 56),
            src_offset: rd_u64(b, 64),
            src_identity: StatIdentity {
                dev: rd_u64(b, 72),
                ino: rd_u64(b, 80),
                size: rd_u64(b, 88),
                mtime_sec: rd_u64(b, 96) as i64,
                mtime_nsec: rd_u32(b, 104),
            },
            flags: rd_u32(b, 108),
        }
    }
}

/// The fixed header, decoded.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Header {
    pub version: u32,
    pub page_size: u32,
    pub seg_count: u32,
    pub run_count: u32,
    pub rw_count: u32,
    pub meta_len: u64,
    pub meta_mapped_len: u64,
    pub file_len: u64,
    pub root: u64,
    pub key_off: u64,
    pub key_len: u64,
    pub seg_off: u64,
    pub rw_off: u64,
    pub run_off: u64,
    pub diff_off: u64,
    pub diff_len: u64,
    pub strings_off: u64,
    pub strings_len: u64,
    pub load_base: u64,
    pub checksum: u64,
    pub flags: u64,
}

impl Header {
    pub(crate) fn encode(&self, out: &mut Vec<u8>) {
        let start = out.len();
        out.extend_from_slice(&MAGIC);
        put_u32(out, self.version);
        put_u32(out, HEADER_SIZE as u32);
        put_u32(out, self.page_size);
        put_u32(out, self.seg_count);
        put_u32(out, self.run_count);
        put_u32(out, self.rw_count);
        for v in [
            self.meta_len,
            self.meta_mapped_len,
            self.file_len,
            self.root,
            self.key_off,
            self.key_len,
            self.seg_off,
            self.rw_off,
            self.run_off,
            self.diff_off,
            self.diff_len,
            self.strings_off,
            self.strings_len,
            self.load_base,
            self.checksum,
            self.flags,
        ] {
            put_u64(out, v);
        }
        debug_assert_eq!(out.len() - start, HEADER_SIZE);
    }

    /// Decode and check the parts of the header that can be checked without
    /// the rest of the metadata (magic, version, size limits). Enough to know
    /// how much of the file to map as metadata.
    pub fn decode(b: &[u8], file_len: u64) -> Result<Header, Refusal> {
        if (b.len() as u64) < HEADER_SIZE as u64 || file_len < HEADER_SIZE as u64 {
            return Err(Refusal::TooSmall(file_len.min(b.len() as u64)));
        }
        if b[..8] != MAGIC {
            return Err(Refusal::BadMagic);
        }
        let version = rd_u32(b, 8);
        if version != FORMAT_VERSION {
            return Err(Refusal::BadVersion(version));
        }
        if rd_u32(b, 12) as usize != HEADER_SIZE {
            return Err(Refusal::Corrupt("header size"));
        }
        let h = Header {
            version,
            page_size: rd_u32(b, 16),
            seg_count: rd_u32(b, 20),
            run_count: rd_u32(b, 24),
            rw_count: rd_u32(b, 28),
            meta_len: rd_u64(b, 32),
            meta_mapped_len: rd_u64(b, 40),
            file_len: rd_u64(b, 48),
            root: rd_u64(b, 56),
            key_off: rd_u64(b, 64),
            key_len: rd_u64(b, 72),
            seg_off: rd_u64(b, 80),
            rw_off: rd_u64(b, 88),
            run_off: rd_u64(b, 96),
            diff_off: rd_u64(b, 104),
            diff_len: rd_u64(b, 112),
            strings_off: rd_u64(b, 120),
            strings_len: rd_u64(b, 128),
            load_base: rd_u64(b, 136),
            checksum: rd_u64(b, 144),
            flags: rd_u64(b, 152),
        };
        if h.file_len != file_len {
            return Err(Refusal::Corrupt(
                "file length differs from header (truncated?)",
            ));
        }
        if file_len > limits::MAX_FILE_BYTES {
            return Err(Refusal::TooLarge("file"));
        }
        if h.meta_len > limits::MAX_META_BYTES {
            return Err(Refusal::TooLarge("metadata"));
        }
        let ps = h.page_size as u64;
        if !ps.is_power_of_two() || ps < 4096 {
            return Err(Refusal::Corrupt("page size"));
        }
        if h.meta_len < HEADER_SIZE as u64
            || h.meta_mapped_len < h.meta_len
            || h.meta_mapped_len % ps != 0
            || h.meta_mapped_len > file_len
        {
            return Err(Refusal::Corrupt("metadata length"));
        }
        Ok(h)
    }
}

/// The checksum over `[0, meta_len)` with the checksum field read as absent.
pub(crate) fn meta_checksum(meta: &[u8]) -> u64 {
    let h = hash64(CHECKSUM_SEED, &meta[..CHECKSUM_AT]);
    hash64(h, &meta[CHECKSUM_AT + 8..])
}

/// A validated view of an image's metadata (header through strings). Every
/// accessor below is bounds-safe because [`ImageMeta::parse`] checked the
/// tables against the header and the header against the checksum.
#[derive(Clone, Copy, Debug)]
pub struct ImageMeta<'a> {
    pub header: Header,
    meta: &'a [u8],
}

fn section(off: u64, len: u64, meta_len: u64) -> Result<Range<usize>, Refusal> {
    let end = off
        .checked_add(len)
        .ok_or(Refusal::Corrupt("section overflow"))?;
    if off < HEADER_SIZE as u64 || end > meta_len {
        return Err(Refusal::Corrupt("section out of bounds"));
    }
    Ok(off as usize..end as usize)
}

impl<'a> ImageMeta<'a> {
    /// Validate `meta` (at least the first `meta_len` bytes of an image whose
    /// total length is `file_len`). Nothing is trusted until this returns.
    pub fn parse(meta: &'a [u8], file_len: u64) -> Result<ImageMeta<'a>, Refusal> {
        let h = Header::decode(meta, file_len)?;
        if (meta.len() as u64) < h.meta_len {
            return Err(Refusal::Corrupt("metadata shorter than header says"));
        }
        let meta = &meta[..h.meta_len as usize];
        if meta_checksum(meta) != h.checksum {
            return Err(Refusal::ChecksumMismatch);
        }
        let ml = h.meta_len;
        let ps = h.page_size as u64;
        if h.key_len > limits::MAX_KEY_BYTES {
            return Err(Refusal::TooLarge("key"));
        }
        if h.seg_count as usize > limits::MAX_SEGMENTS {
            return Err(Refusal::TooLarge("segment count"));
        }
        if h.rw_count as usize > limits::MAX_RW_REGIONS {
            return Err(Refusal::TooLarge("rw region count"));
        }
        if h.run_count as usize > limits::MAX_DIFF_RUNS || h.diff_len > limits::MAX_DIFF_BYTES {
            return Err(Refusal::TooLarge("data diff"));
        }
        if h.strings_len > limits::MAX_STRINGS_BYTES {
            return Err(Refusal::TooLarge("strings"));
        }
        section(h.key_off, h.key_len, ml)?;
        section(h.seg_off, h.seg_count as u64 * SEG_ENTRY_SIZE as u64, ml)?;
        section(h.rw_off, h.rw_count as u64 * RW_ENTRY_SIZE as u64, ml)?;
        section(h.run_off, h.run_count as u64 * RUN_ENTRY_SIZE as u64, ml)?;
        section(h.diff_off, h.diff_len, ml)?;
        section(h.strings_off, h.strings_len, ml)?;
        let view = ImageMeta { header: h, meta };

        // Segments: aligned, in range, ascending, non-overlapping.
        let mut prev_end = 0u64;
        for s in view.segments() {
            let aligned = |v: u64| v % ps == 0;
            if s.len == 0
                || !aligned(s.addr)
                || !aligned(s.len)
                || !aligned(s.payload_len)
                || s.payload_len > s.len
                || s.prot & !7 != 0
                || s.flags & !(SEG_SHARED | SEG_HUGETLB) != 0
            {
                return Err(Refusal::Corrupt("segment shape"));
            }
            let end = s
                .addr
                .checked_add(s.len)
                .ok_or(Refusal::Corrupt("segment overflow"))?;
            if end > limits::MAX_USER_ADDR || s.addr < prev_end || s.addr < ps {
                return Err(Refusal::Corrupt("segment order/range"));
            }
            prev_end = end;
            match s.source {
                SOURCE_ANON => {
                    if s.payload_len > 0 {
                        let pend = s
                            .file_off
                            .checked_add(s.payload_len)
                            .ok_or(Refusal::Corrupt("payload overflow"))?;
                        if !aligned(s.file_off) || s.file_off < h.meta_mapped_len || pend > file_len
                        {
                            return Err(Refusal::Corrupt("payload out of bounds"));
                        }
                    }
                }
                SOURCE_FILE => {
                    let pend = s
                        .path_off
                        .checked_add(s.path_len as u64)
                        .ok_or(Refusal::Corrupt("path overflow"))?;
                    if s.payload_len != s.len
                        || s.path_len == 0
                        || pend > h.strings_len
                        || !aligned(s.src_offset)
                    {
                        return Err(Refusal::Corrupt("file segment"));
                    }
                }
                _ => return Err(Refusal::Corrupt("segment source")),
            }
        }

        // RW regions, then runs inside them.
        let regions: Vec<RwRegionRecord> = view.rw_regions().collect();
        for r in &regions {
            if r.len == 0 || r.offset.checked_add(r.len).is_none() {
                return Err(Refusal::Corrupt("rw region"));
            }
        }
        let mut total = 0u64;
        let mut prev = 0u64;
        for (i, (off, len)) in view.run_table().enumerate() {
            let end = off
                .checked_add(len)
                .ok_or(Refusal::Corrupt("run overflow"))?;
            if len == 0 || (i > 0 && off < prev) {
                return Err(Refusal::Corrupt("run order"));
            }
            if !regions
                .iter()
                .any(|r| off >= r.offset && end <= r.offset + r.len)
            {
                return Err(Refusal::Corrupt("run outside rw regions"));
            }
            prev = end;
            total = total
                .checked_add(len)
                .ok_or(Refusal::Corrupt("run overflow"))?;
        }
        if total != h.diff_len {
            return Err(Refusal::Corrupt("run lengths differ from diff length"));
        }
        Ok(view)
    }

    pub fn key_bytes(&self) -> &'a [u8] {
        let h = &self.header;
        &self.meta[h.key_off as usize..(h.key_off + h.key_len) as usize]
    }

    pub fn segments(&self) -> impl Iterator<Item = SegEntry> + 'a {
        let h = self.header;
        let meta = self.meta;
        (0..h.seg_count as usize).map(move |i| {
            let o = h.seg_off as usize + i * SEG_ENTRY_SIZE;
            SegEntry::decode(&meta[o..o + SEG_ENTRY_SIZE])
        })
    }

    pub fn rw_regions(&self) -> impl Iterator<Item = RwRegionRecord> + 'a {
        let h = self.header;
        let meta = self.meta;
        (0..h.rw_count as usize).map(move |i| {
            let o = h.rw_off as usize + i * RW_ENTRY_SIZE;
            RwRegionRecord {
                offset: rd_u64(meta, o),
                len: rd_u64(meta, o + 8),
            }
        })
    }

    fn run_table(&self) -> impl Iterator<Item = (u64, u64)> + 'a {
        let h = self.header;
        let meta = self.meta;
        (0..h.run_count as usize).map(move |i| {
            let o = h.run_off as usize + i * RUN_ENTRY_SIZE;
            (rd_u64(meta, o), rd_u64(meta, o + 8))
        })
    }

    /// `(offset from load base, bytes)` for every diff run. Allocation-free.
    pub fn diff_runs(&self) -> impl Iterator<Item = (u64, &'a [u8])> + 'a {
        let meta = self.meta;
        let mut at = self.header.diff_off as usize;
        self.run_table().map(move |(off, len)| {
            let b = &meta[at..at + len as usize];
            at += len as usize;
            (off, b)
        })
    }

    /// The source path of a file-backed segment (raw bytes; a Unix path).
    pub fn seg_path(&self, s: &SegEntry) -> &'a [u8] {
        let base = self.header.strings_off as usize + s.path_off as usize;
        &self.meta[base..base + s.path_len as usize]
    }
}

pub(crate) fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}
pub(crate) fn put_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_le_bytes());
}
pub(crate) fn rd_u32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
pub(crate) fn rd_u64(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}
