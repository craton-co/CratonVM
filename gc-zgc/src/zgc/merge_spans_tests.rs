use super::merge_ascending_spans;

fn sorted(a: &[(usize, usize)], b: &[(usize, usize)]) -> Vec<(usize, usize)> {
    let mut all: Vec<(usize, usize)> = a.iter().chain(b.iter()).copied().collect();
    all.sort_unstable();
    all
}

/// Whatever the inputs, the answer is the old `collect` + `sort_unstable`.
#[test]
fn merging_two_runs_equals_sorting_their_concatenation() {
    let stayed = vec![
        (0x100, 0x140),
        (0x400, 0x440),
        (0x900, 0x9a0),
        (0x1000, 0x1010),
    ];
    let slid = vec![(0x140, 0x180), (0x180, 0x1c0), (0x440, 0x800)];
    let cases: Vec<(Vec<(usize, usize)>, Vec<(usize, usize)>)> = vec![
        (stayed.clone(), slid.clone()),
        (stayed.clone(), Vec::new()),
        (Vec::new(), slid.clone()),
        (Vec::new(), Vec::new()),
        // Out of order (an off-grid spill base appended after the bitmap
        // order): the fallback sort.
        (vec![(0x900, 0x9a0), (0x100, 0x140)], slid.clone()),
        (stayed.clone(), vec![(0x440, 0x800), (0x140, 0x180)]),
        // Equal starts.
        (vec![(0x100, 0x140), (0x200, 0x240)], vec![(0x100, 0x120)]),
    ];
    for (a, b) in cases {
        let want = sorted(&a, &b);
        assert_eq!(
            merge_ascending_spans(a.clone(), b.clone()),
            want,
            "{a:x?} + {b:x?}"
        );
    }
}
