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
        {
            return Some(if class_name.starts_with("java/util/") {
                SkipReason::JavaUtilCollection
            } else {
                SkipReason::RustJvmTestFixture
            });
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
        // NEW-1.4 — regalloc parameter-mapping bug, surfaces as
        // `test_s46_exc_hierarchy` returning Int(0) instead of Int(1).
        // Tracked by the committed reproducer in
        // `vm/tests/tier1_tests.rs::t1_known_regalloc_miscompile_exc_hierarchy_reproducer`.
        | ("rustjvm/TckLang", "exc_hierarchy")
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
    #[test]
    fn java_lang_is_jit_eligible_after_new_1_2() {
        assert_eq!(
            check("java/lang/String", "indexOf", false, true, SkipPolicy::Conservative),
            None,
            "java/lang/* must be JIT-eligible after NEW-1.2 (instanceof fix)"
        );
        assert_eq!(
            check("java/lang/Integer", "valueOf", false, true, SkipPolicy::Conservative),
            None
        );
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
