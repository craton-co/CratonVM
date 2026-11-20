// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! The LAMBDA door's half of "a trapping compiled body must not be re-run from
//! entry" — and the reason its sibling file could not assert it.
//!
//! # The gap this closes
//!
//! `jit_bridge_sink_resumes_instead_of_rerunning.rs` fixed and pinned three
//! sinks, and for the third one — `resume_deopted_body`, shared by
//! `execute_jit_call_oneshot` and the compiled caller's
//! `try_lambda_site_direct_call` — it reaches for the counter whose own doc
//! calls it *"the metric a regression test asserts is zero"*:
//!
//! ```ignore
//! let (_resumed, unresumable) = lambda_site_deopt_outcomes();
//! assert_eq!(unresumable, 0, "...");
//! ```
//!
//! **That zero is structural.** Its fixture, `DeoptRerunCount`, contains no SAM
//! anywhere — it is `invokestatic` throughout — so neither `SITE_RESUMED` nor
//! `SITE_UNRESUMABLE` can be incremented by it. The assertion holds just as
//! firmly on a VM whose lambda door is completely broken, because the counter
//! was never touched. A zero from a counter nothing bumped is evidence about
//! the counter.
//!
//! That file's own comment says the fixture "does not otherwise reach" this
//! sink, so the gap is acknowledged rather than hidden; this file is the
//! fixture that does reach it.
//!
//! # What makes it not vacuous
//!
//! Two assertions, and the ORDER matters: engagement first.
//!
//! * `resumed > 0` — the lambda door actually resumed a trapped body in this
//!   run. Without this, everything below is a statement about zero.
//! * `unresumable == 0` — none was answered by a re-run from entry.
//! * `delta == 1` — and the observable agrees: the side effect ran once for one
//!   call. `2` would mean the impl was re-entered from bci 0.
//!
//! The third is the one that would catch a resume that *reported* success and
//! rebuilt the frame wrongly, which the two counters cannot see.
//!
//! # Everything goes through Java, warm-up included
//!
//! For the same two reasons the sibling file documents at length: `vm.invoke`
//! enters `execute`, whose own tier-up sink resumes correctly (so calling the
//! impl from Rust would measure the wrong sink), and whose first-call path
//! caches a SINGLE-PASS body that preempts the optimizing pipeline (so warming
//! the impl directly would pin it at C1, where `can_deopt_resume` IS set and
//! the defect is invisible). `warm()` and `trip()` are Java; the impl is only
//! ever reached through `IntOp.apply`.
//!
//! # And the SAM call site has to be compiled too
//!
//! `try_lambda_site_direct_call` is entered from COMPILED CODE only —
//! `jit_invoke_virtual_mic` and the sibling door in `jit/helpers.rs` — never
//! from an interpreted frame. A first version of this file put
//! `OP.apply(1, 0)` straight into `trip()`, which runs interpreted; the impl
//! WAS compiled and DID trap, but the deopt went to the ordinary interface
//! sink and both lambda-site counters stayed at zero. The engagement assertion
//! below caught that on the first run, which is the whole reason it is there.
//! `DeoptLambdaRerunCount.step` exists to be the compiled call site.

use cratonvm_vm::config::VmConfig;
use cratonvm_vm::types::Value;
use cratonvm_vm::vm::Vm;

const CLASS: &str = "cratonvm/DeoptLambdaRerunCount";

/// The lambda BODY, which is what the optimizing tier compiles and what traps.
/// `javac` emits `(i, d) -> { … }` in a static-field initializer as this
/// synthetic private static method; if a compiler change ever renames it, the
/// anti-vacuity check below fails loudly rather than passing on the wrong
/// method.
const IMPL: &str = "lambda$static$0";
const IMPL_DESC: &str = "(II)I";

/// The SAM CALL SITE. It has to be compiled too — see
/// `wait_until_optimizing_compiled`.
const SITE: &str = "step";
const SITE_DESC: &str = "(I)I";

/// `warm(ITERS_PER_WARM)` per outer call; the PRODUCT is what has to clear
/// `TIER_OVERRIDES`' C2 threshold, since each inner iteration is one dispatch
/// of the impl.
const WARM_CALLS: i32 = 200;
const ITERS_PER_WARM: i32 = 64;









/// Wait for BOTH artifacts this file needs, driving them from BYTECODE.
///
/// * the IMPL, on the OPTIMIZING backend. The backend is checked, not merely
///   that an artifact exists: a single-pass body sets `can_deopt_resume` and
///   the sink's pre-existing arm resumes it, so a green run against one would
///   assert nothing about this defect.
/// * the CALL SITE (`step`), on any backend. This one is easy to forget and a
///   first draft did: `try_lambda_site_direct_call` is entered from compiled
///   code, so if `step` is still interpreted when the trap fires the deopt goes
///   to the ordinary interface sink and both lambda-site counters stay at zero
///   — which is precisely what the engagement assertion then reported.
fn wait_until_optimizing_compiled(vm: &mut Vm) {
    const ATTEMPTS: usize = 400;
    const CALLS_PER_ATTEMPT: i32 = 50;
    let mut saw_impl = false;
    let mut saw_site = false;
    for _ in 0..ATTEMPTS {
        let class_id = vm
            .shared
            .classes
            .class_manager
            .read()
            .get_loaded_class_id(CLASS)
            .expect("the fixture class must be loaded after invoke");
        let (impl_ir, site_any) = {
            let cache = vm.shared.jit.jit_cache.read();
            let impl_ir = match cache.get(CLASS, IMPL, IMPL_DESC, class_id) {
                Some(c) => {
                    saw_impl = true;
                    c.used_ir_backend
                }
                None => false,
            };
            let site_any = cache.get(CLASS, SITE, SITE_DESC, class_id).is_some();
            saw_site |= site_any;
            (impl_ir, site_any)
        };
        if impl_ir && site_any {
            return;
        }
        for _ in 0..CALLS_PER_ATTEMPT {
            let _ = vm.invoke(CLASS, "warm", "(I)I", &[Value::Int(ITERS_PER_WARM)]);
        }
    }
    panic!(
        "the two artifacts this file needs were not both installed: \
         {CLASS}.{IMPL}{IMPL_DESC} on the OPTIMIZING backend was {}, and the \
         compiled call site {CLASS}.{SITE}{SITE_DESC} was {}. Proceeding would \
         measure the single-pass backend, or send the deopt to the ordinary \
         interface sink instead of the lambda door, and pass for the wrong reason",
        if saw_impl {
            "installed, but from the single-pass door"
        } else {
            "never installed"
        },
        if saw_site {
            "installed"
        } else {
            "never installed"
        },
    );
}

/// JIT r23 wave 6 (lane L2;
/// `r23-w5-lambda-door-resume-test-intermittently-never-enters-door-FIXED-20261021`): the two
/// tests in this binary install DIFFERENT process-scoped overrides
/// (`CRATONVM_JIT_IR_LAMBDA_SAM_SPLICE=0` here, `=1` in the sibling), and
/// `cargo test` runs them on parallel threads. `with_process_overrides` leaves
/// serialisation to the caller ("concurrent installs are not supported"), so
/// the door test's VM compiled `step` under the sibling's `SPLICE=1` in about
/// one run in three: the SAM call was spliced, the trap resumed through the
/// splice's chain, and the door's counters never moved (`resumed=0`,
/// `delta=1`); `resumed=2` was the converse. Measured on the fe64a9a5c8 test
/// exe: 4 failures in 12 whole-binary runs, 0 in 6 runs of this test alone.
/// Every test here takes this lock first.
fn serial() -> std::sync::MutexGuard<'static, ()> {
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}



