//! Boot image end to end (Linux): one child process maps a few fixed-address
//! anonymous regions, changes two statics between P0 and P1, and writes an
//! image; a fresh child maps the image and must see the same bytes, the same
//! static values and the same root pointer. A third child pre-occupies one of
//! the addresses and must get `AddressBusy` with nothing left mapped and no
//! static touched. A fourth child spoils its capture and must be refused by
//! the writer.
//!
//! The regions go through WP2's broker and registry the way boot mapping
//! sites do (capture mode, `image_layout::map_anonymous`), and the writer
//! takes them from `segments_at_p1`, which splits a reservation whose first
//! page was `mprotect`ed after the `mmap`. Only the arena stand-in is mapped
//! by hand (WP1's arena is not a brokered mapping).
//!
//! The children run with `ADDR_NO_RANDOMIZE` (via `personality` before exec)
//! so the full key -- shared-library layout and binary load base included --
//! matches between them, as it would for the launcher's re-exec. Where the
//! sandbox forbids that personality (some container seccomp profiles), the
//! children drop the two layout fields from the key and say so on stdout.
#![cfg(all(target_os = "linux", target_pointer_width = "64"))]

use cratonvm_types::boot_image::segments_at_p1;
use cratonvm_types::boot_image::{
    kind, prot, try_map, try_map_with, write_image, BootKey, ImageSpec, KeyInputs, MapOptions,
    Refusal, RwSnapshot, SegmentSpec,
};
use cratonvm_types::image_layout::{self, Backing, ImageMode, SegmentKind};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

const ROLE: &str = "BOOT_IMAGE_TEST_ROLE";
const IMG: &str = "BOOT_IMAGE_TEST_PATH";

/// Arena stand-in at WP1's arena base: 256 KiB, fully written.
const R1: usize = image_layout::CAPTURE_ARENA_BASE;
const R1_LEN: usize = 256 << 10;
/// First brokered heap mapping: 1 MiB, first 8 KiB written (zero tail).
const R2: usize = SegmentKind::Heap.region().0;
const R2_LEN: usize = 1 << 20;
/// First brokered side-table mapping: 4 MiB PROT_NONE, first page committed.
const R3: usize = SegmentKind::GcSideTable.region().0;
const R3_LEN: usize = 4 << 20;

static MARKER: AtomicU64 = AtomicU64::new(0);
static TABLE: [AtomicU64; 64] = [const { AtomicU64::new(0) }; 64];
/// A large zero-initialised static, so `.bss` (not only `.data`) is covered.
static BSS_TAIL: [AtomicU64; 4096] = [const { AtomicU64::new(0) }; 4096];

const ADDR_NO_RANDOMIZE: libc::c_ulong = 0x0040000;
const LAYOUT_FIELDS: &[&str] = &["so_layout", "binary_load_base"];

fn aslr_off() -> bool {
    // SAFETY: 0xffffffff queries the persona without changing it.
    let p = unsafe { libc::personality(0xffff_ffff) };
    p >= 0 && (p as libc::c_ulong & ADDR_NO_RANDOMIZE) != 0
        || std::fs::read_to_string("/proc/sys/kernel/randomize_va_space")
            .map(|s| s.trim() == "0")
            .unwrap_or(false)
}

fn key() -> BootKey {
    let k = BootKey::gather(&KeyInputs {
        java_home: None,
        classpath: &[],
        vm_options: "boot-image-restore-test",
    });
    if aslr_off() {
        k
    } else {
        println!("NOTE: ASLR active in child; layout fields dropped from the key");
        k.without(LAYOUT_FIELDS)
    }
}

fn pattern(i: usize) -> u8 {
    (i.wrapping_mul(131) ^ (i >> 9)) as u8
}

fn map_fixed(addr: usize, len: usize, prot: i32) {
    // SAFETY: NOREPLACE never clobbers an existing mapping.
    let p = unsafe {
        libc::mmap(
            addr as *mut libc::c_void,
            len,
            prot,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_FIXED_NOREPLACE,
            -1,
            0,
        )
    };
    assert_eq!(p as usize, addr, "fixed mmap at {addr:#x} failed");
}

fn is_free(addr: usize, len: usize) -> bool {
    // SAFETY: as above; unmapped again at once.
    let p = unsafe {
        libc::mmap(
            addr as *mut libc::c_void,
            len,
            libc::PROT_NONE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_FIXED_NOREPLACE,
            -1,
            0,
        )
    };
    if p as usize == addr {
        unsafe { libc::munmap(p, len) };
        true
    } else {
        if p != libc::MAP_FAILED {
            unsafe { libc::munmap(p, len) };
        }
        false
    }
}

fn role_write(path: &Path) {
    let key = key();
    image_layout::set_mode(ImageMode::Capture);
    let snap = RwSnapshot::take().expect("snapshot"); // ---- P0
    map_fixed(R1, R1_LEN, libc::PROT_READ | libc::PROT_WRITE);
    let anon = libc::MAP_PRIVATE | libc::MAP_ANONYMOUS;
    let r2 = image_layout::map_anonymous(
        SegmentKind::Heap,
        R2_LEN,
        4096,
        libc::PROT_READ | libc::PROT_WRITE,
        anon,
    );
    assert_eq!(r2.map(|p| p as usize), Some(R2));
    let r3 = image_layout::map_anonymous(
        SegmentKind::GcSideTable,
        R3_LEN,
        4096,
        libc::PROT_NONE,
        anon,
    );
    assert_eq!(r3.map(|p| p as usize), Some(R3));
    // SAFETY: just mapped; commit R3's first page after the mmap.
    unsafe {
        assert_eq!(
            libc::mprotect(
                R3 as *mut libc::c_void,
                4096,
                libc::PROT_READ | libc::PROT_WRITE
            ),
            0
        );
        *(R3 as *mut u8) = 0x77;
        let r1 = std::slice::from_raw_parts_mut(R1 as *mut u8, R1_LEN);
        for (i, b) in r1.iter_mut().enumerate() {
            *b = pattern(i);
        }
        let r2 = std::slice::from_raw_parts_mut(R2 as *mut u8, 8192);
        r2.fill(0x5A);
        // A pointer from one region into another survives as-is.
        *(R2 as *mut usize) = R1 + 0x40;
    }
    MARKER.store(0xC0FF_EE12_3456_789A, Ordering::Relaxed);
    for (i, t) in TABLE.iter().enumerate() {
        t.store((i * i + 7) as u64, Ordering::Relaxed);
    }
    BSS_TAIL[4000].store(0xB55, Ordering::Relaxed);
    let diff = snap.diff(&[]); // ---- P1
    let mut segs = vec![SegmentSpec {
        kind: kind::ARENA,
        addr: R1,
        len: R1_LEN,
        prot: prot::READ | prot::WRITE,
        backing: Backing::Anonymous { hugetlb: false },
    }];
    let brokered = segments_at_p1().expect("segments at P1");
    let shape: Vec<(usize, usize, u32)> =
        brokered.iter().map(|s| (s.addr, s.len, s.prot)).collect();
    assert_eq!(
        shape,
        vec![
            (R2, R2_LEN, prot::READ | prot::WRITE),
            (R3, 4096, prot::READ | prot::WRITE),
            (R3 + 4096, R3_LEN - 4096, prot::NONE),
        ]
    );
    segs.extend(brokered);
    let spec = ImageSpec {
        key: &key,
        root: R1 + 0x40,
        segments: &segs,
        diff: &diff,
    };
    // SAFETY: segments are live mappings this thread alone writes.
    let rep = unsafe { write_image(path, &spec) }.expect("write image");
    println!(
        "WRITE-OK file={} payload={} runs={} diff_bytes={}",
        rep.file_len, rep.payload_bytes, rep.diff_runs, rep.diff_bytes
    );
}

fn role_restore(path: &Path) {
    let key = key();
    image_layout::set_mode(ImageMode::Restored);
    assert_eq!(MARKER.load(Ordering::Relaxed), 0);
    // Wrong key first: refused, nothing mapped.
    let wrong = {
        let mut k = key.clone();
        k.push("vm_options", b"other".to_vec());
        k
    };
    assert_eq!(
        try_map(path, &wrong),
        Err(Refusal::KeyMismatch {
            field: "vm_options".into()
        })
    );
    assert!(is_free(R1, R1_LEN));

    let m = try_map_with(
        path,
        &key,
        MapOptions {
            verify_payload: true,
        },
    )
    .expect("restore");
    assert_eq!(m.root, R1 + 0x40);
    assert_eq!(m.segments, 4);
    assert_eq!(m.mapped_bytes as usize, R1_LEN + R2_LEN + R3_LEN);
    assert_eq!(m.image_bytes as usize, R1_LEN + 8192 + 4096);
    // SAFETY: restored mappings.
    unsafe {
        let r1 = std::slice::from_raw_parts(R1 as *const u8, R1_LEN);
        assert!(
            r1.iter().enumerate().all(|(i, &b)| b == pattern(i)),
            "R1 bytes differ"
        );
        assert_eq!(*(R2 as *const usize), R1 + 0x40);
        let r2 = std::slice::from_raw_parts(R2 as *const u8, R2_LEN);
        assert!(r2[8..8192].iter().all(|&b| b == 0x5A));
        assert!(r2[8192..].iter().all(|&b| b == 0), "zero tail not zero");
        // Restored memory is private and writable.
        *(R2 as *mut u8).add(R2_LEN - 1) = 1;
        *(R1 as *mut u8) = 0xEE;
        // The page committed after the mmap came back committed.
        assert_eq!(*(R3 as *const u8), 0x77);
    }
    assert!(
        !is_free(R3 + 4096, R3_LEN - 4096),
        "PROT_NONE reservation not mapped"
    );
    assert_eq!(MARKER.load(Ordering::Relaxed), 0xC0FF_EE12_3456_789A);
    for (i, t) in TABLE.iter().enumerate() {
        assert_eq!(t.load(Ordering::Relaxed), (i * i + 7) as u64);
    }
    assert_eq!(BSS_TAIL[4000].load(Ordering::Relaxed), 0xB55);
    println!(
        "RESTORE-OK runs={} diff_bytes={} image_bytes={}",
        m.diff_runs, m.diff_bytes, m.image_bytes
    );
    // Allocation discipline: heap objects from before the diff are leaked.
    std::mem::forget(key);
    std::mem::forget(wrong);
}

fn role_busy(path: &Path) {
    let key = key();
    map_fixed(R2 + 4096, 4096, libc::PROT_READ);
    let r = try_map(path, &key);
    assert_eq!(
        r,
        Err(Refusal::AddressBusy {
            addr: R2 as u64,
            len: 8192
        })
    );
    // R1 was mapped before R2 failed: it must have been unmapped again.
    assert!(
        is_free(R1, R1_LEN),
        "R1 left mapped after a refused restore"
    );
    assert!(is_free(R3, R3_LEN));
    assert_eq!(
        MARKER.load(Ordering::Relaxed),
        0,
        "diff applied on a refusal"
    );
    println!("BUSY-OK");
}

fn role_spoiled(path: &Path) {
    let key = key();
    image_layout::set_mode(ImageMode::Capture);
    image_layout::spoil("test: a mapping could not be placed");
    let diff = Default::default();
    let spec = ImageSpec {
        key: &key,
        root: 0,
        segments: &[],
        diff: &diff,
    };
    // SAFETY: no segments.
    let e = unsafe { write_image(path, &spec) }.expect_err("spoiled capture written");
    assert!(e.to_string().contains("spoiled"), "{e}");
    assert!(!path.exists());
    println!("SPOILED-OK");
}

/// The child entry point. A no-op unless a parent set the role.
#[test]
fn boot_image_child_role() {
    let (Ok(role), Ok(path)) = (std::env::var(ROLE), std::env::var(IMG)) else {
        return;
    };
    let path = PathBuf::from(path);
    match role.as_str() {
        "write" => role_write(&path),
        "restore" => role_restore(&path),
        "busy" => role_busy(&path),
        "spoiled" => role_spoiled(&path),
        other => panic!("unknown role {other}"),
    }
}

fn run_child(role: &str, path: &Path) -> String {
    let mut cmd = Command::new(std::env::current_exe().unwrap());
    cmd.args([
        "boot_image_child_role",
        "--exact",
        "--nocapture",
        "--test-threads=1",
    ])
    .env(ROLE, role)
    .env(IMG, path);
    // SAFETY: personality is async-signal-safe; failure is tolerated (the
    // child then keys without layout fields).
    unsafe {
        cmd.pre_exec(|| {
            libc::personality(ADDR_NO_RANDOMIZE);
            Ok(())
        });
    }
    let out = cmd.output().expect("spawn child");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "child `{role}` failed: {}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        out.status
    );
    stdout
}

#[test]
fn image_written_by_one_process_restores_in_another() {
    let dir = std::env::temp_dir().join(format!("cvm-boot-image-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("test.img");

    let w = run_child("write", &path);
    assert!(w.contains("WRITE-OK"), "{w}");
    let r = run_child("restore", &path);
    assert!(r.contains("RESTORE-OK"), "{r}");
    let b = run_child("busy", &path);
    assert!(b.contains("BUSY-OK"), "{b}");
    let s = run_child("spoiled", &dir.join("spoiled.img"));
    assert!(s.contains("SPOILED-OK"), "{s}");
    eprintln!("{w}{r}{b}{s}");

    // A corrupted copy is refused in a fresh process too (no mapping needed:
    // the checksum is checked first, so in-process is enough).
    let mut bytes = std::fs::read(&path).unwrap();
    bytes[200] ^= 0x01;
    let bad = dir.join("bad.img");
    std::fs::write(&bad, &bytes).unwrap();
    assert_eq!(try_map(&bad, &key()), Err(Refusal::ChecksumMismatch));
    let _ = std::fs::remove_dir_all(&dir);
}
