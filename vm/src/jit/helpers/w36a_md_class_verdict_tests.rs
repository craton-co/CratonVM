use super::*;

/// VM A's `MessageDigest` has class id 17; in VM B id 17 is BouncyCastle's
/// digest subclass and its own `MessageDigest` is 23. The process-wide id
/// latched 17 for both VMs: B's subclass was "ours" (its bytes went into
/// another digest's accumulator) and B's own digest never was.
#[test]
fn a_class_id_is_judged_in_the_asking_vm() {
    const VM_A: usize = 0xA36_0001;
    const VM_B: usize = 0xA36_0002;
    let table = |vm: usize, id: u32| -> Option<bool> {
        let name = match (vm, id) {
            (VM_A, 17) | (VM_B, 23) => "java/security/MessageDigest",
            (VM_B, 17) => "org/bouncycastle/jcajce/provider/digest/SHA256$Digest",
            _ => return None,
        };
        Some(
            cratonvm_native_builtins::jca::message_digest::update_fastpath::class_name_is_ours(
                name,
            ),
        )
    };
    assert!(md_class_verdict_memoised(VM_A, 17, || table(VM_A, 17)));
    // Straight after A's verdict is memoised, B asks about the same id.
    assert!(
        !md_class_verdict_memoised(VM_B, 17, || table(VM_B, 17)),
        "VM B's provider subclass was judged by VM A's class id"
    );
    assert!(
        md_class_verdict_memoised(VM_B, 23, || table(VM_B, 23)),
        "VM B's own MessageDigest was refused"
    );
    // A memo hit does not resolve again.
    assert!(md_class_verdict_memoised(VM_B, 23, || unreachable!(
        "the memoised verdict was re-resolved"
    )));
    assert!(!md_class_verdict_memoised(VM_A, 23, || table(VM_A, 23)));
    // Class id 0 (an array, or unresolved) and an unknown class are never
    // ours, and an unknown class is not memoised.
    assert!(!md_class_verdict_memoised(VM_A, 0, || Some(true)));
    assert!(!md_class_verdict_memoised(VM_A, 99, || None));
    assert!(md_class_verdict_memoised(VM_A, 99, || Some(true)));
}
