// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! zs s1/fp: a cheaper first touch for mimalloc's first arena
//! (`CRATONVM_MIMALLOC_FIRST_TOUCH`, opt-in, Linux launcher only).
//!
//! # The trade it tries to break
//!
//! zr measured both ends on the host (`docs/feature-designs/zgc-architecture.md
//! orch-notes.md`, "mimalloc variants" and "xmt"):
//!
//! - mimalloc's default eager arena is one 1 GiB mapping that mimalloc
//!   advises `MADV_HUGEPAGE`. Every 2 MiB run anything touches faults in
//!   whole: few faults (`minflt` ~5k on class loading), but every partly used
//!   small (64 KiB) or medium (512 KiB) page drags its whole 2 MiB into RSS
//!   (Hello 218 MB, 3000 classes 274 MB; HotSpot 42-48 MB).
//! - Lazy commit (`CRATONVM_MIMALLOC_LAZY_ARENA_COMMIT=1`), and equally an
//!   eager arena with `MIMALLOC_ALLOW_THP=0`, fault 4 KiB at a time: -80 MB,
//!   but ~35k more faults, +7..20 % CPU. The CPU is the faults, not the TLB:
//!   eager + no-THP costs what lazy costs.
//!
//! So the RSS is the 4 KiB granularity's and the CPU is the fault count's.
//! Neither of mimalloc's switches separates the two; its arena commit
//! callback can.
//!
//! # What this does
//!
//! Before any Rust allocation (a constructor in `.init_array.00102`, after the
//! lazy-commit hook's `00100`), the launcher maps the first arena itself --
//! `CRATONVM_MIMALLOC_FIRST_TOUCH_ARENA_MB` (default 1024, mimalloc's own
//! `arena_reserve`), 2 MiB aligned, read-write, `MAP_NORESERVE`, advised
//! `MADV_NOHUGEPAGE` as a whole -- and hands it to mimalloc as NOT committed
//! (`mi_manage_memory` with a commit callback). mimalloc then calls
//! [`commit_fun`] for every slice range it is about to use, and for every
//! purge. The callback is where the policy lives:
//!
//! - `huge`: the whole 2 MiB-aligned chunks inside one commit request get
//!   `MADV_HUGEPAGE`. mimalloc commits a whole page at a time, so a request of
//!   2 MiB or more is a large singleton page (one big block: a `Vec`, a hash
//!   table, a buffer) or a run of medium pages -- memory that is used
//!   densely or not at all -- and keeps the huge page's one-fault cost there.
//!   Small and medium pages (64 KiB / 512 KiB requests, the partly used ones)
//!   stay on 4 KiB pages.
//! - `populate`: the 4 KiB part of each commit is pre-faulted with one
//!   `MADV_POPULATE_WRITE` (Linux >= 5.14; refused once with `EINVAL`, then
//!   off): one syscall instead of up to 16 or 128 page faults per small or
//!   medium page. It costs RSS where a committed page is never written
//!   (mimalloc commits a small page whole on first use, but extends its free
//!   list in steps), so it is the arm to weigh against `huge` alone.
//! - `lazy`: neither -- the arena behaves like mimalloc's lazy commit. It is
//!   the control: it must reproduce the `LAZY_ARENA_COMMIT=1` row, or the
//!   managed arena itself costs something.
//!
//! A purge (`commit=false`) is `MADV_DONTNEED` (what mimalloc's own purge
//! does on Linux) and reports the range decommitted, so a reused range comes
//! back through the callback and its policy.
//!
//! `1` / `on` mean `huge,populate`. Tokens combine with `,` or `+`.
//!
//! # Safety of the arrangement
//!
//! - The mapping is read-write from the start: a commit never has to change a
//!   protection, so the callback cannot fail (it returns true), and a purge
//!   leaves a readable, zero range (mimalloc's own Linux purge does the same).
//! - `is_zero` is always reported `false`: mimalloc's arena then trusts only
//!   its own dirty bitmap for zeroness, exactly as with its own commit
//!   (`_mi_prim_commit` reports `false` too).
//! - The arena is registered before the first allocation, so it is the only
//!   arena mimalloc knows when the first page is needed; once it is full,
//!   mimalloc reserves its own arenas as usual (eager, huge-page advised).
//! - mimalloc's process constructor has no priority under gcc (only clang
//!   gives it 101), so the hook calls `mi_process_init` itself first (it runs
//!   once; the later automatic call is a no-op).
//! - Ownership: every block in the arena is a mimalloc block (its page map
//!   has it), so `sparse_block_alloc`'s `mi_is_in_heap_region` routing is
//!   unchanged.
//!
//! Environment only, read with libc `getenv` before `main` (nothing here
//! allocates). A launcher-process allocator policy, not compatibility state:
//! `libcratonvm` and `cratonvm-embed` run on their host's allocator.
//!
//! # Seeing it
//!
//! `CRATONVM_GC_STATS` prints `[cratonvm] mimalloc-first-touch:` (see
//! [`summary_line`]): the mode, whether the arena was registered, and the
//! commit / huge / populate / purge totals. The positive control is
//! `commits > 1` with `outcome=registered` (the first commit is the arena's
//! own meta-data, made at registration; zs s2/rv); `huge_kb` and `populate_kb`
//! say each policy engaged.

use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

/// Bit of [`FirstTouchMode`]: advise whole 2 MiB chunks of big commits
/// `MADV_HUGEPAGE`.
pub(crate) const MODE_HUGE: u8 = 1;
/// Bit of [`FirstTouchMode`]: pre-fault the 4 KiB part of each commit.
pub(crate) const MODE_POPULATE: u8 = 2;
/// Bit of [`FirstTouchMode`]: the managed arena is installed (with no other
/// bit: the `lazy` control).
pub(crate) const MODE_ON: u8 = 4;

/// The huge-page granule the policy aligns to (x86_64 / aarch64-4K PMD).
pub(crate) const HUGE: usize = 2 << 20;
/// Default arena size, MiB: mimalloc's own `arena_reserve` on 64-bit.
pub(crate) const DEFAULT_ARENA_MB: usize = 1024;
/// Smallest arena accepted, MiB (mimalloc refuses an arena below one 32 MiB
/// bitmap chunk; leave room for its meta-data).
pub(crate) const FLOOR_ARENA_MB: usize = 64;
/// Largest arena accepted, MiB (zs s2/rv): 1 TiB. Without a cap a value of
/// 2^44 MiB or more saturated the byte size to `usize::MAX`, `size + HUGE`
/// wrapped before the `mmap`, and mimalloc was handed an extent far past the
/// mapping (pre-`main` writes into foreign memory).
pub(crate) const CEIL_ARENA_MB: usize = 1 << 20;

/// The switch, parsed. `0` is off (mimalloc's own first arena).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FirstTouchMode(pub(crate) u8);

impl FirstTouchMode {
    pub(crate) fn on(self) -> bool {
        self.0 & MODE_ON != 0
    }
    pub(crate) fn huge(self) -> bool {
        self.0 & MODE_HUGE != 0
    }
    pub(crate) fn populate(self) -> bool {
        self.0 & MODE_POPULATE != 0
    }

    /// `off`, `lazy`, `huge`, `populate`, `huge+populate`.
    pub(crate) fn name(self) -> &'static str {
        match (self.on(), self.huge(), self.populate()) {
            (false, _, _) => "off",
            (true, false, false) => "lazy",
            (true, true, false) => "huge",
            (true, false, true) => "populate",
            (true, true, true) => "huge+populate",
        }
    }
}

/// The switch's value to a mode. Unset, empty, or an off word (`0`, `off`,
/// `false`, `no`) → off. `1`, `on`, `true` → `huge,populate`. Otherwise a list
/// of `lazy` / `huge` / `populate` tokens separated by `,` or `+`, any case;
/// an unknown token is ignored, and a list of only unknown tokens is off (a
/// typo must not silently install the arena). Does not allocate.
pub(crate) fn parse_mode(value: Option<&[u8]>) -> FirstTouchMode {
    let Some(v) = value.map(<[u8]>::trim_ascii) else {
        return FirstTouchMode(0);
    };
    if v.is_empty() || crate::mimalloc_tuning::switch_is_off(Some(v)) {
        return FirstTouchMode(0);
    }
    if [&b"1"[..], b"on", b"true"]
        .iter()
        .any(|w| v.eq_ignore_ascii_case(w))
    {
        return FirstTouchMode(MODE_ON | MODE_HUGE | MODE_POPULATE);
    }
    let mut bits = 0u8;
    for tok in v.split(|b| *b == b',' || *b == b'+') {
        let tok = tok.trim_ascii();
        if tok.eq_ignore_ascii_case(b"lazy") {
            bits |= MODE_ON;
        } else if tok.eq_ignore_ascii_case(b"huge") {
            bits |= MODE_ON | MODE_HUGE;
        } else if tok.eq_ignore_ascii_case(b"populate") {
            bits |= MODE_ON | MODE_POPULATE;
        }
    }
    FirstTouchMode(bits)
}

/// The arena size in bytes: a decimal MiB count, at least
/// [`FLOOR_ARENA_MB`]; unset, empty or unparsable → [`DEFAULT_ARENA_MB`].
pub(crate) fn parse_arena_bytes(value: Option<&[u8]>) -> usize {
    let mb = value
        .map(<[u8]>::trim_ascii)
        .filter(|v| !v.is_empty() && v.iter().all(u8::is_ascii_digit))
        .and_then(|v| {
            v.iter().try_fold(0usize, |acc, d| {
                acc.checked_mul(10)?.checked_add((d - b'0') as usize)
            })
        })
        .unwrap_or(DEFAULT_ARENA_MB);
    mb.clamp(FLOOR_ARENA_MB, CEIL_ARENA_MB) << 20
}

/// Split a commit request `[start, start + len)` into the whole 2 MiB chunks
/// it contains (`huge`, possibly empty) and the parts before and after them.
/// Returns `(head, huge, tail)` as `(addr, len)` pairs; lengths may be 0. The
/// three cover the request exactly.
pub(crate) fn split_commit(start: usize, len: usize) -> [(usize, usize); 3] {
    let end = start.saturating_add(len);
    let lo = start.div_ceil(HUGE).saturating_mul(HUGE);
    let hi = end / HUGE * HUGE;
    if hi <= lo || lo < start || hi > end {
        return [(start, len), (lo.min(end), 0), (end, 0)];
    }
    [(start, lo - start), (lo, hi - lo), (hi, end - hi)]
}

/// What the hook did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum Outcome {
    /// The hook never ran (not Linux, no mimalloc).
    NotRun = 0,
    /// The switch is off: mimalloc's own first arena.
    Off = 1,
    /// The arena is mimalloc's first arena.
    Registered = 2,
    /// The mapping failed: mimalloc's own first arena.
    MapFailed = 3,
    /// mimalloc refused the arena (it was unmapped again).
    Refused = 4,
}

static OUTCOME: AtomicU8 = AtomicU8::new(Outcome::NotRun as u8);
static MODE: AtomicU8 = AtomicU8::new(0);
static ARENA_BYTES: AtomicUsize = AtomicUsize::new(0);
static COMMITS: AtomicUsize = AtomicUsize::new(0);
static COMMIT_BYTES: AtomicUsize = AtomicUsize::new(0);
static HUGE_BYTES: AtomicUsize = AtomicUsize::new(0);
static POPULATE_BYTES: AtomicUsize = AtomicUsize::new(0);
static POPULATE_FAILED: AtomicUsize = AtomicUsize::new(0);
/// Set once `MADV_POPULATE_WRITE` answered `EINVAL` (an older kernel):
/// populate stops for the rest of the run.
static POPULATE_UNSUPPORTED: AtomicU8 = AtomicU8::new(0);
static PURGES: AtomicUsize = AtomicUsize::new(0);
static PURGE_BYTES: AtomicUsize = AtomicUsize::new(0);

/// The `[cratonvm] mimalloc-first-touch:` line of the `CRATONVM_GC_STATS`
/// summary.
pub(crate) fn summary_line() -> String {
    let outcome = match OUTCOME.load(Ordering::Relaxed) {
        1 => "off",
        2 => "registered",
        3 => "map-failed",
        4 => "refused",
        _ => "not-run",
    };
    let kb = |a: &AtomicUsize| a.load(Ordering::Relaxed) / 1024;
    format!(
        concat!(
            "[cratonvm] mimalloc-first-touch: mode={} outcome={} arena_mb={} commits={}",
            " commit_kb={} huge_kb={} populate_kb={} populate_failed={}",
            " populate_unsupported={} purges={} purge_kb={}"
        ),
        FirstTouchMode(MODE.load(Ordering::Relaxed)).name(),
        outcome,
        ARENA_BYTES.load(Ordering::Relaxed) >> 20,
        COMMITS.load(Ordering::Relaxed),
        kb(&COMMIT_BYTES),
        kb(&HUGE_BYTES),
        kb(&POPULATE_BYTES),
        POPULATE_FAILED.load(Ordering::Relaxed),
        POPULATE_UNSUPPORTED.load(Ordering::Relaxed),
        PURGES.load(Ordering::Relaxed),
        kb(&PURGE_BYTES),
    )
}

#[cfg(all(feature = "mimalloc", target_os = "linux"))]
mod hook {
    use super::*;
    use std::os::raw::{c_char, c_int, c_long, c_void};

    // zt t3/fp (t2/rv FP-2): the MAP_* / MADV_* values below are the x86_64 /
    // aarch64 (asm-generic) ones; refuse a build for an arch whose values differ.
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    compile_error!("MAP_*/MADV_* constants are the x86_64/aarch64 values");

    const PROT_READ: c_int = 1;
    const PROT_WRITE: c_int = 2;
    const MAP_PRIVATE: c_int = 0x02;
    const MAP_ANONYMOUS: c_int = 0x20;
    const MAP_NORESERVE: c_int = 0x4000;
    const MADV_DONTNEED: c_int = 4;
    const MADV_HUGEPAGE: c_int = 14;
    const MADV_NOHUGEPAGE: c_int = 15;
    const MADV_POPULATE_WRITE: c_int = 23;
    const EINVAL: c_int = 22;
    const MAP_FAILED: *mut c_void = !0usize as *mut c_void;
    const PR_SET_VMA: c_int = 0x5356_4d41;
    const PR_SET_VMA_ANON_NAME: std::os::raw::c_ulong = 0;

    type CommitFun = extern "C" fn(
        commit: bool,
        start: *mut c_void,
        size: usize,
        is_zero: *mut bool,
        arg: *mut c_void,
    ) -> bool;

    extern "C" {
        fn getenv(name: *const c_char) -> *mut c_char;
        fn mmap(
            addr: *mut c_void,
            len: usize,
            prot: c_int,
            flags: c_int,
            fd: c_int,
            off: c_long,
        ) -> *mut c_void;
        fn munmap(addr: *mut c_void, len: usize) -> c_int;
        fn madvise(addr: *mut c_void, len: usize, advice: c_int) -> c_int;
        fn __errno_location() -> *mut c_int;
        fn prctl(option: c_int, ...) -> c_int;
        fn mi_process_init();
        fn mi_manage_memory(
            start: *mut c_void,
            size: usize,
            is_committed: bool,
            is_pinned: bool,
            is_zero: bool,
            numa_node: c_int,
            exclusive: bool,
            commit_fun: CommitFun,
            commit_fun_arg: *mut c_void,
            arena_id: *mut *mut c_void,
        ) -> bool;
    }

    /// `getenv` as bytes, without allocating.
    fn env(name: &'static [u8]) -> Option<&'static [u8]> {
        // SAFETY: `name` is NUL-terminated (every caller passes a `b"..\0"`
        // literal); before `main` no thread can be mutating the environment.
        let p = unsafe { getenv(name.as_ptr() as *const c_char) };
        if p.is_null() {
            None
        } else {
            // SAFETY: `getenv` returned a NUL-terminated string that lives as
            // long as the environment is not modified.
            Some(unsafe { std::ffi::CStr::from_ptr(p) }.to_bytes())
        }
    }

    fn advise(addr: usize, len: usize, advice: c_int) -> Result<(), c_int> {
        if len == 0 {
            return Ok(());
        }
        // SAFETY: `[addr, addr + len)` lies inside the arena mapping this
        // module made (mimalloc only commits / purges ranges of the arena it
        // was given); `madvise` with these advices never unmaps.
        let rc = unsafe { madvise(addr as *mut c_void, len, advice) };
        if rc == 0 {
            Ok(())
        } else {
            // SAFETY: errno is thread-local and valid after a failed call.
            Err(unsafe { *__errno_location() })
        }
    }

    fn populate(addr: usize, len: usize) {
        if len == 0 || POPULATE_UNSUPPORTED.load(Ordering::Relaxed) != 0 {
            return;
        }
        match advise(addr, len, MADV_POPULATE_WRITE) {
            Ok(()) => {
                POPULATE_BYTES.fetch_add(len, Ordering::Relaxed);
            }
            Err(EINVAL) => {
                // Kernel before 5.14: no MADV_POPULATE_WRITE. Off for good;
                // the pages fault in on touch as with `lazy`.
                POPULATE_UNSUPPORTED.store(1, Ordering::Relaxed);
                POPULATE_FAILED.fetch_add(1, Ordering::Relaxed);
            }
            Err(_) => {
                // ENOMEM / EFAULT / EINTR: harmless, the touch faults instead.
                POPULATE_FAILED.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// mimalloc's arena commit callback (`mi_commit_fun_t`). Thread-safe:
    /// syscalls and relaxed counters only, no allocation.
    extern "C" fn commit_fun(
        commit: bool,
        start: *mut c_void,
        size: usize,
        is_zero: *mut bool,
        _arg: *mut c_void,
    ) -> bool {
        if !is_zero.is_null() {
            // SAFETY: mimalloc passes either NULL or a valid out-pointer.
            unsafe { *is_zero = false };
        }
        let start = start as usize;
        if !commit {
            // A purge: give the pages back as mimalloc's own Linux purge does,
            // and report the range decommitted so its reuse comes back here.
            //
            // zs s2/rv: under `huge`, take the THP advice back first. DONTNEED
            // empties the PMD but leaves VM_HUGEPAGE on the range, so a purged
            // chunk later re-committed for small or medium pages faulted a
            // whole 2 MiB page again -- what the policy exists to avoid. A
            // partial purge also clears the rest of that chunk's advice, which
            // is harmless: DONTNEED already split the PMD.
            if FirstTouchMode(MODE.load(Ordering::Relaxed)).huge() {
                let _ = advise(start, size, MADV_NOHUGEPAGE);
            }
            if advise(start, size, MADV_DONTNEED).is_ok() {
                PURGES.fetch_add(1, Ordering::Relaxed);
                PURGE_BYTES.fetch_add(size, Ordering::Relaxed);
                return true;
            }
            return false;
        }
        COMMITS.fetch_add(1, Ordering::Relaxed);
        COMMIT_BYTES.fetch_add(size, Ordering::Relaxed);
        let mode = FirstTouchMode(MODE.load(Ordering::Relaxed));
        // zt t3/fp (t2/rv FP-1, zs2rv item 3): without `huge`, pre-fault at
        // most one `HUGE` (2 MiB) of a commit, so a big singleton commit stays
        // lazy, as the module doc's "small and medium pages" says.
        let [head, huge, tail] = if mode.huge() {
            split_commit(start, size)
        } else {
            [(start, size.min(HUGE)), (start, 0), (start + size, 0)]
        };
        if huge.1 != 0 && advise(huge.0, huge.1, MADV_HUGEPAGE).is_ok() {
            HUGE_BYTES.fetch_add(huge.1, Ordering::Relaxed);
        }
        if mode.populate() {
            populate(head.0, head.1);
            populate(tail.0, tail.1);
        }
        // The mapping is read-write from the start: the commit itself has
        // nothing to do and cannot fail.
        true
    }

    extern "C" fn first_touch_arena() {
        let mode = parse_mode(env(
            concat!("CRATONVM_MIMALLOC_FIRST_TOUCH", "\0").as_bytes()
        ));
        MODE.store(mode.0, Ordering::Relaxed);
        if !mode.on() {
            OUTCOME.store(Outcome::Off as u8, Ordering::Relaxed);
            return;
        }
        let size = parse_arena_bytes(env(
            concat!("CRATONVM_MIMALLOC_FIRST_TOUCH_ARENA_MB", "\0").as_bytes()
        ));
        // Over-map by one huge page and trim, so the arena is 2 MiB aligned
        // and the huge chunks are real PMD-sized runs.
        let Some(want) = size.checked_add(HUGE) else {
            OUTCOME.store(Outcome::MapFailed as u8, Ordering::Relaxed);
            return;
        };
        // SAFETY: an anonymous private mapping at no fixed address.
        let p = unsafe {
            mmap(
                std::ptr::null_mut(),
                want,
                PROT_READ | PROT_WRITE,
                MAP_PRIVATE | MAP_ANONYMOUS | MAP_NORESERVE,
                -1,
                0,
            )
        };
        if p == MAP_FAILED || p.is_null() {
            OUTCOME.store(Outcome::MapFailed as u8, Ordering::Relaxed);
            return;
        }
        let base = p as usize;
        let start = base.div_ceil(HUGE) * HUGE;
        let head = start - base;
        let tail = want - head - size;
        // SAFETY: both trims are inside the mapping just made and nothing
        // else knows of it yet.
        unsafe {
            if head != 0 {
                munmap(base as *mut c_void, head);
            }
            if tail != 0 {
                munmap((start + size) as *mut c_void, tail);
            }
        }
        // 4 KiB pages unless the policy says otherwise, on any THP mode (on a
        // THP=`always` host an unadvised range would fault huge pages too).
        let _ = advise(start, size, MADV_NOHUGEPAGE);
        // Name it as mimalloc names its own mappings, so smaps (and
        // `tools/rss-by-mapping.sh`) show it as `[anon:mimalloc]`. Best
        // effort: a kernel before 5.17 refuses, and nothing depends on it.
        // SAFETY: names a range of the mapping just made; the name is a
        // static NUL-terminated string.
        let _ = unsafe {
            prctl(
                PR_SET_VMA,
                PR_SET_VMA_ANON_NAME,
                start,
                size,
                b"mimalloc\0".as_ptr(),
            )
        };
        // SAFETY: mimalloc's process init runs once (a later call, such as
        // its own constructor's, is a no-op); it reserves no arena.
        unsafe { mi_process_init() };
        // SAFETY: `[start, start + size)` is a fresh read-write mapping that
        // only mimalloc will use from now on; it is zero; `commit_fun` is a
        // valid callback for the life of the process.
        let ok = unsafe {
            mi_manage_memory(
                start as *mut c_void,
                size,
                false,
                false,
                true,
                -1,
                false,
                commit_fun,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if ok {
            // zt t3/fp (t2/rv FP-3): the size is reported only for an arena
            // mimalloc took (a refused one is unmapped below).
            ARENA_BYTES.store(size, Ordering::Relaxed);
            OUTCOME.store(Outcome::Registered as u8, Ordering::Relaxed);
        } else {
            // SAFETY: mimalloc did not take the range; nothing else has it.
            unsafe { munmap(start as *mut c_void, size) };
            OUTCOME.store(Outcome::Refused as u8, Ordering::Relaxed);
        }
    }

    /// Priority 102: after std's `.init_array.00099` argv hook and the
    /// lazy-commit hook's `00100` (so a `CRATONVM_MIMALLOC_LAZY_ARENA_COMMIT`
    /// default is in place before `mi_process_init` reads the options), and
    /// before every constructor without a priority (which may allocate).
    #[used]
    #[link_section = ".init_array.00102"]
    static FIRST_TOUCH_ARENA: extern "C" fn() = first_touch_arena;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zs1fp_the_switch_is_opt_in_and_parses_tokens() {
        for v in [
            None,
            Some(&b""[..]),
            Some(b" "),
            Some(b"0"),
            Some(b"off"),
            Some(b"No"),
        ] {
            assert_eq!(parse_mode(v), FirstTouchMode(0), "{v:?}");
            assert!(!parse_mode(v).on());
        }
        assert_eq!(parse_mode(Some(b"1")).name(), "huge+populate");
        assert_eq!(parse_mode(Some(b"ON")).name(), "huge+populate");
        assert_eq!(parse_mode(Some(b"lazy")).name(), "lazy");
        assert_eq!(parse_mode(Some(b"huge")).name(), "huge");
        assert_eq!(parse_mode(Some(b" Populate ")).name(), "populate");
        assert_eq!(parse_mode(Some(b"huge+populate")).name(), "huge+populate");
        assert_eq!(parse_mode(Some(b"populate, huge")).name(), "huge+populate");
        // A typo installs nothing.
        assert_eq!(parse_mode(Some(b"hugee")), FirstTouchMode(0));
        assert_eq!(parse_mode(Some(b"lazy,bogus")).name(), "lazy");
    }

    #[test]
    fn zs1fp_the_arena_size_is_mib_with_a_floor() {
        assert_eq!(parse_arena_bytes(None), 1024 << 20);
        assert_eq!(parse_arena_bytes(Some(b"")), 1024 << 20);
        assert_eq!(parse_arena_bytes(Some(b"abc")), 1024 << 20);
        assert_eq!(parse_arena_bytes(Some(b"512")), 512 << 20);
        assert_eq!(parse_arena_bytes(Some(b" 2048 ")), 2048 << 20);
        assert_eq!(parse_arena_bytes(Some(b"8")), FLOOR_ARENA_MB << 20);
        assert_eq!(
            parse_arena_bytes(Some(b"99999999999999999999999")),
            1024 << 20,
            "an overflow is unparsable"
        );
        // zs s2/rv: a parsable but absurd size is clamped, never saturated.
        assert_eq!(
            parse_arena_bytes(Some(b"17592186044416")),
            CEIL_ARENA_MB << 20
        );
    }

    #[test]
    fn zs1fp_a_commit_splits_into_whole_huge_chunks_and_the_rest() {
        let m = HUGE;
        // A 64 KiB small page: no whole chunk.
        assert_eq!(
            split_commit(5 * m + 65536, 65536),
            [
                (5 * m + 65536, 65536),
                (5 * m + 131072, 0),
                (5 * m + 131072, 0)
            ]
        );
        // An aligned 4 MiB singleton: all huge.
        assert_eq!(
            split_commit(4 * m, 2 * m),
            [(4 * m, 0), (4 * m, 2 * m), (6 * m, 0)]
        );
        // Unaligned 5 MiB: head, one... two whole chunks, tail.
        let s = 3 * m + m / 2;
        let [h, g, t] = split_commit(s, 5 * (1 << 20));
        assert_eq!(h, (s, m / 2));
        assert_eq!(g, (4 * m, 2 * m));
        assert_eq!(t, (6 * m, s + 5 * (1 << 20) - 6 * m));
        assert_eq!(h.1 + g.1 + t.1, 5 << 20, "the three cover the request");
        // Exactly one chunk's worth but straddling: no whole chunk.
        let [h, g, t] = split_commit(m + 4096, m);
        assert_eq!(g.1, 0);
        assert_eq!(h, (m + 4096, m));
        assert_eq!(t.1, 0);
    }

    #[test]
    fn zs1fp_the_summary_line_names_every_counter() {
        let line = summary_line();
        for key in [
            "mode=",
            "outcome=",
            "arena_mb=",
            "commits=",
            "commit_kb=",
            "huge_kb=",
            "populate_kb=",
            "populate_failed=",
            "populate_unsupported=",
            "purges=",
            "purge_kb=",
        ] {
            assert!(line.contains(key), "{key} missing: {line}");
        }
        assert!(line.starts_with("[cratonvm] mimalloc-first-touch: "));
    }

    /// In this very test binary the hook ran before `main` on Linux with
    /// mimalloc: an off switch (the default) records `off`, an on one
    /// registers the arena (the host's `CRATONVM_MIMALLOC_FIRST_TOUCH=1` run
    /// of this test is the positive control).
    #[cfg(all(feature = "mimalloc", target_os = "linux"))]
    #[test]
    fn zs1fp_the_hook_ran_before_main() {
        let o = OUTCOME.load(Ordering::Relaxed);
        assert_ne!(o, Outcome::NotRun as u8, "{}", summary_line());
        if FirstTouchMode(MODE.load(Ordering::Relaxed)).on() {
            assert_eq!(o, Outcome::Registered as u8, "{}", summary_line());
            // zs s2/rv: MORE THAN ONE commit. `mi_arena_initialize` commits
            // the arena's own meta-data through the callback before any
            // allocation, so `> 0` held even if nothing ever landed here.
            assert!(COMMITS.load(Ordering::Relaxed) > 1, "{}", summary_line());
        } else {
            assert_eq!(o, Outcome::Off as u8, "{}", summary_line());
        }
    }
}
