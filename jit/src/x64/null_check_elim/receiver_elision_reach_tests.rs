/// The two `getfield` arms must consult the dataflow, and the other four
/// receiver-guard sites must NOT.
///
/// This is a source scan because the property is about which CALL each arm
/// makes, and the arms are unreachable from a unit test without a full
/// compile fixture. It is worth the brittleness: the edit that breaks it —
/// "unify these six sites on one helper" — looks like tidying and is a
/// miscompile at four of them.
///
/// * The two `getfield` arms are identifiable by their return shape,
///   `(…, None)`: they hand back a patch list and a separate null patch.
///   Their receiver is the top-of-stack value the preceding `aload`
///   pushed, which is what `preceding_aload_nonnull_local` decodes.
/// * The two `putfield` arms must stay on the bare check. `putfield`'s
///   stack is `[…, objectref, value]`, so the preceding push is the stored
///   VALUE — attributing the receiver's nullness to it is the shape of the
///   Tomcat `MessageBytes.setString` miscompile that
///   `opcode_dereferences_receiver` documents.
/// * The two `checkcast` arms must stay on the bare check for a different
///   reason: `checkcast` does not throw on a null receiver at all (a null
///   casts to anything), so its `JZ` targets a legal null path rather than
///   an NPE. Eliding it would let a null fall into the `KIND_TAGS` byte
///   compare and fault.
#[test]
fn only_the_getfield_arms_consult_the_null_check_dataflow() {
    let src = crate::x64::bytecode_walk::WALK_SOURCES;
    // Counted on the call NAME, not on an argument spelling: arm 2's call is
    // wrapped across lines, and an earlier version of this test matched
    // "(code, pc," and silently scored it as one site instead of two.
    let consulting = src.matches("emit_trusted_oop_receiver_check_at(").count();
    // Every arm that can elide the check MUST bind a recovery address,
    // and the two numbers are checked against each other rather than
    // against a constant. An arm that opts in without binding leaves the
    // site pending; that is caught at runtime by
    // `has_unbound_implicit_null_sites`, but as a refused compile on a
    // live workload rather than here.
    //
    // Note the pairing is what is asserted, not the literal `true`. Arm 1
    // opts in unconditionally; arm 2 opts in exactly when
    // `compact_ref_fields_enabled()`, because that is when it emits the
    // `GC_FLAGS` read the fault lands on. A future arm may well pass a
    // third expression -- what it may not do is pass one without binding.
    let binds = src.matches("self.bind_implicit_null_recovery()").count();
    assert_eq!(
        binds, consulting,
        "{consulting} getfield arms consult the dataflow but {binds} bind a \
         recovery address. Each arm that may elide the receiver check has \
         to bind the slow path the fault recovers into, at the point that \
         slow path begins."
    );
    assert_eq!(
        consulting, 2,
        "expected exactly the two `getfield` arms to consult the dataflow; \
         found {consulting}. A THIRD consulting site is only correct if its \
         receiver is the value the immediately-preceding `aload` pushed — \
         see this test's doc comment for the two shapes where it is not."
    );
    let bare = src
        .matches("self.emit_trusted_oop_receiver_check()")
        .count();
    // 2026-09-12: 4 -> 5. The `instanceof` arm now takes checkcast's
    // inline class-id guard, and its null test must stay bare for the
    // checkcast reason: null is a legal `instanceof` operand (answer 0),
    // so its `JZ` targets a result stub, not an NPE, and an elided test
    // would let null fall into the `KIND_TAGS` byte compare and fault.
    // 2026-09-18: 5 -> 6. Round 9's inline primitive `putfield`
    // (`CRATONVM_JIT_INLINE_PRIM_PUTFIELD`, `op_field.rs`) keeps the bare
    // check: it has no dataflow proof wired, and for a store an elided
    // receiver test costs a wild write, not a wrong read.
    assert_eq!(
        bare, 6,
        "expected the three `putfield` (two helper-routed, one inline \
         primitive), two `checkcast` and one `instanceof` arms to keep the \
         unconditional check; found {bare}. Moving one of them onto the \
         `_at` form is a miscompile, not a simplification."
    );
}
