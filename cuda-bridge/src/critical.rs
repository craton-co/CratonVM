// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
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
//! token declares here are **keep-alive** (must not be freed, remapped after a moving cycle)
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
    /// `true` for [`WaitOutcome::Drained`], and for a timeout whose
    /// outstanding tokens are all [`Relocation::KeepAliveOnly`].
    ///
    /// AUDIT 2026-09-02: this used to answer `false` for every timeout,
    /// on the grounds that a `KeepAliveOnly` holder still had raw
    /// `ObjectRef`s captured at acquisition. That holder now reads its
    /// addresses back through [`CriticalToken::keepalive_addrs`] after
    /// the collector's [`Registry::remap_keepalive`] has rewritten them,
    /// so a moving cycle under a keep-alive token is exactly what the
    /// keep-alive-and-remap contract was built for. Only a token that
    /// declared `Forbidden` — one whose holder has handed the device a
    /// heap address for the length of a DMA — still forbids the move.
    pub fn may_relocate(&self) -> bool {
        match self {
            WaitOutcome::Drained { .. } => true,
            WaitOutcome::TimedOut {
                relocation_forbidden,
                ..
            } => !relocation_forbidden,
        }
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

    /// The keep-alive addresses this token declared, **as the collector
    /// last left them**.
    ///
    /// This is the read half of [`Registry::remap_keepalive`]. A holder
    /// that captured `ObjectRef`s at acquisition and then let a moving
    /// collector run must not write through those captures; it reads
    /// the current addresses back here, in declaration order, and uses
    /// those. Empty once the token has been revoked, so a revoked holder
    /// that forgets to check [`CriticalToken::is_revoked`] still has
    /// nothing to write through.
    pub fn keepalive_addrs(&self) -> Vec<usize> {
        self.registry
            .keepalive_addrs_of(self.id)
            .unwrap_or_default()
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

/// Snapshot of process-wide GPU allocator, VMM, and pinned-staging telemetry
/// captured from [`cratonvm_types::gpu_event_census`].
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct GpuTelemetrySnapshot {
    pub vram_used_bytes: u64,
    pub vram_peak_bytes: u64,
    pub vram_pool_hit_rate: f64,
    pub alloc_pool_hits: u64,
    pub alloc_pool_misses: u64,
    pub alloc_pool_parked: u64,
    pub stream_ordered_reuses: u64,
    pub gc_triggered_by_vram_pressure: u64,
    pub vmm_remaps: u64,
    pub vmm_active_chunks: u64,
    pub vmm_idle_mapped_chunks: u64,
    pub vmm_parked_chunks: u64,
    pub vmm_unmapped_chunks: u64,
    pub vmm_defrag_watermark_hits: u64,
    pub vmm_prewarm_created: u64,
    pub managed_spill_allocs: u64,
    pub managed_spill_bytes: u64,
    pub tier1_latency_count: u64,
    pub tier1_latency_total_ns: u64,
    pub tier2_latency_count: u64,
    pub tier2_latency_total_ns: u64,
    pub pinned_pool_hits: u64,
    pub pinned_pool_misses: u64,
    pub host_pin_cache_hits: u64,
    pub host_pin_cache_registers: u64,
}

impl GpuTelemetrySnapshot {
    /// Read the current process-wide GPU telemetry counters.
    pub fn capture() -> Self {
        use cratonvm_types::gpu_event_census as census;
        let (vram_used_bytes, vram_peak_bytes) = census::vram_used_and_peak();
        let (alloc_pool_hits, alloc_pool_misses, alloc_pool_parked) = census::alloc_totals();
        let (pinned_pool_hits, pinned_pool_misses) = census::pinned_pool_totals();
        let (host_pin_cache_hits, host_pin_cache_registers) = census::host_pin_cache_totals();
        let vmm_chunks = census::vmm_chunk_counts();
        let (managed_spill_allocs, managed_spill_bytes) = census::managed_spill_totals();
        let tier1 = census::tier1_latency_histogram();
        let tier2 = census::tier2_latency_histogram();
        Self {
            vram_used_bytes,
            vram_peak_bytes,
            vram_pool_hit_rate: census::vram_pool_hit_rate(),
            alloc_pool_hits,
            alloc_pool_misses,
            alloc_pool_parked,
            stream_ordered_reuses: census::stream_ordered_reuses(),
            gc_triggered_by_vram_pressure: census::gc_triggered_by_vram_pressure(),
            vmm_remaps: census::vmm_remaps(),
            vmm_active_chunks: vmm_chunks.active,
            vmm_idle_mapped_chunks: vmm_chunks.idle_mapped,
            vmm_parked_chunks: vmm_chunks.parked,
            vmm_unmapped_chunks: vmm_chunks.unmapped,
            vmm_defrag_watermark_hits: census::vmm_defrag_watermark_hits(),
            vmm_prewarm_created: census::vmm_prewarm_created(),
            managed_spill_allocs,
            managed_spill_bytes,
            tier1_latency_count: tier1.count,
            tier1_latency_total_ns: tier1.total_ns,
            tier2_latency_count: tier2.count,
            tier2_latency_total_ns: tier2.total_ns,
            pinned_pool_hits,
            pinned_pool_misses,
            host_pin_cache_hits,
            host_pin_cache_registers,
        }
    }
}

/// The critical-section report, raw plus normalized and paired with GPU
/// telemetry.
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
    /// Process-wide GPU allocator, VMM, and pinned-staging telemetry.
    pub gpu: GpuTelemetrySnapshot,
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
    /// Normalize a raw counter set with zeroed GPU telemetry. Pure — no
    /// globals, no allocation.
    pub fn from_raw(raw: CriticalMetricsRaw) -> Self {
        Self::from_raw_with_gpu(raw, GpuTelemetrySnapshot::default())
    }

    /// Normalize a raw counter set paired with an explicit GPU telemetry
    /// snapshot. Pure — no globals, no allocation.
    pub fn from_raw_with_gpu(raw: CriticalMetricsRaw, gpu: GpuTelemetrySnapshot) -> Self {
        let reaped_total = raw
            .tokens_reaped
            .saturating_add(raw.tokens_reaped_at_shutdown);
        Self {
            raw,
            wait_timeout_rate: ratio(raw.waits_timed_out, raw.waits_entered),
            wait_nanos_per_wait: ratio(raw.wait_nanos, raw.waits_entered),
            leak_rate: ratio(reaped_total, raw.tokens_acquired),
            forced_non_moving_per_wait: ratio(raw.forced_non_moving_collections, raw.waits_entered),
            gpu,
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
        writeln!(
            f,
            "[GPU] critical fallback: forced_non_moving_collections={} \
             per_wait={:.4} (collections diverted to the non-moving path \
             because a GPU critical-section wait expired)",
            r.forced_non_moving_collections, self.forced_non_moving_per_wait,
        )?;
        let g = &self.gpu;
        writeln!(
            f,
            "[GPU] vram: used_bytes={} peak_bytes={} pool_hit_rate={:.4} pool_hits={} pool_misses={} pool_parked={} stream_reuses={} gc_vram_triggers={} vmm_remaps={}",
            g.vram_used_bytes,
            g.vram_peak_bytes,
            g.vram_pool_hit_rate,
            g.alloc_pool_hits,
            g.alloc_pool_misses,
            g.alloc_pool_parked,
            g.stream_ordered_reuses,
            g.gc_triggered_by_vram_pressure,
            g.vmm_remaps,
        )?;
        write!(
            f,
            "[GPU] pinned: pool_hits={} pool_misses={} pin_cache_hits={} pin_cache_registers={}",
            g.pinned_pool_hits,
            g.pinned_pool_misses,
            g.host_pin_cache_hits,
            g.host_pin_cache_registers,
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
    /// Set while a collector is running a cycle that MAY relocate. A
    /// [`Relocation::Forbidden`] acquisition blocks while its VM (or any
    /// unscoped collector) is active — see [`Registry::acquire_with`].
    moving_cycle: AtomicBool,
    /// `(global_count, per_vm_counts)` tracking active moving cycles so VM B's
    /// moving GC does not block VM A's `Forbidden` DMA or vice versa.
    moving_cycle_scopes: Mutex<(usize, BTreeMap<u64, usize>)>,
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
            moving_cycle: AtomicBool::new(false),
            moving_cycle_scopes: Mutex::new((0, BTreeMap::new())),
        })
    }

    #[inline]
    fn lock_moving_scopes(&self) -> std::sync::MutexGuard<'_, (usize, BTreeMap<u64, usize>)> {
        self.moving_cycle_scopes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    #[inline]
    fn is_moving_cycle_blocking(&self, owner_vm: u64) -> bool {
        if !self.moving_cycle.load(Ordering::Acquire) {
            return false;
        }
        let scopes = self.lock_moving_scopes();
        scopes.0 > 0
            || (owner_vm == 0 && !scopes.1.is_empty())
            || scopes.1.get(&owner_vm).copied().unwrap_or(0) > 0
    }

    /// The collector is about to run a cycle that may relocate objects.
    pub fn begin_moving_cycle(&self) {
        let _live = self.live();
        let mut scopes = self.lock_moving_scopes();
        scopes.0 += 1;
        self.moving_cycle.store(true, Ordering::Release);
    }

    /// Atomically begin a moving cycle only if no [`Relocation::Forbidden`]
    /// token is currently live under the registry lock.
    pub fn try_begin_moving_cycle(&self) -> bool {
        self.try_begin_moving_cycle_for_vm(None)
    }

    /// VM-scoped variant of [`Self::try_begin_moving_cycle`]: ignores
    /// `Forbidden` tokens attributed strictly to unrelated VMs (`r.owner.vm != 0`).
    pub fn try_begin_moving_cycle_for_vm(&self, scope_vm: Option<u64>) -> bool {
        let live = self.live();
        if live.values().any(|r| {
            r.relocation == Relocation::Forbidden && Self::owner_matches_vm(r.owner.vm, scope_vm)
        }) {
            return false;
        }
        let mut scopes = self.lock_moving_scopes();
        match scope_vm {
            None => scopes.0 += 1,
            Some(vm_id) => *scopes.1.entry(vm_id).or_insert(0) += 1,
        }
        self.moving_cycle.store(true, Ordering::Release);
        true
    }

    /// See [`Registry::begin_moving_cycle`].
    pub fn end_moving_cycle(&self) {
        self.end_moving_cycle_for_vm(None);
    }

    /// VM-scoped variant of [`Self::end_moving_cycle`].
    pub fn end_moving_cycle_for_vm(&self, scope_vm: Option<u64>) {
        let mut scopes = self.lock_moving_scopes();
        match scope_vm {
            None => {
                scopes.0 = scopes.0.saturating_sub(1);
            }
            Some(vm_id) => {
                if let Some(cnt) = scopes.1.get_mut(&vm_id) {
                    if *cnt <= 1 {
                        scopes.1.remove(&vm_id);
                    } else {
                        *cnt -= 1;
                    }
                }
            }
        }
        let active = scopes.0 > 0 || !scopes.1.is_empty();
        self.moving_cycle.store(active, Ordering::Release);
    }

    /// Whether a collector has declared a moving cycle in progress.
    pub fn moving_cycle_in_progress(&self) -> bool {
        self.moving_cycle.load(Ordering::Acquire)
    }

    #[inline]
    fn owner_matches_vm(owner_vm: u64, scope_vm: Option<u64>) -> bool {
        scope_vm.is_none_or(|id| owner_vm == 0 || owner_vm == id)
    }

    /// Tokens outstanding that forbid relocation visible to `scope_vm`.
    pub fn outstanding_relocation_forbidden_for_vm(&self, scope_vm: Option<u64>) -> u32 {
        self.live()
            .values()
            .filter(|r| {
                r.relocation == Relocation::Forbidden
                    && Self::owner_matches_vm(r.owner.vm, scope_vm)
            })
            .count() as u32
    }

    /// Tokens outstanding that forbid relocation. Takes the lock; the
    /// collector's fast path checks [`Registry::outstanding`] first.
    pub fn outstanding_relocation_forbidden(&self) -> u32 {
        self.outstanding_relocation_forbidden_for_vm(None)
    }

    /// The keep-alive addresses token `id` currently declares, in
    /// declaration order, or `None` for a token the registry no longer
    /// holds.
    pub fn keepalive_addrs_of(&self, id: u64) -> Option<Vec<usize>> {
        self.live().get(&id).map(|r| r.keepalive.clone())
    }

    /// Wait, bounded by `budget`, until no outstanding `Forbidden` token
    /// visible to `scope_vm` remains.
    pub fn wait_for_relocation_clearance_for_vm(
        &self,
        scope_vm: Option<u64>,
        budget: Duration,
    ) -> WaitOutcome {
        self.counters.waits_entered.fetch_add(1, Ordering::Relaxed);
        if self.outstanding.load(Ordering::Acquire) == 0
            || self.outstanding_relocation_forbidden_for_vm(scope_vm) == 0
        {
            self.counters.waits_drained.fetch_add(1, Ordering::Relaxed);
            return WaitOutcome::Drained {
                waited: Duration::ZERO,
            };
        }
        let start = Instant::now();
        let mut spins: u32 = 0;
        loop {
            self.reap_expired();
            if self.outstanding_relocation_forbidden_for_vm(scope_vm) == 0 {
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
                self.counters.wait_nanos.fetch_add(
                    waited.as_nanos().min(u64::MAX as u128) as u64,
                    Ordering::Relaxed,
                );
                let live: Vec<TokenFacts> = self
                    .live_tokens()
                    .into_iter()
                    .filter(|f| Self::owner_matches_vm(f.owner.vm, scope_vm))
                    .collect();
                if !live.iter().any(|f| f.relocation == Relocation::Forbidden) {
                    self.counters.waits_drained.fetch_add(1, Ordering::Relaxed);
                    return WaitOutcome::Drained { waited };
                }
                self.counters
                    .waits_timed_out
                    .fetch_add(1, Ordering::Relaxed);
                return self.timed_out_from(waited, live);
            }
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

    /// Wait, bounded by `budget`, until no outstanding token forbids
    /// relocation.
    pub fn wait_for_relocation_clearance(&self, budget: Duration) -> WaitOutcome {
        self.wait_for_relocation_clearance_for_vm(None, budget)
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
    /// `keepalive` are raw `usize` object addresses — not `ObjectRef`, so this
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
        // A holder about to hand the device a heap address must not do so
        // while a collector is sliding objects. See `begin_moving_cycle`.
        // Bounded only by the cycle itself, which is bounded.
        // While `moving_cycle` is set, block `Forbidden` acquisition so DMA
        // cannot pin/touch heap addresses mid-compaction. Re-check under
        // `this.live()` to serialize with `try_begin_moving_cycle()`.
        let mut spins: u32 = 0;
        let id = this.next_id.fetch_add(1, Ordering::Relaxed);
        let revoked = Arc::new(AtomicBool::new(false));
        let owner_vm = owner.vm;
        let mut record = Some(Record {
            owner,
            acquired_at: Instant::now(),
            lease,
            relocation,
            keepalive: keepalive.to_vec(),
            revoked: Arc::clone(&revoked),
        });
        loop {
            if relocation == Relocation::Forbidden {
                while this.is_moving_cycle_blocking(owner_vm) {
                    spins = spins.saturating_add(1);
                    if spins < 64 {
                        std::hint::spin_loop();
                    } else {
                        std::thread::sleep(Duration::from_micros(50));
                    }
                }
            }
            {
                let mut live = this.live();
                if relocation == Relocation::Forbidden && this.is_moving_cycle_blocking(owner_vm) {
                    drop(live);
                    spins = spins.saturating_add(1);
                    if spins < 64 {
                        std::hint::spin_loop();
                    } else {
                        std::thread::sleep(Duration::from_micros(50));
                    }
                    continue;
                }
                if let Some(rec) = record.take() {
                    live.insert(id, rec);
                }
                // Publish the count and mirror under `live` so a concurrent
                // `reap_expired` cannot `mirror_sub` before `mirror_add` runs.
                this.outstanding.store(live.len() as u32, Ordering::Release);
                this.mirror_add();
                break;
            }
        }
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
                self.mirror_sub(1);
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

    /// Tokens visible to `scope_vm` (`None` = all VMs).
    pub fn outstanding_for_vm(&self, scope_vm: Option<u64>) -> u32 {
        let total = self.outstanding.load(Ordering::Acquire);
        if total == 0 || scope_vm.is_none() {
            return total;
        }
        self.live()
            .values()
            .filter(|r| Self::owner_matches_vm(r.owner.vm, scope_vm))
            .count() as u32
    }

    /// Does any outstanding token forbid relocation?
    ///
    /// The collector must consult this **in addition to** the wait
    /// outcome: a token acquired after a `Drained` wait returned, but
    /// before the pause actually begins, still forbids relocation.
    pub fn relocation_forbidden(&self) -> bool {
        self.relocation_forbidden_for_vm(None)
    }

    /// Does any outstanding token visible to `scope_vm` forbid relocation?
    pub fn relocation_forbidden_for_vm(&self, scope_vm: Option<u64>) -> bool {
        self.live().values().any(|r| {
            r.relocation == Relocation::Forbidden && Self::owner_matches_vm(r.owner.vm, scope_vm)
        })
    }

    /// Deduplicated keep-alive addresses declared by every outstanding
    /// token, for the collector to splice in as additional roots.
    ///
    /// Sorted, so the collector's root buffer is deterministic between
    /// cycles and a diff of two collections is readable.
    pub fn outstanding_keepalive_addrs(&self) -> Vec<usize> {
        self.outstanding_keepalive_addrs_for_vm(None)
    }

    /// Keep-alive addresses declared by outstanding tokens visible to `scope_vm`.
    pub fn outstanding_keepalive_addrs_for_vm(&self, scope_vm: Option<u64>) -> Vec<usize> {
        let live = self.live();
        let mut out: Vec<usize> = live
            .values()
            .filter(|r| Self::owner_matches_vm(r.owner.vm, scope_vm))
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
    /// Like any address-keyed side table (the `memory::addr_keyed` contract),
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
                self.mirror_sub(reaped.len() as u32);
            }
        }
        if reaped.is_empty() {
            return reaped;
        }
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
                self.mirror_sub(reaped.len() as u32);
            }
        }
        if reaped.is_empty() {
            return reaped;
        }
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
                self.counters.wait_nanos.fetch_add(
                    waited.as_nanos().min(u64::MAX as u128) as u64,
                    Ordering::Relaxed,
                );
                let live = self.live_tokens();
                if live.is_empty() {
                    self.counters.waits_drained.fetch_add(1, Ordering::Relaxed);
                    return WaitOutcome::Drained { waited };
                }
                self.counters
                    .waits_timed_out
                    .fetch_add(1, Ordering::Relaxed);
                return self.timed_out_from(waited, live);
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
        self.timed_out_from(waited, self.live_tokens())
    }

    fn timed_out_from(&self, waited: Duration, live: Vec<TokenFacts>) -> WaitOutcome {
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
        CriticalSectionReport::from_raw_with_gpu(
            self.metrics_raw(),
            GpuTelemetrySnapshot::capture(),
        )
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
mod tests;
