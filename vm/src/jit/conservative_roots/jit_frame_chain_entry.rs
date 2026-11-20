#[allow(unused_imports)]
use super::*;


// ---------------------------------------------------------------------------
// Thread-local active JIT entry chain
// ---------------------------------------------------------------------------

/// NEW-12: one entry in the per-thread JIT call chain.
///
/// Every active JIT call pushes one of these at entry and pops it at
/// exit. The `entry_sp` field is the stack pointer captured at the
/// moment of the push — the conservative fallback scanner walks the
/// native stack between the current SP and this value to find any
/// qword that looks like a heap address.
///
/// The `precise` field is `Some(PreciseFrameInfo)` when the caller
/// registered a compiled method that has populated precise oop maps
/// (NEW-12). In that case the root walker can enumerate oops exactly
/// via [`JitFrameChainEntry::precise_oops`] instead of doing a blind
/// range scan. When `precise` is `None`, the entry is conservative:
/// the walker still reads every qword in the spill region and
/// validates via `heap.is_object_address`.
///
/// Both modes produce a **superset** of the real oops (false positives
/// are filtered at heap validation time) so GC correctness is
/// guaranteed regardless of which path runs.
#[derive(Clone, Copy)]
pub(crate) struct JitFrameChainEntry {
    /// Stack pointer captured at JIT entry. Used as the upper bound of
    /// the conservative scan for this frame.
    pub entry_sp: usize,
    /// Optional precise-frame metadata. When `Some`, the walker uses
    /// the compiled method's oop map at the current native PC to
    /// enumerate oops directly from frame slots.
    pub precise: Option<PreciseFrameInfo>,
    /// How many *interpreter* frames this thread had when the entry was
    /// pushed — i.e. where this compiled frame sits in the Java call stack.
    ///
    /// A compiled method executes without pushing a `runtime::frame::Frame`,
    /// so `Thread.getStackTrace()` (and every throwable's trace) used to skip
    /// it entirely: once H2's query chain warmed up, an ALIAS function saw a
    /// 6-frame stack where the interpreter showed 21, and
    /// `org.h2.test.db.TestIndex.testFunctionIndex` — whose whole assertion is
    /// that an `org.h2.command.query.Select` frame is visible from inside the
    /// function — failed. `capture_full_trace` interleaves the chain back in
    /// using this number.
    ///
    /// [`u32::MAX`] means "inherit the enclosing entry's": a JIT→JIT dispatch
    /// (`try_call_compiled_entry_reentrant`) is called *from* compiled code and
    /// has no `JvmThread` to ask, but no interpreter frame can have been pushed
    /// since the entry it nests inside, so the enclosing depth is exact.
    pub interp_depth: u32,
    /// The `GLOBAL_JIT_DEPTH` stripe this entry's push incremented, filled by
    /// [`push_entry_full`]. The pop decrements exactly that stripe, so an entry
    /// released during thread teardown cannot land on stripe 0 and cancel a
    /// live peer's depth. Constructors pass `StripeToken::UNSET`.
    pub depth_stripe: cratonvm_types::striped_counter::StripeToken,
    /// The executable-code quiescence token this entry's push took, filled by
    /// [`push_entry_full`] and handed back to `jit_execution_leave` by the pop.
    /// Constructors pass `JitExecutionToken::UNSET`.
    pub exec_token: cratonvm_jit::JitExecutionToken,
    /// Whether this entry's push skipped the diagnostic
    /// `gc_quiescence::ENTER_COUNT` tally (the door form,
    /// `CRATONVM_JIT_DOOR_THREAD_BLOCK`). Filled by [`push_entry_form`]; every
    /// pop, prune and segment retire picks `leave` / `leave_uncounted` from it,
    /// so the pair stays balanced whichever form pops the entry (t1 wave 9,
    /// lane JITA; `t1-I25-door-form-quiescence-tallies-drift-on-mixed-pops`).
    /// Constructors pass `false`.
    pub tally_skipped: bool,
}

/// Sentinel for [`JitFrameChainEntry::interp_depth`]: resolve at push time from
/// the entry this one nests inside.
pub(crate) const INTERP_DEPTH_INHERIT: u32 = u32::MAX;

/// NEW-12: metadata needed to walk a JIT frame with precise oop maps.
///
/// Stored inline in [`JitFrameChainEntry`] when a caller opts into
/// precise root enumeration via [`JitEntryGuard::enter_with_compiled`].
#[derive(Clone, Copy)]
pub(crate) struct PreciseFrameInfo {
    /// Raw pointer to the [`cratonvm_jit::CompiledMethod`] whose code
    /// is currently executing in this frame. The pointer is borrowed
    /// — callers guarantee the CompiledMethod outlives the JIT call,
    /// which holds trivially because the guard is scoped to a single
    /// call and the caller owns an `&CompiledMethod` for its duration.
    ///
    /// Dereferencing this pointer at GC time is safe because:
    ///   1. The JIT cache holds an owning `Arc<CompiledMethod>` for
    ///      the duration of every compiled method's registration, so
    ///      the CM cannot be dropped while a call is in flight.
    ///   2. The chain entry is popped the moment the JIT call returns
    ///      or unwinds — there is no stale-pointer window.
    pub compiled_method: *const cratonvm_jit::CompiledMethod,
    /// Base address of the frame (the RBP value captured at the
    /// start of the prologue). Oop-map slot offsets are added to
    /// this value to obtain the absolute address of each oop slot.
    ///
    /// Also captured via [`current_stack_pointer`] like `entry_sp`;
    /// in practice the two are within a handful of bytes of each
    /// other because the guard is constructed immediately before the
    /// call transferring control to compiled code. The walker uses
    /// `frame_base` for oop-slot computation and `entry_sp` for
    /// bounding the conservative fallback when no map matches.
    pub frame_base: usize,
    /// Base address of the compiled method's entry point, cached so
    /// the walker can compute `current_pc - entry_ptr = offset` and
    /// look up the matching oop map entry.
    pub entry_ptr: *const u8,
    /// Stage 3/5 (precise relocation) — the EXACT RBP of the *innermost*
    /// active JIT frame, recorded by [`set_top_frame_base`] from the deepest
    /// prologue's `frame_record` helper. Used by precise slot scans for OSR
    /// boundary frames and as the start of the `remap_active_jit_frames`
    /// RBP-chain walk. It is kept SEPARATE from
    /// `frame_base` (the Rust-guard SP captured at entry) because the marking
    /// path [`scan_one_frame_precise`] uses `frame_base` as the UPPER bound of
    /// its conservative sweep `[scanner_sp, frame_base)`: clobbering it with
    /// the (low) innermost RBP shrank that sweep to just the innermost frame,
    /// dropping every ancestor frame's spilled oops → live objects reclaimed →
    /// heap corruption. `0` until the prologue records it (gate off).
    pub exact_rbp: usize,
    /// The compile id published by the frame standing at [`Self::exact_rbp`],
    /// captured in the SAME breath as that RBP.
    ///
    /// Both halves are written together by generated code, but they are only a
    /// matched pair at the instant they are read off the mirrors. Reading the
    /// id later — at scan time — is wrong twice over: the owning thread may
    /// have entered and left other compiled frames since, and the scan often
    /// runs on the COLLECTOR's thread, whose mirrors describe its own stack and
    /// not the one being walked. (Measured: reading it at scan time left the
    /// obligation in 785 of 786 cycles, i.e. it never once resolved.) So it is
    /// snapshotted here, beside the RBP it belongs to, and every consumer takes
    /// it from this struct. `0` = nothing published for that frame.
    pub exact_cm_id: u32,
    /// While `jit_not_entrant_entry_body` re-dispatches a call the not-entrant
    /// stub intercepted, the return address of that call (the stub builds no
    /// frame, so it is the innermost frame's resume pc); `0` otherwise.
    pub not_entrant_ret_pc: usize,
}

// SAFETY: the raw pointers in JitFrameChainEntry are not dereferenced
// without additional validation (CompiledMethod is Arc-owned by the JIT
// cache; the chain is thread-local so no cross-thread access). Marking
// the struct Send+Sync enables storage in the thread-local RefCell,
// which cargo clippy otherwise flags.
unsafe impl Send for JitFrameChainEntry {}
unsafe impl Sync for JitFrameChainEntry {}

/// Stack of entries captured at each active JIT call. The top of
/// the stack is the *innermost* JIT call (most recent).
///
/// Under the NEW-12 refactor the chain holds [`JitFrameChainEntry`]
/// structs instead of raw stack pointers. Entries whose `precise`
/// field is `None` retain the NEW-1 conservative-scan semantics;
/// entries with `precise = Some` are enumerated via the compiled
/// method's oop maps at GC time. A field of [`DOOR_BLOCK`].
pub(in crate::jit::conservative_roots) const JIT_ENTRY_CHAIN: DoorField<RefCell<Vec<JitFrameChainEntry>>> = DoorField(|b| &b.chain);

thread_local! {
    /// Memoizes how deep the "unregistered JIT frame above the tracked chain"
    /// scan (see the A5-fix block in `scan_active_jit_frames`) has already
    /// verified clean: `(verified_lo, code_range_count_at_verification)`.
    ///
    /// That scan checks `[search_lo, stack_high)` for a stray JIT return
    /// address on every `update_root_snapshot` call. `search_lo` tracks the
    /// current (thread-local) native stack pointer, which only DECREASES as
    /// this thread recurses deeper — so once a range `[verified_lo,
    /// stack_high)` has been scanned and found clean, any LATER call whose
    /// `search_lo >= verified_lo` needs only a subset of that same range:
    /// nothing above our current stack pointer can change while we are
    /// nested below it (that memory belongs to still-waiting caller frames),
    /// so the "clean" verdict still holds and the scan can be skipped.
    ///
    /// The one way the verdict COULD go stale is a method compiling (OSR or
    /// otherwise) between the two checks: a stack slot that held a plain
    /// (non-JIT) value at the first scan could later be sitting where a
    /// *new* JIT code range now claims to start. Guard against that by also
    /// storing `jit_code_range_count()` at verification time and requiring
    /// it be unchanged — any new compilation invalidates the memo and forces
    /// a fresh scan. Reset to `(usize::MAX, 0)` so the very first check
    /// always scans.
    ///
    /// 2026-07-17: the same "nothing above our current stack pointer can
    /// change while nested below it" argument also covers the opposite
    /// direction — recursing DEEPER (`search_lo < verified_lo`) with an
    /// unchanged `code_range_count`. The once-verified `[verified_lo,
    /// stack_high)` band is unaffected by descending further below it, so
    /// `scan_active_jit_frames` only needs to scan the new incremental band
    /// `[search_lo, verified_lo)` in that case, not the whole
    /// `[search_lo, stack_high)` range again. See the caller for the
    /// rationale and the throughput evidence that motivated it (a
    /// perpetually-growing recursion depth, as in Hibernate/JUnit5's nested
    /// call chains, previously paid a full stack rescan on every single
    /// per-native-call root snapshot).
    pub(in crate::jit::conservative_roots) static UNREG_JIT_MEMO: std::cell::Cell<UnregMemo> =
        const { std::cell::Cell::new(UnregMemo::new()) };

    /// H2-CID0 (2026-08-06) — is the NEXT unregistered-frame scan on this thread
    /// one a collector will actually consume?
    ///
    /// Set by `invalidate_scan_cache_for_gc` (the four GC-authoritative sites)
    /// and cleared by the scan that follows. Only the audit reads it, and only
    /// to attribute a suppression to the path where suppression is a defect
    /// rather than the intended optimisation.
    pub(in crate::jit::conservative_roots) static UNREG_JIT_AUTHORITATIVE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };


}

/// Perf mirror of the CURRENT top chain entry's `exact_rbp`. The hot
/// per-invocation `set_top_frame_base` (called once per JIT method entry —
/// ~1.8B times for fib44) writes ONLY this `Cell` (a single TLS store, no
/// `RefCell` borrow / `Vec::last_mut` / Option matching), cutting the
/// per-call cost. It is synced with the chain at the rare push/pop/retain
/// boundaries and flushed into the top entry before GC marking/remap reads
/// `exact_rbp`. Invariant:
/// `TOP_RBP` == the live `exact_rbp` of `JIT_ENTRY_CHAIN.last()` (or 0 when
/// the chain is empty / the top is not yet precise). A field of [`DOOR_BLOCK`].
pub(in crate::jit::conservative_roots) const TOP_RBP: DoorField<std::cell::Cell<usize>> = DoorField(|b| &b.top_rbp);

/// Step 1 (`docs/feature-designs/precise-jit-maps-default.md`, inline
/// frame-record) — read/write the innermost-RBP mirror.
///
/// When inline frame-record is active (`cratonvm_jit::x64::inline_rbp_tls_disp()
/// != 0`), generated code stores RBP directly into an OS TLS slot with one
/// segment-relative instruction (`gs:` on Windows, `fs:` on Linux); these
/// accessors read/write that SAME slot so cold push/pop/prune/remap paths
/// observe the inlined writes. When unavailable, they fall back to the Rust
/// `thread_local! TOP_RBP` exactly as before. The displacement and mirror
/// accessors are shared with JIT codegen, so the two cannot disagree on the
/// slot.
#[inline]
pub(in crate::jit::conservative_roots) fn top_rbp_get() -> usize {
    #[cfg(windows)]
    {
        let disp = cratonvm_jit::x64::inline_rbp_tls_disp();
        if disp != 0 {
            // SAFETY: `disp` was validated by the startup sentinel probe in
            // `inline_rbp_tls_disp()` to be a live, 8-byte-aligned TEB TLS slot
            // present on every thread.
            return unsafe { read_gs_qword(disp) };
        }
    }
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    if let Some(value) = cratonvm_jit::x64::inline_rbp_tls_mirror_read() {
        return value;
    }
    TOP_RBP.with(|c| c.get())
}

/// Step 1 self-check (`CRATONVM_DBG_VERIFY_INLINE_FRAME_RECORD`) — public read
/// of the innermost-RBP mirror, used by `jit_verify_inline_frame_record` to
/// confirm the inlined store landed in the slot the GC reads.
pub fn top_rbp_mirror_read() -> usize {
    top_rbp_get()
}

/// Save/restore hook for Rust-side compiled-entry calls
/// (`try_call_compiled_entry` in `vm/src/jit/helpers.rs`): a compiled callee's
/// prologue publishes ITS rbp into the mirror and nothing on the Rust dispatch
/// path restores the JIT caller's value when the callee returns. A GC
/// triggered from any later helper call would then start its precise remap
/// walk at the DEAD callee frame (whose stack memory has been reused by
/// subsequent Rust frames); a garbage "parent rbp"/"return address" pair that
/// happens to resolve into registered JIT code gets its "oop slots" rewritten
/// with relocated pointers — observed as heap addresses appearing inside
/// double[] elements during Arrays.sort (ES SortingDigestTests, -Jit on).
/// The dispatch helpers snapshot the mirror before the raw entry call and
/// write it back afterwards. The JIT→JIT direct-call analogue is
/// `emit_post_call_rbp_republish` in `jit/src/x64.rs`.
pub fn top_rbp_mirror_write(v: usize) {
    top_rbp_set(v);
}

/// Save/restore of the IDENTITY half of the frame record, for the same bracket
/// as [`top_rbp_mirror_write`].
///
/// The two mirrors are written together by generated code, and that pairing is
/// what lets a conservative scan name the method owning the innermost RBP. A
/// bracket that restored only the RBP would leave the pair naming two different
/// frames — the caller's rbp beside the callee's identity — and the scan cannot
/// detect that, because both halves would still read consistently out of the
/// mirrors. So this is not an optimisation: restoring one without the other is
/// how the identity becomes actively wrong rather than merely absent.
pub fn top_cm_id_mirror_read() -> u32 {
    published_compile_id()
}

/// See [`top_cm_id_mirror_read`]. A no-op when no identity slot is active.
pub fn top_cm_id_mirror_write(id: u32) {
    #[cfg(windows)]
    {
        let disp = cratonvm_jit::x64::inline_cm_tls_disp();
        if disp != 0 {
            // SAFETY: `disp` was validated by the startup sentinel probe in
            // `inline_cm_tls_disp()` — a live, 8-byte-aligned TEB TLS slot on
            // every thread, exactly as for the RBP mirror.
            unsafe { write_gs_qword(disp, id as usize) };
            return;
        }
    }
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        if cratonvm_jit::x64::inline_cm_tls_mirror_write(id) {
            return;
        }
    }
    let _ = id;
}

#[inline]
pub(in crate::jit::conservative_roots) fn top_rbp_set(v: usize) {
    #[cfg(windows)]
    {
        let disp = cratonvm_jit::x64::inline_rbp_tls_disp();
        if disp != 0 {
            // SAFETY: see `top_rbp_get`.
            unsafe { write_gs_qword(disp, v) };
            return;
        }
    }
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    if cratonvm_jit::x64::inline_rbp_tls_mirror_write(v) {
        return;
    }
    TOP_RBP.with(|c| c.set(v));
}

/// Read the 8-byte value at `gs:[disp]` (Windows TEB-relative TLS slot).
#[cfg(windows)]
#[inline]
pub(in crate::jit::conservative_roots) unsafe fn read_gs_qword(disp: usize) -> usize {
    let val: usize;
    core::arch::asm!(
        "mov {out}, qword ptr gs:[{addr}]",
        out = out(reg) val,
        addr = in(reg) disp,
        options(nostack, preserves_flags, readonly),
    );
    val
}

/// Write an 8-byte value to `gs:[disp]` (Windows TEB-relative TLS slot).
#[cfg(windows)]
#[inline]
pub(in crate::jit::conservative_roots) unsafe fn write_gs_qword(disp: usize, val: usize) {
    core::arch::asm!(
        "mov qword ptr gs:[{addr}], {val}",
        addr = in(reg) disp,
        val = in(reg) val,
        options(nostack, preserves_flags),
    );
}

/// Process-wide counter of active JIT entries across all threads. Lets the GC
/// quickly answer "is anyone in JIT?" without crossing thread boundaries.
///
/// Striped per thread — it is written twice per interpreter/JIT boundary
/// crossing and read only by the GC, so as one shared `AtomicUsize` it was a
/// contended cache line on the hottest path in the VM. See
/// [`cratonvm_types::striped_counter`].
///
/// Folded (t1 wave 8, lane I25; `CRATONVM_JIT_DOOR_ONE_DEPTH_COUNTER`): a door
/// entry pushed in the one-counter form ([`JitEntryGuard::enter_door`]) is
/// counted once in `cratonvm_types::striped_counter::DOOR_DEPTH` and not here;
/// `get` / `is_zero` add it back.
pub(in crate::jit::conservative_roots) static GLOBAL_JIT_DEPTH: cratonvm_types::striped_counter::DoorFoldedCounter =
    cratonvm_types::striped_counter::DoorFoldedCounter::new();

/// `CRATONVM_XT_PINNED_PEER_DEPTH` -- credit a frozen peer's JIT depth to the
/// cross-thread coverage account when this cycle PINNED that peer's entire
/// stack, instead of refusing the cycle because the peer never parked to prove
/// its own frames rewritable.
///
/// Default ON since 2026-09-04 (the body says why); `=0` is the kill switch,
/// so the two behaviours are one binary apart and the comparison is an A/B
/// rather than a rebuild. (This line said "Default OFF" until gc-common w1-c.)
/// See
/// [`refresh_moving_young_coverage_for_collection`] for the argument, and
/// `cratonvm_gc::gc_quiescence::add_xt_cycle_pinned_jit_depth` for why the
/// per-thread depth is published from here rather than from the (much colder)
/// blocked-region transition.
pub fn xt_pinned_peer_depth_enabled() -> bool {
    static ON: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    // DEFAULT ON since 2026-09-04 -- see `helper_window_discharge_enabled` for
    // the measurement. `CRATONVM_XT_PINNED_PEER_DEPTH=0` is the kill switch.
    //
    // This credit was suspected of causing the SIGSEGV on that class for most
    // of its life, and it did not: the crash was the compaction slide writing
    // into a decommitted granule (`Arena::ensure_committed_span`). The credit
    // only ever raised the amount of compaction, and compaction is what runs
    // slides.
    cratonvm_jit::memo_flag::memo_bool(&ON, || {
        !matches!(
            Err::<String, std::env::VarError>(std::env::VarError::NotPresent).as_deref(),
            Ok("0") | Ok("false") | Ok("off") | Ok("no")
        )
    })
}

/// `CRATONVM_XT_PINNED_PEER_PUBLISH_ONLY=1` -- publish per-thread depths and
/// deposit them, but credit NOTHING to the coverage account.
///
/// The bisect lever for the segfault this feature produced on
/// `TestCachedQueryResults`: with one flag gating both the publisher (a TLS
/// `Arc` written on every JIT chain mutation) and the decision (relocating on
/// cycles that used to refuse), a crash cannot be attributed to either. This
/// arm runs the whole mechanism EXCEPT the decision, so a crash here indicts
/// the publisher and a crash only without it indicts the relocation.
pub fn xt_pinned_peer_publish_only() -> bool {
    static ON: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&ON, || {
        false    })}

/// `CRATONVM_XT_PEER_SHADOW_SCAN` -- scan (and, for a helper window, pin) a
/// frozen peer's SHADOW STACK, not just its registers and machine stack.
/// Default ON; `=0` is the kill switch.
///
/// A JIT frame's oops live in the shadow stack, a per-thread heap allocation
/// the helper-window scan cannot see. Without this the pinned-peer depth credit
/// claims coverage it does not have, and the class SIGSEGVs.
///
/// Since gc-common w1-c (2026-09-23) the same switch also governs the
/// TAKE-OVER pass's read of a peer frozen IN compiled code
/// (`xt_root_scan::scan_taken_over_peer_shadow`), which until then read only
/// registers and machine stack -- so an oop an outer compiled frame parked in
/// its shadow slot across a call into a spinning callee was in no root set.
pub fn xt_peer_shadow_scan_enabled() -> bool {
    static ON: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    // DEFAULT ON since 2026-09-04. Without it the pin is incomplete by
    // construction: a JIT frame's oops live in the shadow stack, a per-thread
    // HEAP allocation that the register+stack scan cannot see, and a blocked
    // peer's is scanned by nothing and remapped by nothing. Measured at 2090
    // heap refs recovered on a 216 s run that registers and stack missed
    // entirely. `CRATONVM_XT_PEER_SHADOW_SCAN=0` is the kill switch.
    cratonvm_jit::memo_flag::memo_bool(&ON, || {
        !matches!(
            Err::<String, std::env::VarError>(std::env::VarError::NotPresent).as_deref(),
            Ok("0") | Ok("false") | Ok("off") | Ok("no")
        )
    })
}

thread_local! {
    /// The `(ShadowStack address, base)` this OS thread last published, or
    /// `(0, 0)` before the first publish.
    pub(in crate::jit::conservative_roots) static SHADOW_ADDR_PUBLISHED: std::cell::Cell<(usize, usize)> =
        const { std::cell::Cell::new((0, 0)) };
}

/// Publish the shadow stack of the `JvmThread` entering JIT code on this OS
/// thread. Called from `set_jit_thread`, which runs on every interpreter->JIT
/// entry, so the repeat path is one TLS compare.
///
/// Republished whenever a DIFFERENT shadow stack enters (gc-common w5-g).
/// This used to publish once per OS thread, which is wrong on a virtual-thread
/// carrier: a carrier is a Rust thread with no `JvmThread` of its own, so the
/// first shadow stack published on it was the first virtual thread it mounted,
/// and every later mount's compiled frames were read through THAT thread's
/// window -- a parked thread's stale `[base, top)`, or, once it died, a freed
/// `JvmThread` (the identity check then reads freed memory before refusing).
/// A take-over of a carrier frozen in a later virtual thread's compiled code
/// missed every value that thread held only in its own shadow stack.
pub fn publish_self_shadow_addr_once(addr: usize, base: usize, end: usize) {
    // `base == 0` means `ensure_allocated` has not run yet; publishing that
    // would register a window with no buffer. Stay unpublished (the reader
    // treats absent as UNKNOWN and refuses) and try again on the next entry.
    if base == 0 {
        return;
    }
    let _ = SHADOW_ADDR_PUBLISHED.try_with(|c| {
        let last = c.get();
        if last == (addr, base) {
            return;
        }
        let os_tid = self_os_tid();
        if last == (0, 0) {
            // The stack band first, under the same tid: a peer reader resolves
            // this thread's indirect shadow entries only inside it (gc-common
            // w5-g, `ShadowOddLongProbe`). Published before the window so a
            // reader that finds the window also finds the band. An OS thread's
            // stack never moves, so once is enough.
            cratonvm_gc::shadow_stack::publish_thread_stack_band(os_tid);
        }
        cratonvm_gc::gc_quiescence::publish_self_shadow_addr(os_tid, addr, base, end);
        c.set((addr, base));
    });
}

/// This thread's OS tid, in the same namespace `blocked_os_tids` reports and
/// `helper_window_pass` enumerates -- the key the initiator will look this
/// thread's depth up by.
#[cfg(windows)]
pub(in crate::jit::conservative_roots) fn self_os_tid() -> u32 {
    unsafe extern "system" {
        fn GetCurrentThreadId() -> u32;
    }
    unsafe { GetCurrentThreadId() }
}

#[cfg(all(unix, target_os = "linux"))]
pub(in crate::jit::conservative_roots) fn self_os_tid() -> u32 {
    // SAFETY: `SYS_gettid` takes no arguments and cannot fail.
    unsafe { libc::syscall(libc::SYS_gettid) as u32 }
}

#[cfg(not(any(windows, all(unix, target_os = "linux"))))]
pub(in crate::jit::conservative_roots) fn self_os_tid() -> u32 {
    // No helper-window scanner on this platform, so nothing ever reads the
    // slot; 0 keeps every thread on the never-registered path.
    0
}

/// Owns this thread's registry entry so it is REMOVED when the thread exits.
///
/// A leftover entry is readable under a tid the OS will recycle, and a recycled
/// thread that never enters JIT never overwrites it -- so the initiator would
/// credit depth nobody holds. `register_self_jit_depth_slot` also resets on
/// re-registration, which covers the threads whose destructors never run.
pub(in crate::jit::conservative_roots) struct JitDepthSlot {
    pub(in crate::jit::conservative_roots) os_tid: u32,
    pub(in crate::jit::conservative_roots) cell: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl Drop for JitDepthSlot {
    fn drop(&mut self) {
        cratonvm_gc::gc_quiescence::unregister_jit_depth_slot(self.os_tid);
        // `unregister_jit_depth_slot` drops the shadow-window triple; the
        // stack band published beside it goes with it. (A thread that never
        // registered a depth slot leaves its band behind; the next thread the
        // OS gives that tid republishes both at its first JIT entry, and a
        // band is never read without a window that passes the identity check.)
        cratonvm_gc::shadow_stack::unpublish_thread_stack_band(self.os_tid);
        // gch h3/fd (Y3): re-arm the shadow publication latch, so a slot
        // dropped at thread teardown does not leave the next publish skipped.
        let _ = SHADOW_ADDR_PUBLISHED.try_with(|c| c.set((0, 0)));
    }
}

/// This thread's published JIT-depth cell, resolved once. `None` until the
/// first chain mutation with the credit enabled. A field of [`DOOR_BLOCK`].
pub(in crate::jit::conservative_roots) const SELF_JIT_DEPTH_SLOT: DoorField<std::cell::RefCell<Option<JitDepthSlot>>> =
    DoorField(|b| &b.depth_slot);

/// Publish this thread's current `JIT_ENTRY_CHAIN` length so a GC initiator
/// that freezes this thread can attribute a depth to it.
///
/// One TLS read and one relaxed store on a path that already does both; the
/// registry lock is taken only on this thread's FIRST call. Gated so the
/// default arm pays a single cached-bool branch.
#[inline]
pub(in crate::jit::conservative_roots) fn publish_self_jit_depth(depth: usize) {
    if !xt_pinned_peer_depth_enabled() {
        return;
    }
    // `try_with`, not `with`: `pop_jit_entry` can run while this thread is
    // tearing down, and `with` on an already-destroyed thread-local PANICS.
    // A skipped publish only under-credits.
    let _ = SELF_JIT_DEPTH_SLOT.try_with(|c| publish_depth_into(c, depth));
}

/// [`publish_self_jit_depth`] with the slot in hand (the chain push and pop,
/// which already hold the [`DoorBlock`]).
#[inline]
pub(in crate::jit::conservative_roots) fn publish_self_jit_depth_in(c: &std::cell::RefCell<Option<JitDepthSlot>>, depth: usize) {
    if xt_pinned_peer_depth_enabled() {
        publish_depth_into(c, depth);
    }
}

#[inline]
pub(in crate::jit::conservative_roots) fn publish_depth_into(c: &std::cell::RefCell<Option<JitDepthSlot>>, depth: usize) {
    // `try_borrow_mut`: this runs on the JIT entry/exit path, which a panic
    // unwind can re-enter. Skipping a publish is safe (the initiator then
    // reads a stale-SMALLER depth and under-credits, refusing a cycle it
    // could have run); a double-borrow panic here would not be.
    let Ok(mut slot) = c.try_borrow_mut() else {
        return;
    };
    let held = slot.get_or_insert_with(|| {
        let os_tid = self_os_tid();
        JitDepthSlot {
            os_tid,
            cell: cratonvm_gc::gc_quiescence::register_self_jit_depth_slot(os_tid),
        }
    });
    held.cell.store(depth, std::sync::atomic::Ordering::Release);
}


/// Whether the JIT **shadow-stack** precise-roots mechanism is enabled
/// (`CRATONVM_SHADOW_STACK`). Cached on first read.
///
/// When on, JIT codegen pushes live oops onto each thread's
/// [`cratonvm_gc::shadow_stack::ShadowStack`] around GC-capable safepoints, the
/// marking root scan folds those values into the root set, the post-move remap
/// rewrites them in place, and the young-gen collector is permitted to run the
/// *moving* (Cheney) cycle even while JIT frames are live (see
/// `gen_heap.rs` quiescence gate). The standalone shadow-stack knob remains
/// opt-in; the default moving-young configuration enables the mechanism.
///
/// The original 2026-06-22 standalone experiment was incomplete. Moving-young
/// now publishes complete oop homes and uses a per-cycle coverage proof; an
/// incomplete frame diverts the collection to the non-moving sweep.
#[inline]
pub fn shadow_stack_enabled() -> bool {
    static ENABLED: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    // `CRATONVM_MOVING_YOUNG` implies the shadow-stack root scan + remap: the
    // moving young gen relies on the complete precise map the shadow stack now
    // publishes (see `moving_young_enabled`).
    //
    // This formula MUST stay identical to the emission side,
    // `jit::x64::shadow_stack_maps_enabled`, or the collector walks a shadow
    // stack the codegen never pushed to. Both were tried scoped to
    // `JIT_PUBLISHES_RELOCATION_CONTRACT` and both were reverted together: the
    // scoping measured as no-change (see that function's comment), and moving
    // one side of this agreement is not worth doing for an unmeasurable win.
    // `CRATONVM_JIT_MY_SHADOW_EMISSION=0` drops the moving-young implication on
    // BOTH sides at once; see `jit::x64::shadow_emission_moving_implication_enabled`.
    // This expression must stay character-for-character equivalent to the
    // emission side's.
    //
    // 2026-07-31 — the `moving_young_enabled() &&` term is REMOVED, on BOTH
    // sides in the same change. Publication is how a JIT frame's live
    // references become visible to the collector at all; that is not a
    // property of which young collector runs. Keyed on moving-young, the
    // documented opt-out `CRATONVM_NO_MOVING_YOUNG=1` withdrew it, and that
    // lane faulted on a zeroed heap slot within seconds of real work.
    // Restoring publication with `CRATONVM_SHADOW_STACK=1` and changing
    // nothing else made the same runs clean. See
    // `jit-no-moving-young-opt-out-unpublishes-roots-CLOSED-20260803.md`.
    cratonvm_jit::memo_flag::memo_bool(&ENABLED, || {
        false
            // gci i9/ng: the same parse as the emission side (`licm.rs`
            // `runtime_flag_default_on`), so `=off` / `=no` turn both sides off.
            || true
    })
}

/// Whether the **moving / compacting young generation** is in effect.
///
/// When on: (1) the JIT publishes a *complete* rewritable precise root map at
/// each safepoint (see `cratonvm_jit::x64::moving_young_enabled`), (2) the
/// conservative JIT-frame scan in `roots.rs` is SUPPRESSED (a fully-precise frame
/// needs no conservative backstop, and mixing a conservatively-marked slot with a
/// precisely-relocated object would corrupt), and (3) `gen_heap` runs the moving
/// (Cheney) young collection even while JIT frames are live instead of diverting
/// to the non-moving sweep.
///
/// # This is an INTERLOCK, not an independent policy decision (arch-2026-07-26)
///
/// This used to be its own `None::<std::ffi::OsString>` read —
/// the second of **three** copies of the same predicate, alongside
/// `cratonvm_jit::x64::moving_young_enabled` (codegen) and
/// `cratonvm_gc::gc_quiescence::moving_young_enabled` (collector). Three
/// independent reads of one safety-critical switch mean the default cannot be
/// flipped safely: flipping the collector without the codegen relocates objects
/// whose only home is a JIT register that no shadow push ever recorded, and
/// flipping the root gatherer without the codegen suppresses the conservative
/// backstop with nothing precise replacing it.
///
/// The effective gate is the **conjunction** of two questions that are answered
/// in different crates:
///
///   * *Can* we move? — `cratonvm_jit::x64::moving_young_enabled()`. Only the
///     codegen can physically emit the shadow push/reload and the
///     per-safepoint `moving_young_coverage_complete` bit. If it says no,
///     nothing anywhere else can make relocation safe.
///   * *Should* we move? — `cratonvm_types::flags().gc.moving_young`, the typed
///     config (opt-OUT `CRATONVM_NO_MOVING_YOUNG` over
///     `flags::DEFAULT_MOVING_YOUNG`).
///
/// AND-ing them is fail-safe in both directions. The centralized x64 projection
/// and this check make `CRATONVM_NO_MOVING_YOUNG` authoritative even when a
/// stale `CRATONVM_MOVING_YOUNG` compatibility variable is also present.
///
/// The result is then published to the GC crate, which cannot call into the JIT
/// crate (`cratonvm-gc` has no `cratonvm-jit` dependency, only a
/// dev-dependency). So the collector always relocates against the same answer
/// the codegen compiled for, and no skew between the three layers is
/// representable. The shipped default is now the single `true` constant in
/// `cratonvm_types::flags::DEFAULT_MOVING_YOUNG`.
#[inline]
pub fn moving_young_enabled() -> bool {
    use std::sync::OnceLock;
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        // `x64::moving_young_enabled` is itself a `OnceLock`, and `flags()`
        // latches on first use, so this resolves once for the process.
        let on =
            cratonvm_jit::x64::moving_young_enabled() && cratonvm_types::flags().gc.moving_young;
        // Hand the collector the same answer. Before this publish the GC uses
        // `gc_flags().moving_young` on its own, which is only reachable in a
        // process with no JIT at all — where there are no JIT frames and moving
        // is unconditionally safe.
        cratonvm_gc::gc_quiescence::publish_moving_young_enabled(on);
        on
    })
}

/// Re-publish the moving-young decision to the GC crate.
///
/// Cheap (one store behind an already-resolved `OnceLock`) and called from the
/// root gatherer so the collector's view is refreshed on the path of every
/// collection, even if the first `moving_young_enabled()` call happened on a
/// different thread than the one that will collect.
#[inline]
pub fn publish_moving_young_gate() {
    let _ = moving_young_enabled();
}

/// Returns true when moving-young must fall back to the conservative
/// JIT-frame scan + non-moving sweep because an active OSR artifact cannot
/// prove rewritable shadow coverage for this collection.
pub fn moving_young_osr_shadow_fallback_needed() -> bool {
    false
}

/// Does the OSR coverage check read the SHADOW aggregate
/// (`CompiledMethod::fully_shadow_covered`) rather than the frame-slot subset
/// (`fully_oop_covered`)?
///
/// Default ON, so a KILL SWITCH: `CRATONVM_OSR_COVERAGE_SHADOW=0` (or
/// `off`/`false`/`no`) restores the frame-slot reading, which is the bisect for
/// anything that appears with this on. Read per call rather than latched, so a
/// test can set it without deciding the answer for every later test in the
/// binary; the call site runs once per live chain entry per collection.
/// spring-bug-10 experiment (`CRATONVM_SHADOW_PIN`): when set, the shadow-stack
/// marking publish (`memory/roots.rs`) treats shadow oops as PINNED rather than
/// MOVABLE — keeping them alive without the evacuate→remap→reload path. Cached.
#[inline]
pub fn shadow_pin_roots() -> bool {
    static ENABLED: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&ENABLED, || false)}

/// Capture the current native stack pointer.
///
/// Implemented as the address of a probe variable that **must** live in the
/// caller's frame, not in a separate callee frame that immediately gets torn
/// down. Hence `#[inline(always)]`: when the function is inlined, `probe`
/// becomes a local of the caller and `&probe` is a pointer into the caller's
/// frame. If we forbade inlining, the probe would live in *this* function's
/// frame, the function would return, the frame would be deallocated, and the
/// recorded SP would point into freed stack memory — which is exactly the bug
/// we're trying to avoid.
#[inline(always)]
pub fn current_stack_pointer() -> usize {
    let probe: u8 = 0;
    // `&probe` forces `probe` to take an address, which forces it onto the
    // (caller's, after inlining) stack rather than living in a register.
    &probe as *const u8 as usize
}

/// The current thread's stack HIGH limit (one past the highest usable stack
/// address). Bounds the A5 unregistered-JIT-frame scan and the
/// `CRATONVM_DBG_FULLSTACK_SCAN` diagnostic.
///
/// Windows: Win32 `GetCurrentThreadStackLimits`. Linux (A5 port, GC audit
/// 2026-07-10 INT-3 follow-up): `pthread_getattr_np` + `pthread_attr_getstack`
/// — `stack_addr` is the LOWEST address, so high = addr + size. glibc answers
/// for the initial thread too (it derives the main stack extent from
/// /proc/self/maps + RLIMIT_STACK), but that derivation is not free, so the
/// result is memoized per thread — a thread's stack top never changes.
/// Returns 0 on failure; callers treat 0 as "unknown" and skip the scan
/// (safe degradation, matching the pre-port non-Windows behaviour).
#[cfg(any(target_os = "windows", target_os = "linux"))]
pub(in crate::jit::conservative_roots) fn current_thread_stack_high() -> usize {
    current_thread_stack_limits().1
}

/// The current thread's stack `(low, high)` limits, `(0, 0)` when unknown.
/// Memoized per thread (a thread's stack never moves).
///
/// gen r5w1/young5: the LOW limit is kept too, so a reader can check that the
/// stack pointer it is about to scan up from actually lies on this thread's
/// stack (see [`own_stack_top`] and [`sp_on_own_stack`]).
///
/// gcj j2/x1 (`gcj-j2x1-wave-j1-default-on-review-residuals` item 7): macOS
/// answers too ([`current_thread_stack_limits_uncached`]'s Darwin arm), so the
/// own-stack readers below know their top there; before, every licensing
/// young-pin deposit with a non-empty chain read as incomplete on macOS and
/// the pinned copy never engaged while compiled frames were live.
#[cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]
pub(in crate::jit::conservative_roots) fn current_thread_stack_limits() -> (usize, usize) {
    thread_local! {
        static CACHED_LIMITS: std::cell::Cell<(usize, usize)> =
            const { std::cell::Cell::new((usize::MAX, usize::MAX)) };
    }
    let cached = CACHED_LIMITS.with(std::cell::Cell::get);
    let limits = if cached.1 != usize::MAX {
        cached
    } else {
        let limits = current_thread_stack_limits_uncached();
        CACHED_LIMITS.with(|c| c.set(limits));
        limits
    };
    segment_bounded_limits(limits, cratonvm_gc::shadow_stack::current_segment_top())
}

/// t1 wave 5 lane E2 (i94 stage 2, `CRATONVM_VTHREAD_SLICE_ON_CARRIER_COMPILED`):
/// the stack `limits` as this thread's own-stack readers see them inside a
/// continuation slice run on its carrier's OS thread -- the top lowered to the
/// slice's base `seg` ([`ChainSegment`]), so the A5 probe, the chain-band
/// scans, `in_native_band_top` and the remap store band stop below the
/// carrier's frozen frames. `seg == 0` (no slice: always, with the switch off)
/// or a base outside the limits leaves them unchanged. Pure.
#[inline]
#[cfg_attr(
    not(any(target_os = "windows", target_os = "linux", target_os = "macos")),
    allow(dead_code)
)]
pub(crate) fn segment_bounded_limits(limits: (usize, usize), seg: usize) -> (usize, usize) {
    if seg != 0 && seg > limits.0 && seg < limits.1 {
        (limits.0, seg)
    } else {
        limits
    }
}

#[cfg(target_os = "windows")]
pub(in crate::jit::conservative_roots) fn current_thread_stack_limits_uncached() -> (usize, usize) {
    #[link(name = "kernel32")]
    extern "system" {
        fn GetCurrentThreadStackLimits(low_limit: *mut usize, high_limit: *mut usize);
    }
    let mut low: usize = 0;
    let mut high: usize = 0;
    // SAFETY: passes two valid out-pointers to a well-known Win32 API that
    // only writes the thread's stack bounds; no other effect.
    unsafe { GetCurrentThreadStackLimits(&mut low, &mut high) };
    if high == 0 || low >= high {
        return (0, 0);
    }
    (low, high)
}

#[cfg(target_os = "linux")]
pub(in crate::jit::conservative_roots) fn current_thread_stack_limits_uncached() -> (usize, usize) {
    // SAFETY: standard glibc stack-introspection sequence. `attr` is
    // initialized by pthread_getattr_np before any read, and destroyed on
    // every successful-init path. The out-pointers are valid locals.
    unsafe {
        let mut attr: libc::pthread_attr_t = std::mem::zeroed();
        if libc::pthread_getattr_np(libc::pthread_self(), &mut attr) != 0 {
            return (0, 0);
        }
        let mut lo: *mut libc::c_void = std::ptr::null_mut();
        let mut size: libc::size_t = 0;
        let ok = libc::pthread_attr_getstack(&attr, &mut lo, &mut size) == 0;
        libc::pthread_attr_destroy(&mut attr);
        if !ok || lo.is_null() || size == 0 {
            return (0, 0);
        }
        // gen r5w1: the reported range ends at the stack MAPPING's end, which
        // holds this thread's static TLS and descriptor; scanning (and a
        // moving collection rewriting) those words corrupted TLS. See
        // `cratonvm_gc::shadow_stack::clamp_stack_top_below_static_tls`.
        cratonvm_gc::shadow_stack::clamp_stack_top_below_static_tls(
            lo as usize,
            (lo as usize).saturating_add(size),
        )
    }
}

/// gcj j2/x1: Darwin's `pthread_get_stackaddr_np` answers the stack's HIGH
/// end (it grows down) and `pthread_get_stacksize_np` its size, for the main
/// thread too. No static TLS sits at the top of a Darwin thread stack (TLS
/// lives in `__thread_data` / a separate allocation), so no clamp is needed.
/// A size the platform under-reports leaves the current SP below `low`, which
/// [`own_stack_top`] answers as unknown (the pre-port behaviour).
#[cfg(target_os = "macos")]
pub(in crate::jit::conservative_roots) fn current_thread_stack_limits_uncached() -> (usize, usize) {
    // SAFETY: both calls only read the calling thread's own pthread record.
    let (high, size) = unsafe {
        let me = libc::pthread_self();
        (
            libc::pthread_get_stackaddr_np(me) as usize,
            libc::pthread_get_stacksize_np(me),
        )
    };
    if high == 0 || size == 0 || size > high {
        return (0, 0);
    }
    (high - size, high)
}

/// Is `sp` on this thread's own stack, as far as the platform can say?
///
/// `true` when the limits are unknown (nothing to check against, the
/// historical behaviour) or `sp` lies in `[low, high)`. `false` means the
/// thread is running on memory that is NOT the stack its limits describe — an
/// SP that is stale or corrupt, or a stack switch — and an own-stack scan that
/// starts at `sp` and runs up to the thread's stack top would cross whatever
/// lies between (gen r5w1/young5: the Finalizer-thread SIGSEGV in
/// `native_stack_has_jit_frame`, a page-aligned fault ~16 MiB above RSP).
#[inline]
#[cfg_attr(
    not(any(target_os = "windows", target_os = "linux", target_os = "macos")),
    allow(dead_code)
)]
pub(crate) fn sp_on_own_stack(sp: usize) -> bool {
    #[cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]
    {
        let (low, high) = current_thread_stack_limits();
        high == 0 || (sp >= low && sp < high)
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
    {
        let _ = sp;
        true
    }
}

/// Own-stack reads refused because the current SP was not on the thread's
/// own stack ([`sp_on_own_stack`]). Zero on a healthy run; a non-zero value
/// names a thread running on a stale or foreign stack pointer.
pub static OWN_STACK_FOREIGN_SP: AtomicUsize = AtomicUsize::new(0);

/// This thread's stack top for bounding an own-stack scan, or `0` when the
/// platform cannot say (every caller then falls back to the fixed cap).
///
/// gen r5w1/young5: when the current SP is NOT inside the thread's known
/// stack, the top is reported UNKNOWN (`0`), so every [`own_stack_scan_hi`]
/// bound falls back to the fixed 8 MiB cap instead of walking from a foreign
/// SP all the way up to a stack top that is not above it. Unknown rather than
/// "nothing readable": these reads are also the conservative ROOT scans, and
/// a limit that is merely misreported must not silently drop a live frame's
/// roots — the cap is exactly what every scan read before the C1 fix. Callers
/// that must not read a capped band as a proof check [`sp_on_own_stack`]
/// themselves and fail closed (the A5 coverage probe and the A5 root scan do).
#[inline]
pub(in crate::jit::conservative_roots) fn own_stack_top() -> usize {
    #[cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]
    {
        let sp = current_stack_pointer();
        let (low, high) = current_thread_stack_limits();
        if high != 0 && (sp < low || sp >= high) {
            OWN_STACK_FOREIGN_SP.fetch_add(1, Ordering::Relaxed);
            return 0;
        }
        high
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
    {
        0
    }
}

/// The cap an own-stack scan falls back to when the band cannot be proved to
/// lie inside this thread's stack. The pre-2026-09-23 value, unchanged.
pub(in crate::jit::conservative_roots) const OWN_STACK_SCAN_FALLBACK_CAP: usize = 8 * 1024 * 1024;

/// The exclusive upper bound an own-thread stack scan of `[lo, hi)` may read to.
///
/// gen r4w2/youngmark (2026-09-23), C1 of
/// `docs/feature-designs/generative-gc-architecture.md`.
/// [`scan_one_frame`], [`native_stack_has_jit_frame`] and its census used to
/// clamp every band to `lo + 8 MiB` — i.e. keep the BOTTOM of the band. Every
/// band they are asked for (the conservative chain band `[scanner_sp,
/// conservative_high)`, the precise-entry whole-band fallback, the
/// unregistered-frame span) has the scanner at its low end and the compiled
/// frames at its HIGH end, so a collection more than 8 MiB below the outermost
/// compiled frame dropped exactly that frame's slots, and the A5 probe then
/// reported a band it had not read as clean.
///
/// When `lo` is at or above the current stack pointer and `hi` at or below the
/// thread's real stack top, the whole band lies in this thread's live,
/// committed stack — the property the cap existed to approximate — so it is
/// read in full. Anything else (unknown platform limits, a `hi` above the
/// stack top, i.e. a stale entry SP) keeps the old cap.
///
/// gen r5w1/young5: and a KNOWN stack top bounds every read, the fallback
/// included. The fallback used to be `hi.min(lo + 8 MiB)` alone, which this
/// doc called "the read-safety direction" — it is not when the top is known:
/// a stale `hi` above the top with `lo` within 8 MiB of it read straight off
/// the top of the stack, into the next mapping's guard page. A `lo` at or
/// above a known top reads nothing (the result is then `<= lo`).
#[inline]
pub(in crate::jit::conservative_roots) fn own_stack_scan_hi(lo: usize, hi: usize, sp_now: usize, stack_top: usize) -> usize {
    if stack_top != 0 && lo >= sp_now && hi <= stack_top {
        return hi;
    }
    let capped = hi.min(lo.saturating_add(OWN_STACK_SCAN_FALLBACK_CAP));
    if stack_top != 0 {
        capped.min(stack_top)
    } else {
        capped
    }
}

/// Would an own-stack read of `[lo, hi)` bounded by [`own_stack_scan_hi`] (as
/// `native_stack_has_jit_frame` and `scan_one_frame` bound it) stop short of
/// `hi`? Then a probe that found nothing has NOT verified the band.
///
/// gen r4w5/pinwords5: the C1 fallback of
/// `docs/feature-designs/generative-gc-architecture.md`.
///
/// `#[inline(never)]`: the stack pointer it compares against must be taken in
/// a frame BELOW the caller's, as the probe's own is — inlined, it could land
/// above a `lo` the caller took from its own frame and report a truncation the
/// probe did not suffer (harmless, but a false "not verified").
#[inline(never)]
pub(in crate::jit::conservative_roots) fn own_stack_band_truncated(lo: usize, hi: usize) -> bool {
    let aligned = (lo + 7) & !7usize;
    hi > aligned && own_stack_scan_hi(aligned, hi, current_stack_pointer(), own_stack_top()) < hi
}

/// This thread's JIT entry-chain history: the returned-frame residue mark
/// (`JIT_RESIDUE_HI` before gce e1/j, see [`ChainHistory::residue_hi`]),
/// the chain's mutation count and the frozen-band memo of the in-native
/// deposit ([`ChainHistory::frozen`]). A field of [`DOOR_BLOCK`].
pub(in crate::jit::conservative_roots) const JIT_CHAIN_HISTORY: DoorField<std::cell::Cell<ChainHistory>> = DoorField(|b| &b.history);

/// See [`JIT_CHAIN_HISTORY`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::jit::conservative_roots) struct ChainHistory {
    /// Highest `entry_sp` among the JIT entries that have RETURNED on this
    /// thread -- an upper bound on the stack addresses whose current contents
    /// may be a returned frame's leftovers rather than anything live.
    ///
    /// A compiled frame writes only below its own `entry_sp`, so once it
    /// returns, every return address it left into JIT code lies below that
    /// mark. `scan_active_jit_frames`'s unregistered-frame probe uses this to
    /// tell residue from a genuinely live guardless frame; see its call site.
    ///
    /// Monotonic. A frame entered at `sp` writes only below `sp`, so it never
    /// overwrites residue at or above `sp`; resetting the mark on a push would
    /// discard exactly the higher-addressed leftovers of an earlier, shallower
    /// frame. The one live guardless frame the probe exists for is the process
    /// entry point, which sits above every JIT entry the run ever makes.
    pub(in crate::jit::conservative_roots) residue_hi: usize,
    /// gce e1/j: every push, pop and prune of `JIT_ENTRY_CHAIN` bumps it
    /// ([`note_chain_mutation`]). Equal across two reads = the chain was not
    /// touched in between.
    pub(in crate::jit::conservative_roots) mutations: u64,
    pub(in crate::jit::conservative_roots) root_mutations: u64,
    /// gce e1/j: a JNI in-native deposit is running on this thread
    /// ([`with_frozen_band_memo`]); only then is [`Self::frozen`] read or
    /// written.
    pub(in crate::jit::conservative_roots) in_native_deposit: bool,
    /// gce e1b/j: the VM's JNI phase census while an in-native deposit runs
    /// with it on (null otherwise), so the coverage proof can time its parts.
    /// Valid for the deposit: the census lives in the VM's `NativeRealm`,
    /// which the depositing thread's dispatch keeps alive.
    pub(in crate::jit::conservative_roots) census: *const crate::native::jni::JniPhaseCensus,
    /// gce e1/j: the last clean verdict of the unregistered-frame probe over
    /// the band above the chain, recorded by an in-native deposit, and whether
    /// the RAW probe was clean (`true`) or only its classification (a residue
    /// hit the coverage proof's filter discards). See [`frozen_band_clean`].
    pub(in crate::jit::conservative_roots) frozen: Option<(FrozenBand, bool)>,
    /// gch h4/bp (`CRATONVM_JNI_IN_NATIVE_BAND_AT_PAUSE`): the JNI in-native
    /// deposit running on this thread leaves its compiled band to the pause
    /// ([`with_band_at_pause_deposit`]): the coverage proof answers "unproven"
    /// and the conservative chain scan publishes nothing.
    pub(in crate::jit::conservative_roots) band_at_pause: bool,
    /// gci i24/bp (`CRATONVM_JNI_IN_NATIVE_PROVEN_DEPTH_CREDIT`, opt-in): inside
    /// a band-at-pause deposit, run the coverage proof after all
    /// ([`with_band_at_pause_proof`]), so the deposit can record the depth it
    /// proved for the pause to credit. The chain scan still publishes nothing.
    pub(in crate::jit::conservative_roots) band_at_pause_prove: bool,
    /// gci i23/r6: the collector's own moving-young coverage proof is running
    /// on this thread ([`with_collector_proof`]); the generation-keyed memo
    /// stays disarmed for it ([`frozen_band_memo_armed`]).
    pub(in crate::jit::conservative_roots) collector_proof: bool,
    /// zt t32: this thread is running VM code with NO Java frame on its
    /// stack ([`with_no_java_on_stack`]: the reference-delivery thread
    /// idling between batches). Its JIT scan then covers nothing.
    pub(in crate::jit::conservative_roots) no_java: bool,
    /// zwr: every compiled frame this thread runs sits below a registered
    /// chain entry ([`with_chain_covered_java`]: the reference-delivery
    /// thread running a batch). Its unregistered-frame probe judges residue.
    pub(in crate::jit::conservative_roots) chain_covered_java: bool,
    /// zwr: the address of the delivery thread's
    /// `GcBlockState::delivery_rust_floor` while [`with_chain_covered_java`]
    /// runs (`0` otherwise): the outermost chain entry's `entry_sp` is
    /// published there ([`publish_delivery_rust_floor`]).
    pub(in crate::jit::conservative_roots) delivery_floor_cell: usize,
    /// gci i23/r6: unit tests only -- `Some(on)` stands in for
    /// `CRATONVM_JIT_UNREG_MEMO_GENERATION_KEYED` on this thread, so both arms
    /// of [`frozen_band_memo_armed`] are testable in one process.
    #[cfg(test)]
    pub(in crate::jit::conservative_roots) keyed_for_test: Option<bool>,
}

impl ChainHistory {
    pub(in crate::jit::conservative_roots) const fn new() -> Self {
        ChainHistory {
            residue_hi: 0,
            mutations: 0,
            root_mutations: 0,
            in_native_deposit: false,
            census: std::ptr::null(),
            frozen: None,
            band_at_pause: false,
            band_at_pause_prove: false,
            collector_proof: false,
            no_java: false,
            chain_covered_java: false,
            delivery_floor_cell: 0,
            #[cfg(test)]
            keyed_for_test: None,
        }
    }
}

/// gce e1/j: the key of a clean unregistered-frame verdict over
/// `[cover_hi, high)`, the band above the outermost JIT entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::jit::conservative_roots) struct FrozenBand {
    pub(in crate::jit::conservative_roots) mutations: u64,
    pub(in crate::jit::conservative_roots) cover_hi: usize,
    pub(in crate::jit::conservative_roots) high: usize,
    pub(in crate::jit::conservative_roots) code_generation: u64,
}

/// Record that a JIT entry with this `entry_sp` has returned.
pub(in crate::jit::conservative_roots) fn note_jit_residue(entry_sp: usize) {
    JIT_CHAIN_HISTORY.with(|c| {
        let mut h = c.get();
        h.residue_hi = h.residue_hi.max(entry_sp);
        c.set(h);
    });
}

/// Upper bound on this thread's returned-JIT-frame residue, or 0 when no JIT
/// frame has returned yet.
pub(in crate::jit::conservative_roots) fn jit_residue_hi() -> usize {
    JIT_CHAIN_HISTORY.with(|c| c.get().residue_hi)
}

/// gce e1/j: `JIT_ENTRY_CHAIN` was pushed, popped or pruned.
#[inline]
pub(in crate::jit::conservative_roots) fn note_chain_mutation() {
    JIT_CHAIN_HISTORY.with(|c| {
        let mut h = c.get();
        h.mutations = h.mutations.wrapping_add(1);
        c.set(h);
    });
}

// ---------------------------------------------------------------------------
// The frozen band of a JNI in-native deposit (gce e1/j)
// ---------------------------------------------------------------------------
//
// `docs/feature-designs/native-dispatch-architecture.md`,
// `gce-e1j-in-native-deposit-rescans-the-whole-stack-above-the-jit-chain-20260929.md`.
//
// With the JNI in-native package (`CRATONVM_JNI_NATIVE_TRANSITIONS=1`) every
// JNI native CALL deposits the thread's roots (`deposit_root_snapshot`), and
// that deposit reset `UNREG_JIT_MEMO` (`invalidate_scan_cache_for_gc`) and
// then read the WHOLE stack above the outermost JIT entry, `[cover_hi,
// stack top)`, word by word for a return address into compiled code -- once
// in `refresh_moving_young_coverage_for_current_thread` (Generational) and
// once in `scan_active_jit_frames`' A5 block. That band is every interpreter
// and Rust frame of the thread above its compiled code: tens of KiB for a
// shallow `main`, hundreds for a JUnit stack. It is the stack-proportional
// part of `Gcd1JniCostProbe noop`'s 6.5 us.
//
// Across the calls of a compiled loop into a native, that band is FROZEN. The
// outermost chain entry's `JitEntryGuard` frame sits at `cover_hi`; while that
// entry is on the chain the thread runs only below it, so no `call` writes a
// return address above it, and a frame above it can only be a suspended one
// that was already there. Callees may still write data into a suspended
// caller's frame through a pointer, and a new code range may make an old word
// look like a return address, but both can only ADD a false positive (the
// module's own argument at `UnregMemo::observe`), never hide a live frame. So
// a clean verdict over `[cover_hi, high)` stays exact while the chain was not
// touched (no push, pop or prune: `ChainHistory::mutations`) and `cover_hi`,
// the stack top and the code-range generation are unchanged.
//
// The memo is read and written ONLY inside an in-native deposit
// ([`with_frozen_band_memo`], called by `native::jni` alone), so every other
// deposit, the safepoint publish and the collector's own scan -- the whole
// default configuration -- probe exactly as before. (gci i23/r6:
// `CRATONVM_JIT_UNREG_MEMO_GENERATION_KEYED=1`, default OFF, arms it for every
// scan but the collector's own; see "The generation-keyed scan memo" below.)
//
// The residual, as for the chain scan beside it: a LEAKED chain entry (a guard
// whose drop was bypassed, `prune_returned_jit_entries`) above the thread's
// real frames is not pruned, and with it on the chain the thread may have
// returned above `cover_hi` and re-descended without any push. The chain band
// scan already trusts the chain in that case; `CRATONVM_DBG_UNREG_MEMO_AUDIT`
// audits this memo's clean answers exactly as it audits `UNREG_JIT_MEMO`'s.

/// gce e1/j: run `f` (a JNI in-native deposit) with the frozen-band memo
/// armed on this thread. Nests (restores the previous state), unwind-safe.
/// gce e1b/j: `census` (the VM's JNI phase census, when on) lets the coverage
/// proof time its parts for the duration.
/// zt t32 (the successor page's section 4, `alive_after_gc=1`): run `f`
/// (a deposit) on a thread that has NO Java frame on its stack -- the VM's
/// reference-delivery thread between batches, in its own Rust loop. A word on
/// such a stack that looks like a return address into JIT code is residue of
/// an earlier batch's compiled frames, and so is every object word next to it:
/// [`scan_active_jit_frames`] publishes nothing for it (no registered entry,
/// no unregistered probe) and reports the chain covered (vacuously). Before
/// this, the delivery thread's idle deposit rooted the dead objects its last
/// GC-notification listener had touched, and one such word kept a weak
/// referent alive whose address a later allocation reused (`Zt1MkSystemGcFresh`,
/// t31-zc11 report section 14). Nests, unwind-safe. A thread whose entry chain
/// is not empty keeps the full scan.
pub(crate) fn with_no_java_on_stack<R>(f: impl FnOnce() -> R) -> R {
    struct Restore(bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            let prev = self.0;
            JIT_CHAIN_HISTORY.with(|c| {
                let mut h = c.get();
                h.no_java = prev;
                c.set(h);
            });
        }
    }
    let prev = JIT_CHAIN_HISTORY.with(|c| {
        let mut h = c.get();
        let prev = h.no_java;
        h.no_java = true;
        c.set(h);
        prev
    });
    let _restore = Restore(prev);
    f()
}

/// zwr (`zgc-delivery-thread-conservative-jit-band-keeps-a-weak-referent-FIXED-20261024`):
/// run `f` (the reference-delivery thread's batch) on a thread whose Java code
/// is entered ONLY from VM Rust code, through the interpreter's doors, each of
/// which pushes a `JitEntryGuard` (`jit_bridge`'s method-entry and OSR doors;
/// the helpers' guardless calls are made FROM compiled code, below an entry).
/// Such a thread has no process entry point, the one live compiled frame the
/// unregistered-frame probe exists for (the main thread's `Vm::invoke` ->
/// compiled `main`), so a JIT return address above its outermost chain entry
/// is a returned frame's residue. [`scan_active_jit_frames`] then judges the
/// probe's hits as the JNI in-native deposit does: a hit the residue mark
/// explains roots nothing (the band above the chain is the batch's Rust
/// frames and their dead words), and only a JIT return address above the
/// mark still marks, from the mark up. Before this, a pause that stopped the
/// delivery thread mid-batch swept its whole Rust stack above the chain as
/// roots, and a dead object's address there kept whatever a later allocation
/// put at it alive (`Zt1MkSystemGcFresh`'s `alive_after_gc=1`).
///
/// The same frames are what a take-over that FREEZES the thread in compiled
/// code would read conservatively, so the thread also publishes, in `floor`
/// (its `GcBlockState::delivery_rust_floor`), the `entry_sp` of its outermost
/// chain entry while it has one; the take-over reads its stack only below
/// that address (`xt_root_scan::cap_at_rust_band_floor`). Measured: a frozen
/// batch's Rust frames held a dead object's exact base 0x5000 above the
/// innermost compiled segment, and the new round's `int[16]` at that address
/// survived `System.gc()`.
///
/// Nests, unwind-safe. Kill switch `CRATONVM_JIT_DELIVERY_BATCH_RESIDUE_JUDGED=0`
/// (both halves).
pub(crate) fn with_chain_covered_java<R>(
    floor: &std::sync::atomic::AtomicUsize,
    f: impl FnOnce() -> R,
) -> R {
    struct Restore(bool, usize);
    impl Drop for Restore {
        fn drop(&mut self) {
            let (prev, prev_cell) = (self.0, self.1);
            JIT_CHAIN_HISTORY.with(|c| {
                let mut h = c.get();
                h.chain_covered_java = prev;
                h.delivery_floor_cell = prev_cell;
                c.set(h);
            });
        }
    }
    let cell = {
        floor as *const std::sync::atomic::AtomicUsize as usize
    };
    let (prev, prev_cell) = JIT_CHAIN_HISTORY.with(|c| {
        let mut h = c.get();
        let prev = (h.chain_covered_java, h.delivery_floor_cell);
        h.chain_covered_java = true;
        h.delivery_floor_cell = cell;
        c.set(h);
        prev
    });
    let _restore = Restore(prev, prev_cell);
    f()
}

/// zwr: publish `sp` (the outermost chain entry's `entry_sp`, or `0` once the
/// chain is empty) into the delivery thread's floor cell, when this thread
/// runs inside [`with_chain_covered_java`]. One thread-local read otherwise;
/// called only at the chain's outermost push and its last pop.
#[inline]
pub(in crate::jit::conservative_roots) fn publish_delivery_rust_floor(sp: usize) {
    let cell = JIT_CHAIN_HISTORY
        .try_with(|c| c.get().delivery_floor_cell)
        .unwrap_or(0);
    if cell != 0 {
        // SAFETY: `with_chain_covered_java` stores the address of an
        // `AtomicUsize` it borrows for the whole scope and clears it (through
        // its `Restore`) before the borrow ends; pushes and pops in that scope
        // run on this thread.
        let floor = unsafe { &*(cell as *const std::sync::atomic::AtomicUsize) };
        floor.store(sp, std::sync::atomic::Ordering::Release);
    }
}

/// zwr: is this thread the delivery thread running a batch (inside
/// [`with_chain_covered_java`], the switch on)? For the batch's per-item
/// dead-stack scrub (`gc_and_alloc::scrub_delivery_dead_stack`).
pub(crate) fn in_delivery_batch() -> bool {
    chain_covered_java()
}

/// zwr: inside [`with_chain_covered_java`]? Kill switch
/// `CRATONVM_JIT_DELIVERY_BATCH_RESIDUE_JUDGED=0` (default ON).
pub(in crate::jit::conservative_roots) fn chain_covered_java() -> bool {
    JIT_CHAIN_HISTORY.with(|c| c.get().chain_covered_java)
}

/// zt t32: inside [`with_no_java_on_stack`]? Kill switch
/// `CRATONVM_JIT_IDLE_DELIVERY_PRECISE=0` (default ON).
pub(in crate::jit::conservative_roots) fn no_java_on_stack() -> bool {
    JIT_CHAIN_HISTORY.with(|c| c.get().no_java)
}

pub(crate) fn with_frozen_band_memo<R>(
    census: Option<&crate::native::jni::JniPhaseCensus>,
    f: impl FnOnce() -> R,
) -> R {
    struct Restore(bool, *const crate::native::jni::JniPhaseCensus);
    impl Drop for Restore {
        fn drop(&mut self) {
            let (prev, prev_census) = (self.0, self.1);
            JIT_CHAIN_HISTORY.with(|c| {
                let mut h = c.get();
                h.in_native_deposit = prev;
                h.census = prev_census;
                c.set(h);
            });
        }
    }
    let census = census
        .filter(|c| c.is_on())
        .map_or(std::ptr::null(), |c| c as *const _);
    let prev = JIT_CHAIN_HISTORY.with(|c| {
        let mut h = c.get();
        let prev = (h.in_native_deposit, h.census);
        h.in_native_deposit = true;
        h.census = census;
        c.set(h);
        prev
    });
    let _restore = Restore(prev.0, prev.1);
    f()
}

/// gce e1b/j: a clock for the coverage proof's parts, `None` unless an
/// in-native deposit runs with the JNI phase census on.
#[inline]
pub(in crate::jit::conservative_roots) fn coverage_census_clock() -> Option<std::time::Instant> {
    let h = JIT_CHAIN_HISTORY.with(|c| c.get());
    (h.in_native_deposit && !h.census.is_null()).then(std::time::Instant::now)
}

/// gce e2/j: count one event of a census `COUNT:` phase, inside an in-native
/// deposit with the JNI phase census on (a no-op otherwise).
#[inline]
pub(in crate::jit::conservative_roots) fn coverage_census_note(phase: crate::native::jni::JniPhase) {
    let h = JIT_CHAIN_HISTORY.with(|c| c.get());
    if h.in_native_deposit && !h.census.is_null() {
        // SAFETY: see `ChainHistory::census`.
        unsafe { &*h.census }.note(phase);
    }
}

/// gce e2/j: add `n` to a census `COUNT:` phase (see [`coverage_census_note`]).
#[inline]
pub(in crate::jit::conservative_roots) fn coverage_census_count(phase: crate::native::jni::JniPhase, n: u64) {
    let h = JIT_CHAIN_HISTORY.with(|c| c.get());
    if h.in_native_deposit && !h.census.is_null() {
        // SAFETY: see `ChainHistory::census`.
        unsafe { &*h.census }.count(phase, n);
    }
}

/// gce e1b/j: charge `phase` since `clock` (see [`coverage_census_clock`]).
#[inline]
pub(in crate::jit::conservative_roots) fn coverage_census_lap(
    phase: crate::native::jni::JniPhase,
    clock: &mut Option<std::time::Instant>,
) {
    if clock.is_none() {
        return;
    }
    let census = JIT_CHAIN_HISTORY.with(|c| c.get().census);
    if !census.is_null() {
        // SAFETY: see `ChainHistory::census`.
        unsafe { &*census }.lap(phase, clock);
    }
}

/// gce e1/j: is a JNI in-native deposit running on this thread
/// ([`with_frozen_band_memo`])? Read by the JNI phase census only.
pub(crate) fn in_native_deposit_armed() -> bool {
    JIT_CHAIN_HISTORY.with(|c| c.get().in_native_deposit)
}

// ---------------------------------------------------------------------------
// The compiled band of a JNI in-native thread, scanned by the pause (gch h4/bp)
// ---------------------------------------------------------------------------
//
// `docs/feature-designs/real-jdk-mode.md`,
// `CRATONVM_JNI_IN_NATIVE_BAND_AT_PAUSE`, default ON since gci i1/jn (with the JNI in-native package
// and its no-signal mark, Generational only, a compiled caller only).
//
// The package's in-native deposit spends about two thirds of its time on the
// compiled frames below the native: the moving-young coverage proof
// (`refresh_moving_young_coverage_for_current_thread`, mostly its band verify)
// and the conservative chain scan (`scan_active_jit_frames`). Under the switch
// the JNI bracket (`native::jni::native_call_enter_native`) records the band
// `[deposit SP, stack top)` in the thread's `GcBlockState` and runs the deposit
// inside [`with_band_at_pause_deposit`]: the proof answers "unproven" at once
// (it writes no ledger row) and the chain scan publishes nothing (the pin
// publication that follows publishes the empty set). Every other part of the
// deposit -- interpreter frames, off-frame roots, the JNI local frames, the
// shadow stack -- is unchanged.
//
// The pause covers the band instead: `ThreadRegistry::scan_in_native_bands`
// reads it, conservatively, for every thread in a blocked region with its
// band recorded (h4a keyed it on the foreign-native mark too: unsound, see
// `thread_registry::in_native_band_of`), and
// `gc_and_alloc::append_in_native_band_roots` marks the cycle's moving-young
// coverage incomplete
// (`CROSS_THREAD_JIT_PEER`, the verdict such a peer already produces: its JIT
// depth is never credited). Why the band is frozen and complete is on
// `native::jni::in_native_band_at_pause`. (gci i28/nb: under the default-on
// `CRATONVM_JNI_IN_NATIVE_BAND_NARROW` the band ends at the outermost entry
// instead of the stack top when the stack above the chain is proven free of
// compiled frames: [`in_native_band_chain_top`].)

/// gch h4/bp: run `f` (a JNI in-native deposit) with its compiled band left
/// to the pause when `armed`; `f()` alone otherwise. Nests (restores the
/// previous state), unwind-safe.
pub(crate) fn with_band_at_pause_deposit<R>(armed: bool, f: impl FnOnce() -> R) -> R {
    if !armed {
        return f();
    }
    struct Restore(bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            let prev = self.0;
            JIT_CHAIN_HISTORY.with(|c| {
                let mut h = c.get();
                h.band_at_pause = prev;
                c.set(h);
            });
        }
    }
    let prev = JIT_CHAIN_HISTORY.with(|c| {
        let mut h = c.get();
        let prev = h.band_at_pause;
        h.band_at_pause = true;
        c.set(h);
        prev
    });
    let _restore = Restore(prev);
    f()
}

/// gch h4/bp: is a deposit whose compiled band the pause scans running on
/// this thread ([`with_band_at_pause_deposit`])?
#[inline]
pub(crate) fn band_at_pause_deposit_armed() -> bool {
    JIT_CHAIN_HISTORY.with(|c| c.get().band_at_pause)
}

/// gci i24/bp (`CRATONVM_JNI_IN_NATIVE_PROVEN_DEPTH_CREDIT`, opt-in;
/// `gch-h4bp-review-findings-RETIRED-UNFIXED-20261018` item 1): run `f` (a JNI in-native
/// deposit, inside [`with_band_at_pause_deposit`]) with the coverage proof
/// armed again when `armed`: [`refresh_moving_young_coverage_for_current_thread`]
/// then proves this thread's chain as the band-off deposit always did, and the
/// deposit records the proven depth in the thread's `GcBlockState`
/// (`in_native_proven_jit_depth`) for the pause to credit
/// (`ThreadRegistry::credit_in_native_proven_bands`). The chain scan still
/// publishes nothing: the pause scans the band. `f()` alone when not armed.
/// Nests (restores the previous state), unwind-safe.
pub(crate) fn with_band_at_pause_proof<R>(armed: bool, f: impl FnOnce() -> R) -> R {
    if !armed {
        return f();
    }
    struct Restore(bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            let prev = self.0;
            JIT_CHAIN_HISTORY.with(|c| {
                let mut h = c.get();
                h.band_at_pause_prove = prev;
                c.set(h);
            });
        }
    }
    let prev = JIT_CHAIN_HISTORY.with(|c| {
        let mut h = c.get();
        let prev = h.band_at_pause_prove;
        h.band_at_pause_prove = true;
        c.set(h);
        prev
    });
    let _restore = Restore(prev);
    f()
}

/// gci i24/bp: is a band-at-pause deposit that runs its coverage proof (and
/// records the proven depth) running on this thread
/// ([`with_band_at_pause_proof`] inside [`with_band_at_pause_deposit`])?
/// False outside a band-at-pause deposit, whatever the proof scope says.
#[inline]
pub(crate) fn band_at_pause_proof_armed() -> bool {
    JIT_CHAIN_HISTORY.with(|c| {
        let h = c.get();
        h.band_at_pause && h.band_at_pause_prove
    })
}

/// gch h4/bp: the exclusive top of this thread's stack, for the band
/// `[lo, top)` a JNI in-native thread leaves to the pause, or `None` when the
/// platform cannot say, `lo` is not on this thread's stack, or the band would
/// be empty. Called by the thread itself.
pub(crate) fn in_native_band_top(lo: usize) -> Option<usize> {
    let top = own_stack_top();
    (top != 0 && lo < top && sp_on_own_stack(lo)).then_some(top)
}

// ---------------------------------------------------------------------------
// The in-native band narrowed to the JIT chain (gci i28/nb, P5/nb)
// ---------------------------------------------------------------------------
//
// `docs/feature-designs/profile-guided-inlining.md`,
// `CRATONVM_JNI_IN_NATIVE_BAND_NARROW` (default ON since the gck flip, `=0` kills; read by `native::jni`).
//
// The band a JNI in-native thread leaves to the pause is `[lo, stack top)`:
// every compiled frame below the outermost JIT entry, AND every interpreter
// and Rust frame of the thread above it (tens of KiB for a shallow `main`,
// hundreds for a JUnit stack), read conservatively per pause. The band-off
// deposit never scanned the part above the chain unless its A5 probe found a
// JIT return address there (`scan_active_jit_frames`' A5 block): the chain
// band `[deposit SP, outermost entry_sp)` is its whole conservative reach.
// [`in_native_band_chain_top`] gives the band the same reach: its top becomes
// the outermost `entry_sp` when a clean A5 verdict covers `[entry_sp, stack
// top)` -- the frozen-band memo's ([`frozen_band_clean`], the same verdict
// the band-off deposit's A5 block and the coverage proof replay), or a fresh
// whole probe, which records its verdict for the next call at the same chain
// state. Whatever lies above is what the band-off deposit left there too:
// interpreter frames (deposited precisely), Rust frames (never roots), and
// returned frames' residue (only when the residue filter, default ON, reads
// it as such, exactly as the coverage proof does).

/// gci i28/nb: how [`in_native_band_chain_top_why`] proved the part of the stack
/// above the chain free of compiled frames.
#[cfg_attr(not(any(target_os = "windows", target_os = "linux")), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ChainBandProof {
    /// A clean verdict the frozen-band memo held for the same chain state.
    Memo,
    /// A fresh whole probe of `[entry_sp, stack top)`, now in the memo.
    Probed,
}

/// gci i28/nb: the narrowed top of a band whose low end is `lo`, from the
/// outermost JIT entry SP `cover_hi` and the stack top `top`: `cover_hi`
/// rounded up to a word, when the chain lies above `lo` and the result is
/// below `top` (else nothing to narrow). Pure.
#[cfg_attr(not(any(target_os = "windows", target_os = "linux")), allow(dead_code))]
pub(in crate::jit::conservative_roots) fn chain_band_hi(lo: usize, cover_hi: usize, top: usize) -> Option<usize> {
    let hi = cover_hi.checked_add(7)? & !7usize;
    (lo < cover_hi && hi < top).then_some(hi)
}

/// gcj j2/jn2: why [`in_native_band_chain_top_why`] kept the whole band. The
/// j1 NB rows probed 2605 calls and narrowed none (`dep_band_narrowed=0`);
/// each refusal is now counted by its reason in the JNI phase census
/// (`dep_band_narrow_no_chain`, `..._truncated`, `..._jit_above`), so one run
/// says which arm refuses.
#[cfg_attr(not(any(target_os = "windows", target_os = "linux")), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ChainBandRefusal {
    /// The chain is empty (or borrowed), not above `lo`, or its outermost
    /// entry rounds up to the stack top: nothing to narrow.
    NoChain,
    /// The read of `[entry_sp, stack top)` would stop short of the top.
    Truncated,
    /// A word above the chain is a JIT return address the residue filter does
    /// not discard (a live compiled frame above the outermost registered
    /// entry, or residue with the filter off).
    JitAbove,
}

/// gcj j2/jn2: `in_native_band_chain_top` with the reason of a refusal.
/// Same answers (`Ok` exactly when it answers `Some`).
pub(crate) fn in_native_band_chain_top_why(
    lo: usize,
    top: usize,
) -> Result<(usize, ChainBandProof), ChainBandRefusal> {
    #[cfg(any(target_os = "windows", target_os = "linux"))]
    {
        let cover_hi = JIT_ENTRY_CHAIN
            .try_with(|c| {
                c.try_borrow()
                    .ok()
                    .and_then(|v| v.iter().map(|e| e.entry_sp).max())
            })
            .ok()
            .flatten()
            .ok_or(ChainBandRefusal::NoChain)?;
        let hi = chain_band_hi(lo, cover_hi, top).ok_or(ChainBandRefusal::NoChain)?;
        // A classified (residue-only) verdict serves only while the residue
        // filter is on, as in the coverage proof.
        let need_raw = !a5_residue_filter_enabled();
        with_frozen_band_memo(None, || {
            if frozen_band_clean(cover_hi, top, need_raw) {
                // `CRATONVM_DBG_UNREG_MEMO_AUDIT`: count a memo answer a fresh
                // probe of the live part of the band would contradict.
                if dbg_unreg_memo_audit() {
                    UNREG_MEMO_SHORTCIRCUITS.fetch_add(1, Ordering::Relaxed);
                    let live_lo = jit_residue_hi().max(cover_hi);
                    if top > live_lo && native_stack_has_jit_frame(live_lo, top).is_some() {
                        UNREG_MEMO_SUPPRESSED.fetch_add(1, Ordering::Relaxed);
                    }
                }
                return Ok((hi, ChainBandProof::Memo));
            }
            // A truncated read proves nothing about the words it skipped.
            if own_stack_band_truncated(cover_hi, top) {
                return Err(ChainBandRefusal::Truncated);
            }
            if native_stack_has_jit_frame(cover_hi, top).is_none() {
                note_frozen_band_clean(cover_hi, top, true);
                return Ok((hi, ChainBandProof::Probed));
            }
            if need_raw {
                return Err(ChainBandRefusal::JitAbove);
            }
            // Only residue below the residue mark, nothing above it: the
            // classified verdict of the coverage proof.
            let live_lo = jit_residue_hi().max(cover_hi);
            if live_lo >= top || native_stack_has_jit_frame(live_lo, top).is_none() {
                note_frozen_band_clean(cover_hi, top, false);
                return Ok((hi, ChainBandProof::Probed));
            }
            Err(ChainBandRefusal::JitAbove)
        })
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    {
        let _ = (lo, top);
        Err(ChainBandRefusal::NoChain)
    }
}

/// gcf f2/j: the key the JNI in-native repeat (`native::jni::InNativeRepeat`)
/// reads of this thread's JIT entry chain: its mutation count
/// ([`ChainHistory::mutations`]) and the innermost entry's interpreter depth,
/// or `None` when the chain is empty (or already borrowed).
pub(crate) fn chain_repeat_key() -> Option<(u64, u32)> {
    let depth = JIT_ENTRY_CHAIN
        .try_with(|c| {
            c.try_borrow()
                .ok()
                .and_then(|v| v.last().map(|e| e.interp_depth))
        })
        .ok()
        .flatten()?;
    Some((JIT_CHAIN_HISTORY.with(|c| c.get().mutations), depth))
}

/// gcf f2/j (`gcf-f1j-deposit-scans-fill-never-read-thread-local-movable-sets`):
/// a band word's movable claim / unrewritable veto goes into the SCANNING
/// thread's sets, which only that thread's own collection reads, after its
/// `collect_roots` cleared them. Inside a JNI in-native deposit the scanning
/// thread is never the collecting one, so the insert is dead (a SipHash
/// insert per object word, per native call, growing until that thread next
/// collects): skipped there. Every other scan is unchanged.
///
/// gcf f4/j: and inside every other root DEPOSIT (the blocking deposit, the
/// wake refresh: `deposit_root_snapshot_inner` holds a
/// `native::jni::RootDepositScope`), for the same reason -- a deposit runs on
/// the depositing thread, never inside its own collection's root gathering.
/// Still unchanged: the safepoint publish (`update_root_snapshot`) and every
/// scan a collector makes, its own stack and its peers' (take-over, helper
/// window) alike.
///
/// gcj-j1pn-young-pin-ledger-review-findings item 4: and inside the safepoint
/// publish (`update_root_snapshot`, `native::jni::SafepointPublishScope`),
/// whose inserts no reader sees either (see the scope's doc). What remains is
/// every scan a collector makes inside its own root gathering, its own stack
/// and its peers' (take-over, helper window) alike: the sets' only readers.
#[inline]
pub(in crate::jit::conservative_roots) fn note_movable_band_root(qword: usize) {
    if band_root_sets_written_here() {
        cratonvm_gc::gc_quiescence::add_movable_jit_root(qword);
    }
}

/// gcf f2/j: see [`note_movable_band_root`].
#[inline]
pub(in crate::jit::conservative_roots) fn note_unrewritable_band_root(qword: usize) {
    if band_root_sets_written_here() {
        cratonvm_gc::gc_quiescence::add_unrewritable_jit_root(qword);
    }
}

/// Does a band word's movable claim / unrewritable veto reach this thread's
/// sets here? Not in a JNI in-native deposit, a root deposit or (gcj j1pn
/// ledger item 4) a safepoint publish whose scope's switch was on.
#[inline]
pub(in crate::jit::conservative_roots) fn band_root_sets_written_here() -> bool {
    !in_native_deposit_armed()
        && !crate::native::jni::root_deposit_running()
        && !crate::native::jni::safepoint_publish_running()
}

/// gcf f1/j: inside a JNI in-native deposit with
/// `CRATONVM_JNI_IN_NATIVE_ENVELOPE_FIRST` on (the package's default), the
/// compiled-frame band walks drop a word outside the heap envelope before any
/// claim, and a frame with no such word whole -- exactly: such a word is never
/// an object start. Every other scan is unchanged. See
/// `docs/feature-designs/real-jdk-mode.md`.
///
/// nio21 w15 L5: also inside every other root DEPOSIT (the blocking-region
/// entry deposit and the wake refresh, both under
/// `native::jni::RootDepositScope`), behind
/// [`deposit_envelope_first_enabled`]. Same exactness argument: the drop is a
/// word the band scan's own envelope test (`scan_one_frame_filtered`, after
/// the claims) would reject, so roots, pins and publications are unchanged and
/// only the diagnostic exclusion counters stop counting it. It matters because
/// every blocking native (a `Pipe` read or write, a park) deposits, and the
/// w12 `Nio21UnsafeVolatile pipe` profile spent 16.8% in
/// `scan_one_compiled_frame_with_layout` running claims on scalar words. No
/// effect under ZGC, whose `conservative_addr_span` is `None`.
#[inline]
pub(in crate::jit::conservative_roots) fn in_native_envelope_first() -> bool {
    (in_native_deposit_armed() && crate::native::jni::in_native_envelope_first_enabled())
        || (crate::native::jni::root_deposit_running() && deposit_envelope_first_enabled())
}

/// `CRATONVM_GC_DEPOSIT_ENVELOPE_FIRST` (default ON; `0|false|off|no` is the
/// kill switch): see [`in_native_envelope_first`].
pub(in crate::jit::conservative_roots) fn deposit_envelope_first_enabled() -> bool {
    static ON: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&ON, || {
        true
    })
}

/// gcf f1/j: does `[lo, hi)` -- 8-aligned words of a compiled frame's band on
/// this thread's own stack -- hold a word `pred` accepts? Reads only.
pub(in crate::jit::conservative_roots) fn band_has_word(lo: usize, hi: usize, pred: impl Fn(usize) -> bool) -> bool {
    let mut a = (lo + 7) & !7usize;
    while a + 8 <= hi {
        // SAFETY: an aligned word inside a compiled frame's own band on this
        // thread's live stack, the interval the band scans themselves read.
        let w = unsafe { (a as *const usize).read() };
        if pred(w) {
            return true;
        }
        a += 8;
    }
    false
}

// ---------------------------------------------------------------------------
// The generation-keyed scan memo (gci i23/r6, P2/r6)
// ---------------------------------------------------------------------------
//
// `docs/feature-designs/jit-ir-optimizations.md`,
// `CRATONVM_JIT_UNREG_MEMO_GENERATION_KEYED` (default OFF).
//
// `invalidate_scan_cache_for_gc` resets `UNREG_JIT_MEMO` before every
// production scan (every `update_root_snapshot`, i.e. every object-returning
// native call, the safepoint publish and the blocked deposit), so outside the
// JNI in-native deposit the unregistered-frame probe re-reads the whole band
// above the chain on every call. (The proposal's second structure, the JIT
// scan cache, was deleted in round 11 wave 15: nothing is left of it to key.)
//
// The proposal keys the memo by "(collection count, chain generation)". That
// key is sound only for the band ABOVE a non-empty chain, and the frozen-band
// memo above is already exactly that memo, keyed by the chain generation
// (`ChainHistory::mutations`), `cover_hi`, the stack top and the code-range
// generation. So the switch does not re-key `UNREG_JIT_MEMO`; it arms the
// frozen-band memo for every scan, not only inside an in-native deposit:
//
// * The empty chain stays unkeyed (`UNREG_JIT_MEMO`, reset as before). With
//   no entry on the chain the band starts at the scanner's own SP and holds
//   live interpreter / Rust frames: a return-and-re-descend through them moves
//   neither the collection count nor the chain generation, which is the
//   972-suppression failure `invalidate_scan_cache_for_gc`'s doc records.
// * The collection count is NOT part of the key. The verdict is "no word of
//   `[cover_hi, high)` is a return address into compiled code"; a collection
//   writes only heap addresses into stack slots (never inside a code range),
//   and the one collector event that changes the code-range set (a reclaimed
//   body) moves `jit_code_ranges_generation`, which is in the key.
// * Outside a deposit only a RAW verdict skips the A5 block's probe
//   (`frozen_band_verdict`): that block marks the band on ANY hit while the
//   chain is not empty, so a classified (residue-only) verdict, which the
//   deposit lets skip the marking, would change what a non-deposit scan marks.
// * The collector's own root pass (`scan_active_jit_frames_for_collection`)
//   and its own coverage proof (`refresh_moving_young_coverage_for_collection`,
//   [`with_collector_proof`]) never use it: once per collection, nothing to
//   save, all the risk.
//
// The residual is the deposit memo's (see the section above): a leaked chain
// entry above the thread's real frames. `CRATONVM_DBG_UNREG_MEMO_AUDIT=1`
// audits every hit of both sites (`[GC] unreg_memo:` under
// `CRATONVM_GC_STATS=1`); `SUPPRESSED` must stay 0.

/// gci i23/r6: `CRATONVM_JIT_UNREG_MEMO_GENERATION_KEYED=1` (default on):
/// arm the frozen-band memo outside the JNI in-native deposit (see the section
/// above).
#[cfg_attr(not(any(target_os = "windows", target_os = "linux")), allow(dead_code))]
pub(in crate::jit::conservative_roots) fn generation_keyed_memo_enabled() -> bool {
    // Read per call (a flag snapshot): no process latch (statics ratchet).
    true
}

/// gci i23/r6: may the frozen-band memo record or answer on this thread now?
/// Always inside an in-native deposit (gce e1/j, unchanged); otherwise only
/// under [`generation_keyed_memo_enabled`] and outside the collector's own
/// root pass and coverage proof.
#[cfg_attr(not(any(target_os = "windows", target_os = "linux")), allow(dead_code))]
pub(in crate::jit::conservative_roots) fn frozen_band_memo_armed(h: &ChainHistory) -> bool {
    if h.in_native_deposit {
        return true;
    }
    #[cfg(test)]
    let keyed = h
        .keyed_for_test
        .unwrap_or_else(generation_keyed_memo_enabled);
    #[cfg(not(test))]
    let keyed = generation_keyed_memo_enabled();
    keyed && !h.collector_proof && !gc_root_pass_active()
}

/// gci i23/r6: run `f` (the collector's own moving-young coverage proof) with
/// the generation-keyed memo disarmed on this thread. Nests (restores the
/// previous state), unwind-safe.
pub(in crate::jit::conservative_roots) fn with_collector_proof<R>(f: impl FnOnce() -> R) -> R {
    struct Restore(bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            let prev = self.0;
            JIT_CHAIN_HISTORY.with(|c| {
                let mut h = c.get();
                h.collector_proof = prev;
                c.set(h);
            });
        }
    }
    let prev = JIT_CHAIN_HISTORY.with(|c| {
        let mut h = c.get();
        let prev = h.collector_proof;
        h.collector_proof = true;
        c.set(h);
        prev
    });
    let _restore = Restore(prev);
    f()
}

/// gce e1/j: the key a verdict over `[search_lo, high)` would have now, or
/// `None` when the memo does not apply: no in-native deposit is running (and
/// gci i23/r6's generation-keyed arm is off, see [`frozen_band_memo_armed`]),
/// the chain is empty, or `search_lo` is not the chain's `cover_hi` (the band
/// would include live frames).
#[cfg_attr(not(any(target_os = "windows", target_os = "linux")), allow(dead_code))]
pub(in crate::jit::conservative_roots) fn frozen_band_key(search_lo: usize, high: usize) -> Option<FrozenBand> {
    let h = JIT_CHAIN_HISTORY.with(|c| c.get());
    if !frozen_band_memo_armed(&h) || high <= search_lo {
        return None;
    }
    let cover_hi = JIT_ENTRY_CHAIN.with(|c| c.borrow().iter().map(|e| e.entry_sp).max())?;
    let sp = current_stack_pointer();
    // An SP off this thread's own stack makes the band unverifiable
    // (`sp_on_own_stack`); both probe sites then fail closed.
    if cover_hi != search_lo || sp >= cover_hi || !sp_on_own_stack(sp) {
        return None;
    }
    Some(FrozenBand {
        mutations: h.mutations,
        cover_hi,
        high,
        code_generation: cratonvm_jit::jit_code_ranges_generation(),
    })
}

/// gce e1/j: inside an in-native deposit (gci i23/r6: or wherever
/// [`frozen_band_memo_armed`] says), is `[search_lo, high)` still covered
/// by a clean verdict recorded at the same chain state? See the section above.
/// `need_raw`: only a verdict whose raw probe was clean counts (the A5 block of
/// `scan_active_jit_frames` marks the band on ANY hit while the chain is not
/// empty, residue included); otherwise a classified-clean one does too (the
/// coverage proof's reading).
#[cfg_attr(not(any(target_os = "windows", target_os = "linux")), allow(dead_code))]
pub(in crate::jit::conservative_roots) fn frozen_band_clean(search_lo: usize, high: usize, need_raw: bool) -> bool {
    let Some(key) = frozen_band_key(search_lo, high) else {
        return false;
    };
    JIT_CHAIN_HISTORY.with(|c| match c.get().frozen {
        Some((k, raw)) => k == key && (raw || !need_raw),
        None => false,
    })
}

/// gce e1b/j: the recorded verdict for `[search_lo, high)` at the current
/// chain state -- `Some(true)` raw-clean, `Some(false)` classified-clean (only
/// residue below the residue mark) -- or `None`.
///
/// gci i23/r6: outside an in-native deposit (the generation-keyed arm) only a
/// raw verdict is answered; a classified one is `None`, so the A5 block probes
/// and marks exactly as it would without the memo.
#[cfg_attr(not(any(target_os = "windows", target_os = "linux")), allow(dead_code))]
pub(in crate::jit::conservative_roots) fn frozen_band_verdict(search_lo: usize, high: usize) -> Option<bool> {
    let key = frozen_band_key(search_lo, high)?;
    let h = JIT_CHAIN_HISTORY.with(|c| c.get());
    match h.frozen {
        Some((k, raw)) if k == key && (raw || h.in_native_deposit) => Some(raw),
        _ => None,
    }
}


/// gce e1/j: inside an in-native deposit, record that a probe of the WHOLE
/// band `[search_lo, high)` (not truncated) found no live JIT frame: `raw`
/// when it found no JIT return address at all, otherwise only after the
/// coverage proof's residue / shape classification. A classified verdict
/// never downgrades a raw one recorded for the same key.
#[cfg_attr(not(any(target_os = "windows", target_os = "linux")), allow(dead_code))]
pub(in crate::jit::conservative_roots) fn note_frozen_band_clean(search_lo: usize, high: usize, raw: bool) {
    let Some(key) = frozen_band_key(search_lo, high) else {
        return;
    };
    JIT_CHAIN_HISTORY.with(|c| {
        let mut h = c.get();
        let raw = raw || matches!(h.frozen, Some((k, true)) if k == key);
        h.frozen = Some((key, raw));
        c.set(h);
    });
}

/// Record that JIT execution is about to begin on the current thread.
///
/// Captures the current stack pointer at the instant of the call and pushes
/// it onto the per-thread chain. Returns the depth *after* the push (1-based)
/// purely as a debugging convenience.
///
/// The caller must pair every `push_jit_entry` with exactly one
/// [`pop_jit_entry`] when the JIT call returns or unwinds. The pairing is
/// stack-discipline (LIFO).
/// Internal: push a *given* stack pointer onto the JIT entry chain. Used by
/// the inline entry-point macros so the captured SP belongs to the caller's
/// frame, not to ours. Direct callers should generally prefer
/// [`JitEntryGuard::enter`] which handles the SP capture and pop pairing.
pub fn push_jit_entry_at(sp: usize) -> usize {
    push_entry_full(JitFrameChainEntry {
        entry_sp: sp,
        precise: None,
        interp_depth: INTERP_DEPTH_INHERIT,
        depth_stripe: cratonvm_types::striped_counter::StripeToken::UNSET,
        exec_token: cratonvm_jit::JitExecutionToken::UNSET,
        tally_skipped: false,
    })
}

/// NEW-12: push a fully-specified chain entry. Used by
/// [`JitEntryGuard::enter_with_compiled`] to register both the stack
/// pointer and the precise-frame metadata in one atomic step.
pub(crate) fn push_entry_full(entry: JitFrameChainEntry) -> usize {
    push_entry_impl(entry, false)
}

/// The door form of a chain push and pop (t1 wave 5, lane S;
/// `CRATONVM_JIT_DOOR_THREAD_BLOCK`, default ON (`=0` kills), read per VM by the
/// interpreter-to-compiled door, `jit_bridge::run_jit_body_raw`). The chain,
/// the counters the collector reads and every published record move exactly
/// as in [`push_entry_full`] / [`pop_jit_entry`]; the door form only
/// (a) touches `JIT_CHAIN_HISTORY` once per push or pop instead of up to three
/// times (the mutation count, the delivery floor cell and, on a pop, the
/// residue mark), and (b) skips the diagnostic `gc_quiescence` enter / leave
/// tallies (`enter_uncounted` / `leave_uncounted`), which only the non-moving
/// sweep's first-corruption line prints.
#[inline]
pub(in crate::jit::conservative_roots) fn door_history_note(c: &std::cell::Cell<ChainHistory>, pop_residue: Option<usize>, root: bool) -> usize {
    let mut h = c.get();
    h.mutations = h.mutations.wrapping_add(1);
    if root { h.root_mutations = h.root_mutations.wrapping_add(1); }
    if let Some(sp) = pop_residue {
        h.residue_hi = h.residue_hi.max(sp);
    }
    c.set(h);
    h.delivery_floor_cell
}

/// [`publish_delivery_rust_floor`] with the cell already read.
#[inline]
pub(in crate::jit::conservative_roots) fn store_delivery_floor(cell: usize, sp: usize) {
    if cell != 0 {
        // SAFETY: as in `publish_delivery_rust_floor`.
        let floor = unsafe { &*(cell as *const std::sync::atomic::AtomicUsize) };
        floor.store(sp, std::sync::atomic::Ordering::Release);
    }
}

pub(in crate::jit::conservative_roots) fn push_entry_impl(entry: JitFrameChainEntry, door: bool) -> usize {
    push_entry_form(entry, door, false)
}

/// [`push_entry_impl`], and with `one_counter` (door form only; t1 wave 8,
/// lane I25, `CRATONVM_JIT_DOOR_ONE_DEPTH_COUNTER`) the entry's three
/// process-wide counts -- `GLOBAL_JIT_DEPTH`, the collector's quiescence depth
/// and the code cache's `ACTIVE_JIT_EXECUTIONS` -- are one increment of
/// `cratonvm_types::striped_counter::DOOR_DEPTH`, which all three fold into
/// their readers. Its token rides in `exec_token`
/// ([`cratonvm_jit::JitExecutionToken::counts_door_depth`]), so every pop,
/// prune and segment retire settles it from the entry itself, whichever guard
/// pops it.
pub(in crate::jit::conservative_roots) fn push_entry_form(entry: JitFrameChainEntry, door: bool, one_counter: bool) -> usize {
    DOOR_BLOCK.with(|b| push_entry_in(b, entry, door, one_counter))
}

/// [`push_entry_form`] with the thread's [`DoorBlock`] in hand.
#[inline]
pub(in crate::jit::conservative_roots) fn push_entry_in(b: &DoorBlock, entry: JitFrameChainEntry, door: bool, one_counter: bool) -> usize {
    #[cfg(any(target_os = "windows", target_os = "linux"))]
    note_pre_entry_stale_words(entry.entry_sp);
    // Chain mutation = JIT boundary: invalidate the per-thread scan cache.
    note_jit_boundary();
    // Every transfer of control into compiled code passes through here, so this
    // is the run's interpreter->JIT entry count. Divided into the JIT's measured
    // CPU delta it gives the per-entry cost, which is the number that decides
    // whether "compiling short methods an interpreted caller invokes" is what
    // makes the JIT a net negative on call-dense classes.
    scan_prof::bump(&scan_prof::JIT_ENTRIES);
    let (depth, entry_sp) = {
        let mut v = b.chain.borrow_mut();
        // Resolve the "same place in the Java stack as my caller" sentinel
        // while the chain is in hand. An empty chain means the entry is the
        // outermost compiled frame reached from a site that could not name its
        // interpreter depth; 0 places it below every interpreter frame, which
        // is the only safe guess and never reorders the frames that DO know.
        let mut entry = entry;
        let entry_sp = entry.entry_sp;
        if entry.interp_depth == INTERP_DEPTH_INHERIT {
            entry.interp_depth = v.last().map_or(0, |e| e.interp_depth);
        }
        // Finalize the outgoing top entry's `exact_rbp` from the cache before it
        // becomes non-top (each entry's `exact_rbp` is read by the relocation
        // walk). The incoming entry starts unrecorded → reset the cache to 0.
        if let Some(old_top) = v.last_mut() {
            if let Some(info) = old_top.precise.as_mut() {
                info.exact_rbp = top_rbp_get();
                info.exact_cm_id = published_compile_id();
            }
        }
        // Both process-wide counts are raised here, with their stripes kept in
        // the entry the matching pop consumes. A few instructions earlier than
        // the push they describe is the over-approximating direction for
        // "is anyone in JIT?".
        if one_counter {
            // One increment for all three counts (`depth_stripe` stays UNSET,
            // so the pop's `GLOBAL_JIT_DEPTH` decrement is a no-op), raised
            // here, before the quiescence depth's usual point below: the
            // over-approximating direction again.
            entry.exec_token = cratonvm_jit::jit_execution_enter_door();
        } else {
            entry.depth_stripe = GLOBAL_JIT_DEPTH.inc_token();
            entry.exec_token = cratonvm_jit::jit_execution_enter();
        }
        // Matches the `enter_uncounted` / `enter` choice below (a one-counter
        // entry takes neither and never reads it).
        entry.tally_skipped = door;
        v.push(entry);
        if !door {
            note_chain_mutation();
        }
        let n = v.len();
        top_rbp_set(0);
        // The incoming entry starts unrecorded in BOTH mirrors. Zeroing only
        // the RBP left the identity naming the entry we just pushed BELOW us,
        // so the pair described two different frames from the first instruction
        // of the new entry. See `reload_top_rbp_cache` for what that cost.
        if cm_id_pairing_enabled() {
            top_cm_id_mirror_write(0);
        }
        (n, entry_sp)
    };
    if door {
        let cell = door_history_note(&b.history, None, depth == 1);
        if depth == 1 {
            store_delivery_floor(cell, entry_sp);
        }
    } else if depth == 1 {
        publish_delivery_rust_floor(entry_sp);
    }
    publish_self_jit_depth_in(&b.depth_slot, depth);
    // P1 shadow record (`docs/threading/thread-transition-states.md` §7.2):
    // this is the ONLY point at which a thread becomes
    // `CompiledUninterruptible`. A nested entry re-records the same state,
    // which the recorder treats as the counting event it is (self-edges are
    // legal) rather than a transition.
    thread_state::record_transition(
        ThreadExecState::CompiledUninterruptible,
        "jit::conservative_roots::push_entry_full",
    );
    // Mirror into the GC-side quiescence flag so the GC can defer
    // compaction whenever any thread is inside a JIT call. NEW-12's
    // precise root walk removes false positives from the root set,
    // but compaction still requires precise oop-map coverage across
    // the *entire* active chain — and today the JIT compiler itself
    // does not yet populate maps from its simulated-stack type
    // tracker, so conservative fallback entries remain possible. The
    // defer guard can only be lifted once every entry in flight is
    // guaranteed precise (a later follow-up under NEW-12).
    // A one-counter entry is counted in `DOOR_DEPTH` above, which `is_active`
    // folds in.
    if !one_counter {
        if door {
            cratonvm_gc::gc_quiescence::enter_uncounted();
        } else {
            cratonvm_gc::gc_quiescence::enter();
        }
    }
    depth
}

/// P1 shadow record — the state a thread leaving one JIT entry lands in, given
/// the chain length that REMAINS after the pop.
///
/// An empty chain means the thread is back in the interpreter
/// (`CompiledUninterruptible -> JavaRunning`, the tabled edge for
/// `pop_jit_entry` / `prune_returned_jit_entries`). A non-empty one means an
/// outer compiled frame is still live, so the thread stays
/// `CompiledUninterruptible` (a legal self-edge).
///
/// The `Deoptimizing` case is the exception, and it is why this is a function
/// rather than a bare `if`: a deopt trap raised the signal from *inside*
/// compiled code, and `Deoptimizing`'s only tabled successors are
/// `JavaRunning` / `VmRunning` — re-recording `CompiledUninterruptible` for a
/// nested pop would manufacture a violation the code is not actually
/// committing. The window is closed at the pop instead, which is the last
/// point this module can observe; the interpreter's own resume sites
/// (`resume_from_ir_deopt`, `real_frame_deopt_resume_and_despeculate`) then
/// re-record `JavaRunning` as a no-op self-edge.
#[inline]
pub(in crate::jit::conservative_roots) fn leaving_compiled_state(remaining: usize) -> ThreadExecState {
    if remaining == 0 || thread_state::current_state() == ThreadExecState::Deoptimizing {
        ThreadExecState::JavaRunning
    } else {
        ThreadExecState::CompiledUninterruptible
    }
}

/// Pop the topmost entry off the JIT entry chain.
///
/// Should be called immediately after a JIT call returns, regardless of
/// success / failure / panic unwind. Returns the popped entry SP for
/// diagnostic purposes.
pub fn pop_jit_entry() -> Option<usize> {
    pop_jit_entry_impl(false)
}

/// [`pop_jit_entry`], or its door form (see [`door_history_note`]).
pub(in crate::jit::conservative_roots) fn pop_jit_entry_impl(door: bool) -> Option<usize> {
    DOOR_BLOCK.with(|b| pop_jit_entry_in(b, door))
}

/// [`pop_jit_entry_impl`] with the thread's [`DoorBlock`] in hand.
#[inline]
pub(in crate::jit::conservative_roots) fn pop_jit_entry_in(b: &DoorBlock, door: bool) -> Option<usize> {
    // Chain mutation = JIT boundary: invalidate the per-thread scan cache.
    note_jit_boundary();
    let (popped, remaining) = {
        let mut v = b.chain.borrow_mut();
        let p = v.pop();
        if p.is_some() && !door {
            note_chain_mutation();
        }
        // Mirror now tracks the entry that became top again.
        reload_top_rbp_cache(&v);
        (p, v.len())
    };
    if let Some(entry) = popped {
        GLOBAL_JIT_DEPTH.dec_token(entry.depth_stripe);
        // Door form: the mutation count, the residue mark and the delivery
        // floor cell in one `JIT_CHAIN_HISTORY` access. The residue mark is
        // only read by this thread's own later scans, so noting it before the
        // counters below is the same answer.
        let door_cell = if door {
            door_history_note(&b.history, Some(entry.entry_sp), remaining == 0)
        } else {
            0
        };
        if remaining == 0 {
            if door {
                store_delivery_floor(door_cell, 0);
            } else {
                publish_delivery_rust_floor(0);
            }
        }
        publish_self_jit_depth_in(&b.depth_slot, remaining);
        thread_state::record_transition(
            leaving_compiled_state(remaining),
            "jit::conservative_roots::pop_jit_entry",
        );
        // A one-counter door entry (t1 wave 8, lane I25) has one count,
        // `DOOR_DEPTH`, settled by `jit_execution_leave` below; it is the
        // quiescence depth's too. Decided by the entry, not by `door`: a
        // guard's descendant-pop loop can pop an entry of the other form. The
        // tally, likewise, follows the entry's push (`tally_skipped`).
        if !entry.exec_token.counts_door_depth() {
            if entry.tally_skipped {
                cratonvm_gc::gc_quiescence::leave_uncounted();
            } else {
                cratonvm_gc::gc_quiescence::leave();
            }
        }
        // P1 code-cache retirement: `jit_execution_leave` is also the
        // retirement queue's wake-up. It records this thread's return to depth
        // 0 and drains when that can release something, so this hot path owes
        // the queue nothing more.
        cratonvm_jit::jit_execution_leave(entry.exec_token);
        if !door {
            note_jit_residue(entry.entry_sp);
        }
        Some(entry.entry_sp)
    } else {
        None
    }
}

/// Round-7 corruption fix: self-heal a LEAKED `JitEntryGuard`.
///
/// When a JIT call's RAII `Drop`/[`pop_jit_entry`] is bypassed (an
/// abandoned compiled-callee frame — e.g. a non-local exit through the
/// JIT return path), a stale entry is left on the chain **and** the
/// `gc_quiescence` counter stays permanently elevated. A wedged
/// quiescence forces the GC onto the *non-moving* young sweep on every
/// collection, which is the heap corruptor (verified round 7:
/// `CRATONVM_DBG_FORCE_MOVING=1` → 0 corruption vs a deterministic 211
/// under `CRATONVM_DBG_GC_STRESS`).
///
/// This prunes every chain entry that has *provably returned*. The
/// conservative scanner's invariant (see [`scan_active_jit_frames`]) is
/// that a **live** JIT spill region lies at or above the scanner's
/// current SP — the native stack grows downward, so a live ancestor
/// frame's `entry_sp` is always `>= scanner_sp`. An entry whose captured
/// `entry_sp` is strictly **below** `scanner_sp` therefore cannot belong
/// to any live frame: its call has returned without popping. Removing
/// such entries (and matching each one with a `GLOBAL_JIT_DEPTH`
/// decrement + `gc_quiescence::leave()`) lets quiescence fall back to the
/// true live count, so the moving collector resumes the moment no JIT
/// frame is genuinely live.
///
/// Soundness: the predicate **never** removes a live frame (a live frame
/// always satisfies `entry_sp >= scanner_sp`), so genuine JIT activity
/// still correctly keeps quiescence active and the non-moving sweep
/// engaged. Returns the number of stale entries reclaimed.
pub fn prune_returned_jit_entries(scanner_sp: usize) -> usize {
    let mut remaining = 0usize;
    // The counter tokens of every pruned entry, so each decrement lands on the
    // stripe its push raised. `Vec::new` does not allocate until a prune
    // actually removes something, which is the rare case.
    let mut pruned_tokens: Vec<(
        cratonvm_types::striped_counter::StripeToken,
        cratonvm_jit::JitExecutionToken,
        bool,
    )> = Vec::new();
    let pruned = JIT_ENTRY_CHAIN.with(|c| {
        let mut v = c.borrow_mut();
        let before = v.len();
        // Keep only entries that could still be live (spill region at or
        // above the scanner SP). Entries below it have provably returned.
        for e in v.iter().filter(|e| e.entry_sp < scanner_sp) {
            note_jit_residue(e.entry_sp);
            pruned_tokens.push((e.depth_stripe, e.exec_token, e.tally_skipped));
        }
        v.retain(|e| e.entry_sp >= scanner_sp);
        let pruned = before - v.len();
        if pruned > 0 {
            note_chain_mutation();
        }
        // Mirror tracks whatever entry is top after pruning — but ONLY when
        // pruning actually changed the top.
        //
        // This reload used to be unconditional, and that single line is what
        // kept the moving young generation switched off. The mirror is written
        // by each compiled prologue (`mov gs:[disp], rbp`, or the
        // `jit_frame_record` helper); `PreciseFrameInfo::exact_rbp` is only a
        // SNAPSHOT of it, taken in `push_entry_full` when an entry stops being
        // top. For the entry that is *currently* top nothing has ever written
        // that field, so it still holds the `0` from `enter_with_compiled`.
        //
        // `refresh_moving_young_coverage_for_current_thread` calls this
        // function first and then reads the mirror. With the unconditional
        // reload, a no-op prune overwrote the live RBP with that `0` on the way
        // in, and the coverage proof then failed itself with MISSING_EXACT_RBP
        // — measured as 100% of fallbacks (67/67 on `BinTreesClassic 18` at
        // `-Xmx512m`, 58/58 at depth 16, 2/2 at depth 14) with `top_rbp=0x0`
        // logged for a frame that had 7 oop maps and a live sp-id slot. Nothing
        // was wrong with the frame; the verifier had erased its own input.
        if pruned > 0 {
            reload_top_rbp_cache(&v);
        }
        remaining = v.len();
        pruned
    });
    if pruned > 0 {
        // Chain mutation = JIT boundary: invalidate the per-thread scan cache.
        note_jit_boundary();
        // P1 shadow record: the self-heal is a real (if belated) observation
        // that those frames have returned — same edge `pop_jit_entry` records,
        // once for the whole batch.
        thread_state::record_transition(
            leaving_compiled_state(remaining),
            "jit::conservative_roots::prune_returned_jit_entries",
        );
    }
    for (depth_stripe, exec_token, tally_skipped) in pruned_tokens {
        settle_reclaimed_entry_counts(depth_stripe, exec_token, tally_skipped);
    }
    if pruned > 0 {
        publish_self_jit_depth(remaining);
    }
    if pruned > 0 {
        // `info!`, not `debug!`. Under `release_max_level_info` a
        // `debug!` is compiled out of every release binary, so a count of
        // LEAKED JIT entries -- an anomaly worth knowing about -- could
        // never reach anyone running a released VM.
        //
        // Promoted only after measuring the rate, because the objection to
        // promoting an anomaly line is that it might flood. It does not:
        // across the 88-vector regression suite, `GcStress` under ZGC /
        // Generational / G1 at -Xmx96m (3, 16 and several collections
        // respectively), and the GPU runtime-stress and residency-gc gates,
        // this fired ZERO times. That measures "does not flood"; it does
        // not prove the line can fire, which is true of any rare-anomaly
        // report and is still strictly better than invisible-by-
        // construction.
        tracing::info!(
            "pruned {} leaked JIT entry/entries (returned frames below scanner \
             SP {:#x}); quiescence healed to live count",
            pruned,
            scanner_sp,
        );
        // P1 code-cache retirement: a leaked `JitEntryGuard` is exactly what
        // wedges the retirement queue, and each leave above pumps the drain only
        // on its thread's schedule. Ask once, explicitly, now that the wedge is
        // gone — retention is the fail-safe, so a missed wake-up would show up
        // as code-cache growth rather than as a crash. See
        // `code_cache_lifecycle`'s §1.3.
        if crate::jit::code_cache_lifecycle::pending_retirements() != 0 {
            crate::jit::code_cache_lifecycle::sweep_if_quiescent();
        }
    }
    pruned
}

/// Settle the process-wide counts of a chain entry removed without its pop
/// (`prune_returned_jit_entries`, a leaked slice entry at `ChainSegment`'s
/// drop), in the pop's order. A one-counter door entry (t1 wave 8, lane I25)
/// raised only `DOOR_DEPTH`, which `jit_execution_leave` lowers; calling the
/// collector's untokened `leave` for it would lower THIS thread's stripe of
/// the quiescence depth, which a live entry of the other form may own. A door
/// entry (`tally_skipped`) never counted an enter, so it settles with
/// `leave_uncounted` (t1 wave 9, lane JITA).
pub(in crate::jit::conservative_roots) fn settle_reclaimed_entry_counts(
    depth_stripe: cratonvm_types::striped_counter::StripeToken,
    exec_token: cratonvm_jit::JitExecutionToken,
    tally_skipped: bool,
) {
    GLOBAL_JIT_DEPTH.dec_token(depth_stripe);
    if !exec_token.counts_door_depth() {
        if tally_skipped {
            cratonvm_gc::gc_quiescence::leave_uncounted();
        } else {
            cratonvm_gc::gc_quiescence::leave();
        }
    }
    cratonvm_jit::jit_execution_leave(exec_token);
}

/// RAII guard that pairs `push_jit_entry` with `pop_jit_entry` on drop.
///
/// Use this at every JIT call site so a panic unwinding through the
/// transition still cleans up the entry chain:
///
/// ```ignore
/// let _guard = JitEntryGuard::enter();
/// let result = std::panic::catch_unwind(|| unsafe { compiled.try_call(args) });
/// // _guard drops here, popping the entry whether result is Ok or Err
/// ```
pub struct JitEntryGuard {
    /// Depth at the moment of construction; used as a sanity check on drop.
    pub(in crate::jit::conservative_roots) depth_at_push: usize,
    pub(in crate::jit::conservative_roots) active_class_id: Option<cratonvm_types::jit_activation::Activation>,
    /// Native-allocation unwind permission suspended for the duration of this
    /// compiled frame, restored on drop. A JIT frame carries no unwind
    /// information, so a panic raised beneath one would terminate the process
    /// instead of reaching the `catch_unwind` that would have converted it into
    /// a Java exception (the same constraint that makes `jit_throw_aioobe`
    /// signal through a thread-local instead of panicking). `0` — the case
    /// where no native call is in flight — means nothing was written and
    /// nothing needs restoring. See `crate::runtime::native_oom`.
    pub(in crate::jit::conservative_roots) saved_native_unwind: u32,
    /// gen r4w6/oomjit6: the exceptional-stash ENTRY FLOOR this guard
    /// replaced, restored on drop. `None` when the feature is disabled
    /// (`CRATONVM_JIT_LOCAL_HANDLER_DROP_ORPHANS=0`), in which case the guard
    /// never touched it. See [`exceptional_stash_floor`].
    pub(in crate::jit::conservative_roots) saved_exc_floor: Option<usize>,
    /// Built by [`JitEntryGuard::enter_door`] (`CRATONVM_JIT_DOOR_THREAD_BLOCK`):
    /// pushed and popped through the door form ([`door_history_note`]).
    pub(in crate::jit::conservative_roots) door: bool,
}

// ---------------------------------------------------------------------------
// gen r4w6/oomjit6 (2026-09-24): the exceptional-stash entry floor
// ---------------------------------------------------------------------------
//
// `docs/feature-designs/jit-ir-optimizations.md`,
// item 1: a compiled callee that publishes its exceptional frame on its way
// out, to a compiled caller that then CATCHES locally, leaves that frame
// stashed with no consumer; every object it names stays a GC root until the
// next take on the thread. The page's precise rule needs "the stash top from
// when the catching activation was entered". Every Rust -> compiled
// transition constructs a `JitEntryGuard`, so the guard is where that is
// recorded: each guard saves the enclosing floor, publishes the current stash
// depth as the floor for its own span, and restores the enclosing floor on
// drop. The compiled local-handler commit (`jit::helpers`) truncates the stash
// to the innermost floor. `cratonvm_jit::deopt::truncate_exceptional_stash_to`
// carries the argument that this drops only frames of the throw being caught.

/// No JIT door is open on this thread (or the floor is not being kept).
pub(in crate::jit::conservative_roots) const NO_EXC_STASH_FLOOR: usize = usize::MAX;

/// The exceptional-stash depth recorded by the innermost live
/// `JitEntryGuard`, or [`NO_EXC_STASH_FLOOR`]. A field of [`DOOR_BLOCK`].
pub(in crate::jit::conservative_roots) const EXC_STASH_FLOOR: DoorField<std::cell::Cell<usize>> = DoorField(|b| &b.exc_floor);

/// The door's per-thread records in one thread-local (stage 4 of
/// `i25-L2-proposal-one-thread-block-for-the-interpreter-to-compiled-door`,
/// tier-2 page `t1-INTB`): a push or a pop through [`JitEntryGuard`] makes one
/// `DOOR_BLOCK` access for the chain, the history, the depth slot and the
/// exceptional-stash floor instead of one per record. The owning thread is the
/// only writer. A peer sees what the owner publishes (the depth slot, the band)
/// and, for a thread it holds in a native, a copy of the chain read through the
/// address of the `chain` field the owner published once
/// ([`current_thread_jit_cells`], [`JitChainSnapshot::of_held_thread`]); that
/// address is the field of a const-initialised thread-local, so it is stable for
/// the thread's life (`t1intb_records_share_one_block_and_the_published_chain_address_is_stable`).
/// Not `#[repr(C)]`: nothing addresses a field by offset.
pub(in crate::jit::conservative_roots) struct DoorBlock {
    pub(in crate::jit::conservative_roots) chain: RefCell<Vec<JitFrameChainEntry>>,
    pub(in crate::jit::conservative_roots) history: std::cell::Cell<ChainHistory>,
    pub(in crate::jit::conservative_roots) top_rbp: std::cell::Cell<usize>,
    pub(in crate::jit::conservative_roots) exc_floor: std::cell::Cell<usize>,
    pub(in crate::jit::conservative_roots) depth_slot: std::cell::RefCell<Option<JitDepthSlot>>,
}

thread_local! {
    pub(in crate::jit::conservative_roots) static DOOR_BLOCK: DoorBlock = const {
        DoorBlock {
            chain: RefCell::new(Vec::new()),
            history: std::cell::Cell::new(ChainHistory::new()),
            top_rbp: std::cell::Cell::new(0),
            exc_floor: std::cell::Cell::new(NO_EXC_STASH_FLOOR),
            depth_slot: std::cell::RefCell::new(None),
        }
    };
}

/// One field of [`DOOR_BLOCK`], spelled like the `thread_local!` it replaced,
/// so its sites keep reading `KEY.with(..)` / `KEY.try_with(..)`.
#[derive(Clone, Copy)]
pub(in crate::jit::conservative_roots) struct DoorField<T: 'static>(fn(&DoorBlock) -> &T);

impl<T: 'static> DoorField<T> {
    #[inline(always)]
    pub(in crate::jit::conservative_roots) fn with<R>(self, f: impl FnOnce(&T) -> R) -> R {
        DOOR_BLOCK.with(|b| f((self.0)(b)))
    }

    #[inline(always)]
    pub(in crate::jit::conservative_roots) fn try_with<R>(self, f: impl FnOnce(&T) -> R) -> Result<R, std::thread::AccessError> {
        DOOR_BLOCK.try_with(|b| f((self.0)(b)))
    }
}

/// `CRATONVM_JIT_LOCAL_HANDLER_DROP_ORPHANS=0` disables the floor that lets a
/// compiled `catch` discard exceptional frames its own callees published during
/// the throw it caught. The floor is scoped to the catching activation, so
/// frames published before entry remain live. See
/// `truncate_exceptional_stash_to`. Read once.
#[inline]
pub(crate) fn local_handler_drops_orphans() -> bool {
    static ON: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&ON, || {
        true
    })
}

/// Record the current exceptional-stash depth as the floor of a JIT door that
/// is being entered; returns the floor it replaced, for the guard to restore.
/// `None` (nothing written) when the floor is not being kept.
#[inline]
pub(in crate::jit::conservative_roots) fn enter_exc_stash_floor() -> Option<usize> {
    if !local_handler_drops_orphans() {
        return None;
    }
    record_exc_stash_floor(cratonvm_jit::deopt::exceptional_stash_depth())
}

/// [`enter_exc_stash_floor_at`] with the floor cell in hand (the door, which
/// already holds the [`DoorBlock`]).
#[inline]
pub(in crate::jit::conservative_roots) fn enter_exc_stash_floor_in(c: &std::cell::Cell<usize>, depth: Option<usize>) -> Option<usize> {
    local_handler_drops_orphans()
        .then(|| c.replace(depth.unwrap_or_else(cratonvm_jit::deopt::exceptional_stash_depth)))
}

/// Publish `depth` as the innermost door's floor; returns the floor it
/// replaced (`None` only if the thread-local is being torn down).
pub(in crate::jit::conservative_roots) fn record_exc_stash_floor(depth: usize) -> Option<usize> {
    EXC_STASH_FLOOR.try_with(|c| c.replace(depth)).ok()
}

/// The floor as recorded, whatever the flag says; `None` for "no door".
pub(in crate::jit::conservative_roots) fn recorded_exc_stash_floor() -> Option<usize> {
    let floor = EXC_STASH_FLOOR
        .try_with(std::cell::Cell::get)
        .unwrap_or(NO_EXC_STASH_FLOOR);
    (floor != NO_EXC_STASH_FLOOR).then_some(floor)
}

/// The exceptional-stash floor of the innermost live JIT door on this thread,
/// or `None` when no door is open or the floor is not being kept.
pub(crate) fn exceptional_stash_floor() -> Option<usize> {
    if !local_handler_drops_orphans() {
        return None;
    }
    recorded_exc_stash_floor()
}

impl JitEntryGuard {
    /// Push a new conservative JIT entry and return a guard that will
    /// pop it on drop. The entry has `precise = None` so the GC root
    /// walker uses the conservative range scan for this frame.
    ///
    /// **Inlined intentionally**: the SP capture must resolve to a probe in
    /// the caller's frame, not in this function's frame, otherwise the SP
    /// recorded in the chain would point into freed stack memory the moment
    /// `enter` returns.
    #[inline(always)]
    pub fn enter() -> Self {
        let sp = current_stack_pointer();
        let depth_at_push = push_jit_entry_at(sp);
        Self {
            depth_at_push,
            active_class_id: None,
            saved_native_unwind: crate::runtime::native_oom::suspend_for_jit(),
            saved_exc_floor: enter_exc_stash_floor(),
            door: false,
        }
    }

    /// NEW-12: push a JIT entry that carries precise-frame metadata.
    ///
    /// When the root walker encounters an entry of this shape it uses
    /// the compiled method's oop maps to enumerate oops exactly where
    /// available. Map-empty methods retain the same metadata so their
    /// conservative fallback can scan a bounded compiled-frame band.
    ///
    /// **Safety**: the caller must hold a live borrow of `cm` for the
    /// duration of the returned guard. In practice this is trivial:
    /// the interpreter's JIT call site owns `&CompiledMethod` and the
    /// guard is dropped immediately after the call returns. The
    /// compiled method itself is kept alive by the JIT cache's Arc
    /// holding, so even after the borrow ends the pointer remains
    /// valid for any in-flight GC walker.
    #[inline(always)]
    pub fn enter_with_compiled(cm: &cratonvm_jit::CompiledMethod) -> Self {
        // The door-form push for nested entries (W5A, `nested_door_forms`) is held back: it did not move
        // the javac harness, and one `AotIntegrationTests` run died with a bad reference in compiled code
        // (docs/known-issues/jit/aot-integration-intermittent-sigsegv.md).
        Self::enter_with_compiled_at(cm, None)
    }

    /// `(door form, one depth counter)` for [`Self::enter_with_compiled`]: the
    /// same two switches the per-VM door reads (`VmConfig::jit_door_thread_block`,
    /// `jit_door_one_depth_counter`), read once per process here because the
    /// reentrant callers hold no VM. The one counter is meaningful only with
    /// the door form.
    #[inline]
    #[allow(dead_code)] // held back, see enter_with_compiled
    pub(in crate::jit::conservative_roots) fn nested_door_forms() -> (bool, bool) {
        static DOOR: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
        static ONE: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
        let door = cratonvm_jit::memo_flag::memo_bool(&DOOR, || {
            true        });
        let one = door
            && cratonvm_jit::memo_flag::memo_bool(&ONE, || {
                true            });
        (door, one)    }

    /// [`Self::enter_with_compiled`] plus the interpreter depth this compiled
    /// frame sits at, so Java-level stack walks can place it (see
    /// [`JitFrameChainEntry::interp_depth`]). `None` inherits the enclosing
    /// entry's depth, which is what a JIT→JIT dispatch wants.
    #[inline(always)]
    pub fn enter_with_compiled_at(
        cm: &cratonvm_jit::CompiledMethod,
        interp_depth: Option<usize>,
    ) -> Self {
        Self::enter_compiled_form(cm, interp_depth, false, false)
    }

    /// Body of [`Self::enter_with_compiled_at`] and [`Self::enter_with_compiled`];
    /// `door`/`one_counter` pick the push form ([`push_entry_in`]). Inlined so
    /// the captured SP is the caller's frame.
    #[inline(always)]
    pub(in crate::jit::conservative_roots) fn enter_compiled_form(
        cm: &cratonvm_jit::CompiledMethod,
        interp_depth: Option<usize>,
        door: bool,
        one_counter: bool,
    ) -> Self {
        // Retain frame metadata even when this method has no oop-map entries.
        // The prologue still records its RBP whenever precise maps are enabled,
        // and the conservative fallback can then scan this compiled frame's
        // bounded spill band instead of every intervening interpreter/Rust
        // frame. Map-empty entries simply contribute no exact slots.
        let sp = current_stack_pointer();
        let entry = JitFrameChainEntry {
            entry_sp: sp,
            depth_stripe: cratonvm_types::striped_counter::StripeToken::UNSET,
            exec_token: cratonvm_jit::JitExecutionToken::UNSET,
            tally_skipped: false,
            interp_depth: match interp_depth {
                Some(d) => u32::try_from(d).unwrap_or(u32::MAX - 1),
                None => INTERP_DEPTH_INHERIT,
            },
            precise: Some(PreciseFrameInfo {
                compiled_method: cm as *const cratonvm_jit::CompiledMethod,
                frame_base: sp,
                entry_ptr: cm.entry_ptr(),
                exact_rbp: 0,
                exact_cm_id: 0,
                not_entrant_ret_pc: 0,
            }),
        };
        // t1 INTB: one `DOOR_BLOCK` access for the push and the exceptional-stash
        // floor, as in `enter_door`. The stash read does not depend on the
        // activation slot or the unwind permission taken below, and the push has
        // always preceded it.
        let (depth_at_push, saved_exc_floor) = DOOR_BLOCK.with(|b| {
            let depth = push_entry_in(b, entry, door, one_counter);
            (depth, enter_exc_stash_floor_in(&b.exc_floor, None))
        });
        Self {
            depth_at_push,
            // The artifact itself carries its declaring class, so marking it
            // active is a per-thread slot write — no map, no global lock.
            active_class_id: cratonvm_types::jit_activation::enter(cm.owner_class_id),
            saved_native_unwind: crate::runtime::native_oom::suspend_for_jit(),
            saved_exc_floor,
            door,
        }
    }

    /// [`Self::enter_with_compiled_at`] for the interpreter-to-compiled door
    /// under `CRATONVM_JIT_DOOR_THREAD_BLOCK` (t1 wave 5, lane S): the same
    /// entry, pushed through the door form ([`door_history_note`]), with the
    /// exceptional-stash depth the door already read (`exc_depth`, `None` to
    /// read it here). Popped through the door form by `Drop`.
    ///
    /// `one_counter` (t1 wave 8, lane I25; `CRATONVM_JIT_DOOR_ONE_DEPTH_COUNTER`,
    /// per VM, default ON since j27 2026-10-28): the entry's three process-wide counts are one
    /// `DOOR_DEPTH` increment ([`push_entry_form`]).
    #[inline(always)]
    pub fn enter_door(
        cm: &cratonvm_jit::CompiledMethod,
        interp_depth: usize,
        exc_depth: Option<usize>,
        one_counter: bool,
    ) -> Self {
        let sp = current_stack_pointer();
        let entry = JitFrameChainEntry {
            entry_sp: sp,
            depth_stripe: cratonvm_types::striped_counter::StripeToken::UNSET,
            exec_token: cratonvm_jit::JitExecutionToken::UNSET,
            tally_skipped: false,
            interp_depth: u32::try_from(interp_depth).unwrap_or(u32::MAX - 1),
            precise: Some(PreciseFrameInfo {
                compiled_method: cm as *const cratonvm_jit::CompiledMethod,
                frame_base: sp,
                entry_ptr: cm.entry_ptr(),
                exact_rbp: 0,
                exact_cm_id: 0,
                not_entrant_ret_pc: 0,
            }),
        };
        // One `DOOR_BLOCK` access for the push and the floor (stage 4). The
        // floor's stash read does not depend on the activation slot or the
        // unwind permission taken below.
        let (depth_at_push, saved_exc_floor) = DOOR_BLOCK.with(|b| {
            let depth = push_entry_in(b, entry, true, one_counter);
            (depth, enter_exc_stash_floor_in(&b.exc_floor, exc_depth))
        });
        Self {
            depth_at_push,
            active_class_id: cratonvm_types::jit_activation::enter(cm.owner_class_id),
            saved_native_unwind: crate::runtime::native_oom::suspend_for_jit(),
            saved_exc_floor,
            door: true,
        }
    }
}

impl Drop for JitEntryGuard {
    fn drop(&mut self) {
        // A compiled OSR body can leave through a non-local deopt/exception
        // path after entering another compiled body.  That nested bridge does
        // not always get a Rust frame to run its own guard's Drop, so merely
        // popping once here would remove the leaked child and strand *this*
        // guard on the chain.  Restore the depth that this guard established
        // first, then remove the guard itself.  The chain is thread-local and
        // stack-disciplined, so entries above `depth_at_push` can only be
        // abandoned descendants of this invocation; never touch outer frames.
        // One `DOOR_BLOCK` access for the whole drop (stage 4).
        DOOR_BLOCK.with(|b| self.drop_in(b));
    }
}

impl JitEntryGuard {
    /// [`Drop::drop`] with the thread's [`DoorBlock`] in hand.
    #[inline]
    pub(in crate::jit::conservative_roots) fn drop_in(&mut self, b: &DoorBlock) {
        let chain_depth = || b.chain.borrow().len();
        let mut depth = chain_depth();
        while depth > self.depth_at_push {
            let _ = pop_jit_entry_in(b, self.door);
            depth = chain_depth();
        }
        // Pop only if THIS guard's entry is still on the chain. The chain can
        // be SHORTER than `depth_at_push` when `prune_returned_jit_entries`
        // already reclaimed this entry (and settled its counters). An
        // unconditional pop then removed the enclosing guard's LIVE entry
        // instead -- decrementing its stripe a second time, dropping a live
        // compiled frame out of the root walk, and leaving that outer guard to
        // pop the next one out in turn. The cascade only ever moves outward, so
        // stop it at the first guard that sees it.
        if depth == self.depth_at_push {
            let popped = pop_jit_entry_in(b, self.door);
            debug_assert!(
                popped.is_some(),
                "JitEntryGuard::drop: chain underflow (was depth {})",
                self.depth_at_push
            );
        } else {
            tracing::warn!(
                "JitEntryGuard::drop: entry at depth {} was already reclaimed \
                 (chain depth {}); not popping an enclosing entry",
                self.depth_at_push,
                chain_depth(),
            );
        }
        if let Some(activation) = self.active_class_id.take() {
            cratonvm_types::jit_activation::exit(activation);
        }
        // Restore the native-allocation unwind permission this compiled frame
        // suspended. `0` means the entry wrote nothing (no native call was in
        // flight), so the common pure-JIT path skips the write entirely.
        if self.saved_native_unwind != 0 {
            crate::runtime::native_oom::restore(self.saved_native_unwind);
        }
        // gen r4w6/oomjit6: hand the enclosing door its floor back. A guard
        // that never recorded one (flag off) writes nothing.
        if let Some(prev) = self.saved_exc_floor.take() {
            b.exc_floor.set(prev);
        }
    }
}

// ---------------------------------------------------------------------------
// The WS1 per-thread JIT-scan cache, deleted (round 11 wave 15, lane fib;
// `r11w14-rt-dead-jit-scan-cache-deletion-patch`)
// ---------------------------------------------------------------------------
//
// It reused the previous `scan_active_jit_frames` roots while a per-thread
// Rust<->JIT boundary generation was unchanged. It could not hit: every
// production caller invalidates immediately before scanning
// (`r11-rt-root-scan-residue` items 3 and 7), and `scan_prof` measured 0.0%
// hits on every run it was pointed at (see that module). Every scan is now
// fresh, which is what the GC-authoritative scans already required
// (`invalidate_scan_cache_for_gc`'s doc records the corruption a stale reuse
// caused). `note_jit_boundary` survives as a no-op for its helper callers.

/// A Rust<->JIT boundary crossing (every JIT runtime-helper entry and every
/// JIT entry-chain mutation). A no-op since the JIT-scan cache, its only
/// reader, was deleted (see the note above); kept so its callers in
/// `vm/src/jit/helpers.rs` need not change in the same commit.
#[inline(always)]
pub fn note_jit_boundary() {}

/// Prepare this thread for a GC-authoritative JIT root scan. **Must be called
/// before every GC-authoritative root collection** (the current thread's
/// `collect_roots`, and a parked thread's pre-STW `update_root_snapshot`
/// publish).
///
/// Its first job, discarding the JIT-scan cache's snapshot, went with the
/// cache (round 11 wave 15; see the note above [`note_jit_boundary`]). That
/// cache reused an earlier conservative scan's roots while no Rust<->JIT
/// boundary had been crossed, and the band it scanned also covers the
/// interpreter / native / Rust stack BELOW a JIT frame, which mutates without
/// any boundary crossing and can hold the only reference to a fresh object: a
/// reused snapshot silently dropped such roots, and the non-moving young sweep
/// then reclaimed live objects ("inconsistent header / implausible object
/// size" under GC stress). Every scan is fresh now. What is left:
///
/// ## H2-CID0 (2026-08-05) — the unregistered-frame memo
///
/// [`UNREG_JIT_MEMO`] memoizes the *other* per-native-call scan: the detection
/// of a JIT frame that is live without having pushed an entry guard. It is the
/// cache that decides whether such a frame's oops are marked at all. Miss the
/// frame and the young non-moving sweep frees objects it alone holds.
///
/// The stale-snapshot argument above applies to it verbatim, and more sharply:
/// a stale root snapshot drops references, while a stale unregistered-frame
/// verdict drops an entire frame's worth of them AND leaves the collector
/// believing it may relocate. Reset it here so the authoritative scan cannot
/// inherit a verdict about a stack that has since been rewritten.
///
/// Cost is one full band rescan per GC-authoritative root collection, not per
/// native call — which is exactly the trade this function already documents.
/// The `UnregMemo::hiwater` rule still does the between-collections tightening;
/// it cannot replace this, because it only reacts to stack-pointer rises it
/// happens to OBSERVE, and a return-and-re-descend that falls entirely between
/// two snapshots is invisible to it (measured: 972 suppressed detections in one
/// run with hi-water enabled).
///
/// `CRATONVM_JIT_UNREG_MEMO_GC_RESET=0` restores the pre-fix behaviour (the
/// memo surviving authoritative scans) so the fix can be A/B'd against the
/// failure in one binary. Without a switch this would be the only change here
/// that could never be shown to work.
#[inline]
pub fn invalidate_scan_cache_for_gc() {
    // Mark the following scan as one a collector will consume, so the audit can
    // attribute a suppression to the path where it is a defect. Set even when
    // the reset is disabled — that is exactly the arm whose count we want.
    UNREG_JIT_AUTHORITATIVE.with(|c| c.set(true));
    if unreg_memo_gc_reset_enabled() {
        UNREG_JIT_MEMO.with(|c| c.set(UnregMemo::new()));
    }
}

/// `CRATONVM_JIT_UNREG_MEMO_GC_RESET=0` — kill switch for the authoritative
/// reset above.
pub(in crate::jit::conservative_roots) fn unreg_memo_gc_reset_enabled() -> bool {
    static ON: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&ON, || {
        true
    })
}

/// Cached `CRATONVM_DBG_NO_PRUNE` gate. `scan_active_jit_frames` runs on every
/// object-returning native call (via `update_root_snapshot`); the previous
/// uncached `env::var_os` was a `GetEnvironmentVariableW` syscall per native
/// call — a kernel transition on the hottest dispatch path. Cached once.
pub(in crate::jit::conservative_roots) fn dbg_no_prune() -> bool {
    static ON: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&ON, || false)}

/// H2-CID0 (2026-08-05) — times the unregistered-JIT-frame memo said "clean"
/// while a real scan of the same range found a frame.
///
/// Only counted under `CRATONVM_DBG_UNREG_MEMO_AUDIT=1`. Non-zero means the
/// memo suppressed a detection that was true: that frame's oops went
/// unmarked and the collector was never told to avoid moving them.
pub static UNREG_MEMO_SUPPRESSED: AtomicUsize = AtomicUsize::new(0);
/// Times the memo short-circuited a scan at all (the audit's denominator — a
/// zero numerator is only meaningful beside a non-zero denominator).
pub static UNREG_MEMO_SHORTCIRCUITS: AtomicUsize = AtomicUsize::new(0);

/// ENGAGEMENT census for the A5 band probe, printed at exit under
/// `CRATONVM_DBG_A5_ENGAGEMENT=1`.
///
/// Four earlier attempts to make this probe cheaper were judged inert from a
/// profile that did not move. That is an inference, not a measurement: a memo
/// that never engages and a memo that engages but saves nothing look identical
/// in a flat profile. These say which. Counted per CALL of the coverage probe.
pub static A5_PROBE_CALLS: AtomicUsize = AtomicUsize::new(0);
/// Calls where the memo would have answered "already clean" (no scan needed).
pub static A5_PROBE_MEMO_CLEAN: AtomicUsize = AtomicUsize::new(0);
/// Calls where the memo could bound the scan to an incremental band.
pub static A5_PROBE_MEMO_BANDED: AtomicUsize = AtomicUsize::new(0);
/// Calls that fell through to a FULL scan because the code-range set changed.
pub static A5_PROBE_FULL_RESCAN: AtomicUsize = AtomicUsize::new(0);
/// Total band words the probe was asked to scan (the quantity a memo shrinks).
pub static A5_PROBE_WORDS: AtomicUsize = AtomicUsize::new(0);

pub fn a5_engagement_enabled() -> bool {
    static ON: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&ON, || false)}

/// Print the A5 FALLBACK census — how many moving-young cycles the A5 probe
/// diverted, and how many of those it diverted on a word that was not a live
/// frame at all.
///
/// Deliberately NOT folded into [`report_a5_engagement`], which answers a
/// different question ("does the memo engage") on a different flag. And
/// deliberately called from BOTH VM exit arms rather than only the `Ok(())`
/// one: the run this census exists to explain is a run whose test FAILED, and
/// on that arm `report_a5_engagement` never executes. A census that prints
/// only when nothing went wrong is a census of the uninteresting population.
/// `Once`-guarded, so having it on both arms is correct.
pub fn report_a5_fallback_census() {
    #[cfg(any(target_os = "windows", target_os = "linux"))]
    {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            if !a5_fallback_dbg() {
                return;
            }
            let (hits, residue, shapeless, filtered) = a5_fallback_census();
            eprintln!(
                "[a5-fallback] TOTALS hits={hits} residue={residue} shapeless={shapeless} \
                 filtered={filtered} residue_filter={} shape_filter={}",
                a5_residue_filter_enabled(),
                a5_shape_filter_enabled(),
            );
        });
    }
}

/// Print the A5 engagement census. Called from the VM's shutdown path.
pub fn report_a5_engagement() {
    if !a5_engagement_enabled() {
        return;
    }
    let calls = A5_PROBE_CALLS.load(Ordering::Relaxed);
    if calls == 0 {
        eprintln!("[a5-engagement] calls=0 (probe never ran)");
        return;
    }
    eprintln!(
        "[a5-engagement] calls={calls} memo_clean={} memo_banded={} full_rescan={} words={}",
        A5_PROBE_MEMO_CLEAN.load(Ordering::Relaxed),
        A5_PROBE_MEMO_BANDED.load(Ordering::Relaxed),
        A5_PROBE_FULL_RESCAN.load(Ordering::Relaxed),
        A5_PROBE_WORDS.load(Ordering::Relaxed),
    );
}
/// H2-CID0 (2026-08-06) — suppressions on a scan a collector CONSUMES.
///
/// The number that judges the fix. Total suppressions are dominated by
/// ordinary per-native-call snapshots, which the memo is supposed to
/// short-circuit and which no collector marks from; a suppression there is the
/// optimisation working. A suppression HERE is a live JIT frame's oops missing
/// from the root set a collection is about to use.
///
/// Expected exactly 0 with the authoritative reset enabled, and that is a
/// measurement of "impossible by construction", not a restatement of it.
pub static UNREG_MEMO_SUPPRESSED_AUTHORITATIVE: AtomicUsize = AtomicUsize::new(0);

/// `CRATONVM_DBG_UNREG_MEMO_AUDIT=1` — verify the memo instead of trusting it.
///
/// Runs the detection scan even on the fast path and only COUNTS the
/// disagreement. It marks nothing and changes no collector decision, so unlike
/// most instruments on this bug it cannot perturb the thing it measures; the
/// only cost is the scan itself.
pub(in crate::jit::conservative_roots) fn dbg_unreg_memo_audit() -> bool {
    static ON: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&ON, || false)}

/// H2-CID0 (2026-08-05) — the unregistered-JIT-frame memo, as a value.
///
/// Stack addresses grow DOWN, so "deeper" is numerically smaller and the
/// verified region is `[floor, stack_high)`.
///
/// * `verified_lo` — the depth at which a scan last came back clean.
/// * `hiwater` — the SHALLOWEST stack pointer observed since that scan.
///
/// The memo's justification is "nothing above our current stack pointer can
/// change while we are nested below it". Rising to `hiwater` pops every frame
/// below it, so only `[hiwater, stack_high)` is provably untouched; combined
/// with the scan's own reach the clean floor is `max(verified_lo, hiwater)`.
///
/// Using `verified_lo` alone — the pre-fix rule — claims the verdict holds for
/// the whole life of the thread, because `verified_lo` only ever moves deeper.
/// A thread that returns above it, enters an already-compiled method (no new
/// code range, no entry guard, so neither existing staleness check fires) and
/// descends again never scans that band again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::jit::conservative_roots) struct UnregMemo {
    pub(in crate::jit::conservative_roots) verified_lo: usize,
    pub(in crate::jit::conservative_roots) verified_ranges: usize,
    pub(in crate::jit::conservative_roots) hiwater: usize,
}

/// What the memo says to do for a root scan at `search_lo`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::jit::conservative_roots) enum UnregScan {
    /// The range is covered by a still-valid earlier verdict.
    AlreadyClean,
    /// Detect over `[search_lo, hi)`; `hi` is `stack_high` for a full rescan.
    Detect { hi: Option<usize> },
}

impl UnregMemo {
    pub(in crate::jit::conservative_roots) const fn new() -> Self {
        Self {
            verified_lo: usize::MAX,
            verified_ranges: 0,
            hiwater: 0,
        }
    }

    /// Clean floor: the deepest address the memo still vouches for.
    pub(in crate::jit::conservative_roots) fn floor(&self, hiwater_on: bool) -> usize {
        if hiwater_on {
            self.verified_lo.max(self.hiwater)
        } else {
            self.verified_lo
        }
    }

    /// Record this observation and decide what to scan.
    ///
    /// `Detect { hi: Some(floor) }` is the incremental band — only the stack
    /// that has appeared (or been rewritten) since the last clean verdict.
    pub(in crate::jit::conservative_roots) fn observe(&mut self, search_lo: usize, code_ranges: usize, hiwater_on: bool) -> UnregScan {
        if hiwater_on {
            self.hiwater = self.hiwater.max(search_lo);
        }
        let floor = self.floor(hiwater_on);
        if code_ranges == self.verified_ranges {
            if search_lo >= floor {
                return UnregScan::AlreadyClean;
            }
            return UnregScan::Detect { hi: Some(floor) };
        }
        // A new compilation invalidates the verdict outright: a slot that held
        // a plain value at the last scan can now sit where a new JIT code range
        // claims to start.
        //
        // MEASURED 2026-08-11, and left alone deliberately. This rule can be
        // argued away — the band is frozen (nothing above the current stack
        // pointer changes while the thread is nested below it) and a genuine
        // return address into range R requires R to have existed when the CALL
        // wrote it, so a range registered after the verdict can only produce a
        // FALSE positive. Removing the check was implemented, unit-tested and
        // measured on `DefaultCatalogAndSchemaTest`, which compiles
        // continuously across 132 SessionFactory bootstraps and so keeps this
        // memo permanently cold: `native_stack_has_jit_frame` read 35.2 BILLION
        // stack words with the check and 35.2 billion without it — byte for
        // byte no change, because the probes that dominate this workload come
        // from `refresh_moving_young_coverage_for_current_thread`, which
        // consults no memo at all. So the rule is a real inefficiency and it is
        // NOT the binding one; it stays until something measures it binding,
        // rather than trading heap-safety-critical behaviour for nothing.
        UnregScan::Detect { hi: None }
    }

    /// A scan starting at `search_lo` came back clean.
    pub(in crate::jit::conservative_roots) fn mark_clean(&mut self, search_lo: usize, code_ranges: usize) {
        self.verified_lo = search_lo;
        self.verified_ranges = code_ranges;
        // The verdict is fresh as of here, so the peak restarts from this
        // depth — otherwise an old peak would force rescans forever.
        self.hiwater = search_lo;
    }
}

/// `CRATONVM_JIT_UNREG_MEMO_HIWATER=0` — restore the pre-fix memo (kill switch
/// for A/B-ing the fix in one binary).
pub(in crate::jit::conservative_roots) fn unreg_memo_hiwater_enabled() -> bool {
    static ON: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&ON, || {
        true
    })
}

thread_local! {
    /// Is the running `scan_active_jit_frames` the COLLECTION's own root pass,
    /// rather than one of the per-native-call snapshot publishers?
    ///
    /// The above-chain conservative band is affordable once per collection and
    /// not once per native call, and only the collection's pass is what the
    /// collector marks from — so the two need telling apart. Set by
    /// [`scan_active_jit_frames_for_collection`], which is the entry point
    /// `memory::roots::collect_roots` uses.
    pub(in crate::jit::conservative_roots) static GC_ROOT_PASS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// True while the collection's own root pass is running on this thread.
#[inline]
pub fn gc_root_pass_active() -> bool {
    GC_ROOT_PASS.with(|c| c.get())
}

/// [`scan_active_jit_frames`] as the COLLECTION's root pass — the entry point
/// `memory::roots::collect_roots` calls, and the only one that scans the band
/// above the JIT entry chain. Restores the previous value rather than clearing,
/// so a nested call (there is none today) cannot silently downgrade the outer
/// pass.
pub fn scan_active_jit_frames_for_collection(heap: &VmHeap, out: &mut Vec<ObjectRef>) {
    let prev = GC_ROOT_PASS.with(|c| c.replace(true));
    scan_active_jit_frames(heap, out);
    GC_ROOT_PASS.with(|c| c.set(prev));
}

/// OPT-IN gate for the above-chain conservative band
/// (`CRATONVM_JIT_ABOVE_CHAIN_SCAN=1`), and it is opt-in because it was
/// MEASURED not to be the fix it was written as.
///
/// The hypothesis was that the BindableTests ByteBuddy reclaim's missed root
/// lives in `[max(entry_sp), stack_high)` — the VM's own Rust frames between
/// the outermost interpreter entry and the compiled call — because
/// `CRATONVM_DBG_FULLSTACK_SCAN=1` cures that failure and its only difference
/// from the normal path is scanning the whole stack. It is not, or not only:
/// with this band walked on every collection (4978 passes, 3 056 492 roots,
/// 1.0 GB of stack read) the failure is unchanged, and so is it with the
/// widest possible CHAIN band (`CRATONVM_NO_PRECISE_JIT_MAPS=1`) and with the
/// A5 filter's veto removed (`CRATONVM_JIT_UNREG_ACCEPT_RESIDUE=1`). What is
/// left of the fullstack diagnostic's difference is that it also scans on the
/// per-native-call snapshot publishers — see [`above_chain_all_paths`].
///
/// Kept, default off, as the lever that reading measures rather than a fix:
/// a default-on gigabyte of stack reads per run buys nothing demonstrated.
/// See
/// `docs/feature-designs/real-jdk-mode.md`.
#[cfg(any(target_os = "windows", target_os = "linux"))]
pub(in crate::jit::conservative_roots) fn above_chain_scan_enabled() -> bool {
    static ON: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&ON, || false)
}

/// `CRATONVM_JIT_ABOVE_CHAIN_ALL_PATHS=1` — also walk the band on the
/// per-native-call snapshot publishers, not only on the collection's own root
/// pass.
///
/// This is the difference between the band repair and
/// `CRATONVM_DBG_FULLSTACK_SCAN`, and it is a flag rather than a default
/// because it is the expensive half: `update_root_snapshot` runs on every
/// object-returning native call, and paying a live-stack walk there is what
/// makes the fullstack diagnostic 2-3x slower than the run it is diagnosing.
/// Kept so the two can be told apart by measurement instead of argument.
#[cfg(any(target_os = "windows", target_os = "linux"))]
pub(in crate::jit::conservative_roots) fn above_chain_all_paths() -> bool {
    static ON: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&ON, || true)}

/// `CRATONVM_JIT_ABOVE_CHAIN_FROM_SP=1` — start the band at the SCANNER's SP
/// rather than at the top of the JIT entry chain.
///
/// The chain scan is supposed to cover `[scanner_sp, max(entry_sp))` already,
/// but only for entries with no precise oop map: a PRECISE entry contributes
/// its map's slots and no band at all. This makes the two hypotheses
/// separable without also changing which paths scan.
#[cfg(any(target_os = "windows", target_os = "linux"))]
pub(in crate::jit::conservative_roots) fn above_chain_from_sp() -> bool {
    static ON: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&ON, || true)}

/// `CRATONVM_DBG_ABOVE_CHAIN_KB=<n>` — cap the above-chain band at `n` KiB.
///
/// Diagnostic only, and the reason it exists: the band's upper bound is the
/// thread's stack top, so "the fix works" says nothing about WHERE the missed
/// root was. Bisecting `n` until the failure returns names the depth, which is
/// the first step in replacing this conservative band with a precise root.
#[cfg(any(target_os = "windows", target_os = "linux"))]
pub(in crate::jit::conservative_roots) fn above_chain_scan_cap_bytes() -> Option<usize> {
    static CAP: std::sync::OnceLock<Option<usize>> = std::sync::OnceLock::new();
    *CAP.get_or_init(|| {
        Err::<String, std::env::VarError>(std::env::VarError::NotPresent)
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|&kb| kb > 0)
            .map(|kb| kb * 1024)
    })
}

/// Cached `CRATONVM_DBG_FULLSTACK_SCAN` gate, same per-native-call hot-path
/// rationale as [`dbg_no_prune`].
///
/// Its doc comment and a `#[cfg(any(windows, linux))]` used to sit ~200 lines
/// above, orphaned where the function had been before it moved — so the
/// attribute landed on [`UNREG_MEMO_SUPPRESSED`] instead, cfg-gating a counter
/// that `vm-cli` reads unconditionally. Both are reunited with the function
/// here; the gate itself needs no cfg (the flag is read on every target, only
/// its CONSUMER in `scan_active_jit_frames` is windows/linux).
pub(in crate::jit::conservative_roots) fn dbg_fullstack_scan() -> bool {
    static ON: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&ON, || false)}

/// A5 fix — scan this thread's native stack band `[lo, hi)` for any word that is
/// an address strictly *inside* a live JIT code range
/// (`cratonvm_jit::lookup_jit_code_range`), i.e. a return address into compiled
/// code. A hit proves a JIT method's frame
/// is on the stack even if it pushed no `JitEntryGuard`. Early-exits on the
/// first hit. Bounded like [`scan_one_frame`] so a stale `hi` cannot run into
/// unmapped pages.
///
/// Returns the `(slot address, word)` of the first hit so the caller's
/// diagnostic can name the actual evidence — "some word somewhere looked like
/// compiled code" is not something a reader can act on, and this probe's own
/// doc comment names false-positive reduction as the follow-up work.
#[cfg(any(target_os = "windows", target_os = "linux"))]
pub(in crate::jit::conservative_roots) fn native_stack_has_jit_frame(lo: usize, hi: usize) -> Option<(usize, usize)> {
    let mut from = lo;
    loop {
        let (slot, word) = native_stack_has_jit_frame_raw(from, hi)?;
        if !is_pre_entry_stale_word(slot, word) {
            return Some((slot, word));
        }
        from = slot + 8;
    }
}

/// The JIT return addresses that were already on this thread's stack, above
/// its first JIT entry, when it made that entry: `(slot, word)` pairs.
///
/// # Why (netty-parity, 2026-10-28)
///
/// A new thread can start on a stack range an exited thread used, and the old
/// contents are still there. Netty's `OpenSslEngineTest` starts thousands of
/// event-loop threads. Some find a return address into JIT code
/// (`AtomicIntegerFieldUpdater$AtomicIntegerFieldUpdaterImpl.compareAndSet`,
/// `WeakHashMap.expungeStaleEntries`, `DefaultChannelPipeline.internalAdd`:
/// whatever the previous thread last ran at that depth) about 1 KB below their
/// stack top. That is above every `entry_sp` the new thread will push, so the
/// residue mark never explains the word. The unregistered-frame probe read it as
/// a live guardless frame at every collection for the thread's whole life, and
/// revoked the moving-young licence. At `-Xmx1500m` ZGC refused relocation on
/// every cycle and the class died of fragmentation (live 400 MB, largest free
/// block 36 MB). HotSpot runs it in the same heap.
///
/// Measured: the pre-entry words appear only on threads whose stack top an
/// earlier thread had used (2 of ~25,000 such threads in four minutes, none of
/// the 142 on a never-used top).
///
/// # Why skipping the pair is sound
///
/// Before its first entry the thread has run no compiled code through a
/// guard. The exception, a guardless compiled frame, is the process entry
/// point (see `ChainHistory::residue_hi`), so the main thread records nothing.
/// Any other compiled frame the thread runs later lies below a pushed
/// `entry_sp` and is covered by the chain, or by the residue mark once it
/// returns. It cannot be in this band. A pair matches only while BOTH the slot and the
/// word are unchanged; anything written there afterwards is probed as before. A
/// declined hit marks nothing, which is what a band with no JIT word gets on
/// every thread that did not inherit one. `CRATONVM_JIT_UNREG_PRE_ENTRY_STALE=0`
/// records nothing.
#[cfg(any(target_os = "windows", target_os = "linux"))]
pub(in crate::jit::conservative_roots) struct PreEntryStale {
    pub(in crate::jit::conservative_roots) recorded: bool,
    pub(in crate::jit::conservative_roots) words: Vec<(usize, usize)>,
}

#[cfg(any(target_os = "windows", target_os = "linux"))]
thread_local! {
    pub(in crate::jit::conservative_roots) static PRE_ENTRY_STALE: std::cell::RefCell<PreEntryStale> =
        const { std::cell::RefCell::new(PreEntryStale { recorded: false, words: Vec::new() }) };
}

/// Bounds the per-thread record; a band holding more than this many
/// inherited JIT words keeps the excess as hits (the conservative direction).
#[cfg(any(target_os = "windows", target_os = "linux"))]
pub(in crate::jit::conservative_roots) const PRE_ENTRY_STALE_MAX: usize = 32;

/// `CRATONVM_JIT_UNREG_PRE_ENTRY_STALE`, default ON (`=0` kills).


/// At this thread's FIRST JIT entry, record the JIT return addresses already
/// above `entry_sp` ([`PreEntryStale`]). One probe of the band per thread.
#[cfg(any(target_os = "windows", target_os = "linux"))]
pub(in crate::jit::conservative_roots) fn note_pre_entry_stale_words(entry_sp: usize) {
    let first = PRE_ENTRY_STALE
        .try_with(|c| {
            let mut s = c.borrow_mut();
            !std::mem::replace(&mut s.recorded, true)
        })
        .unwrap_or(false);
    if !first || std::thread::current().name() == Some("main") {
        return;
    }
    let top = own_stack_top();
    let mut words = Vec::new();
    let mut from = entry_sp;
    while from < top && words.len() < PRE_ENTRY_STALE_MAX {
        let Some((slot, word)) = native_stack_has_jit_frame_raw(from, top) else {
            break;
        };
        words.push((slot, word));
        from = slot + 8;
    }
    if !words.is_empty() {
        PRE_ENTRY_STALE_THREADS.fetch_add(1, Ordering::Relaxed);
        let _ = PRE_ENTRY_STALE.try_with(|c| c.borrow_mut().words = words);
    }
}

/// Threads that inherited at least one JIT word above their first entry.
#[cfg(any(target_os = "windows", target_os = "linux"))]
pub static PRE_ENTRY_STALE_THREADS: AtomicUsize = AtomicUsize::new(0);

/// Is `(slot, word)` one this thread inherited, still unchanged?
#[cfg(any(target_os = "windows", target_os = "linux"))]
#[inline]
pub(in crate::jit::conservative_roots) fn is_pre_entry_stale_word(slot: usize, word: usize) -> bool {
    PRE_ENTRY_STALE
        .try_with(|c| {
            let s = c.borrow();
            !s.words.is_empty() && s.words.contains(&(slot, word))
        })
        .unwrap_or(false)
}

/// [`native_stack_has_jit_frame`] without the inherited-word skip.
#[cfg(any(target_os = "windows", target_os = "linux"))]
pub(in crate::jit::conservative_roots) fn native_stack_has_jit_frame_raw(lo: usize, hi: usize) -> Option<(usize, usize)> {
    // PERF (TC0622 startup): snapshot the JIT code ranges ONCE (single table
    // lock) into a reusable thread-local buffer, then binary-search each stack
    // word lock-free. The previous code called `lookup_jit_code_range` per word,
    // locking a global `Mutex` up to ~1M times per scan — and this runs on the
    // per-native-call root-snapshot path, so it dominated Tomcat `start()`
    // (~66% of CPU). The hit/no-hit result is identical: ranges are disjoint,
    // so `start <= w < end` for the greatest `start <= w` is exact.
    // Opt-out safety net for the gauntlet soak: `CRATONVM_JIT_RANGE_SCAN_LEGACY=1`
    // restores the original per-word `lookup_jit_code_range` path (one global
    // Mutex lock per stack word). The default (fast) path is behavior-identical
    // — disjoint ranges make the binary-search membership test exact — so this
    // gate exists purely so a soak can A/B and revert without a rebuild.
    if jit_range_scan_legacy() {
        let mut addr = (lo + 7) & !7usize;
        // gen r4w2/youngmark: see `own_stack_scan_hi` — the cap used to keep
        // the bottom of the band and drop the JIT frames at its top.
        let hi = own_stack_scan_hi(addr, hi, current_stack_pointer(), own_stack_top());
        while addr + 8 <= hi {
            // SAFETY: see the fast path below.
            let w = unsafe { (addr as *const usize).read() };
            if let Some(cm_ptr) = cratonvm_jit::lookup_jit_code_range(w) {
                // A raw compiled-entry function pointer is routinely kept in
                // Rust helper locals.  It is data, not a native return address,
                // and treating it as a frame forces every precise moving-GC
                // cycle into the conservative fallback.  A genuine return PC
                // is always after the method entry instruction.
                let cm = unsafe { &*(cm_ptr as *const cratonvm_jit::CompiledMethod) };
                let entry = cm.entry_ptr() as usize;
                if w != entry && is_plausible_return_pc(w) {
                    return Some((addr, w));
                }
            }
            addr += 8;
        }
        return None;
    }
    // PERF (2026-07-15, round 2 of the RequestMappingMessageConversionIntegrationTests
    // bootstrap-slowness investigation): the thread-local buffer below used to be
    // rebuilt (full table lock + Vec copy + `sort_unstable()` over every
    // registered JIT code range) on EVERY call to this function, even though
    // nothing had changed since the previous call — a write-only "cache" in
    // name only. This function runs on the per-native-call root-snapshot path,
    // so that cost was paid on every native call, and it grew as the process
    // JIT-compiled more code (the registered range set only grows — see
    // `JIT_CODE_RANGES`'s doc comment in `jit/src/lib.rs`). Now the cached
    // generation is compared against `cratonvm_jit::jit_code_ranges_generation()`
    // first; the expensive resnapshot/resort only runs when the set actually
    // changed since this thread last looked (the overwhelmingly common case is
    // a burst of many calls between any two JIT compiles finishing).
    thread_local! {
        static RANGE_SNAPSHOT: std::cell::RefCell<(u64, Vec<(usize, usize)>)> =
            const { std::cell::RefCell::new((u64::MAX, Vec::new())) };
    }
    RANGE_SNAPSHOT.with(|cell| {
        let mut cached = cell.borrow_mut();
        let (cached_gen, ranges) = &mut *cached;
        let current_gen = cratonvm_jit::jit_code_ranges_generation();
        if *cached_gen != current_gen {
            cratonvm_jit::snapshot_code_ranges_into(ranges);
            *cached_gen = current_gen;
        }
        if ranges.is_empty() {
            return None;
        }
        // Address envelope of ALL code ranges: `ranges` is sorted by start, so
        // the smallest start is first; and because the ranges are also
        // DISJOINT, the largest end is the last one's (round 11: this used to be
        // an O(n) `max` over every range on every probe, i.e. per native call).
        // A stack word outside `[env_lo, env_hi)` cannot be in any
        // range, so reject it with a single compare before the binary search.
        // The overwhelming majority of stack words are ints / data pointers far
        // outside the small JIT code arena, so this prefilter skips the
        // partition_point for ~all words. Strict superset of the membership test
        // → result is identical.
        let env_lo = ranges[0].0;
        let env_hi = ranges.last().map_or(0, |&(_, e)| e);
        let mut addr = (lo + 7) & !7usize;
        // gen r4w2/youngmark: see `own_stack_scan_hi`.
        let hi = own_stack_scan_hi(addr, hi, current_stack_pointer(), own_stack_top());
        // Counted per CALL — see `rootprof::note_jit_probe`. Recorded up front
        // so an early `return Some(..)` (a hit, which stops the walk) still
        // reports the band this probe was ASKED for, which is the quantity the
        // memo's incremental-band logic is supposed to be shrinking.
        crate::memory::native_roots::rootprof::note_jit_probe(
            (hi.saturating_sub(addr) / 8) as u64, // Widening: a stack band length
        );
        while addr + 8 <= hi {
            // SAFETY: aligned read inside the calling thread's own live stack
            // band between two known stack pointers (same contract as
            // `scan_one_frame`).
            let w = unsafe { (addr as *const usize).read() };
            if w >= env_lo && w < env_hi {
                // Greatest range whose start <= w; its start itself is a
                // stored function pointer, while a native return PC is
                // strictly inside the range.
                let idx = ranges.partition_point(|&(s, _)| s <= w);
                if idx > 0
                    && w > ranges[idx - 1].0
                    && w < ranges[idx - 1].1
                    && is_plausible_return_pc(w)
                {
                    return Some((addr, w));
                }
            }
            addr += 8;
        }
        None    })}

/// Does a candidate A5 slot look like the return-address slot of a REAL frame?
///
/// `is_plausible_return_pc` asks whether the *word* could be a return address.
/// This asks the different question the A5 probe actually needs: whether the
/// *slot holding it* is where a live frame's return address sits. It is the
/// frame-shape half of the "raw-word scan, not a frame walk" follow-up that
/// [`native_stack_has_jit_frame`]'s own comment names.
///
/// A frame entered by `call` and opened with the JIT's prologue
/// (`push rbp; mov rbp, rsp`, which every compiled body emits) lays the stack
/// out exactly like this, addresses growing upward:
///
/// ```text
///   slot - 8  : saved caller RBP   <- callee's own RBP points here
///   slot      : return address     <- the candidate word
///   slot + 8  : caller's frame ...
/// ```
///
/// So the word one slot BELOW a genuine return address is the caller's frame
/// base: 8-aligned, strictly above this frame, inside the same stack, and
/// itself the base of a frame whose own return-address slot holds a plausible
/// return PC. A stale return address left behind in the uninitialised middle of
/// a live frame has no such neighbour except by coincidence.
///
/// Deliberately shallow — two links, no walk to the stack base. The chain above
/// a genuine JIT frame runs into VM Rust frames, and this tree does not build
/// with forced frame pointers, so those frames need not maintain RBP at all and
/// a deeper walk would reject real frames. Two links is what can be asserted
/// from the JIT's own calling convention alone.
///
/// Conservative in the safe direction on purpose: this is only ever used to
/// decide whether over-detection is happening, never to suppress a hit that
/// passes.
pub(in crate::jit::conservative_roots) fn a5_slot_has_frame_shape(slot: usize, stack_hi: usize) -> bool {
    if !a5_frame_base_is_plausible(slot, stack_hi) {
        return false;
    }
    // SAFETY: `slot - 8` and `caller_rbp + 8` are 8-aligned addresses inside the
    // calling thread's own stack — `slot` came from the scan, which only offers
    // addresses it has already read, and `a5_frame_base_is_plausible` has
    // bounded `caller_rbp + 8` below `stack_hi`.
    let caller_rbp = unsafe { ((slot - 8) as *const usize).read() };
    if !a5_frame_base_is_plausible_link(slot, caller_rbp, stack_hi) {
        return false;
    }
    let caller_ret = unsafe { ((caller_rbp + 8) as *const usize).read() };
    // The caller of a JIT frame is either another JIT frame or the VM's own
    // code. Only the first is checkable from here; a caller outside every JIT
    // range is accepted, because the interpreter->JIT boundary is exactly that
    // and is the most common real case.
    match cratonvm_jit::lookup_jit_code_range(caller_ret) {
        Some(_) => is_plausible_return_pc(caller_ret),
        None => caller_ret != 0,
    }
}

/// Can `slot` be a return-address slot at all? Split out so the arithmetic is
/// testable without a real stack.
pub(in crate::jit::conservative_roots) fn a5_frame_base_is_plausible(slot: usize, stack_hi: usize) -> bool {
    slot >= 8 && slot & 0x7 == 0 && slot < stack_hi
}

/// Is `caller_rbp`, read from `slot - 8`, shaped like the caller's frame base?
///
/// A saved caller RBP is 8-aligned, strictly OLDER than this frame (a higher
/// address, since the stack grows down), and leaves room for its own
/// return-address slot below `stack_hi`. Pure arithmetic, so the invariant this
/// rests on is stated once and tested directly.
pub(in crate::jit::conservative_roots) fn a5_frame_base_is_plausible_link(slot: usize, caller_rbp: usize, stack_hi: usize) -> bool {
    caller_rbp & 0x7 == 0
        && caller_rbp > slot
        && caller_rbp.checked_add(8).is_some_and(|end| end < stack_hi)
}

/// Census of the A5 band: how many words look like JIT return addresses, and
/// how many of those sit at a slot with real frame shape.
///
/// Diagnostic only — nothing branches on it. It exists to PRICE the frame-shape
/// filter before anything is built on it, the way the fallback-reason mask
/// priced the indirect-call repair: if a cycle's hits are all shapeless, the
/// filter would have converted that cycle; if any hit has frame shape, it would
/// not, and the cycle is blocked by something the filter cannot reach.
pub(in crate::jit::conservative_roots) fn native_stack_jit_frame_census(lo: usize, hi: usize) -> (usize, usize) {
    let mut total = 0usize;
    let mut shaped = 0usize;
    let mut addr = (lo + 7) & !7usize;
    // gen r4w2/youngmark: see `own_stack_scan_hi`.
    let hi_capped = own_stack_scan_hi(addr, hi, current_stack_pointer(), own_stack_top());
    while addr + 8 <= hi_capped {
        // SAFETY: same contract as `native_stack_has_jit_frame` — an aligned
        // read inside this thread's own live stack band.
        let w = unsafe { (addr as *const usize).read() };
        if let Some(cm_ptr) = cratonvm_jit::lookup_jit_code_range(w) {
            let cm = unsafe { &*(cm_ptr as *const cratonvm_jit::CompiledMethod) };
            if w != cm.entry_ptr() as usize && is_plausible_return_pc(w) {
                total += 1;
                if a5_slot_has_frame_shape(addr, hi) {
                    shaped += 1;
                }
            }
        }
        addr += 8;
    }
    (total, shaped)
}

/// Return-address validation for the A5 raw-word scan.
///
/// The scan is a word scan, not a frame walk: any stack slot whose value
/// happens to land inside a JIT code range reads as a return PC. That
/// over-detection is safe (it only forces the non-moving young sweep) but it is
/// not free — a persistent false positive means the young generation never
/// compacts, and `native_stack_has_jit_frame`'s own doc comment names cutting
/// the false-positive rate as the designated follow-up.
///
/// This is that filter, and it uses the one fact that separates a genuine
/// return address from a stored code pointer: **a return address is always the
/// address of the instruction after a `call`**. So the bytes immediately
/// preceding it must be the tail of a call encoding. On x86-64 the forms that
/// can appear in generated code and in the trampolines that enter it are:
///
/// * `E8 rel32`          — 5 bytes, direct near call
/// * `FF /2` (`callq *r/m`) — 2 to 7 bytes, indirect call (register, memory,
///   with or without REX / SIB / displacement)
/// * `9A`, `FF /3`       — far call; not emitted by this JIT, ignored
///
/// A stored function pointer (a cached `entry_ptr`, an OSR entry, a stub
/// address handed to a helper as an argument) is preceded by whatever
/// instruction bytes happen to sit before the callee's own entry — matching one
/// of these encodings is possible but no longer automatic.
///
/// Conservative in the safe direction: when the preceding bytes cannot be read
/// or do not decode, the word is REJECTED as a return PC. That is the direction
/// that costs moving cycles, never correctness — the opposite of a filter that
/// waves a real unregistered frame through.
///
/// Reading the bytes needs a keep-alive, not just an address. The caller's
/// range snapshot is a lock-free copy that can name a body whose last `Arc` has
/// since been dropped — every other user of that snapshot only *compares*
/// addresses, so a stale entry was harmless until this function wanted to
/// dereference one. [`cratonvm_jit::pin_jit_code_range_owner`] upgrades the
/// range's `Weak` and keeps the executable buffer alive for the read; `None`
/// (the body really is gone) is a rejection, and a correct one — no live frame
/// can be returning into reclaimed code.
#[cfg(any(target_os = "windows", target_os = "linux"))]
pub(in crate::jit::conservative_roots) fn is_plausible_return_pc(w: usize) -> bool {
    if !return_pc_validation_enabled() {
        return true;
    }
    let Some(owner) = cratonvm_jit::pin_jit_code_range_owner(w) else {
        return false;
    };
    // Lower bound for the look-back: the body's own entry point, which is the
    // start of its registered range. `w` is strictly inside `[entry, end)`, so
    // the window never reaches the page before the code buffer.
    let entry = owner.entry_ptr() as usize;
    // Bytes available behind `w`, capped at the longest encoding tail we
    // recognise (REX + FF + ModRM + SIB + disp32, of which 7 precede the
    // return address at most).
    let avail = w.saturating_sub(entry).min(CALL_TAIL_MAX);
    let mut window = [0u8; CALL_TAIL_MAX];
    for i in 0..avail {
        // SAFETY: `w - avail + i` is in `[entry, w)`, inside the executable
        // buffer `owner` is holding alive for the duration of this call.
        window[i] = unsafe { ((w - avail + i) as *const u8).read() };
    }
    call_encoding_precedes(&window[..avail])
}

/// Longest instruction tail [`call_encoding_precedes`] inspects.
#[cfg(any(target_os = "windows", target_os = "linux"))]
pub(in crate::jit::conservative_roots) const CALL_TAIL_MAX: usize = 7;

/// Does `window` — the bytes immediately preceding a candidate return address,
/// in address order, at most [`CALL_TAIL_MAX`] of them — end with a near-call
/// encoding?
///
/// Split out from [`is_plausible_return_pc`] so the decode is testable without
/// a live JIT code range: this half is pure, the other half is the pin-and-read.
#[cfg(any(target_os = "windows", target_os = "linux"))]
pub(in crate::jit::conservative_roots) fn call_encoding_precedes(window: &[u8]) -> bool {
    let n = window.len();
    // A near call is at least two bytes (`FF /2` with a register operand), so
    // nothing shorter can decode.
    if n < 2 {
        return false;
    }
    // Right-align into a fixed array so an encoding of length `len` always has
    // its opcode at index `CALL_TAIL_MAX - len`.
    let mut prev = [0u8; CALL_TAIL_MAX];
    prev[CALL_TAIL_MAX - n..].copy_from_slice(window);
    // `E8 rel32`: the direct near call, 5 bytes, so its opcode sits 5 back.
    if n >= 5 && prev[CALL_TAIL_MAX - 5] == 0xE8 {
        return true;
    }
    // `FF /2` indirect near call: 2..=7 bytes. Walk every length the encoding
    // can take and accept if the byte at that distance is `FF` and the ModRM
    // that follows selects reg field 2 (`/2`, bits 5..3 == 0b010).
    for len in 2..=n {
        let op = CALL_TAIL_MAX - len;
        if prev[op] == 0xFF && (prev[op + 1] >> 3) & 0b111 == 0b010 {
            return true;
        }
    }
    // Same, one byte earlier, to allow a single REX prefix (0x40..=0x4F) in
    // front of the `FF`.
    for len in 3..=n {
        let rex = CALL_TAIL_MAX - len;
        if prev[rex] & 0xF0 == 0x40
            && prev[rex + 1] == 0xFF
            && (prev[rex + 2] >> 3) & 0b111 == 0b010
        {
            return true;
        }
    }
    false
}

/// Kill switch for [`is_plausible_return_pc`]: `CRATONVM_JIT_NO_RETPC_VALIDATE=1`
/// restores the pre-2026-08-04 behaviour where every in-range stack word counts
/// as a JIT frame. Exists so a soak can A/B the filter without a rebuild, and so
/// a future bug report has a one-flag bisect for it. Same cached-read rationale
/// as [`jit_range_scan_legacy`] — this sits on the per-native-call root-snapshot
/// path.
#[cfg(any(target_os = "windows", target_os = "linux"))]
pub(in crate::jit::conservative_roots) fn return_pc_validation_enabled() -> bool {
    static ON: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&ON, || !false)
}

/// How many times the A5 probe diverted a moving-young cycle, split by what
/// the ONE hit slot it returned actually looked like.
///
/// The probe is a raw-word scan, and its own doc comment has named cutting the
/// false-positive rate as the designated follow-up since it was written. Two
/// screens exist for that and neither could be judged here, because the
/// population was never counted: `native_stack_jit_frame_census` re-walks the
/// whole band (so it is opt-in and off in every real run) and the residue
/// filter was applied at one of the probe's two call sites only.
///
/// `A5_FALLBACK_RESIDUE` counts hits at a slot BELOW this thread's
/// returned-JIT-frame high-water mark — the leftovers of a compiled frame that
/// has already returned. `A5_FALLBACK_SHAPELESS` counts hits at a slot that is
/// not the return-address slot of a frame at all, and is only computed when
/// something asks for it (it reaches the global code-range table, which the
/// probe's fast path exists to avoid). Either makes the hit a false positive,
/// and a false positive costs the young generation its copying collector for
/// that cycle.
pub static A5_FALLBACK_HITS: AtomicUsize = AtomicUsize::new(0);
/// Hits whose slot sits below [`jit_residue_hi`] — see [`A5_FALLBACK_HITS`].
pub static A5_FALLBACK_RESIDUE: AtomicUsize = AtomicUsize::new(0);
/// Hits whose slot has no frame shape — see [`A5_FALLBACK_HITS`]. Only counted
/// when the shape filter or the fallback dump is on.
pub static A5_FALLBACK_SHAPELESS: AtomicUsize = AtomicUsize::new(0);
/// Hits a screen suppressed — see [`A5_FALLBACK_HITS`].
pub static A5_FALLBACK_FILTERED: AtomicUsize = AtomicUsize::new(0);

/// Snapshot of the A5 fallback census: `(hits, residue, shapeless, filtered)`.
pub fn a5_fallback_census() -> (usize, usize, usize, usize) {
    (
        A5_FALLBACK_HITS.load(Ordering::Relaxed),
        A5_FALLBACK_RESIDUE.load(Ordering::Relaxed),
        A5_FALLBACK_SHAPELESS.load(Ordering::Relaxed),
        A5_FALLBACK_FILTERED.load(Ordering::Relaxed),
    )
}

/// `CRATONVM_DBG_A5_FALLBACK=1` — print the first sixteen A5 probe hits and
/// then every power of two, naming the slot, whether it is residue, and whether
/// it has frame shape.
///
/// Distinct from [`a5_census_enabled`], which re-walks the whole band to count
/// EVERY candidate word in it. This one reports what the probe already found.
/// The probe runs on the per-native-call root-snapshot path — measured at 1.1
/// million calls in one 240 s run — so it is rate-limited rather than
/// per-event, and the totals are in [`a5_fallback_census`].
#[cfg(any(target_os = "windows", target_os = "linux"))]
pub(in crate::jit::conservative_roots) fn a5_fallback_dbg() -> bool {
    static ON: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&ON, || false)}

/// Kill switch for the residue filter on the MOVING-YOUNG COVERAGE probe —
/// `CRATONVM_JIT_A5_RESIDUE_FILTER=0` accepts every hit again.
///
/// `scan_active_jit_frames` has applied exactly this filter, with exactly this
/// argument, since the H2 `FileNioMapped.unMap` investigation: a compiled
/// method that has already returned leaves a return address into JIT code at
/// every depth below its own `entry_sp`, and that is indistinguishable by
/// inspection from a live guardless frame. The one live guardless frame the
/// probe exists for is the process entry point, which sits ABOVE every JIT
/// entry the run has ever made and therefore above the mark.
///
/// The coverage probe in `refresh_moving_young_coverage_for_current_thread` is
/// the same scan asking the same question and did NOT have it — the cheaper
/// half of the two, because there a false positive only declines to MOVE (the
/// sound direction), whereas at the marking site it declines to MARK. The
/// filter was therefore already trusted in the more dangerous of the two
/// places.
#[cfg(any(target_os = "windows", target_os = "linux"))]
pub(in crate::jit::conservative_roots) fn a5_residue_filter_enabled() -> bool {
    static ON: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&ON, || {
        true
    })
}

/// Opt-in gate for the frame-shape filter on the moving-young coverage probe —
/// `CRATONVM_JIT_A5_SHAPE_FILTER=1`.
///
/// Default OFF, and deliberately so on two counts.
/// [`a5_slot_has_frame_shape`] was written as a PRICING instrument and its own
/// doc says it is "only ever used to decide whether over-detection is
/// happening, never to suppress a hit that passes"; turning it into a
/// suppressor is a different claim and needs its own measurement. And it costs
/// a `lookup_jit_code_range`, i.e. the global table lock that the probe's fast
/// path was rewritten to avoid.
#[cfg(any(target_os = "windows", target_os = "linux"))]
pub(in crate::jit::conservative_roots) fn a5_shape_filter_enabled() -> bool {
    static ON: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&ON, || {
        matches!(
            Err::<String, std::env::VarError>(std::env::VarError::NotPresent).as_deref(),
            Ok("1") | Ok("true") | Ok("on")
        )
    })
}

/// Diagnostic gate for [`native_stack_jit_frame_census`]. Off by default: the
/// census re-scans the whole band a second time, which is affordable only when
/// you are deliberately measuring.
#[cfg(any(target_os = "windows", target_os = "linux"))]
pub(in crate::jit::conservative_roots) fn a5_census_enabled() -> bool {
    static ON: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&ON, || false)}

/// How many times the residue filter DECLINED a hit, and how many heap objects
/// those declines chose not to conservatively mark.
///
/// The filter exists to stop a returned frame's leftovers from pinning the
/// whole native stack (the H2 `FileNioMapped.unMap` timeout). Declining is
/// therefore the safe direction for RETENTION and the unsafe direction for
/// LIVENESS: the band it skips is the only thing this site would have marked.
/// Nothing counted either half, so "the filter is why that object was swept"
/// could only ever be an inference.
pub static UNREG_DECLINED: AtomicUsize = AtomicUsize::new(0);
/// Objects the declines above chose not to mark — see [`UNREG_DECLINED`].
pub static UNREG_DECLINED_ROOTS: AtomicUsize = AtomicUsize::new(0);
/// gen r4/mark (2026-09-23): residue-filter declines that a second probe of
/// `[residue_hi, scan_hi)` turned into an accept — a live compiled frame sat
/// ABOVE the residue word that made the first probe decline. Each one is a
/// band whose oops the scan used to drop. Diagnostic counter only.
pub static UNREG_RESIDUE_RESCUES: AtomicUsize = AtomicUsize::new(0);

/// Snapshot of the declined-band census: `(declines, objects not marked)`.
pub fn unreg_declined_census() -> (usize, usize) {
    (
        UNREG_DECLINED.load(Ordering::Relaxed),
        UNREG_DECLINED_ROOTS.load(Ordering::Relaxed),
    )
}

/// `CRATONVM_DBG_UNREG_DECLINED=1` — name the compiled body each declined hit
/// belongs to, and list the heap objects the decline is not marking.
///
/// The body's `method_label` is the question this census exists to answer: a
/// declined hit in a method whose frame is genuinely live means an entry door
/// that pushed no `JitEntryGuard`, and the repair is to register it. A declined
/// hit in a method that has demonstrably returned means the band is being kept
/// alive for some other reason, and the repair is elsewhere.
///
/// Costs a second walk of the band (the `scan_one_frame` the decline skipped,
/// into a throw-away vector), so it is opt-in.
#[cfg(any(target_os = "windows", target_os = "linux"))]
pub(in crate::jit::conservative_roots) fn unreg_declined_dbg() -> bool {
    static ON: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&ON, || false)}



/// Cached `CRATONVM_JIT_RANGE_SCAN_LEGACY` gate — see `native_stack_has_jit_frame`.
#[cfg(any(target_os = "windows", target_os = "linux"))]
pub(in crate::jit::conservative_roots) fn jit_range_scan_legacy() -> bool {
    static ON: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&ON, || false)
}

/// Read the safepoint id a live compiled frame published into
/// `[rbp - cm.sp_id_slot_off]`, for a GC purpose. `None` when the method
/// reserves no such slot, and for a `frame_holds_no_references` frame: its
/// slot may be stale by contract, and a stale id that happened to equal
/// another safepoint's would hand that safepoint's mask and live bound to this
/// frame. `None` is every caller's no-claim answer.
pub(in crate::jit::conservative_roots) fn active_safepoint_id(rbp: usize, cm: &cratonvm_jit::CompiledMethod) -> Option<u32> {
    if cm.frame_holds_no_references {
        return None;
    }
    safepoint_id_slot(rbp, cm)
}

/// The raw word in `[rbp - cm.sp_id_slot_off]`, whatever the frame contract.
/// Only [`activation_bci`] reads it past [`active_safepoint_id`]'s refusal,
/// for the innermost activation, whose slot the contract keeps current.
pub(in crate::jit::conservative_roots) fn safepoint_id_slot(rbp: usize, cm: &cratonvm_jit::CompiledMethod) -> Option<u32> {
    let sp_id_off = cm.sp_id_slot_off;
    if sp_id_off == 0 {
        return None;
    }
    let id_addr = rbp.checked_sub(sp_id_off as usize)?;
    if id_addr & 0x7 != 0 {
        return None;
    }
    // SAFETY: aligned safepoint-id slot in a live JIT frame on this thread.
    Some((unsafe { (id_addr as *const usize).read() }) as u32)
}

/// The exclusive frame-offset bound of the LIVE part of this frame at its
/// active safepoint (`OopMapEntry::live_frame_hi`). `None` when the bound is
/// unknown — no sp-id slot, no matching map, or a map recorded without a
/// paired pre-safepoint spill — in which case the caller must scan the whole
/// region. When several maps share the safepoint id, the largest (most
/// conservative) bound wins.
pub(in crate::jit::conservative_roots) fn moving_young_frame_live_hi(rbp: usize, cm: &cratonvm_jit::CompiledMethod) -> Option<i32> {
    let sp_id = active_safepoint_id(rbp, cm)?;
    let mut hi = 0i32;
    for map in cm.oop_maps.iter().filter(|m| m.bytecode_pc == sp_id) {
        if map.live_frame_hi <= 0 {
            return None;
        }
        hi = hi.max(map.live_frame_hi);
    }
    (hi > 0).then_some(hi)
}

/// Consume-side register-oop-mask census: `(frames_with_mask, frames_no_mask,
/// words_excluded)`. The emit side has its own
/// (`cratonvm_jit::reg_oop_mask_cause`); this one says whether the map that was
/// published actually reached a frame walk.
pub(in crate::jit::conservative_roots) static REGOOP_FRAMES_MASKED: AtomicUsize = AtomicUsize::new(0);
pub(in crate::jit::conservative_roots) static REGOOP_FRAMES_UNMASKED: AtomicUsize = AtomicUsize::new(0);
pub(in crate::jit::conservative_roots) static REGOOP_WORDS_EXCLUDED: AtomicUsize = AtomicUsize::new(0);
pub(in crate::jit::conservative_roots) static DEADSPILL_WORDS_EXCLUDED: AtomicUsize = AtomicUsize::new(0);
pub(in crate::jit::conservative_roots) static OUTGOING_WORDS_EXCLUDED: AtomicUsize = AtomicUsize::new(0);

/// `(frames_with_mask, frames_without, blind-spill words excluded,
/// dead-operand-spill words excluded, outgoing-reserve words excluded)`.
pub fn reg_oop_mask_census() -> (usize, usize, usize, usize, usize) {
    (
        REGOOP_FRAMES_MASKED.load(Ordering::Relaxed),
        REGOOP_FRAMES_UNMASKED.load(Ordering::Relaxed),
        REGOOP_WORDS_EXCLUDED.load(Ordering::Relaxed),
        DEADSPILL_WORDS_EXCLUDED.load(Ordering::Relaxed),
        OUTGOING_WORDS_EXCLUDED.load(Ordering::Relaxed),
    )
}

/// What the ACTIVE safepoint's oop maps say about one frame, read in ONE pass
/// over `cm.oop_maps` (round 11 wave 10: the band scan and the pin publication
/// each used to re-filter the maps for `live_hi`, and the mask took a third
/// pass).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::jit::conservative_roots) struct ActiveMapFacts {
    /// The register-oop mask ([`cratonvm_jit::OopMapEntry::reg_oop_mask`]), or
    /// `None` for "no claim".
    ///
    /// Same resolution as [`moving_young_frame_live_hi`], and the same
    /// most-conservative merge when several maps share the safepoint id: the
    /// UNION of their masks, and `None` the moment any one of them abstains. A
    /// mask is a licence to STOP scanning slots, so the merge has to widen, not
    /// narrow -- an intersection would let one map's silence delete another's
    /// live register.
    pub(in crate::jit::conservative_roots) reg_mask: Option<u16>,
    /// Exactly [`moving_young_frame_live_hi`].
    pub(in crate::jit::conservative_roots) live_hi: Option<i32>,
    /// The active safepoint kept its mask over a reference staged where no
    /// map names it (`CompiledMethod::staged_args_unmapped_sp_ids`, gen
    /// r5w2/oomjit6), so the outgoing reserve must be scanned whatever the
    /// mask says.
    pub(in crate::jit::conservative_roots) staged_unmapped: bool,
    /// gcg g3/v: the blind-spill image slots the active safepoint's spills
    /// wrote ([`cratonvm_jit::OopMapEntry::spill_written`], opt-in
    /// `CRATONVM_JIT_SPILL_WRITTEN_SET` at compile time), merged like
    /// `reg_mask`: the UNION over the id's maps, `None` the moment one
    /// abstains. A slot outside it is skipped by [`spill_slot_may_hold_oop`]
    /// as a root and as a pin; unlike a `Some` `reg_mask` it licenses nothing
    /// else (the outgoing-reserve claim never reads it).
    pub(in crate::jit::conservative_roots) spill_written: Option<u16>,
}

/// [`ActiveMapFacts`] for the frame at `rbp`.
pub(in crate::jit::conservative_roots) fn active_map_facts(rbp: usize, cm: &cratonvm_jit::CompiledMethod) -> ActiveMapFacts {
    let Some(sp_id) = active_safepoint_id(rbp, cm) else {
        return ActiveMapFacts::NONE;
    };
    let mut acc = ActiveMapAcc::new();
    for map in cm.oop_maps.iter().filter(|m| m.bytecode_pc == sp_id) {
        acc.add(map);
    }
    acc.finish(cm, sp_id)
}

impl ActiveMapFacts {
    /// "No claim": no readable safepoint id.
    pub(in crate::jit::conservative_roots) const NONE: ActiveMapFacts = ActiveMapFacts {
        reg_mask: None,
        live_hi: None,
        staged_unmapped: false,
        spill_written: None,
    };
}

/// The fold [`active_map_facts`] runs over the maps of one safepoint id, as a
/// value, so [`frame_claim_facts`] can run it in the same pass as the other
/// claims (gcf f2/f).
pub(in crate::jit::conservative_roots) struct ActiveMapAcc {
    pub(in crate::jit::conservative_roots) mask: u16,
    pub(in crate::jit::conservative_roots) mask_any: bool,
    pub(in crate::jit::conservative_roots) mask_abstains: bool,
    pub(in crate::jit::conservative_roots) hi: i32,
    pub(in crate::jit::conservative_roots) hi_unknown: bool,
    /// gcg g3/v: [`ActiveMapFacts::spill_written`]'s fold.
    pub(in crate::jit::conservative_roots) written: u16,
    pub(in crate::jit::conservative_roots) written_any: bool,
    pub(in crate::jit::conservative_roots) written_abstains: bool,
}

impl ActiveMapAcc {
    pub(in crate::jit::conservative_roots) fn new() -> Self {
        ActiveMapAcc {
            mask: 0,
            mask_any: false,
            mask_abstains: !gc_reg_oop_maps_enabled(),
            hi: 0,
            hi_unknown: false,
            written: 0,
            written_any: false,
            // gcg g4/jm (g3/v review item 5): the consume-side lever withdraws
            // the written set with the register masks -- `=0` scans the whole
            // blind-spill image again, as its doc says.
            written_abstains: !gc_reg_oop_maps_enabled(),
        }
    }

    pub(in crate::jit::conservative_roots) fn add(&mut self, map: &cratonvm_jit::OopMapEntry) {
        match map.reg_oop_mask {
            Some(m) => {
                self.mask |= m;
                self.mask_any = true;
            }
            None => self.mask_abstains = true,
        }
        match map.spill_written {
            Some(w) => {
                self.written |= w;
                self.written_any = true;
            }
            None => self.written_abstains = true,
        }
        if map.live_frame_hi <= 0 {
            self.hi_unknown = true;
        } else {
            self.hi = self.hi.max(map.live_frame_hi);
        }
    }

    pub(in crate::jit::conservative_roots) fn finish(self, cm: &cratonvm_jit::CompiledMethod, sp_id: u32) -> ActiveMapFacts {
        ActiveMapFacts {
            reg_mask: (!self.mask_abstains && self.mask_any).then_some(self.mask),
            live_hi: (!self.hi_unknown && self.hi > 0).then_some(self.hi),
            staged_unmapped: cm.staged_args_unmapped_sp_ids.binary_search(&sp_id).is_ok(),
            spill_written: (!self.written_abstains && self.written_any).then_some(self.written),
        }
    }
}

// gcf f2/f (2026-10-01) -- `gce-e1f-proposal-one-pass-active-safepoint-facts-per-frame`.
//
// `scan_one_frame_filtered` used to ask its frame five questions, each of
// which read the safepoint-id slot again and filtered the whole `oop_maps`
// vector again (`active_map_facts`, `ir_prim_slots`, `sp_local_claim`,
// `frame_liveness_sp_id`), and read four switches through
// `runtime_flag_default_on` per frame. [`frame_claim_facts`] answers all of
// them from ONE read of the id and ONE filter pass, and the switches are read
// once per chain walk ([`BandClaimSwitches::read`]). The answers are the
// same by construction -- the unit test
// `frame_claim_facts_equal_the_separate_questions` holds the old functions
// (kept: the census and the tests still ask them one at a time) against the
// fused one -- so there is no switch.
//
// Not done: the proposal's sorted `oop_map_index` on `CompiledMethod`, which
// would make the one pass a binary search. It moves `CompiledMethod`'s layout
// (`jit/src/lib.rs`, every constructor), and the measured win of the fused
// pass should be read first.

/// The four per-frame band-claim switches, read once per chain walk instead
/// of once per frame. No static (`vm/tests/per_vm_state_statics_ratchet.rs`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::jit::conservative_roots) struct BandClaimSwitches {
    /// [`callee_saved_image_liveness_enabled`].
    pub(in crate::jit::conservative_roots) callee_saved_image: bool,
    /// [`ir_prim_slot_claim_enabled`].
    pub(in crate::jit::conservative_roots) ir_prims: bool,
    /// [`sp_local_map_claim_enabled`].
    pub(in crate::jit::conservative_roots) sp_locals: bool,
    /// [`deopt_image_residue_claim_enabled`].
    pub(in crate::jit::conservative_roots) deopt_residue: bool,
    /// [`ir_deopt_image_residue_claim_enabled`] (gcf f5/f, opt-in; the f6/f
    /// default-on flip was reverted).
    pub(in crate::jit::conservative_roots) ir_deopt_residue: bool,
    /// [`ir_deopt_image_never_roots_enabled`] (gcg g1/f; default ON since gcg
    /// g4/jm's vg2/f verdict).
    pub(in crate::jit::conservative_roots) ir_deopt_never: bool,
    /// [`sp_stub_own_map_enabled`] (gcg g4/jm; default ON since gcg g5/jm):
    /// the post-CALL stub's own map, derived from the call's. It replaced the
    /// f5/f stub-call claim and g1/s hunk 1, deleted in g5.
    pub(in crate::jit::conservative_roots) sp_stub_own_map: bool,
    /// [`sp_stack_prim_claim_enabled`] (gch h4/jm, opt-in): a single-pass
    /// frame's primitive operand slots ([`cratonvm_jit::SpStackPrimFold`]),
    /// spent through the `ir_prims` arm of [`scan_one_frame_filtered`].
    pub(in crate::jit::conservative_roots) sp_stack_prims: bool,
    /// [`reference_free_frame_roots_enabled`] (gci i23/rf, opt-in): a
    /// `frame_holds_no_references` frame's words below `callee_saved_lo` are
    /// not band roots ([`reference_free_band_floor`]).
    pub(in crate::jit::conservative_roots) ref_free_frames: bool,
    /// [`dbg_frame_root_unnamed`] (gci i38/ds diagnostic; hoisted out of the
    /// per-frame scan by gci i41/rs, residuals page item 3).
    pub(in crate::jit::conservative_roots) unnamed_dbg: bool,
    /// [`band_map_dead_not_roots_enabled`] (gci i38/ds, opt-in; hoisted by
    /// gci i41/rs).
    pub(in crate::jit::conservative_roots) map_dead_claim: bool,
    /// [`band_dead_locals_owe_no_pin_enabled`] (t1 W13, opt-in).
    pub(in crate::jit::conservative_roots) dead_locals_no_pin: bool,
}

impl BandClaimSwitches {
    pub(in crate::jit::conservative_roots) fn read() -> Self {
        BandClaimSwitches {
            unnamed_dbg: dbg_frame_root_unnamed(),
            map_dead_claim: band_map_dead_not_roots_enabled(),
            dead_locals_no_pin: band_dead_locals_owe_no_pin_enabled(),
            callee_saved_image: callee_saved_image_liveness_enabled(),
            ir_prims: ir_prim_slot_claim_enabled(),
            sp_locals: sp_local_map_claim_enabled(),
            deopt_residue: deopt_image_residue_claim_enabled(),
            ir_deopt_residue: ir_deopt_image_residue_claim_enabled(),
            ir_deopt_never: ir_deopt_image_never_roots_enabled(),
            sp_stub_own_map: sp_stub_own_map_enabled(),
            sp_stack_prims: sp_stack_prim_claim_enabled(),
            ref_free_frames: reference_free_frame_roots_enabled(),
        }
    }
}

/// Every per-frame input of [`scan_one_frame_filtered`]'s claims.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::jit::conservative_roots) struct FrameClaimFacts<'a> {
    /// Exactly [`active_map_facts`].
    pub(in crate::jit::conservative_roots) map: ActiveMapFacts,
    /// Exactly [`ir_prim_slots`].
    pub(in crate::jit::conservative_roots) ir_prims: Option<&'a [i16]>,
    /// Exactly [`sp_local_claim`].
    pub(in crate::jit::conservative_roots) sp_locals: Option<SpLocalClaim>,
    /// Exactly [`frame_liveness_sp_id`].
    pub(in crate::jit::conservative_roots) live_sp: Option<u32>,
    /// Exactly [`frame_deopt_image_is_residue`].
    pub(in crate::jit::conservative_roots) deopt_residue: bool,
    /// [`callee_saved_image_liveness_enabled`]; the frame-shape half of the
    /// rule (`walk_hi > rbp`) is applied by the scan.
    pub(in crate::jit::conservative_roots) image_rule: bool,
    /// [`BandClaimSwitches::unnamed_dbg`]: the `[frame-root-unnamed]`
    /// diagnostic, read once per chain walk (gci i41/rs), not per frame.
    pub(in crate::jit::conservative_roots) unnamed_dbg: bool,
    /// [`BandClaimSwitches::map_dead_claim`]: the opt-in map-dead band claim,
    /// read once per chain walk (gci i41/rs), not per frame.
    pub(in crate::jit::conservative_roots) map_dead_claim: bool,
    /// [`band_dead_locals_owe_no_pin_enabled`] (t1 W13, opt-in).
    pub(in crate::jit::conservative_roots) dead_locals_no_pin: bool,
}

/// gcf f4/f (`gcf-f2f-proposal-precise-half-reads-the-fused-frame-facts`):
/// the frame's active safepoint id ([`active_safepoint_id`]) and the indices,
/// in `oop_maps` order, of every map that carries it -- ONE read of the id
/// and ONE filter pass, shared by the precise half of the frame walk
/// ([`scan_active_maps`]) and the band half ([`frame_claim_facts_with`]),
/// which used to make one each.
pub(in crate::jit::conservative_roots) struct ActiveMaps {
    pub(in crate::jit::conservative_roots) sp_id: Option<u32>,
    pub(in crate::jit::conservative_roots) idx: smallvec::SmallVec<[u32; 4]>,
    /// gcf f5/f: the slot held a TRACE-ONLY id (a post-CALL stub is running
    /// a Java-capable helper) and `sp_id` / `idx` are that call's own, read
    /// for the stub-call claim ([`active_maps_for`]). Never set without the
    /// opt-in.
    pub(in crate::jit::conservative_roots) trace_only: bool,
}

impl ActiveMaps {
    /// The maps of the active id, in `oop_maps` order.
    pub(in crate::jit::conservative_roots) fn maps<'s, 'c>(
        &'s self,
        cm: &'c cratonvm_jit::CompiledMethod,
    ) -> impl Iterator<Item = &'c cratonvm_jit::OopMapEntry> + use<'s, 'c> {
        self.idx
            .iter()
            .filter_map(move |&i| cm.oop_maps.get(i as usize))
    }
}

/// [`ActiveMaps`] for the frame at `rbp`.
pub(in crate::jit::conservative_roots) fn active_maps(rbp: usize, cm: &cratonvm_jit::CompiledMethod) -> ActiveMaps {
    let sp_id = active_safepoint_id(rbp, cm);
    let mut idx = smallvec::SmallVec::new();
    if let Some(id) = sp_id {
        for (i, m) in cm.oop_maps.iter().enumerate() {
            if m.bytecode_pc == id {
                // Cast: a map index; a body has far fewer than 2^32 maps.
                idx.push(i as u32);
            }
        }
    }
    ActiveMaps {
        sp_id,
        idx,
        trace_only: false,
    }
}

// gcf f5/f (2026-10-01) -- the tenth band claim: a SINGLE-PASS frame parked in
// a post-CALL stub's helper (`CRATONVM_GC_SP_STUB_CALL_MAP_ROOTS=1`, opt-in).
//
// The holder vf2 named on every failing `Gcd1PinnedCalleeOomeProbe` run
// (10 of 10, `gcd-d6u-...`): `main`'s single-pass OSR body, `off=128
// region=operand-spill tier=sp sp=2147483712 live_hi=none in_map=false`. The
// id `0x8000_0040` is the TRACE-ONLY form of call id 64
// (`x64/deopt_stubs.rs`, `stub_sp_id_word`): `main`'s call to `chainCatch`
// returned the deopt sentinel, and `emit_inline_callee_deopt_check`'s stub
// stored the tagged id and called `jit_service_callee_deopt`, which resumes
// `chainCatch` in the interpreter and runs the whole post-catch check (the
// three `System.gc()`, `usable()`) inside that helper. A tagged id matches no
// map by construction, so for the whole service every claim abstained and
// the frame's stale operand-spill word -- a chain node -- was rooted, both by
// the collection's own band scan (`cat=14`) and through every root snapshot
// the band scan deposited (`cat=11`, which `CRATONVM_JIT_BAND_SKIP` cannot
// reach because it removes the word as a root only, not as a publication).
//
// The claim: read the CALL's own maps (the payload id) for the two claims
// that speak about dead WORDS of the frame -- the operand-spill live cursor
// (`live_frame_hi`, the dead-spill claim) and the java-local oracle
// (`SpLocalClaim`) -- and nothing else (no register mask, no staged-args
// fact, no precise-half roots). Why the call's maps describe the stub's frame
// for those two: the stub runs after the CALL returned and before the code
// after it, so the frame holds what the call's map holds across the call
// (the operand stack below the arguments, the locals) plus the stub's own
// inputs. The stub writes no operand-spill or java-local word; what its
// helper reads of the frame is the call's service argument range, which the
// call's map names (`pending_staged_arg_oops`), and on a declined resume the
// caller's own frame state at the call, whose references the map names as
// well. A word above the call's live cursor, or an unnamed java-local home,
// is therefore read by nobody before the frame's next safepoint store.
// Opt-in because the deopt_stubs.rs comment that introduced the tagged id
// says the collector must NOT read the call's map at the stub, and this
// claim narrows that to the two dead-word facts above.

// gcg g5/jm: the claim above was `CRATONVM_GC_SP_STUB_CALL_MAP_ROOTS` (opt-in).
// It and g1/s hunk 1 (`CRATONVM_GC_SP_STUB_IMAGE_CALLER_SAVED_DEAD`) are
// DELETED: `CRATONVM_GC_SP_STUB_OWN_MAP` (below, default ON since g5) reads the
// same two dead-word facts plus the call's callee-saved register mask, and
// vg4's `jm4_pc_om_*` passed 8/8 against a base that failed 8/8. The argument
// above is the own map's argument for its cursor and locals oracle.

/// The callee-saved GPRs' bits in a single-pass `reg_oop_mask` (the
/// `ALL_SPILL_GPRS` order, through the JIT's own bit function).
pub(in crate::jit::conservative_roots) fn callee_saved_spill_image_mask() -> u16 {
    #[cfg(windows)]
    const REGS: [u8; 7] = [3, 6, 7, 12, 13, 14, 15]; // RBX RSI RDI R12-R15
    #[cfg(not(windows))]
    const REGS: [u8; 5] = [3, 12, 13, 14, 15]; // RBX R12-R15
    REGS.iter()
        .fold(0u16, |m, &r| m | cratonvm_jit::x64::reg_oop_mask_bit(r))
}

// gcg g4/jm (2026-10-02) -- `gcg-g3v-proposal-the-post-call-stub-publishes-its-own-map`,
// built opt-in (`CRATONVM_GC_SP_STUB_OWN_MAP=1`).
//
// The proposal: the post-CALL stub of a single-pass frame gets its own oop map,
// built from the CALL's map, so that every band claim speaks at the stub with
// no special case, and the three partial workarounds (`SP_STUB_CALL_MAP_ROOTS`
// above, `SP_STUB_IMAGE_CALLER_SAVED_DEAD` above, and the emitter's
// `CRATONVM_JIT_STUB_ZERO_DEAD_SPILL`) can go.
//
// Built at SCAN time rather than pushed at emission time. The stub's map is a
// pure function of the call's maps (the stub writes no frame word the
// collector reads: it stores the id slot and passes its helper only
// non-reference words and the call's service range), so deriving it here gives
// the same facts without a third id space. A new id space would have had to be
// taught to every reader that decodes `SP_ID_TRACE_ONLY_TAG` -- the stack
// trace's `activation_bci`, `apply_direct_throw_trap_bci`, the debugger -- and
// to `jit/src/x64/deopt_stubs.rs`'s two stubs. Here the slot keeps the
// trace-only word, the trace readers are untouched, and
// `moving_young_frame_coverage_complete_at` still finds no map at the tagged id
// (`NO_MAP_FOR_STORED_ID`), so a MOVING young collection still fails closed for
// the frame exactly as before. That is the hazard the stub comment names: after
// the call the code may have reloaded registers from the image, and a moving
// collection would rewrite the image word but not the register. The derived
// map therefore feeds the band claims (what is DEAD) and never the precise
// half or the moving-coverage answer (what may be REWRITTEN); the call's named
// slots stay band words, rooted conservatively, as during the call.
//
// The derived facts:
// - `live_hi`, `spill_written`, the java-local oracle: the call's (as the f5
//   arm). The stub writes no operand-spill, image or local word.
// - `reg_mask`: the CALL's mask restricted to the callee-saved GPRs. The callee
//   clobbered every caller-saved register, and compiled code never reloads the
//   image, so a caller-saved image word is dead (hunk 1's fact); a callee-saved
//   register still holds its value at the call, so its image word is an oop
//   exactly when the call's mask says so. When the call's mask abstains, the
//   callee-saved set itself (hunk 1's mask). Under `CRATONVM_GC_REG_OOP_MAPS=0`
//   no mask at all (g3/v review item 5: the switch withdraws this claim too).
// - `staged_unmapped`: forced true whenever a mask is published. The stub's
//   helper reads the call's service range, which only the precise half names,
//   and the precise half is not run here; so the outgoing reserve stays
//   scanned (`is_dead_outgoing_reserve` never claims it).

/// `CRATONVM_GC_SP_STUB_OWN_MAP` -- DEFAULT ON since gcg g5/jm (`=0` restores
/// the pre-round answer: no claim at a trace-only id). vg4 (g4b):
/// `Gcd1PinnedCalleeOomeProbe` 8/8 = HotSpot against a base failing 8/8;
/// JitOomRoot / argsp / MtChurn / TakeoverDerivedCursor unchanged or better on
/// all three collectors. See the section above. Read once per chain walk
/// ([`BandClaimSwitches`]), no static.
pub(in crate::jit::conservative_roots) fn sp_stub_own_map_enabled() -> bool {
    true
}

/// The register mask of the post-CALL stub's derived map (see the section
/// above): the call's mask (`call_mask`, already merged over the id's maps)
/// restricted to the callee-saved GPRs, the callee-saved set when the call
/// abstains, and no mask when register maps are off (`reg_maps`).
pub(in crate::jit::conservative_roots) fn sp_stub_own_map_reg_mask(call_mask: Option<u16>, reg_maps: bool) -> Option<u16> {
    if !reg_maps {
        return None;
    }
    let callee_saved = callee_saved_spill_image_mask();
    Some(call_mask.map_or(callee_saved, |m| m & callee_saved))
}

/// [`active_maps`], plus the stub-call resolution of a trace-only id in a
/// single-pass body when `sw` turns it on (see the section above).
pub(in crate::jit::conservative_roots) fn active_maps_for(
    rbp: usize,
    cm: &cratonvm_jit::CompiledMethod,
    sw: BandClaimSwitches,
) -> ActiveMaps {
    let plain = active_maps(rbp, cm);
    if !sw.sp_stub_own_map || cm.used_ir_backend {
        return plain;
    }
    let Some(call_id) = plain.sp_id.and_then(trace_only_sp_id_payload) else {
        return plain;
    };
    let mut idx = smallvec::SmallVec::new();
    for (i, m) in cm.oop_maps.iter().enumerate() {
        if m.bytecode_pc == call_id {
            // Cast: a map index; a body has far fewer than 2^32 maps.
            idx.push(i as u32);
        }
    }
    ActiveMaps {
        sp_id: Some(call_id),
        idx,
        trace_only: true,
    }
}

/// The precise half for a frame whose [`ActiveMaps`] are already resolved:
/// exactly [`scan_active_oop_map_at_rbp`] (same refusals -- both go through
/// [`active_safepoint_id`]'s -- and the same maps in the same order), without
/// a second id read and filter pass.
pub(in crate::jit::conservative_roots) fn scan_active_maps(
    rbp: usize,
    cm: &cratonvm_jit::CompiledMethod,
    active: &ActiveMaps,
    heap: &VmHeap,
    out: &mut Vec<ObjectRef>,
) {
    // gcf f5/f: a stub-call resolution feeds the band claims only; the
    // precise half reads no map at a trace-only id, as before.
    if active.trace_only {
        return;
    }
    for map in active.maps(cm) {
        scan_oop_slots(rbp, &map.frame_slot_offsets, &cm.method_label, heap, out);
    }
}

/// [`FrameClaimFacts`] for the frame at `rbp`: one read of the safepoint id,
/// one pass over the maps that carry it.
#[cfg_attr(not(test), allow(dead_code))]
pub(in crate::jit::conservative_roots) fn frame_claim_facts(
    rbp: usize,
    cm: &cratonvm_jit::CompiledMethod,
    sw: BandClaimSwitches,
) -> FrameClaimFacts<'_> {
    frame_claim_facts_with(rbp, cm, sw, &active_maps_for(rbp, cm, sw))
}

/// [`frame_claim_facts`] over maps already resolved ([`ActiveMaps`]).
pub(in crate::jit::conservative_roots) fn frame_claim_facts_with<'a>(
    rbp: usize,
    cm: &'a cratonvm_jit::CompiledMethod,
    sw: BandClaimSwitches,
    active: &ActiveMaps,
) -> FrameClaimFacts<'a> {
    // gcg g1/f: the never-roots claim answers without reading the frame.
    let deopt_residue = ir_deopt_image_is_dead_with(cm, sw.deopt_residue, sw.ir_deopt_never)
        || frame_deopt_image_is_residue_with(rbp, cm, sw.deopt_residue, sw.ir_deopt_residue);
    let Some(sp_id) = active.sp_id else {
        return FrameClaimFacts {
            map: ActiveMapFacts::NONE,
            ir_prims: None,
            sp_locals: None,
            live_sp: None,
            deopt_residue,
            image_rule: sw.callee_saved_image,
            unnamed_dbg: sw.unnamed_dbg,
            map_dead_claim: sw.map_dead_claim,
            dead_locals_no_pin: sw.dead_locals_no_pin,
        };
    };
    let want_prims = cm.used_ir_backend && sw.ir_prims;
    let want_locals = !cm.used_ir_backend && sw.sp_locals;
    // gch h4/jm (opt-in): a single-pass frame's exact primitive operand
    // list, spent through the same `ir_prims` arm (never both: one tier).
    let want_sp_prims = !cm.used_ir_backend && sw.sp_stack_prims;
    let mut sp_prims = cratonvm_jit::SpStackPrimFold::new();
    let mut acc = ActiveMapAcc::new();
    // `ir_prim_slots` reads the FIRST map of the id.
    let mut prims: Option<&[i16]> = None;
    let mut prims_seen = false;
    let mut locals = SpLocalAcc::new();
    for map in active.maps(cm) {
        acc.add(map);
        if want_prims && !prims_seen {
            prims_seen = true;
            prims = Some(map.non_oop_stack_slots.as_slice()).filter(|s| !s.is_empty());
        }
        if want_locals {
            locals.add(map);
        }
        if want_sp_prims {
            sp_prims.add(map);
        }
    }
    let map = acc.finish(cm, sp_id);
    if active.trace_only {
        // gcg g4/jm: the stub's own map, derived from the call's (see
        // `sp_stub_own_map_enabled`). `map` is the call's merged facts here.
        let reg_mask = sp_stub_own_map_reg_mask(map.reg_mask, gc_reg_oop_maps_enabled());
        return FrameClaimFacts {
            map: ActiveMapFacts {
                reg_mask,
                live_hi: map.live_hi,
                staged_unmapped: reg_mask.is_some(),
                spill_written: map.spill_written,
            },
            ir_prims: None,
            sp_locals: if want_locals { locals.finish() } else { None },
            live_sp: None,
            deopt_residue,
            image_rule: sw.callee_saved_image,
            unnamed_dbg: sw.unnamed_dbg,
            map_dead_claim: sw.map_dead_claim,
            dead_locals_no_pin: sw.dead_locals_no_pin,
        };
    }
    FrameClaimFacts {
        map,
        ir_prims: if want_sp_prims {
            sp_prims.finish(&cm.frame_layout)
        } else {
            prims
        },
        sp_locals: if want_locals { locals.finish() } else { None },
        live_sp: (!cm.frame_liveness.is_empty()).then_some(sp_id),
        deopt_residue,
        image_rule: sw.callee_saved_image,
        unnamed_dbg: sw.unnamed_dbg,
        map_dead_claim: sw.map_dead_claim,
        dead_locals_no_pin: sw.dead_locals_no_pin,
    }
}

// gcf f5/f (2026-10-01): the gcf f3/f IR ref-map claim (token `ir-ref-map-roots`)
// (an optimizing frame's spill-band word no complete map names) is REMOVED.
// vf3 measured it fixing nothing it was built for (`NativeGrowthReclaimProbe`
// 0/3: the holder is not a spill-band word, see below) and failing its
// soundness rows on G1 and ZGC. By reading, its soundness argument had a hole:
// "a moving collection already rewrites only the named slots" is not a proof
// that an unnamed word is dead, because until now every unnamed band word was
// PUBLISHED as unrewritable and so pinned its object -- the band pin, not the
// map, is what kept a map gap safe under G1 and ZGC, and the claim removed
// both the root and that pin. See
// `docs/feature-designs/real-jdk-mode.md`.

/// `CRATONVM_GC_REG_OOP_MAPS=0` -- ignore every published
/// [`cratonvm_jit::OopMapEntry::reg_oop_mask`] and scan the whole blind-spill
/// image again. **Default ON.**
///
/// The consume-side half of the bisect pair; `CRATONVM_JIT_REG_OOP_MAPS=0` is
/// the emit-side one. This one takes effect at the next collection, which is
/// what an investigation on a warm VM wants.
pub(in crate::jit::conservative_roots) fn gc_reg_oop_maps_enabled() -> bool {
    static G: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&G,
        || match Err::<String, std::env::VarError>(std::env::VarError::NotPresent) {
            Ok(v) => !matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "0" | "false" | "off" | "no"
            ),
            Err(_) => true,
        },
    )
}

/// Is `off` in the OUTGOING-argument reserve of a frame whose active safepoint
/// proved it staged no reference there?
///
/// The reserve is the deepest part of the frame: the ABI shadow space and the
/// room `SUB RSP, frame_size` set aside for stack arguments. The prologue never
/// initialises it, so before this frame writes anything it holds whatever a
/// previous, deeper frame left below the old stack pointer -- dead values from
/// a call that already returned, which the conservative scan then marks and
/// (under G1) pins a region for. On H2 `TestValueMemory` Type 3 that class is
/// 314 of the 380 unrewritable band words.
///
/// # What licenses skipping it
///
/// A reference the frame ITSELF stages there is a different matter, and the
/// compiler already reports it: `pending_staged_args_unmapped` is set when a
/// reference is staged into "the native-ABI outgoing-argument area
/// (`emit_stack_arg_setup`), the direct-call service slots, or an inlined
/// callee's parameter locals", and its own doc calls that fail-closed. That bit
/// is exactly what makes [`ActiveMapFacts::reg_mask`]
/// abstain, so **a `Some` mask already carries the proof** and this predicate
/// needs no second channel: no reference of this frame's is in the reserve, and
/// what predates the frame is not this frame's to keep alive.
///
/// The deopt `SavedRegisters` block is NOT covered by that argument -- it is a
/// register image the deopt stub reads back -- and it shares `region_name`'s
/// bucket with the reserve. `FrameLayout::outgoing_lo` is published so the two
/// can be told apart; when it is `0` the producer made no claim and this
/// returns `false`.
#[inline]
pub(in crate::jit::conservative_roots) fn is_dead_outgoing_reserve(
    layout: &cratonvm_jit::FrameLayout,
    off: i32,
    reg_mask: Option<u16>,
    staged_unmapped: bool,
) -> bool {
    if !gc_outgoing_arg_roots_enabled() || reg_mask.is_none() || staged_unmapped {
        return false;
    }
    layout.outgoing_lo > 0 && off >= layout.outgoing_lo
}


pub(crate) fn chain_all_at_depth(depth: u32) -> bool {
    JIT_ENTRY_CHAIN
        .try_with(|c| {
            c.try_borrow().ok().and_then(|v| {
                Some(v.first()?.interp_depth == depth && v.last()?.interp_depth == depth)
            })
        })
        .ok()
        .flatten()
        .unwrap_or(false)
}
