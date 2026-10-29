// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

use super::*;

#[cfg(test)]
mod r15w7_drainc_tp_target_tests {
    use super::*;
    use cratonvm_native_api::test_mock::MockNativeContext;
    use cratonvm_native_api::NativeHeapAccess;

    /// `r15w6-sweepC-review-findings` S1: a wrapper whose `e` is a pool
    /// delegates to the pool (it used to run `ThreadPoolExecutor`'s body on
    /// the wrapper), and a real pool declaring its own `e` that holds no pool
    /// runs its own body (it used to get the synthetic one).
    #[test]
    fn a_wrapper_delegates_and_a_pool_with_its_own_e_runs_its_body() {
        let mut ctx = MockNativeContext::new();
        let workers = ctx.fresh_object_ref();
        let pool = ctx.fresh_object_ref();
        ctx.set_field_by_name(pool, "workers", Value::Object(Some(workers)));
        let wrapper = ctx.fresh_object_ref();
        ctx.set_field_by_name(wrapper, "e", Value::Object(Some(pool)));
        let sub = ctx.fresh_object_ref();
        let not_a_pool = ctx.fresh_object_ref();
        ctx.set_field_by_name(sub, "workers", Value::Object(Some(workers)));
        ctx.set_field_by_name(sub, "e", Value::Object(Some(not_a_pool)));
        let placeholder = ctx.fresh_object_ref();

        assert!(matches!(tp_target(&mut ctx, wrapper), TpTarget::Delegate(p) if p == pool));
        assert!(matches!(tp_target(&mut ctx, pool), TpTarget::Own));
        assert!(matches!(tp_target(&mut ctx, sub), TpTarget::Own));
        assert!(matches!(
            tp_target(&mut ctx, placeholder),
            TpTarget::Synthetic
        ));
        // The old predicate, for contrast: both shapes answered the other way.
        assert!(tp_is_real(&mut ctx, wrapper));
        assert!(!tp_is_real(&mut ctx, sub));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Round 15 wave 2 (lane compat): the immutable factories' JDK shapes --
    // C4-3's canonical empties (`CRATONVM_COMPAT_IMMUTABLE_EMPTY_CANONICAL`)
    // and the null checks (`CRATONVM_COMPAT_IMMUTABLE_OF_NULL_CHECKS`). Both
    // default on; nothing here sets them.
    // -----------------------------------------------------------------------

    /// The message of an NPE result, `None` for anything else (a bare NPE too).
    fn r15_npe_message(r: &MethodCallResult) -> Option<String> {
        match r {
            Err(MethodCallFailed::InternalError(cratonvm_types::error::VmError::Runtime(
                cratonvm_types::error::RuntimeError::NullPointerException { message },
            ))) => message.clone(),
            _ => None,
        }
    }

    fn r15_obj(r: MethodCallResult) -> Option<ObjectRef> {
        match r {
            Ok(Some(Value::Object(Some(o)))) => Some(o),
            _ => None,
        }
    }

    /// `List.of((Object[]) null)` / `Set.of(..)` / `Map.ofEntries(..)` open
    /// with the array's `length`: an NPE on HotSpot. The natives answered an
    /// empty collection.
    #[test]
    fn r15_compat_factories_refuse_a_null_array_as_the_jdk_does() {
        let mut ctx = lbq_blocking_tests::MockCtx::new(1);
        let null = [Value::Object(None)];
        let elements = "Cannot read the array length because \"elements\" is null";
        assert_eq!(
            r15_npe_message(&native_list_of_array(&mut ctx, &null)).as_deref(),
            Some(elements)
        );
        assert_eq!(
            r15_npe_message(&native_set_of_array(&mut ctx, &null)).as_deref(),
            Some(elements)
        );
        assert_eq!(
            r15_npe_message(&native_map_of_entries(&mut ctx, &null)).as_deref(),
            Some("Cannot read the array length because \"entries\" is null")
        );
    }

    /// `Map.ofEntries` dereferences every slot. The native skipped a null one,
    /// so `Map.ofEntries(null)` was `{}` and `Map.ofEntries(e, null)` was
    /// `{e}` (here: a bare NPE, the mock's `e` answering null accessors).
    #[test]
    fn r15_compat_map_of_entries_dereferences_a_null_slot() {
        let mut ctx = lbq_blocking_tests::MockCtx::new(1);
        let one = alloc_ref_array(&mut ctx, 1);
        let r = native_map_of_entries(&mut ctx, &[Value::Object(Some(one))]);
        assert_eq!(
            r15_npe_message(&r).as_deref(),
            Some("Cannot invoke \"java.util.Map$Entry.getKey()\" because \"entries[0]\" is null")
        );
        let two = alloc_ref_array(&mut ctx, 2);
        let e = ctx.alloc_object_of(ClassId::new(4242), 2);
        ctx.set_array_element(two, 0, Value::Object(Some(e)));
        let r = native_map_of_entries(&mut ctx, &[Value::Object(Some(two))]);
        assert_eq!(
            r15_npe_message(&r).as_deref(),
            Some("Cannot invoke \"java.util.Map$Entry.getKey()\" because \"entry\" is null")
        );
    }

    /// `copyOf(null)`: JDK 25 dereferences the argument with `isEmpty()`, so the
    /// NPE carries that call's helpful message (the native's was bare).
    #[test]
    fn r15_compat_copy_of_null_names_the_is_empty_call() {
        let mut ctx = lbq_blocking_tests::MockCtx::new(1);
        let null = [Value::Object(None)];
        let coll = "Cannot invoke \"java.util.Collection.isEmpty()\" because \"coll\" is null";
        assert_eq!(
            r15_npe_message(&native_list_copy_of(&mut ctx, &null)).as_deref(),
            Some(coll)
        );
        assert_eq!(
            r15_npe_message(&native_set_copy_of(&mut ctx, &null)).as_deref(),
            Some(coll)
        );
        assert_eq!(
            r15_npe_message(&native_map_copy_of(&mut ctx, &null)).as_deref(),
            Some("Cannot invoke \"java.util.Map.isEmpty()\" because \"map\" is null")
        );
    }

    /// C4W4-6 (`CRATONVM_COMPAT_NULL_COLLECTION_MESSAGE`, round 15 wave 4,
    /// lane compat5): `addAll(null)` and the copy constructors carry the
    /// helpful message of the JDK line that dereferences `c` (HotSpot 25,
    /// `R15Compat4NullArgMessages` rows 0-5, 8-9); `removeAll(null)` keeps
    /// RULE C's message-less NPE (row 6); `LinkedList.addAll(index, null)`
    /// checks the index first.
    #[test]
    fn r15_compat5_null_collection_names_the_dereference() {
        let mut ctx = lbq_blocking_tests::MockCtx::new(1);
        let recv = Value::Object(Some(ctx.alloc_object_of(ClassId::new(4250), 8)));
        let nul = Value::Object(None);
        type Body = fn(&mut dyn NativeContext, &[Value]) -> MethodCallResult;
        let rows: [(&str, Body, &str); 9] = [
            ("ArrayList.addAll", native_al_add_all, NULL_C_TO_ARRAY),
            ("LinkedList.addAll", native_ll_add_all, NULL_C_TO_ARRAY),
            ("ArrayDeque.addAll", native_ad_add_all, NULL_C_SIZE),
            ("ArrayDeque(c)", native_ad_init_from_collection, NULL_C_SIZE),
            (
                "LinkedList(c)",
                native_ll_init_from_collection,
                NULL_C_TO_ARRAY,
            ),
            (
                "ArrayList(c)",
                native_al_init_from_collection,
                NULL_C_TO_ARRAY,
            ),
            ("HashSet(c)", native_hs_init_from_collection, NULL_C_SIZE),
            // `containsAll(null)` answered `false` (G8-1 section 2.2 measured).
            (
                "HashSet.containsAll",
                native_hs_contains_all,
                NULL_C_ITERATOR,
            ),
            (
                "KeySetView.containsAll",
                native_ksv_contains_all,
                NULL_C_ITERATOR,
            ),
        ];
        for (name, body, want) in rows {
            let r = body(&mut ctx, &[recv, nul]);
            assert_eq!(r15_npe_message(&r).as_deref(), Some(want), "{name}: {r:?}");
        }
        let is_npe = |r: &MethodCallResult| {
            matches!(
                r,
                Err(MethodCallFailed::InternalError(
                    cratonvm_types::error::VmError::Runtime(
                        cratonvm_types::error::RuntimeError::NullPointerException { .. },
                    )
                ))
            )
        };
        let r = native_al_remove_all(&mut ctx, &[recv, nul]);
        assert!(
            is_npe(&r) && r15_npe_message(&r).is_none(),
            "removeAll(null): {r:?}"
        );
        // `checkPositionIndex(5)` on an empty list before `c.toArray()`.
        let r = native_ll_add_all_at(&mut ctx, &[recv, Value::Int(5), nul]);
        assert!(r.is_err() && !is_npe(&r), "addAll(5, null) on []: {r:?}");
        let r = native_ll_add_all_at(&mut ctx, &[recv, Value::Int(0), nul]);
        assert_eq!(
            r15_npe_message(&r).as_deref(),
            Some(NULL_C_TO_ARRAY),
            "{r:?}"
        );
    }

    /// j27 CD16-1 (`CRATONVM_COMPAT_NULL_ARG_MESSAGE_FROM_BYTECODE`, default
    /// ON since 2026-10-28): a null-argument native throws the line its JDK
    /// method's bytecode yields when the query answers, and keeps the
    /// transcribed literal when it declines. Nothing here sets the switch.
    #[test]
    fn j27_null_argument_line_is_asked_of_the_jdk_bytecode_by_default() {
        let asked = "Cannot invoke \"java.util.Collection.toArray()\" because \"src\" is null";
        let mut ctx = lbq_blocking_tests::MockCtx::new(1).with_null_arg_answer(asked);
        let recv = Value::Object(Some(ctx.alloc_object_of(ClassId::new(4252), 8)));
        let nul = Value::Object(None);
        type Body = fn(&mut dyn NativeContext, &[Value]) -> MethodCallResult;
        let rows: [(&str, Body); 4] = [
            ("ArrayList.addAll", native_al_add_all),
            ("ArrayDeque(c)", native_ad_init_from_collection),
            ("HashSet(c)", native_hs_init_from_collection),
            ("LinkedList.toArray(T[])", native_ll_to_array_typed),
        ];
        for (name, body) in rows {
            let r = body(&mut ctx, &[recv, nul]);
            assert_eq!(r15_npe_message(&r).as_deref(), Some(asked), "{name}: {r:?}");
        }
        // The query declines: the literal answers.
        let mut plain = lbq_blocking_tests::MockCtx::new(1);
        let recv = Value::Object(Some(plain.alloc_object_of(ClassId::new(4253), 8)));
        let r = native_al_add_all(&mut plain, &[recv, nul]);
        assert_eq!(r15_npe_message(&r).as_deref(), Some(NULL_C_TO_ARRAY));
    }

    /// Round 15 wave 5, lane compat6: the residual RULE C sites of
    /// `r15w4-compat5-null-argument-message-residuals-FIXED-20261001.md`, each with
    /// the line HotSpot 25 printed (`R15Compat5NullArgMore` rows 0, 1, 3, 5,
    /// 9), the typed `toArray(null)` natives that answered a fresh array, and
    /// `TreeMap$KeySet.addAll(null)` (the JDK's `for (E e : c)`, not `add`'s
    /// UOE). `TreeSet.addAll(null)` is left to the probe (rows 2-3): its
    /// message follows the receiver's size in the process-wide `TreeSet`
    /// side-table, which a concurrent test's mock address could alias.
    #[test]
    fn r15_compat6_null_argument_residuals_name_the_dereference() {
        let mut ctx = lbq_blocking_tests::MockCtx::new(1);
        let recv = Value::Object(Some(ctx.alloc_object_of(ClassId::new(4251), 8)));
        let nul = Value::Object(None);
        type Body = fn(&mut dyn NativeContext, &[Value]) -> MethodCallResult;
        let rows: [(&str, Body, &str); 7] = [
            (
                "PriorityQueue(c)",
                native_pq_init_from_collection,
                NULL_C_TO_ARRAY,
            ),
            ("TreeSet(c)", native_ts_init_collection, NULL_C_SIZE),
            (
                "LinkedList.toArray(T[])",
                native_ll_to_array_typed,
                NULL_A_LENGTH,
            ),
            (
                "ArrayList-shaped toArray(T[])",
                native_al_to_array_typed,
                NULL_A_LENGTH,
            ),
            (
                "KeySetView.toArray(T[])",
                native_ksv_to_array_typed,
                NULL_A_LENGTH,
            ),
            (
                "TreeMap(SortedMap)",
                native_tm_init_from_sorted_map,
                NULL_M_COMPARATOR,
            ),
            (
                "TreeMap$KeySet.addAll",
                native_tree_key_set_add_all,
                NULL_C_ITERATOR,
            ),
        ];
        for (name, body, want) in rows {
            let r = body(&mut ctx, &[recv, nul]);
            assert_eq!(r15_npe_message(&r).as_deref(), Some(want), "{name}: {r:?}");
        }
    }

    /// Round 15 wave 7 (lane sweepC hand-back; `R15SweepCNullArgViews` row 4):
    /// a `TreeSet` range view of a `TreeSet` reads its source LIVE. Before,
    /// `size()` kept the creation-time snapshot (2) after both in-range
    /// elements were removed from the source, so `addAll(null)` asked
    /// `c.iterator()` where JDK 25 asks `c.size()`.
    #[test]
    fn r15_sweepc_ts_range_view_of_a_set_reads_the_source_live() {
        let mut ctx = lbq_blocking_tests::MockCtx::new(1);
        let ts_cid = ClassId::new(4254);
        ctx.define_class(ts_cid, "java/util/TreeSet");
        let s = |ctx: &lbq_blocking_tests::MockCtx, t: &str| {
            Value::Object(Some(
                ctx.define_string(&t.encode_utf16().collect::<Vec<u16>>()),
            ))
        };
        let (a, b, c) = (s(&ctx, "a"), s(&ctx, "b"), s(&ctx, "c"));
        // The source: {a, b, c}, natural order.
        let source = ctx.alloc_object_of(ts_cid, 3);
        let src_data = alloc_ref_array(&mut ctx, TS_DEFAULT_CAPACITY);
        for (i, v) in [a, b, c].into_iter().enumerate() {
            ctx.set_array_element(src_data, i, v);
        }
        ts_set_slot(
            &mut ctx,
            source,
            TS_FIELD_DATA,
            Value::Object(Some(src_data)),
        );
        ts_set_slot(&mut ctx, source, TS_FIELD_SIZE, Value::Int(3));
        ts_set_slot(&mut ctx, source, TS_FIELD_COMPARATOR, Value::Object(None));
        // The view `subSet(a, true, c, false)` as `ts_install_range_view`
        // leaves it: {a, b}, the source marker in the last slot, the range
        // in `tm_view_table`.
        let view = ctx.alloc_object_of(ts_cid, 3);
        let view_data = alloc_ref_array(&mut ctx, TS_DEFAULT_CAPACITY + 1);
        ctx.set_array_element(view_data, 0, a);
        ctx.set_array_element(view_data, 1, b);
        ctx.set_array_element(view_data, TS_DEFAULT_CAPACITY, Value::Object(Some(source)));
        ts_set_slot(
            &mut ctx,
            view,
            TS_FIELD_DATA,
            Value::Object(Some(view_data)),
        );
        ts_set_slot(&mut ctx, view, TS_FIELD_SIZE, Value::Int(2));
        ts_set_slot(&mut ctx, view, TS_FIELD_COMPARATOR, Value::Object(None));
        let spec = TmViewSpec {
            source,
            lo: a,
            lo_bounded: true,
            lo_inclusive: true,
            hi: c,
            hi_bounded: true,
            hi_inclusive: false,
            descending: false,
        };
        tm_register_view(&ctx, view, spec);
        let size = |ctx: &mut lbq_blocking_tests::MockCtx| {
            native_ts_size(ctx, &[Value::Object(Some(view))]).unwrap()
        };
        assert_eq!(size(&mut ctx), Some(Value::Int(2)));
        // `ts.remove(a); ts.remove(b)` through the SOURCE: {c}.
        ctx.set_array_element(src_data, 0, c);
        ctx.set_array_element(src_data, 1, Value::Object(None));
        ctx.set_array_element(src_data, 2, Value::Object(None));
        ts_set_slot(&mut ctx, source, TS_FIELD_SIZE, Value::Int(1));
        assert_eq!(size(&mut ctx), Some(Value::Int(0)));
        // The view still names its source (write-through survives a rebuild).
        assert_eq!(ts_view_source(&ctx, view), Some(source));
        let r = native_ts_add_all(&mut ctx, &[Value::Object(Some(view)), Value::Object(None)]);
        assert_eq!(r15_npe_message(&r).as_deref(), Some(NULL_C_SIZE));
    }

    /// Round 15 wave 6 (lane sweepC; `r15w4-compat4-bulk-ops-residuals` item
    /// 2): a bulk removal compacts in place from `head`, as JDK 25's
    /// `bulkRemoveModified` does, and clears the vacated slots up to the old
    /// `tail`; the rebuild reset `head` to 0.
    #[test]
    fn r15_sweepc_deque_bulk_removal_keeps_head() {
        let mut ctx = lbq_blocking_tests::MockCtx::new(1);
        let dq = ctx.alloc_object_of(ClassId::new(4252), 3);
        native_ad_init(&mut ctx, &[Value::Object(Some(dq))]).unwrap();
        let e: Vec<Value> = (0..5)
            .map(|_| Value::Object(Some(ctx.alloc_object_of(ClassId::new(4253), 1))))
            .collect();
        for v in &e {
            native_ad_add_last(&mut ctx, &[Value::Object(Some(dq)), *v]).unwrap();
        }
        native_ad_remove_first(&mut ctx, &[Value::Object(Some(dq))]).unwrap();
        // e1..e4 at slots 1..=4, head 1, tail 5.
        ad_rebuild(&mut ctx, dq, &[e[2], e[4]]).unwrap();
        let (data, head, tail, size) = ad_state(&ctx, dq);
        assert_eq!((head, tail, size), (1, 3, 2));
        let buf = data.expect("the ring buffer");
        assert_eq!(ctx.get_array_element(buf, 1), e[2]);
        assert_eq!(ctx.get_array_element(buf, 2), e[4]);
        assert_eq!(ctx.get_array_element(buf, 3), Value::Object(None));
        assert_eq!(ctx.get_array_element(buf, 4), Value::Object(None));
        // Everything removed: `tail = head`, not 0.
        ad_rebuild(&mut ctx, dq, &[]).unwrap();
        assert_eq!(ad_state(&ctx, dq).1, 1);
        assert_eq!(ad_state(&ctx, dq).2, 1);
        assert_eq!(ctx.get_array_element(buf, 1), Value::Object(None));
    }

    /// C4W4-1: `putMapEntries`' table for `s` entries at the default factor
    /// -- 2 buckets for one, 4 for 2-3, 8 for 4-6, 16 for 7-12, 32 from 13 --
    /// and `HashSet(c)`'s `max(s, 12)` floor keeps 16 for a small source.
    #[test]
    fn r15_compat5_copy_presize_matches_put_map_entries() {
        let table = |s: i32| hashmap_table_size_for(putmap_entries_target(s, 0.75));
        let got: Vec<i32> = [1, 2, 3, 4, 6, 7, 12, 13, 24, 25]
            .iter()
            .map(|&s| table(s))
            .collect();
        assert_eq!(got, vec![2, 4, 4, 8, 8, 16, 16, 32, 32, 64]);
        assert_eq!(table(std::cmp::max(1, 12)), 16);
        assert_eq!(
            table(20),
            32,
            "a 20-element list with repeats sizes 32 buckets"
        );
        assert_eq!(
            putmap_entries_target(i32::MAX, 0.75),
            JDK_HASHMAP_MAXIMUM_CAPACITY
        );
        assert_eq!(putmap_entries_target(3, 0.5), 6);
    }

    /// C4-3: every empty factory answers `ImmutableCollections.EMPTY_*` (one
    /// object, so `List.of() == List.of()`); without the static, or with a
    /// static of the wrong class, the native keeps minting its wrapper.
    #[test]
    fn r15_compat_empty_factories_answer_the_jdk_singleton() {
        // `wrong_map`: `EMPTY_MAP` holds an object of the wrong class.
        fn setup(
            wrong_map: bool,
        ) -> (lbq_blocking_tests::MockCtx, ObjectRef, ObjectRef, ObjectRef) {
            let ctx = lbq_blocking_tests::MockCtx::new(1);
            let (list_n, set_n, map_n) =
                (ClassId::new(4301), ClassId::new(4302), ClassId::new(4303));
            ctx.define_class(list_n, "java/util/ImmutableCollections$ListN");
            ctx.define_class(set_n, "java/util/ImmutableCollections$SetN");
            ctx.define_class(map_n, "java/util/ImmutableCollections$MapN");
            let el = ctx.alloc_object_of(list_n, 2);
            let es = ctx.alloc_object_of(set_n, 2);
            let em = ctx.alloc_object_of(map_n, 2);
            // This mock's `ensure_class_initialized` answers `ClassId(0)`.
            let ic = ClassId::new(0);
            ctx.define_static(ic, "EMPTY_LIST", Value::Object(Some(el)));
            ctx.define_static(ic, "EMPTY_SET", Value::Object(Some(es)));
            let map = if wrong_map { el } else { em };
            ctx.define_static(ic, "EMPTY_MAP", Value::Object(Some(map)));
            (ctx, el, es, em)
        }
        // No `ImmutableCollections` statics: the synthetic JDK / mid-`<clinit>`.
        let mut bare = lbq_blocking_tests::MockCtx::new(1);
        assert!(canonical_empty_immutable(&mut bare, UNMOD_LIST_CLASS).is_none());
        // A static of the wrong class is refused.
        let (mut ctx, _, _, _) = setup(true);
        assert!(canonical_empty_immutable(&mut ctx, UNMOD_MAP_CLASS).is_none());

        let (mut ctx, el, es, em) = setup(false);
        assert_eq!(r15_obj(native_list_of_0(&mut ctx, &[])), Some(el));
        assert_eq!(r15_obj(native_set_of_0(&mut ctx, &[])), Some(es));
        assert_eq!(r15_obj(native_map_of_0(&mut ctx, &[])), Some(em));
        // `List.of(E...)` / `Set.of(E...)` / `Map.ofEntries(..)` of a
        // zero-length array are the same object on the JDK.
        let empty = alloc_ref_array(&mut ctx, 0);
        let arr = [Value::Object(Some(empty))];
        assert_eq!(r15_obj(native_list_of_array(&mut ctx, &arr)), Some(el));
        assert_eq!(r15_obj(native_set_of_array(&mut ctx, &arr)), Some(es));
        assert_eq!(r15_obj(native_map_of_entries(&mut ctx, &arr)), Some(em));
    }

    // -----------------------------------------------------------------------
    // Round 15 wave 3 (lane compat2): the JDK constructors' one ordered walk
    // (`CRATONVM_COMPAT_IMMUTABLE_OF_JDK_WALK`, default on; nothing here sets
    // it). Every row fails before any build, so the mock's missing Java
    // dispatch is never reached: the elements are modelled Strings, which the
    // walk hashes and compares natively.
    // -----------------------------------------------------------------------

    fn r15w3_str(ctx: &lbq_blocking_tests::MockCtx, s: &str) -> Value {
        let units: Vec<u16> = s.encode_utf16().collect();
        Value::Object(Some(ctx.define_string(&units)))
    }

    /// `Some(message)` for an NPE (`Some(None)`: a bare one), `None` for
    /// anything else.
    fn r15w3_npe(r: &MethodCallResult) -> Option<Option<String>> {
        match r {
            Err(MethodCallFailed::InternalError(cratonvm_types::error::VmError::Runtime(
                cratonvm_types::error::RuntimeError::NullPointerException { message },
            ))) => Some(message.clone()),
            _ => None,
        }
    }

    fn r15w3_iae(r: &MethodCallResult) -> Option<String> {
        match r {
            Err(MethodCallFailed::InternalError(cratonvm_types::error::VmError::Runtime(
                cratonvm_types::error::RuntimeError::IllegalArgumentException { message },
            ))) => Some(message.clone()),
            _ => None,
        }
    }

    /// `SetN` / `MapN` check each element (pair) in order, so a duplicate
    /// ahead of a null is the IAE. The two passes answered the NPE.
    #[test]
    fn r15w3_compat2_a_duplicate_ahead_of_a_null_is_the_iae() {
        let mut ctx = lbq_blocking_tests::MockCtx::new(1);
        let null = Value::Object(None);
        let (a1, a2) = (r15w3_str(&ctx, "a"), r15w3_str(&ctx, "a"));
        let r = native_set_of_varargs(&mut ctx, &[a1, a2, null]);
        assert_eq!(r15w3_iae(&r).as_deref(), Some("duplicate element: a"));

        let (k1, k2, n) = (
            r15w3_str(&ctx, "k"),
            r15w3_str(&ctx, "k"),
            r15w3_str(&ctx, "n"),
        );
        let (v1, v2) = (r15w3_str(&ctx, "1"), r15w3_str(&ctx, "2"));
        let r = native_map_of_varargs(&mut ctx, &[k1, v1, k2, v2, n, null]);
        assert_eq!(r15w3_iae(&r).as_deref(), Some("duplicate key: k"));

        // A null AHEAD of the duplicate is still the NPE (`SetN`'s `"pe"`).
        let (b1, b2) = (r15w3_str(&ctx, "b"), r15w3_str(&ctx, "b"));
        let r = native_set_of_varargs(&mut ctx, &[null, b1, b2]);
        assert_eq!(
            r15w3_npe(&r),
            Some(Some(
                "Cannot invoke \"Object.hashCode()\" because \"pe\" is null".to_string()
            ))
        );
    }

    /// `Set.of`'s NPE message follows the arity: `Set12(e0)` and `Set12(.., e1)`
    /// are `requireNonNull` (bare), `Set12(e0, ..)` dereferences `e0`, `SetN`
    /// dereferences `pe`. `Map1` is bare. The natives were always bare.
    #[test]
    fn r15w3_compat2_set_of_npe_messages_follow_the_arity() {
        let mut ctx = lbq_blocking_tests::MockCtx::new(1);
        let null = Value::Object(None);
        let a = r15w3_str(&ctx, "a");
        let b = r15w3_str(&ctx, "b");
        assert_eq!(r15w3_npe(&native_set_of_1(&mut ctx, &[null])), Some(None));
        assert_eq!(
            r15w3_npe(&native_set_of_2(&mut ctx, &[a, null])),
            Some(None)
        );
        assert_eq!(
            r15w3_npe(&native_set_of_2(&mut ctx, &[null, a])),
            Some(Some(
                "Cannot invoke \"Object.equals(Object)\" because \"e0\" is null".to_string()
            ))
        );
        assert_eq!(
            r15w3_npe(&native_set_of_varargs(&mut ctx, &[a, b, null])),
            Some(Some(
                "Cannot invoke \"Object.hashCode()\" because \"pe\" is null".to_string()
            ))
        );
        assert_eq!(
            r15w3_npe(&native_map_of_1(&mut ctx, &[a, null])),
            Some(None)
        );
        // `Set12(e0, e1)` compares with `equals`, not by hash bucket.
        let a2 = r15w3_str(&ctx, "a");
        assert_eq!(
            r15w3_iae(&native_set_of_2(&mut ctx, &[a, a2])).as_deref(),
            Some("duplicate element: a")
        );
    }

    // -----------------------------------------------------------------------
    // Round 15 wave 3 (lane compat3): C2W3-2, copy constructors ask a foreign
    // source the JDK's questions (`CRATONVM_COMPAT_COPY_CTOR_JDK_QUESTIONS`,
    // default on; nothing here sets it).
    // -----------------------------------------------------------------------

    /// A program's class is foreign; the JDK's and this VM's are not, and a
    /// JDK wrapper is foreign only through what it wraps.
    #[test]
    fn r15w3_compat3_copy_source_foreign_by_class() {
        let ctx = lbq_blocking_tests::MockCtx::new(1);
        let user = ClassId::new(9_101);
        ctx.define_class(user, "com/example/CountingList");
        let jdk = ClassId::new(9_102);
        ctx.define_class(jdk, "java/util/ArrayList");
        let own = ClassId::new(9_103);
        ctx.define_class(own, "cratonvm/internal/UnmodifiableList");
        let wrapper = ClassId::new(9_104);
        ctx.define_class(wrapper, "java/util/Collections$UnmodifiableCollection");
        let u = ctx.alloc_object_of(user, 2);
        let a = ctx.alloc_object_of(jdk, 3);
        let o = ctx.alloc_object_of(own, 1);
        let w = ctx.alloc_object_of(wrapper, 1);
        assert!(copy_source_is_foreign(&ctx, u));
        assert!(!copy_source_is_foreign(&ctx, a));
        assert!(!copy_source_is_foreign(&ctx, o));
        // This mock answers no field by name: a wrapper around nothing
        // readable is the JDK's own.
        assert!(!copy_source_is_foreign(&ctx, w));
    }

    /// `new ArrayList<>(foreign)` asks `toArray()` and nothing else, so a
    /// null answer is the JDK's NPE on `a.length`. The mock's `toArray()`
    /// answers nothing; the probes-then-`size()` path answered an empty list.
    #[test]
    fn r15w3_compat3_array_list_copy_asks_foreign_to_array_only() {
        let mut ctx = lbq_blocking_tests::MockCtx::new(1);
        let user = ClassId::new(9_111);
        ctx.define_class(user, "com/example/CountingList");
        let al = ClassId::new(9_112);
        ctx.define_class(al, "java/util/ArrayList");
        let this = ctx.alloc_object_of(al, 3);
        let source = ctx.alloc_object_of(user, 3);
        let r = native_al_init_from_collection(
            &mut ctx,
            &[Value::Object(Some(this)), Value::Object(Some(source))],
        );
        assert_eq!(
            r15w3_npe(&r),
            Some(Some(
                "Cannot read the array length because \"a\" is null".to_string()
            ))
        );
    }

    // -----------------------------------------------------------------------
    // Round 15 wave 4 (lane compat4): bulk operations and map copies ask a
    // PROGRAM's argument the JDK's questions
    // (`CRATONVM_COMPAT_BULK_ADD_JDK_QUESTIONS`,
    // `CRATONVM_COMPAT_MAP_COPY_JDK_QUESTIONS`, default on; nothing here sets
    // them). This mock's `invoke_virtual` answers nothing, so the first
    // question the JDK asks surfaces as the NPE its null answer causes -- the
    // old snapshot path read the same null answers as "empty" and returned.
    // -----------------------------------------------------------------------

    /// A program's map is copied through its own questions; the JDK's is not.
    #[test]
    fn r15w4_compat4_foreign_map_source_by_class() {
        let ctx = lbq_blocking_tests::MockCtx::new(1);
        let user = ClassId::new(9_201);
        ctx.define_class(user, "com/example/CountingMap");
        let jdk = ClassId::new(9_202);
        ctx.define_class(jdk, "java/util/HashMap");
        let u = ctx.alloc_object_of(user, 2);
        let h = ctx.alloc_object_of(jdk, 3);
        assert_eq!(
            foreign_map_source(&ctx, u).map(|o| o.as_ptr() as usize),
            Some(u.as_ptr() as usize)
        );
        assert!(foreign_map_source(&ctx, h).is_none());
    }

    /// `Map.copyOf(programMap)` asks `entrySet().toArray(new Entry[0])`: a
    /// null `entrySet()` is the NPE of that `toArray` call. The old path asked
    /// `isEmpty()` again, read the null answer as "empty" and returned a map.
    #[test]
    fn r15w4_compat4_map_copy_of_asks_foreign_entry_set_to_array() {
        let mut ctx = lbq_blocking_tests::MockCtx::new(1);
        let user = ClassId::new(9_211);
        ctx.define_class(user, "com/example/CountingMap");
        let m = ctx.alloc_object_of(user, 2);
        let r = native_map_copy_of(&mut ctx, &[Value::Object(Some(m))]);
        assert_eq!(
            r15w3_npe(&r),
            Some(Some(
                "Cannot invoke \"java.util.Set.toArray(Object[])\" because the return value of \
                 \"java.util.Map.entrySet()\" is null"
                    .to_string()
            ))
        );
    }

    /// `linkedList.addAll(programCollection)` asks `c.toArray()` once, so its
    /// null answer is the NPE on `a.length`; the old path answered `false`.
    #[test]
    fn r15w4_compat4_linked_list_add_all_asks_foreign_to_array() {
        let mut ctx = lbq_blocking_tests::MockCtx::new(1);
        let user = ClassId::new(9_221);
        ctx.define_class(user, "com/example/CountingList");
        let ll = ClassId::new(9_222);
        ctx.define_class(ll, "java/util/LinkedList");
        let this = ctx.alloc_object_of(ll, 4);
        let source = ctx.alloc_object_of(user, 2);
        let r = native_ll_add_all(
            &mut ctx,
            &[Value::Object(Some(this)), Value::Object(Some(source))],
        );
        assert_eq!(
            r15w3_npe(&r),
            Some(Some(
                "Cannot read the array length because \"a\" is null".to_string()
            ))
        );
    }

    /// `hashSet.addAll(programCollection)` walks `c.iterator()`: a null
    /// iterator is the NPE of its `hasNext()`. The old path answered `false`.
    #[test]
    fn r15w4_compat4_hash_set_add_all_walks_foreign_iterator() {
        let mut ctx = lbq_blocking_tests::MockCtx::new(1);
        let user = ClassId::new(9_231);
        ctx.define_class(user, "com/example/CountingSet");
        let hs = ClassId::new(9_232);
        ctx.define_class(hs, "java/util/HashSet");
        let this = ctx.alloc_object_of(hs, 2);
        let source = ctx.alloc_object_of(user, 2);
        let r = native_hs_add_all(
            &mut ctx,
            &[Value::Object(Some(this)), Value::Object(Some(source))],
        );
        assert_eq!(r15w3_npe(&r), Some(None));
    }

    /// `arrayDeque.addAll(programCollection)` asks `c.size()`, then walks
    /// `c`'s iterator (`forEach`'s default body): a null iterator is the NPE
    /// of its `hasNext()`. The old path answered `false`.
    #[test]
    fn r15w4_compat4_array_deque_add_all_walks_foreign_iterator() {
        let mut ctx = lbq_blocking_tests::MockCtx::new(1);
        let user = ClassId::new(9_241);
        ctx.define_class(user, "com/example/CountingQueue");
        let ad = ClassId::new(9_242);
        ctx.define_class(ad, "java/util/ArrayDeque");
        let this = ctx.alloc_object_of(ad, 3);
        let source = ctx.alloc_object_of(user, 2);
        let r = native_ad_add_all(
            &mut ctx,
            &[Value::Object(Some(this)), Value::Object(Some(source))],
        );
        assert_eq!(r15w3_npe(&r), Some(None));
    }

    /// `Collections.unmodifiableList(x).get(i)` bounds-checked against
    /// `al_state`'s SIZE, which is 0 both for a genuinely empty array-backed
    /// list and for a backing it could not read at all -- so a view over a
    /// LinkedList or an `Arrays$ArrayList` threw AIOOBE on a VALID index while
    /// iterating fine. The DATA slot is the readability signal, not the size.
    /// That destroyed the real failure inside Spring's
    /// `SummaryGeneratingListener.getFailures().get(0)` and failed both
    /// `AotIntegrationTests` end-to-end tests.
    #[test]
    fn unmod_view_size_stands_aside_when_al_state_cannot_read_the_backing() {
        // A readable array-backed list: pre-check with the size it reported.
        let data = Some(unsafe { ObjectRef::from_raw(0x1000 as *mut u8) });
        assert_eq!(unmod_view_size(data, 3), Some(3));
        // Readable AND genuinely empty -- still a pre-check, so `List.of()`
        // keeps throwing the AIOOBE that HotSpot throws for it.
        assert_eq!(unmod_view_size(data, 0), Some(0));
        // THE BUG: no data slot means al_state could not read the backing, and
        // its 0 is "cannot tell", not "empty". Pre-check must stand aside so
        // the delegate answers -- returning Some(0) here is what rejected
        // `unmodifiableList(new LinkedList<>(List.of("x","y"))).get(0)`.
        assert_eq!(unmod_view_size(None, 0), None);
        assert_eq!(unmod_view_size(None, 7), None);
        // A negative size can never widen the bounds.
        assert_eq!(unmod_view_size(data, -5), Some(0));
    }

    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    /// THE regression test for the values-view fail-fast door.
    ///
    /// The first attempt at that door compared the iterator's raw
    /// `expectedModCount` against the source generation. It made every
    /// measured cell throw -- and then made every Spring Boot class die inside
    /// JUnit discovery with a SPURIOUS `ConcurrentModificationException`,
    /// because an iterator minted on a path that never seeded read **0** and
    /// `0` is a legal generation. The unseeded state has to be REPRESENTABLE
    /// and it has to fail OPEN, which is the whole reason the stamp is biased.
    #[test]
    fn an_unseeded_values_view_iterator_never_reports_a_comodification() {
        // Nothing wrote the slot. Whatever the source is doing, no throw.
        assert!(!view_comod_is_stale(0, 0));
        assert!(!view_comod_is_stale(0, 5));
        assert!(!view_comod_is_stale(0, i32::MAX));
        assert!(!view_comod_is_stale(0, -1));
    }

    /// A stamped iterator agrees with its own generation and disagrees with
    /// every other one -- including generation 0, the value the raw encoding
    /// could not tell apart from "unseeded".
    #[test]
    fn a_stamped_values_view_iterator_tracks_exactly_its_own_generation() {
        for gen in [0i32, 1, 2, 7, 1_000_000, i32::MAX - 1] {
            let stamp = view_comod_stamp(gen);
            assert_ne!(stamp, 0, "gen {gen} must not encode as the unseeded 0");
            assert!(!view_comod_is_stale(stamp, gen), "gen {gen} is its own");
            assert!(view_comod_is_stale(stamp, gen.wrapping_add(1)));
            assert!(view_comod_is_stale(stamp, gen.wrapping_sub(1)));
        }
    }

    /// The one generation the bias cannot carry is `-1`, which stamps to the
    /// unseeded `0`. No `bump_map_mod_count` path produces it, and losing a
    /// check is the direction this whole door is allowed to fail in -- but it
    /// is stated here rather than left to be rediscovered.
    #[test]
    fn generation_minus_one_degrades_to_no_check_rather_than_to_a_wrong_one() {
        assert_eq!(view_comod_stamp(-1), 0);
        assert!(!view_comod_is_stale(view_comod_stamp(-1), 99));
    }

    // Unit tests for helper functions only.
    // Integration tests are in vm.rs since they need the full VM.

    // --- al_slots_for memo -------------------------------------------------
    //
    // `al_slots_for` is on the path of EVERY `native_al_*` operation and used
    // to re-derive a constant each call (a string-keyed `class_id_by_name`, an
    // `is_subclass` walk, two string-keyed `resolve_field_index` lookups, all
    // behind the class-manager lock). It is now memoized per receiver ClassId.
    // A stale or cross-wired entry reads the WRONG field slots on every
    // collection operation in the VM, so the memo is covered directly.

    fn memo_ctx() -> (lbq_blocking_tests::MockCtx, ClassId, ClassId) {
        let ctx = lbq_blocking_tests::MockCtx::new(1);
        let al = ClassId::new(4001);
        let vec = ClassId::new(4002);
        ctx.define_class(al, "java/util/ArrayList");
        ctx.define_class(vec, "java/util/Vector");
        ctx.define_field(al, "elementData", 4);
        ctx.define_field(al, "size", 6);
        ctx.define_field(vec, "elementData", 2);
        ctx.define_field(vec, "elementCount", 3);
        (ctx, al, vec)
    }

    #[test]
    fn al_slots_memo_matches_the_uncached_path_and_is_stable() {
        let (ctx, al, _vec) = memo_ctx();
        let list = ctx.alloc_object_of(al, 8);

        let uncached = super::al_slots_for_uncached(&ctx, al);
        let first = super::al_slots_and_layout_for(&ctx, list);
        assert_eq!(
            first,
            super::al_slots_and_layout_for(&ctx, list),
            "memo must be stable"
        );
        assert_eq!(first, super::al_slots_and_layout_for(&ctx, list));
        if let Some(u) = uncached {
            assert_eq!(first, u, "memo must match the uncached resolution");
        }
    }

    /// The memo carries the layout VERDICT beside the slots now, so a stale or
    /// cross-wired entry can answer "not a list" for a real list — which
    /// `al_state` turns into a silently EMPTY collection rather than an error.
    /// Pin the verdict per receiver class explicitly.
    #[test]
    fn al_layout_verdict_is_per_receiver_class() {
        let (ctx, al, vec) = memo_ctx();
        let list = ctx.alloc_object_of(al, 8);
        let vector = ctx.alloc_object_of(vec, 8);

        assert_eq!(
            super::al_slots_and_layout_for(&ctx, list).1,
            super::AlLayout::ArrayList
        );
        assert_eq!(
            super::al_slots_and_layout_for(&ctx, vector).1,
            super::AlLayout::Vector
        );
        // Both must survive the other being asked for, in both orders.
        assert_eq!(
            super::al_slots_and_layout_for(&ctx, vector).1,
            super::AlLayout::Vector
        );
        assert_eq!(
            super::al_slots_and_layout_for(&ctx, list).1,
            super::AlLayout::ArrayList
        );
        assert!(super::al_is_list_layout(&ctx, list));
        assert!(super::al_is_list_layout(&ctx, vector));
        assert!(super::al_is_arraylist_layout(&ctx, list));
        assert!(
            !super::al_is_arraylist_layout(&ctx, vector),
            "a Vector is list-layout but NOT ArrayList-layout"
        );
    }

    /// A named class that is neither ArrayList nor Vector must be refused, or
    /// `al_state` reads its slots as `elementData`/`size` — the wild read the
    /// guard exists for.
    #[test]
    fn al_layout_verdict_refuses_a_foreign_named_class() {
        let (ctx, _al, _vec) = memo_ctx();
        let foreign = ClassId::new(4003);
        ctx.define_class(foreign, "java/lang/String");
        let s = ctx.alloc_object_of(foreign, 8);
        assert_eq!(
            super::al_slots_and_layout_for(&ctx, s).1,
            super::AlLayout::Foreign
        );
        assert!(!super::al_is_list_layout(&ctx, s));
        assert!(!super::al_is_arraylist_layout(&ctx, s));
    }

    // --- r9w3 collections3: the `al_fast_state` hot path -------------------
    //
    // `native_al_get`/`size`/`isEmpty`/`set`/`add` answer an ordinary
    // `ArrayList` from `al_fast_state` without the wrapper/view routes. A fast
    // path that answered for a receiver the general path would have rerouted is
    // a silent wrong answer, so both halves are pinned: it answers exactly the
    // plain list, and it declines every shape the general path treats
    // specially.

    /// A real-JDK-layout `ArrayList` (`elementData` @4, `size` @6) holding
    /// `elems` in a backing array of length `cap`.
    fn fast_list(
        ctx: &mut lbq_blocking_tests::MockCtx,
        al: ClassId,
        elems: &[Value],
        cap: usize,
    ) -> (ObjectRef, ObjectRef) {
        let list = ctx.alloc_object_of(al, 8);
        let buf = ctx.new_ref_array(ClassId::new(0), cap);
        for (i, v) in elems.iter().enumerate() {
            ctx.set_array_element(buf, i, *v);
        }
        ctx.set_field(list, 4, Value::Object(Some(buf)));
        ctx.set_field(list, 6, Value::Int(elems.len() as i32));
        (list, buf)
    }

    #[test]
    fn al_fast_state_answers_a_plain_arraylist_like_al_state() {
        let (mut ctx, al, _vec) = memo_ctx();
        let a = ctx.alloc_object_of(al, 1);
        let b = ctx.alloc_object_of(al, 1);
        let (list, buf) = fast_list(
            &mut ctx,
            al,
            &[Value::Object(Some(a)), Value::Object(Some(b))],
            10,
        );
        assert_eq!(super::al_fast_state(&ctx, list), Some((buf, 2, 10)));
        assert_eq!(super::al_state(&ctx, list), (Some(buf), 2));

        let get = |ctx: &mut lbq_blocking_tests::MockCtx, i: i32| {
            super::native_al_get(ctx, &[Value::Object(Some(list)), Value::Int(i)])
        };
        assert_eq!(get(&mut ctx, 1).unwrap(), Some(Value::Object(Some(b))));
        assert_eq!(get(&mut ctx, 0).unwrap(), Some(Value::Object(Some(a))));
        // Out of range still throws, through the general path's own wording.
        assert!(get(&mut ctx, 2).is_err());
        assert!(get(&mut ctx, -1).is_err());
        // The JIT direct probes answer the same, and decline out of range.
        assert_eq!(
            super::jit_arraylist_get(&ctx, list, 1),
            Some(Value::Object(Some(b)))
        );
        assert_eq!(super::jit_arraylist_get(&ctx, list, 2), None);
        assert_eq!(super::jit_arraylist_get(&ctx, list, -1), None);
        assert_eq!(super::jit_arraylist_size(&ctx, list), Some(2));

        let recv = [Value::Object(Some(list))];
        assert_eq!(
            super::native_al_size(&mut ctx, &recv).unwrap(),
            Some(Value::Int(2))
        );
        assert_eq!(
            super::native_al_is_empty(&mut ctx, &recv).unwrap(),
            Some(Value::Int(0))
        );

        // set: returns the old element and writes the new one in place.
        let old = super::native_al_set(
            &mut ctx,
            &[
                Value::Object(Some(list)),
                Value::Int(0),
                Value::Object(Some(b)),
            ],
        )
        .unwrap();
        assert_eq!(old, Some(Value::Object(Some(a))));
        assert_eq!(ctx.get_array_element(buf, 0), Value::Object(Some(b)));

        // add with spare capacity: appends into the same array, size moves.
        let add_args = [Value::Object(Some(list)), Value::Object(Some(a))];
        let added = super::native_al_add(&mut ctx, &add_args).unwrap();
        assert_eq!(added, Some(Value::Int(1)));
        assert_eq!(ctx.get_field(list, 4), Value::Object(Some(buf)));
        assert_eq!(ctx.get_field(list, 6), Value::Int(3));
        assert_eq!(ctx.get_array_element(buf, 2), Value::Object(Some(a)));
        assert_eq!(super::al_fast_state(&ctx, list), Some((buf, 3, 10)));
    }

    #[test]
    fn al_fast_add_on_a_full_backing_array_still_grows() {
        let (mut ctx, al, _vec) = memo_ctx();
        let a = ctx.alloc_object_of(al, 1);
        let (list, buf) = fast_list(&mut ctx, al, &[Value::Object(Some(a))], 1);
        assert_eq!(super::al_fast_state(&ctx, list), Some((buf, 1, 1)));
        let add_args = [Value::Object(Some(list)), Value::Object(None)];
        super::native_al_add(&mut ctx, &add_args).unwrap();
        assert_eq!(ctx.get_field(list, 6), Value::Int(2));
        match ctx.get_field(list, 4) {
            Value::Object(Some(grown)) => {
                assert_ne!(grown, buf, "a full array must be replaced, not overrun");
                assert!(ctx.array_length(grown) >= 2);
                assert_eq!(ctx.get_array_element(grown, 0), Value::Object(Some(a)));
            }
            other => panic!("elementData lost on grow: {other:?}"),
        }
    }

    #[test]
    fn al_fast_state_declines_every_shape_the_general_path_special_cases() {
        let (mut ctx, al, vec) = memo_ctx();
        let x = ctx.alloc_object_of(al, 1);

        // A values-view carrier: trailing capacity slot names a source map.
        let (view, buf) = fast_list(&mut ctx, al, &[Value::Object(Some(x))], 10);
        ctx.set_array_element(buf, 9, Value::Object(Some(x)));
        assert_eq!(super::al_fast_state(&ctx, view), None);

        // Null `elementData`.
        let (empty, _) = fast_list(&mut ctx, al, &[], 4);
        ctx.set_field(empty, 4, Value::Object(None));
        assert_eq!(super::al_fast_state(&ctx, empty), None);

        // A negative size (corrupt, or a never-written slot) is not answered.
        let (neg, _) = fast_list(&mut ctx, al, &[], 4);
        ctx.set_field(neg, 6, Value::Int(-1));
        assert_eq!(super::al_fast_state(&ctx, neg), None);

        // Too few fields for the resolved slots.
        let short = ctx.alloc_object_of(al, 5);
        assert_eq!(super::al_fast_state(&ctx, short), None);

        // A Vector is list-layout, but not the ArrayList slots.
        let v = ctx.alloc_object_of(vec, 8);
        let vbuf = ctx.new_ref_array(ClassId::new(0), 4);
        ctx.set_field(v, 2, Value::Object(Some(vbuf)));
        ctx.set_field(v, 3, Value::Int(0));
        assert_eq!(super::al_fast_state(&ctx, v), None);

        // A foreign named class.
        let foreign = ClassId::new(4004);
        ctx.define_class(foreign, "java/lang/String");
        let s = ctx.alloc_object_of(foreign, 8);
        assert_eq!(super::al_fast_state(&ctx, s), None);

        // An unmodifiable wrapper class.
        let unmod = ClassId::new(4005);
        ctx.define_class(unmod, "cratonvm/internal/UnmodifiableList");
        let u = ctx.alloc_object_of(unmod, 8);
        assert_eq!(super::al_fast_state(&ctx, u), None);
    }

    /// The per-thread `AbstractList.modCount` memo answers what the resolver
    /// answers, and is scoped per VM.
    #[test]
    fn abstract_list_mod_count_slot_memo_matches_the_resolver_per_vm() {
        let a = lbq_blocking_tests::MockCtx::new(1);
        let al_a = ClassId::new(4201);
        a.define_class(al_a, "java/util/AbstractList");
        a.define_field(al_a, "modCount", 0);
        let b = lbq_blocking_tests::MockCtx::new(2);
        let al_b = ClassId::new(4202);
        b.define_class(al_b, "java/util/AbstractList");
        b.define_field(al_b, "modCount", 3);

        assert_eq!(super::abstract_list_mod_count_slot(&a), Some(0));
        assert_eq!(super::abstract_list_mod_count_slot(&b), Some(3));
        assert_eq!(super::abstract_list_mod_count_slot(&a), Some(0));
        // A VM where the class is not resolvable answers `None`, uncached.
        let c = lbq_blocking_tests::MockCtx::new(3);
        assert_eq!(super::abstract_list_mod_count_slot(&c), None);
        assert_eq!(super::abstract_list_mod_count_slot(&b), Some(3));
    }

    // --- G22-1 N1/N2: the map-view carrier slot collision ------------------
    //
    // MEASURED before this fix (`scratchpad/g22/G22Slot.java`, both VMs,
    // `--add-opens java.base/java.util=ALL-UNNAMED`):
    //
    //     LinkedHashMap$LinkedValues.this$0  HotSpot: java.util.LinkedHashMap
    //                                       CratonVM: [Ljava.lang.Object;[len=11]
    //
    // `LinkedValues` is the ONE carrier of the six that declares two fields
    // (`reversed` @0, `this$0` @1), so ArrayList's absolute `elementData` slot
    // 1 landed on `this$0` and `values_view_class_source` read the element
    // buffer back as the source map.

    /// The mock layout that reproduces the real-JDK one: `AbstractList
    /// .modCount` @0, `ArrayList.elementData` @1, `ArrayList.size` @2.
    fn carrier_ctx() -> (lbq_blocking_tests::MockCtx, ClassId, ClassId, ClassId) {
        let ctx = lbq_blocking_tests::MockCtx::new(11);
        let al = ClassId::new(4101);
        let hm_values = ClassId::new(4102);
        let linked_values = ClassId::new(4103);
        ctx.define_class(al, "java/util/ArrayList");
        ctx.define_class(hm_values, "java/util/HashMap$Values");
        ctx.define_class(linked_values, "java/util/LinkedHashMap$LinkedValues");
        ctx.define_field(al, "elementData", 1);
        ctx.define_field(al, "size", 2);
        // One declared field, `this$0` @0 — five of the six carriers.
        ctx.define_total_fields(hm_values, 1);
        ctx.define_field(hm_values, "this$0", 0);
        // Two declared fields, `reversed` @0 and `this$0` @1 — the sixth.
        ctx.define_total_fields(linked_values, 2);
        ctx.define_field(linked_values, "reversed", 0);
        ctx.define_field(linked_values, "this$0", 1);
        (ctx, al, hm_values, linked_values)
    }

    /// The whole fix in one assertion: `LinkedValues`' list slots must clear
    /// its two declared fields, and the five one-field carriers must keep the
    /// layout they already had — the measured `HashMap` / `TreeMap` /
    /// `Hashtable` / `ConcurrentHashMap` rows were all CLEAN and must not move.
    #[test]
    fn view_carrier_slots_clear_the_carriers_own_declared_fields() {
        let (ctx, al, hm_values, linked_values) = carrier_ctx();

        // The reference: a real ArrayList is untouched.
        assert_eq!(super::al_slots_for_uncached(&ctx, al).unwrap().0, (1, 2, 3));

        // One declared field: identical to ArrayList's absolute slots, which
        // is why this family never diverged.
        assert_eq!(
            super::view_carrier_slots(&ctx, hm_values),
            Some((1, 2, 3)),
            "a one-field carrier keeps ArrayList's own slots"
        );
        // Two declared fields: elementData must NOT land on `this$0` @1.
        assert_eq!(
            super::view_carrier_slots(&ctx, linked_values),
            Some((2, 3, 4)),
            "LinkedValues declares reversed@0 and this$0@1; the list state \
             starts above both"
        );
        let (slots, layout) = super::al_slots_for_uncached(&ctx, linked_values).unwrap();
        assert_eq!(slots, (2, 3, 4));
        assert_eq!(layout, super::AlLayout::ViewCarrier);
        assert_ne!(
            slots.0,
            super::al_slots_resolved(&ctx).unwrap().0,
            "the collision is exactly elementData == this$0's slot"
        );
    }

    /// A carrier still reads and writes like an ArrayList everywhere else —
    /// `accepts`, the ArrayList-layout question, and the wrapper-route
    /// shortcut. A `ViewCarrier` that answered `Foreign` to any of these would
    /// make every values view read back EMPTY.
    #[test]
    fn view_carrier_behaves_as_a_list_everywhere_but_its_slots() {
        let (ctx, _al, hm_values, linked_values) = carrier_ctx();
        let hv = ctx.alloc_object_of(hm_values, 3);
        let lv = ctx.alloc_object_of(linked_values, 4);
        assert!(super::al_is_list_layout(&ctx, hv));
        assert!(super::al_is_list_layout(&ctx, lv));
        assert!(super::al_is_arraylist_layout(&ctx, hv));
        assert!(super::al_is_arraylist_layout(&ctx, lv));
        assert!(super::AlLayout::ViewCarrier.rules_out_wrapper_routes());
        assert!(super::AlLayout::ViewCarrier.accepts(4));
    }

    /// `AbstractList.modCount` resolves to slot 0, which on a carrier is the
    /// carrier's OWN first declared field — `this$0`, a REFERENCE. Bumping a
    /// counter there is a type-punning store; it is why `HashMap$Values
    /// .this$0` reflected as `null`. A carrier must report no modCount slot.
    #[test]
    fn a_view_carrier_has_no_mod_count_slot() {
        let (ctx, al, hm_values, linked_values) = carrier_ctx();
        let abstract_list = ClassId::new(4104);
        ctx.define_class(abstract_list, "java/util/AbstractList");
        ctx.define_field(abstract_list, "modCount", 0);

        let list = ctx.alloc_object_of(al, 3);
        assert_eq!(
            super::al_mod_count_slot(&ctx, list),
            Some(0),
            "an ordinary ArrayList keeps its comodification counter"
        );
        let hv = ctx.alloc_object_of(hm_values, 3);
        let lv = ctx.alloc_object_of(linked_values, 4);
        assert_eq!(super::al_mod_count_slot(&ctx, hv), None);
        assert_eq!(super::al_mod_count_slot(&ctx, lv), None);
    }

    /// `store_view_carrier_backref` writes the carrier's own `this$0`/`map`
    /// and can never reach the list slots — the collision it repairs.
    #[test]
    fn the_backref_lands_on_this_dollar_zero_and_nowhere_else() {
        let (ctx, al, hm_values, linked_values) = carrier_ctx();
        let mut ctx = ctx;
        let source = ctx.alloc_object_of(al, 3);

        let hv = ctx.alloc_object_of(hm_values, 3);
        super::store_view_carrier_backref(&mut ctx, hv, source);
        assert_eq!(ctx.get_field(hv, 0), Value::Object(Some(source)));

        let lv = ctx.alloc_object_of(linked_values, 4);
        super::store_view_carrier_backref(&mut ctx, lv, source);
        assert_eq!(
            ctx.get_field(lv, 1),
            Value::Object(Some(source)),
            "LinkedValues' this$0 is slot 1, not slot 0"
        );
        assert_eq!(
            ctx.get_field(lv, 0),
            Value::Int(0),
            "`reversed` @0 must be left alone"
        );
        assert_eq!(
            ctx.get_field(lv, 2),
            Value::Int(0),
            "the elementData slot must be left alone"
        );

        // An ordinary ArrayList is never touched, whatever it declares.
        let plain = ctx.alloc_object_of(al, 3);
        super::store_view_carrier_backref(&mut ctx, plain, source);
        assert_eq!(ctx.get_field(plain, 0), Value::Int(0));
        assert_eq!(ctx.get_field(plain, 1), Value::Int(0));
    }

    /// C4-1 (round 14 wave 5, lane compat5): `new LinkedValues(true)` is
    /// marked through the carrier's OWN declared `reversed` @0, below its list
    /// slots, and read back from there; no other carrier (and no ArrayList)
    /// is ever written. Before C4-1 nothing read or wrote the field.
    #[test]
    fn c4_1_linked_values_reversed_is_its_declared_field_and_nothing_else() {
        let (ctx, al, hm_values, linked_values) = carrier_ctx();
        let mut ctx = ctx;
        let source = ctx.alloc_object_of(al, 3);

        let lv = ctx.alloc_object_of(linked_values, 4);
        super::store_view_carrier_backref(&mut ctx, lv, source);
        assert!(
            !super::values_view_reversed(&ctx, lv),
            "a fresh carrier is forward"
        );
        assert!(super::mark_values_view_reversed(&mut ctx, lv));
        assert_eq!(ctx.get_field(lv, 0), Value::Int(1), "`reversed` @0 is set");
        assert_eq!(
            ctx.get_field(lv, 1),
            Value::Object(Some(source)),
            "this$0 kept"
        );
        assert_eq!(ctx.get_field(lv, 2), Value::Int(0), "elementData untouched");
        assert!(super::values_view_reversed(&ctx, lv));

        // A one-field carrier's slot 0 is its `this$0`: never written.
        let hv = ctx.alloc_object_of(hm_values, 3);
        super::store_view_carrier_backref(&mut ctx, hv, source);
        assert!(!super::mark_values_view_reversed(&mut ctx, hv));
        assert_eq!(ctx.get_field(hv, 0), Value::Object(Some(source)));
        assert!(!super::values_view_reversed(&ctx, hv));

        let plain = ctx.alloc_object_of(al, 3);
        assert!(!super::mark_values_view_reversed(&mut ctx, plain));
        assert_eq!(ctx.get_field(plain, 0), Value::Int(0));
    }

    #[test]
    fn al_slots_memo_does_not_cross_wire_two_receiver_classes() {
        let (ctx, al, vec) = memo_ctx();
        let list = ctx.alloc_object_of(al, 8);
        let vector = ctx.alloc_object_of(vec, 8);

        let l1 = super::al_slots_for(&ctx, list);
        let v1 = super::al_slots_for(&ctx, vector);
        // Re-ask in the opposite order: a cache without a tag comparison would
        // hand the second class the first one's layout here.
        assert_eq!(super::al_slots_for(&ctx, vector), v1, "Vector layout moved");
        assert_eq!(
            super::al_slots_for(&ctx, list),
            l1,
            "ArrayList layout moved"
        );
        assert_ne!(
            l1, v1,
            "ArrayList and Vector declare different slots; the memo must keep them apart"
        );
    }

    /// An UNRESOLVABLE layout must not be cached: `resolve_field_index` can
    /// fail before the class is loaded and succeed afterwards, so pinning the
    /// constant fallback would freeze the wrong slots for the rest of the run.
    #[test]
    fn al_slots_memo_does_not_cache_the_fallback() {
        let ctx = lbq_blocking_tests::MockCtx::new(2);
        let foreign = ClassId::new(4003);
        ctx.define_class(foreign, "java/nio/charset/Charset");
        let obj = ctx.alloc_object_of(foreign, 3);

        assert!(super::al_slots_for_uncached(&ctx, foreign).is_none());
        let fallback = (
            super::AL_FIELD_DATA,
            super::AL_FIELD_SIZE,
            super::AL_NUM_FIELDS,
        );
        assert_eq!(super::al_slots_for(&ctx, obj), fallback);

        // Now the layout becomes resolvable, exactly as a late class load would
        // make it. A cached fallback would keep answering the old slots.
        ctx.define_class(foreign, "java/util/ArrayList");
        ctx.define_field(foreign, "elementData", 4);
        ctx.define_field(foreign, "size", 6);
        assert_eq!(
            super::al_slots_for(&ctx, obj),
            (4, 6, 7),
            "the fallback was cached, so a later resolution could not win"
        );
    }

    /// `java.util.Random.nextGaussian()` must reproduce the JDK's documented
    /// sequence for a given seed. The expected bit patterns are the first six
    /// values of `new Random(42)` on Temurin jdk-25.0.3+9.
    ///
    /// Regression for the discarded second variate: returning only the first
    /// variate of each pair advances the LCG at twice the specified rate, and
    /// this asserts the exact stream rather than a distribution, so it fails on
    /// the old behaviour at the very first value.
    #[test]
    fn next_gaussian_matches_jdk_seeded_sequence() {
        use super::{rnd_gaussian_pair, LCG_INCREMENT, LCG_MASK, LCG_MULTIPLIER};

        // `new Random(42)` — seed scrambling plus `next(bits)`, verbatim.
        let mut seed = (42i64 ^ LCG_MULTIPLIER) & LCG_MASK;
        let mut next = move |bits: u32| -> i32 {
            seed = seed
                .wrapping_mul(LCG_MULTIPLIER)
                .wrapping_add(LCG_INCREMENT)
                & LCG_MASK;
            (seed >> (48 - bits)) as i32
        };

        // Drain the pairs the way the native does: first, then the cached second.
        let mut got = Vec::new();
        while got.len() < 6 {
            let (a, b) = rnd_gaussian_pair(&mut next);
            got.push(a);
            got.push(b);
        }

        // Compared BY BIT PATTERN. This used to carry a 1e-12 relative
        // tolerance, excused on the grounds that "the JDK's multiplier goes
        // through `StrictMath.log` (fdlibm) while this uses the platform libm,
        // and those may differ in the last ulp" — which was true, and was the
        // defect, not a property to tolerate. The differential probe read that
        // last ulp straight out of `Double.toString`
        // (`1.141905315473055` vs HotSpot's `1.1419053154730547`), so a
        // tolerance that hid it hid a user-visible divergence. `rnd_gaussian_pair`
        // now uses the fdlibm `log` and the whole stream is exact.
        // W7-44-numberformat-enum-and-double-tostring.md.
        let expected: [i64; 6] = [
            4607821503525903750,
            4606456510138157127,
            -4616641179245592382,
            -4615707776640798080,
            4598733263062401967,
            4604341753479877564,
        ];
        for (i, want_bits) in expected.iter().enumerate() {
            let want = f64::from_bits(*want_bits as u64);
            let have = got[i];
            assert_eq!(
                have.to_bits(),
                *want_bits as u64,
                "nextGaussian[{i}] diverged from the JDK's seeded sequence: \
                 got {have:?} ({:#018x}), want {want:?} ({:#018x})",
                have.to_bits(),
                *want_bits as u64
            );
        }
    }

    #[test]
    fn dbg_hmput_matches_env_presence() {
        // The cached helper must report exactly "env var is set" — identical
        // truthiness to the original `env::var(...).is_ok()` check.
        use super::dbg_hmput;
        let expected = cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_HMPUT").is_some();
        assert_eq!(dbg_hmput(), expected);
        // Cached: second call returns the same value.
        assert_eq!(dbg_hmput(), expected);
    }

    #[test]
    fn stream_reduce_generic_accumulator_is_registered() {
        let registry = build_registry();
        assert!(
            registry
                .find(
                    "java/util/stream/Stream",
                    "reduce",
                    "(Ljava/lang/Object;Ljava/util/function/BiFunction;Ljava/util/function/BinaryOperator;)Ljava/lang/Object;",
                )
                .is_some(),
            "generic Stream.reduce must not fall through to the abstract interface declaration"
        );
    }

    // Fix item 4: Java shortest-round-trip Float/Double formatting helpers.
    #[test]
    fn java_double_to_string_matches_java_spec() {
        use super::java_double_to_string;
        // Always has a decimal point (Java prints "1.0", not "1").
        assert_eq!(java_double_to_string(1.0), "1.0");
        assert_eq!(java_double_to_string(0.0), "0.0");
        assert_eq!(java_double_to_string(-2.0), "-2.0");
        // Shortest round-trip fraction.
        assert_eq!(java_double_to_string(0.1), "0.1");
        // Special values use the Java literals (not Rust's inf/NaN spellings).
        assert_eq!(java_double_to_string(f64::INFINITY), "Infinity");
        assert_eq!(java_double_to_string(f64::NEG_INFINITY), "-Infinity");
        assert_eq!(java_double_to_string(f64::NAN), "NaN");
    }

    #[test]
    fn java_float_to_string_matches_java_spec() {
        use super::java_float_to_string;
        assert_eq!(java_float_to_string(1.0), "1.0");
        assert_eq!(java_float_to_string(-2.0), "-2.0");
        assert_eq!(java_float_to_string(f32::INFINITY), "Infinity");
        assert_eq!(java_float_to_string(f32::NEG_INFINITY), "-Infinity");
        assert_eq!(java_float_to_string(f32::NAN), "NaN");
    }

    // `Properties.store` writes each entry as `saveConvert(key)=saveConvert(value)`;
    // `Properties.load` must read it back to the original. This pins the symmetry
    // between `props_save_convert` (store) and `props_parse_logical_lines` (load)
    // that H2's `FileLock` watchdog relies on (save → sleep → load → equals).
    #[test]
    fn props_save_convert_roundtrips_through_load_parser() {
        use super::{props_parse_logical_lines, props_save_convert};
        let cases = [
            // The exact shape H2's FileLock writes (method=file, id=<hex>).
            ("method", "file"),
            ("id", "18f3a2b1c0deadbeef0011"),
            // Spaces in keys, and separator/comment metacharacters in values.
            ("key with spaces", "value with = and : and # and ! chars"),
            // Whitespace escapes and a backslash path.
            ("tab\tkey", "line1\nline2\tc:\\winpath"),
            // BMP non-ASCII (single \uXXXX units; supplementary chars are a
            // separate known parser limitation and intentionally excluded).
            ("accent\u{00e9}", "caf\u{00e9}\u{2603}value"),
        ];
        for (k, v) in cases {
            let line = format!(
                "{}={}\n",
                props_save_convert(k, true),
                props_save_convert(v, false)
            );
            let parsed = props_parse_logical_lines(&line);
            assert_eq!(parsed.len(), 1, "case {k:?}: line={line:?}");
            assert_eq!(parsed[0].0, k, "key mismatch: line={line:?}");
            assert_eq!(parsed[0].1, v, "val mismatch: line={line:?}");
        }
    }

    #[test]
    fn props_save_convert_escapes_metacharacters() {
        use super::props_save_convert;
        // `= : # !` are escaped in both keys and values.
        assert_eq!(props_save_convert("a=b:c#d!e", false), "a\\=b\\:c\\#d\\!e");
        // Keys escape every space; values escape only a leading one.
        assert_eq!(props_save_convert("a b", true), "a\\ b");
        assert_eq!(props_save_convert("a b", false), "a b");
        assert_eq!(props_save_convert(" x", false), "\\ x");
        // Backslash, tab, newline.
        assert_eq!(props_save_convert("p\\q\tr\n", false), "p\\\\q\\tr\\n");
    }

    // Fix item 1: a recycled identity hash on a different class must not inherit
    // a dead object's slot — `pack_obj_key`/`ObjKeyEntry` carry a class marker.
    /// Defect 5: the MARKER's owner→overlay edge is unvalidated where the
    /// MUTATOR's is validated.
    ///
    /// `widened_obj_key` never hands a newcomer at another address an existing
    /// slot's key (gc-common w26-a; before that, its case 2b re-keyed and
    /// cleared a lone slot when a different class presented its identity
    /// hash). That protects only the MUTATOR's lookup. The dead collection's
    /// rows stay until the prune, and
    /// `gc_overlay_roots_for_collection` -- keyed by address, not by slot --
    /// has nothing to protect it. When an ordinary object lands at the dead
    /// collection's address, nothing clears the entry before the prune, and
    /// `gc_overlay_roots_for_collection` — a bare address lookup with no
    /// identity check at all — hands the old-gen marker the DEAD collection's
    /// references on behalf of the unrelated live object now at that address.
    ///
    /// Observed in the wild as eight
    /// `old-gen mark: rejecting external-overlay(BFS owner) candidate … not a
    /// plausible object base` warnings whose first words decode to ASCII string
    /// payload (`0x766f206b63617473` = "stack ov"), a raw heap pointer, and an
    /// interior address — i.e. the marker being handed non-headers for a live,
    /// already-marked owner.
    #[test]
    fn marker_owner_lookup_survives_address_recycling_by_a_non_collection() {
        use super::{obj_key_shard_for, owner_key_class_matches, pack_obj_key, ObjKeyEntry};
        // Tests the PREDICATE directly, not `gc_overlay_roots_for_collection`,
        // so it documents the defect regardless of whether enforcement is
        // enabled — which it is NOT by default; see
        // `owner_class_filter_enabled` for the measurements that disabled it.

        // A never-dereferenced, 8-aligned fake address for the dead collection.
        const DEAD_HASH: u32 = 0x5EED_BEEF;
        const LINKED_LIST_CLASS: u32 = 4242;
        const UNRELATED_CLASS: u32 = 7777;
        let recycled_addr = 0x5EED_0000usize;
        let dead_key = pack_obj_key(DEAD_HASH, 0);

        // The registry slot `widened_obj_key` would have created for a live
        // LinkedList at `recycled_addr`.
        obj_key_shard_for(DEAD_HASH)
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(DEAD_HASH)
            .or_default()
            .push(ObjKeyEntry {
                last_ptr: recycled_addr,
                generation: 0,
                class_id: LINKED_LIST_CLASS,
                vm: 0,
            });

        // While the collection is alive its own class matches, so its roots
        // are kept. (A predicate that answered `false` here would drop a live
        // collection's references — the failure mode that keeps enforcement
        // switched off by default.)
        assert!(
            owner_key_class_matches(dead_key, LINKED_LIST_CLASS, recycled_addr),
            "a LIVE collection's own overlay keys must be kept"
        );

        // It dies, and its block is REALLOCATED to an ordinary object of a
        // different class that never touches an overlay — so `widened_obj_key`
        // (and therefore case 2b's cleanup) never runs and nothing in the
        // system observes the handover. The recorded class still names the
        // dead collection, so the mismatch is detectable.
        assert!(
            !owner_key_class_matches(dead_key, UNRELATED_CLASS, recycled_addr),
            "a recycled address must not inherit the DEAD collection's overlay \
             keys: the marker has no identity check of its own, while
             `widened_obj_key` applies exactly this one"
        );

        // A ZEROED owner header is ambiguous — an already-freed block and the
        // legitimate `ClassId(0)` container share it — so it must NOT license a
        // drop. Enforcing there cascades one premature free into every element
        // the collection holds.
        assert!(
            owner_key_class_matches(dead_key, 0, recycled_addr),
            "class_id 0 is not evidence of recycling and must retain"
        );
    }

    #[test]
    fn pack_obj_key_roundtrip() {
        use super::pack_obj_key;
        // Distinct generations under the same hash yield distinct packed keys.
        let a = pack_obj_key(0xDEAD_BEEF, 0);
        let b = pack_obj_key(0xDEAD_BEEF, 1);
        assert_ne!(a, b);
        // The high 32 bits carry the hash, the low 32 the generation.
        assert_eq!((a >> 32) as u32, 0xDEAD_BEEF);
        assert_eq!(a as u32, 0);
        assert_eq!(b as u32, 1);
    }

    // Fix item 1 follow-up: generations must be handed out monotonically, never
    // recycled from `slots.len()`. Recycling would re-issue a generation a
    // still-live (or just-removed) slot already owns, re-aliasing two distinct
    // objects onto one packed key.
    #[test]
    fn next_generation_is_monotonic_and_never_recycles() {
        use super::{next_generation, ObjKeyEntry};
        // Empty bucket: first generation is 0.
        assert_eq!(next_generation(&[]), 0);
        // Two live slots, generations 0 and 1: next is 2 (not len()-derived 2 by
        // coincidence — verify it tracks the max, not the count).
        let two = [
            ObjKeyEntry {
                last_ptr: 0x10,
                generation: 0,
                class_id: 7,
                vm: 0,
            },
            ObjKeyEntry {
                last_ptr: 0x20,
                generation: 1,
                class_id: 7,
                vm: 0,
            },
        ];
        assert_eq!(next_generation(&two), 2);
        // After the LOW slot (gen 0) is pruned, only gen 1 remains. `slots.len()`
        // would now wrongly hand out generation 1 again — aliasing the live gen-1
        // slot. `next_generation` returns max(1)+1 = 2, never recycling gen 1.
        let after_prune = [ObjKeyEntry {
            last_ptr: 0x20,
            generation: 1,
            class_id: 7,
            vm: 0,
        }];
        assert_eq!(after_prune.len(), 1);
        assert_eq!(
            next_generation(&after_prune),
            2,
            "must not recycle a live slot's generation after a lower one is pruned"
        );
    }

    #[test]
    fn map_bucket_index_power_of_two() {
        use super::map_bucket_index;
        // For capacity 16, bucket index should be in 0..15
        for hash in [0, 1, 15, 16, 31, 100, -1, -100, i32::MAX, i32::MIN] {
            let idx = map_bucket_index(hash, 16);
            assert!(idx < 16, "hash={hash}, idx={idx}");
        }
    }

    #[test]
    fn map_bucket_index_distribution() {
        use super::map_bucket_index;
        // Different positive hashes should give different buckets
        let b0 = map_bucket_index(0, 16);
        let b1 = map_bucket_index(1, 16);
        let b15 = map_bucket_index(15, 16);
        assert_eq!(b0, 0);
        assert_eq!(b1, 1);
        assert_eq!(b15, 15);
    }

    // -----------------------------------------------------------------------
    // Phase 3.1: Registration completeness tests
    // -----------------------------------------------------------------------
    // These tests verify that all required j.u.concurrent classes are
    // registered correctly by checking the registry after full registration.

    fn build_registry() -> NativeMethodRegistry {
        let mut r = NativeMethodRegistry::new();
        register_collections_natives(&mut r);
        r
    }

    /// No natives may be bound to an iterator class this crate never MINTS.
    ///
    /// The durable output of the iterator-carrier census
    /// (`iterator-carrier-census-20260902.md`).
    /// A registration keyed on a class nobody produces is inert until someone
    /// produces one, and then it WINS the slot over the registration that
    /// matches the shape actually minted. It has happened twice:
    ///
    /// * `java/util/PriorityQueue$Itr` described a 2-field snapshot the real
    ///   class has never had; when `native_pq_iterator` began minting the real
    ///   class, iteration reported every queue EMPTY.
    /// * `java/util/ArrayDeque$Itr` sat in the same state after ArrayDeque's
    ///   iterator became the real `DeqIterator`, and a first attempt at a
    ///   `--synthetic-jdk` iterator during the census minted it and sprang the
    ///   trap again.
    ///
    /// Neither was visible as a test failure — the second's only caller had
    /// been failing at its first line for days inside a module that would not
    /// compile. So this asserts the property directly and cheaply: for the
    /// iterator classes this crate has retired, nothing may be registered.
    ///
    /// This is a DENY-LIST rather than the general property ("every registered
    /// iterator class has a mint site"), because a mint site is not visible to
    /// a registry test — `build_registry` sees names, not allocations. A
    /// deny-list of the two names that have actually caused this is a check
    /// that can fail; the general form would need the static scan the census
    /// ran, and would have to be kept honest by a control row whose answer is
    /// already known. Add a name here when a carrier is retired.
    #[test]
    fn no_natives_are_bound_to_a_retired_iterator_carrier() {
        let r = build_registry();
        for retired in [
            // Retired 2026-09-02: ArrayDeque hands out the real `DeqIterator`.
            "java/util/ArrayDeque$Itr",
            // Retired 2026-08-29: PriorityQueue hands out the real `$Itr`,
            // which IS this name — so the check below is specifically that the
            // 2-field SNAPSHOT natives are not the ones bound to it. The real
            // class is served through `VALUES_ITR_CARRIERS`.
        ] {
            for (method, desc) in [
                ("hasNext", "()Z"),
                ("next", "()Ljava/lang/Object;"),
                ("remove", "()V"),
            ] {
                assert!(
                    r.find(retired, method, desc).is_none(),
                    "{retired}.{method}{desc} is registered, but nothing mints \
                     {retired} — a dormant row is a trap armed for whoever \
                     mints one later; see the iterator-carrier census"
                );
            }
        }
        // Control: the carriers that ARE minted must still be bound, so this
        // test cannot pass by the registry being empty.
        for (_, carrier) in VALUES_ITR_CARRIERS {
            assert!(
                r.find(carrier, "next", "()Ljava/lang/Object;").is_some(),
                "{carrier} is a live carrier and must keep its natives"
            );
        }
    }

    #[test]
    fn collector_contract_interface_methods_registered() {
        let r = build_registry();
        let collector = "java/util/stream/Collector";
        assert!(
            r.find(collector, "supplier", "()Ljava/util/function/Supplier;")
                .is_some(),
            "Collector.supplier"
        );
        assert!(
            r.find(
                collector,
                "accumulator",
                "()Ljava/util/function/BiConsumer;"
            )
            .is_some(),
            "Collector.accumulator"
        );
        assert!(
            r.find(collector, "finisher", "()Ljava/util/function/Function;")
                .is_some(),
            "Collector.finisher"
        );
        assert!(
            r.find(
                collector,
                "combiner",
                "()Ljava/util/function/BinaryOperator;"
            )
            .is_some(),
            "Collector.combiner"
        );

        let object = "java/lang/Object";
        assert!(
            r.find(object, "supplier", "()Ljava/util/function/Supplier;")
                .is_some(),
            "Object.supplier fallback"
        );
        assert!(
            r.find(object, "accumulator", "()Ljava/util/function/BiConsumer;")
                .is_some(),
            "Object.accumulator fallback"
        );
        assert!(
            r.find(object, "finisher", "()Ljava/util/function/Function;")
                .is_some(),
            "Object.finisher fallback"
        );
        assert!(
            r.find(object, "combiner", "()Ljava/util/function/BinaryOperator;")
                .is_some(),
            "Object.combiner fallback"
        );

        assert!(
            r.find("java/util/function/Supplier", "get", "()Ljava/lang/Object;")
                .is_some(),
            "Supplier.get"
        );
        assert!(
            r.find(
                "java/util/function/BiConsumer",
                "accept",
                "(Ljava/lang/Object;Ljava/lang/Object;)V"
            )
            .is_some(),
            "BiConsumer.accept"
        );
        assert!(
            r.find(
                "java/util/function/Function",
                "apply",
                "(Ljava/lang/Object;)Ljava/lang/Object;"
            )
            .is_some(),
            "Function.apply"
        );
        assert!(
            r.find(
                "java/util/function/BinaryOperator",
                "apply",
                "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;"
            )
            .is_some(),
            "BinaryOperator.apply"
        );
    }

    #[test]
    #[cfg(feature = "synthetic-jdk")]
    fn concurrent_linked_queue_registered() {
        let r = build_registry();
        let clq = "java/util/concurrent/ConcurrentLinkedQueue";
        assert!(r.find(clq, "<init>", "()V").is_some(), "CLQ <init>");
        assert!(
            r.find(clq, "offer", "(Ljava/lang/Object;)Z").is_some(),
            "CLQ offer"
        );
        assert!(
            r.find(clq, "poll", "()Ljava/lang/Object;").is_some(),
            "CLQ poll"
        );
        assert!(
            r.find(clq, "peek", "()Ljava/lang/Object;").is_some(),
            "CLQ peek"
        );
        assert!(r.find(clq, "isEmpty", "()Z").is_some(), "CLQ isEmpty");
        assert!(r.find(clq, "size", "()I").is_some(), "CLQ size");
        assert!(
            r.find(clq, "iterator", "()Ljava/util/Iterator;").is_some(),
            "CLQ iterator"
        );
    }

    #[test]
    #[cfg(feature = "synthetic-jdk")]
    fn concurrent_linked_deque_registered() {
        let r = build_registry();
        let cld = "java/util/concurrent/ConcurrentLinkedDeque";
        assert!(
            r.find(cld, "offerFirst", "(Ljava/lang/Object;)Z").is_some(),
            "CLD offerFirst"
        );
        assert!(
            r.find(cld, "offerLast", "(Ljava/lang/Object;)Z").is_some(),
            "CLD offerLast"
        );
        assert!(
            r.find(cld, "pollFirst", "()Ljava/lang/Object;").is_some(),
            "CLD pollFirst"
        );
        assert!(
            r.find(cld, "pollLast", "()Ljava/lang/Object;").is_some(),
            "CLD pollLast"
        );
        assert!(
            r.find(cld, "peekFirst", "()Ljava/lang/Object;").is_some(),
            "CLD peekFirst"
        );
        assert!(
            r.find(cld, "peekLast", "()Ljava/lang/Object;").is_some(),
            "CLD peekLast"
        );
    }

    #[test]
    #[cfg(feature = "synthetic-jdk")]
    fn linked_blocking_queue_registered() {
        let r = build_registry();
        let lbq = "java/util/concurrent/LinkedBlockingQueue";
        assert!(r.find(lbq, "<init>", "()V").is_some(), "LBQ <init>");
        assert!(r.find(lbq, "<init>", "(I)V").is_some(), "LBQ <init>(I)");
        assert!(
            r.find(lbq, "put", "(Ljava/lang/Object;)V").is_some(),
            "LBQ put"
        );
        assert!(
            r.find(lbq, "take", "()Ljava/lang/Object;").is_some(),
            "LBQ take"
        );
        assert!(
            r.find(lbq, "offer", "(Ljava/lang/Object;)Z").is_some(),
            "LBQ offer"
        );
        assert!(
            r.find(lbq, "poll", "()Ljava/lang/Object;").is_some(),
            "LBQ poll"
        );
        assert!(
            r.find(lbq, "peek", "()Ljava/lang/Object;").is_some(),
            "LBQ peek"
        );
        assert!(
            r.find(lbq, "remainingCapacity", "()I").is_some(),
            "LBQ remainingCapacity"
        );
        assert!(r.find(lbq, "size", "()I").is_some(), "LBQ size");
    }

    #[test]
    #[cfg(feature = "synthetic-jdk")]
    fn array_blocking_queue_registered() {
        let r = build_registry();
        let abq = "java/util/concurrent/ArrayBlockingQueue";
        assert!(r.find(abq, "<init>", "(I)V").is_some(), "ABQ <init>(I)");
        assert!(
            r.find(abq, "put", "(Ljava/lang/Object;)V").is_some(),
            "ABQ put"
        );
        assert!(
            r.find(abq, "take", "()Ljava/lang/Object;").is_some(),
            "ABQ take"
        );
        assert!(
            r.find(abq, "offer", "(Ljava/lang/Object;)Z").is_some(),
            "ABQ offer"
        );
        assert!(
            r.find(abq, "poll", "()Ljava/lang/Object;").is_some(),
            "ABQ poll"
        );
        assert!(
            r.find(abq, "remainingCapacity", "()I").is_some(),
            "ABQ remainingCapacity"
        );
    }

    #[test]
    fn completable_future_completion_stage_registered() {
        let r = build_registry();
        let cf = "java/util/concurrent/CompletableFuture";
        assert!(
            r.find(
                cf,
                "thenRun",
                "(Ljava/lang/Runnable;)Ljava/util/concurrent/CompletableFuture;"
            )
            .is_some(),
            "CF thenRun"
        );
        assert!(
            r.find(
                cf,
                "thenCompose",
                "(Ljava/util/function/Function;)Ljava/util/concurrent/CompletableFuture;"
            )
            .is_some(),
            "CF thenCompose"
        );
        assert!(
            r.find(cf, "thenCombine",
                "(Ljava/util/concurrent/CompletionStage;Ljava/util/function/BiFunction;)Ljava/util/concurrent/CompletableFuture;").is_some(),
            "CF thenCombine"
        );
        assert!(
            r.find(
                cf,
                "exceptionally",
                "(Ljava/util/function/Function;)Ljava/util/concurrent/CompletableFuture;"
            )
            .is_some(),
            "CF exceptionally"
        );
        assert!(
            r.find(
                cf,
                "handle",
                "(Ljava/util/function/BiFunction;)Ljava/util/concurrent/CompletableFuture;"
            )
            .is_some(),
            "CF handle"
        );
        assert!(
            r.find(
                cf,
                "whenComplete",
                "(Ljava/util/function/BiConsumer;)Ljava/util/concurrent/CompletableFuture;"
            )
            .is_some(),
            "CF whenComplete"
        );
        assert!(
            r.find(cf, "allOf", "([Ljava/util/concurrent/CompletableFuture;)Ljava/util/concurrent/CompletableFuture;").is_some(),
            "CF allOf"
        );
        assert!(
            r.find(cf, "anyOf", "([Ljava/util/concurrent/CompletableFuture;)Ljava/util/concurrent/CompletableFuture;").is_some(),
            "CF anyOf"
        );
    }

    #[test]
    fn fork_join_pool_invoke_and_quiescence_left_to_native_builtins() {
        let r = build_registry();
        let pool = "java/util/concurrent/ForkJoinPool";
        // `awaitQuiescence` deliberately does NOT live here: `native-builtins`
        // registers it after establishing the real async-worker completion
        // tracker, and `register_collections_natives` runs later, so a local
        // constant would win and overwrite that stateful implementation. This
        // test used to assert the opposite and had gone red unnoticed.
        assert!(
            r.find(
                pool,
                "awaitQuiescence",
                "(JLjava/util/concurrent/TimeUnit;)Z"
            )
            .is_none(),
            "FJP awaitQuiescence must stay owned by native-builtins"
        );
        // `invoke` deliberately does NOT live here either, for the same
        // last-write-wins reason: this registrar runs after
        // `phases_early::register_real_jdk_forkjoin_essentials`, so a local
        // copy would overwrite the shape-aware implementation. See the note
        // at the registration site.
        assert!(
            r.find(
                pool,
                "invoke",
                "(Ljava/util/concurrent/ForkJoinTask;)Ljava/lang/Object;"
            )
            .is_none(),
            "FJP invoke must stay owned by native-builtins"
        );
    }

    /// The `invoke` natives for the ForkJoinTask family must NOT be registered
    /// by this crate. The copies that used to live here wrote an object
    /// reference into `ForkJoinTask.status` (field 0, an `int`) and dispatched
    /// `compute()Ljava/lang/Object;` unconditionally, so every
    /// `RecursiveAction` handed to `pool.invoke()` silently never ran. Because
    /// this registrar runs last, they overwrote the correct
    /// `phases_early` implementations. Keep this crate out of the family.
    #[test]
    fn fork_join_task_invoke_left_to_native_builtins() {
        let r = build_registry();
        for cls in [
            "java/util/concurrent/ForkJoinTask",
            "java/util/concurrent/RecursiveTask",
            "java/util/concurrent/RecursiveAction",
        ] {
            assert!(
                r.find(cls, "invoke", "()Ljava/lang/Object;").is_none(),
                "{cls}.invoke must stay owned by native-builtins"
            );
        }
    }

    #[test]
    fn thread_pool_executor_stats_registered() {
        let r = build_registry();
        let tp = "java/util/concurrent/ThreadPoolExecutor";
        assert!(
            r.find(tp, "getPoolSize", "()I").is_some(),
            "TPE getPoolSize"
        );
        assert!(
            r.find(tp, "getActiveCount", "()I").is_some(),
            "TPE getActiveCount"
        );
        assert!(
            r.find(tp, "getCorePoolSize", "()I").is_some(),
            "TPE getCorePoolSize"
        );
        assert!(
            r.find(tp, "getMaximumPoolSize", "()I").is_some(),
            "TPE getMaximumPoolSize"
        );
        assert!(r.find(tp, "isShutdown", "()Z").is_some(), "TPE isShutdown");
        assert!(
            r.find(tp, "shutdownNow", "()Ljava/util/List;").is_some(),
            "TPE shutdownNow"
        );
    }

    #[test]
    fn completion_stage_interface_registered() {
        let r = build_registry();
        let cs = "java/util/concurrent/CompletionStage";
        assert!(
            r.find(
                cs,
                "thenApply",
                "(Ljava/util/function/Function;)Ljava/util/concurrent/CompletionStage;"
            )
            .is_some(),
            "CS thenApply"
        );
        assert!(
            r.find(
                cs,
                "thenAccept",
                "(Ljava/util/function/Consumer;)Ljava/util/concurrent/CompletionStage;"
            )
            .is_some(),
            "CS thenAccept"
        );
        assert!(
            r.find(
                cs,
                "thenRun",
                "(Ljava/lang/Runnable;)Ljava/util/concurrent/CompletionStage;"
            )
            .is_some(),
            "CS thenRun"
        );
        assert!(
            r.find(
                cs,
                "thenCompose",
                "(Ljava/util/function/Function;)Ljava/util/concurrent/CompletionStage;"
            )
            .is_some(),
            "CS thenCompose"
        );
        assert!(
            r.find(
                cs,
                "exceptionally",
                "(Ljava/util/function/Function;)Ljava/util/concurrent/CompletionStage;"
            )
            .is_some(),
            "CS exceptionally"
        );
    }

    #[test]
    fn concurrent_hashmap_registered() {
        let r = build_registry();
        let chm = "java/util/concurrent/ConcurrentHashMap";
        assert!(r.find(chm, "<init>", "()V").is_some(), "CHM <init>");
        assert!(
            r.find(
                chm,
                "put",
                "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;"
            )
            .is_some(),
            "CHM put"
        );
        assert!(
            r.find(chm, "get", "(Ljava/lang/Object;)Ljava/lang/Object;")
                .is_some(),
            "CHM get"
        );
        assert!(r.find(chm, "size", "()I").is_some(), "CHM size");
        assert!(
            r.find(chm, "containsKey", "(Ljava/lang/Object;)Z")
                .is_some(),
            "CHM containsKey"
        );
    }

    #[test]
    #[cfg(feature = "synthetic-jdk")]
    fn scheduled_executor_registered() {
        let r = build_registry();
        let stpe = "java/util/concurrent/ScheduledThreadPoolExecutor";
        assert!(r.find(stpe, "<init>", "(I)V").is_some(), "STPE <init>");
        assert!(r.find(stpe, "shutdown", "()V").is_some(), "STPE shutdown");
        assert!(
            r.find(stpe, "isShutdown", "()Z").is_some(),
            "STPE isShutdown"
        );
    }

    #[test]
    #[cfg(feature = "synthetic-jdk")]
    fn blocking_queue_interface_registered() {
        let r = build_registry();
        let bq = "java/util/concurrent/BlockingQueue";
        assert!(
            r.find(bq, "put", "(Ljava/lang/Object;)V").is_some(),
            "BQ put"
        );
        assert!(
            r.find(bq, "take", "()Ljava/lang/Object;").is_some(),
            "BQ take"
        );
        assert!(
            r.find(bq, "offer", "(Ljava/lang/Object;)Z").is_some(),
            "BQ offer"
        );
        assert!(
            r.find(bq, "poll", "()Ljava/lang/Object;").is_some(),
            "BQ poll"
        );
    }

    #[test]
    fn cf_field_layout_constants_consistent() {
        // Verify the field layout constants match between this file and
        // the native-builtins crate (both use fields 0=result, 1=done)
        assert_eq!(CF_FIELD_RESULT, 0);
        assert_eq!(CF_FIELD_DONE, 1);
        assert_eq!(TP_FIELD_SIZE, 0);
        assert_eq!(TP_FIELD_SHUTDOWN, 1);
        // Tightened (W7-66): the request must not exceed what the real class
        // declares. `javap -p java.util.concurrent.CompletableFuture` on JDK
        // 25.0.3.9 lists exactly two instance fields — `volatile Object result`
        // and `volatile CompletableFuture$Completion stack` — and its
        // superclass is `java.lang.Object`. The old value was 4, so every
        // synthetic CF carried two slots past the declared width that no site
        // in the workspace read or wrote. Raising this again is an
        // over-allocation and `CRATONVM_DBG_LAYOUT_ALIAS=1` will say so.
        assert_eq!(
            CF_NUM_FIELDS, 2,
            "synthetic CompletableFuture must not be allocated wider than the \
             two fields java.util.concurrent.CompletableFuture declares"
        );
    }

    // -----------------------------------------------------------------------
    // Edge-case tests for capacity growth and map helpers
    // -----------------------------------------------------------------------

    #[test]
    fn map_bucket_index_single_bucket() {
        // Capacity of 1 — everything goes to bucket 0
        assert_eq!(map_bucket_index(0, 1), 0);
        assert_eq!(map_bucket_index(42, 1), 0);
        assert_eq!(map_bucket_index(-1, 1), 0);
    }

    #[test]
    fn map_bucket_index_large_capacity() {
        let cap = 1 << 20; // 1M buckets
        for hash in [0, 1, -1, i32::MAX, i32::MIN] {
            let idx = map_bucket_index(hash, cap);
            assert!(idx < cap as usize, "hash={hash}, idx={idx}, cap={cap}");
        }
    }

    #[test]
    fn map_bucket_index_capacity_two() {
        // Capacity 2: index should be 0 or 1
        for hash in [0, 1, 2, 3, -1, -2, i32::MAX, i32::MIN] {
            let idx = map_bucket_index(hash, 2);
            assert!(idx < 2, "hash={hash}, idx={idx}");
        }
    }

    // -----------------------------------------------------------------------
    // `values_equal` — call the PRODUCTION function, not a copy of its `match`.
    //
    // These four tests used to build a `Value` literal and then assert a
    // `matches!` pattern against that same literal two lines later, e.g.
    // `matches!((&Value::Int(42), &Value::Int(42)), (Value::Int(x), Value::Int(y)) if x == y)`.
    // That is a tautology over constants: `values_equal` was never on the call
    // path, so rewriting its body to `return true` — or deleting it outright —
    // left all four green. None of these cases needs any heap state, so a bare
    // `MockCtx` (no classes, no objects) is enough to reach the real function:
    // the wrapper-class guard at the top of `values_equal` only fires for
    // `Object(Some(_))` on both sides, and `normalize_for_compare` passes raw
    // primitives and `Object(None)` through without touching the context.
    // -----------------------------------------------------------------------

    #[test]
    fn values_equal_null_null() {
        let ctx = lbq_blocking_tests::MockCtx::new(1);
        // Java `null.equals`-free contract: `List.of(a).contains(null)` finds a
        // stored null, and a null map value matches a null probe.
        assert!(
            values_equal(&ctx, &Value::Object(None), &Value::Object(None)),
            "two null references must compare equal"
        );
        // A null must NOT match anything else, or `contains(null)` reports true
        // for a list holding only real elements.
        assert!(
            !values_equal(&ctx, &Value::Object(None), &Value::Int(0)),
            "null must not equal Int(0)"
        );
        assert!(
            !values_equal(&ctx, &Value::Int(0), &Value::Object(None)),
            "Int(0) must not equal null (symmetry)"
        );
    }

    #[test]
    fn values_equal_ints() {
        let ctx = lbq_blocking_tests::MockCtx::new(1);
        assert!(values_equal(&ctx, &Value::Int(42), &Value::Int(42)));
        assert!(!values_equal(&ctx, &Value::Int(42), &Value::Int(99)));
        // Boundaries: a truncating or unsigned comparison would collapse these.
        assert!(values_equal(
            &ctx,
            &Value::Int(i32::MIN),
            &Value::Int(i32::MIN)
        ));
        assert!(!values_equal(
            &ctx,
            &Value::Int(i32::MIN),
            &Value::Int(i32::MAX)
        ));
        assert!(!values_equal(&ctx, &Value::Int(0), &Value::Int(-1)));
    }

    #[test]
    fn values_equal_long() {
        let ctx = lbq_blocking_tests::MockCtx::new(1);
        assert!(values_equal(
            &ctx,
            &Value::Long(123_456_789),
            &Value::Long(123_456_789)
        ));
        assert!(!values_equal(
            &ctx,
            &Value::Long(123_456_789),
            &Value::Long(0)
        ));
        // A 32-bit-narrowing comparison would call these two equal.
        assert!(!values_equal(
            &ctx,
            &Value::Long(1),
            &Value::Long((1_i64 << 32) | 1)
        ));
        assert!(values_equal(
            &ctx,
            &Value::Long(i64::MIN),
            &Value::Long(i64::MIN)
        ));
        assert!(!values_equal(
            &ctx,
            &Value::Long(i64::MIN),
            &Value::Long(i64::MAX)
        ));
    }

    #[test]
    fn values_equal_mixed_types() {
        let ctx = lbq_blocking_tests::MockCtx::new(1);
        // The OLD body asserted that an Int and a Long "should not match".
        // That is not what `values_equal` promises and not what it does: a RAW
        // `Value::Int`/`Value::Long` is a VM-level representation choice with no
        // Java wrapper class behind it, so the cross-type numeric arms are
        // deliberately in force (see the comment on the wrapper-class guard).
        // `values_equal_rejects_cross_wrapper_boxes` covers the BOXED case,
        // which is the one that must answer false.
        assert!(
            values_equal(&ctx, &Value::Int(42), &Value::Long(42)),
            "raw Int(42) and raw Long(42) are the same VM value"
        );
        assert!(
            values_equal(&ctx, &Value::Long(42), &Value::Int(42)),
            "cross-type numeric comparison is symmetric"
        );
        // Sign extension, not zero extension: Int(-1) is Long(-1), not
        // Long(0xFFFF_FFFF).
        assert!(values_equal(&ctx, &Value::Int(-1), &Value::Long(-1)));
        assert!(!values_equal(
            &ctx,
            &Value::Int(-1),
            &Value::Long(0xFFFF_FFFF)
        ));
        // Different values of different types stay unequal.
        assert!(!values_equal(&ctx, &Value::Int(42), &Value::Long(43)));
        // A numeric value never equals a reference.
        assert!(!values_equal(&ctx, &Value::Int(0), &Value::Object(None)));
    }

    #[test]
    fn arraylist_registered_completely() {
        let r = build_registry();
        let c = "java/util/ArrayList";
        assert!(r.find(c, "<init>", "()V").is_some(), "AL <init>");
        assert!(r.find(c, "<init>", "(I)V").is_some(), "AL <init>(I)");
        assert!(r.find(c, "size", "()I").is_some(), "AL size");
        assert!(r.find(c, "isEmpty", "()Z").is_some(), "AL isEmpty");
        assert!(
            r.find(c, "get", "(I)Ljava/lang/Object;").is_some(),
            "AL get"
        );
        assert!(
            r.find(c, "set", "(ILjava/lang/Object;)Ljava/lang/Object;")
                .is_some(),
            "AL set"
        );
        assert!(
            r.find(c, "add", "(Ljava/lang/Object;)Z").is_some(),
            "AL add"
        );
        assert!(
            r.find(c, "remove", "(I)Ljava/lang/Object;").is_some(),
            "AL remove(I)"
        );
        assert!(r.find(c, "clear", "()V").is_some(), "AL clear");
        assert!(
            r.find(c, "contains", "(Ljava/lang/Object;)Z").is_some(),
            "AL contains"
        );
        assert!(
            r.find(c, "indexOf", "(Ljava/lang/Object;)I").is_some(),
            "AL indexOf"
        );
        assert!(
            r.find(c, "ensureCapacity", "(I)V").is_some(),
            "AL ensureCapacity"
        );
        assert!(r.find(c, "trimToSize", "()V").is_some(), "AL trimToSize");
    }

    #[test]
    fn hashmap_registered_completely() {
        let r = build_registry();
        let c = "java/util/HashMap";
        assert!(r.find(c, "<init>", "()V").is_some(), "HM <init>");
        assert!(r.find(c, "<init>", "(I)V").is_some(), "HM <init>(I)");
        assert!(r.find(c, "size", "()I").is_some(), "HM size");
        assert!(r.find(c, "isEmpty", "()Z").is_some(), "HM isEmpty");
        assert!(
            r.find(
                c,
                "put",
                "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;"
            )
            .is_some(),
            "HM put"
        );
        assert!(
            r.find(c, "get", "(Ljava/lang/Object;)Ljava/lang/Object;")
                .is_some(),
            "HM get"
        );
        assert!(
            r.find(c, "remove", "(Ljava/lang/Object;)Ljava/lang/Object;")
                .is_some(),
            "HM remove"
        );
        assert!(
            r.find(c, "containsKey", "(Ljava/lang/Object;)Z").is_some(),
            "HM containsKey"
        );
        assert!(r.find(c, "clear", "()V").is_some(), "HM clear");
    }

    // -----------------------------------------------------------------------
    // HashSet registration completeness
    // -----------------------------------------------------------------------

    #[test]
    fn hashset_registered_completely() {
        let r = build_registry();
        let c = "java/util/HashSet";
        assert!(r.find(c, "<init>", "()V").is_some(), "HS <init>");
        assert!(r.find(c, "<init>", "(I)V").is_some(), "HS <init>(I)");
        assert!(r.find(c, "size", "()I").is_some(), "HS size");
        assert!(r.find(c, "isEmpty", "()Z").is_some(), "HS isEmpty");
        assert!(
            r.find(c, "add", "(Ljava/lang/Object;)Z").is_some(),
            "HS add"
        );
        assert!(
            r.find(c, "remove", "(Ljava/lang/Object;)Z").is_some(),
            "HS remove"
        );
        assert!(
            r.find(c, "contains", "(Ljava/lang/Object;)Z").is_some(),
            "HS contains"
        );
        assert!(r.find(c, "clear", "()V").is_some(), "HS clear");
        assert!(
            r.find(c, "iterator", "()Ljava/util/Iterator;").is_some(),
            "HS iterator"
        );
        assert!(
            r.find(c, "toArray", "()[Ljava/lang/Object;").is_some(),
            "HS toArray"
        );
        assert!(
            r.find(c, "toString", "()Ljava/lang/String;").is_some(),
            "HS toString"
        );
    }

    // -----------------------------------------------------------------------
    // LinkedList registration completeness
    // -----------------------------------------------------------------------

    #[test]
    fn linked_list_registered_completely() {
        let r = build_registry();
        let c = "java/util/LinkedList";
        assert!(r.find(c, "<init>", "()V").is_some(), "LL <init>");
        assert!(
            r.find(c, "add", "(Ljava/lang/Object;)Z").is_some(),
            "LL add"
        );
        assert!(
            r.find(c, "addFirst", "(Ljava/lang/Object;)V").is_some(),
            "LL addFirst"
        );
        assert!(
            r.find(c, "addLast", "(Ljava/lang/Object;)V").is_some(),
            "LL addLast"
        );
        assert!(
            r.find(c, "get", "(I)Ljava/lang/Object;").is_some(),
            "LL get"
        );
        assert!(
            r.find(c, "getFirst", "()Ljava/lang/Object;").is_some(),
            "LL getFirst"
        );
        assert!(
            r.find(c, "getLast", "()Ljava/lang/Object;").is_some(),
            "LL getLast"
        );
        assert!(
            r.find(c, "removeFirst", "()Ljava/lang/Object;").is_some(),
            "LL removeFirst"
        );
        assert!(
            r.find(c, "removeLast", "()Ljava/lang/Object;").is_some(),
            "LL removeLast"
        );
        assert!(r.find(c, "size", "()I").is_some(), "LL size");
    }

    // -----------------------------------------------------------------------
    // LinkedHashMap registration completeness
    // -----------------------------------------------------------------------

    #[test]
    fn linked_hashmap_registered_completely() {
        let r = build_registry();
        let c = "java/util/LinkedHashMap";
        assert!(r.find(c, "<init>", "()V").is_some(), "LHM <init>");
        assert!(r.find(c, "<init>", "(I)V").is_some(), "LHM <init>(I)");
        assert!(r.find(c, "size", "()I").is_some(), "LHM size");
        assert!(r.find(c, "isEmpty", "()Z").is_some(), "LHM isEmpty");
        assert!(
            r.find(
                c,
                "put",
                "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;"
            )
            .is_some(),
            "LHM put"
        );
        assert!(
            r.find(c, "get", "(Ljava/lang/Object;)Ljava/lang/Object;")
                .is_some(),
            "LHM get"
        );
    }

    // -----------------------------------------------------------------------
    // ArrayDeque registration completeness
    // -----------------------------------------------------------------------

    #[test]
    fn array_deque_registered_completely() {
        let r = build_registry();
        let c = "java/util/ArrayDeque";
        assert!(r.find(c, "<init>", "()V").is_some(), "AD <init>");
        assert!(r.find(c, "<init>", "(I)V").is_some(), "AD <init>(I)");
        assert!(r.find(c, "size", "()I").is_some(), "AD size");
        assert!(r.find(c, "isEmpty", "()Z").is_some(), "AD isEmpty");
        assert!(
            r.find(c, "addFirst", "(Ljava/lang/Object;)V").is_some(),
            "AD addFirst"
        );
        assert!(
            r.find(c, "addLast", "(Ljava/lang/Object;)V").is_some(),
            "AD addLast"
        );
        assert!(
            r.find(c, "add", "(Ljava/lang/Object;)Z").is_some(),
            "AD add"
        );
        assert!(
            r.find(c, "offerFirst", "(Ljava/lang/Object;)Z").is_some(),
            "AD offerFirst"
        );
        assert!(
            r.find(c, "offerLast", "(Ljava/lang/Object;)Z").is_some(),
            "AD offerLast"
        );
    }

    // -----------------------------------------------------------------------
    // PriorityQueue registration completeness
    // -----------------------------------------------------------------------

    #[test]
    fn priority_queue_registered_completely() {
        let r = build_registry();
        let c = "java/util/PriorityQueue";
        assert!(r.find(c, "<init>", "()V").is_some(), "PQ <init>");
        assert!(r.find(c, "<init>", "(I)V").is_some(), "PQ <init>(I)");
        assert!(
            r.find(c, "<init>", "(Ljava/util/Comparator;)V").is_some(),
            "PQ <init>(Comparator)"
        );
        assert!(r.find(c, "size", "()I").is_some(), "PQ size");
        assert!(r.find(c, "isEmpty", "()Z").is_some(), "PQ isEmpty");
        assert!(
            r.find(c, "add", "(Ljava/lang/Object;)Z").is_some(),
            "PQ add"
        );
        assert!(
            r.find(c, "offer", "(Ljava/lang/Object;)Z").is_some(),
            "PQ offer"
        );
        assert!(
            r.find(c, "peek", "()Ljava/lang/Object;").is_some(),
            "PQ peek"
        );
        assert!(
            r.find(c, "poll", "()Ljava/lang/Object;").is_some(),
            "PQ poll"
        );
        assert!(
            r.find(c, "remove", "(Ljava/lang/Object;)Z").is_some(),
            "PQ remove"
        );
        assert!(
            r.find(c, "contains", "(Ljava/lang/Object;)Z").is_some(),
            "PQ contains"
        );
        assert!(r.find(c, "clear", "()V").is_some(), "PQ clear");
        assert!(
            r.find(c, "toArray", "()[Ljava/lang/Object;").is_some(),
            "PQ toArray"
        );
    }

    // -----------------------------------------------------------------------
    // Vector registration completeness
    // -----------------------------------------------------------------------

    #[test]
    fn vector_registered_completely() {
        let r = build_registry();
        let c = "java/util/Vector";
        assert!(r.find(c, "<init>", "()V").is_some(), "Vec <init>");
        assert!(r.find(c, "<init>", "(I)V").is_some(), "Vec <init>(I)");
        assert!(r.find(c, "size", "()I").is_some(), "Vec size");
        assert!(r.find(c, "isEmpty", "()Z").is_some(), "Vec isEmpty");
        assert!(
            r.find(c, "get", "(I)Ljava/lang/Object;").is_some(),
            "Vec get"
        );
        assert!(
            r.find(c, "elementAt", "(I)Ljava/lang/Object;").is_some(),
            "Vec elementAt"
        );
    }

    // -----------------------------------------------------------------------
    // Stack registration completeness
    // -----------------------------------------------------------------------

    #[test]
    fn stack_registered_completely() {
        let r = build_registry();
        let c = "java/util/Stack";
        assert!(r.find(c, "<init>", "()V").is_some(), "Stack <init>");
        assert!(r.find(c, "size", "()I").is_some(), "Stack size");
        assert!(r.find(c, "isEmpty", "()Z").is_some(), "Stack isEmpty");
        assert!(
            r.find(c, "get", "(I)Ljava/lang/Object;").is_some(),
            "Stack get"
        );
        assert!(
            r.find(c, "add", "(Ljava/lang/Object;)Z").is_some(),
            "Stack add"
        );
        assert!(
            r.find(c, "remove", "(I)Ljava/lang/Object;").is_some(),
            "Stack remove"
        );
        assert!(r.find(c, "clear", "()V").is_some(), "Stack clear");
        assert!(
            r.find(c, "contains", "(Ljava/lang/Object;)Z").is_some(),
            "Stack contains"
        );
    }

    // -----------------------------------------------------------------------
    // TreeMap registration completeness
    // -----------------------------------------------------------------------

    #[test]
    fn tree_map_registered_completely() {
        let r = build_registry();
        let c = "java/util/TreeMap";
        assert!(r.find(c, "<init>", "()V").is_some(), "TM <init>");
        assert!(
            r.find(c, "<init>", "(Ljava/util/Comparator;)V").is_some(),
            "TM <init>(Comparator)"
        );
        assert!(
            r.find(
                c,
                "put",
                "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;"
            )
            .is_some(),
            "TM put"
        );
        assert!(
            r.find(c, "get", "(Ljava/lang/Object;)Ljava/lang/Object;")
                .is_some(),
            "TM get"
        );
        assert!(r.find(c, "size", "()I").is_some(), "TM size");
    }

    // -----------------------------------------------------------------------
    // TreeSet registration completeness
    // -----------------------------------------------------------------------

    #[test]
    fn tree_set_registered_completely() {
        let r = build_registry();
        let c = "java/util/TreeSet";
        assert!(r.find(c, "<init>", "()V").is_some(), "TS <init>");
        assert!(
            r.find(c, "<init>", "(Ljava/util/Comparator;)V").is_some(),
            "TS <init>(Comparator)"
        );
        assert!(
            r.find(c, "<init>", "(Ljava/util/Collection;)V").is_some(),
            "TS <init>(Collection)"
        );
        assert!(
            r.find(c, "add", "(Ljava/lang/Object;)Z").is_some(),
            "TS add"
        );
        assert!(
            r.find(c, "remove", "(Ljava/lang/Object;)Z").is_some(),
            "TS remove"
        );
        assert!(
            r.find(c, "contains", "(Ljava/lang/Object;)Z").is_some(),
            "TS contains"
        );
        assert!(r.find(c, "size", "()I").is_some(), "TS size");
        assert!(r.find(c, "isEmpty", "()Z").is_some(), "TS isEmpty");
        assert!(r.find(c, "clear", "()V").is_some(), "TS clear");
    }

    // -----------------------------------------------------------------------
    // Optional registration completeness
    // -----------------------------------------------------------------------

    #[test]
    fn optional_registered_completely() {
        let r = build_registry();
        let c = "java/util/Optional";
        assert!(
            r.find(c, "empty", "()Ljava/util/Optional;").is_some(),
            "Opt empty"
        );
        assert!(
            r.find(c, "of", "(Ljava/lang/Object;)Ljava/util/Optional;")
                .is_some(),
            "Opt of"
        );
        assert!(
            r.find(c, "ofNullable", "(Ljava/lang/Object;)Ljava/util/Optional;")
                .is_some(),
            "Opt ofNullable"
        );
        assert!(
            r.find(c, "get", "()Ljava/lang/Object;").is_some(),
            "Opt get"
        );
        assert!(r.find(c, "isPresent", "()Z").is_some(), "Opt isPresent");
        assert!(r.find(c, "isEmpty", "()Z").is_some(), "Opt isEmpty");
    }

    // -----------------------------------------------------------------------
    // Field layout constant tests — correctness and uniqueness
    // -----------------------------------------------------------------------

    #[test]
    fn arraylist_field_layout_valid() {
        assert_eq!(AL_FIELD_DATA, 0, "AL data field should be 0");
        assert_eq!(AL_FIELD_SIZE, 1, "AL size field should be 1");
        assert_eq!(AL_NUM_FIELDS, 2, "AL should have 2 fields");
        assert_eq!(AL_DEFAULT_CAPACITY, 10, "AL default capacity should be 10");
    }

    #[test]
    fn hashmap_field_layout_valid() {
        assert_eq!(MAP_FIELD_BUCKETS, 0);
        assert_eq!(MAP_FIELD_SIZE, 1);
        assert_eq!(MAP_FIELD_CAPACITY, 2);
        assert_eq!(
            MAP_DEFAULT_CAPACITY, 16,
            "HM default capacity should be 16 (power of 2)"
        );
        // Default capacity must be a power of two for bucket indexing
        assert!(
            MAP_DEFAULT_CAPACITY.is_power_of_two(),
            "HM default capacity must be power of 2"
        );
    }

    #[test]
    fn hashmap_node_field_layout_valid() {
        // Node slot layout matches the `NODE_FIELD_*` consts the overlay
        // actually uses: hash@0, key@1, value@2, next@3. The previous
        // assertions asserted key@0/hash@2 — a layout the code never used; they
        // had failed since the initial commit.
        assert_eq!(NODE_FIELD_HASH, 0);
        assert_eq!(NODE_FIELD_KEY, 1);
        assert_eq!(NODE_FIELD_VALUE, 2);
        assert_eq!(NODE_FIELD_NEXT, 3);
        assert_eq!(NODE_NUM_FIELDS, 4);
        // All indices must be within NUM_FIELDS
        assert!(NODE_FIELD_KEY < NODE_NUM_FIELDS);
        assert!(NODE_FIELD_VALUE < NODE_NUM_FIELDS);
        assert!(NODE_FIELD_HASH < NODE_NUM_FIELDS);
        assert!(NODE_FIELD_NEXT < NODE_NUM_FIELDS);
    }

    #[test]
    fn linked_list_field_layout_valid() {
        assert_eq!(LL_FIELD_HEAD, 0);
        assert_eq!(LL_FIELD_TAIL, 1);
        assert_eq!(LL_FIELD_SIZE, 2);
        // Node slot layout (matches the `LL_NODE_*` consts the overlay actually
        // uses): element@0, next@1, prev@2. The previous assertions here had the
        // element/prev indices swapped vs the real constants and had been failing
        // since the initial commit; corrected to the layout the overlay uses.
        assert_eq!(LL_NODE_ELEM, 0);
        assert_eq!(LL_NODE_NEXT, 1);
        assert_eq!(LL_NODE_PREV, 2);
    }

    #[test]
    fn array_deque_field_layout_valid() {
        assert_eq!(AD_FIELD_DATA, 0);
        assert_eq!(AD_FIELD_HEAD, 1);
        assert_eq!(AD_FIELD_TAIL, 2);
        // `AD_FIELD_SIZE` is no longer a field: it is ONE PAST the last slot,
        // and the assertion says exactly that. Slots 0..2 are the REAL
        // `java.util.ArrayDeque` layout (`elements`/`head`/`tail`, in that
        // declaration order, confirmed with `javap -p java.util.ArrayDeque` on
        // Temurin 25.0.3), `synthetic_stub_fields` declares those same three,
        // and the count is derived from `head`/`tail`. That is why real
        // bytecode reading this receiver sees a coherent deque at all — and
        // why `head`/`tail` have to obey the JDK's invariant.
        // The companion half -- that `synthetic_stub_fields` declares exactly
        // these three for `java/util/ArrayDeque` and not a fourth -- is
        // asserted in `vm/tests/tier1_tests.rs`, which can see the classloading
        // crate; this one cannot depend on it.
        assert_eq!(AD_FIELD_SIZE, AD_FIELD_TAIL + 1);
        assert_eq!(AD_DEFAULT_CAPACITY, 16);
        // The array is `AD_DEFAULT_CAPACITY + 1` slots, never a power of two,
        // and that is deliberate: the JDK's own `ArrayDeque()` allocates
        // `new Object[16 + 1]` so that `head == tail` can mean empty and only
        // empty. The former assertion here required a power of two, which read
        // as a claim that the index arithmetic masks — it does not. Every site
        // in this file uses `% cap`, so a non-power-of-two length is correct as
        // well as required. Assert the shape that is actually load-bearing.
        assert!(
            !(AD_DEFAULT_CAPACITY + 1).is_power_of_two(),
            "the allocated ring buffer is CAPACITY + 1 (the JDK's spare slot), \
             so it is not a power of two and no site may mask instead of `%`"
        );
    }

    #[test]
    fn priority_queue_field_layout_valid() {
        assert_eq!(PQ_FIELD_DATA, 0);
        assert_eq!(PQ_FIELD_SIZE, 1);
        assert_eq!(PQ_FIELD_COMPARATOR, 2);
        assert_eq!(
            PQ_DEFAULT_CAPACITY, 11,
            "PQ default capacity should be 11 (matches JDK)"
        );
    }

    #[test]
    fn hashset_delegates_to_hashmap() {
        // HashSet uses a single field for its backing HashMap
        assert_eq!(HS_FIELD_MAP, 0);
        assert_eq!(HS_NUM_FIELDS, 1);
    }

    // -----------------------------------------------------------------------
    // map_bucket_index edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn map_bucket_index_negative_hash() {
        // Negative hashes should still produce valid indices
        for hash in [-1, -100, -1000, i32::MIN, i32::MIN + 1] {
            let idx = map_bucket_index(hash, 16);
            assert!(
                idx < 16,
                "negative hash={hash} produced out-of-range idx={idx}"
            );
        }
    }

    #[test]
    fn map_bucket_index_max_int_hash() {
        let idx = map_bucket_index(i32::MAX, 16);
        assert!(idx < 16);
        assert_eq!(idx, 15, "i32::MAX & 15 should equal 15");
    }

    #[test]
    fn map_bucket_index_zero_hash() {
        assert_eq!(map_bucket_index(0, 16), 0);
        assert_eq!(map_bucket_index(0, 32), 0);
        assert_eq!(map_bucket_index(0, 1), 0);
    }

    // -----------------------------------------------------------------------
    // values_equal edge cases
    // -----------------------------------------------------------------------

    // Same species as the four repaired above: each of these used to assert a
    // `matches!` pattern against a `Value` literal built in the line before it,
    // so `values_equal` was never called. They now go through the production
    // function; the cases below are the ones the four above do NOT cover.

    #[test]
    fn values_equal_int_boundaries() {
        let ctx = lbq_blocking_tests::MockCtx::new(1);
        // i32::MIN is the value `-x == x` fixed point; a comparison written as
        // `a.abs() == b.abs()` or via a widening-to-u32 cast would confuse the
        // two extremes.
        assert!(values_equal(
            &ctx,
            &Value::Int(i32::MIN),
            &Value::Int(i32::MIN)
        ));
        assert!(!values_equal(
            &ctx,
            &Value::Int(i32::MIN),
            &Value::Int(i32::MAX)
        ));
        // i32::MIN widened to i64 is NOT 0x8000_0000.
        assert!(values_equal(
            &ctx,
            &Value::Int(i32::MIN),
            &Value::Long(i32::MIN as i64)
        ));
        assert!(!values_equal(
            &ctx,
            &Value::Int(i32::MIN),
            &Value::Long(0x8000_0000)
        ));
    }

    #[test]
    fn values_equal_long_boundaries() {
        let ctx = lbq_blocking_tests::MockCtx::new(1);
        assert!(values_equal(
            &ctx,
            &Value::Long(i64::MIN),
            &Value::Long(i64::MIN)
        ));
        assert!(!values_equal(
            &ctx,
            &Value::Long(i64::MIN),
            &Value::Long(i64::MAX)
        ));
        // A Long outside int range never equals an Int, however it is narrowed:
        // `i64::MAX as i32` is -1, so an `as i32` comparison would say true.
        assert!(!values_equal(&ctx, &Value::Long(i64::MAX), &Value::Int(-1)));
        assert!(!values_equal(&ctx, &Value::Long(i64::MIN), &Value::Int(0)));
    }

    #[test]
    fn values_equal_null_vs_some() {
        let ctx = lbq_blocking_tests::MockCtx::new(1);
        // A null reference matches no primitive, in either order — otherwise
        // `list.contains(null)` answers true for a list of boxed zeroes.
        for other in [
            Value::Int(0),
            Value::Long(0),
            Value::Float(0.0),
            Value::Double(0.0),
        ] {
            assert!(
                !values_equal(&ctx, &Value::Object(None), &other),
                "null must not equal {other:?}"
            );
            assert!(
                !values_equal(&ctx, &other, &Value::Object(None)),
                "{other:?} must not equal null (symmetry)"
            );
        }
    }

    #[test]
    fn values_equal_zero_int() {
        let ctx = lbq_blocking_tests::MockCtx::new(1);
        assert!(values_equal(&ctx, &Value::Int(0), &Value::Int(0)));
        // Java `Float.equals`/`Double.equals` use bit patterns: +0.0 and -0.0
        // are DISTINCT, and NaN equals itself. A plain `==` inverts both.
        assert!(!values_equal(
            &ctx,
            &Value::Double(0.0),
            &Value::Double(-0.0)
        ));
        assert!(!values_equal(&ctx, &Value::Float(0.0), &Value::Float(-0.0)));
        assert!(values_equal(
            &ctx,
            &Value::Double(f64::NAN),
            &Value::Double(f64::NAN)
        ));
        assert!(values_equal(
            &ctx,
            &Value::Float(f32::NAN),
            &Value::Float(f32::NAN)
        ));
    }

    // -----------------------------------------------------------------------
    // ArrayList method completeness (addAll, subList, etc.)
    // -----------------------------------------------------------------------

    /// `HashMap.resize()`'s closing line, which this VM never ran.
    ///
    /// The row that found it: `new HashSet<>(List.of("a","b","c"))` gives its
    /// backing map a 16-bucket table, and HotSpot leaves `threshold = 12`
    /// while this VM left the 16 its constructor had parked there to mean
    /// "the size the first resize will allocate".
    #[test]
    fn resize_threshold_is_capacity_times_load_factor() {
        // The measured row.
        assert_eq!(resize_threshold(16, 0.75), 12);
        // The doubling chain a growing map walks.
        assert_eq!(resize_threshold(32, 0.75), 24);
        assert_eq!(resize_threshold(64, 0.75), 48);
        // `new HashMap<>(n, lf)` with a factor of its own. The JDK truncates,
        // it does not round: 16 * 0.6 is 9.6 and HotSpot stores 9.
        assert_eq!(resize_threshold(16, 0.6), 9);
        assert_eq!(resize_threshold(16, 1.0), 16);
        // A factor above 1 is legal in the JDK (`HashMap(int, float)` only
        // rejects <= 0 and NaN) and produces a threshold above the table size,
        // which is exactly what HotSpot does.
        assert_eq!(resize_threshold(16, 2.0), 32);
    }

    /// A load factor that cannot produce a threshold falls back to the JDK
    /// default rather than writing a nonsense one.
    ///
    /// `map_ctor_capacity_load_check` refuses these at the constructor, so
    /// reaching the resize with one means the field was written by bytecode
    /// this VM did not screen. Writing `NaN as i32` there would store 0 and
    /// make the map resize on every single insert.
    #[test]
    fn a_load_factor_that_is_not_one_falls_back_to_the_jdk_default() {
        assert_eq!(resize_threshold(16, f32::NAN), 12);
        assert_eq!(resize_threshold(16, 0.0), 12);
        assert_eq!(resize_threshold(16, -1.0), 12);
        assert_eq!(resize_threshold(16, f32::INFINITY), 12);
        // And a zero-width table is a zero threshold, not a negative one.
        assert_eq!(resize_threshold(0, 0.75), 0);
        assert_eq!(resize_threshold(-8, 0.75), 0);
    }

    /// The premise `try_delegate_real_collection` was written on, asserted
    /// against the registry rather than restated in a comment.
    ///
    /// That doc comment used to claim `invoke_special` "does an exact
    /// per-class native lookup (which finds nothing for these real classes)".
    /// It finds nothing for `Collections$SingletonList`, which is what the
    /// sentence was written about. It finds a native for every
    /// [`SET_VIEW_CARRIERS`] entry, because this VM registers those under the
    /// REAL JDK class name — so the helper re-found the native that called it,
    /// tripped its own re-entrancy guard, and returned the sentinel it exists
    /// to avoid.
    ///
    /// If a future change stops registering `size`/`isEmpty` on these classes,
    /// this test goes red and the `class_declares_method` branch in the helper
    /// can be simplified back. That is the compile-time link the comment was
    /// not.
    #[test]
    fn every_set_view_carrier_has_the_natives_that_falsified_the_delegate_premise() {
        let r = build_registry();
        for c in SET_VIEW_CARRIERS {
            assert!(
                r.find(c, "size", "()I").is_some(),
                "{c} has no `size()I` native — the premise \
                 `try_delegate_real_collection` guards against no longer holds \
                 for it, and the guard can be revisited."
            );
            assert!(
                r.find(c, "isEmpty", "()Z").is_some(),
                "{c} has no `isEmpty()Z` native — same as above."
            );
        }
    }

    #[test]
    fn arraylist_extended_methods_registered() {
        let r = build_registry();
        let c = "java/util/ArrayList";
        assert!(
            r.find(c, "add", "(ILjava/lang/Object;)V").is_some(),
            "AL add(I,O)"
        );
        assert!(
            r.find(c, "remove", "(Ljava/lang/Object;)Z").is_some(),
            "AL remove(O)"
        );
        assert!(
            r.find(c, "lastIndexOf", "(Ljava/lang/Object;)I").is_some(),
            "AL lastIndexOf"
        );
        assert!(
            r.find(c, "toArray", "()[Ljava/lang/Object;").is_some(),
            "AL toArray"
        );
        assert!(
            r.find(c, "toString", "()Ljava/lang/String;").is_some(),
            "AL toString"
        );
        assert!(
            r.find(c, "addAll", "(Ljava/util/Collection;)Z").is_some(),
            "AL addAll"
        );
        assert!(
            r.find(c, "subList", "(II)Ljava/util/List;").is_some(),
            "AL subList"
        );
        assert!(r.find(c, "hashCode", "()I").is_some(), "AL hashCode");
        assert!(
            r.find(c, "equals", "(Ljava/lang/Object;)Z").is_some(),
            "AL equals"
        );
    }

    #[test]
    fn arraylist_functional_methods_registered() {
        let r = build_registry();
        let c = "java/util/ArrayList";
        assert!(
            r.find(c, "forEach", "(Ljava/util/function/Consumer;)V")
                .is_some(),
            "AL forEach"
        );
        assert!(
            r.find(c, "sort", "(Ljava/util/Comparator;)V").is_some(),
            "AL sort"
        );
        assert!(
            r.find(c, "removeIf", "(Ljava/util/function/Predicate;)Z")
                .is_some(),
            "AL removeIf"
        );
        assert!(
            r.find(c, "replaceAll", "(Ljava/util/function/UnaryOperator;)V")
                .is_some(),
            "AL replaceAll"
        );
        assert!(
            r.find(c, "stream", "()Ljava/util/stream/Stream;").is_some(),
            "AL stream"
        );
    }

    /// Every [`SET_VIEW_CARRIERS`] entry must have a `removeIf` body.
    ///
    /// Both force-native gates (`force_native_over_real_jdk_bytecode` and its
    /// `vm_exec` twin) list `removeIf` for these classes, and a gate entry with
    /// nothing registered behind it does not throw — it DECLINES and the call
    /// runs the real JDK bytecode. On `ConcurrentHashMap$EntrySetView`, the one
    /// carrier that declares `removeIf` itself, that bytecode is
    /// `return map.removeEntryIf(f)` over a `map` field a CratonVM-minted view
    /// leaves null, so every `chm.entrySet().removeIf(...)` threw NPE. It cost
    /// 66 of the 88 FAIL classes common to all three GC variants of the
    /// 2026-08-19 Spring Framework sweep. Behavioural oracle:
    /// `probes/ViewRemoveIfProbe.expected.txt`.
    #[test]
    fn set_view_carrier_remove_if_registered() {
        let r = build_registry();
        for c in SET_VIEW_CARRIERS {
            assert!(
                r.find(c, "removeIf", "(Ljava/util/function/Predicate;)Z")
                    .is_some(),
                "{c} has a removeIf force-native gate entry with no body behind it"
            );
        }
    }

    /// The set carriers' remaining mutators, for the same reason — each is
    /// listed by both gates, so each needs a body or the gate silently hands
    /// the call back to real JDK bytecode written against a null `this$0`.
    #[test]
    fn set_view_carrier_mutators_registered() {
        let r = build_registry();
        for c in SET_VIEW_CARRIERS {
            for (m, d) in [
                ("remove", "(Ljava/lang/Object;)Z"),
                ("clear", "()V"),
                ("addAll", "(Ljava/util/Collection;)Z"),
                ("removeAll", "(Ljava/util/Collection;)Z"),
                ("retainAll", "(Ljava/util/Collection;)Z"),
            ] {
                assert!(r.find(c, m, d).is_some(), "{c}.{m}{d} unregistered");
            }
        }
    }

    #[test]
    fn map_for_each_interface_native_registered() {
        let r = build_registry();
        assert!(
            r.find(
                "java/util/Map",
                "forEach",
                "(Ljava/util/function/BiConsumer;)V"
            )
            .is_some(),
            "Map.forEach must have a native bridge for immutable-map snapshots"
        );
    }

    // -----------------------------------------------------------------------
    // HashMap extended methods
    // -----------------------------------------------------------------------

    #[test]
    fn hashmap_iterator_and_views_registered() {
        let r = build_registry();
        let c = "java/util/HashMap";
        assert!(
            r.find(c, "keySet", "()Ljava/util/Set;").is_some(),
            "HM keySet"
        );
        assert!(
            r.find(c, "values", "()Ljava/util/Collection;").is_some(),
            "HM values"
        );
        assert!(
            r.find(c, "entrySet", "()Ljava/util/Set;").is_some(),
            "HM entrySet"
        );
    }

    // -----------------------------------------------------------------------
    // MAP_MAX_CAPACITY test
    // -----------------------------------------------------------------------

    #[test]
    fn map_max_capacity_is_power_of_two() {
        assert!(MAP_MAX_CAPACITY > 0);
        assert!((MAP_MAX_CAPACITY as u32).is_power_of_two());
        assert_eq!(MAP_MAX_CAPACITY, 1 << 30);
    }

    // -----------------------------------------------------------------------
    // Iterator field layout
    // -----------------------------------------------------------------------

    #[test]
    fn iterator_field_layout_valid() {
        assert_eq!(AL_ITR_FIELD_LIST, 0);
        assert_eq!(AL_ITR_FIELD_CURSOR, 1);
        assert_eq!(AL_ITR_FIELD_LAST_RET, 2);
        assert_eq!(AL_ITR_NUM_FIELDS, 3);
        // The three field slots must be pairwise distinct -- this is
        // exactly the invariant a prior version of this layout violated
        // (LAST_RET's fallback collided with CURSOR).
        assert_ne!(AL_ITR_FIELD_CURSOR, AL_ITR_FIELD_LAST_RET);
        assert_ne!(AL_ITR_FIELD_LIST, AL_ITR_FIELD_LAST_RET);
        assert_ne!(AL_ITR_FIELD_LIST, AL_ITR_FIELD_CURSOR);
    }

    /// JIT round 16 wave 8 (lane collections): the `java/util/Iterator.next`
    /// interface native over a PRIMITIVE snapshot (the `--compatible`
    /// `PrimitiveIterator$OfInt` from `IntStream.iterator`) hands out a boxed
    /// element, not the raw `Value::Int` the caller read as `null`.
    #[test]
    fn snapshot_itr_next_boxes_a_primitive_snapshot_element() {
        let mut ctx = lbq_blocking_tests::MockCtx::new(1);
        let itr_cid = ClassId::new(41);
        ctx.define_class(itr_cid, "java/util/PrimitiveIterator$OfInt");
        ctx.define_class(ClassId::new(42), "java/lang/Integer");
        let arr = ctx.new_array(ArrayElementType::Int, 2);
        ctx.set_array_element(arr, 0, Value::Int(5));
        ctx.set_array_element(arr, 1, Value::Int(6));
        let itr = ctx.alloc_object_of(itr_cid, 2);
        ctx.set_field(itr, 0, Value::Object(Some(arr)));
        ctx.set_field(itr, 1, Value::Int(0));
        let args = [Value::Object(Some(itr))];

        let boxed = cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_SNAPSHOT_ITR_BOX_PRIMITIVE", None)],
            || native_snapshot_itr_next(&mut ctx, &args),
        );
        let Ok(Some(Value::Object(Some(o)))) = boxed else {
            panic!("element 0 was not boxed: {boxed:?}");
        };
        assert_eq!(
            ctx.class_name_of_id(ctx.class_id_of_object(o)).as_deref(),
            Some("java/lang/Integer")
        );
        assert_eq!(ctx.get_field(o, 0), Value::Int(5));

        // Kill switch: the pre-fix raw element.
        let raw = cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_SNAPSHOT_ITR_BOX_PRIMITIVE", Some("0"))],
            || native_snapshot_itr_next(&mut ctx, &args),
        );
        assert!(matches!(raw, Ok(Some(Value::Int(6)))), "{raw:?}");
    }

    /// The boxing is confined to primitive elements: an `Object[]` snapshot's
    /// reference (and a null slot) pass through unchanged.
    #[test]
    fn snapshot_itr_next_passes_reference_elements_through() {
        let mut ctx = lbq_blocking_tests::MockCtx::new(1);
        let itr_cid = ClassId::new(43);
        ctx.define_class(itr_cid, "cratonvm/test/RefSnapshotItr");
        let elem = ctx.alloc_object_of(ClassId::new(44), 1);
        let arr = ctx.new_array(ArrayElementType::Reference, 2);
        ctx.set_array_element(arr, 0, Value::Object(Some(elem)));
        ctx.set_array_element(arr, 1, Value::Object(None));
        let itr = ctx.alloc_object_of(itr_cid, 2);
        ctx.set_field(itr, 0, Value::Object(Some(arr)));
        ctx.set_field(itr, 1, Value::Int(0));
        let args = [Value::Object(Some(itr))];
        assert!(matches!(
            native_snapshot_itr_next(&mut ctx, &args),
            Ok(Some(Value::Object(Some(o)))) if o == elem
        ));
        assert!(matches!(
            native_snapshot_itr_next(&mut ctx, &args),
            Ok(Some(Value::Object(None)))
        ));
    }

    // -----------------------------------------------------------------------
    // CX6-1 (round-15 backlog Tier A, taken by JIT round 16 wave 8, lane
    // collections): the "no fabricated success" census for explicit null
    // arguments. CENSUS FIRST, as the proposal asks: it prints, it does not
    // assert. The allow-list and the assertion come after its first reading.
    // -----------------------------------------------------------------------

    /// The parameter types of a method descriptor, one string per parameter
    /// (`I`, `Ljava/util/Map;`, `[Ljava/lang/Object;` ...). `None` for a
    /// descriptor this parser does not understand.
    fn cx6_1_params(desc: &str) -> Option<Vec<String>> {
        let body = desc.strip_prefix('(')?.split(')').next()?;
        let bytes = body.as_bytes();
        let mut out = Vec::new();
        let mut i = 0;
        while i < bytes.len() {
            let start = i;
            while bytes[i] == b'[' {
                i += 1;
                if i >= bytes.len() {
                    return None;
                }
            }
            if bytes[i] == b'L' {
                i += body[i..].find(';')? + 1;
            } else {
                i += 1;
            }
            out.push(body[start..i].to_string());
        }
        Some(out)
    }

    /// The parameter types CX6-1 names: a JDK body dereferences each of these
    /// (or `requireNonNull`s it), so a native answering `Ok` for an explicit
    /// null there is a candidate fabricated success.
    fn cx6_1_null_hostile(ty: &str) -> bool {
        matches!(
            ty,
            "Ljava/util/Collection;" | "Ljava/util/Map;" | "[Ljava/lang/Object;"
        ) || ty.starts_with("Ljava/util/function/")
    }

    /// Classes whose registered natives are all `static` (no receiver slot).
    /// The registry does not record staticness, so this is a heuristic, and
    /// the census prints which reading it used.
    fn cx6_1_static(class: &str, name: &str) -> bool {
        matches!(
            class,
            "java/util/Collections"
                | "java/util/Arrays"
                | "java/util/Objects"
                | "java/util/Spliterators"
                | "java/util/stream/StreamSupport"
                | "java/util/stream/Collectors"
        ) || (matches!(class, "java/util/List" | "java/util/Set" | "java/util/Map")
            && matches!(name, "of" | "copyOf" | "ofEntries" | "entry"))
    }

    /// Run with `cargo test -p cratonvm-native-collections --lib
    /// cx6_1_null_argument_census -- --ignored --nocapture`. Prints one
    /// `CX6-1 OK` line per (registration, null slot) whose native returned
    /// `Ok` on a mock receiver with an explicit null in that slot, then a
    /// summary. Each call runs on its own thread with a fresh `MockCtx`, so a
    /// native that panics on the mock is counted `panic` and one that blocks
    /// (a queue `take`) is counted `hang` after 2 s and left behind.
    #[test]
    #[ignore = "census (CX6-1): prints candidates, asserts nothing"]
    fn cx6_1_null_argument_census() {
        use std::sync::mpsc;
        use std::time::Duration;
        let r = build_registry();
        let (mut ok, mut threw, mut panicked, mut hung, mut skipped) = (0, 0, 0, 0, 0);
        let mut seen = std::collections::HashSet::new();
        for (class, name, desc, _kind) in r.dump_registrations() {
            // A re-registered triple has one slot and several rows.
            if !seen.insert((class, name, desc)) {
                continue;
            }
            let Some(params) = cx6_1_params(desc) else {
                skipped += 1;
                continue;
            };
            let Some(cb) = r
                .resolve_id(class, name, desc)
                .and_then(|id| r.callback_of(id))
            else {
                skipped += 1;
                continue;
            };
            let is_static = cx6_1_static(class, name);
            for (slot, ty) in params.iter().enumerate() {
                if !cx6_1_null_hostile(ty) {
                    continue;
                }
                let (class_s, params_c) = (class.to_string(), params.clone());
                let (tx, rx) = mpsc::channel();
                std::thread::spawn(move || {
                    let mut ctx = lbq_blocking_tests::MockCtx::new(1);
                    let recv_cid = ClassId::new(901);
                    ctx.define_class(recv_cid, &class_s);
                    let mut args = Vec::new();
                    if !is_static {
                        args.push(Value::Object(Some(ctx.alloc_object_of(recv_cid, 16))));
                    }
                    for (i, p) in params_c.iter().enumerate() {
                        args.push(match p.as_str() {
                            _ if i == slot => Value::Object(None),
                            "J" => Value::Long(0),
                            "F" => Value::Float(0.0),
                            "D" => Value::Double(0.0),
                            "Z" | "B" | "C" | "S" | "I" => Value::Int(0),
                            _ => Value::Object(Some(ctx.alloc_object_of(ClassId::new(902), 16))),
                        });
                    }
                    let verdict = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        cb(&mut ctx, &args).is_ok()
                    }));
                    let _ = tx.send(verdict);
                });
                match rx.recv_timeout(Duration::from_secs(2)) {
                    Ok(Ok(true)) => {
                        ok += 1;
                        let reading = if is_static { "static" } else { "instance" };
                        println!("CX6-1 OK {class}.{name}{desc} null-slot={slot} ({reading})");
                    }
                    Ok(Ok(false)) => threw += 1,
                    Ok(Err(_)) => panicked += 1,
                    Err(_) => hung += 1,
                }
            }
        }
        println!(
            "CX6-1 summary: ok={ok} threw={threw} panic={panicked} hang={hung} unparsed-or-unresolved={skipped}"
        );
    }

    // -----------------------------------------------------------------------
    // Properties registration (via collections extras)
    // -----------------------------------------------------------------------

    #[test]
    fn properties_registered() {
        let r = build_registry();
        let c = "java/util/Properties";
        assert!(r.find(c, "<init>", "()V").is_some(), "Props <init>");
    }

    // -----------------------------------------------------------------------
    // ConcurrentSkipListMap registration
    // -----------------------------------------------------------------------

    #[test]
    fn concurrent_skip_list_map_not_intercepted() {
        // ConcurrentSkipListMap interception is intentionally DISABLED — the
        // native "sorted-array" overlay hardcoded natural ordering and ignored
        // the (Comparator) ctor, silently dropping puts for non-Comparable keys
        // (broke the Gradle test worker's serializer registry). We now run the
        // real java.util.concurrent.ConcurrentSkipListMap bytecode instead (see
        // the `let _ = register_concurrent_skip_list_map_natives;` no-op call
        // site). This test guards that the natives stay unregistered so the real
        // class is not shadowed again. (Previously asserted the opposite and had
        // failed since the natives were disabled.)
        let r = build_registry();
        let c = "java/util/concurrent/ConcurrentSkipListMap";
        assert!(
            r.find(c, "<init>", "()V").is_none(),
            "CSLM <init> must NOT be intercepted"
        );
        assert!(
            r.find(
                c,
                "put",
                "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;"
            )
            .is_none(),
            "CSLM put must NOT be intercepted"
        );
    }

    // ===================================================================
    // task #15: blocking-queue park/notify discipline.
    //
    // The tests below drive the production `native_lbq_put_blocking` /
    // `native_lbq_poll` / `native_lbq_take_blocking` against a minimal
    // `NativeContext` mock whose monitor primitives implement true
    // Java-style wait/notify semantics (recursive ownership, lock release
    // on wait, condvar-driven wakeups). That's the only way to validate
    // that a producer parked at capacity actually wakes when a consumer
    // drains the queue.
    // ===================================================================

    mod lbq_blocking_tests {
        use super::super::*;
        use cratonvm_native_api::{
            AnnotationData, AnnotationElementValue, FieldMetadata, MethodMetadata, StackTraceEntry,
        };
        #[allow(unused_imports)]
        use cratonvm_native_api::{
            NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
            NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
        };
        use cratonvm_types::error::MethodCallFailed;
        use std::collections::HashMap;
        use std::sync::{Arc, Condvar, Mutex};
        use std::time::Duration;

        // ---- Per-object Java-style monitor ----------------------------
        struct MonitorState {
            owner: Option<u64>,
            count: u32,
        }

        struct ObjMonitor {
            lock: Mutex<MonitorState>,
            cvar: Condvar,
        }

        impl ObjMonitor {
            fn new() -> Arc<Self> {
                Arc::new(ObjMonitor {
                    lock: Mutex::new(MonitorState {
                        owner: None,
                        count: 0,
                    }),
                    cvar: Condvar::new(),
                })
            }

            fn enter(&self, tid: u64) {
                let mut st = self.lock.lock().unwrap();
                while !(st.owner.is_none() || st.owner == Some(tid)) {
                    st = self.cvar.wait(st).unwrap();
                }
                st.owner = Some(tid);
                st.count += 1;
            }

            fn exit(&self, tid: u64) {
                let mut st = self.lock.lock().unwrap();
                debug_assert_eq!(st.owner, Some(tid), "monitor_exit by non-owner");
                st.count -= 1;
                if st.count == 0 {
                    st.owner = None;
                    self.cvar.notify_all();
                }
            }

            fn wait(&self, tid: u64, timeout: Option<u64>) {
                let mut st = self.lock.lock().unwrap();
                debug_assert_eq!(st.owner, Some(tid), "monitor_wait by non-owner");
                let saved_count = st.count;
                st.owner = None;
                st.count = 0;
                self.cvar.notify_all();
                let timeout = timeout.unwrap_or(50);
                let (mut st2, _r) = self
                    .cvar
                    .wait_timeout(st, Duration::from_millis(timeout))
                    .unwrap();
                while !(st2.owner.is_none() || st2.owner == Some(tid)) {
                    st2 = self.cvar.wait(st2).unwrap();
                }
                st2.owner = Some(tid);
                st2.count = saved_count;
            }

            fn notify_all(&self) {
                self.cvar.notify_all();
            }
        }

        // ---- Heap entries -------------------------------------------
        enum HeapEntry {
            Object { fields: Vec<Value> },
            Array { elements: Vec<Value> },
        }

        /// Shared heap + monitor state, behind a single `Mutex` so the
        /// `&self` and `&mut self` trait methods can be called from
        /// multiple OS threads without UB. The native methods we test
        /// hold the per-object monitor across the heap access, but
        /// `alloc_ref_array` (called from `lbq_ensure_capacity`) does
        /// not — so we serialise the heap itself with this lock.
        struct Shared {
            heap: Vec<HeapEntry>,
            ptr_to_index: HashMap<usize, usize>,
            object_classes: HashMap<usize, ClassId>,
            class_names: HashMap<ClassId, String>,
            /// Modelled superclass edges, so a `MockCtx` can present a real
            /// class hierarchy to `classify_class`. Empty by default, which
            /// reproduces the previous always-`None` `superclass_of` stub.
            superclasses: HashMap<ClassId, ClassId>,
            /// Modelled `(class, field name) -> absolute slot`, so a `MockCtx`
            /// can present a REAL field layout to the by-name resolvers.
            /// Empty by default, which reproduces the previous always-`None`
            /// `resolve_field_index_by_class_id` stub — a class nothing has
            /// declared a field for is exactly a fabricated stub, whose slots
            /// are `_f0.._fN`.
            field_indices: HashMap<(ClassId, String), usize>,
            /// Modelled `class -> total instance field count (inherited
            /// included)`, so a `MockCtx` can answer `class_num_total_fields`.
            /// Empty by default, which reproduces the trait's `0` default —
            /// i.e. "class not loaded", the answer `view_carrier_slots` treats
            /// as "no declared field to collide with".
            total_fields: HashMap<ClassId, usize>,
            /// Distinct per `Shared`, so two `MockCtx`s in one process are two
            /// different "VMs".
            ///
            /// `NativeContext::vm_identity` defaults to `0` for every mock,
            /// which is fine while nothing caches against it — but the
            /// per-`ClassId` classification memo does, and it is thread-local.
            /// Two tests running on the same thread with different
            /// `ClassId`-to-name mappings would otherwise read each other's
            /// cached classifications. Minted from a process-global counter
            /// rather than the `Arc` address, because a dropped `Shared`'s
            /// address can be handed straight to the next one.
            vm_id: usize,
            next_ptr: usize,
            monitors: HashMap<usize, Arc<ObjMonitor>>,
            /// Modelled `java.lang.String` content, as UTF-16 code UNITS.
            ///
            /// Empty by default, which reproduces the previous always-`None`
            /// `read_string` stub: an object nothing called `define_string` for
            /// is simply not a String. Units rather than `String` because the
            /// whole point of the tests that use it is content a Rust `str`
            /// cannot hold.
            strings: HashMap<usize, Vec<u16>>,
            /// Modelled static fields `(class, name, value)`; the index into
            /// this vector is the static slot. Empty by default, which
            /// reproduces the previous stubs (no static resolves by name, and
            /// every read answers `Int(0)`).
            statics: Vec<(ClassId, String, Value)>,
            /// Modelled own (non-bridge) method declarations `(class, name,
            /// descriptor)`, so a `MockCtx` can answer
            /// `class_declares_method`. Empty by default, which reproduces the
            /// trait's always-`false` default.
            declared_methods: Vec<(ClassId, String, String)>,
        }

        impl Shared {
            fn new() -> Self {
                static NEXT_VM_ID: std::sync::atomic::AtomicUsize =
                    std::sync::atomic::AtomicUsize::new(1);
                Shared {
                    heap: Vec::new(),
                    ptr_to_index: HashMap::new(),
                    object_classes: HashMap::new(),
                    class_names: HashMap::new(),
                    superclasses: HashMap::new(),
                    field_indices: HashMap::new(),
                    total_fields: HashMap::new(),
                    vm_id: NEXT_VM_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
                    next_ptr: 8,
                    monitors: HashMap::new(),
                    strings: HashMap::new(),
                    statics: Vec::new(),
                    declared_methods: Vec::new(),
                }
            }

            fn alloc_entry(&mut self, entry: HeapEntry) -> ObjectRef {
                let idx = self.heap.len();
                self.heap.push(entry);
                let ptr = self.next_ptr;
                self.next_ptr += 8;
                self.ptr_to_index.insert(ptr, idx);
                unsafe { ObjectRef::from_raw(ptr as *mut u8) }
            }

            fn entry_index(&self, obj: ObjectRef) -> usize {
                let ptr = obj.as_ptr() as usize;
                *self
                    .ptr_to_index
                    .get(&ptr)
                    .expect("invalid ObjectRef in MockCtx")
            }

            fn monitor_for(&mut self, obj: ObjectRef) -> Arc<ObjMonitor> {
                let ptr = obj.as_ptr() as usize;
                self.monitors
                    .entry(ptr)
                    .or_insert_with(ObjMonitor::new)
                    .clone()
            }
        }

        pub(super) struct MockCtx {
            shared: Arc<Mutex<Shared>>,
            thread_id: u64,
            /// r9w12: does this context implement
            /// `NativeSystemAccess::fast_unbox_primitive_wrapper` (and with it
            /// the fused `class_id_and_unboxed_primitive` door the real VM
            /// overrides)?
            ///
            /// Default `false`, which is what this mock has always been, so
            /// every existing test keeps taking the name-ladder route through
            /// `unbox_wrapper`. Flipping it on is how a test reaches the arm a
            /// real `NativeContextImpl` takes -- the two are different code
            /// paths in `chm_wrapper_chain_walk` and `chm_get_wrapper_key_fast`
            /// and the point of the pair is that they answer identically.
            fast_unbox_door: bool,
            /// nio21 w1 L4: does this context answer `is_jdk_only` (and with
            /// it the trait-default `aligned`) `true`? Default `false`, what
            /// this mock has always answered.
            jdk_only: bool,
            /// j27 CD16-1: what `NativeExceptionAccess::null_arg_message`
            /// answers (the line the JDK method's bytecode yields). Default
            /// `None`, the trait default: the query declines and the natives
            /// keep their transcribed literal.
            null_arg_answer: Option<String>,
        }

        impl MockCtx {
            pub(super) fn new(thread_id: u64) -> Self {
                MockCtx {
                    shared: Arc::new(Mutex::new(Shared::new())),
                    thread_id,
                    fast_unbox_door: false,
                    jdk_only: false,
                    null_arg_answer: None,
                }
            }

            /// A context whose bytecode query answers `answer` for every
            /// null-argument site ([`MockCtx::null_arg_answer`]).
            pub(super) fn with_null_arg_answer(mut self, answer: &str) -> Self {
                self.null_arg_answer = Some(answer.to_string());
                self
            }

            /// A `--jdk-only` context: every alignment row in force, so the
            /// JDK `ConcurrentHashMap` model too (`chm_jdk_model`).
            pub(super) fn with_jdk_only(mut self) -> Self {
                self.jdk_only = true;
                self
            }

            /// Turn the VM-style [`MockCtx::fast_unbox_door`] on for this
            /// context. See that field.
            pub(super) fn with_fast_unbox_door(mut self) -> Self {
                self.fast_unbox_door = true;
                self
            }

            /// Sibling `MockCtx` sharing the same heap/monitor state but
            /// reporting a different `thread_id`. Intended for use on a
            /// separate OS thread — each call into the trait re-acquires
            /// the shared `Mutex<Shared>` briefly, so the two contexts
            /// can run concurrently without UB.
            pub(super) fn fork(&self, new_thread_id: u64) -> Self {
                MockCtx {
                    shared: Arc::clone(&self.shared),
                    thread_id: new_thread_id,
                    fast_unbox_door: self.fast_unbox_door,
                    jdk_only: self.jdk_only,
                    null_arg_answer: self.null_arg_answer.clone(),
                }
            }

            pub(super) fn alloc_lbq_object(&self) -> ObjectRef {
                let mut s = self.shared.lock().unwrap();
                s.alloc_entry(HeapEntry::Object {
                    fields: vec![Value::Int(0); 4],
                })
            }

            pub(super) fn define_class(&self, class_id: ClassId, name: &str) {
                self.shared
                    .lock()
                    .unwrap()
                    .class_names
                    .insert(class_id, name.to_string());
            }

            /// Allocate a modelled `java.lang.String` holding exactly `units`.
            ///
            /// The units are the point: a test that builds its fixture from a
            /// Rust `&str` cannot express an unpaired surrogate at all, and a
            /// test that then reads it back through `read_string` PASSES ON THE
            /// BROKEN CODE, because `U+FFFD == U+FFFD`.
            pub(super) fn define_string(&self, units: &[u16]) -> ObjectRef {
                let cid = ClassId::new(9_001);
                self.define_class(cid, "java/lang/String");
                let mut s = self.shared.lock().unwrap();
                // Two fields, so `unbox_wrapper`'s one-field probe cannot
                // mistake a String for a boxed primitive.
                let obj = s.alloc_entry(HeapEntry::Object {
                    fields: vec![Value::Int(0); 2],
                });
                let ptr = obj.as_ptr() as usize;
                s.object_classes.insert(ptr, cid);
                s.strings.insert(ptr, units.to_vec());
                obj
            }

            /// Record that `class_id` itself declares `name` + `descriptor`,
            /// so `class_declares_method` answers `true` for it.
            pub(super) fn define_declared_method(
                &self,
                class_id: ClassId,
                name: &str,
                descriptor: &str,
            ) {
                self.shared.lock().unwrap().declared_methods.push((
                    class_id,
                    name.to_string(),
                    descriptor.to_string(),
                ));
            }

            /// Record `child extends parent`, so `superclass_of` walks.
            pub(super) fn define_superclass(&self, child: ClassId, parent: ClassId) {
                self.shared
                    .lock()
                    .unwrap()
                    .superclasses
                    .insert(child, parent);
            }

            /// Declare that `class_id` (or a subclass of it) has instance field
            /// `name` at absolute slot `index`, so
            /// `resolve_field_index_by_class_id` answers like a real layout.
            /// A class with no declared field of that name is a fabricated
            /// stub as far as the resolver is concerned.
            pub(super) fn define_field(&self, class_id: ClassId, name: &str, index: usize) {
                self.shared
                    .lock()
                    .unwrap()
                    .field_indices
                    .insert((class_id, name.to_string()), index);
            }

            /// Declare static field `name` of `class_id` holding `value`, so
            /// `static_field_index_by_name` / `get_static_field` answer it.
            pub(super) fn define_static(&self, class_id: ClassId, name: &str, value: Value) {
                self.shared
                    .lock()
                    .unwrap()
                    .statics
                    .push((class_id, name.to_string(), value));
            }

            /// Declare `class_id`'s TOTAL instance field count, inherited
            /// fields included — what `NativeClassAccess::class_num_total_fields`
            /// reports for a loaded class. Without this the mock answers the
            /// trait default of `0`, which means "not loaded".
            pub(super) fn define_total_fields(&self, class_id: ClassId, n: usize) {
                self.shared.lock().unwrap().total_fields.insert(class_id, n);
            }

            /// Allocate an object of `class_id` with `num_fields` slots.
            pub(super) fn alloc_object_of(
                &self,
                class_id: ClassId,
                num_fields: usize,
            ) -> ObjectRef {
                let mut s = self.shared.lock().unwrap();
                let obj = s.alloc_entry(HeapEntry::Object {
                    fields: vec![Value::Int(0); num_fields],
                });
                s.object_classes.insert(obj.as_ptr() as usize, class_id);
                obj
            }

            /// Model `ClassStore::remove`: the class becomes invisible to
            /// `class_name_of_id` / `class_id_by_name`, and — crucially — its
            /// `ClassId` is NOT handed back out, exactly as the tombstoned slot
            /// vector guarantees in `classloading/src/class.rs`.
            pub(super) fn unload_class(&self, class_id: ClassId) {
                let mut s = self.shared.lock().unwrap();
                s.class_names.remove(&class_id);
                s.superclasses.remove(&class_id);
            }

            /// gc-common w26-a: allocate an object of `class_id` AT `addr`.
            ///
            /// This mock's identity hash is the address truncated to `i32`, so
            /// two objects `1 << 32` bytes apart share an identity hash (a
            /// live collider), and moving an object by a multiple of `1 << 32`
            /// keeps its hash (a real, hash-preserving move). The addresses are
            /// never dereferenced. Later plain allocations continue above
            /// `addr` if it lies past the current bump pointer.
            pub(super) fn w26a_alloc_object_at(
                &self,
                class_id: ClassId,
                num_fields: usize,
                addr: usize,
            ) -> ObjectRef {
                assert!(addr % 8 == 0 && addr >= 8, "an aligned, non-null address");
                let mut s = self.shared.lock().unwrap();
                let saved = s.next_ptr;
                s.next_ptr = addr;
                let obj = s.alloc_entry(HeapEntry::Object {
                    fields: vec![Value::Int(0); num_fields],
                });
                s.next_ptr = saved.max(addr + 8);
                s.object_classes.insert(obj.as_ptr() as usize, class_id);
                obj
            }
        }

        impl cratonvm_native_api::NativeClassAccess for MockCtx {
            /// Report the TOTAL instance field count a test declared with
            /// `define_total_fields`. `0` — the trait's own default — means
            /// "class not loaded", which is exactly what an undeclared class
            /// is here, so every pre-existing test is unchanged.
            fn class_num_total_fields(&self, class_id: ClassId) -> usize {
                self.shared
                    .lock()
                    .unwrap()
                    .total_fields
                    .get(&class_id)
                    .copied()
                    .unwrap_or(0)
            }
            // --- default stubs for everything else ---------------------
            fn load_class(&mut self, _n: &str) -> MethodCallResult {
                Ok(None)
            }
            fn class_name_of_id(&self, c: ClassId) -> Option<String> {
                self.shared.lock().unwrap().class_names.get(&c).cloned()
            }
            fn class_id_of_object(&self, o: ObjectRef) -> ClassId {
                self.shared
                    .lock()
                    .unwrap()
                    .object_classes
                    .get(&(o.as_ptr() as usize))
                    .copied()
                    .unwrap_or_else(|| ClassId::new(0))
            }
            fn method_exists(&self, _c: &str, _m: &str, _d: &str) -> bool {
                false
            }
            fn ensure_class_initialized(&mut self, _n: &str) -> Result<ClassId, MethodCallFailed> {
                Ok(ClassId::new(0))
            }
            fn is_subclass(&self, _c: ClassId, _p: ClassId) -> bool {
                false
            }
            fn superclass_of(&self, c: ClassId) -> Option<ClassId> {
                if let Some(parent) = self.shared.lock().unwrap().superclasses.get(&c) {
                    return Some(*parent);
                }
                // No modelled edge: same as the previous always-`None` stub.
                None
            }
            fn class_declares_method(&self, c: ClassId, name: &str, descriptor: &str) -> bool {
                self.shared
                    .lock()
                    .unwrap()
                    .declared_methods
                    .iter()
                    .any(|(k, n, d)| *k == c && n == name && d == descriptor)
            }
            /// Reverse of `class_name_of_id` over the classes this mock has
            /// actually been told about. Previously an unconditional `None`,
            /// which made `ClassDiscriminator` unable to resolve anything and
            /// so untestable here. A name that was never `define_class`d — or
            /// that has been `unload_class`d — still answers `None`, so no
            /// existing test's expectations move.
            fn class_id_by_name(&self, n: &str) -> Option<ClassId> {
                let s = self.shared.lock().unwrap();
                s.class_names
                    .iter()
                    .find(|(_, name)| name.as_str() == n)
                    .map(|(id, _)| *id)
            }
            fn loader_id_of_class(&self, _c: ClassId) -> i32 {
                2
            }
            fn is_record_class(&self, _c: ClassId) -> bool {
                false
            }
            fn record_components(&self, _c: ClassId) -> Vec<(String, String)> {
                Vec::new()
            }
            fn is_sealed_class(&self, _c: ClassId) -> bool {
                false
            }
            fn permitted_subclasses(&self, _c: ClassId) -> Vec<String> {
                Vec::new()
            }
            fn declared_fields(&self, _c: ClassId) -> Vec<FieldMetadata> {
                Vec::new()
            }
            fn declared_methods(&self, _c: ClassId) -> Vec<MethodMetadata> {
                Vec::new()
            }
            fn class_interfaces(&self, _c: ClassId) -> Vec<ClassId> {
                Vec::new()
            }
            fn class_access_flags(&self, _c: ClassId) -> u16 {
                0
            }
            fn primitive_class_mirror(&mut self, _n: &str) -> ObjectRef {
                let mut s = self.shared.lock().unwrap();
                s.alloc_entry(HeapEntry::Object { fields: Vec::new() })
            }
            fn class_annotations(&self, _c: ClassId) -> Vec<AnnotationData> {
                Vec::new()
            }
            fn method_annotations(&self, _c: ClassId, _m: &str, _d: &str) -> Vec<AnnotationData> {
                Vec::new()
            }
            fn field_annotations(&self, _c: ClassId, _f: &str) -> Vec<AnnotationData> {
                Vec::new()
            }
            fn method_parameter_annotations(
                &self,
                _c: ClassId,
                _m: &str,
                _d: &str,
            ) -> Vec<Vec<AnnotationData>> {
                Vec::new()
            }
            fn class_signature(&self, _c: ClassId) -> Option<String> {
                None
            }
            fn method_signature(&self, _c: ClassId, _m: &str, _d: &str) -> Option<String> {
                None
            }
            fn field_signature(&self, _c: ClassId, _f: &str) -> Option<String> {
                None
            }
            fn method_annotation_default(
                &self,
                _c: ClassId,
                _m: &str,
                _d: &str,
            ) -> Option<AnnotationElementValue> {
                None
            }
            fn module_name_of_class(&self, _c: ClassId) -> Option<String> {
                None
            }
            fn find_resource(&self, _n: &str) -> Option<Vec<u8>> {
                None
            }
            fn list_application_class_names(&self) -> Vec<String> {
                Vec::new()
            }
            fn register_dynamic_classpath(&mut self, _p: &[String]) {}
            fn define_class_from_bytes(&mut self, _n: &str, _b: &[u8]) -> Option<ClassId> {
                None
            }
            fn define_class_with_loader(
                &mut self,
                _n: &str,
                _b: &[u8],
                _l: u32,
            ) -> Option<ClassId> {
                None
            }
            fn class_id_by_name_and_loader(&self, _n: &str, _l: u32) -> Option<ClassId> {
                None
            }
            fn allocate_loader_id(&mut self) -> u32 {
                0
            }
            fn is_package_exported_unqualified(&self, _m: &str, _p: &str) -> bool {
                true
            }
            fn is_package_exported_to(&self, _m: &str, _p: &str, _t: &str) -> bool {
                true
            }
            fn is_package_open_unqualified(&self, _m: &str, _p: &str) -> bool {
                true
            }
            fn is_package_open_to(&self, _m: &str, _p: &str, _t: &str) -> bool {
                true
            }
            fn check_deep_reflection_access(&self, _a: ClassId, _t: ClassId) -> Result<(), String> {
                Ok(())
            }
        }

        impl cratonvm_native_api::NativeInvokeAccess for MockCtx {
            fn invoke(&mut self, _c: &str, _m: &str, _d: &str, _a: &[Value]) -> MethodCallResult {
                Ok(None)
            }
            fn invoke_virtual(
                &mut self,
                _r: ObjectRef,
                _m: &str,
                _d: &str,
                _a: &[Value],
            ) -> MethodCallResult {
                Ok(None)
            }
        }

        impl cratonvm_native_api::NativeHeapAccess for MockCtx {
            // Pre-existing test-fixture gap (unrelated to this session's fix):
            // `c812b622` added this trait method with no default impl but
            // never updated this inline mock, breaking `cargo test -p
            // cratonvm-native-collections --lib` on `dev`. This mock has no
            // notion of class field layout, so `None` (matching the trait's
            // documented "field not found" case) is the correct stub.
            /// Resolve against the layout the test declared with
            /// `define_field`, walking `define_superclass` edges the way the
            /// real `resolve_field_index_in_hierarchy` walks a class's
            /// ancestry. A class nothing declared a field for still answers
            /// `None` — that is the fabricated-stub case — so every
            /// pre-existing test's expectations are unchanged.
            fn resolve_field_index_by_class_id(
                &self,
                class_id: ClassId,
                field_name: &str,
            ) -> Option<usize> {
                let s = self.shared.lock().unwrap();
                let mut cur = Some(class_id);
                for _ in 0..64 {
                    let c = cur?;
                    if let Some(idx) = s.field_indices.get(&(c, field_name.to_string())) {
                        return Some(*idx);
                    }
                    cur = s.superclasses.get(&c).copied();
                }
                None
            }
            fn new_array(&mut self, _et: ArrayElementType, length: usize) -> ObjectRef {
                let mut s = self.shared.lock().unwrap();
                s.alloc_entry(HeapEntry::Array {
                    elements: vec![Value::Int(0); length],
                })
            }
            fn new_ref_array(&mut self, _c: ClassId, length: usize) -> ObjectRef {
                let mut s = self.shared.lock().unwrap();
                s.alloc_entry(HeapEntry::Array {
                    elements: vec![Value::Object(None); length],
                })
            }
            fn array_length(&self, obj: ObjectRef) -> usize {
                let s = self.shared.lock().unwrap();
                let idx = s.entry_index(obj);
                match &s.heap[idx] {
                    HeapEntry::Array { elements } => elements.len(),
                    _ => 0,
                }
            }
            fn get_array_element(&self, obj: ObjectRef, index: usize) -> Value {
                let s = self.shared.lock().unwrap();
                let idx = s.entry_index(obj);
                match &s.heap[idx] {
                    HeapEntry::Array { elements } => {
                        elements.get(index).copied().unwrap_or(Value::Object(None))
                    }
                    _ => Value::Object(None),
                }
            }
            fn set_array_element(&self, obj: ObjectRef, index: usize, value: Value) {
                let mut s = self.shared.lock().unwrap();
                let idx = s.entry_index(obj);
                if let HeapEntry::Array { elements } = &mut s.heap[idx] {
                    if index < elements.len() {
                        elements[index] = value;
                    }
                }
            }
            fn get_field(&self, obj: ObjectRef, index: usize) -> Value {
                let s = self.shared.lock().unwrap();
                let idx = s.entry_index(obj);
                match &s.heap[idx] {
                    HeapEntry::Object { fields } => {
                        fields.get(index).copied().unwrap_or(Value::Int(0))
                    }
                    _ => Value::Int(0),
                }
            }
            fn set_field(&self, obj: ObjectRef, index: usize, value: Value) {
                let mut s = self.shared.lock().unwrap();
                let idx = s.entry_index(obj);
                if let HeapEntry::Object { fields } = &mut s.heap[idx] {
                    if index >= fields.len() {
                        fields.resize(index + 1, Value::Int(0));
                    }
                    fields[index] = value;
                }
            }
            fn alloc_object(&mut self, c: ClassId, num_fields: usize) -> ObjectRef {
                let mut s = self.shared.lock().unwrap();
                let obj = s.alloc_entry(HeapEntry::Object {
                    fields: vec![Value::Int(0); num_fields],
                });
                s.object_classes.insert(obj.as_ptr() as usize, c);
                obj
            }
            fn object_num_fields(&self, obj: ObjectRef) -> usize {
                let s = self.shared.lock().unwrap();
                let idx = s.entry_index(obj);
                match &s.heap[idx] {
                    HeapEntry::Object { fields } => fields.len(),
                    _ => 0,
                }
            }
            fn heap_kind_of(&self, obj: ObjectRef) -> ObjectKind {
                let s = self.shared.lock().unwrap();
                let idx = s.entry_index(obj);
                match &s.heap[idx] {
                    HeapEntry::Array { .. } => ObjectKind::Array,
                    _ => ObjectKind::Object,
                }
            }
            fn heap_element_type_of(&self, _o: ObjectRef) -> ArrayElementType {
                ArrayElementType::Reference
            }
            /// Modelled so `al_fast_state` (which asks for kind and length in
            /// one call) can be exercised here; the trait default answers
            /// `None` through `object_is_array`'s mock-default `false`.
            fn array_shape(&self, obj: ObjectRef) -> Option<(ArrayElementType, usize)> {
                let s = self.shared.lock().unwrap();
                let idx = s.entry_index(obj);
                match &s.heap[idx] {
                    HeapEntry::Array { elements } => {
                        Some((ArrayElementType::Reference, elements.len()))
                    }
                    _ => None,
                }
            }
            fn new_object(&mut self, _c: &str) -> MethodCallResult {
                Ok(None)
            }
            fn identity_hash_code(&self, o: ObjectRef) -> i32 {
                o.as_ptr() as i32
            }
            fn get_field_by_name(&self, _o: ObjectRef, _n: &str) -> Value {
                Value::Object(None)
            }
            fn set_field_by_name(&self, _o: ObjectRef, _n: &str, _v: Value) {}
            /// Resolve by NAME through the same modelled layout
            /// `resolve_field_index_by_class_id` uses. This was a `None` stub
            /// while the by-id form was fully modelled, so a class the test had
            /// just described with `define_class` + `define_field` still
            /// answered "no such field" here — which forces
            /// `al_slots`/`al_slots_for` onto their constant-fallback path and
            /// makes a layout test unable to exercise the resolved branch.
            fn resolve_field_index(&self, class_name: &str, field_name: &str) -> Option<usize> {
                let class_id = self.class_id_by_name(class_name)?;
                self.resolve_field_index_by_class_id(class_id, field_name)
            }
            fn create_string(&mut self, _t: &str) -> ObjectRef {
                let mut s = self.shared.lock().unwrap();
                s.alloc_entry(HeapEntry::Object { fields: Vec::new() })
            }
            /// LOSSY, exactly as the VM's reader is: an unpaired surrogate
            /// comes back as `U+FFFD` rather than refusing. That fidelity is
            /// what lets a test demonstrate the mangling the units-exact
            /// primitives below avoid.
            fn read_string(&self, o: ObjectRef) -> Option<String> {
                let s = self.shared.lock().unwrap();
                let units = s.strings.get(&(o.as_ptr() as usize))?;
                Some(String::from_utf16_lossy(units))
            }
            /// Answered from the modelled UNITS, as the VM answers it from the
            /// receiver's own character storage.
            fn java_string_hash_code(&self, o: ObjectRef) -> Option<i32> {
                let s = self.shared.lock().unwrap();
                let units = s.strings.get(&(o.as_ptr() as usize))?;
                Some(
                    units
                        .iter()
                        .fold(0i32, |h, &u| h.wrapping_mul(31).wrapping_add(u as i32)),
                )
            }
            /// `None` means "not compared" — an operand is not a String — which
            /// is the contract every caller in this file relies on to fall back
            /// rather than answer "not equal".
            fn java_strings_equal(&self, a: ObjectRef, b: ObjectRef) -> Option<bool> {
                let s = self.shared.lock().unwrap();
                let ua = s.strings.get(&(a.as_ptr() as usize))?;
                let ub = s.strings.get(&(b.as_ptr() as usize))?;
                Some(ua == ub)
            }
            fn get_class_mirror(&mut self, _c: ClassId) -> ObjectRef {
                let mut s = self.shared.lock().unwrap();
                s.alloc_entry(HeapEntry::Object { fields: Vec::new() })
            }
            fn heap_allocated_bytes(&self) -> usize {
                0
            }
            fn get_field_volatile(&self, o: ObjectRef, i: usize) -> Value {
                self.get_field(o, i)
            }
            fn set_field_volatile(&self, o: ObjectRef, i: usize, v: Value) {
                self.set_field(o, i, v)
            }
            fn compare_and_swap_field(
                &mut self,
                _o: ObjectRef,
                _i: usize,
                _e: Value,
                _n: Value,
            ) -> bool {
                false
            }
            fn allocate_instance(&mut self, _c: &str) -> Option<ObjectRef> {
                None
            }
            fn discover_reference(
                &mut self,
                _t: u8,
                _r: ObjectRef,
                _f: ObjectRef,
                _q: Option<ObjectRef>,
            ) {
            }
        }

        impl cratonvm_native_api::NativeThreadAccess for MockCtx {
            // --- monitor primitives (the load-bearing bit) -------------
            fn thread_id(&self) -> u64 {
                self.thread_id
            }
            fn monitor_enter(&mut self, obj: ObjectRef) {
                let m = {
                    let mut s = self.shared.lock().unwrap();
                    s.monitor_for(obj)
                };
                m.enter(self.thread_id);
            }
            fn monitor_exit(&mut self, obj: ObjectRef) {
                let m = {
                    let mut s = self.shared.lock().unwrap();
                    s.monitor_for(obj)
                };
                m.exit(self.thread_id);
            }
            fn monitor_wait(
                &mut self,
                obj: ObjectRef,
                timeout_ms: Option<u64>,
            ) -> MethodCallResult {
                let m = {
                    let mut s = self.shared.lock().unwrap();
                    s.monitor_for(obj)
                };
                m.wait(self.thread_id, timeout_ms);
                Ok(None)
            }
            fn monitor_notify(&mut self, obj: ObjectRef) -> MethodCallResult {
                let m = {
                    let mut s = self.shared.lock().unwrap();
                    s.monitor_for(obj)
                };
                m.notify_all();
                Ok(None)
            }
            fn monitor_notify_all(&mut self, obj: ObjectRef) -> MethodCallResult {
                let m = {
                    let mut s = self.shared.lock().unwrap();
                    s.monitor_for(obj)
                };
                m.notify_all();
                Ok(None)
            }
            fn thread_start(&mut self, _o: ObjectRef) -> MethodCallResult {
                Ok(None)
            }
            fn thread_join(&mut self, _o: ObjectRef) -> MethodCallResult {
                Ok(None)
            }
            fn thread_is_alive(&self, _o: ObjectRef) -> bool {
                false
            }
            fn current_thread_object(&mut self) -> ObjectRef {
                let mut s = self.shared.lock().unwrap();
                s.alloc_entry(HeapEntry::Object { fields: Vec::new() })
            }
            fn thread_interrupt(&mut self, _o: ObjectRef) {}
            fn is_interrupted(&self, _c: bool) -> bool {
                false
            }
            fn active_thread_count(&self) -> i32 {
                1
            }
            fn enumerate_threads(&self, _m: usize) -> Vec<ObjectRef> {
                Vec::new()
            }
            fn park(&mut self, _t: Option<std::time::Duration>) {}
            fn unpark(&self, _o: ObjectRef) {}
            fn get_scoped_value(&self, _k: u64) -> Option<Value> {
                None
            }
            fn push_scoped_value(&mut self, _k: u64, _v: Value) {}
            fn pop_scoped_value(&mut self) {}
            fn scoped_value_depth(&self) -> usize {
                0
            }
        }

        impl cratonvm_native_api::NativeExceptionAccess for MockCtx {
            fn capture_stack_trace(&mut self) -> Vec<StackTraceEntry> {
                Vec::new()
            }

            /// j27 CD16-1: [`MockCtx::with_null_arg_answer`].
            fn null_arg_message(
                &self,
                _class: &str,
                _method: &str,
                _descriptor: &str,
                _param: usize,
            ) -> Option<String> {
                self.null_arg_answer.clone()
            }
        }

        impl cratonvm_native_api::NativeGpuAccess for MockCtx {}

        impl cratonvm_native_api::NativeSystemAccess for MockCtx {
            /// nio21 w1 L4: [`MockCtx::with_jdk_only`].
            fn is_jdk_only(&self) -> bool {
                self.jdk_only
            }

            /// r9w12: the VM-style wrapper door, off unless
            /// [`MockCtx::with_fast_unbox_door`] turned it on.
            ///
            /// Recognises exactly the eight names
            /// `NativeContextImpl::fast_unbox_primitive_wrapper` does and reads
            /// field 0, so a context with it on answers what a real one does.
            /// With it off this stays `None` — "no fast path here" — which is
            /// what every test written before this field assumed.
            fn fast_unbox_primitive_wrapper(&self, obj: ObjectRef) -> Option<Option<Value>> {
                if !self.fast_unbox_door {
                    return None;
                }
                let name = self.class_name_of_id(self.class_id_of_object(obj))?;
                if !matches!(
                    name.as_str(),
                    "java/lang/Integer"
                        | "java/lang/Long"
                        | "java/lang/Boolean"
                        | "java/lang/Character"
                        | "java/lang/Byte"
                        | "java/lang/Short"
                        | "java/lang/Float"
                        | "java/lang/Double"
                ) {
                    return Some(None);
                }
                Some(match self.get_field(obj, 0) {
                    v @ (Value::Int(_) | Value::Long(_) | Value::Float(_) | Value::Double(_)) => {
                        Some(v)
                    }
                    _ => None,
                })
            }

            fn record_printed_value(&mut self, _v: Value) {}
            fn record_printed_line(&mut self, _t: String) {}
            fn get_system_stream(&self, _n: &str) -> Option<ObjectRef> {
                None
            }
            fn get_system_property(&self, _k: &str) -> Option<String> {
                None
            }
            fn set_system_property(&mut self, _k: &str, _v: &str) -> Option<String> {
                None
            }
            fn vm_identity(&self) -> usize {
                self.shared.lock().unwrap().vm_id
            }
            fn is_interface_class(&self, _c: ClassId) -> bool {
                false
            }
            fn loaded_class_count(&self) -> usize {
                0
            }
            fn gc_collection_count(&self) -> u64 {
                0
            }
            fn force_gc(&mut self) {}
            /// The statics a test declared with `define_static`; `Int(0)` for
            /// every other read, as the previous stub answered.
            fn get_static_field(&self, c: ClassId, i: usize) -> Value {
                match self.shared.lock().unwrap().statics.get(i) {
                    Some((cid, _, v)) if *cid == c => *v,
                    _ => Value::Int(0),
                }
            }
            fn static_field_index_by_name(&self, c: ClassId, f: &str) -> Option<usize> {
                self.shared
                    .lock()
                    .unwrap()
                    .statics
                    .iter()
                    .position(|(cid, name, _)| *cid == c && name == f)
            }
            fn set_static_field(&mut self, _c: ClassId, _i: usize, _v: Value) {}
            fn fd_table(&self) -> &cratonvm_native_api::fd_table::FileDescriptorTable {
                use std::sync::OnceLock;
                static FD: OnceLock<cratonvm_native_api::fd_table::FileDescriptorTable> =
                    OnceLock::new();
                FD.get_or_init(cratonvm_native_api::fd_table::FileDescriptorTable::new)
            }
            fn allocate_native_memory(&mut self, _s: usize, _a: usize) -> Option<(i64, *mut u8)> {
                None
            }
            fn free_native_memory(&mut self, _a: i64) {}
            fn load_native_library(&mut self, _p: &str) -> Result<i64, MethodCallFailed> {
                Ok(0)
            }
            fn find_native_symbol(&self, _l: i64, _n: &str) -> Option<usize> {
                None
            }
            fn register_upcall(&mut self, _e: cratonvm_native_api::ffi::UpcallEntry) -> usize {
                0
            }
            fn get_upcall_info(&self, _s: usize) -> Option<(ObjectRef, Vec<i32>, i32)> {
                None
            }
        }

        // ------------------------------------------------------------------
        // G55-1 — the key the map could not find again.
        //
        // MEASURED against Adoptium 25.0.3+9-hotspot on 2026-08-17
        // (`scratchpad/g55/G55Probe.java`, 158 rows; `G55Fp.java`; `G55Ord.java`).
        // A Rust `str` cannot hold an unpaired UTF-16 surrogate, so every
        // `read_string`-derived hash or comparison in this file silently
        // rewrote one to `U+FFFD`. Three consequences, all measured:
        //
        //   * `HashMap.get` / `Hashtable.get` returned `null` for the SAME
        //     `String` object the map was just given — `put` filed it under
        //     `map_hash_key` (units), `get` looked in the `U+FFFD` bucket;
        //   * `List.contains` / `indexOf`, `Deque.contains` and
        //     `Map.containsValue` answered "found" for a string that differs
        //     from the stored one exactly where `String.equals` says different;
        //   * `Map.hashCode()` / `Set.hashCode()` came out stable and wrong.
        //
        // Every assertion below is written in UTF-16 UNITS. Written through
        // `read_string` they all PASS ON THE BROKEN CODE, because two mangled
        // strings are equal to each other — which is precisely how this class
        // of defect survives review.
        // ------------------------------------------------------------------

        const HI: u16 = 0xD800; // unpaired high surrogate
        const LO: u16 = 0xDC00; // unpaired low surrogate

        fn java_hash(units: &[u16]) -> i32 {
            units
                .iter()
                .fold(0i32, |h, &u| h.wrapping_mul(31).wrapping_add(u as i32))
        }

        fn spread(raw: i32) -> i32 {
            raw ^ ((raw as u32) >> 16) as i32
        }

        /// r13w4 hashcompat: `Hashtable` stores and indexes the RAW hash (its
        /// real `putIfAbsent`/`computeIfAbsent`/`hashCode()` bytecode reads
        /// `Entry.hash` as `key.hashCode()`), `HashMap` the spread one. A key
        /// with high bits set is the only kind that tells them apart.
        #[test]
        fn r13_hashcompat_raw_hash_is_unspread_and_map_hash_spreads_it() {
            let mut ctx = MockCtx::new(1);
            let units: Vec<u16> = "alpha".encode_utf16().collect();
            let key = ctx.define_string(&units);
            let raw = java_hash(&units);
            assert_ne!(raw, spread(raw), "fixture check: high bits must be set");
            assert_eq!(map_key_raw_hash(&mut ctx, key).unwrap(), raw);
            assert_eq!(map_hash_key(&mut ctx, key).unwrap(), spread(raw));
        }

        /// r13w4 hashcompat: the `Hashtable` predicate answers String keys by
        /// content in either direction, like `map_keys_equal`.
        #[test]
        fn r13_hashcompat_hashtable_keys_equal_compares_strings_by_units() {
            let mut ctx = MockCtx::new(1);
            let a = ctx.define_string(&[b'x' as u16, HI]);
            let b = ctx.define_string(&[b'x' as u16, HI]);
            let c = ctx.define_string(&[b'x' as u16, LO]);
            assert!(hashtable_keys_equal(&mut ctx, a, b).unwrap());
            assert!(hashtable_keys_equal(&mut ctx, b, a).unwrap());
            assert!(!hashtable_keys_equal(&mut ctx, a, c).unwrap());
        }

        /// r13w6 hashtree: a `Boolean` KEY buckets by `Boolean.hashCode()`
        /// (1231/1237) in `map_key_raw_hash` and in the no-Java CHM wrapper
        /// hash alike, while `Integer` 0/1 keep hashing as 0/1.
        #[test]
        fn r13_hashtree_boolean_keys_hash_like_boolean_hash_code() {
            let mut ctx = MockCtx::new(1);
            let bool_cid = ClassId::new(2201);
            let int_cid = ClassId::new(2202);
            let t = boxed_wrapper(&mut ctx, bool_cid, "java/lang/Boolean", Value::Int(1));
            let f = boxed_wrapper(&mut ctx, bool_cid, "java/lang/Boolean", Value::Int(0));
            let one = boxed_wrapper(&mut ctx, int_cid, "java/lang/Integer", Value::Int(1));
            assert_eq!(map_key_raw_hash(&mut ctx, t).unwrap(), 1231);
            assert_eq!(map_key_raw_hash(&mut ctx, f).unwrap(), 1237);
            assert_eq!(map_hash_key(&mut ctx, t).unwrap(), spread(1231));
            assert_eq!(map_key_raw_hash(&mut ctx, one).unwrap(), 1);
            assert_eq!(
                chm_wrapper_key_hash(&ctx, t),
                Some((Value::Int(1), spread(1231))),
                "the CHM fast path must look in the bucket `chm_key_hash` filed it in"
            );
            assert_eq!(chm_wrapper_key_hash(&ctx, one), Some((Value::Int(1), 1)));
            // The aggregate hash (wave 4) reads the same memoized class fact.
            assert_eq!(
                element_hash_code(&mut ctx, &Value::Object(Some(f))).unwrap(),
                1237
            );
        }

        /// r13w6 hashtree: the integer overlay orders by the unboxed value, so
        /// it must never take a `Boolean` key (its bucket is 1231/1237, not
        /// 0/1). Refused before any overlay row is touched.
        #[test]
        fn r13_hashtree_int_overlay_refuses_boolean_keys() {
            let mut ctx = MockCtx::new(1);
            let map_cid = ClassId::new(2211);
            ctx.define_class(map_cid, "java/util/HashMap");
            let map = ctx.alloc_object(map_cid, 3);
            let t = boxed_wrapper(
                &mut ctx,
                ClassId::new(2212),
                "java/lang/Boolean",
                Value::Int(1),
            );
            assert!(
                try_hm_int_fast_put(&mut ctx, map, Value::Object(Some(t)), Value::Int(1)).is_none(),
                "a Boolean key must take the node path"
            );
        }

        /// r14w1 compat: `HashMap.resize()`'s threshold sequence for a custom
        /// factor, which `lhm_resize` now maintains for a `LinkedHashMap`.
        /// Values are the JDK 25 `resize()` body's, worked by hand.
        #[test]
        fn r14_compat_jdk_next_threshold_follows_resize() {
            // First table (`oldCap == 0`, `oldThr > 0`): recomputed.
            assert_eq!(jdk_hashmap_next_threshold(0, 0, 16, 0.5), 8);
            assert_eq!(jdk_hashmap_next_threshold(0, 0, 64, 0.6), 38);
            // `oldCap >= 16`: `oldThr << 1`, NOT a recomputation (0.6: 9, 18, 36).
            assert_eq!(jdk_hashmap_next_threshold(0, 0, 16, 0.6), 9);
            assert_eq!(jdk_hashmap_next_threshold(16, 9, 32, 0.6), 18);
            assert_eq!(jdk_hashmap_next_threshold(32, 18, 64, 0.6), 36);
            // `oldCap < 16`: recomputed at the new size.
            assert_eq!(jdk_hashmap_next_threshold(4, 2, 8, 0.6), 4);
            assert_eq!(jdk_hashmap_next_threshold(8, 4, 16, 0.6), 9);
            // A zero shift result falls back to the recomputation.
            assert_eq!(jdk_hashmap_next_threshold(16, 0, 32, 0.5), 16);
            // At the cap the JDK stops growing.
            assert_eq!(
                jdk_hashmap_next_threshold(1 << 30, 5, 1 << 30, 0.5),
                i32::MAX
            );
        }

        /// r13w6 hashtree: the JDK's own threshold arithmetic, and the proof
        /// that a 0.75 map keeps the `(cap * 3) / 4` it always had.
        #[test]
        fn r13_hashtree_jdk_threshold_arithmetic() {
            assert_eq!(jdk_hashmap_threshold(16, 0.75), 12);
            assert_eq!(jdk_hashmap_threshold(16, 0.5), 8);
            assert_eq!(jdk_hashmap_threshold(1, 0.75), 0);
            assert_eq!(jdk_hashmap_threshold(2, 0.75), 1);
            assert_eq!(jdk_hashmap_threshold(16, f32::INFINITY), i32::MAX);
            assert_eq!(jdk_hashmap_threshold(1 << 30, 0.75), i32::MAX);
            assert_eq!(jdk_hashtable_threshold(11, 0.75), 8);
            assert_eq!(jdk_hashtable_threshold(10, 0.5), 5);
            assert_eq!(jdk_hashtable_threshold(11, f32::INFINITY), i32::MAX);
            for shift in 0..30 {
                let cap = 1i32 << shift;
                assert_eq!(jdk_hashmap_threshold(cap, 0.75), (cap * 3) / 4, "cap {cap}");
            }
            // `Hashtable` sizes 11, 23, 47, ... while the float product is
            // exact (below 2^24), which is every table a 0.75 map reaches
            // before the two formulas could differ in the last unit.
            let mut cap = 11i32;
            while cap < (1 << 22) {
                assert_eq!(
                    jdk_hashtable_threshold(cap, 0.75),
                    (cap * 3) / 4,
                    "cap {cap}"
                );
                cap = cap * 2 + 1;
            }
        }

        /// r13w6 hashtree: a chain may be as long as its map; only a walk
        /// longer than that is a cycle.
        #[test]
        fn r13_hashtree_chain_walk_limit_scales_with_the_map() {
            assert_eq!(chain_walk_limit(0), 4096);
            assert_eq!(chain_walk_limit(-5), 4096);
            assert_eq!(chain_walk_limit(4000), 4096);
            assert_eq!(chain_walk_limit(10_000), 10_064);
        }

        /// r13w6 hashtree: `new HashSet<>()` plus n distinct adds.
        #[test]
        fn r13_hashtree_hashset_build_capacity_is_what_the_adds_grow_to() {
            assert_eq!(hashset_build_capacity(0), 16);
            assert_eq!(hashset_build_capacity(12), 16);
            assert_eq!(hashset_build_capacity(13), 32);
            assert_eq!(hashset_build_capacity(24), 32);
            assert_eq!(hashset_build_capacity(25), 64);
        }

        /// The bucket half of the silent miss. `put` hashes through
        /// `map_hash_key`; the `get` fast path used to hash the DECODED text.
        /// Pin that both are the receiver's own units, so the two cannot drift
        /// apart again — and that the number is genuinely different from the
        /// `U+FFFD` one, or the test would prove nothing.
        #[test]
        fn a_lone_surrogate_key_hashes_by_its_units_not_by_a_replacement_char() {
            let mut ctx = MockCtx::new(1);
            let key = ctx.define_string(&[b'k' as u16, HI]);

            let units_hash = java_hash(&[b'k' as u16, HI]);
            let mangled_hash = java_hash(&[b'k' as u16, 0xFFFD]);
            assert_ne!(
                units_hash, mangled_hash,
                "fixture check: the two hashes must differ or nothing below is a test"
            );

            assert_eq!(ctx.java_string_hash_code(key), Some(units_hash));
            assert_eq!(
                map_hash_key(&mut ctx, key).unwrap(),
                spread(units_hash),
                "the bucket `put` files this key under"
            );
            assert_eq!(
                element_hash_code(&mut ctx, &Value::Object(Some(key))).unwrap(),
                units_hash,
                "the raw hash the Map/Set hashCode contracts aggregate"
            );
        }

        /// The equality half. Two keys that differ only in WHICH unpaired
        /// surrogate they carry are different keys — and their decoded text is
        /// identical, which is why a `&str` comparison called them equal.
        #[test]
        fn two_different_lone_surrogates_are_not_the_same_key_or_the_same_value() {
            let mut ctx = MockCtx::new(1);
            let high = ctx.define_string(&[b'a' as u16, HI, b'b' as u16]);
            let low = ctx.define_string(&[b'a' as u16, LO, b'b' as u16]);

            assert_eq!(
                ctx.read_string(high),
                ctx.read_string(low),
                "fixture check: the DECODED forms are equal — this is the trap"
            );

            assert!(
                !map_keys_equal(&mut ctx, high, low).unwrap(),
                "HashMap must keep two distinct surrogate keys apart"
            );
            assert!(
                !values_equal(&ctx, &Value::Object(Some(high)), &Value::Object(Some(low))),
                "List.contains / Deque.contains / Map.containsValue must not \
                 report a match for a value the collection does not hold"
            );
            assert_ne!(
                map_hash_key(&mut ctx, high).unwrap(),
                map_hash_key(&mut ctx, low).unwrap()
            );
        }

        /// The other direction, so the test above cannot be satisfied by a
        /// comparison that always answers "not equal": two DISTINCT objects
        /// with the same units are still the same key and the same value.
        #[test]
        fn two_equal_lone_surrogate_strings_are_still_equal() {
            let mut ctx = MockCtx::new(1);
            let a = ctx.define_string(&[b'a' as u16, LO, b'b' as u16]);
            let b = ctx.define_string(&[b'a' as u16, LO, b'b' as u16]);
            assert_ne!(a.as_ptr(), b.as_ptr(), "fixture check: two objects");
            assert!(map_keys_equal(&mut ctx, a, b).unwrap());
            assert!(values_equal(
                &ctx,
                &Value::Object(Some(a)),
                &Value::Object(Some(b))
            ));
            assert_eq!(
                map_hash_key(&mut ctx, a).unwrap(),
                map_hash_key(&mut ctx, b).unwrap()
            );
        }

        /// `java_strings_equal` answering `None` means "not compared", and a
        /// caller must fall through rather than conclude "not equal". A String
        /// against a non-String must still reach the rest of the ladder.
        #[test]
        fn a_string_compared_against_a_non_string_falls_through_rather_than_deciding() {
            let mut ctx = MockCtx::new(1);
            let text = ctx.define_string(&[b'1' as u16]);
            let not_a_string = ctx.alloc_object(ClassId::new(7001), 2);
            assert_eq!(ctx.java_strings_equal(text, not_a_string), None);
            assert!(!values_equal(
                &ctx,
                &Value::Object(Some(text)),
                &Value::Object(Some(not_a_string))
            ));
        }

        /// The fast-mode TreeMap key. A decode that may have lost a surrogate
        /// is REFUSED, so the caller keeps the real object and the receiver's
        /// own `compareTo` decides — rather than two distinct keys collapsing
        /// into one `BTreeMap` entry.
        #[test]
        fn tree_key_refuses_a_string_whose_decode_may_have_lost_a_surrogate() {
            let ctx = MockCtx::new(1);
            let lone = ctx.define_string(&[HI]);
            assert!(
                tree_key_from_value(&ctx, &Value::Object(Some(lone))).is_none(),
                "an unrepresentable key must fall back to the compareTo path, \
                 not be stored under its U+FFFD rendering"
            );
            // A well-formed supplementary character is representable and IS
            // taken — refusing everything would be a different defect.
            let supplementary = ctx.define_string(&[0xD800, 0xDC00]);
            assert_eq!(
                tree_key_from_value(&ctx, &Value::Object(Some(supplementary))),
                Some(TreeKey::Str(vec![0xD800, 0xDC00]))
            );
        }

        /// `String.compareTo` is by code UNIT. `str::cmp` is by code POINT, and
        /// the two disagree above the BMP — which is an ordinary emoji/CJK-ext
        /// key, not surrogate exotica. MEASURED on both VMs: HotSpot enumerates
        /// `[U+10000, U+E000, U+FFFF]`; this crate used to enumerate
        /// `[U+E000, U+FFFF, U+10000]`.
        #[test]
        fn tree_keys_order_by_code_unit_where_a_rust_string_orders_by_code_point() {
            let supplementary = TreeKey::Str("\u{10000}".encode_utf16().collect());
            let e000 = TreeKey::Str(vec![0xE000]);
            let ffff = TreeKey::Str(vec![0xFFFF]);
            let mut bt = std::collections::BTreeMap::new();
            bt.insert(ffff.clone(), 3);
            bt.insert(e000.clone(), 2);
            bt.insert(supplementary.clone(), 1);
            let order: Vec<i32> = bt.values().copied().collect();
            assert_eq!(
                order,
                vec![1, 2, 3],
                "the supplementary key sorts FIRST, as HotSpot orders it"
            );
            assert!(
                "\u{10000}" > "\u{FFFF}",
                "...and Rust's own string ordering, which is byte-lexicographic \
                 over UTF-8, would have sorted it LAST"
            );
        }

        /// Two lone surrogates are two tree keys, not one.
        #[test]
        fn tree_keys_keep_two_lone_surrogates_apart() {
            let mut bt = std::collections::BTreeMap::new();
            bt.insert(TreeKey::Str(vec![HI]), "high");
            bt.insert(TreeKey::Str(vec![LO]), "low");
            assert_eq!(bt.len(), 2);
            assert_eq!(bt.get(&TreeKey::Str(vec![HI])), Some(&"high"));
            assert_eq!(bt.get(&TreeKey::Str(vec![LO])), Some(&"low"));
        }

        /// The two extracted rules, directly. One predicate, two callers — a
        /// second spelling of "was this decode faithful" is how the two would
        /// drift.
        #[test]
        fn the_faithful_decode_rule_gates_both_the_key_and_the_comparison() {
            assert!(decode_is_faithful("plain"));
            assert!(
                decode_is_faithful("\u{1F600}"),
                "a well-formed pair is fine"
            );
            assert!(!decode_is_faithful("a\u{FFFD}b"));

            assert_eq!(tree_key_from_decoded_string("a\u{FFFD}b"), None);
            assert_eq!(
                tree_key_from_decoded_string("ab"),
                Some(TreeKey::Str(vec![b'a' as u16, b'b' as u16]))
            );

            assert_eq!(compare_decoded_strings("a\u{FFFD}", "b"), None);
            assert_eq!(compare_decoded_strings("b", "a\u{FFFD}"), None);
            assert_eq!(
                compare_decoded_strings("\u{10000}", "\u{FFFF}"),
                Some(std::cmp::Ordering::Less),
                "code-unit order, not code-point order"
            );
            assert_eq!(
                compare_decoded_strings("a", "a"),
                Some(std::cmp::Ordering::Equal)
            );
        }

        /// The array-path comparator, end to end through `natural_compare`.
        #[test]
        fn natural_compare_puts_a_supplementary_string_before_u_ffff() {
            let mut ctx = MockCtx::new(1);
            let supplementary = ctx.define_string(&[0xD800, 0xDC00]);
            let ffff = ctx.define_string(&[0xFFFF]);
            let got = natural_compare(
                &mut ctx,
                &Value::Object(Some(supplementary)),
                &Value::Object(Some(ffff)),
            )
            .unwrap();
            assert_eq!(got, Some(Value::Int(-1)));
        }

        #[test]
        fn unbox_wrapper_requires_jdk_wrapper_class() {
            let mut ctx = MockCtx::new(1);
            let entity_class = ClassId::new(1000);
            ctx.define_class(entity_class, "example/EntityWithLongId");
            let entity = ctx.alloc_object(entity_class, 1);
            ctx.set_field(entity, 0, Value::Long(42));

            assert_eq!(unbox_wrapper(&ctx, entity), None);
            let expected_identity = ctx.identity_hash_code(entity);
            assert_eq!(
                element_hash_code(&mut ctx, &Value::Object(Some(entity))).unwrap(),
                expected_identity
            );

            let long_class = ClassId::new(1001);
            ctx.define_class(long_class, "java/lang/Long");
            let boxed = ctx.alloc_object(long_class, 1);
            ctx.set_field(boxed, 0, Value::Long(42));

            assert_eq!(unbox_wrapper(&ctx, boxed), Some(Value::Long(42)));
            assert_eq!(
                element_hash_code(&mut ctx, &Value::Object(Some(boxed))).unwrap(),
                42
            );
        }

        // ------------------------------------------------------------------
        // Cross-wrapper key aliasing (silent wrong answer).
        //
        // `unbox_wrapper` maps Integer / Short / Byte / Character / Boolean
        // onto the same `Value::Int`, and the wrapper-comparison arms also
        // cross-compare Int against Long. Both the exact-HashMap integer
        // overlay and the ordinary bucket-node path keyed off that, so keys
        // real `HashMap` keeps apart collapsed onto one entry. Every assertion
        // below fails against the pre-fix code.
        // ------------------------------------------------------------------

        /// Allocate a JDK wrapper instance of `class_name` holding `value`.
        fn boxed_wrapper(
            ctx: &mut MockCtx,
            cid: ClassId,
            class_name: &str,
            value: Value,
        ) -> ObjectRef {
            ctx.define_class(cid, class_name);
            let obj = ctx.alloc_object(cid, 1);
            ctx.set_field(obj, 0, value);
            obj
        }

        #[test]
        fn wrapper_keys_of_different_classes_are_never_map_equal() {
            let mut ctx = MockCtx::new(1);
            let int_cid = ClassId::new(2001);
            let i65 = boxed_wrapper(&mut ctx, int_cid, "java/lang/Integer", Value::Int(65));
            let i65_other = boxed_wrapper(&mut ctx, int_cid, "java/lang/Integer", Value::Int(65));
            let i1 = boxed_wrapper(&mut ctx, int_cid, "java/lang/Integer", Value::Int(1));
            let c65 = boxed_wrapper(
                &mut ctx,
                ClassId::new(2002),
                "java/lang/Character",
                Value::Int(65),
            );
            let true_box = boxed_wrapper(
                &mut ctx,
                ClassId::new(2003),
                "java/lang/Boolean",
                Value::Int(1),
            );
            let l1 = boxed_wrapper(
                &mut ctx,
                ClassId::new(2004),
                "java/lang/Long",
                Value::Long(1),
            );

            // Unchanged: same wrapper class, same value, distinct objects.
            assert!(
                map_keys_equal(&mut ctx, i65, i65_other).unwrap(),
                "two distinct Integer(65) boxes are the same HashMap key"
            );

            // All four returned `true` before the fix, silently collapsing two
            // distinct JDK keys (which share a bucket, since their hashCodes
            // agree) into one entry on `put`.
            assert!(
                !map_keys_equal(&mut ctx, i65, c65).unwrap(),
                "Integer.valueOf(65) vs Character.valueOf('A')"
            );
            assert!(
                !map_keys_equal(&mut ctx, i1, true_box).unwrap(),
                "Integer.valueOf(1) vs Boolean.TRUE"
            );
            assert!(
                !map_keys_equal(&mut ctx, i1, l1).unwrap(),
                "Integer.valueOf(1) vs Long.valueOf(1L)"
            );
            assert!(
                !map_keys_equal(&mut ctx, l1, i1).unwrap(),
                "Long.valueOf(1L) vs Integer.valueOf(1) (symmetry)"
            );
        }

        #[test]
        fn double_wrapper_keys_use_jdk_bit_equality() {
            let mut ctx = MockCtx::new(1);
            let dbl = ClassId::new(2010);
            let nan_a = boxed_wrapper(&mut ctx, dbl, "java/lang/Double", Value::Double(f64::NAN));
            let nan_b = boxed_wrapper(&mut ctx, dbl, "java/lang/Double", Value::Double(f64::NAN));
            let pos_zero = boxed_wrapper(&mut ctx, dbl, "java/lang/Double", Value::Double(0.0));
            let neg_zero = boxed_wrapper(&mut ctx, dbl, "java/lang/Double", Value::Double(-0.0));

            // `Double.valueOf(NaN).equals(Double.valueOf(NaN))` is TRUE in Java
            // (doubleToLongBits). Was `false` under the old `x == y`, so a NaN
            // key could be stored by `put` and never found again by
            // `get`/`remove`/`containsKey`.
            assert!(
                map_keys_equal(&mut ctx, nan_a, nan_b).unwrap(),
                "a NaN key must find itself"
            );
            // `Double.valueOf(0.0).equals(Double.valueOf(-0.0))` is FALSE in
            // Java. Was `true`, so `put(-0.0, ...)` overwrote the `0.0` entry.
            assert!(
                !map_keys_equal(&mut ctx, pos_zero, neg_zero).unwrap(),
                "+0.0 and -0.0 are distinct HashMap keys"
            );
        }

        #[test]
        fn values_equal_rejects_cross_wrapper_boxes() {
            let mut ctx = MockCtx::new(1);
            let i1 = boxed_wrapper(
                &mut ctx,
                ClassId::new(2020),
                "java/lang/Integer",
                Value::Int(1),
            );
            let l1 = boxed_wrapper(
                &mut ctx,
                ClassId::new(2021),
                "java/lang/Long",
                Value::Long(1),
            );

            // `List.of(Integer.valueOf(1)).contains(Long.valueOf(1L))` is false;
            // so is `map.containsValue(...)` across wrapper types.
            assert!(
                !values_equal(&ctx, &Value::Object(Some(i1)), &Value::Object(Some(l1))),
                "boxed Integer(1) and boxed Long(1L) are not equal"
            );
            // Raw VM-level primitives keep the cross-type numeric comparison —
            // no Java wrapper class is involved there.
            assert!(values_equal(&ctx, &Value::Int(1), &Value::Long(1)));
            // Java `equals` bit semantics for floating point.
            assert!(values_equal(
                &ctx,
                &Value::Double(f64::NAN),
                &Value::Double(f64::NAN)
            ));
            assert!(!values_equal(
                &ctx,
                &Value::Double(0.0),
                &Value::Double(-0.0)
            ));
        }

        #[test]
        fn int_overlay_refuses_a_foreign_wrapper_class() {
            // The overlay side-tables are process-global and keyed by identity
            // hash, which this mock derives from the raw pointer — every
            // `MockCtx` otherwise starts allocating at the same address. Give
            // each overlay-touching test its own pointer range.
            static PTR_SPREAD: std::sync::atomic::AtomicUsize =
                std::sync::atomic::AtomicUsize::new(0);
            let pad = PTR_SPREAD.fetch_add(256, std::sync::atomic::Ordering::Relaxed);
            let mut ctx = MockCtx::new(1);
            let filler = ClassId::new(2030);
            ctx.define_class(filler, "java/lang/Object");
            for _ in 0..pad {
                let _ = ctx.alloc_object(filler, 0);
            }

            let map_cid = ClassId::new(2031);
            ctx.define_class(map_cid, "java/util/HashMap");
            let map = ctx.alloc_object(map_cid, 3);

            let i65 = boxed_wrapper(
                &mut ctx,
                ClassId::new(2032),
                "java/lang/Integer",
                Value::Int(65),
            );
            let c65 = boxed_wrapper(
                &mut ctx,
                ClassId::new(2033),
                "java/lang/Character",
                Value::Int(65),
            );

            // First put seeds the overlay and adopts Integer as its key class.
            assert!(
                try_hm_int_fast_put(&mut ctx, map, Value::Object(Some(i65)), Value::Int(1))
                    .is_some(),
                "fresh exact HashMap with an Integer key takes the overlay"
            );
            // A Character key unboxes to the same `Value::Int(65)`. Before the
            // fix it landed on the SAME overlay slot and overwrote the Integer
            // entry (map.size() stayed 1, get(65) returned the Character's
            // value); now the overlay declines and the caller materializes into
            // real nodes, where `map_keys_equal` keeps the two keys apart.
            assert!(
                try_hm_int_fast_put(&mut ctx, map, Value::Object(Some(c65)), Value::Int(2))
                    .is_none(),
                "a Character key must not reuse the Integer overlay slot"
            );
            let got = try_hm_int_fast_get(&ctx, map, Value::Object(Some(i65)));
            assert!(
                matches!(got, Some(Ok(Some(Value::Int(1))))),
                "Integer entry survives the rejected Character put: {got:?}"
            );
            assert!(
                try_hm_int_fast_get(&ctx, map, Value::Object(Some(c65))).is_none(),
                "a Character lookup must fall through to the node path"
            );
            assert_eq!(hm_int_fast_len(&ctx, map), Some(1));

            // Leave the process-global overlay as we found it.
            let overlay_key = hm_int_fast_obj_key(&ctx, map);
            hm_int_fast_shard_for(overlay_key)
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&overlay_key);
        }

        /// Give an overlay-touching test its own pointer range: the side tables
        /// are process-global and keyed by an identity hash the mock derives
        /// from the raw pointer, and every `MockCtx` starts allocating at the
        /// same address. Same reason as
        /// `int_overlay_refuses_a_foreign_wrapper_class`, hoisted so the
        /// generation tests below can share it.
        fn spread_mock_pointers(ctx: &mut MockCtx) {
            static PTR_SPREAD: std::sync::atomic::AtomicUsize =
                std::sync::atomic::AtomicUsize::new(1024);
            let pad = PTR_SPREAD.fetch_add(256, std::sync::atomic::Ordering::Relaxed);
            let filler = ClassId::new(2130);
            ctx.define_class(filler, "java/lang/Object");
            for _ in 0..pad {
                let _ = ctx.alloc_object(filler, 0);
            }
        }

        /// A class declares `modCount`; an object of that class may still not
        /// HAVE that slot.
        ///
        /// This is the whole defect in one assertion. `resolve_field_index_by_class_id`
        /// answers 5 for every `java/util/HashMap` receiver, including ones
        /// allocated with fewer fields than the class declares — and the by-name
        /// accessors this replaced applied no bound, so the write was dropped
        /// and the read answered a constant. `view_source_generation` reads a
        /// constant as "this map reports when it changes".
        #[test]
        fn mod_count_slot_is_bounded_by_the_objects_own_fields() {
            let mut ctx = MockCtx::new(1);
            define_real_hashmap(&ctx);

            let full = ctx.alloc_object_of(HASHMAP_CID, 8);
            assert_eq!(
                map_mod_count_slot(&ctx, full),
                Some(5),
                "a receiver with the slot reports it"
            );

            let starved = ctx.alloc_object_of(HASHMAP_CID, 3);
            assert_eq!(
                map_mod_count_slot(&ctx, starved),
                None,
                "slot 5 is past this receiver's three fields, so it has no \
                 generation at all — and `None` is what makes every reader \
                 rebuild instead of trusting a frozen counter"
            );
        }

        /// The writer and the reader must agree about which receivers HAVE a
        /// generation. A reader that accepts what the writer cannot reach
        /// reports "unchanged" forever.
        #[test]
        fn the_generation_reader_accepts_exactly_what_the_writer_can_move() {
            let mut ctx = MockCtx::new(1);
            define_real_hashmap(&ctx);

            let full = ctx.alloc_object_of(HASHMAP_CID, 8);
            ctx.set_field(full, 5, Value::Int(0));
            bump_map_mod_count(&ctx, full);
            bump_map_mod_count(&ctx, full);
            assert_eq!(
                map_itr_mod_count(&ctx, full),
                Some(2),
                "two structural mutations, two generations"
            );

            let starved = ctx.alloc_object_of(HASHMAP_CID, 3);
            bump_map_mod_count(&ctx, starved);
            assert_eq!(
                map_itr_mod_count(&ctx, starved),
                None,
                "the writer could not move this receiver, so the reader must \
                 not hand `view_source_generation` a number for it"
            );
        }

        /// JIT round 20 wave 17 (lane hashOff): a receiver with a real `table`
        /// field (a JDK image's `HashMap`) never starts an overlay, so its
        /// Integer keys go to the node table JDK bytecode reads.
        #[test]
        fn overlay_is_refused_for_a_real_table_receiver() {
            let mut ctx = MockCtx::new(1);
            spread_mock_pointers(&mut ctx);
            define_real_hashmap(&ctx);
            let map = ctx.alloc_object_of(HASHMAP_CID, 8);
            let key = boxed_wrapper(
                &mut ctx,
                ClassId::new(2133),
                "java/lang/Integer",
                Value::Int(11),
            );
            assert!(
                try_hm_int_fast_put(&mut ctx, map, Value::Object(Some(key)), Value::Int(1))
                    .is_none(),
                "a real-table HashMap takes the node path"
            );
            assert_eq!(hm_int_fast_len(&ctx, map), None, "and holds no overlay row");
        }

        /// An overlay insert is a structural modification of the `HashMap` the
        /// overlay stands in for, and has to move the generation the keySet-view
        /// rebuild elision reads — otherwise the view freezes at whatever it
        /// held when it was built, `keySet().size()` disagrees with the same
        /// view's iterator, and ecj's `getFramePositions` throws
        /// `ArrayIndexOutOfBoundsException` with index == length.
        ///
        /// A value-replacing put moves nothing, exactly as on HotSpot
        /// (`probes/MapModCountProbe2`: `put=5 remove=6 replace=6`).
        #[test]
        fn an_overlay_put_moves_the_generation_only_when_it_is_structural() {
            let mut ctx = MockCtx::new(1);
            spread_mock_pointers(&mut ctx);
            define_overlay_hashmap(&ctx);
            let map = ctx.alloc_object_of(OVERLAY_HASHMAP_CID, 8);
            ctx.set_field(map, 5, Value::Int(0));

            let k1 = boxed_wrapper(
                &mut ctx,
                ClassId::new(2131),
                "java/lang/Integer",
                Value::Int(7),
            );
            let k2 = ctx.alloc_object_of(ClassId::new(2131), 1);
            ctx.set_field(k2, 0, Value::Int(9));

            assert!(
                try_hm_int_fast_put(&mut ctx, map, Value::Object(Some(k1)), Value::Int(1))
                    .is_some(),
                "a fresh exact HashMap with an Integer key takes the overlay"
            );
            assert_eq!(
                map_itr_mod_count(&ctx, map),
                Some(1),
                "a new key is structural"
            );

            assert!(
                try_hm_int_fast_put(&mut ctx, map, Value::Object(Some(k1)), Value::Int(2))
                    .is_some()
            );
            assert_eq!(
                map_itr_mod_count(&ctx, map),
                Some(1),
                "replacing a value changes no key — HotSpot does not bump here \
                 either, and an entrySet view is already refused a generation"
            );

            assert!(
                try_hm_int_fast_put(&mut ctx, map, Value::Object(Some(k2)), Value::Int(3))
                    .is_some()
            );
            assert_eq!(
                map_itr_mod_count(&ctx, map),
                Some(2),
                "a second new key is structural too"
            );

            // Leave the process-global overlay as we found it.
            hm_int_fast_purge(&ctx, map);
        }

        /// `hm_int_fast_purge` is what makes a field-by-field map reset actually
        /// empty the map. Nothing that writes the bucket array, the size slot or
        /// the `modCount` slot touches the overlay, and `native_map_size` prefers
        /// the overlay's count — so a keySet view's backing, rebuilt through
        /// exactly those field writes, reported a high-water mark until this
        /// ran on it.
        #[test]
        fn purging_the_overlay_is_what_makes_a_field_reset_empty_the_map() {
            let mut ctx = MockCtx::new(1);
            spread_mock_pointers(&mut ctx);
            define_overlay_hashmap(&ctx);
            let map = ctx.alloc_object_of(OVERLAY_HASHMAP_CID, 8);
            ctx.set_field(map, 5, Value::Int(0));
            let key = boxed_wrapper(
                &mut ctx,
                ClassId::new(2132),
                "java/lang/Integer",
                Value::Int(4),
            );
            assert!(
                try_hm_int_fast_put(&mut ctx, map, Value::Object(Some(key)), Value::Int(1))
                    .is_some()
            );
            assert_eq!(hm_int_fast_len(&ctx, map), Some(1));

            // The three writes a view-backing rebuild makes.
            let buckets = ctx.new_ref_array(ClassId::new(0), 16);
            publish_map_table(&mut ctx, map, buckets, 16);
            try_set_jdk_map_field(&mut ctx, map, "modCount", Value::Int(0));
            set_map_size(&mut ctx, map, 0);
            assert_eq!(
                hm_int_fast_len(&ctx, map),
                Some(1),
                "none of them reach the overlay — this is the bug, stated"
            );

            assert!(
                hm_int_fast_purge(&ctx, map),
                "the purge reports what it dropped"
            );
            assert_eq!(hm_int_fast_len(&ctx, map), None, "now the map is empty");
        }

        // ===============================================================
        // Receiver classification memo
        // (perf/collections-classification-cost)
        // ===============================================================

        /// Declare `id` as `name`, optionally extending `parent`.
        fn declare(ctx: &MockCtx, id: u32, name: &str, parent: Option<u32>) -> ClassId {
            let cid = ClassId::new(id);
            ctx.define_class(cid, name);
            if let Some(p) = parent {
                ctx.define_superclass(cid, ClassId::new(p));
            }
            cid
        }

        /// A miniature `java.util` hierarchy, shaped like the real one:
        /// `AbstractMap` and `Dictionary` are deliberately NOT well-known, so
        /// the walk has to pass through classes it does not recognise.
        fn declare_map_hierarchy(ctx: &MockCtx) {
            declare(ctx, 100, "java/lang/Object", None);
            declare(ctx, 101, "java/util/AbstractMap", Some(100));
            declare(ctx, 102, "java/util/HashMap", Some(101));
            declare(ctx, 103, "java/util/LinkedHashMap", Some(102));
            declare(ctx, 104, "java/util/TreeMap", Some(101));
            declare(ctx, 105, "java/util/Dictionary", Some(100));
            declare(ctx, 106, "java/util/Hashtable", Some(105));
            declare(ctx, 107, "java/util/Properties", Some(106));
            declare(
                ctx,
                108,
                "java/util/concurrent/ConcurrentHashMap",
                Some(101),
            );
            // An application subclass, the case the predicates exist for.
            declare(ctx, 109, "app/AnnotationAttributes", Some(103));
            declare(ctx, 110, "cratonvm/internal/UnmodifiableMap", Some(100));
        }

        #[test]
        fn class_facts_classifies_every_map_family() {
            let ctx = MockCtx::new(1);
            declare_map_hierarchy(&ctx);
            let facts = |id: u32| class_facts(&ctx, ClassId::new(id));

            // Exact `java/util/HashMap` — the class the integer overlay is
            // entered for.
            let hm = facts(102);
            assert!(hm.has(CF_EXACT_HASHMAP));
            assert!(hm.has(CF_BUCKET_MAP_NAME));
            assert!(hm.has(CF_HAS_NAME));
            assert!(!hm.has(CF_LHM));
            assert!(!hm.has(CF_TREE_MAP));
            assert!(!hm.has(CF_CHM));
            assert!(!hm.has(CF_HASHTABLE_LAYOUT));
            assert!(!hm.has(CF_HASHTABLE_ANCESTRY));
            assert!(!hm.has(CF_UNMOD_WRAPPER));

            // LinkedHashMap routes to the LHM overlay but still reads as a
            // bucket map (it reaches `HashMap` before `Object`), matching the
            // original walks exactly.
            let lhm = facts(103);
            assert!(lhm.has(CF_LHM));
            assert!(lhm.has(CF_BUCKET_MAP_NAME));
            assert!(!lhm.has(CF_EXACT_HASHMAP));
            assert!(!lhm.has(CF_TREE_MAP));

            // The WFLYCTL0013 case: an application subclass of LinkedHashMap
            // must still route to the LHM natives.
            let app = facts(109);
            assert!(app.has(CF_LHM));
            assert!(!app.has(CF_EXACT_HASHMAP));

            // TreeMap: field 0 is `root`, not a bucket array (the Keycloak
            // FeatureOptions bug), so it must NOT read as a bucket map.
            let tm = facts(104);
            assert!(tm.has(CF_TREE_MAP));
            assert!(!tm.has(CF_BUCKET_MAP_NAME));
            assert!(!tm.has(CF_LHM));
            assert!(!tm.has(CF_CHM));

            let chm = facts(108);
            assert!(chm.has(CF_CHM));
            assert!(!chm.has(CF_LHM));
            assert!(!chm.has(CF_TREE_MAP));

            // Hashtable uses the native bucket layout...
            let ht = facts(106);
            assert!(ht.has(CF_HASHTABLE_LAYOUT));
            assert!(ht.has(CF_HASHTABLE_ANCESTRY));
            assert!(ht.has(CF_BUCKET_MAP_NAME));

            // ...but `Properties` does not: JDK 25 backs it with a side CHM.
            // It must still be seen as Hashtable ANCESTRY, so that
            // `native_map_put_evict` runs `ensure_hashtable_load_factor` on it.
            // This is the one place the two Hashtable bits differ, and it is
            // why both exist.
            let props = facts(107);
            assert!(!props.has(CF_HASHTABLE_LAYOUT));
            assert!(props.has(CF_HASHTABLE_ANCESTRY));
            assert!(props.has(CF_BUCKET_MAP_NAME));

            let object = facts(100);
            assert!(object.has(CF_EXACT_OBJECT));
            assert!(!object.has(CF_BUCKET_MAP_NAME));

            let unmod = facts(110);
            assert!(unmod.has(CF_UNMOD_WRAPPER));

            // An id that was never defined has no name, so
            // `native_map_put_evict`'s family-dispatch block stays skipped.
            let unknown = facts(999);
            assert!(!unknown.has(CF_HAS_NAME));
            assert_eq!(unknown, ClassFacts(0));
        }

        #[test]
        fn class_facts_is_stable_across_repeated_lookups() {
            let ctx = MockCtx::new(1);
            declare_map_hierarchy(&ctx);
            // First call classifies, the rest must be served from the memo and
            // agree with a fresh, uncached classification.
            for id in [100u32, 102, 103, 104, 106, 107, 108, 109, 110, 999] {
                let cid = ClassId::new(id);
                let fresh = classify_class(&ctx, cid);
                assert_eq!(class_facts(&ctx, cid), fresh, "first lookup, id {id}");
                assert_eq!(class_facts(&ctx, cid), fresh, "memoized lookup, id {id}");
                assert_eq!(class_facts(&ctx, cid), fresh, "memoized again, id {id}");
            }
        }

        #[test]
        fn class_facts_is_scoped_per_vm() {
            // The memo is thread-local, so two VMs on one thread would share it
            // if it were not keyed by `vm_identity` — and `ClassId` spaces are
            // per-VM, so the same id means different classes in each.
            let a = MockCtx::new(1);
            let b = MockCtx::new(1);
            assert_ne!(a.vm_identity(), b.vm_identity());
            declare(&a, 42, "java/util/TreeMap", None);
            declare(&b, 42, "java/util/HashMap", None);

            assert!(class_facts(&a, ClassId::new(42)).has(CF_TREE_MAP));
            assert!(class_facts(&b, ClassId::new(42)).has(CF_EXACT_HASHMAP));
            // Re-reading A must not return B's answer.
            assert!(class_facts(&a, ClassId::new(42)).has(CF_TREE_MAP));
            assert!(!class_facts(&a, ClassId::new(42)).has(CF_EXACT_HASHMAP));
        }

        /// nio20 wave 6, L5: [`CF_IDENTITY_KEY`] is set exactly for a class
        /// whose chain below `Object` declares neither `hashCode()I` nor
        /// `equals(Object)Z`, and only when `Object` itself proves the context
        /// answers `class_declares_method`.
        #[test]
        fn identity_key_facts_follow_the_declared_methods() {
            const H: (&str, &str) = ("hashCode", "()I");
            const E: (&str, &str) = ("equals", "(Ljava/lang/Object;)Z");
            let ctx = MockCtx::new(1);
            let object = declare(&ctx, 200, "java/lang/Object", None);
            ctx.define_declared_method(object, H.0, H.1);
            ctx.define_declared_method(object, E.0, E.1);
            declare(&ctx, 201, "app/Plain", Some(200));
            let with_hash = declare(&ctx, 202, "app/WithHash", Some(200));
            ctx.define_declared_method(with_hash, H.0, H.1);
            declare(&ctx, 203, "app/SubOfWithHash", Some(202));
            let with_equals = declare(&ctx, 204, "app/WithEquals", Some(200));
            ctx.define_declared_method(with_equals, E.0, E.1);
            let en = declare(&ctx, 205, "java/lang/Enum", Some(200));
            ctx.define_declared_method(en, H.0, H.1);
            ctx.define_declared_method(en, E.0, E.1);
            declare(&ctx, 206, "app/Color", Some(205));
            declare(&ctx, 207, "cratonvm/internal/Carrier", Some(200));
            declare(&ctx, 208, "java/lang/reflect/ParameterizedType", Some(200));
            declare(&ctx, 209, "java/lang/Class", Some(200));
            let facts = |id: u32| class_facts(&ctx, ClassId::new(id));
            assert!(facts(200).has(CF_IDENTITY_KEY), "a bare Object");
            assert!(facts(201).has(CF_IDENTITY_KEY));
            assert!(facts(209).has(CF_IDENTITY_KEY), "java.lang.Class");
            assert!(!facts(202).has(CF_IDENTITY_KEY));
            assert!(!facts(203).has(CF_IDENTITY_KEY), "inherits a hashCode");
            assert!(!facts(204).has(CF_IDENTITY_KEY));
            assert!(!facts(206).has(CF_IDENTITY_KEY), "an enum keeps its arm");
            assert!(facts(206).has(CF_ENUM));
            assert!(!facts(207).has(CF_IDENTITY_KEY), "a VM carrier");
            assert!(
                !facts(208).has(CF_IDENTITY_KEY),
                "a structural reflect stub"
            );
            assert!(!facts(999).has(CF_IDENTITY_KEY), "no name");

            // A context that cannot show `Object` declaring them proves nothing.
            let blind = MockCtx::new(1);
            declare(&blind, 200, "java/lang/Object", None);
            declare(&blind, 201, "app/Plain", Some(200));
            assert!(!class_facts(&blind, ClassId::new(201)).has(CF_IDENTITY_KEY));
        }

        /// nio20 wave 6, L5: a `Locale` key hashes as its cached
        /// `hashCodeValue` once that is set, without the `hashCode()` call;
        /// while it is 0 the call decides (this mock's call answers nothing,
        /// so the identity fallback shows the field was not used).
        #[test]
        fn locale_key_hash_reads_the_cached_hash_code_value() {
            let mut ctx = MockCtx::new(1);
            declare(&ctx, 300, "java/lang/Object", None);
            let locale = declare(&ctx, 301, "java/util/Locale", Some(300));
            ctx.define_field(locale, "hashCodeValue", 2);
            let key = ctx.alloc_object(locale, 4);
            assert!(class_facts(&ctx, locale).has(CF_LOCALE));
            assert_eq!(
                locale_cached_hash(&ctx, key, locale),
                None,
                "not computed yet"
            );
            assert_eq!(
                map_key_raw_hash(&mut ctx, key).unwrap(),
                ctx.identity_hash_code(key),
                "unset: the call (here the mock's fallback) decides"
            );
            ctx.set_field(key, 2, Value::Int(77));
            assert_eq!(locale_cached_hash(&ctx, key, locale), Some(77));
            assert_eq!(map_key_raw_hash(&mut ctx, key).unwrap(), 77);
        }

        /// nio21 wave 1, L4: the JDK `ConcurrentHashMap` table as the JDK's
        /// `Traverser` reads it -- a plain bin's chain, a `TreeBin` by its
        /// `first` list, a `ReservationNode` as nothing, a `ForwardingNode`'s
        /// bin `i` then `i + n` of `nextTable`, an unknown shape as a partial
        /// walk -- and the collectors take it only in a JDK-model VM.
        #[test]
        fn chm_jdk_table_walk_reads_the_jdk_bins_in_traverser_order() {
            let mut ctx = MockCtx::new(1).with_jdk_only();
            let object = declare(&ctx, 400, "java/lang/Object", None);
            let chm = declare(
                &ctx,
                401,
                "java/util/concurrent/ConcurrentHashMap",
                Some(400),
            );
            ctx.define_field(chm, "table", 0);
            let node = declare(
                &ctx,
                402,
                "java/util/concurrent/ConcurrentHashMap$Node",
                Some(400),
            );
            for (i, name) in ["hash", "key", "val", "next"].into_iter().enumerate() {
                ctx.define_field(node, name, i);
            }
            let tree_bin = declare(
                &ctx,
                403,
                "java/util/concurrent/ConcurrentHashMap$TreeBin",
                Some(402),
            );
            ctx.define_field(tree_bin, "first", 4);
            let fwd = declare(
                &ctx,
                404,
                "java/util/concurrent/ConcurrentHashMap$ForwardingNode",
                Some(402),
            );
            ctx.define_field(fwd, "nextTable", 4);
            let reserved = declare(
                &ctx,
                405,
                "java/util/concurrent/ConcurrentHashMap$ReservationNode",
                Some(402),
            );
            let obj = |ctx: &MockCtx| Value::Object(Some(ctx.alloc_object_of(object, 0)));
            let mk = |ctx: &MockCtx, cid: ClassId, hash: i32, key: Value, val: Value| {
                let n = ctx.alloc_object_of(cid, 5);
                ctx.set_field(n, 0, Value::Int(hash));
                ctx.set_field(n, 1, key);
                ctx.set_field(n, 2, val);
                ctx.set_field(n, 3, Value::Object(None));
                ctx.set_field(n, 4, Value::Object(None));
                n
            };
            let link = |ctx: &MockCtx, a: ObjectRef, b: ObjectRef| {
                ctx.set_field(a, 3, Value::Object(Some(b)));
            };
            let none = Value::Object(None);
            let (k1, v1, k2, v2, k3, v3, k4, v4) = (
                obj(&ctx),
                obj(&ctx),
                obj(&ctx),
                obj(&ctx),
                obj(&ctx),
                obj(&ctx),
                obj(&ctx),
                obj(&ctx),
            );
            // Bin 0: k1 -> k2. Bin 1: empty. Bin 2: a TreeBin over k3 -> k4.
            // Bin 3: a reservation.
            let n1 = mk(&ctx, node, 7, k1, v1);
            let n2 = mk(&ctx, node, 11, k2, v2);
            link(&ctx, n1, n2);
            let n3 = mk(&ctx, node, 2, k3, v3);
            let n4 = mk(&ctx, node, 6, k4, v4);
            link(&ctx, n3, n4);
            let bin = mk(&ctx, tree_bin, CHM_JDK_TREEBIN, none, none);
            ctx.set_field(bin, 4, Value::Object(Some(n3)));
            let res = mk(&ctx, reserved, CHM_JDK_RESERVED, none, none);
            let table = ctx.new_ref_array(ClassId::new(0), 4);
            ctx.set_array_element(table, 0, Value::Object(Some(n1)));
            ctx.set_array_element(table, 2, Value::Object(Some(bin)));
            ctx.set_array_element(table, 3, Value::Object(Some(res)));
            let map = ctx.alloc_object_of(chm, 1);
            ctx.set_field(map, 0, Value::Object(Some(table)));

            let mut seen = Vec::new();
            assert!(chm_jdk_table_walk(&ctx, map, &mut |k, v| seen.push((k, v))));
            assert_eq!(seen, vec![(k1, v1), (k2, v2), (k3, v3), (k4, v4)]);
            assert_eq!(chm_collect_all_entries(&ctx, map), seen);
            assert_eq!(chm_collect_all_keys(&ctx, map), vec![k1, k2, k3, k4]);
            assert_eq!(chm_collect_all_values(&ctx, map), vec![v1, v2, v3, v4]);

            // A resize in flight: bin 0 of a 2-bin table forwarded to a 4-bin
            // `nextTable` holding A at 0 and B at 2; bin 1 still holds C.
            let (ka, kb, kc) = (obj(&ctx), obj(&ctx), obj(&ctx));
            let na = mk(&ctx, node, 4, ka, ka);
            let nb = mk(&ctx, node, 6, kb, kb);
            let nc = mk(&ctx, node, 1, kc, kc);
            let next_table = ctx.new_ref_array(ClassId::new(0), 4);
            ctx.set_array_element(next_table, 0, Value::Object(Some(na)));
            ctx.set_array_element(next_table, 2, Value::Object(Some(nb)));
            let f = mk(&ctx, fwd, CHM_JDK_MOVED, none, none);
            ctx.set_field(f, 4, Value::Object(Some(next_table)));
            let small = ctx.new_ref_array(ClassId::new(0), 2);
            ctx.set_array_element(small, 0, Value::Object(Some(f)));
            ctx.set_array_element(small, 1, Value::Object(Some(nc)));
            let moving = ctx.alloc_object_of(chm, 1);
            ctx.set_field(moving, 0, Value::Object(Some(small)));
            let keys: Vec<Value> = {
                let mut out = Vec::new();
                assert!(chm_jdk_table_walk(&ctx, moving, &mut |k, _| out.push(k)));
                out
            };
            assert_eq!(keys, vec![ka, kb, kc]);

            // An unknown negative hash: what was read, flagged partial.
            let odd = mk(&ctx, node, -9, none, none);
            let odd_table = ctx.new_ref_array(ClassId::new(0), 2);
            ctx.set_array_element(odd_table, 0, Value::Object(Some(n2)));
            ctx.set_array_element(odd_table, 1, Value::Object(Some(odd)));
            let partial = ctx.alloc_object_of(chm, 1);
            ctx.set_field(partial, 0, Value::Object(Some(odd_table)));
            let mut count = 0;
            assert!(!chm_jdk_table_walk(&ctx, partial, &mut |_, _| count += 1));
            assert_eq!(count, 1);

            // A map whose table was never allocated is empty.
            let fresh = ctx.alloc_object_of(chm, 1);
            ctx.set_field(fresh, 0, Value::Object(None));
            assert!(chm_jdk_table_walk(&ctx, fresh, &mut |_, _| panic!(
                "no entries"
            )));
            assert!(chm_collect_all_entries(&ctx, fresh).is_empty());

            // Not a JDK-model VM: the collectors keep the segmented reader.
            let compat = MockCtx::new(1);
            assert!(!chm_jdk_model(&compat));
            assert!(chm_jdk_model(&ctx));
            assert_eq!(chm_jdk_collect(&compat, map), None);
        }

        /// gc-common w20-a: the per-thread `HM_INT_FAST_LAST_KEY` memo is
        /// scoped to `vm_identity`. Two mock VMs hand out the same first address
        /// with the same identity hash (this mock's hash IS the address) -- the
        /// shape of a second VM whose heap was mapped where the first one's was.
        /// The memo used to key on `(address, hash)` alone and answered the
        /// second VM with the FIRST VM's overlay key.
        #[test]
        fn hm_int_fast_obj_key_memo_is_scoped_per_vm() {
            let a = MockCtx::new(1);
            let b = MockCtx::new(1);
            assert_ne!(a.vm_identity(), b.vm_identity());
            let oa = a.alloc_object_of(ClassId::new(0), 1);
            let ob = b.alloc_object_of(ClassId::new(0), 1);
            assert_eq!(
                oa.as_ptr(),
                ob.as_ptr(),
                "the two mock heaps start at one address"
            );
            let ka = hm_int_fast_obj_key(&a, oa);
            let kb = hm_int_fast_obj_key(&b, ob);
            assert_eq!(
                kb,
                widened_obj_key(&b, ob),
                "the second VM must get ITS key for the object, not the memo's"
            );
            assert_ne!(ka, kb, "two VMs' objects must not share an overlay key");
            // And the first VM's answer is still its own.
            assert_eq!(hm_int_fast_obj_key(&a, oa), widened_obj_key(&a, oa));
        }

        #[test]
        fn class_facts_survives_a_direct_mapped_tag_collision() {
            // Two `ClassId`s that share a cache slot must each get their own
            // answer: the slot carries the id it was filled for and is only
            // trusted on an exact match, so a collision costs a
            // re-classification, never a wrong classification.
            let ctx = MockCtx::new(1);
            let base = 700u32;
            let slot = facts_slot_index(base);
            let other = (base + 1..base + 400_000)
                .find(|&id| facts_slot_index(id) == slot)
                .expect("some later id shares the slot");
            assert_ne!(base, other);

            declare(&ctx, base, "java/util/TreeMap", None);
            declare(&ctx, other, "java/util/HashMap", None);

            // Alternate, so each lookup evicts the other from the shared slot.
            for _ in 0..4 {
                assert!(class_facts(&ctx, ClassId::new(base)).has(CF_TREE_MAP));
                assert!(!class_facts(&ctx, ClassId::new(base)).has(CF_EXACT_HASHMAP));
                assert!(class_facts(&ctx, ClassId::new(other)).has(CF_EXACT_HASHMAP));
                assert!(!class_facts(&ctx, ClassId::new(other)).has(CF_TREE_MAP));
            }
            for id in 0u32..2048 {
                assert!(facts_slot_index(id) < RECEIVER_FACTS_WAYS);
            }
        }

        #[test]
        fn unloaded_class_ids_are_never_reissued_so_the_memo_needs_no_generation() {
            // Class unloading landed on `dev` this wave. A `ClassId`-keyed memo
            // is only sound because `ClassStore` never hands an id back out:
            // `remove` leaves a `None` tombstone (`Option::take`) and
            // `next_id()` is the slot VECTOR's length, not `live_count`
            // (`classloading/src/class.rs:909`, `:1096`, pinned by the test
            // `unloaded_slots_are_tombstoned_and_never_reused`). This test
            // exercises both halves of that argument.
            let ctx = MockCtx::new(1);
            let dead = 300u32;
            declare(&ctx, dead, "java/util/TreeMap", None);
            assert!(class_facts(&ctx, ClassId::new(dead)).has(CF_TREE_MAP));

            ctx.unload_class(ClassId::new(dead));

            // Half one — the real invariant. The class loaded after the unload
            // gets a FRESH id, so it can never read the dead class's memo
            // entry, and no generation stamp or unload hook is needed.
            let fresh = dead + 1;
            declare(&ctx, fresh, "java/util/HashMap", None);
            let facts = class_facts(&ctx, ClassId::new(fresh));
            assert!(facts.has(CF_EXACT_HASHMAP));
            assert!(!facts.has(CF_TREE_MAP), "no bleed from the unloaded class");

            // Half two — why the invariant is load-bearing rather than
            // incidental. If an id COULD be recycled, the memo would answer for
            // the dead class. Asserting the stale answer here documents the
            // exact dependency: if `ClassStore` ever starts reusing slots, this
            // assertion flips and the memo needs a generation stamp.
            declare(&ctx, dead, "java/util/HashMap", None);
            assert!(
                class_facts(&ctx, ClassId::new(dead)).has(CF_TREE_MAP),
                "memo is `ClassId`-keyed with no generation — sound ONLY because \
                 `ClassStore` tombstones unloaded slots and never reissues an id"
            );
        }

        // ===============================================================
        // Sharded integer overlay (perf/hm-int-fast-shard)
        // ===============================================================

        #[test]
        fn hm_int_fast_shard_index_is_in_range_and_spreads() {
            let mut seen = [false; HM_INT_FAST_SHARDS];
            for key in 0usize..4096 {
                let idx = hm_int_fast_shard_index(key);
                assert!(idx < HM_INT_FAST_SHARDS, "shard {idx} out of range");
                seen[idx] = true;
            }
            assert!(
                seen.iter().all(|hit| *hit),
                "a 4096-key sample must reach all {HM_INT_FAST_SHARDS} shards"
            );
            // Deterministic: the same key always resolves to the same shard,
            // which is what makes per-key routing equivalent to the old single
            // map.
            for key in [0usize, 1, 12345, usize::MAX / 3] {
                assert_eq!(hm_int_fast_shard_index(key), hm_int_fast_shard_index(key));
            }
        }

        #[test]
        fn sharded_overlay_enumeration_visits_every_shard() {
            // The GC root scan / post-move remap walks the overlay through
            // `for_each_overlay_ref`. With the table sharded, missing a shard
            // silently drops a root — so prove the walk reaches entries planted
            // in several DIFFERENT shards, not just the first.
            let mut ctx = MockCtx::new(1);
            let cid = ClassId::new(400);
            ctx.define_class(cid, "java/lang/Object");

            // One key per target shard, tagged well clear of the packed
            // `widened_obj_key` values real maps produce.
            // Built by shifting rather than written as a 64-bit literal, so the
            // test still compiles if this is ever built for a 32-bit target.
            let tag = 0xC0FF_EE00usize.wrapping_shl(32);
            let mut planted: Vec<(usize, usize)> = Vec::new(); // (key, key-object ptr)
            let mut targets: Vec<usize> = Vec::new();
            let mut probe = 0usize;
            while targets.len() < 8 {
                let key = tag | (probe << 8) | targets.len();
                let shard = hm_int_fast_shard_index(key);
                if !targets.contains(&shard) {
                    targets.push(shard);
                    let key_obj = ctx.alloc_object(cid, 0);
                    let value_obj = ctx.alloc_object(cid, 0);
                    let mut state = HmIntFastState::default();
                    state
                        .entries
                        .insert(7, (key_obj, Value::Object(Some(value_obj))));
                    let prev = hm_int_fast_shard_for(key)
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(key, state);
                    assert!(prev.is_none(), "test key collided with a live overlay");
                    planted.push((key, key_obj.as_ptr() as usize));
                    planted.push((key, value_obj.as_ptr() as usize));
                }
                probe += 1;
                assert!(probe < 100_000, "could not find 8 distinct shards");
            }
            assert!(
                targets.len() > 1,
                "the point of the test is entries in more than one shard"
            );

            let mine: std::collections::HashSet<usize> =
                planted.iter().map(|(_, ptr)| *ptr).collect();
            let mut visited: std::collections::HashSet<usize> = std::collections::HashSet::new();
            for_each_overlay_ref(true, no_skip, |_table, _key, r| {
                let ptr = r.as_ptr() as usize;
                if mine.contains(&ptr) {
                    visited.insert(ptr);
                }
            });

            // Clean up before asserting, so a failure does not poison the
            // process-global tables for every other test.
            for (key, _) in &planted {
                hm_int_fast_shard_for(*key)
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(key);
            }

            assert_eq!(
                visited.len(),
                mine.len(),
                "every planted key and value across {} shards must be visited; \
                 a missed shard is a dropped GC root",
                targets.len()
            );
        }

        /// Helper: initialise a fresh LBQ instance with the given capacity.
        fn make_lbq(ctx: &mut MockCtx, capacity: i32) -> ObjectRef {
            let this = ctx.alloc_lbq_object();
            let _ = native_lbq_init_cap(ctx, &[Value::Object(Some(this)), Value::Int(capacity)]);
            this
        }

        /// Helper: init LBQ + offer a single value, asserting success.
        fn offer_must_succeed(ctx: &mut MockCtx, q: ObjectRef, v: i32) {
            let r = native_lbq_offer_bool(ctx, &[Value::Object(Some(q)), Value::Int(v)]).unwrap();
            assert_eq!(r, Some(Value::Int(1)));
        }

        fn lbq_size(ctx: &MockCtx, q: ObjectRef) -> i32 {
            match ctx.get_field(q, LBQ_FIELD_SIZE) {
                Value::Int(v) => v,
                _ => -1,
            }
        }

        // -------- Test 1: poll/peek/poll_last on empty return null ----
        #[test]
        fn poll_on_empty_returns_null() {
            let mut ctx = MockCtx::new(1);
            let q = make_lbq(&mut ctx, 4);
            let r = native_lbq_poll(&mut ctx, &[Value::Object(Some(q))]).unwrap();
            assert!(matches!(r, Some(Value::Object(None))), "{r:?}");
            let r2 = native_lbq_peek(&mut ctx, &[Value::Object(Some(q))]).unwrap();
            assert!(matches!(r2, Some(Value::Object(None))));
            let r3 = native_lbq_poll_last(&mut ctx, &[Value::Object(Some(q))]).unwrap();
            assert!(matches!(r3, Some(Value::Object(None))));
        }

        // -------- Test 2: put blocks at capacity, wakes on poll ------
        //
        // The producer (main thread) tries to `put` into a full queue;
        // a consumer (spawned thread) polls one slot after 150ms. If
        // the 10k-spin-fallthrough bug were back, the producer's put
        // would return almost instantly (silently overflowing); we
        // assert it took ≥ half the consumer's sleep.
        #[test]
        fn put_blocks_at_capacity_until_poll() {
            use std::time::Instant;
            let mut ctx = MockCtx::new(1);
            let q = make_lbq(&mut ctx, 2);
            offer_must_succeed(&mut ctx, q, 11);
            offer_must_succeed(&mut ctx, q, 22);
            // Third offer must fail — queue at capacity.
            let r3 =
                native_lbq_offer_bool(&mut ctx, &[Value::Object(Some(q)), Value::Int(33)]).unwrap();
            assert_eq!(r3, Some(Value::Int(0)), "third offer must fail at cap");

            let mut consumer_ctx = ctx.fork(2);
            let consumer = std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(150));
                let r = native_lbq_poll(&mut consumer_ctx, &[Value::Object(Some(q))]).unwrap();
                assert!(matches!(r, Some(Value::Int(11))), "consumer polled {r:?}");
            });

            let started = Instant::now();
            let _ = native_lbq_put_blocking(&mut ctx, &[Value::Object(Some(q)), Value::Int(33)]);
            let elapsed = started.elapsed();
            consumer.join().unwrap();

            assert!(
                elapsed >= Duration::from_millis(75),
                "put returned in {elapsed:?} — fallthrough bug is back",
            );
            assert_eq!(lbq_size(&ctx, q), 2);
        }

        // -------- Test 3: concurrent put+poll never loses values -----
        #[test]
        fn concurrent_put_poll_no_lost_values() {
            const N: i32 = 200;
            let mut ctx = MockCtx::new(1);
            let q = make_lbq(&mut ctx, 8); // small cap → exercises the block path

            let mut producer_ctx = ctx.fork(10);
            let producer = std::thread::spawn(move || {
                for i in 0..N {
                    let _ = native_lbq_put_blocking(
                        &mut producer_ctx,
                        &[Value::Object(Some(q)), Value::Int(i)],
                    );
                }
            });

            let mut consumer_ctx = ctx.fork(20);
            let consumer = std::thread::spawn(move || {
                let mut received: Vec<i32> = Vec::with_capacity(N as usize);
                while received.len() < N as usize {
                    if let Ok(Some(Value::Int(v))) =
                        native_lbq_take_blocking(&mut consumer_ctx, &[Value::Object(Some(q))])
                    {
                        received.push(v);
                    }
                }
                received
            });

            producer.join().unwrap();
            let received = consumer.join().unwrap();
            assert_eq!(received.len(), N as usize, "every value delivered");
            // Single producer + single consumer → FIFO order.
            assert_eq!(received, (0..N).collect::<Vec<_>>(), "FIFO order");
            assert_eq!(lbq_size(&ctx, q), 0, "queue drained");
        }

        // -------- Test 4: clear unblocks a waiting put ---------------
        //
        // Pre-fix, `clear` did not notify (and did not take the
        // monitor), so a producer parked in `put` would only unblock
        // via the 50 ms re-check timeout (or never, in real-VM mode).
        // After the fix, clear() notifies, so the put wakes promptly.
        #[test]
        fn clear_unblocks_pending_put() {
            use std::time::Instant;
            let mut ctx = MockCtx::new(1);
            let q = make_lbq(&mut ctx, 1);
            offer_must_succeed(&mut ctx, q, 77);

            let mut clearer_ctx = ctx.fork(3);
            let clearer = std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(100));
                let _ = native_lbq_clear(&mut clearer_ctx, &[Value::Object(Some(q))]);
            });

            let started = Instant::now();
            let _ = native_lbq_put_blocking(&mut ctx, &[Value::Object(Some(q)), Value::Int(88)]);
            let elapsed = started.elapsed();
            clearer.join().unwrap();

            assert!(
                elapsed >= Duration::from_millis(50),
                "put returned in {elapsed:?} without waiting for clear",
            );
            assert_eq!(lbq_size(&ctx, q), 1);
        }

        // -------- Test 5: head-index FIFO drains in order across the
        // backing-buffer boundary (perf P1: O(1) poll via head index) ----
        //
        // Single-threaded, deterministic. With a bounded buffer of len 3
        // (== capacity) we offer 3, poll 2 (head drifts to index 2), then
        // offer 2 more — forcing `lbq_ensure_capacity` to compact the live
        // window back to index 0. Draining must still yield strict FIFO
        // order, proving the head-index poll never reorders or drops
        // elements and that compaction preserves the queue contents.
        #[test]
        fn head_index_fifo_across_buffer_wrap() {
            let mut ctx = MockCtx::new(1);
            let q = make_lbq(&mut ctx, 3);
            // Fill to capacity.
            for v in [10, 20, 30] {
                offer_must_succeed(&mut ctx, q, v);
            }
            // Poll two → head advances; 10 then 20 come out first (FIFO).
            let a = native_lbq_poll(&mut ctx, &[Value::Object(Some(q))]).unwrap();
            let b = native_lbq_poll(&mut ctx, &[Value::Object(Some(q))]).unwrap();
            assert_eq!(a, Some(Value::Int(10)));
            assert_eq!(b, Some(Value::Int(20)));
            assert_eq!(lbq_size(&ctx, q), 1);
            // Offer two more → tail would overflow the buffer, forcing a
            // compaction of the live window (just `30`) back to index 0.
            offer_must_succeed(&mut ctx, q, 40);
            offer_must_succeed(&mut ctx, q, 50);
            assert_eq!(lbq_size(&ctx, q), 3);
            // Drain: must be the remaining elements in strict FIFO order.
            let mut drained = Vec::new();
            for _ in 0..3 {
                if let Some(Value::Int(v)) =
                    native_lbq_poll(&mut ctx, &[Value::Object(Some(q))]).unwrap()
                {
                    drained.push(v);
                }
            }
            assert_eq!(drained, vec![30, 40, 50], "FIFO order across compaction");
            assert_eq!(lbq_size(&ctx, q), 0, "queue fully drained");
            // After full drain the head index resets to 0 so the buffer is
            // reusable from the front.
            assert_eq!(lbq_head(&ctx, q), 0, "head reset on empty");
        }

        // ---- merge_sort_fallible: O(n log n) stable fallible sort ----------

        /// Sorts correctly across several merge passes (n large enough to
        /// exercise width = 1,2,4,8,…) and is stable on equal keys. Each
        /// element packs `(key << 32) | original_index` into a `Value::Long`;
        /// the comparison looks at the key only, so a stable sort must leave
        /// equal-key elements in ascending original-index order.
        #[test]
        fn merge_sort_fallible_sorts_and_is_stable() {
            let mut ctx = MockCtx::new(1);
            // Keys with deliberate duplicates and reverse-ish ordering.
            let keys = [5i64, 3, 5, 1, 3, 9, 5, 2, 3, 8, 1, 7, 0, 3, 5];
            let mut items: Vec<Value> = keys
                .iter()
                .enumerate()
                .map(|(idx, &k)| Value::Long((k << 32) | idx as i64))
                .collect();

            merge_sort_fallible(&mut ctx, &mut items, |_c, a, b| {
                let ka = match a {
                    Value::Long(p) => p >> 32,
                    _ => 0,
                };
                let kb = match b {
                    Value::Long(p) => p >> 32,
                    _ => 0,
                };
                Ok((ka - kb) as i32)
            })
            .unwrap();

            // Decode back to (key, original_index).
            let decoded: Vec<(i64, i64)> = items
                .iter()
                .map(|v| match v {
                    Value::Long(p) => (p >> 32, p & 0xFFFF_FFFF),
                    _ => panic!("unexpected value"),
                })
                .collect();

            // Keys must be non-decreasing.
            for w in decoded.windows(2) {
                assert!(w[0].0 <= w[1].0, "keys sorted: {:?}", decoded);
            }
            // Stable: within an equal-key run, original indices ascend.
            for w in decoded.windows(2) {
                if w[0].0 == w[1].0 {
                    assert!(w[0].1 < w[1].1, "stable on ties: {:?}", decoded);
                }
            }
            // Same multiset of keys as the input (nothing lost/duplicated).
            let mut got: Vec<i64> = decoded.iter().map(|(k, _)| *k).collect();
            got.sort_unstable();
            let mut want: Vec<i64> = keys.to_vec();
            want.sort_unstable();
            assert_eq!(got, want, "all elements present");
        }

        /// An error from the comparison closure short-circuits the whole sort
        /// and is propagated to the caller (mirrors a comparator throwing).
        #[test]
        fn merge_sort_fallible_propagates_comparator_error() {
            let mut ctx = MockCtx::new(1);
            let mut items: Vec<Value> = (0..8).rev().map(Value::Int).collect();
            let res = merge_sort_fallible(&mut ctx, &mut items, |_c, _a, _b| {
                Err(MethodCallFailed::from(
                    cratonvm_types::error::RuntimeError::ClassCastException {
                        message: "boom".to_string(),
                    },
                ))
            });
            assert!(res.is_err(), "comparator error must propagate");
        }

        // ---------------------------------------------------------------
        // JDK-ONLY-LAYOUT (L2) — `publish_map_table` / `props_defaults_slot`
        //
        // The defect these cover is silent by construction: a raw index
        // resolves, type-checks and lands on a DIFFERENT field. So each test
        // states the real layout, does the write, and then asserts on the
        // field that must NOT have moved — the assertion that fails when the
        // resolution regresses to a fixed class name.
        // ---------------------------------------------------------------

        const HASHMAP_CID: ClassId = ClassId::new(41);
        const HASHTABLE_CID: ClassId = ClassId::new(42);
        const PROPERTIES_CID: ClassId = ClassId::new(43);
        const STUB_CID: ClassId = ClassId::new(44);

        /// Real `java/util/HashMap`: `AbstractMap` contributes `keySet`(0) and
        /// `values`(1), so `table` is absolute slot 2 — the same index the
        /// fabricated layout calls `capacity`.
        fn define_real_hashmap(ctx: &MockCtx) {
            ctx.define_class(HASHMAP_CID, "java/util/HashMap");
            ctx.define_field(HASHMAP_CID, "table", 2);
            ctx.define_field(HASHMAP_CID, "size", 4);
            ctx.define_field(HASHMAP_CID, "modCount", 5);
        }

        /// The integer overlay's own receiver: a `java/util/HashMap` with the
        /// generation slots but NO `table` field (the synthetic class library's
        /// shape). JIT round 20 wave 17 (lane hashOff): the overlay is never
        /// started for a receiver with a real `table`, which JDK bytecode reads
        /// directly (`overlay_is_refused_for_a_real_table_receiver`).
        const OVERLAY_HASHMAP_CID: ClassId = ClassId::new(45);

        fn define_overlay_hashmap(ctx: &MockCtx) {
            ctx.define_class(OVERLAY_HASHMAP_CID, "java/util/HashMap");
            ctx.define_field(OVERLAY_HASHMAP_CID, "size", 4);
            ctx.define_field(OVERLAY_HASHMAP_CID, "modCount", 5);
        }

        /// Real `java/util/Properties extends Hashtable`: `Hashtable` declares
        /// `table`(0), `count`(1), `threshold`(2), `loadFactor`(3), and
        /// `Properties` adds `defaults`(8). Absolute slot 2 — where the fixed
        /// `java/util/HashMap` lookup put the bucket array — is an `int`.
        fn define_real_properties(ctx: &MockCtx) {
            ctx.define_class(HASHTABLE_CID, "java/util/Hashtable");
            ctx.define_field(HASHTABLE_CID, "table", 0);
            ctx.define_field(HASHTABLE_CID, "count", 1);
            ctx.define_field(HASHTABLE_CID, "threshold", 2);
            ctx.define_field(HASHTABLE_CID, "loadFactor", 3);
            ctx.define_class(PROPERTIES_CID, "java/util/Properties");
            ctx.define_superclass(PROPERTIES_CID, HASHTABLE_CID);
            ctx.define_field(PROPERTIES_CID, "defaults", 8);
        }

        /// `java.util.Random.nextGaussian()` must return the JDK's seeded
        /// stream AND advance the LCG once per PAIR of calls.
        ///
        /// # Why this exists beside `next_gaussian_matches_jdk_seeded_sequence`
        ///
        /// That test is vacuous for the defect it names, and the shape is worth
        /// keeping in view because it is easy to write by accident: it drives
        /// `rnd_gaussian_pair` and consumes BOTH variates itself, in a loop it
        /// implements in its own body. The defect its doc comment names —
        /// "this used to discard the second and draw a fresh pair every time" —
        /// lives in the CALLER, `native_random_next_gaussian`, which is not on
        /// that test's call path at all. Delete the `RND_FIELD_NEXT_GAUSSIAN`
        /// cache from the native and that test stays green, because the helper
        /// it exercises is still correct. It asserts the side of the fix that
        /// was never broken.
        ///
        /// This one drives the native through a receiver carrying the two real
        /// `java/util/Random` slots, so the caching is what is under test.
        ///
        /// Measured on Temurin jdk-25.0.3+9, not assumed: `new Random(42)`,
        /// six `nextGaussian()` calls, then one `nextInt()`. The discarding
        /// implementation answers -0.9498666368908959 for the second call (the
        /// THIRD value of the conforming stream) and a different `nextInt()`,
        /// so both assertions below separate the two.
        #[test]
        fn next_gaussian_native_caches_the_second_variate_and_advances_once_per_pair() {
            let mut ctx = MockCtx::new(1);
            // Two slots: seed(0) and nextNextGaussian(1) — the layout
            // `native_random_next_gaussian` addresses.
            let rnd = ctx.alloc_object_of(ClassId::new(0), 2);
            ctx.set_field(rnd, RND_FIELD_SEED, Value::Long(rnd_scramble_seed(42)));
            ctx.set_field(rnd, RND_FIELD_NEXT_GAUSSIAN, Value::Object(None));

            fn call(ctx: &mut MockCtx, rnd: ObjectRef) -> f64 {
                match native_random_next_gaussian(ctx, &[Value::Object(Some(rnd))]) {
                    Ok(Some(Value::Double(d))) => d,
                    other => panic!("nextGaussian must answer a Double, got {other:?}"),
                }
            }

            // --- the MECHANISM: one draw feeds two calls -------------------
            let first = call(&mut ctx, rnd);
            assert!(
                matches!(
                    ctx.get_field(rnd, RND_FIELD_NEXT_GAUSSIAN),
                    Value::Double(_)
                ),
                "after the first call the SECOND variate must be cached in nextNextGaussian; an \
                 empty slot here IS the discarding implementation"
            );
            let seed_after_first = ctx.get_field(rnd, RND_FIELD_SEED);
            let second = call(&mut ctx, rnd);
            assert!(
                matches!(
                    ctx.get_field(rnd, RND_FIELD_NEXT_GAUSSIAN),
                    Value::Object(None)
                ),
                "the second call must CONSUME the cached variate, clearing the slot"
            );
            assert_eq!(
                ctx.get_field(rnd, RND_FIELD_SEED),
                seed_after_first,
                "a call served from the cache must not advance the LCG — advancing here is \
                 exactly the 'twice the specified rate' defect"
            );

            // --- the STREAM: six values against the oracle -----------------
            let mut got = vec![first, second];
            while got.len() < 6 {
                got.push(call(&mut ctx, rnd));
            }
            // Same tolerance and same measurement as the sibling test: `log` is
            // the only source of divergence (`sqrt` is IEEE-exact), it differs
            // by at most 1 ulp between fdlibm and the platform libm on these
            // three pairs, and that propagates to at most 1.77e-16 relative.
            // Four ulps is 5x that. It is not what makes this test able to fail
            // — the integer assertion below is.
            const ULPS: f64 = 4.0;
            let expected: [i64; 6] = [
                4607821503525903750,
                4606456510138157127,
                -4616641179245592382,
                -4615707776640798080,
                4598733263062401967,
                4604341753479877564,
            ];
            for (i, want_bits) in expected.iter().enumerate() {
                let want = f64::from_bits(*want_bits as u64);
                let tol = ULPS * f64::EPSILON * want.abs();
                assert!(
                    (got[i] - want).abs() <= tol,
                    "nextGaussian[{i}] diverged from `new Random(42)` on the oracle: got {:?}, \
                     want {want:?} (tolerance {tol:.3e} = {ULPS} ulps)",
                    got[i]
                );
            }

            // --- the RATE, exactly, with no tolerance in it ----------------
            // `nextInt()` is `next(32)`. A tolerance cannot blur an i32, so
            // this is the assertion that cannot be widened into uselessness: an
            // implementation that drew a fresh pair per call has consumed twice
            // as much of the stream and lands somewhere else entirely.
            assert_eq!(
                rnd_next(&mut ctx, rnd, 32),
                1583910553,
                "after six nextGaussian() calls the LCG must be exactly where `new Random(42)` \
                 leaves it on Temurin jdk-25.0.3+9"
            );
        }

        /// A real `HashMap` receiver keeps today's behaviour exactly: the
        /// bucket array at slot 0 AND at the real `table` slot, and no legacy
        /// `Int(capacity)` — writing one there is what put an `Int` in the
        /// `table` field (`arraylength` on an int) and produced the
        /// `HashMap` slot-2 census row.
        #[test]
        fn publish_map_table_writes_only_the_receivers_own_table_slot() {
            let mut ctx = MockCtx::new(1);
            define_real_hashmap(&ctx);
            let map = ctx.alloc_object_of(HASHMAP_CID, 8);
            // A sentinel in slot 0 (`AbstractMap.keySet` on a real HashMap):
            // the test is that publishing a table does not touch it.
            let sentinel = ctx.alloc_object_of(STUB_CID, 1);
            ctx.set_field(map, MAP_FIELD_BUCKETS, Value::Object(Some(sentinel)));
            let buckets = ctx.new_ref_array(ClassId::new(0), 16);

            publish_map_table(&mut ctx, map, buckets, 16);

            assert!(
                matches!(ctx.get_field(map, 2), Value::Object(Some(b)) if b.as_ptr() == buckets.as_ptr()),
                "the real `table` slot holds the array"
            );
            assert!(
                matches!(ctx.get_field(map, MAP_FIELD_BUCKETS), Value::Object(Some(o)) if o.as_ptr() == sentinel.as_ptr()),
                "slot 0 is `AbstractMap.keySet` on this receiver and must be \
                 left alone — HotSpot keeps it null until keySet() is called, \
                 and a non-null value there short-circuits the JDK's lazy init"
            );
            // Read-back through the accessor the natives use: the array must
            // still be findable, or every map operation breaks.
            assert_eq!(map_buckets_slot(&ctx, map), 2);
            assert!(
                matches!(map_state(&ctx, map).0, Some(b) if b.as_ptr() == buckets.as_ptr()),
                "map_state must find the table where publish_map_table put it"
            );
        }

        /// The receiver the whole lane is about. `Properties` is excluded from
        /// `CF_HASHTABLE_LAYOUT`, so it reaches the legacy branch; its own
        /// `table` is slot 0, and slot 2 is `threshold`, an `int` that must
        /// survive untouched.
        #[test]
        fn publish_map_table_leaves_a_properties_int_field_alone() {
            let mut ctx = MockCtx::new(1);
            // A real VM has `java/util/HashMap` loaded too — that is the whole
            // point: the old lookup found ITS `table` (index 2) and wrote it
            // into a `Properties`. Without this the fixture cannot express the
            // defect and the test passes vacuously.
            define_real_hashmap(&ctx);
            define_real_properties(&ctx);
            let props = ctx.alloc_object_of(PROPERTIES_CID, 10);
            ctx.set_field(props, 2, Value::Int(12)); // threshold
            let buckets = ctx.new_ref_array(ClassId::new(0), 16);

            publish_map_table(&mut ctx, props, buckets, 16);

            assert!(
                matches!(ctx.get_field(props, MAP_FIELD_BUCKETS), Value::Object(Some(b)) if b.as_ptr() == buckets.as_ptr()),
                "`Hashtable.table` IS slot 0, so one write covers both roles"
            );
            assert_eq!(
                ctx.get_field(props, 2),
                Value::Int(12),
                "slot 2 is `Hashtable.threshold` on this receiver — the bucket \
                 array must not land there (the surviving census row)"
            );
        }

        /// A fabricated receiver — nothing declares a field called `table` —
        /// keeps the legacy `(buckets, size, capacity)` model, including the
        /// `Int` at slot 2. The predicate is the field NAME, never the slot
        /// count: this stub has MORE slots than the real `HashMap` above.
        #[test]
        fn publish_map_table_keeps_the_capacity_int_on_the_fabricated_layout() {
            let mut ctx = MockCtx::new(1);
            define_real_hashmap(&ctx);
            ctx.define_class(STUB_CID, "cratonvm/util/MapViewBacking");
            let backing = ctx.alloc_object_of(STUB_CID, 16);
            let buckets = ctx.new_ref_array(ClassId::new(0), 32);

            publish_map_table(&mut ctx, backing, buckets, 32);

            assert!(
                matches!(ctx.get_field(backing, MAP_FIELD_BUCKETS), Value::Object(Some(b)) if b.as_ptr() == buckets.as_ptr()),
                "slot 0 always holds the array"
            );
            assert_eq!(
                ctx.get_field(backing, MAP_FIELD_CAPACITY),
                Value::Int(32),
                "slot 2 means `capacity` on our own layout"
            );
        }

        /// `native_props_init`'s `Object(None)` used to land on `loadFactor`,
        /// and the overlay hunter cannot see it (`Object(None)` over a
        /// primitive is not reported). Resolve `defaults` on the receiver.
        #[test]
        fn props_defaults_resolves_the_real_field_and_spares_load_factor() {
            let mut ctx = MockCtx::new(1);
            define_real_properties(&ctx);
            let props = ctx.alloc_object_of(PROPERTIES_CID, 10);
            ctx.set_field(props, 3, Value::Float(0.75)); // loadFactor
            let parent = ctx.alloc_object_of(PROPERTIES_CID, 10);

            props_set_defaults(&mut ctx, props, Value::Object(Some(parent)));

            assert_eq!(
                ctx.get_field(props, 3),
                Value::Float(0.75),
                "slot 3 is `loadFactor` on a real Properties"
            );
            assert!(
                matches!(ctx.get_field(props, 8), Value::Object(Some(p)) if p.as_ptr() == parent.as_ptr()),
                "the reference belongs in the real `defaults` field"
            );
            assert!(
                matches!(props_get_defaults(&ctx, props), Value::Object(Some(p)) if p.as_ptr() == parent.as_ptr()),
                "and the getProperty fallback walk must read it back — reading \
                 raw slot 3 got a Float and stopped the chain dead"
            );
        }

        /// A fabricated `Properties` stub has no `defaults` field to resolve,
        /// so the four-field model's slot 3 stays in force.
        #[test]
        fn props_defaults_falls_back_to_the_model_slot_on_a_stub() {
            let mut ctx = MockCtx::new(1);
            ctx.define_class(STUB_CID, "java/util/Properties");
            let props = ctx.alloc_object_of(STUB_CID, 4);
            let parent = ctx.alloc_object_of(STUB_CID, 4);

            props_set_defaults(&mut ctx, props, Value::Object(Some(parent)));

            assert!(
                matches!(ctx.get_field(props, PROPS_FIELD_DEFAULTS), Value::Object(Some(p)) if p.as_ptr() == parent.as_ptr()),
                "synthetic layout keeps slot 3"
            );
            assert!(
                matches!(props_get_defaults(&ctx, props), Value::Object(Some(p)) if p.as_ptr() == parent.as_ptr()),
                "and reads back from the same slot"
            );
        }

        /// `map_state`'s bucket fallback used to probe `HashMap`'s `table`
        /// index on any receiver. On a `Properties` that index is an `int`
        /// field; the receiver's own `table` is slot 0.
        #[test]
        fn receiver_table_slot_answers_per_receiver_not_per_hashmap() {
            let ctx = MockCtx::new(1);
            define_real_hashmap(&ctx);
            define_real_properties(&ctx);
            let map = ctx.alloc_object_of(HASHMAP_CID, 8);
            let props = ctx.alloc_object_of(PROPERTIES_CID, 10);
            ctx.define_class(STUB_CID, "cratonvm/util/MapViewBacking");
            let stub = ctx.alloc_object_of(STUB_CID, 16);

            assert_eq!(receiver_table_slot(&ctx, map), Some(2));
            assert_eq!(receiver_table_slot(&ctx, props), Some(0));
            assert_eq!(
                receiver_table_slot(&ctx, stub),
                None,
                "no `table` field means our own layout — asked by NAME, and \
                 note this stub is WIDER than the real HashMap, so a field \
                 count could not have told them apart"
            );
        }

        /// `size` lives where the receiver declares it, and the read and the
        /// write must agree — they are the same function.
        #[test]
        fn map_size_slot_is_the_receivers_own_count_field() {
            let ctx = MockCtx::new(1);
            define_real_hashmap(&ctx);
            define_real_properties(&ctx);
            ctx.define_class(STUB_CID, "cratonvm/util/MapViewBacking");

            let map = ctx.alloc_object_of(HASHMAP_CID, 8);
            let props = ctx.alloc_object_of(PROPERTIES_CID, 10);
            let stub = ctx.alloc_object_of(STUB_CID, 16);

            // `HashMap.size` is slot 4 — NOT slot 1, which is
            // `AbstractMap.values` and took the Int for years.
            assert_eq!(map_size_slot(&ctx, map), 4);
            // `Properties` has no `size`; it inherits `Hashtable.count` at 1.
            // The old code got this right only because the raw model slot
            // happens to be 1 as well.
            assert_eq!(map_size_slot(&ctx, props), 1);
            // Fabricated: the model's slot 1 stands.
            assert_eq!(map_size_slot(&ctx, stub), MAP_FIELD_SIZE);
        }

        /// The round-trip that matters: whatever `set_map_size` writes,
        /// `map_state` reads back, on every layout.
        #[test]
        fn map_size_round_trips_on_every_layout() {
            let mut ctx = MockCtx::new(1);
            define_real_hashmap(&ctx);
            define_real_properties(&ctx);
            ctx.define_class(STUB_CID, "cratonvm/util/MapViewBacking");

            for (label, obj) in [
                ("hashmap", ctx.alloc_object_of(HASHMAP_CID, 8)),
                ("properties", ctx.alloc_object_of(PROPERTIES_CID, 10)),
                ("stub", ctx.alloc_object_of(STUB_CID, 16)),
            ] {
                set_map_size(&mut ctx, obj, 7);
                assert_eq!(map_state(&ctx, obj).1, 7, "size round-trip on {label}");
            }
        }

        /// Slot 1 on a real `HashMap` is `AbstractMap.values`. Nothing may
        /// write it — that is the 8,000-hit census row this lane removes.
        #[test]
        fn set_map_size_leaves_abstract_map_values_alone() {
            let mut ctx = MockCtx::new(1);
            define_real_hashmap(&ctx);
            let map = ctx.alloc_object_of(HASHMAP_CID, 8);
            let sentinel = ctx.alloc_object_of(HASHMAP_CID, 1);
            ctx.set_field(map, 1, Value::Object(Some(sentinel)));

            set_map_size(&mut ctx, map, 3);

            assert!(
                matches!(ctx.get_field(map, 1), Value::Object(Some(o)) if o.as_ptr() == sentinel.as_ptr()),
                "`values` must survive a size write"
            );
            assert_eq!(ctx.get_field(map, 4), Value::Int(3), "`size` got it");
        }

        /// `Properties.getProperty`'s own type test — JDK 25
        /// `Properties.java:1099`: `(oval instanceof String) ? (String)oval :
        /// null`. MEASURED (W8-E29-1): `p.put("k", someInteger)` then
        /// `p.getProperty("k")` answered the `Integer` back out of a
        /// `()Ljava/lang/String;` method because the native filtered on
        /// nothing but "present".
        #[test]
        fn props_value_is_string_admits_a_string_and_refuses_everything_else() {
            let ctx = MockCtx::new(1);
            let s = ctx.define_string(&[b'k' as u16]);
            assert!(props_value_is_string(&ctx, s));

            ctx.define_class(STUB_CID, "java/lang/Integer");
            let boxed = ctx.alloc_object_of(STUB_CID, 1);
            assert!(!props_value_is_string(&ctx, boxed));
        }

        /// The bound matters: a real class can declare `table` at an index the
        /// receiver was allocated too small for (a backing map sized to
        /// `MAP_NUM_FIELDS`), and a write there would be dropped or land out
        /// of range.
        #[test]
        fn receiver_table_slot_is_bounded_by_the_allocated_slots() {
            let ctx = MockCtx::new(1);
            define_real_hashmap(&ctx);
            let undersized = ctx.alloc_object_of(HASHMAP_CID, 2);
            assert_eq!(receiver_table_slot(&ctx, undersized), None);
        }
    }

    /// JIT round 16 wave 9 (lane spliter): the synthetic spliterators report
    /// HotSpot's comparator, characteristics, split policy and estimate
    /// (`r16-collections-compatible-spliterator-shims-diverge-FIXED-20261001.md`;
    /// expected values measured by `R16SpliterW9` / `R16CollectionsSpliter`
    /// on HotSpot 25).
    mod spliter_w9_tests {
        use super::super::*;
        use super::lbq_blocking_tests::MockCtx;
        #[allow(unused_imports)]
        use cratonvm_native_api::{
            NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
            NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
        };

        const STREAM: ClassId = ClassId::new(0);
        const SPL: ClassId = ClassId::new(1);
        const CMP: ClassId = ClassId::new(2);

        fn mock() -> MockCtx {
            let ctx = MockCtx::new(1);
            ctx.define_class(STREAM, "java/util/stream/Stream");
            ctx.define_class(SPL, "java/util/Spliterator");
            ctx.define_class(CMP, "java/util/Collections$ReverseComparator");
            ctx
        }

        fn ints(ctx: &mut MockCtx, n: usize) -> ObjectRef {
            let arr = ctx.new_ref_array(ClassId::new(0), n);
            for i in 0..n {
                ctx.set_array_element(arr, i, Value::Int(i as i32));
            }
            arr
        }

        fn source_stream(ctx: &mut MockCtx, n: usize, chars: i32) -> ObjectRef {
            let arr = ints(ctx, n);
            let s = ctx.alloc_object(STREAM, STREAM_NUM_FIELDS);
            ctx.set_field(s, STREAM_FIELD_ELEMENTS, Value::Object(Some(arr)));
            stream_set_source_spl_facts(ctx, s, chars, Value::Object(None));
            s
        }

        /// A wave-9 spliterator over `n` ints, as the producers build it.
        fn facts_spliterator(ctx: &mut MockCtx, n: usize, chars: i32, cmp: Value) -> ObjectRef {
            let arr = ints(ctx, n);
            alloc_facts_spliterator(ctx, arr, chars, cmp, None).expect("mock allocates")
        }

        fn obj(v: MethodCallResult) -> Option<ObjectRef> {
            match v {
                Ok(Some(Value::Object(o))) => o,
                other => panic!("expected an object or null, got {other:?}"),
            }
        }

        fn chars_of(ctx: &mut MockCtx, spl: ObjectRef) -> i32 {
            match native_spliterator_characteristics(ctx, &[Value::Object(Some(spl))]) {
                Ok(Some(Value::Int(c))) => c,
                other => panic!("characteristics: {other:?}"),
            }
        }

        fn est_of(ctx: &mut MockCtx, spl: ObjectRef) -> i64 {
            match native_spliterator_estimate_size(ctx, &[Value::Object(Some(spl))]) {
                Ok(Some(Value::Long(n))) => n,
                other => panic!("estimateSize: {other:?}"),
            }
        }

        fn spliterator_of(ctx: &mut MockCtx, stream: ObjectRef) -> ObjectRef {
            let r = native_stream_spliterator(ctx, &[Value::Object(Some(stream))]);
            obj(r).expect("a spliterator")
        }

        fn comparator_of(ctx: &mut MockCtx, spl: ObjectRef) -> MethodCallResult {
            native_spliterator_get_comparator(ctx, &[Value::Object(Some(spl))])
        }

        fn split(ctx: &mut MockCtx, spl: ObjectRef) -> Option<ObjectRef> {
            obj(native_spliterator_try_split(
                ctx,
                &[Value::Object(Some(spl))],
            ))
        }

        fn on<R>(f: impl FnOnce() -> R) -> R {
            cratonvm_types::flags::with_thread_overrides(
                &[
                    ("CRATONVM_SPLITERATOR_FACTS", None),
                    ("CRATONVM_SPLITERATOR_JDK_FALLBACK", None),
                ],
                f,
            )
        }

        /// The flag algebra against every stage `R16SpliterW9` measured.
        #[test]
        fn wrapped_characteristics_match_hotspot() {
            let of3 = SPL_IMMUTABLE_ARRAY; // Stream.of(x, y, z): 0x4450
            let range = SPL_RANGE; // IntStream.range: 0x4555
            for (upstream, op, want, row) in [
                (of3, SplOp::Preserve, 0x4050, "Stream.of.peek"),
                (of3, SplOp::Map, 0x4050, "Stream.of.map"),
                (of3, SplOp::Filter, 0x10, "Stream.of.filter"),
                (of3, SplOp::SortedNatural, 0x4054, "Stream.of.sorted"),
                (of3, SplOp::SortedCmp, 0x4050, "Stream.of.sortedCmp"),
                (of3, SplOp::Distinct, 0x11, "Stream.of.distinct"),
                (of3, SplOp::FlatMap, 0x10, "Stream.of.flatMap"),
                (range, SplOp::Preserve, 0x4055, "IntStream.range.boxed"),
                (range, SplOp::Map, 0x4050, "IntStream.range.mapToObj"),
                (SPL_LIST_DEFAULT, SplOp::Preserve, 0x4050, "IntStream.of"),
            ] {
                assert_eq!(spl_wrapped_characteristics(upstream, op), want, "{row}");
            }
            // A stage over a stage: `range.boxed().filter(..)` is 0x15,
            // `range.boxed().sorted()` keeps 0x4055.
            let boxed = spl_wrapped_characteristics(range, SplOp::Preserve);
            assert_eq!(spl_wrapped_characteristics(boxed, SplOp::Filter), 0x15);
            let sorted = spl_wrapped_characteristics(boxed, SplOp::SortedNatural);
            assert_eq!(sorted, 0x4055);
        }

        #[test]
        fn packed_facts_round_trip() {
            let mut ctx = mock();
            let s = ctx.alloc_object(STREAM, STREAM_NUM_FIELDS);
            assert_eq!(stream_spl_facts(&ctx, s), None, "a fresh stream");
            let chars = 0x15 | SPL_PRIV_WRAPPED;
            stream_set_spl_facts(&mut ctx, s, pack_spl_facts(chars, Some(4)));
            assert_eq!(stream_spl_facts(&ctx, s), Some((chars, Some(4))));
            stream_set_spl_facts(&mut ctx, s, pack_spl_facts(SPL_RANGE, None));
            assert_eq!(stream_spl_facts(&ctx, s), Some((SPL_RANGE, None)));
            // A short-layout stream has no slot and stays undescribed.
            let short = ctx.alloc_object(STREAM, 2);
            stream_set_spl_facts(&mut ctx, short, pack_spl_facts(SPL_RANGE, None));
            assert_eq!(stream_spl_facts(&ctx, short), None);
        }

        /// `IntStream.range(0, 4).boxed().filter(..).spliterator()`: 0x15, the
        /// SOURCE's estimate 4 although only 3 elements survive, no split,
        /// natural comparator.
        #[test]
        fn a_filtered_stage_reports_the_wrapping_spliterator() {
            on(|| {
                let mut ctx = mock();
                let s = source_stream(&mut ctx, 4, SPL_RANGE);
                let kept = [Value::Int(1), Value::Int(2), Value::Int(3)];
                let d = obj(make_derived_stream(&mut ctx, s, &kept, SplOp::Filter)).unwrap();
                assert_eq!(
                    stream_spl_facts(&ctx, d),
                    Some((0x15 | SPL_PRIV_WRAPPED, Some(4)))
                );
                let spl = spliterator_of(&mut ctx, d);
                assert_eq!(chars_of(&mut ctx, spl), 0x15);
                assert_eq!(est_of(&mut ctx, spl), 4);
                assert_eq!(obj(comparator_of(&mut ctx, spl)), None);
                assert_eq!(split(&mut ctx, spl), None, "sequential: no split");
            });
        }

        /// `Stream.of(x, y, z).spliterator()` is the source's `ArraySpliterator`:
        /// 0x4450, splittable, `getComparator()` throws.
        #[test]
        fn a_source_stage_reports_its_own_spliterator() {
            on(|| {
                let mut ctx = mock();
                let s = source_stream(&mut ctx, 3, SPL_IMMUTABLE_ARRAY);
                let spl = spliterator_of(&mut ctx, s);
                assert_eq!(chars_of(&mut ctx, spl), 0x4450);
                assert_eq!(est_of(&mut ctx, spl), 3);
                assert!(comparator_of(&mut ctx, spl).is_err());
                let left = split(&mut ctx, spl).expect("three elements split");
                assert_eq!(chars_of(&mut ctx, left), 0x4450, "the prefix keeps them");
            });
        }

        /// The tree split policy: two elements do not split (`TreeMap.values()`
        /// over two ascending puts), four do; the prefix keeps SORTED and the
        /// comparator.
        #[test]
        fn a_tree_spliterator_declines_two_elements_and_splits_four() {
            on(|| {
                let mut ctx = mock();
                let two = facts_spliterator(
                    &mut ctx,
                    2,
                    SPL_TREE_VALUES | SPL_PRIV_TREE_SPLIT,
                    Value::Object(None),
                );
                assert_eq!(chars_of(&mut ctx, two), 0x50, "private bits stay private");
                assert_eq!(split(&mut ctx, two), None);
                let cmp = ctx.alloc_object(CMP, 0);
                let four = facts_spliterator(
                    &mut ctx,
                    4,
                    SPL_TREE_SET | SPL_PRIV_TREE_SPLIT,
                    Value::Object(Some(cmp)),
                );
                let left = split(&mut ctx, four).expect("four elements split");
                assert_eq!(chars_of(&mut ctx, left), 0x55);
                assert_eq!(
                    obj(comparator_of(&mut ctx, left)),
                    Some(cmp),
                    "a comparator-ordered TreeSet answers its comparator, not null"
                );
            });
        }

        /// A sub-map key spliterator never splits.
        #[test]
        fn a_no_split_spliterator_declines() {
            on(|| {
                let mut ctx = mock();
                let spl = facts_spliterator(
                    &mut ctx,
                    6,
                    SPL_DISTINCT | SPL_SORTED | SPL_ORDERED | SPL_PRIV_NO_SPLIT,
                    Value::Object(None),
                );
                assert_eq!(split(&mut ctx, spl), None);
                assert_eq!(est_of(&mut ctx, spl), i64::MAX, "unsized, non-concurrent");
            });
        }

        /// `CRATONVM_SPLITERATOR_FACTS=0` restores the pre-wave-9 answers: the
        /// tree policy is not consulted and a stream answers the list default.
        #[test]
        fn the_kill_switch_restores_the_old_answers() {
            cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_SPLITERATOR_FACTS", Some("0"))],
                || {
                    let mut ctx = mock();
                    let two = facts_spliterator(
                        &mut ctx,
                        2,
                        SPL_TREE_VALUES | SPL_PRIV_TREE_SPLIT,
                        Value::Object(None),
                    );
                    assert!(split(&mut ctx, two).is_some());
                    let s = source_stream(&mut ctx, 3, SPL_IMMUTABLE_ARRAY);
                    let spl = spliterator_of(&mut ctx, s);
                    assert_eq!(ctx.object_num_fields(spl), 3, "the three-field snapshot");
                    assert_eq!(chars_of(&mut ctx, spl), SPL_LIST_DEFAULT);
                },
            );
        }

        // ---- Round 16 wave 13, lane compatcoll ----------------------------

        /// The two-element red-black root rule (`R16SpliterW9` `TreeSet.two`
        /// inserted 1, 2 vs `TreeSet.two.rev` inserted 2, 1).
        #[test]
        fn a_two_element_tree_is_rooted_where_hotspot_roots_it() {
            // 1 -> 2 elements: the old element stays the root.
            assert_eq!(pair_root_after_insert(1, 1), Some(false), "1 then 2");
            assert_eq!(pair_root_after_insert(1, 0), Some(true), "2 then 1");
            assert_eq!(pair_root_after_insert(0, 0), None);
            assert_eq!(pair_root_after_insert(2, 0), None);
            // 3 -> 2 elements: the middle element was the root.
            assert_eq!(pair_root_after_remove(3, 0), Some(false), "smallest gone");
            assert_eq!(
                pair_root_after_remove(3, 1),
                Some(true),
                "root gone: successor"
            );
            assert_eq!(pair_root_after_remove(3, 2), Some(true), "largest gone");
            assert_eq!(pair_root_after_remove(4, 0), None);
            assert_eq!(pair_root_after_remove(2, 0), None);
        }

        /// `SPL_PRIV_PAIR_SPLIT` lets a whole two-element tree split; without
        /// it two elements still decline, and three always split.
        #[test]
        fn a_two_element_tree_rooted_past_its_first_element_splits() {
            let tree = SPL_PRIV_TREE_SPLIT;
            let pair = SPL_PRIV_TREE_SPLIT | SPL_PRIV_PAIR_SPLIT;
            assert!(!tree_split_allowed(tree, 2));
            assert!(tree_split_allowed(pair, 2));
            assert!(!tree_split_allowed(pair, 1));
            assert!(tree_split_allowed(tree, 3));
            on(|| {
                let mut ctx = mock();
                let two = facts_spliterator(
                    &mut ctx,
                    2,
                    SPL_TREE_SET | SPL_PRIV_TREE_SPLIT | SPL_PRIV_PAIR_SPLIT,
                    Value::Object(None),
                );
                assert_eq!(chars_of(&mut ctx, two), 0x55, "the pair bit stays private");
                let left = split(&mut ctx, two).expect("root is the larger: split");
                assert_eq!(chars_of(&mut ctx, left), 0x55);
                assert_eq!(split(&mut ctx, left), None, "one element left");
            });
        }

        /// `Stream.of(x, y, z).unordered().spliterator()`: a new stage,
        /// 0x4040, the source's estimate, no split -- and the source is linked.
        #[test]
        fn unordered_on_an_ordered_stream_is_a_new_unordered_stage() {
            assert_eq!(
                spl_wrapped_characteristics(SPL_IMMUTABLE_ARRAY, SplOp::Unordered),
                0x4040
            );
            // 0x4041, not 0x4045: `NOT_ORDERED` clears SORTED with ORDERED,
            // as HotSpot 25.0.3 reports (`IntStream.range(0, 3).unordered()` and
            // its `boxed()` twin both answer 0x4041; measured 2026-10-06, lane
            // testfix18). Round 18 wave 11 `CRATONVM_STREAM_UNORDERED_HOTSPOT`
            // (default ON); `=0` restores the round-16 0x4045.
            assert_eq!(
                spl_wrapped_characteristics(SPL_RANGE, SplOp::Unordered),
                0x4041
            );
            on(|| {
                let mut ctx = mock();
                let s = source_stream(&mut ctx, 3, SPL_IMMUTABLE_ARRAY);
                let u = obj(native_stream_unordered(&mut ctx, &[Value::Object(Some(s))]))
                    .expect("a stage");
                assert_ne!(u, s, "an ORDERED stage gets a new one");
                assert!(stream_is_linked(&ctx, s), "the source is linked");
                let spl = spliterator_of(&mut ctx, u);
                assert_eq!(chars_of(&mut ctx, spl), 0x4040);
                assert_eq!(est_of(&mut ctx, spl), 3);
                assert_eq!(split(&mut ctx, spl), None, "sequential: no split");
                // Unordered already: `return this`.
                let again = obj(native_stream_unordered(&mut ctx, &[Value::Object(Some(u))]));
                assert_eq!(again, Some(u));
                // A stage nothing described keeps the identity.
                let bare = ctx.alloc_object(STREAM, STREAM_NUM_FIELDS);
                let r = obj(native_stream_unordered(
                    &mut ctx,
                    &[Value::Object(Some(bare))],
                ));
                assert_eq!(r, Some(bare));
            });
        }

        /// `CRATONVM_STREAM_UNORDERED_STAGE=0` restores the identity.
        #[test]
        fn the_unordered_kill_switch_restores_the_identity() {
            cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_STREAM_UNORDERED_STAGE", Some("0"))],
                || {
                    let mut ctx = mock();
                    let s = source_stream(&mut ctx, 3, SPL_IMMUTABLE_ARRAY);
                    let r = obj(native_stream_unordered(&mut ctx, &[Value::Object(Some(s))]));
                    assert_eq!(r, Some(s));
                    assert!(!stream_is_linked(&ctx, s));
                },
            );
        }

        /// SPL9-1: a SOURCE stage's recorded characteristics reach the
        /// primitive `spliterator()` natives; a wrapped stage's do not.
        #[test]
        fn a_source_stage_hands_its_characteristics_to_the_primitive_spliterator() {
            on(|| {
                let mut ctx = mock();
                let s = source_stream(&mut ctx, 3, SPL_RANGE);
                assert_eq!(
                    synthetic_stream_spliterator_characteristics(&ctx, s),
                    Some(0x4555)
                );
                let kept = [Value::Int(1)];
                let d = obj(make_derived_stream(&mut ctx, s, &kept, SplOp::Filter)).unwrap();
                assert_eq!(synthetic_stream_spliterator_characteristics(&ctx, d), None);
                let bare = ctx.alloc_object(STREAM, STREAM_NUM_FIELDS);
                assert_eq!(
                    synthetic_stream_spliterator_characteristics(&ctx, bare),
                    None
                );
            });
            cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_PRIMITIVE_SPLITERATOR_FACTS", Some("0"))],
                || {
                    let mut ctx = mock();
                    let s = source_stream(&mut ctx, 3, SPL_RANGE);
                    assert_eq!(synthetic_stream_spliterator_characteristics(&ctx, s), None);
                },
            );
        }
    }

    /// Round 16 wave 14, lane compat2: the parallel stage bit (CC13-3), the
    /// source facts of the one-element primitive `of`s, `String.chars()` and
    /// the hash containers (CC13-1, SPL9-2), and `Collections.sort`'s virtual
    /// `list.sort(c)` (cmecompat backlog b5).
    mod compat2_w14_tests {
        use super::super::*;
        use super::lbq_blocking_tests::MockCtx;
        #[allow(unused_imports)]
        use cratonvm_native_api::{
            NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
            NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
        };

        const STREAM: ClassId = ClassId::new(0);
        const SPL: ClassId = ClassId::new(1);
        const INT_STREAM: ClassId = ClassId::new(2);
        const ARRAY_LIST: ClassId = ClassId::new(3);
        const SORTING_LIST: ClassId = ClassId::new(4);
        const SORTING_SUB: ClassId = ClassId::new(5);
        const PLAIN_LIST: ClassId = ClassId::new(6);
        const SORT_DESC: &str = "(Ljava/util/Comparator;)V";

        fn mock() -> MockCtx {
            let ctx = MockCtx::new(1);
            ctx.define_class(STREAM, "java/util/stream/Stream");
            ctx.define_class(SPL, "java/util/Spliterator");
            ctx.define_class(INT_STREAM, "java/util/stream/IntStream");
            ctx.define_class(ARRAY_LIST, "java/util/ArrayList");
            ctx.define_class(SORTING_LIST, "app/SortingList");
            ctx.define_class(SORTING_SUB, "app/SortingSubList");
            ctx.define_class(PLAIN_LIST, "app/PlainList");
            ctx
        }

        /// Every switch this module reads at its default.
        fn defaults<R>(f: impl FnOnce() -> R) -> R {
            cratonvm_types::flags::with_thread_overrides(
                &[
                    ("CRATONVM_SPLITERATOR_FACTS", None),
                    ("CRATONVM_SPLITERATOR_JDK_FALLBACK", None),
                    ("CRATONVM_STREAM_PARALLEL_STAGE", None),
                    ("CRATONVM_STREAM_SOURCE_FACTS", None),
                    ("CRATONVM_COMPAT_COLLECTIONS_SORT_VIRTUAL", None),
                ],
                f,
            )
        }

        fn source_stream(ctx: &mut MockCtx, n: usize, chars: i32) -> ObjectRef {
            let arr = ctx.new_ref_array(ClassId::new(0), n);
            for i in 0..n {
                ctx.set_array_element(arr, i, Value::Int(i as i32));
            }
            let s = ctx.alloc_object(STREAM, STREAM_NUM_FIELDS);
            ctx.set_field(s, STREAM_FIELD_ELEMENTS, Value::Object(Some(arr)));
            stream_set_source_spl_facts(ctx, s, chars, Value::Object(None));
            s
        }

        fn obj(v: MethodCallResult) -> Option<ObjectRef> {
            match v {
                Ok(Some(Value::Object(o))) => o,
                other => panic!("expected an object or null, got {other:?}"),
            }
        }

        fn is_parallel(ctx: &mut MockCtx, s: ObjectRef) -> bool {
            match native_stream_is_parallel(ctx, &[Value::Object(Some(s))]) {
                Ok(Some(Value::Int(b))) => b != 0,
                other => panic!("isParallel: {other:?}"),
            }
        }

        fn parallel(ctx: &mut MockCtx, s: ObjectRef) {
            let r = obj(native_stream_parallel(ctx, &[Value::Object(Some(s))]));
            assert_eq!(r, Some(s), "parallel() answers this");
        }

        fn sequential(ctx: &mut MockCtx, s: ObjectRef) {
            let r = obj(native_stream_sequential(ctx, &[Value::Object(Some(s))]));
            assert_eq!(r, Some(s), "sequential() answers this");
        }

        fn chars_of(ctx: &mut MockCtx, spl: ObjectRef) -> i32 {
            match native_spliterator_characteristics(ctx, &[Value::Object(Some(spl))]) {
                Ok(Some(Value::Int(c))) => c,
                other => panic!("characteristics: {other:?}"),
            }
        }

        fn mapped_spliterator(ctx: &mut MockCtx, src: ObjectRef) -> ObjectRef {
            let kept = [Value::Int(0), Value::Int(1), Value::Int(2), Value::Int(3)];
            let d = obj(make_derived_stream(ctx, src, &kept, SplOp::Map)).expect("a stage");
            obj(native_stream_spliterator(ctx, &[Value::Object(Some(d))])).expect("a spliterator")
        }

        fn split(ctx: &mut MockCtx, spl: ObjectRef) -> Option<ObjectRef> {
            obj(native_spliterator_try_split(
                ctx,
                &[Value::Object(Some(spl))],
            ))
        }

        /// The parallel bit rides outside the characteristics: the head-size
        /// cap keeps it clear, a slot holding only it describes nothing, and a
        /// facts write keeps it.
        #[test]
        fn the_parallel_bit_is_kept_apart_from_the_facts() {
            assert!(pack_spl_facts(SPL_RANGE, Some(usize::MAX)) >= 0);
            assert_eq!(
                pack_spl_facts(0, Some(usize::MAX)) >> 32,
                i64::from(i32::MAX)
            );
            defaults(|| {
                let mut ctx = mock();
                let s = ctx.alloc_object(STREAM, STREAM_NUM_FIELDS);
                assert!(!stream_is_parallel(&ctx, s));
                parallel(&mut ctx, s);
                assert!(stream_is_parallel(&ctx, s));
                assert_eq!(
                    stream_spl_facts(&ctx, s),
                    None,
                    "the bit alone describes nothing"
                );
                stream_set_spl_facts(&mut ctx, s, pack_spl_facts(SPL_RANGE, Some(3)));
                assert_eq!(stream_spl_facts(&ctx, s), Some((SPL_RANGE, Some(3))));
                assert!(stream_is_parallel(&ctx, s), "a facts write keeps the bit");
                sequential(&mut ctx, s);
                assert!(!stream_is_parallel(&ctx, s));
                assert_eq!(stream_spl_facts(&ctx, s), Some((SPL_RANGE, Some(3))));
            });
        }

        /// `Stream.of(1, 2, 3).parallel().isParallel()` is true, and
        /// `.sequential()` clears it; a carrier too short for the slot, or a
        /// non-stream receiver, keeps the old constant `false`.
        #[test]
        fn is_parallel_reads_what_parallel_and_sequential_wrote() {
            defaults(|| {
                let mut ctx = mock();
                let s = source_stream(&mut ctx, 3, SPL_IMMUTABLE_ARRAY);
                assert!(!is_parallel(&mut ctx, s));
                parallel(&mut ctx, s);
                assert!(is_parallel(&mut ctx, s));
                sequential(&mut ctx, s);
                assert!(!is_parallel(&mut ctx, s));
                let short = ctx.alloc_object(STREAM, 2);
                parallel(&mut ctx, short);
                assert!(!is_parallel(&mut ctx, short));
                let spl = ctx.alloc_object(SPL, STREAM_NUM_FIELDS);
                parallel(&mut ctx, spl);
                assert!(!is_parallel(&mut ctx, spl));
            });
        }

        /// `Stream.of(..).map(f).parallel().spliterator()` splits (HotSpot's
        /// parallel `WrappingSpliterator` splits its source) and a later
        /// stage reports the mode; the sequential twin still declines.
        #[test]
        fn a_parallel_stage_splits_and_passes_the_mode_on() {
            defaults(|| {
                let mut ctx = mock();
                let s = source_stream(&mut ctx, 4, SPL_IMMUTABLE_ARRAY);
                parallel(&mut ctx, s);
                let kept = [Value::Int(0)];
                let d = obj(make_derived_stream(&mut ctx, s, &kept, SplOp::Filter)).unwrap();
                assert!(stream_is_parallel(&ctx, d), "the derived stage is parallel");
                let spl = mapped_spliterator(&mut ctx, s);
                assert_eq!(chars_of(&mut ctx, spl), 0x4050);
                let left = split(&mut ctx, spl).expect("parallel: the source splits");
                assert_eq!(chars_of(&mut ctx, left), 0x4050);
                let seq = source_stream(&mut ctx, 4, SPL_IMMUTABLE_ARRAY);
                let spl = mapped_spliterator(&mut ctx, seq);
                assert_eq!(split(&mut ctx, spl), None, "sequential: no split");
            });
        }

        /// `CRATONVM_STREAM_PARALLEL_STAGE=0`: `parallel()` changes nothing.
        #[test]
        fn the_parallel_kill_switch_restores_the_old_answers() {
            cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_STREAM_PARALLEL_STAGE", Some("0"))],
                || {
                    let mut ctx = mock();
                    let s = source_stream(&mut ctx, 4, SPL_IMMUTABLE_ARRAY);
                    parallel(&mut ctx, s);
                    assert!(!is_parallel(&mut ctx, s));
                    assert_eq!(
                        ctx.get_field(s, STREAM_FIELD_SPL_FACTS),
                        Value::Long(pack_spl_facts(SPL_IMMUTABLE_ARRAY, None)),
                        "the slot is untouched"
                    );
                    let spl = mapped_spliterator(&mut ctx, s);
                    assert_eq!(split(&mut ctx, spl), None);
                },
            );
        }

        /// CC13-1: `IntStream.of(7)` / `"ab".chars()` record the array-style
        /// `0x4450`; a 2-field carrier has nowhere to keep it; `=0` records
        /// nothing.
        #[test]
        fn an_array_source_records_its_spliterator_characteristics() {
            defaults(|| {
                let mut ctx = mock();
                assert!(stream_source_facts_enabled());
                let s = ctx.alloc_object(INT_STREAM, STREAM_NUM_FIELDS);
                record_array_source_spliterator_facts(&mut ctx, s);
                assert_eq!(
                    synthetic_stream_spliterator_characteristics(&ctx, s),
                    Some(0x4450)
                );
                let short = ctx.alloc_object(INT_STREAM, 2);
                record_array_source_spliterator_facts(&mut ctx, short);
                assert_eq!(
                    synthetic_stream_spliterator_characteristics(&ctx, short),
                    None
                );
            });
            cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_STREAM_SOURCE_FACTS", Some("0"))],
                || {
                    let mut ctx = mock();
                    assert!(!stream_source_facts_enabled());
                    let s = ctx.alloc_object(INT_STREAM, STREAM_NUM_FIELDS);
                    record_array_source_spliterator_facts(&mut ctx, s);
                    assert_eq!(synthetic_stream_spliterator_characteristics(&ctx, s), None);
                },
            );
        }

        /// SPL9-2: the stages over a hash container's own spliterator cell,
        /// as HotSpot 25 reports them (`R16Compat2Streams`).
        #[test]
        fn stages_over_a_hash_source_match_hotspot() {
            assert_eq!(SPL_HASH_SET, 0x41, "HashSet.stream().spliterator()");
            for (upstream, op, want, row) in [
                (SPL_HASH_SET, SplOp::Map, 0x40, "HashSet.stream.map"),
                (SPL_HASH_SET, SplOp::Filter, 0x01, "HashSet.stream.filter"),
                (
                    SPL_HASH_SET,
                    SplOp::SortedNatural,
                    0x55,
                    "HashSet.stream.sorted",
                ),
                (
                    SPL_HASH_VALUES,
                    SplOp::Map,
                    0x40,
                    "HashMap.values.stream.map",
                ),
                (
                    SPL_LINKED_SET,
                    SplOp::Map,
                    0x4050,
                    "LinkedHashSet.stream.map",
                ),
            ] {
                assert_eq!(spl_wrapped_characteristics(upstream, op), want, "{row}");
            }
        }

        /// Wave 15: a real intermediate stage reports the list default
        /// without splitting; a real head keeps the splitting snapshot.
        #[test]
        fn a_real_pipeline_stage_is_wrapped_only_past_its_head() {
            let head = RealPipelineShape {
                wrapped: false,
                parallel: false,
            };
            let stage = RealPipelineShape {
                wrapped: true,
                parallel: false,
            };
            assert_eq!(real_pipeline_spl_facts(head), None);
            assert_eq!(
                real_pipeline_spl_facts(stage),
                Some((SPL_LIST_DEFAULT | SPL_PRIV_WRAPPED, None))
            );
            // A plain mock object is no `AbstractPipeline`.
            defaults(|| {
                let mut ctx = mock();
                let s = source_stream(&mut ctx, 3, SPL_IMMUTABLE_ARRAY);
                assert_eq!(real_pipeline_shape(&ctx, s), None);
            });
        }

        /// b5: a program class that declares (or inherits from a program
        /// class) `sort(Comparator)` gets `Collections.sort`'s virtual call;
        /// the JDK's own classes, a program class that declares nothing, a
        /// null list and the kill switch keep the native sort.
        #[test]
        fn collections_sort_calls_a_program_override() {
            let mut ctx = mock();
            ctx.define_superclass(SORTING_LIST, ARRAY_LIST);
            ctx.define_superclass(SORTING_SUB, SORTING_LIST);
            ctx.define_superclass(PLAIN_LIST, ARRAY_LIST);
            ctx.define_declared_method(SORTING_LIST, "sort", SORT_DESC);
            // The JDK class's own declaration is not a program override.
            ctx.define_declared_method(ARRAY_LIST, "sort", SORT_DESC);
            let sorting = ctx.alloc_object(SORTING_LIST, 3);
            let sub = ctx.alloc_object(SORTING_SUB, 3);
            let plain = ctx.alloc_object(PLAIN_LIST, 3);
            let jdk = ctx.alloc_object(ARRAY_LIST, 3);
            assert!(program_class_overrides(&ctx, sorting, "sort", SORT_DESC));
            assert!(program_class_overrides(&ctx, sub, "sort", SORT_DESC));
            assert!(!program_class_overrides(&ctx, plain, "sort", SORT_DESC));
            assert!(!program_class_overrides(&ctx, jdk, "sort", SORT_DESC));
            let null = Value::Object(None);
            defaults(|| {
                for (list, want) in [(sorting, true), (sub, true), (plain, false), (jdk, false)] {
                    let arg = Value::Object(Some(list));
                    let r = collections_sort_override(&mut ctx, Some(&arg), null);
                    assert_eq!(r.is_some(), want, "{list:?}");
                }
                assert!(collections_sort_override(&mut ctx, Some(&null), null).is_none());
                assert!(collections_sort_override(&mut ctx, None, null).is_none());
            });
            cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_COMPAT_COLLECTIONS_SORT_VIRTUAL", Some("0"))],
                || {
                    let v = Value::Object(Some(sorting));
                    assert!(collections_sort_override(&mut ctx, Some(&v), null).is_none());
                },
            );
        }
    }

    /// JIT round 17 wave 1, lane streams
    /// (`r17-compatible-stream-residuals-FIXED-20261005.md`).
    mod streams_r17w1_tests {
        use super::super::*;
        use super::lbq_blocking_tests::MockCtx;
        #[allow(unused_imports)]
        use cratonvm_native_api::{
            NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
            NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
        };

        const STREAM: ClassId = ClassId::new(0);
        const INT_STREAM: ClassId = ClassId::new(1);
        const APP_SPL: ClassId = ClassId::new(2);

        fn mock() -> MockCtx {
            let ctx = MockCtx::new(1);
            ctx.define_class(STREAM, "java/util/stream/Stream");
            ctx.define_class(INT_STREAM, "java/util/stream/IntStream");
            ctx.define_class(APP_SPL, "app/Spl");
            ctx
        }

        /// Every switch this module reads at its default.
        fn defaults<R>(f: impl FnOnce() -> R) -> R {
            cratonvm_types::flags::with_thread_overrides(
                &[
                    ("CRATONVM_SPLITERATOR_FACTS", None),
                    ("CRATONVM_STREAM_PARALLEL_STAGE", None),
                    ("CRATONVM_STREAM_PRIMITIVE_STAGE_FACTS", None),
                    ("CRATONVM_PRIMITIVE_ITERATOR_JDK_ADAPTER", None),
                    ("CRATONVM_STREAM_LAZY_HEAD_SPLITERATOR", None),
                    ("CRATONVM_COMPAT_REAL_PIPELINE_SPLITERATOR", None),
                ],
                f,
            )
        }

        /// `IntPipeline.distinct()` is `boxed().distinct().mapToInt(..)`:
        /// only ORDERED survives a range source.
        #[test]
        fn a_primitive_distinct_keeps_only_the_order() {
            assert_eq!(
                spl_wrapped_characteristics(SPL_RANGE, SplOp::PrimDistinct),
                0x10
            );
            assert_eq!(spl_wrapped_characteristics(SPL_RANGE, SplOp::Filter), 0x15);
        }

        /// A primitive op over a parallel range records the filtered facts,
        /// the source's size and the parallel bit; `=0` records nothing.
        #[test]
        fn a_primitive_op_records_its_facts_and_the_parallel_bit() {
            defaults(|| {
                let mut ctx = mock();
                let arr = ctx.new_ref_array(ClassId::new(0), 4);
                let s = ctx.alloc_object(INT_STREAM, STREAM_NUM_FIELDS);
                ctx.set_field(s, STREAM_FIELD_ELEMENTS, Value::Object(Some(arr)));
                stream_set_source_spl_facts(&mut ctx, s, SPL_RANGE, Value::Object(None));
                let _ = native_stream_parallel(&mut ctx, &[Value::Object(Some(s))]);
                let packed = prim_stage_spl_facts(&ctx, s, SplOp::Filter).expect("facts");
                assert_ne!(packed & STREAM_FACTS_PARALLEL, 0, "the parallel bit");
                let d = ctx.alloc_object(INT_STREAM, STREAM_NUM_FIELDS);
                stream_set_spl_facts(&mut ctx, d, packed);
                assert_eq!(
                    stream_spl_facts(&ctx, d),
                    Some((0x15 | SPL_PRIV_WRAPPED, Some(4)))
                );
                assert!(stream_is_parallel(&ctx, d));
            });
            cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_STREAM_PRIMITIVE_STAGE_FACTS", Some("0"))],
                || {
                    let mut ctx = mock();
                    let s = ctx.alloc_object(INT_STREAM, STREAM_NUM_FIELDS);
                    stream_set_source_spl_facts(&mut ctx, s, SPL_RANGE, Value::Object(None));
                    assert_eq!(prim_stage_spl_facts(&ctx, s, SplOp::Map), None);
                },
            );
        }

        /// The primitive iterator is the JDK's adapter (`Spliterators.iterator`),
        /// the rows that build it stay a stated `Bridge`, and `=0` restores
        /// the interface-classed snapshot.
        #[test]
        fn the_primitive_iterator_is_the_jdk_adapter() {
            defaults(|| {
                assert_eq!(
                    primitive_iterator_jdk_adapter("java/util/PrimitiveIterator$OfLong"),
                    Some((
                        "([JIII)Ljava/util/Spliterator$OfLong;",
                        "(Ljava/util/Spliterator$OfLong;)Ljava/util/PrimitiveIterator$OfLong;",
                    ))
                );
                assert_eq!(primitive_iterator_jdk_adapter("java/util/Iterator"), None);
            });
            cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_PRIMITIVE_ITERATOR_JDK_ADAPTER", Some("0"))],
                || {
                    assert_eq!(
                        primitive_iterator_jdk_adapter("java/util/PrimitiveIterator$OfInt"),
                        None
                    );
                },
            );
            // Round 17 wave 3 (lane streams3): COL8-1 landed, so the rows are a
            // stated `SyntheticStub` by default, and any producer kill switch
            // (or the row's own) restores `Bridge`.
            cratonvm_types::flags::with_thread_overrides(
                &[
                    ("CRATONVM_PRIMITIVE_STREAM_ITERATOR_STUB", None),
                    ("CRATONVM_STRICT_STRING_STREAM_BYTECODE", None),
                    ("CRATONVM_LIST_STREAM_IFACE_STUB", None),
                    ("CRATONVM_COW_SET_STREAM_JDK_BODY", None),
                    ("CRATONVM_STRICT_ELEMENTS_STREAM_JDK_PIPELINE", None),
                ],
                || {
                    assert_eq!(
                        primitive_stream_iterator_kind(),
                        cratonvm_native_api::NativeKind::SyntheticStub
                    );
                    assert_eq!(
                        list_stream_iface_kind(),
                        cratonvm_native_api::NativeKind::SyntheticStub
                    );
                },
            );
            for off in [
                "CRATONVM_PRIMITIVE_STREAM_ITERATOR_STUB",
                "CRATONVM_STRICT_STRING_STREAM_BYTECODE",
                "CRATONVM_LIST_STREAM_IFACE_STUB",
                "CRATONVM_COW_SET_STREAM_JDK_BODY",
                "CRATONVM_STRICT_ELEMENTS_STREAM_JDK_PIPELINE",
            ] {
                cratonvm_types::flags::with_thread_overrides(&[(off, Some("0"))], || {
                    assert_eq!(
                        primitive_stream_iterator_kind(),
                        cratonvm_native_api::NativeKind::Bridge,
                        "{off}=0"
                    );
                });
            }
            cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_LIST_STREAM_IFACE_STUB", Some("0"))],
                || {
                    assert_eq!(
                        list_stream_iface_kind(),
                        cratonvm_native_api::NativeKind::Bridge
                    );
                },
            );
        }

        /// `StreamSupport.stream(spl, false).spliterator()` is `spl` itself,
        /// once: the stage is then linked, and a second call refuses.
        #[test]
        fn a_lazy_head_answers_its_own_spliterator_once() {
            defaults(|| {
                let mut ctx = mock();
                let spl = ctx.alloc_object(APP_SPL, 1);
                let s = ctx.alloc_object(STREAM, STREAM_NUM_FIELDS);
                ctx.set_field(s, STREAM_FIELD_ELEMENTS, Value::Object(None));
                ctx.set_field(s, STREAM_FIELD_LAZY_SPLITERATOR, Value::Object(Some(spl)));
                let r = native_stream_spliterator(&mut ctx, &[Value::Object(Some(s))]);
                assert!(
                    matches!(r, Ok(Some(Value::Object(Some(o)))) if o == spl),
                    "{r:?}"
                );
                assert!(stream_is_linked(&ctx, s));
                assert!(native_stream_spliterator(&mut ctx, &[Value::Object(Some(s))]).is_err());
            });
        }
    }

    /// JIT round 17 wave 2, lane streams2
    /// (`r17-streamsupport-lazy-carrier-residual-shapes-FIXED-20261005.md`).
    mod streams2_r17w2_tests {
        use super::super::*;
        use super::lbq_blocking_tests::MockCtx;
        #[allow(unused_imports)]
        use cratonvm_native_api::{
            NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
            NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
        };

        const STREAM: ClassId = ClassId::new(0);
        const SPL: ClassId = ClassId::new(1);
        const APP_SPL: ClassId = ClassId::new(2);

        fn mock() -> MockCtx {
            let ctx = MockCtx::new(1);
            ctx.define_class(STREAM, "java/util/stream/Stream");
            ctx.define_class(SPL, "java/util/Spliterator");
            ctx.define_class(APP_SPL, "app/Spl");
            ctx
        }

        fn defaults<R>(f: impl FnOnce() -> R) -> R {
            cratonvm_types::flags::with_thread_overrides(
                &[
                    ("CRATONVM_SPLITERATOR_FACTS", None),
                    ("CRATONVM_STREAM_UNORDERED_STAGE", None),
                    ("CRATONVM_STREAM_SUPPORT_ZERO_FACTS", None),
                    ("CRATONVM_STREAM_LAZY_UNORDERED_STAGE", None),
                ],
                f,
            )
        }

        /// A lazy head as `StreamSupport.stream(spl, false)` builds it, with
        /// `chars` recorded.
        fn lazy_head(ctx: &mut MockCtx, chars: i32) -> (ObjectRef, ObjectRef) {
            let spl = ctx.alloc_object(APP_SPL, 1);
            let s = ctx.alloc_object(STREAM, STREAM_NUM_FIELDS);
            ctx.set_field(s, STREAM_FIELD_ELEMENTS, Value::Object(None));
            ctx.set_field(s, STREAM_FIELD_LAZY_SPLITERATOR, Value::Object(Some(spl)));
            stream_set_source_spl_facts(ctx, s, chars, Value::Object(None));
            (s, spl)
        }

        fn chars_of(ctx: &mut MockCtx, spl: ObjectRef) -> i32 {
            match native_spliterator_characteristics(ctx, &[Value::Object(Some(spl))]) {
                Ok(Some(Value::Int(c))) => c,
                other => panic!("characteristics: {other:?}"),
            }
        }

        /// Residual 1: a source reporting 0 is described, its stages derive
        /// 0 (WRAPPED), and a spliterator slot carrying only the private bits
        /// reports 0 rather than the list default.
        #[test]
        fn a_zero_characteristics_source_is_described() {
            defaults(|| {
                let mut ctx = mock();
                let (s, _) = lazy_head(&mut ctx, SPL_PRIV_DESCRIBED);
                assert_eq!(stream_spl_facts(&ctx, s), Some((SPL_PRIV_DESCRIBED, None)));
                let packed = stream_derive_spl_facts(&ctx, s, SplOp::Map).expect("described");
                let d = ctx.alloc_object(STREAM, STREAM_NUM_FIELDS);
                stream_set_spl_facts(&mut ctx, d, packed);
                assert_eq!(stream_spl_facts(&ctx, d), Some((SPL_PRIV_WRAPPED, None)));
                assert!(
                    !stream_unordered_lazy_head(&ctx, s),
                    "not ORDERED: identity"
                );
                let arr = ctx.new_ref_array(ClassId::new(0), 2);
                let described = alloc_facts_spliterator(
                    &mut ctx,
                    arr,
                    SPL_PRIV_DESCRIBED,
                    Value::Object(None),
                    None,
                )
                .expect("mock allocates");
                assert_eq!(chars_of(&mut ctx, described), 0);
                let arr = ctx.new_ref_array(ClassId::new(0), 2);
                let bare = alloc_facts_spliterator(&mut ctx, arr, 0, Value::Object(None), None)
                    .expect("mock allocates");
                assert_eq!(
                    chars_of(&mut ctx, bare),
                    SPL_LIST_DEFAULT,
                    "0 alone: the default"
                );
            });
        }

        /// Residual 4: `unordered()` on an ORDERED lazy head is a new stage
        /// over the same undrained source; the head is linked.
        #[test]
        fn unordered_on_an_ordered_lazy_head_takes_over_the_source() {
            defaults(|| {
                let mut ctx = mock();
                let (s, spl) = lazy_head(&mut ctx, SPL_LIST_DEFAULT);
                assert!(stream_unordered_lazy_head(&ctx, s));
                let u = match native_stream_unordered(&mut ctx, &[Value::Object(Some(s))]) {
                    Ok(Some(Value::Object(Some(u)))) => u,
                    other => panic!("unordered: {other:?}"),
                };
                assert_ne!(u, s);
                assert!(stream_is_linked(&ctx, s), "the head is linked");
                assert!(!stream_is_linked(&ctx, u));
                assert_eq!(stream_lazy_spliterator(&ctx, u), Some(spl), "not drained");
                assert_eq!(
                    stream_spl_facts(&ctx, u),
                    Some((0x4040 | SPL_PRIV_WRAPPED, None))
                );
                assert!(!stream_unordered_lazy_head(&ctx, u), "unordered already");
            });
            cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_STREAM_LAZY_UNORDERED_STAGE", Some("0"))],
                || {
                    let mut ctx = mock();
                    let (s, _) = lazy_head(&mut ctx, SPL_LIST_DEFAULT);
                    let r = native_stream_unordered(&mut ctx, &[Value::Object(Some(s))]);
                    assert!(
                        matches!(r, Ok(Some(Value::Object(Some(o)))) if o == s),
                        "{r:?}"
                    );
                    assert!(!stream_is_linked(&ctx, s));
                },
            );
        }
    }

    /// JIT round 17 wave 8, lane streams4
    /// (`r17-streamsupport-lazy-carrier-residual-shapes-FIXED-20261005.md` residual 5):
    /// one pipeline, one parallel mode (`CRATONVM_STREAM_SHARED_PARALLEL`).
    mod streams4_r17w8_tests {
        use super::super::*;
        use super::lbq_blocking_tests::MockCtx;
        #[allow(unused_imports)]
        use cratonvm_native_api::{
            NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
            NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
        };

        const STREAM: ClassId = ClassId::new(0);
        const APP_SPL: ClassId = ClassId::new(2);

        fn mock() -> MockCtx {
            let ctx = MockCtx::new(1);
            ctx.define_class(STREAM, "java/util/stream/Stream");
            ctx.define_class(APP_SPL, "app/Spl");
            ctx
        }

        fn with_switch<R>(value: Option<&str>, f: impl FnOnce() -> R) -> R {
            cratonvm_types::flags::with_thread_overrides(
                &[
                    ("CRATONVM_STREAM_SHARED_PARALLEL", value),
                    ("CRATONVM_STREAM_PARALLEL_STAGE", None),
                    ("CRATONVM_SPLITERATOR_FACTS", None),
                    ("CRATONVM_STREAM_UNORDERED_STAGE", None),
                    ("CRATONVM_STREAM_LAZY_UNORDERED_STAGE", None),
                ],
                f,
            )
        }

        fn source_stream(ctx: &mut MockCtx) -> ObjectRef {
            let arr = ctx.new_ref_array(ClassId::new(0), 2);
            let s = ctx.alloc_object(STREAM, STREAM_NUM_FIELDS);
            ctx.set_field(s, STREAM_FIELD_ELEMENTS, Value::Object(Some(arr)));
            stream_set_source_spl_facts(ctx, s, SPL_LIST_DEFAULT, Value::Object(None));
            s
        }

        fn stage(r: MethodCallResult) -> ObjectRef {
            match r {
                Ok(Some(Value::Object(Some(o)))) => o,
                other => panic!("expected a stage, got {other:?}"),
            }
        }

        fn is_parallel(ctx: &mut MockCtx, s: ObjectRef) -> bool {
            match native_stream_is_parallel(ctx, &[Value::Object(Some(s))]) {
                Ok(Some(Value::Int(b))) => b != 0,
                other => panic!("isParallel: {other:?}"),
            }
        }

        fn set_mode(ctx: &mut MockCtx, s: ObjectRef, on: bool) {
            let args = [Value::Object(Some(s))];
            let r = if on {
                native_stream_parallel(ctx, &args)
            } else {
                native_stream_sequential(ctx, &args)
            };
            assert_eq!(stage(r), s, "parallel() / sequential() answer this");
        }

        /// `s.map(f).parallel(); s.isParallel()` is true, and `sequential()`
        /// on the linked source switches the tail back.
        #[test]
        fn an_earlier_stage_reports_and_switches_the_pipeline_mode() {
            with_switch(None, || {
                let mut ctx = mock();
                let s = source_stream(&mut ctx);
                let kept = [Value::Int(0)];
                let d = stage(make_derived_stream(&mut ctx, s, &kept, SplOp::Map));
                assert_eq!(stream_pipeline_tail(&ctx, s), d);
                assert_eq!(stream_pipeline_tail(&ctx, d), d);
                set_mode(&mut ctx, d, true);
                assert!(is_parallel(&mut ctx, s), "the source reads the tail");
                set_mode(&mut ctx, s, false);
                assert!(!is_parallel(&mut ctx, d), "the source writes the tail");
                set_mode(&mut ctx, s, true);
                let e = stage(make_derived_stream(&mut ctx, d, &kept, SplOp::Filter));
                assert!(stream_is_parallel(&ctx, e), "a new stage copies the mode");
                set_mode(&mut ctx, e, false);
                assert!(!is_parallel(&mut ctx, s), "three stages, one mode");
            });
        }

        /// The lazy `unordered()` stage over a `StreamSupport.stream` head is
        /// recorded too.
        #[test]
        fn the_lazy_unordered_stage_is_the_heads_next_stage() {
            with_switch(None, || {
                let mut ctx = mock();
                let spl = ctx.alloc_object(APP_SPL, 1);
                let s = ctx.alloc_object(STREAM, STREAM_NUM_FIELDS);
                ctx.set_field(s, STREAM_FIELD_ELEMENTS, Value::Object(None));
                ctx.set_field(s, STREAM_FIELD_LAZY_SPLITERATOR, Value::Object(Some(spl)));
                stream_set_source_spl_facts(&mut ctx, s, SPL_LIST_DEFAULT, Value::Object(None));
                let u = stage(native_stream_unordered(&mut ctx, &[Value::Object(Some(s))]));
                assert_ne!(u, s);
                set_mode(&mut ctx, u, true);
                assert!(is_parallel(&mut ctx, s));
            });
        }

        /// `CRATONVM_STREAM_SHARED_PARALLEL=0`: no next stage is recorded and
        /// each stage keeps its own bit (the round-16 answer).
        #[test]
        fn the_kill_switch_keeps_the_per_stage_bit() {
            with_switch(Some("0"), || {
                let mut ctx = mock();
                let s = source_stream(&mut ctx);
                let kept = [Value::Int(0)];
                let d = stage(make_derived_stream(&mut ctx, s, &kept, SplOp::Map));
                assert_eq!(stream_pipeline_tail(&ctx, s), s);
                set_mode(&mut ctx, d, true);
                assert!(!is_parallel(&mut ctx, s));
                assert!(is_parallel(&mut ctx, d));
            });
        }
    }

    /// JIT round 19 wave 7, lane walk7 (THROWS5-3,
    /// `CRATONVM_STACKWALK_LAZY_FRAMES`): a lazy walk stream builds a frame
    /// only when an operation reads it, each frame once, and answers what the
    /// eager walk's stream answers.
    mod walk7_lazy_walk_tests {
        use super::super::*;
        use super::lbq_blocking_tests::MockCtx;
        #[allow(unused_imports)]
        use cratonvm_native_api::{
            NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
            NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
        };
        use std::cell::Cell;
        use std::rc::Rc;

        const STREAM: ClassId = ClassId::new(0);
        const FRAME: ClassId = ClassId::new(3);
        const FRAMES: usize = 6;

        /// A lazy walk's stream over `FRAMES` frames (frame `i` is an object
        /// whose field 0 is `i`), its guard, and the builder's call count.
        fn lazy_walk(ctx: &mut MockCtx) -> (ObjectRef, LazyWalkGuard, Rc<Cell<usize>>) {
            ctx.define_class(STREAM, "java/util/stream/Stream");
            ctx.define_class(FRAME, "app/Frame");
            let arr = ctx.new_ref_array(ClassId::new(0), FRAMES);
            let s = ctx.alloc_object(STREAM, LAZY_WALK_STREAM_NUM_FIELDS);
            ctx.set_field(s, STREAM_FIELD_ELEMENTS, Value::Object(Some(arr)));
            begin_stack_walk_stream(ctx, s);
            let pin = ctx.pin_native_root(s);
            let calls = Rc::new(Cell::new(0usize));
            let counted = calls.clone();
            let guard = push_lazy_walk_source(
                pin,
                Box::new(move |ctx: &mut dyn NativeContext, i: usize| {
                    counted.set(counted.get() + 1);
                    let f = ctx.alloc_object(FRAME, 1);
                    // Cast: a frame index below `FRAMES`.
                    ctx.set_field(f, 0, Value::Int(i as i32));
                    Ok(f)
                }),
            );
            (s, guard, calls)
        }

        fn index_of(ctx: &dyn NativeContext, v: Value) -> i32 {
            match v {
                Value::Object(Some(f)) => ctx.get_field(f, 0).as_int().unwrap_or(-1),
                other => panic!("expected a frame, got {other:?}"),
            }
        }

        fn stage(r: MethodCallResult) -> ObjectRef {
            match r {
                Ok(Some(Value::Object(Some(o)))) => o,
                other => panic!("expected a stage, got {other:?}"),
            }
        }

        #[test]
        fn an_element_read_builds_that_frame_once() {
            let mut ctx = MockCtx::new(1);
            let (s, guard, calls) = lazy_walk(&mut ctx);
            assert_eq!(lazy_walk_len(&ctx, s), Some(FRAMES));
            let v = lazy_walk_element(&mut ctx, s, 2).expect("built");
            assert_eq!(index_of(&ctx, v), 2);
            let again = lazy_walk_element(&mut ctx, s, 2).expect("kept");
            assert_eq!(again, v, "the same frame object on a second read");
            assert_eq!(calls.get(), 1);
            assert_eq!(guard.built(), 1);
        }

        #[test]
        fn a_short_circuit_builds_up_to_the_deciding_frame() {
            let mut ctx = MockCtx::new(1);
            let (s, _guard, calls) = lazy_walk(&mut ctx);
            let mut seen = Vec::new();
            let done = lazy_walk_each(&mut ctx, s, &mut |c, v| {
                seen.push(index_of(&*c, v));
                Ok(if seen.len() == 3 {
                    PullStep::Stop
                } else {
                    PullStep::Continue
                })
            });
            done.expect("a lazy walk stream").expect("no error");
            assert_eq!(seen, vec![0, 1, 2]);
            assert_eq!(calls.get(), 3);
            assert!(
                stream_is_linked(&ctx, s),
                "the terminal consumed the stream"
            );
        }

        #[test]
        fn count_builds_nothing() {
            let mut ctx = MockCtx::new(1);
            let (s, _guard, calls) = lazy_walk(&mut ctx);
            let r = native_stream_count(&mut ctx, &[Value::Object(Some(s))]);
            assert!(matches!(r, Ok(Some(Value::Long(n))) if n == FRAMES as i64));
            assert_eq!(calls.get(), 0);
        }

        #[test]
        fn a_slice_of_a_slice_builds_only_what_it_reads() {
            let mut ctx = MockCtx::new(1);
            let (s, _guard, calls) = lazy_walk(&mut ctx);
            let skipped = stage(lazy_walk_slice(&mut ctx, s, true, 1).expect("a slice"));
            assert_eq!(calls.get(), 0, "skip builds no frame");
            let limited = stage(lazy_walk_slice(&mut ctx, skipped, false, 3).expect("a slice"));
            let again = stage(lazy_walk_slice(&mut ctx, limited, true, 1).expect("a slice"));
            assert_eq!(calls.get(), 0, "slices of slices build no frame");
            let mut cur = again;
            let elems = stream_elements(&mut ctx, &mut cur).expect("elements");
            let got: Vec<i32> = elems.iter().map(|v| index_of(&ctx, *v)).collect();
            assert_eq!(got, vec![2, 3], "skip(1).limit(3).skip(1) of 0..6");
            assert_eq!(calls.get(), 2, "only the two frames read");
            let v = lazy_walk_element(&mut ctx, s, 2).expect("kept");
            assert_eq!(v, elems[0], "the slice's frame is the walk's frame");
            assert_eq!(calls.get(), 2);
        }

        #[test]
        fn a_whole_read_builds_every_frame_in_order() {
            let mut ctx = MockCtx::new(1);
            let (s, _guard, calls) = lazy_walk(&mut ctx);
            lazy_walk_element(&mut ctx, s, 4).expect("built");
            let mut cur = s;
            let elems = stream_elements(&mut ctx, &mut cur).expect("elements");
            let got: Vec<i32> = elems.iter().map(|v| index_of(&ctx, *v)).collect();
            assert_eq!(got, (0..FRAMES as i32).collect::<Vec<_>>());
            assert_eq!(calls.get(), FRAMES, "frame 4 was not built twice");
        }

        #[test]
        fn the_walk_end_stops_the_laziness_and_the_stream_refuses() {
            let mut ctx = MockCtx::new(1);
            let (s, guard, calls) = lazy_walk(&mut ctx);
            end_stack_walk_stream(&mut ctx, s);
            drop(guard);
            assert_eq!(lazy_walk_len(&ctx, s), None);
            let mut cur = s;
            assert!(
                stream_elements(&mut ctx, &mut cur).is_err(),
                "escaped: refused"
            );
            assert_eq!(calls.get(), 0);
        }

        #[test]
        fn an_eager_walk_stream_is_not_lazy() {
            let mut ctx = MockCtx::new(1);
            ctx.define_class(STREAM, "java/util/stream/Stream");
            let arr = ctx.new_ref_array(ClassId::new(0), 2);
            let s = ctx.alloc_object(STREAM, WALK_STREAM_NUM_FIELDS);
            ctx.set_field(s, STREAM_FIELD_ELEMENTS, Value::Object(Some(arr)));
            begin_stack_walk_stream(&mut ctx, s);
            assert_eq!(lazy_walk_len(&ctx, s), None);
            assert!(lazy_walk_slice(&mut ctx, s, true, 1).is_none());
        }
    }

    /// JIT round 18 wave 10, lane streams5: stage classes
    /// (`CRATONVM_STREAM_STAGE_DISPLAY_CLASS`), primitive / cross-shape stage
    /// links (`CRATONVM_STREAM_SHARED_PARALLEL_PRIMITIVE`) and the slice
    /// argument check (`CRATONVM_STREAM_SLICE_ARG_CHECK`), driven through the
    /// registration wrappers with stand-in bodies.
    mod streams5_r18w10_tests {
        use super::super::*;
        use super::lbq_blocking_tests::MockCtx;
        #[allow(unused_imports)]
        use cratonvm_native_api::{
            NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
            NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
        };

        const STREAM: ClassId = ClassId::new(0);
        const INT_STREAM: ClassId = ClassId::new(2);

        fn mock() -> MockCtx {
            let ctx = MockCtx::new(1);
            ctx.define_class(STREAM, "java/util/stream/Stream");
            ctx.define_class(INT_STREAM, "java/util/stream/IntStream");
            ctx
        }

        fn with_switches<R>(edits: &[(&str, Option<&str>)], f: impl FnOnce() -> R) -> R {
            let mut all: Vec<(&str, Option<&str>)> = vec![
                ("CRATONVM_STREAM_SHARED_PARALLEL", None),
                ("CRATONVM_STREAM_SHARED_PARALLEL_PRIMITIVE", None),
                ("CRATONVM_STREAM_STAGE_DISPLAY_CLASS", None),
                ("CRATONVM_STREAM_SLICE_ARG_CHECK", None),
                ("CRATONVM_STREAM_PARALLEL_STAGE", None),
            ];
            for (k, v) in edits {
                if let Some(slot) = all.iter_mut().find(|slot| slot.0 == *k) {
                    slot.1 = *v;
                }
            }
            cratonvm_types::flags::with_thread_overrides(&all, f)
        }

        /// A stand-in intermediate op: a fresh `IntStream` carrier.
        fn new_int_stage(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
            let s = ctx.alloc_object(INT_STREAM, STREAM_NUM_FIELDS);
            Ok(Some(Value::Object(Some(s))))
        }

        /// A stand-in cross-shape op: a fresh reference carrier.
        fn new_ref_stage(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
            let s = ctx.alloc_object(STREAM, STREAM_NUM_FIELDS);
            Ok(Some(Value::Object(Some(s))))
        }

        /// A stand-in identity op.
        fn identity(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
            Ok(args.first().copied())
        }

        fn stage(r: MethodCallResult) -> ObjectRef {
            match r {
                Ok(Some(Value::Object(Some(o)))) => o,
                other => panic!("expected a stage, got {other:?}"),
            }
        }

        fn is_parallel(ctx: &mut MockCtx, s: ObjectRef) -> bool {
            match native_stream_is_parallel(ctx, &[Value::Object(Some(s))]) {
                Ok(Some(Value::Int(b))) => b != 0,
                other => panic!("isParallel: {other:?}"),
            }
        }

        fn set_mode(ctx: &mut MockCtx, s: ObjectRef, on: bool) {
            let args = [Value::Object(Some(s))];
            let r = if on {
                native_stream_parallel(ctx, &args)
            } else {
                native_stream_sequential(ctx, &args)
            };
            assert_eq!(stage(r), s);
        }

        /// `IntStream s = range(..); s.map(f).parallel(); s.isParallel()` is
        /// true, and the stage names `IntPipeline$4`; the source keeps `Head`.
        #[test]
        fn a_primitive_stage_shares_the_mode_and_names_its_class() {
            with_switches(&[], || {
                let mut ctx = mock();
                let s = ctx.alloc_object(INT_STREAM, STREAM_NUM_FIELDS);
                let args = [Value::Object(Some(s))];
                let d = stage(stream_stage_op(
                    &mut ctx,
                    &args,
                    new_int_stage,
                    StageClass::IntMap,
                ));
                assert_eq!(stream_pipeline_tail(&ctx, s), d);
                set_mode(&mut ctx, d, true);
                assert!(is_parallel(&mut ctx, s), "the source reads the tail");
                assert_eq!(
                    synthetic_stream_stage_display_name(&ctx, d),
                    Some("java/util/stream/IntPipeline$4")
                );
                assert_eq!(synthetic_stream_stage_display_name(&ctx, s), None);
            });
        }

        /// `IntStream.range(..).parallel().boxed()`: the reference stage
        /// inherits the mode, and `sequential()` on it switches the source.
        #[test]
        fn a_cross_shape_stage_inherits_and_shares_the_mode() {
            with_switches(&[], || {
                let mut ctx = mock();
                let s = ctx.alloc_object(INT_STREAM, STREAM_NUM_FIELDS);
                set_mode(&mut ctx, s, true);
                let args = [Value::Object(Some(s))];
                let d = stage(stream_stage_op(
                    &mut ctx,
                    &args,
                    new_ref_stage,
                    StageClass::IntMapToObj,
                ));
                assert!(is_parallel(&mut ctx, d), "the new stage inherits the mode");
                set_mode(&mut ctx, d, false);
                assert!(!is_parallel(&mut ctx, s), "one pipeline, one mode");
                assert_eq!(
                    synthetic_stream_stage_display_name(&ctx, d),
                    Some("java/util/stream/IntPipeline$1")
                );
            });
        }

        /// An identity op is neither stamped nor linked.
        #[test]
        fn an_identity_answer_is_left_alone() {
            with_switches(&[], || {
                let mut ctx = mock();
                let s = ctx.alloc_object(INT_STREAM, STREAM_NUM_FIELDS);
                let args = [Value::Object(Some(s))];
                let d = stage(stream_stage_op(
                    &mut ctx,
                    &args,
                    identity,
                    StageClass::IntSorted,
                ));
                assert_eq!(d, s);
                assert_eq!(synthetic_stream_stage_display_name(&ctx, s), None);
                assert_eq!(stream_pipeline_tail(&ctx, s), s);
            });
        }

        /// Kill switches: `=0` keeps `Head` and the per-stage primitive bit.
        #[test]
        fn the_kill_switches_restore_the_old_answers() {
            with_switches(
                &[
                    ("CRATONVM_STREAM_STAGE_DISPLAY_CLASS", Some("0")),
                    ("CRATONVM_STREAM_SHARED_PARALLEL_PRIMITIVE", Some("0")),
                ],
                || {
                    let mut ctx = mock();
                    let s = ctx.alloc_object(INT_STREAM, STREAM_NUM_FIELDS);
                    let args = [Value::Object(Some(s))];
                    let d = stage(stream_stage_op(
                        &mut ctx,
                        &args,
                        new_int_stage,
                        StageClass::IntMap,
                    ));
                    assert_eq!(synthetic_stream_stage_display_name(&ctx, d), None);
                    assert_eq!(stream_pipeline_tail(&ctx, s), s);
                    set_mode(&mut ctx, d, true);
                    assert!(!is_parallel(&mut ctx, s));
                },
            );
        }

        /// `skip(0)` is `this`, a negative `limit` / `skip` is
        /// `IllegalArgumentException("-1")`; `=0` runs the body.
        #[test]
        fn the_slice_argument_check_matches_hotspot() {
            with_switches(&[], || {
                let mut ctx = mock();
                let s = ctx.alloc_object(INT_STREAM, STREAM_NUM_FIELDS);
                let sl = StageClass::IntSlice;
                let zero = [Value::Object(Some(s)), Value::Long(0)];
                let r = stream_slice_op(&mut ctx, &zero, new_int_stage, sl, true);
                assert_eq!(stage(r), s);
                let neg = [Value::Object(Some(s)), Value::Long(-1)];
                assert!(stream_slice_op(&mut ctx, &neg, new_int_stage, sl, false).is_err());
                let r = stream_slice_op(&mut ctx, &zero, new_int_stage, sl, false);
                let d = stage(r);
                assert_ne!(d, s, "limit(0) is a new stage");
                assert_eq!(
                    synthetic_stream_stage_display_name(&ctx, d),
                    Some("java/util/stream/SliceOps$2")
                );
            });
            with_switches(&[("CRATONVM_STREAM_SLICE_ARG_CHECK", Some("0"))], || {
                let mut ctx = mock();
                let s = ctx.alloc_object(INT_STREAM, STREAM_NUM_FIELDS);
                let sl = StageClass::IntSlice;
                let zero = [Value::Object(Some(s)), Value::Long(0)];
                let r = stream_slice_op(&mut ctx, &zero, new_int_stage, sl, true);
                assert_ne!(stage(r), s);
            });
        }

        /// `Stream.concat`: a null operand throws, and the result is parallel
        /// when either operand is (`CRATONVM_STREAM_CONCAT_HOTSPOT`).
        #[test]
        fn concat_checks_its_operands_and_keeps_their_mode() {
            cratonvm_types::flags::with_thread_overrides(
                &[
                    ("CRATONVM_STREAM_CONCAT_HOTSPOT", None),
                    ("CRATONVM_STREAM_PARALLEL_STAGE", None),
                ],
                || {
                    let mut ctx = mock();
                    let a = ctx.alloc_object(STREAM, STREAM_NUM_FIELDS);
                    let b = ctx.alloc_object(STREAM, STREAM_NUM_FIELDS);
                    let null_b = [Value::Object(Some(a)), Value::Object(None)];
                    assert!(stream_concat_op(&mut ctx, &null_b, new_ref_stage).is_err());
                    let both = [Value::Object(Some(a)), Value::Object(Some(b))];
                    let d = stage(stream_concat_op(&mut ctx, &both, new_ref_stage));
                    assert!(!is_parallel(&mut ctx, d));
                    set_mode(&mut ctx, b, true);
                    let e = stage(stream_concat_op(&mut ctx, &both, new_ref_stage));
                    assert!(is_parallel(&mut ctx, e), "b is parallel");
                },
            );
        }

        /// Wave 11: a linked primitive stage refuses a second intermediate or
        /// terminal op with HotSpot's message; `=0` lets it through.
        #[test]
        fn a_linked_primitive_stage_refuses_reuse() {
            with_switches(&[], || {
                let mut ctx = mock();
                let s = ctx.alloc_object(INT_STREAM, STREAM_NUM_FIELDS);
                let args = [Value::Object(Some(s))];
                let d = stage(stream_stage_op(
                    &mut ctx,
                    &args,
                    new_int_stage,
                    StageClass::IntMap,
                ));
                assert_ne!(d, s);
                stream_mark_linked(&mut ctx, s);
                assert!(
                    stream_stage_op(&mut ctx, &args, new_int_stage, StageClass::IntMap).is_err()
                );
                assert!(stream_terminal_op(&mut ctx, &args, identity).is_err());
            });
            cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_STREAM_PRIMITIVE_LINK_ONCE", Some("0"))],
                || {
                    let mut ctx = mock();
                    let s = ctx.alloc_object(INT_STREAM, STREAM_NUM_FIELDS);
                    stream_mark_linked(&mut ctx, s);
                    let args = [Value::Object(Some(s))];
                    assert!(stream_terminal_op(&mut ctx, &args, identity).is_ok());
                },
            );
        }

        /// Wave 11: a null function is `NullPointerException` before the
        /// receiver is touched.
        #[test]
        fn a_null_function_is_a_null_pointer_exception() {
            with_switches(&[], || {
                let mut ctx = mock();
                let s = ctx.alloc_object(INT_STREAM, STREAM_NUM_FIELDS);
                let args = [Value::Object(Some(s)), Value::Object(None)];
                assert!(
                    stream_stage_op(&mut ctx, &args, new_int_stage, StageClass::IntMap).is_err()
                );
                assert!(!stream_is_linked(&ctx, s));
            });
        }

        /// Wave 11: a derived stage names its pipeline's close root, and a
        /// primitive stage built without facts gets its op's facts.
        #[test]
        fn a_derived_stage_links_the_close_root_and_gets_facts() {
            with_switches(&[], || {
                let mut ctx = mock();
                let s = ctx.alloc_object(INT_STREAM, STREAM_NUM_FIELDS);
                let args = [Value::Object(Some(s))];
                let d = stage(stream_stage_op(
                    &mut ctx,
                    &args,
                    new_int_stage,
                    StageClass::IntMap,
                ));
                assert_eq!(stream_close_root(&ctx, d), s);
                assert_eq!(stream_close_root(&ctx, s), s);
                let dargs = [Value::Object(Some(d))];
                let e = stage(stream_stage_op(
                    &mut ctx,
                    &dargs,
                    new_int_stage,
                    StageClass::IntPeek,
                ));
                assert_eq!(stream_close_root(&ctx, e), s, "one hop to the root");
                assert!(synthetic_primitive_wrapped_stage_facts(&ctx, d).is_some());
                assert!(synthetic_primitive_wrapped_stage_facts(&ctx, s).is_none());
            });
        }

        /// Wave 11: `unordered()` clears SORTED with ORDERED, as HotSpot
        /// reports (`IntStream.range(0, 3).unordered()`: 0x4555 -> 0x4041).
        #[test]
        fn unordered_clears_sorted_with_ordered() {
            cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_STREAM_UNORDERED_HOTSPOT", None)],
                || {
                    let range = SPL_ORDERED | SPL_SIZED | SPL_DISTINCT | SPL_SORTED;
                    assert_eq!(
                        spl_apply_op(range, SplOp::Unordered),
                        SPL_SIZED | SPL_DISTINCT
                    );
                },
            );
        }
    }

    /// JIT round 17 wave 3, lane streams3
    /// (`r17-streamsupport-lazy-carrier-residual-shapes-FIXED-20261005.md` residual 6,
    /// `r17-jdk-only-synthetic-reference-stream-producers-FIXED-20261005.md`).
    mod streams3_r17w3_tests {
        use super::super::*;
        use super::lbq_blocking_tests::MockCtx;
        #[allow(unused_imports)]
        use cratonvm_native_api::{
            NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
            NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
        };

        fn runtime(r: RuntimeError) -> MethodCallFailed {
            MethodCallFailed::InternalError(cratonvm_types::error::VmError::Runtime(r))
        }

        /// Residual 6: a source's own runtime exception is raised, a
        /// resolution-shaped failure is still swallowed, and `=0` swallows
        /// everything.
        #[test]
        fn a_source_exception_is_raised_and_a_resolution_failure_is_not() {
            let ctx = MockCtx::new(1);
            let uoe = || {
                runtime(RuntimeError::UnsupportedOperationException {
                    message: "chars".to_string(),
                })
            };
            let ule = || {
                runtime(RuntimeError::UnsatisfiedLinkError {
                    message: "x".to_string(),
                })
            };
            cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_STREAM_SUPPORT_SOURCE_FACTS_THROW", None)],
                || {
                    assert!(source_facts_failure_to_raise(&ctx, uoe()).is_some());
                    assert!(source_facts_failure_to_raise(&ctx, ule()).is_none());
                    let internal =
                        MethodCallFailed::InternalError(cratonvm_types::error::VmError::Internal {
                            message: "no such method".to_string(),
                        });
                    assert!(source_facts_failure_to_raise(&ctx, internal).is_none());
                },
            );
            cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_STREAM_SUPPORT_SOURCE_FACTS_THROW", Some("0"))],
                || {
                    assert!(source_facts_failure_to_raise(&ctx, uoe()).is_none());
                },
            );
        }
    }

    /// gc-common w26-a: `widened_obj_key` resolves a slot by `(calling VM,
    /// current address)` only
    /// (`common-w25a-native-collections-widened-obj-key-merges-live-same-class-colliders`).
    ///
    /// Each test uses its own `MockCtx` (its own VM identity) and forgets that
    /// VM's rows in a guard, so the process-wide registry and owner index keep
    /// nothing of it and no sibling test's rows are touched. The addresses are
    /// private to this module (`0x26A_`), well clear of the other in-crate
    /// mocks, which allocate from 8 upwards, and of the `0x5EAD_` owner-index
    /// tests.
    #[cfg(target_pointer_width = "64")]
    mod w26a_widened_obj_key_collider_tests {
        use super::super::*;
        use super::lbq_blocking_tests::MockCtx;
        #[allow(unused_imports)]
        use cratonvm_native_api::{
            NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
            NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
        };

        /// One `1 << 32` stride: two addresses this far apart share the
        /// mock's identity hash.
        const STRIDE: usize = 1 << 32;

        /// Class ids private to this module (never 51_001, the class gate
        /// test's "never owns an overlay" id).
        fn tree_map_cid() -> ClassId {
            ClassId::new(26_001)
        }
        fn linked_list_cid() -> ClassId {
            ClassId::new(26_002)
        }

        /// Forget the VM's overlay rows even when an assertion fails.
        struct ForgetVm(usize);
        impl Drop for ForgetVm {
            fn drop(&mut self) {
                forget_vm_collection_overlays(self.0);
            }
        }

        fn overlay_size(ctx: &MockCtx, list: ObjectRef) -> Value {
            __test_ll_get(ctx, list, "size")
        }

        /// The page's scenario: two LIVE same-class collections of one VM
        /// share a non-zero identity hash. Before the fix the second one was
        /// rebound onto the first one's slot, so both read and wrote one
        /// overlay; keyed alternately, each rebound the slot back.
        #[test]
        fn two_live_same_class_colliders_keep_separate_contents() {
            let mut ctx = MockCtx::new(1);
            let _forget = ForgetVm(ctx.vm_identity());
            let base = 0x26A0_0100usize;
            let a = ctx.w26a_alloc_object_at(tree_map_cid(), 1, base);
            let b = ctx.w26a_alloc_object_at(tree_map_cid(), 1, base + STRIDE);
            // Premise (w21 lesson mm): the mock must really make them collide.
            let hash = ctx.identity_hash_code(a);
            assert_ne!(hash, 0, "a zero hash is not the collision under test");
            assert_eq!(
                hash,
                ctx.identity_hash_code(b),
                "premise: one identity hash"
            );
            assert_eq!(ctx.class_id_of_object(a), ctx.class_id_of_object(b));

            let ka = widened_obj_key(&ctx, a);
            let kb = widened_obj_key(&ctx, b);
            assert_ne!(ka, kb, "two live collections must not share an overlay key");
            // Keyed alternately: neither steals the other's slot back.
            for _ in 0..3 {
                assert_eq!(widened_obj_key(&ctx, a), ka);
                assert_eq!(widened_obj_key(&ctx, b), kb);
            }

            __test_ll_set(&mut ctx, a, "size", Value::Int(11));
            __test_ll_set(&mut ctx, b, "size", Value::Int(22));
            assert_eq!(
                overlay_size(&ctx, a),
                Value::Int(11),
                "a reads b's contents"
            );
            assert_eq!(
                overlay_size(&ctx, b),
                Value::Int(22),
                "b reads a's contents"
            );

            // A different-class collider next to them gets its own key and
            // leaves both contents alone. (With a LONE occupant it used to
            // re-key the slot and clear its rows: the next test.)
            let c = ctx.w26a_alloc_object_at(linked_list_cid(), 1, base + 2 * STRIDE);
            assert_eq!(ctx.identity_hash_code(c), hash);
            let kc = widened_obj_key(&ctx, c);
            assert!(kc != ka && kc != kb);
            assert_eq!(overlay_size(&ctx, a), Value::Int(11));
            assert_eq!(overlay_size(&ctx, b), Value::Int(22));
            assert_eq!(overlay_size(&ctx, c), Value::Object(None), "c starts empty");

            // The owner index names each collection at its own address.
            assert!(overlay_owner_still_at(base, ka));
            assert!(overlay_owner_still_at(base + STRIDE, kb));
            assert!(!overlay_owner_still_at(base, kb));
        }

        /// gcg g2/k (`common-w26a-proposal-register-overlay-owner-once-per-slot`):
        /// with the once-per-slot arm a MINT still registers the owner row,
        /// and a steady-state hit does not touch the owner index at all; the
        /// default arm re-registers on every hit. Positive control: the row is
        /// removed by hand between the calls, so only a registration can bring
        /// it back -- and in a debug build the once-per-slot arm's lockstep
        /// check must catch the lost row instead.
        #[test]
        fn gcg_k2_a_slot_hit_registers_its_owner_only_in_the_default_arm() {
            let ctx = MockCtx::new(1);
            let _forget = ForgetVm(ctx.vm_identity());
            let base = 0x26A0_0400usize;
            let a = ctx.w26a_alloc_object_at(tree_map_cid(), 1, base);
            let ka = widened_obj_key_with(&ctx, a, true);
            assert!(
                overlay_owner_still_at(base, ka),
                "a mint registers in both arms"
            );
            assert_eq!(
                widened_obj_key_with(&ctx, a, true),
                ka,
                "a hit returns the slot's key"
            );
            assert_eq!(widened_obj_key_with(&ctx, a, false), ka);
            remove_overlay_owner_key(base, ka);
            if cfg!(debug_assertions) {
                let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    widened_obj_key_with(&ctx, a, true)
                }))
                .is_err();
                assert!(caught, "the debug lockstep check catches a lost owner row");
            } else {
                assert_eq!(widened_obj_key_with(&ctx, a, true), ka);
                assert!(
                    !overlay_owner_still_at(base, ka),
                    "the once-per-slot arm took no owner-index lock on a hit"
                );
            }
            assert_eq!(widened_obj_key_with(&ctx, a, false), ka);
            assert!(
                overlay_owner_still_at(base, ka),
                "the default arm re-registers"
            );
        }

        /// The lone-occupant shape exactly: ONE live collection of this VM
        /// holds the hash, and a different-class object presents it. Before
        /// the fix the live collection's rows were cleared.
        #[test]
        fn a_different_class_collider_does_not_wipe_a_lone_live_collection() {
            let mut ctx = MockCtx::new(1);
            let _forget = ForgetVm(ctx.vm_identity());
            let base = 0x26A0_0200usize;
            let a = ctx.w26a_alloc_object_at(tree_map_cid(), 1, base);
            __test_ll_set(&mut ctx, a, "size", Value::Int(7));
            let ka = widened_obj_key(&ctx, a);

            let other = ctx.w26a_alloc_object_at(linked_list_cid(), 1, base + STRIDE);
            assert_eq!(ctx.identity_hash_code(other), ctx.identity_hash_code(a));
            assert_ne!(widened_obj_key(&ctx, other), ka);
            assert_eq!(
                overlay_size(&ctx, a),
                Value::Int(7),
                "a different-class collider cleared a live collection's overlay"
            );
            assert_eq!(widened_obj_key(&ctx, a), ka, "a keeps its key");
        }

        /// A moved collection keeps its key and its contents, next to a live
        /// collider: the epilogue remap re-addresses its slot, so the lookup
        /// at the new address is an exact match. (The move keeps the low 32
        /// bits, so the mock hash is preserved as a real move preserves it.)
        #[test]
        fn a_moved_collection_keeps_its_contents_next_to_a_live_collider() {
            let mut ctx = MockCtx::new(1);
            let vm = ctx.vm_identity();
            let _forget = ForgetVm(vm);
            let base = 0x26A0_0300usize;
            let a = ctx.w26a_alloc_object_at(tree_map_cid(), 1, base);
            let b = ctx.w26a_alloc_object_at(tree_map_cid(), 1, base + STRIDE);
            __test_ll_set(&mut ctx, a, "size", Value::Int(31));
            __test_ll_set(&mut ctx, b, "size", Value::Int(32));
            let ka = widened_obj_key(&ctx, a);
            let kb = widened_obj_key(&ctx, b);

            // A collection moves `a`; the stop-the-world epilogue hands this
            // VM's pointer map to the overlay remap before mutators resume.
            let moved = ctx.w26a_alloc_object_at(tree_map_cid(), 1, base + 2 * STRIDE);
            assert_eq!(ctx.identity_hash_code(moved), ctx.identity_hash_code(a));
            let mut pm = cratonvm_types::PointerMap::default();
            pm.insert(base, base + 2 * STRIDE);
            gc_update_collection_overlay_refs_for_vm(vm, &pm);

            assert_eq!(
                widened_obj_key(&ctx, moved),
                ka,
                "a moved collection keeps its key"
            );
            assert_eq!(
                overlay_size(&ctx, moved),
                Value::Int(31),
                "and its contents"
            );
            assert_eq!(widened_obj_key(&ctx, b), kb);
            assert_eq!(overlay_size(&ctx, b), Value::Int(32));
            assert!(overlay_owner_still_at(base + 2 * STRIDE, ka));
            assert!(!overlay_owner_still_at(base, ka));
        }

        /// A dead predecessor's rows never reach a newcomer that presents its
        /// identity hash -- before the prune (the newcomer holds its own
        /// generation) and after it (the prune drops the dead slot and every
        /// row keyed by it, and nothing else).
        #[test]
        fn a_dead_predecessors_rows_do_not_reach_a_newcomer() {
            let mut ctx = MockCtx::new(1);
            let vm = ctx.vm_identity();
            let _forget = ForgetVm(vm);
            let base = 0x26A0_0400usize;
            let dead = ctx.w26a_alloc_object_at(tree_map_cid(), 1, base);
            __test_ll_set(&mut ctx, dead, "size", Value::Int(41));
            let k_dead = widened_obj_key(&ctx, dead);

            // `dead` becomes unreachable; before any collection prunes it, a
            // same-class newcomer with its hash is keyed.
            let newcomer = ctx.w26a_alloc_object_at(tree_map_cid(), 1, base + STRIDE);
            let k_new = widened_obj_key(&ctx, newcomer);
            assert_ne!(k_new, k_dead);
            assert_eq!(
                overlay_size(&ctx, newcomer),
                Value::Object(None),
                "the newcomer inherited a dead collection's contents"
            );
            __test_ll_set(&mut ctx, newcomer, "size", Value::Int(42));

            // The next collection finds `dead` dead and the newcomer live.
            gc_prune_dead_collection_overlays_for_vm(vm, &|addr: usize| addr != base);
            assert!(
                !overlay_owner_still_at(base, k_dead),
                "the prune drops the dead collection's owner row"
            );
            assert!(
                ll_overlay()
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get(&k_dead)
                    .is_none(),
                "the prune drops the dead collection's rows"
            );
            assert_eq!(widened_obj_key(&ctx, newcomer), k_new);
            assert_eq!(
                overlay_size(&ctx, newcomer),
                Value::Int(42),
                "the prune of a dead collider must not touch the live newcomer"
            );
        }

        /// The collection re-entrancy guards (`DELEGATE_GUARD`,
        /// `COW_SET_GUARD`, `SNAPITR_FALLBACK`) keyed a receiver by its
        /// identity hash, so a live collider inside an outer frame's
        /// delegation read as a self-re-entry and answered the native
        /// fallback. They now match a pin handle's current address.
        /// (`DELEGATE_GUARD` is thread-local, so this test's entries are its
        /// own.)
        #[test]
        fn a_live_collider_is_not_a_reentry_of_an_outer_delegation() {
            let mut ctx = MockCtx::new(1);
            let base = 0x26A0_0600usize;
            let a = ctx.w26a_alloc_object_at(tree_map_cid(), 1, base);
            let b = ctx.w26a_alloc_object_at(tree_map_cid(), 1, base + STRIDE);
            assert_eq!(
                __test_reentry_guard_key(&ctx, a),
                __test_reentry_guard_key(&ctx, b),
                "premise: the identity-hash key the guards used could not tell them apart"
            );

            let outer = reentry_guard_enter(&mut ctx, &DELEGATE_GUARD, a, "size")
                .expect("an empty guard holds nothing");
            let inner = reentry_guard_enter(&mut ctx, &DELEGATE_GUARD, b, "size");
            assert!(
                inner.is_some(),
                "a live collider inside a's delegation was taken for a re-entry of a"
            );
            // The real self-re-entry is still caught, on the same method only.
            assert!(reentry_guard_enter(&mut ctx, &DELEGATE_GUARD, a, "size").is_none());
            let other_method = reentry_guard_enter(&mut ctx, &DELEGATE_GUARD, a, "isEmpty");
            assert!(other_method.is_some());

            reentry_guard_leave(&mut ctx, &DELEGATE_GUARD, other_method.unwrap());
            reentry_guard_leave(&mut ctx, &DELEGATE_GUARD, inner.unwrap());
            reentry_guard_leave(&mut ctx, &DELEGATE_GUARD, outer);
            assert!(
                DELEGATE_GUARD.with(|g| g.borrow().is_empty()),
                "every entry is popped"
            );
        }

        /// A collision-free object keeps one key for its lifetime, and a
        /// second VM's object at the same address gets its own.
        #[test]
        fn a_collision_free_object_keeps_one_key_and_vms_stay_apart() {
            let ctx = MockCtx::new(1);
            let other_vm = MockCtx::new(1);
            let _forget = ForgetVm(ctx.vm_identity());
            let _forget_other = ForgetVm(other_vm.vm_identity());
            let base = 0x26A0_0500usize;
            let a = ctx.w26a_alloc_object_at(tree_map_cid(), 1, base);
            let k = widened_obj_key(&ctx, a);
            for _ in 0..4 {
                assert_eq!(widened_obj_key(&ctx, a), k);
            }
            let foreign = other_vm.w26a_alloc_object_at(tree_map_cid(), 1, base);
            assert_ne!(widened_obj_key(&other_vm, foreign), k);
            assert_eq!(widened_obj_key(&ctx, a), k);
        }
    }

    // -----------------------------------------------------------------------
    // Stream range materialization — overflow-safe arithmetic + bounded eager
    // count (H11a/H11b). These cover the pure helpers; the callers compute the
    // count in a wider type before delegating here.

    #[test]
    fn range_int_elements_basic() {
        let elems = range_int_elements(0, 5).unwrap();
        assert_eq!(elems.len(), 5);
        assert_eq!(elems[0], Value::Int(0));
        assert_eq!(elems[4], Value::Int(4));
    }

    #[test]
    fn range_int_elements_empty_or_negative_count() {
        assert!(range_int_elements(10, 0).unwrap().is_empty());
        assert!(range_int_elements(10, -3).unwrap().is_empty());
    }

    #[test]
    fn range_int_elements_no_wrap_near_i32_max() {
        // start near i32::MAX: `start + i` must not wrap (would in pure i32).
        let start = (i32::MAX - 2) as i64;
        let elems = range_int_elements(start, 3).unwrap();
        assert_eq!(elems[0], Value::Int(i32::MAX - 2));
        assert_eq!(elems[2], Value::Int(i32::MAX));
    }

    #[test]
    fn range_int_elements_rejects_over_cap() {
        // Mimics IntStream.range(0, i32::MAX): count widened to i64, far past cap.
        let count = i32::MAX as i64; // ~2.1 billion
        assert!(range_int_elements(0, count).is_err());
        // Exactly one over the cap also throws.
        assert!(range_int_elements(0, STREAM_RANGE_MAX_ELEMENTS as i64 + 1).is_err());
        // Exactly at the cap is allowed (no error from the guard itself).
        assert!(range_int_elements(0, STREAM_RANGE_MAX_ELEMENTS as i64).is_ok());
    }

    #[test]
    fn range_long_elements_basic_and_no_wrap() {
        let elems = range_long_elements(0, 4).unwrap();
        assert_eq!(elems.len(), 4);
        assert_eq!(elems[3], Value::Long(3));

        let start = i64::MAX - 2;
        let elems = range_long_elements(start, 3).unwrap();
        assert_eq!(elems[0], Value::Long(i64::MAX - 2));
        assert_eq!(elems[2], Value::Long(i64::MAX));
    }

    #[test]
    fn range_long_elements_rejects_over_cap() {
        // LongStream.range(0, i64::MAX): count widened to i128, vastly past cap.
        let count = i64::MAX as i128;
        assert!(range_long_elements(0, count).is_err());
        assert!(range_long_elements(0, STREAM_RANGE_MAX_ELEMENTS + 1).is_err());
        assert!(range_long_elements(0, STREAM_RANGE_MAX_ELEMENTS).is_ok());
        assert!(range_long_elements(10, -5).unwrap().is_empty());
    }

    /// Every `Collectors.*` factory must mint a DISTINCT tag that
    /// `is_known_collector_tag` accepts, because `native_stream_collect`'s tag
    /// dispatch is the only thing that ever reads a collector.
    ///
    /// This is the invariant that broke: `native-builtins`' phase-56 factories
    /// minted a private second namespace (maxBy=9, minBy=10, filtering=12,
    /// summarizing*=13/14/15) that aliased the tags below, so
    /// `stream.collect(Collectors.minBy(cmp))` was decoded as
    /// `groupingBy(classifier, supplier, downstream)` and answered a Map.
    /// The "tags are exactly 1..=N" check makes a colliding or unregistered
    /// new tag fail here rather than silently in a stream pipeline.
    #[test]
    fn collector_factories_mint_unique_decodable_tags() {
        use lbq_blocking_tests::MockCtx;
        let mut ctx = MockCtx::new(1);
        let null = Value::Object(None);
        let obj = |r: MethodCallResult| -> ObjectRef {
            match r {
                Ok(Some(Value::Object(Some(o)))) => o,
                other => panic!("collector factory returned {other:?}"),
            }
        };
        // One representative factory per tag. Aliases (toUnmodifiableList →
        // toList, joining(d,p,s) → JOINING_DELIM) are deliberately omitted.
        let mints: Vec<(&str, ObjectRef)> = vec![
            ("toList", obj(native_collectors_to_list(&mut ctx, &[]))),
            ("toSet", obj(native_collectors_to_set(&mut ctx, &[]))),
            ("joining", obj(native_collectors_joining(&mut ctx, &[]))),
            (
                "joining(delim)",
                obj(native_collectors_joining_delim(&mut ctx, &[null])),
            ),
            ("toMap", obj(native_collectors_to_map(&mut ctx, &[]))),
            ("counting", obj(native_collectors_counting(&mut ctx, &[]))),
            (
                "groupingBy",
                obj(native_collectors_grouping_by(&mut ctx, &[null])),
            ),
            (
                "partitioningBy",
                obj(native_collectors_partitioning_by(&mut ctx, &[null])),
            ),
            (
                "groupingBy(downstream)",
                obj(native_collectors_grouping_by_downstream(&mut ctx, &[null])),
            ),
            (
                "groupingBy(supplier)",
                obj(native_collectors_grouping_by_supplier(&mut ctx, &[null])),
            ),
            (
                "partitioningBy(downstream)",
                obj(native_collectors_partitioning_by_downstream(
                    &mut ctx,
                    &[null],
                )),
            ),
            (
                "toMap(merge)",
                obj(native_collectors_to_map_merge(&mut ctx, &[null])),
            ),
            (
                "collectingAndThen",
                obj(native_collectors_collecting_and_then(&mut ctx, &[null])),
            ),
            (
                "toCollection",
                obj(native_collectors_to_collection(&mut ctx, &[null])),
            ),
            ("mapping", obj(native_collectors_mapping(&mut ctx, &[null]))),
            (
                "toMap(supplier)",
                obj(native_collectors_to_map_supplier(&mut ctx, &[null])),
            ),
            ("minBy", obj(native_collectors_min_by(&mut ctx, &[null]))),
            ("maxBy", obj(native_collectors_max_by(&mut ctx, &[null]))),
            (
                "filtering",
                obj(native_collectors_filtering(&mut ctx, &[null])),
            ),
            (
                "summarizingInt",
                obj(native_collectors_summarizing_int(&mut ctx, &[null])),
            ),
            (
                "summarizingLong",
                obj(native_collectors_summarizing_long(&mut ctx, &[null])),
            ),
            (
                "summarizingDouble",
                obj(native_collectors_summarizing_double(&mut ctx, &[null])),
            ),
            (
                "averagingInt",
                obj(native_collectors_averaging_int(&mut ctx, &[null])),
            ),
            (
                "averagingLong",
                obj(native_collectors_averaging_long(&mut ctx, &[null])),
            ),
            (
                "averagingDouble",
                obj(native_collectors_averaging_double(&mut ctx, &[null])),
            ),
            (
                "summingInt",
                obj(native_collectors_summing_int(&mut ctx, &[null])),
            ),
            (
                "summingLong",
                obj(native_collectors_summing_long(&mut ctx, &[null])),
            ),
            (
                "summingDouble",
                obj(native_collectors_summing_double(&mut ctx, &[null])),
            ),
            ("teeing", obj(native_collectors_teeing(&mut ctx, &[null]))),
        ];

        let mut seen: Vec<(&str, i32)> = Vec::new();
        for (name, c) in mints {
            assert!(
                ctx.object_num_fields(c) >= COLLECTOR_NUM_FIELDS,
                "{name}: collector layout is undersized ({} fields)",
                ctx.object_num_fields(c)
            );
            let tag = match ctx.get_field(c, COLLECTOR_FIELD_TAG) {
                Value::Int(t) => t,
                other => panic!("{name}: non-int tag {other:?}"),
            };
            assert!(
                is_known_collector_tag(tag),
                "{name}: tag {tag} is not decodable by native_stream_collect"
            );
            if let Some((prev, _)) = seen.iter().find(|(_, t)| *t == tag) {
                panic!("{name} and {prev} both mint tag {tag}");
            }
            seen.push((name, tag));
        }

        let mut tags: Vec<i32> = seen.iter().map(|(_, t)| *t).collect();
        tags.sort_unstable();
        let expected: Vec<i32> = (1..=seen.len() as i32).collect();
        assert_eq!(
            tags, expected,
            "collector tags must be a dense 1..=N range with one factory each"
        );
    }

    /// `collect(minBy/maxBy(cmp))` must answer an `Optional`, which is what
    /// regressed: those collectors were tagged 10 and 9 — GROUPING_BY_SUPPLIER
    /// and GROUPING_BY_DOWNSTREAM here — so `collect` returned a Map.
    #[test]
    fn collect_min_by_max_by_yield_optional() {
        use lbq_blocking_tests::MockCtx;
        let mut ctx = MockCtx::new(1);
        // Every mock object reports ClassId(0); naming it makes `stream_elements`
        // take the synthetic-stream path. The Collector/Optional allocations are
        // recognised by layout (field count), not by name.
        ctx.define_class(ClassId::new(0), "java/util/stream/Stream");

        let elements = [Value::Int(3), Value::Int(1), Value::Int(2)];
        let arr = ctx.new_ref_array(ClassId::new(0), elements.len());
        for (i, v) in elements.iter().enumerate() {
            ctx.set_array_element(arr, i, *v);
        }
        // W7-65: a FRESH stream stage per iteration. This loop used to collect
        // twice from one stream object, which `AbstractPipeline` has always
        // rejected (`IllegalStateException: stream has already been operated
        // upon or closed`) and which CratonVM now rejects too. The assertion
        // being made — that `minBy`/`maxBy` answer an `Optional` — is unchanged
        // and is still made twice; only the illegal sharing is gone.
        let new_stream = |ctx: &mut MockCtx| {
            let s = ctx.alloc_object(ClassId::new(0), STREAM_NUM_FIELDS);
            ctx.set_field(s, STREAM_FIELD_ELEMENTS, Value::Object(Some(arr)));
            s
        };

        let natural = make_comparator(&mut ctx, CMP_TAG_NATURAL_ORDER)
            .expect("Compatible mode never refuses a comparator stand-in");
        for (tag_maker, expected) in [
            (
                // Both makers became fallible when the collector stand-in
                // learned to be refused under `--jdk-only`; the cast here still
                // named the old infallible shape, so this file did not compile
                // under `--all-targets` at all.
                make_min_by_collector
                    as fn(&mut dyn NativeContext, Value) -> Result<ObjectRef, MethodCallFailed>,
                Value::Int(1),
            ),
            (make_max_by_collector, Value::Int(3)),
        ] {
            let collector = tag_maker(&mut ctx, Value::Object(Some(natural)))
                .expect("Compatible mode never refuses a collector stand-in");
            let stream = new_stream(&mut ctx);
            let result = native_stream_collect(
                &mut ctx,
                &[Value::Object(Some(stream)), Value::Object(Some(collector))],
            );
            let opt = match result {
                Ok(Some(Value::Object(Some(o)))) => o,
                other => panic!("collect returned {other:?}"),
            };
            assert_eq!(ctx.get_field(opt, OPT_FIELD_VALUE), expected);
        }
    }

    /// W7-65 — `AbstractPipeline.linkedOrConsumed`, both directions.
    ///
    /// TWO-SIDED ON PURPOSE. The dangerous failure for this change is not the
    /// missing throw (a residual divergence, visible in the shadow
    /// differential); it is a flag set once too often, which turns a working
    /// stream into an `IllegalStateException` on the most pervasive path in the
    /// Spring Boot and Tomcat arms and shows up in neither the differential nor
    /// the 70-vector corpus. So this test asserts BOTH halves against the same
    /// funnel, and neither half is true independently of the code under test:
    ///
    ///   * delete the check -> the first half fails;
    ///   * mark at allocation, or mark twice per operation, or mark a stream
    ///     other than the receiver -> the second half fails.
    ///
    /// The third over-set shape — marking the RESULT of an intermediate op as
    /// well as its receiver — needs a real intermediate op, whose lambdas this
    /// mock cannot dispatch. `probes/StreamReuseThrowsProbe` section B covers
    /// it on a real VM, against the HotSpot values in its `.expected.txt`.
    #[test]
    fn a_consumed_stream_refuses_a_second_operation() {
        use lbq_blocking_tests::MockCtx;
        let mut ctx = MockCtx::new(1);
        ctx.define_class(ClassId::new(0), "java/util/stream/Stream");

        let elements = [Value::Int(3), Value::Int(1), Value::Int(2)];
        let new_stream = |ctx: &mut MockCtx| {
            let arr = ctx.new_ref_array(ClassId::new(0), elements.len());
            for (i, v) in elements.iter().enumerate() {
                ctx.set_array_element(arr, i, *v);
            }
            let s = ctx.alloc_object(ClassId::new(0), STREAM_NUM_FIELDS);
            ctx.set_field(s, STREAM_FIELD_ELEMENTS, Value::Object(Some(arr)));
            s
        };

        // -- the under-set half: one stream, two terminals -------------------
        let s = new_stream(&mut ctx);
        match native_stream_count(&mut ctx, &[Value::Object(Some(s))]) {
            Ok(Some(Value::Long(3))) => {}
            other => panic!("the FIRST terminal on a fresh stream must succeed, got {other:?}"),
        }
        match native_stream_count(&mut ctx, &[Value::Object(Some(s))]) {
            Err(MethodCallFailed::InternalError(cratonvm_types::error::VmError::Runtime(
                cratonvm_types::error::RuntimeError::IllegalStateException { ref message },
            ))) => assert_eq!(
                message.as_str(),
                STREAM_LINKED_MSG,
                "the class is only half the contract; HotSpot's wording is the other half"
            ),
            other => panic!("second terminal must raise IllegalStateException, got {other:?}"),
        }

        // -- the over-set half: independent streams stay independent ---------
        // Each of these is a FRESH stage over equal elements. A flag set at
        // allocation, or set on anything but the receiver, fails here.
        for attempt in 0..3 {
            let fresh = new_stream(&mut ctx);
            match native_stream_count(&mut ctx, &[Value::Object(Some(fresh))]) {
                Ok(Some(Value::Long(3))) => {}
                other => panic!(
                    "attempt {attempt}: a fresh stream must never inherit another \
                     stream's linked flag, got {other:?}"
                ),
            }
        }
    }

    /// A stream with no slot for the flag (the 1-field ad-hoc layout several
    /// native modules still mint, and the historical 2-field layout) must be
    /// left alone entirely. Under-setting there is the deliberate direction of
    /// the error — see `stream_is_linked`.
    #[test]
    fn a_short_layout_stream_is_never_reported_as_linked() {
        use lbq_blocking_tests::MockCtx;
        let mut ctx = MockCtx::new(1);
        ctx.define_class(ClassId::new(0), "java/util/stream/Stream");

        let arr = ctx.new_ref_array(ClassId::new(0), 2);
        ctx.set_array_element(arr, 0, Value::Int(7));
        ctx.set_array_element(arr, 1, Value::Int(8));
        let s = ctx.alloc_object(ClassId::new(0), 1);
        ctx.set_field(s, STREAM_FIELD_ELEMENTS, Value::Object(Some(arr)));

        for attempt in 0..3 {
            match native_stream_count(&mut ctx, &[Value::Object(Some(s))]) {
                Ok(Some(Value::Long(2))) => {}
                other => panic!(
                    "attempt {attempt}: a stream with no linked slot must never be \
                     refused, got {other:?}"
                ),
            }
        }
    }

    // -----------------------------------------------------------------------
    // RULE F — the null functional argument.
    //
    // G8 lane, 2026-08-16. The oracle rows these pin are in
    // `docs/known-issues/jdk-only/G8-1-...md` §2 and were MEASURED on HotSpot
    // 25.0.3+9-LTS. Nothing here has been observed on a CratonVM binary.
    // -----------------------------------------------------------------------

    /// True when `r` is the message-less NPE every one of these methods raises.
    /// A message would be as wrong as no throw at all: `getMessage()` is `null`
    /// on the oracle, printed distinctly from `""` in the probe on purpose.
    fn is_bare_npe(r: &MethodCallResult) -> bool {
        matches!(
            r,
            Err(MethodCallFailed::InternalError(
                cratonvm_types::error::VmError::Runtime(
                    cratonvm_types::error::RuntimeError::NullPointerException { message: None },
                )
            ))
        )
    }

    /// The helper's own truth table, with no VM in the way.
    ///
    /// The middle row is the one that matters and the one a "just check for
    /// null" rewrite loses: a MISSING argument is a malformed native call, not
    /// a program-level NPE, and must keep the body's existing no-op.
    #[test]
    fn reject_null_functional_separates_a_missing_arg_from_an_explicit_null() {
        assert!(
            reject_null_functional(None).is_ok(),
            "a MISSING argument is a dispatch defect, not a Java-visible NPE"
        );
        assert!(
            reject_null_functional(Some(&Value::Int(0))).is_ok(),
            "a non-reference argument is not this helper's business"
        );
        // Never dereferenced — the helper only inspects the discriminant. Same
        // fabricated-address idiom the `dense_int_entries_tests` module uses.
        let some_fn = Some(unsafe { ObjectRef::from_raw(0x1000 as *mut u8) });
        assert!(
            reject_null_functional(Some(&Value::Object(some_fn))).is_ok(),
            "a real function object must pass"
        );
        let refused = reject_null_functional(Some(&Value::Object(None)));
        assert!(
            matches!(
                refused,
                Err(MethodCallFailed::InternalError(
                    cratonvm_types::error::VmError::Runtime(
                        cratonvm_types::error::RuntimeError::NullPointerException { message: None },
                    )
                ))
            ),
            "an explicitly passed null must be the MESSAGE-LESS NPE, got {refused:?}"
        );
    }

    /// Every body on the axis, with an explicitly null function, must throw.
    ///
    /// This is the fabricated-success direction: before this lane each of these
    /// returned `Ok` — the empty answer for its return type — so the caller was
    /// told the operation had succeeded and nothing had happened. The argument
    /// INDEX is carried per row rather than assumed, because it is not uniform:
    /// `native_chm_for_each_parallel` takes `(long, BiConsumer)` so its
    /// function is argument 2, and `native_tm_merge`'s is argument 3.
    #[test]
    fn every_null_function_body_refuses_an_explicit_null() {
        use lbq_blocking_tests::MockCtx;
        let mut ctx = MockCtx::new(1);
        ctx.define_class(ClassId::new(0), "java/util/HashMap");
        let this = ctx.alloc_object(ClassId::new(0), 8);
        let recv = Value::Object(Some(this));
        let nul = Value::Object(None);
        let key = Value::Int(1);

        type Body = fn(&mut dyn NativeContext, &[Value]) -> MethodCallResult;
        let cases: Vec<(&str, Body, Vec<Value>)> = vec![
            ("native_map_for_each", native_map_for_each, vec![recv, nul]),
            (
                "native_map_replace_all",
                native_map_replace_all,
                vec![recv, nul],
            ),
            (
                "native_map_compute_if_absent",
                native_map_compute_if_absent,
                vec![recv, key, nul],
            ),
            (
                "native_map_compute",
                native_map_compute,
                vec![recv, key, nul],
            ),
            (
                "native_map_compute_if_present",
                native_map_compute_if_present,
                vec![recv, key, nul],
            ),
            (
                "native_lhm_compute_if_absent",
                native_lhm_compute_if_absent,
                vec![recv, key, nul],
            ),
            ("native_lhm_for_each", native_lhm_for_each, vec![recv, nul]),
            ("native_tm_for_each", native_tm_for_each, vec![recv, nul]),
            (
                "native_tm_compute_if_absent",
                native_tm_compute_if_absent,
                vec![recv, key, nul],
            ),
            (
                "native_tm_merge",
                native_tm_merge,
                vec![recv, key, key, nul],
            ),
            ("native_al_for_each", native_al_for_each, vec![recv, nul]),
            ("native_al_remove_if", native_al_remove_if, vec![recv, nul]),
            (
                "native_al_replace_all",
                native_al_replace_all,
                vec![recv, nul],
            ),
            ("native_ll_remove_if", native_ll_remove_if, vec![recv, nul]),
            ("native_hs_for_each", native_hs_for_each, vec![recv, nul]),
            ("native_ts_for_each", native_ts_for_each, vec![recv, nul]),
            ("native_ad_for_each", native_ad_for_each, vec![recv, nul]),
            ("native_ksv_for_each", native_ksv_for_each, vec![recv, nul]),
            (
                "native_ksv_remove_if",
                native_ksv_remove_if,
                vec![recv, nul],
            ),
            ("native_chm_for_each", native_chm_for_each, vec![recv, nul]),
            (
                "native_chm_replace_all",
                native_chm_replace_all,
                vec![recv, nul],
            ),
            (
                "native_chm_for_each_parallel",
                native_chm_for_each_parallel,
                vec![recv, Value::Long(1), nul],
            ),
        ];

        assert_eq!(
            cases.len(),
            22,
            "the sweep is the point: a body added to the axis without a row here \
             is a body that can silently go back to answering success"
        );

        for (name, body, args) in cases {
            let r = body(&mut ctx, &args);
            assert!(
                is_bare_npe(&r),
                "{name}: an explicitly null function must raise a MESSAGE-LESS \
                 NullPointerException (MEASURED on all six map receivers and \
                 every collection receiver, empty or not), got {r:?}"
            );
        }
    }

    /// The over-set direction, and the reason the helper takes `Option<&Value>`.
    ///
    /// A native invoked with the functional argument MISSING is a dispatch
    /// defect inside this VM. Reporting it as a Java `NullPointerException`
    /// would put a VM bug into an application's catch block wearing the costume
    /// of a program error. Each of these calls is short by one argument and
    /// must keep the body's pre-existing no-op.
    #[test]
    fn a_missing_functional_argument_is_not_reported_as_a_java_npe() {
        use lbq_blocking_tests::MockCtx;
        let mut ctx = MockCtx::new(1);
        ctx.define_class(ClassId::new(0), "java/util/HashMap");
        let this = ctx.alloc_object(ClassId::new(0), 8);
        let recv = Value::Object(Some(this));

        type Body = fn(&mut dyn NativeContext, &[Value]) -> MethodCallResult;
        let cases: Vec<(&str, Body, Vec<Value>)> = vec![
            ("native_map_for_each", native_map_for_each, vec![recv]),
            ("native_map_replace_all", native_map_replace_all, vec![recv]),
            ("native_al_remove_if", native_al_remove_if, vec![recv]),
            ("native_al_replace_all", native_al_replace_all, vec![recv]),
            ("native_ll_remove_if", native_ll_remove_if, vec![recv]),
            ("native_ksv_remove_if", native_ksv_remove_if, vec![recv]),
        ];

        for (name, body, args) in cases {
            let r = body(&mut ctx, &args);
            assert!(
                !is_bare_npe(&r),
                "{name}: a MISSING argument is a malformed native call and must \
                 not be reported to Java as an NPE, got {r:?}"
            );
        }
    }

    /// The trap this axis sets, pinned so a later sweep cannot walk into it.
    ///
    /// `sort` takes a `Comparator`, and a null `Comparator` is NOT a null
    /// functional argument — it MEANS natural ordering. MEASURED returning
    /// `[a, b]` on `ArrayList`, `LinkedList`, `Vector`, `Stack`,
    /// `CopyOnWriteArrayList`, `Arrays.asList`, `subList`,
    /// `Collections.sort(l, null)` and `Arrays.sort(a, null)`. A blanket
    /// "refuse every null function argument" rule — exactly the generalisation
    /// HANDOFF-20260814 §5 warns against — would turn nine working paths into
    /// throws.
    #[test]
    fn a_null_comparator_means_natural_ordering_and_is_never_refused() {
        use lbq_blocking_tests::MockCtx;
        let mut ctx = MockCtx::new(1);
        ctx.define_class(ClassId::new(0), "java/util/ArrayList");
        let this = ctx.alloc_object(ClassId::new(0), 8);
        let args = [Value::Object(Some(this)), Value::Object(None)];

        let r = native_al_sort_comparator(&mut ctx, &args);
        assert!(
            !is_bare_npe(&r),
            "List.sort(null) sorts by natural ordering on the oracle; refusing \
             it would be a fabricated failure, got {r:?}"
        );

        let r = native_collections_sort_comparator(&mut ctx, &args);
        assert!(
            !is_bare_npe(&r),
            "Collections.sort(l, null) sorts by natural ordering on the oracle, \
             got {r:?}"
        );
    }

    // --- r9w11 chm11: ConcurrentHashMap primitive-wrapper-key fast path ------
    //
    // A hand-built segmented CHM (one segment, four buckets, JDK-layout nodes
    // `[hash:I, key, value, next]`), so the fixture pins exactly the layout the
    // natives read and does not depend on `native_chm_put`'s node-class choice.

    struct R9w11Chm {
        ctx: lbq_blocking_tests::MockCtx,
        chm: ObjectRef,
        seg: ObjectRef,
        buckets: ObjectRef,
        int_cid: ClassId,
        long_cid: ClassId,
        node_cid: ClassId,
    }

    const R9W11_CAP: usize = 4;

    fn r9w11_spread(h: i32) -> i32 {
        h ^ ((h as u32) >> 16) as i32
    }

    fn r9w11_box(ctx: &lbq_blocking_tests::MockCtx, cid: ClassId, v: Value) -> ObjectRef {
        let obj = ctx.alloc_object_of(cid, 1);
        ctx.set_field(obj, 0, v);
        obj
    }

    /// Prepend a JDK-layout node for `(key, value)` to its bucket.
    fn r9w11_insert(f: &R9w11Chm, key: ObjectRef, raw_hash: i32, value: Value) {
        let hash = r9w11_spread(raw_hash);
        let idx = map_bucket_index(hash, R9W11_CAP as i32);
        let head = f.ctx.get_array_element(f.buckets, idx);
        let node = f.ctx.alloc_object_of(f.node_cid, 4);
        f.ctx.set_field(node, NODE_FIELD_HASH, Value::Int(hash));
        f.ctx
            .set_field(node, NODE_FIELD_KEY, Value::Object(Some(key)));
        f.ctx.set_field(node, NODE_FIELD_VALUE, value);
        f.ctx.set_field(node, NODE_FIELD_NEXT, head);
        f.ctx
            .set_array_element(f.buckets, idx, Value::Object(Some(node)));
    }

    /// Integer keys `0..32` -> `Integer(k * 10)`, plus `Long(5)` -> `Long(-5)`
    /// (same hash as `Integer(5)`, same bucket, different class) and
    /// `Integer(100)` parked on a `computeIfAbsent` reservation (value = the
    /// segment itself).
    fn r9w11_fixture(thread_id: u64) -> (R9w11Chm, Vec<(ObjectRef, i32)>) {
        r9w11_fixture_with_door(thread_id, false)
    }

    /// [`r9w11_fixture`] with the VM-style fast-unbox door selectable.
    ///
    /// r9w12: the CHM wrapper path has TWO arms now -- the fused
    /// `class_id_and_unboxed_primitive` door a real `NativeContextImpl`
    /// overrides, and the `unbox_wrapper` name-ladder fallback for a context
    /// that has none. Every test written before this took the fallback, so the
    /// fused arm had no coverage at all; see
    /// `r9w12_chm_wrapper_get_answers_the_same_through_the_fused_door`.
    fn r9w11_fixture_with_door(
        thread_id: u64,
        fast_unbox_door: bool,
    ) -> (R9w11Chm, Vec<(ObjectRef, i32)>) {
        let mut ctx = lbq_blocking_tests::MockCtx::new(thread_id);
        if fast_unbox_door {
            ctx = ctx.with_fast_unbox_door();
        }
        let int_cid = ClassId::new(9_101);
        let long_cid = ClassId::new(9_102);
        let chm_cid = ClassId::new(9_103);
        let node_cid = ClassId::new(9_104);
        ctx.define_class(int_cid, "java/lang/Integer");
        ctx.define_class(long_cid, "java/lang/Long");
        ctx.define_class(chm_cid, "java/util/concurrent/ConcurrentHashMap");
        ctx.define_class(node_cid, "java/util/HashMap$Node");
        let buckets = ctx.new_ref_array(ClassId::new(0), R9W11_CAP);
        let seg = ctx.alloc_object_of(ClassId::new(0), 3);
        ctx.set_field(seg, MAP_FIELD_BUCKETS, Value::Object(Some(buckets)));
        let segments = ctx.new_ref_array(ClassId::new(0), 1);
        ctx.set_array_element(segments, 0, Value::Object(Some(seg)));
        let chm = ctx.alloc_object_of(chm_cid, 2);
        ctx.set_field(chm, CHM_FIELD_SEGMENTS, Value::Object(Some(segments)));
        let f = R9w11Chm {
            ctx,
            chm,
            seg,
            buckets,
            int_cid,
            long_cid,
            node_cid,
        };
        let mut keys = Vec::new();
        for k in 0..32 {
            let key = r9w11_box(&f.ctx, int_cid, Value::Int(k));
            let value = r9w11_box(&f.ctx, int_cid, Value::Int(k * 10));
            r9w11_insert(&f, key, k, Value::Object(Some(value)));
            keys.push((key, k));
        }
        let long_key = r9w11_box(&f.ctx, long_cid, Value::Long(5));
        let long_value = r9w11_box(&f.ctx, long_cid, Value::Long(-5));
        r9w11_insert(&f, long_key, 5, Value::Object(Some(long_value)));
        let reserved = r9w11_box(&f.ctx, int_cid, Value::Int(100));
        r9w11_insert(&f, reserved, 100, Value::Object(Some(seg)));
        (f, keys)
    }

    fn r9w11_get(ctx: &mut lbq_blocking_tests::MockCtx, chm: ObjectRef, key: ObjectRef) -> Value {
        // Mock heaps hand out the same addresses, so a concurrently running
        // test's segment can share this one's resize stripe; a declined probe
        // (busy stripe) is retried rather than read as a failure.
        for _ in 0..100_000 {
            if let Some(v) = jit_chm_get(ctx, chm, Value::Object(Some(key))) {
                return v;
            }
            std::thread::yield_now();
        }
        panic!("a wrapper key on a segmented CHM must be served by the probe");
    }

    fn r9w11_assert_answers(f: &mut R9w11Chm, keys: &[(ObjectRef, i32)]) {
        let chm = f.chm;
        for &(key, k) in keys {
            // A fresh, NON-identical key object: equality must be by value.
            let probe = r9w11_box(&f.ctx, f.int_cid, Value::Int(k));
            for lookup in [key, probe] {
                let Value::Object(Some(v)) = r9w11_get(&mut f.ctx, chm, lookup) else {
                    panic!("Integer({k}) must be found");
                };
                assert_eq!(f.ctx.get_field(v, 0), Value::Int(k * 10));
                // The registered native answers the same through its own door.
                let native = native_chm_get(
                    &mut f.ctx,
                    &[Value::Object(Some(chm)), Value::Object(Some(lookup))],
                )
                .expect("no throw");
                assert_eq!(native, Some(Value::Object(Some(v))));
            }
        }
        // Absent key.
        let missing = r9w11_box(&f.ctx, f.int_cid, Value::Int(999));
        assert_eq!(r9w11_get(&mut f.ctx, chm, missing), Value::Object(None));
        // Same hash, same bucket, different wrapper class: never equal.
        let long5 = r9w11_box(&f.ctx, f.long_cid, Value::Long(5));
        let Value::Object(Some(lv)) = r9w11_get(&mut f.ctx, chm, long5) else {
            panic!("Long(5) must be found");
        };
        assert_eq!(f.ctx.get_field(lv, 0), Value::Long(-5));
        let int5 = r9w11_box(&f.ctx, f.int_cid, Value::Int(5));
        let Value::Object(Some(iv)) = r9w11_get(&mut f.ctx, chm, int5) else {
            panic!("Integer(5) must be found");
        };
        assert_eq!(f.ctx.get_field(iv, 0), Value::Int(50));
        let long6 = r9w11_box(&f.ctx, f.long_cid, Value::Long(6));
        assert_eq!(r9w11_get(&mut f.ctx, chm, long6), Value::Object(None));
        // A `computeIfAbsent` reservation reads as absent.
        let reserved = r9w11_box(&f.ctx, f.int_cid, Value::Int(100));
        assert_eq!(r9w11_get(&mut f.ctx, chm, reserved), Value::Object(None));
        // Null key and non-wrapper keys decline to the native.
        assert_eq!(jit_chm_get(&mut f.ctx, chm, Value::Object(None)), None);
        let other = f.ctx.alloc_object_of(ClassId::new(9_199), 1);
        assert_eq!(
            jit_chm_get(&mut f.ctx, chm, Value::Object(Some(other))),
            None
        );
    }

    /// The untyped (layout-sniffing) node route: no `HashMap$Node` memo.
    #[test]
    fn r9w11_chm_wrapper_get_answers_like_the_general_path() {
        let (mut f, keys) = r9w11_fixture(1);
        HM_NODE_CLASS_MEMO.with(|c| c.set(None));
        r9w11_assert_answers(&mut f, &keys);
    }

    /// r9w12: the FUSED door answers exactly what the name-ladder fallback
    /// answers, on both node routes.
    ///
    /// `chm_wrapper_chain_walk` and `chm_get_wrapper_key_fast` now ask
    /// `NativeSystemAccess::class_id_and_unboxed_primitive` for a class id and
    /// a primitive under ONE membership walk, and fall back to
    /// `class_id_of_object` + `unbox_wrapper` when the context has no such
    /// door. That is two code paths through the same equality decision --
    /// including the "same hash, same bucket, different wrapper class" row and
    /// the `computeIfAbsent` reservation row -- so it is worth asserting they
    /// do not diverge rather than assuming it.
    #[test]
    fn r9w12_chm_wrapper_get_answers_the_same_through_the_fused_door() {
        let (mut f, keys) = r9w11_fixture_with_door(1, true);
        // Both node routes: untyped (layout-sniffing) ...
        HM_NODE_CLASS_MEMO.with(|c| c.set(None));
        r9w11_assert_answers(&mut f, &keys);
        // ... and the typed batched read.
        let vm = f.ctx.vm_identity();
        HM_NODE_CLASS_MEMO.with(|c| c.set(Some((vm, Some(f.node_cid)))));
        r9w11_assert_answers(&mut f, &keys);
        HM_NODE_CLASS_MEMO.with(|c| c.set(None));
    }

    /// r9w12: the default `class_id_and_unboxed_primitive` IS the two calls.
    ///
    /// The trait's default implementation is what every context without a
    /// fused door gets, and the CHM walk's correctness rests on it answering
    /// the same pair. Asserted for a wrapper, a non-wrapper and a context with
    /// no door at all.
    #[test]
    fn r9w12_class_id_and_unboxed_primitive_default_is_the_pair() {
        for door in [false, true] {
            let mut ctx = lbq_blocking_tests::MockCtx::new(7);
            if door {
                ctx = ctx.with_fast_unbox_door();
            }
            let int_cid = ClassId::new(9_111);
            let other_cid = ClassId::new(9_112);
            ctx.define_class(int_cid, "java/lang/Integer");
            ctx.define_class(other_cid, "com/example/Plain");
            let boxed = ctx.alloc_object_of(int_cid, 1);
            ctx.set_field(boxed, 0, Value::Int(42));
            let plain = ctx.alloc_object_of(other_cid, 1);
            for obj in [boxed, plain] {
                let fused = ctx.class_id_and_unboxed_primitive(obj);
                assert_eq!(
                    fused,
                    (
                        ctx.class_id_of_object(obj),
                        ctx.fast_unbox_primitive_wrapper(obj)
                    ),
                    "fused door disagreed with the pair (door={door})"
                );
            }
            // And the door, when present, agrees with `unbox_wrapper`'s answer.
            if door {
                assert_eq!(
                    ctx.class_id_and_unboxed_primitive(boxed).1,
                    Some(Some(Value::Int(42)))
                );
                assert_eq!(unbox_wrapper(&ctx, boxed), Some(Value::Int(42)));
                assert_eq!(ctx.class_id_and_unboxed_primitive(plain).1, Some(None));
                assert_eq!(unbox_wrapper(&ctx, plain), None);
            }
        }
    }

    /// The typed batched node read, taken when the node class is the memoized
    /// real `HashMap$Node`.
    #[test]
    fn r9w11_chm_wrapper_get_typed_node_route_answers_the_same() {
        let (mut f, keys) = r9w11_fixture(1);
        let vm = f.ctx.vm_identity();
        HM_NODE_CLASS_MEMO.with(|c| c.set(Some((vm, Some(f.node_cid)))));
        assert_eq!(hm_node_class_memoized(&f.ctx), Some(f.node_cid));
        r9w11_assert_answers(&mut f, &keys);
        HM_NODE_CLASS_MEMO.with(|c| c.set(None));
    }

    /// Correct under a concurrent writer: a "resizer" repeatedly cuts a chain
    /// in place (the exact hazard `ChmSegmentResizeGuard` exists for) while a
    /// reader looks up every key of that chain. The in-place walk must never
    /// report one of them absent: an overlapped walk is caught by the resize
    /// epoch and redone under the stripe read lock.
    #[test]
    fn r9w11_chm_wrapper_get_survives_a_concurrent_in_place_relink() {
        let (f, keys) = r9w11_fixture(1);
        // Bucket 1 holds 29, 25, ..., 5 (+ Long(5)), 1 -- head first.
        let chained: Vec<(ObjectRef, i32)> = keys
            .iter()
            .copied()
            .filter(|&(_, k)| map_bucket_index(r9w11_spread(k), R9W11_CAP as i32) == 1)
            .collect();
        assert!(chained.len() >= 8);
        let (chm, seg, buckets) = (f.chm, f.seg, f.buckets);
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        let mut writer_ctx = f.ctx.fork(2);
        let writer_stop = std::sync::Arc::clone(&stop);
        let writer = std::thread::spawn(move || {
            let Value::Object(Some(head)) = writer_ctx.get_array_element(buckets, 1) else {
                panic!("bucket 1 is populated");
            };
            let head_pin = writer_ctx.pin_native_root(head);
            let mut cuts = 0u32;
            while !writer_stop.load(std::sync::atomic::Ordering::Relaxed) {
                let (guard, _seg) = ChmSegmentResizeGuard::acquire_gc_safe(&mut writer_ctx, seg);
                let head = writer_ctx.read_native_pin(head_pin, head);
                let saved = writer_ctx.get_field(head, NODE_FIELD_NEXT);
                writer_ctx.set_field_volatile(head, NODE_FIELD_NEXT, Value::Object(None));
                std::thread::yield_now();
                writer_ctx.set_field_volatile(head, NODE_FIELD_NEXT, saved);
                drop(guard);
                cuts += 1;
                std::thread::yield_now();
            }
            writer_ctx.unpin_native_roots(head_pin);
            cuts
        });

        let mut reader_ctx = f.ctx.fork(3);
        let reader = std::thread::spawn(move || {
            let mut served = 0u32;
            for _ in 0..300 {
                for &(key, k) in &chained {
                    // The probe may DECLINE (a busy stripe -- it never
                    // blocks), and then the site's callback, the registered
                    // native, answers. What it may never do is answer wrong.
                    let got = jit_chm_get(&mut reader_ctx, chm, Value::Object(Some(key)));
                    let v = match got {
                        Some(Value::Object(Some(v))) => {
                            served += 1;
                            v
                        }
                        None => match native_chm_get(
                            &mut reader_ctx,
                            &[Value::Object(Some(chm)), Value::Object(Some(key))],
                        ) {
                            Ok(Some(Value::Object(Some(v)))) => v,
                            other => panic!(
                                "Integer({k}) missing from the general path under a relink: {other:?}"
                            ),
                        },
                        other => panic!(
                            "Integer({k}) answered absent under a concurrent relink: {other:?}"
                        ),
                    };
                    assert_eq!(reader_ctx.get_field(v, 0), Value::Int(k * 10));
                }
            }
            served
        });
        let read = reader.join();
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let cuts = writer.join().expect("writer");
        let served = read.expect("reader never missed a key");
        assert!(cuts > 0, "the writer must actually have relinked");
        assert!(served > 0, "the probe must have served some lookups itself");
    }

    /// Correct under a concurrent **resize**, which the relink test above does
    /// not cover (h23, 2026-09-22: the review index listed this as the
    /// unreviewed half of the chm11 item).
    ///
    /// A relink rewrites `next` inside one array. A resize also REPLACES the
    /// array `MAP_FIELD_BUCKETS` names, with one of a different length, so a
    /// bucket index derived from the old capacity would be meaningless in the
    /// new array. The walk is safe against that by construction, and this
    /// pins the construction down: it reads the bucket reference ONCE
    /// (`cursor_field_volatile_reference`) and takes the capacity from THAT
    /// array (`cursor_array_shape`), never from the segment again, so
    /// `map_bucket_index` is always evaluated against the array it then
    /// indexes. A reader that started before the swap finishes in the old
    /// array; one that started after uses the new one; neither can index an
    /// array with the other's capacity.
    ///
    /// The writer does the whole destructive shape at once — allocate a new
    /// array of a DIFFERENT capacity, relink every existing node into it in
    /// place, then publish it — alternating 4 <-> 8 so it can run in a loop.
    /// The allocation is deliberately OUTSIDE the guard: holding the stripe
    /// write lock across one is the separate, already-fixed bug in
    /// `chm-segment-resize-holds-the-stripe-write-lock-across-an-allocation-FIXED-20260920.md`.
    #[test]
    fn r9w11_chm_wrapper_get_survives_a_concurrent_resize() {
        let (f, keys) = r9w11_fixture(4);
        let (chm, seg) = (f.chm, f.seg);
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        let mut writer_ctx = f.ctx.fork(5);
        let writer_stop = std::sync::Arc::clone(&stop);
        let writer = std::thread::spawn(move || {
            let mut cap = R9W11_CAP;
            let mut resizes = 0u32;
            while !writer_stop.load(std::sync::atomic::Ordering::Relaxed) {
                let new_cap = if cap == R9W11_CAP {
                    R9W11_CAP * 2
                } else {
                    R9W11_CAP
                };
                // OUTSIDE the guard, on purpose: see the doc comment.
                let fresh = writer_ctx.new_ref_array(ClassId::new(0), new_cap);
                let fresh_pin = writer_ctx.pin_native_root(fresh);
                {
                    let (guard, seg) = ChmSegmentResizeGuard::acquire_gc_safe(&mut writer_ctx, seg);
                    let fresh = writer_ctx.read_native_pin(fresh_pin, fresh);
                    let Value::Object(Some(old)) = writer_ctx.get_field(seg, MAP_FIELD_BUCKETS)
                    else {
                        panic!("the segment always has a bucket array");
                    };
                    for i in 0..cap {
                        let mut node = writer_ctx.get_array_element(old, i);
                        while let Value::Object(Some(n)) = node {
                            // Save `next` BEFORE overwriting it: the relink
                            // destroys the chain being walked.
                            let next = writer_ctx.get_field(n, NODE_FIELD_NEXT);
                            let Value::Int(h) = writer_ctx.get_field(n, NODE_FIELD_HASH) else {
                                panic!("a node's hash is an int");
                            };
                            let idx = map_bucket_index(h, new_cap as i32);
                            let head = writer_ctx.get_array_element(fresh, idx);
                            writer_ctx.set_field_volatile(n, NODE_FIELD_NEXT, head);
                            writer_ctx.set_array_element(fresh, idx, Value::Object(Some(n)));
                            node = next;
                        }
                    }
                    writer_ctx.set_field_volatile(
                        seg,
                        MAP_FIELD_BUCKETS,
                        Value::Object(Some(fresh)),
                    );
                    drop(guard);
                }
                writer_ctx.unpin_native_roots(fresh_pin);
                cap = new_cap;
                resizes += 1;
                std::thread::yield_now();
            }
            resizes
        });

        let mut reader_ctx = f.ctx.fork(6);
        let reader = std::thread::spawn(move || {
            let mut served = 0u32;
            for _ in 0..200 {
                for &(key, k) in &keys {
                    // As in the relink test: the probe may DECLINE (it never
                    // blocks on a busy stripe) and the registered native then
                    // answers. What neither may do is answer wrong or absent.
                    let got = jit_chm_get(&mut reader_ctx, chm, Value::Object(Some(key)));
                    let v = match got {
                        Some(Value::Object(Some(v))) => {
                            served += 1;
                            v
                        }
                        None => match native_chm_get(
                            &mut reader_ctx,
                            &[Value::Object(Some(chm)), Value::Object(Some(key))],
                        ) {
                            Ok(Some(Value::Object(Some(v)))) => v,
                            other => panic!(
                                "Integer({k}) missing from the general path under a resize: {other:?}"
                            ),
                        },
                        other => panic!(
                            "Integer({k}) answered absent under a concurrent resize: {other:?}"
                        ),
                    };
                    assert_eq!(reader_ctx.get_field(v, 0), Value::Int(k * 10));
                }
            }
            served
        });
        let read = reader.join();
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let resizes = writer.join().expect("writer");
        let served = read.expect("reader never missed a key across a resize");
        assert!(resizes > 0, "the writer must actually have resized");
        assert!(served > 0, "the probe must have served some lookups itself");
    }

    /// gc-common w13-c: `clone_lhm_overlay_ctx` gives the clone the source's
    /// overlay-only state, but never its structural `head`/`tail`/`size`/
    /// `table`.
    ///
    /// A heap-backed LinkedHashMap's structural state lives in its real heap
    /// fields, which `Object.clone()` copies verbatim. `HashMap.clone()` then
    /// runs `reinitialize()` as real bytecode, which nulls the CLONE's heap
    /// fields directly. A clone that carried the source's `head` in its overlay
    /// row would read it ahead of its own reset field, answer the SOURCE's
    /// list, and link its next insert into the source's nodes.
    #[test]
    fn w13c_lhm_clone_keeps_the_heap_fields_authoritative_on_the_clone() {
        let mut ctx = lbq_blocking_tests::MockCtx::new(1);
        let lhm = ClassId::new(4_131);
        ctx.define_class(lhm, "java/util/LinkedHashMap");
        // Distinct slots are what matters here, not the real JDK indices.
        ctx.define_field(lhm, "table", 0);
        ctx.define_field(lhm, "size", 2);
        ctx.define_field(lhm, "head", 6);
        ctx.define_field(lhm, "tail", 7);
        let src = ctx.alloc_object_of(lhm, 9);
        let node = ctx.alloc_object_of(ClassId::new(0), 6);
        lhm_set(&mut ctx, src, "head", 0, Value::Object(Some(node)));
        // Overlay-only names: no heap field, so only the row carries them.
        lhm_set(&mut ctx, src, "__capacity", 0, Value::Int(64));
        lhm_set(&mut ctx, src, "accessOrder", 0, Value::Int(1));

        // `Object.clone()`: a fresh object of the same class, fields copied.
        let dst = ctx.alloc_object_of(lhm, 9);
        for i in 0..9 {
            let v = ctx.get_field(src, i);
            ctx.set_field(dst, i, v);
        }
        clone_lhm_overlay_ctx(&ctx, src, dst);
        assert_eq!(
            lhm_get(&ctx, dst, "head", 0),
            Value::Object(Some(node)),
            "right after the copy the clone sees the source's list"
        );

        // `HashMap.clone()` -> `reinitialize()`, as real bytecode.
        ctx.set_field(dst, 6, Value::Object(None));
        assert_eq!(
            lhm_get(&ctx, dst, "head", 0),
            Value::Object(None),
            "the clone read the source's `head` out of a copied overlay row \
             after its own heap field was reset"
        );
        assert_eq!(
            lhm_get(&ctx, dst, "accessOrder", 0),
            Value::Int(1),
            "the overlay-only state must still reach the clone"
        );
        // r14w1 compat (`CRATONVM_COMPAT_LHM_CLONE_FRESH_CAPACITY`): the
        // source's table size is not; `reinitialize()` leaves the clone to
        // size its own first table, as `HashMap.clone()` does.
        assert_eq!(
            lhm_get(&ctx, dst, "__capacity", 0),
            Value::Object(None),
            "the clone must not inherit the source's table size"
        );
        let dst_key = lhm_overlay_key(&ctx, dst);
        assert!(
            lhm_heap_backed()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains(&dst_key),
            "the clone must inherit the source's heap-backed membership"
        );
        assert!(
            lhm_overlay()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&dst_key)
                .is_some_and(|row| !row.contains_key("head")),
            "the clone's row must not carry the source's structural state"
        );
        assert_eq!(
            lhm_get(&ctx, src, "head", 0),
            Value::Object(Some(node)),
            "the source is untouched"
        );
    }

    /// Round 13 wave 13, lane compat7 (`CRATONVM_COMPAT_LHM_CTOR_CAPACITY`,
    /// proposal C6-4): `new LinkedHashMap<>(n)` allocates `tableSizeFor(n)`
    /// buckets on its first insert, as `HashMap(n)` does. The old constructor
    /// asked for `ceil(4n/3)` floored at 16 and the lazy table then allocated
    /// 16 whatever was asked (`lhm_state` read no pending size).
    #[test]
    fn r13_compat7_lhm_capacity_ctor_sizes_the_first_table_by_table_size_for() {
        let mut ctx = lbq_blocking_tests::MockCtx::new(1);
        let lhm = ClassId::new(4_137);
        ctx.define_class(lhm, "java/util/LinkedHashMap");
        ctx.define_field(lhm, "threshold", 8);
        for (requested, expected) in [(64, 64), (100, 128), (0, 1), (1, 1), (5, 8), (16, 16)] {
            let m = ctx.alloc_object_of(lhm, 10);
            native_lhm_init_capacity(&mut ctx, &[Value::Object(Some(m)), Value::Int(requested)])
                .expect("a legal capacity");
            let (table, size, cap) = lhm_state(&ctx, m);
            assert!(table.is_none(), "the constructor stays lazy");
            assert_eq!(size, 0);
            assert_eq!(cap, expected, "new LinkedHashMap<>({requested})");
        }
        // A map with no pending size (the no-argument constructor, or a
        // `clone()` whose `reinitialize()` zeroed `threshold`) keeps 16.
        let m = ctx.alloc_object_of(lhm, 10);
        native_lhm_init(&mut ctx, &[Value::Object(Some(m))]).expect("init");
        assert_eq!(lhm_state(&ctx, m).2, MAP_DEFAULT_CAPACITY as i32);
        assert_eq!(lhm_jdk_table_size_for(i32::MAX), 1 << 30);
        assert_eq!(lhm_jdk_table_size_for(-3), 1);
        assert_eq!(lhm_jdk_table_size_for(65), 128);
    }
}
