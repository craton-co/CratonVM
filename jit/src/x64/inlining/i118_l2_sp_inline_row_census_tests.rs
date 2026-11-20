use super::sp_rowless_calls;

#[test]
fn only_spliced_rowless_calls_are_reported() {
    // own code 0..10, a splice 10..30, own 30..40, a nested splice 40..60.
    let marks = [(0, false), (10, true), (30, false), (40, true), (60, false)];
    let filed = [20u32, 45].into_iter().collect();
    // 5: own; 20: spliced, filed; 30: ends at the own mark, still the
    // splice's; 35: own; 45 filed; 55 rowless; 70 own.
    let calls = [5, 20, 30, 35, 45, 55, 70];
    let none = std::collections::HashSet::new();
    assert_eq!(sp_rowless_calls(&marks, &calls, &filed, &none), (4, 0, vec![30, 55]));
    // Wave 124 (L2): a declared-leaf helper call in a splice (a barrier)
    // is counted apart, not as rowless; a leaf outside a splice is not
    // counted at all.
    let leaf = [30u32, 5].into_iter().collect();
    assert_eq!(sp_rowless_calls(&marks, &calls, &filed, &leaf), (4, 1, vec![55]));
}
