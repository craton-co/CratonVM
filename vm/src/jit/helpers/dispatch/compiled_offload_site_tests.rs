use super::*;

fn info(class: &'static str, method: &'static str, desc: &'static str) -> JitInvokeInfo {
    JitInvokeInfo {
        class_name: class,
        method_name: method,
        descriptor: desc,
        num_jit_args: 1,
        return_type: b'V',
        invoke_kind: 3,
        declaring_class_id: 0,
        owner_class_id: 0,
    }
}

/// RT-6's whole point. A `JitInvokeInfo` lives in a `Box` inside the
/// artifact's `_jit_invoke_infos`; when the artifact retires that box is
/// freed and the allocator may hand the same address to an unrelated
/// compile. The memo is keyed on the address, so the new site inherits the
/// old one's decision — and a stale `NotKernel` or `Retired` permanently
/// disables offload for a genuine kernel, which is the exact failure the
/// `Unresolved` retry was added to fix, arriving by a different road.
///
/// The identity is what tells the two apart, and it must do so without
/// ever dereferencing the old site's strings, which is why it holds
/// addresses and lengths.
#[test]
fn two_different_sites_never_share_an_identity() {
    let a = info("com/example/A", "kernel", "([I)V");
    let b = info("com/example/B", "kernel", "([I)V");
    assert_ne!(
        CompiledOffloadSiteIdentity::of(&a),
        CompiledOffloadSiteIdentity::of(&b),
        "two sites with different declaring classes must not be mistaken \
         for one another at a recycled address"
    );

    // Same three names, different dispatch kind: still a different site,
    // because `invoke_kind` selects a different resolution entirely.
    let mut c = info("com/example/A", "kernel", "([I)V");
    c.invoke_kind = 0;
    assert_ne!(
        CompiledOffloadSiteIdentity::of(&a),
        CompiledOffloadSiteIdentity::of(&c)
    );

    // And the loader-disambiguating field, which is the one a name-only
    // comparison would miss: two loaders can each define the same class.
    let mut d = info("com/example/A", "kernel", "([I)V");
    d.declaring_class_id = 7;
    assert_ne!(
        CompiledOffloadSiteIdentity::of(&a),
        CompiledOffloadSiteIdentity::of(&d)
    );

    // And the VM: the map is one per process, and one `JitInvokeInfo`
    // (a synthetic site's, or a recycled address) can meet two VMs.
    assert_ne!(
        CompiledOffloadSiteIdentity::of_in(1, &a),
        CompiledOffloadSiteIdentity::of_in(2, &a),
        "one VM's offload decision must not answer for another VM"
    );
}

/// The other direction, which is what keeps the memo a memo: re-deriving
/// the identity of the SAME live site must give the same answer, or every
/// call would miss and pay the three-`Box<str>` name lookup this cache
/// exists to have removed (214 ns/call against 13.8 without it).
#[test]
fn one_site_keeps_its_identity_across_lookups() {
    let a = info("com/example/A", "kernel", "([I)V");
    assert_eq!(
        CompiledOffloadSiteIdentity::of(&a),
        CompiledOffloadSiteIdentity::of(&a)
    );
}

/// A recycled address must not carry a decision forward, and the check
/// that stops it is on the READ. Recorded under one identity, read back
/// under another: the entry is present at that key and must still be
/// treated as absent.
#[test]
fn a_recycled_address_does_not_inherit_the_old_sites_decision() {
    // A key no other test can collide with: this map is process-global.
    let site = 0x5254_3600_usize;
    let old = info("com/example/Retired", "kernel", "([I)V");
    let new = info("com/example/Fresh", "kernel", "([I)V");
    let old_id = CompiledOffloadSiteIdentity::of(&old);
    let new_id = CompiledOffloadSiteIdentity::of(&new);

    record_compiled_offload_site(site, old_id, CompiledOffloadSite::Retired);

    let read_back = |want: CompiledOffloadSiteIdentity| {
        compiled_offload_sites()
            .read()
            .get(&site)
            .copied()
            .filter(|e| e.identity == want)
            .map(|e| e.state)
    };

    assert_eq!(
        read_back(old_id),
        Some(CompiledOffloadSite::Retired),
        "the site that recorded the decision must still see it"
    );
    assert_eq!(
        read_back(new_id),
        None,
        "a different site at the same recycled address must see a MISS, \
         not the retired decision it never made"
    );
}
