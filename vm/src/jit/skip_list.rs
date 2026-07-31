// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT compilation skip list — static eligibility predicate.
//!
//! This module is the **single source of truth** for the static decision
//! "should the JIT compiler attempt to compile this method?". It is the
//! complement of [`crate::vm::SharedVm::jit_skip_set`], which is a *runtime*
//! deopt blacklist populated as the JIT discovers it cannot handle a specific
//! method (e.g. unresolved string ldc, deopt request, hash collision).
//!
//! Every entry below corresponds to a known correctness limitation in the
//! JIT/GC subsystem and is tied to a specific roadmap item. The goal is to
//! shrink this list to **empty** as the underlying gaps in Phase A1 of
//! `docs/roadmap.md` are closed:
//!
//! | Skip rule                       | Status                  |
//! |---------------------------------|-------------------------|
//! | `<clinit>`                      | targeted ban — A1.4 narrowing pending |
//! | `<init>`                        | targeted ban — A1.4 narrowing pending |
//! | interface default methods       | targeted ban — A1.4 (regalloc param mapping) |
//! | `java/util/*`                   | conservative-only — overridable via `CRATONVM_JIT_ALLOW_PACKAGES` |
//! | `cratonvm/*` (legacy fixtures)   | conservative-only — overridable via `CRATONVM_JIT_ALLOW_PACKAGES` |
//! | `java/lang/*`                   | **REMOVED** (NEW-1.2 fixed JIT instanceof) |
//! | `cratonvm/Tck*`                  | **REMOVED** (NEW-1.2 fixed JIT instanceof) |
//! | `cratonvm/*FinalizerTest*`       | **REMOVED** (NEW-1.5 conservative JIT root scan) |
//! | unnamed-thread methods          | targeted — thread-local JIT state pre-init guard |
//!
//! ## NEW-1 progress (2026-04-14)
//!
//! - **NEW-1.2 (instanceof / checkcast)** — closed. The JIT helpers
//!   [`crate::jit::helpers::jit_instanceof`] / [`crate::jit::helpers::jit_checkcast`]
//!   now load the target class on demand and walk the lambda-proxy and
//!   synthetic-implements fallbacks, exactly like the interpreter. Result:
//!   the `java/lang/*` and `cratonvm/Tck*` blanket bans are gone — they were
//!   masking this bug, not protecting against an unrelated one.
//! - **NEW-1.5 (conservative JIT root scan)** — closed. The GC root walker now
//!   scans every active JIT spill area and pins any qword that lies inside the
//!   heap arena. This is the production-safe stop-gap for the absence of
//!   precise JIT oop maps; it removes the `FinalizerTest` ban without risking
//!   collected pointers in JIT frames. Precise oop maps remain a future item.
//! - **NEW-1.3 / NEW-1.4** — `<init>`, `<clinit>`, interface defaults, and the
//!   `java/util/*` / `cratonvm/*` package bans remain because they correspond to
//!   distinct, not-yet-fixed JIT correctness gaps (regalloc parameter mapping
//!   for interface defaults; `<clinit>` re-entrancy during constant-pool
//!   resolution; hash-table loop regalloc miscompile). They can be lifted
//!   per-package via the `CRATONVM_JIT_ALLOW_PACKAGES` environment variable for
//!   development / benchmarking.
//!
//! When [`SkipPolicy::Aggressive`] is selected, the package bans
//! (`java/util/`, `cratonvm/`) are lifted. This is intended for development to
//! surface latent JIT bugs and for benchmarking the maximal reachable code
//! path. Production builds default to [`SkipPolicy::Conservative`].

/// JIT eligibility policy.
///
/// Selects whether the broad correctness-driven blanket bans are applied
/// (`Conservative`) or only the targeted per-method bans are applied
/// (`Aggressive`). Mapped from `VmConfig::jit_aggressive_compilation`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SkipPolicy {
    /// Apply all blanket bans (`java/util/`, `java/lang/`, `cratonvm/Tck`,
    /// `cratonvm/`). Default for production.
    #[default]
    Conservative,
    /// Lift the blanket package bans; only keep targeted bans that correspond
    /// to specific reproducible JIT crashes (`<clinit>`, `<init>`,
    /// interface defaults, `FinalizerTest`, unnamed thread).
    Aggressive,
}

/// Reason a method was rejected by the static skip list. Surfaced for
/// diagnostics; the JIT path consumes only `is_some()`.
///
/// Variants marked `// REMOVED` are kept in the enum so external diagnostics
/// (JFR events, logs from earlier versions) keep parsing, but they are never
/// produced by [`should_skip_jit`] anymore.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// `<clinit>` re-entrancy during constant-pool resolution. (A1.4)
    ClassInitializer,
    /// `<init>` constructor — interaction with field initialization order. (A1.4)
    Constructor,
    /// Interface default method — known regalloc parameter-mapping bug. (A1.4)
    InterfaceDefault,
    /// `java/util/*` — hash table loop miscompiles. (A1.4)
    JavaUtilCollection,
    /// `java/lang/*` — `instanceof` codegen + control flow. (A1.2, A1.4)
    /// REMOVED in NEW-1.2 — never produced. Kept for ABI/parser compatibility.
    JavaLangCore,
    /// TCK class — exercises the `instanceof` JIT bug. (A1.2)
    /// REMOVED in NEW-1.2 — never produced. Kept for ABI/parser compatibility.
    TckClass,
    /// `cratonvm/*` test fixture — broad ban for legacy reasons. (A1.4)
    RustJvmTestFixture,
    /// Finalizer-bearing class — JIT frames lacked GC stack maps. (A1.1)
    /// REMOVED in NEW-1.5 (conservative JIT root scan replaces precise maps).
    /// Never produced. Kept for ABI/parser compatibility.
    FinalizerTest,
    /// Method is being invoked from an unnamed thread (typically a test
    /// harness in early init), where thread-local JIT state may not be set up.
    UnnamedThread,
    /// A generated `java.lang.reflect.Proxy` subclass method. Its body is a
    /// pure `super.h.invoke(this, mN, args)` trampoline whose semantics
    /// CratonVM overrides at DISPATCH, not in the bytecode -- see
    /// `is_jdk_dynamic_proxy_class`.
    JdkDynamicProxyTrampoline,
    /// `MutableBigInteger` divide/normalization arithmetic has a confirmed
    /// JIT-only array-index corruption residual. Keep the implementation
    /// interpreted until the lowering defect is identified.
    BigIntegerArithmetic,
    /// Spring Boot's `ModifiedClassPathClassLoader.loadClass` can spin in its
    /// nested class-path exclusion path once tier-compiled. Keep this one
    /// test-support loader method interpreted until its JIT lowering is
    /// understood.
    SpringBootModifiedClassPathLoader,
    /// `java/util/stream/MatchOps.makeInt/makeRef/makeLong/makeDouble`
    /// unconditionally reach an internal `invokedynamic` (lambda) call site
    /// that `jit_scan` lowers to an always-deopt uncommon trap
    /// (`DeoptReason::UnreachedCode`) on every single invocation, not just a
    /// mispredicted rare path. Once JIT-compiled these factories provide zero
    /// benefit (100% of calls deopt) while still paying compile and
    /// reconstruct-and-resume costs. Keep them interpreted outright rather
    /// than relying on the runtime give-up path, which does not reliably stop
    /// re-invocation once a caller's own inline cache has already cached the
    /// raw entry point (ES-PERF-20260719 testSlicesDense: 6928 deopt events
    /// for `makeInt` alone in a single test run).
    StreamMatchOpsUncommonTrap,
}

/// T1.1.f — classification of `<init>` / `<clinit>` complexity.
///
/// The JIT historically banned every constructor and class initializer
/// because of two distinct interactions:
///   * `<init>` field-initialization-order coupling with the JIT's
///     load-forwarding pass.
///   * `<clinit>` re-entrancy during constant-pool resolution.
///
/// Both interactions only fire when the method *does* the offending
/// thing. Trivial constructors that just chain to `super.<init>` (the
/// 5-byte `aload_0; invokespecial; return` sequence javac emits when
/// no field initializers exist) are completely safe to JIT.
///
/// `classify_init_complexity` walks a method's bytecode and returns
/// `Trivial` if no field stores, no synchronization, and no
/// invokedynamic occur — the three known crash conditions. Callers
/// pass the result through `should_skip_jit` so trivial inits become
/// JIT-eligible automatically.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InitComplexity {
    /// Caller did not classify; legacy callers default here. Treated
    /// the same as `Complex` for backward compatibility.
    #[default]
    Unknown,
    /// No field stores, no monitorenter, no invokedynamic — safe to JIT.
    Trivial,
    /// At least one field store, monitorenter, or invokedynamic — keep
    /// the historical ban.
    Complex,
}

/// Walk a method's raw bytecode and decide whether it qualifies as
/// `InitComplexity::Trivial`. Conservative on parse failures: if the
/// instruction stream cannot be cleanly walked, returns `Complex`.
///
/// This is intentionally a *very fast* scan — it only looks at opcode
/// bytes and operand sizes, not semantics. The opcodes we treat as
/// disqualifying are:
///
/// * `putfield` (0xb5), `putstatic` (0xb3) — field stores trigger the
///   JIT's load-forwarding interaction.
/// * `monitorenter` (0xc2), `monitorexit` (0xc3) — synchronization
///   nests through the JIT helper boundary.
/// * `invokedynamic` (0xba) — bootstrap method resolution can re-enter
///   the class initializer.
///
/// Every other opcode is benign. The scan stops at the first
/// disqualifier and returns `Complex`. If it walks the entire stream
/// without finding one, it returns `Trivial`.
/// **DEFAULT ON since 2026-07-28.** A constructor whose ONLY disqualifier is
/// `putfield` classifies as `Trivial` and is JIT-eligible; the ban is retained
/// for `putstatic` / `monitorenter` / `monitorexit` / `invokedynamic`.
///
/// **Kill switch: `CRATONVM_JIT_PUTFIELD_INIT=0`** (also `off` / `false` /
/// `no`) restores the historical blanket ban with no rebuild. If a regression
/// run turns up a miscompile, a wrong result, or a crash, set that and re-run
/// before doing anything else — it is the fastest possible attribution test for
/// this change, and it cleanly separates "constructor compilation broke it"
/// from everything else in the same build.
///
/// Why the ban was lifted. `putfield` is what essentially every *real*
/// constructor does — it is the whole point of one — so the blanket gate made
/// "allocate an object whose constructor assigns a field" run its constructor
/// in the interpreter forever, and unlike the RBC.6 gate such a constructor was
/// never even *enqueued* for compilation (no `tiered-enqueue` line at all).
/// That is a VM-wide ceiling on `new`: `new String(...)`,
/// `new HashMap.Node(...)`, essentially the whole JDK. Measured on the
/// `CallRate.allocBody` probe from C2-compiled code: **1846 ns/alloc banned vs
/// 226 ns/alloc allowed — 8.2x** — with the empty-constructor controls
/// (`allocArg` / `allocBare` / `allocNoCtor`) flat, so the change does only
/// what it claims.
///
/// Evidence gathered before flipping: the `CtorCheck` probe — which READS BACK
/// every field, so an elided or miscompiled constructor cannot hide behind a
/// good-looking timing — produces byte-identical checksums across HotSpot,
/// CratonVM `--nojit`, ban-on and ban-off at 200k and 2M iterations, covering
/// plain stores, a superclass ctor storing before a subclass ctor, a store fed
/// by an instance-method call on the half-built object, and a conditional
/// store. 26 tests across six JIT test binaries pass with it lifted, and a
/// six-class Tomcat sweep showed no attributable regression (every non-PASS in
/// that sweep reproduces identically with the ban ON).
///
/// What that evidence does NOT cover, stated plainly: this ban predates the
/// open-source import (`a6dc911ed`) and is one of the four *structural* bans;
/// unlike the ~46 named correctness bans it carries no incident write-up, only
/// the "field stores trigger the JIT's load-forwarding interaction" note above.
/// None of the above proves that rationale stale — it shows only that the tests
/// run so far do not catch it. Flipped by explicit maintainer decision with a
/// full regression run to follow; that run is the real verdict, which is why
/// the kill switch exists.
fn allow_putfield_init() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(
        || match cratonvm_types::flags::runtime_var("CRATONVM_JIT_PUTFIELD_INIT") {
            Ok(v) => !matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "0" | "off" | "false" | "no"
            ),
            // Default: a constructor that only stores fields is compilable.
            Err(_) => true,
        },
    )
}

pub fn classify_init_complexity(bytecode: &[u8]) -> InitComplexity {
    classify_init_complexity_with(bytecode, allow_putfield_init())
}

/// [`classify_init_complexity`] with the `putfield` gate supplied explicitly.
///
/// [`allow_putfield_init`] latches its answer in a `OnceLock`, so a test in
/// this process cannot exercise both sides of the `CRATONVM_JIT_PUTFIELD_INIT`
/// kill switch through the public entry point — and that switch is the
/// documented escape hatch for a structural ban lifted on measurement rather
/// than on an incident write-up, so it is precisely the path that wants
/// coverage. Splitting the pure classifier out lets the tests pin both sides.
fn classify_init_complexity_with(bytecode: &[u8], allow_putfield: bool) -> InitComplexity {
    use std::cmp::min;
    let mut pc = 0usize;
    while pc < bytecode.len() {
        let op = bytecode[pc];
        match op {
            // `putfield` alone is separable from the other four: it is benign
            // by default since 2026-07-28 — see `allow_putfield_init`, which
            // also documents the `CRATONVM_JIT_PUTFIELD_INIT=0` kill switch.
            // With that set this arm is unreachable and the classification is
            // byte-identical to the historical behaviour.
            0xb5 if allow_putfield => {}
            0xb3 | 0xb5 | 0xc2 | 0xc3 | 0xba => return InitComplexity::Complex,
            _ => {}
        }
        // Advance pc by the instruction length. We use a small built-in
        // length table for the common variable-length cases; unknown
        // opcodes default to length 1 (treat as conservative).
        let len = match op {
            // 0..=15 are 1-byte
            0x00..=0x0f => 1,
            0x10 => 2,        // bipush
            0x11 => 3,        // sipush
            0x12 => 2,        // ldc
            0x13 | 0x14 => 3, // ldc_w / ldc2_w
            0x15..=0x19 => 2, // iload..aload
            0x1a..=0x35 => 1, // iload_n..saload
            0x36..=0x3a => 2, // istore..astore
            0x3b..=0x56 => 1, // istore_n..sastore
            0x57..=0x5f => 1, // pop..swap
            0x60..=0x83 => 1, // arithmetic
            0x84 => 3,        // iinc
            0x85..=0x93 => 1, // conversions
            0x94..=0x98 => 1, // lcmp/fcmpl/fcmpg/dcmpl/dcmpg
            0x99..=0xa6 => 3, // ifeq..if_acmpne
            0xa7 => 3,        // goto
            0xa8 => 3,        // jsr
            0xa9 => 2,        // ret
            0xaa => {
                // tableswitch — pad to 4-byte boundary then 12 bytes
                // header + (high - low + 1) * 4 jumps.
                let pad = (4 - ((pc + 1) % 4)) % 4;
                let table = pc + 1 + pad;
                if table + 12 > bytecode.len() {
                    return InitComplexity::Complex;
                }
                let low = i32::from_be_bytes([
                    bytecode[table + 4],
                    bytecode[table + 5],
                    bytecode[table + 6],
                    bytecode[table + 7],
                ]);
                let high = i32::from_be_bytes([
                    bytecode[table + 8],
                    bytecode[table + 9],
                    bytecode[table + 10],
                    bytecode[table + 11],
                ]);
                let n = (high as i64 - low as i64 + 1).max(0) as usize;
                1 + pad + 12 + n * 4
            }
            0xab => {
                // lookupswitch — pad to 4-byte boundary then 8 bytes
                // header + npairs * 8 entries.
                let pad = (4 - ((pc + 1) % 4)) % 4;
                let table = pc + 1 + pad;
                if table + 8 > bytecode.len() {
                    return InitComplexity::Complex;
                }
                let npairs = u32::from_be_bytes([
                    bytecode[table + 4],
                    bytecode[table + 5],
                    bytecode[table + 6],
                    bytecode[table + 7],
                ]) as usize;
                1 + pad + 8 + npairs * 8
            }
            0xac..=0xb1 => 1, // ireturn..return
            0xb2..=0xb6 => 3, // getstatic, putstatic, getfield, putfield, invokevirtual
            0xb7 | 0xb8 => 3, // invokespecial, invokestatic
            0xb9 => 5,        // invokeinterface
            // 0xba (invokedynamic) handled above
            0xbb => 3,        // new
            0xbc => 2,        // newarray
            0xbd => 3,        // anewarray
            0xbe..=0xbf => 1, // arraylength, athrow
            0xc0..=0xc1 => 3, // checkcast, instanceof
            // 0xc2/0xc3 monitorenter/monitorexit handled above
            0xc4 => {
                // wide prefix: next opcode is widened
                if pc + 1 >= bytecode.len() {
                    return InitComplexity::Complex;
                }
                let inner = bytecode[pc + 1];
                if inner == 0x84 {
                    // wide iinc: 6 bytes total (c4 84 idx2 const2)
                    6
                } else {
                    // wide iload..astore: 4 bytes total (c4 op idx2)
                    4
                }
            }
            0xc5 => 4,        // multianewarray
            0xc6 | 0xc7 => 3, // ifnull, ifnonnull
            0xc8 | 0xc9 => 5, // goto_w, jsr_w
            _ => 1,
        };
        pc += min(len, bytecode.len() - pc);
        if len == 0 {
            return InitComplexity::Complex;
        }
    }
    InitComplexity::Trivial
}

/// Decide whether to skip JIT compilation for a single method.
///
/// Returns `Some(reason)` if the method must be interpreted, `None` if the
/// JIT may attempt to compile it. Pure function, easily unit-tested. Does
/// **not** consult the runtime deopt blacklist (`SharedVm::jit_skip_set`);
/// callers must check that separately for the orthogonal "this method already
/// failed at runtime" case.
///
/// The `allow_packages` slice carries per-package overrides parsed from the
/// `CRATONVM_JIT_ALLOW_PACKAGES` environment variable (comma-separated package
/// prefixes such as `java/util,cratonvm/`). Any conservative-only blanket ban
/// whose prefix appears in this list is suppressed. Callers that don't need
/// the override should pass `&[]`.
///
/// `init_complexity` (T1.1.f) classifies `<init>`/`<clinit>` methods by
/// whether they perform any of the three known crash-triggering
/// operations. `InitComplexity::Trivial` lifts the constructor /
/// class-initializer bans for that one method.
pub fn should_skip_jit_with_init(
    class_name: &str,
    method_name: &str,
    is_interface_default: bool,
    current_thread_named: bool,
    policy: SkipPolicy,
    allow_packages: &[&str],
    init_complexity: InitComplexity,
) -> Option<SkipReason> {
    if method_name == "<clinit>" {
        if init_complexity != InitComplexity::Trivial {
            return Some(SkipReason::ClassInitializer);
        }
    } else if method_name == "<init>" {
        if init_complexity != InitComplexity::Trivial {
            return Some(SkipReason::Constructor);
        }
    }
    should_skip_jit_internal(
        class_name,
        method_name,
        is_interface_default,
        current_thread_named,
        policy,
        allow_packages,
        /* skip_init_check */ true,
    )
}

pub fn should_skip_jit(
    class_name: &str,
    method_name: &str,
    is_interface_default: bool,
    current_thread_named: bool,
    policy: SkipPolicy,
    allow_packages: &[&str],
) -> Option<SkipReason> {
    should_skip_jit_internal(
        class_name,
        method_name,
        is_interface_default,
        current_thread_named,
        policy,
        allow_packages,
        false,
    )
}

fn should_skip_jit_internal(
    class_name: &str,
    method_name: &str,
    is_interface_default: bool,
    current_thread_named: bool,
    policy: SkipPolicy,
    allow_packages: &[&str],
    skip_init_check: bool,
) -> Option<SkipReason> {
    // ES812 postings JIT residual (2026-07-13): indexedBinarySearch can
    // invoke a lambda apply method through the wrong receiver after an
    // aggressive java/util promotion. Interpret this dispatcher until the
    // invokeinterface PIC invalidation handles changing lambda receivers.
    if class_name == "java/util/Collections" && method_name == "indexedBinarySearch" {
        return Some(SkipReason::JavaUtilCollection);
    }

    // ES-PERF-20260719 (2026-07-21): see StreamMatchOpsUncommonTrap doc comment.
    // Confirmed via CRATONVM_DBG_DEOPT against the real testSlicesDense
    // workload: `makeInt` alone produced 6928 `reason=UnreachedCode
    // action=MakeNotCompilable` deopt events in a single ~700s window, none
    // of which stopped further re-invocation of the doomed compiled entry.
    // `makeRef`/`makeLong`/`makeDouble` share the exact same factory-method
    // shape (a MatchKind switch building a sink via an internal lambda) and
    // are included defensively even though only `makeInt` was observed
    // faulting in this workload.
    if class_name == "java/util/stream/MatchOps"
        && matches!(
            method_name,
            "makeInt" | "makeRef" | "makeLong" | "makeDouble"
        )
    {
        return Some(SkipReason::StreamMatchOpsUncommonTrap);
    }


    // SPRING-TESTCOMPILER.1-4 / HIB-STOREDPROC-JIT.1 / TYPES-ERASURE.1 /
    // SPRINGBOOT-CONDITION-REPORT.1 / SPRINGBOOT-HTTP-HEADER-COMPARATOR.1 /
    // SPRINGBOOT-ANNOTATED-METADATA-COLLECTOR.1 -- ALL REMOVED 2026-07-30.
    //
    // Eleven per-method bans lived here: the eight in-process-javac-family
    // ones (`com/sun/tools/javac/api/JavacTool.getTask`,
    // `com/sun/tools/javac/jvm/ClassReader.readClass`/`.readInnerClasses`/
    // `.readAttrs`, `com/sun/tools/javac/code/ClassFinder.fillIn`,
    // `com/sun/tools/javac/code/Symbol$ClassSymbol.complete`,
    // `com/sun/tools/javac/code/Types.erasure`,
    // `org/springframework/javapoet/CodeBlock$Builder.add`) plus the three
    // 2026-07-29 Spring Boot lambda/collector ones
    // (`ConditionEvaluationReport.lambda$recordConditionEvaluation$0`,
    // `JdkClientHttpRequest.lambda$buildRequest$0`,
    // `AnnotatedTypeMetadata.getAllAnnotationAttributes`).
    //
    // All eleven were re-tested on this dev tip with a build-time kill switch
    // that lifted them individually, and none of the miscompiles they
    // document still reproduces -- the underlying JIT defects were fixed by
    // dev drift between 2026-07-25 and 2026-07-30. The differential is
    // recorded in
    // `docs/internal/spring-jit-bans-inventory-and-ban-lift-experiment-20260730.md`;
    // the short version:
    //
    //   * `JavacConsolidationProbe` (200 varied in-process javac compilations,
    //     the repo's own witness for the javac family) fails at iteration 2 on
    //     dev `351bf59b0` with the bans lifted and passes 200/200 on this tip
    //     with them lifted;
    //   * ten Spring AOT/codegen test classes produce byte-identical results
    //     with the bans active and lifted on this tip;
    //   * `BasicErrorControllerIntegrationTests` (the Spring Boot trio's own
    //     witness) reproduces its failure on `351bf59b0` with those bans
    //     lifted and is clean across repeated runs here.
    //
    // The unit tests below are the regression witnesses: they now assert
    // JIT-ELIGIBILITY for all eleven methods, so re-adding a ban silently is
    // a test failure. If a javac-family miscompile ever comes back, prefer
    // fixing the lowering over re-adding a per-method ban -- this family has
    // regressed twice from unrelated x64 backend changes.

    // SPRINGBOOT-WITHOUT-JACKSON.2 -- REMOVED 2026-07-26. Re-verified with a
    // standalone probe (`SpringBootLoadClassProbe.java`, package-local to
    // `org.springframework.boot.testsupport.classpath` since the real
    // `ModifiedClassPathClassLoader` constructor is package-private) driving
    // the real `spring-boot-test-support` 7.0.7 class's `loadClass(String)`
    // directly, 20000 calls across both the junit/hamcrest-delegation branch
    // and the package-exclusion/`super.loadClass()` branch, plus a
    // `CRATONVM_JIT_THRESHOLD=1` aggressive-compilation pass (5000 calls): 0
    // failures in every configuration. The original 480s watchdog hang was
    // most likely in the surrounding config-property cache machinery, not in
    // this method itself. `SpringBootLoadClassProbe.java` is the regression
    // witness.

    // Bisection hook (development only): `CRATONVM_JIT_BISECT_SKIP` is a
    // comma-separated list of `Class.method` entries (slash-separated
    // class names, e.g. `java/util/Locale.hashCode`). Any listed method
    // is forced to skip the JIT. Used to binary-search a miscompiling
    // method without recompiling.
    {
        use std::sync::OnceLock;
        static BISECT: OnceLock<Vec<(String, String)>> = OnceLock::new();
        let list = BISECT.get_or_init(|| {
            cratonvm_types::flags::runtime_var("CRATONVM_JIT_BISECT_SKIP")
                .ok()
                .map(|s| {
                    s.split(',')
                        .filter_map(|e| {
                            let e = e.trim();
                            e.rfind('.')
                                .map(|i| (e[..i].to_string(), e[i + 1..].to_string()))
                        })
                        .collect()
                })
                .unwrap_or_default()
        });
        if list
            .iter()
            .any(|(c, m)| c == class_name && m == method_name)
        {
            return Some(SkipReason::RustJvmTestFixture);
        }
    }

    // Inverse bisection hook: `CRATONVM_JIT_BISECT_ONLY` is a comma-
    // separated list of class-name prefixes. When set, ANY method whose
    // class does not start with one of the prefixes is forced to skip
    // the JIT — i.e. only the listed packages stay JIT-eligible. Used to
    // binary-search which package contains a miscompiling method.
    {
        use std::sync::OnceLock;
        static ONLY: OnceLock<Option<Vec<String>>> = OnceLock::new();
        let only = ONLY.get_or_init(|| {
            cratonvm_types::flags::runtime_var("CRATONVM_JIT_BISECT_ONLY").ok().map(|s| {
                s.split(',')
                    .map(|e| e.trim().to_string())
                    .filter(|e| !e.is_empty())
                    .collect()
            })
        });
        if let Some(prefixes) = only {
            if !prefixes.iter().any(|p| class_name.starts_with(p.as_str())) {
                return Some(SkipReason::RustJvmTestFixture);
            }
        }
    }

    // Targeted bans — these correspond to *reproducible* JIT crashes and
    // apply under both policies. Removed bans (JavaLangCore, TckClass,
    // FinalizerTest) are intentionally not checked here — see module docs.
    if !skip_init_check {
        if method_name == "<clinit>" {
            return Some(SkipReason::ClassInitializer);
        }
        if method_name == "<init>" {
            return Some(SkipReason::Constructor);
        }
    }
    if is_interface_default {
        return Some(SkipReason::InterfaceDefault);
    }
    if !current_thread_named {
        return Some(SkipReason::UnnamedThread);
    }
    // SPR-PROXY.1 -- see `is_jdk_dynamic_proxy_class`. Deliberately NOT
    // liftable by `CRATONVM_JIT_ALLOW_PACKAGES`: a compiled proxy body is a
    // dispatch path the VM's proxy semantics do not cover, and there is no
    // throughput to win back from a trampoline.
    if is_jdk_dynamic_proxy_class(class_name) {
        return Some(SkipReason::JdkDynamicProxyTrampoline);
    }

    // HIB-BIGINTEGER-AIOOBE.1 (2026-07-17) — the real-JDK
    // `MutableBigInteger` divide/normalization implementation is the only
    // confirmed JIT-only surface behind Hibernate's intermittent
    // `BigInteger.smallToString` AIOOBE ("Index 2 out of bounds for length
    // 2").  The interpreter and `--nojit` execute the exact same bytecode
    // correctly, while multiple live failures have shown a self-consistent
    // bounds check against a two-element array after this class was JITed.
    //
    // This deliberately covers the whole implementation class rather than a
    // guessed leaf such as `mulsub`: the reported frame is several calls
    // above the corrupting write and the historical reproducer is bimodal.
    // A class-local, unconditional fail-closed guard preserves JIT coverage
    // for `BigInteger` callers and all application code, and cannot be lifted
    // by `CRATONVM_JIT_ALLOW_PACKAGES` until the underlying x64 lowering bug
    // has a deterministic regression reproducer.
    if class_name == "java/math/MutableBigInteger" {
        return Some(SkipReason::BigIntegerArithmetic);
    }

    // HIB-BIGINTEGER-AIOOBE.2 (2026-07-26) -- while hunting for the
    // deterministic reproducer HIB-BIGINTEGER-AIOOBE.1 above says never
    // existed, found one for a DIFFERENT symptom in `BigInteger` ITSELF
    // (not just its `MutableBigInteger` helper): `new BigInteger(String)`
    // intermittently throws `NullPointerException: Cannot read the array
    // length because "this.mag" is null` from inside
    // `BigInteger(String, int)` -- i.e. the field write to `this.mag` at
    // the end of construction does not become visible before the very
    // next read of it. Minimal repro (`BigIntegerToStringProbe.java`, not
    // committed, matches the bench/ probe convention): construct a random
    // BigInteger, call `toString()`, then round-trip via
    // `new BigInteger(s)` -- fails at ITERATION 0, no warmup, no explicit
    // divide() needed. Confirmed JIT-only via `--nojit` (clean through
    // 40k+ iterations, vs. instant failure under JIT). This crashes even
    // WITH the HIB-BIGINTEGER-AIOOBE.1 guard above already in place --
    // that guard's scope (`MutableBigInteger` only) does not cover it.
    // Bisected with `CRATONVM_JIT_BISECT_SKIP` (no rebuild): forcing any
    // ONE of the constructor itself, `trustedStripLeadingZeroInts`,
    // `destructiveMulAdd`, `checkRange`, or the internal char-array
    // `parseInt` helper alone to interpret does NOT fix it (each ruled
    // out individually) -- but denying the whole `java/math/BigInteger`
    // class via `CRATONVM_JIT_DENY` does. Not yet narrowed past the
    // whole-class level (a multi-method interaction, same shape as the
    // TYPES-ERASURE.1 consolidation attempt turning out to need more than
    // one method -- see docs/known-issues/jit-skip-list-open-bans-20260725.md).
    // Broadening HIB-BIGINTEGER-AIOOBE.1's scope to also cover
    // `BigInteger` itself rather than adding a second always-checked
    // guard, since both now share one fail-closed disposition until the
    // x64 lowering bug is found.
    if class_name == "java/math/BigInteger" {
        // CROSS-REFERENCE (2026-07-26): the removed SUNEC-INTPOLY ban
        // (see the -- REMOVED comment near sun/security/util/math/intpoly/
        // below) depends on THIS ban staying active -- its own root cause
        // needs BigInteger JIT-compiled too. Do not lift this ban without
        // re-testing SUNEC-INTPOLY's P-384/P-521 EC keygen+sign+verify
        // scenario (EcIntPolyProbe.java) alongside it.
        return Some(SkipReason::BigIntegerArithmetic);
    }

    // ANTLR-COLDPATH.1 — the Groovy-shaded ANTLR runtime blanket ban is
    // liftable for cold-path validation, but the PredictionContext equality /
    // hash cluster is a known correctness defect. Keep that cluster
    // interpreted even when `CRATONVM_JIT_ALLOW_PACKAGES=groovyjarjarantlr4/`
    // lifts the surrounding package, so validation compiles the ATN simulator
    // leaves without reintroducing the old null-PredictionContext parse
    // corruption.
    if is_antlr_prediction_context_miscompile(class_name, method_name) {
        return Some(SkipReason::RustJvmTestFixture);
    }

    // PROXY-JITCALL.1 — REMOVED 2026-07-26. Historically, generated
    // `$ProxyN` dynamic-proxy classes SIGSEGV'd/hung when JIT-compiled
    // together with methods on the OTHER side of the
    // `Proxy$Dispatch.invokeProxy` call (e.g. Spring's meta-annotation
    // introspection calling `annotationType()`/`hashCode()`/`equals()`
    // repeatedly on many distinct `$ProxyN` receiver classes under
    // `CRATONVM_REAL_ANNOTATIONS=1`) -- a JIT-JIT call-boundary
    // register-preservation bug in the same family as
    // `docs/internal/fixed-suite-bugs/jit-regalloc-callee-saved-clobber-family.md`,
    // but not covered by that family's targeted list (the inert
    // `is_known_miscompile` block, removed 2026-07-27) since this check was
    // unconditional.
    // Re-verified 2026-07-26 with `bench/ProxyJitCallProbe.java` (real
    // annotation-proxy instances -- 8 distinct `$ProxyN` classes via
    // 8 distinct annotation types, `CRATONVM_REAL_ANNOTATIONS=1`,
    // `annotationType()`/`hashCode()`/`equals()`/`toString()` called
    // 300k times, `CRATONVM_JIT_THRESHOLD=1` to force maximal JIT
    // engagement on both sides of the call boundary): no crash, no hang,
    // correct output. No longer reproduces on current dev -- fixed as a
    // side effect of general JIT/dispatch correctness work since this ban
    // was added. `bench/ProxyJitCallProbe.java` is the regression witness.

    // SPR-AOT-TESTNG-MAPS.1 -- REMOVED 2026-07-26. Re-verified with a
    // standalone probe (`TestNgMapsProbe.java`, real testng-7.12.0.jar)
    // hammering `Maps.newConcurrentMap()`/`newHashMap()` +
    // `computeIfAbsent` 20000 times, plus a `CRATONVM_JIT_THRESHOLD=1`
    // aggressive-compilation pass (5000 calls): 0 failures, correct values,
    // no recompute-on-second-call, correct `ConcurrentHashMap` identity in
    // every configuration. No longer reproduces on current dev.
    // `TestNgMapsProbe.java` is the regression witness.

    // TOMCAT-DOHEAD-JUNIT-ITERATOR.1 -- REMOVED 2026-07-26. Originally
    // (2026-07-22) an invalid-write matrix deterministically proved that
    // compiling org/junit/runners/model/TestClass.collectAnnotatedMethodValues
    // corrupted its enhanced-for iterator local (`i$` observed null),
    // leaking failures until OutOfMemoryError. Re-verified with a
    // standalone probe (`TomcatDoheadJunitIteratorProbe.java`, real
    // junit-4.13.2.jar) calling the exact banned method
    // (`getAnnotatedMethodValues(target, Rule.class, TestRule.class)`,
    // which internally invokes collectAnnotatedMethodValues's enhanced-for
    // loop over 5 real `@Rule`-annotated methods per call) 20000-40000
    // times: baseline, plus a `CRATONVM_JIT_THRESHOLD=1`/`CRATONVM_JIT=
    // threshold` forced-aggressive-compilation pass -- 0 failures, 0 null
    // results, correct count (5) every call in every configuration. No
    // longer reproduces on current dev. `TomcatDoheadJunitIteratorProbe.java`
    // is the regression witness.
    //
    // HISTORY: when this was removed on 2026-07-26 the class still stayed
    // interpreted by default, because the separate blanket "org/junit/" ban
    // further down also matched org/junit/runners/model/TestClass and caught
    // it first. That removal was nonetheless sound: unlike the other shadowed
    // removals in that sweep, every probe run above ALSO set
    // `CRATONVM_JIT_ALLOW_PACKAGES=org/junit/`, confirming
    // collectAnnotatedMethodValues is safe when genuinely JIT-compiled rather
    // than merely riding on the blanket ban. The blanket ban was itself
    // removed 2026-07-27 (see the TEST-HARNESS BLANKET BANS comment below), so
    // this class is now JIT-eligible by default and this removal is finally
    // observable in a plain run.

    // REACTOR-ADDCAP.1 / REACTOR-FLUXCREATE.1 -- REMOVED 2026-07-26.
    // Re-verified with a standalone probe (`ReactorAddCapProbe.java`, real
    // reactor-core-3.8.6.jar) driving `Flux.create(...)` with a
    // `BaseSubscriber` requesting demand 1-at-a-time (the exact shape that
    // exercises `Operators.addCap` and `FluxCreate$BaseSink`/
    // `$BufferAsyncSink`'s accounting/drain per item): 300 independent
    // 100-item runs (baseline) plus 300 more with both bans' packages
    // explicitly allowed, plus a 200-run `CRATONVM_JIT_THRESHOLD=1`
    // aggressive-compilation pass -- 0 stalls, 0 count mismatches, 0
    // out-of-order deliveries in every configuration. No longer reproduces
    // on current dev. `ReactorAddCapProbe.java` is the regression witness.
    // JETTY-WSIO.1 -- REMOVED 2026-07-26. Re-verified with a standalone
    // probe (`JettyWsIoProbe.java`, real jetty-12.1.10 embedded server +
    // `WebSocketClient`, programmatic `Session.Listener` API) exchanging
    // 64KiB binary frames over a real loopback WebSocket connection: 150
    // round trips (baseline) + 150 more with both packages explicitly
    // allowed, plus a 100-round `CRATONVM_JIT_THRESHOLD=1`
    // aggressive-compilation pass -- 0 stalls, 0 payload corruption in
    // every configuration. No longer reproduces on current dev.
    // `JettyWsIoProbe.java` is the regression witness.

    // HIB-LONGTAIL.1 (2026-07-15; narrowed 2026-07-20, again 2026-07-27 — it
    // covers `org/h2/` ONLY now, see the removal note just above the `if`):
    // Hibernate's H2-backed
    // collection loading runs correctly in the interpreter, but JITting the H2
    // SQL/MVStore and ANTLR-runtime together turned ordinary 9-second HotSpot
    // tests into multi-minute CratonVM runs. Originally this also blanket-banned
    // ALL of java.util (except regex) unconditionally under the conservative
    // policy, reasoning that the three packages needed to be interpreted
    // together. That java.util term:
    // (a) contradicted the T1.1.g invariant a few lines below (java/util/* is
    //     JIT-eligible again outside the few remaining targeted lists) and
    //     broke 7 of this module's own unit tests the day it landed (see git
    //     blame on this comment vs. `tier1_skip_list_no_blanket_java_util_ban`
    //     and friends — those tests predate this ban and were never updated to
    //     match it);
    // (b) turned out to be unnecessary: the ACTUAL root cause of the Hibernate
    //     longtail (see `docs/internal/fixed-suite-bugs/hib-generic-timeout-hang-longtail-resolved-20260715.md`)
    //     was the executor-compatibility bridge returning placeholder
    //     `FutureTask`s, fixed the same day in `native-builtins`/`native-collections`
    //     — not a java.util JIT-throughput interaction;
    // (c) unconditionally force-interpreted java.util for every OTHER test in
    //     the whole suite that never touches H2 or ANTLR, which is exactly
    //     what made `testSlicesDense` (ES vector search, heavy `Arrays`/`BitSet`/
    //     `Objects` usage, no H2/ANTLR in sight) stay stuck at ~600s — see
    //     `docs/known-issues/elasticsearch-suite/ES-PERF-20260719-testSlicesDense-interpreter-throughput.md`.
    // Narrowed back to just the two packages actually implicated (H2, ANTLR
    // runtime) that motivated this rule. (Historical note on why the old
    // java.util term carved out `java.util.regex`: DefaultCatalogAndSchemaTest's
    // AssertJ checks repeatedly compile patterns, and interpreting
    // Pattern.compile turned that finite check into a watchdog timeout while
    // its JIT path was stable — moot now that java.util is JIT-eligible again.)
    //
    // 2026-07-26 re-test. The perf framing above is stale (the Hibernate
    // longtail it cites was root-caused to the executor bridge on 2026-07-15),
    // and so is the correctness reason the 2026-07-25 sweep replaced it with: a
    // systemic `Schema  not found` metadata corruption that turned out to be
    // two general x64 defects, both since fixed (`13055f75c`) — the compact
    // `String.coder`/`hash` offsets and a reload-elision mirror leaking across
    // a control-flow join. That cluster is extinct (0 of 218 classes).
    //
    // The ban nevertheless STAYS, on fresh evidence. A same-binary 218-class
    // A/B was PASS 158 with it and PASS 149 without; ten classes regressed
    // (the -9 is net — `TestMvccMultiThreaded2` improved in the same run).
    // Six of the ten have since been closed, including both that named their
    // mechanism outright, and both turned out to be general x64 defects rather
    // than anything H2-specific: an array reporting itself an instance of its
    // component type (`373e780b7`, hit by `TestObjectDataType`) and
    // invokespecial resolving its target by name instead of through the
    // caller's loader (`f16acca12`, hit by `TestUpgrade`).
    //
    // 2026-07-27 re-test, and the reason to distrust every per-class verdict
    // recorded above. All three classes that held this ban on 2026-07-26 are
    // fixed, and none of them was an H2 bug:
    //   * `TestStreamStore` -- not intermittent (10/10 vs 0/10). Two stacked
    //     defects: `ThreadPoolExecutor.shutdown()` interrupted RUNNING workers
    //     rather than only idle ones (the JDK separates them with
    //     `w.tryLock()` in `interruptIdleWorkers`), and
    //     `AbstractInterruptibleChannel.interruptor` was ALWAYS null because
    //     the `FileChannelImpl` bridge never runs the JDK constructor -- so an
    //     interrupt during channel I/O NPEd instead of closing the channel.
    //   * `TestFreeSpace` / `TestNestedJoins` -- never hangs. A JIT-compiled
    //     caller's `invokevirtual` never reached a JIT-compiled callee, because
    //     `helpers::direct_virtual_compiled_callee_entry_enabled()` was
    //     default-OFF and gates the only write of `mic.cached_entry_ptr`.
    //     Compiling a method made its callees run INTERPRETED; lifting this
    //     ban is what made the callers compiled, so this ban was hiding a
    //     general JIT defect rather than an H2 one. Now default-ON.
    //
    // The ban still STAYS, on entirely new evidence. Same-binary 218-class A/B
    // with that dispatch fix in place: 166 PASS with this ban, 155 PASS + 3
    // CRASH (`TestRunscript`, `TestPageStoreCoverage`, `TestReopen`) without.
    // `TestReopen` is notable -- it was recorded as CLOSED on 2026-07-26 and
    // regressed to a CRASH once compiled H2 code actually started running
    // compiled.
    //
    // The sharpest blocker is now a CORRECTNESS failure, not throughput: with
    // this ban lifted, `TestFileSystem`'s `memLZF:` `testConcurrent` fails
    // intermittently (2 of 4 runs) with `Expected: 3900 actual: 3897` /
    // `Expected: 5128 actual: 5168`. The reader holds the same
    // `AtomicIntegerArray` spin lock the writer held and reads `expected` and
    // then the file; seeing fresh file bytes with a stale `expected` is a
    // memory-ordering violation, since the writer wrote the file, then
    // `expected`, then released the lock. Root-cause that -- compiled `org/h2`
    // code reordering across `AtomicIntegerArray.set`/`compareAndSet` -- before
    // attempting this ban again.
    //
    // Full evidence, repro commands and the residual-4 measurement:
    // `docs/known-issues/h2/h2-jitban-residuals-20260726.md`.
    //
    // 2026-07-27: the `org/antlr/v4/runtime/` half is REMOVED — this rule is
    // now `org/h2/` only. Nothing above ever held the ANTLR half: every piece
    // of evidence this ban still rests on comes from the H2 suite, which never
    // loads an `org/antlr/` class at all (the H2 parser is hand-written). The
    // ANTLR half was isolated against the one real fixture that does exercise
    // it heavily — Hibernate ORM 8.0's own HQL suite, where every query goes
    // through `org/antlr/v4/runtime/` — as a same-binary A/B over all 57
    // `org.hibernate.orm.test.hql` classes with `org/hibernate/` still banned
    // at the time (HIB-TEMPORAL.1) so ANTLR was the only variable:
    // byte-identical
    // per-class ok/failed/aborted counts, 0 failures either way, and the
    // ban-removed binary re-confirmed the same. That also covers this rule's
    // ancestor claim (HIB-ANTLR.1: a JIT-compiled parse leaving
    // `ATNState.transitions` null and corrupting the *next* parse in the same
    // process) — CratonRunner runs every class in one process, so hundreds of
    // consecutive HQL parses shared one ATN, and nothing degraded.
    // Evidence: `docs/internal/jit-bans/hib-antlr-1-removed-shadowed-20260726.md`.
    // The narrow `PredictionContext` equality/hash guard
    // (`is_antlr_prediction_context_miscompile`, ANTLR-COLDPATH.1 below) is
    // unaffected and still applies to both the shaded and unshaded runtimes.
    // 2026-07-28 re-test (against the 2026-07-27 atomic-array RMW fix,
    // `ffb8dfa22`, which was never re-tested against this ban afterward):
    // the fix did NOT clear this ban's residuals. A/B control (same binary,
    // ban active vs. lifted, one class at a time) confirms THREE classes
    // still PASS with the ban active and CRASH the moment `org/h2/` is
    // JIT-eligible -- genuine, reproducible regressions, not stale evidence:
    //   * `TestPageStoreCoverage` and `TestReopen` share one root cause:
    //     `InternalError: precise deoptimization unavailable for
    //     org/h2/mvstore/db/RowDataType.read(...)  at bci 50; refusing
    //     side-effecting replay` -- `interpreter.rs`'s deliberate safety
    //     refusal (better to throw than silently replay committed side
    //     effects) when a JIT-compiled method traps and precise resume
    //     isn't available for it. A precise-maps/deopt coverage gap, not a
    //     new bug class; left for that effort rather than patched here.
    //   * `TestRunscript` fails a DIFFERENT way: `assertEqualDatabases`
    //     diffs diverge (`AssertionError: expected: INSERT INTO
    //     "PUBLIC"."TEST2" VALUES(462);`) -- a genuine data-correctness
    //     bug, distinct root cause, not yet bisected.
    // A fourth class, `TestFileSystem`, hangs to the runner's 300s timeout
    // in BOTH arms (ban active AND lifted) -- ruled OUT as evidence for or
    // against this ban; it is not a JIT regression at all. Full evidence:
    // `docs/known-issues/h2/h2-jitban-longtail1-residuals-20260728.md`.
    //
    // 2026-07-28, THIRD pass: all three of those regressions were ONE bug, and
    // it is fixed. The generated MIC/PIC cascade called its cached compiled
    // callee and never inspected the result, so a callee that trapped handed
    // its `i64::MIN` deopt sentinel to the caller as if the CALLER had
    // deopted, and left its reconstructed frame in the thread's single stash
    // slot for an unrelated sink to mis-attribute -- which de-speculated the
    // wrong method and then, correctly, refused to resume a frame that was not
    // its own. That refusal IS the `InternalError` above; `TestRunscript`'s
    // script diff is the same escape landing somewhere that did not refuse,
    // which is why its stack carried no `RowDataType` frames and it read as a
    // separate defect. Both direct-call sites now service the sentinel through
    // `jit_service_callee_deopt`. `RowDataType.read`'s frame map was ALSO
    // imprecise (a reused local slot voted `Ambiguous` whole-method); that is
    // fixed too, by a per-bci reaching-kind dataflow, but it was not
    // sufficient on its own.
    //
    // The ban nevertheless STAYS, now for exactly ONE class. A full 218-class
    // A/B (one arm at a time) makes lifting worth +7 PASS -- 155/18/45 banned
    // vs 162/22/31 lifted, nine classes recovering -- and every per-class
    // change was re-run in isolation. Only `org.h2.test.jdbc.TestMetaData` is
    // a real regression (PASS 3/3 banned, FAIL 3/3 lifted), and it is
    // PRE-EXISTING: the pre-fix binary fails it identically. It is not an H2
    // bug either -- bisected to `org/h2/command/query/SelectGroups`, whose
    // `new TreeMap<>(session)` loses its comparator once the allocating method
    // is compiled, and reproduced in 60 lines of pure JDK code by
    // `apps/h2database-suite-runner/probes/TreeMapCmpProbe.java` (HotSpot
    // 40000/40000, CratonVM fails from iteration ~503). LIFT THIS BAN once
    // that is fixed; everything else is already in place.
    if class_name.starts_with("org/h2/") && !package_allowed("org/h2/", allow_packages) {
        return Some(SkipReason::RustJvmTestFixture);
    }

    // HIB-LONGTAIL.2 -- REMOVED 2026-07-26. Re-verified with a standalone
    // probe (AttributesImplGrowthProbe.java, pure JDK org.xml.sax.helpers,
    // no external jar needed) driving addAttribute/removeAttribute across
    // 20000 independent AttributesImpl instances at varied sizes (forcing
    // many ensureCapacity growth calls per instance) -- baseline + package
    // explicitly allowed + a 5000-instance CRATONVM_JIT_THRESHOLD=1
    // aggressive-compilation pass: 0 length/value mismatches in every
    // configuration. No longer reproduces on current dev.
    // AttributesImplGrowthProbe.java is the regression witness.

    // HIB-LONGTAIL.3 -- REMOVED 2026-07-26 (shadowing analysis, not a
    // real-app probe). A constructor that initializes a field via
    // putfield -- as this ban's own description says
    // GenerationTargetToScript.<init> does, to set its ScriptTargetOutput
    // field -- is unconditionally classified InitComplexity::Complex by
    // classify_init_complexity (any putfield/putstatic/monitorenter/
    // monitorexit/invokedynamic disqualifies Trivial). Every
    // should_skip_jit_with_init call site computes that classification
    // from the actual method's own bytecode and only sets
    // skip_init_check=true when Trivial; when false, the generic
    // `if !skip_init_check { if method_name == "<init>" { return
    // Some(Constructor) } }` gate above already unconditionally bans this
    // exact constructor before this specific entry could ever be
    // reached. Same shadowing pattern as W2-CHM's Integer/Long <init>
    // entries (see that comment: constructors are banned by the generic
    // <init> gate anyway, so the entries are redundant but document the
    // archetype) and as SPRINGBOOT-WITHOUT-JACKSON.2's removal earlier
    // this session. Verified by reading classify_init_complexity and
    // every should_skip_jit_with_init call site (vm/src/runtime/
    // interpreter.rs, offload_jit_gate.rs) rather than a standalone
    // probe -- no hibernate-tools jar was available on this host to
    // build a real repro, but none is needed: this removal changes no
    // observable behavior, the constructor stays interpreted via the
    // structural non-trivial-constructor gate regardless.
    // T1.1.g — the historical blanket bans for `java/util/*` and
    // `cratonvm/*` were narrowed to targeted per-method exclusions.
    // Those targeted exclusions guarded the callee-saved-GPR local-home
    // regalloc family. The x64 backend now keeps those GPR local homes
    // default-off, so the methods are JIT-eligible again on that safe
    // path. If a developer opts back into the old register homes for
    // diagnosis, or if a non-x64 backend has not installed an equivalent
    // guard, keep the targeted list active.
    //
    // JUNIT.1 -- REMOVED 2026-07-26 (see the removal record where
    // `is_known_miscompile` used to be called, further below, for the
    // re-verification evidence). The CRATONVM_JIT_UNBAN_JUNITCORE DBG bypass that used to
    // live here is no longer needed for JUNIT.1's OWN narrow check.
    // CORRECTION (2026-07-26, same day): as landed, this removal was a
    // shadowed no-op, like SPRINGBOOT-WITHOUT-JACKSON.2 and HIB-ANTLR.1 --
    // JUnitCore.main was NOT actually JIT-eligible by default, because the
    // separate blanket "org/junit/" ban below also matched
    // org/junit/runner/JUnitCore and was never lifted during JUNIT.1's own
    // retest (JUnitCoreMainProbe.java's 110 runs used only
    // CRATONVM_JIT_THRESHOLD=1, not CRATONVM_JIT_ALLOW_PACKAGES=org/junit/).
    // "JIT-eligible unconditionally now" was therefore an overclaim at the
    // time.
    //
    // RESOLVED 2026-07-27: re-run properly with the shadow lifted before
    // removing the blanket ban -- `JUnitCoreMainProbe` driven one call per
    // process with `CRATONVM_JIT_THRESHOLD=1` (so `main`, which runs once per
    // process and exits, is compiled on that single invocation) AND
    // `CRATONVM_JIT_ALLOW_PACKAGES` covering org/junit/. Three test shapes
    // (trivial pass, heavy-allocation pass, intentional fail) x 40 runs x
    // baseline/lifted = 240 runs, 0 wrong exit codes, 0 crashes -- including
    // the failing shape correctly still exiting 1. JUnitCore.main is now
    // genuinely JIT-eligible by default and genuinely verified, not a
    // shadowed no-op.

    // SPB.9-COMMONS-LOGGING (2026-07-26, same day as the SPB.9 blanket
    // slf4j/logback/commons-logging removal above): re-banned
    // `org/apache/commons/logging/` specifically, UNCONDITIONALLY (not
    // inside the Conservative-only heuristic block below, unlike most
    // provisional bans in this file) because this guards a REAL, live-fire
    // Spring Boot regression with concrete reproduction evidence, not a
    // suspected/provisional concern.
    //
    // `org.springframework.boot.context.logging.LoggingApplicationListenerTests`
    // went from 16 to 34 failures (of 41) once the blanket SPB.9 removal
    // landed, with a NEW signature never seen before:
    // `java.lang.IllegalStateException: Unknown FilterReply value: DENY`
    // thrown from `ch.qos.logback.classic.Logger.isTraceEnabled` during
    // `LoggingApplicationListener.initialize` ->
    // `LogbackLoggingSystemProperties.apply`/`applyRollingPolicy` ->
    // `PropertySourcesPropertyResolver.getProperty`. `FilterReply` is a
    // Logback enum (DENY/NEUTRAL/ACCEPT) -- a JIT-caused heap-corruption
    // signature, the same archetype documented throughout this file (a
    // miscompiled store elsewhere clobbers an unrelated live object).
    // Confirmed the regression was JIT-caused (not merely a coincidental
    // dev-drift correlation) by disabling JIT entirely on the exact
    // regressed binary (`CRATONVM_DISABLE_JIT=1`): failures dropped back to
    // 16/41. Bisected the three SPB.9 packages by selectively re-banning
    // subsets of them in an isolated worktree with incremental rebuilds:
    // logback-only re-banned -> still 34/41; logback+slf4j re-banned (only
    // commons-logging left JIT-eligible) -> still 34/41; commons-logging-only
    // re-banned (logback+slf4j left JIT-eligible) -> back to 16/41, matching
    // the clean baseline exactly. This isolates the miscompile to
    // `org/apache/commons/logging/` specifically -- `org/slf4j/` and
    // `ch/qos/logback/` are genuinely safe to leave JIT-eligible (see the
    // SPB.9 removal comment above), matching what `SlfLoggingProbe.java`
    // actually exercised: real Commons Logging was NOT on that probe's
    // classpath, it used `jcl-over-slf4j`, an API-compatible but
    // BYTECODE-DIFFERENT implementation of `org/apache/commons/logging/`,
    // so the probe never actually JIT-compiled the real Commons Logging
    // 1.3.x classes Spring Boot's real test classpath uses. The one
    // JIT-dispatched Commons-Logging method observed in a
    // `CRATONVM_DBG=jit-dispatch` trace of the failing run was
    // `org/apache/commons/logging/impl/Slf4jLogFactory.access$000()
    // Lorg/slf4j/Marker;` (Commons Logging 1.3.6's built-in SLF4J adapter's
    // synthetic private-static-field accessor for its `MARKER` field),
    // called extremely frequently during per-class logger wiring --
    // plausible trigger for the miscompile, though the exact JIT lowering
    // bug for this bytecode shape was not further pinned down (root cause
    // is "some `org/apache/commons/logging/` method", not yet narrowed to a
    // single method/bytecode pattern). Not lifted by
    // `CRATONVM_JIT_ALLOW_PACKAGES` on purpose -- a confirmed corruption bug
    // should not have a casual opt-out; a developer who genuinely needs to
    // bisect further can still comment this guard out locally.
    if class_name.starts_with("org/apache/commons/logging/") {
        return Some(SkipReason::RustJvmTestFixture);
    }

    if policy == SkipPolicy::Conservative {
        if is_unconditional_hash_miscompile_cluster(class_name, method_name)
            && !package_allowed(class_name, allow_packages)
        {
            return Some(SkipReason::JavaUtilCollection);
        }

        // Keep the proven AQS/RRWL queue-synchronizer hazards interpreted under
        // Conservative without broadening the guard to all java/* methods.
        if is_known_miscompile_aqs_family(class_name, method_name)
            && !package_allowed(class_name, allow_packages)
        {
            return Some(SkipReason::JavaUtilCollection);
        }
        // JASPER-JDT.2 (`org/eclipse/jdt/internal/compiler/parser/`) and
        // JASPER-JDT.3 (`org/eclipse/jdt/internal/compiler/ast/`) -- REMOVED
        // 2026-07-28, this time with the defect behind them root-caused first.
        //
        // Both were real Eclipse-JDT (ECJ) miscompiles, found through Tomcat's
        // use of ECJ to compile JSPs. JASPER-JDT.2 (2026-07-08): nondeterministic
        // parser-adjacent heap corruption / OOM, first face an
        // `ArrayIndexOutOfBoundsException` in `Parser.parse`, bisected to
        // `Parser.consumeRule`. JASPER-JDT.3 (2026-07-10): an AIOOBE reported at
        // `QualifiedNameReference.analyseCode` -- a delegating wrapper with no
        // array access of its own -- during FORM-auth JSP compilation.
        //
        // They were removed once before, on 2026-07-26, and RESTORED on
        // 2026-07-27. Every one of those four re-verification runs was made
        // while `helpers::direct_virtual_compiled_callee_entry_enabled()` was
        // default-OFF, and that flag gates the only write of
        // `mic.cached_entry_ptr`: with it off, a compiled caller's
        // `invokevirtual` never reaches a compiled callee at all, so the
        // dispatch this family lives in was inert and those runs measured
        // nothing. With the flag on the family came straight back, with a new
        // face -- real Tomcat `jakarta.el.TestOptionalELResolverInJsp` failing
        // its JSP compile with `ClassCastException:
        // org.eclipse.jdt.internal.compiler.ast.QualifiedTypeReference cannot be
        // cast to org.eclipse.jdt.internal.compiler.ast.FieldDeclaration`
        // (`JasperException: Unable to compile class for JSP` -> HTTP 500).
        //
        // That failure is now fixed, and this removal rests on knowing by what.
        // Bisected over the 150 commits between the restore point (`4f280090f`,
        // 3/3 FAIL) and dev, two runs per step, both packages JIT-allowed and
        // the direct-entry path ON throughout: the first clean commit is
        // `613b10f4c`, "fix(jit): LICM/speculative pre-header bypassed by a
        // branch into the loop header". Every speculative pre-header the x86-64
        // backend emits -- LICM hoists AND speculative bounds-check-elision
        // guards -- is emitted inline at the loop-header PC and is skipped by
        // any forward branch that jumps straight into the header, so such a loop
        // ran against an uninitialised hoist slot, or with bounds checks elided
        // by a guard that never executed. ECJ's `Parser`/AST code is full of
        // that shape, and an unchecked array read handing back the wrong live
        // object is exactly a `QualifiedTypeReference` arriving where a
        // `FieldDeclaration` was expected -- and exactly the AIOOBE /
        // heap-corruption faces these two bans were originally opened for.
        //
        // Re-verified on the real Tomcat fixture with the direct-entry path at
        // its default (ON) and both packages actually compiling (166 parser + 29
        // ast methods per run, counted with `CRATONVM_DBG_DUMP_JIT=LIST`, so the
        // lift is not a no-op): `TestOptionalELResolverInJsp`,
        // `org.apache.jasper.compiler.TestCompiler` (12 tests) and
        // `org.apache.catalina.authenticator.TestFormAuthenticatorA/B/C` (9/6/7
        // tests), repeat runs, all clean.
        //
        // Any future re-test of this family must leave
        // `CRATONVM_JIT_DISPATCH_CACHE_VIRTUAL_DIRECT_ENTRY` at its default
        // (on), or it measures nothing -- that is what voided the 2026-07-26
        // removal.

        // ES-FRAGILE-CLUSTER.1 (blanket `org/elasticsearch/`) -- REMOVED
        // 2026-07-27. The ban's last re-verification (the retired
        // `es-fragile-cluster-confirmed-needed-20260726` write-up)
        // kept it on the strength of a single class,
        // `index.mapper.blockloader.FloatFieldBlockLoaderTests`, which gained
        // 3 failures with the package allowed (38/120 -> 41/120) while the
        // other 17 classes in that spread sample were byte-identical. That
        // regression, and the `cluster.NodeConnectionsServiceTests` SIGSEGV
        // investigated in the same window, were both the ownerless
        // stale-compiled-entry family fixed in `vm/src/jit/helpers.rs` and
        // `vm/src/runtime/interpreter.rs`: `try_jit_compile_callee` handed out
        // a bare compiled-entry address after releasing its
        // `Arc<CompiledMethod>`, so a concurrent tier-up `JitCache::put` could
        // unmap the body while generated code still called it. Re-measured
        // with that fix in place: the 19-class spread sample and 3 runs each
        // way of `TextFieldMapperTests` are identical ban-on vs ban-off,
        // `FloatFieldBlockLoaderTests` is 31 failures both ways, and
        // `FloatHierarchicalKMeansTests` hangs 3/3 with the ban ON but
        // completes in ~50s with it lifted.

        // ES-HAMCREST.1 -- REMOVED 2026-07-26. Re-verified with a
        // standalone probe (`HamcrestProbe.java`, real hamcrest-core +
        // hamcrest + hamcrest-library 3.0 jars) stressing `equalTo`,
        // `containsString`, `hasSize`, `allOf`/`not`/`startsWith` across
        // 20000 varied true/false-case inputs (baseline + packages
        // explicitly allowed), plus a 5000-call `CRATONVM_JIT_THRESHOLD=1`
        // aggressive-compilation pass: 0 mismatches in every configuration.
        // No longer reproduces on current dev (the original bug's own
        // `System$1.findNative` FFM bridge fix + subsequent JIT correctness
        // work appears to have already closed it). The broader
        // `org/elasticsearch/` blanket ban that used to sit immediately above
        // this one was removed 2026-07-27 against the real ES 9.6.0-SNAPSHOT
        // fixture (the retired `es-fragile-cluster-confirmed-needed-20260726`
        // write-up).
        // `HamcrestProbe.java` is the regression witness for this entry only.


        // JSONSMART-PARSER.1 -- REMOVED 2026-07-26. Originally (2026-07-09)
        // the default JIT crashed inside emitted code after compiling parser
        // cursor methods (JSONParserString.read/readS,
        // JSONParserBase.skipSpace), reached via Spring's
        // JsonPathResultMatchersTests. Re-verified with a standalone probe
        // (`JsonSmartProbe.java`, real json-smart-2.3.jar) driving
        // JSONValue.parse/toJSONString/re-parse round trips over 5 varied
        // JSON shapes (whitespace, escapes, nesting, unicode, numbers) 20000
        // times: baseline, plus the package allowed, plus a
        // `CRATONVM_JIT_THRESHOLD=1` aggressive-compilation pass -- 0
        // failures in every configuration. No longer reproduces on current
        // dev. `JsonSmartProbe.java` is the regression witness.
        //
        // Re-confirmed 2026-07-27 against the stronger case that removal did
        // not cover: real json-smart-2.6.0 (not 2.3), 10 varied documents,
        // 300000 iterations = 3,000,000 parse/serialize/re-parse operations,
        // with `JSONParserString.read`/`readS`/`JSONParserBase.skipSpace`
        // verified JIT-compiled (CRATONVM_DBG_DUMP_JIT=LIST) -- 0 errors, and
        // 0 again under CRATONVM_JIT_THRESHOLD=1. The residual that re-testing
        // DID surface was VM-wide rather than json-smart's: the
        // trivial-constructor elision dropped the native-shadowed
        // `java/util/HashMap.<init>()V`, so a JIT-created HashMap got a
        // 32-bucket table where the interpreter gives 16 (fixed in
        // `is_elidable_construction`; regression net
        // vm/tests/jit_collection_ctor_identity.rs). Full writeup:
        // docs/internal/jsonsmart-parser-jit-retired-20260727.md.
        // JAXB (`org/glassfish/jaxb/`) -- REMOVED 2026-07-27. The ban existed
        // for a self-cast `QName cannot be cast to QName` seen while
        // Hibernate mapping metadata built JAXB's QName-heavy runtime graph,
        // and was re-confirmed on 2026-07-26 as an `UnmarshalException:
        // unexpected element (uri:"", local:"widget")` at iteration 81 of
        // `JaxbQNameProbe`. Re-verified 2026-07-27 against that same probe
        // (real jakarta.xml.bind / org.glassfish.jaxb 4.0.7, fresh
        // Marshaller + StringWriter + Unmarshaller per iteration, 4000
        // iterations x 6 runs, ban removed from this file entirely): 0
        // failures. The 2026-07-26 run of the same config on the *previous*
        // dev binary is also clean, so the QName corruption was closed by
        // general JIT work between those two dates, not by this session's
        // change. What this session DID fix is the separate general defect
        // the same probe kept tripping over first -- the LICM/speculative
        // pre-header bypass (`find_bypassable_loop_headers` in
        // `jit/src/x64.rs`), whose `AttributesImpl.ensureCapacity` face made
        // the 4000-iteration probe die with
        // `OutOfMemoryError ... anewarray ... length 1677721600` roughly one
        // run in three. See
        // `docs/internal/jit-licm-preheader-bypass-20260727.md`.
        // `docs/known-issues/repros/jitban-remaining-20260726/JaxbQNameProbe.java`
        // is the regression witness.

        // SPRING-HAZELCAST-XERCES-JIT.1 -- REMOVED 2026-07-26. Historically,
        // Hazelcast's schema validation passed the complete server suite
        // interpreted, but JIT compilation of the JDK-internal Xerces graph
        // corrupted `SchemaGrammar`'s SymbolHash state and raised an NPE in
        // `getGlobalTypeDecl` (2026-07-18). Re-verified 2026-07-26 with
        // `bench/XercesSchemaProbe.java` (real `javax.xml.validation`
        // schema validation -- 4 and separately 32 distinct XSD schemas,
        // repeated `Validator.validate()` calls, ~40k total iterations across
        // both shapes): no crash, no NPE. `CRATONVM_DBG_JITC=1` confirmed
        // `SymbolHash.hash`/`.search`/`.get` -- the exact class the original
        // bug named -- were actively JIT-compiled (`tier=C1`) throughout both
        // runs. No longer reproduces on current dev -- fixed as a side effect
        // of general JIT/GC correctness work since this ban was added.
        // `bench/XercesSchemaProbe.java` is the regression witness.

        // ES-JIT-DEOPT-GC.1 -- REMOVED 2026-07-26. Re-verified with a
        // standalone probe (`SnakeYamlEmitProbe.java`) exercising both a real
        // Jackson-YAML (`jackson-dataformat-yaml`) round trip -- matching the
        // original ES interval-provider serialization shape -- and a plain
        // SnakeYAML 1.33 `Emitter.emit` round trip directly, over nested
        // maps/lists/dates, 3000 iterations each: 0 mismatches. No longer
        // reproduces on current dev. `SnakeYamlEmitProbe.java` is the
        // regression witness. `is_snakeyaml_emitter_emit_jit_corruption` is
        // kept as a helper for now (no other caller) in case of regression.

        // TOMCAT-JNDIREALM-RDN.1 (2026-07-15) and TOMCAT-JNDIREALM-JIT.2
        // (2026-07-23) -- BOTH REMOVED 2026-07-26. RDN.1 kept the single
        // accessor `com/unboundid/ldap/sdk/RDN.getNameValuePairs` interpreted
        // (15/76 special-character-credential and escaped-OU failures in
        // Tomcat's `TestJNDIRealmIntegration`); JIT.2 then widened the ban to
        // ALL of `com/unboundid/` after a second, cross-package producer kept
        // reusing a zero-header `String` receiver during the in-memory LDAP
        // server's DN/RDN matching (assertion failures plus access
        // violations). Both were re-verified as still-live as recently as
        // 2026-07-26 02:43 UTC.
        //
        // Re-verified on this tree with the real 76-case matrix (real UnboundID
        // in-memory LDAP server, real sockets, the suite runner's own env:
        // CRATONVM_REAL_NET_SOCKETS / REAL_AQS / ROOTSNAP_CACHE). A same-box
        // control build at dev e4e4053bb (2026-07-24, the first dev commit
        // after JIT.2 was recorded) still fails 4 of 5 lifted runs with the
        // documented signature, so the harness genuinely reproduces here --
        // this is not a stopped-reproducing-on-Windows artifact.
        // `CRATONVM_DBG_JITC=1` confirms 566 UnboundID compile events per run
        // across every package the bisection implicated -- `ldap/sdk` (RDN
        // included: `getNameValuePairs`, `compare`, `compareTo`),
        // `ldap/matchingrules`, `ldap/listener`, `ldap/protocol`, `asn1`,
        // `ldif`, `util` -- i.e. the guarded code really is compiled now, C1
        // and C2, not merely admitted.
        //
        // The bans are removed only because the PRODUCER was found and fixed,
        // not merely because the assertions stopped failing: TOMCAT-JNDIREALM-
        // JIT.3, `thread.string_case_cache` missing from every cross-thread GC
        // root path (see `update_root_snapshot` / `deposit_root_snapshot_inner`
        // and the frozen-peer scan). Method bisection pinned the trigger to the
        // single method `com/unboundid/util/StaticUtils.toLowerCase`
        // (`CRATONVM_JIT_BISECT_SKIP` on it alone took the reclaimed-live-String
        // count from 3-4 per run to 0 while all other UnboundID classes stayed
        // compiled), and that method's only work is the case-conversion call
        // whose per-thread result cache was the unrooted holder. See
        // `docs/internal/fixed-suite-bugs/tomcat/jndirealmintegration-unboundid-jit-corruption-FIXED.md`.

        // `is_known_miscompile` (~950 lines, 189 (class, method) entries) --
        // REMOVED 2026-07-27, together with this file's own private
        // `callee_saved_gpr_local_homes_enabled()` gate that used to guard it.
        // Full writeup, including the per-entry inventory and the probe
        // evidence: docs/internal/is-known-miscompile-block-retired-20260727.md.
        //
        // Why it was safe to delete, in one paragraph: `a4913d8b` ("disable
        // callee-saved GPR local homes") put the ENTIRE targeted list behind
        // that gate on the premise that every entry was one register-allocator
        // family. The gate here was a SECOND, private copy of the switch that
        // defaulted OFF, while the real allocator switch it was named after --
        // `jit::x64::callee_saved_gpr_local_homes_enabled()` -- has defaulted
        // ON since precise JIT maps went default-on (2026-07-07). So for weeks
        // the allocation strategy these entries were meant to contain has been
        // ACTIVE while not one of them could fire, across every suite this repo
        // runs (Spring, Spring Boot, Tomcat, H2, WildFly, Elasticsearch). The
        // two entries in that list that turned out NOT to belong to the
        // callee-saved family were already re-banned unconditionally and are
        // untouched by this removal: `is_known_miscompile_aqs_family` (AQS /
        // ReentrantLock / ReentrantReadWriteLock) and
        // `is_known_miscompile_clq_family` (ConcurrentLinkedQueue), plus
        // `is_unconditional_hash_miscompile_cluster`, which still carries the
        // `Arrays.hashCode` / `Objects.hash` / `ArraysSupport.hashCode`
        // entries this list also listed.
        //
        // Direct verification rather than reachability argument alone: 19 of
        // the 189 entries were confirmed to be genuinely JIT-compiled TODAY
        // (`CRATONVM_DBG_JITC` compile events) by a purpose-built probe set,
        // and produced identical, oracle-checked results with the JIT on, with
        // `CRATONVM_JIT_THRESHOLD=1`, and under `--nojit` -- including every
        // entry of KC26.LR (SmallRye `ConfigValueProperties.load0` /
        // `LineReader.readLine`), KC-CRED.LAZY (Keycloak
        // `Password{Credential,Secret}Data.getAdditionalParameters`), the
        // Eclipse-JDT `HashtableOf*.rehash` family, BC-ASN1.1
        // (`Calendar.isFieldSet`), and part of ES-HANG-01 (WeakHashMap
        // iterators/spliterators) and EXEC.1 (`LinkedBlockingQueue`
        // offer/take). The remaining entries cannot be compiled at all in this
        // VM: most name methods CratonVM implements as Rust natives (every
        // `java/util/HashMap` entry, `String.toLowerCase`, `Integer.parseInt`,
        // `Class.getDeclaredFields`, `ByteBuffer.allocate`, ...), so their
        // bytecode never runs; the `<init>` entries are already caught by the
        // generic non-trivial-constructor gate; and the
        // `org/springframework/boot/`, `org/hibernate/` and `junit/` entries
        // are shadowed by separate, still-active blanket bans below.

        // TOMCAT-KEYEDLOCK-COMPUTE.1 (2026-07-26) -- an undiscovered JIT
        // miscompile in `KeyedReentrantReadWriteLock$LockImpl.lambda$lock$0`
        // (the ternary-with-allocation `BiFunction` passed to
        // `Map.compute`: `(k, v) -> v == null ? new CountedLock() : v`).
        // Real Tomcat repro: `TestCompiler`/`TestFormAuthenticatorA` under
        // JIT throw `NullPointerException: Cannot read field "count"` from
        // `KeyedReentrantReadWriteLock$LockImpl.lock` -- i.e.
        // `locksMap.compute(key, ...)` itself returns null even though the
        // BiFunction can never return null. Minimal 25-line standalone
        // repro (`ComputeInitProbe.java`: a `ConcurrentHashMap<String,
        // Counted>` where `Counted` has one `AtomicInteger` field, called
        // via `map.compute(key, (k, v) -> v == null ? new Counted() : v)`
        // in a loop) reproduces deterministically at iteration 0 -- no
        // warmup needed. Confirmed JIT-only: identical repro under
        // `--nojit` is 500000/500000 clean. Bisected with
        // `CRATONVM_JIT_DENY`/`CRATONVM_JIT_BISECT_SKIP` (no rebuild): the
        // caller (`main`) and the constructed class's own `<init>` are each
        // individually NOT the culprit (forcing either alone to interpret
        // does not fix it); forcing just the lambda body
        // (`ComputeInitProbe.lambda$main$0`) to interpret fixes it
        // completely. The bug is therefore in how the JIT compiles a
        // lambda body shaped like `v == null ? new X() : v` (branch,
        // allocate-and-construct on one arm, pass through an existing
        // reference on the other, merge, return) -- plausibly the same
        // callee-saved-clobber archetype as
        // `docs/internal/fixed-suite-bugs/jit-regalloc-callee-saved-clobber-family.md`,
        // but this exact shape (a lambda, not a named user method) was not
        // covered by that family's fix or its targeted list. Verified fix:
        // `TestFormAuthenticatorA` no longer hits this NPE with this lambda
        // pinned (a second, independent JIT bug in
        // `org/apache/catalina/webresources/` remains open for that test,
        // tracked separately). Keep this one lambda interpreted until the
        // ternary-with-allocation lowering bug is found generically.
        if class_name == "org/apache/tomcat/util/concurrent/KeyedReentrantReadWriteLock$LockImpl"
            && method_name == "lambda$lock$0"
        {
            return Some(SkipReason::RustJvmTestFixture);
        }

        // ALV5th (2026-07-10) -- ConcurrentLinkedQueue is the same
        // allocate-then-CAS hazard as the AQS family above: offer() does
        // `Node<E> newNode = new Node<E>(e);` then CASes it onto the tail
        // (tryCasSuccessor -> NEXT VarHandle compareAndSet) or appends it via
        // Node.appendRelaxed. Repro: org.junit.runner.Description.fChildren
        // is a ConcurrentLinkedQueue (JUnit 4.13+); a Parameterized test with
        // enough cases (~40+) crosses the instance-method JIT tier-up
        // threshold (CRATONVM_JIT_VIRTUAL_TIERUP, default on) mid-suite,
        // JIT-compiles offer()/tryCasSuccessor(), and a subsequent call
        // raises a message-less NullPointerException from JIT-compiled code
        // (a spurious null-checked store, not a GC/root-scanning issue --
        // gc_quiescence::is_active() was confirmed false at every relocation
        // preceding the crash, ruling out a mid-JIT-call GC race). Verified
        // CRATONVM_JIT_VIRTUAL_TIERUP=0 alone fixes it and
        // CRATONVM_BG_COMPILE=0 alone does not, isolating the defect to this
        // tier-up's JIT codegen for the allocate-then-CAS idiom, matching the
        // AQS family exactly. See
        // docs/known-issues/tomcat-08-07/accesslogvalve-rewritevalve-connection-failures.md.
        // Deliberately unconditional (not gated by
        // callee_saved_gpr_local_homes_enabled()), same rationale as the AQS
        // family immediately above.
        if is_known_miscompile_clq_family(class_name, method_name)
            && !package_allowed(class_name, allow_packages)
        {
            return Some(SkipReason::JavaUtilCollection);
        }

        // KC26-PIC.1 / KC26-PIC.2 — LIFTED 2026-07-27. The ban kept
        // `org/keycloak/`, `picocli/` and `io/smallrye/` interpreted under
        // Conservative because, on 2026-07-05, `PicocliTest` timed out at 265s
        // with default JIT and completed in ~168s under
        // `CRATONVM_JIT_DENY=org/keycloak/,picocli/,io/smallrye/`. That was a
        // *throughput* claim, never a miscompile, and it could not be
        // re-checked for two years of sessions because the real Keycloak
        // server would not boot at all under CratonVM (a classloader
        // stub-fabrication family, fixed 2026-07-27 — see
        // docs/internal/keycloak/keycloak-boot-blocked-version-null-20260726.md).
        //
        // With that boot working, the claim was measured directly on the real
        // Keycloak 26.6.1 `quarkus-dist` server (`kc.sh start-dev`, time from
        // launch to the end-of-startup marker, JIT on):
        //
        //   ban in place (n=6):        126 198 163 195 165 158  (mean 168s)
        //   all four allowed (n=7):    199 183 166 263 183 151 179 (mean 189s)
        //   single packages (n=1 each): org/keycloak/ 199s   picocli/ 284s
        //                               io/smallrye/ 236s    io/reactivex/ 171s
        //
        // The two distributions overlap completely: this build host runs at
        // load average 40-80 with ~15 concurrent sessions, and the SAME
        // configuration varies 126s-198s run to run. Three back-to-back
        // interleaved pairs went 195/183, 165/151, 158/179 — the ban-lifted
        // side won two of three. Every one of the 13 runs reached the
        // end-of-startup marker; the single early exit seen during the sweep
        // was an `org/keycloak/`-only run launched immediately after a previous
        // run's teardown (H2 file lock still held), and both repeats with a
        // settle delay passed. So there is neither a reproducible throughput
        // cost nor any correctness failure left to justify the ban on the
        // workload it was written for.
        //
        // Not re-tested: the `PicocliTest` / `RealmModelTest` JUnit classes
        // themselves — no compiled Keycloak test classes exist on this Linux
        // build host (only the `apps/keycloak` checkout on the Windows side
        // has them). If those classes ever regress, the ban is restorable
        // ad-hoc with `CRATONVM_JIT_DENY=org/keycloak/,picocli/,io/smallrye/`
        // without touching this file.
        //
        // `org/keycloak/models/credential/` (KC-CRED.LAZY) needed an explicit
        // carve-out from this ban to stay JIT-eligible; with the ban gone the
        // carve-out is moot. Those getters were separately verified
        // JIT-compiled and correct when the `is_known_miscompile` block was
        // removed (2026-07-27, docs/internal/is-known-miscompile-block-retired-20260727.md).

        // KC26-RX.1 — LIFTED 2026-07-27, together with KC26-PIC.1 above and
        // for the same reason. The ban kept `io/reactivex/rxjava3/`
        // interpreted because `RealmModelTest` stalled in
        // `BlockingFlowableIterable$BlockingFlowableIterator.hasNext` while
        // Infinispan drained an RxJava-backed distributed stream. Measured on
        // the real Keycloak 26.6.1 server boot (which initialises the
        // Infinispan session providers): `CRATONVM_JIT_ALLOW_PACKAGES=io/reactivex/`
        // alone reached the end-of-startup marker in 171s versus a 126s-198s
        // ban-in-place baseline — no stall, inside the noise. See the
        // KC26-PIC.1 comment above for the full measurement table and for what
        // was NOT re-tested (`RealmModelTest` itself; no compiled Keycloak test
        // classes on this build host). Restorable ad-hoc with
        // `CRATONVM_JIT_DENY=io/reactivex/`.

        // RBC.1 (Session 109) — provisional blanket ban for the
        // BouncyCastle algorithm-registration cascade. BC's
        // `BouncyCastleProvider.<init>` registers ~thousand algorithm
        // mappings in <1s; many of those mapping classes contain hot
        // helper methods (constants generators, key-spec builders) that
        // the JIT promotes after a single warm pass. The miscompile is
        // the same allocate-then-putfield pattern that bites
        // `Integer.valueOf` / `String.toLowerCase`, but applied to BC
        // helper objects instead of JDK ones — and on Windows the
        // resulting bad pointer manifests as STATUS_ACCESS_VIOLATION
        // (rc=139) inside the next consumer's HashMap probe.
        //
        // This is a coarse-grained safety net: it costs throughput on
        // every BC client, but it is the only available mechanism that
        // gives BcProbe a green path to the first println without a
        // proper Windows-debugger backtrace of the failing JIT codegen.
        // Lifted by `CRATONVM_JIT_ALLOW_PACKAGES=org/bouncycastle/`.
        //
        // ── Round 2 (2026-06-03) status ──────────────────────────────────
        // The ban is INTENTIONALLY RETAINED. A round-2 codegen agent
        // reproduced the EC crash (`org/bouncycastle/math/ec/test/AllTests`
        // under `CRATONVM_JIT_ALLOW_PACKAGES=org/bouncycastle/`) and
        // confirmed it is NOT the inline-allocation / allocate-then-putfield
        // family that round-1 + round-2 fixed (the `bintrees18` inline-TLAB
        // header-coherence bug in `jit/src/x64.rs::emit_inline_tlab_new` is
        // now fixed; disabling inline-new via
        // `CRATONVM_JIT_DISABLE_INLINE_NEW=1` does NOT change the EC crash).
        //
        // The EC crash is the *upstream value-production* miscompile
        // documented in `docs/bc-math-ec-jit-miscompile-investigation.md`:
        // a primitive `1` (or `3`) lands in a slot that a downstream inline
        // `getfield`/`arraylength` consumes as an object/array base
        // (`read at 0x31 == 1 + 0x30`; with `DISABLE_INLINE_GETFIELD=1` the
        // fault MOVES to `read at 0x0F == 3 + 0xC`, proving the dereference
        // site is only a witness — the bad value is produced earlier). It
        // requires many BC packages JIT-compiled together (a cross-package
        // JIT→JIT dispatch / operand-slot-reuse interaction) and was not
        // isolatable by single-package bisection. The round-1 dup/swap
        // oop-mark, escape-analysis per-block barrier, and dup2 cat-2 guard
        // fixes are sound but do not cover it; root-causing the exact
        // producing basic block needs an iterative build+bisect loop that
        // was out of scope for this round. Do NOT lift until that producer
        // is found and fixed, or until the EC `AllTests` run completes
        // cleanly under the allow-packages override.

        // SUNEC-INTPOLY -- REMOVED 2026-07-26, BUT SEE THE WARNING BELOW.
        // Re-verified with a standalone probe (EcIntPolyProbe.java, pure
        // JDK java.security -- KeyPairGenerator/Signature "EC", no
        // external jar) driving a real repeated keygen+sign+verify mix on
        // P-384 and P-521 (the exact curves and operation shape the
        // original bug report named): 60 baseline + 60 package-explicitly-
        // allowed P-384 runs, 60 more P-521 runs -- 0 signature/verification
        // failures in any configuration.
        //
        // *** IMPORTANT, DO NOT REMOVE THIS WARNING WITHOUT RE-TESTING ***
        // This ban's own original root-cause analysis says the underlying
        // JIT→JIT arg-marshalling miscompile needs BOTH `intpoly` AND
        // `java/math/BigInteger` JIT-compiled TOGETHER to trigger --
        // `BigInteger`/`MutableBigInteger` are separately, unconditionally
        // banned elsewhere in this file (HIB-BIGINTEGER-AIOOBE.1/.2, no
        // `package_allowed` escape hatch at all -- see the `class_name ==
        // "java/math/BigInteger"` / "java/math/MutableBigInteger"` checks
        // above), so the callee side of that JIT→JIT interaction currently
        // can never happen regardless of this ban. This removal is
        // therefore "safe under the current, still-active BigInteger ban"
        // -- NOT an independent fix of the JIT→JIT arg-marshalling bug
        // itself. If HIB-BIGINTEGER-AIOOBE.1/.2 is EVER lifted in a future
        // session, this exact SUNEC-INTPOLY scenario (P-384/P-521 EC
        // keygen+sign+verify) MUST be re-tested together with BigInteger
        // JIT-compiled before assuming EC crypto is still safe -- do not
        // treat the two bans as independent. EcIntPolyProbe.java is the
        // regression witness for the currently-tested (BigInteger-still-
        // banned) configuration only.

        // HIB-BYTEBUDDY -- REMOVED FOR GOOD 2026-07-30. The initial 2026-07-28
        // removal used only a 15-class Hibernate sample. A later no-ban run
        // from the older 77389fa06 runtime crashed in 302 classes; its exact
        // witness was an instruction-fetch fault in the middle of
        // `ModifierReviewable$AbstractBase.matchesMask`.
        //
        // An instruction-fetch fault at a valid mid-body address is not the
        // shape a bytecode miscompile takes -- a miscompile yields a wrong
        // value or a data fault. It is the signature of a compiled body being
        // unmapped while a live frame still executes it, and that runtime
        // predated the three JIT code-lifetime fixes now on dev (3fe14734a,
        // ac300e6f6, 463bd32e2). ByteBuddy is merely the most JIT-churn-heavy
        // code in the suite -- it retires and republishes artifacts constantly
        // -- so it is where that defect surfaced first, and the 2026-07-29 ban
        // re-instatement hid it rather than fixing it. Current dev, with no
        // `net/bytebuddy/` guard, passes the exact 302-class crash manifest in
        // both JIT and --nojit modes. Full root-cause and marker accounting:
        // `docs/internal/fixed-suite-bugs/hibernate/`
        // `hib-bytebuddy-20260730-FIXED.md`.
        // TEST-HARNESS BLANKET BANS -- REMOVED 2026-07-27. Four blanket
        // package bans lived here together:
        //
        //     com/carrotsearch/randomizedtesting/
        //     org/apache/logging/log4j/
        //     org/junit/
        //     junit/
        //
        // Unlike every documented ban around them, none carried a rationale
        // comment. `git log -S` on each of the four literals returns the SAME
        // single commit, `60ef90d4b` (2026-07-05, "Fix Elasticsearch postings
        // FFM checksum bridges") -- a large, generically-named squash touching
        // 14 files. randomizedtesting/log4j/junit are all central to
        // Elasticsearch's own test framework, so this was one incidental
        // defensive group added to keep ES's harness fully interpreted during
        // that historical investigation, never individually justified.
        //
        // Consequence while they lived here: they silently SHADOWED every
        // narrower ban on a class under those packages, so re-testing such a
        // ban without also setting `CRATONVM_JIT_ALLOW_PACKAGES` produced a
        // false-clean result -- the probe ran, reported success, and the target
        // method was never JIT-compiled at all. That trap caught two removals
        // in the 2026-07-26 sweep (TOMCAT-DOHEAD-JUNIT-ITERATOR.1, JUNIT.1).
        //
        // Evidence for the removal (full writeup, including the exact seeded
        // class lists and how to regenerate them:
        // `docs/internal/blanket-org-junit-ban-undocumented-shadow-20260726.md`).
        // Every comparison below is baseline-vs-lifted on ONE binary, same
        // seeded class list, results normalised to drop timings:
        //
        //   - Elasticsearch (the group's own likely origin, and the leg the
        //     2026-07-26 session could not complete): 60-class seeded sample from the real
        //     `es-fixture-ivfknn-slicesdense-closure-20260717` corpus (2571
        //     compiled test classes) -- baseline vs lifted BYTE-FOR-BYTE
        //     IDENTICAL, 60/60, including every pre-existing failure.
        //   - Hibernate ORM 8.0: 160-class seeded sample (2x the 2026-07-26
        //     sample) through hib-suite-runner's JUnit5 Platform Launcher --
        //     baseline vs lifted BYTE-FOR-BYTE IDENTICAL.
        //   - Spring Boot core: 40-class seeded sample of `core/spring-boot` --
        //     baseline vs lifted BYTE-FOR-BYTE IDENTICAL, 40/40.
        //
        // The ES and Spring Boot runs each included a second, decisive pair:
        // `CRATONVM_JIT_BISECT_ONLY=<these four packages>` + `THRESHOLD=1`,
        // once WITHOUT and once WITH the packages allowed. The control
        // JIT-compiles literally nothing (the four packages are the only
        // JIT-eligible ones and they were still banned); the test compiles
        // ONLY these four packages, on the first invocation of every method.
        // Any difference between that pair is attributable to JIT-compiling
        // exactly the code these bans covered -- so this is not another
        // shadowed no-op. Spring Boot: 40/40 identical. ES: 30/30 identical
        // but for `NodeConnectionsServiceTests`' failure COUNT (2 vs 1), which
        // 8 repeat runs in the CONTROL config alone showed to be flaky in
        // itself (2,2,2,2,1,2,1,1 with the config held constant).
        //
        // Two narrower bans under these prefixes were themselves shadowed and
        // are now the live gates; both keep their own documented evidence and
        // are deliberately NOT removed here:
        //   - PIC.1, `org/junit/platform/console/shadow/picocli/` (further
        //     down in this same Conservative block).
        //   - `("junit/textui/TestRunner", "main")` in `is_known_miscompile`.
        //     That list is gated behind `callee_saved_gpr_local_homes_enabled()`
        //     (default-OFF), so `TestRunner.main` becomes JIT-eligible by
        //     default for the first time with this removal. Probed directly --
        //     see `JUnit3TextUiRunnerProbe.java`.
        //
        // Note that `CRATONVM_JIT_ALLOW_PACKAGES=org/junit/` ALSO lifts PIC.1
        // (`package_allowed` prefix-matches), so the lifted legs above were a
        // strict superset of this removal: the shipping default still has
        // PIC.1 active and is therefore no less conservative than what was
        // measured.

        // SPB.1 (Session 112) — REMOVED 2026-07-26. This was a provisional
        // blanket ban for `org/springframework/util/`, on the theory that
        // `ClassUtils.<clinit>`'s `registerCommonClasses(...)` (~100
        // `HashMap.put` calls into a fresh map) triggered a JIT
        // allocate-then-putfield miscompile. Re-investigated 2026-07-26
        // (`docs/known-issues/spb1-springframework-util-investigation.md`):
        // three standalone repros against real `spring-core-7.0.7.jar`
        // found no JIT-specific corruption, but a fourth (GC-pressure +
        // `URLClassLoader` churn) reproduced real heap corruption with the
        // ban ACTIVE too — ruling out "this ban prevents the regression".
        // Root-caused as a GC-root-scanning gap, not a JIT bug: three
        // `vm/src/memory/roots.rs` sections (static fields, class-lock
        // objects, CONSTANT_Dynamic roots) and one `class_mirrors` section
        // deferred a user-defined-loader class's still-YOUNG-generation
        // object to the `metadata_pin` side-channel instead of rooting it
        // directly; that channel is consulted only by the Generational
        // backend's OLD-GEN mark BFS, so a young object with no other root
        // (the overwhelmingly common case for a static field's value right
        // after `<clinit>` assigns it) was silently reclaimed and its
        // memory reused by the class's own next allocation. Fixed via
        // `VmHeap::metadata_pin_deferrable` (`gc/src/vm_heap.rs`), which
        // gates the defer on the object actually being in old gen.
        // Verified: 50/50 clean runs of the doc's repro-3 with the ban kept
        // and 47/47 clean with it lifted (`CRATONVM_JIT_ALLOW_PACKAGES=
        // org/springframework/util/`), across 30-round `URLClassLoader`
        // churn under concurrent GC pressure — no distinct JIT-specific
        // symptom appears once lifted, so the ban is removed rather than
        // just left liftable.

        // SPB.2 -- REMOVED 2026-07-26. Originally (Session 112 r8):
        // `SerializableTypeWrapper.forTypeProvider` HUNG (never returned)
        // after the lambda body `lambda$forGenericInterfaces$<hash>$1`
        // was JIT-dispatched, reached via `Class.getGenericInterfaces()`
        // promoted after the heavy `ConcurrentReferenceHashMap` segment
        // initialisation in `SerializableTypeWrapper.<clinit>` (16
        // segments x 10 maps = 160 segment ctor entries) -- the same
        // allocate-then-putfield-vs-OSR pattern as Integer.valueOf/
        // String.toLowerCase. Re-verified with a standalone probe
        // (`SerializableTypeWrapperProbe.java`, real spring-core-7.0.7.jar,
        // package-private-access trick via same-package placement)
        // driving the real, public `SerializableTypeWrapper.forField(Field)`
        // entry point on a field whose declaring class implements 3 real
        // generic interfaces (exercising `getGenericInterfaces()` with
        // genuine multi-element work), then forcing resolution of the
        // lazy wrapped `Type` via `toString()`/`equals()`/`hashCode()`/
        // `unwrap()` -- exactly the lambda dispatch path the ban
        // describes -- 5000 times: baseline, package-allowed, and a
        // `CRATONVM_JIT_THRESHOLD=1` aggressive-compilation pass, each
        // run under an external `timeout 60` wrapper given the original
        // symptom was a hang rather than a crash -- 0 hangs, 0 failures,
        // consistent resolution every call in every configuration. No
        // longer reproduces on current dev.
        // `SerializableTypeWrapperProbe.java` is the regression witness.

        // SPB.4 / SPB.4b / SPB.4c -- REMOVED 2026-07-27. All three were
        // provisional blanket bans (Session 113 r1) for
        // org/springframework/boot/context/properties/bind/,
        // org/springframework/boot/context/, and the org/springframework/boot/
        // umbrella (excl. boot/loader/, which SPB.9b covers separately and is
        // untouched by this removal), originally added against a SIGSEGV seen
        // in the ms-course-youtube admin-service frame trace -- a fixture app
        // never present on this host and never independently reproduced here.
        //
        // Re-verified against a real, previously-established fixture:
        // `/data/data/spring-boot-tomcat-crossmodule-20260717/cratonvm-suite`,
        // a 10-scenario Spring Boot functional battery (real
        // spring-boot-4.0.6.jar + spring-context/beans/core/aop/expression-
        // 7.0.7.jar) that exercises SpringApplication boot, property binding,
        // environment/profile resolution, conditionals, events, AOP, and
        // resource loading -- i.e. the exact org/springframework/boot/* boot
        // path these bans target. Ran baseline (bans active) and with
        // `CRATONVM_JIT_ALLOW_PACKAGES=org/springframework/boot/` (a superset
        // that also lifts the narrower SPB.4/.4b prefixes): both configs
        // produced byte-identical per-scenario pass/fail counts across all 10
        // scenarios (S01-S10), no new crash, no new hang, no SIGSEGV. (One
        // pre-existing scenario, S03_ConfigProxy, fails identically 4/8 in
        // both configs -- a real CGLIB @Configuration proxy singleton-identity
        // regression, unrelated to these bans and tracked separately, see
        // docs/known-issues/springboot/configproxy-cglib-singleton-regression-20260727.md.)
        // A further `CRATONVM_JIT_THRESHOLD=1` aggressive pass with the ban
        // lifted surfaced a real `ClassCastException` in
        // `org/springframework/boot/context/config/Profiles.<clinit>`
        // (array-vs-element-type confusion reading a `ResolvableType[]`) --
        // but this reproduces IDENTICALLY with the ban still active (not
        // gated by it at all; the affected class calls into
        // org/springframework/core/ResolvableType, already unbanned since
        // SPB.2's own removal), so it is not evidence for keeping SPB.4/.4b/.4c
        // and is tracked as its own new finding. FIXED 2026-07-28 (an inline
        // cache guarded a virtual call site by class id alone, so an ARRAY
        // receiver whose header carries its COMPONENT class id was dispatched
        // into the component's method body) — see
        // docs/internal/resolvabletype-array-receiver-mic-guard-fixed-20260728.md.
        //
        // No longer reproduces on current dev at real-world JIT thresholds.

        // SPB.5 (Session 113 r1) — provisional blanket ban for Spring
        // Cloud. ms-course-youtube `admin-service` is a Spring Cloud
        // Eureka client; once the property binder is unblocked (SPB.4),
        // the next downstream consumers are Spring Cloud's
        // `BootstrapApplicationListener`, `ConfigDataLocationResolver`,
        // and `EnvironmentChangeEvent` plumbing — all of which exhibit
        // the same allocate-then-putfield idiom on `ConcurrentHashMap`
        // / `LinkedHashMap` containers. The ban is pre-emptive: it
        // costs throughput on every Spring Cloud boot path but avoids
        // a second iteration if the next downstream gap surfaces there.
        // Lifted by `CRATONVM_JIT_ALLOW_PACKAGES=org/springframework/cloud/`.
        // SPB.5's org/springframework/cloud/ guard (Spring CLOUD, not plain

        // ANTLR.1 -- REMOVED 2026-07-26. Reason 1 (correctness -- the
        // PredictionContext equality/hash miscompile) was already
        // independently, unconditionally covered by
        // is_antlr_prediction_context_miscompile (see ANTLR-COLDPATH.1
        // below), which stays interpreted regardless of this broader
        // package ban or CRATONVM_JIT_ALLOW_PACKAGES. Reason 2 (throughput
        // -- ~8x slower cold parse under JIT) was re-tested post the
        // 2026-07-26 JIT rework with a real groovy-3.0.21.jar
        // (GroovyAntlrThroughputProbe.java, found this session at
        // /data/tmp/groovysrc + /data/tmp/groovy-antlr4-probe on this
        // host -- an earlier session's claim that "the shaded package
        // doesn't exist anywhere on this host" was simply wrong, it was
        // never searched for outside .gradle/.m2): 15 cold
        // GroovyShell.evaluate() calls, fresh script + fresh class per
        // iteration. Baseline (ban active): avg 840.6ms/iter (steady-state
        // ~390-480ms after the first classloading-heavy call). Lifted
        // (groovyjarjarantlr4/ JIT-compiled, narrow PredictionContext guard
        // still active): avg 866.1ms/iter, steady-state ~370-460ms --
        // statistically indistinguishable from baseline, not an 8x
        // regression. The throughput justification no longer holds; 0
        // parse failures in either config. GroovyAntlrThroughputProbe.java
        // is the regression witness.

        // HIB-ANTLR.1 -- REMOVED 2026-07-26. Re-verified with the real
        // Hibernate ORM 8.0 test harness (hibernate-orm-harness on this
        // host, hibernate-core testClasses/testRuntimeClasspath + a real
        // H2 in-memory DB via the JUnit5 Platform CratonRunner driver):
        // org.hibernate.orm.test.hql.ASTParserLoadingTest (106 methods,
        // heavy real HQL-via-ANTLR parsing), .hql.HQLInsertAndUpdateTest
        // (5 methods), and .type.temporal.InstantTests (204 methods) all
        // ran clean (0 failures, identical to baseline) with
        // org/antlr/v4/runtime/ JIT-allowed and org/hibernate/ still banned at
        // the time -- this ban's own specific claim (ATNState.transitions
        // corrupted between two separate HQL parses) does not reproduce.
        // IMPORTANT: org/antlr/v4/runtime/ is ALSO, separately, covered by
        // HIB-LONGTAIL.1 above (same prefix, already confirmed still
        // needed via a real 218-class H2 suite run finding a
        // "Schema not found" DB-reconnect corruption -- a different,
        // reconnect-specific trigger this HQL-parsing test batch does not
        // exercise). At the time this check was deleted it was therefore a
        // redundant/shadowed removal, not an independent unban -- default
        // (Conservative) behavior for org/antlr/v4/runtime/ classes was
        // UNCHANGED, they stayed interpreted via HIB-LONGTAIL.1 regardless.
        //
        // 2026-07-27 FOLLOW-UP: that shadow is gone. HIB-LONGTAIL.1 was
        // narrowed to `org/h2/` only (see its own comment above), on a
        // same-binary A/B over all 57 org.hibernate.orm.test.hql classes
        // plus a ban-removed rebuild. org/antlr/v4/runtime/ is now genuinely
        // JIT-eligible under Conservative; only the narrow
        // is_antlr_prediction_context_miscompile guard (ANTLR-COLDPATH.1)
        // still forces specific PredictionContext methods to the
        // interpreter, in both the shaded and unshaded runtimes.

        // SPB.6 -- REMOVED 2026-07-27, UNVERIFIED. Was a provisional
        // blanket ban (Session 113 r1) for the Netflix Eureka discovery
        // client (`com/netflix/discovery/DiscoveryClient.<init>`
        // allocate-then-putfield on `InstanceInfo`/`ApplicationInfoManager`,
        // plus its `ScheduledExecutorService` task-submit path already
        // covered by EXEC.1). No fixture ever existed to test this against
        // on this host: exhaustively searched twice (this session and a
        // concurrent one) for the original `eureka-server` app or any
        // `eureka-client`/`eureka-core` jar (Maven/Gradle cache, vendored,
        // source checkout) -- zero matches. Removed by explicit user
        // decision (accepting the risk of the original, never-reproduced-
        // here SIGSEGV/corruption) rather than re-verified evidence. If a
        // real Eureka client crash resurfaces, re-add this ban and treat
        // it as confirmed-needed, not provisional.

        // SPB.7 (Session 113 r1) — provisional blanket ban for Feign /
        // OpenFeign HTTP client allocations. Spring Cloud OpenFeign
        // builds a `feign.Feign$Builder` that allocates per-method
        // `MethodMetadata` and `RequestTemplate` objects, each storing
        // `template` / `headers` / `body` slots immediately after `new`.
        // Lifted by `CRATONVM_JIT_ALLOW_PACKAGES=feign/`.

        // SPB.8 (Session 113 r2) — provisional blanket ban for the JBoss
        // Modules class-graph and resource loading code paths.
        // `apps/wildfly-39.0.1.Final` boot SIGSEGVs (rc=139) right after the
        // BigInteger ZERO/ONE/TWO post-clinit fixup and the two upstream
        // `<clinit>` swallows (`SimpleLoggerContext`, `ConcurrentClassLoader`)
        // handled by parallel agents. With `CRATONVM_DISABLE_JIT=1` the
        // SIGSEGV is replaced by a clean `NoSuchMethodError` for
        // `Object.loadClass(...)` followed by an orderly `System.exit(1)`
        // — i.e. boot proceeds far past the JIT-on crash point. This
        // confirms a JIT miscompile, not a native gap.
        //
        // The `CRATONVM_DBG_JIT_DISPATCH=1` capture shows the very last
        // dispatched method before the SIGSEGV is
        // `java/lang/Long.parseLong(Ljava/lang/String;I)J` invoked with a
        // corrupted reference arg0 (`0xfffd_026d_7b3e_5570` — the high
        // `0xfffd` half is a tag-bit corruption signature, not a valid heap
        // pointer). The upstream traffic is JBoss Modules's
        // `PropertyReadAction.run` / `Module$1.run` lambdas iterating module
        // descriptors, plus the JBoss AS `PluggableMBeanServerImpl$
        // TcclMBeanServer$4.run` thread-context-classloader doPrivileged
        // chain. Both are allocate-then-putfield-heavy: `Module.<init>`
        // stores `name`/`mainClass`/`fallbackLoader` slots, and the
        // class-graph traversal walks `LocalLoader` / `PathFilter` chains
        // that allocate a fresh `ResourceLoaderSpec` / `Resource` per visit.
        // This matches the W2-CHM / RBC.1 / SPB.1-7 archetype.
        //
        // Lifted by `CRATONVM_JIT_ALLOW_PACKAGES=org/jboss/modules/`. The
        // companion `org/jboss/as/` ban below covers the WildFly server
        // boot path that consumes the module graph.
        // SPB.8's org/jboss/modules/ guard is active.
        if class_name.starts_with("org/jboss/modules/")
            && !package_allowed("org/jboss/modules/", allow_packages)
        {
            return Some(SkipReason::RustJvmTestFixture);
        }

        // SPB.8b — `org/jboss/as/` blanket ban REMOVED 2026-07-27.
        //
        // Added Session 113 r2 as a pre-emptive companion to SPB.8 (never
        // driven by a failure of its own), then re-confirmed on 2026-07-26 by
        // a real `standalone.sh` boot that died on
        // `NullPointerException: ... "this.validTypes" is null` in
        // `ModelTypeValidator` with the ban lifted and JIT on, and booted with
        // the ban lifted and `--nojit` — a differential that read as a JIT
        // miscompile. It was not one. The NPE was a *consequence* of the boot
        // being torn down mid-flight: `Runtime.addShutdownHook` was a no-op
        // stub, so WildFly's `BootstrapImpl$ShutdownHook` — which holds the MSC
        // `ServiceContainer` — became unreachable the moment `Main.main`
        // returned, MSC 1.5's leak-detector `Cleaner` called
        // `container.shutdown()`, and `AbstractControllerService.stop` reset
        // `controller` to null under the still-running boot thread. WHEN that
        // collection happened depended on GC timing, which the JIT changes;
        // hence the clean-looking `--nojit` differential.
        //
        // With that root fixed (plus the `HashSet.addAll(<foreign
        // open-addressed set>)` null-hole fix that unmasked the boot's own
        // failure reporting), WildFly 32.0.1.Final boots to `WFLYSRV0026` with
        // this package JIT-compiled, at the same rate as with the ban in place:
        // 4/6 vs 5/6 over a 6+6 A/B on the Azure host, and every failure in
        // BOTH arms is the same pre-existing flaky
        // `NoSuchMethodError: java/lang/Object.hasNext()Z` from
        // `docs/internal/fixed-suite-bugs/wildfly/wildfly-interpreter-operand-stack-slot-stale-after-nested-alloc-FIXED.md`.
        // The full write-up (root causes, the A/B, and the per-package results
        // for the four sibling bans, which stay in place) is the retired
        // `modeltypevalidator-validtypes-npe` doc — archived with the rest of
        // docs/internal. The residual it hands off to is
        // `docs/internal/fixed-suite-bugs/wildfly/wildfly-interpreter-operand-stack-slot-stale-after-nested-alloc-FIXED.md`.


        // SPB.9 -- `org/slf4j/` and `ch/qos/logback/` REMOVED 2026-07-26.
        // Originally (Session 114) banned as a blanket trio (with
        // `org/apache/commons/logging/`) after `apps/insurance-backend`'s
        // Spring Boot 3.2 boot crashed with `expected object reference, got
        // int(1)` inside `SpringApplication.prepareEnvironment` ->
        // `SystemEnvironmentPropertyMapper.processElementValue` -- a
        // correlation (tight LoggerFactory/Slf4jLog init loop just before
        // the crash), not a proven miscompile inside the logging classes
        // themselves. Re-verified with a standalone probe
        // (`SlfLoggingProbe.java`, real jcl-over-slf4j + slf4j-api +
        // logback-classic/core jars): 0 failures across baseline,
        // package-allowed, and aggressive-compilation passes. These two
        // packages ARE genuinely safe -- see the UNCONDITIONAL
        // `org/apache/commons/logging/` re-ban further up this function
        // (near the other severity-based unconditional entries, e.g.
        // `TYPES-ERASURE.1`) for why that THIRD package from the original
        // trio needed to come back after a same-day real regression
        // (`LoggingApplicationListenerTests` `FilterReply` corruption) that
        // this narrower probe did not exercise (it used `jcl-over-slf4j`,
        // API-compatible but bytecode-different from the real Commons
        // Logging 1.3.x jar Spring Boot's real test classpath uses).
        //
        // IMPORTANT: this removal does NOT independently confirm the
        // original `insurance-backend` crash is fixed -- its actual trigger
        // site (`SystemEnvironmentPropertyMapper.processElementValue`, in
        // `org/springframework/boot/context/properties/bind/`) is a
        // SEPARATE, still-active, already-documented ban (see the
        // `org/springframework/boot/context/properties/bind/` guard
        // above) that this removal does not touch.

        // CGL.1 (Session 117 — agent O4, 2026-05-16) — provisional blanket ban
        // for CGLIB (`net/sf/cglib/`). `apps/cglib_probe` is a minimal repro
        // (`Enhancer.create()` over `Greeter` with a single `MethodInterceptor`
        // lambda). After N3's lambda-subclass fix in `is_subclass`, the
        // "Unknown callback type" error is gone, but the run now SEGFAULTs
        // (rc=139) immediately after `<clinit>` of the generated proxy class
        // (`CglibProbe$Greeter$$EnhancerByCGLIB$$<hash>`). With
        // `CRATONVM_DISABLE_JIT=1` the SEGFAULT disappears and the run
        // surfaces a clean `IllegalStateException` (a separate downstream
        // gap in proxy-class wiring, not a JIT issue) — i.e. classic
        // JIT-miscompile signature.
        //
        // CGLIB's hot boot path is a textbook allocate-then-putfield
        // workload: `AbstractClassGenerator.create` runs through
        // `KeyFactory.Generator` -> `CodeEmitter.emit*` ->
        // `DebuggingClassWriter.toByteArray` ->
        // `ReflectUtils.defineClass` (which calls `Unsafe.defineClass`
        // /`MethodHandles.Lookup.defineClass` with raw `byte[]`/Long
        // pointers), allocating thousands of `MethodInfo`,
        // `MethodWrapper`, `Type`, `Signature`, `MethodInfoTransformer`
        // entries and storing their slots immediately after `new`. The
        // generated proxy `<clinit>` then runs a long sequence of
        // `Class.forName` -> `ReflectUtils.findMethods` ->
        // `MethodProxy.create` calls that store `Method`/`MethodProxy`
        // references into the proxy's static slots — exactly the W2-CHM
        // archetype that bites RBC.1 / SPB.1-9. Same package model:
        // every method on the path is JIT-promotable but the runtime
        // miscompile poisons one of the stored references.
        //
        // Coarse-grained safety net so cglib_probe can progress past the
        // SEGV. Lifted by `CRATONVM_JIT_ALLOW_PACKAGES=net/sf/cglib/`.
        // Track for a real fix once the underlying allocate-then-putfield
        // miscompile is root-caused.
        // CGL.1's net/sf/cglib/ guard (the standalone, unshaded CGLIB
        // artifact -- Spring's OWN internal copy is org/springframework/cglib/

        // SPB-FLYWAY-HSQLDB.1 -- REMOVED 2026-07-26. Re-verified with a
        // standalone probe (FlywayHsqldbProbe.java, real hsqldb-2.7.4.jar --
        // Flyway 12.4.0 dropped built-in HSQLDB support with no plugin jar
        // available on this host, so this drives HSQLDB's own "dense
        // add/update path" directly via JDBC instead: DDL + 200 inserts +
        // 100 updates per run) across 80 independent in-memory databases
        // (baseline + package explicitly allowed), plus a 40-run
        // CRATONVM_JIT_THRESHOLD=1 aggressive-compilation pass: 0 failures
        // in every configuration. No longer reproduces on current dev.
        // FlywayHsqldbProbe.java is the regression witness.
        // SPB.9b -- ALL THREE sub-bans now removed (companion blanket ban,
        // Session 114, for the Spring Boot loader + reactive web context,
        // plus the Spring Beans factory support layer).
        //
        // `org/springframework/beans/factory/support/` was REMOVED
        // 2026-07-26 with real re-verification evidence: a standalone
        // probe (`BeanFactorySupportProbe.java`, real spring-beans-7.0.7.jar
        // + spring-core-7.0.7.jar + commons-logging-1.2.jar) registering 60
        // real `RootBeanDefinition` instances per iteration (matching the
        // ban's own "~50+ beans" scale) via a real
        // `DefaultListableBeanFactory`, 500 iterations: baseline,
        // package-allowed, and a `CRATONVM_JIT_THRESHOLD=1`
        // aggressive-compilation pass -- 0 failures, correct bean counts
        // and field reads every call in every configuration. No longer
        // reproduces on current dev. `BeanFactorySupportProbe.java` is the
        // regression witness for this one sub-package.
        //
        // `org/springframework/boot/loader/` (JarLauncher /
        // LaunchedURLClassLoader) and `org/springframework/web/reactive/` +
        // `org/springframework/boot/web/reactive/` (WebFlux reactive
        // handler chains) were REMOVED 2026-07-27, UNVERIFIED. No fixture
        // ever existed on this host to test these against: they need the
        // full `insurance-backend` app's JarLauncher/WebFlux-boot scaffold
        // (a real executable fat jar + a real reactive web server boot),
        // and that named app was exhaustively searched for at full
        // filesystem depth (by content/purpose, not just name) twice --
        // this session and a concurrent one -- with zero matches. Removed
        // by explicit user decision (accepting the risk of the original,
        // never-reproduced-here SIGSEGV in `JarLauncher.launch`) rather
        // than re-verified evidence. If a real Spring Boot fat-jar boot or
        // WebFlux app crash resurfaces on either of these packages, re-add
        // the ban and treat it as confirmed-needed, not provisional.

        // SPB.9c -- REMOVED 2026-07-26. Originally (Session 114):
        // companion blanket bans for the Spring component-scan critical
        // path (org/springframework/context/annotation/,
        // org/springframework/context/support/,
        // org/springframework/core/io/support/, plus the
        // org/springframework/beans/factory/ check just below this
        // comment) -- with SPB.9/SPB.9b in place, insurance-backend boot
        // reached `SpringApplication.run` -> `...refresh` ->
        // `ConfigurationClassPostProcessor.processConfigBeanDefinitions`
        // -> `ConfigurationClassParser.parse` ->
        // `ClassPathBeanDefinitionScanner.doScan` ->
        // `...scanCandidateComponents` ->
        // `PathMatchingResourcePatternResolver.getResources`, each doing
        // putfield-heavy allocations (SourceClass.<init>, Resource[] via
        // aastore) -- same W2-CHM/RBC.1/SPB.1-9 archetype. Re-verified
        // with a standalone probe (`ComponentScanProbe.java`, real
        // spring-beans/spring-core/spring-context-7.0.7.jar +
        // commons-logging-1.2.jar) driving a real
        // `ClassPathBeanDefinitionScanner.scan(...)` against a real
        // package containing 5 real `@Component`/`@Service`/
        // `@Repository`/`@Configuration` classes -- the exact
        // doScan -> scanCandidateComponents -> getResources chain this
        // ban describes -- 500 times: baseline, package-allowed (all 4
        // packages together), and a `CRATONVM_JIT_THRESHOLD=1`
        // aggressive-compilation pass -- 0 failures, consistent
        // scanned/registered bean counts every call in every
        // configuration. No longer reproduces on current dev.
        // `ComponentScanProbe.java` is the regression witness.
        //
        // UPDATE (same day): `org/springframework/core/io/support/` was
        // initially still shadowed by the separate SPB.2 ban on the
        // broader `org/springframework/core/` (this removal's own claim
        // was genuinely re-verified clean, but ComponentScanProbe doesn't
        // exercise SPB.2's own SerializableTypeWrapper code path). SPB.2
        // has SINCE been independently re-verified and removed too (see
        // its own removal comment a few hundred lines above), so this
        // sub-package is now unconditionally JIT-eligible with no
        // remaining shadow, same as the other three SPB.9c sub-bans.
        // SPB.9d -- REMOVED 2026-07-26. Originally (Session 117):
        // eureka-server boot SEGFAULTed deep in Spring's BeanInfo
        // introspection -- `java/beans/Introspector` delegates into
        // `com/sun/beans/introspect/MethodInfo`/`ClassInfo`, sorting
        // methods/properties via a comparator chain
        // (`MethodInfo$MethodOrder.compare` -> `String.compareTo` ->
        // `Method.getName`/`toString` -> `StringJoiner.<init>`) driven by
        // `java/util/Arrays.sort`; under JIT the comparator returned
        // inconsistent ordering, corrupting transient sort state and
        // SEGFAULTing in the downstream StringJoiner allocation -- the
        // same allocate-then-putfield archetype as NEW-1.3/SPB.1.
        // Re-verified with a standalone, pure-JDK probe (no external jar
        // needed -- `BeanIntrospectorProbe.java`) driving
        // `Introspector.getBeanInfo(Class)` (cache flushed every call to
        // force a real re-sort each time) over a 13-method/10-property
        // bean 20000 times, including exercising the exact
        // `MethodDescriptor.toString()` downstream path the crash trace
        // named: baseline, package-allowed, and a
        // `CRATONVM_JIT_THRESHOLD=1` aggressive-compilation pass -- 0
        // failures, 0 crashes, correct property/method counts every call
        // in every configuration. No longer reproduces on current dev.
        // `BeanIntrospectorProbe.java` is the regression witness.
        // org/springframework/beans/factory/ (excluding .../support/,
        // already removed separately above) -- REMOVED 2026-07-26 as the
        // 4th sub-ban of SPB.9c, see the SPB.9c removal comment above for
        // the full evidence (ComponentScanProbe.java's scan also
        // registers/reads bean definitions through this exact package).

        // PIC.1 (Session 118, 2026-05-25) — provisional blanket ban for the
        // shadowed picocli copy that JUnit Platform ships in its console
        // standalone jar. `junit-platform-console-standalone-1.10.2.jar
        // -- --help` SEGFAULTs (rc=139) right after the BigInteger
        // post-clinit fixup line and before any picocli help-banner output.
        // `CRATONVM_DBG_JIT_ENTRY=1` shows the last methods JIT-entered before
        // the crash are
        // `org/junit/platform/console/shadow/picocli/CommandLine$Model$OptionSpec.equals`,
        // `CommandLine$Assert.equals`, `CommandLine$Model$ArgSpec.equalsImpl`,
        // and `CommandLine$Model$CaseAwareLinkedMap.entrySet/values`. With
        // `CRATONVM_JIT_BISECT_ONLY=java/,sun/` (i.e. picocli not JIT-eligible)
        // the SEGFAULT vanishes and the run progresses to a clean
        // `BreakIteratorProviderImpl.getBreakInstance` NPE — a separate
        // downstream JDK-locale gap, not a JIT issue. Classic
        // JIT-miscompile signature.
        //
        // `OptionSpec.equals` is the canonical allocate-then-putfield
        // archetype that bites W2-CHM / RBC.1 / SPB.*. The hot path:
        //   - call `ArgSpec.equalsImpl` (a comparator chain of getfields),
        //   - allocate two fresh `java.util.HashSet`s wrapping `Arrays.asList`
        //     over the `names:[Ljava/lang/String;` field of each side,
        //   - call `HashSet.equals` to compare the two name sets.
        // Each `new HashSet(Collection)` ctor stores `table`/`size`/
        // `loadFactor` slots immediately after allocation; `Arrays.asList`
        // wraps the array via putfield into a fresh `Arrays$ArrayList`. The
        // bisect also showed `CommandLine$Assert.equals` (a static
        // `Object.equals(Object,Object)` helper with two null checks)
        // appearing on the crash path when allowed alone — picocli's
        // dispatch into the JIT-compiled `Assert.equals` PIC slot was
        // already documented as a Keycloak-26 SEGFAULT site (see
        // `x64.rs:3800` and the SPB.* cascade).
        //
        // Coarse-grained safety net so JUnit Platform's `--help` reaches at
        // least the same point the interpreter does. Lifted by
        // `CRATONVM_JIT_ALLOW_PACKAGES=org/junit/platform/console/shadow/picocli/`.
        // The unshaded picocli copy (`info/picocli/`) is rare in this
        // project and intentionally left JIT-eligible. Track for a real
        // fix once the underlying allocate-then-putfield / PIC dispatch
        // miscompile is root-caused.
        // PIC.1's org/junit/platform/console/shadow/picocli/ guard (JUnit
        // Platform's own console-standalone launcher tool, not any of the 5
    }

    None
}

/// T1.1.g — targeted list of (class, method) pairs known to miscompile
/// under the current JIT. Every other method — including the vast
/// majority of `java/util/*` and `cratonvm/*` methods — is now
/// JIT-eligible. Each entry here corresponds to a tracked NEW-1.3 or
/// NEW-1.4 follow-up.
fn is_unconditional_hash_miscompile_cluster(class_name: &str, method_name: &str) -> bool {
    matches!(
        (class_name, method_name),
        ("jdk/internal/util/ArraysSupport", "hashCode")
            | ("java/util/Arrays", "hashCode")
            | ("java/util/Objects", "hash")
            | ("java/util/Objects", "hashCode")
            | ("java/util/Objects", "equals")
    )
}

fn is_snakeyaml_emitter_emit_jit_corruption(class_name: &str, method_name: &str) -> bool {
    class_name == "org/yaml/snakeyaml/emitter/Emitter" && method_name == "emit"
}

/// Targeted list for `java.util.concurrent.locks`' queue-synchronizer family
/// — `AbstractQueuedSynchronizer` (classic, `int state`) and
/// `AbstractQueuedLongSynchronizer` (JDK 25+, `long state`; used by
/// `ReentrantReadWriteLock$Sync`), their respective `Node`/`ConditionObject`
/// inner classes, and `ReentrantReadWriteLock$Sync`/`HoldCounter`/
/// `ThreadLocalHoldCounter`.
///
/// UNCONDITIONAL — deliberately NOT gated behind the default-off switch the
/// rest of `is_known_miscompile` sat behind (that whole block was inert and
/// was removed 2026-07-27; this family is why it could not simply be
/// deleted wholesale back then). `a4913d8b` ("disable callee-saved GPR local
/// homes") gated the *entire* targeted list behind that flag on the premise
/// that every entry was the same callee-saved-GPR-local-home regalloc
/// family, now closed by making that register-allocation strategy
/// default-off. That premise does not hold for this family: with the
/// default (gate OFF, i.e. `is_known_miscompile` a no-op), a heavy-
/// contention repro reproduces a PERMANENT hang for both
/// `ReentrantReadWriteLock` (confirmed: two threads parked forever at an
/// identical bytecode offset 280+ seconds apart in
/// `AbstractQueuedLongSynchronizer.acquire`) and plain `ReentrantLock`
/// (confirmed: the classic, `int`-state class — proving this is not
/// specific to the newer long-state variant either). `CRATONVM_DBG_JITC`
/// tracing pinned the actually-compiled methods as `Node.getAndUnsetStatus`/
/// `clearStatus` and the synchronizer's own `tryInitializeHead` — none of
/// which were ever on the OLD targeted list at all (it only covered
/// `acquire`/`release`/`acquireShared`/`releaseShared`/`signalNext`/
/// `signalNextIfShared` and `ConditionObject`'s wait/signal methods, never
/// the `Node` class itself or the queue-initialization helpers). This
/// function supersedes and widens that historical list for this specific
/// family; see this file's `is_known_miscompile` removal record (in
/// `should_skip_jit_internal`) for the original CountDownLatch-driven
/// discovery's fate.
///
/// `tryInitializeHead` in particular allocates the CLH queue's sentinel
/// `ExclusiveNode` and immediately `casHead`s it in — an allocate-then-CAS
/// hazard structurally identical to the allocate-then-putfield family, but
/// running only once per lock instance (its first-ever contended acquire),
/// which is exactly why light-contention springboot (a handful of threads,
/// brief hold times) never trigger it while heavy-contention ones
/// (many threads, deep contention) reliably do — matching the observed gap
/// between an initial 4-thread stress repro (passed) and the real
/// Elasticsearch `LongRandomBinaryDocValuesRangeQueryTests` scenario and a
/// 16-thread synthetic repro (both hang identically).
fn is_known_miscompile_aqs_family(class_name: &str, method_name: &str) -> bool {
    matches!(
        (class_name, method_name),
        // --- AbstractQueuedSynchronizer (classic, int state) ---
        // H2 TestFileSystem.testConcurrent hang (2026-07-23): compareAndSetState/
        // getState/setState -- the raw Unsafe-CAS/volatile-accessor wrappers
        // around `state` -- were missing from this family despite being the
        // single hottest, most contended methods of the whole synchronizer
        // protocol (every acquire/release funnels through them). A live gdb
        // attach on a hung two-real-thread ReentrantReadWriteLock repro (H2's
        // TestFileSystem.testConcurrent against the `async:` filesystem)
        // caught one thread parked in `monitor_enter_synchronized_method`
        // waiting on a lock the other thread's `compareAndSetState` call
        // never visibly released, with no forward progress for 100s of
        // seconds under real CPU load -- the same "AbstractQueuedLongSynchronizer.
        // acquire" family hang this list already documents lower down, just
        // one level deeper (the CAS primitive `acquire` itself calls, not
        // `acquire`). See docs/known-issues/h2/
        // bug-h2-testfilesystem-testconcurrent-async-hang.md.
        ("java/util/concurrent/locks/AbstractQueuedSynchronizer", "compareAndSetState")
            | ("java/util/concurrent/locks/AbstractQueuedSynchronizer", "getState")
            | ("java/util/concurrent/locks/AbstractQueuedSynchronizer", "setState")
            | ("java/util/concurrent/locks/AbstractQueuedSynchronizer", "acquire")
            | ("java/util/concurrent/locks/AbstractQueuedSynchronizer", "release")
            | ("java/util/concurrent/locks/AbstractQueuedSynchronizer", "acquireShared")
            | ("java/util/concurrent/locks/AbstractQueuedSynchronizer", "releaseShared")
            | ("java/util/concurrent/locks/AbstractQueuedSynchronizer", "signalNext")
            | ("java/util/concurrent/locks/AbstractQueuedSynchronizer", "signalNextIfShared")
            | ("java/util/concurrent/locks/AbstractQueuedSynchronizer", "tryInitializeHead")
            | ("java/util/concurrent/locks/AbstractQueuedSynchronizer", "casTail")
            | ("java/util/concurrent/locks/AbstractQueuedSynchronizer", "enqueue")
            | ("java/util/concurrent/locks/AbstractQueuedSynchronizer", "isEnqueued")
            | ("java/util/concurrent/locks/AbstractQueuedSynchronizer", "reacquire")
            | ("java/util/concurrent/locks/AbstractQueuedSynchronizer", "acquireOnOOME")
            | ("java/util/concurrent/locks/AbstractQueuedSynchronizer", "cleanQueue")
            | ("java/util/concurrent/locks/AbstractQueuedSynchronizer", "cancelAcquire")
            | ("java/util/concurrent/locks/AbstractQueuedSynchronizer$Node", "casPrev")
            | ("java/util/concurrent/locks/AbstractQueuedSynchronizer$Node", "casNext")
            | ("java/util/concurrent/locks/AbstractQueuedSynchronizer$Node", "getAndUnsetStatus")
            | ("java/util/concurrent/locks/AbstractQueuedSynchronizer$Node", "setPrevRelaxed")
            | ("java/util/concurrent/locks/AbstractQueuedSynchronizer$Node", "setStatusRelaxed")
            | ("java/util/concurrent/locks/AbstractQueuedSynchronizer$Node", "clearStatus")
            | ("java/util/concurrent/locks/AbstractQueuedSynchronizer$ConditionObject", "signal")
            | ("java/util/concurrent/locks/AbstractQueuedSynchronizer$ConditionObject", "signalAll")
            | ("java/util/concurrent/locks/AbstractQueuedSynchronizer$ConditionObject", "doSignal")
            | ("java/util/concurrent/locks/AbstractQueuedSynchronizer$ConditionObject", "await")
            | ("java/util/concurrent/locks/AbstractQueuedSynchronizer$ConditionObject", "awaitNanos")
            | ("java/util/concurrent/locks/AbstractQueuedSynchronizer$ConditionObject", "awaitUntil")
            | (
                "java/util/concurrent/locks/AbstractQueuedSynchronizer$ConditionObject",
                "awaitUninterruptibly"
            )
            | (
                "java/util/concurrent/locks/AbstractQueuedSynchronizer$ConditionObject",
                "newConditionNode"
            )
            | ("java/util/concurrent/locks/AbstractQueuedSynchronizer$ConditionObject", "enableWait")
            // --- ReentrantLock ---
            // The outer lock/unlock methods and the Sync implementations feed
            // directly into the classic AQS queue.  They must be covered by
            // the same unconditional guard: the legacy outer-method entries
            // lived in the GPR-local-home-gated `is_known_miscompile` block
            // (inert, removed 2026-07-27), which left these allocate/CAS queue
            // paths JIT-eligible by default.
            | ("java/util/concurrent/locks/ReentrantLock", "lock")
            | ("java/util/concurrent/locks/ReentrantLock", "unlock")
            | ("java/util/concurrent/locks/ReentrantLock$Sync", "lock")
            | ("java/util/concurrent/locks/ReentrantLock$Sync", "nonfairTryAcquire")
            | ("java/util/concurrent/locks/ReentrantLock$Sync", "tryRelease")
            | ("java/util/concurrent/locks/ReentrantLock$NonfairSync", "initialTryLock")
            | ("java/util/concurrent/locks/ReentrantLock$NonfairSync", "tryAcquire")
            | ("java/util/concurrent/locks/ReentrantLock$FairSync", "initialTryLock")
            | ("java/util/concurrent/locks/ReentrantLock$FairSync", "tryAcquire")
            // --- AbstractQueuedLongSynchronizer (JDK 25+, long state) ---
            // See the matching compareAndSetState/getState/setState note on the
            // classic AbstractQueuedSynchronizer block above -- same gap, same
            // fix, same repro (ReentrantReadWriteLock$Sync extends this class on
            // JDK 25, so this is the copy that actually fired in the H2 hang).
            | ("java/util/concurrent/locks/AbstractQueuedLongSynchronizer", "compareAndSetState")
            | ("java/util/concurrent/locks/AbstractQueuedLongSynchronizer", "getState")
            | ("java/util/concurrent/locks/AbstractQueuedLongSynchronizer", "setState")
            | ("java/util/concurrent/locks/AbstractQueuedLongSynchronizer", "acquire")
            | ("java/util/concurrent/locks/AbstractQueuedLongSynchronizer", "release")
            | ("java/util/concurrent/locks/AbstractQueuedLongSynchronizer", "acquireShared")
            | ("java/util/concurrent/locks/AbstractQueuedLongSynchronizer", "releaseShared")
            | ("java/util/concurrent/locks/AbstractQueuedLongSynchronizer", "signalNext")
            | ("java/util/concurrent/locks/AbstractQueuedLongSynchronizer", "signalNextIfShared")
            | ("java/util/concurrent/locks/AbstractQueuedLongSynchronizer", "tryInitializeHead")
            | ("java/util/concurrent/locks/AbstractQueuedLongSynchronizer", "casTail")
            | ("java/util/concurrent/locks/AbstractQueuedLongSynchronizer", "enqueue")
            | ("java/util/concurrent/locks/AbstractQueuedLongSynchronizer", "isEnqueued")
            | ("java/util/concurrent/locks/AbstractQueuedLongSynchronizer", "reacquire")
            | ("java/util/concurrent/locks/AbstractQueuedLongSynchronizer", "acquireOnOOME")
            | ("java/util/concurrent/locks/AbstractQueuedLongSynchronizer", "cleanQueue")
            | ("java/util/concurrent/locks/AbstractQueuedLongSynchronizer", "cancelAcquire")
            | ("java/util/concurrent/locks/AbstractQueuedLongSynchronizer$Node", "casPrev")
            | ("java/util/concurrent/locks/AbstractQueuedLongSynchronizer$Node", "casNext")
            | (
                "java/util/concurrent/locks/AbstractQueuedLongSynchronizer$Node",
                "getAndUnsetStatus"
            )
            | ("java/util/concurrent/locks/AbstractQueuedLongSynchronizer$Node", "setPrevRelaxed")
            | ("java/util/concurrent/locks/AbstractQueuedLongSynchronizer$Node", "setStatusRelaxed")
            | ("java/util/concurrent/locks/AbstractQueuedLongSynchronizer$Node", "clearStatus")
            | ("java/util/concurrent/locks/AbstractQueuedLongSynchronizer$ConditionObject", "signal")
            | ("java/util/concurrent/locks/AbstractQueuedLongSynchronizer$ConditionObject", "signalAll")
            | ("java/util/concurrent/locks/AbstractQueuedLongSynchronizer$ConditionObject", "doSignal")
            | ("java/util/concurrent/locks/AbstractQueuedLongSynchronizer$ConditionObject", "await")
            | ("java/util/concurrent/locks/AbstractQueuedLongSynchronizer$ConditionObject", "awaitNanos")
            | ("java/util/concurrent/locks/AbstractQueuedLongSynchronizer$ConditionObject", "awaitUntil")
            | (
                "java/util/concurrent/locks/AbstractQueuedLongSynchronizer$ConditionObject",
                "awaitUninterruptibly"
            )
            | (
                "java/util/concurrent/locks/AbstractQueuedLongSynchronizer$ConditionObject",
                "newConditionNode"
            )
            | (
                "java/util/concurrent/locks/AbstractQueuedLongSynchronizer$ConditionObject",
                "enableWait"
            )
            // --- ReentrantReadWriteLock ---
            | ("java/util/concurrent/locks/ReentrantReadWriteLock$WriteLock", "lock")
            | ("java/util/concurrent/locks/ReentrantReadWriteLock$WriteLock", "unlock")
            | ("java/util/concurrent/locks/ReentrantReadWriteLock$WriteLock", "tryLock")
            | ("java/util/concurrent/locks/ReentrantReadWriteLock$ReadLock", "lock")
            | ("java/util/concurrent/locks/ReentrantReadWriteLock$ReadLock", "unlock")
            | ("java/util/concurrent/locks/ReentrantReadWriteLock$ReadLock", "tryLock")
            | ("java/util/concurrent/locks/ReentrantReadWriteLock$Sync", "tryAcquire")
            | ("java/util/concurrent/locks/ReentrantReadWriteLock$Sync", "tryRelease")
            | ("java/util/concurrent/locks/ReentrantReadWriteLock$Sync", "tryAcquireShared")
            | ("java/util/concurrent/locks/ReentrantReadWriteLock$Sync", "tryReleaseShared")
            | ("java/util/concurrent/locks/ReentrantReadWriteLock$Sync$HoldCounter", "<init>")
            | (
                "java/util/concurrent/locks/ReentrantReadWriteLock$Sync$ThreadLocalHoldCounter",
                "initialValue"
            )
    )
}

/// ALV5th (2026-07-10) -- ConcurrentLinkedQueue's allocate-then-CAS hot
/// methods. Sibling of `is_known_miscompile_aqs_family`: same JIT
/// miscompile archetype (allocate a node, then CAS/relaxed-append it into
/// a lock-free linked structure), different j.u.c. class. See the call
/// site's doc comment for the concrete repro (JUnit Description.fChildren).
fn is_known_miscompile_clq_family(class_name: &str, method_name: &str) -> bool {
    matches!(
        (class_name, method_name),
        ("java/util/concurrent/ConcurrentLinkedQueue", "add")
            | ("java/util/concurrent/ConcurrentLinkedQueue", "offer")
            | (
                "java/util/concurrent/ConcurrentLinkedQueue",
                "tryCasSuccessor"
            )
            | ("java/util/concurrent/ConcurrentLinkedQueue", "updateHead")
            | ("java/util/concurrent/ConcurrentLinkedQueue", "succ")
            | ("java/util/concurrent/ConcurrentLinkedQueue", "poll")
            | (
                "java/util/concurrent/ConcurrentLinkedQueue",
                "skipDeadNodes"
            )
            | ("java/util/concurrent/ConcurrentLinkedQueue", "<init>")
            | ("java/util/concurrent/ConcurrentLinkedQueue$Node", "<init>")
            | (
                "java/util/concurrent/ConcurrentLinkedQueue$Node",
                "appendRelaxed"
            )
            | ("java/util/concurrent/ConcurrentLinkedQueue$Node", "casItem")
    )
}

/// True for a class generated by `java.lang.reflect.Proxy` -- JDK 9+ names
/// them `jdk/proxy<N>/$Proxy<M>` (one package per defining loader) and puts
/// unnamed-module proxies for non-public interfaces in the interface's own
/// package as `<pkg>/$Proxy<M>`.
///
/// **Why these must never be JIT-compiled** (SPR-PROXY.1, 2026-07-28): every
/// method a proxy class declares is the same three-line trampoline --
/// `return (T) super.h.invoke(this, mN, args)` -- so compiling one buys
/// nothing in throughput. What it costs is correctness: CratonVM does not
/// implement proxy semantics in that bytecode, it implements them at DISPATCH
/// (`vm_exec.rs`'s `proxy_invoke_handler_shared` /
/// `proxy_annotation_handler_invoke` / `annotation_proxy_dispatch_impl`,
/// which is where annotation-member coercion and the delegation of `equals`
/// to a FOREIGN proxy of the same annotation type live). A compiled proxy
/// body is a THIRD dispatch path that bypasses all of it, on top of the two
/// the `MergedAnnotationsTests` asymmetric-`equals` fix already had to chase.
///
/// Measured symptom: `beans.PropertyDescriptorUtilsPropertyResolutionTests`
/// (a JUnit `@ParameterizedClass` + `@FieldSource` with `@Nested` children,
/// so JUnit's annotation scanning drives proxy accessors hard) died with
/// `OutOfMemoryError: Java heap space` after ~100 s of GCs that reclaimed
/// almost nothing -- at 512 MB, 2 GB and 8 GB heaps alike. `--nojit` runs it
/// clean. Package bisection over eight configurations pinned it exactly:
/// every configuration with `jdk/proxy` JIT-eligible died, every
/// configuration without it passed, including one with `org/springframework/,
/// org/junit/, java/, org/assertj/, net/bytebuddy/` all compiled.
fn is_jdk_dynamic_proxy_class(class_name: &str) -> bool {
    let Some(simple) = class_name.rsplit('/').next() else {
        return false;
    };
    let Some(rest) = simple.strip_prefix("$Proxy") else {
        return false;
    };
    // `$Proxy` is followed by the generator's counter and nothing else; a
    // user class merely NAMED `$ProxyFactory` must stay compilable.
    !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit())
}

fn is_antlr_prediction_context_miscompile(class_name: &str, method_name: &str) -> bool {
    // Matched on the suffix so BOTH copies of the ANTLR 4 runtime are covered:
    // Groovy's shaded `groovyjarjarantlr4/` fork (where the equality/hash
    // miscompile was originally found) and the ordinary unshaded
    // `org/antlr/v4/runtime/` artifact that Hibernate and Keycloak depend on.
    // The two are the same bytecode under different package names, so the same
    // 7 methods are at risk in both.
    //
    // The unshaded half used to be pinned to the interpreter only INCIDENTALLY,
    // by HIB-LONGTAIL.1's second `org/antlr/v4/runtime/` prefix. That prefix
    // was dropped 2026-07-27 (see its comment in `should_skip_jit_internal`)
    // once a real Hibernate HQL A/B showed the broad ban was unnecessary --
    // which would have silently un-pinned this narrow cluster too. Naming both
    // prefixes here keeps the narrow, evidence-backed guard exactly as strong
    // as it was while the broad package ban goes away. Mirrors what
    // `is_antlr_prediction_context_native_override` (interpreter.rs) already
    // does for the native-dispatch side.
    let Some(rest) = class_name
        .strip_prefix("groovyjarjarantlr4/v4/runtime/")
        .or_else(|| class_name.strip_prefix("org/antlr/v4/runtime/"))
    else {
        return false;
    };
    matches!(
        (rest, method_name),
        ("atn/PredictionContext", "calculateHashCode" | "hashCode")
            | ("atn/PredictionContext$IdentityEqualityComparator", "hashCode")
            | ("atn/SingletonPredictionContext", "equals" | "isEmpty" | "size")
            | ("misc/ObjectEqualityComparator", "equals")
    )
}

/// True if `prefix` matches any entry in `allow_packages`. An entry matches if
/// `prefix` starts with the entry, so `CRATONVM_JIT_ALLOW_PACKAGES=java/util`
/// lifts the `java/util/` ban.
fn package_allowed(prefix: &str, allow_packages: &[&str]) -> bool {
    allow_packages
        .iter()
        .any(|entry| !entry.is_empty() && prefix.starts_with(entry))
}

fn is_bouncycastle_crypto_hotpath_carveout(class_name: &str, method_name: &str) -> bool {
    let is_crypto_hotpath = matches!(
        class_name,
        "org/bouncycastle/crypto/BufferedBlockCipher"
            | "org/bouncycastle/crypto/DefaultBufferedBlockCipher"
    ) || matches!(
        class_name,
        c if c.starts_with("org/bouncycastle/crypto/engines/")
            || c.starts_with("org/bouncycastle/crypto/io/")
            || c.starts_with("org/bouncycastle/crypto/modes/")
            || c.starts_with("org/bouncycastle/crypto/paddings/")
            // perf/throughput-20260710 — BC math (EC + field arithmetic)
            // un-banned. The RBC.1 comment's own lifting criterion — "the EC
            // AllTests run completes cleanly under the allow-packages
            // override" — is now met on this tree: `org.bouncycastle.math.ec.
            // test.AllTests` = OK (14 tests) under
            // CRATONVM_JIT_ALLOW_PACKAGES=org/bouncycastle/ (the historic
            // deterministic rc=139 config), FixedPointTest passes repeatedly
            // at the June-05 -Xmx256m 100%-corruption repro config, and
            // GOST3412Test soaks 10/10 with OSR-for-newarray re-enabled. The
            // upstream producers were retired by the accumulated fixes since
            // June (locals/stack NaN-box kind tags, moving-young + precise JIT
            // maps, RRWL/refproc root fixes, ThreadLocal value rooting, the
            // guarded inline getfield). Interpreted EC was the BC suite's
            // dominant cost: NISTECC alone 94.8s interpreted → 43.2s JIT'd.
            || c.starts_with("org/bouncycastle/math/")
    );
    if !is_crypto_hotpath {
        return false;
    }

    // CAST key schedule code corrupts S-box indices when compiled in the full
    // stream test; keep setup interpreted while allowing block operations.
    if matches!(
        class_name,
        "org/bouncycastle/crypto/engines/CAST5Engine"
            | "org/bouncycastle/crypto/engines/CAST6Engine"
    ) && matches!(method_name, "init" | "setKey")
    {
        return false;
    }

    // NIST CTS mode hit a compiled processBytes watchdog during the same
    // validation pass. It is not a dominant hot path, so keep it guarded.
    if class_name == "org/bouncycastle/crypto/modes/NISTCTSBlockCipher" {
        return false;
    }

    true
}

/// Parse the `CRATONVM_JIT_ALLOW_PACKAGES` env var into a list of allowed
/// package prefixes. The result is cached at first call so repeated
/// `should_skip_jit` invocations do not re-parse.
///
/// The leading-edge use case is benchmarking: a developer running
/// `CRATONVM_JIT_ALLOW_PACKAGES=java/util cargo test` lifts the `java/util/`
/// ban for that one run without editing source.
pub fn allow_packages_from_env() -> &'static [&'static str] {
    use std::sync::OnceLock;
    static CACHE: OnceLock<Vec<&'static str>> = OnceLock::new();
    CACHE
        .get_or_init(|| {
            cratonvm_types::flags::runtime_var("CRATONVM_JIT_ALLOW_PACKAGES")
                .ok()
                .map(|s| {
                    // Leak each entry so the borrow lives for 'static. The
                    // env var is read once per process so the leak is bounded.
                    s.split(',')
                        .map(|p| p.trim())
                        .filter(|p| !p.is_empty())
                        .map(|p| {
                            let owned: String = p.to_string();
                            Box::leak(owned.into_boxed_str()) as &'static str // LEAK(intentional): env var read once per process; bounded leak for 'static lifetime in OnceLock
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        })
        .as_slice()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(
        cn: &str,
        mn: &str,
        iface: bool,
        named: bool,
        policy: SkipPolicy,
    ) -> Option<SkipReason> {
        should_skip_jit(cn, mn, iface, named, policy, &[])
    }

    fn check_with(
        cn: &str,
        mn: &str,
        iface: bool,
        named: bool,
        policy: SkipPolicy,
        allow: &[&str],
    ) -> Option<SkipReason> {
        should_skip_jit(cn, mn, iface, named, policy, allow)
    }

    #[test]
    fn clinit_always_skipped() {
        assert_eq!(
            check("Foo", "<clinit>", false, true, SkipPolicy::Aggressive),
            Some(SkipReason::ClassInitializer)
        );
    }

    #[test]
    fn init_always_skipped() {
        assert_eq!(
            check("Foo", "<init>", false, true, SkipPolicy::Aggressive),
            Some(SkipReason::Constructor)
        );
    }

    #[test]
    fn interface_default_always_skipped() {
        assert_eq!(
            check("Foo", "bar", true, true, SkipPolicy::Aggressive),
            Some(SkipReason::InterfaceDefault)
        );
    }

    #[test]
    fn unnamed_thread_always_skipped() {
        assert_eq!(
            check("Foo", "bar", false, false, SkipPolicy::Aggressive),
            Some(SkipReason::UnnamedThread)
        );
    }

    #[test]
    fn spring_boot_modified_classpath_loader_is_jit_eligible_after_removal() {
        // SPRINGBOOT-WITHOUT-JACKSON.2 (a narrow, method-specific,
        // both-policies guard) was removed 2026-07-26 -- see the removal
        // comment above should_skip_jit_internal for the re-verification
        // evidence. At the time, this class was ALSO caught under
        // Conservative by the separate, broader "org/springframework/boot/"
        // blanket ban (SPB.4c), so the removal was only independently
        // observable under Aggressive. SPB.4/.4b/.4c were themselves removed
        // 2026-07-27 (see that removal comment, real spring-boot-4.0.6.jar
        // suite evidence) with no shadow remaining, so this class is now
        // unconditionally JIT-eligible under both policies.
        let class_name =
            "org/springframework/boot/testsupport/classpath/ModifiedClassPathClassLoader";
        for policy in [SkipPolicy::Conservative, SkipPolicy::Aggressive] {
            assert_eq!(
                check(class_name, "loadClass", false, true, policy),
                None,
                "{class_name}.loadClass must be JIT-eligible under {policy:?} now that both SPRINGBOOT-WITHOUT-JACKSON.2 and its former SPB.4c shadow are removed"
            );
        }
    }

    #[test]
    fn biginteger_itself_is_always_interpreted() {
        // HIB-BIGINTEGER-AIOOBE.2: BigInteger itself, not just its
        // MutableBigInteger helper, must stay interpreted regardless of
        // policy or package-allow overrides. Uses a non-constructor method
        // (`<init>` is already caught by the separate, earlier generic
        // constructor gate, so it would not exercise this class-name check).
        for policy in [SkipPolicy::Conservative, SkipPolicy::Aggressive] {
            assert_eq!(
                check_with(
                    "java/math/BigInteger",
                    "toString",
                    false,
                    true,
                    policy,
                    &["java/math/"],
                ),
                Some(SkipReason::BigIntegerArithmetic),
                "BigInteger.toString must remain interpreted under {policy:?}",
            );
        }
    }

    #[test]
    fn hibernate_biginteger_divide_cluster_is_always_interpreted() {
        // HIB-BIGINTEGER-AIOOBE.1: do not let a package-allow override or the
        // aggressive policy re-enable the known-corrupting arithmetic class.
        for policy in [SkipPolicy::Conservative, SkipPolicy::Aggressive] {
            for method in [
                "divideMagnitude",
                "divideKnuth",
                "mulsub",
                "primitiveLeftShift",
            ] {
                assert_eq!(
                    check_with(
                        "java/math/MutableBigInteger",
                        method,
                        false,
                        true,
                        policy,
                        &["java/math/"],
                    ),
                    Some(SkipReason::BigIntegerArithmetic),
                    "{method} must remain interpreted under {policy:?}",
                );
            }
        }

        // HIB-BIGINTEGER-AIOOBE.2 (2026-07-26) widened the quarantine to
        // BigInteger itself too -- see biginteger_itself_is_always_interpreted
        // above and that ban's doc comment for the deterministic repro that
        // justified it. BigInteger.smallToString is therefore ALSO now
        // interpreted, not exempt.
        assert_eq!(
            check(
                "java/math/BigInteger",
                "smallToString",
                false,
                true,
                SkipPolicy::Aggressive,
            ),
            Some(SkipReason::BigIntegerArithmetic),
        );
    }

    #[test]
    fn java_util_regalloc_family_lifted_under_safe_default() {
        // The historical NEW-1.3 member. Its targeted-table entry was inert
        // for weeks and was deleted 2026-07-27; this asserts the resulting
        // (unchanged) behaviour, under both policies.
        assert_eq!(
            check(
                "java/util/HashMap",
                "put",
                false,
                true,
                SkipPolicy::Conservative
            ),
            None
        );
        assert_eq!(
            check(
                "java/util/HashMap",
                "put",
                false,
                true,
                SkipPolicy::Aggressive
            ),
            None
        );
    }

    #[test]
    fn snakeyaml_emitter_emit_is_jit_eligible_after_es_jit_deopt_gc_1_removal() {
        // ES-JIT-DEOPT-GC.1 was removed 2026-07-26 -- see the removal
        // comment above should_skip_jit_internal for the re-verification
        // evidence.
        for policy in [SkipPolicy::Conservative, SkipPolicy::Aggressive] {
            assert_eq!(
                check(
                    "org/yaml/snakeyaml/emitter/Emitter",
                    "emit",
                    false,
                    true,
                    policy,
                ),
                None,
                "Emitter.emit must be JIT-eligible now that ES-JIT-DEOPT-GC.1 is removed"
            );
        }
    }

    #[test]
    fn unboundid_is_jit_eligible_after_jndirealm_ban_removal() {
        // TOMCAT-JNDIREALM-RDN.1 + JIT.2 removed 2026-07-26. Both the single
        // accessor RDN.1 named and the wider set of UnboundID classes JIT.2
        // covered must now be eligible under the CONSERVATIVE policy with no
        // allow-packages override -- that is exactly the configuration the
        // real 76-case TestJNDIRealmIntegration matrix runs in.
        for (class_name, method_name) in [
            ("com/unboundid/ldap/sdk/RDN", "getNameValuePairs"),
            ("com/unboundid/ldap/sdk/RDN", "compare"),
            ("com/unboundid/ldap/sdk/RDN", "compareTo"),
            ("com/unboundid/ldap/sdk/RDNNameValuePair", "compareTo"),
            (
                "com/unboundid/ldap/matchingrules/CaseIgnoreStringMatchingRule",
                "normalizeInternal",
            ),
            ("com/unboundid/asn1/ASN1OctetString", "getValue"),
            ("com/unboundid/util/ByteStringBuffer", "append"),
        ] {
            assert_eq!(
                check(class_name, method_name, false, true, SkipPolicy::Conservative),
                None,
                "{class_name}.{method_name} must be JIT eligible under the \
                 conservative policy after the TOMCAT-JNDIREALM ban removal"
            );
            assert_eq!(
                check(class_name, method_name, false, true, SkipPolicy::Aggressive),
                None
            );
        }
    }

    #[test]
    fn hibernate_temporal_package_is_jit_eligible_after_removal() {
        for cls in [
            "org/hibernate/type/descriptor/sql/internal/DdlTypeImpl",
            "org.hibernate.testing.jdbc.SharedDriverManagerConnectionProvider",
        ] {
            assert_eq!(
                check(cls, "getRawTypeNames", false, true, SkipPolicy::Conservative),
                None,
                "{cls} must be JIT eligible under the conservative policy after \
                 the HIB-TEMPORAL.1 package ban removal"
            );
            assert_eq!(
                check(cls, "getRawTypeNames", false, true, SkipPolicy::Aggressive),
                None
            );
        }
    }

    #[test]
    fn jaxb_mapping_package_is_jit_eligible_after_removal() {
        // The `org/glassfish/jaxb/` ban was removed 2026-07-27 -- see the
        // removal comment in `should_skip_jit_internal` for the
        // re-verification evidence.
        for cls in [
            "org/glassfish/jaxb/runtime/v2/runtime/reflect/Accessor",
            "org.glassfish.jaxb.runtime.v2.runtime.reflect.Accessor",
        ] {
            assert_eq!(
                check(cls, "get", false, true, SkipPolicy::Conservative),
                None,
                "{cls} must be JIT-eligible now that the JAXB ban is gone"
            );
        }
    }

    #[test]
    fn xerces_schema_package_is_jit_eligible_after_hazelcast_removal() {
        // SPRING-HAZELCAST-XERCES-JIT.1 was removed 2026-07-26 -- see the
        // removal comment above should_skip_jit_internal for the
        // re-verification evidence.
        for cls in [
            "com/sun/org/apache/xerces/internal/util/SymbolHash",
            "com.sun.org.apache.xerces.internal.impl.xs.SchemaGrammar",
        ] {
            assert_eq!(
                check(cls, "get", false, true, SkipPolicy::Conservative),
                None,
                "{cls} must be JIT-eligible now that SPRING-HAZELCAST-XERCES-JIT.1 is removed"
            );
        }
    }

    #[test]
    fn aqs_condition_wait_variants_stay_skipped_unconditionally() {
        // Superseded by `is_known_miscompile_aqs_family`: this family is
        // demonstrably still broken even under the safe (GPR-local-homes-
        // disabled) default (see that function's doc comment — a heavy-
        // contention repro of both `ReentrantReadWriteLock` and plain
        // `ReentrantLock` hangs permanently under exactly this config), so
        // it must stay skipped under Conservative regardless of
        // `callee_saved_gpr_local_homes_enabled()`. Aggressive still lifts
        // it, same as every other targeted-list entry, for developers
        // deliberately hunting new miscompiles.
        for method in ["await", "awaitNanos", "awaitUntil", "awaitUninterruptibly"] {
            assert!(is_known_miscompile_aqs_family(
                "java/util/concurrent/locks/AbstractQueuedSynchronizer$ConditionObject",
                method
            ));
            assert_eq!(
                check(
                    "java/util/concurrent/locks/AbstractQueuedSynchronizer$ConditionObject",
                    method,
                    false,
                    true,
                    SkipPolicy::Conservative,
                ),
                Some(SkipReason::JavaUtilCollection),
                "AQS ConditionObject.{method} must stay skipped under Conservative \
                 regardless of the GPR-local-homes gate"
            );
            assert_eq!(
                check(
                    "java/util/concurrent/locks/AbstractQueuedSynchronizer$ConditionObject",
                    method,
                    false,
                    true,
                    SkipPolicy::Aggressive,
                ),
                None,
                "Aggressive policy still deliberately lifts this family"
            );
        }
    }

    #[test]
    fn aqs_long_synchronizer_and_rrwl_sync_family_skipped_unconditionally() {
        // The newly-discovered members of this family (Node helpers,
        // tryInitializeHead, ReentrantReadWriteLock$Sync/HoldCounter) —
        // confirmed via a 16-thread heavy-contention repro that these are
        // the actually-compiled methods (via CRATONVM_DBG_JITC) at the
        // moment both ReentrantReadWriteLock and ReentrantLock hang.
        let cases: &[(&str, &str)] = &[
            (
                "java/util/concurrent/locks/AbstractQueuedSynchronizer",
                "tryInitializeHead",
            ),
            (
                "java/util/concurrent/locks/AbstractQueuedSynchronizer$Node",
                "getAndUnsetStatus",
            ),
            (
                "java/util/concurrent/locks/AbstractQueuedSynchronizer$Node",
                "clearStatus",
            ),
            ("java/util/concurrent/locks/ReentrantLock", "lock"),
            ("java/util/concurrent/locks/ReentrantLock$Sync", "lock"),
            (
                "java/util/concurrent/locks/ReentrantLock$NonfairSync",
                "tryAcquire",
            ),
            (
                "java/util/concurrent/locks/AbstractQueuedLongSynchronizer",
                "acquire",
            ),
            (
                "java/util/concurrent/locks/AbstractQueuedLongSynchronizer",
                "tryInitializeHead",
            ),
            (
                "java/util/concurrent/locks/AbstractQueuedLongSynchronizer$Node",
                "getAndUnsetStatus",
            ),
            (
                "java/util/concurrent/locks/AbstractQueuedLongSynchronizer$Node",
                "clearStatus",
            ),
            (
                "java/util/concurrent/locks/ReentrantReadWriteLock$WriteLock",
                "lock",
            ),
            (
                "java/util/concurrent/locks/ReentrantReadWriteLock$Sync",
                "tryAcquireShared",
            ),
        ];
        for &(class_name, method) in cases {
            assert!(
                is_known_miscompile_aqs_family(class_name, method),
                "{class_name}.{method} must be in the AQS family list"
            );
            assert_eq!(
                check(class_name, method, false, true, SkipPolicy::Conservative),
                Some(SkipReason::JavaUtilCollection),
                "{class_name}.{method} must stay skipped under Conservative"
            );
            assert_eq!(
                check(class_name, method, false, true, SkipPolicy::Aggressive),
                None,
                "{class_name}.{method} must be lifted under Aggressive"
            );
        }

        // `HoldCounter.<init>` is also in the family list (belt-and-suspenders
        // for the `should_skip_jit_with_init`/OSR path's non-trivial-
        // constructor check), but under the plain `check()` helper
        // (`skip_init_check=false`) EVERY `<init>` is unconditionally skipped
        // by an earlier, more general rule (`skip_list.rs` around the
        // `if method_name == "<init>" { return Some(SkipReason::Constructor) }`
        // block) before this family's check is ever reached — so it reports
        // `Constructor`, not `JavaUtilCollection`, but is still skipped
        // either way.
        let class_name = "java/util/concurrent/locks/ReentrantReadWriteLock$Sync$HoldCounter";
        assert!(is_known_miscompile_aqs_family(class_name, "<init>"));
        assert_eq!(
            check(class_name, "<init>", false, true, SkipPolicy::Conservative),
            Some(SkipReason::Constructor),
            "HoldCounter.<init> must stay skipped under Conservative (via the general \
             constructor rule)"
        );
    }

    #[test]
    fn concurrent_locks_package_is_conservative_only() {
        let class_name = "java/util/concurrent/locks/ReentrantLock$Sync";
        assert_eq!(
            check(class_name, "lock", false, true, SkipPolicy::Conservative),
            Some(SkipReason::JavaUtilCollection)
        );
        assert_eq!(
            check(class_name, "lock", false, true, SkipPolicy::Aggressive),
            None
        );
    }

    #[test]
    fn weakhashmap_spliterator_walk_lifted_under_safe_default() {
        // ES-HANG-01's targeted-table entries were inert for weeks and were
        // deleted 2026-07-27; these methods are, and remain, JIT-eligible.
        for (cls, m) in [
            ("java/util/WeakHashMap$KeySpliterator", "tryAdvance"),
            ("java/util/WeakHashMap$KeySpliterator", "forEachRemaining"),
            ("java/util/WeakHashMap$ValueSpliterator", "tryAdvance"),
            ("java/util/WeakHashMap$ValueSpliterator", "forEachRemaining"),
            ("java/util/WeakHashMap$EntrySpliterator", "tryAdvance"),
            ("java/util/WeakHashMap$EntrySpliterator", "forEachRemaining"),
        ] {
            assert_eq!(
                check(cls, m, false, true, SkipPolicy::Conservative),
                None,
                "{cls}.{m} must be JIT-eligible under the safe default"
            );
            assert_eq!(
                check(cls, m, false, true, SkipPolicy::Aggressive),
                None,
                "{cls}.{m} must remain JIT-eligible under Aggressive"
            );
        }
        // A non-walk WeakHashMap method stays JIT-eligible.
        assert_eq!(
            check(
                "java/util/WeakHashMap",
                "size",
                false,
                true,
                SkipPolicy::Conservative
            ),
            None
        );
    }

    #[test]
    fn java_util_unrelated_methods_now_jit_eligible_under_conservative() {
        // T1.1.g — the blanket ban is gone. Unrelated java/util
        // methods are now JIT-eligible even under Conservative.
        assert_eq!(
            check(
                "java/util/HashMap",
                "size",
                false,
                true,
                SkipPolicy::Conservative
            ),
            None,
            "non-targeted HashMap methods must be JIT-eligible after T1.1.g"
        );
        assert_eq!(
            check(
                "java/util/ArrayList",
                "add",
                false,
                true,
                SkipPolicy::Conservative
            ),
            None,
            "non-HashMap java/util classes must be JIT-eligible"
        );
    }

    #[test]
    fn java_util_targeted_overridable_via_allow_packages() {
        assert_eq!(
            check_with(
                "java/util/HashMap",
                "put",
                false,
                true,
                SkipPolicy::Conservative,
                &["java/util/"],
            ),
            None
        );
    }

    #[test]
    fn jdt_parser_and_ast_packages_are_jit_eligible_after_jasper_jdt_2_3_removal() {
        // JASPER-JDT.2 (`org/eclipse/jdt/internal/compiler/parser/`) and
        // JASPER-JDT.3 (`org/eclipse/jdt/internal/compiler/ast/`) were removed
        // 2026-07-26, RESTORED 2026-07-27 -- the removal runs had the
        // compiled-callee direct-entry path switched off and so measured an
        // inert dispatch -- and removed for good 2026-07-28, this time after
        // bisecting the real Tomcat repro to the commit that fixes it
        // (`613b10f4c`, the LICM / speculative pre-header bypass) and re-running
        // the Tomcat JSP suites with that dispatch path ON. See the removal
        // comment in `should_skip_jit_internal`.
        //
        // Both packages must now be JIT-eligible under the DEFAULT Conservative
        // policy, with no allow-list entry needed.
        let cases = [
            (
                "org/eclipse/jdt/internal/compiler/parser/Parser",
                "consumeRule",
            ),
            (
                "org/eclipse/jdt/internal/compiler/parser/Parser",
                "consumeTypeImportOnDemandDeclarationName",
            ),
            (
                "org/eclipse/jdt/internal/compiler/ast/QualifiedNameReference",
                "analyseCode",
            ),
        ];
        for (class_name, method) in cases {
            assert_eq!(
                check(class_name, method, false, true, SkipPolicy::Conservative),
                None,
                "{class_name}.{method} must be JIT-eligible under Conservative -- JASPER-JDT.2/.3 were removed 2026-07-28",
            );
            assert_eq!(
                check(class_name, method, false, true, SkipPolicy::Aggressive),
                None,
                "{class_name}.{method} must be JIT-eligible under Aggressive too",
            );
        }
    }

    #[test]
    fn keycloak_picocli_smallrye_packages_jit_eligible_after_kc26_lift() {
        // KC26-PIC.1 / KC26-PIC.2 were lifted 2026-07-27 (see the comment in
        // `check_conservative`): re-measured on the real Keycloak 26.6.1
        // server boot, JIT-allowing these packages costs nothing measurable
        // and breaks nothing. They must now be JIT-eligible under the DEFAULT
        // Conservative policy, with or without an allow-list entry.
        for class_name in [
            "org/keycloak/models/RealmModel",
            "org/keycloak/quarkus/runtime/cli/Picocli",
            "picocli/CommandLine",
            "io/smallrye/mutiny/Uni",
            "io/smallrye/config/SmallRyeConfig",
            "io/smallrye/config/RelocateConfigSourceInterceptor",
        ] {
            assert_eq!(
                check(class_name, "example", false, true, SkipPolicy::Conservative),
                None,
                "{class_name} must be JIT-eligible under Conservative after the KC26 lift"
            );
            assert_eq!(
                check(class_name, "example", false, true, SkipPolicy::Aggressive),
                None,
                "{class_name} must also be JIT-eligible under Aggressive"
            );
        }
    }

    #[test]
    fn rxjava3_package_jit_eligible_after_kc26_rx1_lift() {
        // KC26-RX.1 lifted 2026-07-27: `CRATONVM_JIT_ALLOW_PACKAGES=io/reactivex/`
        // on the real Keycloak 26.6.1 boot (which initialises the Infinispan
        // session providers this ban was written for) reached the
        // end-of-startup marker with no stall, inside the run-to-run noise of
        // the ban-in-place baseline.
        let class_name =
            "io/reactivex/rxjava3/internal/operators/flowable/BlockingFlowableIterable";
        assert_eq!(
            check(class_name, "hasNext", false, true, SkipPolicy::Conservative),
            None,
            "RxJava3 must be JIT-eligible under Conservative after the KC26-RX.1 lift"
        );
        assert_eq!(
            check(class_name, "hasNext", false, true, SkipPolicy::Aggressive),
            None,
            "RxJava3 must also be JIT-eligible under Aggressive"
        );
    }

    #[test]
    fn elasticsearch_vector_diskbbq_cluster_is_jit_eligible_after_es_cluster_removal() {
        // Drive-by repair, 2026-07-28. The blanket `org/elasticsearch/` ban
        // (ES-FRAGILE-CLUSTER.1) was removed on 2026-07-27 by `bae30dc3c` --
        // see the removal comment above `should_skip_jit_internal` -- but this
        // test was left asserting the ban, so `cargo test -p cratonvm-vm --lib
        // skip_list` has been red on `dev` ever since. Exactly the shape
        // `b8225b18c` had to repair for the JASPER-JDT test, so it is corrected
        // here alongside the JASPER-JDT.2/.3 removal rather than left for the
        // next session to trip over. Nothing about elasticsearch was re-measured
        // in doing so: the assertion is simply flipped to match the ban state
        // the lift established. (ElasticSearch is also outside the
        // tomcat/hibernate/spring/spring-boot/h2 scope this session is
        // otherwise focused on, so no ES-specific ban is being reconsidered
        // here either way -- this is purely fixing a red test.)
        for class_name in [
            "org/elasticsearch/index/codec/vectors/diskbbq/DocIdsWriterTests",
            "org/elasticsearch/index/codec/vectors/diskbbq/ES920DiskBBQVectorsFormatTests",
            "org/elasticsearch/index/codec/vectors/diskbbq/es94/ES940DiskBBQVectorsFormatTests",
            "org/elasticsearch/index/codec/vectors/diskbbq/next/ESNextDiskBBQVectorsFormatTests",
            "org/elasticsearch/index/codec/vectors/es93/ES93FlatVectorFormatTests",
            "org/elasticsearch/index/codec/vectors/es93/ES93HnswBitVectorsFormatTests",
            "org/elasticsearch/search/vectors/IVFKnnFloatSlicedVectorQueryTests",
        ] {
            for policy in [SkipPolicy::Conservative, SkipPolicy::Aggressive] {
                assert_eq!(
                    check(class_name, "testBody", false, true, policy),
                    None,
                    "{class_name} must be JIT-eligible now that the blanket \
                     org/elasticsearch/ ban is removed"
                );
            }
            assert_eq!(
                check_with(
                    class_name,
                    "testBody",
                    false,
                    true,
                    SkipPolicy::Conservative,
                    &["org/elasticsearch/"],
                ),
                None,
                "CRATONVM_JIT_ALLOW_PACKAGES=org/elasticsearch/ must lift {class_name}"
            );
        }

        assert_eq!(
            check(
                "org/apache/lucene/codecs/hnsw/DefaultFlatVectorScorer",
                "score",
                false,
                true,
                SkipPolicy::Conservative,
            ),
            None,
            "Lucene is JIT-admitted at the skip-list level since 117d2d906              retired the LUCENE-POSTINGS.1 package ban (ACC_SYNCHRONIZED              methods are gated in the interpreter, not here)"
        );
    }

    #[test]
    fn hamcrest_matchers_are_jit_eligible_after_es_hamcrest_1_removal() {
        // ES-HAMCREST.1 was removed 2026-07-26 -- see the removal comment
        // above should_skip_jit_internal for the re-verification evidence.
        let class_name = "org/hamcrest/core/IsEqual";
        for policy in [SkipPolicy::Conservative, SkipPolicy::Aggressive] {
            assert_eq!(
                check(class_name, "matches", false, true, policy),
                None,
                "{class_name}.matches must be JIT-eligible now that ES-HAMCREST.1 is removed"
            );
        }
    }

    #[test]
    fn netflix_discovery_is_jit_eligible_after_spb6_removal() {
        // SPB.6 (com/netflix/discovery/, Netflix Eureka DiscoveryClient)
        // was removed 2026-07-27 WITHOUT re-verification -- no fixture
        // ever existed on this host (no eureka-server app, no
        // eureka-client/eureka-core jar anywhere). Removed by explicit
        // user decision; see the removal comment above
        // should_skip_jit_internal.
        for policy in [SkipPolicy::Conservative, SkipPolicy::Aggressive] {
            assert_eq!(
                check(
                    "com/netflix/discovery/DiscoveryClient",
                    "register",
                    false,
                    true,
                    policy,
                ),
                None,
                "com/netflix/discovery/ must be JIT-eligible now that SPB.6 is removed (unverified)"
            );
        }
    }

    #[test]
    fn beans_factory_support_is_jit_eligible_after_spb9b_full_removal() {
        // org/springframework/beans/factory/support/ was removed
        // 2026-07-26 with real re-verification evidence (see the removal
        // comment above should_skip_jit_internal:
        // BeanFactorySupportProbe.java, real spring-beans-7.0.7.jar).
        for policy in [SkipPolicy::Conservative, SkipPolicy::Aggressive] {
            assert_eq!(
                check(
                    "org/springframework/beans/factory/support/DefaultListableBeanFactory",
                    "registerBeanDefinition",
                    false,
                    true,
                    policy,
                ),
                None,
                "org/springframework/beans/factory/support/ must be JIT-eligible now that its SPB.9b sub-ban is removed"
            );
        }
        // The sibling org/springframework/beans/factory/ ban (excluding
        // .../support/) was ALSO removed 2026-07-26, as the 4th sub-ban
        // of the separate SPB.9c group -- see that removal comment above
        // should_skip_jit_internal.
        for policy in [SkipPolicy::Conservative, SkipPolicy::Aggressive] {
            assert_eq!(
                check(
                    "org/springframework/beans/factory/config/BeanDefinitionHolder",
                    "getBeanName",
                    false,
                    true,
                    policy,
                ),
                None,
                "org/springframework/beans/factory/ (excluding support/) must be JIT-eligible now that SPB.9c's 4th sub-ban is also removed"
            );
        }
        // org/springframework/boot/loader/ and org/springframework/web/reactive/
        // + org/springframework/boot/web/reactive/ -- SPB.9b's other two
        // sub-bans -- were removed 2026-07-27 WITHOUT re-verification (no
        // fixture ever existed on this host; see the removal comment above
        // should_skip_jit_internal for the explicit-user-decision rationale).
        for policy in [SkipPolicy::Conservative, SkipPolicy::Aggressive] {
            assert_eq!(
                check(
                    "org/springframework/boot/loader/JarLauncher",
                    "launch",
                    false,
                    true,
                    policy,
                ),
                None,
                "org/springframework/boot/loader/ must be JIT-eligible now that SPB.9b is fully removed (unverified)"
            );
            assert_eq!(
                check(
                    "org/springframework/web/reactive/function/server/RouterFunctions",
                    "route",
                    false,
                    true,
                    policy,
                ),
                None,
                "org/springframework/web/reactive/ must be JIT-eligible now that SPB.9b is fully removed (unverified)"
            );
        }
    }

    #[test]
    fn spb9c_component_scan_packages_are_jit_eligible_after_removal() {
        // SPB.9c (all four sub-bans: org/springframework/context/annotation/,
        // org/springframework/context/support/,
        // org/springframework/core/io/support/, and
        // org/springframework/beans/factory/ excluding .../support/) was
        // removed 2026-07-26 -- see the removal comments above
        // should_skip_jit_internal for the re-verification evidence
        // (ComponentScanProbe.java, real spring-context-7.0.7.jar).
        // org/springframework/core/io/support/ was initially still shadowed
        // by the separate SPB.2 ban on the broader org/springframework/core/,
        // but SPB.2 was ALSO removed 2026-07-26 (see its own removal
        // comment, re-verified with SerializableTypeWrapperProbe.java) --
        // so all four SPB.9c sub-packages are now unconditionally
        // JIT-eligible with no remaining shadow.
        for (class_name, method) in [
            ("org/springframework/context/annotation/ConfigurationClassParser", "parse"),
            ("org/springframework/context/support/AbstractApplicationContext", "refresh"),
            ("org/springframework/core/io/support/PathMatchingResourcePatternResolver", "getResources"),
            ("org/springframework/beans/factory/config/BeanDefinitionHolder", "getBeanName"),
        ] {
            for policy in [SkipPolicy::Conservative, SkipPolicy::Aggressive] {
                assert_eq!(
                    check(class_name, method, false, true, policy),
                    None,
                    "{class_name}.{method} must be JIT-eligible now that SPB.9c (and, for core/io/support/, the formerly-shadowing SPB.2) are removed"
                );
            }
        }
    }

    #[test]
    fn springboot_boot_packages_are_jit_eligible_after_spb4_4b_4c_removal() {
        // SPB.4/.4b/.4c (org/springframework/boot/context/properties/bind/,
        // org/springframework/boot/context/, and the org/springframework/boot/
        // umbrella excl. boot/loader/) were removed 2026-07-27 -- see the
        // removal comment above should_skip_jit_internal for the
        // re-verification evidence (the real spring-boot-tomcat-crossmodule
        // 10-scenario suite, real spring-boot-4.0.6.jar, baseline vs.
        // CRATONVM_JIT_ALLOW_PACKAGES=org/springframework/boot/ byte-identical
        // across all 10 scenarios).
        for (class_name, method) in [
            (
                "org/springframework/boot/context/properties/bind/JavaBeanBinder",
                "bind",
            ),
            (
                "org/springframework/boot/context/properties/source/SystemEnvironmentPropertyMapper",
                "processElementValue",
            ),
            ("org/springframework/boot/SpringApplication", "run"),
        ] {
            for policy in [SkipPolicy::Conservative, SkipPolicy::Aggressive] {
                assert_eq!(
                    check(class_name, method, false, true, policy),
                    None,
                    "{class_name}.{method} must be JIT-eligible now that SPB.4/.4b/.4c are removed"
                );
            }
        }
        // org/springframework/boot/loader/ was untouched by the SPB.4/.4b/.4c
        // removal itself (SPB.9b's own, separate ban on it was the reason it
        // stayed banned at the time) -- but SPB.9b's remaining sub-bans were
        // themselves removed 2026-07-27 (see
        // beans_factory_support_is_jit_eligible_after_spb9b_full_removal),
        // so it is now JIT-eligible too.
        for policy in [SkipPolicy::Conservative, SkipPolicy::Aggressive] {
            assert_eq!(
                check(
                    "org/springframework/boot/loader/JarLauncher",
                    "launch",
                    false,
                    true,
                    policy,
                ),
                None,
                "org/springframework/boot/loader/ must be JIT-eligible now that SPB.9b is fully removed"
            );
        }
    }

    #[test]
    fn spring_core_is_jit_eligible_after_spb2_removal() {
        // SPB.2 (org/springframework/core/) was removed 2026-07-26 -- see
        // the removal comment above should_skip_jit_internal for the
        // re-verification evidence (SerializableTypeWrapperProbe.java,
        // real spring-core-7.0.7.jar, 3 runs under an external timeout
        // wrapper given the original symptom was a hang: baseline,
        // package-allowed, and a CRATONVM_JIT_THRESHOLD=1 aggressive
        // pass, all completed without hanging).
        for policy in [SkipPolicy::Conservative, SkipPolicy::Aggressive] {
            assert_eq!(
                check(
                    "org/springframework/core/SerializableTypeWrapper",
                    "forField",
                    false,
                    true,
                    policy,
                ),
                None,
                "org/springframework/core/SerializableTypeWrapper.forField must be JIT-eligible now that SPB.2 is removed"
            );
        }
    }

    #[test]
    fn beans_introspector_is_jit_eligible_after_spb9d_removal() {
        // SPB.9d was removed 2026-07-26 for the blanket com/sun/beans/ and
        // java/beans/ package ban -- see the removal comment above
        // should_skip_jit_internal for the re-verification evidence
        // (BeanIntrospectorProbe.java, pure JDK, no external jar). A
        // SEPARATE, older duplicate SPB.9d entry used to sit inside the inert
        // is_known_miscompile() block (targeting
        // com/sun/beans/introspect/MethodInfo$MethodOrder.compare,
        // org/springframework/beans/ExtendedBeanInfo$PropertyDescriptorComparator.compare,
        // java/util/StringJoiner.<init>); that whole block was removed
        // 2026-07-27 and never affected this test either way.
        for (class_name, method) in [
            ("java/beans/Introspector", "getBeanInfo"),
            ("com/sun/beans/introspect/MethodInfo", "getMethods"),
        ] {
            for policy in [SkipPolicy::Conservative, SkipPolicy::Aggressive] {
                assert_eq!(
                    check(class_name, method, false, true, policy),
                    None,
                    "{class_name}.{method} must be JIT-eligible now that SPB.9d is removed"
                );
            }
        }
    }

    #[test]
    fn slf4j_logback_are_jit_eligible_after_spb9_narrowing() {
        // SPB.9 was removed 2026-07-26 for `org/slf4j/` and
        // `ch/qos/logback/` -- see the removal/re-ban comment above
        // should_skip_jit_internal for the re-verification evidence and
        // the bisection that isolated the later real regression to
        // `org/apache/commons/logging/` specifically (that package is
        // re-banned below, see commons_logging_is_jit_banned_after_spb9_narrowing).
        for (class_name, method) in [
            ("org/slf4j/LoggerFactory", "getLogger"),
            ("ch/qos/logback/classic/Logger", "info"),
        ] {
            for policy in [SkipPolicy::Conservative, SkipPolicy::Aggressive] {
                assert_eq!(
                    check(class_name, method, false, true, policy),
                    None,
                    "{class_name}.{method} must be JIT-eligible (SPB.9 removal for this package still stands)"
                );
            }
        }
    }

    #[test]
    fn commons_logging_is_jit_banned_after_spb9_narrowing() {
        // Re-banned 2026-07-26, same day as the SPB.9 blanket removal, after
        // a real Spring Boot suite regression (LoggingApplicationListenerTests
        // 16/41 -> 34/41 failing, new "Unknown FilterReply value: DENY"
        // Logback signature) was bisected to `org/apache/commons/logging/`
        // specifically -- see the ban comment above should_skip_jit_internal.
        // `org/slf4j/` and `ch/qos/logback/` remain JIT-eligible (see
        // slf4j_logback_are_jit_eligible_after_spb9_narrowing).
        for (class_name, method) in [
            ("org/apache/commons/logging/LogFactory", "getLog"),
            ("org/apache/commons/logging/impl/Slf4jLogFactory", "getInstance"),
        ] {
            for policy in [SkipPolicy::Conservative, SkipPolicy::Aggressive] {
                assert_eq!(
                    check(class_name, method, false, true, policy),
                    Some(SkipReason::RustJvmTestFixture),
                    "{class_name}.{method} must be JIT-skipped (SPB.9 re-ban for this package specifically)"
                );
            }
        }
    }

    #[test]
    fn json_smart_parser_is_jit_eligible_after_jsonsmart_parser_1_removal() {
        // JSONSMART-PARSER.1 was removed 2026-07-26 -- see the removal
        // comment above should_skip_jit_internal for the re-verification
        // evidence (JsonSmartProbe.java, real json-smart-2.3.jar).
        for (class_name, method) in [
            ("net/minidev/json/parser/JSONParserString", "read"),
            ("net/minidev/json/parser/JSONParserString", "readS"),
            ("net/minidev/json/parser/JSONParserBase", "skipSpace"),
        ] {
            for policy in [SkipPolicy::Conservative, SkipPolicy::Aggressive] {
                assert_eq!(
                    check(class_name, method, false, true, policy),
                    None,
                    "{class_name}.{method} must be JIT-eligible now that JSONSMART-PARSER.1 is removed"
                );
            }
        }
    }

    #[test]
    fn wildfly_controller_is_jit_eligible_after_wildfly_controller_jit_1_removal() {
        // The former blanket org/jboss/as/controller/ guard masked an
        // invokespecial constructor defect. Keep both concrete boot paths
        // eligible under both policies; the real WildFly matrix is the
        // behavioural witness for construction and parallel execution.
        for (class_name, method_name) in [
            ("org/jboss/as/controller/OperationContextImpl", "executeOperation"),
            ("org/jboss/as/controller/AbstractOperationContext", "executeOperation"),
        ] {
            for policy in [SkipPolicy::Conservative, SkipPolicy::Aggressive] {
                assert_eq!(
                    check(class_name, method_name, false, true, policy),
                    None,
                    "{class_name}.{method_name} must stay JIT-eligible after WILDFLY-CONTROLLER-JIT.1 removal",
                );
            }
        }
    }

    #[test]
    fn cratonvm_exc_hierarchy_lifted_after_retry() {
        // 2026-07-01 retry: the former NEW-1.4 reproducer now passes under
        // forced inline JIT, so this stale targeted ban must stay lifted.
        assert_eq!(
            check(
                "cratonvm/TckLang",
                "exc_hierarchy",
                false,
                true,
                SkipPolicy::Conservative
            ),
            None
        );
    }

    #[test]
    fn cratonvm_unrelated_methods_now_jit_eligible_under_conservative() {
        // T1.1.g — the blanket ban on `cratonvm/*` is gone.
        assert_eq!(
            check("cratonvm/Other", "m", false, true, SkipPolicy::Conservative),
            None,
            "unrelated cratonvm/* methods must be JIT-eligible after T1.1.g"
        );
        assert_eq!(
            check(
                "cratonvm/TckLang",
                "str_length",
                false,
                true,
                SkipPolicy::Conservative
            ),
            None,
            "non-miscompile TckLang methods must be JIT-eligible"
        );
    }

    #[test]
    fn known_miscompile_overrides_apply_to_actual_package() {
        // Non-java targeted entries document their own allow-package escape
        // hatches. The override must be checked against the current class, not
        // only a fixed set of historical package roots.
        assert_eq!(
            check_with(
                "io/smallrye/config/ConfigValueConfigSource$ConfigValueProperties$LineReader",
                "readLine",
                false,
                true,
                SkipPolicy::Conservative,
                &["io/smallrye/config/"],
            ),
            None
        );
    }

    #[test]
    fn antlr_coldpath_non_bad_atn_methods_are_jit_eligible_after_antlr_1_removal() {
        // ANTLR.1 (the groovyjarjarantlr4/ blanket ban) was removed
        // 2026-07-26 -- see the removal comment above should_skip_jit_internal
        // for the re-verification evidence (real groovy-3.0.21.jar, cold-parse
        // throughput no longer regressed post the 2026-07-26 JIT rework).
        // ParserATNSimulator.closure_ is not one of the 7 PredictionContext
        // methods is_antlr_prediction_context_miscompile still covers, so it
        // is JIT-eligible under both default and explicit-allow now.
        for policy in [SkipPolicy::Conservative, SkipPolicy::Aggressive] {
            assert_eq!(
                check(
                    "groovyjarjarantlr4/v4/runtime/atn/ParserATNSimulator",
                    "closure_",
                    false,
                    true,
                    policy,
                ),
                None,
                "ANTLR cold-path non-PredictionContext methods must be JIT-eligible now that ANTLR.1 is removed"
            );
        }
    }

    #[test]
    fn hibernate_unshaded_antlr_runtime_is_jit_eligible_after_hib_longtail_1_narrowing() {
        // HIB-ANTLR.1's own check was removed 2026-07-26; the last thing still
        // forcing org/antlr/v4/runtime/ to the interpreter was HIB-LONGTAIL.1's
        // second prefix, dropped 2026-07-27 after a same-binary A/B over all 57
        // org.hibernate.orm.test.hql classes (the only real fixture on record
        // that parses HQL through this runtime) came back byte-identical, and a
        // ban-removed rebuild re-confirmed it. Every remaining justification for
        // HIB-LONGTAIL.1 comes from the H2 suite, which never loads an
        // org/antlr/ class -- so the two halves were independent all along.
        for policy in [SkipPolicy::Conservative, SkipPolicy::Aggressive] {
            assert_eq!(
                check(
                    "org/antlr/v4/runtime/atn/ParserATNSimulator",
                    "computeTargetState",
                    false,
                    true,
                    policy,
                ),
                None,
                "org/antlr/v4/runtime/ must be JIT-eligible now that HIB-LONGTAIL.1 is org/h2/-only"
            );
        }
        // ...but the narrow PredictionContext equality/hash guard
        // (ANTLR-COLDPATH.1) still covers the unshaded runtime too, and is
        // deliberately not lifted by CRATONVM_JIT_ALLOW_PACKAGES.
        assert_eq!(
            check_with(
                "org/antlr/v4/runtime/atn/PredictionContext",
                "calculateHashCode",
                false,
                true,
                SkipPolicy::Conservative,
                &["org/antlr/v4/runtime/"],
            ),
            Some(SkipReason::RustJvmTestFixture),
            "ANTLR-COLDPATH.1 still pins the PredictionContext cluster in the unshaded runtime"
        );
        // The org/h2/ half of HIB-LONGTAIL.1 is untouched by that narrowing.
        assert_eq!(
            check(
                "org/h2/mvstore/MVStore",
                "commit",
                false,
                true,
                SkipPolicy::Conservative,
            ),
            Some(SkipReason::RustJvmTestFixture),
            "org/h2/ must still be interpreted under Conservative via HIB-LONGTAIL.1"
        );
    }

    #[test]
    fn antlr_prediction_context_cluster_stays_interpreted_under_validation_lift() {
        for (cls, mn) in [
            (
                "groovyjarjarantlr4/v4/runtime/atn/PredictionContext",
                "calculateHashCode",
            ),
            (
                "groovyjarjarantlr4/v4/runtime/atn/PredictionContext",
                "hashCode",
            ),
            (
                "groovyjarjarantlr4/v4/runtime/atn/PredictionContext$IdentityEqualityComparator",
                "hashCode",
            ),
            (
                "groovyjarjarantlr4/v4/runtime/atn/SingletonPredictionContext",
                "equals",
            ),
            (
                "groovyjarjarantlr4/v4/runtime/atn/SingletonPredictionContext",
                "isEmpty",
            ),
            (
                "groovyjarjarantlr4/v4/runtime/atn/SingletonPredictionContext",
                "size",
            ),
            (
                "groovyjarjarantlr4/v4/runtime/misc/ObjectEqualityComparator",
                "equals",
            ),
        ] {
            assert_eq!(
                check_with(
                    cls,
                    mn,
                    false,
                    true,
                    SkipPolicy::Conservative,
                    &["groovyjarjarantlr4/"],
                ),
                Some(SkipReason::RustJvmTestFixture),
                "{cls}.{mn} must stay interpreted during ANTLR cold-path validation"
            );
        }
    }

    #[test]
    fn keycloak_credential_lazy_init_getters_lifted_under_safe_default() {
        // KC-CRED.LAZY's two targeted entries were inert and were deleted
        // 2026-07-27. Both getters were verified genuinely JIT-compiled
        // (`CRATONVM_DBG_JITC`) and correct against real keycloak-server-spi
        // 26.6.1 before that removal -- see
        // docs/internal/is-known-miscompile-block-retired-20260727.md.
        for cls in [
            "org/keycloak/models/credential/dto/PasswordCredentialData",
            "org/keycloak/models/credential/dto/PasswordSecretData",
        ] {
            assert_eq!(
                check(
                    cls,
                    "getAdditionalParameters",
                    false,
                    true,
                    SkipPolicy::Conservative
                ),
                None,
                "{cls}.getAdditionalParameters must be JIT-eligible under the safe default"
            );
            assert_eq!(
                check(
                    cls,
                    "getAdditionalParameters",
                    false,
                    true,
                    SkipPolicy::Aggressive
                ),
                None,
                "{cls}.getAdditionalParameters should remain debuggable under Aggressive"
            );
            assert_eq!(
                check_with(
                    cls,
                    "getAdditionalParameters",
                    false,
                    true,
                    SkipPolicy::Conservative,
                    &["org/keycloak/models/credential/"],
                ),
                None,
                "{cls}.getAdditionalParameters should be liftable for bisection"
            );
        }

        assert_eq!(
            check(
                "org/keycloak/models/credential/CredentialModel",
                "getPasswordCredentialData",
                false,
                true,
                SkipPolicy::Conservative
            ),
            None,
            "do not blanket-ban the surrounding Keycloak credential model"
        );
    }

    #[test]
    fn user_class_never_skipped() {
        assert_eq!(
            check(
                "com/example/App",
                "compute",
                false,
                true,
                SkipPolicy::Conservative
            ),
            None
        );
        assert_eq!(
            check(
                "com/example/App",
                "compute",
                false,
                true,
                SkipPolicy::Aggressive
            ),
            None
        );
    }

    // ------------------------------------------------------------------
    // NEW-1 CI gate tests — these enforce that REMOVED bans stay removed.
    // ------------------------------------------------------------------

    /// NEW-1.2 CI gate: java/lang/* is no longer blanket-banned.
    /// (W2-CHM now keeps only boxing constructors under the constructor gate;
    /// `Integer.valueOf` / `Long.valueOf` are JIT-eligible after the 2026-06-11
    /// retest, so unrelated `java/lang/*` methods like `String.indexOf` still demonstrate the
    /// "no blanket ban" guarantee.)
    #[test]
    fn java_lang_is_jit_eligible_after_new_1_2() {
        assert_eq!(
            check(
                "java/lang/String",
                "indexOf",
                false,
                true,
                SkipPolicy::Conservative
            ),
            None,
            "java/lang/* must be JIT-eligible after NEW-1.2 (instanceof fix)"
        );
        assert_eq!(
            check(
                "java/lang/Integer",
                "toString",
                false,
                true,
                SkipPolicy::Conservative
            ),
            None,
            "Integer.toString stays JIT-eligible — only constructors remain guarded"
        );
    }

    /// W2-CHM CI gate: `Integer.valueOf` / `Long.valueOf` were lifted after
    /// the regalloc/length-table fixes, while field-storing boxing constructors
    /// remain interpreted through the generic constructor gate.
    #[test]
    fn integer_long_valueof_lifted_but_constructors_stay_guarded() {
        for (cls, mn) in [
            ("java/lang/Integer", "valueOf"),
            ("java/lang/Long", "valueOf"),
        ] {
            assert_eq!(
                check(cls, mn, false, true, SkipPolicy::Conservative),
                None,
                "{cls}.{mn} should remain JIT-eligible after the W2-CHM lift"
            );
        }
        for (cls, mn) in [
            ("java/lang/Integer", "<init>"),
            ("java/lang/Long", "<init>"),
        ] {
            assert!(
                check(cls, mn, false, true, SkipPolicy::Conservative).is_some(),
                "{cls}.{mn} must stay guarded by the constructor policy"
            );
        }
    }

    /// NEW-1.2 CI gate: cratonvm/Tck* is no longer blanket-banned.
    #[test]
    fn tck_class_is_jit_eligible_after_new_1_2() {
        assert_eq!(
            check(
                "cratonvm/TckLang",
                "test1",
                false,
                true,
                SkipPolicy::Aggressive
            ),
            None,
            "TCK classes must be JIT-eligible after NEW-1.2 (instanceof fix); \
             package-prefix RustJvmTestFixture ban only applies under Conservative"
        );
    }

    /// NEW-1.5 CI gate: FinalizerTest is no longer blanket-banned.
    #[test]
    fn finalizer_test_is_jit_eligible_after_new_1_5() {
        assert_eq!(
            check("FinalizerTest", "run", false, true, SkipPolicy::Aggressive),
            None,
            "FinalizerTest must be JIT-eligible after NEW-1.5 (conservative root scan)"
        );
        assert_eq!(
            check(
                "com/example/MyFinalizerTest",
                "f",
                false,
                true,
                SkipPolicy::Aggressive
            ),
            None
        );
    }

    // ===================================================================
    // T1.1.f — InitComplexity classification + narrowed init/clinit ban
    // ===================================================================

    #[test]
    fn classify_trivial_default_ctor() {
        // javac for `public Foo() {}`:
        //   aload_0 (0x2a); invokespecial #1 (0xb7 00 01); return (0xb1)
        let bc = vec![0x2a, 0xb7, 0x00, 0x01, 0xb1];
        assert_eq!(classify_init_complexity(&bc), InitComplexity::Trivial);
    }

    /// A `putfield`-only constructor is **Trivial by default** since
    /// 2026-07-28 — see `allow_putfield_init`, which lifted that structural
    /// ban on measurement (1846 ns/alloc banned vs 226 allowed) and kept
    /// `CRATONVM_JIT_PUTFIELD_INIT=0` as the kill switch.
    ///
    /// This test used to assert `Complex`, i.e. the pre-flip behaviour, and
    /// was left behind when the default changed. It now pins BOTH sides of the
    /// gate, which is what the kill switch actually needs: flipping the
    /// default back must not require rediscovering that `putfield` is the one
    /// disqualifier the flag governs.
    #[test]
    fn putfield_only_ctor_follows_the_putfield_gate() {
        // aload_0; aload_0; iconst_1; putfield #2; return
        let bc = vec![0x2a, 0x2a, 0x04, 0xb5, 0x00, 0x02, 0xb1];
        assert_eq!(
            classify_init_complexity_with(&bc, true),
            InitComplexity::Trivial,
            "default (CRATONVM_JIT_PUTFIELD_INIT unset): a field-storing ctor is compilable",
        );
        assert_eq!(
            classify_init_complexity_with(&bc, false),
            InitComplexity::Complex,
            "kill switch (CRATONVM_JIT_PUTFIELD_INIT=0): the historical ban returns",
        );
        // The public entry point agrees with whichever side the process latched.
        let expected = if super::allow_putfield_init() {
            InitComplexity::Trivial
        } else {
            InitComplexity::Complex
        };
        assert_eq!(classify_init_complexity(&bc), expected);
    }

    /// The other four disqualifiers are NOT governed by the `putfield` gate:
    /// `putstatic`, `monitorenter`, `monitorexit` and `invokedynamic` stay
    /// `Complex` on both sides of it.
    #[test]
    fn the_other_disqualifiers_ignore_the_putfield_gate() {
        for bc in [
            vec![0x05, 0xb3, 0x00, 0x03, 0xb1], // iconst_2; putstatic #3; return
            vec![0x2a, 0xc2, 0x2a, 0xc3, 0xb1], // aload_0; monitorenter; aload_0; monitorexit; return
            vec![0xba, 0x00, 0x04, 0x00, 0x00, 0xb1], // invokedynamic #4 0 0; return
        ] {
            for allow_putfield in [true, false] {
                assert_eq!(
                    classify_init_complexity_with(&bc, allow_putfield),
                    InitComplexity::Complex,
                    "bytecode {bc:02x?} must stay Complex with allow_putfield={allow_putfield}",
                );
            }
        }
    }

    #[test]
    fn classify_complex_clinit_with_putstatic() {
        // iconst_2; putstatic #3; return
        let bc = vec![0x05, 0xb3, 0x00, 0x03, 0xb1];
        assert_eq!(classify_init_complexity(&bc), InitComplexity::Complex);
    }

    #[test]
    fn classify_complex_clinit_with_invokedynamic() {
        // invokedynamic #4 0 0; return
        let bc = vec![0xba, 0x00, 0x04, 0x00, 0x00, 0xb1];
        assert_eq!(classify_init_complexity(&bc), InitComplexity::Complex);
    }

    #[test]
    fn classify_complex_synchronized_ctor() {
        // aload_0; monitorenter; aload_0; monitorexit; return
        let bc = vec![0x2a, 0xc2, 0x2a, 0xc3, 0xb1];
        assert_eq!(classify_init_complexity(&bc), InitComplexity::Complex);
    }

    #[test]
    fn classify_handles_iinc_three_byte_form() {
        // iinc 0, 1; return  — three-byte form, no disqualifiers
        let bc = vec![0x84, 0x00, 0x01, 0xb1];
        assert_eq!(classify_init_complexity(&bc), InitComplexity::Trivial);
    }

    #[test]
    fn classify_handles_wide_iinc() {
        // wide iinc index2 const2; return — six bytes
        let bc = vec![0xc4, 0x84, 0x00, 0x00, 0x00, 0x01, 0xb1];
        assert_eq!(classify_init_complexity(&bc), InitComplexity::Trivial);
    }

    #[test]
    fn trivial_ctor_skips_constructor_ban() {
        let trivial_bc = vec![0x2a, 0xb7, 0x00, 0x01, 0xb1];
        let comp = classify_init_complexity(&trivial_bc);
        assert_eq!(comp, InitComplexity::Trivial);
        assert_eq!(
            should_skip_jit_with_init(
                "Foo",
                "<init>",
                false,
                true,
                SkipPolicy::Aggressive,
                &[],
                comp,
            ),
            None,
            "trivial constructors must be JIT-eligible (T1.1.f)"
        );
    }

    /// A genuinely complex constructor still keeps the historical ban.
    ///
    /// The bytecode here used to be `putfield`-only, which stopped being
    /// Complex when `allow_putfield_init` flipped on 2026-07-28 — so the test
    /// was asserting the ban against a constructor the VM had deliberately
    /// made eligible. It now uses `monitorenter`/`monitorexit`, a disqualifier
    /// the flag does not govern, so it tests the ban rather than the flag.
    /// The `putfield` side is covered by
    /// [`putfield_only_ctor_is_jit_eligible_by_default`].
    #[test]
    fn complex_ctor_keeps_constructor_ban() {
        // aload_0; monitorenter; aload_0; monitorexit; return
        let complex_bc = vec![0x2a, 0xc2, 0x2a, 0xc3, 0xb1];
        let comp = classify_init_complexity(&complex_bc);
        assert_eq!(comp, InitComplexity::Complex);
        assert_eq!(
            should_skip_jit_with_init(
                "Foo",
                "<init>",
                false,
                true,
                SkipPolicy::Aggressive,
                &[],
                comp,
            ),
            Some(SkipReason::Constructor),
            "complex constructors keep the historical ban"
        );
    }

    /// The point of lifting `allow_putfield_init`: a constructor whose whole
    /// body is field stores must reach the JIT. `classify_init_complexity`
    /// answering `Trivial` is only half of it — this pins the end-to-end
    /// result through `should_skip_jit_with_init`, which is what the compiler
    /// actually consults.
    #[test]
    fn putfield_only_ctor_is_jit_eligible_by_default() {
        if !super::allow_putfield_init() {
            return; // CRATONVM_JIT_PUTFIELD_INIT=0 in this environment
        }
        // aload_0; aload_0; iconst_1; putfield #2; return
        let bc = vec![0x2a, 0x2a, 0x04, 0xb5, 0x00, 0x02, 0xb1];
        let comp = classify_init_complexity(&bc);
        assert_eq!(comp, InitComplexity::Trivial);
        assert_eq!(
            should_skip_jit_with_init(
                "Foo",
                "<init>",
                false,
                true,
                SkipPolicy::Aggressive,
                &[],
                comp,
            ),
            None,
            "a field-storing constructor must be JIT-eligible (allow_putfield_init, 2026-07-28)"
        );
    }

    #[test]
    fn unknown_complexity_keeps_ban_default() {
        // Legacy callers pass Unknown and get the original behavior.
        assert_eq!(
            should_skip_jit_with_init(
                "Foo",
                "<init>",
                false,
                true,
                SkipPolicy::Aggressive,
                &[],
                InitComplexity::Unknown,
            ),
            Some(SkipReason::Constructor),
        );
    }

    #[test]
    fn legacy_should_skip_jit_unchanged() {
        // The legacy `should_skip_jit` (without complexity) still bans
        // every <init>, preserving callers that haven't migrated.
        assert_eq!(
            should_skip_jit("Foo", "<init>", false, true, SkipPolicy::Aggressive, &[]),
            Some(SkipReason::Constructor),
        );
    }

    // =================================================================
    // T1.1.38 — CI gate: blanket package bans must never return.
    //
    // These tests fail the build if anyone reintroduces a blanket
    // `starts_with("java/util/")` or `starts_with("cratonvm/")` ban.
    // A targeted (class, method) list is the only permitted mechanism for
    // banning specific methods going forward -- and it must be one that is
    // actually reachable: the historical `is_known_miscompile` list sat
    // behind a private, default-off gate for weeks before being deleted
    // 2026-07-27 (docs/internal/is-known-miscompile-block-retired-20260727.md).
    // =================================================================

    #[test]
    fn bytebuddy_package_is_jit_eligible_after_full_hibernate_closure() {
        // The first entry is the exact method named by the historical
        // mid-body instruction-fetch crash. The other two are the original
        // 2026-06-13 hang witnesses. None may be hidden by a blanket package
        // guard again.
        for policy in [SkipPolicy::Conservative, SkipPolicy::Aggressive] {
            for (class, method) in [
                (
                    "net/bytebuddy/description/ModifierReviewable$AbstractBase",
                    "matchesMask",
                ),
                (
                    "net/bytebuddy/description/type/TypeDescription",
                    "represents",
                ),
                (
                    "net/bytebuddy/description/type/TypeDefinition$Sort",
                    "describe",
                ),
            ] {
                assert_eq!(
                    check(class, method, false, true, policy),
                    None,
                    "HIB-BYTEBUDDY closure gate: {class}.{method} must remain JIT-eligible"
                );
            }
        }
    }

    #[test]
    fn tier1_skip_list_no_blanket_java_util_ban() {
        // 10 representative java/util methods that are NOT in the
        // targeted miscompile list. Under BOTH policies, all must
        // return None (JIT-eligible). If any returns Some, a blanket
        // ban was reintroduced.
        let methods = [
            ("java/util/ArrayList", "add"),
            ("java/util/ArrayList", "get"),
            ("java/util/ArrayList", "size"),
            ("java/util/LinkedList", "addFirst"),
            ("java/util/TreeMap", "put"),
            ("java/util/HashSet", "contains"),
            ("java/util/HashMap", "size"),
            ("java/util/HashMap", "containsKey"),
            ("java/util/Arrays", "sort"),
            ("java/util/Collections", "unmodifiableList"),
        ];
        for (cls, meth) in &methods {
            assert_eq!(
                check(cls, meth, false, true, SkipPolicy::Conservative),
                None,
                "T1.1.38 GATE: {cls}.{meth} must be JIT-eligible under Conservative — \
                 a blanket java/util/* ban was reintroduced"
            );
            assert_eq!(
                check(cls, meth, false, true, SkipPolicy::Aggressive),
                None,
                "T1.1.38 GATE: {cls}.{meth} must be JIT-eligible under Aggressive"
            );
        }
    }

    #[test]
    fn stream_matchops_uncommon_trap_stays_interpreted() {
        // ES-PERF-20260719: makeInt/makeRef/makeLong/makeDouble deopt on
        // literally every call once JIT-compiled (an internal indy site
        // lowers to an unconditional uncommon trap) — must stay skipped
        // unconditionally, under both policies, regardless of allow-packages.
        for meth in ["makeInt", "makeRef", "makeLong", "makeDouble"] {
            assert_eq!(
                check(
                    "java/util/stream/MatchOps",
                    meth,
                    false,
                    true,
                    SkipPolicy::Conservative
                ),
                Some(SkipReason::StreamMatchOpsUncommonTrap)
            );
            assert_eq!(
                check(
                    "java/util/stream/MatchOps",
                    meth,
                    false,
                    true,
                    SkipPolicy::Aggressive
                ),
                Some(SkipReason::StreamMatchOpsUncommonTrap)
            );
            assert_eq!(
                check_with(
                    "java/util/stream/MatchOps",
                    meth,
                    false,
                    true,
                    SkipPolicy::Conservative,
                    &["java/util/"],
                ),
                Some(SkipReason::StreamMatchOpsUncommonTrap),
                "must not be liftable via CRATONVM_JIT_ALLOW_PACKAGES=java/util/"
            );
        }
        // A sibling MatchOps method not in the targeted list stays eligible.
        assert_eq!(
            check(
                "java/util/stream/MatchOps$MatchKind",
                "values",
                false,
                true,
                SkipPolicy::Conservative
            ),
            None
        );
    }

    #[test]
    fn tier1_skip_list_no_blanket_cratonvm_ban() {
        // Representative cratonvm/* methods NOT in the targeted list.
        let methods = [
            ("cratonvm/TckLang", "str_length"),
            ("cratonvm/TckLang", "obj_hashCode_consistent"),
            ("cratonvm/TckIo", "readLine"),
            ("cratonvm/Other", "anything"),
        ];
        for (cls, meth) in &methods {
            assert_eq!(
                check(cls, meth, false, true, SkipPolicy::Conservative),
                None,
                "T1.1.38 GATE: {cls}.{meth} must be JIT-eligible — \
                 a blanket cratonvm/* ban was reintroduced"
            );
        }
    }

    #[test]
    fn tier1_skip_list_former_targeted_entries_are_jit_eligible() {
        // This test used to police the membership of `is_known_miscompile`.
        // That list was inert (private default-off gate) and was deleted
        // 2026-07-27, so what is worth pinning now is the OUTCOME: every
        // method it used to name is JIT-eligible under both policies, and
        // nothing quietly re-banned them through some other route. Sample one
        // entry from each family the list carried.
        for (cls, method) in [
            ("java/util/HashMap", "put"),
            ("java/util/HashMap", "get"),
            ("java/util/HashMap", "resize"),
            ("java/util/LinkedHashMap", "newNode"),
            ("java/util/WeakHashMap$ValueSpliterator", "tryAdvance"),
            ("java/lang/String", "toLowerCase"),
            ("java/lang/Integer", "parseInt"),
            ("java/security/Provider", "putService"),
            ("java/util/concurrent/LinkedBlockingQueue", "offer"),
            ("java/util/Calendar", "isFieldSet"),
            ("java/io/BufferedInputStream", "read1"),
            ("sun/reflect/generics/parser/SignatureParser", "parseTypeSignature"),
            (
                "org/eclipse/jdt/internal/compiler/util/HashtableOfInt",
                "rehash",
            ),
            (
                "org/keycloak/models/credential/dto/PasswordCredentialData",
                "getAdditionalParameters",
            ),
            (
                "org/keycloak/models/credential/dto/PasswordSecretData",
                "getAdditionalParameters",
            ),
            (
                "io/smallrye/config/ConfigValueConfigSource$ConfigValueProperties",
                "load0",
            ),
            ("org/springframework/util/ObjectUtils", "nullSafeEquals"),
            ("org/apache/tomcat/util/buf/MessageBytes", "newInstance"),
            ("groovy/lang/GroovyClassLoader", "doParseClass"),
            // Never-banned neighbours, unchanged.
            ("java/util/HashMap", "size"),
            ("java/util/ArrayList", "add"),
            ("cratonvm/TckLang", "str_length"),
            ("cratonvm/TckLang", "exc_hierarchy"),
            ("com/example/Foo", "bar"),
        ] {
            for policy in [SkipPolicy::Conservative, SkipPolicy::Aggressive] {
                assert_eq!(
                    check(cls, method, false, true, policy),
                    None,
                    "{cls}.{method} must be JIT-eligible after the \
                     is_known_miscompile removal"
                );
            }
        }

        // The three targeted lists that were deliberately KEPT (they are
        // unconditional, and were never part of the deleted block's family)
        // must still bite, or this removal silently widened its own scope.
        assert_eq!(
            check(
                "java/util/Objects",
                "hash",
                false,
                true,
                SkipPolicy::Conservative
            ),
            Some(SkipReason::JavaUtilCollection),
            "is_unconditional_hash_miscompile_cluster must survive"
        );
        assert_eq!(
            check(
                "java/util/concurrent/locks/ReentrantLock",
                "lock",
                false,
                true,
                SkipPolicy::Conservative
            ),
            Some(SkipReason::JavaUtilCollection),
            "is_known_miscompile_aqs_family must survive"
        );
        assert_eq!(
            check(
                "java/util/concurrent/ConcurrentLinkedQueue",
                "offer",
                false,
                true,
                SkipPolicy::Conservative
            ),
            Some(SkipReason::JavaUtilCollection),
            "is_known_miscompile_clq_family must survive"
        );
    }

    #[test]
    fn bouncycastle_is_jit_eligible_after_rbc1_removed() {
        // RBC.1's org/bouncycastle/ blanket ban was commented out
        // 2026-07-28 (unverified, explicit user decision -- only
        // tomcat/hibernate/spring/spring-boot/h2 need to work right now).
        // The carveout function (`is_bouncycastle_crypto_hotpath_carveout`)
        // used to make a FEW methods JIT-eligible despite the broader ban;
        // now that the ban itself is gone, EVERY bouncycastle method is
        // JIT-eligible, including the ones the carveout used to exempt
        // AND the ones that used to stay banned (e.g. CAST5Engine.init).
        for (class_name, method_name) in [
            ("org/bouncycastle/math/ec/ECPoint", "normalize"),
            ("org/bouncycastle/crypto/BufferedBlockCipher", "getUpdateOutputSize"),
            ("org/bouncycastle/crypto/DefaultBufferedBlockCipher", "getUpdateOutputSize"),
            ("org/bouncycastle/crypto/engines/CAST5Engine", "init"),
        ] {
            assert_eq!(
                check(class_name, method_name, false, true, SkipPolicy::Conservative),
                None,
                "{class_name}.{method_name} must be JIT-eligible now that RBC.1 is commented out"
            );
        }
    }

    #[test]
    fn tomcat_dohead_junit_iterator_is_jit_eligible_after_removal() {
        // TOMCAT-DOHEAD-JUNIT-ITERATOR.1's own specific check was removed
        // 2026-07-26. Until 2026-07-27 the class nonetheless stayed
        // interpreted under Conservative, because the separate, unrelated
        // blanket "org/junit/" ban caught it first; that ban has since been
        // removed too (see the TEST-HARNESS BLANKET BANS comment in
        // should_skip_jit_internal), so the removal is now observable under
        // BOTH policies with no allow-list needed.
        let class_name = "org/junit/runners/model/TestClass";
        let method_name = "collectAnnotatedMethodValues";

        for policy in [SkipPolicy::Conservative, SkipPolicy::Aggressive] {
            assert_eq!(
                check(class_name, method_name, false, true, policy),
                None,
                "TOMCAT-DOHEAD-JUNIT-ITERATOR.1 (removed 2026-07-26) and the blanket org/junit/ ban (removed 2026-07-27) must both be gone under {policy:?}",
            );
        }
    }

    #[test]
    fn blanket_test_harness_package_bans_are_removed() {
        // The four undocumented blanket bans removed 2026-07-27. They were
        // Conservative-only, so Conservative is the policy that actually
        // proves they are gone.
        for class_name in [
            "org/junit/runner/JUnitCore",
            "org/junit/runners/ParentRunner",
            "junit/textui/ResultPrinter",
            "org/apache/logging/log4j/core/Logger",
            "com/carrotsearch/randomizedtesting/RandomizedRunner",
        ] {
            assert_eq!(
                check(class_name, "run", false, true, SkipPolicy::Conservative),
                None,
                "{class_name} must be JIT-eligible: the blanket test-harness package bans were removed 2026-07-27",
            );
        }

        // UPDATE 2026-07-28: PIC.1 itself was ALSO commented out (unverified,
        // explicit user decision -- only tomcat/hibernate/spring/spring-boot/h2
        // need to work right now; the JUnit Platform console-standalone
        // launcher tool this ban protected is none of those five), so the
        // shadowed picocli copy is now JIT-eligible too.
        assert_eq!(
            check(
                "org/junit/platform/console/shadow/picocli/CommandLine$Model$OptionSpec",
                "equals",
                false,
                true,
                SkipPolicy::Conservative,
            ),
            None,
            "PIC.1 was commented out 2026-07-28; the shadowed picocli copy must now be JIT-eligible",
        );
    }

    #[test]
    fn javac_tool_get_task_is_jit_eligible_after_spring_testcompiler_1_removal() {
        for policy in [SkipPolicy::Conservative, SkipPolicy::Aggressive] {
            assert_eq!(
                check(
                    "com/sun/tools/javac/api/JavacTool",
                    "getTask",
                    false,
                    true,
                    policy,
                ),
                None,
                "JavacTool.getTask must be JIT-eligible now that SPRING-TESTCOMPILER.1 is removed",
            );
        }
    }

    #[test]
    fn types_erasure_is_jit_eligible_after_types_erasure_1_removal() {
        for policy in [SkipPolicy::Conservative, SkipPolicy::Aggressive] {
            assert_eq!(
                check(
                    "com/sun/tools/javac/code/Types",
                    "erasure",
                    false,
                    true,
                    policy,
                ),
                None,
                "Types.erasure must be JIT-eligible now that TYPES-ERASURE.1 is removed",
            );
        }
    }

    #[test]
    fn spring_boot_condition_report_mapping_lambda_is_jit_eligible_after_removal() {
        for policy in [SkipPolicy::Conservative, SkipPolicy::Aggressive] {
            assert_eq!(
                check(
                    "org/springframework/boot/autoconfigure/condition/ConditionEvaluationReport",
                    "lambda$recordConditionEvaluation$0",
                    false,
                    true,
                    policy,
                ),
                None,
                "the ConditionEvaluationReport mapping lambda must be JIT-eligible now that SPRINGBOOT-CONDITION-REPORT.1 is removed",
            );
        }
    }

    #[test]
    fn spring_boot_jdk_http_header_comparator_lambda_is_jit_eligible_after_removal() {
        for policy in [SkipPolicy::Conservative, SkipPolicy::Aggressive] {
            assert_eq!(
                check(
                    "org/springframework/http/client/JdkClientHttpRequest",
                    "lambda$buildRequest$0",
                    false,
                    true,
                    policy,
                ),
                None,
                "the JDK HTTP request header comparator lambda must be JIT-eligible now that SPRINGBOOT-HTTP-HEADER-COMPARATOR.1 is removed",
            );
        }
    }

    #[test]
    fn spring_annotated_metadata_attribute_collector_is_jit_eligible_after_removal() {
        for policy in [SkipPolicy::Conservative, SkipPolicy::Aggressive] {
            assert_eq!(
                check(
                    "org/springframework/core/type/AnnotatedTypeMetadata",
                    "getAllAnnotationAttributes",
                    false,
                    true,
                    policy,
                ),
                None,
                "the annotation metadata collector must be JIT-eligible now that SPRINGBOOT-ANNOTATED-METADATA-COLLECTOR.1 is removed",
            );
        }
    }

    /// SPR-PROXY.1 (2026-07-28) re-banned `java.lang.reflect.Proxy`-generated
    /// classes after PROXY-JITCALL.1 had removed the older ban, and this test
    /// — which asserted the gap between the two — was left behind.
    ///
    /// The newer ban is the one that stands, and it is not a tuning choice:
    /// CratonVM implements proxy semantics at DISPATCH (`vm_exec.rs`'s
    /// `proxy_invoke_handler_shared` / `proxy_annotation_handler_invoke` /
    /// `annotation_proxy_dispatch_impl`), so a compiled proxy body is a third
    /// dispatch path that bypasses annotation-member coercion and the
    /// foreign-proxy `equals` delegation. See `is_jdk_dynamic_proxy_class` for
    /// the OOM that pinned it. Deliberately not liftable by
    /// `CRATONVM_JIT_ALLOW_PACKAGES`, hence "under every policy".
    #[test]
    fn generated_proxy_class_is_never_jit_eligible_spr_proxy_1() {
        for policy in [SkipPolicy::Conservative, SkipPolicy::Aggressive] {
            for (class, method) in [
                // JDK 9+ names: one package per defining loader...
                ("jdk/proxy1/$Proxy0", "invoke"),
                ("jdk/proxy3/$Proxy17", "equals"),
                // ...and unnamed-module proxies for non-public interfaces sit
                // in the interface's own package.
                ("com/example/$Proxy0", "invoke"),
                ("com/example/$Proxy42", "hashCode"),
            ] {
                assert_eq!(
                    check(class, method, false, true, policy),
                    Some(SkipReason::JdkDynamicProxyTrampoline),
                    "{class}.{method} must stay interpreted under {policy:?} (SPR-PROXY.1)",
                );
            }
        }
    }

    /// The guard is on the `$ProxyN` naming rule, not on "contains Proxy":
    /// an application class that merely has `Proxy` in its name is ordinary
    /// code and must not be swept up by SPR-PROXY.1.
    #[test]
    fn ordinary_classes_named_proxy_are_not_caught_by_spr_proxy_1() {
        for class in [
            "com/example/ProxyFactory",
            "org/springframework/aop/framework/JdkDynamicAopProxy",
            "com/example/Proxy0",
        ] {
            assert_eq!(
                check(class, "invoke", false, true, SkipPolicy::Aggressive),
                None,
                "{class} is not a generated proxy class",
            );
        }
    }

    #[test]
    fn tomcat_keyedlock_compute_lambda_stays_skipped_under_conservative() {
        assert_eq!(
            check(
                "org/apache/tomcat/util/concurrent/KeyedReentrantReadWriteLock$LockImpl",
                "lambda$lock$0",
                false,
                true,
                SkipPolicy::Conservative,
            ),
            Some(SkipReason::RustJvmTestFixture),
            "KeyedReentrantReadWriteLock$LockImpl.lambda$lock$0 must stay skipped \
             under Conservative",
        );
    }

    /// Regression witness for the 2026-07-30 removal of the remaining
    /// javac-family bans (`SPRING-TESTCOMPILER.2`/`.3`/`.4`): the four methods
    /// that had no dedicated eligibility test of their own must now compile.
    #[test]
    fn javac_family_residual_methods_are_jit_eligible_after_removal() {
        for policy in [SkipPolicy::Conservative, SkipPolicy::Aggressive] {
            for (class_name, method_name) in [
                ("com/sun/tools/javac/jvm/ClassReader", "readClass"),
                ("com/sun/tools/javac/jvm/ClassReader", "readInnerClasses"),
                ("com/sun/tools/javac/jvm/ClassReader", "readAttrs"),
                ("com/sun/tools/javac/code/ClassFinder", "fillIn"),
                ("org/springframework/javapoet/CodeBlock$Builder", "add"),
            ] {
                assert_eq!(
                    check(class_name, method_name, false, true, policy),
                    None,
                    "{class_name}.{method_name} must be JIT-eligible now that the \
                     javac-family bans are removed",
                );
            }
        }
    }

    #[test]
    fn javac_class_symbol_complete_is_jit_eligible_after_hib_storedproc_jit_1_removal() {
        for policy in [SkipPolicy::Conservative, SkipPolicy::Aggressive] {
            assert_eq!(
                check(
                    "com/sun/tools/javac/code/Symbol$ClassSymbol",
                    "complete",
                    false,
                    true,
                    policy,
                ),
                None,
                "Javac ClassSymbol.complete must be JIT-eligible now that HIB-STOREDPROC-JIT.1 is removed",
            );
        }
    }
}
