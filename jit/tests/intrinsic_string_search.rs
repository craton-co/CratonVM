//! STRING_SEARCH intrinsic family — BAIL verification.
//!
//! This family covers four `java/lang/String` instance methods:
//!   * `equals(Ljava/lang/Object;)Z`
//!   * `compareTo(Ljava/lang/String;)I`
//!   * `indexOf(I)I`            (char argument)
//!   * `indexOf(Ljava/lang/String;)I`
//!
//! The layout investigation (documented in full inside the `STRING_SEARCH`
//! regions of `jit/src/lib.rs` and `jit/src/x64.rs`) concluded that NONE of
//! these can be JIT-inlined while remaining bit-identical to JDK semantics:
//!
//!   1. String's field layout is NOT statically fixed. `native-builtins/
//!      src/lang_string.rs` shows the VM may load either the JDK 9+ compact
//!      layout `{value:[B, coder:B, hash:I, hashIsZero:Z}` or a legacy
//!      synthetic layout `{value:[C, hash:I}`. Which one applies is decided
//!      at RUNTIME from the `value` array's element type (`byte[]` vs
//!      `char[]`) — a singly-compiled JIT method cannot know it.
//!
//!   2. The `coder` field index is not even constant: index 1 is `coder`
//!      in the compact layout but `hash` in the legacy layout.
//!
//!   3. The JIT has no inline instance-field or array-element access; every
//!      `getfield` / `baload` is a runtime helper CALL, so an "inlined"
//!      String op would still emit CALLs (violating roadmap §8) and would
//!      risk reading `hash` as `coder`.
//!
//! Per roadmap §3.4 ("correctness over coverage") the family registers
//! NOTHING — `try_resolve_intrinsic` must return `None` for every targeted
//! signature, so each call falls back to the correct native-builtins
//! implementation. This test pins that contract: if a future change ever
//! registers one of these signatures, it must also land a provably-correct
//! inline codegen path, and this test will flag the regression.

use cratonvm_jit::try_resolve_intrinsic;

/// Every targeted `String` search/compare signature must resolve to `None`
/// (the family is intentionally bailed — see the module doc above).
#[test]
fn string_search_signatures_are_not_registered() {
    let bailed: &[(&str, &str, &str)] = &[
        ("java/lang/String", "equals", "(Ljava/lang/Object;)Z"),
        ("java/lang/String", "compareTo", "(Ljava/lang/String;)I"),
        ("java/lang/String", "indexOf", "(I)I"),
        ("java/lang/String", "indexOf", "(Ljava/lang/String;)I"),
    ];

    for &(class, name, descriptor) in bailed {
        assert!(
            try_resolve_intrinsic(class, name, descriptor).is_none(),
            "STRING_SEARCH must NOT register {class}.{name}{descriptor}: \
             String field layout is runtime-determined, so inlining cannot \
             be proven bit-identical to JDK semantics (roadmap §3.4). \
             If you intend to inline this, you must first land a fixed \
             String-layout contract and provably-correct codegen.",
        );
    }
}

/// Guard against accidental registration under the related receiver types
/// `StringLatin1` / `StringUTF16` (the JDK's internal compact-string helper
/// classes). The STRING_SEARCH family deliberately registers none of them.
#[test]
fn string_search_compact_helper_classes_are_not_registered() {
    let probes: &[(&str, &str, &str)] = &[
        ("java/lang/StringLatin1", "equals", "([B[B)Z"),
        ("java/lang/StringLatin1", "compareTo", "([B[B)I"),
        ("java/lang/StringLatin1", "indexOf", "([BI[BII)I"),
        ("java/lang/StringUTF16", "equals", "([B[B)Z"),
        ("java/lang/StringUTF16", "compareTo", "([B[B)I"),
        ("java/lang/StringUTF16", "indexOf", "([BI[BII)I"),
    ];

    for &(class, name, descriptor) in probes {
        assert!(
            try_resolve_intrinsic(class, name, descriptor).is_none(),
            "STRING_SEARCH must NOT register {class}.{name}{descriptor}",
        );
    }
}

/// Sanity check that the bail is *scoped*: the matcher still works for an
/// unrelated, genuinely-registered intrinsic. This ensures the STRING_SEARCH
/// region did not accidentally short-circuit `try_resolve_intrinsic` (e.g.
/// by an early `return None`) for every input.
#[test]
fn string_search_bail_does_not_disturb_other_families() {
    // `Math.sqrt(D)D` is registered unconditionally (no CPU-feature gate)
    // by the Math family — see `try_resolve_intrinsic` in `jit/src/lib.rs`.
    assert!(
        try_resolve_intrinsic("java/lang/Math", "sqrt", "(D)D").is_some(),
        "STRING_SEARCH bail must not break unrelated intrinsic resolution",
    );
}
