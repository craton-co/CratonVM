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
//! - **macOS ARM64 (Apple Silicon):** W^X is enforced PER THREAD on `MAP_JIT`
//!   pages, not per page. The region is mapped once as RWX with `MAP_JIT`, and
//!   `pthread_jit_write_protect_np` switches the calling thread between
//!   "may write, may not execute" and the reverse. `mprotect` is not used: the
//!   hardened runtime refuses to make a once-writable `MAP_JIT` page executable
//!   that way, so the previous RW-then-`mprotect` shape aborted every compile
//!   on a hardened build. Every write to code memory therefore runs inside a
//!   [`JitWriteScope`]. I-cache flush uses Apple's `sys_icache_invalidate`.
//!
//! The Unix `mmap` flags are NOT portable: `MAP_ANONYMOUS` is `0x20` on Linux
//! and `0x1000` on the BSDs and Darwin. See [`map_anonymous`].

/// Errors that can occur during JIT memory operations.
#[derive(Debug)]
pub enum JitError {
    /// OS-level memory allocation failed.
    AllocationFailed,
    /// mprotect / VirtualProtect failed to change page permissions. The payload
    /// is the OS error code (`errno` on Unix, `GetLastError()` on Windows), not
    /// the call's return value, which is always the uninformative `-1` / `0`.
    ProtectFailed(i32),
}

/// The `errno` of the call that just failed, for [`JitError::ProtectFailed`].
///
/// `mprotect` returns `-1` for every failure, so recording its return value --
/// what this module used to do -- told a reader nothing about whether the
/// mapping was wrong (`EINVAL`), the policy refused (`EACCES`) or memory ran
/// out (`ENOMEM`). `-1` survives only when the OS reports no code at all.
#[cfg(not(target_os = "windows"))]
#[cfg_attr(all(target_os = "macos", target_arch = "aarch64"), allow(dead_code))]
fn last_os_error_code() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(-1)
}

/// Which `mmap(2)` flag values an operating system uses.
///
/// `PROT_*`, `MAP_PRIVATE` and `MAP_FAILED` agree across every Unix this crate
/// can target; `MAP_ANONYMOUS` does not. The Unix allocator hard-coded Linux's
/// `0x20` for "Linux, FreeBSD, macOS x86-64" alike. On FreeBSD and Darwin
/// `0x20` is `MAP_RENAME` (or unassigned), so `mmap` failed with `EINVAL`,
/// `alloc_executable` returned `None`, and the JIT was silently dead there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)] // a Windows build names no Unix ABI
enum UnixMmapAbi {
    /// Linux and Android on every architecture but MIPS.
    Linux,
    /// Linux on MIPS, which kept the IRIX flag values.
    LinuxMips,
    /// Darwin, FreeBSD, NetBSD, OpenBSD, DragonFly.
    Bsd,
    /// illumos and Solaris.
    Solaris,
}

/// `MAP_ANONYMOUS` for `abi`. A pure function of its argument so the table is
/// checked on every host, not only on the one it describes.
#[allow(dead_code)] // unused on Windows
const fn map_anonymous(abi: UnixMmapAbi) -> i32 {
    match abi {
        UnixMmapAbi::Linux => 0x20,
        UnixMmapAbi::LinuxMips => 0x800,
        UnixMmapAbi::Bsd => 0x1000,
        UnixMmapAbi::Solaris => 0x100,
    }
}

#[cfg(all(
    any(target_os = "linux", target_os = "android"),
    not(any(
        target_arch = "mips",
        target_arch = "mips64",
        target_arch = "mips32r6",
        target_arch = "mips64r6"
    ))
))]
const HOST_MMAP_ABI: Option<UnixMmapAbi> = Some(UnixMmapAbi::Linux);
#[cfg(all(
    any(target_os = "linux", target_os = "android"),
    any(
        target_arch = "mips",
        target_arch = "mips64",
        target_arch = "mips32r6",
        target_arch = "mips64r6"
    )
))]
const HOST_MMAP_ABI: Option<UnixMmapAbi> = Some(UnixMmapAbi::LinuxMips);
#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly"
))]
#[allow(dead_code)] // macOS/ARM64's allocator names `UnixMmapAbi::Bsd` directly
const HOST_MMAP_ABI: Option<UnixMmapAbi> = Some(UnixMmapAbi::Bsd);
#[cfg(any(target_os = "illumos", target_os = "solaris"))]
const HOST_MMAP_ABI: Option<UnixMmapAbi> = Some(UnixMmapAbi::Solaris);
/// Any other OS: no known flag value, so the allocator refuses rather than
/// passing a guess to `mmap`. Refusing is what the wrong constant did too, but
/// now it is the documented outcome rather than an accident.
#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly",
    target_os = "illumos",
    target_os = "solaris"
)))]
#[allow(dead_code)] // Windows never maps through `mmap`
const HOST_MMAP_ABI: Option<UnixMmapAbi> = None;

/// A region in which the CURRENT THREAD may write into JIT code memory.
///
/// A zero-cost no-op everywhere except macOS on Apple Silicon, where code pages
/// are `MAP_JIT` and executable-or-writable per thread. There, entering the
/// first scope on a thread calls `pthread_jit_write_protect_np(0)` and leaving
/// the last calls `pthread_jit_write_protect_np(1)`. Scopes nest, and the
/// thread is back in execute mode as soon as the outermost one drops.
///
/// Hold it only around the write itself. While it is held, EVERY `MAP_JIT`
/// page is non-executable for this thread, so calling into compiled code from
/// inside a scope faults. That is why `ExecutableBuffer` takes a scope per
/// copy rather than for the life of a buffer: a buffer that is allocated and
/// then abandoned without `finalize` (every failed compile) would otherwise
/// leave its thread unable to run any JIT code again.
///
/// `!Send`, because the permission belongs to the thread that entered it.
#[must_use = "the write permission lasts only as long as the scope"]
pub struct JitWriteScope {
    _thread_bound: std::marker::PhantomData<*const ()>,
}

impl JitWriteScope {
    /// Allow this thread to write code memory until the scope drops.
    #[inline]
    pub fn enter() -> Self {
        platform_jit_write_begin();
        Self {
            _thread_bound: std::marker::PhantomData,
        }
    }
}

impl Drop for JitWriteScope {
    #[inline]
    fn drop(&mut self) {
        platform_jit_write_end();
    }
}

#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
#[inline(always)]
fn platform_jit_write_begin() {}

#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
#[inline(always)]
fn platform_jit_write_end() {}

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
    // SAFETY: a null address asks the OS to choose, so the new region cannot
    // alias an existing one; every other argument is a plain value, and a
    // failure is reported as a null return, handled below.
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

    // SAFETY: `ptr` is the base `VirtualAlloc` returned for this region (the
    // `free_executable` contract), and `MEM_RELEASE` with size 0 releases
    // exactly that reservation. The caller owns no reference into it after.
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
    // SAFETY: `ptr..ptr+size` lies inside a region `platform_alloc` committed,
    // and `old_protect` is a live local the call writes one `u32` into.
    let ret = unsafe { VirtualProtect(ptr, size, PAGE_EXECUTE_READ, &mut old_protect) };
    if ret == 0 {
        // VirtualProtect returns 0 on failure; surface GetLastError() so the
        // diagnostic carries the actual OS error code rather than the useless
        // `0` return value.
        // SAFETY: `GetLastError` takes no arguments and reads thread-local state.
        // Cast: a Win32 error code is a `DWORD`; `ProtectFailed` carries `i32`.
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
    // SAFETY: `GetCurrentProcess` returns a pseudo-handle constant that is
    // always valid for the calling process, and `FlushInstructionCache` only
    // uses `ptr`/`size` as an address range to invalidate; it does not
    // dereference them, so any range (including an empty one) is sound.
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
    // SAFETY: as in `platform_make_executable`: a committed region this module
    // allocated, and a live local for the old protection.
    let ret = unsafe { VirtualProtect(ptr, size, PAGE_READWRITE, &mut old_protect) };
    if ret == 0 {
        // VirtualProtect returns 0 on failure; surface GetLastError() so the
        // diagnostic carries the actual OS error code rather than the useless
        // `0` return value.
        // SAFETY: `GetLastError` takes no arguments and reads thread-local state.
        // Cast: a Win32 error code is a `DWORD`; `ProtectFailed` carries `i32`.
        let err = unsafe { GetLastError() } as i32;
        Err(JitError::ProtectFailed(err))
    } else {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// macOS ARM64 (Apple Silicon) — per-thread W^X on MAP_JIT pages
// ---------------------------------------------------------------------------
//
// The previous shape mapped `MAP_JIT` pages RW and flipped them to RX with
// `mprotect`. The hardened runtime refuses that flip for a page that was ever
// writable, so `finalize` aborted the process on every compile of a hardened
// build, and nothing here ever called the toggle the platform actually
// provides. The supported protocol is: map once as RWX with `MAP_JIT`, then
// let `pthread_jit_write_protect_np` decide, per thread, whether those pages
// are writable (0) or executable (1). `JitWriteScope` holds the thread in write
// mode around each copy; outside a scope it is in execute mode, which is also
// the state a new thread starts in.

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
extern "C" {
    fn pthread_jit_write_protect_np(enabled: i32);
    fn sys_icache_invalidate(start: *mut core::ffi::c_void, size: usize);
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
thread_local! {
    /// How many [`JitWriteScope`]s this thread holds. Only the outermost one
    /// toggles, so a nested write cannot re-protect pages an outer write is
    /// still copying into.
    static JIT_WRITE_DEPTH: std::cell::Cell<usize> = std::cell::Cell::new(0);
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn platform_jit_write_begin() {
    JIT_WRITE_DEPTH.with(|depth| {
        let d = depth.get();
        if d == 0 {
            // SAFETY: takes a plain int and changes only this thread's view of
            // `MAP_JIT` pages. The matching re-protect is in
            // `platform_jit_write_end`, reached from `JitWriteScope::drop`.
            unsafe { pthread_jit_write_protect_np(0) };
        }
        depth.set(d + 1);
    });
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn platform_jit_write_end() {
    JIT_WRITE_DEPTH.with(|depth| {
        let d = depth.get().saturating_sub(1);
        depth.set(d);
        if d == 0 {
            // SAFETY: as in `platform_jit_write_begin`; 1 restores execute mode.
            unsafe { pthread_jit_write_protect_np(1) };
        }
    });
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn platform_alloc(size: usize) -> Option<*mut u8> {
    use std::ptr;

    const PROT_READ: i32 = 1;
    const PROT_WRITE: i32 = 2;
    const PROT_EXEC: i32 = 4;
    const MAP_PRIVATE: i32 = 0x02;
    const MAP_JIT: i32 = 0x0800;
    const MAP_FAILED: *mut u8 = !0 as *mut u8;

    extern "C" {
        fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, offset: i64) -> *mut u8;
    }

    // RWX with MAP_JIT: the per-thread toggle, not the page protection, is
    // what enforces W^X on these pages.
    // SAFETY: a null hint without `MAP_FIXED` lets the kernel choose, so the
    // mapping cannot alias an existing one; every argument is a plain value and
    // a failure comes back as `MAP_FAILED`, handled below.
    let p = unsafe {
        mmap(
            ptr::null_mut(),
            size,
            PROT_READ | PROT_WRITE | PROT_EXEC,
            MAP_PRIVATE | map_anonymous(UnixMmapAbi::Bsd) | MAP_JIT,
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

/// Plain RW anonymous memory for the code-adjacent data cells.
///
/// NOT `MAP_JIT`. Those cells are written by the RUNTIME (the safepoint flag is
/// set by whichever thread requests a stop), and a `MAP_JIT` page is writable
/// only by a thread that is currently in write mode -- every other thread would
/// fault on the store.
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn platform_alloc_data(size: usize) -> Option<*mut u8> {
    const PROT_READ: i32 = 1;
    const PROT_WRITE: i32 = 2;
    const MAP_PRIVATE: i32 = 0x02;
    const MAP_FAILED: *mut u8 = !0 as *mut u8;

    extern "C" {
        fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, offset: i64) -> *mut u8;
    }

    // SAFETY: as in `platform_alloc` -- a kernel-placed anonymous mapping.
    let p = unsafe {
        mmap(
            std::ptr::null_mut(),
            size,
            PROT_READ | PROT_WRITE,
            MAP_PRIVATE | map_anonymous(UnixMmapAbi::Bsd),
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
    // SAFETY: `ptr`/`size` describe a mapping `platform_alloc` returned (the
    // `free_executable` contract), and nothing references it afterwards.
    unsafe {
        munmap(ptr, size);
    }
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn platform_make_executable(ptr: *mut u8, size: usize) -> Result<(), JitError> {
    // Flush the instruction cache -- required on ARM64, where the data and
    // instruction caches are not coherent. There is no permission change to
    // make: the region is already executable for every thread not currently
    // inside a `JitWriteScope`.
    // SAFETY: `sys_icache_invalidate` treats `ptr..ptr+size` as an address
    // range to discard cached decodes for; it does not dereference the memory.
    unsafe { sys_icache_invalidate(ptr.cast(), size) };
    Ok(())
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn platform_make_writable(_ptr: *mut u8, _size: usize) -> Result<(), JitError> {
    // Nothing to do at the page level: a write is permitted exactly while the
    // writing thread holds a `JitWriteScope`, which `ExecutableBuffer`'s write
    // paths take for themselves. Refusing here would break the patch protocol
    // for no gain.
    Ok(())
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
                report("placed", anchor, p as usize, size, attempt);
                let end = (p as usize).saturating_add(size);
                let next = (end + HINT_ALIGN - 1) & !(HINT_ALIGN - 1);
                // A cursor that has walked out of the window is cleared rather
                // than kept, so the next allocation re-enters at the ladder
                // instead of proposing an address the check would refuse.
                CURSOR.store(
                    if in_reach(next, size, anchor) {
                        next
                    } else {
                        0
                    },
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
                let _ = CURSOR.compare_exchange(cursor, 0, Ordering::Relaxed, Ordering::Relaxed);
            }
        }
        // The whole ladder missed. Nothing in this address space is going to
        // change that within the process, and re-walking it per compile is
        // eight wasted `mmap`/`munmap` pairs each time.
        RETIRED.store(true, Ordering::Relaxed);
        FELL_BACK.fetch_add(1, Ordering::Relaxed);
        report("RETIRED", anchor, 0, size, NEAR_GIVE_UP_AFTER);
        None
    }

    /// `CRATONVM_DBG_CODE_NEAR_GLOBALS=1` — one line per placement DECISION,
    /// which is the only way a run can be shown to have engaged this strategy
    /// rather than merely to have set its flag.
    ///
    /// # Why this is not optional bookkeeping
    ///
    /// [`stats`] has said since it was written that it exists "for the
    /// diagnostic line", and there was no diagnostic line — nothing in the
    /// process read it. That gap is not cosmetic on THIS flag: it is a hint to
    /// `mmap` and the kernel is free to ignore it, so "the flag was set" and
    /// "the code moved" are genuinely different facts, and on a host with no
    /// room near the anchor the second is false while the first is true. A
    /// differential run whose arms are indistinguishable from the outside
    /// proves nothing, and before this line the only way to tell them apart
    /// was to disassemble a method and count `81 3D` opcodes.
    ///
    /// `[near-globals] placed anchor=0x… buf=0x… size=… delta=…MB attempt=N
    /// in_reach=… fell_back=… retired=…`
    ///
    /// `delta` is what actually decides the encoding, so it is printed rather
    /// than left for the reader to subtract: a buffer inside the window is
    /// what makes `rip_disp32` hand out the short form.
    fn report(outcome: &str, anchor: usize, buf: usize, size: usize, attempt: usize) {
        if !cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_CODE_NEAR_GLOBALS") {
            return;
        }
        let (in_reach_n, fell_back_n, retired) = stats();
        // Megabytes, integer: the question this answers is "inside ±1.5GB or
        // not", and a byte count makes that harder to read, not easier.
        let delta_mb = if buf == 0 {
            0
        } else {
            buf.abs_diff(anchor) / (1 << 20)
        };
        eprintln!(
            "[near-globals] {outcome} anchor={anchor:#x} buf={buf:#x} size={size} \
             delta={delta_mb}MB attempt={attempt} in_reach={in_reach_n} \
             fell_back={fell_back_n} retired={retired}"
        );
    }

    /// The window [`in_reach`] enforces, so a test can state the architectural
    /// limit it must stay inside rather than restating the number.
    pub(super) const WINDOW: usize = NEAR_WINDOW;

    /// Whether this strategy owns placement in this process.
    ///
    /// Read by [`super::alloc_code_adjacent_cell`], which must NOT hand out a
    /// cell while this is on — see its doc. Not `stats().2`: a strategy that
    /// has retired still owned the decision, and the cells it declined were
    /// declined for the whole process.
    pub(super) fn engaged() -> bool {
        enabled()
    }

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
// Unix (non-macOS-ARM64) — Linux, the BSDs, macOS x86-64, illumos
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
    const MAP_FAILED: *mut u8 = !0 as *mut u8;
    // Per OS, not Linux's value everywhere -- see `UnixMmapAbi`.
    let map_anon = map_anonymous(HOST_MMAP_ABI?);

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
        // SAFETY: no `MAP_FIXED`, so `hint` is advisory and the kernel never
        // replaces an existing mapping; the arguments are plain values and a
        // failure comes back as `MAP_FAILED`, handled below.
        let p = unsafe {
            mmap(
                hint,
                size,
                PROT_READ | PROT_WRITE,
                MAP_PRIVATE | map_anon,
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
        // SAFETY: `p` is a mapping of exactly `size` bytes that `raw` returned
        // a moment ago and that nothing else has seen.
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
    // SAFETY: `ptr`/`size` describe a mapping `platform_alloc` returned (the
    // `free_executable` contract). Either call retires it: `munmap` releases
    // it, `mprotect(PROT_NONE)` leaves it mapped but inaccessible. Nothing may
    // reference the region afterwards, which is the caller's obligation.
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
    // SAFETY: `ptr..ptr+size` is the mapping `platform_alloc` returned, so the
    // end pointer `flush_icache_range_aarch64` computes stays one-past-the-end
    // of that allocation.
    unsafe {
        flush_icache_range_aarch64(ptr, size);
    }

    // SAFETY: a mapping this module owns; `mprotect` changes only its
    // protection and reports failure through its return value.
    let ret = unsafe { mprotect(ptr, size, PROT_READ | PROT_EXEC) };
    if ret != 0 {
        Err(JitError::ProtectFailed(last_os_error_code()))
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
///
/// # Safety
///
/// `ptr..ptr+size` must lie within one allocation, so that `ptr.add(size)` is
/// in bounds or one past the end.
#[cfg(all(
    target_arch = "aarch64",
    any(target_os = "linux", target_os = "freebsd")
))]
unsafe fn flush_icache_range_aarch64(ptr: *mut u8, size: usize) {
    extern "C" {
        fn __clear_cache(begin: *mut core::ffi::c_char, end: *mut core::ffi::c_char);
    }
    let begin = ptr as *mut core::ffi::c_char;
    // SAFETY: the caller guarantees the range is one allocation, so the end
    // pointer is at most one past its end.
    let end = unsafe { ptr.add(size) } as *mut core::ffi::c_char;
    // SAFETY: `__clear_cache` only issues cache-maintenance instructions over
    // the address range; it neither reads nor writes the bytes.
    unsafe { __clear_cache(begin, end) };
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

    // SAFETY: a mapping this module owns; `mprotect` changes only its
    // protection and reports failure through its return value.
    let ret = unsafe { mprotect(ptr, size, PROT_READ | PROT_WRITE) };
    if ret != 0 {
        Err(JitError::ProtectFailed(last_os_error_code()))
    } else {
        Ok(())
    }
}

/// Data cells need no special mapping off macOS/ARM64: see the macOS arm's
/// `platform_alloc_data` for why that one platform must not share the code
/// allocator.
#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
fn platform_alloc_data(size: usize) -> Option<*mut u8> {
    platform_alloc(size)
}

// ---------------------------------------------------------------------------
// Code-adjacent data cells
// ---------------------------------------------------------------------------

/// Whether `near_globals` owns code/data placement in this process.
///
/// Always `false` where that module is not built: Windows lands in reach
/// without help and macOS/ARM64 allocates `MAP_JIT` pages on the kernel's
/// terms, so neither has the strategy and neither has the conflict.
#[cfg(all(
    not(target_os = "windows"),
    not(all(target_os = "macos", target_arch = "aarch64"))
))]
fn near_globals_engaged() -> bool {
    near_globals::engaged()
}

#[cfg(any(
    target_os = "windows",
    all(target_os = "macos", target_arch = "aarch64")
))]
fn near_globals_engaged() -> bool {
    false
}

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
/// # It defers to `near_globals`, and must
///
/// There are two strategies for getting a polled word and the code that polls
/// it into one `disp32` window, and they point in opposite directions. This one
/// moves the WORD to the code's allocator. [`near_globals`] moves the CODE to
/// the allocator the VM's globals already live in, by hinting `mmap` — and it
/// anchors on `layout_replace_epoch_guard()`, relying on every other polled
/// cell being a few hundred megabytes away in that same mimalloc band.
///
/// Handing out a cell while that strategy is engaged would take the safepoint
/// flag OUT of the band the anchor is pulling the code towards, and leave the
/// poll ~130 TB behind — the precise failure `82bf52efd` describes for the
/// epoch counter, in the other cell. So this returns `None` whenever
/// `near_globals` owns placement, and the caller's ordinary-allocation fallback
/// is then the CORRECT answer rather than a degraded one.
///
/// Whoever owns placement must own it for every cell at once. Today that is
/// `near_globals` when `CRATONVM_JIT_CODE_NEAR_GLOBALS` is set and this
/// allocator otherwise.
///
/// Returns `None` if the OS refuses the mapping, or if `near_globals` is
/// engaged; a caller must then fall back to ordinary allocation. In the first
/// case it accepts the long encoding, in the second it is how it gets the short
/// one.
pub fn alloc_code_adjacent_cell() -> Option<*mut u8> {
    if near_globals_engaged() {
        return None;
    }
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
        let base = platform_alloc_data(ARENA_CHUNK)? as usize;
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
        if near_globals_engaged() {
            // The allocator declines by design here; there are no cells to
            // assert about. See `alloc_code_adjacent_cell`.
            return;
        }
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
        if near_globals_engaged() {
            // `near_globals` owns placement and this allocator declines, so
            // there is no cell whose distance means anything. Reach in that
            // configuration is that strategy's own claim, asserted by
            // `near_globals_tests`.
            return;
        }
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

        // Write a pattern (memory starts as RW; on macOS/ARM64 the scope is
        // what makes it writable for this thread).
        {
            let _write = JitWriteScope::enter();
            // SAFETY: `ptr` is a fresh `size`-byte allocation that nothing
            // else references, and every index is below `size`.
            unsafe {
                for i in 0..size {
                    *ptr.add(i) = (i & 0xFF) as u8;
                }
            }
        }

        // Finalize: transition from RW to RX
        make_executable(ptr, size).expect("make_executable failed");

        // Read back (reading is allowed in both RW and RX)
        // SAFETY: as above; reads stay in bounds and the region is readable.
        unsafe {
            for i in 0..size {
                assert_eq!(*ptr.add(i), (i & 0xFF) as u8);
            }
        }

        free_executable(ptr, size);
    }

    /// `MAP_ANONYMOUS` differs per OS, and the table must say so on EVERY host.
    ///
    /// The Unix allocator used Linux's `0x20` for Linux, FreeBSD and macOS
    /// x86-64 alike. On the BSDs and Darwin that is not `MAP_ANON`, `mmap`
    /// failed with `EINVAL`, and the JIT was silently dead on those systems.
    /// This checks the table itself, so a Windows or Linux CI run catches a
    /// regression for a platform it cannot execute.
    #[test]
    fn map_anonymous_follows_each_os_abi() {
        assert_eq!(
            map_anonymous(UnixMmapAbi::Linux),
            0x20,
            "linux <asm-generic/mman-common.h>"
        );
        assert_eq!(
            map_anonymous(UnixMmapAbi::LinuxMips),
            0x800,
            "linux <asm/mman.h> on MIPS"
        );
        assert_eq!(
            map_anonymous(UnixMmapAbi::Bsd),
            0x1000,
            "Darwin and FreeBSD <sys/mman.h>"
        );
        assert_eq!(
            map_anonymous(UnixMmapAbi::Solaris),
            0x100,
            "illumos <sys/mman.h>"
        );
        assert_ne!(
            map_anonymous(UnixMmapAbi::Linux),
            map_anonymous(UnixMmapAbi::Bsd),
            "the two families genuinely differ -- one constant cannot serve both"
        );
    }

    /// The host picks the family its own headers use.
    #[test]
    fn the_host_mmap_abi_is_this_targets_own() {
        #[cfg(all(target_os = "linux", not(target_arch = "mips64")))]
        assert_eq!(HOST_MMAP_ABI, Some(UnixMmapAbi::Linux));
        #[cfg(any(target_os = "macos", target_os = "freebsd"))]
        assert_eq!(HOST_MMAP_ABI, Some(UnixMmapAbi::Bsd));
        #[cfg(target_os = "windows")]
        assert_eq!(HOST_MMAP_ABI, None, "Windows never maps through mmap");
    }

    /// A failed `mprotect` reports `errno`, not its `-1` return value.
    ///
    /// A pointer one byte into a page is not page-aligned, which POSIX makes
    /// `EINVAL` (22 on Linux, the BSDs and Darwin alike).
    #[cfg(all(
        not(target_os = "windows"),
        not(all(target_os = "macos", target_arch = "aarch64"))
    ))]
    #[test]
    fn a_protect_failure_carries_errno() {
        let size = 4096;
        let ptr = alloc_executable(size).expect("alloc_executable failed");
        // SAFETY: one byte into a 4096-byte allocation is in bounds.
        let misaligned = unsafe { ptr.add(1) };
        match make_executable(misaligned, size - 1) {
            Err(JitError::ProtectFailed(code)) => {
                assert_ne!(
                    code, -1,
                    "the payload must be errno, not mprotect's return value"
                );
                assert_eq!(code, 22, "a misaligned mprotect is EINVAL");
            }
            other => panic!("a misaligned mprotect must fail, got {other:?}"),
        }
        free_executable(ptr, size);
    }

    /// Scopes nest, and a write inside the innermost one lands.
    #[test]
    fn jit_write_scopes_nest() {
        let size = 4096;
        let ptr = alloc_executable(size).expect("alloc_executable failed");
        {
            let _outer = JitWriteScope::enter();
            {
                let _inner = JitWriteScope::enter();
                // SAFETY: in bounds of a fresh allocation nothing else holds.
                unsafe { *ptr = 0x5A };
            }
            // Still inside the outer scope: the inner drop must not have
            // re-protected the page for this thread.
            // SAFETY: as above.
            unsafe { *ptr.add(1) = 0xA5 };
            #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
            assert_eq!(JIT_WRITE_DEPTH.with(|d| d.get()), 1);
        }
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        assert_eq!(
            JIT_WRITE_DEPTH.with(|d| d.get()),
            0,
            "the thread is back in execute mode"
        );
        make_executable(ptr, size).expect("make_executable failed");
        // SAFETY: reads in bounds of a readable region.
        unsafe {
            assert_eq!(*ptr, 0x5A);
            assert_eq!(*ptr.add(1), 0xA5);
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
        {
            let _write = JitWriteScope::enter();
            // SAFETY: in bounds of a fresh allocation nothing else references.
            unsafe {
                *ptr = 0xAA;
            }
        }

        // Transition RW -> RX
        make_executable(ptr, size).expect("make_executable failed");

        // Read should still work
        // SAFETY: an in-bounds read of a readable region.
        unsafe {
            assert_eq!(*ptr, 0xAA);
        }

        // Transition RX -> RW for patching
        make_writable(ptr, size).expect("make_writable failed");

        // Write new data
        {
            let _write = JitWriteScope::enter();
            // SAFETY: as above; the region is writable again.
            unsafe {
                *ptr = 0xBB;
            }
        }

        // Transition back to RX
        make_executable(ptr, size).expect("make_executable failed");
        // SAFETY: an in-bounds read of a readable region.
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
        // SAFETY: in bounds of a fresh RW allocation nothing else references.
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
        {
            let _write = JitWriteScope::enter();
            // SAFETY: four in-bounds bytes of a fresh allocation.
            unsafe {
                *ptr.add(0) = 0xC0;
                *ptr.add(1) = 0x03;
                *ptr.add(2) = 0x5F;
                *ptr.add(3) = 0xD6;
            }
        }

        make_executable(ptr, size).expect("make_executable failed");

        // Jump to the freshly-written code. If the icache was not
        // flushed the CPU may fetch zero bytes (UDF) or stale data
        // and trap; a clean flush makes this a no-op call that
        // returns normally.
        // SAFETY: the region now holds exactly one `RET` and is executable, so
        // it is a valid `extern "C" fn()` that returns immediately.
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
        assert!(
            !in_reach(anchor + WINDOW + 1, 4096, anchor),
            "past the window"
        );
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
