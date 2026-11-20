// ---- dup2_x2 form classification (JVMS 6.5) -------------------------
//
// Regression cover for a bug that lived in BOTH interpreter copies of
// this opcode until 2026-09-16: neither looked at `value3`, so every
// category-1/category-1 top was treated as FORM 1 and a fourth entry was
// popped. FORM 3 has no fourth entry. Found by `difftest fuzz-jit`, where
// it made the interpreter -- the differential oracle -- panic on
// `JitFuzz_37.t(IJFD)J pc=49` with `index out of bounds: the len is 24 but
// the index is 18446744073709551615`. The x64 backend had all four forms
// right, so the JIT was never the one that was wrong.

fn stack_of(entries: &[(CompactValue, u8)]) -> ValueStack {
    let mut st = ValueStack::new(16);
    for &(cv, k) in entries {
        st.push_with_kind(cv, k).expect("fits");
    }
    st
}

fn one() -> (CompactValue, u8) {
    (CompactValue::int(1), KIND_UNKNOWN)
}

fn two() -> (CompactValue, u8) {
    (CompactValue::long(1), KIND_LONG)
}

/// i51-L7: the raw top-of-stack reference read admits only a non-null
/// object slot that is not kind-marked long or double, and the in-place
/// int answer leaves a plain int slot.
#[test]
fn i51_l7_the_raw_top_reference_read_and_the_in_place_int() {
    let mut st = ValueStack::new(4);
    assert_eq!(st.peek_object_ptr(), None, "an empty stack");
    st.push(Value::Object(None)).unwrap();
    assert_eq!(st.peek_object_ptr(), None, "null");
    st.push_int(7).unwrap();
    assert_eq!(st.peek_object_ptr(), None, "an int");
    // SAFETY: never dereferenced; only the slot's bits are read.
    let fake = unsafe { ObjectRef::from_raw(0x7f00_1000 as *mut u8) };
    st.push(Value::Object(Some(fake))).unwrap();
    assert_eq!(st.peek_object_ptr(), Some(0x7f00_1000), "a reference");
    st.pop().unwrap();
    // A long whose bits sit in the object sub-tag is marked, and refused.
    let collided = CompactValue::object(0x7f00_1000).to_bits() as i64;
    st.push(Value::Long(collided)).unwrap();
    assert_eq!(st.peek_object_ptr(), None, "a kind-marked long");
    st.replace_top_with_int(1);
    assert_eq!(st.len(), 3);
    assert_eq!(st.peek_object_ptr(), None);
    assert!(
        matches!(st.pop().unwrap(), Value::Int(1)),
        "the mark is gone with the long"
    );
}

#[test]
fn dup2_x2_form_1_is_four_category_one_entries() {
    let st = stack_of(&[one(), one(), one(), one()]);
    assert_eq!(st.dup2_x2_form(), Some(Dup2X2Form::Form1));
    assert_eq!(Dup2X2Form::Form1.consumed(), 4);
    assert_eq!(Dup2X2Form::Form1.push_order(), &[1, 0, 3, 2, 1, 0]);
}

#[test]
fn dup2_x2_form_2_is_a_category_two_top_over_two_singles() {
    // value3, value2 category 1; value1 category 2.
    let st = stack_of(&[one(), one(), two()]);
    assert_eq!(st.dup2_x2_form(), Some(Dup2X2Form::Form2));
    assert_eq!(Dup2X2Form::Form2.consumed(), 3);
}

#[test]
fn dup2_x2_form_3_is_two_singles_over_a_category_two() {
    // This is the form both interpreters missed: three entries, not four.
    let st = stack_of(&[two(), one(), one()]);
    assert_eq!(st.dup2_x2_form(), Some(Dup2X2Form::Form3));
    assert_eq!(Dup2X2Form::Form3.consumed(), 3);
    assert_eq!(Dup2X2Form::Form3.push_order(), &[1, 0, 2, 1, 0]);
}

#[test]
fn dup2_x2_form_4_is_two_category_two_entries() {
    let st = stack_of(&[two(), two()]);
    assert_eq!(st.dup2_x2_form(), Some(Dup2X2Form::Form4));
    assert_eq!(Dup2X2Form::Form4.consumed(), 2);
}

#[test]
fn dup2_x2_form_3_is_not_mistaken_for_form_1_when_a_fourth_entry_exists() {
    // The shape that does NOT fault. With something live beneath the
    // shuffle, the old code popped it as `value4` and left it duplicated
    // in the result instead of underflowing -- a silent wrong answer,
    // which is why the crashing three-entry case was the lucky one.
    let st = stack_of(&[one(), two(), one(), one()]);
    assert_eq!(st.dup2_x2_form(), Some(Dup2X2Form::Form3));
}

#[test]
fn dup2_x2_refuses_a_stack_too_shallow_for_any_form() {
    assert_eq!(ValueStack::new(16).dup2_x2_form(), None);
    assert_eq!(stack_of(&[one()]).dup2_x2_form(), None);
    // Two category-1 entries could only be FORM 1 or FORM 3, and both
    // need a value3.
    assert_eq!(stack_of(&[one(), one()]).dup2_x2_form(), None);
    // FORM 3 needs its value3; two singles alone are not enough.
    assert_eq!(stack_of(&[one(), two()]).dup2_x2_form(), None);
}

#[test]
fn dup2_x2_refuses_shapes_no_jvms_form_admits() {
    // A category-1 value1 over a category-2 value2 is not any of the four
    // forms; verified bytecode never produces it.
    assert_eq!(stack_of(&[one(), two(), one()]).dup2_x2_form(), None);
    // FORM 2 requires value3 category 1.
    assert_eq!(stack_of(&[two(), one(), two()]).dup2_x2_form(), None);
    // FORM 1 requires value4 category 1.
    assert_eq!(stack_of(&[two(), one(), one(), one()]).dup2_x2_form(), None);
}

#[test]
fn every_dup2_x2_push_order_stays_inside_what_the_form_consumed() {
    for form in [
        Dup2X2Form::Form1,
        Dup2X2Form::Form2,
        Dup2X2Form::Form3,
        Dup2X2Form::Form4,
    ] {
        for &d in form.push_order() {
            assert!(
                d < form.consumed(),
                "{form:?} pushes depth {d} but only consumed {}",
                form.consumed()
            );
        }
        // Depth is conserved in JVMS *slots*, not in entries: this VM
        // holds a category-2 value in one entry, so the entry counts
        // differ per form (FORM 2 is 3 in / 4 out, FORM 3 is 3 in / 5
        // out) while all four are 4 slots in and 6 slots out.
        let cat2_at_depth: &[bool] = match form {
            Dup2X2Form::Form1 => &[false, false, false, false],
            Dup2X2Form::Form2 => &[true, false, false],
            Dup2X2Form::Form3 => &[false, false, true],
            Dup2X2Form::Form4 => &[true, true],
        };
        assert_eq!(cat2_at_depth.len(), form.consumed());
        let slots = |ds: &[usize]| -> usize {
            ds.iter()
                .map(|&d| if cat2_at_depth[d] { 2 } else { 1 })
                .sum()
        };
        let consumed_depths: Vec<usize> = (0..form.consumed()).collect();
        assert_eq!(slots(&consumed_depths), 4, "{form:?} consumes 4 slots");
        assert_eq!(slots(form.push_order()), 6, "{form:?} pushes 6 slots");
    }
}

use super::*;

/// gcj j2/pn2: the operand-stack conservative probe's skip rule reads the
/// kind marks a `long` / `double` push leaves; a reference or an `int`
/// push leaves no primitive mark.
#[test]
fn gcj2pn2_conservative_probe_skips_long_and_double_kind_slots() {
    let mut stack = ValueStack::new(8);
    stack.push(Value::Long(0x1000)).unwrap();
    stack.push(Value::Double(1.5)).unwrap();
    stack.push(Value::Int(7)).unwrap();
    stack.push(Value::Object(None)).unwrap();
    let kind = |i: usize| stack.kinds.get(i).copied();
    assert!(conservative_slot_is_primitive_kind(kind(0)), "long");
    assert!(conservative_slot_is_primitive_kind(kind(1)), "double");
    assert!(!conservative_slot_is_primitive_kind(kind(2)), "int");
    assert!(!conservative_slot_is_primitive_kind(kind(3)), "reference");
    assert!(!conservative_slot_is_primitive_kind(None), "no mark");
}

#[test]
fn push_and_pop() {
    let mut stack = ValueStack::new(10);
    stack.push(Value::Int(42)).unwrap();
    stack.push(Value::Int(99)).unwrap();
    assert_eq!(stack.pop_int().unwrap(), 99);
    assert_eq!(stack.pop_int().unwrap(), 42);
}

#[test]
fn pop_empty_fails() {
    let mut stack = ValueStack::new(10);
    assert!(stack.pop().is_err());
}

#[test]
fn overflow_fails() {
    let mut stack = ValueStack::new(2);
    stack.push(Value::Int(1)).unwrap();
    stack.push(Value::Int(2)).unwrap();
    assert!(stack.push(Value::Int(3)).is_err());
}

/// B4 (audit `vm-runtime.md`): an operand-stack overflow must surface as
/// the *catchable* `RuntimeError::StackOverflowError` (which the
/// interpreter routes to a real `java/lang/StackOverflowError`), NOT the
/// uncatchable `RuntimeError::NotImplemented` it previously returned. This
/// covers every `push*` overflow site that shares the contract.
#[test]
fn overflow_returns_stack_overflow_error() {
    // Generic `push`.
    let mut stack = ValueStack::new(1);
    stack.push(Value::Int(1)).unwrap();
    assert!(
        matches!(
            stack.push(Value::Int(2)),
            Err(RuntimeError::StackOverflowError)
        ),
        "push overflow must be StackOverflowError, not NotImplemented"
    );

    // Type-specialised hot-path pushes share the same overflow contract.
    let mut s_int = ValueStack::new(0);
    assert!(matches!(
        s_int.push_int(1),
        Err(RuntimeError::StackOverflowError)
    ));

    let mut s_long = ValueStack::new(0);
    assert!(matches!(
        s_long.push_long(1),
        Err(RuntimeError::StackOverflowError)
    ));

    let mut s_float = ValueStack::new(0);
    assert!(matches!(
        s_float.push_float(1.0),
        Err(RuntimeError::StackOverflowError)
    ));

    let mut s_double = ValueStack::new(0);
    assert!(matches!(
        s_double.push_double(1.0),
        Err(RuntimeError::StackOverflowError)
    ));

    let mut s_null = ValueStack::new(0);
    assert!(matches!(
        s_null.push_null(),
        Err(RuntimeError::StackOverflowError)
    ));
}

#[test]
fn soa_roundtrip_all_types() {
    let mut stack = ValueStack::new(10);
    stack.push(Value::Int(42)).unwrap();
    stack.push(Value::Long(999999999999)).unwrap();
    stack.push(Value::Float(3.15)).unwrap();
    stack.push(Value::Double(2.719)).unwrap();
    stack.push(Value::Object(None)).unwrap();
    stack.push(Value::Uninitialized).unwrap();

    assert_eq!(stack.pop().unwrap(), Value::Uninitialized);
    assert!(stack.pop().unwrap().is_null());
    // Bit equality: a value read back out of a stack slot has been
    // through no rounding step, so the round trip is exact or the slot
    // corrupted it. A tolerance here can only hide the second case.
    assert_eq!(stack.pop_double().unwrap().to_bits(), 2.719f64.to_bits());
    assert_eq!(stack.pop_float().unwrap().to_bits(), 3.15f32.to_bits());
    assert_eq!(stack.pop_long().unwrap(), 999999999999);
    assert_eq!(stack.pop_int().unwrap(), 42);
}

#[test]
fn peek_works() {
    let mut stack = ValueStack::new(10);
    stack.push(Value::Int(42)).unwrap();
    stack.push(Value::Int(99)).unwrap();
    assert_eq!(stack.peek().as_int(), Some(99));
    assert_eq!(stack.peek_at(1).as_int(), Some(42));
}

/// Regression: longs whose bit pattern collides with the NaN-tag int
/// space must survive `push_long` → `pop_long` bit-exactly *whenever the
/// collision is resolvable*. Before the 2026-05-28 fix `pop_long` widened
/// every `SUB_INT`-tagged slot via `as_int()`, dropping the upper bits of
/// every such long — including the BC safegcd `Mod.updateDE30`/`updateFG30`
/// accumulators (signed int*int products landing in the 0xFFFC_…/0xFFFE_…
/// band with nonzero magnitude). See
/// bc-ec-mod-mododdinverse-investigation.md.
#[test]
fn pop_long_preserves_resolvable_nan_tag_collisions() {
    // 0xFFFC_….: full NaN-box marker set; the 3-bit sub-tag (bits 49-47)
    // selects which "non-double" tag the bits masquerade as. A real
    // `CompactValue::int` stores `n as u32` (payload < 2^32), so a SUB_INT
    // slot whose payload bits 32-46 are set CANNOT be a real int — it is a
    // collision long and must reinterpret bit-exact.
    const NANBOX: u64 = 0xFFFC_0000_0000_0000;
    let resolvable: &[i64] = &[
        (NANBOX | (1u64 << 32)) as i64,          // SUB_INT, payload bit 32 set
        (NANBOX | 0x7FFF_FFFF_FFFF) as i64,      // SUB_INT, all 47 payload bits set
        (NANBOX | (1u64 << 47)) as i64,          // SUB_FLOAT pattern (any tag → reinterpret)
        (NANBOX | (2u64 << 47) | 0x55) as i64,   // SUB_OBJECT pattern, nonzero payload
        (NANBOX | (5u64 << 47) | 0x1234) as i64, // SUB_RETADDR pattern, payload bits set
        -1,                                      // natural SUB_LONG_HI
        i64::MIN,                                // untagged fast path
        i64::MAX,
        123_456_789_012_345,
    ];
    for &v in resolvable {
        let mut stack = ValueStack::new(2);
        stack.push_long(v).unwrap();
        assert_eq!(
            stack.pop_long().unwrap(),
            v,
            "push_long/pop_long must round-trip {v:#018x} bit-exact",
        );
    }
}

/// Documented residual: a `SUB_INT` slot with payload < 2^32 is
/// bit-identical to a real `CompactValue::int`, so `pop_long` keeps the
/// JVMS i2l-widen contract there (it cannot tell the two apart from a
/// single 8-byte slot). Closing this fully needs a parallel stack type
/// tag — see bc-ec-mod-mododdinverse-investigation.md. Pin the
/// behaviour so a future encoding change is a conscious decision.
#[test]
fn pop_long_widens_unresolvable_int_collision() {
    const NANBOX: u64 = 0xFFFC_0000_0000_0000;
    // payload 0 → widens to 0L; payload 0x1234 → widens to 0x1234L.
    for &(bits, widened) in &[(NANBOX, 0i64), (NANBOX | 0x1234, 0x1234i64)] {
        let mut stack = ValueStack::new(2);
        // FIX: push via the *unmarked* compact path (`push_compact`), NOT
        // `push_long`. `push_long` is a genuine long producer: it marks the
        // slot `KIND_LONG`, which `pop_long` honors by reading the bits
        // bit-exact (see `pop_long_preserves_resolvable_nan_tag_collisions`,
        // which pushes via `push_long` and asserts bit-exact round-trip). A
        // `KIND_LONG` slot therefore NEVER widens — so the old `push_long`
        // here contradicted the design and yielded the raw bits
        // (-1125899906842624) instead of the widened value.
        //
        // The i2l-widening fallback this test pins is only reachable for an
        // UNRESOLVABLE collision slot, i.e. one pushed *without* the long
        // mark (e.g. `lload_N`'s `push_compact`, or a synthetic int-where-
        // long). Such a SUB_INT slot with payload < 2^32 is bit-identical to
        // a real `CompactValue::int`, so `pop_long` applies JVMS i2l
        // sign-extension — the documented unsalvageable residual.
        stack.push_compact(crate::types::CompactValue::long(bits as i64));
        assert_eq!(stack.pop_long().unwrap(), widened);
    }
}

/// Regression: the full `lload` → consume sequence the interpreter runs.
/// `lload_N` (fast path) now pushes the local's raw CompactValue bits via
/// `push_compact`; the consuming long opcode pops via `pop_long`. Both
/// stages must be bit-exact for resolvable collision-pattern longs.
#[test]
fn lload_then_pop_long_is_bit_exact_for_collisions() {
    const NANBOX: u64 = 0xFFFC_0000_0000_0000;
    for &v in &[
        (NANBOX | 0x7FFF_FFFF_FFFF) as i64, // SUB_INT, payload bits 32-46 set
        (NANBOX | (2u64 << 47) | 0x55) as i64, // SUB_OBJECT pattern
    ] {
        // Simulate `lstore` writing the slot, then `lload` reading it: a
        // local slot stores the long verbatim via CompactValue::long.
        let local = crate::types::CompactValue::long(v);
        let mut stack = ValueStack::new(2);
        stack.push_compact(local); // lload_N
        assert_eq!(stack.pop_long().unwrap(), v, "lload/pop {v:#018x}");
    }
}

/// SCAN/UPDATE SYMMETRY: a smuggled jobject reference (JNI long-as-jobject
/// convention) that `scan_object_refs` yields as a root MUST be rewritten
/// by `update_object_refs` to the relocated address after a moving GC.
///
/// This pins the asymmetry fix: previously the scan rooted such a smuggle
/// out of the `CompactTag::Long | Double` hybrid branch, but the update
/// only had a `Double` arm — so the rooted slot was never remapped and the
/// reference went stale (UAF / wrong object) the instant the referent
/// moved. The test allocates two live heap objects, smuggles object A's
/// address into a long-marked slot, confirms the scan roots it, then
/// relocates A→B via the pointer_map and asserts the slot now carries B's
/// address.
#[test]
fn smuggled_jobject_is_rooted_and_remapped_symmetrically() {
    use crate::memory::VmHeap;
    use cratonvm_gc::GcBackend;
    use cratonvm_types::ClassId;
    use std::collections::HashMap;

    let heap = VmHeap::new(GcBackend::Generational, 16 * 1024 * 1024);
    // Two real, live heap objects: A is the pre-GC referent, B stands in
    // for A's post-compaction location. Both addresses pass `is_heap_addr`.
    let a = heap.alloc_object(ClassId::new(0), 0);
    let b = heap.alloc_object(ClassId::new(0), 0);
    let old_addr = a.as_ptr() as u64;
    let new_addr = b.as_ptr() as u64;
    assert_eq!(old_addr & 0x7, 0, "heap objects are 8-byte aligned");
    assert_eq!(new_addr & 0x7, 0, "heap objects are 8-byte aligned");
    assert_ne!(old_addr, new_addr);

    // Smuggle A's address into a slot via a genuine long producer (the
    // JNI long-as-jobject contract: a heap pointer carried in a long
    // slot). `push_long` marks the slot `KIND_LONG`; a low heap address is
    // not NaN-tagged, so the slot reports `CompactTag::Double` raw bits —
    // exactly the shape `scan_object_refs`'s hybrid branch roots.
    // Mint-provenance (2026-07-03): the rewrite arm only remaps values
    // registered at a mint chokepoint — model the JNI mint explicitly.
    crate::memory::smuggled_longs::record_minted_long(&heap, old_addr);
    let mut stack = ValueStack::new(4);
    stack.push_long(old_addr as i64).unwrap();

    // SCAN: the smuggle must be yielded as a root.
    let mut roots = Vec::new();
    stack.scan_object_refs(&mut roots, &heap);
    assert!(
        roots.iter().any(|r| r.as_ptr() as u64 == old_addr),
        "scan_object_refs must root the smuggled jobject (addr {old_addr:#x})"
    );

    // UPDATE: relocate A → B and assert the slot is rewritten — the half
    // that was missing for the `Long`-classified arm.
    let mut map = cratonvm_types::PointerMap::default();
    map.insert(old_addr as usize, new_addr as usize);
    stack.update_object_refs(&map, &heap);

    let slot = stack.peek_compact();
    assert_eq!(
        slot.to_bits(),
        new_addr,
        "update_object_refs must remap the rooted smuggle to the moved address"
    );
    crate::memory::smuggled_longs::forget_heap(&heap);
}

#[test]
fn smuggled_jobject_remapped_when_old_region_freed_g1() {
    // Step 5 GAP A regression: under G1 the evacuated CSet region is reset to
    // `Free`, so post-GC `is_heap_addr(old_ptr)` returns None for a genuinely
    // moved object. The remap must still rewrite the smuggle (gate on the live
    // `new_addr`, not the freed `old_ptr`). Model the freed from-space with an
    // `old_addr` that is not in any live region while `new_addr` is a real,
    // live object — exactly the asymmetry G1's CSet-free creates.
    use crate::memory::VmHeap;
    use cratonvm_gc::GcBackend;
    use cratonvm_types::ClassId;
    use std::collections::HashMap;

    let heap = VmHeap::new(GcBackend::G1, 16 * 1024 * 1024);
    let b = heap.alloc_object(ClassId::new(0), 0);
    let new_addr = b.as_ptr() as u64;
    // A pointer-shaped (8-aligned, sub-2^48) address that is NOT in any live
    // region — `is_heap_addr` returns None, as it would for a freed CSet slot.
    let old_addr: u64 = 0x1_0000;
    assert!(
        heap.is_heap_addr(old_addr as usize).is_none(),
        "old_addr must look freed (not in a live region)"
    );
    assert!(
        heap.is_heap_addr(new_addr as usize).is_some(),
        "new_addr must be a live relocated object"
    );

    // Mint-provenance (2026-07-03): register the handle as a JNI mint
    // would have; the `from_raw` round-trip ALSO records construction
    // provenance, making this test independent of cross-test
    // provenance-bitmap granule pollution (old_addr=0x1_0000 previously
    // passed `jlong_bits_as_aligned_object_ptr` only when another test in
    // the same process happened to record provenance in that granule).
    // SAFETY: the pointer is only used for its bits (never dereferenced).
    let _ = unsafe { cratonvm_types::ObjectRef::from_raw(old_addr as *mut u8) };
    crate::memory::smuggled_longs::record_minted_long(&heap, old_addr);
    let mut stack = ValueStack::new(4);
    stack.push_long(old_addr as i64).unwrap();
    let mut map = cratonvm_types::PointerMap::default();
    map.insert(old_addr as usize, new_addr as usize);
    stack.update_object_refs(&map, &heap);

    assert_eq!(
        stack.peek_compact().to_bits(),
        new_addr,
        "smuggle must be remapped even though old_addr's region was freed (G1)"
    );
    crate::memory::smuggled_longs::forget_heap(&heap);
}

/// Mint-provenance gate (2026-07-03): a genuine primitive long whose bits
/// coincidentally equal a moved object's from-space address must NOT be
/// rewritten — the collision case the provenance-of-construction check
/// can never discriminate (the moved object is always a real,
/// once-constructed object).
#[test]
fn colliding_primitive_long_is_not_rewritten() {
    use crate::memory::VmHeap;
    use cratonvm_gc::GcBackend;
    use cratonvm_types::ClassId;
    use std::collections::HashMap;

    let heap = VmHeap::new(GcBackend::Generational, 16 * 1024 * 1024);
    let a = heap.alloc_object(ClassId::new(0), 0);
    let b = heap.alloc_object(ClassId::new(0), 0);
    let old_addr = a.as_ptr() as u64;
    let new_addr = b.as_ptr() as u64;

    // The primitive's value equals A's address, but it was NEVER minted
    // as a handle (no record_minted_long).
    //
    // This used to `return` early when the address was already minted,
    // because the registry was one process-global set and another test's
    // heap could have handed out an identical address — i.e. the test
    // silently skipped itself. The registry is now keyed on the heap, but
    // by its ADDRESS (`smuggled_longs::heap_id`): a `VmHeap` local of a
    // test reuses the stack slot, and its mapping the object addresses, of
    // an earlier test's heap on the same thread, so a test that minted and
    // did not `forget_heap` leaves its table to this one (the residual the
    // module docs of `smuggled_longs` record). Start clean.
    crate::memory::smuggled_longs::forget_heap(&heap);
    assert!(
        !crate::memory::smuggled_longs::is_minted(&heap, old_addr),
        "a fresh heap must start with no mints registered against it"
    );
    let mut stack = ValueStack::new(4);
    stack.push_long(old_addr as i64).unwrap();

    let mut map = cratonvm_types::PointerMap::default();
    map.insert(old_addr as usize, new_addr as usize);
    stack.update_object_refs(&map, &heap);

    assert_eq!(
        stack.peek_compact().to_bits(),
        old_addr,
        "an un-minted primitive long colliding with a moved address must be preserved"
    );
}

/// `capacity` is the allocated slot count, independent of the depth.
#[test]
fn capacity_reports_allocated_slots_not_depth() {
    let mut stack = ValueStack::new(5);
    assert_eq!(stack.capacity(), 5);
    stack.push_int(1).unwrap();
    assert_eq!((stack.len(), stack.capacity()), (1, 5));
}

/// The shared rewrite rule the native-call pinned-argument remaps in
/// `vm_exec.rs` now use: an unminted long is never admitted, a long minted
/// under its old bits or its relocation target is.
#[test]
fn smuggled_long_rewrite_admits_only_minted_handles() {
    use crate::memory::VmHeap;
    use cratonvm_gc::GcBackend;
    use cratonvm_types::ClassId;

    let heap = VmHeap::new(GcBackend::Generational, 16 * 1024 * 1024);
    let a = heap.alloc_object(ClassId::new(0), 0);
    let b = heap.alloc_object(ClassId::new(0), 0);
    let c = heap.alloc_object(ClassId::new(0), 0);
    let (old_addr, new_addr) = (a.as_ptr() as u64, b.as_ptr() as u64);
    // A `VmHeap` local of a test reuses the stack slot, and its mapping the
    // addresses, of an earlier test's heap on the same thread, and the mint
    // registry is keyed by the heap's ADDRESS (`smuggled_longs::heap_id`):
    // a test that minted and did not `forget_heap` leaves its table to this
    // one (the residual `smuggled_longs`' module docs record). Start clean.
    crate::memory::smuggled_longs::forget_heap(&heap);
    assert!(!super::smuggled_long_rewrite_admitted(
        &heap, old_addr, new_addr
    ));
    crate::memory::smuggled_longs::record_minted_long(&heap, old_addr);
    assert!(super::smuggled_long_rewrite_admitted(
        &heap, old_addr, new_addr
    ));
    // After the registry was remapped, only the target is registered.
    crate::memory::smuggled_longs::record_minted_long(&heap, c.as_ptr() as u64);
    assert!(super::smuggled_long_rewrite_admitted(
        &heap,
        0x10,
        c.as_ptr() as u64
    ));
    crate::memory::smuggled_longs::forget_heap(&heap);
}

/// i1 wave 9, lane L4 (the long-roots page's Change A): under
/// `scan_object_refs_ruled(.., true)` a `push_long` whose value is a live
/// object's address is rooted only once it is minted; the default scan and
/// `.., false` keep the loose rule and root it either way.
#[test]
fn minted_only_rule_roots_a_kind_marked_long_only_when_minted() {
    use crate::memory::VmHeap;
    use cratonvm_gc::GcBackend;
    use cratonvm_types::ClassId;

    if super::longroot_strict() || super::longrewrite_loose() {
        return; // an env override changes both arms; nothing to compare
    }
    let heap = VmHeap::new(GcBackend::Generational, 16 * 1024 * 1024);
    // A `VmHeap` local of a test reuses the stack slot, and its mapping the
    // addresses, of an earlier test's heap on the same thread, and the mint
    // registry is keyed by the heap's ADDRESS (`smuggled_longs::heap_id`):
    // a test that minted and did not `forget_heap` leaves its table to this
    // one (the residual `smuggled_longs`' module docs record). Start clean.
    crate::memory::smuggled_longs::forget_heap(&heap);
    let obj = heap.alloc_object(ClassId::new(0), 0);
    let addr = obj.as_ptr() as usize;
    let mut stack = ValueStack::new(2);
    stack.push_long(addr as i64).expect("push_long");
    let rooted = |minted_only: bool| {
        let mut roots = Vec::new();
        stack.scan_object_refs_ruled(&mut roots, &heap, minted_only);
        roots.iter().any(|r| r.as_ptr() as usize == addr)
    };
    let mut loose = Vec::new();
    stack.scan_object_refs(&mut loose, &heap);
    assert!(
        loose.iter().any(|r| r.as_ptr() as usize == addr),
        "the default scan roots a heap-address long (the loose rule)"
    );
    assert!(rooted(false), "`false` is the default scan");
    assert!(!rooted(true), "an unminted primitive long is not rooted");
    crate::memory::smuggled_longs::record_minted_long(&heap, addr as u64);
    assert!(rooted(true), "a minted handle is rooted under either rule");
    crate::memory::smuggled_longs::forget_heap(&heap);
}

// B12: `*_unchecked` guards are now `debug_assert!`, so the panic only
// fires in debug builds. Gate the `#[should_panic]` coverage on
// `debug_assertions` so release-mode test runs don't fail these.
#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "stack overflow")]
fn push_unchecked_panics_on_overflow() {
    let mut stack = ValueStack::new(1);
    stack.push_unchecked(Value::Int(1));
    stack.push_unchecked(Value::Int(2)); // should panic
}

#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "stack underflow")]
fn pop_unchecked_panics_on_underflow() {
    let mut stack = ValueStack::new(10);
    stack.pop_unchecked(); // should panic
}

#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "stack underflow")]
fn peek_panics_on_empty() {
    let stack = ValueStack::new(10);
    let _ = stack.peek(); // should panic
}

#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "stack underflow")]
fn peek_at_panics_out_of_range() {
    let mut stack = ValueStack::new(10);
    stack.push(Value::Int(1)).unwrap();
    let _ = stack.peek_at(5); // should panic
}

#[test]
fn peek_checked_empty_returns_err() {
    let stack = ValueStack::new(10);
    assert!(stack.peek_checked().is_err());
}

#[test]
fn get_value_out_of_range_returns_uninitialized() {
    let mut stack = ValueStack::new(10);
    stack.push(Value::Int(42)).unwrap();
    assert_eq!(stack.get_value(99), Value::Uninitialized);
}

#[test]
fn clear_resets_stack() {
    let mut stack = ValueStack::new(10);
    stack.push(Value::Int(1)).unwrap();
    stack.push(Value::Int(2)).unwrap();
    assert_eq!(stack.len(), 2);
    stack.clear();
    assert_eq!(stack.len(), 0);
    assert!(stack.is_empty());
}

#[test]
fn pop_wrong_type_returns_err() {
    let mut stack = ValueStack::new(10);
    stack.push(Value::Float(1.0)).unwrap();
    assert!(stack.pop_int().is_err());
}

#[test]
fn pop_long_widens_int() {
    // Session 71 stack-widening: pop_long accepts Int by sign-extension
    // (compensates for native methods or bytecode sequences that leave
    // an int where a long is expected).
    let mut stack = ValueStack::new(10);
    stack.push(Value::Int(115)).unwrap();
    assert_eq!(stack.pop_long().unwrap(), 115i64);
}

#[test]
fn pop_float_widens_int() {
    let mut stack = ValueStack::new(10);
    stack.push(Value::Int(7)).unwrap();
    assert_eq!(stack.pop_float().unwrap(), 7.0f32);
}

#[test]
fn pop_double_widens_int() {
    let mut stack = ValueStack::new(10);
    stack.push(Value::Int(9)).unwrap();
    assert_eq!(stack.pop_double().unwrap(), 9.0f64);
}

#[test]
fn from_pooled_creates_valid_stack() {
    let vals = Vec::new();
    let tags = Vec::new();
    let mut stack = ValueStack::from_pooled(vals, tags, 5);
    stack.push(Value::Int(42)).unwrap();
    assert_eq!(stack.pop_int().unwrap(), 42);
}

#[test]
fn into_inner_returns_vecs() {
    let mut stack = ValueStack::new(5);
    stack.push(Value::Int(1)).unwrap();
    let (vals, _tags) = stack.into_inner();
    assert!(vals.len() >= 5);
    // tags are now stored inline — an empty Vec is returned for the
    // legacy pool signature.
}

// ── Additional edge case tests ────────────────────────────────────

#[test]
fn stack_size_zero() {
    let stack = ValueStack::new(0);
    assert!(stack.is_empty());
    assert_eq!(stack.len(), 0);
}

#[test]
fn stack_size_zero_push_fails() {
    let mut stack = ValueStack::new(0);
    assert!(stack.push(Value::Int(1)).is_err());
}

#[test]
fn stack_size_zero_pop_fails() {
    let mut stack = ValueStack::new(0);
    assert!(stack.pop().is_err());
}

#[test]
fn stack_size_one_push_pop() {
    let mut stack = ValueStack::new(1);
    stack.push(Value::Int(77)).unwrap();
    assert_eq!(stack.len(), 1);
    assert!(!stack.is_empty());
    assert_eq!(stack.pop_int().unwrap(), 77);
    assert!(stack.is_empty());
}

#[test]
fn stack_size_one_overflow() {
    let mut stack = ValueStack::new(1);
    stack.push(Value::Int(1)).unwrap();
    assert!(stack.push(Value::Int(2)).is_err());
}

#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "stack overflow")]
fn push_unchecked_panics_on_full_size_zero() {
    let mut stack = ValueStack::new(0);
    stack.push_unchecked(Value::Int(1));
}

#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "stack underflow")]
fn peek_at_panics_on_empty_stack() {
    let stack = ValueStack::new(5);
    let _ = stack.peek_at(0);
}

#[test]
fn gc_update_object_refs_empty_map() {
    use crate::memory::vm_heap::{GcBackend, VmHeap};
    let heap = VmHeap::new(GcBackend::Generational, 16 * 1024 * 1024);
    let mut stack = ValueStack::new(5);
    stack.push(Value::Int(42)).unwrap();
    let empty_map = cratonvm_types::PointerMap::default();
    stack.update_object_refs(&empty_map, &heap);
    // Stack should be unchanged
    assert_eq!(stack.pop_int().unwrap(), 42);
}

#[test]
fn from_pooled_with_undersized_vecs() {
    // Start with tiny vecs that are smaller than max_size
    let vals = vec![0u64; 1];
    let tags = vec![0u8; 1];
    let mut stack = ValueStack::from_pooled(vals, tags, 10);
    // Should be able to push up to 10 elements
    for i in 0..10 {
        stack.push(Value::Int(i)).unwrap();
    }
    assert_eq!(stack.len(), 10);
    assert!(stack.push(Value::Int(99)).is_err());
}

#[test]
fn from_pooled_with_oversized_vecs() {
    // Start with vecs larger than max_size -- should resize down
    let vals = vec![0xFFu64; 100];
    let tags = vec![0xFFu8; 100];
    let mut stack = ValueStack::from_pooled(vals, tags, 3);
    stack.push(Value::Int(1)).unwrap();
    stack.push(Value::Int(2)).unwrap();
    stack.push(Value::Int(3)).unwrap();
    assert!(stack.push(Value::Int(4)).is_err());
}

#[test]
fn into_inner_preserves_capacity() {
    let mut stack = ValueStack::new(8);
    stack.push(Value::Int(10)).unwrap();
    stack.push(Value::Long(20)).unwrap();
    let (vals, tags) = stack.into_inner();
    // Vecs should have at least max_size elements
    assert!(vals.len() >= 8);
    // Tags are no longer stored; an empty Vec is returned.
    assert!(tags.is_empty());
    // Reuse them
    let mut stack2 = ValueStack::from_pooled(vals, tags, 8);
    stack2.push(Value::Int(30)).unwrap();
    assert_eq!(stack2.pop_int().unwrap(), 30);
}

/// `take_inner_in_place` must hand the pool exactly what `into_inner`
/// hands it, and must leave the husk describing an EMPTY stack.
///
/// The husk half is the part that matters and the part a naive
/// implementation gets wrong: `mem::take`ing `slots` while leaving `len`
/// at its old depth yields a stack that claims elements it does not have,
/// and the husk is observable (a GC root scan can walk this thread's
/// frames) until the enclosing frame is dropped.
#[test]
fn take_inner_in_place_matches_into_inner_and_empties_the_husk() {
    let mut owned = ValueStack::new(8);
    owned.push(Value::Int(10)).unwrap();
    owned.push(Value::Long(20)).unwrap();
    let (want_vals, want_tags) = owned.into_inner();

    let mut husk = ValueStack::new(8);
    husk.push(Value::Int(10)).unwrap();
    husk.push(Value::Long(20)).unwrap();
    let (got_vals, got_tags) = husk.take_inner_in_place();

    assert_eq!(got_vals, want_vals, "pooled value buffer must match");
    assert!(got_tags.is_empty(), "tag half is returned empty");
    assert_eq!(got_tags.capacity(), want_tags.capacity());

    assert_eq!(husk.len(), 0, "husk must not claim a depth it cannot serve");
    assert!(husk.is_empty());

    // And the harvested buffers still round-trip through the pool.
    let mut reused = ValueStack::from_pooled(got_vals, got_tags, 8);
    reused.push(Value::Int(30)).unwrap();
    assert_eq!(reused.pop_int().unwrap(), 30);
}

#[test]
fn multiple_push_pop_cycles() {
    let mut stack = ValueStack::new(4);
    // Cycle 1
    stack.push(Value::Int(1)).unwrap();
    stack.push(Value::Int(2)).unwrap();
    assert_eq!(stack.pop_int().unwrap(), 2);
    assert_eq!(stack.pop_int().unwrap(), 1);
    assert!(stack.is_empty());

    // Cycle 2
    stack.push(Value::Long(100)).unwrap();
    stack.push(Value::Float(1.5)).unwrap();
    stack.push(Value::Double(2.5)).unwrap();
    assert_eq!(stack.pop_double().unwrap().to_bits(), 2.5f64.to_bits());
    assert_eq!(stack.pop_float().unwrap().to_bits(), 1.5f32.to_bits());
    assert_eq!(stack.pop_long().unwrap(), 100);
    assert!(stack.is_empty());

    // Cycle 3: fill to capacity
    for i in 0..4 {
        stack.push(Value::Int(i)).unwrap();
    }
    assert_eq!(stack.len(), 4);
    stack.clear();
    assert!(stack.is_empty());
}

#[test]
fn stack_depth_tracking_accuracy() {
    let mut stack = ValueStack::new(10);
    assert_eq!(stack.len(), 0);

    stack.push(Value::Int(1)).unwrap();
    assert_eq!(stack.len(), 1);

    stack.push(Value::Int(2)).unwrap();
    assert_eq!(stack.len(), 2);

    let _ = stack.pop();
    assert_eq!(stack.len(), 1);

    stack.push(Value::Int(3)).unwrap();
    stack.push(Value::Int(4)).unwrap();
    assert_eq!(stack.len(), 3);

    stack.clear();
    assert_eq!(stack.len(), 0);

    // push_unchecked also tracks depth
    stack.push_unchecked(Value::Int(10));
    assert_eq!(stack.len(), 1);

    let _ = stack.pop_unchecked();
    assert_eq!(stack.len(), 0);
}

#[test]
fn peek_at_all_positions() {
    let mut stack = ValueStack::new(5);
    stack.push(Value::Int(10)).unwrap();
    stack.push(Value::Int(20)).unwrap();
    stack.push(Value::Int(30)).unwrap();

    assert_eq!(stack.peek_at(0).as_int(), Some(30)); // top
    assert_eq!(stack.peek_at(1).as_int(), Some(20)); // second
    assert_eq!(stack.peek_at(2).as_int(), Some(10)); // bottom
}

#[test]
fn get_value_within_bounds() {
    let mut stack = ValueStack::new(5);
    stack.push(Value::Int(10)).unwrap();
    stack.push(Value::Int(20)).unwrap();
    assert_eq!(stack.get_value(0).as_int(), Some(10));
    assert_eq!(stack.get_value(1).as_int(), Some(20));
}

#[test]
fn snapshot_and_restore() {
    let mut stack = ValueStack::new(10);
    stack.push(Value::Int(1)).unwrap();
    stack.push(Value::Int(2)).unwrap();
    stack.push(Value::Int(3)).unwrap();
    assert_eq!(stack.len(), 3);

    let (vals, tags) = stack.snapshot_raw();
    assert_eq!(vals.len(), 3);
    assert_eq!(tags.len(), 3);

    let mut restored = ValueStack::from_snapshot(vals, tags, 10);
    assert_eq!(restored.len(), 3);
    assert_eq!(restored.pop_int().unwrap(), 3);
    assert_eq!(restored.pop_int().unwrap(), 2);
    assert_eq!(restored.pop_int().unwrap(), 1);
    assert!(restored.is_empty());

    // Can still push after restore (max_size preserved)
    restored.push(Value::Int(99)).unwrap();
    assert_eq!(restored.len(), 1);
}

#[test]
fn snapshot_empty_stack() {
    let stack = ValueStack::new(5);
    let (vals, tags) = stack.snapshot_raw();
    assert!(vals.is_empty());
    assert!(tags.is_empty());

    let restored = ValueStack::from_snapshot(vals, tags, 5);
    assert!(restored.is_empty());
}

// ── T10.6 CompactValue-focused tests ───────────────────────────────

#[test]
fn t10_compact_value_stack_push_pop_roundtrip() {
    let mut stack = ValueStack::new(16);
    let cases = [
        Value::Int(i32::MIN),
        Value::Int(-1),
        Value::Int(0),
        Value::Int(i32::MAX),
        Value::Long(i64::MIN),
        Value::Long(0),
        Value::Long(i64::MAX),
        Value::Float(3.5),
        Value::Float(-0.0),
        Value::Double(std::f64::consts::PI),
        Value::Double(f64::INFINITY),
        Value::Object(None),
        Value::Uninitialized,
        Value::ReturnAddress(12345),
    ];
    for v in cases.iter().copied() {
        stack.push(v).unwrap();
    }
    // Pop in reverse, comparing each value.
    for v in cases.iter().rev() {
        let popped = stack.pop().unwrap();
        match (v, &popped) {
            // Long decodes as Double (untagged) so compare bit patterns
            // instead of enum variants.
            (Value::Long(l), Value::Double(d)) => {
                assert_eq!(*l as u64, d.to_bits());
            }
            _ => assert_eq!(&popped, v, "roundtrip failed for {v}"),
        }
    }
}

#[test]
fn t10_compact_value_stack_memory_footprint() {
    // CompactValue itself is 8 bytes.
    assert_eq!(std::mem::size_of::<CompactValue>(), 8);

    // Each slot in the stack's internal Vec is 8 bytes.
    let stack = ValueStack::new(4);
    assert_eq!(std::mem::size_of_val(&stack.slots[0]), 8);

    // The total underlying buffer is 8 bytes * slot_count.
    let byte_total = stack.slots.len() * std::mem::size_of::<CompactValue>();
    assert_eq!(byte_total, stack.slots.len() * 8);
}

#[test]
fn t10_compact_value_reference_roundtrip() {
    let mut stack = ValueStack::new(4);
    // Use an 8-byte-aligned fake pointer.
    let fake_ptr = 0x1234_5678_ABC0_u64 as *mut u8;
    let obj = unsafe { ObjectRef::from_raw(fake_ptr) };
    stack.push(Value::Object(Some(obj))).unwrap();
    let v = stack.pop().unwrap();
    match v {
        Value::Object(Some(r)) => assert_eq!(r.as_ptr() as u64, 0x1234_5678_ABC0),
        other => panic!("expected Object ref, got {other:?}"),
    }
}

#[test]
fn t10_compact_value_dup_preserves_tag() {
    // Dup-2 semantics for category-2 values: two slots round-trip correctly.
    let mut stack = ValueStack::new(8);
    stack.push(Value::Long(0x0BAD_BEEF_DEAD_CAFE)).unwrap();
    stack.push(Value::Uninitialized).unwrap(); // high half of long
                                               // Simulate dup2: copy the two top slots.
    let high = stack.pop_compact();
    let low = stack.pop_compact();
    stack.push_compact(low);
    stack.push_compact(high);
    stack.push_compact(low);
    stack.push_compact(high);
    // Top of stack: [long_lo, long_hi, long_lo, long_hi]
    let _top_hi = stack.pop_compact(); // high pad
    let top_lo = stack.pop_compact();
    assert_eq!(top_lo.as_long_unchecked(), 0x0BAD_BEEF_DEAD_CAFE);
}

#[test]
fn t10_compact_value_fibonacci_smoke() {
    // Fib(10) computed by push/pop sequences; verifies basic arithmetic
    // round-trips correctly through the CompactValue-backed stack.
    let mut stack = ValueStack::new(32);
    let mut a: i32 = 0;
    let mut b: i32 = 1;
    for _ in 0..10 {
        stack.push(Value::Int(a)).unwrap();
        stack.push(Value::Int(b)).unwrap();
        let vb = stack.pop_int().unwrap();
        let va = stack.pop_int().unwrap();
        let c = va.wrapping_add(vb);
        a = vb;
        b = c;
    }
    assert_eq!(a, 55);
}

#[test]
fn t10_compact_value_push_compact_roundtrip() {
    let mut stack = ValueStack::new(4);
    let cv = CompactValue::int(42);
    stack.push_compact(cv);
    assert_eq!(stack.peek_compact().as_int(), Some(42));
    let popped = stack.pop_compact();
    assert_eq!(popped.as_int(), Some(42));
}

#[test]
fn t10_gc_scan_object_refs_from_compact_stack() {
    let mut stack = ValueStack::new(4);
    let fake = 0x0000_1000_u64 as *mut u8;
    let obj = unsafe { ObjectRef::from_raw(fake) };
    stack.push(Value::Object(Some(obj))).unwrap();
    stack.push(Value::Int(99)).unwrap();
    stack.push(Value::Object(None)).unwrap();

    let mut roots = Vec::new();
    let heap = crate::memory::vm_heap::VmHeap::new(
        crate::memory::vm_heap::GcBackend::Generational,
        1024 * 1024,
    );
    stack.scan_object_refs(&mut roots, &heap);
    assert_eq!(roots.len(), 1);
    assert_eq!(roots[0].as_ptr() as u64, 0x1000);
}

// ── T10.9.D direct-push/pop helpers ────────────────────────────────

#[test]
fn push_int_roundtrip() {
    let mut stack = ValueStack::new(4);
    stack.push_int(42).unwrap();
    assert_eq!(stack.peek_compact().as_int(), Some(42));
    assert_eq!(stack.pop_int().unwrap(), 42);
}

#[test]
fn push_long_roundtrip_negative() {
    let mut stack = ValueStack::new(4);
    stack.push_long(-i64::MAX).unwrap();
    // Untagged — read via unchecked long accessor.
    assert_eq!(stack.peek_compact().as_long_unchecked(), -i64::MAX);
    assert_eq!(stack.pop_long().unwrap(), -i64::MAX);
}

#[test]
fn push_float_roundtrip() {
    let mut stack = ValueStack::new(4);
    stack.push_float(1.5).unwrap();
    assert_eq!(stack.peek_compact().as_float(), Some(1.5f32));
    assert_eq!(stack.pop_float().unwrap().to_bits(), 1.5f32.to_bits());
}

#[test]
fn push_double_roundtrip() {
    let mut stack = ValueStack::new(4);
    stack.push_double(std::f64::consts::PI).unwrap();
    assert_eq!(stack.peek_compact().as_double(), Some(std::f64::consts::PI));
    // Bit equality: a stored value that is read back has been through NO rounding step, so the round trip is bit-exact or the slot corrupted it.
    assert_eq!(
        stack.pop_double().unwrap().to_bits(),
        std::f64::consts::PI.to_bits()
    );
}

#[test]
fn push_null_roundtrip() {
    let mut stack = ValueStack::new(4);
    stack.push_null().unwrap();
    assert!(stack.peek_compact().is_null());
    assert!(stack.pop().unwrap().is_null());
}

#[test]
fn push_int_overflow_returns_err() {
    let mut stack = ValueStack::new(1);
    stack.push_int(1).unwrap();
    assert!(stack.push_int(2).is_err());
}

#[test]
fn t10_gc_update_object_refs_rewrites_pointer() {
    use crate::memory::vm_heap::{GcBackend, VmHeap};
    let heap = VmHeap::new(GcBackend::Generational, 16 * 1024 * 1024);
    let mut stack = ValueStack::new(4);
    let old_ptr = 0x0000_1000_u64;
    let new_ptr = 0x0000_2000_u64;
    let obj = unsafe { ObjectRef::from_raw(old_ptr as *mut u8) };
    stack.push(Value::Object(Some(obj))).unwrap();

    let mut map = cratonvm_types::PointerMap::default();
    map.insert(old_ptr as usize, new_ptr as usize);
    stack.update_object_refs(&map, &heap);

    let popped = stack.pop().unwrap();
    match popped {
        Value::Object(Some(r)) => assert_eq!(r.as_ptr() as u64, new_ptr),
        other => panic!("expected Object, got {other:?}"),
    }
}

/// Per JVM spec a `CompactTag::Long` operand slot IS a primitive long, never
/// a reference. The scan must NOT root such slots — this was the SEGV trigger
/// described in `applogs/letsgo-segv-L1-diagnosis.md`. To exercise the buggy
/// path we need a slot whose `tag()` actually returns `Long`, i.e. a u64 whose
/// high bits collide with the NaN-box marker AND whose 3-bit sub-tag is
/// SUB_LONG_LO (110) or SUB_LONG_HI (111). `CompactValue::long` only produces
/// such a tag for specific bit patterns; build one directly via `from_bits`.
#[test]
fn t10_gc_scan_skips_long_slot_even_if_bits_resemble_heap_pointer() {
    use crate::memory::vm_heap::{GcBackend, VmHeap};

    let heap = VmHeap::new(GcBackend::Generational, 16 * 1024 * 1024);

    // NANBOX_BITS (0xFFFC_0000_0000_0000) | SUB_LONG_LO (6 << 47)
    //   | aligned-pointer-shaped low 47 bits (0x10000).
    // tag() returns CompactTag::Long for this pattern.
    let long_bits: u64 = 0xFFFC_0000_0000_0000 | (6u64 << 47) | 0x1_0000;

    let mut stack = ValueStack::new(4);
    stack.push_compact(CompactValue::from_bits(long_bits));
    assert_eq!(
        stack.get_compact(0).unwrap().tag(),
        CompactTag::Long,
        "test setup must produce a CompactTag::Long slot"
    );

    let mut roots = Vec::new();
    stack.scan_object_refs(&mut roots, &heap);
    assert!(
        roots.is_empty(),
        "primitive Long slots must not be treated as GC roots"
    );
}


/// Untagged raw bits (CompactTag::Double — the ambiguous JVM long/double
/// slot in this encoding) that pass `heap.is_object_address` ARE rooted.
/// This is the safe surviving path after the Long-arm fix.
#[test]
fn t10_gc_scan_roots_untagged_bits_when_heap_validates() {
    use crate::classloading::ClassId;
    use crate::memory::vm_heap::{GcBackend, VmHeap};

    let heap = VmHeap::new(GcBackend::Generational, 16 * 1024 * 1024);
    let obj = heap.alloc_object(ClassId::new(0), 0);
    let addr = obj.as_ptr() as usize;

    let mut stack = ValueStack::new(4);
    // Untagged Double-encoded raw bits (the ambiguous slot kind).
    stack.push_compact(CompactValue::from_bits(addr as u64));

    let mut roots = Vec::new();
    stack.scan_object_refs(&mut roots, &heap);
    assert_eq!(roots.len(), 1);
    assert_eq!(roots[0].as_ptr() as usize, addr);
}

#[test]
fn t10_gc_scan_skips_aligned_integer_long_not_in_heap() {
    use crate::memory::vm_heap::{GcBackend, VmHeap};

    let heap = VmHeap::new(GcBackend::Generational, 16 * 1024 * 1024);
    let mut stack = ValueStack::new(2);
    stack.push_compact(CompactValue::long(8));

    let mut roots = Vec::new();
    stack.scan_object_refs(&mut roots, &heap);
    assert!(roots.is_empty());
}

/// REGRESSION (bc math-ec collision-long corruption): a primitive `long`
/// pushed via `push_long` (⇒ `KIND_LONG`) whose bits collide with the
/// SUB_OBJECT pattern AND whose payload aliases a *live* heap object must
/// be treated as a value, not a reference — neither rooted by
/// `scan_object_refs` nor remapped by `update_object_refs`. Before the fix
/// the `else if … is_heap_addr` rooting branch (scan) and the unconditional
/// `is_object()` remap (update) corrupted such a long under moving GC.
#[test]
fn t10_gc_scan_and_update_skip_collision_long_aliasing_live_object() {
    use crate::classloading::ClassId;
    use crate::memory::vm_heap::{GcBackend, VmHeap};

    let heap = VmHeap::new(GcBackend::Generational, 16 * 1024 * 1024);
    let obj = heap.alloc_object(ClassId::new(0), 0);
    let addr = obj.as_ptr() as u64;

    // Bit-identical to `CompactValue::object(addr)`, but a primitive long.
    let collision_bits = CompactValue::object(addr).raw_bits();

    let mut stack = ValueStack::new(4);
    stack.push_long(collision_bits as i64).unwrap(); // marks KIND_LONG
    assert!(
        stack.get_compact(0).unwrap().is_object(),
        "collision long must carry SUB_OBJECT bits for this test to bite",
    );

    // scan: a KIND_LONG slot is never a root, even aliasing a live object.
    let mut roots = Vec::new();
    stack.scan_object_refs(&mut roots, &heap);
    assert!(
        roots.is_empty(),
        "a KIND_LONG collision long must not be rooted",
    );

    // update: a pointer_map keyed off the collision payload must not rewrite it.
    let mut map = cratonvm_types::PointerMap::default();
    map.insert(addr as usize, (addr as usize) + 64);
    stack.update_object_refs(&map, &heap);
    assert_eq!(
        stack.get_compact(0).unwrap().to_bits(),
        collision_bits,
        "primitive long must be preserved bit-exact across GC update",
    );
}

/// REGRESSION (bc math-ec): the kind-preserving shuffle primitives keep a
/// collision-shaped long's `KIND_LONG` mark across a `dup`, so the GC root
/// scan still skips it. With the old `push_compact_checked` path the
/// duplicate landed `KIND_UNKNOWN` and — sharing the genuine-reference bit
/// pattern — was rooted and relocated, corrupting the long.
#[test]
fn kind_preserving_dup_keeps_collision_long_unrooted() {
    use crate::classloading::ClassId;
    use crate::memory::vm_heap::{GcBackend, VmHeap};

    let heap = VmHeap::new(GcBackend::Generational, 16 * 1024 * 1024);
    let obj = heap.alloc_object(ClassId::new(0), 0);
    let addr = obj.as_ptr() as u64;
    let collision_bits = CompactValue::object(addr).raw_bits();

    let mut stack = ValueStack::new(8);
    stack.push_long(collision_bits as i64).unwrap(); // KIND_LONG

    // Simulate `dup` via the kind-preserving primitives (what the
    // interpreter's Dup/Dup2/Swap handlers now use).
    let (cv, kind) = stack.peek_with_kind().unwrap();
    stack.push_with_kind(cv, kind).unwrap();

    // The duplicate is a category-2 long by its kind (not its bits)…
    assert!(ValueStack::is_cat2_kind(kind, cv));
    // …and neither copy is a GC root.
    let mut roots = Vec::new();
    stack.scan_object_refs(&mut roots, &heap);
    assert!(
        roots.is_empty(),
        "a kind-preserved collision long (and its dup) must not be rooted",
    );
}

/// `update_object_refs` must NOT touch `CompactTag::Long` slots — they are
/// primitive longs, never references. A pointer_map collision against a
/// primitive's bits would otherwise silently corrupt the long value.
/// Uses a hand-rolled bit pattern (see scan-side test) to guarantee a
/// `Long`-tagged slot.
#[test]
fn t10_gc_update_object_refs_preserves_long_slot() {
    use crate::memory::vm_heap::{GcBackend, VmHeap};
    let heap = VmHeap::new(GcBackend::Generational, 16 * 1024 * 1024);
    let mut stack = ValueStack::new(4);
    // NANBOX_BITS | SUB_LONG_HI (7 << 47) | aligned low bits (0x1000).
    let long_bits: u64 = 0xFFFC_0000_0000_0000 | (7u64 << 47) | 0x1000;
    let new_ptr: usize = 0x2000;
    stack.push_compact(CompactValue::from_bits(long_bits));
    assert_eq!(
        stack.get_compact(0).unwrap().tag(),
        CompactTag::Long,
        "test setup must produce a CompactTag::Long slot"
    );

    let mut map = cratonvm_types::PointerMap::default();
    // A malicious pointer_map entry keyed off the Long's low pointer-shaped
    // bits — the fix must NOT remap because Long slots are never roots.
    map.insert(0x1000_usize, new_ptr);
    // Also map the raw long_bits itself (defensive — neither key should fire).
    map.insert(long_bits as usize, new_ptr);
    stack.update_object_refs(&map, &heap);

    // Bits unchanged: primitive long is left strictly alone.
    assert_eq!(stack.get_compact(0).unwrap().to_bits(), long_bits);
}

/// Untagged raw `Double` slot carrying an aligned, mapped pointer IS
/// remapped (these are the only ambiguous slots `scan_object_refs` rooted).
/// B10: the old_ptr MUST be a real heap address — `update_object_refs`
/// now mirrors `scan_object_refs`'s `heap.is_heap_addr` filter to avoid
/// rewriting honest doubles whose bits coincidentally collide with a
/// pointer_map key.
#[test]
fn t10_gc_update_object_refs_rewrites_untagged_pointer_shaped_slot() {
    use crate::classloading::ClassId;
    use crate::memory::vm_heap::{GcBackend, VmHeap};
    let heap = VmHeap::new(GcBackend::Generational, 16 * 1024 * 1024);
    let obj = heap.alloc_object(ClassId::new(0), 0);
    let old_ptr = obj.as_ptr() as usize;
    let new_ptr: usize = old_ptr + 64;
    // Mint-provenance (2026-07-03): raw pointer-shaped slots are only
    // rewritten when the value was registered at a mint chokepoint —
    // model the JNI mint this test's smuggle represents.
    crate::memory::smuggled_longs::record_minted_long(&heap, old_ptr as u64);
    let mut stack = ValueStack::new(4);
    stack.push_compact(CompactValue::from_bits(old_ptr as u64));

    let mut map = cratonvm_types::PointerMap::default();
    map.insert(old_ptr, new_ptr);
    stack.update_object_refs(&map, &heap);

    // Slot now carries the remapped raw bits.
    let cv = stack.get_compact(0).expect("slot present");
    assert_eq!(cv.to_bits(), new_ptr as u64);
    crate::memory::smuggled_longs::forget_heap(&heap);
}

/// B10: an honest f64 whose bit pattern happens to collide with a
/// pointer_map key must NOT be rewritten — the slot is not heap-resident,
/// so it cannot have been a rooted reference. The old behaviour would
/// silently corrupt the f64.
#[test]
fn t10_gc_update_object_refs_leaves_non_heap_double_untouched() {
    use crate::memory::vm_heap::{GcBackend, VmHeap};
    let heap = VmHeap::new(GcBackend::Generational, 16 * 1024 * 1024);
    let mut stack = ValueStack::new(4);
    // Non-heap aligned pointer-shaped bits — the previous behaviour
    // would have rewritten this; with the heap-membership check it
    // stays put.
    let old_bits: u64 = 0x1000;
    let new_ptr: usize = 0x2000;
    stack.push_compact(CompactValue::from_bits(old_bits));

    let mut map = cratonvm_types::PointerMap::default();
    map.insert(old_bits as usize, new_ptr);
    stack.update_object_refs(&map, &heap);

    let cv = stack.get_compact(0).expect("slot present");
    assert_eq!(
        cv.to_bits(),
        old_bits,
        "non-heap Double bits must be preserved"
    );
}

// -----------------------------------------------------------------
// `kinds`-array load-bearing proof (2026-07-26 frame-arena audit)
// -----------------------------------------------------------------
//
// These lock in the invariant that makes the parallel `kinds` array
// impossible to collapse into the NaN-boxed `slots` buffer: an arbitrary
// 64-bit `long` uses every payload bit, so its kind CANNOT live in-slot.
// If someone later removes `kinds`, these fail.

/// Bit patterns that are known to collide with the NaN-box tag space, plus
/// the ordinary extremes. Every one must survive a push/pop round trip
/// bit-exactly.
const LONG_ROUND_TRIP_CASES: &[i64] = &[
    0,
    1,
    -1,
    i64::MAX,
    i64::MIN,
    0x7fff_ffff,
    -0x8000_0000,
    0x1_0000_0000,
    // SUB_OBJECT-colliding pattern (BouncyCastle `LongArray` lxor/lshl).
    0xfffd_1234_5678_9abcu64 as i64,
    0xfffd_0000_0000_0000u64 as i64,
    // SUB_INT-colliding patterns (BC safegcd `Mod.updateDE30`/`updateFG30`
    // accumulators). `0xFFFC_0000_0000_0000` is bit-identical to a real
    // `Value::Int(0)` — only the kind mark tells them apart.
    0xfffc_0000_0000_0000u64 as i64,
    0xfffc_0000_dead_beefu64 as i64,
    0xfffe_0000_0000_0001u64 as i64,
    0xffff_ffff_ffff_ffffu64 as i64,
];

/// `push_long` → `pop_long` must be bit-exact for every case, including the
/// tag-colliding patterns that have no in-slot representation.
#[test]
fn long_round_trips_bit_exactly_through_operand_stack() {
    for &v in LONG_ROUND_TRIP_CASES {
        let mut stack = ValueStack::new(4);
        stack.push_long(v).expect("push_long");
        let got = stack.pop_long().expect("pop_long");
        assert_eq!(
            got as u64, v as u64,
            "push_long/pop_long lost bits for 0x{:016x}",
            v as u64
        );
    }
}

/// Same via the generic `Value` API, which is what the slow interpreter
/// paths use.
#[test]
fn long_round_trips_through_value_api() {
    for &v in LONG_ROUND_TRIP_CASES {
        let mut stack = ValueStack::new(4);
        stack.push(Value::Long(v)).expect("push");
        match stack.pop().expect("pop") {
            Value::Long(got) => assert_eq!(
                got as u64, v as u64,
                "Value::Long round trip lost bits for 0x{:016x}",
                v as u64
            ),
            other => panic!("expected Long for 0x{:016x}, got {other}", v as u64),
        }
    }
}

/// The `push_compact_long` fast path (`lload` forwarding a local without
/// the `Value` round trip) must also mark the slot, or a collision long
/// would be sign-extended as an int on the way out.
#[test]
fn compact_long_push_marks_kind_and_round_trips() {
    for &v in LONG_ROUND_TRIP_CASES {
        let mut stack = ValueStack::new(4);
        stack.push_compact_long(CompactValue::long(v));
        let (_, kind) = stack.peek_with_kind().expect("peek_with_kind");
        assert_eq!(kind, KIND_LONG, "push_compact_long must mark KIND_LONG");
        assert_eq!(
            stack.pop_long().expect("pop_long") as u64,
            v as u64,
            "push_compact_long/pop_long lost bits for 0x{:016x}",
            v as u64
        );
    }
}

/// The exact ambiguity that proves `kinds` is not redundant: the marked
/// long `0xFFFC_0000_0000_0000` and a real `Value::Int(0)` occupy
/// bit-identical slots and are told apart ONLY by the kind mark.
#[test]
fn collision_long_and_int_zero_are_bit_identical_and_need_the_kind_mark() {
    let collision = 0xfffc_0000_0000_0000u64 as i64;

    let mut as_long = ValueStack::new(4);
    as_long.push_long(collision).expect("push_long");
    let long_bits = as_long.get_compact(0).expect("slot").to_bits();

    let mut as_int = ValueStack::new(4);
    as_int.push(Value::Int(0)).expect("push int");
    let int_bits = as_int.get_compact(0).expect("slot").to_bits();

    assert_eq!(
        long_bits, int_bits,
        "the premise of this test changed: the collision long and Int(0) \
         are no longer bit-identical"
    );

    // Same bits, different kinds, different decoded values.
    assert_eq!(
        as_long.peek_with_kind().expect("peek").1,
        KIND_LONG,
        "long slot must carry KIND_LONG"
    );
    assert_eq!(
        as_int.peek_with_kind().expect("peek").1,
        KIND_UNKNOWN,
        "int slot must stay KIND_UNKNOWN"
    );
    assert_eq!(as_long.pop_long().expect("pop") as u64, collision as u64);
    assert_eq!(as_int.pop_int().expect("pop"), 0);
}

/// Recycled buffers must never leak a stale kind mark into a fresh frame:
/// an over-marked slot would hide a genuine object reference from the GC
/// root scan. `from_pooled` clears + resizes the tag half for exactly this
/// reason; this pins the behaviour now that the non-pooled `Frame`
/// constructors also take buffers from a pool.
#[test]
fn from_pooled_clears_stale_kind_marks() {
    let mut dirty = ValueStack::new(8);
    dirty.push_long(-1).expect("push_long");
    dirty.push_double(1.5).expect("push_double");
    let (vals, tags) = dirty.into_inner();

    let mut fresh = ValueStack::from_pooled(vals, tags, 8);
    // Every slot starts UNKNOWN.
    for i in 0..8 {
        assert_eq!(
            fresh.kinds[i], KIND_UNKNOWN,
            "recycled slot {i} kept a stale kind mark"
        );
    }
    // And an object-shaped push is still scannable as a reference.
    fresh.push(Value::Int(7)).expect("push");
    assert_eq!(fresh.pop_int().expect("pop"), 7);
}

/// Every double whose raw bits collide with the NaN-box tag space survives
/// the operand stack bit-exact, by all four doors.
///
/// These are the patterns `probes/F2dCensus.java` measured as lost — an
/// `f2d` of a negative float NaN with mantissa bits 22 and 21 set — and the
/// reason they survive is the `KIND_DOUBLE` mark, not the bits.
#[test]
fn tag_colliding_double_payloads_survive_the_operand_stack() {
    let patterns: [u64; 5] = [
        0xFFFC_0000_0000_0000,
        0xFFFC_541A_8000_0000, // census sample: in=ffe2a0d4
        0xFFFE_5E0E_8000_0000, // census sample: in=ffb2f074
        0xFFFF_AF30_A000_0000, // census sample: in=fffd7985
        0xFFFF_FFFF_FFFF_FFFF,
    ];
    for bits in patterns {
        let d = f64::from_bits(bits);

        let mut s = ValueStack::new(8);
        s.push_double(d).expect("push_double");
        assert_eq!(s.peek_compact().raw_bits(), bits, "slot bits {bits:#018x}");
        match s.peek() {
            Value::Double(x) => assert_eq!(x.to_bits(), bits, "peek {bits:#018x}"),
            other => panic!("peek of a KIND_DOUBLE slot gave {other:?}"),
        }
        assert_eq!(
            s.pop_double().expect("pop_double").to_bits(),
            bits,
            "pop_double {bits:#018x}",
        );

        let mut s = ValueStack::new(8);
        s.push_double_unchecked(d);
        assert_eq!(s.pop_double_unchecked().to_bits(), bits);

        let mut s = ValueStack::new(8);
        s.push(Value::Double(d)).expect("push");
        match s.pop().expect("pop") {
            Value::Double(x) => assert_eq!(x.to_bits(), bits, "push/pop {bits:#018x}"),
            other => panic!("pop of a Value::Double push gave {other:?}"),
        }

        // dup must copy bits AND kind, or the copy would be re-decoded by
        // its sub-tag and (worse) offered to the root scan.
        let mut s = ValueStack::new(8);
        s.push_double(d).expect("push_double");
        let (cv, k) = s.peek_with_kind().expect("peek_with_kind");
        s.push_with_kind(cv, k).expect("push_with_kind");
        assert_eq!(s.pop_double().expect("pop dup").to_bits(), bits);
        assert_eq!(s.pop_double().expect("pop orig").to_bits(), bits);
    }
}

/// The GC half: a tag-colliding double on the operand stack is never a
/// root, even when its 47-bit payload is a live object's address.
///
/// This is the hazard that made storing doubles verbatim look unsafe. It is
/// closed by the same `KIND_DOUBLE` gate that already closed it for the
/// primitive `long` whose bits collide the same way (`CompactValue::long`
/// has stored verbatim since the BC SM2 fix of 2026-05-28).
#[test]
fn scan_object_refs_skips_a_tag_colliding_double_aliasing_a_live_object() {
    use crate::memory::VmHeap;
    use cratonvm_gc::GcBackend;

    let heap = VmHeap::new(GcBackend::Generational, 16 * 1024 * 1024);
    let obj = heap.alloc_object(cratonvm_types::ClassId::new(0), 0);
    let addr = obj.as_ptr() as u64;
    // Bit-identical to `CompactValue::object(addr)` — but a double.
    let bits = CompactValue::object(addr).raw_bits();
    assert!(CompactValue::double_raw(f64::from_bits(bits)).is_object());

    let mut s = ValueStack::new(8);
    s.push_double(f64::from_bits(bits)).expect("push_double");
    let mut roots = Vec::new();
    s.scan_object_refs(&mut roots, &heap);
    assert!(
        roots.is_empty(),
        "a KIND_DOUBLE slot must never be rooted, got {roots:?}",
    );

    // Control: the same address pushed as a genuine reference IS a root.
    let mut s = ValueStack::new(8);
    s.push(Value::Object(Some(obj))).expect("push object");
    let mut roots = Vec::new();
    s.scan_object_refs(&mut roots, &heap);
    assert_eq!(roots.len(), 1, "the control push must root");
}

/// Wave 123, lane L9 (review of the wave-122 precise stack scan): a
/// `long`-shaped slot the push left `KIND_UNKNOWN` (`push_compact`: `dup`,
/// a cached field read or return) still counts as wide, so the precise
/// scan keeps the smuggled-handle branch's decision for it; an int, a
/// float and a reference do not.
#[test]
fn w123_l9_slot_is_wide_reads_the_tag_as_well_as_the_mark() {
    let mut s = ValueStack::new(8);
    s.push_compact(CompactValue::long(0x1000_0040));
    s.push_with_kind(CompactValue::long(7), KIND_LONG).expect("fits");
    s.push_int(3).expect("fits");
    s.push(Value::Float(1.5)).expect("fits");
    s.push(Value::Object(None)).expect("fits");
    assert!(s.slot_is_wide(0), "an unmarked long-shaped slot");
    assert!(s.slot_is_wide(1), "a marked long");
    assert!(!s.slot_is_wide(2), "an int");
    assert!(!s.slot_is_wide(3), "a float");
    assert!(!s.slot_is_wide(4), "a null reference");
    assert!(!s.slot_is_wide(5), "past the depth");
}
