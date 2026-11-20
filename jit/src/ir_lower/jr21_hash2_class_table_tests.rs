//! JIT round 21, lane hash2 (`CRATONVM_JIT_OBJECT_HASH_CLASS_TABLE`): the
//! identity arm's per-class verdict-byte test, executed over real
//! `ObjectHeader`s and a table laid out as the VM's
//! `IdentityHashClassBytes` lays it out.
use cratonvm_types::{ArrayElementType, ClassId, ObjectHeader, ObjectKind};

const CAP: usize = super::IDENTITY_HASH_CLASS_TABLE_CAPACITY;
const HEADER: usize = super::IDENTITY_HASH_CLASS_TABLE_HEADER;

fn table(verdicts: &[(u32, u8)]) -> Vec<u8> {
    let mut t = vec![0u8; HEADER + CAP];
    t[..8].copy_from_slice(&u64::try_from(CAP).unwrap().to_le_bytes());
    for &(cid, v) in verdicts {
        t[HEADER + usize::try_from(cid).unwrap()] = v;
    }
    t
}

/// The test bytes, EXECUTED: `true` on the fall-through, `false` on any
/// decline edge.
fn run(table: &[u8], obj: &ObjectHeader) -> bool {
    use crate::ExecutableBuffer;
    let cid_off = i32::try_from(std::mem::offset_of!(ObjectHeader, class_id)).unwrap();
    // Cast: the table's address, baked as the VM's would be.
    let base = table.as_ptr() as u64;
    let (bytes, decline) = super::identity_hash_class_table_test_bytes(base, cid_off);
    let arg0 = super::CALL_ARG_REGS[0];
    // MOV RAX, arg0 ; <test> ; MOV EAX, 1 ; RET ; decline: XOR EAX, EAX ; RET.
    let mut body = vec![
        0x48 | (u8::from(arg0 >= 8) << 2),
        0x89,
        0xC0 | ((arg0 & 7) << 3),
    ];
    let at = body.len();
    body.extend_from_slice(&bytes);
    body.extend_from_slice(&[0xB8, 1, 0, 0, 0, 0xC3]);
    let decline_at = body.len();
    body.extend_from_slice(&[0x31, 0xC0, 0xC3]);
    for d in decline {
        let operand = at + d;
        let rel = i32::try_from(decline_at).unwrap() - i32::try_from(operand + 4).unwrap();
        body[operand..operand + 4].copy_from_slice(&rel.to_le_bytes());
    }
    let mut buf = ExecutableBuffer::new(256).expect("executable buffer");
    buf.emit(&body);
    assert!(!buf.overflowed());
    buf.finalize();
    // SAFETY: `buf` is finalized and holds a complete
    // `extern "C" fn(u64) -> u64` that reads the header's class id and one
    // table byte inside `table` (the capacity compare guards the index),
    // writing only RAX, RCX and RDX, all caller-saved on both ABIs.
    let f =
        unsafe { std::mem::transmute::<*const u8, extern "C" fn(u64) -> u64>(buf.as_ptr()) };
    f(obj as *const ObjectHeader as u64) == 1
}

fn plain(cid: u32) -> Box<ObjectHeader> {
    Box::new(ObjectHeader::new(
        ClassId::new(cid),
        ObjectKind::Object,
        ArrayElementType::Reference,
        0,
        2,
    ))
}

/// Only a PROVEN byte selects the read; unknown, refused, past-capacity
/// and huge (lambda / sentinel band) ids decline without reading past
/// the table.
#[test]
#[cfg(target_arch = "x86_64")]
fn only_a_proven_class_byte_selects_the_read() {
    let last = u32::try_from(CAP - 1).unwrap();
    let t = table(&[
        (7, super::IDENTITY_HASH_CLASS_PROVEN),
        (9, super::IDENTITY_HASH_CLASS_REFUSED),
        (last, super::IDENTITY_HASH_CLASS_PROVEN),
    ]);
    assert!(run(&t, &plain(7)));
    assert!(run(&t, &plain(last)));
    assert!(!run(&t, &plain(9)), "refused class");
    assert!(!run(&t, &plain(11)), "undecided class");
    assert!(!run(&t, &plain(0)), "class id 0 has its own exact compare");
    assert!(!run(&t, &plain(last + 1)), "first id past the table");
    assert!(!run(&t, &plain(0x8000_0001)), "lambda-proxy band");
    assert!(!run(&t, &plain(u32::MAX)), "sentinel id");
}

