// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Platform-specific executable memory allocation.
//!
//! Abstracts over OS-level APIs for allocating memory that can hold and execute
//! machine code. All platforms enforce W^X (write xor execute):
//!
//! - **Windows:** `VirtualAlloc` with `PAGE_READWRITE`, then `VirtualProtect`
//!   to `PAGE_EXECUTE_READ` when code is finalized. On non-x86 Windows
//!   (`aarch64-pc-windows-msvc`, `arm64ec-*`) the I-cache is *not* coherent
//!   with the D-cache, so `make_executable` additionally calls
//!   `FlushInstructionCache` — see [`flush_icache_range_windows`]. This was
//!   missing until the 2026-08-01 aarch64 parity audit: the Unix path had an
//!   arch-conditional `__clear_cache` (added when the Linux/FreeBSD aarch64
//!   port landed) but the Windows path had none, so a Windows-on-ARM build
//!   published every JIT body without ever invalidating the I-cache.
//! - **Linux/FreeBSD:** `mmap` with `PROT_READ | PROT_WRITE`, then `mprotect`
//!   to `PROT_READ | PROT_EXEC`. On aarch64 we additionally flush the
//!   instruction cache via the compiler builtin `__clear_cache` before the
//!   RW→RX flip, because ARM's I-cache and D-cache are not coherent (unlike
//!   x86-64, which is automatically coherent and needs no flush).
//! - **macOS ARM64 (Apple Silicon):** Hardware-enforced W^X — memory cannot be
//!   writable and executable simultaneously. Must allocate as RW, write code,
//!   then flip to RX via `mprotect`. Apple provides `pthread_jit_write_protect_np`
//!   for per-thread fast toggling on JIT pages allocated with `MAP_JIT`.
//!   I-cache flush uses Apple's `sys_icache_invalidate`.

/// Errors that can occur during JIT memory operations.
#[derive(Debug)]
pub enum JitError {
    /// OS-level memory allocation failed.
    AllocationFailed,
    /// mprotect / VirtualProtect failed to change page permissions.
    ProtectFailed(i32),
}

impl std::fmt::Display for JitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JitError::AllocationFailed => write!(f, "JIT memory allocation failed"),
            JitError::ProtectFailed(code) => write!(f, "JIT memory protect failed (code {})", code),
        }
    }
}

impl std::error::Error for JitError {}

/// Allocate `size` bytes of memory suitable for writing machine code.
///
/// The returned pointer is guaranteed to be writable but NOT executable.
/// Call [`make_executable`] after writing code to enable execution.
pub fn alloc_executable(size: usize) -> Option<*mut u8> {
    platform_alloc(size)
}

/// Free executable memory previously allocated by [`alloc_executable`].
pub fn free_executable(ptr: *mut u8, size: usize) {
    platform_free(ptr, size);
}

/// Transition a region from writable to executable.
///
/// This calls the appropriate OS API to switch from RW to RX permissions.
/// After this call, writes to the memory are undefined behavior until
/// [`make_writable`] is called.
pub fn make_executable(ptr: *mut u8, size: usize) -> Result<(), JitError> {
    platform_make_executable(ptr, size)
}

/// Make a previously-finalized executable region writable again for patching.
///
/// This calls the appropriate OS API to switch from RX back to RW permissions.
pub fn make_writable(ptr: *mut u8, size: usize) -> Result<(), JitError> {
    platform_make_writable(ptr, size)
}

// ---------------------------------------------------------------------------
// Windows implementation
// ---------------------------------------------------------------------------

#[cfg(target_os = "windows")]
fn platform_alloc(size: usize) -> Option<*mut u8> {
    use std::ptr;

    const MEM_COMMIT: u32 = 0x1000;
    const MEM_RESERVE: u32 = 0x2000;
    const PAGE_READWRITE: u32 = 0x04;

    #[link(name = "kernel32")]
    extern "system" {
        fn VirtualAlloc(
            lpAddress: *mut u8,
            dwSize: usize,
            flAllocationType: u32,
            flProtect: u32,
        ) -> *mut u8;
    }

    // Allocate as RW only — W^X enforcement.
    let p = unsafe {
        VirtualAlloc(
            ptr::null_mut(),
            size,
            MEM_COMMIT | MEM_RESERVE,
            PAGE_READWRITE,
        )
    };
    if p.is_null() {
        None
    } else {
        Some(p)
    }
}

#[cfg(target_os = "windows")]
fn platform_free(ptr: *mut u8, _size: usize) {
    const MEM_RELEASE: u32 = 0x8000;

    #[link(name = "kernel32")]
    extern "system" {
        fn VirtualFree(lpAddress: *mut u8, dwSize: usize, dwFreeType: u32) -> i32;
    }

    unsafe {
        VirtualFree(ptr, 0, MEM_RELEASE);
    }
}

#[cfg(target_os = "windows")]
fn platform_make_executable(ptr: *mut u8, size: usize) -> Result<(), JitError> {
    const PAGE_EXECUTE_READ: u32 = 0x20;

    #[link(name = "kernel32")]
    extern "system" {
        fn VirtualProtect(
            lpAddress: *mut u8,
            dwSize: usize,
            flNewProtect: u32,
            lpflOldProtect: *mut u32,
        ) -> i32;
        fn GetLastError() -> u32;
    }

    // Non-x86 Windows (aarch64 / arm64ec) has split, non-coherent I-cache and
    // D-cache exactly like Linux aarch64. `VirtualProtect` alone does NOT
    // invalidate the I-cache, so without this the CPU can fetch stale bytes for
    // a freshly written (or freshly re-patched) JIT body. Done BEFORE the
    // RW→RX flip for the same reason as the Unix path: while the page is still
    // writable no instruction fetch can race the flush.
    //
    // x86-64/x86 Windows needs nothing (coherent caches, Intel SDM Vol.3 §11.6),
    // and `FlushInstructionCache` there would be a pure syscall cost on the JIT
    // hot path — so the *call* is arch-gated even though the helper is compiled
    // (and therefore type- and link-checked) on every Windows host.
    if !cfg!(any(target_arch = "x86", target_arch = "x86_64")) {
        flush_icache_range_windows(ptr, size);
    }

    let mut old_protect: u32 = 0;
    let ret = unsafe { VirtualProtect(ptr, size, PAGE_EXECUTE_READ, &mut old_protect) };
    if ret == 0 {
        // VirtualProtect returns 0 on failure; surface GetLastError() so the
        // diagnostic carries the actual OS error code rather than the useless
        // `0` return value.
        let err = unsafe { GetLastError() } as i32;
        Err(JitError::ProtectFailed(err))
    } else {
        Ok(())
    }
}

/// Invalidate the CPU instruction cache over `[ptr, ptr+size)` on Windows.
///
/// `FlushInstructionCache(GetCurrentProcess(), base, len)` is the documented
/// Windows API for "I have just written instructions through the data path";
/// on ARM64 it issues the `DC CVAU` / `DSB ISH` / `IC IVAU` / `DSB ISH` / `ISB`
/// sequence the ARM ARM requires (B2.4.4, *Concurrent modification and
/// execution of instructions*). It is a documented no-op on x86/x86-64, whose
/// caches are coherent, which is why the call site above skips it there.
///
/// The function itself is compiled on every Windows target so a typo or a
/// signature mistake is caught by an ordinary x86-64 Windows build, not only by
/// a Windows-on-ARM cross-build nobody runs. Exercised on any Windows host by
/// `tests::windows_icache_flush_is_callable`.
#[cfg(target_os = "windows")]
fn flush_icache_range_windows(ptr: *mut u8, size: usize) {
    #[link(name = "kernel32")]
    extern "system" {
        fn GetCurrentProcess() -> *mut core::ffi::c_void;
        fn FlushInstructionCache(
            hProcess: *mut core::ffi::c_void,
            lpBaseAddress: *const core::ffi::c_void,
            dwSize: usize,
        ) -> i32;
    }
    unsafe {
        // Returns BOOL; a failure here cannot be recovered from (we are about
        // to publish this code either way), and the only documented failure
        // mode is an invalid process handle, which `GetCurrentProcess` — a
        // pseudo-handle constant — cannot produce. Deliberately ignored.
        let _ = FlushInstructionCache(GetCurrentProcess(), ptr as *const core::ffi::c_void, size);
    }
}

#[cfg(target_os = "windows")]
fn platform_make_writable(ptr: *mut u8, size: usize) -> Result<(), JitError> {
    const PAGE_READWRITE: u32 = 0x04;

    #[link(name = "kernel32")]
    extern "system" {
        fn VirtualProtect(
            lpAddress: *mut u8,
            dwSize: usize,
            flNewProtect: u32,
            lpflOldProtect: *mut u32,
        ) -> i32;
        fn GetLastError() -> u32;
    }

    let mut old_protect: u32 = 0;
    let ret = unsafe { VirtualProtect(ptr, size, PAGE_READWRITE, &mut old_protect) };
    if ret == 0 {
        // VirtualProtect returns 0 on failure; surface GetLastError() so the
        // diagnostic carries the actual OS error code rather than the useless
        // `0` return value.
        let err = unsafe { GetLastError() } as i32;
        Err(JitError::ProtectFailed(err))
    } else {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// macOS ARM64 (Apple Silicon) — W^X enforced
// ---------------------------------------------------------------------------

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn platform_alloc(size: usize) -> Option<*mut u8> {
    use std::ptr;

    const PROT_READ: i32 = 1;
    const PROT_WRITE: i32 = 2;
    const MAP_PRIVATE: i32 = 0x02;
    const MAP_ANONYMOUS: i32 = 0x1000; // macOS uses 0x1000 for MAP_ANON
    const MAP_JIT: i32 = 0x0800;
    const MAP_FAILED: *mut u8 = !0 as *mut u8;

    extern "C" {
        fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, offset: i64) -> *mut u8;
    }

    // Allocate as RW with MAP_JIT. Apple Silicon requires MAP_JIT for pages
    // that will later be made executable.
    let p = unsafe {
        mmap(
            ptr::null_mut(),
            size,
            PROT_READ | PROT_WRITE,
            MAP_PRIVATE | MAP_ANONYMOUS | MAP_JIT,
            -1,
            0,
        )
    };
    if p == MAP_FAILED {
        None
    } else {
        Some(p)
    }
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn platform_free(ptr: *mut u8, size: usize) {
    extern "C" {
        fn munmap(addr: *mut u8, len: usize) -> i32;
    }
    unsafe {
        munmap(ptr, size);
    }
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn platform_make_executable(ptr: *mut u8, size: usize) -> Result<(), JitError> {
    const PROT_READ: i32 = 1;
    const PROT_EXEC: i32 = 4;

    extern "C" {
        fn mprotect(addr: *mut u8, len: usize, prot: i32) -> i32;
    }

    unsafe {
        // Flush instruction cache before making executable — required on ARM64
        // where data cache and instruction cache are not coherent.
        sys_icache_invalidate(ptr as *const u8, size);
        let ret = mprotect(ptr, size, PROT_READ | PROT_EXEC);
        if ret != 0 {
            return Err(JitError::ProtectFailed(ret));
        }
    }
    Ok(())
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn platform_make_writable(ptr: *mut u8, size: usize) -> Result<(), JitError> {
    const PROT_READ: i32 = 1;
    const PROT_WRITE: i32 = 2;

    extern "C" {
        fn mprotect(addr: *mut u8, len: usize, prot: i32) -> i32;
    }

    unsafe {
        let ret = mprotect(ptr, size, PROT_READ | PROT_WRITE);
        if ret != 0 {
            return Err(JitError::ProtectFailed(ret));
        }
    }
    Ok(())
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
extern "C" {
    fn sys_icache_invalidate(start: *const u8, size: usize);
}

// ---------------------------------------------------------------------------
// Placing JIT code within RIP reach of the VM's global cells (Unix)
// ---------------------------------------------------------------------------

/// `CRATONVM_JIT_CODE_NEAR_GLOBALS=1` — ask `mmap` to place each JIT code
/// buffer within ±2GB of the VM's global guard cells, so the RIP-relative
/// encodings of the safepoint poll and the layout-epoch guard can actually be
/// used. **Default OFF**, Unix only; Windows already lands in reach without
/// help and is untouched.
///
/// # Why this exists: the short encodings were unreachable on System V, always
///
/// `internal/performance/c2-the-layout-epoch-guard-was-unreachable-by-rip-20260910.md`
/// moved `LAYOUT_REPLACE_EPOCH` off the executable image and onto the VM heap
/// so that `CMP dword [rip+disp32], imm32` — one instruction and ten bytes
/// against three and nineteen — would come into range. On Windows it did, and
/// the four-site loop got 14.5% faster.
///
/// On Linux it did not, and not by bad luck. Measured on a Ubuntu 24.04 x86-64
/// host, one process:
///
/// ```text
/// LAYOUT_REPLACE_EPOCH (mimalloc heap)   0x2001E8103F0
/// stw_requested_flag   (mimalloc heap)   0x2000CD6E2C0     295 MB away
/// optimizing tier's code buffer (mmap)   0x7A53DBCB8000    ~130 TB away
/// ```
///
/// The two heap cells are comfortably within reach of each other and neither
/// is within reach of the code. `mmap(NULL, …)` places anonymous mappings in
/// the kernel's own region, high in the address space; mimalloc reserves its
/// arenas near 2TB. Nothing moves those two towards each other, so on this
/// platform **every** guard and **every** back-edge poll took the long form,
/// on every compile, exactly as they did before the counter was moved. The
/// fallback path was not a safety net, it was the whole implementation.
///
/// # What this does
///
/// Nothing that can produce a wrong address, because it only supplies a HINT.
/// `mmap` without `MAP_FIXED` treats its first argument as a suggestion: it
/// never unmaps or overlaps an existing mapping, so this cannot land JIT code
/// on top of the heap, on top of a reserved arena, or anywhere the GC reads.
/// The kernel is free to ignore the hint entirely, and when the returned
/// mapping is out of reach it is released and the next hint tried.
///
/// The window is probed on BOTH sides of the anchor with doubling
/// offsets (`LADDER`), because the room turned out to be under the allocator's
/// arena rather than above it — and once a placement succeeds the next
/// allocation is asked for beside it, so the ladder is walked once and not per
/// compile. A whole ladder that misses retires the strategy permanently: an
/// address space with no room near the anchor pays eight wasted `mmap`/`munmap`
/// pairs once and none afterwards.
///
/// # Why the anchor is the epoch counter and not the safepoint flag
///
/// The flag lives in the VM's `gc_barrier` allocation, which the JIT crate
/// cannot name without a VM dependency; the counter is in `cratonvm_types`,
/// which it already depends on. They come from the same allocator and, as
/// measured above, sit within a few hundred megabytes of each other — so
/// anchoring on either brings both into reach, and the range check in
/// `ir_lower::rip_disp32` is what actually decides per site.
#[cfg(all(
    not(target_os = "windows"),
    not(all(target_os = "macos", target_arch = "aarch64"))
))]
mod near_globals {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    /// Ladder entries tried before the strategy retires itself. One per
    /// [`LADDER`] rung, so the whole window is probed rather than the first
    /// few megabytes of it.
    const NEAR_GIVE_UP_AFTER: usize = LADDER.len() + 1;

    /// How far from the anchor a buffer may sit and still be usable. Half a
    /// gigabyte inside the architectural ±2GB, because the displacement is
    /// measured from the END of an instruction somewhere inside the buffer and
    /// the anchor is one cell of a heap that keeps growing around it.
    const NEAR_WINDOW: usize = 1_500_000_000;

    /// Offsets from the anchor to suggest, **below it first and doubling**.
    ///
    /// # Why a ladder and not a bump, which is what this was first
    ///
    /// A cursor starting just above the anchor and walking up in
    /// allocation-sized steps found nothing, ever, and the address space says
    /// why. Measured on the Ubuntu host, `/proc/<pid>/maps`:
    ///
    /// ```text
    /// 20000000000-20040000000 rw-p [anon:mimalloc]
    /// ```
    ///
    /// **One 16GB reservation**, with the anchor 511MB inside it. Everything
    /// within 1.5GB *above* the anchor is inside that mapping, so every hint
    /// was relocated and the strategy retired itself having probed 16MB of a
    /// 3GB window. The free space is 511MB *below* the anchor, under the
    /// arena's base — which a one-directional walk in 2MB steps would have
    /// needed 255 probes to reach.
    ///
    /// So: both directions, doubling, below first because an arena is
    /// reserved upward from its base and the room is underneath it.
    const LADDER: [(bool, usize); 8] = [
        (false, 64 << 20),
        (true, 64 << 20),
        (false, 256 << 20),
        (true, 256 << 20),
        (false, 512 << 20),
        (true, 512 << 20),
        (false, 1 << 30),
        (true, 1 << 30),
    ];

    /// 2MB, so hints are transparent-hugepage aligned and consecutive buffers
    /// do not straddle.
    const HINT_ALIGN: usize = 2 << 20;

    static ANCHOR: AtomicUsize = AtomicUsize::new(0);
    /// Where the last successful placement ended, so the next buffer is asked
    /// for beside it instead of re-walking the ladder. Zero means "no
    /// placement has succeeded yet".
    static CURSOR: AtomicUsize = AtomicUsize::new(0);
    static RETIRED: AtomicBool = AtomicBool::new(false);

    /// ENGAGEMENT: buffers placed in reach, and buffers that fell back.
    static IN_REACH: AtomicUsize = AtomicUsize::new(0);
    static FELL_BACK: AtomicUsize = AtomicUsize::new(0);

    fn enabled() -> bool {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ON.get_or_init(|| {
            matches!(
                cratonvm_types::flags::runtime_var("CRATONVM_JIT_CODE_NEAR_GLOBALS").as_deref(),
                Ok("1") | Ok("true") | Ok("on") | Ok("yes")
            )
        })
    }

    /// The cell JIT code most wants to reach, forced into existence now so its
    /// address is stable before the first buffer is placed.
    fn anchor() -> usize {
        let a = ANCHOR.load(Ordering::Relaxed);
        if a != 0 {
            return a;
        }
        // Cast: a `*const u32` used only as a number to compute a hint from.
        let addr = cratonvm_types::field_layout::layout_replace_epoch_guard().0 as usize;
        ANCHOR.store(addr, Ordering::Relaxed);
        addr
    }

    pub(super) fn in_reach(p: usize, size: usize, anchor: usize) -> bool {
        let lo = p;
        let hi = p.saturating_add(size);
        lo.abs_diff(anchor) < NEAR_WINDOW && hi.abs_diff(anchor) < NEAR_WINDOW
    }

    /// The `attempt`-th address to suggest for `size` bytes, or `None` once the
    /// ladder is walked out.
    ///
    /// Attempt 0 is `cursor` when one is held (a non-zero `cursor` means a
    /// previous placement succeeded and the next buffer should pack beside
    /// it); after that it is the ladder, in order. `cursor` is a parameter and
    /// not a read of the static so that one call to [`place`] sees one value
    /// of it — otherwise clearing a stale cursor mid-walk would renumber the
    /// rungs underneath the loop and skip one.
    pub(super) fn nth_hint(
        attempt: usize,
        cursor: usize,
        size: usize,
        anchor: usize,
    ) -> Option<usize> {
        let rung = if cursor != 0 {
            if attempt == 0 {
                return in_reach(cursor, size, anchor).then_some(cursor);
            }
            attempt - 1
        } else {
            attempt
        };
        let (up, off) = *LADDER.get(rung)?;
        let addr = if up {
            anchor.checked_add(off)?
        } else {
            anchor.checked_sub(off)?
        } & !(HINT_ALIGN - 1);
        in_reach(addr, size, anchor).then_some(addr)
    }

    /// Try to place `size` bytes near the anchor. `None` means the caller
    /// should fall back to an unhinted mapping.
    ///
    /// `map` is `platform_alloc`'s raw `mmap` and `unmap` its `munmap`, passed
    /// in so this module holds no `extern` declarations of its own and cannot
    /// drift from the flags the real allocator uses.
    pub(super) fn place(
        size: usize,
        map: impl Fn(*mut u8) -> Option<*mut u8>,
        unmap: impl Fn(*mut u8),
    ) -> Option<*mut u8> {
        if !enabled() || RETIRED.load(Ordering::Relaxed) {
            return None;
        }
        let anchor = anchor();
        if anchor == 0 {
            return None;
        }
        // Read once: see `nth_hint`'s contract.
        let cursor = CURSOR.load(Ordering::Relaxed);
        for attempt in 0..NEAR_GIVE_UP_AFTER {
            let Some(hint) = nth_hint(attempt, cursor, size, anchor) else {
                break;
            };
            // Cast: a hint address, which `mmap` may ignore.
            let Some(p) = map(hint as *mut u8) else {
                // The mapping itself failed; an unhinted retry may still work,
                // so this is not a reason to retire the strategy.
                return None;
            };
            if in_reach(p as usize, size, anchor) {
                let end = (p as usize).saturating_add(size);
                let next = (end + HINT_ALIGN - 1) & !(HINT_ALIGN - 1);
                // A cursor that has walked out of the window is cleared rather
                // than kept, so the next allocation re-enters at the ladder
                // instead of proposing an address the check would refuse.
                CURSOR.store(
                    if in_reach(next, size, anchor) { next } else { 0 },
                    Ordering::Relaxed,
                );
                IN_REACH.fetch_add(1, Ordering::Relaxed);
                return Some(p);
            }
            unmap(p);
            if attempt == 0 && cursor != 0 {
                // The packing address is occupied by something else. Keeping
                // it would spend one wasted `mmap`/`munmap` pair on every
                // future allocation; the ladder re-seeds it on its next hit.
                let _ = CURSOR.compare_exchange(
                    cursor,
                    0,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                );
            }
        }
        // The whole ladder missed. Nothing in this address space is going to
        // change that within the process, and re-walking it per compile is
        // eight wasted `mmap`/`munmap` pairs each time.
        RETIRED.store(true, Ordering::Relaxed);
        FELL_BACK.fetch_add(1, Ordering::Relaxed);
        None
    }

    /// The window [`in_reach`] enforces, so a test can state the architectural
    /// limit it must stay inside rather than restating the number.
    pub(super) const WINDOW: usize = NEAR_WINDOW;

    /// `(placed in reach, fell back, retired)` — for the diagnostic line and
    /// for a test that would otherwise be unable to tell "the flag works" from
    /// "the flag is inert on this machine".
    pub fn stats() -> (usize, usize, bool) {
        (
            IN_REACH.load(Ordering::Relaxed),
            FELL_BACK.load(Ordering::Relaxed),
            RETIRED.load(Ordering::Relaxed),
        )
    }
}

#[cfg(all(
    not(target_os = "windows"),
    not(all(target_os = "macos", target_arch = "aarch64"))
))]
pub use near_globals::stats as code_near_globals_stats;

/// Windows and macOS/ARM64 do not hint: the former already lands in reach and
/// the latter allocates `MAP_JIT` pages the kernel places on its own terms.
#[cfg(any(
    target_os = "windows",
    all(target_os = "macos", target_arch = "aarch64")
))]
pub fn code_near_globals_stats() -> (usize, usize, bool) {
    (0, 0, true)
}

// ---------------------------------------------------------------------------
// Unix (non-macOS-ARM64) — Linux, FreeBSD, macOS x86-64
// ---------------------------------------------------------------------------

#[cfg(all(
    not(target_os = "windows"),
    not(all(target_os = "macos", target_arch = "aarch64"))
))]
fn platform_alloc(size: usize) -> Option<*mut u8> {
    use std::ptr;

    const PROT_READ: i32 = 1;
    const PROT_WRITE: i32 = 2;
    const MAP_PRIVATE: i32 = 0x02;
    const MAP_ANONYMOUS: i32 = 0x20;
    const MAP_FAILED: *mut u8 = !0 as *mut u8;

    extern "C" {
        fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, offset: i64) -> *mut u8;
        fn munmap(addr: *mut u8, len: usize) -> i32;
    }

    // Allocate as RW only — W^X enforcement.
    //
    // `hint` is `mmap`'s first argument and nothing more: without `MAP_FIXED`
    // the kernel may place the mapping anywhere, and will never disturb an
    // existing one. A null hint is the historical behaviour exactly.
    let raw = |hint: *mut u8| -> Option<*mut u8> {
        let p = unsafe {
            mmap(
                hint,
                size,
                PROT_READ | PROT_WRITE,
                MAP_PRIVATE | MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if p == MAP_FAILED {
            None
        } else {
            Some(p)
        }
    };
    let release = |p: *mut u8| {
        // Straight `munmap`, never the poisoning path: this region was mapped
        // microseconds ago, held no code, and was never executable.
        unsafe {
            munmap(p, size);
        }
    };
    if let Some(p) = near_globals::place(size, &raw, &release) {
        return Some(p);
    }
    raw(ptr::null_mut())
}

#[cfg(all(
    not(target_os = "windows"),
    not(all(target_os = "macos", target_arch = "aarch64"))
))]
fn platform_free(ptr: *mut u8, size: usize) {
    extern "C" {
        fn munmap(addr: *mut u8, len: usize) -> i32;
        fn mprotect(addr: *mut u8, len: usize, prot: i32) -> i32;
    }
    unsafe {
        if jit_poison_free_enabled() {
            // DIAG: keep the mapping, make it permanently inaccessible, never
            // recycle the address. A later jump into this retired body still
            // faults at the same moment it would have, but the region is still
            // in /proc/self/maps (as `---p`), which positively identifies the
            // fault as "jumped into retired JIT code" instead of leaving an
            // unmapped address with no provenance.
            const PROT_NONE: i32 = 0;
            mprotect(ptr, size, PROT_NONE);
            POISONED_JIT_BYTES.fetch_add(size, std::sync::atomic::Ordering::Relaxed);
            return;
        }
        munmap(ptr, size);
    }
}

/// DIAG (2026-07-27): `CRATONVM_JIT_POISON_FREE=1` retires executable buffers
/// with `mprotect(PROT_NONE)` instead of `munmap`, so a use-after-free jump into
/// retired JIT code is directly observable (the range stays mapped and the
/// address is never reused). Leaks address space by construction.
pub fn jit_poison_free_enabled() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_var("CRATONVM_JIT_POISON_FREE")
            .map(|v| v != "0")
            .unwrap_or(false)
    })
}

/// Total bytes retired via the poison path.
pub static POISONED_JIT_BYTES: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

#[cfg(all(
    not(target_os = "windows"),
    not(all(target_os = "macos", target_arch = "aarch64"))
))]
fn platform_make_executable(ptr: *mut u8, size: usize) -> Result<(), JitError> {
    const PROT_READ: i32 = 1;
    const PROT_EXEC: i32 = 4;

    extern "C" {
        fn mprotect(addr: *mut u8, len: usize, prot: i32) -> i32;
    }

    // ARM64 (Linux/FreeBSD) has split, non-coherent I-cache and D-cache.
    // After writing instructions via the data path we MUST flush the range
    // before allowing execution, otherwise the CPU may fetch stale bytes
    // (or even predecoded garbage) for every JIT-compiled method.
    //
    // We do this BEFORE flipping to PROT_EXEC: while the page is still RW
    // there is no chance of an instruction fetch racing the flush, and on
    // Linux `__clear_cache` does not require execute permission.
    //
    // x86-64 has coherent I-cache/D-cache (Intel SDM Vol.3 §11.6, AMD APM
    // Vol.2 §7.6.1) and only requires a serialising instruction on the
    // executing thread (any branch suffices). No explicit flush needed.
    #[cfg(all(
        target_arch = "aarch64",
        any(target_os = "linux", target_os = "freebsd")
    ))]
    unsafe {
        flush_icache_range_aarch64(ptr, size);
    }

    let ret = unsafe { mprotect(ptr, size, PROT_READ | PROT_EXEC) };
    if ret != 0 {
        Err(JitError::ProtectFailed(ret))
    } else {
        Ok(())
    }
}

/// Flush a range of CPU instruction cache lines covering `[ptr, ptr+size)`.
///
/// Required on aarch64 Linux/FreeBSD where the data cache (used by the
/// JIT writer) and the instruction cache (used by the fetch unit) are
/// not coherent. macOS-arm64 uses `sys_icache_invalidate` (see the
/// macOS block above); Apple's libc is the only one that exposes it.
///
/// Implementation: we link the compiler builtin `__clear_cache`. On
/// aarch64 GCC and clang this expands to the correct sequence of
/// `DC CVAU` + `DSB ISH` + `IC IVAU` (per-line, using `CTR_EL0` to
/// determine cache line size) + `DSB ISH` + `ISB`. The symbol is
/// supplied by libgcc on GNU targets and by compiler-rt on FreeBSD;
/// both are linked by default when building a Rust binary that uses
/// the standard library.
///
/// Signature follows the GCC builtin: `void __clear_cache(char *begin,
/// char *end)`. End is *exclusive*.
#[cfg(all(
    target_arch = "aarch64",
    any(target_os = "linux", target_os = "freebsd")
))]
unsafe fn flush_icache_range_aarch64(ptr: *mut u8, size: usize) {
    extern "C" {
        fn __clear_cache(begin: *mut core::ffi::c_char, end: *mut core::ffi::c_char);
    }
    let begin = ptr as *mut core::ffi::c_char;
    let end = ptr.add(size) as *mut core::ffi::c_char;
    __clear_cache(begin, end);
}

#[cfg(all(
    not(target_os = "windows"),
    not(all(target_os = "macos", target_arch = "aarch64"))
))]
fn platform_make_writable(ptr: *mut u8, size: usize) -> Result<(), JitError> {
    const PROT_READ: i32 = 1;
    const PROT_WRITE: i32 = 2;

    extern "C" {
        fn mprotect(addr: *mut u8, len: usize, prot: i32) -> i32;
    }

    let ret = unsafe { mprotect(ptr, size, PROT_READ | PROT_WRITE) };
    if ret != 0 {
        Err(JitError::ProtectFailed(ret))
    } else {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alloc_write_free() {
        let size = 4096;
        let ptr = alloc_executable(size).expect("alloc_executable failed");
        assert!(!ptr.is_null());

        // Write a pattern (memory starts as RW)
        unsafe {
            for i in 0..size {
                *ptr.add(i) = (i & 0xFF) as u8;
            }
        }

        // Finalize: transition from RW to RX
        make_executable(ptr, size).expect("make_executable failed");

        // Read back (reading is allowed in both RW and RX)
        unsafe {
            for i in 0..size {
                assert_eq!(*ptr.add(i), (i & 0xFF) as u8);
            }
        }

        free_executable(ptr, size);
    }

    /// A zero-length request must be REFUSED, on every platform we build for.
    ///
    /// The old name (`alloc_zero_returns_some`) was the lie: no backend can
    /// satisfy it. `VirtualAlloc(NULL, 0, ...)` fails with
    /// `ERROR_INVALID_PARAMETER` (87), and POSIX specifies `mmap` with
    /// `len == 0` fails with `EINVAL` — so both `platform_alloc` bodies take
    /// their null / `MAP_FAILED` arm and answer `None`. The failure is that a
    /// caller ever gets a `Some` it would then write code into and hand to
    /// `make_executable` on a zero-byte region.
    ///
    /// Was vacuous: `if let Some(ptr) = alloc_executable(0) { free… }` — the
    /// interesting branch was the silent one, so deleting the `p.is_null()` /
    /// `p == MAP_FAILED` check in `platform_alloc` (returning `Some(null)`)
    /// stayed green.
    #[test]
    fn alloc_zero_is_refused() {
        assert!(
            alloc_executable(0).is_none(),
            "a zero-length JIT allocation must be refused, not handed back"
        );
    }

    #[test]
    fn make_executable_then_writable_roundtrip() {
        let size = 4096;
        let ptr = alloc_executable(size).expect("alloc_executable failed");

        // Write initial data
        unsafe {
            *ptr = 0xAA;
        }

        // Transition RW -> RX
        make_executable(ptr, size).expect("make_executable failed");

        // Read should still work
        unsafe {
            assert_eq!(*ptr, 0xAA);
        }

        // Transition RX -> RW for patching
        make_writable(ptr, size).expect("make_writable failed");

        // Write new data
        unsafe {
            *ptr = 0xBB;
        }

        // Transition back to RX
        make_executable(ptr, size).expect("make_executable failed");
        unsafe {
            assert_eq!(*ptr, 0xBB);
        }

        free_executable(ptr, size);
    }

    #[test]
    fn jit_error_display() {
        let e = JitError::AllocationFailed;
        assert!(format!("{}", e).contains("allocation"));
        let e2 = JitError::ProtectFailed(-1);
        assert!(format!("{}", e2).contains("-1"));
    }

    /// `flush_icache_range_windows` must be callable on any Windows host.
    ///
    /// The production call site is arch-gated (x86/x86-64 have coherent
    /// caches and skip it), so on an ordinary x86-64 Windows CI machine the
    /// call site never runs and a broken `extern` declaration — wrong
    /// calling convention, wrong argument widths, missing symbol — would
    /// only surface on a Windows-on-ARM build. Calling it directly here
    /// forces the declaration to be compiled, linked and executed on every
    /// Windows host. `FlushInstructionCache` is valid (and a documented
    /// no-op) on x86-64, so this is safe to run anywhere Windows runs.
    #[cfg(target_os = "windows")]
    #[test]
    fn windows_icache_flush_is_callable() {
        let size = 4096;
        let ptr = alloc_executable(size).expect("alloc_executable failed");
        unsafe {
            *ptr = 0x90; // one byte of "code" so the range is not untouched
        }
        flush_icache_range_windows(ptr, size);
        // Range covering a sub-page slice, and a zero-length range: both are
        // legal arguments and must not fault.
        flush_icache_range_windows(ptr, 4);
        flush_icache_range_windows(ptr, 0);
        free_executable(ptr, size);
    }

    /// End-to-end check that `make_executable` flushes the icache on
    /// aarch64 so a freshly-written function actually executes.
    ///
    /// We allocate a page, write a single `RET` instruction (0xD65F03C0
    /// little-endian on aarch64 = `ret`), finalize via `make_executable`
    /// (which on Linux/FreeBSD aarch64 also runs `__clear_cache`), then
    /// transmute the pointer to a `fn()` and call it. On x86-64 hosts
    /// this test is compiled out; on macOS aarch64 it exercises the
    /// existing `sys_icache_invalidate` path, which is a useful
    /// regression guard too.
    #[cfg(all(
        target_arch = "aarch64",
        any(target_os = "linux", target_os = "freebsd", target_os = "macos")
    ))]
    #[test]
    fn aarch64_icache_flush_executes_fresh_code() {
        let size = 4096;
        let ptr = alloc_executable(size).expect("alloc_executable failed");

        // aarch64 `ret` = 0xD65F03C0, little-endian byte sequence
        // 0xC0 0x03 0x5F 0xD6.
        unsafe {
            *ptr.add(0) = 0xC0;
            *ptr.add(1) = 0x03;
            *ptr.add(2) = 0x5F;
            *ptr.add(3) = 0xD6;
        }

        make_executable(ptr, size).expect("make_executable failed");

        // Jump to the freshly-written code. If the icache was not
        // flushed the CPU may fetch zero bytes (UDF) or stale data
        // and trap; a clean flush makes this a no-op call that
        // returns normally.
        let f: extern "C" fn() = unsafe { std::mem::transmute(ptr) };
        f();

        free_executable(ptr, size);
    }
}

// ---------------------------------------------------------------------------

/// The placement window, checked against the architecture rather than against
/// itself.
///
/// `mmap` hinting cannot produce a wrong address — the fallback re-maps
/// unhinted and every emitter range-checks its own displacement — so what is
/// worth testing is the ARITHMETIC that decides whether a placement was worth
/// keeping. Getting it wrong in the permissive direction costs nothing at run
/// time (the emitter refuses and falls back to the long form) and costs the
/// whole point of the feature, silently.
#[cfg(all(
    test,
    not(target_os = "windows"),
    not(all(target_os = "macos", target_arch = "aarch64"))
))]
mod near_globals_tests {
    use super::near_globals::{in_reach, nth_hint, WINDOW};

    /// Both ENDS have to be in reach, not just the base. A buffer whose start
    /// is 1.4GB away and whose end is 2.1GB away is one the emitter can use
    /// for its first instruction and not for its last, which is the shape that
    /// would make engagement depend on where in the body a guard happens to
    /// land.
    #[test]
    fn a_buffer_is_in_reach_only_when_both_ends_are() {
        let anchor = 0x2000_0000_0000usize;
        assert!(in_reach(anchor + (64 << 20), 4096, anchor), "64MB away");
        assert!(
            !in_reach(anchor + WINDOW - 4096, 1 << 20, anchor),
            "base inside the window and the end outside it must be refused"
        );
        assert!(!in_reach(anchor + WINDOW + 1, 4096, anchor), "past the window");
    }

    /// Below the anchor as well as above it. `abs_diff` is what makes that
    /// true, and a subtraction written the other way round would silently
    /// refuse every placement the kernel puts underneath.
    #[test]
    fn the_window_is_two_sided() {
        let anchor = 0x2000_0000_0000usize;
        assert!(in_reach(anchor - (64 << 20), 4096, anchor));
        assert!(!in_reach(anchor - WINDOW - (1 << 20), 4096, anchor));
    }

    /// The window must stay inside the architectural ±2GB `disp32` reaches,
    /// with room for the displacement being measured from the END of an
    /// instruction somewhere inside the buffer.
    #[test]
    fn the_window_is_inside_what_disp32_reaches() {
        assert!(
            WINDOW < i32::MAX as usize,
            "the placement window is wider than a RIP displacement can reach,              so a buffer this accepts can still emit the long form -- which              makes every engagement count a lottery"
        );
    }

    /// The ladder must probe BELOW the anchor, and early.
    ///
    /// This is the bug the first version shipped with, stated as a test: a
    /// walk that only goes up finds nothing on a host whose allocator reserves
    /// one 16GB arena with the anchor 511MB inside it, because everything
    /// above the anchor within reach is that arena. The free space is
    /// underneath. A ladder that reaches down only on its seventh rung would
    /// have the same failure with extra steps, so this pins the FIRST rung.
    #[test]
    fn the_ladder_probes_below_the_anchor_first() {
        let anchor = 0x2001_E810_3F0usize;
        let first = nth_hint(0, 0, 4096, anchor).expect("a first hint");
        assert!(
            first < anchor,
            "the first hint is above the anchor ({first:#x} vs {anchor:#x}),              which is the side an arena reserved upward from its base occupies"
        );
        // …and the ladder must get far enough down to clear a half-gigabyte
        // arena prefix within its rungs.
        let deepest = (0..16)
            .filter_map(|i| nth_hint(i, 0, 4096, anchor))
            .filter(|h| *h < anchor)
            .map(|h| anchor - h)
            .max()
            .expect("some rung below");
        assert!(
            deepest >= 1 << 30,
            "the ladder only reaches {deepest} bytes below the anchor; an              arena prefix larger than that puts every rung inside it"
        );
    }

    /// Every rung it offers is one `in_reach` would accept, so a hint is never
    /// proposed that the acceptance check would then refuse.
    #[test]
    fn every_rung_is_inside_the_window() {
        let anchor = 0x2001_E810_3F0usize;
        for i in 0..16 {
            if let Some(h) = nth_hint(i, 0, 1 << 20, anchor) {
                assert!(
                    in_reach(h, 1 << 20, anchor),
                    "rung {i} proposes {h:#x}, outside the window it is checked against"
                );
            }
        }
    }
}
