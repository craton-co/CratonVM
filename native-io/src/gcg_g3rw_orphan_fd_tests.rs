use super::*;

/// `take_orphan_fds` hands back exactly one VM's parked ids, once, and
/// leaves every other VM's list alone.
#[test]
fn gcg_g3rw_take_orphan_fds_drains_one_vm_once() {
    // VM identities no real VM in this test binary can have.
    let (vm_a, vm_b) = (usize::MAX - 0x3301, usize::MAX - 0x3302);
    queue_orphan_fds(vm_a, vec![7, 9]);
    queue_orphan_fds(vm_a, vec![]);
    queue_orphan_fds(vm_b, vec![11]);
    queue_orphan_fds_checked(vm_b, vec![(13, 4)]);
    let mut a = take_orphan_fds(vm_a);
    a.sort_unstable();
    assert_eq!(a, vec![(7, None), (9, None)]);
    assert!(take_orphan_fds(vm_a).is_empty(), "drained once");
    assert_eq!(
        take_orphan_fds(vm_b),
        vec![(11, None), (13, Some(4))],
        "the other VM's list is kept, generations with it"
    );
    assert!(take_orphan_fds(vm_b).is_empty());
}

/// JIT r23 wave 3 review (`r23-w3-review-fd-os-keys-reuse-audit`): a
/// generation-checked orphan is closed only while its entry is the one
/// that was parked; a stale generation leaves the live owner open.
#[test]
fn a_checked_orphan_never_closes_a_later_owner() {
    let table =
        cratonvm_native_api::fd_table::FileDescriptorTable::new_with_os_keys(true);
    let path = std::env::temp_dir().join(format!(
        "cratonvm_orphan_gen_{}.txt",
        std::process::id()
    ));
    std::fs::write(&path, b"orphan").unwrap();
    let path = path.to_string_lossy().into_owned();
    let fd = table.open_read(&path).unwrap();
    let live = table.generation(fd).expect("open");
    close_orphan_fd(&table, (fd, Some(live.wrapping_add(1))));
    assert_eq!(table.generation(fd), Some(live), "a stale generation closes nothing");
    close_orphan_fd(&table, (fd, Some(live)));
    assert_eq!(table.generation(fd), None, "the parked generation closes it");
    let unchecked = table.open_read(&path).unwrap();
    close_orphan_fd(&table, (unchecked, None));
    assert_eq!(table.generation(unchecked), None, "an unchecked orphan closes");
    let _ = std::fs::remove_file(&path);
}

/// JIT r23 wave 3 review: a `FileChannel` lock left behind by a closed
/// file (no `release`) does not conflict with a later file that reuses
/// the fd value -- `file_lock_key` qualifies the id by its generation.
#[test]
fn a_dead_files_lock_region_does_not_block_a_reused_fd() {
    use crate::test_support::MockNativeContext;
    let mock = MockNativeContext::new();
    let ctx: &dyn NativeContext = &mock;
    let path = std::env::temp_dir().join(format!(
        "cratonvm_lock_gen_{}.txt",
        std::process::id()
    ));
    std::fs::write(&path, b"lock").unwrap();
    let path = path.to_string_lossy().into_owned();
    let fd = ctx.fd_table().open_read_write(&path, false).unwrap();
    let first_key = file_lock_key(ctx, i64::from(fd));
    assert_eq!(first_key & 0xFFFF_FFFF, i64::from(fd), "the low half is the id");
    let stale = try_acquire_file_lock(first_key, 0, i64::MAX, false, None)
        .expect("the first lock");
    // Closed WITHOUT releasing its lock.
    ctx.fd_table().close(fd).unwrap();
    let mut misses = Vec::new();
    let mut reused = None;
    for _ in 0..64 {
        let again = ctx.fd_table().open_read_write(&path, false).unwrap();
        if again == fd {
            reused = Some(again);
            break;
        }
        misses.push(again);
    }
    if let Some(again) = reused {
        let key = file_lock_key(ctx, i64::from(again));
        if ctx.fd_table().uses_os_keys() {
            assert_ne!(key, first_key, "a later owner has its own key");
            let token = try_acquire_file_lock(key, 0, i64::MAX, false, None)
                .expect("the stale region must not conflict");
            release_file_lock(token);
        }
        ctx.fd_table().close(again).unwrap();
    }
    for m in misses {
        ctx.fd_table().close(m).unwrap();
    }
    release_file_lock(stale);
    let _ = std::fs::remove_file(&path);
}
