//! JIT round 17 wave 1, lane mega (MW13-4): the inline vtable-column
//! probe, EXECUTED on this host's ABI against the real
//! `MegaDispatchTable` class rows and directory. It must hit exactly when
//! the Rust mirror finds the site's cell, hand back the cell's TAGGED
//! word, and leave EAX = the class id on every miss (the stub gate
//! entry's contract).
use super::r12w3_mega2_table_probe_tests::{body, publish};
use super::*;
use crate::{JitCache, MegaDispatchTable};

/// Set on the harness's answer when the probe missed.
const MISSED: u64 = 1 << 63;

/// `extern "C" fn(pic, class_id) -> u64`: R10 = the slot, RAX = the
/// class id, the probe; the cell's tagged word on its hit, `MISSED | EAX`
/// on every miss.
fn harness() -> ExecutableBuffer {
    let mut buf = ExecutableBuffer::new(512).expect("buffer");
    emit_mov_reg(&mut buf, R10, ENTRY_ABI_REGS[0]);
    emit_mov_reg(&mut buf, RAX, ENTRY_ABI_REGS[1]);
    let misses = emit_mega_vtable_column_probe(&mut buf);
    assert_eq!(misses.len(), 7);
    emit_mov_reg(&mut buf, RAX, R11);
    buf.emit_byte(0xC3); // RET
    for miss in misses {
        patch_rel32_to_here(&mut buf, miss);
    }
    buf.emit(&[0x89, 0xC0]); // MOV EAX, EAX
    buf.emit(&[0x48, 0x0F, 0xBA, 0xE8, 0x3F]); // BTS RAX, 63
    buf.emit_byte(0xC3); // RET
    assert!(!buf.overflowed(), "{:?}", buf.codegen_failure_reason());
    buf.finalize();
    buf
}

fn probe(harness: &ExecutableBuffer, pic: &JitPICSlot, class_id: u32) -> u64 {
    // SAFETY: `harness` finalized a complete `extern "C" fn(u64, u64) ->
    // u64` that reads `pic` and the table rows it names, and calls nothing.
    let f = unsafe {
        std::mem::transmute::<*const u8, extern "C" fn(u64, u64) -> u64>(harness.as_ptr())
    };
    f(pic as *const JitPICSlot as u64, u64::from(class_id))
}

fn bound_pic(table: &MegaDispatchTable, selector: u32, column: u32) -> Box<JitPICSlot> {
    let pic = Box::new(JitPICSlot::new());
    pic.bind_mega_dispatch(table, selector);
    pic.bind_mega_class_slot(table, column);
    pic
}

#[test]
fn the_probe_has_its_argued_length() {
    let mut buf = ExecutableBuffer::new(256).expect("buffer");
    let misses = emit_mega_vtable_column_probe(&mut buf);
    assert!(!buf.overflowed(), "{:?}", buf.codegen_failure_reason());
    assert_eq!(buf.pos(), MEGA_VTABLE_COLUMN_PROBE_BYTES);
    assert_eq!(misses.len(), 7);
}

#[test]
fn a_cell_is_found_by_its_column_and_only_by_its_key() {
    let cache = JitCache::new();
    let plain = publish(&cache, "vc-plain", body(ENTRY_ABI_REGS[1], 0x100));
    let ctx = publish(&cache, "vc-ctx", body(ENTRY_ABI_REGS[2], 0x200));
    let table = cache.mega_dispatch_table();
    let selector = table
        .selector_id("p/VC", "f", "(J)J", 0, 41, 0)
        .expect("selector");
    let other = table
        .selector_id("p/VC", "g", "(J)J", 0, 41, 0)
        .expect("selector");
    let pic = bound_pic(table, selector, 3);
    let code = harness();
    let (p, c) = (plain.entry_ptr() as u64, ctx.entry_ptr() as u64);

    assert_eq!(probe(&code, &pic, 7), MISSED | 7, "nothing published");
    table.install_with_class_slot(7, selector, Some((3, 8)), p, false, false);
    assert_eq!(probe(&code, &pic, 7), crate::jit_ic_entry_word(p, false));
    table.install_with_class_slot(9, selector, Some((3, 8)), c, true, false);
    assert_eq!(
        probe(&code, &pic, 9),
        crate::jit_ic_entry_word(c, true),
        "the word comes back tagged"
    );
    // Another selector's cell at this site's column is not this site's.
    table.install_with_class_slot(10, other, Some((3, 8)), p, false, false);
    assert_eq!(
        probe(&code, &pic, 10),
        MISSED | 10,
        "a shared column matched another key"
    );
    // The column past the class's row.
    table.install_with_class_slot(11, selector, Some((2, 2)), p, false, false);
    assert_eq!(
        probe(&code, &pic, 11),
        MISSED | 11,
        "column 3 of a 2-column row"
    );
    // A class past the directory.
    assert_eq!(probe(&code, &pic, 1_000_000), MISSED | 1_000_000);
    // An unbound site, and one bound at another column.
    let unbound = Box::new(JitPICSlot::new());
    unbound.bind_mega_dispatch(table, selector);
    assert_eq!(probe(&code, &unbound, 7), MISSED | 7);
    let fresh = Box::new(JitPICSlot::new());
    assert_eq!(probe(&code, &fresh, 7), MISSED | 7, "no selector, no root");
    let elsewhere = bound_pic(table, selector, 4);
    assert_eq!(probe(&code, &elsewhere, 7), MISSED | 7);

    // Retired cells (key first, then word) are never answered as live.
    table.retire_entry(plain.entry_ptr() as usize);
    assert_eq!(
        probe(&code, &pic, 7),
        MISSED | 7,
        "a retired cell was answered"
    );
    assert_eq!(probe(&code, &pic, 9), crate::jit_ic_entry_word(c, true));
    table.clear_entries();
    assert_eq!(
        probe(&code, &pic, 9),
        MISSED | 9,
        "a cleared table answered"
    );

    drop(code);
    for b in [plain, ctx] {
        crate::defer_jit_owner(Some(b));
    }
}

/// For every class, across directory growths, the inline probe answers
/// exactly what the Rust mirror of the cell does.
#[test]
fn the_inline_probe_agrees_with_the_rust_mirror() {
    let cache = JitCache::new();
    let plain = publish(&cache, "vc-agree", body(ENTRY_ABI_REGS[1], 0x300));
    let table = cache.mega_dispatch_table();
    let selector = table
        .selector_id("p/VD", "h", "(J)J", 0, 42, 0)
        .expect("selector");
    let pic = bound_pic(table, selector, 2);
    let code = harness();
    let classes: Vec<u32> = (1..1200).step_by(3).collect();
    for &c in &classes {
        // Every fifth class gets a one-column row: column 2 is past it.
        let (column, columns) = if c % 5 == 0 { (1, 1) } else { (2, 4) };
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
        let expect = match table.lookup_class_cell(c, selector, 2) {
            Some((entry, ctx)) => crate::jit_ic_entry_word(entry, ctx),
            None => MISSED | u64::from(c),
        };
        assert_eq!(probe(&code, &pic, c), expect, "class {c}");
    }
    drop(code);
    table.clear_entries();
    crate::defer_jit_owner(Some(plain));
}
