use super::ObjectRef;

const A: usize = 0x1000;
const B: usize = 0x2000;

fn oref(addr: usize) -> ObjectRef {
    // Never dereferenced: `forget_unloaded_annotation_proxies` only reads
    // the address and hands it to the caller-supplied `class_id_of`, which
    // these tests stub out.
    unsafe { ObjectRef::from_raw(addr as *mut u8) }
}

fn seed(vm: usize, rows: &[(u32, &str, usize)]) {
    super::ANNOTATION_PROXY_CACHE.with(vm, |table| {
        table.clear();
        for (holder, desc, addr) in rows {
            table.insert((*holder, (*desc).to_string()), oref(*addr));
        }
    });
}

fn rows(vm: usize) -> usize {
    super::ANNOTATION_PROXY_CACHE
        .peek(vm, |table| table.len())
        .unwrap_or(0)
}

/// A JVMTI redefinition replaced the annotations of holder 11 (interpreter
/// round i1 wave 27, lane L3): its rows go, class and method keys alike,
/// and another holder's stay.
#[test]
fn a_redefined_holders_rows_are_dropped() {
    const VM: usize = 0xA0A7;
    seed(
        VM,
        &[
            (11, "Lorg/example/Ann;", A),
            (11, "m()V Lorg/example/Ann;", B),
            (12, "Lorg/example/Ann;", A),
        ],
    );
    let redefined: rustc_hash::FxHashSet<u32> = [11u32].into_iter().collect();
    assert_eq!(
        super::forget_redefined_annotation_proxies(VM, &redefined),
        2
    );
    assert_eq!(rows(VM), 1);
    super::forget_vm_annotation_proxies(VM);
}

/// The row this whole function exists for: the HOLDER is still perfectly
/// alive (a Spring autoconfiguration class on the app loader), and it is
/// the cached proxy's own generated `$ProxyN` class that the collector
/// retired with its per-context loader. Keeping that row is what returned
/// an instance of an unloaded class to `getDeclaredAnnotations()` and threw
/// `NoClassDefFoundError: jdk/proxy1/$Proxy26`.
#[test]
fn a_proxy_whose_generated_class_was_unloaded_is_dropped() {
    const VM: usize = 0xA0A0;
    seed(VM, &[(11, "Lorg/example/Ann;", A)]);
    let dead: rustc_hash::FxHashSet<u32> = [990u32].into_iter().collect();
    // The proxy at A is an instance of the unloaded class 990; the holder
    // (11) is untouched.
    let dropped = super::forget_unloaded_annotation_proxies(VM, &dead, &|o| {
        Some(if o.as_ptr() as usize == A { 990 } else { 7 })
    });
    assert_eq!(dropped, 1, "the row naming an unloaded proxy class must go");
    assert_eq!(rows(VM), 0);
}

/// A live row is not collateral damage.
#[test]
fn a_live_row_survives_the_purge() {
    const VM: usize = 0xA0A1;
    seed(
        VM,
        &[(11, "Lorg/example/Ann;", A), (12, "Lorg/example/Two;", B)],
    );
    let dead: rustc_hash::FxHashSet<u32> = [990u32].into_iter().collect();
    let dropped = super::forget_unloaded_annotation_proxies(VM, &dead, &|o| {
        Some(if o.as_ptr() as usize == A { 990 } else { 7 })
    });
    assert_eq!(dropped, 1);
    assert_eq!(rows(VM), 1, "the row whose proxy class is alive must stay");
    assert!(super::ANNOTATION_PROXY_CACHE
        .peek(VM, |t| t
            .contains_key(&(12u32, "Lorg/example/Two;".to_string())))
        .unwrap_or(false));
}

/// The key half: a row keyed on a retired holder can only be matched again
/// through class-id reuse, and matching by reuse would hand a fresh class
/// the retired one's annotations.
#[test]
fn a_row_keyed_on_an_unloaded_holder_is_dropped_too() {
    const VM: usize = 0xA0A2;
    seed(VM, &[(11, "Lorg/example/Ann;", A)]);
    let dead: rustc_hash::FxHashSet<u32> = [11u32].into_iter().collect();
    let dropped = super::forget_unloaded_annotation_proxies(VM, &dead, &|_| Some(7));
    assert_eq!(dropped, 1);
    assert_eq!(rows(VM), 0);
}

/// A ref that no longer names a valid object cannot be handed back either.
#[test]
fn a_proxy_whose_ref_no_longer_resolves_is_dropped() {
    const VM: usize = 0xA0A3;
    seed(VM, &[(11, "Lorg/example/Ann;", A)]);
    let dead: rustc_hash::FxHashSet<u32> = [990u32].into_iter().collect();
    let dropped = super::forget_unloaded_annotation_proxies(VM, &dead, &|_| None);
    assert_eq!(dropped, 1);
    assert_eq!(rows(VM), 0);
}

/// An empty dead set is a no-op — every collection that unloaded nothing
/// must not churn a cache that is entirely valid.
#[test]
fn an_empty_dead_set_drops_nothing() {
    const VM: usize = 0xA0A4;
    seed(VM, &[(11, "Lorg/example/Ann;", A)]);
    let dead: rustc_hash::FxHashSet<u32> = rustc_hash::FxHashSet::default();
    let dropped = super::forget_unloaded_annotation_proxies(VM, &dead, &|_| {
        panic!("class_id_of must not be consulted for an empty dead set")
    });
    assert_eq!(dropped, 0);
    assert_eq!(rows(VM), 1);
}

/// Another VM's rows are none of this VM's business — the same partition
/// the `vm_identity` scoping exists to keep.
#[test]
fn another_vms_rows_are_untouched() {
    const MINE: usize = 0xA0A5;
    const THEIRS: usize = 0xA0A6;
    seed(MINE, &[(11, "Lorg/example/Ann;", A)]);
    seed(THEIRS, &[(11, "Lorg/example/Ann;", A)]);
    let dead: rustc_hash::FxHashSet<u32> = [990u32].into_iter().collect();
    super::forget_unloaded_annotation_proxies(MINE, &dead, &|_| Some(990));
    assert_eq!(rows(MINE), 0);
    assert_eq!(rows(THEIRS), 1, "the other VM's row must survive");
}

fn entries(vm: usize) -> Vec<(Option<u32>, usize)> {
    let mut out = Vec::new();
    super::gc_scan_annotation_proxy_entries(vm, &mut |owner, o| {
        out.push((owner, o.as_ptr() as usize))
    });
    out.sort();
    out
}

/// gc-common w3-b: the root scan names the HOLDER of every cached proxy
/// and of that proxy's child roots, so the VM can pin a user-loader
/// holder's annotations to its loader instead of rooting them forever. A
/// child row whose proxy is not cached is still reported owner-less
/// (rooted), and the last-proxy interfaces array is not part of it.
#[test]
fn entries_scan_reports_each_proxy_and_its_children_under_the_holder() {
    const VM: usize = 0xA0A9;
    const C1: usize = 0x3000;
    const C2: usize = 0x4000;
    const ORPHAN_PROXY: usize = 0x5000;
    const C3: usize = 0x6000;
    seed(VM, &[(11, "Lorg/example/Ann;", A)]);
    super::remember_annotation_proxy_child_roots(VM, oref(A), vec![oref(C1), oref(C2)]);
    super::remember_annotation_proxy_child_roots(VM, oref(ORPHAN_PROXY), vec![oref(C3)]);
    assert_eq!(
        entries(VM),
        vec![(None, C3), (Some(11), A), (Some(11), C1), (Some(11), C2)]
    );
    let mut plain = Vec::new();
    super::gc_scan_annotation_proxy_roots(VM, &mut plain);
    assert!(
        plain.is_empty(),
        "no last-proxy array was recorded for this VM"
    );
    super::forget_vm_annotation_proxies(VM);
    assert!(entries(VM).is_empty());
}

/// A dead cached proxy and the child row of a dead (uncached) proxy are
/// dropped; live rows stay.
#[test]
fn prune_drops_dead_proxies_and_orphaned_child_rows() {
    const VM: usize = 0xA0A8;
    const ORPHAN_PROXY: usize = 0x5000;
    seed(
        VM,
        &[(11, "Lorg/example/Ann;", A), (12, "Lorg/example/Two;", B)],
    );
    super::remember_annotation_proxy_child_roots(VM, oref(A), vec![oref(0x3000)]);
    super::remember_annotation_proxy_child_roots(VM, oref(ORPHAN_PROXY), vec![oref(0x6000)]);
    let dropped = super::prune_dead_annotation_proxies(VM, &|addr| addr == A);
    assert_eq!(dropped, 2, "the proxy at B and the orphan's child row");
    assert_eq!(entries(VM), vec![(Some(11), A), (Some(11), 0x3000)]);
    super::forget_vm_annotation_proxies(VM);
}
