use super::*;

/// The encoder, for a pair supplied here rather than published.
///
/// This used to save the two globals, `arm` its own pair, encode, and
/// restore — which is not safe in a binary whose tests are parallel
/// THREADS. Two of these interleaved would each restore over the
/// other's arming, and while either was armed any other thread
/// compiling a primitive array store would emit 51 bytes it should
/// not. `cargo test -p cratonvm-jit --lib` failed a different pair of
/// these on about one run in three and was green under
/// `--test-threads=1`. See [`arm`]'s doc.
///
/// Nothing below touches `FILTER_ADDR` or `DIRTY_ADDR`. If a test in
/// this module ever needs to, it belongs in
/// `jit/tests/gpu_barrier_arming.rs` instead — a separate process.
fn encode_with(filter: usize, dirty: usize) -> Vec<u8> {
    barrier_bytes_for(filter, dirty).expect("armed pair")
}

#[test]
fn the_two_jumps_land_exactly_on_the_end() {
    let b = encode_with(0x1122_3344_5566_7788, 0x0088_7766_5544_3322);
    assert_eq!(b.len(), BARRIER_LEN);
    // Each rel8 is measured from the first byte AFTER the branch.
    // Landing one short would re-enter the sequence at
    // `MOV R11,[R11]` with the filter address already consumed;
    // landing one long would resume mid-instruction in whatever the
    // caller emitted next.
    assert_eq!(b[14], 0x74, "JZ opcode");
    assert_eq!(16 + b[15] as usize, BARRIER_LEN, "JZ must land on .skip");
    assert_eq!(b[34], 0x73, "JNC opcode");
    assert_eq!(36 + b[35] as usize, BARRIER_LEN, "JNC must land on .skip");
}

#[test]
fn both_addresses_are_embedded_little_endian() {
    let filter = 0x0000_7FFF_AABB_CCDDusize;
    let dirty = 0x0000_7FFF_AABB_CD1Dusize;
    let b = encode_with(filter, dirty);
    assert_eq!(&b[2..10], &(filter as u64).to_le_bytes());
    assert_eq!(&b[38..46], &(dirty as u64).to_le_bytes());
}

/// The fast path is the whole cost argument: a run that never submits
/// a kernel executes exactly these three instructions per array
/// store, against a word that stays zero for its lifetime.
#[test]
fn the_fast_path_is_three_instructions() {
    let b = encode_with(0x1000, 0x2000);
    assert_eq!(&b[0..2], &[0x49, 0xBB], "MOV R11, imm64");
    assert_eq!(&b[10..14], &[0x49, 0x83, 0x3B, 0x00], "CMP QWORD [R11], 0");
    assert_eq!(b[14], 0x74, "JZ");
}

/// A partially-published pair must emit nothing rather than a
/// sequence that stores through a null pointer.
///
/// Both halves are asserted against `pair_is_armed`, not merely
/// alongside it: `barrier_bytes_for` returning `Some` for a pair the
/// predicate calls unarmed would be a barrier emitted where
/// `offload_jit_gate` believes there is none, which is the
/// refuse-to-compile fallback silently not applying.
#[test]
fn a_half_armed_pair_emits_nothing() {
    for (filter, dirty) in [(0x1000usize, 0usize), (0, 0x2000), (0, 0)] {
        assert!(
            barrier_bytes_for(filter, dirty).is_none(),
            "({filter:#x}, {dirty:#x}) must emit nothing"
        );
        assert!(!pair_is_armed(filter, dirty));
    }
    assert!(pair_is_armed(0x1000, 0x2000));
    assert!(barrier_bytes_for(0x1000, 0x2000).is_some());
}

/// `barrier_bytes_for` answers `Some` exactly when `pair_is_armed`
/// says so. The two are read in different places — the emitters call
/// the first, `offload_jit_gate` calls the second through
/// `is_armed` to choose its array-writer policy — so a disagreement
/// is a gate that stands down while nothing takes its place.
#[test]
fn the_predicate_and_the_encoder_agree_on_every_corner() {
    for filter in [0usize, 1, 0x1000, usize::MAX] {
        for dirty in [0usize, 1, 0x2000, usize::MAX] {
            assert_eq!(
                barrier_bytes_for(filter, dirty).is_some(),
                pair_is_armed(filter, dirty),
                "disagreed on ({filter:#x}, {dirty:#x})"
            );
        }
    }
}
