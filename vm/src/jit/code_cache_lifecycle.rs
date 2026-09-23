// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Code-cache **lifecycle**: the installation / invalidation protocol, the
//! epoch retirement that reclaims superseded bodies, and the counters that make
//! both auditable.
//!
//! > **READ THIS FIRST IF YOU CAME HERE TO AUDIT CODE RECLAMATION.**
//! > [`CodeCacheLifecycle`] is an executable MODEL. The production retirement
//! > queue is `deferred_jit_owners` / `defer_jit_owner` /
//! > `drain_deferred_jit_owners` in `jit/src/lib.rs`, and the two designs are
//! > not the same: this model's grace period is a single all-or-nothing
//! > striped-counter walk, while production has that **plus** per-thread
//! > `ThreadQuiescenceEvidence` with blocked-stack scanning — a materially
//! > more subtle argument, and the one this module's tests do not reach.
//! > Reading the 2,400 lines below and coming away reassured is reassurance
//! > about the simpler design. Section 4 says as much at the bottom of this
//! > header; it is repeated here because the bottom is 200 lines away and the
//! > failure mode is stopping before it.
//! >
//! > **Where the PRODUCTION queue is actually tested** -- the question the
//! > paragraph above sends you away with, answered:
//! >
//! > * the process-wide fast path (retire while a thread is in compiled code,
//! >   release once it leaves): `replaced_body_survives_until_jit_execution_
//! >   is_quiescent` in `jit/src/tests.rs`. This is the production twin of
//! >   this module's `retirement_defers_while_a_thread_is_in_jit_and_proceeds_
//! >   once_it_leaves`, asserted against the real `defer_jit_owner`.
//! > * the per-thread evidence -- the subtle half this model does not have:
//! >   `mod retirement_quiescence_tests` in `jit/src/lib.rs` (a blocked thread
//! >   out since the stamp permits whatever its stack holds; one whose scan
//! >   found no code permits; one parked inside the body does not).
//! > * the lock order the drain's soundness argument rests on (queue before
//! >   `jit_threads()`): `mod retire_queue_lock_order_tests` in
//! >   `jit/src/lib.rs`.
//! >
//! > `NOTES-runtime.md` RT-3 item 3 proposed a `QuiescenceSource` trait so
//! > this module's two pinned tests could run against the real queue. It was
//! > not built, deliberately, on 2026-09-17. Of the two tests it named,
//! > `retire_protocol_never_reopens_a_page` has no quiescence dependency at
//! > all -- it is a `WxState` state-machine property, the same category the
//! > note itself exempted `no_wx_state_is_both_writable_and_executable` for --
//! > and the other's property is already pinned against production by the
//! > first bullet. A trait bridging two designs this header says differ
//! > materially would have bought an abstraction and no coverage.
//!
//! This closes two adjacent items of the C2 review
//! (`deep-research-vm-c2.md`):
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
//! **In the process report only the first half of that signal exists.**
//! `deferred_bodies` and `deferred_bytes` are pulled from the real queue's
//! gauges; `max_deferral_sweeps` is not, because the queue exposes no per-body
//! age (§5). So the wedged-chain reading is "bytes that never fall across two
//! reports", not "an age that climbs", and the report says so where it would
//! otherwise have printed an age. Restoring the second half is one accessor —
//! see `docs/feature-designs/jit-r10-report-proposals.md`.
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
//! `gc_metrics.rs` applies to its collector-side counters). The only boundary
//! touch here is [`pending_retirements`], one relaxed load of the JIT retirement
//! queue's gauge, called from `prune_returned_jit_entries`; the per-exit wake-up
//! lives in `cratonvm_jit::jit_execution_leave`, which makes the same one-load
//! check first. With nothing queued — the overwhelmingly common case — that is
//! the entire cost.
//!
//! # 5. The process report reads the real queue
//!
//! The retirement queue that actually holds executable memory is
//! `cratonvm_jit`'s: `defer_jit_owner` queues every withdrawn owner, stamped
//! with a retirement generation, and its drain releases a body either when the
//! process-wide in-JIT count reads zero or on per-thread evidence — every
//! running JIT thread has returned to depth 0 since the body's stamp, and no
//! thread blocked inside compiled code has a stack word pointing into it
//! (`docs/jit/code-cache-lifetime.md`, "Reclaiming while a thread is parked in
//! compiled code"). The drain takes the queue lock before any observation,
//! which is §1.2's ordering, and retention stays the counted fail-safe.
//!
//! This module used to keep a second, parallel queue whose install and retire
//! counters no production path fed: `record_install` and `record_retirement`
//! had no callers, so the report and the JIT-leave gate always read zero while
//! the real queue grew. The process-level functions are now views of the real
//! one:
//!
//! * [`pending_retirements`] is its queued-owner gauge;
//! * [`sweep_if_quiescent`] runs its drain;
//! * [`code_cache_lifecycle_raw`] takes installs, withdrawals, queued and
//!   reclaimed bytes and drain counts from
//!   `cratonvm_jit::jit_code_reclamation_stats`.
//!
//! Round 10 finished that inversion. Three fields were left out of it —
//! `failed_allocations`, `failed_allocation_bytes` and
//! `failed_allocations_by_reason` — on the grounds that they were "gauges the
//! JIT's real accounting does not carry", to be pushed in by
//! `record_allocation_failure`. They never were, and the reason was structural
//! rather than an unfinished to-do: `cratonvm-vm` depends on `cratonvm-jit`
//! and there is no edge back, so no code in the crate that refuses an
//! allocation can name this module. The JIT now carries them
//! (`cratonvm_jit::jit_code_alloc_failure_stats`) and
//! `code_cache_lifecycle_raw` pulls them. The configured cap had
//! already been pulled from
//! `jit_code_cache_cap_bytes` and was never actually zero; the free-space
//! triple is pulled from the arena census when the arena is on and is
//! structurally zero when it is off.
//!
//! Wave 6 finished the tidy-up that inversion implied. The three process-level
//! `record_*` free functions — the push entry points for exactly those gauges —
//! were deleted, and the model's three instance recorders they wrapped are now
//! private and `#[cfg(test)]`. The reasoning, including what each of this
//! tree's two frozen ratchets had to say about it, is in the banner above
//! `CodeCacheLifecycle::record_allocation_failure` and in the note where the
//! wrappers used to be.
//!
//! Wave 7 closed the last of it, in the order the known-issues page demanded.
//!
//! **The seven fields §5 did not cover.** `recompilations`,
//! `methods_compiled`, `max_versions_for_one_method`, `retirements_by_reason`,
//! `deferrals`, `max_deferral_sweeps` and `peak_live_bytes` have no source in
//! the process report at all: only an instance's `install`/`retire`/`sweep`
//! write them, nothing does that in production, and the JIT's accounting
//! carries no equivalent to pull. Two of the resulting readings were
//! arithmetically impossible rather than merely zero (a `peak` below the
//! current value; `versions_per_method` under `1.0` with installs recorded),
//! which is what made them findable by reading. That was the same disease this
//! section records curing once, still present in the same struct.
//!
//! They were SUPPRESSED first, visibly. `CodeCacheLifecycleRaw::modelled_sources`
//! records which of four modelled groups a snapshot has a producer for —
//! `ModelledFieldSources::ALL` from an instance — and `Display` prints a named
//! `n/a` with its reason in place of each group it has no producer for. Nothing
//! the process report prints is a number without a source.
//!
//! **Wave 8 then gave three of them producers, in `cratonvm_jit`, which is where
//! they always had to live.** Four of the seven fields now carry measurements on
//! the process path and three remain suppressed, and the split is per FIELD
//! rather than per page:
//!
//! * `peak_live_bytes` — FED. `cratonvm_jit` samples
//!   `installed_bytes - reclaimed_bytes` into a `fetch_max` at each publish, the
//!   only event that can raise it. The sample is a LOWER bound (two independent
//!   relaxed loads, saturating subtraction), so `code_cache_lifecycle_raw`
//!   clamps it with `.max(live)` — which is what keeps `peak >= live`, the
//!   identity whose violation was symptom one.
//! * `retirements_by_reason` — FED. Every withdrawal site in `cratonvm_jit`
//!   passes a `retire_reason` code, chosen by the named entry point that
//!   withdrew, so the report can finally tell cache-pressure eviction from class
//!   unloading. Two codes have no site and are reserved; see
//!   `cratonvm_jit::retire_reason`.
//! * the deferral AGE — FED, in a NEW field (`oldest_deferral_generations`) and
//!   a new unit. The queue ages bodies in retirement generations, not sweeps,
//!   and a number in the wrong unit is worse than a suppressed one.
//! * `deferrals` and `max_deferral_sweeps` — STILL SUPPRESSED. Both are in
//!   SWEEPS; the JIT queue does not count drains per queued owner, and nothing
//!   short of a write per queued owner per drain would make it.
//!
//! **Wave 9 fed the fourth group, and with it the second impossible reading.**
//!
//! * `recompilations`, `methods_compiled`, `max_versions_for_one_method` — FED,
//!   from `cratonvm_jit::jit_compilation_census()`: one publication count per
//!   method key, filled at `JitCache::put` and `JitCache::put_osr` and gated on
//!   the same `ExecutableBuffer::mark_published` call that bumps
//!   `installed_bodies`. That gating is the whole design: it makes
//!   `methods_compiled <= installs` true by construction, and therefore
//!   `versions_per_method >= 1.0`, which is the identity whose violation was
//!   symptom two. Not wired from `tiered::CompilationStats::
//!   nominate_to_first_body`, which reads like an exact fit and is a lower bound
//!   behind a second `if`; see `docs/feature-designs/jit-r10-producers-proposals.md`.
//!   The census is bounded, and past its bound it answers `None` and the group
//!   goes back to being suppressed rather than reporting a truncated table.
//!
//! So of the seven, five are now measured on the process path and two —
//! `deferrals` and `max_deferral_sweeps` — stay suppressed because their UNIT,
//! not their measurement, is what the JIT does not keep.
//!
//! **And §5's report now has a reader.** Wave 5 built the pull and wave 6
//! tidied it, but nothing in production called `code_cache_lifecycle_report()`
//! — every ratio and the whole `Display` ran only under `cargo test`, which is
//! the "written, documented, tested and never shown" half of
//! `docs/internal/retired/r10-wrappers-process-report-has-no-reader-and-seven-unfed-fields-20260921-RETIRED-20260922.md`.
//! `maybe_dump_shutdown_reports` in `vm-cli/src/main.rs` calls it under
//! `flags().jit.method_stats`, and the suppression above landed in the same
//! change and before it, because that page's own instruction was that printing
//! an impossible number is worse than printing nothing.
//!
//! Three DERIVED values were in the milder version of the same state —
//! `reachable_bytes`, `sweep_success_rate` and `mean_body_bytes` were computed
//! by `from_raw` on every call, asserted by this module's tests and printed by
//! nothing. All three come entirely from pulled inputs, so they are printed
//! now; no new producer was needed and no claim changed.
//!
//! [`CodeCacheLifecycle`] itself remains as the executable model of the
//! protocol — its tests pin the accounting identities, the W^X steps and the
//! lock ordering against a simulated quiescence signal — but no production path
//! installs into or retires through an instance of it.
//!
//! (`vm/src/runtime/jit_integration.rs`, a never-referenced parallel model of
//! this whole layer, was deleted on 2026-09-02.)

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
    ///
    /// **Reserved, with no producer, deliberately.** `JitCodeArena::alloc` maps
    /// a new region rather than refusing a request no free extent can serve, and
    /// without the arena there is no free list at all, so nothing in this tree
    /// can file this code; the round-10 allocation-failure wiring left it that
    /// way on purpose and records OS refusals as [`OS_REFUSED`], which is what
    /// they are. It is not an orphaned instrument: `allocation_failure_breakdown`
    /// filters `n != 0` BEFORE it labels, so this bucket has never been printed
    /// by any build — an absent label, not a misleading zero
    /// (`a_zero_bucket_is_never_labelled` pins that, and is the whole reason
    /// keeping it costs nothing). Deleting it would renumber
    /// [`SIZE_ESTIMATE_OVERRUN`] in two crates at once and mislabel every
    /// histogram captured from an older build. The full argument, including why
    /// pointing it at a capped arena would be a MISdiagnosis rather than a fix,
    /// is on `cratonvm_jit::code_alloc_failure::NO_EXTENT_LARGE_ENOUGH`.
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
    /// Which sweep this was.
    ///
    /// From [`CodeCacheLifecycle::sweep`] it is a 1-based sweep sequence
    /// number. From [`sweep_if_quiescent`] it is the JIT retirement queue's
    /// cumulative `drains` count AFTER this drain — which is the same quantity
    /// (the Nth drain is the Nth sweep of that queue) but reached by a
    /// different route, and it counts drains any other thread performed too.
    /// The `Display` below labels it `sweep #` for both.
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
    /// The most sweeps any still-queued body has now survived, or `None` when
    /// the sweeper cannot answer.
    ///
    /// `Option` rather than `u32`, as of round 10 wave 7, and the reason is the
    /// same defect class as [`ModelledFieldSources`] one level down.
    /// [`sweep_if_quiescent`] — the only production sweeper — hard-coded this
    /// to `0`, and the `Display` below printed it in the NOT-QUIESCENT branch,
    /// which is the one branch where a climbing age is the whole signal (§1.4:
    /// a wedged entry chain shows up as bytes that never fall and an age that
    /// climbs). A constant `0` there reads as "this body was queued moments
    /// ago" on a body that may have been stuck for the life of the process.
    ///
    /// `None` makes that unsayable. [`CodeCacheLifecycle::sweep`] answers
    /// `Some`, because the model tracks a per-body `deferrals` count; the
    /// process path answers `None` because it has no SWEEP count to give — as
    /// of round 10 wave 8 it answers [`Self::oldest_deferral_generations`]
    /// instead, which is the age the queue really does know, in the unit the
    /// queue really keeps.
    pub oldest_deferral_sweeps: Option<u32>,
    /// Age of the oldest owner in the JIT retirement queue, in RETIREMENT
    /// GENERATIONS, or `None` when this sweeper cannot answer.
    ///
    /// The process path ([`sweep_if_quiescent`]) fills this from
    /// `cratonvm_jit::jit_oldest_retirement_age()`;
    /// [`CodeCacheLifecycle::sweep`] leaves it `None` because the model has no
    /// notion of a retirement generation. The mirror image of
    /// [`Self::oldest_deferral_sweeps`], and the units are NOT interchangeable —
    /// see [`CodeCacheLifecycleRaw::oldest_deferral_generations`] for why they
    /// are two fields rather than one.
    pub oldest_deferral_generations: Option<u64>,
}

impl std::fmt::Display for SweepOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "[JIT] code-cache sweep #{seq}: {verdict} reclaimed={bodies} bodies / {bytes} bytes; \
             deferred={dbodies} bodies / {dbytes} bytes",
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
        )?;
        // The age clause, and only when there is an age. See the field's doc:
        // this used to be an unconditional `(oldest deferred {age} sweeps)` fed
        // by a hard-coded `0` on the one path an operator would ever see.
        //
        // Two units, each printed only by the sweeper that keeps it, and
        // NEVER folded into one clause. See
        // `CodeCacheLifecycleRaw::oldest_deferral_generations`: a retirement
        // generation advances per withdrawal and a sweep per drain, so the two
        // numbers are not comparable and a reader must be told which they have.
        // The unit is spelled out in the text for exactly that reason.
        match (
            self.oldest_deferral_sweeps,
            self.oldest_deferral_generations,
        ) {
            // BOTH is unreachable today — the model fills sweeps and the process
            // path fills generations — and it is handled anyway rather than left
            // to a `(Some(age), _)` arm that would silently drop the second
            // figure. A sweeper that learns to answer in both units would
            // otherwise have half its answer discarded by a `match` nobody
            // revisited, which is this round's defect class wearing a wildcard.
            (Some(age), Some(gens)) => write!(
                f,
                " (oldest deferred {age} sweeps / {gens} retirement generations — two \
                 different units, see the field docs)"
            ),
            (Some(age), None) => write!(f, " (oldest deferred {age} sweeps)"),
            (None, Some(gens)) => write!(
                f,
                " (oldest deferred {gens} retirement generations — a generation is one \
                 WITHDRAWAL, not one drain, so this is not a sweep count)"
            ),
            (None, None) => write!(
                f,
                " (oldest deferral age n/a — the JIT retirement queue exposes no per-body age)"
            ),
        }
    }
}

// ---------------------------------------------------------------------------
// The lifecycle
// ---------------------------------------------------------------------------

/// A code cache's lifecycle accounting and its retirement queue.
///
/// The process-level functions ([`pending_retirements`], [`sweep_if_quiescent`],
/// [`code_cache_lifecycle_raw`]) report the real JIT retirement queue, not an
/// instance of this model; tests construct their own. An instance built with
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

    // -----------------------------------------------------------------------
    // The three gauge recorders: MODEL-ONLY, and `#[cfg(test)]` for that reason
    //
    // Round 10 (wave 6, lane `wrappers`), closing
    // `docs/known-issues/jit/r10-gauges-code-cache-push-wrappers-outlived-their-direction-20260921.md`.
    //
    // These three used to have process-level free-function twins
    // (`record_allocation_failure` / `record_capacity_bytes` /
    // `record_free_space`) that pushed into `PROCESS_LIFECYCLE`. Those twins
    // were deleted: the push direction they modelled is not expressible in
    // this crate graph at all, because every site that can refuse a code-cache
    // allocation, know the configured cap or describe the arena's free list is
    // inside `cratonvm-jit`, and `cratonvm-vm` depends on `cratonvm-jit` with
    // no edge back. Wave 5 inverted the direction — `code_cache_lifecycle_raw`
    // PULLS all three groups now — which left the push side with no caller and
    // no way ever to acquire one.
    //
    // What is left is the MODEL's own recorders, and they are gated rather
    // than left `pub` because leaving them `pub` is what the two frozen
    // ratchets in this tree both object to, for the same underlying reason:
    //
    //   * `vm/tests/no_test_only_public_api.rs` — a `pub` item in `vm/src`
    //     whose only references outside its own declaration are test
    //     references is an "offender". With the free functions deleted, each
    //     of these three names would have had exactly one production line (its
    //     own declaration) plus this module's test calls, which is that
    //     predicate exactly, and the offender count would have gone 291 -> 294
    //     against an `assert_eq!` on a frozen number.
    //   * `scripts/check-orphan-instruments.sh` C1 — `pub fn record_*` whose
    //     body touches an atomic and which nothing calls from another FILE.
    //     That check is textual and does not read `cfg`, so a `#[cfg(test)]`
    //     `pub fn record_…(` would still be a candidate; dropping `pub` is
    //     what takes the three names off its census.
    //
    // Dropping `pub` alone would trade both for a `dead_code` warning on every
    // non-test build, since the only callers are in `mod tests` below — which
    // is precisely the "an item whose sole real caller is a test should be
    // `#[cfg(test)]` itself" remedy `no_test_only_public_api.rs`'s header
    // offers. So: private AND gated. They keep their coverage (the accounting
    // identities below, and `external_fragmentation`'s only non-arena test)
    // and they stop advertising an entry point the crate graph cannot serve.
    //
    // If a future embedder genuinely needs to push these numbers in, the
    // wrappers are `git show <this commit>` away — but read
    // `docs/feature-designs/jit-r10-wrappers-proposals.md` first: what it
    // would actually need is a pull from wherever that embedder keeps them,
    // the same shape §5 of this header already describes.
    // -----------------------------------------------------------------------

    /// Record an allocation the code cache refused.
    ///
    /// Touches **no** occupancy counter: a refused allocation reserved nothing,
    /// so `installed - reclaimed == live` must survive it unchanged. `reason`
    /// is an [`alloc_failure`] code.
    ///
    /// Model-only; see the banner above. The process report's equivalent
    /// figures are pulled from `cratonvm_jit::jit_code_alloc_failure_stats`.
    #[cfg(test)]
    fn record_allocation_failure(&self, requested_bytes: u64, reason: usize) {
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
    ///
    /// Model-only; see the banner above. The process report's capacity is
    /// pulled from `cratonvm_jit::jit_code_cache_cap_bytes`.
    #[cfg(test)]
    fn record_capacity_bytes(&self, bytes: u64) {
        self.counters.capacity_bytes.store(bytes, Ordering::Relaxed);
    }

    /// Publish the arena's free-space shape (a gauge). Feeds
    /// [`external_fragmentation`].
    ///
    /// Model-only; see the banner above. The process report's free-space
    /// triple is pulled from `cratonvm_jit::platform::jit_code_arena_census`
    /// when the shared arena is on, and is structurally zero when it is off
    /// (every body is its own mapping, so there is no free list to describe).
    #[cfg(test)]
    fn record_free_space(&self, free: FreeSpace) {
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
                // `Some`, always: the model keeps a per-body `deferrals` count,
                // so it can always answer, including with the `0` that
                // `max().unwrap_or(0)` gives for an empty queue — which is a
                // real zero (nothing is queued) rather than an unknown.
                oldest_deferral_sweeps: Some(oldest_deferral_sweeps),
                // `None`: the model has no notion of a retirement generation.
                // It counts sweeps, which is the unit above. See
                // `CodeCacheLifecycleRaw::oldest_deferral_generations`.
                oldest_deferral_generations: None,
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
            // `None`, for the same reason `SweepOutcome`'s twin field is: the
            // model ages queued bodies in SWEEPS, which the field above
            // carries. Leaving this `None` is what keeps an instance's report
            // from printing two ages in two units, one of them invented.
            oldest_deferral_generations: None,
            failed_allocations: c.failed_allocations.load(Ordering::Relaxed),
            failed_allocation_bytes: c.failed_allocation_bytes.load(Ordering::Relaxed),
            failed_allocations_by_reason,
            capacity_bytes: c.capacity_bytes.load(Ordering::Relaxed),
            free_extents: c.free_extents.load(Ordering::Relaxed),
            free_bytes: c.free_bytes.load(Ordering::Relaxed),
            largest_free_extent: c.largest_free_extent.load(Ordering::Relaxed),
            peak_live_bytes: c.peak_live_bytes.load(Ordering::Relaxed),
            // An INSTANCE feeds all four modelled groups by construction:
            // `install` writes the compilation census and the peak, `retire`
            // writes the per-reason histogram, `sweep` writes the deferral
            // count and age. So an instance's report prints them, and the
            // suppression below applies only to the process pull, which has a
            // producer for none of them. This is the line that keeps the
            // model's own tests reading real numbers.
            modelled_sources: ModelledFieldSources::ALL,
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
    /// Most sweeps survived by any one queued body. **Modelled** — see
    /// [`CodeCacheLifecycleRaw::modelled_sources`]; the process path has no
    /// producer for this and reports [`Self::oldest_deferral_generations`]
    /// instead, in a different unit.
    pub max_deferral_sweeps: u64,
    /// Age of the oldest owner in the JIT retirement queue, in RETIREMENT
    /// GENERATIONS, or `None` when the producer cannot answer.
    ///
    /// # Why this is a separate field and not poured into `max_deferral_sweeps`
    ///
    /// Because it is a different unit, and a number in the wrong unit is worse
    /// than a suppressed one. [`Self::max_deferral_sweeps`] is "most SWEEPS
    /// survived by any one queued body", which [`CodeCacheLifecycle::sweep`]
    /// counts by bumping a per-body `deferrals` on every sweep that retains. A
    /// retirement generation advances once per WITHDRAWAL
    /// (`cratonvm_jit`'s `bump_retire_generation`), not once per drain, so a
    /// queue holding one wedged body while a hundred other methods are
    /// recompiled reports an age of ~100 having survived possibly one drain.
    ///
    /// Both answer "has this been stuck a long time?", which is why
    /// `docs/feature-designs/jit-r10-report-proposals.md` was tempted to assign
    /// one into the other, and neither is wrong — but they are not
    /// interchangeable, and quietly relabelling generations as sweeps is the
    /// same class of error the `const` numbering assertions in this file exist
    /// to prevent one level down. So the process path fills THIS field and
    /// leaves `max_deferral_sweeps` suppressed, and the model fills
    /// `max_deferral_sweeps` and leaves this `None`.
    ///
    /// # Why `Option` and not a flag in `ModelledFieldSources`
    ///
    /// The provenance IS the value here — there is exactly one field in the
    /// group, so a separate flag would be a second copy of the same bit that
    /// could disagree with it. `None` also has to be distinguishable from `0`
    /// for a reason that has nothing to do with provenance: an age of `0` means
    /// "queued by the most recent withdrawal", i.e. genuinely fresh, and that is
    /// a real reading the producer can return. Same shape, and the same
    /// argument, as [`SweepOutcome::oldest_deferral_sweeps`] in this file.
    pub oldest_deferral_generations: Option<u64>,
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
    /// High-water mark of live bytes. **Modelled** — see
    /// [`CodeCacheLifecycleRaw::modelled_sources`].
    pub peak_live_bytes: u64,
    /// Which of the four MODELLED field groups this snapshot has a producer
    /// for. `Display` prints a group only when its flag is set, and prints a
    /// named `n/a` instead when it is not.
    ///
    /// This is the field that stops the process report from printing an
    /// arithmetically impossible number. See [`ModelledFieldSources`].
    pub modelled_sources: ModelledFieldSources,
}

/// Which of [`CodeCacheLifecycleRaw`]'s **modelled** field groups a snapshot
/// actually has a producer for.
///
/// # Why a snapshot has to carry its own provenance
///
/// [`CodeCacheLifecycleRaw`] is produced two ways, and they do not feed the
/// same fields:
///
/// * [`CodeCacheLifecycle::raw`] — an instance of the model. Its
///   `install`/`retire`/`sweep` write every field, so every group is fed and
///   this is [`Self::ALL`].
/// * [`code_cache_lifecycle_raw`] — the process report, which PULLS from
///   `cratonvm_jit`. It starts from [`Self::NONE`] and flips the flags it has a
///   producer for: as of round 10 wave 9 that is [`Self::peak_live_bytes`],
///   [`Self::retire_reasons`] and [`Self::compilation_census`], and not
///   [`Self::deferral_age`]. The incremental shape below is what that
///   granularity was for, and it has now been exercised three waves running.
///
///   [`Self::compilation_census`] is the one of the four whose flag is
///   CONDITIONAL on the producer rather than on the producer existing:
///   `cratonvm_jit::jit_compilation_census()` returns an `Option`, `None`
///   meaning its bounded per-method table stopped being exact, and the pull
///   flips the flag only on `Some`. A group can therefore be suppressed at
///   runtime on a process that has the producer, which is the state the
///   granularity is for now.
///
/// Before round 10 wave 7 the two were indistinguishable in the output, and
/// that was not merely a missing figure — it made the process report print two
/// readings no fed counter set can produce:
///
/// * `peak_live_bytes` at `0` beside a large `live_bytes`. A high-water mark
///   cannot be below the current value.
/// * `versions_per_method` at `0.0` beside a positive `installs`, because
///   `methods_compiled` was `0` and [`ratio`] answers `0.0` rather than `NaN`
///   for a zero denominator. A fed counter cannot put that below `1.0`.
///
/// An operator acting on either would be chasing a defect that is not there,
/// which is why
/// `docs/internal/retired/r10-wrappers-process-report-has-no-reader-and-seven-unfed-fields-20260921-RETIRED-20260922.md`
/// ordered the suppression BEFORE the print site. A missing line is honest; a
/// `0` is not.
///
/// # Why four groups rather than seven flags or one bool
///
/// The groups are the units `Display` decides in, not the fields. Each one is
/// exactly one printed fragment, and every field in a group shares a single
/// producer, so no configuration this type can express is one the JIT could
/// reach and this type could not. One bool would have been enough in wave 7 —
/// the two producers fed all four or none — but the suggested fix was explicitly
/// incremental, and **wave 8 proved the granularity was not hypothetical**: the
/// pull now sets two of the four flags and leaves two clear, which is a state one
/// bool could not have expressed at all.
///
/// # What four flags turned out NOT to be enough for, which is worth recording
///
/// The deferral age. `Self::deferral_age` gates two fields that are both in
/// SWEEPS, and the producer wave 8 found answers in retirement GENERATIONS —
/// a different unit, not a different provenance. Adding a fifth flag would have
/// been wrong twice over: it would have made [`Self::ALL`] a lie (an instance
/// feeds no generation count, so `ALL` would claim a producer the model does not
/// have, which is this type's own disease one level down), and it would have put
/// the provenance of a single field in a second place that can disagree with it.
/// So that one is carried as
/// [`CodeCacheLifecycleRaw::oldest_deferral_generations`], an `Option` whose
/// `None` IS the suppression — the same shape
/// [`SweepOutcome::oldest_deferral_sweeps`] already uses in this file. The rule
/// that falls out: a flag per printed FRAGMENT, an `Option` per field whose
/// absence is itself a reading.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ModelledFieldSources {
    /// [`CodeCacheLifecycleRaw::recompilations`],
    /// [`CodeCacheLifecycleRaw::methods_compiled`] and
    /// [`CodeCacheLifecycleRaw::max_versions_for_one_method`] — and therefore
    /// also the derived [`CodeCacheLifecycleReport::recompilation_ratio`] and
    /// [`CodeCacheLifecycleReport::versions_per_method`], which are not
    /// derivable without them.
    pub compilation_census: bool,
    /// [`CodeCacheLifecycleRaw::retirements_by_reason`], and therefore
    /// [`CodeCacheLifecycleReport::retire_reason_breakdown`].
    pub retire_reasons: bool,
    /// [`CodeCacheLifecycleRaw::deferrals`] and
    /// [`CodeCacheLifecycleRaw::max_deferral_sweeps`] — the deferral COUNT and
    /// the oldest queued body's AGE. Grouped because one producer supplies
    /// both: whatever can age a queued body can count the ageing.
    pub deferral_age: bool,
    /// [`CodeCacheLifecycleRaw::peak_live_bytes`].
    pub peak_live_bytes: bool,
}

impl ModelledFieldSources {
    /// Every group is fed: what an instance of the model reports about itself.
    pub const ALL: Self = Self {
        compilation_census: true,
        retire_reasons: true,
        deferral_age: true,
        peak_live_bytes: true,
    };

    /// No group is fed: the BASE [`code_cache_lifecycle_raw`] starts from before
    /// flipping the flags its pull actually feeds.
    ///
    /// Spelled as a named constant rather than left to `Default` so that
    /// assignment reads as a deliberate statement about the JIT's accounting and
    /// not as an initialisation nobody got round to — and it is still assigned
    /// FIRST, before any pull, for a reason wave 8 did not change: the
    /// `PROCESS_LIFECYCLE` instance the function starts from answers
    /// [`Self::ALL`] truthfully about itself, and nothing writes to it, so every
    /// group arrives at zero wearing a "fed" flag. Clearing them all and then
    /// setting back only what is really fed is the only order in which forgetting
    /// a line fails SAFE.
    pub const NONE: Self = Self {
        compilation_census: false,
        retire_reasons: false,
        deferral_age: false,
        peak_live_bytes: false,
    };
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
        // `reachable=` and `mean_body=` were COMPUTED and never printed.
        //
        // Round 10 wave 7: `from_raw` derived `reachable_bytes`,
        // `sweep_success_rate` and `mean_body_bytes` on every call, each was
        // asserted by this module's tests, and none of the three appeared in
        // any `Display` line — so outside `cargo test` all three were dead
        // arithmetic. That is a milder form of the same shape as the seven
        // unfed fields below (a number nobody can read), and it is the milder
        // form precisely because these three are derived ENTIRELY from pulled
        // inputs: `live_bytes` and `deferred_bytes` for the first,
        // `sweeps`/`sweeps_deferred` for the second, `installed_bytes`/
        // `installs` for the third. They were always correct and always
        // invisible, so printing them needs no new producer and states nothing
        // that was not already true.
        //
        // `reachable=` earns its place on the occupancy line rather than being
        // left to subtraction: §1.4's fail-safe rule makes `deferred_bytes` the
        // leak signal, and `live - deferred` is the figure that says how much
        // of the mapped cache is actually in service. An operator asking "is
        // the cache full or is it wedged?" is asking for these two side by
        // side.
        write!(
            f,
            "[JIT] code-cache: installed={} bodies / {} bytes  reclaimed={} bodies / {} bytes  \
             live={} bodies / {} bytes  reachable={} bytes  mean_body={:.1} bytes  ",
            r.installs,
            r.installed_bytes,
            r.reclaimed_bodies,
            r.reclaimed_bytes,
            self.live_bodies,
            self.live_bytes,
            self.reachable_bytes,
            self.mean_body_bytes,
        )?;
        // SUPPRESSION 1 of 4 — the high-water mark.
        //
        // Printed as a NAMED absence, never as `peak 0`. `peak 0` beside a
        // large `live` is the arithmetically impossible reading that
        // `r10-wrappers-process-report-has-no-reader-and-seven-unfed-fields-20260921-RETIRED-20260922.md`
        // gives as its first confirmable-by-inspection symptom, and it is worse
        // than silence because a reader who trusts it concludes the cache has
        // never been fuller than it is now — i.e. that it is not growing.
        //
        // FED as of round 10 wave 8 on the process path: `cratonvm_jit` samples
        // live bytes at each publish. The `n/a` arm is KEPT rather than deleted,
        // and not out of caution — `CodeCacheLifecycleRaw::default()` still
        // reaches it (the flag is `false` by `Default`), and so would any future
        // producer that can be configured off. A suppression arm with no
        // reachable caller would be this round's own defect class; this one has
        // two.
        if r.modelled_sources.peak_live_bytes {
            writeln!(f, "peak={}", r.peak_live_bytes)?;
        } else {
            writeln!(
                f,
                "peak=n/a (no producer for this snapshot's high-water mark, so this is \
                 NOT `0`)"
            )?;
        }
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
        // `success_rate=` is the third of the computed-but-never-printed
        // derived values, and it belongs beside `sweeps=`/`(deferred N)` rather
        // than anywhere else: it is exactly their quotient, and printing the
        // pair without it invites a reader to divide two cumulative counters in
        // their head and get the same number a wedged queue would give.
        writeln!(
            f,
            "[JIT] code-cache retirement: retired={} bodies / {} bytes  sweeps={} \
             (deferred {}) success_rate={:.4} reclaim_completion={:.4}",
            r.retirements,
            r.retired_bytes,
            r.sweeps,
            r.sweeps_deferred,
            self.sweep_success_rate,
            self.reclaim_completion,
        )?;
        if r.deferred_bodies == 0 {
            writeln!(
                f,
                "[JIT] code-cache retirement: nothing awaiting reclamation"
            )?;
        } else {
            write!(
                f,
                "[JIT] code-cache retirement: RETAINED {} bodies / {} bytes ({:.4} of live) — \
                 quiescence unproven",
                r.deferred_bodies, r.deferred_bytes, self.deferred_fraction,
            )?;
            // SUPPRESSION 2 of 4 — the deferral count and the oldest queued
            // body's age.
            //
            // This is the suppression that costs the most and is the least
            // avoidable. §1.4 names a climbing `max_deferral_sweeps` as THE
            // signature of a permanently wedged entry chain, and this branch is
            // the only place it would ever be read — so `oldest deferred 0
            // sweeps`, printed in the RETAINED branch on every process report,
            // is a positive statement that the retention is fresh at the exact
            // moment a reader is looking for the opposite. Naming the absence
            // at least sends them to the queue itself.
            //
            // `cratonvm_jit`'s queue CAN answer an age — every `DeferredOwner`
            // carries a `retired_gen` and `JIT_RETIRE_GENERATION` is the
            // current stamp — and as of round 10 wave 8 it does, through
            // `jit_oldest_retirement_age()`. **In GENERATIONS, not sweeps**, so
            // it is a THIRD arm here and not a filling-in of the first: a
            // generation advances per withdrawal and a sweep per drain, so a
            // queue holding one wedged body through a hundred unrelated
            // recompiles reports an age of ~100 having survived possibly one
            // drain. Printing that under the word "sweeps" would have answered
            // §1.4's question with a number in the wrong unit, which is worse
            // than the `n/a` it replaced because it is actionable and wrong.
            //
            // `deferrals` (the model's per-body deferral EVENT count) and
            // `max_deferral_sweeps` still have no producer on this path, which
            // is why `modelled_sources.deferral_age` stays false here and the
            // first arm stays model-only.
            if r.modelled_sources.deferral_age {
                write!(
                    f,
                    ", deferral_events={} oldest deferred {} sweeps",
                    r.deferrals, r.max_deferral_sweeps,
                )?;
            } else if let Some(gens) = r.oldest_deferral_generations {
                write!(
                    f,
                    ", oldest deferred {gens} RETIREMENT GENERATIONS (a generation is one \
                     withdrawal, not one drain — this is not a sweep count, and \
                     deferral_events/max_deferral_sweeps have no producer on this path). \
                     A figure that climbs across reports while `deferred=` never falls is \
                     §1.4's wedged entry chain"
                )?;
            } else {
                write!(
                    f,
                    ", age=n/a (the JIT retirement queue reported no age — it was empty at \
                     the moment of the call, which is NOT the same as a freshly-queued \
                     body; read `deferred={}` against an earlier report to see whether it \
                     is falling)",
                    r.deferred_bodies,
                )?;
            }
            writeln!(
                f,
                ". Retention is the fail-safe: a leak is a bug, freeing live code is a crash."
            )?;
        }
        // `CRATONVM_JIT_POISON_FREE=1` retires bodies with
        // `mprotect(PROT_NONE)` instead of `munmap`, so a use-after-free jump
        // into retired code still faults at an address that is still in the
        // process map. It leaks address space by construction, and
        // `cratonvm_jit::platform::POISONED_JIT_BYTES` is how much.
        //
        // Round 10, lane `gauges`: that counter was incremented on every
        // poisoned free and read by NOTHING in the workspace — the same
        // never-consumed-gauge shape as the failed-allocation fields this
        // report now pulls, one step further gone (written, never read, so not
        // even reachable). This is its consumer. Printed only when it is
        // non-zero, which outside the diagnostic mode it never is, so the
        // ordinary report is byte-for-byte what it was.
        let poisoned = cratonvm_jit::platform::POISONED_JIT_BYTES.load(Ordering::Relaxed);
        if poisoned != 0 {
            writeln!(
                f,
                "[JIT] code-cache retirement: CRATONVM_JIT_POISON_FREE is on — {poisoned} bytes \
                 of retired code left mapped PROT_NONE and never recycled. Expected in this \
                 mode: it trades address space for provenance on a use-after-free jump. \
                 Investigate only if the mode was not intended.",
            )?;
        } else if cratonvm_jit::platform::jit_poison_free_enabled() {
            // The ON-BUT-INERT arm. Round 10 wave 7, handed to this lane by lane
            // `nearglobals` as
            // `r10-nearglobals-poison-mode-cannot-report-that-it-poisoned-nothing-20260921-RESOLVED-20260922.md`
            // because that lane owned `jit/src/platform.rs` and not this file.
            //
            // `poisoned == 0` had SEVERAL causes and the report could not tell
            // them apart, so an operator read the silence as "no retired-code
            // bytes were poisoned" — true in every case and useful in none. The
            // expensive one is that the mode they switched on is not running:
            // `jit_poison_free_enabled()` is deliberately NOT `cfg`-gated and
            // answers `true` for the flag on every target, but only the Unix
            // non-macOS-ARM64 `platform_free` consults it. `platform.rs`' own
            // doc on that function says so in as many words — "on those hosts a
            // run with the flag set is indistinguishable from a run without it —
            // the mode silently does nothing" — and this is the reader that
            // sentence needed. The mode is switched on by someone already
            // chasing a crash, so a clean-looking report costs them the
            // experiment.
            //
            // This is also `jit_poison_free_enabled`'s FIRST caller anywhere:
            // before this it was `pub` with three in-file references (its
            // definition, the `platform_free` call and its own doc) and no
            // reader — the round's recurring defect class with the polarity
            // inverted. Not an instrument nobody reads, but a zero nobody can
            // interpret, with the predicate that interprets it sitting `pub` and
            // uncalled one crate away.
            //
            // FOUR causes are named, not the three the filing page listed. The
            // fourth was found by reading `free_executable`, which the page did
            // not: it begins `if free_if_arena(ptr) { return; }`, so a body
            // served out of a pooled arena REGION is released through
            // `JitCodeArena::release` and never reaches `platform_free` at all.
            // Only `BlockOrigin::Standalone` blocks and whole surplus regions go
            // that way. So on a Linux host with BOTH
            // `CRATONVM_JIT_CODE_ARENA=1` and `CRATONVM_JIT_POISON_FREE=1`,
            // region-served bodies are never poisoned and this counter can stay
            // at zero with the arm present and working — which would otherwise
            // read here as "the target has no poisoning arm" and send someone to
            // the wrong file.
            //
            // The second cause is also stated more precisely than the page had
            // it: the poison path is in `platform_free`, reached from
            // `ExecutableBuffer::drop`, so what has to have happened is a
            // RECLAMATION, not a retirement. `reclaimed=` on the first line of
            // this report is the exact disambiguator, so it is cited rather than
            // described.
            //
            // It deliberately does NOT assert which cause it is, because this
            // report cannot know: two of the four need figures it does print
            // (`reclaimed=`, and whether the arena is on), one needs the
            // `tracing::warn!` that `platform_free` emits on an `mprotect`
            // failure, and one needs the target triple. A line claiming "this
            // target has no poisoning arm" would be wrong on a Linux host that
            // has simply not reclaimed anything yet.
            //
            // Costs a default run one cached `OnceLock<bool>` load on a branch
            // that is then not taken — and only on the `poisoned == 0` side,
            // which with the flag off is always. (The first call also reads the
            // environment variable once, inside `get_or_init`. This is a
            // shutdown diagnostic printed once per process, so that is not a
            // cost worth a sentence except to be accurate.) The existing
            // promise that the ordinary report is byte-for-byte what it was
            // still holds: with the flag off this arm prints nothing. No new
            // counter, so nothing new to orphan.
            writeln!(
                f,
                "[JIT] code-cache retirement: CRATONVM_JIT_POISON_FREE is on and poisoned \
                 NOTHING — the mode is NOT in force, and a use-after-free jump into retired \
                 code will not be identifiable as one. Four causes, in the order they are \
                 cheapest to rule out: (1) this target has no poisoning arm — only the Unix, \
                 non-macOS-ARM64 `platform_free` consults the flag, so on Windows and \
                 macOS/ARM64 the mode is inert by construction; (2) nothing has been \
                 reclaimed yet — the poison path runs on `ExecutableBuffer::drop`, so check \
                 `reclaimed=` on the first line above, and a zero there explains this one; \
                 (3) the shared code arena is on and the retired bodies came out of pooled \
                 regions — `free_executable` short-circuits through `free_if_arena` before \
                 `platform_free`, so only standalone blocks are ever poisoned; (4) every \
                 `mprotect(PROT_NONE)` failed, in which case `platform_free` emitted a \
                 `tracing::warn!` saying so (needs RUST_LOG=warn and a subscriber).",
            )?;
        }
        // Keep-alive failures, from `cratonvm_jit::jit_code_keepalive_census`.
        //
        // Round 10, lane `gauges`: the five counters behind that accessor were
        // incremented by production paths in `jit/src/lib.rs` and read by
        // nothing in the workspace — the same never-consumed shape as the
        // failed-allocation fields above, one step further gone (no accessor
        // at all, so not even reachable). This report is their consumer,
        // because what they count is code LIFETIME: each is a case where a
        // compiled body's keep-alive did not hold, which is the precondition
        // for executing unmapped code. That belongs beside "retention is the
        // fail-safe" and nowhere else.
        //
        // Printed only when something fired. In a healthy process all five are
        // zero and the report is byte-for-byte what it was.
        let (unpinned, unrooted, rebound, retired_callees, ic_rollbacks) =
            cratonvm_jit::jit_code_keepalive_census();
        // Parenthesised: `!=` binds tighter than `|` in Rust, so the bare
        // chain would be `… | (ic_rollbacks != 0)` and would not even typecheck.
        if (unpinned | unrooted | rebound | retired_callees | ic_rollbacks) != 0 {
            writeln!(
                f,
                "[JIT] code-cache keep-alive: unpinned_entries={unpinned} \
                 unrooted_direct_callees={unrooted} rebound_direct_callees={rebound} \
                 retired_direct_callees={retired_callees} ic_retired_install_rollbacks={ic_rollbacks} \
                 — each is a baked reference whose owner could not be held; \
                 non-zero is the precondition for executing a body that was unmapped \
                 under its caller, not proof that one was.",
            )?;
        }
        // SUPPRESSION 3 of 4 — the per-reason retirement histogram.
        //
        // The pre-existing behaviour here was the SILENT kind, and it is the
        // one worth spelling out: `retire_reason_breakdown()` filters out zero
        // buckets, so with no producer it returns empty, `!reasons.is_empty()`
        // is false, and the line simply does not appear. No number is printed,
        // so nothing is arithmetically impossible — and that is precisely why it
        // is the most misleading of the four. A reader who knows this report
        // omits the line when nothing was retired reads its absence as "nothing
        // was retired", beside a `retired=` that says otherwise. The one line
        // an operator wants in order to tell cache-pressure eviction from class
        // unloading is missing in the way that looks like an answer.
        //
        // So the `else` arm fires on `retirements != 0`: an absence is only
        // worth naming when there was something to classify.
        //
        // FED as of round 10 wave 8 on the process path: every withdrawal site
        // in `cratonvm_jit` passes a `retire_reason` code, and the buckets sum
        // to `retired=`. Two of the six codes carry no site — `SHUTDOWN` has no
        // production caller at all, and `DEOPTIMIZED` comes only from
        // `JitCache::remove` — and both are simply absent from the line, which
        // is the right behaviour for a reason that did not fire. See
        // `cratonvm_jit::retire_reason` for which codes are reserved and why.
        if r.modelled_sources.retire_reasons {
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
            } else if r.retirements != 0 {
                // A fed histogram whose every bucket is zero, beside a non-zero
                // `retired=`. Without this arm the zero-filter omits the line,
                // and an omission here reads as "nothing was retired" — the
                // exact silent failure the `else` branch below exists for, come
                // back wearing a producer, which is strictly harder to find.
                //
                // Reachable for one benign reason and one real one. Benign: the
                // total and the buckets are separate relaxed loads, so a
                // snapshot taken mid-withdrawal is off by that one event. Real:
                // a withdrawal site passing a code outside
                // `cratonvm_jit::retire_reason` is counted in the total and
                // dropped from the histogram by design, because a panic on an
                // observability path that retires live code would be a leak at
                // best. Either way, say so.
                writeln!(
                    f,
                    "[JIT] code-cache retirement by reason: EMPTY beside retired={} — every \
                     bucket is zero although a producer is present. Either this snapshot \
                     caught a withdrawal between two relaxed loads (off by one, harmless), \
                     or a withdrawal site passed a reason code outside \
                     `cratonvm_jit::retire_reason`, which counts in the total and is dropped \
                     from the histogram",
                    r.retirements,
                )?;
            }
        } else if r.retirements != 0 {
            writeln!(
                f,
                "[JIT] code-cache retirement by reason: n/a for all {} retirements (no \
                 producer: the JIT's withdrawal sites do not record WHY, so supersede, \
                 invalidate and cold-sweep are indistinguishable here)",
                r.retirements,
            )?;
        }
        // SUPPRESSION 4 of 4 — the compilation census, and with it two ratios.
        //
        // `versions_per_method` is the second arithmetically impossible reading
        // of the two the known-issues page names: it is
        // `ratio(installs, methods_compiled)`, `installs` is pulled and
        // positive, `methods_compiled` has no producer, and `ratio` answers
        // `0.0` rather than `NaN` for a zero denominator. A fed counter set
        // cannot put mean versions per method below `1.0` — a method with a
        // compiled body has at least one version — so `0.0000` is not a small
        // value, it is a broken one.
        //
        // The whole line goes rather than the individual figures, because none
        // of the five survives alone: `methods=0` contradicts `installs>0`,
        // `ratio` and `versions_per_method` are derived from the missing
        // denominators, and a `max_versions_for_one_method` of 0 beside
        // installs is the same impossibility as the peak. `installs` is
        // repeated in the replacement text so the reader can see that the
        // numerator IS known and only the denominators are not.
        //
        // The flag is fed on the process path as of round 10 wave 9, so the
        // `else` arm is now reached for a DIFFERENT reason than the one it was
        // written for: not "no producer" but "the producer says its table is no
        // longer exact" (`jit_compilation_census` answered `None` because the
        // census hit its method bound). The model still reaches it too, if a
        // caller ever constructs a raw snapshot by hand. Both are covered by the
        // text, which names the bound rather than claiming an absence.
        //
        // The `methods_compiled == 0` arm is a THIRD case and not a fourth way
        // of saying `n/a`. With a fed census, zero methods means nothing has
        // been compiled at all, which is a true and useful reading — but the two
        // ratios are `ratio(_, 0)`, and [`ratio`] answers `0.0` for a zero
        // denominator rather than `NaN`, so printing them would put the literal
        // string `versions_per_method=0.0000` in front of an operator. That
        // string is the second impossible reading this whole story is about, and
        // it must not be printed even on the one snapshot where it happens to be
        // arithmetically defensible: a reader who has learned to treat it as a
        // broken counter is right to, and cannot tell the two cases apart.
        if r.modelled_sources.compilation_census && r.methods_compiled == 0 {
            writeln!(
                f,
                "[JIT] code-cache compilation: no method has a compiled body yet \
                 ({} installs observed) — the two ratios have a zero denominator \
                 and are not printed rather than printed as 0.0000",
                r.installs,
            )?;
        } else if r.modelled_sources.compilation_census {
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
        } else {
            writeln!(
                f,
                "[JIT] code-cache compilation: n/a — this snapshot has no exact \
                 per-method version table (the JIT census is bounded and stops \
                 being reported once it is full), so methods, recompilations, \
                 max-versions, recompilation_ratio and versions_per_method are \
                 not derivable ({} installs were observed, which is the \
                 numerator of both ratios)",
                r.installs,
            )?;
        }
        let failures = self.allocation_failure_breakdown();
        if failures.is_empty() {
            write!(f, "[JIT] code-cache allocation: no failed allocations")
        } else {
            // `>=` on the bytes, not `=`, and that is the producer's contract
            // rather than caution. `JitCodeAllocFailureStats::failed_bytes`
            // documents itself as "a LOWER BOUND on the memory the process
            // could not get, not a total: cap-gate refusals contribute zero
            // because they happen before codegen computes a size ... Compare it
            // against `failures` rather than treating it as a mean."
            //
            // Printed as a flat `requesting N bytes` beside a `failed=` count,
            // it invited exactly the division that doc forbids — and on a
            // cap-exhausted cache, which is the case this line exists for, every
            // refusal contributes 0 bytes, so the mean would be 0 on the run
            // where the number matters most. The `>=` and the note are cheaper
            // than a reader rediscovering it.
            write!(
                f,
                "[JIT] code-cache allocation: failed={} requesting >={} bytes (a lower bound: \
                 cap-gate refusals happen before codegen sizes a buffer and contribute 0, so \
                 do not divide) (rate {:.4}) — {}",
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

/// The process instance of the model.
///
/// As of round 10 it contributes NOTHING an operator reads, because nothing in
/// this workspace writes to it. [`code_cache_lifecycle_raw`] pulls every field
/// it has a source for and assigns over this instance's zero; the fields it has
/// no source for stay zero, and the seven that are *structurally* unreachable
/// that way are enumerated in that function's doc comment under "What this does
/// not pull". It is kept because `raw()` is the snapshot's constructor and
/// because the instance is the model the tests exercise — and for no third
/// reason since wave 6 deleted the three `record_*` process wrappers that used
/// to be described here as "available to an embedder". An embedder could not
/// reach them either; they were `pub` in a crate no refusing allocator can
/// name.
///
/// It used to be described as holding "the gauges the JIT's real accounting
/// does not carry: failed allocations, configured capacity and the arena's
/// free-space shape". It held them at zero: the `record_*` wrappers that were
/// supposed to write them had no caller and no way to get one across the crate
/// boundary. See the note above `code_cache_lifecycle_raw`.
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
/// (`vm-jit-cache-keying.md`) and deliberately left
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

/// Owners waiting in the JIT retirement queue — the real queue in
/// `cratonvm_jit`, not an instance of this module's model. One relaxed load.
///
/// The gate `prune_returned_jit_entries` tests before asking for a drain.
#[inline]
pub fn pending_retirements() -> usize {
    cratonvm_jit::jit_retirement_queue_len()
}

/// Ask the JIT retirement queue to release every retired body no thread can
/// still execute or return into.
///
/// `quiescent` reports that nothing is left queued. `reclaimed_*` are the
/// process-wide deltas across the call, so a drain another thread ran
/// concurrently is included; `sequence` is the queue's drain count.
pub fn sweep_if_quiescent() -> SweepOutcome {
    let before = cratonvm_jit::jit_code_reclamation_stats();
    let after = cratonvm_jit::reclaim_retired_jit_code();
    SweepOutcome {
        sequence: after.drains,
        quiescent: after.queued_owners == 0,
        reclaimed_bodies: after
            .reclaimed_bodies
            .saturating_sub(before.reclaimed_bodies),
        reclaimed_bytes: after.reclaimed_bytes.saturating_sub(before.reclaimed_bytes),
        deferred_bodies: after.queued_owners,
        deferred_bytes: after.queued_bytes,
        // `None`, not `0`. This used to be `0`, and the `Display` printed it as
        // "(oldest deferred 0 sweeps)" in the NOT-QUIESCENT branch on every
        // production sweep — a positive claim that the retention was fresh, in
        // the branch whose whole purpose is to surface retention that is NOT.
        //
        // `JitCodeReclamationStats` carries no SWEEP age: `queued_owners` and
        // `queued_bytes` are gauges of the queue's current size and say nothing
        // about how long anything has been in it, and the JIT queue does not
        // count drains per queued owner.
        //
        // It does know the age in its OWN unit, and as of round 10 wave 8 it
        // says so — one field down, never in this one. See
        // `CodeCacheLifecycleRaw::oldest_deferral_generations` for why
        // assigning generations here would have been the worse outcome than the
        // `None` it replaces.
        oldest_deferral_sweeps: None,
        // The age the queue really keeps: `JIT_RETIRE_GENERATION` minus the
        // smallest `retired_gen` among queued owners. `None` when the queue was
        // empty at the moment of the call, which is NOT the same statement as an
        // age of `0` (queued by the most recent withdrawal).
        //
        // Takes the retirement-queue lock, for a walk of the queue. That is
        // sound from here: the accessor takes only that lock, the lock order the
        // JIT's drain establishes is queue-before-threads, and this call holds
        // neither — see `cratonvm_jit::jit_oldest_retirement_age`. It is taken
        // AFTER `reclaim_retired_jit_code` above rather than inside it, so the
        // age describes the queue as it stands after this drain, which is the
        // queue the `deferred_*` figures beside it describe too.
        oldest_deferral_generations: cratonvm_jit::jit_oldest_retirement_age(),
    }
}

// THE THREE `record_*` PROCESS WRAPPERS THAT USED TO SIT HERE WERE DELETED.
//
// Round 10 wave 6, lane `wrappers`, closing
// `docs/known-issues/jit/r10-gauges-code-cache-push-wrappers-outlived-their-direction-20260921.md`.
//
// `record_allocation_failure(u64, usize)`, `record_capacity_bytes(u64)` and
// `record_free_space(FreeSpace)` pushed into `PROCESS_LIFECYCLE`. They had no
// caller anywhere in the workspace and could not acquire one: every site that
// can refuse a code-cache allocation, know the configured cap or describe the
// arena's free list is inside `cratonvm-jit`, and `cratonvm-vm` depends on
// `cratonvm-jit` with no edge back, so the push they modelled is not
// expressible in this crate graph. Wave 5 (lane `gauges`) inverted the
// direction for all three groups — [`code_cache_lifecycle_raw`] PULLS them,
// exactly as §5 of this module's header already records for installs and
// retirements — which left these three as an advertised entry point onto a
// direction of data flow that no longer exists.
//
// Wave 5 kept them and made the pull ADD to what they published, so that a
// push could not be silently swallowed. That was the right call for a lane
// that owned neither `vm/tests/no_test_only_public_api.rs` nor the freedom to
// move its frozen offender count. It is not the right end state: the operand
// it preserved is one no caller in any build configuration can make non-zero,
// which is the round's own defect class (an input that can never fire, reading
// identically to one that merely never fired). The pull now ASSIGNS, and the
// three model recorders these wrapped are `#[cfg(test)]` and private — see the
// banner above `CodeCacheLifecycle::record_allocation_failure` for why that
// exact shape, and for what it does to both frozen ratchets.
//
// What protects against the silent zero the wrappers were blamed for is not
// the arithmetic of the join; it is
// `the_process_report_reads_the_jits_refused_allocations` in this file's test
// module, which forces a real refusal through `platform::alloc_executable(0)`
// and asserts the report carries it, in the right reason bucket.

/// Static proof that this module's [`retire_reason`] numbering and
/// `cratonvm_jit::retire_reason`'s are the same enum.
///
/// Added in round 10 wave 8 with the per-reason withdrawal histogram, and it is
/// not optional for exactly the reason the sibling assertion below states in
/// full: [`code_cache_lifecycle_raw`] copies the JIT's histogram into this
/// module's array POSITIONALLY, so a divergence between the two numberings
/// would not fail to compile — the report would print `class-unloaded` over a
/// count of supersedes, which is the one reading
/// `docs/internal/retired/r10-report-seven-code-cache-fields-still-have-no-producer-20260921-RETIRED-20260922.md`
/// says an operator reaches for this line to get. This turns that into a build
/// error at the moment either side is edited.
const _: () = {
    assert!(retire_reason::COUNT == cratonvm_jit::retire_reason::COUNT);
    assert!(retire_reason::SUPERSEDED == cratonvm_jit::retire_reason::SUPERSEDED);
    assert!(retire_reason::INVALIDATED == cratonvm_jit::retire_reason::INVALIDATED);
    assert!(retire_reason::DEOPTIMIZED == cratonvm_jit::retire_reason::DEOPTIMIZED);
    assert!(retire_reason::CACHE_PRESSURE == cratonvm_jit::retire_reason::CACHE_PRESSURE);
    assert!(retire_reason::CLASS_UNLOADED == cratonvm_jit::retire_reason::CLASS_UNLOADED);
    assert!(retire_reason::SHUTDOWN == cratonvm_jit::retire_reason::SHUTDOWN);
};

/// Static proof that this module's [`alloc_failure`] numbering and
/// `cratonvm_jit::code_alloc_failure`'s are the same enum.
///
/// The two exist separately only because the crate graph forbids the JIT from
/// naming this module (see the note above). `code_cache_lifecycle_raw` copies
/// the JIT's per-reason histogram into this module's array POSITIONALLY, so a
/// divergence would not fail to compile on its own — it would silently
/// relabel every bucket, and `code_cache_lifecycle_report()` would print
/// "code-cache-cap-exceeded" over a count of OS refusals. This turns that into
/// a build error at the moment either side is edited.
const _: () = {
    assert!(alloc_failure::COUNT == cratonvm_jit::code_alloc_failure::COUNT);
    assert!(alloc_failure::OS_REFUSED == cratonvm_jit::code_alloc_failure::OS_REFUSED);
    assert!(alloc_failure::CAP_EXCEEDED == cratonvm_jit::code_alloc_failure::CAP_EXCEEDED);
    assert!(
        alloc_failure::NO_EXTENT_LARGE_ENOUGH
            == cratonvm_jit::code_alloc_failure::NO_EXTENT_LARGE_ENOUGH
    );
    assert!(
        alloc_failure::SIZE_ESTIMATE_OVERRUN
            == cratonvm_jit::code_alloc_failure::SIZE_ESTIMATE_OVERRUN
    );
};

/// Snapshot the process code cache's raw counters.
///
/// **Every figure the pull reaches comes from the JIT.** `PROCESS_LIFECYCLE`
/// is the starting point only because it constructs the struct; nothing in the
/// workspace writes to it, so what it contributes is zero — which for the
/// fields below is harmless, and for the seven fields the pull does NOT reach
/// is a reading in its own right. See "What this does not pull" below; it is
/// not a footnote.
///
/// * installs, withdrawals, queued and reclaimed bodies and bytes and drain
///   counts — `cratonvm_jit::jit_code_reclamation_stats`, assigned over;
/// * the configured cap — `cratonvm_jit::jit_code_cache_cap_bytes`, assigned
///   over whenever a cap is configured (the default);
/// * failed allocations, their bytes and their per-reason histogram —
///   `cratonvm_jit::jit_code_alloc_failure_stats`, assigned over.
///   Round 10: before that these three read zero on every real run, because
///   the `record_*` push wrappers that were supposed to feed them had no
///   caller and no way to acquire one across the crate boundary;
/// * the free-space shape — the shared arena's census, assigned over when the
///   arena is on. With the arena off every body is its own mapping and there
///   is no free list to describe, so the triple stays at the zero the process
///   instance was constructed with.
///
/// Round 10 wave 8 added three more pulls, each of them a `cratonvm_jit`
/// accessor written for this function:
///
/// * the published-bytes high-water mark — `peak_live_bytes` on
///   `jit_code_reclamation_stats`, CLAMPED with `.max(live)` here for the reason
///   given at the assignment;
/// * the per-reason withdrawal histogram — `withdrawn_by_reason` on the same
///   struct, copied positionally under a `const` assertion;
/// * the oldest queued body's age in retirement generations —
///   `cratonvm_jit::jit_oldest_retirement_age()`, into
///   `oldest_deferral_generations` and not into `max_deferral_sweeps`.
///
/// Round 10 wave 9 added the fourth and last of them:
///
/// * the compilation census — `cratonvm_jit::jit_compilation_census()`, an
///   `Option` whose `None` keeps the group suppressed rather than printing an
///   inexact figure. See below for what it does and does not count.
///
/// # What this does not pull — and therefore does not PRINT
///
/// Two [`CodeCacheLifecycleRaw`] fields still have no source in the process
/// report, and it is their UNIT that is missing rather than the measurement.
/// Only [`CodeCacheLifecycle::install`], [`CodeCacheLifecycle::retire`] and
/// [`CodeCacheLifecycle::sweep`] write them, no production path calls any of the
/// three on `PROCESS_LIFECYCLE`, and `cratonvm_jit`'s accounting carries no
/// equivalent to take:
///
/// * `deferrals` and `max_deferral_sweeps` — both counted in SWEEPS. The JIT
///   queue advances a generation per WITHDRAWAL and keeps no per-owner drain
///   count, so there is nothing in that unit to pull. The age it does keep is
///   pulled into its own field; see
///   [`CodeCacheLifecycleRaw::oldest_deferral_generations`].
///
/// So those two stay SUPPRESSED rather than fed. `raw.modelled_sources` is set
/// to [`ModelledFieldSources::NONE`] at the top of this function and the flags
/// with producers are set back afterwards; the `Display` fragment for the rest
/// prints a named `n/a` with the reason. They still read `0` in the struct —
/// a caller reading the fields directly gets the model's zero, as it always did —
/// but nothing puts that zero in front of an operator.
///
/// The compilation group's OLD reason for being suppressed is worth keeping,
/// because the trap in it is still live for anyone who reaches for the obvious
/// counter. `tiered::CompilationStats::nominate_to_first_body` reads like an
/// exact fit for `methods_compiled` and is not: its increment sits inside a
/// SECOND `if` in `note_time_to_tier`, so a method whose `first_nominated_ms`
/// was never stamped gets a first body and is not counted. It is a lower bound
/// that exists as the denominator of a time-to-tier average, and pouring it into
/// `methods_compiled` would have made `versions_per_method` an over-estimate of
/// unknown size — or, if no stamp ever landed, `installs / 0`, the `0.0000` this
/// whole story exists to stop printing, back again WITH a producer behind it.
/// `jit_compilation_census` is not that counter: it is a table filled at the two
/// publication sites, gated on the same `mark_published` call that bumps
/// `installed_bodies`, so `methods_compiled <= installs` holds by construction.
///
/// That ordering was the point. Two of the readings were not merely zero, they
/// were *impossible*, which is why the defect was findable by reading rather
/// than only by a run:
///
/// * `peak_live_bytes` is `0` while `live_bytes` is large. A high-water mark
///   below the current value cannot happen in a fed counter.
/// * `versions_per_method` is `ratio(installs, methods_compiled)` — a positive
///   numerator over a zero denominator, and this module's [`ratio`] helper
///   answers `0.0` for a zero denominator rather than `NaN`. A fed counter can
///   never put it below `1.0`.
///
/// and `retire_reason_breakdown()` was empty however many retirements the pull
/// reported, so `Display` omitted the "retirement by reason" line entirely —
/// the silent variant, and the worst of the three, because a missing line reads
/// as "nothing was retired" beside a `retired=` that says otherwise.
///
/// **Both of the impossible readings now have a producer behind them, and the
/// first one is why the clamp at the `peak_live_bytes` assignment exists.** A
/// fed-but-lower-bound peak can still come back below `live_bytes` under
/// concurrency, so feeding the counter did not remove that hazard — it moved it
/// from "no producer" to "a producer with a known bias", which is a fixable
/// shape. `.max(live)` fixes it. `versions_per_method` is printed as of wave 9
/// and is at or above `1.0` by construction: its denominator counts exactly the
/// publications its numerator counts, because both are gated on the same
/// `ExecutableBuffer::mark_published` call reporting that it did the accounting.
///
/// `deferrals` is the one of the seven with no `Display` consequence at all: it
/// was never printed on any path, fed or not, so there was nothing to suppress.
/// It is printed now, in the RETAINED branch, when a producer exists — which on
/// the process path it still does not.
///
/// The two remaining fields become real when `cratonvm_jit` grows a per-owner
/// drain count. Each flip is exactly one flag in [`ModelledFieldSources`] at the
/// pull that feeds it — or, where the honest producer answers in a different
/// unit, a field of its own rather than a borrowed one, which is the lesson
/// `docs/feature-designs/jit-r10-producers-proposals.md` records from the
/// deferral age.
///
/// Cheap: a few dozen relaxed loads, plus one arena census under the arena's
/// lock when `CRATONVM_JIT_CODE_ARENA` is on, plus — as of wave 8 — one walk of
/// the JIT retirement queue under its own lock, which is empty or short on any
/// healthy process and is the quantity being measured when it is not. Each
/// counter is read independently, so a snapshot taken during an install or a
/// refusal can be off by that one event; none of the derived ratios depends on
/// the reads being consistent with each other.
pub fn code_cache_lifecycle_raw() -> CodeCacheLifecycleRaw {
    let mut raw = PROCESS_LIFECYCLE.raw();
    // FIRST, and assigned rather than left to `Default`, because
    // `PROCESS_LIFECYCLE.raw()` above answers `ModelledFieldSources::ALL` —
    // truthfully, for the instance it describes, which no production path
    // writes to. Every one of those four groups therefore arrives here at zero
    // with a flag saying "fed", which is exactly the pair that produced a
    // `peak` of 0 beside a large `live` and a `versions_per_method` of 0.0
    // beside positive installs. The pull has a producer for NONE of them (see
    // "What this does not pull" above), so it says so, and `Display` prints a
    // named `n/a` in place of each.
    //
    // This line is what makes the print site in `vm-cli/src/main.rs` safe to
    // have at all. If a later change teaches `cratonvm_jit` to carry one of the
    // four, flip that ONE flag here beside the pull that feeds it — the groups
    // are independent for exactly that reason.
    raw.modelled_sources = ModelledFieldSources::NONE;
    // The compilation census is sampled FIRST, before the install counter it is
    // the denominator of, and the order is load-bearing for the same reason
    // `note_jit_published_live_bytes_peak`'s two loads are ordered.
    //
    // The two reads cannot be atomic with respect to each other, so a
    // publication landing between them skews one of them. Census first means the
    // census is the STALE one, so `methods_compiled <= installs` survives the
    // race and `versions_per_method` stays at or above `1.0`. Read the other way
    // round, a single concurrent publication can put a fresh census against a
    // stale install count and print a mean below one version per method — which
    // is the exact impossible reading this whole three-wave story is about,
    // reachable again purely from statement order.
    let census = cratonvm_jit::jit_compilation_census();
    let jit = cratonvm_jit::jit_code_reclamation_stats();
    raw.installs = jit.installed_bodies;
    raw.installed_bytes = jit.installed_bytes;
    raw.installed_code_bytes = jit.installed_code_bytes;
    raw.retirements = jit.withdrawn_bodies;
    raw.retired_bytes = jit.withdrawn_bytes;
    raw.reclaimed_bodies = jit.reclaimed_bodies;
    raw.reclaimed_bytes = jit.reclaimed_bytes;
    raw.reclaimed_code_bytes = jit.reclaimed_code_bytes;
    raw.sweeps = jit.drains;
    raw.sweeps_deferred = jit.drains_deferred;
    raw.deferred_bodies = jit.queued_owners;
    raw.deferred_bytes = jit.queued_bytes;
    // ---- the three groups that gained a producer in round 10 wave 8 --------
    //
    // Each one flips exactly one flag, beside the pull that feeds it, which is
    // what `ModelledFieldSources`' four independent flags were for.
    //
    // GROUP 1 — the high-water mark. `cratonvm_jit` now samples
    // `installed_bytes - reclaimed_bytes` at each publish (the only event that
    // can raise it) into a `fetch_max`.
    //
    // **`.max(live)` is load-bearing and must not be simplified away.** The
    // JIT's sample is a LOWER bound by construction: its two loads are
    // independent and the subtraction saturates, so a release landing between
    // them makes that sample low by up to one body — see
    // `JitCodeReclamationStats::peak_live_bytes`, which says so in its own doc.
    // If the skewed sample was the last one taken and nothing has been reclaimed
    // since, the pulled peak comes back BELOW `live_bytes` computed from the
    // same snapshot, and `peak < live` is precisely the arithmetically
    // impossible reading this whole three-wave story exists to stop printing.
    // The current live figure is itself a valid lower bound on the peak (the
    // cache was at least this full: it is this full now), and it is taken from
    // the same `jit` snapshot `from_raw` will derive `live_bytes` from, so the
    // maximum of the two is both honest and guaranteed to satisfy
    // `peak >= live` in the printed report. Feeding the counter is what makes
    // this necessary; it was not needed while the field was suppressed.
    let live_now = jit.installed_bytes.saturating_sub(jit.reclaimed_bytes);
    raw.peak_live_bytes = jit.peak_live_bytes.max(live_now);
    raw.modelled_sources.peak_live_bytes = true;
    // GROUP 2 — the per-reason withdrawal histogram. Every withdrawal site in
    // `cratonvm_jit` now passes a `retire_reason` code to
    // `retire_withdrawn_body`, which bumps the total and one bucket together, so
    // the buckets sum to `raw.retirements` above.
    //
    // Copied POSITIONALLY, through `zip` for the same reason the
    // allocation-failure histogram below is: the two array lengths are two
    // different const paths that the `const` assertion proves equal, and `zip`
    // needs no such equality to keep working. The assertion is what makes the
    // positional copy safe — without it a renumbering on either side would
    // silently print `class-unloaded` over a count of supersedes.
    for (out, pulled) in raw
        .retirements_by_reason
        .iter_mut()
        .zip(jit.withdrawn_by_reason)
    {
        *out = pulled;
    }
    raw.modelled_sources.retire_reasons = true;
    // GROUP 3 — the deferral AGE, in retirement generations, into its own field.
    //
    // `modelled_sources.deferral_age` is deliberately NOT flipped. That flag
    // gates `deferrals` (the model's per-body deferral EVENT count) and
    // `max_deferral_sweeps` (the age in SWEEPS), and the JIT has a producer for
    // neither: its queue counts withdrawals, not drains per queued owner. What
    // it can answer is the age in GENERATIONS, which is a different unit, so it
    // goes in `oldest_deferral_generations` and `Display` prints it from a
    // separate clause that names the unit. Pouring it into `max_deferral_sweeps`
    // and flipping `deferral_age` would have printed a plausible sweep count
    // that is not one, and also un-suppressed `deferrals`, which still has no
    // producer at all. See `CodeCacheLifecycleRaw::oldest_deferral_generations`.
    //
    // Takes the JIT retirement-queue lock for a walk of the queue; see
    // `cratonvm_jit::jit_oldest_retirement_age` for why that cannot invert a
    // lock order from here.
    raw.oldest_deferral_generations = cratonvm_jit::jit_oldest_retirement_age();
    // GROUP 4 — the compilation census, and the last of the four to get a
    // producer (round 10 wave 9).
    //
    // `cratonvm_jit` now keeps one publication count per method key, filled at
    // `JitCache::put` and `JitCache::put_osr` and gated on the same
    // `mark_published` call that bumps `installed_bodies`. So the three figures
    // are exact and `methods_compiled <= installs` holds by construction, which
    // is what puts `versions_per_method` at or above `1.0` — the reading whose
    // impossibility started this story.
    //
    // **`None` means "no longer exact", not "zero", and must stay suppressed.**
    // The census table is bounded; past its limit it stops tracking new methods
    // and latches. A truncated table would pin `methods_compiled` while
    // `installs` kept climbing, which drives `versions_per_method` steadily
    // upward and reads exactly like the compilation thrash the ratio exists to
    // detect — a plausible wrong number, which this module's whole rebuild says
    // is worse than an `n/a`. So the flag is flipped only on `Some`, and the
    // three fields keep the model's zero that nothing prints.
    //
    // `census` was sampled at the top of this function; see the note there for
    // why it is read before `installs` and not after.
    if let Some(census) = census {
        raw.methods_compiled = census.methods_compiled;
        raw.recompilations = census.recompilations;
        raw.max_versions_for_one_method = census.max_versions_for_one_method;
        raw.modelled_sources.compilation_census = true;
    }
    let cap = cratonvm_jit::jit_code_cache_cap_bytes();
    if cap != usize::MAX {
        raw.capacity_bytes = cap as u64;
    }
    // The refused-allocation gauges. The JIT counts these at the four sites
    // that can actually refuse — `platform::alloc_executable`'s OS-refusal
    // branch, the arena's `ExecutableBuffer::new_in`, the code-cache cap gate
    // and `try_compile`'s code-buffer-estimate discard — and this is the pull.
    //
    // ASSIGNED, not added. Wave 5 added instead, to keep a push through the
    // process `record_allocation_failure` wrapper from being swallowed on the
    // next snapshot; wave 6 deleted that wrapper (see the note above), so the
    // pushed operand is one that no caller in any build configuration can make
    // non-zero. Summing against a provably-zero tally is exactly the shape this
    // round spent itself removing — a path that cannot fire, indistinguishable
    // from one that merely has not. One source, assigned over, and the doc
    // comment above is then true rather than nearly true.
    //
    // The histogram is copied positionally; the `const` assertion above is
    // what makes that safe.
    //
    // Copied through `zip` rather than by whole-array assignment on purpose:
    // the two arrays' lengths are `alloc_failure::COUNT` and
    // `cratonvm_jit::code_alloc_failure::COUNT`, which the `const` assertion
    // proves equal but which are two different const paths. `zip` needs no
    // such equality, so the loop keeps working — copying the shorter, which
    // the assertion forbids from differing — whichever way a future edit takes
    // the two numberings before the assertion catches it.
    let alloc = cratonvm_jit::jit_code_alloc_failure_stats();
    raw.failed_allocations = alloc.failures;
    raw.failed_allocation_bytes = alloc.failed_bytes;
    for (out, pulled) in raw
        .failed_allocations_by_reason
        .iter_mut()
        .zip(alloc.by_reason)
    {
        *out = pulled;
    }
    // The free-space gauge: when the shared code arena is on, it is the only
    // allocator that keeps free space, so its census is the honest source.
    // Without the arena every body is its own mapping and there is no free
    // space to speak of, and the triple stays at the zero the process
    // instance was constructed with — which is the correct reading, not a
    // missing one. (`external_fragmentation` answers `0.0` for zero free
    // bytes, so the derived report is right in that configuration too.)
    if cratonvm_jit::platform::jit_code_arena_enabled() {
        let free = arena_free_space(&cratonvm_jit::platform::jit_code_arena_census());
        raw.free_extents = free.extents;
        raw.free_bytes = free.free_bytes;
        raw.largest_free_extent = free.largest_free_extent;
    }
    raw
}

/// The free-space shape of the shared JIT code arena, from its census.
///
/// * `free_bytes` — free-listed bytes plus bump room never handed out: both
///   can serve a future install without mapping a new region.
/// * `largest_free_extent` — the census's `largest_free_extent_bytes` (the
///   biggest free-listed block or the most bump room in one region). The free
///   lists never coalesce, so this is NOT `free_bytes`, and the ratio
///   [`external_fragmentation`] computes is real rather than a structural `0.0`.
/// * `extents` — free-listed blocks, plus one for the bump tail when any bump
///   room is left. A lower bound: several regions may each have a tail, and
///   the census reports only their sum.
fn arena_free_space(census: &cratonvm_jit::platform::JitCodeArenaCensus) -> FreeSpace {
    let free_bytes =
        (census.bytes_free_listed as u64).saturating_add(census.bytes_never_bumped as u64);
    FreeSpace {
        extents: (census.free_blocks as u64)
            .saturating_add(u64::from(census.bytes_never_bumped > 0)),
        free_bytes,
        largest_free_extent: (census.largest_free_extent_bytes as u64).min(free_bytes),
    }
}

/// The process code cache's lifecycle report, derived from
/// [`code_cache_lifecycle_raw`].
///
/// Cheap (a few dozen relaxed loads and some float division, plus one arena
/// census — a lock and a walk of the live blocks — when
/// `CRATONVM_JIT_CODE_ARENA` is on); safe to call outside a pause.
///
/// # Who calls this
///
/// `maybe_dump_shutdown_reports` in `vm-cli/src/main.rs`, under
/// `flags().jit.method_stats`, beside the `[cratonvm] implicit null-check table
/// health:` line — as of round 10 wave 7. That is the ONLY production caller,
/// and `code_cache_lifecycle_raw` is still called by nothing but this, so the
/// whole pull hangs off one print site. Deleting that line returns every figure
/// below to being test-only, which is the state waves 5 and 6 left it in and
/// which `r10-wrappers-process-report-has-no-reader-and-seven-unfed-fields-20260921-RETIRED-20260922.md`
/// was filed about; if it ever has to go, move the call rather than drop it.
///
/// **The ordering that page insisted on was honoured.** It said the report must
/// not be wired to a print site while seven of its fields had no source,
/// because two of the resulting readings were arithmetically impossible and
/// "printing a wrong number is worse than printing nothing". So the suppression
/// landed first, in the same change: [`ModelledFieldSources`] records which
/// modelled groups a snapshot actually has a producer for,
/// [`code_cache_lifecycle_raw`] assigns [`ModelledFieldSources::NONE`] because
/// the pull reaches none of them, and the four affected `Display` fragments
/// print a named `n/a` with the reason instead of a figure. Nothing this
/// function can print is now a number without a producer.
///
/// What it still cannot print is in
/// `docs/feature-designs/jit-r10-report-proposals.md`: four JIT-side
/// accessors, none of which this lane could add.
pub fn code_cache_lifecycle_report() -> CodeCacheLifecycleReport {
    CodeCacheLifecycleReport::from_raw(code_cache_lifecycle_raw())
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
            // `Some`: the model can always answer. The process sweeper answers
            // `None`, which is what round 10 wave 7 changed and what the
            // `a_process_sweep_does_not_claim_a_deferral_age` test below pins.
            assert_eq!(out.oldest_deferral_sweeps, Some(expected_age));
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

    /// The decision behind `alloc_failure::NO_EXTENT_LARGE_ENOUGH`: a bucket
    /// nothing produces is NOT a misleading zero, because the breakdown filters
    /// zeros out before it looks up a label.
    ///
    /// Round 10 wave 5 filed that reason code as an unreachable instrument and
    /// wave 6 decided to KEEP it — reserved, unproduced — specifically on this
    /// property. That makes the `.filter(|&(_, n)| n != 0)` in
    /// `allocation_failure_breakdown` load-bearing for a documented decision
    /// rather than a cosmetic tidy, so it gets a test: drop the filter and the
    /// report starts printing `no-free-extent-large-enough 0` on every run,
    /// which is exactly the round's recurring defect (`osr_compile_declined`
    /// printed at 0 on every run that has ever existed) that this code was
    /// adjudicated NOT to be.
    #[test]
    fn a_zero_bucket_is_never_labelled() {
        let lc = CodeCacheLifecycle::with_simulated_quiescence();

        // Nothing recorded at all: no labels, and in particular no zero rows.
        assert!(
            lc.report().allocation_failure_breakdown().is_empty(),
            "a cache with no failed allocations must produce NO rows, not four \
             zero rows"
        );

        // One real failure. The breakdown must carry that row ALONE — the other
        // three codes, `NO_EXTENT_LARGE_ENOUGH` among them, stay invisible.
        lc.record_allocation_failure(1 << 20, alloc_failure::CAP_EXCEEDED);
        let rows = lc.report().allocation_failure_breakdown();
        assert_eq!(
            rows,
            vec![(alloc_failure::label(alloc_failure::CAP_EXCEEDED), 1)]
        );
        assert!(
            !rows
                .iter()
                .any(|&(label, _)| label
                    == alloc_failure::label(alloc_failure::NO_EXTENT_LARGE_ENOUGH)),
            "the reserved, unproduced reason code must not surface as a zero row"
        );

        // And the code is still a real label rather than `unknown`: keeping it
        // means keeping the numbering, which is the cost the decision weighed.
        assert_eq!(
            alloc_failure::label(alloc_failure::NO_EXTENT_LARGE_ENOUGH),
            "no-free-extent-large-enough"
        );
        assert_eq!(alloc_failure::SIZE_ESTIMATE_OVERRUN, 3);
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
        // `CodeCacheLifecycleRaw::default()` leaves `modelled_sources` at its
        // own `Default`, i.e. every flag false, so this report also exercises
        // the SUPPRESSED rendering of all four groups. That is not incidental
        // to this test's subject: `recompilation_ratio` and
        // `versions_per_method` are the two ratios above whose denominators are
        // the unfed fields, and "normalizes to 0.0 rather than NaN" is exactly
        // the property that let a `0.0000` reach an operator looking like a
        // measurement. The ratios still normalize; they are no longer shown.
        assert!(
            text.contains("peak=n/a"),
            "an unsourced high-water mark must be named, not printed as 0: {text}"
        );
        assert!(
            text.contains("[JIT] code-cache compilation: n/a"),
            "an unsourced compilation census must not print methods=0 \
             versions_per_method=0.0000: {text}"
        );
    }

    /// An instance report prints the four modelled groups as NUMBERS.
    ///
    /// The other half of the suppression, and the half a careless follow-up
    /// would break: `Display` now has an `if` on `modelled_sources` in four
    /// places, and a change that inverted any of them would leave the model's
    /// own report — the only place these counters are real — printing `n/a` for
    /// figures it actually has. `CodeCacheLifecycle::raw` answers
    /// `ModelledFieldSources::ALL`, and this is what that buys.
    #[test]
    fn an_instance_report_prints_the_modelled_groups_it_does_feed() {
        let lc = CodeCacheLifecycle::with_simulated_quiescence();
        let m = method_id("a/B", "c", "()V");
        let first = lc.install(m, extent(100, 4096));
        assert_eq!(first.version, 1);
        let second = lc.install(m, extent(120, 4096));
        assert!(second.is_recompilation, "the second install is a recompile");
        // `SUPERSEDED`'s label is "superseded-by-recompilation" — the string
        // asserted below.
        lc.retire(m, extent(100, 4096), retire_reason::SUPERSEDED, None);

        let raw = lc.raw();
        assert_eq!(
            raw.modelled_sources,
            ModelledFieldSources::ALL,
            "an instance feeds every modelled group"
        );
        let text = lc.report().to_string();
        // None of the four suppression markers: the instance has a producer for
        // every group. Checked marker by marker rather than as a bare
        // `!contains("n/a")`, because two of the `Display`'s lines read
        // process-wide globals (the poison-free warning and the keep-alive
        // census) and may fire here on the strength of what another test in this
        // binary did — so a blanket substring ban would be a pin on text this
        // test does not control.
        for marker in [
            "peak=n/a",
            "age=n/a",
            "retirement by reason: n/a",
            "compilation: n/a",
        ] {
            assert!(
                !text.contains(marker),
                "an instance feeds every group, so {marker:?} must not appear: {text}"
            );
        }
        assert!(text.contains("peak="), "{text}");
        assert!(text.contains("methods=1"), "{text}");
        assert!(text.contains("recompilations=1"), "{text}");
        assert!(
            text.contains("superseded-by-recompilation=1"),
            "the per-reason histogram is fed here: {text}"
        );
    }

    /// The process sweeper must not claim a deferral age in SWEEPS, and must
    /// name the unit of the age it does claim.
    ///
    /// `sweep_if_quiescent` hard-coded `oldest_deferral_sweeps: 0`, and
    /// `SweepOutcome`'s `Display` printed it as "(oldest deferred 0 sweeps)" in
    /// the NOT-QUIESCENT branch — the one branch whose purpose is to surface a
    /// retention that is NOT fresh. Wave 7 made it `None`. Wave 8 gave the
    /// sweeper a real age from `cratonvm_jit::jit_oldest_retirement_age()`, in
    /// RETIREMENT GENERATIONS, in its own field — so the thing this test pins is
    /// now two properties rather than one, and the second is the one a careless
    /// follow-up would break:
    ///
    /// 1. `oldest_deferral_sweeps` is STILL `None`. The JIT queue counts
    ///    withdrawals, not drains per queued owner, so there is still no sweep
    ///    count to give and pouring generations into that field would be a
    ///    plausible number in the wrong unit.
    /// 2. whatever age IS printed names its unit. A bare number in this clause
    ///    reads as sweeps, because that is what the clause said for years.
    ///
    /// Asserts on the shape of the answer, not on the queue's contents: another
    /// test in this binary may have queued or drained anything at any moment, so
    /// `quiescent`, `deferred_*` and the age's VALUE are deliberately not
    /// asserted — including whether the age is `Some` at all, since a queue that
    /// happens to be empty answers `None` and that is correct.
    #[test]
    fn a_process_sweep_does_not_claim_a_deferral_age_in_sweeps() {
        let out = sweep_if_quiescent();
        assert_eq!(
            out.oldest_deferral_sweeps, None,
            "the JIT queue exposes no per-BODY sweep count, so the process \
             sweeper must answer None rather than 0 — and must not answer with \
             the generation age either, which is a different unit",
        );
        let text = out.to_string();
        assert!(
            !text.contains("oldest deferred 0 sweeps"),
            "the reading this test exists to forbid: {text}"
        );
        match out.oldest_deferral_generations {
            Some(gens) => {
                assert!(
                    text.contains(&format!("oldest deferred {gens} retirement generations")),
                    "an age in generations must say so in as many words, or it \
                     reads as the sweep count this clause used to carry: {text}"
                );
            }
            None => assert!(
                text.contains("oldest deferral age n/a"),
                "an empty queue has no age and must say so: {text}"
            ),
        }
    }

    /// The process report never prints a figure it has no producer for.
    ///
    /// The regression pin for
    /// `r10-wrappers-process-report-has-no-reader-and-seven-unfed-fields-20260921-RETIRED-20260922.md`
    /// Finding 2, and it is written against the two readings that page calls
    /// arithmetically impossible, because those are the ones an operator would
    /// act on:
    ///
    /// * a `peak` below the current `live` — a high-water mark that says the
    ///   cache has never been fuller than right now, i.e. that it is not
    ///   growing;
    /// * a `versions_per_method` below `1.0` beside a positive `installs` — a
    ///   method with a compiled body has at least one version, so any value
    ///   under 1 is a broken counter, and `0.0000` is what a zero denominator
    ///   produces through `ratio`.
    ///
    /// Asserts on absences and on the named suppressions rather than on
    /// numbers: this reads process-wide counters that every other test in the
    /// binary also moves.
    ///
    /// **Updated in wave 8, which fed three of the four groups.** The subject is
    /// unchanged — nothing may be printed without a producer — but the flag
    /// vector is no longer `NONE`, so asserting `NONE` would have pinned the
    /// absence of the fix rather than the property. It is asserted field by
    /// field instead, with the reason for each, which is also what makes the
    /// next wave's edit obvious: whichever flag it flips, it changes one line
    /// here and the `Display` arm in the same commit.
    #[test]
    fn the_process_report_suppresses_every_field_it_cannot_source() {
        let raw = code_cache_lifecycle_raw();
        assert!(
            raw.modelled_sources.peak_live_bytes,
            "wave 8 gave the high-water mark a producer \
             (`JitCodeReclamationStats::peak_live_bytes`); if this fails the pull \
             or the `fetch_max` at the publish site is gone",
        );
        assert!(
            raw.modelled_sources.retire_reasons,
            "wave 8 threaded a `retire_reason` through every withdrawal site in \
             `cratonvm_jit`",
        );
        assert!(
            raw.modelled_sources.compilation_census,
            "wave 9 gave the compilation census a producer \
             (`cratonvm_jit::jit_compilation_census`), a per-method publication \
             table filled at `JitCache::put`/`put_osr`. A `false` here means \
             either that the pull is gone or that the census answered `None` \
             because its bounded table filled up — the second is a correct \
             suppression but cannot happen in a test binary, which compiles \
             nothing like 2^17 distinct methods",
        );
        assert!(
            !raw.modelled_sources.deferral_age,
            "STILL suppressed: this flag gates `deferrals` and \
             `max_deferral_sweeps`, both in SWEEPS, and the JIT queue counts \
             withdrawals. The age it can answer is in generations and lives in \
             `oldest_deferral_generations`; flipping this flag to carry it would \
             print generations under the word `sweeps`",
        );
        // `peak >= live` — the identity whose violation is the first symptom
        // `r10-wrappers-...` names, and the one the feed made possible rather
        // than impossible: the JIT's sample is a lower bound, so
        // `code_cache_lifecycle_raw` clamps it with `.max(live)`. This is the pin
        // on that clamp.
        let report = CodeCacheLifecycleReport::from_raw(raw);
        assert!(
            report.raw.peak_live_bytes >= report.live_bytes,
            "a high-water mark below the current value is the reading this whole \
             change exists to stop printing: peak={} live={}",
            report.raw.peak_live_bytes,
            report.live_bytes,
        );

        // The second identity, and the one the census was fed to make true
        // rather than absent: `versions_per_method >= 1.0` whenever anything was
        // installed.
        //
        // Stated as inequalities, not equalities, and that is not slack. This
        // binary runs tests in parallel and every one of them that compiles
        // something moves both counters, so the census and the install count are
        // two reads of a moving process. What `code_cache_lifecycle_raw`
        // guarantees under that race is a DIRECTION — it samples the census
        // first, so the census is the stale side and can only be low — and a
        // direction is exactly what these assertions pin. The exact identity
        // `methods + recompilations == installs` is asserted where it can be:
        // `jit/tests/r10_producers_code_cache_census.rs`, on a quiet process.
        if report.raw.installs > 0 {
            assert!(
                report.raw.methods_compiled > 0,
                "installs without a single distinct method is the zero \
                 denominator that produced `versions_per_method=0.0000`: \
                 installs={} methods={}",
                report.raw.installs,
                report.raw.methods_compiled,
            );
            assert!(
                report.raw.methods_compiled <= report.raw.installs,
                "every census entry was created by a publication that bumped \
                 `installed_bodies`, and the census is sampled FIRST, so it \
                 cannot exceed the install count: installs={} methods={}",
                report.raw.installs,
                report.raw.methods_compiled,
            );
            assert!(
                report.raw.methods_compiled + report.raw.recompilations
                    <= report.raw.installs,
                "every publication is either a method's first or a \
                 recompilation; the sum may lag installs under a concurrent \
                 publish but must never lead it: installs={} methods={} \
                 recompilations={}",
                report.raw.installs,
                report.raw.methods_compiled,
                report.raw.recompilations,
            );
            assert!(
                report.versions_per_method >= 1.0,
                "a method with a compiled body has at least one version: {}",
                report.versions_per_method,
            );
            assert!(
                report.raw.max_versions_for_one_method >= 1,
                "something was published, so some method reached version 1",
            );
        }

        let text = code_cache_lifecycle_report().to_string();
        assert!(
            !text.contains("peak=n/a"),
            "the high-water mark has a producer now and must print a figure: {text}"
        );
        assert!(
            !text.contains("[JIT] code-cache compilation: n/a"),
            "the compilation census has a producer now, so the only reason for \
             `n/a` would be a truncated table, which this binary cannot cause: \
             {text}"
        );
        // The exact substrings the old rendering produced. `peak 0` was
        // unconditional; `versions_per_method=0.0000` was what `ratio` gave for
        // `installs / 0`.
        assert!(
            !text.contains("peak 0)"),
            "the first impossible reading is back: {text}"
        );
        assert!(
            !text.contains("versions_per_method=0.0000"),
            "the second impossible reading is back: {text}"
        );
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

    /// The process report reads the JIT's real accounting: publishing a body is
    /// an install, flushing it is a withdrawal, and the report's identities hold.
    /// This used to drive a model queue nothing in production fed, so the
    /// report it checked always read zero. Deltas and identities only — other
    /// tests share the process counters.
    #[test]
    fn the_process_report_reads_the_real_jit_accounting() {
        let before = code_cache_lifecycle_raw();

        let cache = cratonvm_jit::JitCache::new();
        let mut buf = cratonvm_jit::ExecutableBuffer::new(64).expect("alloc executable");
        buf.emit(&[0xC3]);
        cache.put(
            "lifecycle/Probe".into(),
            "run".into(),
            "()V".into(),
            cratonvm_types::ClassId::new(7),
            cratonvm_jit::CompiledMethod::new(buf),
        );
        let installed = code_cache_lifecycle_raw();
        assert!(
            installed.installs >= before.installs + 1,
            "a publication must count as an install"
        );
        assert!(installed.installed_bytes >= before.installed_bytes + 64);

        assert_eq!(cache.clear_all(), 1);
        let withdrawn = code_cache_lifecycle_raw();
        assert!(
            withdrawn.retirements >= before.retirements + 1,
            "a flushed body must count as a withdrawal"
        );

        let swept = sweep_if_quiescent();
        assert_eq!(
            swept.quiescent,
            swept.deferred_bodies == 0,
            "the sweep reports the real queue it drained"
        );

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
    }

    /// The refused-allocation gauges read the JIT's real refusals.
    ///
    /// Round 10 (lane `gauges`,
    /// `r10-gcs2-code-cache-alloc-failure-gauges-never-fed-20260921.md`).
    /// Before that join, `failed_allocations` could not leave zero on any real
    /// run: the only way in was `record_allocation_failure`, and no crate could
    /// call it — every site that refuses a code-cache allocation is inside
    /// `cratonvm-jit`, which `cratonvm-vm` depends on and not the reverse. So
    /// the report printed "no failed allocations" whether or not there had been
    /// any, which is the failure mode this test exists to stop coming back.
    ///
    /// Deltas only: other tests in this binary share the process counters, and
    /// a compile elsewhere may add a `SIZE_ESTIMATE_OVERRUN` at any moment.
    ///
    /// The forced refusal is a zero-byte executable allocation. Every backing
    /// allocator rejects it (`mmap(len=0)` is `EINVAL`, `VirtualAlloc(0)`
    /// returns null) and `jit/src/platform.rs` has its own test pinning that,
    /// so it is a refusal this test can provoke without exhausting the machine
    /// — which is the only other way to reach `OS_REFUSED`.
    #[test]
    fn the_process_report_reads_the_jits_refused_allocations() {
        let before = code_cache_lifecycle_raw();
        let jit_before = cratonvm_jit::jit_code_alloc_failure_stats();

        assert!(
            cratonvm_jit::platform::alloc_executable(0).is_none(),
            "a zero-byte code allocation must be refused"
        );

        let jit_after = cratonvm_jit::jit_code_alloc_failure_stats();
        assert!(
            jit_after.failures >= jit_before.failures + 1,
            "the JIT must have counted its own refusal"
        );
        assert!(
            jit_after.by_reason[alloc_failure::OS_REFUSED]
                >= jit_before.by_reason[alloc_failure::OS_REFUSED] + 1,
            "and counted it under OS_REFUSED"
        );

        let after = code_cache_lifecycle_raw();
        assert!(
            after.failed_allocations >= before.failed_allocations + 1,
            "the report must carry the JIT's refusal, not a model's zero",
        );
        assert!(
            after.failed_allocations_by_reason[alloc_failure::OS_REFUSED]
                >= before.failed_allocations_by_reason[alloc_failure::OS_REFUSED] + 1,
            "and must carry it in the bucket the JIT named — the histogram is \
             copied positionally, so a mislabelled bucket is the drift the \
             `const _` assertion above guards against",
        );
        assert!(
            !code_cache_lifecycle_report()
                .to_string()
                .contains("no failed allocations"),
            "with a refusal recorded, the report must not claim there were none",
        );
    }

    /// r9w3 (runtime2 request 4): the arena census feeds the free-space gauge
    /// with its real largest extent, so a shattered free list reads as
    /// fragmented instead of the structural `0.0` of "largest = total".
    #[test]
    fn r9w3_arena_census_feeds_a_real_largest_free_extent() {
        let census = cratonvm_jit::platform::JitCodeArenaCensus {
            bytes_free_listed: 8 * 4096,
            bytes_never_bumped: 4 * 4096,
            free_blocks: 8,
            largest_free_extent_bytes: 4 * 4096,
            ..Default::default()
        };
        let free = arena_free_space(&census);
        assert_eq!(free.free_bytes, 12 * 4096);
        assert_eq!(free.largest_free_extent, 4 * 4096);
        assert_eq!(free.extents, 9, "eight free blocks plus one bump tail");
        let frag = external_fragmentation(free);
        assert!((frag - (1.0 - 4.0 / 12.0)).abs() < 1e-9, "got {frag}");

        // Nothing free: no extents, and a nonsensical largest clamps to zero.
        let empty = arena_free_space(&cratonvm_jit::platform::JitCodeArenaCensus {
            largest_free_extent_bytes: 4096,
            ..Default::default()
        });
        assert_eq!(empty, FreeSpace::default());
        assert_eq!(external_fragmentation(empty), 0.0);
    }
}
