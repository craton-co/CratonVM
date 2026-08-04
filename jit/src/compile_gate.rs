// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The one door every backend entry point must pass through.
//!
//! The `osr-01` lane brief (retired to
//! `osr-01-entry-metadata-contract-RETIRED-20260804.md`) asks for "a single
//! entry point for *produce an OSR-capable artifact*, so the direct path in
//! `invoke.rs` and the `try_compile` path cannot drift again". This is that
//! entry point, in the shape `docs/feature-designs/jit-osr-entry-metadata.md`
//! step 3 settled on: **a shared gate function every door must call**, with the
//! drift made observable instead of invisible.
//!
//! # There are THREE doors, not two
//!
//! The brief says two. [`jit_force_interpret`](crate::jit_force_interpret)'s
//! own doc already names three, because it had to be taught the same lesson
//! from the other side:
//!
//! | Door | Where | Reaches the backend via |
//! |---|---|---|
//! | [`CompileDoor::MethodEntry`] | `try_compile_with_invokespecial_resolver` | `try_compile_inner` → `x64::compile_with_param_slots` |
//! | [`CompileDoor::EagerFirstCall`] | `execute`'s first-call compile (`vm/src/runtime/interpreter.rs`) | `x64::compile_with_param_slots` directly |
//! | [`CompileDoor::Osr`] | `compile_osr_artifact` (`vm/src/runtime/interpreter/jit_bridge.rs`) | `x64::compile_with_param_slots` directly |
//!
//! Only the first went through the admission checks. The other two grew
//! *hand-copied* subsets of them, each added reactively after its own bug:
//! the permanent bail-list (after 35 923 wasted pipelines on `Nat.inc`), the
//! bisect levers (after every bisect step on
//! `docs/known-issues/jit/annotation-scan-arrayread-sigsegv.md` read "no
//! effect" while 11 methods were still compiling), and the OSR-entry-rejected
//! memo (after 256 re-compiles over ten H2 operations). Three separate
//! discoveries of one fact.
//!
//! # What the gate asks, and why each one belongs to every door
//!
//! 1. **The kill switch** (`CRATONVM_DISABLE_JIT`). Previously checked by the
//!    OSR door and by the VM-side callers, but not by `try_compile` itself.
//! 2. **The permanent bail-list.** A method the backend already refused cannot
//!    start succeeding; re-running the pipeline is pure waste. This is the
//!    check `compile_osr_artifact` hand-copied.
//! 3. **The bisect levers** (`CRATONVM_JIT_DENY` / `CRATONVM_JIT_BISECT_ONLY`).
//!    A lever that cannot actually stop a compile makes a bisect *lie*, which
//!    is worse than a missing feature — see [`crate::jit_force_interpret`].
//! 4. **The code-cache cap.** Never checked by either direct door: an OSR or
//!    first-call compile could commit code past a cap the ordinary door was
//!    respecting.
//!
//! And two things the gate *does*, rather than asks:
//!
//! 5. **Opens the compile-epoch witness** ([`crate::open_compile_epoch_witness`])
//!    so the artifact is stamped from before the first constant-pool read, not
//!    from buffer finalize. The OSR door opened one, but ~1 000 lines late —
//!    after all of its class loading and CP resolution — so a redefinition
//!    landing in that window produced a body stamped current and published.
//! 6. **Clears the one-shot bail-site record** so a bail can only ever report
//!    its own cause.
//!
//! # How a future drift is stopped, and then caught
//!
//! Two layers, because they fail differently.
//!
//! **The type system stops the accident.**
//! `x64::compile_with_param_slots` takes `&CompileAdmission`, and the only way
//! to get one is [`admit`]. A fourth door written without the gate does not
//! compile. The brief this closes asked for the two paths to be unable to
//! "drift again", and a counter asserted zero by a test is *will be caught*,
//! not *cannot*; this is the *cannot* half.
//!
//! **The counter catches the deliberate bypass.** The `jit` crate's own tests
//! drive the backend with hand-built bytecode and no method identity, so they
//! need a way in: [`CompileAdmission::for_backend_test`]. That is a genuine
//! hole, so it is built not to hide anything — it does not open the thread
//! scope, so a backend entry made under it is still counted by
//! [`ungated_backend_entries`], which the VM asserts is zero over a real run.
//! Its name is what makes a production use greppable.
//!
//! Both layers are behaviour-named rather than source-scanning: a check that
//! grepped for `compile_with_param_slots(` would have died the day `x64.rs`
//! was split, as five checks in this repository did.

use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};

/// Which entry point is asking to compile.
///
/// Recorded per door so "the OSR path calls the gate" is a *measurement*
/// rather than a claim about the source text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompileDoor {
    /// `try_compile_with_invokespecial_resolver` — the ordinary tiering door.
    MethodEntry,
    /// The interpreter's eager first-call single-pass compile.
    EagerFirstCall,
    /// `compile_osr_artifact` — the on-stack-replacement door.
    Osr,
}

impl CompileDoor {
    /// Every door, for the tests and the diagnostic dump.
    pub const ALL: [CompileDoor; 3] = [
        CompileDoor::MethodEntry,
        CompileDoor::EagerFirstCall,
        CompileDoor::Osr,
    ];

    const fn index(self) -> usize {
        match self {
            CompileDoor::MethodEntry => 0,
            CompileDoor::EagerFirstCall => 1,
            CompileDoor::Osr => 2,
        }
    }

    /// Stable name for diagnostics.
    pub const fn label(self) -> &'static str {
        match self {
            CompileDoor::MethodEntry => "method-entry",
            CompileDoor::EagerFirstCall => "eager-first-call",
            CompileDoor::Osr => "osr",
        }
    }
}

/// Why the gate refused.
///
/// Carries no payload: every variant is a whole-method verdict whose inputs are
/// process state, and the method identity is the caller's. What a caller does
/// with the distinction differs — `PermanentlyBailListed` is forever,
/// `CodeCacheAtCapacity` is transient — so they are kept apart rather than
/// collapsed into a bool.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompileRefusal {
    /// `CRATONVM_DISABLE_JIT` — interpreter-only execution.
    JitDisabled,
    /// The backend already refused this method; the bytecode has not changed.
    PermanentlyBailListed,
    /// `CRATONVM_JIT_DENY` / `CRATONVM_JIT_BISECT_ONLY` force-interpret it.
    ForceInterpreted,
    /// Retained JIT code is at the configured cap. Transient: reclaiming a body
    /// restores headroom.
    CodeCacheAtCapacity,
}

impl CompileRefusal {
    /// Whether re-asking later could produce a different answer.
    ///
    /// The bail-list is the one verdict that is a pure function of the
    /// bytecode; the other three are configuration or resource state.
    pub const fn is_transient(self) -> bool {
        matches!(self, CompileRefusal::CodeCacheAtCapacity)
    }
}

impl std::fmt::Display for CompileRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            CompileRefusal::JitDisabled => "CRATONVM_DISABLE_JIT",
            CompileRefusal::PermanentlyBailListed => "permanently bail-listed",
            CompileRefusal::ForceInterpreted => "force-interpreted by a bisect lever",
            CompileRefusal::CodeCacheAtCapacity => "code-cache cap reached",
        };
        f.write_str(s)
    }
}

static ADMISSIONS: [AtomicU64; 3] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];
static REFUSALS: [AtomicU64; 3] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];
static UNGATED_BACKEND_ENTRIES: AtomicU64 = AtomicU64::new(0);

thread_local! {
    /// Depth of open admissions on this thread. A count rather than a flag
    /// because `callee_compiler` recursion nests one compile inside another.
    static OPEN_ADMISSIONS: Cell<u32> = const { Cell::new(0) };
}

/// Proof that a compile passed the admission gate.
///
/// Hold it for the whole compilation: it owns the compile-epoch witness, so
/// dropping it early re-narrows the install-epoch window this exists to widen.
#[must_use = "the admission token must stay alive for the whole compilation — \
              it owns the compile-epoch witness"]
pub struct CompileAdmission {
    door: CompileDoor,
    // Dropped after the counter below, which is what makes "an admission is
    // open" and "the epoch witness is open" the same interval.
    _epoch: crate::CompileEpochWitness,
    /// Whether this token incremented the thread scope. `false` only for
    /// [`CompileAdmission::for_backend_test`], whose whole point is that it
    /// does not — see there.
    opened_scope: bool,
}

impl CompileAdmission {
    /// Which door this admission was granted to.
    pub fn door(&self) -> CompileDoor {
        self.door
    }

    /// A token for a **test** that drives the backend directly.
    ///
    /// `x64::compile_with_param_slots` takes `&CompileAdmission`, which is what
    /// makes a door that skips the gate a *compile error* rather than a red
    /// test. The `jit` crate's own tests are not doors — they hand the backend
    /// hand-built bytecode with no method identity to admit — so they need a
    /// way in, and an integration test under `jit/tests/` is a separate crate,
    /// so `#[cfg(test)]` cannot provide it.
    ///
    /// This is therefore a real bypass, and it is built so that using it in
    /// production is still caught: it does **not** open the thread scope, so a
    /// backend entry made under it is still counted by
    /// [`ungated_backend_entries`], which the VM asserts is zero. The type
    /// system stops the accident; the counter stops the deliberate misuse. The
    /// name is what makes the second one greppable.
    #[doc(hidden)]
    pub fn for_backend_test() -> Self {
        Self {
            door: CompileDoor::MethodEntry,
            _epoch: crate::open_compile_epoch_witness(),
            opened_scope: false,
        }
    }
}

impl Drop for CompileAdmission {
    fn drop(&mut self) {
        if self.opened_scope {
            OPEN_ADMISSIONS.with(|c| c.set(c.get().saturating_sub(1)));
        }
    }
}

/// The whole-method checks that must also veto *reusing* an already-compiled
/// body, not merely producing a new one.
///
/// Split out of [`admit`] because the OSR door consults a cached artifact
/// before deciding to compile, and a `CRATONVM_JIT_DENY` that stops the compile
/// while the cached body keeps running is exactly the lie
/// [`crate::jit_force_interpret`] documents. The resource and bail-list checks
/// deliberately do NOT live here: refusing to *reuse* a cached body because the
/// code cache is full would cost throughput and buy nothing — the body is
/// already committed.
pub fn compiled_execution_forbidden(class_name: &str, method_name: &str) -> bool {
    jit_disabled() || crate::jit_force_interpret(class_name, method_name)
}

/// `CRATONVM_DISABLE_JIT`, read through the shared flag snapshot.
///
/// Same source as the VM's `env_cache::disable_jit`, so the two cannot
/// disagree; the jit crate needs its own accessor because the gate lives below
/// the VM.
fn jit_disabled() -> bool {
    cratonvm_types::flags().jit.disable_jit
}

/// Ask the gate whether `door` may compile this method now.
///
/// On `Ok` the returned token must be held for the whole compilation.
pub fn admit(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    door: CompileDoor,
) -> Result<CompileAdmission, CompileRefusal> {
    // Open the compilation scope FIRST, before any constant-pool resolver runs.
    // Every `CompiledMethod` built under it — including one built by a nested
    // `callee_compiler` compile on this thread — is stamped with the install
    // epoch as of right now, so a redefinition or layout upgrade that lands
    // while this compile is reading bytecode makes the result unpublishable.
    // See `CompiledMethod::install_epoch` and `JitCache::flush_barrier`.
    //
    // Held even on the refusal paths below only for as long as this function
    // runs: the witness is dropped with the `Err`, which is correct — no
    // artifact was built.
    let epoch = crate::open_compile_epoch_witness();

    let refusal = if jit_disabled() {
        Some(CompileRefusal::JitDisabled)
    } else if crate::is_jit_bail_listed(class_name, method_name, descriptor) {
        crate::note_jit_bail_shortcircuit();
        Some(CompileRefusal::PermanentlyBailListed)
    } else if crate::jit_force_interpret(class_name, method_name) {
        Some(CompileRefusal::ForceInterpreted)
    } else if crate::jit_code_cache_at_capacity() {
        crate::note_jit_code_cache_cap_refusal();
        Some(CompileRefusal::CodeCacheAtCapacity)
    } else {
        None
    };

    if let Some(r) = refusal {
        REFUSALS[door.index()].fetch_add(1, Ordering::Relaxed);
        return Err(r);
    }

    // Same one-shot discipline the ordinary door has always had for the
    // bail-site record: clear whatever the previous compile on this thread
    // left behind, so a bail below can only ever report its own cause.
    let _ = crate::take_jit_bail_site();

    ADMISSIONS[door.index()].fetch_add(1, Ordering::Relaxed);
    OPEN_ADMISSIONS.with(|c| c.set(c.get() + 1));
    Ok(CompileAdmission {
        door,
        _epoch: epoch,
        opened_scope: true,
    })
}

/// Compiles admitted through `door`.
pub fn admissions(door: CompileDoor) -> u64 {
    ADMISSIONS[door.index()].load(Ordering::Relaxed)
}

/// Compiles refused at `door`.
pub fn refusals(door: CompileDoor) -> u64 {
    REFUSALS[door.index()].load(Ordering::Relaxed)
}

/// Whether an admission is open on the current thread.
pub fn admission_is_open() -> bool {
    OPEN_ADMISSIONS.with(|c| c.get()) > 0
}

/// Backend entries taken with no admission open on the thread.
///
/// **Expected to stay zero in the VM.** Non-zero means a door reached the
/// backend without the gate — the exact drift this module exists to prevent.
/// Non-zero inside the `jit` crate's own tests is normal: a unit test calling
/// the backend directly is not a door.
pub fn ungated_backend_entries() -> u64 {
    UNGATED_BACKEND_ENTRIES.load(Ordering::Relaxed)
}

/// Called by the backend on entry. See [`ungated_backend_entries`].
pub(crate) fn note_backend_entry() {
    if !admission_is_open() {
        UNGATED_BACKEND_ENTRIES.fetch_add(1, Ordering::Relaxed);
    }
}

/// Test support: zero every counter.
#[cfg(test)]
pub(crate) fn reset_for_test() {
    for d in CompileDoor::ALL {
        ADMISSIONS[d.index()].store(0, Ordering::Relaxed);
        REFUSALS[d.index()].store(0, Ordering::Relaxed);
    }
    UNGATED_BACKEND_ENTRIES.store(0, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A name no production class can collide with, so these tests can mark the
    /// bail-list without perturbing another test in the same binary.
    fn unique(tag: &str) -> String {
        format!("cratonvm/test/compile_gate/{tag}")
    }

    #[test]
    fn an_admission_opens_and_closes_the_thread_scope() {
        assert!(!admission_is_open());
        let cls = unique("scope");
        let a = admit(&cls, "m", "()V", CompileDoor::Osr).expect("clean method admits");
        assert!(admission_is_open());
        assert_eq!(a.door(), CompileDoor::Osr);
        drop(a);
        assert!(!admission_is_open());
    }

    /// The nesting case: `callee_compiler` compiles a callee inside the
    /// caller's compilation, so a flag would clear on the inner drop and make
    /// the rest of the outer compile read as ungated.
    #[test]
    fn admissions_nest() {
        let outer = admit(&unique("nest-outer"), "m", "()V", CompileDoor::MethodEntry)
            .expect("admits");
        let inner = admit(&unique("nest-inner"), "m", "()V", CompileDoor::MethodEntry)
            .expect("admits");
        drop(inner);
        assert!(
            admission_is_open(),
            "the outer compilation is still open after the callee's admission drops"
        );
        drop(outer);
        assert!(!admission_is_open());
    }

    /// The check `compile_osr_artifact` hand-copied. A bail-listed method is
    /// refused at every door, not just the one that recorded the bail.
    #[test]
    fn a_bail_listed_method_is_refused_at_every_door() {
        let cls = unique("bail-listed");
        crate::mark_jit_bail_listed(&cls, "m", "()V");
        for door in CompileDoor::ALL {
            assert_eq!(
                admit(&cls, "m", "()V", door).err(),
                Some(CompileRefusal::PermanentlyBailListed),
                "{} must honour the bail-list",
                door.label()
            );
        }
    }

    /// A permanent refusal and a transient one are different answers to
    /// "should I ask again", and the callers act on that difference.
    #[test]
    fn only_the_cap_refusal_is_transient() {
        assert!(CompileRefusal::CodeCacheAtCapacity.is_transient());
        assert!(!CompileRefusal::PermanentlyBailListed.is_transient());
        assert!(!CompileRefusal::ForceInterpreted.is_transient());
        assert!(!CompileRefusal::JitDisabled.is_transient());
    }

    /// The per-door counter is what makes "the OSR path calls the gate" a
    /// measurement. A gate whose counter did not move would let the VM-side
    /// assertion pass vacuously.
    #[test]
    fn admissions_are_counted_per_door() {
        let before = admissions(CompileDoor::Osr);
        let _a = admit(&unique("counted"), "m", "()V", CompileDoor::Osr).expect("admits");
        assert_eq!(admissions(CompileDoor::Osr), before + 1);
    }

    /// The drift witness itself. Written as an explicit
    /// with-token/without-token pair because a counter that never moves and a
    /// counter that always moves both read as "zero ungated entries" from the
    /// VM side.
    #[test]
    fn the_ungated_witness_fires_only_without_a_token() {
        let before = ungated_backend_entries();
        note_backend_entry();
        assert_eq!(
            ungated_backend_entries(),
            before + 1,
            "a backend entry with no admission open must be counted"
        );
        let _a = admit(&unique("witness"), "m", "()V", CompileDoor::Osr).expect("admits");
        note_backend_entry();
        assert_eq!(
            ungated_backend_entries(),
            before + 1,
            "a backend entry under an admission must not be counted"
        );
    }

    /// The escape hatch must not launder a backend entry.
    ///
    /// `for_backend_test` exists because an integration test under `jit/tests/`
    /// is a separate crate and cannot reach a `#[cfg(test)]` constructor. That
    /// makes it a real bypass of the type-level gate, so the *runtime* witness
    /// has to keep seeing through it — otherwise a production caller could
    /// reach for it and both layers would go quiet at once.
    #[test]
    fn the_backend_test_token_is_still_counted_as_ungated() {
        let before = ungated_backend_entries();
        let t = CompileAdmission::for_backend_test();
        assert!(
            !admission_is_open(),
            "the test token must not open the thread scope"
        );
        note_backend_entry();
        assert_eq!(
            ungated_backend_entries(),
            before + 1,
            "a backend entry under the test token must still be counted"
        );
        drop(t);
        assert!(!admission_is_open());
    }

    #[test]
    fn every_door_has_a_distinct_slot_and_label() {
        let mut labels: Vec<&str> = CompileDoor::ALL.iter().map(|d| d.label()).collect();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), CompileDoor::ALL.len());
        let mut idx: Vec<usize> = CompileDoor::ALL.iter().map(|d| d.index()).collect();
        idx.sort_unstable();
        assert_eq!(idx, vec![0, 1, 2]);
    }
}
