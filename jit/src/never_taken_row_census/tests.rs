use super::*;

fn rows(pairs: &[(usize, u32, u32)]) -> FxHashMap<usize, BranchCounts> {
    pairs
        .iter()
        .map(|&(pc, taken, not_taken)| (pc, BranchCounts { taken, not_taken }))
        .collect()
}

#[test]
fn j27_w8_row_census_counts_full_rows_against_the_prune_floor() {
    let floor = crate::ir::MIN_OBSERVATIONS_TO_PRUNE;
    let c = row_census(&rows(&[(3, 0, floor), (9, 1, floor - 2), (20, 5, 7)]));
    assert_eq!(c, RowCensus { rows: 3, full: 1, max_total: u64::from(floor) });
}

#[test]
fn j27_w8_row_census_of_no_rows_is_empty() {
    assert_eq!(row_census(&rows(&[])), RowCensus::default());
}

#[test]
fn j27_w8_census_line_names_the_fill_and_the_offer() {
    let line = census_line("A.m()V", RowCensus { rows: 4, full: 1, max_total: 2500 }, 0);
    assert_eq!(
        line,
        format!(
            "[ir] never-taken rows in A.m()V: rows=4 full=1 (floor {}) max_total=2500 offered=0",
            crate::ir::MIN_OBSERVATIONS_TO_PRUNE
        )
    );
}
