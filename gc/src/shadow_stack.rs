// SPDX-License-Identifier: Apache-2.0
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
        match current_thread_stack() {
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
/// `docs/internal/gc/generational-bytebuf-suite-sigsegv-hashbrown-rehash-FIXED-20260928.md`. No
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
    #[inline]
    pub unsafe fn resolve_entry(entry: usize, band: SlotBand) -> usize {
        if entry & Self::INDIRECT_TAG == 0 {
            return entry;
        }
        match Self::indirect_slot_in(entry, band) {
            // SAFETY: the caller's contract: a live frame's aligned slot,
            // inside the owner's stack.
            Some(addr) => unsafe { (addr as *const usize).read() },
            None => 0,
        }
    }
}

// SAFETY: a `ShadowStack` is exclusive to its owning thread; the raw addresses
// in `base`/`top`/`end` point into `_buf`, which is `Send`. No concurrent
// access occurs except at a stop-the-world safepoint, where the collecting
// thread reads another thread's stack only after that thread has parked.
unsafe impl Send for ShadowStack {}

impl Default for ShadowStack {
    fn default() -> Self {
        Self::empty()
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
        if self.base == 0 {
            return;
        }
        let clamped = top.clamp(self.base, self.end);
        self.top = clamped & !(std::mem::size_of::<usize>() - 1);
    }

    /// Push one oop value. Returns `false` (and drops the value) on overflow —
    /// the JIT fast path does *not* call this (it bumps inline); this is the
    /// helper/testing entry point.
    #[inline]
    pub fn push(&mut self, oop: usize) -> bool {
        if self.base == 0 || self.top >= self.end {
            return false;
        }
        // SAFETY: `top` is in `[base, end)` and 8-aligned by construction.
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
        let mut p = self.base;
        while p < limit {
            // SAFETY: `p` walks 8-aligned slots within the live
            // `[base, min(top, end))` range of the backing buffer; each holds a
            // pushed oop value or an indirect frame-slot entry. The `end` half
            // of that bound is what keeps a JIT-written `top` from taking this
            // read past the `Box`.
            let raw = unsafe { (p as *const usize).read() };
            // SAFETY: an indirect entry inside `band` names a live frame's
            // slot while it is below `top` — the pusher's contract (module
            // docs); one outside it is never read.
            f(unsafe { Self::resolve_entry(raw, band) });
            p += std::mem::size_of::<usize>();
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
        let mut p = self.base;
        while p < limit {
            // SAFETY: see `for_each_value_in`.
            let raw = unsafe { (p as *const usize).read() };
            if let Some(slot) = Self::indirect_slot_in(raw, band) {
                out.push(slot);
            }
            p += std::mem::size_of::<usize>();
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
        let mut p = self.base;
        while p < limit {
            // SAFETY: see `for_each_value_in`.
            let raw = unsafe { (p as *const usize).read() };
            // The word that holds the reference: the shadow slot itself for a
            // value entry, the frame slot for an indirect one.
            let home = if raw & Self::INDIRECT_TAG == 0 {
                Some(p)
            } else {
                Self::indirect_slot_in(raw, band)
            };
            if let Some(home) = home {
                // SAFETY: a shadow slot inside the scan range, or a live
                // frame's aligned slot inside the owner's stack (the pusher's
                // contract).
                let old = unsafe { (home as *const usize).read() };
                if let Some(&new) = pointer_map.get(&old) {
                    // Only an INDIRECT entry's home is a raw frame word; a
                    // value entry's home is this shadow stack's own buffer.
                    if raw & Self::INDIRECT_TAG != 0 && crate::root_write_audit::enabled() {
                        crate::root_write_audit::note_write(
                            crate::root_write_audit::RootWriteKind::ShadowStackHome,
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
            }
            p += std::mem::size_of::<usize>();
        }
        rewritten
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// gen r5w1: a spawned thread's stack band ends BELOW its thread-local
    /// storage. glibc puts the static TLS block at the top of the stack
    /// mapping that `pthread_getattr_np` reports, and a band that covered it
    /// let a moving collection rewrite TLS words that looked like objects.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_threads_stack_band_ends_below_its_thread_locals() {
        thread_local! {
            static MARK: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
        }
        std::thread::Builder::new()
            .stack_size(8 * 1024 * 1024)
            .spawn(|| {
                let band = current_thread_stack().expect("Linux reports a stack");
                let tls = MARK.with(|m| m as *const std::cell::Cell<usize> as usize);
                let local = 0usize;
                let local_at = std::hint::black_box(&local) as *const usize as usize;
                assert!(band.lo <= local_at && local_at < band.hi, "a local is on the band");
                assert!(band.hi <= tls, "the band stops below the TLS block");
            })
            .expect("spawn")
            .join()
            .expect("thread");
    }

    #[test]
    fn empty_is_noop() {
        let s = ShadowStack::empty();
        assert!(s.is_unallocated());
        assert_eq!(s.depth(), 0);
        let mut seen = 0;
        s.for_each_value(|_| seen += 1);
        assert_eq!(seen, 0);
        assert_eq!(s.remap(&cratonvm_types::PointerMap::default()), 0);
    }

    #[test]
    fn layout_offsets_match_jit_contract() {
        let s = ShadowStack::empty();
        let base = &s as *const ShadowStack as usize;
        assert_eq!(
            &s.top as *const usize as usize - base,
            ShadowStack::TOP_OFFSET
        );
        assert_eq!(
            &s.end as *const usize as usize - base,
            ShadowStack::END_OFFSET
        );
        assert_eq!(
            &s.base as *const usize as usize - base,
            ShadowStack::BASE_OFFSET,
            "the moving-young coverage verifier recovers the published shadow \
             window [base, top) from a live compiled frame using these offsets",
        );
    }

    /// The verifier compares a compiled frame's spill words against the values
    /// in `[base, top)`. That range is what `for_each_value` walks, so the two
    /// must agree exactly — a `top` above the pushed data would let the
    /// verifier accept an unpublished oop because a stale slot happened to
    /// hold it.
    #[test]
    fn published_window_matches_the_scanned_range() {
        let mut s = ShadowStack::empty();
        s.ensure_allocated();
        assert!(s.push(0x1000));
        assert!(s.push(0x2000));

        let mut scanned = Vec::new();
        s.for_each_value(|v| scanned.push(v));
        let window: Vec<usize> = (0..s.depth())
            .map(|i| unsafe { ((s.base + i * 8) as *const usize).read() })
            .collect();
        assert_eq!(scanned, window);
        assert_eq!(s.top, s.base + scanned.len() * 8);
    }

    /// `top` is the ONE field JIT-compiled code bumps inline, so it is the one
    /// field the collector cannot assume is in range. `set_top` has clamped
    /// since it was written, but the JIT does not go through `set_top`, so the
    /// read side has to clamp too: before it did, a `top` above `end` made
    /// `for_each_value` read past the backing `Box` and `remap` WRITE past it.
    #[test]
    fn a_top_above_end_cannot_widen_the_scan_or_the_remap() {
        let mut s = ShadowStack::empty();
        s.ensure_allocated();
        assert!(s.push(0x1000));

        // What an inline push with a defeated overflow guard leaves behind.
        s.top = s.end + 4096;

        assert_eq!(
            s.depth(),
            DEFAULT_SHADOW_SLOTS,
            "depth must saturate at the buffer, not at the corrupt top"
        );

        let mut seen = 0usize;
        s.for_each_value(|_| seen += 1);
        assert_eq!(
            seen, DEFAULT_SHADOW_SLOTS,
            "the scan must stop at `end`, not at `top`"
        );

        // The remap walks the same range and WRITES into it; the pushed value
        // is still rewritten, and nothing past `end` is touched.
        let mut map = cratonvm_types::PointerMap::default();
        map.insert(0x1000usize, 0x9000usize);
        assert_eq!(s.remap(&map), 1);
        let mut after = Vec::new();
        s.for_each_value(|v| {
            if v != 0 {
                after.push(v)
            }
        });
        assert_eq!(after, vec![0x9000]);
    }

    /// `indirect_slots` names exactly the words `remap` rewrites through —
    /// sorted, once each, value entries and popped entries excluded — because
    /// it is the list a frame walk skips so no word is rewritten twice.
    ///
    /// The "frame" is a STACK array of this test, as a compiled frame's slots
    /// are: indirect entries are trusted only inside the owner's stack.
    #[test]
    fn indirect_slots_names_the_words_remap_rewrites_through() {
        let mut frame = [0x2000usize, 0x3000, 0x4000];
        let fp = std::hint::black_box(frame.as_mut_ptr());
        let a = fp as usize;
        let b = fp as usize + 8;
        let c = fp as usize + 16;
        let mut s = ShadowStack::empty();
        assert!(s.indirect_slots().is_empty(), "unallocated: nothing to name");
        s.ensure_allocated();
        assert!(s.push(b | ShadowStack::INDIRECT_TAG));
        assert!(s.push(0x5000));
        assert!(s.push(a | ShadowStack::INDIRECT_TAG));
        assert!(s.push(b | ShadowStack::INDIRECT_TAG));
        let popped_at = s.top;
        assert!(s.push(c | ShadowStack::INDIRECT_TAG));
        s.set_top(popped_at);
        assert_eq!(s.indirect_slots(), vec![a.min(b), a.max(b)]);
        let _ = std::hint::black_box(&mut frame);
    }

    /// An indirect entry publishes the word at its slot, and a remap rewrites
    /// that word — never the entry — so the frame reads the moved address back
    /// with no copy-back of its own. A value entry beside it behaves as before.
    /// The frame is on this thread's stack, as a real compiled frame is.
    #[test]
    fn indirect_entries_resolve_and_rewrite_through_their_slot() {
        let mut frame = [0x2000usize, 0];
        let fp = std::hint::black_box(frame.as_mut_ptr());
        let slot0 = fp as usize;
        let slot1 = fp as usize + 8;
        let mut s = ShadowStack::empty();
        s.ensure_allocated();
        assert!(s.push(slot0 | ShadowStack::INDIRECT_TAG));
        assert!(s.push(0x3000));
        // A null frame slot (a reference home not yet written) is published
        // as null, which every consumer skips.
        assert!(s.push(slot1 | ShadowStack::INDIRECT_TAG));

        let mut seen = Vec::new();
        s.for_each_value(|v| seen.push(v));
        assert_eq!(seen, vec![0x2000, 0x3000, 0]);

        let mut map = cratonvm_types::PointerMap::default();
        map.insert(0x2000usize, 0x9000usize);
        map.insert(0x3000usize, 0xA000usize);
        assert_eq!(s.remap(&map), 2);
        // SAFETY: `fp` points at this test's live two-word array.
        let (w0, w1) = unsafe { (fp.read_volatile(), fp.add(1).read_volatile()) };
        assert_eq!(w0, 0x9000, "the frame slot itself is rewritten");
        assert_eq!(w1, 0, "a null slot is never a map key");

        let mut raw = Vec::new();
        for i in 0..s.depth() {
            raw.push(unsafe { ((s.base + i * 8) as *const usize).read() });
        }
        assert_eq!(
            raw,
            vec![
                slot0 | ShadowStack::INDIRECT_TAG,
                0xA000,
                slot1 | ShadowStack::INDIRECT_TAG
            ],
            "an indirect entry is never rewritten, only the slot it names"
        );
        let _ = std::hint::black_box(&mut frame);
    }

    /// THE CRASH (`ShadowOddLongProbe`, gc-common w5-g): a producer published
    /// an odd `long` in a reference home, and every reader took it for
    /// `slot | INDIRECT_TAG`. An odd value that is not a word of the owner's
    /// stack is neither dereferenced nor written by `for_each_value`, `remap`,
    /// `indirect_slots` or `resolve_entry`.
    ///
    /// Two shapes: the probe's own value (`n` = 5,924,137, unmapped: before the
    /// fix the read faulted), and a READABLE heap word, so a regression shows
    /// up as a changed word or a published `0x2000` instead of a crash.
    #[test]
    fn an_odd_primitive_outside_the_stack_is_neither_read_nor_written() {
        let heap: Box<[usize; 1]> = Box::new([0x2000]);
        let heap_ptr = std::hint::black_box(heap.as_ptr());
        let heap_odd = heap_ptr as usize | ShadowStack::INDIRECT_TAG;
        let probe_odd = 5_924_137usize;
        assert_eq!(
            ShadowStack::indirect_slot(heap_odd),
            Some(heap_ptr as usize),
            "shape alone accepts it: the band is what refuses it"
        );
        assert_eq!(ShadowStack::indirect_slot(probe_odd), Some(probe_odd - 1));

        let mut s = ShadowStack::empty();
        s.ensure_allocated();
        assert!(s.push(heap_odd));
        assert!(s.push(probe_odd));
        assert!(s.push(0x3000));

        let refused_before = rejected_indirect_entries();
        let mut seen = Vec::new();
        s.for_each_value(|v| seen.push(v));
        assert_eq!(seen, vec![0, 0, 0x3000], "published as null, not read");
        assert!(s.indirect_slots().is_empty(), "no frame word is claimed");

        let mut map = cratonvm_types::PointerMap::default();
        map.insert(0x2000usize, 0x9000usize);
        map.insert(probe_odd - 1, 0xA000usize);
        map.insert(0x3000usize, 0xB000usize);
        assert_eq!(s.remap(&map), 1, "only the value entry is rewritten");
        // SAFETY: `heap_ptr` is the live box above.
        assert_eq!(
            unsafe { heap_ptr.read_volatile() },
            0x2000,
            "remap must never write through an entry outside the owner's stack"
        );
        let band = SlotBand::current_thread();
        // SAFETY: both are refused by the band before any read.
        assert_eq!(unsafe { ShadowStack::resolve_entry(heap_odd, band) }, 0);
        assert_eq!(unsafe { ShadowStack::resolve_entry(probe_odd, band) }, 0);
        assert!(rejected_indirect_entries() > refused_before);
        drop(heap);
    }

    /// A cross-thread reader passes the PEER's band: an entry inside it
    /// resolves and remaps; the same entry against a band that excludes it
    /// (the empty band, or the reader's own stack) publishes null.
    #[test]
    fn an_explicit_band_bounds_a_cross_thread_read() {
        // Stands in for a peer's stack: readable memory that is not the
        // calling thread's stack.
        let peer: Box<[usize; 4]> = Box::new([0, 0x2000, 0, 0]);
        let peer_ptr = std::hint::black_box(peer.as_ptr());
        let peer_band = SlotBand::new(peer_ptr as usize, peer_ptr as usize + 32);
        let entry = (peer_ptr as usize + 8) | ShadowStack::INDIRECT_TAG;
        let mut s = ShadowStack::empty();
        s.ensure_allocated();
        assert!(s.push(entry));

        let mut seen = Vec::new();
        s.for_each_value_in(peer_band, |v| seen.push(v));
        assert_eq!(seen, vec![0x2000]);
        assert_eq!(s.indirect_slots_in(peer_band), vec![peer_ptr as usize + 8]);

        let mut seen = Vec::new();
        s.for_each_value_in(SlotBand::NONE, |v| seen.push(v));
        assert_eq!(seen, vec![0]);
        let mut seen = Vec::new();
        s.for_each_value(|v| seen.push(v));
        assert_eq!(seen, vec![0], "the own-thread band is not the peer's");

        let mut map = cratonvm_types::PointerMap::default();
        map.insert(0x2000usize, 0x9000usize);
        assert_eq!(s.remap(&map), 0, "not this thread's stack: skipped");
        assert_eq!(s.remap_in(&map, peer_band), 1);
        // SAFETY: `peer_ptr` is the live box above.
        assert_eq!(unsafe { peer_ptr.add(1).read_volatile() }, 0x9000);
        drop(peer);
    }

    /// `contains_slot` admits a whole word only, and the own-thread band holds
    /// this test's locals (every compiled frame of a reader's owner is, like
    /// them, a caller of the reader).
    #[test]
    fn slot_band_edges_and_the_own_thread_band() {
        let b = SlotBand::new(0x1_0000, 0x1_0040);
        assert!(b.contains_slot(0x1_0000));
        assert!(b.contains_slot(0x1_0038));
        assert!(!b.contains_slot(0x1_003C), "straddles hi");
        assert!(!b.contains_slot(0x1_0040));
        assert!(!b.contains_slot(0xFFF8));
        assert!(!SlotBand::NONE.contains_slot(0));
        assert!(!SlotBand::new(0, usize::MAX).contains_slot(usize::MAX - 3));

        let local = [0usize; 2];
        let at = std::hint::black_box(local.as_ptr()) as usize;
        let own = SlotBand::current_thread();
        assert!(own.contains_slot(at), "{own:?} must hold a caller's local {at:#x}");
        if let Some(stack) = current_thread_stack() {
            assert!(stack.lo <= own.lo && own.hi == stack.hi);
        }
    }

    /// The per-OS-thread band registry cross-thread readers look peers up in.
    #[test]
    fn thread_stack_band_registry_round_trips() {
        const TID: u32 = 0xFFFF_FD01;
        assert_eq!(thread_stack_band(TID), None);
        publish_thread_stack_band(TID);
        assert_eq!(thread_stack_band(TID), current_thread_stack());
        unpublish_thread_stack_band(TID);
        assert_eq!(thread_stack_band(TID), None);
    }

    /// A tagged word that cannot be a slot address is ignored rather than
    /// dereferenced: published as null, never rewritten through.
    #[test]
    fn a_malformed_indirect_entry_is_neither_read_nor_written() {
        assert_eq!(ShadowStack::indirect_slot(0x10), None, "untagged");
        assert_eq!(ShadowStack::indirect_slot(0x1), None, "null page");
        assert_eq!(ShadowStack::indirect_slot(0x2_0005), None, "misaligned");
        assert_eq!(ShadowStack::indirect_slot(0x2_0009), Some(0x2_0008));

        let mut s = ShadowStack::empty();
        s.ensure_allocated();
        assert!(s.push(0x5 | ShadowStack::INDIRECT_TAG));
        let mut seen = Vec::new();
        s.for_each_value(|v| seen.push(v));
        assert_eq!(seen, vec![0]);
        let mut map = cratonvm_types::PointerMap::default();
        map.insert(0x5usize, 0x9000usize);
        assert_eq!(s.remap(&map), 0);
    }

    #[test]
    fn push_scan_remap_roundtrip() {
        let mut s = ShadowStack::empty();
        s.ensure_allocated();
        assert!(!s.is_unallocated());
        assert!(s.push(0x1000));
        assert!(s.push(0x2000));
        assert!(s.push(0x3000));
        assert_eq!(s.depth(), 3);

        let mut collected = Vec::new();
        s.for_each_value(|v| collected.push(v));
        assert_eq!(collected, vec![0x1000, 0x2000, 0x3000]);

        // Relocate 0x2000 -> 0x9000.
        let mut map = cratonvm_types::PointerMap::default();
        map.insert(0x2000usize, 0x9000usize);
        assert_eq!(s.remap(&map), 1);

        let mut after = Vec::new();
        s.for_each_value(|v| after.push(v));
        assert_eq!(after, vec![0x1000, 0x9000, 0x3000]);

        // Pop semantics via set_top.
        s.set_top(s.base + 8);
        assert_eq!(s.depth(), 1);
        s.reset();
        assert_eq!(s.depth(), 0);
    }
}
