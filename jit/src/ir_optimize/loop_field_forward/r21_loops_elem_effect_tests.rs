use super::{elem_effect_conflicts, ElemKey};
use crate::ir::{AccessOffset, AliasClass, Graph, IrType, MemEffect, MemKind, Op};

/// rev-ir finding 3: a non-access node is judged by its EFFECT class. A
/// write or read the model cannot separate from the cell (the same array,
/// another array of the same kind, raw memory, anything) conflicts; a
/// field, a static, a length word, nothing at all, or an element of
/// another kind on another array does not.
#[test]
fn r21_loops_an_effect_that_may_touch_the_element_conflicts() {
    let mut g = Graph::empty(0);
    let a = g.add(Op::Param(0), IrType::Ref, vec![], None);
    let b = g.add(Op::Param(1), IrType::Ref, vec![], None);
    let k = g.add(Op::Const(1), IrType::Int, vec![], None);
    let j = g.add(Op::Param(2), IrType::Int, vec![], None);
    let keys = [ElemKey {
        array: a,
        index: k,
        kind: MemKind::Int,
    }];
    let elem = |array, index, elem| AliasClass::ArrayElem {
        array,
        index: AccessOffset::Dynamic(index),
        elem,
    };
    // An intrinsic-style write of the same array at any index.
    assert!(elem_effect_conflicts(&g, MemEffect::write(elem(a, j, MemKind::Int)), &keys));
    // Another `int[]` node may be the same array.
    assert!(elem_effect_conflicts(&g, MemEffect::write(elem(b, j, MemKind::Int)), &keys));
    // A reader sees the stale cell too.
    assert!(elem_effect_conflicts(&g, MemEffect::read(elem(a, k, MemKind::Int)), &keys));
    // Raw memory (a scoped / Unsafe access) and the top class.
    assert!(elem_effect_conflicts(&g, MemEffect::write(AliasClass::Raw), &keys));
    assert!(elem_effect_conflicts(&g, MemEffect::OPAQUE, &keys));
    // Disjoint by the model.
    assert!(!elem_effect_conflicts(&g, MemEffect::NONE, &keys));
    assert!(!elem_effect_conflicts(&g, MemEffect::write(elem(b, j, MemKind::Long)), &keys));
    assert!(!elem_effect_conflicts(
        &g,
        MemEffect::read(AliasClass::ArrayLength { array: a }),
        &keys
    ));
    assert!(!elem_effect_conflicts(
        &g,
        MemEffect::write(AliasClass::Static {
            class_id: 3,
            field: 0
        }),
        &keys
    ));
}
