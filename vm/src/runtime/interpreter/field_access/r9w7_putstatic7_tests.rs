use super::*;

/// JVMS §6.5 `putstatic` narrowing, written out independently of
/// `narrow_int_to_field_type` so the test pins the SPEC, not the helper.
/// `jit/tests/r9w7_putstatic7_narrow.rs` pins the single-pass JIT against
/// the same table (`jvms_static_store`), which is what makes the two tiers
/// provably agree.
fn jvms_static_store(tag: u8, x: i32) -> i32 {
    match tag {
        b'Z' => x & 1,
        b'B' => i32::from(x as i8),
        b'C' => i32::from(x as u16),
        b'S' => i32::from(x as i16),
        _ => x,
    }
}

const INPUTS: [i32; 12] = [
    0,
    1,
    2,
    3,
    -1,
    300,
    -129,
    0x0001_8000,
    0x0001_0000,
    0x1234_5680,
    i32::MIN,
    i32::MAX,
];

/// `iconst_2; putstatic Flag:Z` must store 0 (and `3` store 1); a
/// `byte`/`char`/`short` static keeps only its own width; `I` is
/// untouched. Before round 9 wave 7 the catch-all arm stored the raw int.
#[test]
fn putstatic_narrows_sub_int_values_to_the_field_width() {
    for &tag in b"ZBCSI" {
        for &x in &INPUTS {
            let mut stack = crate::runtime::ValueStack::new(4);
            stack.push(Value::Int(x)).expect("push");
            let got = pop_static_field_value(&mut stack, Some(tag)).expect("pop");
            assert_eq!(
                got,
                Value::Int(jvms_static_store(tag, x)),
                "putstatic {}:{x:#x}",
                tag as char
            );
        }
    }
}

/// The static/instance mismatch reports HotSpot's wording, naming the
/// class in external (dot) form and the direction the OPCODE expected.
#[test]
fn field_staticness_mismatch_uses_hotspot_wording() {
    assert_eq!(
        field_staticness_message(true, "p/q/C", "f"),
        "Expected static field p.q.C.f"
    );
    assert_eq!(
        field_staticness_message(false, "C$Inner", "count"),
        "Expected non-static field C$Inner.count"
    );
}

fn final_field(declaring: u32, is_static: bool, is_final: bool) -> ResolvedField {
    ResolvedField {
        declaring_class_id: ClassId::new(declaring),
        field_index: 0,
        is_static,
        is_volatile: false,
        is_final,
        is_reference: false,
        desc_byte: b'I',
    }
}

/// JVMS §6.5 / HotSpot `LinkResolver::resolve_field`: the declaring-class
/// rule holds for every class file version, the initializer-method rule
/// only from version 53, and a non-final field is never refused.
#[test]
fn final_field_put_violation_follows_hotspot_rules() {
    use FinalPutViolation::*;
    let c = ClassId::new(5);
    let v =
        |f: &ResolvedField, m: &str, major: u16| final_field_put_violation(f, c, m, || major);
    // Not final: anything goes, and the version is never asked.
    assert_eq!(
        final_field_put_violation(&final_field(9, false, false), c, "m", || {
            panic!("version asked for a non-final field")
        }),
        None
    );
    // Another class's final field: refused whatever the method or version.
    assert_eq!(
        v(&final_field(9, false, true), "<init>", 69),
        Some(DifferentClass)
    );
    assert_eq!(
        v(&final_field(9, true, true), "<clinit>", 50),
        Some(DifferentClass)
    );
    // Own instance final: `<init>` always; other methods only below 53.
    assert_eq!(v(&final_field(5, false, true), "<init>", 69), None);
    assert_eq!(v(&final_field(5, false, true), "reset", 52), None);
    assert_eq!(
        v(&final_field(5, false, true), "reset", 53),
        Some(DifferentMethod)
    );
    assert_eq!(
        v(&final_field(5, false, true), "<clinit>", 69),
        Some(DifferentMethod)
    );
    // Own static final: `<clinit>` only, from 53.
    assert_eq!(v(&final_field(5, true, true), "<clinit>", 69), None);
    assert_eq!(
        v(&final_field(5, true, true), "<init>", 69),
        Some(DifferentMethod)
    );
    assert_eq!(v(&final_field(5, true, true), "<init>", 52), None);
    // A legal constructor store never looks the version up.
    assert_eq!(
        final_field_put_violation(&final_field(5, false, true), c, "<init>", || {
            panic!("version asked for an <init> store")
        }),
        None
    );
}

/// JIT round 21 wave 14, lane hidden-text: a hidden class keeps its
/// `/0x<hex>` tail in both class names (`Klass::external_name()`), and in
/// the staticness text.
#[test]
fn hidden_class_names_keep_their_slash_0x_tail_in_field_texts() {
    assert_eq!(
        final_field_put_message(
            FinalPutViolation::DifferentClass,
            true,
            "p/H/0x0000000000000002",
            "TAG",
            "p/D/0x0000000000000003",
            "m"
        ),
        "Update to static final field p.H/0x0000000000000002.TAG attempted from a \
         different class (p.D/0x0000000000000003) than the field's declaring class"
    );
    assert_eq!(
        field_staticness_message(true, "p/H/0x2a", "f"),
        "Expected static field p.H/0x2a.f"
    );
}

/// HotSpot's two `IllegalAccessError` texts, including the trailing space
/// of the initializer-method form.
#[test]
fn final_field_put_message_uses_hotspot_wording() {
    assert_eq!(
        final_field_put_message(
            FinalPutViolation::DifferentClass,
            false,
            "p/C",
            "f",
            "p/D",
            "m"
        ),
        "Update to non-static final field p.C.f attempted from a different class (p.D) \
         than the field's declaring class"
    );
    assert_eq!(
        final_field_put_message(
            FinalPutViolation::DifferentMethod,
            true,
            "p/C",
            "S",
            "p/C",
            "set"
        ),
        "Update to static final field p.C.S attempted from a different method (set) \
         than the initializer method <clinit> "
    );
    assert_eq!(
        final_field_put_message(
            FinalPutViolation::DifferentMethod,
            false,
            "C",
            "x",
            "C",
            "reset"
        ),
        "Update to non-static final field C.x attempted from a different method (reset) \
         than the initializer method <init> "
    );
}

/// The JIT doors' opcode scan matches only the requested opcode and
/// operand at an instruction start, never operand bytes that happen to
/// spell it.
#[test]
fn code_has_field_put_matches_only_instruction_starts() {
    // sipush 0xb5_00; pop; getfield #7; putfield #7; return
    let code = [
        0x11, 0xb5, 0x00, 0x57, 0xb4, 0x00, 0x07, 0xb5, 0x00, 0x07, 0xb1,
    ];
    assert!(code_has_field_put(&code, 7, false));
    assert!(!code_has_field_put(&code, 7, true));
    assert!(
        !code_has_field_put(&code, 0x57, false),
        "sipush operand is not a putfield"
    );
    // getstatic #3; putstatic #4; return
    let code = [0xb2, 0x00, 0x03, 0xb3, 0x00, 0x04, 0xb1];
    assert!(code_has_field_put(&code, 4, true));
    assert!(!code_has_field_put(&code, 3, true));
}

/// Non-int carriers and descriptor-less pops are passed through.
#[test]
fn putstatic_narrowing_leaves_other_descriptors_alone() {
    let mut stack = crate::runtime::ValueStack::new(4);
    stack.push(Value::Float(2.5)).expect("push");
    assert_eq!(
        pop_static_field_value(&mut stack, Some(b'F')).expect("pop"),
        Value::Float(2.5)
    );
    stack.push(Value::Int(0x1234_5680)).expect("push");
    assert_eq!(
        pop_static_field_value(&mut stack, None).expect("pop"),
        Value::Int(0x1234_5680)
    );
}

/// i66-L7: a `D` pop decodes before it pops. A tag no `double` producer
/// leaves returns `Err` with the operand still on the stack, and an empty
/// stack reports the underflow instead of wrapping `len`.
#[test]
fn a_d_pop_that_fails_leaves_the_operand_stack_as_it_was() {
    let mut stack = crate::runtime::ValueStack::new(4);
    stack.push(Value::Int(7)).expect("push");
    stack.push(Value::Float(2.5)).expect("push");
    assert!(pop_static_field_value(&mut stack, Some(b'D')).is_err());
    assert_eq!(stack.len(), 2, "the odd-tag error popped nothing");
    assert_eq!(stack.pop().expect("pop"), Value::Float(2.5));

    let mut empty = crate::runtime::ValueStack::new(4);
    assert!(pop_static_field_value(&mut empty, Some(b'D')).is_err());
    assert_eq!(empty.len(), 0);

    // An accepted tag still pops exactly one slot.
    stack.push(Value::Long(0)).expect("push");
    assert_eq!(
        pop_static_field_value(&mut stack, Some(b'D')).expect("pop"),
        Value::Double(0.0)
    );
    assert_eq!(stack.len(), 1);
}
