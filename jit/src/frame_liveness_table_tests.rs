use super::{FrameLivenessTable, SafepointFrameLiveness};

fn rec(sp_id: u32, live: &[i16], complete: bool) -> SafepointFrameLiveness {
    SafepointFrameLiveness {
        sp_id,
        live: live.to_vec(),
        complete,
    }
}

/// A covered word is dead exactly where a complete record for the active
/// id leaves it out; everything else stays a root.
#[test]
fn only_a_complete_record_for_the_active_id_proves_a_covered_word_dead() {
    let t = FrameLivenessTable {
        covered: vec![16, 24, 32],
        records: vec![
            rec(3, &[16], true),
            rec(5, &[], false),
            rec(7, &[24, 32], true),
        ],
    };
    assert!(t.proves_dead(3, 24));
    assert!(t.proves_dead(3, 32));
    assert!(!t.proves_dead(3, 16), "listed live");
    assert!(!t.proves_dead(3, 40), "not covered");
    assert!(!t.proves_dead(4, 24), "no record for the id");
    assert!(!t.proves_dead(5, 24), "an incomplete record is no claim");
    assert!(t.proves_dead(7, 16));
    assert!(!t.proves_dead(7, 24));
    assert!(!FrameLivenessTable::default().proves_dead(0, 16));
}

/// Records sharing an id intersect their dead sets: one record that keeps
/// a word live keeps it live.
#[test]
fn records_sharing_an_id_prove_only_the_words_all_of_them_call_dead() {
    let t = FrameLivenessTable {
        covered: vec![16, 24],
        records: vec![rec(9, &[16], true), rec(9, &[24], true)],
    };
    assert!(!t.proves_dead(9, 16));
    assert!(!t.proves_dead(9, 24));
    let t = FrameLivenessTable {
        covered: vec![16, 24],
        records: vec![rec(9, &[], true), rec(9, &[24], true)],
    };
    assert!(t.proves_dead(9, 16));
    assert!(!t.proves_dead(9, 24));
    let t = FrameLivenessTable {
        covered: vec![16],
        records: vec![rec(9, &[], true), rec(9, &[], false)],
    };
    assert!(
        !t.proves_dead(9, 16),
        "one incomplete sibling withdraws the claim"
    );
}
