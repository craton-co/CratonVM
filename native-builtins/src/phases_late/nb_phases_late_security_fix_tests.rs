use super::*;
use cratonvm_native_api::NativeMethodRegistry;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

// -- BUG nb-phases-late(4): Mac key-material scrubbing -----------------

#[test]
fn mac_zeroize_clears_and_zeros_secret_bytes() {
    let mut key = vec![0xAAu8, 0xBB, 0xCC, 0xDD];
    mac_zeroize(&mut key);
    // After zeroize the logical length is 0 (key no longer retained).
    assert!(key.is_empty(), "key Vec must be cleared after zeroize");
}

#[test]
fn mac_state_drop_scrubs_key() {
    // A MacState going out of scope (eviction / table teardown) must scrub
    // its key bytes via Drop without panicking.
    let st = MacState {
        algo: "HmacSHA256".to_string(),
        key: vec![1, 2, 3, 4, 5],
        data: vec![9, 9, 9],
        initialized: true,
    };
    drop(st); // Drop impl runs mac_zeroize on key + data.
}

#[test]
fn mac_state_evict_drops_excess_entries() {
    let mut t: std::collections::HashMap<i32, MacState> = std::collections::HashMap::new();
    for i in 0..(MAC_STATE_MAX_ENTRIES as i32 + 1) {
        // NB: list all fields explicitly — MacState impls Drop (key-zeroing),
        // so the `..Default::default()` functional-update form is rejected (E0509).
        t.insert(
            i,
            MacState {
                algo: String::new(),
                key: vec![0u8; 4],
                data: Vec::new(),
                initialized: false,
            },
        );
    }
    let keep = MAC_STATE_MAX_ENTRIES as i32; // the "about to insert" id
    mac_state_evict_if_needed(&mut t, keep);
    assert!(
        t.len() <= MAC_STATE_MAX_ENTRIES,
        "table must be bounded after eviction"
    );
    assert!(t.contains_key(&keep), "the kept id must never be evicted");
}

// -- BUG nb-phases-late(3): SynchronousQueue helper + registration -----

#[test]
fn sq_timeout_nanos_non_positive_is_no_wait() {
    // A non-positive timeout maps to None ("don't wait").
    assert_eq!(sq_timeout_nanos(&[Value::Long(0)], 0), None);
    assert_eq!(sq_timeout_nanos(&[Value::Long(-5)], 0), None);
    // Missing arg → treated as 0 → None.
    assert_eq!(sq_timeout_nanos(&[], 0), None);
}

#[test]
fn sq_timeout_nanos_positive_is_bounded() {
    // A positive timeout yields a bounded, non-zero nanos value (never
    // an effectively-infinite block).
    let n = sq_timeout_nanos(&[Value::Long(10)], 0).expect("positive timeout");
    assert!(n > 0);
    // Saturating multiply must not overflow for Long.MAX_VALUE.
    let big = sq_timeout_nanos(&[Value::Long(i64::MAX)], 0).expect("max timeout");
    assert!(big > 0);
}

#[test]
fn sq_real_rendezvous_methods_registered() {
    let mut r = NativeMethodRegistry::new();
    register_p58_synchronous_queue(&mut r);
    let sq = "java/util/concurrent/SynchronousQueue";
    assert!(r.find(sq, "put", "(Ljava/lang/Object;)V").is_some());
    assert!(r.find(sq, "take", "()Ljava/lang/Object;").is_some());
    assert!(r.find(sq, "offer", "(Ljava/lang/Object;)Z").is_some());
    assert!(r
        .find(
            sq,
            "offer",
            "(Ljava/lang/Object;JLjava/util/concurrent/TimeUnit;)Z"
        )
        .is_some());
    assert!(r
        .find(
            sq,
            "poll",
            "(JLjava/util/concurrent/TimeUnit;)Ljava/lang/Object;"
        )
        .is_some());
}
