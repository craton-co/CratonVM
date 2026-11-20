//! Round 12 wave 4, lane mega3 (proposal M3-1): the hashed stub's
//! class-slot probe, EXECUTED on this host's ABI (Win64 or SysV), against
//! the real `MegaDispatchTable` class arrays and directory.
use super::r12w3_mega2_table_probe_tests::{body, build_with, publish, run, Obj, ARG, MISS};
use super::*;
use crate::{JitCache, MegaDispatchTable, MEGA_DISPATCH_WAYS};

/// Fill `class_id`'s hashed set for `selector` with other classes, so a
/// later publication for `class_id` lands in its class cell ONLY and a
/// hit can only have come from the class-slot probe. The fillers are all
/// above 1000, so they are never a class the tests below drive.
fn crowd_out(table: &MegaDispatchTable, selector: u32, class_id: u32, filler: u64) {
    assert!(class_id <= 1000);
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

fn bound_pic(table: &MegaDispatchTable, selector: u32, column: u32) -> Box<JitPICSlot> {
    let pic = Box::new(JitPICSlot::new());
    pic.bind_mega_dispatch(table, selector);
    pic.bind_mega_class_slot(table, column);
    pic
}

#[test]
fn a_class_cell_is_called_from_machine_code() {
    let cache = JitCache::new();
    let plain = publish(&cache, "cs-plain", body(ENTRY_ABI_REGS[1], 0x100));
    let ctx = publish(&cache, "cs-ctx", body(ENTRY_ABI_REGS[2], 0x200));
    let filler = publish(&cache, "cs-filler", body(ENTRY_ABI_REGS[1], 0x7));
    let table = cache.mega_dispatch_table();
    let selector = table
        .selector_id("p/V", "f", "(J)J", 0, 21, 0)
        .expect("selector");
    let other = table
        .selector_id("p/V", "g", "(J)J", 0, 21, 0)
        .expect("selector");
    let pic = bound_pic(table, selector, 3);
    let stub = build_with(&pic, true, true);
    let without = build_with(&pic, true, false);
    let fill = filler.entry_ptr() as u64;
    let (p, c) = (plain.entry_ptr() as u64, ctx.entry_ptr() as u64);

    let seven = Obj::new(7);
    crowd_out(table, selector, 7, fill);
    assert_eq!(run(&stub, seven.addr()), MISS);
    table.install_with_class_slot(7, selector, Some((3, 8)), p, false, false);
    assert!(
        table.lookup(7, selector).is_none(),
        "the hashed set is full"
    );
    assert_eq!(
        run(&stub, seven.addr()),
        ARG ^ 0x100,
        "the class cell is called"
    );
    assert_eq!(
        run(&without, seven.addr()),
        MISS,
        "only the class probe reads it"
    );

    // A context-tagged cell: the context goes first, the argument third.
    let nine = Obj::new(9);
    crowd_out(table, selector, 9, fill);
    table.install_with_class_slot(9, selector, Some((3, 8)), c, true, false);
    assert_eq!(
        run(&stub, nine.addr()),
        ARG ^ 0x200,
        "the tag selects the ABI"
    );

    // Another selector's cell at this site's column is not this site's.
    let ten = Obj::new(10);
    crowd_out(table, selector, 10, fill);
    table.install_with_class_slot(10, other, Some((3, 8)), p, false, false);
    assert_eq!(
        run(&stub, ten.addr()),
        MISS,
        "a shared column matched another key"
    );

    // The site's column past the class's array: the bound refuses it.
    let eleven = Obj::new(11);
    crowd_out(table, selector, 11, fill);
    table.install_with_class_slot(11, selector, Some((2, 2)), p, false, false);
    assert!(table.lookup_class_cell(11, selector, 2).is_some());
    assert_eq!(
        run(&stub, eleven.addr()),
        MISS,
        "column 3 of a 2-column array"
    );

    // A class the directory does not cover, and one with no array.
    assert_eq!(run(&stub, Obj::new(1_000_000).addr()), MISS);
    // (A class inside the directory with no array: not one of the
    // fillers, which hold hashed ways for this selector.)
    let lonely = (13u32..256)
        .find(|&c| table.lookup(c, selector).is_none())
        .expect("a class with no way");
    assert_eq!(run(&stub, Obj::new(lonely).addr()), MISS);

    // Null and non-object receivers never reach the probe.
    assert_eq!(run(&stub, 0), MISS);
    let mut array = Obj::new(7);
    const _: () = assert!(cratonvm_types::KIND_TAGS_BYTE_OFFSET < 8);
    array.words[0] |= u64::from(cratonvm_types::KIND_TAG_BYTE_MASK)
        << (8 * cratonvm_types::KIND_TAGS_BYTE_OFFSET);
    assert_eq!(
        run(&stub, array.addr()),
        MISS,
        "an array never matches a class cell"
    );

    // A site with no column (or another one) reads no cell.
    let unbound = Box::new(JitPICSlot::new());
    unbound.bind_mega_dispatch(table, selector);
    let unbound_stub = build_with(&unbound, true, true);
    assert_eq!(run(&unbound_stub, seven.addr()), MISS);
    let elsewhere = bound_pic(table, selector, 4);
    let elsewhere_stub = build_with(&elsewhere, true, true);
    assert_eq!(run(&elsewhere_stub, seven.addr()), MISS);

    // Retired cells (key first, then word) are never probed as live.
    table.retire_entry(plain.entry_ptr() as usize);
    assert_eq!(
        run(&stub, seven.addr()),
        MISS,
        "a retired cell was probed as live"
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
        "an invalidated cell was probed as live"
    );

    table.clear_entries();
    drop((stub, without, unbound_stub, elsewhere_stub));
    for b in [plain, ctx, filler] {
        crate::defer_jit_owner(Some(b));
    }
}

/// For every class, across two directory growths, the stub hits exactly
/// when the Rust mirror finds a cell or a hashed way for it.
#[test]
fn the_machine_class_probe_agrees_with_the_rust_mirror() {
    let cache = JitCache::new();
    let plain = publish(&cache, "cs-agree", body(ENTRY_ABI_REGS[1], 0x300));
    let table = cache.mega_dispatch_table();
    let selector = table
        .selector_id("p/W", "h", "(J)J", 0, 22, 0)
        .expect("selector");
    let pic = bound_pic(table, selector, 2);
    let stub = build_with(&pic, true, true);
    let classes: Vec<u32> = (1..1200).step_by(3).collect();
    for &c in &classes {
        // Every fifth class gets a one-column array: column 2 is past it.
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
        assert_eq!(run(&stub, Obj::new(c).addr()), expect, "class {c}");
    }
    table.clear_entries();
    for &c in &classes {
        assert_eq!(
            run(&stub, Obj::new(c).addr()),
            MISS,
            "class {c} after the clear"
        );
    }
    drop(stub);
    crate::defer_jit_owner(Some(plain));
}


/// The probe's encoding length includes its rel32 branches, and the
/// switch-off stub carries none of it.
#[test]
fn the_probe_has_its_argued_length_and_is_absent_when_off() {
    let mut buf = ExecutableBuffer::new(2048).expect("buffer");
    let tail = buf.pos();
    assert_eq!(emit_mega_class_slot_probe(&mut buf, Some(tail)), None);
    assert!(!buf.overflowed(), "{:?}", buf.codegen_failure_reason());
    let probe_bytes = buf.pos() - tail;
    assert!(probe_bytes > 85);
    // The forward form (the cell-first order) is the same length.
    let start = buf.pos();
    let hit = emit_mega_class_slot_probe(&mut buf, None).expect("a forward hit exit");
    assert!(!buf.overflowed(), "{:?}", buf.codegen_failure_reason());
    assert_eq!(buf.pos() - start, probe_bytes);
    assert_eq!(hit, buf.pos() - 4, "the hit is the probe's last rel32");

    let pic = Box::new(JitPICSlot::new());
    let mut load = vec![0x41, 0x8B, 0x8A];
    load.extend_from_slice(&(JitPICSlot::MEGA_CLASS_SLOT_OFFSET as i32).to_le_bytes());
    let on = build_with(&pic, true, true);
    let off = build_with(&pic, true, false);
    let bare = build_with(&pic, false, true);
    assert!(on.as_slice().windows(load.len()).any(|w| w == load));
    assert!(!off.as_slice().windows(load.len()).any(|w| w == load));
    assert!(!bare.as_slice().windows(load.len()).any(|w| w == load));
    assert_eq!(on.pos(), off.pos() + probe_bytes);
    drop((buf, on, off, bare));
}
