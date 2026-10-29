//! Writing an image at P1: lay out the metadata, then stream each anonymous
//! segment's live bytes into a temp file next to the target, `fsync`, and
//! `rename` over the target. A reader therefore sees either the old image,
//! the new one, or none -- never a partial file (which would also fail the
//! length check in the header).

use super::format::{
    limits, meta_checksum, put_u64, DataDiff, Header, SegEntry, CHECKSUM_AT, FORMAT_VERSION,
    HEADER_SIZE, RUN_ENTRY_SIZE, RW_ENTRY_SIZE, SEG_ENTRY_SIZE, SEG_HUGETLB, SEG_SHARED,
    SOURCE_ANON, SOURCE_FILE,
};
use super::hash::hash64;
use super::key::{field, BootKey, StatIdentity};
use super::prot;
use crate::image_layout::{self, Backing, Segment};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// One fixed-address mapping to image. `backing` is WP2's registry type:
/// `Anonymous` segments have their bytes stored in the image; `File` segments
/// are re-mapped from the recorded file on restore (identity-checked), and
/// must never have been written (a written private file mapping would have to
/// be imaged as `Anonymous`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SegmentSpec {
    /// [`super::kind`] value.
    pub kind: u32,
    pub addr: usize,
    pub len: usize,
    /// [`prot`] bits the restore maps with (the protection *at P1*).
    pub prot: u32,
    pub backing: Backing,
}

impl SegmentSpec {
    /// The spec for a registry segment, with its recorded protection.
    pub fn from_segment(seg: &Segment) -> SegmentSpec {
        SegmentSpec {
            kind: super::kind::of(seg.kind),
            addr: seg.addr,
            len: seg.len,
            prot: seg.prot as u32 & 7,
            backing: seg.backing.clone(),
        }
    }
}

/// The capture's segments as they stand at P1: every segment WP2's registry
/// recorded ([`image_layout::segments`]), split wherever the protection
/// changed after the `mmap` (a heap granule committed with `mprotect`, a poll
/// page armed), using `/proc/self/maps`. A recorded page that is no longer
/// mapped at all is an error: the registry missed an unmap.
pub fn segments_at_p1() -> io::Result<Vec<SegmentSpec>> {
    let segs = image_layout::segments();
    #[cfg(target_os = "linux")]
    {
        let maps = std::fs::read_to_string("/proc/self/maps")?;
        split_by_maps(&segs, &maps)
    }
    #[cfg(not(target_os = "linux"))]
    {
        Ok(segs.iter().map(SegmentSpec::from_segment).collect())
    }
}

/// [`segments_at_p1`]'s split, over a given `/proc/self/maps` text.
pub fn split_by_maps(segs: &[Segment], maps: &str) -> io::Result<Vec<SegmentSpec>> {
    let mut vmas: Vec<(usize, usize, u32)> = Vec::new();
    for line in maps.lines() {
        let mut cols = line.split_whitespace();
        let (Some(range), Some(perms)) = (cols.next(), cols.next()) else {
            continue;
        };
        let Some((a, b)) = range.split_once('-') else {
            continue;
        };
        let (Ok(a), Ok(b)) = (usize::from_str_radix(a, 16), usize::from_str_radix(b, 16)) else {
            continue;
        };
        let pb = perms.as_bytes();
        let mut p = 0;
        if pb.first() == Some(&b'r') {
            p |= prot::READ;
        }
        if pb.get(1) == Some(&b'w') {
            p |= prot::WRITE;
        }
        if pb.get(2) == Some(&b'x') {
            p |= prot::EXEC;
        }
        vmas.push((a, b, p));
    }
    let mut out = Vec::new();
    for seg in segs {
        let (s, e) = (seg.addr, seg.addr + seg.len);
        let mut covered = s;
        for &(a, b, p) in vmas.iter().filter(|v| v.0 < e && v.1 > s) {
            let (lo, hi) = (a.max(s), b.min(e));
            if lo != covered {
                break;
            }
            let mut spec = SegmentSpec::from_segment(seg);
            spec.addr = lo;
            spec.len = hi - lo;
            spec.prot = p;
            if let Backing::File(id) = &mut spec.backing {
                id.offset += (lo - s) as u64;
            }
            out.push(spec);
            covered = hi;
        }
        if covered != e {
            return Err(invalid(format!(
                "recorded segment {:#x}+{:#x} is not fully mapped at P1 (gap at {covered:#x})",
                seg.addr, seg.len
            )));
        }
    }
    Ok(out)
}

/// Everything an image holds.
#[derive(Clone, Copy, Debug)]
pub struct ImageSpec<'a> {
    pub key: &'a BootKey,
    /// The pointer handed back to the restored run (the `SharedVm`).
    pub root: usize,
    pub segments: &'a [SegmentSpec],
    pub diff: &'a DataDiff,
}

/// What [`write_image`] wrote.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WriteReport {
    pub path: PathBuf,
    pub file_len: u64,
    pub meta_len: u64,
    /// Bytes of segment payload stored (after trailing-zero trimming).
    pub payload_bytes: u64,
    /// Bytes of segment address space covered.
    pub mapped_bytes: u64,
    pub diff_runs: usize,
    pub diff_bytes: u64,
}

fn invalid(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, msg.into())
}

fn round_up(v: u64, a: u64) -> u64 {
    v.div_ceil(a) * a
}

fn path_bytes(p: &Path) -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        p.as_os_str().as_bytes().to_vec()
    }
    #[cfg(not(unix))]
    {
        p.to_string_lossy().as_bytes().to_vec()
    }
}

/// Length of `mem` after dropping whole trailing pages that are all zero.
fn trimmed_len(mem: &[u8], page: usize) -> usize {
    let mut end = mem.len();
    while end >= page {
        let pg = &mem[end - page..end];
        let (pre, words, post) = unsafe { pg.align_to::<u64>() };
        let zero = pre.iter().all(|&b| b == 0)
            && words.iter().all(|&w| w == 0)
            && post.iter().all(|&b| b == 0);
        if !zero {
            break;
        }
        end -= page;
    }
    end
}

/// Write the image described by `spec` to `path` atomically.
///
/// Refuses (with `InvalidInput`) a capture WP2's broker marked spoiled
/// ([`image_layout::spoiled`]: some P0..P1 mapping could not be placed or
/// recorded), and a spec that breaks a format rule: unaligned
/// or overlapping segments, an anonymous segment that is mapped but not
/// readable, sizes over [`limits`], diff runs outside their regions, or a key
/// whose `so_layout` field no longer matches this process (a library was
/// loaded between gathering the key and P1, so the restore side would not
/// have it at the address the image's pointers expect).
///
/// # Safety
///
/// For every `Anonymous` segment with [`prot::READ`], `[addr, addr+len)` must
/// be mapped and readable, and must not be written concurrently while this
/// runs (the bytes are read in place, twice: once to hash and trim, once to
/// write).
pub unsafe fn write_image(path: &Path, spec: &ImageSpec<'_>) -> io::Result<WriteReport> {
    if let Some(why) = image_layout::spoiled() {
        return Err(invalid(format!("capture is spoiled: {why}")));
    }
    let page = super::page_size();
    let page64 = page as u64;

    if let Some(stored) = spec.key.get(field::SO_LAYOUT) {
        let now = super::key::so_layout();
        if now != stored {
            return Err(invalid(format!(
                "shared-library layout changed since the key was gathered (now:\n{})",
                String::from_utf8_lossy(&now)
            )));
        }
    }

    // ---- validate segments ---------------------------------------------
    if spec.segments.len() > limits::MAX_SEGMENTS {
        return Err(invalid("too many segments"));
    }
    let mut order: Vec<usize> = (0..spec.segments.len()).collect();
    order.sort_by_key(|&i| spec.segments[i].addr);
    let mut prev_end = 0usize;
    for &i in &order {
        let s = &spec.segments[i];
        if s.len == 0 || s.addr % page != 0 || s.len % page != 0 || s.addr < page {
            return Err(invalid(format!("segment {i} not page-aligned or empty")));
        }
        if s.prot & !7 != 0 {
            return Err(invalid(format!("segment {i} has unknown prot bits")));
        }
        let end = s
            .addr
            .checked_add(s.len)
            .filter(|&e| e as u64 <= limits::MAX_USER_ADDR)
            .ok_or_else(|| invalid(format!("segment {i} out of range")))?;
        if s.addr < prev_end {
            return Err(invalid(format!("segment {i} overlaps another")));
        }
        prev_end = end;
        match &s.backing {
            Backing::Anonymous { .. } => {
                if s.prot != prot::NONE && s.prot & prot::READ == 0 {
                    return Err(invalid(format!("segment {i} is mapped but not readable")));
                }
            }
            Backing::File(id) => {
                if id.offset % page64 != 0 || id.path.as_os_str().is_empty() {
                    return Err(invalid(format!("segment {i} file source misaligned")));
                }
            }
        }
    }

    // ---- validate the diff ---------------------------------------------
    let d = spec.diff;
    if d.regions.len() > limits::MAX_RW_REGIONS
        || d.runs.len() > limits::MAX_DIFF_RUNS
        || d.total_bytes() > limits::MAX_DIFF_BYTES
    {
        return Err(invalid("data diff too large"));
    }
    let mut prev = 0u64;
    for (i, r) in d.runs.iter().enumerate() {
        let end = r.offset + r.bytes.len() as u64;
        if r.bytes.is_empty()
            || (i > 0 && r.offset < prev)
            || !d
                .regions
                .iter()
                .any(|g| r.offset >= g.offset && end <= g.offset + g.len)
        {
            return Err(invalid(format!(
                "diff run {i} empty, unordered or outside regions"
            )));
        }
        prev = end;
    }

    // ---- payload lengths and hashes (reads live memory) -----------------
    let mut entries: Vec<SegEntry> = Vec::with_capacity(order.len());
    let mut strings: Vec<u8> = Vec::new();
    for &i in &order {
        let s = &spec.segments[i];
        let mut e = SegEntry {
            addr: s.addr as u64,
            len: s.len as u64,
            file_off: 0,
            payload_len: 0,
            prot: s.prot,
            kind: s.kind,
            source: SOURCE_ANON,
            path_len: 0,
            payload_hash: 0,
            path_off: 0,
            src_offset: 0,
            src_identity: StatIdentity::default(),
            flags: 0,
        };
        match &s.backing {
            Backing::Anonymous { hugetlb } => {
                if *hugetlb {
                    e.flags |= SEG_HUGETLB;
                }
                if s.prot & prot::READ != 0 {
                    // SAFETY: caller contract.
                    let mem = unsafe { std::slice::from_raw_parts(s.addr as *const u8, s.len) };
                    let n = trimmed_len(mem, page);
                    e.payload_len = n as u64;
                    e.payload_hash = hash64(0, &mem[..n]);
                }
            }
            Backing::File(id) => {
                let pb = path_bytes(&id.path);
                e.source = SOURCE_FILE;
                e.payload_len = e.len;
                e.path_off = strings.len() as u64;
                e.path_len = pb.len() as u32;
                e.src_offset = id.offset;
                e.src_identity = StatIdentity::from_layout(id);
                if id.shared {
                    e.flags |= SEG_SHARED;
                }
                strings.extend_from_slice(&pb);
            }
        }
        entries.push(e);
    }
    if strings.len() as u64 > limits::MAX_STRINGS_BYTES {
        return Err(invalid("segment paths too long"));
    }

    // ---- metadata layout ---------------------------------------------------
    let key = spec.key.encode();
    if key.len() as u64 > limits::MAX_KEY_BYTES {
        return Err(invalid("key too large"));
    }
    let a8 = |v: u64| round_up(v, 8);
    let key_off = HEADER_SIZE as u64;
    let rw_off = a8(key_off + key.len() as u64);
    let seg_off = rw_off + (d.regions.len() * RW_ENTRY_SIZE) as u64;
    let run_off = seg_off + (entries.len() * SEG_ENTRY_SIZE) as u64;
    let diff_off = run_off + (d.runs.len() * RUN_ENTRY_SIZE) as u64;
    let diff_len = d.total_bytes();
    let strings_off = diff_off + diff_len;
    let meta_len = strings_off + strings.len() as u64;
    if meta_len > limits::MAX_META_BYTES {
        return Err(invalid("metadata too large"));
    }
    let meta_mapped_len = round_up(meta_len, page64);
    let mut file_len = meta_mapped_len;
    for e in entries.iter_mut() {
        if e.source == SOURCE_ANON && e.payload_len > 0 {
            e.file_off = file_len;
            file_len += e.payload_len;
        }
    }
    if file_len > limits::MAX_FILE_BYTES {
        return Err(invalid("image too large"));
    }

    let header = Header {
        version: FORMAT_VERSION,
        page_size: page as u32,
        seg_count: entries.len() as u32,
        run_count: d.runs.len() as u32,
        rw_count: d.regions.len() as u32,
        meta_len,
        meta_mapped_len,
        file_len,
        root: spec.root as u64,
        key_off,
        key_len: key.len() as u64,
        seg_off,
        rw_off,
        run_off,
        diff_off,
        diff_len,
        strings_off,
        strings_len: strings.len() as u64,
        load_base: d.load_base,
        checksum: 0,
        flags: 0,
    };
    let mut meta = Vec::with_capacity(meta_mapped_len as usize);
    header.encode(&mut meta);
    meta.extend_from_slice(&key);
    meta.resize(rw_off as usize, 0);
    for r in &d.regions {
        put_u64(&mut meta, r.offset);
        put_u64(&mut meta, r.len);
    }
    for e in &entries {
        e.encode(&mut meta);
    }
    for r in &d.runs {
        put_u64(&mut meta, r.offset);
        put_u64(&mut meta, r.bytes.len() as u64);
    }
    for r in &d.runs {
        meta.extend_from_slice(&r.bytes);
    }
    meta.extend_from_slice(&strings);
    debug_assert_eq!(meta.len() as u64, meta_len);
    let sum = meta_checksum(&meta);
    meta[CHECKSUM_AT..CHECKSUM_AT + 8].copy_from_slice(&sum.to_le_bytes());
    meta.resize(meta_mapped_len as usize, 0);

    // ---- write temp + rename ----------------------------------------------
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let tmp = dir.join(format!(
        ".{}.tmp.{}.{nanos}",
        path.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "image".into()),
        std::process::id()
    ));
    let result = (|| -> io::Result<()> {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        f.write_all(&meta)?;
        for e in &entries {
            if e.source == SOURCE_ANON && e.payload_len > 0 {
                // SAFETY: caller contract; validated readable above.
                let mem = unsafe {
                    std::slice::from_raw_parts(e.addr as usize as *const u8, e.payload_len as usize)
                };
                f.write_all(mem)?;
            }
        }
        f.flush()?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, path)?;
        #[cfg(unix)]
        if let Ok(d) = std::fs::File::open(dir) {
            let _ = d.sync_all();
        }
        Ok(())
    })();
    if let Err(e) = result {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }

    Ok(WriteReport {
        path: path.to_path_buf(),
        file_len,
        meta_len,
        payload_bytes: file_len - meta_mapped_len,
        mapped_bytes: entries.iter().map(|e| e.len).sum(),
        diff_runs: d.runs.len(),
        diff_bytes: diff_len,
    })
}
