//! Interpreter round i3 wave 75, lane L7: the element read with a local
//! past 3 (`CRATONVM_INTERP_FUSED_WIDE_LOCALS`,
//! `array_load_wide_locals`). A hit pushes what the unfused `aload;
//! iload; <x>aload` pushes and returns the pc past the `*aload`; every
//! decline pushes nothing.
use super::*;

/// A static frame over `code`, `max_locals` 6, `args` in its first
/// locals.
fn frame(code: Vec<u8>, args: &[Value]) -> Frame {
    Frame::new(
        ClassId::new(0x00ab_0753),
        "cratonvm/test/i75l7/WideLocals".to_string(),
        "m".to_string(),
        "([IIIIIF)V".to_string(),
        None,
        code,
        vec![],
        4,
        6,
        args,
    )
}

fn run(zgc: &crate::memory::vm_heap::VmHeap, f: &mut Frame, iload_pc: usize, array_local: usize) -> Option<usize> {
    let code_len = f.code.len() - 2;
    array_load_wide_locals(zgc, f, code_len, iload_pc, array_local, true, false)
}

/// Locals: 0 = `arr`, 1 = 2, 2..=3 = 0, 4 = `index`, 5 = a float.
fn locals(arr: Option<ObjectRef>, index: Value) -> [Value; 6] {
    [
        Value::Object(arr),
        Value::Int(2),
        Value::Int(0),
        Value::Int(0),
        index,
        Value::Float(0.5),
    ]
}

#[test]
fn the_wide_index_and_the_wide_array_forms_read_the_element() {
    let zgc = VmHeap::from_zgc_capacity(1 << 20);
    let (Some(ints), Some(longs)) = (
        zgc.try_alloc_array(ClassId::new(0), ArrayElementType::Int, 3),
        zgc.try_alloc_array(ClassId::new(0), ArrayElementType::Long, 3),
    ) else {
        return;
    };
    assert!(array_store_prim(
        &zgc,
        ints,
        1,
        0x4f,
        CompactValue::int(-7),
        0
    ));
    assert!(array_store_prim(
        &zgc,
        ints,
        2,
        0x4f,
        CompactValue::int(9),
        0
    ));
    assert!(array_store_prim(
        &zgc,
        longs,
        1,
        0x50,
        CompactValue::long(i64::MIN),
        ValueStack::KIND_MARK_LONG
    ));
    // `aload_0; iload 4; iaload` (the for-each read).
    let mut f = frame(
        vec![0x2a, 0x15, 0x04, 0x2e, 0xb1],
        &locals(Some(ints), Value::Int(1)),
    );
    assert_eq!(run(&zgc, &mut f, 1, 0), Some(4));
    assert_eq!(f.stack.len(), 1);
    assert_eq!(f.stack.pop_compact().as_int(), Some(-7));
    // `aload_0; iload 4; laload`.
    let mut f = frame(
        vec![0x2a, 0x15, 0x04, 0x2f, 0xb1],
        &locals(Some(longs), Value::Int(1)),
    );
    assert_eq!(run(&zgc, &mut f, 1, 0), Some(4));
    assert_eq!(f.stack.pop_compact().as_long_unchecked(), i64::MIN);
    // `aload 0; iload_1; iaload` (the wide-array arm, short index:
    // local 1 holds 2).
    let mut f = frame(
        vec![0x19, 0x00, 0x1b, 0x2e, 0xb1],
        &locals(Some(ints), Value::Int(1)),
    );
    assert_eq!(run(&zgc, &mut f, 2, 0), Some(4));
    assert_eq!(f.stack.pop_compact().as_int(), Some(9));
    // `aload 0; iload 4; iaload`.
    let mut f = frame(
        vec![0x19, 0x00, 0x15, 0x04, 0x2e, 0xb1],
        &locals(Some(ints), Value::Int(1)),
    );
    assert_eq!(run(&zgc, &mut f, 2, 0), Some(5));
    assert_eq!(f.stack.pop_compact().as_int(), Some(-7));
}

#[test]
fn every_decline_pushes_nothing() {
    let zgc = VmHeap::from_zgc_capacity(1 << 20);
    let Some(ints) = zgc.try_alloc_array(ClassId::new(0), ArrayElementType::Int, 3) else {
        return;
    };
    let read = vec![0x2a, 0x15, 0x04, 0x2e, 0xb1];
    let cases: [(Vec<u8>, Option<ObjectRef>, Value); 8] = [
        // Out of range either side (the unfused `iaload` throws).
        (read.clone(), Some(ints), Value::Int(3)),
        (read.clone(), Some(ints), Value::Int(-1)),
        // A null array (the unfused `iaload` throws the NPE).
        (read.clone(), None, Value::Int(0)),
        // An element type the opcode does not name.
        (
            vec![0x2a, 0x15, 0x04, 0x33, 0xb1],
            Some(ints),
            Value::Int(0),
        ),
        // An index local that is not an int.
        (read.clone(), Some(ints), Value::Float(1.0)),
        // A local past `max_locals`.
        (
            vec![0x2a, 0x15, 0x09, 0x2e, 0xb1],
            Some(ints),
            Value::Int(0),
        ),
        // Not an `*aload` after the index.
        (
            vec![0x2a, 0x15, 0x04, 0x36, 0x05, 0xb1],
            Some(ints),
            Value::Int(0),
        ),
        // Not an `iload` at all.
        (
            vec![0x2a, 0x16, 0x04, 0x2e, 0xb1],
            Some(ints),
            Value::Int(0),
        ),
    ];
    for (code, arr, index) in cases {
        let mut f = frame(code.clone(), &locals(arr, index));
        assert_eq!(run(&zgc, &mut f, 1, 0), None, "{code:x?}");
        assert_eq!(f.stack.len(), 0, "{code:x?}");
    }
    // A group cut off by the end of the code.
    let mut f = frame(read, &locals(Some(ints), Value::Int(0)));
    assert_eq!(
        array_load_wide_locals(&zgc, &mut f, 3, 1, 0, true, false),
        None
    );
    assert_eq!(f.stack.len(), 0);
    // An array local past `max_locals`.
    let mut f = frame(
        vec![0x19, 0x07, 0x15, 0x04, 0x2e, 0xb1],
        &locals(Some(ints), Value::Int(0)),
    );
    assert_eq!(run(&zgc, &mut f, 2, 7), None);
    assert_eq!(f.stack.len(), 0);
}
