use super::{GetClassMirrorWords, GETCLASS_MIRROR_WORDS_CAPACITY};
use crate::classloading::ClassId;
use crate::types::ObjectRef;

fn fake(addr: usize) -> ObjectRef {
    // SAFETY: used only as an address; never dereferenced.
    unsafe { ObjectRef::from_raw(addr as *mut u8) }
}

/// nio20 L3: before a compile allocates the table nothing is stored, so
/// a VM that never JITs a `getClass()` pays no table; once allocated,
/// word 0 is the capacity compiled code bounds-checks against.
#[test]
fn an_unallocated_table_stores_nothing_and_an_allocated_one_states_its_capacity() {
    let words = GetClassMirrorWords::new();
    let cid = ClassId::new(7);
    words.publish(cid, fake(0xD100));
    assert!(!words.is_allocated());
    assert_eq!(words.get(cid), 0);

    let base = words.base_addr();
    assert_ne!(base, 0);
    assert_eq!(words.base_addr(), base, "the address is stable");
    // SAFETY: `base` is the live allocation `words` owns; word 0 is in it.
    let capacity = unsafe { *(base as *const usize) };
    assert_eq!(capacity, GETCLASS_MIRROR_WORDS_CAPACITY);

    words.publish(cid, fake(0xD100));
    assert_eq!(words.get(cid), 0xD100);
    // SAFETY: as above; word `1 + 7` is inside the allocation.
    let raw = unsafe { *((base as *const usize).add(1 + 7)) };
    assert_eq!(raw, 0xD100, "compiled code reads word 1 + class_id");
}

/// JIT round 20 wave 19, lane zAssign: a mirror hint lands in the band
/// slot compiled code reads (`mirror_hint_slot` of the address), and
/// forgetting its class clears it -- and a remembered refusal -- but no
/// other class's hint.
#[test]
fn a_mirror_hint_sits_in_its_slot_and_dies_with_its_class() {
    use cratonvm_jit::ir_lower as jl;
    let words = GetClassMirrorWords::new();
    words.publish_mirror_hint(0xE100, 1);
    assert_eq!(words.mirror_hint(0xE100), 0, "no table, no hint");
    let base = words.base_addr();
    let (a, b, c) = (0xE100usize, 0xE2F8usize, 0xE4F0usize);
    let (ca, cb, cc) = (ClassId::new(9), ClassId::new(10), ClassId::new(11));
    let hint = |cid: ClassId, ok: bool| {
        usize::try_from(jl::mirror_hint_word(cid.as_u32(), ok)).expect("64-bit")
    };
    words.publish_mirror_hint(a, hint(ca, true));
    words.publish_mirror_hint(b, hint(cb, false));
    let refused =
        usize::try_from(jl::MIRROR_HINT_DECLINED | u64::from(cc.as_u32())).expect("64-bit");
    words.publish_mirror_hint(c, refused);
    assert_eq!(words.mirror_hint(a), hint(ca, true));
    // SAFETY: `base` is the live allocation `words` owns; the hint band is
    // inside its `GETCLASS_WORDS_TOTAL` words.
    let raw = unsafe {
        *((base as *const usize)
            .add(jl::GETCLASS_WORDS_MIRROR_HINT_BASE + jl::mirror_hint_slot(a)))
    };
    assert_eq!(raw, hint(ca, true), "compiled code reads HINT_BASE + slot");
    words.forget(ca);
    words.forget(cc);
    assert_eq!(words.mirror_hint(a), 0);
    assert_eq!(words.mirror_hint(c), 0);
    assert_eq!(
        words.mirror_hint(b),
        hint(cb, false),
        "another class's hint stays"
    );
}

/// An id past the class band and outside the lambda band has no word:
/// publishing it is a no-op and compiled code's bounds checks send it to
/// the helper. nio20 wave 5 L2: a lambda proxy id inside the band HAS a
/// word, at `1 + CAPACITY + (id - LAMBDA_PROXY_ID_BASE)`, which is where
/// `getclass_table_lookup_bytes` reads it.
#[test]
fn an_id_past_the_capacity_has_no_word_but_a_lambda_id_has_its_band_word() {
    let words = GetClassMirrorWords::new();
    let base = words.base_addr();
    // Cast: the capacity fits a class id.
    let past = ClassId::new(GETCLASS_MIRROR_WORDS_CAPACITY as u32);
    words.publish(past, fake(0xD200));
    assert_eq!(words.get(past), 0);
    let between = ClassId::new(super::LAMBDA_PROXY_ID_BASE - 1);
    words.publish(between, fake(0xD200));
    assert_eq!(words.get(between), 0);
    let lcap = cratonvm_jit::ir_lower::GETCLASS_WORDS_LAMBDA_CAPACITY as u32; // Cast: 1 << 16
    let beyond = ClassId::new(super::LAMBDA_PROXY_ID_BASE + lcap);
    words.publish(beyond, fake(0xD200));
    assert_eq!(words.get(beyond), 0);

    let lambda = ClassId::new(super::LAMBDA_PROXY_ID_BASE + 3);
    words.publish(lambda, fake(0xD208));
    assert_eq!(words.get(lambda), 0xD208);
    // SAFETY: `base` is the live allocation `words` owns, and the index is
    // inside its `GETCLASS_WORDS_TOTAL` words.
    let raw = unsafe { *((base as *const usize).add(1 + GETCLASS_MIRROR_WORDS_CAPACITY + 3)) };
    assert_eq!(raw, 0xD208, "compiled code reads the lambda band");
    words.rekey(lambda, 0xD208, 0xD210);
    assert_eq!(words.get(lambda), 0xD210);
    words.forget(lambda);
    assert_eq!(words.get(lambda), 0);
}

/// A re-key moves only a word that names the old address; a word never
/// published stays `0` (the helper keeps answering it), and `forget`
/// clears.
#[test]
fn rekey_follows_only_the_published_address_and_forget_clears() {
    let words = GetClassMirrorWords::new();
    let _ = words.base_addr();
    let (published, unpublished) = (ClassId::new(11), ClassId::new(12));
    words.publish(published, fake(0xD300));
    words.rekey(published, 0xD300, 0xD308);
    assert_eq!(words.get(published), 0xD308);
    words.rekey(unpublished, 0xD310, 0xD318);
    assert_eq!(words.get(unpublished), 0, "never published: stays 0");
    // A stale `old` (not the word's value) leaves it alone.
    words.rekey(published, 0xD300, 0xD3F0);
    assert_eq!(words.get(published), 0xD308);
    words.forget(published);
    assert_eq!(words.get(published), 0);
}

/// nio20 w4 L2: the debug census reports at powers of two only, with both
/// counts in `(served, declined)` order whichever side ticked.
#[test]
fn helper_call_census_reports_at_powers_of_two() {
    let words = GetClassMirrorWords::new();
    assert_eq!(words.note_helper_call(true), Some((1, 0)));
    assert_eq!(words.note_helper_call(true), Some((2, 0)));
    assert_eq!(words.note_helper_call(true), None);
    assert_eq!(words.note_helper_call(false), Some((3, 1)));
    assert_eq!(words.note_helper_call(true), Some((4, 1)));
    assert_eq!(words.note_helper_call(false), Some((4, 2)));
    assert_eq!(words.note_helper_call(false), None);
}

/// nio21 wave 1 L5: a primitive-array word sits where compiled code reads
/// it (`GETCLASS_WORDS_PRIM_ARRAY_BASE + slot`), and the collector's
/// re-key / forget of the ARRAY CLASS ID reach it; the class band word of
/// that id is never written by the alias.
#[test]
fn prim_array_words_follow_their_array_class_id() {
    if !cratonvm_jit::ir_lower::getclass_prim_array_words_enabled() {
        return;
    }
    let words = GetClassMirrorWords::new();
    let arr = ClassId::new(21);
    // Before the table exists: nothing.
    words.publish_prim_array(6, arr, fake(0xD400));
    assert_eq!(words.get_prim_array(6), 0);
    let base = words.base_addr();
    words.publish_prim_array(6, arr, fake(0xD400));
    assert_eq!(words.get_prim_array(6), 0xD400);
    // SAFETY: `base` is the live allocation `words` owns, and the index is
    // inside its `GETCLASS_WORDS_TOTAL` words.
    let raw = unsafe {
        *((base as *const usize)
            .add(cratonvm_jit::ir_lower::GETCLASS_WORDS_PRIM_ARRAY_BASE + 6))
    };
    assert_eq!(raw, 0xD400, "compiled code reads the primitive-array band");
    assert_eq!(words.get(arr), 0, "the class band word is not the alias");
    words.rekey(arr, 0xD400, 0xD408);
    assert_eq!(words.get_prim_array(6), 0xD408);
    words.rekey(ClassId::new(22), 0xD408, 0xD4F0);
    assert_eq!(
        words.get_prim_array(6),
        0xD408,
        "another id leaves it alone"
    );
    words.forget(arr);
    assert_eq!(words.get_prim_array(6), 0);
    // A slot past the band is a no-op.
    words.publish_prim_array(8, arr, fake(0xD500));
    assert_eq!(words.get_prim_array(8), 0);
}

/// nio21 wave 2 L5: a reference-array word sits where compiled code reads
/// it (`GETCLASS_WORDS_REF_ARRAY_BASE + component_id`); the collector's
/// re-key / forget of the ARRAY CLASS ID reach it through the owner map,
/// forgetting the COMPONENT clears it too, and ids past the band are
/// no-ops.
#[test]
fn ref_array_words_follow_their_array_class_id() {
    if !cratonvm_jit::ir_lower::getclass_ref_array_words_enabled() {
        return;
    }
    let words = GetClassMirrorWords::new();
    let (component, arr) = (ClassId::new(31), ClassId::new(32));
    words.publish_ref_array(component, arr, fake(0xD600));
    assert_eq!(words.get_ref_array(component), 0, "no table yet");
    let base = words.base_addr();
    words.publish_ref_array(component, arr, fake(0xD600));
    assert_eq!(words.get_ref_array(component), 0xD600);
    // SAFETY: `base` is the live allocation `words` owns, and the index is
    // inside its `GETCLASS_WORDS_TOTAL` words.
    let raw = unsafe {
        *((base as *const usize)
            .add(cratonvm_jit::ir_lower::GETCLASS_WORDS_REF_ARRAY_BASE + 31))
    };
    assert_eq!(raw, 0xD600, "compiled code reads the reference-array band");
    assert_eq!(
        words.get(component),
        0,
        "the class band word is not the alias"
    );
    assert_eq!(words.get(arr), 0, "nor the array class's own word");
    words.rekey(arr, 0xD600, 0xD608);
    assert_eq!(words.get_ref_array(component), 0xD608);
    words.rekey(component, 0xD608, 0xD6F0);
    assert_eq!(
        words.get_ref_array(component),
        0xD608,
        "the component id is not the re-key"
    );
    words.forget(arr);
    assert_eq!(words.get_ref_array(component), 0);
    // Re-published, then the component forgotten: cleared as well.
    words.publish_ref_array(component, arr, fake(0xD610));
    assert_eq!(words.get_ref_array(component), 0xD610);
    words.forget(component);
    assert_eq!(words.get_ref_array(component), 0);
    // Ids past the band are no-ops.
    let cap = cratonvm_jit::ir_lower::GETCLASS_WORDS_REF_ARRAY_CAPACITY as u32; // Cast: 1 << 16
    words.publish_ref_array(ClassId::new(cap), arr, fake(0xD700));
    assert_eq!(words.get_ref_array(ClassId::new(cap)), 0);
    words.publish_ref_array(component, ClassId::new(cap), fake(0xD700));
    assert_eq!(words.get_ref_array(component), 0);
}
