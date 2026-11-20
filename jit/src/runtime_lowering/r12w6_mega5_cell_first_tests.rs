//! Round 12 wave 6, lane mega5 (proposals W5-4 and W5-6): the cell-first
//! stub order, the megamorphic gate and the inline-first publication,
//! EXECUTED on this host's ABI against real published bodies and the real
//! `MegaDispatchTable`.
use super::r12w3_mega2_table_probe_tests::{
    body, build_ordered, build_with, publish, run, Obj, ARG, MISS,
};
use super::*;
use crate::{JitCache, MegaDispatchTable};

fn bound_pic(table: &MegaDispatchTable, selector: u32, column: u32) -> Box<JitPICSlot> {
    let pic = Box::new(JitPICSlot::new());
    pic.bind_mega_dispatch(table, selector);
    pic.bind_mega_class_slot(table, column);
    pic
}

/// With a receiver in BOTH the site's own hashed way (one body) and its
/// class cell (another), the cell-first stub calls the cell and the
/// wave-5 order calls the hashed way; every other answer is the same.
#[test]
fn a_cell_first_stub_calls_the_cell_before_the_sites_own_way() {
    let cache = JitCache::new();
    let plain = publish(&cache, "cf-plain", body(ENTRY_ABI_REGS[1], 0x100));
    let ctx = publish(&cache, "cf-ctx", body(ENTRY_ABI_REGS[2], 0x200));
    let table = cache.mega_dispatch_table();
    let selector = table
        .selector_id("p/CF", "f", "(J)J", 0, 31, 0)
        .expect("selector");
    let pic = bound_pic(table, selector, 3);
    let first = build_ordered(&pic, true, true, true);
    let wave5 = build_with(&pic, true, true);
    let (p, c) = (plain.entry_ptr() as u64, ctx.entry_ptr() as u64);
    let seven = Obj::new(7);

    assert_eq!(run(&first, seven.addr()), MISS);
    pic.install(7, "Seven", c, true, false);
    assert_eq!(
        run(&first, seven.addr()),
        ARG ^ 0x200,
        "no cell: the hashed way"
    );
    table.install_with_class_slot(7, selector, Some((3, 8)), p, false, false);
    assert!(table.lookup_class_cell(7, selector, 3).is_some());
    assert_eq!(
        run(&first, seven.addr()),
        ARG ^ 0x100,
        "the cell is probed first"
    );
    assert_eq!(
        run(&wave5, seven.addr()),
        ARG ^ 0x200,
        "the wave-5 order: the site's way"
    );
    pic.clear_entries();
    assert_eq!(run(&first, seven.addr()), ARG ^ 0x100);
    assert_eq!(run(&wave5, seven.addr()), ARG ^ 0x100);

    // A context-tagged cell reached first selects its ABI.
    let nine = Obj::new(9);
    table.install_with_class_slot(9, selector, Some((3, 8)), c, true, false);
    assert_eq!(
        run(&first, nine.addr()),
        ARG ^ 0x200,
        "the tag selects the ABI"
    );

    // A column-less site (and another column) falls into the hashed ways
    // and then the shared table.
    let unbound = Box::new(JitPICSlot::new());
    unbound.bind_mega_dispatch(table, selector);
    let unbound_first = build_ordered(&unbound, true, true, true);
    let eleven = Obj::new(11);
    unbound.install(11, "Eleven", c, true, false);
    assert_eq!(run(&unbound_first, eleven.addr()), ARG ^ 0x200);
    let elsewhere = bound_pic(table, selector, 4);
    let elsewhere_first = build_ordered(&elsewhere, true, true, true);
    let shared = if table.lookup(7, selector).is_some() {
        ARG ^ 0x100
    } else {
        MISS
    };
    assert_eq!(
        run(&unbound_first, seven.addr()),
        shared,
        "the shared table"
    );
    assert_eq!(
        run(&elsewhere_first, seven.addr()),
        shared,
        "the shared table"
    );

    // Null and non-object receivers never reach a probe.
    assert_eq!(run(&first, 0), MISS);
    let mut array = Obj::new(7);
    const _: () = assert!(cratonvm_types::KIND_TAGS_BYTE_OFFSET < 8);
    array.words[0] |= u64::from(cratonvm_types::KIND_TAG_BYTE_MASK)
        << (8 * cratonvm_types::KIND_TAGS_BYTE_OFFSET);
    assert_eq!(
        run(&first, array.addr()),
        MISS,
        "an array never matches a cell"
    );

    // Retired cells are never probed as live.
    table.retire_entry(plain.entry_ptr() as usize);
    assert_eq!(
        run(&first, seven.addr()),
        MISS,
        "a retired cell was probed as live"
    );
    assert_eq!(
        run(&first, nine.addr()),
        ARG ^ 0x200,
        "only the named body retires"
    );
    table.clear_entries();
    assert_eq!(run(&first, nine.addr()), MISS);

    // The order costs no bytes: the same probe, moved.
    assert_eq!(first.pos(), wave5.pos());
    let bare = build_ordered(&pic, true, false, true);
    assert_eq!(
        bare.pos(),
        build_with(&pic, true, false).pos(),
        "no class-slot probe, no cell-first order"
    );
    drop((first, wave5, unbound_first, elsewhere_first, bare));
    unbound.clear_entries();
    for b in [plain, ctx] {
        crate::defer_jit_owner(Some(b));
    }
}

/// For every class, across directory growths, the cell-first stub hits
/// exactly when the wave-5 stub does, and both agree with the Rust mirror
/// (a cell, or a way of the shared table).
#[test]
fn the_cell_first_order_agrees_with_the_wave5_order_on_every_class() {
    let cache = JitCache::new();
    let plain = publish(&cache, "cf-agree", body(ENTRY_ABI_REGS[1], 0x300));
    let table = cache.mega_dispatch_table();
    let selector = table
        .selector_id("p/CG", "h", "(J)J", 0, 32, 0)
        .expect("selector");
    let pic = bound_pic(table, selector, 2);
    let first = build_ordered(&pic, true, true, true);
    let wave5 = build_with(&pic, true, true);
    let classes: Vec<u32> = (1..1200).step_by(3).collect();
    for &c in &classes {
        let columns = if c % 5 == 0 { 1 } else { 4 };
        let column = if c % 5 == 0 { 1 } else { 2 };
        table.install_with_class_slot(
            c,
            selector,
            Some((column, columns)),
            plain.entry_ptr() as u64,
            false,
            false,
        );
    }
    for &c in &classes {
        let expect = if table.lookup_class_cell(c, selector, 2).is_some()
            || table.lookup(c, selector).is_some()
        {
            ARG ^ 0x300
        } else {
            MISS
        };
        let obj = Obj::new(c);
        assert_eq!(run(&first, obj.addr()), expect, "class {c}, cell first");
        assert_eq!(run(&wave5, obj.addr()), expect, "class {c}, wave-5 order");
    }
    table.clear_entries();
    drop((first, wave5));
    crate::defer_jit_owner(Some(plain));
}

/// `extern "C" fn(pic) -> u64`: R10 = the slot, the gate, 1 on its
/// fall-through and 2 where it jumps.
fn gate_harness() -> ExecutableBuffer {
    let mut buf = ExecutableBuffer::new(128).expect("buffer");
    emit_mov_reg(&mut buf, R10, ENTRY_ABI_REGS[0]);
    let jump = emit_megamorphic_gate(&mut buf);
    emit_mov_imm64(&mut buf, RAX, 1);
    buf.emit_byte(0xC3); // RET
    patch_rel32_to_here(&mut buf, jump);
    emit_mov_imm64(&mut buf, RAX, 2);
    buf.emit_byte(0xC3); // RET
    assert!(!buf.overflowed());
    buf.finalize();
    buf
}

fn gate(harness: &ExecutableBuffer, pic: &JitPICSlot) -> u64 {
    // SAFETY: `gate_harness` finalized a complete
    // `extern "C" fn(u64) -> u64` that reads one dword of `pic`.
    let f = unsafe {
        std::mem::transmute::<*const u8, extern "C" fn(u64) -> u64>(harness.as_ptr())
    };
    f(pic as *const JitPICSlot as u64)
}

/// The gate falls through until a fifth live receiver class overflows the
/// four inline ways, and jumps from then on. Re-publishing a receiver the
/// ways hold, or one whose way was retired for a recompiled callee, is not
/// an overflow.
#[test]
fn the_megamorphic_gate_jumps_only_once_the_site_overflowed() {
    let harness = gate_harness();
    let pic = Box::new(JitPICSlot::new());
    assert_eq!(gate(&harness, &pic), 1, "a fresh site");
    // Raw, even entry words: never called, only published.
    let word = |c: u32| 0x1000 + (u64::from(c) << 4);
    for c in 1..=4u32 {
        pic.install(c, "C", word(c), false, false);
        assert_eq!(gate(&harness, &pic), 1, "{c} receivers fit the ways");
    }
    pic.install(2, "C", word(2), false, false);
    assert_eq!(gate(&harness, &pic), 1, "a receiver the ways hold");
    // Class 3's callee recompiled: its way retires, and a way that is
    // retired (graced or not) is never a fifth live class.
    pic.install(3, "C", word(3) + 0x10_0000, false, false);
    assert_eq!(
        gate(&harness, &pic),
        1,
        "a retired way is not a fifth class"
    );
    assert!(!pic.is_megamorphic());

    let site = Box::new(JitPICSlot::new());
    for c in 1..=5u32 {
        site.install(c, "C", word(c), false, false);
    }
    assert!(site.is_megamorphic());
    assert_eq!(
        gate(&harness, &site),
        2,
        "the fifth class overflowed the ways"
    );
    site.clear_entries();
    assert_eq!(gate(&harness, &site), 2, "the state is sticky");
    pic.clear_entries();
}

/// Five receiver classes whose per-site hashed sets are pairwise distinct,
/// so each has a way of its own in the stub's table.
fn five_spread_classes() -> Vec<u32> {
    let mut seen = std::collections::HashSet::new();
    (1u32..10_000)
        .filter(|&c| seen.insert(JitPICSlot::mega_base_index(c)))
        .take(5)
        .collect()
}

/// W5-6, as machine code sees it: at a site marked as probing its inline
/// ways, a receiver the inline ways take does not also take a hashed way,
/// so the hashed stub misses it; the fifth receiver overflows into the
/// hashed ways, turns the site megamorphic and promotes the four inline
/// receivers into hashed ways, which the stub (the gate's target) then
/// finds. An unmarked site keeps the historical duplicates.
#[test]
fn an_inline_first_site_promotes_its_ways_when_it_overflows() {
    let cache = JitCache::new();
    let plain = publish(&cache, "cf-inline", body(ENTRY_ABI_REGS[1], 0x400));
    let p = plain.entry_ptr() as u64;
    let classes = five_spread_classes();
    assert_eq!(classes.len(), 5);
    let objs: Vec<Box<Obj>> = classes.iter().map(|&c| Obj::new(c)).collect();

    let marked = Box::new(JitPICSlot::new());
    marked.note_inline_ways_probed();
    let unmarked = Box::new(JitPICSlot::new());
    // Only the site's own hashed ways: no shared table, no cells.
    let marked_stub = build_with(&marked, false, false);
    let unmarked_stub = build_with(&unmarked, false, false);
    for &c in &classes[..4] {
        marked.install(c, "C", p, false, false);
        unmarked.install(c, "C", p, false, false);
    }
    for obj in &objs[..4] {
        assert_eq!(
            run(&marked_stub, obj.addr()),
            MISS,
            "inline only, no duplicate"
        );
        assert_eq!(
            run(&unmarked_stub, obj.addr()),
            ARG ^ 0x400,
            "the historical duplicate"
        );
    }
    assert_eq!(marked.entries_used(), 4);
    assert!(!marked.is_megamorphic());

    marked.install(classes[4], "C", p, false, false);
    assert!(marked.is_megamorphic(), "the fifth class overflowed");
    for obj in &objs {
        assert_eq!(
            run(&marked_stub, obj.addr()),
            ARG ^ 0x400,
            "promoted or overflowed"
        );
    }
    drop((marked_stub, unmarked_stub));
    marked.clear_entries();
    unmarked.clear_entries();
    crate::defer_jit_owner(Some(plain));
}
