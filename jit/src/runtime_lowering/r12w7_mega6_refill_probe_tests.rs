//! Round 12 wave 7, lane mega6 (`CRATONVM_JIT_IC_SAME_KEY_REFILL`): what
//! the cell-first stub a megamorphic site runs sees when a receiver's
//! callee tiers up while a thread is inside compiled code (an OSR loop).
//! The supersede retires the receiver's class cell; the helper's next
//! publication must refill it, or every later call of that receiver
//! misses the cell for the life of the VM. EXECUTED on this host's ABI
//! against real published bodies and the real `MegaDispatchTable`, with a
//! JIT execution token held so nothing is graced in between.
use super::r12w3_mega2_table_probe_tests::{
    body, build_ordered, publish, run, Obj, ARG, ARG1, CTX, MISS, RECV,
};
use super::*;
use crate::{JitCache, MegaDispatchTable, MEGA_DISPATCH_WAYS};

/// `build_ordered`'s frame and stub, entered the way the single-pass
/// megamorphic gate enters it (W7-1): the harness loads the receiver's
/// class id into EAX as the cascade does, poisons RDX (the gate entry must
/// set it), and jumps to the stub's gate entry. Only for a non-null
/// plain-object receiver: the gate entry's contract.
fn build_gate_entry(
    pic: &JitPICSlot,
    mega_table: bool,
    class_slots: bool,
    cell_first: bool,
) -> ExecutableBuffer {
    let mut buf = ExecutableBuffer::new(4096).expect("buffer");
    buf.emit(&[0x55, 0x48, 0x89, 0xE5, 0x48, 0x83, 0xEC, 0x40]);
    emit_store_frame(&mut buf, CTX, ENTRY_ABI_REGS[0]);
    emit_store_frame(&mut buf, RECV, ENTRY_ABI_REGS[1]);
    emit_store_frame(&mut buf, ARG1, ENTRY_ABI_REGS[2]);
    emit_load_frame(&mut buf, RAX, RECV);
    buf.emit(&[0x8B, 0x00]); // MOV EAX, [RAX]: the cascade's class id
    emit_mov_imm64(&mut buf, RDX, 0xDEAD_BEEF_DEAD_BEEF);
    let to_gate = emit_jmp(&mut buf);
    let (hits, _, fast, _) = emit_hashed_vtable_stub_body(
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
        true,
    );
    assert_eq!(hits.len(), 1, "one hit exit");
    let fast = fast.expect("a gate entry when asked for");
    let rel = i32::try_from(fast as i64 - (to_gate as i64 + 4)).expect("rel32");
    assert!(buf.try_patch_i32(to_gate, rel).is_ok());
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

/// W7-1: for every class, in every stub order, the gate entry answers
/// exactly what the full entry answers -- the class cell, the site's own
/// hashed ways, the shared table, or the fall-through -- including the
/// context ABI, although RDX was poisoned on the way in.
#[test]
fn the_gate_entry_answers_what_the_full_entry_answers() {
    let cache = JitCache::new();
    let plain = publish(&cache, "ge-plain", body(ENTRY_ABI_REGS[1], 0x500));
    let ctx = publish(&cache, "ge-ctx", body(ENTRY_ABI_REGS[2], 0x600));
    let table = cache.mega_dispatch_table();
    let selector = table
        .selector_id("p/GE", "f", "(J)J", 0, 51, 0)
        .expect("selector");
    let pic = Box::new(JitPICSlot::new());
    pic.bind_mega_dispatch(table, selector);
    pic.bind_mega_class_slot(table, 2);
    let classes: Vec<u32> = (1..900).step_by(7).collect();
    for &c in &classes {
        let (entry, tagged) = if c % 2 == 0 {
            (ctx.entry_ptr() as u64, true)
        } else {
            (plain.entry_ptr() as u64, false)
        };
        if c % 3 == 0 {
            // A cell (and a shared way, when its set has room).
            let slot = Some((2, 4));
            table.install_with_class_slot(c, selector, slot, entry, tagged, false);
        } else if c % 3 == 1 {
            // The site's own hashed ways (and inline ways).
            pic.install(c, "C", entry, tagged, false);
        }
        // Otherwise nothing: the fall-through.
    }
    let orders = [
        (true, true, true),
        (true, true, false),
        (true, false, false),
        (false, false, false),
    ];
    for (mega_table, class_slots, cell_first) in orders {
        let full = build_ordered(&pic, mega_table, class_slots, cell_first);
        let gate = build_gate_entry(&pic, mega_table, class_slots, cell_first);
        let mut hits = 0;
        for &c in &classes {
            let obj = Obj::new(c);
            let expect = run(&full, obj.addr());
            if expect != MISS {
                hits += 1;
            }
            assert_eq!(
                run(&gate, obj.addr()),
                expect,
                "class {c}, order {mega_table}/{class_slots}/{cell_first}"
            );
        }
        assert!(hits > 0, "the orders must hit something");
    }
    pic.clear_entries();
    table.clear_entries();
    for b in [plain, ctx] {
        crate::defer_jit_owner(Some(b));
    }
}

/// Fill `class_id`'s shared set for `selector` with other classes, so a
/// hit for `class_id` can only come from its class cell.
fn crowd_out(table: &MegaDispatchTable, selector: u32, class_id: u32, filler: u64) {
    let set = MegaDispatchTable::set_base(class_id, selector);
    let others: Vec<u32> = (1001u32..400_000)
        .filter(|&c| MegaDispatchTable::set_base(c, selector) == set)
        .take(MEGA_DISPATCH_WAYS)
        .collect();
    assert_eq!(others.len(), MEGA_DISPATCH_WAYS);
    for c in others {
        table.install(c, selector, filler, false, false);
    }
}

#[test]
fn a_cell_a_supersede_retired_is_called_again_once_its_key_refills_it() {
    let cache = JitCache::new();
    let c1 = publish(&cache, "rf-c1-body", body(ENTRY_ABI_REGS[1], 0x100));
    let c2 = publish(&cache, "rf-c2-body", body(ENTRY_ABI_REGS[2], 0x200));
    let filler = publish(&cache, "rf-filler", body(ENTRY_ABI_REGS[1], 0x7));
    let table = cache.mega_dispatch_table();
    let selector = table
        .selector_id("p/RF", "f", "(J)J", 0, 41, 0)
        .expect("selector");
    let other = table
        .selector_id("p/RF", "g", "(J)J", 0, 41, 0)
        .expect("selector");
    let pic = Box::new(JitPICSlot::new());
    pic.bind_mega_dispatch(table, selector);
    pic.bind_mega_class_slot(table, 3);
    let stub = build_ordered(&pic, true, true, true);
    let seven = Obj::new(7);
    crowd_out(table, selector, 7, filler.entry_ptr() as u64);

    let (e1, e2) = (c1.entry_ptr() as u64, c2.entry_ptr() as u64);
    let execution = crate::jit_execution_enter();
    table.install_with_class_slot(7, selector, Some((3, 8)), e1, false, false);
    assert!(
        table.lookup(7, selector).is_none(),
        "the shared set is full"
    );
    assert_eq!(
        run(&stub, seven.addr()),
        ARG ^ 0x100,
        "the C1 body, from the cell"
    );
    // The C2 body supersedes it: the cell is retired, the stub misses.
    table.retire_entry(e1 as usize);
    assert_eq!(run(&stub, seven.addr()), MISS);
    // Another selector sharing the column must wait for the grace.
    table.install_with_class_slot(7, other, Some((3, 8)), e1, false, false);
    assert_eq!(
        run(&stub, seven.addr()),
        MISS,
        "another key's word was called"
    );
    // The helper's re-publication of the SAME key refills the cell at
    // once, and the stub calls the new body, with the new body's ABI.
    table.install_with_class_slot(7, selector, Some((3, 8)), e2, true, false);
    let refilled = crate::inline_cache_pic::same_key_refill_enabled();
    let expect = if refilled { ARG ^ 0x200 } else { MISS };
    assert_eq!(run(&stub, seven.addr()), expect, "refill {refilled}");
    crate::jit_execution_leave(execution);

    table.clear_entries();
    assert_eq!(run(&stub, seven.addr()), MISS);
    drop(stub);
    for b in [c1, c2, filler] {
        crate::defer_jit_owner(Some(b));
    }
}

/// The same at the site's own hashed ways (the stub's second probe): a
/// retired way its receiver refills is called; nothing else is.
#[test]
fn a_sites_retired_hashed_way_is_called_again_once_its_class_refills_it() {
    let cache = JitCache::new();
    let c1 = publish(&cache, "rf-h1-body", body(ENTRY_ABI_REGS[1], 0x300));
    let c2 = publish(&cache, "rf-h2-body", body(ENTRY_ABI_REGS[2], 0x400));
    let pic = Box::new(JitPICSlot::new());
    // No shared table and no cells: only the site's own hashed ways.
    let stub = build_ordered(&pic, false, false, false);
    let target = JitPICSlot::mega_base_index(7);
    let partner = (8u32..100_000)
        .find(|&c| JitPICSlot::mega_base_index(c) == target)
        .expect("a class sharing class 7's set");
    let seven = Obj::new(7);

    let execution = crate::jit_execution_enter();
    pic.install(7, "Seven", c1.entry_ptr() as u64, false, false);
    pic.install(partner, "Partner", c1.entry_ptr() as u64, false, false);
    assert_eq!(run(&stub, seven.addr()), ARG ^ 0x300);
    assert!(
        pic.retire_entry(c1.entry_ptr() as usize),
        "the supersede walk"
    );
    assert_eq!(run(&stub, seven.addr()), MISS);
    pic.install(partner, "Partner", c2.entry_ptr() as u64, true, false);
    pic.install(7, "Seven", c2.entry_ptr() as u64, true, false);
    let refilled = crate::inline_cache_pic::same_key_refill_enabled();
    let expect = if refilled { ARG ^ 0x400 } else { MISS };
    assert_eq!(run(&stub, seven.addr()), expect, "refill {refilled}");
    crate::jit_execution_leave(execution);

    pic.clear_entries();
    assert_eq!(run(&stub, seven.addr()), MISS);
    drop(stub);
    for b in [c1, c2] {
        crate::defer_jit_owner(Some(b));
    }
}
