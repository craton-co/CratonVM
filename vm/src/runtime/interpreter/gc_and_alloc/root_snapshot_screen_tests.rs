use super::*;
use crate::config::VmConfig;
use crate::runtime::frame::Frame;
use crate::vm::SharedVm;
use cratonvm_types::ClassId;

/// The operand-stack half of a frame's root scan must not be screened
/// more strictly than the locals half.
///
/// `Frame::scan_local_objects` screens a local with `is_heap_addr`
/// (alignment + arena containment) on purpose: `is_object_address` is a
/// full header probe and rejects a young / mid-init object whose header it
/// cannot yet vouch for, and dropping such a root reclaims a
/// still-referenced object. `ValueStack::scan_object_refs` was changed for
/// the same reason. `scan_frame_roots` then re-screened only that scan's
/// output with the strict probe, which put both fixes back — invisibly,
/// because the two screens agree on every HEALTHY object.
///
/// So the test builds an address where they provably disagree, asserts the
/// disagreement (a vacuous pass is the failure mode that matters here),
/// and then asserts the scan keeps it from BOTH slot kinds.
#[test]
fn operand_stack_roots_use_the_same_screen_as_locals() {
    // The collector is PINNED, not defaulted. The disagreement this test
    // manufactures is generational-specific: it corrupts the header's
    // `ObjectKind` byte so the strict probe rejects the address while
    // arena containment still accepts it. On ZGC `is_object_address` is a
    // registry-base lookup, not a header-tag probe, so the corruption does
    // not move it and the precondition assert below fails — which is what
    // happened when the default collector became `Zgc` on 2026-08-10.
    //
    // Left as a generational test rather than generalised, deliberately:
    // whether the operand-stack and locals screens also agree under ZGC's
    // registry-based probe is a real and separate question, and this
    // test's setup cannot ask it. It is not covered here.
    let cfg = VmConfig {
        gc_algorithm: crate::config::GcAlgorithm::Generational,
        ..VmConfig::default()
    };
    let shared = SharedVm::new(cfg);
    let heap = &shared.mem.heap;

    let obj = heap.alloc_object(ClassId::new(0), 0);
    let addr = obj.as_ptr() as usize;
    assert!(
        heap.is_object_address(addr).is_some(),
        "a freshly allocated object must pass the strict probe — otherwise \
         the disagreement asserted below would not be the one this test means"
    );

    // Make the strict probe reject it while arena containment still holds:
    // an out-of-range `ObjectKind` discriminant is exactly what
    // `is_object_address` screens for, and is what a mid-init or
    // conservatively-reached address looks like to it.
    // SAFETY: `addr` is a live object header in a mapped arena; the kind
    // tag is a single byte at a fixed in-bounds header offset.
    unsafe {
        *((addr + cratonvm_types::KIND_TAGS_BYTE_OFFSET) as *mut u8) = 0xEE;
    }
    assert!(
        heap.is_object_address(addr).is_none(),
        "precondition: the strict probe must now reject this address"
    );
    assert!(
        heap.is_heap_addr(addr).is_some(),
        "precondition: arena containment must still accept it"
    );

    // `aload_0; return` — local 0 is live at pc 0, so the per-bci liveness
    // mask keeps it and the two slot kinds are directly comparable.
    let mut frame = Frame::new(
        ClassId::new(0),
        "T".to_string(),
        "m".to_string(),
        "()V".to_string(),
        None,
        vec![0x2a, 0xb1],
        vec![],
        8,
        4,
        &[],
    );
    frame.set_local(0, Value::Object(Some(obj)));
    frame.stack.push(Value::Object(Some(obj))).unwrap();

    let mut roots = Vec::new();
    scan_frame_roots(&frame, &mut roots, heap, false, false, None);
    let hits = roots.iter().filter(|r| r.as_ptr() as usize == addr).count();
    assert_eq!(
        hits, 2,
        "both the local and the operand-stack slot must be rooted; {hits} of 2 \
         survived, so the operand-stack post-filter is dropping a root the \
         locals scan keeps"
    );
}

/// gch h1/jp (`gcf-f1f-debugger-local-reads-export-liveness-dead-stale-locals`,
/// fr request 3): the deposit's frame scan roots a liveness-dead object
/// local only while a debugger can read locals, and the plain scan is the
/// liveness-filtered one it always was.
#[test]
fn gch_h1jp_the_deposit_frame_scan_roots_a_dead_local_only_under_a_debugger() {
    let cfg = VmConfig {
        gc_algorithm: crate::config::GcAlgorithm::Generational,
        ..VmConfig::default()
    };
    let shared = SharedVm::new(cfg);
    let heap = &shared.mem.heap;
    let obj = heap.alloc_object(ClassId::new(0), 0);
    // `return` at pc 0: slot 1 is never read again, so it is dead to the
    // liveness filter (slot 0 is always kept).
    let mut frame = Frame::new(
        ClassId::new(0),
        "T".to_string(),
        "m".to_string(),
        "()V".to_string(),
        None,
        vec![0xb1],
        vec![],
        4,
        4,
        &[],
    );
    frame.set_local(1, Value::Object(Some(obj)));
    let mut plain = Vec::new();
    scan_frame_roots(&frame, &mut plain, heap, false, false, None);
    assert!(
        !plain.contains(&obj),
        "fixture: slot 1 must be liveness-dead"
    );
    let mut observed = Vec::new();
    scan_frame_roots(&frame, &mut observed, heap, false, true, None);
    assert!(
        observed.contains(&obj),
        "a debugger keeps every local alive"
    );
    assert!(
        !crate::runtime::jvmti::debugger_observes_locals(&shared),
        "no JDWP agent in the default config: the deposits stay byte-identical"
    );
}
