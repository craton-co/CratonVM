//! Platform-independent tests: hash, key, diff, format round trip, refusals.
//! The mapping path is exercised by `types/tests/boot_image_restore.rs`
//! (Linux, child processes).

use super::format::{
    limits, Header, HEADER_SIZE, SEG_HUGETLB, SEG_SHARED, SOURCE_ANON, SOURCE_FILE,
};
use super::key::{env_hash, so_layout_from_maps};
use super::*;
use crate::image_layout::{Backing, FileIdentity, Segment, SegmentKind};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// A registry-style identity of `path` (portable: `from_metadata` is Unix-only).
fn layout_identity(path: &Path, shared: bool) -> FileIdentity {
    let st = StatIdentity::of(path).unwrap();
    FileIdentity {
        path: path.to_path_buf(),
        offset: 0,
        shared,
        dev: st.dev,
        ino: st.ino,
        size: st.size,
        mtime_sec: st.mtime_sec,
        mtime_nsec: st.mtime_nsec as i64,
    }
}

const ANON: Backing = Backing::Anonymous { hugetlb: false };

struct Aligned {
    ptr: *mut u8,
    layout: std::alloc::Layout,
}
impl Aligned {
    fn new(len: usize) -> Aligned {
        let layout = std::alloc::Layout::from_size_align(len, page_size()).unwrap();
        // SAFETY: non-zero size.
        let ptr = unsafe { std::alloc::alloc_zeroed(layout) };
        assert!(!ptr.is_null());
        Aligned { ptr, layout }
    }
    fn addr(&self) -> usize {
        self.ptr as usize
    }
    fn bytes(&mut self) -> &mut [u8] {
        // SAFETY: owned allocation.
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.layout.size()) }
    }
}
impl Drop for Aligned {
    fn drop(&mut self) {
        // SAFETY: allocated with this layout.
        unsafe { std::alloc::dealloc(self.ptr, self.layout) }
    }
}

fn tmp_dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("cvm-boot-image-unit-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn test_key(opts: &str) -> BootKey {
    let mut k = BootKey::new();
    k.push("binary", b"build-id:0123".to_vec());
    k.push("cwd", b"/work".to_vec());
    k.push("vm_options", opts.as_bytes().to_vec());
    k
}

fn sample_diff() -> DataDiff {
    DataDiff {
        load_base: 0x5555_0000_0000,
        regions: vec![RwRegionRecord {
            offset: 0x10_000,
            len: 0x2000,
        }],
        runs: vec![
            DiffRun {
                offset: 0x10_010,
                bytes: vec![1, 2, 3, 4],
            },
            DiffRun {
                offset: 0x11_000,
                bytes: vec![9; 40],
            },
        ],
    }
}

/// Writes an image of two anonymous segments (one with a zero tail), one
/// PROT_NONE reservation, and one file-backed segment. Returns the path and
/// the buffers (kept alive) with their expected stored bytes.
fn write_sample(dir: &Path) -> (PathBuf, Vec<Aligned>, WriteReport) {
    let ps = page_size();
    let mut a = Aligned::new(3 * ps);
    let mut b = Aligned::new(ps);
    let c = Aligned::new(2 * ps); // PROT_NONE: bytes never read
    for (i, x) in a.bytes()[..ps + 7].iter_mut().enumerate() {
        *x = (i * 7 + 3) as u8;
    }
    b.bytes().fill(0xAB);
    let src = dir.join("source.bin");
    std::fs::write(&src, vec![5u8; ps]).unwrap();
    let segs = vec![
        SegmentSpec {
            kind: kind::ARENA,
            addr: a.addr(),
            len: 3 * ps,
            prot: prot::READ | prot::WRITE,
            backing: ANON,
        },
        SegmentSpec {
            kind: kind::of(SegmentKind::Heap),
            addr: b.addr(),
            len: ps,
            prot: prot::READ,
            backing: Backing::Anonymous { hugetlb: true },
        },
        SegmentSpec {
            kind: kind::OTHER,
            addr: c.addr(),
            len: 2 * ps,
            prot: prot::NONE,
            backing: ANON,
        },
        SegmentSpec {
            kind: kind::of(SegmentKind::RuntimeImage),
            addr: 0x4000_0000_0000,
            len: ps,
            prot: prot::READ,
            backing: Backing::File(layout_identity(&src, true)),
        },
    ];
    let key = test_key("-Xmx1g");
    let diff = sample_diff();
    let path = dir.join(key.file_name());
    let spec = ImageSpec {
        key: &key,
        root: a.addr() + 64,
        segments: &segs,
        diff: &diff,
    };
    // SAFETY: the anonymous readable segments are live owned buffers.
    let rep = unsafe { write_image(&path, &spec) }.unwrap();
    (path, vec![a, b, c], rep)
}

#[test]
fn hash_is_stable_and_sensitive() {
    let data: Vec<u8> = (0..1000u32).map(|i| (i * 31) as u8).collect();
    let h = hash64(1, &data);
    assert_eq!(h, hash64(1, &data));
    assert_ne!(h, hash64(2, &data));
    for i in [0usize, 7, 31, 32, 500, 999] {
        let mut d = data.clone();
        d[i] ^= 1;
        assert_ne!(h, hash64(1, &d), "flip at {i} not seen");
    }
    assert_ne!(hash64(0, &[]), hash64(0, &[0]));
}

#[test]
fn key_round_trips_and_names_the_mismatched_field() {
    let k = test_key("-Xmx1g");
    let enc = k.encode();
    assert_eq!(BootKey::decode(&enc).unwrap(), k);
    assert_eq!(k.check_encoded(&enc), Ok(()));

    let other = test_key("-Xmx2g");
    assert_eq!(
        other.check_encoded(&enc),
        Err(Refusal::KeyMismatch {
            field: "vm_options".into()
        })
    );
    let mut longer = k.clone();
    longer.push("thp", b"never".to_vec());
    assert_eq!(
        longer.check_encoded(&enc),
        Err(Refusal::KeyMismatch {
            field: "thp".into()
        })
    );
    assert_eq!(
        k.without(&["cwd"]).check_encoded(&enc),
        Err(Refusal::KeyMismatch {
            field: "cwd".into()
        })
    );
    assert_ne!(k.hash(), other.hash());
    assert!(k.file_name().ends_with(".img"));
    assert!(matches!(
        k.check_encoded(&enc[..enc.len() - 1]),
        Err(Refusal::Corrupt(_))
    ));
}

#[test]
fn gathered_key_has_every_field() {
    let cp = vec![PathBuf::from("does-not-exist.jar")];
    let k = BootKey::gather(&KeyInputs {
        java_home: None,
        classpath: &cp,
        vm_options: "--compatible",
    });
    let names: Vec<&str> = k.fields().map(|f| f.0).collect();
    assert_eq!(
        names,
        [
            "binary",
            "binary_load_base",
            "jdk_modules",
            "classpath",
            "cwd",
            "user",
            "env",
            "cpus",
            "phys_ram",
            "page_size",
            "thp",
            "vm_options",
            "so_layout"
        ]
    );
    // Gathering twice in one process is deterministic.
    let k2 = BootKey::gather(&KeyInputs {
        java_home: None,
        classpath: &cp,
        vm_options: "--compatible",
    });
    assert_eq!(k, k2);
}

#[test]
fn env_hash_picks_boot_vars_and_skips_image_control() {
    let p = "CRATONVM_";
    let v = |n: String, val: &str| (OsString::from(n), OsString::from(val));
    let base = vec![
        v(format!("{p}GC"), "z"),
        v("LC_ALL".into(), "C"),
        v("TZ".into(), "UTC"),
    ];
    let h = env_hash(base.clone().into_iter());
    // Irrelevant variable and image-control variable: no change.
    let mut more = base.clone();
    more.push(v("PATH".into(), "/bin"));
    more.push(v(format!("{p}BOOT_IMAGE_DIR"), "/x"));
    more.push(v(format!("{p}BOOT_IMAGE"), "auto"));
    more.reverse(); // order independent
    assert_eq!(h, env_hash(more.into_iter()));
    // Relevant changes are seen.
    for extra in [
        v("JAVA_TOOL_OPTIONS".into(), "-Dx"),
        v("_JAVA_OPTIONS".into(), "-Dy"),
        v("LANG".into(), "de_DE"),
        v(format!("{p}JIT"), "off"),
        // Shares the image-control prefix but configures class loading.
        v(format!("{p}BOOT_IMAGE_CLASS_BYTES"), "1"),
    ] {
        let mut e = base.clone();
        e.push(extra);
        assert_ne!(h, env_hash(e.into_iter()));
    }
}

#[test]
fn so_layout_parses_maps() {
    let maps = "\
55d0c0000000-55d0c0001000 r--p 00000000 08:01 1 /usr/bin/cratonvm
7f0000000000-7f0000001000 r--p 00000000 08:01 2 /usr/lib/x86_64-linux-gnu/libc.so.6
7f0000001000-7f0000002000 r-xp 00001000 08:01 2 /usr/lib/x86_64-linux-gnu/libc.so.6
7f0000100000-7f0000101000 r--p 00000000 08:01 3 /opt/lib/libfoo.so
7f0000200000-7f0000201000 rw-p 00000000 00:00 0
7ffd00000000-7ffd00002000 r-xp 00000000 00:00 0                          [vdso]
7ffd00010000-7ffd00020000 rw-p 00000000 00:00 0                          [stack]
";
    let got = String::from_utf8(so_layout_from_maps(maps)).unwrap();
    assert_eq!(
        got,
        "/usr/lib/x86_64-linux-gnu/libc.so.6@7f0000000000\n/opt/lib/libfoo.so@7f0000100000\n[vdso]@7ffd00000000\n"
    );
}

#[test]
fn diff_runs_merge_small_gaps_and_respect_excludes() {
    let before = vec![0u8; 256];
    let mut after = before.clone();
    after[10] = 1;
    after[20] = 2; // within MERGE_GAP of 10: one run 10..=20
    after[100] = 3; // far: own run
    after[200] = 4;
    after[201] = 5; // 201 excluded
    let mut runs = Vec::new();
    let excluded = 0x1000 + 201..0x1000 + 202;
    DataDiff::compute_region(
        0x1000,
        &before,
        &after,
        std::slice::from_ref(&excluded),
        &mut runs,
    );
    let got: Vec<(u64, Vec<u8>)> = runs.into_iter().map(|r| (r.offset, r.bytes)).collect();
    let mut first = vec![0u8; 11];
    first[0] = 1;
    first[10] = 2;
    assert_eq!(
        got,
        vec![(0x100a, first), (0x1064, vec![3]), (0x10c8, vec![4])]
    );
    let mut none = Vec::new();
    DataDiff::compute_region(0, &before, &before, &[], &mut none);
    assert!(none.is_empty());
}

#[test]
fn format_round_trips() {
    let ps = page_size() as u64;
    let dir = tmp_dir("roundtrip");
    let (path, bufs, rep) = write_sample(&dir);
    let bytes = std::fs::read(&path).unwrap();
    assert_eq!(bytes.len() as u64, rep.file_len);
    let meta = ImageMeta::parse(&bytes, bytes.len() as u64).unwrap();
    let h = meta.header;
    assert_eq!(h.root, bufs[0].addr() as u64 + 64);
    assert_eq!(h.page_size as u64, ps);
    assert_eq!(h.meta_mapped_len % ps, 0);
    assert_eq!(test_key("-Xmx1g").check_encoded(meta.key_bytes()), Ok(()));

    let segs: Vec<SegEntry> = meta.segments().collect();
    assert_eq!(segs.len(), 4);
    assert!(segs.windows(2).all(|w| w[0].addr + w[0].len <= w[1].addr));
    for s in &segs {
        if s.source == SOURCE_ANON && s.payload_len > 0 {
            assert_eq!(s.file_off % ps, 0, "payload not page aligned");
            assert!(s.file_off >= h.meta_mapped_len);
        }
    }
    let by_addr = |a: usize| segs.iter().find(|s| s.addr == a as u64).unwrap();
    // Arena: 3 pages, only the first two hold non-zero bytes.
    let a = by_addr(bufs[0].addr());
    assert_eq!(
        (a.len, a.payload_len, a.kind),
        (3 * ps, 2 * ps, kind::ARENA)
    );
    let stored = &bytes[a.file_off as usize..(a.file_off + a.payload_len) as usize];
    // SAFETY: live buffer.
    let live = unsafe { std::slice::from_raw_parts(bufs[0].addr() as *const u8, 2 * ps as usize) };
    assert_eq!(stored, live);
    assert_eq!(a.payload_hash, hash64(0, live));
    let b = by_addr(bufs[1].addr());
    assert_eq!(b.payload_len, ps);
    assert_eq!(b.flags, SEG_HUGETLB);
    assert_eq!(a.flags, 0);
    assert!(bytes[b.file_off as usize..(b.file_off + ps) as usize]
        .iter()
        .all(|&x| x == 0xAB));
    let c = by_addr(bufs[2].addr());
    assert_eq!((c.prot, c.payload_len), (prot::NONE, 0));
    let f = by_addr(0x4000_0000_0000);
    assert_eq!(f.source, SOURCE_FILE);
    assert_eq!(f.flags, SEG_SHARED);
    assert_eq!(f.kind, SegmentKind::RuntimeImage as u32);
    assert_eq!(f.payload_len, f.len);
    assert_eq!(
        Path::new(&*String::from_utf8_lossy(meta.seg_path(f))),
        dir.join("source.bin")
    );
    assert_eq!(
        f.src_identity,
        StatIdentity::of(&dir.join("source.bin")).unwrap()
    );

    let regions: Vec<RwRegionRecord> = meta.rw_regions().collect();
    assert_eq!(regions, sample_diff().regions);
    let runs: Vec<(u64, Vec<u8>)> = meta.diff_runs().map(|(o, b)| (o, b.to_vec())).collect();
    let want: Vec<(u64, Vec<u8>)> = sample_diff()
        .runs
        .into_iter()
        .map(|r| (r.offset, r.bytes))
        .collect();
    assert_eq!(runs, want);
    assert_eq!(rep.diff_bytes, 44);
    assert_eq!(rep.payload_bytes, 3 * ps);

    // No temp file left behind.
    let left: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().contains(".tmp."))
        .collect();
    assert!(left.is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn key_mismatch_is_refused() {
    let dir = tmp_dir("keymismatch");
    let (path, _bufs, _) = write_sample(&dir);
    let bytes = std::fs::read(&path).unwrap();
    let meta = ImageMeta::parse(&bytes, bytes.len() as u64).unwrap();
    assert_eq!(
        test_key("-Xmx512m").check_encoded(meta.key_bytes()),
        Err(Refusal::KeyMismatch {
            field: "vm_options".into()
        })
    );
    // The loader refuses before mapping anything (on Linux: key mismatch; off
    // Linux: unsupported).
    let r = try_map(&path, &test_key("-Xmx512m"));
    #[cfg(all(target_os = "linux", target_pointer_width = "64"))]
    assert_eq!(
        r,
        Err(Refusal::KeyMismatch {
            field: "vm_options".into()
        })
    );
    #[cfg(not(all(target_os = "linux", target_pointer_width = "64")))]
    assert_eq!(r, Err(Refusal::Unsupported));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn corruption_is_refused() {
    let dir = tmp_dir("corrupt");
    let (path, _bufs, _) = write_sample(&dir);
    let good = std::fs::read(&path).unwrap();
    let meta_len = Header::decode(&good, good.len() as u64).unwrap().meta_len as usize;
    // Flip one bit at a spread of offsets across the whole metadata.
    let mut offsets: Vec<usize> = (0..meta_len).step_by(7).collect();
    offsets.push(meta_len - 1);
    for off in offsets {
        let mut bad = good.clone();
        bad[off] ^= 0x10;
        let r = ImageMeta::parse(&bad, bad.len() as u64);
        assert!(r.is_err(), "bit flip at {off} accepted");
    }
    // Truncation, extension, empty.
    let r = ImageMeta::parse(&good[..good.len() - 1], good.len() as u64 - 1);
    assert!(matches!(r, Err(Refusal::Corrupt(_))), "{r:?}");
    assert!(matches!(
        ImageMeta::parse(&good[..10], 10),
        Err(Refusal::TooSmall(_))
    ));
    let mut wrong_magic = good.clone();
    wrong_magic[0] = b'X';
    assert_eq!(
        ImageMeta::parse(&wrong_magic, good.len() as u64).unwrap_err(),
        Refusal::BadMagic
    );
    // A file truncated on disk is refused by the loader too.
    let trunc = dir.join("trunc.img");
    std::fs::write(&trunc, &good[..good.len() - page_size()]).unwrap();
    let r = try_map(&trunc, &test_key("-Xmx1g"));
    assert!(r.is_err());
    let flipped = dir.join("flipped.img");
    let mut bad = good.clone();
    bad[HEADER_SIZE + 3] ^= 1;
    std::fs::write(&flipped, &bad).unwrap();
    let r = try_map(&flipped, &test_key("-Xmx1g"));
    #[cfg(all(target_os = "linux", target_pointer_width = "64"))]
    assert_eq!(r, Err(Refusal::ChecksumMismatch));
    #[cfg(not(all(target_os = "linux", target_pointer_width = "64")))]
    assert_eq!(r, Err(Refusal::Unsupported));
    assert_eq!(
        try_map(&dir.join("absent.img"), &test_key("x")).unwrap_err(),
        if cfg!(all(target_os = "linux", target_pointer_width = "64")) {
            Refusal::NotFound
        } else {
            Refusal::Unsupported
        }
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn writer_rejects_bad_specs() {
    let ps = page_size();
    let dir = tmp_dir("badspec");
    let buf = Aligned::new(2 * ps);
    let key = test_key("x");
    let diff = DataDiff::default();
    let seg = |addr: usize, len: usize| SegmentSpec {
        kind: 0,
        addr,
        len,
        prot: prot::READ,
        backing: ANON,
    };
    let try_write = |segs: &[SegmentSpec], diff: &DataDiff| {
        let spec = ImageSpec {
            key: &key,
            root: 0,
            segments: segs,
            diff,
        };
        // SAFETY: every readable segment lies inside `buf`.
        unsafe { write_image(&dir.join("x.img"), &spec) }
    };
    assert!(try_write(&[seg(buf.addr() + 1, ps)], &diff).is_err());
    assert!(try_write(&[seg(buf.addr(), ps + 1)], &diff).is_err());
    assert!(try_write(&[seg(buf.addr(), 2 * ps), seg(buf.addr() + ps, ps)], &diff).is_err());
    let mut wonly = seg(buf.addr(), ps);
    wonly.prot = prot::WRITE;
    assert!(try_write(&[wonly], &diff).is_err());
    let outside = DataDiff {
        load_base: 0,
        regions: vec![RwRegionRecord { offset: 0, len: 8 }],
        runs: vec![DiffRun {
            offset: 6,
            bytes: vec![1, 2, 3],
        }],
    };
    assert!(try_write(&[seg(buf.addr(), ps)], &outside).is_err());
    assert!(try_write(&[seg(buf.addr(), ps)], &diff).is_ok());
    assert!(limits::MAX_SEGMENTS >= 64);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn registry_segments_split_by_p1_protection() {
    let heap = Segment {
        kind: SegmentKind::Heap,
        addr: 0x2800_0000_0000,
        len: 0x10_000,
        prot: 0,
        backing: ANON,
    };
    let file = Segment {
        kind: SegmentKind::RuntimeImage,
        addr: 0x4800_0000_0000,
        len: 0x2000,
        prot: 1,
        backing: Backing::File(FileIdentity {
            path: PathBuf::from("/jdk/lib/modules"),
            offset: 0,
            shared: true,
            dev: 1,
            ino: 2,
            size: 0x2000,
            mtime_sec: 3,
            mtime_nsec: 4,
        }),
    };
    // The heap's first 8 KiB was committed (mprotect RW); the file is one VMA
    // that also covers bytes beyond the recorded range.
    let maps = "\
280000000000-280000002000 rw-p 00000000 00:00 0
280000002000-280000010000 ---p 00000000 00:00 0
480000000000-480000004000 r--s 00000000 08:01 2   /jdk/lib/modules
";
    let got = split_by_maps(&[heap.clone(), file.clone()], maps).unwrap();
    let shape: Vec<(usize, usize, u32)> = got.iter().map(|s| (s.addr, s.len, s.prot)).collect();
    assert_eq!(
        shape,
        vec![
            (0x2800_0000_0000, 0x2000, prot::READ | prot::WRITE),
            (0x2800_0000_2000, 0xE000, prot::NONE),
            (0x4800_0000_0000, 0x2000, prot::READ),
        ]
    );
    assert_eq!(got[2].backing, file.backing);
    // A recorded range with a hole is refused (the registry missed an unmap).
    let holed = "280000000000-280000002000 rw-p 00000000 00:00 0\n";
    assert!(split_by_maps(&[heap], holed).is_err());
}

#[test]
fn cache_dir_prefers_override() {
    let o = Path::new("/explicit/dir");
    assert_eq!(cache_dir(Some(o)).unwrap(), o);
    let k = test_key("x");
    assert_eq!(image_path(o, &k), o.join(k.file_name()));
}
