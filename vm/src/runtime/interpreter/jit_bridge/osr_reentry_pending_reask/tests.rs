use super::*;


#[test]
fn only_the_16th_and_later_powers_of_two_reask() {
    let asks: Vec<u32> = (1..=200).filter(|&n| pending_hit_reasks(n)).collect();
    assert_eq!(asks, vec![16, 32, 64, 128]);
    assert!(!pending_hit_reasks(u32::MAX), "a saturated count stops asking");
}

/// The losing order: the worker's verdict came back transient, so the
/// entry stays pending with nothing cached and nothing refused. Off,
/// no hit ever asks again (the stuck state). On, the 16th, 32nd and 64th
/// hits do, and a queued re-ask is a task the worker will judge.
/// Put a fresh pending memo entry for `cid` on this thread (the state
/// `try_osr`'s background arm leaves); `false` when the memo is off.
fn put_pending_entry(shared: &SharedVm, cid: ClassId, method: &str) -> bool {
    let Some(stamp) = osr_reentry_stamp(shared) else {
        return false; // CRATONVM_JIT_OSR_REENTRY_MEMO=0
    };
    let single_pass = {
        let mut buf = cratonvm_jit::ExecutableBuffer::new(64).unwrap();
        buf.emit(&[0xC3]); // ret
        let mut cm = cratonvm_jit::CompiledMethod::new(buf);
        cm.compiled_via_osr = true;
        cm.osr_pc_to_native = Some(vec![0; 32]);
        Arc::new(cm)
    };
    let (m, d): (Arc<str>, Arc<str>) = (Arc::from(method), Arc::from("()V"));
    osr_reentry_memo_put(shared, stamp, 0x4747, cid, &m, &d, 7, &single_pass);
    osr_reentry_memo_set_optimizing_pending(true);
    true
}


/// The count lives on the entry: a new entry restarts it, and with no
/// entry there is no hit to count (so never a re-ask).
#[test]
fn another_entry_restarts_the_count() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let cid = ClassId::new(4949);
    if !put_pending_entry(&shared, cid, "a") {
        return;
    }
    for _ in 0..15 {
        osr_reentry_memo_note_pending_hit();
    }
    assert_eq!(osr_reentry_memo_note_pending_hit(), 16);
    assert!(put_pending_entry(&shared, cid, "b"));
    assert_eq!(osr_reentry_memo_note_pending_hit(), 1, "b's entry starts at 1");
    osr_reentry_memo_clear();
    assert_eq!(osr_reentry_memo_note_pending_hit(), 0, "no entry, no count");    }