use super::{jar_cache_revalidate, jar_contents_cached, jar_path_mtime};
use std::fs::File;
use std::io::Write;
use std::time::{Duration, SystemTime};

/// Write a one-entry zip at `path` whose single entry is named `entry`,
/// then stamp it with `mtime` so the test does not depend on the host
/// filesystem's timestamp granularity.
fn write_jar(path: &std::path::Path, entry: &str, mtime: SystemTime) {
    let file = File::create(path).expect("create jar");
    let mut zipw = zip::ZipWriter::new(file);
    zipw.start_file(entry, zip::write::SimpleFileOptions::default())
        .expect("start_file");
    zipw.write_all(b"x").expect("write entry");
    let file = zipw.finish().expect("finish zip");
    file.set_modified(mtime).expect("set mtime");
    file.sync_all().expect("sync");
}

/// The accessors read a MEMOISED mtime, and opening re-probes it.
///
/// Both halves are load-bearing and neither is incidental:
///
/// * without the memo, every `JarFile.getEntry`/`size`/`getJarEntry` call
///   pays a `std::fs::metadata` (a `CreateFileW` on Windows, ~45 us on the
///   host this was measured on) — see `jar_path_mtime`;
/// * without the re-probe on open, a jar rewritten on disk and reopened
///   would be served from the stale parse, which is the exact bug the
///   `(path, mtime)` cache key was introduced to prevent.
#[test]
fn rewritten_jar_is_stale_until_reopened() {
    let dir = tempfile::tempdir().expect("tempdir");
    let jar = dir.path().join("memo.jar");
    let path = jar.to_string_lossy().into_owned();

    let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000);
    write_jar(&jar, "first.txt", t0);

    // First touch parses and memoises the mtime.
    let first = jar_contents_cached(&path).expect("first parse");
    assert!(first.by_name.contains_key("first.txt"));
    let memoised = jar_path_mtime(&path);

    // Rewrite with a DIFFERENT entry and a distinctly later mtime.
    let t1 = t0 + Duration::from_secs(3600);
    write_jar(&jar, "second.txt", t1);

    // Still the snapshot taken at open: the accessors do not re-stat.
    assert_eq!(
        jar_path_mtime(&path),
        memoised,
        "the accessor path must not re-stat"
    );
    let stale = jar_contents_cached(&path).expect("cached parse");
    assert!(
        stale.by_name.contains_key("first.txt"),
        "an already-open jar keeps its snapshot, as HotSpot's ZipFile.Source does"
    );

    // Opening re-probes, and the changed mtime produces a new cache key.
    jar_cache_revalidate(&path);
    assert_ne!(
        jar_path_mtime(&path),
        memoised,
        "revalidation must observe the new mtime"
    );
    let fresh = jar_contents_cached(&path).expect("reparse");
    assert!(
        fresh.by_name.contains_key("second.txt"),
        "a reopened jar must be reparsed, not served stale"
    );
    assert!(!fresh.by_name.contains_key("first.txt"));
}

/// The memo's `LockLevel::Scratch` claim is actually EVALUATED by these
/// tests, and the miss path does not re-enter the lock it just took.
///
/// Two things worth one test between them:
///
/// * `enforcement_active()` is unconditionally true in DEBUG builds, which
///   is what makes every other test in this module a live check of the
///   ordering claim rather than a run with the checker asleep. Asserting it
///   there means a future change that makes debug enforcement conditional
///   turns this module from silently vacuous into loudly red. The
///   assertion is `cfg`-gated because in RELEASE enforcement is off unless
///   `CRATONVM_LOCK_ORDER_CHECK` is set, and `cargo test --release` runs
///   this test too — an ungated assert here fails on a correct tree, which
///   is how the first version of it was caught.
/// * `jar_path_mtime` on a MISS takes the memo lock, drops it, and only
///   then calls `jar_path_mtime_probe`, which takes it again. The guard
///   used to live in an `if let` scrutinee, where it outlives the body —
///   and `parking_lot` is not reentrant, so getting that wrong is a hang,
///   not a failure. A hang is exactly what this asserts the absence of, and
///   it is profile-independent, so that half runs in both.
#[test]
fn memo_lock_is_order_checked_and_not_re_entered_on_a_miss() {
    #[cfg(debug_assertions)]
    assert!(
        cratonvm_types::lock_order::enforcement_active(),
        "debug builds must enforce lock order, or this module's ordering \
         claim is never checked by anything"
    );

    let dir = tempfile::tempdir().expect("tempdir");
    let jar = dir.path().join("reentry.jar");
    let path = jar.to_string_lossy().into_owned();
    let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_100_000_000);
    write_jar(&jar, "only.txt", t0);

    // Miss: lock, release, probe, lock again.
    let first = jar_path_mtime(&path);
    // Hit: the copied-out fast path.
    assert_eq!(jar_path_mtime(&path), first);
    // Re-probe, then hit again — the whole cycle, still single-entry.
    jar_cache_revalidate(&path);
    assert_eq!(jar_path_mtime(&path), first);
}

/// A path that does not exist must not pin a 0 mtime for the life of the
/// process: a jar written later has to be seen.
#[test]
fn missing_path_is_not_memoised() {
    let dir = tempfile::tempdir().expect("tempdir");
    let jar = dir.path().join("later.jar");
    let path = jar.to_string_lossy().into_owned();

    assert_eq!(jar_path_mtime(&path), 0, "absent file reports 0");
    assert!(jar_contents_cached(&path).is_none());

    let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_500_000_000);
    write_jar(&jar, "late.txt", t0);

    assert_ne!(
        jar_path_mtime(&path),
        0,
        "a failed stat must not be memoised, or the file could never appear"
    );
    let contents = jar_contents_cached(&path).expect("parse after creation");
    assert!(contents.by_name.contains_key("late.txt"));
}
