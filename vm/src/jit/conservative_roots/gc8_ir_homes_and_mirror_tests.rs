//! JIT round 19 proposal wave 8, lane gc8: GC719-1 (an optimizing-tier
//! activation's block locks read from its published homes, only where the
//! active map names them, all or nothing) and GC719-3 (the positive
//! innermost-mirror check), over synthetic heap-backed stack bands.
use super::*;

const SP_ID: u32 = 7;

/// A 32-word band, RBP at word 24: the safepoint id at `[rbp - 8]`, lock
/// objects at `[rbp - 16]` and `[rbp - 40]`.
fn band() -> (Box<[usize; 32]>, usize) {
    let mut words = Box::new([0usize; 32]);
    let rbp = words.as_ptr() as usize + 8 * 24;
    words[23] = SP_ID as usize;
    words[22] = 0x7200_0010;
    words[19] = 0x7200_0050;
    (words, rbp)
}

fn ir_artifact(
    maps: &[(u32, Vec<i16>)],
    homes: &[(u32, Vec<i32>)],
) -> cratonvm_jit::CompiledMethod {
    let mut buf = cratonvm_jit::ExecutableBuffer::new(64).expect("executable buffer");
    buf.emit_byte(0xC3);
    let mut cm = cratonvm_jit::CompiledMethod::new(buf);
    cm.used_ir_backend = true;
    cm.sp_id_slot_off = 8;
    cm.method_label = "p/I.blocks:()V".to_string();
    for (i, (pc, offs)) in maps.iter().enumerate() {
        let at = 0x10 + 8 * u32::try_from(i).expect("a test map index");
        let mut m = cratonvm_jit::OopMapEntry::new(at);
        m.bytecode_pc = *pc;
        m.frame_slot_offsets = offs.clone();
        cm.push_oop_map(m);
    }
    cm.ir_monitor_homes = homes
        .iter()
        .map(|(id, offs)| (*id, offs.clone().into_boxed_slice()))
        .collect::<Vec<_>>()
        .into_boxed_slice();
    cm
}

fn read_ir(cm: &cratonvm_jit::CompiledMethod, rbp: usize) -> Vec<usize> {
    ir_block_lock_words_at(cm, SP_ID, |off| {
        let active = active_maps(rbp, cm);
        if active.sp_id != Some(SP_ID) {
            return None;
        }
        reference_word_named_by_active_maps(rbp, cm, off, 0)
    })
}

#[test]
fn ir_homes_are_read_outermost_first_where_the_active_map_names_them() {
    let (_band, rbp) = band();
    let cm = ir_artifact(&[(SP_ID, vec![16, 40])], &[(SP_ID, vec![40, 16])]);
    assert_eq!(read_ir(&cm, rbp), vec![0x7200_0050, 0x7200_0010]);
    assert_eq!(cm.ir_monitor_homes_at(SP_ID), &[40, 16]);
    assert!(cm.ir_monitor_homes_at(SP_ID + 1).is_empty());
}

#[test]
fn ir_homes_are_all_or_nothing() {
    let (mut band, rbp) = band();
    // One home the active map does not name.
    let partial = ir_artifact(&[(SP_ID, vec![16])], &[(SP_ID, vec![40, 16])]);
    assert!(read_ir(&partial, rbp).is_empty());
    // No homes at the active id.
    let other = ir_artifact(&[(SP_ID, vec![16, 40])], &[(SP_ID + 1, vec![16])]);
    assert!(read_ir(&other, rbp).is_empty());
    // A null lock word.
    let cm = ir_artifact(&[(SP_ID, vec![16, 40])], &[(SP_ID, vec![40, 16])]);
    band[19] = 0;
    assert!(read_ir(&cm, rbp).is_empty());
    // A trace-only id in the slot names no map.
    band[19] = 0x7200_0050;
    band[23] = (SP_ID | 0x8000_0000) as usize;
    assert!(read_ir(&cm, rbp).is_empty());
}

/// The tier dispatch of `compiled_frame_block_lock_words`: an
/// optimizing-tier body is read only with `ir_homes`, and never without a
/// frame pointer; a single-pass body never reads the IR table.
#[test]
fn the_ir_table_is_read_only_for_an_ir_body_with_the_switch() {
    let cm = ir_artifact(&[(SP_ID, vec![16])], &[(SP_ID, vec![16])]);
    let mut f = ActiveCompiledFrame {
        inline_chain_shared: None,
        interp_depth: 0,
        chain_index: 0,
        label: std::sync::Arc::from("p/I.blocks:()V"),
        owner_class_id: 0,
        cm_ptr: &cm as *const cratonvm_jit::CompiledMethod as usize,
        bci: -1,
        inline_chain: Vec::new(),
        chain_exact: false,
        trap_site_exact: false,
        chain_poisoned: false,
        compile_cp_stamp: None,
        fast_throw_c2_tier: false,
        artifact_id: 0,
        frame_rbp: None,
    };
    assert!(compiled_frame_block_lock_words(&f, &cm, true).is_empty());
    let (_band, rbp) = band();
    f.frame_rbp = Some(rbp);
    assert!(compiled_frame_block_lock_words(&f, &cm, false).is_empty());
    let mut sp = ir_artifact(&[(SP_ID, vec![16])], &[(SP_ID, vec![16])]);
    sp.used_ir_backend = false;
    assert!(compiled_frame_block_lock_words(&f, &sp, true).is_empty());
}

const CM: usize = 0x5000_0000;
const OTHER: usize = 0x6000_0000;

fn owner(w: usize) -> Option<usize> {
    if (CM..CM + 0x1000).contains(&w) {
        Some(CM)
    } else if (OTHER..OTHER + 0x1000).contains(&w) {
        Some(OTHER)
    } else {
        None
    }
}

fn check(
    words: &[usize; 32],
    rbp: usize,
    floor: usize,
    plausible: impl Fn(usize) -> bool,
) -> MirrorLiveness {
    let base = words.as_ptr() as usize;
    innermost_mirror_liveness_with(
        rbp,
        floor,
        CM,
        |addr| words[(addr - base) / 8],
        owner,
        plausible,
    )
}

#[test]
fn a_frame_that_called_out_of_its_body_is_live() {
    let mut words = [0usize; 32];
    words[10] = CM + 0x40; // the helper call's return address into the body
    let base = words.as_ptr() as usize;
    let rbp = base + 8 * 24;
    assert_eq!(
        check(&words, rbp, base, |_| true),
        MirrorLiveness::CalledOut
    );
}

#[test]
fn a_frame_with_no_call_out_below_it_is_refused() {
    let mut words = [0usize; 32];
    // Another body's return address, a word above the RBP, a return
    // address whose bytes are no call, and one below the floor.
    words[10] = OTHER + 0x40;
    words[26] = CM + 0x40;
    words[12] = CM + 0x80;
    words[2] = CM + 0x48;
    let base = words.as_ptr() as usize;
    let rbp = base + 8 * 24;
    let floor = base + 8 * 4;
    assert_eq!(
        check(&words, rbp, floor, |w| w != CM + 0x80),
        MirrorLiveness::NoCallOut
    );
    // The floor lowered: the word at index 2 is found.
    assert_eq!(
        check(&words, rbp, base, |w| w != CM + 0x80),
        MirrorLiveness::CalledOut
    );
}

#[test]
fn an_unusable_mirror_is_out_of_band() {
    let words = [0usize; 32];
    let base = words.as_ptr() as usize;
    assert_eq!(
        check(&words, base + 8 * 24 + 4, base, |_| true),
        MirrorLiveness::OutOfBand
    );
    assert_eq!(
        check(&words, base, base, |_| true),
        MirrorLiveness::OutOfBand
    );
    assert_eq!(
        innermost_mirror_liveness_with(base + 64, base, 0, |_| 0, owner, |_| true),
        MirrorLiveness::OutOfBand
    );
}
