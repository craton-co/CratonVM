// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Per-VM `--jdk-only-report` sinks (t1 wave 3, lane Y).
//!
//! The JIT's compile-time refusal list (`JDK_ONLY_VIOLATIONS`), its two refusal
//! counters and its drop counter were process statics in `lib.rs`, and the VM's
//! JIT-helper fast-path sink (`JDK_ONLY_HELPER_VIOLATIONS`, its drop counter and
//! `JDK_ONLY_FASTPATH_REFUSALS`) were process statics in
//! `vm/src/jit/helpers/typecheck.rs`. Two `--jdk-only` VMs in one process --
//! or one after another, in an embedder -- therefore wrote one report, and a
//! VM's report named refusals another VM made. AGENTS.md: "No process globals
//! for compatibility state."
//!
//! Each VM now owns a [`JdkOnlyReportSinks`] on its `JitCache`
//! (`JitCache::jdk_only_sinks`). How each refusal reaches it:
//!
//! * **Compile-time thin-native binds** (`direct_native_helper`, about sixty
//!   call sites in the compile scan): `try_compile_request` enters a
//!   [`JdkOnlySinkScope`] for the request's [`CompileRequest::jdk_only_sinks`]
//!   (`crate::CompileRealm`), and the recorder writes to the innermost scope on
//!   the compiling thread. A compile with no VM in scope (the positional shim,
//!   unit tests) records into [`process_jdk_only_sinks`], as before.
//! * **VM-side recorders** (the OSR / eager first-call ladders, the JIT
//!   helpers' fast-path admission) hold the VM, and record into its sinks
//!   directly.
//! * **Inline-cache publications refused on a mutator thread**
//!   (`jit_entry_publishable`): the publication API carries a `jdk_only` bit
//!   and no VM, so every VM publication site (`vm/src/jit/helpers`, under
//!   `JdkOnly` only) opens a [`JdkOnlySinkScope`] for its own VM around the
//!   `update` / `install` calls (t1 wave 4, lane Y2:
//!   `jdk_only_ic_publication_scope`). Only a publication made with no VM in
//!   hand (unit tests calling the slot API directly) still lands in the
//!   process sink. Count only, no rows.
//! * **The guarded thin direct-helper gate's refusals and admissions** (per
//!   call, `vm/src/jit/helpers/fast_native.rs` `direct_helper_gate_refuses`):
//!   each thread registers a single-writer [`DirectHelperGateTally`] with the
//!   calling VM's sinks the first time it serves that VM, and bumps it without
//!   a lock prefix (t1 wave 6, lane Y3; they were summed over every thread of
//!   the process before).
//!
//! [`CompileRequest::jdk_only_sinks`]: crate::CompileRequest::jdk_only_sinks

use cratonvm_types::error::JdkOnlyViolation;
use std::sync::atomic::{AtomicU64, Ordering};

/// One bounded, de-duplicated violation list plus its exact event counter and
/// its drop counter. The semantics are the ones the two process sinks had:
/// `events` counts every refusal, `rows` holds at most `cap` distinct
/// violations (dedup by `Vec::contains` over retained rows), and `dropped`
/// counts violations that found no room (an upper bound on the missing distinct
/// rows; it errs upward).
#[derive(Default)]
pub struct JdkOnlyViolationSink {
    events: AtomicU64,
    rows: parking_lot::Mutex<Vec<JdkOnlyViolation>>,
    dropped: AtomicU64,
}

impl JdkOnlyViolationSink {
    /// Count one refusal event and, when there is one, retain its violation
    /// (bounded by `cap`).
    pub fn record(&self, violation: Option<JdkOnlyViolation>, cap: usize) {
        self.events.fetch_add(1, Ordering::Relaxed);
        let Some(violation) = violation else {
            return;
        };
        let mut rows = self.rows.lock();
        // The dedup test comes FIRST, so a full sink can tell "a repeat" from
        // "no room" and counts only the latter as a drop.
        if !rows.contains(&violation) {
            if rows.len() < cap {
                rows.push(violation);
            } else {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Exact refusal events.
    pub fn events(&self) -> u64 {
        self.events.load(Ordering::Relaxed)
    }

    /// Snapshot of the retained violations.
    pub fn rows(&self) -> Vec<JdkOnlyViolation> {
        self.rows.lock().clone()
    }

    /// How many distinct violations are retained.
    pub fn len(&self) -> usize {
        self.rows.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Violations that found no room.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Did this sink drop anything? Not `len == cap`: a run whose last
    /// distinct violation exactly fills the sink dropped nothing.
    pub fn saturated(&self) -> bool {
        self.dropped() > 0
    }
}

/// One thread's guarded thin direct-helper tally for ONE VM: how often the
/// callee-side `--jdk-only` gate refused / admitted a call
/// (`vm/src/jit/helpers/fast_native.rs` `direct_helper_gate_refuses`).
///
/// That gate runs on every call into ~25 guarded helpers under `--jdk-only`,
/// so the tally is single-writer: the thread that registered it (with
/// [`JdkOnlyReportSinks::register_direct_helper_tally`]) is its only writer and
/// increments with a relaxed load-add-store, no lock prefix (round 11's cure
/// for these two counters, kept). Readers on other threads only load.
///
/// t1 wave 6, lane Y3: these were two fields of the VM crate's per-thread
/// `JitCounterBlock`, summed over every thread of the process, so a VM's
/// report counted every other VM's calls too.
#[derive(Default)]
pub struct DirectHelperGateTally {
    refusals: AtomicU64,
    admissions: AtomicU64,
}

impl DirectHelperGateTally {
    /// The owning thread's +1. Sound only from the thread that registered
    /// this tally (single writer; see the type doc).
    #[inline]
    pub fn bump_from_owner(&self, refused: bool) {
        let c = if refused {
            &self.refusals
        } else {
            &self.admissions
        };
        c.store(c.load(Ordering::Relaxed).wrapping_add(1), Ordering::Relaxed);
    }

    pub fn refusals(&self) -> u64 {
        self.refusals.load(Ordering::Relaxed)
    }

    pub fn admissions(&self) -> u64 {
        self.admissions.load(Ordering::Relaxed)
    }
}

/// One VM's `--jdk-only-report` state that the JIT and its runtime helpers
/// feed. Owned by the VM's `JitCache`.
#[derive(Default)]
pub struct JdkOnlyReportSinks {
    /// Compile-time thin direct-native binds refused under `JdkOnly`
    /// (`NativeShadowsBytecode`, `native_kind: "jit-thin-direct-helper"`).
    pub jit_compile: JdkOnlyViolationSink,
    /// Inline-cache publications of an unowned (native/builtin) entry refused
    /// under `JdkOnly`. A count: the site has an address, not a name triple.
    pub jit_ic_native_refusals: AtomicU64,
    /// The JIT runtime helpers' by-name fast-path admissions refused under
    /// `JdkOnly` (recorded by the VM, `vm/src/jit/helpers/typecheck.rs`).
    pub jit_helper: JdkOnlyViolationSink,
    /// Every [`DirectHelperGateTally`] a thread registered for this VM (one
    /// per thread per VM since t1 wave 7). The report sums them. The
    /// registering thread also keeps its tally alive (`JitCounterBlock`), so
    /// a tally outlives neither reader. t1 wave 6, lane Y3.
    jit_direct_helper_gate: parking_lot::Mutex<Vec<std::sync::Arc<DirectHelperGateTally>>>,
}

impl JdkOnlyReportSinks {
    pub fn new() -> Self {
        Self::default()
    }

    /// A fresh tally for the calling thread, counted in this VM's report.
    /// Cold: once per thread per VM (the thread reuses it on a switch back).
    pub fn register_direct_helper_tally(&self) -> std::sync::Arc<DirectHelperGateTally> {
        let tally = std::sync::Arc::new(DirectHelperGateTally::default());
        self.jit_direct_helper_gate
            .lock()
            .push(std::sync::Arc::clone(&tally));
        tally
    }

    /// Guarded thin direct-helper calls this VM's gate refused under
    /// `JdkOnly`, over every thread that served it.
    pub fn direct_helper_refusals(&self) -> u64 {
        self.jit_direct_helper_gate
            .lock()
            .iter()
            .map(|t| t.refusals())
            .sum()
    }

    /// The admissions beside [`Self::direct_helper_refusals`].
    pub fn direct_helper_admissions(&self) -> u64 {
        self.jit_direct_helper_gate
            .lock()
            .iter()
            .map(|t| t.admissions())
            .sum()
    }

    pub fn ic_native_refusals(&self) -> u64 {
        self.jit_ic_native_refusals.load(Ordering::Relaxed)
    }

    /// The process sinks, unless `self` IS them (no double count).
    fn unscoped(&self) -> Option<&'static JdkOnlyReportSinks> {
        let p = process_jdk_only_sinks();
        (!std::ptr::eq(self, p)).then_some(p)
    }

    /// What a VM's `--jdk-only-report` shows for the JIT compile-time source:
    /// this VM's rows, then any row work with no VM in scope recorded (the
    /// positional compile shim) that this VM did not. The remainder is the
    /// only cross-VM leak left; it is empty unless a door compiles without a
    /// VM realm.
    pub fn compile_rows_for_report(&self) -> Vec<JdkOnlyViolation> {
        let mut rows = self.jit_compile.rows();
        if let Some(p) = self.unscoped() {
            for r in p.jit_compile.rows() {
                if !rows.contains(&r) {
                    rows.push(r);
                }
            }
        }
        rows
    }

    /// Compile-time refusal events for the report: this VM's plus the
    /// no-VM-in-scope remainder.
    pub fn compile_events_for_report(&self) -> u64 {
        self.jit_compile.events() + self.unscoped().map_or(0, |p| p.jit_compile.events())
    }

    /// Compile-time drops for the report, same composition.
    pub fn compile_dropped_for_report(&self) -> u64 {
        self.jit_compile.dropped() + self.unscoped().map_or(0, |p| p.jit_compile.dropped())
    }

    /// Inline-cache native refusals for the report: those made inside this
    /// VM's compiles and at its VM-side publication sites (each opens a scope
    /// for this VM), plus the process count of refusals made with no VM in
    /// scope at all (direct slot-API callers; see the module doc).
    pub fn ic_native_refusals_for_report(&self) -> u64 {
        self.ic_native_refusals() + self.unscoped().map_or(0, |p| p.ic_native_refusals())
    }
}

/// The sinks of work with no VM in scope: unit tests, the positional compile
/// shim, and inline-cache refusals made through the slot API with no VM scope
/// open. No VM owns it; a VM's report reads its own sinks (and may add this
/// one as the "no VM in scope" remainder).
pub fn process_jdk_only_sinks() -> &'static JdkOnlyReportSinks {
    static PROCESS: std::sync::OnceLock<JdkOnlyReportSinks> = std::sync::OnceLock::new();
    PROCESS.get_or_init(JdkOnlyReportSinks::new)
}

thread_local! {
    /// The innermost [`JdkOnlySinkScope`] on this thread, or null. A raw
    /// pointer because the scope borrows the sinks for its own lifetime and
    /// restores the previous value on drop (compiles nest: a callee compile
    /// inside a caller's).
    static CURRENT: std::cell::Cell<*const JdkOnlyReportSinks> =
        const { std::cell::Cell::new(std::ptr::null()) };
}

/// While alive, the JIT's recorders on this thread write to `sinks`.
pub struct JdkOnlySinkScope<'a> {
    previous: *const JdkOnlyReportSinks,
    _sinks: std::marker::PhantomData<&'a JdkOnlyReportSinks>,
}

impl<'a> JdkOnlySinkScope<'a> {
    pub fn enter(sinks: &'a JdkOnlyReportSinks) -> Self {
        let previous = CURRENT.with(|c| c.replace(sinks as *const JdkOnlyReportSinks));
        JdkOnlySinkScope {
            previous,
            _sinks: std::marker::PhantomData,
        }
    }
}

impl Drop for JdkOnlySinkScope<'_> {
    fn drop(&mut self) {
        CURRENT.with(|c| c.set(self.previous));
    }
}

/// Run `f` on the innermost scoped sinks of this thread, or on
/// [`process_jdk_only_sinks`] when no scope is open. Cold paths only.
pub fn with_current_jdk_only_sinks<R>(f: impl FnOnce(&JdkOnlyReportSinks) -> R) -> R {
    let ptr = CURRENT.with(|c| c.get());
    if ptr.is_null() {
        f(process_jdk_only_sinks())
    } else {
        // SAFETY: `ptr` was set by a live `JdkOnlySinkScope<'a>` on this
        // thread, which borrows the sinks for `'a` and clears the slot (back to
        // the previous scope's pointer) when it drops; scopes on one thread
        // drop in LIFO order, so a non-null slot always names a borrow that is
        // still alive.
        f(unsafe { &*ptr })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(m: &str) -> JdkOnlyViolation {
        JdkOnlyViolation::NativeShadowsBytecode {
            class: "a/B".to_string(),
            method: m.to_string(),
            descriptor: "()V".to_string(),
            native_kind: "jit-thin-direct-helper",
            class_origin: None,
        }
    }

    #[test]
    fn t1y_a_sink_dedups_caps_and_counts_drops() {
        let s = JdkOnlyViolationSink::default();
        s.record(Some(row("m")), 1);
        s.record(Some(row("m")), 1);
        s.record(Some(row("n")), 1);
        s.record(None, 1);
        assert_eq!(s.events(), 4);
        assert_eq!(s.len(), 1);
        assert_eq!(s.dropped(), 1);
        assert!(s.saturated());
    }

    #[test]
    fn t1y_scopes_route_to_the_innermost_vm_and_restore() {
        let vm_a = JdkOnlyReportSinks::new();
        let vm_b = JdkOnlyReportSinks::new();
        let process_before = process_jdk_only_sinks().jit_compile.events();
        {
            let _a = JdkOnlySinkScope::enter(&vm_a);
            with_current_jdk_only_sinks(|s| s.jit_compile.record(Some(row("a")), 8));
            {
                let _b = JdkOnlySinkScope::enter(&vm_b);
                with_current_jdk_only_sinks(|s| s.jit_compile.record(Some(row("b")), 8));
            }
            with_current_jdk_only_sinks(|s| s.jit_compile.record(None, 8));
        }
        assert_eq!(vm_a.jit_compile.events(), 2);
        assert_eq!(vm_a.jit_compile.rows(), vec![row("a")]);
        assert_eq!(vm_b.jit_compile.rows(), vec![row("b")]);
        // Nothing reached the process sink from inside the scopes. (Other
        // tests may record into it concurrently, so only `>=`.)
        assert!(process_jdk_only_sinks().jit_compile.events() >= process_before);
        let ptr = CURRENT.with(|c| c.get());
        assert!(ptr.is_null(), "every scope restored its predecessor");
    }

    /// t1 wave 6, lane Y3: the direct-helper gate tallies are per VM. Two VMs'
    /// sinks in one process each sum only the tallies registered with them,
    /// across threads.
    #[test]
    fn t1y3_direct_helper_tallies_sum_per_vm() {
        let vm_a = JdkOnlyReportSinks::new();
        let vm_b = JdkOnlyReportSinks::new();
        let a1 = vm_a.register_direct_helper_tally();
        a1.bump_from_owner(true);
        a1.bump_from_owner(false);
        std::thread::scope(|s| {
            s.spawn(|| {
                let a2 = vm_a.register_direct_helper_tally();
                a2.bump_from_owner(true);
                let b1 = vm_b.register_direct_helper_tally();
                b1.bump_from_owner(false);
                b1.bump_from_owner(false);
            });
        });
        assert_eq!(vm_a.direct_helper_refusals(), 2);
        assert_eq!(vm_a.direct_helper_admissions(), 1);
        assert_eq!(vm_b.direct_helper_refusals(), 0);
        assert_eq!(vm_b.direct_helper_admissions(), 2);
    }
}
