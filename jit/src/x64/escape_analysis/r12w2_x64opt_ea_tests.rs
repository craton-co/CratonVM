//! Round 12 wave 2 (x64opt): `analyze_escapes` and a value-returning
//! `invokespecial` (`super.foo()`).

use super::{analyze_escapes, InvokeSpecialShape};
use rustc_hash::FxHashMap;

/// `use(new T(), super.foo().f)`: `T` sits under the non-void
/// `invokespecial` and is really passed to `use`. The walk used to pop
/// only the call's receiver and push nothing, so `getfield` popped `T` in
/// the result's place and the `invokestatic` escaped an empty stack.
#[test]
fn a_value_returning_invokespecial_does_not_hide_an_object_beneath_it() {
    let code: Vec<u8> = vec![
        0xbb, 0x00, 0x01, // 0: new #1 (T)
        0x59, // 3: dup
        0xb7, 0x00, 0x02, // 4: invokespecial T.<init>()V (proven empty)
        0x2a, // 7: aload_0
        0xb7, 0x00, 0x03, // 8: invokespecial Super.foo()LR;
        0xb4, 0x00, 0x04, // 11: getfield R.f:I
        0xb8, 0x00, 0x05, // 14: invokestatic use(LT;I)V
        0xb1, // 17: return
    ];
    let mut shapes: FxHashMap<usize, InvokeSpecialShape> = FxHashMap::default();
    shapes.insert(
        4,
        InvokeSpecialShape {
            arg_slots: 1,
            is_trivial_void_init: true,
        },
    );
    shapes.insert(
        8,
        InvokeSpecialShape {
            arg_slots: 1,
            is_trivial_void_init: false,
        },
    );
    assert!(
        !analyze_escapes(&code, code.len(), &shapes).contains(&0),
        "`T` is an argument of `use` and must escape"
    );
}

/// `S = t; return t.x` with `t` fresh: the plan must poison an object
/// published through `putstatic` itself, like `putfield`/`aastore` values,
/// rather than rely on `analyze_escapes` alone.
#[test]
fn the_plan_poisons_a_putstatic_value() {
    let code: Vec<u8> = vec![
        0xbb, 0x00, 0x01, // 0: new #1
        0x59, // 3: dup
        0xb7, 0x00, 0x02, // 4: invokespecial <init>()V
        0x59, // 7: dup
        0xb3, 0x00, 0x03, // 8: putstatic #3
        0xb4, 0x00, 0x04, // 11: getfield #4
        0xac, // 14: ireturn
    ];
    // LEAK(intentional): test-only; the plan reads it through a raw pointer.
    let init: *const crate::JitInvokeInfo = Box::leak(Box::new(crate::JitInvokeInfo {
        class_name: "Foo",
        method_name: "<init>",
        descriptor: "()V",
        num_jit_args: 1,
        return_type: b'V',
        invoke_kind: 1,
        declaring_class_id: 0,
        owner_class_id: 0,
    }));
    let mut non_escaping = std::collections::HashSet::new();
    non_escaping.insert(0usize);
    let new_info = vec![(0usize, 1u32, 1usize, true, true)];
    let invoke_info = vec![(4usize, init)];
    let plan = super::plan_scalar_replacement(
        &code,
        code.len(),
        &non_escaping,
        &new_info,
        &invoke_info,
        0,
    );
    assert!(
        plan.objects.is_empty() && plan.field_ops.is_empty(),
        "an object stored to a static is never scalar-replaced"
    );
}
