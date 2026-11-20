use super::*;

fn lambda(proxy: u32) -> ResolvedCallSite {
    ResolvedCallSite::Lambda(LambdaCallSite {
        functional_interface: Arc::from("java/util/function/Supplier"),
        sam_method_name: Arc::from("get"),
        sam_descriptor: Arc::from("()Ljava/lang/Object;"),
        impl_handle: MethodHandle {
            kind: MethodHandleKind::InvokeStatic,
            class_name: Arc::from("r21w11/Host"),
            member_name: Arc::from("lambda$0"),
            descriptor: Arc::from("()Ljava/lang/Object;"),
        },
        instantiated_descriptor: Arc::from("()Ljava/lang/Object;"),
        capture_types: Vec::new(),
        proxy_class_id: ClassId::new(proxy),
        serializable_flag: false,
        functional_interface_id: None,
    })
}

fn proxy_of(cache: &ResolutionCache, class: ClassId, cp: u16) -> Option<ClassId> {
    match cache.get_call_site(class, cp) {
        Some(ResolvedCallSite::Lambda(site)) => Some(site.proxy_class_id),
        _ => None,
    }
}

/// Push `RESOLUTION_CACHE_CAP + 1` further call sites through the cache.
fn flood(cache: &mut ResolutionCache) {
    for i in 0..=RESOLUTION_CACHE_CAP {
        // Cast: test-only; `i` is at most 2^16, so both halves fit.
        let (class, cp) = (ClassId::new(1_000 + (i >> 16) as u32), (i & 0xFFFF) as u16);
        cache.put_call_site(class, cp, lambda(500_000 + i as u32));
    }
}

#[test]
fn r21w11_a_linked_lambda_row_survives_the_fifo_cap() {
    let mut cache = ResolutionCache::with_linked_permanent(true);
    assert!(cache.linked_constants_permanent());
    let host = ClassId::new(77);
    cache.put_call_site(host, 5, lambda(400_001));
    flood(&mut cache);
    assert_eq!(
        proxy_of(&cache, host, 5),
        Some(ClassId::new(400_001)),
        "the first-linked row is still the site's linkage"
    );
    assert_eq!(cache.call_site_count(), RESOLUTION_CACHE_CAP + 2);
    assert!(cache.call_sites_order.is_empty(), "no FIFO tracking");
}

/// Positive control: the kill switch restores the FIFO eviction the page
/// describes (the oldest row goes first).
#[test]
fn r21w11_kill_switch_restores_the_fifo_eviction() {
    let mut cache = ResolutionCache::with_linked_permanent(false);
    assert!(!cache.linked_constants_permanent());
    let host = ClassId::new(77);
    cache.put_call_site(host, 5, lambda(400_001));
    flood(&mut cache);
    assert_eq!(proxy_of(&cache, host, 5), None, "evicted, as before");
    assert!(cache.call_site_count() <= RESOLUTION_CACHE_CAP);
}

/// Permanence is for the class's lifetime: unloading and redefinition
/// still drop the row, and the layout-only sweep still keeps it.
#[test]
fn r21w11_permanent_rows_go_with_their_class() {
    let mut cache = ResolutionCache::with_linked_permanent(true);
    let (dead, live, redefined) = (ClassId::new(81), ClassId::new(82), ClassId::new(83));
    cache.put_call_site(dead, 1, lambda(400_010));
    cache.put_call_site(live, 1, lambda(400_011));
    cache.put_call_site(redefined, 1, lambda(400_012));
    cache.invalidate_class_field_layout(live);
    assert_eq!(proxy_of(&cache, live, 1), Some(ClassId::new(400_011)));
    cache.forget_classes(|c| c == dead);
    assert_eq!(proxy_of(&cache, dead, 1), None);
    cache.invalidate_class(redefined);
    assert_eq!(proxy_of(&cache, redefined, 1), None);
    assert_eq!(proxy_of(&cache, live, 1), Some(ClassId::new(400_011)));
    assert_eq!(cache.call_site_count(), 1);
}
