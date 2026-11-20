use super::class_word_of_bytes;
use crate::ir::{CmpOp, Graph, IrType, Op};

/// `int f(Object o) { return <expr over ClassWordOf(o)>; }`, lowered.
/// `compare_with`: `None` returns the word itself, `Some(id)` returns
/// `Cmp(Eq, word, Const(id as i32))`, as the builder emits it.
fn body(compare_with: Option<u32>) -> crate::CompiledMethod {
    let mut g = Graph::empty(0);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let _mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let obj = g.add(Op::Param(0), IrType::Ref, vec![start], None);
    let word = g.add(Op::ClassWordOf, IrType::Int, vec![obj], Some(0));
    let value = match compare_with {
        None => word,
        Some(id) => {
            // Cast: the id's bits, read as an `int` (the builder's form).
            let c = g.add(Op::Const(i64::from(id as i32)), IrType::Int, vec![], None);
            g.add(Op::Cmp(CmpOp::Eq), IrType::Int, vec![word, c], Some(0))
        }
    };
    g.exit = g.add(Op::Return, IrType::Void, vec![ctrl, value], Some(0));
    let schedule = crate::ir_schedule::schedule(&g);
    // SAFETY: `JitRuntimeHelpers` is `#[repr(C)]` and every field is a `usize`, so all-zero is a valid value; this body calls no helper.
    let helpers: crate::JitRuntimeHelpers = unsafe { std::mem::zeroed() };
    super::lower(&g, &schedule, 1, 1, &helpers).expect("lowers")
}

fn run(cm: &crate::CompiledMethod, class_id: u32, kind_byte: u8) -> i64 {
    assert!(
        cratonvm_types::KIND_TAGS_BYTE_OFFSET < 32,
        "the kind byte lies in the fake header"
    );
    let mut header = [0u64; 4];
    let base = header.as_mut_ptr().cast::<u8>();
    // SAFETY: both writes are inside the 32-byte, 8-aligned `header`.
    unsafe {
        base.cast::<u32>().write(class_id);
        *base.add(cratonvm_types::KIND_TAGS_BYTE_OFFSET) = kind_byte;
    }
    // SAFETY: an `int f(ref)` body with no call and no guard; the
    // reference is a live 32-byte buffer. Cast: pointer as the oop.
    unsafe { cm.try_call(&[base as i64]) }.expect("call")
}

#[test]
#[cfg(target_arch = "x86_64")]
fn class_word_of_answers_the_class_word_of_plain_objects_and_zero_otherwise() {
    let cm = body(None);
    let plain = cratonvm_types::ObjectKind::Object as u8;
    // Cast: an `Int` result is the low 32 bits.
    assert_eq!(
        run(&cm, 77, plain) as i32,
        77,
        "a plain object's class word"
    );
    assert_eq!(run(&cm, 77, plain | 1) as i32, 0, "an array answers 0");
    // SAFETY: as in `run`; a null reference is answered without a load.
    let null = unsafe { cm.try_call(&[0]) }.expect("call");
    // Cast: an `Int` result is the low 32 bits.
    assert_eq!(null as i32, 0, "null answers 0");
    // Cast: the sign-extended reading of a high-bit id.
    assert_eq!(run(&cm, 0x8000_0005, plain) as i32, 0x8000_0005u32 as i32);
}

/// The chain's test answers exactly `ExactClassIs`'s truth table,
/// including an id with the sign bit set (the word is sign-extended and
/// the constant is the id's `int` reading) and a high-half difference.
#[test]
#[cfg(target_arch = "x86_64")]
fn the_chain_compare_answers_like_exact_class_is() {
    let plain = cratonvm_types::ObjectKind::Object as u8;
    for id in [9u32, 0x0001_0009, 0x8000_0009] {
        let cm = body(Some(id));
        // Cast: an `Int` result is the low 32 bits.
        assert_eq!(run(&cm, id, plain) as i32, 1, "exact class {id:#x}");
        assert_eq!(run(&cm, id + 1, plain) as i32, 0, "another class {id:#x}");
        assert_eq!(
            run(&cm, id ^ 0x0001_0000, plain) as i32,
            0,
            "high half {id:#x}"
        );
        assert_eq!(
            run(&cm, id, plain | 1) as i32,
            0,
            "an array of the class {id:#x}"
        );
        // SAFETY: as in `run`; null is answered without a load.
        let null = unsafe { cm.try_call(&[0]) }.expect("call");
        // Cast: an `Int` result is the low 32 bits.
        assert_eq!(null as i32, 0, "null {id:#x}");
    }
}

/// The fixed-length sequence's rel8 jumps land where its layout says:
/// JZ and JMP on `done` (the end), JNE on the `XOR EAX, EAX`. The OSR
/// recomputation (`OsrRematInsn::ClassWordOf`) emits these same bytes.
#[test]
fn the_class_word_bytes_jump_to_their_documented_targets() {
    let b = class_word_of_bytes();
    let target = |at: usize| at + 2 + usize::from(b[at + 1]);
    assert_eq!((b[3], target(3)), (0x74, b.len()), "JZ done");
    assert_eq!((b[9], target(9)), (0x75, 16), "JNE zero");
    assert_eq!(&b[16..18], &[0x31, 0xC0], "zero: XOR EAX, EAX");
    assert_eq!((b[14], target(14)), (0xEB, b.len()), "JMP done");
    assert_eq!(&b[11..14], &[0x48, 0x63, 0x00], "MOVSXD RAX, DWORD [RAX]");
}
