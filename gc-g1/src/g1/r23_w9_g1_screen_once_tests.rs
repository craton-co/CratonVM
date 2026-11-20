use super::*;

#[test]
fn r23_w9_a_proved_candidate_gets_the_full_supply_screen_verdict() {
    let gc = G1Collector::new(G1CollectorConfig {
        heap_size: 8 * 1024 * 1024,
        initial_heap_size: 8 * 1024 * 1024,
        region_size: 1024 * 1024,
        ..G1CollectorConfig::default()
    });
    let mut objs: Vec<ObjectRef> = Vec::new();
    for i in 0..32usize {
        objs.push(gc.alloc_object(ClassId::new(1), 1 + i % 4));
        objs.push(gc.alloc_array(ClassId::new(0), ArrayElementType::Byte, 1 + i * 3));
        objs.push(gc.alloc_array(ClassId::new(1), ArrayElementType::Reference, 1 + i % 5));
    }
    let regions = gc.regions.read();
    let view = || RegionView::Slice(&regions[..]);
    for (i, &o) in objs.iter().enumerate() {
        let addr = o.as_ptr() as usize;
        let (verdict, _) = gc.classify_candidate_header_view(view(), addr);
        if verdict != HeaderVerdict::Object {
            // Not a proved candidate: the screen-once arm is never asked.
            continue;
        }
        // SAFETY: a live object of this test's heap, read only.
        let header = unsafe { &*(addr as *const ObjectHeader) };
        let full = gc.addr_is_followable_object_view(view(), addr, "w9-test", GridProof::No);
        let once = !gc.note_implausible_legacy_header_view(
            view(),
            addr as *mut u8,
            header,
            "w9-test",
        );
        assert_eq!(once, full, "object {i}: the two verdicts agree");
    }
    assert!(
        objs.iter().any(|&o| {
            gc.classify_candidate_header_view(view(), o.as_ptr() as usize).0
                == HeaderVerdict::Object
        }),
        "the fixture has proved candidates"
    );
}
