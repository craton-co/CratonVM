//! JIT round 21 wave 4, lane ic (IC21-2): the vtable-column probe with the
//! site's binding baked, EXECUTED against the real `MegaDispatchTable`. It
//! must answer exactly what the loaded probe answers for a slot bound to
//! the same pair, with EAX = the class id on every miss.
use super::r12w3_mega2_table_probe_tests::{body, publish};
use super::*;
use crate::{JitCache, MegaDispatchTable};

const MISSED: u64 = 1 << 63;

/// `extern "C" fn(pic, class_id) -> u64`, as the r17 column harness; the
/// probe is the baked one when `binding` is given, the loaded one else.
fn harness(binding: Option<(usize, u32, u32)>) -> ExecutableBuffer {
    let mut buf = ExecutableBuffer::new(512).expect("buffer");
    emit_mov_reg(&mut buf, R10, ENTRY_ABI_REGS[0]);
    emit_mov_reg(&mut buf, RAX, ENTRY_ABI_REGS[1]);
    let misses = match binding {
        Some(binding) => emit_mega_vtable_column_probe_baked(&mut buf, binding),
        None => emit_mega_vtable_column_probe(&mut buf),
    };
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
    // u64` that reads `pic` (loaded form only) and the table rows, and
    // calls nothing.
    let f = unsafe {
        std::mem::transmute::<*const u8, extern "C" fn(u64, u64) -> u64>(harness.as_ptr())
    };
    f(pic as *const JitPICSlot as u64, u64::from(class_id))
}

#[test]
fn the_baked_probe_has_its_argued_length() {
    let mut buf = ExecutableBuffer::new(256).expect("buffer");
    let misses = emit_mega_vtable_column_probe_baked(&mut buf, (0x1000, 7, crate::MEGA_CLASS_SLOT_MAX));
    assert!(!buf.overflowed(), "{:?}", buf.codegen_failure_reason());
    assert_eq!(buf.pos(), MEGA_VTABLE_COLUMN_BAKED_PROBE_BYTES);
    assert_eq!(misses.len(), 5);
}

#[test]
fn the_baked_probe_answers_what_the_loaded_one_does() {
    let cache = JitCache::new();
    let plain = publish(&cache, "bk-plain", body(ENTRY_ABI_REGS[1], 0x400));
    let table: &MegaDispatchTable = cache.mega_dispatch_table();
    let selector = table.selector_id("p/BK", "f", "(J)J", 0, 43, 0).expect("selector");
    let other = table.selector_id("p/BK", "g", "(J)J", 0, 43, 0).expect("selector");
    let pic = Box::new(JitPICSlot::new());
    assert_eq!(pic.baked_column_binding(), None, "an unbound slot bakes nothing");
    pic.bind_mega_dispatch(table, selector);
    pic.bind_mega_class_slot(table, 3);
    let binding = pic.baked_column_binding().expect("bound");
    assert_eq!(binding, (table.class_root_addr(), selector, 3));
    let baked = harness(Some(binding));
    let loaded = harness(None);
    let p = plain.entry_ptr() as u64;
    table.install_with_class_slot(7, selector, Some((3, 8)), p, false, false);
    table.install_with_class_slot(9, other, Some((3, 8)), p, false, false);
    table.install_with_class_slot(11, selector, Some((2, 2)), p, false, false);
    // Many classes, across a directory growth.
    for c in (20..900u32).step_by(7) {
        table.install_with_class_slot(c, selector, Some((3, 4)), p, true, false);
    }
    let mut classes: Vec<u32> = vec![7, 9, 11, 12, 1_000_000];
    classes.extend((20..900u32).step_by(7));
    for &c in &classes {
        let want = probe(&loaded, &pic, c);
        assert_eq!(probe(&baked, &pic, c), want, "class {c}");
    }
    assert_eq!(probe(&baked, &pic, 7), crate::jit_ic_entry_word(p, false));
    assert_eq!(probe(&baked, &pic, 9), MISSED | 9, "another selector's cell");
    assert_eq!(probe(&baked, &pic, 11), MISSED | 11, "column past the row");
    // A fresh slot does not matter to baked code: it reads no slot.
    let fresh = Box::new(JitPICSlot::new());
    assert_eq!(probe(&baked, &fresh, 7), crate::jit_ic_entry_word(p, false));
    table.retire_entry(p as usize);
    assert_eq!(probe(&baked, &pic, 7), MISSED | 7, "a retired cell was answered");
    drop(baked);
    drop(loaded);
    table.clear_entries();
    crate::defer_jit_owner(Some(plain));
}
