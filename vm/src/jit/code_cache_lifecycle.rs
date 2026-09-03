// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Code-cache **lifecycle**: the installation / invalidation protocol, the
//! epoch retirement that reclaims superseded bodies, and the counters that make
//! both auditable.
//!
//! This closes two adjacent items of the C2 review
//! (`feature-designs/c2/deep-research-vm-c2.md`):
//!
//! * P1 *"Add code-cache lifecycle metrics and reclamation"* — acceptance:
//!   installed/reclaimed bytes, fragmentation, sweeps, failed allocations,
//!   recompilations.
//! * P1 *"Add code installation and invalidation protocol"* — acceptance:
//!   allocate under code-cache synchronization, apply relocations, flush the
//!   instruction cache, transition RW→RX, publish metadata atomically, and
//!   **retire code only after no thread can execute it**.
//!
//! The counter/report idiom deliberately mirrors `gc/src/gc_metrics.rs`, so the
//! two subsystem reports read alike: a flat block of relaxed counters, a raw
//! snapshot struct, a pure `from_raw` normalization, and a `Display` that
//! prints `0.0` rather than `NaN` for every zero denominator.
//!
//! # 1. The retirement protocol
//!
//! Retiring a compiled body has three phases, and only the middle one is new:
//!
//! 1. **Unpublish** (caller's obligation, *before* calling [`CodeCacheLifecycle::retire`]).
//!    The body must be removed from every surface a thread could dispatch
//!    through: the compiled-method cache, monomorphic/polymorphic inline
//!    caches, external dispatch caches, and any baked direct-call target in
//!    another compiled body. After this step **no new activation of the body
//!    can begin**; the only threads that can still be inside it are those that
//!    entered before this instant.
//! 2. **Grace period** (this module). The body sits in the retirement queue
//!    until a sweep can prove that every thread has been outside compiled code
//!    at some instant *after* step 1.
//! 3. **Reclaim** (this module). The queued owner is dropped **outside** the
//!    queue lock, which releases the last `Arc` and unmaps the executable
//!    mapping.
//!
//! ## 1.1 The quiescence signal is `GLOBAL_JIT_DEPTH`, not a second mechanism
//!
//! `docs/threading/thread-transition-states.md` §7.2 / §10.3 establish that
//! `CompiledUninterruptible` is recorded at `push_entry_full`, `pop_jit_entry`
//! and `prune_returned_jit_entries`, and that the process-wide in-JIT depth is
//! the striped counter `GLOBAL_JIT_DEPTH` in
//! [`crate::jit::conservative_roots`]. **That counter is the quiescence signal
//! used here**, read through the existing predicate
//! [`crate::jit::conservative_roots::any_thread_in_jit`]. Nothing in this module
//! maintains a parallel in-JIT count, and no call site was added to the
//! interpreter/JIT boundary to feed one: the boundary already increments and
//! decrements exactly the counter this protocol needs.
//!
//! ## 1.2 Why "the striped sum reads zero" is a real grace period
//!
//! A striped counter's `is_zero()` walks 64 stripes one at a time, so a `true`
//! answer does **not** mean every thread was out of JIT *simultaneously*. For a
//! *count* that race matters. For a *grace period* it does not, and the reason
//! is worth stating precisely, because it is what makes this protocol correct
//! rather than probabilistic:
//!
//! * A thread's stripe index is assigned once and never changes
//!   (`cratonvm_types::striped_counter`), so a stripe read at instant `t`
//!   returning `0` is a **simultaneous** observation that *every thread mapped
//!   to that stripe* had in-JIT depth zero at `t`.
//! * A full walk that returns `true` therefore yields, for every thread in the
//!   process, some instant at which that thread was outside compiled code.
//! * [`CodeCacheLifecycle::sweep`] takes the queue lock **first** and performs
//!   the quiescence walk **while holding it**. Every body in the queue was
//!   therefore unpublished strictly before the walk began, so each thread's
//!   witness instant is after every queued body's unpublication.
//! * A thread outside compiled code at its witness instant can only re-enter
//!   compiled code through a dispatch surface, and every such surface was
//!   cleared for these bodies in step 1. So it cannot be executing any of them.
//!
//! The lock ordering is load-bearing, not incidental. Observing quiescence and
//! *then* taking the queue would let a body unpublished in between be freed on
//! the strength of a witness that predates its unpublication — the window is
//! narrow but it is a use-after-free of executable memory, which is the exact
//! failure mode this item exists to remove. See §5 for the pre-existing drain
//! path that has this shape.
//!
//! ## 1.3 The OS-frozen peer, and failing safe
//!
//! `docs/threading/objectref-concurrency-contract.md` and §6.2/§10.4 of the
//! transition-states document describe the hard case: a peer thread can be
//! **OS-frozen inside compiled code** by the cross-thread STW takeover
//! (`crate::jit::xt_root_scan`). That thread cannot run any cooperative
//! handshake, so no "ask every thread to acknowledge" protocol can complete
//! while it is frozen.
//!
//! It does not need to. A frozen peer's `push_entry_full` already incremented
//! `GLOBAL_JIT_DEPTH` and its matching `pop_jit_entry` has not run, so the
//! depth stays elevated for as long as it is frozen and the sweep simply does
//! not reclaim. The same is true of the *helper window* (a compiled frame that
//! called a Rust helper): the chain depth stays elevated across the helper even
//! though the peer's `Rip` is outside every JIT range, which is the
//! over-approximating half of the §10.4 classifier split — and
//! over-approximation is the correct polarity for "may I free this?", exactly
//! as it is for "may I relocate?".
//!
//! ## 1.4 Quiescence is the backstop, not the primary argument
//!
//! 2026-08-03 (`jit-code-buffer-released-outside-retirement-queue-fixed-20260803.md`):
//! the primary reason a reclamation is safe is **ownership**, not this walk. A
//! thread inside a compiled body always holds an owning `Arc<CompiledMethod>`
//! for it — the interpreter's call sites hold one across `try_call`, generated
//! code reaches a callee only through a surface that roots it, and JIT→JIT
//! dispatch pins via `pin_jit_code_range_owner`. So a reference count reaching
//! zero is itself a proof that no thread is inside.
//!
//! The quiescence walk exists because that argument is only as good as its
//! weakest holder, and holders are added by people who do not know they have an
//! obligation. `cratonvm_jit::RetainedCode` routes a keep-alive's release
//! through the queue by construction, and `published_code_free_audit()` counts
//! any release that reached the OS without one. That counter must be zero; when
//! it is not, `CRATONVM_DBG_JIT_CODE_FREE=1` names the release site.
//!
//! **Fail-safe rule.** If quiescence cannot be established, the body is
//! RETAINED and counted as deferred ([`CodeCacheLifecycleRaw::deferred_bodies`],
//! [`CodeCacheLifecycleRaw::deferred_bytes`],
//! [`CodeCacheLifecycleRaw::sweeps_deferred`],
//! [`CodeCacheLifecycleRaw::max_deferral_sweeps`]). A leak is a bug; freeing
//! live code is a crash. The counters exist so the leak is *visible*: a
//! permanently wedged entry chain (the failure `prune_returned_jit_entries`
//! self-heals) shows up as a `deferred_bytes` that never falls and a
//! `max_deferral_sweeps` that climbs, instead of as silent unbounded growth.
//!
//! # 2. W^X
//!
//! [`WxState`] has no variant that is both writable and executable, so W^X is a
//! property of the type rather than of a runtime check
//! (`no_wx_state_is_both_writable_and_executable` pins it). The install
//! protocol is `Unmapped -Allocate-> Writable -Publish-> Executable`, matching
//! `cratonvm_jit::platform` (RW `mmap`/`VirtualAlloc`, then `mprotect`/
//! `VirtualProtect` to RX, with the aarch64 I-cache flush issued *before* the
//! flip while the page is still RW).
//!
//! Reclamation does **not** touch page permissions at all: it drops the owning
//! `Arc`, whose `ExecutableBuffer::drop` unmaps (or, under
//! `CRATONVM_JIT_POISON_FREE=1`, `mprotect`s to `PROT_NONE`). In particular the
//! protocol never patches a retired body into a trap sequence, which would
//! require an RX→RW→RX round trip; `retire_protocol_never_reopens_a_page`
//! asserts the retirement event sequence never enters `Writable`.
//!
//! # 3. Fragmentation
//!
//! Two different quantities, both reported, because conflating them is how a
//! code cache gets declared "fragmented" when it is merely padded:
//!
//! * **Internal** — `1 - live_code_bytes / live_bytes`. Every body is allocated
//!   in a page-rounded buffer whose capacity comes from a codegen size
//!   *estimate*, so the slack between emitted bytes and reserved bytes is real
//!   retained memory that no allocator policy can recover.
//! * **External** — `1 - largest_free_extent / free_bytes`, computed by
//!   [`external_fragmentation`] from a [`FreeSpace`] gauge the arena owner
//!   publishes. It answers "can a 40 KiB body be installed even though 400 KiB
//!   is free?". Today CratonVM allocates each body as its own OS mapping, so
//!   there is no shared arena and this reads `0.0` until one is published — see
//!   §5.
//!
//! # 4. Cost
//!
//! Every recording entry point here is **per compilation or per retirement**,
//! not per call, so all of them are ungated (the same reasoning
//! `gc_metrics.rs` applies to its collector-side counters). The single hot-path
//! touch is [`pending_retirements`], one relaxed load of a process-global
//! `AtomicUsize`, called from `pop_jit_entry` / `prune_returned_jit_entries`
//! before anything else runs. With nothing queued — the overwhelmingly common
//! case — that is the entire cost.
//!
//! # 5. Reconciliation with what already exists
//!
//! `jit/src/lib.rs` already carries a partial version of this: `defer_jit_owner`
//! queues a superseded `Arc<CompiledMethod>` when `ACTIVE_JIT_EXECUTIONS`
//! (another striped counter, fed from the *same* `push_entry_full` /
//! `pop_jit_entry` boundary) is non-zero, and
//! `drain_deferred_jit_owners_if_quiescent` drops the queue when it reads zero.
//! What this module adds:
//!
//! 1. **Ordering.** ~~`drain_deferred_jit_owners_if_quiescent` performs the
//!    `is_zero()` walk *before* taking the queue lock~~ — fixed on `dev`; it
//!    now walks with the queue lock held, as [`CodeCacheLifecycle::sweep`]
//!    does. See §1.2. What that path still lacks is this module's measurement
//!    and its named fail-safe, below.
//! 2. **Measurement.** The existing path reclaims silently. Nothing counts
//!    installed or reclaimed bytes, sweeps, deferrals, failed allocations or
//!    recompilations, so "the code cache is growing" cannot be distinguished
//!    from "the code cache cannot reclaim".
//! 3. **A named fail-safe.** A retention that cannot be proven safe is a
//!    first-class, counted outcome rather than an early `return`.
//!
//! The two are not yet joined: `jit/src/lib.rs` is outside this change's file
//! ownership, so the install/retire call sites there still have to be routed
//! into this module. `docs/jit/code-cache-lifecycle.md` lists them with the
//! exact edit each one needs.
//!
//! This list named `vm/src/runtime/jit_integration.rs` as a second such place
//! until 2026-09-02. It had no call sites to route: that module was a PARALLEL,
//! never-referenced model of this whole layer -- its own counters, code cache,
//! OSR manager, deopt manager and inline caches -- dead since the initial
//! commit, and it has been deleted. The real install and retire sites are the
//! ones in `jit/src/lib.rs`, and they are the whole list.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

// ---------------------------------------------------------------------------
// Identity
// ---------------------------------------------------------------------------

/// Stable identity of a Java method for recompilation accounting.
///
/// A hash rather than a `String` so the version table costs 16 bytes per
/// method and the install path allocates nothing.
pub type MethodId = u64;

/// Derive a [`MethodId`] from a method's fully-qualified name.
///
/// Separator bytes are hashed between the components so
/// `("a", "bc", "()V")` and `("ab", "c", "()V")` cannot collide by
/// concatenation.
pub fn method_id(class: &str, name: &str, descriptor: &str) -> MethodId {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    class.hash(&mut h);
    0xffu8.hash(&mut h);
    name.hash(&mut h);
    0xffu8.hash(&mut h);
    descriptor.hash(&mut h);
    h.finish()
}

/// What one compiled body occupies in the code cache.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BodyExtent {
    /// Entry-point address. Diagnostic only — the protocol never dereferences
    /// it, so a test may pass any value.
    pub entry: usize,
    /// Machine-code bytes actually emitted.
    pub code_bytes: u64,
    /// Bytes the allocator reserved: the page-rounded buffer capacity. This is
    /// what the OS has mapped and therefore what reclamation returns.
    pub reserved_bytes: u64,
}

impl BodyExtent {
    /// Bytes reserved but never emitted into — the body's internal
    /// fragmentation.
    pub fn slack_bytes(&self) -> u64 {
        self.reserved_bytes.saturating_sub(self.code_bytes)
    }
}

// ---------------------------------------------------------------------------
// Reason codes
// ---------------------------------------------------------------------------

/// Why a compiled body was withdrawn from every dispatch surface.
///
/// Numbering is stable; append new codes and bump [`retire_reason::COUNT`].
pub mod retire_reason {
    /// A newer compilation of the same method replaced it.
    pub const SUPERSEDED: usize = 0;
    /// A speculative assumption failed — class hierarchy change, redefinition,
    /// a broken final/stable-field dependency.
    pub const INVALIDATED: usize = 1;
    /// The body deoptimized and its frames were materialised into the
    /// interpreter.
    pub const DEOPTIMIZED: usize = 2;
    /// Evicted to make room: the cache is at its cap.
    pub const CACHE_PRESSURE: usize = 3;
    /// The owning class (or class loader) was unloaded.
    pub const CLASS_UNLOADED: usize = 4;
    /// VM shutdown / cache teardown.
    pub const SHUTDOWN: usize = 5;

    /// One past the highest defined code.
    pub const COUNT: usize = 6;

    /// Human-readable label. The report prints this verbatim.
    pub fn label(code: usize) -> &'static str {
        match code {
            SUPERSEDED => "superseded-by-recompilation",
            INVALIDATED => "assumption-invalidated",
            DEOPTIMIZED => "deoptimized",
            CACHE_PRESSURE => "evicted-cache-pressure",
            CLASS_UNLOADED => "class-unloaded",
            SHUTDOWN => "shutdown",
            _ => "unknown",
        }
    }
}

/// Why a code-cache allocation was refused.
///
/// Numbering is stable; append new codes and bump [`alloc_failure::COUNT`].
pub mod alloc_failure {
    /// The OS refused the mapping (`mmap`/`VirtualAlloc` returned null).
    pub const OS_REFUSED: usize = 0;
    /// The configured code-cache cap would have been exceeded.
    pub const CAP_EXCEEDED: usize = 1;
    /// Enough total free space, but no single extent large enough — the
    /// failure mode [`super::external_fragmentation`] predicts.
    pub const NO_EXTENT_LARGE_ENOUGH: usize = 2;
    /// Codegen overran its size estimate and the emitted body did not fit the
    /// buffer it was given (`ExecutableBuffer::overflowed`).
    pub const SIZE_ESTIMATE_OVERRUN: usize = 3;

    /// One past the highest defined code.
    pub const COUNT: usize = 4;

    /// Human-readable label. The report prints this verbatim.
    pub fn label(code: usize) -> &'static str {
        match code {
            OS_REFUSED => "os-refused-mapping",
            CAP_EXCEEDED => "code-cache-cap-exceeded",
            NO_EXTENT_LARGE_ENOUGH => "no-free-extent-large-enough",
            SIZE_ESTIMATE_OVERRUN => "size-estimate-overrun",
            _ => "unknown",
        }
    }
}

// ---------------------------------------------------------------------------
// W^X state machine
// ---------------------------------------------------------------------------

/// The page states a code buffer can be in.
///
/// **There is deliberately no `WritableExecutable` variant.** W^X is enforced
/// by the shape of this enum rather than by an assertion someone can forget to
/// call, which is why the reclamation path can be audited by replaying its
/// event sequence instead of by reading `mprotect` call sites.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WxState {
    /// No mapping (before allocation, after release).
    Unmapped,
    /// RW: `mmap(PROT_READ|PROT_WRITE)` / `VirtualAlloc(PAGE_READWRITE)`.
    Writable,
    /// RX: `mprotect(PROT_READ|PROT_EXEC)` / `VirtualProtect(PAGE_EXECUTE_READ)`.
    Executable,
    /// `PROT_NONE`, retained forever — the `CRATONVM_JIT_POISON_FREE=1`
    /// diagnostic mode in `cratonvm_jit::platform`.
    Poisoned,
}

impl WxState {
    /// Can the CPU store to this mapping?
    pub fn is_writable(self) -> bool {
        matches!(self, WxState::Writable)
    }

    /// Can the CPU fetch instructions from this mapping?
    pub fn is_executable(self) -> bool {
        matches!(self, WxState::Executable)
    }
}

/// A permission transition the code cache can perform.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WxEvent {
    /// Map the buffer RW.
    Allocate,
    /// Flip RW→RX after relocations and the I-cache flush.
    Publish,
    /// Flip RX→RW to patch an already-published body.
    Reopen,
    /// Unmap.
    Release,
    /// `mprotect(PROT_NONE)` instead of unmapping (diagnostic mode).
    Poison,
}

/// A transition the state machine refuses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WxViolation {
    /// The state the buffer was in.
    pub state: WxState,
    /// The event that is not legal from it.
    pub event: WxEvent,
}

impl std::fmt::Display for WxViolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "W^X violation: {:?} is not a legal transition from {:?}",
            self.event, self.state,
        )
    }
}

/// Apply one permission transition.
pub fn wx_step(state: WxState, event: WxEvent) -> Result<WxState, WxViolation> {
    let next = match (state, event) {
        (WxState::Unmapped, WxEvent::Allocate) => WxState::Writable,
        (WxState::Writable, WxEvent::Publish) => WxState::Executable,
        (WxState::Executable, WxEvent::Reopen) => WxState::Writable,
        (WxState::Writable, WxEvent::Release) => WxState::Unmapped,
        (WxState::Executable, WxEvent::Release) => WxState::Unmapped,
        (WxState::Executable, WxEvent::Poison) => WxState::Poisoned,
        _ => return Err(WxViolation { state, event }),
    };
    Ok(next)
}

/// Replay a sequence of transitions from `start`.
pub fn wx_replay(start: WxState, events: &[WxEvent]) -> Result<WxState, WxViolation> {
    let mut state = start;
    for &event in events {
        state = wx_step(state, event)?;
    }
    Ok(state)
}

/// The permission transitions installation performs, in order.
pub const INSTALL_WX_PROTOCOL: [WxEvent; 2] = [WxEvent::Allocate, WxEvent::Publish];

/// The permission transitions retirement performs, in order.
///
/// One event. Reclamation never reopens a page for writing — it unmaps after
/// the grace period. See the module header §2.
pub const RETIRE_WX_PROTOCOL: [WxEvent; 1] = [WxEvent::Release];

// ---------------------------------------------------------------------------
// Installation protocol
// ---------------------------------------------------------------------------

/// The ordered steps of a code installation.
///
/// The order is the acceptance criterion of the review's *"Add code
/// installation and invalidation protocol"* item, restated as something a test
/// can check: allocate under code-cache synchronization, apply relocations,
/// flush the instruction cache, transition RW→RX, publish metadata, publish the
/// entry point.
pub mod install_step {
    /// Reserve the buffer with the code-cache allocation lock held.
    pub const ALLOCATE_UNDER_CACHE_LOCK: usize = 0;
    /// Apply relocations into the RW buffer.
    pub const APPLY_RELOCATIONS: usize = 1;
    /// Flush the I-cache for the range — **before** the RW→RX flip, while the
    /// page is still writable and no fetch can race the flush
    /// (`cratonvm_jit::platform::platform_make_executable`).
    pub const FLUSH_ICACHE: usize = 2;
    /// Flip RW→RX.
    pub const TRANSITION_RW_TO_RX: usize = 3;
    /// Publish oop maps, deopt metadata and the code range — everything a
    /// stack walker or the GC needs to interpret a frame.
    pub const PUBLISH_METADATA: usize = 4;
    /// Publish the entry point into the dispatch surfaces. Last, because from
    /// this instant a peer thread can enter the body and immediately be
    /// stack-walked.
    pub const PUBLISH_ENTRY: usize = 5;

    /// One past the highest defined step.
    pub const COUNT: usize = 6;

    /// Human-readable label.
    pub fn label(step: usize) -> &'static str {
        match step {
            ALLOCATE_UNDER_CACHE_LOCK => "allocate-under-cache-lock",
            APPLY_RELOCATIONS => "apply-relocations",
            FLUSH_ICACHE => "flush-icache",
            TRANSITION_RW_TO_RX => "transition-rw-to-rx",
            PUBLISH_METADATA => "publish-metadata",
            PUBLISH_ENTRY => "publish-entry",
            _ => "unknown",
        }
    }
}

/// How an install sequence departs from [`install_step`]'s order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InstallProtocolError {
    /// `step` ran after `after`, but must precede it.
    OutOfOrder {
        /// The step that ran late.
        step: usize,
        /// The step it should have preceded.
        after: usize,
    },
    /// A required step never ran.
    Missing(usize),
    /// A step ran twice.
    Duplicate(usize),
    /// Not a defined step code.
    Unknown(usize),
}

impl std::fmt::Display for InstallProtocolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match *self {
            InstallProtocolError::OutOfOrder { step, after } => write!(
                f,
                "install step {} ran after {}, but must precede it",
                install_step::label(step),
                install_step::label(after),
            ),
            InstallProtocolError::Missing(step) => {
                write!(f, "install step {} never ran", install_step::label(step))
            }
            InstallProtocolError::Duplicate(step) => {
                write!(f, "install step {} ran twice", install_step::label(step))
            }
            InstallProtocolError::Unknown(step) => write!(f, "unknown install step {step}"),
        }
    }
}

/// Check that an observed install sequence is the protocol.
///
/// Every step must appear exactly once, in [`install_step`] order. This is the
/// executable form of "flush the instruction cache before the RW→RX flip" and
/// "publish metadata before the entry point".
pub fn install_sequence_is_legal(steps: &[usize]) -> Result<(), InstallProtocolError> {
    let mut seen = [false; install_step::COUNT];
    let mut highest: Option<usize> = None;
    for &step in steps {
        if step >= install_step::COUNT {
            return Err(InstallProtocolError::Unknown(step));
        }
        if seen[step] {
            return Err(InstallProtocolError::Duplicate(step));
        }
        if let Some(prev) = highest {
            if step < prev {
                return Err(InstallProtocolError::OutOfOrder { step, after: prev });
            }
        }
        seen[step] = true;
        highest = Some(step);
    }
    for (step, ran) in seen.iter().enumerate() {
        if !*ran {
            return Err(InstallProtocolError::Missing(step));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Fragmentation
// ---------------------------------------------------------------------------

/// The free-space shape of a code-cache arena, as a gauge.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FreeSpace {
    /// Number of disjoint free extents.
    pub extents: u64,
    /// Total free bytes across all extents.
    pub free_bytes: u64,
    /// The largest single free extent — the biggest body that can still be
    /// installed without growing the arena.
    pub largest_free_extent: u64,
}

/// External fragmentation: `1 - largest_free_extent / free_bytes`.
///
/// `0.0` means the free space is one contiguous run (or there is none — a full
/// cache is not a fragmented one). Approaching `1.0` means the free space is
/// shattered and a large body cannot be installed even though the total looks
/// ample.
pub fn external_fragmentation(free: FreeSpace) -> f64 {
    if free.free_bytes == 0 {
        return 0.0;
    }
    let largest = free.largest_free_extent.min(free.free_bytes);
    1.0 - (largest as f64 / free.free_bytes as f64)
}

// ---------------------------------------------------------------------------
// Counters
// ---------------------------------------------------------------------------

/// Every lifecycle counter in one block.
///
/// All relaxed: each is touched once per compilation, retirement or sweep, so
/// there is no hot-path cost to gate away (the same reasoning
/// `gc_metrics.rs` gives for its collector-side counters). Fields documented
/// `gauge` hold a current value; everything else is monotone.
struct Counters {
    /// Compiled bodies installed.
    installs: AtomicU64,
    /// Reserved (mapped) bytes installed.
    installed_bytes: AtomicU64,
    /// Machine-code bytes installed, i.e. reserved minus buffer slack.
    installed_code_bytes: AtomicU64,
    /// Installs that replaced an earlier version of the same method.
    recompilations: AtomicU64,
    /// Distinct methods that have been compiled at least once.
    methods_compiled: AtomicU64,
    /// The highest version number any single method has reached (gauge).
    max_versions_for_one_method: AtomicU64,
    /// Bodies handed to [`CodeCacheLifecycle::retire`].
    retirements: AtomicU64,
    /// Reserved bytes handed to `retire`.
    retired_bytes: AtomicU64,
    /// Retirements by [`retire_reason`] code.
    retirements_by_reason: [AtomicU64; retire_reason::COUNT],
    /// Bodies whose mapping was actually released.
    reclaimed_bodies: AtomicU64,
    /// Reserved bytes released.
    reclaimed_bytes: AtomicU64,
    /// Machine-code bytes released.
    reclaimed_code_bytes: AtomicU64,
    /// Sweeps run.
    sweeps: AtomicU64,
    /// Sweeps that reclaimed nothing because quiescence could not be
    /// established. **This is the fail-safe's visible half.**
    sweeps_deferred: AtomicU64,
    /// Body-sweep deferral events (a body deferred by three sweeps counts
    /// three).
    deferrals: AtomicU64,
    /// Bodies currently awaiting reclamation (gauge).
    deferred_bodies: AtomicU64,
    /// Reserved bytes currently awaiting reclamation (gauge). Retained-but-
    /// unreachable code: mapped, unexecutable-by-anything-new, not yet freed.
    deferred_bytes: AtomicU64,
    /// The most sweeps any single queued body has survived (gauge, high-water).
    /// A climbing value with a non-empty queue is a wedged in-JIT depth.
    max_deferral_sweeps: AtomicU64,
    /// Allocation requests the code cache refused.
    failed_allocations: AtomicU64,
    /// Bytes those refused requests asked for.
    failed_allocation_bytes: AtomicU64,
    /// Failed allocations by [`alloc_failure`] code.
    failed_allocations_by_reason: [AtomicU64; alloc_failure::COUNT],
    /// Configured code-cache capacity in bytes (gauge). `0` = uncapped.
    capacity_bytes: AtomicU64,
    /// Free-extent count (gauge).
    free_extents: AtomicU64,
    /// Free bytes (gauge).
    free_bytes: AtomicU64,
    /// Largest free extent (gauge).
    largest_free_extent: AtomicU64,
    /// High-water mark of live (mapped) bytes.
    peak_live_bytes: AtomicU64,
}

impl Counters {
    const fn new() -> Self {
        Self {
            installs: AtomicU64::new(0),
            installed_bytes: AtomicU64::new(0),
            installed_code_bytes: AtomicU64::new(0),
            recompilations: AtomicU64::new(0),
            methods_compiled: AtomicU64::new(0),
            max_versions_for_one_method: AtomicU64::new(0),
            retirements: AtomicU64::new(0),
            retired_bytes: AtomicU64::new(0),
            retirements_by_reason: [const { AtomicU64::new(0) }; retire_reason::COUNT],
            reclaimed_bodies: AtomicU64::new(0),
            reclaimed_bytes: AtomicU64::new(0),
            reclaimed_code_bytes: AtomicU64::new(0),
            sweeps: AtomicU64::new(0),
            sweeps_deferred: AtomicU64::new(0),
            deferrals: AtomicU64::new(0),
            deferred_bodies: AtomicU64::new(0),
            deferred_bytes: AtomicU64::new(0),
            max_deferral_sweeps: AtomicU64::new(0),
            failed_allocations: AtomicU64::new(0),
            failed_allocation_bytes: AtomicU64::new(0),
            failed_allocations_by_reason: [const { AtomicU64::new(0) }; alloc_failure::COUNT],
            capacity_bytes: AtomicU64::new(0),
            free_extents: AtomicU64::new(0),
            free_bytes: AtomicU64::new(0),
            largest_free_extent: AtomicU64::new(0),
            peak_live_bytes: AtomicU64::new(0),
        }
    }
}

// ---------------------------------------------------------------------------
// Retirement queue
// ---------------------------------------------------------------------------

/// A body that has been unpublished and is waiting out its grace period.
struct RetiredBody {
    /// Retirement id, unique within the cache.
    id: u64,
    /// Which method it was compiled from.
    method: MethodId,
    /// What it occupies.
    extent: BodyExtent,
    /// [`retire_reason`] code.
    reason: usize,
    /// The sweep sequence in effect when it was enqueued. Diagnostic: the
    /// grace-period argument is carried by the lock ordering, not by this.
    retired_after_sweep: u64,
    /// How many sweeps have already declined to reclaim it.
    deferrals: u32,
    /// Dropping this releases the mapping. In production it is the last
    /// `Arc<CompiledMethod>`; in tests it is a drop probe. `None` means the
    /// caller is accounting a body it frees itself.
    owner: Option<Box<dyn Send>>,
}

impl std::fmt::Debug for RetiredBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RetiredBody")
            .field("id", &self.id)
            .field("method", &self.method)
            .field("extent", &self.extent)
            .field("reason", &retire_reason::label(self.reason))
            .field("retired_after_sweep", &self.retired_after_sweep)
            .field("deferrals", &self.deferrals)
            .field("has_owner", &self.owner.is_some())
            .finish()
    }
}

/// Everything the lifecycle keeps under one lock.
struct CacheState {
    /// Unpublished bodies awaiting their grace period.
    queue: Vec<RetiredBody>,
    /// Reserved bytes summed over [`CacheState::queue`], maintained
    /// incrementally so `retire` stays O(1) with a long queue.
    queued_bytes: u64,
    /// Per-method version counter. Lazily created so [`CacheState::new`] can be
    /// `const` and the process instance can be a plain `static` — which is what
    /// keeps [`pending_retirements`] to a single relaxed load with no
    /// `OnceLock` indirection on the JIT-leave path.
    versions: Option<HashMap<MethodId, u32>>,
    /// Next install/retirement id.
    next_id: u64,
    /// Sweeps run so far.
    sweep_seq: u64,
}

impl CacheState {
    const fn new() -> Self {
        Self {
            queue: Vec::new(),
            queued_bytes: 0,
            versions: None,
            next_id: 1,
            sweep_seq: 0,
        }
    }
}

// ---------------------------------------------------------------------------
// Outcomes
// ---------------------------------------------------------------------------

/// What [`CodeCacheLifecycle::install`] recorded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Installed {
    /// Unique id for this installation.
    pub id: u64,
    /// 1 for a first compilation, 2 for its first recompilation, and so on.
    pub version: u32,
    /// `version > 1`.
    pub is_recompilation: bool,
}

/// What one [`CodeCacheLifecycle::sweep`] did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SweepOutcome {
    /// 1-based sweep sequence number.
    pub sequence: u64,
    /// Was quiescence established? `false` means every queued body was
    /// **retained** — the fail-safe.
    pub quiescent: bool,
    /// Bodies whose mapping was released by this sweep.
    pub reclaimed_bodies: u64,
    /// Reserved bytes released by this sweep.
    pub reclaimed_bytes: u64,
    /// Bodies still queued after this sweep.
    pub deferred_bodies: u64,
    /// Reserved bytes still queued after this sweep.
    pub deferred_bytes: u64,
    /// The most sweeps any still-queued body has now survived.
    pub oldest_deferral_sweeps: u32,
}

impl std::fmt::Display for SweepOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "[JIT] code-cache sweep #{seq}: {verdict} reclaimed={bodies} bodies / {bytes} bytes; \
             deferred={dbodies} bodies / {dbytes} bytes (oldest deferred {age} sweeps)",
            seq = self.sequence,
            verdict = if self.quiescent {
                "QUIESCENT"
            } else {
                "NOT-QUIESCENT (retained, fail-safe)"
            },
            bodies = self.reclaimed_bodies,
            bytes = self.reclaimed_bytes,
            dbodies = self.deferred_bodies,
            dbytes = self.deferred_bytes,
            age = self.oldest_deferral_sweeps,
        )
    }
}

// ---------------------------------------------------------------------------
// The lifecycle
// ---------------------------------------------------------------------------

/// A code cache's lifecycle accounting and its retirement queue.
///
/// There is one process instance ([`process_lifecycle`]); tests construct their
/// own so they neither observe nor disturb it. An instance built with
/// [`CodeCacheLifecycle::new`] reads the real quiescence signal
/// (`GLOBAL_JIT_DEPTH`); one built with
/// [`CodeCacheLifecycle::with_simulated_quiescence`] reads a depth the test
/// drives itself.
pub struct CodeCacheLifecycle {
    counters: Counters,
    state: Mutex<CacheState>,
    /// Queue length, mirrored out of the lock so the JIT-leave hot path can
    /// test it with one relaxed load.
    pending: AtomicUsize,
    /// `None` → the production signal, `GLOBAL_JIT_DEPTH` via
    /// [`crate::jit::conservative_roots::any_thread_in_jit`]. `Some` → a
    /// simulated in-JIT depth, for tests that need to hold a thread inside
    /// compiled code deterministically.
    simulated_depth: Option<Arc<AtomicUsize>>,
}

impl CodeCacheLifecycle {
    /// A lifecycle whose quiescence signal is the real process-wide in-JIT
    /// depth.
    pub const fn new() -> Self {
        Self {
            counters: Counters::new(),
            state: Mutex::new(CacheState::new()),
            pending: AtomicUsize::new(0),
            simulated_depth: None,
        }
    }

    /// A lifecycle whose quiescence signal is an explicit depth the caller
    /// drives with [`CodeCacheLifecycle::simulated_depth`].
    ///
    /// Tests only. It exists so "a thread is inside compiled code" can be made
    /// deterministic: pushing a *real* JIT entry would make every concurrently
    /// running unit test's sweeps defer as a side effect.
    pub fn with_simulated_quiescence() -> Self {
        Self {
            counters: Counters::new(),
            state: Mutex::new(CacheState::new()),
            pending: AtomicUsize::new(0),
            simulated_depth: Some(Arc::new(AtomicUsize::new(0))),
        }
    }

    /// The simulated in-JIT depth, if this instance has one. Increment it to
    /// put a thread inside compiled code, decrement it to take it out.
    pub fn simulated_depth(&self) -> Option<Arc<AtomicUsize>> {
        self.simulated_depth.clone()
    }

    fn lock_state(&self) -> MutexGuard<'_, CacheState> {
        // A panic inside the queue lock must not wedge reclamation forever; the
        // state is plain data and stays consistent across an unwind.
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The quiescence question: is **no** thread anywhere inside compiled code?
    ///
    /// Production reads `GLOBAL_JIT_DEPTH` through the existing predicate. See
    /// the module header §1.1/§1.2 for why one striped-sum zero read is a valid
    /// grace period when it is taken with the queue lock held.
    fn no_thread_in_jit(&self) -> bool {
        match &self.simulated_depth {
            Some(depth) => depth.load(Ordering::Acquire) == 0,
            None => !crate::jit::conservative_roots::any_thread_in_jit(),
        }
    }

    /// Bodies awaiting reclamation. One relaxed load.
    #[inline]
    pub fn pending_retirements(&self) -> usize {
        self.pending.load(Ordering::Relaxed)
    }

    /// Alias for [`CodeCacheLifecycle::pending_retirements`], for call sites
    /// that read better as a queue length.
    pub fn queued_bodies(&self) -> usize {
        self.pending_retirements()
    }

    // -----------------------------------------------------------------------
    // Installation
    // -----------------------------------------------------------------------

    /// Record a successful installation of `method`'s compiled body.
    ///
    /// Call this **after** the full [`install_step`] sequence has run, i.e.
    /// once the body is reachable. The returned [`Installed::version`] is the
    /// method's compilation count including this one.
    pub fn install(&self, method: MethodId, extent: BodyExtent) -> Installed {
        let (id, version) = {
            let mut st = self.lock_state();
            let id = st.next_id;
            st.next_id = st.next_id.wrapping_add(1);
            let version = {
                let versions = st.versions.get_or_insert_with(HashMap::new);
                let slot = versions.entry(method).or_insert(0);
                *slot = slot.saturating_add(1);
                *slot
            };
            (id, version)
        };

        let c = &self.counters;
        c.installs.fetch_add(1, Ordering::Relaxed);
        c.installed_bytes
            .fetch_add(extent.reserved_bytes, Ordering::Relaxed);
        c.installed_code_bytes
            .fetch_add(extent.code_bytes, Ordering::Relaxed);
        if version > 1 {
            c.recompilations.fetch_add(1, Ordering::Relaxed);
        } else {
            c.methods_compiled.fetch_add(1, Ordering::Relaxed);
        }
        c.max_versions_for_one_method
            .fetch_max(version as u64, Ordering::Relaxed);
        let live = c
            .installed_bytes
            .load(Ordering::Relaxed)
            .saturating_sub(c.reclaimed_bytes.load(Ordering::Relaxed));
        c.peak_live_bytes.fetch_max(live, Ordering::Relaxed);

        Installed {
            id,
            version,
            is_recompilation: version > 1,
        }
    }

    /// Record an allocation the code cache refused.
    ///
    /// Touches **no** occupancy counter: a refused allocation reserved nothing,
    /// so `installed - reclaimed == live` must survive it unchanged. `reason`
    /// is an [`alloc_failure`] code.
    pub fn record_allocation_failure(&self, requested_bytes: u64, reason: usize) {
        let c = &self.counters;
        c.failed_allocations.fetch_add(1, Ordering::Relaxed);
        c.failed_allocation_bytes
            .fetch_add(requested_bytes, Ordering::Relaxed);
        if let Some(slot) = c.failed_allocations_by_reason.get(reason) {
            slot.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Publish the configured code-cache capacity (a gauge). `0` = uncapped,
    /// which reports occupancy as `0.0` rather than dividing by zero.
    pub fn record_capacity_bytes(&self, bytes: u64) {
        self.counters.capacity_bytes.store(bytes, Ordering::Relaxed);
    }

    /// Publish the arena's free-space shape (a gauge). Feeds
    /// [`external_fragmentation`].
    pub fn record_free_space(&self, free: FreeSpace) {
        let c = &self.counters;
        c.free_extents.store(free.extents, Ordering::Relaxed);
        c.free_bytes.store(free.free_bytes, Ordering::Relaxed);
        c.largest_free_extent
            .store(free.largest_free_extent, Ordering::Relaxed);
    }

    // -----------------------------------------------------------------------
    // Retirement
    // -----------------------------------------------------------------------

    /// Enqueue an **already-unpublished** body for reclamation.
    ///
    /// # Precondition — the caller's half of the protocol
    ///
    /// By the time this is called the body must be unreachable from every
    /// dispatch surface: the compiled-method cache, every inline cache, every
    /// external dispatch cache, and every baked direct-call target in another
    /// body. This module can prove "no thread is *currently* inside compiled
    /// code"; only the caller can guarantee "and no thread can enter this body
    /// again". Retiring a still-published body is a use-after-free that no
    /// grace period can prevent.
    ///
    /// Dropping `owner` is what releases the mapping — in production the last
    /// `Arc<CompiledMethod>`, whose `ExecutableBuffer::drop` unmaps. It is
    /// dropped **outside** the queue lock.
    ///
    /// Returns the retirement id.
    pub fn retire(
        &self,
        method: MethodId,
        extent: BodyExtent,
        reason: usize,
        owner: Option<Box<dyn Send>>,
    ) -> u64 {
        let id = {
            let mut st = self.lock_state();
            let id = st.next_id;
            st.next_id = st.next_id.wrapping_add(1);
            let retired_after_sweep = st.sweep_seq;
            st.queue.push(RetiredBody {
                id,
                method,
                extent,
                reason,
                retired_after_sweep,
                deferrals: 0,
                owner,
            });
            st.queued_bytes = st.queued_bytes.saturating_add(extent.reserved_bytes);
            let queued = st.queue.len();
            let queued_bytes = st.queued_bytes;
            self.pending.store(queued, Ordering::Relaxed);
            // Both gauges move together: a report taken between a retirement
            // and the next sweep must not say "1 body, 0 bytes".
            self.counters
                .deferred_bodies
                .store(queued as u64, Ordering::Relaxed);
            self.counters
                .deferred_bytes
                .store(queued_bytes, Ordering::Relaxed);
            id
        };

        let c = &self.counters;
        c.retirements.fetch_add(1, Ordering::Relaxed);
        c.retired_bytes
            .fetch_add(extent.reserved_bytes, Ordering::Relaxed);
        if let Some(slot) = c.retirements_by_reason.get(reason) {
            slot.fetch_add(1, Ordering::Relaxed);
        }
        id
    }

    /// Run one reclamation sweep.
    ///
    /// Takes the queue lock, asks the quiescence question **with the lock
    /// held** (see §1.2 — this ordering is the correctness argument, not a
    /// performance choice), and either releases every queued body or retains
    /// all of them and counts the deferral.
    ///
    /// Owners are dropped after the lock is released, so the unmap — which
    /// re-enters `cratonvm_jit`'s own region bookkeeping — never runs beneath
    /// this lock.
    pub fn sweep(&self) -> SweepOutcome {
        let mut owners: Vec<Box<dyn Send>> = Vec::new();
        let outcome;
        {
            let mut st = self.lock_state();
            st.sweep_seq = st.sweep_seq.wrapping_add(1);
            let sequence = st.sweep_seq;

            // An empty queue is trivially reclaimable, and asking the
            // quiescence question for it would charge a `sweeps_deferred`
            // against a sweep that had nothing to defer.
            let quiescent = st.queue.is_empty() || self.no_thread_in_jit();

            let mut reclaimed_bodies = 0u64;
            let mut reclaimed_bytes = 0u64;
            let mut reclaimed_code_bytes = 0u64;
            if quiescent {
                for mut body in st.queue.drain(..) {
                    reclaimed_bodies += 1;
                    reclaimed_bytes += body.extent.reserved_bytes;
                    reclaimed_code_bytes += body.extent.code_bytes;
                    if let Some(owner) = body.owner.take() {
                        owners.push(owner);
                    }
                }
                st.queued_bytes = 0;
            } else {
                for body in st.queue.iter_mut() {
                    body.deferrals = body.deferrals.saturating_add(1);
                }
            }

            let deferred_bodies = st.queue.len() as u64;
            let deferred_bytes = st.queued_bytes;
            let oldest_deferral_sweeps = st.queue.iter().map(|b| b.deferrals).max().unwrap_or(0);
            self.pending.store(st.queue.len(), Ordering::Relaxed);

            let c = &self.counters;
            c.sweeps.fetch_add(1, Ordering::Relaxed);
            if !quiescent {
                c.sweeps_deferred.fetch_add(1, Ordering::Relaxed);
                c.deferrals.fetch_add(deferred_bodies, Ordering::Relaxed);
            }
            c.reclaimed_bodies
                .fetch_add(reclaimed_bodies, Ordering::Relaxed);
            c.reclaimed_bytes
                .fetch_add(reclaimed_bytes, Ordering::Relaxed);
            c.reclaimed_code_bytes
                .fetch_add(reclaimed_code_bytes, Ordering::Relaxed);
            c.deferred_bodies.store(deferred_bodies, Ordering::Relaxed);
            c.deferred_bytes.store(deferred_bytes, Ordering::Relaxed);
            c.max_deferral_sweeps
                .fetch_max(oldest_deferral_sweeps as u64, Ordering::Relaxed);

            outcome = SweepOutcome {
                sequence,
                quiescent,
                reclaimed_bodies,
                reclaimed_bytes,
                deferred_bodies,
                deferred_bytes,
                oldest_deferral_sweeps,
            };
        }
        // Outside the lock: this is where executable mappings actually go away.
        drop(owners);
        outcome
    }

    /// How many times `method` has been compiled.
    pub fn versions_of(&self, method: MethodId) -> u32 {
        let st = self.lock_state();
        st.versions
            .as_ref()
            .and_then(|v| v.get(&method).copied())
            .unwrap_or(0)
    }

    // -----------------------------------------------------------------------
    // Reporting
    // -----------------------------------------------------------------------

    /// Snapshot the raw counters.
    ///
    /// Not snapshot-consistent across fields — these are observability, not a
    /// transaction, exactly as in `gc_metrics.rs`. The derived identities in
    /// [`CodeCacheLifecycleReport`] are computed with saturating subtraction so
    /// a torn read can never produce a negative "live".
    pub fn raw(&self) -> CodeCacheLifecycleRaw {
        let c = &self.counters;
        let mut retirements_by_reason = [0u64; retire_reason::COUNT];
        for (slot, out) in c
            .retirements_by_reason
            .iter()
            .zip(retirements_by_reason.iter_mut())
        {
            *out = slot.load(Ordering::Relaxed);
        }
        let mut failed_allocations_by_reason = [0u64; alloc_failure::COUNT];
        for (slot, out) in c
            .failed_allocations_by_reason
            .iter()
            .zip(failed_allocations_by_reason.iter_mut())
        {
            *out = slot.load(Ordering::Relaxed);
        }
        CodeCacheLifecycleRaw {
            installs: c.installs.load(Ordering::Relaxed),
            installed_bytes: c.installed_bytes.load(Ordering::Relaxed),
            installed_code_bytes: c.installed_code_bytes.load(Ordering::Relaxed),
            recompilations: c.recompilations.load(Ordering::Relaxed),
            methods_compiled: c.methods_compiled.load(Ordering::Relaxed),
            max_versions_for_one_method: c.max_versions_for_one_method.load(Ordering::Relaxed),
            retirements: c.retirements.load(Ordering::Relaxed),
            retired_bytes: c.retired_bytes.load(Ordering::Relaxed),
            retirements_by_reason,
            reclaimed_bodies: c.reclaimed_bodies.load(Ordering::Relaxed),
            reclaimed_bytes: c.reclaimed_bytes.load(Ordering::Relaxed),
            reclaimed_code_bytes: c.reclaimed_code_bytes.load(Ordering::Relaxed),
            sweeps: c.sweeps.load(Ordering::Relaxed),
            sweeps_deferred: c.sweeps_deferred.load(Ordering::Relaxed),
            deferrals: c.deferrals.load(Ordering::Relaxed),
            deferred_bodies: c.deferred_bodies.load(Ordering::Relaxed),
            deferred_bytes: c.deferred_bytes.load(Ordering::Relaxed),
            max_deferral_sweeps: c.max_deferral_sweeps.load(Ordering::Relaxed),
            failed_allocations: c.failed_allocations.load(Ordering::Relaxed),
            failed_allocation_bytes: c.failed_allocation_bytes.load(Ordering::Relaxed),
            failed_allocations_by_reason,
            capacity_bytes: c.capacity_bytes.load(Ordering::Relaxed),
            free_extents: c.free_extents.load(Ordering::Relaxed),
            free_bytes: c.free_bytes.load(Ordering::Relaxed),
            largest_free_extent: c.largest_free_extent.load(Ordering::Relaxed),
            peak_live_bytes: c.peak_live_bytes.load(Ordering::Relaxed),
        }
    }

    /// The normalized lifecycle report.
    pub fn report(&self) -> CodeCacheLifecycleReport {
        CodeCacheLifecycleReport::from_raw(self.raw())
    }
}

impl Default for CodeCacheLifecycle {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// The report
// ---------------------------------------------------------------------------

/// The raw counter values, before normalization.
///
/// Split out from [`CodeCacheLifecycleReport`] so the derivation is a pure
/// function of plain numbers and can be tested without touching any global —
/// the same split `gc_metrics::GcMetricsRaw` uses.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CodeCacheLifecycleRaw {
    /// Bodies installed.
    pub installs: u64,
    /// Reserved bytes installed.
    pub installed_bytes: u64,
    /// Machine-code bytes installed.
    pub installed_code_bytes: u64,
    /// Installs that superseded an earlier version of the same method.
    pub recompilations: u64,
    /// Distinct methods compiled at least once.
    pub methods_compiled: u64,
    /// Highest version reached by any one method.
    pub max_versions_for_one_method: u64,
    /// Bodies enqueued for retirement.
    pub retirements: u64,
    /// Reserved bytes enqueued for retirement.
    pub retired_bytes: u64,
    /// Retirements per [`retire_reason`] code.
    pub retirements_by_reason: [u64; retire_reason::COUNT],
    /// Bodies actually unmapped.
    pub reclaimed_bodies: u64,
    /// Reserved bytes actually unmapped.
    pub reclaimed_bytes: u64,
    /// Machine-code bytes actually unmapped.
    pub reclaimed_code_bytes: u64,
    /// Sweeps run.
    pub sweeps: u64,
    /// Sweeps that reclaimed nothing because quiescence failed.
    pub sweeps_deferred: u64,
    /// Body-sweep deferral events.
    pub deferrals: u64,
    /// Bodies queued right now.
    pub deferred_bodies: u64,
    /// Reserved bytes queued right now.
    pub deferred_bytes: u64,
    /// Most sweeps survived by any one queued body.
    pub max_deferral_sweeps: u64,
    /// Allocation requests refused.
    pub failed_allocations: u64,
    /// Bytes those requests asked for.
    pub failed_allocation_bytes: u64,
    /// Failed allocations per [`alloc_failure`] code.
    pub failed_allocations_by_reason: [u64; alloc_failure::COUNT],
    /// Configured capacity (`0` = uncapped).
    pub capacity_bytes: u64,
    /// Free extents in the arena.
    pub free_extents: u64,
    /// Free bytes in the arena.
    pub free_bytes: u64,
    /// Largest single free extent.
    pub largest_free_extent: u64,
    /// High-water mark of live bytes.
    pub peak_live_bytes: u64,
}

/// Code-cache lifecycle, raw and derived.
///
/// Every ratio is `0.0` when its denominator is zero — a report taken before
/// the first compilation reads as "nothing observed", never as `NaN`/`inf`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CodeCacheLifecycleReport {
    /// The counters this was derived from.
    pub raw: CodeCacheLifecycleRaw,

    // --- occupancy identities -------------------------------------------
    /// `installs - reclaimed_bodies`. Bodies still mapped.
    pub live_bodies: u64,
    /// `installed_bytes - reclaimed_bytes`. **The mapped-bytes identity the
    /// counters must satisfy.**
    pub live_bytes: u64,
    /// `installed_code_bytes - reclaimed_code_bytes`.
    pub live_code_bytes: u64,
    /// `live_bytes - deferred_bytes`. Mapped bytes that are still reachable,
    /// i.e. genuinely in service.
    pub reachable_bytes: u64,

    // --- normalized ------------------------------------------------------
    /// `live_bytes / capacity_bytes`; `0.0` when uncapped.
    pub occupancy: f64,
    /// `1 - live_code_bytes / live_bytes`: buffer slack retained per live byte.
    pub internal_fragmentation: f64,
    /// `1 - largest_free_extent / free_bytes`, from the published arena gauge.
    pub external_fragmentation: f64,
    /// `deferred_bytes / live_bytes`: how much of the mapped cache is retained
    /// only because reclamation could not be proven safe.
    pub deferred_fraction: f64,
    /// `reclaimed_bytes / retired_bytes`: how much of what was withdrawn has
    /// actually been returned. Below `1.0` with a stable workload means the
    /// grace period is not completing.
    pub reclaim_completion: f64,
    /// `(sweeps - sweeps_deferred) / sweeps`.
    pub sweep_success_rate: f64,
    /// `recompilations / installs`. High values are compilation thrash.
    pub recompilation_ratio: f64,
    /// `installs / methods_compiled`: mean compiled versions per method.
    pub versions_per_method: f64,
    /// `failed_allocations / (installs + failed_allocations)`.
    pub allocation_failure_rate: f64,
    /// `installed_bytes / installs`: mean reserved bytes per body.
    pub mean_body_bytes: f64,
}

#[inline]
fn ratio(num: u64, den: u64) -> f64 {
    if den == 0 {
        0.0
    } else {
        num as f64 / den as f64
    }
}

impl CodeCacheLifecycleReport {
    /// Derive the report from a raw counter set. Pure — no globals, no
    /// allocation.
    pub fn from_raw(raw: CodeCacheLifecycleRaw) -> Self {
        let live_bodies = raw.installs.saturating_sub(raw.reclaimed_bodies);
        let live_bytes = raw.installed_bytes.saturating_sub(raw.reclaimed_bytes);
        let live_code_bytes = raw
            .installed_code_bytes
            .saturating_sub(raw.reclaimed_code_bytes);
        let reachable_bytes = live_bytes.saturating_sub(raw.deferred_bytes);
        Self {
            raw,
            live_bodies,
            live_bytes,
            live_code_bytes,
            reachable_bytes,
            occupancy: ratio(live_bytes, raw.capacity_bytes),
            internal_fragmentation: if live_bytes == 0 {
                0.0
            } else {
                1.0 - ratio(live_code_bytes.min(live_bytes), live_bytes)
            },
            external_fragmentation: external_fragmentation(FreeSpace {
                extents: raw.free_extents,
                free_bytes: raw.free_bytes,
                largest_free_extent: raw.largest_free_extent,
            }),
            deferred_fraction: ratio(raw.deferred_bytes, live_bytes),
            reclaim_completion: ratio(raw.reclaimed_bytes, raw.retired_bytes),
            sweep_success_rate: ratio(raw.sweeps.saturating_sub(raw.sweeps_deferred), raw.sweeps),
            recompilation_ratio: ratio(raw.recompilations, raw.installs),
            versions_per_method: ratio(raw.installs, raw.methods_compiled),
            allocation_failure_rate: ratio(
                raw.failed_allocations,
                raw.installs.saturating_add(raw.failed_allocations),
            ),
            mean_body_bytes: ratio(raw.installed_bytes, raw.installs),
        }
    }

    /// Labels of every retire reason that fired at least once, with counts.
    pub fn retire_reason_breakdown(&self) -> Vec<(&'static str, u64)> {
        self.raw
            .retirements_by_reason
            .iter()
            .copied()
            .enumerate()
            .filter(|&(_, n)| n != 0)
            .map(|(code, n)| (retire_reason::label(code), n))
            .collect()
    }

    /// Labels of every allocation-failure reason that fired at least once,
    /// with counts.
    pub fn allocation_failure_breakdown(&self) -> Vec<(&'static str, u64)> {
        self.raw
            .failed_allocations_by_reason
            .iter()
            .copied()
            .enumerate()
            .filter(|&(_, n)| n != 0)
            .map(|(code, n)| (alloc_failure::label(code), n))
            .collect()
    }
}

impl std::fmt::Display for CodeCacheLifecycleReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let r = &self.raw;
        writeln!(
            f,
            "[JIT] code-cache: installed={} bodies / {} bytes  reclaimed={} bodies / {} bytes  \
             live={} bodies / {} bytes (peak {})",
            r.installs,
            r.installed_bytes,
            r.reclaimed_bodies,
            r.reclaimed_bytes,
            self.live_bodies,
            self.live_bytes,
            r.peak_live_bytes,
        )?;
        writeln!(
            f,
            "[JIT] code-cache: occupancy={:.4} (capacity={}) internal_frag={:.4} \
             external_frag={:.4} (free={} bytes in {} extents, largest={})",
            self.occupancy,
            r.capacity_bytes,
            self.internal_fragmentation,
            self.external_fragmentation,
            r.free_bytes,
            r.free_extents,
            r.largest_free_extent,
        )?;
        writeln!(
            f,
            "[JIT] code-cache retirement: retired={} bodies / {} bytes  sweeps={} \
             (deferred {}) reclaim_completion={:.4}",
            r.retirements, r.retired_bytes, r.sweeps, r.sweeps_deferred, self.reclaim_completion,
        )?;
        if r.deferred_bodies == 0 {
            writeln!(
                f,
                "[JIT] code-cache retirement: nothing awaiting reclamation"
            )?;
        } else {
            writeln!(
                f,
                "[JIT] code-cache retirement: RETAINED {} bodies / {} bytes ({:.4} of live) — \
                 quiescence unproven, oldest deferred {} sweeps. Retention is the fail-safe: \
                 a leak is a bug, freeing live code is a crash.",
                r.deferred_bodies, r.deferred_bytes, self.deferred_fraction, r.max_deferral_sweeps,
            )?;
        }
        let reasons = self.retire_reason_breakdown();
        if !reasons.is_empty() {
            writeln!(
                f,
                "[JIT] code-cache retirement by reason: {}",
                reasons
                    .iter()
                    .map(|(label, n)| format!("{label}={n}"))
                    .collect::<Vec<_>>()
                    .join(" "),
            )?;
        }
        writeln!(
            f,
            "[JIT] code-cache compilation: methods={} recompilations={} ratio={:.4} \
             versions_per_method={:.4} max_versions_for_one_method={}",
            r.methods_compiled,
            r.recompilations,
            self.recompilation_ratio,
            self.versions_per_method,
            r.max_versions_for_one_method,
        )?;
        let failures = self.allocation_failure_breakdown();
        if failures.is_empty() {
            write!(f, "[JIT] code-cache allocation: no failed allocations")
        } else {
            write!(
                f,
                "[JIT] code-cache allocation: failed={} requesting {} bytes (rate {:.4}) — {}",
                r.failed_allocations,
                r.failed_allocation_bytes,
                self.allocation_failure_rate,
                failures
                    .iter()
                    .map(|(label, n)| format!("{label}={n}"))
                    .collect::<Vec<_>>()
                    .join(" "),
            )
        }
    }
}

// ---------------------------------------------------------------------------
// The process instance
// ---------------------------------------------------------------------------

/// The process-wide code-cache lifecycle.
///
/// A plain `static` rather than a `OnceLock`, deliberately: [`pending_retirements`]
/// is read on the JIT-leave path and must be one relaxed load with no lazy-init
/// branch. `CodeCacheLifecycle::new` is `const` for exactly this reason.
///
/// Unlike `gc_metrics.rs`, this is **not** made thread-local under `cfg(test)`.
/// A per-thread process instance would make the concurrency test unable to
/// share one, and the assertions this module's tests make against it are
/// deltas and identities (which hold under interference) rather than absolute
/// values. Tests that need absolute values build their own instance.
///
/// # Per-VM state audit: BENIGN — process scope is the CORRECT scope here
///
/// Checked by the 2026-08-01 `vm/src/jit/` cache-keying sweep
/// (`audits/vm-jit-cache-keying.md`) and deliberately left
/// process-global. Two independent reasons:
///
///  * what it tracks is genuinely process-wide. `BodyExtent` names executable
///    pages in one process-wide code arena, and the quiescence predicate is
///    `conservative_roots::any_thread_in_jit()` — a process-wide striped depth.
///    Reclaiming a page is only safe when NO thread anywhere is in compiled
///    code, which is a property of the process, not of a VM.
///  * the key cannot alias per-VM state. `MethodId` is
///    `method_id(class, name, descriptor)`, a hash of the fully-qualified NAME
///    triple — not a `ClassId`, not a loader id. Two VMs compiling
///    `java/lang/String.hashCode()I` deliberately share a row, exactly like
///    `runtime/frame.rs`'s content-hashed `padded_code_cache` in the round-1
///    sweep. The worst a cross-VM "hit" does is merge two VMs' version counts
///    in a diagnostic (`versions_of`), which is the intended process-wide
///    census reading.
static PROCESS_LIFECYCLE: CodeCacheLifecycle = CodeCacheLifecycle::new();

/// The process-wide code-cache lifecycle.
pub fn process_lifecycle() -> &'static CodeCacheLifecycle {
    &PROCESS_LIFECYCLE
}

/// Bodies awaiting reclamation process-wide.
///
/// **The hot-path gate.** `pop_jit_entry` / `prune_returned_jit_entries` test
/// this before doing anything else; with nothing queued (the overwhelmingly
/// common case) one relaxed load is the entire cost of the retirement
/// machinery on the interpreter/JIT boundary.
#[inline]
pub fn pending_retirements() -> usize {
    PROCESS_LIFECYCLE.pending.load(Ordering::Relaxed)
}

/// Run a process-wide sweep. Reclaims only if quiescence holds; otherwise
/// retains everything and counts the deferral.
///
/// Called from the JIT-leave path, which is the moment the in-JIT depth can
/// have reached zero.
pub fn sweep_if_quiescent() -> SweepOutcome {
    PROCESS_LIFECYCLE.sweep()
}

/// Record an installation in the process cache. See
/// [`CodeCacheLifecycle::install`].
pub fn record_install(method: MethodId, extent: BodyExtent) -> Installed {
    PROCESS_LIFECYCLE.install(method, extent)
}

/// Enqueue an already-unpublished body in the process cache. See
/// [`CodeCacheLifecycle::retire`] for the caller's precondition.
pub fn record_retirement(
    method: MethodId,
    extent: BodyExtent,
    reason: usize,
    owner: Option<Box<dyn Send>>,
) -> u64 {
    PROCESS_LIFECYCLE.retire(method, extent, reason, owner)
}

/// Record a refused code-cache allocation in the process cache.
pub fn record_allocation_failure(requested_bytes: u64, reason: usize) {
    PROCESS_LIFECYCLE.record_allocation_failure(requested_bytes, reason);
}

/// Publish the process code cache's configured capacity (gauge).
pub fn record_capacity_bytes(bytes: u64) {
    PROCESS_LIFECYCLE.record_capacity_bytes(bytes);
}

/// Publish the process code cache's free-space shape (gauge).
pub fn record_free_space(free: FreeSpace) {
    PROCESS_LIFECYCLE.record_free_space(free);
}

/// Snapshot the process code cache's raw counters.
pub fn code_cache_lifecycle_raw() -> CodeCacheLifecycleRaw {
    PROCESS_LIFECYCLE.raw()
}

/// The process code cache's lifecycle report.
///
/// Cheap (a few dozen relaxed loads and some float division); safe to call
/// outside a pause.
pub fn code_cache_lifecycle_report() -> CodeCacheLifecycleReport {
    PROCESS_LIFECYCLE.report()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Dropping one of these is what a test observes instead of an unmap.
    struct DropProbe {
        drops: Arc<AtomicU64>,
        /// The simulated in-JIT depth at the instant of the drop, recorded so a
        /// test can assert that nothing was freed while a thread was inside
        /// compiled code.
        depth: Arc<AtomicUsize>,
        freed_while_in_jit: Arc<AtomicU64>,
    }

    impl Drop for DropProbe {
        fn drop(&mut self) {
            if self.depth.load(Ordering::Acquire) != 0 {
                self.freed_while_in_jit.fetch_add(1, Ordering::Relaxed);
            }
            self.drops.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn extent(code: u64, reserved: u64) -> BodyExtent {
        BodyExtent {
            entry: 0x1000,
            code_bytes: code,
            reserved_bytes: reserved,
        }
    }

    // -----------------------------------------------------------------------
    // Counter identities
    // -----------------------------------------------------------------------

    #[test]
    fn installed_minus_reclaimed_is_live() {
        let lc = CodeCacheLifecycle::with_simulated_quiescence();
        let m = method_id("java/lang/String", "hashCode", "()I");

        for _ in 0..4 {
            lc.install(m, extent(300, 4096));
        }
        let r = lc.report();
        assert_eq!(r.live_bodies, 4);
        assert_eq!(r.live_bytes, 4 * 4096);
        assert_eq!(r.live_code_bytes, 4 * 300);
        assert_eq!(
            r.reachable_bytes, r.live_bytes,
            "nothing retired yet, so every live byte is reachable",
        );

        // Retire two. They are still MAPPED — retirement is not reclamation.
        lc.retire(m, extent(300, 4096), retire_reason::SUPERSEDED, None);
        lc.retire(m, extent(300, 4096), retire_reason::INVALIDATED, None);
        let r = lc.report();
        assert_eq!(r.live_bytes, 4 * 4096, "a retired body is still mapped");
        assert_eq!(
            (r.raw.deferred_bodies, r.raw.deferred_bytes),
            (2, 2 * 4096),
            "both deferred gauges move with the retirement, not with the sweep",
        );
        assert_eq!(r.reachable_bytes, 2 * 4096);
        assert_eq!(lc.queued_bodies(), 2);

        let out = lc.sweep();
        assert!(out.quiescent);
        assert_eq!(out.reclaimed_bodies, 2);
        assert_eq!(out.reclaimed_bytes, 2 * 4096);

        let r = lc.report();
        assert_eq!(
            r.live_bytes,
            r.raw.installed_bytes - r.raw.reclaimed_bytes,
            "installed - reclaimed == live",
        );
        assert_eq!(r.live_bytes, 2 * 4096);
        assert_eq!(r.live_bodies, 2);
        assert_eq!(r.reachable_bytes, r.live_bytes);
        assert_eq!(r.reclaim_completion, 1.0);
    }

    #[test]
    fn a_retired_body_is_mapped_but_not_reachable_until_it_is_swept() {
        let lc = CodeCacheLifecycle::with_simulated_quiescence();
        let depth = lc.simulated_depth().expect("simulated");
        let m = method_id("C", "m", "()V");

        lc.install(m, extent(500, 8192));
        lc.install(m, extent(500, 8192));
        lc.retire(m, extent(500, 8192), retire_reason::SUPERSEDED, None);

        depth.fetch_add(1, Ordering::SeqCst);
        lc.sweep();
        let r = lc.report();
        assert_eq!(r.live_bytes, 2 * 8192);
        assert_eq!(r.raw.deferred_bytes, 8192);
        assert_eq!(
            r.reachable_bytes, 8192,
            "the retired body is mapped but unreachable; only one body is in service",
        );
        assert_eq!(r.deferred_fraction, 0.5);
        depth.fetch_sub(1, Ordering::SeqCst);
    }

    // -----------------------------------------------------------------------
    // The grace period
    // -----------------------------------------------------------------------

    #[test]
    fn retirement_defers_while_a_thread_is_in_jit_and_proceeds_once_it_leaves() {
        let lc = CodeCacheLifecycle::with_simulated_quiescence();
        let depth = lc.simulated_depth().expect("simulated probe");
        let drops = Arc::new(AtomicU64::new(0));
        let freed_while_in_jit = Arc::new(AtomicU64::new(0));
        let m = method_id("C", "hot", "()V");

        lc.install(m, extent(1_000, 4096));

        // A thread enters compiled code.
        depth.fetch_add(1, Ordering::SeqCst);

        let owner: Option<Box<dyn Send>> = Some(Box::new(DropProbe {
            drops: Arc::clone(&drops),
            depth: Arc::clone(&depth),
            freed_while_in_jit: Arc::clone(&freed_while_in_jit),
        }));
        lc.retire(m, extent(1_000, 4096), retire_reason::INVALIDATED, owner);

        // Sweep repeatedly: every one must RETAIN.
        for expected_age in 1..=3u32 {
            let out = lc.sweep();
            assert!(
                !out.quiescent,
                "a thread is in JIT — sweep must not reclaim"
            );
            assert_eq!(out.reclaimed_bodies, 0);
            assert_eq!(out.deferred_bodies, 1);
            assert_eq!(out.deferred_bytes, 4096);
            assert_eq!(out.oldest_deferral_sweeps, expected_age);
            assert_eq!(drops.load(Ordering::Relaxed), 0, "freed live code");
        }
        let r = lc.report();
        assert_eq!(r.raw.sweeps, 3);
        assert_eq!(r.raw.sweeps_deferred, 3);
        assert_eq!(r.raw.deferrals, 3);
        assert_eq!(r.raw.max_deferral_sweeps, 3);
        assert_eq!(r.sweep_success_rate, 0.0);
        assert!(
            r.to_string().contains("RETAINED"),
            "the report must NAME the retention, or a leak is invisible: {r}",
        );

        // The thread leaves compiled code.
        depth.fetch_sub(1, Ordering::SeqCst);

        let out = lc.sweep();
        assert!(out.quiescent);
        assert_eq!(out.reclaimed_bodies, 1);
        assert_eq!(out.reclaimed_bytes, 4096);
        assert_eq!(out.deferred_bodies, 0);
        assert_eq!(drops.load(Ordering::Relaxed), 1);
        assert_eq!(
            freed_while_in_jit.load(Ordering::Relaxed),
            0,
            "no body may be unmapped while a thread is inside compiled code",
        );
        assert_eq!(lc.queued_bodies(), 0);
        assert_eq!(lc.report().raw.deferred_bytes, 0);
    }

    #[test]
    fn n_recompilations_reclaim_every_superseded_version() {
        const N: u32 = 8;
        let lc = CodeCacheLifecycle::with_simulated_quiescence();
        let m = method_id("C", "loop", "(I)I");
        let drops = Arc::new(AtomicU64::new(0));
        let depth = lc.simulated_depth().expect("simulated");
        let freed_while_in_jit = Arc::new(AtomicU64::new(0));

        // v1 installs; each later version supersedes the one before it.
        let first = lc.install(m, extent(200, 4096));
        assert_eq!(first.version, 1);
        assert!(!first.is_recompilation);

        for v in 2..=N {
            let installed = lc.install(m, extent(200, 4096));
            assert_eq!(installed.version, v);
            assert!(installed.is_recompilation);
            let owner: Option<Box<dyn Send>> = Some(Box::new(DropProbe {
                drops: Arc::clone(&drops),
                depth: Arc::clone(&depth),
                freed_while_in_jit: Arc::clone(&freed_while_in_jit),
            }));
            lc.retire(m, extent(200, 4096), retire_reason::SUPERSEDED, owner);
        }

        assert_eq!(lc.versions_of(m), N);
        assert_eq!(lc.queued_bodies(), (N - 1) as usize);

        let out = lc.sweep();
        assert!(out.quiescent);
        assert_eq!(out.reclaimed_bodies, (N - 1) as u64);
        assert_eq!(drops.load(Ordering::Relaxed), (N - 1) as u64);

        let r = lc.report();
        assert_eq!(r.raw.installs, N as u64);
        assert_eq!(r.raw.recompilations, (N - 1) as u64);
        assert_eq!(r.raw.methods_compiled, 1);
        assert_eq!(r.raw.max_versions_for_one_method, N as u64);
        assert_eq!(r.versions_per_method, N as f64);
        assert_eq!(
            r.live_bodies, 1,
            "N compilations of one method must leave exactly one live body",
        );
        assert_eq!(r.live_bytes, 4096);
        assert_eq!(
            r.raw.retirements_by_reason[retire_reason::SUPERSEDED],
            (N - 1) as u64,
        );
        assert!(r.to_string().contains("superseded-by-recompilation"), "{r}");
    }

    // -----------------------------------------------------------------------
    // Fragmentation
    // -----------------------------------------------------------------------

    #[test]
    fn fragmentation_is_computed_from_the_free_extent_shape() {
        // 1000 free bytes, biggest run 600 → 40% of the free space is
        // unreachable to a 600+ byte body.
        assert_eq!(
            external_fragmentation(FreeSpace {
                extents: 3,
                free_bytes: 1_000,
                largest_free_extent: 600,
            }),
            0.4,
        );
        // One contiguous run is not fragmented.
        assert_eq!(
            external_fragmentation(FreeSpace {
                extents: 1,
                free_bytes: 1_000,
                largest_free_extent: 1_000,
            }),
            0.0,
        );
        // A FULL cache is not a fragmented one, and must not divide by zero.
        assert_eq!(external_fragmentation(FreeSpace::default()), 0.0);
        // A nonsensical gauge (largest > total) clamps instead of going
        // negative.
        assert_eq!(
            external_fragmentation(FreeSpace {
                extents: 1,
                free_bytes: 100,
                largest_free_extent: 400,
            }),
            0.0,
        );

        // Internal fragmentation is buffer slack over live bytes.
        let lc = CodeCacheLifecycle::with_simulated_quiescence();
        let m = method_id("C", "m", "()V");
        // 3 KiB emitted into a 4 KiB page → 25% slack.
        lc.install(m, extent(3_072, 4_096));
        lc.record_capacity_bytes(16_384);
        lc.record_free_space(FreeSpace {
            extents: 3,
            free_bytes: 1_000,
            largest_free_extent: 600,
        });
        let r = lc.report();
        assert_eq!(r.internal_fragmentation, 0.25);
        assert_eq!(r.external_fragmentation, 0.4);
        assert_eq!(r.occupancy, 0.25);
        assert_eq!(extent(3_072, 4_096).slack_bytes(), 1_024);

        // Reclaiming the body takes its slack away with it.
        lc.retire(m, extent(3_072, 4_096), retire_reason::CACHE_PRESSURE, None);
        lc.sweep();
        let r = lc.report();
        assert_eq!(r.live_bytes, 0);
        assert_eq!(
            r.internal_fragmentation, 0.0,
            "an empty cache is not fragmented",
        );
    }

    // -----------------------------------------------------------------------
    // Failed allocations
    // -----------------------------------------------------------------------

    #[test]
    fn a_failed_allocation_is_counted_and_changes_nothing_else() {
        let lc = CodeCacheLifecycle::with_simulated_quiescence();
        let m = method_id("C", "big", "()V");
        lc.install(m, extent(1_000, 4_096));
        lc.retire(m, extent(1_000, 4_096), retire_reason::SUPERSEDED, None);
        let before = lc.report();

        lc.record_allocation_failure(1 << 20, alloc_failure::CAP_EXCEEDED);
        lc.record_allocation_failure(1 << 21, alloc_failure::NO_EXTENT_LARGE_ENOUGH);
        lc.record_allocation_failure(64, alloc_failure::OS_REFUSED);

        let after = lc.report();
        assert_eq!(after.raw.failed_allocations, 3);
        assert_eq!(
            after.raw.failed_allocation_bytes,
            (1u64 << 20) + (1u64 << 21) + 64,
        );
        assert_eq!(
            after.raw.failed_allocations_by_reason[alloc_failure::CAP_EXCEEDED],
            1,
        );

        // A refused allocation reserved nothing and withdrew nothing: every
        // occupancy figure and the retirement queue must be untouched.
        assert_eq!(after.raw.installs, before.raw.installs);
        assert_eq!(after.raw.installed_bytes, before.raw.installed_bytes);
        assert_eq!(after.raw.reclaimed_bytes, before.raw.reclaimed_bytes);
        assert_eq!(after.live_bytes, before.live_bytes);
        assert_eq!(after.live_bodies, before.live_bodies);
        assert_eq!(lc.queued_bodies(), 1, "the queue survived the failures");

        // ...and the queue still works afterwards.
        let out = lc.sweep();
        assert!(out.quiescent);
        assert_eq!(out.reclaimed_bodies, 1);
        assert_eq!(lc.report().live_bytes, 0);

        assert_eq!(lc.report().allocation_failure_rate, 3.0 / 4.0);
        assert!(
            lc.report().to_string().contains("code-cache-cap-exceeded"),
            "{}",
            lc.report(),
        );
    }

    // -----------------------------------------------------------------------
    // Concurrency
    // -----------------------------------------------------------------------

    #[test]
    fn concurrent_install_and_retire_lose_nothing_and_free_nothing_live() {
        const THREADS: u64 = 4;
        const PER_THREAD: u64 = 200;

        let lc = Arc::new(CodeCacheLifecycle::with_simulated_quiescence());
        let depth = lc.simulated_depth().expect("simulated");
        let drops = Arc::new(AtomicU64::new(0));
        let freed_while_in_jit = Arc::new(AtomicU64::new(0));

        // --- phase 1: a thread is inside compiled code for the whole phase --
        depth.fetch_add(1, Ordering::SeqCst);

        let mut handles = Vec::new();
        for t in 0..THREADS {
            let lc = Arc::clone(&lc);
            let drops = Arc::clone(&drops);
            let depth = Arc::clone(&depth);
            let freed = Arc::clone(&freed_while_in_jit);
            handles.push(std::thread::spawn(move || {
                for i in 0..PER_THREAD {
                    let m = method_id("C", "m", "()V").wrapping_add(t * 1_000 + i);
                    lc.install(m, extent(100, 1_024));
                    let owner: Option<Box<dyn Send>> = Some(Box::new(DropProbe {
                        drops: Arc::clone(&drops),
                        depth: Arc::clone(&depth),
                        freed_while_in_jit: Arc::clone(&freed),
                    }));
                    lc.retire(m, extent(100, 1_024), retire_reason::INVALIDATED, owner);
                }
            }));
        }
        for _ in 0..2 {
            let lc = Arc::clone(&lc);
            handles.push(std::thread::spawn(move || {
                for _ in 0..PER_THREAD {
                    let out = lc.sweep();
                    assert_eq!(
                        out.reclaimed_bodies, 0,
                        "a sweep reclaimed while a thread was in JIT",
                    );
                    std::thread::yield_now();
                }
            }));
        }
        for h in handles {
            h.join().expect("worker panicked");
        }

        let total = THREADS * PER_THREAD;
        assert_eq!(
            drops.load(Ordering::Relaxed),
            0,
            "nothing may be freed while a thread is inside compiled code",
        );
        assert_eq!(lc.queued_bodies() as u64, total);
        assert_eq!(lc.report().raw.deferred_bodies, total);

        // --- the thread leaves; everything becomes reclaimable -------------
        depth.fetch_sub(1, Ordering::SeqCst);
        let out = lc.sweep();
        assert!(out.quiescent);
        assert_eq!(out.reclaimed_bodies, total);
        assert_eq!(
            drops.load(Ordering::Relaxed),
            total,
            "a retirement was lost"
        );
        assert_eq!(freed_while_in_jit.load(Ordering::Relaxed), 0);

        // --- phase 2: install / retire / sweep all racing, no one in JIT ---
        let mut handles = Vec::new();
        for t in 0..THREADS {
            let lc = Arc::clone(&lc);
            let drops = Arc::clone(&drops);
            let depth = Arc::clone(&depth);
            let freed = Arc::clone(&freed_while_in_jit);
            handles.push(std::thread::spawn(move || {
                for i in 0..PER_THREAD {
                    let m = method_id("D", "m", "()V").wrapping_add(t * 1_000 + i);
                    lc.install(m, extent(100, 1_024));
                    let owner: Option<Box<dyn Send>> = Some(Box::new(DropProbe {
                        drops: Arc::clone(&drops),
                        depth: Arc::clone(&depth),
                        freed_while_in_jit: Arc::clone(&freed),
                    }));
                    lc.retire(m, extent(100, 1_024), retire_reason::SUPERSEDED, owner);
                }
            }));
        }
        for _ in 0..2 {
            let lc = Arc::clone(&lc);
            handles.push(std::thread::spawn(move || {
                for _ in 0..PER_THREAD {
                    lc.sweep();
                    std::thread::yield_now();
                }
            }));
        }
        for h in handles {
            h.join().expect("worker panicked");
        }
        // Drain whatever the racing sweeps left behind.
        while lc.queued_bodies() != 0 {
            let out = lc.sweep();
            assert!(
                out.quiescent,
                "no thread is in JIT, so a sweep must proceed"
            );
        }

        assert_eq!(
            drops.load(Ordering::Relaxed),
            2 * total,
            "every retired body must be reclaimed exactly once",
        );
        assert_eq!(freed_while_in_jit.load(Ordering::Relaxed), 0);

        let r = lc.report();
        assert_eq!(r.raw.installs, 2 * total);
        assert_eq!(r.raw.retirements, 2 * total);
        assert_eq!(r.raw.reclaimed_bodies, 2 * total);
        assert_eq!(
            r.live_bytes,
            r.raw.installed_bytes - r.raw.reclaimed_bytes,
            "installed - reclaimed == live, under concurrency",
        );
        assert_eq!(r.live_bytes, 0);
        assert_eq!(r.raw.deferred_bytes, 0);
        assert_eq!(r.reclaim_completion, 1.0);
    }

    // -----------------------------------------------------------------------
    // W^X
    // -----------------------------------------------------------------------

    #[test]
    fn no_wx_state_is_both_writable_and_executable() {
        // The structural half of the W^X claim: the enum has no RWX variant,
        // so no transition can produce one.
        for state in [
            WxState::Unmapped,
            WxState::Writable,
            WxState::Executable,
            WxState::Poisoned,
        ] {
            assert!(
                !(state.is_writable() && state.is_executable()),
                "{state:?} is both writable and executable",
            );
        }
        // ...and no legal transition from any state produces one either.
        for state in [
            WxState::Unmapped,
            WxState::Writable,
            WxState::Executable,
            WxState::Poisoned,
        ] {
            for event in [
                WxEvent::Allocate,
                WxEvent::Publish,
                WxEvent::Reopen,
                WxEvent::Release,
                WxEvent::Poison,
            ] {
                if let Ok(next) = wx_step(state, event) {
                    assert!(!(next.is_writable() && next.is_executable()));
                }
            }
        }
    }

    #[test]
    fn the_install_protocol_allocates_rw_then_publishes_rx() {
        let mut state = WxState::Unmapped;
        let mut seen_writable = false;
        for &event in INSTALL_WX_PROTOCOL.iter() {
            state = wx_step(state, event).expect("install protocol must be legal");
            seen_writable |= state.is_writable();
        }
        assert!(seen_writable, "code is written before it is published");
        assert_eq!(state, WxState::Executable);
        // Publishing something that was never mapped writable is refused.
        assert_eq!(
            wx_step(WxState::Unmapped, WxEvent::Publish),
            Err(WxViolation {
                state: WxState::Unmapped,
                event: WxEvent::Publish,
            }),
        );
    }

    #[test]
    fn retire_protocol_never_reopens_a_page() {
        // The retirement path must go straight from Executable to Unmapped.
        // Patching a retired body into a trap would need an RX->RW round trip,
        // which is a window where a still-executing thread reads half-written
        // instructions.
        let mut state = WxState::Executable;
        for &event in RETIRE_WX_PROTOCOL.iter() {
            assert_ne!(
                event,
                WxEvent::Reopen,
                "reclamation must not make a published page writable again",
            );
            state = wx_step(state, event).expect("retire protocol must be legal");
            assert!(!state.is_writable());
        }
        assert_eq!(state, WxState::Unmapped);
        assert_eq!(
            wx_replay(WxState::Executable, &RETIRE_WX_PROTOCOL),
            Ok(WxState::Unmapped),
        );
        // The diagnostic poison path is also non-writable.
        assert_eq!(
            wx_step(WxState::Executable, WxEvent::Poison),
            Ok(WxState::Poisoned),
        );
        assert!(!WxState::Poisoned.is_writable());
        assert!(!WxState::Poisoned.is_executable());
        // A full lifecycle replays end to end.
        assert_eq!(
            wx_replay(
                WxState::Unmapped,
                &[WxEvent::Allocate, WxEvent::Publish, WxEvent::Release],
            ),
            Ok(WxState::Unmapped),
        );
        assert!(wx_replay(WxState::Unmapped, &[WxEvent::Release]).is_err());
    }

    // -----------------------------------------------------------------------
    // Install protocol ordering
    // -----------------------------------------------------------------------

    #[test]
    fn the_install_sequence_must_flush_and_publish_in_order() {
        let ordered: Vec<usize> = (0..install_step::COUNT).collect();
        assert_eq!(install_sequence_is_legal(&ordered), Ok(()));

        // Flushing the I-cache AFTER the RW->RX flip races the fetch unit on a
        // non-coherent core.
        let mut late_flush = ordered.clone();
        late_flush.swap(
            install_step::FLUSH_ICACHE,
            install_step::TRANSITION_RW_TO_RX,
        );
        assert_eq!(
            install_sequence_is_legal(&late_flush),
            Err(InstallProtocolError::OutOfOrder {
                step: install_step::FLUSH_ICACHE,
                after: install_step::TRANSITION_RW_TO_RX,
            }),
        );

        // Publishing the entry before its metadata lets a peer enter the body
        // and be stack-walked with no oop map.
        let mut early_entry = ordered.clone();
        early_entry.swap(install_step::PUBLISH_METADATA, install_step::PUBLISH_ENTRY);
        assert_eq!(
            install_sequence_is_legal(&early_entry),
            Err(InstallProtocolError::OutOfOrder {
                step: install_step::PUBLISH_METADATA,
                after: install_step::PUBLISH_ENTRY,
            }),
        );

        // Skipping a step is a protocol error, not a fast path.
        let missing: Vec<usize> = ordered
            .iter()
            .copied()
            .filter(|&s| s != install_step::APPLY_RELOCATIONS)
            .collect();
        assert_eq!(
            install_sequence_is_legal(&missing),
            Err(InstallProtocolError::Missing(
                install_step::APPLY_RELOCATIONS
            )),
        );

        let mut duplicated = ordered.clone();
        duplicated.push(install_step::PUBLISH_ENTRY);
        assert_eq!(
            install_sequence_is_legal(&duplicated),
            Err(InstallProtocolError::Duplicate(install_step::PUBLISH_ENTRY)),
        );

        assert_eq!(
            install_sequence_is_legal(&[install_step::COUNT]),
            Err(InstallProtocolError::Unknown(install_step::COUNT)),
        );
    }

    // -----------------------------------------------------------------------
    // Report hygiene
    // -----------------------------------------------------------------------

    #[test]
    fn every_reason_and_step_has_a_label() {
        for code in 0..retire_reason::COUNT {
            assert_ne!(
                retire_reason::label(code),
                "unknown",
                "retire reason {code} needs a label — the report prints it verbatim",
            );
        }
        assert_eq!(retire_reason::label(retire_reason::COUNT), "unknown");
        for code in 0..alloc_failure::COUNT {
            assert_ne!(alloc_failure::label(code), "unknown");
        }
        assert_eq!(alloc_failure::label(alloc_failure::COUNT), "unknown");
        for step in 0..install_step::COUNT {
            assert_ne!(install_step::label(step), "unknown");
        }
        assert_eq!(install_step::label(install_step::COUNT), "unknown");
    }

    #[test]
    fn every_ratio_is_zero_when_its_denominator_is_zero() {
        let r = CodeCacheLifecycleReport::from_raw(CodeCacheLifecycleRaw::default());
        for v in [
            r.occupancy,
            r.internal_fragmentation,
            r.external_fragmentation,
            r.deferred_fraction,
            r.reclaim_completion,
            r.sweep_success_rate,
            r.recompilation_ratio,
            r.versions_per_method,
            r.allocation_failure_rate,
            r.mean_body_bytes,
        ] {
            assert_eq!(v, 0.0, "a zero denominator must normalize to 0.0, not NaN");
            assert!(v.is_finite());
        }
        let text = r.to_string();
        assert!(text.contains("nothing awaiting reclamation"), "{text}");
        assert!(text.contains("no failed allocations"), "{text}");
        assert!(!text.contains("NaN"), "{text}");
    }

    #[test]
    fn a_sweep_with_an_empty_queue_is_not_counted_as_deferred() {
        let lc = CodeCacheLifecycle::with_simulated_quiescence();
        let depth = lc.simulated_depth().expect("simulated");
        depth.fetch_add(1, Ordering::SeqCst);
        let out = lc.sweep();
        assert!(
            out.quiescent,
            "an empty queue has nothing to defer, so the sweep is trivially done",
        );
        assert_eq!(lc.report().raw.sweeps_deferred, 0);
        depth.fetch_sub(1, Ordering::SeqCst);
    }

    // -----------------------------------------------------------------------
    // The real quiescence signal
    // -----------------------------------------------------------------------

    /// The production instance must read `GLOBAL_JIT_DEPTH` and nothing else.
    ///
    /// Uses a locally-constructed lifecycle (whose probe is the *real* signal)
    /// rather than the process one, so it neither observes nor disturbs other
    /// tests' retirements. Only the deferral half is asserted unconditionally:
    /// this thread being in JIT makes `any_thread_in_jit()` true no matter what
    /// any other test thread is doing, whereas the reclaim half depends on
    /// every other thread also being out.
    #[test]
    fn the_production_quiescence_signal_is_the_global_jit_depth() {
        use crate::jit::conservative_roots;

        let lc = CodeCacheLifecycle::new();
        assert!(
            lc.simulated_depth().is_none(),
            "CodeCacheLifecycle::new must read the real in-JIT depth",
        );
        let drops = Arc::new(AtomicU64::new(0));
        let never_in_jit = Arc::new(AtomicUsize::new(0));
        let freed_while_in_jit = Arc::new(AtomicU64::new(0));
        let m = method_id("C", "compiled", "()V");
        lc.install(m, extent(256, 4_096));

        // A stack address well above any later scanner SP, so the entry is not
        // pruned as "provably returned" while we hold it.
        let anchor = 0u64;
        let sp = &anchor as *const u64 as usize;
        conservative_roots::push_jit_entry_at(sp);
        assert!(
            conservative_roots::any_thread_in_jit(),
            "this thread is inside compiled code",
        );

        let owner: Option<Box<dyn Send>> = Some(Box::new(DropProbe {
            drops: Arc::clone(&drops),
            depth: Arc::clone(&never_in_jit),
            freed_while_in_jit: Arc::clone(&freed_while_in_jit),
        }));
        lc.retire(m, extent(256, 4_096), retire_reason::DEOPTIMIZED, owner);

        for _ in 0..3 {
            let out = lc.sweep();
            assert!(
                !out.quiescent,
                "GLOBAL_JIT_DEPTH is elevated, so reclamation must be deferred",
            );
            assert_eq!(out.reclaimed_bodies, 0);
            assert_eq!(drops.load(Ordering::Relaxed), 0);
        }

        conservative_roots::pop_jit_entry();

        // Now only *other* threads can hold the depth up. Retry generously:
        // vm-crate unit tests that push JIT entries are short-lived, so a
        // handful of yields is ample.
        let mut reclaimed = false;
        for _ in 0..10_000 {
            if lc.sweep().reclaimed_bodies == 1 {
                reclaimed = true;
                break;
            }
            std::thread::yield_now();
        }
        assert!(
            reclaimed,
            "once no thread is in JIT the body must be reclaimed; \
             deferred forever means the entry chain is wedged",
        );
        assert_eq!(drops.load(Ordering::Relaxed), 1);
        assert_eq!(lc.report().live_bytes, 0);
    }

    /// The process shim is wired to the same machinery, and its identity holds
    /// even though other tests share it. Deltas and identities only — see the
    /// `PROCESS_LIFECYCLE` doc comment.
    #[test]
    fn the_process_shim_maintains_the_installed_minus_reclaimed_identity() {
        let before = code_cache_lifecycle_raw();
        let m = method_id("shim/Probe", "run", "()V");
        record_install(m, extent(128, 2_048));
        let after = code_cache_lifecycle_raw();
        assert!(after.installs >= before.installs + 1);
        assert!(after.installed_bytes >= before.installed_bytes + 2_048);

        record_retirement(m, extent(128, 2_048), retire_reason::SHUTDOWN, None);
        // Drain: the JIT-leave hook may also have swept it already, which is
        // the point of the shared instance and is why nothing here asserts an
        // absolute queue length.
        while pending_retirements() != 0 && sweep_if_quiescent().quiescent {}

        let r = code_cache_lifecycle_report();
        assert_eq!(
            r.live_bytes,
            r.raw.installed_bytes.saturating_sub(r.raw.reclaimed_bytes),
            "installed - reclaimed == live",
        );
        assert_eq!(
            r.live_bodies,
            r.raw.installs.saturating_sub(r.raw.reclaimed_bodies),
        );
        assert_eq!(
            pending_retirements(),
            process_lifecycle().queued_bodies(),
            "the hot-path gate and the queue it guards must be the same number",
        );
    }
}
