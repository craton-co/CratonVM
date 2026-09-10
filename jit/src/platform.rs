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
    }

    // Allocate as RW only — W^X enforcement.
    let p = unsafe {
        mmap(
            ptr::null_mut(),
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
// Code-adjacent data cells
// ---------------------------------------------------------------------------

/// One cell: a full cache line, so a cell can never share a line with an
/// unrelated one and a writer of one cannot invalidate another's.
///
/// x86-64's destructive-interference size, and the same 64 the two in-tree
/// `#[repr(align(64))]` precedents (`gc/src/zgc/census.rs`,
/// `gc/src/collector.rs`) already use.
pub const CODE_ADJACENT_CELL_SIZE: usize = 64;

/// How much address space one arena chunk covers.
///
/// 64 KiB rather than a single 4 KiB page because that is Windows' allocation
/// GRANULARITY: `VirtualAlloc(NULL, 4096, ...)` reserves 64 KiB regardless and
/// wastes the other 60 KiB of address space, so asking for less buys nothing.
/// On Unix the extra pages are demand-faulted and cost one page-table entry
/// each until touched. At [`CODE_ADJACENT_CELL_SIZE`] per cell that is 1024
/// cells per chunk, so in practice a process allocates exactly one chunk.
const ARENA_CHUNK: usize = 64 * 1024;

/// `(bump cursor, chunk end)`. `(0, 0)` means "no chunk yet".
static CELL_ARENA: std::sync::Mutex<(usize, usize)> = std::sync::Mutex::new((0, 0));

/// Hand out one zero-filled, cache-line-isolated, never-unmapped
/// [`CODE_ADJACENT_CELL_SIZE`]-byte cell for a word that JIT-compiled code
/// READS DIRECTLY, from the same OS primitive [`alloc_executable`] uses.
///
/// # Why the allocator matters
///
/// A word compiled code polls wants to be within ±2 GB of the code that polls
/// it, because that is the reach of x86-64's `disp32` and therefore the
/// difference between the one-instruction, no-register RIP-relative form and a
/// materialize-the-address fallback (`MOV r64, imm64` + the access: 15 bytes
/// and a clobbered register instead of 7 and none). Nothing FAILS when the
/// word is out of reach — every emitter that wants the short form checks the
/// delta and falls back — so the cost is silent, and no test notices.
///
/// The Rust heap cannot supply that adjacency. The default allocator here is
/// mimalloc (`vm-cli/src/main.rs`), which reserves its arenas from an address
/// range unrelated to the one an anonymous `mmap` / `VirtualAlloc(NULL, ...)`
/// hands out; measured on Linux, a `GcBarrier` field and a code buffer sat
/// ~124 TB apart. Two allocations from THE SAME primitive land in the same
/// region of the address space, which is what puts them in `disp32` range.
///
/// This is a placement HINT, not a guarantee: the OS chooses, and a caller
/// must stay correct when it chooses badly. Every caller therefore keeps its
/// out-of-reach fallback, and none may assume the cell is close to anything.
///
/// # Lifetime
///
/// Chunks are never unmapped and cells are never recycled, so an address
/// handed out here is valid for the rest of the process and can be baked into
/// generated code as an immediate. The cost is
/// [`CODE_ADJACENT_CELL_SIZE`] bytes per cell ever requested, permanently —
/// affordable only because callers are per-VM-ish singletons, not per-object.
/// Do not call this from anything that runs more than a bounded number of
/// times.
///
/// Returns `None` if the OS refuses the mapping; a caller must then fall back
/// to ordinary allocation and accept the long encoding.
pub fn alloc_code_adjacent_cell() -> Option<*mut u8> {
    // A poisoned arena mutex would mean a panic while holding it, and the only
    // thing done under it is integer arithmetic plus `platform_alloc` — take
    // the lock back rather than propagating a `None` that would silently
    // downgrade every later caller's encoding.
    let mut arena = CELL_ARENA.lock().unwrap_or_else(|e| e.into_inner());
    let (cursor, end) = *arena;
    if cursor == 0 || cursor + CODE_ADJACENT_CELL_SIZE > end {
        // `platform_alloc` returns page-aligned (Unix) or 64 KiB-granule
        // aligned (Windows) memory, both a multiple of the cell size, so the
        // bump cursor stays cache-line aligned without any rounding here.
        let base = platform_alloc(ARENA_CHUNK)? as usize;
        *arena = (base, base + ARENA_CHUNK);
    }
    let cell = arena.0;
    arena.0 += CODE_ADJACENT_CELL_SIZE;
    // Fresh anonymous pages are zero-filled by both `mmap(MAP_ANONYMOUS)` and
    // `VirtualAlloc(MEM_COMMIT)`, so this is redundant on the first cell of a
    // chunk. Written anyway: the guarantee a caller needs is "this cell reads
    // as zero", and making it depend on which cell of which chunk it happened
    // to get is the kind of thing that holds until someone adds recycling.
    // SAFETY: `cell` is the cell just carved out of a mapping this arena owns
    // and never releases; the bump cursor guarantees it is in bounds and that
    // no other reference to it exists.
    unsafe { std::ptr::write_bytes(cell as *mut u8, 0, CODE_ADJACENT_CELL_SIZE) };
    Some(cell as *mut u8)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Cells must be distinct, cache-line aligned, and zero.
    ///
    /// Zero is the part with teeth: `CacheLineFlag::new` writes the initial
    /// value itself, but every other prospective caller (a counter, an epoch)
    /// will read the cell before writing it, and "fresh anonymous pages are
    /// zero-filled" is a property of the FIRST cell of a chunk, not of the
    /// 900th.
    #[test]
    fn code_adjacent_cells_are_distinct_aligned_and_zero() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..8 {
            let cell = alloc_code_adjacent_cell().expect("cell allocation failed");
            let addr = cell as usize;
            assert_eq!(
                addr % CODE_ADJACENT_CELL_SIZE,
                0,
                "cell {addr:#x} is not cache-line aligned, so it can share a \
                 line with its neighbour"
            );
            assert!(seen.insert(addr), "cell {addr:#x} was handed out twice");
            // SAFETY: a freshly carved cell of exactly this size that nothing
            // else holds a reference to.
            let bytes = unsafe { std::slice::from_raw_parts(cell, CODE_ADJACENT_CELL_SIZE) };
            assert!(
                bytes.iter().all(|&b| b == 0),
                "cell {addr:#x} is not zeroed"
            );
            // Dirty it, so the next iteration's zero check is about the
            // allocator rather than about untouched pages.
            // SAFETY: as above; this cell is ours for the rest of the process.
            unsafe { std::ptr::write_bytes(cell, 0xA5, CODE_ADJACENT_CELL_SIZE) };
        }
    }

    /// The whole point: a cell must land within `disp32` reach of JIT code.
    ///
    /// This is the property `jit/src/x64/safepoint.rs::emit_safepoint_poll`
    /// silently loses when it does not hold — the poll stays correct and grows
    /// from 7 bytes to 15 plus a clobbered register, at every loop back edge
    /// and method entry in the process. Nothing else in the tree fails, which
    /// is exactly why it went unnoticed from 2026-09-02 to 2026-09-10 with the
    /// flag on the mimalloc heap, ~124 TB from the code cache.
    ///
    /// x86-64 only: `disp32` reach is what makes the distance matter, and
    /// aarch64's `ADRP` has its own (±4 GB) window that no emitter uses yet.
    ///
    /// Both sides come from `platform_alloc`, so this asserts that the OS puts
    /// two allocations from one primitive in one region. That is true of every
    /// mapper we run on but is not architecturally guaranteed; if it ever
    /// fails, the fallback still runs and this test is the notification.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn a_cell_is_within_disp32_of_a_code_buffer() {
        let code = alloc_executable(4096).expect("alloc_executable failed");
        let cell = alloc_code_adjacent_cell().expect("cell allocation failed");
        // Widening: usize -> i128, so the subtraction cannot wrap.
        let delta = (cell as usize as i128) - (code as usize as i128);
        let in_reach = delta >= i32::MIN as i128 && delta <= i32::MAX as i128;
        free_executable(code, 4096);
        assert!(
            in_reach,
            "code buffer {:#x} and data cell {:#x} are {:.1} GB apart; a \
             RIP-relative poll cannot reach, so every safepoint poll in the \
             process falls back to MOV r64, imm64",
            code as usize,
            cell as usize,
            (delta.unsigned_abs() as f64) / (1024.0 * 1024.0 * 1024.0),
        );
    }

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
