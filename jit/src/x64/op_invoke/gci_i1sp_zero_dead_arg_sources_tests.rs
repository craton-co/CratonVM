//! gci i1/sp: `CRATONVM_JIT_SP_ZERO_DEAD_ARG_SOURCES` (g5jm's holder).
use super::dead_arg_source_offsets;
use crate::x64::StackSlot;

/// g5jm's shape at `sp=55`: `new WeakReference<>(chainHead)`, the `dup`
/// at 72 and the marker at 80 popped from a cursor of 88 to 72, with the
/// `new` result left at 64. Both popped reference sources are zeroed.
#[test]
fn gci_i1sp_the_g5jm_sources_are_the_reclaimed_reference_slots() {
    let slots = [StackSlot::Frame(72), StackSlot::Frame(80)];
    assert_eq!(
        dead_arg_source_offsets(&slots, &[true, true], 64, 72, 88),
        vec![72, 80]
    );
}

/// A primitive argument, a register-resident one and a slot outside the
/// reclaimed range (a deeper word an aliasing operand still owns, a word
/// below the operand band) are never zeroed; two arguments aliasing one
/// word give it once; nothing reclaimed means nothing zeroed.
#[test]
fn gci_i1sp_only_reclaimed_reference_words_in_the_band_qualify() {
    let slots = [
        StackSlot::Frame(64),
        StackSlot::Frame(80),
        StackSlot::Frame(72),
        StackSlot::CalleeSaved(12),
        StackSlot::Frame(72),
    ];
    let oops = [true, false, true, true, true];
    assert_eq!(dead_arg_source_offsets(&slots, &oops, 64, 72, 88), vec![72]);
    assert_eq!(
        dead_arg_source_offsets(&slots, &oops, 80, 72, 88),
        Vec::<i32>::new()
    );
    assert_eq!(
        dead_arg_source_offsets(&slots, &oops, 64, 88, 88),
        Vec::<i32>::new()
    );
    assert_eq!(
        dead_arg_source_offsets(&slots, &oops, 64, 96, 88),
        Vec::<i32>::new()
    );
    assert_eq!(
        dead_arg_source_offsets(&[], &[], 64, 72, 88),
        Vec::<i32>::new()
    );
}

/// The store is `MOV qword [rbp - off], 0`: disp8 for a near word, disp32
/// for a far one, the zero immediate last, and it drops the slot mirror.
#[test]
fn gci_i1sp_the_zero_store_encodes_mov_rbp_disp_imm32_zero() {
    let mut c = super::super::emit::tests::test_compiler();
    let start = c.buf.pos();
    c.emit_zero_frame_word(80);
    assert_eq!(
        &c.buf.as_slice()[start..c.buf.pos()],
        &[0x48, 0xC7, 0x45, 0xB0, 0, 0, 0, 0][..]
    );
    assert!(c.slot_mirror.is_none());
    let start = c.buf.pos();
    c.emit_zero_frame_word(200);
    let mut far = vec![0x48, 0xC7, 0x85];
    far.extend_from_slice(&(-200i32).to_le_bytes());
    far.extend_from_slice(&[0, 0, 0, 0]);
    assert_eq!(&c.buf.as_slice()[start..c.buf.pos()], &far[..]);
}

/// The call arms that stage their arguments elsewhere -- the two
/// dispatch-helper arms, the two direct-call arms and (gci i2/sp) the
/// cycle-edge cell call and the bridged `invokedynamic` -- each ask once.
#[test]
fn gci_i1sp_every_staging_arm_offers_the_zeroing() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("x64/op_invoke");
    let mut count = 0usize;
    for entry in std::fs::read_dir(&dir).expect("op_invoke dir").flatten() {
        let p = entry.path();
        if p.extension().is_some_and(|e| e == "rs")
            && p.file_name().is_some_and(|n| {
                let s = n.to_string_lossy();
                s != "tests.rs" && !s.ends_with("_tests.rs")
            })
        {
            let text = std::fs::read_to_string(&p).expect("source is UTF-8");
            let production = text.split("#[cfg(test)]").next().unwrap_or("");
            count += production
                .matches("self.emit_zero_dead_arg_sources(")
                .count();
        }
    }
    assert_eq!(count, 6);
}
