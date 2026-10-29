// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! BUG-03 — cross-thread stop-the-world JIT conservative root scan.
//!
//! ## The gap this closes
//!
//! A stop-the-world GC initiator marks every *parked* mutator from that
//! thread's published `root_snapshot`. A thread that is executing
//! JIT-compiled code may not cooperatively reach an interpreter safepoint in
//! time, so this scan remains the backstop.
//!
//! This paragraph used to say flatly that "JIT code does not poll the STW
//! flag". That was written when it was true, then stayed on the page after
//! `emit_safepoint_poll` started emitting polls — and it was ACCIDENTALLY
//! true again for dispatch-free compiled methods, because the fast-path
//! entry doors in `jit_bridge.rs` never installed the `JIT_THREAD` TLS that
//! `jit_safepoint_slow_path_body` needs, so those polls called a helper that
//! did nothing. Both halves are fixed: the doors install the thread, and this
//! note no longer claims a property the emitter contradicts. What remains
//! true, and is the reason this module exists, is that a poll is only reached
//! at a poll SITE — a thread between two polls, or in a method compiled
//! before the safepoint-flag address was available, is still invisible to the
//! cooperative protocol. Its live object roots
//! therefore live only in its CPU registers and JIT stack frames, which the
//! cross-thread collector cannot see (the thread-local conservative scanner
//! runs on the *collector's* stack, not the peer's). Its only coverage is
//! the snapshot it published at its last object-returning native call — and
//! if its JIT slots advanced past that snapshot, the collector is blind to
//! the new roots and the non-moving sweep reclaims a still-live object. The
//! peer then dereferences the freed slot → SIGSEGV (the classic truncated /
//! garbage pointer crash). See `conservative_roots::warn_cross_thread_jit_gap`.
//!
//! ## The fix (Windows)
//!
//! Instead of relying on the in-JIT peer to *cooperatively* arrive at a
//! safepoint (which it may never do while spinning in a JIT loop — the
//! `wait_for_all` hang), the collector forcibly stops it at the OS level:
//!
//!   1. Take the roster of registered Java threads' OS tids (both OS arms;
//!      the full process enumeration is the `CRATONVM_XT_ROOT_SCAN_AUDIT` path).
//!   2. `SuspendThread` + `GetThreadContext` each one.
//!   3. If its `Rip` is inside a registered JIT code range, the thread is
//!      executing pure JIT instructions and holds **no** VM/Rust lock (every
//!      lock-taking helper is itself Rust code, so its `Rip` would be outside
//!      the JIT range). It is safe to keep it frozen across the collection.
//!      Conservatively scan its 16 integer registers and its used stack for
//!      heap object addresses and add them as roots. (The probe is the
//!      caller's: `takeover_word_probe`, an exact base first and then, since
//!      gc-common w2-c, a derived pointer -- the Generational interior lookup
//!      takes heap mutexes, which a peer frozen in compiled code cannot hold.
//!      gci i1/fr's opt-in `take_over_pass_split` gives the words above the
//!      innermost compiled segment a starts-only probe.)
//!   4. Otherwise (the thread is in the interpreter / a native / already
//!      parked) resume it immediately and let it arrive cooperatively — it
//!      either reaches a safepoint and parks, or it is a blocking-native
//!      thread already excluded from the barrier. We must NOT keep such a
//!      thread frozen: it may hold a lock the collector needs (deadlock).
//!
//! The taken-over (frozen) peers are excluded from the barrier's `expected`
//! count (so `wait_for_all` cannot hang on them) and resumed once the
//! collection has completed.
//!
//! ## Why this is sound
//!
//!   * The conservative roots contributed here are never relocated, so an
//!     interior / false-positive pointer can never be mis-rewritten.
//!
//!     arch-2026-07-26 (`moving-young-precise-roots`): this used to be stated
//!     as a *consequence* — "a frozen in-JIT peer keeps its JIT-entry guard
//!     live, so `gc_quiescence::is_active()` stays true for the whole
//!     collection and the heap performs a non-moving sweep". That inference
//!     held only while `is_active()` unconditionally forced the non-moving
//!     sweep. Under a moving young generation it does not: `is_active()` is
//!     precisely the condition moving-young is designed to run *through*. Each
//!     pass below therefore asserts the obligation itself, calling
//!     `gc_quiescence::mark_moving_young_coverage_incomplete_because` whenever
//!     it actually contributes a peer's conservative roots — this collection
//!     cannot rewrite a frozen peer's registers, so it must not move. (gci
//!     i3/tk: through `mark_takeover_peer_frozen`, whose opt-in pinned door
//!     still marks the cycle incomplete; only the un-rewritable bit, i.e.
//!     selective promotion, is licensed per pause.)
//!   * `is_object_address` is lock-free and inclusive: a false positive only
//!     inflates retention. It is NOT complete for a peer frozen at an
//!     arbitrary instruction: it accepts exact object bases only, so an object
//!     a compiled loop names only through a derived pointer (an interior or
//!     one-past-the-end cursor) would be missed by an exact-base probe, so
//!     the take-over pass uses `takeover_word_probe`, which also resolves
//!     derived pointers (`CRATONVM_XT_TAKEOVER_INTERIOR`, default ON since
//!     gc-common w2-c, `=0` is the kill switch; see
//!     `docs/feature-designs/jdk-only-mode.md`).
//!     The helper-window pass resolves interior pointers by default too.
//!   * Only threads whose `Rip` is in JIT code (lock-free instruction stream)
//!     are held across the mark/sweep, so the collector can never deadlock on
//!     a lock owned by a frozen thread.
//!
//! Default ON (opt-out): set `CRATONVM_XT_JIT_ROOT_SCAN=0` to disable. Flipped
//! from default-off after it was validated to suppress the
//! multi-thread-in-JIT-under-STW root gap on the Tomcat suite (see [`enabled`]).

use crate::types::ObjectRef;
use std::sync::atomic::{AtomicU64, Ordering};

/// Number of times a peer thread was taken over (suspended in JIT and
/// conservatively scanned). Exposed for tests / JFR / the validation gate.
pub static XT_THREADS_TAKEN_OVER: AtomicU64 = AtomicU64::new(0);
/// Number of conservative roots contributed by taken-over peers.
pub static XT_ROOTS_FOUND: AtomicU64 = AtomicU64::new(0);
/// A4 (fork6-fjp) — number of helper-window peers scanned: threads whose `Rip`
/// was OUTSIDE JIT code at the STW but whose native stack still held JIT
/// frames (a blocked FJP worker under compiled `runWorker`/`doExec`, or a peer
/// inside a Rust runtime helper called from JIT).
pub static XT_HELPER_WINDOWS_SCANNED: AtomicU64 = AtomicU64::new(0);

/// Helper windows DISCHARGED by pinning the peer's conservative roots, and
/// those that still refused the collection.
///
/// The pair is the point: `pinned` alone cannot say whether the refusal is
/// gone, and `refused` alone cannot say whether the pass ever ran. Zero in both
/// means no peer was caught inside a helper.
pub static XT_HELPER_WINDOWS_PINNED: AtomicU64 = AtomicU64::new(0);
pub static XT_HELPER_WINDOWS_REFUSED: AtomicU64 = AtomicU64::new(0);

/// Blocked peers the helper-window pass parked (or suspended) but whose stack
/// band it could not read a single word of, and whose registers held no JIT
/// return address (gcd d10/t, `gcd-d9c-linux-xt-band-judgments-...` item 2).
/// Each is counted as an unreadable peer: it refuses the discharge, its
/// shadow stack and register candidates are marked, and it bumps
/// `XT_PEERS_UNCLASSIFIED`. Lifetime total, both OS arms; the Linux arm first
/// re-reads `/proc/self/maps` and parks the peer once more (a stack mapped
/// after the pass's snapshot is the known cause there).
pub static XT_HELPER_WINDOW_BANDLESS_PEERS: AtomicU64 = AtomicU64::new(0);

/// What one blocked peer the helper-window pass read contributes (gcd d10/t).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(
    not(any(test, windows, all(target_os = "linux", target_arch = "x86_64"))),
    allow(dead_code)
)]
pub(crate) enum BlockedPeerRead {
    /// A JIT return address in its registers or its band: a helper window.
    Window,
    /// A band was read and holds no JIT address: a pure-native stack, nothing
    /// this pass exists for.
    NativeOnly,
    /// NO band word was read and the registers name no compiled code. Its
    /// compiled frames, if it has any, are in no root set -- the peer is
    /// UNKNOWN, not native. Until gcd d10/t both arms skipped it as
    /// `NativeOnly`: not a window, not unreadable, so nothing refused the
    /// cycle and nothing marked its shadow stack (the Windows arm's
    /// `Snapshot::Failed` was already counted; its empty `Snapshot::Ok` band
    /// was not).
    Unreadable,
}

/// The rule behind [`BlockedPeerRead`]: `has_jit` from the classifier over the
/// registers and the copied band, `band_words` the number of band words copied.
#[cfg_attr(
    not(any(test, windows, all(target_os = "linux", target_arch = "x86_64"))),
    allow(dead_code)
)]
pub(crate) fn blocked_peer_read(has_jit: bool, band_words: usize) -> BlockedPeerRead {
    if has_jit {
        BlockedPeerRead::Window
    } else if band_words == 0 {
        BlockedPeerRead::Unreadable
    } else {
        BlockedPeerRead::NativeOnly
    }
}

// gci i1/fr (2026-10-11) -- `gcg-g3h-helper-window-protocol-review-findings`
// item 1: a blocked peer whose PUBLISHED JIT depth is zero is not a window.
//
// The classifier calls a band a window when ANY register or band word equals
// an address inside a registered code range (`in_code_ranges`). A stale
// return address -- left in an uninitialised slot of a live Rust frame, or by
// a compiled method that returned long ago -- therefore makes a pure-native
// blocked peer a window, and on Generational every in-region word of its band
// a marking root (retention only; the census line `[xt-band] pop=helper ...
// depth=Some(0)` names such a band).
//
// The published depth (`gc_quiescence::jit_depth_of_tid`, written on every
// JIT entry-chain push before control transfers and on every pop) is the
// question the take-over already asks before signalling a peer
// (`takeover_signal_jit_only_enabled`, default ON since gce ve2): `Some(0)`
// proves the thread has no compiled frame, `None` (never published) proves
// nothing. A blocked peer cannot push or pop while the pause is active
// (`mark_blocked_region_leave` waits it out), so its depth read after the
// copy is the depth of the copied stack.
//
// What the switch changes: such a band takes the pure-native path -- its
// stack-slot captures are still recorded (the blocked-wake remap needs them,
// g5/xt), its candidates are not committed, it is not a window (so it neither
// pins nor refuses). An EMPTY band demoted this way is an unreadable peer, as
// any pure-native empty band is (`blocked_peer_read`), so it still refuses.
// The deleted g6 switch `CRATONVM_XT_HELPER_WINDOW_JIT_ONLY` skipped such peers
// before reading them and so lost their captures; this one reads them first.
//
// Census (`CRATONVM_DBG=oldmark-root-census`), on both arms and both switch
// states: one `[xt-hw-zero-depth] tid=<t> applied=<bool>` line per band the
// rule demotes (switch on) or would demote (switch off).

/// `CRATONVM_XT_HELPER_WINDOW_ZERO_DEPTH_NATIVE=1` -- opt-in (default OFF):
/// see the section above. Read once per helper-window pass; no static.
pub fn helper_window_zero_depth_native_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_XT_HELPER_WINDOW_ZERO_DEPTH_NATIVE")
}

/// The rule of the section above: a band the classifier called a window
/// (`has_jit`) whose peer published a JIT depth of exactly zero holds no
/// compiled frame. `None` is an unknown depth and keeps the window. Pure.
pub(crate) fn zero_depth_window_is_native(has_jit: bool, depth: Option<usize>) -> bool {
    has_jit && depth == Some(0)
}

/// The `has_jit` a helper-window arm goes on with for peer `tid`: unchanged,
/// unless [`zero_depth_window_is_native`] holds and `apply` is set. Reads the
/// published depth only when it can matter (a window, and the switch or the
/// census on), after the peer was resumed or released.
#[cfg_attr(
    not(any(test, windows, all(target_os = "linux", target_arch = "x86_64"))),
    allow(dead_code)
)]
fn helper_window_after_depth_check(tid: u32, has_jit: bool, apply: bool) -> bool {
    if !has_jit {
        return false;
    }
    let census = xt_census_on();
    // gci i3/tk: the never-published rule (section below), read per window
    // (a pause sees a handful) so the three call sites stay as they were.
    let unpublished = helper_window_unpublished_native_enabled();
    // Its census line also prints under the light refusal census.
    let rcensus = census || crate::jit::helper_window_refusal::census_on();
    if !apply && !unpublished && !rcensus {
        return true;
    }
    let depth = cratonvm_gc::gc_quiescence::jit_depth_of_tid(tid);
    if zero_depth_window_is_native(has_jit, depth) {
        if census {
            eprintln!("[xt-hw-zero-depth] tid={tid} applied={apply}");
        }
        return !apply;
    }
    if unpublished_window_is_native(
        has_jit,
        depth,
        cratonvm_gc::gc_quiescence::shadow_window_of_tid(tid).is_some(),
        jit_entry_publication_on(),
    ) {
        if rcensus {
            eprintln!("[xt-hw-unpublished] tid={tid} applied={unpublished}");
        }
        return !unpublished;
    }
    true
}

// gci i20/vh (2026-10-15) -- `gci-i20vh-g1-helper-windows-pin-the-frozen-bands-
// of-parked-carriers-every-pause-20261015`: a BLOCKED peer whose blocking
// deposit deferred a complete compiled-chain walk is covered without the
// helper-window pass.
//
// The pass's premise ("a blocked peer is covered only by its
// `deposit_root_snapshot`, which never scans the JIT band") stopped being true
// on 2026-06-30 (`24cf0b19b0`, the blocking deposit runs the conservative JIT
// scan) and again with the lazy deposit (nio21 w17, default ON): a deposit that
// DEFERRED its chain left a record the collector walks every pause
// (`conservative_roots::resolve_deferred_blocked_jit_chains`), MARKING and
// PINNING every compiled frame's words exactly as the eager deposit did, and
// the wake remaps the compiled frames, the register images and the shadow
// stack (`vm_exec::apply_blocked_wake_jit_remap`). What the window adds on top
// is the rest of the band, `[rsp, stack top)`: the Rust frames of the blocking
// call and of the run loop above the chain, whose words a pure-native blocked
// peer has repaired at its wake (the stack-slot captures) but never pinned. On
// G1 every such word that lands in a live region pins that region
// (`helper_window_word_probe` answers any in-region word with itself), and a
// peer parked for the whole run (an idle ForkJoinPool carrier) pins the same
// regions on every pause: vi19-vh X2, `hw_windows=30 hw_roots~3776` per pause,
// a stable 1520-1569-address pin set holding 46 of 64 regions at `-Xmx64m`,
// against 0 under `--nojit` (no code range, no window).
//
// What the switch changes (G1 only; the driver hands the pass the peers):
// such a peer takes the pure-native path -- its stack-slot captures are
// recorded (the wake repair still owes them), its candidates are not committed,
// it is not a window (it neither pins nor refuses). Its compiled frames stay
// marked and pinned through its record. Census (the zero-depth rule's
// `CRATONVM_DBG=oldmark-root-census` or `xt-refusal-census`):
// `[xt-hw-deferred-chain] tid=<t> applied=true` per demoted band.
//
// gci i24/jp (2026-10-15) -- DEFAULT ON (`=0` kills). Why the demotion is
// sound on the default path, word population by word population, for a band
// the driver hands in (G1 only; `ThreadRegistry::blocked_os_tids_with_deferred_jit_chain`):
//
// 1. The compiled frames. Every frame of every chain entry is walked, MARKED
//    and PINNED by `resolve_deferred_blocked_jit_chains` in the same pause --
//    the walk the eager deposit makes for every blocked JIT peer on every
//    collector, over memory a blocked thread cannot change before the pause
//    ends (nio21 w17 obligations 1-5). The demoted tid set is a subset of the
//    set that walk resolves: a row needs a record, the flag up, and an owner
//    that is not dropped (the resolve's own filter; `deferred_chain_row`), and
//    a tid with any blocked row WITHOUT a record is never demoted. With the
//    lazy deposit off (`CRATONVM_GC_LAZY_BLOCKED_JIT_DEPOSIT=0`) no record
//    exists and nothing is demoted.
// 2. The Rust frames below the innermost compiled frame. A blocked region is
//    entered by a flag-raising deposit in one of two families (gci i25/rv
//    corrected "only through `begin_blocking_region*`"): the native bodies
//    behind the interpreter's and the JIT's native doors, through
//    `NativeContext::begin_blocking_region*` or the context's own blocking
//    methods (`thread_join`, `park_with_state_until`, `monitor_wait_*`), which
//    hold no reference across the wait that the door did not pin; and the
//    VM's monitor-entry helpers (`monitor_enter_contended`,
//    `monitor_enter_synchronized_method_contended`), which pin the object and
//    the popped arguments in `native_pin_roots` and read them back after the
//    wake. The JIT door roots its reference arguments in
//    `native_pin_roots` (`CompileArgPins`), rewritten by the wake and read back
//    after it, and a JIT helper that blocks itself re-reads its object after
//    the wake (`monitor_enter_blocking` returns the moved address). These are
//    the frames, and the contract, of a PURE-NATIVE blocked peer -- a band the
//    pass has never pinned, on any collector, since the pass exists: the
//    stack-slot captures (recorded here as for every band, `tail.finish` /
//    `record_helper_stack_captures`) repair a raw `ObjectRef` such a frame
//    holds across the block, and a derived pointer's base is rooted by the door
//    (gci i2/fr's audit). JNI locals are indirect (`CRATONVM_JNI_INDIRECT_LOCALS`,
//    default ON), so a JNI native's C frames hold no raw address either.
// 3. The Rust frames above the outermost chain entry (the run loop, the
//    interpreter). A thread parked at a SAFEPOINT never roots, pins or repairs
//    these (the above-chain band is read only for an unregistered compiled
//    frame, by the deposit, eagerly), and objects move under them on every
//    collector: Rust code above a compiled entry holds no raw reference across
//    the call. The blocked peer runs the same code.
// 4. The register file. The thread is in its blocking wait, with the
//    register file of a pure-native peer in the same wait: recorded for the
//    pairing census, never pinned or rewritten for such a peer (compiled code
//    keeps no reference only in a register across a GC-capable call: the
//    callee-saved oop flush and the register-local publish spill it to its
//    frame home, which point 1 covers). `jvm_thread::run_blocked_wait` states
//    the wait's own half of the contract.
//
// What the window added on top of that was a pin on every word of the band,
// i.e. the over-retention of `gci-i20vh-...`: no word population above is left
// without the cover a pure-native or safepoint-parked peer gets. The residual
// is the one every conservative capture carries (and every pure-native band
// carries today): a non-reference word that equals the start of an object this
// pause MOVED is rewritten at the wake if it still reads its captured value.
// Rows: `vi20-vh.list` P (pins, OOM) and S (soundness twins) on the opt-in;
// `vi24-jp.list` re-runs both on the default.

/// `CRATONVM_XT_HELPER_WINDOW_DEFERRED_CHAIN_NATIVE` -- DEFAULT ON since gci
/// i24/jp (`=0` restores the i20 window path): see the two sections above.
/// Acts on G1 only (the driver hands the pass the peers only there). Read by
/// the collection driver once per pause, no static.
pub fn helper_window_deferred_chain_native_enabled() -> bool {
    // WARNING gci-i24jp-flip-20261015: default flipped ON by lane i24/jp (brief: make the demotion the default), pending apps triage.
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_XT_HELPER_WINDOW_DEFERRED_CHAIN_NATIVE")
}

/// The rule of the section above: a band the classifier called a window
/// whose peer's current blocking deposit deferred its compiled chain to this
/// pause's walk. Pure.
pub(crate) fn deferred_chain_window_is_native(has_jit: bool, deferred_chain: bool) -> bool {
    has_jit && deferred_chain
}

/// The `has_jit` a helper-window arm goes on with after the depth step:
/// unchanged, unless [`deferred_chain_window_is_native`] holds. `deferred_chain`
/// is the driver's answer for this peer (`TakenOver::demotes_deferred_chain_window`),
/// `false` whenever the switch is off.
#[cfg_attr(
    not(any(test, windows, all(target_os = "linux", target_arch = "x86_64"))),
    allow(dead_code)
)]
fn helper_window_deferred_chain_step(tid: u32, has_jit: bool, deferred_chain: bool) -> bool {
    if !deferred_chain_window_is_native(has_jit, deferred_chain) {
        return has_jit;
    }
    if xt_census_on() || crate::jit::helper_window_refusal::census_on() {
        eprintln!("[xt-hw-deferred-chain] tid={tid} applied=true");
    }
    false
}

// ki3 (2026-10-16) -- `jr20-unreachable-weak-referent-survives-system-gc-in-
// some-runs` (docs/feature-designs/jit-ir-optimizations.md): a COVERED window pins its words
// but does not MARK through them.
//
// The defect. `Ki3LonelyAmp` / `R20FuzzIRefGet1`: `main` (compiled) calls
// `lonely(id)`, which returns `new WeakReference<>(new Node(id))`, then parks
// in `Semaphore.acquire` while a peer runs `System.gc()`. HotSpot clears the
// referent every run; CratonVM kept it in 5-8 of 8 tries on ZGC, G1 and
// Generational, never under `--nojit`. A root hunt (2026-10-16) named one
// producer: this pass, over the parked `main` -- its band, read word by word
// from the wait's `rsp` to the stack top, held the dead `Node` in a dozen
// slots (`rsp+0x578` .. `rsp+0x3bd0`) next to stale return addresses into
// `lonely`, `WeakReference.<init>` and `Reference.get`: dead-frame residue in
// the never-written slots of the LIVE frames of the park call, which the
// window committed as marking roots, so the referent was strongly reachable.
// The deferred-chain demotion (section above) never applied: a park, a monitor
// wait and a join enter their window through the EAGER deposit
// (`deposit_root_snapshot`), which leaves no record, and the driver hands
// peers in on G1 only.
//
// The rule. A blocked peer whose current window's deposit covered its whole
// compiled chain -- deferred it (a record) or walked it itself
// (`GcBlockState::deposit_walked_jit_chain`, the record's eager twin, set only
// by a flag-raising deposit that opens and is closed by the window, never for
// foreign code or a terminating thread) -- has every compiled frame's words
// MARKED by that deposit's snapshot or by the record's walk, and PINNED by the
// same (`publish_pinned_jit_roots` / `add_xt_cycle_pinned_jit_roots`). On
// Generational a deposit may skip the walk because the moving-young coverage
// proof holds for its whole chain (`moving_young_precise_only`): every live
// compiled frame's oops are then in the shadow stack, which the snapshot
// folds, and the deposit sets the same flag -- the proof every moving young
// cycle already relies on (without it the residual stayed: 8 of 12 runs kept
// a referent, 0 of 12 under `CRATONVM_NO_MOVING_YOUNG=1`). Its
// band adds the Rust frames of the blocking call and of the run loop and the
// register file, which hold no reference the pause must keep alive (points 2
// to 4 of the gci i24/jp section above). Two consequences:
//
// * On G1 the deferred-chain demotion takes such a peer (the driver hands
//   `set_deferred_chain_peers` the covered roster, a superset of the deferred
//   one): the eager deposit marks and pins the compiled frames, G1 band-reject
//   pins included, exactly as the record's walk does, which is obligation 1 of
//   that section. Pins alone would not do on G1: a pinned word HOLDS a
//   referent in G1's young referent verdict (`g1_pinned_referents`), so the
//   rule below still kept the `Node` (1-1 of 8 kept against 3-4 for the kill).
// * On ZGC and Generational (and on G1 with the demotion killed) such a window
//   still does everything a window does -- it pins its candidates and credits
//   its depth (or refuses), records its captures, and hands its words to the
//   driver's companion pinning -- except that its candidates are not committed
//   as MARKING roots (`TakenOver::commit_window_roots`). It keeps the pins, so
//   no pause relocates anything it would not have relocated before (a window
//   still marks the cycle `XT_HELPER_WINDOW`); the only behaviour that changes
//   is that an object named by nothing but residue in such a band is no longer
//   kept alive.
//
// Census (the zero-depth rule's `CRATONVM_DBG=oldmark-root-census` or
// `xt-refusal-census`): `[xt-hw-pin-only] tid=<t> words=<n>` per window.

/// `CRATONVM_XT_HELPER_WINDOW_COVERED_CHAIN_PIN_ONLY` -- default ON (`=0`
/// kills: every window's words are marking roots again, G1's demotion is back
/// to the deferred roster, and no deposit records `deposit_walked_jit_chain`).
/// See the section above. Read by the deposit and by the collection driver, no
/// static.
pub fn helper_window_covered_chain_pin_only_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on(
        "CRATONVM_XT_HELPER_WINDOW_COVERED_CHAIN_PIN_ONLY",
    )
}

// gci i3/tk (2026-10-12) -- `gci-i2yg-refusing-helper-windows-switch-off-
// young-promotion-for-a-whole-run-20261012` fix 2: a "window" whose peer has
// published NEITHER a JIT depth NOR a shadow window holds no compiled frame.
//
// Every interpreter->JIT entry runs `helpers::set_jit_thread`, the only
// setter of the JIT thread pointer compiled code and its helpers run on;
// with the shadow stack and the peer shadow scan on (both default ON) it
// publishes the thread's shadow-window triple under its OS tid
// (`conservative_roots::publish_self_shadow_addr_once`), and every JIT
// entry-chain push publishes the depth slot under the same tid
// (`publish_self_jit_depth`, `CRATONVM_XT_PINNED_PEER_DEPTH`, default ON).
// Both entries are dropped together, and only, by `JitDepthSlot::drop` at
// the thread's TLS teardown, after its last frame returned. So a blocked
// peer with `jit_depth_of_tid == None` AND `shadow_window_of_tid == None`
// never entered compiled code (or has left every frame for good), and the
// code address the classifier found in its band is stale: the window is a
// false positive (`gcg-g3h-...` item 1, the `depth=None` half that the
// zero-depth rule cannot reach). The i1 refusal rows
// (`yg1_cost_yp_ck_{3,7}`: ~4 such peers per pause, `sh_windows=0` for the
// whole run, `depth=None` on the `[xt-jit-roots]` lines of
// `jn1_e_zgc_roots`) fit it; `vi2-yg` block R names the reason.
//
// What the switch changes: exactly what the zero-depth rule does (the band
// takes the pure-native path: captures recorded, candidates not committed,
// not a window, an empty band still unreadable). Any of the three kill
// switches set to `0` makes an absent publication prove nothing, and the
// rule stands down.
//
// Census (`CRATONVM_DBG=oldmark-root-census` or `xt-refusal-census`), both
// arms, both switch states: `[xt-hw-unpublished] tid=<t> applied=<bool>`.

/// `CRATONVM_XT_HELPER_WINDOW_UNPUBLISHED_NATIVE` -- default ON since gci i4
/// (vi3-tk A: sound on every row, and every refusal of the refusing default
/// row was `shadow-unpublished depth=None`); `=0` is the kill. See the section
/// above. No static.
pub fn helper_window_unpublished_native_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_XT_HELPER_WINDOW_UNPUBLISHED_NATIVE")
}

/// Whether every JIT entry publishes both facts the never-published rule
/// reads (the shadow stack, the peer shadow scan and the depth credit, all
/// default ON). Cached readers, no lock.
#[cfg_attr(
    not(any(test, windows, all(target_os = "linux", target_arch = "x86_64"))),
    allow(dead_code)
)]
fn jit_entry_publication_on() -> bool {
    crate::jit::conservative_roots::shadow_stack_enabled()
        && crate::jit::conservative_roots::xt_peer_shadow_scan_enabled()
        && crate::jit::conservative_roots::xt_pinned_peer_depth_enabled()
}

/// The rule of the section above: a band the classifier called a window
/// whose peer published no JIT depth and no shadow window, while every JIT
/// entry publishes both, holds no compiled frame. A published depth of any
/// value (zero is the other rule's), or a published window, keeps it. Pure.
#[cfg_attr(
    not(any(test, windows, all(target_os = "linux", target_arch = "x86_64"))),
    allow(dead_code)
)]
pub(crate) fn unpublished_window_is_native(
    has_jit: bool,
    depth: Option<usize>,
    shadow_published: bool,
    publication_on: bool,
) -> bool {
    has_jit && publication_on && depth.is_none() && !shadow_published
}

// gci i3/tk (2026-10-12) -- a frozen peer or a refusing helper window need
// not switch off Generational selective promotion (`gci-i1jn-takeover-of-a-
// compiled-peer-forbids-promotion-on-every-young-cycle-20261011` X2, and
// fix 3 of `gci-i2yg-refusing-helper-windows-...`). Two opt-in switches,
// Generational only.
//
// The promotion gate (`gen_heap` `selective_on`, through
// `gc_quiescence::unrewritable_peer_state`) was written for one hazard: a
// frozen or blocked peer whose register or stack holds ONLY a derived
// pointer to an object, which did not resolve to a root, so pin-by-value
// left the base promotable. Both probes resolve derived pointers now -- the
// take-over's `takeover_word_probe` + companion (`CRATONVM_XT_TAKEOVER_
// INTERIOR`, default ON) and the helper window's `helper_window_word_probe`
// + companion (`CRATONVM_XT_HELPER_WINDOW_PIN_RESOLVE`, default ON) -- and
// the sweep's pin set pins every root value AND the base of an interior one
// (`gen_heap`, "Pin the base too"); on an incomplete cycle it also pins the
// movable shadow roots (`honour_movable`). So every object such a peer's
// words can reach is pinned, which is all selective promotion needs. The
// cycle stays incomplete (no Cheney copy: a copy cannot honour a pin set).
//
// What each licence must add:
// * take-over: the frozen peer's SHADOW window read whole (it is never
//   remapped: the peer resumes straight into compiled code), its machine
//   stack read whole, no running unclassified peer, no FFM native claim, and
//   the above-chain starts rule off (it narrows marking roots to starts).
// * helper window: every refused window's band read whole (`complete`) and
//   no unreadable blocked peer; the window's own shadow stack need not be
//   read, because a blocked peer folded it into its blocking deposit
//   (`vm_exec::deposit_root_snapshot_inner_with`, "Shadow-stack precise
//   roots"), which the sweep pins by value; and the starts-only marking rule
//   (`CRATONVM_XT_HELPER_WINDOW_ROOT_STARTS`) off.
// The pass-wide facts are known only at the end of the pause, so the
// per-peer sites record the reason through the pinned door and the driver
// (`gc_and_alloc::stw_take_over_and_wait`, [`pause_keeps_promotion`]) raises
// the un-rewritable bit whenever the pause-wide licence fails. The old-pin
// plan (`gen_heap` `OldPinnedCompact::for_this_pause`) still refuses any
// recorded peer-state reason.

/// `CRATONVM_GEN_TAKEOVER_PROMOTES=1` -- opt-in (default OFF): see above.
pub fn gen_takeover_promotes_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_GEN_TAKEOVER_PROMOTES")
}

/// `CRATONVM_GEN_HELPER_WINDOW_REFUSAL_PROMOTES=1` -- opt-in (default OFF):
/// see above.
pub fn gen_helper_window_refusal_promotes_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_GEN_HELPER_WINDOW_REFUSAL_PROMOTES")
}

/// The take-over's static licence for this pause. Pure.
pub(crate) fn takeover_promotes_licence(
    generational: bool,
    switch: bool,
    interior: bool,
    above_chain_starts: bool,
    peer_shadow_scan: bool,
) -> bool {
    generational && switch && interior && !above_chain_starts && peer_shadow_scan
}

/// The helper window's static licence for this pause. Pure.
pub(crate) fn helper_refusal_promotes_licence(
    generational: bool,
    switch: bool,
    pin_resolve: bool,
    root_starts: bool,
) -> bool {
    generational && switch && pin_resolve && !root_starts
}

/// May a helper-window pass record its refusal through the pinned door?
/// Licensed, every refused window read whole, no unreadable peer. Pure.
#[cfg_attr(
    not(any(test, windows, all(target_os = "linux", target_arch = "x86_64"))),
    allow(dead_code)
)]
pub(crate) fn helper_window_refusal_relieved(
    licensed: bool,
    refused_incomplete: usize,
    unknown_peers: usize,
) -> bool {
    licensed && refused_incomplete == 0 && unknown_peers == 0
}

/// A take-over pass's mark for one peer it froze in compiled code: the
/// un-rewritable `XT_TAKEOVER` door, or under the pause's licence the pinned
/// door (the driver re-raises the bit if the pause-wide licence fails).
#[cfg_attr(
    not(any(windows, all(target_os = "linux", target_arch = "x86_64"))),
    allow(dead_code)
)]
fn mark_takeover_peer_frozen(taken: &TakenOver) {
    let reason = cratonvm_gc::gc_quiescence::incomplete_reason::XT_TAKEOVER;
    if taken.takeover_promotes {
        cratonvm_gc::gc_quiescence::mark_moving_young_coverage_incomplete_pinned_because(reason);
    } else {
        cratonvm_gc::gc_quiescence::mark_moving_young_coverage_incomplete_because(reason);
    }
}

/// A helper-window pass's refusal mark (both arms): the un-rewritable
/// `XT_HELPER_WINDOW` door, or the pinned door when
/// [`helper_window_refusal_relieved`] holds, which it then records on
/// `taken` for the driver.
#[cfg_attr(
    not(any(windows, all(target_os = "linux", target_arch = "x86_64"))),
    allow(dead_code)
)]
fn mark_helper_window_refusal(taken: &TakenOver, refused_incomplete: usize, unknown_peers: usize) {
    let reason = cratonvm_gc::gc_quiescence::incomplete_reason::XT_HELPER_WINDOW;
    if helper_window_refusal_relieved(taken.helper_refusal_promotes, refused_incomplete, unknown_peers)
    {
        taken
            .helper_refusal_relieved
            .store(true, std::sync::atomic::Ordering::Relaxed);
        cratonvm_gc::gc_quiescence::mark_moving_young_coverage_incomplete_pinned_because(reason);
    } else {
        cratonvm_gc::gc_quiescence::mark_moving_young_coverage_incomplete_because(reason);
    }
}

/// The pause-wide facts [`pause_keeps_promotion`] reads, gathered by the
/// take-over driver after both passes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct PausePromoteFacts {
    /// Peers held by the take-over (`TakenOver::count`).
    pub(crate) frozen: usize,
    /// FFM native claims among them (`TakenOver::native_claim_count`).
    pub(crate) native_claims: usize,
    /// [`takeover_promotes_licence`] for this pause.
    pub(crate) takeover_licensed: bool,
    /// A frozen peer's machine stack was not read whole (the pause ledger).
    pub(crate) stack_incomplete: bool,
    /// Frozen peers whose shadow window was untrusted or bandless this pause
    /// (a delta of `XT_TAKEOVER_SHADOW_UNPUBLISHED`; another VM's take-over
    /// can only raise it, which refuses).
    pub(crate) takeover_shadow_untrusted: u64,
    /// Running peers the take-over could not classify.
    pub(crate) unclassified: u64,
    /// Helper windows found (with pinned native claims, as the driver counts).
    pub(crate) helper_windows: usize,
    /// [`helper_refusal_promotes_licence`] for this pause.
    pub(crate) helper_licensed: bool,
    /// The pass recorded its refusal through the pinned door.
    pub(crate) helper_relieved: bool,
    /// Refusals the helper-window pass recorded (unpinned windows plus
    /// unreadable peers).
    pub(crate) helper_refusals: u64,
}

/// May this pause leave selective promotion on although it froze a peer or
/// found a helper window (the driver's `!discharged` branch)?
///
/// gci i5/tk: a licensed take-over pause whose helper windows were ALL
/// pinned (no refusal, no unreadable peer) keeps promotion without the
/// helper licence, when a helper-only pause with the same windows would
/// have been discharged and promoted ([`pinned_helper_windows_keep_promotion`]).
/// Before, such a pause needed `CRATONVM_GEN_HELPER_WINDOW_REFUSAL_PROMOTES`
/// although nothing refused, so `CRATONVM_GEN_TAKEOVER_PROMOTES` alone lost
/// every pause where a frozen peer met a pinned window. The switch is read
/// only on that shape (a licensed take-over), so the default path reads
/// nothing new.
pub(crate) fn pause_keeps_promotion(f: &PausePromoteFacts) -> bool {
    let pinned_ok = f.frozen > 0
        && f.takeover_licensed
        && f.helper_windows > 0
        && f.helper_refusals == 0
        && pinned_helper_windows_keep_promotion();
    pause_keeps_promotion_with(f, pinned_ok)
}

/// [`pause_keeps_promotion`]'s rule, with the pinned-window fact given.
/// `pinned_windows_promote`: every helper window of the pause was pinned and
/// a discharged helper-only pause would promote with them. Pure.
pub(crate) fn pause_keeps_promotion_with(f: &PausePromoteFacts, pinned_windows_promote: bool) -> bool {
    if f.native_claims > 0 || f.unclassified > 0 {
        return false;
    }
    let frozen_ok = f.frozen == 0
        || (f.takeover_licensed && !f.stack_incomplete && f.takeover_shadow_untrusted == 0);
    let helper_ok = (f.helper_windows == 0 && f.helper_refusals == 0)
        || (f.helper_refusals == 0 && pinned_windows_promote)
        || (f.helper_licensed && (f.helper_refusals == 0 || f.helper_relieved));
    let engaged = (f.frozen > 0 && f.takeover_licensed) || (f.helper_windows > 0 && f.helper_licensed);
    frozen_ok && helper_ok && engaged
}

/// Would a helper-only pause whose windows were all pinned be discharged
/// AND keep promotion? The driver discharges it under
/// [`helper_window_discharge_enabled`] (default ON) and then raises the
/// un-rewritable bit only under `CRATONVM_XT_KEEP_UNREWRITABLE_ON_DISCHARGE`
/// (opt-in, read by presence in `gc_and_alloc::keep_unrewritable_peer_state_on_discharge`;
/// the same presence rule here). With discharge killed the pass itself
/// records the window through the un-rewritable door (or the relieved pinned
/// door under the helper licence), so answering `false` then changes
/// nothing. The default `discharge` + promote shape is the vi3-tk
/// `tk3_d_pin1_*` rows (`hw_pinned` = `helper_windows`, `sp_selective` 24).
fn pinned_helper_windows_keep_promotion() -> bool {
    helper_window_discharge_enabled()
        && cratonvm_types::flags::runtime_var_os("CRATONVM_XT_KEEP_UNREWRITABLE_ON_DISCHARGE")
            .is_none()
}

// Helper windows THIS CYCLE that could not be pinned (a partial scan, or the
// pin switch off), plus blocked peers whose stack could not be read at all
// (gc-common w1-c, 2026-09-23). Per-cycle, unlike the two lifetime totals
// above, because the discharge decision is per-cycle: a collection may relocate
// only if EVERY window it saw is covered. Both OS arms pin (the Windows one
// since 2026-09-02).
//
// PER PAUSE since gcd d2/i (2026-09-27): the row lives in the pause's
// `gc_quiescence::PauseLedger` (`set_xt_helper_windows_unpinned` /
// `xt_helper_windows_unpinned`), not in the process static
// `XT_HELPER_WINDOWS_UNPINNED_CYCLE` it was. Another VM's take-over stored its
// own 0 over this VM's refusal between this VM's pass and its reads; see the
// thread audit beside `gc_quiescence::set_xt_helper_windows_unpinned`.
//
// Reset by [`reset_helper_window_cycle`] at the top of every take-over (and by
// the ledger's `reset_xt_cycle` / pause open), not only inside the pass: the
// pass is skipped outright on a cycle with no blocked thread, and a value left
// over from the previous cycle would then answer for this one.

/// Open a new cycle for the helper-window verdict. Called by
/// `stw_take_over_and_wait` beside `gc_quiescence::reset_xt_cycle`, and by the
/// pass itself before any early return.
pub fn reset_helper_window_cycle() {
    cratonvm_gc::gc_quiescence::set_xt_helper_windows_unpinned(0);
}

/// This cycle's refusing helper windows (unpinned windows plus unreadable
/// blocked peers), from the pause's ledger. During thread-local teardown the
/// ledger cannot be read and this answers `1`: a refusal, the fail-closed
/// reading for both of its uses (the discharge and the `TakeoverVerdict`'s
/// `unreadable` term).
pub fn helper_windows_unpinned_this_cycle() -> u64 {
    cratonvm_gc::gc_quiescence::xt_helper_windows_unpinned().unwrap_or(1)
}

/// May this cycle's helper windows be discharged instead of refusing?
///
/// True only when the pass pinned every window it saw. Read by
/// `interpreter::gc_and_alloc`, which raises the SECOND (unlabelled) refusal,
/// and by the coverage accounting in `conservative_roots`. False when the
/// pause's ledger cannot be read (see [`helper_windows_unpinned_this_cycle`]).
pub fn helper_windows_all_pinned_this_cycle() -> bool {
    helper_windows_unpinned_this_cycle() == 0
}

/// Credit the blocked `monitorenter` peers whose blocking deposit PROVED their
/// JIT entry chain rewritable (gcd d2/i, opt-in
/// `CRATONVM_XT_BLOCKED_MONITOR_PROOF`; see
/// `cratonvm_gc::gc_quiescence::blocked_monitor_proof_enabled`), and return
/// the OS tids the helper-window pass must still scan: `rest` plus every
/// proven peer whose credit could not be made.
///
/// A proven peer is credited only when the depth its deposit proved equals the
/// depth it published (`depth_of`, `gc_quiescence::jit_depth_of_tid` in
/// production): the coverage accounting subtracts published depth, so a
/// larger published depth would be a shortfall anyway and a smaller one would
/// be an over-credit, the one unsound direction of that ledger. Such a peer is
/// scanned exactly as before.
///
/// `credit` receives each credited depth (`gc_quiescence::add_peer_proven_jit_depth`,
/// into the pause's own ledger, on the initiator). The two closures are
/// parameters so the rule is testable without a published depth slot.
pub(crate) fn credit_proven_blocked_monitor_peers(
    mut rest: Vec<u32>,
    proven: &[(u32, usize)],
    depth_of: impl Fn(u32) -> Option<usize>,
    mut credit: impl FnMut(usize),
) -> Vec<u32> {
    let (mut credited, mut depth_total, mut rescanned) = (0usize, 0usize, 0usize);
    // gcd d3/m: when the pause's young pin ledger licenses a relocating cycle
    // (the pinned copy, or term 4's option B), a credited peer's band must
    // also be read into that ledger, or the ledger reads its JIT depth as a
    // shortfall and the cycle diverts anyway. The pass reads them; see
    // `gc_quiescence::stash_proven_monitor_peer_for_ledger`.
    let ledger_capture = young_pin_ledger_licenses_moves();
    for &(tid, depth) in proven {
        if depth_of(tid) == Some(depth) {
            credit(depth);
            if ledger_capture {
                cratonvm_gc::gc_quiescence::stash_proven_monitor_peer_for_ledger(tid, depth);
            }
            credited += 1;
            depth_total += depth;
            cratonvm_gc::gc_quiescence::XT_BLOCKED_MONITOR_PEERS_PROVEN
                .fetch_add(1, Ordering::Relaxed);
        } else {
            rescanned += 1;
            cratonvm_gc::gc_quiescence::XT_BLOCKED_MONITOR_PEERS_DEPTH_MISMATCH
                .fetch_add(1, Ordering::Relaxed);
            rest.push(tid);
        }
    }
    if dbg() && !proven.is_empty() {
        eprintln!(
            "[xt-jit-roots] blocked-monitor proof: credited={credited} depth={depth_total} rescanned={rescanned}"
        );
    }
    rest
}

/// Does this pause's young pin ledger license a RELOCATING young cycle under
/// live compiled frames -- the pinned in-place copy
/// (`CRATONVM_GEN_PINNED_YOUNG_COPY`) or term 4's option B
/// (`CRATONVM_GEN_YOUNG_PIN_LEDGER_TERM4`)? The same question
/// `conservative_roots::young_pin_ledger_licenses_moves` asks for the
/// deposits; with both off nothing here reads a band into the ledger.
/// JIT r20 wave 18, lane gcI: and precise-root promotion
/// (`gc_quiescence::young_pin_ledger_licenses_moves`, the one definition).
fn young_pin_ledger_licenses_moves() -> bool {
    cratonvm_gc::gc_quiescence::young_pin_ledger_licenses_moves()
}

/// g1ep7: `CRATONVM_XT_HELPER_PRESIGNAL` (default ON, `=0` kills): the
/// Linux helper-window pass signals every blocked peer it will park before
/// waiting on any (see the pass). Read once per pass.
#[cfg_attr(
    not(any(windows, all(target_os = "linux", target_arch = "x86_64"))),
    allow(dead_code)
)]
fn helper_window_presignal_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_XT_HELPER_PRESIGNAL")
}

/// gcd d3/m: does the helper-window pass read each PINNED window's band into
/// the young pin ledger? Only under `CRATONVM_GEN_PINNED_YOUNG_COPY_TAKEOVER`
/// with the pinned copy itself on (the only consumer); off, the pass is
/// byte-for-byte as before. See
/// `cratonvm_gc::gc_quiescence::pinned_young_copy_takeover_enabled`.
#[cfg_attr(
    not(any(windows, all(target_os = "linux", target_arch = "x86_64"))),
    allow(dead_code)
)]
fn helper_window_band_capture_enabled() -> bool {
    cratonvm_gc::gc_quiescence::generational_takeover_pins_honoured()
}

/// gcd d3/m: append every word of `words` that lies in a published young
/// region (`gc_quiescence::young_pin_range`, low slack and one-past-end
/// included) to `out`, RAW -- a band read into the young pin ledger pins
/// every young word it holds, bases, interior and derived words alike.
#[cfg_attr(
    not(any(test, windows, all(target_os = "linux", target_arch = "x86_64"))),
    allow(dead_code)
)]
fn collect_young_band_words(
    range: &cratonvm_gc::gc_quiescence::YoungPinRange,
    words: impl IntoIterator<Item = usize>,
    out: &mut Vec<usize>,
) {
    out.extend(words.into_iter().filter(|&w| range.contains(w)));
}

/// Engagement census for [`scan_peer_shadow_window`].
///
/// Without these a clean result cannot be told apart from a scan that never
/// ran: a peer whose shadow stack is legitimately empty and a peer whose window
/// was never read both contribute zero roots. `WINDOWS` is the denominator,
/// `SLOTS` says whether the windows had anything in them, and `UNTRUSTED`
/// counts the windows that refused the pin rather than claim coverage.
pub static XT_PEER_SHADOW_WINDOWS: AtomicU64 = AtomicU64::new(0);
pub static XT_PEER_SHADOW_SLOTS: AtomicU64 = AtomicU64::new(0);
pub static XT_PEER_SHADOW_ROOTS: AtomicU64 = AtomicU64::new(0);
pub static XT_PEER_SHADOW_UNTRUSTED: AtomicU64 = AtomicU64::new(0);

/// Scan a frozen blocked peer's SHADOW STACK, appending every heap address it
/// names to `out`.
///
/// The window is `[base, top)` of the `ShadowStack` the peer published the
/// address of (`gc_quiescence::publish_self_shadow_addr`). Reading it is sound
/// only for a peer that cannot run: `mark_blocked_region_leave` waits out an
/// active pause, so a blocked peer's `top` is stable for the whole STW.
///
/// Every field is validated before use. The address came from another thread
/// and a stale or torn one would be dereferenced here -- the same shape of
/// mistake that SIGSEGV'd the band verifier on a `base` of `0x5555_0000_0004`.
/// Returns the number of slots scanned, or `None` if the window could not be
/// trusted (which must keep the cycle refusing rather than claim coverage).
pub fn scan_peer_shadow_window<F>(
    os_tid: u32,
    is_obj: &F,
    out: &mut Vec<ObjectRef>,
) -> Option<usize>
where
    F: Fn(usize) -> Option<ObjectRef>,
{
    let found_before = out.len();
    match read_peer_shadow_window(os_tid, is_obj, out) {
        Some(slots) => {
            XT_PEER_SHADOW_WINDOWS.fetch_add(1, Ordering::Relaxed);
            XT_PEER_SHADOW_SLOTS.fetch_add(slots as u64, Ordering::Relaxed);
            XT_PEER_SHADOW_ROOTS.fetch_add((out.len() - found_before) as u64, Ordering::Relaxed);
            Some(slots)
        }
        None => {
            XT_PEER_SHADOW_UNTRUSTED.fetch_add(1, Ordering::Relaxed);
            None
        }
    }
}

/// Take-over twin of the `XT_PEER_SHADOW_*` census: shadow windows read for
/// peers FROZEN IN COMPILED CODE by `take_over_pass` (gc-common w1-c,
/// 2026-09-23). Kept apart from the blocked-peer census so neither population
/// dilutes the other's denominator.
///
/// `WINDOWS` is the denominator, `ROOTS` what the reads added to the root set,
/// and `UNPUBLISHED` the frozen peers whose window could not be trusted (never
/// published, or an identity mismatch) -- those contribute registers and
/// machine stack only, exactly as before this scan existed.
pub static XT_TAKEOVER_SHADOW_WINDOWS: AtomicU64 = AtomicU64::new(0);
/// Take-over peers whose machine-stack scan hit its backstop or could not be
/// bounded at all (unreadable or unusable `Rsp`). Non-zero is worth reading:
/// those peers' outer frames were not scanned. Both OS arms count it since
/// gc-common w2-c (Windows only before). A lifetime census for the `[GC]
/// xt_takeover_shadow` exit line only: the pause's `TakeoverVerdict` reads
/// the per-pause ledger row, never this process total (see
/// [`note_takeover_stack_incomplete`]).
pub static XT_TAKEOVER_STACK_INCOMPLETE: AtomicU64 = AtomicU64::new(0);

/// Count one take-over peer whose stack could not be read whole: the lifetime
/// total [`XT_TAKEOVER_STACK_INCOMPLETE`] AND the calling thread's pause
/// ledger (`gc_quiescence::note_xt_takeover_stack_incomplete`, gcd d10/t).
///
/// The pause's `TakeoverVerdict` reads "pins complete" from the ledger row
/// (`gc_quiescence::xt_takeover_stack_incomplete_this_pause`, in
/// `stw_take_over_and_wait`). It used to read a delta of the process total,
/// which another VM's overlapping take-over also moves, so this VM refused a
/// pinned move it was licensed to make
/// (`gcd-d10t-takeover-stack-incomplete-is-read-as-a-process-delta-20260928`).
/// Every caller runs on the initiator, whose ledger is its own VM's
/// (`GcBarrier` binds it when the request wins).
#[inline]
fn note_takeover_stack_incomplete() {
    XT_TAKEOVER_STACK_INCOMPLETE.fetch_add(1, Ordering::Relaxed);
    cratonvm_gc::gc_quiescence::note_xt_takeover_stack_incomplete();
}
pub static XT_TAKEOVER_SHADOW_ROOTS: AtomicU64 = AtomicU64::new(0);
pub static XT_TAKEOVER_SHADOW_UNPUBLISHED: AtomicU64 = AtomicU64::new(0);

/// Add a TAKEN-OVER peer's shadow-stack oops to `roots`.
///
/// # The hole this closes (gc-common w1-c, 2026-09-23)
///
/// Compiled code keeps every oop that is live across a GC-capable call in the
/// thread's shadow stack -- a heap `Box<[usize]>`, not the machine stack --
/// and RELOADS it from there after the call (`cratonvm_gc::shadow_stack`). A
/// value held in a caller-saved register across a JIT->JIT call therefore
/// lives ONLY in its shadow slot while the callee runs.
///
/// `take_over_pass` freezes a peer whose `Rip` is inside compiled code and
/// scanned its register file and `[rsp, stack top)`. Neither reaches the
/// shadow stack, and the frozen-peer walk in `stw_take_over_and_wait` covers
/// interpreter frames and native side tables only. So an outer compiled
/// frame's oop, pushed after the peer's last snapshot publish and held across
/// a call into a spinning compiled callee, was in NO root set: the non-moving
/// sweep (Generational, ZGC under `XT_TAKEOVER`) freed it and G1 evacuated it
/// unpinned, and the peer resumed into a use-after-free. The helper-window
/// pass has scanned this window for BLOCKED peers since 2026-09-02; the
/// frozen-in-JIT population was never given the same read.
///
/// Sound for the same reason the blocked-peer read is: the peer cannot run
/// (OS-suspended on Windows, parked in the takeover handler on Linux), its
/// `JvmThread` cannot move or drop while it holds a live compiled frame, and
/// [`read_peer_shadow_window`] identity-checks the published triple before
/// trusting `top`. A peer frozen between a shadow store and its `top` bump
/// still holds the value in the register the store came from, which the
/// register scan covers; a slot above a stale `top` is only an over-retention.
///
/// Gated by the existing `CRATONVM_XT_PEER_SHADOW_SCAN` kill switch (default
/// ON), so `=0` restores the pre-fix take-over root set in one binary.
pub(crate) fn scan_taken_over_peer_shadow<F>(
    os_tid: u32,
    is_obj: &F,
    roots: &mut Vec<ObjectRef>,
) -> usize
where
    F: Fn(usize) -> Option<ObjectRef>,
{
    if !crate::jit::conservative_roots::xt_peer_shadow_scan_enabled() {
        return 0;
    }
    let before = roots.len();
    match read_peer_shadow_window_ex(os_tid, is_obj, roots) {
        PeerWindowRead::Whole(_) => {
            XT_TAKEOVER_SHADOW_WINDOWS.fetch_add(1, Ordering::Relaxed);
        }
        PeerWindowRead::BandlessIndirect(_) => {
            // gc-common w6-g: the peer published no stack band (always so on
            // a target other than Windows/Linux, and on those two when the OS
            // would not report the thread's stack), so its INDIRECT entries
            // were not dereferenced. No root is lost by that: an indirect
            // entry names a slot of one of the peer's own compiled frames,
            // i.e. a word of `[rsp, stack top)`, which this pass reads
            // conservatively. But nothing proves each named slot was inside
            // the band that scan read whole, so this pause must not claim a
            // complete pin set: count it where the `TakeoverVerdict` looks
            // (`pins_complete`), which keeps a pinnable backend from a
            // pinned MOVE. Never a root dropped, never a coverage claim made.
            XT_TAKEOVER_SHADOW_UNPUBLISHED.fetch_add(1, Ordering::Relaxed);
            XT_TAKEOVER_SHADOW_BANDLESS.fetch_add(1, Ordering::Relaxed);
            note_takeover_stack_incomplete();
        }
        PeerWindowRead::Untrusted => {
            XT_TAKEOVER_SHADOW_UNPUBLISHED.fetch_add(1, Ordering::Relaxed);
        }
    }
    let n = roots.len() - before;
    XT_TAKEOVER_SHADOW_ROOTS.fetch_add(n as u64, Ordering::Relaxed);
    n
}

/// The native claim holds the shadow owner outside Java until release.
/// Only a whole identity-checked window can license a pinned deposit.
/// Untrusted and bandless indirect windows retain the frozen-peer fallback.
pub(crate) fn scan_native_claim_shadow<F>(
    os_tid: u32,
    is_obj: &F,
    roots: &mut Vec<ObjectRef>,
) -> Option<usize>
where
    F: Fn(usize) -> Option<ObjectRef>,
{
    if !crate::jit::conservative_roots::xt_peer_shadow_scan_enabled() {
        return None;
    }
    let before = roots.len();
    match read_peer_shadow_window_ex(os_tid, is_obj, roots) {
        PeerWindowRead::Whole(_) => Some(roots.len() - before),
        _ => None,
    }
}

/// Pauses that ran the take-over (`stw_take_over_and_wait` past its
/// `enabled` gate), and how many of them froze at least one peer in compiled
/// code (gc-common w6-g). Their ratio is the first measurement
/// `common-c-proposal-roll-forward-to-a-poll-REJECTED-20260928` asks for: the share of pauses
/// whose licence is degraded by a frozen peer, i.e. what rolling a peer
/// forward to its next poll could buy back. `XT_THREADS_TAKEN_OVER` counts
/// peers, not pauses, so it cannot answer that.
pub static XT_TAKEOVER_PAUSES: AtomicU64 = AtomicU64::new(0);
/// See [`XT_TAKEOVER_PAUSES`].
pub static XT_TAKEOVER_PAUSES_WITH_FROZEN: AtomicU64 = AtomicU64::new(0);

/// `cratonvm_gc::shadow_stack::rejected_indirect_entries()`, re-read here so a
/// crate without a `cratonvm-gc` dependency (the launcher's exit lines) can
/// print it. Lifetime count of indirect shadow entries refused by the
/// stack-band check; non-zero means a compiled frame published a
/// non-reference in a reference home (gc-common w6-g).
pub fn shadow_rejected_indirect_entries() -> u64 {
    cratonvm_gc::shadow_stack::rejected_indirect_entries()
}

/// Take-over windows whose peer had no published stack band while holding
/// INDIRECT entries (gc-common w6-g). Each is also counted in
/// `XT_TAKEOVER_SHADOW_UNPUBLISHED` and `XT_TAKEOVER_STACK_INCOMPLETE`; see
/// [`scan_taken_over_peer_shadow`]. Printed on the `[xt-jit-roots] pass:`
/// debug line.
pub static XT_TAKEOVER_SHADOW_BANDLESS: AtomicU64 = AtomicU64::new(0);

/// What one read of a peer's shadow window found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PeerWindowRead {
    /// Every slot of `[base, top)` was read (the slot count).
    Whole(usize),
    /// The window was read, but the peer published no stack band, so its
    /// INDIRECT entries were not dereferenced (the slot count). Value entries
    /// were added. Untrusted: the caller must not claim coverage from it.
    BandlessIndirect(usize),
    /// Never published, stale (identity mismatch) or torn `top`: nothing read.
    Untrusted,
}

/// The census-free core of [`scan_peer_shadow_window`]: validate the triple
/// `os_tid` published and push every slot of `[base, top)` that `is_obj`
/// accepts. `None` means the window could not be trusted.
fn read_peer_shadow_window<F>(os_tid: u32, is_obj: &F, out: &mut Vec<ObjectRef>) -> Option<usize>
where
    F: Fn(usize) -> Option<ObjectRef>,
{
    match read_peer_shadow_window_ex(os_tid, is_obj, out) {
        PeerWindowRead::Whole(slots) => Some(slots),
        PeerWindowRead::BandlessIndirect(_) | PeerWindowRead::Untrusted => None,
    }
}

/// [`read_peer_shadow_window`], saying WHY a window is untrusted.
fn read_peer_shadow_window_ex<F>(
    os_tid: u32,
    is_obj: &F,
    out: &mut Vec<ObjectRef>,
) -> PeerWindowRead
where
    F: Fn(usize) -> Option<ObjectRef>,
{
    // The peer never published: UNKNOWN coverage, not empty coverage.
    let Some((ss, pub_base, pub_end)) = cratonvm_gc::gc_quiescence::shadow_window_of_tid(os_tid)
    else {
        return PeerWindowRead::Untrusted;
    };
    if ss == 0 || ss & 0x7 != 0 {
        return PeerWindowRead::Untrusted;
    }
    // SAFETY: `ss` is the `#[repr(C)] ShadowStack` address the owning thread
    // published. The thread is alive and cannot run -- in `blocked_os_tids`
    // and blocked, or frozen in compiled code by the take-over pass -- so it
    // is not mutating, its `JvmThread` cannot have moved while it holds live
    // compiled frames (the JIT caches `*mut JvmThread` per frame and reaches
    // the shadow stack through it), and the entry is removed when the thread
    // exits. Fields are `top`, `end`, `base` at 0, 8, 16 -- asserted by
    // `layout_offsets_match_jit_contract`.
    //
    // RESIDUAL, stated plainly: this read happens BEFORE the identity check
    // below can reject a stale address, so a thread that died between the
    // blocked-tid snapshot and here would be read after free. The window is
    // narrow and the same shape the existing `shadow_window_from_frame` lives
    // with; the identity check is what stops a stale read from being ACTED on.
    let (top, end, base) = unsafe {
        let p = ss as *const usize;
        (p.read(), p.add(1).read(), p.add(2).read())
    };
    // IDENTITY CHECK, and the reason reading `ss` is defensible: the struct's
    // own `base`/`end` must be exactly what this thread published. They point
    // into the heap `Box` and never change after `ensure_allocated`, so a
    // struct that moved (or was freed) does not match, and a stale address is
    // rejected instead of dereferenced further.
    if base != pub_base || end != pub_end {
        return PeerWindowRead::Untrusted;
    }
    // The `#[repr(C)]` invariant. `top` is the only field compiled code writes,
    // so it is the only one that still needs checking.
    if top & 0x7 != 0 || top < base || top > end {
        return PeerWindowRead::Untrusted;
    }
    let slots = (top - base) / 8;
    // The PEER's stack band, published beside its window under the same tid
    // (gc-common w5-g). An indirect entry is resolved only inside it: an odd
    // primitive a compiled frame published in a reference home
    // (`ShadowOddLongProbe`) names no word of the peer's stack and is not
    // dereferenced. With no band there is no way to tell a slot from a
    // mistyped value, so indirect entries resolve to null AND the window is
    // reported untrusted (`BandlessIndirect`), which keeps the cycle from
    // claiming coverage it does not have; value entries are still added
    // (roots are additive). No band is ever published on a target other than
    // Windows/Linux (`shadow_stack::os_current_thread_stack` has no arm), so
    // there every peer window with an indirect entry takes this path; the
    // take-over caller then withholds `pins_complete` (gc-common w6-g).
    let band = cratonvm_gc::shadow_stack::thread_stack_band(os_tid);
    let mut unresolved_indirect = false;
    // rx27: through the one window walker, so a frame descriptor record is
    // expanded here too (`shadow_stack::walk_window`).
    // SAFETY: `[base, top)` is inside the validated window, which the blocked
    // or frozen peer is not mutating. A `Frame` home is reported only inside
    // the peer's band: the peer cannot run, and every entry below `top` inside
    // it names a live frame -- the block's entries are stored before `top` is
    // bumped over them, and every exit retracts `top` before tearing its frame
    // down.
    unsafe {
        cratonvm_gc::shadow_stack::walk_window(base, top, band, |home| {
            if home == cratonvm_gc::shadow_stack::EntryHome::Unresolved && band.is_none() {
                unresolved_indirect = true;
            }
            let v = cratonvm_gc::shadow_stack::read_home(home);
            if let Some(o) = is_obj(v) {
                out.push(o);
            }
        });
    }
    if unresolved_indirect {
        return PeerWindowRead::BandlessIndirect(slots);
    }
    PeerWindowRead::Whole(slots)
}

/// `CRATONVM_XT_HELPER_WINDOW_PIN_RESOLVE=0` -- resolve a frozen peer's words
/// with `is_heap_addr` rather than `resolve_interior_for_pin`.
///
/// The difference is the two cases `is_heap_addr` drops and a frozen peer's
/// registers hold: a MISALIGNED interior pointer and a ONE-PAST-THE-END cursor.
/// Both leave an object unpinned, and relocation then moves it out from under
/// the register that names it.
///
/// # DEFAULT ON since 2026-09-08, because the discharge made it load-bearing
///
/// It shipped opt-in on 2026-09-04 and, as
/// `fixed-suite-bugs/bug-testlargeblob-segv-decommit-under-live-memcpy-20260904`
/// records while eliminating it as that page's cause, *"it is
/// `runtime_var_os(..).is_some()`, i.e. opt-in and default OFF, so it was never
/// active in any run"*. That left an asymmetry nobody had to notice while the
/// pin was merely additive: `helper_window_discharge_enabled` is default ON and
/// **discharges the refusal on the strength of the pin**, so from that day the
/// pin stopped being a hint and became the thing standing between relocation
/// and a peer's registers.
///
/// A discharged cycle asks `helper_windows_all_pinned_this_cycle`, and that
/// answers yes for a window whose every candidate came back from a predicate
/// documented to drop exactly the two shapes a compiled loop puts in a register.
/// `is_heap_addr` rejects a misaligned address -- a cursor into a `char[]` or
/// `byte[]` -- and its extent test is `addr < end`, so a cursor one past the
/// last element resolves to no base. Either leaves the array unpinned while the
/// cycle relocates, which is a use-after-free rather than lost compaction.
///
/// So the two flags have to agree, and the direction to agree in is the one
/// `ZgcRealHeap::resolve_interior_for_pin` already argues for itself: the
/// result withholds a page from relocation, so a false positive costs one page
/// of compaction and a false negative costs a use-after-free.
///
/// `CRATONVM_XT_HELPER_WINDOW_PIN_RESOLVE=0` is the kill switch and restores the
/// `is_heap_addr` probe. It is the A/B for pricing the wider pin, not a
/// configuration anyone should run with the discharge on.
pub fn helper_window_pin_resolve_enabled() -> bool {
    static ON: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&ON, || {
        !matches!(
            cratonvm_types::flags::runtime_var("CRATONVM_XT_HELPER_WINDOW_PIN_RESOLVE").as_deref(),
            Ok("0") | Ok("false") | Ok("off") | Ok("no")
        )
    })
}

/// `CRATONVM_XT_TAKEOVER_INTERIOR` -- let the TAKE-OVER pass root a frozen
/// peer's DERIVED pointers, not only exact object bases. **Default ON since
/// gc-common w2-c (2026-09-23); `=0` is the kill switch.** Shipped opt-in by
/// w1-c the same day.
///
/// # The gap
///
/// `take_over_pass` probes every register and stack word of a peer frozen in
/// compiled code with `is_object_address` -- exact bases only. A compiled loop
/// that keeps only a cursor into an array body (or one past its end) has no
/// base anywhere in its frame, so the array is in no root set:
///
/// * Generational and ZGC run the cycle non-moving (`XT_TAKEOVER` is an
///   unpinnable refusal on both), so the array is merely UNMARKED -- and freed
///   by the sweep if nothing else reaches it;
/// * G1 ignores the refusal and evacuates everything it was not told to pin,
///   and `pin_frozen_peer_roots_for_g1` pins only what this probe returned --
///   so the array MOVES under the frozen register.
///
/// The helper-window pass closed the same hole for blocked peers with
/// `resolve_interior_for_pin` (default ON since 2026-09-08); the take-over
/// population never got it. This is that probe for the take-over pass, with
/// two differences: it tries the exact-base answer first (so the common word
/// costs what it did), and it screens by the heap's conservative envelope
/// before the interior lookup, because the generational `is_heap_addr` takes
/// three locks per word and a frozen stack can be megabytes.
///
/// # Why it is on by default (w2-c; the full argument is on the page
/// `common-c-takeover-probe-drops-derived-pointers`)
///
/// * It is a use-after-free fix on all three backends, and the default
///   `compatible` mode takes genuine bug fixes.
/// * The CONSUMERS of an interior word in `xt_roots` are not new: the
///   helper-window pass has fed `resolve_interior_for_pin` answers into the same
///   vector, to the same Generational mark (which resolves an interior root to
///   its containing object, GC.md "Standing invariants"), the same G1 region
///   pin (`pin_frozen_peer_roots_for_g1`) and the same ZGC page withholding, by
///   default since 2026-09-08. The flip adds a population, not a code path.
/// * The cost is confined to take-over cycles (a peer frozen in compiled code),
///   and within them to words inside the heap envelope that MISS the exact-base
///   probe: everything else dies on the first compare, as before.
/// * Locking is safe: the interior lookup takes heap mutexes (three on
///   Generational) while peers are frozen, but a peer is frozen only with `Rip`
///   inside compiled code, which holds no Rust lock -- the same argument that
///   already lets ZGC's `is_object_address` take its registry lock here.
///
/// What stays unmeasured, and is the reason `=0` exists: the size of the
/// widening on a real take-over population, and the extra G1 regions it pins.
pub fn takeover_interior_enabled() -> bool {
    static ON: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&ON, || {
        !matches!(
            cratonvm_types::flags::runtime_var("CRATONVM_XT_TAKEOVER_INTERIOR").as_deref(),
            Ok("0") | Ok("false") | Ok("off") | Ok("no")
        )
    })
}

/// The word probe `stw_take_over_and_wait` hands [`take_over_pass`].
///
/// An exact base first (so the common word costs what it always did). With
/// [`takeover_interior_enabled`] a miss is then resolved as a DERIVED pointer:
/// `w` itself, and for a MISALIGNED `w` (a cursor into a `byte[]`/`char[]`,
/// which the Generational/G1 `is_heap_addr` alignment test rejects outright)
/// the aligned word containing it. The object that ENDS at `w` -- the other
/// thing a derived pointer can mean -- is [`takeover_word_companion`]'s answer.
///
/// `w` is resolved before the word below it on purpose: `w` may be the base of
/// an object whose header the strict exact probe declined (mid-initialisation),
/// and the Generational/G1 interior answer for it is `w` itself -- trying the
/// word below first would root the PREVIOUS object instead and drop that one.
pub(crate) fn takeover_word_probe(heap: &crate::memory::VmHeap, w: usize) -> Option<ObjectRef> {
    if let Some(o) = heap.is_object_address(w) {
        return Some(o);
    }
    if w < 8 || !takeover_interior_enabled() {
        return None;
    }
    if let Some((lo, hi)) = heap.conservative_addr_span() {
        // `<= hi`: a one-past-the-end cursor of the highest object.
        if w < lo || w > hi {
            return None;
        }
    }
    heap.resolve_interior_for_pin(w).or_else(|| {
        if w & 7 != 0 {
            heap.resolve_interior_for_pin(w & !7)
        } else {
            None
        }
    })
}

/// The object a frozen word `w` may ALSO name: the one that ENDS at `w`.
///
/// A one-past-the-end cursor of `O` (the loop finished; the cursor is all that
/// is left of `O` in the frame) lies OUTSIDE `O`, so [`takeover_word_probe`]
/// answers it as whatever is there instead:
///
/// * in the common case EXACTLY the base of the object allocated right after
///   `O` -- bump allocation makes them contiguous -- which the exact probe
///   accepts as that next object;
/// * on Generational/G1, whose interior answer is "aligned and inside a live
///   region", the address itself -- and for an `O` ending exactly on a G1
///   region boundary that is the NEXT region, so the pin lands there and `O`'s
///   region is evacuated under the frozen register.
///
/// Both answers name the word itself, so the rule is: whenever the probe's
/// answer IS the (aligned) word, also root the word below it. A resolved
/// interior answer (ZGC returns the containing object's base) needs no
/// companion. The cost is one extra interior lookup per such word and the
/// object physically before each root retained for one take-over cycle; it is
/// the only way to cover `O` with a probe that returns one answer per word
/// (gc-common w2-c).
///
/// `None` with the interior probe off (the kill switch restores the exact-base
/// root set exactly) and when the word below is not heap.
pub(crate) fn takeover_word_companion(
    heap: &crate::memory::VmHeap,
    w: usize,
    probed: Option<ObjectRef>,
) -> Option<ObjectRef> {
    if !takeover_interior_enabled() {
        return None;
    }
    word_companion_below(heap, w, probed)
}

/// The companion rule shared by both populations: when the probe's answer for
/// `w` IS the aligned word (an exact base, or the Generational/G1 "inside a
/// live region" echo), also resolve the word below it -- the object that ENDS
/// at `w`. A resolved interior answer (ZGC's containing base) needs none.
fn word_companion_below(
    heap: &crate::memory::VmHeap,
    w: usize,
    probed: Option<ObjectRef>,
) -> Option<ObjectRef> {
    let o = probed?;
    let aligned = w & !7;
    if o.as_ptr() as usize != aligned || aligned < 8 {
        return None;
    }
    heap.resolve_interior_for_pin(aligned - 8)
}

// gci i1/fr (2026-10-11) -- `gcf-f7f-...` kind A: a frozen peer's stack words
// ABOVE its innermost compiled segment root object starts only (opt-in
// `CRATONVM_XT_TAKEOVER_ABOVE_CHAIN_STARTS`, Generational only).
//
// The take-over probes EVERY word of a frozen peer's stack, from its `rsp` to
// the stack top, with `takeover_word_probe`, whose derived-pointer arm answers
// any aligned in-region word with the word itself on Generational; the old
// mark then resolves that word to its containing object. gch h3/yc's forced
// take-overs (vh3b-yc block C, `[xt-frame]` census) placed the kind-A holder
// (`cat=xt frozen in-JIT peers ... interior=+0x28 stamped=false`) 0xab8 ABOVE
// the outermost compiled frame of the peer's innermost segment -- in the Rust
// entry frames, not in a compiled frame -- so the frozen-peer proposal's
// per-frame map reading would not have touched it (h3/yc rejected it).
//
// The derived-pointer arm exists for the frames frozen at an ARBITRARY pc:
// the register file and the innermost compiled segment, whose top frame may
// hold a cursor into an array with no base anywhere (`common-c-takeover-
// probe-drops-derived-pointers`). Every frame above that segment is suspended
// at a CALL:
//   * a Rust frame (the JIT entry door, an interpreter frame's Rust, a helper
//     that re-entered compiled code) -- the cooperative scans root the same
//     frames of a parked thread as object STARTS only (`scan_one_frame` over
//     the chain band) or not at all (the above-chain band is opt-in,
//     `CRATONVM_JIT_ABOVE_CHAIN_SCAN`), so a Rust local that names an object
//     only through a derived pointer is already unrooted at every cooperative
//     pause;
//   * a compiled frame of an OUTER segment -- stopped at a call into a helper
//     that ran Java, a GC point, across which compiled code keeps no derived
//     pointer (no derived-pointer table: a moving young cycle there would
//     leave it dangling).
// So above the innermost segment's top this rule gives a frozen peer what the
// cooperative protocol gives the same frames: object starts (the root
// validator) that the gcf f7/f proof does not show to be header-shaped
// interiors (`helper_window_root_is_start`'s test). The register file and the
// innermost segment keep `takeover_word_probe`. A chain the walk cannot follow
// (no plausible `rbp` record at the frozen `rip`) keeps the whole band on the
// full probe -- fail-safe.
//
// MARKING only on Generational: on G1 the take-over's roots are also the
// pin set (`pin_frozen_peer_roots_for_g1`), so G1 keeps `take_over_pass`. A
// Generational take-over cycle is non-moving (the `XT_TAKEOVER` refusal) unless
// the opt-in pinned young copy takes it, which pins through the young pin
// ledger, not through these roots.
//
// zwr (ZGC, default ON, `CRATONVM_XT_TAKEOVER_ABOVE_CHAIN_STARTS_ZGC=0`
// kills): ZGC's interior answer is the CONTAINING object's base, so a stale
// derived pointer in the Rust frames above the segment keeps whatever object
// now covers that address. `Zt1MkSystemGcFresh` measured it: the delivery
// thread, frozen mid-batch in compiled listener code, held words `+0x18`..
// `+0x40` into an address a new round's `int[16]` reused, 0x9000-0xc000 above
// the segment top, and that weak referent survived `System.gc()` (4/400 runs;
// HotSpot 0). Above the segment ZGC takes the exact live-base registry answer
// (`is_object_address`), which a header-shaped interior cannot pass, and no
// companion -- what the cooperative band scans give the same frames.
//
// And on ZGC the split is at the top of the innermost FRAME, not of the
// innermost segment (`TakenOver::set_above_split_frames(1)`): every other
// compiled frame of the segment is suspended at a call too, which is the
// argument above. Measured: the delivery thread frozen in
// `AbstractSet.equals` held, in its depth-2 frame (`CompositeDataSupport.<init>`,
// at a call), three words naming a live `WeakReference`'s exact base; the
// companion rule then rooted the object ENDING there -- the probe's `int[16]`
// allocated just before that reference -- and it survived `System.gc()`. The
// frozen peer's shadow stack holds exact oops by construction (compiled code
// parks references there across a call), never a cursor, so a split pass reads
// it with the exact-start probe too.
//
// The caller chooses the split probe (`take_over_pass_split` with
// [`takeover_above_chain_word_probe`]) under the switch; without it the pass
// is byte-for-byte `take_over_pass`.

/// `CRATONVM_XT_TAKEOVER_ABOVE_CHAIN_STARTS=1` -- opt-in (default OFF): see
/// the section above. Read once per take-over round by the caller; no static.
pub fn takeover_above_chain_starts_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_XT_TAKEOVER_ABOVE_CHAIN_STARTS")
}

/// zwr: the same rule on ZGC, `CRATONVM_XT_TAKEOVER_ABOVE_CHAIN_STARTS_ZGC`
/// (default ON, `=0` kills; see the section above). Read once per take-over
/// round by the caller; no static.
pub fn takeover_above_chain_starts_zgc_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_XT_TAKEOVER_ABOVE_CHAIN_STARTS_ZGC")
}

/// Does a take-over round on `heap` split its probe at the innermost segment
/// top? Generational under the opt-in, ZGC under its default-on switch, never
/// G1 (its roots are its pin set).
pub(crate) fn takeover_above_chain_splits(heap: &crate::memory::VmHeap) -> bool {
    if heap.is_generational() {
        takeover_above_chain_starts_enabled()
    } else if heap.is_g1() {
        false
    } else {
        takeover_above_chain_starts_zgc_enabled()
    }
}

/// The probe for a frozen peer's stack words above its innermost compiled
/// segment (see the section above): on Generational an object start the root
/// validator accepts and the gcf f7/f interior proof does not refute, and no
/// companion; on ZGC an exact live base (`is_object_address`); on G1 exactly
/// [`takeover_word_probe`].
pub(crate) fn takeover_above_chain_word_probe(
    heap: &crate::memory::VmHeap,
    w: usize,
) -> Option<ObjectRef> {
    if heap.is_g1() {
        return takeover_word_probe(heap, w);
    }
    if !heap.is_generational() {
        return heap.is_object_address(w);
    }
    let o = heap.is_object_address_for_root_scan(w)?;
    if crate::jit::conservative_roots::frame_word_is_proven_interior_validated(heap, w) {
        return None;
    }
    Some(o)
}

/// Frames [`takeover_innermost_segment_top`] follows at most. A deeper
/// segment ends the walk early, which only moves more words to the starts
/// rule inside compiled frames that are suspended at calls.
pub(crate) const TAKEOVER_SEGMENT_MAX_FRAMES: usize = 4096;

/// The top (`rbp + 16` of its outermost frame record) of a frozen peer's
/// innermost compiled segment: the walk [`frozen_frame_chain`] makes, without
/// collecting the frames (it runs with peers frozen, so it allocates nothing).
/// `None` when the frozen `rip`/`rbp` give no plausible first frame: the
/// caller then keeps the whole band on the full probe. Pure.
#[cfg_attr(
    not(any(test, windows, all(target_os = "linux", target_arch = "x86_64"))),
    allow(dead_code)
)]
pub(crate) fn takeover_innermost_segment_top(
    rip: usize,
    rbp: usize,
    band: (usize, usize),
    max_frames: usize,
    frame_size_at: impl Fn(usize) -> Option<usize>,
    read_record: impl Fn(usize) -> Option<(usize, usize)>,
) -> Option<usize> {
    let (lo, hi) = band;
    let (mut pc, mut cur) = (rip, rbp);
    let mut top = None;
    for _ in 0..max_frames {
        if frame_size_at(pc).is_none() {
            break;
        }
        if cur < lo || cur & 7 != 0 || cur.checked_add(16).is_none_or(|e| e > hi) {
            break;
        }
        top = Some(cur + 16);
        let Some((parent, ret)) = read_record(cur) else {
            break;
        };
        if parent <= cur {
            break;
        }
        pc = ret;
        cur = parent;
    }
    top
}

/// The band size of the compiled body containing `pc` in `view` (`None`: not
/// compiled code), as the `[xt-frame]` census reads it.
#[cfg_attr(
    not(any(windows, all(target_os = "linux", target_arch = "x86_64"))),
    allow(dead_code)
)]
fn compiled_frame_size_in(view: &cratonvm_jit::JitCodeRangeView, pc: usize) -> Option<usize> {
    let p = view.lookup(pc)?;
    // SAFETY: a registered code range keeps its CompiledMethod alive while a
    // frame of it is live, and the peer is frozen inside these frames for the
    // whole pass (the contract `census_frozen_frames` relies on too).
    let cm = unsafe { &*(p as *const cratonvm_jit::CompiledMethod) };
    usize::try_from(cm.osr_frame_size).ok()
}

// gcg g1/f (2026-10-01) -- the census lines that name a take-over or
// helper-window word.
//
// Census (`CRATONVM_DBG=oldmark-root-census`, both arms): each frozen peer
// prints one `[xt-band]` line and each word it roots one `[xt-word]` line
// (capped per peer), so a census holder `root[i]=0x<a> cat=xt frozen ...`
// is matched by its address to the peer, the stack slot and the band
// (`reg` / `stack`); helper-window words print `pop=helper`.
//
// The g1/f opt-in that scoped the take-over's derived-pointer arm to a frozen
// peer's innermost frames (`xt-takeover-interior-innermost`) was DELETED in
// gcg g5/xt on lane jm's g5 verdict: with it on, both whole census runs still
// named the frozen-peer interior word as `holder#1`, and the JitOomRoot
// failure rate was 0/8 in both arms. Every stack word of a frozen peer gets
// `takeover_word_probe` again, as before g1/f.

/// Per frozen peer, how many `[xt-word]` census lines are printed.
pub const XT_CENSUS_WORDS_PER_PEER: usize = 256;

/// `CRATONVM_DBG=oldmark-root-census`: the xt census lines are printed.
pub fn xt_census_on() -> bool {
    crate::jit::helpers::oldmark_census_on()
}

/// One `[xt-word]` census line (see the section above). `band` is `reg`,
/// `stack` or `hw`; `at` is the stack slot (0 for a register).
pub fn xt_census_word(pop: &str, tid: u32, band: &str, at: usize, w: usize, root: usize) {
    eprintln!("[xt-word] pop={pop} tid={tid} band={band} at=0x{at:x} word=0x{w:x} root=0x{root:x}");
}

/// gch h3/yc (R3 of `h2-fj-report.md`, the g5jm attribution): one compiled
/// frame of a peer frozen by the take-over, found by [`frozen_frame_chain`].
/// Census only: the `[xt-frame]` line lets a holder's `[xt-word] at=` slot be
/// attributed to the frame whose `[lo, rbp + 16)` contains it, i.e. to the
/// innermost frame (stopped at an arbitrary pc, no map applies) or to an outer
/// one (stopped at a call site, whose map would apply).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(
    not(any(test, all(target_os = "linux", target_arch = "x86_64"))),
    allow(dead_code)
)]
pub(crate) struct FrozenFrame {
    /// 0: the frame record the frozen `RBP` names.
    pub(crate) depth: usize,
    /// The frame record: `[rbp]` is the caller's RBP, `[rbp + 8]` the
    /// return address into the caller.
    pub(crate) rbp: usize,
    /// Where the frame is stopped: the frozen `RIP` at depth 0 (which, frozen
    /// in a prologue or an epilogue, belongs to the CALLEE of the record's
    /// method -- a census caveat), the return address above.
    pub(crate) pc: usize,
    /// `rbp - frame_size`: the low end of the frame's band.
    pub(crate) lo: usize,
}

/// The compiled frames of a frozen peer, innermost first, by its RBP chain
/// (gch h3/yc, census only). `frame_size_at(pc)` answers the frame size of the
/// compiled body containing `pc` (`None`: not compiled code -- the walk
/// stops), `read_record(rbp)` the `(caller RBP, return address)` pair stored
/// at `rbp` (`None`: unreadable -- the walk stops). The walk also stops at a
/// record outside `band` (`[lo, hi)`, the scanned stack), misaligned, or not
/// climbing, and after `max_frames` frames. Pure.
#[cfg_attr(
    not(any(test, all(target_os = "linux", target_arch = "x86_64"))),
    allow(dead_code)
)]
pub(crate) fn frozen_frame_chain(
    rip: usize,
    rbp: usize,
    band: (usize, usize),
    max_frames: usize,
    frame_size_at: impl Fn(usize) -> Option<usize>,
    read_record: impl Fn(usize) -> Option<(usize, usize)>,
) -> Vec<FrozenFrame> {
    let (lo, hi) = band;
    let mut out = Vec::new();
    let (mut pc, mut cur) = (rip, rbp);
    while out.len() < max_frames {
        let Some(size) = frame_size_at(pc) else {
            break;
        };
        if cur < lo || cur & 7 != 0 || cur.checked_add(16).is_none_or(|e| e > hi) {
            break;
        }
        out.push(FrozenFrame {
            depth: out.len(),
            rbp: cur,
            pc,
            lo: cur.saturating_sub(size),
        });
        let Some((parent, ret)) = read_record(cur) else {
            break;
        };
        if parent <= cur {
            break;
        }
        pc = ret;
        cur = parent;
    }
    out
}

/// Companions the helper-window pass rooted and pinned (gc-common w3-g): the
/// objects ending at an accepted word. Cumulative; the measurement the page
/// `common-w2c-helper-window-probe-misses-the-object-ending-at-a-cursor` asked
/// for is this against `XT_HELPER_WINDOW_ROOTS`.
pub static XT_HELPER_WINDOW_COMPANIONS: AtomicU64 = AtomicU64::new(0);

/// The word probe `stw_take_over_and_wait` hands [`helper_window_pass`] while
/// [`helper_window_pin_resolve_enabled`] (default ON): `resolve_interior_for_pin`,
/// and for a MISALIGNED word it rejects -- a cursor into a `byte[]`/`char[]`,
/// which the Generational/G1 `is_heap_addr` alignment test refuses outright --
/// the aligned word containing it, exactly as [`takeover_word_probe`] does for
/// frozen peers (gc-common w3-g).
///
/// No exact-base-first step and no envelope screen, unlike the take-over probe:
/// the helper-window pass classifies a COPY of the band with the peer running,
/// so its per-word cost is not paid with peers frozen, and
/// `resolve_interior_for_pin` already answers an exact base with itself.
pub(crate) fn helper_window_word_probe(
    heap: &crate::memory::VmHeap,
    w: usize,
) -> Option<ObjectRef> {
    heap.resolve_interior_for_pin(w).or_else(|| {
        if w & 7 != 0 {
            heap.resolve_interior_for_pin(w & !7)
        } else {
            None
        }
    })
}

/// [`takeover_word_companion`]'s rule for helper windows (gc-common w3-g,
/// `common-w2c-helper-window-probe-misses-the-object-ending-at-a-cursor`).
///
/// A DISCHARGED helper-window cycle relocates on the strength of the window's
/// pin set, so a one-past-the-end cursor that the probe answered as the NEXT
/// object (or, on Generational/G1, as the word itself -- the next G1 region for
/// an object ending on a region boundary) left the array it walked unpinned
/// and, on Generational/ZGC, unmarked. Gated on the same switch as the probe
/// it completes: `CRATONVM_XT_HELPER_WINDOW_PIN_RESOLVE=0` restores the
/// pre-2026-09-08 probe and with it the exact root set, companions included.
///
/// Applied by `stw_take_over_and_wait` AFTER the pass, to the roots the pass
/// COMMITTED whose answer echoed their word (a band without a JIT frame commits
/// nothing, so pure-native stacks get no companions). Cost: one extra lookup
/// per such root (not per band word), and the object physically before it
/// retained -- on G1/ZGC usually in the SAME region/page, so rarely an extra
/// pin -- for one cycle.
pub(crate) fn helper_window_word_companion(
    heap: &crate::memory::VmHeap,
    w: usize,
    probed: Option<ObjectRef>,
) -> Option<ObjectRef> {
    if !helper_window_pin_resolve_enabled() {
        return None;
    }
    word_companion_below(heap, w, probed)
}

// gcg g3/h (2026-10-01) -- a blocked peer's helper window ROOTS object starts
// only; its PINS are unchanged (`CRATONVM_XT_HELPER_WINDOW_ROOT_STARTS`,
// default ON since the user flip gci-user-flip-20261013, `=0` kills;
// Generational only). Proposal
// `gcg-g1f-proposal-helper-window-roots-only-object-starts-DONE-20261015`.
//
// The pass probes every word of a blocked peer's band with
// `helper_window_word_probe` (`resolve_interior_for_pin`), which on
// Generational answers ANY aligned word inside a live region with the word
// itself; the old mark then resolves it to its containing object. So every
// in-region word anywhere on a blocked JDK thread's stack -- a stale slot of a
// returned Rust frame, an `Node + 0x28` interior -- roots a whole object graph
// (the g1 census: 13-269 helper bands per JitOomRoot run against 0-5 take-over
// bands). The cooperative scans root a parked peer's same frames for object
// STARTS only (`scan_one_frame`, `scan_one_frame_filtered`).
//
// Why starts are enough for MARKING a blocked peer:
// * Every compiled frame of a blocked peer is suspended at a CALL into a Rust
//   helper that entered a blocked region. That helper is a GC point (it may
//   block across a pause), and compiled code keeps no derived pointer across a
//   GC point -- there is no derived-pointer table, so a moving young cycle there
//   would leave it dangling. A parked peer's identical frames are read
//   starts-only today; a blocked peer's frames are the same frames.
// * The Rust frames of the blocked region may hold a derived pointer (an array
//   body a blocking read writes into). Its BASE is a root elsewhere: the native
//   door's argument pins (`CompileArgPins`, `push_off_frame_thread_roots`), a
//   JNI handle or critical record, or the Java caller's frame. That is the
//   audit the proposal names; it was completed by gci i2/fr (gcg-g3h item 6,
//   refuted by reading), which is what licensed the default-ON flip.
// * PINNING is a different obligation (a relocating cycle must not move what a
//   derived pointer addresses) and is untouched: the pass still pins every
//   candidate (`add_xt_cycle_pinned_jit_roots`) and the caller still pins the
//   companions. Only the MARKING roots narrow.
//
// Why Generational only: on G1 the marking roots are ALSO the pin set
// (`pin_frozen_peer_roots_for_g1` pins every `xt_roots` entry, which is the
// only pin a refusing, unpinned window gets), and ZGC's probe already answers
// a containing base (a start), so neither gains and G1 would lose pins. On
// Generational a helper-window cycle is non-moving unless the opt-in pinned
// young copy takes it, and that copy pins the raw band words through the young
// pin ledger, not through `xt_roots`.
//
// A root is kept when the root validator accepts its address as an object
// start (`is_object_address_for_root_scan`) and the gcf f7/f proof does not
// show it to be an interior (`frame_word_is_proven_interior_validated`: an unstamped
// header-shaped word covered by a stamped object below it -- kind A's
// `Node + 0x28` word in the old generation, which has no start bitmap).
//
// Census (`CRATONVM_DBG=oldmark-root-census`), on both arms: one
// `[xt-hw-starts] applied=<bool> kept=<n> dropped=<n>` line per pass, and one
// `[xt-hw-drop] root=0x..` line per dropped (or, switch off, droppable) root,
// capped at `XT_CENSUS_WORDS_PER_PEER`; so the OFF arm says what the ON arm
// would drop, and a census holder `root[i]=0x<a>` on the OFF arm that matches
// an `[xt-hw-drop] root=0x<a>` line is a holder the switch removes.

/// `CRATONVM_XT_HELPER_WINDOW_ROOT_STARTS` -- default ON since JIT r20 GC
/// wave 9 (lane gcD, host build w9c) and by the user flip (`=0` kills): see the
/// section above. Read once per helper-window pass by its caller; no static.
pub fn helper_window_root_starts_enabled() -> bool {
    // WARNING gci-user-flip-20261013: default flipped ON by the user's decision, pending apps triage.
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_XT_HELPER_WINDOW_ROOT_STARTS")
}

/// Keep in `roots[from..]` only the entries `is_start` accepts, in order;
/// `dropped` sees each address removed. Entries before `from` are untouched.
/// Returns how many were removed. Pure, for the tests.
pub fn retain_root_starts_from(
    roots: &mut Vec<ObjectRef>,
    from: usize,
    is_start: impl Fn(usize) -> bool,
    mut dropped: impl FnMut(usize),
) -> usize {
    if from >= roots.len() {
        return 0;
    }
    let before = roots.len();
    let mut tail = roots.split_off(from);
    tail.retain(|o| {
        let a = o.as_ptr() as usize;
        let keep = is_start(a);
        if !keep {
            dropped(a);
        }
        keep
    });
    roots.append(&mut tail);
    before - roots.len()
}

/// Is the helper-window root `a` an object START the cooperative scans would
/// root (see the section above)?
pub fn helper_window_root_is_start(heap: &crate::memory::VmHeap, a: usize) -> bool {
    heap.is_object_address_for_root_scan(a).is_some()
        && !crate::jit::conservative_roots::frame_word_is_proven_interior_validated(heap, a)
}

/// The caller's entry point, run on `xt_roots[from..]` (the helper-window
/// pass's committed roots and their companions) AFTER the pins are taken.
/// `apply`: [`helper_window_root_starts_enabled`]; with it false nothing is
/// removed and only the census counts (printed under
/// `CRATONVM_DBG=oldmark-root-census`). Not Generational: nothing, `0`.
/// Returns the number removed (or, `apply` false, removable).
pub fn helper_window_keep_root_starts(
    heap: &crate::memory::VmHeap,
    roots: &mut Vec<ObjectRef>,
    from: usize,
    apply: bool,
) -> usize {
    if !heap.is_generational() || from >= roots.len() {
        return 0;
    }
    let census = xt_census_on();
    if !apply && !census {
        return 0;
    }
    let mut shown = 0usize;
    let mut note = |a: usize| {
        if census && shown < XT_CENSUS_WORDS_PER_PEER {
            eprintln!("[xt-hw-drop] root=0x{a:x} applied={apply}");
            shown += 1;
        }
    };
    let total = roots.len() - from;
    let n = if apply {
        retain_root_starts_from(
            roots,
            from,
            |a| helper_window_root_is_start(heap, a),
            &mut note,
        )
    } else {
        let mut n = 0usize;
        for o in &roots[from..] {
            let a = o.as_ptr() as usize;
            if !helper_window_root_is_start(heap, a) {
                note(a);
                n += 1;
            }
        }
        n
    };
    if census {
        eprintln!(
            "[xt-hw-starts] applied={apply} kept={} dropped={n}",
            total - n
        );
    }
    n
}

// ---------------------------------------------------------------------------
// gcg g4/xt -- a blocked peer's stack-slot remap captures, recorded per band
// ---------------------------------------------------------------------------
//
// `gcg-g3h-helper-window-protocol-review-findings-DONE-20261012.md` item 3 asked
// whether a PURE-NATIVE band's captures (a blocked peer with no JIT return
// address, whose candidates the pass discards) could be dropped. g4/xt built
// that as an opt-in switch (`xt-helper-capture-windows-only`), and vg4 refuted it: with
// the switch on, `PEER_STACK_SLOTS_WRITTEN` fell from 259 / 163 (G1,
// `GcfHelperWindowControlProbe` / `MtChurnProbe`) and 12 / 2 (Generational)
// to 0 on every row. Every word the blocked-wake repair stored came from a
// pure-native band: those captures are load-bearing (VM Rust frames holding a
// raw `ObjectRef` across a block), and they are also the pin source of a
// blocked peer's non-start words on pinned / option-B young cycles
// (`gen_heap`'s `peer_stack_slot_values_into`). The switch was deleted in g5.
//
// What stays: both helper arms collect a band's captures in a local vector and
// record them in one place once the band is classified, so the census can say
// which population each band is. Census (`CRATONVM_DBG=oldmark-root-census`):
// one `[xt-hw-cap] tid= window=<has_jit> captures=<n>` line per classified
// band with captures.

/// Record one classified band's stack-slot captures `(slot, word)` for the
/// blocked-wake remap -- every one, window or not (see the section above) --
/// and print its census line. Returns how many were recorded.
#[cfg_attr(
    not(any(windows, all(target_os = "linux", target_arch = "x86_64"))),
    allow(dead_code)
)]
fn record_helper_stack_captures(tid: u32, has_jit: bool, pending: &[(usize, usize)]) -> usize {
    for &(at, w) in pending {
        cratonvm_gc::gc_quiescence::record_peer_stack_slot(tid, at, w);
    }
    if !pending.is_empty() && xt_census_on() {
        eprintln!(
            "[xt-hw-cap] tid={tid} window={has_jit} captures={}",
            pending.len()
        );
    }
    pending.len()
}

/// `CRATONVM_XT_HELPER_WINDOW_DISCHARGE` -- let a fully-pinned helper window
/// stop refusing the collection. **Default ON since 2026-09-04**; `=0` is the
/// kill switch.
///
/// (gc-common w1-c, 2026-09-23: this doc block used to sit on
/// `XT_PEER_SHADOW_WINDOWS` below, where it still said "Default OFF" and the
/// census statics carried the discharge's history as their rustdoc.)
///
/// It shipped off because it is a behaviour change on the relocation gate and
/// the first attempt at it (722de9a33) was wrong in two ways at once: it
/// discharged only the LABELLED refusal, leaving the unlabelled one in
/// `interpreter::gc_and_alloc` to refuse anyway, and it pinned a root set that
/// could not be complete because the probe was `is_object_address` (exact bases
/// only), so a peer's derived pointer left its base unpinned.
///
/// Both are addressed: this flag implies the interior-resolving probe, and it
/// gates BOTH sites off the same per-cycle condition. The widening that implies
/// was measured at **+25 % conservative roots per window** on `TestMultiThread`
/// (111 -> 139), which is what makes it affordable.
///
/// Measure it with `CRATONVM_GC_STATS=1` and read `relocation_on_proven_jit`:
/// a zero still voids the run.
pub fn helper_window_discharge_enabled() -> bool {
    // DEFAULT ON since 2026-09-04. A helper window whose peer is completely
    // pinned -- register file, whole `[rsp, stack_base)` band, and the peer's
    // shadow stack -- no longer refuses the collection.
    //
    // Measured on `org.h2.test.jdbc.TestCachedQueryResults` with the arena
    // commit fix in: 5 runs, 0 SIGSEGV, ZERO ref-array OOM, 99953-99978,
    // completing in 555-728 s, compaction intact at 25 cycles / 545893 objects.
    // Against 98304 with 1497 OOMs in ~1519 s before. Regression suite 88/88.
    //
    // `CRATONVM_XT_HELPER_WINDOW_DISCHARGE=0` is the kill switch: it restores
    // the blanket refusal, which costs ~6264 OOMs on that class and does not
    // complete.
    !matches!(
        cratonvm_types::flags::runtime_var("CRATONVM_XT_HELPER_WINDOW_DISCHARGE").as_deref(),
        Ok("0") | Ok("false") | Ok("off") | Ok("no")
    )
}

/// Peers the STW cross-thread scan could NOT classify: it signalled them and
/// they did not reach the handler before the deadline (`STATE_CANCELLED`), or
/// no slot was free to arm. Such a peer is neither parked nor proven
/// interpreter-side, so its JIT-frame oops are absent from the root set while
/// it KEEPS RUNNING — and the non-moving sweep then frees on `GC_FLAG_MARKED`
/// alone. This is the counter that says whether that happened.
pub static XT_PEERS_UNCLASSIFIED: AtomicU64 = AtomicU64::new(0);
/// Collections during which at least one peer went unclassified.
pub static XT_CYCLES_WITH_UNCLASSIFIED: AtomicU64 = AtomicU64::new(0);

/// H2-CID0 (2026-08-05) — takeover signals re-sent because a peer had not
/// reached the handler within one per-attempt deadline.
///
/// Non-zero here is the normal, healthy state on a loaded box; it is the
/// counter that says the retry is doing work rather than being decorative.
pub static XT_PEER_RESIGNALS: AtomicU64 = AtomicU64::new(0);
/// H2-CID0 (2026-08-05) — peers that answered only AFTER a re-signal.
///
/// Every one of these would have been an UNCLASSIFIED peer before the retry
/// landed: a running thread whose JIT-frame oops were absent from the mark set
/// while the non-moving sweep freed on `GC_FLAG_MARKED` alone.
pub static XT_PEERS_CLASSIFIED_AFTER_RETRY: AtomicU64 = AtomicU64::new(0);

/// Deadline for a signalled peer to reach the takeover handler.
///
/// `CRATONVM_XT_PEER_DEADLINE_MS` (default 20). The default is a scheduling
/// bet: a runnable peer under heavy CPU contention can simply not be
/// scheduled within it.
pub fn peer_deadline_ms() -> u64 {
    use std::sync::OnceLock;
    static G: OnceLock<u64> = OnceLock::new();
    *G.get_or_init(|| {
        cratonvm_types::flags::runtime_var("CRATONVM_XT_PEER_DEADLINE_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(20)
    })
}

/// H2-CID0 (2026-08-05) — TOTAL time a peer gets to answer, across re-signals.
///
/// [`peer_deadline_ms`] is one scheduling bet. Losing it means the peer was not
/// placed on a CPU in that window, which on a loaded host says nothing about
/// whether it would ever answer — and the cost of concluding "unclassified" is
/// that the non-moving sweep marks from a root set provably missing a RUNNING
/// thread's JIT frames, then frees on `GC_FLAG_MARKED` alone. Measured 16 such
/// peers across 15 cycles on the H2 `ClassId(0)` reproducer.
///
/// The takeover is signal-based, so a peer need not reach a safepoint to
/// answer; it only needs to be scheduled. Retrying therefore converges for any
/// peer that is genuinely running, which is exactly the population that matters
/// here. `CRATONVM_XT_PEER_TOTAL_MS` (default 1000); set it equal to
/// `CRATONVM_XT_PEER_DEADLINE_MS` to restore the old one-shot behaviour.
pub fn peer_total_deadline_ms() -> u64 {
    use std::sync::OnceLock;
    static G: OnceLock<u64> = OnceLock::new();
    *G.get_or_init(|| {
        let total = cratonvm_types::flags::runtime_var("CRATONVM_XT_PEER_TOTAL_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(1000);
        // A total below one attempt would silently shorten the per-attempt
        // deadline instead of extending it.
        total.max(peer_deadline_ms())
    })
}

/// Whether the cross-thread STW JIT root scan is enabled.
///
/// Default ON (opt-OUT): set `CRATONVM_XT_JIT_ROOT_SCAN=0` (or `false`/`off`)
/// to disable. Flipped to default-on after the cross-thread STW JIT root scan
/// (BUG-03) was validated to suppress the multi-thread-in-JIT-under-STW root
/// gap — e.g. it lets the Tomcat `TestHttpServletDoHead*` (HTTP/2) classes
/// complete instead of dying with the `cross_thread_jit_gap` warning.
#[inline]
pub fn enabled() -> bool {
    static CACHE: AtomicU64 = AtomicU64::new(u64::MAX);
    let c = CACHE.load(Ordering::Relaxed);
    if c != u64::MAX {
        return c == 1;
    }
    // On unless explicitly disabled.
    let on = cratonvm_types::flags::runtime_flag_default_on("CRATONVM_XT_JIT_ROOT_SCAN");
    CACHE.store(on as u64, Ordering::Relaxed);
    on
}

/// A4 (fork6-fjp) — whether the post-barrier helper-window scan is enabled.
///
/// The takeover pass above only freezes+scans peers whose `Rip` is INSIDE a
/// registered JIT code range. A peer that is *blocked* (or parked) with its
/// `Rip` in Rust/native code can still have live JIT frames on its native
/// stack — e.g. an FJP worker blocked in `join()`/park under JIT-compiled
/// `ForkJoinPool.runWorker`/`ForkJoinTask.doExec` frames that are the sole
/// holders of a forked subtask. Such a thread is excluded from the barrier
/// (`threads_blocked`), and its `deposit_root_snapshot` covered only
/// interpreter frames + native pins — never the JIT band — so the non-moving
/// sweep reclaimed the JIT-band oops (the Fork6 stale all-zero receivers).
/// (Historical: the deposit scans the compiled chain since `24cf0b19b0`; see
/// `helper_window_pass` for what the pass still adds, gci i24/jp.)
/// The helper-window pass closes that gap by conservatively scanning the
/// register file + used stack of every remaining peer whose stack contains a
/// JIT return address, once per collection, after the barrier is satisfied.
///
/// Default ON (opt-out): set `CRATONVM_XT_HELPER_WINDOW_SCAN=0` to disable
/// (the A/B kill switch for validating the fix on one binary).
#[inline]
pub fn helper_window_scan_enabled() -> bool {
    static CACHE: AtomicU64 = AtomicU64::new(u64::MAX);
    let c = CACHE.load(Ordering::Relaxed);
    if c != u64::MAX {
        return c == 1;
    }
    let on = cratonvm_types::flags::runtime_flag_default_on("CRATONVM_XT_HELPER_WINDOW_SCAN");
    CACHE.store(on as u64, Ordering::Relaxed);
    on
}

/// `CRATONVM_XT_HELPER_WINDOW_PIN=0` -- stop publishing a frozen helper-window
/// peer's conservative roots as pins.
///
/// Default ON. On its own it does NOT discharge
/// `incomplete_reason::XT_HELPER_WINDOW`: the discharge is
/// [`helper_window_discharge_enabled`] (default ON since 2026-09-04), which
/// consults the per-cycle "every window pinned" verdict this switch feeds. With
/// the pin off every window counts as unpinned, so the discharge can never fire.
///
/// (gc-common w1-c, 2026-09-23: this used to say the pin "does NOT discharge"
/// and that the probe was `is_object_address`; both stopped being true when
/// the discharge and `helper_window_pin_resolve_enabled` went default-on.)
///
/// The scan is complete in what it READS -- the published register file and
/// every readable word from `rsp` up -- and a window whose band could not be
/// read whole (a failed snapshot, a band truncated at `MAX_STACK_SCAN`, an
/// unanswered signal) is counted UNPINNED so it keeps refusing.
///
/// Read once (round 11 wave 5): it is asked per frozen peer, per GC.
fn helper_window_pin_enabled() -> bool {
    static ON: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&ON, || {
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_XT_HELPER_WINDOW_PIN")
    })
}

/// Whether `w` lies inside one of `ranges`, which must be SORTED by start and
/// DISJOINT — `jit_code_ranges_snapshot()` returns them that way (it walks the
/// registry's live ranges in order).
///
/// Round 11 (lane `rt`): every per-word "is this a JIT return address?" test in
/// this file was `ranges.iter().any(..)`, i.e. O(ranges) per stack word, so a
/// blocked peer with no JIT frame — which never sets `has_jit` and so never
/// short-circuits — cost O(band words x compiled bodies) per scan, on every
/// peer, inside the stop-the-world pause. An envelope reject (almost every
/// word) and a binary search give the identical answer for sorted disjoint
/// ranges; `native_stack_has_jit_frame` has done exactly this since 2026-07.
#[inline]
#[cfg_attr(
    not(any(test, windows, all(target_os = "linux", target_arch = "x86_64"))),
    allow(dead_code)
)]
pub(crate) fn in_code_ranges(ranges: &[(usize, usize)], w: usize) -> bool {
    let (Some(&(env_lo, _)), Some(&(_, env_hi))) = (ranges.first(), ranges.last()) else {
        return false;
    };
    if w < env_lo || w >= env_hi {
        return false;
    }
    let idx = ranges.partition_point(|&(lo, _)| lo <= w);
    idx > 0
        && ranges
            .get(idx - 1)
            .is_some_and(|&(lo, hi)| w >= lo && w < hi)
}

/// A4 (fork6-fjp) — classify one peer's already-copied stack/register words.
///
/// Returns `true` if any word is a return address into a registered JIT code
/// range (i.e. the peer has JIT frames on its native stack — the
/// helper/blocked window), and pushes every word that resolves to a live
/// object onto `candidates`. The caller commits `candidates` as roots only
/// when the band is classified as a helper window, so a pure-native thread
/// (no JIT frames anywhere on its stack) contributes nothing.
///
/// Both OS arms now inline this loop so they can record each accepted word's
/// stack address in the same pass (the Windows arm used to probe every word
/// twice to do that); it stays as the tested statement of the classification
/// rule.
#[cfg(test)]
pub(crate) fn classify_helper_window_words<F>(
    words: impl Iterator<Item = usize>,
    ranges: &[(usize, usize)],
    is_obj: &F,
    candidates: &mut Vec<ObjectRef>,
) -> bool
where
    F: Fn(usize) -> Option<ObjectRef>,
{
    let mut has_jit = false;
    for w in words {
        if !has_jit && crate::jit::xt_root_scan::in_code_ranges(&ranges, w) {
            has_jit = true;
        }
        if let Some(o) = is_obj(w) {
            candidates.push(o);
        }
    }
    has_jit
}

/// `CRATONVM_DBG_XT_JIT_ROOT_SCAN`, read once: asked per peer and per frame.
#[inline]
fn dbg() -> bool {
    static ON: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&ON, || cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_XT_JIT_ROOT_SCAN"))
}

/// Opt-in verification that the roster `take_over_pass` is handed really does
/// cover every thread that can be in compiled code. Deliberately NOT part of
/// `dbg()`: the audit re-walks the whole system thread table, which is the
/// exact cost the roster exists to remove, and folding it into the ordinary
/// scan-debug flag would make the scan un-observable without also restoring
/// that cost. See `imp::audit_roster_covers_jit_peers`.
#[inline]
fn roster_audit_enabled() -> bool {
    static ON: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&ON, || cratonvm_types::flags::runtime_flag_on("CRATONVM_XT_ROOT_SCAN_AUDIT"))
}

/// Number of times the roster audit (`CRATONVM_XT_ROOT_SCAN_AUDIT=1`) found an
/// in-process thread that was absent from the roster `take_over_pass` was given
/// AND had its `Rip` inside a registered JIT code range — i.e. a peer the pass
/// would have failed to freeze and scan. Any non-zero value is a coverage hole
/// in the roster and a live use-after-free risk; see the Windows
/// `take_over_pass`'s "Coverage obligation".
///
/// Both OS arms feed it since gc-common w2-c (2026-09-23), when the Linux arm
/// moved from `/proc/self/task` to the roster; it used to live in the Windows
/// `imp` module.
pub static XT_ROSTER_MISSED_JIT_PEERS: AtomicU64 = AtomicU64::new(0);

/// Number of in-process threads seen by the audit's full enumeration but
/// absent from the roster while NOT in JIT code. Expected to be non-zero and
/// harmless: the Rust-side threads (JIT compiler, GC workers, watchdogs) never
/// execute compiled Java. Tracked so the audit can distinguish "the roster is
/// narrower, as designed" from "the roster is wrong".
pub static XT_ROSTER_SKIPPED_NON_JAVA: AtomicU64 = AtomicU64::new(0);

/// gci i12/fs (`gci-i1orch-windows-jni-foreign-attach-hangs-RETIRED-UNFIXED-20261018`): the
/// Windows roster audit's per-pass line, printed under
/// `CRATONVM_DBG_XT_JIT_ROOT_SCAN=1` next to the pass line:
///
/// `[xt-jit-roots] roster audit: audit_us=<u> walked=<w> off_roster=<n>`
///
/// `audit_us` is the audit's own time (the system-wide thread snapshot plus
/// one suspend / `GetThreadContext` / resume per off-roster in-process
/// thread). It runs inside the barrier wait, so it is a SHARE of the pause
/// line's `xt_pass_us` (and of `ttsp_us`), never an addition to it: summed
/// over a run against `ttsp_us` it says whether an audit row is audit-bound.
/// `walked` counts every process's thread entries, `off_roster` the
/// in-process threads absent from the roster (each suspended once).
#[cfg_attr(not(windows), allow(dead_code))]
fn roster_audit_line(audit_us: u128, walked: u32, off_roster: u32) -> String {
    format!(
        "[xt-jit-roots] roster audit: audit_us={audit_us} walked={walked} off_roster={off_roster}"
    )
}

/// Handles of peer threads that were suspended in JIT code and must be
/// resumed once the collection completes. Resuming is mandatory for liveness
/// (a leaked suspend wedges the peer forever), so this is `#[must_use]`.
#[must_use = "suspended peers must be resumed via resume()"]
#[derive(Default)]
pub struct TakenOver {
    precise_native_tids: Vec<u32>,
    pinned_native_tids: Vec<u32>,
    handles: Vec<isize>,
    /// OS tids of the frozen peers, parallel to `handles`. `pub(crate)` so
    /// the initiator's identity-based barrier excusal (xt-hardening
    /// 2026-07-03) can check each newly-frozen tid against the counted-set
    /// snapshot.
    pub(crate) tids: Vec<u32>,
    /// Linux: this take-over holds one count of the signal handler's
    /// session counter for its parked peers, released by `resume`
    /// (gc-common w9-g). Never set on Windows, where a frozen peer is held by
    /// its own `SuspendThread` count.
    #[cfg_attr(
        not(all(target_os = "linux", target_arch = "x86_64")),
        allow(dead_code)
    )]
    holds_session: bool,
    /// Linux: passes this take-over has run, for the bounded narrowing of
    /// [`takeover_signal_jit_only_enabled`] (gcd d10/t). Unused on Windows.
    #[cfg_attr(
        not(all(target_os = "linux", target_arch = "x86_64")),
        allow(dead_code)
    )]
    passes: u32,
    /// JIT round 16 wave 14 (lane ffm3): threads this take-over CLAIMED out of
    /// a lazy in-native FFM downcall (`CRATONVM_FFM_DOWNCALL_IN_NATIVE_LAZY`;
    /// `ThreadRegistry::claim_lazy_native_peers`), by OS tid. Each tid is
    /// also in [`Self::tids`], so every tid-keyed reader of the frozen set
    /// (the barrier excusal, the interpreter-frame walk, the G1 pins) covers
    /// it; no OS handle and no Linux signal slot belongs to it. [`resume`]
    /// hands each back (`GcBlockState::release_lazy_native_claim`).
    native_claims: Vec<(
        u32,
        std::sync::Arc<crate::threading::jvm_thread::GcBlockState>,
    )>,
    /// JIT round 18 wave 10b (lane ffmeintr, `CRATONVM_FFM_LAZY_SIGNAL_GATE`):
    /// the block state of every roster thread by OS tid (sorted), refreshed
    /// by the initiator before each Linux take-over pass
    /// ([`TakenOver::set_signal_gates`]). The pass closes a thread's lazy
    /// in-native entry (`GcBlockState::try_gate_takeover_signal`) for the
    /// instant it signals it, so the signal can never land inside the C call
    /// the thread was about to make. Empty: no gate (the switch off, Windows).
    #[cfg_attr(
        not(all(target_os = "linux", target_arch = "x86_64")),
        allow(dead_code)
    )]
    signal_gates: Vec<(
        u32,
        std::sync::Arc<crate::threading::jvm_thread::GcBlockState>,
    )>,
    /// zt t26/zc5b item 1 (`CRATONVM_XT_SKIP_TLAB_POINTER_SLOTS`, default ON,
    /// `=0` kills): the `[cursor, end, start]` words of every registered
    /// thread's `Tlab` (`[tlab, tlab + 24)`, sorted), gathered once per pause by
    /// the initiator ([`TakenOver::set_tlab_pointer_slots`]); see
    /// [`is_tlab_pointer_slot`].
    tlab_pointer_slots: Vec<(usize, usize)>,
    /// zwr (`CRATONVM_JIT_DELIVERY_BATCH_RESIDUE_JUDGED`): the delivery
    /// threads' block states, sorted by OS tid, gathered once per pause by the
    /// initiator ([`TakenOver::set_rust_band_floors`]). A frozen one's
    /// `delivery_rust_floor` caps its stack band
    /// ([`cap_at_rust_band_floor`]).
    rust_band_floors: Vec<(
        u32,
        std::sync::Arc<crate::threading::jvm_thread::GcBlockState>,
    )>,
    /// zwr: how many compiled frames, from the innermost, a split take-over
    /// keeps on the full probe (`0`: the whole innermost segment, the
    /// Generational opt-in's rule; `1`: the innermost frame only, ZGC's). See
    /// [`TakenOver::set_above_split_frames`].
    above_split_frames: usize,
    /// JIT round 19 wave 1 (lane eintr19): [`release_native_claims`] ran for
    /// this take-over. A `TakenOver` dropped WITHOUT it -- an unwind out of
    /// the pause between the claim and `resume` -- releases its claims and
    /// depths in `Drop`, so a thread claimed in C does not wait at its leave
    /// forever (and with it every later pause, which counts it and can
    /// neither claim nor reach it): a silent hang of the R17 battery's shape.
    claims_released: std::sync::atomic::AtomicBool,
    /// JIT round 19 residual (lane eintr19's page): [`resume`] ran for this
    /// take-over. A `TakenOver` dropped WITHOUT it -- an unwind out of the
    /// pause before a guard holds it (the take-over loop, the frozen-peer
    /// walk) -- resumes in `Drop`: its frozen peers (Windows `SuspendThread`,
    /// Linux parked slots), its Linux handler session, and its claims. Each
    /// left held is a peer suspended for good and every later pause waiting
    /// on it.
    resumed: bool,
    /// gci i3/tk (`CRATONVM_GEN_TAKEOVER_PROMOTES`, opt-in): this pause's
    /// static licence for a frozen peer to leave selective promotion on
    /// ([`takeover_promotes_licence`]). Set once by the collection driver
    /// (`gc_and_alloc::stw_take_over_and_wait`) before its first pass; every
    /// other `TakenOver` (heap dump, handshakes) keeps `false`, i.e. the
    /// `XT_TAKEOVER` door that also raises the un-rewritable bit.
    pub(crate) takeover_promotes: bool,
    /// gci i3/tk (`CRATONVM_GEN_HELPER_WINDOW_REFUSAL_PROMOTES`, opt-in): the
    /// same for a refusing helper window ([`helper_refusal_promotes_licence`]).
    pub(crate) helper_refusal_promotes: bool,
    /// gci i3/tk: the helper-window pass of THIS take-over recorded its
    /// refusal through the pinned door (every refused window read whole, no
    /// unreadable peer; see [`helper_window_refusal_relieved`]). Atomic only
    /// because the pass holds `&TakenOver`.
    helper_refusal_relieved: std::sync::atomic::AtomicBool,
    /// gci i4/wr (census only; `gci-i1fr-frames-roots-and-helper-windows-review-findings`
    /// item 1): the length of the driver's `xt_roots` when the helper-window
    /// pass began, so the old-mark root census splits the take-over's roots
    /// (frozen peers' bands, shadows and interpreter frames) from the
    /// helper windows' (blocked peers, their companions, the deferred
    /// blocked chains). `None`: no collection driver recorded it.
    census_helper_from: Option<usize>,
    /// gci i20/vh (`CRATONVM_XT_HELPER_WINDOW_DEFERRED_CHAIN_NATIVE`, default
    /// ON since gci i24/jp, `=0` kills):
    /// OS tids (sorted) of the blocked peers whose blocking deposit deferred a
    /// complete compiled chain to this pause
    /// (`ThreadRegistry::blocked_os_tids_with_deferred_jit_chain`), handed in
    /// by the collection driver before the helper-window pass. Empty: no
    /// demotion (the switch off, a non-G1 heap, every other `TakenOver`).
    deferred_chain_tids: Vec<u32>,
    /// ki3 (`CRATONVM_XT_HELPER_WINDOW_COVERED_CHAIN_PIN_ONLY`, default ON):
    /// OS tids (sorted) of the blocked peers whose current window's deposit
    /// covered their whole compiled chain
    /// (`ThreadRegistry::blocked_os_tids_with_covered_jit_chain`), handed in by
    /// the collection driver before the helper-window pass. Empty: every
    /// window commits its words as marking roots, as before.
    covered_chain_tids: Vec<u32>,
    /// ki3: the candidates of those peers' windows, pinned by the pass but not
    /// committed as marking roots; the driver takes them
    /// ([`Self::take_pin_only_roots`]) for its companion pins.
    pin_only_roots: std::sync::Mutex<Vec<ObjectRef>>,
    /// gcj j2/jt2 (`CRATONVM_JIT_POST_ONLY_CARD_MARK`): every general-purpose
    /// register word of every peer this take-over FROZE in compiled code, as
    /// captured at the freeze (Windows: the `GetThreadContext` the freeze
    /// read; Linux: the parked slot's register file). The peer stays frozen
    /// until [`resume`], so these are its registers for the whole pause.
    /// [`resume_completing_post_only_card_marks`] hands them to the heap's
    /// completion. A peer claimed in C has no entry (it is in no compiled
    /// store window).
    frozen_reg_words: Vec<usize>,
}

impl Drop for TakenOver {
    fn drop(&mut self) {
        if !self.resumed {
            resume_held(self);
        }
    }
}

/// `CRATONVM_FFM_LAZY_CLAIM_PIN` (JIT round 16 wave 16, lane ffm3; a
/// correctness fix, default ON, `=0` kills it): a peer claimed out of a lazy
/// in-native downcall has its roots pinned for the cycle
/// (`gc_and_alloc::pin_lazy_native_claim_roots`). `=0` disables this pin
/// publication; the explicit frozen-peer collector refusal still applies.
/// Read per claim, no cache.
pub(crate) fn lazy_claim_pin_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_FFM_LAZY_CLAIM_PIN")
}

impl TakenOver {
    /// Number of peers currently held: suspended (or parked in the Linux
    /// handler) in compiled code, plus those claimed in C (JIT round 16 wave
    /// 14: a claimed peer is frozen for this collection exactly as a
    /// suspended one -- conservatively scanned, not rewritable, excused).
    pub fn count(&self) -> usize {
        self.handles.len() + self.native_claims.len()
    }
    fn contains(&self, tid: u32) -> bool {
        self.tids.iter().any(|&t| t == tid)
    }

    /// gci i4/wr (census only): record where the helper-window pass's roots
    /// begin in the driver's `xt_roots` (see `census_helper_from`).
    pub(crate) fn note_census_helper_from(&mut self, at: usize) {
        self.census_helper_from = Some(at);
    }

    /// gci i4/wr (census only): the index recorded by
    /// [`Self::note_census_helper_from`], if any.
    pub(crate) fn census_helper_from(&self) -> Option<usize> {
        self.census_helper_from
    }

    /// gci i20/vh: hand the helper-window pass the blocked peers whose
    /// deposit deferred a complete compiled chain (see `deferred_chain_tids`).
    /// The collection driver calls it only with the switch on, on G1.
    pub(crate) fn set_deferred_chain_peers(&mut self, mut tids: Vec<u32>) {
        tids.sort_unstable();
        tids.dedup();
        self.deferred_chain_tids = tids;
    }

    /// gci i20/vh: is `tid` one of the peers [`Self::set_deferred_chain_peers`]
    /// handed in? `false` for every peer when nothing was handed in.
    #[cfg_attr(
        not(any(test, windows, all(target_os = "linux", target_arch = "x86_64"))),
        allow(dead_code)
    )]
    pub(crate) fn demotes_deferred_chain_window(&self, tid: u32) -> bool {
        self.deferred_chain_tids.binary_search(&tid).is_ok()
    }

    /// ki3: hand the helper-window pass the blocked peers whose window's
    /// deposit covered their whole compiled chain (see `covered_chain_tids`).
    /// The collection driver calls it only with the switch on.
    pub(crate) fn set_covered_chain_peers(&mut self, mut tids: Vec<u32>) {
        tids.sort_unstable();
        tids.dedup();
        self.covered_chain_tids = tids;
    }

    /// ki3: the commit of one helper window's `candidates` (the section on
    /// covered windows above `helper_window_covered_chain_pin_only_enabled`):
    /// into `roots`, the pause's marking roots -- or, for a peer
    /// [`Self::set_covered_chain_peers`] handed in, aside into
    /// `pin_only_roots`. Pins and captures are the caller's, before this.
    /// Leaves `candidates` empty either way.
    #[cfg_attr(
        not(any(test, windows, all(target_os = "linux", target_arch = "x86_64"))),
        allow(dead_code)
    )]
    pub(crate) fn commit_window_roots(
        &self,
        tid: u32,
        candidates: &mut Vec<ObjectRef>,
        roots: &mut Vec<ObjectRef>,
    ) {
        if self.covered_chain_tids.binary_search(&tid).is_err() {
            roots.append(candidates);
            return;
        }
        if xt_census_on() || crate::jit::helper_window_refusal::census_on() {
            eprintln!("[xt-hw-pin-only] tid={tid} words={}", candidates.len());
        }
        self.pin_only_roots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .append(candidates);
    }

    /// ki3: the candidates [`Self::commit_window_roots`] set aside this pass.
    pub(crate) fn take_pin_only_roots(&self) -> Vec<ObjectRef> {
        std::mem::take(
            &mut *self
                .pin_only_roots
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }

    /// gci i3/tk: did this take-over's helper-window pass record its
    /// refusal through the pinned door? See `helper_refusal_relieved`.
    pub(crate) fn helper_refusal_relieved(&self) -> bool {
        self.helper_refusal_relieved
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Conservative peers; precise native deposits keep a moving licence.
    pub(crate) fn unrewritable_count(&self) -> usize {
        self.count() - self.precise_native_tids.len()
    }

    pub(crate) fn unrewritable_os_tids(&self) -> Vec<u32> {
        self.tids
            .iter()
            .copied()
            .filter(|t| !self.precise_native_tids.contains(t))
            .collect()
    }

    pub(crate) fn push_precise_native_claim(
        &mut self,
        os_tid: u32,
        state: std::sync::Arc<crate::threading::jvm_thread::GcBlockState>,
    ) {
        state
            .lazy_native_precise_claim
            .store(true, std::sync::atomic::Ordering::Release);
        self.tids.push(os_tid);
        self.precise_native_tids.push(os_tid);
        self.native_claims.push((os_tid, state));
    }

    pub(crate) fn pinned_native_claim_count(&self) -> usize {
        self.pinned_native_tids.len()
    }

    /// A complete native band/shadow pin plus an exact interpreter deposit.
    /// Its machine frames remain immobile; its managed thread fields are
    /// rewritten before release. Count it like a pinned blocked helper window.
    pub(crate) fn push_pinned_native_claim(
        &mut self,
        os_tid: u32,
        state: std::sync::Arc<crate::threading::jvm_thread::GcBlockState>,
    ) {
        self.pinned_native_tids.push(os_tid);
        self.push_precise_native_claim(os_tid, state);
    }
    /// JIT round 16 wave 14 (lane ffm3): record a peer claimed out of a lazy
    /// in-native downcall; [`resume`] releases it.
    pub(crate) fn push_native_claim(
        &mut self,
        os_tid: u32,
        state: std::sync::Arc<crate::threading::jvm_thread::GcBlockState>,
    ) {
        self.tids.push(os_tid);
        self.native_claims.push((os_tid, state));
        // The pause publishes this peer in TakeoverVerdict::frozen before
        // any collector runs. Collector guards consume that per-VM fact;
        // a native claim never changes the JIT entry/coverage depth.
    }

    /// JIT round 18 wave 10b (lane ffmeintr): hand the next take-over pass
    /// the roster's block states (`ThreadRegistry::signal_gate_states`), or
    /// clear them. See `signal_gates`.
    /// zt t26/zc5b item 1: the pause's TLAB pointer slots (see
    /// `tlab_pointer_slots`); `ThreadRegistry::tlab_pointer_slots` gathers
    /// them, sorted.
    pub(crate) fn set_tlab_pointer_slots(&mut self, mut slots: Vec<(usize, usize)>) {
        slots.sort_unstable();
        self.tlab_pointer_slots = slots;
    }

    /// The pause's TLAB pointer slots, sorted (empty: none gathered, or the
    /// switch off).
    pub(crate) fn tlab_pointer_slots(&self) -> &[(usize, usize)] {
        &self.tlab_pointer_slots
    }

    /// zwr: the split's frame count (see the field). `0` keeps the innermost
    /// segment whole.
    pub(crate) fn set_above_split_frames(&mut self, frames: usize) {
        self.above_split_frames = frames;
    }

    /// zwr: the `max_frames` the split's segment walk takes
    /// ([`takeover_innermost_segment_top`]).
    #[cfg_attr(
        not(any(windows, all(target_os = "linux", target_arch = "x86_64"))),
        allow(dead_code)
    )]
    fn above_split_frames(&self) -> usize {
        match self.above_split_frames {
            0 => TAKEOVER_SEGMENT_MAX_FRAMES,
            n => n,
        }
    }

    /// zwr: the pause's delivery threads (`ThreadRegistry::delivery_thread_states`).
    pub(crate) fn set_rust_band_floors(
        &mut self,
        mut states: Vec<(
            u32,
            std::sync::Arc<crate::threading::jvm_thread::GcBlockState>,
        )>,
    ) {
        states.sort_unstable_by_key(|(t, _)| *t);
        self.rust_band_floors = states;
    }

    /// zwr: `tid`'s block state when it is a delivery thread of this pause.
    /// A binary search and an `Arc` clone: no allocation, safe with peers
    /// frozen.
    #[cfg_attr(
        not(any(windows, all(target_os = "linux", target_arch = "x86_64"))),
        allow(dead_code)
    )]
    fn rust_band_floor_state(
        &self,
        tid: u32,
    ) -> Option<std::sync::Arc<crate::threading::jvm_thread::GcBlockState>> {
        self.rust_band_floors
            .binary_search_by_key(&tid, |(t, _)| *t)
            .ok()
            .map(|i| std::sync::Arc::clone(&self.rust_band_floors[i].1))
    }

    pub(crate) fn set_signal_gates(
        &mut self,
        mut gates: Vec<(
            u32,
            std::sync::Arc<crate::threading::jvm_thread::GcBlockState>,
        )>,
    ) {
        gates.sort_unstable_by_key(|(t, _)| *t);
        self.signal_gates = gates;
    }

    /// The block state the pass gates for `tid`, if any.
    #[cfg_attr(
        not(all(target_os = "linux", target_arch = "x86_64")),
        allow(dead_code)
    )]
    fn signal_gate(
        &self,
        tid: u32,
    ) -> Option<std::sync::Arc<crate::threading::jvm_thread::GcBlockState>> {
        self.signal_gates
            .binary_search_by_key(&tid, |(t, _)| *t)
            .ok()
            .map(|i| std::sync::Arc::clone(&self.signal_gates[i].1))
    }

    /// How many of the held peers were claimed in C rather than frozen.
    pub(crate) fn native_claim_count(&self) -> usize {
        self.native_claims.len()
    }

    /// JIT round 19 wave 1 (lane eintr19, FFMEINTR18-1): the OS tids of the
    /// peers this take-over claimed in C (held until [`resume`]).
    pub(crate) fn native_claim_os_tids(&self) -> Vec<u32> {
        self.native_claims.iter().map(|(t, _)| *t).collect()
    }

    /// Is `tid` a claimed (not a suspended or signalled) peer?
    #[cfg_attr(
        not(all(target_os = "linux", target_arch = "x86_64")),
        allow(dead_code)
    )]
    fn is_native_claim(&self, tid: u32) -> bool {
        self.native_claims.iter().any(|(t, _)| *t == tid)
    }

    /// gcj j2/jt2: record one frozen peer's general-purpose registers, as the
    /// freeze read them (see `frozen_reg_words`). Both OS arms call it for
    /// every peer they keep frozen in compiled code, right where they record
    /// its tid.
    #[cfg_attr(
        not(any(test, windows, all(target_os = "linux", target_arch = "x86_64"))),
        allow(dead_code)
    )]
    pub(crate) fn note_frozen_registers(&mut self, regs: &[usize]) {
        self.frozen_reg_words.extend_from_slice(regs);
    }

    /// gcj j2/jt2: every register word [`Self::note_frozen_registers`]
    /// recorded this take-over.
    pub(crate) fn frozen_register_words(&self) -> &[usize] {
        &self.frozen_reg_words
    }
}

/// gcj j2/jt2 (`CRATONVM_JIT_POST_ONLY_CARD_MARK`) — [`resume`], after the
/// heap's completion of the post-only card mark: the card of every frozen
/// peer's in-range register word is dirtied
/// (`GenerationalHeap::takeover_complete_post_only_card_marks`) before any
/// frozen peer runs again. Every collection's resume goes through this
/// (`gc_and_alloc::retire_skip_spans_and_resume`; the only other resume, the
/// non-collection pause's debug keep-the-spans lever, clears no card), so a
/// collection that scanned and cleared the cards
/// while a peer was frozen between a post-only store and its card byte hands
/// that card back dirty.
///
/// A no-op beyond [`resume`] on G1 and ZGC, on Generational while no
/// post-only site was compiled, and for a take-over that froze nobody.
/// Returns the in-range words the completion dirtied.
pub(crate) fn resume_completing_post_only_card_marks(
    taken: TakenOver,
    heap: &crate::memory::VmHeap,
) -> usize {
    let words = taken.frozen_register_words();
    let completed = if words.is_empty() {
        0
    } else {
        heap.takeover_complete_post_only_card_marks(words)
    };
    if completed > 0 && dbg() {
        eprintln!(
            "[xt-jit-roots] post-only card completion: {completed} register word(s) of {} frozen peer(s) dirtied",
            taken.count()
        );
    }
    resume(taken);
    completed
}

/// JIT round 16 wave 14 (lane ffm3): hand every peer `taken` claimed out of a
/// lazy in-native downcall back to its thread (`CLAIMED -> IN`): from here its
/// leave from C may pass. Called by every [`resume`], after the collection.
fn release_native_claims(taken: &TakenOver) {
    // JIT round 19 wave 1 (lane eintr19): once per take-over, whether from
    // `resume` or from `Drop` after an unwind.
    let false = taken
        .claims_released
        .swap(true, std::sync::atomic::Ordering::AcqRel)
    else {
        return;
    };
    for (_, state) in &taken.native_claims {
        state
            .lazy_native_precise_claim
            .store(false, std::sync::atomic::Ordering::Release);
        let _ = state.release_lazy_native_claim();
    }
    if dbg() && !taken.native_claims.is_empty() {
        eprintln!(
            "[xt-jit-roots] released {} lazy in-native peer(s)",
            taken.native_claims.len()
        );
    }
}

/// Whether a held native band names compiled code, including an A5 frame
/// absent from the JIT chain. Unknown/invalid bands are never an absence proof.
/// # Safety
/// The same held-owner/live-band contract as `scan_native_claim_band`.
pub(crate) unsafe fn native_claim_band_has_compiled_word(
    lo: usize,
    hi: usize,
    ranges: &[(usize, usize)],
) -> Option<bool> {
    if !crate::threading::jvm_thread::lazy_native_band_ok(lo, hi) {
        return None;
    }
    if ranges.is_empty() {
        return Some(false);
    }
    let mut at = lo;
    while at + 8 <= hi {
        // SAFETY: the held owner's validated live band.
        let word = unsafe { (at as *const usize).read_volatile() };
        if in_code_ranges(ranges, word) {
            return Some(true);
        }
        at += 8;
    }
    Some(false)
}

/// JIT round 16 wave 14 (lane ffm3): conservatively scan the stack band
/// `[lo, hi)` of a peer claimed in C out of a lazy in-native FFM downcall,
/// pushing every word `is_obj` accepts onto `roots`. The band runs from below
/// the downcall's own frame (`panama_libffi`'s band marker; the callee-saved
/// registers of the compiled caller are spilled inside it) to the thread's
/// stack top, so it holds every compiled and Rust frame the thread had when
/// it went into C; its interpreter frames and thread fields are walked
/// separately, as for a frozen peer. Returns how many roots it pushed.
///
/// # Safety
///
/// `[lo, hi)` must be the claimed thread's live stack band
/// (`GcBlockState::try_claim_lazy_native` answered it), and the claim must be
/// held for the whole scan: the thread is then in C or waiting at its leave,
/// so every frame above its C code is frozen and mapped. The C code itself
/// may still write the lowest part of the band (its own frames start below the
/// downcall's frame, at most where the marker was), which is why every word is
/// read volatile: such a word is never a Java reference (no heap address
/// reaches C), and a garbage word that `is_obj` accepts only over-retains.
pub(crate) unsafe fn scan_native_claim_band<F>(
    lo: usize,
    hi: usize,
    is_obj: &F,
    roots: &mut Vec<ObjectRef>,
) -> usize
where
    F: Fn(usize) -> Option<ObjectRef>,
{
    if !crate::threading::jvm_thread::lazy_native_band_ok(lo, hi) {
        note_takeover_stack_incomplete();
        return 0;
    }
    let mut found = 0usize;
    let mut p = lo;
    while p + 8 <= hi {
        // SAFETY: the caller's contract (a held claim on a live band).
        let w = std::ptr::read_volatile(p as *const usize);
        if let Some(o) = is_obj(w) {
            roots.push(o);
            found += 1;
        }
        p += 8;
    }
    found
}

/// The OS threads one Linux take-over pass signals, in the order it signals
/// them (gc-common w9-g; the rule of `common-c-linux-takeover-signals-every-thread-FIXED-20260929`).
///
/// By default the ROSTER (`roster`: the registered Java threads' published
/// `gettid()`s). With the audit armed, `audit_tasks` (every `/proc/self/task`
/// entry) replaces it. Either way the list is sorted and de-duplicated (a
/// mounted virtual thread republishes its carrier's tid under its own id, so
/// the roster can name one OS thread twice), and three tids are never
/// signalled: `0` (registered, not yet at `set_os_tid_current`, so it has not
/// run Java), the collector itself, and a peer this take-over already froze.
///
/// Split out of the Linux `take_over_pass` so the choice is unit-tested on
/// every host; only the Linux arm calls it outside tests.
#[cfg_attr(
    not(all(target_os = "linux", target_arch = "x86_64")),
    allow(dead_code)
)]
fn takeover_signal_candidates(
    roster: &[u32],
    audit_tasks: Option<Vec<u32>>,
    self_tid: u32,
    taken: &TakenOver,
) -> Vec<u32> {
    let mut candidates = audit_tasks.unwrap_or_else(|| roster.to_vec());
    candidates.sort_unstable();
    candidates.dedup();
    candidates.retain(|&tid| tid != 0 && tid != self_tid && !taken.contains(tid));
    candidates
}

/// `CRATONVM_XT_TAKEOVER_SIGNAL_JIT_ONLY=1` -- let the Linux take-over signal
/// only the roster threads that can be in compiled code (gcd d10/t,
/// `common-c-linux-takeover-signals-every-thread-FIXED-20260929`). **Default ON since gce ve2
/// (2026-09-29); `=0` signals the whole roster again.**
///
/// The roster (gc-common w2-c) is every registered Java thread, and every pass
/// signals each of them: a thread parked at the barrier, blocked in a native
/// call or running the interpreter answers `STATE_NOT_JIT`, but only after a
/// `SIGUSR2` delivery and a wait for it to be scheduled (up to
/// `CRATONVM_XT_PEER_DEADLINE_MS` per attempt), and a thread blocked in a
/// syscall that `SA_RESTART` does not restart (`epoll_wait`, `nanosleep`,
/// `poll`, `select`, a timed `recv`) returns `EINTR` to whatever native code
/// made it -- HotSpot never signals a thread to reach a safepoint. A thread
/// whose PUBLISHED JIT depth (`gc_quiescence::jit_depth_of_tid`, written on
/// every entry-chain push before control transfers and on every pop) is zero
/// has no compiled frame, so it cannot have `Rip` in compiled code: the
/// narrowing skips exactly those. `None` (never published) is signalled.
///
/// Why it was opt-in until ve2: the argument rests on every transfer into compiled code going
/// through the entry chain (`JitEntryGuard`), the same claim the roster makes
/// about registration, and a gap in it would leave a compiled peer unfrozen.
/// For a COUNTED peer that costs only time -- the barrier still waits for it,
/// and after [`JIT_ONLY_SIGNAL_PASSES`] passes of one take-over the whole
/// roster is signalled again -- but an uncounted newcomer is not waited for.
/// `CRATONVM_XT_ROOT_SCAN_AUDIT=1` checks the argument whether or not this is
/// set: a peer that parks in compiled code with a published depth of zero is
/// reported as `JIT-ONLY SIGNAL MISS` (`XT_JIT_ONLY_SIGNAL_MISSES`).
///
/// gci i12/sx: read from the flag snapshot per call (once per take-over
/// pass), with no process `OnceLock`, so a per-thread test override reaches
/// it (`vm/tests/per_vm_state_statics_ratchet.rs` -1). Same value rule.
pub fn takeover_signal_jit_only_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_XT_TAKEOVER_SIGNAL_JIT_ONLY")
}

#[cfg(test)]
mod gci_i12sx_switch_tests {
    use cratonvm_types::flags::with_thread_overrides;

    const JIT_ONLY: &str = "CRATONVM_XT_TAKEOVER_SIGNAL_JIT_ONLY";

    /// gci i12/sx: default ON, `=0` off, and nothing latched between reads.
    #[test]
    fn gci_i12sx_takeover_signal_jit_only_follows_thread_overrides() {
        with_thread_overrides(&[(JIT_ONLY, None)], || {
            assert!(super::takeover_signal_jit_only_enabled(), "default ON");
        });
        with_thread_overrides(&[(JIT_ONLY, Some("0"))], || {
            assert!(!super::takeover_signal_jit_only_enabled());
        });
        with_thread_overrides(&[(JIT_ONLY, Some("1"))], || {
            assert!(super::takeover_signal_jit_only_enabled(), "nothing latched");
        });
    }
}

// gcg g3/h: the gcf f2/r opt-in `CRATONVM_XT_TAKEOVER_PARALLEL_SIGNAL` (the
// Linux take-over signalling a batch of candidates and reading
// `/proc/self/maps` once per pass; `gce-e1t-proposal-linux-takeover-signals-
// peers-in-parallel`) is DELETED. vf2 measured it never engaged: every ON pass
// signalled 0 peers, because since gce e2/t's first-pass grace and the
// uncounted-roster narrowing the passes have nobody left to signal in
// parallel (the g1 census: 0-5 take-over bands per JitOomRoot run). The
// serial pass is what ran on every row, with or without it.

/// Passes of ONE take-over that may narrow the signal set
/// ([`takeover_signal_jit_only_enabled`]); every later pass signals the whole
/// roster, so a compiled peer whose depth publication lagged is still frozen.
#[cfg_attr(
    not(all(target_os = "linux", target_arch = "x86_64")),
    allow(dead_code)
)]
const JIT_ONLY_SIGNAL_PASSES: u32 = 8;

/// `CRATONVM_DBG_XT_FORCE_TAKEOVER=1` (gce e2/t, DBG, default OFF): make the
/// take-over FIRE, so its runs are not vacuous.
///
/// A compiled loop polls on every back edge, and the inline poll sees
/// `stw_requested` within nanoseconds of the request. So a peer in a compiled
/// loop almost always reaches `jit_safepoint_slow_path` and arrives at the
/// barrier cooperatively before the take-over's first signal lands. The wave
/// e1 runs on an 8-core host under load showed `taken_over=0` on every run.
/// That left the take-over itself, the `CRATONVM_XT_TAKEOVER_SIGNAL_JIT_ONLY`
/// narrowing and the parked-peer yield unmeasured.
///
/// With the switch on, a compiled poll's slow path DECLINES to park while a
/// pause of its VM is in progress. It returns straight to the compiled code,
/// which keeps looping, so the take-over's next pass finds the peer in
/// compiled code and freezes it (a pass runs every 1 ms round for the first
/// 20 rounds). Each thread declines for at most
/// [`ForceTakeover::DECLINE_LIMIT`] per pause, then parks cooperatively as
/// usual. So a pause that never takes a peer over is late, never hung: a
/// plain `wait_for_all` pause, `CRATONVM_XT_JIT_ROOT_SCAN=0`, or a peer that
/// is in Rust code every time a signal lands.
///
/// Only the COMPILED poll declines. Interpreter polls, allocation slow paths
/// and blocking transitions park as usual. A declining peer is still a counted
/// mutator: the barrier waits for it, or excuses it once frozen. It is a
/// diagnostic, not a mode: it changes WHEN a peer stops, never what the
/// collector may assume about it.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct ForceTakeover {
    /// `None` until the first compiled poll that finds a pause reads the switch.
    enabled: Option<bool>,
    /// The barrier generation of the pause being declined, and when the
    /// first decline for it happened.
    declining: Option<(u64, std::time::Instant)>,
}

impl ForceTakeover {
    /// How long one thread declines one pause before parking cooperatively.
    pub(crate) const DECLINE_LIMIT: std::time::Duration = std::time::Duration::from_millis(200);

    pub(crate) const fn new() -> Self {
        Self {
            enabled: None,
            declining: None,
        }
    }

    /// Should this compiled poll return to compiled code instead of parking?
    /// `generation` is the VM's barrier generation, which names the pause in
    /// progress. The caller asks only while `stw_requested` is set.
    pub(crate) fn should_decline(&mut self, generation: u64) -> bool {
        let enabled = *self.enabled.get_or_insert_with(|| {
            cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_XT_FORCE_TAKEOVER")
        });
        enabled && self.decline_at(generation, std::time::Instant::now())
    }

    /// The pure half of [`Self::should_decline`]: decline while less than
    /// [`Self::DECLINE_LIMIT`] has passed since the first decline of THIS
    /// generation. A new generation starts a new window.
    fn decline_at(&mut self, generation: u64, now: std::time::Instant) -> bool {
        match self.declining {
            Some((gen, since)) if gen == generation => {
                now.saturating_duration_since(since) < Self::DECLINE_LIMIT
            }
            _ => {
                self.declining = Some((generation, now));
                true
            }
        }
    }
}

/// Take-over signals [`takeover_signal_jit_only_enabled`] withheld (census).
pub static XT_JIT_ONLY_SIGNALS_SKIPPED: AtomicU64 = AtomicU64::new(0);

/// Audit (`CRATONVM_XT_ROOT_SCAN_AUDIT=1`): peers that PARKED in compiled code
/// while their published JIT depth read zero -- the peers the narrowing of
/// [`takeover_signal_jit_only_enabled`] would have skipped. Must stay zero for
/// the narrowing to be default-able.
pub static XT_JIT_ONLY_SIGNAL_MISSES: AtomicU64 = AtomicU64::new(0);

// zt t26/zc5b item 1 (2026-10-23): a frozen or blocked peer's band holds its
// own `JvmThread`, which lives in the thread's entry frame, and with it the
// thread's `Tlab` -- `cursor`, `end` and `start`, three raw pointers that are
// never Java references. The band scans probe every word, so `end` (the first
// byte past the peer's chunk) was taken as a reference to whatever object
// starts there: the first object of the NEXT chunk, typically another
// thread's newest allocation. `GciI31GqSystemGcFinalizerChainProbe`'s churn
// thread was frozen in its compiled allocation loop at ~1 % of `System.gc()`
// pauses with `end` naming the round's `Child[]` that `main` had just put at
// the head of its own fresh chunk, so that round's twelve weak references
// survived the call (HotSpot clears them all: its maps name no such word).
// `cursor` and `start` only ever named the peer's own objects (the one ending
// at the cursor, the chunk's first), i.e. over-retention of the same kind.
// The slots are skipped by address, never by value: a frame word holding the
// same address as a real reference is still a root.

/// `CRATONVM_XT_SKIP_TLAB_POINTER_SLOTS` (zt t26/zc5b item 1, correctness,
/// default ON, `=0` kills it): the take-over and helper-window band scans
/// skip every registered thread's `Tlab` pointer words. Read once per pause
/// by the initiator (`gc_and_alloc::stw_take_over_and_wait`); no static.
pub fn skip_tlab_pointer_slots_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_XT_SKIP_TLAB_POINTER_SLOTS")
}

/// zwr: the end of a frozen peer's scanned stack band `[rsp, end)` given its
/// published Rust band floor (`GcBlockState::delivery_rust_floor`, `0`: none).
/// A floor inside the band ends it there: the words at and above it are the
/// delivery batch's VM Rust frames, which no cooperative scan of the thread
/// roots (`conservative_roots::with_chain_covered_java`). Any other floor --
/// none, at or below `rsp` (not this frame's), or past `end` -- leaves `end`:
/// the fail-safe whole band. Pure.
pub(crate) fn cap_at_rust_band_floor(rsp: usize, end: usize, floor: usize) -> usize {
    if floor > rsp && floor < end {
        floor
    } else {
        end
    }
}

/// zwr: the floor a frozen peer published, read after it is frozen (`0`: not
/// a delivery thread, or no compiled Java on its batch's stack).
#[cfg_attr(
    not(any(windows, all(target_os = "linux", target_arch = "x86_64"))),
    allow(dead_code)
)]
fn published_rust_band_floor(
    state: Option<&std::sync::Arc<crate::threading::jvm_thread::GcBlockState>>,
) -> usize {
    state.map_or(0, |s| {
        s.delivery_rust_floor
            .load(std::sync::atomic::Ordering::Acquire)
    })
}

/// Is the stack slot `at` one of `slots` (sorted, disjoint `[lo, hi)`)? One
/// binary search per object-looking band word; `false` on an empty list.
#[inline]
pub(crate) fn is_tlab_pointer_slot(slots: &[(usize, usize)], at: usize) -> bool {
    if slots.is_empty() {
        return false;
    }
    let i = slots.partition_point(|&(lo, _)| lo <= at);
    i > 0 && at < slots[i - 1].1
}

/// JIT round 24 A1 lever 1: [`is_tlab_pointer_slot`] for a band walked
/// upward. Each band scan asks about every word in ascending address order,
/// so one forward cursor over the sorted, disjoint `slots` answers the same
/// question as the per-word binary search. On `R23LeanLoop long16`
/// (Generational) that search was 95% of `classify_parked_snapshot`'s samples
/// and about 7% of the run, all of it inside young pauses.
pub(crate) struct TlabSlotCursor<'a> {
    slots: &'a [(usize, usize)],
    next: usize,
}

impl<'a> TlabSlotCursor<'a> {
    pub(crate) fn new(slots: &'a [(usize, usize)]) -> Self {
        Self { slots, next: 0 }
    }

    /// Is `at` one of the slots? Every call's `at` must be at or above the
    /// previous call's.
    #[inline]
    pub(crate) fn hit(&mut self, at: usize) -> bool {
        while self.slots.get(self.next).is_some_and(|&(_, hi)| hi <= at) {
            self.next += 1;
        }
        self.slots
            .get(self.next)
            .is_some_and(|&(lo, hi)| lo <= at && at < hi)
    }
}

/// The TLAB skip-span guard's three buckets, lifetime totals over every
/// backend that runs it (Generational and G1): `(cursor, unallocated,
/// violations)` -- roots exactly at a published span's start (the TLAB
/// cursor), roots strictly inside a span whose memory reads unallocated, and
/// roots inside a span that holds an allocation (the stale-span defect, which
/// must stay zero). See `cratonvm_gc::heap::skip_spans_hold_no_root`
/// (gcd d10/t). Printed on the take-over passes' debug lines.
pub fn skip_span_root_counts() -> (u64, u64, u64) {
    (
        cratonvm_gc::heap::SKIP_SPAN_CURSOR_ROOTS.load(Ordering::Relaxed),
        cratonvm_gc::heap::SKIP_SPAN_UNALLOCATED_ROOTS.load(Ordering::Relaxed),
        cratonvm_gc::heap::SKIP_SPAN_ROOT_VIOLATIONS.load(Ordering::Relaxed),
    )
}

/// Split take-over candidates by their published JIT depth: `(may_be_in_jit,
/// no_compiled_frame)`. `depth_of` is `gc_quiescence::jit_depth_of_tid` in
/// production; `Some(0)` is the only answer that proves a thread has no
/// compiled frame (`None` is an unknown depth, kept). Order is preserved.
#[cfg_attr(
    not(any(test, all(target_os = "linux", target_arch = "x86_64"))),
    allow(dead_code)
)]
fn split_by_published_jit_depth(
    candidates: Vec<u32>,
    depth_of: impl Fn(u32) -> Option<usize>,
) -> (Vec<u32>, Vec<u32>) {
    candidates
        .into_iter()
        .partition(|&tid| depth_of(tid) != Some(0))
}

/// `CRATONVM_XT_TAKEOVER_SKIP_IN_SYSCALL` (JIT round 17 wave 1, lane eintr; a
/// correctness fix, default ON, `=0` kills it): the Linux take-over does not
/// signal a peer the kernel reports blocked inside a system call whose user
/// `Rip` is outside every compiled-code range
/// ([`takeover_signal_skips_peer_in_syscall`]).
///
/// Why: such a peer's `SIGUSR2` handler can only answer `STATE_NOT_JIT` (its
/// `Rip` is the syscall's return address, in libc or the VM's Rust), and that
/// answer makes the pass do nothing with it -- the slot is cleared, the peer is
/// not frozen, a counted peer is still waited for at the barrier. The one
/// thing the signal DID do was make the syscall fail: `nanosleep`/`usleep`,
/// `poll`, `select`, `epoll_wait`, `sem_timedwait` and timed `recv` return
/// `EINTR` after any handler, `SA_RESTART` or not (signal(7)). That is the
/// HotSpot-visible defect of
/// `r16-ffm-takeover-signal-interrupts-blocking-syscalls-in-native-20261002`
/// for a COUNTED thread in C (a default FFM downcall, a JNI native without the
/// in-native package, a `GetPrimitiveArrayCritical` holder): HotSpot never
/// signals a thread to reach a safepoint.
///
/// Unlike a "counted thread is in C" mark (JNIEINTR-1), the predicate needs no
/// door audit: it is read from the kernel per peer, per pass, so it is right
/// for every way into C (JNI, FFM, JVMTI agents, a thread the VM never
/// marked), for virtual threads (it is per OS thread), and for a recycled tid.
/// Read per pass, no cache (no new `static`).
#[cfg_attr(
    not(all(target_os = "linux", target_arch = "x86_64")),
    allow(dead_code)
)]
fn takeover_skip_in_syscall_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_XT_TAKEOVER_SKIP_IN_SYSCALL")
}

/// What `/proc/self/task/<tid>/syscall` said about a peer (proc(5)): running
/// (on a CPU or runnable), blocked inside a system call with user `Rip`
/// `pc`, blocked outside any system call (a page fault, a stop), or
/// unreadable / malformed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(
    not(any(test, all(target_os = "linux", target_arch = "x86_64"))),
    allow(dead_code)
)]
enum PeerSyscall {
    Running,
    InSyscall {
        pc: usize,
    },
    OffSyscall,
    /// JIT round 17 wave 6 (lane eintr2): the `syscall` file could not be
    /// read, and the thread's `stat` said `S` (an interruptible sleep in the
    /// kernel, user `Rip` not reported). Only
    /// [`peer_syscall_from_stat_state`] makes it, under
    /// [`takeover_stat_fallback_enabled`].
    Sleeping,
    Unknown,
}

/// Parse one read of `/proc/self/task/<tid>/syscall`. The kernel writes
/// `running`, or `-1 <sp> <pc>` (blocked, no system call), or `<nr> <arg0>
/// .. <arg5> <sp> <pc>` (blocked in system call `nr`), each number but `nr` in
/// `0x` hex (`proc_pid_syscall`). Anything else is [`PeerSyscall::Unknown`],
/// which the pass treats as "signal as before".
#[cfg_attr(
    not(any(test, all(target_os = "linux", target_arch = "x86_64"))),
    allow(dead_code)
)]
fn parse_proc_syscall(text: &[u8]) -> PeerSyscall {
    let Ok(text) = std::str::from_utf8(text) else {
        return PeerSyscall::Unknown;
    };
    let text = text.trim();
    if text == "running" {
        return PeerSyscall::Running;
    }
    let hex = |t: &str| -> Option<usize> {
        let digits = t.strip_prefix("0x")?;
        usize::from_str_radix(digits, 16).ok()
    };
    // At most nine tokens; a tenth makes the line malformed. No allocation.
    let mut tokens = [""; 9];
    let mut count = 0usize;
    for t in text.split_ascii_whitespace() {
        if count == tokens.len() {
            return PeerSyscall::Unknown;
        }
        tokens[count] = t;
        count += 1;
    }
    let Some(nr) = tokens[0].parse::<i64>().ok() else {
        return PeerSyscall::Unknown;
    };
    match (nr, count) {
        (n, 3) if n < 0 => match hex(tokens[2]) {
            Some(_) => PeerSyscall::OffSyscall,
            None => PeerSyscall::Unknown,
        },
        (n, 9) if n >= 0 => match hex(tokens[8]) {
            Some(pc) => PeerSyscall::InSyscall { pc },
            None => PeerSyscall::Unknown,
        },
        _ => PeerSyscall::Unknown,
    }
}

/// `CRATONVM_XT_TAKEOVER_STAT_FALLBACK` (JIT round 17 wave 6, lane eintr2; a
/// correctness fix, default ON, `=0` kills it): section 4 of
/// `r17-takeover-signal-residual-eintr-windows-20261005`. When
/// `/proc/self/task/<tid>/syscall` cannot be read, ask the thread's `stat`
/// (proc(5) field 3) instead, and leave a peer the kernel reports in an
/// interruptible sleep (`S`) unsignalled, as
/// [`takeover_skip_in_syscall_enabled`] does for a peer read blocked in a
/// system call outside compiled code.
///
/// Why: `syscall` is mode `0400` and needs ptrace access, so a process that
/// made itself non-dumpable (`prctl(PR_SET_DUMPABLE, 0)`, a setuid or
/// file-capability launch: procfs then owns the file as root) and a sandbox
/// without the file (gVisor) read `Unknown` for every peer, and every
/// sleeping thread in C was signalled again -- the `EINTR` of
/// `r16-ffm-takeover-signal-interrupts-blocking-syscalls-in-native-20261002`
/// back on exactly those hosts. `stat` is mode `0444`, so it stays readable
/// when procfs hands the files to root, and gVisor implements it.
///
/// What the letter loses is the user `Rip`, so the "outside compiled code"
/// half of the rule is argued instead of read: a thread in `S` is inside the
/// kernel in an interruptible wait, which compiled code never enters (the JIT
/// emits no `syscall`; a page fault sleeps in `D`, which is still signalled).
/// The one way to an `S` with `Rip` in compiled code is a `userfaultfd`-
/// backed fault, which the VM does not set up. And skipping is the
/// `STATE_NOT_JIT` outcome anyway: the peer is not frozen and not excused, a
/// counted one is waited for at the barrier, and the next pass reads it
/// again -- latency, never a root. Read only after a failed `syscall` read,
/// so a host with the file never pays it. No cache (no new `static`).
#[cfg_attr(
    not(all(target_os = "linux", target_arch = "x86_64")),
    allow(dead_code)
)]
fn takeover_stat_fallback_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_XT_TAKEOVER_STAT_FALLBACK")
}

/// The state letter of one read of `/proc/<pid>/task/<tid>/stat` (proc(5)
/// field 3). It follows the LAST `)`: field 2 is the command name in
/// parentheses, and the name may itself hold spaces and parentheses. `None`:
/// no `)`, or the next field is not a single letter.
#[cfg_attr(
    not(any(test, all(target_os = "linux", target_arch = "x86_64"))),
    allow(dead_code)
)]
fn parse_proc_stat_state(text: &[u8]) -> Option<u8> {
    let close = text.iter().rposition(|&b| b == b')')?;
    let mut fields = text[close + 1..]
        .split(|&b| b == b' ')
        .filter(|f| !f.is_empty());
    match fields.next()? {
        [c] if c.is_ascii_alphabetic() => Some(*c),
        _ => None,
    }
}

/// What a `stat` state letter lets the take-over conclude: `S` is
/// [`PeerSyscall::Sleeping`], `R` is [`PeerSyscall::Running`], and every
/// other letter (`D` uninterruptible, `T`/`t` stopped, `Z`, `X`, `I`, ...) or
/// none is [`PeerSyscall::Unknown`] -- signalled as before.
#[cfg_attr(
    not(any(test, all(target_os = "linux", target_arch = "x86_64"))),
    allow(dead_code)
)]
fn peer_syscall_from_stat_state(state: Option<u8>) -> PeerSyscall {
    match state {
        Some(b'S') => PeerSyscall::Sleeping,
        Some(b'R') => PeerSyscall::Running,
        _ => PeerSyscall::Unknown,
    }
}

/// May the take-over leave this peer unsignalled this pass? Only when the
/// kernel read it blocked INSIDE a system call with its user `Rip` outside
/// every compiled range: the handler could then only answer `STATE_NOT_JIT`,
/// which the pass ignores, and the signal's one effect would be an `EINTR`.
///
/// Not skipped: a running peer (it may be in compiled code; a signal to a
/// thread in user mode interrupts no syscall), a peer blocked outside a
/// system call (no `EINTR` to cause), a `Rip` inside compiled code (the
/// handler would park it: a JIT-emitted syscall), and an unreadable state.
///
/// The read is a snapshot, exactly like the handler's answer: a peer that
/// leaves the syscall right after it and runs into compiled code is
/// unfrozen this pass, which is also what a `STATE_NOT_JIT` answer leaves
/// behind. A counted peer is still waited for at the barrier (the pause
/// excuses only peers it froze or claimed), and the next pass reads it
/// again, so skipping costs at most one pass of latency, never a root.
#[cfg_attr(
    not(any(test, all(target_os = "linux", target_arch = "x86_64"))),
    allow(dead_code)
)]
fn takeover_signal_skips_peer_in_syscall(state: PeerSyscall, ranges: &[(usize, usize)]) -> bool {
    match state {
        PeerSyscall::InSyscall { pc } => pc != 0 && !in_code_ranges(ranges, pc),
        // JIT round 17 wave 6 (lane eintr2): only made by the `stat`
        // fallback; see `takeover_stat_fallback_enabled` for why an
        // interruptible sleep is outside compiled code.
        PeerSyscall::Sleeping => true,
        _ => false,
    }
}

/// `CRATONVM_FFM_DOWNCALL_MASK_TAKEOVER_SIGNAL` (JIT round 17 wave 2, lane
/// ffm3, EINTR17-1; opt-in, default OFF), the take-over's half. The thread
/// half (`native-builtins` `panama_libffi::TakeoverSignalMask`) blocks the
/// take-over signal for the C code of every counted FFM downcall, so a signal
/// sent while the peer runs C stays pending and cannot fail a blocking
/// syscall with `EINTR` (the race the kernel read of
/// [`takeover_skip_in_syscall_enabled`] leaves:
/// `r17-takeover-signal-residual-eintr-windows-20261005`, section 1). This
/// half reads the peer's mask (`SigBlk` of `/proc/self/task/<tid>/status`):
/// a peer that blocks the signal is not signalled (it is in C, so its
/// handler could only answer `STATE_NOT_JIT`), and a peer that blocked it
/// after the read is given up on after one attempt instead of the whole
/// [`peer_total_deadline_ms`] budget; its pending signal answers nothing
/// (the slot is cancelled) when the call returns. Both outcomes are the
/// `STATE_NOT_JIT` outcome: the peer is not frozen and not excused, and a
/// counted one is still waited for at the barrier. Read per pass, no cache
/// (no new `static`).
#[cfg_attr(
    not(all(target_os = "linux", target_arch = "x86_64")),
    allow(dead_code)
)]
fn takeover_mask_door_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_FFM_DOWNCALL_MASK_TAKEOVER_SIGNAL")
}

/// `CRATONVM_XT_HELPER_BAND_PREREAD` (JIT round 18 wave 10, lane ffmeintr; a
/// liveness fix, default ON, `=0` restores the old order): the Linux
/// helper-window pass reads a blocked peer's published stack band
/// (`shadow_stack::thread_stack_band`, a process-wide `RwLock`) BEFORE it
/// parks the peer, not inside the parked window, where the peer may hold the
/// `malloc` arena a writer of that lock needs. Read once per parked peer,
/// before its slot is armed.
#[cfg_attr(
    not(all(target_os = "linux", target_arch = "x86_64")),
    allow(dead_code)
)]
fn helper_band_preread_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_XT_HELPER_BAND_PREREAD")
}

/// `CRATONVM_DBG_XT_UNPARKED_READ_AUDIT=1` (JIT round 18 wave 10, lane
/// ffmeintr; debug, with `CRATONVM_XT_HELPER_UNPARKED_READ=1`): every peer
/// the helper-window pass could read unparked is ALSO parked and read, the
/// parked read is the one used, and one `[xt-unparked-audit]` line compares
/// the two. Read once per pass.
#[cfg_attr(
    not(all(target_os = "linux", target_arch = "x86_64")),
    allow(dead_code)
)]
fn unparked_read_audit_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_XT_UNPARKED_READ_AUDIT")
}

/// `CRATONVM_JNI_ATTACH_UNBLOCK_TAKEOVER_SIGNAL` (JIT round 17 wave 6, lane
/// eintr2; a correctness fix, default ON, `=0` kills it): a thread that
/// becomes a Java thread through JNI `AttachCurrentThread` stops blocking the
/// take-over signal, as HotSpot's attach does (`os::create_attached_thread`
/// -> `PosixSignals::hotspot_sigmask` unblocks its suspend/resume signal,
/// `SIGUSR2`, for every attaching thread).
///
/// Why: a thread inherits its creator's mask (POSIX). One created by C code
/// inside a masked FFM downcall (`CRATONVM_FFM_DOWNCALL_MASK_TAKEOVER_SIGNAL`),
/// or by a library that masks `SIGUSR2` in its pool, attached with the signal
/// blocked and then ran compiled Java that the take-over could not reach:
/// signalled, it never answered, cost each pass the whole
/// [`peer_total_deadline_ms`] budget and was counted unclassified; under the
/// mask door it was skipped as "in C" while in compiled code. Section "Residual
/// of the residual" of `r17-takeover-signal-residual-eintr-windows-20261005`.
///
/// The handler is installed first, so the unblocked signal never meets the
/// default disposition (terminate). Only the attaching thread's own mask
/// changes; a thread already attached (a JNIEnv call from inside its own
/// masked downcall) is never touched, so its C call stays masked. Linux
/// x86-64 only (the only signal-based take-over); a no-op elsewhere.
pub(crate) fn unblock_takeover_signal_on_attach() {
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        if cratonvm_types::flags::runtime_flag_default_on(
            "CRATONVM_JNI_ATTACH_UNBLOCK_TAKEOVER_SIGNAL",
        ) {
            imp::unblock_takeover_signal_for_current_thread();
        }
    }
}

/// JIT round 18 wave 10 (lane ffmeintr, `CRATONVM_DBG_STW_WATCHDOG_SECS`): the
/// OS side of one peer for the stop-the-world watchdog dump
/// (`ThreadRegistry::debug_stw_watchdog_census`). Linux: the kernel's state
/// letter, the `syscall` line (number, arguments, `sp`, `pc`), `wchan`, whether
/// the take-over signal is blocked, and the peer's take-over slot (state,
/// helper, resume) if it has one. Diagnostic only; empty elsewhere.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub fn debug_peer_os_state(os_tid: u32) -> String {
    imp::debug_peer_os_state(os_tid)
}

/// See the Linux arm.
#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
pub fn debug_peer_os_state(_os_tid: u32) -> String {
    String::new()
}

/// Does a read of `/proc/<pid>/task/<tid>/status` say the thread blocks
/// signal `sig`? The kernel writes `SigBlk:\t<16 hex digits>` (bit `sig - 1`
/// of the 64-bit mask, proc(5)). `None`: no such line, or not hex.
#[cfg_attr(
    not(any(test, all(target_os = "linux", target_arch = "x86_64"))),
    allow(dead_code)
)]
fn parse_status_sigblk(text: &[u8], sig: i32) -> Option<bool> {
    if !(1..=64).contains(&sig) {
        return None;
    }
    const KEY: &[u8] = b"SigBlk:";
    let mut rest = text;
    loop {
        let end = rest.iter().position(|&b| b == b'\n').unwrap_or(rest.len());
        let line = &rest[..end];
        if let Some(value) = line.strip_prefix(KEY) {
            let value = std::str::from_utf8(value).ok()?.trim();
            let mask = u64::from_str_radix(value, 16).ok()?;
            return Some(mask & (1u64 << (sig - 1)) != 0);
        }
        if end >= rest.len() {
            return None;
        }
        rest = &rest[end + 1..];
    }
}

// ---------------------------------------------------------------------------
// Windows implementation
// ---------------------------------------------------------------------------

#[cfg(windows)]
mod imp {
    use super::*;
    use core::ffi::c_void;

    #[link(name = "kernel32")]
    extern "system" {
        fn GetCurrentThreadId() -> u32;
        fn GetCurrentProcessId() -> u32;
        fn OpenThread(access: u32, inherit: i32, tid: u32) -> isize;
        fn SuspendThread(h: isize) -> u32;
        fn ResumeThread(h: isize) -> u32;
        fn GetThreadContext(h: isize, ctx: *mut u8) -> i32;
        fn CloseHandle(h: isize) -> i32;
        fn CreateToolhelp32Snapshot(flags: u32, pid: u32) -> isize;
        fn Thread32First(snap: isize, entry: *mut ThreadEntry32) -> i32;
        fn Thread32Next(snap: isize, entry: *mut ThreadEntry32) -> i32;
        fn GetProcessIdOfThread(h: isize) -> u32;
    }

    /// `OpenThread` for a tid taken from this VM's roster, refusing a handle
    /// that names a thread of ANOTHER process.
    ///
    /// Windows thread ids are one system-wide namespace and are recycled
    /// quickly. A roster entry read at the top of a barrier round can outlive
    /// its thread by that round, and if the id has since been handed to a
    /// thread elsewhere on the machine, `OpenThread` succeeds (same user) and
    /// the pass SUSPENDS A FOREIGN PROCESS'S THREAD -- and, should its `Rip`
    /// happen to fall inside one of our JIT ranges numerically, keeps it
    /// suspended for the whole collection and reads "its stack" at an address
    /// that means nothing in this address space. One extra query per peer
    /// closes it (gc-common w1-c, 2026-09-23). Returns 0 exactly as a failed
    /// `OpenThread` does, which every caller already treats as "gone".
    unsafe fn open_own_thread(tid: u32) -> isize {
        let h = OpenThread(
            THREAD_GET_CONTEXT | THREAD_SUSPEND_RESUME | THREAD_QUERY_INFORMATION,
            0,
            tid,
        );
        if h == 0 {
            return 0;
        }
        if GetProcessIdOfThread(h) != GetCurrentProcessId() {
            CloseHandle(h);
            return 0;
        }
        h
    }

    // THREADENTRY32 (28 bytes). MUST stay structurally identical to the
    // declaration in `stwhang_watch.rs` — both reach `Thread32First/Next`,
    // so `clashing_extern_declarations` compares them.
    #[repr(C)]
    struct ThreadEntry32 {
        dw_size: u32,
        cnt_usage: u32,
        th32_thread_id: u32,
        th32_owner_process_id: u32,
        tp_base_pri: i32,
        tp_delta_pri: i32,
        dw_flags: u32,
    }
    const TH32CS_SNAPTHREAD: u32 = 0x0000_0004;

    // VirtualQuery — MUST stay byte-for-byte structurally identical to the
    // declarations in `memwatch.rs` / `crash_handler.rs` (same symbol →
    // `clashing_extern_declarations` deny-lint compares them).
    #[repr(C)]
    struct MemoryBasicInformation {
        base_address: *mut c_void,
        allocation_base: *mut c_void,
        allocation_protect: u32,
        partition_id: u16,
        _pad: u16,
        region_size: usize,
        state: u32,
        protect: u32,
        type_: u32,
    }
    extern "system" {
        fn VirtualQuery(
            lp_address: *const c_void,
            lp_buffer: *mut MemoryBasicInformation,
            dw_length: usize,
        ) -> usize;
    }
    const MEM_COMMIT: u32 = 0x1000;
    const PAGE_NOACCESS: u32 = 0x01;
    const PAGE_GUARD: u32 = 0x100;

    const THREAD_GET_CONTEXT: u32 = 0x0008;
    const THREAD_SUSPEND_RESUME: u32 = 0x0002;
    const THREAD_QUERY_INFORMATION: u32 = 0x0040;

    // x64 CONTEXT: 1232 bytes, 16-byte aligned.
    const CTX_SIZE: usize = 1232;
    const OFF_FLAGS: usize = 0x30;
    const OFF_RSP: usize = 0x98;
    // Rbp, the GPR slot after Rsp (gci i1/fr: the frozen frame chain).
    const OFF_RBP: usize = 0xA0;
    const OFF_RIP: usize = 0xF8;
    // Integer GPR block: Rax(0x78) .. R15(0xF0), 16 contiguous u64 slots.
    const OFF_GPR_LO: usize = 0x78;
    const OFF_GPR_HI: usize = 0xF0;
    // gcj j2/jt2: the words in `[OFF_GPR_LO, OFF_GPR_HI]`.
    const GPR_COUNT: usize = (OFF_GPR_HI - OFF_GPR_LO) / 8 + 1;
    // CONTEXT_CONTROL | CONTEXT_INTEGER for AMD64.
    //
    // No CONTEXT_FLOATING_POINT, deliberately: neither JIT tier ever holds a
    // reference in an XMM register. The single-pass operand stack puts only
    // float/double values in `StackSlot::Xmm`, and the IR tier's linear scan
    // gives XMM homes only to FP-typed intervals. A reference that is live in
    // compiled code is in a general-purpose register or a stack word, which is
    // exactly what this capture and the stack band below cover. A change that
    // lets an oop into a vector register must extend both captures first.
    const CONTEXT_CONTROL_INTEGER: u32 = 0x0010_0001 | 0x0010_0002;

    /// Backstop on how far above `Rsp` the HELPER-WINDOW pass copies a peer's
    /// stack (matches the thread-local conservative scanner's
    /// `MAX_SCAN_BYTES`). A band that reaches it is reported INCOMPLETE -- the
    /// `main-vm` thread is spawned with a 128 MiB stack, so a deep one can
    /// exceed it -- and an incomplete window is never pinned.
    const MAX_STACK_SCAN: usize = 8 * 1024 * 1024;

    /// Backstop for the TAKE-OVER scan, which reads the frozen peer's stack IN
    /// PLACE and so has no copy buffer to size. It is bounded by the stack's
    /// own reservation long before this; the constant only guards a corrupt
    /// `Rsp` that lands in some unrelated huge mapping.
    const MAX_TAKEOVER_STACK_SCAN: usize = 512 * 1024 * 1024;

    /// The readable stack band above a (frozen or suspended) peer's `Rsp`.
    ///
    /// `[lo, hi)` is what may be read; `complete` says whether it is the WHOLE
    /// of the stack above `Rsp`. An incomplete band is still a sound root
    /// source (every word in it is real stack), it just cannot license a pin
    /// that claims coverage.
    struct StackSpan {
        lo: usize,
        hi: usize,
        complete: bool,
    }

    unsafe fn query(addr: usize) -> Option<MemoryBasicInformation> {
        let mut mbi = core::mem::MaybeUninit::<MemoryBasicInformation>::uninit();
        let n = VirtualQuery(
            addr as *const c_void,
            mbi.as_mut_ptr(),
            core::mem::size_of::<MemoryBasicInformation>(),
        );
        if n == 0 {
            None
        } else {
            Some(mbi.assume_init())
        }
    }

    fn readable(mbi: &MemoryBasicInformation) -> bool {
        mbi.state == MEM_COMMIT && mbi.protect & PAGE_NOACCESS == 0 && mbi.protect & PAGE_GUARD == 0
    }

    /// Compute the readable band above `rsp`, bounded by `cap` bytes.
    ///
    /// # Why this replaced `committed_region_end` (gc-common w1-c, 2026-09-23)
    ///
    /// That function answered with the ONE `VirtualQuery` region containing
    /// `rsp`, and scanned nothing at all (registers only) when that region was
    /// not readable. Two shapes defeat it, and both are silent:
    ///
    /// * **`Rsp` inside the guard page.** A thread that has just lowered `rsp`
    ///   for a new frame but not yet touched the page is suspended with `rsp`
    ///   in the `PAGE_GUARD` page below the committed stack. The old code read
    ///   that as "unreadable" and scanned no stack words -- every compiled
    ///   frame's spill slots, from a peer that was then KEPT FROZEN (take-over)
    ///   or counted as a COMPLETE window and pinned (helper window, where
    ///   `snapshot_peer` returned `Some((ctx, 0))`). A guard page that is still
    ///   guarded has never been written (the first touch clears the attribute),
    ///   so skipping it loses nothing; the stack proper starts at the next
    ///   region.
    /// * **A committed stack split into several regions** (any page whose
    ///   protection differs, e.g. after a `VirtualProtect`). The band stopped
    ///   at the first boundary and the frames above it went unscanned, again
    ///   while claiming completeness. Adjacent readable regions of the SAME
    ///   allocation (the thread's stack reservation) are now followed.
    unsafe fn readable_stack_span(rsp: usize, cap: usize) -> StackSpan {
        let empty = StackSpan {
            lo: rsp,
            hi: rsp,
            complete: false,
        };
        let Some(first) = query(rsp) else {
            return empty;
        };
        let alloc_base = first.allocation_base as usize;
        let mut region = first;
        let mut lo = rsp;
        if !readable(&region) {
            // Only a guard or reserved page of this same stack may be stepped
            // over; anything else is a foreign or corrupt `rsp`.
            let skippable = region.protect & PAGE_GUARD != 0 || region.state != MEM_COMMIT;
            let next = (region.base_address as usize).saturating_add(region.region_size);
            let Some(above) = (if skippable { query(next) } else { None }) else {
                return empty;
            };
            if above.allocation_base as usize != alloc_base || !readable(&above) {
                return empty;
            }
            lo = next;
            region = above;
        }
        let limit = lo.saturating_add(cap);
        let mut hi = (region.base_address as usize).saturating_add(region.region_size);
        // A handful of regions at most on a real stack; the bound only stops a
        // pathological map from looping. Reaching `limit` or the iteration
        // bound is reported INCOMPLETE (the fail-safe direction, including the
        // exact-boundary case where nothing more may lie above).
        for _ in 0..64 {
            if hi >= limit {
                break;
            }
            match query(hi) {
                Some(m) if m.allocation_base as usize == alloc_base && readable(&m) => {
                    hi = (m.base_address as usize).saturating_add(m.region_size);
                }
                // The first region that is not this stack's readable memory
                // ends the stack: the band is whole.
                _ => {
                    return StackSpan {
                        lo,
                        hi,
                        complete: true,
                    };
                }
            }
        }
        StackSpan {
            lo,
            hi: hi.min(limit),
            complete: false,
        }
    }

    /// Conservatively scan a frozen peer's integer registers and used stack,
    /// pushing every value that resolves to a live object onto `roots`.
    unsafe fn scan_context<F, G>(
        ctx: &[u8; CTX_SIZE],
        is_obj: &F,
        above: Option<(&G, &cratonvm_jit::JitCodeRangeView, usize)>,
        roots: &mut Vec<ObjectRef>,
        os_tid: u32,
        tlab_slots: &[(usize, usize)],
        rust_floor: usize,
    ) -> usize
    where
        F: Fn(usize) -> Option<ObjectRef>,
        G: Fn(usize) -> Option<ObjectRef>,
    {
        let census = super::xt_census_on();
        let mut census_words = 0usize;
        let mut found = 0usize;
        // Integer registers (covers oops that live only in a register — the
        // truncated-r10 SIGSEGV signature).
        let mut off = OFF_GPR_LO;
        while off <= OFF_GPR_HI {
            let v = *(ctx.as_ptr().add(off) as *const u64) as usize;
            // Pairing capture: EVERY word, before any root filter. These
            // registers are not heap and not frame-band memory, and a Cheney
            // copy never rewrites them, so if one names an object this cycle
            // relocates, the peer resumes holding a vacated address.
            cratonvm_gc::gc_quiescence::record_peer_reg(os_tid, ((off - OFF_GPR_LO) / 8) as u8, v);
            if let Some(o) = is_obj(v) {
                if census && census_words < super::XT_CENSUS_WORDS_PER_PEER {
                    census_words += 1;
                    super::xt_census_word("takeover", os_tid, "reg", 0, v, o.as_ptr() as usize);
                }
                roots.push(o);
                found += 1;
            }
            off += 8;
        }
        // Used stack: the readable band above rsp.
        let rsp = *(ctx.as_ptr().add(OFF_RSP) as *const u64) as usize;
        if rsp == 0 || rsp & 0x7 != 0 {
            super::note_takeover_stack_incomplete();
            return found;
        }
        let span = readable_stack_span(rsp, MAX_TAKEOVER_STACK_SCAN);
        if !span.complete {
            super::note_takeover_stack_incomplete();
        }
        // zwr: a delivery thread's batch Rust frames are not read.
        let end = super::cap_at_rust_band_floor(rsp, span.hi, rust_floor);
        // gci i1/fr (`gcf-f7f-...` kind A, opt-in): the words at or above the
        // innermost compiled segment's top take the `above` probe (object
        // starts); below it, and with no plausible chain, `is_obj` as always.
        let lo = span.lo;
        let above_from = above.and_then(|(_, view, frames)| {
            let rip = *(ctx.as_ptr().add(OFF_RIP) as *const u64) as usize;
            let rbp = *(ctx.as_ptr().add(OFF_RBP) as *const u64) as usize;
            super::takeover_innermost_segment_top(
                rip,
                rbp,
                (lo, end),
                frames,
                |pc| super::compiled_frame_size_in(view, pc),
                // SAFETY: `takeover_innermost_segment_top` reads a record only
                // when `[at, at + 16)` lies inside `[lo, end)`, which
                // `VirtualQuery` confirmed committed and readable; the peer is
                // frozen.
                |at| Some((*(at as *const usize), *((at + 8) as *const usize))),
            )
        });
        if census {
            eprintln!("[xt-band] pop=takeover tid={os_tid} rsp=0x{rsp:x} end=0x{end:x}");
            if above.is_some() {
                eprintln!("[xt-above-chain] tid={os_tid} from={above_from:x?}");
            }
        }
        let mut p = span.lo;
        let mut tlab_cursor = super::TlabSlotCursor::new(tlab_slots);
        while p + 8 <= end {
            // SAFETY: VirtualQuery confirmed [lo, end) is committed+readable
            // and the peer is frozen, so the words are stable for this read.
            let w = *(p as *const usize);
            // zt t26/zc5b item 1: a `Tlab` pointer word is never a reference.
            if tlab_cursor.hit(p) {
                p += 8;
                continue;
            }
            let probed = match (above, above_from) {
                (Some((above_probe, _, _)), Some(from)) if p >= from => above_probe(w),
                _ => is_obj(w),
            };
            if let Some(o) = probed {
                if census && census_words < super::XT_CENSUS_WORDS_PER_PEER {
                    census_words += 1;
                    super::xt_census_word("takeover", os_tid, "stack", p, w, o.as_ptr() as usize);
                }
                // Pairing capture, STACK side -- the JIT spill slots the
                // `CompiledUninterruptible` comment names alongside registers.
                // Gated on `is_obj` rather than taking every word: a
                // `pointer_map` key is by construction an object this cycle
                // relocated, so the collector's own object test accepts it,
                // and the alternative is millions of words per peer. Register
                // 0xff marks the stack side.
                // 0xfe = TAKE-OVER stack (peer held frozen for the whole
                // collection); 0xff = helper window (suspended and resumed
                // inside root gathering, before anything relocates).
                cratonvm_gc::gc_quiescence::record_peer_reg(os_tid, 0xfe, w);
                // The UNROUTED census, as the Linux arm has always fed it
                // (gc-common w2-c; `common-c-takeover-small-residue` row 2):
                // without it `PEER_STACK_SLOTS_UNROUTED` -- "a frozen peer's
                // stack word named an object this cycle moved" -- was
                // structurally zero for Windows take-over peers. The mutex it
                // takes cannot be held by a frozen peer (`Rip` in compiled
                // code holds no Rust lock), and the take-over share of the
                // buffer is capped so the helper-window repair keeps its room.
                cratonvm_gc::gc_quiescence::record_takeover_stack_slot(os_tid, p, w);
                roots.push(o);
                found += 1;
            }
            p += 8;
        }
        found
    }

    /// One enumeration pass: suspend each not-yet-taken peer thread, and for
    /// those whose `Rip` is in JIT code, scan + keep them frozen. Returns the
    /// number of peers newly taken over this pass.
    ///
    /// `live_tids` is the roster of OS thread ids this pass may freeze — the
    /// registered Java threads, re-read by the caller on every barrier round
    /// so a thread that registers mid-collection is picked up on the next one.
    ///
    /// ## Why a caller-supplied roster and not `CreateToolhelp32Snapshot`
    ///
    /// (2026-09-10.) That call enumerates every thread on the MACHINE, not in
    /// this process, and the `Thread32Next` walk that follows filters the
    /// whole system table down to the handful of entries that belong here.
    /// Measured on the reference box: 6,952 system threads to find 44 of ours,
    /// 9.9ms for the snapshot and 83ms for snapshot+walk — per pass. This pass
    /// runs once per barrier round, so a `VthreadGcStress` run spent 14.5s
    /// across 174 passes walking the machine's thread table to freeze nothing
    /// at all (0 peers taken over in every pass of every run measured). That
    /// cost is why `CRATONVM_XT_JIT_ROOT_SCAN=0` finished the same workload in
    /// 5s against 13-37s with the scan on; it was never the SuspendThread /
    /// GetThreadContext / ResumeThread triples, which come to ~0.1s for all
    /// 6,960 of them.
    ///
    /// The Linux implementation never had the enumeration cost -- it read
    /// `/proc/self/task`, which is process-local -- but it had the signalling
    /// cost of every non-Java thread in the process, and since gc-common w2-c
    /// (2026-09-23) it takes this same roster. Both platforms ask the same
    /// question, and `CRATONVM_XT_ROOT_SCAN_AUDIT=1` checks the answer on both.
    ///
    /// ## Coverage obligation
    ///
    /// The roster MUST contain every thread that can have `Rip` inside a
    /// registered JIT code range. A peer this pass does not freeze is a peer
    /// whose registers and spill slots go unscanned — a missed conservative
    /// root, and a use-after-free of an object only that peer still names.
    ///
    /// It does contain them: compiled code is entered only through
    /// `JitEntryGuard::enter_with_compiled` / `enter_with_compiled_at`, whose
    /// production call sites all sit on Java execution paths
    /// (`runtime::interpreter`, `runtime::interpreter::jit_bridge`,
    /// `jit::helpers`), and every thread executing Java is in the registry
    /// with its OS tid published. (`memory::roots` uses the plain
    /// `JitEntryGuard::enter`, which records a chain entry for the root walk
    /// and transfers control to nothing, so it never puts `Rip` in JIT code.)
    ///
    /// Because being wrong here is silent and fatal, the argument is also
    /// checked at runtime rather than only asserted: setting
    /// `CRATONVM_XT_ROOT_SCAN_AUDIT=1` enables
    /// [`audit_roster_covers_jit_peers`], which re-walks the full system
    /// snapshot and reports any in-process thread absent from the roster,
    /// loudly if its `Rip` is in JIT code.
    pub fn take_over_pass<F>(
        taken: &mut TakenOver,
        is_obj: &F,
        roots: &mut Vec<ObjectRef>,
        live_tids: &[u32],
    ) -> usize
    where
        F: Fn(usize) -> Option<ObjectRef>,
    {
        take_over_pass_split::<F, F>(taken, is_obj, None, roots, live_tids)
    }

    /// [`take_over_pass`] with a second probe for the stack words at or above
    /// each frozen peer's innermost compiled segment (gci i1/fr, `gcf-f7f-...`
    /// kind A; see `super::takeover_above_chain_word_probe`). `above: None` is
    /// `take_over_pass` exactly.
    pub fn take_over_pass_split<F, G>(
        taken: &mut TakenOver,
        is_obj: &F,
        above: Option<&G>,
        roots: &mut Vec<ObjectRef>,
        live_tids: &[u32],
    ) -> usize
    where
        F: Fn(usize) -> Option<ObjectRef>,
        G: Fn(usize) -> Option<ObjectRef>,
    {
        let self_tid = unsafe { GetCurrentThreadId() };
        // zt t26/zc5b item 1: copied before any peer is suspended (no
        // allocation while one is frozen).
        let tlab_slots = taken.tlab_pointer_slots().to_vec();
        // BUG-03 deadlock avoidance: snapshot the JIT code ranges BEFORE
        // suspending any peer, and classify every frozen peer's Rip against
        // this local copy. (`lookup_jit_code_range` itself is lock-free today
        // -- an `arc_swap` snapshot plus a binary search -- but anything that
        // allocates or takes a lock while peers are frozen can block on a
        // lock a frozen peer holds, so no such call belongs inside the loop.)
        let ranges = crate::jit::jit_code_ranges_snapshot();
        // gci i1/fr: the code-range view the frame-chain walk reads, taken
        // with the ranges (before any peer is suspended), only for the split.
        let view = above.map(|_| cratonvm_jit::jit_code_range_view());
        // zwr: how many frames of the innermost segment keep the full probe.
        let frames = taken.above_split_frames();
        let above_v = above.zip(view.as_ref()).map(|(g, v)| (g, v, frames));
        let mut newly = 0usize;
        let mut dbg_suspended = 0usize;
        let mut unclassified = 0usize;
        let mut roots_this_pass = 0usize;
        for &tid in live_tids {
            // tid 0 is "unpublished": a thread that registered but has not yet
            // reached `set_os_tid_current`. It has not run Java either, so it
            // cannot be in compiled code, and OpenThread(0) would fail anyway.
            if tid != 0 && tid != self_tid && !taken.contains(tid) {
                dbg_suspended += 1;
                let floor_state = taken.rust_band_floor_state(tid);
                match unsafe {
                    try_take(
                        tid,
                        &ranges,
                        is_obj,
                        above_v,
                        roots,
                        &tlab_slots,
                        floor_state.as_ref(),
                    )
                } {
                    Take::Gone | Take::NotJit => {}
                    Take::Unclassified => {
                        // Opened but could not be suspended or read: the peer
                        // is still RUNNING and nothing here saw its registers.
                        // The Linux arm has always counted this shape (a
                        // signal no handler answered); the Windows arm dropped
                        // it on the floor, so its sweeps reported coverage
                        // they did not have (gc-common w1-c, 2026-09-23).
                        unclassified += 1;
                        XT_PEERS_UNCLASSIFIED.fetch_add(1, Ordering::Relaxed);
                    }
                    Take::Kept(kept, found, regs) => {
                        taken.handles.push(kept);
                        taken.tids.push(tid);
                        // gcj j2/jt2: the registers the freeze read, for the
                        // post-only card completion at resume.
                        taken.note_frozen_registers(&regs);
                        newly += 1;
                        roots_this_pass += found;
                        XT_THREADS_TAKEN_OVER.fetch_add(1, Ordering::Relaxed);
                        XT_ROOTS_FOUND.fetch_add(found as u64, Ordering::Relaxed);
                        // The roots we just contributed came from a FROZEN
                        // peer's register file and raw stack — conservatively
                        // discovered and, critically, not rewritable: this
                        // collection never applies its pointer map to a frozen
                        // peer (it is excused from the barrier and resumes
                        // straight back into compiled code). The module's
                        // soundness argument used to lean on "a frozen in-JIT
                        // peer keeps its JIT-entry guard live, so `is_active()`
                        // stays true and the heap performs a non-moving sweep" —
                        // which stopped being automatic the moment moving-young
                        // could run under `is_active()`. State the obligation
                        // directly instead of inheriting it. (Also asserted at
                        // the call site; kept here so any future caller of this
                        // pass inherits the guarantee.) gci i3/tk: under the
                        // pause's promotion licence the reason goes through the
                        // pinned door (`mark_takeover_peer_frozen`).
                        super::mark_takeover_peer_frozen(taken);
                        if dbg() {
                            eprintln!(
                                "[xt-jit-roots] took over tid={tid} (Rip in JIT): {found} conservative roots"
                            );
                        }
                    }
                }
            }
        }
        if unclassified > 0 {
            XT_CYCLES_WITH_UNCLASSIFIED.fetch_add(1, Ordering::Relaxed);
        }
        // Publish this pass where the SWEEP can read it, as the Linux arm has
        // done since H2-CID0. Without it every Windows sweep captured
        // `xt_passes=0` into the young-freed ring, and the reclaim guard
        // reported `root_coverage=NEVER-LOOKED` for a sweep whose take-over had
        // in fact run -- on the platform this VM is mostly debugged on.
        cratonvm_gc::gc_quiescence::publish_xt_pass(
            newly as u64,
            unclassified as u64,
            roots_this_pass as u64,
        );
        if roster_audit_enabled() {
            // gci i12/fs (`gci-i1orch-windows-jni-foreign-attach-hangs`): the
            // audit runs inside the barrier wait, so its cost is part of the
            // pause line's `xt_pass_us` and `ttsp_us`. Timed here, printed only
            // with the scan-debug flag (the audit rows set both), so a row can
            // tell an audit-bound pause from a slow safepoint.
            let audit_t0 = std::time::Instant::now();
            let (walked, off_roster) = audit_roster_covers_jit_peers(self_tid, live_tids, &ranges);
            if dbg() {
                eprintln!(
                    "{}",
                    roster_audit_line(audit_t0.elapsed().as_micros(), walked, off_roster)
                );
            }
        }
        if dbg() {
            eprintln!(
                "[xt-jit-roots] pass: examined {dbg_suspended} peer(s), {newly} newly taken over (Rip in JIT); {} code ranges; any_thread_in_jit={} jit_gate={} rejected_indirect={} bandless_windows={} skip_span_roots={:?}",
                ranges.len(),
                crate::jit::conservative_roots::any_thread_in_jit(),
                cratonvm_jit::xt_jit_root_scan_enabled(),
                // gc-common w6-g: lifetime totals. `rejected_indirect` > 0
                // means a compiled frame published a non-reference in a
                // reference home (`ShadowOddLongProbe`, a JIT bug the band
                // defence contains); `bandless_windows` > 0 means a frozen
                // peer's indirect entries could not be checked at all.
                super::shadow_rejected_indirect_entries(),
                super::XT_TAKEOVER_SHADOW_BANDLESS.load(Ordering::Relaxed),
                // gcd d10/t: (cursor, unallocated, violations).
                super::skip_span_root_counts(),
            );
        }
        newly
    }

    /// Check, the expensive way, that the roster handed to `take_over_pass`
    /// really did cover every thread that could be in compiled code.
    ///
    /// This deliberately does the thing `take_over_pass` no longer does — walk
    /// the whole system thread table — and then suspends each in-process
    /// thread the roster omitted just long enough to read its `Rip`. It is
    /// gated on its own `CRATONVM_XT_ROOT_SCAN_AUDIT` rather than on `dbg()`
    /// because that walk is precisely the 83ms-per-pass cost the roster exists
    /// to avoid: leaving it on the ordinary scan-debug flag would put the cost
    /// back the moment anyone tried to observe the scan, which is how the
    /// coupling described in `stw_takeover_should_scan` stayed invisible. It is
    /// not a fast path and must never be called on one.
    ///
    /// The point is that the roster's coverage argument is a claim about every
    /// site that can transfer into compiled code, and such claims rot silently
    /// as call sites are added. This turns the claim into something a soak run
    /// can falsify.
    ///
    /// gci i12/fs: answers `(walked, off_roster)` -- the system-wide thread
    /// entries the snapshot listed (every process's, which is why the walk
    /// costs what it costs on a busy host) and the in-process threads absent
    /// from the roster, each suspended once to read its `Rip`; `(0, 0)` when
    /// the snapshot could not be taken. For [`roster_audit_line`] only.
    fn audit_roster_covers_jit_peers(
        self_tid: u32,
        live_tids: &[u32],
        ranges: &[(usize, usize)],
    ) -> (u32, u32) {
        let pid = unsafe { GetCurrentProcessId() };
        let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
        if snap == -1 || snap == 0 {
            return (0, 0);
        }
        let mut walked = 0u32;
        let mut off_roster = 0u32;
        let mut e: ThreadEntry32 = unsafe { core::mem::zeroed() };
        e.dw_size = core::mem::size_of::<ThreadEntry32>() as u32;
        let mut ok = unsafe { Thread32First(snap, &mut e) };
        while ok != 0 {
            walked = walked.saturating_add(1);
            let tid = e.th32_thread_id;
            if e.th32_owner_process_id == pid
                && tid != self_tid
                && tid != 0
                && !live_tids.contains(&tid)
            {
                off_roster = off_roster.saturating_add(1);
                match unsafe { rip_of(tid) } {
                    Some(rip) if crate::jit::xt_root_scan::in_code_ranges(&ranges, rip) => {
                        let hole = XT_ROSTER_MISSED_JIT_PEERS.fetch_add(1, Ordering::Relaxed) + 1;
                        eprintln!(
                            "[xt-jit-roots] ROSTER HOLE #{hole}: tid={tid} is in JIT code (rip={rip:#x}) \
                             but was absent from the roster, so take_over_pass would NOT have \
                             frozen or scanned it. Its registers and spill slots are unscanned \
                             conservative roots. See take_over_pass's coverage obligation."
                        );
                    }
                    // Absent and not in JIT code: a Rust-side thread (compiler,
                    // GC worker, watchdog), narrower-by-design, not a hole.
                    _ => {}
                }
            }
            e.dw_size = core::mem::size_of::<ThreadEntry32>() as u32;
            ok = unsafe { Thread32Next(snap, &mut e) };
        }
        unsafe { CloseHandle(snap) };
        (walked, off_roster)
    }

    /// Suspend `tid` just long enough to read its `Rip`, then resume it.
    /// Audit-only: unlike `try_take` this never keeps the peer frozen and
    /// never scans it, so it cannot contribute roots.
    unsafe fn rip_of(tid: u32) -> Option<usize> {
        let h = OpenThread(
            THREAD_GET_CONTEXT | THREAD_SUSPEND_RESUME | THREAD_QUERY_INFORMATION,
            0,
            tid,
        );
        if h == 0 {
            return None;
        }
        if SuspendThread(h) == u32::MAX {
            CloseHandle(h);
            return None;
        }
        #[repr(C, align(16))]
        struct Ctx([u8; CTX_SIZE]);
        let mut ctx = Ctx([0u8; CTX_SIZE]);
        *(ctx.0.as_mut_ptr().add(OFF_FLAGS) as *mut u32) = CONTEXT_CONTROL_INTEGER;
        let got = GetThreadContext(h, ctx.0.as_mut_ptr());
        let rip = if got == 0 {
            None
        } else {
            Some(*(ctx.0.as_ptr().add(OFF_RIP) as *const u64) as usize)
        };
        ResumeThread(h);
        CloseHandle(h);
        rip
    }

    /// What one [`try_take`] learned about a peer.
    enum Take {
        /// `OpenThread` failed: the thread has exited since the roster was
        /// read. Nothing to scan and nothing running -- not a coverage hole.
        Gone,
        /// Opened, but `SuspendThread` or `GetThreadContext` failed. The peer
        /// may still be running compiled code, and nothing saw its registers.
        Unclassified,
        /// `Rip` outside compiled code: resumed, it arrives cooperatively.
        NotJit,
        /// Frozen in compiled code and scanned; the handle stays suspended.
        /// gcj j2/jt2: with the 16 general-purpose registers the freeze read.
        Kept(isize, usize, [usize; GPR_COUNT]),
    }

    /// Suspend `tid`, read its context. If its `Rip` is in a JIT code range,
    /// scan it (registers, machine stack AND shadow stack) and return
    /// [`Take::Kept`] with the handle still suspended; otherwise resume and
    /// close it. Nothing is held on any other outcome.
    unsafe fn try_take<F, G>(
        tid: u32,
        ranges: &[(usize, usize)],
        is_obj: &F,
        above: Option<(&G, &cratonvm_jit::JitCodeRangeView, usize)>,
        roots: &mut Vec<ObjectRef>,
        tlab_slots: &[(usize, usize)],
        floor_state: Option<&std::sync::Arc<crate::threading::jvm_thread::GcBlockState>>,
    ) -> Take
    where
        F: Fn(usize) -> Option<ObjectRef>,
        G: Fn(usize) -> Option<ObjectRef>,
    {
        let h = open_own_thread(tid);
        if h == 0 {
            return Take::Gone;
        }
        // SuspendThread returns (prev suspend count) or u32::MAX on failure.
        if SuspendThread(h) == u32::MAX {
            CloseHandle(h);
            return Take::Unclassified;
        }
        #[repr(C, align(16))]
        struct Ctx([u8; CTX_SIZE]);
        let mut ctx = Ctx([0u8; CTX_SIZE]);
        *(ctx.0.as_mut_ptr().add(OFF_FLAGS) as *mut u32) = CONTEXT_CONTROL_INTEGER;
        // `GetThreadContext` on a suspended thread is also what makes the
        // (asynchronous) `SuspendThread` synchronous: it does not return until
        // the target has actually stopped, so the context is not torn.
        if GetThreadContext(h, ctx.0.as_mut_ptr()) == 0 {
            ResumeThread(h);
            CloseHandle(h);
            return Take::Unclassified;
        }
        let rip = *(ctx.0.as_ptr().add(OFF_RIP) as *const u64) as usize;
        // Lock-free classification against the pre-suspend snapshot (see
        // `take_over_pass`): nothing that can allocate or lock runs while a
        // peer is frozen.
        let in_jit = crate::jit::xt_root_scan::in_code_ranges(&ranges, rip);
        if in_jit {
            // Pure JIT instruction stream → holds no VM lock → safe to freeze.
            // zwr: read once the peer is suspended.
            let rust_floor = super::published_rust_band_floor(floor_state);
            let mut found = scan_context(&ctx.0, is_obj, above, roots, tid, tlab_slots, rust_floor);
            // The oops compiled code parked in the shadow stack across a call
            // are in neither the registers nor the machine stack.
            // zwr: exact oops by construction, never a cursor, so a split pass
            // reads them with its exact-start probe (no companion).
            found += match above {
                Some((above_probe, _, _)) => {
                    super::scan_taken_over_peer_shadow(tid, above_probe, roots)
                }
                None => super::scan_taken_over_peer_shadow(tid, is_obj, roots),
            };
            // gcj j2/jt2: the integer GPR block, Rax .. R15, as read above.
            let mut regs = [0usize; GPR_COUNT];
            for (i, off) in (OFF_GPR_LO..=OFF_GPR_HI).step_by(8).enumerate() {
                regs[i] = *(ctx.0.as_ptr().add(off) as *const u64) as usize;
            }
            Take::Kept(h, found, regs) // keep suspended; caller records the handle
        } else {
            // Interpreter / native / already parked → let it arrive cooperatively.
            ResumeThread(h);
            CloseHandle(h);
            Take::NotJit
        }
    }

    /// A4 (fork6-fjp) — post-barrier helper-window pass: conservatively scan
    /// the register file + used stack of every remaining peer (not the
    /// initiator, not already taken over) whose native stack contains a JIT
    /// return address, appending the discovered roots to `roots`. Returns
    /// `(helper_windows_found, roots_found)`.
    ///
    /// ## When this runs and why it is complete
    ///
    /// Called AFTER the STW barrier is satisfied. At that point every alive
    /// peer is exactly one of:
    ///   * arrived cooperatively — published `update_root_snapshot` (which
    ///     includes `scan_active_jit_frames`) and is parked at the barrier;
    ///   * taken over — frozen in JIT and scanned by `take_over_pass`;
    ///   * BLOCKED (`threads_blocked`-excluded) — covered by its
    ///     `deposit_root_snapshot`. That deposit has scanned the compiled
    ///     chain since `24cf0b19b0` (2026-06-30), eagerly or, with the lazy
    ///     deposit, through a record this pause walks
    ///     (`conservative_roots::resolve_deferred_blocked_jit_chains`); what
    ///     this pass adds is the REST of the band (the Rust frames around the
    ///     chain), pinned for a window and captured for the blocked-wake repair
    ///     for every band. A band whose chain a record covers is demoted to the
    ///     pure-native path (gci i20/vh, default ON since gci i24/jp, G1).
    ///
    /// ## Why the reads are sound without keeping the peer frozen for GC
    ///
    /// The peer is suspended only long enough to read its context and copy
    /// its used stack into a pre-allocated local buffer (both allocation-free
    /// operations — a peer suspended mid-`malloc` inside a Rust helper could
    /// otherwise deadlock us on the allocator lock). It is resumed before any
    /// classification/scanning happens. The copied band stays a faithful root
    /// source for this collection because a blocked thread cannot unwind past
    /// its blocked-region leave while the pause is active
    /// (`BlockedGuard::drop` / `mark_blocked_region_leave` wait out the STW
    /// before decrementing), and blocked-region code does not touch the Java
    /// heap — so the JIT frames above the blocking point are stable and no
    /// NEW heap references can appear on that stack during the collection.
    ///
    /// ## Why over-retention is bounded (vs the reverted 2026-06-19b deposit
    /// scan)
    ///
    /// The reverted attempt scanned + pinned the full blocked stack at EVERY
    /// deposit (every blocking native call, program-wide, persisting in the
    /// snapshot). This pass runs once per collection, only when a blocked
    /// thread exists while some thread holds live JIT frames, contributes
    /// roots only for stacks that actually carry a JIT return address, and
    /// the roots live only for the one collection that scanned them.
    ///
    /// `_unparked` (JIT round 18 wave 10, EINTR2-1) is the Linux arm's: a
    /// suspended peer needs no signal, so every peer is read as before.
    pub fn helper_window_pass<F>(
        taken: &TakenOver,
        is_obj: &F,
        roots: &mut Vec<ObjectRef>,
        blocked_os_tids: &[u32],
        _unparked: &[(
            u32,
            std::sync::Arc<crate::threading::jvm_thread::GcBlockState>,
        )],
    ) -> (usize, usize)
    where
        F: Fn(usize) -> Option<ObjectRef>,
    {
        // Per-cycle verdict: open it before any early return.
        super::reset_helper_window_cycle();
        let tlab_slots = taken.tlab_pointer_slots().to_vec();
        // gcd d3/m: the blocked-monitor peers credited by proof this pause
        // whose bands the young pin ledger needs (empty unless the proof flag
        // and a ledger-licensed move are on), and whether pinned windows'
        // bands go into the ledger too (`CRATONVM_GEN_PINNED_YOUNG_COPY_TAKEOVER`).
        let ledger_only = cratonvm_gc::gc_quiescence::take_proven_monitor_peers_for_ledger();
        let band_capture = super::helper_window_band_capture_enabled();
        let ranges = crate::jit::jit_code_ranges_snapshot();
        if ranges.is_empty() || (blocked_os_tids.is_empty() && ledger_only.is_empty()) {
            if !ledger_only.is_empty() {
                // A credited peer's band was owed and cannot be read here.
                cratonvm_gc::gc_quiescence::mark_peer_band_capture_incomplete();
            }
            return (0, 0);
        }
        // gci i1/fr (g3h item 1): read once per pass.
        let zero_depth_native = super::helper_window_zero_depth_native_enabled();
        let young_range = cratonvm_gc::gc_quiescence::young_pin_range();
        let mut band_words: Vec<usize> = Vec::new();
        let self_tid = unsafe { GetCurrentThreadId() };
        // Reusable copy buffer for each peer's used stack. Pre-sized so the
        // common case never allocates while a peer is frozen; grown (with the
        // peer running) when a band is larger.
        let mut band: Vec<u8> = Vec::with_capacity(256 * 1024);
        let mut candidates: Vec<ObjectRef> = Vec::new();
        // gcg g4/xt: one band's stack-slot captures, reused across peers.
        let mut pending_captures: Vec<(usize, usize)> = Vec::new();
        let mut windows = 0usize;
        let mut pinned_windows = 0usize;
        let mut unpinned_windows = 0usize;
        // gci i3/tk: unpinned windows whose band was not read whole.
        let mut unpinned_incomplete = 0usize;
        // Blocked peers whose stack could not be read at all.
        let mut unknown_peers = 0usize;
        let mut found_total = 0usize;
        // Iterate the blocked roster directly rather than walking the system
        // thread table (2026-09-10). The filter below was always
        // `blocked_os_tids.contains(&tid)`, so `CreateToolhelp32Snapshot` +
        // `Thread32Next` were enumerating every thread on the MACHINE — 6,952
        // of them on the reference box — purely to intersect with a list this
        // function is handed as an argument. That walk cost ~83ms a pass and
        // this pass runs once per collection: 5.4s across the 65 passes of one
        // `VthreadGcStress` run. Iterating the roster is exactly equivalent
        // (a blocked tid is in-process by construction) and does no I/O.
        // JIT round 17 wave 1 (lane eintr): once per OS thread, as the Linux
        // arm (the registry's blocked list is not de-duplicated).
        let mut blocked_once: Vec<u32> = blocked_os_tids.to_vec();
        blocked_once.sort_unstable();
        blocked_once.dedup();
        for &tid in &blocked_once {
            // xt-hardening follow-up (2026-07-03): this pass exists ONLY to
            // close the BLOCKED-thread coverage gap (the deposit covers the
            // compiled chain since `24cf0b19b0`, not the Rust frames around
            // it; gci i25/rv) — a cooperatively-arrived mutator
            // already published its JIT roots via update_root_snapshot
            // before parking at the barrier. Scanning it again is pure
            // redundant over-retention risk (a false-positive conservative
            // candidate can only ever help correctness for a REAL gap;
            // widening the candidate volume for threads that need no help
            // just adds corruption surface). Skip any peer not in the
            // blocked-tid snapshot — this also skips the suspend/resume
            // round-trip entirely for the (large majority) non-blocked case.
            if tid != 0 && tid != self_tid && !taken.contains(tid) {
                let snap = match unsafe { snapshot_peer(tid, &mut band) } {
                    Snapshot::Gone => continue,
                    Snapshot::Failed => {
                        // A blocked peer we could not read. Its JIT band (if
                        // any) is in no root set, so this cycle must not
                        // relocate on the strength of "every window pinned";
                        // and its shadow stack -- which needs no suspension,
                        // the peer is blocked -- is still worth marking from.
                        //
                        // Until 2026-09-23 this peer was simply skipped: not a
                        // window, not unpinned, so a cycle whose only unread
                        // peer was this one DISCHARGED and relocated.
                        unknown_peers += 1;
                        XT_PEERS_UNCLASSIFIED.fetch_add(1, Ordering::Relaxed);
                        crate::jit::helper_window_refusal::note_unreadable_helper_peer(
                            "win",
                            tid,
                            "snapshot-failed",
                        );
                        let before = roots.len();
                        if crate::jit::conservative_roots::xt_peer_shadow_scan_enabled() {
                            let _ = super::scan_peer_shadow_window(tid, is_obj, roots);
                        }
                        found_total += roots.len() - before;
                        continue;
                    }
                    Snapshot::Ok(s) => s,
                };
                candidates.clear();
                let ctx = &snap.ctx;
                // Integer registers: a callee-saved register can still
                // hold a JIT-frame oop that no Rust callee spilled.
                let mut off = OFF_GPR_LO;
                let mut has_jit = false;
                while off <= OFF_GPR_HI {
                    // SAFETY: `ctx` is a fully-initialized CONTEXT copy;
                    // read_unaligned because the by-value array is only
                    // byte-aligned.
                    let v =
                        unsafe { (ctx.as_ptr().add(off) as *const u64).read_unaligned() } as usize;
                    // Pairing capture -- see the take-over path. EVERY
                    // register, before any root filter: besides the pairing
                    // diagnostic, `record_peer_reg` puts a register word in a
                    // published young region into the young pin ledger, and
                    // that region's low slack and one-past-end words are not
                    // object-looking (gcg g4/xt: the Linux arms now do the
                    // same; `gcg-g3h-...-review-findings` item 4).
                    cratonvm_gc::gc_quiescence::record_peer_reg(
                        tid,
                        ((off - OFF_GPR_LO) / 8) as u8,
                        v,
                    );
                    if !has_jit && crate::jit::xt_root_scan::in_code_ranges(&ranges, v) {
                        has_jit = true;
                    }
                    if let Some(o) = is_obj(v) {
                        candidates.push(o);
                    }
                    off += 8;
                }
                // ONE pass over the band (gc-common w1-c, 2026-09-23). This used
                // to probe every band word TWICE -- once for the stack-slot /
                // pairing capture, once more in `classify_helper_window_words`
                // -- and the default probe is `resolve_interior_for_pin`, which
                // on the generational heap is `is_heap_addr`: three mutex
                // acquisitions per word. Same outputs, half the probes.
                //
                // The band is a COPY of `[band_lo, band_lo + band_len)`, so the
                // peer's real address for word `i` is `band_lo + i*8` -- which
                // is what the blocked-wake fixup has to store into. `band_lo` is
                // the context's `Rsp` unless a guard page sat under it.
                let pairing = cratonvm_gc::gc_quiescence::peer_reg_pairing_enabled();
                // gcg g4/xt (`gcg-g3h-...-review-findings` item 3): the
                // stack-slot remap captures, recorded once `has_jit` is known
                // (`record_helper_stack_captures`); and item 5: the Linux
                // arm's `[xt-band]` / `[xt-word] pop=helper` census, which
                // this arm never printed. The peer is running again (the band
                // is a copy), so neither costs it anything.
                pending_captures.clear();
                let mut census_hits: Option<Vec<(usize, usize, usize)>> =
                    super::xt_census_on().then(Vec::new);
                let mut first_jit_at: Option<usize> = None;
                let mut tlab_cursor = super::TlabSlotCursor::new(&tlab_slots);
                for i in 0..snap.band_len / 8 {
                    // SAFETY: `band[..band_len]` was copied while the peer was
                    // frozen; reading the copy is unconditionally safe.
                    let w = unsafe { (band.as_ptr().add(i * 8) as *const usize).read_unaligned() };
                    if !has_jit && crate::jit::xt_root_scan::in_code_ranges(&ranges, w) {
                        has_jit = true;
                        first_jit_at = Some(snap.band_lo + i * 8);
                    }
                    // zt t26/zc5b item 1: a `Tlab` pointer word is never a reference.
                    if tlab_cursor.hit(snap.band_lo + i * 8) {
                        continue;
                    }
                    if let Some(o) = is_obj(w) {
                        // Pairing capture, helper-window stack side (0xff).
                        if pairing {
                            cratonvm_gc::gc_quiescence::record_peer_reg(tid, 0xff, w);
                        }
                        pending_captures.push((snap.band_lo + i * 8, w));
                        if let Some(hits) = census_hits.as_mut() {
                            if hits.len() < super::XT_CENSUS_WORDS_PER_PEER {
                                hits.push((snap.band_lo + i * 8, w, o.as_ptr() as usize));
                            }
                        }
                        candidates.push(o);
                    }
                }
                // gci i1/fr (g3h item 1): a published depth of zero demotes a
                // window to the pure-native path (opt-in; census either way).
                // gci i20/vh (opt-in): a deferred-chain peer's band too.
                let has_jit = super::helper_window_deferred_chain_step(
                    tid,
                    super::helper_window_after_depth_check(tid, has_jit, zero_depth_native),
                    taken.demotes_deferred_chain_window(tid),
                );
                super::record_helper_stack_captures(tid, has_jit, &pending_captures);
                if let Some(hits) = census_hits.filter(|_| has_jit) {
                    let jit_at = match first_jit_at {
                        Some(a) => format!("0x{a:x}"),
                        None => "reg".to_string(),
                    };
                    eprintln!(
                        "[xt-band] pop=helper tid={tid} rsp=0x{:x} words={} complete={} depth={:?} jit_at={jit_at}",
                        snap.band_lo,
                        snap.band_len / 8,
                        snap.complete,
                        cratonvm_gc::gc_quiescence::jit_depth_of_tid(tid),
                    );
                    for (at, w, root) in hits {
                        super::xt_census_word("helper", tid, "hw", at, w, root);
                    }
                }
                if super::blocked_peer_read(has_jit, snap.band_len / 8)
                    == super::BlockedPeerRead::Unreadable
                {
                    // gcd d10/t: an EMPTY band (`VirtualQuery` found no
                    // readable stack at `Rsp`) with no JIT address in the
                    // registers is an unknown peer, not a native one -- the
                    // same peer `Snapshot::Failed` above already counts. It
                    // was skipped as native here.
                    unknown_peers += 1;
                    XT_PEERS_UNCLASSIFIED.fetch_add(1, Ordering::Relaxed);
                    super::XT_HELPER_WINDOW_BANDLESS_PEERS.fetch_add(1, Ordering::Relaxed);
                    crate::jit::helper_window_refusal::note_unreadable_helper_peer(
                        "win",
                        tid,
                        "empty-band",
                    );
                    found_total += candidates.len();
                    roots.append(&mut candidates);
                    let before = roots.len();
                    if crate::jit::conservative_roots::xt_peer_shadow_scan_enabled() {
                        let _ = super::scan_peer_shadow_window(tid, is_obj, roots);
                    }
                    found_total += roots.len() - before;
                    continue;
                }
                if !has_jit {
                    // A pure-native stack: nothing this pass exists for.
                    continue;
                }
                windows += 1;
                // A JIT frame's oops live in the SHADOW STACK, which is not the
                // machine stack and so is invisible to everything above. Scan
                // it too, or the pin is incomplete and any coverage credited on
                // it is a lie. An untrusted window refuses the pin rather than
                // claiming coverage it does not have.
                let shadow_ok = if crate::jit::conservative_roots::xt_peer_shadow_scan_enabled() {
                    super::scan_peer_shadow_window(tid, is_obj, &mut candidates).is_some()
                } else {
                    true
                };
                found_total += candidates.len();
                // PIN, exactly as the Linux arm does -- but only a COMPLETE
                // band. `snapshot_peer` used to report an unreadable band as
                // `Some((ctx, 0))`, i.e. a zero-length window counted as whole,
                // so a peer suspended with `Rsp` in its guard page was pinned
                // (and its depth credited) on the strength of its registers
                // alone. A band cut at `MAX_STACK_SCAN` is incomplete the same
                // way.
                if super::helper_window_pin_enabled() && shadow_ok && snap.complete {
                    let addrs: Vec<usize> =
                        candidates.iter().map(|o| o.as_ptr() as usize).collect();
                    cratonvm_gc::gc_quiescence::add_xt_cycle_pinned_jit_roots(&addrs);
                    pinned_windows += 1;
                    // A pinned window covers the peer's WHOLE stack
                    // (`[rsp, stack_base)`) plus its register file, so every JIT
                    // frame it holds is immobile -- which is what the
                    // cross-thread coverage account wants to hear, and it wants
                    // to hear it as a DEPTH.
                    //
                    // Reading the peer's published depth after it has resumed is
                    // still exact: `mark_blocked_region_leave` waits out an
                    // active pause, so a blocked peer cannot run Java (and so
                    // cannot mutate its chain) between the block and the end of
                    // this STW.
                    //
                    // `None` -- a peer that never registered a slot -- poisons
                    // the ledger rather than crediting zero.
                    if crate::jit::conservative_roots::xt_pinned_peer_depth_enabled() {
                        cratonvm_gc::gc_quiescence::add_xt_cycle_pinned_jit_depth(
                            cratonvm_gc::gc_quiescence::jit_depth_of_tid(tid),
                        );
                    }
                    // gcd d3/m (`CRATONVM_GEN_PINNED_YOUNG_COPY_TAKEOVER`): the
                    // same whole band, every young word raw, into the young pin
                    // ledger, crediting the peer's depth there too, so the
                    // pinned in-place copy can pin what this window holds --
                    // plus the window's resolved candidates, which include its
                    // SHADOW-stack objects, so they stay put whatever the wake
                    // remap does.
                    if band_capture {
                        snapshot_young_words(
                            &snap.ctx,
                            &band[..snap.band_len],
                            &young_range,
                            &mut band_words,
                        );
                        super::collect_young_band_words(
                            &young_range,
                            candidates.iter().map(|o| o.as_ptr() as usize),
                            &mut band_words,
                        );
                        cratonvm_gc::gc_quiescence::note_peer_band_pin_words(
                            &band_words,
                            cratonvm_gc::gc_quiescence::jit_depth_of_tid(tid),
                            true,
                            true,
                        );
                    }
                } else {
                    unpinned_windows += 1;
                    // gci i3/tk: a refused window whose band was cut keeps
                    // the un-rewritable door (`mark_helper_window_refusal`).
                    if !snap.complete {
                        unpinned_incomplete += 1;
                    }
                    // gch h3/fd (d9c step 1): why, under `xt-refusal-census`.
                    crate::jit::helper_window_refusal::note_helper_window_refusal(
                        "win",
                        tid,
                        super::helper_window_pin_enabled(),
                        shadow_ok,
                        snap.complete,
                    );
                }
                taken.commit_window_roots(tid, &mut candidates, roots);
                if dbg() {
                    // gcd d5/s: why a window refuses (`shadow_ok`) and whether
                    // its depth can be credited (`depth=None` voids the whole
                    // pinned-depth credit), beside the band's completeness.
                    eprintln!(
                        "[xt-jit-roots] helper-window tid={tid}: JIT frames on native stack (Rip outside JIT), band={}B complete={} shadow_ok={shadow_ok} depth={:?}",
                        snap.band_len,
                        snap.complete,
                        cratonvm_gc::gc_quiescence::jit_depth_of_tid(tid),
                    );
                }
            }
        }
        // gcd d3/m: the blocked-monitor peers credited by proof (see
        // `credit_proven_blocked_monitor_peers`): not windows, so nothing
        // above read them, but the young pin ledger needs their bands -- their
        // register files and Rust frames hold young words nothing rewrites,
        // and their JIT depth must be accounted for. A peer that cannot be
        // read whole fails the ledger for this pause (the cycle then diverts,
        // as it did before the proof).
        for &(tid, depth) in &ledger_only {
            if tid == 0 || tid == self_tid || taken.contains(tid) {
                cratonvm_gc::gc_quiescence::mark_peer_band_capture_incomplete();
                continue;
            }
            match unsafe { snapshot_peer(tid, &mut band) } {
                Snapshot::Ok(s) if s.complete => {
                    snapshot_young_words(
                        &s.ctx,
                        &band[..s.band_len],
                        &young_range,
                        &mut band_words,
                    );
                    cratonvm_gc::gc_quiescence::note_peer_band_pin_words(
                        &band_words,
                        Some(depth),
                        true,
                        false,
                    );
                }
                _ => cratonvm_gc::gc_quiescence::mark_peer_band_capture_incomplete(),
            }
        }
        // An unreadable blocked peer counts against the discharge exactly like
        // an unpinned window: it is a stack nobody covered.
        let refusing = unpinned_windows + unknown_peers;
        XT_HELPER_WINDOWS_SCANNED.fetch_add(windows as u64, Ordering::Relaxed);
        super::XT_HELPER_WINDOWS_PINNED.fetch_add(pinned_windows as u64, Ordering::Relaxed);
        super::XT_HELPER_WINDOWS_REFUSED.fetch_add(refusing as u64, Ordering::Relaxed);
        // Into THIS pause's ledger (gcd d2/i), not a process word.
        cratonvm_gc::gc_quiescence::set_xt_helper_windows_unpinned(refusing as u64);
        if unknown_peers > 0 {
            XT_CYCLES_WITH_UNCLASSIFIED.fetch_add(1, Ordering::Relaxed);
        }
        // Helper-window roots are a blocked peer's register file + raw stack:
        // conservative and un-rewritable, so without a pin this collection must
        // not relocate. WITH one -- and with the interior-resolving probe the
        // discharge implies, so a derived pointer resolves to the base that
        // must stay still -- the objects the peer can reach are held in place
        // and the rest of the heap may move. See the Linux arm for the full
        // argument and for why the pins alone are not sufficient without the
        // second site in `interpreter::gc_and_alloc` agreeing.
        if (windows > 0 || unknown_peers > 0)
            && !(super::helper_window_discharge_enabled() && refusing == 0)
        {
            // gci i3/tk: the pinned door when every refused band was read
            // whole and no peer was unreadable, under the pause's licence.
            super::mark_helper_window_refusal(taken, unpinned_incomplete, unknown_peers);
        }
        // Parity with the Linux arm: the sweep's per-cycle coverage record.
        cratonvm_gc::gc_quiescence::publish_xt_helper_window(windows as u64, found_total as u64);
        if dbg() {
            eprintln!(
                "[xt-jit-roots] helper-window pass: {windows} window(s), {found_total} conservative root(s), {unknown_peers} unreadable peer(s)"
            );
        }
        (windows, found_total)
    }

    /// gcd d3/m: every young word of a snapshot's integer register file and
    /// of its copied band, raw, into `out` (cleared first). See
    /// `super::collect_young_band_words`.
    fn snapshot_young_words(
        ctx: &[u8; CTX_SIZE],
        band: &[u8],
        range: &cratonvm_gc::gc_quiescence::YoungPinRange,
        out: &mut Vec<usize>,
    ) {
        out.clear();
        let regs = (OFF_GPR_LO..=OFF_GPR_HI).step_by(8).map(|off| {
            let mut w = [0u8; 8];
            w.copy_from_slice(&ctx[off..off + 8]);
            u64::from_ne_bytes(w) as usize
        });
        super::collect_young_band_words(range, regs, out);
        // gcj-j1pn-pinned-cycle-review-findings item 1: the register file's
        // words, tagged for the pinned plan (every caller notes the band).
        cratonvm_gc::gc_quiescence::note_peer_band_register_words(out);
        let words = band.chunks_exact(8).map(|c| {
            let mut w = [0u8; 8];
            w.copy_from_slice(c);
            u64::from_ne_bytes(w) as usize
        });
        super::collect_young_band_words(range, words, out);
    }

    /// What [`snapshot_peer`] copied out of a suspended blocked peer.
    struct PeerSnapshot {
        ctx: [u8; CTX_SIZE],
        /// The peer's address of `band[0]`: its `Rsp`, or the base of the
        /// stack proper when `Rsp` sat in the guard page below it.
        band_lo: usize,
        band_len: usize,
        /// The band is the WHOLE readable stack above `Rsp`. Only a complete
        /// band may be pinned in place of a refusal.
        complete: bool,
    }

    enum Snapshot {
        /// `OpenThread` failed: the thread has exited. Not a coverage hole.
        Gone,
        /// Opened but not readable (suspend / context failure, a corrupt
        /// `Rsp`, or four failed attempts to size the copy buffer).
        Failed,
        Ok(PeerSnapshot),
    }

    /// Suspend `tid` just long enough to read its thread context and copy its
    /// readable stack band into `band` (growing `band` only while the peer is
    /// RUNNING — never allocate while it is frozen; it may be suspended
    /// mid-`malloc` inside a Rust helper and re-entering the allocator would
    /// deadlock).
    unsafe fn snapshot_peer(tid: u32, band: &mut Vec<u8>) -> Snapshot {
        band.clear();
        let h = open_own_thread(tid);
        if h == 0 {
            return Snapshot::Gone;
        }
        #[repr(C, align(16))]
        struct Ctx([u8; CTX_SIZE]);
        let mut ctx = Ctx([0u8; CTX_SIZE]);
        // Up to 4 attempts: each failed attempt resumes the peer, grows the
        // buffer to the band size just observed, and retries (the peer's rsp
        // may move between attempts, so the bounds are recomputed each time).
        for _ in 0..4 {
            if SuspendThread(h) == u32::MAX {
                CloseHandle(h);
                return Snapshot::Failed;
            }
            ctx.0 = [0u8; CTX_SIZE];
            *(ctx.0.as_mut_ptr().add(OFF_FLAGS) as *mut u32) = CONTEXT_CONTROL_INTEGER;
            if GetThreadContext(h, ctx.0.as_mut_ptr()) == 0 {
                ResumeThread(h);
                CloseHandle(h);
                return Snapshot::Failed;
            }
            let rsp = *(ctx.0.as_ptr().add(OFF_RSP) as *const u64) as usize;
            if rsp == 0 || rsp & 0x7 != 0 {
                ResumeThread(h);
                CloseHandle(h);
                return Snapshot::Failed;
            }
            // VirtualQuery is a pure syscall (no user-mode lock) — safe while
            // the peer is frozen.
            let span = readable_stack_span(rsp, MAX_STACK_SCAN);
            let len = span.hi.saturating_sub(span.lo);
            if len <= band.capacity() {
                if len > 0 {
                    // SAFETY: [lo, hi) is committed+readable and the peer is
                    // frozen, so the copy reads stable memory; the destination
                    // capacity was checked above.
                    core::ptr::copy_nonoverlapping(span.lo as *const u8, band.as_mut_ptr(), len);
                }
                ResumeThread(h);
                CloseHandle(h);
                // SAFETY: `len` bytes were just initialized above.
                band.set_len(len);
                return Snapshot::Ok(PeerSnapshot {
                    ctx: ctx.0,
                    band_lo: span.lo,
                    band_len: len,
                    // An empty band (`VirtualQuery` failed, or `Rsp` in memory
                    // that is not this stack) is NOT complete, whatever the
                    // registers say.
                    complete: span.complete && len > 0,
                });
            }
            // Buffer too small: resume the peer FIRST, then grow.
            ResumeThread(h);
            band.reserve(len);
        }
        CloseHandle(h);
        Snapshot::Failed
    }

    /// Resume + close every taken-over peer. Called after the collection has
    /// completed and the heap is consistent again.
    pub fn resume(mut taken: TakenOver) {
        resume_held(&mut taken);
    }

    /// [`resume`]'s body, once per take-over (also `TakenOver`'s `Drop`).
    pub(super) fn resume_held(taken: &mut TakenOver) {
        if std::mem::replace(&mut taken.resumed, true) {
            return;
        }
        // JIT round 16 wave 14 (lane ffm3): the peers claimed in C hold no
        // handle; they are released through their state word.
        super::release_native_claims(taken);
        for &h in &taken.handles {
            unsafe {
                ResumeThread(h);
                CloseHandle(h);
            }
        }
        if dbg() && !taken.handles.is_empty() {
            eprintln!(
                "[xt-jit-roots] resumed {} taken-over peer(s)",
                taken.handles.len()
            );
        }
    }

    #[cfg(test)]
    mod stack_span_tests {
        use super::*;

        /// This thread's own stack, from a live local up: complete, starting
        /// exactly at the probe, and ending at the stack's high limit.
        #[test]
        fn span_of_a_live_stack_is_complete_and_reaches_the_stack_top() {
            #[link(name = "kernel32")]
            extern "system" {
                fn GetCurrentThreadStackLimits(low_limit: *mut usize, high_limit: *mut usize);
            }
            let probe: u64 = 0x5eed;
            let rsp = (&probe as *const u64 as usize) & !7;
            let s = unsafe { readable_stack_span(rsp, MAX_TAKEOVER_STACK_SCAN) };
            assert!(s.complete, "a live stack band must be reported whole");
            assert_eq!(s.lo, rsp);
            let (mut low, mut high) = (0usize, 0usize);
            unsafe { GetCurrentThreadStackLimits(&mut low, &mut high) };
            assert_eq!(
                s.hi, high,
                "the band must end at the stack's own top, not at a region boundary"
            );
            assert_eq!(std::hint::black_box(probe), 0x5eed);
        }

        /// A cap below the band reports INCOMPLETE -- the shape a helper window
        /// deeper than `MAX_STACK_SCAN` has, which must not be pinned.
        #[test]
        fn span_cut_by_its_cap_is_incomplete() {
            let probe: u64 = 1;
            let rsp = (&probe as *const u64 as usize) & !7;
            let s = unsafe { readable_stack_span(rsp, 8) };
            assert!(!s.complete);
            assert_eq!(s.lo, rsp);
            assert_eq!(s.hi, rsp + 8);
            assert_eq!(std::hint::black_box(probe), 1);
        }

        /// `Rsp` in the guard page below the committed stack: the band starts at
        /// the stack proper and is complete. The old `committed_region_end`
        /// answered "nothing readable" here and a frozen peer's whole machine
        /// stack went unscanned.
        #[test]
        fn span_from_the_guard_page_steps_over_it() {
            let probe: u64 = 2;
            let rsp = (&probe as *const u64 as usize) & !7;
            let committed = unsafe { query(rsp) }.expect("VirtualQuery of a live local");
            let committed_lo = committed.base_address as usize;
            let below = committed_lo - 8;
            let Some(guard) = (unsafe { query(below) }) else {
                return;
            };
            if guard.protect & PAGE_GUARD == 0 {
                // Not the shape under test on this thread (no guard page right
                // below the committed stack); nothing to assert.
                return;
            }
            let s = unsafe { readable_stack_span(below, MAX_TAKEOVER_STACK_SCAN) };
            assert!(s.complete);
            assert_eq!(s.lo, committed_lo, "the guard page itself is skipped");
            assert!(s.hi > rsp);
            assert_eq!(std::hint::black_box(probe), 2);
        }
    }
}

#[cfg(windows)]
use imp::resume_held;
#[cfg(windows)]
pub use imp::{helper_window_pass, resume, take_over_pass, take_over_pass_split};

// ---------------------------------------------------------------------------
// Linux x86-64 implementation (signal rendezvous)
// ---------------------------------------------------------------------------

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
mod imp {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, AtomicUsize, Ordering};
    use std::sync::Once;
    use std::time::{Duration, Instant};

    const TAKEOVER_SIGNAL: i32 = libc::SIGUSR2;
    const MAX_SLOTS: usize = 512;
    const MAX_RANGES: usize = 65_536;
    const MAX_STACK_SCAN: usize = 8 * 1024 * 1024;
    const REG_COUNT: usize = 17;

    thread_local! {
        /// g1ep4: [`helper_window_pass`]'s copy band, kept on the thread
        /// that ran the pass for its next one ([`take_pass_band`]). A fresh
        /// band is a fresh 8 MiB mapping, and every page a peer's copy writes
        /// is a page fault the kernel zeroes first: 109-208 k faults (0.4-0.8
        /// GB) on `G1epKeepRounds 10 -Xmx1g`'s 94 pauses, inside the pause. A
        /// kept band holds the pages its deepest copy touched (a few MB).
        static PASS_BAND: std::cell::RefCell<Vec<u64>> =
            const { std::cell::RefCell::new(Vec::new()) };
    }

    /// g1ep4: the band [`helper_window_pass`] copies peers' stacks into:
    /// this thread's kept one, else a new one. Its contents are stale; every
    /// reader reads only the words a copy wrote (`band_words`), as the band
    /// was already shared by every peer of one pass.
    fn take_pass_band() -> Vec<u64> {
        let kept = PASS_BAND
            .try_with(|b| {
                let taken = b.try_borrow_mut().map(|mut b| std::mem::take(&mut *b));
                taken.unwrap_or_default()
            })
            .unwrap_or_default();
        if kept.len() == MAX_STACK_SCAN / 8 {
            kept
        } else {
            vec![0u64; MAX_STACK_SCAN / 8]
        }
    }

    /// g1ep4: keep `band` for this thread's next pass.
    fn give_pass_band(band: Vec<u64>) {
        let _ = PASS_BAND.try_with(|b| {
            if let Ok(mut b) = b.try_borrow_mut() {
                *b = band;
            }
        });
    }

    use super::{
        peer_deadline_ms, peer_total_deadline_ms, XT_CYCLES_WITH_UNCLASSIFIED,
        XT_PEERS_CLASSIFIED_AFTER_RETRY, XT_PEERS_UNCLASSIFIED, XT_PEER_RESIGNALS,
    };

    const STATE_EMPTY: u8 = 0;
    const STATE_ARMED: u8 = 1;
    const STATE_PARKED: u8 = 2;
    const STATE_NOT_JIT: u8 = 3;
    const STATE_DONE: u8 = 4;
    const STATE_CANCELLED: u8 = 5;

    /// How many `spin_loop` turns a peer parked in `takeover_signal_handler`
    /// makes before it sleeps on its slot's `resume` word (gce e1/t, e2/t).
    /// A few microseconds: a pass that scans and releases at once never
    /// sleeps.
    const PARKED_SPINS_BEFORE_YIELD: u32 = 1 << 12;

    /// The longest one `FUTEX_WAIT` of a parked peer lasts (1 ms), so an exit
    /// condition that no `release_resume` wakes for is seen within it.
    const PARKED_WAIT_SLICE_NS: i64 = 1_000_000;

    /// `FUTEX_WAIT` on `word` while it reads 0, for at most
    /// [`PARKED_WAIT_SLICE_NS`]. Called from `takeover_signal_handler`, so it
    /// is a raw syscall with no lock and no allocation, and it puts back the
    /// interrupted code's `errno` (the wait's own `EAGAIN` / `ETIMEDOUT` /
    /// `EINTR` land there).
    ///
    /// # Safety
    /// `word` is a live `AtomicU32` (a `SLOTS` entry is `'static`).
    unsafe fn futex_wait_while_zero(word: &AtomicU32) {
        let errno = libc::__errno_location();
        let saved = *errno;
        let slice = libc::timespec {
            tv_sec: 0,
            tv_nsec: PARKED_WAIT_SLICE_NS,
        };
        libc::syscall(
            libc::SYS_futex,
            word.as_ptr(),
            libc::FUTEX_WAIT | libc::FUTEX_PRIVATE_FLAG,
            0u32,
            &slice as *const libc::timespec,
        );
        *errno = saved;
    }

    /// Wake every thread `FUTEX_WAIT`ing on `word` (at most the one peer
    /// parked on that slot). Cheap when nobody waits.
    fn futex_wake_all(word: &AtomicU32) {
        // SAFETY: a futex wake on a live, aligned 32-bit word of this process;
        // it reads nothing and writes nothing of ours.
        unsafe {
            libc::syscall(
                libc::SYS_futex,
                word.as_ptr(),
                libc::FUTEX_WAKE | libc::FUTEX_PRIVATE_FLAG,
                i32::MAX,
            );
        }
    }

    /// `CRATONVM_XT_PARKED_SPIN_ONLY=1` (gce e2/t, opt-in, default OFF): the
    /// kill switch of the parked-peer sleep (gce e1/t, e2/t) -- a parked peer
    /// spins for the whole pause again. Read when a slot is armed (never in
    /// the handler), so one binary can A/B the sleep's CPU and
    /// time-to-safepoint cost.
    fn parked_spin_only() -> bool {
        cratonvm_types::flags::runtime_flag_on("CRATONVM_XT_PARKED_SPIN_ONLY")
    }

    struct LinuxSlot {
        tid: AtomicU32,
        state: AtomicU8,
        /// Non-zero releases the parked peer. 32 bits since gce e2/t, so the
        /// parked peer can `FUTEX_WAIT` on it; every store of 1 goes through
        /// [`LinuxSlot::release_resume`], which wakes it.
        resume: AtomicU32,
        /// Armed by the helper-window pass: the handler parks this peer
        /// wherever the signal found it, not only in compiled code. Per slot
        /// since gc-common w9-g; it was one process-wide `HELPER_MODE`, so a
        /// second VM's helper-window pass made THIS VM's take-over park a
        /// peer interrupted in Rust code (in `malloc`, holding a lock) and
        /// then scan it with an allocating walk.
        helper: AtomicBool,
        /// gce e2/t: `CRATONVM_XT_PARKED_SPIN_ONLY=1` when this slot was
        /// armed -- the parked peer spins for the whole pause, as before
        /// gce e1/t, instead of sleeping on its slot. Per slot because the
        /// handler cannot read a flag: the A/B of the sleep in one binary.
        spin_only: AtomicBool,
        rip: AtomicUsize,
        rsp: AtomicUsize,
        regs: [AtomicUsize; REG_COUNT],
    }

    impl LinuxSlot {
        const fn new() -> Self {
            Self {
                tid: AtomicU32::new(0),
                state: AtomicU8::new(STATE_EMPTY),
                resume: AtomicU32::new(0),
                helper: AtomicBool::new(false),
                spin_only: AtomicBool::new(false),
                rip: AtomicUsize::new(0),
                rsp: AtomicUsize::new(0),
                regs: [const { AtomicUsize::new(0) }; REG_COUNT],
            }
        }

        fn arm(&self, tid: u32, helper: bool, spin_only: bool) {
            self.tid.store(tid, Ordering::Release);
            self.helper.store(helper, Ordering::Release);
            self.spin_only.store(spin_only, Ordering::Release);
            self.resume.store(0, Ordering::Release);
            self.rip.store(0, Ordering::Release);
            self.rsp.store(0, Ordering::Release);
            for reg in &self.regs {
                reg.store(0, Ordering::Release);
            }
            self.state.store(STATE_ARMED, Ordering::Release);
        }

        /// Release a peer parked on this slot: `resume = 1`, then wake its
        /// `FUTEX_WAIT` (a no-op syscall when nobody waits).
        fn release_resume(&self) {
            self.resume.store(1, Ordering::Release);
            futex_wake_all(&self.resume);
        }

        fn clear(&self) {
            self.release_resume();
            self.rip.store(0, Ordering::Release);
            self.rsp.store(0, Ordering::Release);
            self.tid.store(0, Ordering::Release);
            self.state.store(STATE_EMPTY, Ordering::Release);
        }
    }

    struct AtomicRange {
        lo: AtomicUsize,
        hi: AtomicUsize,
    }

    impl AtomicRange {
        const fn new() -> Self {
            Self {
                lo: AtomicUsize::new(0),
                hi: AtomicUsize::new(0),
            }
        }
    }

    static INSTALL: Once = Once::new();
    /// How many parties need the handler live: one per pass in progress
    /// ([`PassSession`]) and one per take-over still holding parked peers
    /// (`TakenOver::holds_session`, released by [`resume`]). The handler
    /// answers only while it is non-zero, and a parked peer's spin treats a
    /// zero as a release.
    ///
    /// gc-common w9-g: this was one `ACTIVE: AtomicBool`, cleared by whichever
    /// pass or `resume` finished last. The signal machinery is per process and
    /// the VMs are not: a second VM's take-over that froze nothing (or its
    /// `resume`, or its helper-window pass) stored `false` while THIS VM's
    /// peers were parked, and they left the handler and ran compiled code
    /// through a collection that had scanned them as frozen -- the non-moving
    /// sweep freeing what their registers named, or G1 evacuating it.
    static ACTIVE_SESSIONS: AtomicUsize = AtomicUsize::new(0);
    /// Serialises the passes of different VMs in this process (gc-common
    /// w9-g): `RANGES` is one table, and two passes publishing different
    /// snapshots into it at once could tear an entry and make the handler
    /// park a peer that is not in compiled code. Held for one pass only, never
    /// across the frozen interval, and never by the handler; a parked peer
    /// holds no Rust lock, so no pass can wait on it.
    static PASS_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    static SLOTS: [LinuxSlot; MAX_SLOTS] = [const { LinuxSlot::new() }; MAX_SLOTS];
    static RANGES: [AtomicRange; MAX_RANGES] = [const { AtomicRange::new() }; MAX_RANGES];
    static RANGES_LEN: AtomicUsize = AtomicUsize::new(0);

    /// One pass in progress: [`PASS_LOCK`] held and one [`ACTIVE_SESSIONS`]
    /// count, both released on drop (the count first).
    struct PassSession {
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl PassSession {
        fn open() -> Self {
            let lock = PASS_LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            ACTIVE_SESSIONS.fetch_add(1, Ordering::AcqRel);
            PassSession { _lock: lock }
        }
    }

    impl Drop for PassSession {
        fn drop(&mut self) {
            ACTIVE_SESSIONS.fetch_sub(1, Ordering::AcqRel);
        }
    }

    #[inline]
    fn gettid() -> u32 {
        unsafe { libc::syscall(libc::SYS_gettid) as u32 }
    }

    #[inline]
    fn publish_ranges(ranges: &[(usize, usize)]) {
        let n = ranges.len().min(MAX_RANGES);
        for (i, &(lo, hi)) in ranges.iter().take(n).enumerate() {
            RANGES[i].lo.store(lo, Ordering::Release);
            RANGES[i].hi.store(hi, Ordering::Release);
        }
        RANGES_LEN.store(n, Ordering::Release);
        if dbg() && ranges.len() > MAX_RANGES {
            eprintln!(
                "[xt-jit-roots] linux range table capped: {} -> {} ranges",
                ranges.len(),
                MAX_RANGES
            );
        }
    }

    #[inline]
    fn find_slot(tid: u32) -> Option<&'static LinuxSlot> {
        SLOTS.iter().find(|slot| {
            slot.tid.load(Ordering::Acquire) == tid
                && slot.state.load(Ordering::Acquire) != STATE_EMPTY
        })
    }

    fn arm_slot(tid: u32, helper: bool) -> Option<&'static LinuxSlot> {
        let spin_only = parked_spin_only();
        for slot in &SLOTS {
            if slot.state.load(Ordering::Acquire) == STATE_EMPTY {
                slot.arm(tid, helper, spin_only);
                return Some(slot);
            }
        }
        None
    }

    unsafe extern "C" fn takeover_signal_handler(
        _sig: i32,
        _info: *mut libc::siginfo_t,
        ucontext: *mut libc::c_void,
    ) {
        if ACTIVE_SESSIONS.load(Ordering::Acquire) == 0 || ucontext.is_null() {
            return;
        }
        let tid = gettid();
        let Some(slot) = find_slot(tid) else {
            return;
        };
        if slot.state.load(Ordering::Acquire) != STATE_ARMED {
            return;
        }

        let uc = &*(ucontext as *const libc::ucontext_t);
        // General-purpose registers only; see CONTEXT_CONTROL_INTEGER in the
        // Windows arm for why no JIT reference can be in `fpregs`.
        let g = &uc.uc_mcontext.gregs;
        let read_reg = |idx: i32| -> usize { g[idx as usize] as usize };
        let rip = read_reg(libc::REG_RIP);
        let rsp = read_reg(libc::REG_RSP);

        let mut in_jit = false;
        let len = RANGES_LEN.load(Ordering::Acquire);
        let mut i = 0usize;
        while i < len {
            let lo = RANGES[i].lo.load(Ordering::Acquire);
            let hi = RANGES[i].hi.load(Ordering::Acquire);
            if rip >= lo && rip < hi {
                in_jit = true;
                break;
            }
            i += 1;
        }
        if !in_jit && !slot.helper.load(Ordering::Acquire) {
            slot.state.store(STATE_NOT_JIT, Ordering::Release);
            return;
        }

        let regs = [
            libc::REG_R8,
            libc::REG_R9,
            libc::REG_R10,
            libc::REG_R11,
            libc::REG_R12,
            libc::REG_R13,
            libc::REG_R14,
            libc::REG_R15,
            libc::REG_RDI,
            libc::REG_RSI,
            libc::REG_RBP,
            libc::REG_RBX,
            libc::REG_RDX,
            libc::REG_RAX,
            libc::REG_RCX,
            libc::REG_RSP,
            libc::REG_RIP,
        ];
        slot.rip.store(rip, Ordering::Release);
        slot.rsp.store(rsp, Ordering::Release);
        for (i, reg) in regs.iter().enumerate() {
            slot.regs[i].store(read_reg(*reg), Ordering::Release);
        }
        // SLOT OWNERSHIP (gc-common w1-c, 2026-09-23). Both transitions this
        // handler makes are CASes against the state it expects, and the spin
        // also watches the slot's `tid`. They used to be plain stores, and a
        // slot is RECYCLED: `release_slot` gives a late peer 100 ms to leave
        // and then `clear()`s the slot, and the helper-window pass re-arms the
        // first EMPTY slot for the NEXT blocked tid straight away. A peer that
        // was not scheduled inside those 100 ms then
        //
        //   * found `resume` reset to 0 by the re-arm and kept spinning on a
        //     slot that now described someone else, and
        //   * on leaving, stored `STATE_DONE` over whatever the slot said --
        //     turning the next peer's `PARKED` into a `DONE` the collector
        //     reads as a non-JIT answer (its band never scanned, and not
        //     counted as unclassified), or a cleared slot into one that is no
        //     longer `EMPTY` and so is never armed again (a slot leak toward
        //     the 512-slot "no free slot" refusal).
        //
        // The same shape at the front: a peer that passed the `ARMED` check
        // just before the collector's deadline cancelled the slot stored
        // `PARKED` over `CANCELLED`/`EMPTY`, parking on a slot nobody would
        // release except through `ACTIVE` (now `ACTIVE_SESSIONS`).
        if slot
            .state
            .compare_exchange(
                STATE_ARMED,
                STATE_PARKED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            return;
        }

        // gce e1/t + e2/t: spin briefly, then SLEEP on the slot's `resume`
        // word. A parked peer waits out the rest of the pass and the whole
        // collection; a pure spin kept each frozen peer at 100 % of a core for
        // all of it -- cores the collector's workers and the NEXT peer this
        // serial pass signals (which must be scheduled to answer) compete for.
        // The Windows arm's peers are OS-suspended and use none. e1/t yielded
        // (`sched_yield`), which frees the core only to a thread that wants it
        // and still burns it otherwise; e2/t waits on a futex instead:
        // `release_resume` wakes it, and each wait is bounded
        // (`PARKED_WAIT_SLICE_NS`), so an exit condition nobody wakes for
        // (`ACTIVE_SESSIONS` falling to 0, the slot re-armed for another tid)
        // is still seen within a slice. A raw syscall: no lock, no
        // allocation; `futex_wait_while_zero` restores errno.
        // `CRATONVM_XT_PARKED_SPIN_ONLY=1` (read when the slot was armed)
        // keeps the pure spin, for the A/B.
        let spin_only = slot.spin_only.load(Ordering::Acquire);
        let mut spins = 0u32;
        while slot.resume.load(Ordering::Acquire) == 0
            && ACTIVE_SESSIONS.load(Ordering::Acquire) > 0
            && slot.tid.load(Ordering::Acquire) == tid
            && slot.state.load(Ordering::Acquire) == STATE_PARKED
        {
            if spin_only || spins < PARKED_SPINS_BEFORE_YIELD {
                spins = spins.saturating_add(1);
                core::hint::spin_loop();
            } else {
                futex_wait_while_zero(&slot.resume);
            }
        }
        if slot.tid.load(Ordering::Acquire) == tid {
            let _ = slot.state.compare_exchange(
                STATE_PARKED,
                STATE_DONE,
                Ordering::AcqRel,
                Ordering::Acquire,
            );
        }
    }

    fn install_handler() {
        INSTALL.call_once(|| unsafe {
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_flags = libc::SA_SIGINFO | libc::SA_RESTART;
            sa.sa_sigaction = takeover_signal_handler as usize;
            libc::sigemptyset(&mut sa.sa_mask);
            let rc = libc::sigaction(TAKEOVER_SIGNAL, &sa, std::ptr::null_mut());
            if rc != 0 {
                eprintln!(
                    "[xt-jit-roots] failed to install linux takeover signal handler: errno={}",
                    *libc::__errno_location()
                );
            }
        });
    }

    fn list_thread_tids() -> Vec<u32> {
        let mut tids = Vec::new();
        let Ok(entries) = std::fs::read_dir("/proc/self/task") else {
            return tids;
        };
        for entry in entries.flatten() {
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if let Ok(tid) = name.parse::<u32>() {
                tids.push(tid);
            }
        }
        tids
    }

    fn send_takeover_signal(tid: u32) -> bool {
        let rc = unsafe {
            libc::syscall(
                libc::SYS_tgkill,
                libc::getpid(),
                tid as libc::pid_t,
                TAKEOVER_SIGNAL,
            )
        };
        rc == 0
    }

    /// JIT round 17 wave 6 (lane eintr2): see
    /// `super::unblock_takeover_signal_on_attach`. Installs the handler,
    /// then removes the take-over signal from the CALLING thread's mask.
    pub(super) fn unblock_takeover_signal_for_current_thread() {
        install_handler();
        // SAFETY: a zeroed `sigset_t` is a valid argument to `sigemptyset`;
        // `pthread_sigmask` changes only the calling thread's mask.
        unsafe {
            let mut set: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut set);
            libc::sigaddset(&mut set, TAKEOVER_SIGNAL);
            libc::pthread_sigmask(libc::SIG_UNBLOCK, &set, std::ptr::null_mut());
        }
    }

    /// JIT round 17 wave 1 (lane eintr): read `/proc/self/task/<tid>/syscall`
    /// for a peer of this process (`super::PeerSyscall`). One `open`, one
    /// `read` into a stack buffer, one `close`; no allocation. The kernel lets
    /// a thread read its own thread group's file whatever the ptrace policy
    /// (`__ptrace_may_access` answers same-thread-group first); a process that
    /// made itself non-dumpable, a sandbox without the file, or any error reads
    /// `Unknown`, and the peer is signalled as before -- unless the `stat`
    /// fallback answers (JIT round 17 wave 6, lane eintr2,
    /// `super::takeover_stat_fallback_enabled`, default ON): an unreadable or
    /// unparsable `syscall` asks `/proc/self/task/<tid>/stat`, and its `S`
    /// reads `Sleeping`. The switch is read only on that path.
    fn peer_syscall_state(tid: u32) -> super::PeerSyscall {
        let mut buf = [0u8; 256];
        let state = match read_own_task_file(tid, b"/syscall\0", &mut buf) {
            Some(n) => super::parse_proc_syscall(&buf[..n]),
            None => super::PeerSyscall::Unknown,
        };
        if state != super::PeerSyscall::Unknown || !super::takeover_stat_fallback_enabled() {
            return state;
        }
        // `stat` is one line of ~300 bytes; the state letter sits right
        // after the command name, so a short read still holds it.
        let mut stat = [0u8; 512];
        match read_own_task_file(tid, b"/stat\0", &mut stat) {
            Some(n) => {
                super::peer_syscall_from_stat_state(super::parse_proc_stat_state(&stat[..n]))
            }
            None => super::PeerSyscall::Unknown,
        }
    }

    /// JIT round 17 wave 2 (lane ffm3, EINTR17-1): does the peer block the
    /// take-over signal (`SigBlk` of `/proc/self/task/<tid>/status`)? A
    /// counted FFM downcall blocks it for its C code under
    /// `CRATONVM_FFM_DOWNCALL_MASK_TAKEOVER_SIGNAL`
    /// (`super::takeover_mask_door_enabled`). `None` when unreadable. One
    /// `open`, at most a few `read`s into a stack buffer, one `close`.
    fn peer_blocks_takeover_signal(tid: u32) -> Option<bool> {
        // `status` is ~1.5 KiB; `SigBlk` sits in its first half.
        let mut buf = [0u8; 4096];
        let n = read_own_task_file(tid, b"/status\0", &mut buf)?;
        super::parse_status_sigblk(&buf[..n], TAKEOVER_SIGNAL)
    }

    /// Read `/proc/self/task/<tid><name>` (`name` starts with `/` and ends
    /// in NUL) into `buf`, until EOF or `buf` is full; the bytes read, or
    /// `None` on any error or an empty file. No allocation.
    fn read_own_task_file(tid: u32, name: &[u8], buf: &mut [u8]) -> Option<usize> {
        const PREFIX: &[u8] = b"/proc/self/task/";
        let mut path = [0u8; 48];
        if PREFIX.len() + 10 + name.len() > path.len() || name.last() != Some(&0) {
            return None;
        }
        let mut digits = [0u8; 10];
        let mut nd = 0usize;
        let mut t = tid;
        loop {
            digits[nd] = b'0' + (t % 10) as u8;
            nd += 1;
            t /= 10;
            if t == 0 {
                break;
            }
        }
        path[..PREFIX.len()].copy_from_slice(PREFIX);
        let mut at = PREFIX.len();
        for &d in digits[..nd].iter().rev() {
            path[at] = d;
            at += 1;
        }
        path[at..at + name.len()].copy_from_slice(name);
        // SAFETY: `path` is NUL-terminated (`name` ends in 0); every `read`
        // writes inside `buf`'s unread tail; the fd is closed on every path.
        unsafe {
            let fd = libc::open(
                path.as_ptr() as *const libc::c_char,
                libc::O_RDONLY | libc::O_CLOEXEC,
            );
            if fd < 0 {
                return None;
            }
            let mut total = 0usize;
            while total < buf.len() {
                let n = libc::read(
                    fd,
                    buf[total..].as_mut_ptr() as *mut libc::c_void,
                    buf.len() - total,
                );
                if n <= 0 {
                    break;
                }
                let asked = buf.len() - total;
                total += n as usize;
                // A procfs file comes whole in one read when it fits; a short
                // read is its end (no second `read` just to see EOF).
                if (n as usize) < asked {
                    break;
                }
            }
            libc::close(fd);
            (total > 0).then_some(total)
        }
    }

    /// JIT round 18 wave 10 (lane ffmeintr): see `super::debug_peer_os_state`.
    /// Allocates (the answer is a `String`); called only by the watchdog dump,
    /// between take-over rounds, never while a helper slot holds a peer.
    pub(super) fn debug_peer_os_state(tid: u32) -> String {
        let mut stat = [0u8; 512];
        let kstate = read_own_task_file(tid, b"/stat\0", &mut stat)
            .and_then(|n| super::parse_proc_stat_state(&stat[..n]))
            .map(|c| c as char)
            .unwrap_or('?');
        let mut sc = [0u8; 256];
        let syscall = read_own_task_file(tid, b"/syscall\0", &mut sc)
            .map(|n| String::from_utf8_lossy(&sc[..n]).trim().to_string())
            .unwrap_or_else(|| "?".to_string());
        let mut wc = [0u8; 128];
        let wchan = read_own_task_file(tid, b"/wchan\0", &mut wc)
            .map(|n| String::from_utf8_lossy(&wc[..n]).trim().to_string())
            .unwrap_or_else(|| "?".to_string());
        let sig_blocked = peer_blocks_takeover_signal(tid);
        let slot = SLOTS
            .iter()
            .find(|s| s.tid.load(Ordering::Acquire) == tid)
            .map(|s| {
                (
                    s.state.load(Ordering::Acquire),
                    s.helper.load(Ordering::Acquire),
                    s.resume.load(Ordering::Acquire),
                )
            });
        format!(
            "kstate={kstate} syscall=\"{syscall}\" wchan={wchan} takeover_sig_blocked={sig_blocked:?} \
             slot(state,helper,resume)={slot:?} active_sessions={}",
            ACTIVE_SESSIONS.load(Ordering::Acquire)
        )
    }

    /// Poll for at most `timeout`, WITHOUT cancelling when it expires.
    ///
    /// Split out of `wait_for_response` so an expired attempt can be retried:
    /// cancelling is a decision about the whole budget, not about one attempt,
    /// and `STATE_CANCELLED` is the state the handler reads to bail out.
    fn poll_for_response(slot: &LinuxSlot, timeout: Duration) -> Option<u8> {
        let start = Instant::now();
        loop {
            let state = slot.state.load(Ordering::Acquire);
            if state != STATE_ARMED {
                return Some(state);
            }
            if start.elapsed() >= timeout {
                return None;
            }
            std::thread::yield_now();
        }
    }

    /// H2-CID0 (2026-08-05) — wait for a peer across re-signals.
    ///
    /// Replaces a single [`peer_deadline_ms`] bet. Missing that deadline means
    /// the peer was not scheduled inside the window; because the takeover is
    /// signal-based rather than poll-based, that is a statement about the
    /// scheduler and not about whether the peer can answer. Concluding
    /// "unclassified" instead costs a root set that provably omits a RUNNING
    /// thread's JIT-frame oops, which the non-moving sweep then frees on
    /// `GC_FLAG_MARKED` alone.
    ///
    /// Re-signalling an already-`STATE_ARMED` slot is safe and idempotent: the
    /// handler re-checks `ACTIVE_SESSIONS`, re-finds the slot and early-returns unless
    /// the state is still `STATE_ARMED`, so a redundant delivery (including one
    /// racing with the peer parking) does no work.
    ///
    /// Returns `STATE_EMPTY` if the peer exited mid-wait — nothing to scan and
    /// nothing running, which is not a coverage hole and must not be counted as
    /// one.
    fn wait_for_response_retrying(slot: &LinuxSlot, tid: u32) -> u8 {
        wait_for_response_retrying_with(slot, tid, false)
    }

    /// [`wait_for_response_retrying`]; with `mask_door`
    /// (`super::takeover_mask_door_enabled`, the take-over pass only), a
    /// peer that missed an attempt is checked for the take-over signal
    /// BLOCKED before it is re-signalled: then it is in a masked counted C
    /// call, its handler runs only when the call returns, and the slot is
    /// cancelled at once and answered `STATE_NOT_JIT` (the answer its
    /// handler would give from C) instead of waiting out the budget.
    fn wait_for_response_retrying_with(slot: &LinuxSlot, tid: u32, mask_door: bool) -> u8 {
        let attempt = Duration::from_millis(peer_deadline_ms());
        let budget = Duration::from_millis(peer_total_deadline_ms());
        let start = Instant::now();
        loop {
            if let Some(state) = poll_for_response(slot, attempt) {
                return state;
            }
            if mask_door && peer_blocks_takeover_signal(tid) == Some(true) {
                // The same CAS as the budget's cancel below: a peer that
                // answered in the same instant keeps its answer.
                return match slot.state.compare_exchange(
                    STATE_ARMED,
                    STATE_CANCELLED,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => {
                        slot.release_resume();
                        STATE_NOT_JIT
                    }
                    Err(answered) => answered,
                };
            }
            if start.elapsed() >= budget {
                // Out of budget. This IS a coverage hole; the caller counts it.
                //
                // A CAS, not a store: a peer that reached the handler in the
                // same instant has already moved the slot to `PARKED` (or
                // `NOT_JIT`), and overwriting that with `CANCELLED` threw away
                // a real answer -- a parked peer then left the handler
                // unscanned while being counted as unclassified. The handler's
                // own `ARMED -> PARKED` CAS fails once this one succeeds.
                return match slot.state.compare_exchange(
                    STATE_ARMED,
                    STATE_CANCELLED,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => {
                        slot.release_resume();
                        STATE_CANCELLED
                    }
                    Err(answered) => answered,
                };
            }
            if !send_takeover_signal(tid) {
                // ESRCH: exited while we waited.
                slot.release_resume();
                slot.state.store(STATE_CANCELLED, Ordering::Release);
                slot.clear();
                return STATE_EMPTY;
            }
            XT_PEER_RESIGNALS.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Snapshot of every readable mapping, from `/proc/self/maps`.
    ///
    /// # This snapshot must be taken PER PARKED PEER, never hoisted
    ///
    /// It is used to bound a conservative stack walk, and the walk
    /// DEREFERENCES what it bounds. A snapshot only describes the address
    /// space at the instant it was read, and this process remaps constantly:
    /// every platform thread that starts or exits maps or unmaps an 8 MiB
    /// stack, and a virtual-thread workload churns them by the thousand.
    ///
    /// The danger is not that the parked peer's own stack disappears — a
    /// parked peer cannot exit, so its mapping is stable for exactly as long
    /// as the walk needs it. It is that a STALE entry can still CONTAIN the
    /// peer's `rsp` while describing a mapping that no longer exists: an
    /// exited thread's 8 MiB stack is unmapped, a new thread's smaller stack
    /// is later placed inside that freed span, and the old entry answers the
    /// lookup with an `hi` far above the new stack's real top.
    /// `readable_region_end_from_regions` then returns an `end` past the end
    /// of the peer's stack and the walk steps off it.
    ///
    /// Measured 2026-09-08 on Linux, `VthreadProbe` (10000 virtual threads),
    /// with `helper_window_pass` hoisting one snapshot for the whole pass:
    ///
    /// ```text
    /// [xt-hw] tid=582 rsp=0x7126e17f29b8 end=0x7126e1ff29b8   <- rsp + 8 MiB
    /// SIGSEGV               addr=0x7126e17fb000               <- ~34 KiB up
    /// ```
    ///
    /// `end` had fallen back to the `MAX_STACK_SCAN` cap, so the matched entry
    /// claimed at least 8 MiB above `rsp`, while the peer's real stack ended
    /// 34 KiB up — a page-aligned fault on the first unmapped page above it,
    /// killing the VM mid-collection. Both `vthread_probe_10000_all_increment`
    /// and `vthread_gc_stress_completes` died this way.
    ///
    /// Taking it after the peer parks removes the window: the entry containing
    /// a parked peer's `rsp` is that peer's own live stack, and a parked peer
    /// holds it mapped.
    fn readable_regions() -> Vec<(usize, usize)> {
        let mut regions = Vec::new();
        let Ok(maps) = std::fs::read_to_string("/proc/self/maps") else {
            return regions;
        };
        for line in maps.lines() {
            let mut parts = line.split_whitespace();
            let Some(range) = parts.next() else { continue };
            let Some(perms) = parts.next() else { continue };
            if !perms.starts_with('r') {
                continue;
            }
            let Some((lo, hi)) = range.split_once('-') else {
                continue;
            };
            let Ok(lo) = usize::from_str_radix(lo, 16) else {
                continue;
            };
            let Ok(hi) = usize::from_str_radix(hi, 16) else {
                continue;
            };
            regions.push((lo, hi));
        }
        regions
    }

    /// Whether `process_vm_readv` can read THIS process's own memory here.
    ///
    /// Probed once, against a known-good address, because a kernel or a seccomp
    /// profile that refuses the syscall must not silently turn every stack scan
    /// into zero roots — that is a missing-root bug, which is far worse than
    /// the crash this reader exists to prevent. When it is unavailable the
    /// walk falls back to the direct load it has always used.
    fn safe_self_read_available() -> bool {
        static OK: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
        cratonvm_jit::memo_flag::memo_bool(&OK, || {
            // Kill switch, so the reader and the historical direct load are
            // A/B-able inside ONE binary. Setting it restores the pre-fix
            // behaviour exactly — including the SIGSEGV.
            if cratonvm_types::flags::runtime_flag_on("CRATONVM_XT_NO_SAFE_PEER_READ") {
                return false;
            }
            let probe: u64 = 0x5ab0_1234_5678_9abc;
            let mut out: u64 = 0;
            let n = unsafe {
                let local = libc::iovec {
                    iov_base: (&mut out as *mut u64).cast::<libc::c_void>(),
                    iov_len: 8,
                };
                let remote = libc::iovec {
                    iov_base: (&probe as *const u64 as *mut u64).cast::<libc::c_void>(),
                    iov_len: 8,
                };
                libc::process_vm_readv(libc::getpid(), &local, 1, &remote, 1, 0)
            };
            n == 8 && out == probe
        })
    }

    /// Copy up to `out.len()` bytes from OUR OWN address space at `addr`,
    /// returning how many bytes were actually copied.
    ///
    /// # Why a syscall and not a load
    ///
    /// The conservative stack walk dereferences addresses it derived from a
    /// `/proc/self/maps` snapshot, and NO snapshot of this process is
    /// trustworthy for the duration of a walk. glibc caches an exited thread's
    /// stack with its pages still readable — so the kernel reports it merged
    /// with the neighbouring mapping — and then, when a new thread reuses that
    /// cached stack, `mprotect`s a PROT_NONE guard page into the middle of the
    /// span. A region that was readable when it was read back can therefore
    /// grow an unreadable hole INSIDE it a moment later, with no unmapping
    /// involved and nothing the reader could have re-checked. Under a
    /// virtual-thread workload that churns thousands of threads, this happens
    /// constantly.
    ///
    /// `process_vm_readv` makes the kernel do the access check: an unreadable
    /// page ends the copy with a SHORT READ instead of delivering SIGSEGV. It
    /// is the same primitive a debugger uses, applied to our own pid, where it
    /// needs no privilege. Verified against a deliberately `mprotect`ed
    /// PROT_NONE page: it returns exactly the bytes before the hole.
    ///
    /// A short read is not an error and is not a coverage hole: it means the
    /// peer's readable stack ends there. Everything above is some other
    /// thread's memory, which this peer cannot reach and which the walk had no
    /// business reading in the first place — the fault was only the visible
    /// half of that mistake.
    fn read_self_memory(addr: usize, out: &mut [u8]) -> usize {
        if !safe_self_read_available() {
            // Historical behaviour, kept for a kernel that refuses the syscall.
            unsafe {
                std::ptr::copy_nonoverlapping(addr as *const u8, out.as_mut_ptr(), out.len());
            }
            return out.len();
        }
        let n = unsafe {
            let local = libc::iovec {
                iov_base: out.as_mut_ptr().cast::<libc::c_void>(),
                iov_len: out.len(),
            };
            let remote = libc::iovec {
                iov_base: addr as *mut libc::c_void,
                iov_len: out.len(),
            };
            libc::process_vm_readv(libc::getpid(), &local, 1, &remote, 1, 0)
        };
        if n <= 0 {
            0
        } else {
            (n as usize).min(out.len())
        }
    }

    /// Number of 8-byte words copied per `read_self_memory` call.
    ///
    /// One syscall per 8 KiB. The scanned span is `stack_top - rsp`, i.e. the
    /// peer's USED depth, which is tens of KiB in the common case — a handful
    /// of syscalls per peer, against a walk that already touches every word.
    const SCAN_CHUNK_WORDS: usize = 1024;

    /// Backstop for the TAKE-OVER scan. Unlike the helper-window pass it reads
    /// through `read_self_memory` in fixed chunks rather than into a band sized
    /// up front, and it stops at the first unreadable page (the guard page of
    /// whatever mapping sits above the stack), so it can afford to follow a
    /// deep stack all the way up. `main-vm` runs with a 128 MiB stack, and a
    /// peer frozen more than 8 MiB below its outermost frame used to have
    /// every frame above that line go unscanned (gc-common w1-c, 2026-09-23).
    const MAX_TAKEOVER_STACK_SCAN: usize = 512 * 1024 * 1024;

    /// `end`, lowered to the stack top `tid` published when `rsp` lies in
    /// that band (gen r5w1). The published top excludes the static TLS block
    /// and thread descriptor at the top of the stack mapping; a maps-derived
    /// `end` does not.
    fn peer_band_end(
        published: Option<cratonvm_gc::shadow_stack::SlotBand>,
        rsp: usize,
        end: usize,
    ) -> usize {
        match published {
            Some(b) if rsp >= b.lo && rsp < b.hi => end.min(b.hi),
            _ => end,
        }
    }

    /// `(end, clamped)`: the bound of the readable mapping containing `addr`,
    /// capped at `addr + cap`; `clamped` says the cap, not the mapping, set it.
    fn readable_region_end_from_regions(
        addr: usize,
        regions: &[(usize, usize)],
        cap: usize,
    ) -> Option<(usize, bool)> {
        for &(lo, hi) in regions {
            if addr >= lo && addr < hi {
                let limit = addr.saturating_add(cap);
                return Some((hi.min(limit), hi > limit));
            }
        }
        None
    }

    /// `(end, cap_decided)` of the band a peer's stack scan reads from `rsp`:
    /// the readable mapping's end capped at `rsp + cap`, lowered to the stack
    /// top `tid` published ([`peer_band_end`]). `cap_decided` says the CAP set
    /// `end` -- the band may stop short of the peer's stack -- and is false
    /// when the published top lowered `end` below it, because then the band is
    /// the peer's whole stack however far the mapping ran.
    ///
    /// gcd d10/t (`gcd-d9c-linux-xt-band-judgments-...`, item 1): the ONE
    /// statement of the judgement both Linux readers make. d9/c fixed it in
    /// `snapshot_parked_slot` (helper windows); the take-over's
    /// `scan_slot_with_regions` still bumped `XT_TAKEOVER_STACK_INCOMPLETE` on
    /// the mapping's `clamped` alone, so a stack whose `/proc/self/maps` line
    /// merged with a readable neighbour (thread stacks are plain anonymous rw
    /// mappings) read as an incomplete pin set on every take-over pause.
    fn peer_band_bound(
        tid: u32,
        rsp: usize,
        regions: &[(usize, usize)],
        cap: usize,
    ) -> Option<(usize, bool)> {
        let (region_end, clamped) = readable_region_end_from_regions(rsp, regions, cap)?;
        let end = peer_band_end(
            cratonvm_gc::shadow_stack::thread_stack_band(tid),
            rsp,
            region_end,
        );
        Some((end, clamped && end == region_end))
    }

    /// [`peer_band_bound`] with the peer's published stack band already read
    /// (`published`), so it takes no lock (JIT round 18 wave 10, lane
    /// ffmeintr). The helper-window pass reads the band BEFORE it parks a
    /// peer: `thread_stack_band` takes the process-wide band table's
    /// `RwLock`, and a peer parked on a helper slot can be stopped anywhere
    /// -- inside `malloc` -- while a third thread holds (or, the lock being
    /// writer-preferring, merely waits for) the table's write lock and needs
    /// that `malloc` arena (`publish_thread_stack_band` inserts into a
    /// `HashMap` at a thread's first JIT entry). The collector would then wait
    /// on the lock with the peer parked: a silent hang. A blocked peer's band
    /// cannot change while it is blocked (it cannot exit), so the early read
    /// is the same answer.
    fn peer_band_bound_published(
        published: Option<cratonvm_gc::shadow_stack::SlotBand>,
        rsp: usize,
        regions: &[(usize, usize)],
        cap: usize,
    ) -> Option<(usize, bool)> {
        let (region_end, clamped) = readable_region_end_from_regions(rsp, regions, cap)?;
        let end = peer_band_end(published, rsp, region_end);
        Some((end, clamped && end == region_end))
    }

    /// Index of `RBP` in [`LinuxSlot::regs`] (`takeover_signal_handler`'s
    /// register order: R8..R15, RDI, RSI, RBP, ...).
    const REG_IDX_RBP: usize = 10;

    /// Frames printed per frozen peer by [`census_frozen_frames`].
    const XT_CENSUS_FRAMES_PER_PEER: usize = 64;

    /// gch h3/yc (census, `CRATONVM_DBG=oldmark-root-census`): one
    /// `[xt-frame]` line per compiled frame of a frozen peer
    /// ([`super::frozen_frame_chain`] over its frozen `RIP` / `RBP`), so a
    /// census holder's `[xt-word] at=` slot names its frame. Reads the frame
    /// records through `read_self_memory`, as the band scan reads the stack.
    /// The roots are unchanged.
    fn census_frozen_frames(tid: u32, rip: usize, rbp: usize, rsp: usize, end: usize) {
        let frame_size_at = |pc: usize| -> Option<usize> {
            let p = cratonvm_jit::lookup_jit_code_range(pc)?;
            // SAFETY: a registered code range keeps its CompiledMethod alive
            // while a frame of it is live, and the peer is frozen inside these
            // frames for the whole pass (the contract every frame walk here
            // relies on).
            let cm = unsafe { &*(p as *const cratonvm_jit::CompiledMethod) };
            // `osr_frame_size`: every body's band size, the name is historical
            // (`conservative_roots` reads its frame bands the same way).
            usize::try_from(cm.osr_frame_size).ok()
        };
        let read_record = |at: usize| -> Option<(usize, usize)> {
            let mut rec = [0u64; 2];
            // SAFETY: the 16-byte local buffer viewed as bytes.
            let bytes =
                unsafe { std::slice::from_raw_parts_mut(rec.as_mut_ptr().cast::<u8>(), 16) };
            // Cast: a stack word read back as the address it holds.
            (read_self_memory(at, bytes) == 16).then(|| (rec[0] as usize, rec[1] as usize))
        };
        let frames = super::frozen_frame_chain(
            rip,
            rbp,
            (rsp, end),
            XT_CENSUS_FRAMES_PER_PEER,
            frame_size_at,
            read_record,
        );
        if frames.is_empty() {
            eprintln!("[xt-frame] pop=takeover tid={tid} frames=0 rip=0x{rip:x} rbp=0x{rbp:x}");
        }
        for f in &frames {
            let method = cratonvm_jit::lookup_jit_code_range(f.pc).map_or_else(String::new, |p| {
                // SAFETY: as in `frame_size_at` above.
                let cm = unsafe { &*(p as *const cratonvm_jit::CompiledMethod) };
                cm.method_label.clone()
            });
            eprintln!(
                "[xt-frame] pop=takeover tid={tid} depth={} rbp=0x{:x} lo=0x{:x} hi=0x{:x} \
                 pc=0x{:x} method={method}",
                f.depth,
                f.rbp,
                f.lo,
                f.rbp + 16,
                f.pc,
            );
        }
    }

    fn scan_slot<F, G>(
        slot: &LinuxSlot,
        is_obj: &F,
        above: Option<(&G, &cratonvm_jit::JitCodeRangeView, usize)>,
        roots: &mut Vec<ObjectRef>,
        tlab_slots: &[(usize, usize)],
        rust_floor: usize,
    ) -> usize
    where
        F: Fn(usize) -> Option<ObjectRef>,
        G: Fn(usize) -> Option<ObjectRef>,
    {
        let regions = readable_regions();
        scan_slot_with_regions(slot, &regions, is_obj, above, roots, tlab_slots, rust_floor)
    }

    fn scan_slot_with_regions<F, G>(
        slot: &LinuxSlot,
        regions: &[(usize, usize)],
        is_obj: &F,
        above: Option<(&G, &cratonvm_jit::JitCodeRangeView, usize)>,
        roots: &mut Vec<ObjectRef>,
        tlab_slots: &[(usize, usize)],
        rust_floor: usize,
    ) -> usize
    where
        F: Fn(usize) -> Option<ObjectRef>,
        G: Fn(usize) -> Option<ObjectRef>,
    {
        let census = super::xt_census_on();
        let mut census_words = 0usize;
        let mut found = 0usize;
        let pair_tid = slot.tid.load(Ordering::Acquire);
        for (ri, reg) in slot.regs.iter().enumerate() {
            let v = reg.load(Ordering::Acquire);
            // LINUX arm of `CRATONVM_DBG_PEER_REG_PAIRING` (§11 built the
            // Windows one). Cast: REG_COUNT is 17. EVERY register, before
            // any root filter, as the Windows arm records: `record_peer_reg`
            // also puts a young-region register word into the young pin
            // ledger, and the region's slack words are not object-looking
            // (gcg g4/xt, `gcg-g3h-...-review-findings` item 4).
            cratonvm_gc::gc_quiescence::record_peer_reg(pair_tid, ri as u8, v);
            if let Some(o) = is_obj(v) {
                if census && census_words < super::XT_CENSUS_WORDS_PER_PEER {
                    census_words += 1;
                    super::xt_census_word("takeover", pair_tid, "reg", 0, v, o.as_ptr() as usize);
                }
                roots.push(o);
                found += 1;
            }
        }

        // `XT_TAKEOVER_STACK_INCOMPLETE` on the three ways this band is not
        // the peer's whole stack, as the Windows arm counts them (gc-common
        // w2-c): the pause's `TakeoverVerdict` reads it as "the pin set is not
        // complete". This arm used to return silently on all three.
        let rsp = slot.rsp.load(Ordering::Acquire);
        if rsp == 0 || rsp & 0x7 != 0 {
            super::note_takeover_stack_incomplete();
            return found;
        }
        // gen r5w1: never past the peer's published stack band. A maps line
        // ends at the stack MAPPING's end, which holds the peer's static TLS
        // and thread descriptor (`shadow_stack::clamp_stack_top_below_static_tls`);
        // a TLS word that looks like an object must not become a root.
        // gcd d10/t: and the band is incomplete only when the CAP, not the
        // published top, decided its end (`peer_band_bound`).
        let Some((end, cap_decided)) =
            peer_band_bound(pair_tid, rsp, regions, MAX_TAKEOVER_STACK_SCAN)
        else {
            super::note_takeover_stack_incomplete();
            return found;
        };
        if cap_decided {
            super::note_takeover_stack_incomplete();
        }
        // zwr: a delivery thread's batch Rust frames are not read.
        let end = super::cap_at_rust_band_floor(rsp, end, rust_floor);
        // gci i1/fr (`gcf-f7f-...` kind A, opt-in): the words at or above the
        // innermost compiled segment's top take the `above` probe (object
        // starts); below it, and with no plausible chain, `is_obj` as always.
        // The records are read through the kernel, as the band is.
        let above_from = above.and_then(|(_, view, frames)| {
            super::takeover_innermost_segment_top(
                slot.rip.load(Ordering::Acquire),
                slot.regs[REG_IDX_RBP].load(Ordering::Acquire),
                (rsp, end),
                frames,
                |pc| super::compiled_frame_size_in(view, pc),
                |at| {
                    let mut rec = [0u64; 2];
                    // SAFETY: the 16-byte local buffer viewed as bytes.
                    let bytes = unsafe {
                        std::slice::from_raw_parts_mut(rec.as_mut_ptr().cast::<u8>(), 16)
                    };
                    // Cast: a stack word read back as the address it holds.
                    (read_self_memory(at, bytes) == 16).then(|| (rec[0] as usize, rec[1] as usize))
                },
            )
        });
        if census && above.is_some() {
            eprintln!("[xt-above-chain] tid={pair_tid} from={above_from:x?}");
        }
        if census {
            eprintln!("[xt-band] pop=takeover tid={pair_tid} rsp=0x{rsp:x} end=0x{end:x}");
            // gch h3/yc: the frozen peer's compiled frames (census only).
            census_frozen_frames(
                pair_tid,
                slot.rip.load(Ordering::Acquire),
                slot.regs[REG_IDX_RBP].load(Ordering::Acquire),
                rsp,
                end,
            );
        }
        // Read through the kernel rather than dereferencing directly: the
        // bound above comes from a `/proc/self/maps` snapshot, and a snapshot
        // of this process is stale the instant it is taken. See
        // `read_self_memory`.
        let mut p = rsp;
        let mut chunk = [0u64; SCAN_CHUNK_WORDS];
        let mut tlab_cursor = super::TlabSlotCursor::new(tlab_slots);
        while p + 8 <= end {
            let want = (end - p).min(SCAN_CHUNK_WORDS * 8) & !7;
            let bytes =
                unsafe { std::slice::from_raw_parts_mut(chunk.as_mut_ptr().cast::<u8>(), want) };
            let got = read_self_memory(p, bytes) & !7;
            if got == 0 {
                // The peer's readable stack ends here.
                break;
            }
            for (i, &w) in chunk[..got / 8].iter().enumerate() {
                let w = w as usize;
                let at = p + i * 8;
                // zt t26/zc5b item 1: a `Tlab` pointer word is never a reference.
                if tlab_cursor.hit(at) {
                    continue;
                }
                let probed = match (above, above_from) {
                    (Some((above_probe, _, _)), Some(from)) if at >= from => above_probe(w),
                    _ => is_obj(w),
                };
                if let Some(o) = probed {
                    if census && census_words < super::XT_CENSUS_WORDS_PER_PEER {
                        census_words += 1;
                        super::xt_census_word(
                            "takeover",
                            pair_tid,
                            "stack",
                            at,
                            w,
                            o.as_ptr() as usize,
                        );
                    }
                    // 0xfe = TAKE-OVER stack word, the convention the Windows
                    // arm documents (0xff is the helper window). This arm used
                    // 0xff for both, so a pairing hit could not say which
                    // population it came from.
                    cratonvm_gc::gc_quiescence::record_peer_reg(pair_tid, 0xfe, w);
                    // A taken-over peer is NOT in a blocked region, so the fold
                    // will not adopt this capture: it is recorded to be COUNTED
                    // (`PEER_STACK_SLOTS_UNROUTED`) should this cycle relocate
                    // the object, which the take-over's refusal (Generational,
                    // ZGC) or pin (G1) is supposed to prevent.
                    // Take-over twin of the capture: capped at half the buffer
                    // so it cannot crowd out the helper-window words the
                    // blocked-wake remap needs (w2-c, `record_takeover_stack_slot`).
                    cratonvm_gc::gc_quiescence::record_takeover_stack_slot(pair_tid, at, w);
                    roots.push(o);
                    found += 1;
                }
            }
            p += got;
            if got < want {
                // Short read: an unreadable page, i.e. the top of the stack.
                break;
            }
        }
        found
    }

    fn release_slot(slot: &LinuxSlot) {
        slot.release_resume();
        let start = Instant::now();
        while slot.state.load(Ordering::Acquire) == STATE_PARKED
            && start.elapsed() < Duration::from_millis(100)
        {
            std::thread::yield_now();
        }
        slot.clear();
    }

    /// What [`snapshot_parked_slot`] copied out of a parked peer: its register
    /// file, its `rsp`, how many words of `[rsp, ..)` landed in the band, and
    /// whether the band is the peer's WHOLE readable stack (`false` for an
    /// unusable `rsp` or one no mapping contains).
    struct ParkedSnapshot {
        tid: u32,
        regs: [usize; REG_COUNT],
        rsp: usize,
        band_words: usize,
        band_complete: bool,
    }

    /// The COPY half of the helper-window classification (r9w2-vm2), run while
    /// the peer is PARKED in the takeover handler.
    ///
    /// On a helper slot (`LinuxSlot::helper`) the handler parks a peer at
    /// whatever instruction the signal found — inside `malloc`, holding a
    /// `parking_lot` lock in
    /// blocked-region Rust code, anywhere. So nothing here may allocate or take
    /// a lock: it reads the slot's atomics and copies `[rsp, end)` through
    /// `read_self_memory` (a syscall) into `band`, which the caller sized
    /// before any peer was signalled to hold `MAX_STACK_SCAN` bytes — the
    /// most `readable_region_end_from_regions` can ever return — so no copy is
    /// ever truncated. Everything that allocates or locks (the candidate
    /// pushes, `record_peer_stack_slot`'s mutex, `record_peer_reg`) runs in
    /// [`classify_parked_snapshot`] AFTER `release_slot`, over the copy. This
    /// is what the Windows arm's `snapshot_peer` has always done.
    #[cfg_attr(not(test), allow(dead_code))]
    fn snapshot_parked_slot(
        slot: &LinuxSlot,
        regions: &[(usize, usize)],
        band: &mut [u64],
    ) -> ParkedSnapshot {
        snapshot_parked_slot_with(slot, regions, band, None)
    }

    /// [`snapshot_parked_slot`], with the peer's published stack band read
    /// before it was parked when `preread` is `Some` (JIT round 18 wave 10,
    /// lane ffmeintr: [`peer_band_bound_published`] says why the lookup must
    /// not run inside the parked window). `None` looks it up here, as before
    /// (`CRATONVM_XT_HELPER_BAND_PREREAD=0`, and the unit tests).
    fn snapshot_parked_slot_with(
        slot: &LinuxSlot,
        regions: &[(usize, usize)],
        band: &mut [u64],
        preread: Option<Option<cratonvm_gc::shadow_stack::SlotBand>>,
    ) -> ParkedSnapshot {
        let mut regs = [0usize; REG_COUNT];
        for (dst, reg) in regs.iter_mut().zip(slot.regs.iter()) {
            *dst = reg.load(Ordering::Acquire);
        }
        let mut snap = ParkedSnapshot {
            tid: slot.tid.load(Ordering::Acquire),
            regs,
            rsp: slot.rsp.load(Ordering::Acquire),
            band_words: 0,
            band_complete: false,
        };
        let rsp = snap.rsp;
        if rsp == 0 || rsp & 0x7 != 0 {
            return snap;
        }
        // gen r5w1: see `scan_slot_with_regions` -- the band stops at the
        // peer's published stack top, below its static TLS.
        // gcd d9/c: the cap decided the band's end only if the published stack
        // top did not lower it further. The mapping's own `clamped` compares
        // its end with `rsp + MAX_STACK_SCAN`, and a `/proc/self/maps` line can
        // merge a thread's stack with a readable neighbour far above it; a band
        // that then stops at the peer's own published top is its WHOLE stack,
        // and judging it incomplete refused the window's pin (and, by the
        // discharge, the cycle's promotion) on every pause. One statement of
        // that judgement for both readers since gcd d10/t: `peer_band_bound`.
        let bound = match preread {
            Some(published) => peer_band_bound_published(published, rsp, regions, MAX_STACK_SCAN),
            None => peer_band_bound(snap.tid, rsp, regions, MAX_STACK_SCAN),
        };
        let Some((end, clamped)) = bound else {
            return snap;
        };
        let (words, complete) = copy_peer_band(rsp, end, clamped, band);
        snap.band_words = words;
        snap.band_complete = complete;
        snap
    }

    /// The copy of [`snapshot_parked_slot_with`]: `[rsp, end)` into `band`
    /// through `read_self_memory`; `(words copied, band complete)`. No
    /// allocation, no lock. Split out (JIT round 18 wave 10, lane ffmeintr) so
    /// the unparked read of a blocked peer (`CRATONVM_XT_HELPER_UNPARKED_READ`)
    /// copies with exactly the parked read's rules.
    fn copy_peer_band(rsp: usize, end: usize, clamped: bool, band: &mut [u64]) -> (usize, bool) {
        // Same kernel-mediated read as `scan_slot_with_regions`, and for the
        // same reason: this pass is the one that SIGSEGV'd the VM on
        // `VthreadProbe`. See `read_self_memory`.
        let mut p = rsp;
        let mut words = 0usize;
        let mut short_read = false;
        while p + 8 <= end && words < band.len() {
            let want_words = ((end - p) / 8)
                .min(SCAN_CHUNK_WORDS)
                .min(band.len() - words);
            let dst = &mut band[words..words + want_words];
            // SAFETY: `dst` is `want_words` initialised `u64`s; viewing them as
            // bytes is always valid.
            let bytes = unsafe {
                std::slice::from_raw_parts_mut(dst.as_mut_ptr().cast::<u8>(), want_words * 8)
            };
            let got = read_self_memory(p, bytes) & !7;
            if got == 0 {
                short_read = true;
                break;
            }
            words += got / 8;
            p += got;
            if got < want_words * 8 {
                // Short read: an unreadable page, i.e. the top of the stack.
                short_read = true;
                break;
            }
        }
        // COMPLETE: every readable word from `rsp` up has been read. Stopping
        // at an unreadable page is not a partial scan — that page is the end of
        // this peer's stack, and what lies above belongs to another thread.
        //
        // What is NOT complete (gc-common w1-c, 2026-09-23): a read that ran
        // to an `end` the `MAX_STACK_SCAN` cap set rather than the mapping, or
        // that filled the band first. This used to be set unconditionally on
        // the argument that the cap "never cuts a read short of `end`" -- true,
        // but `end` itself was the cap for a stack deeper than 8 MiB (`main-vm`
        // runs on 128 MiB), and such a band was pinned as the peer's WHOLE
        // stack, licensing a discharge with its outer frames unscanned.
        // And a band with NO word in it is never complete: `rsp` sat in a
        // mapping the snapshot called readable and the kernel did not.
        let band_full = words >= band.len() && p + 8 <= end;
        (words, words > 0 && (short_read || (!clamped && !band_full)))
    }

    /// JIT round 18 wave 10 (lane ffmeintr): the published stack band of a
    /// blocked peer, read BEFORE the helper-window pass parks it
    /// ([`peer_band_bound_published`] says why); `None` (look it up inside
    /// the parked window, the old order) with
    /// `CRATONVM_XT_HELPER_BAND_PREREAD=0`. Allocates nothing, but reads a
    /// flag, so it is never called with a peer parked.
    fn helper_band_preread(tid: u32) -> Option<Option<cratonvm_gc::shadow_stack::SlotBand>> {
        super::helper_band_preread_enabled()
            .then(|| cratonvm_gc::shadow_stack::thread_stack_band(tid))
    }

    /// gcd d10/t: park a blocked peer ONCE MORE on a helper slot and copy its
    /// band against `regions` (a fresh `/proc/self/maps` read), for a peer the
    /// pass's first read found no band for. `None` when no slot is free, the
    /// peer exited, or it did not answer; the caller then counts it
    /// unreadable, as it would have.
    fn repark_helper_snapshot(
        tid: u32,
        regions: &[(usize, usize)],
        band: &mut [u64],
    ) -> Option<ParkedSnapshot> {
        // JIT round 18 wave 10 (lane ffmeintr): before the park.
        let preread = helper_band_preread(tid);
        let slot = arm_slot(tid, true)?;
        if !send_takeover_signal(tid) {
            slot.clear();
            return None;
        }
        match wait_for_response_retrying(slot, tid) {
            STATE_PARKED => {
                let snap = snapshot_parked_slot_with(slot, regions, band, preread);
                release_slot(slot);
                Some(snap)
            }
            _ => {
                slot.clear();
                None
            }
        }
    }

    /// gcd d3/m: every young word of a parked snapshot's register file and of
    /// its copied band `band[..snap.band_words]`, raw, into `out` (cleared
    /// first). See `super::collect_young_band_words`. Runs after the release,
    /// over the copy, like [`classify_parked_snapshot`].
    fn parked_snapshot_young_words(
        snap: &ParkedSnapshot,
        band: &[u64],
        range: &cratonvm_gc::gc_quiescence::YoungPinRange,
        out: &mut Vec<usize>,
    ) {
        out.clear();
        super::collect_young_band_words(range, snap.regs.iter().copied(), out);
        // gcj-j1pn-pinned-cycle-review-findings item 1: as `snapshot_young_words`.
        cratonvm_gc::gc_quiescence::note_peer_band_register_words(out);
        let words = snap.band_words.min(band.len());
        super::collect_young_band_words(range, band[..words].iter().map(|&w| w as usize), out);
    }

    /// JIT round 18 wave 10 (lane ffmeintr, EINTR2-1,
    /// `CRATONVM_DBG_XT_UNPARKED_READ_AUDIT=1`): compare an unparked read of a
    /// blocked peer (`copy`, the words of `[lo, ..)`) with a parked read of
    /// the same peer taken right after (`snap` over `band`, already released),
    /// and print one `[xt-unparked-audit]` line:
    ///
    /// * `same` / `differ_obj`: words at the same address that agree, and that
    ///   differ where either side names an object (should be 0 -- the band
    ///   above `lo` is frozen while the peer waits; a peer woken between the
    ///   two reads can make it non-zero);
    /// * `below_lo_obj`: object-looking words the parked read found BELOW `lo`
    ///   (the wait's own frames; the trampoline's contract says none);
    /// * `callee_saved_regs_missing`: callee-saved registers of the parked
    ///   peer (r12-r15, rbp, rbx) naming an object that is not among the
    ///   unparked words (the spill's claim says none).
    ///
    /// Any non-zero last two is a soundness finding for the unparked read.
    fn unparked_read_audit<F>(
        tid: u32,
        lo: usize,
        copy: &[u64],
        snap: &ParkedSnapshot,
        band: &[u64],
        is_obj: &F,
    ) where
        F: Fn(usize) -> Option<ObjectRef>,
    {
        let words = snap.band_words.min(band.len());
        let (mut same, mut differ_obj, mut outside) = (0usize, 0usize, 0usize);
        for (j, &u) in copy.iter().enumerate() {
            let a = lo + j * 8;
            if a < snap.rsp || (a - snap.rsp) / 8 >= words {
                outside += 1;
                continue;
            }
            let p = band[(a - snap.rsp) / 8];
            if p == u {
                same += 1;
            } else if is_obj(u as usize).is_some() || is_obj(p as usize).is_some() {
                differ_obj += 1;
            }
        }
        let mut below_lo_obj = 0usize;
        for (i, &w) in band[..words].iter().enumerate() {
            if snap.rsp + i * 8 >= lo {
                break;
            }
            if is_obj(w as usize).is_some() {
                below_lo_obj += 1;
            }
        }
        // `regs` order: r8-r11, r12-r15 (4..=7), rdi, rsi, rbp (10), rbx (11), ...
        let mut regs_missing = 0usize;
        for i in [4usize, 5, 6, 7, 10, 11] {
            let v = snap.regs[i];
            if v != 0 && is_obj(v).is_some() && !copy.iter().any(|&w| w as usize == v) {
                regs_missing += 1;
            }
        }
        eprintln!(
            "[xt-unparked-audit] tid={tid} lo={lo:#x} rsp={:#x} unparked_words={} parked_words={words} \
             same={same} differ_obj={differ_obj} outside={outside} below_lo_obj={below_lo_obj} \
             callee_saved_regs_missing={regs_missing}",
            snap.rsp,
            copy.len()
        );
    }

    /// One helper window's census lines (`[xt-band] pop=helper` and its
    /// `[xt-word] pop=helper` words), collected by
    /// [`classify_parked_snapshot`] and printed by the caller AFTER the gci
    /// i1/fr zero-depth step, so a demoted band prints no window line -- as the
    /// Windows arm, which prints after the step (gci i2/fr,
    /// `gci-i1fr-frames-roots-and-helper-windows-review-findings` item 2).
    struct HelperBandCensus {
        tid: u32,
        rsp: usize,
        words: usize,
        complete: bool,
        first_jit_at: Option<usize>,
        hits: Vec<(usize, usize, usize)>,
    }

    impl HelperBandCensus {
        /// Print the lines; the peer is running again, so this costs it
        /// nothing.
        fn print(self) {
            // gcg g3/h: `depth=` (the published JIT depth: `Some(0)` names a
            // window whose band holds only a stale JIT address) and `jit_at=`
            // (the slot of the first JIT address; `reg` when the register file
            // decided).
            let jit_at = match self.first_jit_at {
                Some(a) => format!("0x{a:x}"),
                None => "reg".to_string(),
            };
            eprintln!(
                "[xt-band] pop=helper tid={} rsp=0x{:x} words={} complete={} depth={:?} jit_at={jit_at}",
                self.tid,
                self.rsp,
                self.words,
                self.complete,
                cratonvm_gc::gc_quiescence::jit_depth_of_tid(self.tid),
            );
            for (at, w, root) in self.hits {
                super::xt_census_word("helper", self.tid, "hw", at, w, root);
            }
        }
    }

    /// What [`classify_parked_snapshot`] leaves for its caller to do once the
    /// zero-depth step has decided `has_jit` (gci i2/fr): record the band's
    /// stack-slot captures (`record_helper_stack_captures`, which labels them
    /// `window=<has_jit>`) and print the window's census lines -- the order
    /// the Windows arm runs them in.
    struct ParkedBandTail {
        tid: u32,
        captures: Vec<(usize, usize)>,
        census: Option<HelperBandCensus>,
    }

    impl ParkedBandTail {
        /// Record the captures (always) and print the census (a window only).
        fn finish(self, has_jit: bool) {
            super::record_helper_stack_captures(self.tid, has_jit, &self.captures);
            if let Some(c) = self.census.filter(|_| has_jit) {
                c.print();
            }
        }
    }

    /// The CLASSIFY half (r9w2-vm2): runs after `release_slot`, over the copy
    /// [`snapshot_parked_slot`] took. Returns `(has_jit, complete, tail)`;
    /// the caller applies its zero-depth step to `has_jit` and then calls
    /// [`ParkedBandTail::finish`] (gci i2/fr: the captures' `window=` label and
    /// the census lines follow the step, as on Windows). `tail.census` is
    /// `Some` only under `CRATONVM_DBG=oldmark-root-census` for a band
    /// classified a window.
    ///
    /// `complete` is the half that licenses PINNING instead of refusing the
    /// cycle: it says this peer's conservative root set is the WHOLE of what it
    /// can reach -- its register file and every readable word of its stack from
    /// `rsp` up. Pinning a partial set helps nothing, because what was missed
    /// is unrewritable too, so an incomplete band reports `false` and the
    /// caller keeps refusing.
    ///
    /// The words are still a faithful root source after the release: the peer
    /// is BLOCKED and `mark_blocked_region_leave` waits out the pause, so it
    /// cannot change them. The blocked-wake stack-slot remap needs each word's
    /// ORIGINAL address, which is `rsp + i * 8`.
    fn classify_parked_snapshot<F>(
        snap: &ParkedSnapshot,
        band: &[u64],
        ranges: &[(usize, usize)],
        is_obj: &F,
        candidates: &mut Vec<ObjectRef>,
        tlab_slots: &[(usize, usize)],
    ) -> (bool, bool, ParkedBandTail)
    where
        F: Fn(usize) -> Option<ObjectRef>,
    {
        let mut has_jit = false;
        let pair_tid_hw = snap.tid;
        for (ri_hw, &v) in snap.regs.iter().enumerate() {
            if !has_jit && crate::jit::xt_root_scan::in_code_ranges(&ranges, v) {
                has_jit = true;
            }
            // Cast: REG_COUNT is 17. Every register, as the Windows arm
            // records (gcg g4/xt; see `scan_slot_with_regions`).
            cratonvm_gc::gc_quiescence::record_peer_reg(pair_tid_hw, ri_hw as u8, v);
            if let Some(o) = is_obj(v) {
                candidates.push(o);
            }
        }
        // gcd d9/c: an INCOMPLETE band is still classified, as the Windows arm
        // does (`helper_window_pass` there scans every copied word and lets
        // `complete` gate only the pin). This returned here before reading a
        // word, so a peer whose band was cut short (a stack deeper than
        // `MAX_STACK_SCAN`) and whose REGISTERS held no JIT address was no
        // window at all: its compiled frames' words were in no root set and
        // nothing refused the cycle. Now its copied words are roots and a JIT
        // return address among them makes it an unpinned (refusing) window.
        // `CRATONVM_XT_HELPER_WINDOW_SCAN=0` still turns the whole pass off.
        let words = snap.band_words.min(band.len());
        // gcg g1/f: `(slot, word, root)` of each candidate, for the census
        // lines printed below when the window is kept.
        let mut census_hits: Option<Vec<(usize, usize, usize)>> =
            super::xt_census_on().then(Vec::new);
        // gen r5w1/crash5, `CRATONVM_DBG_ROOT_WRITE_AUDIT` only: the peer's own
        // stack as it published it (at its first compiled entry), to report a
        // band that ran PAST it -- the end comes from a `/proc/self/maps` line,
        // which merges a stack with a readable neighbour -- and each capture
        // taken from beyond it. Such a capture is adopted by this peer and was,
        // before the write-back's own-stack bound, stored back into memory some
        // other thread owns. Diagnostic only: nothing below changes.
        let own_stack = if cratonvm_gc::root_write_audit::enabled() {
            cratonvm_gc::shadow_stack::thread_stack_band(pair_tid_hw)
                .filter(|b| snap.rsp >= b.lo && snap.rsp < b.hi)
        } else {
            None
        };
        if let Some(b) = own_stack {
            let band_end = snap.rsp + words * 8;
            if band_end > b.hi {
                cratonvm_gc::root_write_audit::note_band_overrun(
                    pair_tid_hw,
                    snap.rsp,
                    band_end,
                    b,
                );
            }
        }
        // gcg g3/h: the band slot of the FIRST JIT address, for the census
        // (`None`: the registers decided, or no JIT address at all). A window
        // kept only because a deep slot holds a stale return address is the
        // `has_jit` weakness the census line names with this.
        let mut first_jit_at: Option<usize> = None;
        // gcg g4/xt (`gcg-g3h-...-review-findings` item 3): the stack-slot
        // remap captures `(slot, word)`, recorded after the loop, once
        // `has_jit` is known -- see `record_helper_stack_captures`.
        let mut pending_captures: Vec<(usize, usize)> = Vec::new();
        let mut tlab_cursor = super::TlabSlotCursor::new(tlab_slots);
        for (i, &w) in band[..words].iter().enumerate() {
            let w = w as usize;
            if !has_jit && crate::jit::xt_root_scan::in_code_ranges(&ranges, w) {
                has_jit = true;
                first_jit_at = Some(snap.rsp + i * 8);
            }
            // zt t26/zc5b item 1: a `Tlab` pointer word is never a reference
            // (the JIT-address test above still reads it).
            if tlab_cursor.hit(snap.rsp + i * 8) {
                continue;
            }
            if let Some(o) = is_obj(w) {
                let at = snap.rsp + i * 8;
                if let Some(b) = own_stack {
                    if !b.contains_slot(at) {
                        cratonvm_gc::root_write_audit::note_foreign_capture(pair_tid_hw, at, w, b);
                    }
                }
                cratonvm_gc::gc_quiescence::record_peer_reg(pair_tid_hw, 0xff, w);
                pending_captures.push((at, w));
                if let Some(hits) = census_hits.as_mut() {
                    if hits.len() < super::XT_CENSUS_WORDS_PER_PEER {
                        hits.push((at, w, o.as_ptr() as usize));
                    }
                }
                candidates.push(o);
            }
        }
        // JIT round 21 lane refs (`CRATONVM_DBG_REFERENT_HOLDER`, diagnostic):
        // name the band slot of every root a KEPT window contributes, for the
        // post-collection `[referent-holder]` report.
        if has_jit && crate::runtime::env_cache::dbg_referent_holder() {
            for &(at, w) in &pending_captures {
                if let Some(o) = is_obj(w) {
                    cratonvm_gc::gc_quiescence::record_jit_holder_provenance(
                        o.as_ptr() as usize,
                        format!(
                            "xt-helper-window tid={pair_tid_hw} slot=0x{at:x} rsp=0x{:x} \
                             above_rsp={} word=0x{w:x} first_jit_at={first_jit_at:x?}",
                            snap.rsp,
                            at.saturating_sub(snap.rsp),
                        ),
                    );
                }
            }
        }
        // gcg g1/f: the census lines of a window whose candidates are kept
        // (a JIT return address in its registers or band). gci i2/fr: they
        // and the captures (`record_helper_stack_captures`) are handed to the
        // caller, which runs them after its zero-depth step.
        let census = census_hits.filter(|_| has_jit).map(|hits| HelperBandCensus {
            tid: pair_tid_hw,
            rsp: snap.rsp,
            words,
            complete: snap.band_complete,
            first_jit_at,
            hits,
        });
        let tail = ParkedBandTail { tid: pair_tid_hw, captures: pending_captures, census };
        (has_jit, snap.band_complete, tail)
    }

    /// Linux implementation of the cross-thread JIT root scan. We cannot use
    /// Windows' SuspendThread/GetThreadContext primitives, so the collector sends
    /// a private signal to peer threads. The handler only parks peers interrupted
    /// inside registered JIT code; interpreter/native peers return immediately
    /// and remain cooperative barrier participants.
    ///
    /// ## Which threads are signalled: the roster (gc-common w2-c, 2026-09-23)
    ///
    /// `live_tids` -- the registered Java threads' published `gettid()`s, the
    /// same roster the Windows arm has used since 2026-09-10, under the same
    /// coverage obligation (see the Windows `take_over_pass`): compiled code is
    /// entered only through `JitEntryGuard` on Java execution paths, and every
    /// thread executing Java is in the registry with its OS tid published.
    ///
    /// This used to ignore the roster and signal every entry of
    /// `/proc/self/task` on the ground that the enumeration is cheap. It is;
    /// signalling the result is not. JIT compiler workers, GC pool workers,
    /// watchdogs and any thread a native library made were each sent `SIGUSR2`
    /// and waited for, and one whose mask blocks `SIGUSR2` (glibc's
    /// `SIGEV_THREAD` timer helper, a `signalfd` consumer, a runtime that masks
    /// its pool) never answers: it cost the pause the whole
    /// `CRATONVM_XT_PEER_TOTAL_MS` budget (1 s) ON EVERY PASS -- and a pass runs
    /// every barrier round for the first twenty -- and was then counted
    /// `XT_PEERS_UNCLASSIFIED`, a coverage hole for a thread that cannot hold a
    /// Java root. See `common-c-linux-takeover-signals-every-thread-FIXED-20260929`.
    ///
    /// `CRATONVM_XT_ROOT_SCAN_AUDIT=1` restores the superset AND checks the
    /// argument: every task is signalled, and one absent from the roster that
    /// parks (its `Rip` was in compiled code) is reported as a ROSTER HOLE and
    /// counted in `XT_ROSTER_MISSED_JIT_PEERS` -- still taken over and scanned,
    /// so the audit run itself is covered.
    pub fn take_over_pass<F>(
        taken: &mut TakenOver,
        is_obj: &F,
        roots: &mut Vec<ObjectRef>,
        live_tids: &[u32],
    ) -> usize
    where
        F: Fn(usize) -> Option<ObjectRef>,
    {
        take_over_pass_split::<F, F>(taken, is_obj, None, roots, live_tids)
    }

    /// [`take_over_pass`] with a second probe for the stack words at or above
    /// each parked peer's innermost compiled segment (gci i1/fr, `gcf-f7f-...`
    /// kind A; see `super::takeover_above_chain_word_probe`). `above: None` is
    /// `take_over_pass` exactly.
    pub fn take_over_pass_split<F, G>(
        taken: &mut TakenOver,
        is_obj: &F,
        above: Option<&G>,
        roots: &mut Vec<ObjectRef>,
        live_tids: &[u32],
    ) -> usize
    where
        F: Fn(usize) -> Option<ObjectRef>,
        G: Fn(usize) -> Option<ObjectRef>,
    {
        install_handler();
        // zt t26/zc5b item 1: copied before any peer is parked.
        let tlab_slots = taken.tlab_pointer_slots().to_vec();
        let ranges = crate::jit::jit_code_ranges_snapshot();
        // gci i1/fr: the code-range view the frame-chain walk reads, only for
        // the split (taken before any peer is signalled).
        let view = above.map(|_| cratonvm_jit::jit_code_range_view());
        // zwr: how many frames of the innermost segment keep the full probe.
        let frames = taken.above_split_frames();
        let above_v = above.zip(view.as_ref()).map(|(g, v)| (g, v, frames));
        if ranges.is_empty() {
            // Nothing compiled, so nothing to look for -- but the pass DID
            // look, and the sweep's coverage record must not read `passes=0`
            // ("never looked") for it. The Windows arm always publishes.
            cratonvm_gc::gc_quiescence::publish_xt_pass(0, 0, 0);
            return 0;
        }
        // Before the ranges are published and before any peer is signalled:
        // the handler answers only while a session is open (gc-common w9-g).
        let _session = PassSession::open();
        publish_ranges(&ranges);

        let self_tid = gettid();
        let mut newly = 0usize;
        let mut examined = 0usize;
        let mut unclassified = 0usize;
        let mut roots_this_pass = 0usize;
        let audit = roster_audit_enabled();
        // The roster (or, audited, every task), sorted, de-duplicated, without
        // 0 / ourselves / an already-frozen peer: `takeover_signal_candidates`.
        let candidates = super::takeover_signal_candidates(
            live_tids,
            audit.then(list_thread_tids),
            self_tid,
            taken,
        );
        // gcd d10/t (`common-c`): opt-in, the first passes of a take-over
        // signal only candidates that can be in compiled code (a published
        // JIT depth other than zero); see `takeover_signal_jit_only_enabled`.
        // The audit signals everyone and judges the rule instead.
        taken.passes = taken.passes.saturating_add(1);
        let narrow = !audit
            && super::takeover_signal_jit_only_enabled()
            && taken.passes <= super::JIT_ONLY_SIGNAL_PASSES;
        let (candidates, zero_depth) = if narrow || audit {
            super::split_by_published_jit_depth(
                candidates,
                cratonvm_gc::gc_quiescence::jit_depth_of_tid,
            )
        } else {
            (candidates, Vec::new())
        };
        let candidates = if narrow {
            let skipped = zero_depth.len() as u64;
            super::XT_JIT_ONLY_SIGNALS_SKIPPED.fetch_add(skipped, Ordering::Relaxed);
            candidates
        } else if zero_depth.is_empty() {
            candidates
        } else {
            // Audited: every candidate, in `takeover_signal_candidates`'
            // sorted order.
            let mut all = candidates;
            all.extend_from_slice(&zero_depth);
            all.sort_unstable();
            all
        };
        // gcg g3/h: the opt-in parallel signal (gcf f2/r) is deleted; see the
        // note above `JIT_ONLY_SIGNAL_PASSES`.
        // JIT round 17 wave 1 (lane eintr): read once per pass.
        let skip_in_syscall = super::takeover_skip_in_syscall_enabled();
        let mut in_syscall_skipped = 0usize;
        // JIT round 17 wave 2 (lane ffm3, EINTR17-1): read once per pass.
        let mask_door = super::takeover_mask_door_enabled();
        let mut masked_skipped = 0usize;
        // JIT round 18 wave 10b (lane ffmeintr): peers found in lazy C by the
        // signal gate, not signalled.
        let mut lazy_skipped = 0usize;
        for tid in candidates.iter().copied() {
            // JIT round 17 wave 1 (lane eintr,
            // `CRATONVM_XT_TAKEOVER_SKIP_IN_SYSCALL`): a peer blocked in a
            // system call with its `Rip` outside compiled code could only
            // answer `STATE_NOT_JIT`, which this loop ignores; the signal's
            // only effect would be an `EINTR` in the peer's C code. Every
            // pass and the audit apply it alike (on all passes, not only the
            // JIT-only ones: a counted thread sleeping in C called from the
            // interpreter used to be signalled from pass 9 on).
            if skip_in_syscall
                && super::takeover_signal_skips_peer_in_syscall(peer_syscall_state(tid), &ranges)
            {
                in_syscall_skipped += 1;
                continue;
            }
            // JIT round 17 wave 2 (lane ffm3, EINTR17-1,
            // `CRATONVM_FFM_DOWNCALL_MASK_TAKEOVER_SIGNAL`): a peer that
            // blocks the take-over signal is in a counted C call; its handler
            // could only answer `STATE_NOT_JIT` when the call returns, so it
            // is not signalled (`takeover_mask_door_enabled`).
            if mask_door && peer_blocks_takeover_signal(tid) == Some(true) {
                masked_skipped += 1;
                continue;
            }
            // JIT round 18 wave 10b (lane ffmeintr, `CRATONVM_FFM_LAZY_SIGNAL_GATE`,
            // default ON): the kernel read above is a snapshot; between it and
            // the `tgkill` the peer can go into C through a lazy downcall and
            // enter a blocking syscall, which the signal then interrupts
            // (`EINTR` in C; `R17Ffm3EintrRace`'s `short-poll ... EINTR seen: 1`).
            // Close that door first: a peer already in C lazily is not
            // signalled at all (the claim covers it, or the pause waits for it,
            // as for a sleeper), and a peer that is not stays out of lazy C
            // until this pass has its answer (`enter_lazy_native` waits on
            // `SIGNALLING`), so the signal lands in Rust or Java. The gate is
            // reopened when this iteration ends, on every path.
            struct Ungate(Option<std::sync::Arc<crate::threading::jvm_thread::GcBlockState>>);
            impl Drop for Ungate {
                fn drop(&mut self) {
                    if let Some(state) = self.0.take() {
                        state.ungate_takeover_signal();
                    }
                }
            }
            let _ungate = match taken.signal_gate(tid) {
                Some(state) => match state.try_gate_takeover_signal() {
                    crate::threading::jvm_thread::TakeoverSignalGate::InNative => {
                        lazy_skipped += 1;
                        continue;
                    }
                    crate::threading::jvm_thread::TakeoverSignalGate::Gated => Ungate(Some(state)),
                    crate::threading::jvm_thread::TakeoverSignalGate::Other => Ungate(None),
                },
                None => Ungate(None),
            };
            let off_roster = audit && !live_tids.contains(&tid);
            let Some(slot) = arm_slot(tid, false) else {
                // No free slot: THIS peer goes unscanned. Keep going rather
                // than `break`: every remaining peer is unscanned too and each
                // must be counted, which is what the helper-window pass does
                // since w1-c (a `break` counted one peer for all of them).
                XT_PEERS_UNCLASSIFIED.fetch_add(1, Ordering::Relaxed);
                unclassified += 1;
                continue;
            };
            examined += 1;
            if !send_takeover_signal(tid) {
                // ESRCH: the thread exited between listing and signalling.
                // Nothing to scan and nothing running — not a coverage hole.
                slot.clear();
                continue;
            }
            let resignals_before = XT_PEER_RESIGNALS.load(Ordering::Relaxed);
            let answer = wait_for_response_retrying_with(slot, tid, mask_door);
            let retried = XT_PEER_RESIGNALS.load(Ordering::Relaxed) != resignals_before;
            // Any DEFINITIVE answer that needed a re-signal would have been an
            // UNCLASSIFIED peer before the retry — `STATE_NOT_JIT` counts, it
            // means the peer reached the handler with its `Rip` outside JIT
            // code and is a cooperative barrier participant publishing its own
            // roots. Counting only `STATE_PARKED` reported zero on a run where
            // the retry had just converted sixteen.
            if answer != STATE_CANCELLED && retried {
                XT_PEERS_CLASSIFIED_AFTER_RETRY.fetch_add(1, Ordering::Relaxed);
            }
            if off_roster {
                if answer == STATE_PARKED {
                    XT_ROSTER_MISSED_JIT_PEERS.fetch_add(1, Ordering::Relaxed);
                    eprintln!(
                        "[xt-jit-roots] ROSTER HOLE: tid={tid} is in JIT code (rip={:#x}) \
                         but was absent from the roster, so take_over_pass would NOT have \
                         frozen or scanned it without CRATONVM_XT_ROOT_SCAN_AUDIT. Its \
                         registers and spill slots are unscanned conservative roots on a \
                         default run. See the Windows take_over_pass's coverage obligation.",
                        slot.rip.load(Ordering::Acquire)
                    );
                } else {
                    XT_ROSTER_SKIPPED_NON_JAVA.fetch_add(1, Ordering::Relaxed);
                }
            }
            if audit && answer == STATE_PARKED && zero_depth.contains(&tid) {
                super::XT_JIT_ONLY_SIGNAL_MISSES.fetch_add(1, Ordering::Relaxed);
                eprintln!(
                    concat!(
                        "[xt-jit-roots] JIT-ONLY SIGNAL MISS: tid={} is in JIT code (rip={:#x}) ",
                        "with a published JIT depth of 0, so CRATONVM_XT_TAKEOVER_SIGNAL_JIT_ONLY ",
                        "would not have signalled it on this pass."
                    ),
                    tid,
                    slot.rip.load(Ordering::Acquire)
                );
            }
            match answer {
                STATE_PARKED => {
                    // zwr: read once the peer is parked.
                    let rust_floor =
                        super::published_rust_band_floor(taken.rust_band_floor_state(tid).as_ref());
                    let mut found =
                        scan_slot(slot, is_obj, above_v, roots, &tlab_slots, rust_floor);
                    // The peer stays parked in the handler until `resume`, so
                    // its shadow stack is as stable as its machine stack. See
                    // `scan_taken_over_peer_shadow` for why neither of the
                    // other two scans reaches it.
                    // zwr: exact oops, never a cursor: the split pass's
                    // exact-start probe (no companion), as on Windows.
                    found += match above_v {
                        Some((above_probe, _, _)) => {
                            super::scan_taken_over_peer_shadow(tid, above_probe, roots)
                        }
                        None => super::scan_taken_over_peer_shadow(tid, is_obj, roots),
                    };
                    taken.handles.push(0);
                    taken.tids.push(tid);
                    // gcj j2/jt2: the register file the handler parked with,
                    // for the post-only card completion at resume.
                    let mut regs = [0usize; REG_COUNT];
                    for (r, w) in regs.iter_mut().zip(slot.regs.iter()) {
                        *r = w.load(Ordering::Acquire);
                    }
                    taken.note_frozen_registers(&regs);
                    newly += 1;
                    roots_this_pass += found;
                    XT_THREADS_TAKEN_OVER.fetch_add(1, Ordering::Relaxed);
                    XT_ROOTS_FOUND.fetch_add(found as u64, Ordering::Relaxed);
                    // See the Windows arm: a frozen peer's conservatively
                    // scanned registers/stack are not rewritable by this
                    // collection, so it must not be a moving one. gci i3/tk:
                    // the pinned door under the pause's promotion licence.
                    super::mark_takeover_peer_frozen(taken);
                    if dbg() {
                        eprintln!(
                            "[xt-jit-roots] linux took over tid={tid} rip=0x{:x}: {found} conservative roots",
                            slot.rip.load(Ordering::Acquire)
                        );
                    }
                }
                // `STATE_NOT_JIT` is a real answer: the peer reached the
                // handler and its `Rip` was outside JIT code, so it is a
                // cooperative barrier participant publishing its own precise
                // roots. `STATE_CANCELLED` is NOT an answer — the peer never
                // reached the handler, is STILL RUNNING, and its JIT-frame
                // oops are in no root set.
                STATE_CANCELLED => {
                    XT_PEERS_UNCLASSIFIED.fetch_add(1, Ordering::Relaxed);
                    unclassified += 1;
                    slot.clear();
                }
                _ => slot.clear(),
            }
        }
        if unclassified > 0 {
            XT_CYCLES_WITH_UNCLASSIFIED.fetch_add(1, Ordering::Relaxed);
        }
        // Publish this cycle's coverage where the SWEEP can read it. A looping
        // reproduction never reaches the shutdown summary these counters were
        // previously only visible in, and the question "did this sweep mark
        // from a complete root set?" has to be answerable per sweep.
        cratonvm_gc::gc_quiescence::publish_xt_pass(
            newly as u64,
            unclassified as u64,
            roots_this_pass as u64,
        );
        // Parked peers outlive this pass: hold the handler live for them
        // until `resume`, one count per take-over, taken while the pass's own
        // count still keeps them parked. (This stored a process-wide
        // `ACTIVE = false` when THIS take-over had frozen nobody, releasing any
        // other VM's parked peers; `RANGES_LEN` is no longer zeroed either --
        // every pass republishes before it signals, and the handler reads the
        // table only for an ARMED slot.)
        if !taken.tids.is_empty() && !taken.holds_session {
            ACTIVE_SESSIONS.fetch_add(1, Ordering::AcqRel);
            taken.holds_session = true;
        }
        if dbg() {
            eprintln!(
                "[xt-jit-roots] linux pass: signaled {examined} peer(s), {newly} newly taken over; {} code ranges; any_thread_in_jit={} jit_gate={} rejected_indirect={} bandless_windows={} jit_only_skipped={} skip_span_roots={:?} in_syscall_skipped={in_syscall_skipped} masked_skipped={masked_skipped} lazy_skipped={lazy_skipped} gated={}",
                ranges.len(),
                crate::jit::conservative_roots::any_thread_in_jit(),
                cratonvm_jit::xt_jit_root_scan_enabled(),
                // gc-common w6-g: see the Windows arm's twin line.
                super::shadow_rejected_indirect_entries(),
                super::XT_TAKEOVER_SHADOW_BANDLESS.load(Ordering::Relaxed),
                // gcd d10/t: signals `CRATONVM_XT_TAKEOVER_SIGNAL_JIT_ONLY`
                // withheld, lifetime total.
                super::XT_JIT_ONLY_SIGNALS_SKIPPED.load(Ordering::Relaxed),
                // gcd d10/t: (cursor, unallocated, violations).
                super::skip_span_root_counts(),
                // JIT round 18 wave 10b: whether this pass gated its signals.
                !taken.signal_gates.is_empty(),
            );
        }
        newly
    }

    pub fn resume(mut taken: TakenOver) {
        resume_held(&mut taken);
    }

    /// [`resume`]'s body, once per take-over (also `TakenOver`'s `Drop`).
    pub(super) fn resume_held(taken: &mut TakenOver) {
        if std::mem::replace(&mut taken.resumed, true) {
            return;
        }
        // Release EVERY frozen peer first, then wait once, against one
        // deadline. This called `release_slot` per peer, and each call waits
        // up to 100 ms for that peer to be scheduled out of the handler -- so K
        // unscheduled peers added up to K x 100 ms to a pause the initiator
        // still owns (`complete_gc` runs after this). Clearing a slot whose peer
        // has not left yet is safe: the handler's exit is a CAS on its own
        // `PARKED` state guarded by its tid (gc-common w1-c, 2026-09-23).
        // JIT round 16 wave 14 (lane ffm3): a peer claimed in C was never
        // signalled by this take-over and owns no slot; it is released
        // through its state word.
        super::release_native_claims(taken);
        let slots: Vec<&'static LinuxSlot> = taken
            .tids
            .iter()
            .filter(|&&tid| !taken.is_native_claim(tid))
            .filter_map(|&tid| find_slot(tid))
            .collect();
        for slot in &slots {
            slot.release_resume();
        }
        let start = Instant::now();
        while slots
            .iter()
            .any(|s| s.state.load(Ordering::Acquire) == STATE_PARKED)
            && start.elapsed() < Duration::from_millis(100)
        {
            std::thread::yield_now();
        }
        for slot in &slots {
            slot.clear();
        }
        // Only THIS take-over's count (gc-common w9-g). The unconditional
        // `ACTIVE = false` / `HELPER_MODE = false` here also released every
        // other VM's parked peers, and ran even for an empty `taken`.
        if taken.holds_session {
            ACTIVE_SESSIONS.fetch_sub(1, Ordering::AcqRel);
        }
    }

    /// `unparked` (JIT round 18 wave 10, lane ffmeintr, EINTR2-1): the blocked
    /// peers inside a blocked-wait trampoline
    /// (`ThreadRegistry::blocked_wait_band_states`), read without a signal;
    /// empty unless `CRATONVM_XT_HELPER_UNPARKED_READ`.
    pub fn helper_window_pass<F>(
        taken: &TakenOver,
        is_obj: &F,
        roots: &mut Vec<ObjectRef>,
        blocked_os_tids: &[u32],
        unparked: &[(
            u32,
            std::sync::Arc<crate::threading::jvm_thread::GcBlockState>,
        )],
    ) -> (usize, usize)
    where
        F: Fn(usize) -> Option<ObjectRef>,
    {
        // Per-cycle verdict: open it before any early return.
        super::reset_helper_window_cycle();
        let tlab_slots = taken.tlab_pointer_slots().to_vec();
        // gcd d3/m: see the Windows arm -- the proven blocked-monitor peers
        // whose bands the young pin ledger needs, and whether pinned windows'
        // bands go into it (`CRATONVM_GEN_PINNED_YOUNG_COPY_TAKEOVER`).
        let ledger_only = cratonvm_gc::gc_quiescence::take_proven_monitor_peers_for_ledger();
        let band_capture = super::helper_window_band_capture_enabled();
        let ranges = crate::jit::jit_code_ranges_snapshot();
        if ranges.is_empty() || (blocked_os_tids.is_empty() && ledger_only.is_empty()) {
            if !ledger_only.is_empty() {
                // A credited peer's band was owed and cannot be read here.
                cratonvm_gc::gc_quiescence::mark_peer_band_capture_incomplete();
            }
            return (0, 0);
        }
        let young_range = cratonvm_gc::gc_quiescence::young_pin_range();
        let mut band_words: Vec<usize> = Vec::new();
        install_handler();
        // gc-common w9-g: a session for this pass (the handler answers only
        // while one is open), and helper slots instead of a process-wide
        // `HELPER_MODE` (see `LinuxSlot::helper`).
        let _session = PassSession::open();
        publish_ranges(&ranges);
        // Hoisted deliberately: `/proc/self/maps` is enormous in a process
        // holding thousands of thread stacks, and re-reading it per peer cost
        // this pass 119-268 s against a 120 s cap (measured 2026-09-08). It is
        // only ever an UPPER BOUND now — `read_self_memory` stops the walk at
        // the first unreadable page whatever this says. Re-read (gcd d10/t)
        // only for a parked peer whose `rsp` it has no region for: a stack
        // mapped after this snapshot, see `BlockedPeerRead::Unreadable`.
        let mut regions = readable_regions();
        // gci i1/fr (g3h item 1): read once per pass.
        let zero_depth_native = super::helper_window_zero_depth_native_enabled();

        let self_tid = gettid();
        // r9w2-vm2: the copy band for `snapshot_parked_slot`, sized ONCE, before
        // any peer is parked, to the most a band can ever be
        // (`readable_region_end_from_regions` caps `end` at
        // `rsp + MAX_STACK_SCAN`). `vec![0; n]` is a zeroed allocation, which
        // for 8 MiB is a fresh anonymous mapping: only the pages a copy
        // actually writes are ever touched. g1ep4: the one this thread's last
        // pass used, so those pages are not faulted in again every pause.
        let mut band: Vec<u64> = take_pass_band();
        // Force the one-time probe (it reads an env flag, which may allocate)
        // now, not inside the first peer's parked window.
        let _ = safe_self_read_available();
        // Classification runs after the release now, so this no longer has to
        // avoid growing; the reservation just saves reallocations.
        let mut candidates: Vec<ObjectRef> = Vec::with_capacity(4096);
        let mut windows = 0usize;
        let mut pinned_windows = 0usize;
        let mut unpinned_windows = 0usize;
        // gci i3/tk: unpinned windows whose band was not read whole.
        let mut unpinned_incomplete = 0usize;
        let mut found_total = 0usize;
        let mut examined = 0usize;
        let mut unclassified = 0usize;
        // Blocked peers this pass could not read (no free slot, or no answer
        // within the budget). Each one refuses the discharge, and each still
        // has its shadow stack marked: it is BLOCKED, so the window is stable
        // without the peer's cooperation (gc-common w1-c, 2026-09-23 -- they
        // used to be counted and then forgotten, so a cycle whose only unread
        // peer was one of these discharged and relocated).
        let mut unknown_peers = 0usize;
        let shadow_only = |tid: u32, roots: &mut Vec<ObjectRef>| -> usize {
            if !crate::jit::conservative_roots::xt_peer_shadow_scan_enabled() {
                return 0;
            }
            let before = roots.len();
            let _ = super::scan_peer_shadow_window(tid, is_obj, roots);
            roots.len() - before
        };
        // JIT round 17 wave 1 (lane eintr): one signal per OS thread. The
        // registry's blocked list is not de-duplicated, and a mounted virtual
        // thread shares its carrier's tid, so one OS thread could be parked,
        // scanned and counted twice (its roots appended twice, a second
        // wake of its blocking call). Sorted here, before any peer is parked.
        let mut blocked_once: Vec<u32> = blocked_os_tids.to_vec();
        blocked_once.sort_unstable();
        blocked_once.dedup();
        // JIT round 17 wave 6 (lane eintr2): read once per pass; see below.
        let mask_door = super::takeover_mask_door_enabled();
        // JIT round 18 wave 10 (lane ffmeintr, EINTR2-1): read once per pass.
        let unparked_audit = !unparked.is_empty() && super::unparked_read_audit_enabled();
        let mut unparked_reads = 0usize;
        // g1ep7: the words this pass classified, for the debug line below.
        let mut band_words_total = 0usize;
        // g1ep7 (`CRATONVM_XT_HELPER_PRESIGNAL`, default ON, `=0` signals each
        // peer when its turn comes): arm and signal every peer the loop below
        // would park BEFORE waiting on any, so the peers reach their handlers
        // together and the pass waits for the slowest instead of the sum. On a
        // loaded host a signalled peer takes milliseconds to be scheduled, and
        // every pause signals the same handful of blocked JVM threads (the
        // reference handler, the finalizer, the cleaner): about 10 ms of every
        // pause, 1.1-1.2 s of `G1epKeepRounds 10 -Xmx1g`'s wall at load 45.
        // A peer parked here waits in its handler (asleep) until the loop
        // reads and releases it, as one signalled in turn would. Skipped: the
        // peers the loop reads without a signal (unparked, masked); a peer
        // left without a slot, or gone, takes the loop's own path.
        let mut presignalled: Vec<(u32, &'static LinuxSlot, Option<Option<cratonvm_gc::shadow_stack::SlotBand>>)> =
            Vec::new();
        if super::helper_window_presignal_enabled() {
            for &tid in &blocked_once {
                if tid == self_tid
                    || taken.contains(tid)
                    || unparked.iter().any(|(t, _)| *t == tid)
                    || (mask_door && peer_blocks_takeover_signal(tid) == Some(true))
                {
                    continue;
                }
                let preread = helper_band_preread(tid);
                let Some(slot) = arm_slot(tid, true) else {
                    break;
                };
                if !send_takeover_signal(tid) {
                    slot.clear();
                    continue;
                }
                examined += 1;
                presignalled.push((tid, slot, preread));
            }
        }
        for &tid in &blocked_once {
            if tid == self_tid || taken.contains(tid) {
                continue;
            }
            let early = presignalled
                .iter()
                .position(|&(t, _, _)| t == tid)
                .map(|i| {
                    let (_, slot, preread) = presignalled.swap_remove(i);
                    (slot, preread)
                });
            // JIT round 17 wave 6 (lane eintr2; the opt-in
            // `CRATONVM_FFM_DOWNCALL_MASK_TAKEOVER_SIGNAL` arm only): a blocked
            // peer that blocks the take-over signal -- a JNIEnv call from C
            // inside a masked FFM downcall that blocked in a VM region --
            // cannot answer until its region ends, after this pause. Signalled,
            // it cost the pause the whole `peer_total_deadline_ms` budget and
            // was then counted exactly like this: unclassified, an unknown
            // peer that refuses the discharge, its shadow stack marked.
            if early.is_none() && mask_door && peer_blocks_takeover_signal(tid) == Some(true) {
                XT_PEERS_UNCLASSIFIED.fetch_add(1, Ordering::Relaxed);
                crate::jit::helper_window_refusal::note_unreadable_helper_peer(
                    "linux",
                    tid,
                    "signal-masked",
                );
                unclassified += 1;
                unknown_peers += 1;
                found_total += shadow_only(tid, roots);
                continue;
            }
            // JIT round 18 wave 10 (lane ffmeintr, EINTR2-1,
            // `CRATONVM_XT_HELPER_UNPARKED_READ`): a peer waiting inside a
            // blocked-wait trampoline is read WITHOUT a signal. Holding its
            // band keeps it inside the trampoline, so `[lo, stack top)` --
            // every frame above its wait, and the callee-saved registers of
            // those frames, spilled at `lo` -- is frozen while it is copied
            // (`jvm_thread::run_blocked_wait`). Copied with the parked read's
            // rules (`copy_peer_band`) and classified by the same code below;
            // no register file (the registers are the wait's own, which by
            // the trampoline's contract hold no Java reference). A peer that
            // has left its trampoline, or whose stack this pass's maps
            // snapshot does not hold, is parked as before.
            let mut unparked_snap: Option<ParkedSnapshot> = None;
            let mut audit_copy: Option<(usize, Vec<u64>)> = None;
            if let Some((_, state)) = unparked.iter().find(|(t, _)| early.is_none() && *t == tid) {
                let published = cratonvm_gc::shadow_stack::thread_stack_band(tid);
                if let Some(lo) = state.try_hold_blocked_wait_band() {
                    let copied = peer_band_bound_published(published, lo, &regions, MAX_STACK_SCAN)
                        .map(|(end, clamped)| copy_peer_band(lo, end, clamped, &mut band));
                    state.release_blocked_wait_band(lo);
                    if let Some((words, complete)) = copied.filter(|&(w, _)| w > 0) {
                        if unparked_audit {
                            // Compare against a parked read of the same peer,
                            // and use the parked one.
                            audit_copy = Some((lo, band[..words].to_vec()));
                        } else {
                            unparked_reads += 1;
                            unparked_snap = Some(ParkedSnapshot {
                                tid,
                                regs: [0usize; REG_COUNT],
                                rsp: lo,
                                band_words: words,
                                band_complete: complete,
                            });
                        }
                    }
                }
            }
            let parked = if let Some(early) = early {
                Some(early)
            } else if unparked_snap.is_some() {
                None
            } else {
                // JIT round 18 wave 10 (lane ffmeintr): the published stack
                // band is read BEFORE the park (`peer_band_bound_published`).
                let preread = helper_band_preread(tid);
                let Some(slot) = arm_slot(tid, true) else {
                    // No free slot. This used to `break`, counting ONE unclassified
                    // peer for however many were left; every one of them is a
                    // blocked stack nobody read.
                    XT_PEERS_UNCLASSIFIED.fetch_add(1, Ordering::Relaxed);
                    crate::jit::helper_window_refusal::note_unreadable_helper_peer(
                        "linux", tid, "no-slot",
                    );
                    unclassified += 1;
                    unknown_peers += 1;
                    found_total += shadow_only(tid, roots);
                    continue;
                };
                examined += 1;
                if !send_takeover_signal(tid) {
                    // ESRCH: the thread exited between listing and signalling.
                    // Nothing to scan and nothing running — not a coverage hole.
                    slot.clear();
                    continue;
                }
                Some((slot, preread))
            };
            let answer = match parked {
                None => STATE_PARKED,
                Some((slot, _)) => {
                    let resignals_before = XT_PEER_RESIGNALS.load(Ordering::Relaxed);
                    let answer = wait_for_response_retrying(slot, tid);
                    // Any DEFINITIVE answer that needed a re-signal would have been an
                    // UNCLASSIFIED peer before the retry — `STATE_NOT_JIT` counts, it
                    // means the peer reached the handler with its `Rip` outside JIT
                    // code and is a cooperative barrier participant publishing its own
                    // roots. Counting only `STATE_PARKED` reported zero on a run where
                    // the retry had just converted sixteen.
                    if answer != STATE_CANCELLED
                        && XT_PEER_RESIGNALS.load(Ordering::Relaxed) != resignals_before
                    {
                        XT_PEERS_CLASSIFIED_AFTER_RETRY.fetch_add(1, Ordering::Relaxed);
                    }
                    answer
                }
            };
            match answer {
                STATE_PARKED => {
                    candidates.clear();
                    // r9w2-vm2: while parked, only COPY (registers + stack
                    // band into the pre-sized `band`) — then release, then
                    // classify over the copy. A helper slot parks a peer at
                    // whatever instruction the signal found it on, which can
                    // be inside `malloc` or holding a lock, and the classifier
                    // pushes into a `Vec` and takes the quiescence mutex
                    // (`record_peer_stack_slot`) for every object-looking
                    // word. The Windows arm has always resumed before
                    // classifying, for the reason its `helper_window_pass` doc
                    // gives ("a peer suspended mid-`malloc` ... could
                    // otherwise deadlock us on the allocator lock").
                    // What follows needs nothing the peer can change: it is
                    // BLOCKED, and `mark_blocked_region_leave` waits out the
                    // pause, so its stack words, shadow stack and chain depth
                    // are stable — the argument the depth read below already
                    // made, and the one the Windows shadow scan relies on
                    // after its resume.
                    let mut snap = match (unparked_snap.take(), parked) {
                        (Some(s), _) => s,
                        (None, Some((slot, preread))) => {
                            let s = snapshot_parked_slot_with(slot, &regions, &mut band, preread);
                            release_slot(slot);
                            s
                        }
                        (None, None) => continue,
                    };
                    if let Some((lo, copy)) = audit_copy.take() {
                        unparked_read_audit(tid, lo, &copy, &snap, &band, is_obj);
                    }
                    band_words_total += snap.band_words;
                    let (mut has_jit, mut complete, tail) = classify_parked_snapshot(
                        &snap,
                        &band,
                        &ranges,
                        is_obj,
                        &mut candidates,
                        &tlab_slots,
                    );
                    // gci i1/fr (g3h item 1): a published depth of zero demotes
                    // a window to the pure-native path (opt-in; census either
                    // way). Before the unreadable test, so a demoted EMPTY band
                    // is an unreadable peer, as any pure-native empty band is.
                    // gci i20/vh (opt-in): a deferred-chain peer's band too.
                    has_jit = super::helper_window_deferred_chain_step(
                        tid,
                        super::helper_window_after_depth_check(tid, has_jit, zero_depth_native),
                        taken.demotes_deferred_chain_window(tid),
                    );
                    // gci i2/fr (i1/fr review item 2): the captures and the
                    // window's census lines follow the step, as on Windows.
                    tail.finish(has_jit);
                    // gcd d10/t (`gcd-d9c-...` item 2): no band word read and
                    // no JIT address in the registers is an UNKNOWN peer, not
                    // a native one. The known cause is a stack mapped after
                    // the pass's maps snapshot, so re-read it (the peer is
                    // released: allocating is fine) and park the peer once
                    // more; it is blocked, so its compiled frames are where
                    // they were.
                    if super::blocked_peer_read(has_jit, snap.band_words)
                        == super::BlockedPeerRead::Unreadable
                    {
                        regions = readable_regions();
                        if let Some(again) = repark_helper_snapshot(tid, &regions, &mut band) {
                            candidates.clear();
                            snap = again;
                            let (again_jit, again_complete, again_tail) = classify_parked_snapshot(
                                &snap,
                                &band,
                                &ranges,
                                is_obj,
                                &mut candidates,
                                &tlab_slots,
                            );
                            complete = again_complete;
                            has_jit = super::helper_window_deferred_chain_step(
                                tid,
                                super::helper_window_after_depth_check(
                                    tid,
                                    again_jit,
                                    zero_depth_native,
                                ),
                                taken.demotes_deferred_chain_window(tid),
                            );
                            again_tail.finish(has_jit);
                        }
                    }
                    if super::blocked_peer_read(has_jit, snap.band_words)
                        == super::BlockedPeerRead::Unreadable
                    {
                        // Counted and covered like a peer that never answered
                        // (`STATE_CANCELLED` below): a refusal of the discharge,
                        // its shadow stack marked -- and its register
                        // candidates kept, which the Windows arm's empty band
                        // now keeps too. Over-retention only.
                        XT_PEERS_UNCLASSIFIED.fetch_add(1, Ordering::Relaxed);
                        super::XT_HELPER_WINDOW_BANDLESS_PEERS.fetch_add(1, Ordering::Relaxed);
                        crate::jit::helper_window_refusal::note_unreadable_helper_peer(
                            "linux",
                            tid,
                            "empty-band",
                        );
                        unclassified += 1;
                        unknown_peers += 1;
                        found_total += candidates.len();
                        roots.append(&mut candidates);
                        found_total += shadow_only(tid, roots);
                        if dbg() {
                            eprintln!(
                                "[xt-jit-roots] linux helper-window tid={tid}: no stack band readable (rsp={:#x}); counted unreadable",
                                snap.rsp
                            );
                        }
                    }
                    if has_jit {
                        windows += 1;
                        // JIT round 21 lane refs, REFS21-6 (opt-in
                        // `CRATONVM_XT_HELPER_WINDOW_PARK_CREDIT`): a peer in an
                        // armed Java-level park whose blocking deposit proved its
                        // compiled chain (the collector walks that chain this
                        // pause, marking AND pinning every frame word it names:
                        // `resolve_deferred_blocked_jit_chains`) takes no MARKING
                        // root from its register file and machine-stack words,
                        // only what the shadow-stack read below adds.
                        //
                        // JIT round 21 lane parkcredit: those words are still
                        // PINNED (unconditionally, whatever `complete` says).
                        // Dropping the pins too (the first cut) let a moving
                        // collection relocate every object the peer's Rust
                        // frames and registers name, live ones included: a
                        // callee-saved register, an uncaptured or saturated
                        // slot, a door's argument copy or an interior word is
                        // then stale at the wake, and only exact-base stack
                        // captures are repaired (registers never are). The
                        // G1 + `-Xmx64m` + GC-stress SIGSEGV of
                        // `R21RefsParkCreditStress` was ON-only. Pinned, an
                        // object the band names never moves, exactly as with
                        // the credit off; the credit changes only which objects
                        // are MARKED (a pin keeps a region / page out of the
                        // relocation set, it does not keep an unmarked object
                        // alive), which is all the stale-referent fix needs.
                        if cratonvm_gc::gc_quiescence::xt_park_credited(tid) {
                            if dbg() || crate::runtime::env_cache::dbg_referent_holder() {
                                // Every pinned-unrooted word, so a crash's
                                // `si_addr` / registers can be matched to the
                                // window: `rN=word` (register file) and
                                // `+off=word` (band, `off` above `rsp`), each
                                // `->base` when the probe resolved elsewhere.
                                // Bounded; diagnostic only.
                                let mut words = String::new();
                                let mut listed = 0usize;
                                for (ri, &v) in snap.regs.iter().enumerate() {
                                    if let Some(o) = is_obj(v) {
                                        if listed < 48 {
                                            let base = o.as_ptr() as usize;
                                            words.push_str(&format!(" r{ri}={v:#x}"));
                                            if base != v {
                                                words.push_str(&format!("->{base:#x}"));
                                            }
                                        }
                                        listed += 1;
                                    }
                                }
                                for (i, &w) in
                                    band[..snap.band_words.min(band.len())].iter().enumerate()
                                {
                                    let w = w as usize;
                                    if let Some(o) = is_obj(w) {
                                        if listed < 48 {
                                            let base = o.as_ptr() as usize;
                                            words.push_str(&format!(" +{:#x}={w:#x}", i * 8));
                                            if base != w {
                                                words.push_str(&format!("->{base:#x}"));
                                            }
                                        }
                                        listed += 1;
                                    }
                                }
                                eprintln!(
                                    "[xt-park-credit] gate=dropped tid={tid} rsp={:#x} band_words={} \
                                     dropped_candidates={} pinned_unrooted={listed} words:{words}",
                                    snap.rsp,
                                    snap.band_words,
                                    candidates.len()
                                );
                            }
                            let addrs: Vec<usize> =
                                candidates.iter().map(|o| o.as_ptr() as usize).collect();
                            cratonvm_gc::gc_quiescence::add_xt_cycle_pinned_jit_roots(&addrs);
                            candidates.clear();
                        }
                        // A JIT frame's oops live in the SHADOW STACK, which
                        // is not the machine stack and so is invisible to
                        // everything above. Scan it too, or the pin is
                        // incomplete and any coverage credited on it is a lie.
                        // An untrusted window refuses the pin rather than
                        // claiming coverage it does not have.
                        let shadow_ok =
                            if crate::jit::conservative_roots::xt_peer_shadow_scan_enabled() {
                                super::scan_peer_shadow_window(tid, is_obj, &mut candidates)
                                    .is_some()
                            } else {
                                true
                            };
                        found_total += candidates.len();
                        let roots_this_window = candidates.len();
                        // PIN, rather than refuse the whole cycle.
                        //
                        // This peer's conservative root set is COMPLETE -- the
                        // classifier read its register file and every readable
                        // word of its stack -- so nothing it can reach is
                        // missing from `candidates`. Pinning exactly those
                        // addresses is the same contract the cooperatively
                        // parked threads already get through
                        // `publish_pinned_jit_roots`: the objects do not move,
                        // their FIELDS are still rewritten through the pointer
                        // map, and everything else in the heap may relocate.
                        //
                        // Every peer here is BLOCKED (`blocked_os_tids`): it
                        // reached a blocking-region entry and deposited, but
                        // its deposit covers the compiled chain only, not the
                        // Rust frames around it that this band adds (corrected
                        // gci i24/jp; this said the peer reached neither
                        // deposit point, true of no peer this pass reads).
                        // The scan runs on the COLLECTOR's thread, so it cannot
                        // publish under the peer's `ThreadId` either; hence a
                        // per-cycle set.
                        if complete && shadow_ok && helper_window_pin_enabled() {
                            let addrs: Vec<usize> =
                                candidates.iter().map(|o| o.as_ptr() as usize).collect();
                            cratonvm_gc::gc_quiescence::add_xt_cycle_pinned_jit_roots(&addrs);
                            pinned_windows += 1;
                            // A pinned window covers the peer's WHOLE stack
                            // (`[rsp, stack_base)`) plus its register file, so
                            // every JIT frame it holds is immobile -- which is
                            // what the cross-thread coverage account wants to
                            // hear, and it wants to hear it as a DEPTH.
                            //
                            // Reading the peer's published depth after it has
                            // resumed is still exact: `mark_blocked_region_leave`
                            // waits out an active pause, so a blocked peer
                            // cannot run Java (and so cannot mutate its chain)
                            // between the block and the end of this STW.
                            //
                            // `None` -- a peer that never registered a slot --
                            // poisons the ledger rather than crediting zero.
                            if crate::jit::conservative_roots::xt_pinned_peer_depth_enabled() {
                                cratonvm_gc::gc_quiescence::add_xt_cycle_pinned_jit_depth(
                                    cratonvm_gc::gc_quiescence::jit_depth_of_tid(tid),
                                );
                            }
                            // gcd d3/m (`CRATONVM_GEN_PINNED_YOUNG_COPY_TAKEOVER`):
                            // the same whole band, every young word raw, into
                            // the young pin ledger with the peer's depth, so the
                            // pinned in-place copy can pin what it holds -- plus
                            // the window's resolved candidates (its shadow-stack
                            // objects among them).
                            if band_capture {
                                parked_snapshot_young_words(
                                    &snap,
                                    &band,
                                    &young_range,
                                    &mut band_words,
                                );
                                super::collect_young_band_words(
                                    &young_range,
                                    candidates.iter().map(|o| o.as_ptr() as usize),
                                    &mut band_words,
                                );
                                cratonvm_gc::gc_quiescence::note_peer_band_pin_words(
                                    &band_words,
                                    cratonvm_gc::gc_quiescence::jit_depth_of_tid(tid),
                                    true,
                                    true,
                                );
                            }
                        } else {
                            unpinned_windows += 1;
                            // gci i3/tk: see the Windows arm.
                            if !complete {
                                unpinned_incomplete += 1;
                            }
                            // gch h3/fd (d9c step 1): why, under `xt-refusal-census`.
                            crate::jit::helper_window_refusal::note_helper_window_refusal(
                                "linux",
                                tid,
                                helper_window_pin_enabled(),
                                shadow_ok,
                                complete,
                            );
                        }
                        taken.commit_window_roots(tid, &mut candidates, roots);
                        if dbg() {
                            // gcd d5/s: why a window refuses (`complete`,
                            // `shadow_ok`) and whether its depth can be
                            // credited (`depth=None` voids the whole credit).
                            eprintln!(
                                "[xt-jit-roots] linux helper-window tid={tid}: JIT frames on native stack (Rip outside JIT), {roots_this_window} conservative roots complete={complete} shadow_ok={shadow_ok} depth={:?}",
                                cratonvm_gc::gc_quiescence::jit_depth_of_tid(tid),
                            );
                        }
                    }
                }
                // `STATE_NOT_JIT` is a real answer: the peer reached the
                // handler and its `Rip` was outside JIT code, so it is a
                // cooperative barrier participant publishing its own precise
                // roots. `STATE_CANCELLED` is NOT an answer — the peer never
                // reached the handler, is STILL RUNNING, and its JIT-frame
                // oops are in no root set.
                STATE_CANCELLED => {
                    XT_PEERS_UNCLASSIFIED.fetch_add(1, Ordering::Relaxed);
                    crate::jit::helper_window_refusal::note_unreadable_helper_peer(
                        "linux",
                        tid,
                        "cancelled",
                    );
                    unclassified += 1;
                    unknown_peers += 1;
                    if let Some((slot, _)) = parked {
                        slot.clear();
                    }
                    found_total += shadow_only(tid, roots);
                }
                _ => {
                    if let Some((slot, _)) = parked {
                        slot.clear();
                    }
                }
            }
        }
        // gcd d3/m: the blocked-monitor peers credited by proof (see the
        // Windows arm): read each one's band into the young pin ledger through
        // the same helper-slot park as a window, never as a window. Anything
        // short of a whole band fails the ledger for this pause.
        for &(tid, depth) in &ledger_only {
            if tid == self_tid || taken.contains(tid) {
                cratonvm_gc::gc_quiescence::mark_peer_band_capture_incomplete();
                continue;
            }
            // JIT round 18 wave 10 (lane ffmeintr): before the park.
            let preread = helper_band_preread(tid);
            let Some(slot) = arm_slot(tid, true) else {
                cratonvm_gc::gc_quiescence::mark_peer_band_capture_incomplete();
                continue;
            };
            if !send_takeover_signal(tid) {
                slot.clear();
                cratonvm_gc::gc_quiescence::mark_peer_band_capture_incomplete();
                continue;
            }
            match wait_for_response_retrying(slot, tid) {
                STATE_PARKED => {
                    let snap = snapshot_parked_slot_with(slot, &regions, &mut band, preread);
                    release_slot(slot);
                    if snap.band_complete {
                        parked_snapshot_young_words(&snap, &band, &young_range, &mut band_words);
                        cratonvm_gc::gc_quiescence::note_peer_band_pin_words(
                            &band_words,
                            Some(depth),
                            true,
                            false,
                        );
                    } else {
                        cratonvm_gc::gc_quiescence::mark_peer_band_capture_incomplete();
                    }
                }
                _ => {
                    slot.clear();
                    cratonvm_gc::gc_quiescence::mark_peer_band_capture_incomplete();
                }
            }
        }
        if unclassified > 0 {
            XT_CYCLES_WITH_UNCLASSIFIED.fetch_add(1, Ordering::Relaxed);
        }

        // No `HELPER_MODE` / `ACTIVE` reset (gc-common w9-g): every helper slot
        // was released above, and `_session` closes this pass's count on
        // return.
        XT_HELPER_WINDOWS_SCANNED.fetch_add(windows as u64, Ordering::Relaxed);
        // THE DISCHARGE. A cycle whose helper windows were ALL pinned from a
        // complete scan -- and whose blocked peers were all readable -- may
        // relocate; anything else refuses with `XT_HELPER_WINDOW`.
        //
        // (gc-common w1-c, 2026-09-23: a 30-line comment stood here saying
        // "EVERY window still refuses, and the pins above do NOT discharge it",
        // because the probe was exact-base `is_object_address`. Both halves
        // stopped being true when `helper_window_discharge_enabled` and
        // `helper_window_pin_resolve_enabled` went default-on (2026-09-04 and
        // -08); the code below has discharged since, and the comment described
        // the opposite of what ran. The derived-pointer residue it worried
        // about is what `CRATONVM_XT_KEEP_UNREWRITABLE_ON_DISCHARGE` exists for,
        // at the second refusal site in `interpreter::gc_and_alloc`.)
        let refusing = unpinned_windows + unknown_peers;
        // Into THIS pause's ledger (gcd d2/i), not a process word.
        cratonvm_gc::gc_quiescence::set_xt_helper_windows_unpinned(refusing as u64);
        if (windows > 0 || unknown_peers > 0)
            && !(helper_window_discharge_enabled() && refusing == 0)
        {
            // gci i3/tk: see the Windows arm.
            super::mark_helper_window_refusal(taken, unpinned_incomplete, unknown_peers);
        }
        XT_HELPER_WINDOWS_PINNED.fetch_add(pinned_windows as u64, Ordering::Relaxed);
        XT_HELPER_WINDOWS_REFUSED.fetch_add(refusing as u64, Ordering::Relaxed);
        cratonvm_gc::gc_quiescence::publish_xt_helper_window(windows as u64, found_total as u64);
        if dbg() {
            eprintln!(
                "[xt-jit-roots] linux helper-window pass: examined {examined} blocked peer(s), {windows} window(s), {found_total} conservative root(s), {unknown_peers} unreadable (bandless_total={}) unparked_reads={unparked_reads} band_words={band_words_total}",
                super::XT_HELPER_WINDOW_BANDLESS_PEERS.load(Ordering::Relaxed),
            );
        }
        give_pass_band(band);
        (windows, found_total)
    }

    #[cfg(test)]
    mod parked_snapshot_tests {
        use super::*;

        /// r9w2-vm2: the parked-window COPY takes the register file and the
        /// whole readable band `[rsp, end)` into the caller's buffer, and the
        /// post-release CLASSIFY finds a JIT return address in the copy and an
        /// object in a register exactly as the old in-place classifier did.
        /// (No band word is reported as an object here: that would publish a
        /// stack-slot remap for this test's own stack into the process-global
        /// capture, which a concurrently running collection could act on.)
        #[test]
        fn parked_snapshot_copies_then_classifies_off_the_copy() {
            let stack: [u64; 6] = [0, 0x7000_0123, 0xdead_beef, 42, 0, 7];
            let rsp = stack.as_ptr() as usize;
            let regions = [(rsp, rsp + stack.len() * 8)];
            let slot = LinuxSlot::new();
            slot.tid.store(4242, Ordering::Release);
            slot.rsp.store(rsp, Ordering::Release);
            let obj_reg = 0x10_0008usize;
            slot.regs[3].store(obj_reg, Ordering::Release);

            let mut band = vec![0u64; 64];
            let snap = snapshot_parked_slot(&slot, &regions, &mut band);
            assert!(snap.band_complete);
            assert_eq!(snap.tid, 4242);
            assert_eq!(snap.rsp, rsp);
            assert_eq!(snap.band_words, stack.len());
            assert_eq!(&band[..stack.len()], &stack[..]);
            assert_eq!(snap.regs[3], obj_reg);

            let ranges = [(0x7000_0000usize, 0x7000_4000usize)];
            let is_obj = |a: usize| {
                if a == obj_reg {
                    // SAFETY: non-null, 8-aligned; never dereferenced.
                    Some(unsafe { ObjectRef::from_raw(a as *mut u8) })
                } else {
                    None
                }
            };
            let mut candidates = Vec::new();
            let (has_jit, complete, _) =
                classify_parked_snapshot(&snap, &band, &ranges, &is_obj, &mut candidates, &[]);
            assert!(
                has_jit,
                "the JIT return address in the copied band classifies"
            );
            assert!(complete);
            assert_eq!(candidates.len(), 1);
            assert_eq!(candidates[0].as_ptr() as usize, obj_reg);
        }

        /// A band too small for the stack is INCOMPLETE. The completeness flag
        /// used to be set unconditionally, so a stack deeper than the band was
        /// pinned as whole and could license a discharge with its outer frames
        /// unread (gc-common w1-c, 2026-09-23).
        #[test]
        fn parked_snapshot_that_fills_its_band_is_incomplete() {
            let stack: [u64; 6] = [1, 2, 3, 4, 5, 6];
            let rsp = stack.as_ptr() as usize;
            let regions = [(rsp, rsp + stack.len() * 8)];
            let slot = LinuxSlot::new();
            slot.tid.store(4243, Ordering::Release);
            slot.rsp.store(rsp, Ordering::Release);
            let mut band = vec![0u64; 4];
            let snap = snapshot_parked_slot(&slot, &regions, &mut band);
            assert_eq!(snap.band_words, 4);
            assert!(
                !snap.band_complete,
                "a band cut short by its buffer is not whole"
            );
            assert_eq!(std::hint::black_box(stack[5]), 6);
        }

        /// The take-over and helper caps are reported, not silently applied.
        #[test]
        fn region_end_says_when_the_cap_set_it() {
            let regions = [(0x1000usize, 0x9000usize)];
            assert_eq!(
                readable_region_end_from_regions(0x1000, &regions, 0x10_0000),
                Some((0x9000, false))
            );
            assert_eq!(
                readable_region_end_from_regions(0x1000, &regions, 0x1000),
                Some((0x2000, true))
            );
            assert_eq!(
                readable_region_end_from_regions(0x9000, &regions, 0x1000),
                None
            );
        }

        /// An `rsp` no readable region contains yields an INCOMPLETE snapshot:
        /// registers are still classified, the window is never pinned.
        #[test]
        fn parked_snapshot_without_a_region_is_incomplete() {
            let slot = LinuxSlot::new();
            slot.rsp.store(0x1000, Ordering::Release);
            let mut band = vec![0u64; 8];
            let snap = snapshot_parked_slot(&slot, &[], &mut band);
            assert!(!snap.band_complete);
            assert_eq!(snap.band_words, 0);
            let mut candidates = Vec::new();
            let (has_jit, complete, _) =
                classify_parked_snapshot(&snap, &band, &[], &|_| None, &mut candidates, &[]);
            assert!(!has_jit);
            assert!(!complete);
        }

        /// gcd d9/c: an INCOMPLETE band is still classified. A JIT return
        /// address among its copied words makes the peer a window, unpinned
        /// (`complete` false), where it used to be no window at all. (No word
        /// is reported as an object, for the reason the first test gives.)
        #[test]
        fn an_incomplete_band_still_classifies_its_copied_words() {
            let stack: [u64; 6] = [1, 0x7000_0123, 3, 4, 5, 6];
            let rsp = stack.as_ptr() as usize;
            let regions = [(rsp, rsp + stack.len() * 8)];
            let slot = LinuxSlot::new();
            slot.tid.store(4244, Ordering::Release);
            slot.rsp.store(rsp, Ordering::Release);
            let mut band = vec![0u64; 4];
            let snap = snapshot_parked_slot(&slot, &regions, &mut band);
            assert!(
                !snap.band_complete,
                "a band cut short by its buffer is not whole"
            );
            let ranges = [(0x7000_0000usize, 0x7000_4000usize)];
            let mut candidates = Vec::new();
            let (has_jit, complete, _) =
                classify_parked_snapshot(&snap, &band, &ranges, &|_| None, &mut candidates, &[]);
            assert!(
                has_jit,
                "the JIT return address in the copied words classifies"
            );
            assert!(!complete, "and the window stays unpinned");
            assert!(candidates.is_empty());
            assert_eq!(std::hint::black_box(stack[5]), 6);
        }

        /// gcd d9/c: a band that stops at the peer's PUBLISHED stack top is its
        /// whole stack, even when the readable region containing `rsp` runs
        /// past `rsp + MAX_STACK_SCAN` (a maps line that merged the stack with
        /// a readable neighbour). `clamped` used to be judged against the
        /// region's end alone, so such a band read incomplete on every pause.
        #[test]
        fn a_band_ending_at_the_published_top_is_whole_past_a_long_region() {
            struct Unpublish(u32);
            impl Drop for Unpublish {
                fn drop(&mut self) {
                    cratonvm_gc::shadow_stack::unpublish_thread_stack_band(self.0);
                }
            }
            // A tid no live thread has: the band table is process-wide.
            const FAKE_TID: u32 = u32::MAX - 0xd9c;
            cratonvm_gc::shadow_stack::publish_thread_stack_band(FAKE_TID);
            let _unpublish = Unpublish(FAKE_TID);
            let Some(own) = cratonvm_gc::shadow_stack::thread_stack_band(FAKE_TID) else {
                return; // this thread's stack cannot be introspected here
            };
            let local = [0u64; 4];
            let rsp = std::hint::black_box(local.as_ptr() as usize) & !7;
            if rsp < own.lo || rsp >= own.hi || own.hi - rsp > MAX_STACK_SCAN {
                return; // the published top would not decide the end
            }
            let regions = [(rsp, rsp.saturating_add(4 * MAX_STACK_SCAN))];
            let slot = LinuxSlot::new();
            slot.tid.store(FAKE_TID, Ordering::Release);
            slot.rsp.store(rsp, Ordering::Release);
            let mut band = vec![0u64; MAX_STACK_SCAN / 8];
            let snap = snapshot_parked_slot(&slot, &regions, &mut band);
            assert!(snap.band_words > 0);
            assert!(
                snap.band_complete,
                "the band ran to the published top, below the cap: it is whole"
            );
            assert_eq!(std::hint::black_box(local[0]), 0);
        }

        /// gcd d10/t (`gcd-d9c-...`, item 1): the take-over reader's band
        /// judgement is the helper reader's. With the peer's stack top
        /// published, a long readable region does not make the cap decide the
        /// end (`scan_slot_with_regions` used to count
        /// `XT_TAKEOVER_STACK_INCOMPLETE` here, on every take-over pause of
        /// such a peer); without a published top it does.
        #[test]
        fn the_takeover_band_is_whole_when_the_published_top_ends_it() {
            struct Unpublish(u32);
            impl Drop for Unpublish {
                fn drop(&mut self) {
                    cratonvm_gc::shadow_stack::unpublish_thread_stack_band(self.0);
                }
            }
            // Not the d9/c test's fake tid: the band table is process-wide and
            // the tests run in parallel.
            const FAKE_TID: u32 = u32::MAX - 0xd10;
            const UNPUBLISHED_TID: u32 = u32::MAX - 0xd11;
            cratonvm_gc::shadow_stack::publish_thread_stack_band(FAKE_TID);
            let _unpublish = Unpublish(FAKE_TID);
            let Some(own) = cratonvm_gc::shadow_stack::thread_stack_band(FAKE_TID) else {
                return; // this thread's stack cannot be introspected here
            };
            let local = [0u64; 4];
            let rsp = std::hint::black_box(local.as_ptr() as usize) & !7;
            let cap = MAX_TAKEOVER_STACK_SCAN;
            if rsp < own.lo || rsp >= own.hi || own.hi - rsp > cap {
                return; // the published top would not decide the end
            }
            let regions = [(rsp, rsp.saturating_add(4 * cap))];
            assert_eq!(
                peer_band_bound(FAKE_TID, rsp, &regions, cap),
                Some((own.hi, false)),
                "the published top ended the band below the cap: it is whole"
            );
            assert_eq!(
                peer_band_bound(UNPUBLISHED_TID, rsp, &regions, cap),
                Some((rsp + cap, true)),
                "no published top: the cap decided the end"
            );
            assert_eq!(peer_band_bound(FAKE_TID, rsp, &[], cap), None);
            assert_eq!(std::hint::black_box(local[0]), 0);
        }
    }
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
use imp::resume_held;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub use imp::{helper_window_pass, resume, take_over_pass, take_over_pass_split};

// ---------------------------------------------------------------------------
// Non-Windows stubs (the OS-suspend primitive is Windows-only here)
// ---------------------------------------------------------------------------

#[cfg(not(any(windows, all(target_os = "linux", target_arch = "x86_64"))))]
pub fn take_over_pass<F>(
    _taken: &mut TakenOver,
    _is_obj: &F,
    _roots: &mut Vec<ObjectRef>,
    _live_tids: &[u32],
) -> usize
where
    F: Fn(usize) -> Option<ObjectRef>,
{
    0
}

/// See the Windows / Linux `take_over_pass_split` (gci i1/fr).
#[cfg(not(any(windows, all(target_os = "linux", target_arch = "x86_64"))))]
pub fn take_over_pass_split<F, G>(
    _taken: &mut TakenOver,
    _is_obj: &F,
    _above: Option<&G>,
    _roots: &mut Vec<ObjectRef>,
    _live_tids: &[u32],
) -> usize
where
    F: Fn(usize) -> Option<ObjectRef>,
    G: Fn(usize) -> Option<ObjectRef>,
{
    0
}

#[cfg(not(any(windows, all(target_os = "linux", target_arch = "x86_64"))))]
pub fn resume(mut taken: TakenOver) {
    resume_held(&mut taken);
}

/// [`resume`]'s body, once per take-over (also `TakenOver`'s `Drop`).
#[cfg(not(any(windows, all(target_os = "linux", target_arch = "x86_64"))))]
fn resume_held(taken: &mut TakenOver) {
    if std::mem::replace(&mut taken.resumed, true) {
        return;
    }
    // JIT round 16 wave 14 (lane ffm3): nothing is suspended here, but a
    // claimed peer must still be handed back.
    release_native_claims(taken);
}

#[cfg(not(any(windows, all(target_os = "linux", target_arch = "x86_64"))))]
pub fn helper_window_pass<F>(
    _taken: &TakenOver,
    _is_obj: &F,
    _roots: &mut Vec<ObjectRef>,
    _blocked_os_tids: &[u32],
    _unparked: &[(
        u32,
        std::sync::Arc<crate::threading::jvm_thread::GcBlockState>,
    )],
) -> (usize, usize)
where
    F: Fn(usize) -> Option<ObjectRef>,
{
    (0, 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_obj(addr: usize) -> Option<ObjectRef> {
        // 8-aligned non-null address — the validator contract the real
        // `is_object_address` provides.
        if addr != 0 && addr % 8 == 0 {
            // SAFETY: non-null, 8-aligned; test-only value never dereferenced.
            Some(unsafe { ObjectRef::from_raw(addr as *mut u8) })
        } else {
            None
        }
    }

    /// gcd d2/i: a proven blocked monitor peer is credited only at the depth
    /// it published, and one whose depth disagrees goes back to the scan.
    #[test]
    fn proven_blocked_monitor_peers_are_credited_only_at_their_published_depth() {
        let depth_of = |tid: u32| match tid {
            2 => Some(3),
            4 => Some(6),
            _ => None,
        };
        let mut credited = Vec::new();
        let scan = credit_proven_blocked_monitor_peers(
            vec![1],
            &[(2, 3), (4, 5), (7, 1)],
            depth_of,
            |d| credited.push(d),
        );
        assert_eq!(
            credited,
            vec![3],
            "only the peer whose depth matches is credited"
        );
        assert_eq!(
            scan,
            vec![1, 4, 7],
            "the rest are scanned as helper windows"
        );
    }

    /// zt t26/zc5b item 1: exactly the published `Tlab` pointer words are
    /// skipped -- `[tlab, tlab + 24)` of each thread, the cursor / end / start
    /// slots -- and nothing beside them; `TakenOver` sorts what it is given,
    /// and an empty list (the switch off, a no-roots pause) skips nothing.
    #[test]
    fn zt26_zc5b_only_the_tlab_pointer_words_are_skipped() {
        let mut taken = TakenOver::default();
        assert!(!super::is_tlab_pointer_slot(
            taken.tlab_pointer_slots(),
            0x1000
        ));
        taken.set_tlab_pointer_slots(vec![(0x9000, 0x9018), (0x1000, 0x1018)]);
        let slots = taken.tlab_pointer_slots();
        assert_eq!(slots, &[(0x1000, 0x1018), (0x9000, 0x9018)]);
        for at in [0x1000, 0x1008, 0x1010, 0x9000, 0x9010] {
            assert!(super::is_tlab_pointer_slot(slots, at), "{at:#x}");
        }
        for at in [0x0ff8, 0x1018, 0x5000, 0x8ff8, 0x9018] {
            assert!(!super::is_tlab_pointer_slot(slots, at), "{at:#x}");
        }
        // JIT round 24 A1: the band walks' forward cursor answers exactly as
        // the binary search, word by word upward.
        let mut cursor = super::TlabSlotCursor::new(slots);
        for at in (0x0f00..0x9100).step_by(8) {
            assert_eq!(
                cursor.hit(at),
                super::is_tlab_pointer_slot(slots, at),
                "{at:#x}"
            );
        }
        assert!(!super::TlabSlotCursor::new(&[]).hit(0x1000));
        assert!(super::skip_tlab_pointer_slots_enabled(), "default ON");
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_XT_SKIP_TLAB_POINTER_SLOTS", Some("0"))],
            || assert!(!super::skip_tlab_pointer_slots_enabled()),
        );
        // `cratonvm_gc::Tlab`'s JIT contract: cursor, end, start at 0, 8, 16
        // -- the three words `ThreadRegistry::tlab_pointer_slots` spans.
        assert_eq!(cratonvm_gc::Tlab::CURSOR_OFFSET, 0);
        assert_eq!(cratonvm_gc::Tlab::END_OFFSET, 8);
    }

    /// zt t26/zc5b item 1: the registry spans every published TLAB address
    /// and nothing for a thread that published none.
    #[test]
    fn zt26_zc5b_the_registry_spans_each_published_tlab() {
        let shared = crate::vm::SharedVm::new(crate::config::VmConfig::default());
        let registry = &shared.threads.thread_registry;
        let before = registry.tlab_pointer_slots().len();
        let tid = crate::ThreadId(26_026);
        registry.register(tid, "zc5b-tlab", None);
        registry.set_tlab_addr(tid, 0x4_0000);
        let after = registry.tlab_pointer_slots();
        assert_eq!(after.len(), before + 1);
        assert!(after.contains(&(0x4_0000, 0x4_0018)));
        registry.clear_tlab_addr(tid);
        assert_eq!(registry.tlab_pointer_slots().len(), before);
    }

    /// gcd d3/m: a band read into the young pin ledger keeps every word in a
    /// young region RAW (a base, an interior word, one-past-end, the low
    /// slack) and nothing else, in order, appended to what is there.
    #[test]
    fn a_band_keeps_every_young_word_raw_and_nothing_else() {
        const BASE: usize = 0x7000_0000;
        const END: usize = 0x7010_0000;
        let range = cratonvm_gc::gc_quiescence::YoungPinRange::from_regions([(BASE, END), (0, 0)]);
        let slack = cratonvm_gc::gc_quiescence::YOUNG_PIN_LOW_SLACK;
        let mut out = vec![1usize];
        collect_young_band_words(
            &range,
            [
                0,
                BASE,
                BASE + 0x1003,
                42,
                END,
                END + 8,
                BASE - slack,
                BASE - slack - 8,
            ],
            &mut out,
        );
        assert_eq!(out, vec![1, BASE, BASE + 0x1003, END, BASE - slack]);
        let empty = cratonvm_gc::gc_quiescence::YoungPinRange::from_regions([(0, 0), (0, 0)]);
        let mut out = Vec::new();
        collect_young_band_words(&empty, [BASE, BASE + 8], &mut out);
        assert!(
            out.is_empty(),
            "no published young region: nothing is young"
        );
    }

    /// The per-pause helper-window verdict (gcd d2/i): an unbound test thread
    /// reads its own fallback ledger, so a refusal recorded here is read back
    /// here and the reset clears it.
    #[test]
    fn the_helper_window_verdict_reads_the_pause_ledger() {
        reset_helper_window_cycle();
        assert!(helper_windows_all_pinned_this_cycle());
        cratonvm_gc::gc_quiescence::set_xt_helper_windows_unpinned(2);
        assert_eq!(helper_windows_unpinned_this_cycle(), 2);
        assert!(!helper_windows_all_pinned_this_cycle());
        reset_helper_window_cycle();
        assert_eq!(helper_windows_unpinned_this_cycle(), 0);
    }

    /// A stack band with a JIT return address is classified as a helper
    /// window and its object-resolving words become candidate roots.
    #[test]
    fn helper_window_classifier_detects_jit_band_and_collects_candidates() {
        let ranges = [(0x7000_0000usize, 0x7000_4000usize)];
        // Two heap-shaped words; the validator accepts only 0x10_0008.
        let obj = 0x10_0008usize;
        let words = [
            0usize,
            0x7000_0123, // return address into the JIT range
            obj,
            0xdead_beef,
        ];
        let is_obj = |a: usize| if a == obj { fake_obj(a) } else { None };
        let mut candidates = Vec::new();
        let has_jit =
            classify_helper_window_words(words.iter().copied(), &ranges, &is_obj, &mut candidates);
        assert!(
            has_jit,
            "JIT return address on the band must classify as a helper window"
        );
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].as_ptr() as usize, obj);
    }

    /// A pure-native stack (no JIT return address anywhere) is NOT a helper
    /// window — the caller must discard its candidates (no blanket
    /// over-retention for threads that never ran JIT code).
    #[test]
    fn helper_window_classifier_rejects_band_without_jit_frames() {
        let ranges = [(0x7000_0000usize, 0x7000_4000usize)];
        let obj = 0x10_0008usize;
        let words = [obj, 0x12345usize, 0usize];
        let is_obj = |a: usize| if a == obj { fake_obj(a) } else { None };
        let mut candidates = Vec::new();
        let has_jit =
            classify_helper_window_words(words.iter().copied(), &ranges, &is_obj, &mut candidates);
        assert!(!has_jit);
        // Candidates were still collected — the caller's `has_jit` gate is
        // what discards them.
        assert_eq!(candidates.len(), 1);
    }

    /// A frozen-in-JIT peer's shadow stack is read into the take-over root set.
    ///
    /// The layout is `ShadowStack`'s `#[repr(C)]` prefix (`top`, `end`,
    /// `base`), published under a tid no real thread in the test binary holds.
    /// Before gc-common w1-c the take-over pass never read this window at all,
    /// so the one oop here -- a value an outer compiled frame parked across a
    /// call into a spinning callee -- was in no root set.
    #[test]
    fn taken_over_peer_shadow_window_is_scanned() {
        #[repr(C)]
        struct FakeShadow {
            top: usize,
            end: usize,
            base: usize,
        }
        const TID: u32 = 0xFFFF_FE01;
        let obj = 0x10_0010usize;
        let buf: Box<[usize; 4]> = Box::new([0, obj, 0xdead_beef, 0]);
        let base = buf.as_ptr() as usize;
        let end = base + 4 * 8;
        // `top` covers three slots; the fourth is above the stack and must
        // not be read.
        let ss = FakeShadow {
            top: base + 3 * 8,
            end,
            base,
        };
        let ss_addr = &ss as *const FakeShadow as usize;
        cratonvm_gc::gc_quiescence::publish_self_shadow_addr(TID, ss_addr, base, end);
        let is_obj = |a: usize| if a == obj { fake_obj(a) } else { None };
        let mut roots = Vec::new();
        let n = scan_taken_over_peer_shadow(TID, &is_obj, &mut roots);
        let enabled = crate::jit::conservative_roots::xt_peer_shadow_scan_enabled();
        cratonvm_gc::gc_quiescence::unregister_jit_depth_slot(TID);
        if enabled {
            assert_eq!(n, 1);
            assert_eq!(roots.len(), 1);
            assert_eq!(roots[0].as_ptr() as usize, obj);
        } else {
            assert_eq!(
                n, 0,
                "the kill switch restores the pre-fix take-over root set"
            );
        }
        assert_eq!(std::hint::black_box(ss.top), base + 24);
    }

    /// A published triple that no longer matches the struct it names is
    /// refused rather than read -- the identity check the blocked-peer path
    /// already relied on, shared unchanged by the take-over path.
    #[test]
    fn taken_over_peer_shadow_window_refuses_a_stale_triple() {
        #[repr(C)]
        struct FakeShadow {
            top: usize,
            end: usize,
            base: usize,
        }
        const TID: u32 = 0xFFFF_FE02;
        let obj = 0x10_0018usize;
        let buf: Box<[usize; 2]> = Box::new([obj, obj]);
        let base = buf.as_ptr() as usize;
        let ss = FakeShadow {
            top: base + 16,
            end: base + 16,
            base,
        };
        // Published with a DIFFERENT end: the struct moved or was freed.
        cratonvm_gc::gc_quiescence::publish_self_shadow_addr(
            TID,
            &ss as *const FakeShadow as usize,
            base,
            base + 64,
        );
        let is_obj = |a: usize| if a == obj { fake_obj(a) } else { None };
        let mut roots = Vec::new();
        let n = scan_taken_over_peer_shadow(TID, &is_obj, &mut roots);
        cratonvm_gc::gc_quiescence::unregister_jit_depth_slot(TID);
        assert_eq!(n, 0);
        assert!(roots.is_empty());
        assert_eq!(std::hint::black_box(ss.top), base + 16);
    }

    /// gc-common w5-g (`ShadowOddLongProbe`): a peer's INDIRECT shadow entry
    /// is resolved only inside the stack band the peer published under the
    /// same tid. Inside it the frame word is a root; an odd primitive outside
    /// it (here: a readable heap word holding an object address) is not read;
    /// and with no band at all the entry is not read and the window is
    /// untrusted rather than claimed covered.
    #[test]
    fn peer_indirect_entries_resolve_only_inside_the_published_stack_band() {
        #[repr(C)]
        struct FakeShadow {
            top: usize,
            end: usize,
            base: usize,
        }
        const TID: u32 = 0xFFFF_FE03;
        let obj = 0x10_0020usize;
        let decoy = 0x10_0028usize;
        // The "peer's compiled frame": a word of THIS thread's stack, whose
        // band the test publishes under the fake tid.
        let mut frame = [obj, 0usize];
        let slot = std::hint::black_box(frame.as_mut_ptr()) as usize;
        // A readable word that is NOT stack, holding an object address: what
        // an odd primitive that happens to name mapped memory would reach.
        let heap: Box<[usize; 1]> = Box::new([decoy]);
        let heap_word = std::hint::black_box(heap.as_ptr()) as usize;
        let tag = cratonvm_gc::shadow_stack::ShadowStack::INDIRECT_TAG;
        let buf: Box<[usize; 2]> = Box::new([slot | tag, heap_word | tag]);
        let base = buf.as_ptr() as usize;
        let end = base + 16;
        let ss = FakeShadow {
            top: end,
            end,
            base,
        };
        let ss_addr = &ss as *const FakeShadow as usize;
        cratonvm_gc::gc_quiescence::publish_self_shadow_addr(TID, ss_addr, base, end);
        let is_obj = |a: usize| {
            if a == obj || a == decoy {
                fake_obj(a)
            } else {
                None
            }
        };

        // No band published: nothing indirect is read, the window is untrusted.
        let mut roots = Vec::new();
        assert!(read_peer_shadow_window(TID, &is_obj, &mut roots).is_none());
        assert!(roots.is_empty());

        // Band published: the stack slot resolves, the heap word does not.
        cratonvm_gc::shadow_stack::publish_thread_stack_band(TID);
        let mut roots = Vec::new();
        let got = read_peer_shadow_window(TID, &is_obj, &mut roots);
        let band_known = cratonvm_gc::shadow_stack::thread_stack_band(TID).is_some();
        cratonvm_gc::shadow_stack::unpublish_thread_stack_band(TID);
        cratonvm_gc::gc_quiescence::unregister_jit_depth_slot(TID);
        if band_known {
            assert_eq!(got, Some(2));
            let addrs: Vec<usize> = roots.iter().map(|r| r.as_ptr() as usize).collect();
            assert_eq!(
                addrs,
                vec![obj],
                "the decoy behind a non-stack odd word is never read"
            );
        }
        assert_eq!(std::hint::black_box(ss.top), end);
        let _ = std::hint::black_box(&mut frame);
        drop(heap);
    }

    /// rx27 (frame descriptor records): a peer window holding a descriptor
    /// record yields every home inside the published band as a root (and not
    /// the record's own words), and none without a band.
    #[test]
    fn rx27_peer_descriptor_record_homes_are_roots() {
        #[repr(C)]
        struct FakeShadow {
            top: usize,
            end: usize,
            base: usize,
        }
        const TID: u32 = 0xFFFF_FE13;
        let obj_a = 0x10_0040usize;
        let obj_b = 0x10_0048usize;
        // The peer's frame: four words of THIS thread's stack; rbp one past.
        let mut frame = [obj_b, 0usize, 0usize, obj_a];
        let lo = std::hint::black_box(frame.as_mut_ptr()) as usize;
        let rbp = lo + 32;
        // Homes rbp-8 (obj_a) and rbp-32 (obj_b).
        let packed = 1usize | (4usize << 16);
        let buf: Box<[usize; 3]> = Box::new([
            rbp | cratonvm_gc::shadow_stack::DESCRIPTOR_TAG,
            cratonvm_gc::shadow_stack::descriptor_header(2) as usize,
            packed,
        ]);
        let base = buf.as_ptr() as usize;
        let end = base + 24;
        let ss = FakeShadow {
            top: end,
            end,
            base,
        };
        let ss_addr = &ss as *const FakeShadow as usize;
        cratonvm_gc::gc_quiescence::publish_self_shadow_addr(TID, ss_addr, base, end);
        let is_obj = |a: usize| {
            if a == obj_a || a == obj_b {
                fake_obj(a)
            } else {
                None
            }
        };
        // No band: the record's homes are not read; the window is untrusted.
        let mut roots = Vec::new();
        assert_eq!(
            read_peer_shadow_window_ex(TID, &is_obj, &mut roots),
            PeerWindowRead::BandlessIndirect(3)
        );
        assert!(roots.is_empty());
        cratonvm_gc::shadow_stack::publish_thread_stack_band(TID);
        let mut roots = Vec::new();
        let got = read_peer_shadow_window(TID, &is_obj, &mut roots);
        let band_known = cratonvm_gc::shadow_stack::thread_stack_band(TID).is_some();
        cratonvm_gc::shadow_stack::unpublish_thread_stack_band(TID);
        cratonvm_gc::gc_quiescence::unregister_jit_depth_slot(TID);
        if band_known {
            assert_eq!(got, Some(3));
            let addrs: Vec<usize> = roots.iter().map(|r| r.as_ptr() as usize).collect();
            assert_eq!(addrs, vec![obj_a, obj_b], "both homes, in record order");
        }
        assert_eq!(std::hint::black_box(ss.top), end);
        let _ = std::hint::black_box(&mut frame);
    }

    /// gc-common w6-g: a TAKE-OVER window whose peer published no stack band
    /// (the only case on a target other than Windows/Linux) adds its value
    /// entries, never dereferences an indirect one, and withholds the pause's
    /// complete-pins claim through the pause ledger's stack-incomplete row
    /// (also counted in the process total `XT_TAKEOVER_STACK_INCOMPLETE`),
    /// which `stw_take_over_and_wait` reads into the `TakeoverVerdict`.
    #[test]
    fn a_bandless_takeover_window_keeps_values_and_withholds_complete_pins() {
        #[repr(C)]
        struct FakeShadow {
            top: usize,
            end: usize,
            base: usize,
        }
        const TID: u32 = 0xFFFF_FE04;
        let obj = 0x10_0030usize;
        let behind = 0x10_0038usize;
        // A readable word holding an object address, named by an indirect
        // entry: it must not be read without a band.
        let word: Box<[usize; 1]> = Box::new([behind]);
        let word_addr = std::hint::black_box(word.as_ptr()) as usize;
        let tag = cratonvm_gc::shadow_stack::ShadowStack::INDIRECT_TAG;
        let buf: Box<[usize; 2]> = Box::new([obj, word_addr | tag]);
        let base = buf.as_ptr() as usize;
        let end = base + 16;
        let ss = FakeShadow {
            top: end,
            end,
            base,
        };
        cratonvm_gc::gc_quiescence::publish_self_shadow_addr(
            TID,
            &ss as *const FakeShadow as usize,
            base,
            end,
        );
        assert!(cratonvm_gc::shadow_stack::thread_stack_band(TID).is_none());
        let is_obj = |a: usize| {
            if a == obj || a == behind {
                fake_obj(a)
            } else {
                None
            }
        };

        let mut roots = Vec::new();
        assert_eq!(
            read_peer_shadow_window_ex(TID, &is_obj, &mut roots),
            PeerWindowRead::BandlessIndirect(2)
        );
        let addrs: Vec<usize> = roots.iter().map(|r| r.as_ptr() as usize).collect();
        assert_eq!(
            addrs,
            vec![obj],
            "the value entry is a root; the indirect one is not read"
        );

        let incomplete_before = XT_TAKEOVER_STACK_INCOMPLETE.load(Ordering::Relaxed);
        let bandless_before = XT_TAKEOVER_SHADOW_BANDLESS.load(Ordering::Relaxed);
        // gce e1/t: the verdict reads THIS pause's ledger row, not the process
        // total, so the note must land in the initiator's bound ledger -- and
        // only there (a second VM's ledger stays untouched).
        let this_vm = std::sync::Arc::new(cratonvm_gc::gc_quiescence::PauseLedger::new());
        let other_vm = std::sync::Arc::new(cratonvm_gc::gc_quiescence::PauseLedger::new());
        let mut roots = Vec::new();
        let n = cratonvm_gc::gc_quiescence::with_pause_ledger(&this_vm, || {
            cratonvm_gc::gc_quiescence::reset_xt_cycle();
            scan_taken_over_peer_shadow(TID, &is_obj, &mut roots)
        });
        let enabled = crate::jit::conservative_roots::xt_peer_shadow_scan_enabled();
        cratonvm_gc::gc_quiescence::unregister_jit_depth_slot(TID);
        let row = |l: &std::sync::Arc<cratonvm_gc::gc_quiescence::PauseLedger>| {
            cratonvm_gc::gc_quiescence::with_pause_ledger(
                l,
                cratonvm_gc::gc_quiescence::xt_takeover_stack_incomplete_this_pause,
            )
        };
        if enabled {
            assert_eq!(n, 1);
            assert!(XT_TAKEOVER_STACK_INCOMPLETE.load(Ordering::Relaxed) > incomplete_before);
            assert!(XT_TAKEOVER_SHADOW_BANDLESS.load(Ordering::Relaxed) > bandless_before);
            assert_eq!(row(&this_vm), Some(1), "the initiator's own pause row");
        } else {
            assert_eq!(row(&this_vm), Some(0));
        }
        assert_eq!(row(&other_vm), Some(0), "another VM's pause never sees it");
        assert_eq!(std::hint::black_box(ss.top), end);
        drop(word);
    }

    /// Empty range set never classifies anything (JIT never emitted code).
    #[test]
    fn helper_window_classifier_empty_ranges() {
        let words = [0x7000_0123usize];
        let mut candidates = Vec::new();
        let has_jit =
            classify_helper_window_words(words.iter().copied(), &[], &|_| None, &mut candidates);
        assert!(!has_jit);
        assert!(candidates.is_empty());
    }

    /// gc-common w2-c: the take-over probe resolves a frozen register's DERIVED
    /// pointer -- a misaligned cursor into a `byte[]` and a one-past-the-end
    /// cursor -- to an address INSIDE the array, on Generational, whose
    /// `is_heap_addr` answers "aligned and in a region", not "in an object".
    ///
    /// Before w2-c the probe was exact-base only by default (both cursors ->
    /// `None`, the array in no root set), and with the opt-in it resolved `w`
    /// first, which for a one-past-the-end cursor answers `w` itself: the NEXT
    /// object's (or on G1, possibly the next region's) address.
    #[test]
    fn takeover_probe_resolves_derived_cursors_inside_their_array() {
        use crate::config::{GcAlgorithm, VmConfig};
        if !takeover_interior_enabled() {
            // `CRATONVM_XT_TAKEOVER_INTERIOR=0` in the test environment: the
            // kill switch restores the exact-base probe, nothing to check.
            return;
        }
        let shared = crate::vm::SharedVm::new(VmConfig {
            gc_algorithm: GcAlgorithm::Generational,
            ..VmConfig::default()
        });
        let heap = &shared.mem.heap;
        let arr = heap.alloc_array(
            crate::classloading::ClassId::new(0),
            crate::memory::heap::ArrayElementType::Byte,
            13,
        );
        let base = arr.as_ptr() as usize;
        let size = heap
            .walk_objects()
            .iter()
            .find(|(p, _)| *p as usize == base)
            .map(|&(_, s)| s)
            .expect("a freshly allocated array is walkable");
        assert!(size >= 16, "an array has a header");
        let inside = |w: usize| {
            let r = takeover_word_probe(heap, w)
                .unwrap_or_else(|| panic!("derived pointer {w:#x} resolved to nothing"));
            let a = r.as_ptr() as usize;
            assert!(
                a >= base && a < base + size,
                "derived pointer {w:#x} resolved to {a:#x}, outside [{base:#x}, {:#x})",
                base + size
            );
        };
        // The exact base is still the exact base.
        assert_eq!(
            takeover_word_probe(heap, base).map(|o| o.as_ptr() as usize),
            Some(base)
        );
        // A misaligned cursor into the body.
        inside(base + size - 3);
        // One past the end: the loop finished, the cursor is all that is left.
        // Allocate a neighbour so the common shape is exercised -- bump
        // allocation puts the next object's BASE exactly there, the exact probe
        // accepts it as that object, and only the companion names `arr`.
        let next = heap.alloc_array(
            crate::classloading::ClassId::new(0),
            crate::memory::heap::ArrayElementType::Byte,
            5,
        );
        let end = base + size;
        let probed = takeover_word_probe(heap, end);
        let companion = takeover_word_companion(heap, end, probed);
        let names_arr = |o: Option<ObjectRef>| {
            o.is_some_and(|o| {
                let a = o.as_ptr() as usize;
                a >= base && a < end
            })
        };
        assert!(
            names_arr(probed) || names_arr(companion),
            "a one-past-the-end cursor {end:#x} must root the array it walked \
             (probe {:?}, companion {:?}, neighbour at {:#x})",
            probed.map(|o| o.as_ptr()),
            companion.map(|o| o.as_ptr()),
            next.as_ptr() as usize
        );
        // A companion is only ever produced for an EXACT-base answer.
        assert!(takeover_word_companion(heap, base + size - 3, None).is_none());
        let _keep = (arr, next);
    }

    /// gc-common w3-g: the same two derived-pointer shapes through the
    /// HELPER-WINDOW probe (`common-w2c-helper-window-probe-misses-the-object-
    /// ending-at-a-cursor`). Before w3-g the pass probed with a bare
    /// `resolve_interior_for_pin`: a misaligned cursor resolved to nothing on
    /// Generational, and a one-past-the-end cursor named only the neighbour.
    #[test]
    fn helper_window_probe_resolves_derived_cursors_inside_their_array() {
        use crate::config::{GcAlgorithm, VmConfig};
        if !helper_window_pin_resolve_enabled() {
            // Kill switch set in the test environment: the pre-2026-09-08
            // probe, and no companion, by design.
            return;
        }
        let shared = crate::vm::SharedVm::new(VmConfig {
            gc_algorithm: GcAlgorithm::Generational,
            ..VmConfig::default()
        });
        let heap = &shared.mem.heap;
        let arr = heap.alloc_array(
            crate::classloading::ClassId::new(0),
            crate::memory::heap::ArrayElementType::Byte,
            13,
        );
        let base = arr.as_ptr() as usize;
        let size = heap
            .walk_objects()
            .iter()
            .find(|(p, _)| *p as usize == base)
            .map(|&(_, s)| s)
            .expect("a freshly allocated array is walkable");
        let end = base + size;
        let names_arr = |o: Option<ObjectRef>| {
            o.is_some_and(|o| {
                let a = o.as_ptr() as usize;
                a >= base && a < end
            })
        };
        // A misaligned cursor into the body resolves inside the array.
        assert!(
            names_arr(helper_window_word_probe(heap, end - 3)),
            "misaligned cursor {:#x} must resolve inside the array",
            end - 3
        );
        let next = heap.alloc_array(
            crate::classloading::ClassId::new(0),
            crate::memory::heap::ArrayElementType::Byte,
            5,
        );
        let probed = helper_window_word_probe(heap, end);
        let companion = helper_window_word_companion(heap, end, probed);
        assert!(
            names_arr(probed) || names_arr(companion),
            "a one-past-the-end cursor {end:#x} must root the array it walked \
             (probe {:?}, companion {:?}, neighbour at {:#x})",
            probed.map(|o| o.as_ptr()),
            companion.map(|o| o.as_ptr()),
            next.as_ptr() as usize
        );
        // No companion for a word the probe declined.
        assert!(helper_window_word_companion(heap, end - 3, None).is_none());
        let _keep = (arr, next);
    }

    /// gc-common w9-g (`common-c-linux-takeover-signals-every-thread-FIXED-20260929`): the
    /// Linux take-over signals the ROSTER by default, not every task of the
    /// process; the audit's task list replaces it only when armed. Either list
    /// is sorted and de-duplicated (a mounted virtual thread republishes its
    /// carrier's tid), and 0 (unpublished), the collector and an
    /// already-frozen peer are never signalled. Pure, so it runs on every host.
    #[test]
    fn linux_takeover_signals_the_deduplicated_roster_unless_audited() {
        let mut taken = TakenOver::default();
        taken.handles.push(0);
        taken.tids.push(41);
        let roster = [44, 0, 41, 43, 44, 42];
        assert_eq!(
            takeover_signal_candidates(&roster, None, 42, &taken),
            vec![43, 44],
            "roster only: sorted, one signal per OS thread, no 0, no self, no frozen peer"
        );
        // Audited: every task, including the ones off the roster (7 is a JIT
        // compiler worker, say) -- with the same three exclusions.
        let tasks = vec![7, 44, 42, 41, 43, 900];
        assert_eq!(
            takeover_signal_candidates(&roster, Some(tasks), 42, &taken),
            vec![7, 43, 44, 900]
        );
        // An empty roster signals nobody; nothing is invented.
        assert!(takeover_signal_candidates(&[], None, 42, &TakenOver::default()).is_empty());
        // A fresh take-over holds no Linux handler session.
        assert!(!TakenOver::default().holds_session);
        // The peers above are invented: nothing to hand back on drop.
        taken.resumed = true;
    }

    /// JIT round 19 residual (lane eintr19's page): a take-over dropped by an
    /// unwind before any `resume` hands its native claims back, once;
    /// a resumed one does nothing more on drop.
    #[test]
    fn an_unwind_out_of_a_pause_resumes_the_take_over_once() {
        use crate::threading::jvm_thread::{GcBlockState, LAZY_NATIVE_IN, LAZY_NATIVE_OFF};
        let state = std::sync::Arc::new(GcBlockState::new());
        state.enter_lazy_native(0x10_000, 0x20_000);
        assert!(state.try_claim_lazy_native().is_some());
        let held = std::sync::Arc::clone(&state);
        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let mut taken = TakenOver::default();
            taken.push_native_claim(4242, held);
            panic!("a collection unwinds before resume");
        }));
        assert!(unwound.is_err());
        assert_eq!(
            state.lazy_native.load(Ordering::Acquire),
            LAZY_NATIVE_IN,
            "the unwind handed the claim back"
        );
        assert!(!state.leave_lazy_native());
        assert_eq!(state.lazy_native.load(Ordering::Acquire), LAZY_NATIVE_OFF);
        // Normal path: `resume` releases, the drop inside it adds nothing.
        let mut taken = TakenOver::default();
        assert!(!taken.resumed);
        resume_held(&mut taken);
        assert!(taken.resumed);
        assert!(taken
            .claims_released
            .load(std::sync::atomic::Ordering::Acquire));
    }

    /// JIT round 16 wave 14 (lane ffm3): a peer claimed in C counts as held,
    /// is in the tid set every frozen-peer reader uses, is never signalled,
    /// and is released by `resume`.
    #[test]
    fn a_claimed_peer_is_held_like_a_frozen_one_and_released_by_resume() {
        use crate::threading::jvm_thread::{GcBlockState, LAZY_NATIVE_IN, LAZY_NATIVE_OFF};
        let state = std::sync::Arc::new(GcBlockState::new());
        state.enter_lazy_native(0x10_000, 0x20_000);
        assert!(state.try_claim_lazy_native().is_some());
        let mut taken = TakenOver::default();
        taken.push_native_claim(4242, std::sync::Arc::clone(&state));
        assert_eq!(taken.count(), 1);
        assert_eq!(taken.native_claim_count(), 1);
        assert!(taken.contains(4242));
        assert!(taken.is_native_claim(4242));
        assert!(
            takeover_signal_candidates(&[4242, 4343], None, 1, &taken) == vec![4343],
            "a claimed peer is never signalled"
        );
        resume(taken);
        assert_eq!(state.lazy_native.load(Ordering::Acquire), LAZY_NATIVE_IN);
        assert!(!state.leave_lazy_native());
        assert_eq!(state.lazy_native.load(Ordering::Acquire), LAZY_NATIVE_OFF);
    }

    #[test]
    fn a_conservative_native_claim_never_fabricates_a_jit_entry() {
        let src = include_str!("xt_root_scan.rs");
        let push = &src[src.find("pub(crate) fn push_native_claim(").unwrap()..];
        let push = &push[..push.find("\n    \u{7d}\n").unwrap()];
        assert!(!push.contains("gc_quiescence::enter("));
        let release = &src[src
            .find("fn release_native_claims(taken: &TakenOver)")
            .unwrap()..];
        let release = &release[..release.find("\n\u{7d}\n").unwrap()];
        assert!(!release.contains("gc_quiescence::leave("));
        std::thread::spawn(|| {
            use crate::threading::jvm_thread::{GcBlockState, LAZY_NATIVE_IN};
            let state = std::sync::Arc::new(GcBlockState::new());
            state.enter_lazy_native(0x10_000, 0x20_000);
            assert!(state.try_claim_lazy_native().is_some());
            let mut taken = TakenOver::default();
            taken.push_native_claim(4242, std::sync::Arc::clone(&state));
            assert_eq!(taken.unrewritable_count(), 1);
            resume(taken);
            assert_eq!(state.lazy_native.load(Ordering::Acquire), LAZY_NATIVE_IN);
            assert!(!state.leave_lazy_native());
        })
        .join()
        .unwrap();
    }
    #[test]
    fn native_claim_shadow_requires_the_whole_identity_checked_window() {
        #[repr(C)]
        struct FakeShadow {
            top: usize,
            end: usize,
            base: usize,
        }
        const TID: u32 = 0xFFFF_FD43;
        let obj = 0x10_0010usize;
        let buf = Box::new([obj, 0usize]);
        let base = buf.as_ptr() as usize;
        let mut shadow = FakeShadow {
            top: base + 8,
            end: base + 16,
            base,
        };
        cratonvm_gc::gc_quiescence::publish_self_shadow_addr(
            TID,
            &shadow as *const FakeShadow as usize,
            base,
            base + 16,
        );
        let mut roots = Vec::new();
        if crate::jit::conservative_roots::xt_peer_shadow_scan_enabled() {
            assert_eq!(
                scan_native_claim_shadow(TID, &fake_obj, &mut roots),
                Some(1)
            );
            assert_eq!(roots[0].as_ptr() as usize, obj);
            shadow.top = base + 24;
            std::hint::black_box(&shadow);
            assert_eq!(
                scan_native_claim_shadow(TID, &fake_obj, &mut roots),
                None,
                "a corrupt top must not buy a pin credit"
            );
            shadow.top = base + 8;
            shadow.base = base + 8;
            std::hint::black_box(&shadow);
            assert_eq!(
                scan_native_claim_shadow(TID, &fake_obj, &mut roots),
                None,
                "a stale window must not buy a pin credit"
            );
        }
        cratonvm_gc::gc_quiescence::unregister_jit_depth_slot(TID);
        assert_eq!(
            scan_native_claim_shadow(TID, &fake_obj, &mut roots),
            None,
            "unpublished is unknown, not empty"
        );
    }

    #[test]
    fn pinned_native_claim_deposits_without_a_frozen_peer_or_fake_jit_entry() {
        use crate::threading::jvm_thread::{GcBlockState, LAZY_NATIVE_IN};
        let state = std::sync::Arc::new(GcBlockState::new());
        state.enter_lazy_native(0x10_000, 0x20_000);
        assert!(state.try_claim_lazy_native().is_some());
        let mut taken = TakenOver::default();
        taken.push_pinned_native_claim(4242, std::sync::Arc::clone(&state));
        assert_eq!(taken.count(), 1);
        assert_eq!(
            taken.unrewritable_count(),
            0,
            "complete deposit is not a frozen peer"
        );
        assert_eq!(taken.pinned_native_claim_count(), 1);
        assert!(state.lazy_native_precise_claim.load(Ordering::Acquire));
        assert!(taken.unrewritable_os_tids().is_empty());
        assert!(takeover_signal_candidates(&[4242, 4343], None, 1, &taken) == vec![4343]);
        // An actual frozen/conservative peer must still refuse the cycle.
        taken.tids.push(4343);
        taken.handles.push(0);
        assert_eq!(taken.unrewritable_count(), 1);
        assert_eq!(taken.unrewritable_os_tids(), vec![4343]);
        taken.tids.pop();
        taken.handles.pop();
        resume(taken);
        assert_eq!(state.lazy_native.load(Ordering::Acquire), LAZY_NATIVE_IN);
        assert!(!state.lazy_native_precise_claim.load(Ordering::Acquire));
        assert!(!state.leave_lazy_native());
    }
    #[test]
    fn a_native_band_must_prove_no_guardless_compiled_frame() {
        let band = [0usize, 0x7000_0123, 0x10_0010, usize::MAX];
        let lo = band.as_ptr() as usize;
        let hi = lo + std::mem::size_of_val(&band);
        let ranges = [(0x7000_0000, 0x7000_4000)];
        // SAFETY: a live local array; invalid bands below are never read.
        unsafe {
            assert_eq!(
                native_claim_band_has_compiled_word(lo, hi, &ranges),
                Some(true),
                "an empty entry chain cannot excuse this compiled return address"
            );
            assert_eq!(
                native_claim_band_has_compiled_word(lo, hi, &[(0x8000_0000, 0x8000_4000)]),
                Some(false)
            );
            assert_eq!(
                native_claim_band_has_compiled_word(lo + 4, hi, &ranges),
                None
            );
            assert_eq!(native_claim_band_has_compiled_word(lo, lo, &ranges), None);
        }
    }

    /// JIT round 16 wave 14 (lane ffm3): the claimed band is read word by
    /// word, every accepted word a root; an unusable band reads nothing.
    #[test]
    fn a_claimed_band_is_scanned_conservatively() {
        let band: [usize; 6] = [0, 0x5008, 7, 0x6010, 0x6011, usize::MAX];
        let lo = band.as_ptr() as usize;
        let hi = lo + std::mem::size_of_val(&band);
        let mut roots = Vec::new();
        // SAFETY: a live local array, read while it is in scope.
        let n = unsafe { scan_native_claim_band(lo, hi, &fake_obj, &mut roots) };
        let got: Vec<usize> = roots.iter().map(|r| r.as_ptr() as usize).collect();
        assert_eq!(got, vec![0x5008, 0x6010]);
        assert_eq!(n, 2);
        let mut none = Vec::new();
        // SAFETY: a refused band is not read.
        assert_eq!(
            unsafe { scan_native_claim_band(lo + 4, hi, &fake_obj, &mut none) },
            0
        );
        assert!(none.is_empty());
    }

    /// gce e2/t: `CRATONVM_DBG_XT_FORCE_TAKEOVER` declines one pause for at
    /// most `DECLINE_LIMIT` from its first decline, then parks; the next
    /// pause (a new barrier generation) opens a new window. So a forced run
    /// can be late but never hangs.
    #[test]
    fn force_takeover_declines_each_pause_for_a_bounded_window() {
        let t0 = std::time::Instant::now();
        let limit = ForceTakeover::DECLINE_LIMIT;
        let mut f = ForceTakeover::new();
        assert!(f.decline_at(7, t0), "first poll of a pause declines");
        assert!(f.decline_at(7, t0 + limit / 2), "inside the window");
        assert!(!f.decline_at(7, t0 + limit), "the window is closed: park");
        assert!(
            !f.decline_at(7, t0 + limit * 3),
            "and stays closed for that pause"
        );
        assert!(
            f.decline_at(8, t0 + limit * 3),
            "a new pause opens a new window"
        );
        assert!(f.decline_at(8, t0 + limit * 3 + limit / 2));
        assert!(!f.decline_at(8, t0 + limit * 4));
        // A clock that reads earlier than the window's start (another core's
        // `Instant`) is inside the window, not a panic.
        let mut g = ForceTakeover::new();
        assert!(g.decline_at(1, t0 + limit));
        assert!(g.decline_at(1, t0));
    }

    /// gcd d10/t (`common-c`): the opt-in narrowing withholds the signal only
    /// from a thread whose published JIT depth is exactly zero. An unknown
    /// depth (`None`: never published) is signalled, and order is kept.
    #[test]
    fn jit_only_signalling_skips_only_a_published_zero_depth() {
        let depth = |tid: u32| match tid {
            10 => Some(0),
            11 => Some(3),
            12 => None,
            13 => Some(0),
            _ => Some(1),
        };
        let (signal, skipped) = split_by_published_jit_depth(vec![10, 11, 12, 13, 14], depth);
        assert_eq!(signal, vec![11, 12, 14]);
        assert_eq!(skipped, vec![10, 13]);
        let (signal, skipped) = split_by_published_jit_depth(Vec::new(), depth);
        assert!(signal.is_empty() && skipped.is_empty());
        // Bounded per take-over: a fresh one starts counting its passes at 0
        // (`JIT_ONLY_SIGNAL_PASSES` narrowed passes, then the whole roster).
        assert_eq!(TakenOver::default().passes, 0);
    }

    /// JIT round 17 wave 1 (lane eintr): the three shapes the kernel writes to
    /// `/proc/<pid>/task/<tid>/syscall`, and garbage.
    #[test]
    fn eintr17_proc_syscall_lines_parse() {
        assert_eq!(parse_proc_syscall(b"running\n"), PeerSyscall::Running);
        assert_eq!(
            parse_proc_syscall(b"-1 0x7ffc1000 0x7f0011223344\n"),
            PeerSyscall::OffSyscall
        );
        // clock_nanosleep (230) as usleep makes it: six args, sp, pc.
        assert_eq!(
            parse_proc_syscall(b"230 0x0 0x0 0x7ffd5000 0x0 0x0 0x0 0x7ffd4f00 0x7f00aabbccdd\n"),
            PeerSyscall::InSyscall {
                pc: 0x7f00_aabb_ccdd
            }
        );
        let malformed: [&[u8]; 8] = [
            b"",
            b"\n",
            b"sleeping",
            b"230 0x0 0x0",
            b"230 0x0 0x0 0x0 0x0 0x0 0x0 0x1 zz",
            b"230 0x0 0x0 0x0 0x0 0x0 0x0 0x1 0x2 0x3",
            b"-1 0x1",
            b"\xff\xfe",
        ];
        for bad in malformed {
            assert_eq!(parse_proc_syscall(bad), PeerSyscall::Unknown, "{bad:?}");
        }
    }

    /// JIT round 17 wave 1 (lane eintr): only a peer blocked INSIDE a system
    /// call with its `Rip` outside every compiled range goes unsignalled.
    #[test]
    fn eintr17_only_a_peer_in_a_syscall_outside_compiled_code_is_not_signalled() {
        let ranges = [(0x1000usize, 0x2000usize), (0x5000, 0x6000)];
        let skip = |s| takeover_signal_skips_peer_in_syscall(s, &ranges);
        assert!(skip(PeerSyscall::InSyscall { pc: 0x7f00_0000 }));
        assert!(
            skip(PeerSyscall::InSyscall { pc: 0x2000 }),
            "one past a range is outside"
        );
        // A JIT-emitted syscall: the handler would park it, so it is signalled.
        assert!(!skip(PeerSyscall::InSyscall { pc: 0x1800 }));
        assert!(!skip(PeerSyscall::InSyscall { pc: 0x5000 }));
        assert!(!skip(PeerSyscall::InSyscall { pc: 0 }));
        // Running, blocked outside a syscall, unreadable: signalled as before.
        assert!(!skip(PeerSyscall::Running));
        assert!(!skip(PeerSyscall::OffSyscall));
        assert!(!skip(PeerSyscall::Unknown));
        // No compiled code at all: nothing can be in it.
        assert!(takeover_signal_skips_peer_in_syscall(
            PeerSyscall::InSyscall { pc: 0x1800 },
            &[]
        ));
    }

    /// JIT round 17 wave 6 (lane eintr2, `CRATONVM_XT_TAKEOVER_STAT_FALLBACK`):
    /// the state letter follows the LAST `)` of a `stat` line, whatever the
    /// command name holds; only `S` is a skippable sleep, and it is skipped
    /// with or without compiled code.
    #[test]
    fn eintr2_stat_state_letters_parse_and_only_a_sleep_is_skipped() {
        assert_eq!(
            parse_proc_stat_state(b"4242 (java) S 1 4242 0 0 -1\n"),
            Some(b'S')
        );
        assert_eq!(
            parse_proc_stat_state(b"77 (C2 Compiler) R 1 77\n"),
            Some(b'R')
        );
        // A name with spaces and parentheses of its own.
        assert_eq!(parse_proc_stat_state(b"9 (a) b (c)) D 1 9\n"), Some(b'D'));
        // Truncated right after the letter: still read.
        assert_eq!(parse_proc_stat_state(b"9 (x) t"), Some(b't'));
        let malformed: [&[u8]; 6] = [
            b"",
            b"9 (x)",
            b"9 (x) ",
            b"9 (x) SS 1",
            b"9 (x) 1 2",
            b"no paren S",
        ];
        for bad in malformed {
            assert_eq!(parse_proc_stat_state(bad), None, "{bad:?}");
        }
        assert_eq!(
            peer_syscall_from_stat_state(Some(b'S')),
            PeerSyscall::Sleeping
        );
        assert_eq!(
            peer_syscall_from_stat_state(Some(b'R')),
            PeerSyscall::Running
        );
        for other in [
            Some(b'D'),
            Some(b'T'),
            Some(b't'),
            Some(b'Z'),
            Some(b'I'),
            None,
        ] {
            assert_eq!(peer_syscall_from_stat_state(other), PeerSyscall::Unknown);
        }
        let ranges = [(0x1000usize, 0x2000usize)];
        assert!(takeover_signal_skips_peer_in_syscall(
            PeerSyscall::Sleeping,
            &ranges
        ));
        assert!(takeover_signal_skips_peer_in_syscall(
            PeerSyscall::Sleeping,
            &[]
        ));
        // The kernel's `syscall` parser never answers `Sleeping`.
        assert_ne!(parse_proc_syscall(b"S\n"), PeerSyscall::Sleeping);
    }

    /// JIT round 17 wave 6 (lane eintr2,
    /// `CRATONVM_JNI_ATTACH_UNBLOCK_TAKEOVER_SIGNAL`): the attach door takes
    /// the take-over signal out of the calling thread's mask; `=0` leaves the
    /// mask alone. Run on a fresh thread so no other test's mask changes.
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    #[test]
    fn eintr2_attach_unblocks_the_takeover_signal_of_the_calling_thread() {
        std::thread::spawn(|| {
            // SAFETY: zeroed `sigset_t`s initialised by `sigemptyset`;
            // `pthread_sigmask` touches only this thread.
            let blocked = || unsafe {
                let mut old: libc::sigset_t = std::mem::zeroed();
                libc::sigemptyset(&mut old);
                libc::pthread_sigmask(libc::SIG_BLOCK, std::ptr::null(), &mut old);
                libc::sigismember(&old, libc::SIGUSR2) == 1
            };
            unsafe {
                let mut set: libc::sigset_t = std::mem::zeroed();
                libc::sigemptyset(&mut set);
                libc::sigaddset(&mut set, libc::SIGUSR2);
                libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
            }
            assert!(blocked());
            cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_JNI_ATTACH_UNBLOCK_TAKEOVER_SIGNAL", Some("0"))],
                unblock_takeover_signal_on_attach,
            );
            assert!(blocked(), "=0 leaves the mask alone");
            unblock_takeover_signal_on_attach();
            assert!(!blocked(), "the attach unblocks the take-over signal");
        })
        .join()
        .expect("the attach thread");
    }

    /// JIT round 17 wave 6 (lane eintr2): the Linux arm asks `stat` only
    /// after an unreadable `syscall`, behind its switch. Source pin: the read
    /// needs a host without the file.
    #[test]
    fn eintr2_the_stat_fallback_runs_only_after_a_failed_syscall_read() {
        let src = include_str!("xt_root_scan.rs").replace("\r\n", "\n");
        let linux = src
            .split("// Linux x86-64 implementation (signal rendezvous)\n")
            .nth(1)
            .expect("the Linux arm's banner");
        let body = linux
            .split("fn peer_syscall_state(tid: u32) -> super::PeerSyscall {")
            .nth(1)
            .and_then(|s| s.split("fn peer_blocks_takeover_signal(").next())
            .expect("linux peer_syscall_state");
        let first = body.find("b\"/syscall\\0\"").expect("reads syscall");
        let gate = body
            .find("super::takeover_stat_fallback_enabled()")
            .expect("behind its switch");
        let second = body.find("b\"/stat\\0\"").expect("reads stat");
        assert!(first < gate && gate < second);
    }

    /// JIT round 17 wave 1 (lane eintr): the Linux take-over consults the
    /// kernel before it arms a slot, on every pass, behind its switch; and the
    /// helper-window pass signals each OS thread once. Source pins: the passes
    /// need live peers and signals to run.
    #[test]
    fn eintr17_the_linux_passes_skip_in_syscall_peers_and_dedupe_blocked_tids() {
        let src = include_str!("xt_root_scan.rs").replace("\r\n", "\n");
        let linux = src
            .split("// Linux x86-64 implementation (signal rendezvous)\n")
            .nth(1)
            .expect("the Linux arm's banner");
        let pass = linux
            .split("pub fn take_over_pass<F>(")
            .nth(1)
            .and_then(|s| s.split("pub fn resume(").next())
            .expect("linux take_over_pass");
        let check = pass
            .find("takeover_signal_skips_peer_in_syscall(peer_syscall_state(tid)")
            .expect("the pass reads the peer's syscall state");
        let arm = pass.find("arm_slot(tid, false)").expect("arms a slot");
        assert!(check < arm, "the kernel is read before any slot is armed");
        assert!(pass.contains("super::takeover_skip_in_syscall_enabled()"));
        let hw = linux
            .split("pub fn helper_window_pass<F>(")
            .nth(1)
            .expect("linux helper_window_pass");
        let dedup = hw.find("blocked_once.dedup();").expect("dedup");
        let first_arm = hw.find("arm_slot(tid, true)").expect("arms a helper slot");
        assert!(dedup < first_arm);
    }

    /// g1ep7 (`CRATONVM_XT_HELPER_PRESIGNAL`): the Linux helper-window pass
    /// signals its peers before it waits on the first one, behind its switch,
    /// and the loop takes a pre-armed slot instead of arming another. Source
    /// pins: the pass needs live blocked peers and signals to run.
    #[test]
    fn g1ep7_the_helper_window_pass_signals_every_peer_before_waiting() {
        let src = include_str!("xt_root_scan.rs").replace("\r\n", "\n");
        let linux = src
            .split("// Linux x86-64 implementation (signal rendezvous)\n")
            .nth(1)
            .expect("the Linux arm's banner");
        let hw = linux
            .split("pub fn helper_window_pass<F>(")
            .nth(1)
            .and_then(|s| s.split("\n    }\n").next())
            .expect("linux helper_window_pass");
        let gate = hw.find("super::helper_window_presignal_enabled()").expect("the switch");
        let signal = hw.find("presignalled.push((tid, slot, preread))").expect("the pre-signal");
        let wait = hw.find("wait_for_response_retrying(slot, tid)").expect("the wait");
        assert!(gate < signal && signal < wait, "every peer is signalled before the first wait");
        assert!(hw.contains("let parked = if let Some(early) = early {"));
    }

    /// JIT round 17 wave 2 (lane ffm3, EINTR17-1): the `SigBlk` line of a
    /// task's `status` names the blocked signals, bit `sig - 1`.
    #[test]
    fn ffm3_status_sigblk_lines_parse() {
        let usr2 = 12;
        let blocked = b"Name:\tjava\nSigQ:\t0/63432\nSigPnd:\t0000000000000000\n\
ShdPnd:\t0000000000000000\nSigBlk:\t0000000000000800\nSigIgn:\t0000000000001000\n";
        assert_eq!(parse_status_sigblk(blocked, usr2), Some(true));
        assert_eq!(parse_status_sigblk(blocked, 10), Some(false), "SIGUSR1");
        let open = b"SigPnd:\t0000000000000800\nSigBlk:\t0000000000000000\n";
        assert_eq!(
            parse_status_sigblk(open, usr2),
            Some(false),
            "pending is not blocked"
        );
        assert_eq!(
            parse_status_sigblk(b"SigBlk:\tfffffffe7ffbfeff", usr2),
            Some(true)
        );
        assert_eq!(parse_status_sigblk(b"Name:\tjava\n", usr2), None, "no line");
        assert_eq!(parse_status_sigblk(b"SigBlk:\tzz\n", usr2), None, "not hex");
        assert_eq!(parse_status_sigblk(b"SigBlk:\t\n", usr2), None, "empty");
        assert_eq!(parse_status_sigblk(blocked, 0), None, "no signal 0");
    }

    /// JIT round 17 wave 2 (lane ffm3, EINTR17-1): the Linux take-over reads
    /// the peer's mask after the syscall read and before it arms a slot, and
    /// hands the switch to its wait. Source pins.
    #[test]
    fn ffm3_the_linux_take_over_skips_masked_peers_before_arming() {
        let src = include_str!("xt_root_scan.rs").replace("\r\n", "\n");
        let linux = src
            .split("// Linux x86-64 implementation (signal rendezvous)\n")
            .nth(1)
            .expect("the Linux arm's banner");
        let pass = linux
            .split("pub fn take_over_pass<F>(")
            .nth(1)
            .and_then(|s| s.split("pub fn resume(").next())
            .expect("linux take_over_pass");
        let syscall = pass
            .find("takeover_signal_skips_peer_in_syscall(peer_syscall_state(tid)")
            .expect("the syscall read");
        let mask = pass
            .find("mask_door && peer_blocks_takeover_signal(tid) == Some(true)")
            .expect("the mask read");
        let arm = pass.find("arm_slot(tid, false)").expect("arms a slot");
        assert!(syscall < mask && mask < arm);
        assert!(pass.contains("wait_for_response_retrying_with(slot, tid, mask_door)"));
        assert!(pass.contains("super::takeover_mask_door_enabled()"));
    }

    /// gcd d10/t (`gcd-d9c-...` item 2): a blocked peer with no band word read
    /// and no JIT address in its registers is UNREADABLE, not native.
    #[test]
    fn a_blocked_peer_without_a_band_is_unreadable_not_native() {
        assert_eq!(blocked_peer_read(true, 0), BlockedPeerRead::Window);
        assert_eq!(blocked_peer_read(true, 12), BlockedPeerRead::Window);
        assert_eq!(blocked_peer_read(false, 12), BlockedPeerRead::NativeOnly);
        assert_eq!(blocked_peer_read(false, 0), BlockedPeerRead::Unreadable);
    }

    /// gcg g3/h (`CRATONVM_XT_HELPER_WINDOW_ROOT_STARTS`): only the entries
    /// from `from` on are judged; the rejected ones are reported in order and
    /// removed, the kept ones keep their order, and the prefix is untouched.
    #[test]
    fn gcg_g3h_root_starts_judge_only_the_helper_window_tail() {
        let addrs = [0x1000usize, 0x2008, 0x3000, 0x3010, 0x4000, 0x4018];
        let mut roots: Vec<ObjectRef> = addrs.iter().filter_map(|&a| fake_obj(a)).collect();
        assert_eq!(roots.len(), addrs.len());
        // "Starts" are the 0x1000-aligned addresses; 0x2008 sits in the prefix.
        let is_start = |a: usize| a & 0xfff == 0;
        let mut dropped = Vec::new();
        let n = retain_root_starts_from(&mut roots, 2, is_start, |a| dropped.push(a));
        assert_eq!(n, 2);
        assert_eq!(dropped, vec![0x3010, 0x4018]);
        let left: Vec<usize> = roots.iter().map(|o| o.as_ptr() as usize).collect();
        assert_eq!(left, vec![0x1000, 0x2008, 0x3000, 0x4000]);
    }

    /// gcg g3/h: an empty or out-of-range tail removes nothing and reports
    /// nothing; a tail with no start is emptied.
    #[test]
    fn gcg_g3h_root_starts_edge_tails() {
        let mut roots: Vec<ObjectRef> = [0x1008usize, 0x2008]
            .iter()
            .filter_map(|&a| fake_obj(a))
            .collect();
        let mut seen = 0usize;
        assert_eq!(
            retain_root_starts_from(&mut roots, 2, |_| false, |_| seen += 1),
            0
        );
        assert_eq!(
            retain_root_starts_from(&mut roots, 9, |_| false, |_| seen += 1),
            0
        );
        assert_eq!(seen, 0);
        assert_eq!(roots.len(), 2);
        assert_eq!(
            retain_root_starts_from(&mut roots, 0, |_| true, |_| seen += 1),
            0
        );
        assert_eq!(roots.len(), 2);
        assert_eq!(
            retain_root_starts_from(&mut roots, 0, |_| false, |_| seen += 1),
            2
        );
        assert_eq!(seen, 2);
        assert!(roots.is_empty());
    }

    /// gcg g3/h: the switch is applied by the helper-window caller AFTER the
    /// pins are taken, and only on Generational -- a source pin, because the
    /// G1 marking roots are also its pin set (`pin_frozen_peer_roots_for_g1`).
    #[test]
    fn gcg_g3h_root_starts_are_generational_only_and_never_touch_pins() {
        let src = include_str!("xt_root_scan.rs");
        let body = src
            .split("pub fn helper_window_keep_root_starts(")
            .nth(1)
            .expect("helper_window_keep_root_starts");
        let body = &body[..body.find("\n}\n").expect("end of fn")];
        assert!(
            body.contains("if !heap.is_generational()"),
            "Generational only"
        );
        assert!(
            !body.contains("add_xt_cycle_pinned_jit_roots"),
            "the starts filter narrows marking roots, never the pin set"
        );
    }

    /// gcg g5/xt: the windows-only capture switch was deleted on vg4's
    /// measurement (every repaired word came from a pure-native band); no
    /// reader of it may come back, and the recorder records every capture.
    #[test]
    fn gcg_g5xt_pure_native_band_captures_are_always_recorded() {
        let src = include_str!("xt_root_scan.rs");
        let prod = &src[..src.find("\nmod tests {").expect("tests module")];
        let key = concat!("CRATON", "VM_XT_HELPER_CAPTURE_WINDOWS_ONLY");
        assert!(
            !prod.contains(key),
            "a reader of the deleted switch is back"
        );
        let body = prod
            .split("fn record_helper_stack_captures(")
            .nth(1)
            .expect("record_helper_stack_captures");
        let body = &body[..body.find("\n}\n").expect("end of fn")];
        assert!(
            !body.contains("windows_only"),
            "no population filter in the recorder"
        );
        assert!(
            body.contains("for &(at, w) in pending"),
            "every capture is recorded"
        );
    }

    /// gcg g4/xt: `record_helper_stack_captures` is the ONLY production
    /// caller of `record_peer_stack_slot` in this file, so neither helper
    /// arm can capture a band word before `has_jit` is known again. A source
    /// pin (LF line endings assumed, as the g3/h pin).
    #[test]
    fn gcg_g4xt_helper_captures_have_one_recording_site() {
        let src = include_str!("xt_root_scan.rs");
        let prod = &src[..src.find("\nmod tests {").expect("tests module")];
        let needle = concat!("gc_quiescence::", "record_peer_stack_slot(");
        assert_eq!(prod.matches(needle).count(), 1, "one recording site");
        let body = prod
            .split("fn record_helper_stack_captures(")
            .nth(1)
            .expect("record_helper_stack_captures");
        let body = &body[..body.find("\n}\n").expect("end of fn")];
        assert!(
            body.contains(needle),
            "the one site is the deferred recorder"
        );
    }

    /// gcg g4/xt (`gcg-g3h-...-review-findings` item 4): both Linux arms
    /// record EVERY peer register through `record_peer_reg` before the object
    /// filter, as the Windows arms do -- the call also puts young-region
    /// register words (slack words included) into the young pin ledger. A
    /// source pin over the two Linux functions.
    #[test]
    fn gcg_g4xt_linux_arms_record_every_peer_register() {
        let src = include_str!("xt_root_scan.rs");
        let prod = &src[..src.find("\nmod tests {").expect("tests module")];
        for (fn_head, rec) in [
            (
                "fn scan_slot_with_regions<F, G>(",
                concat!("record_peer_reg(", "pair_tid, ri as u8, v)"),
            ),
            (
                "fn classify_parked_snapshot<F>(",
                concat!("record_peer_reg(", "pair_tid_hw, ri_hw as u8, v)"),
            ),
        ] {
            let body = prod.split(fn_head).nth(1).expect(fn_head);
            let at_rec = body.find(rec).expect("register record");
            let at_filter = body.find("is_obj(v)").expect("register filter");
            assert!(
                at_rec < at_filter,
                "{fn_head}: record before the object filter"
            );
        }
    }

    /// gcg g5/xt: the take-over's innermost-frame scope (g1/f) was deleted on
    /// lane jm's g5 verdict; no reader of its switch and no second probe for
    /// a frozen peer's stack may come back. (gci i1/fr's opt-in
    /// `take_over_pass_split` is not that scope: it splits at the TOP of the
    /// innermost compiled segment, giving the Rust frames above it the
    /// cooperative scans' starts rule; every compiled frame of the innermost
    /// segment keeps the full probe.)
    #[test]
    fn gcg_g5xt_takeover_probes_every_stack_word_alike() {
        let src = include_str!("xt_root_scan.rs");
        let prod = &src[..src.find("\nmod tests {").expect("tests module")];
        let key = concat!("CRATON", "VM_XT_TAKEOVER_INTERIOR_INNERMOST");
        assert!(
            !prod.contains(key),
            "a reader of the deleted switch is back"
        );
        assert!(
            !prod.contains(concat!("is_obj", "_outer")),
            "a second take-over probe is back"
        );
        assert!(!prod.contains(concat!("fn take_over_pass", "_scoped")));
    }

    /// gch h3/yc: the `[xt-frame]` census walks a frozen peer's RBP chain,
    /// innermost first, and stops at the first return into code that is not
    /// compiled, at a record that does not climb or leaves the band, and at
    /// its frame budget.
    #[test]
    fn gch_h3yc_frozen_frame_chain_follows_the_records_and_stops_where_it_must() {
        // Code: [0x1000, 0x2000) has 0x40-byte frames, [0x3000, 0x4000) 0x80.
        let size_at = |pc: usize| match pc {
            0x1000..=0x1fff => Some(0x40),
            0x3000..=0x3fff => Some(0x80),
            _ => None,
        };
        // Records: 0x7000 -> (0x7100, ret 0x3010) -> (0x7200, ret 0x1020)
        // -> (0x7300, ret 0x9000: not compiled).
        let records = |at: usize| match at {
            0x7000 => Some((0x7100, 0x3010)),
            0x7100 => Some((0x7200, 0x1020)),
            0x7200 => Some((0x7300, 0x9000)),
            _ => None,
        };
        let band = (0x6000, 0x8000);
        let frames = frozen_frame_chain(0x1004, 0x7000, band, 64, size_at, records);
        let got: Vec<(usize, usize, usize, usize)> = frames
            .iter()
            .map(|f| (f.depth, f.rbp, f.pc, f.lo))
            .collect();
        assert_eq!(
            got,
            vec![
                (0, 0x7000, 0x1004, 0x6fc0),
                (1, 0x7100, 0x3010, 0x7080),
                (2, 0x7200, 0x1020, 0x71c0)
            ],
        );
        // A frozen RIP outside compiled code: no frame at all.
        assert!(frozen_frame_chain(0x9000, 0x7000, band, 64, size_at, records).is_empty());
        // The budget.
        assert_eq!(
            frozen_frame_chain(0x1004, 0x7000, band, 2, size_at, records).len(),
            2
        );
        // A record outside the band, misaligned, or not climbing ends the walk.
        assert!(frozen_frame_chain(0x1004, 0x7ff8, band, 64, size_at, records).is_empty());
        assert!(frozen_frame_chain(0x1004, 0x7004, band, 64, size_at, records).is_empty());
        let falling = |at: usize| (at == 0x7000).then_some((0x6f00, 0x1010));
        assert_eq!(
            frozen_frame_chain(0x1004, 0x7000, band, 64, size_at, falling).len(),
            1
        );
    }

    /// gci i1/fr (`gcg-g3h-...` item 1): only a window whose peer published a
    /// depth of exactly zero is demoted; an unknown depth (`None`) and any
    /// positive depth keep it, and a non-window stays one.
    #[test]
    fn gci_i1fr_a_zero_depth_window_is_native_and_an_unknown_depth_is_not() {
        assert!(zero_depth_window_is_native(true, Some(0)));
        assert!(!zero_depth_window_is_native(true, None), "unknown depth keeps the window");
        assert!(!zero_depth_window_is_native(true, Some(1)));
        assert!(!zero_depth_window_is_native(true, Some(7)));
        assert!(!zero_depth_window_is_native(false, Some(0)), "a native band stays native");
        assert!(!zero_depth_window_is_native(false, None));
        // The arm's step: a non-window is never promoted, and a tid that never
        // published a depth keeps its window with the rule applied.
        // The never-published rule (CRATONVM_XT_HELPER_WINDOW_UNPUBLISHED_NATIVE) is default ON
        // since gci i4c; this test pins the zero-depth rule alone, so it kills that one.
        let up = concat!("CRATON", "VM_XT_HELPER_WINDOW_UNPUBLISHED_NATIVE");
        cratonvm_types::flags::with_thread_overrides(&[(up, Some("0"))], || {
            assert!(!helper_window_after_depth_check(0xFFFF_FF01, false, true));
            assert!(helper_window_after_depth_check(0xFFFF_FF01, true, true));
            assert!(helper_window_after_depth_check(0xFFFF_FF01, true, false));
        });
    }

    /// gci i20/vh: only a window whose peer's deposit deferred its chain is
    /// demoted; a native band stays native, and a window without a record
    /// keeps its window.
    #[test]
    fn gci_i20vh_a_deferred_chain_window_is_native_and_nothing_else_moves() {
        assert!(deferred_chain_window_is_native(true, true));
        assert!(!deferred_chain_window_is_native(true, false), "no record: the window stays");
        assert!(!deferred_chain_window_is_native(false, true), "a native band stays native");
        assert!(!deferred_chain_window_is_native(false, false));
        assert!(!helper_window_deferred_chain_step(0xFFFF_FD01, true, true));
        assert!(helper_window_deferred_chain_step(0xFFFF_FD01, true, false));
        assert!(!helper_window_deferred_chain_step(0xFFFF_FD01, false, true));
        assert!(!helper_window_deferred_chain_step(0xFFFF_FD01, false, false));
    }

    /// gci i20/vh: a default `TakenOver` demotes nothing; the driver's list is
    /// sorted and deduplicated and answered by membership.
    #[test]
    fn ki3_a_covered_peers_window_is_pinned_but_not_committed() {
        // SAFETY: never dereferenced; addresses only.
        let fake = |a: usize| unsafe { ObjectRef::from_raw(a as *mut u8) };
        let mut taken = TakenOver::default();
        let mut roots: Vec<ObjectRef> = Vec::new();
        // Nothing handed in: every window commits, as before.
        let mut c = vec![fake(0x1000), fake(0x2000)];
        taken.commit_window_roots(42, &mut c, &mut roots);
        assert!(c.is_empty());
        assert_eq!(roots.len(), 2);
        assert!(taken.take_pin_only_roots().is_empty());
        taken.set_covered_chain_peers(vec![42, 7, 42]);
        // A covered peer's words go aside, not into the marking roots.
        let mut c = vec![fake(0x3000)];
        taken.commit_window_roots(42, &mut c, &mut roots);
        assert!(c.is_empty());
        assert_eq!(roots.len(), 2);
        // Another peer still commits.
        let mut c = vec![fake(0x4000)];
        taken.commit_window_roots(8, &mut c, &mut roots);
        assert_eq!(roots.len(), 3);
        let aside = taken.take_pin_only_roots();
        assert_eq!(aside.len(), 1);
        assert_eq!(aside[0].as_ptr() as usize, 0x3000);
        assert!(taken.take_pin_only_roots().is_empty(), "taken once");
        // Never a demotion: the deferred-chain roster is separate.
        assert!(!taken.demotes_deferred_chain_window(42));
    }

    /// ki3: the switch is default ON, `=0` kills, and both arms commit a
    /// window through `commit_window_roots` (a source pin; LF assumed).
    #[test]
    fn ki3_the_covered_chain_switch_and_both_arms() {
        use cratonvm_types::flags::with_thread_overrides;
        let k = "CRATONVM_XT_HELPER_WINDOW_COVERED_CHAIN_PIN_ONLY";
        assert!(with_thread_overrides(
            &[(k, None)],
            helper_window_covered_chain_pin_only_enabled
        ));
        assert!(!with_thread_overrides(
            &[(k, Some("0"))],
            helper_window_covered_chain_pin_only_enabled
        ));
        let src = include_str!("xt_root_scan.rs");
        let commit = concat!("taken.commit_window_roots", "(tid, &mut candidates, roots)");
        assert_eq!(
            src.matches(commit).count(),
            2,
            "the Windows and the Linux arm"
        );
    }

    #[test]
    fn gci_i20vh_taken_over_answers_the_deferred_chain_peers_it_was_handed() {
        let mut taken = TakenOver::default();
        assert!(!taken.demotes_deferred_chain_window(0));
        assert!(!taken.demotes_deferred_chain_window(42));
        taken.set_deferred_chain_peers(vec![42, 7, 42, 19]);
        assert!(taken.demotes_deferred_chain_window(7));
        assert!(taken.demotes_deferred_chain_window(19));
        assert!(taken.demotes_deferred_chain_window(42));
        assert!(!taken.demotes_deferred_chain_window(8));
        taken.set_deferred_chain_peers(Vec::new());
        assert!(!taken.demotes_deferred_chain_window(42));
        // Nothing was suspended: dropping it resumes nothing.
        taken.resumed = true;
    }

    /// gci i24/jp (was `gci_i20vh_the_deferred_chain_switch_is_default_off`):
    /// the switch is default ON (`runtime_flag_default_on`): unset and `1` are
    /// ON, `0` is the kill.
    #[test]
    fn gci_i24jp_the_deferred_chain_switch_is_default_on() {
        use cratonvm_types::flags::with_thread_overrides;
        let k = concat!("CRATON", "VM_XT_HELPER_WINDOW_DEFERRED_CHAIN_NATIVE");
        assert!(with_thread_overrides(&[(k, None)], helper_window_deferred_chain_native_enabled));
        assert!(with_thread_overrides(&[(k, Some("1"))], helper_window_deferred_chain_native_enabled));
        assert!(!with_thread_overrides(&[(k, Some("0"))], helper_window_deferred_chain_native_enabled));
    }

    /// gci i24/jp: the default flip carries its greppable apps-triage marker
    /// on the reader, and the reader is the default-ON form (a source pin; LF
    /// line endings assumed).
    #[test]
    fn gci_i24jp_the_deferred_chain_flip_is_marked_on_its_reader() {
        let src = include_str!("xt_root_scan.rs");
        let prod = &src[..src.find("\nmod tests {").expect("tests module")];
        let head = concat!("pub fn helper_window_deferred_chain_native_enabled", "() -> bool {");
        let at = prod.find(head).expect("the reader");
        let body = &prod[at..at + prod[at..].find("\n}\n").expect("the reader's end")];
        assert!(body.contains(concat!("WARNING gci-i24jp-flip", "-20261015")), "{body}");
        assert!(body.contains(concat!("runtime_flag_default_on", "(")), "{body}");
        assert!(!body.contains(concat!("runtime_flag_on", "(")), "{body}");
    }

    /// gci i25/rv (`gci-i25rv-i24-default-path-review-findings-FIXED-20261016` item
    /// 9): the comments that say why the helper-window pass runs no longer
    /// restate the premise "a blocked peer's deposit never scans the JIT band"
    /// (false since `24cf0b19b0`; the i24/jp flip's soundness argument rests on
    /// the opposite), and the argument's point 2 names both families of
    /// blocking entries. Source pins, CRLF-tolerant.
    #[test]
    fn gci_i25rv_the_blocked_deposit_premise_is_stated_as_it_is() {
        let read = |rel: &str| {
            let path = format!("{}/{rel}", env!("CARGO_MANIFEST_DIR"));
            std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("{path}: {e}"))
                .replace("\r\n", "\n")
        };
        let own = read("src/jit/xt_root_scan.rs");
        assert!(!own.contains(concat!(
            "coverage gap (deposit_root_snapshot\n",
            "            // never scans the JIT band)"
        )));
        assert!(own.contains(concat!("entered by a flag-raising deposit in one", " of two families")));
        assert!(own.contains(concat!("`monitor_enter_synchronized_method_contended`", "),")));
        let registry = read("src/threading/thread_registry.rs");
        assert!(!registry.contains(concat!(
            "only by `deposit_root_snapshot` — which never scans the JIT band",
            " on"
        )));
        let gc = read("src/runtime/interpreter/gc_and_alloc.rs");
        assert!(!gc.contains(concat!(
            "covered only by its `deposit_root_snapshot`, which\n",
            "    // never scans JIT frames)"
        )));
        assert!(!gc.contains(concat!(
            "`CRATONVM_XT_HELPER_WINDOW_DEFERRED_CHAIN_NATIVE`, opt-in,\n",
            "        // G1 only)"
        )));
    }

    /// gci i20/vh: every helper-window depth step (one Windows site, two
    /// Linux sites) goes through the deferred-chain step with the take-over's
    /// answer for the same peer. A source pin (LF line endings assumed).
    #[test]
    fn gci_i20vh_every_helper_depth_step_applies_the_deferred_chain_step() {
        let src = include_str!("xt_root_scan.rs");
        let prod = &src[..src.find("\nmod tests {").expect("tests module")];
        let step = concat!("super::helper_window_deferred_chain_step", "(");
        assert_eq!(prod.matches(step).count(), 3, "one Windows site, two Linux sites");
        let answer = concat!("taken.demotes_deferred_chain_window", "(tid)");
        assert_eq!(prod.matches(answer).count(), 3);
        let check = concat!("super::helper_window_after_depth_check", "(");
        for (at, _) in prod.match_indices(step) {
            let tail = &prod[at..];
            let check_at = tail.find(check).expect("a depth check after the step");
            let answer_at = tail.find(answer).expect("the answer after the step");
            assert!(check_at < answer_at, "the depth check is the step's input");
        }
    }

    /// gci i1/fr: both helper-window arms run the depth check -- the Windows
    /// arm once, after its band loop and before the captures are recorded and
    /// the unreadable test; the Linux arm after each `classify_parked_snapshot`
    /// of the pass (the first read and the re-park) -- and the switch is read
    /// once per pass, not per peer. A source pin (LF line endings assumed).
    #[test]
    fn gci_i1fr_both_helper_arms_apply_the_zero_depth_check_before_the_unreadable_test() {
        let src = include_str!("xt_root_scan.rs");
        let prod = &src[..src.find("\nmod tests {").expect("tests module")];
        let call = concat!("super::helper_window_after_depth_check", "(");
        assert_eq!(prod.matches(call).count(), 3, "one Windows site, two Linux sites");
        let reader = concat!("super::helper_window_zero_depth_native_enabled", "()");
        assert_eq!(prod.matches(reader).count(), 2, "one read per arm's pass");
        // Windows: the check precedes the recorder and the unreadable test.
        let win = prod
            .split("mod imp {")
            .nth(1)
            .expect("the Windows arm")
            .split("pub fn helper_window_pass<F>(")
            .nth(1)
            .expect("its helper_window_pass");
        let at_check = win.find(call).expect("Windows check");
        let at_record = win.find("super::record_helper_stack_captures(tid, has_jit").expect("rec");
        let at_unreadable = win.find("super::blocked_peer_read(has_jit").expect("unreadable test");
        assert!(at_check < at_record && at_record < at_unreadable);
    }

    /// gci i2/fr (`gci-i1fr-...-review-findings` item 2, and item 1 of
    /// `gci-i2fr-...-review-findings`): the Linux classifier neither prints a
    /// census line nor records its captures; the pass does both through
    /// `ParkedBandTail::finish` after each depth step (first read, then the
    /// re-park) and before the unreadable test, as Windows does. A source pin
    /// (LF line endings assumed).
    #[test]
    fn gci_i2fr_the_linux_helper_census_prints_after_the_depth_step() {
        let src = include_str!("xt_root_scan.rs");
        let prod = &src[..src.find("\nmod tests {").expect("tests module")];
        let linux = prod.split("mod imp {").nth(2).expect("the Linux arm");
        let classify = linux
            .split("fn classify_parked_snapshot<F>(")
            .nth(1)
            .expect("the classifier")
            .split("\n    }\n")
            .next()
            .expect("its body");
        assert!(!classify.contains("eprintln!"), "the classifier prints no census");
        let recorder = concat!("record_helper_stack_captures", "(");
        assert!(!classify.contains(recorder), "the classifier records no capture");
        let call = concat!("super::helper_window_after_depth_check", "(");
        let finish = concat!("tail.finish", "(has_jit);");
        let checks: Vec<usize> = linux.match_indices(call).map(|(i, _)| i).collect();
        let finishes: Vec<usize> = linux.match_indices(finish).map(|(i, _)| i).collect();
        assert_eq!(checks.len(), 2, "two Linux depth steps");
        assert_eq!(finishes.len(), 2, "two Linux tails finished");
        assert!(checks[0] < finishes[0] && finishes[0] < checks[1] && checks[1] < finishes[1]);
        let unreadable = linux.find("super::blocked_peer_read(has_jit").expect("unreadable test");
        assert!(finishes[0] < unreadable, "the first tail finishes before the unreadable test");
    }

    /// gci i1/fr (`gcf-f7f-...` kind A): the innermost segment's top is the
    /// `rbp + 16` of the last frame `frozen_frame_chain` finds -- the same
    /// walk, with nothing collected -- and `None` where that walk finds none.
    #[test]
    fn gci_i1fr_the_innermost_segment_top_is_the_frame_chain_s_last_record() {
        let size_at = |pc: usize| match pc {
            0x1000..=0x1fff => Some(0x40),
            0x3000..=0x3fff => Some(0x80),
            _ => None,
        };
        let records = |at: usize| match at {
            0x7000 => Some((0x7100, 0x3010)),
            0x7100 => Some((0x7200, 0x1020)),
            0x7200 => Some((0x7300, 0x9000)),
            _ => None,
        };
        let band = (0x6000, 0x8000);
        for (rip, rbp, max) in [
            (0x1004, 0x7000, 64),
            (0x1004, 0x7000, 2),
            (0x1004, 0x7000, 1),
            (0x9000, 0x7000, 64),
            (0x1004, 0x7ff8, 64),
            (0x1004, 0x7004, 64),
            (0x1004, 0x5000, 64),
        ] {
            let chain = frozen_frame_chain(rip, rbp, band, max, size_at, records);
            let want = chain.last().map(|f| f.rbp + 16);
            assert_eq!(
                takeover_innermost_segment_top(rip, rbp, band, max, size_at, records),
                want,
                "rip={rip:#x} rbp={rbp:#x} max={max}"
            );
        }
        // The fixture's whole chain: three frames, the outermost at 0x7200.
        assert_eq!(
            takeover_innermost_segment_top(0x1004, 0x7000, band, 64, size_at, records),
            Some(0x7210)
        );
        // zwr: one frame (ZGC's split) is the innermost frame's top.
        assert_eq!(
            takeover_innermost_segment_top(0x1004, 0x7000, band, 1, size_at, records),
            Some(0x7010)
        );
        // A frozen `rip` outside compiled code: no segment, the full probe.
        assert_eq!(
            takeover_innermost_segment_top(0x9000, 0x7000, band, 64, size_at, records),
            None
        );
    }

    /// gci i1/fr: `take_over_pass` is `take_over_pass_split` with no second
    /// probe on both OS arms, and both stack loops pick the `above` probe only
    /// at or above the segment top they computed. Source pins (LF assumed).
    #[test]
    fn gci_i1fr_both_take_over_arms_split_only_above_the_innermost_segment() {
        let src = include_str!("xt_root_scan.rs");
        let prod = &src[..src.find("\nmod tests {").expect("tests module")];
        let delegate =
            concat!("take_over_pass_split::<F, F>(taken, is_obj, ", "None, roots, live_tids)");
        assert_eq!(prod.matches(delegate).count(), 2, "both arms delegate");
        assert_eq!(
            prod.matches(concat!("Some(from)) if p ", ">= from => above_probe(w)")).count(),
            1,
            "Windows stack loop"
        );
        assert_eq!(
            prod.matches(concat!("Some(from)) if at ", ">= from => above_probe(w)")).count(),
            1,
            "Linux stack loop"
        );
        // Registers are never split: both register loops still call `is_obj`.
        assert_eq!(prod.matches(concat!("super::takeover_innermost_segment_top", "(")).count(), 2);
        let probe = prod
            .split("pub(crate) fn takeover_above_chain_word_probe(")
            .nth(1)
            .expect("the above-chain probe");
        let probe = &probe[..probe.find("\n}\n").expect("end of fn")];
        assert!(probe.contains("if heap.is_g1()"), "G1 keeps the full probe");
        assert!(
            probe.contains("return takeover_word_probe(heap, w);"),
            "G1: the full probe"
        );
        assert!(
            probe.contains("return heap.is_object_address(w);"),
            "ZGC: exact bases"
        );
        assert!(!probe.contains("companion"), "no companion above the segment");
    }

    /// gci i1/fr: both new switches are default OFF (`runtime_flag_on`) and
    /// read `1` as on.
    #[test]
    fn gci_i1fr_the_zero_depth_and_above_chain_switches_are_default_off() {
        use cratonvm_types::flags::with_thread_overrides;
        let zd = concat!("CRATON", "VM_XT_HELPER_WINDOW_ZERO_DEPTH_NATIVE");
        let ac = concat!("CRATON", "VM_XT_TAKEOVER_ABOVE_CHAIN_STARTS");
        assert!(!with_thread_overrides(&[(zd, None)], helper_window_zero_depth_native_enabled));
        assert!(with_thread_overrides(&[(zd, Some("1"))], helper_window_zero_depth_native_enabled));
        assert!(!with_thread_overrides(&[(ac, None)], takeover_above_chain_starts_enabled));
        assert!(with_thread_overrides(&[(ac, Some("1"))], takeover_above_chain_starts_enabled));
    }

    /// zwr: a published floor inside the band ends it there; none, one at or
    /// below `rsp`, or one past the end leaves the whole band.
    #[test]
    fn zwr_a_rust_band_floor_caps_only_inside_the_band() {
        assert_eq!(cap_at_rust_band_floor(0x1000, 0x9000, 0x5000), 0x5000);
        assert_eq!(cap_at_rust_band_floor(0x1000, 0x9000, 0), 0x9000);
        assert_eq!(cap_at_rust_band_floor(0x1000, 0x9000, 0x1000), 0x9000);
        assert_eq!(cap_at_rust_band_floor(0x1000, 0x9000, 0x0800), 0x9000);
        assert_eq!(cap_at_rust_band_floor(0x1000, 0x9000, 0x9000), 0x9000);
        assert_eq!(cap_at_rust_band_floor(0x1000, 0x9000, 0xa000), 0x9000);
    }

    /// zwr: the ZGC arm of the above-chain rule is default ON and `=0` kills it.
    #[test]
    fn zwr_the_zgc_above_chain_switch_is_default_on() {
        use cratonvm_types::flags::with_thread_overrides;
        let z = concat!("CRATON", "VM_XT_TAKEOVER_ABOVE_CHAIN_STARTS_ZGC");
        assert!(with_thread_overrides(
            &[(z, None)],
            takeover_above_chain_starts_zgc_enabled
        ));
        assert!(!with_thread_overrides(
            &[(z, Some("0"))],
            takeover_above_chain_starts_zgc_enabled
        ));
    }

    /// gci i3/tk (`gci-i2yg-refusing-helper-windows-...` fix 2): only a
    /// window whose peer published neither a depth nor a shadow window, while
    /// every JIT entry publishes both, is demoted.
    #[test]
    fn gci_i3tk_an_unpublished_window_is_native_only_when_every_entry_publishes() {
        assert!(unpublished_window_is_native(true, None, false, true));
        assert!(!unpublished_window_is_native(true, None, false, false), "a kill switch: proves nothing");
        assert!(!unpublished_window_is_native(true, None, true, true), "a published window keeps it");
        assert!(!unpublished_window_is_native(true, Some(0), false, true), "zero is the other rule's");
        assert!(!unpublished_window_is_native(true, Some(3), false, true));
        assert!(!unpublished_window_is_native(false, None, false, true), "a native band stays native");
    }

    /// gci i3/tk: the arm step demotes a never-published peer only with the
    /// switch on (and only while every entry publishes), and keeps the window
    /// of a peer that published a shadow window.
    #[test]
    fn gci_i3tk_the_depth_step_applies_the_unpublished_rule_under_its_switch() {
        use cratonvm_types::flags::with_thread_overrides;
        let up = concat!("CRATON", "VM_XT_HELPER_WINDOW_UNPUBLISHED_NATIVE");
        const NEVER: u32 = 0xFFFF_FE21;
        const SHADOWED: u32 = 0xFFFF_FE22;
        // Default ON since gci i4c: the kill word is `=0`.
        let off = with_thread_overrides(&[(up, Some("0"))], || {
            helper_window_after_depth_check(NEVER, true, false)
        });
        assert!(off, "switch off: the window stays");
        let dflt = with_thread_overrides(&[(up, None)], || {
            helper_window_after_depth_check(NEVER, true, false)
        });
        assert_eq!(dflt, !jit_entry_publication_on(), "default ON: demoted iff publication is on");
        let on = with_thread_overrides(&[(up, Some("1"))], || {
            helper_window_after_depth_check(NEVER, true, false)
        });
        assert_eq!(on, !jit_entry_publication_on(), "switch on: demoted iff publication is on");
        let a = Box::new([0usize; 4]);
        let base = a.as_ptr() as usize;
        cratonvm_gc::gc_quiescence::publish_self_shadow_addr(SHADOWED, base, base, base + 32);
        let kept = with_thread_overrides(&[(up, Some("1"))], || {
            helper_window_after_depth_check(SHADOWED, true, false)
        });
        cratonvm_gc::gc_quiescence::unregister_jit_depth_slot(SHADOWED);
        assert!(kept, "a published shadow window keeps the window");
        let native = with_thread_overrides(&[(up, Some("1"))], || {
            helper_window_after_depth_check(NEVER, false, false)
        });
        assert!(!native, "a native band stays native");
    }

    /// gci i3/tk: both promotion licences are Generational only and need the
    /// derived-pointer probe of their population; the starts-only marking
    /// rules (and, for the take-over, a dead peer shadow scan) refuse.
    #[test]
    fn gci_i3tk_promotion_licences_are_generational_and_need_the_derived_pointer_probes() {
        assert!(takeover_promotes_licence(true, true, true, false, true));
        assert!(!takeover_promotes_licence(false, true, true, false, true), "G1 / ZGC");
        assert!(!takeover_promotes_licence(true, false, true, false, true), "switch off");
        assert!(!takeover_promotes_licence(true, true, false, false, true), "exact-base probe");
        assert!(!takeover_promotes_licence(true, true, true, true, true), "above-chain starts");
        assert!(!takeover_promotes_licence(true, true, true, false, false), "shadow scan killed");
        assert!(helper_refusal_promotes_licence(true, true, true, false));
        assert!(!helper_refusal_promotes_licence(false, true, true, false), "G1 / ZGC");
        assert!(!helper_refusal_promotes_licence(true, false, true, false), "switch off");
        assert!(!helper_refusal_promotes_licence(true, true, false, false), "pin resolve off");
        assert!(!helper_refusal_promotes_licence(true, true, true, true), "root starts");
        assert!(helper_window_refusal_relieved(true, 0, 0));
        assert!(!helper_window_refusal_relieved(false, 0, 0));
        assert!(!helper_window_refusal_relieved(true, 1, 0), "a cut band");
        assert!(!helper_window_refusal_relieved(true, 0, 1), "an unreadable peer");
    }

    /// gci i3/tk: the pause-wide licence. Every fact that leaves a word
    /// unread refuses; both switches off never keep promotion.
    #[test]
    fn gci_i3tk_a_pause_keeps_promotion_only_under_its_whole_licence() {
        let frozen = PausePromoteFacts {
            frozen: 1,
            takeover_licensed: true,
            ..PausePromoteFacts::default()
        };
        assert!(pause_keeps_promotion(&frozen));
        assert!(!pause_keeps_promotion(&PausePromoteFacts { takeover_licensed: false, ..frozen }));
        assert!(!pause_keeps_promotion(&PausePromoteFacts { stack_incomplete: true, ..frozen }));
        assert!(!pause_keeps_promotion(&PausePromoteFacts {
            takeover_shadow_untrusted: 1,
            ..frozen
        }));
        assert!(!pause_keeps_promotion(&PausePromoteFacts { unclassified: 1, ..frozen }));
        assert!(!pause_keeps_promotion(&PausePromoteFacts { native_claims: 1, frozen: 2, ..frozen }));
        // A frozen peer beside a refusing helper window: both licences needed.
        let both = PausePromoteFacts { helper_windows: 2, helper_refusals: 1, ..frozen };
        assert!(!pause_keeps_promotion(&both), "helper not licensed");
        assert!(!pause_keeps_promotion(&PausePromoteFacts { helper_licensed: true, ..both }));
        assert!(pause_keeps_promotion(&PausePromoteFacts {
            helper_licensed: true,
            helper_relieved: true,
            ..both
        }));
        // Helper windows alone.
        let hw = PausePromoteFacts {
            helper_windows: 3,
            helper_refusals: 3,
            helper_licensed: true,
            helper_relieved: true,
            ..PausePromoteFacts::default()
        };
        assert!(pause_keeps_promotion(&hw));
        assert!(!pause_keeps_promotion(&PausePromoteFacts { helper_relieved: false, ..hw }));
        assert!(!pause_keeps_promotion(&PausePromoteFacts { helper_licensed: false, ..hw }));
        // An unreadable peer and no window (the pass never relieves it).
        assert!(!pause_keeps_promotion(&PausePromoteFacts {
            helper_windows: 0,
            helper_relieved: false,
            ..hw
        }));
        // Neither switch: never.
        assert!(!pause_keeps_promotion(&PausePromoteFacts {
            frozen: 1,
            helper_windows: 1,
            ..PausePromoteFacts::default()
        }));
    }

    /// gci i3/tk: the two new promotion switches are default OFF
    /// (`runtime_flag_on`) and read `1` as on; the unpublished rule is default
    /// ON since gci i4 (`=0` kills). The test name predates that flip.
    #[test]
    fn gci_i3tk_the_promotion_and_unpublished_switches_are_default_off() {
        use cratonvm_types::flags::with_thread_overrides;
        for (key, read) in [
            (
                concat!("CRATON", "VM_GEN_TAKEOVER_PROMOTES"),
                gen_takeover_promotes_enabled as fn() -> bool,
            ),
            (
                concat!("CRATON", "VM_GEN_HELPER_WINDOW_REFUSAL_PROMOTES"),
                gen_helper_window_refusal_promotes_enabled,
            ),
        ] {
            assert!(!with_thread_overrides(&[(key, None)], read), "{key} default OFF");
            assert!(!with_thread_overrides(&[(key, Some("0"))], read), "{key}=0 OFF");
            assert!(with_thread_overrides(&[(key, Some("1"))], read), "{key}=1 ON");
        }
        // gci i4: the never-published rule is default ON, `=0` its kill.
        let key = concat!("CRATON", "VM_XT_HELPER_WINDOW_UNPUBLISHED_NATIVE");
        let read = helper_window_unpublished_native_enabled;
        assert!(with_thread_overrides(&[(key, None)], read), "{key} default ON");
        assert!(!with_thread_overrides(&[(key, Some("0"))], read), "{key}=0 OFF");
        assert!(with_thread_overrides(&[(key, Some("1"))], read), "{key}=1 ON");
    }

    /// gci i3/tk: every per-peer peer-state mark of both OS arms goes through
    /// the two licence-aware helpers, and only they call the un-rewritable
    /// door. A source pin (LF assumed).
    #[test]
    fn gci_i3tk_both_arms_mark_peer_state_through_the_licence_helpers() {
        let src = include_str!("xt_root_scan.rs");
        let prod = &src[..src.find("\nmod tests {").expect("tests module")];
        let frozen = concat!("super::mark_takeover_peer_frozen", "(taken);");
        let refusal = concat!(
            "super::mark_helper_window_refusal",
            "(taken, unpinned_incomplete, unknown_peers);"
        );
        assert_eq!(prod.matches(frozen).count(), 2, "both take-over arms");
        assert_eq!(prod.matches(refusal).count(), 2, "both helper-window arms");
        let door = concat!("mark_moving_young_coverage_incomplete_because", "(reason);");
        assert_eq!(prod.matches(door).count(), 2, "only the two helpers");
        assert_eq!(
            prod.matches(concat!("incomplete_because", "(\n")).count(),
            0,
            "no direct multi-line call left"
        );
    }

    /// gci i5/tk: a licensed take-over beside helper windows that were ALL
    /// pinned keeps promotion without the helper licence when a discharged
    /// helper-only pause would; a refusal, a missing take-over licence or the
    /// pinned fact absent still refuse, and the pinned fact never engages a
    /// pause by itself.
    #[test]
    fn gci_i5tk_a_licensed_takeover_beside_pinned_windows_keeps_promotion() {
        let f = PausePromoteFacts {
            frozen: 2,
            takeover_licensed: true,
            helper_windows: 3,
            ..PausePromoteFacts::default()
        };
        assert!(pause_keeps_promotion_with(&f, true));
        assert!(!pause_keeps_promotion_with(&f, false), "discharge killed or kept un-rewritable");
        assert!(
            !pause_keeps_promotion_with(&PausePromoteFacts { helper_refusals: 1, ..f }, true),
            "a refusing window still needs the helper licence"
        );
        assert!(
            !pause_keeps_promotion_with(&PausePromoteFacts { takeover_licensed: false, ..f }, true),
            "the take-over licence is still needed"
        );
        assert!(
            !pause_keeps_promotion_with(&PausePromoteFacts { frozen: 0, ..f }, true),
            "pinned windows alone engage nothing"
        );
        assert!(!pause_keeps_promotion_with(&PausePromoteFacts { stack_incomplete: true, ..f }, true));
        assert!(!pause_keeps_promotion_with(&PausePromoteFacts { native_claims: 1, ..f }, true));
        // The wrapper: discharge on and keep-on-discharge unset -> kept; either
        // switch the other way -> refused (the i3 rule).
        use cratonvm_types::flags::with_thread_overrides;
        let dis = concat!("CRATON", "VM_XT_HELPER_WINDOW_DISCHARGE");
        let keep = concat!("CRATON", "VM_XT_KEEP_UNREWRITABLE_ON_DISCHARGE");
        assert!(with_thread_overrides(&[(dis, None), (keep, None)], || pause_keeps_promotion(&f)));
        assert!(!with_thread_overrides(&[(dis, Some("0")), (keep, None)], || {
            pause_keeps_promotion(&f)
        }));
        assert!(!with_thread_overrides(&[(dis, None), (keep, Some("1"))], || {
            pause_keeps_promotion(&f)
        }));
    }

    /// gci i12/fs (`gci-i1orch-windows-jni-foreign-attach-hangs-RETIRED-UNFIXED-20261018`): the
    /// Windows roster audit's per-pass line names its own time and the two
    /// counts, in the grammar the vi12-fs rows grep.
    #[test]
    fn gci_i12fs_roster_audit_line_shape() {
        assert_eq!(
            roster_audit_line(28_467, 5_120, 23),
            "[xt-jit-roots] roster audit: audit_us=28467 walked=5120 off_roster=23"
        );
        assert_eq!(
            roster_audit_line(0, 0, 0),
            "[xt-jit-roots] roster audit: audit_us=0 walked=0 off_roster=0"
        );
    }

    /// gci i12/fs: the Windows take-over pass times the audit around the audit
    /// call itself and prints the line only with the scan-debug flag; the audit
    /// answers its two counts. Source pins: the audit needs a live process
    /// thread table.
    #[test]
    fn gci_i12fs_the_windows_pass_times_the_roster_audit_under_dbg_only() {
        let src = include_str!("xt_root_scan.rs").replace("\r\n", "\n");
        let windows = src
            .split("#[cfg(windows)]\nmod imp {\n")
            .nth(1)
            .and_then(|s| s.split("// Linux x86-64 implementation (signal rendezvous)\n").next())
            .expect("the Windows arm");
        let gate = windows.find("if roster_audit_enabled() {").expect("the audit gate");
        let t0 = windows.find("let audit_t0 = std::time::Instant::now();").expect("timed");
        let call = windows
            .find("let (walked, off_roster) = audit_roster_covers_jit_peers(")
            .expect("the audit call answers its counts");
        let line = windows
            .find("roster_audit_line(audit_t0.elapsed().as_micros(), walked, off_roster)")
            .expect("the line");
        assert!(gate < t0 && t0 < call && call < line);
        let between = &windows[call..line];
        assert!(between.contains("if dbg() {"), "printed only under the scan-debug flag");
        let audit = windows.find("fn audit_roster_covers_jit_peers(").expect("the audit");
        let body = &windows[audit..];
        let ret = body.find(") -> (u32, u32) {").expect("answers (walked, off_roster)");
        let snap = body.find("CreateToolhelp32Snapshot(").expect("the snapshot");
        assert!(ret < snap);
        assert!(body.contains("return (0, 0);"), "no snapshot answers zero counts");
    }
}

/// gcj j2/jt2 (`CRATONVM_JIT_POST_ONLY_CARD_MARK`): the take-over's half of the
/// post-only card mark.
#[cfg(test)]
mod gcj2jt2_post_only_completion_tests {
    use super::*;

    /// A take-over records the registers of every peer it froze, in freeze
    /// order; one that froze nobody records none.
    #[test]
    fn gcj2jt2_frozen_register_words_accumulate_per_peer() {
        let mut taken = TakenOver::default();
        assert!(taken.frozen_register_words().is_empty());
        taken.note_frozen_registers(&[1, 2, 3]);
        taken.note_frozen_registers(&[4]);
        assert_eq!(taken.frozen_register_words(), &[1, 2, 3, 4]);
    }

    /// The resume completes nothing while the heap's JIT has compiled no
    /// post-only site, then dirties the card of each frozen register word in
    /// the old generation (and only those) once one exists; a take-over that
    /// froze nobody completes nothing. G1 completes nothing either.
    #[test]
    fn gcj2jt2_resume_completes_the_post_only_marks_of_the_frozen_registers() {
        use crate::memory::{GcBackend, VmHeap};
        const MB: usize = 1024 * 1024;
        let heap = VmHeap::new(GcBackend::Generational, 16 * MB);
        let (view, old_base, old_end) = heap
            .jit_card_table_info()
            .expect("a generational card view");
        assert!(old_base + 64 < old_end);
        let frozen = |words: &[usize]| {
            let mut t = TakenOver::default();
            t.note_frozen_registers(words);
            t
        };
        let words = [old_base + 64, 0, 0x1000, old_end];
        assert_eq!(
            resume_completing_post_only_card_marks(frozen(&words), &heap),
            0,
            "no post-only site: nothing owed"
        );
        // The compiler's bump at a post-only site.
        // SAFETY: `view` is the live table's boxed `JitCardView`, which lives
        // as long as `heap`.
        let v = unsafe { &*(view as *const cratonvm_gc::card_table::JitCardView) };
        v.post_only_sites.fetch_add(1, Ordering::Release);
        assert_eq!(
            resume_completing_post_only_card_marks(frozen(&words), &heap),
            1,
            "the one old-generation word"
        );
        assert_eq!(
            resume_completing_post_only_card_marks(TakenOver::default(), &heap),
            0,
            "nobody frozen"
        );
        let g1 = VmHeap::new(GcBackend::G1, 64 * MB);
        assert_eq!(resume_completing_post_only_card_marks(frozen(&words), &g1), 0);
    }
}
