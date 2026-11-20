use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{NativeClassAccess, NativeHeapAccess, NativeSystemAccess};

#[test]
fn j27_the_batched_mint_copies_the_prototype_and_binds_the_new_value_last() {
    let mut ctx = MockNativeContext::new();
    // A `Species_LIL` layout: type 0, form 1, argL0 2, argI1 3, argL2 4.
    let species = ClassId::new(0x7271);
    let other = ClassId::new(0x7272);
    let proto = ctx.alloc_object(species, 5);
    let fresh = ctx.alloc_object(species, 5);
    let [ty, old_form, new_form, target, old_x, new_x] =
        [0; 6].map(|_| ctx.alloc_object(other, 0));
    ctx.set_field(proto, 0, Value::Object(Some(ty)));
    ctx.set_field(proto, 1, Value::Object(Some(old_form)));
    ctx.set_field(proto, 2, Value::Object(Some(target)));
    ctx.set_field(proto, 3, Value::Int(42));
    ctx.set_field(proto, 4, Value::Object(Some(old_x)));
    let mut args = [0usize; REAL_BIND_MAX_ARGS];
    args[..3].copy_from_slice(&[2, 3, 4]);
    let mut desc = [b'L'; REAL_BIND_MAX_ARGS];
    desc[1] = species_letter_descriptor('I').expect("an int letter");
    let slots = RealBindSlots {
        type_slot: 0,
        form_slot: 1,
        args,
        desc,
        count: 3,
    };
    real_bind_mint_batched(&ctx, proto, new_form, fresh, Some(new_x), &slots);
    assert_eq!(ctx.get_field(fresh, 0), Value::Object(Some(ty)));
    assert_eq!(ctx.get_field(fresh, 1), Value::Object(Some(new_form)));
    assert_eq!(ctx.get_field(fresh, 2), Value::Object(Some(target)));
    assert_eq!(ctx.get_field(fresh, 3), Value::Int(42));
    assert_eq!(ctx.get_field(fresh, 4), Value::Object(Some(new_x)));
    // The prototype is read, never written.
    assert_eq!(ctx.get_field(proto, 1), Value::Object(Some(old_form)));
    assert_eq!(ctx.get_field(proto, 4), Value::Object(Some(old_x)));
    // A `null` bound value is stored as `null`.
    let fresh2 = ctx.alloc_object(species, 5);
    real_bind_mint_batched(&ctx, proto, new_form, fresh2, None, &slots);
    assert_eq!(ctx.get_field(fresh2, 4), Value::Object(None));
    assert_eq!(ctx.get_field(fresh2, 2), Value::Object(Some(target)));
}


#[test]
fn j27_species_letters_map_to_their_field_descriptors() {
    let letters: Vec<Option<u8>> = "LIJFDX".chars().map(species_letter_descriptor).collect();
    assert_eq!(
        letters,
        vec![Some(b'L'), Some(b'I'), Some(b'J'), Some(b'F'), Some(b'D'), None]
    );
}
