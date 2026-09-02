// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Process-lifetime cache for the `CRATONVM_*` debug/trace environment
//! variables that are read from interpreter and JIT hot paths.
//!
//! `cratonvm_types::flags::runtime_var` / `cratonvm_types::flags::runtime_var_os` are surprisingly expensive on every
//! platform:
//!
//! * On Linux they take a global mutex in `libc::getenv` plus an `OsString`
//!   allocation and a `String` UTF-8 validation (for `var`).
//! * On Windows `GetEnvironmentVariableW` is a syscall (~500 ns) that copies
//!   the value into a wide buffer; `var` then decodes the UTF-16 to UTF-8.
//!
//! Reading any of these flags on every JIT entry, frame push/pop, branch or
//! exception throw burns measurable CPU and serialises threads on the libc
//! env-lock. Since the flags are only meant to be set once at process start
//! (`CRATONVM_FRAME_TRACE=1 java …`), we cache the parsed boolean (or value)
//! the first time it is read and serve every subsequent query from the
//! cached `OnceLock`. Setting the variable after the first read will have
//! no effect — same semantics as `runtime::exceptions::iae_trace_enabled`,
//! which has used this pattern since the original audit. A test that needs a
//! different value uses `flags::with_thread_overrides`, which every helper
//! here honours; see "Test overrides" below.
//!
//! Each helper is `#[inline]` so the cold first-call cost (one `getenv`
//! plus the `OnceLock::get_or_init` CAS) is paid exactly once per flag,
//! and steady-state cost collapses to a relaxed load of the `OnceLock`.
//!
//! # Test overrides
//!
//! This is a *second* latch, stacked on the process-wide
//! [`cratonvm_types::flags`] snapshot — which latches too, on the first read of
//! any flag. `flags::with_thread_overrides` can defeat the snapshot but not a
//! memo that has already answered, so every helper here goes through
//! [`memoized`], which recomputes from source while an override is installed
//! and never populates its `OnceLock` in that window. See
//! `libcratonvm-no-jdk-test-order-dependent-fixed-20260730.md`.

use cratonvm_types::flags::{MemoSlot, MEMO_UNSET};
use std::collections::HashSet;
use std::sync::OnceLock;

/// A memoised `bool`, invalidated when a test override is installed.
///
/// This is what keeps the memo from being a second, deeper latch than the one
/// `flags::override_thread` exists to escape.
///
/// The hot path is one relaxed load and a compare — the same shape the
/// `OnceLock` had, and the reason the override check lives in the *writer*
/// ([`MemoSlot::publish`]) rather than here: these predicates are read from the
/// interpreter's per-bytecode path, where asking "is an override installed?"
/// on every read measured at +1.15% of `execute_instruction`.
#[inline]
fn slot_bool<F: FnOnce() -> bool>(slot: &'static MemoSlot, compute: F) -> bool {
    let state = slot.load();
    if state == MEMO_UNSET {
        return derive_bool(slot, compute);
    }
    state == MEMO_TRUE
}

/// [`slot_bool`]'s cold arm: runs once per flag per process, and on every read
/// while an override is installed (because `publish` declines to store then).
///
/// `compute` is `impl FnOnce`, NOT a `fn` pointer. Every closure here captures
/// nothing, so as a generic it is a ZST and costs the hot path no argument at
/// all; taking `fn() -> bool` instead makes each call site materialise the
/// pointer before the branch that almost never needs it, and measured
/// **+0.85%** of interpreter instructions against this form's +0.24%.
#[cold]
#[inline(never)]
fn derive_bool<F: FnOnce() -> bool>(slot: &'static MemoSlot, compute: F) -> bool {
    let value = compute();
    slot.publish(if value { MEMO_TRUE } else { MEMO_FALSE });
    value
}

/// [`slot_bool`] for an `Option<bool>`, using a third encoded state.
#[inline]
fn slot_opt_bool<F: FnOnce() -> Option<bool>>(slot: &'static MemoSlot, compute: F) -> Option<bool> {
    match slot.load() {
        MEMO_UNSET => derive_opt_bool(slot, compute),
        MEMO_NONE => None,
        v => Some(v == MEMO_TRUE),
    }
}

#[cold]
#[inline(never)]
fn derive_opt_bool<F: FnOnce() -> Option<bool>>(
    slot: &'static MemoSlot,
    compute: F,
) -> Option<bool> {
    let value = compute();
    slot.publish(match value {
        None => MEMO_NONE,
        Some(false) => MEMO_FALSE,
        Some(true) => MEMO_TRUE,
    });
    value
}

/// Encodings for [`MemoSlot`]; `MEMO_UNSET` (0) is reserved by the slot itself.
const MEMO_FALSE: u8 = 1;
const MEMO_TRUE: u8 = 2;
const MEMO_NONE: u8 = 3;

/// Serve `cache`, except while a [`cratonvm_types::flags`] test override is
/// installed — then recompute from source.
///
/// For the handful of memos whose value does not fit a [`MemoSlot`]'s `u8`
/// (a threshold, a pattern list, a selector). None of them are on the
/// per-bytecode path, so paying the `overrides_active` load per read is fine
/// here; the two properties that matter are the same as for `publish`:
///
/// * While an override is live the `OnceLock` is **not populated**. Populating
///   it would capture the override's value for the rest of the process and
///   poison every later reader — strictly worse than the bug being fixed.
/// * The recompute path is `#[cold]` and out of line.
#[inline]
fn memoized_with<T: Copy, F: FnOnce() -> T>(cache: &'static OnceLock<T>, compute: F) -> T {
    if cratonvm_types::flags::overrides_active() {
        return recompute(compute);
    }
    *cache.get_or_init(compute)
}

/// Reference-returning sibling of [`memoized_with`], for
/// [`real_bytecode_selector`] — the one memo whose value is not `Copy`.
///
/// The override arm leaks, which is what lets it hand back a `&'static` at all.
/// That parse is cold enough for it not to matter, and it only ever happens
/// under a test override. `field_watch_class_matches` deliberately does NOT use
/// this: it is called per watched field access, so it builds its list on the
/// stack instead.
#[inline]
fn memoized_ref<T: 'static, F: FnOnce() -> T>(
    cache: &'static OnceLock<T>,
    compute: F,
) -> &'static T {
    if cratonvm_types::flags::overrides_active() {
        return Box::leak(Box::new(recompute(compute)));
    }
    cache.get_or_init(compute)
}

/// The override-active arm, out of line so the production path pays nothing but
/// the branch — `#[inline(never)]` is what keeps each caller's closure body out
/// of its own hot function.
#[cold]
#[inline(never)]
fn recompute<T, F: FnOnce() -> T>(compute: F) -> T {
    compute()
}

/// Build a boolean predicate that returns `true` iff the named env var is
/// **set** (any value, including the empty string), matching the semantics
/// of `cratonvm_types::flags::runtime_var_os(NAME).is_some()`.
macro_rules! cached_is_set {
    ($name:ident, $env:literal) => {
        #[inline]
        pub fn $name() -> bool {
            static CACHE: MemoSlot = MemoSlot::new();
            slot_bool(&CACHE, || {
                cratonvm_types::flags::runtime_var_os($env).is_some()
            })
        }
    };
}

/// Build a boolean predicate matching `cratonvm_types::flags::runtime_var(NAME).is_ok()` —
/// behaviourally identical to `is_set` on every platform we target, but
/// kept as a separate macro so the call sites that previously used `var`
/// instead of `var_os` keep their exact semantics (e.g. the var being
/// unset returns `false`; an invalid-UTF-8 OsString would technically
/// have differed, but we don't set our flags to non-UTF-8 in practice).
macro_rules! cached_is_ok {
    ($name:ident, $env:literal) => {
        #[inline]
        pub fn $name() -> bool {
            static CACHE: MemoSlot = MemoSlot::new();
            slot_bool(&CACHE, || cratonvm_types::flags::runtime_var($env).is_ok())
        }
    };
}

// ── Flags read from the JIT entry / dispatch hot path ───────────────────

/// `CRATONVM_DISABLE_JIT` — kill-switch that forces interpreter-only
/// execution. Read at every `jit_invoke_dispatch` call, every OSR
/// candidate, and several other JIT entry points. Semantics: treat empty
/// or `"0"` as disabled, anything else as enabled — same as the original
/// inline check.
#[inline]
pub fn disable_jit() -> bool {
    cratonvm_types::flags().jit.disable_jit
}

/// `CRATONVM_JIT_THRESHOLD` — invocation count at which a method becomes
/// JIT-compilation eligible (default 500). Read once and cached. Used by both
/// the interpreter's per-method upgrade gate and the dispatch-helper gate so a
/// single knob controls JIT eagerness. Raising it keeps short-lived / call-heavy
/// code interpreted (HotSpot's interpreter-first behaviour); genuinely-hot
/// compute loops cross any reasonable threshold and still compile. Invalid /
/// unset values fall back to 500. A value of 0 is clamped to 1 (0 would compile
/// on the first call, defeating warmup).
#[inline]
pub fn jit_invocation_threshold() -> u32 {
    static CACHE: OnceLock<u32> = OnceLock::new();
    memoized_with(&CACHE, || {
        cratonvm_types::flags::runtime_var("CRATONVM_JIT_THRESHOLD")
            .ok()
            .and_then(|v| v.trim().parse::<u32>().ok())
            .map(|v| v.max(1))
            .unwrap_or(500)
    })
}

/// `CRATONVM_JIT_OSR` — master enable for back-edge **On-Stack Replacement**
/// (entering JIT code mid-loop at a hot back-edge).
///
/// **Default: ON.** The blockers that kept back-edge OSR default-off have been
/// retired: reference parameters are now seeded into the OSR compile's oop mask,
/// primitive locals that collide with NaN-box object tags round-trip through the
/// OSR snapshot bit-exactly, and entries with an unsafe dead-local mask are
/// rejected before the trampoline runs. Regular whole-method JIT remains
/// unaffected; this gate controls only the mid-loop back-edge trigger.
///
/// Set `CRATONVM_JIT_OSR=0` (or `false`/`off`/`no`) to force OSR off for
/// diagnosis or bisection. Any other value, and an unset variable, enables OSR.
/// Read once and cached.
#[inline]
fn parse_osr_backedge_enabled(raw: Option<&str>) -> bool {
    raw.map(|v| {
        let v = v.trim();
        !(v == "0"
            || v.eq_ignore_ascii_case("false")
            || v.eq_ignore_ascii_case("off")
            || v.eq_ignore_ascii_case("no"))
    })
    .unwrap_or(true)
}

#[inline]
pub fn osr_backedge_enabled() -> bool {
    static CACHE: MemoSlot = MemoSlot::new();
    slot_bool(&CACHE, || {
        parse_osr_backedge_enabled(
            cratonvm_types::flags::runtime_var("CRATONVM_JIT_OSR")
                .ok()
                .as_deref(),
        )
    })
}

/// `CRATONVM_LOADER_AWARE_RESOLUTION` — loader-faithful `CONSTANT_Class`
/// resolution (ProxyClassReuseTest / IsoProbe family).
///
/// **Loader-identity consolidation:** this used to be one of THREE
/// independent copies of the same env-var parse (this file, a
/// `classloading::class_manager` copy, and a `native-builtins::classloader`
/// copy) — they drifted out of lock-step at least once in production (see
/// `fixed-suite-bugs/loader-identity.md`). `cratonvm_classloading::
/// loader_aware_resolution` is now the single source of truth; this
/// function is kept (same name, same signature, own doc history below) so
/// none of ITS callers have to change, but it simply forwards to the
/// classloading crate's `OnceLock`-cached copy rather than maintaining a
/// second cache of its own.
///
/// **Default: ON** (flipped from off during the `context.groovy` bug-cluster
/// fix — see below). When on, an implicit class-constant reference (`ldc
/// X.class`, `new X`, `checkcast`/`instanceof X`, `anewarray X`, and
/// field/method owner resolution) reached from bytecode defined by a
/// *user-defined* class loader is resolved through that loader as the JVMS
/// §5.4.3 *initiating* loader — i.e. by invoking its `loadClass` — instead of
/// through CratonVM's flat global class store. This makes two isolating
/// loaders that each define their own copy of a class `X` resolve `X` to
/// their *own* copy (HotSpot semantics) rather than collapsing both to the
/// first-loaded (application) copy.
///
/// Built-in-loader (bootstrap/extension/application) references keep the exact
/// global fast path, so this only engages for classes defined by custom
/// loaders — but that still covers web-app / OSGi / proxy loaders broadly,
/// which is why this shipped gated (default off) pending an app-gauntlet
/// soak; see `fixed-suite-bugs/hibernate/
/// hib-proxyclassreuse-loader-blind-class-resolution-FIXED.md`.
///
/// **Why the default flipped:** every Apache Groovy dynamic-DSL script run
/// (e.g. Spring's `GroovyBeanDefinitionReader`/`GenericGroovyApplicationContext`)
/// compiles each script through its own fresh `GroovyClassLoader$InnerLoader`
/// instance, but every closure literal in a script is named positionally
/// (`<ScriptClass>$_run_closure1`, `$_run_closure2`, …) — so two *different*
/// scripts loaded in the same process very commonly produce two DIFFERENT
/// classes with the textually IDENTICAL name. With this gate off, the
/// interpreter's implicit class-constant resolution (the `new`/`checkcast`
/// opcodes emitted for the closure's own instantiation, and its `doCall`
/// dispatch) resolved through the flat global store and silently collapsed
/// to whichever same-named closure class was registered FIRST — the SECOND
/// script's closure body then ran the FIRST script's compiled bytecode with
/// no exception, e.g. `beans { framework String, 'Grails' }` running some
/// unrelated earlier script's closure and registering zero of the beans the
/// current script actually declared (`NoSuchBeanDefinitionException: No bean
/// named 'framework' available` even though the resource loaded successfully
/// and `GroovyBeanDefinitionReader` reported no error). Confirmed via a
/// minimal repro (two `GroovyShell.evaluate(script, "beans")` calls, same
/// script `name` argument, different closure bodies): with the gate off,
/// `inner1.getClass() == inner2.getClass()` was `true` and the second
/// closure's `call()` ran the first closure's println; with the gate on,
/// they compare unequal and each dispatches its own body, matching HotSpot.
///
/// Regression check before flipping the default: ran the 3 target
/// `context.groovy` classes plus `scripting.bsh.{BshScriptEvaluatorTests,
/// BshScriptFactoryTests}` and `scripting.groovy.{GroovyAspectIntegrationTests,
/// GroovyAspectTests,GroovyClassLoadingTests,GroovyScriptEvaluatorTests,
/// GroovyScriptFactoryTests}` gate-off vs gate-on: every class was either
/// unchanged or strictly improved with the gate on (no new failures observed
/// in this slice). `GroovyApplicationContextTests` and
/// `GroovyApplicationContextDynamicBeanPropertyTests` go fully green
/// (byte-for-byte HotSpot match); `GroovyAspectTests` gains one more pass.
/// **2026-07-04 Hibernate app-gauntlet soak (the broader validation this doc
/// asked for):** full Hibernate ORM 8.0 suite, 4548 classes, real-JDK JIT-on,
/// gate on, TIMEOUT=600s, Linux (Azure host, dev `81a31c08`+): PASS 4293/4548
/// (94.4%), FAIL 133, CRASH 17, HANG 8, ABORTED 3. `ProxyClassReuseTest`
/// (the original bug this gate fixes) is 3/3 clean. Diffed against the known
/// baseline and filtered for already-documented pre-existing clusters
/// (bytecode.enhancement/lazytoone, jar-scanning, temporal-GC, the OSR-vtable
/// family): the residual ~62 classes are the same pre-existing bugs
/// independently root-caused elsewhere this session — **no evidence of the
/// gate turning any previously-passing class into a failure**. The two
/// specific classes flagged as gate-sensitive earlier (`bytecode.enhancement.
/// basic.{InheritedTest,MappedSuperclassTest}`) were never passing gate-off
/// either; gate-on changes their failure mode from a hard native CRASH
/// (rc=139) to ABORTED — a safety improvement, not a new regression, though
/// still not a clean pass. Conclusion: default-on holds under the Hibernate
/// custom-loader-heavy suite this comment previously flagged as untested.
/// If a broader regression turns up, revert this default flip (the env var
/// still overrides either way: `CRATONVM_LOADER_AWARE_RESOLUTION=0` to force
/// off) rather than reverting the loader-aware resolution logic itself, which
/// is independently correct.
///
/// Empty or `"0"` ⇒ disabled; any other value ⇒ enabled. Read once and
/// cached (in `cratonvm_classloading`'s own `OnceLock` — see the
/// consolidation note above; this function no longer keeps a second cache).
#[inline]
pub fn loader_aware_resolution() -> bool {
    cratonvm_classloading::loader_aware_resolution()
}

/// `CRATONVM_TIER_OSR_BACKEDGE` — wire-tiered-manager Step 6 — per-frame
/// back-edge count at which OSR is first attempted (`Frame::should_try_osr`'s
/// `osr_threshold` argument), default 1000 (the historical `OSR_THRESHOLD`
/// const in `interpreter.rs`). This is the *live* OSR trigger on BOTH the
/// default inline-OSR path and the Step-5 background-OSR path (which is why it
/// is a VM-side env knob, separate from the tiered manager's policy
/// `osr_threshold`). Lowering it makes hot loops OSR sooner (useful for
/// gauntlet tuning / quick springboot); raising it defers OSR. Invalid/unset →
/// useful profile/warmup). `None` when unset/invalid, so the caller keeps its
/// own default (`OSR_THRESHOLD`); `0` clamps to `1`. Read once and cached.
#[inline]
pub fn tier_osr_backedge() -> Option<u32> {
    static CACHE: OnceLock<Option<u32>> = OnceLock::new();
    memoized_with(&CACHE, || {
        cratonvm_types::flags::runtime_var("CRATONVM_TIER_OSR_BACKEDGE")
            .ok()
            .and_then(|v| v.trim().parse::<u32>().ok())
            .map(|v| v.max(1))
    })
}

/// `CRATONVM_OSR_NEWARRAY` — back-edge OSR for methods containing a primitive
/// `newarray` (0xbc). **Default: ON** (perf/throughput-20260710).
///
/// The 2026-07-10 BC-crypto session permanently OSR-denied any such method as
/// a blanket workaround for an OSR corruption it attributed to
/// `GOST3412_2015Engine.init_gf256_mul_table` ("resume with corrupt stack
/// state for the next newarray length"). That deny forced every
/// array-allocating hot loop in every program to stay interpreted forever —
/// BenchSuite sieve250k regressed 3.2s → 177s (~55x), and BC's math/EC
/// kernels lost OSR entirely under JIT-allow.
///
/// The corruption does not reproduce on the current tree: GOST3412Test soaks
/// 10/10 at -Xmx256m across both getfield modes, a purpose-built
/// nested-allocation-loop repro (OsrNewArrayRepro, the exact
/// init_gf256_mul_table shape) is checksum-exact vs HotSpot under heap
/// pressure, and the EC AllTests suite passes under full JIT-allow. The
/// trigger was most plausibly one of the concurrently-fixed root gaps
/// (ThreadLocal value rooting landed in the SAME commit as the deny). Set
/// `CRATONVM_OSR_NEWARRAY=0` to restore the deny for bisection if a
/// suspicious newarray-loop corruption ever resurfaces. Read once and cached.
#[inline]
pub fn osr_newarray_allowed() -> bool {
    static CACHE: MemoSlot = MemoSlot::new();
    slot_bool(&CACHE, || {
        match cratonvm_types::flags::runtime_var("CRATONVM_OSR_NEWARRAY") {
            Ok(v) => v != "0" && !v.eq_ignore_ascii_case("false"),
            Err(_) => true,
        }
    })
}

/// `CRATONVM_JIT_OSR_ATHROW` — back-edge OSR for a method that contains a bare
/// `athrow` (0xbf) and declares **no** local exception handlers. **Default: ON**
/// (RBC.6 lift, 2026-08-17).
///
/// RBC.6 refused every `athrow`-containing method outright, because the OSR
/// bail path's only move was to re-stash the throwable and resume the live
/// interpreter frame at the STALE pre-OSR back-edge pc — re-running every
/// iteration the OSR'd code had already committed (RBC.7's silent-corruption
/// shape). With no handlers declared, the throwable provably cannot be caught
/// by the OSR'd frame, so `propagate_osr_exception` unwinds it out of the frame
/// instead and there is no resume left to be stale. A method that DOES declare
/// handlers is still refused here, for a reason RBC.6b's own lift does not
/// cover — see the gate itself, in `compile_osr_artifact`, for that argument.
///
/// Set `CRATONVM_JIT_OSR_ATHROW=0` to restore the blanket refusal so ONE binary
/// can A/B the lift (a cross-binary A/B is not an A/B). Read once and cached.
#[inline]
pub fn osr_athrow_allowed() -> bool {
    static CACHE: MemoSlot = MemoSlot::new();
    slot_bool(&CACHE, || {
        match cratonvm_types::flags::runtime_var("CRATONVM_JIT_OSR_ATHROW") {
            Ok(v) => v != "0" && !v.eq_ignore_ascii_case("false"),
            Err(_) => true,
        }
    })
}

/// `CRATONVM_JIT_OSR_EXC_TABLE` — back-edge OSR for a method with a non-empty
/// exception table. **Default: ON** (2026-08-17, this is the RBC.6b lift).
///
/// RBC.6b refused every such method outright. Because OSR is the ONLY door out
/// of the interpreter for a method invoked once — a `@Test` body, a `main`, any
/// one-shot driver — that made "a hot loop with a try/catch in it" run
/// interpreted for its whole life: netty's two `HttpHeaderValidationUtilTest`
/// exhaustive loops measured 19 242 and 309 423 ns/iteration against HotSpot's
/// 8.2 and 9.4.
///
/// The lift stages the method-entry path's precise-exception-frame contract for
/// the OSR compile and admits only methods where every throwing site inside a
/// protected range publishes a reason-9 frame
/// (`first_unsupported_precise_frame_site`). Set `=0` to restore the blanket
/// refusal, so one binary can A/B the lift — the arm that answers "did this
/// change the answer, or only the speed?". Read once and cached.
///
/// See fixed-suite-bugs/jit/osr-refuses-any-method-with-an-exception-table-FIXED-20260817.md.
#[inline]
pub fn osr_exception_table_allowed() -> bool {
    static CACHE: MemoSlot = MemoSlot::new();
    slot_bool(&CACHE, || {
        match cratonvm_types::flags::runtime_var("CRATONVM_JIT_OSR_EXC_TABLE") {
            Ok(v) => v != "0" && !v.eq_ignore_ascii_case("false"),
            Err(_) => true,
        }
    })
}

/// `CRATONVM_JIT_INLINE_CALLS` — allow a call INSIDE a spliced (inlined) body.
/// **Default: ON since 2026-08-20; `=0` restores the refusal.**
///
/// Until this existed, `resolve_inline_site_from` rejected any callee
/// containing an `invoke*`, so inlining reached only call-free leaves. That is
/// what made a JUnit assertion chain un-collapsible: every rung of
/// `assertEquals(int,int)` -> `assertEquals(Object,Object)` -> `objectsAreEqual`
/// is small enough to splice, but each one CALLS the next, so none of them was
/// ever eligible and every rung paid a full dispatch round trip.
///
/// With the gate on, such a call is emitted as the ordinary
/// `jit_invoke_dispatch` sequence — the same helper, the same post-invoke
/// exception check, the same oop map — resolved against the CALLEE's constant
/// pool (`InlineSite::invoke_targets`). It is not a cheaper call; the win is
/// that the ENCLOSING body becomes inlineable at all.
///
/// It shipped default-OFF, on the argument that the inline emitter's failure
/// mode is a silent wrong answer and that the gate is what makes a bisect
/// possible. Both halves are still true; what changed is that the arm has now
/// been measured rather than reasoned about, and a feature nobody runs is worth
/// nothing:
///
/// * `probes/AssertChainProbe`, one binary, interleaved, `assertFull`:
///   **176.5/166.5 -> 124.4/121.2 ns/iter, -29%/-27%**, with
///   `inline-nest` + `inline-splice-devirt` (the three move together; each on
///   its own does nothing, see their own docs). The Azure figure for the same
///   set was -24%.
/// * The whole netty `codec-http` suite, 103 classes, same binary, flags off
///   and on: **identical result sets**, including which three classes miss the
///   180 s wall.
/// * `regression-suite/run.sh`, 64 vectors, three arms (default / local
///   handlers / everything): identical, 63 pass and the one pre-existing
///   `RImmutableFactoryTypes` failure that is also there on unmodified `dev`.
///
/// The bisect lever is unchanged in the other direction: `=0` (also `false`)
/// restores the refusal, so one binary still has two arms. Read once and
/// cached.
#[inline]
/// Bind an `invokevirtual` whose target is `final` (or whose class is) as a
/// direct, non-dispatching call. Default ON; `CRATONVM_JIT_FINAL_DEVIRT=0`
/// reverts to virtual dispatch at every such site.
///
/// The A/B lever for `invoke::invokevirtual_site_final_owner`. It engages in
/// the three `cp_invokespecial_owner_resolver` closures, which feed BOTH the
/// single-pass and the optimizing-tier invoke classification — so one switch
/// moves both, and a measurement that moves only one arm means the site was
/// reached through a door this rule does not sit in.
pub fn jit_final_devirt() -> bool {
    static CACHE: MemoSlot = MemoSlot::new();
    slot_bool(&CACHE, || {
        match cratonvm_types::flags::runtime_var("CRATONVM_JIT_FINAL_DEVIRT") {
            Ok(v) => !matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "0" | "false" | "off" | "no"
            ),
            Err(_) => true,
        }
    })
}

pub fn jit_inline_calls() -> bool {
    static CACHE: MemoSlot = MemoSlot::new();
    slot_bool(&CACHE, || {
        match cratonvm_types::flags::runtime_var("CRATONVM_JIT_INLINE_CALLS") {
            Ok(v) => !matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "0" | "false" | "off" | "no"
            ),
            Err(_) => true,
        }
    })
}

/// `CRATONVM_JIT_INLINE_SPLICE_DEVIRT` — devirtualise a `invokevirtual` /
/// `invokeinterface` INSIDE a spliced body, behind a receiver class-id guard.
/// **Default: ON since 2026-08-20; `=0` restores the refusal.** The measured
/// case for the flip is on [`jit_inline_calls`] — the three flags move together
/// and none of them does anything alone.
///
/// Nesting on its own reaches only statically bound calls, which is why it
/// recovered the step-4 regression without beating the baseline: the JUnit
/// chain's terminal `UNKNOWN.equals(k)` is an `invokevirtual`, and so are
/// `HttpStatusClass.valueOf`'s five `contains` calls.
///
/// The profile this needs already existed and nobody had looked for it there.
/// Receiver types are recorded by the interpreter against the bci of the method
/// that is EXECUTING, so a call inside `objectsAreEqual` is profiled under
/// `objectsAreEqual`'s own [`MethodKey`](crate::jit::profile::MethodKey) at its
/// own bci — exactly the (method, pc) pair a nested site names. Re-keying the
/// ENCLOSING method's profile by (caller pc, callee pc), which is what the
/// netty pages predicted would be needed, would have been the wrong shape: that
/// profile never had the information.
///
/// Same 80% dominance bar as the top-level guarded-virtual planner, and the
/// same bargain — the hot edge is a spliced body, the cold edge is the ordinary
/// call. It is only a bargain while the guard holds, which is what the bar is
/// for. Requires [`jit_inline_nest`]. Read once and cached.
#[inline]
pub fn jit_inline_splice_devirt() -> bool {
    static CACHE: MemoSlot = MemoSlot::new();
    slot_bool(&CACHE, || {
        match cratonvm_types::flags::runtime_var("CRATONVM_JIT_INLINE_SPLICE_DEVIRT") {
            Ok(v) => !matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "0" | "false" | "off" | "no"
            ),
            Err(_) => true,
        }
    })
}

/// `CRATONVM_JIT_INLINE_CALL_DISPATCH` — let a call inside a spliced body fall
/// back to the blind `jit_invoke_dispatch` helper. **Default: OFF, and the
/// default is a MEASURED one.**
///
/// The first cut of [`jit_inline_calls`] emitted every admitted call that way.
/// Measured on `probes/AssertChainProbe`, Azure Linux, release, one binary,
/// three interleaved rounds:
///
/// | arm | `assertFull` ns/iter |
/// |---|---:|
/// | base | 47.2 / 45.3 / 47.9 |
/// | + main inline | 50.3 / 79.8 / 58.9 |
/// | + inline calls (dispatch fallback) | **163.3 / 266.6 / 172.2** |
/// | + nesting | 47.3 / 59.6 / 80.2 |
///
/// and the counter that names the mechanism, `CRATONVM_DBG=mic-prof` over
/// 2 000 000 iterations: `disp_calls` **3 870 -> 2 003 361**, `cyc_disp_total`
/// **1.86M -> 1 049M cycles**. One blind dispatch per iteration, ~524 cycles
/// each.
///
/// The reason is structural, not a tuning miss. The chain this was built for is
/// already DIRECT-BOUND: each rung is a raw `CALL` to a compiled entry, a few
/// nanoseconds. Splicing the enclosing body removes one frame and converts its
/// inner call from that direct call into the blind helper, which resolves by
/// name on every execution. The frame saved is worth ~4 ns; the call downgraded
/// costs ~175. **Splicing a body is only worth it when the call inside it does
/// not get worse.**
///
/// So with this off, a callee containing a call is admitted ONLY when every one
/// of those calls is itself spliced (`InlineSite::nested_sites`), and the
/// dispatch fallback is not merely unused but not planned — `invoke_targets` is
/// cleared, so a nested splice that bails at emission time bails the enclosing
/// splice too rather than silently degrading to the helper.
///
/// Kept as a flag rather than deleted because it is the arm that REPRODUCES the
/// measurement above, and because it is what a direct-binding follow-up (give a
/// spliced call the `direct_calls` treatment the top level already has) would
/// replace rather than remove. Read once and cached.
#[inline]
pub fn jit_inline_call_dispatch() -> bool {
    static CACHE: MemoSlot = MemoSlot::new();
    slot_bool(&CACHE, || {
        matches!(
            cratonvm_types::flags::runtime_var("CRATONVM_JIT_INLINE_CALL_DISPATCH"),
            Ok(ref v) if v != "0" && !v.eq_ignore_ascii_case("false")
        )
    })
}

/// `CRATONVM_JIT_INLINE_NEST` — splice a call that is itself inside a spliced
/// body, up to `cratonvm_jit::MAX_INLINE_NEST_DEPTH` levels. **Default: ON
/// since 2026-08-20; `=0` restores the refusal.** The measured case for the
/// flip is on [`jit_inline_calls`] — the three flags move together and none of
/// them does anything alone.
///
/// Requires [`jit_inline_calls`]: nesting resolves its candidates out of
/// `InlineSite::invoke_targets`, which stays empty with that gate off. Kept
/// SEPARATE from it so a regression can be bisected to "calls inside splices"
/// versus "splices inside splices" — two different emitter paths with two
/// different failure modes.
///
/// Only statically-bound calls (`invokestatic`, `invokespecial`) nest; a
/// virtual or interface call inside a spliced body keeps the dispatch helper,
/// because selecting its body needs a runtime receiver and the receiver
/// profile is keyed by the ENCLOSING method's bci, not a callee-internal pc.
/// Read once and cached.
#[inline]
pub fn jit_inline_nest() -> bool {
    static CACHE: MemoSlot = MemoSlot::new();
    slot_bool(&CACHE, || {
        match cratonvm_types::flags::runtime_var("CRATONVM_JIT_INLINE_NEST") {
            Ok(v) => !matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "0" | "false" | "off" | "no"
            ),
            Err(_) => true,
        }
    })
}

/// `CRATONVM_DISABLE_INTRINSICS` — kill-switch that prevents the interpreter
/// from ever populating a `CachedInvokeTarget::Intrinsic` inline-cache entry,
/// forcing every call through the ordinary native/bytecode dispatch path.
///
/// This is the off-switch for the intrinsic-table differential tests: running
/// a program once with intrinsics on and once with this set to `1` must
/// produce identical output. Semantics match `disable_jit()` — empty or `"0"`
/// is treated as disabled (i.e. intrinsics stay enabled), anything else
/// disables intrinsics.
#[inline]
pub fn intrinsics_disabled() -> bool {
    static CACHE: MemoSlot = MemoSlot::new();
    slot_bool(&CACHE, || {
        match cratonvm_types::flags::runtime_var("CRATONVM_DISABLE_INTRINSICS") {
            Ok(v) => !v.is_empty() && v != "0",
            Err(_) => false,
        }
    })
}

/// `CRATONVM_ENFORCE_NATIVE_SHADOW` — arm §1.4 enforcement at
/// `try_stackless_invoke` step 1 under `--jdk-only`.
///
/// Step 1 always *observes* a `Bridge` standing in front of concrete bytecode
/// now (see `resolve_step1_native`), so the census is truthful either way. This
/// dial decides whether the observation is also acted on — whether the bridge
/// yields and the real bytecode runs.
///
/// **Default off, and that is a measurement, not a preference.** Turning it on
/// takes the `--jdk-only` regression corpus from **32 passed / 17 failed to 3
/// passed / 46 failed** (Azure Linux, JDK 25, 2026-08-06). The failures are not
/// dispatch faults: `System.props` is null, `Charset.forName` hands out an
/// instance of the ABSTRACT `java.nio.charset.Charset`, `String`'s coder does
/// not match its `value[]`. Under `--jdk-only` the surviving bridges ARE the
/// object model for large parts of `java.base`, so yielding them to bytecode
/// hands real code objects it cannot service. §1.4's remedy for those is to
/// retire the registration once the class's state is real (wave-2 item 4), not
/// to yield at dispatch — this dial exists so that migration can re-take the
/// measurement one subsystem at a time instead of arguing about it.
///
/// All five blocker families, with symptoms, are in
/// `jdk-only-step1-bytecode-available-RESOLVED-20260806.md`.
///
/// ## Scoping it to one subsystem
///
/// The all-or-nothing form could not deliver on "one subsystem at a time": the
/// whole-corpus arming is the only thing it could do, and the whole-corpus
/// answer is 3/46, which says nothing about any individual family. So the
/// variable is also a **prefix list**:
///
/// ```text
///   CRATONVM_ENFORCE_NATIVE_SHADOW=1                      every receiver
///   CRATONVM_ENFORCE_NATIVE_SHADOW=all                    every receiver
///   CRATONVM_ENFORCE_NATIVE_SHADOW=javax/management/      the JMX subsystem
///   CRATONVM_ENFORCE_NATIVE_SHADOW=java/util/logging/,javax/management/
/// ```
///
/// A value that is not `1`/`all`/`true`/`yes` is read as a comma-separated list
/// of INTERNAL class-name prefixes (slashes, not dots), and enforcement applies
/// to a dispatch only when the receiver's class name starts with one of them.
/// Empty and `0` stay "off entirely", so the two spellings that already meant
/// something keep meaning it.
///
/// A prefix list is the right granularity rather than a registrar name because
/// the decision is made at DISPATCH, where the registering file is not
/// something the VM knows — the census's `registered_by` is a `#[track_caller]`
/// record of registration, not of dispatch. A subsystem's receivers share a
/// package prefix; that is the handle dispatch actually has.
///
/// No effect outside `--jdk-only`: the caller tests `is_jdk_only()` first.
///
/// # THE CONTRACT — what an armed run does and does not entitle you to say
///
/// `H16-3` N2 and `H17-2` N5 both asked for this, because four records read the
/// dial as "simulate a retirement" while it did something narrower, and nobody
/// had written down which. It is stated here, at the flag, rather than in a
/// record, because the reader who needs it is the one about to type the
/// variable.
///
/// **What it does.** For every dispatch of a `Bridge` native whose receiver
/// class the scope covers, under `--jdk-only`, where the shadowed method has a
/// concrete `Code` attribute: the native declines and the real JDK bytecode
/// runs. Every dispatch, at every one of the fourteen doors — not the first
/// one, not the cold ones. Before 2026-08-21 that last clause was false, and
/// every armed cell published before then is void (see `H17-2`, and caveat 4
/// of `scripts/jdk-only-blast-radius.sh`).
///
/// **What it still is not.** A retirement removes the REGISTRATION. Three
/// differences survive, and all three are permanent properties of a dial
/// rather than bugs to be fixed:
///
/// 1. **No-bytecode triples still run their native.** A yield needs concrete
///    bytecode to yield to; a retirement of a triple with no `Code` produces an
///    `AbstractMethodError`, not a working call. An armed run therefore
///    UNDER-prices those, and this is the one direction that really is a floor.
///    `enforcement_dial.declined_no_bytecode` in `--jdk-only-report` counts
///    exactly them, so the size of the gap is measurable rather than assumed.
/// 2. **The registration is still there.** 162 triples are registered more than
///    once and only the `owns_slot: true` one is reachable, so a real retirement
///    PROMOTES the loser while the dial does not. `H22` nearly put sixteen
///    already-condemned bodies into service this way. Diff
///    `--dump-native-registry` across any deletion; an armed run cannot warn you.
/// 3. **A scope narrower than `all` produces a MIXED heap.** Objects built by a
///    covered class's bytecode and objects built by an uncovered class's native
///    coexist and are handed to each other. That is not a smaller version of the
///    retirement — it is a state no configuration reaches. It is also the entire
///    reason scoping exists, so this is a cost to be aware of, not avoided.
///
/// **The asymmetry, which is the part worth memorising.** An armed FAILURE is
/// real: something genuinely broke when real bytecode ran. An armed ZERO is
/// weaker than it looks — it says the corpus asked no question this prefix
/// answers wrongly, and the corpus does not ask about array component types,
/// the CONTENT of a built string, or the class identity of a returned object.
/// `H23`'s table fix was green three arms running and completely inert.
#[inline]
pub fn jdk_only_enforce_shadow() -> bool {
    static CACHE: MemoSlot = MemoSlot::new();
    slot_bool(&CACHE, || !enforce_shadow_scope().is_off())
}

/// How `CRATONVM_ENFORCE_NATIVE_SHADOW` was spelled. See
/// [`jdk_only_enforce_shadow`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EnforceShadowScope {
    /// Unset, empty, or `0` — §1.4 is observed and not acted on.
    Off,
    /// `1` / `all` / `true` / `yes` — every receiver.
    All,
    /// A comma-separated prefix list; only receivers under one of these
    /// internal-name prefixes yield to bytecode.
    Prefixes(Vec<String>),
}

impl EnforceShadowScope {
    /// Nothing is enforced.
    pub fn is_off(&self) -> bool {
        match self {
            EnforceShadowScope::Off => true,
            EnforceShadowScope::All => false,
            // A value of `,` or `,,` parses to no prefixes, which would enforce
            // nothing while reading as armed. Say so here rather than letting a
            // typo produce a silently-inert run that looks like a green result.
            EnforceShadowScope::Prefixes(p) => p.is_empty(),
        }
    }

    /// How this scope should be spelled in `--jdk-only-report`.
    ///
    /// An armed report and an unarmed one were byte-identical in every field a
    /// reader could use to tell them apart, which is how four records came to
    /// quote armed cells beside unarmed ones. The dial changes what the whole
    /// census MEANS -- under `enforce` the bridge does not run, so the
    /// `bridge-ran-over-bytecode` half of `violations[]` is empty by
    /// construction rather than for want of shadows -- so the report has to say
    /// which one it is. `"off"` is written out rather than omitted for the same
    /// reason `observation_sink` is written in `Compatible`: an absent field is
    /// ambiguous between "unarmed" and "this binary cannot answer".
    pub fn report_spelling(&self) -> String {
        match self {
            EnforceShadowScope::Off => "off".to_string(),
            EnforceShadowScope::All => "all".to_string(),
            EnforceShadowScope::Prefixes(p) => p.join(","),
        }
    }

    /// Does enforcement apply to a dispatch on `class_name` (internal form)?
    pub fn covers(&self, class_name: &str) -> bool {
        match self {
            EnforceShadowScope::Off => false,
            EnforceShadowScope::All => true,
            EnforceShadowScope::Prefixes(p) => p.iter().any(|pre| class_name.starts_with(pre)),
        }
    }
}

/// The parsed `CRATONVM_ENFORCE_NATIVE_SHADOW`, memoised.
pub fn enforce_shadow_scope() -> &'static EnforceShadowScope {
    static CACHE: OnceLock<EnforceShadowScope> = OnceLock::new();
    memoized_ref(&CACHE, || {
        parse_enforce_shadow_scope(
            cratonvm_types::flags::runtime_var("CRATONVM_ENFORCE_NATIVE_SHADOW")
                .ok()
                .as_deref(),
        )
    })
}

/// The parse behind [`enforce_shadow_scope`], separated so it can be tested
/// without a process-wide env var — a memoised reader cannot be re-armed once
/// something in the same process has read it.
fn parse_enforce_shadow_scope(raw: Option<&str>) -> EnforceShadowScope {
    let Some(raw) = raw else {
        return EnforceShadowScope::Off;
    };
    let raw = raw.trim();
    if raw.is_empty() || raw == "0" {
        return EnforceShadowScope::Off;
    }
    if matches!(
        raw.to_ascii_lowercase().as_str(),
        "1" | "all" | "true" | "yes" | "on"
    ) {
        return EnforceShadowScope::All;
    }
    EnforceShadowScope::Prefixes(
        raw.split(',')
            .map(|t| t.trim().replace('.', "/"))
            .filter(|t| !t.is_empty())
            .collect(),
    )
}

/// [`jdk_only_enforce_shadow`], narrowed to one receiver class.
///
/// This is the predicate dispatch must ask. `jdk_only_enforce_shadow()` alone
/// answers "is anything armed", which is the right question for a banner and
/// the WRONG one for a dispatch decision: under a prefix list it is true for
/// every class, and enforcing a subsystem's dial across the whole VM is exactly
/// the 3/46 collapse the scoping exists to avoid.
#[inline]
pub fn jdk_only_enforce_shadow_for(class_name: &str) -> bool {
    // The common case is Off, and `jdk_only_enforce_shadow` is a memoised bool
    // read; only an armed run pays for the scope lookup and the prefix scan.
    jdk_only_enforce_shadow() && enforce_shadow_scope().covers(class_name)
}

/// `CRATONVM_HELPFUL_NPE_OPCODES` — JEP 358 increment 2 opt-in. When set,
/// the non-invoke null-deref opcodes (`getfield`/`putfield`, `arraylength`,
/// the array load/store family, `monitorenter`/`monitorexit`, `athrow`) emit
/// the HotSpot-style `Cannot <action> because "<expr>" is null` message
/// instead of their older ad-hoc null-NPE text.
///
/// DEFAULT-ON (Increment 5): the differential compliance run confirmed these
/// messages are byte-identical to HotSpot's `getExtendedNPEMessage`, so the
/// `vm-cli` launcher now resolves an absent flag to **on** (matching HotSpot)
/// and calls [`set_show_code_details_in_exception_messages`] accordingly. The
/// increment-1 invoke-site message is unconditionally on (it shipped already)
/// and is unaffected by this flag. The `CRATONVM_HELPFUL_NPE_OPCODES` env var
/// (empty/`"0"` off, anything else on) still overrides everything.
///
/// Process-global, set once at VM init from the parsed
/// `-XX:±ShowCodeDetailsInExceptionMessages` flag (see
/// [`set_show_code_details_in_exception_messages`]). `-1` = `set()` never
/// called, `0` = off, `1` = on. The `-1` sentinel reads as **off** so a host
/// that embeds the VM without wiring the flag (and the in-process Rust test
/// harness, which never calls `set()`) keeps the legacy strings; the CLI always
/// calls `set()` with the resolved value, so app runs get the on default.
static SHOW_CODE_DETAILS: std::sync::atomic::AtomicI8 = std::sync::atomic::AtomicI8::new(-1);

/// Apply the HotSpot `-XX:±ShowCodeDetailsInExceptionMessages` VM flag. Called
/// once at VM startup (from `vm-cli`, before `Vm::new` runs any bytecode) with
/// the resolved [`crate::config::VmConfig`] value. The interim
/// `CRATONVM_HELPFUL_NPE_OPCODES` env var still overrides this when set.
#[inline]
pub fn set_show_code_details_in_exception_messages(on: bool) {
    SHOW_CODE_DETAILS.store(if on { 1 } else { 0 }, std::sync::atomic::Ordering::Relaxed);
}

/// The explicitly-configured value of the flag, or `None` when nothing set it
/// (the `-1` sentinel and no `CRATONVM_HELPFUL_NPE_OPCODES`).
#[inline]
fn show_code_details_explicit() -> Option<bool> {
    // Explicit env override wins (interim developer knob), parsed once.
    static ENV: MemoSlot = MemoSlot::new();
    let env = slot_opt_bool(&ENV, || {
        match cratonvm_types::flags::runtime_var("CRATONVM_HELPFUL_NPE_OPCODES") {
            Ok(v) => Some(!v.is_empty() && v != "0"),
            Err(_) => None,
        }
    });
    if env.is_some() {
        return env;
    }
    match SHOW_CODE_DETAILS.load(std::sync::atomic::Ordering::Relaxed) {
        1 => Some(true),
        0 => Some(false),
        _ => None,
    }
}

/// Whether the flag was explicitly turned **off** — `-XX:-ShowCodeDetails\
/// InExceptionMessages` or `CRATONVM_HELPFUL_NPE_OPCODES=0`.
///
/// HotSpot's `getMessage()` is `null` for an implicit-dereference NPE under that
/// flag, so this is *not* the same as "not on": the `-1` sentinel (an embedder
/// or the in-process Rust test harness that never wires the flag) must keep the
/// legacy diagnostic strings, and only an explicit opt-out suppresses the
/// message outright.
#[inline]
pub fn helpful_npe_suppressed() -> bool {
    show_code_details_explicit() == Some(false)
}

/// Whether a null-deref opcode should build its message through the JEP-358
/// path at all.
///
/// True both when the flag is **on** (build the HotSpot message) and when it is
/// explicitly **off** (build the HotSpot *suppression* — the builders return the
/// empty marker, which becomes a null `getMessage()`). Only the unwired sentinel
/// returns false, keeping the pre-JEP-358 diagnostics for embedders and the
/// in-process test harness.
#[inline]
pub fn helpful_npe_opcodes() -> bool {
    show_code_details_explicit().is_some()
}

/// proxy-real-classfile — real-super gate. When ON (the **default**, per the
/// "real Java by default, synthetic experimental" project rule), the generated
/// `$ProxyN` classes extend the **real** `java.lang.reflect.Proxy` (whose sole
/// instance field `h` sits at slot 0, matching the handler slot the dispatch
/// path already reads) — so `getSuperclass()` is `Proxy` and `instanceof Proxy`
/// is true, matching HotSpot. Set `CRATONVM_REAL_PROXY_SUPER=0` (or
/// `false`/`off`/`no`) to opt into the **experimental synthetic**
/// `java/lang/reflect/Proxy$Instance` super instead. BOTH implementations are
/// retained — nothing is deleted. Parsed once, cached — consulted on the proxy
/// dispatch hot path (`class_chain_reaches_proxy_instance`), so the lookup must
/// be cheap. Must stay in lockstep with `native_builtins::real_proxy_super()`.
#[inline]
pub fn real_proxy_super() -> bool {
    static GATE: MemoSlot = MemoSlot::new();
    slot_bool(&GATE, || {
        match cratonvm_types::flags::runtime_var("CRATONVM_REAL_PROXY_SUPER") {
            Ok(v) => {
                let v = v.trim().to_ascii_lowercase();
                !(v == "0" || v == "false" || v == "off" || v == "no")
            }
            // Unset (the default): real `java.lang.reflect.Proxy` super.
            Err(_) => true,
        }
    })
}

// Cache the per-native-call GC root snapshot's *frozen* lower frames and
// re-scan only the churning top, keyed by per-frame `(seq, exec_epoch)` + GC
// generation.
// Correctness rests on the LIFO stack discipline (a frame still present at
// index k with unchanged seq proves [0..k) stayed continuously frozen). See
// `update_root_snapshot`. Local writes bump `exec_epoch`, so a frozen-frame
// cache entry is reused only while that frame's root shape is unchanged. The
// live guard on the cache is `roots::conservative_locals_enabled()`, read in
// `update_root_snapshot`.
//
// RETIRED 2026-07-31 — this is where `cached_is_set!(real_forkjoinpool,
// "CRATONVM_REAL_FORKJOINPOOL")` used to live, and both cache sites bypassed
// themselves when it was true. It was a *presence* test written while the real
// pool was opt-in, so "the variable is set" and "we are in the real lane" were
// the same statement. Real ForkJoinPool then became the default (opt out with
// `CRATONVM_SYNTHETIC_FORKJOINPOOL`), nobody sets the old variable any more,
// and the predicate went silently false on exactly the configuration it was
// written to catch — the bypass had not fired on a default run since the flip.
//
// It was removed rather than repointed at `flags().natives.real_forkjoinpool`
// because the cache was first checked directly against the root loss the
// bypass claimed to prevent: `CRATONVM_DBG_ROOTSNAP_VERIFY=1` re-scans every
// frame the uncached way after each cached snapshot and reports any root the
// cached snapshot lacks, and the real-lane Fork6/Fork6Hard GC-stress repros
// reported none. Repointing it at the resolved flag would instead have fired
// the bypass always and made the cache dead code on every run. See
// rootsnap-cache-bypass-lost-its-trigger-RESOLVED-20260731.md.

// DEFAULT-ON as of 2026-06-16 (SpringRepositoriesExtension hang). Previously
// default-OFF: `update_root_snapshot` rescans EVERY interpreter frame on every
// object-returning native call, so a native-call-heavy hot loop running at a
// deep stack (Groovy compile under JUnit at depth ~46) pays O(stack-depth) per
// call and hangs (>300s). Measured: parse#0 depth-40 95s→42s with the cache;
// the real test goes from a 300s-timeout hang to completing. The cached path is
// byte-identical to the default path in the default config (`conservative_locals`
// off); `update_root_snapshot` falls back to the default path when the opt-in
// real ForkJoinPool lane is active, including its `conservative_locals`
// hardening. Validated: bt16=14985902, bt18=68332206 (== golden, with vs
// without). Off-switch for diagnosis/bisection: `CRATONVM_ROOTSNAP_CACHE=0`.
#[inline]
pub fn rootsnap_cache() -> bool {
    static CACHE: MemoSlot = MemoSlot::new();
    slot_bool(&CACHE, || {
        match cratonvm_types::flags::runtime_var("CRATONVM_ROOTSNAP_CACHE") {
            // Explicit opt-out only: `0` / `false` disable; unset or any other
            // value (incl. `1`, empty) enables.
            Ok(v) => v != "0" && !v.eq_ignore_ascii_case("false"),
            Err(_) => true,
        }
    })
}

// Opt-in: keep the `rootsnap_cache` frozen-frame cache valid ACROSS a GC by
// remapping its cached roots through the collection's `pointer_map`, instead of
// discarding the whole cache on every `collection_count` bump. The cache holds
// object ADDRESSES; a collection only invalidates them if it RELOCATED the
// object — and even the non-moving young sweep relocates via selective
// promotion (young→old), so the plain gen gate rebuilds the cache on nearly
// every collection during an allocation-heavy deploy. Remapping (the same proven
// operation that relocates frame locals) lets the cache survive. Fail-safe:
// `rs_cache_gen` is advanced to the post-collection count ONLY at the remap
// sites, so any GC path that relocates this thread WITHOUT remapping leaves the
// gen stale → the gate rebuilds (a stale cached address is never trusted).
// Requires `rootsnap_cache` (else `rs_cache` is always empty → no-op).
// See `update_root_snapshot` and `remap_rs_cache_after_gc`.
//
// DEFAULT-ON as of 2026-06-16 (SpringRepositoriesExtension hang). Essential for
// the hang fix: under allocation-heavy compile the plain gen gate rebuilds the
// cache on nearly every collection (selective-promote relocates young→old), so
// without survive-GC the cache only gets past `<clinit>` and the test still
// times out. Fail-safe by construction (a missed remap site only costs a rebuild,
// never correctness). Validated: bt16=14985902, bt18=68332206 (== golden, with vs
// without). Off-switch: `CRATONVM_ROOTSNAP_CACHE_SURVIVE_GC=0`.
#[inline]
pub fn rootsnap_cache_survive_gc() -> bool {
    static CACHE: MemoSlot = MemoSlot::new();
    slot_bool(&CACHE, || {
        match cratonvm_types::flags::runtime_var("CRATONVM_ROOTSNAP_CACHE_SURVIVE_GC") {
            Ok(v) => v != "0" && !v.eq_ignore_ascii_case("false"),
            Err(_) => true,
        }
    })
}

cached_is_set!(jit_dispatch_dbg, "CRATONVM_DBG_JIT_DISPATCH");
// Opt-in: enable small-method inlining in the main JIT tier-up compile path.
// Default-OFF until a `try_emit_inline_body` miscompile (Spring boot enum CCE)
// is root-caused. See `try_jit_upgrade_with_gate`.
cached_is_set!(jit_main_inline, "CRATONVM_JIT_MAIN_INLINE");
// wire-tiered-manager: OFF-THREAD codegen for the invocation tier-up trigger.
// When on, the interpreter's invocation tier-up trigger ENQUEUES a
// `CompilationTask` for the background compile thread (which runs the real
// codegen via `try_jit_compile_callee` and publishes into `shared.jit.jit_cache`)
// and DOES NOT compile inline on the mutator — the mutator keeps interpreting
// until the worker publishes, at which point the `jit_cache` fast-path flips the
// call site to `Jit`. Step-5 OSR likewise compiles off-thread when on.
//
// **DEFAULT-ON as of wire-tiered-manager Step 7** ("retire the single
// fixed-threshold inline path"): the fixed-threshold invocation path
// (`try_jit_upgrade_with_gate`) no longer compiles synchronously on the mutator
// by default. The eager *first-call* single-pass compile (`fn execute`) remains
// as the quick first tier; this gate governs the invocation-counted re-tiering
// and OSR. Opt-out — `CRATONVM_BG_COMPILE=0` (or `false`) restores the historical
// inline path (the safety net while the off-thread pipeline soaks on the
// gauntlet). See `docs/feature-designs/wire-tiered-manager.md` (Step 7).
//
// History (2026-07-14): the Hibernate H2 long-tail work briefly flipped this
// default OFF because a short, very hot H2 lateral-unnest query could finish
// its whole timeout budget in the interpreter before the background-produced
// body published. That flip silently reverted the Step-7 posture the entire
// perf line is measured on (bt18 1.9s → 4.5s alone). The starvation mechanism
// it worked around has since been addressed independently (the OSR
// tier-starvation fix and "OSR completion no longer stamps the method-entry
// tier", both on dev); suites that still need the historical inline tier-up
// can set `CRATONVM_BG_COMPILE=0` per run instead of changing the global
// default.
#[inline]
pub fn bg_compile() -> bool {
    static CACHE: MemoSlot = MemoSlot::new();
    slot_bool(&CACHE, || {
        match cratonvm_types::flags::runtime_var("CRATONVM_BG_COMPILE") {
            Ok(v) => v != "0" && !v.eq_ignore_ascii_case("false"),
            Err(_) => true,
        }
    })
}
// wire-tiered-manager Step 4 (PGO handoff C1 → C2): opt-in profile collection.
// When set, `SharedVm::new` calls `jit::profile::enable_profiling(true)` once at
// VM init, so the interpreter's existing branch / receiver / back-edge recording
// sites populate `shared.jit.profile_store` during the interpreted ("C1"/warmup)
// phase. The optimizing C2 compile then consumes that profile — the single-pass
// backend already biases branch layout + pre-populates virtual-call MICs from it,
// and (Step 4) the optimizing IR pipeline now reads branch bias too. Default-OFF:
// every `ProfileStore::record_*` short-circuits on `is_profiling_enabled()`, so
// the interpreter hot loop and all codegen are byte-for-byte unchanged unless
// opted in. See `docs/feature-designs/wire-tiered-manager.md` (Step 4).
cached_is_set!(tier_pgo, "CRATONVM_TIER_PGO");
// Invocation-count tier-up for INSTANCE methods (invokevirtual/invokeinterface).
//
// DEFAULT-ON. It was turned off wholesale in `c28bdd687` because the
// pre-decoded instance-call route could strand a live embedded-server request
// (Spring Boot `MultipartAutoConfigurationTests`) after promotion. That was a
// real hazard but the wrong scope: the stranding needs a callee that DECLARES
// AN EXCEPTION TABLE, because a direct compiled entry has no interpreter
// boundary at which the callee's own handler can be resumed. The same commit
// gated exactly that on the MIC/PIC route
// (`mic_callee_has_exception_table`), the OSR direct-call route
// (`osr_callee_bars_direct_call`) and the inline-compile route
// (`try_jit_upgrade_with_gate`) — but MISSED the `bg_compile` route, which is
// the default one: the background worker publishes and
// `execute_invokevirtual_cached`'s `jit_cache` probe promotes the site without
// consulting any gate. That gap is now closed at the promotion site, so the
// blanket default-OFF is no longer what is holding the hazard shut.
//
// Turning it off cost ~8.4x on ordinary instance-method bytecode — measured on
// `CalleeTierUpProbe`, 2 432 ns on / 18 047 ns off — because `recycle()`-shaped
// methods (plain field stores, no handlers) are exactly the ones the ban was
// never about. See `fixed-suite-bugs/tomcat/32-doc04-residual-perf-assertions-CLOSED.md`.
//
// Off-switch for diagnosis/bisection: `CRATONVM_JIT_VIRTUAL_TIERUP=0`.
#[inline]
pub fn jit_virtual_tierup() -> bool {
    static CACHE: MemoSlot = MemoSlot::new();
    slot_bool(&CACHE, || {
        match cratonvm_types::flags::runtime_var("CRATONVM_JIT_VIRTUAL_TIERUP") {
            // Explicit opt-out only: `0` / `false` disable; unset or any other
            // value enables.
            Ok(v) => v != "0" && !v.eq_ignore_ascii_case("false"),
            Err(_) => true,
        }
    })
}

/// `CRATONVM_JIT_VIRTUAL_NOMINATE_ALWAYS` — nomination is not promotion.
///
/// **DEFAULT-OFF, `=1` opts in, and the default is the measurement's.** With
/// it on, `HibfixComposeProbe2` tracks 19 methods instead of 8 and compiles 12
/// at C2 instead of 3 — and costs **0.994x** (8 interleaved reps, quiet host,
/// user CPU, ranges 15.80-16.60 against 15.86-16.45). It buys nothing here
/// because the site that does the counting is still the site that may not
/// PROMOTE: `getNow` is compiled and is still entered interpreted 40 000 times
/// in 40 000 chains. Nominating more methods is not what composition was short
/// of — that is the finding, and it is why the lever ships off rather than
/// being deleted: it is the instrument that establishes it, and the arm anyone
/// re-opening the promotion question has to run first.
///
/// `execute_invokevirtual_cached`'s tier-up chain
/// used to gate the invocation COUNTER on the same two conditions that gate
/// the SITE's promotion to a direct compiled entry — a `java/util/` receiver
/// and a callee that declares an exception table. Those two are promotion
/// hazards; neither is a reason to stop counting a callee's invocations.
///
/// The cost of conflating them is measured, not argued.
/// `CRATONVM_DBG_TIERUP_DECLINE=1` on `HibfixComposeProbe2` reports 46 364
/// declines over 40 000 chains and **every** meaningful row is
/// `receiver_is_java_util` — `java/util/concurrent/` is inside the prefix, so
/// the whole of `CompletableFuture` is refused. `CompletableFuture.getNow`
/// takes 39 998 of them and is still interpreted on call 40 000; it is 1.00
/// interpreted frame per chain in the `CRATONVM_DBG_INTERP_FRAMES` census.
/// This is the mechanism behind
/// `known-issues/perf/completablefuture-composition-is-20x-and-5-percent-compiled-20260901.md`
/// finding #2: lowering `CRATONVM_JIT_THRESHOLD` 25x bought 6 more tracked
/// methods and no time, because the methods that matter never reach the
/// counter at all.
///
/// The 2026-08-05 measurement that refused to narrow the `java/util/` prefix
/// (`retired/aqs-thread-handoff-latency-RETIRED-20260805.md` item 3, a
/// `ReentrantLock` loop 30 % SLOWER with tier-up admitted) priced ADMITTING
/// THE PROMOTION. This gate does not admit it: `promotion_barred` still
/// suppresses the `jit_cache` probe and the inline upgrade for exactly the
/// same set. Only the counter and the tiered nomination are hoisted, so a
/// nominated java.util callee becomes reachable through the doors that carry
/// their own exception-table guards (`mic_callee_has_exception_table`,
/// `osr_callee_bars_direct_call`) and never through this one.
#[inline]
pub fn jit_virtual_nominate_always() -> bool {
    static CACHE: MemoSlot = MemoSlot::new();
    slot_bool(&CACHE, || {
        // Explicit opt-IN: `1` / `true` enable, unset or anything else leaves
        // the chain exactly as it was before 2026-09-02.
        match cratonvm_types::flags::runtime_var("CRATONVM_JIT_VIRTUAL_NOMINATE_ALWAYS") {
            Ok(v) => v == "1" || v.eq_ignore_ascii_case("true"),
            Err(_) => false,
        }
    })
}

/// `CRATONVM_JIT_HOT_LOOKUP_CACHE` — default-ON, `=0` opts out.
///
/// The kill switch for the three per-call lookups this change removed from
/// paths the composition workload runs several times per chain: the two
/// uncached declared-flag reads ([`dbg_shadow`], [`needs_exact_trace`]) and
/// the un-range-guarded `lambda_proxies` probe in
/// `NativeContextImpl::invoke_virtual`. They are grouped under one switch
/// because they are one finding — a lookup on a hot path whose answer never
/// changes — and because none of them is separately interesting.
///
/// A declared-flag read is not cheap: `runtime_var_os` converts the `OsStr`,
/// FxHashes the ~20-byte name against the declared-flag SET, and then hashes
/// it AGAIN against the legacy-value MAP. `CRATONVM_DBG_FLAGREADS=1` on
/// `HibfixComposeProbe2` counted 400 000 of these in 80 000 chains, 98 % of
/// them the two names above.
#[inline]
pub fn hot_lookup_cache() -> bool {
    static CACHE: MemoSlot = MemoSlot::new();
    slot_bool(&CACHE, || {
        match cratonvm_types::flags::runtime_var("CRATONVM_JIT_HOT_LOOKUP_CACHE") {
            Ok(v) => v != "0" && !v.eq_ignore_ascii_case("false"),
            Err(_) => true,
        }
    })
}

/// `CRATONVM_JIT_VIRTUAL_PROMOTE_JAVA_UTIL` — default-OFF, `=1` opts in.
///
/// Admits a `java/util/` receiver to the cached-virtual PROMOTION, i.e. lets
/// `execute_invokevirtual_cached` enter a compiled body directly at a site the
/// `receiver_is_java_util` exclusion has always barred. The exception-table
/// half of `promotion_barred` is NOT relaxed by this: that one is a
/// correctness hazard (a handler-bearing callee entered by a direct compiled
/// call has no interpreter boundary at which its own handler can be resumed),
/// where the prefix is a performance policy.
///
/// It exists because the policy has never been priced on its own.
/// `retired/aqs-thread-handoff-latency-RETIRED-20260805.md` item 3 measured
/// `ReentrantLock` against a user subclass `MyLock extends ReentrantLock` and
/// found the subclass 0.77x — but those are two receiver classes in two loop
/// methods, so the comparison carries "different class, different call site,
/// different inlining" along with the tier-up. With this switch the SAME
/// receiver in ONE binary is the A/B.
///
/// Pair it with [`jit_virtual_nominate_always`]: with nomination barred there
/// is usually no compiled body to promote to, so a promotion arm measured
/// alone re-measures the conflated thing from the other side.
#[inline]
pub fn jit_virtual_promote_java_util() -> bool {
    static CACHE: MemoSlot = MemoSlot::new();
    slot_bool(&CACHE, || {
        match cratonvm_types::flags::runtime_var("CRATONVM_JIT_VIRTUAL_PROMOTE_JAVA_UTIL") {
            Ok(v) => v == "1" || v.eq_ignore_ascii_case("true"),
            Err(_) => false,
        }
    })
}

/// `CRATONVM_DBG_SHADOW` — one-shot shadow-stack trace in `set_jit_thread`.
///
/// Cached because it is read on EVERY interpreter->JIT boundary crossing.
/// `CRATONVM_DBG_FLAGREADS=1` on `HibfixComposeProbe2` put it at 250 135 of
/// 400 000 flag reads (3.1 per composition chain), each one an `OsStr`
/// conversion, an FxHash of the name and a probe of the declared-flag set —
/// which is what the `HashMap<&str, ()>::contains_key` and part of the
/// `__memcmp_evex_movbe` line in that page's profile actually are. The read
/// sat IN FRONT of the `ONCE` swap that makes the trace one-shot, so it kept
/// paying long after the trace could ever fire again.
#[inline]
pub fn dbg_shadow() -> bool {
    static CACHE: MemoSlot = MemoSlot::new();
    slot_bool(&CACHE, || {
        cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_SHADOW").is_some()
    })
}

/// `CRATONVM_NEEDS_EXACT_TRACE` — the sibling of [`dbg_shadow`], 141 112 reads
/// (1.76 per chain) in the same census. Its call site is
/// `NativeContextImpl::invoke_virtual`, i.e. every native that calls back into
/// Java by name; the trace it guards fires for ONE hard-coded method name.
#[inline]
pub fn needs_exact_trace() -> bool {
    static CACHE: MemoSlot = MemoSlot::new();
    slot_bool(&CACHE, || {
        cratonvm_types::flags::runtime_var_os("CRATONVM_NEEDS_EXACT_TRACE").is_some()
    })
}

/// `CRATONVM_JIT_LAMBDA_TIERUP` — count invocations of a lambda SAM
/// implementation, and enter its compiled body directly once one exists.
///
/// A lambda impl reached through `try_lambda_dispatch` used to touch neither
/// `profile_store.increment_invocation` nor `jit.jit_cache`, so it could never
/// be nominated for compilation no matter how hot: confirmed with
/// `CRATONVM_DBG_JITC=1` against `probes/SamDispatchDecompositionProbe.java` —
/// a lambda's synthetic `lambda$...` method never once appears in the
/// tiered-enqueue/bg-compile log, while the identical body on a named or
/// anonymous class compiles normally within a few hundred calls and runs ~40x
/// faster. See
/// known-issues/perf/lambda-sam-dispatch-bypasses-the-cached-invoke-path-20260817.md.
///
/// Default ON. Off-switch for diagnosis/bisection, and the kill switch a
/// same-binary A/B needs: `CRATONVM_JIT_LAMBDA_TIERUP=0`.
#[inline]
pub fn jit_lambda_tierup() -> bool {
    static CACHE: MemoSlot = MemoSlot::new();
    slot_bool(&CACHE, || {
        match cratonvm_types::flags::runtime_var("CRATONVM_JIT_LAMBDA_TIERUP") {
            // Explicit opt-out only: `0` / `false` disable; unset or any other
            // value enables.
            Ok(v) => v != "0" && !v.eq_ignore_ascii_case("false"),
            Err(_) => true,
        }
    })
}
/// `CRATONVM_JIT_LAMBDA_ADAPTER` — let a SAM call site keep an inline-cache
/// entry of its own.
///
/// A lambda receiver was the one receiver the monomorphic inline cache could
/// not hold, and the obstacle was an argument shuffle rather than anything
/// about caching: the cascade passes `(proxy, samArgs…)` and a non-capturing
/// lambda's impl wants `(samArgs…)`. With this on, a small thunk performs the
/// shuffle and tail-jumps to the impl, and the slot holds the thunk — so the
/// dispatch happens in machine code with no Rust on the path, exactly as it
/// does for a named class.
///
/// Default ON. `CRATONVM_JIT_LAMBDA_ADAPTER=0` keeps the Rust fast path
/// (`try_lambda_site_direct_call`) and is the kill switch a same-binary A/B of
/// the thunk needs; `CRATONVM_JIT_LAMBDA_SITE=0` disables that arm too, and
/// `CRATONVM_JIT_LAMBDA_TIERUP=0` disables the whole feature.
#[inline]
pub fn jit_lambda_adapter() -> bool {
    static CACHE: MemoSlot = MemoSlot::new();
    slot_bool(&CACHE, || {
        match cratonvm_types::flags::runtime_var("CRATONVM_JIT_LAMBDA_ADAPTER") {
            Ok(v) => v != "0" && !v.eq_ignore_ascii_case("false"),
            Err(_) => true,
        }
    })
}

/// `CRATONVM_JIT_LAMBDA_CAPTURE_ADAPTER` — extend that inline-cache entry to a
/// lambda that CAPTURES.
///
/// A capturing lambda's impl wants `(captures…, samArgs…)`, so its thunk has to
/// read the captured values out of the proxy object before it jumps. That is the
/// one part of the thunk that touches memory, and the only part whose
/// preconditions are not purely about registers — the proxy's field layout, the
/// capture types, and, for a reference capture, the collector's read-barrier
/// state.
///
/// It gets its own switch for that reason, and because a same-binary A/B of
/// "capturing lambdas too" against "non-capturing only" is otherwise impossible:
/// [`jit_lambda_adapter`]`=0` turns off both at once and would measure the wrong
/// difference.
///
/// Default ON. `CRATONVM_JIT_LAMBDA_CAPTURE_ADAPTER=0` leaves capturing sites on
/// the Rust fast path (`try_lambda_site_direct_call`) while non-capturing ones
/// keep their thunks.
#[inline]
pub fn jit_lambda_capture_adapter() -> bool {
    static CACHE: MemoSlot = MemoSlot::new();
    slot_bool(&CACHE, || {
        match cratonvm_types::flags::runtime_var("CRATONVM_JIT_LAMBDA_CAPTURE_ADAPTER") {
            Ok(v) => v != "0" && !v.eq_ignore_ascii_case("false"),
            Err(_) => true,
        }
    })
}

/// `CRATONVM_JIT_FJP_SUBCLASS_BLOCKLIST=1` — put back the RFJP.1 workaround that
/// forced the interpreter for every method on a `ForkJoinTask` subclass.
///
/// **Default OFF since 2026-08-27**, i.e. those methods compile. The workaround
/// guarded a `RecursiveTask<Long>.compute()` that returned 0 past recursion
/// depth ~10; that miscompile was root-caused to `Long.valueOf` boxing and
/// fixed in Session 108 (commit 6f605451d, "RFJP.1 closed as side-effect"),
/// and the blocklist was simply never taken back out. Its cost was not small:
/// `CompletableFuture$UniCompose` and `$UniRelay` extend `Completion`, which
/// extends `ForkJoinTask`, so the whole completion machinery ran interpreted.
///
/// This flag stays as a ROLLBACK LEVER, not because the defect is expected
/// back. See
/// `performance/completablefuture-composition-force-interpreted-by-a-stale-forkjointask-blocklist-FIXED-20260827.md`
/// for the evidence that retired it.
#[inline]
pub fn jit_fjp_subclass_blocklist() -> bool {
    static CACHE: MemoSlot = MemoSlot::new();
    slot_bool(&CACHE, || {
        match cratonvm_types::flags::runtime_var("CRATONVM_JIT_FJP_SUBCLASS_BLOCKLIST") {
            Ok(v) => v != "0" && !v.eq_ignore_ascii_case("false"),
            Err(_) => false,
        }
    })
}
/// `CRATONVM_JIT_LAMBDA_CONST_PROBE` — screen SAM calls whose implementation
/// body is a CONSTANT (`iconst_<n>/bipush/sipush` then `ireturn`) and report
/// any call that observes a different value.
///
/// This exists for one shape: `CompletionStages::alwaysTrue` is
/// `return true;`, and `ArrayLoop.next(int)` skips straight to `end` — ending
/// a hibernate-reactive loop after one iteration, silently and with no
/// exception — if that predicate ever answers `false`. A constant body is the
/// one case where a wrong answer needs no baseline, no repeat runs and no
/// statistics: the correct value is known from the bytecode, so the FIRST
/// wrong call is proof. See
/// `docs/known-issues/hibernate/hib-reactive-multithreaded-insertion-lazy-connection-20260822.md`
/// section 5.
///
/// Default OFF. `=1` screens; `=strict` (or `=2`) additionally refuses the
/// emitted inline-cache thunk for such impls, because a thunk tail-jumps and
/// returns straight to its compiled caller with no Rust on the path — those
/// calls are INVISIBLE to this probe, which is what
/// `site_const_opaque` counts.
#[inline]
pub fn jit_lambda_const_probe() -> bool {
    static CACHE: MemoSlot = MemoSlot::new();
    slot_bool(&CACHE, || {
        match cratonvm_types::flags::runtime_var("CRATONVM_JIT_LAMBDA_CONST_PROBE") {
            Ok(v) => v != "0" && !v.eq_ignore_ascii_case("false"),
            Err(_) => false,
        }
    })
}

/// Is the constant-return probe in its THUNK-REFUSING mode? See
/// [`jit_lambda_const_probe`] — this trades the fast path for coverage, and is
/// a diagnostic setting rather than one to measure performance under.
#[inline]
pub fn jit_lambda_const_probe_strict() -> bool {
    static CACHE: MemoSlot = MemoSlot::new();
    slot_bool(&CACHE, || {
        match cratonvm_types::flags::runtime_var("CRATONVM_JIT_LAMBDA_CONST_PROBE") {
            Ok(v) => v == "2" || v.eq_ignore_ascii_case("strict"),
            Err(_) => false,
        }
    })
}

/// `CRATONVM_JIT_STATIC_BYTECODE_CALLEE` — let a compiled caller's
/// `invokestatic` to a callee the JIT did NOT compile enter that callee through
/// a per-site cached interpreter frame template, instead of re-resolving it
/// from its class NAME on every call.
///
/// # What it is worth
///
/// MEASURED 2026-08-22, `probes/XferProbe.java` — a hot loop calling a one-line
/// `static int callee(int x) { return x + 1; }`:
///
/// | configuration | ns/op |
/// |---|---:|
/// | compiled caller -> compiled callee | 22-33 |
/// | both interpreted (`--nojit`) | 236-385 |
/// | compiled caller -> INTERPRETED callee | 1242-1902 |
///
/// Compiling the caller and not the callee was **5x slower than compiling
/// neither**. On a real application most callees are never compiled, so that
/// loss is paid continuously and cancels the JIT's wins — which is exactly the
/// "JIT 24.6 ms/op vs `--nojit` 25.8 ms/op" wash recorded for
/// `WebClientIntegrationTests`. `invokestatic` is where it concentrates:
/// `CRATONVM_DBG=mic-prof` on `probes/ReactorProbe.java` reports
/// `kind_static=2_986_402` of `disp_calls=3_356_461` — **89%**.
///
/// The virtual/interface/special half followed on 2026-08-23; see
/// [`jit_virtual_bytecode_callee`].
///
/// Default ON. `CRATONVM_JIT_STATIC_BYTECODE_CALLEE=0` restores the by-name
/// path, which is the A/B a same-binary bisection needs.
#[inline]
pub fn jit_static_bytecode_callee() -> bool {
    static CACHE: MemoSlot = MemoSlot::new();
    slot_bool(&CACHE, || {
        match cratonvm_types::flags::runtime_var("CRATONVM_JIT_STATIC_BYTECODE_CALLEE") {
            Ok(v) => v != "0" && !v.eq_ignore_ascii_case("false"),
            Err(_) => true,
        }
    })
}

/// `CRATONVM_JIT_VIRTUAL_BYTECODE_CALLEE` — the invokevirtual / invokeinterface
/// twin of [`jit_static_bytecode_callee`].
///
/// The static half left 71% of a reactive workload's dispatch tail on the
/// by-name path, because real application code is overwhelmingly virtual and
/// interface. This is that half.
///
/// # What it is worth
///
/// MEASURED 2026-08-23 on this tree, `probes/XferProbe2.java`, one binary:
///
/// | arm | compiled -> compiled | compiled -> INTERPRETED | both interpreted |
/// |---|---:|---:|---:|
/// | `invokevirtual` | 33.4 ns | **2031.4 ns** | 535.4 ns |
/// | `invokeinterface` | 36.0 ns | **2298.9 ns** | 839.8 ns |
///
/// i.e. compiling the caller and not the callee was 3.8x (virtual) and 2.7x
/// (interface) SLOWER than compiling neither — the same shape the static half
/// was fixed for, at the invoke kinds where the volume actually is.
///
/// Default ON. Set to `0` to restore the by-name `invoke_or_native` tail, which
/// is the A/B a same-binary bisection needs.
#[inline]
pub fn jit_virtual_bytecode_callee() -> bool {
    static CACHE: MemoSlot = MemoSlot::new();
    slot_bool(&CACHE, || {
        match cratonvm_types::flags::runtime_var("CRATONVM_JIT_VIRTUAL_BYTECODE_CALLEE") {
            Ok(v) => v != "0" && !v.eq_ignore_ascii_case("false"),
            Err(_) => true,
        }
    })
}

/// `CRATONVM_JIT_INDY_BRIDGE` — let a compiled method EXECUTE an
/// `invokedynamic` through a runtime bridge instead of lowering it to an
/// uncommon trap.
///
/// # What it decides
///
/// Without the bridge, a method containing any non-`StringConcatFactory` indy
/// takes an unconditional reason-8 trap on its first compiled execution and is
/// retired with `MakeNotCompilable`; OSR is refused for it outright, and
/// method-wide. So every method that CREATES a lambda runs interpreted for the
/// life of the process. MEASURED on `probes/IndyScopeProbe.java`, current
/// `dev` against this branch, four interleaved pairs on a quiet host:
///
/// | arm | HotSpot | bridge OFF | bridge ON |
/// |---|---:|---:|---:|
/// | loop whose method creates the lambda | 1.4 ns | 698-757 ns | **22.4-24.2 ns** |
/// | identical loop, lambda hoisted out | 2.5 ns | 22.4-24.1 ns | 22.4-24.8 ns |
/// | a fresh lambda per call | 2.7 ns | 881-936 ns | **267-287 ns** |
///
/// ~30x, and the first row lands exactly on the second — the penalty for
/// putting a `->` inside the loop's own method is gone rather than reduced.
///
/// On the workloads, same binary and same switch, medians of six interleaved
/// runs with `ReactorProbe`'s non-reactive `control` arm flat at 25-34 ns/op:
/// `Fp16VectorDotBench` **1.93x** (12 157-13 064 -> 6137-6695 ns/lane),
/// `ReactorProbe` operator assembly **1.41x** (8876 -> 6281 ns/op),
/// assemble+run 1.15x, `mono chain` 1.17x. `ExchangeProbe` is a WASH, and that
/// is not a contradiction: its request-path methods are called ~60 times, below
/// the C1 threshold, so ~98% of it never compiles and there is nothing for a
/// compiled-code fix to move.
///
/// # Why it is a switch and not a constant
///
/// Because the trade is not one-signed. Bridging makes the METHOD compilable,
/// which is a large win wherever the indy is a small part of a hot method; it
/// also makes the indy ITSELF more expensive than the interpreter's own path,
/// because the bridge builds a synthetic frame per call. On a workload whose
/// hot methods are mostly lambda creation the second term can dominate. This
/// switch is what lets that be measured on ONE binary instead of two.
///
/// Default ON. `CRATONVM_JIT_INDY_BRIDGE=0` restores the trap.
#[inline]
pub fn jit_indy_bridge() -> bool {
    static CACHE: MemoSlot = MemoSlot::new();
    slot_bool(&CACHE, || {
        match cratonvm_types::flags::runtime_var("CRATONVM_JIT_INDY_BRIDGE") {
            Ok(v) => v != "0" && !v.eq_ignore_ascii_case("false"),
            Err(_) => true,
        }
    })
}

/// `CRATONVM_JIT_LAMBDA_SITE` — the JIT-side half of the lambda tier-up: a
/// compiled caller's SAM call served straight from the call site's own cached
/// target (`jit::helpers::try_lambda_site_direct_call`).
///
/// Default ON, and separate from [`jit_lambda_tierup`] on purpose. The two
/// halves of this feature serve DIFFERENT callers — this one a compiled caller,
/// the interpreter's one-shot (`execute_jit_call_oneshot`) an interpreted one —
/// and a workload reaches whichever its callers happen to be. Turning this one
/// off routes a compiled caller's SAM call back through the generic path and
/// therefore through the interpreted half, which is what makes each half
/// separately measurable, separately bisectable, and separately TESTABLE: see
/// `vm/tests/lambda_jit_oneshot_tests.rs`, which exists because the correctness
/// suite otherwise never reaches the interpreted half at all.
///
/// `CRATONVM_JIT_LAMBDA_SITE=0` disables it; `CRATONVM_JIT_LAMBDA_TIERUP=0`
/// disables both halves.
#[inline]
pub fn jit_lambda_site() -> bool {
    static CACHE: MemoSlot = MemoSlot::new();
    slot_bool(&CACHE, || {
        match cratonvm_types::flags::runtime_var("CRATONVM_JIT_LAMBDA_SITE") {
            Ok(v) => v != "0" && !v.eq_ignore_ascii_case("false"),
            Err(_) => true,
        }
    })
}

/// `CRATONVM_NATIVE_STRING_REGEX` — route `String.replaceAll` / `replaceFirst`
/// / `matches` and the literal `String.replace(CharSequence,CharSequence)` to
/// CratonVM's fast cached Rust natives instead of the real JDK bytecode. The
/// real-JDK `java.util.regex` engine runs interpreted (every `Matcher` step
/// crosses the VM→native String-accessor boundary), and the literal `replace`
/// overload runs an interpreted per-char scan — 10–600× slower than HotSpot
/// for regex-heavy build steps (ShrinkWrap archive packaging, Spring Boot's
/// `PluginXmlParser`, AsciiDoc/Javadoc `{@code}` rewriting). The natives use the
/// `regex` / `fancy-regex` crates (bounded compile cache, Java-faithful
/// replacement `$N`/`${name}`/`\`-escapes, ASCII-default `\d`/`\w`/`\s`/`\b`)
/// and `str::replace`, and are validated byte-identical to HotSpot.
///
/// **DEFAULT-ON** (opt-out `CRATONVM_NATIVE_STRING_REGEX=0`/`false`). These are
/// faithful alternate implementations (like intrinsics), not synthetic stubs,
/// and a parity battery confirms HotSpot-identical output incl. non-ASCII Perl
/// classes; the opt-out is the safety net if an app hits a regex-feature gap.
#[inline]
pub fn native_string_regex() -> bool {
    static CACHE: MemoSlot = MemoSlot::new();
    slot_bool(&CACHE, || {
        match cratonvm_types::flags::runtime_var("CRATONVM_NATIVE_STRING_REGEX") {
            // Explicit opt-out only: `0` / `false` disable; unset or any other
            // value (incl. `1`, empty) enables.
            Ok(v) => v != "0" && !v.eq_ignore_ascii_case("false"),
            Err(_) => true,
        }
    })
}

/// `CRATONVM_NATIVE_MATCHER_FIND` — route `java.util.regex.Matcher.find()` /
/// `find(int)` / `start()` / `start(int)` / `end()` / `end(int)` / `group()`
/// / `group(int)` to a fast Rust-native fast path operating directly on the
/// REAL OpenJDK `Matcher`/`Pattern` object layout (fields resolved by name,
/// never a hardcoded slot index), instead of the real JDK bytecode's
/// interpreted state-machine engine.
///
/// `CRATONVM_NATIVE_STRING_REGEX` above only accelerates the `String`
/// convenience methods (`replaceAll`/`replaceFirst`/`matches`); it does
/// nothing for the extremely common explicit `while (m.find()) { ...;
/// m.group(N); }` idiom, which still ran the interpreted engine — this is
/// what the README's `String/Regex` QuickBench kernel actually measures.
/// `start`/`end`/`group` are included because they sit on the same hot loop
/// and only read state `find`/`find(int)` already populate — measured,
/// leaving them interpreted left most of the per-iteration cost on the
/// table (find-only: ~3.2x of the full win; find+start+end+group: the rest).
/// See `native-builtins/src/lib.rs`'s "real-JDK-layout `find()`/`find(int)`
/// fast path" module banner for the full design (offset-table UTF-16↔UTF-8
/// bridging, bail-to-real-bytecode escape hatch for anything the fast path
/// can't faithfully reproduce, and the `hitEnd`/`requireEnd` approximation
/// residual).
///
/// **DEFAULT-ON** (opt-out `CRATONVM_NATIVE_MATCHER_FIND=0`/`false`), after a
/// 141-case parity battery confirmed `find`/`find(int)`/`start`/`end`/`group`
/// results byte-identical to HotSpot (the only observed differences are the
/// documented `hitEnd`/`requireEnd` approximation and the pre-existing
/// Windows-console non-ASCII display artifact — same residual category
/// `CRATONVM_NATIVE_STRING_REGEX` already accepted before its own flag
/// flipped default-ON). Reduced the README `String/Regex (10K)` kernel's
/// CratonVM-vs-HotSpot ratio from 37.6x to roughly 16x on an equivalent
/// standalone benchmark (see docs/known-issues for the measurement).
#[inline]
pub fn native_matcher_find() -> bool {
    static CACHE: MemoSlot = MemoSlot::new();
    slot_bool(&CACHE, || {
        match cratonvm_types::flags::runtime_var("CRATONVM_NATIVE_MATCHER_FIND") {
            // Explicit opt-out only: `0` / `false` disable; unset or any other
            // value (incl. `1`, empty) enables.
            Ok(v) => v != "0" && !v.eq_ignore_ascii_case("false"),
            Err(_) => true,
        }
    })
}
cached_is_set!(jit_mic_dbg, "CRATONVM_DBG_JIT_MIC");
cached_is_set!(jit_entry_dbg, "CRATONVM_DBG_JIT_ENTRY");
cached_is_set!(jit_callee_deopt_dbg, "CRATONVM_DBG_CALLEE_DEOPT");
cached_is_set!(jit_putfield_diag, "CRATONVM_DBG_JIT_PUTFIELD");
cached_is_set!(letsgo_dbg, "CRATONVM_DBG_LETSGO");
/// `CRATON_JIT_PFO_TRACE` / `CRATON_JIT_PFI_TRACE` / `CRATON_JIT_NEWARRAY_TRACE`
/// — suspect-pattern tracing in the JIT putfield/newarray helpers. These sit on
/// the hottest allocation/store paths (`jit_putfield_object` runs once per
/// reference field store), where an uncached `env::var_os` is a per-call
/// environment-block scan under the process env lock — measured as a
/// multi-second cost on allocation-heavy runs (bintrees18: ~69M ref stores).
cached_is_set!(jit_pfo_trace, "CRATON_JIT_PFO_TRACE");
cached_is_set!(jit_pfi_trace, "CRATON_JIT_PFI_TRACE");
cached_is_set!(jit_newarray_trace, "CRATON_JIT_NEWARRAY_TRACE");
/// `CRATONVM_NO_CTOR_DIRECT_CALL` — opt out of eager-compiling a trivial
/// constructor at an `invokespecial …<init>` site into a direct CALL. When
/// set, such sites fall back to the per-allocation `jit_invoke_dispatch` slow
/// path (the pre-fix behaviour). Read only at JIT compile time, so caching is
/// for tidiness rather than hot-path cost.
cached_is_set!(ctor_direct_call_disabled, "CRATONVM_NO_CTOR_DIRECT_CALL");
/// `CRATONVM_DBG_CTOR_FIX` — diagnostic tracing for the trivial-constructor
/// elision (which ctor sites are deferred / resolved elidable). Read only at
/// JIT compile time.
cached_is_set!(ctor_fix_dbg, "CRATONVM_DBG_CTOR_FIX");
/// `CRATONVM_NO_OSR_CTOR_BIND` — opt OUT of routing a NON-elidable
/// `invokespecial …<init>()V` site in the **OSR** compile door through the
/// eager-compile + direct-bind path every other statically-bound site there
/// already takes. When set, such sites fall back to the per-allocation
/// `jit_invoke_dispatch` slow path.
///
/// The off-switch exists because this exact reroute was tried on 2026-08-13
/// and reverted: it made `compile_with_param_slots` refuse the enclosing
/// method, and an OSR refusal marks the method OSR-denied for the process, so
/// the hot loop interpreted forever (4.5x SLOWER). The cause was a
/// direct-bound site with no `JitInvokeInfo` — a hole this door has since
/// closed for its sibling non-`()V` admission. Read only at JIT compile time.
cached_is_set!(osr_ctor_bind_disabled, "CRATONVM_NO_OSR_CTOR_BIND");
/// `CRATONVM_JIT_REAL_NEW_SITE_FLAGS` — put the REAL `has_prim_init` /
/// `has_finalizer` at a `new` site compiled through the interpreter's
/// first-call door or the OSR door, instead of the conservative
/// `(true, true)` those two doors hard-code.
///
/// OPT-IN, and the reason is a measurement rather than a doubt about
/// correctness. Setting the real flags is what makes `bytecode_walk`'s
/// `skip_helper` reachable at all from those doors, which is the in-tree TODO
/// the `fastthreadlocal-2e9-iteration-throughput-wall` page investigated. It
/// works — and it buys nothing, because `emit_inline_tlab_new` does not
/// actually allocate inline: its own comment records that the raw compiled
/// cursor bump was routed back through the checked runtime helper after it
/// left a malformed young-space span under Elasticsearch merge churn. Only
/// the header writes are inline, so `skip_helper` removes a
/// `jit_post_tlab_init` call and leaves the allocation cost where it was.
///
/// Measured (Azure Linux, interleaved, same binary, `probes/CtorShapeRateProbe`):
/// `new Object()` 111.4 ns/op with the inline arm off, 118.0 ns/op with it on;
/// the real `new FastThreadLocal<Boolean>()` loop was no better in 3 of 4
/// rounds. Turn this on again when the JIT-emitted bump can share
/// `Tlab::alloc_initialized`'s publication contract — at that point it is the
/// switch that makes the inline path worth having.
cached_is_set!(jit_real_new_site_flags, "CRATONVM_JIT_REAL_NEW_SITE_FLAGS");

// ── Frame-trace and interpreter hot-path flags ──────────────────────────

/// `CRATONVM_TRIVIAL_GETTER` — off-switch for `execute_invokevirtual_cached`'s
/// stackless `aload_0; getfield; <x>return` accessor fast path. `0`/`off`/
/// `false`/`no` disables it; anything else (including unset) leaves it on.
///
/// The fast path reimplements the `getfield` opcode's value semantics, so a
/// divergence between the two would be a silent-wrong-value bug rather than a
/// crash. It is also the single biggest change to `--nojit` allocation and
/// safepoint timing on accessor-heavy workloads, which makes it the first
/// suspect whenever an interpreter-only run starts producing nondeterministic
/// wrong answers. Being able to A/B it within ONE binary is what let the
/// Hibernate HQL mis-parse be attributed to the moving young collector instead
/// (see `fixed-suite-bugs/hibernate/hib-bytebuddy-20260730-FIXED.md`);
/// keep the switch so the next such question costs one run, not one build.
#[inline]
pub fn trivial_getter_fast_path() -> bool {
    static CACHE: MemoSlot = MemoSlot::new();
    slot_bool(&CACHE, || {
        match cratonvm_types::flags::runtime_var("CRATONVM_TRIVIAL_GETTER") {
            Ok(v) => !matches!(v.trim(), "0" | "off" | "false" | "no"),
            Err(_) => true,
        }
    })
}

/// `CRATONVM_TRIVIAL_GETTER_VERIFY` — cross-check every trivial-accessor fast
/// path hit against the loader-aware `getfield` resolver and report any
/// divergence. Expensive (it performs the full resolution the fast path exists
/// to avoid); diagnostic use only.
cached_is_set!(trivial_getter_verify, "CRATONVM_TRIVIAL_GETTER_VERIFY");

cached_is_set!(frame_trace, "CRATONVM_FRAME_TRACE");
cached_is_set!(iae_trace_os, "CRATONVM_IAE_TRACE");
cached_is_set!(bd_debug, "CRATONVM_BD_DEBUG");
cached_is_set!(dbg_native_shadow, "CRATONVM_DBG_NATIVE_SHADOW");
cached_is_set!(nocode_dbg, "CRATONVM_DBG_NOCODE");
cached_is_set!(nsme_dbg, "CRATONVM_DBG_NSME");
cached_is_set!(cce_dbg, "CRATONVM_DBG_CCE");
/// `CRATONVM_DBG_OVERLAY` — a native `set_field`/`get_field` whose value tag
/// is destructively cross-type with the bound class's declared field
/// descriptor at that slot (a primitive written to a reference field, or a
/// reference written to a primitive field). This is the "synthetic overlay
/// bound to a real JDK class" corruption: the slot's real descriptor coerces
/// the overlay value to null / numeric and silently loses it (the
/// LinkedList$ListItr cursor bug). The hunter logs class + slot + value +
/// descriptor + native caller so every instance can be enumerated in one run.
cached_is_set!(overlay_corruption_dbg, "CRATONVM_DBG_OVERLAY");
/// `CRATONVM_DBG_CORRUPT_CELL` - when a VM-side field read decodes a `Value`
/// cell with an out-of-range discriminant, name the RECEIVER and the Java frames
/// that reached it. The collector's own guard reports the cell; only this side
/// can report who was holding the reference into a swept-then-re-served block.
/// Costs one relaxed load per `NativeContext::get_field` while armed and nothing
/// at all while it is not.
cached_is_set!(corrupt_cell_dbg, "CRATONVM_DBG_CORRUPT_CELL");
/// `CRATONVM_DBG_CORRUPT_CELL_SELFTEST` — fabricate one corrupt-cell hit at the
/// first interpreter `getfield` and one at the first `set_field`, so a run can
/// prove the DOOR reporter and the safepoint BACKSTOP both actually speak. A
/// silent diagnostic and a broken one look identical from the outside, and this
/// instrument has already been read the wrong way round once.
cached_is_set!(corrupt_cell_selftest, "CRATONVM_DBG_CORRUPT_CELL_SELFTEST");
cached_is_set!(lambda_dbg, "CRATONVM_DBG_LAMBDA");
cached_is_set!(resume_pc_dbg, "CRATONVM_DBG_RESUME_PC");
/// `CRATONVM_DBG_BADRECV` — on a getfield/putfield/array/invoke receiver whose
/// pointer is not a valid heap address (the H2 TestScript SEGV: a corrupted
/// `Object(Some(ptr=6))` reaching `get_field` → header read faults), log the
/// Java frame stack + field + Rust backtrace and raise an NPE instead of
/// dereferencing the wild pointer, so the origin can be localized.
cached_is_set!(badrecv_dbg, "CRATONVM_DBG_BADRECV");
/// `CRATONVM_DBG_FIELDADDR` — trace put/get of specific instance fields
/// (object address + resolved slot) to localize a write that does not reach
/// the read site (e.g. the EnhancedQueueExecutor constructor field gap).
cached_is_set!(field_addr_dbg, "CRATONVM_DBG_FIELDADDR");
/// `CRATONVM_DBG_JETTY` — trace dispatch into the `org/eclipse/jetty/start/`
/// launcher package (every invokevirtual/invokespecial receiver + args) so
/// the boot-test NPE at `Main.start(Main.java:397)` can be pinpointed.
cached_is_set!(dbg_jetty, "CRATONVM_DBG_JETTY");
/// `CRATONVM_DBG_JETTY2` — Jetty classpath dispatch trace, read with an
/// UNCACHED `cratonvm_types::flags::runtime_var_os(...).is_some()` inside `execute_invokevirtual_vtable_fast`
/// (i.e. a `GetEnvironmentVariableW` syscall on the virtual-call dispatch path).
/// Cached.
cached_is_set!(dbg_jetty2, "CRATONVM_DBG_JETTY2");
// `CRATONVM_DBG_VDISP` -- virtual-dispatch diagnostic. This sits on the
// invokevirtual/invokeinterface miss path and must not call into the process
// environment on every dispatch.
cached_is_set!(dbg_vdisp, "CRATONVM_DBG_VDISP");
cached_is_set!(dbg_ccsprobe, "CRATONVM_DBG_CCSPROBE");
// `CRATONVM_DBG_TPE_SHAPE` was removed 2026-08-06 together with the predicate
// it instrumented. It reported every outcome of
// `threadpool_executor_has_real_workers`, the `ThreadPoolExecutor.execute`
// receiver-shape probe, for L10's "is this predicate universally true?"
// question. L11 item 7 answered that question by deleting the eight call sites
// and the predicate: there is no receiver-shape decision left to report, so a
// flag that can only ever print nothing is worse than no flag. The measurement
// it took is kept in `L10-blocker-threadpool-init-DONE-20260806.md`.
// `CRATONVM_DBG_HANG_SAMPLE` -- temporary diagnostic for the AOT
// bean-registration hang investigation (2026-07-13). Periodically samples
// the method being invoked in `execute_invoke_kind` (every Nth call) so a
// hung process's last-known activity can be inspected from stderr without a
// native debugger. Not perf-sensitive: gated behind a cached env lookup and
// only actually prints once every 200,000 calls.
cached_is_set!(dbg_hang_sample, "CRATONVM_DBG_HANG_SAMPLE");
cached_is_set!(dbg_pbstart, "CRATONVM_DBG_PBSTART");
cached_is_set!(dbg_bblp, "CRATONVM_DBG_BBLP");
cached_is_set!(dbg_jitc, "CRATONVM_DBG_JITC");
cached_is_set!(dbg_unpark_miss, "CRATONVM_DBG_UNPARK_MISS");
cached_is_set!(dbg_jit_ldc, "CRATONVM_DBG_JIT_LDC");
cached_is_set!(trace_unimplemented, "CRATONVM_TRACE_UNIMPLEMENTED");
/// `CRATONVM_DBG_HOTPATH_COUNTS` — temporary call-count instrumentation for
/// the silent-hang-no-signature-cluster throughput residual investigation
/// (2026-07-13). Tallies invocations of several suspected interpreter
/// dispatch hot-path functions and periodically reports counts, independent
/// of wall-clock timing (robust to host contention noise). See
/// `interpreter::hotpath_counts`.
cached_is_set!(dbg_hotpath_counts, "CRATONVM_DBG_HOTPATH_COUNTS");
/// `CRATONVM_JIT_NO_BACKEDGE_POLL_GATE` -- restore the unconditional
/// `safepoint_check` call on every interpreted backward branch.
///
/// The gate it disables lives in `execute_frame_from_index`: a back edge now
/// tests `stw_requested` plus the hoisted async-exception slot (two relaxed
/// loads) before paying for the call, because the only work in that call not
/// repeated by the loop-top poll thirty lines later was an async-exception
/// drain costing three locked read-modify-writes. Measured at 33-37 ns per
/// back edge against HotSpot's 4.9 ns, so the two arms have to be comparable
/// inside one binary.
cached_is_set!(no_backedge_poll_gate, "CRATONVM_JIT_NO_BACKEDGE_POLL_GATE");

/// `CRATONVM_JIT_NO_FIELD_FAST_PATH` -- disable the quickened `getfield` /
/// `putfield` arms in the interpreter (per-site receiver-class + offset
/// memo, raw compact-layout load/store). Off restores the full
/// `op_getfield` / `op_putfield` handler on every instance field access.
/// Token: `CRATONVM_JIT=-field-fast-path`.
cached_is_set!(no_field_fast_path, "CRATONVM_JIT_NO_FIELD_FAST_PATH");

/// `CRATONVM_JIT_NO_OSR_INLINE_GATE` -- call `try_osr_with_backoff` on every
/// backward branch instead of only once `Frame::backward_count` has reached
/// the smallest threshold the call could accept. Token:
/// `CRATONVM_JIT=-osr-inline-gate`.
cached_is_set!(no_osr_inline_gate, "CRATONVM_JIT_NO_OSR_INLINE_GATE");

/// `CRATONVM_JIT_NO_INVOKE_FAST_DOOR` -- disable the monomorphic
/// `invokevirtual` / `invokeinterface` fast door (borrowed cache entry,
/// compact argument transfer, per-method invocation counter). Off routes
/// every cache hit through `execute_invokevirtual_cached`. Token:
/// `CRATONVM_JIT=-invoke-fast-door`.
cached_is_set!(no_invoke_fast_door, "CRATONVM_JIT_NO_INVOKE_FAST_DOOR");

/// `CRATONVM_JIT_NO_NONVIRTUAL_FAST_DOOR` -- disable the monomorphic
/// `invokestatic` / `invokespecial` fast doors (borrowed cache entry,
/// verbatim `CompactValue` argument transfer, and for `invokestatic` a
/// per-method invocation counter in place of the sharded profile-store
/// lock). Off routes every cache hit through the general dispatcher.
/// Token: `CRATONVM_JIT=-nonvirtual-fast-door`.
cached_is_set!(no_nonvirtual_fast_door, "CRATONVM_JIT_NO_NONVIRTUAL_FAST_DOOR");

/// `CRATONVM_JIT_NO_FRAME_FILL_FAST` -- restore the `clear()` + per-argument
/// `push()` + `resize()` locals build in `Frame`, instead of sizing both
/// buffers once with the filler already in place and writing the arguments by
/// index. Token: `CRATONVM_JIT=-frame-fill-fast`.
cached_is_set!(no_frame_fill_fast, "CRATONVM_JIT_NO_FRAME_FILL_FAST");
/// `CRATONVM_DBG_BYTECODE_DUMP` -- temporary raw-bytecode + mnemonic
/// disassembly dump (2026-07-15, JRubyScriptTemplateTests round 3): see
/// `push_frame_and_fire_entry`'s own doc comment for the full story --
/// dumps the runtime-generated bytecode for any frame whose class name
/// contains "version" (JRuby's URI-mangled class name for
/// `rubygems/version.rb`), since these snippets are synthesized at
/// runtime and have no static `.class` file `javap` can decompile.
cached_is_set!(dbg_bytecode_dump, "CRATONVM_DBG_BYTECODE_DUMP");

/// `CRATONVM_DBG_DUPCALL_FILTER` -- temporary double-invocation trace
/// (2026-07-23, WFLYCTL0079 investigation): see `push_frame_and_fire_entry`'s
/// own doc comment. Logs every entry to
/// `ParallelExtensionAddHandler$ExtensionInitializeTask.call()` with the
/// receiver's identity, to test whether the boot executor ever double-
/// dispatches the same task (which would explain the rare "attribute
/// already registered" duplicate-registration failure).
cached_is_set!(dbg_dupcall_filter, "CRATONVM_DBG_DUPCALL_FILTER");

/// `CRATONVM_INVOKE_VIRTUAL_ENTRY_TRACE` — AOT management-context trace
/// (`aotContributedInitializerStartsManagementContext`), on the
/// `invoke_on_class_shared_inner` lambda-dispatch path.
///
/// PERF (2026-07-25): the two call sites read this with an **uncached**
/// `cratonvm_types::flags::runtime_var_os` on *every* `invokevirtual` entry, and — because the env
/// probe was the left operand of the `&&` — paid a `getenv` before the cheap
/// `method_name ==` compare could short-circuit it. `getenv` takes the process
/// environ lock and linearly scans environ, so this alone was ~10% of the
/// CratonBench `hashmap` phase (10M virtual calls). Same class of bug as the
/// `CRATONVM_DBG_BLOCKGC` note in `vm_exec.rs`. Keep this predicate as the
/// left operand: a `MemoSlot` read is cheaper than the string compare,
/// so it short-circuits the common (unset) case in a single load.
cached_is_set!(
    invoke_virtual_entry_trace,
    "CRATONVM_INVOKE_VIRTUAL_ENTRY_TRACE"
);

// ── PERF 2026-07-25: the five `getenv` hogs found by an LD_PRELOAD tally ──
//
// An `LD_PRELOAD` shim counting `getenv()` by name over the CratonBench
// `hashmap` phase (10M put/get) recorded **~130 million calls**, ≈13 per
// benchmark iteration:
//
//     60,008,062  CRATONVM_DBG_LOADER_TRACE
//     20,001,011  CRATONVM_DBG_MH_STACK
//     20,001,011  CRATONVM_DBG_MH_ADAPTER
//     20,001,005  CRATONVM_DBG_STACKLESS
//     10,002,013  CRATONVM_DBG_H2TRACE
//
// All were uncached `cratonvm_types::flags::runtime_var`/`var_os` probes sitting on the `new`
// opcode and `try_stackless_invoke` paths, and most had the env probe as the
// LEFT operand of an `&&` whose right operand is a cheap string compare — so
// the `getenv` (which takes the process environ lock and linearly scans
// environ) ran unconditionally and the cheap test could never short-circuit
// it. Together they were ~11% of the phase's CPU. Caching is the fix; keep
// these predicates as the left operand, since a `MemoSlot` read is
// cheaper than the string compares they guard.
cached_is_set!(dbg_mh_stack, "CRATONVM_DBG_MH_STACK");
cached_is_set!(dbg_mh_adapter, "CRATONVM_DBG_MH_ADAPTER");
cached_is_set!(dbg_stackless, "CRATONVM_DBG_STACKLESS");
cached_is_set!(dbg_h2trace, "CRATONVM_DBG_H2TRACE");

// ── Flags read via `env::var(...).is_ok()` ──────────────────────────────

/// `CRATONVM_NO_LOCAL_LIVENESS` — disable the per-bci local-variable
/// liveness filter in the interpreter frame GC root scan (restores the
/// scan-every-object-typed-slot behaviour; see `runtime::local_liveness`).
cached_is_ok!(no_local_liveness, "CRATONVM_NO_LOCAL_LIVENESS");
/// `CRATONVM_DBG_ARRLEN` — diagnostic for a non-array reaching
/// `NativeContextImpl::array_length`. PERF (2026-07-25): was an uncached
/// `cratonvm_types::flags::runtime_var` (which allocates a `String` on a hit and takes the environ
/// lock either way) evaluated on every `array_length` call whose receiver is
/// not an array. Cached for the same reason as
/// [`invoke_virtual_entry_trace`].
cached_is_ok!(dbg_arrlen, "CRATONVM_DBG_ARRLEN");
/// `CRATONVM_DBG_LOADER_TRACE` — MVStore `RootReference` loader-identity
/// trace. The single worst `getenv` offender on the interpreter hot path
/// (60M calls in one CratonBench `hashmap` run): ~33 uncached call sites,
/// several on the `new` opcode path. See the tally note above
/// [`dbg_mh_stack`]. Both `.is_ok()` and `.is_some()` spellings existed at
/// the call sites; they are equivalent here (the flag is never set to
/// non-UTF-8), so one predicate serves both.
cached_is_ok!(dbg_loader_trace, "CRATONVM_DBG_LOADER_TRACE");
/// `CRATONVM_DBG=coerce` — the loader-split coercion diagnostic.
///
/// The flag lives in `NativeFlags` because `native-builtins`' `Array.set` /
/// `Field.set` refusals were its only readers. The JIT's `invokestatic`
/// loader-faithful owner override prints under the SAME token deliberately: it
/// is the same subject — one binary name, two loaders — seen from the other
/// end, and someone who turns the token on to ask "is a loader split behind
/// this?" wants both halves of the answer in one run. Read from the parsed
/// flags rather than the environment, so the grouped spelling is what decides.
#[inline]
pub fn dbg_coerce() -> bool {
    cratonvm_types::flags().natives.dbg_coerce
}
/// `CRATONVM_DBG_ISOLATED_CNF` — narrow trace for the isolated-URLClassLoader
/// "class not found" hard-fail in [`resolve_class_loader_aware`]: prints the
/// name, the referencing class, and the Java frames whenever an isolating
/// loader's own `loadClass` declined a name and the VM therefore refuses to
/// fall back to the global store. Fires at most a handful of times per run,
/// unlike `dbg_loader_trace`.
cached_is_ok!(dbg_isolated_cnf, "CRATONVM_DBG_ISOLATED_CNF");
cached_is_ok!(trace_sb_filter, "CRATONVM_TRACE_SB_FILTER");
cached_is_ok!(nsee_trace, "CRATONVM_NSEE_TRACE");
cached_is_ok!(iae_trace, "CRATONVM_IAE_TRACE");
cached_is_ok!(athrow_dbg, "CRATONVM_DBG_ATHROW");
/// `CRATONVM_DBG_STUBLOADER` -- trace the "would fabricate a synthetic stub,
/// ask the calling class's own ClassLoader first" fallback in
/// `NativeContextImpl::load_class`.
cached_is_ok!(dbg_stub_loader, "CRATONVM_DBG_STUBLOADER");
cached_is_ok!(npe_invoke_dbg, "CRATONVM_DBG_NPE_INVOKE");
/// `CRATONVM_DBG_MODSTATIC` — JBoss-Modules `<clinit>`/static-dispatch
/// diagnostic. This was read with an UNCACHED `cratonvm_types::flags::runtime_var(...).is_ok()`
/// from `ensure_class_initialized_shared` (the per-barrier class-init gate
/// fired on every getstatic/putstatic/new/invokestatic, *before* the
/// fast-path "already initialized" check) and from two method-resolution
/// sites — i.e. a ~500 ns `GetEnvironmentVariableW` syscall on a large
/// fraction of all executed bytecodes. That single uncached lookup
/// dominated steady-state interpreter throughput (a getstatic-only loop
/// measured ~567 ns/iter, most of it this call). Cached like its siblings.
cached_is_ok!(modstatic_dbg, "CRATONVM_DBG_MODSTATIC");
/// `CRATON_HASHTABLEOFINT_TRACE` — niche getfield/putfield diagnostic that was
/// read with an UNCACHED `cratonvm_types::flags::runtime_var(...).is_ok()` on EVERY getfield and
/// putfield (the two most common opcodes in object-oriented bytecode) — a
/// per-field-access `GetEnvironmentVariableW` syscall. Cached.
cached_is_ok!(hashtableofint_trace, "CRATON_HASHTABLEOFINT_TRACE");
cached_is_ok!(dbg_toarray, "CRATONVM_DBG_TOARRAY");
/// `CRATON_BAOS_DBG` — ByteArrayOutputStream `buf`/`count` putfield diagnostic,
/// read with an UNCACHED `cratonvm_types::flags::runtime_var_os(...).is_some()` on EVERY putfield.
/// Cached.
cached_is_set!(baos_dbg, "CRATON_BAOS_DBG");

/// Perf: SINGLE consolidated fast-path gate for ALL of the per-field-access
/// diagnostic blocks in the `Getfield`/`Putfield` opcode handlers (the two
/// hottest opcodes in object-oriented bytecode). Each individual gate below is
/// already a cached `OnceLock` bool, but the hot handlers previously branched
/// through ~4–5 of them in sequence on every single field access. This OR of
/// every field-diagnostic flag is itself cached once, so the common
/// no-diagnostics case takes exactly ONE branch (`if any_field_diag()`) instead
/// of one per gate, and the per-gate checks inside that block are reached only
/// when at least one diagnostic var is actually set.
///
/// Behaviour is identical to checking each gate individually: with no
/// diagnostic var set this returns `false` and every inner block was a no-op
/// anyway (including the `CRATONVM_DBG_BADRECV` wild-pointer guard, which only
/// acts when its own var is set); with any var set this returns `true` and the
/// inner per-gate checks select exactly the same blocks as before. The two
/// interpreter-local gates (`CRATONVM_DBG_STRAYSTACK`, `CRATONVM_DBG_NULLTHIS`)
/// are folded in here too so the single gate covers every field-diagnostic path.
#[inline]
pub fn any_field_diag() -> bool {
    static CACHE: MemoSlot = MemoSlot::new();
    slot_bool(&CACHE, || {
        field_addr_dbg()
            || badrecv_dbg()
            || hashtableofint_trace()
            || bd_debug()
            || baos_dbg()
            || cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_STRAYSTACK").is_some()
            || cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_NULLTHIS").is_some()
    })
}

/// `CRATONVM_DBG_CHARSET=1` — targeted diagnostic for the bare
/// `NullPointerException: charset` blocker (keycloak26 Picocli /
/// Hazelcast JAXP-XPath). When set, the `Athrow` opcode handler and
/// `throw_runtime_error` dump the full live Java thread stack
/// (`class.method:pc` for every frame) the moment an NPE whose message
/// is exactly `charset` is thrown — pinpointing the JDK method and app
/// call site that dereferenced a null `Charset`.
cached_is_ok!(charset_dbg, "CRATONVM_DBG_CHARSET");

// ── `CRATONVM_STRICT_SWALLOWS` is read for its value, not just presence ──

/// `CRATONVM_STRICT_SWALLOWS=1` switches several diagnostic sites into a
/// hard-fail mode for swallowed exceptions. Cached as a single bool —
/// only the literal `"1"` is honoured (matching the existing call sites
/// `... .as_deref() == Some("1")`).
#[inline]
pub fn strict_swallows() -> bool {
    static CACHE: MemoSlot = MemoSlot::new();
    slot_bool(&CACHE, || {
        match cratonvm_types::flags::runtime_var("CRATONVM_STRICT_SWALLOWS") {
            Ok(v) => v == "1",
            Err(_) => false,
        }
    })
}

#[inline]
pub fn jit_scalar_new() -> bool {
    static CACHE: MemoSlot = MemoSlot::new();
    slot_bool(&CACHE, || {
        cratonvm_types::flags::runtime_var("CRATONVM_JIT_SCALAR_NEW").map_or(true, |v| v != "0")
    })
}

/// C1→C2 supersede (default-ON): after the background worker publishes a C1
/// body for a call-free/allocation-free IR-eligible method, it enqueues a
/// Low-priority C2 recompile whose publish replaces the C1 body and bumps
/// the supersede epoch (per-thread invoke caches re-resolve on their next
/// hit). `=0`/`false` keeps every method at its first-published tier (the
/// pre-supersede behaviour) — the safety net while the upgrade soaks.
#[inline]
/// The native-shadow CALLER seal, as a whole.
///
/// Default ON and load-bearing for correctness — see the call site. Off is a
/// MEASUREMENT configuration (`CRATONVM_JIT=-native-shadow-caller-seal`) for
/// pricing the seal's ceiling before anyone rebuilds it per-site; a run with it
/// off may misbehave and must never ship.
/// `CRATONVM_GC_NOFLAG_DEPOSIT_SKIP_JIT_SCAN=1` — let the NON-BLOCKING root
/// snapshot deposit skip `scan_active_jit_frames`.
///
/// MEASUREMENT LEVER, DEFAULT OFF. This is a GC-safety-relevant switch and it
/// must earn its default with stress, not with an argument.
///
/// What it is for. `deposit_root_snapshot_inner` runs the JIT conservative scan
/// on BOTH its paths — the flag-raising one (a thread about to block) and the
/// no-flag one (`deposit_root_snapshot_no_flag`: the wake path, and
/// `NativeContext::refresh_root_snapshot`, which the synthetic-stream drain
/// loops call). The scan's expensive half is the A5 unregistered-frame probe, a
/// raw word walk of `[scanner_sp, stack_high)`. `CRATONVM_DBG_ROOTPROF=1` on a
/// `StackWalker.walk` over a 120-frame stack, 8000 walks, on dev
/// `ccdafa676`:
///
/// ```text
/// scan_active_jit_frames by caller: gc-roots=0 safepoint=0 blocked-deposit=39,521
/// jitprobe calls=39,496 words=255,087,460
/// ```
///
/// 255 million words — 2 GB of native stack read — with EVERY scan attributed
/// to the deposit path and NONE to the two the A5 block's own comment claims it
/// is gated to ("only on the GC root-scan path").
///
/// Why it is not obviously safe, and why it is off. `xt_root_scan`'s module doc
/// states the opposite obligation for a peer that is executing JIT code: "its
/// only coverage is the snapshot it published at its last object-returning
/// native call — and if its JIT slots advanced past that snapshot, the collector
/// is blind to the new roots and the non-moving sweep reclaims a still-live
/// object." A no-flag deposit IS such a publish. Two things argue it is
/// nevertheless redundant there, and neither is a measurement:
///
///   * a thread on the no-flag path is a COUNTED mutator (the flag is what
///     excludes it), so a peer STW waits for it to arrive at a safepoint, where
///     `update_root_snapshot` runs the same scan — or freezes it via
///     `xt_root_scan`, which conservatively scans its registers and stack
///     directly and is a superset of what this contributes;
///   * the A5 hit's other effect, `mark_moving_young_coverage_incomplete`, is
///     reset per cycle by `begin_moving_young_coverage_cycle`, so a hit recorded
///     at deposit time — before any cycle — is cleared before the collection
///     that would consume it.
///
/// The hole in both: `CRATONVM_XT_JIT_ROOT_SCAN=0` disables the first, and a
/// peer whose `Rip` is in a JIT *helper* rather than pure JIT code is resumed
/// rather than taken over.
///
/// # MEASURED 2026-08-26: it engages completely and buys NOTHING. Route closed.
///
/// So the safety question above never has to be answered. In-binary ABBA on
/// `probes/StackWalkerFindFirstProbe.java`, 2000 iterations, medians of 6, with
/// `CRATONVM_DBG_JIT_SCAN_PROF`'s exit-time `scans=` as the engagement counter:
///
/// | arm | `scans` | depth 40 full-drain | depth 120 full-drain |
/// |---|---:|---:|---:|
/// | keep (default) | 8,495 | 512 ms | 1,421 ms |
/// | skip | **0** | 510 ms | 1,462 ms |
///
/// The early-match control does not move either (154 -> 145 ms at depth 40,
/// 411 -> 348 at depth 120, both inside the spread).
///
/// This is NOT the "changed a call site that never runs" failure the sibling
/// `quartz-stackwalker` page records four times. The skip fires COMPLETELY —
/// 8,495 scans to zero, removing all 255 million words (2 GB) of native stack
/// the deposit path reads over 8000 walks. It is inert because that work is
/// genuinely cheap: the band is ~52 KB of hot, cached stack walked
/// sequentially with a per-word range check, which is ~0.5 ns/word and ~2% of
/// the run at most. A big BYTE count is not a big TIME cost, and 2 GB of L2-resident
/// sequential reads is the case where the two come apart.
///
/// Kept, default off, for the same reason `jit_native_shadow_caller_seal` is
/// kept: the lever plus its number is what stops the next person spending a day
/// re-deriving that the deposit reads gigabytes, concluding it must be the
/// bottleneck, and then having to reason about `collect_all_root_snapshots`
/// consuming every alive thread's snapshot in order to remove it.
pub fn noflag_deposit_skips_jit_scan() -> bool {
    static CACHE: MemoSlot = MemoSlot::new();
    slot_bool(&CACHE, || {
        cratonvm_types::flags::runtime_var("CRATONVM_GC_NOFLAG_DEPOSIT_SKIP_JIT_SCAN")
            .is_ok_and(|v| v != "0" && v != "false")
    })
}

pub fn jit_native_shadow_caller_seal() -> bool {
    static CACHE: MemoSlot = MemoSlot::new();
    slot_bool(&CACHE, || {
        cratonvm_types::flags::runtime_var("CRATONVM_JIT_NATIVE_SHADOW_CALLER_SEAL")
            .map_or(true, |v| v != "0" && v != "false")
    })
}

/// The `interface_blind_possible_shadow` arm of the native-shadow seal.
///
/// Default ON — it is a correctness guard. `CRATONVM_JIT=-native-shadow-interface-blind`
/// turns it off so its cost can be measured against a real workload: it is the
/// class-blind arm ("does ANY registered native have this (name, descriptor)"),
/// and on a Spring Boot startup the seal it belongs to excludes more methods
/// from the JIT than reach C2.
pub fn jit_native_shadow_interface_blind() -> bool {
    static CACHE: MemoSlot = MemoSlot::new();
    slot_bool(&CACHE, || {
        cratonvm_types::flags::runtime_var("CRATONVM_JIT_NATIVE_SHADOW_INTERFACE_BLIND")
            .map_or(true, |v| v != "0" && v != "false")
    })
}

/// Transitive eager callee compilation (default-ON).
///
/// A statically bound call site can only be bound to a raw `CALL` if the callee is
/// compiled ALREADY. `try_jit_compile_callee_slow`'s resolver used to answer only
/// from `jit_cache`, so a caller compiled one moment before its callee bound that
/// site to the generic `jit_invoke_dispatch` helper — and a compiled body never
/// re-binds. Measured on `probes/org/junit/jupiter/api/CompileOrderProbe.java`:
/// 478 ns/iter when the chain compiles top-down against 73 when it compiles
/// bottom-up, entirely accounted for by ~1 helper round trip per iteration
/// (`CRATONVM_DBG_MIC_PROF=1`: `disp_calls` 2 003 538 against 3 926).
///
/// `CRATONVM_JIT='-eager-callee-chain'` (or `CRATONVM_JIT_EAGER_CALLEE_CHAIN=0`)
/// restores the one-level behaviour. Correct either way: the fallback is the
/// checked dispatch helper.
#[inline]
pub fn jit_eager_callee_chain() -> bool {
    static CACHE: MemoSlot = MemoSlot::new();
    slot_bool(&CACHE, || {
        cratonvm_types::flags::runtime_var("CRATONVM_JIT_EAGER_CALLEE_CHAIN")
            .map_or(true, |v| v != "0" && v != "false")
    })
}

pub fn c2_supersede() -> bool {
    static CACHE: MemoSlot = MemoSlot::new();
    slot_bool(&CACHE, || {
        cratonvm_types::flags::runtime_var("CRATONVM_C2_SUPERSEDE")
            .map_or(true, |v| v != "0" && v != "false")
    })
}

#[inline]
pub fn jit_ir_call() -> bool {
    static CACHE: MemoSlot = MemoSlot::new();
    slot_bool(&CACHE, || {
        cratonvm_types::flags::runtime_var("CRATONVM_JIT_IR_CALL").map_or(true, |v| v != "0")
    })
}

#[inline]
pub fn jit_ir_call_special() -> bool {
    static CACHE: MemoSlot = MemoSlot::new();
    slot_bool(&CACHE, || {
        cratonvm_types::flags::runtime_var("CRATONVM_JIT_IR_CALL_SPECIAL")
            .map_or(true, |v| v != "0")
    })
}

#[inline]
pub fn jit_ir_long() -> bool {
    static CACHE: MemoSlot = MemoSlot::new();
    slot_bool(&CACHE, || {
        cratonvm_types::flags::runtime_var("CRATONVM_JIT_IR_LONG").map_or(true, |v| v != "0")
    })
}

#[inline]
pub fn jit_ir_call_virtual() -> bool {
    static CACHE: MemoSlot = MemoSlot::new();
    slot_bool(&CACHE, || {
        cratonvm_types::flags::runtime_var("CRATONVM_JIT_IR_CALL_VIRTUAL")
            .map_or(true, |v| v != "0" && !v.eq_ignore_ascii_case("false"))
    })
}

/// PGO-02: guarded monomorphic-virtual-call inlining. Default-OFF (absent or
/// `"0"`/`"false"` => disabled) - a new speculative JIT lowering soaks behind
/// an opt-in flag per the c2 remediation wave's own rule, not the inverted
/// default some `ir-*` levers above use. `=1` (or any other non-`"0"`/
/// `"false"` value) opts in. See
/// `docs/feature-designs/profile-guided-inlining.md`.
#[inline]
pub fn jit_guarded_virtual_inline() -> bool {
    static CACHE: MemoSlot = MemoSlot::new();
    slot_bool(&CACHE, || {
        cratonvm_types::flags::runtime_var("CRATONVM_JIT_GUARDED_VIRTUAL_INLINE")
            .is_ok_and(|v| v != "0" && !v.eq_ignore_ascii_case("false"))
    })
}

#[inline]
pub fn jit_ir_fp() -> bool {
    static CACHE: MemoSlot = MemoSlot::new();
    slot_bool(&CACHE, || {
        cratonvm_types::flags::runtime_var("CRATONVM_JIT_IR_FP").map_or(true, |v| v != "0")
    })
}

#[inline]
pub fn inline_allow_static() -> bool {
    static CACHE: MemoSlot = MemoSlot::new();
    slot_bool(&CACHE, || {
        cratonvm_types::flags::runtime_var_os("CRATONVM_INLINE_ALLOW_STATIC").is_some()
    })
}

// NOTE (real-cdi-bean-container Step 3): the former `real_spring_startup()` gate
// (and its `CRATONVM_SYNTHETIC_SPRING_STARTUP` opt-out) has been removed. Spring's
// `org.springframework.core.metrics` startup-metrics subsystem now runs its real
// bytecode unconditionally — the no-op `spring_startup_bootstrap.rs` shim, the
// `vm_exec.rs` force arms, and the `vm_util.rs` `<clinit>`-swallow / `post_clinit_fixup`
// backfill for `ApplicationStartup` are all gone. Validated 10/10 == HotSpot on the
// Spring Boot functional battery and pinned by `vm/tests/nested_clinit_startup.rs`.

// ── `CRATONVM_REAL` — synthetic-stub differential switch ─────────────────

/// Parsed, process-lifetime view of the `CRATONVM_REAL` (and legacy
/// `CRATONVM_REAL_JCA`) selector. Decides, per class, whether a
/// `NativeKind::SyntheticStub` registration should be bypassed in favour of
/// running the class's real bytecode — so fakes can be differentially
/// compared against the genuine implementation.
///
/// Parsed exactly once from the environment by [`real_bytecode_selector`].
/// When neither env var is set, every field is empty/false and
/// [`RealSelector::prefers_real`] returns `false` for all classes — so the
/// dispatcher behaves byte-for-byte as it does today.
pub struct RealSelector {
    /// `CRATONVM_REAL` contained the `all` token — prefer real for every class.
    all_flag: bool,
    /// Exact internal-form class names listed in `CRATONVM_REAL`
    /// (e.g. `java/util/stream/Collectors`).
    exact_classes: HashSet<String>,
    /// The `jca` group alias was requested (via the `jca` token in
    /// `CRATONVM_REAL`, or the legacy `CRATONVM_REAL_JCA` var being set):
    /// prefer real for the `java/security/`, `javax/crypto/`, and
    /// `sun/security/` families.
    jca_group: bool,
}

impl RealSelector {
    /// Parse a `RealSelector` from the two env vars. `real` is the raw value
    /// of `CRATONVM_REAL` (if present); `jca_legacy` is whether the legacy
    /// `CRATONVM_REAL_JCA` var is set (non-empty).
    fn parse(real: Option<&str>, jca_legacy: bool) -> Self {
        let mut all_flag = false;
        let mut exact_classes = HashSet::new();
        let mut jca_group = jca_legacy;
        if let Some(raw) = real {
            for tok in raw.split(',') {
                let tok = tok.trim();
                if tok.is_empty() {
                    continue;
                }
                match tok {
                    "all" => all_flag = true,
                    "jca" => jca_group = true,
                    other => {
                        exact_classes.insert(other.to_string());
                    }
                }
            }
        }
        RealSelector {
            all_flag,
            exact_classes,
            jca_group,
        }
    }

    /// `true` iff real bytecode should be preferred over a synthetic-stub
    /// native for `class_name` (internal form, e.g.
    /// `java/util/stream/Collectors`). O(1)-ish: a bool, a hash-set membership
    /// test, and at most three `starts_with` checks.
    #[inline]
    pub fn prefers_real(&self, class_name: &str) -> bool {
        if self.all_flag {
            return true;
        }
        if self.exact_classes.contains(class_name) {
            return true;
        }
        if self.jca_group
            && (class_name.starts_with("java/security/")
                || class_name.starts_with("javax/crypto/")
                || class_name.starts_with("sun/security/"))
        {
            return true;
        }
        false
    }
}

// ── Ad-hoc debug traces on interpreter hot paths ────────────────────────
//
// Each of these was an inline `cratonvm_types::flags::runtime_var_os(...)` sitting on a path the
// interpreter runs per invoke / per putfield / per `if_acmpne`. An
// `LD_PRELOAD` `getenv` tally over `RrwlSingle` (300k uncontended
// `ReentrantReadWriteLock`/`ReentrantLock` lock-unlock pairs, the reduced form
// of H2 `TestFileSystem.testConcurrent`) counted **6.9 million** `getenv`
// calls — about 23 per lock/unlock — with these six names accounting for
// 6,900,696 of them:
//
// ```text
// 4501107  CRATONVM_DBG_GSE                          (invoke-cache lookup, twice per lookup)
// 1101202  CRATONVM_DBG_FIELD_WATCH                  (every putfield + every field retarget)
//  599999  CRATONVM_DBG_WATCHREF
//  398881  CRATONVM_DBG_ASSERTEQ                     (every JIT-ABI invoke)
//  199000  CRATONVM_EXEC_FRAME_TRACE                 (every frame entry)
//  100507  CRATONVM_ACTIVE_PROFILES_IDENTITY_TRACE   (every if_acmpne)
// ```
//
// `perf record -F 999` attributed ~3.5% of the run to `getenv` and its
// callers. This is the same defect class as the 2026-07-23 fix for
// `callee_saved_gpr_local_homes_enabled()` in `vm/src/jit/skip_list.rs` — see
// `fixed-suite-bugs/h2-suite-bugs/bug-h2-testfilesystem-testconcurrent-async-hang-FIXED.md`,
// which is where that one was found and where these were.
//
// NOTE: like every other helper in this module, these read the legacy
// per-flag variable directly rather than through
// `cratonvm_types::flags()`, so the grouped `CRATONVM_DBG=gse` spelling does
// not reach them. That is pre-existing behaviour for all 30 helpers here, not
// something this change introduced; the direct `CRATONVM_DBG_GSE=1` spelling
// works exactly as before.

cached_is_set!(dbg_gse, "CRATONVM_DBG_GSE");
cached_is_set!(dbg_field_watch, "CRATONVM_DBG_FIELD_WATCH");

/// Class-name filter for the [`dbg_field_watch`] getfield/putfield ledger.
///
/// `CRATONVM_DBG_FIELD_WATCH` started life as a bare on/off switch whose
/// ledger was hard-wired to the H2 `Page` / `RootReference` investigation.
/// The variable's *value* is now a comma-separated list of class-name
/// substrings, so the same ledger can be aimed at any "the store ran but the
/// read sees null" question (e.g.
/// `CRATONVM_DBG_FIELD_WATCH=AbstractControllerService`). Call sites pass
/// `<class>.<field>` where the field name is already to hand, so a pattern
/// may also name a single field
/// (`CRATONVM_DBG_FIELD_WATCH=AbstractControllerService.controller`) and keep
/// the ledger down to the handful of lines that answer the question. An
/// on/off-shaped
/// value (empty, `1`, `true`, `on`, `yes`) keeps the original H2 filter, so
/// every existing invocation behaves exactly as before.
#[inline]
pub fn field_watch_class_matches(name: &str) -> bool {
    fn patterns() -> Vec<String> {
        let raw =
            cratonvm_types::flags::runtime_var("CRATONVM_DBG_FIELD_WATCH").unwrap_or_default();
        let trimmed = raw.trim();
        if trimmed.is_empty() || matches!(trimmed, "1" | "true" | "on" | "yes") {
            return Vec::new();
        }
        trimmed
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect()
    }
    fn matches(pats: &[String], name: &str) -> bool {
        if pats.is_empty() {
            return name.contains("Page") || name.contains("RootReference");
        }
        pats.iter().any(|p| name.contains(p.as_str()))
    }
    // Not `memoized_ref`: this is called per watched field access, and that
    // helper leaks its recomputation. Build the list on the stack instead —
    // the override arm is test-only and already off any path that matters.
    if cratonvm_types::flags::overrides_active() {
        return matches(&patterns(), name);
    }
    static CACHE: OnceLock<Vec<String>> = OnceLock::new();
    matches(CACHE.get_or_init(patterns), name)
}
cached_is_set!(dbg_watchref, "CRATONVM_DBG_WATCHREF");
cached_is_set!(dbg_asserteq, "CRATONVM_DBG_ASSERTEQ");
cached_is_set!(exec_frame_trace, "CRATONVM_EXEC_FRAME_TRACE");
cached_is_set!(
    active_profiles_identity_trace,
    "CRATONVM_ACTIVE_PROFILES_IDENTITY_TRACE"
);

/// Once-initialized accessor for the `CRATONVM_REAL` / `CRATONVM_REAL_JCA`
/// differential switch. Parses the env vars exactly once; every subsequent
/// call serves the cached [`RealSelector`].
#[inline]
pub fn real_bytecode_selector() -> &'static RealSelector {
    static CACHE: OnceLock<RealSelector> = OnceLock::new();
    memoized_ref(&CACHE, || {
        let real = cratonvm_types::flags::runtime_var("CRATONVM_REAL").ok();
        let jca_legacy = cratonvm_types::flags::runtime_var_os("CRATONVM_REAL_JCA")
            .map(|v| !v.is_empty())
            .unwrap_or(false);
        RealSelector::parse(real.as_deref(), jca_legacy)
    })
}

#[cfg(test)]
mod tests {
    use super::parse_osr_backedge_enabled;
    use super::{parse_enforce_shadow_scope, EnforceShadowScope};

    /// A `cached_is_set!` / `cached_is_ok!` predicate answers exactly one
    /// question: "was this environment variable explicitly set?". Call sites
    /// almost always want a different one: "is this feature active?". The two
    /// coincide only while the feature's resolved default is *itself* a bare
    /// presence test on the same variable — and they stop coinciding, silently
    /// and without any call site changing, the moment that default moves.
    ///
    /// That is exactly how the real-ForkJoinPool root-snapshot-cache bypass
    /// died: `real_forkjoinpool()` was a presence test on
    /// `CRATONVM_REAL_FORKJOINPOOL`, the flag's default became
    /// `!present(CRATONVM_SYNTHETIC_FORKJOINPOOL) || present(...)`, and the
    /// predicate started answering `false` on the very configuration it
    /// guarded. Nothing failed; the guard simply stopped running.
    ///
    /// So: no presence predicate in this file may name a variable that
    /// `types/src/flags.rs` resolves with a compound default. Either the flag
    /// default is a bare presence test (the two agree), or the call sites must
    /// read the resolved flag instead of a presence test.
    /// The memo must not be a *second* latch behind the flag snapshot.
    ///
    /// Every helper in this file is read through `memoized_with`, which
    /// recomputes while a `flags` override is installed. This test forces the
    /// memo to populate FIRST (the state a real test binary is always in by the
    /// time it runs), then checks the override still wins.
    ///
    /// `CRATONVM_FRAME_TRACE` is the probe: `cached_is_set!`, read on every
    /// frame push/pop, and inert unless set.
    #[test]
    fn an_override_beats_an_already_populated_memo() {
        // Populate the memo from the ambient environment.
        let latched = super::frame_trace();
        assert!(
            !latched,
            "the test environment must not have CRATONVM_FRAME_TRACE set"
        );

        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_FRAME_TRACE", Some("1"))],
            || {
                assert!(
                    super::frame_trace(),
                    "the memo swallowed the override — it is a second latch again"
                );
            },
        );

        assert!(
            !super::frame_trace(),
            "the memo must not have latched the override's value"
        );
    }

    /// The same, for a hand-written helper that reads a *value* rather than
    /// testing presence, and whose default is ON.
    #[test]
    fn an_override_beats_the_memo_for_value_flags() {
        assert!(super::bg_compile(), "bg_compile defaults ON; memo now warm");
        cratonvm_types::flags::with_thread_overrides(&[("CRATONVM_BG_COMPILE", Some("0"))], || {
            assert!(!super::bg_compile());
        });
        assert!(super::bg_compile(), "the guard must restore the default");

        let base = super::jit_invocation_threshold();
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_THRESHOLD", Some("7"))],
            || assert_eq!(super::jit_invocation_threshold(), 7),
        );
        assert_eq!(super::jit_invocation_threshold(), base);
    }

    /// A thread-scoped override must not leak into a neighbouring test's view
    /// of the memo — the property that makes thread scope the safe default.
    #[test]
    fn a_thread_override_does_not_publish_through_the_shared_memo() {
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_FRAME_TRACE", Some("1"))],
            || {
                assert!(super::frame_trace());
                let elsewhere = std::thread::spawn(super::frame_trace)
                    .join()
                    .expect("probe thread");
                assert!(
                    !elsewhere,
                    "another thread saw this thread's override through the memo"
                );
            },
        );
    }

    #[test]
    fn no_presence_predicate_shadows_a_compound_flag_default() {
        const ENV_CACHE_SRC: &str = include_str!("env_cache.rs");
        const FLAGS_SRC: &str = include_str!("../../../types/src/flags.rs");

        // Every env var behind a cached presence predicate in this file.
        let mut presence_vars: Vec<&str> = Vec::new();
        for line in ENV_CACHE_SRC.lines() {
            let l = line.trim_start();
            if l.starts_with("cached_is_set!(") || l.starts_with("cached_is_ok!(") {
                if let Some(var) = l.split('"').nth(1) {
                    presence_vars.push(var);
                }
            }
        }
        assert!(
            presence_vars.len() > 20,
            "the scanner found only {} presence predicates — it stopped matching \
             the macro call shape it audits, so this gate is inert",
            presence_vars.len()
        );

        // `include_str!` embeds the file's RAW bytes, and this repository is
        // checked out with CRLF on Windows. Every `,\n` boundary search below
        // would then match nothing (the bytes are `,\r\n`): `initializer_around`
        // would run from the last `{` in the whole file to EOF, hand back a
        // ~60 KB slab that trivially contains `||`, and report a long list of
        // "compound" offenders that are nothing of the sort — the first being
        // `CRATONVM_IAE_TRACE`, whose initializer is a plain
        // `present(src, ..)`. Normalise once so this gate asks the SAME
        // question on a CRLF and an LF checkout. (Only `flags.rs` needs it:
        // `str::lines()` already strips a trailing `\r` from the env-cache
        // scan above.)
        let flags_src = FLAGS_SRC.replace("\r\n", "\n");
        let flags_src = flags_src.as_str();

        // The flags.rs field initializer that resolves a given variable: from
        // just after the previous initializer's `,` up to this one's.
        fn initializer_around(src: &str, idx: usize) -> &str {
            let mut start = src[..idx].rfind(",\n").map(|p| p + 2).unwrap_or(0);
            if let Some(brace) = src[start..idx].rfind('{') {
                start += brace + 1;
            }
            let end = src[idx..].find(",\n").map(|p| idx + p).unwrap_or(src.len());
            &src[start..end]
        }

        let mut offenders: Vec<(&str, String)> = Vec::new();
        for var in &presence_vars {
            let needle = format!("present(src, \"{var}\")");
            let mut from = 0;
            while let Some(rel) = flags_src[from..].find(&needle) {
                let idx = from + rel;
                from = idx + needle.len();
                let init = initializer_around(flags_src, idx);
                // A default that can be true with the variable unset always
                // reaches for another term: `||`, `&&`, or a negated presence.
                if init.contains("||") || init.contains("&&") || init.contains("!present") {
                    offenders.push((var, init.split_whitespace().collect::<Vec<_>>().join(" ")));
                }
            }
        }

        assert!(
            offenders.is_empty(),
            "these env vars have a compound (non-presence) default in flags.rs \
             but are still answered by a presence predicate in env_cache.rs, so \
             the predicate now means \"explicitly requested\" and not \"active\": \
             {offenders:?}"
        );
    }

    #[test]
    fn osr_backedge_defaults_on_and_has_explicit_opt_outs() {
        assert!(parse_osr_backedge_enabled(None));
        assert!(parse_osr_backedge_enabled(Some("")));
        assert!(parse_osr_backedge_enabled(Some("1")));
        assert!(parse_osr_backedge_enabled(Some("true")));
        assert!(!parse_osr_backedge_enabled(Some("0")));
        assert!(!parse_osr_backedge_enabled(Some("false")));
        assert!(!parse_osr_backedge_enabled(Some("OFF")));
        assert!(!parse_osr_backedge_enabled(Some(" no ")));
    }
    /// `CRATONVM_ENFORCE_NATIVE_SHADOW` is a §1.4 dial AND a subsystem
    /// selector, and the two spellings that already meant something keep
    /// meaning it.
    #[test]
    fn enforce_shadow_scope_parses_the_three_spellings() {
        assert_eq!(parse_enforce_shadow_scope(None), EnforceShadowScope::Off);
        assert_eq!(
            parse_enforce_shadow_scope(Some("")),
            EnforceShadowScope::Off
        );
        assert_eq!(
            parse_enforce_shadow_scope(Some("  ")),
            EnforceShadowScope::Off
        );
        assert_eq!(
            parse_enforce_shadow_scope(Some("0")),
            EnforceShadowScope::Off
        );
        for on in ["1", "all", "ALL", "true", "Yes", "on"] {
            assert_eq!(
                parse_enforce_shadow_scope(Some(on)),
                EnforceShadowScope::All,
                "{on}"
            );
        }
        assert_eq!(
            parse_enforce_shadow_scope(Some("javax/management/")),
            EnforceShadowScope::Prefixes(vec!["javax/management/".to_string()])
        );
        // Dotted spelling and whitespace both normalise, because a reader who
        // has just been looking at a census (dots) and one who has been looking
        // at the registry (slashes) must not get different behaviour.
        assert_eq!(
            parse_enforce_shadow_scope(Some(" java.util.logging. , javax/management/ ")),
            EnforceShadowScope::Prefixes(vec![
                "java/util/logging/".to_string(),
                "javax/management/".to_string()
            ])
        );
    }

    /// A prefix list that parses to NOTHING must read as off, not as armed —
    /// otherwise a typo produces an inert run that looks like a clean result.
    #[test]
    fn an_empty_prefix_list_is_off_not_vacuously_armed() {
        let s = parse_enforce_shadow_scope(Some(",  , ,"));
        assert_eq!(s, EnforceShadowScope::Prefixes(vec![]));
        assert!(s.is_off(), "an empty prefix list must not read as armed");
        assert!(!s.covers("javax/management/MBeanServer"));
    }

    /// `covers` is what dispatch asks, and it must be narrow: arming one
    /// subsystem must not enforce §1.4 on every other receiver in the VM.
    #[test]
    fn enforce_shadow_scope_covers_only_the_named_subsystem() {
        let all = EnforceShadowScope::All;
        assert!(all.covers("java/lang/String"));
        assert!(all.covers("javax/management/MBeanServer"));

        let jmx = parse_enforce_shadow_scope(Some("javax/management/"));
        assert!(!jmx.is_off());
        assert!(jmx.covers("javax/management/MBeanServer"));
        assert!(jmx.covers("javax/management/openmbean/CompositeDataSupport"));
        assert!(
            !jmx.covers("java/lang/String"),
            "the JMX dial must not reach java.lang"
        );
        assert!(!jmx.covers("java/util/logging/LogManager"));

        let two = parse_enforce_shadow_scope(Some("java/util/logging/,javax/management/"));
        assert!(two.covers("java/util/logging/LogManager"));
        assert!(two.covers("javax/management/MBeanServer"));
        assert!(!two.covers("java/lang/System"));

        assert!(!EnforceShadowScope::Off.covers("javax/management/MBeanServer"));
    }
}

/// The positive half of `execute()`'s static JIT-eligibility short-circuit —
/// `JitRealm::jit_gate_pass`.
///
/// Default ON. `CRATONVM_JIT_GATE_PASS_MEMO=0` restores the pre-fix behaviour
/// (re-run the whole gate, including the O(bytecode) native-shadow scan, on
/// every `execute()` entry for every method that passes it) so the fix can be
/// A/B'd in one binary. Off is a MEASUREMENT configuration; it is correct, just
/// slow.
pub fn jit_gate_pass_memo() -> bool {
    static CACHE: MemoSlot = MemoSlot::new();
    slot_bool(&CACHE, || {
        cratonvm_types::flags::runtime_var("CRATONVM_JIT_GATE_PASS_MEMO")
            .map_or(true, |v| v != "0" && v != "false")
    })
}
