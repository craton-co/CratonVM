//! Round 15 wave 1 (lane vh): `collectReturnValue`'s composition, the
//! value-site judgement and the typed (F7-1) argument screen.
use super::*;
use cratonvm_native_api::poly_call_site::vh_site;

#[test]
fn a_unary_filter_is_a_collect_at_zero_with_no_permute() {
    let plan =
        collect_return_value_plan("(Ljava/lang/invoke/VarHandle;Ljava/lang/Object;)I", "(I)J");
    assert_eq!(
        plan,
        Some(CollectReturnValuePlan {
            leading: 0,
            permute: None
        })
    );
}

#[test]
fn leading_filter_parameters_trail_the_adapter() {
    // `V filter(B0, B1, T)` over `T target(A0, A1, A2)`:
    // collected `(B0, B1, A0, A1, A2)V`, adapter `(A0, A1, A2, B0, B1)V`.
    let plan = collect_return_value_plan(
        "(Ljava/lang/invoke/VarHandle;JS)I",
        "(Ljava/lang/String;DI)J",
    );
    let Some(CollectReturnValuePlan {
        leading,
        permute: Some((desc, reorder)),
    }) = plan
    else {
        panic!("no permute planned for a two-parameter prefix");
    };
    assert_eq!(leading, 2);
    assert_eq!(desc, "(Ljava/lang/invoke/VarHandle;JSLjava/lang/String;D)J");
    // Collected parameter i lands at adapter position reorder[i].
    assert_eq!(reorder, vec![3, 4, 0, 1, 2]);
}

#[test]
fn shapes_the_jdk_body_decides_are_not_planned() {
    // The filter's value parameter is not the target's return.
    assert_eq!(collect_return_value_plan("()I", "(J)J"), None);
    // A value filter over a `void` target.
    assert_eq!(collect_return_value_plan("()V", "(I)I"), None);
    // `V filter()` over a `void` target is `filterReturnValue`'s case.
    assert_eq!(
        collect_return_value_plan("(I)V", "()I"),
        Some(CollectReturnValuePlan {
            leading: 0,
            permute: None
        })
    );
    // A `V filter()` cannot swallow a value.
    assert_eq!(collect_return_value_plan("()I", "()I"), None);
}

#[test]
fn value_counts_follow_the_access_types() {
    for (ordinal, (name, _)) in VH_ACCESS_MODES.iter().enumerate() {
        let want = if name.starts_with("GET_AND_") {
            1
        } else if name.starts_with("GET") {
            0
        } else if name.starts_with("SET") {
            1
        } else {
            2
        };
        assert_eq!(vh_access_mode_value_count(ordinal), want, "{name}");
    }
}

#[test]
fn an_int_site_does_not_narrow_into_a_char_value() {
    assert_eq!(vh_indirect_site_admits("C", b'I'), Some(false));
    assert_eq!(vh_indirect_site_admits("S", b'B'), Some(true));
    assert_eq!(vh_indirect_site_admits("J", b'I'), Some(true));
    assert_eq!(vh_indirect_site_admits("I", b'I'), Some(true));
    assert_eq!(vh_indirect_site_admits("Z", b'I'), Some(false));
    // A primitive site boxes: `Integer` is a `Number`, not a `Long`.
    assert_eq!(
        vh_indirect_site_admits("Ljava/lang/Number;", b'I'),
        Some(true)
    );
    assert_eq!(
        vh_indirect_site_admits("Ljava/lang/Long;", b'I'),
        Some(false)
    );
    // Not judged: nothing armed, or a reference into a reference.
    assert_eq!(vh_indirect_site_admits("C", vh_site::UNKNOWN), None);
    assert_eq!(
        vh_indirect_site_admits("Ljava/lang/String;", vh_site::OBJECT),
        None
    );
    // A reference site into a primitive: `canConvert`'s wrapper rule.
    assert_eq!(vh_indirect_site_admits("I", vh_site::W_LONG), Some(false));
    assert_eq!(vh_indirect_site_admits("J", vh_site::W_INTEGER), Some(true));
}

/// Interpreter round i2 wave 55, lane L7: the compare-and-exchange witness
/// is `expected` on an exchange (which stores), the value found on a
/// mismatch (which does not), and a context whose CAS equality is
/// narrower than `vh_values_match` (the mock compares floats with `==`,
/// so a NaN never swaps) still answers, after the bounded retries.
#[test]
fn the_compare_and_exchange_witness_is_the_value_the_slot_held() {
    use cratonvm_native_api::NativeHeapAccess;
    let mut ctx = crate::test_utils::mock_ctx();
    let obj = ctx.alloc_object(ClassId::new(1), 1);
    ctx.set_field(obj, 0, Value::Int(7));
    let w = vh_compare_and_exchange_slot(&mut ctx, obj, 0, false, Value::Int(7), Value::Int(9));
    assert_eq!(w, Value::Int(7));
    assert_eq!(ctx.get_field(obj, 0), Value::Int(9), "exchanged");
    let w =
        vh_compare_and_exchange_slot(&mut ctx, obj, 0, false, Value::Int(7), Value::Int(11));
    assert_eq!(w, Value::Int(9), "the witness of a mismatch");
    assert_eq!(ctx.get_field(obj, 0), Value::Int(9), "nothing stored");
    let nan = Value::Float(f32::NAN);
    ctx.set_field(obj, 0, nan.clone());
    let w = vh_compare_and_exchange_slot(&mut ctx, obj, 0, false, nan, Value::Float(1.0));
    assert!(
        matches!(w, Value::Float(f) if f.is_nan()),
        "bounded, then the re-read: {w:?}"
    );
}

#[test]
fn typed_arguments_are_the_declared_types_or_nothing() {
    let mut ctx = crate::test_utils::mock_ctx();
    let params: Vec<String> = [
        "Ljava/lang/invoke/VarHandle;",
        "J",
        "I",
        "Ljava/lang/Object;",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    // An `int` into a `long` coordinate is widened, as `asType` does.
    let args = [
        Value::Object(None),
        Value::Int(3),
        Value::Int(4),
        Value::Object(None),
    ];
    assert_eq!(
        vh_indirect_typed_args(&mut ctx, &args, &params),
        Some(vec![Value::Long(3), Value::Int(4), Value::Object(None)])
    );
    // A `long` cannot narrow into the `int`; a primitive into the
    // `Object` needs a box: both left to `invokeWithArguments`.
    let narrowing = [
        Value::Object(None),
        Value::Long(3),
        Value::Long(4),
        Value::Object(None),
    ];
    assert_eq!(vh_indirect_typed_args(&mut ctx, &narrowing, &params), None);
    let boxing = [
        Value::Object(None),
        Value::Long(3),
        Value::Int(4),
        Value::Int(5),
    ];
    assert_eq!(vh_indirect_typed_args(&mut ctx, &boxing, &params), None);
    // An arity the mode handle does not take is not typed.
    assert_eq!(vh_indirect_typed_args(&mut ctx, &args[..3], &params), None);
}

#[test]
fn a_store_modes_last_coordinate_is_judged_from_the_cell() {
    // `filterCoordinates(arrayElementVarHandle(int[].class), 1, (char)int)`:
    // `set` is `(VarHandle, int[], char, int)void`.
    let mut ctx = crate::test_utils::mock_ctx();
    let set = 1; // `VH_ACCESS_MODES[1]` is SET
    assert_eq!(VH_ACCESS_MODES[set].1, "set");
    let params: Vec<String> = ["Ljava/lang/invoke/VarHandle;", "[I", "C", "I"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let args = [
        Value::Object(None),
        Value::Object(None),
        Value::Int(1),
        Value::Int(7),
    ];
    let pack = |coord: u8, value: u8| (u64::from(coord) << 8) | u64::from(value);
    // `f.set(arr, (char) 1, 7)`: the declared types exactly.
    assert!(
        vh_indirect_value_site_refusal(&mut ctx, &args, &params, set, pack(b'C', b'I'))
            .is_none()
    );
    // `f.set(arr, 1, 7)`: an `int` narrowed into the `char` coordinate.
    assert!(
        vh_indirect_value_site_refusal(&mut ctx, &args, &params, set, pack(b'I', b'I'))
            .is_some()
    );
    // Nothing armed: not judged.
    assert!(vh_indirect_value_site_refusal(&mut ctx, &args, &params, set, 0).is_none());
}

/// Round 15 wave 3 (lane vh3; VH1-3,
/// `r15w1-vh-indirect-coordinate-narrowing-FIXED-20260930.md`): a READ mode's
/// coordinates, and a store mode's EARLIER coordinates, are judged from
/// the parameter-site word, and a door that armed only the 16-bit cell
/// leaves them unjudged.
#[test]
fn every_covered_coordinate_is_judged_from_the_param_site_word() {
    use cratonvm_native_api::poly_call_site::vh_param_site_classes;
    if !vh_indirect_site_all_coords_enabled() || !vh_indirect_site_coord_wmte_enabled() {
        return; // an explicit `0` in the test environment wins
    }
    fn strings(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|s| s.to_string()).collect()
    }
    let mut ctx = crate::test_utils::mock_ctx();
    // `get` of `(VarHandle, int[], char)int`: `f.get(arr, i)` is `([II)I`.
    let get = 0;
    assert_eq!(VH_ACCESS_MODES[get].1, "get");
    let params = strings(&["Ljava/lang/invoke/VarHandle;", "[I", "C"]);
    let args = [Value::Object(None), Value::Object(None), Value::Int(1)];
    let wrong = vh_param_site_classes("([II)I");
    let right = vh_param_site_classes("([IC)I");
    assert!(vh_indirect_value_site_refusal(&mut ctx, &args, &params, get, wrong).is_some());
    assert!(vh_indirect_value_site_refusal(&mut ctx, &args, &params, get, right).is_none());
    // A byte view of the coordinate that widens (`char` into `int`) is
    // admitted: `(VarHandle, int[], int)` from a `([IC)I` site.
    let int_coord = strings(&["Ljava/lang/invoke/VarHandle;", "[I", "I"]);
    assert!(vh_indirect_value_site_refusal(&mut ctx, &args, &int_coord, get, right).is_none());
    // `set` of `(VarHandle, char, int[], int)void` (a permuted handle):
    // the FIRST coordinate sits three bytes up.
    let set = 1;
    let permuted = strings(&["Ljava/lang/invoke/VarHandle;", "C", "[I", "I"]);
    let set_args = [
        Value::Object(None),
        Value::Int(1),
        Value::Object(None),
        Value::Int(7),
    ];
    let narrowed = vh_param_site_classes("(I[II)V");
    let exact = vh_param_site_classes("(C[II)V");
    assert!(
        vh_indirect_value_site_refusal(&mut ctx, &set_args, &permuted, set, narrowed).is_some()
    );
    assert!(
        vh_indirect_value_site_refusal(&mut ctx, &set_args, &permuted, set, exact).is_none()
    );
    // Only the old 16-bit cell armed: the first coordinate reads UNKNOWN.
    assert!(vh_indirect_value_site_refusal(
        &mut ctx,
        &set_args,
        &permuted,
        set,
        narrowed & 0xffff
    )
    .is_none());
}
