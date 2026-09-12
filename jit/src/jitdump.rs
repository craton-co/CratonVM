// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! jitdump: the binary JIT log `perf inject --jit` turns into one ELF image per
//! compiled body (`CRATONVM_JIT_JITDUMP`, default-OFF, Linux only).
//!
//! Format: `tools/perf/Documentation/jitdump-specification.txt` in the Linux
//! tree, version 1. All integers are in host byte order; the magic tells the
//! reader which that was.
//!
//! # What is written
//!
//! * The file header, once: magic `0x4A695444`, version 1, `EM_X86_64` or
//!   `EM_AARCH64`, pid, and a `CLOCK_MONOTONIC` timestamp (the clock perf uses
//!   with `perf record -k 1`, i.e. `-k CLOCK_MONOTONIC`).
//! * One `JIT_CODE_LOAD` per published region, carrying the name, a
//!   monotonically increasing code index and a copy of the machine code taken
//!   at publication.
//! * Before a load on x86-64, a `JIT_CODE_UNWINDING_INFO` record, but only when
//!   the body begins with the standard frame record `push rbp; mov rbp, rsp`
//!   (`55 48 89 E5`). That is how `x64::Compiler::emit_prologue` and the OSR
//!   trampoline both open (`jit/src/x64/frames.rs`), and it is checked on the
//!   actual bytes rather than assumed, so a body that opens differently gets no
//!   unwind table instead of a wrong one. The table says: at entry
//!   `CFA = RSP+8, RA at CFA-8`; after the push `CFA = RSP+16, RBP at CFA-16`;
//!   after the move `CFA = RBP+16`. It is not corrected for the epilogue, so a
//!   sample on the final `pop rbp`/`ret` bytes unwinds one frame wrong; perf
//!   tolerates that. No unwinding info is emitted on aarch64.
//! * `JIT_CODE_CLOSE` from an `atexit` handler, so it is written when the
//!   process leaves through `exit` (a normal `main` return or
//!   `std::process::exit`). A crash, `_exit`, or a kill skips it; perf does not
//!   require the record.
//!
//! No unload records: version 1 has none. Freed and reused addresses are
//! resolved by perf from the record timestamps.
//!
//! # The mmap marker
//!
//! `perf record` only learns about the file because the process maps it
//! executable: the file is mapped `PROT_READ | PROT_EXEC` once, right after it
//! is created, and the mapping is kept for the life of the process. A directory
//! on a `noexec` mount refuses that mapping; the sink then disables itself with
//! one stderr line. The file is `$JITDUMPDIR/jit-<pid>.dump`, defaulting to
//! `/tmp`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

/// `JiTD` as a host-order `u32`.
pub const JITHEADER_MAGIC: u32 = 0x4A69_5444;
/// The only specified version.
pub const JITHEADER_VERSION: u32 = 1;
/// `struct jitheader`: 6 × u32 + 2 × u64.
pub const HEADER_SIZE: usize = 40;
/// `struct jr_prefix`: id u32, total_size u32, timestamp u64.
pub const RECORD_PREFIX_SIZE: usize = 16;
/// `struct jr_code_load` before the name: prefix, pid, tid, vma, code_addr,
/// code_size, code_index.
pub const CODE_LOAD_FIXED_SIZE: usize = RECORD_PREFIX_SIZE + 4 + 4 + 8 * 4;
/// `struct jr_code_unwinding_info` before the data: prefix, unwinding_size,
/// eh_frame_hdr_size, mapped_size.
pub const UNWINDING_INFO_FIXED_SIZE: usize = RECORD_PREFIX_SIZE + 8 * 3;

pub const JIT_CODE_LOAD: u32 = 0;
pub const JIT_CODE_MOVE: u32 = 1;
pub const JIT_CODE_DEBUG_INFO: u32 = 2;
pub const JIT_CODE_CLOSE: u32 = 3;
pub const JIT_CODE_UNWINDING_INFO: u32 = 4;

pub const EM_X86_64: u32 = 62;
pub const EM_AARCH64: u32 = 183;

/// Size of the `.eh_frame_hdr` [`build_x64_eh_frame`] appends.
pub const EH_FRAME_HDR_SIZE: usize = 20;

/// The ELF machine of the running host, or 0 where jitdump is not produced.
pub fn host_elf_machine() -> u32 {
    #[cfg(target_arch = "x86_64")]
    {
        EM_X86_64
    }
    #[cfg(target_arch = "aarch64")]
    {
        EM_AARCH64
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    {
        0
    }
}

fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_ne_bytes());
}

fn put_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_ne_bytes());
}

fn put_prefix(out: &mut Vec<u8>, id: u32, total_size: usize, timestamp: u64) {
    put_u32(out, id);
    // Cast: a record larger than 4 GiB cannot be produced (the code buffer
    // cap is far below that); saturate rather than wrap if it ever were.
    put_u32(out, u32::try_from(total_size).unwrap_or(u32::MAX));
    put_u64(out, timestamp);
}

/// Append the file header.
pub fn encode_header(out: &mut Vec<u8>, elf_mach: u32, pid: u32, timestamp: u64) {
    put_u32(out, JITHEADER_MAGIC);
    put_u32(out, JITHEADER_VERSION);
    put_u32(out, HEADER_SIZE as u32);
    put_u32(out, elf_mach);
    put_u32(out, 0); // pad1
    put_u32(out, pid);
    put_u64(out, timestamp);
    put_u64(out, 0); // flags: no JITDUMP_FLAGS_ARCH_TIMESTAMP, the clock is CLOCK_MONOTONIC
}

/// Total size of the `JIT_CODE_LOAD` record for `name` and `code_len` bytes.
pub fn code_load_size(name: &str, code_len: usize) -> usize {
    CODE_LOAD_FIXED_SIZE + name.len() + 1 + code_len
}

/// Append a `JIT_CODE_LOAD` record. `vma` and `code_addr` are both the code's
/// runtime address: the body is executed where it was written.
#[allow(clippy::too_many_arguments)]
pub fn encode_code_load(
    out: &mut Vec<u8>,
    pid: u32,
    tid: u32,
    timestamp: u64,
    code_index: u64,
    addr: u64,
    code: &[u8],
    name: &str,
) {
    let total = code_load_size(name, code.len());
    out.reserve(total);
    put_prefix(out, JIT_CODE_LOAD, total, timestamp);
    put_u32(out, pid);
    put_u32(out, tid);
    put_u64(out, addr); // vma
    put_u64(out, addr); // code_addr
    put_u64(out, code.len() as u64);
    put_u64(out, code_index);
    // The name is NUL-terminated; an interior NUL would truncate it for the
    // reader and misplace nothing (total_size is explicit), but drop them so
    // the symbol is what was meant.
    out.extend(name.bytes().filter(|&b| b != 0));
    let dropped = name.bytes().filter(|&b| b == 0).count();
    out.resize(out.len() + dropped + 1, 0);
    out.extend_from_slice(code);
}

/// Append a `JIT_CODE_CLOSE` record.
pub fn encode_code_close(out: &mut Vec<u8>, timestamp: u64) {
    put_prefix(out, JIT_CODE_CLOSE, RECORD_PREFIX_SIZE, timestamp);
}

/// Append a `JIT_CODE_UNWINDING_INFO` record. `unwinding` is `.eh_frame`
/// followed by `.eh_frame_hdr` (the last `eh_frame_hdr_size` bytes), which is
/// how `perf inject` splits it. The record is padded to an 8-byte boundary.
pub fn encode_unwinding_info(out: &mut Vec<u8>, timestamp: u64, unwinding: &[u8], eh_frame_hdr_size: usize) {
    let content = UNWINDING_INFO_FIXED_SIZE + unwinding.len();
    let total = align8(content);
    put_prefix(out, JIT_CODE_UNWINDING_INFO, total, timestamp);
    put_u64(out, unwinding.len() as u64); // unwinding_size
    put_u64(out, eh_frame_hdr_size as u64);
    put_u64(out, unwinding.len() as u64); // mapped_size
    out.extend_from_slice(unwinding);
    out.resize(out.len() + (total - content), 0);
}

fn align8(n: usize) -> usize {
    (n + 7) & !7
}

/// Does `code` open with `push rbp; mov rbp, rsp` (`55 48 89 E5`)?
pub fn has_standard_x64_frame_record(code: &[u8]) -> bool {
    code.starts_with(&[0x55, 0x48, 0x89, 0xE5])
}

// DWARF call-frame constants used below.
const DW_CFA_ADVANCE_LOC: u8 = 0x40;
const DW_CFA_OFFSET: u8 = 0x80;
const DW_CFA_DEF_CFA: u8 = 0x0c;
const DW_CFA_DEF_CFA_REGISTER: u8 = 0x0d;
const DW_CFA_DEF_CFA_OFFSET: u8 = 0x0e;
const DW_CFA_NOP: u8 = 0x00;
const DW_EH_PE_UDATA4: u8 = 0x03;
const DW_EH_PE_SDATA4: u8 = 0x0b;
const DW_EH_PE_PCREL: u8 = 0x10;
const DW_EH_PE_DATAREL: u8 = 0x30;
// x86-64 DWARF register numbers.
const DWARF_RBP: u8 = 6;
const DWARF_RSP: u8 = 7;
const DWARF_RA: u8 = 16;

fn pad_to_8_with_nops(buf: &mut Vec<u8>, record_start: usize) {
    while (buf.len() - record_start) % 8 != 0 {
        buf.push(DW_CFA_NOP);
    }
}

fn patch_u32(buf: &mut [u8], at: usize, v: u32) {
    buf[at..at + 4].copy_from_slice(&v.to_ne_bytes());
}

/// `.eh_frame` (CIE, one FDE, terminator) followed by a 20-byte
/// `.eh_frame_hdr`, for one x86-64 body of `code_size` bytes that opens with
/// the standard frame record.
///
/// Addresses are self-relative and assume `perf inject`'s layout: `.text` at
/// some offset `T` (a multiple of 8), `.eh_frame` at `T + align8(code_size)`,
/// `.eh_frame_hdr` directly after `.eh_frame`. This is the layout V8's jitdump
/// writer targets as well.
pub fn build_x64_eh_frame(code_size: usize) -> Vec<u8> {
    let mut buf = Vec::with_capacity(96);

    // --- CIE ---
    let cie_start = buf.len();
    buf.extend_from_slice(&[0; 4]); // length, patched
    buf.extend_from_slice(&0u32.to_ne_bytes()); // CIE id
    buf.push(1); // version
    buf.extend_from_slice(b"zR\0"); // augmentation
    buf.push(1); // code alignment factor (uleb 1)
    buf.push(0x78); // data alignment factor (sleb -8)
    buf.push(DWARF_RA); // return address register
    buf.push(1); // augmentation data length
    buf.push(DW_EH_PE_PCREL | DW_EH_PE_SDATA4); // FDE pointer encoding
    // Initial state: CFA = RSP + 8, RA at CFA - 8.
    buf.extend_from_slice(&[DW_CFA_DEF_CFA, DWARF_RSP, 8]);
    buf.extend_from_slice(&[DW_CFA_OFFSET | DWARF_RA, 1]);
    pad_to_8_with_nops(&mut buf, cie_start);
    let cie_size = buf.len() - cie_start;
    patch_u32(&mut buf, cie_start, (cie_size - 4) as u32);

    // --- FDE ---
    let fde_start = buf.len();
    buf.extend_from_slice(&[0; 4]); // length, patched
    // CIE pointer: distance from this field back to the CIE.
    buf.extend_from_slice(&((fde_start + 4 - cie_start) as u32).to_ne_bytes());
    // pc_begin, pc-relative: the code starts align8(code_size) before
    // `.eh_frame`, and this field is at `.eh_frame + fde_start + 8`.
    let pc_begin = -((align8(code_size) + fde_start + 8) as i64);
    buf.extend_from_slice(&(pc_begin as i32).to_ne_bytes());
    buf.extend_from_slice(&(u32::try_from(code_size).unwrap_or(u32::MAX)).to_ne_bytes()); // pc_range
    buf.push(0); // augmentation data length
    // After `push rbp` (1 byte): CFA = RSP + 16, RBP saved at CFA - 16.
    buf.push(DW_CFA_ADVANCE_LOC | 1);
    buf.extend_from_slice(&[DW_CFA_DEF_CFA_OFFSET, 16]);
    buf.extend_from_slice(&[DW_CFA_OFFSET | DWARF_RBP, 2]);
    // After `mov rbp, rsp` (3 bytes): CFA = RBP + 16.
    buf.push(DW_CFA_ADVANCE_LOC | 3);
    buf.extend_from_slice(&[DW_CFA_DEF_CFA_REGISTER, DWARF_RBP]);
    pad_to_8_with_nops(&mut buf, fde_start);
    let fde_size = buf.len() - fde_start;
    patch_u32(&mut buf, fde_start, (fde_size - 4) as u32);

    // --- terminator ---
    buf.extend_from_slice(&0u32.to_ne_bytes());
    let eh_frame_size = buf.len();

    // --- .eh_frame_hdr ---
    buf.push(1); // version
    buf.push(DW_EH_PE_PCREL | DW_EH_PE_SDATA4); // eh_frame_ptr encoding
    buf.push(DW_EH_PE_UDATA4); // fde_count encoding
    buf.push(DW_EH_PE_DATAREL | DW_EH_PE_SDATA4); // table encoding
    // eh_frame_ptr, relative to this field (at hdr + 4).
    buf.extend_from_slice(&(-((eh_frame_size + 4) as i64) as i32).to_ne_bytes());
    buf.extend_from_slice(&1u32.to_ne_bytes()); // fde_count
    // Table entry, both relative to the start of `.eh_frame_hdr`.
    let initial_loc = -((align8(code_size) + eh_frame_size) as i64);
    buf.extend_from_slice(&(initial_loc as i32).to_ne_bytes());
    let fde_addr = -((eh_frame_size - fde_start) as i64);
    buf.extend_from_slice(&(fde_addr as i32).to_ne_bytes());
    debug_assert_eq!(buf.len() - eh_frame_size, EH_FRAME_HDR_SIZE);
    buf
}

// ---------------------------------------------------------------------------
// The sink
// ---------------------------------------------------------------------------

static FAILED: AtomicBool = AtomicBool::new(false);

/// `CRATONVM_JIT_JITDUMP`, read once.
fn requested() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        crate::code_events::flag_is_on(cratonvm_types::flags::runtime_var("CRATONVM_JIT_JITDUMP"))
    })
}

fn fail(what: std::fmt::Arguments<'_>) {
    if !FAILED.swap(true, Ordering::SeqCst) {
        use std::io::Write;
        let _ = writeln!(std::io::stderr(), "[cratonvm] jitdump disabled: {what}");
    }
}

/// Whether `JIT_CODE_LOAD` records should be written.
pub fn enabled() -> bool {
    if !requested() {
        return false;
    }
    #[cfg(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    {
        !FAILED.load(Ordering::Relaxed)
    }
    #[cfg(not(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    )))]
    {
        // Say so once rather than silently producing nothing.
        fail(format_args!("jitdump is only produced on Linux x86-64/aarch64"));
        false
    }
}

/// Record a published region. No-op unless enabled.
pub fn record_load(start: usize, len: usize, name: &str) {
    if !enabled() {
        return;
    }
    #[cfg(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    linux::record_load(start, len, name);
}

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
mod linux {
    use super::*;
    use std::io::Write;
    use std::os::unix::io::AsRawFd;
    use std::sync::{Mutex, PoisonError};

    struct DumpFile {
        file: std::fs::File,
        next_code_index: u64,
        scratch: Vec<u8>,
    }

    static SINK: OnceLock<Mutex<Option<DumpFile>>> = OnceLock::new();

    #[repr(C)]
    struct Timespec {
        tv_sec: i64,
        tv_nsec: i64,
    }

    unsafe extern "C" {
        fn clock_gettime(clock_id: i32, tp: *mut Timespec) -> i32;
        fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, offset: i64) -> *mut u8;
        fn syscall(num: i64, ...) -> i64;
        fn atexit(cb: extern "C" fn()) -> i32;
    }

    const CLOCK_MONOTONIC: i32 = 1;
    const PROT_READ: i32 = 1;
    const PROT_EXEC: i32 = 4;
    const MAP_PRIVATE: i32 = 0x02;
    #[cfg(target_arch = "x86_64")]
    const SYS_GETTID: i64 = 186;
    #[cfg(target_arch = "aarch64")]
    const SYS_GETTID: i64 = 178;

    fn monotonic_ns() -> u64 {
        let mut ts = Timespec { tv_sec: 0, tv_nsec: 0 };
        // SAFETY: `ts` is a live, writable `struct timespec` (two `long`s on
        // LP64) and the clock id is a kernel constant.
        let rc = unsafe { clock_gettime(CLOCK_MONOTONIC, &mut ts) };
        if rc != 0 {
            return 0;
        }
        (ts.tv_sec as u64)
            .saturating_mul(1_000_000_000)
            .saturating_add(ts.tv_nsec as u64)
    }

    fn current_tid() -> u32 {
        // SAFETY: `gettid` takes no arguments and cannot fail.
        let tid = unsafe { syscall(SYS_GETTID) };
        u32::try_from(tid).unwrap_or(std::process::id())
    }

    fn dump_path(pid: u32) -> std::path::PathBuf {
        let dir = std::env::var_os("JITDUMPDIR")
            .filter(|d| !d.is_empty())
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from("/tmp"));
        dir.join(format!("jit-{pid}.dump"))
    }

    fn open() -> Option<DumpFile> {
        let pid = std::process::id();
        let path = dump_path(pid);
        // Read access is required: a PROT_READ mapping of a write-only fd fails.
        let file = match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
        {
            Ok(f) => f,
            Err(e) => {
                fail(format_args!("cannot open {}: {e}", path.display()));
                return None;
            }
        };
        // The marker perf record looks for. Never unmapped and never touched:
        // the mapping only has to exist so the kernel reports it.
        // SAFETY: a null hint without MAP_FIXED lets the kernel choose; the fd
        // is open for reading; the result is only compared, never dereferenced.
        let marker = unsafe {
            mmap(
                std::ptr::null_mut(),
                4096,
                PROT_READ | PROT_EXEC,
                MAP_PRIVATE,
                file.as_raw_fd(),
                0,
            )
        };
        if marker as usize == usize::MAX || marker.is_null() {
            fail(format_args!(
                "cannot map {} PROT_EXEC ({}); perf would not find it -- set JITDUMPDIR \
                 to a directory that is not mounted noexec",
                path.display(),
                std::io::Error::last_os_error()
            ));
            return None;
        }
        let mut dump = DumpFile {
            file,
            next_code_index: 0,
            scratch: Vec::with_capacity(4096),
        };
        encode_header(&mut dump.scratch, host_elf_machine(), pid, monotonic_ns());
        if let Err(e) = dump.file.write_all(&dump.scratch) {
            fail(format_args!("write to {} failed: {e}", path.display()));
            return None;
        }
        // SAFETY: `close_at_exit` is an `extern "C" fn()` with no captured
        // state; registering it has no precondition.
        unsafe {
            atexit(close_at_exit);
        }
        Some(dump)
    }

    pub(super) fn record_load(start: usize, len: usize, name: &str) {
        let lock = SINK.get_or_init(|| Mutex::new(open()));
        let mut guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(dump) = guard.as_mut() else {
            return;
        };
        // SAFETY: `code_events::publish`'s contract — the region is mapped and
        // readable for the duration of this call.
        let code = unsafe { std::slice::from_raw_parts(start as *const u8, len) };
        let timestamp = monotonic_ns();
        let mut scratch = std::mem::take(&mut dump.scratch);
        scratch.clear();
        #[cfg(target_arch = "x86_64")]
        {
            if has_standard_x64_frame_record(code) {
                let unwinding = build_x64_eh_frame(len);
                encode_unwinding_info(&mut scratch, timestamp, &unwinding, EH_FRAME_HDR_SIZE);
            }
        }
        encode_code_load(
            &mut scratch,
            std::process::id(),
            current_tid(),
            timestamp,
            dump.next_code_index,
            start as u64,
            code,
            name,
        );
        dump.next_code_index += 1;
        let result = dump.file.write_all(&scratch);
        // Keep the allocation, but not a huge one from a huge body.
        if scratch.capacity() <= 1 << 20 {
            dump.scratch = scratch;
        }
        if let Err(e) = result {
            *guard = None;
            fail(format_args!("write failed: {e}"));
        }
    }

    extern "C" fn close_at_exit() {
        let Some(lock) = SINK.get() else {
            return;
        };
        // `try_lock`: a thread still running at exit may hold the lock, and
        // waiting for it here could hang the exit.
        let Ok(mut guard) = lock.try_lock() else {
            return;
        };
        if let Some(mut dump) = guard.take() {
            let mut rec = Vec::with_capacity(RECORD_PREFIX_SIZE);
            encode_code_close(&mut rec, monotonic_ns());
            let _ = dump.file.write_all(&rec);
            let _ = dump.file.flush();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u32_at(b: &[u8], at: usize) -> u32 {
        u32::from_ne_bytes(b[at..at + 4].try_into().unwrap())
    }
    fn i32_at(b: &[u8], at: usize) -> i32 {
        i32::from_ne_bytes(b[at..at + 4].try_into().unwrap())
    }
    fn u64_at(b: &[u8], at: usize) -> u64 {
        u64::from_ne_bytes(b[at..at + 8].try_into().unwrap())
    }

    #[test]
    fn header_is_forty_bytes_with_magic_version_and_machine() {
        let mut out = Vec::new();
        encode_header(&mut out, EM_X86_64, 1234, 99);
        assert_eq!(out.len(), HEADER_SIZE);
        assert_eq!(u32_at(&out, 0), JITHEADER_MAGIC);
        assert_eq!(&out[0..4], &0x4A69_5444u32.to_ne_bytes());
        assert_eq!(u32_at(&out, 4), 1);
        assert_eq!(u32_at(&out, 8), 40);
        assert_eq!(u32_at(&out, 12), 62);
        assert_eq!(u32_at(&out, 20), 1234);
        assert_eq!(u64_at(&out, 24), 99);
        assert_eq!(u64_at(&out, 32), 0);
    }

    #[test]
    fn code_load_record_layout() {
        let code = [0x55u8, 0x48, 0x89, 0xE5, 0xC3];
        let name = "a/B.m()V [c1]";
        let mut out = Vec::new();
        encode_code_load(&mut out, 7, 8, 42, 3, 0x1000, &code, name);
        assert_eq!(CODE_LOAD_FIXED_SIZE, 56);
        assert_eq!(out.len(), 56 + name.len() + 1 + code.len());
        assert_eq!(u32_at(&out, 0), JIT_CODE_LOAD);
        assert_eq!(u32_at(&out, 4) as usize, out.len());
        assert_eq!(u64_at(&out, 8), 42);
        assert_eq!(u32_at(&out, 16), 7);
        assert_eq!(u32_at(&out, 20), 8);
        assert_eq!(u64_at(&out, 24), 0x1000); // vma
        assert_eq!(u64_at(&out, 32), 0x1000); // code_addr
        assert_eq!(u64_at(&out, 40), code.len() as u64);
        assert_eq!(u64_at(&out, 48), 3);
        assert_eq!(&out[56..56 + name.len()], name.as_bytes());
        assert_eq!(out[56 + name.len()], 0);
        assert_eq!(&out[57 + name.len()..], &code);
    }

    #[test]
    fn interior_nul_in_a_name_does_not_change_the_record_size() {
        let mut a = Vec::new();
        encode_code_load(&mut a, 1, 1, 1, 0, 0x10, &[0xC3], "ab\0c");
        assert_eq!(a.len(), code_load_size("ab\0c", 1));
        assert_eq!(u32_at(&a, 4) as usize, a.len());
    }

    #[test]
    fn close_record_is_a_bare_prefix() {
        let mut out = Vec::new();
        encode_code_close(&mut out, 5);
        assert_eq!(out.len(), RECORD_PREFIX_SIZE);
        assert_eq!(u32_at(&out, 0), JIT_CODE_CLOSE);
        assert_eq!(u32_at(&out, 4), 16);
        assert_eq!(u64_at(&out, 8), 5);
    }

    #[test]
    fn unwinding_record_is_padded_to_eight_and_sizes_agree() {
        for code_size in [1usize, 7, 8, 9, 4096, 12345] {
            let unwinding = build_x64_eh_frame(code_size);
            let mut out = Vec::new();
            encode_unwinding_info(&mut out, 1, &unwinding, EH_FRAME_HDR_SIZE);
            assert_eq!(out.len() % 8, 0);
            assert_eq!(u32_at(&out, 0), JIT_CODE_UNWINDING_INFO);
            assert_eq!(u32_at(&out, 4) as usize, out.len());
            assert_eq!(u64_at(&out, 16) as usize, unwinding.len());
            assert_eq!(u64_at(&out, 24) as usize, EH_FRAME_HDR_SIZE);
            assert_eq!(u64_at(&out, 32) as usize, unwinding.len());
            assert_eq!(&out[40..40 + unwinding.len()], &unwinding[..]);
        }
    }

    #[test]
    fn eh_frame_offsets_resolve_to_the_code_and_the_fde() {
        let code_size = 13usize; // align8 → 16
        let buf = build_x64_eh_frame(code_size);
        let eh_frame_size = buf.len() - EH_FRAME_HDR_SIZE;
        // CIE and FDE records are each 8-byte aligned, then a 4-byte terminator.
        let cie_len = u32_at(&buf, 0) as usize + 4;
        assert_eq!(cie_len % 8, 0);
        assert_eq!(u32_at(&buf, 4), 0, "CIE id");
        let fde_start = cie_len;
        let fde_len = u32_at(&buf, fde_start) as usize + 4;
        assert_eq!(fde_len % 8, 0);
        assert_eq!(u32_at(&buf, fde_start + 4) as usize, fde_start + 4, "CIE pointer");
        assert_eq!(fde_start + fde_len + 4, eh_frame_size);
        assert_eq!(u32_at(&buf, eh_frame_size - 4), 0, "terminator");

        // Place .eh_frame at E = T + 16 for text base T; the code must be at T.
        let text: i64 = 0x80;
        let e = text + 16;
        let pc_begin_field = e + fde_start as i64 + 8;
        assert_eq!(pc_begin_field + i32_at(&buf, fde_start + 8) as i64, text);
        assert_eq!(u32_at(&buf, fde_start + 12) as usize, code_size);

        let hdr = eh_frame_size;
        let hdr_addr = e + hdr as i64;
        assert_eq!(buf[hdr], 1);
        assert_eq!(hdr_addr + 4 + i32_at(&buf, hdr + 4) as i64, e, "eh_frame_ptr");
        assert_eq!(u32_at(&buf, hdr + 8), 1, "fde_count");
        assert_eq!(hdr_addr + i32_at(&buf, hdr + 12) as i64, text, "initial_loc");
        assert_eq!(
            hdr_addr + i32_at(&buf, hdr + 16) as i64,
            e + fde_start as i64,
            "fde address"
        );
    }

    #[test]
    fn the_frame_record_check_reads_the_bytes() {
        assert!(has_standard_x64_frame_record(&[0x55, 0x48, 0x89, 0xE5, 0x48]));
        assert!(!has_standard_x64_frame_record(&[0x48, 0x89, 0xE5]));
        assert!(!has_standard_x64_frame_record(&[0x55]));
    }
}
