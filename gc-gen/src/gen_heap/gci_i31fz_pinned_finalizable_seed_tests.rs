use super::{pinned_plan_finalizable_members, pinned_plan_seeds_without_unnamed_watched};

// A pinned page as the probe lays it out: [Fin[] 0x110][F0 0x18][pad0 0x118][F1 0x18][pad1 0x118].
const ARR: usize = 0x1000;
const F0: usize = 0x1110;
const P0: usize = 0x1128;
const F1: usize = 0x1240;
const P1: usize = 0x1258;
const OBJECTS: [(usize, usize); 5] =
    [(ARR, 0x110), (F0, 0x18), (P0, 0x118), (F1, 0x18), (P1, 0x118)];

/// Only plan bases that are candidates are members: an interior address,
/// an address outside the plan and a duplicate are not (twice).
#[test]
fn gci_i31fz_members_are_the_candidate_plan_bases() {
    let got = pinned_plan_finalizable_members(&OBJECTS, &[F1, 0x9000, F0 + 8, F0, F1, 0x10]);
    assert_eq!(got, vec![F0, F1]);
    assert!(pinned_plan_finalizable_members(&[], &[F0]).is_empty());
    assert!(pinned_plan_finalizable_members(&OBJECTS, &[]).is_empty());
}

/// The probe's shape: the frame's stale word names the array by its base;
/// both dead `Fin`s stop being seeds, the array and the pads stay seeds.
#[test]
fn gci_i31fz_unnamed_finalizables_are_not_seeds() {
    let fins = pinned_plan_finalizable_members(&OBJECTS, &[F0, F1]);
    let seeds: Vec<usize> = OBJECTS.iter().map(|&(b, _)| b).collect();
    let got = pinned_plan_seeds_without_unnamed_watched(
        &[ARR],
        &OBJECTS,
        &seeds,
        |b| fins.binary_search(&b).is_ok(),
        |b| b == ARR,
    );
    assert_eq!(got, Some(vec![ARR, P0, P1]));
}

/// A finalizable a word names (inside it) stays a seed.
#[test]
fn gci_i31fz_a_named_finalizable_stays_a_seed() {
    let fins = pinned_plan_finalizable_members(&OBJECTS, &[F0, F1]);
    let seeds: Vec<usize> = OBJECTS.iter().map(|&(b, _)| b).collect();
    let got = pinned_plan_seeds_without_unnamed_watched(
        &[F1 + 8],
        &OBJECTS,
        &seeds,
        |b| fins.binary_search(&b).is_ok(),
        |_| false,
    );
    assert_eq!(got, Some(vec![ARR, P0, F1, P1]));
}
