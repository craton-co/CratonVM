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
pub fn classify_init_complexity(bytecode: &[u8]) -> InitComplexity {
    use std::cmp::min;
    let mut pc = 0usize;
    while pc < bytecode.len() {
        let op = bytecode[pc];
        match op {
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

    // Bisection hook (development only): `CRATONVM_JIT_BISECT_SKIP` is a
    // comma-separated list of `Class.method` entries (slash-separated
    // class names, e.g. `java/util/Locale.hashCode`). Any listed method
    // is forced to skip the JIT. Used to binary-search a miscompiling
    // method without recompiling.
    {
        use std::sync::OnceLock;
        static BISECT: OnceLock<Vec<(String, String)>> = OnceLock::new();
        let list = BISECT.get_or_init(|| {
            std::env::var("CRATONVM_JIT_BISECT_SKIP")
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
            std::env::var("CRATONVM_JIT_BISECT_ONLY").ok().map(|s| {
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

    // PROXY-JITCALL.1 — generated `$ProxyN` dynamic-proxy classes (any
    // package — `define_or_get_proxy_class` places a package-private
    // interface's proxy in the interface's OWN package, so this can't be a
    // package-prefix check) SIGSEGV/hang when JIT-compiled together with
    // methods on the OTHER side of the `Proxy$Dispatch.invokeProxy` call —
    // e.g. Spring's `org/springframework/core/annotation/*` meta-annotation
    // introspection calling `annotationType()`/`hashCode()`/`equals()`
    // repeatedly on many distinct `$ProxyN` receiver classes under
    // `CRATONVM_REAL_ANNOTATIONS=1`. Confirmed via bisection
    // (`CRATONVM_JIT_BISECT_ONLY`): `jdk/proxy` alone is clean, the Spring
    // annotation package alone is clean, but JIT-compiling BOTH sides
    // together SIGSEGVs (rc=139, read near address 0x1/0x18 — a garbage
    // register value used as a pointer) or hangs — a JIT→JIT call-boundary
    // register-preservation bug in the same family as
    // `docs/internal/jit-regalloc-callee-saved-clobber-family.md`, but NOT
    // covered by that family's `is_known_miscompile` targeted list (which is
    // gated behind `callee_saved_gpr_local_homes_enabled()`, default OFF as
    // of the 2026-07-04 fix — so this residual case has no existing safety
    // net). Checked unconditionally (not gated by policy or the GPR-homes
    // flag) because the generated proxy method bodies are a handful of
    // bytecodes (marshal args, box, call, unbox, return) — the JIT gains
    // essentially nothing compiling them, so banning them outright is safe.
    // `getInterfaces`/superclass access on a NON-generated class named
    // e.g. `$ProxyHelper` by a user is not affected — the check requires the
    // exact `$Proxy<digits>` simple-name shape `emit_proxy_classfile` emits.
    if is_generated_proxy_class(class_name) {
        return Some(SkipReason::RustJvmTestFixture);
    }

    // SPR-AOT-TESTNG-MAPS.1 (2026-07-08) — TestNG's Maps helper is a set
    // of tiny allocation factories. JIT-compiling Maps.newConcurrentMap()
    // can hand ClassMethodMap a malformed/empty map path that later makes
    // computeIfAbsent appear to return null. Keep this tiny helper interpreted.
    if class_name == "org/testng/collections/Maps" {
        return Some(SkipReason::RustJvmTestFixture);
    }

    // REACTOR-ADDCAP.1 (2026-07-09) — Reactor's demand accounting helper
    // `Operators.addCap(...)` is tiny but correctness-critical. Under Craton's
    // optimized JIT it can corrupt requested-count bookkeeping in WebSocket
    // send chains: after several Reactor publisher pipelines, Jetty client
    // sends stall reproducibly at 84/100 messages while the interpreter and
    // HotSpot complete. Keep both overloads interpreted until the JIT long/CAS
    // lowering bug is fixed.
    if class_name == "reactor/core/publisher/Operators" && method_name == "addCap" {
        return Some(SkipReason::RustJvmTestFixture);
    }
    // REACTOR-FLUXCREATE.1 (2026-07-09) — the WebFlux websocket integration
    // sequence still lost demand after `Operators.addCap` was pinned. Runtime
    // bisection narrowed the remaining Reactor-side poison to the FluxCreate
    // sink accounting/drain pair below. Leaving them interpreted fixes the
    // ReactorNetty/JettyCore echo stall without disabling broader Reactor JIT.
    if class_name == "reactor/core/publisher/FluxCreate$BaseSink" && method_name == "addCap" {
        return Some(SkipReason::RustJvmTestFixture);
    }
    if class_name == "reactor/core/publisher/FluxCreate$BufferAsyncSink" && method_name == "drain" {
        return Some(SkipReason::RustJvmTestFixture);
    }
    // JETTY-WSIO.1 (2026-07-09) — Jetty websocket large-payload receives need
    // the websocket core and IO packages to be compiled together to reproduce:
    // each subpackage alone is clean, but `CRATONVM_JIT_BISECT_ONLY=org/eclipse/jetty/`
    // times out all Jetty-client large-payload combinations after the normal
    // websocket method warmup. Exact env skipping of every compiled
    // `org/eclipse/jetty/{websocket,io}/...` method (113 entries in the probe)
    // clears the timeout, so keep this interaction interpreted until the shared
    // IO/websocket lowering bug is fixed.
    if class_name.starts_with("org/eclipse/jetty/websocket/")
        || class_name.starts_with("org/eclipse/jetty/io/")
    {
        return Some(SkipReason::RustJvmTestFixture);
    }

    // T1.1.g — the historical blanket bans for `java/util/*` and
    // `cratonvm/*` were narrowed to targeted per-method exclusions.
    // Those targeted exclusions guarded the callee-saved-GPR local-home
    // regalloc family. The x64 backend now keeps those GPR local homes
    // default-off, so the methods are JIT-eligible again on that safe
    // path. If a developer opts back into the old register homes for
    // diagnosis, or if a non-x64 backend has not installed an equivalent
    // guard, keep the targeted list active.
    //
    // The conservative policy applies the targeted list only when the legacy
    // GPR local-home allocator is explicitly enabled. The aggressive policy
    // (set via `jit_aggressive_compilation` or `CRATONVM_JIT_ALLOW_PACKAGES`)
    // still lifts the targeted list so developers can surface new miscompiles.
    // DBG bypass: force-compile JUnitCore.main despite the JUNIT.1 stopgap ban,
    // so its emitted code can be dumped/diagnosed. Default-off; the ban holds in
    // normal runs.
    if class_name == "org/junit/runner/JUnitCore"
        && method_name == "main"
        && std::env::var_os("CRATONVM_JIT_UNBAN_JUNITCORE").is_some()
    {
        return None;
    }

    if policy == SkipPolicy::Conservative {
        if is_unconditional_hash_miscompile_cluster(class_name, method_name)
            && !package_allowed(class_name, allow_packages)
        {
            return Some(SkipReason::JavaUtilCollection);
        }

        // JASPER-JDT.2 (2026-07-08) - default Tomcat
        // `org.apache.jasper.compiler.TestCompiler` order corrupts Eclipse JDT
        // parser state under JIT and then fails JSP compilation. The first
        // stable face was `ArrayIndexOutOfBoundsException: Index -1 out of
        // bounds for length 100` in `Parser.parse`: `--nojit` passed, running
        // `testBug55262` alone passed, and package bisection showed
        // `CRATONVM_JIT_BISECT_ONLY=org/eclipse/jdt/internal/compiler/parser/`
        // still failed while `CRATONVM_JIT_BISECT_SKIP=.../Parser.consumeRule`
        // made the two-method repro pass. After rebasing onto newer `dev`, the
        // full class still produced nondeterministic parser-adjacent heap
        // corruption/OOM around the `testBug53257*` sequence unless the parser
        // package was interpreted. The affected generated parser methods
        // (`consumeRule`, `consumeBlock`, `consumeTypeImportOnDemandDeclarationName`,
        // and siblings) share the same huge switch/stack update shape, so keep
        // the parser package interpreted under Conservative until the backend
        // producer is root-caused. Liftable for diagnosis with
        // `CRATONVM_JIT_ALLOW_PACKAGES=org/eclipse/jdt/internal/compiler/parser/`.
        if class_name.starts_with("org/eclipse/jdt/internal/compiler/parser/")
            && !package_allowed("org/eclipse/jdt/internal/compiler/parser/", allow_packages)
        {
            return Some(SkipReason::RustJvmTestFixture);
        }

        // JASPER-JDT.3 (2026-07-10) - a second, independent Eclipse JDT
        // miscompile family, this one in the AST/flow-analysis package
        // rather than JASPER-JDT.2's parser package. Real Tomcat FORM-auth
        // repro (`TestFormAuthenticatorA/B/C` forwarding to the login-page
        // JSP): `Servlet.service()` intermittently threw `JasperException:
        // Unable to compile class for JSP` with root cause
        // `ArrayIndexOutOfBoundsException: Index 1 out of bounds for
        // length 1` — reported stack frame was
        // `QualifiedNameReference.analyseCode(QualifiedNameReference.java:170)`,
        // which is JUST a trivial 3-arg-to-4-arg delegating wrapper
        // (`return analyseCode(scope, ctx, info, true);`, no array access
        // of its own) — i.e. the JIT lost/mis-attributed the inlined
        // callee's own frame, the same symptom shape as JASPER-JDT.2's
        // "size varies run to run" AIOOBEs. `--nojit` never reproduces (0/8
        // hits across repeated full-class runs vs. consistent hits with JIT
        // on); `CRATONVM_JIT_BISECT_SKIP=.../QualifiedNameReference.analyseCode`
        // alone eliminates it (confirmed clean across 3 repeat runs). Not
        // yet root-caused to a specific backend bug (unlike JASPER-JDT.2's
        // three fully-diagnosed getfield/deopt/arraycopy bugs) — the AST
        // package's many `analyseCode` overrides likely share a similar
        // "small final-array-length loop across an inlined overload
        // boundary" shape, so interpret the whole `ast` package rather than
        // just this one class, mirroring JASPER-JDT.2's package-wide scope.
        // Liftable for diagnosis with
        // `CRATONVM_JIT_ALLOW_PACKAGES=org/eclipse/jdt/internal/compiler/ast/`.
        if class_name.starts_with("org/eclipse/jdt/internal/compiler/ast/")
            && !package_allowed("org/eclipse/jdt/internal/compiler/ast/", allow_packages)
        {
            return Some(SkipReason::RustJvmTestFixture);
        }
        if is_elasticsearch_suite_jit_fragile_cluster(class_name, method_name)
            && !package_allowed(class_name, allow_packages)
        {
            return Some(SkipReason::RustJvmTestFixture);
        }

        // ES-HAMCREST.1 (2026-07-09) - after the Elasticsearch FFM
        // `System$1.findNative` bridge fix, `ClusterShardHealthTests` reaches
        // its real test body under JIT but Hamcrest's equality matcher reports
        // equal-looking boxed hash values as unequal. `--nojit` passes, and
        // package bisection (`CRATONVM_JIT_BISECT_ONLY=org/hamcrest/`) keeps the
        // failure while JUnit/randomizedtesting/java.lang-only runs pass. Keep
        // Hamcrest interpreted under Conservative until the matcher-codegen
        // producer is narrowed. Liftable for bisection with
        // `CRATONVM_JIT_ALLOW_PACKAGES=org/hamcrest/`.
        if class_name.starts_with("org/hamcrest/")
            && !package_allowed("org/hamcrest/", allow_packages)
        {
            return Some(SkipReason::RustJvmTestFixture);
        }

        // WILDFLY-CONTROLLER-JIT.1 (2026-07-13): the optimized
        // invokespecial path skipped AbstractOperationContext.<init> while
        // constructing OperationContextImpl. Its controllerOperations list
        // remained null and parallel EJB boot failed; the same standalone boot
        // reaches past that point with CRATONVM_DISABLE_JIT=1. Keep controller
        // bytecode interpreted until the special-call backend is corrected.
        // Liftable with CRATONVM_JIT_ALLOW_PACKAGES=org/jboss/as/controller/.
        if class_name.starts_with("org/jboss/as/controller/")
            && !package_allowed("org/jboss/as/controller/", allow_packages)
        {
            return Some(SkipReason::RustJvmTestFixture);
        }

        // JSONSMART-PARSER.1 (2026-07-09) - Spring's JsonPathResultMatchersTests
        // now reach json-smart parsing after the EnumSet bridge fix. The
        // interpreter is correct (43/43), but the default JIT crashes inside
        // emitted code after compiling parser cursor methods such as
        // JSONParserString.read(), JSONParserString.readS(), and
        // JSONParserBase.skipSpace(). Keep this small parser package
        // interpreted under Conservative until the x64 lowering issue is
        // narrowed. Liftable for diagnosis with
        // CRATONVM_JIT_ALLOW_PACKAGES=net/minidev/json/parser/.
        if class_name.starts_with("net/minidev/json/parser/")
            && !package_allowed("net/minidev/json/parser/", allow_packages)
        {
            return Some(SkipReason::RustJvmTestFixture);
        }
        // HIB-TEMPORAL.1 (2026-07-08) - Hibernate temporal suite residuals.
        // The `InstantTests` failure cluster was not a Hibernate data bug: with
        // default JIT, `DdlTypeImpl.getRawTypeName` saw a null `typeNamePattern`
        // during `TIMESTAMP_UTC` DDL descriptor registration (37 failures). A cold
        // standalone `H2Dialect.columnType(3003)` probe matched HotSpot, but
        // `CRATONVM_TIER_ENABLED=0` removed the type-name-pattern signature, so
        // this is a JIT-only corruption in the Hibernate DDL/type hot path.
        //
        // Narrow bisection found `org/hibernate/type/descriptor/sql/internal/`
        // as the DDL NPE face, but that exposed intermittent empty-message
        // `IllegalThreadStateException`, H2 connection `<local4>` NPE, and
        // `Object.{test,apply}` NoSuchMethodError failures in adjacent temporal
        // runs. The stable control is interpreting Hibernate bytecode as a
        // package (repeat `CRATONVM_JIT_DENY=org/hibernate/` runs: 0 failures in
        // `InstantTests`), matching the existing conservative third-party
        // fail-closed guards in this file. The helper accepts both slash and
        // dotted class-name spellings because the JIT eligibility path can see
        // either shape depending on the caller. Liftable for bisection with
        // `CRATONVM_JIT_ALLOW_PACKAGES=org/hibernate/` or `org.hibernate.` once
        // the underlying JIT producer is narrowed.
        if let Some(prefix) = hibernate_temporal_residual_skip_prefix(class_name) {
            if !package_allowed(prefix, allow_packages) {
                return Some(SkipReason::RustJvmTestFixture);
            }
        }

        // Hibernate mapping metadata initializes JAXB's QName-heavy runtime
        // graph.  JITting org.glassfish.jaxb currently corrupts that graph and
        // produces a self-cast `QName cannot be cast to QName`; interpreting
        // the package reproduces the no-JIT result.  Keep this scoped guard
        // liftable for bisection.
        if let Some(prefix) = jaxb_mapping_residual_skip_prefix(class_name) {
            if !package_allowed(prefix, allow_packages) {
                return Some(SkipReason::RustJvmTestFixture);
            }
        }

        // ES-JIT-DEOPT-GC.1 (2026-07-08) - Elasticsearch interval-provider
        // tests crash under JIT while serializing through Jackson YAML. Package
        // bisection narrowed the producer from org/yaml/snakeyaml/emitter/ to
        // Emitter.emit(Event): interpreting only this dispatcher changes the
        // three interval classes from rc=139 SIGSEGV to ordinary JUnit failures,
        // while a high JIT threshold and --nojit show the same non-crash shape.
        // Keep the tiny dispatcher interpreted under Conservative until the
        // emitter-state codegen defect is root-caused. Liftable with
        // CRATONVM_JIT_ALLOW_PACKAGES=org/yaml/snakeyaml/emitter/.
        if is_snakeyaml_emitter_emit_jit_corruption(class_name, method_name)
            && !package_allowed("org/yaml/snakeyaml/emitter/", allow_packages)
        {
            return Some(SkipReason::RustJvmTestFixture);
        }
        if callee_saved_gpr_local_homes_enabled()
            && is_known_miscompile(class_name, method_name)
            && !package_allowed(class_name, allow_packages)
        {
            return Some(if class_name.starts_with("java/util/") {
                SkipReason::JavaUtilCollection
            } else {
                SkipReason::RustJvmTestFixture
            });
        }

        // Core-library JIT code is fail-closed under Conservative.  The AQS
        // probe still corrupts an AQS node after its entire lock package is
        // interpreted, proving that the remaining producer can be a core-Java
        // caller or helper outside a hand-maintained method list.  Until the
        // shared JIT allocation/root-preservation defect is fixed, compile no
        // `java/` method by default. Aggressive and the package allow-list
        // retain the diagnostic escape hatch.
        if class_name.starts_with("java/") && !package_allowed("java/", allow_packages)
        {
            return Some(SkipReason::JavaUtilCollection);
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

        // KC26-PIC.1 (2026-07-05) — Keycloak PicocliTest post-CompactValue
        // residual timeout. The class no longer hits the old raw CompactValue
        // SIGSEGV, but default JIT spends the watchdog window cycling through
        // Picocli command reflection and Keycloak/SmallRye configuration
        // mapper iteration. Direct controls on the Keycloak 26.6.1 runtime
        // classpath: `--nojit` completes the class in ~172s with the known
        // behavioral failures; default JIT times out at 265s;
        // `CRATONVM_JIT_DENY=org/keycloak/,picocli/,io/smallrye/` completes
        // in ~168s with the same failures. Disabling inline allocation does
        // not help, so this is not the old inline-new header race. Keep these
        // app/config packages interpreted under Conservative until the exact
        // JIT throughput/correctness defect is narrowed. Liftable with e.g.
        // `CRATONVM_JIT_ALLOW_PACKAGES=org/keycloak/,picocli/,io/smallrye/`.
        //
        // Carve-out: `org/keycloak/models/credential/` (the credential
        // DTO/model classes, e.g. `PasswordCredentialData`,
        // `PasswordSecretData`, `CredentialModel`) is unrelated to the
        // Picocli-command / SmallRye-config-mapper timeout this ban targets
        // — it's plain data-holder getters. KC-CRED.LAZY (2026-07-01,
        // above in `is_known_miscompile`) already deliberately narrowed a
        // real correctness bug in exactly two of these methods to a targeted
        // entry gated behind `callee_saved_gpr_local_homes_enabled()`
        // (default off), i.e. these getters were already meant to be
        // JIT-eligible under the safe Conservative default. Without this
        // carve-out this later, broader ban silently re-skip-lists them,
        // regressing that earlier decision.
        //
        // Carve-out: `io/smallrye/config/` + `org/keycloak/quarkus/runtime/
        // configuration/` (2026-07-13, KC26-PIC.2). `PicocliTest` was found
        // to genuinely HANG (not just run slowly) well past this ban's
        // original 265s watchdog window: `SmallRyeConfig`'s
        // `RelocateConfigSourceInterceptor.getValue()` calls
        // `context.proceed()` TWICE per invocation (once for the relocated
        // name, once for the original — legitimate SmallRye semantics), and
        // Quarkus/Keycloak stack N `RelocateConfigSourceInterceptor`
        // instances (one per legacy-property-relocation source), so a
        // single property lookup costs up to O(2^N) total interceptor
        // invocations. That fan-out is negligible under a JIT (nanoseconds/
        // call) but not under a pure bytecode interpreter, where every call
        // pays full dispatch overhead — this reproduced as `httpAccessLog`
        // (and later tests) never completing within a 180s watchdog.
        // Allowing JIT for just these packages took `httpAccessLog` from a
        // >180s hang to 41.8s; bisected the underlying interceptor fan-out
        // itself as pre-existing (reproduces identically at `058e2b957`,
        // immediately before the unrelated KC26-CFG.1 config-resolution fix
        // in `10a561f21`) — not a regression from that commit's
        // native-override removals. `org/keycloak/quarkus/runtime/cli/`
        // (Picocli.java's own `validateConfig`/`validateProperty` orchestration,
        // which loops over every registered CLI option calling into the
        // now-carved-out config/interceptor code once per option) was added
        // after a later test (`duplicatedCliOptions`) hung inside THAT loop
        // specifically rather than inside the interceptor chain itself —
        // the loop's own per-option interpreted overhead was the remaining
        // bottleneck once the interceptor calls themselves got fast.
        // Deliberately narrower than lifting the whole ban: `picocli/`
        // itself remains interpreted, since this ban's own history
        // (KC26-PIC.1 above) found unrestricted JIT for ALL THREE packages
        // was empirically SLOWER for this same test class in 2026-07-05 —
        // that finding may or may not still hold given how much bytecode
        // this ban's own native-fast-path history has changed since, but
        // there is no evidence either way for `picocli/` specifically, so
        // it stays banned. See
        // docs/known-issues/keycloak/quarkus-runtime-picocli-arggroupspec-synopsis-hang-20260713.md
        // for the full investigation.
        let smallrye_relocate_carveout = class_name.starts_with("io/smallrye/config/")
            || class_name.starts_with("org/keycloak/quarkus/runtime/configuration/")
            || class_name.starts_with("org/keycloak/quarkus/runtime/cli/");
        if (class_name.starts_with("org/keycloak/")
            || class_name.starts_with("picocli/")
            || class_name.starts_with("io/smallrye/"))
            && !class_name.starts_with("org/keycloak/models/credential/")
            && !smallrye_relocate_carveout
            && !package_allowed(class_name, allow_packages)
        {
            return Some(SkipReason::RustJvmTestFixture);
        }

        // KC26-RX.1 (2026-07-08) -- Keycloak `RealmModelTest` post-Infinispan
        // bootstrap residual: default JIT gets past the old `FileDescriptor.
        // fullName` decode error and the `DefaultCacheManager` configuration
        // native gaps, then stalls while Infinispan drains a RxJava-backed
        // distributed stream (`BlockingFlowableIterable$BlockingFlowableIterator.
        // hasNext` waiting on an AQS condition; last default-JIT progress was
        // `PublisherHandler` request `node-1#2`). Direct controls on the real
        // Keycloak/Infinispan classpath showed `CRATONVM_JIT_DENY=io/reactivex/`
        // advances through the publisher requests (`node-1#6` complete) and
        // into Liquibase parsing, while narrower Infinispan-only denies do not.
        // Keep RxJava3 interpreted under Conservative until the exact producer
        // or consumer miscompile is isolated. Liftable with
        // `CRATONVM_JIT_ALLOW_PACKAGES=io/reactivex/`.
        if class_name.starts_with("io/reactivex/rxjava3/")
            && !package_allowed(class_name, allow_packages)
        {
            return Some(SkipReason::RustJvmTestFixture);
        }

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
        if class_name.starts_with("org/bouncycastle/")
            && !is_bouncycastle_crypto_hotpath_carveout(class_name, method_name)
            && !package_allowed("org/bouncycastle/", allow_packages)
        {
            return Some(SkipReason::RustJvmTestFixture);
        }

        // SUNEC-INTPOLY (2026-06-14) — blanket JIT ban for SunEC's field
        // arithmetic (`sun/security/util/math/intpoly/`). For P-384 / P-521 a
        // repeated keygen+sign+verify mix progressively corrupts the curve's
        // field-element limb arrays (`long[]`), zeroing chunks of the cached
        // generator point, so `ECOperations.multiply` then fails "point NOT ON
        // CURVE" (keycloak DefaultCryptoJWKTest publicEs256P384/P521,
        // BCECDSACryptoProviderTest secp384/521, SdJwtVP AltCurves). It is the
        // JIT-only face of the documented cross-package JIT→JIT arg-marshalling
        // miscompile (a primitive value lands in a reference/array slot — cf.
        // docs/bc-math-ec-jit-miscompile-investigation.md): `CRATONVM_DISABLE_JIT=1`
        // makes it 0/40, and bisection shows it needs BOTH `intpoly` AND
        // `java/math` (BigInteger) JIT-compiled together — `intpoly` is the
        // compiled caller, so banning it from the JIT (callee→interpreter) breaks
        // the bad JIT→JIT call. P-256 is unaffected (smaller field) but is banned
        // too for safety; EC field math is correctness-critical crypto, never a
        // benchmarked hot path, so interpreter-only is the right trade. Lifted by
        // `CRATONVM_JIT_ALLOW_PACKAGES=sun/security/util/math/intpoly/`.
        if class_name.starts_with("sun/security/util/math/intpoly/")
            && !package_allowed("sun/security/util/math/intpoly/", allow_packages)
        {
            return Some(SkipReason::RustJvmTestFixture);
        }

        // HIB-BYTEBUDDY (2026-06-13) — provisional blanket ban for ByteBuddy's
        // runtime class-build chain (`net/bytebuddy/`). The narrow HIB-PROXY ban
        // on `ByteBuddyState.make` only covered the lazy-proxy path; Hibernate's
        // bytecode-enhancement path (`EnhancerImpl.enhance` -> `ByteBuddyState.
        // rewrite` -> `DynamicType...make` -> `MethodRegistry.prepare` -> deep
        // `net/bytebuddy/description/type/TypeDescription*` resolution) hangs
        // forever once those type-description methods are JIT-compiled
        // (`SimpleEnhancerTests` rc=124; the stack spins in
        // `TypeDefinition$Sort.describe` / `TypeDescription.represents`). It is
        // the same "JIT'd build-chain receiver corruption / loop never returns"
        // miscompile as HIB-PROXY, and `CRATONVM_DISABLE_JIT=1` makes the whole
        // enhancer pass (ok=1). ByteBuddy is a one-shot code generator, never a
        // benchmarked hot path, so interpreter-only is the right trade. Lifted
        // by `CRATONVM_JIT_ALLOW_PACKAGES=net/bytebuddy/`.
        if class_name.starts_with("net/bytebuddy/")
            && !package_allowed("net/bytebuddy/", allow_packages)
        {
            return Some(SkipReason::RustJvmTestFixture);
        }

        // LUCENE-POSTINGS.1 (2026-07-05) — fail-closed JIT bans for the Lucene
        // postings validation path used by Elasticsearch's ES812 postings-format
        // focused repro (`B17AC9D3E1F2A0C4`,
        // `testDocsAndFreqsAndPositionsAndPayloads`). With normal JIT the worker
        // threads throw AIOOBE with byte-swapped-looking postings indices and can
        // later SIGSEGV from poisoned state. The same binary with
        // `CRATONVM_DISABLE_JIT=1` passes the method in ~64s; focused
        // MMapDirectory scalar/bulk/random-access probes match HotSpot, so this
        // is a JIT execution bug above the FFM read primitives.
        //
        // Bisection evidence:
        // - `CRATONVM_JIT_BISECT_ONLY=org/apache/lucene/codecs/` passes in
        //   isolation, but with Lucene index and ES postings packages skipped
        //   the remaining compiled hot path is still codecs-side
        //   `Impact.toString`, so keep codecs interpreted as part of this
        //   fail-closed postings cluster.
        // - `CRATONVM_JIT_BISECT_ONLY=org/apache/lucene/index/` reproduces the
        //   crash, dominated by `SlowImpactsEnum.nextDoc/freq`.
        // - Skipping only `SlowImpactsEnum.nextDoc/freq` exposes corrupted
        //   receiver state (`PForUtil` where a postings enum receiver is
        //   expected), so the producer is broader than those leaf methods.
        //
        // Later JIT-entry summaries with index/codecs/postings already skipped
        // showed remaining compiled Lucene store/util/backward-codecs methods
        // (`IndexInput.toString`, `BytesRef.compareTo`, `DataInput.readVInt`,
        // block-tree frame helpers) before the same postings corruption. The
        // exact producer is still unresolved, so keep all Lucene bytecode
        // interpreted for correctness. This is broad but bounded to Lucene and
        // the focused repro still completes comfortably under the 300s suite
        // timeout. Liftable via `CRATONVM_JIT_ALLOW_PACKAGES=org/apache/lucene/`.
        if class_name.starts_with("org/apache/lucene/")
            && !package_allowed("org/apache/lucene/", allow_packages)
        {
            return Some(SkipReason::RustJvmTestFixture);
        }
        if class_name.starts_with("com/carrotsearch/randomizedtesting/")
            && !package_allowed("com/carrotsearch/randomizedtesting/", allow_packages)
        {
            return Some(SkipReason::RustJvmTestFixture);
        }

        if class_name.starts_with("org/apache/logging/log4j/")
            && !package_allowed("org/apache/logging/log4j/", allow_packages)
        {
            return Some(SkipReason::RustJvmTestFixture);
        }

        if class_name.starts_with("org/junit/") && !package_allowed("org/junit/", allow_packages) {
            return Some(SkipReason::RustJvmTestFixture);
        }

        if class_name.starts_with("junit/") && !package_allowed("junit/", allow_packages) {
            return Some(SkipReason::RustJvmTestFixture);
        }

        // SPB.1 (Session 112) — provisional blanket ban for the Spring
        // Framework `org/springframework/util/` package. `ClassUtils.
        // <clinit>` runs `registerCommonClasses(...)` ~10 times for
        // primitive / wrapper / collection / common-types groups, putting
        // ~100 entries into a fresh HashMap. With JIT enabled the run
        // segfaults right after the log4j-api StatusLogger warning; with
        // `CRATONVM_DISABLE_JIT=1` the segfault disappears (a different
        // downstream gap surfaces in PropertiesUtil.<clinit>). The frame
        // trace shows the very last frame popping is
        // `ClassUtils.registerCommonClasses` after a long sequence of
        // `put -> putVal -> newNode -> Node.<init> -> afterNodeInsertion`
        // cycles — the same allocate-then-putfield archetype documented
        // in W2-CHM / RBC.1 / EXEC.1. The narrow HashMap entries above
        // (`putVal`, `newNode`, `treeifyBin`, `hash`, `afterNode*`) cover
        // the JDK side, but the Spring `ClassUtils.registerCommonClasses`
        // method itself iterates the input array and calls
        // `clazz.getName() -> Class.getName() -> String allocation` per
        // element, which the JIT may compile after the second batch and
        // miscompile the new String's value/coder slots. Spring's
        // `ReflectionUtils`, `StringUtils`, etc. share the same
        // allocate-heavy idioms.
        //
        // Like the BouncyCastle ban above, this is a coarse-grained
        // safety net so SportMe boot can progress past `ClassUtils.
        // <clinit>`. Lifted by
        // `CRATONVM_JIT_ALLOW_PACKAGES=org/springframework/util/`. Track
        // for a real fix once the underlying allocate-then-putfield
        // miscompile is root-caused.
        if class_name.starts_with("org/springframework/util/")
            && !package_allowed("org/springframework/util/", allow_packages)
        {
            return Some(SkipReason::RustJvmTestFixture);
        }

        // SPB.2 (Session 112 r8) — provisional blanket ban for
        // `org/springframework/core/`. SerializableTypeWrapper.forTypeProvider
        // hangs after Assert.notNull POP when the lambda body
        // `lambda$forGenericInterfaces$<hash>$1(Class, int)` is dispatched.
        // The lambda body calls `Class.getGenericInterfaces()` which the
        // JIT promotes after the heavy ConcurrentReferenceHashMap segment
        // initialisation in SerializableTypeWrapper.<clinit> (16 segments
        // x 10 maps = 160 segment ctor entries). With JIT enabled the
        // SAM dispatch into the lambda body never returns; with
        // `CRATONVM_DISABLE_JIT=1` boot proceeds past the lambda (and a
        // different downstream gap surfaces in log4j PropertiesUtil
        // <clinit>). The same allocate-then-putfield-vs-OSR pattern that
        // bites Integer.valueOf / String.toLowerCase applies here:
        // ConcurrentReferenceHashMap.Reference / Node allocation paths
        // store fields immediately after `new`. Lifted by
        // `CRATONVM_JIT_ALLOW_PACKAGES=org/springframework/core/`.
        if class_name.starts_with("org/springframework/core/")
            && !package_allowed("org/springframework/core/", allow_packages)
        {
            return Some(SkipReason::RustJvmTestFixture);
        }

        // SPB.4 (Session 113 r1) — provisional blanket ban for the Spring
        // Boot configuration-property binder package. ms-course-youtube
        // `admin-service` SIGSEGVs (rc=139) deep inside the property bind
        // path: `JavaBeanBinder$Bean.<init>` -> `BeanProperties.<init>`
        // -> `BeanProperties.addProperties` -> `getSorted`. The
        // `CRATONVM_FRAME_TRACE=1` capture shows the very last frames
        // before the crash are `Banner$Mode.<clinit>` returning into
        // `Class$ReflectionData.<init>` then `Reflection.filter` —
        // i.e. the JavaBeanBinder is reflectively scanning a class for
        // bindable properties via `Class.getDeclaredMethods()` /
        // `Class.getDeclaredFields()`, sorting the filtered Member array,
        // and storing each into a `LinkedHashMap` keyed by property name.
        //
        // The signature matches W2-CHM / RBC.1 / SPB.1 / SPB.2 / SPB.3:
        // every method on this hot path is allocate-then-putfield-heavy.
        // `BeanProperties.addProperties` calls `addMethod`/`addField`
        // which allocate a fresh `BeanProperty` and immediately store
        // `name`/`type`/`getter`/`setter`/`field` slots; `Bean.<init>`
        // builds a `Bindable.BindMethod` enum and a `Constructor`
        // reference; the SAM `BiPredicate.lambda$or$0` captured by the
        // tight `ConfigurationPropertyName.isAncestorOf` loop allocates
        // a fresh `lambda$or$0` capture object on each invocation. With
        // `CRATONVM_DISABLE_JIT=1` the SIGSEGV is replaced by a clean
        // `NullPointerException` in `PathMatchingResourcePatternResolver.
        // <clinit>` (a different downstream gap, not a JIT issue).
        //
        // Lifted by `CRATONVM_JIT_ALLOW_PACKAGES=org/springframework/boot/
        // context/properties/bind/`. The narrow per-method entries
        // (SPB.3) for `SpringIterableConfigurationPropertySource` cover
        // the upstream cache-key build path; this blanket ban covers the
        // downstream binder dispatch.
        if class_name.starts_with("org/springframework/boot/context/properties/bind/")
            && !package_allowed(
                "org/springframework/boot/context/properties/bind/",
                allow_packages,
            )
        {
            return Some(SkipReason::RustJvmTestFixture);
        }

        // SPB.4b (Session 113 r1) — broaden the SPB.4 ban to cover the
        // surrounding Spring Boot context-property plumbing. After SPB.4
        // pins the binder dispatch, the very next consumer is
        // `org/springframework/boot/context/properties/source/
        // SystemEnvironmentPropertyMapper.processElementValue`, which
        // calls `String.toLowerCase` (already covered) but also
        // allocates fresh `CharSequence` views via `String.subSequence`
        // on every property name. The companion package
        // `org/springframework/boot/context/properties/source/` (where
        // SPB.3 has narrow per-method pins) plus the umbrella
        // `org/springframework/boot/context/` are blanket-banned here so
        // the JIT cannot promote any method on the bind path. The
        // SportMe agent's SPB.3 narrow pins remain in effect; this
        // broader ban is additive, not replacing those entries. Lifted
        // by `CRATONVM_JIT_ALLOW_PACKAGES=org/springframework/boot/context/`.
        if class_name.starts_with("org/springframework/boot/context/")
            && !package_allowed("org/springframework/boot/context/", allow_packages)
        {
            return Some(SkipReason::RustJvmTestFixture);
        }

        // SPB.4c (Session 113 r1) — Spring Boot's top-level
        // `SpringApplication`, `Banner$Mode`, `ApplicationEnvironment`,
        // `DefaultApplicationContextFactory`, `ApplicationInfoPropertySource`,
        // and friends are also on the boot critical path observed in the
        // ms-course-youtube admin-service frame trace. Those classes
        // execute exactly once at boot but do thousand+ allocations
        // each, putting them above the JIT thresholds. Lifted by
        // `CRATONVM_JIT_ALLOW_PACKAGES=org/springframework/boot/`. (This
        // is the umbrella ban; subpackages like `boot/loader/` are
        // already past their ctor by the time the binder runs, so the
        // throughput loss is bounded to startup.)
        if class_name.starts_with("org/springframework/boot/")
            && !class_name.starts_with("org/springframework/boot/loader/")
            && !package_allowed("org/springframework/boot/", allow_packages)
        {
            return Some(SkipReason::RustJvmTestFixture);
        }

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
        if class_name.starts_with("org/springframework/cloud/")
            && !package_allowed("org/springframework/cloud/", allow_packages)
        {
            return Some(SkipReason::RustJvmTestFixture);
        }

        // ANTLR.1 (2026-06-23) — blanket ban for the shaded ANTLR v4 runtime
        // that Groovy's parser (`org.apache.groovy.parser.antlr4`) and any
        // ANTLR-based grammar (HQL, SpEL, …) execute. Two independent reasons,
        // both verified on the SpringRepositoriesExtensionTests / Groovy parse:
        //
        // 1. CORRECTNESS — JIT-compiling the ANTLR ATN simulation MISCOMPILES.
        //    A method in `…/runtime/atn/` produces a null `PredictionContext`
        //    that flows into interpreted `ATNConfigSet.optimizeConfigs` ->
        //    `ATN.getCachedContext` -> NPE, surfacing as Groovy
        //    `MultipleCompilationErrorsException: General error during parsing:
        //    NullPointerException` (the script fails to compile). `--nojit`
        //    parses the SAME script cleanly. Bisection via
        //    `CRATONVM_JIT_BISECT_SKIP` proved it and NARROWED the culprit from
        //    the ~105 compiled `groovyjarjarantlr4/*` methods down to a 7-method
        //    `PredictionContext` equality/hash cluster — de-JIT'ing just these 7
        //    makes the parse succeed:
        //      PredictionContext.{calculateHashCode, hashCode},
        //      PredictionContext$IdentityEqualityComparator.hashCode,
        //      SingletonPredictionContext.{equals, isEmpty, size},
        //      ObjectEqualityComparator.equals.
        //    (A wrong hash/equals corrupts ATN config-context dedup, leaving a
        //    config with a null `PredictionContext` that later NPEs.) The exact
        //    single method / codegen archetype is the open follow-up; the
        //    package ban is the sound, evidence-backed stop-gap (a surgical
        //    per-method ban of those 7 is the future minimal fix once the
        //    codegen bug is root-caused — see docs/known-issues).
        // 2. THROUGHPUT — JIT-compiling ANTLR is also a large REGRESSION here:
        //    the first cold parse runs ~8x SLOWER with JIT than `--nojit`
        //    (a trivial warmup class alone takes ~95 s under JIT). The ATN
        //    simulation is a one-shot, branch-heavy interpreter loop, not a
        //    benchmarked hot path — exactly the BouncyCastle / ByteBuddy /
        //    Spring archetype banned above. Same root family as the
        //    Hibernate HQL reproducer in
        //    `springrepos-extension-hang-jit-throughput-and-deep-recursion.md`.
        //
        // Lifted by `CRATONVM_JIT_ALLOW_PACKAGES=groovyjarjarantlr4/` for
        // cold-path validation, but the PredictionContext equality/hash
        // cluster above stays interpreted.
        if class_name.starts_with("groovyjarjarantlr4/")
            && !package_allowed("groovyjarjarantlr4/", allow_packages)
        {
            return Some(SkipReason::RustJvmTestFixture);
        }

        // SPB.6 (Session 113 r1) — provisional blanket ban for the
        // Netflix Eureka discovery client. `com/netflix/discovery/
        // DiscoveryClient.<init>` allocates Eureka `InstanceInfo` /
        // `ApplicationInfoManager` objects whose ctors store
        // `metadata`/`leaseInfo`/`port` immediately after allocation
        // — the same allocate-then-putfield archetype. Eureka also
        // installs a `ScheduledExecutorService` whose task submit path
        // is the same `LinkedBlockingQueue.offer` / `enqueue` pair
        // already covered by EXEC.1; this per-package ban covers the
        // Eureka-specific allocations. Lifted by
        // `CRATONVM_JIT_ALLOW_PACKAGES=com/netflix/discovery/`.
        if class_name.starts_with("com/netflix/discovery/")
            && !package_allowed("com/netflix/discovery/", allow_packages)
        {
            return Some(SkipReason::RustJvmTestFixture);
        }

        // SPB.7 (Session 113 r1) — provisional blanket ban for Feign /
        // OpenFeign HTTP client allocations. Spring Cloud OpenFeign
        // builds a `feign.Feign$Builder` that allocates per-method
        // `MethodMetadata` and `RequestTemplate` objects, each storing
        // `template` / `headers` / `body` slots immediately after `new`.
        // Lifted by `CRATONVM_JIT_ALLOW_PACKAGES=feign/`.
        if class_name.starts_with("feign/") && !package_allowed("feign/", allow_packages) {
            return Some(SkipReason::RustJvmTestFixture);
        }

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
        if class_name.starts_with("org/jboss/modules/")
            && !package_allowed("org/jboss/modules/", allow_packages)
        {
            return Some(SkipReason::RustJvmTestFixture);
        }

        // SPB.8b (Session 113 r2) — companion blanket ban for the WildFly
        // server boot path (`org/jboss/as/`). Once JBoss Modules is
        // unblocked by SPB.8, the next downstream consumer is the JBoss AS
        // server bootstrap (`org/jboss/as/server`, `org/jboss/as/controller`,
        // `org/jboss/as/jmx`, etc.), which exhibits the same
        // allocate-then-putfield pattern: `ServerLogger_$logger_en_US`
        // ctors store i18n message slots, `PluggableMBeanServerImpl`
        // delegates allocate fresh `Subject` / `ClassLoader` references
        // per invocation, and `ServerEnvironment.<init>` resolves dozens
        // of `-Djboss.*` properties via `Long.parseLong` /
        // `Boolean.parseBoolean`. Pre-emptive to avoid a second iteration
        // if the next gap surfaces in this layer. Lifted by
        // `CRATONVM_JIT_ALLOW_PACKAGES=org/jboss/as/`.
        if class_name.starts_with("org/jboss/as/")
            && !package_allowed("org/jboss/as/", allow_packages)
        {
            return Some(SkipReason::RustJvmTestFixture);
        }

        // SPB.8c (Session 113 r2) — companion blanket ban for the WildFly
        // security-manager package (`org/wildfly/`). The
        // `CRATONVM_DBG_JIT_DISPATCH=1` capture shows the very last JIT
        // dispatches before the SIGSEGV are
        // `org/wildfly/security/manager/WildFlySecurityManager.<init>` and
        // `WildFlySecurityManager$2.run`, plus
        // `GetAccessibleDeclaredFieldAction.run` and
        // `ReadPropertyAction.run`, immediately followed by a
        // `java/lang/reflect/AccessibleObject.setAccessible0(Z)Z` chain
        // that culminates in `java/lang/Long.parseLong(String,int)` being
        // dispatched with a corrupted reference arg0
        // (`0xfffd_<heap-ptr>`). The 16-bit-tag corruption at offset 48
        // is the same JIT codegen archetype that bites
        // `Integer.valueOf` / `String.toLowerCase` — the JIT promotes
        // `ReadPropertyAction.run` and miscompiles the field load that
        // returns the property value, OR-ing the high tag bits into the
        // String reference before it is forwarded to `Long.parseLong`.
        // Lifted by `CRATONVM_JIT_ALLOW_PACKAGES=org/wildfly/`.
        // WildFly 39 / session-15 progression: boot now advances past the
        // `org/wildfly/` ban and the same rc=139 SIGSEGV surfaces in the
        // JBoss MSC service container (`ServiceName.equals`) plus the
        // JBoss Logging facade (`JDKLogger.<init>` / `LoggerProvider.getLogger`
        // were dispatched ~200x in the trace immediately before the crash).
        // Both `org/jboss/msc/` and `org/jboss/logging/` are extended below
        // with the same SPB.8 archetype reasoning.
        if class_name.starts_with("org/wildfly/")
            && !package_allowed("org/wildfly/", allow_packages)
        {
            return Some(SkipReason::RustJvmTestFixture);
        }
        if class_name.starts_with("org/jboss/msc/")
            && !package_allowed("org/jboss/msc/", allow_packages)
        {
            return Some(SkipReason::RustJvmTestFixture);
        }
        if class_name.starts_with("org/jboss/logging/")
            && !package_allowed("org/jboss/logging/", allow_packages)
        {
            return Some(SkipReason::RustJvmTestFixture);
        }

        // SPB.9 (Session 114) — provisional blanket ban for the SLF4J /
        // Logback / commons-logging facades. `apps/insurance-backend` Spring
        // Boot 3.2 boot reaches the Spring banner then crashes with
        // `expected object reference, got int(1)` while
        // `SpringApplication.prepareEnvironment` walks the
        // `SystemEnvironmentPropertyMapper.processElementValue` chain (frame
        // depth 18). With `CRATONVM_DISABLE_JIT=1` the same int(1) crash
        // surfaces — but the very last methods JIT-dispatched before the
        // failure (`CRATONVM_DBG_JIT_DISPATCH=1` capture) are an extremely
        // tight loop of `LogAdapter$Slf4jLog.<init>`,
        // `LogAdapter$Slf4jLocationAwareLog.<init>`,
        // `LoggerFactory.getLogger`, `LoggerFactory.getProvider`,
        // `SLF4JServiceProvider.getLoggerFactory`,
        // `ILoggerFactory.getLogger`, and
        // `LoggerContext.getLogger` — Spring Boot's per-class logger
        // wiring during component scan. Each call returns a JIT-compiled
        // `Logger` reference that is then stored into the
        // `Slf4jLog.logger` slot via the same allocate-then-putfield
        // archetype that bites W2-CHM / RBC.1 / SPB.1-8. The miscompiled
        // store leaves an `int(1)` (likely the `LocationAwareLogger`
        // instance test boolean) where a `Logger` reference belongs; the
        // next interpreter `pop_object_ref` on that slot raises the
        // observed `expected object reference, got int(1)` crash.
        //
        // The three logging facades are tightly coupled at boot:
        // `org/slf4j/` (the API), `ch/qos/logback/` (Spring Boot 3.2's
        // default backend), and `org/apache/commons/logging/` (the
        // bridge Spring uses internally). Banning all three together
        // covers the full per-class logger wiring path. Lifted by
        // `CRATONVM_JIT_ALLOW_PACKAGES=org/slf4j/,ch/qos/logback/,
        // org/apache/commons/logging/`.
        if class_name.starts_with("org/slf4j/") && !package_allowed("org/slf4j/", allow_packages) {
            return Some(SkipReason::RustJvmTestFixture);
        }
        if class_name.starts_with("ch/qos/logback/")
            && !package_allowed("ch/qos/logback/", allow_packages)
        {
            return Some(SkipReason::RustJvmTestFixture);
        }
        if class_name.starts_with("org/apache/commons/logging/")
            && !package_allowed("org/apache/commons/logging/", allow_packages)
        {
            return Some(SkipReason::RustJvmTestFixture);
        }

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
        if class_name.starts_with("net/sf/cglib/")
            && !package_allowed("net/sf/cglib/", allow_packages)
        {
            return Some(SkipReason::RustJvmTestFixture);
        }

        // SPB-FLYWAY-HSQLDB.1: Flyway's HSQLDB integration runs this package
        // through a dense add/update path. With JIT enabled it SIGSEGVs after
        // CGLIB configuration enhancement; CRATONVM_JIT_DENY=org/hsqldb/
        // consistently completes all test methods. Keep it interpreted until
        // the lowering defect is isolated. Opt in for bisection with
        // CRATONVM_JIT_ALLOW_PACKAGES=org/hsqldb/.
        if class_name.starts_with("org/hsqldb/")
            && !package_allowed("org/hsqldb/", allow_packages)
        {
            return Some(SkipReason::RustJvmTestFixture);
        }
        // SPB.9b (Session 114) — companion blanket ban for the Spring
        // Boot loader + reactive web context, plus the Spring Beans
        // factory support layer. After SPB.9 pins the per-class logger
        // wiring, the next downstream consumers that allocate-then-putfield
        // on the `prepareEnvironment` -> component-scan critical path are:
        //   * `org/springframework/boot/loader/` — JarLauncher /
        //     LaunchedURLClassLoader allocate per-jar `Archive` /
        //     `Source` records and store them via putfield. Note this
        //     intentionally overrides the SPB.4c `loader/` exemption
        //     because the insurance-backend JarLauncher.launch frame is
        //     itself the entry point that fails dispatch.
        //   * `org/springframework/web/reactive/` and
        //     `org/springframework/boot/web/reactive/` — insurance-backend
        //     uses Spring WebFlux; `ReactiveWebServerApplicationContext`
        //     and `ReactiveWebServerFactory` allocate Reactor Netty
        //     handler chains (`HttpHandler`, `WebFilter`) whose ctors
        //     store config slots immediately after `new`.
        //   * `org/springframework/beans/factory/support/` —
        //     `DefaultListableBeanFactory.registerBeanDefinition` and
        //     `BeanDefinitionMap.put` are called once per scanned
        //     component (~50+ beans for a minimal Spring Boot 3.2
        //     reactive app), and the `RootBeanDefinition.<init>` ctor
        //     copies ~15 fields (factoryClass, factoryMethod, scope,
        //     ctorArgs, ...) via putfield — exact W2-CHM archetype.
        // Lifted per-package via
        // `CRATONVM_JIT_ALLOW_PACKAGES=org/springframework/boot/loader/,
        // org/springframework/web/reactive/,
        // org/springframework/boot/web/reactive/,
        // org/springframework/beans/factory/support/`.
        if class_name.starts_with("org/springframework/boot/loader/")
            && !package_allowed("org/springframework/boot/loader/", allow_packages)
        {
            return Some(SkipReason::RustJvmTestFixture);
        }
        if class_name.starts_with("org/springframework/web/reactive/")
            && !package_allowed("org/springframework/web/reactive/", allow_packages)
        {
            return Some(SkipReason::RustJvmTestFixture);
        }
        if class_name.starts_with("org/springframework/boot/web/reactive/")
            && !package_allowed("org/springframework/boot/web/reactive/", allow_packages)
        {
            return Some(SkipReason::RustJvmTestFixture);
        }
        if class_name.starts_with("org/springframework/beans/factory/support/")
            && !package_allowed("org/springframework/beans/factory/support/", allow_packages)
        {
            return Some(SkipReason::RustJvmTestFixture);
        }

        // SPB.9c (Session 114) — companion blanket bans for the Spring
        // component-scan critical path. With SPB.9 and SPB.9b in place,
        // insurance-backend boot reaches `SpringApplication.run` ->
        // `AbstractApplicationContext.refresh` ->
        // `PostProcessorRegistrationDelegate.invokeBeanFactoryPostProcessors`
        // -> `ConfigurationClassPostProcessor.processConfigBeanDefinitions`
        // -> `ConfigurationClassParser.parse` ->
        // `ClassPathBeanDefinitionScanner.doScan` ->
        // `ClassPathScanningCandidateComponentProvider.scanCandidateComponents`
        // -> `PathMatchingResourcePatternResolver.getResources` /
        // `findAllModulePathResources` -> `ModuleLayer.configuration` /
        // `Configuration.modules()` (frame trace depth 15-18 immediately
        // before the int(1) crash). Each of these makes putfield-heavy
        // allocations: `ConfigurationClassParser.SourceClass.<init>`
        // stores `metadata`/`source`/`importBy` slots, and the
        // `PathMatching` resolver allocates a `Resource[]` per scanned
        // package and stores resolved `Resource` references via aastore.
        // Same W2-CHM / RBC.1 / SPB.1-9 archetype.
        //
        // Lifted per-package via
        // `CRATONVM_JIT_ALLOW_PACKAGES=org/springframework/context/annotation/,
        // org/springframework/context/support/,
        // org/springframework/core/io/support/,
        // org/springframework/beans/factory/`.
        if class_name.starts_with("org/springframework/context/annotation/")
            && !package_allowed("org/springframework/context/annotation/", allow_packages)
        {
            return Some(SkipReason::RustJvmTestFixture);
        }
        if class_name.starts_with("org/springframework/context/support/")
            && !package_allowed("org/springframework/context/support/", allow_packages)
        {
            return Some(SkipReason::RustJvmTestFixture);
        }
        if class_name.starts_with("org/springframework/core/io/support/")
            && !package_allowed("org/springframework/core/io/support/", allow_packages)
        {
            return Some(SkipReason::RustJvmTestFixture);
        }
        // SPB.9d (Session 117) — eureka-server boot SEGFAULTs deep in
        // Spring's BeanInfo/ExtendedBeanInfo introspection. Spring
        // introspects every bean class via `java/beans/Introspector`,
        // which delegates into `com/sun/beans/introspect/MethodInfo`
        // and `ClassInfo` and sorts methods/properties via
        // comparators. Frame trace + CRATONVM_DBG_JIT_DISPATCH=1 show
        // the very last hot JIT-compiled callees on the crash path are
        // `MethodInfo$MethodOrder.compare`, `String.compareTo`,
        // `Method.getName`, `Arrays.hashCode`, `Method.toString`,
        // and `StringJoiner.<init>` — i.e. a JIT-compiled comparator
        // chain driven by `java/util/Arrays.sort`. With JIT disabled
        // the run terminates cleanly with the parallel agent's
        // `Attribute 'type' not found` (rc=1, no segfault); with JIT
        // enabled the comparator returns inconsistent ordering,
        // corrupting transient sort state and SEGFAULTing in a
        // downstream `Method.toString` -> `StringJoiner` allocation.
        // Same allocate-then-putfield archetype as NEW-1.3 / SPB.1.
        //
        // Blanket-ban the JDK BeanInfo introspection package and the
        // `java/beans/` reflection-driven sort callers. Liftable via
        // `CRATONVM_JIT_ALLOW_PACKAGES=com/sun/beans/,java/beans/`
        // when the underlying allocate-then-putfield miscompile is
        // root-caused.
        if class_name.starts_with("com/sun/beans/")
            && !package_allowed("com/sun/beans/", allow_packages)
        {
            return Some(SkipReason::RustJvmTestFixture);
        }
        if class_name.starts_with("java/beans/") && !package_allowed("java/beans/", allow_packages)
        {
            return Some(SkipReason::RustJvmTestFixture);
        }
        if class_name.starts_with("org/springframework/beans/factory/")
            && !class_name.starts_with("org/springframework/beans/factory/support/")
            && !package_allowed("org/springframework/beans/factory/", allow_packages)
        {
            return Some(SkipReason::RustJvmTestFixture);
        }

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
        if class_name.starts_with("org/junit/platform/console/shadow/picocli/")
            && !package_allowed("org/junit/platform/console/shadow/picocli/", allow_packages)
        {
            return Some(SkipReason::RustJvmTestFixture);
        }
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

// Elasticsearch suite classes are kept interpreted under the conservative
// policy until the package can be safely re-bisected. The July 2026 vector and
// DiskBBQ hang residuals are covered by this same containment: the affected
// test classes and their Elasticsearch vector-codec bodies sit under
// `org/elasticsearch/`, while Lucene bytecode has its own fail-closed package
// skip below.
fn is_elasticsearch_suite_jit_fragile_cluster(class_name: &str, _method_name: &str) -> bool {
    class_name.starts_with("org/elasticsearch/")
}

fn hibernate_temporal_residual_skip_prefix(class_name: &str) -> Option<&'static str> {
    const SLASH_PREFIX: &str = "org/hibernate/";
    const DOT_PREFIX: &str = "org.hibernate.";
    if class_name.starts_with(SLASH_PREFIX) {
        Some(SLASH_PREFIX)
    } else {
        class_name.starts_with(DOT_PREFIX).then_some(DOT_PREFIX)
    }
}

fn jaxb_mapping_residual_skip_prefix(class_name: &str) -> Option<&'static str> {
    const SLASH_PREFIX: &str = "org/glassfish/jaxb/";
    const DOT_PREFIX: &str = "org.glassfish.jaxb.";
    if class_name.starts_with(SLASH_PREFIX) {
        Some(SLASH_PREFIX)
    } else {
        class_name.starts_with(DOT_PREFIX).then_some(DOT_PREFIX)
    }
}

fn is_snakeyaml_emitter_emit_jit_corruption(class_name: &str, method_name: &str) -> bool {
    class_name == "org/yaml/snakeyaml/emitter/Emitter" && method_name == "emit"
}

fn is_known_miscompile(class_name: &str, method_name: &str) -> bool {
    matches!(
        (class_name, method_name),
        // CM-FASTMATH (2026-06-11) — RESOLVED, ban lifted. The commons-math3
        // `FastMath` trig family (sin/sinQ/polySine/...) miscompile was NOT a
        // codegen bug in those methods: `regalloc.rs::bc_len` was missing
        // `ldc`/`ldc_w`/`ldc2_w`, so the liveness walk read constant-pool
        // index operand bytes as opcodes. FastMath's huge pool put its trig
        // coefficients at indices whose bytes decode as returns/`athrow`
        // (phantom block terminators), hiding later local uses from liveness
        // — the allocator then coalesced two live doubles onto one XMM
        // register (`polySine` computed `p*x2*x2` instead of `p*x2*x`).
        // That is also why the bug needed a large constant pool and resisted
        // every small-CP synthetic repro, and why per-method skip bisection
        // pointed at "the whole family" (skipping a method shifts which
        // methods get compiled, not the defect). Fixed in regalloc.rs (plus
        // the same desync in x64.rs `detect_loops`/`estimate_max_stack`);
        // see docs/gaps/gap-jit-fastmath-transform-miscompile.md. Validated:
        // 6.3M-input FastMath.sin sweep == Math.sin, transform suite 54/56
        // with FastMath JIT-compiled (remaining = the pre-existing
        // `testAdHocData` NPE + the interpreter-level `testTransformReal`
        // precision flake, both unrelated to this ban).
        //
        // NEW-1.3 — hash-table hot loop miscompile, surfaces under
        // HashMap.put/get/resize. These three are the observed
        // failing methods from the `CRATONVM_JIT_ALLOW_PACKAGES=java/util`
        // test run; narrow other HashMap methods stay JIT-eligible.
        ("java/util/HashMap", "put")
        | ("java/util/HashMap", "get")
        | ("java/util/HashMap", "resize")
        // SPB.1 (Session 112) — `apps/SportMe-master`'s Spring Boot
        // bootstrap segfaults in `org/springframework/util/ClassUtils.
        // <clinit>` when `registerCommonClasses(Class...)` does ~100 back-
        // to-back `HashMap.put` calls into a freshly allocated
        // `commonClassCache` map. With JIT enabled the run terminates
        // with rc=139 (STATUS_ACCESS_VIOLATION) right after the log4j-api
        // StatusLogger "no log4j-core" warning; with `CRATONVM_DISABLE_JIT=1`
        // the segfault disappears (and a different downstream gap surfaces
        // in PropertiesUtil.<clinit>). The `CRATONVM_FRAME_TRACE=1` capture
        // shows the very last frame is `ClassUtils.registerCommonClasses`
        // popping after a long sequence of `put -> putVal -> newNode ->
        // Node.<init> -> afterNodeInsertion` cycles, with `putVal` and
        // `newNode` being JIT-eligible (only `put` was previously skipped
        // via NEW-1.3).
        //
        // `putVal` is the canonical allocate-then-putfield archetype
        // documented in W2-CHM / RBC.1 / EXEC.1: it allocates a fresh
        // `HashMap$Node` and immediately stores it into `table[i]` via
        // putfield-equivalent IASTORE; under the per-callee invocation
        // threshold (2000) this hits the same regalloc clobber that bites
        // `Integer.valueOf` / `String.toLowerCase`. `newNode` wraps the
        // raw `new Node(...)` allocation, `treeifyBin` rebuilds the bin
        // into a TreeNode (allocate + putfield-heavy), and `hash` is on
        // the call site of every put/get. Skip-listing these four extends
        // the W2-CHM containment to the Spring boot path. Other HashMap
        // methods (size, containsKey, isEmpty, clear, etc.) stay
        // JIT-eligible because they don't allocate-then-putfield.
        | ("java/util/HashMap", "putVal")
        | ("java/util/HashMap", "newNode")
        | ("java/util/HashMap", "treeifyBin")
        | ("java/util/HashMap", "hash")
        | ("java/util/HashMap", "afterNodeInsertion")
        | ("java/util/HashMap", "afterNodeAccess")
        | ("java/util/HashMap", "afterNodeRemoval")
        // LinkedHashMap inherits the same allocate-then-putfield idiom in
        // its overridden `newNode` / `newTreeNode` (which allocate
        // `LinkedHashMap$Entry` whose ctor sets `before`/`after` via
        // putfield), and is the backing map for every Spring config /
        // ServiceLoader cache. Skip-list it preemptively to avoid a
        // second iteration if the next downstream gap exposes it.
        | ("java/util/LinkedHashMap", "newNode")
        | ("java/util/LinkedHashMap", "newTreeNode")
        | ("java/util/LinkedHashMap", "afterNodeInsertion")
        | ("java/util/LinkedHashMap", "afterNodeAccess")
        | ("java/util/LinkedHashMap", "afterNodeRemoval")
        // ES-HANG-01 (2026-06-18) — Elasticsearch 9.5 unit-test suite: every
        // `ESTestCase`/`LuceneTestCase` suite livelocks during RandomizedRunner
        // setup under the JIT; `--nojit` runs fine and the process is still hung
        // at 650s (permanent, CPU-bound — cdb shows an interpreter/JIT execution
        // loop, not a deadlock). Bisected with `CRATONVM_JIT_BISECT_ONLY/SKIP`:
        //   - `BISECT_ONLY=java/util/WeakHashMap`            -> still hangs
        //   - `BISECT_ONLY=java/util/WeakHashMap` + SKIP
        //     `WeakHashMap$ValueSpliterator.tryAdvance`      -> runs
        // i.e. compiling ONLY WeakHashMap reproduces it and skipping exactly
        // `ValueSpliterator.tryAdvance` eliminates it — so the defect is in that
        // method's JIT code, not a compilation-shift artifact (cf. CM-FASTMATH).
        //
        // `WeakHashMap$*Spliterator.tryAdvance` is the table-walk loop
        // `while (current != null || index < fence) { if (current==null)
        // current = tab[index++]; else { ... current = current.next; ... } }`.
        // The post-increment `current = tab[index++]` is emitted as the awkward
        // `dup_x1` stack dance (bci 60..74: getfield index; dup_x1; iconst_1;
        // iadd; putfield index; aaload; putfield current) interleaving the
        // `index` putfield with the array load. The JIT'd loop never terminates
        // — same family as NETTY.1 (`Arrays.fill` counted-loop) and the HashMap
        // hot-loop miscompiles above: either the `index`/`current` putfield is
        // dropped (IV never advances) or the `if_icmpge`/`ifnonnull` exit is
        // miscompiled. The Key/Value/Entry spliterators and their
        // `forEachRemaining` share byte-for-byte the same walk, so all six are
        // skip-listed together (cf. the pre-emptive `Arrays.fill` variants).
        // Skipping these runs them in the interpreter (correct) and unblocks the
        // entire ES suite. Repro: `apps/elasticsearch/cratonvm-suite/probe/
        // LuceneOnlyTest` under JIT. Root-cause fix in the loop/putfield codegen
        // would let these be lifted (like NETTY.1 was after the regalloc fix).
        | ("java/util/WeakHashMap$KeySpliterator", "tryAdvance")
        | ("java/util/WeakHashMap$KeySpliterator", "forEachRemaining")
        | ("java/util/WeakHashMap$ValueSpliterator", "tryAdvance")
        | ("java/util/WeakHashMap$ValueSpliterator", "forEachRemaining")
        | ("java/util/WeakHashMap$EntrySpliterator", "tryAdvance")
        | ("java/util/WeakHashMap$EntrySpliterator", "forEachRemaining")
        // NETTY.1 (current session) — JIT'd `java/util/Arrays.fill(byte[], byte)`
        // never returns. Reproducer: `apps/netty/NettyEchoTest` (rc=124 after
        // 30s) hangs during the netty bootstrap cascade. `CRATONVM_FRAME_TRACE=1`
        // capture shows the very last frame pushed before the freeze is
        // `java/util/Arrays.fill([BB)V`, called from
        // `io/netty/util/internal/StringUtil.<clinit>` at bci 121 to zero-fill
        // the 65536-element `HEX2B` byte array with -1. With
        // `CRATONVM_DISABLE_JIT=1` the hang vanishes and surfaces a clean
        // `PlatformDependent0.<clinit>` NPE (a separate downstream gap, not a
        // JIT issue) — classic JIT-miscompile signature.
        //
        // The 65536-iteration counted loop (bci 5..17: `iload i; iload n;
        // if_icmpge 20; aload arr; iload i; iload b; bastore; iinc i,1;
        // goto 5`) crosses the 1000-backedge OSR threshold on its first call
        // and triggers OSR re-compilation of `Arrays.fill`. The JIT'd version
        // has an infinite loop — either the bounds check is miscompiled (so
        // `if_icmpge` never fires) or the `iinc` mishandles the IV (so `i`
        // never reaches `n`). The companion `Arrays.fill(int[], int)` etc.
        // share the same bytecode shape and are skip-listed pre-emptively.
        // Other `java/util/Arrays` methods (sort, copyOf, hashCode) don't
        // exhibit this counted-loop shape and stay JIT-eligible.
        //
        // NETTY.1 LIFTED (2026-06-11, CM-FASTMATH retest session): with the
        // regalloc/lentable fixes in place, `bench/FillProbe` replays the
        // exact repro shape (byte[65536] filled with -1, OSR at the backedge
        // threshold, plus the long[]/char[] variants — `fill([II)V` is
        // native-bridged and never compiles) — all OSR-compile, terminate,
        // and match HotSpot's checksum. Ban removed; FillProbe is the
        // regression witness.
        // JUNIT.1 (current session) — STOPGAP. JIT-compiling
        // `org/junit/runner/JUnitCore.main` under real-JCA produces severe
        // young-gen heap corruption: out-of-bounds heap writes overwrite live
        // object headers with garbage (class_ids decay to interface ids like
        // java/io/Serializable / java/lang/Appendable), desyncing the
        // non-moving young sweep and crashing (rc=139), or — once the sweep's
        // diagnostic was hardened to re-sync — dying downstream in BC EC
        // `precompute` on a monitor abort. Isolated via
        // `CRATONVM_JIT_BISECT_SKIP=org/junit/runner/JUnitCore.main` (→ no
        // crash); JIT-only-JUnitCore still crashes. Allocations are correctly
        // sized (validated), and putfield is bounds-checked, so the leading
        // suspect is `emit_inline_tlab_new` writing the header at a wrong
        // R11/TLAB-cursor. Real crypto apps work JIT-on; only the JUnit test
        // harness crashes. This ban unblocks JUnit-under-JIT until the
        // inline-new/TLAB root cause is fixed. See
        // docs/.. / memory `reference_jit_junitcore_corruption`.
        //
        // Retest (2026-06-11, CM-FASTMATH session): inconclusive — in
        // realistic runs (keycloak crypto classes via JUnitCore, with
        // `CRATONVM_JIT_UNBAN_JUNITCORE=1`) `main` never reaches a compile
        // threshold (one invocation per process, no hot back-edges), so the
        // ban could not be exercised; it is also zero-cost for the same
        // reason. KEEP until the original heavy real-JCA harness conditions
        // can be recreated.
        | ("org/junit/runner/JUnitCore", "main")
        // W2-CHM (Cluster B-CHM, Session 108) — JIT miscompiles
        // `Integer.valueOf(int)` / `Integer.<init>(int)` such that the
        // returned `Integer` has `value=0` instead of the requested int
        // for any caller that crosses both the OSR back-edge threshold
        // (1000) and the per-callee invocation threshold (2000) within
        // the same outer-method frame. Reproducer:
        // `apps/chm_basic/ChmScale` puts/gets 1000 keys into a CHM;
        // entries `k992..k999` come back as 0 instead of 992..999
        // because `Integer.valueOf(992..999)` returned the `value=0`
        // path of the JIT'd allocate-and-init sequence. Pinned by
        // `vm/tests/wave2_chm.rs::chm_scale_pins_integer_valueof_jit_miscompile`.
        // Narrow: other `Integer` methods (`intValue` reads field 0;
        // `parseInt` parses a `String`) stay JIT-eligible because they
        // do not exercise the allocate-then-putfield sequence.
        //
        // W2-CHM `valueOf` LIFTED (2026-06-11, CM-FASTMATH retest session):
        // with the regalloc/lentable fixes, `bench/ChmScale` (recreated —
        // the original apps/chm_basic reproducer is gone) + `HashMapProbe`
        // + `ParseProbe` all compile `Integer.valueOf` (upgrade-OK) and
        // match HotSpot exactly, including the historical k992..k999
        // boundary. The `<init>` entries stay: constructors are banned by
        // the generic `<init>` gate anyway (field-storing ctors are never
        // `InitComplexity::Trivial`), so the entries are redundant but
        // document the archetype.
        | ("java/lang/Integer", "<init>")
        | ("java/lang/Long", "<init>")
        // SPB.8 (Session 113 r2) — `java/lang/Long.parseLong(String,int)`
        // and friends. WildFly boot dispatch trace shows this method
        // invoked with a 16-bit-tag-corrupted reference arg0
        // (`0xfffd_<heap-ptr>`) right after a `setAccessible0` chain,
        // crashing in the JIT prologue before any Java bytecode runs.
        // The miscompile is in the JIT calling convention for the
        // (String, int) -> long signature: an int local slot is being
        // mapped onto the String parameter register. Banning these keeps
        // the parse path in the interpreter where calling-convention
        // marshalling is correct. Also covers `parseInt` for symmetry —
        // same archetype (String, int) -> int. `Integer.valueOf` was lifted
        // by the W2-CHM retest above; the remaining boxing constructors are
        // still covered by the constructor gate.
        | ("java/lang/Long", "parseLong")
        | ("java/lang/Integer", "parseInt")
        // RBC.1 (Session 109) — BouncyCastleProvider.<clinit> drives a
        // ~thousand-class init avalanche where every algorithm Mappings
        // class registers via `Provider.put` -> `parseLegacy` ->
        // `String.toLowerCase`/`toUpperCase` -> `Provider$ServiceKey.<init>`.
        // The hot ASCII-only fast path in `String.toLowerCase()` /
        // `toUpperCase()` allocates a fresh `String` and copies its byte
        // array via the same allocate-then-putfield sequence that the JIT
        // miscompiles for `Integer.valueOf`. Under BC's load (every
        // Provider.put call is ~3 toLowerCase calls, ~thousand puts), the
        // miscompiled hot path corrupts the new `String.value` /
        // `String.coder` slots and the next consumer (HashMap.hash via
        // `String.hashCode`) dereferences a bad pointer, manifesting on
        // Windows as STATUS_ACCESS_VIOLATION (0xC0000005, rc=139).
        // Pinned by `apps/bc_probe/BcProbe`: with these entries skipped,
        // BcProbe reaches `bc.added providers=14` (first println) instead
        // of segfaulting in <10s. Narrow: other String methods
        // (`indexOf`, `length`, `equals`, `charAt`) do not allocate a new
        // backing array and stay JIT-eligible.
        | ("java/lang/String", "toLowerCase")
        | ("java/lang/String", "toUpperCase")
        // SPB.1 (Session 112) — `String.hashCode()` caches its result in
        // the `hash` field on first invocation (`if (h == 0) hash = h;` —
        // a putfield). Under heavy `HashMap.put`-of-String-keys load
        // (Spring's `ClassUtils.registerCommonClasses` puts ~100 String
        // keys via `clazz.getName()`, and Spring config loading puts
        // thousands more), the JIT hits the per-callee threshold and
        // produces the same allocate-then-putfield clobber that bites
        // `Integer.valueOf`. Skip-listing keeps `String.hashCode` in the
        // interpreter for the boot phase. Other String methods that don't
        // putfield (length, charAt, isEmpty) stay JIT-eligible.
        | ("java/lang/String", "hashCode")
        // RBC.1 cont. — `java/security/Provider$ServiceKey.<init>` /
        // `hashCode` are on the hot path of `Provider.put` and exhibit the
        // same allocate-then-putfield pattern as `Integer.valueOf`. The
        // ServiceKey is instantiated inside `parseLegacy` for every
        // algorithm registration; under BC's load (~thousand registrations
        // in <1s), the JIT'd ctor leaves the `algorithm`/`type` slots
        // pointing at stale memory and the next `equals` /  `hashCode`
        // call dereferences a corrupt String pointer.
        | ("java/security/Provider$ServiceKey", "hashCode")
        | ("java/security/Provider$ServiceKey", "equals")
        | ("java/security/Provider", "put")
        | ("java/security/Provider", "parseLegacy")
        | ("java/security/Provider", "putService")
        | ("java/security/Provider", "implPut")
        // EXEC.1 (Session 111) — `apps/executor_probe/ExecProbe` test2
        // builds an `Executors.newFixedThreadPool(4)` and submits 4000
        // tasks that each call `AtomicInteger.incrementAndGet()` and
        // `CountDownLatch.countDown()`. With JIT enabled the run
        // segfaults (rc=139, STATUS_ACCESS_VIOLATION on Windows) right
        // after `test1=42`; with `CRATONVM_DISABLE_JIT=1` the entire test
        // suite passes (test2/test3/test4 all OK).
        //
        // The j.u.c. concurrency primitives are dominated by the same
        // allocate-then-putfield idiom that bites `Integer.valueOf` /
        // `String.toLowerCase`: AQS allocates a fresh `ConditionNode` /
        // `ExclusiveNode` and immediately `putfield`s `prev`/`next`/
        // `waiter` into it, AtomicInteger's CAS retry path produces and
        // unwraps boxed Integers via `Integer.valueOf`, ThreadPoolExecutor
        // re-uses internal `Worker` objects whose ctor stores `firstTask`
        // / `thread` immediately after allocation, etc. Under the 4000-
        // iteration submit/run loop, every one of those callees crosses
        // the OSR (1000) and per-callee (2000) thresholds in the same
        // outer frame, so the regalloc clobber in `patch_self_calls` /
        // `emit_invoke_virtual` (vm/src/jit/x64.rs ~10266 / ~9696) leaves
        // a stale pointer in a callee-saved register and the next field
        // dereference faults.
        //
        // Per the S108 / S109 precedent (`Integer.valueOf`,
        // `String.toLowerCase`), the workaround is a targeted skip list
        // until the underlying regalloc bug is fixed in `x64.rs`. The
        // entries below cover the j.u.c. submit / atomic / AQS hot paths
        // exercised by ExecProbe; other j.u.c. methods stay JIT-eligible.
        //
        // ThreadPoolExecutor + LinkedBlockingQueue submit/run path —
        // both LBQ.offer and LBQ.enqueue allocate a fresh `Node` and
        // immediately `putfield` `item` / `next` into it; under 4000
        // iterations this hits the same allocate-then-putfield
        // miscompile as `Integer.valueOf` and corrupts the queue tail
        // pointer. ThreadPoolExecutor.execute is the public submit
        // entry; runWorker/getTask are the worker-thread loops.
        | ("java/util/concurrent/ThreadPoolExecutor", "execute")
        | ("java/util/concurrent/ThreadPoolExecutor", "runWorker")
        | ("java/util/concurrent/ThreadPoolExecutor", "getTask")
        | ("java/util/concurrent/LinkedBlockingQueue", "offer")
        | ("java/util/concurrent/LinkedBlockingQueue", "enqueue")
        | ("java/util/concurrent/LinkedBlockingQueue", "take")
        | ("java/util/concurrent/LinkedBlockingQueue", "dequeue")
        // AtomicInteger CAS retry loops + Integer.valueOf interaction
        | ("java/util/concurrent/atomic/AtomicInteger", "incrementAndGet")
        | ("java/util/concurrent/atomic/AtomicInteger", "getAndIncrement")
        // CountDownLatch — `countDown` must dispatch correctly to
        // `Sync.tryReleaseShared` which CAS-decrements the count and
        // signals waiters at zero. The JIT'd inner Sync method
        // miscompiles the CAS retry loop's allocate-then-putfield
        // (the retry uses `getStateVolatile` -> `compareAndSetState`),
        // leaving the count stuck above zero so `await` never wakes.
        | ("java/util/concurrent/CountDownLatch", "countDown")
        | ("java/util/concurrent/CountDownLatch", "await")
        | ("java/util/concurrent/CountDownLatch$Sync", "tryReleaseShared")
        | ("java/util/concurrent/CountDownLatch$Sync", "tryAcquireShared")
        // AbstractQueuedSynchronizer/AbstractQueuedLongSynchronizer's own
        // hot dispatch + node alloc paths, and ReentrantReadWriteLock's Sync
        // hot path, are handled by `is_known_miscompile_aqs_family` below —
        // an UNCONDITIONAL check, not gated by
        // `callee_saved_gpr_local_homes_enabled()`. See its doc comment for
        // why: this is a demonstrably distinct, still-reproducing miscompile
        // family from the callee-saved-GPR-local-homes one `a4913d8b` made
        // conditional, so it must not be swept behind that same gate.
        // ReentrantLock guards LBQ — every offer/take takes the lock
        | ("java/util/concurrent/locks/ReentrantLock", "lock")
        | ("java/util/concurrent/locks/ReentrantLock", "unlock")
        // SPB.2 (Session 112 r8) — `Class.getGenericInterfaces()` and the
        // companion `Class.getGenericSuperclass()` / `Class.getGenericInfo()`
        // walk the lazily-built `ClassRepository` cache; the cache
        // population path stores into volatile `genericInfo` immediately
        // after `new ClassRepository(...)`, hitting the same allocate-then-
        // putfield miscompile that bites `Integer.valueOf`. Spring's
        // `SerializableTypeWrapper.lambda$forGenericInterfaces$<hash>$1`
        // hangs on the second invocation (the first pre-warms the JIT,
        // the second is dispatched into JIT'd code that loops). The
        // `ClassRepository.getSuperInterfaces` / `getSuperclass` getters
        // are similarly lazy-then-store. Skip-listing these forces
        // interpreter dispatch and unblocks Spring's deep-generic walk.
        | ("java/lang/Class", "getGenericInterfaces")
        | ("java/lang/Class", "getGenericSuperclass")
        | ("java/lang/Class", "getGenericInfo")
        | ("sun/reflect/generics/repository/ClassRepository", "getSuperInterfaces")
        | ("sun/reflect/generics/repository/ClassRepository", "getSuperclass")
        | ("sun/reflect/generics/repository/ClassRepository", "make")
        | ("sun/reflect/generics/repository/AbstractRepository", "getTree")
        // HIB-PROXY (2026-06-11) — Hibernate ByteBuddy lazy-proxy generation.
        // `ByteBuddyState.make` (invoked from the `lambda$load$0` Callable that
        // `TypeCache.findOrInsert` runs) drives ByteBuddy's runtime subclass
        // build. When BOTH this Hibernate caller AND the
        // `net/bytebuddy/dynamic/*` build chain are JIT-compiled, a receiver is
        // lost across the JIT->JIT call boundary deep in the build, surfacing as
        // `NullPointerException: Cannot write field 'name'` (null `this`) in
        // `InstrumentedType$Default.<init>` — which fails
        // `SingleTableEntityPersister.<init>` and the whole SessionFactory
        // build. Isolated via `CRATONVM_JIT_BISECT_ONLY` (needs both
        // `org/hibernate/bytecode` and `net/bytebuddy/dynamic`) + `BISECT_SKIP`
        // (skipping either `ByteBuddyState.make` or `lambda$load$0` fixes it).
        // The whole ByteBuddy subclass build works with `CRATONVM_DISABLE_JIT=1`
        // — same regalloc/calling-convention class as the bans above. Ban the
        // build entry so Hibernate proxies generate correctly; the underlying
        // codegen defect is tracked for a general fix.
        | ("org/hibernate/bytecode/internal/bytebuddy/ByteBuddyState", "make")
        // NOTE (bug-03 layer C, root-caused 2026-06-15): the former
        // `("java/util/regex/Matcher", "search")` ban is GONE. The defect was
        // not in `search`'s compiled body but in the JIT virtual-dispatch *bail*
        // path: when `jit_invoke_virtual_mic`'s register-arg table overflowed
        // (the 4-arg-with-ctx `Pattern$Node.match` call), `bail_to_interpreter`
        // resolved the callee against the *static* call-site class
        // (`Pattern$Node`) instead of the receiver's runtime class
        // (`Pattern$Start`). `Pattern$Node.match` is a concrete zero-width
        // "accept" node, so `find()` matched empty at every position and
        // `replaceAll("[.]","/")` produced "/o/r/g/...". Fixed in
        // `vm/src/jit/helpers.rs::bail_to_interpreter` (receiver-class
        // resolution for invoke_kind 0/2); `Matcher.search` now compiles
        // correctly under `CRATONVM_JIT_VIRTUAL_TIERUP`. Analysis in
        // docs/wildfly-suite-bugs/bug-03-regex-perf-deployment-build.md.
        // SPB.3 (Session 111 r14) — `apps/SportMe-master`'s Spring Boot
        // bootstrap segfaults (rc=139) deep in Spring's
        // `ConfigurationPropertySources` cache-key build path. Per r13
        // SportMe agent's `CRATONVM_FRAME_TRACE=1` capture, the very last
        // frames before the crash are
        // `MapPropertySource.getPropertyNames` -> `StringUtils.
        // toStringArray(Collection)` -> `HashMap.keysToArray(Object[])`
        // and `SpringIterableConfigurationPropertySource$CacheKey.<init>`
        // / `HashSet.<init>(Collection)` -> `HashMap$KeySet.iterator()`
        // -> `HashMap$KeyIterator.<init>` -> `HashMap$HashIterator.<init>`
        // -> `HashMap$HashIterator.hasNext()`. With `CRATONVM_DISABLE_JIT=1`
        // the seg vanishes (a clean SLF4J `NoSuchMethodError` surfaces
        // instead — the boot reaches a much later phase). The signature
        // matches W2-CHM / RBC.1 / SPB.1 / SPB.2: every one of these
        // methods is allocate-then-putfield-heavy (HashIterator's ctor
        // stores `next`/`expectedModCount`/`current`; `keysToArray`
        // allocates a fresh array and writes via IASTORE per key;
        // `CacheKey` stores `key`/`source` immediately after `new`).
        //
        // Skip-list these per-method (do not blanket-ban the package, to
        // stay non-overlapping with the parallel msyt-segfault agent's
        // Spring Cloud / Eureka entries). `MapPropertySource.getProperty`
        // already shows up in the frame trace right before the crash
        // (it's a getter that `HashMap.get`s the source map then
        // `getProperty(name)`s), so include it too.
        | ("java/util/HashMap$HashIterator", "<init>")
        | ("java/util/HashMap$HashIterator", "hasNext")
        | ("java/util/HashMap$HashIterator", "nextNode")
        | ("java/util/HashMap$KeyIterator", "next")
        | ("java/util/HashMap$EntryIterator", "next")
        | ("java/util/HashMap$ValueIterator", "next")
        | ("java/util/HashMap", "keysToArray")
        | ("java/util/HashMap", "valuesToArray")
        | ("java/util/HashMap", "prepareArray")
        | ("java/util/HashSet", "<init>")
        | ("java/util/HashSet", "iterator")
        | ("java/util/AbstractCollection", "addAll")
        | ("java/util/AbstractCollection", "toArray")
        // Tomcat Bug B (apps/tomcat suite) — `jakarta.el.TestExpressionFactoryCache`
        // hangs in JIT mode but PASSES with `CRATONVM_DISABLE_JIT=1`. The hot
        // loop is `WeakHashMap` copy-construction (`new WeakHashMap<>(cache)` ->
        // `putAll` -> `entrySet().iterator()`), and the JIT mis-compiles the
        // allocate-then-putfield-heavy iterator/entry methods exactly like the
        // `HashMap$HashIterator` family above (`HashIterator.<init>` stores
        // next/current/expectedModCount; `Entry.<init>` is new+putfield), so
        // `HashIterator.hasNext()` never terminates. Mirror the HashMap ban for
        // WeakHashMap. See CRATONVM_BUGS/BUG-B-*.
        | ("java/util/WeakHashMap$HashIterator", "<init>")
        | ("java/util/WeakHashMap$HashIterator", "hasNext")
        | ("java/util/WeakHashMap$EntryIterator", "next")
        | ("java/util/WeakHashMap$KeyIterator", "next")
        | ("java/util/WeakHashMap$ValueIterator", "next")
        | ("java/util/WeakHashMap$Entry", "<init>")
        | ("java/util/WeakHashMap", "getTable")
        | ("java/util/WeakHashMap", "expungeStaleEntries")
        // Tomcat Bug D (apps/tomcat suite) — `org.apache.catalina.filters.
        // TestRemoteCIDRFilter` SIGSEGVs in JIT mode but completes cleanly with
        // `CRATONVM_DISABLE_JIT=1`. Root cause (CRATONVM_BUGS/BUG-D-*): a JIT
        // GC-interaction defect in the `new X; …; invokespecial <init>` sequence
        // when the constructor is itself a GC-capable allocation site — its
        // `<init>` allocates (e.g. `HeapByteBuffer.<init>` news a 16 KiB `byte[]`),
        // triggering a young GC while the freshly-`new`'d object is live in the
        // JIT frame. The corrupting write is an allocation-overlap (the heap walker
        // re-syncs over an 8-byte size desync), NOT a field store: the emitted code
        // spills the return ref to a frame slot across the safepoint and
        // `num_fields` matches the (clean) interpreter, and every putfield/array
        // store is bounds-guarded — so this is the shared deep-`<init>` archetype,
        // not a per-method codegen error. `ByteBuffer.allocate` is the confirmed
        // corruptor via keep-only `CRATONVM_JIT_BISECT_SKIP` bisection (skipping
        // these → 0 corruption across 6 runs vs ~100% baseline crash); the others
        // share the pattern. `Integer.valueOf` (trivial ctor) is NOT a corruptor —
        // keep-only-Integer.valueOf was clean — so it is deliberately left
        // JIT-eligible. Liftable per-package once a heap-write watchpoint pins the
        // exact overlapping store.
        | ("java/nio/ByteBuffer", "allocate")
        | ("org/apache/tomcat/util/buf/MessageBytes", "newInstance")
        | ("org/apache/tomcat/util/buf/MessageBytes$MessageBytesFactory", "newInstance")
        | ("java/util/logging/Level$KnownLevel", "lambda$add$0")
        | ("java/util/logging/Level$KnownLevel", "lambda$add$1")
        | ("java/util/logging/SimpleFormatter", "getLoggingProperty")
        // SPB.9d (Session 117) — eureka-server JIT-mode SEGFAULTs deep
        // inside `org/springframework/core/annotation/*` annotation
        // processing during BeanInfo introspection. Spring's
        // `ExtendedBeanInfo` introspects bean properties for every bean
        // class via `java/beans/Introspector`, which sorts methods using
        // `com/sun/beans/introspect/MethodInfo$MethodOrder.compare` and
        // sorts properties using
        // `org/springframework/beans/ExtendedBeanInfo$PropertyDescriptorComparator.compare`.
        // CRATONVM_DBG_JIT_DISPATCH=1 (run 117a) shows the hot JIT-callees
        // before the segfault are: `MethodInfo$MethodOrder.compare` (149x),
        // `PropertyDescriptorComparator.compare` (17x),
        // `Method.getName()` (360x), `String.compareTo(String)` (167x),
        // and `StringJoiner.<init>(LCS;LCS;LCS;)V` (24x), all called
        // from a JIT-compiled sort comparator chain. With
        // `CRATONVM_DISABLE_JIT=1` the run terminates earlier with the
        // parallel agent's `IllegalArgumentException: Attribute 'type'
        // not found` (rc=1, no segfault); with JIT enabled the
        // miscompiled comparator returns inconsistent ordering, causing
        // `Arrays.sort` to corrupt the comparator's transient state and
        // SEGFAULT during a subsequent allocate-then-putfield sequence
        // (`StringJoiner.<init>` / `Method.toString`). Same archetype as
        // NEW-1.3 / SPB.1.
        //
        // Conservative skip: ban the two comparator entry points plus
        // `StringJoiner.<init>` (the immediate downstream alloc that
        // exhibits the bad pointer). Liftable via
        // `CRATONVM_JIT_ALLOW_PACKAGES=java/util/,com/sun/beans/`.
        | ("java/util/StringJoiner", "<init>")
        | ("com/sun/beans/introspect/MethodInfo$MethodOrder", "compare")
        | (
            "org/springframework/beans/ExtendedBeanInfo$PropertyDescriptorComparator",
            "compare",
        )
        | ("org/springframework/util/StringUtils", "toStringArray")
        | ("org/springframework/core/env/MapPropertySource", "getPropertyNames")
        | ("org/springframework/core/env/MapPropertySource", "getProperty")
        | ("org/springframework/core/env/MapPropertySource", "containsProperty")
        | (
            "org/springframework/boot/context/properties/source/SpringIterableConfigurationPropertySource$CacheKey",
            "<init>",
        )
        | (
            "org/springframework/boot/context/properties/source/SpringIterableConfigurationPropertySource$CacheKey",
            "equals",
        )
        | (
            "org/springframework/boot/context/properties/source/SpringIterableConfigurationPropertySource$CacheKey",
            "hashCode",
        )
        | (
            "org/springframework/boot/context/properties/source/SpringIterableConfigurationPropertySource",
            "getCacheKey",
        )
        | (
            "org/springframework/boot/context/properties/source/SpringIterableConfigurationPropertySource",
            "getPropertyMappings",
        )
        // SPB.3 cont. — `SourcesIterator.fetchNext` / `hasNext` / `next` are
        // the outer loop driving CacheKey.equals/hashCode through
        // ConcurrentReferenceHashMap.get for every PropertySource in the
        // environment. Under SportMe's ~12-source pipeline the loop runs
        // hundreds of times (each property name is searched across every
        // source) and crosses both JIT thresholds. The same allocate-then-
        // putfield miscompile applies — `fetchNext` builds intermediate
        // SourcesIterator state and a fresh ConcurrentReferenceHashMap
        // probe context per call.
        | (
            "org/springframework/boot/context/properties/source/SpringConfigurationPropertySources$SourcesIterator",
            "fetchNext",
        )
        | (
            "org/springframework/boot/context/properties/source/SpringConfigurationPropertySources$SourcesIterator",
            "hasNext",
        )
        | (
            "org/springframework/boot/context/properties/source/SpringConfigurationPropertySources$SourcesIterator",
            "next",
        )
        | (
            "org/springframework/boot/context/properties/source/SpringConfigurationPropertySources$SourcesIterator",
            "isIgnored",
        )
        | (
            "org/springframework/boot/context/properties/source/ConfigurationPropertyState",
            "search",
        )
        // ConcurrentReferenceHashMap.get/getReference/getEntryIfAvailable
        // and the Segment.findInChain/getReference helpers are the inner
        // loop. The Segment ctor allocates fresh Reference[] arrays and
        // stores the bucket head via putfield — same archetype.
        | ("org/springframework/util/ConcurrentReferenceHashMap", "get")
        | ("org/springframework/util/ConcurrentReferenceHashMap", "getReference")
        | (
            "org/springframework/util/ConcurrentReferenceHashMap",
            "getEntryIfAvailable",
        )
        | (
            "org/springframework/util/ConcurrentReferenceHashMap$Segment",
            "getReference",
        )
        | (
            "org/springframework/util/ConcurrentReferenceHashMap$Segment",
            "findInChain",
        )
        | (
            "org/springframework/util/ConcurrentReferenceHashMap$Segment",
            "restructureIfNecessary",
        )
        // AbstractSet.equals dispatches to AbstractCollection.containsAll
        // which iterates and probes HashMap. CacheKey.equals delegates to
        // nullSafeEquals -> AbstractSet.equals — banning these closes the
        // remaining JIT-eligible methods on the SportMe seg path.
        | ("java/util/AbstractSet", "equals")
        | ("java/util/AbstractCollection", "containsAll")
        | ("org/springframework/util/ObjectUtils", "nullSafeEquals")
        | ("org/springframework/util/ObjectUtils", "nullSafeHashCode")
        // SPB.4 cont. (Session 113 r1) — `apps/ms-course-youtube/admin-
        // service` segfaults inside the Spring Boot bind path. The
        // `CRATONVM_FRAME_TRACE=1` capture shows the most-called methods
        // (12k+ / 10k+ invocations) are
        // `java/io/BufferedInputStream.read` / `getBufIfOpen` — those
        // are well past the per-callee threshold (2000) and they
        // putfield-cache `pos`/`count` after refilling the buffer. The
        // SignatureParser hot loop (`current` / `advance`, ~4k+ calls
        // each) reads the generic-signature char array and increments a
        // putfield index — same archetype. These are the inner loops
        // driving `Class.getGenericInterfaces()` (which Spring's
        // SerializableTypeWrapper invokes via reflection on every
        // `@ConfigurationProperties` candidate).
        | ("java/io/BufferedInputStream", "read")
        | ("java/io/BufferedInputStream", "read1")
        | ("java/io/BufferedInputStream", "getBufIfOpen")
        | ("java/io/BufferedInputStream", "ensureOpen")
        | ("java/io/BufferedInputStream", "fill")
        | ("sun/reflect/generics/parser/SignatureParser", "current")
        | ("sun/reflect/generics/parser/SignatureParser", "advance")
        | ("sun/reflect/generics/parser/SignatureParser", "parseTypeSignature")
        | ("sun/reflect/generics/parser/SignatureParser", "parseFieldTypeSignature")
        | ("sun/reflect/generics/parser/SignatureParser", "parseClassTypeSignature")
        | ("sun/reflect/generics/parser/SignatureParser", "parsePackageNameAndSimpleClassTypeSignature")
        | ("sun/reflect/generics/parser/SignatureParser", "parseTypeArguments")
        | ("sun/reflect/generics/parser/SignatureParser", "parseTypeArgument")
        | ("sun/reflect/generics/parser/SignatureParser", "parseIdentifier")
        | ("sun/reflect/generics/parser/SignatureParser", "parseSimpleClassTypeSignature")
        // SPB.4 cont. — `java/util/function/BiPredicate.lambda$or$0`
        // is the synthetic capture that `BiPredicate.or` returns. The
        // tight `ConfigurationPropertyName.isAncestorOf` loop dispatches
        // through this lambda thousands of times; its allocate-on-call
        // capture frame builder is the same archetype. Companion
        // `Predicate.lambda$and$1` etc. follow the same pattern.
        | ("java/util/function/BiPredicate", "lambda$or$0")
        | ("java/util/function/BiPredicate", "lambda$and$0")
        | ("java/util/function/Predicate", "lambda$or$0")
        | ("java/util/function/Predicate", "lambda$and$1")
        | ("java/util/function/Predicate", "lambda$negate$0")
        // SPB.4 cont. — `Reference.<init>` already banned via the
        // generic <init> ban; here we ensure the SoftReference / Reference
        // companion methods (referent setters, get/clear) are also
        // pinned. Spring's ConcurrentReferenceHashMap allocates a
        // SoftReference per entry; `enqueue` / `clear` trigger the
        // GC-side queue manipulation putfield path.
        | ("java/lang/ref/Reference", "clear")
        | ("java/lang/ref/Reference", "clear0")
        | ("java/lang/ref/Reference", "enqueue")
        | ("java/lang/ref/SoftReference", "get")
        // SPB.4 cont. — `ConfigurationPropertyName.isAncestorOf` /
        // `elementsEqual` / `fastElementEquals` etc. are the property-name
        // hot loop in the Spring Boot bind path. The class is in the
        // `org/springframework/boot/context/properties/source/` package
        // which is now blanket-banned via SPB.4b, but make these explicit
        // so the targeted check fires before the package check (and so
        // the bind agent can lift the package ban while keeping these).
        | (
            "org/springframework/boot/context/properties/source/ConfigurationPropertyName",
            "isAncestorOf",
        )
        | (
            "org/springframework/boot/context/properties/source/ConfigurationPropertyName",
            "elementsEqual",
        )
        | (
            "org/springframework/boot/context/properties/source/ConfigurationPropertyName",
            "elementDiffers",
        )
        | (
            "org/springframework/boot/context/properties/source/ConfigurationPropertyName",
            "fastElementEquals",
        )
        | (
            "org/springframework/boot/context/properties/source/ConfigurationPropertyName$ElementsParser",
            "updateType",
        )
        | (
            "org/springframework/boot/context/properties/source/ConfigurationPropertyName$ElementsParser",
            "isValidChar",
        )
        | (
            "org/springframework/boot/context/properties/source/ConfigurationPropertyName$ElementsParser",
            "add",
        )
        // SPB.4 cont. (Session 113 r1) — `CRATONVM_DBG_JIT_COMPILE` shows
        // the LAST JIT compile before the SIGSEGV is `java/lang/Class.
        // copyFields([Ljava/lang/reflect/Field;)[Ljava/lang/reflect/Field;`,
        // which allocates a fresh Field[] and iterates the input,
        // calling `ReflectionFactory.copyField` per slot — the same
        // allocate-then-iterate-and-store archetype that bites
        // `Integer.valueOf` / `String.toLowerCase`. The companion
        // `copyMethods` and `copyConstructors` do the same. The
        // upstream `getDeclaredFields` / `privateGetDeclaredFields` /
        // `reflectionData` build the cache lazily and putfield-store
        // the result; same archetype. Filtering through
        // `Reflection.filterFields` is also on the hot path. Skip-list
        // these to keep them in the interpreter; other Class methods
        // (`getName`, `getSimpleName`, etc.) stay JIT-eligible.
        | ("java/lang/Class", "copyFields")
        | ("java/lang/Class", "copyMethods")
        | ("java/lang/Class", "copyConstructors")
        | ("java/lang/Class", "getDeclaredFields")
        | ("java/lang/Class", "getDeclaredMethods")
        | ("java/lang/Class", "getDeclaredConstructors")
        | ("java/lang/Class", "privateGetDeclaredFields")
        | ("java/lang/Class", "privateGetDeclaredMethods")
        | ("java/lang/Class", "privateGetDeclaredConstructors")
        | ("java/lang/Class", "privateGetPublicFields")
        | ("java/lang/Class", "privateGetPublicMethods")
        | ("java/lang/Class", "reflectionData")
        | ("java/lang/Class", "newReflectionData")
        | ("jdk/internal/reflect/Reflection", "filterFields")
        | ("jdk/internal/reflect/Reflection", "filterMethods")
        | ("jdk/internal/reflect/Reflection", "filter")
        // ReflectionFactory.copyField / copyMethod / copyConstructor —
        // each allocates a fresh Field/Method/Constructor and copies the
        // declaring class / name / type / modifiers / etc. via field
        // stores. Same archetype.
        | ("jdk/internal/reflect/ReflectionFactory", "copyField")
        | ("jdk/internal/reflect/ReflectionFactory", "copyMethod")
        | ("jdk/internal/reflect/ReflectionFactory", "copyConstructor")
        | ("java/lang/reflect/Field", "copy")
        | ("java/lang/reflect/Method", "copy")
        | ("java/lang/reflect/Constructor", "copy")
        // ImmutableCollections.MapN.get / probe — Set.of / Map.of return
        // these immutable collections; their `get` / `probe` walk the
        // open-addressed table. With putfield on the entries' fields they
        // share the allocate-then-putfield issue. Set.of is heavily used
        // by Spring Boot reflection filtering.
        | ("java/util/ImmutableCollections$MapN", "get")
        | ("java/util/ImmutableCollections$MapN", "probe")
        | ("java/util/ImmutableCollections$SetN", "contains")
        | ("java/util/ImmutableCollections$SetN", "probe")
        // SPB.5 (Insurance-backend, Spring Boot 3.2.0) — JIT_DISPATCH trace
        // shows the segfault occurs inside the JIT-compiled
        // `Objects.hash(Object[])` / `Arrays.hashCode(Object[])` /
        // `ArraysSupport.hashCode(Object[],int,int,int)` chain. The first
        // call returns correctly (1625377124), then on the 5th invocation
        // the run STATUS_ACCESS_VIOLATIONs without a matching
        // `JIT_DISPATCH_RET`. The `[Ljava/lang/Object;III)I` overload of
        // `ArraysSupport.hashCode` is a virtual-dispatch hot loop: it
        // iterates the input Object[] and for each element calls
        // `Objects.hashCode(o)` -> `Object.hashCode()`, which is the
        // canonical pattern that miscompiles under the per-callee
        // threshold (W2-CHM / RBC.1 archetype, but applied to a virtual
        // dispatch site instead of an allocate-then-putfield). With
        // `CRATONVM_DISABLE_JIT=1` the bootstrap advances ~16 lines further
        // and surfaces a clean Java-level
        // `MissingWebServerFactoryBeanException` — proof the segfault is
        // JIT-only. Skip-list the entire `Objects.hash` / `Arrays.hashCode`
        // / `ArraysSupport.hashCode` cluster (Spring uses these heavily in
        // `ConfigurationPropertyName.hashCode` and its bind-path keys).
        // Other ArraysSupport intrinsic dispatchers (vectorizedHashCode is
        // a native, not a Java method) are unaffected.
        | ("jdk/internal/util/ArraysSupport", "hashCode")
        | ("java/util/Arrays", "hashCode")
        | ("java/util/Objects", "hash")
        | ("java/util/Objects", "hashCode")
        // FELIX.1 (Session continues 2026-05-16) — `apps/felix-framework-7.0.5/
        // bin/felix.jar` with `CRATONVM_FELIX_REAL=1` SEGVs (rc=139) on
        // Windows during the OSGi `FrameworkFactory` bootstrap. The
        // `CRATONVM_DBG_JIT_ENTRY=1` trace shows the last JIT entry before
        // the crash is
        // `java/lang/reflect/AccessibleObject.setAccessible([Ljava/lang/reflect/AccessibleObject;Z)V`,
        // invoked from the JIT-compiled
        // `org/apache/felix/framework/util/SecureAction.lambda$getAccessor$0`.
        // The JDK implementation of this overload is a simple
        // `for (AccessibleObject obj : array) obj.setAccessible(flag);`
        // loop — same allocate-then-iterate-and-dispatch archetype as
        // `Class.copyFields` / `Objects.hash` (SPB.4 / SPB.5), but applied
        // to a virtual-dispatch site (each element resolves to a different
        // `Method` / `Field` / `Constructor` subclass and calls the
        // per-subclass `setAccessible0` native). Under Felix's bulk
        // reflection setup the loop crosses the per-callee threshold and
        // the next field deref faults inside one of the polymorphic
        // `setAccessible0` callees. With `CRATONVM_DISABLE_JIT=1` the rc
        // changes from 139 to 0 and a clean
        // `java.lang.NullPointerException: Cannot invoke length on null`
        // surfaces at `Main.java:287` (config-properties path; an unrelated
        // Felix data gap). Skip-list the bulk overload plus the
        // SecureAction lambda that drives it; the per-instance
        // `setAccessible(boolean)` overloads stay JIT-eligible. Liftable
        // via `CRATONVM_JIT_ALLOW_PACKAGES=java/lang/reflect/`.
        | ("java/lang/reflect/AccessibleObject", "setAccessible")
        | (
            "org/apache/felix/framework/util/SecureAction",
            "lambda$getAccessor$0",
        )
        // KC26.LR (current session) — `apps/keycloak-26.2.4` with
        // `quarkus-run.jar … show-config` HANGS (rc=124) inside SmallRye
        // Config's copy of the JDK `Properties$LineReader`. SmallRye ships
        // its own `io/smallrye/config/ConfigValueConfigSource$ConfigValueProperties`
        // whose `load0(LineReader)` drives `LineReader.readLine()` in a
        // bytecode loop until `readLine` returns -1 (EOF). The
        // `application.properties` resource is served as a synthetic
        // `ByteArrayInputStream` (from `URL.openStream`); a standalone
        // `CRATONVM_FRAME_TRACE=1` capture shows `readLine` returning a
        // non-negative value forever — `load0` never sees EOF, so it
        // re-`put`s the same lines indefinitely.
        //
        // The InputStream EOF contract itself is correct: a structural
        // copy of `LineReader` (`apps`-style probe) terminates cleanly,
        // and `ByteArrayInputStream.read([BII)I` returns -1 at EOF as
        // verified by direct probes. The hang only reproduces in the full
        // KC26 run, and `CRATONVM_DISABLE_JIT=1` makes it vanish — `read`
        // is then called multiple times and the boot advances past the
        // LineReader to a later, unrelated gap. Classic JIT-miscompile
        // signature (NETTY.1 / W2-CHM archetype): `readLine` is a hot
        // counted loop (`while (inOff < inLimit) { … inByteBuf[inOff++] … }`)
        // whose `iinc inOff` / bounds compare is miscompiled under OSR,
        // so the loop's index never advances and EOF is never reached.
        // The companion `load0` (outer loop) is skip-listed for symmetry.
        // Liftable via `CRATONVM_JIT_ALLOW_PACKAGES=io/smallrye/config/`.
        | (
            "io/smallrye/config/ConfigValueConfigSource$ConfigValueProperties$LineReader",
            "readLine",
        )
        | (
            "io/smallrye/config/ConfigValueConfigSource$ConfigValueProperties",
            "load0",
        )
        // KC-CRED.LAZY (2026-07-01) — Keycloak `CredentialModelTest.
        // canCreateDefaultCredentialModel` returns `null` from a lazy-init
        // `getAdditionalParameters()` getter under JIT, while `--nojit` and
        // HotSpot return `{}`. The bare lazy-init shape compiles correctly in
        // isolation, and the original escape-analysis/scalar-replacement
        // theory was refuted; the failure needs the real
        // PasswordCredentialData / PasswordSecretData classes reached through
        // Jackson deserialization. Keep just these two getters interpreted
        // until the context-sensitive single-pass `getfield`/`putfield`/
        // `areturn` codegen issue can be reproduced in-tree. Liftable via
        // `CRATONVM_JIT_ALLOW_PACKAGES=org/keycloak/models/credential/`.
        | (
            "org/keycloak/models/credential/dto/PasswordCredentialData",
            "getAdditionalParameters",
        )
        | (
            "org/keycloak/models/credential/dto/PasswordSecretData",
            "getAdditionalParameters",
        )
        // BC-ASN1.1 (current session) — `apps/_test-suites/bc-java`'s
        // `org.bouncycastle.asn1.test.RegressionTest` SEGFAULTs (rc=139) on
        // Windows after ~20 tests pass, with the last successful output line
        // being `String: DERT61String.getString() result incorrect`. With
        // `CRATONVM_DISABLE_JIT=1` (or `--nojit`) the full 58-test suite
        // completes cleanly (RC=0). Bisection via `CRATONVM_JIT_BISECT_ONLY=
        // java/util/Calendar` plus `CRATONVM_JIT_BISECT_SKIP` pinpointed
        // `java/util/Calendar.isFieldSet(II)Z` as the single offending JIT
        // entry: with just that one method skipped, the suite completes
        // cleanly and 38/58 tests pass (vs. 20/58 before the SEGFAULT under
        // JIT — the remaining failures are pre-existing data-correctness
        // gaps in BC's String / OID / X509 paths, not JIT crashes).
        //
        // `isFieldSet` is a tiny static helper used internally by Calendar
        // field-mask logic: `(fieldMask & (1 << fieldIndex)) != 0`. Bytecode
        // is `iload_0; iconst_1; iload_1; ishl; iand; ifeq …; iconst_1/0;
        // ireturn` — a four-op bit test. The JIT'd version produces an
        // incorrect boolean for at least one (mask, field) input, which
        // mis-routes a downstream `selectFields` / `computeFields` branch
        // in `GregorianCalendar` and ultimately surfaces as a raw native
        // SEGFAULT (Windows STATUS_ACCESS_VIOLATION) when a corrupt index
        // is fed back into an `int[]` slot. Same archetype as NETTY.1's
        // `Arrays.fill` counted-loop miscompile, but on a much smaller
        // bytecode shape — likely a regalloc / x64 codegen bug for the
        // `ishl` / `iand` / `ifeq` short basic block. Liftable via
        // `CRATONVM_JIT_ALLOW_PACKAGES=java/util/`.
        | ("java/util/Calendar", "isFieldSet")
        // ECJ Eclipse-JDT issue #23 (BC SM2 followup, 2026-05-28) — the
        // JIT-compiled `org/eclipse/jdt/internal/compiler/util/HashtableOfInt.rehash`
        // produces a `keyTable` whose backing int[] header is corrupted
        // (`val_cid=0 val_kind=0 val_arrlen=0x01010101` in the JIT-PFO
        // trace), causing the next `HashtableOfInt.put` to divide by
        // zero. Bisected via `CRATONVM_JIT_BISECT_SKIP`:
        // `org/eclipse/jdt/internal/compiler/util/HashtableOfInt.rehash`
        // closes the bug. `--nojit` and disabling the inline-TLAB
        // `new` codegen path BOTH still fail, so the miscompile is
        // somewhere in the rehash() method's other JIT-emitted code
        // (loop iteration over the old keyTable, the three trailing
        // putfield_object stores that copy the new instance's fields,
        // or the JIT's tracking of stack slots across the dup/new/
        // invokespecial sequence). The deeper investigation needs a
        // Windows-side debugger watchpoint on the int[]'s header bytes.
        // Pre-emptively include the sibling Hashtable* classes — they
        // share the same `rehash` shape (allocate `new HashtableOfX`,
        // iterate, replace fields) and would surface the same bug if
        // the JIT ever crosses their per-method threshold.
        | ("org/eclipse/jdt/internal/compiler/util/HashtableOfInt", "rehash")
        | ("org/eclipse/jdt/internal/compiler/util/HashtableOfLong", "rehash")
        | ("org/eclipse/jdt/internal/compiler/util/HashtableOfObject", "rehash")
        | ("org/eclipse/jdt/internal/compiler/util/HashtableOfObjectToInt", "rehash")
        | ("org/eclipse/jdt/internal/compiler/util/HashtableOfPackage", "rehash")
        | ("org/eclipse/jdt/internal/compiler/util/HashtableOfType", "rehash")
        | ("org/eclipse/jdt/internal/compiler/util/HashtableOfModule", "rehash")
        // bc-math-ec JUnit-3 AllTests SEGV (BC SM2 followup, 2026-05-28)
        // — when the JIT compiles `junit/textui/TestRunner.main`, the
        // `new TestRunner; dup; invokespecial <init>; astore_1; aload_1;
        // invokevirtual start(args)` sequence miscompiles and the next
        // minor GC walker finds object headers reading byte[] data
        // (`class_id=36 array_length=54 num_slots=60` in the trace).
        // Same dup/new/invokespecial/astore pattern that bites
        // `HashtableOfInt.rehash` above; the regalloc / stack-slot
        // tracking across the invokespecial likely drops the dup'd
        // reference. Bisection via
        // `CRATONVM_JIT_BISECT_ONLY=...,junit/textui/TestRunner` +
        // `BISECT_SKIP=junit/textui/TestRunner.main` closes the bug.
        // `--nojit` works; disabling inline-TLAB `new` doesn't.
        | ("junit/textui/TestRunner", "main")
        // dacapo-lucene + H2 TestAll SEGV (BC SM2 followup, 2026-05-28)
        // — bisected via `CRATONVM_JIT_BISECT_ONLY` per-`jdk/internal/`
        // subpackage: only `jdk/internal/ref/` triggers the SEGV, and
        // within it the single offending method is `CleanerImpl.run`.
        // JIT-compiling CleanerImpl.run produces code whose effect at
        // GC time leaves the walker reading random byte[] data as
        // object headers (`class_id=1785409400 = 0x6A617661 = "java"`
        // ASCII signature in the diagnostic dump). CleanerImpl.run is
        // the Cleaner thread's main loop — it pulls phantom-cleanable
        // refs off the queue and invokes their thunks. The bug also
        // fires under H2 TestAll (same root cause, different witness).
        | ("jdk/internal/ref/CleanerImpl", "run")
        // HIB-CV-02 (2026-06-13/14) — the provisional ban on Xerces'
        // `XMLEntityScanner.skipString` was REMOVED after re-verification:
        // `LocalXmlResourceResolverTest` (the original witness) passes 23/23
        // JIT-on with skipString compilable, and a faithful instance-method
        // replica forced through the JIT (Rep5) compiles + runs correctly. The
        // underlying "backward compare loop" miscompile is no longer
        // reproducible on dev (fixed by intervening JIT codegen work — cf. the
        // bug-H/I exception-routing + bug-24 inline-cache fixes); the only
        // residual is JIT-helper *slowness*, a separate perf concern, not a
        // hang. See apps/hibernate-orm/cratonvm-bug-reports/HIB-CV-02.
        // SB-17 (2026-06-14) — Groovy `GroovyClassLoader.parseClass` JIT heap
        // corruption. JIT-compiling `groovy/lang/GroovyClassLoader.doParseClass`
        // deterministically writes the int `512` into the `array_length` header
        // word (offset 12) of an unrelated live `kind=Object` heap object, which
        // makes the non-moving young sweep bail on the "inconsistent header"
        // (`array_length=512` on a kind=Object), re-sync at a wrong boundary, and
        // reclaim live objects — surfacing downstream as either
        // `AbstractMethodError: java/util/Deque.iterator()` (a reclaimed
        // `LinkedList` in `CompilationUnit.phaseOperations[]`) or the
        // `CompactValue::object: pointer 0x… exceeds 47-bit` panic
        // (`AstBuilder.buildAST` dispatch reads a header whose lock-state bits are
        // spuriously `Forwarded`). Isolated via `CRATONVM_JIT_BISECT_ONLY` /
        // `BISECT_SKIP` bisection on a quiet machine (java/* clean, org/antlr
        // clean, org/codehaus/groovy/{runtime,control,ast,classgen} clean,
        // vmplugin/transform/MetaClassImpl clean) down to this single method:
        // keep-only-skip of `GroovyClassLoader.doParseClass` → 0 corruption across
        // runs vs 6 with it JIT-eligible. `doParseClass` itself emits NO direct
        // heap stores (only `[rbp-…]` frame spills + indirect helper/method
        // calls) and contains no longs / arrays / the value 512 in its bytecode —
        // so the defect is a value-typing/regalloc/oop-map miscompile of this
        // large method (reused local slot 6: url→collector; the run logs a
        // "long↔object NaN-box collision degraded to Value::Long", i.e. a `long`
        // slot read as an object). NOT bounds-check elimination (NO_BCE still
        // corrupts), NOT inline-`new` (DISABLE_INLINE_NEW still corrupts), NOT
        // loop unrolling, NOT selective promotion (NO_SELECTIVE_PROMOTE still
        // corrupts). Same "JIT'd method corrupts a neighbour's int[] header"
        // archetype as HashtableOfInt.rehash / CleanerImpl.run above. Ban narrowly
        // (method runs correctly interpreted — skip → parse advances to the next
        // phase exactly like `--nojit`) until the codegen defect is pinned with a
        // header-write watchpoint. Liftable via
        // `CRATONVM_JIT_ALLOW_PACKAGES=groovy/lang/`.
        | ("groovy/lang/GroovyClassLoader", "doParseClass")
    )
}

/// Targeted list for `java.util.concurrent.locks`' queue-synchronizer family
/// — `AbstractQueuedSynchronizer` (classic, `int state`) and
/// `AbstractQueuedLongSynchronizer` (JDK 25+, `long state`; used by
/// `ReentrantReadWriteLock$Sync`), their respective `Node`/`ConditionObject`
/// inner classes, and `ReentrantReadWriteLock$Sync`/`HoldCounter`/
/// `ThreadLocalHoldCounter`.
///
/// UNCONDITIONAL — deliberately NOT gated by
/// `callee_saved_gpr_local_homes_enabled()`, unlike the rest of
/// `is_known_miscompile`. `a4913d8b` ("disable callee-saved GPR local
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
/// family; see the removed entries' history in `is_known_miscompile` for
/// the original CountDownLatch-driven discovery.
///
/// `tryInitializeHead` in particular allocates the CLH queue's sentinel
/// `ExclusiveNode` and immediately `casHead`s it in — an allocate-then-CAS
/// hazard structurally identical to the allocate-then-putfield family, but
/// running only once per lock instance (its first-ever contended acquire),
/// which is exactly why light-contention repros (a handful of threads,
/// brief hold times) never trigger it while heavy-contention ones
/// (many threads, deep contention) reliably do — matching the observed gap
/// between an initial 4-thread stress repro (passed) and the real
/// Elasticsearch `LongRandomBinaryDocValuesRangeQueryTests` scenario and a
/// 16-thread synthetic repro (both hang identically).
fn is_known_miscompile_aqs_family(class_name: &str, method_name: &str) -> bool {
    matches!(
        (class_name, method_name),
        // --- AbstractQueuedSynchronizer (classic, int state) ---
        ("java/util/concurrent/locks/AbstractQueuedSynchronizer", "acquire")
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
            // in `is_known_miscompile` are gated by the GPR-local-home switch,
            // leaving these allocate/CAS queue paths JIT-eligible by default.
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
            | ("java/util/concurrent/ConcurrentLinkedQueue", "tryCasSuccessor")
            | ("java/util/concurrent/ConcurrentLinkedQueue", "updateHead")
            | ("java/util/concurrent/ConcurrentLinkedQueue", "succ")
            | ("java/util/concurrent/ConcurrentLinkedQueue", "poll")
            | ("java/util/concurrent/ConcurrentLinkedQueue", "skipDeadNodes")
            | ("java/util/concurrent/ConcurrentLinkedQueue", "<init>")
            | ("java/util/concurrent/ConcurrentLinkedQueue$Node", "<init>")
            | ("java/util/concurrent/ConcurrentLinkedQueue$Node", "appendRelaxed")
            | ("java/util/concurrent/ConcurrentLinkedQueue$Node", "casItem")
    )
}

fn is_antlr_prediction_context_miscompile(class_name: &str, method_name: &str) -> bool {
    matches!(
        (class_name, method_name),
        (
            "groovyjarjarantlr4/v4/runtime/atn/PredictionContext",
            "calculateHashCode" | "hashCode"
        ) | (
            "groovyjarjarantlr4/v4/runtime/atn/PredictionContext$IdentityEqualityComparator",
            "hashCode"
        ) | (
            "groovyjarjarantlr4/v4/runtime/atn/SingletonPredictionContext",
            "equals" | "isEmpty" | "size"
        ) | (
            "groovyjarjarantlr4/v4/runtime/misc/ObjectEqualityComparator",
            "equals"
        )
    )
}

/// True for a dynamically generated `$ProxyN` class's exact simple-name
/// shape (`emit_proxy_classfile` / `build_proxy_spec_for` in
/// `native-builtins/src/lib.rs`), regardless of package — `$Proxy` followed
/// by one or more ASCII digits and nothing else. Deliberately narrow (exact
/// shape, not a substring/prefix check) so it can never match ordinary user
/// code that happens to declare a class starting with `$Proxy` (HotSpot
/// reserves this exact pattern for its own generated proxies too, so real
/// code doing this is already vanishingly rare).
fn is_generated_proxy_class(class_name: &str) -> bool {
    let simple_name = class_name.rsplit('/').next().unwrap_or(class_name);
    match simple_name.strip_prefix("$Proxy") {
        Some(rest) if !rest.is_empty() => rest.bytes().all(|b| b.is_ascii_digit()),
        _ => false,
    }
}

fn callee_saved_gpr_local_homes_enabled() -> bool {
    #[cfg(not(target_arch = "x86_64"))]
    {
        return true;
    }

    #[cfg(target_arch = "x86_64")]
    std::env::var("CRATONVM_JIT_ENABLE_CALLEE_SAVED_GPR_LOCALS")
        .ok()
        .map(|v| {
            matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "on" | "yes"
            )
        })
        .unwrap_or(false)
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
            std::env::var("CRATONVM_JIT_ALLOW_PACKAGES")
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
    fn java_util_regalloc_family_lifted_under_safe_default() {
        // The targeted table still records the historical NEW-1.3 member, but
        // x64 no longer uses callee-saved GPR local homes by default, so the
        // method is eligible even under Conservative policy.
        assert!(is_known_miscompile("java/util/HashMap", "put"));
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
    fn snakeyaml_emitter_emit_skipped_conservatively() {
        assert_eq!(
            check(
                "org/yaml/snakeyaml/emitter/Emitter",
                "emit",
                false,
                true,
                SkipPolicy::Conservative,
            ),
            Some(SkipReason::RustJvmTestFixture)
        );
        assert_eq!(
            check(
                "org/yaml/snakeyaml/emitter/Emitter",
                "writeWhitespace",
                false,
                true,
                SkipPolicy::Conservative,
            ),
            None,
            "the ES interval crash guard must stay exact to Emitter.emit"
        );
    }

    #[test]
    fn snakeyaml_emitter_emit_lifts_with_allow_packages() {
        assert_eq!(
            check_with(
                "org/yaml/snakeyaml/emitter/Emitter",
                "emit",
                false,
                true,
                SkipPolicy::Conservative,
                &["org/yaml/snakeyaml/emitter/"],
            ),
            None
        );
        assert_eq!(
            check(
                "org/yaml/snakeyaml/emitter/Emitter",
                "emit",
                false,
                true,
                SkipPolicy::Aggressive,
            ),
            None
        );
    }

    #[test]
    fn hibernate_temporal_residual_package_skipped_conservatively() {
        for cls in [
            "org/hibernate/dialect/H2Dialect",
            "org/hibernate/type/descriptor/sql/internal/DdlTypeImpl",
            "org/hibernate/testing/jdbc/SharedDriverManagerConnectionProvider",
            "org/hibernate/orm/test/type/temporal/InstantTests",
            "org.hibernate.dialect.H2Dialect",
        ] {
            assert_eq!(
                check(
                    cls,
                    "getRawTypeNames",
                    false,
                    true,
                    SkipPolicy::Conservative
                ),
                Some(SkipReason::RustJvmTestFixture),
                "{cls} should stay interpreted under the conservative Hibernate temporal guard"
            );
        }
    }

    #[test]
    fn hibernate_temporal_residual_package_lifts_under_aggressive_policy() {
        assert_eq!(
            check(
                "org/hibernate/type/descriptor/sql/internal/DdlTypeImpl",
                "getRawTypeNames",
                false,
                true,
                SkipPolicy::Aggressive,
            ),
            None,
            "aggressive policy should lift the Hibernate temporal guard"
        );
    }

    #[test]
    fn hibernate_temporal_residual_package_lifts_with_allow_packages() {
        assert_eq!(
            check_with(
                "org/hibernate/testing/jdbc/SharedDriverManagerConnectionProvider",
                "onDefaultTimeZoneChange",
                false,
                true,
                SkipPolicy::Conservative,
                &["org/hibernate/"],
            ),
            None
        );
        assert_eq!(
            check_with(
                "org.hibernate.testing.jdbc.SharedDriverManagerConnectionProvider",
                "onDefaultTimeZoneChange",
                false,
                true,
                SkipPolicy::Conservative,
                &["org.hibernate."],
            ),
            None
        );
    }

    #[test]
    fn jaxb_mapping_package_skipped_conservatively_and_lifts_for_bisection() {
        for cls in [
            "org/glassfish/jaxb/runtime/v2/runtime/reflect/Accessor",
            "org.glassfish.jaxb.runtime.v2.runtime.reflect.Accessor",
        ] {
            assert_eq!(
                check(cls, "get", false, true, SkipPolicy::Conservative),
                Some(SkipReason::RustJvmTestFixture),
                "{cls} should stay interpreted under the JAXB mapping guard"
            );
        }
        assert_eq!(
            check_with(
                "org/glassfish/jaxb/runtime/v2/runtime/reflect/Accessor",
                "get",
                false,
                true,
                SkipPolicy::Conservative,
                &["org/glassfish/jaxb/"],
            ),
            None
        );
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
        // ES-HANG-01 remains recorded in the targeted table, but the callee-
        // saved GPR local-home path that required the ban is default-off.
        for (cls, m) in [
            ("java/util/WeakHashMap$KeySpliterator", "tryAdvance"),
            ("java/util/WeakHashMap$KeySpliterator", "forEachRemaining"),
            ("java/util/WeakHashMap$ValueSpliterator", "tryAdvance"),
            ("java/util/WeakHashMap$ValueSpliterator", "forEachRemaining"),
            ("java/util/WeakHashMap$EntrySpliterator", "tryAdvance"),
            ("java/util/WeakHashMap$EntrySpliterator", "forEachRemaining"),
        ] {
            assert!(is_known_miscompile(cls, m));
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
    fn jdt_parser_package_skips_under_conservative() {
        assert_eq!(
            check(
                "org/eclipse/jdt/internal/compiler/parser/Parser",
                "consumeRule",
                false,
                true,
                SkipPolicy::Conservative,
            ),
            Some(SkipReason::RustJvmTestFixture),
            "JDT Parser.consumeRule must stay interpreted for the Jasper parser residual"
        );
        assert_eq!(
            check(
                "org/eclipse/jdt/internal/compiler/parser/Parser",
                "consumeTypeImportOnDemandDeclarationName",
                false,
                true,
                SkipPolicy::Conservative,
            ),
            Some(SkipReason::RustJvmTestFixture),
            "the Jasper residual guard covers the JDT parser package, not only consumeRule"
        );
        assert_eq!(
            check(
                "org/eclipse/jdt/internal/compiler/lookup/Scope",
                "getType",
                false,
                true,
                SkipPolicy::Conservative,
            ),
            None,
            "the Jasper residual guard is intentionally limited to the parser package"
        );
    }

    #[test]
    fn keycloak_picocli_smallrye_packages_skip_under_conservative() {
        for (class_name, allow) in [
            // `org/keycloak/quarkus/runtime/cli/` itself now has a targeted
            // carve-out (KC26-PIC.2, below) — exercise a sibling
            // org/keycloak/ package here to keep covering the general
            // (non-carved-out) ban.
            ("org/keycloak/models/RealmModel", "org/keycloak/"),
            ("picocli/CommandLine", "picocli/"),
            // `io/smallrye/config/` itself now has a targeted carve-out
            // (KC26-PIC.2, below) — exercise a sibling io.smallrye package
            // here to keep covering the general (non-carved-out) ban.
            ("io/smallrye/mutiny/Uni", "io/smallrye/"),
        ] {
            assert_eq!(
                check(class_name, "example", false, true, SkipPolicy::Conservative),
                Some(SkipReason::RustJvmTestFixture),
                "{class_name} must stay interpreted under the conservative policy"
            );
            assert_eq!(
                check(class_name, "example", false, true, SkipPolicy::Aggressive),
                None,
                "aggressive policy must lift {class_name}"
            );
            assert_eq!(
                check_with(
                    class_name,
                    "example",
                    false,
                    true,
                    SkipPolicy::Conservative,
                    &[allow],
                ),
                None,
                "allow-package entry must lift {class_name}"
            );
        }
    }

    #[test]
    fn kc26_pic2_smallrye_relocate_carveout_lifted_under_conservative() {
        // KC26-PIC.2: these packages are carved OUT of the KC26-PIC.1 ban
        // (RelocateConfigSourceInterceptor exponential fan-out is tractable
        // under JIT, catastrophic under the interpreter; Picocli.java's own
        // validateConfig/validateProperty loop over every CLI option was a
        // secondary bottleneck once the interceptor calls got fast) and so
        // must be JIT-eligible even under the default Conservative policy.
        for class_name in [
            "io/smallrye/config/SmallRyeConfig",
            "io/smallrye/config/RelocateConfigSourceInterceptor",
            "org/keycloak/quarkus/runtime/configuration/PropertyMappingInterceptor",
            "org/keycloak/quarkus/runtime/configuration/NestedPropertyMappingInterceptor",
            "org/keycloak/quarkus/runtime/cli/Picocli",
            "org/keycloak/quarkus/runtime/cli/command/AbstractCommand",
        ] {
            assert_eq!(
                check(class_name, "example", false, true, SkipPolicy::Conservative),
                None,
                "{class_name} must be carved out of the ban under the conservative policy"
            );
        }
        // The rest of org/keycloak/ (outside .../configuration/ and .../cli/)
        // and all of picocli/ must remain banned — the carve-out is
        // deliberately narrow.
        for class_name in [
            "org/keycloak/models/RealmModel",
            "picocli/CommandLine",
        ] {
            assert_eq!(
                check(class_name, "example", false, true, SkipPolicy::Conservative),
                Some(SkipReason::RustJvmTestFixture),
                "{class_name} must remain interpreted — the KC26-PIC.2 carve-out must not widen to this package"
            );
        }
    }

    #[test]
    fn rxjava3_package_skips_under_conservative_for_keycloak_reactive_wait() {
        let class_name =
            "io/reactivex/rxjava3/internal/operators/flowable/BlockingFlowableIterable";
        assert_eq!(
            check(class_name, "hasNext", false, true, SkipPolicy::Conservative,),
            Some(SkipReason::RustJvmTestFixture),
            "RxJava3 must stay interpreted under the conservative policy"
        );
        assert_eq!(
            check(class_name, "hasNext", false, true, SkipPolicy::Aggressive),
            None,
            "aggressive policy must lift the RxJava3 package ban"
        );
        assert_eq!(
            check_with(
                class_name,
                "hasNext",
                false,
                true,
                SkipPolicy::Conservative,
                &["io/reactivex/"],
            ),
            None,
            "CRATONVM_JIT_ALLOW_PACKAGES=io/reactivex/ must lift RxJava3"
        );
    }

    #[test]
    fn elasticsearch_vector_diskbbq_hang_cluster_stays_interpreted_by_default() {
        for class_name in [
            "org/elasticsearch/index/codec/vectors/diskbbq/DocIdsWriterTests",
            "org/elasticsearch/index/codec/vectors/diskbbq/ES920DiskBBQVectorsFormatTests",
            "org/elasticsearch/index/codec/vectors/diskbbq/es94/ES940DiskBBQVectorsFormatTests",
            "org/elasticsearch/index/codec/vectors/diskbbq/next/ESNextDiskBBQVectorsFormatTests",
            "org/elasticsearch/index/codec/vectors/es93/ES93FlatVectorFormatTests",
            "org/elasticsearch/index/codec/vectors/es93/ES93HnswBitVectorsFormatTests",
            "org/elasticsearch/search/vectors/IVFKnnFloatSlicedVectorQueryTests",
        ] {
            assert_eq!(
                check(
                    class_name,
                    "testBody",
                    false,
                    true,
                    SkipPolicy::Conservative,
                ),
                Some(SkipReason::RustJvmTestFixture),
                "{class_name} must stay interpreted under the conservative policy"
            );
            assert_eq!(
                check(class_name, "testBody", false, true, SkipPolicy::Aggressive),
                None,
                "aggressive policy must still lift {class_name} for bisection"
            );
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
            Some(SkipReason::RustJvmTestFixture),
            "Lucene vector leaves must stay interpreted by the separate Lucene package ban"
        );
    }

    #[test]
    fn hamcrest_matchers_stay_interpreted_for_elasticsearch_assertions() {
        let class_name = "org/hamcrest/core/IsEqual";
        assert_eq!(
            check(class_name, "matches", false, true, SkipPolicy::Conservative),
            Some(SkipReason::RustJvmTestFixture),
            "Hamcrest matchers must stay interpreted under the conservative policy"
        );
        assert_eq!(
            check(class_name, "matches", false, true, SkipPolicy::Aggressive),
            None,
            "aggressive policy must lift the Hamcrest package ban"
        );
        assert_eq!(
            check_with(
                class_name,
                "matches",
                false,
                true,
                SkipPolicy::Conservative,
                &["org/hamcrest/"],
            ),
            None,
            "CRATONVM_JIT_ALLOW_PACKAGES=org/hamcrest/ must lift Hamcrest"
        );
    }

    #[test]
    fn json_smart_parser_package_stays_interpreted_for_spring_jsonpath() {
        for (class_name, method) in [
            ("net/minidev/json/parser/JSONParserString", "read"),
            ("net/minidev/json/parser/JSONParserString", "readS"),
            ("net/minidev/json/parser/JSONParserBase", "skipSpace"),
        ] {
            assert_eq!(
                check(class_name, method, false, true, SkipPolicy::Conservative),
                Some(SkipReason::RustJvmTestFixture),
                "{class_name}.{method} must stay interpreted under the conservative policy"
            );
            assert_eq!(
                check(class_name, method, false, true, SkipPolicy::Aggressive),
                None,
                "aggressive policy must lift the json-smart parser guard"
            );
            assert_eq!(
                check_with(
                    class_name,
                    method,
                    false,
                    true,
                    SkipPolicy::Conservative,
                    &["net/minidev/json/parser/"],
                ),
                None,
                "CRATONVM_JIT_ALLOW_PACKAGES=net/minidev/json/parser/ must lift the guard"
            );
        }
        assert_eq!(
            check(
                "net/minidev/json/writer/JsonReaderI",
                "read",
                false,
                true,
                SkipPolicy::Conservative,
            ),
            None,
            "the json-smart guard is intentionally limited to the parser package"
        );
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
    fn antlr_coldpath_blanket_ban_holds_by_default() {
        assert_eq!(
            check(
                "groovyjarjarantlr4/v4/runtime/atn/ParserATNSimulator",
                "closure_",
                false,
                true,
                SkipPolicy::Conservative,
            ),
            Some(SkipReason::RustJvmTestFixture),
            "ANTLR cold-path methods stay interpreted by default"
        );
    }

    #[test]
    fn antlr_coldpath_validation_lifts_non_bad_atn_methods() {
        assert_eq!(
            check_with(
                "groovyjarjarantlr4/v4/runtime/atn/ParserATNSimulator",
                "closure_",
                false,
                true,
                SkipPolicy::Conservative,
                &["groovyjarjarantlr4/"],
            ),
            None,
            "CRATONVM_JIT_ALLOW_PACKAGES=groovyjarjarantlr4/ is the cold-path validation lift"
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
        for cls in [
            "org/keycloak/models/credential/dto/PasswordCredentialData",
            "org/keycloak/models/credential/dto/PasswordSecretData",
        ] {
            assert!(is_known_miscompile(cls, "getAdditionalParameters"));
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

    #[test]
    fn classify_complex_ctor_with_putfield() {
        // aload_0; aload_0; iconst_1; putfield #2; return
        let bc = vec![0x2a, 0x2a, 0x04, 0xb5, 0x00, 0x02, 0xb1];
        assert_eq!(classify_init_complexity(&bc), InitComplexity::Complex);
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

    #[test]
    fn complex_ctor_keeps_constructor_ban() {
        let complex_bc = vec![0x2a, 0x2a, 0x04, 0xb5, 0x00, 0x02, 0xb1];
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
    // `starts_with("java/util/")` or `starts_with("cratonvm/")` ban
    // that covers methods NOT in the `is_known_miscompile` targeted
    // list. The targeted list is the only permitted mechanism for
    // banning specific methods going forward.
    // =================================================================

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
    fn tier1_skip_list_targeted_entries_are_only_known_miscompiles() {
        // The targeted list must contain ONLY the documented
        // miscompiles. If someone adds a new entry they must also
        // update this test (which forces a conscious review).
        assert!(is_known_miscompile("java/util/HashMap", "put"));
        assert!(is_known_miscompile("java/util/HashMap", "get"));
        assert!(is_known_miscompile("java/util/HashMap", "resize"));
        assert!(!is_known_miscompile("cratonvm/TckLang", "exc_hierarchy"));
        assert!(is_known_miscompile(
            "org/keycloak/models/credential/dto/PasswordCredentialData",
            "getAdditionalParameters"
        ));
        assert!(is_known_miscompile(
            "org/keycloak/models/credential/dto/PasswordSecretData",
            "getAdditionalParameters"
        ));
        // Everything else must NOT be in the list.
        assert!(!is_known_miscompile("java/util/HashMap", "size"));
        assert!(!is_known_miscompile("java/util/ArrayList", "add"));
        assert!(!is_known_miscompile("cratonvm/TckLang", "str_length"));
        assert!(!is_known_miscompile(
            "org/keycloak/models/credential/CredentialModel",
            "getPasswordCredentialData"
        ));
        assert!(!is_known_miscompile("com/example/Foo", "bar"));
    }

    #[test]
    fn bouncycastle_crypto_hotpath_carveout_keeps_math_ec_banned() {
        assert_eq!(
            check(
                "org/bouncycastle/math/ec/ECPoint",
                "normalize",
                false,
                true,
                SkipPolicy::Conservative
            ),
            Some(SkipReason::RustJvmTestFixture)
        );
        assert_eq!(
            check(
                "org/bouncycastle/crypto/BufferedBlockCipher",
                "getUpdateOutputSize",
                false,
                true,
                SkipPolicy::Conservative
            ),
            None
        );
        assert_eq!(
            check(
                "org/bouncycastle/crypto/DefaultBufferedBlockCipher",
                "getUpdateOutputSize",
                false,
                true,
                SkipPolicy::Conservative
            ),
            None
        );
        assert_eq!(
            check(
                "org/bouncycastle/crypto/engines/CAST5Engine",
                "init",
                false,
                true,
                SkipPolicy::Conservative
            ),
            Some(SkipReason::RustJvmTestFixture)
        );
    }
}
