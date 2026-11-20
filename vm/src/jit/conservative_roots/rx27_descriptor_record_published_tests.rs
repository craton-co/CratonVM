#[test]
fn rx27_published_shadow_values_expand_a_descriptor_record() {
    let obj_a = 0x10_0040usize;
    let obj_b = 0x10_0048usize;
    // A "compiled frame" on this thread's stack, a caller of the reader.
    let mut frame = [obj_b, 0usize, 0usize, obj_a];
    let lo = std::hint::black_box(frame.as_mut_ptr()) as usize;
    let rbp = lo + 32;
    let buf: Box<[usize; 4]> = Box::new([
        0x10_0050,
        rbp | cratonvm_gc::shadow_stack::DESCRIPTOR_TAG,
        cratonvm_gc::shadow_stack::descriptor_header(2) as usize,
        1usize | (4usize << 16),
    ]);
    let base = buf.as_ptr() as usize;
    let set = super::published_shadow_values(Some((base, base + 32)));
    assert!(set.contains(&0x10_0050), "the value entry");
    assert!(set.contains(&obj_a), "home rbp-8");
    assert!(set.contains(&obj_b), "home rbp-32");
    assert!(!set.contains(&(rbp | cratonvm_gc::shadow_stack::DESCRIPTOR_TAG)));
    let _ = std::hint::black_box(&mut frame);
}
