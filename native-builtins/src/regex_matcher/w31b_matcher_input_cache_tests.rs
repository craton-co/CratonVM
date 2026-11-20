use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

const VM: usize = 0x31B0_0A01;

struct Forget;
impl Drop for Forget {
    fn drop(&mut self) {
        matcher_input_cache()
            .lock()
            .retain(|bucket, _| bucket.0 != VM);
    }
}

fn obj(addr: usize) -> ObjectRef {
    // SAFETY: a key only; the mock hashes the address and never
    // dereferences it (8-byte aligned, lesson o).
    unsafe { ObjectRef::from_raw(addr as *mut u8) }
}

fn spans(key: MatcherCacheKey) -> Option<Vec<Option<(usize, usize)>>> {
    matcher_cache_lookup_captures(key, 0).map(|(groups, _)| groups)
}

#[test]
fn a_colliding_matcher_never_reads_or_overwrites_the_others_captures() {
    let _forget = Forget;
    let ctx = crate::test_utils::mock_ctx();
    ctx.set_vm_identity(VM);
    let a = matcher_cache_key(&ctx, obj(0x1_31B0_0D08));
    let b = matcher_cache_key(&ctx, obj(0x2_31B0_0D08));
    assert_eq!(
        a.bucket, b.bucket,
        "premise: the two Matchers share a bucket"
    );
    assert_ne!(a.addr, b.addr);

    // `a.find()` filed its decode and its spans.
    matcher_input_cache().lock().insert(
        a.bucket,
        MatcherInputCacheEntry {
            owner_addr: a.addr,
            input_identity: 7,
            input_addr: 0x31B0_1008,
            decoded: std::sync::Arc::from("shared input"),
            captures: None,
        },
    );
    matcher_cache_store_captures(a, 0, vec![Some((0, 6))], std::collections::HashMap::new());
    assert_eq!(spans(a), Some(vec![Some((0, 6))]));

    // `b` shares the hash but is another Matcher: no spans, and its own
    // `find()` cannot overwrite `a`'s.
    assert_eq!(spans(b), None, "b read a's capture spans");
    matcher_cache_store_captures(b, 0, vec![Some((7, 12))], std::collections::HashMap::new());
    assert_eq!(
        spans(a),
        Some(vec![Some((0, 6))]),
        "b's find() overwrote a's spans"
    );
}

/// gc-common w34-a: replacing a Matcher's input drops its row (so a new
/// input that reused the old one's address and hash cannot hit it), and
/// only its own: a colliding Matcher's row in the bucket is left alone.
#[test]
fn w34a_e_replacing_the_input_forgets_only_this_matchers_row() {
    // Its own VM identity and guard (lesson pp): `Forget` drops every
    // row of `VM`, and the test above may run concurrently.
    const VM_E: usize = 0x34A0_E001;
    struct ForgetE;
    impl Drop for ForgetE {
        fn drop(&mut self) {
            matcher_input_cache()
                .lock()
                .retain(|bucket, _| bucket.0 != VM_E);
        }
    }
    let _forget = ForgetE;
    let ctx = crate::test_utils::mock_ctx();
    ctx.set_vm_identity(VM_E);
    let a = matcher_cache_key(&ctx, obj(0x1_31B0_0E08));
    let b = matcher_cache_key(&ctx, obj(0x2_31B0_0E08));
    assert_eq!(a.bucket, b.bucket, "premise: one bucket");
    let entry = |owner_addr| MatcherInputCacheEntry {
        owner_addr,
        input_identity: 9,
        input_addr: 0x31B0_1108,
        decoded: std::sync::Arc::from("old input"),
        captures: None,
    };

    matcher_input_cache().lock().insert(a.bucket, entry(a.addr));
    matcher_input_cache_forget(&ctx, obj(b.addr));
    assert!(
        matcher_input_cache().lock().get(&a.bucket).is_some(),
        "a colliding Matcher's reset leaves this row"
    );
    matcher_input_cache_forget(&ctx, obj(a.addr));
    assert!(
        matcher_input_cache().lock().get(&a.bucket).is_none(),
        "this Matcher's reset drops its row"
    );
}
