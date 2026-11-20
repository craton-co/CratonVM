use super::*;

fn counted(local: usize, init: IntRange, cmp: ExitCmp, bound: BoundSource) -> CountedLoop {
    CountedLoop {
        header_pc: 0,
        back_edge_pc: 0,
        iv: AffineIv {
            local,
            init,
            stride: Stride::Const(1),
        },
        cmp,
        bound,
        bound_range: IntRange::unknown(),
        form: LoopForm::PreTested,
        modified_locals: 1u64 << local,
        heap_stable: true,
        has_other_exit: false,
    }
}

/// `for (i = 0; i < 4; ) { i++; for (j = 3; j < i; j++) a[j]; }`, analysed
/// with the outer IV bound to its exit-test range `[0, 3]`: the inner span
/// comes out empty, although the access runs once (at `i == 4`). An empty
/// span that rests on a bound local is no longer `Static`.
#[test]
fn an_empty_inner_span_under_a_bound_outer_iv_is_not_static() {
    let outer = counted(1, IntRange::constant(0), ExitCmp::Ge, BoundSource::Const(4));
    let inner = counted(3, IntRange::constant(3), ExitCmp::Ge, BoundSource::Local(1));
    let env = RangeEnv::new().with_loop_iv(&outer);
    assert_eq!(env.local(1), IntRange::new(0, 3));
    let proof =
        inner.prove_index_in_bounds(&IndexExpr::identity(3), IntRange::constant(4), &env);
    assert_ne!(proof, BoundsProof::Static);
    assert!(matches!(proof, BoundsProof::Refused(_)));
}

/// With nothing bound, a loop that provably never runs still needs no
/// check.
#[test]
fn an_empty_span_with_no_bound_local_stays_static() {
    let l = counted(1, IntRange::constant(5), ExitCmp::Ge, BoundSource::Const(5));
    let env = RangeEnv::new();
    let proof = l.prove_index_in_bounds(&IndexExpr::identity(1), IntRange::constant(0), &env);
    assert_eq!(proof, BoundsProof::Static);
}

/// An outer loop at pcs `[0, 8)`, back edge at 8, IV in local 1.
fn placed_outer() -> CountedLoop {
    let mut outer = counted(1, IntRange::constant(0), ExitCmp::Ge, BoundSource::Const(4));
    outer.header_pc = 0;
    outer.back_edge_pc = 8;
    outer
}

/// `iinc 1 1` in front of the inner header (pc 3): the outer advance runs
/// before the inner loop, so the exit-test range is not bound.
#[test]
fn with_loop_iv_placed_drops_the_binding_when_the_prefix_advances_the_outer_iv() {
    let code = [0x84u8, 1, 1, 0x00, 0x00, 0x00, 0x00, 0x00, 0xa7, 0xff, 0xf8];
    let env = RangeEnv::new().with_loop_iv_placed(&placed_outer(), &code, 3);
    assert_eq!(env, RangeEnv::new());
}

/// Nothing in the prefix writes local 1: the binding matches
/// `with_loop_iv`.
#[test]
fn with_loop_iv_placed_binds_when_the_prefix_leaves_the_outer_iv_alone() {
    let code = [0x84u8, 2, 1, 0x00, 0x00, 0x00, 0x00, 0x00, 0xa7, 0xff, 0xf8];
    let outer = placed_outer();
    let env = RangeEnv::new().with_loop_iv_placed(&outer, &code, 3);
    assert_eq!(env, RangeEnv::new().with_loop_iv(&outer));
    assert_eq!(env.local(1), IntRange::new(0, 3));
}

/// A prefix store to slot 70 cannot be represented, and an inner header
/// outside the outer body cannot be placed: both refuse the binding.
#[test]
fn with_loop_iv_placed_refuses_what_it_cannot_read() {
    let high = [
        0x36u8, 70, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xa7, 0xff, 0xf8,
    ];
    let outer = placed_outer();
    assert_eq!(
        RangeEnv::new().with_loop_iv_placed(&outer, &high, 3),
        RangeEnv::new()
    );
    let clean = [0x00u8; 11];
    assert_eq!(
        RangeEnv::new().with_loop_iv_placed(&outer, &clean, 0),
        RangeEnv::new()
    );
    assert_eq!(
        RangeEnv::new().with_loop_iv_placed(&outer, &clean, 8),
        RangeEnv::new()
    );
    assert_eq!(
        RangeEnv::new().with_loop_iv_placed(&outer, &clean, 40),
        RangeEnv::new()
    );
}
