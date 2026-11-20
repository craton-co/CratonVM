//! gc-common round 2026-09-23, wave 4, lane C4: converting a VM-raised
//! `OutOfMemoryError` into a throwable does not run the allocation ladder
//! a second time (`common-w3a-oome-construction-reruns-the-allocation-ladder`).

/// The body of the production function starting at `head`. `\u{7d}` is a
/// closing brace, spelled so this line's braces balance for the
/// brace-counting production-panic scanner.
fn body_of<'a>(src: &'a str, head: &str) -> &'a str {
    let start = src.find(head).unwrap_or_else(|| panic!("{head} not found"));
    let len = src[start..].find("\n\u{7d}\n").expect("function end");
    &src[start..start + len]
}

/// Source witness: a driven exhaustion needs a heap full of live data a
/// unit test cannot build, and the property is structural -- the
/// conversion of an `OutOfMemoryError` takes the no-collect door, and that
/// door's failure arm neither collects nor reclaims.
#[test]
fn a_vm_raised_oome_is_built_without_collecting_again() {
    let src = include_str!("../exceptions.rs");
    let throw = body_of(src, "pub fn throw_runtime_error(");
    assert!(
        throw.contains("create_vm_oome_object("),
        "throw_runtime_error must build an OutOfMemoryError through the \
         no-collect door"
    );
    // gen r5w1/oom5: that door is the no-collect constructor, and its one
    // collecting retry is the VM-limit refusal's, which ran no ladder.
    let vm_oome = body_of(src, "fn create_vm_oome_object_with_array_retry(");
    assert!(vm_oome.contains("create_exception_object_no_collect("));
    // gcf f3/o, f4/o: the retry is an arm of `built`, then shaped
    // (`vm_oome_hotspot_shape`).
    let retry = vm_oome
        .find("create_exception_object(shared, thread, class_name, message)")
        .expect("the VM-limit retry");
    assert!(
        vm_oome[..retry].contains("ARRAY_SIZE_EXCEEDS_VM_LIMIT"),
        "the collecting retry must be reserved for the VM-limit refusal"
    );
    let inner = body_of(src, "fn create_exception_object_for_class_inner(");
    let arm_start = inner
        .find("None if alloc_mode == ExceptionAlloc::NoCollect")
        .expect("the no-collect arm");
    let arm_len = inner[arm_start..]
        .find("None =>")
        .expect("the collecting arm follows");
    let arm = &inner[arm_start..arm_start + arm_len];
    for forbidden in [
        "maybe_gc_forced",
        "last_ditch",
        "gc_overhead_limit_exceeded",
    ] {
        assert!(
            !arm.contains(forbidden),
            "the no-collect arm calls `{forbidden}`"
        );
    }
    // And the collecting door is unchanged for every other exception.
    let collecting = &inner[arm_start + arm_len..];
    assert!(collecting.contains("maybe_gc_forced_pub_at("));
}
