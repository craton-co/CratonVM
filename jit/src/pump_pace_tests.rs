use super::pump_pace;

#[test]
fn pump_backs_off_after_futile_drains_and_caps_at_64() {
    assert_eq!(pump_pace(7, 5), (true, 0), "no backoff below 8 futile drains");
    assert_eq!(pump_pace(8, 0), (true, 0), "every = 1");
    assert_eq!(pump_pace(9, 0), (false, 1), "every = 2");
    assert_eq!(pump_pace(9, 1), (true, 0));
    let mut skipped = 0;
    let mut runs = 0;
    for _ in 0..640 {
        let (due, next) = pump_pace(1000, skipped);
        skipped = next;
        runs += due as u32;
    }
    assert_eq!(runs, 10, "capped at one tick in 64");
}
