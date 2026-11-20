use super::*;
use cratonvm_gc_core::tlab::Tlab;

const KIB: usize = 1024;
const MIB: usize = 1024 * 1024;

/// A 1 MiB / 1 MiB heap with the tail sink ON (latched when built).
fn heap_with_sink() -> GenerationalHeap {
    cratonvm_types::flags::with_thread_overrides(
        // gcj j2: the cursor arithmetic below pins the default carve, so the
        // smallest-whole-block refill (default ON since the j2 flip) is off.
        &[
            ("CRATONVM_GEN_TLAB_TAIL_SINK", Some("1")),
        ],
        || GenerationalHeap::with_sizes(MIB, MIB),
    )
}

/// A buffer over a fresh `size`-byte carve, armed with the heap as its
/// tail sink exactly as the VM's refill site arms it; and its base.
fn carve(heap: &GenerationalHeap, size: usize) -> (Tlab, usize) {
    let (ptr, got) = heap.refill_tlab(size).expect("room for the carve");
    assert_eq!(got, size, "the whole request was carved");
    // SAFETY: `refill_tlab` handed `[ptr, ptr + got)` to this caller alone,
    // zeroed and 8-aligned; nothing else is handed it until the retire.
    let mut tlab = unsafe { Tlab::new(ptr, got) };
    assert!(heap.tlab_tail_sink_enabled());
    // SAFETY: the heap outlives every buffer carved from it in these tests.
    unsafe { tlab.attach_tail_sink(heap) };
    (tlab, ptr as usize)
}

/// The young from-space's low bump cursor, as an address.
fn cursor(heap: &GenerationalHeap) -> usize {
    let from = heap.lock_young_from();
    from.base_ptr() as usize + from.used()
}

/// A young SLOW-path `long[]` whose footprint is `bytes` — the object a
/// keep sends outside the buffer — as its `[start, end)`.
fn slow_long_array(heap: &GenerationalHeap, bytes: usize) -> (usize, usize) {
    let len = (bytes - ARRAY_DATA_OFFSET) / 8;
    let obj = heap
        .try_alloc_array(ClassId::new(0), ArrayElementType::Long, len)
        .expect("young has room for the slow-path object");
    let at = obj.as_ptr() as usize;
    (at, at + ARRAY_DATA_OFFSET + len * 8)
}

fn assert_disjoint(spans: &[(&str, usize, usize)]) {
    for (i, &(n1, s1, e1)) in spans.iter().enumerate() {
        assert!(s1 < e1, "{n1} is empty");
        for &(n2, s2, e2) in &spans[i + 1..] {
            assert!(
                e1 <= s2 || e2 <= s1,
                "{n1} [{s1:#x},{e1:#x}) and {n2} [{s2:#x},{e2:#x}) share bytes: \
                 one span handed to two owners",
            );
        }
    }
}

/// The keep sequence: a buffer that missed is KEPT (it keeps bumping) while
/// the missed object is allocated on the young slow path right after it;
/// another buffer is carved; the kept one retires with a tail that no
/// longer ends at the cursor (free-listed), a slow-path object reuses that
/// tail, the later buffer retires unused (a retraction) and a third is
/// carved from the retracted cursor. No byte is handed out twice, and the
/// slow-path object's body survives every later carve (a carve over it
/// would zero it — the "static reads null" face of the page).
#[test]
fn a_keep_and_the_tail_sink_never_hand_a_byte_out_twice() {
    let heap = heap_with_sink();
    let zero_repairs = heap.stats().snapshot().young_zero_repairs;
    let (mut a, a0) = carve(&heap, 64 * KIB);
    a.alloc(40 * KIB, 8).expect("inside A");
    // The kept miss: a 32 KiB object does not fit A's 24 KiB tail, and goes
    // to the young slow path, which bumps right past A.
    let x = slow_long_array(&heap, 32 * KIB);
    assert_eq!(
        x.0,
        a0 + 64 * KIB,
        "the slow path bumps past the kept buffer"
    );
    // SAFETY: the first data word of X, inside the span just allocated.
    unsafe { std::ptr::write((x.0 + ARRAY_DATA_OFFSET) as *mut u64, 0x11A7_E0B1) };
    // The kept buffer keeps serving (the JIT's inline bump, here `alloc`).
    let a_more = a.alloc(16 * KIB, 8).expect("the kept tail still serves") as usize;
    assert_eq!(a_more, a0 + 40 * KIB);
    let (mut b, b0) = carve(&heap, 64 * KIB);
    assert_eq!(b0, x.1, "B follows X");
    let used = heap.lock_young_from().used();
    a.retire();
    assert_eq!(
        heap.lock_young_from().used(),
        used,
        "A's tail ends at X, not at the cursor: no retraction"
    );
    assert_eq!(
        heap.lock_young_from().free_list_bytes(),
        8 * KIB,
        "A's tail is listed"
    );
    let y = slow_long_array(&heap, KIB);
    assert!(
        y.0 >= a0 + 56 * KIB && y.1 <= a0 + 64 * KIB,
        "the free list served Y from A's tail: [{:#x},{:#x})",
        y.0,
        y.1
    );
    b.retire();
    assert_eq!(
        cursor(&heap),
        b0,
        "B came back whole: the cursor retracts onto its base"
    );
    assert!(x.1 <= cursor(&heap), "X stays under the cursor");
    let (mut c, c0) = carve(&heap, 64 * KIB);
    assert_eq!(
        c0, b0,
        "A's remaining 7 KiB cannot hold C: carved from the cursor"
    );
    c.alloc(64 * KIB, 8).expect("C is all C's");
    assert_disjoint(&[
        ("A (consumed)", a0, a0 + 56 * KIB),
        ("X", x.0, x.1),
        ("Y", y.0, y.1),
        ("C", c0, c0 + 64 * KIB),
    ]);
    // SAFETY: X's first data word, still allocated.
    let body = unsafe { std::ptr::read((x.0 + ARRAY_DATA_OFFSET) as *const u64) };
    assert_eq!(body, 0x11A7_E0B1, "X's body survived every later hand-out");
    assert_eq!(
        heap.stats().snapshot().young_zero_repairs,
        zero_repairs,
        "every bump-tail carve over a returned span read zero"
    );
    c.retire();
}

/// A retiring buffer's tail that ends at ANOTHER live buffer is listed, not
/// retracted, so the cursor never drops under the live one; the live one's
/// own retire retracts only onto its cursor, never below what it handed
/// out; and the next carve starts exactly there.
#[test]
fn a_retraction_never_lowers_the_cursor_under_a_live_buffer() {
    let heap = heap_with_sink();
    let (mut a, a0) = carve(&heap, 64 * KIB);
    let (mut b, b0) = carve(&heap, 64 * KIB);
    assert_eq!(b0, a0 + 64 * KIB);
    a.alloc(128, 8).expect("inside A");
    b.alloc(128, 8).expect("inside B");
    a.retire();
    assert_eq!(
        cursor(&heap),
        b0 + 64 * KIB,
        "B is still live above A's tail"
    );
    let more = b.alloc(KIB, 8).expect("B still owns its chunk") as usize;
    assert_eq!(more, b0 + 128);
    b.retire();
    assert_eq!(
        cursor(&heap),
        b0 + 128 + KIB,
        "B's retire retracts onto its own cursor, not below it"
    );
    let (c_ptr, c_got) = heap.refill_tlab(64 * KIB).expect("room for C");
    assert_eq!(c_ptr as usize, a0 + 128, "reuses A's listed tail");
    assert_eq!(c_got, 64 * KIB - 128);
    let mut c = unsafe { Tlab::new(c_ptr, c_got) };
    unsafe { c.attach_tail_sink(&heap) };
    assert_disjoint(&[
        ("A (consumed)", a0, a0 + 128),
        ("B (consumed)", b0, b0 + 128 + KIB),
        ("C", c_ptr as usize, c_ptr as usize + c_got),
    ]);
    c.retire();
}

/// `gengc-r5w3-oldgen7-young-uncommit-ignores-the-xms-floor`: without
/// `-Xms` the evacuated semi-space goes back to the OS as before; with it,
/// the arena keeps at least the recorded floor committed, and the return
/// value is the net release.
#[test]
fn the_evacuated_semi_space_keeps_the_xms_floor_committed() {
    if Arena::new(16 * MIB).commit_bits().is_none() {
        // The wholly committed fallback store releases nothing to floor.
        return;
    }
    let run = |xms: Option<usize>| {
        cratonvm_types::flags::with_thread_overrides(&[("CRATONVM_GEN_UNCOMMIT", None)], || {
            let heap = GenerationalHeap::with_capacity(64 * MIB);
            if let Some(bytes) = xms {
                heap.commit_initial_heap(bytes);
            }
            let mut evacuated = Arena::new(16 * MIB);
            let _ = evacuated.commit_initial_prefix(12 * MIB);
            let before = evacuated.committed_bytes();
            let released = heap.uncommit_evacuated_young(&mut evacuated);
            (
                heap.young_xms_floor(),
                before,
                released,
                evacuated.committed_bytes(),
            )
        })
    };
    let (floor, before, released, after) = run(None);
    assert_eq!(floor, 0, "no -Xms, no floor");
    assert!(
        after < before,
        "the evacuated semi-space goes back ({after} of {before})"
    );
    assert_eq!(released, before - after);
    let (floor, before, released, after) = run(Some(32 * MIB));
    assert!(floor > 0, "-Xms records a young floor");
    assert!(
        after >= floor.min(16 * MIB),
        "the floor stays committed: {after} < {floor}"
    );
    assert_eq!(
        released,
        before.saturating_sub(after),
        "the NET release is reported"
    );
}
