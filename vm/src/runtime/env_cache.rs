// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Process-lifetime cache for the `CRATONVM_*` debug/trace environment
//! variables that are read from interpreter and JIT hot paths.
//!
//! `std::env::var` / `std::env::var_os` are surprisingly expensive on every
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
//! which has used this pattern since the original audit.
//!
//! Each helper is `#[inline]` so the cold first-call cost (one `getenv`
//! plus the `OnceLock::get_or_init` CAS) is paid exactly once per flag,
//! and steady-state cost collapses to a relaxed load of the `OnceLock`.

use std::collections::HashSet;
use std::sync::OnceLock;

/// Build a boolean predicate that returns `true` iff the named env var is
/// **set** (any value, including the empty string), matching the semantics
/// of `std::env::var_os(NAME).is_some()`.
macro_rules! cached_is_set {
    ($name:ident, $env:literal) => {
        #[inline]
        pub fn $name() -> bool {
            static CACHE: OnceLock<bool> = OnceLock::new();
            *CACHE.get_or_init(|| std::env::var_os($env).is_some())
        }
    };
}

/// Build a boolean predicate matching `std::env::var(NAME).is_ok()` —
/// behaviourally identical to `is_set` on every platform we target, but
/// kept as a separate macro so the call sites that previously used `var`
/// instead of `var_os` keep their exact semantics (e.g. the var being
/// unset returns `false`; an invalid-UTF-8 OsString would technically
/// have differed, but we don't set our flags to non-UTF-8 in practice).
macro_rules! cached_is_ok {
    ($name:ident, $env:literal) => {
        #[inline]
        pub fn $name() -> bool {
            static CACHE: OnceLock<bool> = OnceLock::new();
            *CACHE.get_or_init(|| std::env::var($env).is_ok())
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
    static CACHE: OnceLock<bool> = OnceLock::new();
    *CACHE.get_or_init(|| match std::env::var("CRATONVM_DISABLE_JIT") {
        Ok(v) => !v.is_empty() && v != "0",
        Err(_) => false,
    })
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
    *CACHE.get_or_init(|| {
        std::env::var("CRATONVM_JIT_THRESHOLD")
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
    static CACHE: OnceLock<bool> = OnceLock::new();
    *CACHE.get_or_init(|| {
        parse_osr_backedge_enabled(std::env::var("CRATONVM_JIT_OSR").ok().as_deref())
    })
}

/// `CRATONVM_LOADER_AWARE_RESOLUTION` — loader-faithful `CONSTANT_Class`
/// resolution (ProxyClassReuseTest / IsoProbe family).
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
/// soak; see `docs/known-issues/hib-proxyclassreuse-loader-blind-class-
/// resolution.md`.
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
/// Empty or `"0"` ⇒ disabled; any other value ⇒ enabled. Read once and cached.
#[inline]
pub fn loader_aware_resolution() -> bool {
    static CACHE: OnceLock<bool> = OnceLock::new();
    *CACHE.get_or_init(|| match std::env::var("CRATONVM_LOADER_AWARE_RESOLUTION") {
        // Explicitly set: preserve the original opt-out semantics (empty or
        // "0" disables; anything else enables) so `CRATONVM_LOADER_AWARE_
        // RESOLUTION=0` still forces the old (gate-off) behavior verbatim.
        Ok(v) => !v.is_empty() && v != "0",
        // Unset: new default is enabled (see doc comment above).
        Err(_) => true,
    })
}

/// `CRATONVM_TIER_OSR_BACKEDGE` — wire-tiered-manager Step 6 — per-frame
/// back-edge count at which OSR is first attempted (`Frame::should_try_osr`'s
/// `osr_threshold` argument), default 1000 (the historical `OSR_THRESHOLD`
/// const in `interpreter.rs`). This is the *live* OSR trigger on BOTH the
/// default inline-OSR path and the Step-5 background-OSR path (which is why it
/// is a VM-side env knob, separate from the tiered manager's policy
/// `osr_threshold`). Lowering it makes hot loops OSR sooner (useful for
/// gauntlet tuning / quick repros); raising it defers OSR. Invalid/unset →
/// useful profile/warmup). `None` when unset/invalid, so the caller keeps its
/// own default (`OSR_THRESHOLD`); `0` clamps to `1`. Read once and cached.
#[inline]
pub fn tier_osr_backedge() -> Option<u32> {
    static CACHE: OnceLock<Option<u32>> = OnceLock::new();
    *CACHE.get_or_init(|| {
        std::env::var("CRATONVM_TIER_OSR_BACKEDGE")
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
    static CACHE: OnceLock<bool> = OnceLock::new();
    *CACHE.get_or_init(|| match std::env::var("CRATONVM_OSR_NEWARRAY") {
        Ok(v) => v != "0" && !v.eq_ignore_ascii_case("false"),
        Err(_) => true,
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
    static CACHE: OnceLock<bool> = OnceLock::new();
    *CACHE.get_or_init(|| match std::env::var("CRATONVM_DISABLE_INTRINSICS") {
        Ok(v) => !v.is_empty() && v != "0",
        Err(_) => false,
    })
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

#[inline]
pub fn helpful_npe_opcodes() -> bool {
    // Explicit env override wins (interim developer knob), parsed once.
    static ENV: OnceLock<Option<bool>> = OnceLock::new();
    let env = *ENV.get_or_init(|| match std::env::var("CRATONVM_HELPFUL_NPE_OPCODES") {
        Ok(v) => Some(!v.is_empty() && v != "0"),
        Err(_) => None,
    });
    if let Some(b) = env {
        return b;
    }
    // Otherwise honor `-XX:±ShowCodeDetailsInExceptionMessages`. The CLI
    // resolves an absent flag to the on default and calls `set()`, so `== 1`
    // here yields the HotSpot-matching default for app runs; the `-1` sentinel
    // (set() never called) stays off for embedders/tests that don't wire it.
    SHOW_CODE_DETAILS.load(std::sync::atomic::Ordering::Relaxed) == 1
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
    static GATE: OnceLock<bool> = OnceLock::new();
    *GATE.get_or_init(|| match std::env::var("CRATONVM_REAL_PROXY_SUPER") {
        Ok(v) => {
            let v = v.trim().to_ascii_lowercase();
            !(v == "0" || v == "false" || v == "off" || v == "no")
        }
        // Unset (the default): real `java.lang.reflect.Proxy` super.
        Err(_) => true,
    })
}

// Cache the per-native-call GC root snapshot's *frozen* lower frames and
// re-scan only the churning top, keyed by per-frame `(seq, exec_epoch)` + GC
// generation.
// Correctness rests on the LIFO stack discipline (a frame still present at
// index k with unchanged seq proves [0..k) stayed continuously frozen). See
// `update_root_snapshot`. Local writes bump `exec_epoch`, so a frozen-frame
// cache entry is reused only while that frame's root shape is unchanged. The
// real ForkJoinPool lane bypasses both cache reuse and survive-GC remapping.
//
cached_is_set!(real_forkjoinpool, "CRATONVM_REAL_FORKJOINPOOL");

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
    static CACHE: OnceLock<bool> = OnceLock::new();
    *CACHE.get_or_init(|| match std::env::var("CRATONVM_ROOTSNAP_CACHE") {
        // Explicit opt-out only: `0` / `false` disable; unset or any other
        // value (incl. `1`, empty) enables.
        Ok(v) => v != "0" && !v.eq_ignore_ascii_case("false"),
        Err(_) => true,
    })
}

// Opt-in: skip the SECOND, redundant `update_root_snapshot` that
// `native_return_pushed_to_stack` runs after pushing a native's return value
// onto the operand stack. The FIRST snapshot (in `safe_native_call`, gated on
// `native_pending_return.is_some()`) already published the returned object as a
// conservative root; once the value is on the operand stack it is covered by the
// next snapshot refresh (the next native call's `safe_native_call`, or the
// safepoint-poll refresh in `safepoint_check`/`maybe_gc` that every STW
// responder runs BEFORE the collector reads its roots). The post-return thread
// is interruptible, so a moving STW collector never reads this snapshot — it
// waits for the thread to refresh at the barrier. Dropping the rebuild halves
// `update_root_snapshot` frequency on the hot reflective-deploy path (bug 04:
// `update_root_snapshot` is ~68% of an embedded-server deploy). Default-OFF: it
// touches GC root publication; verified with the bt18 checksum oracle (68332206).
// See `native_return_pushed_to_stack`.
//
// DEFAULT-ON as of 2026-06-16 (SpringRepositoriesExtension hang) — halves
// `update_root_snapshot` frequency on every object-returning native call.
// Validated: bt16=14985902, bt18=68332206 (== golden, with vs without).
// Off-switch: `CRATONVM_SKIP_REDUNDANT_NATIVE_SNAPSHOT=0`.
#[inline]
pub fn skip_redundant_native_snapshot() -> bool {
    static CACHE: OnceLock<bool> = OnceLock::new();
    *CACHE.get_or_init(
        || match std::env::var("CRATONVM_SKIP_REDUNDANT_NATIVE_SNAPSHOT") {
            Ok(v) => v != "0" && !v.eq_ignore_ascii_case("false"),
            Err(_) => true,
        },
    )
}

// Opt-in: keep the `rootsnap_cache` frozen-frame cache valid ACROSS a GC by
// remapping its cached roots through the collection's `pointer_map`, instead of
// discarding the whole cache on every `collection_count` bump. The cache holds
// object ADDRESSES; a collection only invalidates them if it RELOCATED the
// object — and even the default non-moving young sweep relocates via selective
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
    static CACHE: OnceLock<bool> = OnceLock::new();
    *CACHE.get_or_init(
        || match std::env::var("CRATONVM_ROOTSNAP_CACHE_SURVIVE_GC") {
            Ok(v) => v != "0" && !v.eq_ignore_ascii_case("false"),
            Err(_) => true,
        },
    )
}

cached_is_set!(jit_dispatch_dbg, "CRATONVM_DBG_JIT_DISPATCH");
// Opt-in: enable small-method inlining in the main JIT tier-up compile path.
// Default-OFF until a `try_emit_inline_body` miscompile (Spring boot enum CCE)
// is root-caused. See `try_jit_upgrade_with_gate`.
cached_is_set!(jit_main_inline, "CRATONVM_JIT_MAIN_INLINE");
// wire-tiered-manager: OFF-THREAD codegen for the invocation tier-up trigger.
// When on, the interpreter's invocation tier-up trigger ENQUEUES a
// `CompilationTask` for the background compile thread (which runs the real
// codegen via `try_jit_compile_callee` and publishes into `shared.jit_cache`)
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
    static CACHE: OnceLock<bool> = OnceLock::new();
    *CACHE.get_or_init(|| match std::env::var("CRATONVM_BG_COMPILE") {
        Ok(v) => v != "0" && !v.eq_ignore_ascii_case("false"),
        Err(_) => true,
    })
}
// wire-tiered-manager Step 4 (PGO handoff C1 → C2): opt-in profile collection.
// When set, `SharedVm::new` calls `jit::profile::enable_profiling(true)` once at
// VM init, so the interpreter's existing branch / receiver / back-edge recording
// sites populate `shared.profile_store` during the interpreted ("C1"/warmup)
// phase. The optimizing C2 compile then consumes that profile — the single-pass
// backend already biases branch layout + pre-populates virtual-call MICs from it,
// and (Step 4) the optimizing IR pipeline now reads branch bias too. Default-OFF:
// every `ProfileStore::record_*` short-circuits on `is_profiling_enabled()`, so
// the interpreter hot loop and all codegen are byte-for-byte unchanged unless
// opted in. See `docs/feature-designs/wire-tiered-manager.md` (Step 4).
cached_is_set!(tier_pgo, "CRATONVM_TIER_PGO");
// Invocation-count tier-up for INSTANCE methods (invokevirtual/invokeinterface).
// DEFAULT-ON as of 2026-06-15 (bug-03 layer B). Previously default-OFF: only
// static methods had an invocation counter (`execute_invokestatic_cached`), so
// short-loop instance hot methods (e.g. java.util.regex `Pattern$*.match`) never
// compiled and ran ~1000x slow. `execute_invokevirtual_cached` now compiles +
// dispatches monomorphic instance call sites via the JIT. The two defects that
// blocked default-on are fixed: (1) the virtual-dispatch BAIL resolved on the
// static call-site class (regex zero-width corruption — `bail_to_interpreter`
// receiver-class fix); (2) the codePointAt precise-ON inline-cascade spill crash.
// Validated: bt16/bt18 golden + regex + WildFly smoke sample B-on == B-off.
// Off-switch for diagnosis/bisection: `CRATONVM_JIT_VIRTUAL_TIERUP=0`.
#[inline]
pub fn jit_virtual_tierup() -> bool {
    static CACHE: OnceLock<bool> = OnceLock::new();
    *CACHE.get_or_init(|| match std::env::var("CRATONVM_JIT_VIRTUAL_TIERUP") {
        // Explicit opt-out only: `0` / `false` disable; unset or any other
        // value (incl. `1`, empty) enables.
        Ok(v) => v != "0" && !v.eq_ignore_ascii_case("false"),
        Err(_) => true,
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
    static CACHE: OnceLock<bool> = OnceLock::new();
    *CACHE.get_or_init(|| match std::env::var("CRATONVM_NATIVE_STRING_REGEX") {
        // Explicit opt-out only: `0` / `false` disable; unset or any other
        // value (incl. `1`, empty) enables.
        Ok(v) => v != "0" && !v.eq_ignore_ascii_case("false"),
        Err(_) => true,
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
    static CACHE: OnceLock<bool> = OnceLock::new();
    *CACHE.get_or_init(|| match std::env::var("CRATONVM_NATIVE_MATCHER_FIND") {
        // Explicit opt-out only: `0` / `false` disable; unset or any other
        // value (incl. `1`, empty) enables.
        Ok(v) => v != "0" && !v.eq_ignore_ascii_case("false"),
        Err(_) => true,
    })
}
cached_is_set!(jit_mic_dbg, "CRATONVM_DBG_JIT_MIC");
cached_is_set!(jit_entry_dbg, "CRATONVM_DBG_JIT_ENTRY");
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

// ── Frame-trace and interpreter hot-path flags ──────────────────────────

cached_is_set!(frame_trace, "CRATONVM_FRAME_TRACE");
cached_is_set!(iae_trace_os, "CRATONVM_IAE_TRACE");
cached_is_set!(bd_debug, "CRATONVM_BD_DEBUG");
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
/// UNCACHED `std::env::var_os(...).is_some()` inside `execute_invokevirtual_vtable_fast`
/// (i.e. a `GetEnvironmentVariableW` syscall on the virtual-call dispatch path).
/// Cached.
cached_is_set!(dbg_jetty2, "CRATONVM_DBG_JETTY2");
// `CRATONVM_DBG_VDISP` -- virtual-dispatch diagnostic. This sits on the
// invokevirtual/invokeinterface miss path and must not call into the process
// environment on every dispatch.
cached_is_set!(dbg_vdisp, "CRATONVM_DBG_VDISP");
cached_is_set!(dbg_ccsprobe, "CRATONVM_DBG_CCSPROBE");
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
/// `CRATONVM_DBG_BYTECODE_DUMP` -- temporary raw-bytecode + mnemonic
/// disassembly dump (2026-07-15, JRubyScriptTemplateTests round 3): see
/// `push_frame_and_fire_entry`'s own doc comment for the full story --
/// dumps the runtime-generated bytecode for any frame whose class name
/// contains "version" (JRuby's URI-mangled class name for
/// `rubygems/version.rb`), since these snippets are synthesized at
/// runtime and have no static `.class` file `javap` can decompile.
cached_is_set!(dbg_bytecode_dump, "CRATONVM_DBG_BYTECODE_DUMP");

// ── Flags read via `env::var(...).is_ok()` ──────────────────────────────

/// `CRATONVM_NO_LOCAL_LIVENESS` — disable the per-bci local-variable
/// liveness filter in the interpreter frame GC root scan (restores the
/// scan-every-object-typed-slot behaviour; see `runtime::local_liveness`).
cached_is_ok!(no_local_liveness, "CRATONVM_NO_LOCAL_LIVENESS");
cached_is_ok!(trace_sb_filter, "CRATONVM_TRACE_SB_FILTER");
cached_is_ok!(nsee_trace, "CRATONVM_NSEE_TRACE");
cached_is_ok!(iae_trace, "CRATONVM_IAE_TRACE");
cached_is_ok!(athrow_dbg, "CRATONVM_DBG_ATHROW");
cached_is_ok!(npe_invoke_dbg, "CRATONVM_DBG_NPE_INVOKE");
/// `CRATONVM_DBG_MODSTATIC` — JBoss-Modules `<clinit>`/static-dispatch
/// diagnostic. This was read with an UNCACHED `std::env::var(...).is_ok()`
/// from `ensure_class_initialized_shared` (the per-barrier class-init gate
/// fired on every getstatic/putstatic/new/invokestatic, *before* the
/// fast-path "already initialized" check) and from two method-resolution
/// sites — i.e. a ~500 ns `GetEnvironmentVariableW` syscall on a large
/// fraction of all executed bytecodes. That single uncached lookup
/// dominated steady-state interpreter throughput (a getstatic-only loop
/// measured ~567 ns/iter, most of it this call). Cached like its siblings.
cached_is_ok!(modstatic_dbg, "CRATONVM_DBG_MODSTATIC");
/// `CRATON_HASHTABLEOFINT_TRACE` — niche getfield/putfield diagnostic that was
/// read with an UNCACHED `std::env::var(...).is_ok()` on EVERY getfield and
/// putfield (the two most common opcodes in object-oriented bytecode) — a
/// per-field-access `GetEnvironmentVariableW` syscall. Cached.
cached_is_ok!(hashtableofint_trace, "CRATON_HASHTABLEOFINT_TRACE");
cached_is_ok!(dbg_toarray, "CRATONVM_DBG_TOARRAY");
/// `CRATON_BAOS_DBG` — ByteArrayOutputStream `buf`/`count` putfield diagnostic,
/// read with an UNCACHED `std::env::var_os(...).is_some()` on EVERY putfield.
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
    static CACHE: OnceLock<bool> = OnceLock::new();
    *CACHE.get_or_init(|| {
        field_addr_dbg()
            || badrecv_dbg()
            || hashtableofint_trace()
            || bd_debug()
            || baos_dbg()
            || std::env::var_os("CRATONVM_DBG_STRAYSTACK").is_some()
            || std::env::var_os("CRATONVM_DBG_NULLTHIS").is_some()
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
    static CACHE: OnceLock<bool> = OnceLock::new();
    *CACHE.get_or_init(|| match std::env::var("CRATONVM_STRICT_SWALLOWS") {
        Ok(v) => v == "1",
        Err(_) => false,
    })
}

#[inline]
pub fn jit_scalar_new() -> bool {
    static CACHE: OnceLock<bool> = OnceLock::new();
    *CACHE.get_or_init(|| std::env::var("CRATONVM_JIT_SCALAR_NEW").map_or(true, |v| v != "0"))
}

/// C1→C2 supersede (default-ON): after the background worker publishes a C1
/// body for a call-free/allocation-free IR-eligible method, it enqueues a
/// Low-priority C2 recompile whose publish replaces the C1 body and bumps
/// the supersede epoch (per-thread invoke caches re-resolve on their next
/// hit). `=0`/`false` keeps every method at its first-published tier (the
/// pre-supersede behaviour) — the safety net while the upgrade soaks.
#[inline]
pub fn c2_supersede() -> bool {
    static CACHE: OnceLock<bool> = OnceLock::new();
    *CACHE.get_or_init(|| {
        std::env::var("CRATONVM_C2_SUPERSEDE").map_or(true, |v| v != "0" && v != "false")
    })
}

#[inline]
pub fn jit_ir_call() -> bool {
    static CACHE: OnceLock<bool> = OnceLock::new();
    *CACHE.get_or_init(|| std::env::var("CRATONVM_JIT_IR_CALL").map_or(true, |v| v != "0"))
}

#[inline]
pub fn jit_ir_call_special() -> bool {
    static CACHE: OnceLock<bool> = OnceLock::new();
    *CACHE.get_or_init(|| std::env::var("CRATONVM_JIT_IR_CALL_SPECIAL").map_or(true, |v| v != "0"))
}

#[inline]
pub fn jit_ir_long() -> bool {
    static CACHE: OnceLock<bool> = OnceLock::new();
    *CACHE.get_or_init(|| std::env::var("CRATONVM_JIT_IR_LONG").map_or(true, |v| v != "0"))
}

#[inline]
pub fn jit_ir_call_virtual() -> bool {
    static CACHE: OnceLock<bool> = OnceLock::new();
    *CACHE.get_or_init(|| std::env::var_os("CRATONVM_JIT_IR_CALL_VIRTUAL").is_some())
}

#[inline]
pub fn jit_ir_fp() -> bool {
    static CACHE: OnceLock<bool> = OnceLock::new();
    *CACHE.get_or_init(|| std::env::var("CRATONVM_JIT_IR_FP").map_or(true, |v| v != "0"))
}

#[inline]
pub fn inline_allow_static() -> bool {
    static CACHE: OnceLock<bool> = OnceLock::new();
    *CACHE.get_or_init(|| std::env::var_os("CRATONVM_INLINE_ALLOW_STATIC").is_some())
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

/// Once-initialized accessor for the `CRATONVM_REAL` / `CRATONVM_REAL_JCA`
/// differential switch. Parses the env vars exactly once; every subsequent
/// call serves the cached [`RealSelector`].
#[inline]
pub fn real_bytecode_selector() -> &'static RealSelector {
    static CACHE: OnceLock<RealSelector> = OnceLock::new();
    CACHE.get_or_init(|| {
        let real = std::env::var("CRATONVM_REAL").ok();
        let jca_legacy = std::env::var_os("CRATONVM_REAL_JCA")
            .map(|v| !v.is_empty())
            .unwrap_or(false);
        RealSelector::parse(real.as_deref(), jca_legacy)
    })
}

#[cfg(test)]
mod tests {
    use super::parse_osr_backedge_enabled;

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
}
