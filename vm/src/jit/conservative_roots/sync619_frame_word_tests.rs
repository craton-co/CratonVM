//! JIT round 19 proposal wave 7, lane gc7 (SYNC619-1): the compiled walk
//! publishes each activation's frame pointer, and
//! [`reference_word_named_by_active_maps`] reads a frame word only where
//! the frame's ACTIVE oop maps name it -- the precise root the collector
//! rewrites -- over a synthetic stack band.
use super::*;

const SP_ID: u32 = 5;
const OBJ: usize = 0x7100_0040;

/// A 32-word band, the frame's RBP at word 24: its safepoint id at
/// `[rbp - 8]`, a reference word at `[rbp - 16]`, an unnamed word at
/// `[rbp - 24]`, and a saved RBP / return address of zero (the boundary).
fn band() -> (Box<[usize; 32]>, usize) {
    let mut words = Box::new([0usize; 32]);
    let base = words.as_ptr() as usize;
    let rbp = base + 8 * 24;
    words[23] = SP_ID as usize;
    words[22] = OBJ;
    words[21] = 0x7100_0080;
    (words, rbp)
}

fn artifact(maps: &[(u32, Vec<i16>)]) -> cratonvm_jit::CompiledMethod {
    let mut buf = cratonvm_jit::ExecutableBuffer::new(64).expect("executable buffer");
    buf.emit_byte(0xC3);
    let mut cm = cratonvm_jit::CompiledMethod::new(buf);
    cm.sp_id_slot_off = 8;
    cm.method_label = "p/S.held:()V".to_string();
    for (i, (pc, offs)) in maps.iter().enumerate() {
        let at = 0x10 + 8 * u32::try_from(i).expect("a test map index");
        let mut m = cratonvm_jit::OopMapEntry::new(at);
        m.bytecode_pc = *pc;
        m.frame_slot_offsets = offs.clone();
        cm.push_oop_map(m);
    }
    cm
}

#[test]
fn a_word_every_active_map_names_is_read() {
    let (_band, rbp) = band();
    let cm = artifact(&[(SP_ID, vec![16]), (SP_ID, vec![16, 24]), (9, vec![])]);
    assert_eq!(
        reference_word_named_by_active_maps(rbp, &cm, 16, 0),
        Some(OBJ)
    );
}

#[test]
fn every_unproven_word_is_refused() {
    let (mut band, rbp) = band();
    // Not named by the active map (a map of another id names it).
    let other = artifact(&[(SP_ID, vec![24]), (9, vec![16])]);
    assert_eq!(
        reference_word_named_by_active_maps(rbp, &other, 16, 0),
        None
    );
    // Named by one active map, not by the other.
    let split = artifact(&[(SP_ID, vec![16]), (SP_ID, vec![24])]);
    assert_eq!(
        reference_word_named_by_active_maps(rbp, &split, 16, 0),
        None
    );
    let cm = artifact(&[(SP_ID, vec![16])]);
    // Unaligned, zero, negative, beyond i16.
    for off in [12, 0, -16, 1 << 20] {
        assert_eq!(
            reference_word_named_by_active_maps(rbp, &cm, off, 0),
            None,
            "{off}"
        );
    }
    // Below the reader's stack floor.
    assert_eq!(
        reference_word_named_by_active_maps(rbp, &cm, 16, rbp - 8),
        None
    );
    // An unaligned RBP.
    assert_eq!(
        reference_word_named_by_active_maps(rbp + 4, &cm, 16, 0),
        None
    );
    // A frame that holds no references publishes no active id.
    let mut none = artifact(&[(SP_ID, vec![16])]);
    none.frame_holds_no_references = true;
    assert_eq!(reference_word_named_by_active_maps(rbp, &none, 16, 0), None);
    // A trace-only id (a post-call stub running a helper) matches no map.
    band[23] = (SP_ID | 0x8000_0000) as usize;
    assert_eq!(reference_word_named_by_active_maps(rbp, &cm, 16, 0), None);
    // An id no map is recorded at.
    band[23] = 6;
    assert_eq!(reference_word_named_by_active_maps(rbp, &cm, 16, 0), None);
    // A null word.
    band[23] = SP_ID as usize;
    band[22] = 0;
    assert_eq!(reference_word_named_by_active_maps(rbp, &cm, 16, 0), None);
}

/// The blocks a body describes at the active id are read from their
/// locals' homes, outermost first, all or nothing.
#[test]
fn block_locks_are_read_from_named_local_homes_all_or_nothing() {
    let (mut band, rbp) = band();
    band[20] = 0x7100_00c0; // local 3's home, 32 bytes below RBP
    let with = |maps: &[(u32, Vec<i16>)]| {
        let mut cm = artifact(maps);
        cm.taken_monitor_slots =
            vec![(SP_ID, vec![3u16, 2].into_boxed_slice())].into_boxed_slice();
        cm
    };
    let cm = with(&[(SP_ID, vec![16, 24, 32])]);
    let read = |cm: &cratonvm_jit::CompiledMethod| {
        block_lock_words_at(cm, SP_ID, false, |off| {
            reference_word_named_by_active_maps(rbp, cm, off, 0)
        })
    };
    assert_eq!(read(&cm), vec![0x7100_00c0, 0x7100_0080]);
    // An id the body described no block at.
    assert!(block_lock_words_at(&cm, 9, false, |_| Some(1)).is_empty());
    // One home the map does not name: nothing.
    let partial = with(&[(SP_ID, vec![16, 24])]);
    assert!(read(&partial).is_empty());
    // A lock's local held in a register: nothing.
    let mut reg = with(&[(SP_ID, vec![16, 24, 32])]);
    reg.osr_local_assignments = Some(vec![None, None, Some(3), None]);
    assert!(read(&reg).is_empty());
    // ... unless register homes are read (round 19 wave 9, lane gc9):
    // the canonical slot the map names, as for a frame-homed local.
    let homes = block_lock_words_at(&reg, SP_ID, true, |off| {
        reference_word_named_by_active_maps(rbp, &reg, off, 0)
    });
    assert_eq!(homes, vec![0x7100_00c0, 0x7100_0080]);
    // ... and still only where the map names the slot.
    let mut reg_partial = with(&[(SP_ID, vec![16, 32])]);
    reg_partial.osr_local_assignments = Some(vec![None, None, Some(3), None]);
    let none = block_lock_words_at(&reg_partial, SP_ID, true, |off| {
        reference_word_named_by_active_maps(rbp, &reg_partial, off, 0)
    });
    assert!(none.is_empty());
    // The lookup itself: sorted table, absent ids empty.
    let mut table = artifact(&[]);
    table.taken_monitor_slots = vec![
        (2, vec![0u16].into_boxed_slice()),
        (SP_ID, vec![1u16, 2].into_boxed_slice()),
        (40, vec![4u16].into_boxed_slice()),
    ]
    .into_boxed_slice();
    assert_eq!(table.taken_monitor_locals_at(SP_ID), &[1, 2]);
    assert_eq!(table.taken_monitor_locals_at(40), &[4]);
    assert!(table.taken_monitor_locals_at(3).is_empty());
}

fn entry(
    cm: &cratonvm_jit::CompiledMethod,
    exact_rbp: usize,
    entry_sp: usize,
) -> JitFrameChainEntry {
    JitFrameChainEntry {
        entry_sp,
        precise: Some(PreciseFrameInfo {
            compiled_method: cm as *const cratonvm_jit::CompiledMethod,
            frame_base: 0,
            entry_ptr: cm.entry_ptr(),
            exact_rbp,
            exact_cm_id: 0,
            not_entrant_ret_pc: 0,
        }),
        interp_depth: 2,
        depth_stripe: cratonvm_types::striped_counter::StripeToken::UNSET,
        exec_token: cratonvm_jit::JitExecutionToken::UNSET,
        tally_skipped: false,
    }
}

/// The walk reports the innermost activation with the RBP it read its
/// line from, and that RBP reads the named word; an out-of-band mirror
/// RBP is reported as no RBP, and nothing is read through it.
#[test]
fn the_walk_publishes_the_frame_pointer_it_read_and_none_out_of_band() {
    let (band, rbp) = band();
    let cm = artifact(&[(SP_ID, vec![16])]);
    let lo = band.as_ptr() as usize;
    let hi = lo + 8 * 32;
    let walk = walk_jit_chain(&[entry(&cm, rbp, hi)], lo, 0, usize::MAX, false, None, None);
    assert_eq!(walk.len(), 1);
    assert_eq!(walk[0].frame_rbp, Some(rbp));
    let read_rbp = walk[0].frame_rbp.expect("an RBP");
    assert_eq!(
        reference_word_named_by_active_maps(read_rbp, &cm, 16, 0),
        Some(OBJ)
    );

    // The mirror's RBP above the entry's stack pointer: out of band.
    let walk = walk_jit_chain(
        &[entry(&cm, rbp, rbp)],
        lo,
        0,
        usize::MAX,
        false,
        None,
        None,
    );
    assert_eq!(walk.len(), 1, "the boundary method is still reported");
    assert_eq!(walk[0].frame_rbp, None);
    assert_eq!(compiled_frame_reference_word(&walk[0], &cm, 16), None);
}

/// GC920-4 (round 19 wave 11, lane gc11): a HELD thread's activation is
/// read within its own band `[rbp - frame_size, rbp)`, never against the
/// reader's stack pointer; a body that published no frame size, or an
/// offset outside the band, reads nothing, and the map check still holds.
#[test]
fn a_held_threads_frame_word_is_read_within_its_own_band() {
    let (band, rbp) = band();
    let mut cm = artifact(&[(SP_ID, vec![16])]);
    let lo = band.as_ptr() as usize;
    let hi = lo + 8 * 32;
    let walk = walk_jit_chain(&[entry(&cm, rbp, hi)], lo, 0, usize::MAX, false, None, None);
    assert_eq!(walk.len(), 1);
    let held = FrameWordReader::HeldThread;
    // No published frame size: nothing.
    assert_eq!(held_thread_frame_floor(rbp, &cm, 16), None);
    assert_eq!(
        compiled_frame_reference_word_by(&walk[0], &cm, 16, held),
        None
    );
    cm.frame_layout.frame_size = 24;
    assert_eq!(held_thread_frame_floor(rbp, &cm, 16), Some(rbp - 24));
    assert_eq!(
        compiled_frame_reference_word_by(&walk[0], &cm, 16, held),
        Some(OBJ)
    );
    // Outside the band, or not a positive offset: no floor, no read.
    assert_eq!(held_thread_frame_floor(rbp, &cm, 32), None);
    assert_eq!(held_thread_frame_floor(rbp, &cm, 0), None);
    assert_eq!(
        compiled_frame_reference_word_by(&walk[0], &cm, 32, held),
        None
    );
    // Inside the band but unnamed by the active map: refused as before.
    assert_eq!(
        compiled_frame_reference_word_by(&walk[0], &cm, 24, held),
        None
    );
}

/// OSRFRAME18-1: a mirror-read frame whose id names no safepoint of its
/// artifact is counted in the `innermost-unrecorded` sub-column beside
/// `id-unrecorded`; a frame placed by a return address, or one with no
/// RBP, or another verdict, is not.
#[test]
fn a_mirror_read_frame_with_an_unrecorded_id_is_counted_apart() {
    use cratonvm_jit::{
        FRAME_LINE_INNERMOST_UNRECORDED as SUB, FRAME_LINE_REFUSED_ID_UNRECORDED as UNREC,
        FRAME_LINE_REFUSED_NO_SP_ID,
    };
    assert!(innermost_unrecorded_refusal(Some(0x1000), None, UNREC));
    assert!(!innermost_unrecorded_refusal(
        Some(0x1000),
        Some(0x7000),
        UNREC
    ));
    assert!(!innermost_unrecorded_refusal(None, None, UNREC));
    assert!(!innermost_unrecorded_refusal(
        Some(0x1000),
        None,
        FRAME_LINE_REFUSED_NO_SP_ID
    ));

    let (mut band, rbp) = band();
    band[23] = 6; // an id the artifact recorded no map at: a dead frame's slot
    let cm = artifact(&[(SP_ID, vec![16])]);
    let lo = band.as_ptr() as usize;
    let before = cratonvm_jit::compiled_frame_line_counts();
    let walk = walk_jit_chain(
        &[entry(&cm, rbp, lo + 8 * 32)],
        lo,
        0,
        usize::MAX,
        false,
        None,
        None,
    );
    let after = cratonvm_jit::compiled_frame_line_counts();
    assert_eq!(walk.len(), 1);
    assert_eq!(walk[0].bci, -1, "the line is refused");
    // Process-wide counters other tests add to: lower bounds only.
    assert!(after[UNREC] > before[UNREC]);
    assert!(after[SUB] > before[SUB]);
}
