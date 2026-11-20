#[allow(unused_imports)]
use super::*;

/// Detect — and loudly report — the unsupported *multi-thread-in-JIT* condition
/// for the thread-local JIT-frame scanner.
///
/// ## What it detects
///
/// `scan_active_jit_frames` only ever discovers the **calling** thread's JIT
/// roots (the chain is thread-local). When the calling thread's chain is empty
/// (`current_thread_jit_depth() == 0`) yet `any_thread_in_jit()` is true, this
/// scan contributes **nothing** for JIT roots while *some other* thread holds
/// live JIT spill slots. If this scan is part of a peer-triggered STW
/// collector's authoritative root walk, those peer roots are reachable ONLY via
/// that peer's last-published `root_snapshot` (see the `scan_active_jit_frames`
/// doc): if that snapshot is stale, a live root is silently dropped.
///
/// This is exactly the documented gap (a full cross-thread STW JIT root scan is
/// a separate, unimplemented feature). Rather than proceed silently, we surface
/// the condition:
///
/// - **Always:** a rate-limited `tracing::warn!` (first occurrence, then every
///   power-of-two thereafter) so the gap is visible in any run without flooding
///   the per-native-call hot path.
/// - **Opt-in (`CRATONVM_STRICT_JIT_ROOTS`):** a panic, turning the gap into a
///   hard failure for CI / fuzzing / bisection.
///
/// ## Why this is conservative (does NOT destabilize the single-thread path)
///
/// The detector NEVER changes the root set — it only observes and logs. The
/// common single-thread-in-JIT path (`current_thread_jit_depth() > 0`) never
/// trips it. A peer being in JIT while THIS thread is not is also benign in the
/// normal flow (the peer published a fresh snapshot at its safepoint); the warn
/// flags the *structural* window in which the unimplemented cross-thread scan
/// would be required, which is the actionable signal for the follow-up.
#[cold]
#[inline(never)]
pub(in crate::jit::conservative_roots) fn warn_cross_thread_jit_gap() {
    // BUG-03 — when the cross-thread STW JIT root scan is enabled, the
    // collector forcibly stops every in-JIT peer and conservatively scans its
    // registers + stack at GC time (see `crate::jit::xt_root_scan`). The
    // condition this detector flags (a peer in JIT while this thread's chain
    // is empty) is therefore no longer a gap — peer JIT roots are covered
    // regardless of snapshot freshness. Treat it as a non-event: do not count
    // a hit and never panic under CRATONVM_STRICT_JIT_ROOTS (so the strict
    // gate verifies the fix reaches zero gap hits).
    if crate::jit::xt_root_scan::enabled() {
        return;
    }
    let hits = CROSS_THREAD_JIT_GAP_HITS.fetch_add(1, Ordering::Relaxed) + 1;
    // Rate-limit: log the 1st hit and every subsequent power of two so a
    // long-running multi-threaded JIT workload does not spam the log, but the
    // condition is never fully silent.
    if (hits & (hits - 1)) == 0 {
        tracing::warn!(
            cross_thread_jit_gap_hits = hits,
            global_jit_depth = GLOBAL_JIT_DEPTH.get(),
            "scan_active_jit_frames: another thread holds live JIT frames while \
             this thread's JIT chain is empty — the thread-local scanner cannot \
             see the peer's JIT roots. They are covered ONLY by that peer's \
             last-published root_snapshot; a stale snapshot would drop a live \
             root. This is the documented multi-thread-in-JIT-under-STW gap; the \
             cross-thread STW JIT root scan is a tracked follow-up. Set \
             CRATONVM_STRICT_JIT_ROOTS=1 to make this fatal."
        );
    }
    if strict_jit_roots() {
        panic!(
            "CRATONVM_STRICT_JIT_ROOTS: unsupported multi-thread-in-JIT condition \
             in scan_active_jit_frames (this thread's JIT chain is empty but \
             GLOBAL_JIT_DEPTH={} > 0). The thread-local conservative scanner \
             cannot enumerate a peer thread's JIT roots; a cross-thread STW JIT \
             root scan is required and is not yet implemented.",
            GLOBAL_JIT_DEPTH.get(),
        );
    }
}

// ---------------------------------------------------------------------------
// Scanner
// ---------------------------------------------------------------------------

// (gc-common w1-c, 2026-09-23: `scan_active_jit_frames`' doc block and an
// `#[inline(always)]` used to sit above this flag's doc, so the scanner's
// rustdoc was attached to `dbg_no_jit_root_scan` and the scanner had none. The
// doc now sits on the scanner below; the attribute stays on the tiny gate it
// had actually been applying to.)

/// `CRATONVM_DBG_NO_JIT_ROOT_SCAN=1` — skip the conservative JIT frame scan
/// UNCONDITIONALLY. **Diagnostic only, and unsound**: a JIT-held object whose
/// only reference is a register or spill slot stops being a root at all, so a
/// moving cycle will relocate it under the running frame. Never ship a run with
/// this set.
///
/// It exists because the two levers that LOOK like they answer "is the
/// conservative scan what retains this object?" do not:
///
/// * `CRATONVM_GC_PRECISE_ONLY_ROOTS` suppresses the scan only when the
///   coverage proof passes, and its own doc records that firing on **~0.1 % of
///   collections** (2 of 14 420, 31 of 46 135, 84 of 70 144). A run with it set
///   still scans conservatively on 999 collections in 1000, so a null result
///   from it is a vacuous zero, not evidence.
/// * `CRATONVM_NO_CONSERVATIVE_LOCALS` gates the INTERPRETER's local scan,
///   which is a different path.
///
/// The gate lives HERE and not at a call site because there are THREE doors
/// into this function — `memory::roots::collect_roots` (gc-roots),
/// `vm_exec`'s safepoint deposit, and `interpreter::gc_and_alloc`'s
/// blocked-deposit. Gating only the first leaves the other two publishing
/// conservative roots, which is a lever that reads as "no effect" while never
/// having been applied.
#[inline(always)]
pub(in crate::jit::conservative_roots) fn dbg_no_jit_root_scan() -> bool {
    static ON: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&ON, || false)}

/// `CRATONVM_DBG_JIT_ROOTSCAN`, read once. Its sites run per compiled frame and
/// per unrewritable band word on every collection, where an uncached flag read
/// was two hash lookups each (round 11).
pub(in crate::jit::conservative_roots) fn dbg_jit_rootscan() -> bool {
    static ON: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&ON, || false)}

/// `CRATONVM_DBG_INNERMOST_MIRROR_CHECK=1` (JIT round 19 proposal wave 8,
/// lane gc8, GC719-3; debug, opt-in, default OFF): the moving-young coverage
/// proof ([`refresh_moving_young_coverage_for_current_thread`]'s chain loop) asks
/// [`innermost_mirror_liveness`] of each chain entry's innermost-frame mirror
/// and, when the frame it names cannot be shown to have called out of its
/// body, refuses the proof (the non-moving sweep) and prints a
/// `[gc719-3] innermost mirror ...` line. Read once per proof (a cold path).
pub(in crate::jit::conservative_roots) fn innermost_mirror_check_enabled() -> bool {
    false
}

/// GC719-3's verdict on an innermost-frame mirror ([`innermost_mirror_liveness`]).
/// Since GC819-2 the proof asks [`innermost_mirror_call_out`], whose `Ok` is
/// `CalledOut`; the verdict form stays for the unit tests and the docs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
pub(in crate::jit::conservative_roots) enum MirrorLiveness {
    /// A plausible return address INTO the named body lies below the RBP: the
    /// frame called out of compiled code, as every live innermost frame a
    /// collection can observe has (a helper call, a poll's slow path, a
    /// native call, the call into the interpreter that pushed the next chain
    /// entry).
    CalledOut,
    /// No such word in the scanned band: the mirror may name a frame torn
    /// down before a `JMP` into a helper, whose RBP now lies inside the
    /// helper's live Rust frames (the case the `innermost-unrecorded` census
    /// of OSRFRAME18-1 cannot see when the stale safepoint-id slot holds a
    /// recorded id).
    NoCallOut,
    /// The RBP is not an aligned word above the scanner's stack pointer.
    OutOfBand,
}

/// How many words below the mirror's RBP [`innermost_mirror_liveness`] scans
/// for the call-out (512 KiB of stack). The return address of the innermost
/// frame's call out is normally a few words below its frame; the cap only
/// bounds a pathological band.
pub(in crate::jit::conservative_roots) const MIRROR_CALL_OUT_SCAN_WORDS: usize = 1 << 16;

/// GC719-3 (JIT round 19 proposal wave 8, lane gc8): a POSITIVE check that
/// the innermost-frame mirror's RBP `rbp` names a LIVE frame of `cm`: some
/// word between the reader's stack pointer `scanner_sp` and `rbp` is a
/// plausible return address into `cm`'s own code (registered to `cm`, the
/// bytes before it a `call`; [`is_plausible_return_pc`]). Debug only
/// ([`innermost_mirror_check_enabled`]).
///
/// What it proves, and what not: a live innermost compiled frame reaches the
/// VM only through a call, whose return address into its body sits below its
/// RBP for as long as the call is in flight, so a live frame always passes
/// (a frame entered from a shared stub that called out from outside the body
/// would read `NoCallOut`, a false refusal that costs one moving cycle under
/// the debug switch, never a wrong rewrite). A dead frame whose old RBP lies
/// inside live Rust frames passes only if a stale return address into the same
/// body survived below it; so this catches the dead-frame case except for that
/// residue, which is why it is a check and not a proof.
#[cfg_attr(not(test), allow(dead_code))]
pub(in crate::jit::conservative_roots) fn innermost_mirror_liveness(
    rbp: usize,
    scanner_sp: usize,
    cm: *const cratonvm_jit::CompiledMethod,
) -> MirrorLiveness {
    match innermost_mirror_call_out(rbp, scanner_sp, cm) {
        Ok(_) => MirrorLiveness::CalledOut,
        Err(verdict) => verdict,
    }
}

/// [`innermost_mirror_liveness`], answering the return address it found
/// (which GC819-2 anchors, [`mirror_return_anchor`]) or the refusal.
pub(in crate::jit::conservative_roots) fn innermost_mirror_call_out(
    rbp: usize,
    scanner_sp: usize,
    cm: *const cratonvm_jit::CompiledMethod,
) -> Result<usize, MirrorLiveness> {
    #[cfg(any(target_os = "windows", target_os = "linux"))]
    let plausible = is_plausible_return_pc;
    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    let plausible = |_: usize| true;
    innermost_mirror_call_out_with(
        rbp,
        scanner_sp,
        cm as usize,
        // SAFETY: `innermost_mirror_liveness_with` reads only aligned words in
        // `[scanner_sp, rbp)`, the calling thread's own live stack between its
        // stack pointer and a frame base the coverage proof already bounded
        // inside the chain entry's band.
        |addr| unsafe { (addr as *const usize).read() },
        cratonvm_jit::lookup_jit_code_range,
        plausible,
    )
}

/// [`innermost_mirror_liveness_with`], answering the first (highest) return
/// address into the body it found, or the refusal (GC819-2, round 19 wave 9,
/// lane gc9).
pub(in crate::jit::conservative_roots) fn innermost_mirror_call_out_with(
    rbp: usize,
    floor: usize,
    cm: usize,
    read: impl Fn(usize) -> usize,
    owner: impl Fn(usize) -> Option<usize>,
    plausible: impl Fn(usize) -> bool,
) -> Result<usize, MirrorLiveness> {
    if rbp & 0x7 != 0 || rbp <= floor || cm == 0 {
        return Err(MirrorLiveness::OutOfBand);
    }
    let mut addr = rbp;
    for _ in 0..MIRROR_CALL_OUT_SCAN_WORDS {
        let Some(next) = addr.checked_sub(8).filter(|&a| a >= floor) else {
            break;
        };
        addr = next;
        let word = read(addr);
        if owner(word) == Some(cm) && plausible(word) {
            return Ok(word);
        }
    }
    Err(MirrorLiveness::NoCallOut)
}

/// `CRATONVM_DBG_INNERMOST_MIRROR_ANCHOR=1` (JIT round 19 proposal wave 9,
/// lane gc9, GC819-2; debug, opt-in, default OFF, read only under
/// `CRATONVM_DBG_INNERMOST_MIRROR_CHECK=1`): GC719-3's check also requires
/// the return address it found to be ANCHORED to the frame's active
/// safepoint ([`mirror_return_anchor`]) when the frame is a single-pass one
/// whose maps carry anchors; a `Foreign` or `NoActiveMap` answer then refuses
/// the proof (fail closed) with a `[gc819-2] ...` line. Without it the check
/// stays GC719-3's loose one and a disagreement only prints a
/// `[gc819-2] census ...` line: the measurement that decides whether the
/// tight form is worth having (how often a LIVE frame's found word is not
/// its active anchor -- each such line under the tight form would be a false
/// refusal). Read once per proof (a cold path).
pub(in crate::jit::conservative_roots) fn innermost_mirror_anchor_enabled() -> bool {
    false
}

/// GC819-2's answer for the return address an innermost-frame mirror's check
/// found ([`mirror_return_anchor`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::jit::conservative_roots) enum MirrorAnchor {
    /// No anchor to test: an optimizing-tier body (its maps are unanchored,
    /// `native_pc_offset == 0`), a single-pass body none of whose maps carry
    /// one, a reference-free frame (its id slot may be stale by contract), or
    /// a trace-only id (a post-call stub's own helper call, whose return
    /// address lies in the stub, not before the call's map).
    Unanchored,
    /// The return address IS an active map's anchor: the frame is parked in
    /// the very call its safepoint id names.
    Exact,
    /// The return address lies in the active map's window: the first anchored
    /// map at or after it is one of the active id's. Covers the single-pass
    /// shadow reload, which runs between a call's return and its map's anchor
    /// (`x64/safepoint.rs` takes `native_pc` after `emit_shadow_reload`).
    Window,
    /// The first anchored map at or after the return address belongs to
    /// ANOTHER id (or none follows it): the frame's id slot names a different
    /// call than the one the found word returns into -- a live frame inside a
    /// helper call that records no map, or a dead frame's stale word.
    Foreign,
    /// The frame's id slot names no anchored map (unset, or an id no map
    /// carries).
    NoActiveMap,
}



pub(in crate::jit::conservative_roots) fn mirror_return_anchor(
    cm: &cratonvm_jit::CompiledMethod,
    sp_id: Option<u32>,
    ret_off: u32,
) -> MirrorAnchor {
    if (cm.used_ir_backend && false)
        || cm.frame_holds_no_references
        || cm.oop_maps.iter().all(|m| m.native_pc_offset == 0)
    {
        return MirrorAnchor::Unanchored;
    }
    let Some(id) = sp_id else {
        return MirrorAnchor::NoActiveMap;
    };
    if trace_only_sp_id_payload(id).is_some() {
        return MirrorAnchor::Unanchored;
    }
    let mut active = cm
        .oop_maps
        .iter()
        .filter(|m| m.bytecode_pc == id && m.native_pc_offset != 0)
        .peekable();
    if active.peek().is_none() {
        if cm.oop_maps.iter().any(|m| m.bytecode_pc == id) {
            return MirrorAnchor::Unanchored;
        }
        return MirrorAnchor::NoActiveMap;
    }
    if active.any(|m| m.native_pc_offset == ret_off) {
        return MirrorAnchor::Exact;
    }
    let next = cm
        .oop_maps
        .iter()
        .filter(|m| m.native_pc_offset != 0 && m.native_pc_offset >= ret_off)
        .min_by_key(|m| m.native_pc_offset);
    match next {
        Some(m) if m.bytecode_pc == id => MirrorAnchor::Window,
        _ => MirrorAnchor::Foreign,
    }
}

/// GC719-3 and GC819-2 over one chain entry's innermost mirror: `None` when
/// the proof may go on, else the line naming why it is refused. A census line
/// (the loose check passed, the anchor disagrees, the tight form is off) is
/// printed here and answers `None`.
///
/// `cm` is the method the coverage proof resolved for the frame at `rbp`,
/// kept alive by that live frame (the proof's own contract).
pub(in crate::jit::conservative_roots) fn innermost_mirror_refusal(
    rbp: usize,
    scanner_sp: usize,
    cm: &cratonvm_jit::CompiledMethod,
    anchor_tight: bool,
) -> Option<String> {
    let label = cm.method_label.as_str();
    let Ok(ret) = innermost_mirror_call_out(rbp, scanner_sp, cm) else {
        return Some(format!(
            "[gc719-3] innermost mirror rbp=0x{rbp:x} names no frame that called out of {label}"
        ));
    };
    let ret_off = ret
        .checked_sub(cm.entry_ptr() as usize)
        .and_then(|o| u32::try_from(o).ok())?;
    let sp_id = safepoint_id_slot(rbp, cm);
    let anchor = mirror_return_anchor(cm, sp_id, ret_off);
    if !matches!(anchor, MirrorAnchor::Foreign | MirrorAnchor::NoActiveMap) {
        return None;
    }
    let what = format!(
        "innermost mirror rbp=0x{rbp:x} of {label}: return address +0x{ret_off:x} is {anchor:?} \
         for the active id {}",
        sp_id.map_or_else(|| "none".to_string(), |id| format!("0x{id:x}")),
    );
    if anchor_tight {
        Some(format!("[gc819-2] {what}"))
    } else {
        eprintln!("[gc819-2] census {what} (loose check passed; tight form off)");
        None
    }
}

/// `CRATONVM_MOVING_YOUNG_COVERAGE_DBG`, read once: its reader runs on every
/// native call from compiled code (round 11).
pub(in crate::jit::conservative_roots) fn moving_young_coverage_dbg() -> bool {
    static ON: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&ON, || false)}

/// How many times [`dbg_no_jit_root_scan`] actually suppressed a scan. Its one
/// reader is the first-engagement stderr line in [`scan_active_jit_frames`],
/// which is what keeps the arm from being read as "no effect" when it was in
/// fact "never engaged". (Its doc used to say "reported at exit"; nothing
/// reports it at exit, and it was `pub` for no caller.)
pub(in crate::jit::conservative_roots) static NO_JIT_ROOT_SCAN_SUPPRESSED: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Walk every active JIT spill region on the current thread and report each
/// qword whose value is a valid object address as a conservative root.
///
/// # ⚠ THREAD-LOCALITY — known limitation and safety envelope
///
/// This scanner is **strictly thread-local**: the `JIT_ENTRY_CHAIN` and the
/// captured stack pointers belong to the calling thread, so a single call
/// only ever discovers the *calling* thread's live JIT spill roots. It does
/// **not** and **cannot** discover the JIT roots of any *other* thread.
///
/// That distinction matters because two callers invoke it with different
/// expectations:
///
/// - **Single-thread-in-JIT (SAFE).** A self-triggered collection runs
///   `collect_roots` *on the same thread* whose JIT frames hold the roots
///   (`roots.rs`), and a thread snapshotting itself runs
///   `update_root_snapshot` *on its own thread* (`interpreter.rs`). In both
///   cases the calling thread == the thread owning the JIT chain, so the
///   thread-local scan is complete and correct. This is the common path.
///
/// - **Multi-thread-in-JIT under a peer STW (THE GAP).** A *cross-thread*
///   stop-the-world collector marks every parked thread from that thread's
///   `root_snapshot` (`collect_all_root_snapshots`); it does **not** call
///   `collect_roots` — nor this scanner — *for* a non-current thread (doing
///   so would scan the **collector's** empty JIT chain, not the parked
///   worker's). The only mechanism that carries a parked worker's JIT roots
///   to the collector is that worker having published a *fresh* snapshot via
///   its own `update_root_snapshot` (which folds this scan in) **before it
///   parked at the STW safepoint**. The STW barrier (`gc_barrier.rs`) does
///   force every running mutator — including a thread spinning in JIT — to a
///   safepoint before the collector proceeds, and the safepoint publishes a
///   fresh snapshot, so in the *normal* flow the worker's current JIT roots
///   are visible.
///
///   The residual gap: if a worker's *published* snapshot is ever stale
///   relative to its current JIT spill slots (its slots changed after its
///   last publish and it reached the STW park without re-publishing), the
///   peer collector is **blind to those roots**. A full, correct fix is a
///   *cross-thread STW JIT root scan* — a per-thread snapshot of
///   `(JIT_ENTRY_CHAIN, current_sp)` captured AT the safepoint and walked by
///   the collector. That is a hard, separate feature (precise oop maps /
///   shadow-stack work tracks it); it is **NOT implemented here**.
///
/// ## Why a missed root has not been observed to corrupt the heap
///
/// While *any* thread is in JIT, `gc_quiescence::is_active()` is set, which
/// forces the **non-moving** young sweep with selective promotion — objects
/// are never relocated out from under a stale stack qword. So the failure
/// mode of the gap is *reclamation* of a still-live object, not a wrong
/// relocation. The published-snapshot mitigation above closes that in the
/// normal flow; the strict guard below (see [`warn_cross_thread_jit_gap`])
/// makes the residual unsupported condition **fail loudly** instead of
/// silently dropping a root.
///
/// ## The cross-thread half now exists
///
/// This section used to close with "FOLLOW-UP (tracked): implement the real
/// cross-thread STW JIT root scan ... Until then, single-thread-in-JIT is the
/// supported, verified path." That scan is [`crate::jit::xt_root_scan`]
/// (BUG-03, default ON since 2026-06): the initiator OS-freezes every peer
/// whose `Rip` is in compiled code and scans its registers, machine stack and
/// (since gc-common w1-c, 2026-09-23) shadow stack, and a post-barrier pass
/// does the same for BLOCKED peers with compiled frames below them. Peers that
/// park cooperatively still publish through this function on their own thread.
/// What remains thread-local is exactly what the paragraphs above say: this
/// function only ever sees the calling thread's chain.
///
/// # Filtering
///
/// 1. Only 8-byte aligned addresses are read (matches the JIT calling
///    convention's spill slot alignment).
/// 2. Each candidate value is passed to [`VmHeap::is_object_address`], which
///    confirms the address falls inside the heap arena AND lands on a valid
///    object header (correct alignment, valid kind, plausible class id).
/// 3. False positives only inflate the root set. On a moving cycle only a
///    slot an oop map verifies is rewritten; a word the scan cannot prove is
///    a reference is pinned instead (`publish_unrewritable_band_roots`), so a
///    false positive is never rewritten. (This used to say "the GC is in
///    non-compacting mode", which stopped being true when the moving young
///    collector landed.)
///
/// # Safety
///
/// The scanner reads raw memory between two stack-pointer values. Both
/// pointers come from the same thread's call stack and the read range is
/// always non-empty / well-defined: if `current_sp >= entry_sp` (chain
/// inverted) or the range is empty, the entry is silently skipped. We never
/// dereference the read qword as a Rust reference; it is treated as an
/// opaque address until validated.
pub fn scan_active_jit_frames(heap: &VmHeap, out: &mut Vec<ObjectRef>) {
    let _ = scan_active_jit_frames_impl(heap, out, ChainWalk::Scan);
}

/// [`scan_active_jit_frames`] for the flag-raising blocking deposit: answers
/// whether the compiled-chain walk RAN here (ki3, see
/// `xt_root_scan::helper_window_covered_chain_pin_only_enabled`). `false` for
/// every early exit that walks nothing: no chain entry, the diagnostic that
/// suppresses the scan, a band left to the pause.
pub(crate) fn scan_active_jit_frames_walking_chain(
    heap: &VmHeap,
    out: &mut Vec<ObjectRef>,
) -> bool {
    matches!(
        scan_active_jit_frames_impl(heap, out, ChainWalk::Scan),
        ChainCover::Walked
    )
}

/// nio21 wave 17 L3 (`CRATONVM_GC_LAZY_BLOCKED_JIT_DEPOSIT`): the blocking
/// deposit's [`scan_active_jit_frames`], with the CHAIN half deferred to the
/// collector when it provably reads only frozen memory
/// ([`DeferredJitChain`]). Everything before the chain walk (the unregistered
/// frame probe, the above-chain band) still runs here, into `out`. `Some` is
/// the deferred chain the caller must hand to its `GcBlockState` before it
/// raises `in_blocked_region`; `Walked` means the chain was scanned here as
/// always, `NotWalked` that there was none (or nothing was scanned).
pub(crate) fn scan_active_jit_frames_deferring_chain(
    heap: &VmHeap,
    out: &mut Vec<ObjectRef>,
) -> ChainCover {
    scan_active_jit_frames_impl(heap, out, ChainWalk::Defer)
}

/// What one [`scan_active_jit_frames_impl`] did with the compiled chain.
pub(crate) enum ChainCover {
    /// Walked nothing: no chain entry, or an exit before the walk.
    NotWalked,
    /// Walked every chain entry here, into the caller's roots.
    Walked,
    /// Deferred the walk to the collector ([`DeferredJitChain`]).
    Deferred(DeferredJitChain),
}

/// nio21 wave 18 L5 (`CRATONVM_GC_LAZY_BLOCKED_JIT_WAKE`, see
/// [`lazy_blocked_jit_wake_enabled`]): the wake refresh's
/// [`scan_active_jit_frames`] WITHOUT the compiled-chain walk. Everything
/// before the walk (the unregistered-frame probe, the above-chain band) still
/// runs into `out`. Only for a counted mutator's refresh (flag down) whose
/// window's entry deferred its chain: see the section on the lazy wake below
/// [`DeferredJitChain`] for why no reader needs the chain's roots in that
/// snapshot.
pub(crate) fn scan_active_jit_frames_skipping_chain(heap: &VmHeap, out: &mut Vec<ObjectRef>) {
    let _ = scan_active_jit_frames_impl(heap, out, ChainWalk::Skip);
}

/// What [`scan_active_jit_frames_impl`] does with the compiled chain.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(in crate::jit::conservative_roots) enum ChainWalk {
    /// Walk it here (every scan but the two below).
    Scan,
    /// Defer it to the collector when it reads only frozen memory
    /// ([`scan_active_jit_frames_deferring_chain`]).
    Defer,
    /// Leave it out ([`scan_active_jit_frames_skipping_chain`]).
    Skip,
}

/// The body of [`scan_active_jit_frames`]; `chain` selects
/// [`scan_active_jit_frames_deferring_chain`]'s or
/// [`scan_active_jit_frames_skipping_chain`]'s answer. Not inlined, so the
/// scanner SP below is this frame's, as the doc above describes.
#[inline(never)]
pub(in crate::jit::conservative_roots) fn scan_active_jit_frames_impl(
    heap: &VmHeap,
    out: &mut Vec<ObjectRef>,
    chain: ChainWalk,
) -> ChainCover {
    if dbg_no_jit_root_scan() {
        let n = NO_JIT_ROOT_SCAN_SUPPRESSED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        if n == 1 {
            eprintln!(
                "[jitrootscan] CRATONVM_DBG_NO_JIT_ROOT_SCAN engaged \
                 -- conservative JIT frame roots are NOT being published (UNSOUND)"
            );
        }
        return ChainCover::NotWalked;
    }
    // gch h4/bp: the band this scan would read is the pause's
    // (`with_band_at_pause_deposit`); not even the prune below runs, so the
    // deposit leaves the chain exactly as the pause will find it.
    if band_at_pause_deposit_armed() {
        return ChainCover::NotWalked;
    }
    // zt t32: a thread with no Java frame on its stack has no compiled frame
    // to scan. Every entry its chain still holds has returned (a deposit made
    // deeper than an earlier batch's entry would otherwise read that entry as
    // live and scan its band): pruned, as `prune_returned_jit_entries` prunes
    // a returned one, and the chain is covered, vacuously.
    if no_java_on_stack() {
        let _ = prune_returned_jit_entries(usize::MAX);
        return ChainCover::Walked;
    }
    // Capture the scanner's own SP. This function is NOT `#[inline]` (the
    // `#[inline(always)]` that suggested otherwise belonged to
    // `dbg_no_jit_root_scan`), so this is the SP of this frame, which lies
    // BELOW the caller's -- still a valid lower bound: every active JIT spill
    // region has its *lowest* address at or above this value (the Rust stack
    // grows downward on every supported target), and a deeper bound only
    // widens the scanned band by this frame (gc-common w2-c wording fix,
    // `common-c-takeover-small-residue` row 6). We use `current_stack_pointer` rather than reading `RSP`
    // directly so the implementation is portable across architectures.
    let scanner_sp = current_stack_pointer();
    // DBG (CRATONVM_DBG_FULLSTACK_SCAN): scan the ENTIRE native stack
    // [scanner_sp, stack_high] as conservative roots, not just the per-entry
    // [scanner_sp, entry_sp] ranges. Decisive experiment for the bintrees18
    // non-moving-sweep corruption: if this makes a live object visible (and
    // stops the sweep zeroing it), the missed root WAS on the stack but
    // outside the JIT chain's bounds (a range bug); if corruption persists,
    // the missed root is not on the stack at all. Validated by is_object_address.
    #[cfg(any(target_os = "windows", target_os = "linux"))]
    if dbg_fullstack_scan() {
        let high = current_thread_stack_high();
        if high > scanner_sp {
            scan_one_frame(scanner_sp, high, heap, out);
        }
    }
    // Round-7 corruption fix: before scanning, reclaim any LEAKED JIT
    // entry (a returned frame whose guard Drop was bypassed) so the GC's
    // `gc_quiescence` flag reflects only genuinely-live JIT frames. A
    // wedged flag forces the heap-corrupting non-moving young sweep on
    // every GC; healing it here lets the moving collector resume. Sound:
    // only entries strictly below the scanner SP (provably returned) are
    // pruned — live frames (entry_sp >= scanner_sp) are always retained.
    // DBG: CRATONVM_DBG_NO_PRUNE disables the self-heal so the leak (and its
    // non-moving-sweep corruption) can be A/B-reproduced in the same binary.
    if !dbg_no_prune() {
        let _ = prune_returned_jit_entries(scanner_sp);
    }
    let chain_len = JIT_ENTRY_CHAIN.with(|c| c.borrow().len());
    // gce e2/j: the JNI phase census's split of this scan (an in-native
    // deposit with `CRATONVM_DBG_JNI_PHASE=1` only; `None` otherwise).
    let mut census_clock = coverage_census_clock();

    // A5 fix — UNREGISTERED JIT frame on the native stack. A JIT method can be
    // live WITHOUT having pushed a `JitEntryGuard`: the process entry point
    // (`Vm::invoke` → compiled app `main`) is the canonical case — it can sit on
    // the stack while a clinit / interpreted callee runs and triggers a GC. With
    // `is_active()` false the generational collector picks the MOVING young
    // collector, which relocates that frame's live objects and cannot rewrite its
    // raw (register/spill) stack slots → stale all-zero-header receiver (the
    // `main`-compiled bintrees corruption).
    //
    // The registered chain covers `[scanner_sp, max(entry_sp))`; an unregistered
    // frame sits ABOVE that. Detect it via a JIT code return address in
    // `[cover_hi, stack_high)`. On a hit, conservatively scan that above-chain
    // band (MARK the frame's oops via `is_object_address`) and flag the collector
    // to run the NON-MOVING sweep — so the oops are pinned, not relocated, keeping
    // the unscannable raw slots valid. When the chain is NON-empty the sweep is
    // ALREADY non-moving (`is_active()` true), so this only adds the marking
    // (over-retention, safe — no collector-choice / throughput change); when it
    // is empty this also flips the collector off the moving path. Cheap-gated:
    // only when ≥1 method is compiled, only on the GC root-scan path, and the
    // scan is bounded + early-exits. Windows + Linux (the Linux port landed
    // with the 2026-07-10 GC audit INT-3 follow-up, once precise maps /
    // JIT_CODE_RANGES defaulted on everywhere).
    #[cfg(any(target_os = "windows", target_os = "linux"))]
    {
        let code_ranges = cratonvm_jit::jit_code_range_count();
        // arch-2026-07-26 (`moving-young-precise-roots`): this used to carry a
        // `!moving_young_enabled()` exemption matching the one in
        // `refresh_moving_young_coverage_for_current_thread`. Both are gone.
        // This function only runs at all when the precise-only path was NOT
        // taken (see `roots.rs`), i.e. when something already needs the
        // conservative backstop — and in that situation an unregistered frame's
        // oops MUST be marked, or the non-moving sweep reclaims them. Skipping
        // the probe here made the fallback path itself lossy under moving-young.
        if code_ranges > 0 {
            let cover_hi = JIT_ENTRY_CHAIN
                .with(|c| c.borrow().iter().map(|e| e.entry_sp).max())
                .unwrap_or(scanner_sp);
            let search_lo = scanner_sp.max(cover_hi);
            let hiwater_on = unreg_memo_hiwater_enabled();
            let decision = UNREG_JIT_MEMO.with(|c| {
                let mut m = c.get();
                let d = m.observe(search_lo, code_ranges, hiwater_on);
                c.set(m);
                d
            });
            // ENGAGEMENT census for the site that actually runs. The coverage
            // probe's counter reported `calls=0` on the StackWalker workload,
            // so this is where the 17.9% comes from; counting the verdict here
            // says whether the memo saves the scan or merely observes it.
            if a5_engagement_enabled() {
                A5_PROBE_CALLS.fetch_add(1, Ordering::Relaxed);
                match decision {
                    UnregScan::AlreadyClean => A5_PROBE_MEMO_CLEAN.fetch_add(1, Ordering::Relaxed),
                    UnregScan::Detect { hi: Some(_) } => {
                        A5_PROBE_MEMO_BANDED.fetch_add(1, Ordering::Relaxed)
                    }
                    UnregScan::Detect { hi: None } => {
                        A5_PROBE_FULL_RESCAN.fetch_add(1, Ordering::Relaxed)
                    }
                };
            }
            // gce e1/j: inside a JNI in-native deposit, a clean verdict over the
            // whole band recorded at the same chain state still holds (see
            // "The frozen band of a JNI in-native deposit"). Never true
            // outside one, so every other scan decides exactly as before.
            // gce e1b/j: a classified-clean (residue-only) verdict replays the
            // licence decision the decline below makes for it.
            let frozen = if decision == UnregScan::AlreadyClean {
                None
            } else {
                frozen_band_verdict(search_lo, current_thread_stack_high())
            };

            match frozen {
                Some(true) => coverage_census_note(crate::native::jni::JniPhase::A5MemoRaw),
                Some(false) => coverage_census_note(crate::native::jni::JniPhase::A5MemoClassified),
                None => {}
            }
            let already_clean = decision == UnregScan::AlreadyClean || frozen.is_some();
            // Consume the authoritative marker: this scan is the one the
            // preceding `invalidate_scan_cache_for_gc` was announcing.
            let authoritative = UNREG_JIT_AUTHORITATIVE.with(|c| c.replace(false));
            if already_clean && dbg_unreg_memo_audit() {
                // Pure observer: scan the range the memo just vouched for and
                // count the disagreement. Marks nothing, decides nothing.
                UNREG_MEMO_SHORTCIRCUITS.fetch_add(1, Ordering::Relaxed);
                let high = current_thread_stack_high();
                if high > search_lo && native_stack_has_jit_frame(search_lo, high).is_some() {
                    UNREG_MEMO_SUPPRESSED.fetch_add(1, Ordering::Relaxed);
                    if authoritative {
                        // A live JIT frame's oops are missing from a root set a
                        // collection is about to mark from. This is the defect.
                        UNREG_MEMO_SUPPRESSED_AUTHORITATIVE.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            // gen r5w1/young5: an SP that is not on this thread's own stack
            // makes the band below unverifiable (`own_stack_top` reports the
            // top unknown, so the probe reads at most the fixed 8 MiB cap).
            // Fail closed on the relocation licence, as a hit would.
            if !already_clean && !sp_on_own_stack(scanner_sp) {
                OWN_STACK_FOREIGN_SP.fetch_add(1, Ordering::Relaxed);
                cratonvm_gc::gc_quiescence::set_unregistered_jit_frame_on_stack();
                cratonvm_gc::gc_quiescence::mark_moving_young_coverage_incomplete_because(
                    cratonvm_gc::gc_quiescence::incomplete_reason::UNREGISTERED_JIT_FRAME,
                );
            }
        }
    }

    coverage_census_lap(crate::native::jni::JniPhase::JitA5, &mut census_clock);
    if chain_len == 0 {
        // Cross-thread JIT-root gap detector: this thread has no live JIT
        // frames, so this thread-local scan contributes nothing — but if some
        // OTHER thread is in JIT, and this scan is part of a peer-triggered STW
        // collector's root walk, that peer's JIT roots are invisible here (they
        // are carried only by the peer's last-published `root_snapshot`). Flag
        // the unsupported multi-thread-in-JIT condition loudly instead of
        // silently proceeding (see `warn_cross_thread_jit_gap` and the
        // `scan_active_jit_frames` doc). The `any_thread_in_jit()` guard keeps
        // the truly-quiescent common case (the overwhelming majority of
        // `update_root_snapshot`'s per-native-call invocations) zero-cost.
        if any_thread_in_jit() {
            warn_cross_thread_jit_gap();
        }
        // No live JIT frames on THIS thread — nothing to scan.
        return ChainCover::NotWalked;
    }
    // ABOVE-CHAIN CONSERVATIVE BAND — an OPT-IN LEVER, not a fix. Read
    // `above_chain_scan_enabled` before reaching for it.
    //
    // The chain scan below covers `[scanner_sp, max(entry_sp))` — every frame
    // BELOW the JIT entry. Nothing covers `[max(entry_sp), stack_high)`, the
    // VM's own Rust frames between the outermost interpreter entry and the
    // compiled call, except the A5 probe — and the A5 probe only marks that
    // band when it first finds a JIT RETURN ADDRESS in it. That is a real hole
    // in the root set, and closing it was the obvious reading of the
    // BindableTests ByteBuddy reclaim, because `CRATONVM_DBG_FULLSTACK_SCAN=1`
    // cures that failure and scanning this band is most of what it does.
    //
    // It is not the hole that failure falls through. Walking the band on every
    // collection (4978 passes, 3 056 492 roots, 1.0 GB of stack read) leaves
    // 26/27 exactly as it was, and so do the two other levers that widen the
    // same neighbourhood: `CRATONVM_NO_PRECISE_JIT_MAPS=1` (every chain entry
    // conservative, so the chain band is at its widest) and
    // `CRATONVM_JIT_UNREG_ACCEPT_RESIDUE=1` (the A5 filter's veto removed).
    // What remains of the fullstack diagnostic's difference is the PATH, not
    // the range: it also scans from `update_root_snapshot`, on every
    // object-returning native call. See `above_chain_all_paths`, and
    // `docs/feature-designs/real-jdk-mode.md`.
    //
    // Sound on the same terms as the chain band beside it, which has always
    // pushed conservative roots on cycles that could still relocate: a live
    // chain entry makes `gc_quiescence::is_active()` true, and
    // `collect_garbage_inner` then diverts to the non-moving sweep through
    // either `has_conservative_roots && !moving_young` or
    // `unrewritable_conservative_jit_roots` (which additionally requires that
    // a conservative scan ran this cycle — this one). Nothing is relocated, so
    // a pointer-shaped `i64` picked up here can only over-retain.
    //
    // `CRATONVM_DBG_ABOVE_CHAIN_KB=<n>` caps the band at `n` KiB above the
    // chain, which is how a holder's depth would be bisected once one is found
    // up there.
    #[cfg(any(target_os = "windows", target_os = "linux"))]
    if above_chain_scan_enabled() && (gc_root_pass_active() || above_chain_all_paths()) {
        let cover_hi = if above_chain_from_sp() {
            scanner_sp
        } else {
            JIT_ENTRY_CHAIN
                .with(|c| c.borrow().iter().map(|e| e.entry_sp).max())
                .unwrap_or(scanner_sp)
                .max(scanner_sp)
        };
        let high = current_thread_stack_high();
        let hi = match above_chain_scan_cap_bytes() {
            Some(cap) => high.min(cover_hi.saturating_add(cap)),
            None => high,
        };
        if hi > cover_hi {
            let before = out.len();
            scan_one_frame(cover_hi, hi, heap, out);
            above_chain::note(out.len() - before, hi - cover_hi);
        }
    }
    // Always a fresh scan: the WS1 JIT-scan cache that could reuse an earlier
    // one was deleted (round 11 wave 15; see the note above
    // `note_jit_boundary`).
    scan_prof::bump(&scan_prof::SCANS);
    // nio21 wave 17 L3: the blocking deposit's chain half, deferred when it
    // reads only frozen memory (see `DeferredJitChain`).
    // nio21 wave 18 L5: the lazy wake's refresh leaves the chain out (see
    // `lazy_blocked_jit_wake_enabled`).
    if chain == ChainWalk::Skip {
        LAZY_CHAIN_CENSUS[LAZY_WAKE_SKIPPED].fetch_add(1, Ordering::Relaxed);
        coverage_census_lap(
            crate::native::jni::JniPhase::JitChainFrames,
            &mut census_clock,
        );
        return ChainCover::NotWalked;
    }
    if chain == ChainWalk::Defer {
        if let Some(deferred) = defer_chain_if_frozen(scanner_sp) {
            LAZY_CHAIN_CENSUS[LAZY_DEFERRED].fetch_add(1, Ordering::Relaxed);
            coverage_census_lap(
                crate::native::jni::JniPhase::JitChainFrames,
                &mut census_clock,
            );
            return ChainCover::Deferred(deferred);
        }
        LAZY_CHAIN_CENSUS[LAZY_REFUSED].fetch_add(1, Ordering::Relaxed);
    }
    scan_active_jit_frames_with_sp(scanner_sp, heap, out);
    coverage_census_lap(
        crate::native::jni::JniPhase::JitChainFrames,
        &mut census_clock,
    );
    ChainCover::Walked
}

/// Inner scanner: takes the caller-supplied scanner SP so it can be a
/// non-inlined function (which keeps code size sensible).
///
/// NEW-12: dispatches each chain entry to either the precise oop-map
/// walker or the conservative range scan based on the entry's
/// `precise` field. The precise path enumerates exact oops from the
/// compiled method's map; the conservative path is the NEW-1 blind
/// scan, still required for entries that were pushed without precise
/// metadata.
pub fn scan_active_jit_frames_with_sp(scanner_sp: usize, heap: &VmHeap, out: &mut Vec<ObjectRef>) {
    JIT_ENTRY_CHAIN.with(|c| {
        {
            let mut chain = c.borrow_mut();
            flush_top_rbp_cache_to_chain(chain.as_mut_slice());
        }
        let chain = c.borrow();
        // Every CONSERVATIVE entry is scanned as `[scanner_sp, entry_sp)` --
        // they all share the same low bound, so a chain of K conservative
        // entries used to walk the innermost frames K times over (O(K * depth)
        // word reads and `is_object_address` calls for a root set that is, by
        // construction, the union). The union of intervals sharing a low bound
        // is just the widest one, so take the max `entry_sp` and scan once.
        // Coverage is identical; only duplicate root pushes disappear (`out` is
        // a root list -- order and multiplicity are already immaterial to every
        // consumer). The precise entries keep their per-frame walk: each one
        // enumerates its OWN frame's oop-map slots, not a range.
        let mut conservative_high = 0usize;
        for entry in chain.iter() {
            match entry.precise {
                Some(info) => {
                    scan_prof::bump(&scan_prof::PRECISE_FRAMES);
                    scan_one_frame_precise(info, heap, out)
                }
                None => {
                    coverage_census_note(crate::native::jni::JniPhase::FrameConservative);
                    conservative_high = conservative_high.max(entry.entry_sp)
                }
            }
        }
        if conservative_high != 0 {
            scan_prof::bump(&scan_prof::BAND_SCANS);
            let band = conservative_high.saturating_sub(scanner_sp) as u64;
            scan_prof::add(&scan_prof::BAND_WORDS, band / 8);
            scan_prof::observe_max(&scan_prof::BAND_MAX_BYTES, band);
            scan_one_frame(scanner_sp, conservative_high, heap, out);
        }
    });
}

// ---------------------------------------------------------------------------
// nio21 wave 17 L3 -- the lazy blocked-thread JIT deposit
// (`CRATONVM_GC_LAZY_BLOCKED_JIT_DEPOSIT`, opt-in; see
// `docs/feature-designs/jit-ir-optimizations.md`)
// ---------------------------------------------------------------------------
//
// A blocking native (`begin_blocking_region_with_state`) deposits this thread's
// roots and then becomes invisible to every collector except through that
// deposit, so the deposit used to run the whole compiled-chain scan
// (`scan_precise_frames_and_bands` per entry) on EVERY blocking call -- a pipe
// round trip pays it twice -- although almost no call has a collection land in
// its window.
//
// Lazy, the deposit records only what the chain walk reads ([`DeferredJitChain`]:
// the scanner SP and each entry's `PreciseFrameInfo`, after the top-RBP mirror
// was flushed into it) and the collector runs the SAME walk in its root
// gathering, once per pause, for every thread still blocked with a record
// ([`resolve_deferred_blocked_jit_chains`]). The soundness obligations:
//
// 1. Frozen input. The walk reads only compiled frames -- each frame's own band
//    `[rbp - frame_size, rbp)`, its saved RBP and its return address -- plus
//    code-range and compile-id lookups. A blocked thread cannot run compiled
//    code before it leaves the region, and it cannot leave while a pause is
//    active (`GcBarrier::leave_blocked_region_flagged` drains pauses first), so
//    every word the walk reads is the word the deposit would have read. The
//    deposit DEFERS ONLY when a dry walk proves the scan would stay on that
//    frozen memory ([`precise_band_walk_is_complete`]): every chain entry
//    precise, the innermost method resolvable, every frame's layout usable and
//    every parent link valid. Anything that would fall back to a layout-free
//    sweep of `[scanner_sp, ..)` -- the Rust frames below the door, which the
//    thread keeps running in -- is scanned eagerly as before.
// 2. The bodies stay alive. Every compiled frame of the chain has a return
//    address on the blocked stack, so the code-reclamation summary of the
//    window (`cratonvm_jit::jit_thread_blocked_enter`) keeps its body, and the
//    `CompiledMethod` pointers the record holds stay valid until the leave.
// 3. Same roots, same pins. The collector's walk runs under a
//    `RootDepositScope`, so it makes no movable / unrewritable claim -- exactly
//    the deposit's rule (a claim is only true for the collector's OWN frames,
//    which its remap walks). Every address found is MARKED (into `xt_roots`,
//    like a helper window's) and PINNED for the pause
//    (`add_xt_cycle_pinned_jit_roots`), as the eager deposit pinned it through
//    `publish_pinned_jit_roots` for every collector; G1 band-reject pins are
//    captured the same way. Every pin consumer runs inside the requester's
//    pause, after this root gathering (`gc_quiescence`'s pin-reader audit).
// 4. Nothing else moves. The deposit's interpreter half, off-frame roots,
//    shadow stack, deopt stash, the unregistered-frame probe and the
//    above-chain band all stay eager; only the chain walk is deferred.
// 5. The record dies with the window. Every deposit clears it before it
//    publishes, and the wake (`check_post_block_gc_inner`) takes it right after
//    the flag goes down, and refuses the wave-68 kept-snapshot shortcut when it
//    held one (that snapshot lacks the chain roots).

/// The deferred chain half of one blocking deposit (see the section above).
pub(crate) struct DeferredJitChain {
    /// The depositing scan's own SP: the lower bound every frame link was
    /// validated against, as in [`scan_one_frame_precise`].
    pub(in crate::jit::conservative_roots) scanner_sp: usize,
    /// Every chain entry's precise metadata, innermost RBP flushed.
    pub(in crate::jit::conservative_roots) entries: Vec<PreciseFrameInfo>,
}

// SAFETY: the raw pointers in `PreciseFrameInfo` are dereferenced only by the
// collector's walk, during a pause, while the owning thread is still blocked
// with this record published -- the window in which the JIT cache and the
// window's code-reclamation summary keep every named `CompiledMethod` alive
// (obligation 2 above). The record is never read outside that window.
unsafe impl Send for DeferredJitChain {}
unsafe impl Sync for DeferredJitChain {}

/// The lazy deposit's census (process-wide diagnostics counters, one static
/// for the statics ratchet), indexed by the `LAZY_*` constants below: blocking
/// deposits whose chain walk was deferred / refused deferral (the dry walk
/// said it would leave frozen memory), and records the collector resolved /
/// that needed the layout-free fallback at resolution.
pub static LAZY_CHAIN_CENSUS: [AtomicUsize; 5] = [
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
];
pub const LAZY_DEFERRED: usize = 0;
pub const LAZY_REFUSED: usize = 1;
pub const LAZY_RESOLVED: usize = 2;
pub const LAZY_RESOLVE_FALLBACKS: usize = 3;
/// nio21 wave 18 L5: wake refreshes that left the chain out
/// ([`scan_active_jit_frames_skipping_chain`]).
pub const LAZY_WAKE_SKIPPED: usize = 4;

/// `CRATONVM_GC_LAZY_BLOCKED_JIT_DEPOSIT=1` -- defer the blocking deposit's
/// compiled-chain walk to the collector (see the section above). Default ON
/// since nio21 wave 18: it earned it with collector stress, not an argument
/// (w18 host: `Nio21BlockedRoots` bad=0 plain / `CRATONVM_DBG_GC_STRESS=1` /
/// `CRATONVM_XT_JIT_ROOT_SCAN=0`, `Nio21CasRace` lost=0, `Nio21SatbStress`
/// clean, under ZGC, G1 and Generational; `pipe` 46.9 -> 22.8 us/op). `=0`
/// restores the eager walk.
pub(crate) fn lazy_blocked_jit_deposit_enabled() -> bool {
    lazy_blocked_jit_switches().0
}

/// The two lazy blocked-deposit switches, read once per process into ONE
/// cache (the per-VM statics ratchet counts declarations): the deposit
/// (`CRATONVM_GC_LAZY_BLOCKED_JIT_DEPOSIT`, default on) and the wake
/// (`CRATONVM_GC_LAZY_BLOCKED_JIT_WAKE`, default on, only with the deposit).
pub(in crate::jit::conservative_roots) fn lazy_blocked_jit_switches() -> (bool, bool) {
    static ON: std::sync::OnceLock<(bool, bool)> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        let deposit =
            true;
        let wake = deposit;
        (deposit, wake)
    })
}

// nio21 wave 18 L5 -- the lazy WAKE refresh
// (`CRATONVM_GC_LAZY_BLOCKED_JIT_WAKE`, default ON, acts only under
// `CRATONVM_GC_LAZY_BLOCKED_JIT_DEPOSIT`; page
// `r21-blocking-wake-refresh-rescans-the-compiled-chain-FIXED-20261009.md`).
//
// A blocking native deposits twice: at the region's entry (flag up; lazy, its
// chain walk is the collector's) and at the wake (`deposit_root_snapshot_no_flag`,
// flag already down). The wake's refresh used to walk the whole compiled chain
// again. Under this switch it leaves the chain walk out
// (`scan_active_jit_frames_skipping_chain`) when the window's entry deferred
// its chain AND no collection folded into the window (`folds_k == 0`, empty
// fixup) -- the per-call case of a `Pipe` round trip. Everything else of the
// refresh (frames, origins, off-frame roots, the unregistered-frame probe, the
// above-chain band, the pin publication of what it did scan) is unchanged.
//
// Why no reader needs the chain's roots in that snapshot. After the leave the
// thread is a COUNTED mutator, and a counted mutator's snapshot is never the
// collector's view of its compiled frames:
//
// 1. Every root-gathering pause completes only when each counted mutator has
//    ARRIVED or been FROZEN (`stw_take_over_and_wait` loops until
//    `wait_for_all_timeout` is satisfied; without the take-over it is a plain
//    `wait_for_all`). Every arrival door republishes first, with a fresh
//    compiled-frame scan: the safepoint park (`update_root_snapshot` in
//    `safepoint_check`, which the JIT poll slow path reaches), and every
//    blocked-region / in-native / park / FFM entry (a flag-raising deposit,
//    whose chain is scanned or deferred-and-resolved). A frozen peer
//    (`xt_root_scan`) has its registers and whole used stack scanned
//    conservatively -- a superset of its compiled frames -- and its roots
//    pinned for the pause.
// 2. No pause reads a counted, running thread's snapshot instead of one of
//    those: a thread whose `Rip` is outside compiled code is RESUMED and
//    waited for (it arrives through a door of 1).
// 3. The snapshot is reused under a re-raised flag only by the debugger park
//    region's standing deposit (`ParkGcRegion::reuse_standing`), whose key
//    includes the publish sequence this refresh bumps, and whose own entry is
//    the EAGER `deposit_root_snapshot` (no deferred chain, so its wake never
//    skips). The wave-68 kept snapshot is refused for a deferred window
//    (`had_deferred_chain` in `check_post_block_gc_inner`).
// 4. Restricted to an unfolded window: a window a collection moved things in
//    keeps the eager refresh (rare, so the cost does not matter), and the
//    pins the refresh publishes for what it scanned replace the entry's,
//    which covered the same non-chain words.
//
// Residual (the same as `CRATONVM_GC_NOFLAG_DEPOSIT_SKIP_JIT_SCAN`, narrower):
// diagnostic readers of every thread's snapshot outside a pause
// (`collect_all_root_snapshots` for the HPROF root list) do not see the
// chain's roots of a thread that woke and has not republished since.

/// `CRATONVM_GC_LAZY_BLOCKED_JIT_WAKE` (default ON; `=0` restores the eager
/// wake refresh). Only ever consulted with
/// [`lazy_blocked_jit_deposit_enabled`] on, so it changes nothing unless that
/// opt-in is set. See the section above.
pub(crate) fn lazy_blocked_jit_wake_enabled() -> bool {
    lazy_blocked_jit_switches().1
}

/// The deferred chain of THIS thread, when the walk
/// [`scan_active_jit_frames_with_sp`] would make from `scanner_sp` reads only
/// frozen compiled-frame memory; `None` otherwise (the caller then scans).
pub(in crate::jit::conservative_roots) fn defer_chain_if_frozen(scanner_sp: usize) -> Option<DeferredJitChain> {
    // Diagnostics that publish from inside the walk on the CALLING thread's
    // behalf, and the whole-band lever, keep the eager scan.
    if coverage_pin_enabled()
        || verify_oop_maps_enabled()
        || pin_unnamed_frame_refs_enabled()
    {
        return None;
    }
    JIT_ENTRY_CHAIN.with(|c| {
        {
            let mut chain = c.borrow_mut();
            flush_top_rbp_cache_to_chain(chain.as_mut_slice());
        }
        let chain = c.borrow();
        let mut entries = Vec::with_capacity(chain.len());
        for entry in chain.iter() {
            // A conservative entry's band is `[scanner_sp, entry_sp)`: the
            // Rust frames below the door, which do not stay frozen.
            let info = entry.precise?;
            if !precise_band_walk_is_complete(info, scanner_sp) {
                return None;
            }
            entries.push(info);
        }
        (!entries.is_empty()).then_some(DeferredJitChain {
            scanner_sp,
            entries,
        })
    })
}

/// Would [`scan_one_frame_precise`] cover `info` with its bounded frame bands
/// alone -- no layout-free sweep of `[scanner_sp, ..)` anywhere? A dry run of
/// the stopping rules of [`scan_precise_frames_and_bands`]'s band half (and of
/// the exact-RBP test in front of it), reading only frame links and layouts.
pub(in crate::jit::conservative_roots) fn precise_band_walk_is_complete(info: PreciseFrameInfo, scanner_sp: usize) -> bool {
    let entry_sp = info.frame_base;
    let mut rbp = info.exact_rbp;
    if rbp == 0 || rbp & 0x7 != 0 || rbp < scanner_sp || rbp >= entry_sp {
        return false;
    }
    // An unresolvable innermost frame gets a layout-free `[scanner_sp, rbp)`.
    let Some(innermost) = innermost_frame_method(
        rbp,
        info.exact_cm_id,
        entry_sp,
        scanner_sp,
        info.compiled_method,
    ) else {
        return false;
    };
    // SAFETY: as in `scan_precise_frames_and_bands` -- Arc-owned by the JIT
    // cache while any of its frames is live.
    if !compiled_frame_layout_scannable(rbp, unsafe { &*innermost }) {
        return false;
    }
    let mut band_frames = 1usize;
    let mut guard = 0usize;
    while guard < 4096 {
        guard += 1;
        // SAFETY: `rbp` is an aligned address in this thread's live JIT stack
        // interval (validated above, then as each parent link below), as in
        // `scan_precise_frames_and_bands`.
        let parent_rbp = unsafe { (rbp as *const usize).read() };
        let ret_addr = unsafe { ((rbp + 8) as *const usize).read() };
        let Some(cm_ptr) = cratonvm_jit::lookup_jit_code_range(ret_addr) else {
            return true;
        };
        if parent_rbp <= rbp
            || parent_rbp & 0x7 != 0
            || parent_rbp < scanner_sp
            || parent_rbp >= entry_sp
        {
            return false;
        }
        if band_frames >= 4096 {
            return false;
        }
        band_frames += 1;
        // SAFETY: code ranges retain their CompiledMethod metadata for the
        // lifetime of an active frame.
        let parent_cm: &cratonvm_jit::CompiledMethod =
            unsafe { &*(cm_ptr as *const cratonvm_jit::CompiledMethod) };
        if !compiled_frame_layout_scannable(parent_rbp, parent_cm) {
            return false;
        }
        rbp = parent_rbp;
    }
    false
}

/// The `true` answer of [`scan_one_compiled_frame_with_layout`]'s layout test:
/// a usable recorded frame size.
pub(in crate::jit::conservative_roots) fn compiled_frame_layout_scannable(rbp: usize, cm: &cratonvm_jit::CompiledMethod) -> bool {
    let frame_size = cm.osr_frame_size;
    if frame_size <= 0 {
        return false;
    }
    let frame_size = frame_size as usize;
    frame_size <= 1024 * 1024 && frame_size <= rbp
}

/// The collector half of the lazy blocked deposit: walk the deferred chain of
/// every thread still blocked with one, MARK what it names (into `xt_roots`)
/// and PIN it for this pause. Called once per root-gathering pause from
/// `stw_take_over_and_wait`, after the barrier is satisfied (every blocked
/// thread is held in its region until the pause ends). Answers the roots added.
/// One cached-flag load with the switch off.
pub(crate) fn resolve_deferred_blocked_jit_chains(
    shared: &crate::vm::SharedVm,
    xt_roots: &mut Vec<ObjectRef>,
) -> usize {
    if !lazy_blocked_jit_deposit_enabled() {
        return 0;
    }
    let heap = &shared.mem.heap;
    // The deposit's rules for the walk (obligation 3 above): no movable /
    // unrewritable claims, the deposit's envelope-first test, and the G1
    // band-reject pins captured.
    let _deposit_scope = crate::native::jni::RootDepositScope::enter();
    let reject_capture = g1_band_reject_pins::Capture::arm_if_wanted(heap, false);
    let mut pins: Vec<usize> = Vec::new();
    let mut found = 0usize;
    shared
        .threads
        .thread_registry
        .for_each_gc_block_state(|state| {
            if state.owner_dropped()
                || !state
                    .in_blocked_region
                    .load(std::sync::atomic::Ordering::Acquire)
            {
                return;
            }
            let record = state.deferred_jit_chain.lock();
            let Some(chain) = record.as_ref() else {
                return;
            };
            let before = xt_roots.len();
            for &info in &chain.entries {
                if !scan_precise_frames_and_bands(info, chain.scanner_sp, true, heap, xt_roots) {
                    // Not reachable for a record the dry walk admitted (its
                    // frames cannot change); kept as `scan_one_frame_precise`'s
                    // own fallback so a broken invariant over-retains rather
                    // than drops.
                    LAZY_CHAIN_CENSUS[LAZY_RESOLVE_FALLBACKS].fetch_add(1, Ordering::Relaxed);
                    scan_one_frame(chain.scanner_sp, info.frame_base, heap, xt_roots);
                }
            }
            pins.extend(xt_roots[before..].iter().map(|r| r.as_ptr() as usize));
            found += xt_roots.len() - before;
            LAZY_CHAIN_CENSUS[LAZY_RESOLVED].fetch_add(1, Ordering::Relaxed);
        });
    let reject_pins = reject_capture.map(|c| c.finish()).unwrap_or_default();
    g1_band_reject_pins::extend_published(&reject_pins, &mut pins);
    cratonvm_gc::gc_quiescence::add_xt_cycle_pinned_jit_roots(&pins);
    found
}

/// JIT round 23 wave 1 L7 -- the HPROF residual of the lazy deposit (the
/// wave 17 page's third residual: "the HPROF heap dump's root list no longer
/// names a lazily blocked thread's compiled-frame roots"). The compiled-frame
/// roots ONE blocked thread's deferred chain names: the walk
/// [`resolve_deferred_blocked_jit_chains`] makes for it, read-only -- no pin,
/// no cycle census bump, no claim (`RootDepositScope`, as the resolver). Empty
/// when the switch is off or the thread is not blocked with a record.
///
/// The caller must hold a pause through the barrier (the heap dump's own
/// `NonMovingPause`): `GcBarrier::leave_blocked_region_flagged` drains every
/// active pause before the flag drops, so the blocked thread cannot leave its
/// window, and so cannot run compiled code or unwind the frames the record
/// names, while this reads them (obligations 1 and 2 of the section above).
pub(crate) fn deferred_blocked_jit_chain_roots(
    heap: &VmHeap,
    state: &crate::threading::jvm_thread::GcBlockState,
) -> Vec<ObjectRef> {
    let mut out: Vec<ObjectRef> = Vec::new();
    if !lazy_blocked_jit_deposit_enabled()
        || state.owner_dropped()
        || !state
            .in_blocked_region
            .load(std::sync::atomic::Ordering::Acquire)
    {
        return out;
    }
    let record = state.deferred_jit_chain.lock();
    let Some(chain) = record.as_ref() else {
        return out;
    };
    let _deposit_scope = crate::native::jni::RootDepositScope::enter();
    for &info in &chain.entries {
        if !scan_precise_frames_and_bands(info, chain.scanner_sp, true, heap, &mut out) {
            // The resolver's own fallback (over-retains, never drops).
            scan_one_frame(chain.scanner_sp, info.frame_base, heap, &mut out);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Stage 3 (precise oop maps) — exact RBP recording + precise relocation
// ---------------------------------------------------------------------------

/// Does the innermost JIT entry this thread pushed name `cm` -- did a Rust
/// door (`JitEntryGuard::enter_with_compiled`: the interpreter's method-entry
/// doors, the dispatch helpers, a not-entrant re-dispatch) enter `cm` LAST,
/// with no JIT entry since? Interpreter round i2 wave 54, lane L2 (the
/// per-activation question of
/// `docs/feature-designs/real-jdk-mode.md`).
///
/// A compiled caller's direct `CALL` pushes no entry, so for a body that can
/// call nothing (its activation is the innermost compiled frame whenever it
/// polls) `true` means its running activation was entered by that door, and
/// `false` that a compiled frame called it, or that the door pushed a
/// conservative entry naming no method.
///
/// `polling_rbp` is the polling activation's frame base (the RBP mirror its
/// prologue published, paired with the compile id the verdict confirmed). It
/// must lie strictly below the entry's `entry_sp`: a door's entry was taken
/// in the door's own frame, above the body it called. An entry ABANDONED by
/// a non-local exit (`JitEntryGuard`'s drop restores the depth only when the
/// enclosing guard drops) was taken in a helper frame below a compiled
/// caller, so a body that caller later CALLs directly sits ABOVE it, and the
/// stale entry is not taken for its door. Fail closed: a zero base, a busy
/// chain or no entry answer `false`. Never blocks, allocates nothing.
pub(crate) fn innermost_entry_names(
    cm: *const cratonvm_jit::CompiledMethod,
    polling_rbp: usize,
) -> bool {
    if polling_rbp == 0 {
        return false;
    }
    JIT_ENTRY_CHAIN
        .try_with(|c| {
            c.try_borrow().ok().is_some_and(|chain| {
                chain.last().is_some_and(|top| {
                    polling_rbp < top.entry_sp
                        && top
                            .precise
                            .is_some_and(|info| std::ptr::eq(info.compiled_method, cm))
                })
            })
        })
        .unwrap_or(false)
}

/// Publish (`pc != 0`) or retract (`0`) the not-entrant stub's intercepted
/// return address on the TOP chain entry; returns the previous value so the
/// caller restores it (a re-dispatch can nest). No-op on a conservative top.
pub(crate) fn swap_top_not_entrant_ret_pc(pc: usize) -> usize {
    JIT_ENTRY_CHAIN.with(|c| {
        c.borrow_mut()
            .last_mut()
            .and_then(|top| top.precise.as_mut())
            .map_or(0, |info| {
                std::mem::replace(&mut info.not_entrant_ret_pc, pc)
            })
    })
}

/// Stage 3 — record the EXACT RBP of the *innermost* active JIT frame.
///
/// Called from the JIT prologue (via the `frame_record` helper) immediately
/// after `mov rbp, rsp`, when `CRATONVM_PRECISE_JIT_MAPS` is on. The matching
/// chain entry was pushed by [`JitEntryGuard::enter_with_compiled`] just before
/// control transferred to compiled code, but it could only capture an
/// approximate Rust-side SP as `frame_base`. This overwrites it with the
/// precise value so the relocation walker can address oop-map slots as
/// `[rbp - offset]`.
///
/// No-op if the top entry is conservative (`precise == None`): such methods
/// have no oop maps and are never relocated through this path.
pub fn set_top_frame_base(rbp: usize) {
    // HOT path (once per JIT invocation): write only the cached mirror. The
    // value is synced into the top chain entry's `exact_rbp` at push/pop/retain
    // and flushed by `remap_active_jit_frames` before the (only) reader runs.
    // `frame_base` is deliberately NOT touched — the marking path uses it as the
    // upper bound of its conservative sweep.
    top_rbp_set(rbp);
    if moving_young_enabled() {
        JIT_ENTRY_CHAIN.with(|c| {
            if let Some(top) = c.borrow_mut().last_mut() {
                if let Some(info) = top.precise.as_mut() {
                    info.exact_rbp = rbp;
                    // This RBP came from the caller, not from the mirror, so
                    // the mirror's id is not known to describe it. Publishing
                    // nothing is the honest answer — the scan falls back to
                    // decoding the call, exactly as before.
                    info.exact_cm_id = 0;
                }
            }
        });
    }
}

/// Sync `TOP_RBP` to the current top entry's saved `exact_rbp` (or 0 when the
/// chain is empty). Called after pop/retain so the mirror tracks the new top.
///
/// **Only call this when the top entry actually CHANGED.** The mirror is the
/// live value (the prologue's inline `mov gs:[disp], rbp` writes it and nothing
/// writes `exact_rbp`); the field is only a snapshot taken when an entry stops
/// being top. Reloading the field over an unchanged top therefore replaces a
/// live RBP with a stale one — `0` for an entry that has been top since it was
/// pushed, which is the common case. See `prune_returned_jit_entries`.
#[inline]
pub(in crate::jit::conservative_roots) fn reload_top_rbp_cache(v: &[JitFrameChainEntry]) {
    let (rbp, cm_id) = v
        .last()
        .and_then(|e| e.precise.as_ref())
        .map_or((0, 0), |info| (info.exact_rbp, info.exact_cm_id));
    top_rbp_set(rbp);
    // BOTH halves, from the SAME snapshot. Restoring only the RBP is what
    // `top_cm_id_mirror_read` warns about in as many words: it leaves the pair
    // naming two different frames — this entry's rbp beside whatever method
    // last ran — "and the scan cannot detect that, because both halves would
    // still read consistently out of the mirrors".
    //
    // That is not hypothetical. It is what
    // `bug-h2-testkillprocess-zgc-oom-at-97-percent-free-20260821`'s
    // `ACTIVE_FRAME_MAP` residual was: `[frame-cov]` showed ONE rbp
    // (`0x…3a00`, correctly restored here) claimed across collections by four
    // different methods (`MVMap.put`, `MVStore.openMap`,
    // `MVStore$Builder.autoCommitDisabled`, `RootReference.isLocked` — the last
    // with `maps=0`, so it emits no safepoint and cannot be the frame at a
    // collection at all). `moving_young_frame_coverage_complete` then read
    // `[rbp - wrong_method.sp_id_slot_off]`, matched no map, and refused the
    // cycle.
    //
    // The path that makes it the coverage proof's OWN input:
    // `refresh_moving_young_coverage_for_current_thread` calls
    // `prune_returned_jit_entries` first, which lands here when it pruned
    // anything, and then immediately stamps
    // `info.exact_rbp = top_rbp_get(); info.exact_cm_id = published_compile_id()`
    // — a correct rbp paired with the identity this function failed to move.
    //
    // Zero is the honest value when the top entry has no snapshot: it means
    // "nothing published", which routes `published_innermost_method` to the
    // stack decode and, failing that, fails closed. A WRONG id does not fail
    // closed; it resolves confidently to the wrong map.
    if cm_id_pairing_enabled() {
        top_cm_id_mirror_write(cm_id);
    }
}

/// `CRATONVM_GC_NO_CM_ID_PAIRING=1` — stop moving the compile-id mirror with
/// the RBP mirror, restoring the 2026-08-24 behaviour in which only the RBP
/// half was reset on push and restored on pop.
///
/// The bisect lever for that repair, default-ON. Latched: it decides what the
/// entry-chain bookkeeping does, and that must not change under a running
/// process.
pub(in crate::jit::conservative_roots) fn cm_id_pairing_enabled() -> bool {
    static G: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&G, || !false)
}

#[inline]
pub(in crate::jit::conservative_roots) fn flush_top_rbp_cache_to_chain(v: &mut [JitFrameChainEntry]) {
    if let Some(top) = v.last_mut() {
        if let Some(info) = top.precise.as_mut() {
            info.exact_rbp = top_rbp_get();
            info.exact_cm_id = published_compile_id();
        }
    }
}

/// The `shadow_owned` argument of [`remap_active_jit_frames`] for a thread
/// whose shadow stack is remapped after the walk: the words it publishes by
/// address. Empty when the shadow stack is not remapped, so the walk stays the
/// only rewriter of every slot.
pub fn shadow_owned_slots(shadow: &cratonvm_gc::shadow_stack::ShadowStack) -> Vec<usize> {
    if shadow_stack_enabled() {
        shadow.indirect_slots()
    } else {
        Vec::new()
    }
}

/// Stage 3 — precisely relocate the oop slots of every active JIT frame on
/// the current thread after a moving collection.
///
/// For each frame compiled with the precise gate on (`cm.sp_id_slot_off != 0`,
/// which also guarantees `frame_base` is the EXACT RBP recorded by
/// [`set_top_frame_base`]):
///   1. read the active safepoint's bytecode PC from `[rbp - sp_id_slot_off]`;
///   2. look up the matching [`OopMapEntry`] (`bytecode_pc == sp_id`);
///   3. for each recorded slot at `[rbp - offset]`, if the stored address moved
///      (`pointer_map[old] = new`), rewrite the slot in place.
///
/// This is the JIT-frame analogue of `update_all_roots` for interpreter
/// frames — the piece that makes a *moving* collector safe while JIT frames
/// are live. Inert by default: when the gate is off every method has
/// `sp_id_slot_off == 0`, so the walk skips all frames and touches nothing.
///
/// `shadow_owned` is this thread's `ShadowStack::indirect_slots()`: the frame
/// words its shadow stack publishes by address, which `ShadowStack::remap`
/// rewrites. The walk leaves those alone — every caller follows it with that
/// remap, and rewriting a word in both places applies the map to it twice.
/// Pass `&[]` when the shadow stack is not remapped.
///
/// # Safety
/// Reads and writes aligned qwords on the calling thread's own stack between
/// known-valid frame slots. The CompiledMethod is kept alive by the JIT cache
/// for the duration of the call (same contract as [`scan_one_frame_precise`]).
pub fn remap_active_jit_frames(pointer_map: &cratonvm_types::PointerMap, shadow_owned: &[usize]) {
    if pointer_map.is_empty() {
        return;
    }
    let scanner_sp = current_stack_pointer();
    // gen r5w2/roots6: every store below lands in `[scanner_sp, stack top)`.
    let store_band = jit_remap_store_band(scanner_sp);
    let frame_map = pointer_map;
    // Stage 5 diagnostic (CRATONVM_DBG_PRECISE): count frames walked / slots
    // rewritten / chain entries so we can see whether the RBP-chain walk
    // actually engages. Printed once per remap call (grep-friendly).
    let dbg = false;
    let mut dbg_entries = 0usize;
    let mut dbg_precise = 0usize;
    let mut dbg_frames = 0usize;
    let dbg_slots = std::cell::Cell::new(0usize);
    let dbg_maps_found = std::cell::Cell::new(0usize);
    let dbg_examined = std::cell::Cell::new(0usize);
    JIT_ENTRY_CHAIN.with(|c| {
        // Flush the cached top `exact_rbp` into the top chain entry so the walk
        // below sees the live innermost RBP.
        {
            let mut v = c.borrow_mut();
            flush_top_rbp_cache_to_chain(v.as_mut_slice());
        }
        let chain = c.borrow();
        for entry in chain.iter() {
            dbg_entries += 1;
            let Some(info) = entry.precise else { continue };
            dbg_precise += 1;
            let entry_sp = entry.entry_sp;
            // The innermost frame's child is a Rust helper, so its return
            // address cannot resolve this frame through the parent walk below.
            // Remap it directly from the CompiledMethod registered at the
            // interpreter-to-JIT boundary.  This is required for *every*
            // moving collection, not just OSR: the active safepoint id and
            // oop slots are addressed from the exact RBP recorded by the
            // prologue, and leaving this boundary frame unpatched strands the
            // current method's register/frame oops at pre-move addresses.
            if info.exact_rbp != 0
                && info.exact_rbp & 0x7 == 0
                && info.exact_rbp >= scanner_sp
                && info.exact_rbp < entry_sp
            {
                // See `innermost_frame_method`: after a JIT->JIT call this RBP
                // is the CALLEE's frame, which `info.compiled_method` does not
                // describe. Remap it with the method that DOES — the callee
                // resolved from the frame's own return address — and skip the
                // frame entirely when that resolution fails, because remapping
                // with another method's oop map is the exact corruption this
                // guard exists to prevent.
                //
                // Only the remap is skipped when resolution fails — the parent
                // chain walk below still runs, exactly as it did when this was a
                // `!chain_entry_rbp_is_foreign(..)` term in the condition above.
                if let Some(innermost_cm) = innermost_frame_method(
                    info.exact_rbp,
                    info.exact_cm_id,
                    entry_sp,
                    scanner_sp,
                    info.compiled_method,
                ) {
                    // SAFETY: the chain entry's pointer is kept alive by the JIT
                    // cache while the frame is active; a resolved callee is kept
                    // alive by the live frame whose return address resolved it.
                    let boundary_cm: &cratonvm_jit::CompiledMethod = unsafe { &*innermost_cm };
                    let (found, examined, n) = remap_one_jit_frame(
                        info.exact_rbp,
                        boundary_cm,
                        frame_map,
                        shadow_owned,
                        store_band,
                    );
                    if stale_frame_word_check_enabled() {
                        audit_stale_frame_words(info.exact_rbp, boundary_cm, pointer_map);
                    }
                    dbg_frames += 1;
                    dbg_slots.set(dbg_slots.get() + n);
                    if found {
                        dbg_maps_found.set(dbg_maps_found.get() + 1);
                    }
                    dbg_examined.set(dbg_examined.get() + examined);
                }
            }
            // Stage 5 — walk the JIT RBP chain from the innermost frame
            // (`info.frame_base`, the EXACT RBP recorded by the deepest
            // prologue's `set_top_frame_base`) outward to the interpreter
            // boundary, remapping EVERY ancestor frame.
            //
            // Recursive/nested JIT→JIT calls do not push `JIT_ENTRY_CHAIN`
            // entries (only the interpreter→JIT boundary does), so without this
            // walk only the innermost frame would be covered — the root cause
            // of the partial bt18 fix. JIT frames use `push rbp; mov rbp,rsp`,
            // so within `[scanner_sp, entry_sp)` the saved-RBP chain
            // (`[rbp]` = caller rbp, `[rbp+8]` = return address into the
            // caller) is walkable.
            //
            // `child_rbp`'s saved-rbp/return-address identify its PARENT, which
            // is the frame we remap each step. The innermost frame was already
            // remapped directly above because its child is a Rust helper rather
            // than a JIT return address.
            //
            // Start from `exact_rbp` (the precise innermost RBP from the
            // prologue), NOT `frame_base` (the Rust-guard SP). Skip if it was
            // never recorded (gate off / no precise prologue).
            if info.exact_rbp == 0 {
                continue;
            }
            let mut child_rbp = info.exact_rbp;
            // gch h1/jw (r17): every activation of the entry, not the first
            // 4096: a deeper compiled recursion kept pre-move addresses in the
            // oop slots of its outer activations. Under the `=0` legacy guard
            // a cut is reported; the coverage proof refuses the cycle on the
            // same cut, so no move relies on the frames left here.
            let mut walk = GcChainWalkGuard::for_entry(scanner_sp, entry_sp);
            loop {
                if !walk.step() {
                    if dbg {
                        eprintln!(
                            "[PRECISE] remap: parent walk cut after {} steps at rbp=0x{:x}",
                            walk.limit(),
                            child_rbp
                        );
                    }
                    break;
                }
                if child_rbp == 0 || child_rbp & 0x7 != 0 {
                    break;
                }
                if child_rbp < scanner_sp || child_rbp >= entry_sp {
                    break;
                }
                // SAFETY: `child_rbp` is an aligned address inside the calling
                // thread's own live JIT stack region (bounded by scanner_sp /
                // entry_sp). `[child_rbp]` is the saved caller RBP, `[child_rbp
                // + 8]` the return address into the caller.
                let parent_rbp = unsafe { (child_rbp as *const usize).read() };
                let ret_addr = unsafe { ((child_rbp + 8) as *const usize).read() };
                // A link read out of the stack is NOT yet a frame base. Both
                // helpers below dereference `[parent_rbp - sp_id_slot_off]`
                // (and `remap_one_jit_frame` WRITES every slot its oop map
                // names), so the link must be validated BEFORE it is used, not
                // after: a zero/garbage word faulted at `0 - sp_id_slot_off`
                // while walking a json-smart chain. Same order and same
                // predicate as the other rbp-chain walks in this file
                // (`scan_active_oop_map_at_rbp`'s walk and the two conservative
                // band walks).
                if parent_rbp <= child_rbp
                    || parent_rbp & 0x7 != 0
                    || parent_rbp < scanner_sp
                    || parent_rbp >= entry_sp
                {
                    break;
                }
                // Resolve the PARENT frame's CompiledMethod from the return
                // address that points into it.
                match cratonvm_jit::lookup_jit_code_range(ret_addr) {
                    Some(cm_ptr) => {
                        // SAFETY: the cm is Arc-owned by the JIT cache while any
                        // of its frames is live (a live frame keeps it cached);
                        // the registry is evicted before a cm is dropped.
                        let cm: &cratonvm_jit::CompiledMethod =
                            unsafe { &*(cm_ptr as *const cratonvm_jit::CompiledMethod) };
                        let (found, examined, n) = remap_one_jit_frame(
                            parent_rbp,
                            cm,
                            frame_map,
                            shadow_owned,
                            store_band,
                        );
                        if stale_frame_word_check_enabled() {
                            audit_stale_frame_words(parent_rbp, cm, pointer_map);
                        }
                        dbg_frames += 1;
                        dbg_slots.set(dbg_slots.get() + n);
                        if found {
                            dbg_maps_found.set(dbg_maps_found.get() + 1);
                        }
                        dbg_examined.set(dbg_examined.get() + examined);
                    }
                    None => break, // parent is the interpreter / Rust boundary
                }
                child_rbp = parent_rbp;
            }
        }
    });
    if dbg {
        eprintln!(
            "[PRECISE] remap: chain_entries={} precise={} frames_walked={} maps_found={} slots_examined={} slots_rewritten={} reg_size={}",
            dbg_entries,
            dbg_precise,
            dbg_frames,
            dbg_maps_found.get(),
            dbg_examined.get(),
            dbg_slots.get(),
            cratonvm_jit::jit_code_range_count(),
        );
    }
}

/// Stage 5 — rewrite one JIT frame's oop slots via `pointer_map`.
///
/// Reads the active safepoint's bytecode PC from `[rbp - sp_id_slot_off]`,
/// finds the matching [`cratonvm_jit::OopMapEntry`], and for each recorded
/// slot at `[rbp - off]` rewrites a relocated reference in place. No-op when
/// the method was compiled without the precise gate (`sp_id_slot_off == 0`).
/// Returns (map_found, slots_examined, slots_rewritten) — the extra counts are
/// for the CRATONVM_DBG_PRECISE diagnostic (distinguish "sp-id lookup miss"
/// from "mapped slots hold only pinned oops").
/// `CRATONVM_DBG_STALE_FRAME_WORDS=1` -- after a frame is remapped, report any
/// word still holding an address this collection MOVED.
///
/// This asks the failing question directly. `CRATONVM_DBG_VERIFY_OOP_MAPS` asks
/// "does the class file call this word a reference", and on the workloads that
/// crash it answers `verifier_oop=0` for all 2.8 M candidates -- it cannot see
/// the root. This asks "did we move the object this word points at and leave
/// the word pointing at the old address", which is the defect itself: a stale
/// reference in a live compiled frame is a use-after-free in its own right.
///
/// It was built for the SIGSEGV of the retired
/// `bug-box-unbox-intrinsic-segv-under-relocation-20260902`, on that page's
/// conclusion that the fault was such a reference being read. That conclusion
/// was RETRACTED on 2026-09-04: the fault was a WRITE, by `relocate_stw`'s own
/// `ptr::copy`, into an arena granule the give-back had decommitted -- see
/// `fixed-bugs/zgc-relocation-slides-wrote-into-decommitted-granules-FIXED-20260904`.
/// This detector is kept because what it FOUND is real and independent of that
/// crash (see `remap_unmapped_frame_dupes_enabled`), not because the crash is
/// still open.
///
/// A hit is only a candidate, not proof: a DEAD copy of a moved pointer left in
/// a spill slot is stale and harmless. What makes it actionable is the slot
/// CLASS -- `classify_frame_slot` names the three storage classes the coverage
/// model does not describe -- and the method label.
pub fn stale_frame_word_check_enabled() -> bool {
    static ON: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&ON, || false)
}

/// Scan `[rbp - frame_size, rbp]` for words that still name a moved object.
///
/// Called AFTER `remap_one_jit_frame` has rewritten every slot the maps name,
/// so anything left is by definition a slot no map named.
pub(in crate::jit::conservative_roots) fn audit_stale_frame_words(
    rbp: usize,
    cm: &cratonvm_jit::CompiledMethod,
    pointer_map: &cratonvm_types::PointerMap,
) {
    use std::sync::atomic::Ordering as AOrd;
    let frame_size = cm.osr_frame_size;
    if frame_size <= 0 {
        return;
    }
    let frame_size = frame_size as usize;
    const MAX_COMPILED_FRAME_BYTES: usize = 1024 * 1024;
    if frame_size > MAX_COMPILED_FRAME_BYTES || frame_size > rbp {
        return;
    }
    stale_frame_audit::FRAMES.fetch_add(1, AOrd::Relaxed);
    let lo = rbp - frame_size;
    let mut a = (lo + 7) & !7usize;
    // `< rbp`, not `<=`: `[rbp]` is the saved RBP, outside the `[lo, rbp)` band.
    while a < rbp {
        // SAFETY: inside a live compiled frame of THIS thread, bounded by the
        // same frame-size check every other walk in this file applies.
        let v = unsafe { (a as *const usize).read() };
        if let Some(&moved_to) = pointer_map.get(&v) {
            let off = (rbp - a) as i32;
            let class = classify_frame_slot(off, &cm.frame_layout);
            let unmodelled = matches!(
                class,
                "scalar-replacement-field"
                    | "licm-ref-hoist"
                    | "licm-arith-hoist"
                    | "gpr-safepoint-spill"
            );
            stale_frame_audit::STALE.fetch_add(1, AOrd::Relaxed);
            if unmodelled {
                stale_frame_audit::STALE_UNMODELLED.fetch_add(1, AOrd::Relaxed);
            }
            static LOGGED: AtomicUsize = AtomicUsize::new(0);
            if LOGGED.fetch_add(1, AOrd::Relaxed) < 40 {
                eprintln!(
                    "[STALE-FRAME-WORD] method={} slot=[rbp-{off:#x}] class={class} \
                     stale={v:#x} should_be={moved_to:#x} frame_size={frame_size} \
                     maps={} covered={} shadow_covered={}",
                    cm.method_label,
                    cm.oop_maps.len(),
                    cm.fully_oop_covered,
                    cm.fully_shadow_covered,
                );
            }
        }
        a += 8;
    }
}

/// gen r5w2/roots6: JIT-frame remap stores REFUSED because the target word
/// was not in the calling thread's live stack ABOVE the walk's own frame --
/// see [`jit_remap_store_band`]. Zero on a correct chain; a non-zero value is
/// a stale `JIT_ENTRY_CHAIN` entry (a cached `exact_rbp` or frame base that
/// outlived its frame) caught before it wrote. Closes
/// `docs/feature-designs/jit-ir-optimizations.md`.
pub static JIT_REMAP_REFUSED_OFF_STACK: AtomicUsize = AtomicUsize::new(0);

/// The words a JIT-frame remap walk may store into: `[scanner_sp, stack top)`
/// of the CALLING thread.
///
/// Every compiled frame a walk rewrites is a CALLER of the walk, so each of
/// its slots lies at or above the walk's own stack pointer (`scanner_sp`, taken
/// in the walk's frame) and below the thread's stack top. The three walks used
/// to accept any frame base in `[scanner_sp, entry_sp)` read out of the chain
/// and then write `[rbp - frame_size, rbp)`, with nothing checking that range
/// against the real stack: a stale `rbp` near `scanner_sp` put the low end of
/// that range into the walk's OWN frames below `scanner_sp`, and a stale
/// `entry_sp` could put the high end past the stack top. Both are refused
/// now. (A stale `rbp` inside the live Rust frames between the helper and the
/// walk is NOT caught by this bound -- those frames are legitimately above
/// `scanner_sp`; it stays value-gated, as before, and every such store is
/// reported as `place=own-live` by the audit.) Bounding the STORE, not the
/// walk, keeps every read and every count unchanged and removes only stores a
/// correct chain never makes. The top is `SlotBand::current_thread`'s, i.e.
/// clamped below the static TLS block on Linux.
#[inline(never)]
pub(in crate::jit::conservative_roots) fn jit_remap_store_band(scanner_sp: usize) -> cratonvm_gc::shadow_stack::SlotBand {
    let own = cratonvm_gc::shadow_stack::SlotBand::current_thread();
    cratonvm_gc::shadow_stack::SlotBand::new(own.lo.max(scanner_sp & !7usize), own.hi)
}

/// One raw JIT-frame remap store of `new` over `old` at `slot`, made only when
/// the slot lies in `band` ([`jit_remap_store_band`]). Every store and every
/// refusal is reported to `CRATONVM_DBG=root-write-audit` under `kind`.
/// Returns whether it stored.
#[inline]
pub(in crate::jit::conservative_roots) fn store_jit_remap_word(
    kind: cratonvm_gc::root_write_audit::RootWriteKind,
    band: cratonvm_gc::shadow_stack::SlotBand,
    slot: usize,
    old: usize,
    new: usize,
) -> bool {
    let audit = cratonvm_gc::root_write_audit::enabled();
    if !band.contains_slot(slot) {
        JIT_REMAP_REFUSED_OFF_STACK.fetch_add(1, Ordering::Relaxed);
        if audit {
            cratonvm_gc::root_write_audit::note_write(kind, band, slot, old, new, false);
        }
        return false;
    }
    if audit {
        cratonvm_gc::root_write_audit::note_write(kind, band, slot, old, new, true);
    }
    // SAFETY: `slot` is an aligned word (every caller checks `& 0x7`) inside
    // `[scanner_sp, stack top)` of the calling thread, i.e. a live frame of a
    // caller of the walk, and this code runs ON that thread.
    unsafe { (slot as *mut usize).write(new) };
    true
}

pub(in crate::jit::conservative_roots) fn remap_one_jit_frame(
    rbp: usize,
    cm: &cratonvm_jit::CompiledMethod,
    pointer_map: &cratonvm_types::PointerMap,
    shadow_owned: &[usize],
    band: cratonvm_gc::shadow_stack::SlotBand,
) -> (bool, usize, usize) {
    // No slot of such a frame holds a reference, so there is nothing to
    // rewrite -- and its safepoint-id slot may be stale by contract, so it must
    // not select a map (`CompiledMethod::frame_holds_no_references`). The
    // callee-saved GPR image is `remap_register_image_words`' job, unchanged.
    if cm.frame_holds_no_references {
        return (true, 0, 0);
    }
    let sp_id_off = cm.sp_id_slot_off;
    if sp_id_off == 0 {
        if remap_residue_dbg() {
            eprintln!(
                "[remap-frame] method={} NO_SP_ID_SLOT inlined={:?}",
                cm.method_label,
                cm.inlined_methods
                    .iter()
                    .map(|(c, m, d)| format!("{c}.{m}{d}"))
                    .collect::<Vec<_>>()
            );
        }
        return (false, 0, 0);
    }
    let id_addr = rbp.wrapping_sub(sp_id_off as usize);
    if id_addr & 0x7 != 0 {
        return (false, 0, 0);
    }
    // SAFETY: aligned frame slot of a live JIT frame on this thread.
    let sp_id = (unsafe { (id_addr as *const usize).read() }) as u32;
    // EVERY map recorded at this bytecode pc, not the first one.
    //
    // `bytecode_pc` identifies the SAFEPOINT'S BCI, and one bci can carry more
    // than one safepoint: a call-carrying inline splice emits a real call for
    // every `invoke*` in the spliced body while `cur_bc_pc` stays pinned to the
    // enclosing invoke's bci, so `AbstractLongAssert.<init>` spliced into
    // `LongAssert.<init>` produces two maps under one id with different live
    // sets. `find` took whichever was recorded first and silently left the
    // other safepoint's slots unrewritten.
    //
    // The union is the fail-safe direction and the direction every other reader
    // of this table already takes (`moving_young_frame_coverage_complete_at`
    // and the band verifier both `filter`). Rewriting a slot that is not live
    // at this particular safepoint can only replace a word that already equals
    // a from-space base with that object's new base -- which is what the
    // conservative sweep this path replaces did to every such word in the
    // frame.
    // A slot the shadow stack publishes BY ADDRESS (an IR frame block, or a
    // single-pass frame home published for the call in flight) is rewritten
    // through that entry by `ShadowStack::remap`, which every caller runs after
    // this walk. Rewriting it here as well would take the already-moved address
    // as a fresh key, which is wrong exactly when a collector can move one
    // object onto another's old address. So the entry owns the word and this
    // walk only counts it. "Published" is what is on the shadow stack now, not
    // what the method could publish: an IR block that has not been pushed yet,
    // or whose push overflowed, names no word, and this walk stays its rewriter.
    let mut examined = 0usize;
    let mut rewritten = 0usize;
    let mut found = false;
    // gch h1/jw (review): every slot done, not the first 64. Past a fixed
    // 64-entry array the old loop rewrote a slot named by two maps TWICE, "safe
    // because the second lookup of a to-space address misses" -- which does
    // not hold for a collector that moves one object onto another's old
    // address (a sliding or in-place copy: the hazard the `shadow_owned`
    // paragraph above names), where the second lookup re-forwards the word to
    // the OTHER object's new address. Inline up to 64, so the common frame
    // still allocates nothing.
    let mut seen: smallvec::SmallVec<[i16; 64]> = smallvec::SmallVec::new();
    let mut coverage_complete = true;
    for map in cm.oop_maps.iter().filter(|m| m.bytecode_pc == sp_id) {
        found = true;
        coverage_complete &= map.moving_young_coverage_complete;
        for &off in &map.frame_slot_offsets {
            // Two maps at one bci overlap heavily (`this` is live at both), and
            // rewriting a slot twice would take the already-moved value as a
            // fresh key. The map is small, so a linear scan over what has been
            // done is cheaper than a set.
            if seen.contains(&off) {
                continue;
            }
            seen.push(off);
            examined += 1;
            // Slots are positive offsets; the value lives at `[rbp - off]`.
            let slot_addr = rbp.wrapping_sub(off as usize);
            if slot_addr & 0x7 != 0 {
                continue;
            }
            if shadow_owned.binary_search(&slot_addr).is_ok() {
                continue;
            }
            // SAFETY: aligned frame slot of a live JIT frame on this thread.
            let old = unsafe { (slot_addr as *const usize).read() };
            if let Some(&new) = pointer_map.get(&old) {
                // gen r5w2/roots6: stored only inside the walk's own live
                // stack band (`jit_remap_store_band`).
                if store_jit_remap_word(
                    cratonvm_gc::root_write_audit::RootWriteKind::JitFrameSlot,
                    band,
                    slot_addr,
                    old,
                    new,
                ) {
                    rewritten += 1;
                }
            }
        }
    }
    if !found {
        if remap_residue_dbg() {
            eprintln!(
                "[remap-frame] method={} sp_id={} NO_MAP_FOR_SP_ID maps={}",
                cm.method_label,
                sp_id,
                cm.oop_maps.len()
            );
        }
        return (false, 0, 0);
    }
    if remap_residue_dbg() {
        report_remap_residue(
            rbp,
            cm,
            pointer_map,
            sp_id,
            &seen[..],
            rewritten,
            coverage_complete,
        );
    }
    if remap_unmapped_dupes_enabled() {
        remap_unmapped_frame_dupes(rbp, cm, pointer_map, &seen[..], band);
    }
    (true, examined, rewritten)
}

/// `CRATONVM_JIT_REMAP_UNMAPPED_DUPES=1` -- after the precise remap, rewrite any
/// word in the frame that STILL holds an address this collection moved.
///
/// The defect, measured 2026-09-03 with `CRATONVM_DBG_STALE_FRAME_WORDS=1`: the
/// register allocator keeps a reference in several frame slots at once and the
/// oop map names only the canonical home. `TestCachedQueryResults.queryCounter`
/// held one object at four slots -- a GPR safepoint spill, an operand spill and
/// two below the locals boundary -- and the remap rewrote one. The rest keep
/// pointing at the old address. Confirmed at scale:
/// `duplicate_of_mapped=47946355`.
///
/// That is a use-after-free waiting to be read, and it is NOT the SIGSEGV it
/// was found while hunting: the retired
/// `bug-box-unbox-intrinsic-segv-under-relocation-20260902` turned out to be a
/// decommitted-granule WRITE inside `relocate_stw` itself. Its own numbers
/// said so before the retraction did -- rewriting these duplicates left the
/// crash at 3 of 4. Real, enormous, and a different defect.
///
/// PINNING those objects was tried first and does not work: pins only withhold
/// PAGES in the low compaction region (`relocate_stw`'s
/// `addr >= base && addr < low_end`), so an object the general slide moves is
/// unprotected. 2 of 3 runs still SIGSEGV with 96 M pins published.
///
/// So rewrite instead. The conservative-write hazard is real but narrow, and it
/// is the hazard this path's PREDECESSOR already accepted -- see
/// `remap_one_jit_frame`'s own comment: the sweep it replaced "did [this] to
/// every such word in the frame". A word is only rewritten when its value is a
/// KEY of the pointer map, i.e. exactly the base of an object this collection
/// actually moved; a primitive holding that precise bit pattern is possible and
/// is the residual risk, recorded rather than hidden.
pub fn remap_unmapped_dupes_enabled() -> bool {
    static ON: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&ON, || false)
}

pub(in crate::jit::conservative_roots) fn remap_unmapped_frame_dupes(
    rbp: usize,
    cm: &cratonvm_jit::CompiledMethod,
    pointer_map: &cratonvm_types::PointerMap,
    named: &[i16],
    band: cratonvm_gc::shadow_stack::SlotBand,
) {
    use std::sync::atomic::Ordering as AOrd;
    let frame_size = cm.osr_frame_size;
    if frame_size <= 0 {
        return;
    }
    let frame_size = frame_size as usize;
    const MAX_COMPILED_FRAME_BYTES: usize = 1024 * 1024;
    if frame_size > MAX_COMPILED_FRAME_BYTES || frame_size > rbp {
        return;
    }
    unmapped_dupe_remap::FRAMES.fetch_add(1, AOrd::Relaxed);
    // JIT round 25 (item 3): a claimed `Prim` colour holds a primitive or
    // residue, and rewriting a primitive whose bits equal a moved base is the
    // one corruption this pass admits; a body that carries the claim is spared.
    let prim_claim = ir_prim_colour_claim_on_chain(cm);
    let lo = rbp - frame_size;
    let mut a = (lo + 7) & !7usize;
    // `< rbp`, not `<=`: `[rbp]` is the saved RBP, outside the `[lo, rbp)` band.
    while a < rbp {
        let off = (rbp - a) as i64;
        // Slots the precise loop already handled are skipped: it rewrote them,
        // so their value is now a to-space address, and a second lookup would
        // miss anyway -- but skipping keeps the intent explicit.
        let is_named = off > 0 && off <= i16::MAX as i64 && named.contains(&(off as i16));
        if !is_named && !ir_prim_colour_claimed(prim_claim, off) {
            // SAFETY: aligned word inside this thread's live compiled frame,
            // bounded by the frame size recorded at compile time.
            let v = unsafe { (a as *const usize).read() };
            if let Some(&new) = pointer_map.get(&v) {
                // gen r5w2/roots6: same aligned in-frame slot, rewriting a
                // reference to an object this collection relocated -- stored
                // only inside the walk's own live stack band.
                if store_jit_remap_word(
                    cratonvm_gc::root_write_audit::RootWriteKind::JitFrameDuplicate,
                    band,
                    a,
                    v,
                    new,
                ) {
                    unmapped_dupe_remap::REWRITTEN.fetch_add(1, AOrd::Relaxed);                }            }        }
        a += 8;
    }
}

/// `CRATONVM_DBG=remap-residue` -- after a frame's oop map has been applied,
/// walk the WHOLE frame band and report any aligned word that is still a KEY of
/// the pointer map, i.e. still names a from-space address the slide moved.
///
/// This is the direct instrument for "which frame word did the oop map fail to
/// name". The map-driven rewrite has already run when this fires, so every hit
/// is a word the moving cycle left pointing at vacated memory; it prints the
/// method, the offset from RBP, the stale value and where the object went, plus
/// the frame's mapped slots and its coverage claim.
///
/// A hit is EVIDENCE, not a verdict: a compiled frame's band also holds dead
/// spill residue, and a stale word nothing will read harms nobody. What makes
/// it decisive is the method name beside it -- a spliced constructor listed here
/// while its `this` is live is the shape of
/// `known-issues/netty/longlonghashmaptest-nullpointerexception...`, and it is
/// how that page was root-caused.
///
/// # The `oracle=[...]` field is the part that can say "live"
///
/// `stale_live` (below `live_frame_hi`) is an UPPER BOUND and was twice read as
/// a verdict. The watermark is where the operand-spill cursor stood, not a
/// liveness bound, so a slot holding bytes some earlier expression left there
/// counts as LIVE. Each such word is therefore put to the compiler's own
/// forward "must be oop" dataflow, carried on the map as
/// `OopMapEntry::local_oop_mask`:
///
/// * `local_oop` -- the dataflow PROVES this local holds a reference and the
///   map did not name it. This is the only count that is a missed root, and it
///   should be structurally impossible (every mask bit is pushed into the slot
///   list); a non-zero here is the finding, and is always printed.
/// * `local_not_oop` -- the dataflow proves the slot is not a reference at this
///   bci. Dead storage. This is what the `String.substring` witness was.
/// * `local_unreached` -- the dataflow never reached this bci, so the map named
///   NO locals and nothing can be attributed. Suspicious but unproven; paired
///   with `map_incomplete_cause::LOCAL_MASK_UNREACHED`.
/// * `inline_local_oop` / `inline_local_not_oop` -- the same two answers for a
///   local of an inlined callee, from that splice's own mask
///   (`OopMapEntry::inline_local_scopes`). Spliced locals live in the SPILL
///   band, so without the scopes every one of them read as `outside_locals`,
///   and "a spliced callee's locals are named by no oop map" is a defect this
///   repo has already paid for once.
/// * `stack_not_oop` -- the safepoint's own operand-stack model classified that
///   spill slot as a non-reference and its marks were exact. Dead storage.
///   This is the band the H2 residue is actually in: 36 of 37 stale words on
///   `TestRandomMapOps` were `region=operand-spill`.
/// * `mapped_alias` -- the slot IS named by the map and was rewritten; the value
///   the rewrite wrote is itself a pointer-map key, because a slide moves
///   objects into space other objects vacated. This is why "the word is a key
///   of the pointer map" is not proof that the word is stale, and it produced
///   this instrument's first and only `LOCAL-OOP-UNMAPPED` before it was
///   separated out.
/// * `duplicate_of_mapped` -- no oracle claims the slot, but the object the word
///   points at is ALSO named by a mapped slot of the same frame, which
///   relocation rewrote. The object is not lost. Measured on H2: the map names
///   a group of spill slots and the stale words are the copies three slots
///   below them, holding the same objects at their pre-move addresses.
/// * `outside_locals` -- spill or staging band with no scope claiming it, where
///   this oracle is silent. The per-frame detail prints
///   `FrameLayout::region_name` beside each, which is what makes an
///   `outside_locals` word actionable.
///
/// Only the single-pass x86-64 tier homes local `k` at `[rbp - 8*(k+1)]`, so IR
/// frames record no mask and no scopes. They are not silent, though: the IR
/// lowerer's slot COLOURING publishes every provably-primitive spill slot as
/// `non_oop_stack_slots`, so `stack_not_oop` is the verdict that speaks for
/// them.
pub(in crate::jit::conservative_roots) fn remap_residue_dbg() -> bool {
    static ON: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&ON, || false)}

/// The stale-word oracle's run totals, on `CRATONVM_DBG=remap-residue`.
///
/// `local_oop` is the only count that names a missed root: a frame word the
/// compiler's own "must be oop" dataflow proves holds a reference, below the
/// live watermark, that the safepoint's map did not name. The other three are
/// the reasons the raw `stale_live` count overstates — dead storage, an
/// unreached bci, and the spill band this oracle cannot speak for.
///
/// `frames` is the ENGAGEMENT counter: a zero `local_oop` beside a zero
/// `frames` says the instrument never ran, which is not the same reading as a
/// zero beside thousands of frames.
pub fn report_remap_residue_census_at_exit() {
    use std::sync::atomic::Ordering::Relaxed;
    let frames = residue_census::FRAMES.load(Relaxed);
    if frames == 0 {
        return;
    }
    eprintln!(
        "[remap-residue-summary] frames={} frames_with_live_stale={} local_oop={} local_not_oop={} local_unreached={} frames_with_inline_scopes={} inline_scopes={} inline_local_oop={} inline_local_not_oop={} frames_with_stack_model={} stack_not_oop={} duplicate_of_mapped={} mapped_alias={} outside_locals={}",
        frames,
        residue_census::FRAMES_WITH_LIVE.load(Relaxed),
        residue_census::LOCAL_OOP.load(Relaxed),
        residue_census::LOCAL_NOT_OOP.load(Relaxed),
        residue_census::LOCAL_UNREACHED.load(Relaxed),
        residue_census::FRAMES_WITH_INLINE_SCOPES.load(Relaxed),
        residue_census::INLINE_SCOPES_SEEN.load(Relaxed),
        residue_census::INLINE_LOCAL_OOP.load(Relaxed),
        residue_census::INLINE_LOCAL_NOT_OOP.load(Relaxed),
        residue_census::FRAMES_WITH_STACK_MODEL.load(Relaxed),
        residue_census::STACK_NOT_OOP.load(Relaxed),
        residue_census::DUPLICATE_OF_MAPPED.load(Relaxed),
        residue_census::MAPPED_ALIAS.load(Relaxed),
        residue_census::OUTSIDE_LOCALS.load(Relaxed),
    );
}

/// What the local-oop dataflow says about a stale frame word that
/// `live_frame_hi` classified as LIVE. See `report_remap_residue`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::jit::conservative_roots) enum StaleVerdict {
    /// Above the live watermark, or the watermark is unknown — not asked.
    NotLive,
    /// The dataflow PROVES this local holds a reference and the map did not
    /// name it. The only verdict that is a missed root.
    LocalOopUnmapped,
    /// The dataflow proves the slot is not a reference at this bci: dead
    /// storage the spill cursor abandoned without clearing.
    LocalNotOop,
    /// The dataflow never reached this bci, so the map named no locals and
    /// nothing can be attributed. Unproven either way.
    LocalUnreached,
    /// As `LocalOopUnmapped`, for a local of an inlined callee.
    InlineLocalOopUnmapped,
    /// As `LocalNotOop`, for a local of an inlined callee.
    InlineLocalNotOop,
    /// The safepoint's own operand-stack model classified this spill slot as
    /// NOT holding a reference, and the mark vector was exact. Dead storage.
    StackNotOop,
    /// No oracle claims the slot, but the object it points at IS named by
    /// another slot of this same frame, which relocation rewrote. The object is
    /// therefore not lost; this word is a stale DUPLICATE of a live root.
    DuplicateOfMapped,
    /// The slot IS in this safepoint's map and was rewritten, and the value the
    /// rewrite wrote is ITSELF a key of the pointer map -- a to-space address
    /// that aliases some other object's from-space address, because a slide
    /// moves objects into space other objects vacated.
    ///
    /// Never a missed root, and the reason "this word is a key of the pointer
    /// map" is not by itself proof that a word is stale. Observed on
    /// `FileStore.readChunkFooter` (`mapped=[8=..]`, `rewritten=4` of 4 slots),
    /// where it produced the instrument's first and only LOCAL-OOP-UNMAPPED.
    MappedAlias,
    /// Spill or staging band — not a local home, so this oracle is silent.
    OutsideLocals,
}

impl StaleVerdict {
    pub(in crate::jit::conservative_roots) fn as_str(self) -> &'static str {
        match self {
            StaleVerdict::NotLive => "",
            StaleVerdict::LocalOopUnmapped => "LOCAL-OOP-UNMAPPED",
            StaleVerdict::LocalNotOop => "local-not-oop",
            StaleVerdict::LocalUnreached => "local-unreached",
            StaleVerdict::InlineLocalOopUnmapped => "INLINE-LOCAL-OOP-UNMAPPED",
            StaleVerdict::InlineLocalNotOop => "inline-local-not-oop",
            StaleVerdict::StackNotOop => "stack-not-oop",
            StaleVerdict::DuplicateOfMapped => "duplicate-of-mapped",
            StaleVerdict::MappedAlias => "mapped-alias",
            StaleVerdict::OutsideLocals => "outside-locals",
        }
    }
}

/// Put one stale frame word to the compiler's forward "must be oop" dataflow.
///
/// `off` is the positive distance below RBP of the word; the single-pass x86-64
/// emitter homes JVM local `k` at `[rbp - 8*(k+1)]` (`Compiler::local_offset`),
/// so the offset names a local exactly. `local_mask` is the safepoint's
/// `OopMapEntry::local_oop_mask` — `None` meaning the dataflow never reached
/// this bci, which is NOT the same as `Some(0)`.
pub(in crate::jit::conservative_roots) fn classify_stale_local(
    off: usize,
    mapped: &[i16],
    local_mask: Option<u64>,
    num_locals: usize,
    inline_scopes: &[(i32, u16, u64)],
    non_oop_stack: &[i16],
) -> StaleVerdict {
    // FIRST, because a slot the map NAMED was rewritten and cannot be a missed
    // root. Its current value being a pointer-map key is address aliasing -- a
    // slide moves objects into space other objects vacated -- and asking the
    // locals oracle about it is how this instrument produced its only false
    // `LOCAL-OOP-UNMAPPED`. See `StaleVerdict::MappedAlias`.
    if i16::try_from(off).is_ok_and(|o| mapped.contains(&o)) {
        return StaleVerdict::MappedAlias;
    }
    if off < 8 || off % 8 != 0 || (off / 8 - 1) >= num_locals {
        // Not a java local of this method. A SPLICED callee's locals are
        // allocated out of the operand-spill band and are addressed from their
        // scope's own base, so ask each live scope before giving up.
        return match classify_stale_inline_local(off, inline_scopes) {
            // No splice claims it. The last oracle is the safepoint's own
            // operand-stack model, which is where the H2 residue actually sits.
            StaleVerdict::OutsideLocals => {
                if i16::try_from(off).is_ok_and(|o| non_oop_stack.contains(&o)) {
                    StaleVerdict::StackNotOop
                } else {
                    StaleVerdict::OutsideLocals
                }
            }
            other => other,
        };
    }
    let k = off / 8 - 1;
    match local_mask {
        None => StaleVerdict::LocalUnreached,
        // A mask is 64 bits wide and cannot speak past that. Unreachable today
        // (`compute_local_oop_masks` returns nothing at all above 64 locals, so
        // the entry records `None`), and it must stay UNPROVEN rather than
        // falling into the "proved not a reference" arm below if that changes.
        Some(_) if k >= 64 => StaleVerdict::LocalUnreached,
        Some(m) if m & (1u64 << k) != 0 => StaleVerdict::LocalOopUnmapped,
        Some(_) => StaleVerdict::LocalNotOop,
    }
}

/// The same question for the locals of an inlined callee. Spliced callee local
/// `k` of a scope based at `base` is homed at `[rbp - (base + 8*k)]`
/// (`x64::inlining`), so a word inside a scope's band names one of its locals
/// exactly and the scope's own mask answers for it.
pub(in crate::jit::conservative_roots) fn classify_stale_inline_local(off: usize, inline_scopes: &[(i32, u16, u64)]) -> StaleVerdict {
    let Ok(off_i) = i32::try_from(off) else {
        return StaleVerdict::OutsideLocals;
    };
    for &(base, n, mask) in inline_scopes {
        if base <= 0 || n == 0 || off_i < base {
            continue;
        }
        let delta = off_i - base;
        if delta % 8 != 0 {
            continue;
        }
        let k = (delta / 8) as usize;
        if k >= n as usize {
            continue;
        }
        // Same 64-bit limit as the outer mask: past it the scope says nothing.
        if k >= 64 {
            return StaleVerdict::LocalUnreached;
        }
        return if mask & (1u64 << k) != 0 {
            StaleVerdict::InlineLocalOopUnmapped
        } else {
            StaleVerdict::InlineLocalNotOop
        };
    }
    StaleVerdict::OutsideLocals
}

pub(in crate::jit::conservative_roots) fn report_remap_residue(
    rbp: usize,
    cm: &cratonvm_jit::CompiledMethod,
    pointer_map: &cratonvm_types::PointerMap,
    sp_id: u32,
    mapped: &[i16],
    rewritten: usize,
    coverage_complete: bool,
) {
    let frame_size = cm.osr_frame_size;
    let mut hits = 0usize;
    // A raw `stale_words` count is an UPPER BOUND and cannot be acted on. The
    // spill cursor "reclaims by moving, it does not clear" (see
    // `OopMapEntry::live_frame_hi`), so a word above the live bound still holds
    // whatever reference last occupied it — a from-space address there is DEAD
    // and rewriting it would be pointless, not a missed root. Only a stale word
    // BELOW the bound is a live reference the map failed to name, and only that
    // number says whether `moving_young_coverage_complete` is lying.
    //
    // `live_frame_hi == 0` means "unknown" (the same sentinel the band verifier
    // reads), so those are counted separately rather than being silently folded
    // into either answer.
    let live_hi = cm
        .oop_maps
        .iter()
        .filter(|m| m.bytecode_pc == sp_id)
        .map(|m| m.live_frame_hi)
        .max()
        .unwrap_or(0);
    // THE ORACLE. `live_hi` is a spill watermark, so "below it and unmapped" is
    // an upper bound on missed roots and cannot be acted on — it counts dead
    // spill residue as LIVE, which is how a correct map was read as short (see
    // the page cited on `OopMapEntry::local_oop_mask`). The forward "must be
    // oop" dataflow answers the question the watermark cannot: for a word in
    // the LOCALS band, is that slot a reference at this bci?
    //
    // A `None` from ANY map at this sp-id poisons the answer for the safepoint:
    // that map named no locals at all, so nothing here can be attributed to a
    // slot the dataflow classified. Otherwise take the union — two maps at one
    // bci both describe this frame, and a slot either names a reference or does
    // not.
    let mut local_mask: Option<u64> = Some(0);
    let mut num_locals: usize = 0;
    let mut inline_scopes: Vec<(i32, u16, u64)> = Vec::new();
    let mut non_oop_stack: Vec<i16> = Vec::new();
    for m in cm.oop_maps.iter().filter(|m| m.bytecode_pc == sp_id) {
        num_locals = num_locals.max(m.num_locals as usize);
        inline_scopes.extend_from_slice(&m.inline_local_scopes);
        // A non-exact mark vector was PADDED with "not an oop" for entries
        // nobody classified, so its complement is a default and not a proof.
        // Spend it only when the safepoint said it was exact.
        if m.stack_marks_exact {
            non_oop_stack.extend_from_slice(&m.non_oop_stack_slots);
        }
        local_mask = match (local_mask, m.local_oop_mask) {
            (Some(a), Some(b)) => Some(a | b),
            _ => None,
        };
    }
    let mut stale_live = 0usize;
    let mut stale_dead = 0usize;
    let mut stale_unknown = 0usize;
    // The four verdicts the oracle can return about a word BELOW the watermark.
    // Only the first is a missed root; the other three are the reasons a raw
    // `stale_live` count overstates.
    let mut oracle_local_oop = 0usize;
    let mut oracle_local_not_oop = 0usize;
    let mut oracle_local_unreached = 0usize;
    let mut oracle_inline_local_oop = 0usize;
    let mut oracle_inline_local_not_oop = 0usize;
    let mut oracle_stack_not_oop = 0usize;
    let mut oracle_duplicate = 0usize;
    let mut oracle_mapped_alias = 0usize;
    let mut oracle_outside_locals = 0usize;
    let mut detail = String::new();
    // The values the map's own slots hold NOW, i.e. after relocation rewrote
    // them. A stale word whose target is among these points at an object the
    // frame still reaches through a named root: the object is not lost, and the
    // word is a stale DUPLICATE rather than a missing root. Cheap, and it needs
    // no metadata the map does not already carry.
    let mut mapped_vals: Vec<usize> = Vec::new();
    for &off in mapped {
        let a = rbp.wrapping_sub(off as usize);
        // One entry per mapped slot, in order, so the description below can
        // index it; a misaligned slot (never seen) contributes 0, which is not
        // a pointer-map key and so cannot produce a false duplicate.
        mapped_vals.push(if a & 0x7 == 0 {
            // SAFETY: aligned frame slot of a live JIT frame on this thread.
            unsafe { (a as *const usize).read() }
        } else {
            0
        });
    }
    if frame_size > 0 && (frame_size as usize) <= 1024 * 1024 && (frame_size as usize) <= rbp {
        let frame_size = frame_size as usize;
        let lo = (rbp - frame_size + 7) & !7usize;
        let mut addr = lo;
        while addr + 8 <= rbp {
            // SAFETY: aligned word inside this thread's own live JIT frame band.
            let w = unsafe { (addr as *const usize).read() };
            if let Some(&new) = pointer_map.get(&w) {
                hits += 1;
                let off = rbp - addr;
                let class = if live_hi <= 0 {
                    stale_unknown += 1;
                    "unknown"
                } else if (off as i64) < live_hi as i64 {
                    stale_live += 1;
                    "LIVE"
                } else {
                    stale_dead += 1;
                    "dead"
                };
                // Ask the oracle about the words the watermark called LIVE.
                // Local `k` is homed at `[rbp - 8*(k+1)]` (the emitter's
                // `local_offset`), so an offset maps back to a local index
                // exactly; anything outside that band is spill or staging,
                // about which this oracle says nothing.
                let verdict = if class != "LIVE" {
                    StaleVerdict::NotLive
                } else {
                    classify_stale_local(
                        off,
                        mapped,
                        local_mask,
                        num_locals,
                        &inline_scopes,
                        &non_oop_stack,
                    )
                };
                // Last resort before "unexplained": is this the same object a
                // named slot of this frame already points at?
                let verdict =
                    if verdict == StaleVerdict::OutsideLocals && mapped_vals.contains(&new) {
                        StaleVerdict::DuplicateOfMapped
                    } else {
                        verdict
                    };
                match verdict {
                    StaleVerdict::NotLive => {}
                    StaleVerdict::LocalOopUnmapped => oracle_local_oop += 1,
                    StaleVerdict::LocalNotOop => oracle_local_not_oop += 1,
                    StaleVerdict::LocalUnreached => oracle_local_unreached += 1,
                    StaleVerdict::InlineLocalOopUnmapped => oracle_inline_local_oop += 1,
                    StaleVerdict::InlineLocalNotOop => oracle_inline_local_not_oop += 1,
                    StaleVerdict::StackNotOop => oracle_stack_not_oop += 1,
                    StaleVerdict::DuplicateOfMapped => oracle_duplicate += 1,
                    StaleVerdict::MappedAlias => oracle_mapped_alias += 1,
                    StaleVerdict::OutsideLocals => oracle_outside_locals += 1,
                }
                // A word the oracle proves is a reference is the finding, and
                // it is rare enough to always print. The rest share a budget.
                if matches!(
                    verdict,
                    StaleVerdict::LocalOopUnmapped | StaleVerdict::InlineLocalOopUnmapped
                ) || (stale_live <= 12 && class == "LIVE")
                {
                    detail.push_str(&format!(
                        " [LIVE off={} k={} {} region={} stale=0x{:x}->0x{:x}]",
                        off,
                        if off >= 8 && off % 8 == 0 {
                            (off / 8 - 1) as i64
                        } else {
                            -1
                        },
                        verdict.as_str(),
                        // Which band of the frame the word sits in. Free, from
                        // the layout the method already carries, and it is what
                        // makes an `outside-locals` verdict actionable: an
                        // operand spill slot, a callee-saved register image and
                        // an outgoing-argument word are three different
                        // questions, and the oracle above cannot tell them
                        // apart.
                        cm.frame_layout
                            .region_name(i32::try_from(off).unwrap_or(i32::MAX)),
                        w,
                        new
                    ));
                }
            }
            addr += 8;
        }
    }
    {
        use std::sync::atomic::Ordering::Relaxed;
        residue_census::FRAMES.fetch_add(1, Relaxed);
        if stale_live > 0 {
            residue_census::FRAMES_WITH_LIVE.fetch_add(1, Relaxed);
        }
        residue_census::LOCAL_OOP.fetch_add(oracle_local_oop, Relaxed);
        residue_census::LOCAL_NOT_OOP.fetch_add(oracle_local_not_oop, Relaxed);
        residue_census::LOCAL_UNREACHED.fetch_add(oracle_local_unreached, Relaxed);
        if !inline_scopes.is_empty() {
            residue_census::FRAMES_WITH_INLINE_SCOPES.fetch_add(1, Relaxed);
            residue_census::INLINE_SCOPES_SEEN.fetch_add(inline_scopes.len(), Relaxed);
        }
        residue_census::INLINE_LOCAL_OOP.fetch_add(oracle_inline_local_oop, Relaxed);
        residue_census::INLINE_LOCAL_NOT_OOP.fetch_add(oracle_inline_local_not_oop, Relaxed);
        residue_census::STACK_NOT_OOP.fetch_add(oracle_stack_not_oop, Relaxed);
        residue_census::DUPLICATE_OF_MAPPED.fetch_add(oracle_duplicate, Relaxed);
        residue_census::MAPPED_ALIAS.fetch_add(oracle_mapped_alias, Relaxed);
        if !non_oop_stack.is_empty() {
            residue_census::FRAMES_WITH_STACK_MODEL.fetch_add(1, Relaxed);
        }
        residue_census::OUTSIDE_LOCALS.fetch_add(oracle_outside_locals, Relaxed);
    }
    let mut mapped_desc = String::new();
    for (i, &off) in mapped.iter().enumerate() {
        mapped_desc.push_str(&format!(
            " {}=0x{:x}",
            off,
            mapped_vals.get(i).copied().unwrap_or(0)
        ));
    }
    eprintln!(
        "[remap-frame] method={} sp_id={} frame_size={} cov_complete={} live_hi={} local_mask={:?} num_locals={} mapped=[{}] rewritten={} inlined={:?} stale_words={} stale_live={} stale_dead={} stale_unknown={} scopes={:?} oracle=[local_oop={} local_not_oop={} local_unreached={} inline_local_oop={} inline_local_not_oop={} stack_not_oop={} duplicate_of_mapped={} mapped_alias={} outside_locals={}]{}",
        cm.method_label,
        sp_id,
        frame_size,
        coverage_complete,
        live_hi,
        local_mask,
        num_locals,
        mapped_desc,
        rewritten,
        cm.inlined_methods
            .iter()
            .map(|(c, m, d)| format!("{c}.{m}{d}"))
            .collect::<Vec<_>>(),
        hits,
        stale_live,
        stale_dead,
        stale_unknown,
        inline_scopes,
        oracle_local_oop,
        oracle_local_not_oop,
        oracle_local_unreached,
        oracle_inline_local_oop,
        oracle_inline_local_not_oop,
        oracle_stack_not_oop,
        oracle_duplicate,
        oracle_mapped_alias,
        oracle_outside_locals,
        detail,
    );
}

// NEW-12: enumerate exact oops in a JIT frame using the compiled
// method's precise oop map.
//
// The current PC in the active frame is obtained by subtracting the
// compiled method's entry pointer from the current return-address at
// `frame_base - 8` (the standard x86-64 calling convention stores the
// return PC one word below RBP when RBP has been spilled; for
// currently-executing frames with RBP = entry SP we take the next
// word up as a safe over-approximation and accept that some
// safepoints may fall through to the conservative fallback).
//
// For correctness-at-any-PC coverage, if no exact map match is found
// we fall through to the conservative scan of the frame region. This
// keeps the walker functional even when the compiler has only
// populated oop maps at a subset of safepoints — a realistic state
// during the staged rollout described in `docs/feature-designs/jit-ir-optimizations.md` NEW-12.
// ---------------------------------------------------------------------------
// Step 3 (docs/feature-designs/precise-jit-maps-default.md) — coverage
// visibility + a completeness oracle. Both gates are default-OFF and read-only,
// so the default GC scan is unchanged (two cached-bool branches per frame).
// ---------------------------------------------------------------------------

/// `CRATONVM_DBG_VERIFY_OOP_MAPS` — when set, each precise-frame GC scan also
/// runs [`verify_precise_covers_conservative`], logging any in-band word that
/// looks like a live oop but is not recorded by ANY of the method's oop maps.
/// Default-off; the normal union scan still runs, so behaviour is unchanged.
/// This is the completeness oracle the design doc wants before the conservative
/// backstop could ever be lifted for a moving collector.
#[inline]
pub(in crate::jit::conservative_roots) fn verify_oop_maps_enabled() -> bool {
    static E: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&E, || {
        if false {
            return true;
        }
        // ARMED AUTOMATICALLY FOR THE AARCH64 BACKEND.
        //
        // That backend's `fully_oop_covered` is computed from an argument, not
        // from a run: no host in this repository executes aarch64, so its maps
        // have never been checked against a live frame. This oracle is the
        // check -- it walks each frame against its OWN method's maps and
        // refutes a coverage claim it can disprove
        // (`incomplete_reason::COVERAGE_ORACLE_REFUTED`) -- and a claim that
        // has never been executed should not be trusted on its first run
        // merely because nobody remembered an environment variable.
        //
        // IT USED TO ARM ON `CRATONVM_JIT_ARM64_SAFEPOINTS` ALONE, which was
        // right when a poll was the only thing that recorded a map. Round 9
        // wave 18 made an ALLOCATION a safepoint on that backend, and wave 21
        // gave an allocating method the frame base its maps are read through,
        // so a default `CRATONVM_JIT_ARM64=1` run now publishes and spends
        // precise maps with polls off -- i.e. on exactly the configuration
        // this oracle had stopped watching. Reading the backend switch instead
        // arms it for every aarch64 run that can produce a map; the polls
        // switch is kept in the disjunction because it is what a reader looks
        // for, and because it is legal to set it without the backend one in a
        // test.
        //
        // It costs a read-only walk per precise frame, on a path that is
        // itself opt-in and experimental. With neither switch set -- every
        // x86-64 build, which is every production build -- this is exactly as
        // it was: off.
        ["CRATONVM_JIT_ARM64", "CRATONVM_JIT_ARM64_SAFEPOINTS"]
            .iter()
            .any(|key| {
                matches!(
                    cratonvm_types::flags::runtime_var(key)
                        .as_deref()
                        .map(str::trim)
                        .map(str::to_ascii_lowercase)
                        .as_deref(),
                    Ok("1") | Ok("true") | Ok("on") | Ok("yes")
                )
            })
    })
}

/// `CRATONVM_PRECISE_COVERAGE_PIN` — when set, surface
/// [`cratonvm_jit::CompiledMethod::fully_oop_covered`] at GC scan time: count
/// (and rate-limit log) precise frames that are NOT fully covered. On the
/// non-moving sweep the conservative backstop already pins every found oop, so
/// this is byte-identical today; it is the explicit visibility + scaffold for
/// the future moving path, where an un-covered frame must be PINNED, not
/// relocated. Default-off.
#[inline]
pub(in crate::jit::conservative_roots) fn coverage_pin_enabled() -> bool {
    static E: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&E, || false)
}

/// Whether the completeness oracle has ever refuted a coverage claim in this
/// process. Read by the root scan before it spends the bit.
pub fn coverage_oracle_refuted() -> bool {
    coverage_oracle_refuted_get()
}

/// Record a refutation and mark THIS cycle incomplete, so the collector that is
/// mid-decision diverts as well as every later one.
pub(in crate::jit::conservative_roots) fn note_coverage_oracle_refutation() {
    let first = !coverage_oracle_refuted_swap();
    cratonvm_gc::gc_quiescence::mark_moving_young_coverage_incomplete_because(
        cratonvm_gc::gc_quiescence::incomplete_reason::COVERAGE_ORACLE_REFUTED,
    );
    if first {
        eprintln!(
            "[VERIFY-OOP-MAPS] REFUTED: a frame asserting fully_oop_covered holds an \
             in-band oop no map names. Conservative JIT backstop is now forced ON for \
             the rest of this process."
        );
    }
}

/// Run the completeness oracle over this thread's live compiled frames and
/// report whether it refutes any frame's coverage claim.
///
/// This exists because of a structural hole, not a missing feature. The oracle
/// runs inside `scan_one_frame_precise`, which runs inside
/// `scan_active_jit_frames` — and `collect_roots` calls that only when it has
/// DECIDED NOT to suppress. So on every cycle where the coverage bit is
/// actually spent, the instrument designated to check it is not running. Every
/// `while_covered=0` reading ever taken was taken on cycles that did not use
/// the bit.
///
/// `roots` is scanned into and then truncated back by the caller when the proof
/// holds: the walk cannot be done without producing roots, and producing them
/// is how we know the walk really covered the same frames the backstop would.
pub fn verify_active_coverage_into(heap: &VmHeap, roots: &mut Vec<ObjectRef>) -> bool {
    let before = oop_map_audit::NEVER_MAPPED_WHILE_COVERED.load(Ordering::Relaxed);
    // Both claims, because the near-vacuous one cannot carry this alone:
    // `fully_oop_covered` is the FRAME-SLOT notion, false for any method with a
    // direct JIT->JIT call taking a reference argument, so a gate watching only
    // it almost never has a true guard to fire on. The moving path spends
    // `fully_shadow_covered`, so an unmapped live oop under THAT claim is the
    // refutation that matters.
    let before_shadow = oop_map_audit::NEVER_MAPPED_WHILE_SHADOW_COVERED.load(Ordering::Relaxed);
    // The map-SELECTION gap refutes the suppression exactly as a coverage gap
    // does, and this gate used not to look at it.
    //
    // `WRONG_MAP` counts an in-band live object address named by SOME oop map
    // of the owning method but NOT by the one its safepoint id selects — and
    // the selected maps are all the precise scan reads
    // (`scan_active_oop_map_at_rbp` iterates the `slot_offsets` of every map
    // carrying the frame's safepoint id — the union since gen r4w2; it used to
    // resolve ONE map through `find_oop_map_for_safepoint_id`). So such a word
    // is invisible to the precise walk. Today that is harmless
    // because the conservative backstop finds it anyway; the whole point of
    // this gate is to decide whether the backstop may be SKIPPED, and on that
    // question a wrong-map word strands its object just as completely as an
    // unmapped one.
    //
    // Measured on the 2026-09-02 soak: `wrong_map` is non-zero on workloads
    // whose `while_covered` is zero, so a gate reading only the coverage
    // counters returns "proof holds" over frames it has just been shown hold
    // unreachable-by-the-scan oops.
    let before_wrong_map = oop_map_audit::WRONG_MAP_VERIFIER_OOP.load(Ordering::Relaxed);
    scan_active_jit_frames(heap, roots);
    if oracle_force_refute() {
        note_coverage_oracle_refutation();
        return true;
    }
    oop_map_audit::NEVER_MAPPED_WHILE_COVERED.load(Ordering::Relaxed) > before
        || oop_map_audit::NEVER_MAPPED_WHILE_SHADOW_COVERED.load(Ordering::Relaxed) > before_shadow
        || oop_map_audit::WRONG_MAP_VERIFIER_OOP.load(Ordering::Relaxed) > before_wrong_map
}

/// Whether the pre-suppression verification should run: only when the oracle is
/// enabled (it is the thing doing the verifying) or when the forced-refutation
/// switch is on. Default-off, so the default path pays one cached bool.
pub fn coverage_gate_active() -> bool {
    verify_oop_maps_enabled() || oracle_force_refute()
}

/// `CRATONVM_DBG_OOP_ORACLE_FORCE_REFUTE=1` — treat the first covered frame the
/// oracle inspects as refuted, without a real unmapped oop.
///
/// The gate below is only reachable when a compiled method actually strands an
/// oop, which is exactly the thing every fix on this page has been closing —
/// `while_covered` is 0 on every workload measured. A gate whose wiring has
/// never been executed is indistinguishable from a gate that does not work, so
/// this forces the branch instead of waiting for a defect to supply it.
/// Test-only; it makes the VM refuse a suppression it could legitimately take.
pub(in crate::jit::conservative_roots) fn oracle_force_refute() -> bool {
    static E: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&E, || {
        false    })}

/// Count of precise frames observed NOT `fully_oop_covered` during GC scans
/// while `CRATONVM_PRECISE_COVERAGE_PIN` is on. Diagnostic only.
pub(in crate::jit::conservative_roots) static UNCOVERED_PRECISE_FRAMES: AtomicUsize = AtomicUsize::new(0);
/// Shared rate-limit for the Step-3 diagnostic logs (so neither knob spams).
pub(in crate::jit::conservative_roots) static STEP3_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);
pub(in crate::jit::conservative_roots) const STEP3_LOG_CAP: usize = 64;

/// Diagnostic read of the uncovered-precise-frame counter (see
/// [`coverage_pin_enabled`]). 0 when the knob was never on.
pub fn uncovered_precise_frame_count() -> usize {
    UNCOVERED_PRECISE_FRAMES.load(Ordering::Relaxed)
}

/// Step 3 completeness oracle — for one precise frame, diff the union of the
/// method's oop-map slots against a conservative sweep of the same stack band,
/// logging (rate-limited) any band word that looks like a live oop but is not
/// recorded by ANY map. Read-only.
///
/// CAVEAT: a `JIT_ENTRY_CHAIN` entry is pushed only at the interpreter→JIT
/// boundary, so the `[scanner_sp, frame_base)` band also spans this method's
/// *nested JIT→JIT callees*. Their oops are correctly absent from THIS method's
/// maps and therefore show here as "unmapped" — expected, not a gap. The signal
/// is sharpest for leaf-ish compiled frames (the register/spill-resident root
/// class, e.g. the bintrees-`main`-reads-`args` and `codePointAt` families).
/// Counters for [`verify_precise_covers_conservative`], reported at exit.
/// What the CLASS FILE's own verifier says about a java-local slot at a bci.
///
/// The reason this exists is that the oop-map oracle could not tell its three
/// populations apart, and said so: at 5–6 % of every in-band word on every
/// workload measured, `never_mapped` was dominated by two false positives it
/// declared but could not subtract —
///
///   * a primitive `i64` whose bits happen to land on a live object header, and
///   * a DEAD slot: an object reference left behind after its variable went out
///     of scope (`MVStore.closeStore` slot 5, scoped 149..170, read at bci 174,
///     plus javac's three `finally`-tail copies of it).
///
/// `org.h2.test.db.TestMultiThread` PASSES under a moving collector with 1.1 M
/// never-mapped words, 670 k of them in frames asserting shadow coverage. If
/// those were genuine the workload would corrupt. So the counter could not be a
/// backstop, and the page tracking it recorded that "a backstop that can tell a
/// dead slot from a live one without the map does not exist yet".
///
/// # Why the verifier's map is an INDEPENDENT answer and the oop map is not
///
/// Asking the JIT's own abstract interpreter would be circular: the oop map IS
/// its output, so "the map does not name this slot" and "the model says it is
/// not a live reference" are the same sentence. `classloading::type_maps` is a
/// different oracle with a different provenance — it is what
/// `bytecode_verifier` retains from the JVMS §4.10.1 StackMapTable walk, i.e.
/// **javac's own claim** about the type of every local at every instruction
/// start. It answers both false positives at once: a primitive local is not an
/// oop there, and an out-of-scope local is `Top`, which is not an oop either.
///
/// # What it refuses to answer, and why each refusal is load-bearing
///
/// * **An INLINED frame.** A spliced callee's locals live in the same java-local
///   region and the safepoint's bci belongs to the CALLEE's bytecode, so reading
///   the outer method's map at that pc reads a different method's types at a
///   coincidentally-valid pc. `Unknown`, always.
/// * **Any slot that is not a java local.** Operand spill is `Frame::stack`
///   indexed by runtime depth, which this frame does not carry; LICM hoists,
///   scalar-replaced fields and the register images are regions the verifier
///   has never heard of. Those are exactly the regions the band scan still
///   covers, so nothing is lost.
/// * **A pc with no row.** `local_oops_at` returns `None` for a pc that is not
///   an instruction start — which, for a safepoint id, means the frame is not
///   where its sp-id slot says it is. That is a finding of its own and must not
///   be read as "not an oop".
/// * **No store, or an unowned artifact.** The maps are read from the VM's own
///   [`cratonvm_classloading::TypeMapStore`], which the root pass binds for its
///   thread ([`OracleTypeMapsScope`]), keyed by the class id the artifact was
///   published under (`CompiledMethod::owner_class_id`). A scan with no bound
///   store (a unit test, an entry point that does not bind), or an artifact
///   that was never published, has nothing to ask; the scans with no store are
///   counted in [`oop_map_audit::VERIFIER_NO_STORE`].
///
/// Every refusal counts as [`oop_map_audit::VERIFIER_UNKNOWN`] rather than
/// being folded into either verdict, because a backstop whose "no gap here"
/// silently includes "could not look" is the vacuous green this whole line of
/// work exists to avoid.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(in crate::jit::conservative_roots) enum VerifierSlotVerdict {
    /// The class file says this local holds a REFERENCE at this bci, and no oop
    /// map of the frame names its slot. This is the actionable number.
    Oop,
    /// The class file says it is a primitive or out of scope here. The word is
    /// a false positive of the address-shaped-bits test.
    NotOop,
    /// No independent answer available — see the refusals above.
    Unknown,
}

/// Split a `CompiledMethod::method_label` (`"org/h2/mvstore/MVStore.closeStore:(ZI)V"`)
/// into its three parts.
///
/// Descriptor first: a descriptor cannot contain `:`, and a method name cannot
/// contain `.` or `:`, so two `rsplit_once`es are exact. Labels the JIT writes
/// for synthetic bodies (`"lambda-adapter->0x…"`) have neither separator and
/// fall out as `None`.
pub(in crate::jit::conservative_roots) fn split_method_label(label: &str) -> Option<(&str, &str, &str)> {
    let (owner_and_name, descriptor) = label.rsplit_once(':')?;
    let (class_name, method_name) = owner_and_name.rsplit_once('.')?;
    if class_name.is_empty() || method_name.is_empty() || descriptor.is_empty() {
        return None;
    }
    Some((class_name, method_name, descriptor))
}

/// The verifier's verdict for the java local at `[rbp - off]` of `cm`, at the
/// bci its active safepoint id names. See [`VerifierSlotVerdict`].
pub(in crate::jit::conservative_roots) fn verifier_local_verdict(
    cm: &cratonvm_jit::CompiledMethod,
    off: i32,
    bci: u32,
) -> VerifierSlotVerdict {
    if !cm.inlined_methods.is_empty() {
        return VerifierSlotVerdict::Unknown;
    }
    if bci == u32::MAX {
        return VerifierSlotVerdict::Unknown;
    }
    // `FrameLayout`: java local `i` lives at `[rbp - (i + 1) * 8]`.
    if off < 8 || off % 8 != 0 || cm.frame_layout.java_locals_hi <= 0 {
        return VerifierSlotVerdict::Unknown;
    }
    // jr21 localshi (R16B-177): `java_locals_hi` is EXCLUSIVE, like every
    // `FrameLayout` bound -- both x64 producers publish `(num_locals + 1) * 8`
    // (single-pass `frames.rs`, IR `ir_published_java_locals_hi`), and the
    // production readers (`layout_is_java_local`,
    // `region_is_dataflow_modelled`, `is_non_oop_reserved_tail_slot`) test
    // `off < java_locals_hi`. This used to read it inclusively (`off >`),
    // asking the class file about the word one past the last local.
    if off >= cm.frame_layout.java_locals_hi {
        return VerifierSlotVerdict::Unknown;
    }
    let local_index = (off / 8 - 1) as usize;
    let Some((_class_name, method_name, descriptor)) = split_method_label(&cm.method_label) else {
        return VerifierSlotVerdict::Unknown;
    };
    if cm.owner_class_id == cratonvm_types::jit_activation::NO_OWNER_CLASS {
        return VerifierSlotVerdict::Unknown;
    }
    let class_id = cratonvm_types::ClassId::new(cm.owner_class_id);
    with_oracle_type_maps(|store| {
        let Some(store) = store else {
            oop_map_audit::VERIFIER_NO_STORE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return VerifierSlotVerdict::Unknown;
        };
        let Some(maps) = store.type_maps_for_named(class_id, method_name, descriptor) else {
            return VerifierSlotVerdict::Unknown;
        };
        let Some(bits) = maps.local_oops_at(bci) else {
            return VerifierSlotVerdict::Unknown;
        };
        if local_index >= bits.len() {
            return VerifierSlotVerdict::Unknown;
        }
        if bits.get(local_index) {
            VerifierSlotVerdict::Oop
        } else {
            VerifierSlotVerdict::NotOop
        }
    })
}

/// Is the verifier oracle armed? `CRATONVM_DBG_VERIFY_OOP_MAPS` present, with
/// any value, read once.
///
/// Deliberately the arming the retired process mirror had (presence, not
/// [`verify_oop_maps_enabled`]'s truthiness plus the aarch64 auto-arm): the
/// band scan's liveness screen (`band_has_unpublished_word_with_map`) acts on
/// a `NotOop` verdict, so arming it more widely would change when a
/// moving-young cycle may move. Keeping the screen debug-only is a decision,
/// with its evidence on [`band_liveness_screen_disabled`].
pub(in crate::jit::conservative_roots) fn verifier_oracle_armed() -> bool {
    static ON: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&ON, || {
        false    })}

thread_local! {
    /// The VM type-map store the verifier oracle reads on this thread, bound by
    /// [`OracleTypeMapsScope`] for the duration of a root pass. `None` unless
    /// the oracle is armed.
    pub(in crate::jit::conservative_roots) static ORACLE_TYPE_MAPS: std::cell::RefCell<
        Option<std::sync::Arc<cratonvm_classloading::TypeMapStore>>,
    > = const { std::cell::RefCell::new(None) };
}

/// Run `f` with the store bound on this thread, if any. Thread teardown or a
/// re-entrant borrow reads as "no store". The `Arc` clone happens only when a
/// store is bound, i.e. only while the oracle is armed.
pub(in crate::jit::conservative_roots) fn with_oracle_type_maps<R>(
    f: impl FnOnce(Option<&cratonvm_classloading::TypeMapStore>) -> R,
) -> R {
    let bound = ORACLE_TYPE_MAPS
        .try_with(|cell| cell.try_borrow().ok().and_then(|b| b.clone()))
        .ok()
        .flatten();
    f(bound.as_deref())
}

/// Binds a VM's verifier type maps as the compiled-frame oracle's store for
/// the current thread until dropped, restoring what was bound before.
///
/// A no-op (no `Arc` clone, no TLS write) unless `CRATONVM_DBG_VERIFY_OOP_MAPS`
/// is set, so the root passes that bind it pay one cached bool when unarmed.
/// Every JIT root walk runs on the thread whose frames it scans, so a
/// thread-scoped binding is the VM those frames belong to.
#[must_use = "the binding lasts only while the scope is alive"]
pub struct OracleTypeMapsScope {
    pub(in crate::jit::conservative_roots) previous: Option<Option<std::sync::Arc<cratonvm_classloading::TypeMapStore>>>,
}

impl OracleTypeMapsScope {
    /// Bind `store` (the VM's `ClassRealm::type_maps`) for this thread.
    #[inline]
    pub fn bind(store: &std::sync::Arc<cratonvm_classloading::TypeMapStore>) -> Self {
        if !verifier_oracle_armed() {
            return Self { previous: None };
        }
        Self::bind_armed(store)
    }

    pub(in crate::jit::conservative_roots) fn bind_armed(store: &std::sync::Arc<cratonvm_classloading::TypeMapStore>) -> Self {
        let previous = ORACLE_TYPE_MAPS
            .try_with(|cell| match cell.try_borrow_mut() {
                Ok(mut bound) => Some(bound.replace(std::sync::Arc::clone(store))),
                Err(_) => None,
            })
            .ok()
            .flatten();
        Self { previous }
    }
}

impl Drop for OracleTypeMapsScope {
    fn drop(&mut self) {
        if let Some(previous) = self.previous.take() {
            let _ = ORACLE_TYPE_MAPS.try_with(|cell| {
                if let Ok(mut bound) = cell.try_borrow_mut() {
                    *bound = previous;
                }
            });
        }
    }
}

/// Which storage class of the compiled frame the slot at `[rbp - off]` is in.
///
/// This is what turns a never-mapped hit from a number into a verdict. The
/// abstract model behind `moving_young_coverage_complete` describes Java locals
/// and the operand stack; the module block on
/// `refresh_moving_young_coverage_for_current_thread` names the three storage
/// classes it does NOT describe — scalar-replacement field slots, LICM hoist
/// slots, and the full-GPR safepoint spill area. A hit in one of those is the
/// PREDICTED defect. A hit in a Java local is either a genuine miss of what the
/// model does claim, or this oracle's known false positive (a primitive whose
/// bits land on a live object header).
pub(in crate::jit::conservative_roots) fn classify_frame_slot(off: i32, layout: &cratonvm_jit::FrameLayout) -> &'static str {
    let within = |lo: i32, hi: i32| hi > lo && off >= lo && off < hi;
    if within(layout.scalar_lo, layout.scalar_hi) {
        "scalar-replacement-field"
    } else if within(layout.ref_hoist_lo, layout.ref_hoist_hi) {
        "licm-ref-hoist"
    } else if within(layout.arith_lo, layout.arith_hi) {
        "licm-arith-hoist"
    } else if within(layout.reg_spill_lo, layout.reg_spill_hi) {
        "gpr-safepoint-spill"
    } else if layout.java_locals_hi > 0 && off < layout.java_locals_hi {
        // jr21 localshi (R16B-177): exclusive bound, as `FrameLayout` documents
        // and `FrameLayout::region_name` reads it; `<=` labelled the word one
        // past the last local "java-local".
        "java-local"
    } else if within(layout.spill_lo, layout.spill_hi) {
        "operand-spill"
    } else if layout.locals_hi > 0 && off < layout.locals_hi {
        "reserved-locals-tail"
    } else {
        "other"
    }
}

/// The oop-map slot offsets the ACTIVE safepoint selects for the frame at
/// `rbp` — the exact set `scan_active_oop_map_at_rbp` would publish.
///
/// `None` means the id or its map could not be read, which is a different
/// finding from "the map is empty" and is counted separately.
pub(in crate::jit::conservative_roots) fn active_map_slots(rbp: usize, cm: &cratonvm_jit::CompiledMethod) -> Option<Vec<i16>> {
    // The true answer for such a frame, whatever its (possibly stale) id slot
    // holds: no slot is live (`CompiledMethod::frame_holds_no_references`).
    if cm.frame_holds_no_references {
        return Some(Vec::new());
    }
    let sp_id_slot_off = cm.sp_id_slot_off;
    if sp_id_slot_off <= 0 || rbp < sp_id_slot_off as usize {
        return None;
    }
    let id_addr = rbp - sp_id_slot_off as usize;
    if id_addr & 0x7 != 0 {
        return None;
    }
    // SAFETY: the safepoint id lives in the validated frame's reserved local
    // slot, exactly as `scan_active_oop_map_at_rbp` reads it.
    let safepoint_id = unsafe { (id_addr as *const usize).read() } as u32;
    // gen r4w2/youngmark: the union over every map with this id — the set
    // `scan_active_oop_map_at_rbp` now publishes (it used to read the first
    // map only). `None` still means "no map at all for this id".
    let mut slots: Option<Vec<i16>> = None;
    for map in cm.oop_maps.iter().filter(|m| m.bytecode_pc == safepoint_id) {
        let acc = slots.get_or_insert_with(Vec::new);
        for &off in &map.frame_slot_offsets {
            if !acc.contains(&off) {
                acc.push(off);
            }
        }
    }
    slots
}

/// The per-frame offset set type of [`verify_precise_covers_conservative`].
pub(in crate::jit::conservative_roots) type ScanSet = std::collections::HashSet<i16>;

/// Bytes of a [`ScanSet`] handle: a hashbrown table (control pointer and three
/// words) plus `RandomState`'s two keys -- word-sized fields, no padding.
pub(in crate::jit::conservative_roots) const SCAN_SET_BYTES: usize = std::mem::size_of::<ScanSet>();

/// The raw bytes of `s`'s handle, copied, not interpreted.
pub(in crate::jit::conservative_roots) fn scan_set_bytes(s: &ScanSet) -> [u8; SCAN_SET_BYTES] {
    let mut out = [0u8; SCAN_SET_BYTES];
    // SAFETY: `s` is a live, aligned `HashSet` of exactly `SCAN_SET_BYTES`
    // bytes whose fields are all pointer- or word-sized (no padding), so every
    // byte is initialised; they are copied into a local array and compared.
    unsafe {
        std::ptr::copy_nonoverlapping(
            (s as *const ScanSet).cast::<u8>(),
            out.as_mut_ptr(),
            SCAN_SET_BYTES,
        );
    }
    out
}

/// gen r5w4/pin8 — the fingerprint of the two sets one iteration of
/// [`verify_precise_covers_conservative`] builds, taken under
/// `CRATONVM_DBG=root-write-audit` (any value) right after they are built and
/// checked right before they drop.
///
/// Those sets are the victim of the pinned-copy netty SIGSEGV
/// (`docs/feature-designs/real-jdk-mode.md`):
/// mimalloc faulted freeing `active`'s table while `scan_one_frame_precise`
/// ran on the thread that owns them. Between the two points the scan only
/// READS its stack and the heap, so the check splits the crash three ways:
///
/// * the handle bytes changed: a STORE INTO THIS FRAME of the scanning
///   thread (a stack writer), caught before the free;
/// * the handle is intact but the table's element count changed: a store into
///   the table's heap allocation;
/// * both intact, and the drop still faults: the allocator's own metadata
///   (the page or segment the table lives in) was corrupted, i.e. the set is
///   an incidental victim of a heap store elsewhere.
///
/// The first two abort with a `SCANNER LOCAL CHANGED` line
/// (`root_write_audit::scanner_local_changed`); the third leaves the crash as
/// it was, now attributable. Cost when armed: 2 x 48-byte copies and two
/// small-table walks per compiled frame the oracle inspects.
pub(in crate::jit::conservative_roots) struct ScannerSetWatch {
    pub(in crate::jit::conservative_roots) active: [u8; SCAN_SET_BYTES],
    pub(in crate::jit::conservative_roots) any: [u8; SCAN_SET_BYTES],
    pub(in crate::jit::conservative_roots) active_count: usize,
    pub(in crate::jit::conservative_roots) any_count: usize,
}

impl ScannerSetWatch {
    pub(in crate::jit::conservative_roots) fn arm(active: &ScanSet, any: &ScanSet) -> Self {
        Self {
            active: scan_set_bytes(active),
            any: scan_set_bytes(any),
            active_count: active.iter().count(),
            any_count: any.iter().count(),
        }
    }

    /// Abort, naming the set and the frame, if either set changed since
    /// [`Self::arm`].
    pub(in crate::jit::conservative_roots) fn check(
        &self,
        active: &ScanSet,
        any: &ScanSet,
        rbp: usize,
        cm: &cratonvm_jit::CompiledMethod,
    ) {
        let pairs: [(&'static str, &ScanSet, &[u8; SCAN_SET_BYTES], usize); 2] = [
            (
                "verify_precise_covers_conservative::active",
                active,
                &self.active,
                self.active_count,
            ),
            (
                "verify_precise_covers_conservative::any",
                any,
                &self.any,
                self.any_count,
            ),
        ];
        for (site, set, before, count) in pairs {
            let now = scan_set_bytes(set);
            let addr = set as *const ScanSet as usize;
            if now != *before {
                cratonvm_gc::root_write_audit::scanner_local_changed(
                    site,
                    addr,
                    before,
                    &now,
                    format_args!(
                        "handle_changed=true (a store into the scanning thread's own frame) \
                         while inspecting the frame at rbp={rbp:#x} of {}",
                        cm.method_label
                    ),
                );
            }
            // The handle is the one `arm` saw, so the table it names is the
            // original allocation and walking its control bytes is sound.
            let now_count = set.iter().count();
            if now_count != count {
                cratonvm_gc::root_write_audit::scanner_local_changed(
                    site,
                    addr,
                    before,
                    &now,
                    format_args!(
                        "handle_changed=false table_count {count} -> {now_count} (a store into \
                         the set's heap table) while inspecting the frame at rbp={rbp:#x} of {}",
                        cm.method_label
                    ),
                );
            }
        }
    }
}

/// `CRATONVM_DBG_VERIFY_OOP_MAPS` — does the precise oop map actually cover
/// every live reference in the compiled frames it claims?
///
/// # What the previous version measured, and why it was an upper bound
///
/// It built its `mapped` set from ONE method's maps at ONE frame base
/// (`info.exact_rbp`) and then scanned the WHOLE band `[scanner_sp,
/// frame_base)`. That band spans every compiled frame in the chain plus the
/// interpreter, native and Rust frames the compiled code called into, so:
///
///   * a nested JIT callee's slots, correctly named by the CALLEE's own map at
///     the CALLEE's own rbp, counted as unmapped;
///   * every interpreter/native word counted as unmapped, though no oop map has
///     ever claimed to describe those — they are published by their own root
///     mechanisms;
///   * register images and dead spill slots counted as unmapped, which the band
///     verifier next door already documents as a false verdict "on every
///     collection".
///
/// It reported 64 hits on the `PolynomialTest` failure and its own header
/// admitted the ambiguity ("NB band may include nested-JIT-callee slots"), so
/// the number could not be used to say whether the codegen bit was sound.
///
/// # What this measures
///
/// The RBP chain is walked exactly as `scan_precise_frames_and_bands` walks it, and
/// each frame is checked against ITS OWN method's maps at ITS OWN rbp, over its
/// own band only, skipping the slots `band_slot_is_verifiable` excludes. Every
/// in-band object address is then classified:
///
///   * `never_mapped` — named by no map of the owning frame. A live reference
///     the collector would neither publish nor rewrite. This is the number that
///     answers whether the coverage bit is sound, and `never_mapped_while_covered`
///     is the subset where the frame's `fully_oop_covered` asserted otherwise.
///   * `wrong_map` — named by some map of the frame but not the one its
///     safepoint id selects. Not a codegen coverage gap; a selection gap.
///   * `below_jit` — below the innermost frame. The interpreter/native region,
///     reported so it can be subtracted rather than counted as a gap.
///
/// Still conservative in ONE direction, deliberately: a primitive `i64` whose
/// bits land on a live object header is counted. So a non-zero `never_mapped`
/// is a lead, and a ZERO is the strong result — it says no verifiable in-band
/// word went unnamed.
pub(in crate::jit::conservative_roots) fn verify_precise_covers_conservative(
    info: PreciseFrameInfo,
    cm: &cratonvm_jit::CompiledMethod,
    heap: &VmHeap,
) {
    use oop_map_audit as audit;
    use std::sync::atomic::Ordering as AOrd;

    // gen r5w4/pin8: under `CRATONVM_DBG=root-write-audit` the two per-frame
    // sets below are fingerprinted when built and checked before they drop
    // (`ScannerSetWatch`) -- they are the victim of the pinned-copy netty
    // SIGSEGV. Read once per call; one latched load when the audit is off.
    let watch_sets = cratonvm_gc::root_write_audit::enabled();
    let scanner_sp = current_stack_pointer();
    let entry_sp = info.frame_base;
    let mut rbp = info.exact_rbp;
    if rbp == 0 || rbp & 0x7 != 0 || rbp < scanner_sp || rbp >= entry_sp {
        audit::UNREADABLE_FRAMES.fetch_add(1, AOrd::Relaxed);
        return;
    }
    let Some(innermost) = innermost_frame_method(
        rbp,
        info.exact_cm_id,
        entry_sp,
        scanner_sp,
        info.compiled_method,
    ) else {
        // Nothing describes the frame at `exact_rbp`; every offset would be
        // read against the wrong method. Same fail-closed stance as the band
        // verifier.
        audit::UNREADABLE_FRAMES.fetch_add(1, AOrd::Relaxed);
        return;
    };
    // SAFETY: as in `scan_precise_frames_and_bands` — a chain entry's method is
    // Arc-owned while any of its frames is live, and a resolved callee is kept
    // alive by the live frame whose return address resolved it.
    let mut frame_cm: &cratonvm_jit::CompiledMethod = unsafe { &*innermost };
    let _ = cm;

    let mut lowest_band_lo = usize::MAX;
    // gch h1/jw (r17): the entry band's guard; a cut is an unreadable rest.
    let mut walk = GcChainWalkGuard::for_entry(scanner_sp, entry_sp);
    loop {
        if !walk.step() {
            audit::UNREADABLE_FRAMES.fetch_add(1, AOrd::Relaxed);
            break;
        }
        let frame_size = frame_cm.osr_frame_size;
        if frame_size <= 0 {
            audit::UNREADABLE_FRAMES.fetch_add(1, AOrd::Relaxed);
            break;
        }
        let frame_size = frame_size as usize;
        const MAX_COMPILED_FRAME_BYTES: usize = 1024 * 1024;
        if frame_size > MAX_COMPILED_FRAME_BYTES || frame_size > rbp {
            audit::UNREADABLE_FRAMES.fetch_add(1, AOrd::Relaxed);
            break;
        }
        let band_lo = rbp - frame_size;
        lowest_band_lo = lowest_band_lo.min(band_lo);

        // Engagement, counted per FRAME INSPECTED rather than per hit, so a zero
        // refutation above can be told from "the oracle never inspected a frame
        // that made the claim". Counted here — after the frame is known
        // readable, before its words are walked — so the denominator is exactly
        // the frames the refutation counters could have fired on.
        if frame_cm.fully_oop_covered {
            audit::FRAMES_CLAIMING_OOP_COVERAGE.fetch_add(1, AOrd::Relaxed);
        }
        if frame_cm.fully_shadow_covered {
            audit::FRAMES_CLAIMING_SHADOW_COVERAGE.fetch_add(1, AOrd::Relaxed);
        }

        // The bci this frame is standing on, so a reported site can be checked
        // against the method's LocalVariableTable scopes.
        let active_sp_id = frame_active_sp_id(rbp, frame_cm).unwrap_or(u32::MAX);
        let active: std::collections::HashSet<i16> = match active_map_slots(rbp, frame_cm) {
            Some(v) => v.into_iter().collect(),
            None => {
                audit::UNREADABLE_FRAMES.fetch_add(1, AOrd::Relaxed);
                std::collections::HashSet::new()
            }
        };
        let any: std::collections::HashSet<i16> = frame_cm
            .oop_maps
            .iter()
            .flat_map(|m| m.frame_slot_offsets.iter().copied())
            .collect();
        let set_watch = watch_sets.then(|| ScannerSetWatch::arm(&active, &any));

        audit::FRAMES.fetch_add(1, AOrd::Relaxed);
        let live_hi = moving_young_frame_live_hi(rbp, frame_cm);
        let mut off = 8i32;
        while (off as usize) <= frame_size {
            if !band_slot_is_verifiable(off, &frame_cm.frame_layout, live_hi) {
                off += 8;
                continue;
            }
            let addr = rbp - off as usize;
            if addr & 0x7 == 0 {
                audit::WORDS.fetch_add(1, AOrd::Relaxed);
                // SAFETY: `addr` is an 8-aligned address inside this thread's
                // own live compiled frame band, the same region
                // `scan_one_frame` reads.
                let qword = unsafe { (addr as *const usize).read() };
                if heap.is_object_address(qword).is_some() {
                    let off16 = i16::try_from(off).unwrap_or(i16::MAX);
                    if active.contains(&off16) {
                        // covered by the map the collector will actually scan
                    } else if any.contains(&off16) {
                        audit::WRONG_MAP.fetch_add(1, AOrd::Relaxed);
                        // Split it the way NEVER_MAPPED is split. Without this
                        // the map-selection population has no verdict at all,
                        // and a raw non-zero reading cannot distinguish a live
                        // reference the selected map omits from an old pointer
                        // lying dead in a reusable slot.
                        match verifier_local_verdict(frame_cm, off, active_sp_id) {
                            VerifierSlotVerdict::Oop => {
                                audit::WRONG_MAP_VERIFIER_OOP.fetch_add(1, AOrd::Relaxed)
                            }
                            _ => audit::WRONG_MAP_VERIFIER_OTHER.fetch_add(1, AOrd::Relaxed),
                        };
                    } else {
                        audit::NEVER_MAPPED.fetch_add(1, AOrd::Relaxed);
                        let class = classify_frame_slot(off, &frame_cm.frame_layout);
                        // WHAT THE CLASS FILE ITSELF SAYS about this slot at
                        // this bci -- the independent answer that separates a
                        // real coverage gap from the two false positives the
                        // raw counter cannot subtract. See
                        // `VerifierSlotVerdict`.
                        let verdict = verifier_local_verdict(frame_cm, off, active_sp_id);
                        match verdict {
                            VerifierSlotVerdict::Oop => {
                                audit::VERIFIER_OOP.fetch_add(1, AOrd::Relaxed)
                            }
                            VerifierSlotVerdict::NotOop => {
                                audit::VERIFIER_NOT_OOP.fetch_add(1, AOrd::Relaxed)
                            }
                            VerifierSlotVerdict::Unknown => {
                                audit::VERIFIER_UNKNOWN.fetch_add(1, AOrd::Relaxed)
                            }
                        };
                        // Only a frame that ASSERTS full coverage is evidence about
                        // the codegen bit. A map-less frame (`maps=0 covered=false`)
                        // is already reported by the NO_PRECISE_MAP obligation and
                        // says nothing here -- the first run's probe arm was almost
                        // entirely those.
                        audit::note_site(
                            frame_cm.entry_ptr() as usize,
                            off,
                            class,
                            &frame_cm.method_label,
                            active_sp_id,
                            frame_cm.fully_oop_covered,
                            frame_cm.fully_shadow_covered,
                        );
                        if frame_cm.fully_shadow_covered {
                            audit::NEVER_MAPPED_WHILE_SHADOW_COVERED.fetch_add(1, AOrd::Relaxed);
                        }
                        if frame_cm.fully_oop_covered {
                            audit::NEVER_MAPPED_WHILE_COVERED.fetch_add(1, AOrd::Relaxed);
                            // The bit has been caught claiming coverage it does
                            // not have -- but only if the word really is an oop.
                            // Latched, and `collect_roots` consults the latch
                            // before skipping the conservative backstop.
                            //
                            // GATED ON THE VERIFIER, for exactly the reason the
                            // shadow latch below already gives: the raw counter
                            // flags any in-band word that LOOKS like a heap
                            // address, and an old pointer left in a reusable
                            // slot after its value died looks exactly like one.
                            // The two latches sat side by side with only the
                            // second one gated, and the first one's cost was
                            // measured on 2026-09-03: H2
                            // (`org.h2.test.store.TestMVStoreTool`, 612
                            // compiled frames) trips it 400 times with
                            // `verifier_oop=0` on every one, and because the
                            // latch is process-wide the run then reports
                            // `root coverage: incomplete` on 100% of pauses --
                            // where the same class without the oracle reports
                            // 0.00%. A refusal that fires on shape rather than
                            // evidence, and it fired on every real workload.
                            if verdict == VerifierSlotVerdict::Oop {
                                note_coverage_oracle_refutation();
                            }
                        }
                        // ...and the same latch on the claim the MOVING path
                        // actually spends, but ONLY on a corroborated hit.
                        //
                        // `fully_shadow_covered` is the bit the per-frame proof
                        // is built from, so it is the one an unmapped live oop
                        // refutes -- but latching on the raw counter would have
                        // suppressed every collection on every workload
                        // measured, since 5-6% of in-band words trip it and
                        // `TestMultiThread` passes with 670 k of them. Gated on
                        // the verifier's own verdict, this is a refusal that
                        // fires on evidence rather than on shape.
                        if verdict == VerifierSlotVerdict::Oop && frame_cm.fully_shadow_covered {
                            note_coverage_oracle_refutation();
                        }
                        if STEP3_LOG_COUNT.fetch_add(1, AOrd::Relaxed) < STEP3_LOG_CAP {
                            eprintln!(
                                "[VERIFY-OOP-MAPS] NEVER-MAPPED in-band oop: code@{:p} \
                                 rbp={rbp:#x} slot=[rbp-{off:#x}] class={class} \
                                 addr={addr:#x} \
                                 val={qword:#x} frame_size={frame_size} maps={} covered={}",
                                frame_cm.entry_ptr(),
                                frame_cm.oop_maps.len(),
                                frame_cm.fully_oop_covered,
                            );
                        }
                    }
                }
            }
            off += 8;
        }
        // gen r5w4/pin8: nothing above wrote either set; check they are
        // still what was built before this iteration drops them.
        if let Some(w) = set_watch.as_ref() {
            w.check(&active, &any, rbp, frame_cm);
        }

        // `[rbp]` / `[rbp + 8]` are the saved caller RBP and return PC; a
        // non-JIT parent ends the walk.
        let parent_rbp = unsafe { (rbp as *const usize).read() };
        let ret_addr = unsafe { ((rbp + 8) as *const usize).read() };
        let Some(parent_cm_ptr) = cratonvm_jit::lookup_jit_code_range(ret_addr) else {
            break;
        };
        if parent_rbp <= rbp
            || parent_rbp & 0x7 != 0
            || parent_rbp >= entry_sp
            || parent_rbp < scanner_sp
        {
            audit::UNREADABLE_FRAMES.fetch_add(1, AOrd::Relaxed);
            break;
        }
        frame_cm = unsafe { &*(parent_cm_ptr as *const cratonvm_jit::CompiledMethod) };
        rbp = parent_rbp;
    }

    // Everything below the innermost compiled band is interpreter / native /
    // Rust. Counted, never blamed on an oop map.
    if lowest_band_lo != usize::MAX && lowest_band_lo > scanner_sp {
        let mut a = (scanner_sp + 7) & !7usize;
        while a + 8 <= lowest_band_lo {
            // SAFETY: 8-aligned address in this thread's own live stack band.
            let qword = unsafe { (a as *const usize).read() };
            if heap.is_object_address(qword).is_some() {
                audit::BELOW_JIT.fetch_add(1, AOrd::Relaxed);
            }
            a += 8;
        }
    }
}

pub(in crate::jit::conservative_roots) fn scan_one_frame_precise(info: PreciseFrameInfo, heap: &VmHeap, out: &mut Vec<ObjectRef>) {
    // SAFETY: `info.compiled_method` was populated from a live
    // `&CompiledMethod` at push time, and the chain is popped before
    // the borrow ends. The JIT cache also keeps the CompiledMethod
    // alive via Arc for the duration of the call. Reading through
    // the pointer is valid for the lifetime of this function.
    let cm: &cratonvm_jit::CompiledMethod = unsafe { &*info.compiled_method };

    // Step 3 (precise-jit-maps-default.md) — coverage visibility + completeness
    // oracle. Both gates default-OFF; off → two cached-bool branches and the
    // scan below is unchanged.
    if coverage_pin_enabled() && !cm.fully_oop_covered {
        let n = UNCOVERED_PRECISE_FRAMES.fetch_add(1, Ordering::Relaxed);
        if n < STEP3_LOG_CAP {
            eprintln!(
                "[COVERAGE-PIN] precise frame NOT fully_oop_covered (pinned via backstop): \
                 code@{:p} maps={}",
                info.entry_ptr,
                cm.oop_maps.len(),
            );
        }
    }
    if verify_oop_maps_enabled() {
        verify_precise_covers_conservative(info, cm, heap);
    }

    // A GC runs inside a helper, so its instruction pointer cannot identify
    // the suspended JIT caller. The emitter therefore writes the bytecode PC
    // of the active safepoint into a dedicated `[rbp - sp_id_slot_off]` slot
    // before every GC-capable call. Use that id to select ONE exact map for the
    // innermost JIT frame, then follow the standard saved-RBP/return-address
    // chain to select the exact map for each compiled caller as well. The old
    // implementation scanned the union of every map in only the boundary
    // method; besides retaining dead oops, it omitted maps belonging to nested
    // JIT callers entirely.
    let scanner_sp = current_stack_pointer();
    // ONE walk of the chain for both halves (round 11 wave 14, rt
    // `r11-rt-root-scan-residue` item 2): the precise maps and the bounded
    // bands used to walk it separately, each resolving the innermost method
    // and every parent's return address on its own. `bands_ok` is what the
    // retired `scan_compiled_frame_bands` returned.
    let bands_ok = info.exact_rbp != 0
        && info.exact_rbp & 0x7 == 0
        && info.exact_rbp >= scanner_sp
        && info.exact_rbp < info.frame_base
        && scan_precise_frames_and_bands(info, scanner_sp, true, heap, out);
    debug_assert!(bands_ok, "retired band_path::FALLBACK: scan_precise_frames_and_bands failed");
    coverage_census_note(crate::native::jni::JniPhase::FrameBands);
    band_path::BANDS.fetch_add(1, Ordering::Relaxed);
    let _ = info.entry_ptr; // reserved for future PC-precise lookup
}


// gci i23/rf -- reference-free compiled frames skip the band scan (proposal
// `common-w36b-proposal-reference-free-frames-skip-the-band-scan`, rank P1/rf).
//
// A body the optimizing tier published `frame_holds_no_references` keeps, by
// contract 1 of `docs/feature-designs/real-jdk-mode.md`,
// no reference in any word below `frame_layout.callee_saved_lo` (no `Ref`
// node, no reference parameter, no frame block, every map empty). The band
// verifier (`band_has_unpublished_young_word`), the maps
// (`active_map_slots`) and the remap (`remap_one_jit_frame`) already trust
// it; the band ROOT scan did not, so a stale heap word an earlier, deeper
// activation left in such a frame's locals or spills stayed a conservative
// root (and on G1 a region pin, published unrewritable) for as long as the
// frame lived. Under the switch the scan stops at the floor: the words at or
// beyond `callee_saved_lo` -- the CALLER's callee-saved GPR image, the XMM
// images, the blind safepoint spill, the deopt `SavedRegisters` block and the
// outgoing reserve, all of which the IR tier publishes as the tail
// `callee_saved_lo..frame_size` -- are scanned exactly as before, since a
// register the body leaves alone can still hold the caller's reference.
//
// It turns a contract the collector used only to SKIP work into one it uses
// to DROP roots; if an emitter ever broke contract 1, the object would be
// freed or moved under the frame. Hence opt-in, with the soak rows of
// `vi23-rf.list` (Gen, G1, a `--nojit` control) and the oracle's
// `reference-free` label (`CRATONVM_DBG_VERIFY_REG_OOP_MAPS=1`).

/// `CRATONVM_GC_REFERENCE_FREE_FRAME_ROOTS=1` (gci i23/rf, default on): see the
/// section above. Read once per chain walk ([`BandClaimSwitches`]), no static.
pub(in crate::jit::conservative_roots) fn reference_free_frame_roots_enabled() -> bool {
    true
}

/// The reference-free floor of one frame: `Some(callee_saved_lo)` when the
/// band scan may leave every word `[rbp - off]` with `off < callee_saved_lo`
/// unread, `None` (scan the whole band, as before) otherwise. Pure.
///
/// All of these must hold:
/// * `on` -- [`reference_free_frame_roots_enabled`];
/// * `holds_no_references` -- `CompiledMethod::frame_holds_no_references`;
/// * `ir_backend` -- the optimizing tier produced the body
///   (`CompiledMethod::used_ir_backend`). The contract is that emitter's; a
///   framed lambda thunk, the only other producer of the flag, publishes no
///   layout and is excluded by the floor test below as well;
/// * `on_chain` -- the frame is on the registered chain (`remap_covered`). A
///   frame the A5 pass recovered by INSPECTION may be a misread one, and a
///   misread floor would hide another frame's words, so it keeps the scan;
/// * the layout is the x86-64 geometry (`!callee_saved_shallow`: the save
///   area is the DEEP tail, so `callee_saved_lo` is a half-line) and the
///   floor lies inside the band (`0 < callee_saved_lo <= frame_size`).
pub(in crate::jit::conservative_roots) fn reference_free_band_floor(
    on: bool,
    holds_no_references: bool,
    ir_backend: bool,
    on_chain: bool,
    layout: &cratonvm_jit::FrameLayout,
    frame_size: usize,
) -> Option<i32> {
    if !(on && holds_no_references && ir_backend && on_chain) || layout.callee_saved_shallow {
        return None;
    }
    let lo = layout.callee_saved_lo;
    // Cast: `lo > 0` is checked first, so the widening is exact.
    (lo > 0 && lo as usize <= frame_size).then_some(lo)
}

/// gci i23/rf: `(frames the reference-free floor applied to, band words it
/// left unread)`, cumulative. The engagement counter of
/// `CRATONVM_GC_REFERENCE_FREE_FRAME_ROOTS`: `(0, _)` with the switch on means
/// no reference-free optimizing-tier frame was on a scanned chain, and the
/// run says nothing about the switch.
pub fn reference_free_frame_census() -> (usize, usize) {
    (
        LIVESET_CENSUS[REF_FREE_FRAMES].load(Ordering::Relaxed),
        LIVESET_CENSUS[REF_FREE_WORDS_EXCLUDED].load(Ordering::Relaxed),
    )
}

/// Scan ONE compiled frame's own band using its layout, and publish the
/// movable/unrewritable partition for it.
///
/// Returns `false` when the frame's recorded size is unusable, which is the
/// caller's signal to fall back to a layout-free sweep.
///
/// Factored out of [`scan_precise_frames_and_bands`] so the A5 unregistered-frame
/// path can reach it: a frame found by inspection is still a compiled frame
/// with a layout, and scanning it as one is the difference between a bounded
/// band that publishes a partition and a raw span sweep that pins everything
/// it touches.
///
/// `walk_hi` (gen r5w6/oomjit10) bounds the parent-frame walk the
/// callee-saved-image rule makes ([`caller_register_claim`]): a parent RBP at
/// or above it is not followed. `0` disables the rule for this frame.
#[allow(clippy::too_many_arguments)]
pub(in crate::jit::conservative_roots) fn scan_one_compiled_frame_with_layout(
    rbp: usize,
    cm: &cratonvm_jit::CompiledMethod,
    heap: &VmHeap,
    out: &mut Vec<ObjectRef>,
    remap_covered: bool,
    walk_hi: usize,
    switches: BandClaimSwitches,
    caller_claims: Option<&mut CallerClaimMemo>,
    // gcf f4/f: the frame's active maps when the caller (the precise walk)
    // already resolved them; `None` resolves them here.
    active: Option<&ActiveMaps>,
) -> bool {
    let frame_size = cm.osr_frame_size;
    if frame_size <= 0 {
        return false;
    }
    let frame_size = frame_size as usize;
    const MAX_COMPILED_FRAME_BYTES: usize = 1024 * 1024;
    if frame_size > MAX_COMPILED_FRAME_BYTES || frame_size > rbp {
        return false;
    }
    // gcf f1/j: the package's in-native deposit, envelope first. A band with
    // no word inside the heap envelope roots, pins and publishes nothing
    // whatever its claims say (each word dies at the envelope test of
    // `scan_one_frame_filtered`; the plain arm's validators reject it), so
    // its claims are not computed at all.
    if in_native_envelope_first() {
        if let Some((lo, hi)) = heap.conservative_addr_span() {
            if !band_has_word(rbp - frame_size, rbp, |w| w >= lo && w < hi) {
                coverage_census_note(crate::native::jni::JniPhase::EnvFrameSkipped);
                return true;
            }
        }
    }
    // The register-file half of this frame's active oop map. `None` keeps the
    // whole blind image in the scan, which is what every frame did before
    // 2026-09-09.
    // `live_hi` is read here once for both halves below; nothing runs on this
    // thread's frame between them, so the sp-id slot cannot change.
    // gcf f2/f: with every other claim's input, in one pass
    // ([`frame_claim_facts`]).
    let mut claims = match active {
        Some(active) => frame_claim_facts_with(rbp, cm, switches, active),
        None => frame_claim_facts(rbp, cm, switches),
    };
    // JIT round 25 (item 3, `CRATONVM_JIT_IR_PRIMITIVE_COLOUR_CLAIM`): the
    // body's own `Prim`-colour list at every poll, not only at one whose map
    // resolved (where it is the same list). See `ir_prim_colour_claim`.
    let body_prims = ir_prim_colour_claim(cm, switches.ir_prims, remap_covered);
    if let Some(prims) = body_prims {
        if claims.ir_prims.is_none() {
            LIVESET_CENSUS[IR_PRIM_BODY_CLAIM_FRAMES].fetch_add(1, Ordering::Relaxed);
        }
        claims.ir_prims = Some(prims);
    }
    let facts = claims.map;
    if facts.reg_mask.is_some() {
        REGOOP_FRAMES_MASKED.fetch_add(1, Ordering::Relaxed);
    } else {
        REGOOP_FRAMES_UNMASKED.fetch_add(1, Ordering::Relaxed);
    }
    // Both arms read the band as marking roots, which is what keeps these
    // objects alive across the pause AND what gets them copied, and both say
    // which of them arrived through a word no channel rewrites, so the pin
    // decision can veto a movable claim made elsewhere for the same address.
    // Only reachable with a resolved layout -- a foreign innermost frame has
    // none, and it already forces the non-moving sweep through
    // `FOREIGN_INNERMOST_RBP`, so there is no move to veto there.
    //
    // The filtered arm (the default: `CRATONVM_GC_DEAD_SPILL_ROOTS` is on)
    // does both in ONE walk; the plain arm keeps its two.
    let skip = band_skip_classes();
    // gen r5w3/live7: a body carrying a liveness table takes the filtered arm
    // too, so the claim is spent even with the dead-spill claim switched off.
    let liveness = !cm.frame_liveness.is_empty();
    // gci i23/rf (`CRATONVM_GC_REFERENCE_FREE_FRAME_ROOTS`, opt-in): a
    // reference-free optimizing-tier frame on the registered chain is cut at
    // `callee_saved_lo`; only the filtered arm spends a claim, so it takes it.
    let ref_free_lo = reference_free_band_floor(
        switches.ref_free_frames,
        cm.frame_holds_no_references,
        cm.used_ir_backend,
        remap_covered,
        &cm.frame_layout,
        frame_size,
    );
    // gcg g3/v: a published written set is a claim too, and only the
    // filtered arm keeps the root / pin partition for it.
    if skip.is_some()
        || facts.reg_mask.is_some()
        || facts.spill_written.is_some()
        || gc_dead_spill_roots_enabled()
        || liveness
        || ref_free_lo.is_some()
        // JIT round 25 (item 3): the claim stands in for a clear the JIT left
        // out, so it takes the arm that spends it.
        || body_prims.is_some()
    {
        scan_one_frame_filtered_from(
            rbp,
            frame_size,
            cm,
            skip,
            claims,
            heap,
            out,
            remap_covered,
            walk_hi,
            caller_claims,
            ref_free_lo,
        );
    } else {
        // The plain arm roots every word and pins in a second walk, so it
        // cannot spend the callee-saved-image rule without splitting the
        // partition (a word rooted but not pinned). It is reached only with
        // `CRATONVM_GC_DEAD_SPILL_ROOTS=0` and no other claim.
        scan_one_frame(rbp - frame_size, rbp, heap, out);
        publish_unrewritable_band_roots(rbp, frame_size, cm, facts, heap, remap_covered);
    }
    true
}

/// gkr (OOM21-2, `cratonvm_jit::service_copy_window_enabled`): may this walk
/// clear closed service copies at all? The switch; no class ever redefined
/// (a redefined callee's re-run is kept even when unsound, and it would read
/// the copies); and the refusals that keep every other unsound callee re-run
/// from reading them (`deopt_resume::unsound_rerun_refused`'s callee
/// families). Read once per chain walk.
pub(in crate::jit::conservative_roots) fn service_copy_window_live() -> bool {
    use cratonvm_types::flags::runtime_flag_default_on;
    cratonvm_jit::service_copy_window_enabled()
        && true
        && true
        && cratonvm_jit::deopt_sink_resume_enabled()
        // The forced-decline diagnostic intentionally exercises entry replay;
        // keep its inputs rooted so that it remains a valid control.
        && Err::<String, std::env::VarError>(std::env::VarError::NotPresent).is_err()
        && !crate::classloading::any_class_redefined()}

/// gkr (OOM21-2): `caller` (at `caller_rbp`) is parked in the direct call that
/// returns to `ret_addr`, into `callee` (at `callee_rbp`). When the call kept
/// service copies of its reference arguments
/// (`CompiledMethod::service_arg_copies`) and the callee stands at a
/// safepoint from which no exact re-run point is reachable
/// (`CompiledMethod::rerun_window_closed_sp_ids`), no reader of those copies
/// is left: the service refuses every re-run from here with `InternalError`
/// before it reads one, and a callee with no handlers has no handler arm. So
/// the copies are cleared, before any scan of the caller's frame reads them,
/// and the arguments live exactly as long as the callee's own frame keeps
/// them -- HotSpot's outgoing arguments belong to the callee. `live` is
/// [`service_copy_window_live`] for this walk. Returns the slots cleared.
pub(in crate::jit::conservative_roots) fn clear_closed_service_copies(
    live: bool,
    callee_rbp: usize,
    callee: &cratonvm_jit::CompiledMethod,
    caller_rbp: usize,
    caller: &cratonvm_jit::CompiledMethod,
    ret_addr: usize,
) -> usize {
    if caller.service_arg_copies.is_empty() {
        return 0;
    }
    if !live {
        if dbg_jit_rootscan() {
            eprintln!(
                "[jitrootscan] service-copies kept: window off for this walk, caller={}",
                caller.method_label
            );
        }
        return 0;
    }
    if (callee.rerun_window_closed_sp_ids.is_empty()
        && callee.service_arg_resume_only_sp_ids.is_empty())
        || !callee.method_declares_no_handlers
    {
        if dbg_jit_rootscan() {
            eprintln!(
                "[jitrootscan] service-copies kept: caller={} callee={} closed_ids={} no_handlers={}",
                caller.method_label,
                callee.method_label,
                callee.rerun_window_closed_sp_ids.len(),
                callee.method_declares_no_handlers
            );
        }
        return 0;
    }
    let ret_off = ret_addr
        .checked_sub(caller.entry_ptr() as usize)
        .and_then(|off| u32::try_from(off).ok());
    let Some(slots) = ret_off.and_then(|off| caller.service_arg_copies_at(off)) else {
        if dbg_jit_rootscan() {
            eprintln!(
                "[jitrootscan] service-copies none at ret_off={ret_off:?}: caller={} rows={:?}",
                caller.method_label,
                caller.service_arg_copies.iter().map(|(o, _)| *o).collect::<Vec<_>>()
            );
        }
        return 0;
    };
    let id = safepoint_id_slot(callee_rbp, callee);
    if !id.is_some_and(|id| {
        callee.rerun_window_closed_at_sp_id(id)
            || callee.service_arg_resume_only_sp_ids.binary_search(&id).is_ok()
    }) {
        if dbg_jit_rootscan() {
            eprintln!(
                "[jitrootscan] service-copies kept: caller={} callee={} sp={id:?} closed={:?}",
                caller.method_label, callee.method_label, callee.rerun_window_closed_sp_ids
            );
        }
        return 0;
    }
    let id = id.unwrap_or_default();
    let frame_size = usize::try_from(caller.osr_frame_size).unwrap_or(0);
    let mut cleared = 0;
    for &off in slots {
        let Ok(off) = usize::try_from(off) else {
            continue;
        };
        if off == 0 || off > frame_size || off % 8 != 0 {
            continue;
        }
        // SAFETY: `[caller_rbp - off]` is a word of the validated caller
        // frame (`off` within its `osr_frame_size`, 8-aligned); the frame's
        // thread is stopped for this scan, and no reader of the slot is left.
        unsafe { ((caller_rbp - off) as *mut usize).write(0) };
        cleared += 1;
    }
    if cleared > 0 && dbg_jit_rootscan() {
        eprintln!(
            "[jitrootscan] service-copies cleared={cleared} caller={} callee={} sp={id}",
            caller.method_label, callee.method_label
        );
    }
    cleared
}

/// `scan_precise_frames_and_bands`: walk a precise chain entry's RBP chain
/// ONCE, and at every compiled frame publish both its active oop map (the
/// precise half) and, when `bands` is on, its bounded band (what
/// `scan_compiled_frame_bands` did in a second walk until round 11 wave 14).
///
/// The caller has validated `info.exact_rbp` against `[scanner_sp,
/// info.frame_base)`. Returns whether the BAND half covered every frame, i.e.
/// whether the whole-band fallback may be skipped; `false` whenever `bands` is
/// off. The two halves keep their own stopping rules exactly:
///
///   * the precise half walks until a non-JIT return address, an invalid
///     parent link, or the end of the entry band ([`GcChainWalkGuard`]; 4096
///     parent steps under `CRATONVM_JIT_COMPILED_WALK_WHOLE_ENTRY=0`);
///   * the band half gives up (answer `false`) on an invalid link, a frame
///     whose layout is unusable, or a frame past that guard -- and once it has
///     given up the precise half keeps walking, as its own walk did.
pub(in crate::jit::conservative_roots) fn scan_precise_frames_and_bands(
    info: PreciseFrameInfo,
    scanner_sp: usize,
    bands: bool,
    heap: &VmHeap,
    out: &mut Vec<ObjectRef>,
) -> bool {
    let entry_sp = info.frame_base;
    let mut rbp = info.exact_rbp;
    let pin_unnamed = pin_unnamed_frame_refs_enabled();
    // `info.compiled_method` describes the frame this chain entry was pushed
    // for. It does NOT describe `exact_rbp` when an unguarded JIT->JIT call has
    // since published a deeper frame base there (see
    // `chain_entry_rbp_is_foreign`); reading a safepoint id out of that frame at
    // this method's offset and publishing this method's slot list would leave
    // the callee's real oops unmarked. So the innermost frame is published
    // under the method that actually describes it -- the entry's own for the
    // boundary or a direct self-call, the resolved callee after a direct
    // JIT->JIT call -- and an unresolvable (indirect) callee publishes no map
    // and gets a layout-free band bounded by the scanner's own SP: the
    // collector runs beneath that frame, so `[scanner_sp, rbp)` covers all of
    // it and nothing above it. Parents are resolved from their own return
    // addresses either way.
    let innermost = innermost_frame_method(
        rbp,
        info.exact_cm_id,
        entry_sp,
        scanner_sp,
        info.compiled_method,
    );
    let mut bands_ok = bands;
    // gcf f2/f: the band claims' switches, once per chain walk, and the
    // caller-register memo for this walk's fixed bound (`entry_sp`).
    let switches = BandClaimSwitches::read();
    let mut caller_claims = CallerClaimMemo::new(entry_sp);
    // gkr (OOM21-2): the frame below each parent, for its service copies.
    let window_live = service_copy_window_live();
    let mut callee_cm: Option<&cratonvm_jit::CompiledMethod> = None;
    match innermost {
        Some(p) => {
            // SAFETY: the chain entry's pointer is Arc-owned by the JIT cache
            // while any of its frames is live; a resolved callee is kept alive
            // by the live frame whose return address resolved it.
            let icm: &cratonvm_jit::CompiledMethod = unsafe { &*p };
            callee_cm = Some(icm);
            // gcf f4/f: one id read and one maps pass for both halves.
            let active = active_maps_for(rbp, icm, switches);
            scan_active_maps(rbp, icm, &active, heap, out);
            if pin_unnamed {
                pin_unnamed_frame_refs(rbp, icm, heap);
            }
            // `true`: this frame is on the REGISTERED chain, which is the set
            // `remap_register_image_words` walks, so its register image is
            // rewritten after a move.
            if bands_ok
                && !scan_one_compiled_frame_with_layout(
                    rbp,
                    icm,
                    heap,
                    out,
                    true,
                    entry_sp,
                    switches,
                    Some(&mut caller_claims),
                    Some(&active),
                )
            {
                bands_ok = false;
            }
        }
        None => {
            if bands_ok {
                // Same blindness as the whole-band fallback, for one frame: no
                // layout, so no partition, so every object here is an
                // unarguable pin. See `band_path`.
                band_path::FOREIGN_INNERMOST.fetch_add(1, Ordering::Relaxed);
                scan_one_frame(scanner_sp, rbp, heap, out);
            }
        }
    }
    let mut band_frames = 1usize;
    // gch h1/jw (r17): the entry band's guard for both halves. A cut answers
    // `false` (below), so the caller's whole-band sweep covers the frames the
    // cut left -- as the 4096 guard always did -- but a well-formed chain is
    // no longer cut at all.
    let mut walk = GcChainWalkGuard::for_entry(scanner_sp, entry_sp);
    while walk.step() {
        // SAFETY: `rbp` is an aligned address in this thread's live JIT stack
        // interval (validated by the caller, then as each parent link below).
        // The frame prologue establishes `[rbp]` as the caller RBP and
        // `[rbp + 8]` as its return address.
        let parent_rbp = unsafe { (rbp as *const usize).read() };
        let ret_addr = unsafe { ((rbp + 8) as *const usize).read() };
        let Some(cm_ptr) = cratonvm_jit::lookup_jit_code_range(ret_addr) else {
            // A non-JIT parent ends both walks cleanly: it owns no compiled
            // spill band and no oop map.
            return bands_ok;
        };
        if parent_rbp <= rbp
            || parent_rbp & 0x7 != 0
            || parent_rbp < scanner_sp
            || parent_rbp >= entry_sp
        {
            return false;
        }
        // SAFETY: code ranges retain their CompiledMethod metadata for the
        // lifetime of an active frame.
        let parent_cm: &cratonvm_jit::CompiledMethod =
            unsafe { &*(cm_ptr as *const cratonvm_jit::CompiledMethod) };
        // gkr (OOM21-2): before either half reads the parent's frame.
        if let Some(callee) = callee_cm {
            clear_closed_service_copies(window_live, rbp, callee, parent_rbp, parent_cm, ret_addr);
        }
        callee_cm = Some(parent_cm);
        // gcf f4/f: one id read and one maps pass for both halves.
        let active = active_maps_for(parent_rbp, parent_cm, switches);
        scan_active_maps(parent_rbp, parent_cm, &active, heap, out);
        if pin_unnamed {
            pin_unnamed_frame_refs(parent_rbp, parent_cm, heap);
        }
        if bands_ok {
            if band_frames >= walk.limit() {
                bands_ok = false;
            } else {
                band_frames += 1;
                if !scan_one_compiled_frame_with_layout(
                    parent_rbp,
                    parent_cm,
                    heap,
                    out,
                    true,
                    entry_sp,
                    switches,
                    Some(&mut caller_claims),
                    Some(&active),
                ) {
                    bands_ok = false;
                }
            }
        }
        rbp = parent_rbp;
    }
    false
}

/// `CRATONVM_JIT_PIN_UNNAMED_FRAME_REFS=1` -- pin every object a live compiled
/// frame names in a slot its oop maps do NOT name.
///
/// The defect this closes, measured 2026-09-03 on
/// `TestCachedQueryResults` with `CRATONVM_DBG_STALE_FRAME_WORDS=1`: one
/// reference lives in SEVERAL slots of a frame at once -- a GPR safepoint
/// spill, an operand spill and two below the locals boundary all held
/// `0x1fef6b70878` -- while the map names only the canonical home. Relocation
/// rewrites that home and every duplicate keeps pointing at the vacated page.
/// Reading one is a use-after-free. It is not, as this comment said until
/// 2026-09-05, the SIGSEGV of the retired
/// `bug-box-unbox-intrinsic-segv-under-relocation-20260902`: that one was a
/// WRITE by the slide's own `ptr::copy` into a decommitted granule, and
/// rewriting these duplicates did not move it.
///
/// Map SELECTION is not the problem and was ruled out first: `NO_MAP_FOR_SP_ID`
/// and `NO_SP_ID_SLOT` are both zero on that workload, and dev's
/// precise-oop-maps-for->64-locals fix changes nothing. The maps are found and
/// correct; they simply do not enumerate every copy the register allocator made.
///
/// Why PIN rather than rewrite the duplicates. Rewriting every frame word whose
/// value equals a moved object's base is a conservative WRITE: a primitive that
/// happens to hold that bit pattern would be silently corrupted, which is worse
/// than the crash it fixes. Pinning is conservative in the safe direction -- the
/// object does not move, so every copy of the reference stays valid, named or
/// not. It costs compaction, not correctness.
pub fn pin_unnamed_frame_refs_enabled() -> bool {
    static ON: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&ON, || {
        false
    })
}

/// Pin every object this frame names in a slot its maps do not.
///
/// Selection matches `remap_one_jit_frame` exactly -- the UNION of every map
/// whose `bytecode_pc` equals the frame's stamped safepoint id, not the first
/// one -- because a slot the remap WILL rewrite must not be pinned, and one it
/// will not must be.
pub(in crate::jit::conservative_roots) fn pin_unnamed_frame_refs(rbp: usize, cm: &cratonvm_jit::CompiledMethod, heap: &VmHeap) {
    use std::sync::atomic::Ordering as AOrd;
    let sp_id_off = cm.sp_id_slot_off;
    let frame_size = cm.osr_frame_size;
    if sp_id_off <= 0 || frame_size <= 0 || rbp < sp_id_off as usize {
        return;
    }
    let frame_size = frame_size as usize;
    const MAX_COMPILED_FRAME_BYTES: usize = 1024 * 1024;
    if frame_size > MAX_COMPILED_FRAME_BYTES || frame_size > rbp {
        return;
    }
    let id_addr = rbp - sp_id_off as usize;
    if id_addr & 0x7 != 0 {
        return;
    }
    // SAFETY: the safepoint id lives in the frame's reserved slot, written
    // before the helper call that can trigger this scan.
    let sp_id = unsafe { (id_addr as *const usize).read() } as u32;

    // The offsets the remap will rewrite. Same 64-entry bound and same union
    // as `remap_one_jit_frame`; past the bound we pin conservatively, which is
    // the safe direction (an over-pin costs compaction).
    let mut named: [i16; 64] = [0; 64];
    let mut named_len = 0usize;
    for map in cm.oop_maps.iter().filter(|m| m.bytecode_pc == sp_id) {
        for &off in &map.frame_slot_offsets {
            if named_len < named.len() && !named[..named_len].contains(&off) {
                named[named_len] = off;
                named_len += 1;
            }
        }
    }

    // The values the map DOES name, so a duplicate can be told from a lone
    // unmapped reference. Diagnostic only -- both get pinned.
    let mut mapped_vals: [usize; 64] = [0; 64];
    let mut mapped_len = 0usize;
    for &off in named[..named_len].iter() {
        if off <= 0 || rbp < off as usize {
            continue;
        }
        let a = rbp - off as usize;
        if a & 0x7 != 0 {
            continue;
        }
        // SAFETY: aligned slot inside this thread's live frame.
        let v = unsafe { (a as *const usize).read() };
        if mapped_len < mapped_vals.len() {
            mapped_vals[mapped_len] = v;
            mapped_len += 1;
        }
    }

    unnamed_ref_pins::FRAMES.fetch_add(1, AOrd::Relaxed);
    // JIT round 25 (item 3): a claimed `Prim` colour is no unnamed reference.
    let prim_claim = ir_prim_colour_claim_on_chain(cm);
    let lo = rbp - frame_size;
    let mut a = (lo + 7) & !7usize;
    let mut pins: Vec<usize> = Vec::new();
    // `< rbp`, not `<=`: `[rbp]` is the saved RBP, outside the `[lo, rbp)` band.
    while a < rbp {
        let off = (rbp - a) as i64;
        let is_named =
            off > 0 && off <= i16::MAX as i64 && named[..named_len].contains(&(off as i16));
        if !is_named && !ir_prim_colour_claimed(prim_claim, off) {
            // SAFETY: aligned word inside this thread's live compiled frame,
            // bounded by the recorded frame size.
            let v = unsafe { (a as *const usize).read() };
            if heap.is_object_address_for_root_scan(v).is_some() {
                pins.push(v);
                unnamed_ref_pins::PINNED.fetch_add(1, AOrd::Relaxed);
                if mapped_vals[..mapped_len].contains(&v) {
                    unnamed_ref_pins::DUPLICATE_OF_MAPPED.fetch_add(1, AOrd::Relaxed);                }            }        }
        a += 8;
    }
    if !pins.is_empty() {
        cratonvm_gc::gc_quiescence::add_xt_cycle_pinned_jit_roots(&pins);
    }
}

/// Scan the oop maps selected by a live frame's safepoint-id slot.
/// `rbp` must be the exact frame base established by the compiled prologue.
/// A missing id or map deliberately scans nothing here: the caller's
/// conservative compatibility backstop remains responsible for legacy and
/// uncovered frames.
///
/// (gen r5w3/live7: this doc sat stranded on `pin_unnamed_frame_refs_enabled`,
/// where it read as the first paragraph of that flag's doc.)
///
/// gcf f4/f: the frame walk now reads [`scan_active_maps`] over the maps it
/// resolved once; this stays as the reference the equivalence test holds
/// that path to.
#[cfg_attr(not(test), allow(dead_code))]
pub(in crate::jit::conservative_roots) fn scan_active_oop_map_at_rbp(
    rbp: usize,
    cm: &cratonvm_jit::CompiledMethod,
    heap: &VmHeap,
    out: &mut Vec<ObjectRef>,
) {
    let sp_id_slot_off = cm.sp_id_slot_off;
    // `frame_holds_no_references`: no map of such a body names a slot, and its
    // id slot may be stale by contract, so there is nothing to read.
    if sp_id_slot_off <= 0 || rbp < sp_id_slot_off as usize || cm.frame_holds_no_references {
        return;
    }
    let id_addr = rbp - sp_id_slot_off as usize;
    if id_addr & 0x7 != 0 {
        return;
    }
    // SAFETY: the safepoint id lives in the validated frame's reserved local
    // slot. It is written before the helper call that can trigger this scan.
    let safepoint_id = unsafe { (id_addr as *const usize).read() } as u32;
    // gen r4w2/youngmark (2026-09-23): EVERY map carrying this id, not the
    // first. Several `OopMapEntry`s can share a safepoint id, and the relocation
    // side (`remap_one_jit_frame`, `moving_young_frame_live_hi`,
    // `active_map_facts`) already merges them all — `find` there "silently
    // left the other safepoint's slots unrewritten". Marking from one map while
    // relocating from all of them is a missed root waiting for the band scan to
    // stop backstopping it. A slot listed by two maps is pushed twice, which
    // every root consumer tolerates (the root vector is deduplicated
    // downstream, and marking is idempotent).
    for map in cm.oop_maps.iter().filter(|m| m.bytecode_pc == safepoint_id) {
        scan_oop_slots(rbp, &map.frame_slot_offsets, &cm.method_label, heap, out);
    }
}

/// Read each oop slot listed in `slot_offsets` (positive byte distances below
/// `rbp`), validate via `heap.is_object_address`, and push any hit into `out`.
/// Used by [`scan_one_frame_precise`].
pub(in crate::jit::conservative_roots) fn scan_oop_slots(
    rbp: usize,
    slot_offsets: &[i16],
    method_label: &str,
    heap: &VmHeap,
    out: &mut Vec<ObjectRef>,
) {
    let record_provenance = record_provenance_for(heap);
    // JIT round 21 lane refs: a scan outside a collection's armed window (a
    // blocking deposit, a snapshot scan) under `CRATONVM_DBG_REFERENT_HOLDER`.
    let holder_prov = !record_provenance && crate::runtime::env_cache::dbg_referent_holder();
    for &offset in slot_offsets {
        // x64 map entries are positive distances from RBP to slots in the
        // downward-growing local/spill area: `off` means `[rbp - off]`.
        // Reject malformed zero/negative entries rather than ever reading a
        // saved RBP or return address as an oop slot.
        if offset <= 0 || rbp < offset as usize {
            continue;
        }
        let addr = rbp - offset as usize;
        // Alignment check defensively matches the conservative scan.
        if addr & 0x7 != 0 {
            continue;
        }
        // SAFETY: `frame_base` came from `current_stack_pointer()`
        // on this thread, and the offset is bounded by the frame
        // size recorded at compile time. The read is within the
        // calling thread's own stack region.
        let qword = unsafe { (addr as *const usize).read() };
        if let Some(obj) = heap.is_object_address_for_root_scan(qword) {
            // gen r4/mark: G1 only -- see `record_provenance_for`.
            if record_provenance || holder_prov {
                note_root_provenance(
                    record_provenance,
                    qword,
                    format!("scan_oop_slots method={method_label} off={offset}"),
                );
            }
            out.push(obj);
        }
    }
}

// gcf f7/f -- the frame-scan interior proof (a census; the opt-in filter
// `CRATONVM_GC_FRAME_SCAN_INTERIOR_FILTER` that dropped proven words from the
// roots was deleted by gch h3/yc: 0 words dropped in 6 runs of the JitOomRoot
// family, whose interior holder is the xt take-over's derived-pointer pass,
// which the filter exempted by design -- `gch-h2fj` report, block C).
//
// The f6 census named two holders of `GenR4W6JitOomRootProbe`'s dead `Node`
// chain that are not object starts at all: a `scan_one_frame` word 0x10 into a
// `Node` and an xt frozen-peer word 0x28 into one
// (`docs/feature-designs/jit-ir-optimizations.md`).
// Both pass the structural validator because a `Node`'s `long` fields decode
// as a header: at `+0x10` the field `a = v` gives `class_id = v`, a zero mark
// word (kind `Object`, no flags) and, from `b = v + 1`, a legacy slot count
// whose extent fits the generation. The old-gen mark then resolves the word to
// its containing object (`old_gen_interior_root_base`, the H2-CID0 pin) and
// keeps the whole chain.
//
// The band scans' contract is object STARTS only: a derived pointer (an
// element cursor, a field address) is not rooted by them unless its bytes
// happen to decode as a header. So dropping a word that is PROVEN not to be an
// object start removes nothing the contract promised; it removes the accident.
// The proof needs two facts, so that one broken invariant cannot drop a real
// object:
//
// 1. the word's own "header" lacks `GC_FLAG_HEADER`, which every allocator
//    stamps and every mark-word transition keeps (`types/src/heap_types.rs`);
//    G1 already refuses such a root as an object start
//    (`G1Collector::mark_root_is_object_start`), the Generational heap does not
//    (`header_reserved_fields_plausible` explains why it will not on reasoning
//    alone, hence the switch);
// 2. a STAMPED object the same validator accepts starts within
//    [`FRAME_INTERIOR_PROBE_WORDS`] words below it and its extent covers it.
//
// The proof itself stays: the helper-window ROOT_STARTS filter
// (`xt_root_scan::helper_window_root_is_start`) and the census below use it.
// The xt take-over probe never consulted it: it roots derived pointers on
// purpose (`CRATONVM_XT_TAKEOVER_INTERIOR`).

/// How far below a candidate [`frame_word_is_proven_interior_with`] looks for
/// the stamped object that contains it: 64 words, i.e. an interior offset up to
/// 504 bytes. A deeper interior word is left as it was (kept).
pub(in crate::jit::conservative_roots) const FRAME_INTERIOR_PROBE_WORDS: usize = 64;

/// Is the validated candidate `c` proven to be an object INTERIOR? Answers the
/// containing object's base, or `None`. `probe(a)` is `Some((stamped, size))`
/// when the root validator accepts `a` as an object start (`stamped`: its mark
/// carries `GC_FLAG_HEADER`; `size`: its extent in bytes), `None` otherwise.
///
/// `Some` only when `c` itself is accepted but unstamped AND the nearest stamped
/// accepted start below it, within [`FRAME_INTERIOR_PROBE_WORDS`] words,
/// extends past `c`. Unstamped starts below `c` are skipped (they are the same
/// kind of coincidence `c` may be); the first stamped one decides, because
/// objects do not overlap: if it ends at or before `c`, nothing below it can
/// contain `c` either.
pub(in crate::jit::conservative_roots) fn frame_word_is_proven_interior_with(
    c: usize,
    probe: impl Fn(usize) -> Option<(bool, usize)>,
) -> Option<usize> {
    match probe(c) {
        Some((false, _)) => {}
        _ => return None,
    }
    for k in 1..=FRAME_INTERIOR_PROBE_WORDS {
        let b = c.checked_sub(8 * k)?;
        match probe(b) {
            Some((true, size)) => return (b.saturating_add(size) > c).then_some(b),
            Some((false, _)) | None => {}
        }
    }
    None
}

/// The heap probe of [`frame_word_is_proven_interior_with`]: `Some((stamped,
/// size))` when the root validator accepts `a`, `None` otherwise.
pub(in crate::jit::conservative_roots) fn frame_interior_root_scan_probe(heap: &VmHeap, a: usize) -> Option<(bool, usize)> {
    heap.is_object_address_for_root_scan(a)?;
    // SAFETY: the root validator accepted `a`: an 8-aligned, mapped,
    // structurally valid header inside a heap arena whose extent is
    // committed.
    let h = unsafe { &*(a as *const cratonvm_types::ObjectHeader) };
    Some((
        h.gc_flags() & cratonvm_types::GC_FLAG_HEADER != 0,
        cratonvm_gc::gc::object_total_size(h),
    ))
}

/// gci i2/fr (`gci-i1fr-...-review-findings` item 3): the proof for a
/// candidate `c` the CALLER's validator already accepted. `c`'s own probe
/// answer is `(c_stamped, _)` from the header the caller just validated, so
/// the validator is not run on `c` a second time; the words below are probed
/// as before. Same answer as [`frame_word_is_proven_interior_with`] whenever
/// `probe(c)` would have accepted `c` with that stamp. Pure.
pub(in crate::jit::conservative_roots) fn frame_word_is_proven_interior_validated_with(
    c: usize,
    c_stamped: bool,
    probe: impl Fn(usize) -> Option<(bool, usize)>,
) -> Option<usize> {
    if c_stamped {
        return None;
    }
    frame_word_is_proven_interior_with(c, |a| if a == c { Some((false, 0)) } else { probe(a) })
}

/// gcg g1/f: the interior proof as a yes/no, for the opt-in starts filters
/// (`xt_root_scan::helper_window_root_is_start`,
/// `xt_root_scan::takeover_above_chain_word_probe`). `w` MUST be a word
/// `heap.is_object_address_for_root_scan` has just accepted (gci i2/fr: the
/// callers validate first, so `w`'s header is read directly instead of being
/// validated twice). No switch and no census here: the callers are behind
/// their own opt-ins.
pub(crate) fn frame_word_is_proven_interior_validated(heap: &VmHeap, w: usize) -> bool {
    // SAFETY: the caller's validator accepted `w` (a mapped header).
    let stamped = unsafe {
        (*(w as *const cratonvm_types::ObjectHeader)).gc_flags() & cratonvm_types::GC_FLAG_HEADER
            != 0
    };
    frame_word_is_proven_interior_validated_with(w, stamped, |a| {
        frame_interior_root_scan_probe(heap, a)
    })
    .is_some()
}

/// gcf f7/f census (the word stays a root): under
/// `CRATONVM_DBG=oldmark-root-census`, count a validated band candidate
/// `qword` that is proven to be a header-shaped object interior (`seen=`).
/// Cheap for every real object: a stamped header returns before anything else
/// is read, and nothing past it runs without the census.
pub(in crate::jit::conservative_roots) fn note_frame_scan_interior_word(heap: &VmHeap, qword: usize) {
    // SAFETY: the caller's validator accepted `qword` (a mapped header).
    let stamped = unsafe {
        (*(qword as *const cratonvm_types::ObjectHeader)).gc_flags()
            & cratonvm_types::GC_FLAG_HEADER
            != 0
    };
    if stamped || !crate::jit::helpers::oldmark_census_on() {
        return;
    }
    // gci i2/fr: `qword` is validated and unstamped; probe only below it.
    let Some(base) = frame_word_is_proven_interior_validated_with(qword, false, |a| {
        frame_interior_root_scan_probe(heap, a)    }) else {
        return;
    };
    let seen = LIVESET_CENSUS[FRAME_INTERIOR_SEEN].fetch_add(1, Ordering::Relaxed) + 1;
    if seen <= 8 || seen.is_power_of_two() {
        eprintln!(
            "[frame-interior] seen={seen} word=0x{qword:x} base=0x{base:x} off=+0x{:x}",
            qword - base,
        );
    }
}

/// gcf f7/f: header-shaped interior band words proven, cumulative
/// ([`note_frame_scan_interior_word`]).
pub fn frame_scan_interior_census() -> usize {
    LIVESET_CENSUS[FRAME_INTERIOR_SEEN].load(Ordering::Relaxed)
}

/// The census provenance of a word [`scan_one_frame`] rejected: a raw band
/// word (a Rust helper frame or a compiled frame scanned whole), named by the
/// nearest return address above it and the executable's base, so
/// `addr2line -e <exe> $((ra - exe_base))` names the owning function.
#[cold]
pub(in crate::jit::conservative_roots) fn band_reject_origin(slot: usize, hi: usize) -> String {
    format!(
        "band ra=0x{:x} exe_base=0x{:x}",
        g1_band_reject_pins::nearest_return_address(slot, hi),
        g1_band_reject_pins::exe_base(),
    )
}

/// The census provenance of a word [`scan_one_frame_filtered`] rejected: the
/// compiled method, the slot's frame offset and its layout class.
#[cold]
pub(in crate::jit::conservative_roots) fn compiled_reject_origin(
    cm: &cratonvm_jit::CompiledMethod,
    layout: &cratonvm_jit::FrameLayout,
    off: i32,
) -> String {
    format!(
        "compiled method={} off={off} region={}",
        cm.method_label,
        layout.region_name(off)
    )
}

