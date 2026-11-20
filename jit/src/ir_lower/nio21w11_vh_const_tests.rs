use super::{
    vh_const_core, vh_const_core_bytes, vh_const_site_shape, VhConstCore, VhConstOp,
    VhConstPlan, VhRefGates, VhRefValueCheck, VhSubtypeDisplay, CALL_ARG_REGS,
};
// JIT r20 wave 11 (lane pAlloc): the typecheck display arm's bytes.
use super::typecheck_display_hit_bytes;
// JIT r20 wave 17 (lane pAlloc2): its hit-or-miss twin.
use super::typecheck_display_answer_bytes;
use crate::direct_helpers::{UnsafeBitOp, UnsafeRmwOp};

#[test]
fn the_site_recogniser_takes_exact_kinds_only() {
    let s = vh_const_site_shape("get", "(LFoo;)I").expect("int get");
    assert_eq!((s.op, s.kind, s.ret), (VhConstOp::Get, b'I', b'I'));
    let s = vh_const_site_shape("getAcquire", "(LFoo;)Ljava/lang/Object;").expect("ref get");
    assert_eq!((s.kind, s.ret), (b'L', b'L'));
    let s = vh_const_site_shape("setVolatile", "(LFoo;J)V").expect("long set");
    assert_eq!(s.op, VhConstOp::Set { volatile: true });
    let s = vh_const_site_shape("setRelease", "(LFoo;LBar;)V").expect("ref set");
    assert_eq!((s.op, s.kind), (VhConstOp::Set { volatile: false }, b'L'));
    let s = vh_const_site_shape("compareAndSet", "(LFoo;II)Z").expect("int cas");
    assert_eq!(s.op, VhConstOp::Rmw(UnsafeRmwOp::Cas));
    assert!(s.has_expected());
    let s = vh_const_site_shape(
        "weakCompareAndSetRelease",
        "(LFoo;Ljava/lang/Object;LBar;)Z",
    )
    .expect("ref weak cas");
    assert_eq!((s.op, s.kind), (VhConstOp::Rmw(UnsafeRmwOp::Cas), b'L'));
    let s = vh_const_site_shape("compareAndExchange", "(LFoo;JJ)J").expect("long cae");
    assert_eq!(s.op, VhConstOp::Rmw(UnsafeRmwOp::Cae));
    let s = vh_const_site_shape("getAndAddRelease", "(LFoo;I)I").expect("int xadd");
    assert_eq!(s.op, VhConstOp::Rmw(UnsafeRmwOp::GetAndAdd));
    let s = vh_const_site_shape("getAndBitwiseXor", "(LFoo;J)J").expect("long xor");
    assert_eq!(
        s.op,
        VhConstOp::Rmw(UnsafeRmwOp::GetAndBitwise(UnsafeBitOp::Xor))
    );
    let s = vh_const_site_shape("getAndSet", "(LFoo;LBar;)LBar;").expect("ref xchg");
    assert_eq!(
        (s.op, s.kind, s.ret),
        (VhConstOp::Rmw(UnsafeRmwOp::GetAndSet), b'L', b'L')
    );
    for (m, d) in [
        ("get", "(LFoo;)F"),                                          // a float
        ("get", "(I)I"),                                              // no reference coordinate
        ("get", "([II)I"),                                            // an array-element shape
        ("get", "()I"),                                               // a static-field shape
        ("set", "(LFoo;Ljava/lang/Integer;I)V"),                      // two coordinates
        ("compareAndSet", "(LFoo;IJ)Z"),                              // mixed kinds
        ("compareAndSet", "(LFoo;II)I"),                              // wrong return
        ("getAndAdd", "(LFoo;Ljava/lang/Object;)Ljava/lang/Object;"), // numeric on a ref
        ("getAndBitwiseOr", "(LFoo;LBar;)LBar;"),
        ("getAndAdd", "(LFoo;I)J"), // a widening the handle would apply
        ("set", "(LFoo;I)I"),
        ("toMethodHandle", "(LFoo;)I"),
    ] {
        assert!(vh_const_site_shape(m, d).is_none(), "{m}{d}");
    }
    assert!(vh_const_site_shape("get", "(LFoo;)J").is_some());
}

#[test]
fn the_plan_word_round_trips_and_refuses_what_it_did_not_write() {
    for plan in [
        VhConstPlan {
            receiver_class_id: 1,
            slot: 0,
            kind: b'I',
            read_only: false,
        },
        VhConstPlan {
            receiver_class_id: u32::MAX,
            slot: u16::MAX,
            kind: b'L',
            read_only: true,
        },
        VhConstPlan {
            receiver_class_id: 0x4242,
            slot: 9,
            kind: b'J',
            read_only: false,
        },
    ] {
        let word = plan.to_word();
        assert!(word > 0, "bit 63 stays clear");
        assert_eq!(VhConstPlan::from_word(word), Some(plan));
    }
    assert_eq!(VhConstPlan::from_word(0), None);
    let bad_kind = VhConstPlan {
        receiver_class_id: 1,
        slot: 0,
        kind: b'F',
        read_only: false,
    };
    assert_eq!(VhConstPlan::from_word(bad_kind.to_word()), None);
    assert_eq!(
        VhConstPlan::from_word(1 | (i64::from(b'I') << 48) | (4 << 56)),
        None
    );
}

/// The refusals decided before any layout is consulted.
#[test]
fn a_read_only_handle_and_a_kind_mismatch_keep_the_call() {
    let ro = VhConstPlan {
        receiver_class_id: 3,
        slot: 1,
        kind: b'I',
        read_only: true,
    };
    let set = vh_const_site_shape("set", "(LFoo;I)V").expect("shape");
    assert!(
        vh_const_core(&set, ro, None, 3, None, None, 0, &|_| None).is_none(),
        "final field"
    );
    let rw = VhConstPlan {
        read_only: false,
        ..ro
    };
    let long_get = vh_const_site_shape("get", "(LFoo;)J").expect("shape");
    assert!(
        vh_const_core(&long_get, rw, None, 3, None, None, 0, &|_| None).is_none(),
        "J site, I field"
    );
}

// ── the bytes, executed ──────────────────────────────────────────────

const CLASS: u32 = 0x1234;
const OTHER: u32 = 0x4321;
const DISP: i32 = 16;
const DECLINED: i64 = -1;

/// A fake object: class id at 0, a plain-object kind byte, the gc-flags
/// byte, and room for a field at `DISP` (16). Behind an `UnsafeCell`, so
/// the emitted code may write the bytes the test then reads.
#[repr(C, align(16))]
struct Obj(std::cell::UnsafeCell<[u8; 64]>);

impl Obj {
    fn new(class_id: u32, compact: bool) -> Box<Self> {
        let o = Box::new(Obj(std::cell::UnsafeCell::new([0; 64])));
        o.bytes()[..4].copy_from_slice(&class_id.to_le_bytes());
        if compact {
            o.or_byte(
                cratonvm_types::GC_FLAGS_BYTE_OFFSET,
                cratonvm_types::GC_FLAG_COMPACT,
            );
        }
        o
    }
    #[allow(clippy::mut_from_ref)]
    fn bytes(&self) -> &mut [u8; 64] {
        // SAFETY: single-threaded test; no other reference to the bytes is
        // live while this one is used, and the emitted code runs only
        // between such uses.
        unsafe { &mut *self.0.get() }
    }
    fn or_byte(&self, at: usize, bits: u8) {
        self.bytes()[at] |= bits;
    }
    fn addr(&self) -> u64 {
        self.0.get() as u64 // Cast: the object's address
    }
    fn int(&self) -> i32 {
        i32::from_le_bytes(self.bytes()[16..20].try_into().expect("4 bytes"))
    }
    fn long(&self) -> i64 {
        i64::from_le_bytes(self.bytes()[16..24].try_into().expect("8 bytes"))
    }
    fn set_int(&self, v: i32) {
        self.bytes()[16..20].copy_from_slice(&v.to_le_bytes());
    }
    fn set_long(&self, v: i64) {
        self.bytes()[16..24].copy_from_slice(&v.to_le_bytes());
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

fn mk_core(op: VhConstOp, kind: u8) -> VhConstCore {
    VhConstCore {
        op,
        kind,
        disp: DISP,
        class_id: CLASS,
        receiver_display: None,
        cid_off: 0,
        expected_check: VhRefValueCheck::Admitted,
        value_check: VhRefValueCheck::Admitted,
        gates: None,
        g1: None,
        result_check: VhRefValueCheck::Admitted,
    }
}

#[test]
fn int_and_long_accesses_hit_and_every_other_receiver_declines() {
    let o = Obj::new(CLASS, true);
    o.set_int(-5);
    let get = build(&mk_core(VhConstOp::Get, b'I'));
    assert_eq!(run(&get, o.addr(), 0, 0), -5, "sign-extended");
    assert_eq!(
        run(&get, 0, 0, 0),
        DECLINED,
        "null receiver: the call's NPE"
    );
    let other = Obj::new(OTHER, true);
    assert_eq!(run(&get, other.addr(), 0, 0), DECLINED, "another class");
    let legacy = Obj::new(CLASS, false);
    assert_eq!(
        run(&get, legacy.addr(), 0, 0),
        DECLINED,
        "a legacy instance"
    );
    let array = Obj::new(CLASS, true);
    array.or_byte(cratonvm_types::KIND_TAGS_BYTE_OFFSET, 1);
    assert_eq!(run(&get, array.addr(), 0, 0), DECLINED, "an array header");

    let set = build(&mk_core(VhConstOp::Set { volatile: false }, b'I'));
    run(&set, o.addr(), 0, 77);
    assert_eq!(o.int(), 77);
    let set_v = build(&mk_core(VhConstOp::Set { volatile: true }, b'I'));
    run(&set_v, o.addr(), 0, -9);
    assert_eq!(o.int(), -9);

    let cas = build(&mk_core(VhConstOp::Rmw(UnsafeRmwOp::Cas), b'I'));
    assert_eq!(run(&cas, o.addr(), -9, 10), 1);
    assert_eq!(o.int(), 10);
    assert_eq!(run(&cas, o.addr(), -9, 11), 0, "a failed CAS");
    assert_eq!(o.int(), 10);
    let cae = build(&mk_core(VhConstOp::Rmw(UnsafeRmwOp::Cae), b'I'));
    assert_eq!(run(&cae, o.addr(), 3, 4), 10, "the witness");

    let l = Obj::new(CLASS, true);
    l.set_long(1 << 40);
    let xadd = build(&mk_core(VhConstOp::Rmw(UnsafeRmwOp::GetAndAdd), b'J'));
    assert_eq!(run(&xadd, l.addr(), 0, 5), 1 << 40, "the previous value");
    assert_eq!(l.long(), (1 << 40) + 5);
    let get_l = build(&mk_core(VhConstOp::Get, b'J'));
    assert_eq!(run(&get_l, l.addr(), 0, 0), (1 << 40) + 5);
    let or = build(&mk_core(
        VhConstOp::Rmw(UnsafeRmwOp::GetAndBitwise(UnsafeBitOp::Or)),
        b'J',
    ));
    assert_eq!(run(&or, l.addr(), 0, 2), (1 << 40) + 5);
    assert_eq!(l.long(), (1 << 40) + 7);
    let xchg = build(&mk_core(VhConstOp::Rmw(UnsafeRmwOp::GetAndSet), b'J'));
    assert_eq!(run(&xchg, l.addr(), 0, -1), (1 << 40) + 7);
    assert_eq!(l.long(), -1);
}

/// JIT round 21 wave 1, lane updaters (`CRATONVM_JIT_VH_CONST_CAST_RETURN`):
/// a reference `Get` with the site's cast as its result screen returns a
/// null or an instance of the class and declines on anything else (the
/// call throws the ClassCastException); an array return admits only null.
#[test]
fn r21_updaters_a_cast_return_screens_the_loaded_reference() {
    let holder = Obj::new(CLASS, true);
    let value = Obj::new(OTHER, true);
    let wrong = Obj::new(0x77, true);
    let mut c = mk_core(VhConstOp::Get, b'L');
    c.result_check = VhRefValueCheck::ExactClass(OTHER);
    let get = build(&c);
    // Cast: the fake object's address as the field's raw pointer word.
    holder.set_long(value.addr() as i64);
    assert_eq!(run(&get, holder.addr(), 0, 0), value.addr() as i64, "an instance");
    holder.set_long(0);
    assert_eq!(run(&get, holder.addr(), 0, 0), 0, "null passes every cast");
    holder.set_long(wrong.addr() as i64);
    assert_eq!(run(&get, holder.addr(), 0, 0), DECLINED, "another class: the call's CCE");
    let mut n = mk_core(VhConstOp::Get, b'L');
    n.result_check = VhRefValueCheck::NullOnly;
    let get_n = build(&n);
    assert_eq!(run(&get_n, holder.addr(), 0, 0), DECLINED, "an array cast: non-null declines");
    holder.set_long(0);
    assert_eq!(run(&get_n, holder.addr(), 0, 0), 0);
    // Without a screen the load is unchanged.
    holder.set_long(wrong.addr() as i64);
    let plain = build(&mk_core(VhConstOp::Get, b'L'));
    assert_eq!(run(&plain, holder.addr(), 0, 0), wrong.addr() as i64);
}

/// JIT round 21 wave 1, lane updaters (UPDATERS21-6,
/// `CRATONVM_JIT_VH_CONST_CAST_RMW`): a reference getAndSet with a cast
/// screens the OLD value before it writes -- an instance of the class or
/// null is swapped and returned; anything else declines with the field
/// unchanged (the call swaps, then throws).
#[test]
fn r21_updaters_a_cast_get_and_set_decides_the_cast_before_the_swap() {
    let holder = Obj::new(CLASS, true);
    let old = Obj::new(OTHER, true);
    let new = Obj::new(OTHER, true);
    let wrong = Obj::new(0x77, true);
    let mut c = mk_core(VhConstOp::Rmw(UnsafeRmwOp::GetAndSet), b'L');
    c.result_check = VhRefValueCheck::ExactClass(OTHER);
    let gas = build(&c);
    // Casts: fake objects' addresses as raw pointer words.
    holder.set_long(old.addr() as i64);
    assert_eq!(run(&gas, holder.addr(), 0, new.addr() as i64), old.addr() as i64);
    assert_eq!(holder.long(), new.addr() as i64, "swapped");
    holder.set_long(0);
    assert_eq!(run(&gas, holder.addr(), 0, new.addr() as i64), 0, "a null old value");
    assert_eq!(holder.long(), new.addr() as i64);
    holder.set_long(wrong.addr() as i64);
    assert_eq!(run(&gas, holder.addr(), 0, new.addr() as i64), DECLINED);
    assert_eq!(holder.long(), wrong.addr() as i64, "nothing written on a failed cast");
}

/// JIT round 21 wave 22, lane vhconst (UPDATERS21-6,
/// `CRATONVM_JIT_VH_CONST_CAST_CAE`): a reference compareAndExchange with
/// a cast decides the outcome and the cast from one read: a matching
/// witness that passes the cast is exchanged and returned, a non-matching
/// one that passes is returned with the field unchanged, and a witness
/// that fails the cast declines on both paths with nothing written (the
/// call exchanges, then throws the ClassCastException).
#[test]
fn r21_vhconst_a_cast_compare_and_exchange_screens_the_witness_before_any_write() {
    let holder = Obj::new(CLASS, true);
    let old = Obj::new(OTHER, true);
    let new = Obj::new(OTHER, true);
    let wrong = Obj::new(0x77, true);
    let mut c = mk_core(VhConstOp::Rmw(UnsafeRmwOp::Cae), b'L');
    c.result_check = VhRefValueCheck::ExactClass(OTHER);
    let cae = build(&c);
    // Casts: fake objects' addresses as raw pointer words.
    let (o, n, w) = (old.addr() as i64, new.addr() as i64, wrong.addr() as i64);
    holder.set_long(o);
    assert_eq!(run(&cae, holder.addr(), o, n), o, "a hit returns the witness");
    assert_eq!(holder.long(), n, "exchanged");
    assert_eq!(run(&cae, holder.addr(), o, 0), n, "a miss returns the field");
    assert_eq!(holder.long(), n, "unchanged on a miss");
    holder.set_long(0);
    assert_eq!(run(&cae, holder.addr(), 0, o), 0, "a null witness passes the cast");
    assert_eq!(holder.long(), o);
    holder.set_long(w);
    assert_eq!(run(&cae, holder.addr(), w, n), DECLINED, "the hit path's cast fails");
    assert_eq!(holder.long(), w, "nothing written");
    assert_eq!(run(&cae, holder.addr(), o, n), DECLINED, "the miss path's cast fails");
    assert_eq!(holder.long(), w);
    // Without a cast the exchange is the plain LOCK CMPXCHG row.
    let plain = build(&mk_core(VhConstOp::Rmw(UnsafeRmwOp::Cae), b'L'));
    assert_eq!(run(&plain, holder.addr(), w, n), w);
    assert_eq!(holder.long(), n);
}

/// JIT round 21 wave 1, lane updaters (review UPD21-T1): the `Unsafe`
/// reference volatile / getAndSet access bytes
/// (`direct_helpers::unsafe_ref_volatile_access_bytes`, the tail
/// `UNSAFE_REF_EXCHANGE_RESULT_BYTES`), EXECUTED on a fake holder with
/// RAX = holder, R11 = the published displacement and RDX = the value, as
/// both tiers leave them after the cell screen. A get returns the field,
/// a put (volatile, release, opaque) stores it, a getAndSet stores it and
/// returns the previous reference; the neighbouring words never change.
#[test]
fn upd21_reference_volatile_access_bytes_execute_as_documented() {
    use crate::direct_helpers::{
        unsafe_ref_volatile_access_bytes, UnsafeRefVolatileAccess,
        UNSAFE_REF_EXCHANGE_RESULT_BYTES,
    };
    // `extern "C" fn(holder, displacement, value) -> i64`.
    let build3 = |access: UnsafeRefVolatileAccess| {
        let mut body: Vec<u8> = Vec::new();
        body.extend_from_slice(&mov(0, CALL_ARG_REGS[0])); // RAX = holder
        body.extend_from_slice(&mov(11, CALL_ARG_REGS[1])); // R11 = displacement
        body.extend_from_slice(&mov(2, CALL_ARG_REGS[2])); // RDX = value
        body.extend_from_slice(&unsafe_ref_volatile_access_bytes(access));
        if access.exchange {
            body.extend_from_slice(&UNSAFE_REF_EXCHANGE_RESULT_BYTES);
        }
        body.push(0xC3); // RET: a get's reference or a getAndSet's previous one in RAX
        let mut buf = crate::ExecutableBuffer::new(1024).expect("executable buffer");
        buf.emit(&body);
        assert!(!buf.overflowed());
        buf.finalize();
        buf
    };
    let run3 = |buf: &crate::ExecutableBuffer, holder: u64, disp: i64, value: i64| -> i64 {
        // SAFETY: a complete `extern "C" fn(u64, i64, i64) -> i64` that
        // reads and writes only the fake holder the caller keeps alive and
        // writes RAX, RDX and R11 (caller-saved on both ABIs).
        let f = unsafe {
            std::mem::transmute::<*const u8, extern "C" fn(u64, i64, i64) -> i64>(buf.as_ptr())
        };
        f(holder, disp, value)
    };
    let access = |is_put: bool, ordered_store: bool, exchange: bool| UnsafeRefVolatileAccess {
        is_put,
        ordered_store,
        exchange,
    };
    // The fake holder's field is the long at `DISP` (16); the words after
    // it are sentinels no access may touch.
    let holder = Obj::new(CLASS, true);
    let sentinel = |at: usize| -> i64 {
        i64::from_le_bytes(holder.bytes()[at..at + 8].try_into().expect("8 bytes"))
    };
    let reset = || {
        holder.set_long(0x2222);
        holder.bytes()[24..32].copy_from_slice(&0x3333i64.to_le_bytes());
        holder.bytes()[32..40].copy_from_slice(&0x4444i64.to_le_bytes());
    };
    let untouched = |what: &str| {
        assert_eq!(sentinel(24), 0x3333, "{what}: the next word");
        assert_eq!(sentinel(32), 0x4444, "{what}: the word after");
    };
    let disp = i64::from(DISP);
    reset();
    let get = build3(access(false, false, false));
    assert_eq!(run3(&get, holder.addr(), disp, 0), 0x2222, "a get returns the field");
    assert_eq!(holder.long(), 0x2222, "a get writes nothing");
    untouched("get");
    for (what, ordered) in [("volatile put", false), ("release / opaque put", true)] {
        reset();
        let put = build3(access(true, ordered, false));
        run3(&put, holder.addr(), disp, 0x7777);
        assert_eq!(holder.long(), 0x7777, "{what} stores");
        untouched(what);
        run3(&put, holder.addr(), disp, 0);
        assert_eq!(holder.long(), 0, "{what} stores a null");
    }
    reset();
    let xchg = build3(access(true, false, true));
    assert_eq!(
        run3(&xchg, holder.addr(), disp, 0x9999),
        0x2222,
        "getAndSet returns the previous"
    );
    assert_eq!(holder.long(), 0x9999, "getAndSet stores");
    assert_eq!(run3(&xchg, holder.addr(), disp, 0), 0x9999, "...and again");
    assert_eq!(run3(&xchg, holder.addr(), disp, 0x5555), 0, "the previous null");
    assert_eq!(holder.long(), 0x5555);
    untouched("getAndSet");
}







/// A fake supers display (nio21 wave 12 L2): `Object` = 1, `CLASS` its
/// subclass, `SUB` and `SUB2` below `CLASS`, `OTHER` beside it. Returns the
/// cell, which owns (through the leaked boxes) every table and display.
fn fake_display() -> (Box<u64>, Vec<Box<[u32]>>, Box<[u64]>) {
    let chains: [&[u32]; 5] = [
        &[1],
        &[1, CLASS],
        &[1, CLASS, SUB],
        &[1, CLASS, SUB, SUB2],
        &[1, OTHER],
    ];
    // nio21 wave 14 L4: and each class's interface set after its chain.
    let ifaces: [&[u32]; 5] = [&[], &[], &[IFACE_B], &[IFACE_B, IFACE_A], &[IFACE_A]];
    let displays: Vec<Box<[u32]>> = chains
        .iter()
        .zip(ifaces)
        .map(|(c, i)| {
            let mut d = vec![u32::try_from(c.len()).expect("small")];
            d.extend_from_slice(c);
            d.push(u32::try_from(i.len()).expect("small"));
            d.extend_from_slice(i);
            d.into_boxed_slice()
        })
        .collect();
    let cap = 0x5000u64;
    let mut table = vec![0u64; usize::try_from(cap).expect("small") + 1];
    table[0] = cap;
    for (c, d) in chains.iter().zip(&displays) {
        let id = *c.last().expect("non-empty");
        table[1 + usize::try_from(id).expect("small")] = d.as_ptr() as u64; // Cast: address
    }
    let table = table.into_boxed_slice();
    let cell = Box::new(table.as_ptr() as u64); // Cast: address
    (cell, displays, table)
}

const SUB: u32 = 0x2001;
const SUB2: u32 = 0x2002;
const IFACE_A: u32 = 0x3001;
const IFACE_B: u32 = 0x3002;

/// nio21 wave 14 L4: an interface-typed field's value passes when its
/// class's interface set lists the interface, wherever in the set.
#[test]
fn an_interface_typed_value_passes_the_interface_set() {
    let (cell, displays, table) = fake_display();
    let cell_addr = &*cell as *const u64 as usize; // Cast: the cell's address
    let holder = Obj::new(CLASS, true);
    let addr = |o: &Obj| i64::try_from(o.addr()).expect("user-space address");
    for (iface, admitted, refused) in [
        (
            IFACE_A,
            vec![SUB2, OTHER],
            vec![SUB, CLASS, 1, 0x4FFF, 0x7000],
        ),
        (IFACE_B, vec![SUB, SUB2], vec![OTHER, CLASS]),
    ] {
        let mut s = mk_core(VhConstOp::Rmw(UnsafeRmwOp::Cas), b'L');
        let d = VhSubtypeDisplay {
            cell: cell_addr,
            depth: 0,
            interface: true,
        };
        s.expected_check = VhRefValueCheck::Subtype(iface, d);
        s.value_check = VhRefValueCheck::Subtype(iface, d);
        let cas = build(&s);
        for id in admitted {
            let v = Obj::new(id, true);
            holder.set_long(0);
            assert_eq!(
                run(&cas, holder.addr(), 0, addr(&v)),
                1,
                "{iface:#x} admits {id:#x}"
            );
            assert_eq!(holder.long(), addr(&v));
            assert_eq!(
                run(&cas, holder.addr(), addr(&v), 0),
                1,
                "as the expected value"
            );
        }
        for id in refused {
            let v = Obj::new(id, true);
            assert_eq!(
                run(&cas, holder.addr(), 0, addr(&v)),
                DECLINED,
                "{iface:#x} refuses {id:#x}"
            );
        }
    }
    drop((cell, displays, table));
}

#[test]
fn a_subclass_receiver_and_a_subclass_value_pass_the_display() {
    let (cell, displays, table) = fake_display();
    let class_display = VhSubtypeDisplay {
        cell: &*cell as *const u64 as usize, // Cast: the cell's address
        depth: 1,
        interface: false,
    };
    // A getAndSet of a reference whose receiver may be a CLASS subclass
    // and whose value must be a CLASS subclass too (the CompletableFuture
    // `Completion.NEXT` shape: an abstract coordinate class and type).
    let mut c = mk_core(VhConstOp::Get, b'J');
    c.receiver_display = Some(class_display);
    let get = build(&c);
    for id in [CLASS, SUB, SUB2] {
        let o = Obj::new(id, true);
        o.set_long(id.into());
        assert_eq!(
            run(&get, o.addr(), 0, 0),
            i64::from(id),
            "receiver class {id:#x}"
        );
    }
    for id in [OTHER, 1, 0x4FFF, 0x7000] {
        let o = Obj::new(id, true);
        assert_eq!(
            run(&get, o.addr(), 0, 0),
            DECLINED,
            "receiver class {id:#x}"
        );
    }
    let mut s = mk_core(VhConstOp::Set { volatile: false }, b'L');
    s.receiver_display = Some(class_display);
    s.value_check = VhRefValueCheck::Subtype(CLASS, class_display);
    let set = build(&s);
    let holder = Obj::new(SUB, true);
    let addr = |o: &Obj| i64::try_from(o.addr()).expect("user-space address");
    for id in [CLASS, SUB2] {
        let v = Obj::new(id, true);
        assert_ne!(
            run(&set, holder.addr(), 0, addr(&v)),
            DECLINED,
            "value class {id:#x}"
        );
        assert_eq!(holder.long(), addr(&v));
    }
    let unrelated = Obj::new(OTHER, true);
    assert_eq!(run(&set, holder.addr(), 0, addr(&unrelated)), DECLINED);
    assert_ne!(run(&set, holder.addr(), 0, 0), DECLINED, "null");
    assert_eq!(holder.long(), 0);
    drop((cell, displays, table));
}

/// JIT r20 wave 11 (lane pAlloc, W14L4-3): the typecheck display arm, as
/// `extern "C" fn(class_id) -> 1 | DECLINED`, answers a hit for the
/// target, a subclass and an implementor, and declines every other class
/// -- an unrelated one, one with no display, one past the table, and a
/// synthetic id at or past `0x8000_0000` -- so a miss always reaches the
/// helper.
#[test]
fn the_typecheck_display_arm_hits_subtypes_and_declines_the_rest() {
    let (cell, displays, table) = fake_display();
    let cell_addr = &*cell as *const u64 as usize; // Cast: the cell's address
    let build_arm = |display: VhSubtypeDisplay, target: u32| {
        let (bytes, decline) = typecheck_display_hit_bytes(display, target);
        let mut body: Vec<u8> = Vec::new();
        // RCX = arg0, zero-extended as `MOV ECX, [RAX]` leaves it.
        body.extend_from_slice(&mov(1, CALL_ARG_REGS[0]));
        body.extend_from_slice(&[0x89, 0xC9]); // MOV ECX, ECX
        let at = body.len();
        body.extend_from_slice(&bytes);
        body.extend_from_slice(&[0xB8, 0x01, 0x00, 0x00, 0x00, 0xC3]); // MOV EAX, 1 ; RET
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
    };
    let ask = |buf: &crate::ExecutableBuffer, id: u32| -> i64 {
        // SAFETY: a complete `extern "C" fn(u64) -> i64` that reads only
        // the fake display tables the test keeps alive and writes RAX,
        // RCX, R11 (caller-saved on both ABIs).
        let f = unsafe {
            std::mem::transmute::<*const u8, extern "C" fn(u64) -> i64>(buf.as_ptr())
        };
        f(u64::from(id))
    };
    let class_arm = build_arm(
        VhSubtypeDisplay {
            cell: cell_addr,
            depth: 1,
            interface: false,
        },
        CLASS,
    );
    for id in [CLASS, SUB, SUB2] {
        assert_eq!(ask(&class_arm, id), 1, "{id:#x} is a CLASS");
    }
    for id in [OTHER, 1, 0x4FFF, 0x7000, 0x8000_0001, u32::MAX] {
        assert_eq!(ask(&class_arm, id), DECLINED, "{id:#x} declines");
    }
    let iface_arm = build_arm(
        VhSubtypeDisplay {
            cell: cell_addr,
            depth: 0,
            interface: true,
        },
        IFACE_A,
    );
    for id in [SUB2, OTHER] {
        assert_eq!(ask(&iface_arm, id), 1, "{id:#x} implements IFACE_A");
    }
    for id in [SUB, CLASS, 1, 0x4FFF, 0x8000_0001] {
        assert_eq!(ask(&iface_arm, id), DECLINED, "{id:#x} declines");
    }
    drop((cell, displays, table));
}

/// JIT r20 wave 17 (lane pAlloc2, `CRATONVM_JIT_TYPECHECK_DISPLAY_MISS`):
/// the hit-or-miss arm, as `extern "C" fn(class_id) -> 1 | 0 | DECLINED`
/// over displays that carry the trailing verdict word. A subtype is `1`; a
/// non-subtype whose display says hierarchy-only is `0`; a verdict-0
/// class, a class with no display, an id past the table and any class
/// while the duplicate-name latch is raised decline.
#[test]
fn the_typecheck_display_answer_arm_answers_misses_of_hierarchy_only_classes() {
    const PROXYISH: u32 = 0x2003;
    // (chain, interfaces, verdict)
    let rows: [(&[u32], &[u32], u32); 5] = [
        (&[1], &[], 1),
        (&[1, CLASS], &[], 1),
        (&[1, CLASS, SUB], &[IFACE_B], 1),
        (&[1, OTHER], &[IFACE_A], 1),
        (&[1, PROXYISH], &[IFACE_B], 0),
    ];
    let displays: Vec<Box<[u32]>> = rows
        .iter()
        .map(|(c, i, v)| {
            let mut d = vec![u32::try_from(c.len()).expect("small")];
            d.extend_from_slice(c);
            d.push(u32::try_from(i.len()).expect("small"));
            d.extend_from_slice(i);
            d.push(*v);
            d.into_boxed_slice()
        })
        .collect();
    let cap = 0x5000u64;
    let mut table = vec![0u64; usize::try_from(cap).expect("small") + 1];
    table[0] = cap;
    for ((c, _, _), d) in rows.iter().zip(&displays) {
        let id = *c.last().expect("non-empty");
        table[1 + usize::try_from(id).expect("small")] = d.as_ptr() as u64; // Cast: address
    }
    let table = table.into_boxed_slice();
    let cell = Box::new(table.as_ptr() as u64); // Cast: address
    let cell_addr = &*cell as *const u64 as usize; // Cast: the cell's address
    let latch = Box::new(std::sync::atomic::AtomicU8::new(0));
    let latch_addr = latch.as_ptr() as usize; // Cast: the latch's address
    let build_arm = |display: VhSubtypeDisplay, target: u32| {
        let (bytes, decline, hit, miss) =
            typecheck_display_answer_bytes(display, target, latch_addr);
        let mut body: Vec<u8> = Vec::new();
        body.extend_from_slice(&mov(1, CALL_ARG_REGS[0]));
        body.extend_from_slice(&[0x89, 0xC9]); // MOV ECX, ECX
        let at = body.len();
        body.extend_from_slice(&bytes);
        let hit_at = body.len();
        body.extend_from_slice(&[0xB8, 0x01, 0x00, 0x00, 0x00, 0xC3]); // MOV EAX, 1 ; RET
        let miss_at = body.len();
        body.extend_from_slice(&[0x31, 0xC0, 0xC3]); // XOR EAX, EAX ; RET
        let decline_at = body.len();
        body.extend_from_slice(&[0x48, 0xC7, 0xC0, 0xFF, 0xFF, 0xFF, 0xFF, 0xC3]);
        let mut patch = |field: usize, to: usize| {
            let operand = at + field;
            let rel =
                i32::try_from(to).expect("small") - i32::try_from(operand + 4).expect("small");
            body[operand..operand + 4].copy_from_slice(&rel.to_le_bytes());
        };
        for d in decline {
            patch(d, decline_at);
        }
        patch(hit, hit_at);
        patch(miss, miss_at);
        let mut buf = crate::ExecutableBuffer::new(1024).expect("executable buffer");
        buf.emit(&body);
        assert!(!buf.overflowed());
        buf.finalize();
        buf
    };
    let ask = |buf: &crate::ExecutableBuffer, id: u32| -> i64 {
        // SAFETY: a complete `extern "C" fn(u64) -> i64` that reads only
        // the fake display tables and the latch the test keeps alive and
        // writes RAX, RCX, R11 (caller-saved on both ABIs).
        let f = unsafe {
            std::mem::transmute::<*const u8, extern "C" fn(u64) -> i64>(buf.as_ptr())
        };
        f(u64::from(id))
    };
    let class_arm = build_arm(
        VhSubtypeDisplay {
            cell: cell_addr,
            depth: 1,
            interface: false,
        },
        CLASS,
    );
    for id in [CLASS, SUB] {
        assert_eq!(ask(&class_arm, id), 1, "{id:#x} is a CLASS");
    }
    for id in [OTHER, 1] {
        assert_eq!(ask(&class_arm, id), 0, "{id:#x} is not a CLASS");
    }
    for id in [PROXYISH, 0x4FFF, 0x8000_0001, u32::MAX] {
        assert_eq!(ask(&class_arm, id), DECLINED, "{id:#x} declines");
    }
    let iface_arm = build_arm(
        VhSubtypeDisplay {
            cell: cell_addr,
            depth: 0,
            interface: true,
        },
        IFACE_A,
    );
    assert_eq!(ask(&iface_arm, OTHER), 1, "OTHER implements IFACE_A");
    for id in [SUB, CLASS, 1] {
        assert_eq!(ask(&iface_arm, id), 0, "{id:#x} does not implement IFACE_A");
    }
    for id in [PROXYISH, 0x4FFF, 0x8000_0001] {
        assert_eq!(ask(&iface_arm, id), DECLINED, "{id:#x} declines");
    }
    // A raised duplicate-name latch turns every miss into a decline; hits
    // stand.
    latch.store(1, std::sync::atomic::Ordering::Relaxed);
    assert_eq!(ask(&class_arm, SUB), 1);
    assert_eq!(ask(&class_arm, OTHER), DECLINED);
    assert_eq!(ask(&iface_arm, SUB), DECLINED);
    drop((cell, displays, table, latch));
}
