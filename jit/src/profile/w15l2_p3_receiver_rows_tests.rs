//! JIT r20 wave 11, lane pProf (W15L2-P3): the lock-free receiver rows.
use super::*;

fn key(class_id: u32, name: &Arc<str>, desc: &Arc<str>) -> MethodKey {
    MethodKey {
        class_id,
        method_name: name.clone(),
        descriptor: desc.clone(),
    }
}

fn rows_store() -> ProfileStore {
    let mut store = ProfileStore::new();
    store.receiver_rows_on = true;
    store
}

/// The rows take the record without the mutex, and a snapshot shows it.
#[test]
fn a_held_recorder_records_into_the_rows_and_a_snapshot_folds_them() {
    let store = rows_store();
    let name: Arc<str> = Arc::from("run");
    let desc: Arc<str> = Arc::from("()V");
    let rec = store.receiver_recorder_borrowed(7, &name, &desc);
    assert!(rec.1.is_some());
    for _ in 0..3 {
        assert!(rec.record(10, 41, 20));
    }
    assert!(rec.record(10, 42, 20));
    assert!(rec.record_call_site(10, 20));
    assert!(rec.record_call_site(14, 20));
    let slot = store.get_or_insert_borrowed(7, &name, &desc);
    assert!(
        slot.lock().receivers.is_empty(),
        "nothing took the mutex path"
    );
    let p = store.get_profile(&key(7, &name, &desc)).expect("slot");
    assert_eq!(p.receivers[&10][&41], 3);
    assert_eq!(p.receivers[&10][&42], 1);
    assert_eq!(p.call_sites[&10], 1);
    assert_eq!(p.call_sites[&14], 1);
    assert_eq!(p.receiver_summary(10).expect("site").types, 2);
}

/// More classes than a site has rows: the extras go to the map, so the
/// type count stays exact (the planner's megamorphic decisions read it).
#[test]
fn classes_past_the_row_width_are_recorded_in_the_map_not_dropped() {
    let store = rows_store();
    let name: Arc<str> = Arc::from("run");
    let desc: Arc<str> = Arc::from("()V");
    let rec = store.receiver_recorder_borrowed(8, &name, &desc);
    let classes = RECEIVER_ROW_WIDTH as u32 + 5;
    for class_id in 0..classes {
        assert!(rec.record(3, class_id, 0));
        assert!(rec.record(3, class_id, 0));
    }
    let p = store.get_profile(&key(8, &name, &desc)).expect("slot");
    let summary = p.receiver_summary(3).expect("site");
    assert_eq!(summary.types, classes as usize);
    assert_eq!(summary.observations, 2 * classes);
    assert!(p.receivers[&3].values().all(|&n| n == 2));
}

/// More sites than the table holds: the extras go to the map.
#[test]
fn sites_past_the_table_are_recorded_in_the_map() {
    let store = rows_store();
    let name: Arc<str> = Arc::from("run");
    let desc: Arc<str> = Arc::from("()V");
    let rec = store.receiver_recorder_borrowed(9, &name, &desc);
    for site in 0..100usize {
        assert!(rec.record(site * 3, 5, 0));
        assert!(rec.record_call_site(site * 3, 0));
    }
    let p = store.get_profile(&key(9, &name, &desc)).expect("slot");
    for site in 0..100usize {
        assert_eq!(p.receivers[&(site * 3)][&5], 1, "site {site}");
        assert_eq!(p.call_sites[&(site * 3)], 1, "site {site}");
    }
}

/// Reclaimed stays exact: the held handle answers `false` from the next
/// call on and records nothing; a fresh lookup gets the new slot.
#[test]
fn a_reclaimed_slot_answers_false_through_the_rows() {
    let store = rows_store();
    let name: Arc<str> = Arc::from("run");
    let desc: Arc<str> = Arc::from("()V");
    let rec = store.receiver_recorder_borrowed(11, &name, &desc);
    assert!(rec.record(4, 50, 16));
    store.invalidate_class(11);
    assert!(!rec.record(4, 51, 16));
    assert!(!rec.record_call_site(4, 16));
    let fresh = store.receiver_recorder_borrowed(11, &name, &desc);
    assert!(fresh.record(4, 52, 16));
    let p = store.get_profile(&key(11, &name, &desc)).expect("slot");
    assert_eq!(p.receivers[&4].len(), 1);
    assert_eq!(p.receivers[&4][&52], 1);
}

/// Another version's frame takes the mutex path, which drops it, even
/// when the slot was stamped after the rows were made.
#[test]
fn another_versions_frame_is_still_dropped() {
    let store = rows_store();
    let name: Arc<str> = Arc::from("run");
    let desc: Arc<str> = Arc::from("()V");
    let slot = store.get_or_insert_borrowed(12, &name, &desc);
    let rec = store.receiver_recorder_borrowed(12, &name, &desc);
    assert!(slot.lock().note_code_len(40));
    assert!(rec.record(5, 77, 42));
    assert!(rec.record(5, 77, 58), "dropped, but the slot is live");
    let p = store.get_profile(&key(12, &name, &desc)).expect("slot");
    assert_eq!(p.receivers[&5][&77], 1);
}

/// The inline-cache seed tops the row's TOTAL (rows + map) up to its
/// count; it does not add a second copy on top of the rows.
#[test]
fn the_ic_seed_counts_what_the_rows_hold() {
    super::tests::with_profiling_enabled(|| {
        let store = rows_store();
        let name: Arc<str> = Arc::from("run");
        let desc: Arc<str> = Arc::from("()V");
        let k = key(13, &name, &desc);
        let rec = store.receiver_recorder_borrowed(13, &name, &desc);
        for _ in 0..5 {
            assert!(rec.record(6, 3, 0));
        }
        assert!(store.seed_ic_observed_receiver(&k, 6, 3, 8));
        assert!(!store.seed_ic_observed_receiver(&k, 6, 3, 8), "idempotent");
        let p = store.get_profile(&k).expect("slot");
        assert_eq!(p.receivers[&6][&3], 8);
    });
}

/// `=0`: no rows, every record takes the mutex (today's code).
#[test]
fn the_switch_off_records_through_the_mutex() {
    let mut store = ProfileStore::new();
    store.receiver_rows_on = false;
    let name: Arc<str> = Arc::from("run");
    let desc: Arc<str> = Arc::from("()V");
    let rec = store.receiver_recorder_borrowed(14, &name, &desc);
    assert!(rec.1.is_none());
    assert!(rec.record(2, 9, 0));
    let slot = store.get_or_insert_borrowed(14, &name, &desc);
    assert_eq!(slot.lock().receivers[&2][&9], 1);
}

/// Lossless and monotone under contention, like the mutex path's test.
#[test]
fn concurrent_row_recording_is_lossless() {
    const THREADS: usize = 4;
    const PER_THREAD: u32 = 5_000;
    let store = Arc::new(rows_store());
    let name: Arc<str> = Arc::from("run");
    let desc: Arc<str> = Arc::from("()V");
    let mut handles = Vec::new();
    for t in 0..THREADS {
        let store = Arc::clone(&store);
        let (name, desc) = (name.clone(), desc.clone());
        handles.push(std::thread::spawn(move || {
            let rec = store.receiver_recorder_borrowed(15, &name, &desc);
            for i in 0..PER_THREAD {
                // Six classes over four rows: both paths race.
                let class_id = (t as u32 + i) % 6;
                assert!(rec.record(21, class_id, 0));
                assert!(rec.record_call_site(21, 0));
            }
        }));
    }
    let mut prev = 0u32;
    for _ in 0..64 {
        if let Some(p) = store.get_profile(&key(15, &name, &desc)) {
            let total = p.receiver_summary(21).map_or(0, |s| s.observations);
            assert!(total >= prev, "a snapshot moved backwards");
            prev = total;
        }
    }
    for h in handles {
        h.join().expect("recorder thread");
    }
    let p = store.get_profile(&key(15, &name, &desc)).expect("slot");
    let s = p.receiver_summary(21).expect("site");
    assert_eq!(s.observations, PER_THREAD * THREADS as u32);
    assert_eq!(s.types, 6);
    assert_eq!(p.call_sites[&21], PER_THREAD * THREADS as u32);
}

/// A sampled record carries the calls it stands for, through the rows and
/// through the map alike, so a monomorphic site sampled by the invoke doors
/// still reaches `MIN_GUARDED_RECEIVER_EVIDENCE` after that many calls.
#[test]
fn a_weighted_record_counts_every_call_it_stands_for() {
    let name: Arc<str> = Arc::from("run");
    let desc: Arc<str> = Arc::from("()V");
    for store in [rows_store(), ProfileStore::new()] {
        let rec = store.receiver_recorder_borrowed(12, &name, &desc);
        assert!(rec.record_n(6, 70, 0, 4));
        assert!(rec.record_n(6, 70, 0, 28));
        assert!(rec.record_call_site_n(6, 0, 32));
        let p = store.get_profile(&key(12, &name, &desc)).expect("slot");
        assert_eq!(p.receivers[&6][&70], 32);
        assert_eq!(p.call_sites[&6], 32);
        assert_eq!(p.receiver_summary(6).expect("site").observations, crate::MIN_GUARDED_RECEIVER_EVIDENCE);
    }
}
