use super::*;

#[test]
fn every_writer_withdraws_the_previous_version_before_mutating() {
    let set = JitSkipSet::default();
    let key = (Arc::from("Probe"), Arc::from("call"), Arc::from("()V"));
    let first = set.epoch();
    assert_eq!(first & 1, 0);
    {
        let mut writer = set.write();
        assert_eq!(set.epoch() & 1, 1);
        assert_ne!(set.epoch(), first);
        writer.insert(key.clone());
    }
    let sealed = set.epoch();
    assert_eq!(sealed & 1, 0);
    assert!(set.read().contains(&key));
    set.write().remove(&key);
    assert_ne!(set.epoch(), sealed);
    assert!(!set.read().contains(&key));
}
