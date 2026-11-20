use super::*;

#[test]
fn cast_pass_memo_is_keyed_by_vm_class_and_target() {
    let vm = 0x5715_0001usize;
    let cid = ClassId::new(4242);
    assert!(!cast_pass_memo_hit_keyed(true, vm, cid, "java/lang/Integer"));
    cast_pass_memo_note_keyed(true, vm, cid, "java/lang/Integer");
    assert!(cast_pass_memo_hit_keyed(true, vm, cid, "java/lang/Integer"));
    assert!(!cast_pass_memo_hit_keyed(true, vm + 1, cid, "java/lang/Integer"));
    assert!(!cast_pass_memo_hit_keyed(
        true,
        vm,
        ClassId::new(4243),
        "java/lang/Integer"
    ));
    assert!(!cast_pass_memo_hit_keyed(true, vm, cid, "java/lang/Integes"));
    assert!(!cast_pass_memo_hit_keyed(true, vm, cid, "java/lang/Number"));
}

/// gci i14/sx: with the VM's switch off (`CRATONVM_LAMBDA_CAST_PASS_MEMO=0`
/// read into `LambdaSwitches`) nothing is recorded and nothing answers,
/// a row recorded while on included. Both arms run in one process: the
/// switch is no longer a process latch.
#[test]
fn gci_i14sx_cast_pass_memo_off_records_and_answers_nothing() {
    let vm = 0x5715_0002usize;
    let cid = ClassId::new(4244);
    cast_pass_memo_note_keyed(false, vm, cid, "java/lang/Long");
    assert!(
        !cast_pass_memo_hit_keyed(true, vm, cid, "java/lang/Long"),
        "off records nothing"
    );
    cast_pass_memo_note_keyed(true, vm, cid, "java/lang/Long");
    assert!(cast_pass_memo_hit_keyed(true, vm, cid, "java/lang/Long"));
    assert!(
        !cast_pass_memo_hit_keyed(false, vm, cid, "java/lang/Long"),
        "off answers nothing"
    );
}

#[test]
fn cast_pass_memo_index_stays_in_bounds() {
    for id in [0u32, 1, 77, u32::MAX - 1, u32::MAX] {
        for t in ["", "a", "java/lang/String", "java/util/List"] {
            assert!(cast_pass_memo_index(ClassId::new(id), t) < CAST_PASS_MEMO_ROWS);
        }
    }
}
