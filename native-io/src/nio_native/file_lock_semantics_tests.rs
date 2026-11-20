use super::os_lock;
use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;

fn open_at(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
}

#[test]
fn append_output_stream_can_lock_and_append() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("append-lock");
    std::fs::write(&path, b"start").unwrap();
    let table = cratonvm_native_api::fd_table::FileDescriptorTable::new();
    let fd = table.open_write(path.to_str().unwrap(), true).unwrap();
    let file = table.clone_file(fd).unwrap();
    assert!(os_lock::lock_range(&file, 0, i64::MAX, false, false).unwrap());
    let rival = open_at(&path).unwrap();
    assert!(!os_lock::lock_range(&rival, 0, i64::MAX, false, false).unwrap());
    // A position change must not redirect an append stream's writes.
    table.rw_seek(fd, io::SeekFrom::Start(0)).unwrap();
    table.write_bytes(fd, b"!").unwrap();
    os_lock::unlock_range(&file, 0, i64::MAX).unwrap();
    table.close(fd).unwrap();
    assert_eq!(std::fs::read(path).unwrap(), b"start!");
}

/// `lock0`'s load-bearing premise: the lock it places through a transient
/// `clone_file` handle is still held after that handle is dropped, because
/// the fd_table's own handle keeps the underlying file object alive.
///
/// If this ever stops holding, `lock0` returns `LOCKED (0)` — the value
/// `FileChannelImpl.lock`/`tryLock` branches on to hand back a live
/// `FileLockImpl` — for a lock that is not there, and two processes running
/// `if (ch.tryLock() == null) bail;` both proceed. That is the H2
/// `FileLock` / Derby `db.lck` / Lucene `NativeFSLockFactory` shape, so this
/// test is the guard on a silent two-writer corruption, not on a comment.
#[test]
fn a_lock_outlives_the_transient_handle_it_was_placed_through() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("guard.lck");

    // Stands in for the fd_table entry: alive for the channel's lifetime.
    let owner = open_at(&path).expect("owner handle");
    // Stands in for `ctx.fd_table().clone_file(fd)`.
    let transient = owner.try_clone().expect("clone_file stand-in");

    assert!(
        os_lock::lock_range(&transient, 0, i64::MAX, false, false).expect("lock_range"),
        "the first exclusive whole-file lock must be granted"
    );
    drop(transient); // exactly what `lock0` does on its way out

    // Stands in for the other process / another opener.
    let rival = open_at(&path).expect("rival handle");
    assert!(
        !os_lock::lock_range(&rival, 0, i64::MAX, false, false)
            .expect("a contended tryLock must report, not error"),
        "PREMISE VIOLATED: dropping the handle `lock0` locked through released \
         the lock, so `lock0` returns LOCKED for a lock that is not held"
    );

    // `release0`'s actual implementation: unlock through a FRESH clone,
    // not through the handle that locked. Measured to work — which is what
    // makes `release0` correct despite its (now corrected) comment.
    let fresh = owner.try_clone().expect("fresh clone");
    os_lock::unlock_range(&fresh, 0, i64::MAX)
        .expect("unlock through a different handle onto the same file object");

    assert!(
        os_lock::lock_range(&rival, 0, i64::MAX, false, false).expect("lock_range"),
        "after release0's unlock the range must be acquirable again"
    );
    os_lock::unlock_range(&rival, 0, i64::MAX).expect("cleanup unlock");
}

/// NEGATIVE CONTROL. Without this, the test above would also pass on a
/// platform where nothing ever contends — e.g. if `lock_range` quietly
/// no-op'd and always answered `true`.
#[test]
fn control_an_independent_opener_really_is_refused_and_then_admitted() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("control.lck");

    let holder = open_at(&path).expect("holder");
    let rival = open_at(&path).expect("rival");

    assert!(
        os_lock::lock_range(&holder, 0, i64::MAX, false, false).expect("lock"),
        "control: an uncontended lock must be granted"
    );
    assert!(
        !os_lock::lock_range(&rival, 0, i64::MAX, false, false).expect("probe"),
        "control: locking is not enforced at all on this platform — every \
         assertion in this module is vacuous"
    );
    os_lock::unlock_range(&holder, 0, i64::MAX).expect("unlock");
    assert!(
        os_lock::lock_range(&rival, 0, i64::MAX, false, false).expect("probe 2"),
        "control: the range stayed locked after an explicit unlock"
    );
    os_lock::unlock_range(&rival, 0, i64::MAX).expect("cleanup");
}

/// Closing the LAST handle onto the file object releases the range. This is
/// the property that bounds the blast radius of a `FileChannel.close()` that
/// never called `release0`.
#[test]
fn closing_the_only_handle_releases_the_range() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("sole.lck");

    let sole = open_at(&path).expect("sole handle");
    assert!(os_lock::lock_range(&sole, 0, i64::MAX, false, false).expect("lock"));
    drop(sole);

    let rival = open_at(&path).expect("rival");
    assert!(
        os_lock::lock_range(&rival, 0, i64::MAX, false, false).expect("probe"),
        "a lock must not outlive the last handle onto its file"
    );
    os_lock::unlock_range(&rival, 0, i64::MAX).expect("cleanup");
}
