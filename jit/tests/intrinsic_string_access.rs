//! STRING_ACCESS intrinsic family — bail verification.
//!
//! Family: `java/lang/String` instance methods `length()I`, `charAt(I)C`,
//! `isEmpty()Z`, `hashCode()I`.
//!
//! OUTCOME: every one of these is INTENTIONALLY NOT inlined. The JIT
//! intrinsic matcher (`cratonvm_jit::try_resolve_intrinsic`) returns `None`
//! for all of them, so each call falls back to the normal native-dispatch
//! path (the implementations in `native-builtins/src/lang_string.rs`, which
//! are the differential oracle).
//!
//! WHY BAIL — inlining these would be unsound in this VM:
//!
//!   1. No static class-layout registry. The JIT resolves `getfield` field
//!      offsets only from the *currently-compiled method's* constant pool,
//!      keyed per bytecode PC (`field_info` in `x64.rs`). There is no
//!      `String -> {value@N, coder@M, hash@K}` offset table that either the
//!      matcher or the codegen could consult. `try_resolve_intrinsic` is
//!      handed only `(class, name, descriptor)` strings — nothing about the
//!      target class's field layout.
//!
//!   2. String's runtime layout is genuinely variable. The native code in
//!      `native-builtins/src/lang_string.rs` (`native_string_hash_code`,
//!      ~lines 590-604) documents two distinct String layouts the VM
//!      actually loads at runtime:
//!        * JDK-25 compact:  { value: [B, coder: B, hash: I, hashIsZero: Z }
//!        * legacy synthetic:{ value: [C, hash: I }
//!      The cached `hash` field is at index 2 in the first layout and index
//!      1 in the second. The native code picks the slot at runtime by
//!      inspecting `heap_element_type_of(value)`. Machine code emitted with
//!      a hard-coded field offset would read/clobber the wrong slot (e.g.
//!      corrupt `coder`) on whichever layout it was not compiled for.
//!
//!   3. `length()` and `charAt(I)C` need runtime information even given the
//!      field offsets:
//!        * `length()` is `value.length` for a legacy `char[]` value but
//!          `value.length >> coder` for a compact `byte[]` value — and
//!          whether `value` is `byte[]` or `char[]`, plus the `coder` byte
//!          value, are properties of the heap object, not statically known.
//!        * `charAt(i)` decodes a LATIN1 byte (zero-extend) or a UTF-16 LE
//!          byte pair depending on `coder`; same runtime dependency.
//!
//!   4. Object fields are stored as the 16-byte Rust enum `Value` (tag +
//!      payload, implementation-defined repr) — not raw scalars. Even with a
//!      known offset, a field cannot be loaded with a plain `MOV`; the
//!      `getfield` path goes through a helper performing `ptr::read::<Value>`
//!      and a tag match. `x64.rs` (the `putfield` 0xb5 arm) explicitly calls
//!      this out as a known inlining gap.
//!
//! Per roadmap §3.4 ("Never trade correctness for inlining"), the matcher
//! registers nothing for this family and these calls dispatch normally.
//!
//! WHAT A FIXED-LAYOUT CONTRACT WOULD NEED (so a future agent can revisit):
//!   - A single canonical, VM-wide `java/lang/String` layout (drop the
//!     legacy synthetic `{value:[C], hash:I}` layout entirely), with
//!     `value`, `coder`, `hash` at fixed, documented field indices.
//!   - A static layout descriptor the JIT can query by class name at
//!     compile time (a `String -> {value_idx, coder_idx, hash_idx}` map, or
//!     a hard pin in `jit/src`), pinned to `lang_string.rs`.
//!   - A compact, plain-scalar field storage for the `coder`/`hash` int
//!     fields (or at least a stable `#[repr]` on `Value`) so the JIT can
//!     emit a direct load instead of the `Value`-enum helper call.
//!   - The `value` array's element type (`byte[]` vs `char[]`) likewise
//!     pinned, so `length`/`charAt` decoding can be selected at compile
//!     time rather than via a runtime `heap_element_type_of` check.
//! Until all of that holds, `length`/`charAt`/`isEmpty`/`hashCode` must
//! stay on the native-dispatch path.

use cratonvm_jit::try_resolve_intrinsic;

/// The four targeted `java/lang/String` instance methods. The matcher must
/// return `None` for every one — they are not registered as intrinsics.
const STRING_ACCESS_SIGNATURES: &[(&str, &str)] = &[
    ("length", "()I"),
    ("charAt", "(I)C"),
    ("isEmpty", "()Z"),
    ("hashCode", "()I"),
];

#[test]
fn string_access_methods_are_not_registered_as_intrinsics() {
    for &(name, descriptor) in STRING_ACCESS_SIGNATURES {
        let hit = try_resolve_intrinsic("java/lang/String", name, descriptor);
        assert!(
            hit.is_none(),
            "java/lang/String.{name}{descriptor} must NOT resolve to a JIT \
             intrinsic — String's field layout is runtime-determined, so \
             inlining would be unsound. Matcher returned {hit:?}. See the \
             module-level doc comment for the full bail rationale.",
        );
    }
}

/// Defence in depth: the same methods on `java/lang/StringLatin1` (the JDK
/// helper class the compact-string fast paths delegate to) must also not be
/// intrinsified — it has the same unknowable-layout problem.
#[test]
fn string_latin1_access_methods_are_not_registered_as_intrinsics() {
    for &(name, descriptor) in STRING_ACCESS_SIGNATURES {
        let hit = try_resolve_intrinsic("java/lang/StringLatin1", name, descriptor);
        assert!(
            hit.is_none(),
            "java/lang/StringLatin1.{name}{descriptor} must NOT resolve to a \
             JIT intrinsic. Matcher returned {hit:?}.",
        );
    }
}

/// Sanity check that the matcher is wired up and *can* return `Some` — this
/// guards against a false-negative where the bail assertions above pass only
/// because `try_resolve_intrinsic` is trivially broken. `Math.sqrt` is a
/// long-standing registered intrinsic (Phase 0 baseline).
#[test]
fn matcher_is_live_math_sqrt_still_resolves() {
    let hit = try_resolve_intrinsic("java/lang/Math", "sqrt", "(D)D");
    assert!(
        hit.is_some(),
        "sanity: Math.sqrt(D)D should still resolve as an intrinsic; if this \
         fails the STRING_ACCESS bail assertions are vacuous",
    );
}
