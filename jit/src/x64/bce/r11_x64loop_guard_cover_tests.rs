use super::GuardShape;
use crate::scev::{BoundSource, BoundTerm, PreheaderGuard, SymBound};

/// A decreasing exclusive loop has `bound_addend() == 1`, and the emitter
/// keys `JB` vs `JBE` on `addend == 0` alone, so it emits `JB`: `length >=
/// bound`, never `bound + 2`.
#[test]
fn a_decreasing_exclusive_addend_proves_only_what_jb_proves() {
    let shape = GuardShape {
        iv_local: 0,
        bound_local: Some(2),
        bound_term: BoundTerm::Bound(BoundSource::Local(2)),
        addend: 1,
        step_local: None,
        inline_length: false,
        down_entry: false,
    };
    let need = |k: i32| {
        PreheaderGuard::LengthAtLeast(SymBound {
            base: BoundTerm::Bound(BoundSource::Local(2)),
            addend: k,
        })
    };
    assert!(shape.covers(&need(0)));
    assert!(!shape.covers(&need(1)));
    assert!(!shape.covers(&need(2)));
}
