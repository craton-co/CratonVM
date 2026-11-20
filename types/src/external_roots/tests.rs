use super::*;

const OWNER: usize = 0xABCD_0000;
const ROOT: usize = 0xDCBA_0000;
thread_local! {
    // Per thread: the provider is registered for the life of the binary
    // and `gen_heap`'s collections call `remap_external_roots` from other
    // tests' threads (gc-common w25-c).
    static SCANS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static REMAPS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn object(addr: usize) -> ObjectRef {
    unsafe { ObjectRef::from_raw(addr as *mut u8) }
}
fn scan(roots: &mut Vec<ObjectRef>) {
    SCANS.with(|c| c.set(c.get() + 1));
    roots.push(object(ROOT));
}
fn owners() -> Option<HashSet<usize>> {
    Some(HashSet::from([OWNER]))
}
fn roots_for_owner(owner: usize, _class_id: Option<u32>) -> Vec<ObjectRef> {
    (owner == OWNER).then(|| object(ROOT)).into_iter().collect()
}
fn matching(predicate: &OwnerPredicate<'_>) -> Vec<ObjectRef> {
    predicate(OWNER).then(|| object(ROOT)).into_iter().collect()
}
fn remap(_map: &crate::PointerMap) {
    REMAPS.with(|c| c.set(c.get() + 1));
}
fn prune(_is_live: &OwnerPredicate<'_>) {}

/// Set only by [`the_paired_generation_is_read_before_the_walk`], on its own
/// thread, so a test running concurrently in this binary cannot consume the
/// one nested registration this fixture performs.
thread_local! {
    static NEST_ON_NEXT_WALK: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

const NESTED_OWNER: usize = 0xABCE_0000;

fn nested_owners() -> Option<HashSet<usize>> {
    Some(HashSet::from([NESTED_OWNER]))
}
fn nested_roots_for_owner(_owner: usize, _class_id: Option<u32>) -> Vec<ObjectRef> {
    Vec::new()
}
fn nested_matching(_predicate: &OwnerPredicate<'_>) -> Vec<ObjectRef> {
    Vec::new()
}
fn nested_scan(_roots: &mut Vec<ObjectRef>) {}
/// A no-op remap. Deliberately NOT the counting `remap` above: these two
/// fixture providers stay registered for the life of the process, and
/// reusing it would make `complete_provider_is_idempotent_and_fans_out`'s
/// `REMAPS == 1` depend on test order.
fn nested_remap(_map: &crate::PointerMap) {}

/// A provider that registers ANOTHER provider from inside its own
/// `owner_addrs` callback — i.e. exactly the mid-walk registration the
/// generation pairing has to survive. `snapshot()` loaded `PUBLISHED` before
/// the walk began, so the nested provider is certainly absent from the
/// owner set this walk returns.
fn nesting_owners() -> Option<HashSet<usize>> {
    let armed = NEST_ON_NEXT_WALK.with(|c| c.replace(false));
    if armed {
        register_external_root_provider(ExternalRootProvider {
            name: "gc-test-provider-nested",
            scan: nested_scan,
            owner_addrs: nested_owners,
            roots_for_owner: nested_roots_for_owner,
            roots_for_matching_owners: nested_matching,
            remap: nested_remap,
            prune,
            gate_stats: None,
            owner_rows: None,
        });
    }
    Some(HashSet::new())
}

/// **The generation must be read BEFORE the walk, or the pair is a false
/// proof.**
///
/// The caller's claim is "the generation is still `g`, so my owner set is
/// still complete". A provider registered mid-walk is missing from the set
/// this call returns — `snapshot()` iterates one immutable, already-loaded
/// table. If the generation were read afterwards it would already count
/// that provider, the caller's revalidation would pass, and the extra-root
/// filter would keep excluding an address the new provider owns roots for:
/// an overlay swept while its owner is live.
///
/// Read before, the pair invalidates itself, which is the safe direction.
#[test]
fn the_paired_generation_is_read_before_the_walk() {
    register_external_root_provider(ExternalRootProvider {
        name: "gc-test-provider-nesting",
        scan: nested_scan,
        owner_addrs: nesting_owners,
        roots_for_owner: nested_roots_for_owner,
        roots_for_matching_owners: nested_matching,
        remap: nested_remap,
        prune,
        gate_stats: None,
        owner_rows: None,
    });

    NEST_ON_NEXT_WALK.with(|c| c.set(true));
    let (owners, _complete, paired) = owner_addrs_and_completeness_with_generation();
    NEST_ON_NEXT_WALK.with(|c| c.set(false));
    let after = provider_table_generation();

    assert!(
        !owners.contains(&NESTED_OWNER),
        "the walk iterates the table it loaded first, so a provider \
         registered during it cannot be in the result -- if this fires the \
         fixture is no longer testing the hazard"
    );
    assert_ne!(
        paired, after,
        "the generation was read AFTER the walk: an owner set missing the \
         nested provider is now labelled with a generation that counts it, \
         and a caller revalidating against it holds a false proof"
    );
    assert!(paired < after);
}

fn provider() -> ExternalRootProvider {
    ExternalRootProvider {
        name: "gc-test-provider",
        scan,
        owner_addrs: owners,
        roots_for_owner,
        roots_for_matching_owners: matching,
        remap,
        prune,
        gate_stats: None,
        owner_rows: None,
    }
}

/// The per-thread scratch is taken with ONE borrow attempt, and a nested
/// call made from inside `visit` (the scratch is borrowed) must still get a
/// correct answer through the fresh-`Vec` arm rather than a panic or an
/// aliased buffer.
#[test]
fn owner_roots_scratch_is_reentrant() {
    register_external_root_provider(provider());
    let (outer, inner) = with_external_roots_for_owner(OWNER, None, |outer| {
        let inner = with_external_roots_for_owner(OWNER, None, |inner| inner.to_vec());
        (outer.to_vec(), inner)
    });
    assert!(outer.contains(&object(ROOT)), "outer call: {outer:?}");
    assert!(inner.contains(&object(ROOT)), "nested call: {inner:?}");
    // And the scratch is usable again once both borrows are gone.
    let again = with_external_roots_for_owner(OWNER, None, |r| r.to_vec());
    assert!(again.contains(&object(ROOT)));
}

// gc-common w10-g: a provider whose prune records, on the CALLING thread,
// which of three owners the predicate condemns -- one inside the
// collecting heap's span and dead, one inside and live, one outside (the
// other VM's). Thread-local, so a concurrent test's prune cannot write it.
const SPAN: (usize, usize) = (0x1000_0000, 0x2000_0000);
const OURS_DEAD: usize = 0x1000_0040;
const OURS_LIVE: usize = 0x1800_0000;
const FOREIGN: usize = 0x3000_0040;
std::thread_local! {
    static CONDEMNED: std::cell::RefCell<Option<Vec<usize>>> =
        const { std::cell::RefCell::new(None) };
}
fn recording_prune(is_live: &OwnerPredicate<'_>) {
    CONDEMNED.with(|c| {
        if let Some(out) = c.borrow_mut().as_mut() {
            out.extend(
                [OURS_DEAD, OURS_LIVE, FOREIGN]
                    .into_iter()
                    .filter(|a| !is_live(*a)),
            );
        }
    });
}
fn no_owners() -> Option<HashSet<usize>> {
    Some(HashSet::new())
}
fn no_roots_for_owner(_owner: usize, _class_id: Option<u32>) -> Vec<ObjectRef> {
    Vec::new()
}
fn no_matching(_predicate: &OwnerPredicate<'_>) -> Vec<ObjectRef> {
    Vec::new()
}
fn no_scan(_roots: &mut Vec<ObjectRef>) {}
fn no_remap(_map: &crate::PointerMap) {}

/// `common-w8d-overlay-prune-condemns-other-vms-collections`: with the
/// collecting heap's span, a predicate that answers "dead" for every
/// address it does not know (as a VM's liveness does for another VM's
/// heap) condemns only the collecting heap's dead owner. Without a span
/// (the Generational arm today) it condemns the foreign owner too -- the
/// defect the screen exists for, kept visible here.
#[test]
fn prune_condemns_only_the_collecting_heaps_owners() {
    register_external_root_provider(ExternalRootProvider {
        name: "gc-test-provider-prune-screen",
        scan: no_scan,
        owner_addrs: no_owners,
        roots_for_owner: no_roots_for_owner,
        roots_for_matching_owners: no_matching,
        remap: no_remap,
        prune: recording_prune,
        gate_stats: None,
        owner_rows: None,
    });
    let is_live = |addr: usize| addr == OURS_LIVE;
    let condemned_with = |owned: Option<(usize, usize)>| {
        CONDEMNED.with(|c| *c.borrow_mut() = Some(Vec::new()));
        prune_external_roots_owned(owned, &is_live);
        CONDEMNED.with(|c| c.borrow_mut().take().unwrap_or_default())
    };
    assert_eq!(condemned_with(Some(SPAN)), vec![OURS_DEAD]);
    assert_eq!(condemned_with(None), vec![OURS_DEAD, FOREIGN]);

    assert!(owner_in_collecting_heap(Some(SPAN), SPAN.0));
    assert!(
        !owner_in_collecting_heap(Some(SPAN), SPAN.1),
        "the span is half-open"
    );
    assert!(!owner_in_collecting_heap(Some(SPAN), SPAN.0 - 8));
    assert!(owner_in_collecting_heap(None, FOREIGN));
}

// gc-common w12-c: the `_for_vm` fan-outs. Driven through the `_in` forms
// with local provider slices, NOT through the process-wide registry: a
// registry fan-out would also call the counting `scan` / `remap` above,
// under `complete_provider_is_idempotent_and_fans_out`'s feet.
std::thread_local! {
    /// `(which callback, vm)` in call order, on the calling thread.
    static SCOPED_CALLS: std::cell::RefCell<Vec<(&'static str, usize)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}
fn note(what: &'static str, vm: usize) {
    SCOPED_CALLS.with(|c| c.borrow_mut().push((what, vm)));
}
fn scoped_scan(vm: usize, roots: &mut Vec<ObjectRef>) {
    note("scan", vm);
    roots.push(object(0x5000_0000 + vm * 8));
}
fn scoped_remap(vm: usize, _map: &crate::PointerMap) {
    note("remap", vm);
}
fn scoped_prune(vm: usize, is_live: &OwnerPredicate<'_>) {
    // Record which of the w10-g fixture owners the screened predicate
    // condemns, tagged with the VM.
    for addr in [OURS_DEAD, OURS_LIVE, FOREIGN] {
        if !is_live(addr) {
            note("prune-condemned", vm ^ addr);
        }
    }
}
fn scoped_forget(vm: usize) {
    note("forget", vm);
}
fn unscoped_scan(_roots: &mut Vec<ObjectRef>) {
    note("unscoped-scan", 0);
}
fn unscoped_remap(_map: &crate::PointerMap) {
    note("unscoped-remap", 0);
}
fn unscoped_prune(_is_live: &OwnerPredicate<'_>) {
    note("unscoped-prune", 0);
}
const SCOPED_CBS: VmScopedRootCallbacks = VmScopedRootCallbacks {
    scan: scoped_scan,
    remap: scoped_remap,
    prune: scoped_prune,
    forget_vm: scoped_forget,
};
fn fixture_provider(name: &'static str) -> ExternalRootProvider {
    ExternalRootProvider {
        name,
        scan: unscoped_scan,
        owner_addrs: no_owners,
        roots_for_owner: no_roots_for_owner,
        roots_for_matching_owners: no_matching,
        remap: unscoped_remap,
        prune: unscoped_prune,
        gate_stats: None,
        owner_rows: None,
    }
}
fn take_scoped_calls() -> Vec<(&'static str, usize)> {
    SCOPED_CALLS.with(|c| std::mem::take(&mut *c.borrow_mut()))
}

/// `common-b-process-global-root-sources` (`collection-overlays`): a
/// provider with VM-scoped callbacks is handed the collecting VM and its
/// VM-less halves are never called; a provider without them keeps the
/// VM-less behaviour.
#[test]
fn w12c_vm_scoped_callbacks_replace_the_vm_less_halves() {
    const VM_A: usize = 0xA11;
    let providers = [
        fixture_provider("w12c-scoped"),
        fixture_provider("w12c-unscoped"),
    ];
    let scoped = [("w12c-scoped", SCOPED_CBS)];
    take_scoped_calls();

    let mut roots = Vec::new();
    scan_for_vm_in(&providers, &scoped, VM_A, &mut roots);
    remap_for_vm_in(
        &providers,
        &scoped,
        VM_A,
        &crate::PointerMap::default(),
    );
    assert_eq!(
        take_scoped_calls(),
        vec![
            ("scan", VM_A),
            ("unscoped-scan", 0),
            ("remap", VM_A),
            ("unscoped-remap", 0)
        ]
    );
    assert_eq!(roots, vec![object(0x5000_0000 + VM_A * 8)]);

    // The prune gets the span-screened predicate either way, so the
    // foreign owner is still never condemned through a VM-less provider.
    let is_live = |addr: usize| addr == OURS_LIVE;
    let screened = |addr: usize| !owner_in_collecting_heap(Some(SPAN), addr) || is_live(addr);
    prune_for_vm_in(&providers, &scoped, VM_A, &screened);
    assert_eq!(
        take_scoped_calls(),
        vec![("prune-condemned", VM_A ^ OURS_DEAD), ("unscoped-prune", 0)]
    );

    assert!(scoped_for(&scoped, "w12c-unscoped").is_none());
    assert!(scoped_for(&scoped, "w12c-scoped").is_some());
}

/// Registration of the VM-scoped record is idempotent for the same
/// callbacks and refuses different ones, like the provider's own. The
/// name is unique to this test and no provider carries it, so the
/// registry fan-outs never reach these callbacks -- except
/// `forget_vm_external_roots`, which reaches every VM-scoped record and
/// is exercised here on this thread.
#[test]
fn w12c_vm_scoped_registration_is_idempotent_and_forget_reaches_it() {
    register_vm_scoped_root_callbacks("w12c-registration-only", SCOPED_CBS);
    register_vm_scoped_root_callbacks("w12c-registration-only", SCOPED_CBS);
    let n = VM_SCOPED
        .read()
        .iter()
        .filter(|(name, _)| *name == "w12c-registration-only")
        .count();
    assert_eq!(n, 1);

    take_scoped_calls();
    forget_vm_external_roots(0xF0F0);
    assert!(take_scoped_calls().contains(&("forget", 0xF0F0)));

    let clash = std::panic::catch_unwind(|| {
        register_vm_scoped_root_callbacks(
            "w12c-registration-only",
            VmScopedRootCallbacks {
                forget_vm: |_: usize| {},
                ..SCOPED_CBS
            },
        );
    });
    assert!(
        clash.is_err(),
        "a different callback set under one name must panic"
    );
}

#[test]
fn complete_provider_is_idempotent_and_fans_out() {
    register_external_root_provider(provider());
    register_external_root_provider(provider());

    SCANS.with(|c| c.set(0));
    REMAPS.with(|c| c.set(0));
    let mut roots = Vec::new();
    scan_external_roots(&mut roots);
    remap_external_roots(&crate::PointerMap::default());

    assert_eq!(SCANS.with(|c| c.get()), 1);
    assert_eq!(REMAPS.with(|c| c.get()), 1);
    assert!(roots.contains(&object(ROOT)));
    assert!(external_owner_addrs().unwrap().contains(&OWNER));
    assert_eq!(external_roots_for_owner(OWNER, None), vec![object(ROOT)]);
    assert_eq!(
        external_roots_for_matching_owners(&|owner| owner == OWNER),
        vec![object(ROOT)]
    );
}
