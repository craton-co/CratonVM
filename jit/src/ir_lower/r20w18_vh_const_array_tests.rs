use super::{
    vh_const_array_core, vh_const_array_core_bytes, vh_const_array_kind_from_word,
    vh_const_array_plan_word, vh_const_array_site_shape, VhConstArrayCore, VhConstOp,
    VhConstPlan, CALL_ARG_REGS,
};
use super::{vh_const_ref_array_get_bytes, VhConstRefArrayCore, VhRefValueCheck};
use crate::direct_helpers::{UnsafeBitOp, UnsafeRmwOp};


#[test]
fn the_array_plan_word_is_not_a_field_plan_and_round_trips() {
    for kind in *b"IJ" {
        let word = vh_const_array_plan_word(kind);
        assert!(word > 0, "bit 63 stays clear");
        assert_eq!(vh_const_array_kind_from_word(word), Some(kind));
        assert_eq!(
            VhConstPlan::from_word(word),
            None,
            "the field arm never reads it"
        );
    }
    // CHANGED JIT round 21 wave 1, lane updaters (UPDATERS21-5): an
    // `Object[]` handle has a plan word too (filed only under
    // `CRATONVM_JIT_VH_CONST_REF_ARRAY_GET`); every other kind still none.
    let ref_word = vh_const_array_plan_word(b'L');
    assert!(ref_word > 0);
    assert_eq!(vh_const_array_kind_from_word(ref_word), Some(b'L'));
    assert_eq!(VhConstPlan::from_word(ref_word), None);
    assert_eq!(vh_const_array_plan_word(b'Z'), 0);
    assert_eq!(vh_const_array_kind_from_word(0), None);
    let field = VhConstPlan {
        receiver_class_id: 7,
        slot: 1,
        kind: b'I',
        read_only: false,
    };
    assert_eq!(vh_const_array_kind_from_word(field.to_word()), None);
    let shape = vh_const_array_site_shape("get", "([JI)J").expect("shape");
    assert!(
        vh_const_array_core(&shape, b'I').is_none(),
        "a long site on an int[] handle"
    );
    assert!(vh_const_array_core(&shape, b'J').is_some());
}

// ── the bytes, executed ──────────────────────────────────────────────

const DECLINED: i64 = -1;

/// A fake array: the kind/element byte, a 32-bit length at 8, the payload
/// at 16. Behind an `UnsafeCell`, so the emitted code may write it.
#[repr(C, align(16))]
struct Arr(std::cell::UnsafeCell<[u8; 96]>);

impl Arr {
    fn new(descriptor: &str, length: u32) -> Box<Self> {
        let a = Box::new(Arr(std::cell::UnsafeCell::new([0; 96])));
        let tag = cratonvm_types::primitive_array_kind_tags_byte(descriptor).expect("tag");
        a.bytes()[cratonvm_types::KIND_TAGS_BYTE_OFFSET] = tag;
        let len = cratonvm_types::ARRAY_LENGTH_OFFSET;
        a.bytes()[len..len + 4].copy_from_slice(&length.to_le_bytes());
        a
    }
    #[allow(clippy::mut_from_ref)]
    fn bytes(&self) -> &mut [u8; 96] {
        // SAFETY: single-threaded test; no other reference to the bytes is
        // live while this one is used, and the emitted code runs only
        // between such uses.
        unsafe { &mut *self.0.get() }
    }
    fn addr(&self) -> u64 {
        self.0.get() as u64 // Cast: the array's address
    }
    fn int(&self, i: usize) -> i32 {
        let at = 16 + 4 * i;
        i32::from_le_bytes(self.bytes()[at..at + 4].try_into().expect("4 bytes"))
    }
    fn long(&self, i: usize) -> i64 {
        let at = 16 + 8 * i;
        i64::from_le_bytes(self.bytes()[at..at + 8].try_into().expect("8 bytes"))
    }
    fn set_int(&self, i: usize, v: i32) {
        let at = 16 + 4 * i;
        self.bytes()[at..at + 4].copy_from_slice(&v.to_le_bytes());
    }
    fn set_long(&self, i: usize, v: i64) {
        let at = 16 + 8 * i;
        self.bytes()[at..at + 8].copy_from_slice(&v.to_le_bytes());
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

/// The core as `extern "C" fn(array, index, expected, value) -> i64`,
/// `DECLINED` on a decline edge.
fn build(op: VhConstOp, kind: u8) -> crate::ExecutableBuffer {
    let (bytes, decline) = vh_const_array_core_bytes(&VhConstArrayCore { op, kind });
    let mut body: Vec<u8> = Vec::new();
    // RAX = arg0, R11 = arg1, R10 = arg2, RDX = arg3 -- an order that
    // reads every argument register before it is written on both ABIs.
    body.extend_from_slice(&mov(0, CALL_ARG_REGS[0]));
    body.extend_from_slice(&mov(11, CALL_ARG_REGS[1]));
    body.extend_from_slice(&mov(10, CALL_ARG_REGS[2]));
    body.extend_from_slice(&mov(2, CALL_ARG_REGS[3]));
    let at = body.len();
    body.extend_from_slice(&bytes);
    body.push(0xC3); // RET: the result in RAX
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

fn run(
    buf: &crate::ExecutableBuffer,
    array: u64,
    index: i64,
    expected: i64,
    value: i64,
) -> i64 {
    // SAFETY: a complete `extern "C" fn(u64, i64, i64, i64) -> i64` that
    // reads and writes only the fake array the caller keeps alive and
    // writes RAX, RCX, RDX, R10, R11 (caller-saved on both ABIs).
    let f = unsafe {
        std::mem::transmute::<*const u8, extern "C" fn(u64, i64, i64, i64) -> i64>(buf.as_ptr())
    };
    f(array, index, expected, value)
}

/// UPDATERS21-5's bytes as `extern "C" fn(array, index, _, _) -> i64`.
fn build_ref(core: &VhConstRefArrayCore) -> crate::ExecutableBuffer {
    let (bytes, decline) = vh_const_ref_array_get_bytes(core);
    let mut body: Vec<u8> = Vec::new();
    body.extend_from_slice(&mov(0, CALL_ARG_REGS[0]));
    body.extend_from_slice(&mov(11, CALL_ARG_REGS[1]));
    let at = body.len();
    body.extend_from_slice(&bytes);
    body.push(0xC3); // RET: the element in RAX
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

/// JIT round 21 wave 1, lane updaters (UPDATERS21-5): a reference-array
/// GET reads the element of a reference array in bounds and declines a
/// null, a primitive array and an out-of-range or negative index; with a
/// cast it also declines an element of another class and passes null.
#[test]
fn r21_updaters_reference_array_get_hits_in_bounds_and_screens_the_cast() {
    const REF_TAG: u8 = (cratonvm_types::ObjectKind::Array as u8)
        | ((cratonvm_types::ArrayElementType::Reference as u8) << 2);
    // A fake "element object": class id 0x4321 at 0, a plain-object kind byte.
    #[repr(C, align(16))]
    struct Elem(std::cell::UnsafeCell<[u8; 32]>);
    let elem = Box::new(Elem(std::cell::UnsafeCell::new([0; 32])));
    // SAFETY: single-threaded test; no other reference to the bytes is live.
    unsafe { (&mut *elem.0.get())[..4].copy_from_slice(&0x4321u32.to_le_bytes()) };
    let elem_addr = elem.0.get() as u64; // Cast: the fake object's address
    let a = Arr::new("[J", 4);
    a.bytes()[cratonvm_types::KIND_TAGS_BYTE_OFFSET] = REF_TAG;
    // Cast: the fake element's address as the slot's raw pointer word.
    a.set_long(2, elem_addr as i64);
    let plain = build_ref(&VhConstRefArrayCore {
        result_check: VhRefValueCheck::Admitted,
        cid_off: 0,
    });
    assert_eq!(run(&plain, a.addr(), 2, 0, 0), elem_addr as i64);
    assert_eq!(run(&plain, a.addr(), 1, 0, 0), 0, "a null element");
    assert_eq!(run(&plain, a.addr(), 4, 0, 0), DECLINED, "past the end");
    assert_eq!(run(&plain, a.addr(), -1, 0, 0), DECLINED, "negative");
    assert_eq!(run(&plain, 0, 0, 0, 0), DECLINED, "a null array");
    let prim = Arr::new("[J", 4);
    assert_eq!(run(&plain, prim.addr(), 0, 0, 0), DECLINED, "a long[]");
    let cast_ok = build_ref(&VhConstRefArrayCore {
        result_check: VhRefValueCheck::ExactClass(0x4321),
        cid_off: 0,
    });
    assert_eq!(run(&cast_ok, a.addr(), 2, 0, 0), elem_addr as i64);
    assert_eq!(run(&cast_ok, a.addr(), 1, 0, 0), 0, "null passes the cast");
    let cast_bad = build_ref(&VhConstRefArrayCore {
        result_check: VhRefValueCheck::ExactClass(0x7777),
        cid_off: 0,
    });
    assert_eq!(run(&cast_bad, a.addr(), 2, 0, 0), DECLINED, "the call's CCE");
}

#[test]
fn int_array_accesses_hit_in_bounds_and_decline_everything_else() {
    let a = Arr::new("[I", 4);
    a.set_int(3, -5);
    let get = build(VhConstOp::Get, b'I');
    assert_eq!(run(&get, a.addr(), 3, 0, 0), -5, "sign-extended");
    assert_eq!(
        run(&get, 0, 0, 0, 0),
        DECLINED,
        "null array: the call's NPE"
    );
    assert_eq!(run(&get, a.addr(), 4, 0, 0), DECLINED, "index == length");
    assert_eq!(run(&get, a.addr(), -1, 0, 0), DECLINED, "a negative index");
    assert_eq!(run(&get, a.addr(), i64::from(i32::MIN), 0, 0), DECLINED);
    // Only the low 32 bits are the index: garbage above them is ignored.
    assert_eq!(run(&get, a.addr(), (7_i64 << 32) | 3, 0, 0), -5);
    for other in ["[J", "[B", "[S", "[C", "[Z", "[F", "[D"] {
        let o = Arr::new(other, 4);
        assert_eq!(run(&get, o.addr(), 0, 0, 0), DECLINED, "{other}");
    }
    let empty = Arr::new("[I", 0);
    assert_eq!(run(&get, empty.addr(), 0, 0, 0), DECLINED, "an empty array");

    let set = build(VhConstOp::Set { volatile: false }, b'I');
    run(&set, a.addr(), 0, 0, 77);
    assert_eq!(a.int(0), 77);
    assert_eq!(a.int(1), 0, "the neighbour is untouched");
    assert_eq!(run(&set, a.addr(), 4, 0, 1), DECLINED);
    let set_v = build(VhConstOp::Set { volatile: true }, b'I');
    run(&set_v, a.addr(), 1, 0, -9);
    assert_eq!(a.int(1), -9);

    let cas = build(VhConstOp::Rmw(UnsafeRmwOp::Cas), b'I');
    assert_eq!(run(&cas, a.addr(), 1, -9, 10), 1);
    assert_eq!(a.int(1), 10);
    assert_eq!(run(&cas, a.addr(), 1, -9, 11), 0, "a failed CAS");
    assert_eq!(a.int(1), 10);
    assert_eq!(run(&cas, a.addr(), 9, 10, 11), DECLINED);
    let cae = build(VhConstOp::Rmw(UnsafeRmwOp::Cae), b'I');
    assert_eq!(run(&cae, a.addr(), 1, 3, 4), 10, "the witness");
    let xadd = build(VhConstOp::Rmw(UnsafeRmwOp::GetAndAdd), b'I');
    assert_eq!(run(&xadd, a.addr(), 2, 0, 5), 0);
    assert_eq!(run(&xadd, a.addr(), 2, 0, -7), 5, "the previous value");
    assert_eq!(a.int(2), -2);
    let xchg = build(VhConstOp::Rmw(UnsafeRmwOp::GetAndSet), b'I');
    assert_eq!(run(&xchg, a.addr(), 2, 0, 42), -2);
    assert_eq!(a.int(2), 42);
    let xor = build(
        VhConstOp::Rmw(UnsafeRmwOp::GetAndBitwise(UnsafeBitOp::Xor)),
        b'I',
    );
    assert_eq!(run(&xor, a.addr(), 2, 0, 3), 42);
    assert_eq!(a.int(2), 42 ^ 3);
    assert_eq!(a.int(3), -5, "the last element is untouched");
}

#[test]
fn long_array_accesses_scale_by_eight() {
    let a = Arr::new("[J", 4);
    a.set_long(3, 1 << 40);
    let get = build(VhConstOp::Get, b'J');
    assert_eq!(run(&get, a.addr(), 3, 0, 0), 1 << 40);
    assert_eq!(run(&get, a.addr(), 4, 0, 0), DECLINED);
    let ints = Arr::new("[I", 8);
    assert_eq!(
        run(&get, ints.addr(), 0, 0, 0),
        DECLINED,
        "an int[] under a long[] handle"
    );
    let set_v = build(VhConstOp::Set { volatile: true }, b'J');
    run(&set_v, a.addr(), 0, 0, -1);
    assert_eq!(a.long(0), -1);
    assert_eq!(a.long(1), 0);
    let cas = build(VhConstOp::Rmw(UnsafeRmwOp::Cas), b'J');
    assert_eq!(run(&cas, a.addr(), 3, 1 << 40, 7), 1);
    assert_eq!(a.long(3), 7);
    assert_eq!(run(&cas, a.addr(), 3, 1 << 40, 8), 0);
    let xadd = build(VhConstOp::Rmw(UnsafeRmwOp::GetAndAdd), b'J');
    assert_eq!(run(&xadd, a.addr(), 1, 0, 1 << 33), 0);
    assert_eq!(a.long(1), 1 << 33);
    let cae = build(VhConstOp::Rmw(UnsafeRmwOp::Cae), b'J');
    assert_eq!(
        run(&cae, a.addr(), 1, 0, 9),
        1 << 33,
        "the witness of a failed exchange"
    );
    assert_eq!(a.long(1), 1 << 33);
    assert_eq!(a.long(0), -1, "the neighbour is untouched");
}


/// JIT round 21 wave 22, lane vhconst (UPDATERS21-5): the two halves of the
/// reference-array store arm, EXECUTED with the tier's reload between them
/// (the operands baked as immediates): `fn() -> i64`, `DECLINED` on any
/// decline edge.
fn build_ref_store(
    core: &super::VhConstRefArrayStoreCore,
    array: u64,
    index: i64,
    expected: i64,
    value: i64,
) -> crate::ExecutableBuffer {
    let (screen, screen_decline) = super::vh_const_ref_array_store_screen_bytes(core);
    let (access, access_decline) = super::vh_const_ref_array_store_access_bytes(core);
    let mut body: Vec<u8> = Vec::new();
    let imm = |body: &mut Vec<u8>, op: [u8; 2], v: u64| {
        body.extend_from_slice(&op);
        body.extend_from_slice(&v.to_le_bytes());
    };
    // Casts: two's-complement operand words.
    imm(&mut body, [0x48, 0xB8], array); // MOV RAX, imm64
    imm(&mut body, [0x49, 0xBA], expected as u64); // MOV R10, imm64
    imm(&mut body, [0x48, 0xBA], value as u64); // MOV RDX, imm64
    let screen_at = body.len();
    body.extend_from_slice(&screen);
    imm(&mut body, [0x49, 0xBB], index as u64); // MOV R11, imm64
    imm(&mut body, [0x49, 0xBA], expected as u64); // MOV R10, imm64
    imm(&mut body, [0x48, 0xBA], value as u64); // MOV RDX, imm64
    let access_at = body.len();
    body.extend_from_slice(&access);
    body.push(0xC3); // RET: the result in RAX
    let decline_at = body.len();
    body.extend_from_slice(&[0x48, 0xC7, 0xC0, 0xFF, 0xFF, 0xFF, 0xFF, 0xC3]);
    let edges = screen_decline
        .into_iter()
        .map(|d| screen_at + d)
        .chain(access_decline.into_iter().map(|d| access_at + d));
    for operand in edges {
        let rel = i32::try_from(decline_at).expect("small")
            - i32::try_from(operand + 4).expect("small");
        body[operand..operand + 4].copy_from_slice(&rel.to_le_bytes());
    }
    let mut buf = crate::ExecutableBuffer::new(2048).expect("executable buffer");
    buf.emit(&body);
    assert!(!buf.overflowed());
    buf.finalize();
    buf
}

fn run_ref_store(
    core: &super::VhConstRefArrayStoreCore,
    array: u64,
    index: i64,
    expected: i64,
    value: i64,
) -> i64 {
    let buf = build_ref_store(core, array, index, expected, value);
    // SAFETY: a complete `extern "C" fn() -> i64` that reads the gate
    // bytes and the fake objects the caller keeps alive, writes only the
    // fake array's slots, and writes RAX, RCX, RDX, R10, R11
    // (caller-saved on both ABIs).
    let f = unsafe { std::mem::transmute::<*const u8, extern "C" fn() -> i64>(buf.as_ptr()) };
    f()
}

/// JIT round 21 wave 22, lane vhconst (UPDATERS21-5,
/// `CRATONVM_JIT_VH_CONST_REF_ARRAY_STORE`): a reference-array set / CAS /
/// compareAndExchange / getAndSet stores only what the JDK body would
/// store without throwing and without a barrier -- a null value, any value
/// into an `Object[]` (component id 0), a plain value of exactly the
/// component -- and declines (nothing written) a value of another class
/// (the call's ArrayStoreException), an array value into a typed array, a
/// null or primitive array, an index out of range, an old array while
/// old objects exist, and a running mark.
#[test]
fn r21_vhconst_reference_array_store_checks_covariance_and_gates_before_writing() {
    const REF_TAG: u8 = (cratonvm_types::ObjectKind::Array as u8)
        | ((cratonvm_types::ArrayElementType::Reference as u8) << 2);
    const COMP: u32 = 0x4321;
    #[repr(C, align(16))]
    struct Elem(std::cell::UnsafeCell<[u8; 32]>);
    let mk_elem = |cid: u32, kind_tags: u8| {
        let e = Box::new(Elem(std::cell::UnsafeCell::new([0; 32])));
        // SAFETY: single-threaded test; no other reference to the bytes is live.
        let b = unsafe { &mut *e.0.get() };
        b[..4].copy_from_slice(&cid.to_le_bytes());
        b[cratonvm_types::KIND_TAGS_BYTE_OFFSET] = kind_tags;
        e
    };
    let addr = |e: &Elem| e.0.get() as i64; // Cast: the fake object's address
    let good = mk_elem(COMP, 0);
    let other = mk_elem(0x7777, 0);
    let arr_value = mk_elem(COMP, REF_TAG); // a `COMP[]`, whose header names COMP
    let ref_array = |component: u32| {
        let a = Arr::new("[J", 4);
        a.bytes()[cratonvm_types::KIND_TAGS_BYTE_OFFSET] = REF_TAG;
        a.bytes()[..4].copy_from_slice(&component.to_le_bytes());
        a
    };
    let pre = Box::new(std::cell::Cell::new(0u8));
    let post = Box::new(std::cell::Cell::new(1u8));
    let core = |op: VhConstOp| super::VhConstRefArrayStoreCore {
        op,
        gates: super::VhRefGates {
            pre: pre.as_ptr() as usize,   // Cast: the gate byte's address
            post: post.as_ptr() as usize, // Cast: as above
            floor: 0,
            mask: Some(0x10),
        },
        null_old_expected: false,
        display_cell: None,
        cid_off: 0,
    };
    let set = core(VhConstOp::Set { volatile: false });
    let set_v = core(VhConstOp::Set { volatile: true });
    // An Object[]: every value, an array value included.
    let objs = ref_array(0);
    run_ref_store(&set, objs.addr(), 1, 0, addr(&other));
    assert_eq!(objs.long(1), addr(&other));
    run_ref_store(&set_v, objs.addr(), 2, 0, addr(&arr_value));
    assert_eq!(objs.long(2), addr(&arr_value));
    // A typed array: exactly the component, or null.
    let typed = ref_array(COMP);
    run_ref_store(&set, typed.addr(), 0, 0, addr(&good));
    assert_eq!(typed.long(0), addr(&good));
    assert_eq!(run_ref_store(&set, typed.addr(), 0, 0, addr(&other)), DECLINED, "the ASE");
    assert_eq!(typed.long(0), addr(&good), "nothing written");
    assert_eq!(
        run_ref_store(&set, typed.addr(), 1, 0, addr(&arr_value)),
        DECLINED,
        "an array value into a typed array"
    );
    run_ref_store(&set, typed.addr(), 0, 0, 0);
    assert_eq!(typed.long(0), 0, "a null value");
    // The array and the index.
    assert_eq!(run_ref_store(&set, 0, 0, 0, 0), DECLINED, "the NPE");
    let prim = Arr::new("[J", 4);
    assert_eq!(run_ref_store(&set, prim.addr(), 0, 0, 0), DECLINED, "a long[]");
    assert_eq!(run_ref_store(&set, typed.addr(), 4, 0, addr(&good)), DECLINED, "AIOOBE");
    assert_eq!(run_ref_store(&set, typed.addr(), -1, 0, addr(&good)), DECLINED, "negative");
    assert_eq!(typed.long(3), 0);
    // The gates: an old array while old objects exist owes a card ...
    let old = ref_array(COMP);
    old.bytes()[cratonvm_types::GC_FLAGS_BYTE_OFFSET] |= 0x10;
    assert_eq!(run_ref_store(&set, old.addr(), 0, 0, addr(&good)), DECLINED);
    assert_eq!(old.long(0), 0);
    run_ref_store(&set, old.addr(), 0, 0, 0);
    post.set(0);
    run_ref_store(&set, old.addr(), 0, 0, addr(&good));
    assert_eq!(old.long(0), addr(&good), "no old objects: no card");
    post.set(1);
    // ... and a running mark owes the SATB record.
    pre.set(1);
    assert_eq!(run_ref_store(&set, typed.addr(), 1, 0, addr(&good)), DECLINED);
    assert_eq!(typed.long(1), 0);
    pre.set(0);
    // CAS / compareAndExchange / getAndSet.
    let cas = core(VhConstOp::Rmw(UnsafeRmwOp::Cas));
    assert_eq!(run_ref_store(&cas, typed.addr(), 1, 0, addr(&good)), 1);
    assert_eq!(typed.long(1), addr(&good));
    assert_eq!(run_ref_store(&cas, typed.addr(), 1, 0, addr(&good)), 0, "holds good");
    assert_eq!(run_ref_store(&cas, typed.addr(), 1, addr(&good), addr(&other)), DECLINED);
    assert_eq!(typed.long(1), addr(&good));
    let cae = core(VhConstOp::Rmw(UnsafeRmwOp::Cae));
    assert_eq!(
        run_ref_store(&cae, typed.addr(), 1, 0, 0),
        addr(&good),
        "a failed exchange's witness"
    );
    assert_eq!(run_ref_store(&cae, typed.addr(), 1, addr(&good), 0), addr(&good));
    assert_eq!(typed.long(1), 0, "exchanged");
    let gas = core(VhConstOp::Rmw(UnsafeRmwOp::GetAndSet));
    assert_eq!(run_ref_store(&gas, typed.addr(), 2, 0, addr(&good)), 0);
    assert_eq!(run_ref_store(&gas, typed.addr(), 2, 0, 0), addr(&good));
    assert_eq!(typed.long(2), 0);
    // The null-old proof: while marking, a CAS whose `expected` is null.
    let mut cas_null_old = core(VhConstOp::Rmw(UnsafeRmwOp::Cas));
    cas_null_old.null_old_expected = true;
    pre.set(1);
    assert_eq!(run_ref_store(&cas_null_old, typed.addr(), 3, 0, addr(&good)), 1);
    assert_eq!(
        run_ref_store(&cas_null_old, typed.addr(), 3, addr(&good), 0),
        DECLINED,
        "a non-null old value while marking"
    );
    pre.set(0);
    assert_eq!(typed.long(3), addr(&good));
}
