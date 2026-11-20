use super::{
    vh_const_access_bytes, vh_const_core_bytes, vh_const_core_bytes_post, vh_g1_gates,
    VhConstCore, VhConstOp, VhG1Gates, VhRefValueCheck, CALL_ARG_REGS,
};
use crate::direct_helpers::UnsafeRmwOp;
use std::cell::{Cell, UnsafeCell};
use std::sync::atomic::AtomicUsize;

const CLASS: u32 = 0x2468;
const DISP: i32 = 16;
const DECLINED: i64 = -1;

/// A 1 KiB fake heap, region-aligned; objects of 64 bytes at chosen
/// offsets (class id at 0, a plain-object kind byte, COMPACT, the field at
/// `DISP`).
#[repr(C, align(256))]
struct Arena(UnsafeCell<[u8; 1024]>);

impl Arena {
    fn new() -> Box<Self> {
        Box::new(Arena(UnsafeCell::new([0; 1024])))
    }
    #[allow(clippy::mut_from_ref)]
    fn bytes(&self) -> &mut [u8; 1024] {
        // SAFETY: single-threaded test; no other reference to the bytes is
        // live while this one is used, and the emitted code runs only
        // between such uses.
        unsafe { &mut *self.0.get() }
    }
    fn base(&self) -> u64 {
        self.0.get() as u64 // Cast: the arena's address
    }
    /// Lay out an object at `off`; returns its address.
    fn obj(&self, off: usize) -> u64 {
        let b = self.bytes();
        b[off..off + 4].copy_from_slice(&CLASS.to_le_bytes());
        b[off + cratonvm_types::GC_FLAGS_BYTE_OFFSET] |= cratonvm_types::GC_FLAG_COMPACT;
        self.base() + off as u64 // Cast: a small offset
    }
    fn field(&self, obj: u64) -> i64 {
        let off = usize::try_from(obj - self.base()).expect("inside") + 16;
        i64::from_le_bytes(self.bytes()[off..off + 8].try_into().expect("8 bytes"))
    }
    fn set_field(&self, obj: u64, v: i64) {
        let off = usize::try_from(obj - self.base()).expect("inside") + 16;
        self.bytes()[off..off + 8].copy_from_slice(&v.to_le_bytes());
    }
}

/// `JIT_G1_BARRIER`'s shape: arena base, length, region mask, card words.
fn table(base: u64) -> Box<[AtomicUsize; 5]> {
    Box::new([
        AtomicUsize::new(usize::try_from(base).expect("address")),
        AtomicUsize::new(1024),
        AtomicUsize::new(!0xFF),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
    ])
}

fn core(
    op: VhConstOp,
    pre: &Cell<u8>,
    table: &[AtomicUsize; 5],
    post_call: bool,
) -> VhConstCore {
    VhConstCore {
        op,
        kind: b'L',
        disp: DISP,
        class_id: CLASS,
        receiver_display: None,
        cid_off: 0,
        expected_check: VhRefValueCheck::Admitted,
        value_check: VhRefValueCheck::Admitted,
        gates: None,
        result_check: VhRefValueCheck::Admitted,
        g1: Some(VhG1Gates {
            pre: pre.as_ptr() as usize,     // Cast: the gate byte's address
            table: table.as_ptr() as usize, // Cast: the table's address
            post_call,
        }),
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

fn patch(body: &mut [u8], operand: usize, target: usize) {
    let rel =
        i32::try_from(target).expect("small") - i32::try_from(operand + 4).expect("small");
    body[operand..operand + 4].copy_from_slice(&rel.to_le_bytes());
}

/// The core as `extern "C" fn(_, receiver, expected, value) -> i64`:
/// `DECLINED` on a decline edge; the post edge runs the access, sets
/// `*posted = 1` and returns the access's result. `plain` builds
/// [`vh_const_core_bytes`] (no post edge) instead.
fn build(core: &VhConstCore, posted: &Cell<u8>, plain: bool) -> crate::ExecutableBuffer {
    let (bytes, decline, post) = if plain {
        let (b, d) = vh_const_core_bytes(core);
        (b, d, None)
    } else {
        vh_const_core_bytes_post(core)
    };
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
    let post_at = body.len();
    let (access, access_decline) = vh_const_access_bytes(core);
    assert!(access_decline.is_empty(), "a plain access has no screen");
    body.extend_from_slice(&access);
    body.extend_from_slice(&[0x49, 0xBB]); // MOV R11, imm64
    body.extend_from_slice(&(posted.as_ptr() as u64).to_le_bytes()); // Cast: address
    body.extend_from_slice(&[0x41, 0xC6, 0x03, 0x01]); // MOV BYTE [R11], 1
    body.push(0xC3); // RET: the access's result in RAX
    for d in decline {
        patch(&mut body, at + d, decline_at);
    }
    if let Some(p) = post {
        patch(&mut body, at + p, post_at);
    }
    let mut buf = crate::ExecutableBuffer::new(1024).expect("executable buffer");
    buf.emit(&body);
    assert!(!buf.overflowed());
    buf.finalize();
    buf
}

fn run(buf: &crate::ExecutableBuffer, recv: u64, expected: u64, value: u64) -> i64 {
    // SAFETY: a complete `extern "C" fn(u64, u64, u64, u64) -> i64` that
    // reads only the fake arena, table and gate bytes the caller keeps
    // alive, and writes RAX, RCX, RDX, R10, R11 (caller-saved on both
    // ABIs), the fake field and the flag byte.
    let f = unsafe {
        std::mem::transmute::<*const u8, extern "C" fn(u64, u64, u64, u64) -> i64>(buf.as_ptr())
    };
    f(0, recv, expected, value)
}

fn i(v: u64) -> i64 {
    i64::try_from(v).expect("user-space address")
}

#[test]
fn without_a_mark_a_cas_is_inline_and_only_a_cross_region_value_takes_the_post_edge() {
    let heap = Arena::new();
    let t = table(heap.base());
    let pre = Box::new(Cell::new(0u8));
    let posted = Box::new(Cell::new(0u8));
    let holder = heap.obj(0x000);
    let near = heap.obj(0x040); // the holder's region
    let far = heap.obj(0x100); // the next region
    let cas_op = VhConstOp::Rmw(UnsafeRmwOp::Cas);
    let cas = build(&core(cas_op, &pre, &t, true), &posted, false);
    assert_eq!(run(&cas, holder, 0, near), 1);
    assert_eq!(
        (heap.field(holder), posted.get()),
        (i(near), 0),
        "same region: no barrier"
    );
    assert_eq!(run(&cas, holder, near, far), 1);
    assert_eq!(
        (heap.field(holder), posted.get()),
        (i(far), 1),
        "cross-region: the post edge"
    );
    posted.set(0);
    assert_eq!(run(&cas, holder, near, 0), 0, "a failed CAS");
    assert_eq!(
        (heap.field(holder), posted.get()),
        (i(far), 0),
        "a null value: no barrier"
    );
    assert_eq!(run(&cas, holder, far, 0), 1);
    assert_eq!((heap.field(holder), posted.get()), (0, 0));
    // A failed cross-region CAS still takes the edge (HotSpot's post
    // barrier runs on the new value whatever the outcome) and writes nothing.
    assert_eq!(run(&cas, holder, near, far), 0);
    assert_eq!((heap.field(holder), posted.get()), (0, 1));
    posted.set(0);
    // Without the post edge a cross-region value takes the call.
    let no_post = build(&core(cas_op, &pre, &t, false), &posted, false);
    assert_eq!(run(&no_post, holder, 0, far), DECLINED);
    assert_eq!(heap.field(holder), 0, "nothing stored before the call");
    assert_eq!(run(&no_post, holder, 0, near), 1);
    // And the plain bytes (no post edge to patch) decline it as well.
    let plain = build(&core(cas_op, &pre, &t, true), &posted, true);
    assert_eq!(run(&plain, holder, near, far), DECLINED);
    assert_eq!((heap.field(holder), posted.get()), (i(near), 0));
    // getAndSet / setVolatile / compareAndExchange with no mark.
    let xchg_op = VhConstOp::Rmw(UnsafeRmwOp::GetAndSet);
    let xchg = build(&core(xchg_op, &pre, &t, true), &posted, false);
    assert_eq!(run(&xchg, holder, 0, far), i(near), "the previous value");
    assert_eq!((heap.field(holder), posted.get()), (i(far), 1));
    posted.set(0);
    let set_v_op = VhConstOp::Set { volatile: true };
    let set_v = build(&core(set_v_op, &pre, &t, true), &posted, false);
    run(&set_v, holder, 0, near);
    assert_eq!((heap.field(holder), posted.get()), (i(near), 0));
    let cae_op = VhConstOp::Rmw(UnsafeRmwOp::Cae);
    let cae = build(&core(cae_op, &pre, &t, true), &posted, false);
    assert_eq!(run(&cae, holder, near, far), i(near), "the witness");
    assert_eq!((heap.field(holder), posted.get()), (i(far), 1));
    drop((pre, posted, t));
}

#[test]
fn the_region_test_subtracts_the_arena_base_first() {
    let heap = Arena::new();
    // The arena starts half a region in: regions are base + 0x80 + 256 k.
    let t = table(heap.base() + 0x80);
    let pre = Box::new(Cell::new(0u8));
    let posted = Box::new(Cell::new(0u8));
    let holder = heap.obj(0x0C0); // offset 0x40: region 0
    let same = heap.obj(0x100); // offset 0x80: region 0 (raw bit 8 differs)
    let other = heap.obj(0x180); // offset 0x100: region 1
    let set_op = VhConstOp::Set { volatile: false };
    let set = build(&core(set_op, &pre, &t, true), &posted, false);
    run(&set, holder, 0, same);
    assert_eq!((heap.field(holder), posted.get()), (i(same), 0));
    run(&set, holder, 0, other);
    assert_eq!((heap.field(holder), posted.get()), (i(other), 1));
    drop((pre, posted, t));
}

#[test]
fn while_marking_only_a_write_replacing_null_is_inline() {
    let heap = Arena::new();
    let t = table(heap.base());
    let pre = Box::new(Cell::new(1u8)); // the SATB queue is active
    let posted = Box::new(Cell::new(0u8));
    let holder = heap.obj(0x000);
    let near = heap.obj(0x040);
    let far = heap.obj(0x100);
    let cas_op = VhConstOp::Rmw(UnsafeRmwOp::Cas);
    let cas = build(&core(cas_op, &pre, &t, true), &posted, false);
    assert_eq!(
        run(&cas, holder, 0, near),
        1,
        "expected null: null replaced"
    );
    assert_eq!(heap.field(holder), i(near));
    assert_eq!(
        run(&cas, holder, near, far),
        DECLINED,
        "the helper logs the replaced value"
    );
    assert_eq!(
        (heap.field(holder), posted.get()),
        (i(near), 0),
        "nothing stored"
    );
    let cae_op = VhConstOp::Rmw(UnsafeRmwOp::Cae);
    let cae = build(&core(cae_op, &pre, &t, true), &posted, false);
    assert_eq!(
        run(&cae, holder, 0, far),
        i(near),
        "a failed exchange writes nothing"
    );
    assert_eq!(heap.field(holder), i(near));
    posted.set(0);
    assert_eq!(run(&cae, holder, near, far), DECLINED);
    let set_op = VhConstOp::Set { volatile: false };
    let set = build(&core(set_op, &pre, &t, true), &posted, false);
    assert_eq!(run(&set, holder, 0, far), DECLINED, "a non-null old value");
    assert_eq!(heap.field(holder), i(near));
    heap.set_field(holder, 0);
    run(&set, holder, 0, far);
    assert_eq!(
        (heap.field(holder), posted.get()),
        (i(far), 1),
        "a null old value"
    );
    posted.set(0);
    let xchg_op = VhConstOp::Rmw(UnsafeRmwOp::GetAndSet);
    let xchg = build(&core(xchg_op, &pre, &t, true), &posted, false);
    assert_eq!(
        run(&xchg, holder, 0, near),
        DECLINED,
        "getAndSet calls while marking"
    );
    assert_eq!(heap.field(holder), i(far));
    pre.set(0);
    assert_eq!(run(&xchg, holder, 0, near), i(far));
    assert_eq!(heap.field(holder), i(near));
    drop((pre, posted, t));
}

#[test]
fn the_g1_plan_needs_no_published_gates_a_pre_gate_and_a_live_table() {
    let t = table(0x1000_0000);
    let addr = t.as_ptr() as usize; // Cast: the table's address
    assert_eq!(
        vh_g1_gates(Some((1, 2, 0)), addr, 8, 16),
        None,
        "gates published"
    );
    assert_eq!(vh_g1_gates(None, addr, 0, 16), None, "no pre-gate byte");
    assert_eq!(vh_g1_gates(None, 0, 8, 16), None, "no table");
    let dead = table(0x1000_0000);
    dead[1].store(0, std::sync::atomic::Ordering::Release);
    let dead_addr = dead.as_ptr() as usize; // Cast: the table's address
    assert_eq!(vh_g1_gates(None, dead_addr, 8, 16), None, "unpublished");
    if super::vh_const_g1_ref_enabled()
        && super::ir_g1_ref_store_enabled()
        && crate::x64::g1_inline_barrier_enabled()
    {
        let g = vh_g1_gates(None, addr, 8, 16).expect("served");
        assert_eq!((g.pre, g.table), (8, addr));
        assert_eq!(g.post_call, super::ir_g1_gated_ref_store_enabled());
        let no_fn = vh_g1_gates(None, addr, 8, 0).expect("served");
        assert!(!no_fn.post_call, "no barrier function");
    }
}
