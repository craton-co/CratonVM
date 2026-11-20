use super::{ClassId, JdkHandlerRow, ObjectRef, ANNOTATION_JDK_HANDLERS};

fn oref(addr: usize) -> ObjectRef {
    // Never dereferenced: the table hooks only move addresses around.
    unsafe { ObjectRef::from_raw(addr as *mut u8) }
}

fn seed(vm: usize, rows: &[(usize, usize, u32)]) {
    ANNOTATION_JDK_HANDLERS.with(vm, |table| {
        table.clear();
        for &(proxy, handler, owner) in rows {
            table.insert(
                proxy,
                JdkHandlerRow {
                    handler: oref(handler),
                    owner,
                },
            );
        }
    });
}

fn rows(vm: usize) -> Vec<(usize, usize, u32)> {
    let mut out: Vec<(usize, usize, u32)> = ANNOTATION_JDK_HANDLERS
        .peek(vm, |table| {
            table
                .iter()
                .map(|(&p, r)| (p, r.handler.as_ptr() as usize, r.owner))
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}

/// Each handler is reported under its annotation type (the owner), never
/// unconditionally: a live proxy keeps that type's mirror, so the handler
/// lives exactly as long as it can be asked for.
#[test]
fn handlers_are_scanned_under_their_annotation_type() {
    const VM: usize = 0xB0A1;
    seed(VM, &[(0x1000, 0x7000, 33), (0x2000, 0x8000, 34)]);
    let mut seen = Vec::new();
    super::gc_scan_annotation_proxy_entries(VM, &mut |owner, o| {
        seen.push((owner, o.as_ptr() as usize))
    });
    seen.sort();
    assert_eq!(seen, vec![(Some(33), 0x7000), (Some(34), 0x8000)]);
    super::forget_vm_annotation_proxies(VM);
    assert!(rows(VM).is_empty(), "teardown drops the rows");
}

/// A moving collection re-keys a moved proxy and repoints a moved handler.
#[test]
fn a_moving_collection_remaps_keys_and_handlers() {
    const VM: usize = 0xB0A2;
    seed(VM, &[(0x1000, 0x7000, 33), (0x2000, 0x8000, 34)]);
    let mut map = cratonvm_types::PointerMap::new();
    map.insert(0x1000, 0x1100);
    map.insert(0x8000, 0x8100);
    super::gc_update_annotation_proxy_refs(VM, &map);
    assert_eq!(rows(VM), vec![(0x1100, 0x7000, 33), (0x2000, 0x8100, 34)]);
    super::forget_vm_annotation_proxies(VM);
}

/// A row goes when its proxy OR its handler did not survive; a reused
/// proxy address can then never answer another annotation's handler.
#[test]
fn prune_drops_rows_of_dead_proxies_and_dead_handlers() {
    const VM: usize = 0xB0A3;
    seed(
        VM,
        &[
            (0x1000, 0x7000, 33),
            (0x2000, 0x8000, 34),
            (0x3000, 0x9000, 35),
        ],
    );
    let dropped =
        super::prune_dead_annotation_proxies(VM, &|addr| addr != 0x2000 && addr != 0x9000);
    assert_eq!(dropped, 2);
    assert_eq!(rows(VM), vec![(0x1000, 0x7000, 33)]);
    super::forget_vm_annotation_proxies(VM);
}

/// Class unloading drops the rows of an unloaded annotation type; another
/// type's rows and another VM's rows stay.
#[test]
fn unloading_an_annotation_type_drops_its_handlers() {
    const VM: usize = 0xB0A4;
    const OTHER: usize = 0xB0A5;
    seed(VM, &[(0x1000, 0x7000, 33), (0x2000, 0x8000, 34)]);
    seed(OTHER, &[(0x1000, 0x7000, 33)]);
    let dead: rustc_hash::FxHashSet<u32> = [33u32].into_iter().collect();
    super::forget_unloaded_annotation_proxies(VM, &dead, &|_| Some(7));
    assert_eq!(rows(VM), vec![(0x2000, 0x8000, 34)]);
    assert_eq!(rows(OTHER), vec![(0x1000, 0x7000, 33)]);
    super::forget_vm_annotation_proxies(VM);
    super::forget_vm_annotation_proxies(OTHER);
}

/// AO-2: a memoized member order is dropped when its annotation type is
/// redefined or unloaded (a class id is reused after unloading).
#[test]
fn the_member_order_memo_is_purged_for_redefined_and_unloaded_types() {
    const VM: usize = 0xB0A6;
    let names = |v: &[&str]| v.iter().map(|n| n.to_string()).collect::<Vec<_>>();
    super::remember_defaulted_order(VM, ClassId::new(40), &names(&["n", "s", "k"]));
    super::remember_defaulted_order(VM, ClassId::new(41), &names(&["a"]));
    let got = super::memoized_defaulted_order(VM, ClassId::new(40)).expect("memoized");
    assert_eq!(
        got.iter().map(|n| &**n).collect::<Vec<_>>(),
        vec!["n", "s", "k"]
    );
    let redefined: rustc_hash::FxHashSet<u32> = [40u32].into_iter().collect();
    super::forget_redefined_annotation_proxies(VM, &redefined);
    assert!(super::memoized_defaulted_order(VM, ClassId::new(40)).is_none());
    assert!(super::memoized_defaulted_order(VM, ClassId::new(41)).is_some());
    let dead: rustc_hash::FxHashSet<u32> = [41u32].into_iter().collect();
    super::forget_unloaded_annotation_proxies(VM, &dead, &|_| Some(7));
    assert!(super::memoized_defaulted_order(VM, ClassId::new(41)).is_none());
    super::remember_defaulted_order(VM, ClassId::new(42), &names(&["x"]));
    super::forget_vm_annotation_proxies(VM);
    assert!(super::memoized_defaulted_order(VM, ClassId::new(42)).is_none());
}
