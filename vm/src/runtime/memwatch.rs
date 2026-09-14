// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Targeted absolute-address memory watch for the bc-math-ec `0x4` corruption
//! (session 2026-06-09).
//!
//! Measured fact: the corrupted young-arena address REPEATS across runs
//! (`ECCurve$Fp`/`ECCurve$F2m` @0x20a00fb8 fld[4] in two different runs), so a
//! single absolute payload address can be watched. Unlike the whole-young
//! `youngscan` (O(heap) per native, races the SEGV), this is **O(1) per poll**:
//! one volatile 8-byte read + compare, so it can run at full interpreter
//! safepoint frequency and at every native return.
//!
//! Gate: `CRATONVM_DBG_MEMWATCH=<hex addr>` (with or without `0x`), the
//! ABSOLUTE address of the watched 8 bytes — for a field cell's payload use
//! `obj + 40 + fld*16 + 8`. The first poll memory-maps-validates the address
//! via `VirtualQuery` (a wrong-layout run would otherwise fault on the read)
//! and disarms permanently if unmapped. A change is REPORTED only when the new
//! value is small-nonzero (`0 < v < 0x1000`, the corruption signature — e.g.
//! 4 = the `Value::Object` discriminant landing mis-gridded on a payload);
//! ordinary pointer/zero writes to the (constantly reused) slot stay silent.

use std::sync::atomic::{AtomicI8, AtomicU64, AtomicUsize, Ordering};
use std::sync::OnceLock;

/// Parsed watch address (0 = gate off / parse failure).
fn addr() -> usize {
    static A: OnceLock<usize> = OnceLock::new();
    *A.get_or_init(|| {
        cratonvm_types::flags::runtime_var("CRATONVM_DBG_MEMWATCH")
            .ok()
            .and_then(|v| {
                let s = v.trim().trim_start_matches("0x").trim_start_matches("0X");
                usize::from_str_radix(s, 16).ok()
            })
            .unwrap_or(0)
    })
}

/// Is a watch address configured at all?
///
/// [`addr`] is memoized from `CRATONVM_DBG_MEMWATCH` and never changes after
/// the first read, so this is a startup-static gate even though `ARMED` below
/// is not. ARCH-2026-08-04 A3: `safe_native_call_impl` folds this into its
/// diagnostic mask so an unconfigured run does not reach [`poll_with`] on
/// every native return.
#[inline]
pub(crate) fn is_watching() -> bool {
    addr() != 0
}

/// -1 = unprobed, 1 = armed (address readable), 0 = disarmed (unmapped).
static ARMED: AtomicI8 = AtomicI8::new(-1);
/// Last observed value (u64::MAX sentinel = no observation yet).
static LAST: AtomicU64 = AtomicU64::new(u64::MAX);
/// Reports emitted (capped so a pathological run can't flood stderr).
static REPORTS: AtomicUsize = AtomicUsize::new(0);
const REPORT_CAP: usize = 12;

#[cfg(windows)]
fn probe_readable(a: usize) -> bool {
    // Minimal VirtualQuery binding — confirm the page is committed+readable.
    //
    // This declaration MUST stay byte-for-byte structurally identical to the
    // one in `crash_handler::windows_fault` (same field types/order, same
    // pointer types, same explicit `_pad`). Both declare the same `VirtualQuery`
    // symbol, so Rust's `clashing_extern_declarations` lint compares the two and
    // warns if they diverge. The explicit `_pad: u16` is the real Win32
    // MEMORY_BASIC_INFORMATION alignment slot before the 8-aligned `region_size`
    // (the compiler inserts it implicitly either way; making it explicit here
    // keeps the two declarations structurally equal so the lint stays quiet and
    // the layouts can never silently drift apart).
    #[repr(C)]
    struct MemoryBasicInformation {
        base_address: *mut core::ffi::c_void,
        allocation_base: *mut core::ffi::c_void,
        allocation_protect: u32,
        partition_id: u16,
        _pad: u16,
        region_size: usize,
        state: u32,
        protect: u32,
        type_: u32,
    }
    extern "system" {
        fn VirtualQuery(
            lp_address: *const core::ffi::c_void,
            lp_buffer: *mut MemoryBasicInformation,
            dw_length: usize,
        ) -> usize;
    }
    const MEM_COMMIT: u32 = 0x1000;
    const PAGE_NOACCESS: u32 = 0x01;
    const PAGE_GUARD: u32 = 0x100;
    let mut mbi = std::mem::MaybeUninit::<MemoryBasicInformation>::uninit();
    // SAFETY: querying arbitrary address metadata is always safe; the struct
    // is plain-old-data filled by the kernel on success.
    let n = unsafe {
        VirtualQuery(
            a as *const core::ffi::c_void,
            mbi.as_mut_ptr(),
            std::mem::size_of::<MemoryBasicInformation>(),
        )
    };
    if n == 0 {
        return false;
    }
    // SAFETY: VirtualQuery returned non-zero, so the buffer is initialized.
    let mbi = unsafe { mbi.assume_init() };
    mbi.state == MEM_COMMIT && mbi.protect & PAGE_NOACCESS == 0 && mbi.protect & PAGE_GUARD == 0
}

#[cfg(not(windows))]
fn probe_readable(_a: usize) -> bool {
    false // posix: not needed for this Windows-box hunt
}

/// Poll the watched address with a fixed site label. See [`poll_with`].
#[inline]
pub fn poll(site: &'static str, java_stack: impl FnOnce() -> String) {
    poll_with(|| site.to_string(), java_stack);
}

/// Poll the watched address. `site` (lazily built — only evaluated on a hit
/// or arming) labels the call point (e.g. "safepoint", "native:<name>",
/// "post-gc"); `java_stack` supplies the frames to print on a hit (top
/// first), already formatted one per line.
#[inline]
pub fn poll_with(site: impl FnOnce() -> String, java_stack: impl FnOnce() -> String) {
    let a = addr();
    if a == 0 {
        return;
    }
    match ARMED.load(Ordering::Relaxed) {
        0 => return,
        -1 => {
            let ok = probe_readable(a) && probe_readable(a + 7);
            ARMED.store(if ok { 1 } else { 0 }, Ordering::Relaxed);
            if !ok {
                eprintln!(
                    "[memwatch] 0x{a:x} not committed/readable this run — disarmed \
                     (arena layout differs); re-run."
                );
                return;
            }
            eprintln!("[memwatch] armed on 0x{a:x} (site={})", site());
            // First observation happens on the next poll (LAST is still the
            // sentinel); returning here also keeps `site` single-use for the
            // borrow checker.
            return;
        }
        _ => {}
    }
    // SAFETY: probe_readable confirmed the 8 bytes are committed+readable;
    // the managed arenas stay mapped for the process lifetime.
    let now = unsafe { std::ptr::read_volatile(a as *const u64) };
    let last = LAST.swap(now, Ordering::Relaxed);
    if last == now || last == u64::MAX {
        return;
    }
    // Report only the corruption signature: small non-zero new value.
    if now != 0 && now < 0x1000 {
        let k = REPORTS.fetch_add(1, Ordering::Relaxed);
        if k < REPORT_CAP {
            // Disc word of the owning cell (addr-8) for context.
            // SAFETY: cell disc lives in the same committed arena page range.
            let disc = unsafe { std::ptr::read_volatile((a - 8) as *const u64) };
            eprintln!(
                "[memwatch] #{k} HIT @0x{a:x}: 0x{last:x} -> 0x{now:x} (cell disc=0x{disc:x}) at {}",
                site()
            );
            eprintln!("[memwatch] Java stack (top first):\n{}", java_stack());
        }
    }
}
