//! Round 12 wave 3, lane mega2 (proposal W2-1 step 2): the hashed stub's
//! shared-table probe, EXECUTED on this host's ABI (Win64 or SysV), around
//! real published bodies and the real `MegaDispatchTable` protocol.
use super::*;
use crate::{CompiledMethod, JitCache};
use std::sync::Arc;

/// `[RBP - off]` homes of the harness frame: context, receiver, argument.
pub(super) const CTX: i32 = 8;
pub(super) const RECV: i32 = 16;
pub(super) const ARG1: i32 = 24;
/// What the harness returns when the stub falls through to its helper.
pub(super) const MISS: u64 = 0x0DEA_D000;
pub(super) const ARG: u64 = 0x5000_0000;

/// A plain object: class id in the first four bytes, a zero mark word
/// (kind bits 0 = object).
#[repr(C, align(8))]
pub(super) struct Obj {
    pub(super) words: [u64; 4],
}

impl Obj {
    pub(super) fn new(class_id: u32) -> Box<Self> {
        Box::new(Self {
            words: [u64::from(class_id), 0, 0, 0],
        })
    }

    pub(super) fn addr(&self) -> u64 {
        self as *const Self as u64
    }
}

/// A callee returning its argument word `arg_reg` XOR `marker`: which body
/// ran, and whether the stub marshalled the ABI the way's word said.
pub(super) fn body(arg_reg: u8, marker: i32) -> CompiledMethod {
    let mut buf = ExecutableBuffer::new(64).expect("buffer");
    emit_mov_reg(&mut buf, RAX, arg_reg); // MOV RAX, arg
    buf.emit(&[0x48, 0x35]); // XOR RAX, imm32
    buf.emit(&marker.to_le_bytes());
    buf.emit_byte(0xC3); // RET
    CompiledMethod::new(buf)
}

pub(super) fn publish(
    cache: &JitCache,
    name: &str,
    code: CompiledMethod,
) -> Arc<CompiledMethod> {
    let cid = cratonvm_types::ClassId::new(0x1203);
    let class: Arc<str> = Arc::from("R12w3Mega2");
    let method: Arc<str> = Arc::from(name);
    let desc: Arc<str> = Arc::from("(J)J");
    cache.put(class.clone(), method.clone(), desc.clone(), cid, code);
    cache.get(&class, &method, &desc, cid).expect("published")
}

/// `extern "C" fn(context, receiver, arg) -> u64`: a frame holding the
/// three words, the stub for a `(receiver, arg)` site, `MISS` on its
/// fall-through and the callee's result on its hit exit. The class-slot
/// probe (round 12 wave 4) rides with the shared-table probe, as in
/// production; an unbound column skips it.
fn build(pic: &JitPICSlot, mega_table: bool) -> ExecutableBuffer {
    build_with(pic, mega_table, mega_table)
}

/// [`build`] with the class-slot probe chosen separately.
pub(super) fn build_with(
    pic: &JitPICSlot,
    mega_table: bool,
    class_slots: bool,
) -> ExecutableBuffer {
    build_ordered(pic, mega_table, class_slots, false)
}

/// [`build_with`], and `cell_first` for the round-12-wave-6 order (the
/// class-slot probe ahead of the site's own hashed ways).
pub(super) fn build_ordered(
    pic: &JitPICSlot,
    mega_table: bool,
    class_slots: bool,
    cell_first: bool,
) -> ExecutableBuffer {
    let mut buf = ExecutableBuffer::new(4096).expect("buffer");
    // PUSH RBP; MOV RBP, RSP; SUB RSP, 64 (16-aligned at the CALL, and the
    // homes sit above a Win64 callee's 32-byte shadow area).
    buf.emit(&[0x55, 0x48, 0x89, 0xE5, 0x48, 0x83, 0xEC, 0x40]);
    emit_store_frame(&mut buf, CTX, ENTRY_ABI_REGS[0]);
    emit_store_frame(&mut buf, RECV, ENTRY_ABI_REGS[1]);
    emit_store_frame(&mut buf, ARG1, ENTRY_ABI_REGS[2]);
    let (hits, imm_at, fast, _) = emit_hashed_vtable_stub_body(
        &mut buf,
        pic as *const JitPICSlot as usize,
        CTX,
        &[RECV, ARG1],
        0,
        0,
        0,
        0,
        None,
        None,
        mega_table,
        class_slots,
        cell_first,
        false,
    );
    assert_eq!(fast, None, "no gate entry unless asked for");
    assert_eq!(hits.len(), 1, "one hit exit");
    assert_eq!(
        buf.as_slice().get(imm_at..imm_at + 8),
        Some(&(pic as *const JitPICSlot as u64).to_le_bytes()[..]),
        "the reported offset names the baked slot"
    );
    emit_mov_imm64(&mut buf, RAX, MISS);
    buf.emit(&[0xC9, 0xC3]); // LEAVE; RET
    for hit in hits {
        patch_rel32_to_here(&mut buf, hit);
    }
    buf.emit(&[0xC9, 0xC3]); // LEAVE; RET
    assert!(!buf.overflowed());
    buf.finalize();
    buf
}

pub(super) fn run(stub: &ExecutableBuffer, receiver: u64) -> u64 {
    // SAFETY: `build` finalized `stub`, a complete
    // `extern "C" fn(u64, u64, u64) -> u64` whose callees are published
    // bodies retained by the table, the PIC or the test's own `Arc`s.
    let f = unsafe {
        std::mem::transmute::<*const u8, extern "C" fn(u64, u64, u64) -> u64>(stub.as_ptr())
    };
    f(0x1234, receiver, ARG)
}

#[test]
fn a_shared_table_hit_is_called_from_machine_code() {
    let cache = JitCache::new();
    let plain = publish(&cache, "plain", body(ENTRY_ABI_REGS[1], 0x100));
    let ctx = publish(&cache, "ctx", body(ENTRY_ABI_REGS[2], 0x200));
    let table = cache.mega_dispatch_table();
    let selector = table
        .selector_id("p/I", "f", "(J)J", 2, 11, 0)
        .expect("selector");
    let pic = Box::new(JitPICSlot::new());
    let stub = build(&pic, true);
    let seven = Obj::new(7);
    let nine = Obj::new(9);

    table.install(7, selector, plain.entry_ptr() as u64, false, false);
    assert_eq!(
        run(&stub, seven.addr()),
        MISS,
        "an unbound site reads no table"
    );
    pic.bind_mega_dispatch(table, selector);
    assert_eq!(
        run(&stub, seven.addr()),
        ARG ^ 0x100,
        "the shared way is called"
    );

    // A context-tagged way: the context goes first, the argument third.
    assert_eq!(run(&stub, nine.addr()), MISS);
    table.install(9, selector, ctx.entry_ptr() as u64, true, false);
    assert_eq!(
        run(&stub, nine.addr()),
        ARG ^ 0x200,
        "the tag selects the ABI"
    );

    // Another selector's way is not this site's.
    let other = table
        .selector_id("p/I", "g", "(J)J", 2, 11, 0)
        .expect("selector");
    let ten = Obj::new(10);
    table.install(10, other, plain.entry_ptr() as u64, false, false);
    assert_eq!(run(&stub, ten.addr()), MISS);

    // Null and non-object receivers never reach the probe.
    assert_eq!(run(&stub, 0), MISS);
    let mut array = Obj::new(7);
    const _: () = assert!(cratonvm_types::KIND_TAGS_BYTE_OFFSET < 8);
    array.words[0] |= u64::from(cratonvm_types::KIND_TAG_BYTE_MASK)
        << (8 * cratonvm_types::KIND_TAGS_BYTE_OFFSET);
    assert_eq!(
        run(&stub, array.addr()),
        MISS,
        "an array never matches a class way"
    );

    // The site's own hashed way is probed first.
    pic.install(7, "Seven", ctx.entry_ptr() as u64, true, false);
    assert_eq!(run(&stub, seven.addr()), ARG ^ 0x200);
    pic.clear_entries();
    assert_eq!(
        run(&stub, seven.addr()),
        ARG ^ 0x100,
        "back to the shared way"
    );

    // A retired way (key to RETIRED_KEY, then word to zero) is never live.
    table.retire_entry(plain.entry_ptr() as usize);
    assert_eq!(
        run(&stub, seven.addr()),
        MISS,
        "a retired way was probed as live"
    );
    assert_eq!(
        run(&stub, nine.addr()),
        ARG ^ 0x200,
        "only the named body retires"
    );
    let mut doomed = std::collections::HashSet::new();
    doomed.insert(ctx.entry_ptr() as usize);
    table.invalidate_targets(&doomed);
    assert_eq!(
        run(&stub, nine.addr()),
        MISS,
        "an invalidated way was probed as live"
    );

    // The same stub without the probe never reads the table.
    table.install(9, selector, ctx.entry_ptr() as u64, true, false);
    assert_eq!(run(&stub, nine.addr()), ARG ^ 0x200);
    let bare = build(&pic, false);
    assert_eq!(run(&bare, nine.addr()), MISS);
    let mut load = vec![0x41, 0x8B, 0x8A];
    load.extend_from_slice(&(JitPICSlot::MEGA_DISPATCH_SELECTOR_OFFSET as i32).to_le_bytes());
    assert!(!bare.as_slice().windows(load.len()).any(|w| w == load));
    assert!(stub.as_slice().windows(load.len()).any(|w| w == load));

    table.clear_entries();
    assert_eq!(run(&stub, nine.addr()), MISS);
    drop((stub, bare));
    for b in [plain, ctx] {
        crate::defer_jit_owner(Some(b));
    }
}

/// The machine hash and key are `MegaDispatchTable::set_base` / `key`: for
/// every class, the stub hits exactly when the Rust reader does (a full
/// set publishes nothing, so some classes miss in both).
#[test]
fn the_machine_probe_agrees_with_the_rust_reader_on_every_class() {
    let cache = JitCache::new();
    let plain = publish(&cache, "agree", body(ENTRY_ABI_REGS[1], 0x300));
    let table = cache.mega_dispatch_table();
    let selector = table
        .selector_id("p/J", "h", "(J)J", 0, 12, 0)
        .expect("selector");
    let pic = Box::new(JitPICSlot::new());
    pic.bind_mega_dispatch(table, selector);
    let stub = build(&pic, true);
    let classes: Vec<u32> = (1..1200).step_by(3).collect();
    for &c in &classes {
        table.install(c, selector, plain.entry_ptr() as u64, false, false);
    }
    let mut hits = 0;
    for &c in &classes {
        let obj = Obj::new(c);
        let expect = if table.lookup(c, selector).is_some() {
            hits += 1;
            ARG ^ 0x300
        } else {
            MISS
        };
        assert_eq!(run(&stub, obj.addr()), expect, "class {c}");
    }
    assert!(
        hits > classes.len() / 2,
        "the table published too little: {hits}"
    );
    table.clear_entries();
    drop(stub);
    crate::defer_jit_owner(Some(plain));
}

/// The single-pass entry point reports where the slot is baked (what the
/// loop duplicator re-points), and nothing for a refused stub.
#[test]
fn the_located_stub_names_its_baked_slot() {
    let slot = 0x7fff_0000_0000_2000usize;
    let mut buf = ExecutableBuffer::new(4096).expect("buffer");
    let (hits, imm_at, fast) =
        emit_hashed_vtable_stub_located(&mut buf, slot, CTX, &[RECV, ARG1], 0, 0, 0, 0, true);
    if !crate::direct_jit_callee_calls_enabled() {
        assert!(hits.is_empty() && imm_at.is_none() && fast.is_none());
        return;
    }
    // The gate entry, when asked for: `MOV EDX, EAX` then a JMP back.
    let fast = fast.expect("an emitted stub has its gate entry when asked");
    assert_eq!(
        buf.as_slice().get(fast..fast + 3),
        Some(&[0x89u8, 0xC2, 0xE9][..])
    );
    let at = imm_at.expect("an emitted stub names its slot");
    assert_eq!(
        buf.as_slice().get(at - 2..at + 8),
        Some(&[&[0x49u8, 0xBA][..], &(slot as u64).to_le_bytes()[..]].concat()[..]),
        "`MOV R10, imm64` in its 10-byte form"
    );
    let mut refused = ExecutableBuffer::new(4096).expect("buffer");
    let (hits, imm_at, fast) =
        emit_hashed_vtable_stub_located(&mut refused, 0, CTX, &[RECV, ARG1], 0, 0, 0, 0, true);
    assert!(hits.is_empty() && imm_at.is_none() && fast.is_none() && refused.pos() == 0);
}
