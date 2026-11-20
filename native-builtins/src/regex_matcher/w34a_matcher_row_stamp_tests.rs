use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

fn obj(addr: usize) -> ObjectRef {
    // SAFETY: a key only; the registry and the mock hash the address and
    // never dereference it (8-byte aligned, lesson o).
    unsafe { ObjectRef::from_raw(addr as *mut u8) }
}

fn stamp(
    text_addr: usize,
    pattern_addr: usize,
    epoch: u64,
    mod_count: i32,
) -> MatcherRealRowStamp {
    MatcherRealRowStamp {
        text_addr,
        pattern_addr,
        epoch,
        mod_count,
    }
}

fn decoded(
    text: &str,
    text_identity: i32,
    pattern_identity: i32,
) -> std::sync::Arc<MatcherRealCache> {
    let (byte_to_utf16, utf16_to_byte) = matcher_realjdk_build_offset_tables(text);
    std::sync::Arc::new(MatcherRealCache {
        ascii_run: None,
        text_identity,
        pattern_identity,
        capture_count: 1,
        utf8: std::sync::Arc::from(text.to_string().into_boxed_str()),
        byte_to_utf16: std::sync::Arc::from(byte_to_utf16.into_boxed_slice()),
        utf16_to_byte: std::sync::Arc::from(utf16_to_byte.into_boxed_slice()),
        re: compile_java_regex("a", 0).expect("trivial pattern"),
    })
}

struct ForgetVm(usize);
impl Drop for ForgetVm {
    fn drop(&mut self) {
        crate::forget_vm_lock_keys(self.0);
    }
}

/// The validator alone: within an epoch the addresses decide; across a
/// collection the Matcher's `modCount` does. Never a hash.
#[test]
fn w34a_c_a_stamp_names_objects_within_an_epoch_and_mutations_across() {
    let row = stamp(0x1_34A0_C000, 0x1_34A0_D000, 4, 7);
    let same_hash_input = 0x2_34A0_C000usize;
    assert_eq!(
        same_hash_input as i32, row.text_addr as i32,
        "premise: one mock hash"
    );
    assert!(
        !row.still_names(&stamp(same_hash_input, 0x1_34A0_D000, 4, 8)),
        "reset(input) to a same-hash String in the same epoch"
    );
    assert!(
        !row.still_names(&stamp(same_hash_input, 0x1_34A0_D000, 4, 7)),
        "within an epoch the address decides, whatever modCount says"
    );
    assert!(
        !row.still_names(&stamp(0x1_34A0_C000, 0x2_34A0_D000, 4, 8)),
        "usePattern to a same-hash Pattern"
    );
    assert!(
        row.still_names(&stamp(0x1_34A0_C000, 0x1_34A0_D000, 4, 9)),
        "the same objects after a plain reset(): still them"
    );
    assert!(
        !row.still_names(&stamp(0x3_34A0_C000, 0x3_34A0_D000, 5, 8)),
        "a collection later, after a reset: re-decode"
    );
    assert!(
        row.still_names(&stamp(0x3_34A0_C000, 0x3_34A0_D000, 5, 7)),
        "a collection later, untouched Matcher: keep the decoded text"
    );
}

/// The table lookup: a `reset` to a same-hash input misses; a collection
/// with no `reset` hits and re-stamps the row; a different hash misses.
#[test]
fn w34a_c_the_table_row_answers_only_its_objects() {
    const VM: usize = 0x34A0_C001;
    let _forget = ForgetVm(VM);
    let ctx = crate::test_utils::mock_ctx();
    ctx.set_vm_identity(VM);
    let key = crate::gc_stable_weak_lock_key(&ctx, obj(0x1_34A0_E000)).unwrap();
    let built = stamp(0x1_34A0_C000, 0x1_34A0_D000, 4, 7);
    matcher_realjdk_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(key, (built, decoded("old input", 11, 22)));

    let hit = matcher_realjdk_row_lookup(key, 11, 22, built).expect("its own objects");
    assert_eq!(&*hit.utf8, "old input");
    assert!(
        matcher_realjdk_row_lookup(key, 11, 22, stamp(0x2_34A0_C000, 0x1_34A0_D000, 4, 8))
            .is_none(),
        "a reset to a colliding input is not served the old text"
    );
    let moved = stamp(0x3_34A0_C000, 0x3_34A0_D000, 5, 7);
    assert!(matcher_realjdk_row_lookup(key, 11, 22, moved).is_some());
    let restamped = matcher_realjdk_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&key)
        .map(|(s, _)| *s);
    assert_eq!(restamped, Some(moved), "a hit re-stamps the row");
    assert!(
        matcher_realjdk_row_lookup(key, 12, 22, moved).is_none(),
        "another text hash"
    );
}
