use super::*;

fn rows_of(vm: usize) -> Vec<String> {
    DELETE_ON_EXIT
        .get()
        .map(|l| {
            l.lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .filter(|(owner, _)| *owner == vm)
                .map(|(_, p)| p.clone())
                .collect()
        })
        .unwrap_or_default()
}

/// gc-common w19-a (`common-w18f-delete-on-exit-list-is-process-wide`):
/// two VMs register a temp file each (and the same shared path); tearing
/// one down deletes only its own file and leaves the other VM's rows. The
/// shared path is registered by both, so the per-VM de-duplication keeps a
/// row for each, and the surviving VM's row for it is still there after
/// the other VM deleted the file.
#[test]
fn a_vms_teardown_deletes_only_its_own_delete_on_exit_files() {
    // VM identities no real VM (heap-address identities) and no other
    // test uses.
    const VM_A: usize = 0x7ff1_9a31;
    const VM_B: usize = 0x7ff1_9a39;
    let dir = std::env::temp_dir().join(format!("cratonvm-w19a-doe-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let a = dir.join("a.tmp");
    let b = dir.join("b.tmp");
    std::fs::write(&a, b"a").expect("write a");
    std::fs::write(&b, b"b").expect("write b");
    let a_s = a.to_string_lossy().into_owned();
    let b_s = b.to_string_lossy().into_owned();
    let shared_s = dir.join("shared.tmp").to_string_lossy().into_owned();

    delete_on_exit_register(VM_A, a_s.clone());
    delete_on_exit_register(VM_A, a_s.clone());
    delete_on_exit_register(VM_A, shared_s.clone());
    delete_on_exit_register(VM_B, b_s.clone());
    delete_on_exit_register(VM_B, shared_s.clone());
    assert_eq!(
        rows_of(VM_A),
        vec![a_s.clone(), shared_s.clone()],
        "de-duplicated per VM"
    );
    assert_eq!(
        rows_of(VM_B),
        vec![b_s.clone(), shared_s.clone()],
        "not against A's row"
    );

    forget_vm_delete_on_exit(VM_A);
    let a_gone = !a.exists();
    let b_kept = b.exists();
    let a_rows = rows_of(VM_A);
    let b_rows = rows_of(VM_B);

    forget_vm_delete_on_exit(VM_B);
    let b_gone = !b.exists();
    let _ = std::fs::remove_dir(&dir);

    assert!(a_gone, "A's teardown must delete A's file");
    assert!(b_kept, "A's teardown must not touch B's file");
    assert!(a_rows.is_empty(), "A's rows are dropped: {a_rows:?}");
    assert_eq!(b_rows, vec![b_s, shared_s], "B's rows are untouched");
    assert!(b_gone, "B's own teardown deletes its file");
    assert!(rows_of(VM_B).is_empty());
}
