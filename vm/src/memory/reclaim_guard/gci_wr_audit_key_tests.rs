use super::first_audit_for;

#[test]
fn gci_wr_frame_audit_dedup_is_per_vm_thread_and_epoch() {
    // A fresh OS thread, so the thread-local starts at its initial key.
    std::thread::spawn(|| {
        assert!(first_audit_for(0xA0, 7, 3), "first audit of VM A runs");
        assert!(!first_audit_for(0xA0, 7, 3), "a repeat in the same epoch is skipped");
        assert!(
            first_audit_for(0xB0, 7, 3),
            "VM B with the same thread id and count is a different key"
        );
        assert!(first_audit_for(0xB0, 7, 4), "a new epoch audits again");
        assert!(first_audit_for(0xB0, 8, 4), "another thread id audits");
    })
    .join()
    .expect("audit-key test thread");
}
