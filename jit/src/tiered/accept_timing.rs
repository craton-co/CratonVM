// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
//! j27 wave 6, lane T1TIERMGR: TIERMGR21-4 of
//! `docs/internal/proposals/jit/tier1/jit-r21-tiermgr-proposals.md`, which is
//! step 2 of ACCEPT21-1 (`jit-r21-accept-proposals.md`) -- the TIMING HOOK
//! only. The side slot that holds a refused optimizing body and the harness
//! that calls both bodies are the accept book's (wave 7).
//!
//! What this gives the accept lane:
//!
//! - a per-manager (so per-VM) queue of methods whose lowered optimizing body
//!   the evidence gate refused, fed by
//!   [`TieredCompilationManager::note_accept_refused`] while
//!   `CRATONVM_DBG_ACCEPT_CENSUS` is on and a timer is installed; each method
//!   is queued at most once ([`MAX_ACCEPT_TIMED_METHODS`]) and at most
//!   [`MAX_ACCEPT_TIMING_PENDING`] wait at once;
//! - the timer slot, [`TieredCompilationManager::set_accept_timing_hook`]: the
//!   VM's READER of one method's timings ([`AcceptTimingSample`]). It must
//!   NOT call Java code: it runs on a compile worker (not a Java thread), and
//!   replaying a method would rerun its side effects with stale arguments
//!   (`docs/internal/jit/j27-accept-timing-replay-harness-reruns-side-effects-FIXED-20261029.md`).
//!   The timings come from the program's own routed calls
//!   (`crate::accept_measure::route_entry` / `MeasureWindow`), handed over by
//!   `crate::accept_measure::timing_sample`;
//! - the decision point: an idle C1-lane worker reads ONE queued method per
//!   idle tick, with no manager lock held and inside the compile panic
//!   containment, and prints `[accept-census] timing <method> c1=<ns>
//!   c2=<ns> calls=<n> faster=<c1|c2|tie>` ([`accept_timing_line`]).
//!
//! Nothing here changes what is published. With no timer installed (every
//! build until wave 7 installs one) `note_accept_refused` queues nothing and
//! the idle tick does nothing.

use std::collections::VecDeque;
use std::sync::Arc;

use parking_lot::Mutex;
use rustc_hash::FxHashSet;

use super::{CompilerCore, MethodKey, TieredCompilationManager};

/// One method's two bodies, timed over the same `calls` calls each.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AcceptTimingSample {
    /// Total nanoseconds of `calls` calls of the single-pass body.
    pub single_pass_ns: u64,
    /// Total nanoseconds of `calls` calls of the refused optimizing body.
    pub optimizing_ns: u64,
    /// Calls of each body.
    pub calls: u32,
}

impl AcceptTimingSample {
    /// Nanoseconds per call of a total over [`Self::calls`]; `0` for no calls.
    pub fn per_call(&self, total_ns: u64) -> u64 {
        if self.calls == 0 {
            return 0;
        }
        total_ns / u64::from(self.calls)
    }

    /// Which body was faster per call: `c1` (single-pass), `c2` (the refused
    /// optimizing body -- a gate error) or `tie`. One rule with the measured
    /// window's verdict: `crate::accept_measure::compare_medians` (its
    /// `TIE_MARGIN_PERCENT` tie margin), so the two census lines agree.
    pub fn faster(&self) -> &'static str {
        use crate::accept_measure::{compare_medians, MeasuredVerdict};
        let c1 = self.per_call(self.single_pass_ns);
        let c2 = self.per_call(self.optimizing_ns);
        match compare_medians(c1, c2) {
            MeasuredVerdict::SinglePassFaster => "c1",
            MeasuredVerdict::OptimizingFaster => "c2",
            MeasuredVerdict::Tie | MeasuredVerdict::Pending => "tie",
        }
    }
}

/// The VM's reader of one gate-refused method's routed-call timings (never a
/// replay of the method): `None` while its measure window is pending or
/// absent, or the VM is gone.
pub type AcceptTimingFn =
    Box<dyn Fn(&MethodKey) -> Option<AcceptTimingSample> + Send + Sync + 'static>;

/// Methods waiting to be timed at once.
pub const MAX_ACCEPT_TIMING_PENDING: usize = 64;

/// Methods ever queued per manager (each is timed once).
pub const MAX_ACCEPT_TIMED_METHODS: usize = 1024;

type SharedTimer = Arc<dyn Fn(&MethodKey) -> Option<AcceptTimingSample> + Send + Sync + 'static>;

/// The per-core timing state ([`CompilerCore`]'s `accept_timing`).
#[derive(Default)]
pub(super) struct AcceptTiming {
    /// Cleared at shutdown: the VM's timer holds a `Weak` of the VM and the
    /// core can outlive it (as `idle_tick`).
    timer: Mutex<Option<SharedTimer>>,
    queue: Mutex<AcceptTimingQueue>,
}

#[derive(Default)]
struct AcceptTimingQueue {
    pending: VecDeque<MethodKey>,
    noted: FxHashSet<MethodKey>,
}

impl AcceptTimingQueue {
    /// Queue `key` unless it was queued before or a bound is reached.
    fn push(&mut self, key: &MethodKey) -> bool {
        if self.pending.len() >= MAX_ACCEPT_TIMING_PENDING
            || self.noted.len() >= MAX_ACCEPT_TIMED_METHODS
            || !self.noted.insert(key.clone())
        {
            return false;
        }
        self.pending.push_back(key.clone());
        true
    }
}

impl AcceptTiming {
    /// Install `timer`; the first one wins. `true` when installed.
    fn install(&self, timer: AcceptTimingFn) -> bool {
        let mut slot = self.timer.lock();
        if slot.is_some() {
            return false;
        }
        *slot = Some(Arc::from(timer));
        true
    }

    /// Drop the timer (shutdown). Queued methods stay, untimed.
    pub(super) fn clear(&self) {
        *self.timer.lock() = None;
    }

    /// Queue `key` for timing; only while a timer is installed.
    fn note(&self, key: &MethodKey) -> bool {
        if self.timer.lock().is_none() {
            return false;
        }
        self.queue.lock().push(key)
    }

    /// Time the oldest queued method and return its census line. Neither lock
    /// is held while the timer runs.
    fn run_one(&self) -> Option<String> {
        let timer = self.timer.lock().clone()?;
        let key = self.queue.lock().pending.pop_front()?;
        let sample = (*timer)(&key);
        let method = format!("{}.{}{}", key.class_name, key.method_name, key.descriptor);
        Some(accept_timing_line(&method, sample))
    }
}

/// The `[accept-census] timing` line for `method` (`Class.name(desc)`).
/// `c1` is the single-pass body, `c2` the refused optimizing body, both in ns
/// per call.
pub fn accept_timing_line(method: &str, sample: Option<AcceptTimingSample>) -> String {
    match sample {
        Some(s) => format!(
            "[accept-census] timing {method} c1={} c2={} calls={} faster={}",
            s.per_call(s.single_pass_ns),
            s.per_call(s.optimizing_ns),
            s.calls,
            s.faster(),
        ),
        None => format!("[accept-census] timing {method} untimed"),
    }
}

impl CompilerCore {
    /// The idle tick's timing step: one queued method, if any. Called by an
    /// idle C1-lane worker with no lock of the loop held.
    pub(super) fn run_accept_timing_tick(&self) {
        if let Some(line) = self.accept_timing.run_one() {
            eprintln!("{line}");
        }
    }
}

impl TieredCompilationManager {
    /// Install the VM's timer of gate-refused bodies (TIERMGR21-4). The first
    /// one wins; `true` when this one was installed. Cleared by
    /// `BackgroundCompiler::shutdown`.
    pub fn set_accept_timing_hook(&self, timer: AcceptTimingFn) -> bool {
        self.core.accept_timing.install(timer)
    }

    /// The evidence gate refused `key`'s lowered optimizing body: queue it to
    /// be timed on an idle tick, and ask for that tick. Only under
    /// `CRATONVM_DBG_ACCEPT_CENSUS` with a timer installed; `true` when
    /// queued. Read once per refused body (cold).
    pub fn note_accept_refused(&self, key: &MethodKey) -> bool {
        if !crate::ir_evidence::accept_census_enabled() || !self.core.accept_timing.note(key) {
            return false;
        }
        self.core.request_idle_tick();
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    const CENSUS: &str = "CRATONVM_DBG_ACCEPT_CENSUS";

    fn census<R>(f: impl FnOnce() -> R) -> R {
        cratonvm_types::flags::with_thread_overrides(&[(CENSUS, Some("1"))], f)
    }

    fn key(name: &str) -> MethodKey {
        MethodKey::new("j27/AcceptTiming", name, "()I")
    }

    fn sample(c1: u64, c2: u64, calls: u32) -> AcceptTimingSample {
        AcceptTimingSample { single_pass_ns: c1, optimizing_ns: c2, calls }
    }

    #[test]
    fn the_line_names_both_bodies_per_call_and_the_faster_one() {
        assert_eq!(
            accept_timing_line("C.m()I", Some(sample(3_000, 1_000, 1_000))),
            "[accept-census] timing C.m()I c1=3 c2=1 calls=1000 faster=c2"
        );
        assert_eq!(sample(1_000, 3_000, 1_000).faster(), "c1");
        assert_eq!(sample(1_000, 1_499, 1_000).faster(), "tie", "same ns per call");
        // compare_medians' 10% margin: 105 vs 100 is a tie, 120 vs 100 is not.
        assert_eq!(sample(105, 100, 1).faster(), "tie", "within the margin");
        assert_eq!(sample(120, 100, 1).faster(), "c2");
        assert_eq!(sample(5, 9, 0).per_call(5), 0, "no calls");
        assert_eq!(accept_timing_line("C.m()I", None), "[accept-census] timing C.m()I untimed");
    }

    #[test]
    fn nothing_is_queued_without_a_timer_or_without_the_census() {
        let mgr = TieredCompilationManager::with_default_policy();
        assert!(!census(|| mgr.note_accept_refused(&key("noTimer"))), "no timer");
        assert!(mgr.set_accept_timing_hook(Box::new(|_: &MethodKey| None)));
        assert!(!mgr.set_accept_timing_hook(Box::new(|_: &MethodKey| None)), "first wins");
        cratonvm_types::flags::with_thread_overrides(&[(CENSUS, None)], || {
            assert!(!mgr.note_accept_refused(&key("noCensus")), "census off");
        });
        assert!(census(|| mgr.note_accept_refused(&key("queued"))));
    }

    #[test]
    fn each_method_is_timed_once_in_order_with_the_timer() {
        let mgr = TieredCompilationManager::with_default_policy();
        let calls = Arc::new(AtomicU32::new(0));
        let seen = Arc::clone(&calls);
        mgr.set_accept_timing_hook(Box::new(move |k: &MethodKey| {
            seen.fetch_add(1, Ordering::Relaxed);
            (&*k.method_name == "a").then(|| sample(2_000, 1_000, 100))
        }));
        census(|| {
            assert!(mgr.note_accept_refused(&key("a")));
            assert!(!mgr.note_accept_refused(&key("a")), "queued once");
            assert!(mgr.note_accept_refused(&key("b")));
        });
        let timing = &mgr.core.accept_timing;
        assert_eq!(
            timing.run_one().as_deref(),
            Some("[accept-census] timing j27/AcceptTiming.a()I c1=20 c2=10 calls=100 faster=c2")
        );
        assert_eq!(
            timing.run_one().as_deref(),
            Some("[accept-census] timing j27/AcceptTiming.b()I untimed")
        );
        assert_eq!(timing.run_one(), None, "drained");
        assert_eq!(calls.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn the_queue_is_bounded_and_a_cleared_timer_times_nothing() {
        let mut q = AcceptTimingQueue::default();
        for i in 0..MAX_ACCEPT_TIMING_PENDING {
            assert!(q.push(&key(&format!("m{i}"))));
        }
        assert!(!q.push(&key("over")), "pending bound");
        q.pending.clear();
        assert!(q.push(&key("room")));

        let mgr = TieredCompilationManager::with_default_policy();
        mgr.set_accept_timing_hook(Box::new(|_: &MethodKey| Some(sample(1, 1, 1))));
        assert!(census(|| mgr.note_accept_refused(&key("left"))));
        mgr.core.accept_timing.clear();
        assert_eq!(mgr.core.accept_timing.run_one(), None, "shutdown: untimed");
    }
}
