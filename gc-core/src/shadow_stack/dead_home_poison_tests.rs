use super::*;

#[test]
fn a_poisoned_home_publishes_null_through_either_entry_kind() {
    let poison = cratonvm_types::jit_activation::DEAD_HOME_POISON as usize;
    let mut frame = [poison, 0x2000usize];
    let fp = std::hint::black_box(frame.as_mut_ptr());
    let slot0 = fp as usize;
    let slot1 = fp as usize + 8;
    let mut s = ShadowStack::empty();
    s.ensure_allocated();
    assert!(s.push(slot0 | ShadowStack::INDIRECT_TAG));
    assert!(s.push(poison));
    assert!(s.push(slot1 | ShadowStack::INDIRECT_TAG));

    let mut seen = Vec::new();
    s.for_each_value(|v| seen.push(v));
    assert_eq!(seen, vec![0, 0, 0x2000]);

    let mut map = cratonvm_types::PointerMap::default();
    map.insert(0x2000usize, 0x9000usize);
    assert_eq!(s.remap(&map), 1, "only the live slot is rewritten");
    // SAFETY: `fp` points at this test's live two-word array.
    let (w0, w1) = unsafe { (fp.read_volatile(), fp.add(1).read_volatile()) };
    assert_eq!(w0, poison, "the poisoned slot is left as it is");
    assert_eq!(w1, 0x9000);
    let _ = std::hint::black_box(&mut frame);
    s.reset();
}
