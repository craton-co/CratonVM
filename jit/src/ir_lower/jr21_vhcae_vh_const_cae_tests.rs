use super::{
    vh_const_core_bytes, vh_const_ref_return_proved, vh_const_ret_proof_caller_with,
    vh_const_ret_same_loader_enabled, vh_const_site_shape, VhConstCore, VhConstOp,
    VhConstRefFacts, VhRefGates, VhRefValueCheck, CALL_ARG_REGS,
};
use crate::direct_helpers::UnsafeRmwOp;
use std::cell::{Cell, UnsafeCell};

const COORD: u32 = 0x3579;
const CALLER: u32 = 0x357A;
const FIELD_TYPE: u32 = 0x357B;
const DISP: i32 = 16;
const DECLINED: i64 = -1;

fn facts(name: Option<&str>) -> VhConstRefFacts {
    VhConstRefFacts {
        unchecked: false,
        type_class_id: FIELD_TYPE,
        proving_name: name.map(str::to_owned),
    }
}

#[test]
fn a_typed_return_is_proven_only_by_the_coordinate_class_loader() {
    // javac types a null literal operand `Void` and the return as the cast.
    let cae = vh_const_site_shape("compareAndExchange", "(LN;Ljava/lang/Void;LN;)LN;")
        .expect("ref cae");
    assert_eq!(
        (cae.op, cae.kind, cae.ret),
        (VhConstOp::Rmw(UnsafeRmwOp::Cae), b'L', b'L')
    );
    let f = facts(Some("N"));
    assert!(
        vh_const_ref_return_proved(&cae, COORD, &f, COORD),
        "proven in the coordinate class (or by its loader)"
    );
    assert!(
        !vh_const_ref_return_proved(&cae, COORD, &f, CALLER),
        "another class without the loader proof"
    );
    assert!(!vh_const_ref_return_proved(&cae, COORD, &f, 0));
    assert!(
        !vh_const_ref_return_proved(&cae, COORD, &facts(None), COORD),
        "no proving name: an interface or a foreign field type"
    );
    assert!(
        !vh_const_ref_return_proved(&cae, COORD, &facts(Some("Other")), COORD),
        "a return naming another class"
    );
    let obj = vh_const_site_shape(
        "compareAndExchange",
        "(LN;Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
    )
    .expect("object cae");
    assert!(
        vh_const_ref_return_proved(&obj, COORD, &facts(None), CALLER),
        "an Object return needs no proof"
    );
    let cas = vh_const_site_shape("compareAndSet", "(LN;LN;LN;)Z").expect("ref cas");
    assert!(vh_const_ref_return_proved(
        &cas,
        COORD,
        &facts(None),
        CALLER
    ));
    let int_cae = vh_const_site_shape("compareAndExchange", "(LN;II)I").expect("int cae");
    assert_eq!(int_cae.op, VhConstOp::Rmw(UnsafeRmwOp::Cae));
    assert!(vh_const_ref_return_proved(&int_cae, COORD, &facts(None), 0));
    assert!(vh_const_site_shape("compareAndExchange", "(LN;JJ)J").is_some());
}

#[test]
fn the_return_proof_moves_to_the_coordinate_only_for_one_known_loader() {
    let loaders = |cid: u32| -> i64 {
        match cid {
            COORD | CALLER => 3,
            FIELD_TYPE => 1,
            _ => 0,
        }
    };
    let moved = if vh_const_ret_same_loader_enabled() {
        COORD
    } else {
        CALLER
    };
    assert_eq!(
        vh_const_ret_proof_caller_with(CALLER, COORD, &loaders),
        moved
    );
    assert_eq!(
        vh_const_ret_proof_caller_with(FIELD_TYPE, COORD, &loaders),
        FIELD_TYPE,
        "another loader"
    );
    assert_eq!(
        vh_const_ret_proof_caller_with(0x9999, COORD, &loaders),
        0x9999,
        "the caller's loader unknown"
    );
    assert_eq!(
        vh_const_ret_proof_caller_with(CALLER, 0x9999, &loaders),
        CALLER,
        "the coordinate's loader unknown"
    );
    assert_eq!(vh_const_ret_proof_caller_with(0, COORD, &loaders), 0);
    let never = |_: u32| -> i64 { 0 };
    assert_eq!(vh_const_ret_proof_caller_with(COORD, COORD, &never), COORD);
}

// ── the bytes, executed ──────────────────────────────────────────────

/// A fake compact object: class id at 0, plain kind, the field at `DISP`.
#[repr(C, align(16))]
struct Obj(UnsafeCell<[u8; 64]>);

impl Obj {
    fn new(class_id: u32) -> Box<Self> {
        let o = Box::new(Obj(UnsafeCell::new([0; 64])));
        o.bytes()[..4].copy_from_slice(&class_id.to_le_bytes());
        o.bytes()[cratonvm_types::GC_FLAGS_BYTE_OFFSET] |= cratonvm_types::GC_FLAG_COMPACT;
        o
    }
    #[allow(clippy::mut_from_ref)]
    fn bytes(&self) -> &mut [u8; 64] {
        // SAFETY: single-threaded test; no other reference to the bytes is
        // live while this one is used, and the emitted code runs only
        // between such uses.
        unsafe { &mut *self.0.get() }
    }
    fn make_old(&self) {
        self.bytes()[cratonvm_types::GC_FLAGS_BYTE_OFFSET] |= cratonvm_types::GC_FLAG_OLD_GEN;
    }
    fn addr(&self) -> u64 {
        self.0.get() as u64 // Cast: the object's address
    }
    fn field(&self) -> i64 {
        i64::from_le_bytes(self.bytes()[16..24].try_into().expect("8 bytes"))
    }
}

/// `MOV dst, src` (64-bit).
fn mov(dst: u8, src: u8) -> [u8; 3] {
    [
        0x48 | (u8::from(src >= 8) << 2) | u8::from(dst >= 8),
        0x89,
        0xC0 | ((src & 7) << 3) | (dst & 7),
    ]
}

/// The core as `extern "C" fn(_, receiver, expected, value) -> i64`,
/// `DECLINED` on a decline edge.
fn build(core: &VhConstCore) -> crate::ExecutableBuffer {
    let (bytes, decline) = vh_const_core_bytes(core);
    let mut body: Vec<u8> = Vec::new();
    // RAX = arg1, R10 = arg2, RDX = arg3 -- in an order that reads every
    // argument register before it is written on both ABIs.
    body.extend_from_slice(&mov(0, CALL_ARG_REGS[1]));
    body.extend_from_slice(&mov(10, CALL_ARG_REGS[2]));
    body.extend_from_slice(&mov(2, CALL_ARG_REGS[3]));
    let at = body.len();
    body.extend_from_slice(&bytes);
    body.push(0xC3); // RET: the witness in RAX
    let decline_at = body.len();
    body.extend_from_slice(&[0x48, 0xC7, 0xC0, 0xFF, 0xFF, 0xFF, 0xFF, 0xC3]);
    for d in decline {
        let operand = at + d;
        let rel = i32::try_from(decline_at).expect("small")
            - i32::try_from(operand + 4).expect("small");
        body[operand..operand + 4].copy_from_slice(&rel.to_le_bytes());
    }
    let mut buf = crate::ExecutableBuffer::new(1024).expect("executable buffer");
    buf.emit(&body);
    assert!(!buf.overflowed());
    buf.finalize();
    buf
}

fn run(buf: &crate::ExecutableBuffer, recv: u64, expected: i64, value: i64) -> i64 {
    // SAFETY: a complete `extern "C" fn(u64, u64, i64, i64) -> i64` that
    // reads only the fake objects and gate bytes the caller keeps alive and
    // writes RAX, RCX, RDX, R10, R11 (caller-saved on both ABIs) and the
    // fake object's field.
    let f = unsafe {
        std::mem::transmute::<*const u8, extern "C" fn(u64, u64, i64, i64) -> i64>(buf.as_ptr())
    };
    f(0, recv, expected, value)
}

