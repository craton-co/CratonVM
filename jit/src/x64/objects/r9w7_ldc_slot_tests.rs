use super::*;

/// What the miss arm of the test snippet loads, so a miss is visible.
const MISS: i64 = 0x5EED_0BAD;

/// Emit `emit_ldc_slot_probe(slot)` inside a tiny frame, with the miss
/// arm loading [`MISS`], and return RAX. This is the fast path
/// `emit_ldc_cp_site` puts in front of the helper call.
fn run_probe(slot: &std::sync::atomic::AtomicU64) -> i64 {
    let mut c = crate::x64::flag_and_header_contracts::bounds_check_test_compiler();
    let entry_off = c.buf.pos();
    c.buf.emit(&[0x55]); // push rbp
    c.buf.emit(&[0x48, 0x89, 0xE5]); // mov rbp, rsp
    let done = c.emit_ldc_slot_probe(slot as *const std::sync::atomic::AtomicU64 as usize);
    c.emit_mov_imm64(RAX, MISS);
    c.patch_rel32_to_here(done);
    c.buf.emit(&[0xC9, 0xC3]); // leave; ret
    assert!(!c.buf.overflowed(), "the test buffer must hold the snippet");
    assert!(c.buf.codegen_failure_reason().is_none());
    crate::platform::make_executable(c.buf.as_ptr() as *mut u8, c.buf.capacity())
        .expect("the test buffer must be flippable to RX");
    // SAFETY: `entry_off` is a byte offset inside the same allocation.
    let entry = unsafe { c.buf.as_ptr().add(entry_off) };
    // SAFETY: the snippet builds and tears down its own frame, reads one
    // aligned word through `slot` (alive across the call), calls nothing
    // and returns an i64. `c` owns the buffer and outlives the call.
    let f: extern "C" fn() -> i64 = unsafe { std::mem::transmute(entry) };
    let r = f();
    drop(c);
    r
}

/// Round 9 wave 7 (`perf-compiled-ldc-string-is-a-helper-call-per-execution`):
/// a filled slot answers the `ldc` with no call, and the answer is read
/// THROUGH the slot on every execution, so a collector that rewrites the
/// slot in place is followed. An empty slot falls to the helper arm.
#[test]
fn the_ldc_slot_probe_reads_through_the_slot_and_misses_on_zero() {
    let slot = std::sync::atomic::AtomicU64::new(0x0000_7F00_1234_5670);
    assert_eq!(run_probe(&slot), 0x0000_7F00_1234_5670);
    // The collector moved the literal: the same code answers the new word.
    slot.store(0x0000_7F00_8765_4320, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(run_probe(&slot), 0x0000_7F00_8765_4320);
    slot.store(0, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(
        run_probe(&slot),
        MISS,
        "an empty slot must take the helper arm"
    );
}

/// With a slot, the site starts with the probe (so the shadow-stack push in
/// the spill is on the helper arm only), still pushes exactly one oop, and
/// leaves no operand in a scratch register (the two arms meet with one
/// stack model). With no slot, no probe is emitted.
#[test]
fn a_slotted_ldc_site_probes_first_and_pushes_one_oop() {
    // Any in-reach address: the snippet is emitted, never executed.
    let helper = bounds_check_probe_target as *const () as usize;
    let fake_slot: usize = 0x7FFF_1234_5678_0000;
    for slot in [fake_slot, 0] {
        let mut c = crate::x64::flag_and_header_contracts::bounds_check_test_compiler();
        c.heap_local_offset = 8;
        let start = c.buf.pos();
        let depth = c.stack.len();
        c.emit_ldc_cp_site(helper, 0x0123, 7, slot);
        assert!(c.buf.codegen_failure_reason().is_none());
        assert_eq!(c.stack.len(), depth + 1, "slot={slot:#x}: one push");
        assert_eq!(
            c.stack_oop_marks.last(),
            Some(&true),
            "slot={slot:#x}: an oop"
        );
        assert!(
            !c.stack.iter().any(|s| matches!(s, StackSlot::Scratch(_))),
            "slot={slot:#x}: no operand may stay in a scratch register across the join"
        );
        // SAFETY: `start..start + 10` was just written by this compiler.
        let head = unsafe { std::slice::from_raw_parts(c.buf.as_ptr().add(start), 10) };
        let mut movabs = vec![0x48u8, 0xB8];
        movabs.extend_from_slice(&(fake_slot as u64).to_le_bytes());
        if slot != 0 {
            assert_eq!(head, &movabs[..], "the probe comes before anything else");
        } else {
            assert_ne!(head, &movabs[..], "no slot, no probe");
        }
    }
}

// SAFETY: never called; only its address is baked into unexecuted code.
unsafe extern "C" fn bounds_check_probe_target(_vm: i64, _holder: i64, _cp: i64) -> i64 {
    0
}
