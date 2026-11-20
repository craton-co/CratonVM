use super::{classify_stale_local, StaleVerdict};

/// `local_offset(k) == 8*(k+1)`, so offset 40 is local 4 and offset 32 is
/// local 3. This is the `StringConcatHelper.doConcat` witness that
/// `bug-h2-testrandommapops-small-heap-corruption-20260829-RETIRED-20260909.md` reported as
/// a missed root: mask `Some(19)` = locals 0, 1 and 4, and the stale word
/// sat at offset 32 — local 3, which `javap` shows is an `int`
/// (`25: istore_3`). The oracle has to call that dead storage, not a root.
#[test]
fn the_doconcat_witness_is_dead_storage() {
    assert_eq!(
        classify_stale_local(32, &[], Some(0b10011), 6, &[], &[]),
        StaleVerdict::LocalNotOop
    );
    // ... while the slots the mask DOES name would be roots if they were
    // ever found stale.
    for off in [8usize, 16, 40] {
        assert_eq!(
            classify_stale_local(off, &[], Some(0b10011), 6, &[], &[]),
            StaleVerdict::LocalOopUnmapped,
            "offset {off}"
        );
    }
}

/// `None` and `Some(0)` are different claims and must not be folded: the
/// first says the dataflow never reached the bci (so the map named no
/// locals and nothing is attributable), the second says it reached and
/// proved no local holds a reference.
#[test]
fn unreached_is_not_the_empty_mask() {
    assert_eq!(
        classify_stale_local(32, &[], None, 6, &[], &[]),
        StaleVerdict::LocalUnreached
    );
    assert_eq!(
        classify_stale_local(32, &[], Some(0), 6, &[], &[]),
        StaleVerdict::LocalNotOop
    );
}

/// Past the locals band the offsets are spill and staging slots, which are
/// not local homes; the oracle must decline rather than answer about the
/// wrong slot. Offsets 0 and unaligned words are not local homes either.
#[test]
fn outside_the_locals_band_the_oracle_is_silent() {
    assert_eq!(
        classify_stale_local(56, &[], Some(0), 6, &[], &[]),
        StaleVerdict::OutsideLocals
    );
    assert_eq!(
        classify_stale_local(0, &[], Some(0), 6, &[], &[]),
        StaleVerdict::OutsideLocals
    );
    assert_eq!(
        classify_stale_local(36, &[], Some(0), 6, &[], &[]),
        StaleVerdict::OutsideLocals
    );
    // A frame with no mask at all (the IR tier) has `num_locals == 0`, so
    // every word is outside the band and nothing is misattributed.
    assert_eq!(
        classify_stale_local(8, &[], None, 0, &[], &[]),
        StaleVerdict::OutsideLocals
    );
}

/// A spliced callee's locals live in the operand-spill band, addressed
/// from the scope's own base — outside the java-locals band entirely. The
/// outer mask cannot speak for them, and before the scopes were recorded
/// every such word read as unclassifiable spill.
#[test]
fn a_spliced_callees_locals_are_classified_by_their_own_scope() {
    // Outer method has 2 locals (band 8..=16); a splice based at 96 with
    // 3 locals whose local 1 is a reference.
    let scopes = [(96i32, 3u16, 0b010u64)];
    assert_eq!(
        classify_stale_local(96, &[], Some(0), 2, &scopes, &[]),
        StaleVerdict::InlineLocalNotOop
    );
    assert_eq!(
        classify_stale_local(104, &[], Some(0), 2, &scopes, &[]),
        StaleVerdict::InlineLocalOopUnmapped
    );
    assert_eq!(
        classify_stale_local(112, &[], Some(0), 2, &scopes, &[]),
        StaleVerdict::InlineLocalNotOop
    );
    // One past the scope's last local is spill again, not local 3.
    assert_eq!(
        classify_stale_local(120, &[], Some(0), 2, &scopes, &[]),
        StaleVerdict::OutsideLocals
    );
    // Below the scope's base, likewise.
    assert_eq!(
        classify_stale_local(88, &[], Some(0), 2, &scopes, &[]),
        StaleVerdict::OutsideLocals
    );
    // A scope that could not classify its locals is not recorded at all,
    // so its band stays honestly unattributed rather than reading "dead".
    assert_eq!(
        classify_stale_local(104, &[], Some(0), 2, &[], &[]),
        StaleVerdict::OutsideLocals
    );
}

/// The java-locals band wins when the two could overlap: a splice base is
/// allocated out of the spill band, so an overlap means the scope record is
/// wrong, and answering from the method's own dataflow is the safer half.
#[test]
fn the_outer_locals_band_is_consulted_first() {
    let scopes = [(8i32, 4u16, u64::MAX)];
    assert_eq!(
        classify_stale_local(8, &[], Some(0), 4, &scopes, &[]),
        StaleVerdict::LocalNotOop
    );
}

/// A slot the map NAMED cannot be a missed root: relocation rewrote it, and
/// its current value being a pointer-map key means that to-space address
/// aliases some other object's from-space address. Asking the locals oracle
/// about it produced the instrument's only false `LOCAL-OOP-UNMAPPED`, on
/// `FileStore.readChunkFooter` -- `mapped=[8=..]` with `rewritten` equal to
/// the slot count, so slot 8 held a value the rewrite had just written.
#[test]
fn a_mapped_slot_is_never_a_missed_root() {
    // Local 0, mask says it IS a reference: the exact shape that misfired.
    assert_eq!(
        classify_stale_local(8, &[], Some(0b1), 9, &[], &[]),
        StaleVerdict::LocalOopUnmapped,
        "an UNMAPPED reference local is still the finding"
    );
    assert_eq!(
        classify_stale_local(8, &[8], Some(0b1), 9, &[], &[]),
        StaleVerdict::MappedAlias,
        "the same word, once the map names it, is not a missed root"
    );
    // The mapped check outranks every other oracle, including the ones
    // that would otherwise call the word dead.
    assert_eq!(
        classify_stale_local(96, &[96], Some(0), 2, &[], &[96]),
        StaleVerdict::MappedAlias
    );
}

/// The operand-spill band is where the residue actually is on the workload
/// this oracle was built for: 36 of 37 stale words on
/// `org.h2.test.store.TestRandomMapOps` sat in `region=operand-spill`,
/// which the locals and splice oracles both decline to answer for. The
/// safepoint's own stack model closes that gap for the slots it modelled.
#[test]
fn an_operand_spill_slot_the_stack_model_calls_a_primitive_is_dead() {
    // Outer method: 2 locals; spill slots 96 and 104 modelled as non-oops.
    assert_eq!(
        classify_stale_local(96, &[], Some(0), 2, &[], &[96, 104]),
        StaleVerdict::StackNotOop
    );
    // A spill slot the model did not mention stays unproven.
    assert_eq!(
        classify_stale_local(112, &[], Some(0), 2, &[], &[96, 104]),
        StaleVerdict::OutsideLocals
    );
    // A splice's claim wins over the stack model for the same word: the
    // scope knows it is a local, the stack model only knows it is a slot.
    assert_eq!(
        classify_stale_local(96, &[], Some(0), 2, &[(96, 1, 0b1)], &[96]),
        StaleVerdict::InlineLocalOopUnmapped
    );
}

/// `compute_local_oop_masks` gives up past 64 locals, so a mask can only
/// speak for bits 0..63; a local above that must not read as "not an oop",
/// which would be claiming a proof the mask does not carry.
#[test]
fn a_local_past_the_mask_width_is_not_proven_dead() {
    assert_eq!(
        classify_stale_local(8 * 65, &[], Some(u64::MAX), 80, &[], &[]),
        StaleVerdict::LocalUnreached
    );
}
