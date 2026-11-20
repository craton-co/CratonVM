use super::*;

#[test]
fn parse_simple_class_sig() {
    let sig = parse_class_signature("Ljava/lang/Object;").unwrap();
    assert!(sig.type_params.is_empty());
    match &sig.super_class {
        TypeSig::Class {
            name,
            type_args,
            owner,
        } => {
            assert_eq!(name, "java/lang/Object");
            assert!(type_args.is_empty());
            assert!(owner.is_none());
        }
        other => panic!("expected Class, got {:?}", other),
    }
}

#[test]
fn parse_nested_class_owner_chain() {
    // `Outer<Integer>.Inner<Long>` as a field signature: the inner type
    // carries its OWN args (`Long`) and an owner node for `Outer<Integer>`,
    // with the flat `$`-joined raw name. Regression for the bug where the
    // inner args were discarded and the outer args left on the inner name.
    let sig =
        parse_field_signature("LOuter<Ljava/lang/Integer;>.Inner<Ljava/lang/Long;>;").unwrap();
    match sig {
        TypeSig::Class {
            name,
            type_args,
            owner,
        } => {
            assert_eq!(name, "Outer$Inner");
            assert_eq!(type_args.len(), 1);
            assert_eq!(
                type_args[0],
                TypeArg::Exact(TypeSig::Class {
                    name: "java/lang/Long".into(),
                    type_args: vec![],
                    owner: None,
                })
            );
            let owner = owner.expect("owner must be present");
            match *owner {
                TypeSig::Class {
                    name,
                    type_args,
                    owner,
                } => {
                    assert_eq!(name, "Outer");
                    assert_eq!(
                        type_args[0],
                        TypeArg::Exact(TypeSig::Class {
                            name: "java/lang/Integer".into(),
                            type_args: vec![],
                            owner: None,
                        })
                    );
                    assert!(owner.is_none());
                }
                other => panic!("expected owner Class, got {:?}", other),
            }
        }
        other => panic!("expected Class, got {:?}", other),
    }
}

#[test]
fn parse_void_method() {
    let sig = parse_method_signature("()V").unwrap();
    assert!(sig.param_types.is_empty());
    assert!(matches!(sig.return_type, TypeSig::Base('V')));
}

#[test]
fn trailing_garbage_rejected_by_uncached_parsers() {
    assert!(parse_class_signature("Ljava/lang/Object;garbage").is_none());
    assert!(parse_method_signature("()Vgarbage").is_none());
    assert!(parse_field_signature("Ljava/lang/Object;garbage").is_none());
}

#[test]
fn trailing_garbage_rejected_after_complex_generic_signatures() {
    assert!(parse_class_signature(
        "<T:Ljava/lang/Object;>Ljava/lang/Object;Ljava/io/Serializable;garbage"
    )
    .is_none());
    assert!(parse_method_signature(concat!(
        "<T:Ljava/lang/Object;>",
        "(Ljava/util/List<TT;>;)",
        "Ljava/util/List<TT;>;",
        "^Ljava/lang/Exception;garbage"
    ))
    .is_none());
    assert!(parse_field_signature("[Ljava/util/List<+Ljava/lang/Number;>;garbage").is_none());
}

#[test]
fn trailing_garbage_rejected_by_cached_parsers() {
    let class_sig: Arc<str> = Arc::from("Ljava/lang/Object;cached_class_garbage");
    let method_sig: Arc<str> = Arc::from("()Vcached_method_garbage");
    let field_sig: Arc<str> = Arc::from("Ljava/lang/Object;cached_field_garbage");

    assert!(parse_class_signature_cached(&class_sig).is_none());
    assert!(parse_method_signature_cached(&method_sig).is_none());
    assert!(parse_field_signature_cached(&field_sig).is_none());
}

#[test]
fn deeply_nested_signature_is_rejected_not_overflow() {
    // A pathological array signature `[[[...I` nested far past the
    // depth cap must fail to parse rather than overflow the stack.
    let bomb = format!("{}I", "[".repeat(100_000));
    assert!(parse_field_signature(&bomb).is_none());

    // Deeply-nested generic type arguments are also bounded: build
    // `Lp<Lp<Lp<...>;>;>;` to a hostile depth and confirm no overflow.
    let mut nested = String::new();
    for _ in 0..100_000 {
        nested.push_str("Lp<");
    }
    nested.push_str("Lp;");
    for _ in 0..100_000 {
        nested.push_str(">;");
    }
    assert!(parse_field_signature(&nested).is_none());
}

#[test]
fn cached_parse_round_trips() {
    // Use a signature unique to this test so we don't race with
    // sibling tests that share the global signature cache.
    let key: Arc<str> = Arc::from("()Lround_trips_marker;");
    let m = parse_method_signature_cached(&key).expect("cached parse");
    assert!(m.param_types.is_empty());
    // Hit returns the same Arc (refcount > 1 because the cache
    // holds one reference too).
    let m2 = parse_method_signature_cached(&key).expect("cached hit");
    assert!(Arc::ptr_eq(&m, &m2));
}

#[test]
fn cached_invalid_is_remembered() {
    // Use a unique invalid signature so we don't race with other
    // tests on the shared cache.
    let bad: Arc<str> = Arc::from("not a sig // invalid_remembered_marker");
    assert!(parse_field_signature_cached(&bad).is_none());
    // Second call still returns None — the cache should record
    // the Invalid verdict and short-circuit.
    assert!(parse_field_signature_cached(&bad).is_none());
}

#[test]
fn cached_invalid_verdict_is_shape_qualified() {
    // Regression: the memoized `Invalid` verdict used to be recorded
    // against the signature *string* alone, not the (string, shape)
    // pair. `<T:Ljava/lang/Object;>L...;` is a perfectly valid CLASS
    // signature but is NOT a valid FIELD signature (a field signature
    // is a single type signature, which cannot start with `<`). So a
    // caller that probed the field shape first poisoned the entry, and
    // a later class lookup returned `None` without ever re-parsing —
    // generic type information silently disappeared depending on
    // lookup order.
    //
    // The unique class name keeps this key off the process-global
    // cache's other entries (cf. `cached_and_uncached_agree_on_depth_bomb`).
    // It has to be unique *within* the grammar: a trailing `//marker`
    // comment would leave the parser short of `at_end()` and make the
    // string invalid for every shape.
    let key: Arc<str> = Arc::from("<T:Ljava/lang/Object;>Lshapequal/InvalidVerdictMarker;");

    // Ground truth from the uncached parsers: valid as a class
    // signature, invalid as a field signature.
    assert!(
        parse_class_signature(&key).is_some(),
        "uncached class parse must accept a generic class signature"
    );
    assert!(
        parse_field_signature(&key).is_none(),
        "uncached field parse must reject a class signature"
    );

    // Poison the entry via the field shape first...
    assert!(
        parse_field_signature_cached(&key).is_none(),
        "cached field parse must agree with the uncached verdict"
    );
    // ...and the class shape must still parse it.
    assert!(
        parse_class_signature_cached(&key).is_some(),
        "an Invalid verdict recorded for the field shape must not \
         short-circuit a class lookup"
    );
    // Both orders, and the memoized second call, stay consistent.
    assert!(
        parse_class_signature_cached(&key).is_some(),
        "class verdict must stay Some once memoized"
    );
    assert!(
        parse_field_signature_cached(&key).is_none(),
        "field verdict must stay None"
    );
}

#[test]
fn invalid_verdicts_merge_into_one_entry() {
    // Shape-qualifying the verdict must not cost memoization: a string
    // that is garbage under all three grammars still ends up with one
    // cache entry carrying all three rejection flags, so every entry
    // point short-circuits after its own first probe.
    let bad: Arc<str> = Arc::from("not a sig // merge_invalid_marker");
    assert!(parse_field_signature_cached(&bad).is_none());
    assert!(parse_class_signature_cached(&bad).is_none());
    assert!(parse_method_signature_cached(&bad).is_none());

    let cache = signature_cache().lock();
    match cache.map.get(&bad) {
        Some(ParsedSignature::Invalid {
            class,
            method,
            field,
        }) => {
            assert!(*class && *method && *field, "all shapes must be recorded");
        }
        _ => panic!("expected a merged Invalid entry"),
    }
    // One string, one entry — the merge must not fan out into three.
    assert_eq!(
        cache.order.iter().filter(|k| ***k == *bad).count(),
        1,
        "merging must keep exactly one recency slot per key"
    );
}

#[test]
fn cached_and_uncached_agree_on_depth_bomb() {
    // Regression for the depth-guard bypass: a generic signature
    // nested past MAX_SIG_DEPTH must be rejected on BOTH the cached
    // and uncached paths. Previously the cached path discarded the
    // parser and never read `depth_exceeded`, so it accepted (and
    // memoized) a partial AST the uncached path rejects.
    let mut nested = String::new();
    for _ in 0..100_000 {
        nested.push_str("Lp<");
    }
    nested.push_str("Lp;");
    for _ in 0..100_000 {
        nested.push_str(">;");
    }
    // Unique marker so we don't collide with the sibling
    // `deeply_nested_signature_is_rejected_not_overflow` test or
    // race on the shared cache.
    nested.push_str("// cached_depth_bomb_marker");
    let key: Arc<str> = Arc::from(nested.as_str());

    // Uncached verdict is the ground truth.
    assert!(
        parse_field_signature(&key).is_none(),
        "uncached path must reject a depth-bombed signature"
    );
    // Cached path must agree on the FIRST (parsing) call...
    assert!(
        parse_field_signature_cached(&key).is_none(),
        "cached path must honor the depth-exceeded guard"
    );
    // ...and on subsequent (memoized) calls — the verdict cached
    // must be Invalid, not a partial AST.
    assert!(
        parse_field_signature_cached(&key).is_none(),
        "cached path must remember the Invalid verdict"
    );
}

#[test]
fn reinsert_keeps_order_and_map_in_lockstep() {
    // Regression for the LRU dedup fix: re-inserting an already-present
    // key must REFRESH its single recency slot, never append a duplicate.
    // Otherwise `order.len()` drifts past `map.len()` and eviction can
    // drop a still-hot key.
    let mut cache = SignatureCacheInner::new();
    let k: Arc<str> = Arc::from("Ldedup/Marker;");

    cache.insert(
        Arc::clone(&k),
        ParsedSignature::invalid_for(SigShape::Field),
    );
    assert_eq!(cache.map.len(), 1);
    assert_eq!(cache.order.len(), 1);

    // Re-insert the SAME content many times (including via a distinct
    // Arc allocation carrying the same bytes — the map keys by content,
    // so the dedup must match on content, not pointer identity).
    for _ in 0..100 {
        cache.insert(
            Arc::clone(&k),
            ParsedSignature::invalid_for(SigShape::Field),
        );
        let fresh_alloc: Arc<str> = Arc::from("Ldedup/Marker;");
        cache.insert(fresh_alloc, ParsedSignature::invalid_for(SigShape::Field));
    }

    // The map still holds exactly one entry and the recency queue is in
    // lockstep — no duplicate slots accumulated.
    assert_eq!(cache.map.len(), 1, "map must hold one entry per key");
    assert_eq!(
        cache.order.len(),
        cache.map.len(),
        "order must stay in lockstep with map (one slot per key)"
    );
}
