use super::{attribute_locked_monitors, HeldMonitors};
use crate::types::ObjectRef;

fn obj(addr: usize) -> ObjectRef {
    // SAFETY: never dereferenced; the record only compares and remaps.
    unsafe { ObjectRef::from_raw(addr as *mut u8) }
}

/// Interpreter round i7 wave 136, lane L6: a freeze and thaw keep the
/// record's mark, a trusted seed's provenance included.
#[test]
fn i7w136_a_thaw_keeps_the_frozen_mark() {
    use super::RecordProducer;
    let mut trusted = HeldMonitors::new();
    trusted.set_provenance(RecordProducer::DeoptBuild);
    trusted.push(obj(0x1000));
    let code = trusted.mark_code();
    assert!(code.is_some());
    let thawed = HeldMonitors::from_frozen(trusted.as_slice(), false, code);
    assert_eq!(thawed.producer(), Some(RecordProducer::DeoptBuild));
    assert!(!thawed.is_understated());
    assert_eq!(thawed.as_slice(), trusted.as_slice());

    let mut under = HeldMonitors::new();
    under.mark_understated_by(RecordProducer::SinglePassOsrExit);
    let thawed = HeldMonitors::from_frozen(under.as_slice(), true, under.mark_code());
    assert_eq!(thawed.producer(), Some(RecordProducer::SinglePassOsrExit));
    assert!(thawed.is_understated());

    // No mark code (an older freeze): the understated flag alone.
    let thawed = HeldMonitors::from_frozen(&[], true, None);
    assert_eq!(thawed.producer(), Some(RecordProducer::Thaw));
    assert!(thawed.is_understated());
    let plain = HeldMonitors::from_frozen(&[obj(0x2000)], false, None);
    assert_eq!(plain.producer(), None);
    assert_eq!(plain.mark_code(), None);
    assert_eq!(plain.len(), 1);
}

#[test]
fn two_entries_stay_inline_and_a_third_spills() {
    let mut h = HeldMonitors::new();
    assert!(h.is_empty());
    h.push(obj(0x1000));
    h.push(obj(0x2000));
    assert!(!h.objs.spilled(), "0-2 monitors must not allocate");
    h.push(obj(0x3000));
    assert_eq!(h.len(), 3);
}

#[test]
fn remove_newest_takes_the_newest_matching_entry() {
    let mut h = HeldMonitors::new();
    h.push(obj(0x1000));
    h.push(obj(0x2000));
    h.push(obj(0x1000));
    assert!(h.remove_newest(obj(0x2000)));
    assert_eq!(h.as_slice(), &[obj(0x1000), obj(0x1000)]);
    assert!(h.remove_newest(obj(0x1000)));
    assert_eq!(h.count_of(obj(0x1000)), 1);
    assert!(
        !h.remove_newest(obj(0x4000)),
        "an object the frame never entered"
    );
    assert!(h.remove_newest(obj(0x1000)));
    assert!(h.is_empty());
    assert!(!h.remove_newest(obj(0x1000)));
}

#[test]
fn remap_rewrites_each_entry_once() {
    let mut h = HeldMonitors::new();
    h.push(obj(0x1000));
    h.push(obj(0x2000));
    // A chained map (0x1000 -> 0x2000 -> 0x3000) must not move an entry
    // twice: 0x1000 lands on 0x2000, and 0x2000 on 0x3000.
    h.remap_with(|a| match a {
        0x1000 => Some(0x2000),
        0x2000 => Some(0x3000),
        _ => None,
    });
    assert_eq!(h.as_slice(), &[obj(0x2000), obj(0x3000)]);
    // A null target is never installed.
    h.remap_with(|_| Some(0));
    assert_eq!(h.as_slice(), &[obj(0x2000), obj(0x3000)]);
}

/// Wave 24 (L7): the `L7W24LockedMonitorDepths` shape -- `outer` (a
/// synchronized method on A with a block on B), `middle` (block on C),
/// `inner` (block re-entering A) -- reported innermost first, A twice,
/// B before the method monitor A in `outer`; the waited-on monitor and a
/// frame entry the lock stack does not back are dropped; an owned monitor
/// no frame names is kept at depth -1.
#[test]
fn locked_monitors_are_attributed_innermost_first() {
    let (a, b, c, w, jit) = (obj(0x10), obj(0x20), obj(0x30), obj(0x40), obj(0x50));
    // frames: 0 = main, 1 = outer, 2 = middle, 3 = inner, 4 = leaf
    let frames = vec![(3, a), (2, c), (1, b), (1, a), (2, w), (0, obj(0x60))];
    let owned = vec![a, b, c, w, jit];
    let (monitors, depths) = attribute_locked_monitors(5, owned, Some(frames), Some(w));
    assert_eq!(monitors, vec![a, c, b, a, jit]);
    assert_eq!(depths, vec![1, 2, 3, 3, -1]);
}

#[test]
fn without_attribution_only_the_waited_monitor_is_dropped() {
    let (a, w) = (obj(0x10), obj(0x40));
    let (monitors, depths) = attribute_locked_monitors(3, vec![a, w], None, Some(w));
    assert_eq!(monitors, vec![a]);
    assert!(
        depths.is_empty(),
        "no attribution: the JMX builder's old default"
    );
}

#[test]
fn pop_and_clear() {
    let mut h = HeldMonitors::new();
    h.push(obj(0x1000));
    h.push(obj(0x2000));
    assert_eq!(h.pop(), Some(obj(0x2000)));
    h.clear();
    assert!(h.is_empty());
    assert_eq!(h.pop(), None);
}
