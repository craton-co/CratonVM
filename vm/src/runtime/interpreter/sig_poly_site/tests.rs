use super::*;

#[test]
fn the_site_key_hash_separates_name_and_descriptor() {
    assert_eq!(
        site_key_hash("get", "(LH;)I"),
        site_key_hash("get", "(LH;)I")
    );
    assert_ne!(
        site_key_hash("get", "(LH;)I"),
        site_key_hash("get", "(LH;)J")
    );
    assert_ne!(
        site_key_hash("get", "(LH;)I"),
        site_key_hash("set", "(LH;)I")
    );
}

/// JIT round 16 wave 7 (lane varhandle): the shapes behind
/// `r16-varhandle-compare-and-set-seven-times-slower-since-round-15`. A
/// three-parameter site's word has its receiver coordinate above the low
/// 16 value bits, which the round-15 merge guard read as "the replay
/// cannot reproduce this door"; the two-parameter controls never had.
/// The route now carries the whole word.
#[test]
fn a_compare_and_set_site_word_has_bits_above_the_value_classes() {
    use cratonvm_native_api::poly_call_site::vh_param_site_classes;
    for three in [
        "(LH;II)Z",
        "(LH;II)I",
        "(LH;Ljava/lang/Object;Ljava/lang/Object;)Z",
    ] {
        assert_ne!(vh_param_site_classes(three) >> 16, 0, "{three}");
    }
    for control in [
        "(LH;)I",
        "(LH;I)I",
        "(LH;Ljava/lang/Object;)Ljava/lang/Object;",
    ] {
        assert_eq!(vh_param_site_classes(control) >> 16, 0, "{control}");
    }
    let door = SigPolyDoor::PolyBlock {
        vh_param_word: Some(vh_param_site_classes("(LH;II)Z")),
        vh_narrow_reference_return: false,
    };
    let whole = vh_param_site_classes("(LH;II)Z");
    assert!(
        matches!(door, SigPolyDoor::PolyBlock { vh_param_word: Some(w), .. } if w == whole),
        "kept whole"
    );
}

/// Interpreter round i2 wave 53 (lane L7): the CAS arm serves the strong
/// `compareAndSet` and its four weak forms, never the exchange forms
/// (whose answer is the witness, not a boolean) nor `getAndSet`.
#[test]
fn the_cas_arm_serves_the_strong_and_weak_boolean_cas_modes_only() {
    for mode in [
        "compareAndSet",
        "weakCompareAndSet",
        "weakCompareAndSetPlain",
        "weakCompareAndSetAcquire",
        "weakCompareAndSetRelease",
    ] {
        assert!(is_boolean_cas_access_mode(mode), "{mode}");
    }
    for mode in [
        "compareAndExchange",
        "compareAndExchangeAcquire",
        "getAndSet",
        "get",
        "weakCompareAndSetVolatile",
    ] {
        assert!(!is_boolean_cas_access_mode(mode), "{mode}");
    }
}

/// Interpreter round i2 wave 54 (lane L7, `CRATONVM_VH_INTERP_CAE_FAST`):
/// the exchange arm serves the three exchange modes over an `int` or
/// `long` variable whose site returns that kind, nothing else.
#[test]
fn i54_l7_the_exchange_arm_serves_int_and_long_exchange_sites_only() {
    use crate::jit::helpers::varhandle_cae_site_kind;
    for mode in [
        "compareAndExchange",
        "compareAndExchangeAcquire",
        "compareAndExchangeRelease",
    ] {
        assert!(is_exchange_access_mode(mode), "{mode}");
    }
    for mode in [
        "compareAndSet",
        "getAndSet",
        "getAndAdd",
        "weakCompareAndSet",
    ] {
        assert!(!is_exchange_access_mode(mode), "{mode}");
    }
    assert_eq!(varhandle_cae_site_kind("(LH;II)I", b'I'), Some(b'I'));
    assert_eq!(varhandle_cae_site_kind("(LH;JJ)J", b'J'), Some(b'J'));
    for (desc, ret) in [
        ("(LH;II)Z", b'Z'),
        ("(LH;IJ)J", b'J'),
        (
            "(LH;Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            b'L',
        ),
        ("(LH;BB)B", b'B'),
        ("(LH;DD)D", b'D'),
        ("(II)I", b'I'),
    ] {
        assert_eq!(varhandle_cae_site_kind(desc, ret), None, "{desc}");
    }
}

/// Interpreter round i2 wave 54 (lane L7, `CRATONVM_SIG_POLY_SITE_FIRST`):
/// the site filter has no false negative, says nothing for an empty
/// table, and is cleared with the table.
#[test]
fn i54_l7_the_site_filter_holds_every_filled_site_and_clears_with_the_table() {
    let mut cache = SigPolySiteCache::default();
    let sites = [(3u16, 0usize), (3, 7), (40, 7), (65_535, 65_530), (1, 12)];
    for &(cp, pc) in &sites {
        assert!(site_filter_bit(cp, pc) < SITE_FILTER_BITS);
        assert!(!cache.may_hold(cp, pc), "an empty table holds nothing");
    }
    for &(cp, pc) in &sites {
        let bit = site_filter_bit(cp, pc);
        cache.filter[bit >> 6] |= 1u64 << (bit & 63);
    }
    for &(cp, pc) in &sites {
        assert!(cache.may_hold(cp, pc), "({cp}, {pc})");
    }
    // Neighbouring pcs of one method spread over the filter.
    // addr-keyed: not an address (filter bit indices, local to this test).
    let distinct: std::collections::HashSet<usize> =
        (0..64usize).map(|pc| site_filter_bit(9, pc * 3)).collect();
    assert!(distinct.len() > 48, "{}", distinct.len());
    cache.clear_sites();
    for &(cp, pc) in &sites {
        assert!(!cache.may_hold(cp, pc));
    }
}

fn test_site(name: &str) -> Arc<SigPolySite> {
    Arc::new(SigPolySite {
        method_name: Arc::from(name),
        descriptor: Arc::from("(LH;)I"),
        facts: cratonvm_jit_api::DescriptorFacts::of("(LH;)I"),
        num_params: 1,
        is_var_handle_site: true,
        vh_cas_site_kind: None,
        vh_cae_site_kind: None,
        registry_generation: 0,
        routes: smallvec::SmallVec::new(),
    })
}

fn slot(name: &str) -> SigPolySlot {
    SigPolySlot {
        site: Some(test_site(name)),
        reentrant: false,
    }
}

/// Interpreter round i2 wave 57 (lane L7, `CRATONVM_SIG_POLY_SITE_LEND`):
/// a lent site leaves its entry empty, comes back on `give_back` with no
/// extra reference, and is NOT put back over a clear (the table moved to
/// another redefinition count) or over a nested fill of the same key. A
/// nested probe of a lent entry declines once and marks the entry
/// reentrant, which is served by a clone from then on.
#[test]
fn i57_l7_a_lent_site_comes_back_unless_the_table_moved_on() {
    let key = (ClassId::new(7), 3u16, 11u32);
    let mut cache = SigPolySiteCache::default();
    cache.sites.insert(key, slot("get"));
    // Other tests may redefine a class meanwhile (the count is a process
    // counter): the table is made current right before each lend, and
    // the entry is read directly, not through `get`.
    let current = |cache: &mut SigPolySiteCache| {
        cache.redefinitions_seen = crate::classloading::class_redefinition_count();
    };
    let lend = |cache: &mut SigPolySiteCache| {
        current(cache);
        let lent = cache.lend(key).expect("lent");
        assert_eq!(lent.1, Some(cache.redefinitions_seen));
        lent
    };
    let name_in = |cache: &SigPolySiteCache| {
        cache
            .sites
            .get(&key)?
            .site
            .as_ref()
            .map(|s| s.method_name.to_string())
    };

    // Lend and give back: one reference, the entry restored.
    let (site, under) = lend(&mut cache);
    assert_eq!(
        Arc::strong_count(&site),
        1,
        "lending moves, it does not clone"
    );
    assert_eq!(name_in(&cache), None, "a lent entry is empty");
    cache.give_back(key, site, under);
    assert_eq!(name_in(&cache).as_deref(), Some("get"));
    assert!(!cache.sites[&key].reentrant);

    // A nested fill of the same key while lent: the newer site stays.
    let (site, under) = lend(&mut cache);
    cache.sites.insert(key, slot("set"));
    cache.give_back(key, site, under);
    assert_eq!(name_in(&cache).as_deref(), Some("set"));

    // A clear while lent (a redefinition): nothing is put back.
    let (site, under) = lend(&mut cache);
    cache.clear_sites();
    cache.redefinitions_seen = under.expect("lent") + 1;
    cache.sites.insert(
        key,
        SigPolySlot {
            site: None,
            reentrant: false,
        },
    );
    cache.give_back(key, site, under);
    assert_eq!(name_in(&cache), None, "not put back over a clear");

    // A nested probe of a lent entry: declines once, marks it reentrant;
    // the outer give-back restores it, and later probes clone.
    cache.sites.insert(key, slot("get"));
    let (site, under) = lend(&mut cache);
    current(&mut cache);
    assert!(
        cache.lend(key).is_none(),
        "a nested probe of a lent entry declines"
    );
    assert!(cache.sites[&key].reentrant, "and marks it reentrant");
    cache.give_back(key, site, under);
    assert_eq!(name_in(&cache).as_deref(), Some("get"));
    current(&mut cache);
    let (outer, outer_under) = cache.lend(key).expect("cloned");
    assert_eq!(outer_under, None, "a reentrant entry is cloned, not lent");
    assert_eq!(Arc::strong_count(&outer), 2);
    current(&mut cache);
    let (inner, inner_under) = cache.lend(key).expect("a nested probe is served");
    assert_eq!(inner_under, None);
    cache.give_back(key, inner, inner_under);
    cache.give_back(key, outer, outer_under);
    assert_eq!(name_in(&cache).as_deref(), Some("get"));
    assert_eq!(
        Arc::strong_count(cache.sites[&key].site.as_ref().expect("site")),
        1
    );
}

#[test]
fn an_idle_record_is_the_default() {
    assert!(matches!(SigPolyRecord::default(), SigPolyRecord::Idle));
    assert_eq!(SigPolySiteCache::default().len(), 0);
}
