// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! The page form of the compiled safepoint poll (t1 wave 8, lane SAFEPOLL;
//! `CRATONVM_JIT_PAGE_POLL`: default ON on Linux x86-64 and Windows x86-64
//! since j27 2026-10-28 (`=0` kills); book
//! `docs/feature-designs/generative-gc-architecture.md`).
//!
//! # Shape
//!
//! The byte poll is `TEST byte [rip+flag], 0xFF ; JZ .done` (plus, under
//! `CRATONVM_JIT_POLL_THREAD_WORD`, a TLS load, a null test and a second byte
//! test). The page form is two instructions and no conditional branch:
//!
//! ```text
//! MOV  R11, <seg>:[cell_disp]        ; this OS thread's poll cell: 0 or ARMED_OFFSET
//! TEST [R11 + pair_base], EAX         ; the poll site: reads the VM's stop page,
//!                                     ; or its armed page when the cell is armed
//! JMP  .done
//! .slow:                              ; reached only through the fault
//!   <the byte poll's slow path, unchanged>
//! .done:
//! ```
//!
//! `pair_base` is a disp32, so every VM maps its page pair in the low 2 GiB
//! ([`map_pair`]). The pair is two pages:
//!
//! * the STOP page (`pair_base`): readable, protected `PAGE_NOACCESS` /
//!   `PROT_NONE` for the duration of that VM's stop-the-world pause
//!   ([`set_stop`], called by the barrier's raise / lower pair) -- one
//!   protection change per pause, the pre-JEP-312 HotSpot polling page;
//! * the ARMED page (`pair_base + ARMED_OFFSET`): never readable. A thread
//!   whose cell holds [`ARMED_OFFSET`] faults at its next poll of ANY VM's
//!   code: that is the per-thread arm a handshake requester sets
//!   ([`store_cell`]) -- one store, no syscall, no TLB shootdown.
//!
//! A thread whose cell was never written reads `0` (the TLS slot's initial
//! value), i.e. the stop page: every thread is reached by a pause whether or
//! not it ever registered a loop poll word, which is what the byte poll's
//! baked-flag fallback guaranteed.
//!
//! # The fault
//!
//! The poll site and its slow-path label are registered in the implicit-null
//! table (`crate::implicit_null`), whose two fault handlers already redirect an
//! exact registered PC; `implicit_null::recover` accepts a fault address inside
//! a registered pair ([`is_poll_page_addr`]) besides the null page. So no new
//! handler and no new table: the single-pass driver's all-or-nothing site
//! registration, the unroller's per-copy shifting and the mapping-wide
//! retirement apply to poll sites unchanged.
//!
//! # Process state
//!
//! The pair registry is process-wide because the fault handler is: it maps an
//! address to "a poll page", nothing per VM. Each VM owns its own pair (its
//! barrier maps it), so one VM's pause never stops another VM's threads.
//! Pairs are never unmapped (bounded by the number of VMs a process creates,
//! like the barrier's code-adjacent flag cell).

use std::sync::atomic::{AtomicUsize, Ordering};

/// One page.
pub const PAGE: usize = 4096;
/// The armed page's offset from the pair base, and the value an armed cell
/// holds.
pub const ARMED_OFFSET: usize = PAGE;
/// Forced mode-exit offset: bit 13 (`0x2000`) of the baked disp32. Setting
/// byte 1 `|= 0x20` forces the poll's read onto this protected page even while
/// the cell holds `0`.
pub const FORCED_EXIT_OFFSET: usize = 2 * PAGE;
/// Required alignment of the poll base so `base | FORCED_EXIT_OFFSET == base + FORCED_EXIT_OFFSET`.
pub const PAIR_ALIGN: usize = 4 * PAGE;
/// The whole pair (4 pages: readable base, NOACCESS armed, NOACCESS forced exit, NOACCESS both).
pub const PAIR_BYTES: usize = 4 * PAGE;
/// Pairs the fault handler can recognise (one per VM).
const MAX_PAIRS: usize = 64;
/// The highest end address a pair may have: the poll bakes `pair_base` as a
/// sign-extended disp32 and reads up to `pair_base + PAIR_BYTES`.
const LOW_LIMIT: usize = 0x7FFF_0000;

/// Registered pair bases (`0`: free). Written once per VM, read by the fault
/// handler (async-signal-safe: atomic loads only).
static PAIR_BASES: [AtomicUsize; MAX_PAIRS] = [const { AtomicUsize::new(0) }; MAX_PAIRS];

/// `CRATONVM_JIT_PAGE_POLL` is on (unless `=0`, Linux and Windows x86-64)
/// AND the per-OS-thread cell's TLS slot was found. Read once per process through [`cell_tls_disp`]'s cache.
pub fn enabled() -> bool {
    cell_tls_disp() != 0
}

/// May a compile emit the page form now? [`enabled`], plus the implicit-null
/// table active (a recovering fault handler installed and room left), because
/// the poll site is registered there and an unregistered site turns a pause
/// into a process crash.
pub fn emit_enabled() -> bool {
    enabled() && crate::implicit_null::active()
}

/// Is `addr` inside a registered pair? Async-signal-safe.
pub fn is_poll_page_addr(addr: usize) -> bool {
    for slot in &PAIR_BASES {
        let base = slot.load(Ordering::Acquire);
        if base != 0 && addr.wrapping_sub(base) < PAIR_BYTES {
            return true;
        }
    }
    false
}

/// Can `base` be baked as the poll's disp32?
pub fn base_fits_disp32(base: usize) -> bool {
    base != 0
        && base % PAIR_ALIGN == 0
        && base.checked_add(PAIR_BYTES).is_some_and(|e| e <= LOW_LIMIT)
}

fn register_pair(base: usize) -> bool {
    for slot in &PAIR_BASES {
        if slot
            .compare_exchange(0, base, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            return true;
        }
    }
    false
}

/// Map a pair in the low 2 GiB (stop page readable, armed page not) and
/// register it with the fault handler; `None` when the switch is off, the OS
/// gives no low mapping, or the registry is full. The caller (one per VM)
/// keeps the base for its life and bakes it into every page-form poll.
pub fn map_pair() -> Option<usize> {
    if !enabled() {
        return None;
    }
    map_pair_for_measurement()
}

/// [`map_pair`] whatever the switch says: map a low pair and register it with
/// the fault handler's address test. For the fault round-trip measurement the
/// book asks for before any further emitter work (t1 wave 11, lane GCA:
/// `crash_handler`'s ignored `t1w11_gca_poll_fault_round_trip` test). A
/// registered pair only widens `implicit_null::recover`'s address test by its
/// own two pages, which nothing but a registered poll site touches, so an
/// extra pair is inert. Never called by the VM.
#[doc(hidden)]
pub fn map_pair_for_measurement() -> Option<usize> {
    let base = os::map_low_pair()?;
    if !base_fits_disp32(base) || !register_pair(base) {
        os::unmap_pair(base);
        return None;
    }
    Some(base)
}

/// Protect (`stop = true`) or unprotect the stop page of the pair at `base`.
/// `false` when the OS refused: the caller's byte flag is still authoritative
/// for every byte-form poll, so a refused protect costs page-form polls their
/// pause reach -- the barrier reports it.
pub fn set_stop(base: usize, stop: bool) -> bool {
    if base == 0 {
        return false;
    }
    os::protect_page(base, !stop)
}

/// Store `value` (`0` or [`ARMED_OFFSET`]) into the poll cell at `cell_addr`
/// (an address [`this_threads_cell_addr`] answered on its owning thread, still
/// alive). `SeqCst`: the arm protocol on the VM side re-reads its word after
/// the store.
///
/// # Safety
///
/// `cell_addr` must be a live poll cell: its owning OS thread must not have
/// finished running thread-local destructors.
pub unsafe fn store_cell(cell_addr: usize, value: usize) {
    if cell_addr == 0 {
        return;
    }
    // SAFETY: the caller guarantees a live, 8-aligned, 8-byte TLS cell, which
    // only atomics write and compiled code reads with a plain load.
    unsafe { (*(cell_addr as *const AtomicUsize)).store(value, Ordering::SeqCst) };
}

/// The calling thread's cell value, `None` with the feature off (tests only).
#[cfg(test)]
pub(crate) fn this_threads_cell_value() -> Option<usize> {
    let addr = this_threads_cell_addr();
    if addr == 0 {
        return None;
    }
    // SAFETY: the calling thread's own live cell.
    Some(unsafe { (*(addr as *const AtomicUsize)).load(Ordering::SeqCst) })
}

// ── The per-OS-thread cell ─────────────────────────────────────────────

/// Windows: a TEB TLS slot (`TlsAlloc`), found among the 64 inline slots by a
/// sentinel probe, exactly as `x64::loop_poll_word_tls_disp` finds its own.
/// `0` when the switch is off or no inline slot was found.
#[cfg(all(windows, target_arch = "x86_64"))]
pub fn cell_tls_disp() -> usize {
    use std::sync::OnceLock;
    static DISP: OnceLock<usize> = OnceLock::new();
    *DISP.get_or_init(|| {
        // WARNING j27-user-flip-20261028 (default ON; =0 restores the old default)
        #[link(name = "kernel32")]
        extern "system" {
            fn TlsAlloc() -> u32;
            fn TlsSetValue(idx: u32, val: *mut core::ffi::c_void) -> i32;
        }
        const TLS_OUT_OF_INDEXES: u32 = 0xFFFF_FFFF;
        const TEB_TLS_SLOTS_OFF: usize = 0x1480;
        // SAFETY: plain kernel32 calls, and every gs read indexes this
        // thread's TEB TLS slot array (64 slots from `TEB_TLS_SLOTS_OFF`),
        // mapped for the life of the thread.
        unsafe {
            let slot = TlsAlloc();
            if slot == TLS_OUT_OF_INDEXES {
                return 0;
            }
            let sentinel: usize = 0x5047_504F_4C4C_0000 | (slot as usize & 0xFFFF);
            if TlsSetValue(slot, sentinel as *mut core::ffi::c_void) == 0 {
                return 0;
            }
            let mut found = 0;
            let mut d = TEB_TLS_SLOTS_OFF;
            let end = TEB_TLS_SLOTS_OFF + 64 * 8;
            while d < end {
                if read_gs_qword(d) == sentinel {
                    found = d;
                    break;
                }
                d += 8;
            }
            TlsSetValue(slot, core::ptr::null_mut());
            found
        }
    })
}

/// Windows: the calling thread's cell, `TEB + disp` (`gs:[0x30]` is the TEB's
/// self pointer). Valid until the thread terminates, which is after its
/// thread-local destructors ran.
#[cfg(all(windows, target_arch = "x86_64"))]
pub fn this_threads_cell_addr() -> usize {
    let disp = cell_tls_disp();
    if disp == 0 {
        return 0;
    }
    // SAFETY: `gs:[0x30]` is the running thread's TEB self pointer on x64.
    let teb = unsafe { read_gs_qword(0x30) };
    teb + disp
}

#[cfg(all(windows, target_arch = "x86_64"))]
#[inline]
unsafe fn read_gs_qword(disp: usize) -> usize {
    let val: usize;
    // SAFETY: the caller names an offset inside the running thread's TEB.
    unsafe {
        core::arch::asm!(
            "mov {out}, qword ptr gs:[{addr}]",
            addr = in(reg) disp,
            out = lateout(reg) val,
            options(nostack, readonly, preserves_flags)
        );
    }
    val
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
thread_local! {
    /// The calling OS thread's poll cell: `0` or [`ARMED_OFFSET`]. Const and
    /// drop-free, so it lives in the static TLS block at a fixed `fs:` offset
    /// for the life of the thread.
    static LINUX_POLL_CELL: AtomicUsize = const { AtomicUsize::new(0) };
}

/// Linux x86-64: the cell's `fs:` displacement, proved on this thread and a
/// fresh one (`cratonvm_types::fs_tls_proof`). `0` when the switch is off or
/// the proof failed.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub fn cell_tls_disp() -> usize {
    use std::sync::OnceLock;
    static DISP: OnceLock<usize> = OnceLock::new();
    *DISP.get_or_init(|| {
        // WARNING j27-user-flip-20261028 (default ON; =0 restores the old default)
        cratonvm_types::fs_tls_proof::prove_fs_disp(
            || {
                LINUX_POLL_CELL
                    .try_with(|c| c as *const AtomicUsize as usize)
                    .ok()
            },
            0x5047_504F_4C4C_4C58,
        )
        // Cast: the emitter bakes the negative offset as a sign-extended disp32.
        .map_or(0, |d| (d as u32) as usize)
    })
}

/// Linux x86-64: the calling thread's cell address.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub fn this_threads_cell_addr() -> usize {
    if cell_tls_disp() == 0 {
        return 0;
    }
    LINUX_POLL_CELL
        .try_with(|c| c as *const AtomicUsize as usize)
        .unwrap_or(0)
}

#[cfg(not(any(
    all(windows, target_arch = "x86_64"),
    all(target_os = "linux", target_arch = "x86_64")
)))]
pub fn cell_tls_disp() -> usize {
    0
}

#[cfg(not(any(
    all(windows, target_arch = "x86_64"),
    all(target_os = "linux", target_arch = "x86_64")
)))]
pub fn this_threads_cell_addr() -> usize {
    0
}

// ── OS mapping ─────────────────────────────────────────────────────────

#[cfg(windows)]
mod os {
    use super::{LOW_LIMIT, PAGE, PAIR_BYTES};

    const MEM_COMMIT: u32 = 0x1000;
    const MEM_RESERVE: u32 = 0x2000;
    const MEM_RELEASE: u32 = 0x8000;
    const PAGE_NOACCESS: u32 = 0x01;
    const PAGE_READONLY: u32 = 0x02;
    /// Windows' allocation granularity: a reservation's base is a multiple.
    const GRANULE: usize = 0x1_0000;

    #[link(name = "kernel32")]
    extern "system" {
        fn VirtualAlloc(addr: *mut u8, size: usize, ty: u32, protect: u32) -> *mut u8;
        fn VirtualFree(addr: *mut u8, size: usize, ty: u32) -> i32;
        fn VirtualProtect(addr: *mut u8, size: usize, protect: u32, old: *mut u32) -> i32;
    }

    /// Try granule-aligned addresses downward from just under 2 GiB; the low
    /// half of a 64-bit process is normally sparse (images load above 4 GiB).
    pub(super) fn map_low_pair() -> Option<usize> {
        let mut hint = (LOW_LIMIT - PAIR_BYTES) & !(GRANULE - 1);
        let floor = 0x1000_0000usize;
        let mut tries = 0;
        while hint >= floor && tries < 4096 {
            // SAFETY: a fresh reservation at a hint; a taken range answers null.
            let p = unsafe {
                VirtualAlloc(
                    hint as *mut u8,
                    PAIR_BYTES,
                    MEM_RESERVE | MEM_COMMIT,
                    PAGE_READONLY,
                )
            };
            if !p.is_null() {
                let base = p as usize;
                let mut old = 0u32;
                // SAFETY: the guard pages [PAGE..PAIR_BYTES) of the reservation just committed.
                let ok = unsafe {
                    VirtualProtect(
                        (base + PAGE) as *mut u8,
                        PAIR_BYTES - PAGE,
                        PAGE_NOACCESS,
                        &mut old,
                    )
                } != 0;
                if ok && base == hint {
                    return Some(base);
                }
                unmap_pair(base);
            }
            hint -= GRANULE;
            tries += 1;
        }
        None
    }

    pub(super) fn unmap_pair(base: usize) {
        // SAFETY: `base` is a reservation this module made.
        unsafe {
            VirtualFree(base as *mut u8, 0, MEM_RELEASE);
        }
    }

    pub(super) fn protect_page(base: usize, readable: bool) -> bool {
        let mut old = 0u32;
        let prot = if readable {
            PAGE_READONLY
        } else {
            PAGE_NOACCESS
        };
        // SAFETY: the stop page of a pair this module mapped and never unmaps.
        unsafe { VirtualProtect(base as *mut u8, PAGE, prot, &mut old) != 0 }
    }
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
mod os {
    use super::{PAGE, PAIR_ALIGN, PAIR_BYTES};

    const PROT_NONE: i32 = 0;
    const PROT_READ: i32 = 1;
    const MAP_PRIVATE: i32 = 0x02;
    const MAP_ANONYMOUS: i32 = 0x20;
    /// x86-64 Linux: place the mapping in the first 2 GiB.
    const MAP_32BIT: i32 = 0x40;

    extern "C" {
        fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, offset: i64) -> *mut u8;
        fn munmap(addr: *mut u8, len: usize) -> i32;
        fn mprotect(addr: *mut u8, len: usize, prot: i32) -> i32;
    }

    pub(super) fn map_low_pair() -> Option<usize> {
        // Phase 5 boot image (WP2): a capture run takes the pair from the
        // brokered low region (fixed, below 2 GiB); `None` in every other mode.
        let brokered = cratonvm_types::image_layout::map_anonymous(
            cratonvm_types::image_layout::SegmentKind::PollPage,
            PAIR_BYTES,
            PAIR_ALIGN,
            PROT_READ,
            MAP_PRIVATE | MAP_ANONYMOUS,
        );
        let base = if let Some(p) = brokered {
            if p.is_null() || p as isize == -1 {
                return None;
            }
            let b = p as usize;
            if b % PAIR_ALIGN != 0 {
                cratonvm_types::image_layout::forget(b);
                // SAFETY: `p` is the mapping just made.
                unsafe {
                    munmap(p, PAIR_BYTES);
                }
                return None;
            }
            b
        } else {
            // Over-map so we can trim prefix/suffix pages to guarantee `PAIR_ALIGN`.
            let over = PAIR_BYTES + PAIR_ALIGN - PAGE;
            // SAFETY: an anonymous private mapping; MAP_FAILED is `-1`.
            let p = unsafe {
                mmap(
                    core::ptr::null_mut(),
                    over,
                    PROT_READ,
                    MAP_PRIVATE | MAP_ANONYMOUS | MAP_32BIT,
                    -1,
                    0,
                )
            };
            if p.is_null() || p as isize == -1 {
                return None;
            }
            let raw = p as usize;
            let b = (raw + PAIR_ALIGN - 1) & !(PAIR_ALIGN - 1);
            if b > raw {
                // SAFETY: unaligned prefix pages from our over-mapping.
                unsafe {
                    munmap(p, b - raw);
                }
            }
            let tail = (raw + over) - (b + PAIR_BYTES);
            if tail > 0 {
                // SAFETY: unused suffix pages from our over-mapping.
                unsafe {
                    munmap((b + PAIR_BYTES) as *mut u8, tail);
                }
            }
            b
        };
        // SAFETY: the guard pages [PAGE..PAIR_BYTES) of the mapping just made.
        if unsafe { mprotect((base + PAGE) as *mut u8, PAIR_BYTES - PAGE, PROT_NONE) } != 0 {
            unmap_pair(base);
            return None;
        }
        Some(base)
    }

    pub(super) fn unmap_pair(base: usize) {
        // Phase 5 boot image (WP2): drop the capture record (no-op otherwise).
        cratonvm_types::image_layout::forget(base);
        // SAFETY: a mapping this module made.
        unsafe {
            munmap(base as *mut u8, PAIR_BYTES);
        }
    }

    pub(super) fn protect_page(base: usize, readable: bool) -> bool {
        let prot = if readable { PROT_READ } else { PROT_NONE };
        // SAFETY: the stop page of a pair this module mapped and never unmaps.
        unsafe { mprotect(base as *mut u8, PAGE, prot) == 0 }
    }
}

#[cfg(not(any(windows, all(target_os = "linux", target_arch = "x86_64"))))]
mod os {
    pub(super) fn map_low_pair() -> Option<usize> {
        None
    }
    pub(super) fn unmap_pair(_base: usize) {}
    pub(super) fn protect_page(_base: usize, _readable: bool) -> bool {
        false
    }
}

#[cfg(test)]
mod tests;
