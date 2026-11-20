use super::fall_through_resolves_more_calls;

fn body(inlined_sites: u32, direct: usize, ir: bool) -> cratonvm_jit::CompiledMethod {
    let mut buf = cratonvm_jit::ExecutableBuffer::new(64).unwrap();
    buf.emit(&[0xC3]); // ret -- metadata-only fixture, never executed
    let mut cm = cratonvm_jit::CompiledMethod::new(buf);
    cm.inline_tally.inlined_sites = inlined_sites;
    cm._direct_callee_entries = (0..direct).map(|i| 0x1000 + i).collect();
    cm.used_ir_backend = ir;
    cm
}

/// The `R12Mega4OneSite.entStatic24` shape: a stripped baseline (no
/// splice, no bind) is replaced by a fall-through that splices or binds;
/// an equal or poorer recompile is abandoned as before, and a published
/// optimizing body is never displaced by a single-pass one.
#[test]
fn only_a_fall_through_that_resolves_more_calls_is_kept() {
    let stripped = body(0, 0, false);
    assert!(fall_through_resolves_more_calls(
        &body(1, 0, false),
        &stripped
    ));
    assert!(fall_through_resolves_more_calls(
        &body(0, 1, false),
        &stripped
    ));
    assert!(!fall_through_resolves_more_calls(
        &body(1, 2, false),
        &body(1, 2, false)
    ));
    assert!(!fall_through_resolves_more_calls(
        &body(0, 0, false),
        &body(1, 1, false)
    ));
    assert!(!fall_through_resolves_more_calls(
        &body(3, 3, false),
        &body(0, 0, true)
    ));
}

/// Outside any compile a pending verdict is stale: taken and not handed
/// back, so the compile that follows reads only its own.
#[test]
fn a_stale_verdict_is_discarded_at_a_top_level_compile() {
    assert_eq!(cratonvm_jit::jit_active_compile_depth(), 0);
    cratonvm_jit::ir_evidence::publish_verdict(false);
    drop(super::NestedVerdictHandOff::take_for_this_compile());
    assert_eq!(cratonvm_jit::ir_evidence::take_last_verdict(), None);
}
