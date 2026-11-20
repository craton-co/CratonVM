#[allow(unused_imports)]
use super::super::*;

/// Code-cache install and reclamation accounting, as one snapshot.
///
/// Read one relaxed counter at a time, so a snapshot taken during a
/// publication or a drain can be off by that event. Installs and reclaims are
/// both counted on the executable buffer of a PUBLISHED body
/// (`ExecutableBuffer::mark_published` and its `Drop`), so
/// `installed - reclaimed` is exactly the published code still mapped.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct JitCodeReclamationStats {
    /// Bodies published into a dispatch surface.
    pub installed_bodies: u64,
    /// Reserved bytes of those bodies.
    pub installed_bytes: u64,
    /// Emitted machine-code bytes of those bodies.
    pub installed_code_bytes: u64,
    /// Bodies withdrawn from a `JitCache`: superseded, invalidated or flushed.
    pub withdrawn_bodies: u64,
    /// Reserved bytes of those bodies.
    pub withdrawn_bytes: u64,
    /// Withdrawals per [`retire_reason`] code.
    ///
    /// Sums to [`Self::withdrawn_bodies`] exactly — every withdrawal goes
    /// through [`retire_withdrawn_body`], which bumps the total and one bucket
    /// in the same call — subject only to the usual relaxed-snapshot skew of one
    /// event, since the total and the buckets are separate loads.
    pub withdrawn_by_reason: [u64; retire_reason::COUNT],
    /// High-water mark of `installed_bytes - reclaimed_bytes`, i.e. of the
    /// published code mapped at once.
    ///
    /// A LOWER BOUND, and the direction is deliberate. The sample is taken at
    /// each publish from two independent relaxed loads (installed first, then
    /// reclaimed), so a release landing between them makes that sample low by
    /// up to one body; nothing can make it high. An operator reads a high-water
    /// mark as "the cache was at least this full", which a low bias never
    /// contradicts.
    ///
    /// **It can nevertheless come back BELOW `installed_bytes - reclaimed_bytes`
    /// read from this same snapshot**, if the skewed sample was the last one
    /// taken and nothing has been reclaimed since — which is precisely the
    /// "peak below live" reading that
    /// `r10-wrappers-process-report-has-no-reader-and-seven-unfed-fields-20260921-RETIRED-20260922.md`
    /// names as arithmetically impossible in a fed counter. That is why the VM
    /// side does not print this value bare: `code_cache_lifecycle_raw` takes
    /// `max(peak, live)` of the same snapshot, with the reasoning in its own
    /// comment. Do not remove that clamp on the grounds that the counter is now
    /// fed; being fed is what makes the clamp necessary.
    pub peak_live_bytes: u64,
    /// Owners waiting in the retirement queue now (gauge). One body can be
    /// queued by more than one holder.
    pub queued_owners: u64,
    /// Reserved bytes of the queued owners' bodies (gauge).
    pub queued_bytes: u64,
    /// Published bodies whose mapping was released.
    pub reclaimed_bodies: u64,
    /// Reserved bytes released.
    pub reclaimed_bytes: u64,
    /// Machine-code bytes released.
    pub reclaimed_code_bytes: u64,
    /// Drain attempts on a non-empty queue.
    pub drains: u64,
    /// Drains that found every thread outside compiled code.
    pub drains_quiescent: u64,
    /// Drains that released bodies on per-thread evidence while some thread
    /// was still inside compiled code — typically parked there.
    pub drains_by_thread_evidence: u64,
    /// Drains that released nothing.
    pub drains_deferred: u64,
    /// JIT round 22 wave 11, lane runtime (reporting only; the
    /// `code_cache_sweep_phases` failure message reads these): queued bodies a
    /// per-thread drain KEPT because some thread running inside compiled code
    /// had not returned to JIT depth 0 since the body's retirement stamp
    /// (`retired_gen > min_running_gen`). One count per body per drain.
    pub kept_by_running_thread: u64,
    /// Queued bodies kept only because a blocked thread's stack band holds a
    /// return address into them. One count per body per drain.
    pub kept_by_blocked_band: u64,
    /// Drains that released nothing because some JIT execution ran on a thread
    /// with no quiescence record (`UNTRACKED_JIT_EXECUTIONS != 0`).
    pub drains_untracked: u64,
}

/// Snapshot the code-cache install and reclamation accounting.
/// Bodies withdrawn from any JIT cache so far: superseded, invalidated,
/// flushed or swept. Monotonic; one relaxed load.
///
/// The cheap question a dispatch cache asks before scanning itself for
/// withdrawn bodies it still pins -- see
/// `vm::runtime::interpreter::jit_bridge::release_withdrawn_code_owners`.
/// Unchanged means nothing can have been withdrawn since the last scan.
pub fn jit_withdrawn_bodies() -> u64 {
    RECLAMATION
        .withdrawn_bodies
        .load(std::sync::atomic::Ordering::Relaxed)
}

pub fn jit_code_reclamation_stats() -> JitCodeReclamationStats {
    use std::sync::atomic::Ordering::Relaxed;
    let c = &RECLAMATION;
    JitCodeReclamationStats {
        installed_bodies: c.installed_bodies.load(Relaxed),
        installed_bytes: c.installed_bytes.load(Relaxed),
        installed_code_bytes: c.installed_code_bytes.load(Relaxed),
        withdrawn_bodies: c.withdrawn_bodies.load(Relaxed),
        withdrawn_bytes: c.withdrawn_bytes.load(Relaxed),
        withdrawn_by_reason: {
            let mut out = [0u64; retire_reason::COUNT];
            for (slot, o) in c.withdrawn_by_reason.iter().zip(out.iter_mut()) {
                *o = slot.load(Relaxed);
            }
            out
        },
        peak_live_bytes: c.peak_live_bytes.load(Relaxed),
        queued_owners: c.queued_owners.load(Relaxed),
        queued_bytes: c.queued_bytes.load(Relaxed),
        reclaimed_bodies: c.reclaimed_bodies.load(Relaxed),
        reclaimed_bytes: c.reclaimed_bytes.load(Relaxed),
        reclaimed_code_bytes: c.reclaimed_code_bytes.load(Relaxed),
        drains: c.drains.load(Relaxed),
        drains_quiescent: c.drains_quiescent.load(Relaxed),
        drains_by_thread_evidence: c.drains_by_thread_evidence.load(Relaxed),
        drains_deferred: c.drains_deferred.load(Relaxed),
        kept_by_running_thread: c.kept_by_running_thread.load(Relaxed),
        kept_by_blocked_band: c.kept_by_blocked_band.load(Relaxed),
        drains_untracked: c.drains_untracked.load(Relaxed),
    }
}

/// Raise the published-live-bytes high-water mark to the current live figure.
///
/// Called immediately after every `ExecutableBuffer::mark_published()` in this
/// crate — there are exactly two, both in [`JitCache::put`] and
/// [`JitCache::put_osr`] — which is where the sample belongs even though the
/// two `fetch_add`s it reads are inside `mark_published` itself
/// (`jit/src/exec_memory.rs`, a file this change does not own). Calling from the
/// caller is equivalent: `mark_published` is synchronous, so by the time it
/// returns its install accounting is complete, and it is idempotent, so a second
/// call on an already-published buffer re-samples the same live figure into a
/// `fetch_max` that then does nothing.
///
/// **A publish is the only event that can RAISE live bytes**, so sampling here
/// misses no maximum: releases only lower it, and `installed_bytes` moves only
/// under `mark_published`.
///
/// # `saturating_sub`, and the hazard it is for — which is real
///
/// `docs/feature-designs/jit-ir-optimizations.md` flags this and it was
/// checked against the counters rather than taken on trust. The two loads are
/// independent. Let `I(t)`/`R(t)` be installed and reclaimed bytes; the loads
/// happen at `t1 < t2`, and the invariant `R(t) <= I(t)` holds at every instant
/// but says nothing about `R(t2)` against `I(t1)`. A publish of 50 bytes and a
/// release of 60 bytes both landing in that window take `I: 100 -> 150` and
/// `R: 90 -> 150`, so the subtraction is `100 - 150`. In a release build a
/// wrapping `-` yields ~`u64::MAX` and `fetch_max` pins the high-water mark
/// there for the life of the process — strictly worse than not having the
/// counter, because it is not merely wrong but unfalsifiable. `saturating_sub`
/// answers `0` for that window instead, which the next publish corrects.
///
/// The LOAD ORDER carries the other half and is not interchangeable: installed
/// first, reclaimed second, so the error is a value too LOW. Reading them the
/// other way round cannot underflow at all but biases the peak HIGH, and a
/// high-water mark that overstates itself is the one error direction an operator
/// cannot detect from the report.
///
/// Two relaxed loads and one relaxed `fetch_max` per published body.
#[inline]
pub(crate) fn note_jit_published_live_bytes_peak() {
    use std::sync::atomic::Ordering::Relaxed;
    let c = &RECLAMATION;
    let installed = c.installed_bytes.load(Relaxed);
    let reclaimed = c.reclaimed_bytes.load(Relaxed);
    c.peak_live_bytes
        .fetch_max(installed.saturating_sub(reclaimed), Relaxed);
}

/// The per-method publication census's lock.
///
/// A leaf: nothing is acquired while it is held, and the only two writers
/// ([`JitCache::put`] and [`JitCache::put_osr`]) already hold their cache's
/// `mutation`, which is never taken from here. See [`CompilationCensusState`].
pub(crate) fn compilation_census() -> &'static parking_lot::Mutex<CompilationCensusState> {
    RECLAMATION.compilation_census.get_or_init(|| {
        parking_lot::Mutex::new(CompilationCensusState {
            versions: FxHashMap::default(),
            truncated: false,
        })
    })
}

/// Record one published body for the method whose key hash is `h`.
///
/// Called from [`JitCache::put`] and [`JitCache::put_osr`], immediately after
/// `mark_published` reports that it performed the install accounting — and
/// **only** when it reports that, so this census counts exactly the population
/// `RECLAMATION.installed_bodies` counts. That equality is what makes
/// `versions_per_method = installs / methods_compiled` a number at or above
/// `1.0` by construction rather than by hope, which is the reading the whole
/// three-wave story behind
/// `docs/feature-designs/jit-ir-optimizations.md`
/// exists to make trustworthy.
///
/// One lock, one hash lookup and one increment per published body, on a path
/// that has already cloned a whole shard map.
pub(crate) fn note_jit_published_method_version(h: u64) {
    let mut st = compilation_census().lock();
    match st.versions.get_mut(&h) {
        Some(count) => *count = count.saturating_add(1),
        None => {
            if st.versions.len() >= CENSUS_METHOD_LIMIT {
                // Latch and stop. See `CompilationCensusState`: a truncated
                // table is reported as absent, never as a smaller number.
                st.truncated = true;
            } else {
                st.versions.insert(h, 1);
            }
        }
    }
}

/// The compilation census: what the process report's compilation group needs.
///
/// Every field is derived from one table at read time, so the three cannot
/// disagree with each other. See [`CompilationCensusState`] for what a version
/// is and why it is counted per method rather than per pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JitCompilationCensus {
    /// Distinct methods with at least one published body.
    ///
    /// The denominator of `versions_per_method`. Never larger than
    /// `JitCodeReclamationStats::installed_bodies`, because every entry here was
    /// created by a publication that bumped that counter.
    pub methods_compiled: u64,
    /// Publications that were not a method's first.
    ///
    /// `installed_bodies - methods_compiled`, computed from the table rather
    /// than from that subtraction so the identity is a check and not a
    /// definition.
    pub recompilations: u64,
    /// The highest version any one method reached.
    ///
    /// `1` when every compiled method was compiled once; `0` only when nothing
    /// has been published at all.
    pub max_versions_for_one_method: u64,
}

/// The per-method compilation census, or `None` when it is no longer exact.
///
/// `None` means the table hit [`CENSUS_METHOD_LIMIT`] and stopped tracking new
/// methods; the caller must keep the group SUPPRESSED rather than print the
/// figures, because a `methods_compiled` pinned at the limit while `installs`
/// keeps climbing reads exactly like the compilation thrash the ratio exists to
/// detect. See [`CompilationCensusState`].
///
/// Takes the census lock for one walk of the table. Called once per process
/// report, never on a hot path. It is a leaf lock; see [`compilation_census`].
pub fn jit_compilation_census() -> Option<JitCompilationCensus> {
    let st = compilation_census().lock();
    if st.truncated {
        return None;
    }
    let mut total: u64 = 0;
    let mut max: u64 = 0;
    for v in st.versions.values() {
        total = total.saturating_add(*v as u64);
        max = max.max(*v as u64);
    }
    let methods = st.versions.len() as u64;
    Some(JitCompilationCensus {
        methods_compiled: methods,
        recompilations: total.saturating_sub(methods),
        max_versions_for_one_method: max,
    })
}

/// Age of the oldest owner in the retirement queue, in RETIREMENT GENERATIONS,
/// or `None` when the queue is empty.
///
/// The queue already knew this and had no way to say it: every
/// [`DeferredOwner`] carries the `retired_gen` stamped after its raw targets
/// were unpublished, and [`JIT_RETIRE_GENERATION`] is the current stamp, so the
/// oldest entry's age is `current - min(retired_gen)`. Written for
/// `vm::jit::code_cache_lifecycle`, whose process report printed
/// `age=n/a (no producer: the JIT retirement queue exposes no per-body deferral
/// age)` in the RETAINED branch — the one branch where a climbing age is the
/// whole signal (§1.4 of that module: a wedged entry chain shows up as bytes
/// that never fall and an age that climbs).
///
/// # The unit is GENERATIONS, not sweeps, and the two must not be conflated
///
/// `CodeCacheLifecycleRaw::max_deferral_sweeps` is "most SWEEPS survived by any
/// one queued body" and the model counts exactly that. A retirement generation
/// advances once per WITHDRAWAL ([`bump_retire_generation`]), not once per
/// drain, so a queue holding one wedged body while a hundred other methods are
/// recompiled reports an age of ~100 having survived possibly one drain. Both
/// numbers answer "has this been stuck a long time?" and neither is wrong, but
/// they are not interchangeable, and pouring generations into a field named
/// `_sweeps` is the silent relabelling the `const` numbering assertions in
/// `code_cache_lifecycle.rs` exist to prevent one level down. So the VM side
/// carries this in its own field, `oldest_deferral_generations`, and
/// `max_deferral_sweeps` stays model-only and stays suppressed on the process
/// path.
///
/// # Locking
///
/// Takes the retirement-queue lock and nothing else, **through
/// [`lock_retire_queue`] and not through `deferred_jit_owners().lock()`
/// directly**. That is not a style preference and getting it wrong would not be
/// a subtle bug: `no_raw_acquisition_bypasses_the_ordering_check` in this file's
/// test module counts raw acquisitions of the two locks over `lib.rs`'s code
/// lines and asserts there is EXACTLY ONE of each — the ones inside the two
/// helpers. A second raw acquisition compiles, works, and silently falls outside
/// the ordering rule, which is precisely why that test exists.
///
/// The rule it enforces: the lock order [`drain_deferred_jit_owners`] establishes
/// is queue-before-[`jit_threads`]. This function takes only the queue, so it
/// cannot invert anything by itself — but it must not be called while
/// `jit_threads()` is already held, and going through the helper is what makes
/// that a reported panic rather than a deadlock nobody can attribute.
///
/// Called once per process report, never on a hot path.
///
/// `None` means the queue was empty at the moment of the call, which is a
/// different statement from an age of `0` (something was queued by the most
/// recent withdrawal). The caller must keep them apart; the VM side's field is
/// an `Option` for exactly that.
#[must_use]
pub fn jit_oldest_retirement_age() -> Option<u64> {
    let queue = lock_retire_queue();
    let oldest = queue.iter().map(|o| o.retired_gen).min()?;
    // `saturating_sub` is belt-and-braces rather than a known hazard:
    // `retired_gen` values are all stamps this counter already issued, so
    // `current >= oldest` holds. It costs nothing and an observability call must
    // never be the thing that panics a shutdown report.
    Some(
        JIT_RETIRE_GENERATION
            .load(std::sync::atomic::Ordering::Acquire)
            .saturating_sub(oldest),
    )
}

/// Owners waiting in the retirement queue. One relaxed load.
#[inline]
pub fn jit_retirement_queue_len() -> usize {
    RECLAMATION
        .queued_owners
        .load(std::sync::atomic::Ordering::Relaxed) as usize
}

/// Release every retired body no thread can still execute or return into, and
/// report the accounting afterwards.
pub fn reclaim_retired_jit_code() -> JitCodeReclamationStats {
    drain_deferred_jit_owners();
    jit_code_reclamation_stats()
}

/// Why a queued body may be released while some thread is still inside
/// compiled code. Collected with the retirement queue lock held, so every
/// reading in it postdates the unpublication of every queued body.
pub(crate) struct ThreadQuiescenceEvidence {
    /// Every thread inside compiled code and not blocked has returned to JIT
    /// depth 0 after this retirement generation was reached.
    pub(crate) min_running_gen: u64,
    /// The INLINE-CACHE grace's bound over the same running threads: each
    /// vouches up to the later of its `quiescent_gen` and its
    /// `ic_quiescent_gen` (a miss-handler entry, [`jit_ic_quiescent_point`]).
    /// Never below `min_running_gen`. Read only by the grace notes
    /// (`note_graced_generation`), NEVER by [`Self::permits`]: a thread at a
    /// miss handler is outside every probe but still inside compiled bodies
    /// (round 14 wave 2, lane mic, M8-1).
    pub(crate) min_running_ic_gen: u64,
    /// For each thread blocked inside compiled code: the generation it last
    /// returned to depth 0 at, and the sorted executable-buffer starts its
    /// stack can return into.
    pub(crate) blocked: Vec<(u64, Vec<usize>)>,
}

impl ThreadQuiescenceEvidence {
    /// Whether no thread can still be executing, or return into, `body`.
    ///
    /// A running thread is safe once it has been at depth 0 after the body's
    /// retirement stamp. A blocked thread is safe on the same condition, or
    /// when no word on its stack points into the body: it cannot run compiled
    /// code without first leaving the blocked window, and when it does it can
    /// only return through the addresses its stack holds or enter through a
    /// dispatch surface — and the body was unpublished from every surface
    /// before it was queued.
    pub(crate) fn permits(&self, body: &DeferredOwner) -> bool {
        self.permits_stamp(body.retired_gen, body.buffer_start)
    }

    /// [`Self::permits`] over the two fields it actually reads.
    ///
    /// Split out so the predicate can be tested at all. A [`DeferredOwner`]
    /// owns an `Arc<CompiledMethod>`, which cannot be synthesised without
    /// running a real compile — and an end-to-end test of a *release* decision
    /// proves little when it passes, because "nothing was freed" is also what a
    /// permanently-false predicate looks like.
    ///
    /// Note what the search is over. `buffers` holds buffer STARTS, not ranges:
    /// the range containment lives in [`collect_code_buffers_in_band`], which
    /// maps each stack word to its owning buffer's start. By the time a list
    /// reaches here every entry is an exact identity and no length is in scope,
    /// so there is no `entry + len - 1` case in this function.
    pub(crate) fn permits_stamp(&self, retired_gen: u64, buffer_start: usize) -> bool {
        retired_gen <= self.min_running_gen
            && self.blocked.iter().all(|(quiescent_gen, buffers)| {
                retired_gen <= *quiescent_gen || buffers.binary_search(&buffer_start).is_err()
            })
    }
}

/// Per-thread quiescence evidence, or `None` when some JIT execution runs on a
/// thread whose record is gone and so cannot be vouched for.
///
/// Lock order: the retirement queue (held by the caller), then [`JIT_THREADS`],
/// then one thread's summary at a time.
pub(crate) fn collect_thread_quiescence_evidence() -> Option<ThreadQuiescenceEvidence> {
    use std::sync::atomic::Ordering;
    let untracked = UNTRACKED_JIT_EXECUTIONS.load(Ordering::Acquire);
    // Clone the thread list only when the untracked count leaves any evidence
    // to build. `classify_thread_quiescence` re-tests `untracked`, so the whole
    // decision — including the fail-closed arm — lives in one place a test can
    // reach; this is a lock-avoidance short circuit, not a second policy.
    let threads: Vec<Arc<JitThreadQuiescence>> = if untracked == 0 {
        lock_jit_threads().clone()
    } else {
        Vec::new()
    };
    classify_thread_quiescence(untracked, &threads)
}

/// [`collect_thread_quiescence_evidence`]'s whole decision, over an explicit
/// untracked count and thread list rather than the two globals.
///
/// The fail-closed arm is the one worth stating twice, because it is the one a
/// test can get wrong in the safe-looking direction. `untracked != 0` means
/// some JIT execution is running on a thread whose quiescence record has
/// already been torn down, so the per-thread proof is INCOMPLETE and no body
/// may be released on its strength — `None`, not an empty-but-permissive
/// `ThreadQuiescenceEvidence`. The two are opposites: an evidence value with no
/// blocked threads and `min_running_gen == u64::MAX` permits EVERYTHING.
///
/// The same inversion is available one level down. A summary with
/// `valid == false` is not evidence of quiescence — it is a thread that may run
/// compiled code again — so it is folded into `min_running_gen` as a RUNNING
/// thread and never pushed onto `blocked`.
pub(crate) fn classify_thread_quiescence(
    untracked: usize,
    threads: &[Arc<JitThreadQuiescence>],
) -> Option<ThreadQuiescenceEvidence> {
    use std::sync::atomic::Ordering;
    if untracked != 0 {
        return None;
    }
    let mut evidence = ThreadQuiescenceEvidence {
        min_running_gen: u64::MAX,
        min_running_ic_gen: u64::MAX,
        blocked: Vec::new(),
    };
    let mut pin_parked = false;
    for thread in threads {
        // Depth 0 now: outside compiled code at an instant after every queued
        // unpublication, so this thread holds none of the queued bodies.
        if thread.depth.load(Ordering::Acquire) == 0 {
            continue;
        }
        let quiescent_gen = thread.quiescent_gen.load(Ordering::Acquire);
        {
            // Holding this lock while `valid` reads true proves the thread is
            // still blocked: it clears `valid` under this lock before it can
            // run compiled code again.
            let mut summary = thread.blocked.lock();
            // nio21 wave 17 L3: the lazy summary's band is scanned here, under
            // the lock the thread must take before it can run compiled code
            // again (`PendingBlockedBand`). A band too large leaves it invalid:
            // the thread then counts as running, below.
            if summary.valid {
                summary.resolve_pending();
            }
            if summary.valid {
                // `ThreadQuiescenceEvidence::permits_stamp` answers this list
                // with `binary_search`, which is only a membership test on a
                // SORTED list; on an unsorted one it silently answers "absent"
                // and the drain frees a body the thread can return into. The
                // producer (`collect_code_buffers_in_band`) sorts and dedups,
                // and this is where a future second producer that forgot to
                // would be caught.
                debug_assert!(
                    summary.buffers.windows(2).all(|pair| pair[0] < pair[1]),
                    "a blocked stack summary must be sorted and deduplicated; \
                     `permits_stamp` binary-searches it"
                );
                evidence
                    .blocked
                    .push((quiescent_gen, summary.buffers.clone()));
                continue;
            }
        }
        if quiescent_gen < evidence.min_running_gen {
            pin_parked = thread.blocked_nesting.load(Ordering::Acquire) != 0;
        }
        evidence.min_running_gen = evidence.min_running_gen.min(quiescent_gen);
        // M8-1: both stamps are "outside every inline-cache window at an
        // instant after this generation", so the later one bounds the grace.
        let ic_quiescent_gen = thread.ic_quiescent_gen.load(Ordering::Acquire);
        evidence.min_running_ic_gen = evidence
            .min_running_ic_gen
            .min(quiescent_gen.max(ic_quiescent_gen));
    }
    LAST_PIN_PARKED.store(pin_parked, Ordering::Relaxed);
    Some(evidence)
}

/// Smallest `retired_gen` in the retirement queue (`u64::MAX` when empty). Written only under the
/// queue lock, so a drain can test "is any body old enough to release" without walking the queue.
pub(crate) static QUEUE_MIN_GEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(u64::MAX);

/// Reporting only: whether the thread that set `min_running_gen` in the last evidence pass sat in
/// a blocked window whose stack summary was invalid (a parked thread counted as running).
pub(crate) static LAST_PIN_PARKED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// Reporting only: futile-drain pins by a parked-invalid thread vs a genuinely running one.
pub static PINS_BY_PARKED_THREAD: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub static PINS_BY_RUNNING_THREAD: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Republish the queue gauges. Caller holds the queue lock.
pub(crate) fn publish_queue_gauges(queue: &[DeferredOwner]) {
    use std::sync::atomic::Ordering::Relaxed;
    RECLAMATION.queued_owners.store(queue.len() as u64, Relaxed);
    RECLAMATION
        .queued_bytes
        .store(queue.iter().map(|body| body.bytes).sum(), Relaxed);
}

/// Release every queued owner no thread can still execute or return into.
///
/// Two proofs, tried in order. The process-wide one: every thread was outside
/// compiled code at some instant after the queue lock was taken, which frees
/// everything. The per-thread one, used while some thread is still in compiled
/// code: see [`ThreadQuiescenceEvidence::permits`]. It is what lets a thread
/// parked inside compiled code stop holding every retired body in the process.
pub(crate) fn drain_deferred_jit_owners() {
    use std::sync::atomic::Ordering;
    if jit_leak_code_enabled() {
        return;
    }
    // Whether `released` was proven by per-thread evidence (each body against
    // its own stamp and buffer) rather than by the process-wide zero.
    let mut by_thread_evidence = false;
    let released: Vec<DeferredOwner> = {
        // Take the lock BEFORE any quiescence observation, and hold it across
        // them. Observing first is a use-after-free of executable memory: this
        // thread can observe quiescence at t0, be descheduled while another
        // thread enters JIT at t1 and a third unpublishes-and-queues a body at
        // t2 > t1, then resume and free that body on the strength of a t0
        // witness that predates its unpublication. Queueing also takes this
        // lock, so holding it guarantees every witness postdates every queued
        // body's unpublication.
        let mut queue = lock_retire_queue();
        if queue.is_empty() {
            return;
        }
        RECLAMATION.drains.fetch_add(1, Ordering::Relaxed);
        let observed = JIT_RETIRE_GENERATION.load(Ordering::Acquire);
        let released = if ACTIVE_JIT_EXECUTIONS.is_zero() {
            note_graced_generation(observed);
            note_code_graced_generation(observed);
            RECLAMATION.drains_quiescent.fetch_add(1, Ordering::Relaxed);
            QUEUE_MIN_GEN.store(u64::MAX, Ordering::Relaxed);
            std::mem::take(&mut *queue)
        } else if let Some(evidence) = collect_thread_quiescence_evidence() {
            // A blocked thread cannot be between an inline-cache compare and
            // its entry load, so only running threads bound the grace of a
            // retired way (each up to its later stamp, M8-1; the release
            // decision below keeps `min_running_gen`).
            note_graced_generation(observed.min(evidence.min_running_ic_gen));
            // RV3-2: the code grace, from depth-0 returns only.
            note_code_graced_generation(observed.min(evidence.min_running_gen));
            // A futile sweep (nothing permitted: the common case while one
            // thread stays pinned in compiled code) must not move or
            // reallocate the whole queue; test first, partition only to release.
            // With no blocked-stack evidence a body is permitted only when its stamp is
            // <= `min_running_gen`, so a queue whose oldest stamp is newer is futile in O(1).
            let futile_fast = evidence.blocked.is_empty()
                && QUEUE_MIN_GEN.load(Ordering::Relaxed) > evidence.min_running_gen;
            let released = if futile_fast {
                Vec::new()
            } else if queue.iter().any(|body| evidence.permits(body)) {
                let (released, kept): (Vec<DeferredOwner>, Vec<DeferredOwner>) =
                    std::mem::take(&mut *queue)
                        .into_iter()
                        .partition(|body| evidence.permits(body));
                *queue = kept;
                QUEUE_MIN_GEN.store(
                    queue.iter().map(|b| b.retired_gen).min().unwrap_or(u64::MAX),
                    Ordering::Relaxed,
                );
                released
            } else {
                Vec::new()
            };
            // Reporting only (wave 11): why each kept body was kept.
            // Counted locally, two atomic adds per drain: a futile drain walks the
            // whole queue, and one shared-line RMW per body was billions of them.
            let by_running = if futile_fast {
                queue.len() as u64
            } else {
                queue
                    .iter()
                    .filter(|body| body.retired_gen > evidence.min_running_gen)
                    .count() as u64
            };
            RECLAMATION.kept_by_running_thread.fetch_add(by_running, Ordering::Relaxed);
            if by_running != 0 {
                let pin = if LAST_PIN_PARKED.load(Ordering::Relaxed) {
                    &PINS_BY_PARKED_THREAD
                } else {
                    &PINS_BY_RUNNING_THREAD
                };
                pin.fetch_add(1, Ordering::Relaxed);
            }
            RECLAMATION
                .kept_by_blocked_band
                .fetch_add(queue.len() as u64 - by_running, Ordering::Relaxed);
            if !released.is_empty() {
                RECLAMATION
                    .drains_by_thread_evidence
                    .fetch_add(1, Ordering::Relaxed);
                by_thread_evidence = true;
            }
            released
        } else {
            RECLAMATION.drains_untracked.fetch_add(1, Ordering::Relaxed);
            Vec::new()
        };
        if released.is_empty() {
            RECLAMATION.drains_deferred.fetch_add(1, Ordering::Relaxed);
            RECLAMATION.futile_drain_streak.fetch_add(1, Ordering::Relaxed);
        } else {
            RECLAMATION.futile_drain_streak.store(0, Ordering::Relaxed);
            // An unchanged queue has unchanged gauges.
            publish_queue_gauges(&queue);
        }
        released
    };
    if released.is_empty() {
        return;
    }
    // Outside the queue lock: dropping the last owner is where the executable
    // mapping goes back to the OS.
    if by_thread_evidence {
        // Round 14 wave 1 (lane codecache, `gcd-d10v`): the evidence covered
        // each body's OWN buffer. A published body this drop releases last (a
        // callee root, an inline-cache or cell owner) was never checked against
        // the blocked threads' stack summaries, so it is re-queued under a
        // fresh stamp (`ExecutableBuffer::drop`, `reclaim_is_cascaded`) rather
        // than unmapped on this body's proof.
        for body in released {
            let _authorised = AuthorisedReclaim::enter_for(body.buffer_start);
            drop(body);
        }
        return;
    }
    let _authorised = AuthorisedReclaim::enter();
    drop(released);
}

/// Whether a depth-0 pump tick should run a per-thread-evidence drain now.
///
/// While one thread stays pinned in compiled code every drain is futile, yet
/// each walks the thread registry and the queue (Spring AOT javac: 19583
/// sweeps, 94% futile). After 8 futile drains in a row the pump runs one tick
/// in 2, 4 ... 64; any release resets it. The enqueue and cap paths still drain
/// unpaced, so a new retirement or a full cache is never delayed by this.
pub(crate) fn pump_drain_is_due() -> bool {
    use std::sync::atomic::Ordering::Relaxed;
    let streak = RECLAMATION.futile_drain_streak.load(Relaxed);
    let skipped = RECLAMATION.pump_ticks_skipped.load(Relaxed);
    let (due, skipped) = pump_pace(streak, skipped);
    RECLAMATION.pump_ticks_skipped.store(skipped, Relaxed);
    due
}

/// [`pump_drain_is_due`] over explicit state: `(due, skipped ticks afterwards)`.
pub(crate) fn pump_pace(streak: u64, skipped: u64) -> (bool, u64) {
    if streak < 8 {
        return (true, 0);
    }
    let every = 1u64 << (streak - 8).min(6);
    if skipped + 1 >= every {
        (true, 0)
    } else {
        (false, skipped + 1)
    }
}


pub(crate) fn defer_jit_owner(owner: Option<Arc<CompiledMethod>>) {
    let Some(owner) = owner else {
        return;
    };
    let buffer_start = owner.entry as usize;
    let bytes = owner._buffer.capacity() as u64;
    enqueue_retired(RetiredCode::Owner(owner), buffer_start, bytes);
}

/// Hand the mapping of a published body whose LAST OWNER was dropped outside
/// the retirement queue to that queue, instead of unmapping it on the spot.
///
/// Called only from `ExecutableBuffer::drop`, for a published buffer released
/// without `AuthorisedReclaim`. The buffer is still registered in the code-region
/// table, so a blocked thread's stack summary still recognises it, and it is
/// still mapped with its original bytes, so a thread executing inside it keeps
/// executing the code it entered. It is unmapped when the queue's proof — the
/// same one every other retired body waits for — says no thread can be inside.
///
/// Why the queue's proof is sound for an orphan: its owner is gone, so no cache,
/// dispatch memo or baked caller (`_direct_callee_roots`) can hand its entry to
/// a thread any more — it is exactly as unpublished as a withdrawn body is when
/// `defer_jit_owner` sees it. The one path the owner count could not see is a
/// thread already INSIDE it, and that is precisely what the proof covers.
pub(crate) fn defer_orphaned_mapping(buffer: crate::exec_memory::ExecutableBuffer) {
    // Counted by the caller, as `published_code_free_audit().1`.
    let buffer_start = buffer.ptr as usize;
    let bytes = buffer.capacity as u64;
    enqueue_retired(RetiredCode::Mapping(buffer), buffer_start, bytes);
}

thread_local! {
    /// The owners an open [`RetireBatch`] on this thread has collected; `None`
    /// outside one.
    pub(crate) static RETIRE_BATCH: std::cell::RefCell<Option<Vec<(RetiredCode, usize, u64)>>> =
        const { std::cell::RefCell::new(None) };
}

/// One retirement-queue trip for every owner a bulk invalidation retires on
/// this thread, whichever door it leaves by.
///
/// [`enqueue_retired_batch`] already makes one trip per CALL, but a flush
/// retires most bodies through doors that each make their own call: the shard
/// maps' counted [`RetainedCode`] handles, every body's inline caches
/// (`clear_cached_targets`), the cycle and retire cells. While some thread is
/// inside compiled code -- the thread calling `retransformClasses` from a
/// compiled caller is -- no drain can release anything, so each of those calls
/// partitioned the whole growing queue: quadratic in the size of the cache.
/// The cache Hibernate's `DefaultCatalogAndSchemaTest` leaves behind made
/// Mockito's first `retransformClasses` (a whole-cache flush, since it
/// retransforms `java/lang/Object`) stop the VM for ~120 s
/// (`docs/feature-designs/jit-ir-optimizations.md`).
///
/// Sound for the reason the batch form is: the scope's single stamp is taken
/// when the outermost scope closes, after every member's unpublication, which
/// is all a stamp has to postdate; and the members stay owned meanwhile, so
/// nothing is released early. A nested scope joins the open one.
pub(crate) struct RetireBatch {
    pub(crate) outermost: bool,
}

impl RetireBatch {
    pub(crate) fn open() -> Self {
        let outermost = RETIRE_BATCH
            .try_with(|pending| {
                let mut pending = pending.borrow_mut();
                if pending.is_some() {
                    return false;
                }
                *pending = Some(Vec::new());
                true
            })
            .unwrap_or(false);
        Self { outermost }
    }
}

impl Drop for RetireBatch {
    fn drop(&mut self) {
        if !self.outermost {
            return;
        }
        // Closed before the trip: dropping a released owner can retire another
        // (`ExecutableBuffer::drop`), which must then go to the queue itself.
        let batch = RETIRE_BATCH
            .try_with(|pending| pending.borrow_mut().take())
            .ok()
            .flatten()
            .unwrap_or_default();
        enqueue_retired_batch(batch);
    }
}

/// The shared half of [`defer_jit_owner`] and [`defer_orphaned_mapping`]:
/// release at once under a process-wide quiescence proof, or queue under a
/// fresh retirement generation and let the drain decide.
pub(crate) fn enqueue_retired(owner: RetiredCode, buffer_start: usize, bytes: u64) {
    enqueue_retired_batch(vec![(owner, buffer_start, bytes)]);
}

/// [`enqueue_retired`] for several owners at once: `(owner, buffer_start,
/// bytes)` per entry, EVERY one of which the caller has already unpublished.
///
/// One quiescence observation, one queue insertion and one drain cover the
/// whole batch. Retiring an invalidation's bodies one at a time ran the drain
/// once per body, and whenever some thread is inside compiled code (the
/// ordinary state of a VM a redefine or an unload lands in) each drain collects
/// per-thread evidence and partitions the WHOLE queue — O(n^2) in the size of
/// the invalidation, all of it on the invalidating thread. See
/// [`retire_withdrawn_bodies`].
///
/// One retirement stamp for the batch is exactly as sound as one per member:
/// the stamp only has to postdate each member's unpublication, and all of them
/// precede this call. It is also no less conservative — a thread's witness has
/// to reach the single stamp, which is no later than the first of the stamps a
/// one-at-a-time loop would have issued.
pub(crate) fn enqueue_retired_batch(batch: Vec<(RetiredCode, usize, u64)>) {
    use std::sync::atomic::Ordering;
    if batch.is_empty() {
        return;
    }
    // Inside a `RetireBatch` scope the owners wait for the scope's single trip.
    let mut batch = Some(batch);
    let _ = RETIRE_BATCH.try_with(|pending| {
        if let Some(pending) = pending.borrow_mut().as_mut() {
            pending.extend(batch.take().into_iter().flatten());
        }
    });
    let Some(batch) = batch else {
        return;
    };
    let leak = jit_leak_code_enabled();
    let observed = JIT_RETIRE_GENERATION.load(Ordering::Acquire);
    if !leak && ACTIVE_JIT_EXECUTIONS.is_zero() {
        // The caller has already unpublished these owners' raw targets, so a
        // walk that finds every thread outside compiled code proves none can
        // reach them.
        note_graced_generation(observed);
        note_code_graced_generation(observed);
        let _authorised = AuthorisedReclaim::enter();
        drop(batch);
        return;
    }
    // Stamped after the caller's unpublication: a thread that returns to
    // depth 0 after this point cannot hold any member on its stack.
    let retired_gen = bump_retire_generation();
    {
        let mut queue = lock_retire_queue();
        QUEUE_MIN_GEN.fetch_min(retired_gen, std::sync::atomic::Ordering::Relaxed);
        queue.extend(
            batch
                .into_iter()
                .map(|(owner, buffer_start, bytes)| DeferredOwner {
                    retired_gen,
                    buffer_start,
                    bytes,
                    owner,
                }),
        );
        publish_queue_gauges(&queue);
    }
    // Close the race where the final execution leaves between the first load
    // and queue publication, and let per-thread evidence release it at once
    // when the only threads still in compiled code are parked elsewhere.
    drain_deferred_jit_owners();
}

/// Release the callee pins [`JitCache::prepare_for_publication`] took for a
/// body that is then NOT published (a refused rebound or retired callee, or an
/// artifact the strict-roots rule refuses as a whole).
///
/// A pin is an owning `Arc` on a PUBLISHED body, and the one taken on a retired
/// callee is exactly the kind that can be that body's last owner — the
/// retirement queue may have drained its own reference while the pin was held.
/// Dropping it bare would then return the mapping outside any authorised
/// reclamation: harmless in itself (the callee was unpublished before its
/// queue entry was released, so nothing can be inside it), but it is counted by
/// `published_code_free_audit().1`, the invariant `code-cache-lifecycle.md`
/// says must stay zero, and a false positive there costs a day of chasing a
/// use-after-free that is not one. [`RetainedCode`]'s drop is the existing
/// "plain drop if not last, queue if last" rule; reuse it.
pub(crate) fn release_unpublished_pins(pins: Vec<Arc<CompiledMethod>>) {
    for pin in pins {
        drop(RetainedCode::new(pin));
    }
}

/// The closing half of the lambda-adapter invalidation protocol (round 9
/// wave 2; `lambda-adapter-built-for-retiring-impl-20260918.md`).
///
/// Called by `JitCache::invalidate_matching_collecting` AFTER it has marked
/// every doomed body `retired` and BEFORE it clears inline caches. Any thunk
/// that still jumps into a member of `remove_entries` was built after the
/// first sweep; it is forgotten (marked retired, handed to the retirement
/// queue) and its entry added to `remove_entries`, so the inline-cache pass
/// that follows clears every slot it was installed into.
///
/// Sound together with `lambda_adapter_entry`'s re-check under the adapter
/// map lock: if this sweep takes the lock first, the (SeqCst) mark is already
/// visible to the insert's later re-check, which withdraws the thunk itself;
/// if the insert takes it first, this sweep runs after and finds the thunk.
/// Thunks jump only into impls, never into other thunks, so one pass is a
/// fixed point.
pub(crate) fn withdraw_late_lambda_adapters(
    adapters: &crate::lambda_adapter::LambdaAdapterMap,
    remove_entries: &mut std::collections::HashSet<usize>,
) {
    let late: Vec<usize> = crate::lambda_adapter::adapters_reaching(adapters, remove_entries);
    if late.is_empty() {
        return;
    }
    LATE_LAMBDA_ADAPTERS_WITHDRAWN
        .fetch_add(late.len() as u64, std::sync::atomic::Ordering::Relaxed);
    let late_set: std::collections::HashSet<usize> = late.iter().copied().collect();
    crate::lambda_adapter::forget_adapters(adapters, &late_set);
    remove_entries.extend(late);
}

/// Thunks the second, post-mark adapter sweep had to withdraw — each one a
/// thunk that would otherwise have kept an inline cache dispatching into a
/// withdrawn body. Diagnostic; expected to stay at or near zero.
pub(crate) static LATE_LAMBDA_ADAPTERS_WITHDRAWN: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// See [`LATE_LAMBDA_ADAPTERS_WITHDRAWN`].
pub fn late_lambda_adapters_withdrawn() -> u64 {
    LATE_LAMBDA_ADAPTERS_WITHDRAWN.load(std::sync::atomic::Ordering::Relaxed)
}

/// A body leaving a [`JitCache`] — superseded, invalidated or flushed. Counted
/// as a withdrawal, then handed to the retirement queue like any other owner.
///
/// `reason` is a [`retire_reason`] code and is REQUIRED rather than defaulted,
/// which is the whole point of the parameter: every caller of this function
/// already knows why it is withdrawing, and until round 10 wave 8 none of them
/// said, so `code_cache_lifecycle_report()`'s "retirement by reason" line — the
/// one line that tells cache-pressure eviction from class unloading — had no
/// producer and was omitted entirely, which reads as "nothing was retired"
/// beside a `retired=` that says otherwise. See [`retire_reason`] for each
/// caller's code and for the two codes that deliberately have no site.
///
/// An out-of-range code still counts in `withdrawn_bodies` and is dropped from
/// the histogram rather than panicking, exactly as
/// [`note_jit_code_alloc_failure`] does and for the same reason: this is
/// observability on a path that retires live code, where a panic would be a
/// leak at best.
pub(crate) fn retire_withdrawn_body(body: Option<Arc<CompiledMethod>>, reason: usize) {
    if let Some(cm) = &body {
        use std::sync::atomic::Ordering::Relaxed;
        RECLAMATION.withdrawn_bodies.fetch_add(1, Relaxed);
        RECLAMATION
            .withdrawn_bytes
            .fetch_add(cm._buffer.capacity() as u64, Relaxed);
        if let Some(slot) = RECLAMATION.withdrawn_by_reason.get(reason) {
            slot.fetch_add(1, Relaxed);
        }
    }
    defer_jit_owner(body);
}

/// [`retire_withdrawn_body`] for every body one invalidation withdrew, with the
/// same per-body accounting and ONE trip through the retirement queue — see
/// [`enqueue_retired_batch`] for why one trip is sound and why a trip per body
/// was quadratic. Every body must already be unpublished, exactly as for the
/// single form.
pub(crate) fn retire_withdrawn_bodies(bodies: Vec<Arc<CompiledMethod>>, reason: usize) {
    use std::sync::atomic::Ordering::Relaxed;
    let batch: Vec<(RetiredCode, usize, u64)> = bodies
        .into_iter()
        .map(|cm| {
            let bytes = cm._buffer.capacity() as u64;
            RECLAMATION.withdrawn_bodies.fetch_add(1, Relaxed);
            RECLAMATION.withdrawn_bytes.fetch_add(bytes, Relaxed);
            if let Some(slot) = RECLAMATION.withdrawn_by_reason.get(reason) {
                slot.fetch_add(1, Relaxed);
            }
            let buffer_start = cm.entry as usize;
            (RetiredCode::Owner(cm), buffer_start, bytes)
        })
        .collect();
    enqueue_retired_batch(batch);
}

/// The largest band a blocked-window summary scans; a wider one leaves the
/// summary unusable (the thread counts as running, the fail-safe direction).
pub(crate) const MAX_BLOCKED_BAND_BYTES: usize = 1 << 20;

/// The code-region snapshot a blocked-window band scan reads, caught up with
/// every registration so far.
///
/// The lock-free snapshot may LAG registrations (see
/// `JIT_CODE_REGION_SNAPSHOT`): a buffer created since the last republish is
/// absent from it. A summary built from a lagging snapshot would omit a body
/// this stack returns into, and the drain would free it under the parked
/// thread. So catch the snapshot up first. One lock per blocking transition,
/// and only a clone when something was registered since the last republish;
/// a blocked thread runs no code newer than this capture before it leaves.
///
/// The snapshot is LOADED under the same lock, not after it. A deregistration
/// withdraws the snapshot by storing an EMPTY one (see
/// `JitCodeRegion::deregister`), so a load taken after unlocking could see that
/// empty list and build a summary naming nothing -- and an empty summary
/// PERMITS the drain to free every body this parked stack returns into. Under
/// the lock no deregistration can intervene between the catch-up and the load.
pub(crate) fn caught_up_region_snapshot() -> Arc<Vec<(usize, usize)>> {
    let mut guard = jit_code_regions().lock().unwrap_or_else(|e| e.into_inner());
    if guard.published_epoch != REGIONS_EPOCH.load(std::sync::atomic::Ordering::Acquire) {
        publish_region_snapshot(&mut guard);
    }
    jit_code_region_snapshot().load_full()
}

/// Record the start of every executable buffer of `regions` a word of
/// `[lo, hi)` points into, sorted and deduplicated, into `out`.
///
/// Conservative: a stale word counts, which only retains more. Returns `false`
/// for a band too large to scan, which leaves the summary unusable -- the
/// thread then counts as running, the fail-safe direction.
///
/// Words are read volatile: under the lazy summary (nio21 wave 17 L3) the band
/// belongs to ANOTHER thread, blocked, whose frames below its blocking door
/// may still be running native code (see [`PendingBlockedBand`]).
///
/// # Safety
///
/// Every byte of `[lo, hi)` must be readable.
pub(crate) unsafe fn scan_code_buffers_in_band(
    lo: usize,
    hi: usize,
    regions: &[(usize, usize)],
    out: &mut Vec<usize>,
) -> bool {
    let lo = (lo + 7) & !7usize;
    if hi <= lo {
        return true;
    }
    if hi - lo > MAX_BLOCKED_BAND_BYTES {
        return false;
    }
    let (Some(&(envelope_lo, _)), Some(&(_, envelope_hi))) = (regions.first(), regions.last())
    else {
        return true;
    };
    // nio21 wave 17 L3: the last region a word resolved to, tested before the
    // binary search. A compiled stack's return addresses cluster in a few
    // bodies, and a hit answers exactly what the search would (regions are
    // disjoint, so the region containing a word is unique).
    let mut last: Option<(usize, usize)> = None;
    let mut addr = lo;
    while addr + 8 <= hi {
        // SAFETY: the caller guarantees the band is readable; `addr` is 8-byte
        // aligned and `addr + 8 <= hi`.
        let word = unsafe { std::ptr::read_volatile(addr as *const usize) };
        // Regions are sorted and disjoint, so their envelope rejects almost
        // every word with two compares before the binary search.
        if word >= envelope_lo && word < envelope_hi {
            let hit = match last {
                Some((start, end)) if word >= start && word < end => Some((start, end)),
                _ => region_containing_in(regions, word),
            };
            if let Some((start, end)) = hit {
                last = Some((start, end));
                out.push(start);
            }
        }
        addr += 8;
    }
    out.sort_unstable();
    out.dedup();
    true
}

/// Record the start of every executable buffer a word of `[lo, hi)` points
/// into, sorted and deduplicated, into `out` (the eager summary).
///
/// # Safety
///
/// Every byte of `[lo, hi)` must be readable.
pub(crate) unsafe fn collect_code_buffers_in_band(lo: usize, hi: usize, out: &mut Vec<usize>) -> bool {
    let aligned_lo = (lo + 7) & !7usize;
    if hi <= aligned_lo {
        return true;
    }
    if hi - aligned_lo > MAX_BLOCKED_BAND_BYTES {
        return false;
    }
    let regions = caught_up_region_snapshot();
    // SAFETY: forwarded from the caller.
    unsafe { scan_code_buffers_in_band(lo, hi, &regions, out) }
}

/// nio21 wave 17 L3: a blocked window's band, recorded at the transition and
/// scanned only if a drain asks for this thread's evidence while it is still
/// blocked (`CRATONVM_JIT_LAZY_BLOCKED_SUMMARY`, default on; `=0` scans at the
/// transition, as before).
///
/// Why the deferred scan answers what the eager one would have:
///
/// * `regions` is the snapshot the eager scan would have read, captured at the
///   transition under the same catch-up rule ([`caught_up_region_snapshot`]).
/// * Every return address that can lead this thread back into compiled code
///   while it is blocked lies in a frame that stays LIVE for the whole window:
///   the compiled frames themselves, and the return address the compiled
///   `CALL` pushed above the Rust door that blocked. Those words cannot change
///   until the thread leaves the window, and it cannot run compiled code
///   without [`jit_thread_blocked_leave`] or [`jit_execution_enter`], both of
///   which take the summary lock -- so a drain scanning under that lock reads
///   them exactly as the transition would have.
/// * The words below the door (the blocking native's own Rust frames) may have
///   changed since. A word read there is at worst a stale code address, which
///   retains more, never less: nothing below the door is a compiled frame.
/// * The band stays mapped while the summary is valid: the thread's exit
///   (`ThreadQuiescenceHandle::drop`) clears `valid` under the same lock
///   before its stack can be released.
pub(crate) struct PendingBlockedBand {
    pub(crate) lo: usize,
    pub(crate) hi: usize,
    pub(crate) regions: Arc<Vec<(usize, usize)>>,
}

impl BlockedStackSummary {
    /// Resolve a pending band into `buffers` (the drain side of the lazy
    /// summary). Called with the summary lock held and `valid` true. A band
    /// that cannot be scanned leaves the summary invalid, as the eager scan's
    /// `false` did.
    pub(crate) fn resolve_pending(&mut self) {
        let Some(band) = self.pending.take() else {
            return;
        };
        self.buffers.clear();
        // SAFETY: see `PendingBlockedBand` -- the band is the blocked thread's
        // own live stack, mapped while `valid` holds under this lock.
        let complete = unsafe {
            scan_code_buffers_in_band(band.lo, band.hi, &band.regions, &mut self.buffers)
        };
        self.valid = complete;
    }
}

/// Record, at a transition into a blocked state, which executable buffers the
/// calling thread's stack can return into while it stays blocked.
///
/// A thread parked inside compiled code holds its JIT execution for as long as
/// it is parked. While blocked it can only return through the addresses on its
/// stack; it cannot run compiled code without first calling
/// [`jit_thread_blocked_leave`] or re-entering through [`jit_execution_enter`],
/// and both withdraw this summary before anything runs. So a retired body none
/// of those addresses point into is unreachable from this thread, and the drain
/// may release it without waiting for the thread to wake.
///
/// `[stack_lo, stack_hi)` must cover every frame between the caller and the
/// thread's outermost compiled entry. Nested windows keep the outermost
/// window's summary. A thread holding no JIT execution records nothing — its
/// depth of 0 already vouches for it.
///
/// # Safety
///
/// Every byte of `[stack_lo, stack_hi)` must be readable for the duration of
/// the call. Callers pass a band of the calling thread's own stack. Under the
/// lazy summary (nio21 wave 17 L3) the band is read later, by a drain, while
/// the window is open: a band of the calling thread's own stack stays mapped
/// for as long as the thread lives, and its exit withdraws the summary first.
pub unsafe fn jit_thread_blocked_enter(stack_lo: usize, stack_hi: usize) {
    use std::sync::atomic::Ordering;
    if !THREAD_QUIESCENCE_REGISTERED.with(std::cell::Cell::get) {
        return;
    }
    let _ = THREAD_QUIESCENCE.try_with(|handle| {
        let record = &handle.record;
        let nesting = record.blocked_nesting.load(Ordering::Relaxed);
        record
            .blocked_nesting
            .store(nesting.saturating_add(1), Ordering::Release);
        if nesting != 0 || record.depth.load(Ordering::Relaxed) == 0 {
            return;
        }
        // nio21 wave 17 L3: the lazy summary records the band and the region
        // snapshot the scan below would read, and leaves the scan to a drain
        // that asks while this thread is still blocked (`PendingBlockedBand`
        // says why that answers the same). Every blocking native from compiled
        // code used to pay the scan (`Nio21UnsafeVolatile pipe`: 2.7% of the
        // row in `collect_code_buffers_in_band`); a drain is rare.
        if handle.lazy_blocked_summary() {
            let aligned_lo = (stack_lo + 7) & !7usize;
            if stack_hi > aligned_lo && stack_hi - aligned_lo > MAX_BLOCKED_BAND_BYTES {
                // What the eager scan answers without reading a word.
                let mut summary = record.blocked.lock();
                summary.pending = None;
                summary.valid = false;
                return;
            }
            let regions = caught_up_region_snapshot();
            let mut summary = record.blocked.lock();
            summary.buffers.clear();
            summary.pending = Some(PendingBlockedBand {
                lo: stack_lo,
                hi: stack_hi,
                regions,
            });
            summary.valid = true;
            return;
        }
        // Scan without the summary lock held, into the summary's own buffer.
        let mut buffers = std::mem::take(&mut record.blocked.lock().buffers);
        buffers.clear();
        // SAFETY: the caller guarantees the band is readable.
        let complete = unsafe { collect_code_buffers_in_band(stack_lo, stack_hi, &mut buffers) };
        let mut summary = record.blocked.lock();
        summary.buffers = buffers;
        summary.pending = None;
        summary.valid = complete;
    });
}



/// End the blocked window [`jit_thread_blocked_enter`] opened.
pub fn jit_thread_blocked_leave() {
    use std::sync::atomic::Ordering;
    if !THREAD_QUIESCENCE_REGISTERED.with(std::cell::Cell::get) {
        return;
    }
    let _ = THREAD_QUIESCENCE.try_with(|handle| {
        let record = &handle.record;
        let nesting = record.blocked_nesting.load(Ordering::Relaxed);
        if nesting == 0 {
            return;
        }
        record.blocked_nesting.store(nesting - 1, Ordering::Release);
        if nesting == 1 {
            // Under the summary lock: a drain that read the summary as valid
            // held this lock, so it read it while this thread was still
            // blocked. (nio21 wave 17 L3: and a drain resolving the lazy
            // summary's band held it for the whole read, so this thread could
            // not get past here, and so not back into compiled code, mid-read.)
            let mut summary = record.blocked.lock();
            summary.valid = false;
            // The band is this window's; drop it (and the region snapshot it
            // holds) with the window.
            summary.pending = None;
        }
    });
}

/// t1 wave 5 lane E2 (stage 2 of
/// `i94-L6-proposal-run-a-jdk-continuation-slice-on-its-carrier-os-thread`,
/// `CRATONVM_VTHREAD_SLICE_ON_CARRIER_COMPILED`): open a "segment hold" on the
/// calling thread's blocked window for a continuation slice that runs on this
/// OS thread below a carrier which is blocked with compiled frames. Answers the
/// nesting to put back ([`jit_thread_blocked_segment_leave`]).
///
/// The slice's blocked-region transitions are not balanced on this OS thread
/// (its remount LEAVES a region it entered on another thread, its yield ENTERS
/// one it leaves elsewhere), so without the hold the remount's leave closes the
/// carrier's window and the slice's next enter scans a slice-only band: a
/// summary that vouches the thread returns into nothing while the carrier
/// still returns into compiled code. Raising the nesting by one keeps it
/// non-zero for the whole slice (every enter precedes its leave), so no band is
/// scanned, and the summary is withdrawn: the thread counts as running -- the
/// retaining direction -- until the carrier's own window closes. A thread with
/// no record answers 0 and nothing is held.
pub fn jit_thread_blocked_segment_enter() -> u32 {
    use std::sync::atomic::Ordering;
    if !THREAD_QUIESCENCE_REGISTERED.with(std::cell::Cell::get) {
        return 0;
    }
    THREAD_QUIESCENCE
        .try_with(|handle| {
            let record = &handle.record;
            let nesting = record.blocked_nesting.load(Ordering::Relaxed);
            // Withdraw first, then raise (the order `jit_execution_enter`
            // keeps: the summary stops vouching before anything else runs).
            {
                let mut summary = record.blocked.lock();
                summary.valid = false;
                summary.pending = None;
            }
            record
                .blocked_nesting
                .store(nesting.saturating_add(1), Ordering::Release);
            nesting
        })
        .unwrap_or(0)
}

/// Close the hold [`jit_thread_blocked_segment_enter`] opened: the nesting is
/// put back to exactly the carrier's (whatever the slice's unbalanced
/// transitions left), and the summary stays withdrawn -- the carrier is
/// counted as running until its own blocked window closes.
pub fn jit_thread_blocked_segment_leave(saved: u32) {
    use std::sync::atomic::Ordering;
    if !THREAD_QUIESCENCE_REGISTERED.with(std::cell::Cell::get) {
        return;
    }
    let _ = THREAD_QUIESCENCE.try_with(|handle| {
        let record = &handle.record;
        {
            let mut summary = record.blocked.lock();
            summary.valid = false;
            summary.pending = None;
        }
        record.blocked_nesting.store(saved, Ordering::Release);
    });
}

/// Whether a [`JitExecutionToken`] was counted in its thread's quiescence
/// record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ExecutionTracking {
    /// A placeholder no `enter` produced.
    Unset,
    /// Counted in the thread's [`JitThreadQuiescence::depth`].
    Tracked,
    /// The thread's record was already gone; counted in
    /// [`UNTRACKED_JIT_EXECUTIONS`].
    Untracked,
}

/// What one [`jit_execution_enter`] recorded, for the matching
/// [`jit_execution_leave`].
///
/// Carries the striped counter's stripe so the decrement lands where the
/// increment did even when the leave runs during thread teardown, after the
/// thread's stripe thread-local is gone (see
/// [`cratonvm_types::striped_counter::StripeToken`]), and which per-thread
/// count the entry went into.
#[derive(Clone, Copy, Debug)]
pub struct JitExecutionToken {
    pub(crate) stripe: cratonvm_types::striped_counter::StripeToken,
    pub(crate) tracking: ExecutionTracking,
    /// t1 wave 8, lane I25: `stripe` is a `DOOR_DEPTH` stripe
    /// ([`jit_execution_enter_door`]), not an `ACTIVE_JIT_EXECUTIONS` one.
    pub(crate) door: bool,
}

impl JitExecutionToken {
    /// A token no `enter` produced; leaving with it changes no count.
    pub const UNSET: JitExecutionToken = JitExecutionToken {
        stripe: cratonvm_types::striped_counter::StripeToken::UNSET,
        tracking: ExecutionTracking::Unset,
        door: false,
    };

    /// Whether [`jit_execution_enter_door`] produced this token: its entry is
    /// counted in `cratonvm_types::striped_counter::DOOR_DEPTH`, which stands
    /// for the VM's and the collector's depth counters as well, so the chain
    /// pop that settles it must not lower those.
    #[inline]
    pub fn counts_door_depth(&self) -> bool {
        self.door
    }
}

/// The per-thread half of [`jit_execution_enter`]: this thread's quiescence
/// depth (or the untracked count when its record is gone).
#[inline]
pub(crate) fn execution_tracking_enter() -> ExecutionTracking {
    use std::sync::atomic::Ordering;
    let tracked = THREAD_QUIESCENCE
        .try_with(|handle| {
            let record = &handle.record;
            let depth = record.depth.load(Ordering::Relaxed);
            record.depth.store(depth + 1, Ordering::Release);
            if record.blocked_nesting.load(Ordering::Relaxed) != 0 {
                // Compiled code is about to run inside a blocked window — a
                // native method calling back into Java. Frames it pushes can
                // reach bodies published after the window's stack scan, so the
                // summary stops vouching for this thread before any of that
                // code runs.
                record.blocked.lock().valid = false;
            }
        })
        .is_ok();
    if tracked {
        ExecutionTracking::Tracked
    } else {
        UNTRACKED_JIT_EXECUTIONS.fetch_add(1, Ordering::AcqRel);
        ExecutionTracking::Untracked
    }
}

/// [`jit_execution_enter`] for the interpreter-to-compiled door's one-counter
/// form (t1 wave 8, lane I25; `CRATONVM_JIT_DOOR_ONE_DEPTH_COUNTER`, per VM,
/// default ON since j27 2026-10-28). The per-thread record moves exactly as in `jit_execution_enter`;
/// the process-wide count is one increment of
/// `cratonvm_types::striped_counter::DOOR_DEPTH`, which the readers of
/// `ACTIVE_JIT_EXECUTIONS` (a `DoorFoldedCounter`) add back, and which the VM's
/// `GLOBAL_JIT_DEPTH` and the collector's quiescence depth fold in too -- so
/// the caller raises neither of those for this entry.
/// [`jit_execution_leave`] undoes it.
#[must_use = "the token must be passed to the matching jit_execution_leave"]
#[inline]
pub fn jit_execution_enter_door() -> JitExecutionToken {
    let tracking = execution_tracking_enter();
    JitExecutionToken {
        stripe: cratonvm_types::striped_counter::DOOR_DEPTH.enter(),
        tracking,
        door: true,
    }
}

/// Enter/leave the executable-code quiescence epoch. VM JIT entry guards call
/// these at the same boundaries as their precise frame chain, and keep the
/// returned token beside the entry it describes.
///
/// Each call updates both the process-wide count (the drain's fast path) and
/// this thread's own depth, which is what lets a drain release a body while
/// some other thread stays inside compiled code.
#[must_use = "the token must be passed to the matching jit_execution_leave"]
pub fn jit_execution_enter() -> JitExecutionToken {
    let tracking = execution_tracking_enter();
    JitExecutionToken {
        stripe: ACTIVE_JIT_EXECUTIONS.inc_token(),
        tracking,
        door: false,
    }
}

pub fn jit_execution_leave(token: JitExecutionToken) {
    use std::sync::atomic::Ordering;
    if token.door {
        cratonvm_types::striped_counter::DOOR_DEPTH.leave(token.stripe);
    } else {
        ACTIVE_JIT_EXECUTIONS.dec_token(token.stripe);
    }
    let mut pump = false;
    match token.tracking {
        ExecutionTracking::Unset => return,
        ExecutionTracking::Untracked => {
            let _ =
                UNTRACKED_JIT_EXECUTIONS.try_update(Ordering::Release, Ordering::Acquire, |n| {
                    Some(n.saturating_sub(1))
                });
        }
        ExecutionTracking::Tracked => {
            let _ = THREAD_QUIESCENCE.try_with(|handle| {
                let record = &handle.record;
                let depth = record.depth.load(Ordering::Relaxed).saturating_sub(1);
                record.depth.store(depth, Ordering::Release);
                if record.blocked_nesting.load(Ordering::Relaxed) != 0 {
                    // Round 14 wave 1 (lane codecache, `gcd-d10v`): compiled code
                    // returned to a door INSIDE a blocked window. When the door
                    // was entered inside the window too (a native region calling
                    // back into Java: an FFM upcall, a JNI callback) the enter
                    // already withdrew the summary; otherwise the window was left
                    // open around running compiled code, and its stack scan no
                    // longer describes the frames. The thread counts as running
                    // until the window closes -- the retaining direction.
                    // Round 14 wave 2 (lane review2): the switch is read only
                    // while the summary still vouches, so the callback case
                    // (every upcall return) pays no flag read.
                    let mut blocked = record.blocked.lock();
                    if blocked.valid {
                        blocked.valid = false;
                    }
                }
                if depth != 0 {
                    return;
                }
                // Read with this thread's depth already 0: every retirement
                // stamped at or below it predates an instant at which this
                // thread held no compiled frame.
                record.quiescent_gen.store(
                    JIT_RETIRE_GENERATION.load(Ordering::Acquire),
                    Ordering::Release,
                );
                // Round 13 wave 2 (lane mega,
                // `r12w7-mega6-grace-starves-while-a-thread-stays-compiled`):
                // the pump also runs while only a retired inline-cache way
                // (nothing queued) waits for its grace -- see below.
                // A grace can lag for as long as a thread spins in compiled
                // code, so a grace-only pump runs at a sparser cadence than a
                // drain pump: the walk is O(registered threads).
                let queued = RECLAMATION.queued_owners.load(Ordering::Relaxed) != 0;
                let grace_lags = !queued
                    && JIT_RETIRE_GENERATION.load(Ordering::Relaxed)
                        > JIT_GRACED_GENERATION.load(Ordering::Relaxed)
                    && handle.grace_by_thread_evidence();
                if queued || grace_lags {
                    let left = handle.drain_countdown.get();
                    if left == 0 {
                        handle.drain_countdown.set(if queued {
                            DRAIN_PUMP_INTERVAL
                        } else {
                            GRACE_PUMP_INTERVAL
                        });
                        pump = true;
                    } else {
                        handle.drain_countdown.set(left - 1);
                    }
                }
            });
        }
    }
    if RECLAMATION.queued_owners.load(Ordering::Relaxed) != 0 {
        // The process-wide proof is one short-circuiting walk and can free
        // everything; per-thread evidence walks the registry, so it rides only
        // this thread's pump.
        if ACTIVE_JIT_EXECUTIONS.is_zero() {
            drain_deferred_jit_owners();
        } else if pump && pump_drain_is_due() {
            drain_deferred_jit_owners();
        }
    } else if JIT_RETIRE_GENERATION.load(Ordering::Relaxed)
        > JIT_GRACED_GENERATION.load(Ordering::Relaxed)
    {
        // Nothing queued, but a retired inline-cache way may be waiting out its
        // grace period.
        let observed = JIT_RETIRE_GENERATION.load(Ordering::Acquire);
        if ACTIVE_JIT_EXECUTIONS.is_zero() {
            note_graced_generation(observed);
            note_code_graced_generation(observed);
        } else if pump {
            // Round 13 wave 2 (lane mega): the drain's per-thread proof,
            // without a queued owner to carry it. Before, with the queue
            // empty, only a process-wide zero graced a way, so ONE worker
            // parked inside compiled code (`LinkedBlockingQueue.take()`)
            // stranded every retired way of the process for as long as the
            // queue stayed empty. Same rule as the drain: `observed` is read
            // before the evidence, blocked threads cannot be inside a probe,
            // and a running thread vouches only up to the generation it read
            // at its last return to depth 0. `CRATONVM_JIT_IC_GRACE_THREAD_EVIDENCE=0`
            // (read per thread, once) never sets `pump` on this path.
            if let Some(evidence) = collect_thread_quiescence_evidence() {
                // Round 14 wave 2 (lane mic, M8-1): a running thread vouches up
                // to its later stamp, the miss-handler one included.
                note_graced_generation(observed.min(evidence.min_running_ic_gen));
                // RV3-2: the code grace, from depth-0 returns only.
                note_code_graced_generation(observed.min(evidence.min_running_gen));
            }
        }
    }
}

/// Hand a compiled artifact to the retirement queue.
///
/// The public face of `defer_jit_owner`. **Every** long-lived holder of an
/// `Arc<CompiledMethod>` that backs a raw entry pointer must release it through
/// here rather than by dropping the `Arc`: a plain drop that happens to be the
/// last one unmaps the body without asking whether a thread is inside it, which
/// is exactly the use-after-free the queue exists to prevent. Prefer
/// [`RetainedCode`], which cannot forget.
pub fn retire_jit_owner(owner: Option<Arc<CompiledMethod>>) {
    defer_jit_owner(owner);
}

/// An `Arc<CompiledMethod>` held as the keep-alive for a raw entry pointer,
/// whose release goes through the retirement queue **by construction**.
///
/// The distinction matters only on the last reference, and only for a body a
/// thread can be executing — but that is precisely the case a per-call-site or
/// per-thread dispatch cache produces. Such a cache is evicted by the very
/// thread dispatching through it (a generation flush, a staleness eviction, a
/// replacement), so the eviction can run while that thread is *inside* the body
/// whose only remaining owner the cache entry is. Dropping the `Arc` there
/// unmaps the code under the thread's own return address.
///
/// Wrapping the field makes the routing structural: `clear`, `remove`,
/// map-replacement, `Vec` truncation and thread-exit teardown all release
/// through [`retire_jit_owner`] without any of those call sites knowing they
/// had an obligation.
pub struct RetainedCode(pub(crate) Option<Arc<CompiledMethod>>);

impl RetainedCode {
    /// Take ownership of a compiled artifact as a raw-entry keep-alive.
    pub fn new(owner: Arc<CompiledMethod>) -> Self {
        // CODECACHE21-2 (JIT round 21 wave 4, lane codecache): count the
        // handle. Relaxed: the decrement that matters is the AcqRel one in
        // `drop`, and every increment happens-before it through the `Arc`
        // this handle carries to wherever it is dropped.
        if owner.retained_holder_count {
            owner
                .retained_holders
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        Self(Some(owner))
    }

    /// The underlying artifact.
    pub fn arc(&self) -> &Arc<CompiledMethod> {
        self.0
            .as_ref()
            .expect("RetainedCode is only empty while dropping")
    }
}

impl Clone for RetainedCode {
    fn clone(&self) -> Self {
        match &self.0 {
            Some(owner) => Self::new(Arc::clone(owner)),
            None => Self(None),
        }
    }
}

impl std::ops::Deref for RetainedCode {
    type Target = CompiledMethod;
    fn deref(&self) -> &CompiledMethod {
        self.arc()
    }
}

impl std::fmt::Debug for RetainedCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "RetainedCode(entry={:#x})", self.entry_ptr() as usize)
    }
}

impl From<Arc<CompiledMethod>> for RetainedCode {
    fn from(owner: Arc<CompiledMethod>) -> Self {
        Self::new(owner)
    }
}

impl Drop for RetainedCode {
    fn drop(&mut self) {
        let Some(owner) = self.0.take() else {
            return;
        };
        // The per-call-site invoke cache is *cloned* on every cached dispatch,
        // so this drop is a hot path. A drop that is provably not the last
        // reference cannot unmap anything, and the retirement queue has nothing
        // to say about it — one relaxed load instead of a striped-counter walk.
        //
        // Two concurrent last-two drops can both read `> 1` and neither route.
        // Measured, that does happen (a C2 compile task and a mutator releasing
        // the same superseded body), and the argument that used to be offered
        // here — "every thread inside a compiled body holds an owning
        // reference, so a count reaching zero proves no thread is inside" — was
        // contradicted by a Spring crash whose fault pc sat in exactly such a
        // body. The race is closed one level down instead:
        // `ExecutableBuffer::drop` rescues a published mapping released outside
        // the queue INTO the queue, so the plain last drop this branch can
        // produce is deferred like any other
        // (`jit-code-uaf-outside-retirement-queue-FIXED-20260918.md`).
        // `published_code_free_audit` still counts it.
        //
        // The one caveat, stated because it is invisible at this call site: the
        // JIT->JIT dispatch pin resolves through the code-range registry, which
        // `put`/`put_osr` populate only when precise JIT maps OR the
        // cross-thread root scan are on. Both default ON, and opting out of
        // BOTH (`CRATONVM_NO_PRECISE_JIT_MAPS=1` with
        // `CRATONVM_XT_JIT_ROOT_SCAN=0`) leaves that one entry path unpinned —
        // as it was before this existed, so no regression, but the ownership
        // argument above is then weaker than the queue alone.
        //
        // NOT `Arc::into_inner` + re-wrap, although that would detect the last
        // reference exactly: it frees the ORIGINAL allocation at once, and the
        // code-range registry holds that allocation's raw address
        // (`register_jit_code_range(entry, len, Arc::as_ptr(..))`) until the
        // body's own drop unregisters it. Keeping the original `Arc` alive in
        // the queue is what keeps that address valid meanwhile.
        // CODECACHE21-2 (JIT round 21 wave 4, lane codecache,
        // `CRATONVM_JIT_RETAINED_HOLDER_COUNT`): the shortcut below lets two
        // concurrent last-two drops both read `> 1` and both skip the queue.
        // With the holder count, exactly one handle -- the one whose decrement
        // reaches zero -- routes its `Arc` through the queue. A handle that is
        // not the last may drop plainly: the last one still holds its own
        // `Arc` (or has already queued it), so this drop cannot be the final
        // reference. Bare `Arc` holders are outside the count; a body they
        // release last is rescued by `ExecutableBuffer::drop`, as before.
        //
        // Review rev-w45-jit item 1 (wave 7, lane codecache): this does NOT
        // queue a reader's handle on a body the cache still publishes. The
        // shard map's entry is itself a counted `RetainedCode` (`JitCacheMap`),
        // as is every VM memo entry, so a published body's count stays >= 1
        // and a per-thread invoke-cache eviction is not the last holder: it
        // takes the plain branch below. `last_holder` is true only once every
        // publisher has let go -- superseded, withdrawn, never published, or a
        // memo-only body -- and that is exactly when this reference may be the
        // final one while a thread is inside. A "not retired, not withdrawn
        // => plain" narrowing would therefore drop superseded and unpublished
        // bodies plainly, a regression below even the `strong_count` shortcut.
        // Pinned by `r21w7_codecache_published_handle_tests`.
        if owner.retained_holder_count {
            let last_holder = owner
                .retained_holders
                .fetch_sub(1, std::sync::atomic::Ordering::AcqRel)
                <= 1;
            if !last_holder && Arc::strong_count(&owner) > 1 {
                drop(owner);
                return;
            }
            defer_jit_owner(Some(owner));
            return;
        }
        if Arc::strong_count(&owner) > 1 {
            drop(owner);
            return;
        }
        defer_jit_owner(Some(owner));
    }
}

/// Pin a raw compiled entry while an external dispatch cache can publish it.
pub fn pin_jit_entry(entry: usize) -> Option<Arc<CompiledMethod>> {
    let pinned = resolve_jit_entry_owner(entry);
    if pinned.is_none() {
        // DIAG (2026-07-27): a caller is about to cache this RAW entry with no
        // keep-alive, so nothing stops the body being munmapped under it. Count
        // and report under `CRATONVM_DBG_JIT_PIN=1`.
        UNPINNED_JIT_ENTRIES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if dbg_jit_pin_enabled() {
            eprintln!("[jit-pin-miss] entry=0x{entry:x} cached with NO owner keep-alive");
        }
    }
    pinned
}

/// Count of `pin_jit_entry` calls that could not find a live owner.
pub static UNPINNED_JIT_ENTRIES: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// Count of baked direct-call targets that could not be pinned at publication.
pub static UNROOTED_DIRECT_CALLEES: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// Baked direct-call targets whose entry address changed hands between bake
/// and publication. `UNROOTED_DIRECT_CALLEES` cannot see these: the pin
/// succeeds, on the wrong body.
pub static REBOUND_DIRECT_CALLEES: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// Publications refused because a baked direct-call target had already been
/// retired by an invalidation. `UNROOTED_DIRECT_CALLEES` cannot see these: the
/// pin succeeds, on a body the invalidation withdrew.
pub static RETIRED_DIRECT_CALLEES: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// The five keep-alive failure counters above, as one snapshot, in this order:
///
/// 1. `UNPINNED_JIT_ENTRIES` — `pin_jit_entry` calls that found no live owner,
///    so a raw entry was cached with nothing stopping its body being unmapped
///    under the cache.
/// 2. `UNROOTED_DIRECT_CALLEES` — baked direct-call targets that could not be
///    pinned at publication.
/// 3. `REBOUND_DIRECT_CALLEES` — baked targets whose entry address changed
///    hands between bake and publication (the pin succeeded, on the wrong
///    body).
/// 4. `RETIRED_DIRECT_CALLEES` — publications refused because a baked target
///    had already been withdrawn by an invalidation.
/// 5. `IC_RETIRED_INSTALL_ROLLBACKS` — inline-cache installs rolled back
///    because the target had been retired.
///
/// # Why this accessor exists (round 10, lane `gauges`)
///
/// All five were incremented by production paths and **read by nothing in the
/// workspace** — no accessor, no diagnostic, no test. They are the same shape
/// as the code-cache failed-allocation gauges this round wired up
/// (`r10-gcs2-code-cache-alloc-failure-gauges-never-fed-20260921.md`), one
/// step further gone: those at least had an entry point nobody called, these
/// had no way out at all. A counter nothing reads is indistinguishable from a
/// counter that never fires, and what these count is not cosmetic — each is a
/// case where a compiled body's keep-alive did not hold, which is the
/// precondition for executing unmapped code.
///
/// `UNPINNED_JIT_ENTRIES` did have a per-occurrence `eprintln!` under
/// `CRATONVM_DBG_JIT_PIN=1`, which is a different instrument: it names one
/// entry address at the moment it happens and says nothing about how often.
/// The total is what tells an operator whether the condition is a one-off or
/// the steady state, and it is the number this returns.
///
/// Consumed by `vm::jit::code_cache_lifecycle`'s report, which prints the line
/// only when some counter is non-zero — so a healthy process's report is
/// unchanged.
pub fn jit_code_keepalive_census() -> (u64, u64, u64, u64, u64) {
    use std::sync::atomic::Ordering::Relaxed;
    (
        UNPINNED_JIT_ENTRIES.load(Relaxed) as u64,
        UNROOTED_DIRECT_CALLEES.load(Relaxed) as u64,
        REBOUND_DIRECT_CALLEES.load(Relaxed) as u64,
        RETIRED_DIRECT_CALLEES.load(Relaxed) as u64,
        IC_RETIRED_INSTALL_ROLLBACKS.load(Relaxed),
    )
}

/// Whether an unrootable baked direct-call target blocks publication
/// (`CRATONVM_JIT_STRICT_CALLEE_ROOTS`).
///
/// The summary line above this one used to read "`CRATONVM_JIT_STRICT_CALLEE_
/// ROOTS=1` refuses to publish a compiled body whose baked direct-call targets
/// cannot all be kept alive", which was the flag's opt-IN description and
/// survived the 2026-07-28 flip: it contradicted the next paragraph and the
/// reader below (`unwrap_or(true)`). Removed in r10 (lane `drv`).
///
/// OFF-WORD ASYMMETRY, deliberate but worth knowing: this reader treats ONLY
/// the exact string `"0"` as off, and does not trim. The other default-ON JIT
/// flags (`deopt_real_enabled`, `local_handlers_enabled`,
/// `receiver_despec_enabled`) accept `0`/`false`/`off`/`no` and trim first, so
/// `CRATONVM_JIT_STRICT_CALLEE_ROOTS=false` leaves this ON while the same word
/// turns those OFF.
///
/// DEFAULT-ON since 2026-07-28. It arrived default-OFF with the diagnostic that
/// found the retired-JIT-code family (e4b848394), which only *counted* the
/// event — so the common case was to publish anyway, exactly the use-after-free
/// `prepare_for_publication`'s doc comment says must not happen. Observed on
/// the `JitOsrLoopProgress` fixture: `step`'s C2 compile baked a CALL to
/// `maybeThrow`'s live entry, a concurrent `maybeThrow` recompile replaced it
/// and unmapped the old body BEFORE `step` reached publication, and the
/// published `step` then jumped into the freed page (SIGSEGV, `pc == addr`).
///
/// Refusing publication costs one wasted compile: the method stays interpreted
/// and is recompiled on a later invocation, by which point the callee has a
/// live body. `CRATONVM_JIT_STRICT_CALLEE_ROOTS=0` restores the historical
/// publish-anyway behaviour for bisection.
pub(crate) fn strict_callee_roots_enabled() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        Err::<String, std::env::VarError>(std::env::VarError::NotPresent)
            .map(|v| v != "0")
            .unwrap_or(true)
    })
}

pub(crate) fn dbg_jit_pin_enabled() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        Err::<String, std::env::VarError>(std::env::VarError::NotPresent)
            .map(|v| v != "0")
            .unwrap_or(false)
    })
}

/// `CRATONVM_JIT_PUBLISH_OVER_SUPERSEDED_CALLEE` (JIT r25, lane regex rows;
/// perf, **default ON** since R25-FLIP, `=0` kills): [`JitCache::prepare_for_publication`] no longer
/// refuses a body whose baked direct-call target was retired by the supersede
/// redirect alone (superseded, not entrant, withdrawn by no logged
/// invalidation); see [`JitCache::superseded_callee_still_callable`]. Off, every
/// retired callee refuses the publication, as before. Read per publication
/// that baked a retired callee, never on a hot path.
pub(crate) fn publish_over_superseded_callee_enabled() -> bool {
    // R25-FLIP: default ON; `=0` is the kill switch.
    true
}

/// `CRATONVM_JIT_PIN_BAKED_CALLEES_WHILE_COMPILING` (JIT r25 wave 2, lane regex
/// rows; perf, **default ON** since R25-FLIP, `=0` kills; meant with
/// [`publish_over_superseded_callee_enabled`]): the optimizing tier pins every
/// direct-call target it baked during planning (a `RetainedCode` per entry whose
/// owner is still the artifact recorded at bake time) from just before the
/// graph build until the artifact's own publication, which replaces the pins
/// with its `_direct_callee_roots`. Without it, a callee superseded during a
/// long compile whose old body no published caller roots is freed before
/// `put`, and its entry no longer upgrades (`baked-callee UNROOTED`), which
/// `CRATONVM_JIT_STRICT_CALLEE_ROOTS` refuses whatever the supersede rule says.
/// Read once per optimizing compile.
pub(crate) fn pin_baked_callees_while_compiling_enabled() -> bool {
    // R25-FLIP: default ON; `=0` is the kill switch.
    true
}

/// JIT round 21 (lane accept; W18L4-5 of `jit-r21-nio-proposals-w18-L4.md`,
/// the method-entry half): `JitCache::put` drops a body without a word on six
/// paths (a redefinition hold, a stale install epoch, a live optimizing body a
/// single-pass one may not replace, an invalidated dependency, an unpinnable
/// baked callee, a redefined baked callee), and the door that asked for the
/// publication reads the cache back and sees whatever is there. Under
/// `CRATONVM_DBG_JIT_COMPILED` each drop now prints one line, beside the `put`
/// line a publication prints, so "produced a body" with no `put` line names its
/// reason. Diagnostic only: the flag is read only on a drop.
pub(crate) fn dbg_jit_put_dropped(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    compiled: &CompiledMethod,
    reason: &str,
) {
    if false {
        eprintln!(
            "CRATONVM_DBG_JIT_COMPILED: dropped {class_name}.{method_name}{descriptor} \
             ({reason}; {} body)",
            if compiled.used_ir_backend {
                "optimizing"
            } else {
                "single-pass"
            }
        );
    }
}

/// Monotonic process-wide *compilation* epoch.
///
/// Read at the START of a compilation and stamped onto the resulting artifact
/// ([`CompiledMethod::install_epoch`]); advanced by every event that makes an
/// already-running compilation's input bytecode or class layout obsolete —
/// [`bump_redefine_epoch`] and [`JitCache::clear_all`].
///
/// This is a *counter*, not a gate. The gate is per-cache
/// ([`JitCache::flush_barrier`]) so that one VM's redefinition cannot refuse
/// another VM's unrelated publication, and so a test flushing its own
/// `JitCache` cannot refuse a concurrent test's `put`. Starts at 1 so `0` is
/// never a legitimate stamp.
pub static JIT_INSTALL_EPOCH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// Current compilation epoch (one acquire load).
#[inline]
pub fn jit_install_epoch() -> u64 {
    JIT_INSTALL_EPOCH.load(std::sync::atomic::Ordering::Acquire)
}

// The process-wide redefinition count (`JIT_PROCESS_REDEFINE_EPOCH`,
// `redefine_epoch()`) is gone since interpreter round i1 wave 23: its last
// reader, the IR refusal memo key, reads the compiling VM's own redefine epoch
// through its `JitVerdictRegistry` (`i22-L5-ir-site-trap-decisions-are-process-wide-by-method-name`).
// The per-VM counter inline caches compare against is [`JitCache::redefine_epoch`].

/// Advance the compilation epoch and return the NEW value.
#[inline]
pub fn bump_jit_install_epoch() -> u64 {
    JIT_INSTALL_EPOCH.fetch_add(1, std::sync::atomic::Ordering::AcqRel) + 1
}

/// Monotonic process-wide *invalidation* epoch.
///
/// Advanced by every dependency invalidation of a [`JitCache`]
/// (`invalidate_for_class_change`, `invalidate_for_class`,
/// `invalidate_unloaded_class`, `remove`) as it logs the invalidation, and read
/// at the START of every compilation through the same witness that stamps the
/// install epoch ([`CompiledMethod::invalidation_epoch`]). Like
/// [`JIT_INSTALL_EPOCH`] it is a counter, not a gate: the gate is each cache's
/// own invalidation log, so one VM's invalidation cannot refuse another VM's
/// unrelated publication.
pub(crate) static JIT_INVALIDATION_EPOCH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// Current invalidation epoch (one acquire load). Public so a body kept
/// outside every `JitCache` (the VM's optimizing OSR memo) can tell, with one
/// load, whether any invalidation was logged since it last checked; see
/// [`JitCache::unpublished_body_dependencies_are_current`].
#[inline]
pub fn jit_invalidation_epoch() -> u64 {
    JIT_INVALIDATION_EPOCH.load(std::sync::atomic::Ordering::Acquire)
}

thread_local! {
    /// The `(install, invalidation)` epochs the compilation running on this
    /// thread began at, or `None` when no compilation is open.
    pub(crate) static COMPILE_INSTALL_EPOCH: std::cell::Cell<Option<(u64, u64)>> =
        const { std::cell::Cell::new(None) };
}

/// RAII scope opened for the whole duration of one compilation.
///
/// Re-entrant and conservative: a nested compile (the `callee_compiler`
/// recursion, or an inlined callee compiled on the same thread) keeps the
/// OUTERMOST — i.e. oldest — epoch, because the inner artifact is only useful
/// if the outer one is, and an outer stamp can only refuse more.
#[must_use = "the witness must stay alive for the whole compilation"]
pub struct CompileEpochWitness {
    pub(crate) prev: Option<(u64, u64)>,
    pub(crate) prev_cp_stamp: Option<u64>,
    /// This witness opened the scope's copied-classes list (it is the
    /// outermost one), so its drop closes it (interpreter round i1 wave 25,
    /// lane L6).
    pub(crate) opened_copies: bool,
    /// The scope's layout-epoch snapshot before this witness opened (round
    /// 16, lane fieldguard, FG-b3); restored on drop.
    pub(crate) prev_layout_epoch: Option<u32>,
}

impl Drop for CompileEpochWitness {
    fn drop(&mut self) {
        COMPILE_INSTALL_EPOCH.with(|c| c.set(self.prev));
        COMPILE_CP_STAMP.with(|c| {
            if let Ok(mut scope) = c.try_borrow_mut() {
                scope.cp_stamp = self.prev_cp_stamp;
                scope.layout_epoch = self.prev_layout_epoch;
                if self.opened_copies {
                    scope.copied = None;
                }
            }
            // Wave 25 (lane initwitness): the outermost witness closes the
            // lost-record flag with the list it opened.
            if self.opened_copies {
                c.copy_lost.set(false);
            }
        });
        if self.opened_copies {
            if let Ok(Some(end)) = COMPILE_CP_STAMP.try_with(|c| c.end_hook.take()) {
                end();
            }
        }
    }
}

/// Run `end` once, when the outermost [`CompileEpochWitness`] open on this
/// thread drops (the VM clears its per-compile inline-resolution memo there).
/// Per thread and per compile, so it needs no process-wide state; a second
/// registration replaces the first.
pub fn on_compile_end(end: fn()) {
    let _ = COMPILE_CP_STAMP.try_with(|c| c.end_hook.set(Some(end)));
}

/// Is a compilation (an outermost [`CompileEpochWitness`]) open on this thread?
pub fn compile_scope_open() -> bool {
    COMPILE_CP_STAMP
        .try_with(|c| c.try_borrow().is_ok_and(|scope| scope.copied.is_some()))
        .unwrap_or(false)
}

/// The VM's class-redefinition count (`cratonvm_classloading::class_redefinition_count`),
/// registered once by the VM ([`register_cp_stamp_source`]). The jit crate
/// cannot name the class loader; it only needs the number, the same one an
/// interpreter frame records as its `redefine_stamp`.
pub(crate) static CP_STAMP_SOURCE: std::sync::OnceLock<fn() -> u64> = std::sync::OnceLock::new();

/// Register the VM's class-redefinition count as the source of the
/// constant-pool stamp compiled code bakes next to a `(holder, cp index)`
/// helper argument ([`cp_holder_word`]). Idempotent; the first registration
/// wins (the count is process-wide, so every VM registers the same one).
pub fn register_cp_stamp_source(source: fn() -> u64) {
    let _ = CP_STAMP_SOURCE.set(source);
}

/// What the compilation open on this thread records beside its epochs
/// ([`open_compile_epoch_witness`]).
pub(crate) struct CompileScope {
    /// The class-redefinition count the compilation began at, or `None` when
    /// no compilation is open. See [`cp_holder_word`].
    pub(crate) cp_stamp: Option<u64>,
    /// The classes (ids) whose method bytecode the compilation copied into
    /// the code it builds ([`JitCache::note_bytecode_copied`] while it is
    /// open), or `None` when no compilation is open. Every
    /// [`CompiledMethod`] built inside the scope carries the list as it
    /// stands then ([`CompiledMethod::copied_classes`]; interpreter round i1
    /// wave 25, lane L6, stage 1 of
    /// `i20-L1-proposal-per-body-copied-bytecode-dependencies`).
    pub(crate) copied: Option<Vec<u32>>,
    /// Round 16 (lane fieldguard, FG-b3): `LAYOUT_REPLACE_EPOCH` as the
    /// compilation began -- before any compact field row of it was resolved
    /// -- or `None` when no compilation is open (or
    /// `CRATONVM_JIT_LAYOUT_EPOCH_SNAPSHOT=0`). Every inline layout guard the
    /// compilation emits bakes THIS value ([`compile_baked_layout_epoch`]).
    pub(crate) layout_epoch: Option<u32>,
}

/// The thread-local slot of [`CompileScope`]: the scope behind its
/// `RefCell` (every access site borrows it through [`std::ops::Deref`]), and
/// beside it, outside the borrow, the flag a copy record that could not be
/// written sets ([`note_copy_in_open_compile`]; JIT round 21 wave 25, lane
/// initwitness, flip review item 7 of
/// `r21-optin-flip-review-FIXED-20261021.md`). A `Cell`, so
/// it can be set while the scope's `RefCell` is borrowed -- exactly when the
/// record is lost.
pub(crate) struct CompileScopeSlot {
    pub(crate) scope: std::cell::RefCell<CompileScope>,
    /// A copy made while the compilation open on this thread had its scope
    /// borrowed went to the cache-wide set only, not onto the scope's
    /// `copied` list. Cleared when the outermost witness opens and closes
    /// the list; read into every body built meanwhile
    /// ([`CompiledMethod::copy_record_lost`]).
    pub(crate) copy_lost: std::cell::Cell<bool>,
    /// [`on_compile_end`].
    pub(crate) end_hook: std::cell::Cell<Option<fn()>>,
}

impl std::ops::Deref for CompileScopeSlot {
    type Target = std::cell::RefCell<CompileScope>;

    fn deref(&self) -> &Self::Target {
        &self.scope
    }
}

thread_local! {
    /// The compilation scope open on this thread ([`CompileScope`]). One
    /// thread-local for both of its halves (the wave-20 constant-pool stamp
    /// and the wave-25 copied classes), so the jit crate's statics ratchet
    /// does not move.
    pub(crate) static COMPILE_CP_STAMP: CompileScopeSlot =
        const {
            CompileScopeSlot {
                scope: std::cell::RefCell::new(CompileScope {
                    cp_stamp: None,
                    copied: None,
                    layout_epoch: None,
                }),
                copy_lost: std::cell::Cell::new(false),
                end_hook: std::cell::Cell::new(None),
            }
        };
}

/// The constant-pool stamp of the code being compiled on this thread: the
/// class-redefinition count when the open compilation began, or (no
/// compilation open) now; `None` when no source is registered.
pub fn current_compile_cp_stamp() -> Option<u64> {
    COMPILE_CP_STAMP
        .with(|c| c.try_borrow().ok().and_then(|scope| scope.cp_stamp))
        .or_else(|| CP_STAMP_SOURCE.get().map(|source| source()))
}

/// Record that the compilation open on this thread copied bytecode of the
/// class `class_id`; `false` when no compilation is open here (interpreter
/// round i1 wave 25, lane L6).
///
/// `false` too when the scope is borrowed (`try_borrow_mut` failed): the
/// caller then records the copy in the cache-wide set, the conservative
/// answer for every whole-cache path. JIT round 21 wave 25 (lane
/// initwitness; flip review item 7 of
/// `r21-optin-flip-review-FIXED-20261021.md`): the lost
/// record is also flagged on the slot ([`CompileScopeSlot::copy_lost`]), so
/// each body the open compilation builds from then on carries
/// [`CompiledMethod::copy_record_lost`], and the by-id finalizable-subtype
/// rule (`CRATONVM_JIT_FINALIZER_WITHDRAW_BY_ID`, opt-in) takes the name
/// evidence for it -- the old rule's answer -- even when its
/// `copied_classes` holds other copies. The flag only widens that opt-in
/// rule's withdrawal; nothing else reads it.
pub(crate) fn note_copy_in_open_compile(class_id: u32) -> bool {
    COMPILE_CP_STAMP
        .try_with(|c| match c.try_borrow_mut() {
            Ok(mut scope) => match scope.copied.as_mut() {
                Some(copied) => {
                    if !copied.contains(&class_id) {
                        copied.push(class_id);
                    }
                    true
                }
                None => false,
            },
            Err(_) => {
                c.copy_lost.set(true);
                false
            }
        })
        .unwrap_or(false)
}

/// Did the compilation open on this thread lose a copy record
/// ([`CompileScopeSlot::copy_lost`])? What a [`CompiledMethod`] built now
/// carries as [`CompiledMethod::copy_record_lost`] (JIT round 21 wave 25,
/// lane initwitness). A flag set with no compilation open is stale and is
/// cleared when the next outermost witness opens.
pub(crate) fn current_compile_copy_record_lost() -> bool {
    COMPILE_CP_STAMP.try_with(|c| c.copy_lost.get()).unwrap_or(false)
}

/// The classes the compilation open on this thread has copied bytecode of so
/// far, sorted; empty when none is open. What a [`CompiledMethod`] built now
/// carries as [`CompiledMethod::copied_classes`].
pub(crate) fn current_compile_copied_classes() -> Box<[u32]> {
    let mut copied: Vec<u32> = COMPILE_CP_STAMP
        .try_with(|c| {
            c.try_borrow()
                .ok()
                .and_then(|scope| scope.copied.clone())
                .unwrap_or_default()
        })
        .unwrap_or_default();
    copied.sort_unstable();
    copied.into_boxed_slice()
}

/// The word compiled code passes as the `holder_class_id` argument of a
/// `(vm, holder, cp index)` helper (`new_object_cp`, `ldc_class_cp`,
/// `ldc_string_cp`, `anewarray_object_cp`): the holder class id in the low
/// 32 bits, and in the high 32 bits the constant-pool stamp of the code
/// ([`current_compile_cp_stamp`]) PLUS ONE, i.e. which generation of the
/// holder's constant pool `cp_idx` indexes; `0` there means unknown (no
/// source, or a count past 32 bits). Decoded by [`cp_holder_word_parts`]. A
/// body still running after its class was redefined translates its index
/// through the class's redefinition history before it reads the pool
/// (`vm::jit::helpers::cp_site_index`); a helper that ignores the high half
/// reads the id exactly as before (interpreter round i1 wave 20, lane L1,
/// `i19-L3-compiled-bodies-of-a-redefined-class-resolve-old-indices-in-the-new-pool`).
/// Since wave 21 an unbound type-check site's identity keeps its holder in
/// this form too ([`typecheck_holder_word_for_site`]); a `multianewarray`
/// site word carries a truncated stamp of its own
/// ([`pack_multianewarray_site`]).
pub fn cp_holder_word(holder_class_id: u32) -> u64 {
    let stamp = current_compile_cp_stamp()
        .and_then(|stamp| u32::try_from(stamp.checked_add(1)?).ok())
        .unwrap_or(0);
    u64::from(holder_class_id) | (u64::from(stamp) << 32)
}

/// `(holder class id, constant-pool stamp)` of a [`cp_holder_word`]; the
/// stamp is `None` when the word carries none.
pub fn cp_holder_word_parts(word: u64) -> (u32, Option<u64>) {
    // Cast: the low half is the class id, by construction.
    let holder = word as u32;
    let stamp = (word >> 32).checked_sub(1);
    (holder, stamp)
}

/// Open a compilation scope, stamping every [`CompiledMethod`] built inside it
/// with the epoch as of *now* rather than as of buffer finalize.
///
/// [`try_compile_with_invokespecial_resolver`] opens one around the whole
/// pipeline. A backend entered directly (the OSR compile in
/// `vm/src/runtime/interpreter/invoke.rs` calls `x64::compile_with_param_slots`
/// without going through `try_compile`) simply gets the narrower
/// finalize-to-publish window, which is still a real gate — see
/// `docs/jit/code-cache-lifetime.md` for the one-line change that widens it.
pub fn open_compile_epoch_witness() -> CompileEpochWitness {
    let (prev_cp_stamp, opened_copies, prev_layout_epoch) =
        COMPILE_CP_STAMP.with(|c| match c.try_borrow_mut() {
            Ok(mut scope) => {
                let prev = scope.cp_stamp;
                if prev.is_none() {
                    scope.cp_stamp = CP_STAMP_SOURCE.get().map(|source| source());
                }
                // Wave 25 (lane L6): the outermost witness opens the list of the
                // classes the compilation copies; a nested one shares it.
                let opened = scope.copied.is_none();
                if opened {
                    scope.copied = Some(Vec::new());
                    // Wave 25 (lane initwitness): a fresh list has lost no
                    // record yet.
                    c.copy_lost.set(false);
                }
                // Round 16 (lane fieldguard, FG-b3): the outermost witness
                // snapshots the layout-replacement epoch. Every door opens its
                // witness before it reads the bytecode, so before it resolves
                // a single compact field row; a nested compile keeps the
                // outer (older) value, which can only fail more guards.
                let prev_layout = scope.layout_epoch;
                if prev_layout.is_none() && layout_epoch_snapshot_enabled() {
                    scope.layout_epoch = Some(cratonvm_types::layout_replace_epoch());
                }
                (prev, opened, prev_layout)
            }
            Err(_) => (None, false, None),
        });
    COMPILE_INSTALL_EPOCH.with(|c| {
        let prev = c.get();
        if prev.is_none() {
            c.set(Some((jit_install_epoch(), jit_invalidation_epoch())));
        }
        CompileEpochWitness {
            prev,
            prev_cp_stamp,
            opened_copies,
            prev_layout_epoch,
        }
    })
}

/// `CRATONVM_JIT_LAYOUT_EPOCH_SNAPSHOT` (round 16, lane fieldguard, FG-b3) --
/// **default ON**, `0` is the kill switch: the outermost compile-epoch witness
/// snapshots `LAYOUT_REPLACE_EPOCH`, and every inline layout guard of that
/// compilation bakes the snapshot instead of the epoch read when the guard is
/// emitted. Read once per witness; no static.
pub(crate) fn layout_epoch_snapshot_enabled() -> bool {
    true
}

/// The layout-replacement epoch the compilation open on this thread began at,
/// or `None` (no compilation open, or the snapshot switched off).
pub(crate) fn compile_layout_epoch_snapshot() -> Option<u32> {
    COMPILE_CP_STAMP
        .try_with(|c| c.try_borrow().ok().and_then(|scope| scope.layout_epoch))
        .ok()
        .flatten()
}

/// The epoch an inline layout guard emitted NOW must compare against: the open
/// compilation's snapshot, else `live` (the value read at emission, the
/// behaviour before round 16).
///
/// # Why the snapshot (FG-b3)
///
/// A guard's baked value vouches for the compact offsets the compilation
/// resolved (`compact_field_disp` through the doors' field resolvers), and
/// those rows are resolved when the compilation BEGINS, while the guard used
/// to read the epoch when it was EMITTED, later. A layout replacement landing
/// between the two left code whose guard passes (it baked the new epoch) with
/// offsets from the old layout -- for the life of the body. Baking the epoch as
/// of before the first row was resolved makes that replacement fail the guard
/// (helper or deopt) instead. Every emitter asks this: the optimizing tier's
/// `Lowerer::emit_layout_epoch_cmp` and the single-pass
/// `Compiler::emit_layout_epoch_guard` (top level and inlined callees alike).
pub(crate) fn compile_baked_layout_epoch(live: u32) -> u32 {
    compile_layout_epoch_snapshot().unwrap_or(live)
}

/// The epoch a `CompiledMethod` built right now must be stamped with: the open
/// compilation's start epoch, or (no compilation open) the live epoch.
#[inline]
pub(crate) fn current_compile_install_epoch() -> u64 {
    COMPILE_INSTALL_EPOCH
        .with(|c| c.get())
        .map_or_else(jit_install_epoch, |(install, _)| install)
}

/// The invalidation epoch a `CompiledMethod` built right now must be stamped
/// with: the open compilation's start epoch, or (no compilation open) the live
/// epoch.
#[inline]
pub(crate) fn current_compile_invalidation_epoch() -> u64 {
    COMPILE_INSTALL_EPOCH
        .with(|c| c.get())
        .map_or_else(jit_invalidation_epoch, |(_, invalidation)| invalidation)
}

/// What one [`JitCache`] invalidation withdrew, kept in that cache's
/// invalidation log so a publication whose compilation began earlier can be
/// checked against it.
pub(crate) enum InvalidationRecord {
    /// `invalidate_for_class_change` / `invalidate_for_class`: every body whose
    /// `inlined_methods` names this class — a callee it inlined, or the
    /// receiver class a guarded speculation relied on.
    InlinedClass(Arc<str>),
    /// `invalidate_for_new_subtype` (interpreter round i4 wave 88, lane L7,
    /// `CRATONVM_JIT_SUBTYPE_DEFINE_SCOPED`): `class_name` gained a subtype.
    /// Withdraws a body whose `inlined_methods` names the class with a member
    /// a new subtype CAN change the binding of -- every member but those in
    /// `fixed` (constructors, initialisers, and the class's static, private
    /// and final methods: a new subtype overrides none of them, so no
    /// inlined copy or devirtualisation of them goes stale).
    NewSubtypeOf {
        class_name: Arc<str>,
        fixed: Arc<[(Arc<str>, Arc<str>)]>,
    },
    NewSubtypeIntroducing {
        class_name: Arc<str>,
        fixed: Arc<[(Arc<str>, Arc<str>)]>,
        introduced: Arc<[(Arc<str>, Arc<str>)]>,
    },
    /// `invalidate_unloaded_class`: bodies the class declares, and bodies that
    /// inlined from it.
    UnloadedClass {
        class_id: cratonvm_types::ClassId,
        class_name: Arc<str>,
    },
    /// `remove`: one method key.
    Key {
        class_name: Arc<str>,
        method_name: Arc<str>,
        descriptor: Arc<str>,
        declaring_class_id: cratonvm_types::ClassId,
    },
    /// `invalidate_for_redefinition`: bodies the redefined class declares,
    /// and bodies whose `inlined_methods` name it. A compile that read the
    /// class's old bytecode and publishes after the scoped eviction must not
    /// survive it; the full flush's barrier did this job before (interpreter
    /// round i1 wave 20, lane L1).
    RedefinedClass {
        class_id: cratonvm_types::ClassId,
        class_name: Arc<str>,
    },
    /// `invalidate_for_finalizable_subtype` (JIT round 21 wave 13, lane
    /// finalize, `CRATONVM_FINALIZER_ANCESTOR_CTOR_STAYS_INTERPRETED`): the
    /// class gained a FINALIZABLE subtype and has a constructor that can throw
    /// before its own `super(..)` / `this(..)`. Withdraws the class's own
    /// constructor bodies, every body whose `inlined_methods` names one of its
    /// constructors, and every body that copied its bytecode.
    FinalizableSubtypeOf {
        class_id: cratonvm_types::ClassId,
        class_name: Arc<str>,
    },
    /// [`JitCache::refuse_in_flight_for_object_observer`] (t1 wave 5, lane M;
    /// `CRATONVM_JIT_OBSERVER_GAIN_REFUSES_IN_FLIGHT`): the VM gained an
    /// object observer (a live-phase `can_tag_objects`). Every body whose
    /// compilation BEGAN before it is refused at publication: its request may
    /// have read "no observer" and scalar-replaced allocations a heap walk
    /// must now see. HotSpot's `ciEnv::jvmti_state_changed` fails every such
    /// compile ("JVMTI state has changed during compilation") whatever it
    /// contains; the method is compiled again under the new state. Logged
    /// only: bodies already installed stay, as HotSpot's do.
    ObjectObserverGained,
}

impl InvalidationRecord {
    /// Whether this invalidation withdraws a body published under `key`.
    pub(crate) fn withdraws(&self, key: &JitKey, compiled: &CompiledMethod) -> bool {
        let inlined_from = |class: &str| {
            compiled
                .inlined_methods
                .iter()
                .any(|(cls, _, _)| cls == class)
        };
        match self {
            Self::InlinedClass(class) => inlined_from(class.as_ref()),
            Self::NewSubtypeOf { class_name, fixed } => {
                names_subtype_sensitive_member(compiled, class_name, fixed)
            }
            Self::NewSubtypeIntroducing {
                class_name,
                fixed,
                introduced,
            } => names_subtype_sensitive_introduced_member(compiled, class_name, fixed, introduced),
            // Round 13 wave 12 (lane guardsplice): and a body that spliced the
            // class's bytecode (`copied_classes`), which the IR tier records
            // instead of `inlined_methods`.
            Self::UnloadedClass {
                class_id,
                class_name,
            } => {
                key.declaring_class_id == *class_id
                    || inlined_from(class_name.as_ref())
                    || (class_id.as_u32() != 0
                        && (compiled.copied_bytecode_of(*class_id)
                            // JIT round 22 wave 2, lane review: a lost copy
                            // record may be this class's.
                            || (compiled.copy_record_lost)))
            }
            // Wave 25 (lane L6): a compile that copied the class's bytecode
            // too (`CompiledMethod::copied_classes`).
            Self::RedefinedClass {
                class_id,
                class_name,
            } => {
                key.declaring_class_id == *class_id
                    || inlined_from(class_name.as_ref())
                    || compiled.copied_bytecode_of(*class_id)
            }
            Self::Key {
                class_name,
                method_name,
                descriptor,
                declaring_class_id,
            } => {
                key.class_name == *class_name
                    && key.method_name == *method_name
                    && key.descriptor == *descriptor
                    && key.declaring_class_id == *declaring_class_id
            }
            Self::FinalizableSubtypeOf {
                class_id,
                class_name,
            } => withdraws_for_finalizable_subtype(key, compiled, *class_id, class_name, false),
            // t1 wave 5, lane M: every compile that began before the gain.
            Self::ObjectObserverGained => true,
        }
    }

    /// [`Self::withdraws`] as `cache` judges it: a finalizable-subtype record
    /// also asks the cache-wide copy set ([`JitCache::bytecode_was_copied`]),
    /// so a body whose own copy record was lost is still matched by name (JIT
    /// round 21 wave 24, lane flipprep, flip review item 7). Every other
    /// record is judged as by [`Self::withdraws`].
    pub(crate) fn withdraws_in(&self, cache: &JitCache, key: &JitKey, compiled: &CompiledMethod) -> bool {
        match self {
            Self::FinalizableSubtypeOf {
                class_id,
                class_name,
            } => withdraws_for_finalizable_subtype(
                key,
                compiled,
                *class_id,
                class_name,
                finalizer_copy_unrecorded_on_body(cache, *class_id),
            ),
            _ => self.withdraws(key, compiled),
        }
    }

    /// [`Self::withdraws_in`] for a body that WAS installed (a superseded
    /// callee, `JitCache::superseded_callee_still_callable`). t1 wave 6 (lane
    /// V5): an [`Self::ObjectObserverGained`] record refuses only compiles in
    /// flight at the gain; an installed body stays, as HotSpot's do
    /// (`ciEnv::jvmti_state_changed` fails compiles, it deoptimizes nothing).
    /// Judged by its compile-start stamp, every pre-gain callee read as
    /// withdrawn, and every caller compiled after the gain that baked one was
    /// dropped as `baked-callee-retired`.
    pub(crate) fn withdraws_installed_in(
        &self,
        cache: &JitCache,
        key: &JitKey,
        compiled: &CompiledMethod,
    ) -> bool {
        !matches!(self, Self::ObjectObserverGained) && self.withdraws_in(cache, key, compiled)
    }
}


/// May a body have copied `class_id`'s bytecode without recording it on its
/// own `copied_classes`? True when the cache-wide set
/// ([`JitCache::bytecode_was_copied`]) names the class: a copy made while the
/// open compile's scope could not be borrowed (`note_copy_in_open_compile`
/// failed on `try_borrow_mut`), or with no compile open on the thread, lands
/// only there. Only consulted under `CRATONVM_JIT_FINALIZER_WITHDRAW_BY_ID`
/// (JIT round 21 wave 24, lane flipprep, flip review item 7); a zero id never
/// is.
pub(crate) fn finalizer_copy_unrecorded_on_body(cache: &JitCache, class_id: cratonvm_types::ClassId) -> bool {
    class_id.as_u32() != 0 && finalizer_withdraw_by_id_enabled() && cache.bytecode_was_copied(class_id)
}

/// The match of [`JitCache::invalidate_for_finalizable_subtype`] and of its
/// log record (JIT round 21 wave 13, lane finalize): a constructor body the
/// class declares (by id, or by name for a key minted without one), a body
/// whose `inlined_methods` names one of the class's constructors (the
/// single-pass splice), or a body that copied the class's bytecode (the IR
/// tier records a splice there instead).
///
/// `copy_unrecorded` ([`finalizer_copy_unrecorded_on_body`]): the cache-wide
/// copy set names the class, so a body's `copied_classes` may be missing a
/// copy it made; under the by-id rule its `inlined_methods` name evidence then
/// counts even when `copied_classes` is not empty (JIT round 21 wave 24, lane
/// flipprep, flip review item 7). It only widens the withdrawal.
pub(crate) fn withdraws_for_finalizable_subtype(
    key: &JitKey,
    compiled: &CompiledMethod,
    class_id: cratonvm_types::ClassId,
    class_name: &str,
    copy_unrecorded: bool,
) -> bool {
    // JIT round 21 wave 17 (lane jit-selfcopy, w16 review item 6): with the
    // class's id known, match by id, so a same-named class of ANOTHER loader
    // (two web apps' `p/Base`) keeps its constructor bodies. The name stays
    // the evidence only where no id is: a key minted without one, or a body
    // whose compile recorded no copied class at all (built with no compile
    // scope open, so `copied_classes` cannot speak for its splices; the VM
    // resolver marks every spliced body's declaring class there otherwise).
    if class_id.as_u32() != 0 && finalizer_withdraw_by_id_enabled() {
        let own_ctor = &*key.method_name == "<init>"
            && (key.declaring_class_id == class_id
                || (key.declaring_class_id.as_u32() == 0 && &*key.class_name == class_name));
        // Wave 25 (lane initwitness, flip review item 7): a body whose own
        // compile lost a copy record answers by name too, whatever the
        // cache-wide set says.
        let spliced_by_name = (compiled.copied_classes.is_empty()
            || copy_unrecorded
            || compiled.copy_record_lost)
            && compiled
                .inlined_methods
                .iter()
                .any(|(cls, method, _)| cls == class_name && method == "<init>");
        return own_ctor || spliced_by_name || compiled.copied_bytecode_of(class_id);
    }
    let own_ctor = &*key.method_name == "<init>"
        && ((class_id.as_u32() != 0 && key.declaring_class_id == class_id)
            || &*key.class_name == class_name);
    own_ctor
        || compiled
            .inlined_methods
            .iter()
            .any(|(cls, method, _)| cls == class_name && method == "<init>")
        || (class_id.as_u32() != 0 && compiled.copied_bytecode_of(class_id))
}

/// JIT round 21 wave 17 (lane jit-selfcopy, w16 review item 6): the
/// finalizable-subtype withdrawal matches by class id when it has one
/// ([`withdraws_for_finalizable_subtype`]). **Opt-in** (`=1`): a perf fix (it
/// stops other loaders' same-named constructors churning), and it narrows a
/// withdrawal, so it ships off until the finalizer probes ran with it. Read
/// per withdrawal and per publication check against the log record (cold).
pub(crate) fn finalizer_withdraw_by_id_enabled() -> bool {
    false
}

/// Does `compiled` depend on a member of `class_name` whose binding a NEW
/// SUBTYPE of the class can change: an `inlined_methods` triple naming the
/// class with a `(method, descriptor)` that is not in `fixed` (the class's
/// constructors, initialiser, and static / private / final methods, which no
/// subtype overrides) and is not a constructor or initialiser by name? The
/// match of [`JitCache::invalidate_for_new_subtype`] and of its log record
/// (interpreter round i4 wave 88, lane L7).
///
/// Why the name alone cannot decide: a triple is either the class of a
/// spliced callee (`InlineDependency::InlinedCallee`) or the static /
/// receiver type of a speculation (`SpeculatedReceiver`,
/// `UniqueConcreteMethod`), and only the last two (and an inlined VIRTUAL
/// callee, conservatively) are about the class's subtree. Every constructor
/// inlines its superclass constructors down to `java/lang/Object.<init>`, so
/// matching by class name alone retired every compiled constructor -- and
/// every method that inlined one -- on every class define, since every new
/// class has `java/lang/Object` in its supertype closure.
pub(crate) fn names_subtype_sensitive_member(
    compiled: &CompiledMethod,
    class_name: &str,
    fixed: &[(Arc<str>, Arc<str>)],
) -> bool {
    compiled.inlined_methods.iter().any(|(cls, method, desc)| {
        cls == class_name
            && subtype_sensitive_member(method, desc, fixed)
    })
}

pub(crate) fn names_subtype_sensitive_introduced_member(
    compiled: &CompiledMethod,
    class_name: &str,
    fixed: &[(Arc<str>, Arc<str>)],
    introduced: &[(Arc<str>, Arc<str>)],
) -> bool {
    compiled.inlined_methods.iter().any(|(cls, method, desc)| {
        cls == class_name
            && subtype_sensitive_member(method, desc, fixed)
            && introduced
                .iter()
                .any(|(m, d)| m.as_ref() == method && d.as_ref() == desc)
    })
}

pub(crate) fn subtype_sensitive_member(method: &str, desc: &str, fixed: &[(Arc<str>, Arc<str>)]) -> bool {
    method != "<init>"
        && method != "<clinit>"
        && method != DECLARING_CLASS_REPLACE_MARKER
        && !fixed.iter().any(|(m, d)| m.as_ref() == method && d.as_ref() == desc)
}




/// Can a redefinition of `(class_id, class_name)` make the body published
/// under `key` stale by itself: does the class declare it, or does its
/// `inlined_methods` name the class? The seed of the scoped eviction
/// ([`JitCache::invalidate_for_redefinition`]) and of its census
/// ([`JitCache::redefinition_dependents`]); the direct-call closure is added
/// by the caller. `inlined_methods` is matched by NAME, so a same-named class
/// of another loader counts too, the safe direction.
pub(crate) fn depends_on_redefined_class(
    key: &JitKey,
    compiled: &CompiledMethod,
    class_id: cratonvm_types::ClassId,
    class_name: &str,
) -> bool {
    key.declaring_class_id == class_id
        || compiled
            .inlined_methods
            .iter()
            .any(|(cn, _, _)| cn == class_name)
        // Wave 25 (lane L6): a class whose bytecode its compile copied.
        || compiled.copied_bytecode_of(class_id)
}

/// Stage 3 of `i20-L1-proposal-per-body-copied-bytecode-dependencies`
/// (interpreter round i1 wave 25, lane L6). `true`: a copy of a class's
/// bytecode made while a compilation is open on the copying thread is
/// recorded only on that compilation's bodies
/// ([`CompiledMethod::copied_classes`]), so a redefinition of the class stays
/// scoped to its dependents (`JitRealm::scoped_redefinition_admitted` asks
/// [`JitCache::bytecode_was_copied`], which then answers only for copies made
/// outside one). Default `true` since interpreter round i1 wave 29 (triage:
/// the hotswap probes, the jdk-only corpus and the core suite ran with it
/// on). `false` restores the wave-20 behaviour: every copy also marks the
/// class, and a redefinition of a copied class takes the whole-cache path;
/// the per-body list is recorded and matched anyway, which changes no answer
/// then.
pub const SCOPE_REDEFINITIONS_BY_COPIED_CLASSES: bool = true;







/// Does a redefinition of `(class_id, class_name)` make `body` stale, whether
/// or not it is published: it declares the class (its publication stamped
/// `owner_class_id`), inlined from it, or is one of `reached`, the bodies
/// that call into it ([`JitCache::redefinition_call_dependents`])? The test a
/// cell or an inline-cache slot's retained body is judged by (interpreter
/// round i1 wave 21, lane L1); [`depends_on_redefined_class`] is the
/// published-body half, keyed by the cache key.
pub(crate) fn redefinition_makes_stale(
    body: &CompiledMethod,
    class_id: cratonvm_types::ClassId,
    class_name: &str,
    reached: &FxHashSet<u64>,
) -> bool {
    body.owner_class_id == class_id.as_u32()
        || body
            .inlined_methods
            .iter()
            .any(|(cn, _, _)| cn == class_name)
        || body.copied_bytecode_of(class_id)
        || reached.contains(&body.artifact_id)
}

/// A bounded log of one cache's invalidations, oldest first.
#[derive(Default)]
pub(crate) struct InvalidationLog {
    pub(crate) records: std::collections::VecDeque<(u64, InvalidationRecord)>,
    /// The newest epoch that has fallen off the front of `records`. A
    /// compilation stamped below it can no longer be checked, so it is refused.
    pub(crate) dropped_through: u64,
}

/// Records one [`JitCache`] keeps before the oldest falls off. A compilation
/// that stays in flight across more invalidations than this is refused and
/// recompiles.
pub(crate) const INVALIDATION_LOG_CAPACITY: usize = 16_384;

/// Publications refused because an invalidation that began after their
/// compilation withdrew something they depend on.
pub(crate) static STALE_DEPENDENCY_REFUSALS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Number of artifacts refused publication because a later invalidation of the
/// same cache withdrew a class they inlined or speculated on, their own key, or
/// their unloaded declaring class.
pub fn stale_dependency_refusals() -> u64 {
    STALE_DEPENDENCY_REFUSALS.load(std::sync::atomic::Ordering::Relaxed)
}

/// Publications refused because the compilation predated a cache flush.
pub(crate) static STALE_INSTALL_EPOCH_REFUSALS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Number of artifacts refused publication for having been compiled against
/// bytecode a later redefinition or layout upgrade replaced.
pub fn stale_install_epoch_refusals() -> u64 {
    STALE_INSTALL_EPOCH_REFUSALS.load(std::sync::atomic::Ordering::Relaxed)
}

/// Whether a compilation that predates a cache flush is refused publication.
///
/// DEFAULT-ON. Refusing costs one wasted compile: the method stays interpreted
/// and recompiles on a later invocation, against the bytecode that is actually
/// installed. Publishing anyway runs the code the redefinition replaced —
/// silently, because nothing downstream can tell a stale body from a current
/// one. `CRATONVM_JIT_STRICT_INSTALL_EPOCH=0` restores the historical
/// publish-anyway behaviour for bisection; the refusal is still counted.
pub(crate) fn strict_install_epoch_enabled() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        Err::<String, std::env::VarError>(std::env::VarError::NotPresent)
            .map(|v| v != "0")
            .unwrap_or(true)
    })
}

/// Monotonic publication/invalidation generation for external entry caches,
/// summed over every [`JitCache`] in the process.
///
/// Advanced by EVERY mutation of any cache — `put`, `put_osr`, any
/// `invalidate_*`, and `clear_all` — including a first-time insertion of a key
/// that was not previously present, and by the drop of every `CompiledMethod`
/// that owns `JitInvokeInfo`s. Two counters share the word: a first-time
/// publication adds 1, and a replacement, a withdrawal or such a drop adds
/// `1 << JIT_CACHE_RETIRE_SHIFT`. Either way the word changes, so an equality
/// check sees every mutation. Each cache also keeps its own
/// [`JitCache::generation`], which is what the interpreter's negative memos
/// read; this process-wide sum is for the thread-local raw-entry dispatch memos
/// in `vm/src/jit/helpers.rs`, which outlive any one VM.
///
/// That last case matters: until T2.2 the two `put` paths bumped only when they
/// *replaced* an existing entry, which was sufficient for the original consumer
/// (`vm/src/jit/helpers.rs`'s thread-local raw-entry dispatch caches, a purely
/// *positive* cache that a brand-new key cannot invalidate). The interpreter's
/// invoke-cache epoch check is a *negative* cache — "this method has no compiled
/// body" — and a first-time publication is precisely what falsifies it, so the
/// bump has to be unconditional. Under-bumping there would leave an interpreted
/// call site pinned to the interpreter forever after a background compile
/// published its body.
pub fn jit_cache_generation() -> u64 {
    JIT_CACHE_GENERATION.load(std::sync::atomic::Ordering::Acquire)
}

/// Publication buckets per [`JitCache`] ([`JitCache::callee_publish_stamp`]).
/// A power of two. Two names sharing a bucket only cost each other a
/// re-probe.
pub const CALLEE_PUBLISH_BUCKETS: usize = 1024;

/// The [`JitCache::callee_publish_stamp`] bucket of `method_name descriptor`:
/// FNV-1a over both, with a separator. The class name is left out (see that
/// method).
#[inline]
pub fn callee_publish_bucket(method_name: &str, descriptor: &str) -> usize {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut h = FNV_OFFSET;
    for part in [method_name, descriptor] {
        for &b in part.as_bytes() {
            h = (h ^ u64::from(b)).wrapping_mul(FNV_PRIME);
        }
        h = (h ^ 0xff).wrapping_mul(FNV_PRIME);
    }
    // Truncation: the low bits index the bucket table.
    (h ^ (h >> 32)) as usize & (CALLEE_PUBLISH_BUCKETS - 1)
}

