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
//! | `java/util/*`                   | conservative-only — overridable via `RUSTJVM_JIT_ALLOW_PACKAGES` |
//! | `rustjvm/*` (legacy fixtures)   | conservative-only — overridable via `RUSTJVM_JIT_ALLOW_PACKAGES` |
//! | `java/lang/*`                   | **REMOVED** (NEW-1.2 fixed JIT instanceof) |
//! | `rustjvm/Tck*`                  | **REMOVED** (NEW-1.2 fixed JIT instanceof) |
//! | `rustjvm/*FinalizerTest*`       | **REMOVED** (NEW-1.5 conservative JIT root scan) |
//! | unnamed-thread methods          | targeted — thread-local JIT state pre-init guard |
//!
//! ## NEW-1 progress (2026-04-14)
//!
//! - **NEW-1.2 (instanceof / checkcast)** — closed. The JIT helpers
//!   [`crate::jit::helpers::jit_instanceof`] / [`crate::jit::helpers::jit_checkcast`]
//!   now load the target class on demand and walk the lambda-proxy and
//!   synthetic-implements fallbacks, exactly like the interpreter. Result:
//!   the `java/lang/*` and `rustjvm/Tck*` blanket bans are gone — they were
//!   masking this bug, not protecting against an unrelated one.
//! - **NEW-1.5 (conservative JIT root scan)** — closed. The GC root walker now
//!   scans every active JIT spill area and pins any qword that lies inside the
//!   heap arena. This is the production-safe stop-gap for the absence of
//!   precise JIT oop maps; it removes the `FinalizerTest` ban without risking
//!   collected pointers in JIT frames. Precise oop maps remain a future item.
//! - **NEW-1.3 / NEW-1.4** — `<init>`, `<clinit>`, interface defaults, and the
//!   `java/util/*` / `rustjvm/*` package bans remain because they correspond to
//!   distinct, not-yet-fixed JIT correctness gaps (regalloc parameter mapping
//!   for interface defaults; `<clinit>` re-entrancy during constant-pool
//!   resolution; hash-table loop regalloc miscompile). They can be lifted
//!   per-package via the `RUSTJVM_JIT_ALLOW_PACKAGES` environment variable for
//!   development / benchmarking.
//!
//! When [`SkipPolicy::Aggressive`] is selected, the package bans
//! (`java/util/`, `rustjvm/`) are lifted. This is intended for development to
//! surface latent JIT bugs and for benchmarking the maximal reachable code
//! path. Production builds default to [`SkipPolicy::Conservative`].

/// JIT eligibility policy.
///
/// Selects whether the broad correctness-driven blanket bans are applied
/// (`Conservative`) or only the targeted per-method bans are applied
/// (`Aggressive`). Mapped from `VmConfig::jit_aggressive_compilation`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SkipPolicy {
    /// Apply all blanket bans (`java/util/`, `java/lang/`, `rustjvm/Tck`,
    /// `rustjvm/`). Default for production.
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
    /// `rustjvm/*` test fixture — broad ban for legacy reasons. (A1.4)
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
            0x10 => 2, // bipush
            0x11 => 3, // sipush
            0x12 => 2, // ldc
            0x13 | 0x14 => 3, // ldc_w / ldc2_w
            0x15..=0x19 => 2, // iload..aload
            0x1a..=0x35 => 1, // iload_n..saload
            0x36..=0x3a => 2, // istore..astore
            0x3b..=0x56 => 1, // istore_n..sastore
            0x57..=0x5f => 1, // pop..swap
            0x60..=0x83 => 1, // arithmetic
            0x84 => 3, // iinc
            0x85..=0x93 => 1, // conversions
            0x94..=0x98 => 1, // lcmp/fcmpl/fcmpg/dcmpl/dcmpg
            0x99..=0xa6 => 3, // ifeq..if_acmpne
            0xa7 => 3, // goto
            0xa8 => 3, // jsr
            0xa9 => 2, // ret
            0xaa => {
                // tableswitch — pad to 4-byte boundary then 12 bytes
                // header + (high - low + 1) * 4 jumps.
                let pad = (4 - ((pc + 1) % 4)) % 4;
                let table = pc + 1 + pad;
                if table + 12 > bytecode.len() {
                    return InitComplexity::Complex;
                }
                let low = i32::from_be_bytes([
                    bytecode[table + 4], bytecode[table + 5],
                    bytecode[table + 6], bytecode[table + 7],
                ]);
                let high = i32::from_be_bytes([
                    bytecode[table + 8], bytecode[table + 9],
                    bytecode[table + 10], bytecode[table + 11],
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
                    bytecode[table + 4], bytecode[table + 5],
                    bytecode[table + 6], bytecode[table + 7],
                ]) as usize;
                1 + pad + 8 + npairs * 8
            }
            0xac..=0xb1 => 1, // ireturn..return
            0xb2..=0xb6 => 3, // getstatic, putstatic, getfield, putfield, invokevirtual
            0xb7 | 0xb8 => 3, // invokespecial, invokestatic
            0xb9 => 5, // invokeinterface
            // 0xba (invokedynamic) handled above
            0xbb => 3, // new
            0xbc => 2, // newarray
            0xbd => 3, // anewarray
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
            0xc5 => 4, // multianewarray
            0xc6 | 0xc7 => 3, // ifnull, ifnonnull
            0xc8 | 0xc9 => 5, // goto_w, jsr_w
            _ => 1,
        };
        pc += min(len, bytecode.len() - pc);
        if len == 0 { return InitComplexity::Complex; }
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
/// `RUSTJVM_JIT_ALLOW_PACKAGES` environment variable (comma-separated package
/// prefixes such as `java/util,rustjvm/`). Any conservative-only blanket ban
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

    // T1.1.g — the historical blanket bans for `java/util/*` and
    // `rustjvm/*` have been narrowed to targeted per-method
    // exclusions covering only the specific reproducible miscompiles
    // that NEW-1.3/1.4 have not yet closed. Every other method in
    // those packages is now JIT-eligible under both policies, which
    // closes the T1.1.g "delete blanket bans" roadmap item while
    // preserving correctness for the known-failing cases.
    //
    // The conservative policy still applies the targeted list; the
    // aggressive policy (set via `jit_aggressive_compilation` or
    // `RUSTJVM_JIT_ALLOW_PACKAGES`) lifts even the targeted list so
    // developers can surface new miscompiles.
    if policy == SkipPolicy::Conservative {
        if is_known_miscompile(class_name, method_name)
            && !package_allowed("java/util/", allow_packages)
            && !package_allowed("rustjvm/", allow_packages)
            && !package_allowed("java/lang/", allow_packages)
            && !package_allowed("java/security/", allow_packages)
        {
            return Some(if class_name.starts_with("java/util/") {
                SkipReason::JavaUtilCollection
            } else {
                SkipReason::RustJvmTestFixture
            });
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
        // Lifted by `RUSTJVM_JIT_ALLOW_PACKAGES=org/bouncycastle/`.
        // Track for a real fix once the underlying allocate-then-putfield
        // miscompile is root-caused (see `is_known_miscompile` doc).
        if class_name.starts_with("org/bouncycastle/")
            && !package_allowed("org/bouncycastle/", allow_packages)
        {
            return Some(SkipReason::RustJvmTestFixture);
        }

        // SPB.1 (Session 112) — provisional blanket ban for the Spring
        // Framework `org/springframework/util/` package. `ClassUtils.
        // <clinit>` runs `registerCommonClasses(...)` ~10 times for
        // primitive / wrapper / collection / common-types groups, putting
        // ~100 entries into a fresh HashMap. With JIT enabled the run
        // segfaults right after the log4j-api StatusLogger warning; with
        // `RUSTJVM_DISABLE_JIT=1` the segfault disappears (a different
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
        // `RUSTJVM_JIT_ALLOW_PACKAGES=org/springframework/util/`. Track
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
        // `RUSTJVM_DISABLE_JIT=1` boot proceeds past the lambda (and a
        // different downstream gap surfaces in log4j PropertiesUtil
        // <clinit>). The same allocate-then-putfield-vs-OSR pattern that
        // bites Integer.valueOf / String.toLowerCase applies here:
        // ConcurrentReferenceHashMap.Reference / Node allocation paths
        // store fields immediately after `new`. Lifted by
        // `RUSTJVM_JIT_ALLOW_PACKAGES=org/springframework/core/`.
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
        // `RUSTJVM_FRAME_TRACE=1` capture shows the very last frames
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
        // `RUSTJVM_DISABLE_JIT=1` the SIGSEGV is replaced by a clean
        // `NullPointerException` in `PathMatchingResourcePatternResolver.
        // <clinit>` (a different downstream gap, not a JIT issue).
        //
        // Lifted by `RUSTJVM_JIT_ALLOW_PACKAGES=org/springframework/boot/
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
        // by `RUSTJVM_JIT_ALLOW_PACKAGES=org/springframework/boot/context/`.
        if class_name.starts_with("org/springframework/boot/context/")
            && !package_allowed(
                "org/springframework/boot/context/",
                allow_packages,
            )
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
        // `RUSTJVM_JIT_ALLOW_PACKAGES=org/springframework/boot/`. (This
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
        // Lifted by `RUSTJVM_JIT_ALLOW_PACKAGES=org/springframework/cloud/`.
        if class_name.starts_with("org/springframework/cloud/")
            && !package_allowed("org/springframework/cloud/", allow_packages)
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
        // `RUSTJVM_JIT_ALLOW_PACKAGES=com/netflix/discovery/`.
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
        // Lifted by `RUSTJVM_JIT_ALLOW_PACKAGES=feign/`.
        if class_name.starts_with("feign/")
            && !package_allowed("feign/", allow_packages)
        {
            return Some(SkipReason::RustJvmTestFixture);
        }

        // SPB.8 (Session 113 r2) — provisional blanket ban for the JBoss
        // Modules class-graph and resource loading code paths.
        // `apps/wildfly-39.0.1.Final` boot SIGSEGVs (rc=139) right after the
        // BigInteger ZERO/ONE/TWO post-clinit fixup and the two upstream
        // `<clinit>` swallows (`SimpleLoggerContext`, `ConcurrentClassLoader`)
        // handled by parallel agents. With `RUSTJVM_DISABLE_JIT=1` the
        // SIGSEGV is replaced by a clean `NoSuchMethodError` for
        // `Object.loadClass(...)` followed by an orderly `System.exit(1)`
        // — i.e. boot proceeds far past the JIT-on crash point. This
        // confirms a JIT miscompile, not a native gap.
        //
        // The `RUSTJVM_DBG_JIT_DISPATCH=1` capture shows the very last
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
        // Lifted by `RUSTJVM_JIT_ALLOW_PACKAGES=org/jboss/modules/`. The
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
        // `RUSTJVM_JIT_ALLOW_PACKAGES=org/jboss/as/`.
        if class_name.starts_with("org/jboss/as/")
            && !package_allowed("org/jboss/as/", allow_packages)
        {
            return Some(SkipReason::RustJvmTestFixture);
        }

        // SPB.8c (Session 113 r2) — companion blanket ban for the WildFly
        // security-manager package (`org/wildfly/`). The
        // `RUSTJVM_DBG_JIT_DISPATCH=1` capture shows the very last JIT
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
        // Lifted by `RUSTJVM_JIT_ALLOW_PACKAGES=org/wildfly/`.
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
        // depth 18). With `RUSTJVM_DISABLE_JIT=1` the same int(1) crash
        // surfaces — but the very last methods JIT-dispatched before the
        // failure (`RUSTJVM_DBG_JIT_DISPATCH=1` capture) are an extremely
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
        // `RUSTJVM_JIT_ALLOW_PACKAGES=org/slf4j/,ch/qos/logback/,
        // org/apache/commons/logging/`.
        if class_name.starts_with("org/slf4j/")
            && !package_allowed("org/slf4j/", allow_packages)
        {
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
        // `RUSTJVM_DISABLE_JIT=1` the SEGFAULT disappears and the run
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
        // SEGV. Lifted by `RUSTJVM_JIT_ALLOW_PACKAGES=net/sf/cglib/`.
        // Track for a real fix once the underlying allocate-then-putfield
        // miscompile is root-caused.
        if class_name.starts_with("net/sf/cglib/")
            && !package_allowed("net/sf/cglib/", allow_packages)
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
        // `RUSTJVM_JIT_ALLOW_PACKAGES=org/springframework/boot/loader/,
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
        // `RUSTJVM_JIT_ALLOW_PACKAGES=org/springframework/context/annotation/,
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
        // comparators. Frame trace + RUSTJVM_DBG_JIT_DISPATCH=1 show
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
        // `RUSTJVM_JIT_ALLOW_PACKAGES=com/sun/beans/,java/beans/`
        // when the underlying allocate-then-putfield miscompile is
        // root-caused.
        if class_name.starts_with("com/sun/beans/")
            && !package_allowed("com/sun/beans/", allow_packages)
        {
            return Some(SkipReason::RustJvmTestFixture);
        }
        if class_name.starts_with("java/beans/")
            && !package_allowed("java/beans/", allow_packages)
        {
            return Some(SkipReason::RustJvmTestFixture);
        }
        if class_name.starts_with("org/springframework/beans/factory/")
            && !class_name.starts_with("org/springframework/beans/factory/support/")
            && !package_allowed("org/springframework/beans/factory/", allow_packages)
        {
            return Some(SkipReason::RustJvmTestFixture);
        }
    }

    None
}

/// T1.1.g — targeted list of (class, method) pairs known to miscompile
/// under the current JIT. Every other method — including the vast
/// majority of `java/util/*` and `rustjvm/*` methods — is now
/// JIT-eligible. Each entry here corresponds to a tracked NEW-1.3 or
/// NEW-1.4 follow-up.
fn is_known_miscompile(class_name: &str, method_name: &str) -> bool {
    matches!(
        (class_name, method_name),
        // NEW-1.3 — hash-table hot loop miscompile, surfaces under
        // HashMap.put/get/resize. These three are the observed
        // failing methods from the `RUSTJVM_JIT_ALLOW_PACKAGES=java/util`
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
        // StatusLogger "no log4j-core" warning; with `RUSTJVM_DISABLE_JIT=1`
        // the segfault disappears (and a different downstream gap surfaces
        // in PropertiesUtil.<clinit>). The `RUSTJVM_FRAME_TRACE=1` capture
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
        // NEW-1.4 — regalloc parameter-mapping bug, surfaces as
        // `test_s46_exc_hierarchy` returning Int(0) instead of Int(1).
        // Tracked by the committed reproducer in
        // `vm/tests/tier1_tests.rs::t1_known_regalloc_miscompile_exc_hierarchy_reproducer`.
        | ("rustjvm/TckLang", "exc_hierarchy")
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
        | ("java/lang/Integer", "valueOf")
        | ("java/lang/Integer", "<init>")
        | ("java/lang/Long", "valueOf")
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
        // same archetype (String, int) -> int. Other parsing helpers
        // (Integer.valueOf, Long.valueOf already banned above) cover the
        // (String) -> Number boxing path.
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
        // after `test1=42`; with `RUSTJVM_DISABLE_JIT=1` the entire test
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
        // AbstractQueuedSynchronizer hot dispatch + node alloc paths.
        // AQS allocates an `ExclusiveNode` / `ConditionNode` for every
        // contended acquire/release; that allocate-then-putfield in the
        // node ctor is the same miscompile signature. The do*/signalNext
        // inner helpers walk the waiter list and re-link nodes via
        // putfield-on-fresh-allocation; without skipping them, the
        // signaling path corrupts the next-pointer and waiters are
        // never woken (latch.await stays parked indefinitely).
        | ("java/util/concurrent/locks/AbstractQueuedSynchronizer", "acquire")
        | ("java/util/concurrent/locks/AbstractQueuedSynchronizer", "release")
        | ("java/util/concurrent/locks/AbstractQueuedSynchronizer", "acquireShared")
        | ("java/util/concurrent/locks/AbstractQueuedSynchronizer", "releaseShared")
        | ("java/util/concurrent/locks/AbstractQueuedSynchronizer", "signalNext")
        | ("java/util/concurrent/locks/AbstractQueuedSynchronizer", "signalNextIfShared")
        | ("java/util/concurrent/locks/AbstractQueuedSynchronizer$ConditionObject", "signal")
        | ("java/util/concurrent/locks/AbstractQueuedSynchronizer$ConditionObject", "signalAll")
        | ("java/util/concurrent/locks/AbstractQueuedSynchronizer$ConditionObject", "doSignal")
        | ("java/util/concurrent/locks/AbstractQueuedSynchronizer$ConditionObject", "await")
        | ("java/util/concurrent/locks/AbstractQueuedSynchronizer$ConditionObject", "newConditionNode")
        | ("java/util/concurrent/locks/AbstractQueuedSynchronizer$ConditionObject", "enableWait")
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
        // SPB.3 (Session 111 r14) — `apps/SportMe-master`'s Spring Boot
        // bootstrap segfaults (rc=139) deep in Spring's
        // `ConfigurationPropertySources` cache-key build path. Per r13
        // SportMe agent's `RUSTJVM_FRAME_TRACE=1` capture, the very last
        // frames before the crash are
        // `MapPropertySource.getPropertyNames` -> `StringUtils.
        // toStringArray(Collection)` -> `HashMap.keysToArray(Object[])`
        // and `SpringIterableConfigurationPropertySource$CacheKey.<init>`
        // / `HashSet.<init>(Collection)` -> `HashMap$KeySet.iterator()`
        // -> `HashMap$KeyIterator.<init>` -> `HashMap$HashIterator.<init>`
        // -> `HashMap$HashIterator.hasNext()`. With `RUSTJVM_DISABLE_JIT=1`
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
        // SPB.9d (Session 117) — eureka-server JIT-mode SEGFAULTs deep
        // inside `org/springframework/core/annotation/*` annotation
        // processing during BeanInfo introspection. Spring's
        // `ExtendedBeanInfo` introspects bean properties for every bean
        // class via `java/beans/Introspector`, which sorts methods using
        // `com/sun/beans/introspect/MethodInfo$MethodOrder.compare` and
        // sorts properties using
        // `org/springframework/beans/ExtendedBeanInfo$PropertyDescriptorComparator.compare`.
        // RUSTJVM_DBG_JIT_DISPATCH=1 (run 117a) shows the hot JIT-callees
        // before the segfault are: `MethodInfo$MethodOrder.compare` (149x),
        // `PropertyDescriptorComparator.compare` (17x),
        // `Method.getName()` (360x), `String.compareTo(String)` (167x),
        // and `StringJoiner.<init>(LCS;LCS;LCS;)V` (24x), all called
        // from a JIT-compiled sort comparator chain. With
        // `RUSTJVM_DISABLE_JIT=1` the run terminates earlier with the
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
        // `RUSTJVM_JIT_ALLOW_PACKAGES=java/util/,com/sun/beans/`.
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
        // `RUSTJVM_FRAME_TRACE=1` capture shows the most-called methods
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
        // SPB.4 cont. (Session 113 r1) — `RUSTJVM_DBG_JIT_COMPILE` shows
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
        // `RUSTJVM_DISABLE_JIT=1` the bootstrap advances ~16 lines further
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
        // bin/felix.jar` with `RUSTJVM_FELIX_REAL=1` SEGVs (rc=139) on
        // Windows during the OSGi `FrameworkFactory` bootstrap. The
        // `RUSTJVM_DBG_JIT_ENTRY=1` trace shows the last JIT entry before
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
        // `setAccessible0` callees. With `RUSTJVM_DISABLE_JIT=1` the rc
        // changes from 139 to 0 and a clean
        // `java.lang.NullPointerException: Cannot invoke length on null`
        // surfaces at `Main.java:287` (config-properties path; an unrelated
        // Felix data gap). Skip-list the bulk overload plus the
        // SecureAction lambda that drives it; the per-instance
        // `setAccessible(boolean)` overloads stay JIT-eligible. Liftable
        // via `RUSTJVM_JIT_ALLOW_PACKAGES=java/lang/reflect/`.
        | ("java/lang/reflect/AccessibleObject", "setAccessible")
        | (
            "org/apache/felix/framework/util/SecureAction",
            "lambda$getAccessor$0",
        )
    )
}

/// True if `prefix` matches any entry in `allow_packages`. An entry matches if
/// `prefix` starts with the entry, so `RUSTJVM_JIT_ALLOW_PACKAGES=java/util`
/// lifts the `java/util/` ban.
fn package_allowed(prefix: &str, allow_packages: &[&str]) -> bool {
    allow_packages
        .iter()
        .any(|entry| !entry.is_empty() && prefix.starts_with(entry))
}

/// Parse the `RUSTJVM_JIT_ALLOW_PACKAGES` env var into a list of allowed
/// package prefixes. The result is cached at first call so repeated
/// `should_skip_jit` invocations do not re-parse.
///
/// The leading-edge use case is benchmarking: a developer running
/// `RUSTJVM_JIT_ALLOW_PACKAGES=java/util cargo test` lifts the `java/util/`
/// ban for that one run without editing source.
pub fn allow_packages_from_env() -> &'static [&'static str] {
    use std::sync::OnceLock;
    static CACHE: OnceLock<Vec<&'static str>> = OnceLock::new();
    CACHE
        .get_or_init(|| {
            std::env::var("RUSTJVM_JIT_ALLOW_PACKAGES")
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
    fn java_util_targeted_methods_only_skipped_under_conservative() {
        // T1.1.g — HashMap.put is on the targeted list (NEW-1.3).
        assert_eq!(
            check("java/util/HashMap", "put", false, true, SkipPolicy::Conservative),
            Some(SkipReason::JavaUtilCollection)
        );
        // Aggressive still lifts it.
        assert_eq!(
            check("java/util/HashMap", "put", false, true, SkipPolicy::Aggressive),
            None
        );
    }

    #[test]
    fn java_util_unrelated_methods_now_jit_eligible_under_conservative() {
        // T1.1.g — the blanket ban is gone. Unrelated java/util
        // methods are now JIT-eligible even under Conservative.
        assert_eq!(
            check("java/util/HashMap", "size", false, true, SkipPolicy::Conservative),
            None,
            "non-targeted HashMap methods must be JIT-eligible after T1.1.g"
        );
        assert_eq!(
            check("java/util/ArrayList", "add", false, true, SkipPolicy::Conservative),
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
    fn rustjvm_targeted_miscompile_only_skipped() {
        // T1.1.g — exc_hierarchy is the NEW-1.4 reproducer.
        assert_eq!(
            check(
                "rustjvm/TckLang",
                "exc_hierarchy",
                false,
                true,
                SkipPolicy::Conservative
            ),
            Some(SkipReason::RustJvmTestFixture)
        );
    }

    #[test]
    fn rustjvm_unrelated_methods_now_jit_eligible_under_conservative() {
        // T1.1.g — the blanket ban on `rustjvm/*` is gone.
        assert_eq!(
            check("rustjvm/Other", "m", false, true, SkipPolicy::Conservative),
            None,
            "unrelated rustjvm/* methods must be JIT-eligible after T1.1.g"
        );
        assert_eq!(
            check("rustjvm/TckLang", "str_length", false, true, SkipPolicy::Conservative),
            None,
            "non-miscompile TckLang methods must be JIT-eligible"
        );
    }

    #[test]
    fn rustjvm_targeted_overridable_via_allow_packages() {
        assert_eq!(
            check_with(
                "rustjvm/TckLang",
                "exc_hierarchy",
                false,
                true,
                SkipPolicy::Conservative,
                &["rustjvm/"],
            ),
            None
        );
    }

    #[test]
    fn user_class_never_skipped() {
        assert_eq!(
            check("com/example/App", "compute", false, true, SkipPolicy::Conservative),
            None
        );
        assert_eq!(
            check("com/example/App", "compute", false, true, SkipPolicy::Aggressive),
            None
        );
    }

    // ------------------------------------------------------------------
    // NEW-1 CI gate tests — these enforce that REMOVED bans stay removed.
    // ------------------------------------------------------------------

    /// NEW-1.2 CI gate: java/lang/* is no longer blanket-banned.
    /// (W2-CHM narrowed `Integer.valueOf` / `<init>` and `Long.valueOf` /
    /// `<init>` to targeted bans — see `is_known_miscompile` — so unrelated
    /// `java/lang/*` methods like `String.indexOf` still demonstrate the
    /// "no blanket ban" guarantee.)
    #[test]
    fn java_lang_is_jit_eligible_after_new_1_2() {
        assert_eq!(
            check("java/lang/String", "indexOf", false, true, SkipPolicy::Conservative),
            None,
            "java/lang/* must be JIT-eligible after NEW-1.2 (instanceof fix)"
        );
        assert_eq!(
            check("java/lang/Integer", "toString", false, true, SkipPolicy::Conservative),
            None,
            "Integer.toString stays JIT-eligible — only valueOf/<init> are skip-listed"
        );
    }

    /// W2-CHM CI gate: `Integer.valueOf` and `Integer.<init>` are skipped
    /// under the conservative policy because the JIT miscompiles them
    /// (Integer's value field reads back as 0 when allocated via the
    /// allocate-then-putfield JIT sequence after the OSR + per-callee
    /// invocation thresholds are crossed in the same outer frame).
    /// `Long.valueOf` / `Long.<init>` are skipped by the same logic.
    #[test]
    fn integer_long_box_methods_skipped_under_conservative_policy() {
        for (cls, mn) in [
            ("java/lang/Integer", "valueOf"),
            ("java/lang/Integer", "<init>"),
            ("java/lang/Long", "valueOf"),
            ("java/lang/Long", "<init>"),
        ] {
            assert!(
                check(cls, mn, false, true, SkipPolicy::Conservative).is_some(),
                "{cls}.{mn} must be skipped under Conservative policy (W2-CHM)"
            );
        }
    }

    /// NEW-1.2 CI gate: rustjvm/Tck* is no longer blanket-banned.
    #[test]
    fn tck_class_is_jit_eligible_after_new_1_2() {
        assert_eq!(
            check("rustjvm/TckLang", "test1", false, true, SkipPolicy::Aggressive),
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
            check("com/example/MyFinalizerTest", "f", false, true, SkipPolicy::Aggressive),
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
    // `starts_with("java/util/")` or `starts_with("rustjvm/")` ban
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
    fn tier1_skip_list_no_blanket_rustjvm_ban() {
        // Representative rustjvm/* methods NOT in the targeted list.
        let methods = [
            ("rustjvm/TckLang", "str_length"),
            ("rustjvm/TckLang", "obj_hashCode_consistent"),
            ("rustjvm/TckIo", "readLine"),
            ("rustjvm/Other", "anything"),
        ];
        for (cls, meth) in &methods {
            assert_eq!(
                check(cls, meth, false, true, SkipPolicy::Conservative),
                None,
                "T1.1.38 GATE: {cls}.{meth} must be JIT-eligible — \
                 a blanket rustjvm/* ban was reintroduced"
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
        assert!(is_known_miscompile("rustjvm/TckLang", "exc_hierarchy"));
        // Everything else must NOT be in the list.
        assert!(!is_known_miscompile("java/util/HashMap", "size"));
        assert!(!is_known_miscompile("java/util/ArrayList", "add"));
        assert!(!is_known_miscompile("rustjvm/TckLang", "str_length"));
        assert!(!is_known_miscompile("com/example/Foo", "bar"));
    }
}
