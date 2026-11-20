use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

fn write_file(p: &std::path::Path, data: &[u8]) {
    std::fs::write(p, data).expect("write file");
}

/// Wait up to `timeout` for `predicate` to return true while pumping
/// the drain loop. Returns true if it fired.
fn wait_for(
    ws_id: i32,
    key_id: i32,
    timeout: Duration,
    predicate: impl Fn(&[(i32, String)]) -> bool,
) -> bool {
    let start = Instant::now();
    let mut accumulated: Vec<(i32, String)> = Vec::new();
    while start.elapsed() < timeout {
        let evs = poll_events(ws_id, key_id);
        accumulated.extend(evs);
        if predicate(&accumulated) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

#[test]
fn wp3_8_open_and_close_round_trip() {
    let id = open_watch_service().expect("open watch service");
    assert!(id >= 1);
    close_watch_service(id);
    // After close, register/poll/take all return errors or 0.
    let r = register_dir(id, ".", KIND_CREATE);
    assert!(r.is_err(), "register on closed ws must error");
}

#[test]
fn wp3_8_register_nonexistent_path_errors() {
    let id = open_watch_service().unwrap();
    let r = register_dir(id, "/path/that/should/not/exist/zzz", KIND_CREATE);
    assert!(r.is_err());
    close_watch_service(id);
}

#[test]
fn wp3_8_classify_event_kinds() {
    use notify::event::{CreateKind, ModifyKind, RemoveKind};
    assert_eq!(
        classify_event_kind(&EventKind::Create(CreateKind::File)),
        Some(KIND_CREATE)
    );
    assert_eq!(
        classify_event_kind(&EventKind::Remove(RemoveKind::File)),
        Some(KIND_DELETE)
    );
    assert_eq!(
        classify_event_kind(&EventKind::Modify(ModifyKind::Any)),
        Some(KIND_MODIFY)
    );
    // EventKind::Any is the "we have no idea what happened" variant
    // and we deliberately don't surface it as create/modify/delete.
    assert_eq!(classify_event_kind(&EventKind::Any), None);
}

#[test]
fn wp3_8_create_event_fires_on_real_filesystem() {
    let dir = tempfile::tempdir().expect("tmpdir");
    let id = open_watch_service().expect("open");
    let key = register_dir(id, dir.path().to_str().unwrap(), KIND_CREATE | KIND_MODIFY)
        .expect("register");

    // Touch a file inside the watched directory.
    write_file(&dir.path().join("hello.txt"), b"hi");

    let saw = wait_for(id, key, Duration::from_secs(3), |evs| {
        evs.iter()
            .any(|(k, _)| *k & (KIND_CREATE | KIND_MODIFY) != 0)
    });
    assert!(saw, "should observe a create or modify event");
    close_watch_service(id);
}

#[test]
fn wp3_8_delete_event_fires() {
    let dir = tempfile::tempdir().expect("tmpdir");
    let path = dir.path().join("doomed.bin");
    write_file(&path, b"x");

    let id = open_watch_service().expect("open");
    let key = register_dir(id, dir.path().to_str().unwrap(), KIND_DELETE).expect("register");

    std::fs::remove_file(&path).expect("remove");

    let saw = wait_for(id, key, Duration::from_secs(3), |evs| {
        evs.iter().any(|(k, _)| *k & KIND_DELETE != 0)
    });
    assert!(saw, "should observe a delete event");
    close_watch_service(id);
}

#[test]
fn wp3_8_modify_event_fires() {
    let dir = tempfile::tempdir().expect("tmpdir");
    let path = dir.path().join("file.bin");
    write_file(&path, b"v1");

    let id = open_watch_service().expect("open");
    let key = register_dir(id, dir.path().to_str().unwrap(), KIND_MODIFY).expect("register");

    // Sleep briefly to let the watcher install before the second write.
    std::thread::sleep(Duration::from_millis(50));
    write_file(&path, b"v2 longer content");

    let saw = wait_for(id, key, Duration::from_secs(3), |evs| {
        evs.iter().any(|(k, _)| *k & KIND_MODIFY != 0)
    });
    assert!(saw, "should observe a modify event");
    close_watch_service(id);
}

#[test]
fn wp3_8_poll_with_timeout_returns_zero_when_idle() {
    let dir = tempfile::tempdir().expect("tmpdir");
    let id = open_watch_service().unwrap();
    let _key = register_dir(id, dir.path().to_str().unwrap(), KIND_CREATE).unwrap();
    // Use a short positive timeout — should expire and return 0.
    let start = Instant::now();
    let key = poll_with_timeout(id, Duration::from_millis(150).as_nanos() as i64).unwrap();
    let elapsed = start.elapsed();
    assert_eq!(key, 0);
    assert!(elapsed >= Duration::from_millis(100), "elapsed {elapsed:?}");
    assert!(elapsed < Duration::from_secs(2), "elapsed {elapsed:?}");
    close_watch_service(id);
}

/// A NEGATIVE timeout is "already expired", not "wait forever".
///
/// RED against the tree as it stood before `watch_wait_for` existed: the
/// condition there was `timeout_ns == i64::MIN || timeout_ns < 0`, so every
/// negative classified as indefinite. The oracle is
/// `sun.nio.fs.AbstractWatchService.poll(long, TimeUnit)` handing the value
/// to `LinkedBlockingDeque.poll`, whose loop opens `if (nanos <= 0L) return
/// null;`.
///
/// Asserted on the classifier rather than by timing `poll_with_timeout`,
/// deliberately: the pre-fix behaviour of the negative case is *never
/// returns*, so a test that told the two apart by waiting would hang on a
/// red tree instead of failing it, and any bound that avoided the hang would
/// be a fixed wall-clock bound.
#[test]
fn wp3_8_a_negative_watch_timeout_does_not_wait() {
    assert_eq!(watch_wait_for(-1), WatchWait::Now, "poll(-1) must not wait");
    assert_eq!(watch_wait_for(-1_000_000_000), WatchWait::Now);
    assert_eq!(watch_wait_for(i64::MIN + 1), WatchWait::Now);
    // The one sentinel that does mean "block", written by `take_blocking`.
    assert_eq!(watch_wait_for(i64::MIN), WatchWait::Forever);
    // Unchanged either side of the fix.
    assert_eq!(watch_wait_for(0), WatchWait::Now);
    assert_eq!(watch_wait_for(1), WatchWait::Until(1));
}

/// `take()` must still block. The guard against fixing the row above
/// backwards by collapsing `i64::MIN` into the other negatives — which would
/// turn every `WatchService.take()` in the process into a busy `poll()`
/// returning `null`, and no assertion in this file would have said so.
#[test]
fn wp3_8_take_still_blocks_after_the_negative_timeout_fix() {
    assert_eq!(
        watch_wait_for(i64::MIN),
        WatchWait::Forever,
        "take_blocking's sentinel must remain the indefinite one"
    );
}

#[test]
fn wp3_8_take_returns_signalled_key() {
    let dir = tempfile::tempdir().expect("tmpdir");
    let id = open_watch_service().unwrap();
    let key = register_dir(
        id,
        dir.path().to_str().unwrap(),
        KIND_CREATE | KIND_MODIFY | KIND_DELETE,
    )
    .unwrap();

    // Run take() on a worker so we can drive an event from the test thread.
    let path = dir.path().to_path_buf();
    let worker = std::thread::spawn(move || take_blocking(id));

    // Give the worker a moment to enter its loop.
    std::thread::sleep(Duration::from_millis(80));
    write_file(&path.join("trigger.txt"), b"go");

    let result = worker.join().expect("worker join").expect("take ok");
    assert_eq!(result, key);
    close_watch_service(id);
}

#[test]
fn wp3_8_cancel_invalidates_key_and_subsequent_events_drop() {
    let dir = tempfile::tempdir().expect("tmpdir");
    let id = open_watch_service().unwrap();
    let key = register_dir(id, dir.path().to_str().unwrap(), KIND_CREATE).unwrap();

    cancel_key(id, key);

    // Generate an event after cancel — it must NOT signal the key.
    write_file(&dir.path().join("after_cancel.txt"), b"x");
    std::thread::sleep(Duration::from_millis(200));
    let r = poll_with_timeout(id, 0).unwrap();
    assert_eq!(r, 0, "cancelled key must not surface events");
    close_watch_service(id);
}

#[test]
fn wp3_8_reset_re_arms_signal_when_more_events_pending() {
    let dir = tempfile::tempdir().expect("tmpdir");
    let id = open_watch_service().unwrap();
    let key =
        register_dir(id, dir.path().to_str().unwrap(), KIND_CREATE | KIND_MODIFY).unwrap();

    write_file(&dir.path().join("a.txt"), b"x");
    std::thread::sleep(Duration::from_millis(150));
    // Drain once → key gets unsignalled.
    let _ = poll_events(id, key);

    // reset() with no pending: returns true (still valid), no
    // re-enqueue.
    assert!(reset_key(id, key));

    // Now generate another event and verify reset re-arms.
    write_file(&dir.path().join("b.txt"), b"y");
    std::thread::sleep(Duration::from_millis(150));
    let r = poll_with_timeout(id, 0).unwrap();
    assert_eq!(r, key, "next event must signal the key again");
    close_watch_service(id);
}

#[test]
fn wp3_8_kind_mask_filters_unrelated_events() {
    let dir = tempfile::tempdir().expect("tmpdir");
    let id = open_watch_service().unwrap();
    // Only register CREATE — modifies on existing files must not surface.
    let path = dir.path().join("preexisting.bin");
    write_file(&path, b"v1");
    std::thread::sleep(Duration::from_millis(50));

    let key = register_dir(id, dir.path().to_str().unwrap(), KIND_CREATE).unwrap();
    // Modify the preexisting file — should NOT be visible (we only
    // asked for CREATE).
    write_file(&path, b"v2 different bytes");
    std::thread::sleep(Duration::from_millis(200));

    let evs = poll_events(id, key);
    assert!(
        evs.iter().all(|(k, _)| *k & KIND_CREATE != 0),
        "registering CREATE-only must not yield modify events: {evs:?}"
    );
    close_watch_service(id);
}

#[test]
fn wp3_8_thread_local_name_stash_isolates_per_key() {
    stash_names(1, 2, vec!["a".into(), "b".into()]);
    stash_names(1, 3, vec!["c".into()]);
    let n2 = take_stashed_names(1, 2);
    assert_eq!(n2, vec!["a".to_string(), "b".to_string()]);
    let n3 = take_stashed_names(1, 3);
    assert_eq!(n3, vec!["c".to_string()]);
    // Re-take returns empty (consumed exactly once).
    assert!(take_stashed_names(1, 2).is_empty());
}

#[test]
fn wp3_8_register_on_closed_service_errors() {
    let id = open_watch_service().unwrap();
    close_watch_service(id);
    let r = register_dir(id, ".", KIND_CREATE);
    assert!(r.is_err());
}

#[test]
fn wp3_8_register_returns_distinct_key_ids_per_call() {
    let dir1 = tempfile::tempdir().unwrap();
    let dir2 = tempfile::tempdir().unwrap();
    let id = open_watch_service().unwrap();
    let k1 = register_dir(id, dir1.path().to_str().unwrap(), KIND_CREATE).unwrap();
    let k2 = register_dir(id, dir2.path().to_str().unwrap(), KIND_CREATE).unwrap();
    assert_ne!(k1, k2);
    close_watch_service(id);
}

/// HIGH-severity security regression guard: under
/// `set_path_confine_to_cwd(true)`, a guest must not be able to call
/// `WatchService.register("../../etc")` and bypass `validate_path`
/// via the inotify / ReadDirectoryChangesW backend. The Java-visible
/// failure must be an `IOException` (the entry point's declared
/// throws), not a panic, not a silent success, and not a
/// `SecurityException` leaking up out of the watch service API.
#[test]
fn ws_register_rejects_relative_traversal_under_confinement() {
    let _g = crate::test_support::confine_test_lock().lock();
    crate::set_path_confine_to_cwd(true);

    let id = open_watch_service().expect("open watch service");
    let r = register_dir(id, "../../etc/passwd", KIND_CREATE);

    // Restore default BEFORE assertions so a failing assert doesn't
    // strand the rest of the test suite in confinement mode.
    crate::set_path_confine_to_cwd(false);
    close_watch_service(id);

    assert!(
        r.is_err(),
        "traversal path accepted by WatchService.register"
    );
    let err = format!("{:?}", r.unwrap_err());
    assert!(
        err.contains("IOException"),
        "expected IOException, got: {err}"
    );
    assert!(
        err.contains("sandbox") || err.contains("traversal"),
        "expected sandbox/traversal-related message, got: {err}"
    );
}
