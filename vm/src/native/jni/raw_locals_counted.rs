#[allow(unused_imports)]
use super::*;

// ---------------------------------------------------------------------------
// The raw-JNI-locals count (gcd d4/k2, lane m's request 1)
// ---------------------------------------------------------------------------
//
// `cratonvm_gc::gc_quiescence::RawJniLocalsScope`: while any thread of a VM
// may hold a RAW local ref (an address its C code keeps and nothing can
// rewrite), the young pin ledger reads INCOMPLETE, so the opt-in pinned young
// copy / option B do not relocate under it. Fed from here:
//
// * every JNI native dispatch ([`dispatch_jni_native`], so both arms of the
//   `vm_exec.rs` JNI region by construction) holds a [`RawLocalsDispatch`]
//   from just after its receiver and arguments are recorded -- so a raw one
//   among them is known -- until after the C call returned and, with the
//   native transitions, left native. Counted iff [`local_refs_may_be_held_raw`]
//   (the default representation, or a raw escape on this thread). Nested
//   natives hold their own scopes, balanced by RAII;
// * a raw escape INSIDE a dispatch that was counted "indirect" (practically
//   unreachable: an implicit frame is always open, and a slot index past 2^31
//   or a frame depth past 65535 is needed) opens this thread's count for good
//   ([`note_raw_local_escape`], once per thread): fail closed, a lost
//   optimisation rather than a stale raw copy;
// * an `AttachCurrentThread` attachment with raw locals is counted from attach
//   to detach ([`count_attachment_raw_locals`], closed by its
//   `ForeignThreadBox`).

/// Does anything read the raw-JNI-locals count (`raw_jni_locals_matter`:
/// the pinned young copy, option B or precise-root promotion is on, or ZGC's
/// `CRATONVM_ZGC_RELOCATE_REFUSES_RAW_JNI_LOCALS`)? Unit tests switch it per
/// thread.
pub(in crate::native::jni) fn raw_locals_counted() -> bool {
    #[cfg(test)]
    {
        if let Some(on) = JNI_NATIVE_CALL
            .try_with(|t| t.raw_locals_override.get())
            .ok()
            .flatten()
        {
            return on;
        }
    }
    cratonvm_gc::gc_quiescence::raw_jni_locals_matter()
}

/// The raw-JNI-locals count of one native dispatch (see the section above).
/// `pub(crate)` for the other places C code runs with a JNI context and may
/// hold raw locals: `JNI_OnLoad` (`vm_exec.rs`, the `System.load` path) and a
/// JVMTI agent's event callback (`jvmti::native_env::in_event_context`).
#[must_use = "the raw-local count drops when the scope drops"]
pub(crate) struct RawLocalsDispatch {
    pub(in crate::native::jni) _scope: Option<cratonvm_gc::gc_quiescence::RawJniLocalsScope>,
    /// This dispatch bumped [`NativeCallTls::uncounted_dispatches`].
    pub(in crate::native::jni) uncounted: bool,
}

impl RawLocalsDispatch {
    /// For the C call the JNI context (`JniContextGuard` /
    /// `replace_jni_context`) is installed for: counts it in that VM's pause
    /// ledger while [`local_refs_may_be_held_raw`]. Inert when nothing reads
    /// the count or no context is installed.
    pub(crate) fn enter() -> Self {
        let inert = RawLocalsDispatch {
            _scope: None,
            uncounted: false,
        };
        if !raw_locals_counted() {
            return inert;
        }
        // zt t3/rl: an INDIRECT-locals dispatch counts nothing in the ledger;
        // it only leaves the thread-local mark a later raw escape reads
        // (`note_raw_local_escape`). So it no longer clones the ledger `Arc`
        // (two atomic RMWs per JNI call) just to enter an empty scope. Same
        // answers: no context installed is still inert.
        if !local_refs_may_be_held_raw() {
            let installed = JNI_SHARED_VM
                .try_with(|c| c.try_borrow().is_ok_and(|b| b.is_some()))
                .unwrap_or(false);
            if !installed {
                return inert;
            }
            let _ = JNI_NATIVE_CALL
                .try_with(|t| t.uncounted_dispatches.set(t.uncounted_dispatches.get() + 1));
            return RawLocalsDispatch {
                _scope: None,
                uncounted: true,
            };
        }
        let ledger = JNI_SHARED_VM
            .try_with(|c| {
                c.try_borrow().ok().and_then(|b| {
                    b.as_ref()
                        .map(|s| Arc::clone(s.mem.gc_barrier.pause_ledger()))
                })
            })
            .ok()
            .flatten();
        let Some(ledger) = ledger else {
            return inert;
        };
        let scope = cratonvm_gc::gc_quiescence::RawJniLocalsScope::enter(&ledger, true);
        RawLocalsDispatch {
            _scope: Some(scope),
            uncounted: false,
        }
    }
}

impl Drop for RawLocalsDispatch {
    fn drop(&mut self) {
        if self.uncounted {
            let _ = JNI_NATIVE_CALL.try_with(|t| {
                t.uncounted_dispatches
                    .set(t.uncounted_dispatches.get().saturating_sub(1))
            });
        }
    }
}

thread_local! {
    /// See [`NativeCallTls`].
    pub(in crate::native::jni) static JNI_NATIVE_CALL: NativeCallTls = const { NativeCallTls::new() };
}

// ---------------------------------------------------------------------------
// The JNI phase census (gce e1/j, `CRATONVM_DBG_JNI_PHASE=1`)
// ---------------------------------------------------------------------------

/// gce e1/j: one phase of a JNI native dispatch timed by [`JniPhaseCensus`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum JniPhase {
    /// `vm_exec`'s JNI door: `find_jni_native`.
    Lookup,
    /// `vm_exec`: `JniContextGuard::install_for_native`.
    CtxInstall,
    /// `vm_exec`: `JniImplicitFrameGuard::enter`.
    LocalFramePush,
    /// `vm_exec`: env, receiver, arity check, census counter, native frame.
    DoorPrologue,
    /// `vm_exec`: the whole `dispatch_jni_native` call.
    Dispatch,
    /// `vm_exec`: `jni_pending_exception_after_native`.
    ExceptionCheck,
    /// `dispatch_jni_native`: tags, receiver / argument handles, `jargs`.
    ArgMarshal,
    /// `dispatch_jni_native`: `RawLocalsDispatch::enter`.
    RawLocals,
    /// `JniNativeCall::enter_engaged`, whole (the package only).
    EnterNative,
    /// `native_call_enter_native`: TLAB retire.
    TlabRetire,
    /// `native_call_enter_native`: the whole deposit.
    Deposit,
    /// `native_call_enter_native`: `enter_in_native_region` (barrier).
    InNativeRegion,
    /// The C function itself (`call_jni_marshalled`).
    NativeBody,
    /// `JniNativeCall`'s drop, whole (the package only).
    LeaveNative,
    /// `native_call_leave_native`: `GcBarrier::leave_in_native_if`.
    LeaveBarrier,
    /// `native_call_leave_native`: quiet tail (origins, redefinition check).
    LeaveQuietTail,
    /// `native_call_leave_native`: the full wake (`check_post_block_gc`).
    LeaveFullWake,
    /// `dispatch_jni_native`: result decode.
    ResultDecode,
    /// Deposit (in-native only): publish marker, SATB flush, scan invalidation.
    DepPrologue,
    /// Deposit: `frame_class_owners`.
    DepClassOwners,
    /// Deposit: the interpreter frames' slots (the `snapshot` lock included).
    DepFrames,
    /// Deposit: stashed deopt frames and `slot_origins`.
    DepOrigins,
    /// Deposit: off-frame thread roots and the JNI local frames.
    DepOffFrame,
    /// Deposit: the moving-young coverage proof (Generational).
    DepCoverage,
    /// Deposit: the conservative JIT scan and `publish_pinned_jit_roots`.
    DepJitScan,
    /// Deposit: the shadow stack and the debug checks after it.
    DepShadow,
    /// Deposit: the reclaimed-slot audit and the flag.
    DepTail,
    /// Coverage proof (inside `dep_coverage`): gates, prune, `shadow_bail`.
    CovPrologue,
    /// Coverage proof: the per-entry chain walk (frame maps, parents).
    CovChainWalk,
    /// Coverage proof: the A5 block (memoised by the frozen band).
    CovA5,
    /// Coverage proof: `moving_young_unpublished_frame_oop_present`.
    CovBandVerify,
    /// gce e2/j: `scan_active_jit_frames`: the A5 block (unregistered-frame probe).
    JitA5,
    /// gce e2/j: `scan_active_jit_frames`: the chain's frames (`scan_active_jit_frames_with_sp`).
    JitChainFrames,
    /// gce e2/j: Deposit: `publish_pinned_jit_roots` and the G1 reject pins.
    DepJitPins,
    /// gce e2/j: Band verify: building the shadow-published value set.
    CovBandShadowSet,
    /// gce e2/j: Deposit: `capture_published_trace` and its store.
    TraceCapture,
    /// gce e2/j: Deposit: `publish_jmx_frame_monitors`.
    TraceJmx,
    /// gce e2/j: Deposit: `publish_blocked_frame_classes`.
    TraceCensus,
    /// gce e2/j: COUNT: A5 block answered by a raw-clean frozen-band verdict.
    A5MemoRaw,
    /// gce e2/j: COUNT: A5 block answered by a classified (residue-only) verdict.
    A5MemoClassified,
    /// gce e2/j: COUNT: A5 probe found no JIT return address.
    A5ProbeClean,
    /// gce e2/j: COUNT: A5 hit at or above the residue mark: accepted, band marked.
    A5AcceptLive,
    /// gce e2/j: COUNT: A5 hit with no residue mark (0): accepted, band marked.
    A5AcceptNoResidue,
    /// gce e2/j: COUNT: A5 hit accepted because the chain is non-empty (not judged).
    A5AcceptChain,
    /// gce e2/j: COUNT: A5 residue hit, but a live frame above the mark: rescued, marked.
    A5Rescued,
    /// gce e2/j: COUNT: A5 residue-only hit declined (nothing marked).
    A5Declined,
    /// gce e2/j: COUNT: a clean / declined verdict NOT memoised (partial or truncated band).
    A5NotRecorded,
    /// gce e2/j: COUNT: a precise chain entry scanned by its frame bands.
    FrameBands,
    /// gce e2/j: COUNT: a precise entry fell back to the whole-band scan: no usable exact RBP.
    FrameFallbackNoRbp,
    /// gce e2/j: COUNT: a precise entry fell back to the whole-band scan: bands refused.
    FrameFallbackBands,
    /// gce e2/j: COUNT: a conservative (map-less) chain entry: whole-band scan.
    FrameConservative,
    /// gce e2/j: COUNT: compiled frames the band verify walked.
    CovBandFrames,
    /// gce e2/j: COUNT (summed): words of those frames' bands.
    CovBandWords,
    /// gcf f1/j: COUNT: a compiled frame the in-native deposit skipped whole
    /// (`CRATONVM_JNI_IN_NATIVE_ENVELOPE_FIRST`): its band held no word the
    /// scan could root (G1 / ZGC band scan) or the verify could flag
    /// (Generational band verify).
    EnvFrameSkipped,
    /// gcf f1/j: COUNT (summed): band words of a scanned frame dropped before
    /// the claims because they lie outside the heap envelope.
    EnvWordsSkipped,
    /// gcf f1/j: COUNT (summed): band words of a scanned frame inside the
    /// envelope, which went through every claim as before.
    EnvWordsKept,
    /// gcf f2/j: COUNT: an in-native deposit that reused the previous one's
    /// interpreter half ([`InNativeRepeat`]).
    DepRepeat,
    /// gcf f2/j: COUNT: ... and left the previous one's published trace,
    /// JMX attribution and frame classes standing.
    DepRepeatTrace,
    /// gcf f2/j: COUNT: `CRATONVM_DBG_JNI_REPEAT_AUDIT=1` recomputed a reused
    /// half and found it different (must stay 0).
    DepRepeatMismatch,
    /// gch h2/jr: COUNT: an in-native entry a pause raced, which arrived at
    /// that pause without parking (`CRATONVM_JNI_IN_NATIVE_ARRIVE_NO_WAIT=1`,
    /// `GcBarrier::arrive_in_native`). The switch's engagement counter.
    InNativeArriveNoWait,
    /// gch h4/bp: COUNT: an in-native deposit that left its compiled band to
    /// the pause (`CRATONVM_JNI_IN_NATIVE_BAND_AT_PAUSE`, on with the package). The switch's
    /// engagement counter on the thread side.
    DepBandAtPause,
    /// gch h4/bp: a pause's scan of every in-native band
    /// (`gc_and_alloc::append_in_native_band_roots`), timed per pause that
    /// found at least one band.
    PauseBandScan,
    /// gch h4/bp: COUNT (summed): bands those pauses scanned.
    PauseBandThreads,
    /// gch h4/bp: COUNT (summed): words of those bands.
    PauseBandWords,
    /// gch h4/bp: COUNT (summed): roots those bands gave (companions included).
    PauseBandRoots,
    /// gci i4/fj: an idle foreign attachment's idle -> running transition,
    /// whole ([`foreign_leave_idle`]: a JNIEnv function's `ForeignJniEntry`
    /// or a Java call's `ForeignCallGuard` at depth 0).
    ForeignLeave,
    /// gci i4/fj: [`foreign_leave_idle`]: `mark_blocked_region_leave` (waits
    /// out a pause in progress).
    ForeignLeaveBarrier,
    /// gci i4/fj: [`foreign_leave_idle`]: the tid publish and the full wake
    /// (`check_post_block_gc`: the flagged leave, the fixups, the refresh
    /// deposit).
    ForeignWake,
    /// gci i4/fj: the function body between the leave and the enter of a
    /// foreign transition (a JNIEnv function, or a Java call with its Java).
    ForeignBody,
    /// gci i4/fj: an idle foreign attachment's running -> idle transition,
    /// whole ([`foreign_enter_idle`]).
    ForeignEnter,
    /// gci i4/fj: [`foreign_enter_idle`]: the tid withdrawal and the TLAB retire.
    ForeignTlabRetire,
    /// gci i4/fj: [`foreign_enter_idle`]: the full deposit.
    ForeignDeposit,
    /// gci i4/fj: [`foreign_enter_idle`]: `mark_blocked_region_enter` and,
    /// when a pause raced it, the arrival (which waits that pause out).
    ForeignEnterBarrier,
    /// gci i4/fj: COUNT: a foreign leave that found a pause requested or in
    /// progress when it began (it waits that pause out).
    ForeignLeaveRaced,
    /// gci i4/fj: COUNT: a foreign enter that a pause raced (`pre_stw`).
    ForeignEnterRaced,
    /// gci i5/fj: COUNT: a foreign leave that was quiet and skipped the wake
    /// (`CRATONVM_JNI_FOREIGN_QUIET_IDLE`). The switch's engagement counter.
    ForeignLeaveQuiet,
    /// gci i5/fj: COUNT: a foreign enter that went idle incrementally, without
    /// the full deposit ([`foreign_enter_idle_incremental`]).
    ForeignEnterIncremental,
    /// gci i16/dp (i15/jk R3): COUNT: a repeated in-native deposit that kept
    /// the saved compiled activations without walking them again, because
    /// the saved walk's raw key still matches
    /// ([`InNativeRepeat::trace_still_published`]). Engagement counter.
    DepRepeatWalkSkipped,
    /// gci i20/jt: COUNT: an in-native leave taken by the lock-free leave
    /// (`CRATONVM_JNI_IN_NATIVE_LOCKFREE_LEAVE=1` with the flag-first request
    /// order; quiet or not). The switch's engagement counter.
    LeaveLockfree,
    /// gci i28/nb: the narrowing of an in-native band to the JIT chain
    /// (`CRATONVM_JNI_IN_NATIVE_BAND_NARROW=1`, [`in_native_band_at_pause`]):
    /// the memo read or the fresh probe of the stack above the chain, timed
    /// per banded call.
    DepBandNarrowProbe,
    /// gci i28/nb: COUNT: a band narrowed to the chain. The switch's
    /// engagement counter.
    DepBandNarrowed,
    /// gci i28/nb: COUNT: ... on a verdict the frozen-band memo held (no
    /// probe).
    DepBandNarrowMemo,
    /// gci i48 lane jl48: COUNT: an in-native leave whose full wake kept the
    /// snapshot the window's pauses folded instead of refreshing it
    /// ([`native_call_leave_native`], [`NativeLeave::KeptWake`]).
    LeaveWakeKept,
    /// gcj j2/jn2: COUNT: a band the narrowing kept whole because the chain was
    /// empty, not above the band's low end, or at the stack top
    /// (`conservative_roots::ChainBandRefusal::NoChain`).
    DepBandNarrowNoChain,
    /// gcj j2/jn2: COUNT: ... because the read above the chain would be
    /// truncated (`ChainBandRefusal::Truncated`).
    DepBandNarrowTruncated,
    /// gcj j2/jn2: COUNT: ... because a JIT return address above the chain
    /// survived the residue filter (`ChainBandRefusal::JitAbove`).
    DepBandNarrowJitAbove,
    /// gcj j2/jn2: COUNT: ... because the narrowed band failed the band rule
    /// (the dispatch's spill outside it, [`in_native_band_narrowed_from`]).
    DepBandNarrowSpillOutside,
    /// gcj j2/jn2 (cut 2 of `gcj-j1jn-proposal-jni-transition-cost-ledger-and-three-cuts`,
    /// `CRATONVM_JNI_IN_NATIVE_KEEP_TLAB=1`): COUNT: an in-native entry that
    /// kept its TLAB instead of retiring it. The switch's engagement counter.
    TlabKept,
    /// gcj j2/jn2: COUNT: a kept TLAB a pause retired on its in-native
    /// owner's behalf (`gc_and_alloc::retire_in_native_kept_tlabs`).
    TlabKeptRetiredByPause,
    /// gcj j2/jn2: COUNT: an in-native leave that found its kept TLAB intact
    /// (no pause retired it) and allocates on from it.
    TlabKeptReused,
}

impl JniPhase {
    pub(in crate::native::jni) const COUNT: usize = JniPhase::TlabKeptReused as usize + 1;
    pub(in crate::native::jni) const NAMES: [&'static str; JniPhase::COUNT] = [
        "lookup",
        "ctx_install",
        "local_frame_push",
        "door_prologue",
        "dispatch",
        "exception_check",
        "arg_marshal",
        "raw_locals",
        "enter_native",
        "tlab_retire",
        "deposit",
        "in_native_region",
        "native_body",
        "leave_native",
        "leave_barrier",
        "leave_quiet_tail",
        "leave_full_wake",
        "result_decode",
        "dep_prologue",
        "dep_class_owners",
        "dep_frames",
        "dep_origins",
        "dep_off_frame",
        "dep_coverage",
        "dep_jit_scan",
        "dep_shadow",
        "dep_tail",
        "cov_prologue",
        "cov_chain_walk",
        "cov_a5",
        "cov_band_verify",
        "jit_a5",
        "jit_chain_frames",
        "dep_jit_pins",
        "cov_band_shadow_set",
        "trace_capture",
        "trace_jmx",
        "trace_census",
        "a5_memo_raw",
        "a5_memo_classified",
        "a5_probe_clean",
        "a5_accept_live",
        "a5_accept_no_residue",
        "a5_accept_chain",
        "a5_rescued",
        "a5_declined",
        "a5_not_recorded",
        "frame_bands",
        "frame_fallback_no_rbp",
        "frame_fallback_bands",
        "frame_conservative",
        "cov_band_frames",
        "cov_band_words",
        "env_frame_skipped",
        "env_words_skipped",
        "env_words_kept",
        "dep_repeat",
        "dep_repeat_trace",
        "dep_repeat_mismatch",
        "in_native_arrive_no_wait",
        "dep_band_at_pause",
        "pause_band_scan",
        "pause_band_threads",
        "pause_band_words",
        "pause_band_roots",
        "foreign_leave",
        "foreign_leave_barrier",
        "foreign_wake",
        "foreign_body",
        "foreign_enter",
        "foreign_tlab_retire",
        "foreign_deposit",
        "foreign_enter_barrier",
        "foreign_leave_raced",
        "foreign_enter_raced",
        "foreign_leave_quiet",
        "foreign_enter_incremental",
        "dep_repeat_walk_skipped",
        "leave_lockfree",
        "dep_band_narrow_probe",
        "dep_band_narrowed",
        "dep_band_narrow_memo",
        "leave_wake_kept",
        "dep_band_narrow_no_chain",
        "dep_band_narrow_truncated",
        "dep_band_narrow_jit_above",
        "dep_band_narrow_spill_outside",
        "tlab_kept",
        "tlab_kept_retired_by_pause",
        "tlab_kept_reused",
    ];
}

/// gce e1/j: per-VM wall-time census of the phases of a JNI native dispatch
/// ([`JniPhase`]), on with `CRATONVM_DBG_JNI_PHASE=1` (read once, when the VM
/// is built: `NativeRealm::jni_phase_census`). Off, every probe is one load
/// of [`Self::on`] and a not-taken branch. On, each phase boundary is one
/// `Instant::now()` and two relaxed adds; the deposit's sub-phases are timed
/// only inside the package's in-native deposit. Printed at exit as one
/// `[GC] jni_phase:` line (`jni_phase_census_line`, from `vm-cli`).
pub struct JniPhaseCensus {
    pub(in crate::native::jni) on: bool,
    pub(in crate::native::jni) ns: [std::sync::atomic::AtomicU64; JniPhase::COUNT],
    pub(in crate::native::jni) hits: [std::sync::atomic::AtomicU64; JniPhase::COUNT],
    /// gch h3/jx: the exit line was handed out ([`Self::take_exit_line`]), so
    /// the two exit arms of `vm-cli` never print it twice.
    pub(in crate::native::jni) exit_line_taken: std::sync::atomic::AtomicBool,
}

impl JniPhaseCensus {
    /// Read `CRATONVM_DBG_JNI_PHASE` (once per VM).
    pub fn from_env() -> Self {
        Self::new(false)
    }

    pub(crate) fn new(on: bool) -> Self {
        JniPhaseCensus {
            on,
            ns: std::array::from_fn(|_| std::sync::atomic::AtomicU64::new(0)),
            hits: std::array::from_fn(|_| std::sync::atomic::AtomicU64::new(0)),
            exit_line_taken: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// gch h3/jx: [`Self::line`] for an exit report, at most ONCE per VM (the
    /// first caller gets it, every later one `None`). `vm-cli` asks from both
    /// of its exit arms: `run()` on a normal `main` return, while it still
    /// owns the VM, and `maybe_dump_shutdown_reports` (through
    /// [`jni_phase_census_line`]) on the `System.exit` arm. Until h3 only the
    /// second asked, and on a normal return it runs after `run()` dropped its
    /// VM, so `process_vm()`'s weak cell was empty and a probe that returns
    /// from `main` (`Gcd1JniCostProbe`, the M1 perf runs) never printed the
    /// line it was run for.
    pub fn take_exit_line(&self) -> Option<String> {
        if !self.on
            || self
                .exit_line_taken
                .swap(true, std::sync::atomic::Ordering::AcqRel)
        {
            return None;
        }
        self.line()
    }

    /// A phase clock, or `None` when the census is off.
    /// Is the census on (the one `bool` every probe reads)?
    #[inline(always)]
    pub(crate) fn is_on(&self) -> bool {
        self.on
    }

    #[inline(always)]
    pub(crate) fn start(&self) -> Option<std::time::Instant> {
        if self.on {
            Some(std::time::Instant::now())
        } else {
            None
        }
    }

    /// Charge `phase` with the time since `clock` and restart the clock.
    #[inline(always)]
    pub(crate) fn lap(&self, phase: JniPhase, clock: &mut Option<std::time::Instant>) {
        if let Some(t0) = *clock {
            let now = std::time::Instant::now();
            self.add(phase, now.duration_since(t0));
            *clock = Some(now);
        }
    }

    /// gce e2/j: count one event of a `COUNT:` phase (no time).
    #[inline(always)]
    pub(crate) fn note(&self, phase: JniPhase) {
        if self.on {
            self.add(phase, std::time::Duration::ZERO);
        }
    }

    /// gce e2/j: add `n` to a `COUNT:` phase's count (no time). The line's
    /// `avg` is then 0 and the count is the sum.
    #[inline(always)]
    pub(crate) fn count(&self, phase: JniPhase, n: u64) {
        if self.on {
            self.hits[phase as usize].fetch_add(n, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// Charge `phase` with the time since `clock`.
    #[inline(always)]
    pub(crate) fn stop(&self, phase: JniPhase, clock: Option<std::time::Instant>) {
        if let Some(t0) = clock {
            self.add(phase, t0.elapsed());
        }
    }

    #[cold]
    pub(in crate::native::jni) fn add(&self, phase: JniPhase, d: std::time::Duration) {
        use std::sync::atomic::Ordering::Relaxed;
        let i = phase as usize;
        self.ns[i].fetch_add(u64::try_from(d.as_nanos()).unwrap_or(u64::MAX), Relaxed);
        self.hits[i].fetch_add(1, Relaxed);
    }

    /// The `[GC] jni_phase:` line: per phase that ran, `name=avg_ns/count`,
    /// in [`JniPhase`] order. `None` when off.
    pub fn line(&self) -> Option<String> {
        use std::sync::atomic::Ordering::Relaxed;
        if !self.on {
            return None;
        }
        let mut out = String::from("[GC] jni_phase:");
        for (i, name) in JniPhase::NAMES.iter().enumerate() {
            let hits = self.hits[i].load(Relaxed);
            if hits == 0 {
                continue;
            }
            let avg = self.ns[i].load(Relaxed) / hits;
            out.push_str(&format!(" {name}={avg}ns/{hits}"));
        }
        Some(out)
    }
}

/// gce e1/j: the process VM's `[GC] jni_phase:` line, or `None` when its
/// census is off (or no VM is left). `vm-cli` prints it at exit.
///
/// gch h3/jx: once per VM ([`JniPhaseCensus::take_exit_line`]); `None` when
/// the normal-return arm already printed it.
pub fn jni_phase_census_line() -> Option<String> {
    process_vm()?.natives.jni_phase_census.take_exit_line()
}

/// gce e1/j: the in-native deposit's sub-phase clock -- `None` unless the
/// census is on AND this deposit is the package's in-native one.
#[inline(always)]
pub(crate) fn deposit_phase_clock(census: &JniPhaseCensus) -> Option<std::time::Instant> {
    if census.on && crate::jit::conservative_roots::in_native_deposit_armed() {
        census.start()
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// The repeated in-native entry (gcf f2/j, `CRATONVM_JNI_IN_NATIVE_REPEAT`)
// ---------------------------------------------------------------------------
//
// `docs/feature-designs/real-jdk-mode.md`.
//
// With the JNI package every native call deposits the thread's roots
// (`native_call_enter_native`). For a compiled loop calling a native, about
// 0.9 us of that deposit recomputes, per call, what cannot have changed since
// the previous call's deposit: the interpreter half (each frame's class owner,
// the frames' slots, monitors and `slot_origins`) and the published trace
// (`frame_trace`, the JMX frame attribution, the frame-class census).
//
// Why it cannot have changed. When the innermost JIT entry was pushed at the
// thread's current interpreter depth (`interp_depth == frames.len()`), no
// interpreter frame was pushed since: every interpreter frame is a suspended
// caller of compiled code (or of Rust that entered it), and its slots, pc and
// monitors change only when
//
// * it runs again -- the chain is popped or pruned first (the chain's mutation
//   count), or an interpreted callee is pushed (`frames.len()`, and the top
//   frame's `(seq, exec_epoch)`);
// * a pause rewrites it -- every pause advances the barrier generation, a fold
//   into a blocked thread the fold count ([`NativeSync`]);
// * a class redefinition converts it (`redefinitions_seen`, and the
//   redefinition count for the trace's lines);
// * another deposit of the thread ran in between (an `Object.wait`, a park, a
//   debugger suspension whose wake applies deferred frame writes):
//   `JvmThread::root_publish_seq`, bumped by every deposit.
//
// All of those are the [`RepeatKey`]. While it matches, the interpreter half
// the last full deposit computed is, value for value, what this one would
// compute, so it is copied from the saved one; the compiled-frame half, the
// stashed deopt frames, the off-frame roots, the JNI locals, the coverage
// proof, the pins and the shadow stack are computed as always. The published
// trace also names the compiled activations, which differ per call site (the
// innermost frame's bci, an inlined callee): it stands only when
// `active_compiled_frames()` answers exactly what it answered then, with no
// reflective call, served `Thread.run` row or redefinition in it and no JMX
// frame attribution to republish.
//
// Kept by the depositing OS thread ([`NativeCallTls::repeat`]), keyed by the
// `JvmThread`'s address and id too. The saved references are never roots on
// their own: they are copied back only while no pause ran since they were
// read, so they are the current addresses.
//
// The residual, as for the frozen band memo of `jit::conservative_roots`: a
// LEAKED chain entry whose interpreter frame resumed without the entry being
// popped would read as suspended; guard drops truncate the chain to their own
// depth, so such an entry cannot outlive the entry it nests in.
// `CRATONVM_DBG_JNI_REPEAT_AUDIT=1` recomputes every reused half and counts a
// difference (`dep_repeat_mismatch`, stderr `[jni-repeat] MISMATCH`).

/// gcf f2/j: what has to be unchanged for [`InNativeRepeat`] to reuse a
/// deposit's interpreter half. See the section above for each term.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(in crate::native::jni) struct RepeatKey {
    pub(in crate::native::jni) thread: usize,
    pub(in crate::native::jni) thread_id: u64,
    pub(in crate::native::jni) publish_seq: u64,
    pub(in crate::native::jni) sync: NativeSync,
    pub(in crate::native::jni) redefinitions_seen: u64,
    pub(in crate::native::jni) redefinition_count: u64,
    pub(in crate::native::jni) chain: Option<(u64, u32)>,
    pub(in crate::native::jni) above_len: usize,
    pub(in crate::native::jni) frames_len: usize,
    pub(in crate::native::jni) top_frame: (u64, u64),
    pub(in crate::native::jni) top_small_locals: Option<([(u64, u8); 4], usize, Option<ObjectRef>)>,
    pub(in crate::native::jni) served_thread_run: (usize, (u32, u32), bool, u64, usize),
    /// `roots::conservative_locals_enabled()`: which frame scan the half was
    /// computed with. It is ON by default whenever any thread is in compiled
    /// code (`gc_quiescence::is_active()`), i.e. in exactly the calls the
    /// repeat exists for, so it is a key term, not a refusal: a half reused
    /// only under the same scan. The conservative scan's extra words are
    /// heap-validated, but a frozen frame's lost-tag reference named an object
    /// that existed when it was stored, so it was validated (and saved) at the
    /// recording deposit; a scalar that only LATER coincides with a new object
    /// is not a reference, and leaving it out only drops an over-retention.
    pub(in crate::native::jni) conservative_locals: bool,
}

const REPEAT_ABOVE_MAX_FRAMES: usize = 32;

fn encode_above_frames(frames: &[crate::runtime::frame::Frame], out: &mut Vec<u64>) {
    out.clear();
    for f in frames {
        f.push_repeat_key_words(out);
    }
}

fn above_frames_match(saved: &[u64], frames: &[crate::runtime::frame::Frame]) -> bool {
    let mut pos = 0usize;
    for f in frames {
        if !f.matches_repeat_key_words(saved, &mut pos) {
            return false;
        }
    }
    pos == saved.len()
}

impl RepeatKey {
    /// The key of `thread` now, or `None` when no repeat applies (`above_len`
    /// too deep, an unsequenced synthetic frame above the chain, or an active
    /// reflective row).
    pub(in crate::native::jni) fn read(shared: &SharedVm, thread: &JvmThread) -> Option<RepeatKey> {
        let (chain, conservative_locals) = repeat_process_terms();
        let frames_len = thread.frames.len();
        let above_start = match chain {
            Some((_, d)) => usize::try_from(d).ok()?,
            None => 0,
        };
        if above_start > frames_len {
            return None;
        }
        let above_len = frames_len - above_start;
        if above_len > REPEAT_ABOVE_MAX_FRAMES {
            return None;
        }
        if thread.frames[above_start..frames_len]
            .iter()
            .any(|f| f.seq == 0)
        {
            return None;
        }
        let served_thread_run = thread.served_thread_run.repeat_key()?;
        let (chain, top_frame, top_small_locals) = match chain {
            Some((chain_mutations, top_depth)) if above_len == 0 => {
                let small_locals =
                    if crate::jit::conservative_roots::chain_all_at_depth(top_depth) {
                        thread.frames.last().and_then(|f| f.repeat_small_locals_key())
                    } else {
                        None
                    };
                match (thread.frames.last(), small_locals) {
                    (Some(f), Some(small)) => (Some((0, top_depth)), (f.seq, 0), Some(small)),
                    (Some(f), None) => (Some((chain_mutations, top_depth)), (f.seq, f.exec_epoch), None),
                    (None, _) => (Some((chain_mutations, top_depth)), (0, 0), None),
                }
            }
            other => {
                let top_frame = if above_start > 0 {
                    let f = &thread.frames[above_start - 1];
                    if f.seq == 0 {
                        return None;
                    }
                    (f.seq, f.exec_epoch)
                } else {
                    (0, 0)
                };
                (other, top_frame, None)
            }
        };
        Some(RepeatKey {
            thread: thread as *const JvmThread as usize,
            thread_id: thread.thread_id.0,
            publish_seq: thread
                .root_publish_seq
                .load(std::sync::atomic::Ordering::Relaxed),
            sync: NativeSync::read(shared, &thread.gc_block_state),
            redefinitions_seen: thread.redefinitions_seen,
            redefinition_count: cratonvm_classloading::class_redefinition_count(),
            chain,
            above_len,
            frames_len,
            top_frame,
            top_small_locals,
            served_thread_run,
            conservative_locals,
        })
    }
}

/// gcf f2/j: the two key terms [`RepeatKey::read`] takes from outside the
/// thread: this OS thread's JIT chain (`conservative_roots::chain_repeat_key`)
/// and `roots::conservative_locals_enabled()`. Unit tests set them per thread
/// (`NativeCallTls::repeat_terms_override`): a real chain entry would raise
/// the process-wide "a thread is in compiled code" state, which switches
/// every concurrently running test's root scan to the conservative-locals
/// path (and the other tests flip `conservative_locals` under this one).
pub(in crate::native::jni) fn repeat_process_terms() -> (Option<(u64, u32)>, bool) {
    #[cfg(test)]
    {
        if let Some(terms) = JNI_NATIVE_CALL
            .try_with(|t| t.repeat_terms_override.get())
            .ok()
            .flatten()
        {
            return terms;
        }
    }
    (
        crate::jit::conservative_roots::chain_repeat_key(),
        crate::memory::roots::conservative_locals_enabled(),
    )
}

const REPEAT_TRACE_WAYS: usize = 32;

pub(in crate::native::jni) struct SavedTraceWay {
    walk: SavedWalk,
    lock_state: (usize, [usize; 8]),
    entries: Vec<crate::native::registry::StackTraceEntry>,
    jmx_attr: Option<crate::threading::thread_registry::FrameMonitorAttribution>,
}

/// gcf f2/j: what the last full in-native deposit on this OS thread computed
/// for its interpreter frames, under the key it holds for.
pub(crate) struct RepeatSaved {
    pub(in crate::native::jni) key: RepeatKey,
    /// Encoded identity + mutable non-local state for `frames[above_start..]`.
    pub(in crate::native::jni) above_words: Vec<u64>,
    /// The snapshot's interpreter half: frame slots, monitors, class owners.
    pub(in crate::native::jni) prefix: Vec<ObjectRef>,
    /// `GcBlockState::slot_origins` as that deposit recorded them.
    pub(in crate::native::jni) origins: Vec<crate::threading::jvm_thread::SlotOrigin>,
    /// The compiled activations its published trace was built from, or `None`
    /// when that trace must not be reused (see [`trace_inputs_reusable`]).
    /// gci i16/dp: with the raw key of the walk that produced them
    /// ([`SavedWalk`]).
    pub(in crate::native::jni) traces: Vec<SavedTraceWay>,
}

/// gci i16/dp (i15/jk R3, `gcg-g1j-jni-package-repeat-probe-excess-...`): a
/// compiled-frame walk's answer -- exactly `active_compiled_frames()` -- with
/// what that walk READ ([`crate::jit::conservative_roots::RawWalkKey`]). While
/// the key still matches ([`Self::still_answers`]: the JIT cache generation,
/// the chain, the RBP-chain words and return addresses, the safepoint-id
/// slots), a walk now would answer `frames` exactly, so a repeated in-native
/// deposit keeps them instead of walking again: no `Vec`, no `Arc` clones, no
/// inline-chain copies per call. The throwable capture memo
/// (`stackwalker::ThrowableCaptureMemo::lookup_raw`) relies on the same
/// contract.
pub(in crate::native::jni) struct SavedWalk {
    pub(in crate::native::jni) frames: Vec<crate::jit::conservative_roots::ActiveCompiledFrame>,
    pub(in crate::native::jni) key: crate::jit::conservative_roots::RawWalkKey,
}

impl SavedWalk {
    /// `active_compiled_frames()` (the same walk: from depth 0, unbounded,
    /// per-call lookups, default options), recording its raw key into `key`
    /// -- a reused buffer: the walk clears it first and keeps its capacity.
    pub(in crate::native::jni) fn walk(mut key: crate::jit::conservative_roots::RawWalkKey) -> SavedWalk {
        let frames = crate::jit::conservative_roots::active_compiled_frames_walk_recorded(
            usize::MAX,
            false,
            &mut key,
        );
        SavedWalk { frames, key }
    }

    /// Would `active_compiled_frames()` now answer exactly `self.frames`?
    /// Allocates nothing; `false` for a key the walk could not record.
    pub(in crate::native::jni) fn still_answers(&self) -> bool {
        crate::jit::conservative_roots::raw_walk_key_matches(&self.key)
    }
}

/// gcf f2/j: one deposit's use of the saved interpreter half (see the
/// section above). Made by `deposit_root_snapshot_inner` at its start
/// ([`Self::begin`]) and consumed at its end ([`Self::finish`]); inert in
/// every deposit but the package's in-native one with the switch on.
pub(crate) struct InNativeRepeat {
    /// This deposit is the package's in-native one, the switch is on and a
    /// key could be read: it records what it computes.
    pub(in crate::native::jni) armed: bool,
    pub(in crate::native::jni) chain_none: bool,
    /// `CRATONVM_DBG_JNI_REPEAT_AUDIT=1`: compute anyway and compare.
    pub(in crate::native::jni) audit: bool,
    /// The saved half, when its key matches this deposit's.
    pub(in crate::native::jni) hit: Option<RepeatSaved>,
    /// What this deposit computed itself (a miss, or an audit).
    pub(in crate::native::jni) fresh_prefix: Option<Vec<ObjectRef>>,
    pub(in crate::native::jni) fresh_origins: Option<Vec<crate::threading::jvm_thread::SlotOrigin>>,
    pub(in crate::native::jni) matched_way: Option<SavedTraceWay>,
    pub(in crate::native::jni) fresh_walk: Option<(SavedWalk, (usize, [usize; 8]))>,
    /// gci i16/dp: the saved half whose key did NOT match this deposit's (a
    /// miss). Never published: only its buffers are reused, by
    /// [`Self::note_interp`], [`Self::note_origins`] and the walk's raw key,
    /// so a miss copies into storage it already owns instead of allocating.
    pub(in crate::native::jni) spare: Option<RepeatSaved>,
}

impl InNativeRepeat {
    /// The inert value: every deposit but the package's in-native one.
    pub(crate) fn off() -> Self {
        InNativeRepeat {
            armed: false,
            chain_none: false,
            audit: false,
            hit: None,
            fresh_prefix: None,
            fresh_origins: None,
            matched_way: None,
            fresh_walk: None,
            spare: None,
        }
    }

    pub(crate) fn begin(shared: &SharedVm, thread: &JvmThread, raise_blocked_flag: bool) -> Self {
        let switches = jni_switches_of(shared);
        Self::begin_with(
            shared,
            thread,
            repeat_armed_from(&switches, raise_blocked_flag),
            switches.repeat_audit,
        )
    }

    /// [`Self::begin`] with the switches given (unit tests).
    pub(in crate::native::jni) fn begin_with(shared: &SharedVm, thread: &JvmThread, enabled: bool, audit: bool) -> Self {
        let mut r = InNativeRepeat::off();
        if !enabled || !crate::jit::conservative_roots::in_native_deposit_armed() {
            return r;
        }
        let saved = JNI_NATIVE_CALL
            .try_with(|t| t.repeat.try_borrow_mut().ok().and_then(|mut s| s.take()))
            .ok()
            .flatten();
        let Some(key) = RepeatKey::read(shared, thread) else {
            return r;
        };
        r.armed = true;
        r.chain_none = key.chain.is_none();
        r.audit = audit;
        let above_start = key.frames_len - key.above_len;
        match saved {
            Some(s)
                if s.key == key
                    && (key.above_len == 0
                        || above_frames_match(
                            &s.above_words,
                            &thread.frames[above_start..key.frames_len],
                        )) =>
            {
                r.hit = Some(s);
            }
            other => r.spare = other,
        }
        if r.hit.is_some() {
            shared.natives.jni_phase_census.note(JniPhase::DepRepeat);
        }
        r
    }

    pub(crate) fn reuse_interp(&self) -> Option<&[ObjectRef]> {
        if self.audit {
            return None;
        }
        self.hit.as_ref().map(|h| h.prefix.as_slice())
    }

    pub(crate) fn note_interp(&mut self, shared: &SharedVm, computed: &[ObjectRef]) {
        if !self.armed {
            return;
        }
        if let Some(h) = self.hit.as_ref() {
            if h.prefix.as_slice() != computed {
                repeat_mismatch(shared, "interpreter half", h.prefix.len(), computed.len());
            }
        }
        let mut prefix = self
            .spare
            .as_mut()
            .map(|s| std::mem::take(&mut s.prefix))
            .unwrap_or_default();
        prefix.clear();
        prefix.extend_from_slice(computed);
        self.fresh_prefix = Some(prefix);
    }

    pub(crate) fn reuse_origins(&self) -> Option<&[crate::threading::jvm_thread::SlotOrigin]> {
        if self.audit {
            return None;
        }
        self.hit.as_ref().map(|h| h.origins.as_slice())
    }

    pub(crate) fn note_origins(
        &mut self,
        shared: &SharedVm,
        computed: &[crate::threading::jvm_thread::SlotOrigin],
    ) {
        if !self.armed {
            return;
        }
        if let Some(h) = self.hit.as_ref() {
            if !same_slot_origins(&h.origins, computed) {
                repeat_mismatch(shared, "slot origins", h.origins.len(), computed.len());
            }
        }
        let mut origins = self
            .spare
            .as_mut()
            .map(|s| std::mem::take(&mut s.origins))
            .unwrap_or_default();
        origins.clear();
        origins.extend_from_slice(computed);
        self.fresh_origins = Some(origins);
    }

    /// Take the compiled activations walked during `trace_still_published` on a
    /// trace miss so `capture_published_trace_with_jit` can use them directly
    /// without walking `active_compiled_frames()` a second time.
    pub(crate) fn take_walked_frames_for_capture(
        &mut self,
    ) -> Vec<crate::jit::conservative_roots::ActiveCompiledFrame> {
        match self.fresh_walk.as_ref() {
            Some((w, _)) => w.frames.clone(),
            None => crate::jit::stack_trace::active_compiled_frames(),
        }
    }

    /// May the trace, JMX attribution and frame-class census the last full
    /// deposit published stand (or be restored from the 4-way cache)?
    pub(crate) fn trace_still_published(&mut self, shared: &SharedVm, thread: &JvmThread) -> bool {
        if !self.armed || !trace_inputs_reusable(thread) {
            return false;
        }
        let Some(lock_state) = shared
            .threads
            .thread_registry
            .jmx_lock_stack_repeat_state(thread.thread_id)
        else {
            return false;
        };

        let chain_none = self.chain_none;
        let raw_match_idx = self.hit.as_ref().and_then(|h| {
            h.traces.iter().position(|w| {
                w.lock_state == lock_state
                    && (w.walk.still_answers() || (chain_none && w.walk.frames.is_empty()))
            })
        });

        if let Some(idx) = raw_match_idx {
            if !self.audit {
                let way = self.hit.as_mut().unwrap().traces.remove(idx);
                shared
                    .natives
                    .jni_phase_census
                    .note(JniPhase::DepRepeatWalkSkipped);
                let frame_trace_empty = thread.frame_trace.lock().is_empty();
                let pause_published = thread
                    .gc_block_state
                    .native_excuse
                    .pause_published_trace();
                let same = if way.entries.is_empty() {
                    !frame_trace_empty && !pause_published
                } else {
                    if idx > 0 || frame_trace_empty || pause_published {
                        *thread.frame_trace.lock() = way.entries.clone();
                        shared
                            .threads
                            .thread_registry
                            .note_trace_deposit(thread.thread_id);
                        shared.threads.thread_registry.restore_jmx_frame_monitors(
                            thread.thread_id,
                            &thread.jmx_frame_monitors_published,
                            way.jmx_attr.as_ref(),
                        );
                    }
                    true
                };
                if same {
                    shared
                        .natives
                        .jni_phase_census
                        .note(JniPhase::DepRepeatTrace);
                }
                self.matched_way = Some(way);
                return same;
            }
        }

        let recycled_key = self
            .spare
            .as_mut()
            .and_then(|s| s.traces.pop())
            .map(|w| w.walk.key)
            .or_else(|| {
                self.hit
                    .as_mut()
                    .and_then(|h| (h.traces.len() >= REPEAT_TRACE_WAYS).then(|| h.traces.pop()))
                    .flatten()
                    .map(|w| w.walk.key)
            })
            .unwrap_or_default();
        let now = SavedWalk::walk(recycled_key);

        if let Some(idx) = raw_match_idx {
            // Audit mode: verify the raw-matched way has identical activations.
            if let Some(prev) = self.hit.as_ref().and_then(|h| h.traces.get(idx)) {
                if !same_activations(&prev.walk.frames, &now.frames) {
                    repeat_mismatch(
                        shared,
                        "compiled activations",
                        prev.walk.frames.len(),
                        now.frames.len(),
                    );
                }
            }
        }

        let act_match_idx = if !self.audit {
            self.hit.as_ref().and_then(|h| {
                h.traces.iter().position(|w| {
                    w.lock_state == lock_state && same_activations(&w.walk.frames, &now.frames)
                })
            })
        } else {
            None
        };

        if let Some(idx) = act_match_idx {
            let mut way = self.hit.as_mut().unwrap().traces.remove(idx);
            way.walk = now;
            let frame_trace_empty = thread.frame_trace.lock().is_empty();
            let pause_published = thread
                .gc_block_state
                .native_excuse
                .pause_published_trace();
            let same = if way.entries.is_empty() {
                !frame_trace_empty && !pause_published
            } else {
                if idx > 0 || frame_trace_empty || pause_published {
                    *thread.frame_trace.lock() = way.entries.clone();
                    shared
                        .threads
                        .thread_registry
                        .note_trace_deposit(thread.thread_id);
                    shared.threads.thread_registry.restore_jmx_frame_monitors(
                        thread.thread_id,
                        &thread.jmx_frame_monitors_published,
                        way.jmx_attr.as_ref(),
                    );
                }
                true
            };
            if same {
                shared
                    .natives
                    .jni_phase_census
                    .note(JniPhase::DepRepeatTrace);
            }
            self.matched_way = Some(way);
            return same;
        }

        self.fresh_walk = Some((now, lock_state));
        false
    }

    pub(crate) fn finish(mut self, shared: &SharedVm, thread: &JvmThread) {
        if !self.armed {
            return;
        }
        let Some(key) = RepeatKey::read(shared, thread) else {
            return;
        };
        let above_start = key.frames_len - key.above_len;
        let (hit_above, hit_prefix, hit_origins, mut traces) = match self.hit {
            Some(h) => (Some(h.above_words), Some(h.prefix), Some(h.origins), h.traces),
            None => (None, None, None, Vec::new()),
        };
        let (Some(prefix), Some(origins)) = (
            self.fresh_prefix.or(hit_prefix),
            self.fresh_origins.or(hit_origins),
        ) else {
            return;
        };
        let above_words = if key.above_len == 0 {
            let mut v = hit_above
                .or_else(|| self.spare.as_mut().map(|s| std::mem::take(&mut s.above_words)))
                .unwrap_or_default();
            v.clear();
            v
        } else if let Some(v) = hit_above {
            v
        } else {
            let mut v = self
                .spare
                .as_mut()
                .map(|s| std::mem::take(&mut s.above_words))
                .unwrap_or_default();
            encode_above_frames(&thread.frames[above_start..key.frames_len], &mut v);
            v
        };
        if trace_inputs_reusable(thread) {
            if let Some(way) = self.matched_way {
                traces.insert(0, way);
            } else if let Some((walk, lock_state)) = self.fresh_walk {
                let entries = thread.frame_trace.lock().clone();
                let jmx_attr = shared
                    .threads
                    .thread_registry
                    .jmx_frame_monitors_snapshot(thread.thread_id);
                traces.insert(
                    0,
                    SavedTraceWay {
                        walk,
                        lock_state,
                        entries,
                        jmx_attr,
                    },
                );
                if traces.len() > REPEAT_TRACE_WAYS {
                    traces.truncate(REPEAT_TRACE_WAYS);
                }
            } else if traces.is_empty() {
                if let Some(lock_state) = shared
                    .threads
                    .thread_registry
                    .jmx_lock_stack_repeat_state(thread.thread_id)
                {
                    let spare_key = self
                        .spare
                        .and_then(|mut s| s.traces.pop())
                        .map(|w| w.walk.key)
                        .unwrap_or_default();
                    let walk = SavedWalk::walk(spare_key);
                    let entries = thread.frame_trace.lock().clone();
                    let jmx_attr = shared
                        .threads
                        .thread_registry
                        .jmx_frame_monitors_snapshot(thread.thread_id);
                    traces.push(SavedTraceWay {
                        walk,
                        lock_state,
                        entries,
                        jmx_attr,
                    });
                }
            }
        } else {
            traces.clear();
        }
        let saved = RepeatSaved {
            key,
            above_words,
            prefix,
            origins,
            traces,
        };
        let _ = JNI_NATIVE_CALL.try_with(|t| {
            if let Ok(mut slot) = t.repeat.try_borrow_mut() {
                *slot = Some(saved);
            }
        });
    }
}

/// gcf f2/j: may a published trace be reused at all? Not while a reflective
/// call or a served `Thread.run` row is spliced into it (their naming can
/// change without a deposit), nor after any class redefinition (a compiled
/// activation's line then depends on its stamp).
pub(in crate::native::jni) fn trace_inputs_reusable(thread: &JvmThread) -> bool {
    thread.reflective_calls.is_empty()
        && thread.served_thread_run.is_empty()
        && cratonvm_classloading::class_redefinition_count() == 0
}

/// gcf f2/j: every field a published trace reads from two activation lists.
pub(in crate::native::jni) fn same_activations(
    a: &[crate::jit::conservative_roots::ActiveCompiledFrame],
    b: &[crate::jit::conservative_roots::ActiveCompiledFrame],
) -> bool {
    a.len() == b.len()
        && a.iter().zip(b).all(|(x, y)| {
            x.interp_depth == y.interp_depth
                && x.chain_index == y.chain_index
                && x.cm_ptr == y.cm_ptr
                && x.owner_class_id == y.owner_class_id
                && x.bci == y.bci
                && x.chain_exact == y.chain_exact
                && x.trap_site_exact == y.trap_site_exact
                && x.compile_cp_stamp == y.compile_cp_stamp
                && (Arc::ptr_eq(&x.label, &y.label) || x.label == y.label)
                && x.inline_chain.len() == y.inline_chain.len()
                && x.inline_chain.iter().zip(&y.inline_chain).all(|(p, q)| {
                    p.bci == q.bci
                        && p.class_id == q.class_id
                        && (Arc::ptr_eq(&p.label, &q.label) || p.label == q.label)
                })
        })
}

/// gcf f2/j: two `slot_origins` lists, field for field.
pub(in crate::native::jni) fn same_slot_origins(
    a: &[crate::threading::jvm_thread::SlotOrigin],
    b: &[crate::threading::jvm_thread::SlotOrigin],
) -> bool {
    a.len() == b.len()
        && a.iter().zip(b).all(|(x, y)| {
            x.frame == y.frame
                && x.idx == y.idx
                && x.is_stack == y.is_stack
                && x.live == y.live
                && x.orig == y.orig
                && x.cur == y.cur
        })
}

/// gcf f2/j: the audit found a reused half that differs from the recomputed
/// one -- a missing key term. Counted (`dep_repeat_mismatch`) and printed.
#[cold]
pub(in crate::native::jni) fn repeat_mismatch(shared: &SharedVm, what: &str, saved: usize, computed: usize) {
    let census = &shared.natives.jni_phase_census;
    census.note(JniPhase::DepRepeatMismatch);
    eprintln!(
        "[jni-repeat] MISMATCH {what}: saved {saved} entries, recomputed {computed} \
         (CRATONVM_DBG_JNI_REPEAT_AUDIT; a repeat would have published the saved one)"
    );
}

/// Running -> in native for `thread`'s JNI native call: the entry half of
/// `NativeContextImpl::begin_blocking_region`, minus its WAITING state and its
/// debugger inspection window (a thread in native is RUNNABLE, as in HotSpot).
///
/// gcd d10/j: minus also the blocked-stack summary of code reclamation
/// ([`crate::threading::gc_barrier::GcBarrier::mark_in_native_enter`]).
///
/// gch h4/bp: `band` is the call's compiled band left to the pause
/// ([`in_native_band_at_pause`]); `None` deposits as before. Recorded (or
/// cleared) BEFORE the deposit, whose last step raises `in_blocked_region`
/// with Release: a pause that reads the flag up reads this band.
pub(in crate::native::jni) fn native_call_enter_native(
    shared: &SharedVm,
    thread: &mut JvmThread,
    band: Option<(usize, usize)>,
) {
    let census = &shared.natives.jni_phase_census;
    let mut clock = census.start();
    // gcj j2/jn2: the VM in hand's switches (cut 1's raise, cut 2's TLAB).
    let switches = jni_switches_of(shared);
    store_in_native_band(&thread.gc_block_state, band);
    // gci i24/bp (`CRATONVM_JNI_IN_NATIVE_PROVEN_DEPTH_CREDIT`, opt-in): with a
    // band on a Generational heap the deposit below runs the coverage proof
    // after all (`conservative_roots::with_band_at_pause_proof`) and records
    // the depth it proved; zeroed first, with the flag down, so a depth read
    // by a pause is always this window's.
    let prove = band.is_some() && in_native_proven_depth_credit_admits(shared);
    thread
        .gc_block_state
        .in_native_proven_jit_depth
        .store(0, std::sync::atomic::Ordering::Relaxed);
    thread
        .gc_block_state
        .java_state
        .store(0, std::sync::atomic::Ordering::Release);
    // While still a counted mutator (a pause requested now waits for us), so
    // the tail filler cannot race its collector; see
    // `begin_blocking_region_with_state` for why a blocked thread must not
    // keep a TLAB. (gcf f7/j: the retire's real cost is the 256 KiB re-carve
    // the next allocation makes, not the retire itself; see
    // `gcf-f5j-jni-package-retires-the-tlab-on-every-native-call`. gcg g1/j:
    // with `CRATONVM_TLAB_SHARE_SIZER=1` that re-carve is capped at twice what
    // the retired buffer used, and no longer raises the sizer --
    // `Tlab::share_refill_request`.)
    // gcj j2/jn2 (cut 2, `CRATONVM_JNI_IN_NATIVE_KEEP_TLAB=1`, opt-in): kept
    // instead, and a collecting pause retires it on this thread's behalf; see
    // [`in_native_keeps_tlab`].
    retire_or_keep_tlab_into_native(shared, &switches, thread);
    census.lap(JniPhase::TlabRetire, &mut clock);
    // JIT round 16 wave 11 (lane jnieintr): the native's C code runs from
    // here; raised BEFORE the deposit (which raises `in_blocked_region` as its
    // last step), so no pause signals the thread while both are up
    // (`thread_registry::ffm_in_native_signal_exempt`). See
    // [`raise_foreign_native`]. gcj j2/jn2 (cut 1): from the VM in hand when
    // `CRATONVM_JNI_SWITCHES_ONCE_PER_CALL` is on.
    if switches.switches_once_per_call {
        raise_foreign_native_with(thread, switches.in_native_no_signal);
    } else {
        raise_foreign_native(thread);
    }
    let tid = thread.thread_id;
    // gce e1/j (`gce-e1j-in-native-deposit-rescans-the-whole-stack-above-the-jit-chain`):
    // the deposit's unregistered-JIT-frame probes of the stack ABOVE the
    // outermost compiled entry -- the whole interpreter / Rust stack of the
    // thread, per call -- reuse a clean verdict while the JIT entry chain was
    // not touched since it was recorded; see "The frozen band of a JNI
    // in-native deposit" in `jit::conservative_roots`. Every other deposit
    // probes as before.
    // gch h4/bp: with a band, the deposit skips the compiled frames' coverage
    // proof and chain scan (`conservative_roots::with_band_at_pause_deposit`):
    // the pause scans the band instead.
    crate::jit::conservative_roots::with_frozen_band_memo(Some(census), || {
        crate::jit::conservative_roots::with_band_at_pause_deposit(band.is_some(), || {
            crate::jit::conservative_roots::with_band_at_pause_proof(prove, || {
                crate::vm::NativeContextImpl { shared, thread }.deposit_root_snapshot();
            });
        });
    });
    if band.is_some() {
        census.note(JniPhase::DepBandAtPause);
    }
    census.lap(JniPhase::Deposit, &mut clock);
    enter_in_native_region(shared, tid);
    census.stop(JniPhase::InNativeRegion, clock);
}

/// JIT round 16 wave 11 (lane jnieintr,
/// `r16-ffm-takeover-signal-interrupts-blocking-syscalls-in-native-20261002`):
/// mark `thread` as running foreign C code for the take-over roster and the
/// helper-window list, as `NativeContextImpl::begin_native_region` does for a
/// GC-safe FFM downcall. Called by the owning thread only, before the flag
/// that makes it a blocked thread goes up ([`native_call_enter_native`]'s
/// deposit, [`native_call_reenter_incremental`]'s store); lowered first thing
/// by [`native_call_leave_native`], every way out of native.
///
/// Why the exemption is exact for a JNI native in native (both flags up):
/// the deposit made on the way in published every root the thread's stack
/// holds above the C frames -- interpreter frames, compiled frames scanned
/// conservatively and published pinned, native pins, the JNI local frames
/// (the incremental re-entry appends what a JNIEnv function minted, and its
/// frames are unchanged since the last full deposit); the C code holds only
/// indirect locals, global refs and detached copies (the package refuses the
/// call otherwise, `locals_are_indirect_here`); a leaf window, the one way
/// the thread touches the heap without leaving native, is drained before any
/// pause scans (`GcBarrier::drain_leaf_windows`); and every other JNIEnv
/// function, up-call included, leaves native -- lowering this -- before it
/// runs VM or Java code. So the thread's `Rip` is in C or in a Rust VM path
/// whose stack the deposit already covers: the signal would buy nothing and
/// would make the C code's `usleep` / `poll` / `epoll_wait` fail with EINTR.
/// `CRATONVM_JNI_IN_NATIVE_NO_SIGNAL=0` (or the registry-wide
/// `CRATONVM_FFM_IN_NATIVE_NO_SIGNAL=0`) signals it again.
#[inline]
pub(in crate::native::jni) fn raise_foreign_native(thread: &JvmThread) {
    raise_foreign_native_with(thread, jni_switches().in_native_no_signal);
}

/// gcj j2/jn2 (cut 1): [`raise_foreign_native`] with
/// `JniSwitches::in_native_no_signal` already in hand.
#[inline]
pub(in crate::native::jni) fn raise_foreign_native_with(thread: &JvmThread, no_signal: bool) {
    if no_signal {
        thread
            .gc_block_state
            .foreign_native
            .store(true, std::sync::atomic::Ordering::Release);
    }
}

/// gcj j2/jn2 (cut 2 of
/// `gcj-j1jn-proposal-jni-transition-cost-ledger-and-three-cuts-20261019`):
/// `CRATONVM_JNI_IN_NATIVE_KEEP_TLAB`, default ON since 2026-10-31 (`=0` kills;
/// `runtime_flag_default_on`), read once per VM ([`JniSwitches::read_flags`], only with the package).
pub(in crate::native::jni) fn in_native_keep_tlab_enabled() -> bool {
    true
}

/// gcj j2/jn2 (cut 2): is the keep-the-TLAB rule live on `shared`'s heap?
/// The switch (with the package), a Generational heap, and not the lock-free
/// leave (`CRATONVM_JNI_IN_NATIVE_LOCKFREE_LEAVE`, default on: its leave drops the
/// flag without the barrier lock, so a pause could read the flag up while the
/// owner already allocates). Read by the pause
/// (`gc_and_alloc::retire_in_native_kept_tlabs`) and the entry.
pub(crate) fn in_native_keep_tlab_enabled_for(shared: &SharedVm) -> bool {
    in_native_keeps_tlab(shared, &jni_switches_of(shared))
}

/// gcj j2/jn2 (cut 2): [`in_native_keep_tlab_enabled_for`] over switches in
/// hand.
///
/// Why an in-native thread may keep its TLAB (HotSpot keeps it and retires
/// every TLAB at the safepoint, `ensure_parsability`): from the entry to its
/// leave the thread never touches the TLAB (its C code reaches the heap only
/// through JNIEnv functions, which leave native first and wait out a pause; a
/// leaf window does not allocate). So the one hazard is a collection that
/// resets the young space under a live `[cursor, end)`; the collecting pause
/// retires every kept TLAB on its owner's behalf before it collects
/// (`ThreadRegistry::retire_in_native_kept_tlabs`, with the owner unable to
/// leave), and every other pause publishes the tail as reserved, as it does a
/// frozen peer's (`collect_reserved_tlab_tails`), which every linear walk
/// skips. The leave then finds either its TLAB intact (no collection in the
/// window: no re-carve, the gcf-f5j cost) or retired.
pub(in crate::native::jni) fn in_native_keeps_tlab(shared: &SharedVm, switches: &JniSwitches) -> bool {
    switches.in_native_keep_tlab
        && !switches.in_native_lockfree_leave
        && shared.mem.heap.is_generational()
}

/// gcj j2/jn2 (cut 2): the TLAB step of going into native, while still a
/// counted mutator: retire it (the default), or keep it and raise
/// `GcBlockState::in_native_tlab_kept` before the deposit raises
/// `in_blocked_region` (Release) -- [`in_native_keeps_tlab`].
pub(in crate::native::jni) fn retire_or_keep_tlab_into_native(
    shared: &SharedVm,
    switches: &JniSwitches,
    thread: &mut JvmThread,
) {
    if in_native_keeps_tlab(shared, switches) {
        thread
            .gc_block_state
            .in_native_tlab_kept
            .store(true, std::sync::atomic::Ordering::Release);
        shared.natives.jni_phase_census.note(JniPhase::TlabKept);
    } else {
        thread.tlab.retire();
    }
}

/// gcj j2/jn2 (cut 2): the leave's half, with `in_blocked_region` down (no
/// pause reads the mark from here): take the mark back. Still up = no
/// collecting pause retired the TLAB in the window, so it is intact and the
/// thread allocates on from it. A plain swap with the switch off (the mark is
/// never raised then).
#[inline]
pub(in crate::native::jni) fn reclaim_kept_tlab_after_native(
    shared: &SharedVm,
    block: &crate::threading::jvm_thread::GcBlockState,
) {
    if block
        .in_native_tlab_kept
        .swap(false, std::sync::atomic::Ordering::AcqRel)
    {
        shared.natives.jni_phase_census.note(JniPhase::TlabKeptReused);
    }
}

/// The barrier half of going into native, after the thread's
/// `in_blocked_region` went up: count it blocked, and arrive if a pause raced
/// in -- the flag went up before this check, so the pause in progress may
/// have excluded it already, and `auto` arrives only if it counted it.
///
/// gch h2/jr: under `CRATONVM_JNI_IN_NATIVE_ARRIVE_NO_WAIT=1` (with the
/// package) the arrival does not park: `GcBarrier::arrive_in_native` counts it
/// once if the census did, excludes it for the rest of the pause, and returns,
/// so the C code runs while the pause collects, as for a thread the census
/// excluded. The map `auto` returns was never applied here either: the flag is
/// up, so the pause's fold is this thread's channel, and its leave takes the
/// full wake.
pub(in crate::native::jni) fn enter_in_native_region(shared: &SharedVm, tid: ThreadId) {
    if shared.mem.gc_barrier.mark_in_native_enter() {
        if jni_switches_of(shared).in_native_arrive_no_wait {
            if shared.mem.gc_barrier.arrive_in_native(tid) {
                shared
                    .natives
                    .jni_phase_census
                    .note(JniPhase::InNativeArriveNoWait);
            }
        } else {
            let _ = shared.mem.gc_barrier.arrive_and_wait_auto(tid);
        }
    }
}

/// gcd d10/j: running -> in native again after a function entered through
/// [`ForeignJniEntry::enter_vm_only`] that left native quietly, ran no Java,
/// and saw no pause complete and no fold since the call's last full deposit
/// (the caller checked all of that, with the record's `NativeSync`, its
/// `deposit_redefinitions` and its `appended` bound).
///
/// What the deposit on the way in published is still exact for everything
/// such a function cannot change: the interpreter frames (their slots,
/// `slot_origins` kept by the quiet leave, the class owners, the published
/// trace), the compiled frames and their pins, the stashed deopt frames, the
/// shadow stack, and the thread's off-frame fields (its ordinary path writes
/// no `JvmThread` field: it allocates through the heap, mints locals in the
/// thread-local frames and logs SATB entries; the one field a JNIEnv function
/// writes, `native_pending_return`, is set through `with_jni_context`, which
/// escalates the entry to the full deposit). What it can have added is the
/// locals it minted -- the entries of the local frames past `mark`, the
/// frames' extent when it left native ([`local_frames_mark`]) -- and those
/// are appended to the snapshot: appended, never replacing, so whatever the
/// snapshot held stays covered (a deleted local's referent is over-retained
/// until the next full deposit; no root is ever lost). The SATB entries it
/// logged are flushed and the TLAB retired, as the full deposit does. Then
/// the flag goes up and the thread is counted blocked again, exactly as on
/// the full path.
///
/// Returns how many entries it appended (the caller bounds the total).
pub(in crate::native::jni) fn native_call_reenter_incremental(
    shared: &SharedVm,
    thread: &mut JvmThread,
    mark: LocalFramesMark,
) -> u32 {
    // As on the full path: no blocked thread keeps a TLAB (gcj j2/jn2: unless
    // `CRATONVM_JNI_IN_NATIVE_KEEP_TLAB` keeps it, as on the full path).
    retire_or_keep_tlab_into_native(shared, &jni_switches_of(shared), thread);
    // `deposit_root_snapshot_inner`'s first step (fork6 GC_STRESS fix): a
    // store the function made during a concurrent mark must reach the
    // collector's queue before the thread is excluded from the remark.
    shared.mem.heap.flush_thread_satb();
    let appended = {
        let mut snapshot = thread.root_snapshot.lock();
        let before = snapshot.len();
        collect_local_ref_roots_since(mark, &mut snapshot);
        u32::try_from(snapshot.len() - before).unwrap_or(u32::MAX)
    };
    // JIT round 16 wave 11 (lane jnieintr): back to the C code; before the
    // blocked flag, as on the full path ([`raise_foreign_native`]).
    raise_foreign_native(thread);
    // After the snapshot is complete, as `deposit_root_snapshot_inner` raises
    // it: from here every pause folds this thread's moves into its fixup.
    thread
        .gc_block_state
        .in_blocked_region
        .store(true, std::sync::atomic::Ordering::Release);
    enter_in_native_region(shared, thread.thread_id);
    appended
}

/// gci i16/sn (`gci-i15rb-vm-default-path-changes-review-findings-FIXED-20261017`
/// finding 6, `CRATONVM_DEBUG_SUSPEND_AT_NATIVE_TRANSITION`, default off):
/// the native -> VM transition's suspension check. Called first thing by
/// [`ForeignJniEntry::enter_from_native`] and
/// [`ForeignJniEntry::enter_leaf_window`] -- every JNIEnv function and every
/// C JVMTI function that takes the JNI entry, from a JNI native's C code or a
/// `RunAgentThread` start function in native -- BEFORE the record leaves
/// native: while a debugger or a C JVMTI agent holds this thread suspended
/// it waits here (`debug::hold_at_native_transition_while_suspended`), still
/// in native, so every pause keeps excluding it and the in-native leave's
/// fairness rule never counts it; the leave runs after the resume. HotSpot's
/// `ThreadInVMfromNative` blocks a suspended thread at the same point.
///
/// One thread-local read and one load (the VM's `suspension_in_force`) when
/// the innermost record is in native; nothing more unless some thread is
/// suspended with the switch on. The record's borrow ends before the wait.
#[cfg(feature = "experimental-debug")]
#[inline]
pub(crate) fn hold_in_native_while_suspended() {
    let target = JNI_NATIVE_CALL
        .try_with(|t| {
            let slot = t.record.try_borrow().ok()?;
            let rec = slot.as_ref().filter(|r| r.in_native)?;
            if !crate::debug::native_transition_hold_armed(&rec.shared) {
                return None;
            }
            // SAFETY: as in `ForeignJniEntry::enter_from_native`: the record's
            // `JvmThread` outlives the record; `thread_id` is an immutable
            // `Copy` field, read through the pointer without forming a
            // reference to the thread.
            let tid = unsafe { (*rec.thread).thread_id.0 };
            Some((Arc::clone(&rec.shared), tid))
        })
        .ok()
        .flatten();
    if let Some((shared, tid)) = target {
        crate::debug::hold_at_native_transition_while_suspended(&shared, tid);
    }
}

/// [`hold_in_native_while_suspended`] in a build without a debugger: nothing
/// can suspend a thread.
#[cfg(not(feature = "experimental-debug"))]
#[inline(always)]
pub(crate) fn hold_in_native_while_suspended() {}

/// gci i17/fz (i16/sn's residual; `CRATONVM_DEBUG_SUSPEND_AT_NATIVE_TRANSITION`,
/// default off): [`hold_in_native_while_suspended`] for a FOREIGN attachment
/// (`AttachCurrentThread`), whose JNIEnv calls reach the VM through
/// [`ForeignJniEntry::enter_foreign`] rather than an in-native record. Called
/// there at depth 0, before `foreign_leave_idle`: while a debugger
/// (`VirtualMachine.Suspend`, `ThreadReference.Suspend`) or a C JVMTI agent's
/// `SuspendThread` holds the thread it waits, still idle -- GC-blocked, its
/// idle deposit its roots, excluded from every pause -- and leaves idle after
/// the resume. HotSpot lists an attached thread in `GetAllThreads` once its
/// attach completes, so JDWP's suspend-all counts it, and its next JNIEnv
/// call's `ThreadInVMfromNative` blocks it. Only while the attachment is
/// actually in its blocked region (a counted attachment would stall pauses).
/// One load (`suspension_in_force`) when nothing is suspended.
#[cfg(feature = "experimental-debug")]
#[inline]
pub(in crate::native::jni) fn hold_foreign_idle_while_suspended(shared: &SharedVm) {
    if !crate::debug::native_transition_hold_armed(shared) {
        return;
    }
    let tid = with_foreign_thread(|jt| {
        jt.gc_block_state
            .in_blocked_region
            .load(std::sync::atomic::Ordering::Acquire)
            .then_some(jt.thread_id.0)
    })
    .flatten();
    if let Some(tid) = tid {
        crate::debug::hold_at_native_transition_while_suspended(shared, tid);
    }
}

/// [`hold_foreign_idle_while_suspended`] in a build without a debugger.
#[cfg(not(feature = "experimental-debug"))]
#[inline(always)]
pub(in crate::native::jni) fn hold_foreign_idle_while_suspended(_shared: &SharedVm) {}

/// In native -> running: wait out a pause in progress, clear the flag under
/// the barrier lock, apply the fixups every collection of the window
/// accumulated (frames, native pins, JNI local frames, compiled frames) and
/// re-deposit -- `NativeContextImpl::end_blocking_region`.
///
/// gcd d5/f: `sync` is what [`NativeSync::read`] answered just before the
/// call went into native. When the barrier finds it unchanged at the moment
/// it clears the flag (`GcBarrier::leave_blocked_region_flagged_if`: under its
/// lock, no pause in progress or able to begin), no pause completed and no
/// relocation was folded into this thread for the whole window, so the full
/// wake has nothing to do: the fixup, the native-slot captures are empty, every
/// `slot_origins` entry still reads its origin, and the snapshot deposited on
/// the way in still describes the thread's frames exactly (a thread in native
/// changes none of them). That wake -- chiefly its snapshot refresh, a second
/// full deposit -- is skipped; only a class redefinition in the window still
/// needs the frames converted. Anything else takes the full wake as before.
///
/// gcd d10/j: the counter release and the flag clear are ONE barrier-lock
/// hold when no pause is in progress (`GcBarrier::leave_in_native_if`), and
/// no blocked-stack summary is withdrawn (none was published,
/// `mark_in_native_enter`). `keep_origins` (a function entered through
/// [`ForeignJniEntry::enter_vm_only`]) keeps `slot_origins` on a quiet leave
/// for the incremental re-entry. Returns how the leave went.
///
/// gci i48 lane jl48 (`ja11_w_tag`, i10al item 4): the full wake keeps the
/// snapshot the window's pauses folded when the thread's frames are all
/// interpreted (`NativeContextImpl::check_post_block_gc_in_native`), instead
/// of a refresh deposit per JNIEnv call; with `keep_origins` it leaves
/// `slot_origins` rebased, so the function's drop may go back incrementally
/// ([`NativeLeave::KeptWake`]).
pub(in crate::native::jni) fn native_call_leave_native(
    shared: &SharedVm,
    thread: &mut JvmThread,
    sync: NativeSync,
    keep_origins: bool,
) -> NativeLeave {
    let census = &shared.natives.jni_phase_census;
    let mut clock = census.start();
    // JIT round 16 wave 11 (lane jnieintr): the C code is done (or calls into
    // the VM); lowered while the thread is still blocked, so the registry
    // never exempts a running thread ([`raise_foreign_native`]). Unconditional:
    // a plain store, and a stale flag must never survive the bracket.
    thread
        .gc_block_state
        .foreign_native
        .store(false, std::sync::atomic::Ordering::Release);
    // gci i20/jt (`CRATONVM_JNI_IN_NATIVE_LOCKFREE_LEAVE`, opt-in): no barrier
    // lock when no pause is in progress; the production census counts a
    // thread whose `in_native_leaving` is up (`GcBlockState::census_blocked`).
    let block = &thread.gc_block_state;
    let unchanged = || NativeSync::read(shared, block) == sync;
    // gcj j2/jn2 (cut 1): the VM in hand's switches when
    // `CRATONVM_JNI_SWITCHES_ONCE_PER_CALL` is on.
    let switches = jni_switches_of(shared);
    let lockfree = if switches.switches_once_per_call {
        switches.in_native_lockfree_leave
    } else {
        jni_switches().in_native_lockfree_leave
    };
    let quiet = if lockfree {
        if shared.mem.gc_barrier.lockfree_blocked_enabled() {
            census.note(JniPhase::LeaveLockfree);
        }
        shared.mem.gc_barrier.leave_in_native_lockfree_if(
            thread.thread_id,
            &block.in_blocked_region,
            &block.in_native_leaving,
            unchanged,
        )
    } else {
        shared.mem.gc_barrier.leave_in_native_if(
            thread.thread_id,
            &block.in_blocked_region,
            unchanged,
        )
    };
    census.lap(JniPhase::LeaveBarrier, &mut clock);
    if quiet {
        // Every entry still reads `cur == orig`; the next flag-raising
        // deposit records a fresh set. Cleared rather than left, so no later
        // wake can apply them to other frames -- unless the caller re-enters
        // on this very deposit (`keep_origins`), and then it clears them
        // itself on every other way out.
        if !keep_origins {
            thread.gc_block_state.slot_origins.lock().clear();
            // gch h4/bp: the band goes with the origins: kept only for the
            // incremental re-entry on this very deposit.
            clear_in_native_band(&thread.gc_block_state);
        }
        // gcj j2/jn2 (cut 2): the flag is down, so no pause retires a kept
        // TLAB from here on; it is this thread's again.
        reclaim_kept_tlab_after_native(shared, &thread.gc_block_state);
        crate::runtime::interpreter::obsolete_frames::convert_obsolete_frames_if_redefined(
            shared, thread,
        );
        census.stop(JniPhase::LeaveQuietTail, clock);
        return NativeLeave::Quiet;
    }
    // The flag is still up: the full wake clears it itself.
    let kept = crate::vm::NativeContextImpl { shared, thread }
        .check_post_block_gc_in_native(keep_origins);
    // gcj j2/jn2 (cut 2): the flag is down now (a pause of the window may
    // have retired a kept TLAB already; then the mark is down too).
    reclaim_kept_tlab_after_native(shared, &thread.gc_block_state);
    // gch h4/bp: the flag is down now; the call's next entry into native
    // records its band again (`native_call_enter_native`).
    clear_in_native_band(&thread.gc_block_state);
    if kept {
        census.note(JniPhase::LeaveWakeKept);
    }
    census.stop(JniPhase::LeaveFullWake, clock);
    if kept {
        NativeLeave::KeptWake
    } else {
        NativeLeave::FullWake
    }
}

/// gci i48 lane jl48: how [`native_call_leave_native`] took a call out of
/// native.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(in crate::native::jni) enum NativeLeave {
    /// No pause completed and nothing was folded since the deposit: no wake.
    Quiet,
    /// The full wake ran and kept the snapshot the window's pauses folded
    /// (`check_post_block_gc_in_native`): it names the thread's unchanged
    /// frames at their objects' current addresses, as a refresh would, and,
    /// for a `keep_origins` leave, `slot_origins` track the same slots from
    /// there.
    KeptWake,
    /// The full wake ran and refreshed the snapshot.
    FullWake,
}

/// gch h4/bp: withdraw the compiled band a JNI in-native thread left to the
/// pause. Called by the owning thread only once `in_blocked_region` is down
/// (a pause reads the band only while the flag is up), so no pause is
/// reading it.
pub(in crate::native::jni) fn clear_in_native_band(block: &crate::threading::jvm_thread::GcBlockState) {
    store_in_native_band(block, None);
    // gci i24/bp: the proven depth goes with the band.
    block
        .in_native_proven_jit_depth
        .store(0, std::sync::atomic::Ordering::Relaxed);
}

/// gch h4/bp: record `band` (`None`: no band, `(0, 0)`) as the compiled band
/// the pause scans for this thread. The owning thread only, with
/// `in_blocked_region` down; the flag's Release store publishes it.
pub(in crate::native::jni) fn store_in_native_band(
    block: &crate::threading::jvm_thread::GcBlockState,
    band: Option<(usize, usize)>,
) {
    let (lo, hi) = band.unwrap_or((0, 0));
    block
        .in_native_band_lo
        .store(lo, std::sync::atomic::Ordering::Relaxed);
    block
        .in_native_band_hi
        .store(hi, std::sync::atomic::Ordering::Relaxed);
}

/// gch h4/bp (`CRATONVM_JNI_IN_NATIVE_BAND_AT_PAUSE`): is the switch on (with
/// the package and its no-signal mark) on `shared`? Read by the pause's band
/// scan (`gc_and_alloc::append_in_native_band_roots`) to stay off its registry
/// walk entirely when no thread can have a band. (gci i27/kd: the VM's own
/// copy, for the collector's callers, which hold it but run outside any JNI
/// context. gcj j2/jn2: the context-resolving twin
/// `in_native_band_at_pause_enabled()` is gone: its one caller, the dispatch,
/// reads the switches it resolved itself.)
#[inline]
pub(crate) fn in_native_band_at_pause_enabled_for(shared: &SharedVm) -> bool {
    jni_switches_of(shared).in_native_band_at_pause
}

/// gch h4/bp: the band `[lo, top)` a JNI native call leaves to the pause, from
/// the SP `sp` of a frame below the dispatch, the address `spill_at` of the
/// dispatch's callee-saved register spill and the thread's stack top: `lo` is
/// `sp` rounded down to a word, the spill must lie inside the band (one whole
/// spill below `top`), and the band must pass the lazy claim's own bounds
/// (`jvm_thread::lazy_native_band_ok`). Pure.
pub(in crate::native::jni) fn in_native_band_from(sp: usize, spill_at: usize, top: usize) -> Option<(usize, usize)> {
    let lo = sp & !7;
    let spill_end = spill_at.checked_add(std::mem::size_of::<[usize; 8]>())?;
    (spill_at >= lo
        && spill_end <= top
        && crate::threading::jvm_thread::lazy_native_band_ok(lo, top))
    .then_some((lo, top))
}

/// gch h4/bp (`gcg-g4jn-proposal-in-native-band-scanned-at-the-pause-DONE-20261015`,
/// `CRATONVM_JNI_IN_NATIVE_BAND_AT_PAUSE`, on with the package since gci i1/jn): the compiled band this JNI
/// native call leaves to the pause, or `None` (the deposit scans its compiled
/// frames itself, as before). Called by [`JniNativeCall::enter_engaged`] on
/// the dispatching thread, below the dispatch's frame; `spill_at` is the
/// address of the dispatch's `CalleeSavedSpill`.
///
/// Only with the switch (the package and its no-signal mark), on x86_64
/// Linux / Windows (where the spill captures), on a Generational heap and
/// under a compiled caller (`current_thread_jit_depth() > 0`: the cost this
/// removes is the compiled frames'; with none the deposit is cheap).
///
/// Why the pause may scan `[lo, top)` instead of the deposit scanning the
/// compiled frames:
///
/// * **Frozen.** The thread is in native: its C code runs below `lo` and
///   cannot return past the dispatch without `native_call_leave_native`, which
///   waits out a pause in progress (`GcBarrier::leave_in_native_if`); a
///   JNIEnv function that leaves native does the same, and the incremental
///   re-entry raises the flag over the same band. So while a pause holds the
///   flag up, every frame above the C code is as the thread left it, and the
///   thread cannot exit (its stack stays mapped). Words between `lo` and the
///   C frames may be rewritten by the C code; they are read volatile, and a
///   garbage word that the probe accepts only over-retains.
/// * **Complete.** The band is every compiled and Rust frame of the thread
///   above its C code, up to the stack top -- a superset of the deposit's
///   chain scan `[deposit SP, outermost entry)` and of its A5 rescue above the
///   chain. A value the compiled caller keeps only in a callee-saved register
///   is in the dispatch's spill (captured in the frame that makes the C call,
///   before this entry raised anything) or was saved by the prologue of a
///   frame between the caller and the dispatch: inside the band either way.
///   The interpreter frames, the off-frame roots, the JNI local frames and the
///   shadow stack are still deposited.
/// * **Never relocated under.** The pause marks the cycle's moving-young
///   coverage incomplete (`CROSS_THREAD_JIT_PEER`), so a Generational
///   collection of the window runs non-moving; the band words are roots, i.e.
///   pins of the non-moving sweep's selective promotion, as the deposit's scan
///   words were. (gci i24/bp: `compiled_frames_live` alone has not made a
///   cycle non-moving since moving young runs under live compiled frames; the
///   coverage mark is what does. Under the opt-in
///   `CRATONVM_JNI_IN_NATIVE_PROVEN_DEPTH_CREDIT` a band whose deposit proved
///   its chain is credited instead and its young words go into the pause's
///   young pin ledger, so the pinned in-place copy keeps every object a band
///   word names in place: `gc_and_alloc::credit_in_native_proven_bands`.)
///   On G1 (gci i5g/g1r, opt-in [`in_native_band_at_pause_g1_enabled`]) the
///   pause PINS every band root for its cycle instead
///   (`gc_and_alloc::append_in_native_band_roots`), before G1 chooses its
///   collection set, which is how a frozen peer's words are kept in place.
///
/// gci i28/nb (`CRATONVM_JNI_IN_NATIVE_BAND_NARROW`, default ON, `=0` kills; P5/nb,
/// `gch-h4bp-proposal-narrow-the-in-native-band-to-the-chain-20261009`): the
/// band's top is the outermost JIT entry SP instead of the stack top when a
/// clean A5 verdict covers the stack above the chain
/// (`conservative_roots::in_native_band_chain_top`), i.e. exactly the band-off
/// deposit's conservative reach; the spill must still lie inside the narrowed
/// band (else the whole band is kept). See [`in_native_band_narrowed_from`].
pub(in crate::native::jni) fn in_native_band_at_pause(shared: &SharedVm, spill_at: usize) -> Option<(usize, usize)> {
    let switches = jni_switches_of(shared);
    if !switches.in_native_band_at_pause
        || !cfg!(all(
            target_arch = "x86_64",
            any(target_os = "windows", target_os = "linux")
        ))
        || crate::jit::conservative_roots::current_thread_jit_depth() == 0
        || !in_native_band_heap_admits(shared, &switches)
    {
        return None;
    }
    let sp = crate::jit::conservative_roots::current_stack_pointer();
    let top = crate::jit::conservative_roots::in_native_band_top(sp)?;
    let wide = in_native_band_from(sp, spill_at, top)?;
    // gcj j1/jn: the VM's latched switch (`JniSwitches::in_native_band_narrow`),
    // not a flag-snapshot lookup per call.
    if !switches.in_native_band_narrow {
        return Some(wide);
    }
    // gci i28/nb: the narrowing's own cost (the memo read, or a fresh probe
    // of the stack above the chain) and its engagement, in the JNI phase
    // census (`CRATONVM_DBG_JNI_PHASE=1`).
    let census = &shared.natives.jni_phase_census;
    let clock = census.start();
    let why = crate::jit::conservative_roots::in_native_band_chain_top_why(wide.0, top);
    census.stop(JniPhase::DepBandNarrowProbe, clock);
    // gcj j2/jn2: every refusal counted by its reason (j1's NB rows narrowed
    // none of 2605 probed calls and could not say why).
    if let Err(refusal) = why {
        use crate::jit::conservative_roots::ChainBandRefusal;
        census.note(match refusal {
            ChainBandRefusal::NoChain => JniPhase::DepBandNarrowNoChain,
            ChainBandRefusal::Truncated => JniPhase::DepBandNarrowTruncated,
            ChainBandRefusal::JitAbove => JniPhase::DepBandNarrowJitAbove,
        });
    }
    let chain = why.ok();
    let (band, narrowed) = in_native_band_narrowed_from(sp, spill_at, wide, chain.map(|c| c.0));
    if narrowed {
        census.note(JniPhase::DepBandNarrowed);
        if matches!(
            chain,
            Some((_, crate::jit::conservative_roots::ChainBandProof::Memo))
        ) {
            census.note(JniPhase::DepBandNarrowMemo);
        }
    } else if chain.is_some() {
        census.note(JniPhase::DepBandNarrowSpillOutside);
    }
    Some(band)
}

/// gci i28/nb: the band a call leaves to the pause, given its whole band
/// `wide` (`[lo, stack top)`) and the chain top `chain_hi` that
/// `conservative_roots::in_native_band_chain_top` proved (if any): the band
/// `[lo, chain_hi)` when `chain_hi` narrows it and the narrowed band still
/// passes [`in_native_band_from`] (the dispatch's callee-saved spill inside,
/// the lazy claim's bounds), else `wide`. The flag says which. Pure.
pub(in crate::native::jni) fn in_native_band_narrowed_from(
    sp: usize,
    spill_at: usize,
    wide: (usize, usize),
    chain_hi: Option<usize>,
) -> ((usize, usize), bool) {
    match chain_hi
        .filter(|&hi| hi > wide.0 && hi < wide.1)
        .and_then(|hi| in_native_band_from(sp, spill_at, hi))
    {
        Some(band) => (band, true),
        None => (wide, false),
    }
}

/// gci i28/nb (P5/nb, `gch-h4bp-proposal-narrow-the-in-native-band-to-the-chain-20261009`):
/// `CRATONVM_JNI_IN_NATIVE_BAND_NARROW`, default ON (`runtime_flag_default_on`,
/// `=0` kills).
///
/// WARNING gck-jn2-user-flip-20261025: default ON, was opt-in. The j1 rows
/// that kept it off read the census's `avg_ns` field (`dep_band_narrowed=0ns/N`)
/// as the count; it narrows every banded call of the NB probes, halves the
/// band words per pause, and every soundness row stays SAME as HotSpot
/// (`docs/feature-designs/generative-gc-architecture.md`).
/// With the band at the pause (`CRATONVM_JNI_IN_NATIVE_BAND_AT_PAUSE`, on with
/// the package; Generational, and G1 under its own default-on switch), the
/// band ends at the outermost JIT entry instead of the stack top when the
/// stack above the chain is proven free of compiled frames
/// ([`in_native_band_at_pause`]). gcj j1/jn: read once per VM, by
/// [`JniSwitches::read_flags`] (`JniSwitches::in_native_band_narrow`); it was
/// read per banded call (no process static either way).
pub(in crate::native::jni) fn in_native_band_narrow_enabled() -> bool {
    true
}

/// gci i5g/g1r (the G1 half of `gcf-f1j-in-native-deposit-runs-every-band-claim-on-scalar-words`,
/// hunk 1 of `docs/feature-designs/zgc-architecture.md`):
/// may this heap leave a JNI in-native thread's compiled band to the pause?
/// Generational always (gch h4/bp). G1 only with
/// [`in_native_band_at_pause_g1_enabled`]: every pause that reads the
/// parked threads' snapshots on G1 scans the bands too and pins what it
/// finds for its cycle. ZGC never (out of the gci round's scope; its slide
/// would need the same pins at `relocate_stw_admitted`).
///
/// t1 w8 JNICUT: the G1 switch from `switches` (the VM in hand's latch,
/// `JniSwitches::in_native_band_at_pause_g1`), not a flag lookup per call.
pub(in crate::native::jni) fn in_native_band_heap_admits(shared: &SharedVm, switches: &JniSwitches) -> bool {
    let heap = &shared.mem.heap;
    heap.is_generational() || (heap.is_g1() && switches.in_native_band_at_pause_g1)
}

/// gci i5g/g1r: `CRATONVM_JNI_IN_NATIVE_BAND_AT_PAUSE_G1` -- default ON since
/// the vi6-g1r verdict (gci orchestrator, 2026-10-13: B1 by (a1), B2, B3 c1
/// PB/P 0.45); `=0` kills (`runtime_flag_default_on`). With the package and
/// `CRATONVM_JNI_IN_NATIVE_BAND_AT_PAUSE` (on with the package), a G1 heap's
/// in-native deposit under a compiled caller skips its compiled-frame scan
/// (`dep_jit_scan`) and the pause scans and pins the band. Read only on a G1
/// heap with a compiled caller, i.e. on the package's G1 in-native path.
pub(crate) fn in_native_band_at_pause_g1_enabled() -> bool {
    true
}

/// gci i24/bp (`gch-h4bp-review-findings-RETIRED-UNFIXED-20261018` item 1, proposal P2/bp):
/// `CRATONVM_JNI_IN_NATIVE_PROVEN_DEPTH_CREDIT=1`, default OFF
/// (`runtime_flag_on`). With the package and
/// `CRATONVM_JNI_IN_NATIVE_BAND_AT_PAUSE` (both on by default), a
/// Generational heap's in-native deposit under a compiled caller runs its
/// coverage proof again and records the depth it proved
/// (`GcBlockState::in_native_proven_jit_depth`); each pause then credits that
/// depth to the cross-thread coverage account and reads the thread's band
/// into the young pin ledger (`gc_and_alloc::credit_in_native_proven_bands`),
/// so a JIT-warm program with a native in progress is no longer refused every
/// moving young cycle (`CROSS_THREAD_JIT_PEER`). Read per call only on that
/// path (a band, a Generational heap). Never on G1 or ZGC.
///
/// gcj j1/jn: this is the flag reader, called once per VM by
/// [`JniSwitches::read_flags`]; the call and pause paths read the latched
/// value ([`in_native_proven_depth_credit_enabled_for`]).
pub(in crate::native::jni) fn in_native_proven_depth_credit_enabled() -> bool {
    false
}

/// gcj j1/jn: [`in_native_proven_depth_credit_enabled`] of `shared`, latched
/// once per VM (`JniSwitches::in_native_proven_depth_credit`). Read by the
/// in-native entry ([`in_native_proven_depth_credit_admits`]) and by the
/// pause (`gc_and_alloc::in_native_credit_stamp`).
#[inline]
pub(crate) fn in_native_proven_depth_credit_enabled_for(shared: &SharedVm) -> bool {
    jni_switches_of(shared).in_native_proven_depth_credit
}

/// gci i24/bp: does this call's deposit prove for the credit? Only on a
/// Generational heap (the only collector whose moving young cycle reads the
/// coverage account and the young pin ledger) with the switch on.
pub(in crate::native::jni) fn in_native_proven_depth_credit_admits(shared: &SharedVm) -> bool {
    shared.mem.heap.is_generational() && in_native_proven_depth_credit_enabled_for(shared)
}

/// JIT round 21 wave 19 (lane jnilazy, FF3-3): the stack band `[lo, top)` a
/// JNI native call in the LAZY in-native state leaves to a pause that claims
/// the thread, from the address `spill_at` of the dispatch's
/// `CalleeSavedSpill`: `lo` is the SP here, below the dispatch's frame, and
/// the band must hold the spill and pass the claim's own bounds
/// ([`in_native_band_from`], the gch h4/bp rule). Called by
/// [`JniNativeCall::enter_engaged`] on the dispatching thread. `None`: the
/// platform cannot say, and the call takes the deposit.
///
/// Why the band is the whole machine-word story of the call: every compiled
/// and Rust frame above the C code is in it (they cannot change while the
/// thread is claimed: the C code cannot return past the dispatch without
/// [`native_call_leave_lazy`], which waits out the claim), and a value a
/// compiled caller keeps only in a callee-saved register is in the spill
/// (captured in the frame that makes the C call, before the state is
/// published) or in the save area of a frame between that caller and the
/// dispatch -- the FFM downcall's argument (`panama_libffi::CalleeSavedSpill`).
#[inline]
pub(in crate::native::jni) fn in_native_lazy_band(spill_at: usize) -> Option<(usize, usize)> {
    let sp = crate::jit::conservative_roots::current_stack_pointer();
    let top = crate::jit::conservative_roots::in_native_band_top(sp)?;
    in_native_band_from(sp, spill_at, top)
}

/// JIT round 21 wave 19 (lane jnilazy, FF3-3): running -> in native, LAZILY,
/// for `thread`'s JNI native call: `NativeContextImpl::begin_native_region_lazy_with_locals`
/// (the FFM downcall's transition: SATB flushed, the claim's eligibility words
/// written, ONE compare-exchange of `GcBlockState::lazy_native` to `IN`). No
/// deposit, no TLAB retire, no barrier lock; the thread stays a counted
/// mutator, RUNNABLE, and a pause claims it instead of waiting for it. `false`:
/// nothing done (no claim is possible here: the target, the heap, an
/// unreadable JIT chain, a band out of bounds); the caller takes the deposit.
pub(in crate::native::jni) fn native_call_enter_lazy(
    shared: &SharedVm,
    thread: &mut JvmThread,
    band: (usize, usize),
) -> bool {
    let mut ctx = crate::vm::NativeContextImpl { shared, thread };
    cratonvm_native_api::NativeThreadAccess::begin_native_region_lazy_with_locals(
        &mut ctx, band.0, band.1,
    )
}

/// JIT round 21 wave 19 (lane jnilazy, FF3-3): in native -> running for a call
/// [`native_call_enter_lazy`] took into native:
/// `NativeContextImpl::end_native_region_lazy` -- one compare-exchange back
/// to `OFF`, which waits while a pause that claimed the thread runs (and,
/// when a no-roots pause could not claim it, leaves through the barrier so
/// the thread stays out of the heap until that pause ends); then obsolete
/// frames are converted after a redefinition. HotSpot's
/// `_thread_in_native_trans` check: the thread touches nothing of the heap
/// before this returns.
pub(in crate::native::jni) fn native_call_leave_lazy(shared: &SharedVm, thread: &mut JvmThread) {
    let mut ctx = crate::vm::NativeContextImpl { shared, thread };
    cratonvm_native_api::NativeThreadAccess::end_native_region_lazy(&mut ctx);
}

/// JIT round 21 wave 19 (lane jnilazy, FF3-3): the innermost record of
/// `thread` can no longer use the lazy state: it is the package's from here
/// on. Its `sync` and redefinition count are read afresh (before the deposit
/// the caller makes next), and its gch h4/bp band returned for that deposit.
pub(in crate::native::jni) fn demote_lazy_record(thread: *mut JvmThread) -> Option<(usize, usize)> {
    // SAFETY: a `Copy` field of the live `JvmThread` (see `enter_engaged`).
    let redefinitions_now = unsafe { (*thread).redefinitions_seen };
    JNI_NATIVE_CALL
        .try_with(|t| {
            let mut slot = t.record.try_borrow_mut().ok()?;
            let rec = slot.as_mut().filter(|r| r.thread == thread)?;
            rec.lazy = None;
            // SAFETY: see `NativeCallRecord::block_state`.
            rec.sync = NativeSync::read(&rec.shared, unsafe { &*rec.block_state });
            rec.deposit_redefinitions = redefinitions_now;
            rec.appended = 0;
            rec.band
        })
        .ok()
        .flatten()
}

/// The in-native bracket of one JNI native call ([`dispatch_jni_native`]);
/// see the section comment above. Inert unless the flag is on and the
/// call qualifies.
#[must_use = "the call leaves native when the guard drops"]
pub(crate) struct JniNativeCall {
    /// This guard installed a record and must remove it (leaving native if
    /// the record is still in native).
    pub(in crate::native::jni) engaged: bool,
    /// The enclosing call's record, put back on drop.
    pub(in crate::native::jni) prev: Option<NativeCallRecord>,
}

impl JniNativeCall {
    /// Take the calling thread into native for its JNI native call.
    /// `escapes_before` is [`raw_local_escapes`] read before the receiver and
    /// the arguments were recorded: a raw one among them keeps the call
    /// counted.
    #[inline]
    pub(crate) fn enter(escapes_before: u64) -> Self {
        if !native_transitions_active() {
            return JniNativeCall {
                engaged: false,
                prev: None,
            };
        }
        Self::enter_engaged(escapes_before, None, None).unwrap_or(JniNativeCall {
            engaged: false,
            prev: None,
        })
    }

    /// gch h4/bp: [`Self::enter`] for a dispatch that captured its caller's
    /// callee-saved registers in `spill`, in the frame that makes the C call
    /// and before this entry (`dispatch_jni_native`), and keeps it live until
    /// the guard drops. With `CRATONVM_JNI_IN_NATIVE_BAND_AT_PAUSE` such a call
    /// may leave its compiled band to the pause ([`in_native_band_at_pause`]);
    /// `None` is [`Self::enter`].
    #[inline]
    pub(crate) fn enter_banded(
        escapes_before: u64,
        spill: Option<&cratonvm_native_builtins::panama_libffi::CalleeSavedSpill>,
    ) -> Self {
        if !native_transitions_active() {
            return JniNativeCall {
                engaged: false,
                prev: None,
            };
        }
        let spill_at = spill.map(|s| s as *const _ as usize);
        Self::enter_engaged(escapes_before, spill_at, None).unwrap_or(JniNativeCall {
            engaged: false,
            prev: None,
        })
    }

    /// gcj j2/jn2 (cut 1, `CRATONVM_JNI_SWITCHES_ONCE_PER_CALL`):
    /// [`Self::enter_banded`] over the switches the dispatch resolved, so the
    /// entry resolves none itself. Same answers.
    #[inline]
    pub(crate) fn enter_banded_with(
        switches: &JniSwitches,
        escapes_before: u64,
        spill: Option<&cratonvm_native_builtins::panama_libffi::CalleeSavedSpill>,
    ) -> Self {
        if !native_transitions_active_with(switches) {
            return JniNativeCall {
                engaged: false,
                prev: None,
            };
        }
        let spill_at = spill.map(|s| s as *const _ as usize);
        Self::enter_engaged(escapes_before, spill_at, Some(switches)).unwrap_or(JniNativeCall {
            engaged: false,
            prev: None,
        })
    }

    /// `switches`: the dispatch's resolved switches (gcj j2/jn2, cut 1), or
    /// `None` to resolve them here as before.
    #[inline(never)]
    pub(in crate::native::jni) fn enter_engaged(
        escapes_before: u64,
        spill_at: Option<usize>,
        switches: Option<&JniSwitches>,
    ) -> Option<Self> {
        let indirect_here = match switches {
            Some(sw) => locals_are_indirect_here_with(sw),
            None => locals_are_indirect_here(),
        };
        if !indirect_here || raw_local_escapes() != escapes_before {
            return None;
        }
        // JIT round 16 wave 12 (lane inpkg): inside an FFM downcall (this
        // native was called from an up-call's Java), an FFM stub the C code
        // calls is entered through that downcall's context
        // (`panama_upcall::upcall_entry` -> `with_active_context`), which
        // knows nothing of this record and would run the up-call's Java with
        // the thread still in native. Such a call stays counted, as before
        // the package. One thread-local read.
        if cratonvm_native_builtins::panama_libffi::has_active_downcall_context()
            && switches.map_or_else(|| jni_switches().in_native_doors, |sw| sw.in_native_doors)
        {
            return None;
        }
        let shared = JNI_SHARED_VM
            .try_with(|c| c.try_borrow().ok().and_then(|b| b.clone()))
            .ok()
            .flatten()?;
        let thread = JNI_THREAD
            .try_with(|c| c.get())
            .unwrap_or(std::ptr::null_mut()) as *mut JvmThread;
        if thread.is_null() {
            return None;
        }
        // SAFETY: `JNI_THREAD` is the dispatching thread's live `JvmThread`,
        // installed by the dispatch's `JniContextGuard` for the whole call;
        // only an atomic is read here.
        let blocked = unsafe { &*thread }
            .gc_block_state
            .in_blocked_region
            .load(std::sync::atomic::Ordering::Acquire);
        if blocked {
            // Not a counted mutator to begin with (never expected here).
            return None;
        }
        // gcd d5/f: see `NativeCallRecord::block_state` / `::sync`. Read
        // before the deposit, so a pause completing during it counts.
        // SAFETY: as above; the `Arc` field is read, not the thread mutated.
        let block_state = Arc::as_ptr(&unsafe { &*thread }.gc_block_state);
        // SAFETY: `block_state` points into the live `JvmThread` (above).
        let sync = NativeSync::read(&shared, unsafe { &*block_state });
        // SAFETY: as above; a `Copy` field of the live `JvmThread`.
        let deposit_redefinitions = unsafe { (*thread).redefinitions_seen };
        // gce e1/j: the one `Arc` taken above moves into the record (it was
        // cloned a second time per call); the transition below borrows the VM
        // through this pointer, which the record -- and the dispatch's own
        // `JNI_SHARED_VM` context -- keep alive until this guard drops.
        let shared_ptr = Arc::as_ptr(&shared);
        // gch h4/bp: decided here, below the dispatch's frame (its spill).
        let band = spill_at.and_then(|at| in_native_band_at_pause(&shared, at));
        // JIT round 21 wave 19 (lane jnilazy, FF3-3; JNILAZY21R-1): the lazy
        // band, when the call may go into C lazily (with the switch and a
        // spill). When local frames are non-empty (e.g. an instance native's
        // receiver or object arguments), `begin_native_region_lazy_with_locals`
        // sets `locals_empty = false` so a claiming pause takes the non-precise
        // pinned claim and reads the published `JNI_LOCAL_FRAMES`.
        let lazy_band = if jni_switches().in_native_lazy {
            spill_at.and_then(in_native_lazy_band)
        } else {
            None
        };
        let prev = JNI_NATIVE_CALL
            .try_with(|t| {
                t.record.try_borrow_mut().ok().map(|mut slot| {
                    slot.replace(NativeCallRecord {
                        shared,
                        thread,
                        in_native: true,
                        raw_escapes: escapes_before,
                        block_state,
                        sync,
                        deposit_redefinitions,
                        appended: 0,
                        band,
                        lazy: lazy_band,
                    })
                })
            })
            .ok()
            .flatten()?;
        // JIT round 21 wave 19 (lane jnilazy, FF3-3): the lazy state first;
        // when the context declines it, the record goes back to the package's
        // deposit for the whole call.
        if let Some(lazy) = lazy_band {
            // SAFETY: as below.
            if native_call_enter_lazy(unsafe { &*shared_ptr }, unsafe { &mut *thread }, lazy) {
                return Some(JniNativeCall {
                    engaged: true,
                    prev,
                });
            }
            let _ = demote_lazy_record(thread);
        }
        // SAFETY: as above; the `&mut` lives only for the transition, while
        // the interpreter that holds the thread is suspended in this call.
        // `shared_ptr`: the record just installed owns the `Arc`, and only
        // this guard's drop removes it.
        native_call_enter_native(unsafe { &*shared_ptr }, unsafe { &mut *thread }, band);
        Some(JniNativeCall {
            engaged: true,
            prev,
        })
    }
}

impl Drop for JniNativeCall {
    fn drop(&mut self) {
        if !self.engaged {
            return;
        }
        let prev = self.prev.take();
        let mine = JNI_NATIVE_CALL
            .try_with(|t| {
                t.record
                    .try_borrow_mut()
                    .ok()
                    .and_then(|mut slot| std::mem::replace(&mut *slot, prev))
            })
            .ok()
            .flatten();
        if let Some(rec) = mine {
            if rec.in_native && rec.lazy.is_some() {
                // JIT round 21 wave 19 (lane jnilazy, FF3-3): out of the lazy
                // state before the result is decoded.
                // SAFETY: see `enter_engaged`.
                native_call_leave_lazy(&rec.shared, unsafe { &mut *rec.thread });
            } else if rec.in_native {
                // SAFETY: see `enter_engaged`.
                let _ = native_call_leave_native(
                    &rec.shared,
                    unsafe { &mut *rec.thread },
                    rec.sync,
                    false,
                );
            }
        }
    }
}

/// Take (read + clear) the pending JNI exception, if any.
///
/// Returns `Some(raw_value)` if an exception was set via `Throw` or `ThrowNew`,
/// and clears it so subsequent calls return `None`. The raw value is the
/// JNI-level handle (u64); `u64::MAX` is only a fallback marker when a JNI
/// helper could not materialise a Java exception because no VM thread context
/// was available.
pub fn take_jni_pending_exception() -> Option<u64> {
    JNI_PENDING_EXCEPTION.with(|cell| {
        let val = cell.get();
        if val != 0 {
            cell.set(0);
            Some(val)
        } else {
            None
        }
    })
}

/// Publish a real pending JNI exception and keep it in the VM's GC-updated
/// native-return root slot until the interpreter observes it.  The raw JNI
/// handle remains available to `ExceptionOccurred`, while `native_pending_return`
/// is the authoritative (and moving-GC-safe) reference at native return.
pub(in crate::native::jni) fn set_jni_pending_exception_object(exception: ObjectRef) {
    let handle = obj_to_jobject(exception);
    JNI_PENDING_EXCEPTION.with(|cell| cell.set(handle));
    let _ = with_jni_context(|_shared, thread| {
        thread.native_pending_return = Some(exception);
    });
}

/// Access the VM context from within a JNI function. Returns None if not set.
///
/// The `Arc<SharedVm>` in TLS guarantees the VM is alive for the entire
/// duration of the closure — no raw-pointer cast, no use-after-free risk.
pub(in crate::native::jni) fn with_shared_vm<F, R>(f: F) -> Option<R>
where
    F: FnOnce(&SharedVm) -> R,
{
    JNI_SHARED_VM.with(|c| {
        let borrow = c.borrow();
        match borrow.as_ref() {
            None => None,
            Some(arc) => {
                let gen_before = JNI_CONTEXT_GENERATION.with(|g| g.get());
                let result = f(arc);
                let gen_after = JNI_CONTEXT_GENERATION.with(|g| g.get());
                if gen_before != gen_after {
                    // Nesting is legal and handled: `JniContextGuard` saves and
                    // restores rather than clearing, so a nested native call
                    // leaves this thread's context exactly as it found it. The
                    // generation still moves (twice), which is why this is a
                    // debug note and not a warning — it fires on every correct
                    // native -> Java -> native chain.
                    tracing::debug!(
                        "JNI context generation changed during callback ({} -> {}) — a nested native call, which restore-on-exit handles",
                        gen_before,
                        gen_after
                    );
                }
                Some(result)
            }
        }
    })
}

/// Access both SharedVm and JvmThread from within a JNI function.
/// The JvmThread borrow is exclusive for the duration of the closure.
/// Returns None if either context is not set.
pub(in crate::native::jni) fn with_jni_context<F, R>(f: F) -> Option<R>
where
    F: FnOnce(&SharedVm, &mut JvmThread) -> R,
{
    // gcd d5/f: a leaf JNIEnv function that got here (to raise) leaves its
    // leaf window for the full transition first. One thread-local read.
    escalate_leaf_window();
    let shared_arc: Option<Arc<SharedVm>> = JNI_SHARED_VM.with(|c| c.borrow().clone());
    let thread_ptr = JNI_THREAD.with(|c| c.get());
    match (shared_arc, thread_ptr.is_null()) {
        (Some(ref arc), false) => {
            // Safety: JNI_THREAD is set to a valid JvmThread pointer by set_jni_thread
            // and cleared by clear_jni_thread. The pointer is only used from the owning
            // thread (thread-local), and we hold exclusive access for the closure duration.
            Some(f(arc, unsafe { &mut *(thread_ptr as *mut JvmThread) }))
        }
        _ => {
            // A `None` here is answered to the host library as a null `jobject`
            // or a zero, which it reads as an ordinary answer. That silence is
            // what made the TLSv1.3 client-certificate defect cost a week: a
            // nested native call used to CLEAR the enclosing call's context, and
            // every later up-call the outer native made landed here and was
            // quietly ignored. See `JniContextGuard::install`.
            //
            // A `warn!` is wrong — a genuinely detached host thread reaches this
            // legitimately and often — so the instrument is a count, reported
            // with the other JNI figures. Zero is the expected value for any run
            // whose native calls all originate from Java.
            JNI_UPCALLS_WITHOUT_CONTEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            None
        }
    }
}

/// The registry id of the `JvmThread` this OS thread's JNI binding names, if
/// one is installed.
///
/// Reads one `Copy` field through the `JNI_THREAD` pointer without forming a
/// `&mut` (so it cannot alias an interpreter borrow the way
/// [`with_jni_context`] can), and without counting a miss in
/// [`JNI_UPCALLS_WITHOUT_CONTEXT`]. `try_with`: callable from a thread whose
/// TLS is being torn down (the answer is then "unbound").
pub(in crate::native::jni) fn jni_bound_thread_id() -> Option<ThreadId> {
    let p = JNI_THREAD
        .try_with(|c| c.get())
        .unwrap_or(std::ptr::null_mut());
    if p.is_null() {
        return None;
    }
    // SAFETY: a non-null `JNI_THREAD` points at a live `JvmThread` for as long
    // as it stays installed (`set_jni_thread`'s contract); `thread_id` is an
    // immutable `Copy` field written once at construction.
    Some(unsafe { (*(p as *const JvmThread)).thread_id })
}

/// The CALLER's registry id, for anything keyed on thread identity — monitor
/// ownership and the STW barrier (gc-common w2-c, 2026-09-23;
/// `common-a-jni-placeholder-thread-id-at-barrier`).
///
/// JNI binding first (it is exact), then the published OS tid, which for a
/// carrier answers the MOUNTED virtual thread (the newest publish on this OS
/// thread, gc-common w6-g). `None` means unregistered (or, for entries not
/// published through `set_os_tid_current`, still ambiguous): callers must NOT
/// substitute a placeholder
/// id at the barrier. `ThreadId(0)` is the MAIN thread's real id, so arriving
/// under it either makes the caller look like the initiator (a counted caller
/// never arrives: hang) or fills main's quota slot (early release while main
/// still runs: a moving collection over live frames).
pub(in crate::native::jni) fn jni_caller_thread_id(shared: &SharedVm) -> Option<ThreadId> {
    jni_bound_thread_id().or_else(|| {
        shared
            .threads
            .thread_registry
            .thread_id_for_current_os_tid()
    })
}

/// How many JNI up-calls found no thread context and were answered as if the VM
/// were detached.
///
/// Reported by `CRATONVM_INTRINSIC_STATS=1`. See the `_` arm of
/// [`with_jni_context`] for what a non-zero count means and why it is a counter
/// rather than a warning.
pub static JNI_UPCALLS_WITHOUT_CONTEXT: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Number of JNI up-calls answered with no context since process start.
pub fn jni_upcalls_without_context() -> u64 {
    JNI_UPCALLS_WITHOUT_CONTEXT.load(std::sync::atomic::Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// JValue union (mirrors C `jvalue` union from jni.h)
// ---------------------------------------------------------------------------

/// C-compatible union of all JNI primitive and reference types.
/// Used by the CallXxxMethodA / CallStaticXxxMethodA families.
#[repr(C)]
pub union JValue {
    pub z: JBoolean, // boolean
    pub b: JByte,    // byte
    pub c: JChar,    // char
    pub s: JShort,   // short
    pub i: JInt,     // int
    pub j: JLong,    // long
    pub f: JFloat,   // float
    pub d: JDouble,  // double
    pub l: JObject,  // object / array reference
}

// ---------------------------------------------------------------------------
// Descriptor parsing helpers
// ---------------------------------------------------------------------------

/// Look up parsed parameter types from the thread-local cache, or parse and cache.
///
/// gci i16/dp: no production caller is left (`va_list_to_jvalues`, the last
/// one, reads [`param_type_tags`] in place since i16); kept, with its cache,
/// for `parse_param_types_cached_matches_inner` and the bounded-LRU tests.
#[cfg_attr(not(test), allow(dead_code))]
pub(in crate::native::jni) fn parse_param_types_cached(descriptor: &str) -> Vec<u8> {
    JNI_DESCRIPTOR_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if let Some(cached) = cache.get(descriptor) {
            return cached.to_vec();
        }
        let types = parse_param_types_inner(descriptor);
        // Bounded LRU: evicts a single least-recently-used entry when full,
        // avoiding the periodic full-flush thrash of a wholesale clear().
        cache.insert(descriptor, types.clone());
        types
    })
}

/// gcd d10/j (`gcd-d5f-jni-default-dispatch-costs-500ns`): the tags of
/// [`parse_param_types_inner`], produced in place -- no thread-local cache
/// probe (a `String`-keyed hash of the descriptor), no `Vec` copy per call.
/// [`dispatch_jni_native`]'s hot path reads them once each. Same bytes, same
/// order, same answer for a malformed descriptor
/// (`param_type_tags_match_the_parser`).
pub(in crate::native::jni) struct ParamTypeTags<'a> {
    pub(in crate::native::jni) bytes: &'a [u8],
    pub(in crate::native::jni) i: usize,
}

/// See [`ParamTypeTags`].
pub(in crate::native::jni) fn param_type_tags(descriptor: &str) -> ParamTypeTags<'_> {
    ParamTypeTags {
        bytes: descriptor.as_bytes(),
        i: 1, // skip leading '('
    }
}

impl ParamTypeTags<'_> {
    /// Skip past the next `;` (an object type's name and its terminator).
    #[inline]
    pub(in crate::native::jni) fn skip_class_name(&mut self) {
        while self.i < self.bytes.len() && self.bytes[self.i] != b';' {
            self.i += 1;
        }
        self.i += 1; // skip ';'
    }
}

impl Iterator for ParamTypeTags<'_> {
    type Item = u8;

    fn next(&mut self) -> Option<u8> {
        // Exactly `parse_param_types_inner`'s loop, one tag per call; an
        // unknown byte is skipped as it skips it.
        while self.i < self.bytes.len() && self.bytes[self.i] != b')' {
            match self.bytes[self.i] {
                b @ (b'Z' | b'B' | b'C' | b'S' | b'I' | b'J' | b'F' | b'D') => {
                    self.i += 1;
                    return Some(b);
                }
                b'L' => {
                    self.i += 1;
                    self.skip_class_name();
                    return Some(b'L');
                }
                b'[' => {
                    self.i += 1;
                    if self.i < self.bytes.len() && self.bytes[self.i] == b'L' {
                        self.i += 1;
                        self.skip_class_name();
                    } else if self.i < self.bytes.len() && self.bytes[self.i] == b'[' {
                        while self.i < self.bytes.len() && self.bytes[self.i] == b'[' {
                            self.i += 1;
                        }
                        if self.i < self.bytes.len() && self.bytes[self.i] == b'L' {
                            self.i += 1;
                            self.skip_class_name();
                        } else {
                            self.i += 1;
                        }
                    } else {
                        self.i += 1; // primitive element type
                    }
                    return Some(b'[');
                }
                _ => {
                    self.i += 1;
                }
            }
        }
        None
    }
}

/// Parse the parameter type tags from a JNI method descriptor, e.g.
/// `(ILjava/lang/String;[BZ)V` → `[b'I', b'L', b'[', b'Z']`.
/// Object types (`L...;`) and array types (`[...`) are both returned as a
/// single token (`b'L'` and `b'['` respectively).
pub(in crate::native::jni) fn parse_param_types_inner(descriptor: &str) -> Vec<u8> {
    let mut types = Vec::new();
    let bytes = descriptor.as_bytes();
    let mut i = 1; // skip leading '('
    while i < bytes.len() && bytes[i] != b')' {
        match bytes[i] {
            b'Z' | b'B' | b'C' | b'S' | b'I' | b'J' | b'F' | b'D' => {
                types.push(bytes[i]);
                i += 1;
            }
            b'L' => {
                types.push(b'L');
                i += 1;
                while i < bytes.len() && bytes[i] != b';' {
                    i += 1;
                }
                i += 1; // skip ';'
            }
            b'[' => {
                types.push(b'[');
                i += 1;
                // Skip the element type (may itself be an object or array)
                if i < bytes.len() && bytes[i] == b'L' {
                    i += 1;
                    while i < bytes.len() && bytes[i] != b';' {
                        i += 1;
                    }
                    i += 1; // skip ';'
                } else if i < bytes.len() && bytes[i] == b'[' {
                    // multi-dimensional: skip extra '[' characters
                    while i < bytes.len() && bytes[i] == b'[' {
                        i += 1;
                    }
                    if i < bytes.len() && bytes[i] == b'L' {
                        i += 1;
                        while i < bytes.len() && bytes[i] != b';' {
                            i += 1;
                        }
                        i += 1;
                    } else {
                        i += 1;
                    }
                } else {
                    i += 1; // primitive element type
                }
            }
            _ => {
                i += 1;
            }
        }
    }
    types
}

/// Long-smuggle mint chokepoint for the JNI paths that turn a native `jlong`
/// into a Java `long` without going through `SetLongField` /
/// `SetLongArrayRegion` / the generic-JNI `J` return (which mint inline):
/// call arguments (`jvalues_to_values`, which the `va_list` forms reach via
/// `va_list_to_jvalues`), `SetStaticLongField`, and the long-array write-back
/// of `Release<Long>ArrayElements` / `ReleasePrimitiveArrayCritical`. Same
/// strict object-start probe as those sites, so ordinary numeric values
/// register nothing (see `memory::smuggled_longs`).
#[inline]
pub(in crate::native::jni) fn mint_if_smuggled_jlong(heap: &crate::memory::vm_heap::VmHeap, bits: u64) {
    if bits != 0 && bits & 0x7 == 0 && heap.is_object_address(bits as usize).is_some() {
        crate::memory::smuggled_longs::record_minted_long(heap, bits);
    }
}

/// [`mint_if_smuggled_jlong`] for an already-decoded element value: a no-op
/// for everything but `Value::Long`.
#[inline]
pub(in crate::native::jni) fn mint_if_smuggled_long_value(heap: &crate::memory::vm_heap::VmHeap, value: &Value) {
    if let Value::Long(l) = *value {
        mint_if_smuggled_jlong(heap, l as u64);
    }
}

/// Convert a slice of `JValue`s to `Value`s using the type tags from
/// `parse_param_types`. Returns an empty vector if `args` is null.
///
/// A `J` argument is a mint site (`mint_if_smuggled_jlong`):
/// `CallVoidMethod(obj, setHandle, (jlong) someJobject)` is the classic
/// long-as-jobject smuggle, and every `Call*Method{,V,A}` / `NewObject{,V,A}`
/// funnels through here.
///
/// gci i15/jk: the up-calls build their list with [`jni_upcall_args`] (same
/// conversion, [`jvalue_to_value`], in place); this stays as the tests'
/// reference.
#[cfg_attr(not(test), allow(dead_code))]
pub(in crate::native::jni) unsafe fn jvalues_to_values(
    heap: &crate::memory::vm_heap::VmHeap,
    args: *const JValue,
    types: &[u8],
) -> Vec<Value> {
    if args.is_null() {
        return Vec::new();
    }
    types
        .iter()
        .enumerate()
        .map(|(idx, &tag)| jvalue_to_value(heap, &*args.add(idx), tag))
        .collect()
}

/// One `JValue` read as the parameter type `tag` names (the one conversion
/// behind [`jvalues_to_values`] and [`jni_upcall_args`]).
///
/// # Safety
/// `jv` holds the member `tag` names (the caller's descriptor says so).
#[inline]
pub(in crate::native::jni) unsafe fn jvalue_to_value(heap: &crate::memory::vm_heap::VmHeap, jv: &JValue, tag: u8) -> Value {
    match tag {
        b'Z' => Value::Int(jv.z as i32),
        b'B' => Value::Int(jv.b as i32),
        b'C' => Value::Int(jv.c as i32),
        b'S' => Value::Int(jv.s as i32),
        b'I' => Value::Int(jv.i),
        b'J' => {
            mint_if_smuggled_jlong(heap, jv.j as u64);
            Value::Long(jv.j)
        }
        b'F' => Value::Float(jv.f),
        b'D' => Value::Double(jv.d),
        _ /* b'L' | b'[' */ => match jobject_to_obj(jv.l) {
            Some(r) => Value::Object(Some(r)),
            None => Value::Object(None),
        },
    }
}

/// gci i15/jk: the Java argument list of a JNI up-call, inline up to eight
/// values (a receiver and seven arguments).
pub(in crate::native::jni) type JniUpcallArgs = smallvec::SmallVec<[Value; 8]>;

/// gci i15/jk (`gcg-g1j-jni-package-repeat-probe-excess-is-not-a-per-call-phase-20261001`,
/// the per-call allocations of the JNI call path): the argument list of a
/// `Call<T>Method{,A,V}` / `CallNonvirtual*` / `CallStatic*` / `NewObject*`
/// up-call -- `receiver` first when the call has one, then `args` read by
/// `descriptor`'s parameter tags -- built in place. It was
/// `parse_param_types_cached` (a thread-local, `String`-keyed descriptor-cache
/// probe and a `Vec` copy of the cached tags), [`jvalues_to_values`] (a `Vec`)
/// and a third `Vec` the receiver and that one were copied into: three heap
/// allocations per up-call, all Rust memory, all redundant. The tags come from
/// [`param_type_tags`], the dispatch's own in-place reader
/// (`param_type_tags_match_the_parser`: same bytes, same order). Same values
/// in the same order as before; a null `args` still contributes no argument.
///
/// # Safety
/// As [`jvalues_to_values`]: a non-null `args` points at one `JValue` per
/// parameter of `descriptor`.
pub(in crate::native::jni) unsafe fn jni_upcall_args(
    heap: &crate::memory::vm_heap::VmHeap,
    receiver: Option<ObjectRef>,
    args: *const JValue,
    descriptor: &str,
) -> JniUpcallArgs {
    let mut out = JniUpcallArgs::new();
    if let Some(r) = receiver {
        out.push(Value::Object(Some(r)));
    }
    if args.is_null() {
        return out;
    }
    for (idx, tag) in param_type_tags(descriptor).enumerate() {
        out.push(jvalue_to_value(heap, &*args.add(idx), tag));
    }
    out
}

// ---------------------------------------------------------------------------
// Core call helpers used by all Call*MethodA variants
// ---------------------------------------------------------------------------
//
// Real-JDK mode (`docs/book/src/user-guide/real-jdk-mode.md` §7): JNI does **not**
// carry a second native-vs-bytecode resolver. Every `Call*Method*` family
// member below funnels into `invoke_on_class_shared`, which is routed through
// `resolve_dispatch`, so JNI inherits the single policy-aware decision point
// (and the §4 invocation census taken there) for free. That is exactly the
// duplicate-dispatch bypass this wave exists to close: the fix here is *not* to
// add a registry lookup of our own.
//
// What JNI does have to add is the **surfacing** rule. These helpers return
// `Option<Value>` and drop `Err` on the floor (`.ok().flatten()`), so a
// `JdkOnlyViolation` raised behind a `CallObjectMethod` would vanish and the run
// would report zero violations having just taken the forbidden path — an
// unverifiable path, which §11's "zero synthetic-stub invocations through any
// path" cannot tolerate. [`jni_surface_jdk_only`] intercepts that one case.

/// Convert an `invoke_on_class_shared` result into JNI's `Option<Value>`,
/// surfacing a Real-JDK refusal as a pending JNI exception instead of `None`.
///
/// **`Compatible` mode is bit-for-bit unchanged**: the only intercepted value is
/// `VmError::JdkOnly`, which `Compatible` mode never constructs. Every other
/// outcome — including a Java `ExceptionThrown`, which these JNI helpers have
/// always dropped (a separate, pre-existing gap; see the JDK-ONLY-NOTE below) —
/// takes precisely the path it took before.
///
/// JDK-ONLY-NOTE — **CLOSED.** This used to read "the JNI `Call*Method` helpers
/// silently swallow `MethodCallFailed::ExceptionThrown`, so a Java exception
/// thrown by a JNI-initiated call never becomes a pending JNI exception. Fixing
/// it would alter `Compatible` behaviour, which this wave may not do." The
/// `ExceptionThrown` arm below has published the pending exception since the
/// bare-varargs `Call<T>Method` slots started dispatching, so the note has been
/// contradicted by the code thirty lines under it. Corrected 2026-08-06.
///
/// What is still dropped is every OTHER `Err(_)`, which is the honest residual:
/// an internal VM failure has no JNI representation short of inventing one.
/// Except under `--real-jdk` (round 12 wave 6, lane jni): a Java-typed VM
/// error (`VmError::Runtime` / `VmError::Linkage`) is the throwable it names
/// ([`jni_vm_error_as_throwable`]) and is published pending like any other.
/// That is the shape a registered native's failure takes when the up-call
/// target IS a native (`Class.forName0` answering `ClassNotFoundException`),
/// which an interpreter invoke arm materialises and this used to drop, leaving
/// the native a 0 with nothing pending.
#[inline]
pub(in crate::native::jni) fn jni_surface_jdk_only(
    shared: &SharedVm,
    thread: &mut JvmThread,
    result: crate::error::MethodCallResult,
) -> Option<Value> {
    // Interpreter round i4 wave 99, lane L6: an up-call that failed inside
    // the VM dropped its frames without a Java return or unwind; this
    // thread's death keeps the full monitor sweep
    // (`JvmThread::vm_error_upcall_seen`).
    if matches!(result, Err(crate::error::MethodCallFailed::InternalError(_))) {
        thread.vm_error_upcall_seen = true;
    }
    let result = match result {
        Err(err) if jni_types_vm_errors(shared) => {
            Err(jni_vm_error_as_throwable(shared, thread, err))
        }
        other => other,
    };
    match result {
        Ok(value) => value,
        Err(crate::error::MethodCallFailed::InternalError(crate::error::VmError::JdkOnly(
            violation,
        ))) => {
            raise_jdk_only_violation(shared, thread, &violation);
            None
        }
        // An ordinary Java exception thrown by the method the native called.
        //
        // JNI's contract is that it is PENDING when the up-call returns, so
        // the native's `ExceptionCheck` sees it and can decide what to do;
        // discarding it — which this arm used to do, along with every other
        // failure — meant a native saw a `0` return from a method that had in
        // fact thrown, with nothing pending to say so. That was unobservable
        // for as long as the bare-varargs `Call<T>Method(...)` slots raised
        // UnsatisfiedLinkError instead of dispatching; it is reachable now
        // that they work.
        //
        // Published in place rather than through
        // `set_jni_pending_exception_object`, which would re-enter
        // `with_jni_context` for the `&mut JvmThread` this scope already
        // holds. Same two writes: the raw handle for `ExceptionOccurred`, and
        // the GC-remapped `native_pending_return` that is authoritative at the
        // native's own return.
        Err(crate::error::MethodCallFailed::ExceptionThrown(exc)) => {
            let handle = obj_to_jobject(exc);
            JNI_PENDING_EXCEPTION.with(|cell| cell.set(handle));
            thread.native_pending_return = Some(exc);
            None
        }
        Err(_) => None,
    }
}

/// Does this VM publish a Java-typed VM error an up-call returned as the
/// throwable it names ([`jni_surface_jdk_only`])? In both modes: the owner
/// turned it on for `--compatible` too (2026-09-27). Kill switch
/// `CRATONVM_JNI_UPCALL_TYPED_ERRORS=0`.
pub(in crate::native::jni) fn jni_types_vm_errors(_shared: &SharedVm) -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        !matches!(
            Err::<String, std::env::VarError>(std::env::VarError::NotPresent)
                .as_deref()
                .map(str::trim),
            Ok("0" | "false" | "off" | "no")
        )
    })
}

/// `VmError::Runtime` / `VmError::Linkage` as the Java throwable each names,
/// thrown (`MethodCallFailed::ExceptionThrown`), the conversion the
/// interpreter's invoke arms make (`classify_fastpath_invoke_error`); any
/// other failure unchanged. Cold: error paths only.
#[cold]
pub(in crate::native::jni) fn jni_vm_error_as_throwable(
    shared: &SharedVm,
    thread: &mut JvmThread,
    err: crate::error::MethodCallFailed,
) -> crate::error::MethodCallFailed {
    match err {
        crate::error::MethodCallFailed::InternalError(crate::error::VmError::Runtime(re)) => {
            crate::runtime::exceptions::throw_runtime_error(shared, thread, re)
        }
        crate::error::MethodCallFailed::InternalError(crate::error::VmError::Linkage(le)) => {
            crate::runtime::exceptions::throw_linkage_error(shared, thread, le)
        }
        other => other,
    }
}

/// Materialise a Real-JDK refusal as a pending JNI exception.
///
/// Mirrors [`jni_throw_unsatisfied_link`]'s shape, but takes the already-held
/// `shared`/`thread` rather than re-entering `with_jni_context` — these call
/// sites are *inside* that closure. Cold: only ever reached on a refusal, so the
/// `String` the message needs is never built on the fast path.
///
/// The exception class is `java/lang/InternalError`, and as of 2026-08-06 that
/// is a decision rather than a placeholder. The open question this comment used
/// to carry — "if `--explain-jdk-only`'s rendering settles on a different
/// Java-visible type, follow it" — was checked and has no answer to follow:
/// `JdkOnlyViolation::render` produces a text/JSON report and names no Java
/// type, so nothing else in the tree materialises a violation as a throwable.
///
/// `InternalError` satisfies the constraint that actually matters, which is
/// that a policy refusal must be **catchable**: this builds a real exception
/// object and leaves it pending, so a native's `ExceptionCheck` sees it and a
/// `catch (Throwable)` can observe it. That is the opposite of the bare
/// `MethodCallFailed::InternalError` the other refusal sites return, whose own
/// doc says it is "not catchable by Java code" and aborts the run — this JNI
/// path is the one place a `--real-jdk` refusal is already the right shape.
/// Contract §5's spec-appropriate types (`NoClassDefFoundError` and friends,
/// via `native_api::refusal_to_java_failure`) apply to class-identity
/// refusals; a dispatch-policy refusal is not one of those, so it does not
/// inherit their type. Revisit only if a Java-visible type is chosen elsewhere.
/// The message body is `JdkOnlyViolation::summary()` either way.
#[cold]
#[inline(never)]
pub(in crate::native::jni) fn raise_jdk_only_violation(
    shared: &SharedVm,
    thread: &mut JvmThread,
    violation: &cratonvm_types::error::JdkOnlyViolation,
) {
    let msg = violation.summary();
    match crate::runtime::exceptions::create_exception_object(
        shared,
        thread,
        "java/lang/InternalError",
        Some(&msg),
    ) {
        Ok(exc) => set_jni_pending_exception_object(exc),
        // Heap exhausted / class-load failure while building the report: fall
        // back to the `ThrowNew` sentinel so the refusal is flagged rather than
        // swallowed, exactly as `jni_throw_unsatisfied_link` does. gcd d4/k:
        // a heap failure pends the preallocated OOME instead (a real object
        // for `ExceptionOccurred`; `pend_unbuilt_throwable`).
        Err(e) => {
            let oom = if is_heap_exhaustion(&e) {
                *shared.mem.singleton_oom.read()
            } else {
                None
            };
            match oom {
                Some(oom) => set_jni_pending_exception_object(oom),
                None => JNI_PENDING_EXCEPTION.with(|cell| cell.set(u64::MAX)),
            }
        }
    }
}

/// Perform a virtual instance method call from JNI.
/// `obj` is the receiver; `mid` encodes (declaring_class_id, method_index);
/// `args` is the JValue array (may be null for zero-arg methods).
pub(in crate::native::jni) fn jni_call_instance(obj: JObject, mid: JMethodID, args: *const JValue) -> Option<Value> {
    if obj == 0 || mid == 0 {
        return None;
    }
    let _fg = ForeignCallGuard::enter();
    with_jni_context(|shared, thread| {
        let oref = jobject_to_obj(obj)?;
        let (decl_class_id, method_index) = decode_method_id(mid);
        // An array's header carries its COMPONENT's class id, so dispatching
        // on it ran the component's override with the array as `this`
        // (`CallObjectMethod(stringArray, toString)` reached
        // `String.toString`). An array inherits every instance method from
        // `java/lang/Object` and overrides none, so its virtual target is the
        // method ID's own declaring class; `invoke_on_class_shared` does not
        // retarget an array receiver.
        let is_array = shared.mem.heap.kind_of(oref) == crate::memory::heap::ObjectKind::Array;
        let obj_class_id = if is_array {
            decl_class_id
        } else {
            shared.mem.heap.class_id_of(oref)
        };
        // Wave 60 (lane L1): a lambda proxy's own method id too; the call
        // dispatches on the receiver as an inherited id's does.
        let (method_name, descriptor) = jni_method_name_and_descriptor(shared, mid)?;
        // gci i15/jk: built in place (`jni_upcall_args`; three heap `Vec`s before).
        let jvm_args = unsafe { jni_upcall_args(&shared.mem.heap, Some(oref), args, &descriptor) };
        // JIT round 21 wave 8, lane jni: a constructor `methodID` registers a
        // finalizable receiver before it runs (HotSpot: at `Object.<init>`'s
        // return; `jni_constructor_call_registers`).
        let withdrawable = jni_constructor_call_withdrawable(shared, &method_name, oref);
        // JIT round 21 wave 22, lane finalize: a re-`<init>` of a registered
        // object registers it again at `Object.<init>`'s return (HotSpot).
        let reinit = jni_reinit_of_registered_finalizable(shared, &method_name, oref, withdrawable);
        // Wave 25 (lane initwitness): the receiver's `<init>`-return witness
        // before the call (`jni_reinit_registers_again_witnessed`).
        let reinit_witness = reinit.and_then(|_| jni_reinit_init_return_witness(shared, oref));
        // JIT round 22 wave 1, lane review (`CRATONVM_JNI_REINIT_WITNESS_INTERPRETS`):
        // with the witness armed, the call runs interpreted with every frame
        // kept, so `Object.<init>` returning on the receiver moves it.
        let _reinit_window = reinit_witness.and_then(|_| JniReinitWitnessWindow::open(shared));
        jni_constructor_call_registers(shared, &method_name, oref);
        let is_ctor = &*method_name == "<init>";
        // JIT round 21 wave 8, lane jni (`CRATONVM_JNI_CALL_INIT_EXACT_MID`,
        // default ON, read only for a constructor whose declaring class is not
        // the receiver's): HotSpot's `jni_invoke_nonstatic` selects through
        // the vtable only for a method that has a vtable index, and `<init>`
        // has none (`nonvirtual_vtable_index`), so `CallVoidMethod(obj,
        // superCtorID)` runs exactly THAT constructor. Dispatching `<init>` by
        // name on the receiver's class ran the receiver's own same-descriptor
        // constructor instead (as `NewObjectA` did before
        // `CRATONVM_JNI_NEW_OBJECT_EXACT_MID`). No re-target: an abstract
        // declaring class's constructor is still its own body.
        let exact_ctor = is_ctor
            && !is_array
            && decl_class_id != obj_class_id
            // Read once per VM (`VmConfig::jni_call_init_exact_mid`; JIT round
            // 21 wave 13, review rev-native w13 item 5).
            && true;
        // JIT round 21 wave 10, lane jni: the rest of HotSpot's rule. Only a
        // methodID whose declaring class is not the receiver's can differ, so
        // nothing below is asked (no class-store read, no switch read) when
        // the two are the same class or the receiver is an array.
        let inherited = !is_ctor && !is_array && decl_class_id != obj_class_id;
        let (statically_bound, decl_is_interface) = if inherited {
            let cm = shared.classes.class_manager.read();
            let store = &cm.class_store;
            (
                jni_mid_is_statically_bound(store, decl_class_id, method_index),
                store.get(decl_class_id).is_some_and(|c| c.is_interface()),
            )
        } else {
            (false, false)
        };
        // `CRATONVM_JNI_CALL_STATICALLY_BOUND_EXACT_MID` (default ON):
        // `jni_invoke_nonstatic` runs the methodID's own method when it can be
        // statically bound -- private, final, or declared by a final class
        // (`Method::can_be_statically_bound`; a private method has no vtable
        // index). Dispatching by name on the receiver's class ran a
        // subclass's same-signature method instead (a private method of the
        // superclass is not overridden by it, JVMS 5.4.5). Read once per VM
        // (`VmConfig::jni_call_statically_bound_exact_mid`; review rev-w10).
        let exact_bound = statically_bound;
        // `CRATONVM_JNI_CALL_VIRTUAL_SELECTS` (default ON): any other inherited
        // methodID is SELECTED against its own declaration (JVMS 5.4.6, the
        // receiver-class door the interpreter's virtual fallback uses), so a
        // cross-package package-private method of the superclass is not run
        // as the receiver's same-signature method, nor hidden by a private or
        // static one. Only a strict `Selected` answer replaces the by-name
        // walk (`selection::refine_lenient_selection_for`), so a hierarchy
        // with a compatibility shape in it keeps the walk's answer. A lambda
        // proxy receiver keeps the plain door, as the interpreter's does. Read
        // once per VM (`VmConfig::jni_call_virtual_selects`; review rev-w10:
        // it was an environment read on every inherited call).
        let selecting = if inherited
            && !exact_bound
            && !shared.classes.is_lambda_proxy_class(obj_class_id)
        {
            use crate::runtime::resolve::selection::ResolvedRef;
            Some(if decl_is_interface {
                ResolvedRef::Interface(Some(decl_class_id))
            } else {
                ResolvedRef::Declared(decl_class_id)
            })
        } else {
            None
        };
        // Real-JDK §7: the resolver runs inside `invoke_on_class_shared`; this
        // site only has to keep its refusal from being swallowed.
        let result = if exact_ctor || exact_bound {
            crate::vm::invoke_on_class_shared_no_retarget(
                shared,
                thread,
                decl_class_id,
                &method_name,
                &descriptor,
                &jvm_args,
            )
        } else if let Some(resolved) = selecting {
            crate::vm::invoke_on_receiver_class_selecting(
                shared,
                thread,
                obj_class_id,
                resolved,
                &method_name,
                &descriptor,
                &jvm_args,
            )
        } else {
            invoke_on_class_shared(
                shared,
                thread,
                obj_class_id,
                &method_name,
                &descriptor,
                &jvm_args,
            )
        };
        if let (Some(epoch), true) = (withdrawable, result.is_err()) {
            crate::runtime::interpreter::withdraw_unconstructed_finalizable_at_door(
                shared, oref, epoch,
            );
        }
        if let Some(epoch) = reinit {
            jni_reinit_registers_again_witnessed(
                shared,
                obj,
                oref,
                epoch,
                result.as_ref().err(),
                reinit_witness,
            );
        }
        jni_surface_jdk_only(shared, thread, result)
    })
    .flatten()
}

/// SUPERSEDED (gcj close, 2026-10-20): the dev merge brought
/// `CRATONVM_JNI_CALL_VIRTUAL_SELECTS` (default ON, every collector), which
/// selects in [`jni_call_instance`] itself; this helper is kept for its tests.
#[allow(dead_code)]
/// gcj j2/jn2 (`CRATONVM_JNI_VIRTUAL_UPCALL_SELECTS=1`, opt-in, Generational
/// only; `gcj-j2jn2-jni-virtual-upcall-runs-a-private-or-non-overriding-hider-20261019`):
/// the reference [`jni_call_instance`] selects its target against, or `None`
/// for the by-name walk (the switch off, another collector, an array receiver
/// -- its target is the method ID's own class already -- or a lambda proxy's
/// method ID, which the class store does not hold). An interface's method ID
/// selects as `invokeinterface` does, any other as `invokevirtual`.
pub(in crate::native::jni) fn jni_virtual_upcall_selection(
    shared: &SharedVm,
    decl_class_id: ClassId,
    is_array: bool,
) -> Option<crate::runtime::resolve::selection::ResolvedRef> {
    use crate::runtime::resolve::selection::ResolvedRef;
    if is_array
        || !jni_switches_of(shared).virtual_upcall_selects
        || !shared.mem.heap.is_generational()
        || shared.classes.is_lambda_proxy_class(decl_class_id)
    {
        return None;
    }
    let cm = shared.classes.class_manager.read();
    let interface = cm.get_class(decl_class_id)?.is_interface();
    Some(if interface {
        ResolvedRef::Interface(Some(decl_class_id))
    } else {
        ResolvedRef::Declared(decl_class_id)
    })
}

/// JIT round 21 wave 22, lane finalize
/// (`VmConfig::jni_reinit_registers_finalizer_again`,
/// `CRATONVM_JNI_REINIT_REGISTERS_FINALIZER_AGAIN`, default ON; item 2 of
/// `docs/feature-designs/real-jdk-mode.md`): is this
/// JNI `<init>` call a RE-construction of an object that is already
/// registered for finalization? `Some(address epoch)` when the receiver is
/// finalizable and has an active row (`withdrawable` is `None` for exactly
/// that reason, or because the call is no constructor call). HotSpot
/// registers at every `Object.<init>` return (`_return_register_finalizer`,
/// `RegisterFinalizersAtInit`), so such a call makes a second `Finalizer`
/// once its constructor gets that far, and `finalize()` then runs twice
/// ([`jni_reinit_registers_again`]). A receiver with no active row (an
/// `AllocObject` instance, a withdrawn or an already finalized object) is
/// registered by `jni_constructor_call_registers` itself, as before. One bool
/// test unless the receiver is finalizable; then one processor lock.
pub(in crate::native::jni) fn jni_reinit_of_registered_finalizable(
    shared: &SharedVm,
    method_name: &str,
    receiver: ObjectRef,
    withdrawable: Option<u64>,
) -> Option<u64> {
    if withdrawable.is_some()
        || !shared.config.jni_reinit_registers_finalizer_again
        || method_name != "<init>"
        || !crate::runtime::interpreter::class_has_finalizer_shared(shared, receiver)
    {
        return None;
    }
    let registered = shared
        .mem
        .ref_processor
        .lock()
        .is_registered_finalizable(receiver.as_ptr() as usize); // Cast: GC object pointer to address
    registered.then(|| shared.mem.address_epoch())
}

// The second half of [`jni_reinit_of_registered_finalizable`]: the
// re-construction call returned (`threw`: with an exception). Its
// `Object.<init>` returned unless the interpreter's unwinder proved that the
// constructor threw before it did (the note it makes for a JNI upcall,
// `exception_dispatch::unconstructed_mark_at_entry`; dropped here, nothing
// is withdrawn): then HotSpot makes no second `Finalizer`. A normal return
// of a verified constructor always passed `Object.<init>`, and a throw the
// unwinder could not place before it is taken as after it, as the
// withdrawing door takes it. A collection during the call (the address
// epoch moved) leaves the object as it is: one `finalize()`, the answer
// before the switch. Returns whether the extra registration was counted.
//
// JIT round 21 wave 23, lane finalize (`VmConfig::jni_reinit_count_by_outcome`,
// `CRATONVM_JNI_REINIT_COUNT_BY_OUTCOME`, default ON, `=0` restores the
// wave-22 rule above; items 2 and 3 of
// `docs/feature-designs/real-jdk-mode.md`):
//
// - Item 2: a NORMAL return counts whatever the epoch did. No unwinder note
//   can exist then, so the pre-call address served only to find the row, and
//   the receiver's current address is read back from the native's handle
//   ([`jni_receiver_after_call`]). A collection during the constructor (or
//   any concurrent sweep, which also moves the epoch) dropped the count, the
//   common case on a busy VM. A throw keeps the epoch test: its note is keyed
//   by address and epoch, and "no count" is the fail-safe there.
// - Item 3: a failure counts only when it can have come out of the
//   constructor's own frames ([`jni_reinit_failure_reached_ctor`]): not a VM
//   refusal (`MethodCallFailed::InternalError`: a `--real-jdk` refusal, a
//   link failure of the method id's class) and not a `StackOverflowError`,
//   which `JavaCalls::call_helper` raises before the method is entered. Both
//   err toward the one `finalize()` of the pre-switch answer.
// Wave 25: production calls go through `jni_reinit_registers_again_witnessed`;
// this witness-less form is the wave-24 rule the unit tests pin.
#[cfg_attr(not(test), allow(dead_code))]
pub(in crate::native::jni) fn jni_reinit_registers_again(
    shared: &SharedVm,
    handle: JObject,
    receiver: ObjectRef,
    epoch: u64,
    failure: Option<&crate::error::MethodCallFailed>,
) -> bool {
    jni_reinit_registers_again_witnessed(shared, handle, receiver, epoch, failure, None)
}

/// The receiver's `<init>`-return witness before a JNI re-`<init>` call
/// (JIT round 21 wave 25, lane initwitness,
/// `VmConfig::jni_reinit_init_return_witness`,
/// `CRATONVM_JNI_REINIT_INIT_RETURN_WITNESS`, default ON; item 2 of
/// `docs/feature-designs/real-jdk-mode.md`, the exact
/// answer): the active row's `ReferenceEntry::init_returns`, which the
/// interpreter's `<init>`-return hook (`exception_dispatch.rs`
/// `finalizer_init_returned`, the point where this VM marks a row
/// constructed, i.e. HotSpot's `RegisterFinalizersAtInit` point) moves.
/// `None` when the switch, or the wave-23 outcome rule it refines, is off,
/// or when the hook is not armed (then it never moves). Per VM and per
/// object: the count lives on the receiver's own row in this VM's
/// reference processor. Only asked for a re-`<init>` of a registered
/// finalizable object: one processor lock.
/// The witness window of one JNI re-`<init>` call (JIT round 22 wave 1, lane
/// review; `VmConfig::jni_reinit_witness_interprets`,
/// `CRATONVM_JNI_REINIT_WITNESS_INTERPRETS`, default ON; item 2 of
/// `docs/feature-designs/real-jdk-mode.md`, the silent
/// witness). Opened only when [`jni_reinit_init_return_witness`] armed a
/// witness, closed on drop (every exit, unwinding included). While any window
/// of the VM is open, its doors run bodies interpreted and keep frames
/// (`SharedVm::jni_reinit_witness_window`), so the `<init>`-return hook sees
/// every constructor return on the receiver and `Object.<init>`'s own: the
/// witness moved during a failed call exactly when `Object.<init>` returned,
/// which the elided `Object.<init>`, the frameless trivial constructors and
/// compiled super constructors used to hide. Constructors already running
/// compiled on other threads finish as they are, as under JVMTI's
/// interp-only mode; they cannot touch this call's receiver's witness.
pub(in crate::native::jni) struct JniReinitWitnessWindow<'a> {
    pub(in crate::native::jni) shared: &'a SharedVm,
}

impl<'a> JniReinitWitnessWindow<'a> {
    pub(in crate::native::jni) fn open(shared: &'a SharedVm) -> Option<Self> {
        if !shared.config.jni_reinit_witness_interprets {
            return None;
        }
        shared
            .jni_reinit_witness_window
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        Some(Self { shared })
    }
}

impl Drop for JniReinitWitnessWindow<'_> {
    fn drop(&mut self) {
        self.shared
            .jni_reinit_witness_window
            .fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
}

/// A JNI re-`<init>` call's witness, read before the call
/// ([`jni_reinit_init_return_witness`]): the row's `<init>`-return count and,
/// under `VmConfig::jni_reinit_witness_nested_exact` (JIT round 22 wave 2, lane
/// review), the row's re-`<init>` call count after this call's own bump.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::native::jni) struct JniReinitWitness {
    pub(in crate::native::jni) init_returns: u32,
    pub(in crate::native::jni) calls: Option<u32>,
}

pub(in crate::native::jni) fn jni_reinit_init_return_witness(
    shared: &SharedVm,
    receiver: ObjectRef,
) -> Option<JniReinitWitness> {
    if !shared.config.jni_reinit_init_return_witness || !shared.config.jni_reinit_count_by_outcome
    {
        return None;
    }
    let mut rp = shared.mem.ref_processor.lock();
    if !rp.reregister_at_init_return() {
        return None;
    }
    let addr = receiver.as_ptr() as usize; // Cast: GC object pointer to address
    let init_returns = rp.finalizable_init_returns(addr)?;
    // JIT round 22 wave 2, lane review (`CRATONVM_JNI_REINIT_WITNESS_NESTED_EXACT`;
    // the "known imprecision" of item 2 of `r21-review-w24-findings-FIXED-20261020.md`):
    // this call is counted on the row, so a NESTED re-`<init>` of the same
    // object during it (another thread, or a native the constructor calls)
    // shows as a moved count after the call.
    let calls = {
        rp.note_jni_reinit_call(addr)
    };
    Some(JniReinitWitness {
        init_returns,
        calls,
    })
}

/// [`jni_reinit_registers_again`] with the receiver's `<init>`-return
/// witness read before the call (`witness_before`,
/// [`jni_reinit_init_return_witness`]; wave 25, lane initwitness). A
/// failure decides "reached" in this order:
///
/// 1. the unwinder's note (`exception_dispatch::unconstructed_mark_at_entry`)
///    proves the constructor threw before `Object.<init>`: not reached;
/// 2. the witness moved during the call: a constructor of the receiver
///    returned normally, so (verified code) its `super(..)` / `this(..)` and
///    `Object.<init>` returned: reached, whatever the error kind -- this
///    retires the wave-23 `StackOverflowError` residual (an SOE raised after
///    a super constructor returned) and the wave-24 `InternalError` guess
///    whenever the hook saw the return;
/// 3. otherwise the error-kind rule ([`jni_reinit_failure_reached_ctor`]).
///
/// The witness only ever answers "reached": a silent witness is no proof,
/// because the hook sees interpreted constructor returns only (a compiled
/// constructor, and `Object.<init>` itself where the interpreter elides its
/// empty body, return unseen). `witness_before == None` is the wave-24 rule.
pub(in crate::native::jni) fn jni_reinit_registers_again_witnessed(
    shared: &SharedVm,
    handle: JObject,
    receiver: ObjectRef,
    epoch: u64,
    failure: Option<&crate::error::MethodCallFailed>,
    witness_before: Option<JniReinitWitness>,
) -> bool {
    let by_outcome = shared.config.jni_reinit_count_by_outcome;
    // Wave 24 (item 1 of `r21-review-w24-findings-FIXED-20261020.md`): decided
    // BEFORE the processor lock (it reads the class manager, which must not
    // nest inside `ref_processor`), and applied AFTER the unwinder's note is
    // consumed, so a "not reached" failure no longer leaves its note for a
    // later re-`<init>` of the same object to find.
    let mut reached = true;
    let addr = match failure {
        None if by_outcome => match jni_receiver_after_call(shared, handle, receiver, epoch) {
            Some(now) => now.as_ptr() as usize, // Cast: GC object pointer to address
            None => return false,
        },
        _ => {
            if shared.mem.address_epoch() != epoch {
                return false;
            }
            if by_outcome && failure.is_some_and(|f| !jni_reinit_failure_reached_ctor(shared, f)) {
                reached = false;
            }
            receiver.as_ptr() as usize // Cast: GC object pointer to address
        }
    };
    let mut rp = shared.mem.ref_processor.lock();
    if failure.is_some() && rp.forget_unconstructed_finalizable(addr, epoch) {
        return false;
    }
    if !reached
        && by_outcome
        && witness_before.is_some_and(|before| {
            rp.finalizable_init_returns(addr) != Some(before.init_returns)
                // JIT round 22 wave 2, lane review: no nested re-`<init>` of
                // the object ran during the call (its `<init>` returns would
                // have moved the count too); otherwise the error-kind rule.
                && before.calls.is_none_or(|own| rp.jni_reinit_calls(addr) == Some(own))
        })
    {
        // Wave 25 (lane initwitness): the receiver's `Object.<init>`
        // returned during the call (step 2 above). `addr` is the pre-call
        // address in the same epoch here (a failure keeps the epoch test),
        // and the row is the one the witness was read from (`None` after:
        // the row left, nothing to count, `note_finalizer_registered_again`
        // answers `false` below).
        reached = true;
    }
    if !reached {
        return false;
    }
    rp.note_finalizer_registered_again(addr)
}

/// The receiver of a JNI call that just returned, at its CURRENT address
/// (wave 23, item 2): `receiver` itself while no collection or sweep ran
/// since `epoch`; otherwise the native's `handle` read back, when it is one a
/// collection rewrites -- a global ref (its table entry) or an indirect local
/// (`CRATONVM_JNI_INDIRECT_LOCALS`, default ON: its slot is a root). A raw
/// local handle keeps the pre-call address, which may now name another
/// object, so it answers `None` (no count, the pre-switch answer).
pub(in crate::native::jni) fn jni_receiver_after_call(
    shared: &SharedVm,
    handle: JObject,
    receiver: ObjectRef,
    epoch: u64,
) -> Option<ObjectRef> {
    if shared.mem.address_epoch() == epoch {
        return Some(receiver);
    }
    let tracks_moves = handle != 0
        && !is_jclass_handle(handle)
        && (handle & 1 == 1 || decode_indirect_local(handle).is_some());
    if !tracks_moves {
        return None;
    }
    jobject_to_obj(handle)
}

/// Can the failed JNI constructor call have run its constructor, so that its
/// `Object.<init>` may have returned (wave 23, item 3)? Not for a VM refusal
/// (`InternalError`), and not for a `StackOverflowError`: HotSpot's
/// `JavaCalls::call_helper` checks the stack before it enters the method, and
/// a depth refusal here is raised at the same point. A `StackOverflowError`
/// raised deeper, after `Object.<init>` returned, is therefore counted as not
/// reached: one `finalize()` where HotSpot runs two (the pre-switch answer;
/// recorded on the w23 page).
pub(in crate::native::jni) fn jni_reinit_failure_reached_ctor(
    shared: &SharedVm,
    failure: &crate::error::MethodCallFailed,
) -> bool {
    match failure {
        // Wave 24 (item 2 of `r21-review-w24-findings-FIXED-20261020.md`): the
        // Java-visible VM errors (`Runtime` / `Linkage`, which
        // `jni_surface_jdk_only` turns into the throwable each names) are the
        // ones that leave a RUNNING constructor's body without a Java unwind,
        // so they count as reached, as `ExceptionThrown` does -- except the
        // depth refusal at entry, `Runtime(StackOverflowError)`. The rest
        // (`JdkOnly`, `Internal`, `InvalidConfiguration`, `ClassFile`) are
        // refusals and stay "not reached".
        crate::error::MethodCallFailed::InternalError(crate::error::VmError::Runtime(
            crate::error::RuntimeError::StackOverflowError,
        )) => false,
        crate::error::MethodCallFailed::InternalError(
            crate::error::VmError::Runtime(_) | crate::error::VmError::Linkage(_),
        ) => true,
        crate::error::MethodCallFailed::InternalError(_) => false,
        crate::error::MethodCallFailed::ExceptionThrown(exc) => {
            let class_id = shared.mem.heap.class_id_of(*exc);
            shared
                .classes
                .class_manager
                .read()
                .get_class(class_id)
                .map_or(true, |c| &*c.name != "java/lang/StackOverflowError")
        }
    }
}

/// JIT round 21 wave 10, lane finalize (`CRATONVM_FINALIZER_REGISTER_AT_OBJECT_INIT`):
/// before a JNI `<init>` call registers its receiver
/// (`jni_constructor_call_registers`), whether a throw of that constructor
/// may withdraw the registration: `Some(address epoch)` when the receiver is
/// finalizable and NOT registered yet (an `AllocObject` instance), so the
/// registration about to be made is this call's own. A receiver some earlier
/// construction registered (a second `<init>` call on a constructed object)
/// keeps its row whatever this call does. One class read and, for a
/// finalizable class only, one processor lock.
pub(in crate::native::jni) fn jni_constructor_call_withdrawable(
    shared: &SharedVm,
    method_name: &str,
    receiver: ObjectRef,
) -> Option<u64> {
    if method_name != "<init>" || !crate::runtime::interpreter::class_has_finalizer_shared(shared, receiver)
    {
        return None;
    }
    let registered = shared
        .mem
        .ref_processor
        .lock()
        .is_registered_finalizable(receiver.as_ptr() as usize); // Cast: GC object pointer to address
    (!registered).then(|| shared.mem.address_epoch())
}

/// JIT round 21 wave 10, lane jni (`CRATONVM_JNI_CALL_STATICALLY_BOUND_EXACT_MID`):
/// is method `method_index` of `decl_class_id` one HotSpot's
/// `jni_invoke_nonstatic` runs exactly, without vtable / itable selection
/// (`Method::can_be_statically_bound`)? A private method (it has no vtable
/// index), a final method, or any method of a final class. `<init>` has its own
/// switch (`CRATONVM_JNI_CALL_INIT_EXACT_MID`), and a static method, a missing
/// class (a lambda proxy's own id) or an index out of range answers `false`, so
/// the caller keeps its receiver dispatch.
pub(in crate::native::jni) fn jni_mid_is_statically_bound(
    store: &crate::classloading::ClassStore,
    decl_class_id: ClassId,
    method_index: u16,
) -> bool {
    use cratonvm_reader::class_access_flags::MethodAccessFlags;
    let Some(class) = store.get(decl_class_id) else {
        return false;
    };
    let Some(m) = class.methods.get(usize::from(method_index)) else {
        return false;
    };
    if m.is_static() || &*m.name == "<init>" || &*m.name == "<clinit>" {
        return false;
    }
    m.access_flags.contains(MethodAccessFlags::PRIVATE)
        || m.access_flags.contains(MethodAccessFlags::FINAL)
        || (class.is_final() && !class.is_interface())
}

/// Perform a nonvirtual instance method call from JNI (dispatch on `clazz`,
/// not on the runtime type of `obj`).
pub(in crate::native::jni) fn jni_call_nonvirtual(
    obj: JObject,
    clazz: JClass,
    mid: JMethodID,
    args: *const JValue,
) -> Option<Value> {
    if obj == 0 || mid == 0 {
        return None;
    }
    let _fg = ForeignCallGuard::enter();
    with_jni_context(|shared, thread| {
        let oref = jobject_to_obj(obj)?;
        let (decl_class_id, method_index) = decode_method_id(mid);
        // Round 15 wave 6 (lane jni6): HotSpot's `CallNonvirtual<T>Method*`
        // runs exactly `methodID` (`jni_invoke_nonstatic`, `JNI_NONVIRTUAL`:
        // the selected method IS the resolved one; `clazz` is not consulted).
        // This read `methods[method_index]` of `clazz` -- an index into the
        // DECLARING class's table (`encode_method_id`) -- so an inherited
        // `methodID` (`GetMethodID(sub, "toString", ..)` declares on `Object`)
        // named whatever `clazz` has at that index, and the parameters were
        // then read from the native's `jvalue` array by that other
        // descriptor. And an abstract / interface declaring class was
        // re-targeted to the receiver's override, so a native calling its
        // superclass's body re-entered itself. Exact only where the two can
        // differ; `CRATONVM_JNI_NONVIRTUAL_EXACT_MID=0` restores the lookup.
        let clazz_id = (clazz != 0).then(|| jclass_class_id(clazz));
        // gcj j2/jn2 (`gcj-j1jn-proposal-jni-upcalls-dispatch-the-method-id`
        // step 3): the switch is the VM's latched copy
        // (`JniSwitches::nonvirtual_exact_mid`, same value rule), and the
        // retarget question and the method read share one class-manager read
        // (two before). Same answers.
        let (exact, dispatch_class_id, stored) = {
            let cm = shared.classes.class_manager.read();
            let decl_retargets = cm
                .get_class(decl_class_id)
                .map_or(false, |c| c.is_interface() || c.is_abstract());
            let exact = (decl_retargets || clazz_id.is_some_and(|c| c != decl_class_id))
                && jni_switches_of(shared).nonvirtual_exact_mid;
            let dispatch_class_id = match clazz_id {
                Some(c) if !exact => c,
                _ => decl_class_id,
            };
            let stored = cm
                .class_store
                .get(dispatch_class_id)
                .and_then(|class| class.methods.get(method_index as usize))
                .map(|method| (method.name.clone(), method.descriptor.clone()));
            (exact, dispatch_class_id, stored)
        };
        // Wave 60 (lane L1): a lambda proxy's own method id names a method of
        // a final class the class store does not hold; the receiver's
        // dispatch runs exactly it.
        let (stored, proxy_method) = match stored {
            Some(named) => (named, false),
            None => (
                lambda_proxy_jni_method(shared, decl_class_id, method_index)?,
                true,
            ),
        };
        let (method_name, descriptor) = stored;
        // gci i15/jk: built in place (`jni_upcall_args`).
        let jvm_args = unsafe { jni_upcall_args(&shared.mem.heap, Some(oref), args, &descriptor) };
        // JIT round 21 wave 8, lane jni: `AllocObject` +
        // `CallNonvirtualVoidMethod(obj, clazz, <init>)` -- the JNI spec's
        // own two-step construction -- registers a finalizable receiver
        // before the constructor runs (HotSpot: at `Object.<init>`'s return;
        // `jni_constructor_call_registers`).
        let withdrawable = jni_constructor_call_withdrawable(shared, &method_name, oref);
        // JIT round 21 wave 22, lane finalize: see `jni_call_instance`.
        let reinit = jni_reinit_of_registered_finalizable(shared, &method_name, oref, withdrawable);
        // Wave 25 (lane initwitness): the receiver's `<init>`-return witness
        // before the call (`jni_reinit_registers_again_witnessed`).
        let reinit_witness = reinit.and_then(|_| jni_reinit_init_return_witness(shared, oref));
        // JIT round 22 wave 1, lane review (`CRATONVM_JNI_REINIT_WITNESS_INTERPRETS`):
        // with the witness armed, the call runs interpreted with every frame
        // kept, so `Object.<init>` returning on the receiver moves it.
        let _reinit_window = reinit_witness.and_then(|_| JniReinitWitnessWindow::open(shared));
        jni_constructor_call_registers(shared, &method_name, oref);
        // Real-JDK §7: resolver runs inside `invoke_on_class_shared`.
        let result = if exact && !proxy_method {
            crate::vm::invoke_on_class_shared_no_retarget(
                shared,
                thread,
                decl_class_id,
                &method_name,
                &descriptor,
                &jvm_args,
            )
        } else {
            invoke_on_class_shared(
                shared,
                thread,
                dispatch_class_id,
                &method_name,
                &descriptor,
                &jvm_args,
            )
        };
        // JIT round 21 wave 10, lane finalize: see `jni_call_instance`.
        if let (Some(epoch), true) = (withdrawable, result.is_err()) {
            crate::runtime::interpreter::withdraw_unconstructed_finalizable_at_door(
                shared, oref, epoch,
            );
        }
        if let Some(epoch) = reinit {
            jni_reinit_registers_again_witnessed(
                shared,
                obj,
                oref,
                epoch,
                result.as_ref().err(),
                reinit_witness,
            );
        }
        jni_surface_jdk_only(shared, thread, result)
    })
    .flatten()
}

/// Perform a static method call from JNI.
pub(in crate::native::jni) fn jni_call_static(clazz: JClass, mid: JMethodID, args: *const JValue) -> Option<Value> {
    if mid == 0 {
        return None;
    }
    let _fg = ForeignCallGuard::enter();
    with_jni_context(|shared, thread| {
        let (decl_class_id, method_index) = decode_method_id(mid);
        // Round 15 wave 7 (lane drainA; N4 of `r15w6-jni6-review-findings`):
        // HotSpot's `CallStatic<T>Method*` runs exactly `methodID`
        // (`jni_invoke_static`; `clazz` is not consulted) and initializes the
        // method's holder. Dispatching by name on `clazz` ran a same-signature
        // static that `clazz` declares to hide it, and initialized `clazz`
        // for an inherited static. `CRATONVM_JNI_STATIC_EXACT_MID=0` restores
        // the by-name dispatch (read only when the two classes differ).
        // gcj j2/jn2 (the up-call proposal's step 3): the VM's latched copy
        // of the switch (`JniSwitches::static_exact_mid`, same value rule).
        let class_id = match (clazz != 0).then(|| jclass_class_id(clazz)) {
            Some(c) if c == decl_class_id || !jni_switches_of(shared).static_exact_mid => c,
            _ => decl_class_id,
        };
        let (method_name, descriptor) = {
            let cm = shared.classes.class_manager.read();
            let class = cm.class_store.get(decl_class_id)?;
            let method = class.methods.get(method_index as usize)?;
            (method.name.clone(), method.descriptor.clone())
        };
        // gci i15/jk: built in place (`jni_upcall_args`).
        let jvm_args = unsafe { jni_upcall_args(&shared.mem.heap, None, args, &descriptor) };
        // Real-JDK §7: resolver runs inside `invoke_on_class_shared`.
        let result = invoke_on_class_shared(
            shared,
            thread,
            class_id,
            &method_name,
            &descriptor,
            &jvm_args,
        );
        jni_surface_jdk_only(shared, thread, result)
    })
    .flatten()
}

// ---------------------------------------------------------------------------
// Global Reference Table
// ---------------------------------------------------------------------------
//
// Design: each global ref is stored in a heap-allocated `Box<ObjectRef>`.
// The `JObject` handle returned to native code has bit 0 set (tag bit) to
// distinguish global refs from ordinary local refs (raw heap pointers, which
// are at least 8-byte aligned so their bit 0 is always 0).
//
// Global ref handle encoding:
//   `handle = (Box::into_raw(box_ptr) as usize) | 1`
//
// Decoding (in `jobject_to_obj`):
//   if `handle & 1 == 1` → `*(handle & !1) as *const ObjectRef`
//   else                 → `handle` is a raw heap pointer
//
// WEAK global refs (gc-common w8-c, `common-w7c-jni-weak-global-refs-are-strong`)
// use the same encoding, but their box is a `Box<usize>` holding the referent's
// address, or 0 once the referent died, and it is kept in a SEPARATE set:
//
// * `collect_roots` and `update_after_gc` never see a weak entry, so a weak
//   global is not a GC root and is not remapped by `update_all_roots`;
// * [`sweep_weak_global_refs`] (every stop-the-world collection) remaps a weak
//   entry whose referent survived and clears one whose referent died;
//   [`clear_weak_global_refs_unmarked`] clears at a concurrent cycle's remark
//   (the bitmap verdict, before anything is freed), and the out-of-pause
//   address-keyed sweeps clear what a concurrent reclamation freed;
// * resolving a live weak handle is a keep-alive (the SATB enqueue
//   `Reference.get()` uses), so a referent a native picks up during a
//   concurrent mark cannot be freed by that cycle.
//
// A cleared entry stays until `DeleteWeakGlobalRef`, as the JNI spec requires:
// the handle is still valid, it just reads as NULL.

pub struct JniGlobalRefs {
    /// Raw `Box<ObjectRef>` pointers (stored as usize for Send/Sync).
    /// Each entry owns its allocation until `remove` is called. Keyed by the
    /// pointer itself so `resolve`/`remove` are O(1) on this hot path (the
    /// handle is just `raw | 1`, so the untagged pointer is a unique key —
    /// distinct `Box` allocations never collide).
    ///
    /// addr-keyed: not a heap address -- C-heap `Box` pointers this table owns (gcj j2/jn2).
    pub(in crate::native::jni) entries: crate::runtime::fx_collections::FxHashSet<usize>,
    /// gc-common w8-c: raw `Box<usize>` pointers of the WEAK global refs. The
    /// box holds the referent's current address, or 0 once it was cleared.
    /// Disjoint from `entries` (distinct live `Box` allocations never share
    /// an address), not rooted, and not remapped by `update_after_gc`: see
    /// [`sweep_weak_global_refs`].
    ///
    /// addr-keyed: not a heap address -- C-heap `Box` pointers this table owns (gcj j2/jn2).
    pub(in crate::native::jni) weak: crate::runtime::fx_collections::FxHashSet<usize>,
    /// Recently deleted weak-ref boxes reused by short-lived interpreter/JIT
    /// doors. This avoids a C-heap allocation and free for each call while
    /// bounding retained memory per VM.
    pub(in crate::native::jni) weak_recycled: Vec<usize>,
    /// gc-common w10-c (`common-w9g-jni-element-copies-are-bound-to-the-getting-thread`):
    /// the `Get<Type>ArrayElements` copies outstanding in THIS VM, keyed by
    /// the buffer address handed to native code. Each record's `array_gref`
    /// is a strong entry of this table. They used to live in a
    /// `thread_local!`, so a `Release` on another thread (legal: the JNI spec
    /// binds a `JNIEnv`, not an elements pointer, to its thread) missed the
    /// record, lost the write-back, leaked the buffer and never deleted the
    /// keep-alive global ref: the array was immortal. Keyed by C-heap
    /// addresses, not `ObjectRef`s, so no collection touches the keys.
    ///
    /// addr-keyed: not a heap address -- C-heap buffers handed to native code (gcj j2/jn2).
    pub(in crate::native::jni) elem_copies: HashMap<usize, ArrayElemBuffer>,
    /// The `GetStringChars` / `GetStringCritical` buffers outstanding in this
    /// VM: buffer address -> allocated UTF-16 unit count (the string's units
    /// plus the NUL terminator, gci i13/jc), the layout `ReleaseStringChars`
    /// rebuilds to free it. Per VM for the same reason (gc-common w10-c).
    ///
    /// addr-keyed: not a heap address -- C-heap UTF-16 buffers handed to native code (gcj j2/jn2).
    pub(in crate::native::jni) string_copies: HashMap<usize, usize>,
    /// gci i13/jc (F10): the `GetPrimitiveArrayCritical` copies outstanding in
    /// this VM, keyed by the buffer address handed to native code; each
    /// record's `array_gref` is a strong entry of this table. They lived in a
    /// `thread_local!` (`JNI_CRITICAL_COPIES`), so a thread that exited with a
    /// section open dropped the record and leaked its keep-alive global (the
    /// array was immortal, the record unreachable), and a release reaching
    /// this VM on another thread found nothing. Per VM, as `elem_copies`.
    ///
    /// addr-keyed: not a heap address -- C-heap buffers handed to native code (gcj j2/jn2).
    pub(in crate::native::jni) critical_copies: HashMap<usize, CriticalCopy>,
    /// gci i14/pn (`CRATONVM_JNI_CRITICAL_G1_PIN`): the G1 critical sections
    /// open in this VM that hand native code the array's own body, keyed by
    /// that body's address (heap, never a C-heap `critical_copies` key). Two
    /// sections on one array share the key, hence the `Vec`. Each record's
    /// `array_gref` is a strong entry of this table; its region holds one pin.
    ///
    /// Each record's region pin and strong `array_gref` hold the array in place until its
    /// release removes the row (G1 only; gcj j2/jn2):
    /// addr-keyed: a HEAP address that cannot move or be reused while it is a key.
    pub(in crate::native::jni) critical_pins: HashMap<usize, Vec<CriticalPin>>,
}

/// What kind of entry a tagged global handle names (gc-common w8-c).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JniGlobalKind {
    /// `NewGlobalRef` (and the VM's own users of the table).
    Strong,
    /// `NewWeakGlobalRef`.
    Weak,
}

/// What one weak-global sweep did, for tests and diagnostics (gc-common w8-c).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct WeakGlobalSweepStats {
    /// Referents the collection relocated; the entry now names the new address.
    pub moved: usize,
    /// Referents that survived in place.
    pub retained: usize,
    /// Referents that died this cycle; the entry now reads as NULL.
    pub cleared: usize,
}

// Safety: `JniGlobalRefs` is stored behind `parking_lot::Mutex<>` in `SharedVm`.
// All mutations (`add`, `remove`, `update_after_gc`) require `&mut self`, enforced by
// the Mutex.  All reads (`resolve`, `collect_roots`, `count`) also go through the
// Mutex lock.  Critically, `jobject_to_obj()` acquires the same Mutex when resolving
// global ref handles, preventing a use-after-free race between concurrent `resolve()`
// and `remove()` calls.
unsafe impl Send for JniGlobalRefs {}
unsafe impl Sync for JniGlobalRefs {}

impl JniGlobalRefs {
    pub(in crate::native::jni) const WEAK_RECYCLE_LIMIT: usize = 256;

    pub fn new() -> Self {
        Self {
            entries: crate::runtime::fx_collections::fx_hashset(),
            weak: crate::runtime::fx_collections::fx_hashset(),
            weak_recycled: Vec::new(),
            elem_copies: HashMap::new(),
            string_copies: HashMap::new(),
            critical_copies: HashMap::new(),
            critical_pins: HashMap::new(),
        }
    }

    /// gci i14/pn: record a pinned G1 critical section on `array`, whose body
    /// at `ptr` native code now holds, minting the strong global ref that
    /// keeps the array alive until the section's release. The caller already
    /// pinned `region` ([`open_critical_g1_pin`]).
    pub(in crate::native::jni) fn open_critical_pin(
        &mut self,
        ptr: usize,
        array: ObjectRef,
        region: usize,
        element_type: ArrayElementType,
        len: usize,
        opener: Option<ThreadId>,
    ) {
        let array_gref = self.add(array);
        self.critical_pins.entry(ptr).or_default().push(CriticalPin {
            ptr,
            array_gref,
            region,
            element_type,
            len,
            opener,
        });
    }

    /// gci i14/pn: take one pinned section on the body at `ptr` out of the
    /// table (the one `releaser` opened, else the newest) and delete its
    /// global ref; the caller unpins `region` after dropping this lock. `None`
    /// (one empty-map test, the default path) when `ptr` names no pinned body.
    pub(in crate::native::jni) fn take_critical_pin(
        &mut self,
        ptr: usize,
        releaser: Option<ThreadId>,
    ) -> Option<CriticalPin> {
        if self.critical_pins.is_empty() {
            return None;
        }
        let list = self.critical_pins.get_mut(&ptr)?;
        let at = list
            .iter()
            .rposition(|p| releaser.is_some() && p.opener == releaser)
            .unwrap_or(list.len().checked_sub(1)?);
        let pin = list.remove(at);
        if list.is_empty() {
            self.critical_pins.remove(&ptr);
        }
        self.remove(pin.array_gref);
        Some(pin)
    }

    /// gci i14/pn: take every pinned section `tid` opened and never released
    /// (the thread is exiting) out of the table, deleting their global refs;
    /// the caller unpins their regions after dropping this lock.
    pub(in crate::native::jni) fn take_critical_pins_opened_by(&mut self, tid: ThreadId) -> Vec<CriticalPin> {
        let mut out = Vec::new();
        if self.critical_pins.is_empty() {
            return out;
        }
        self.critical_pins.retain(|_, list| {
            list.retain(|p| {
                if p.opener == Some(tid) {
                    out.push(*p);
                    false
                } else {
                    true
                }
            });
            !list.is_empty()
        });
        for p in &out {
            self.remove(p.array_gref);
        }
        out
    }

    /// Record a `GetPrimitiveArrayCritical` copy of `array` at `ptr`, minting
    /// the strong global ref that keeps the array alive and follows it through
    /// every relocation until the final release (gci i13/jc, F10).
    pub(in crate::native::jni) fn open_critical_copy(
        &mut self,
        ptr: usize,
        array: ObjectRef,
        element_type: ArrayElementType,
        len: usize,
        stride: usize,
    ) {
        let array_gref = self.add(array);
        if let Some(stale) = self.critical_copies.insert(
            ptr,
            CriticalCopy {
                array_gref,
                element_type,
                len,
                stride,
                opens: 1,
            },
        ) {
            // Unreachable: a live buffer's address is unique (never 0 bytes).
            self.remove(stale.array_gref);
        }
    }

    /// zt t18/hunt1: the buffer of a critical copy of `array` (its CURRENT
    /// address) already open in this VM, with one more section counted on
    /// it, or `None`. A second `GetPrimitiveArrayCritical` of one array while
    /// the first is open shares that buffer: on HotSpot both answers are the
    /// array's own body (`isCopy` false; ZGC stalls the collector, G1 pins
    /// the region), so a native's write through one is visible through the
    /// other, and releasing one leaves the other valid. Two copies gave each
    /// section its own snapshot and the later release's copy-back undid the
    /// earlier one's writes. One pass over the open copies (a handful at
    /// most); `len` and the element type must match too.
    pub(in crate::native::jni) fn share_critical_copy(
        &mut self,
        array: ObjectRef,
        element_type: ArrayElementType,
        len: usize,
    ) -> Option<usize> {
        if self.critical_copies.is_empty() {
            return None;
        }
        let want = array.as_ptr() as usize;
        let ptr = self
            .critical_copies
            .iter()
            .find(|(_, c)| {
                c.element_type == element_type
                    && c.len == len
                    && self
                        .resolve(c.array_gref)
                        .is_some_and(|a| a.as_ptr() as usize == want)
            })
            .map(|(&ptr, _)| ptr)?;
        if let Some(c) = self.critical_copies.get_mut(&ptr) {
            c.opens += 1;
        }
        Some(ptr)
    }

    /// The critical copy record at `ptr`, taken out of the table (`take`, a
    /// freeing release) or left in place (`JNI_COMMIT`), and whether this
    /// release is the buffer's LAST (zt t18/hunt1: a buffer shared by several
    /// sections, [`Self::share_critical_copy`], stays in the table, counted
    /// down, until its last freeing release).
    pub(in crate::native::jni) fn critical_copy(&mut self, ptr: usize, take: bool) -> Option<(CriticalCopy, bool)> {
        if !take {
            return self.critical_copies.get(&ptr).map(|c| (*c, false));
        }
        let c = self.critical_copies.get_mut(&ptr)?;
        if c.opens > 1 {
            c.opens -= 1;
            return Some((*c, false));
        }
        self.critical_copies.remove(&ptr).map(|c| (c, true))
    }

    /// Record a `Get<Type>ArrayElements` copy of `array` at `ptr` (`len`
    /// initialised elements of a `cap`-element `Vec`), minting the strong
    /// global ref that keeps the array alive and follows it through every
    /// relocation until the final release. gc-common w10-c.
    pub(in crate::native::jni) fn open_elem_copy(&mut self, ptr: usize, len: usize, cap: usize, array: ObjectRef) {
        let array_gref = self.add(array);
        if let Some(stale) = self.elem_copies.insert(
            ptr,
            ArrayElemBuffer {
                len,
                cap,
                array_gref,
            },
        ) {
            // Unreachable: a live buffer's address is unique, and a released
            // one's record went with its release. Never leave its root behind.
            self.remove(stale.array_gref);
        }
    }

    /// The copy record at `ptr`, taken out of the table (`take`, a freeing
    /// release) or left in place (`JNI_COMMIT`). The caller deletes the
    /// global ref once it has copied back.
    pub(in crate::native::jni) fn elem_copy(&mut self, ptr: usize, take: bool) -> Option<ArrayElemBuffer> {
        if take {
            self.elem_copies.remove(&ptr)
        } else {
            self.elem_copies.get(&ptr).copied()
        }
    }

    /// Record a `GetStringChars` buffer of `len` UTF-16 units at `ptr`.
    pub(in crate::native::jni) fn open_string_copy(&mut self, ptr: usize, len: usize) {
        self.string_copies.insert(ptr, len);
    }

    /// Take the `GetStringChars` record at `ptr` out of the table.
    pub(in crate::native::jni) fn take_string_copy(&mut self, ptr: usize) -> Option<usize> {
        self.string_copies.remove(&ptr)
    }

    /// How many element and string copies native code has not released yet in
    /// this VM. Nonzero at the end of a run means a native leaked a `Get` (for
    /// an element copy that is an array kept alive by its global ref). Not
    /// `pub`: only the tests read it (`no_test_only_public_api`).
    pub(in crate::native::jni) fn outstanding_copies(&self) -> usize {
        self.elem_copies.len()
            + self.string_copies.len()
            + self.critical_copies.len()
            + self.critical_pins.values().map(Vec::len).sum::<usize>()
    }

    /// Create a WEAK global ref for `obj` (gc-common w8-c). Returns a tagged
    /// `JObject` handle, encoded like a strong one. See the section comment
    /// above for what makes it weak.
    pub fn add_weak(&mut self, obj: ObjectRef) -> JObject {
        let address = obj.as_ptr() as usize;
        let raw = match self.weak_recycled.pop() {
            Some(raw) => {
                // SAFETY: the recycled slot remains owned by this table and is
                // absent from `weak` until reactivated here.
                unsafe { *(raw as *mut usize) = address };
                raw
            }
            None => Box::into_raw(Box::new(address)) as usize,
        };
        self.weak.insert(raw);
        (raw | 1) as JObject
    }

    /// Is `handle` a live WEAK entry? Cheap when no weak global exists (the
    /// strong-only hot path pays one `is_empty`).
    pub fn is_weak(&self, handle: JObject) -> bool {
        handle & 1 == 1 && !self.weak.is_empty() && self.weak.contains(&((handle & !1) as usize))
    }

    /// Strong, weak, or not a handle of this table (`None`: 0, an untagged
    /// handle, or one already deleted).
    pub fn kind(&self, handle: JObject) -> Option<JniGlobalKind> {
        if handle == 0 || handle & 1 == 0 {
            return None;
        }
        let raw = (handle & !1) as usize;
        if self.entries.contains(&raw) {
            Some(JniGlobalKind::Strong)
        } else if self.weak.contains(&raw) {
            Some(JniGlobalKind::Weak)
        } else {
            None
        }
    }

    /// The current referent address of every UNCLEARED weak entry, as
    /// `(box pointer, address)`. The sweeps snapshot this, judge the addresses
    /// with no table lock held, then apply the verdicts through
    /// [`Self::apply_weak_verdicts`].
    pub(in crate::native::jni) fn weak_snapshot(&self) -> Vec<(usize, usize)> {
        self.weak
            .iter()
            .filter_map(|&raw| {
                // Safety: raw is a valid Box<usize> pointer that we own.
                let addr = unsafe { *(raw as *const usize) };
                (addr != 0).then_some((raw, addr))
            })
            .collect()
    }

    /// Write each `(box pointer, expected address, new address)` verdict: the
    /// new address (0 = cleared) replaces the slot only if the entry still
    /// exists and still holds `expected`, so an entry deleted, or rewritten by
    /// an interleaved sweep, since the snapshot is left alone.
    pub(in crate::native::jni) fn apply_weak_verdicts(&mut self, verdicts: &[(usize, usize, usize)]) {
        for &(raw, expected, new_addr) in verdicts {
            if !self.weak.contains(&raw) {
                continue;
            }
            let slot = raw as *mut usize;
            // Safety: raw is a valid Box<usize> pointer that we own (checked
            // just above, under `&mut self`).
            unsafe {
                if *slot == expected {
                    *slot = new_addr;
                }
            }
        }
    }

    /// Create a global ref for `obj`. Returns a tagged `JObject` handle.
    pub fn add(&mut self, obj: ObjectRef) -> JObject {
        let boxed: Box<ObjectRef> = Box::new(obj);
        let raw = Box::into_raw(boxed) as usize; // OWNERSHIP: transferred to self.entries, freed by JniGlobalRefs::remove() or Drop impl
        self.entries.insert(raw);
        (raw | 1) as JObject
    }

    /// Release a global ref created by `add`. Returns `true` if found and removed.
    pub fn remove(&mut self, handle: JObject) -> bool {
        if handle & 1 == 0 {
            return false; // not a global ref handle
        }
        let raw = (handle & !1) as usize;
        if self.entries.remove(&raw) {
            // Safety: raw was created by Box::into_raw and we own it.
            unsafe { drop(Box::from_raw(raw as *mut ObjectRef)) };
            true
        } else if self.remove_weak(handle) {
            true
        } else {
            tracing::debug!("JNI remove: global ref handle {handle:#x} not found");
            false
        }
    }

    /// Release a weak global ref when the caller already knows its kind.
    /// Internal weak-door cleanup uses this to skip probing the strong-ref
    /// set before removing from `weak`.
    pub fn remove_weak(&mut self, handle: JObject) -> bool {
        if handle == 0 || handle & 1 == 0 {
            return false;
        }
        let raw = (handle & !1) as usize;
        if !self.weak.remove(&raw) {
            return false;
        }
        // gc-common w8-c: a weak entry (`DeleteWeakGlobalRef`, or a native
        // that deletes a weak handle through `DeleteGlobalRef`).
        if self.weak_recycled.len() < Self::WEAK_RECYCLE_LIMIT {
            // SAFETY: raw was created by Box::into_raw (a Box<usize>) and we
            // own it; after clearing it is retained in the recycle pool,
            // never exposed to weak-handle lookup until reactivated.
            unsafe { *(raw as *mut usize) = 0 };
            self.weak_recycled.push(raw);
        } else {
            // SAFETY: raw was created by Box::into_raw (a Box<usize>) and we
            // own it; the bounded pool is full.
            unsafe { drop(Box::from_raw(raw as *mut usize)) };
        }
        true
    }

    /// Resolve a global ref handle to the referenced ObjectRef.
    /// Returns `None` if `handle` is 0 or is not a valid global ref.
    pub fn resolve(&self, handle: JObject) -> Option<ObjectRef> {
        if handle == 0 || handle & 1 == 0 {
            return None;
        }
        let raw = (handle & !1) as usize;
        // Validate the pointer is still tracked to prevent use-after-free.
        if !self.entries.contains(&raw) {
            // gc-common w8-c: a weak entry resolves to its referent, or to
            // NULL once the referent was collected (the handle stays valid).
            if self.weak.contains(&raw) {
                // Safety: raw is a Box<usize> pointer still in self.weak.
                let addr = unsafe { *(raw as *const usize) };
                if addr == 0 {
                    return None;
                }
                // Safety: a non-zero slot is the referent's current address,
                // kept current by every sweep since it was stored.
                return Some(unsafe { ObjectRef::from_raw(addr as *mut u8) });
            }
            tracing::warn!("JNI resolve: handle {handle:#x} not found in global ref table");
            return None;
        }
        // Safety: raw was created by Box::into_raw and is still in self.entries.
        Some(unsafe { *(raw as *const ObjectRef) })
    }

    /// [`Self::resolve`] of a STRONG handle without the table's mutex and
    /// without the membership check (L5W17-5, JIT round 20 wave 11, lane pNat:
    /// the shared fast-throw instance's hand-out, `exceptions.rs`).
    ///
    /// The slot is the `Box<ObjectRef>` [`Self::add`] allocated; it moves
    /// only when the handle is removed (`remove` / `Drop` free it), and its
    /// value is rewritten only by [`Self::update_after_gc`], which collectors
    /// run inside a pause. So a mutator outside a pause reads the same value
    /// `resolve` would read under the lock.
    ///
    /// # Safety
    ///
    /// `handle` came from [`Self::add`] on a table that is still alive and is
    /// never passed to [`Self::remove`] while this may run; the caller is a
    /// mutator thread not inside a collection pause.
    pub(crate) unsafe fn resolve_strong_unlocked(handle: JObject) -> Option<ObjectRef> {
        if handle == 0 || handle & 1 == 0 {
            return None;
        }
        let raw = (handle & !1) as usize;
        Some(*(raw as *const ObjectRef))
    }

    /// How many active global refs exist.
    pub fn count(&self) -> usize {
        self.entries.len()
    }

    /// How many weak global refs exist, cleared ones included (they stay
    /// until `DeleteWeakGlobalRef`): a thread dump's `weak refs: N`
    /// (interpreter round i3 wave 81, lane L6).
    pub fn weak_count(&self) -> usize {
        self.weak.len()
    }

    /// Collect all referenced ObjectRefs into `out` (for GC root scanning).
    pub fn collect_roots(&self, out: &mut Vec<ObjectRef>) {
        for &raw in &self.entries {
            // Safety: raw is a valid Box<ObjectRef> pointer that we own.
            out.push(unsafe { *(raw as *const ObjectRef) });
        }
    }

    /// Apply a GC pointer map: update all stored ObjectRefs to their new addresses.
    pub fn update_after_gc(&mut self, pointer_map: &cratonvm_types::PointerMap) {
        for &raw in &self.entries {
            // Safety: raw is a valid Box<ObjectRef> pointer that we own.
            let slot = raw as *mut ObjectRef;
            let old_obj = unsafe { *slot };
            let old_addr = old_obj.as_ptr() as usize;
            if let Some(&new_addr) = pointer_map.get(&old_addr) {
                unsafe { *slot = ObjectRef::from_raw(new_addr as *mut u8) };
            }
        }
    }
}

impl Default for JniGlobalRefs {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for JniGlobalRefs {
    fn drop(&mut self) {
        for raw in self.entries.drain() {
            // Safety: raw was created by Box::into_raw and we own it.
            unsafe { drop(Box::from_raw(raw as *mut ObjectRef)) };
        }
        for raw in self.weak.drain() {
            // Safety: raw was created by Box::into_raw (a Box<usize>) and we
            // own it.
            unsafe { drop(Box::from_raw(raw as *mut usize)) };
        }
        for raw in self.weak_recycled.drain(..) {
            // SAFETY: these were created by Box::into_raw and are owned by the
            // recycle pool, outside both active handle sets.
            unsafe { drop(Box::from_raw(raw as *mut usize)) };
        }
    }
}

// ---------------------------------------------------------------------------
// Weak global refs: the post-collection sweeps (gc-common w8-c)
// ---------------------------------------------------------------------------

/// Add every uncleared weak global's referent to the collecting thread's
/// WATCHED set for the collection about to run.
///
/// Call it on the collecting thread AFTER `weakref_null_referents_pre_gc`
/// (which REPLACES the set) and before the collection. It is what makes
/// [`sweep_weak_global_refs`]'s verdict exact on a Generational cycle that
/// reclaims old-gen storage: the old-gen paths emit an identity pointer-map
/// entry only for a WATCHED survivor that did not move, and
/// `VmHeap::watched_pre_gc_addr_survived` judges an unwatched, unmoved
/// old-gen address dead once old-gen storage was reclaimed. Without this, a
/// live, unmoved old-gen referent would read as NULL after a major cycle
/// (never the reverse: the verdict cannot keep a dead referent). Watching an
/// address never keeps it alive; the set only answers "did it survive".
///
/// Costs nothing when no weak global exists (one lock, one empty check).
pub(crate) fn watch_weak_global_referents(shared: &SharedVm) {
    let addrs = weak_global_referent_addrs(shared);
    if addrs.is_empty() {
        return;
    }
    // Extend in place (gc-common w9-g): this used to clone the whole set, copy
    // it into a Vec and rebuild it to add these few addresses, on every pause.
    cratonvm_gc::gc_quiescence::add_watched_referents(&addrs);
}

/// The current referent address of every UNCLEARED weak global of `shared`'s
/// table, read-only (one lock, nothing allocated when the table has no weak
/// entry). Read inside a pause these are PRE-collection addresses until
/// [`sweep_weak_global_refs_with`] re-addresses them.
///
/// Callers: [`watch_weak_global_referents`] (before the collection), and gci
/// i28/wt's G1 kept-young-region verdict (`run_collection_pause`, after the
/// collection and before the sweep), which offers them to
/// `G1Collector::pinned_young_referents_unheld` so a weak global to a dead
/// object in a young region the pause kept in place is cleared as HotSpot
/// clears it (`gci-i27gr-g1-kept-young-region-weak-tables-read-region-liveness-FIXED-20261016`).
pub(crate) fn weak_global_referent_addrs(shared: &SharedVm) -> Vec<usize> {
    let refs = shared.natives.jni_global_refs.lock();
    if refs.weak.is_empty() {
        return Vec::new();
    }
    // Straight into the address list (gc-common w10-c): no intermediate
    // `(box, address)` snapshot on every pause.
    refs.weak
        .iter()
        // Safety: every `raw` is a `Box<usize>` this table owns, read
        // under its lock.
        .map(|&raw| unsafe { *(raw as *const usize) })
        .filter(|&addr| addr != 0)
        .collect()
}

/// [`sweep_weak_global_refs`] with the caller's in-place survival verdict
/// (`run_collection_pause`: a once-per-pause `addr_keyed::InPlaceVerdict`,
/// gc-common w18-a).
pub(crate) fn sweep_weak_global_refs_with(
    shared: &SharedVm,
    pointer_map: &cratonvm_types::PointerMap,
    survived: &dyn Fn(usize) -> bool,
) -> WeakGlobalSweepStats {
    let snapshot = shared.natives.jni_global_refs.lock().weak_snapshot();
    if snapshot.is_empty() {
        return WeakGlobalSweepStats::default();
    }
    let heap = &shared.mem.heap;
    let mut stats = WeakGlobalSweepStats::default();
    let mut verdicts: Vec<(usize, usize, usize)> = Vec::with_capacity(snapshot.len());
    for (raw, addr) in snapshot {
        let now = match pointer_map.get(&addr) {
            Some(&to) => {
                if to != addr {
                    stats.moved += 1;
                } else {
                    stats.retained += 1;
                }
                to
            }
            None if heap.watched_pre_gc_addr_survived(addr, pointer_map) && survived(addr) => {
                stats.retained += 1;
                addr
            }
            None => {
                stats.cleared += 1;
                0
            }
        };
        if now != addr {
            verdicts.push((raw, addr, now));
        }
    }
    if !verdicts.is_empty() {
        shared
            .natives
            .jni_global_refs
            .lock()
            .apply_weak_verdicts(&verdicts);
    }
    stats
}

/// Resolve a global handle of `shared`'s table for a VM-side or native caller,
/// with the keep-alive a WEAK handle owes: the SATB enqueue `jobject_to_obj`
/// does for a JNI native and `Reference.get()` does for a referent. A weak
/// global is not a root, so a concurrent mark may not have reached its
/// referent; a caller that stores what it resolved into an already-marked
/// object would otherwise have it freed by that cycle. Inert (one load) when
/// no cycle is marking; a strong handle pays the `is_weak` test only. The
/// enqueue runs outside the table lock (it may flush into the collector's
/// queue).
///
/// gci i4/wr: for VM-side callers that hand the object to Java (the `ldc` of a
/// lone-surrogate literal, `constants.rs`, whose surrogate-pool row is weak
/// under the weak String pool). A native resolving through
/// `NativeContext::resolve_global_root` follows that trait's contract
/// instead (`gc_reference_keep_alive` on what it hands on).
pub(crate) fn resolve_global_handle_keeping_weak_alive(
    shared: &SharedVm,
    handle: usize,
) -> Option<ObjectRef> {
    let jobj = handle as JObject;
    let (obj, weak) = {
        let refs = shared.natives.jni_global_refs.lock();
        let obj = refs.resolve(jobj);
        (obj, obj.is_some() && refs.is_weak(jobj))
    };
    if weak {
        if let Some(o) = obj {
            shared.mem.heap.referent_keep_alive(Value::Object(Some(o)));
        }
    }
    obj
}

/// Clear every weak global ref whose referent the concurrent cycle's COMPLETED
/// mark judged dead, at the remark, before anything is freed (gc-common w8-c).
///
/// `is_marked` is the collector's remark verdict over current addresses
/// (G1: bitmap + TAMS, `g1_remark_process_references`; Generational: bitmap,
/// or not sweep-eligible, `ConcurrentMarker::remark_with_reference_processing`).
/// Nothing moves in a remark, so there is no remap. `keep` lists addresses the
/// same pause is about to resurrect (the finalizable objects the reference
/// pass returns); their weak globals are kept, the phantom strength
/// [`sweep_weak_global_refs`] documents. An object reachable ONLY through such
/// a resurrected object is not in `keep` and is cleared here, one cycle
/// earlier than HotSpot would: safe (the handle reads NULL), and rare.
///
/// Why at the remark and not after the reclamation: after the remark the
/// SATB barrier is off, so a native that resolved a dead referent between the
/// remark and the free would hold a local the concurrent sweep then frees
/// (Generational), or read through it into a region cleanup freed (G1: a
/// dead object in a partly live region outlives the cycle, its dead
/// neighbours do not). Clearing here makes the handle NULL before any mutator
/// runs again. Call sites: the handoff named at [`sweep_weak_global_refs`].
pub(crate) fn clear_weak_global_refs_unmarked(
    shared: &SharedVm,
    is_marked: &dyn Fn(usize) -> bool,
    keep: &[usize],
) -> usize {
    let snapshot = shared.natives.jni_global_refs.lock().weak_snapshot();
    if snapshot.is_empty() {
        return 0;
    }
    // gch h3/jx: `keep` is the remark's whole resurrection list; a linear
    // `contains` per weak global was O(weak globals x finalizables) inside the
    // remark pause. A set once the list is not tiny; same answers.
    // addr-keyed: pause-local -- the remark's resurrection list, dropped before the pause ends.
    let keep_set: Option<rustc_hash::FxHashSet<usize>> =
        (keep.len() > 16).then(|| keep.iter().copied().collect());
    let kept = |addr: usize| match &keep_set {
        Some(set) => set.contains(&addr),
        None => keep.contains(&addr),
    };
    let verdicts: Vec<(usize, usize, usize)> = snapshot
        .into_iter()
        .filter(|&(_, addr)| !is_marked(addr) && !kept(addr))
        .map(|(raw, addr)| (raw, addr, 0))
        .collect();
    if !verdicts.is_empty() {
        shared
            .natives
            .jni_global_refs
            .lock()
            .apply_weak_verdicts(&verdicts);
    }
    verdicts.len()
}

/// The out-of-pause half: clear every weak global whose referent is no longer
/// a live object in place, with an EMPTY pointer map (nothing moved). Driven by
/// `addr_keyed::sweep_address_keyed_tables` after the reclamations that are
/// not followed by a collection epilogue (the Generational concurrent sweep,
/// G1's remark cleanup). Same verdict as the other address-keyed sweeps
/// there: `addr_keyed::survived_in_place`.
pub(crate) fn sweep_weak_global_refs_in_place(shared: &SharedVm) -> usize {
    let snapshot = shared.natives.jni_global_refs.lock().weak_snapshot();
    if snapshot.is_empty() {
        return 0;
    }
    let heap = &shared.mem.heap;
    let verdicts: Vec<(usize, usize, usize)> = snapshot
        .into_iter()
        .filter(|&(_, addr)| !crate::memory::addr_keyed::survived_in_place(heap, addr))
        .map(|(raw, addr)| (raw, addr, 0))
        .collect();
    if !verdicts.is_empty() {
        shared
            .natives
            .jni_global_refs
            .lock()
            .apply_weak_verdicts(&verdicts);
    }
    verdicts.len()
}

/// Clear every weak global whose referent lies in one of `spans` (freed spans,
/// sorted by start, non-overlapping). A pure range test, safe UNDER the
/// old-gen guard: it takes only this table's lock, a leaf that is never held
/// while the heap is asked (see [`sweep_weak_global_refs`]). Driven by
/// `addr_keyed::drop_address_keyed_rows_in`.
pub(crate) fn clear_weak_global_refs_in_spans(
    shared: &SharedVm,
    spans: &[(usize, usize)],
) -> usize {
    if spans.is_empty() {
        return 0;
    }
    let refs = shared.natives.jni_global_refs.lock();
    if refs.weak.is_empty() {
        return 0;
    }
    // In place, under the one lock hold (gc-common w10-c): this used to copy
    // every uncleared weak entry into a snapshot `Vec`, filter it into a
    // second, then walk the set again to apply them, for a verdict that needs
    // no lock released in between (a pure range test).
    let mut cleared = 0usize;
    for &raw in &refs.weak {
        let slot = raw as *mut usize;
        // Safety: `raw` is a `Box<usize>` this table owns; the table lock is
        // held, and every other access to a weak slot takes it.
        unsafe {
            let addr = *slot;
            if addr != 0 && crate::memory::addr_keyed::in_spans(addr, spans) {
                *slot = 0;
                cleared += 1;
            }
        }
    }
    cleared
}

// ---------------------------------------------------------------------------
// Local Reference Frame Stack
// ---------------------------------------------------------------------------
//
// JNI local refs are raw heap pointers (bit 0 = 0). Native code can call
// PushLocalFrame/PopLocalFrame to scope their lifetime. We maintain a per-
// thread stack of frames in JNI TLS; each frame is a Vec<JObject>.
//
// Thread-local local frame stack — used only while JNI is active.

thread_local! {
    pub(in crate::native::jni) static JNI_LOCAL_FRAMES: std::cell::RefCell<Vec<Vec<JObject>>> =
        std::cell::RefCell::new(Vec::new());
}

thread_local! {
    /// gc-common w7-c: the depths (indices into [`JNI_LOCAL_FRAMES`]) of the
    /// frames NATIVE CODE opened with `PushLocalFrame`, ascending. Every other
    /// frame is one the VM opened around a call (the dispatch's implicit
    /// frame, a foreign thread's per-call frame), and `PopLocalFrame` must
    /// never pop one of those: see [`jni_pop_local_frame`].
    pub(in crate::native::jni) static JNI_EXPLICIT_FRAME_DEPTHS: std::cell::RefCell<Vec<usize>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// The most slots [`push_local_frame`] reserves up front. A frame grows on
/// demand past it; the reservation is only a hint.
pub(in crate::native::jni) const LOCAL_FRAME_RESERVE_MAX: usize = 4096;

/// Push a new local frame onto this thread's local frame stack.
///
/// gc-common w10-c: `capacity` is a hint, reserved up to
/// [`LOCAL_FRAME_RESERVE_MAX`]. It came straight from `PushLocalFrame`, and
/// `Vec::with_capacity` of a native's `PushLocalFrame(env, INT_MAX)` is a 16 GiB
/// reservation that aborts the process instead of answering.
pub fn push_local_frame(capacity: usize) {
    let want = capacity.clamp(16, LOCAL_FRAME_RESERVE_MAX);
    // gcd d10/j: reuse the storage of the last closed frame when it is big
    // enough (every JNI native dispatch opens and closes one implicit frame).
    let frame = take_spare_local_frame(want).unwrap_or_else(|| Vec::with_capacity(want));
    JNI_LOCAL_FRAMES.with(|f| {
        f.borrow_mut().push(frame);
    });
}

/// gcd d10/j: the largest frame storage (in slots) kept for reuse; a bigger
/// one is freed as before, so a native that once minted thousands of locals
/// does not pin that much memory for the thread's life.
pub(in crate::native::jni) const SPARE_LOCAL_FRAME_MAX: usize = 64;

/// The spare frame storage ([`NativeCallTls::spare_local_frame`]) when it has
/// room for `want` slots. Always empty.
pub(in crate::native::jni) fn take_spare_local_frame(want: usize) -> Option<Vec<JObject>> {
    JNI_NATIVE_CALL
        .try_with(|t| {
            let spare = t.spare_local_frame.take();
            if spare.capacity() >= want {
                Some(spare)
            } else {
                // Too small for this frame: keep it for the next one.
                t.spare_local_frame.set(spare);
                None
            }
        })
        .ok()
        .flatten()
}

/// Keep `frame`'s storage for the next [`push_local_frame`], emptied, when no
/// storage is kept yet and it is not oversized; otherwise drop it.
pub(in crate::native::jni) fn keep_spare_local_frame(mut frame: Vec<JObject>) {
    if frame.capacity() == 0 || frame.capacity() > SPARE_LOCAL_FRAME_MAX {
        return;
    }
    frame.clear();
    let _ = JNI_NATIVE_CALL.try_with(|t| {
        let current = t.spare_local_frame.take();
        if current.capacity() >= frame.capacity() {
            t.spare_local_frame.set(current);
        } else {
            t.spare_local_frame.set(frame);
        }
    });
}

/// How many local frames this thread has open. A VM-side scope records it
/// before opening its frame and hands it back to [`truncate_local_frames`]
/// when the scope ends.
pub fn local_frame_depth() -> usize {
    JNI_LOCAL_FRAMES
        .try_with(|f| f.try_borrow().map_or(0, |s| s.len()))
        .unwrap_or(0)
}

