// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! GPU critical-section tokens: **owned** lifetime, **bounded** collector
//! waiting, and the counters that make a GPU-induced GC stall attributable
//! to a named holder.
//!
//! # Why this module exists
//!
//! The pre-existing coordination between an in-flight kernel and the
//! collector is a single process-wide counter
//! (`cratonvm_gc::vm_heap::GPU_CRITICAL_COUNT`) plus two spin loops
//! (`cratonvm_gc::vm_heap::wait_for_gpu_critical_drain`,
//! `cratonvm_gc::heap::Heap::wait_for_gpu_critical`). Both loops are
//! documented as never terminating while the counter is non-zero:
//!
//! > *"After `GPU_CRITICAL_DEADLINE_SECS` seconds we log a single
//! > `tracing::warn!` and keep waiting — we NEVER force a collection while
//! > a token is alive."*
//!
//! That is a correct **safety** rule and an absent **liveness** rule. A
//! counter that is never decremented — a cancelled kernel whose
//! `FinalizeState` nobody drains, a device fault on a submission nobody
//! polls, a VM torn down between dispatch and finalize — converts into a
//! collector that spins forever, on every thread, for the rest of the
//! process. Nothing in the counter says *who* failed to decrement it, so
//! the failure presents as an unexplained hang rather than as a leak.
//!
//! This module keeps the safety rule and adds the missing three:
//!
//! 1. **The collector's wait is bounded.** [`Registry::wait_for_drain`]
//!    takes a budget and returns a [`WaitOutcome`]. It never blocks past
//!    the budget and it never force-releases a token to make the wait
//!    succeed.
//! 2. **The token's lifetime is bounded.** Every token carries a *lease*.
//!    A token held past its lease is **revoked** — its holder is named in
//!    the log, its writeback is poisoned via [`CriticalToken::is_revoked`],
//!    and the outstanding count drops. A leak therefore heals, loudly,
//!    instead of wedging the collector.
//! 3. **Every token has an identity.** [`OwnerId`] records the VM, the
//!    thread, the submission handle and the acquisition site, so a stall
//!    names its holder.
//!
//! # What expiry does and does not authorize
//!
//! [`WaitOutcome::TimedOut`] is **not** permission to relocate. It is the
//! collector's cue to take the fail-safe it already has: a *non-moving*
//! cycle, with [`Registry::outstanding_keepalive_addrs`] spliced in as
//! additional roots so nothing a still-in-flight submission will write
//! back is reclaimed. That is the same shape as the JNI critical-pin
//! splice in `Heap::collect_garbage`, and the same shape as the
//! conservative-JIT-root fallback: an obligation that cannot be discharged
//! forces the weaker collection, it never weakens the obligation. See
//! `docs/gpu/critical-sections.md` §"The wait bound".
//!
//! `TimedOut` carries [`WaitOutcome::relocation_forbidden`] so the
//! collector cannot accidentally read a timeout as "proceed": the flag is
//! `true` whenever any outstanding token declared
//! [`Relocation::Forbidden`], and the keep-alive address list is non-empty
//! whenever any outstanding token declared roots.
//!
//! # Relationship to the two "pin" vocabularies
//!
//! `docs/threading/objectref-concurrency-contract.md` §4.3 records that
//! the workspace has two incompatible meanings for "pin". The addresses a
//! token declares here are **keep-alive**, in the `gc/src/pinned.rs` sense
//! — they must not be *freed*, and they are *remapped* after a moving
//! cycle via [`Registry::remap_keepalive`]. They are deliberately **not**
//! a no-relocation request. A token that genuinely does hand the device a
//! heap address for longer than one safepoint must declare
//! [`Relocation::Forbidden`], and the collector must then refuse to
//! relocate rather than merely keep the object alive.
//!
//! ## Why keep-alive is enough today, and what it rests on
//!
//! AUDIT 2026-09-02. This paragraph used to say the device never holds a
//! JVM heap address at all, because `gpu_marshal::host_view_*` copies the
//! array body into a fresh host `Vec` before upload, exactly as JNI hands
//! native code a detached copy. That is true of the STAGED path and false
//! of the one that actually runs: `gpu_marshal`'s `direct_xfer!` arm
//! takes `heap.array_data_ptr(obj)`, builds a slice over the arena in
//! place, and hands it to `cuMemcpyHtoDAsync`. The device reads the JVM
//! heap directly, which is the entire point of the zero-copy path — it is
//! what removes an array-sized host memcpy per direction.
//!
//! Keep-alive is still sufficient, for a reason neither module stated:
//! `DeviceBufferInner::from_host` host-blocks on
//! `cuStreamSynchronize(copy_h2d)` before returning, so the DMA has
//! retired while the caller's `SafepointToken` is still held. The
//! collector cannot run, so the arena cannot move, so there is no window.
//!
//! **That is a load-bearing invariant, not an incidental property.** Any
//! change that lets the upload outlive the marshalling call — moving the
//! marshaller to `from_host_async`, say, which is otherwise a good idea —
//! breaks the argument silently: nothing in the type system connects the
//! token's lifetime to the copy's. Such a change must either keep the
//! staged copy for the zero-copy arm, or hold a token declaring
//! [`Relocation::Forbidden`] until the upload event has fired.
//!
//! `to_host`'s D→H direction has the same shape and the same guarantee,
//! for the same reason.
//!
//! # Registry instances vs. the process-global one
//!
//! Unlike `gc::gc_metrics`, this module does **not** swap its storage for
//! a `thread_local!` under `cfg(test)`: a registry whose entire purpose is
//! cross-thread ownership cannot be thread-local without making the
//! cross-thread tests vacuous. Instead [`Registry`] is a plain value that
//! tests construct directly, and [`global`] is the one process-wide
//! instance production uses.

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Tunables
// ---------------------------------------------------------------------------

/// Default upper bound on how long a collector will wait for GPU critical
/// sections to drain, in milliseconds.
///
/// Chosen to be *shorter than a pause budget people notice*, not longer
/// than a kernel: the point of the bound is that the collector stops
/// waiting, not that the kernel finishes. A kernel that legitimately runs
/// longer than this simply causes the cycle to take the non-moving
/// fail-safe, which is always correct and merely less effective. Compare
/// `cratonvm_gc::safepoint::GPU_CRITICAL_DEADLINE_SECS` (5 s), which is a
/// *logging* threshold on an unbounded wait and not a bound at all.
///
/// Override with `CRATONVM_GPU_CRITICAL_WAIT_MS`.
pub const DEFAULT_COLLECTOR_WAIT_MS: u64 = 50;

/// Default token lease, in seconds — the hard upper bound on how long one
/// token may stay outstanding before it is revoked and reaped.
///
/// Deliberately much longer than [`DEFAULT_COLLECTOR_WAIT_MS`]: the wait
/// budget bounds *the collector*, while the lease bounds *the leak*. A
/// legitimate long-running kernel must be able to outlive many collector
/// waits without being revoked; only a submission that has plainly been
/// abandoned should be.
///
/// Override with `CRATONVM_GPU_CRITICAL_LEASE_MS`.
pub const DEFAULT_TOKEN_LEASE_SECS: u64 = 30;

/// How many reaped/shutdown token records the registry keeps for the
/// report, so a leak can still be named after the fact.
const REAP_LOG_CAPACITY: usize = 16;

/// How many live holders [`WaitOutcome::TimedOut`] names. The payload is
/// capped so a pathological workload cannot make the timeout path
/// allocate proportionally to the number of in-flight submissions.
const MAX_NAMED_HOLDERS: usize = 8;

/// Sentinel for "this tunable has not been resolved from the environment
/// yet". `u64::MAX` is not a plausible millisecond value.
const UNRESOLVED: u64 = u64::MAX;

static COLLECTOR_WAIT_MS: AtomicU64 = AtomicU64::new(UNRESOLVED);
static TOKEN_LEASE_MS: AtomicU64 = AtomicU64::new(UNRESOLVED);

fn resolve_ms(cell: &AtomicU64, var: &str, default_ms: u64) -> u64 {
    let cached = cell.load(Ordering::Relaxed);
    if cached != UNRESOLVED {
        return cached;
    }
    let resolved = std::env::var(var)
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(default_ms);
    cell.store(resolved, Ordering::Relaxed);
    resolved
}

/// The collector's wait budget. Resolved once from
/// `CRATONVM_GPU_CRITICAL_WAIT_MS`, defaulting to
/// [`DEFAULT_COLLECTOR_WAIT_MS`].
pub fn collector_wait_budget() -> Duration {
    Duration::from_millis(resolve_ms(
        &COLLECTOR_WAIT_MS,
        "CRATONVM_GPU_CRITICAL_WAIT_MS",
        DEFAULT_COLLECTOR_WAIT_MS,
    ))
}

/// The default token lease. Resolved once from
/// `CRATONVM_GPU_CRITICAL_LEASE_MS`, defaulting to
/// [`DEFAULT_TOKEN_LEASE_SECS`].
pub fn default_token_lease() -> Duration {
    Duration::from_millis(resolve_ms(
        &TOKEN_LEASE_MS,
        "CRATONVM_GPU_CRITICAL_LEASE_MS",
        DEFAULT_TOKEN_LEASE_SECS * 1_000,
    ))
}

/// Set the collector wait budget programmatically.
///
/// Exists for the same reason `gc_metrics::set_hot_path_counters_enabled`
/// does: the value latches on first read, so a test cannot arm it by
/// setting the environment variable after startup. Production never calls
/// this.
pub fn set_collector_wait_budget(d: Duration) {
    COLLECTOR_WAIT_MS.store(
        d.as_millis().min(u64::MAX as u128) as u64,
        Ordering::Relaxed,
    );
}

/// Set the default token lease programmatically. Tests only — see
/// [`set_collector_wait_budget`].
pub fn set_default_token_lease(d: Duration) {
    TOKEN_LEASE_MS.store(
        d.as_millis().min(u64::MAX as u128) as u64,
        Ordering::Relaxed,
    );
}

// ---------------------------------------------------------------------------
// Ownership identity
// ---------------------------------------------------------------------------

static NEXT_THREAD_SEQ: AtomicU64 = AtomicU64::new(1);

thread_local! {
    static THREAD_SEQ: u64 = NEXT_THREAD_SEQ.fetch_add(1, Ordering::Relaxed);
}

/// A stable, small integer identifying the calling thread.
///
/// `std::thread::ThreadId` has no stable numeric projection on the MSRV,
/// and `Debug`-formatting it produces a string that is not comparable
/// across runs. A registry-local sequence number is both cheap to store
/// in a record and readable in a log line.
pub fn current_thread_seq() -> u64 {
    THREAD_SEQ.with(|t| *t)
}

/// Who holds a token.
///
/// Every field is answerable at the acquisition site and none requires a
/// lookup later, which is the property that makes a leak *nameable*: the
/// record is complete the moment the token exists, so reaping it years
/// later (in wall-clock terms) still identifies the submission that
/// abandoned it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OwnerId {
    /// Identifies the `SharedVm` instance. `0` when the caller has none
    /// (a pre-VM probe, or a unit test).
    ///
    /// This is the field that closes the VM-shutdown leak: a torn-down VM
    /// can reap exactly its own tokens via [`Registry::shutdown_vm`]
    /// without touching a concurrently-live second VM's.
    pub vm: u64,
    /// [`current_thread_seq`] of the acquiring thread.
    pub thread: u64,
    /// `std::thread::current().name()`, when the thread has one.
    pub thread_name: Option<String>,
    /// The submission handle this token brackets, when there is one.
    /// `None` for a token taken around marshalling before a handle exists.
    pub submission: Option<u64>,
    /// Static description of the acquisition site, e.g.
    /// `"dispatch_method_from_native"`. `&'static str` so recording it
    /// costs a pointer, not a formatting call, on the acquire path.
    pub site: &'static str,
}

impl OwnerId {
    /// Build an identity for the calling thread.
    pub fn current(vm: u64, submission: Option<u64>, site: &'static str) -> Self {
        Self {
            vm,
            thread: current_thread_seq(),
            thread_name: std::thread::current().name().map(str::to_owned),
            submission,
            site,
        }
    }
}

impl std::fmt::Display for OwnerId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "vm={} thread={}", self.vm, self.thread)?;
        if let Some(name) = &self.thread_name {
            write!(f, "({name})")?;
        }
        match self.submission {
            Some(h) => write!(f, " submission={h}")?,
            None => write!(f, " submission=none")?,
        }
        write!(f, " site={}", self.site)
    }
}

// ---------------------------------------------------------------------------
// Relocation policy
// ---------------------------------------------------------------------------

/// What a token's declared addresses require of the collector.
///
/// The two variants are exactly the two "pin" vocabularies
/// `docs/threading/objectref-concurrency-contract.md` §4.3 says the
/// workspace conflates. Making the caller pick one is the point: a token
/// that hands a device a JVM heap address and declares
/// [`Relocation::KeepAliveOnly`] is a correctness bug, and the enum is
/// where that decision is written down.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Relocation {
    /// The declared addresses must not be **freed**, and must be
    /// **remapped** if the collector moves them
    /// ([`Registry::remap_keepalive`]). Relocation is safe.
    ///
    /// This is what today's marshalling path needs: `host_view_*` copies
    /// the array body into a host `Vec` before upload, so the device
    /// holds a device pointer into device memory, never a heap address.
    KeepAliveOnly,
    /// The declared addresses must not be freed **and must not move**,
    /// because something outside the collector's rewrite protocol is
    /// holding them — a zero-copy / unified-memory mapping, a pinned host
    /// staging buffer aliased to the heap, or any future path that hands
    /// a device a raw heap pointer.
    ///
    /// A collector that observes this on any outstanding token must not
    /// run a moving cycle, regardless of how the wait ended.
    Forbidden,
}

impl Relocation {
    /// Label used in reports and log lines.
    pub fn label(self) -> &'static str {
        match self {
            Relocation::KeepAliveOnly => "keep-alive-only",
            Relocation::Forbidden => "no-relocation",
        }
    }
}

// ---------------------------------------------------------------------------
// Release causes
// ---------------------------------------------------------------------------

/// How a token left the registry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReleaseCause {
    /// [`CriticalToken::release`] was called explicitly.
    Released,
    /// The token's `Drop` ran — normal scope exit, an early `return` on an
    /// error path, or unwinding out of a panic.
    Dropped,
    /// The lease expired and [`Registry::reap_expired`] revoked it.
    Reaped,
    /// [`Registry::shutdown_vm`] revoked it because its VM went away.
    VmShutdown,
}

impl ReleaseCause {
    /// Label used in reports and log lines.
    pub fn label(self) -> &'static str {
        match self {
            ReleaseCause::Released => "released",
            ReleaseCause::Dropped => "dropped",
            ReleaseCause::Reaped => "reaped-lease-expired",
            ReleaseCause::VmShutdown => "reaped-vm-shutdown",
        }
    }

    /// Did this cause represent the *holder* giving the token up, rather
    /// than the registry taking it away? Reaping is a repaired leak, not a
    /// normal release, and the two must never be summed together.
    pub fn is_voluntary(self) -> bool {
        matches!(self, ReleaseCause::Released | ReleaseCause::Dropped)
    }
}

// ---------------------------------------------------------------------------
// Token facts
// ---------------------------------------------------------------------------

/// A snapshot of one token, live or reaped. This is what a report or a
/// timeout payload names.
#[derive(Clone, Debug)]
pub struct TokenFacts {
    /// Registry-assigned identifier, monotonic per [`Registry`].
    pub id: u64,
    /// Who took the token.
    pub owner: OwnerId,
    /// How long it had been held when this snapshot was taken.
    pub held: Duration,
    /// The lease it was granted.
    pub lease: Duration,
    /// What its declared addresses require of the collector.
    pub relocation: Relocation,
    /// How many keep-alive addresses it declared.
    pub keepalive_addrs: usize,
    /// `None` while the token is still live.
    pub cause: Option<ReleaseCause>,
}

impl std::fmt::Display for TokenFacts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "token#{id} {owner} held={held:.3}ms lease={lease:.3}ms {reloc} roots={roots}",
            id = self.id,
            owner = self.owner,
            held = self.held.as_secs_f64() * 1.0e3,
            lease = self.lease.as_secs_f64() * 1.0e3,
            reloc = self.relocation.label(),
            roots = self.keepalive_addrs,
        )?;
        match self.cause {
            Some(c) => write!(f, " {}", c.label()),
            None => write!(f, " LIVE"),
        }
    }
}

// ---------------------------------------------------------------------------
// The wait outcome
// ---------------------------------------------------------------------------

/// The result of a bounded wait for GPU critical sections to drain.
///
/// This type is the whole point of the module: the pre-existing
/// `wait_for_gpu_critical_drain()` returns `()`, so a collector literally
/// cannot express "I waited and it did not drain". Returning an outcome
/// forces the caller to have a documented behaviour for expiry.
#[derive(Clone, Debug)]
pub enum WaitOutcome {
    /// No token was outstanding when the wait finished. The collector may
    /// proceed with whatever cycle it had planned — this outcome imposes
    /// no constraint of its own.
    Drained {
        /// How long the wait took. Zero when nothing was outstanding on
        /// entry (the overwhelmingly common case).
        waited: Duration,
    },
    /// The budget expired with tokens still outstanding.
    ///
    /// **The collector must degrade, not proceed.** The documented
    /// behaviour is: run a *non-moving* cycle, splice
    /// [`WaitOutcome::keepalive_addrs`] in as additional roots, and record
    /// the fallback via [`Registry::record_forced_non_moving_collection`]
    /// so the cycle is attributable. Relocating on this outcome is a
    /// correctness bug whenever `relocation_forbidden` is set, and is a
    /// stale-`ObjectRef` bug in the writeback path even when it is not.
    TimedOut {
        /// How long the wait took (≥ the budget).
        waited: Duration,
        /// Tokens still outstanding at expiry.
        outstanding: u32,
        /// `true` when at least one outstanding token declared
        /// [`Relocation::Forbidden`].
        relocation_forbidden: bool,
        /// Deduplicated keep-alive addresses declared by the outstanding
        /// tokens, for the collector to splice in as extra roots.
        keepalive_addrs: Vec<usize>,
        /// How long the oldest outstanding token had been held.
        longest_held: Duration,
        /// Up to `MAX_NAMED_HOLDERS` holders, oldest first. A stall
        /// names its holder here rather than in a post-mortem.
        holders: Vec<TokenFacts>,
        /// `true` when more holders existed than `holders` could carry.
        holders_truncated: bool,
    },
}

impl WaitOutcome {
    /// Did the wait drain?
    pub fn drained(&self) -> bool {
        matches!(self, WaitOutcome::Drained { .. })
    }

    /// May the collector run a *moving* cycle as far as GPU coordination
    /// is concerned?
    ///
    /// `true` only for [`WaitOutcome::Drained`]. This deliberately answers
    /// `false` for **every** timeout, not merely for
    /// `relocation_forbidden` ones: an outstanding `KeepAliveOnly` token
    /// still has a `MarshalWriteback` holding raw `ObjectRef`s that are
    /// in no rewritable root family, so moving them out from under an
    /// in-flight submission corrupts the writeback.
    pub fn may_relocate(&self) -> bool {
        self.drained()
    }

    /// How long the wait took, either way.
    pub fn waited(&self) -> Duration {
        match self {
            WaitOutcome::Drained { waited } => *waited,
            WaitOutcome::TimedOut { waited, .. } => *waited,
        }
    }

    /// Keep-alive addresses the collector must treat as additional roots.
    /// Empty for [`WaitOutcome::Drained`].
    pub fn keepalive_addrs(&self) -> &[usize] {
        match self {
            WaitOutcome::Drained { .. } => &[],
            WaitOutcome::TimedOut {
                keepalive_addrs, ..
            } => keepalive_addrs,
        }
    }
}

impl std::fmt::Display for WaitOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WaitOutcome::Drained { waited } => write!(
                f,
                "[GPU] critical wait: DRAINED after {:.3}ms",
                waited.as_secs_f64() * 1.0e3,
            ),
            WaitOutcome::TimedOut {
                waited,
                outstanding,
                relocation_forbidden,
                keepalive_addrs,
                longest_held,
                holders,
                holders_truncated,
            } => {
                write!(
                    f,
                    "[GPU] critical wait: TIMED OUT after {:.3}ms — {outstanding} token(s) \
                     outstanding, longest_held={:.3}ms, {} keep-alive root(s), \
                     relocation_forbidden={relocation_forbidden}; \
                     collector must take the NON-MOVING fail-safe",
                    waited.as_secs_f64() * 1.0e3,
                    longest_held.as_secs_f64() * 1.0e3,
                    keepalive_addrs.len(),
                )?;
                for h in holders {
                    write!(f, "\n[GPU] critical wait: holder {h}")?;
                }
                if *holders_truncated {
                    write!(f, "\n[GPU] critical wait: … more holders not shown")?;
                }
                Ok(())
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The token
// ---------------------------------------------------------------------------

/// An owned GPU critical-section token.
///
/// # How this differs from `cratonvm_gc::safepoint::SafepointToken`
///
/// * It is **`Send`**. `SafepointToken` is deliberately `!Send`, which is
///   why `vm::runtime::offload` had to hand-roll a parallel `GcCriticalGuard`
///   to move the critical section onto the finalize thread — two mechanisms
///   over one counter, only one of which has an identity. A token that can
///   legally cross threads does not need a shadow implementation.
/// * It has an **identity** ([`CriticalToken::owner`]).
/// * It has a **lease**, so abandoning it is bounded rather than permanent.
/// * It can be **revoked** by the registry ([`CriticalToken::is_revoked`]),
///   which is what makes reaping safe: the holder learns it lost the token
///   and suppresses its writeback instead of writing through a possibly
///   stale reference.
///
/// Release is by `Drop`, so every exit path — success, `?`, an early
/// `return` on a device error, a cancel, a panic unwinding through the
/// frame — releases exactly once.
#[must_use = "dropping a CriticalToken immediately ends the GPU critical section"]
pub struct CriticalToken {
    id: u64,
    registry: Arc<Registry>,
    revoked: Arc<AtomicBool>,
    /// Set by [`CriticalToken::release`] so the subsequent `Drop` is inert.
    /// A token can therefore never decrement twice even though both the
    /// explicit and the implicit path exist.
    released: bool,
}

impl CriticalToken {
    /// Registry-assigned identifier.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Who this registry believes holds the token.
    pub fn owner(&self) -> Option<OwnerId> {
        self.registry.facts(self.id).map(|f| f.owner)
    }

    /// Has the registry revoked this token (lease expiry or VM shutdown)?
    ///
    /// **A revoked token no longer bounds the collector.** The holder must
    /// treat this as "my inputs may have been collected or moved" and
    /// suppress any heap writeback rather than write through references
    /// captured before the revocation. Checking this immediately before a
    /// writeback is the mechanism that makes reaping a *safe* repair
    /// instead of a silent corruption.
    pub fn is_revoked(&self) -> bool {
        self.revoked.load(Ordering::Acquire)
    }

    /// Release the token explicitly. Equivalent to dropping it; provided
    /// so a call site can be explicit about *where* the critical section
    /// ends without relying on a `drop(token)` line that a later edit can
    /// silently move.
    pub fn release(mut self) {
        self.released = true;
        self.registry.release_inner(self.id, ReleaseCause::Released);
    }
}

impl Drop for CriticalToken {
    fn drop(&mut self) {
        if !self.released {
            self.registry.release_inner(self.id, ReleaseCause::Dropped);
        }
    }
}

impl std::fmt::Debug for CriticalToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CriticalToken")
            .field("id", &self.id)
            .field("revoked", &self.is_revoked())
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Counters
// ---------------------------------------------------------------------------

/// Raw counter values, before normalization. Shape mirrors
/// `cratonvm_gc::gc_metrics::GcMetricsRaw` so the two reports read alike.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CriticalMetricsRaw {
    /// Tokens acquired since startup.
    pub tokens_acquired: u64,
    /// Tokens released voluntarily (explicit release or `Drop`).
    pub tokens_released: u64,
    /// Tokens revoked because their lease expired. **Every one of these is
    /// a repaired leak** — a non-zero value means some exit path is not
    /// releasing.
    pub tokens_reaped: u64,
    /// Tokens revoked because their VM was torn down while they were
    /// outstanding.
    pub tokens_reaped_at_shutdown: u64,
    /// Releases that named an id the registry no longer had. Benign
    /// (idempotent), but a non-zero value means a token was released twice
    /// or after a reap.
    pub double_release_suppressed: u64,
    /// Tokens outstanding right now (**gauge**).
    pub tokens_outstanding: u64,
    /// Longest any single token has been held, in nanoseconds (**max**,
    /// over both released and reaped tokens).
    pub longest_held_nanos: u64,
    /// Bounded waits entered.
    pub waits_entered: u64,
    /// Waits that drained inside the budget.
    pub waits_drained: u64,
    /// Waits that hit the budget with tokens still outstanding.
    pub waits_timed_out: u64,
    /// Total nanoseconds spent inside bounded waits.
    pub wait_nanos: u64,
    /// Collections forced onto the non-moving path because a GPU wait
    /// expired. This is the GC-latency figure the report attributes to
    /// GPU waiting.
    pub forced_non_moving_collections: u64,
}

struct Counters {
    tokens_acquired: AtomicU64,
    tokens_released: AtomicU64,
    tokens_reaped: AtomicU64,
    tokens_reaped_at_shutdown: AtomicU64,
    double_release_suppressed: AtomicU64,
    longest_held_nanos: AtomicU64,
    waits_entered: AtomicU64,
    waits_drained: AtomicU64,
    waits_timed_out: AtomicU64,
    wait_nanos: AtomicU64,
    forced_non_moving_collections: AtomicU64,
}

impl Counters {
    const fn new() -> Self {
        Self {
            tokens_acquired: AtomicU64::new(0),
            tokens_released: AtomicU64::new(0),
            tokens_reaped: AtomicU64::new(0),
            tokens_reaped_at_shutdown: AtomicU64::new(0),
            double_release_suppressed: AtomicU64::new(0),
            longest_held_nanos: AtomicU64::new(0),
            waits_entered: AtomicU64::new(0),
            waits_drained: AtomicU64::new(0),
            waits_timed_out: AtomicU64::new(0),
            wait_nanos: AtomicU64::new(0),
            forced_non_moving_collections: AtomicU64::new(0),
        }
    }

    fn bump_max(&self, held: Duration) {
        let n = held.as_nanos().min(u64::MAX as u128) as u64;
        // Plain CAS loop rather than `fetch_max`, which is available but
        // reads oddly next to the rest of the file; contention here is
        // one op per token release, not per store.
        let mut cur = self.longest_held_nanos.load(Ordering::Relaxed);
        while n > cur {
            match self.longest_held_nanos.compare_exchange_weak(
                cur,
                n,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(observed) => cur = observed,
            }
        }
    }
}

/// The critical-section report, raw plus normalized.
///
/// Every ratio is `0.0` when its denominator is zero, for the same reason
/// `GcMetricsReport` does it: a `NaN` in a summary line is
/// indistinguishable from a parse bug for whoever reads the log.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CriticalSectionReport {
    pub raw: CriticalMetricsRaw,
    /// Fraction of bounded waits that expired. The single number that says
    /// "GPU coordination is costing this workload collection latency".
    pub wait_timeout_rate: f64,
    /// Mean nanoseconds per bounded wait entered.
    pub wait_nanos_per_wait: f64,
    /// Fraction of acquired tokens the registry had to reap. **Any**
    /// non-zero value is a defect signal, not a tuning signal.
    pub leak_rate: f64,
    /// Forced non-moving collections per wait entered.
    pub forced_non_moving_per_wait: f64,
}

#[inline]
fn ratio(num: u64, den: u64) -> f64 {
    if den == 0 {
        0.0
    } else {
        num as f64 / den as f64
    }
}

impl CriticalSectionReport {
    /// Normalize a raw counter set. Pure — no globals, no allocation.
    pub fn from_raw(raw: CriticalMetricsRaw) -> Self {
        let reaped_total = raw
            .tokens_reaped
            .saturating_add(raw.tokens_reaped_at_shutdown);
        Self {
            raw,
            wait_timeout_rate: ratio(raw.waits_timed_out, raw.waits_entered),
            wait_nanos_per_wait: ratio(raw.wait_nanos, raw.waits_entered),
            leak_rate: ratio(reaped_total, raw.tokens_acquired),
            forced_non_moving_per_wait: ratio(raw.forced_non_moving_collections, raw.waits_entered),
        }
    }
}

impl std::fmt::Display for CriticalSectionReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let r = &self.raw;
        writeln!(
            f,
            "[GPU] critical: outstanding={} acquired={} released={} \
             longest_held_ms={:.3}",
            r.tokens_outstanding,
            r.tokens_acquired,
            r.tokens_released,
            r.longest_held_nanos as f64 / 1.0e6,
        )?;
        if r.tokens_reaped == 0 && r.tokens_reaped_at_shutdown == 0 {
            writeln!(
                f,
                "[GPU] critical leaks: none — every token was released by its holder"
            )?;
        } else {
            writeln!(
                f,
                "[GPU] critical leaks: reaped_lease_expired={} reaped_vm_shutdown={} \
                 leak_rate={:.4} — EVERY reap is a repaired leak; see the holders below",
                r.tokens_reaped, r.tokens_reaped_at_shutdown, self.leak_rate,
            )?;
        }
        if r.double_release_suppressed != 0 {
            writeln!(
                f,
                "[GPU] critical: double_release_suppressed={} — a token was released \
                 twice or after being reaped",
                r.double_release_suppressed,
            )?;
        }
        writeln!(
            f,
            "[GPU] critical waits: entered={} drained={} timed_out={} timeout_rate={:.4} \
             total_wait_ms={:.3} mean_wait_ms={:.3}",
            r.waits_entered,
            r.waits_drained,
            r.waits_timed_out,
            self.wait_timeout_rate,
            r.wait_nanos as f64 / 1.0e6,
            self.wait_nanos_per_wait / 1.0e6,
        )?;
        write!(
            f,
            "[GPU] critical fallback: forced_non_moving_collections={} \
             per_wait={:.4} (collections diverted to the non-moving path \
             because a GPU critical-section wait expired)",
            r.forced_non_moving_collections, self.forced_non_moving_per_wait,
        )
    }
}

// ---------------------------------------------------------------------------
// The registry
// ---------------------------------------------------------------------------

struct Record {
    owner: OwnerId,
    acquired_at: Instant,
    lease: Duration,
    relocation: Relocation,
    keepalive: Vec<usize>,
    revoked: Arc<AtomicBool>,
}

/// The owner of every live GPU critical-section token.
///
/// Production uses the single process-wide instance returned by
/// [`global`]. Tests construct their own so that parallel test threads
/// cannot perturb each other's arithmetic — see the module header for why
/// this differs from `gc_metrics`'s `thread_local!` approach.
pub struct Registry {
    live: Mutex<BTreeMap<u64, Record>>,
    /// Mirror of `live.len()`, readable without taking the lock. The
    /// collector's fast path ("is anything outstanding at all?") must not
    /// contend on a mutex the acquire path also takes.
    outstanding: AtomicU32,
    next_id: AtomicU64,
    /// Optional mirror into an external counter — in production, the
    /// pre-existing `cratonvm_gc::vm_heap::GPU_CRITICAL_COUNT`, so the
    /// legacy spin loops observe reaps as well as releases and cannot be
    /// left wedged by a token this registry has already revoked.
    mirror: OnceLock<&'static AtomicU32>,
    /// Bounded history of reaped tokens, so a leak can be named in a
    /// report taken after the fact.
    reap_log: Mutex<VecDeque<TokenFacts>>,
    counters: Counters,
}

impl Registry {
    /// A fresh, empty registry. Tests want this; production wants
    /// [`global`].
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            live: Mutex::new(BTreeMap::new()),
            outstanding: AtomicU32::new(0),
            next_id: AtomicU64::new(1),
            mirror: OnceLock::new(),
            reap_log: Mutex::new(VecDeque::new()),
            counters: Counters::new(),
        })
    }

    /// Lock `live`, recovering from poisoning.
    ///
    /// A panic while the map is locked must not wedge the collector
    /// forever — that would reintroduce, at a different layer, exactly the
    /// unbounded stall this module exists to remove. Every critical
    /// section over this lock is a short map mutation with no user code
    /// inside it, so the map cannot be observed half-updated.
    fn live(&self) -> std::sync::MutexGuard<'_, BTreeMap<u64, Record>> {
        self.live.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn reap_log(&self) -> std::sync::MutexGuard<'_, VecDeque<TokenFacts>> {
        self.reap_log.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Mirror this registry's outstanding count into an external counter.
    ///
    /// Call **once**, before any token is acquired, from the crate that
    /// owns the legacy counter. The current outstanding count is added on
    /// bind so a late bind cannot desynchronize the mirror downward.
    /// Returns `false` if a mirror was already bound.
    pub fn bind_mirror(&self, counter: &'static AtomicU32) -> bool {
        if self.mirror.set(counter).is_err() {
            return false;
        }
        let now = self.outstanding.load(Ordering::Acquire);
        if now != 0 {
            counter.fetch_add(now, Ordering::AcqRel);
        }
        true
    }

    fn mirror_add(&self) {
        if let Some(m) = self.mirror.get() {
            m.fetch_add(1, Ordering::AcqRel);
        }
    }

    fn mirror_sub(&self, n: u32) {
        if n == 0 {
            return;
        }
        if let Some(m) = self.mirror.get() {
            m.fetch_sub(n, Ordering::AcqRel);
        }
    }

    /// Acquire a token with the default lease and no declared roots.
    ///
    /// The short form for a critical section that only needs the "do not
    /// collect while I read the heap" property and holds no addresses past
    /// its own scope.
    ///
    /// An associated function rather than a method because the token must
    /// hold an owning `Arc` (it outlives the caller's borrow whenever it is
    /// moved onto a finalize thread), and `self: &Arc<Self>` is not a
    /// stable receiver.
    pub fn acquire(registry: &Arc<Self>, owner: OwnerId) -> CriticalToken {
        Self::acquire_with(
            registry,
            owner,
            default_token_lease(),
            Relocation::KeepAliveOnly,
            &[],
        )
    }

    /// Acquire a token, declaring a lease, a relocation policy, and the
    /// heap addresses the submission depends on.
    ///
    /// `keepalive` are raw object addresses in the same representation
    /// `gc::pinned::pinned_addrs` uses — `usize`, not `ObjectRef`, so this
    /// crate need not depend on `cratonvm-types` and so the collector can
    /// splice them into its root buffer with the same
    /// `ObjectRef::from_raw` step it already performs for JNI pins.
    pub fn acquire_with(
        registry: &Arc<Self>,
        owner: OwnerId,
        lease: Duration,
        relocation: Relocation,
        keepalive: &[usize],
    ) -> CriticalToken {
        let this: &Self = registry;
        let id = this.next_id.fetch_add(1, Ordering::Relaxed);
        let revoked = Arc::new(AtomicBool::new(false));
        let record = Record {
            owner,
            acquired_at: Instant::now(),
            lease,
            relocation,
            keepalive: keepalive.to_vec(),
            revoked: Arc::clone(&revoked),
        };
        {
            let mut live = this.live();
            live.insert(id, record);
            // Publish the count from under the lock so a concurrent
            // `wait_for_drain` can never observe `outstanding == 0` while
            // the map already holds this entry.
            this.outstanding.store(live.len() as u32, Ordering::Release);
        }
        this.mirror_add();
        this.counters
            .tokens_acquired
            .fetch_add(1, Ordering::Relaxed);
        CriticalToken {
            id,
            registry: Arc::clone(registry),
            revoked,
            released: false,
        }
    }

    fn release_inner(&self, id: u64, cause: ReleaseCause) {
        let removed = {
            let mut live = self.live();
            let removed = live.remove(&id);
            if removed.is_some() {
                self.outstanding.store(live.len() as u32, Ordering::Release);
            }
            removed
        };
        let Some(rec) = removed else {
            // Already gone: released twice, or reaped before the holder
            // got here. Idempotent by design (`release_submission` has the
            // same contract), but counted so it is never silent.
            self.counters
                .double_release_suppressed
                .fetch_add(1, Ordering::Relaxed);
            return;
        };
        self.mirror_sub(1);
        self.counters.bump_max(rec.acquired_at.elapsed());
        if cause.is_voluntary() {
            self.counters
                .tokens_released
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Tokens outstanding right now. Lock-free.
    pub fn outstanding(&self) -> u32 {
        self.outstanding.load(Ordering::Acquire)
    }

    /// Does any outstanding token forbid relocation?
    ///
    /// The collector must consult this **in addition to** the wait
    /// outcome: a token acquired after a `Drained` wait returned, but
    /// before the pause actually begins, still forbids relocation.
    pub fn relocation_forbidden(&self) -> bool {
        self.live()
            .values()
            .any(|r| r.relocation == Relocation::Forbidden)
    }

    /// Deduplicated keep-alive addresses declared by every outstanding
    /// token, for the collector to splice in as additional roots.
    ///
    /// Sorted, so the collector's root buffer is deterministic between
    /// cycles and a diff of two collections is readable.
    pub fn outstanding_keepalive_addrs(&self) -> Vec<usize> {
        let live = self.live();
        let mut out: Vec<usize> = live
            .values()
            .flat_map(|r| r.keepalive.iter().copied())
            .collect();
        out.sort_unstable();
        out.dedup();
        out
    }

    /// Rewrite declared keep-alive addresses after a moving collection.
    ///
    /// `remap` is the collector's pointer map lookup: it returns the new
    /// address for a moved object, or `None` for one that did not move.
    /// This is the exact counterpart of `gc::pinned::update_after_gc`, and
    /// omitting it is the failure mode §7 item 3 of the `ObjectRef`
    /// concurrency contract describes — an address-keyed table that
    /// silently breaks across a moving cycle.
    pub fn remap_keepalive(&self, remap: impl Fn(usize) -> Option<usize>) {
        let mut live = self.live();
        for rec in live.values_mut() {
            for addr in rec.keepalive.iter_mut() {
                if let Some(new) = remap(*addr) {
                    *addr = new;
                }
            }
        }
    }

    /// Snapshot one live token.
    pub fn facts(&self, id: u64) -> Option<TokenFacts> {
        self.live().get(&id).map(|r| TokenFacts {
            id,
            owner: r.owner.clone(),
            held: r.acquired_at.elapsed(),
            lease: r.lease,
            relocation: r.relocation,
            keepalive_addrs: r.keepalive.len(),
            cause: None,
        })
    }

    /// Snapshot every live token, oldest first.
    pub fn live_tokens(&self) -> Vec<TokenFacts> {
        let live = self.live();
        let mut out: Vec<TokenFacts> = live
            .iter()
            .map(|(id, r)| TokenFacts {
                id: *id,
                owner: r.owner.clone(),
                held: r.acquired_at.elapsed(),
                lease: r.lease,
                relocation: r.relocation,
                keepalive_addrs: r.keepalive.len(),
                cause: None,
            })
            .collect();
        out.sort_by(|a, b| b.held.cmp(&a.held));
        out
    }

    /// Tokens the registry has revoked, newest last, capped at
    /// `REAP_LOG_CAPACITY` (16).
    pub fn reaped_tokens(&self) -> Vec<TokenFacts> {
        self.reap_log().iter().cloned().collect()
    }

    fn push_reap_log(&self, facts: TokenFacts) {
        let mut log = self.reap_log();
        if log.len() == REAP_LOG_CAPACITY {
            log.pop_front();
        }
        log.push_back(facts);
    }

    /// Revoke and release every token whose lease has expired.
    ///
    /// Returns the reaped tokens, each naming its holder. Each one is also
    /// logged at `error` level, because a reap is a **repaired defect**:
    /// some exit path failed to release, and the registry is papering over
    /// it so the collector can make progress.
    ///
    /// Revocation is published (`AtomicBool` + `Release`) before the entry
    /// leaves the map, so a holder that checks
    /// [`CriticalToken::is_revoked`] before its writeback cannot observe
    /// "still valid" after the registry has stopped bounding the
    /// collector on its behalf.
    pub fn reap_expired(&self) -> Vec<TokenFacts> {
        // Cheap pre-check: the common case is nothing outstanding at all.
        if self.outstanding.load(Ordering::Acquire) == 0 {
            return Vec::new();
        }
        let now = Instant::now();
        let mut reaped = Vec::new();
        {
            let mut live = self.live();
            let expired: Vec<u64> = live
                .iter()
                .filter(|(_, r)| now.saturating_duration_since(r.acquired_at) >= r.lease)
                .map(|(id, _)| *id)
                .collect();
            for id in expired {
                let Some(rec) = live.remove(&id) else {
                    continue;
                };
                rec.revoked.store(true, Ordering::Release);
                reaped.push(TokenFacts {
                    id,
                    owner: rec.owner,
                    held: now.saturating_duration_since(rec.acquired_at),
                    lease: rec.lease,
                    relocation: rec.relocation,
                    keepalive_addrs: rec.keepalive.len(),
                    cause: Some(ReleaseCause::Reaped),
                });
            }
            if !reaped.is_empty() {
                self.outstanding.store(live.len() as u32, Ordering::Release);
            }
        }
        if reaped.is_empty() {
            return reaped;
        }
        self.mirror_sub(reaped.len() as u32);
        self.counters
            .tokens_reaped
            .fetch_add(reaped.len() as u64, Ordering::Relaxed);
        for f in &reaped {
            self.counters.bump_max(f.held);
            tracing::error!(
                "gpu critical: LEAKED token reaped after {:.3}ms (lease {:.3}ms) — {} \
                 [{} keep-alive root(s), {}]. Its holder never released it; the writeback \
                 is now poisoned via CriticalToken::is_revoked.",
                f.held.as_secs_f64() * 1.0e3,
                f.lease.as_secs_f64() * 1.0e3,
                f.owner,
                f.keepalive_addrs,
                f.relocation.label(),
            );
            self.push_reap_log(f.clone());
        }
        reaped
    }

    /// Revoke and release every token belonging to `vm`.
    ///
    /// This is the close for the VM-shutdown leak: a submission still in
    /// flight when its `SharedVm` is dropped can never be finalized (the
    /// completion reaper's `Weak::upgrade` fails and it returns silently),
    /// so nothing would otherwise drop the guard. Calling this from VM
    /// teardown converts "the next VM in this process can never collect"
    /// into a logged, attributed release.
    pub fn shutdown_vm(&self, vm: u64) -> Vec<TokenFacts> {
        let now = Instant::now();
        let mut reaped = Vec::new();
        {
            let mut live = self.live();
            let owned: Vec<u64> = live
                .iter()
                .filter(|(_, r)| r.owner.vm == vm)
                .map(|(id, _)| *id)
                .collect();
            for id in owned {
                let Some(rec) = live.remove(&id) else {
                    continue;
                };
                rec.revoked.store(true, Ordering::Release);
                reaped.push(TokenFacts {
                    id,
                    owner: rec.owner,
                    held: now.saturating_duration_since(rec.acquired_at),
                    lease: rec.lease,
                    relocation: rec.relocation,
                    keepalive_addrs: rec.keepalive.len(),
                    cause: Some(ReleaseCause::VmShutdown),
                });
            }
            if !reaped.is_empty() {
                self.outstanding.store(live.len() as u32, Ordering::Release);
            }
        }
        if reaped.is_empty() {
            return reaped;
        }
        self.mirror_sub(reaped.len() as u32);
        self.counters
            .tokens_reaped_at_shutdown
            .fetch_add(reaped.len() as u64, Ordering::Relaxed);
        for f in &reaped {
            self.counters.bump_max(f.held);
            tracing::warn!(
                "gpu critical: token still outstanding at VM shutdown, reaped after \
                 {:.3}ms — {}. The submission it bracketed can no longer be finalized.",
                f.held.as_secs_f64() * 1.0e3,
                f.owner,
            );
            self.push_reap_log(f.clone());
        }
        reaped
    }

    /// Wait, **bounded by `budget`**, for every GPU critical section to
    /// drain.
    ///
    /// This is the entry point a collector calls in place of the
    /// unbounded `wait_for_gpu_critical_drain()`. It:
    ///
    /// * returns [`WaitOutcome::Drained`] immediately when nothing is
    ///   outstanding (the overwhelmingly common case — one relaxed load);
    /// * reaps expired tokens on each iteration, so a wait whose only
    ///   holders are orphans drains rather than expiring;
    /// * returns [`WaitOutcome::TimedOut`] the moment `budget` elapses,
    ///   naming the holders — it **never** force-releases a live,
    ///   in-lease token to make itself succeed.
    ///
    /// The caller's obligation on `TimedOut` is documented on that
    /// variant: degrade to a non-moving cycle with the returned keep-alive
    /// addresses as extra roots, and record the diversion via
    /// [`Registry::record_forced_non_moving_collection`].
    pub fn wait_for_drain(&self, budget: Duration) -> WaitOutcome {
        self.counters.waits_entered.fetch_add(1, Ordering::Relaxed);
        if self.outstanding.load(Ordering::Acquire) == 0 {
            self.counters.waits_drained.fetch_add(1, Ordering::Relaxed);
            return WaitOutcome::Drained {
                waited: Duration::ZERO,
            };
        }

        let start = Instant::now();
        let mut spins: u32 = 0;
        loop {
            // Reaping inside the loop is what makes an orphaned token a
            // bounded delay rather than a permanent one: if every
            // remaining holder is past its lease, this drains the wait.
            self.reap_expired();
            if self.outstanding.load(Ordering::Acquire) == 0 {
                let waited = start.elapsed();
                self.counters.waits_drained.fetch_add(1, Ordering::Relaxed);
                self.counters.wait_nanos.fetch_add(
                    waited.as_nanos().min(u64::MAX as u128) as u64,
                    Ordering::Relaxed,
                );
                return WaitOutcome::Drained { waited };
            }

            let waited = start.elapsed();
            if waited >= budget {
                self.counters
                    .waits_timed_out
                    .fetch_add(1, Ordering::Relaxed);
                self.counters.wait_nanos.fetch_add(
                    waited.as_nanos().min(u64::MAX as u128) as u64,
                    Ordering::Relaxed,
                );
                return self.timed_out(waited);
            }

            // Escalating backoff, capped so the loop never sleeps past the
            // budget: a bound that overshoots by a sleep quantum is not a
            // bound.
            spins = spins.saturating_add(1);
            if spins < 64 {
                std::hint::spin_loop();
            } else if spins < 128 {
                std::thread::yield_now();
            } else {
                let remaining = budget.saturating_sub(waited);
                std::thread::sleep(remaining.min(Duration::from_micros(200)));
            }
        }
    }

    /// Wait using the process-wide budget from
    /// [`collector_wait_budget`].
    pub fn wait_for_drain_default(&self) -> WaitOutcome {
        self.wait_for_drain(collector_wait_budget())
    }

    fn timed_out(&self, waited: Duration) -> WaitOutcome {
        let live = self.live_tokens();
        let outstanding = live.len() as u32;
        let longest_held = live.first().map(|f| f.held).unwrap_or_default();
        let relocation_forbidden = live.iter().any(|f| f.relocation == Relocation::Forbidden);
        let holders_truncated = live.len() > MAX_NAMED_HOLDERS;
        let holders: Vec<TokenFacts> = live.into_iter().take(MAX_NAMED_HOLDERS).collect();
        let outcome = WaitOutcome::TimedOut {
            waited,
            outstanding,
            relocation_forbidden,
            keepalive_addrs: self.outstanding_keepalive_addrs(),
            longest_held,
            holders,
            holders_truncated,
        };
        tracing::warn!("{outcome}");
        outcome
    }

    /// Record that a collection was diverted to the non-moving path
    /// because a GPU critical-section wait expired.
    ///
    /// Called by the collector on the [`WaitOutcome::TimedOut`] arm. Kept
    /// as an explicit call rather than being bumped inside
    /// `wait_for_drain` so the counter measures *collections actually
    /// diverted*, not *waits that expired* — a collector that was already
    /// going to run non-moving for its own reasons must not inflate the
    /// figure GPU coordination is blamed for.
    pub fn record_forced_non_moving_collection(&self) {
        self.counters
            .forced_non_moving_collections
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Snapshot the raw counters.
    pub fn metrics_raw(&self) -> CriticalMetricsRaw {
        let c = &self.counters;
        CriticalMetricsRaw {
            tokens_acquired: c.tokens_acquired.load(Ordering::Relaxed),
            tokens_released: c.tokens_released.load(Ordering::Relaxed),
            tokens_reaped: c.tokens_reaped.load(Ordering::Relaxed),
            tokens_reaped_at_shutdown: c.tokens_reaped_at_shutdown.load(Ordering::Relaxed),
            double_release_suppressed: c.double_release_suppressed.load(Ordering::Relaxed),
            tokens_outstanding: self.outstanding.load(Ordering::Acquire) as u64,
            longest_held_nanos: c.longest_held_nanos.load(Ordering::Relaxed),
            waits_entered: c.waits_entered.load(Ordering::Relaxed),
            waits_drained: c.waits_drained.load(Ordering::Relaxed),
            waits_timed_out: c.waits_timed_out.load(Ordering::Relaxed),
            wait_nanos: c.wait_nanos.load(Ordering::Relaxed),
            forced_non_moving_collections: c.forced_non_moving_collections.load(Ordering::Relaxed),
        }
    }

    /// The normalized report. Cheap; safe to call outside a pause. The
    /// counters are not snapshot-consistent with each other — they are
    /// observability, not a transaction.
    pub fn metrics_report(&self) -> CriticalSectionReport {
        CriticalSectionReport::from_raw(self.metrics_raw())
    }

    /// The full human-readable report: counters, then the live holders,
    /// then any tokens the registry had to reap.
    ///
    /// Deliberately shaped like
    /// `gc_metrics::collector_decision_report()` — one `[GPU]`-prefixed
    /// line per fact, holders named rather than summarized, so a stall in
    /// a log is diagnosable without a debugger.
    pub fn report(&self) -> String {
        let mut s = self.metrics_report().to_string();
        let live = self.live_tokens();
        if live.is_empty() {
            s.push_str("\n[GPU] critical holders: none outstanding");
        } else {
            for f in live.iter().take(MAX_NAMED_HOLDERS) {
                s.push_str(&format!("\n[GPU] critical holder: {f}"));
            }
            if live.len() > MAX_NAMED_HOLDERS {
                s.push_str(&format!(
                    "\n[GPU] critical holders: … {} more not shown",
                    live.len() - MAX_NAMED_HOLDERS,
                ));
            }
        }
        for f in self.reaped_tokens() {
            s.push_str(&format!("\n[GPU] critical reaped: {f}"));
        }
        s
    }

    /// Reset every counter and forget the reap log. Tests only —
    /// production counters are monotonic for the life of the process.
    ///
    /// Does **not** touch live tokens: a reset that silently released
    /// them would be a worse version of the bug this module exists to
    /// prevent.
    pub fn reset_metrics_for_test(&self) {
        let c = &self.counters;
        c.tokens_acquired.store(0, Ordering::Relaxed);
        c.tokens_released.store(0, Ordering::Relaxed);
        c.tokens_reaped.store(0, Ordering::Relaxed);
        c.tokens_reaped_at_shutdown.store(0, Ordering::Relaxed);
        c.double_release_suppressed.store(0, Ordering::Relaxed);
        c.longest_held_nanos.store(0, Ordering::Relaxed);
        c.waits_entered.store(0, Ordering::Relaxed);
        c.waits_drained.store(0, Ordering::Relaxed);
        c.waits_timed_out.store(0, Ordering::Relaxed);
        c.wait_nanos.store(0, Ordering::Relaxed);
        c.forced_non_moving_collections.store(0, Ordering::Relaxed);
        self.reap_log().clear();
    }
}

/// Method form of [`Registry::acquire`] / [`Registry::acquire_with`].
///
/// [`Registry::acquire`] has to be an associated function because a token
/// owns an `Arc<Registry>` and `self: &Arc<Self>` is not a stable
/// receiver. This trait restores the `registry.acquire(owner)` call shape
/// at the cost of one `use`. Both forms are the same code; pick whichever
/// reads better at the call site.
pub trait CriticalRegistryExt {
    /// See [`Registry::acquire`].
    fn acquire(&self, owner: OwnerId) -> CriticalToken;
    /// See [`Registry::acquire_with`].
    fn acquire_with(
        &self,
        owner: OwnerId,
        lease: Duration,
        relocation: Relocation,
        keepalive: &[usize],
    ) -> CriticalToken;
}

impl CriticalRegistryExt for Arc<Registry> {
    fn acquire(&self, owner: OwnerId) -> CriticalToken {
        Registry::acquire(self, owner)
    }

    fn acquire_with(
        &self,
        owner: OwnerId,
        lease: Duration,
        relocation: Relocation,
        keepalive: &[usize],
    ) -> CriticalToken {
        Registry::acquire_with(self, owner, lease, relocation, keepalive)
    }
}

static GLOBAL: OnceLock<Arc<Registry>> = OnceLock::new();

/// The one process-wide registry.
///
/// Process-global for the same reason `GPU_CRITICAL_COUNT` is: there is
/// one collector per process, so the set of things that can delay it is a
/// process-level fact. Per-VM scoping lives in [`OwnerId::vm`] and
/// [`Registry::shutdown_vm`], not in separate registries — a second VM
/// must be able to see (and outlive) a first VM's abandoned tokens.
pub fn global() -> &'static Arc<Registry> {
    GLOBAL.get_or_init(Registry::new)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    // ── Device-layer mock ───────────────────────────────────────────────
    //
    // No GPU, no driver, not even `cuda_bridge::Stream`: the token
    // lifetime contract is a host-side property, and stubbing the device
    // as an enum makes "the kernel was cancelled" and "the device
    // faulted" as reachable in a unit test as "it succeeded". This is the
    // whole reason the registry does not depend on `DeviceContext`.

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum FakeDevice {
        /// Kernel completes and the writeback drains.
        Succeeds,
        /// `event.synchronize()` fails — the device-error path.
        Faults,
        /// The host cancels before the kernel is observed complete.
        Cancelled,
        /// The host thread panics mid-submission.
        Panics,
        /// The submission is dispatched and then abandoned: the token is
        /// deliberately leaked, exactly as a `FinalizeState` that nobody
        /// ever drains leaks its `GcCriticalGuard`.
        NeverCompletes,
    }

    /// Run one fake submission under a token. Returns the token id on a
    /// completed writeback, `Err` on device error or cancel. Panics for
    /// `FakeDevice::Panics` (the caller catches it).
    fn run_submission(
        reg: &Arc<Registry>,
        device: FakeDevice,
        owner: OwnerId,
        lease: Duration,
    ) -> Result<u64, String> {
        let token = reg.acquire_with(
            owner,
            lease,
            Relocation::KeepAliveOnly,
            &[0xdead_0000, 0xbeef_0000],
        );
        let id = token.id();
        match device {
            FakeDevice::Succeeds => {
                assert!(!token.is_revoked(), "a live token must not be revoked");
                Ok(id)
            }
            FakeDevice::Faults => Err("event.synchronize: fake device fault".to_string()),
            FakeDevice::Cancelled => Err("cancelled".to_string()),
            FakeDevice::Panics => panic!("fake host-thread panic mid-submission"),
            FakeDevice::NeverCompletes => {
                // The leak: the token escapes its scope without ever being
                // released. `mem::forget` is the closest a test can get to
                // "an `Arc<StreamSubmission>` nobody drops".
                std::mem::forget(token);
                Ok(id)
            }
        }
    }

    fn owner(site: &'static str) -> OwnerId {
        OwnerId::current(1, Some(7), site)
    }

    // ── 1. Released on every exit path ──────────────────────────────────

    #[test]
    fn token_is_released_on_success_error_and_cancel() {
        for device in [
            FakeDevice::Succeeds,
            FakeDevice::Faults,
            FakeDevice::Cancelled,
        ] {
            let reg = Registry::new();
            let _ = run_submission(&reg, device, owner("unit"), Duration::from_secs(3_600));
            assert_eq!(
                reg.outstanding(),
                0,
                "{device:?} must leave no outstanding token",
            );
            let raw = reg.metrics_raw();
            assert_eq!(raw.tokens_acquired, 1);
            assert_eq!(
                raw.tokens_released, 1,
                "{device:?} must count as a VOLUNTARY release, not a reap",
            );
            assert_eq!(raw.tokens_reaped, 0);
        }
    }

    #[test]
    fn token_is_released_when_the_holder_panics() {
        let reg = Registry::new();
        let r = Arc::clone(&reg);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = run_submission(
                &r,
                FakeDevice::Panics,
                owner("panic-path"),
                Duration::from_secs(3_600),
            );
        }));
        assert!(result.is_err(), "the fake submission must have panicked");
        assert_eq!(
            reg.outstanding(),
            0,
            "unwinding through the frame must run the token's Drop",
        );
        assert_eq!(reg.metrics_raw().tokens_released, 1);
    }

    #[test]
    fn token_is_released_at_vm_shutdown_and_the_holder_learns_it() {
        let reg = Registry::new();
        // Two VMs, one token each — the exact multi-`SharedVm` situation
        // `offload.rs`'s REAPER_QUEUE comment says real-hardware
        // validation caught.
        let t1 = reg.acquire(OwnerId::current(1, Some(11), "vm1-dispatch"));
        let t2 = reg.acquire(OwnerId::current(2, Some(22), "vm2-dispatch"));
        assert_eq!(reg.outstanding(), 2);

        let reaped = reg.shutdown_vm(1);
        assert_eq!(reaped.len(), 1, "only VM 1's token may be reaped");
        assert_eq!(reaped[0].owner.vm, 1);
        assert_eq!(reaped[0].cause, Some(ReleaseCause::VmShutdown));
        assert_eq!(
            reg.outstanding(),
            1,
            "a concurrently-live second VM keeps its token",
        );
        assert!(
            t1.is_revoked(),
            "the shut-down VM's holder must observe revocation before it writes back",
        );
        assert!(!t2.is_revoked(), "the live VM's holder is untouched");

        // Dropping the already-reaped token is idempotent and must not
        // decrement anything a second time.
        drop(t1);
        assert_eq!(reg.outstanding(), 1);
        assert_eq!(reg.metrics_raw().double_release_suppressed, 1);
        drop(t2);
        assert_eq!(reg.outstanding(), 0);
    }

    // ── 2. The wait is bounded ──────────────────────────────────────────

    #[test]
    fn a_stalled_submission_produces_a_bounded_wait_not_an_unbounded_spin() {
        let reg = Registry::new();
        // A lease far longer than the wait budget: the token is stalled,
        // NOT orphaned, so the registry must not reap it — the wait has to
        // terminate on its own budget instead.
        let _token = reg.acquire_with(
            owner("stalled-kernel"),
            Duration::from_secs(3_600),
            Relocation::KeepAliveOnly,
            &[0x1000, 0x2000],
        );

        let budget = Duration::from_millis(20);
        let started = Instant::now();
        let outcome = reg.wait_for_drain(budget);
        let elapsed = started.elapsed();

        assert!(
            !outcome.drained(),
            "a stalled token must not report drained"
        );
        assert!(
            !outcome.may_relocate(),
            "a timeout must never authorize relocation",
        );
        assert!(
            elapsed >= budget,
            "the wait must actually wait its budget, not return early",
        );
        assert!(
            elapsed < budget * 50,
            "the wait must be BOUNDED: {elapsed:?} for a {budget:?} budget",
        );

        match outcome {
            WaitOutcome::TimedOut {
                outstanding,
                relocation_forbidden,
                keepalive_addrs,
                holders,
                ..
            } => {
                assert_eq!(outstanding, 1);
                assert!(!relocation_forbidden, "this token declared keep-alive only");
                assert_eq!(
                    keepalive_addrs,
                    vec![0x1000, 0x2000],
                    "the fail-safe cycle needs the roots the submission depends on",
                );
                assert_eq!(holders.len(), 1);
                assert_eq!(holders[0].owner.site, "stalled-kernel");
            }
            other => panic!("expected TimedOut, got {other:?}"),
        }

        let raw = reg.metrics_raw();
        assert_eq!(raw.waits_entered, 1);
        assert_eq!(raw.waits_timed_out, 1);
        assert_eq!(raw.waits_drained, 0);
        assert_eq!(
            raw.tokens_reaped, 0,
            "an in-lease token must NOT be force-released to make the wait succeed",
        );
    }

    #[test]
    fn a_drained_wait_reports_drained_and_permits_relocation() {
        let reg = Registry::new();
        {
            let _t = reg.acquire(owner("short"));
        }
        let outcome = reg.wait_for_drain(Duration::from_millis(50));
        assert!(outcome.drained());
        assert!(outcome.may_relocate());
        assert!(outcome.keepalive_addrs().is_empty());
        let raw = reg.metrics_raw();
        assert_eq!(raw.waits_drained, 1);
        assert_eq!(raw.waits_timed_out, 0);
    }

    #[test]
    fn a_forbidden_relocation_token_is_reported_as_such_on_timeout() {
        let reg = Registry::new();
        let _t = reg.acquire_with(
            owner("zero-copy-kernel"),
            Duration::from_secs(3_600),
            Relocation::Forbidden,
            &[0xabc0],
        );
        match reg.wait_for_drain(Duration::from_millis(5)) {
            WaitOutcome::TimedOut {
                relocation_forbidden,
                ..
            } => assert!(
                relocation_forbidden,
                "a token holding a device-visible heap address must forbid relocation",
            ),
            other => panic!("expected TimedOut, got {other:?}"),
        }
        assert!(reg.relocation_forbidden());
    }

    // ── 3. Orphans are reaped and named ─────────────────────────────────

    #[test]
    fn an_orphaned_token_is_reaped_and_names_its_holder() {
        let reg = Registry::new();
        // A submission that never completes, with a one-millisecond lease
        // standing in for the production default.
        let leaked = reg.acquire_with(
            OwnerId::current(42, Some(9_001), "abandoned-dispatch"),
            Duration::from_millis(1),
            Relocation::KeepAliveOnly,
            &[0xfeed],
        );
        let revoked_flag = Arc::clone(&leaked.revoked);
        std::mem::forget(leaked);
        assert_eq!(reg.outstanding(), 1);

        // A wait that would spin forever under the old contract instead
        // drains, because the orphan is reaped inside the loop.
        std::thread::sleep(Duration::from_millis(5));
        let outcome = reg.wait_for_drain(Duration::from_millis(500));
        assert!(
            outcome.drained(),
            "reaping the orphan must let the wait drain: {outcome}",
        );
        assert_eq!(reg.outstanding(), 0);
        assert!(
            revoked_flag.load(Ordering::Acquire),
            "a reaped token must be revoked so its writeback is suppressed",
        );

        let raw = reg.metrics_raw();
        assert_eq!(raw.tokens_reaped, 1);
        assert_eq!(raw.tokens_released, 0, "a reap is not a voluntary release");
        assert!(raw.longest_held_nanos > 0);

        // The leak NAMES its holder rather than manifesting as a stall.
        let reaped = reg.reaped_tokens();
        assert_eq!(reaped.len(), 1);
        assert_eq!(reaped[0].owner.vm, 42);
        assert_eq!(reaped[0].owner.submission, Some(9_001));
        assert_eq!(reaped[0].cause, Some(ReleaseCause::Reaped));

        let text = reg.report();
        assert!(text.contains("abandoned-dispatch"), "{text}");
        assert!(text.contains("submission=9001"), "{text}");
        assert!(text.contains("reaped-lease-expired"), "{text}");
        assert!(
            text.contains("EVERY reap is a repaired leak"),
            "the report must not present a reap as routine: {text}",
        );
    }

    /// The same leak, driven end-to-end through the mocked device layer
    /// rather than by a hand-forgotten token: a submission that is
    /// dispatched and then never completes must not be able to stall the
    /// collector for the rest of the process.
    #[test]
    fn a_submission_that_never_completes_is_bounded_by_its_lease() {
        let reg = Registry::new();
        let id = run_submission(
            &reg,
            FakeDevice::NeverCompletes,
            OwnerId::current(5, Some(404), "never-completes"),
            Duration::from_millis(1),
        )
        .expect("dispatch itself succeeds; it is the completion that never arrives");

        assert!(
            reg.facts(id).is_some(),
            "the token is live right after dispatch"
        );
        assert_eq!(reg.outstanding(), 1);

        std::thread::sleep(Duration::from_millis(5));
        let outcome = reg.wait_for_drain(Duration::from_millis(500));
        assert!(outcome.drained(), "{outcome}");
        assert!(
            reg.facts(id).is_none(),
            "the abandoned token must be gone from the registry",
        );
        let reaped = reg.reaped_tokens();
        assert!(
            reaped
                .iter()
                .any(|f| f.id == id && f.owner.submission == Some(404)),
            "the reap log must name the abandoned submission: {reaped:?}",
        );
    }

    #[test]
    fn the_reap_log_is_bounded() {
        let reg = Registry::new();
        for i in 0..(REAP_LOG_CAPACITY * 2) {
            let t = reg.acquire_with(
                OwnerId::current(1, Some(i as u64), "leaky"),
                Duration::ZERO,
                Relocation::KeepAliveOnly,
                &[],
            );
            std::mem::forget(t);
            reg.reap_expired();
        }
        assert_eq!(reg.reaped_tokens().len(), REAP_LOG_CAPACITY);
        assert_eq!(
            reg.metrics_raw().tokens_reaped,
            (REAP_LOG_CAPACITY * 2) as u64
        );
    }

    // ── 4. Concurrency ──────────────────────────────────────────────────

    #[test]
    fn concurrent_acquire_release_neither_loses_nor_double_releases() {
        const THREADS: usize = 8;
        const PER_THREAD: usize = 200;

        let reg = Registry::new();
        let mirror: &'static AtomicU32 = Box::leak(Box::new(AtomicU32::new(0)));
        assert!(reg.bind_mirror(mirror));

        let observed_nonzero = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();
        for _ in 0..THREADS {
            let r = Arc::clone(&reg);
            let seen = Arc::clone(&observed_nonzero);
            handles.push(std::thread::spawn(move || {
                for i in 0..PER_THREAD {
                    let t = r.acquire_with(
                        OwnerId::current(1, Some(i as u64), "churn"),
                        Duration::from_secs(3_600),
                        Relocation::KeepAliveOnly,
                        &[i],
                    );
                    if r.outstanding() > 0 {
                        seen.fetch_add(1, Ordering::Relaxed);
                    }
                    // Alternate the two release paths so both are exercised
                    // under contention.
                    if i % 2 == 0 {
                        t.release();
                    } else {
                        drop(t);
                    }
                }
            }));
        }
        for h in handles {
            h.join().expect("worker panicked");
        }

        let total = (THREADS * PER_THREAD) as u64;
        let raw = reg.metrics_raw();
        assert_eq!(raw.tokens_acquired, total);
        assert_eq!(raw.tokens_released, total, "no release was lost");
        assert_eq!(
            raw.double_release_suppressed, 0,
            "no token was released twice",
        );
        assert_eq!(reg.outstanding(), 0, "the gauge returned to zero");
        assert_eq!(
            mirror.load(Ordering::Acquire),
            0,
            "the mirrored legacy counter must also return to zero, or the old \
             spin loops stay wedged",
        );
        assert!(
            observed_nonzero.load(Ordering::Relaxed) > 0,
            "the test never actually held a token concurrently",
        );
    }

    #[test]
    fn the_mirror_tracks_reaps_as_well_as_releases() {
        // The failure this pins: reaping a token in THIS registry while
        // the legacy `GPU_CRITICAL_COUNT` keeps its increment would leave
        // `wait_for_gpu_critical_drain` spinning forever on a token that
        // no longer exists.
        let reg = Registry::new();
        let mirror: &'static AtomicU32 = Box::leak(Box::new(AtomicU32::new(0)));
        assert!(reg.bind_mirror(mirror));
        assert!(!reg.bind_mirror(mirror), "binding twice must be refused");

        let t = reg.acquire_with(
            owner("leaky"),
            Duration::ZERO,
            Relocation::KeepAliveOnly,
            &[],
        );
        std::mem::forget(t);
        assert_eq!(mirror.load(Ordering::Acquire), 1);
        assert_eq!(reg.reap_expired().len(), 1);
        assert_eq!(
            mirror.load(Ordering::Acquire),
            0,
            "a reap must decrement the mirrored counter too",
        );
    }

    // ── 5. GC-latency attribution ───────────────────────────────────────

    #[test]
    fn forced_non_moving_collections_are_attributed_to_gpu_waiting() {
        let reg = Registry::new();
        let _t = reg.acquire_with(
            owner("long-kernel"),
            Duration::from_secs(3_600),
            Relocation::KeepAliveOnly,
            &[0x40, 0x80],
        );

        // Two collection attempts; only the ones that actually time out
        // and divert are attributed.
        let mut diverted = 0;
        for _ in 0..2 {
            let outcome = reg.wait_for_drain(Duration::from_millis(2));
            if !outcome.may_relocate() {
                // This is the collector's documented expiry behaviour.
                let roots = outcome.keepalive_addrs().to_vec();
                assert!(
                    !roots.is_empty(),
                    "the fail-safe cycle needs its extra roots"
                );
                reg.record_forced_non_moving_collection();
                diverted += 1;
            }
        }
        assert_eq!(diverted, 2);

        let report = reg.metrics_report();
        assert_eq!(report.raw.forced_non_moving_collections, 2);
        assert_eq!(report.raw.waits_entered, 2);
        assert_eq!(report.raw.waits_timed_out, 2);
        assert_eq!(report.wait_timeout_rate, 1.0);
        assert_eq!(report.forced_non_moving_per_wait, 1.0);

        let text = report.to_string();
        assert!(text.contains("forced_non_moving_collections=2"), "{text}");
        assert!(
            text.contains("because a GPU critical-section wait expired"),
            "the report must attribute the diversion, not merely count it: {text}",
        );
        assert!(text.contains("timed_out=2"), "{text}");
    }

    #[test]
    fn a_wait_that_never_ran_reports_no_cost_rather_than_nan() {
        let report = CriticalSectionReport::from_raw(CriticalMetricsRaw::default());
        for v in [
            report.wait_timeout_rate,
            report.wait_nanos_per_wait,
            report.leak_rate,
            report.forced_non_moving_per_wait,
        ] {
            assert_eq!(v, 0.0, "a zero denominator must normalize to 0.0, not NaN");
            assert!(v.is_finite());
        }
        let text = report.to_string();
        assert!(text.contains("outstanding=0"), "{text}");
        assert!(
            text.contains("critical leaks: none"),
            "a clean run must say so explicitly rather than printing nothing: {text}",
        );
    }

    // ── Keep-alive addresses / remap ────────────────────────────────────

    #[test]
    fn keepalive_addresses_are_deduped_sorted_and_remappable() {
        let reg = Registry::new();
        let _a = reg.acquire_with(
            owner("a"),
            Duration::from_secs(60),
            Relocation::KeepAliveOnly,
            &[0x300, 0x100],
        );
        let _b = reg.acquire_with(
            owner("b"),
            Duration::from_secs(60),
            Relocation::KeepAliveOnly,
            &[0x100, 0x200],
        );
        assert_eq!(
            reg.outstanding_keepalive_addrs(),
            vec![0x100, 0x200, 0x300],
            "the collector's extra-root buffer must be deterministic",
        );

        // A moving cycle relocated 0x100 -> 0x900 and left the rest.
        reg.remap_keepalive(|a| if a == 0x100 { Some(0x900) } else { None });
        assert_eq!(
            reg.outstanding_keepalive_addrs(),
            vec![0x200, 0x300, 0x900],
            "keep-alive addresses are address-keyed and MUST be remapped after a \
             moving cycle, exactly like gc::pinned::update_after_gc",
        );
    }

    // ── Labels ──────────────────────────────────────────────────────────

    #[test]
    fn every_release_cause_and_relocation_policy_has_a_distinct_label() {
        let causes = [
            ReleaseCause::Released,
            ReleaseCause::Dropped,
            ReleaseCause::Reaped,
            ReleaseCause::VmShutdown,
        ];
        let mut seen: Vec<&str> = Vec::new();
        for c in causes {
            assert!(!c.label().is_empty());
            assert!(
                !seen.contains(&c.label()),
                "duplicate label {:?}",
                c.label()
            );
            seen.push(c.label());
        }
        assert!(ReleaseCause::Released.is_voluntary());
        assert!(ReleaseCause::Dropped.is_voluntary());
        assert!(
            !ReleaseCause::Reaped.is_voluntary(),
            "a reap must never be summed into the voluntary-release total",
        );
        assert!(!ReleaseCause::VmShutdown.is_voluntary());

        assert_ne!(
            Relocation::KeepAliveOnly.label(),
            Relocation::Forbidden.label(),
            "the two pin vocabularies must never print identically",
        );
    }

    #[test]
    fn owner_display_names_every_field_a_stall_needs() {
        let o = OwnerId {
            vm: 3,
            thread: 5,
            thread_name: Some("gpu-worker".to_string()),
            submission: Some(77),
            site: "dispatch_method_from_native",
        };
        let text = o.to_string();
        assert!(text.contains("vm=3"), "{text}");
        assert!(text.contains("thread=5(gpu-worker)"), "{text}");
        assert!(text.contains("submission=77"), "{text}");
        assert!(text.contains("site=dispatch_method_from_native"), "{text}");

        let anon = OwnerId {
            submission: None,
            thread_name: None,
            ..o
        };
        assert!(anon.to_string().contains("submission=none"));
    }
}
