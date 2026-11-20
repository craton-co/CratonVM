// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Per-thread **shadow stack** of live object references for precise,
//! *rewritable* GC roots inside JIT-compiled code.
//!
//! # Why this exists
//!
//! A moving (Cheney) young-gen collection must update every root that points
//! at a relocated object. For interpreter frames the VM tracks exact oop
//! locations, so `update_all_roots` rewrites them. JIT-compiled frames are the
//! problem: the only root information the GC otherwise has is the
//! **conservative** native-stack scan (`conservative_roots::scan_active_jit_frames`),
//! which can *mark* (a value that looks like a heap pointer is treated as a
//! root, over-retaining at worst) but can **not rewrite** — a stack word that
//! merely *looks* like a pointer might be an `i64`, and rewriting it would
//! corrupt mutator data. Because of that the collector is forced onto a
//! non-moving young sweep whenever a JIT frame is live, which cannot drain a
//! large long-lived young set (the bintrees18 throughput wall).
//!
//! The precise-oop-map approach (RBP-chain walk + per-safepoint bytecode-PC
//! matching) is fragile and, critically, **misses operand-stack oops held in
//! callee-saved registers** across a safepoint: e.g. `make()` does
//! `aload_1 (n); … invokestatic make; putfield l` — `n` is live on the operand
//! stack across the recursive call but lives in a register, so the frame-slot
//! oop map never lists it and the move leaves the register stale.
//!
//! # The mechanism
//!
//! The shadow stack is a flat, thread-local array of **oop values**. JIT code,
//! immediately before any GC-capable call, *pushes* every live oop (locals AND
//! operand-stack entries) onto it, and immediately after the call *reloads*
//! each value from its slot (the GC may have rewritten it) and pops. Because
//! every slot is, by construction, exactly one object reference:
//!   * the GC **marks** each slot value precisely (no false positives), and
//!   * the GC **rewrites** each slot in place when its object moved — safe,
//!     because the slot is known to be an oop.
//!
//! No RBP-chain walk, no per-PC oop-map matching, no register-invisibility:
//! the live set is materialised explicitly at runtime. After the call the JIT
//! reloads from the (updated) slots, so a relocated object's new address flows
//! back into the registers the compiled code keeps using.
//!
//! # JIT contract (layout)
//!
//! `top` is at byte offset 0 so the push fast path is a `[reg+0]` load/store
//! (1 byte shorter ModR/M), mirroring [`crate::tlab::Tlab`]. `end` follows at
//! offset 8 for the overflow guard. The VM embeds a `ShadowStack` in each
//! `JvmThread` and exposes `JvmThread base + SHADOW_OFFSET + TOP_OFFSET` to the
//! codegen.
//!
//! # Indirect entries (frame blocks)
//!
//! An entry with bit 0 set ([`ShadowStack::INDIRECT_TAG`]) is not a value: it
//! is the ADDRESS of a compiled frame's slot, plus one. Every reader resolves
//! it through [`ShadowStack::resolve_entry`] — the published reference is the
//! word AT that address, and [`ShadowStack::remap`] rewrites that word in place.
//!
//! Two producers use them, under one contract — a published slot holds a
//! reference or null for as long as its entry is below `top`, and the frame it
//! names outlives the entry:
//!
//! * the optimizing tier publishes a frame's reference homes ONCE per
//!   activation (`ir_lower`'s frame block) instead of copying every live value
//!   out before each call and back after it. Its colouring keeps reference and
//!   primitive slots in disjoint pools, its prologue zeroes every reference
//!   slot, and every exit retracts `top` past the block;
//! * the single-pass tier publishes a live reference that sits in a frame slot
//!   by address for the duration of ONE call (`x64::safepoint`'s push/reload),
//!   so the post-call reload has nothing to copy back — the collector already
//!   rewrote the slot.
//!
//! A word named by an indirect entry has exactly one rewriter, [`Self::remap`]:
//! a frame walk that rewrites compiled frames' reference slots must skip the
//! words [`Self::indirect_slots`] returns, or the pointer map is applied to
//! them twice. A value is never 8-aligned plus one, so the tag cannot collide
//! with a pushed oop.
//!
//! # Frame descriptor records (rx27, `CRATONVM_JIT_IR_FRAME_BLOCK_DESCRIPTOR`)
//!
//! A frame block may instead be published as ONE variable-length record:
//!
//! ```text
//! w0       = rbp | DESCRIPTOR_TAG (2)          -- the frame's base pointer
//! w1       = DESCRIPTOR_MAGIC << 32 | n        -- n homes, n <= DESCRIPTOR_MAX_HOMES
//! w2..     = ceil(n / 4) words, four u16 each   -- home k is the word at rbp - 8 * u16[k]
//! ```
//!
//! so a body publishes its reference homes with `2 + ceil(n/4)` stores instead
//! of `n` address entries. A record is constant per compiled body, so the
//! producer normally interns it once ([`intern_descriptor`]) and publishes
//! only `w0` and `w1 = the interned record's address` (two stores); `w1` is
//! read as an address only when it names a record start inside the interned
//! table. The record names exactly the words the `n` indirect
//! entries would, under the same contract (each holds a reference or null
//! while the record is below `top`; the frame outlives it), and every reader
//! expands it through [`walk_window`], the ONE walker of a window: a reader
//! that stepped word by word would read `w0` and `w1` as values (neither is an
//! object) and lose every home. `w0` is never a value (an object is 8-aligned,
//! so bit 1 is clear) and `w1`'s magic is above any user-space address, so a
//! record is recognised only when both agree; anything else is the old
//! one-word reading. The homes are trusted only inside the owner's band, as
//! indirect entries are.
//!
//! # A tagged entry is trusted only inside the owner's stack ([`SlotBand`])
//!
//! "A value is never 8-aligned plus one" holds for a REFERENCE. It does not
//! hold for a primitive that a producer mistakenly published in a reference
//! home: an odd `long` counter is exactly "aligned plus one" half of the time.
//! Before the frame-block producers existed such a value was harmless (every
//! reader screens values through `is_object_address`); once an odd entry
//! became a POINTER, `ShadowOddLongProbe` (a compiled loop with an odd `long`
//! counter) segfaulted all three collectors, and [`Self::remap`] could WRITE
//! through the value (page `common-w4o-shadow-stack-dereferences-a-mistyped-odd-primitive`,
//! filed in `docs/known-issues/gc/` while the JIT half is open).
//!
//! So a reader dereferences an indirect entry only when the slot it names lies
//! in a [`SlotBand`] of the native stack that holds the owner's compiled
//! frames; anything else resolves to null and is never written. Two bands:
//!
//! * **Own thread** ([`SlotBand::current_thread`]): `[caller's sp, stack top)`
//!   of the CALLING OS thread. Every compiled frame of the owner is a caller
//!   of the reader, so every legitimate slot is inside it. This is what the
//!   band-less methods ([`Self::for_each_value`], [`Self::remap`],
//!   [`Self::indirect_slots`]) use, so they must only be called on the thread
//!   that owns the shadow stack — which every such caller is (safepoint
//!   snapshot, post-pause remap, blocked-wake remap, coverage verifier).
//! * **Peer** ([`thread_stack_band`]): a thread's whole stack reservation,
//!   published once per OS thread ([`publish_thread_stack_band`]) beside its
//!   shadow-window triple and keyed by the same OS tid. A cross-thread reader
//!   (`xt_root_scan`) passes it to the `*_in` methods.
//!
//! Keyed by OS thread, never recorded in the `ShadowStack`: a virtual thread's
//! `JvmThread` (and its shadow stack) remounts on whatever carrier picks it up,
//! and its compiled frames live on THAT carrier's native stack.

/// Default capacity (in 8-byte slots) of a thread's shadow stack.
///
/// This bounds the total number of live oops across *all* simultaneously
/// active JIT frames on one thread. Java stack depth is itself bounded
/// (StackOverflowError) long before a realistic method nest pushes anywhere
/// near this many oops, so 256K slots (2 MiB) is a generous ceiling that never
/// reallocates (keeping `base`/`end` stable for the JIT). Threads that never
/// run JIT code keep an empty (unallocated) shadow stack.
pub const DEFAULT_SHADOW_SLOTS: usize = 256 * 1024;

/// A band `[lo, hi)` of native stack in which an indirect entry's slot may
/// lie. See the module docs ("A tagged entry is trusted only inside the
/// owner's stack").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SlotBand {
    /// Lowest address a slot may start at (inclusive).
    pub lo: usize,
    /// One past the highest address a slot may END at.
    pub hi: usize,
}

impl SlotBand {
    /// The band that admits no slot: every indirect entry resolves to null.
    pub const NONE: SlotBand = SlotBand { lo: 0, hi: 0 };

    /// `[lo, hi)`.
    #[inline]
    pub const fn new(lo: usize, hi: usize) -> Self {
        Self { lo, hi }
    }

    /// Does the 8-byte word at `addr` lie entirely inside this band?
    #[inline]
    pub fn contains_slot(self, addr: usize) -> bool {
        addr >= self.lo && addr < self.hi && self.hi - addr >= std::mem::size_of::<usize>()
    }

    /// The live part of the CALLING OS thread's stack: from the caller's stack
    /// pointer up to the thread's stack top.
    ///
    /// Every compiled frame of a shadow stack's owner is a caller of any
    /// reader running on the owner's thread, so each of its slots is above
    /// this function's stack pointer and below the top. `#[inline(never)]` so
    /// the probe sits in this function's OWN frame, below every caller's: an
    /// inlined probe is just one local of the caller's frame, and the compiler
    /// may place the caller's other locals below it (measured: the unit test's
    /// local landed 0x90 below the probe). The band then starts at this one
    /// still-committed frame below the caller, which is only more permissive.
    ///
    /// When the platform cannot say where the stack ends, the band is bounded
    /// below only (`[sp, usize::MAX)`); when the stack pointer is not inside
    /// the reported stack (an alternate signal stack), the whole thread stack
    /// is used.
    #[inline(never)]
    pub fn current_thread() -> SlotBand {
        let probe = 0u8;
        let sp = (std::hint::black_box(&probe) as *const u8 as usize) & !0x7;
        // t1 wave 5 lane E2 (i94 stage 2, `CRATONVM_VTHREAD_SLICE_ON_CARRIER_COMPILED`):
        // inside a continuation slice run on its carrier's OS thread, the live
        // part of THIS thread's stack ends at the slice's base; the words above
        // it are the blocked carrier's (see [`current_segment_top`]).
        let seg = current_segment_top();
        match current_thread_stack() {
            Some(s) if seg != 0 && sp >= s.lo && sp < seg && seg <= s.hi => {
                SlotBand { lo: sp, hi: seg }
            }
            Some(s) if sp >= s.lo && sp < s.hi => SlotBand { lo: sp, hi: s.hi },
            Some(s) => s,
            None => SlotBand {
                lo: sp,
                hi: usize::MAX,
            },
        }
    }
}

/// The calling OS thread's whole stack `[low limit, high limit)`, or `None`
/// when the platform cannot say. Memoized per OS thread: a thread's stack
/// never moves.
pub fn current_thread_stack() -> Option<SlotBand> {
    thread_local! {
        static CACHED: std::cell::Cell<(usize, usize)> =
            const { std::cell::Cell::new((usize::MAX, 0)) };
    }
    let cached = CACHED.try_with(std::cell::Cell::get).ok();
    let (lo, hi) = match cached {
        Some(c) if c.0 != usize::MAX => c,
        _ => {
            let fresh = os_current_thread_stack().unwrap_or((0, 0));
            let _ = CACHED.try_with(|c| c.set(fresh));
            fresh
        }
    };
    (lo != 0 && hi > lo).then_some(SlotBand { lo, hi })
}

#[cfg(target_os = "windows")]
fn os_current_thread_stack() -> Option<(usize, usize)> {
    #[link(name = "kernel32")]
    extern "system" {
        fn GetCurrentThreadStackLimits(low_limit: *mut usize, high_limit: *mut usize);
    }
    let mut lo = 0usize;
    let mut hi = 0usize;
    // SAFETY: two valid out-pointers to a Win32 API that only writes the
    // calling thread's stack bounds.
    unsafe { GetCurrentThreadStackLimits(&mut lo, &mut hi) };
    Some((lo, hi))
}

#[cfg(target_os = "linux")]
fn os_current_thread_stack() -> Option<(usize, usize)> {
    /// Opaque `pthread_attr_t`: 56 bytes on glibc x86-64, 64 on aarch64; this
    /// crate does not depend on `libc`, so reserve more than either.
    #[repr(C, align(16))]
    struct Attr([u8; 128]);
    extern "C" {
        fn pthread_self() -> usize;
        fn pthread_getattr_np(thread: usize, attr: *mut Attr) -> i32;
        fn pthread_attr_getstack(
            attr: *const Attr,
            stackaddr: *mut *mut u8,
            stacksize: *mut usize,
        ) -> i32;
        fn pthread_attr_destroy(attr: *mut Attr) -> i32;
    }
    let mut attr = Attr([0u8; 128]);
    // SAFETY: the standard glibc stack-introspection sequence; `attr` is
    // initialised by `pthread_getattr_np` before it is read and destroyed on
    // the success path. The out-pointers are valid locals.
    unsafe {
        if pthread_getattr_np(pthread_self(), &mut attr) != 0 {
            return None;
        }
        let mut addr: *mut u8 = std::ptr::null_mut();
        let mut size = 0usize;
        let ok = pthread_attr_getstack(&attr, &mut addr, &mut size) == 0;
        pthread_attr_destroy(&mut attr);
        if !ok || addr.is_null() || size == 0 {
            return None;
        }
        Some(clamp_stack_top_below_static_tls(
            addr as usize,
            (addr as usize).saturating_add(size),
        ))
    }
}

/// `[lo, hi)` as `pthread_getattr_np` reported it, with `hi` lowered to the
/// bottom of this thread's static TLS block when that block lies inside the
/// range. Linux only; every other target returns the range unchanged.
///
/// gen r5w1 (orchestrator, 2026-09-26). glibc allocates a thread's static TLS
/// block and its thread descriptor (`struct pthread`, the TCB `%fs` points at)
/// at the TOP of the thread's stack mapping, and `pthread_getattr_np` reports
/// that whole mapping as the stack. So every "own stack `[sp, top)`" band --
/// the conservative JIT stack scans, the blocked-thread native-slot captures
/// and their write-back, the published peer band -- also covered the thread's
/// TLS and TCB, and a TLS word that happened to look like a heap object was
/// treated as a root and REWRITTEN by a moving collection. Measured on
/// `GenR4W6JitOomRootProbe` (Generational, `-Xmx64m`): a thread-local caching
/// the thread's own stack limits held the stack top, which was also the base
/// of the young arena mapped directly above; the blocked-peer native-slot
/// write-back relocated it (`old=<stack top> new=<to-space address>`), and the
/// thread's next own-stack scan ran ~110 MiB off its stack. The same route
/// explains the corrupted `%fs:0`, the flipped `OnceLock<bool>`, and the
/// hashbrown/`Arc`/mimalloc pointer corruptions recorded on
/// `docs/feature-designs/generative-gc-architecture.md`. No
/// frame lives above the static TLS block, so nothing a scan needs is lost.
///
/// The main thread's TLS is not on its stack, so its range never contains the
/// bound and is returned unchanged.
pub fn clamp_stack_top_below_static_tls(lo: usize, hi: usize) -> (usize, usize) {
    #[cfg(target_os = "linux")]
    {
        if let Some(tls_lo) = static_tls_low_bound() {
            let tls_lo = tls_lo & !0xf;
            // Never below a live frame: every frame of this thread is below
            // its static TLS, so a bound under the CURRENT stack pointer is
            // wrong (the thread is not on this stack, or the loader reported
            // something unexpected), and clamping to it would hide live frames
            // from every scan. Keep the reported range then.
            let sp = current_sp();
            if tls_lo > lo && tls_lo < hi && sp < tls_lo {
                return (lo, tls_lo);
            }
        }
    }
    (lo, hi)
}

/// An address inside the caller's current frame: a stack pointer good enough
/// to compare against the static TLS bound (every frame lies below it).
#[cfg(target_os = "linux")]
#[inline(never)]
fn current_sp() -> usize {
    let probe = 0u8;
    std::hint::black_box(&probe) as *const u8 as usize
}

/// The lowest address of the calling thread's static TLS block and thread
/// descriptor, or `None` when it cannot be derived.
///
/// Exact, not estimated: `dl_iterate_phdr` reports, for every loaded object
/// with a `PT_TLS` segment, `dlpi_tls_data` -- the CALLING thread's instance of
/// that segment. On x86-64 (TLS variant II) the static instances lie
/// between the stack and the thread pointer, which is the descriptor's
/// address, so the bound is the lowest instance within a window below the
/// thread pointer. On variant I targets (aarch64) the instances lie ABOVE the
/// thread pointer and none falls in the window, so the bound is the thread
/// pointer itself; the descriptor just below it stays in the band there (not
/// yet measured on aarch64). A size ESTIMATE was tried first and was wrong by a few
/// hundred bytes in both directions (glibc's surplus placement), which either
/// hid live frames or left TLS words in the band. Per thread, so not cached
/// here; every caller caches its band per thread.
#[cfg(target_os = "linux")]
fn static_tls_low_bound() -> Option<usize> {
    #[repr(C)]
    struct Elf64Phdr {
        p_type: u32,
        p_flags: u32,
        p_offset: u64,
        p_vaddr: u64,
        p_paddr: u64,
        p_filesz: u64,
        p_memsz: u64,
        p_align: u64,
    }
    /// glibc's `struct dl_phdr_info` through `dlpi_tls_data` (glibc 2.4+ for
    /// `adds`/`subs`, 2.12+ for the TLS pair); the callback's `size`
    /// argument says whether the TLS pair is present.
    #[repr(C)]
    struct DlPhdrInfo {
        dlpi_addr: u64,
        dlpi_name: *const u8,
        dlpi_phdr: *const Elf64Phdr,
        dlpi_phnum: u16,
        dlpi_adds: u64,
        dlpi_subs: u64,
        dlpi_tls_modid: usize,
        dlpi_tls_data: *mut u8,
    }
    extern "C" {
        fn pthread_self() -> usize;
        fn dl_iterate_phdr(
            callback: unsafe extern "C" fn(*mut DlPhdrInfo, usize, *mut std::ffi::c_void) -> i32,
            data: *mut std::ffi::c_void,
        ) -> i32;
    }
    const PT_TLS: u32 = 7;
    /// How far below the thread pointer a static TLS instance may lie. A
    /// dynamically allocated (non-static) instance lives in the heap, far
    /// outside this window, and is ignored.
    const WINDOW: usize = 16 * 1024 * 1024;
    struct Acc {
        tp: usize,
        lowest: usize,
    }
    unsafe extern "C" fn each(
        info: *mut DlPhdrInfo,
        size: usize,
        data: *mut std::ffi::c_void,
    ) -> i32 {
        // SAFETY: `dl_iterate_phdr` passes a valid `dl_phdr_info` of `size`
        // bytes whose `dlpi_phdr` array has `dlpi_phnum` entries; the TLS
        // fields are read only when `size` covers them. `data` is the `Acc`
        // passed below.
        unsafe {
            if size < std::mem::size_of::<DlPhdrInfo>() {
                return 0;
            }
            let info = &*info;
            let acc = &mut *data.cast::<Acc>();
            let mut has_tls = false;
            for i in 0..usize::from(info.dlpi_phnum) {
                if (*info.dlpi_phdr.add(i)).p_type == PT_TLS {
                    has_tls = true;
                }
            }
            let at = info.dlpi_tls_data as usize;
            if has_tls && at != 0 && at <= acc.tp && acc.tp - at <= WINDOW && at < acc.lowest {
                acc.lowest = at;
            }
        }
        0
    }
    // SAFETY: `pthread_self` has no preconditions.
    let tp = unsafe { pthread_self() };
    if tp == 0 {
        return None;
    }
    let mut acc = Acc { tp, lowest: tp };
    // SAFETY: `each` matches the callback ABI and only reads loader-provided
    // headers; `acc` outlives the call.
    unsafe { dl_iterate_phdr(each, (&mut acc as *mut Acc).cast()) };
    Some(acc.lowest)
}

/// No stack-introspection backend on this target (gc-common w6-g, the
/// platform arm of `common-w4o-shadow-stack-dereferences-a-mistyped-odd-primitive`).
/// What that means for each reader, stated so nobody has to re-derive it:
///
/// * Own thread: [`SlotBand::current_thread`] is `[sp, usize::MAX)`, bounded
///   below only. The owner's frames are all above `sp`, so no legitimate
///   entry is refused; a mistyped odd value above `sp` is not refused either
///   (the pre-w5 exposure, on this target only).
/// * Cross-thread: [`publish_thread_stack_band`] publishes nothing, so a peer
///   reader finds no band and resolves every indirect entry to null WITHOUT
///   reading it. It must then treat the window as untrusted, never as covered:
///   `xt_root_scan::read_peer_shadow_window_ex` answers `BandlessIndirect`,
///   the helper-window pass refuses the pin, and the take-over withholds
///   `pins_complete`. No root is dropped by that: an indirect entry names a
///   word of the peer's own stack, which the take-over's machine-stack scan
///   reads conservatively.
#[cfg(not(any(target_os = "windows", target_os = "linux")))]
fn os_current_thread_stack() -> Option<(usize, usize)> {
    None
}

/// Per-OS-thread stack bands, for cross-thread readers of a shadow stack.
///
/// A process fact (an OS thread's stack is the same for every VM in the
/// process), keyed by the OS tid the shadow-window triple is published under
/// (`gc_quiescence::publish_self_shadow_addr`), and published at the same
/// once-per-thread moment.
fn thread_stack_bands() -> &'static std::sync::RwLock<std::collections::HashMap<u32, SlotBand>> {
    static BANDS: std::sync::OnceLock<std::sync::RwLock<std::collections::HashMap<u32, SlotBand>>> =
        std::sync::OnceLock::new();
    BANDS.get_or_init(|| std::sync::RwLock::new(std::collections::HashMap::new()))
}

/// Publish the calling thread's stack band under `os_tid`. A no-op when the
/// platform cannot say where the stack is (always, on a target other than
/// Windows/Linux): a reader then treats the peer's indirect entries as
/// unresolvable and its window as untrusted (see the non-Windows/Linux
/// `os_current_thread_stack`).
pub fn publish_thread_stack_band(os_tid: u32) {
    let Some(band) = current_thread_stack() else {
        return;
    };
    thread_stack_bands()
        .write()
        .unwrap_or_else(|p| p.into_inner())
        .insert(os_tid, band);
}

/// The stack band `os_tid` published, if any.
pub fn thread_stack_band(os_tid: u32) -> Option<SlotBand> {
    thread_stack_bands()
        .read()
        .unwrap_or_else(|p| p.into_inner())
        .get(&os_tid)
        .copied()
}

/// Drop `os_tid`'s band when its thread exits (the OS recycles tids).
pub fn unpublish_thread_stack_band(os_tid: u32) {
    thread_stack_bands()
        .write()
        .unwrap_or_else(|p| p.into_inner())
        .remove(&os_tid);
}

// ---------------------------------------------------------------------------
// Stack segments (t1 wave 5 lane E2; stage 2 of
// `i94-L6-proposal-run-a-jdk-continuation-slice-on-its-carrier-os-thread`)
// ---------------------------------------------------------------------------
//
// A JDK continuation's slice may run on its carrier's own OS thread, below the
// carrier's frames, which stay on that stack GC-blocked for the slice
// (`vm::jdk_continuation::enter_special`). One OS stack then holds two
// registry threads' words: `[sp, base)` the slice's, `[base, top)` the
// carrier's, where `base` is the stack pointer at `enterSpecial`.
//
// * The OWN-thread half ([`current_segment_top`]): while the slice runs, every
//   own-stack reader of this thread (the JIT's conservative scans and A5 probe,
//   the remap store band, the in-native band, [`SlotBand::current_thread`])
//   stops at `base`, so the slice's deposits neither root nor rewrite the
//   carrier's slots (the carrier's own deposit and wake own them).
// * The CROSS-thread half ([`stack_segments_snapshot`]): a helper-window pass
//   that freezes this OS thread captures its whole band under the OS tid; the
//   blocked-peer fold routes a capture at or above `base` to the segment's
//   OWNER (the carrier's registry id) and below it to the tid's claimant (the
//   continuation), so each word is written back once, by the thread whose
//   stack it is. Published for longer than the own-thread half: until the
//   carrier has left its blocked region, i.e. after any pause whose captures
//   may name the band has folded them.
//
// Keyed by OS tid like `thread_stack_bands`: a process fact (an OS thread's
// stack belongs to no VM), not per-VM compatibility state.

thread_local! {
    /// The calling thread's current slice base, `0` outside a slice.
    static SEGMENT_TOP: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// The calling OS thread's slice base while a continuation slice runs on it
/// below its carrier's frames, else `0`. Own-stack readers bound their band's
/// top by it.
#[inline]
pub fn current_segment_top() -> usize {
    SEGMENT_TOP.try_with(std::cell::Cell::get).unwrap_or(0)
}

/// Set the calling thread's slice base (`0` clears); answers the previous one,
/// for the caller to put back.
pub fn replace_current_segment_top(top: usize) -> usize {
    SEGMENT_TOP.try_with(|c| c.replace(top)).unwrap_or(0)
}

/// `os_tid -> (base, owner registry id)` of every published segment.
fn stack_segments() -> &'static std::sync::RwLock<std::collections::HashMap<u32, (usize, u64)>> {
    static SEGMENTS: std::sync::OnceLock<
        std::sync::RwLock<std::collections::HashMap<u32, (usize, u64)>>,
    > = std::sync::OnceLock::new();
    SEGMENTS.get_or_init(|| std::sync::RwLock::new(std::collections::HashMap::new()))
}

/// Publish that `os_tid`'s stack at and above `base` belongs to registry
/// thread `owner` (the carrier). Answers the publication it replaced (none,
/// unless a carrier nests a slice, which the JDK never does), for
/// [`restore_stack_segment`].
pub fn publish_stack_segment(os_tid: u32, base: usize, owner: u64) -> Option<(usize, u64)> {
    stack_segments()
        .write()
        .unwrap_or_else(|p| p.into_inner())
        .insert(os_tid, (base, owner))
}

/// Put back what [`publish_stack_segment`] replaced (`None`: withdraw).
pub fn restore_stack_segment(os_tid: u32, prev: Option<(usize, u64)>) {
    let mut map = stack_segments().write().unwrap_or_else(|p| p.into_inner());
    match prev {
        Some(seg) => {
            map.insert(os_tid, seg);
        }
        None => {
            map.remove(&os_tid);
        }
    }
}

/// `(os_tid, base, owner)` of every published segment; empty (no allocation)
/// when no slice runs on a carrier, the default.
pub fn stack_segments_snapshot() -> Vec<(u32, usize, u64)> {
    let map = stack_segments().read().unwrap_or_else(|p| p.into_inner());
    if map.is_empty() {
        return Vec::new();
    }
    map.iter().map(|(&t, &(b, o))| (t, b, o)).collect()
}

/// Which registry thread a blocked-peer stack capture of `(os_tid, addr)`
/// belongs to, given the published `segments`: `Some(owner)` when `addr` lies
/// at or above the base of a segment published for `os_tid` (the carrier's
/// half), `None` when the OS tid's claimant owns it (no segment, or the
/// slice's half). Pure, for the fold and its tests.
pub fn segment_owner_of_capture(segments: &[(u32, usize, u64)], os_tid: u32, addr: usize) -> Option<u64> {
    segments
        .iter()
        .find(|&&(t, _, _)| t == os_tid)
        .and_then(|&(_, base, owner)| (addr >= base).then_some(owner))
}

#[cfg(test)]
mod t1e2_stack_segment_tests;

/// Refusals of an indirect entry whose slot is outside the owner's stack band
/// — a mistyped primitive published in a reference home
/// (`common-w4o-shadow-stack-dereferences-a-mistyped-odd-primitive`). Counts
/// refusals, not entries: one such entry is refused by each reader that walks
/// it (snapshot, frame-walk skip list, remap). Lifetime total; non-zero means
/// a producer published a non-reference, which is a JIT bug even though the
/// collector is now safe from it.
static REJECTED_INDIRECT_ENTRIES: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// See [`REJECTED_INDIRECT_ENTRIES`].
pub fn rejected_indirect_entries() -> u64 {
    REJECTED_INDIRECT_ENTRIES.load(std::sync::atomic::Ordering::Relaxed)
}

/// A per-thread stack of live object references, maintained by JIT-compiled
/// code around GC-capable call sites and scanned/rewritten by the collector.
///
/// Layout is `#[repr(C)]` with `top` first — see the module docs for the JIT
/// contract. The backing storage is a heap `Box<[usize]>` whose address is
/// stable for the lifetime of the thread, so `base`/`end` (and any live `top`)
/// remain valid even if the `ShadowStack` struct itself is moved.
#[repr(C)]
pub struct ShadowStack {
    /// Next free slot address (exclusive high-water of pushed oops).
    /// The GC scans `[base, top)`. JIT contract: must remain at byte offset 0.
    pub top: usize,
    /// One-past-the-last usable slot address (overflow guard).
    /// JIT contract: must remain at byte offset 8.
    pub end: usize,
    /// First slot address (inclusive). Used to bound the GC scan and to reset.
    pub base: usize,
    /// Owns the backing storage. Never read directly through this field by the
    /// JIT or GC — they use `base`/`top`/`end` raw addresses — but keeping the
    /// `Box` here ties the allocation's lifetime to the thread.
    _buf: Box<[usize]>,
}

impl ShadowStack {
    /// Byte offset of the `top` field. Read by the JIT-emitted inline push.
    pub const TOP_OFFSET: usize = 0;
    /// Byte offset of the `end` field. Read by the JIT-emitted overflow guard
    /// that both backends emit ahead of every push (`x64::emit_shadow_push`,
    /// `ir_lower::emit_shadow_push`): a push whose slots would not all fit
    /// below `end` stores nothing, leaves `top` alone, and marks its saved-base
    /// slot with bit 0 so the paired reload skips restoring homes from slots
    /// that were never written. Bailing degrades that safepoint's root
    /// publication — the runtime coverage verifier then rejects the moving-young
    /// proof — but it is bounded; storing past `end` was not.
    pub const END_OFFSET: usize = 8;
    /// Byte offset of the `base` field.
    ///
    /// Part of the same `#[repr(C)]` contract as `TOP_OFFSET`/`END_OFFSET` and
    /// asserted by `layout_offsets_match_jit_contract`. Not read by codegen —
    /// it exists so the moving-young coverage verifier
    /// (`conservative_roots::moving_young_unpublished_frame_oop_present`) can
    /// recover a thread's published shadow window `[base, top)` from a live
    /// compiled frame alone: that frame caches its `*mut JvmThread` in
    /// `[rbp - CompiledMethod::shadow_thread_slot_off]` and the `ShadowStack`
    /// sits at `thread + CompiledMethod::shadow_off_in_thread`. Without `base`
    /// the verifier could find `top` but not the start of the scan range.
    pub const BASE_OFFSET: usize = 16;

    /// Bit 0 of an entry: the entry is `slot_address | 1`, an indirect
    /// reference to a compiled-frame slot rather than a pushed value. See the
    /// module docs (`Indirect entries`).
    pub const INDIRECT_TAG: usize = 1;

    /// The address an indirect entry names, or `None` for a value entry or an
    /// indirect entry that cannot name a readable slot. Slots are 8-aligned,
    /// so anything else after removing the tag is not one this codebase
    /// pushed, and is ignored rather than dereferenced.
    #[inline]
    pub fn indirect_slot(entry: usize) -> Option<usize> {
        if entry & Self::INDIRECT_TAG == 0 {
            return None;
        }
        let addr = entry & !Self::INDIRECT_TAG;
        (addr >= 0x1_0000 && addr & 0x7 == 0).then_some(addr)
    }

    /// [`Self::indirect_slot`], and additionally inside `band` — the owner's
    /// stack. A tagged entry outside it is a mistyped primitive, not a slot:
    /// `None`, counted in [`rejected_indirect_entries`].
    #[inline]
    pub fn indirect_slot_in(entry: usize, band: SlotBand) -> Option<usize> {
        let addr = Self::indirect_slot(entry)?;
        if band.contains_slot(addr) {
            Some(addr)
        } else {
            REJECTED_INDIRECT_ENTRIES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            None
        }
    }

    /// The reference an entry publishes: the entry itself for a value entry,
    /// the word at its slot for an indirect one, and `0` (null, which every
    /// consumer already skips) for a tagged entry that names no valid slot —
    /// malformed, or outside `band` (see the module docs: a mistyped odd
    /// primitive is never dereferenced).
    ///
    /// # Safety
    ///
    /// `band` must be stack of the thread that owns the entries
    /// ([`SlotBand::current_thread`] on that thread; [`thread_stack_band`],
    /// the whole reservation, from another), and an indirect entry inside it
    /// must name a slot of a frame that is still live — the contract its
    /// pusher keeps by retracting `top` on every exit. A live frame's slot is
    /// always committed; only a mistyped value could name an uncommitted page
    /// of a peer's reservation, the residual the band cannot exclude.
    ///
    /// JIT round 21 lane stress (FUZZ21-3): a published word holding
    /// [`cratonvm_types::jit_activation::DEAD_HOME_POISON`] -- a home the
    /// optimizing tier cleared as dead under `CRATONVM_DBG_DEAD_HOME_POISON`,
    /// which a later unarmed map publishes again, as it publishes the `0` the
    /// clear writes by default -- resolves to `0` too: it is a cleared home,
    /// not a reference, and no consumer may chase it.
    #[inline]
    pub unsafe fn resolve_entry(entry: usize, band: SlotBand) -> usize {
        if entry & Self::INDIRECT_TAG == 0 {
            return Self::unpoison(entry);
        }
        match Self::indirect_slot_in(entry, band) {
            // SAFETY: the caller's contract: a live frame's aligned slot,
            // inside the owner's stack.
            Some(addr) => Self::unpoison(unsafe { (addr as *const usize).read() }),
            None => 0,
        }
    }

    /// `0` for the dead-home poison word, the word itself otherwise. See
    /// [`Self::resolve_entry`]. One compare; never true outside the debug
    /// switch, which is the only writer of the poison.
    #[inline]
    fn unpoison(word: usize) -> usize {
        Self::unpoison_word(word)
    }

    /// [`Self::unpoison`] for the module's walker.
    #[inline]
    pub fn unpoison_word(word: usize) -> usize {
        // Cast: usize is 64 bits on every target that emits the poison.
        if word as u64 == cratonvm_types::jit_activation::DEAD_HOME_POISON {
            0
        } else {
            word
        }
    }
}

// No `unsafe impl Send`: every field (`top` / `end` / `base` are `usize`,
// `_buf` is a `Box<[usize]>`) is `Send`, so the auto impl applies. A
// `ShadowStack` is exclusive to its owning thread; another thread reads it
// only at a stop-the-world safepoint, after the owner has parked. That is a
// protocol (and `Sync`-like) property, not something an `unsafe impl Send`
// could assert.

/// Bit pattern of a frame descriptor record's first word (module docs, "Frame
/// descriptor records"): `rbp | DESCRIPTOR_TAG`. Bit 0 clear (not an indirect
/// entry), bit 1 set (never an 8-aligned object).
pub const DESCRIPTOR_TAG: usize = 2;
pub use cratonvm_types::frame_descriptor::{
    descriptor_header, descriptor_record_words, intern_descriptor, DESCRIPTOR_MAGIC,
    DESCRIPTOR_MAX_HOMES,
};
use cratonvm_types::frame_descriptor::interned_descriptor;

/// Where one published reference lives, as [`walk_window`] reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryHome {
    /// A value entry: the reference is the shadow-stack word at this address.
    Shadow(usize),
    /// An indirect entry or a descriptor home: the reference is the word in a
    /// live compiled frame at this address (inside the band).
    Frame(usize),
    /// A tagged entry or record home that names no slot inside the band, or
    /// any tagged one when no band is known (`band == None`): not read.
    Unresolved,
}

/// Walk the window `[base, limit)` of 8-aligned shadow words and report every
/// published reference's home to `f`, in order: a value entry as
/// [`EntryHome::Shadow`], an indirect entry and every home of a descriptor
/// record as [`EntryHome::Frame`] when inside `band`, [`EntryHome::Unresolved`]
/// otherwise (and for every tagged entry when `band` is `None`). The ONE
/// walker of a window: every reader (this module's, the cross-thread scan,
/// the coverage verifier) goes through it, so a record cannot be skipped by
/// one of them.
///
/// # Safety
///
/// `[base, limit)` must be readable shadow-stack words, `base` and `limit`
/// 8-aligned. A `Frame` home is only reported inside `band`; reading it is the
/// caller's business under the same contract as [`ShadowStack::resolve_entry`].
pub unsafe fn walk_window(
    base: usize,
    limit: usize,
    band: Option<SlotBand>,
    mut f: impl FnMut(EntryHome),
) {
    let w = std::mem::size_of::<usize>();
    let mut p = base;
    while p < limit {
        // SAFETY: the caller's contract.
        let raw = unsafe { (p as *const usize).read() };
        if raw & 0x3 == DESCRIPTOR_TAG && p + w < limit {
            // SAFETY: inside the window, checked above.
            let w1 = unsafe { ((p + w) as *const usize).read() };
            // (homes, address of the packed words, record words in the window):
            // the inline record, or `[rbp | 2, address]` of an interned one.
            let record = match descriptor_record_words(w1) {
                Some(len) if p + len * w <= limit => {
                    Some(((w1 as u64 & 0xFFFF_FFFF) as usize, p + 2 * w, len))
                }
                Some(_) => None,
                None => interned_descriptor(w1).map(|(n, packed)| (n, packed, 2)),
            };
            if let Some((n, packed_at, len)) = record {
                let rbp = raw & !0x3;
                for k in 0..n {
                    // SAFETY: packed word k / 4 of the record, inside the
                    // window or the interned table.
                    let packed = unsafe { ((packed_at + (k / 4) * w) as *const u64).read() };
                    let v = ((packed >> (16 * (k % 4))) & 0xFFFF) as usize;
                    let slot = rbp.wrapping_sub(v * w);
                    let home = match band {
                        Some(b) if v != 0 && rbp & 0x7 == 0 && b.contains_slot(slot) => {
                            EntryHome::Frame(slot)
                        }
                        Some(_) => {
                            REJECTED_INDIRECT_ENTRIES
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            EntryHome::Unresolved
                        }
                        None => EntryHome::Unresolved,
                    };
                    f(home);
                }
                p += len * w;
                continue;
            }
        }
        if raw & ShadowStack::INDIRECT_TAG == 0 {
            f(EntryHome::Shadow(p));
        } else {
            f(match band {
                Some(b) => ShadowStack::indirect_slot_in(raw, b)
                    .map_or(EntryHome::Unresolved, EntryHome::Frame),
                None => EntryHome::Unresolved,
            });
        }
        p += w;
    }
}

/// The reference [`walk_window`] reported at `home`: the word there (with the
/// dead-home poison read as null), or null for an unresolved home.
///
/// # Safety
///
/// As [`ShadowStack::resolve_entry`]: a `Frame` home names a live frame's slot.
pub unsafe fn read_home(home: EntryHome) -> usize {
    match home {
        // SAFETY: the caller's contract.
        EntryHome::Shadow(a) | EntryHome::Frame(a) => {
            ShadowStack::unpoison_word(unsafe { (a as *const usize).read() })
        }
        EntryHome::Unresolved => 0,
    }
}

impl Default for ShadowStack {
    fn default() -> Self {
        Self::empty()
    }
}

/// gci i47w lane oe48: a published cross-thread window
/// (`gc_quiescence::publish_self_shadow_addr`) must not outlive the buffer it
/// describes. The OS thread that published it can outlive this stack's
/// `JvmThread` (a continuation runner, a carrier), and a peer reader
/// dereferences the published struct address before its identity check can
/// refuse it. See `gc_quiescence::retract_shadow_window_of_buffer`.
impl Drop for ShadowStack {
    fn drop(&mut self) {
        if self.base != 0 {
            crate::gc_quiescence::retract_shadow_window_of_buffer(self.base, self.end);
        }
    }
}

impl ShadowStack {
    /// An unallocated shadow stack (no backing storage). Pushing is impossible
    /// until [`Self::ensure_allocated`] runs; scans/remaps are no-ops.
    pub fn empty() -> Self {
        Self {
            top: 0,
            end: 0,
            base: 0,
            _buf: Vec::new().into_boxed_slice(),
        }
    }

    /// Allocate the backing buffer if this stack is still empty. Idempotent.
    /// Called lazily the first time a thread is about to enter JIT code with
    /// the shadow-stack mechanism enabled.
    pub fn ensure_allocated(&mut self) {
        if self.base != 0 {
            return;
        }
        let mut buf = vec![0usize; DEFAULT_SHADOW_SLOTS].into_boxed_slice();
        let base = buf.as_mut_ptr() as usize;
        self.base = base;
        self.top = base;
        self.end = base + DEFAULT_SHADOW_SLOTS * std::mem::size_of::<usize>();
        self._buf = buf;
    }

    /// True if no backing storage has been allocated.
    #[inline]
    pub fn is_unallocated(&self) -> bool {
        self.base == 0
    }

    /// Number of oops currently pushed.
    ///
    /// Clamped at both ends by the same rule the scan uses, so `depth()` and
    /// [`Self::for_each_value`] can never disagree about how many slots are
    /// live — the coverage verifier compares one against the other.
    #[inline]
    pub fn depth(&self) -> usize {
        if self.base == 0 || self.top < self.base {
            return 0;
        }
        // `saturating_sub`: `scan_limit` is `min(top, end)`, and a struct whose
        // `end` is below its `base` (never produced by `ensure_allocated`, but
        // reachable by field assignment) would otherwise underflow into a
        // gigantic depth — the exact shape this clamp exists to refuse.
        self.scan_limit().saturating_sub(self.base) / std::mem::size_of::<usize>()
    }

    /// Reset to empty (drop all pushed oops). Used as a defensive heal if a
    /// thread unwinds out of JIT code abnormally (e.g. exception) without the
    /// codegen having popped — see the entry/exit watermark in the VM.
    #[inline]
    pub fn reset(&mut self) {
        if self.base != 0 {
            self.top = self.base;
        }
    }

    /// Set the high-water mark (used to restore a saved watermark on JIT exit).
    /// Clamped into `[base, end]` so a corrupt value can never widen the scan
    /// range beyond the backing buffer.
    #[inline]
    pub fn set_top(&mut self, top: usize) {
        // gcj j1/cm: `end < base` (reachable by field assignment, which
        // `depth` already survives) made `Ord::clamp` PANIC here, on the JIT
        // exit watermark restore (`vm/src/jit/helpers.rs`). Refuse it instead.
        if self.base == 0 || self.end < self.base {
            return;
        }
        let clamped = top.max(self.base).min(self.end);
        self.top = clamped & !(std::mem::size_of::<usize>() - 1);
    }

    /// Push one oop value. Returns `false` (and drops the value) on overflow —
    /// the JIT fast path does *not* call this (it bumps inline); this is the
    /// helper/testing entry point.
    #[inline]
    pub fn push(&mut self, oop: usize) -> bool {
        // gcj j1/cm: `top` is the field JIT code bumps inline (see
        // `scan_limit`), so it is checked against `base` and for alignment
        // too, not only against `end`, before this writes through it.
        if self.base == 0
            || self.top >= self.end
            || self.top < self.base
            || self.top & (std::mem::size_of::<usize>() - 1) != 0
        {
            return false;
        }
        // SAFETY: `top` is in `[base, end)` and 8-aligned, checked above.
        unsafe { (self.top as *mut usize).write(oop) };
        self.top += std::mem::size_of::<usize>();
        true
    }

    /// The exclusive end of the range the GC may touch.
    ///
    /// `top` is the ONE field in this struct that JIT-compiled code bumps
    /// INLINE — no Rust runs on a push — so it is the one field the GC cannot
    /// assume is in range. [`Self::set_top`] already clamps for exactly this
    /// reason ("so a corrupt value can never widen the scan range beyond the
    /// backing buffer"), but the JIT does not go through `set_top`, so the
    /// clamp has to be re-applied on the read side. Without it a `top` above
    /// `end` makes [`Self::for_each_value`] read past the `Box` and
    /// [`Self::remap`] WRITE past it — a heap corruption originating in the
    /// collector, from one bad word.
    #[inline]
    fn scan_limit(&self) -> usize {
        self.top.min(self.end)
    }

    /// Invoke `f` with every currently-published reference, resolving indirect
    /// entries against the CALLING thread's stack ([`SlotBand::current_thread`]).
    ///
    /// Own-thread only: call it on the thread whose compiled frames the
    /// entries name. A cross-thread reader uses [`Self::for_each_value_in`]
    /// with the peer's [`thread_stack_band`].
    #[inline]
    pub fn for_each_value(&self, f: impl FnMut(usize)) {
        self.for_each_value_in(SlotBand::current_thread(), f);
    }

    /// [`Self::for_each_value`] with an explicit owner stack band: an
    /// indirect entry naming a slot outside `band` publishes null and is
    /// never dereferenced.
    #[inline]
    pub fn for_each_value_in(&self, band: SlotBand, mut f: impl FnMut(usize)) {
        if self.base == 0 {
            return;
        }
        let limit = self.scan_limit();
        // SAFETY: `[base, min(top, end))` is the live range of the backing
        // buffer (the `end` half keeps a JIT-written `top` from taking the
        // walk past the `Box`); a `Frame` home inside `band` names a live
        // frame's slot while it is below `top` (module docs).
        unsafe {
            walk_window(self.base, limit, Some(band), |home| f(read_home(home)));
        }
    }

    /// The frame words named by the indirect entries in `[base, top)`, sorted
    /// and deduplicated — the words [`Self::remap`] rewrites through. A frame
    /// walk rewriting the same frames must leave exactly these alone (module
    /// docs, "Indirect entries"). Own-thread only, like [`Self::remap`], and
    /// bounded by the same band so the two lists agree.
    pub fn indirect_slots(&self) -> Vec<usize> {
        self.indirect_slots_in(SlotBand::current_thread())
    }

    /// [`Self::indirect_slots`] with an explicit owner stack band.
    pub fn indirect_slots_in(&self, band: SlotBand) -> Vec<usize> {
        let mut out = Vec::new();
        if self.base == 0 {
            return out;
        }
        let limit = self.scan_limit();
        // SAFETY: see `for_each_value_in`.
        unsafe {
            walk_window(self.base, limit, Some(band), |home| {
                if let EntryHome::Frame(slot) = home {
                    out.push(slot);
                }
            });
        }
        out.sort_unstable();
        out.dedup();
        out
    }

    /// After a moving collection, rewrite every pushed slot whose value names a
    /// relocated object (`pointer_map[old] = new`). This is the precise,
    /// *rewritable* root update that conservative scanning cannot do — every
    /// slot here is known to be an oop, so the rewrite is unconditionally safe.
    /// An indirect entry is rewritten THROUGH: the frame slot it names gets the
    /// new address, and the entry itself is left alone. Returns the number of
    /// slots rewritten (for diagnostics).
    ///
    /// Own-thread only: indirect entries are bounded by the CALLING thread's
    /// stack ([`SlotBand::current_thread`]); one naming anything else — a
    /// mistyped odd primitive — is skipped, never written through.
    pub fn remap(&self, pointer_map: &cratonvm_types::PointerMap) -> usize {
        self.remap_in(pointer_map, SlotBand::current_thread())
    }

    /// [`Self::remap`] with an explicit owner stack band.
    pub fn remap_in(&self, pointer_map: &cratonvm_types::PointerMap, band: SlotBand) -> usize {
        if self.base == 0 || pointer_map.is_empty() {
            return 0;
        }
        let mut rewritten = 0usize;
        let limit = self.scan_limit();
        // Frame words already rewritten through an indirect entry or a
        // descriptor home: two names of the same frame slot must rewrite it
        // ONCE (a second pass is wrong for a chained map, `old -> new` where
        // `new` is also a key). Allocates only when a frame home exists.
        let mut rewritten_homes: rustc_hash::FxHashSet<usize> = rustc_hash::FxHashSet::default();
        let mut visit = |home: usize, frame: bool| {
            if frame && !rewritten_homes.insert(home) {
                return;
            }
            // SAFETY: a shadow slot inside the scan range, or a live frame's
            // aligned slot inside the owner's stack (the pusher's contract).
            let old = unsafe { (home as *const usize).read() };
            if let Some(&new) = pointer_map.get(&old) {
                // Only a frame home is a raw frame word; a value entry's home
                // is this shadow stack's own buffer.
                if frame && crate::root_write_audit::enabled() {
                    crate::root_write_audit::note_write(
                        crate::root_write_audit::RootWriteKind::ShadowStackHome,
                        band,
                        home,
                        old,
                        new,
                        true,
                    );
                }
                // SAFETY: same word, rewriting a known oop in place.
                unsafe { (home as *mut usize).write(new) };
                rewritten += 1;
            }
        };
        // SAFETY: see `for_each_value_in`.
        unsafe {
            walk_window(self.base, limit, Some(band), |home| match home {
                EntryHome::Shadow(a) => visit(a, false),
                EntryHome::Frame(a) => visit(a, true),
                EntryHome::Unresolved => {}
            });
        }
        rewritten
    }
}

#[cfg(test)]
mod tests;

/// JIT round 21 lane stress (FUZZ21-3): the dead-home poison is a cleared
/// home, published as null whether it arrives as a value entry or through the
/// frame slot an indirect entry names, and never a remap key.
#[cfg(test)]
mod dead_home_poison_tests;

/// rx27 (frame descriptor records): every in-crate reader expands a record.
#[cfg(test)]
mod rx27_descriptor_record_tests;
