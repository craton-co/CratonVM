use super::zgc_relocation_permitted;

/// **Relocation is permitted only where a reference load is barriered.**
///
/// The original rule was "refused whenever the JIT is enabled", because
/// JIT-compiled code baked raw 8-byte reference loads at compile-time
/// offsets and would read a coloured word as a pointer -- a use-after-free
/// with no error path. That is still the hazard; what changed on
/// 2026-08-13 is that stage (a) of `zgc-jit-load-barrier.md` landed, so
/// the JIT no longer does that: `x64::zgc_read_barrier_blocks_inline_fields`
/// routes every compact-field access through `jit_getfield` /
/// `jit_putfield_object` whenever the barrier is armed, and those go
/// through the heap accessors, which barrier.
///
/// So the predicate is now a disjunction, and both arms are asserted here
/// rather than one being assumed. **If inline reference emission is ever
/// re-enabled under an armed barrier, `zgc_codegen_honours_read_barrier`
/// must go back to `false` and this test must go red** -- that is what it
/// is for.
#[test]
fn relocation_is_permitted_only_where_reference_loads_are_barriered() {
    let jit_off = crate::runtime::env_cache::disable_jit();
    let codegen_ok = cratonvm_jit::x64::zgc_codegen_honours_read_barrier();
    assert_eq!(
        zgc_relocation_permitted(true),
        jit_off || codegen_ok,
        "requested relocation must be permitted IF AND ONLY IF every \
         reference load is barriered -- either because there is no JIT \
         code, or because the JIT routes reference loads through the \
         barriered helpers when the barrier is armed"
    );
}

/// The hazard the gate exists for, stated so it cannot be lost: with the
/// JIT on and codegen NOT honouring the barrier, relocation must refuse.
///
/// Asserted as an implication rather than by forcing the state, because
/// neither input is settable from a test in this process -- `disable_jit`
/// is latched and `zgc_codegen_honours_read_barrier` is a build property.
/// The value is that the rule is written down as an executable claim: if
/// someone makes the predicate permissive in a way that drops one of the
/// two arms, the assertion above fails.
#[test]
fn relocation_refuses_when_neither_arm_holds() {
    let jit_off = crate::runtime::env_cache::disable_jit();
    let codegen_ok = cratonvm_jit::x64::zgc_codegen_honours_read_barrier();
    if !jit_off && !codegen_ok {
        assert!(
            !zgc_relocation_permitted(true),
            "unbarriered JIT reference loads plus a moving cycle is a \
             use-after-free; the gate must refuse"
        );
    }
}

/// Not requested is not permitted — the branch `vm_init` actually takes
/// today (`RELOCATION_REQUESTED = false`).
///
/// **Weaker than it looks, and saying so is the point.** In a test process
/// with the JIT enabled the `!requested` early return and the JIT refusal
/// both answer `false`, so this assertion cannot distinguish them:
/// deleting the early return leaves it passing. It is kept as a statement
/// of the contract, not as a mutation detector, and the detector for the
/// branch that matters is
/// [`relocation_is_permitted_only_when_the_jit_is_off`] — verified to fail
/// when the gate is short-circuited to always permit. A `--nojit` test
/// process is what would separate these two, and this crate's suite does
/// not run one.
#[test]
fn relocation_that_was_not_requested_is_never_permitted() {
    assert!(
        !zgc_relocation_permitted(false),
        "the gate must never permit relocation nobody asked for"
    );
}
