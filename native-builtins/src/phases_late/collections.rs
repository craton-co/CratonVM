// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! `java.util` collection natives: sequenced collections, NavigableMap/Set, AbstractMap, WeakHashMap, EnumMap/EnumSet, checked wrappers.
//!
//! Pure code move out of `phases_late.rs` (no logic, signature or ordering
//! changes). Every registration call site is untouched and the per-phase
//! dispatchers stay in the parent module, so the native registration SEQUENCE
//! is byte-identical to before the split.

use super::*;

// ---------------------------------------------------------------------------
// Collection extras: IdentityHashMap, Collections.unmodifiableX, etc.
// ---------------------------------------------------------------------------
pub(crate) fn register_phase55_collection_extras(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    // --- IdentityHashMap (same 3-field layout as HashMap) ---
    let ihm = "java/util/IdentityHashMap";
    r.register(ihm, "<init>", "()V", native_al_init_default_for_map);
    r.register(ihm, "<init>", "(I)V", native_al_init_default_for_map);
    r.register(ihm, "size", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 1)))
    });
    r.register(ihm, "isEmpty", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let sz = ctx.get_field(this, 1).as_int().unwrap_or(0);
        Ok(Some(Value::Int(if sz == 0 { 1 } else { 0 })))
    });

    // --- Collections extras (unmodifiable/synchronized already in collections.rs) ---
    let coll = "java/util/Collections";
    r.register(
        coll,
        "synchronizedList",
        "(Ljava/util/List;)Ljava/util/List;",
        crate::native_synchronized_list,
    );
    r.register(
        coll,
        "synchronizedSet",
        "(Ljava/util/Set;)Ljava/util/Set;",
        crate::native_synchronized_set,
    );
    r.register(
        coll,
        "synchronizedMap",
        "(Ljava/util/Map;)Ljava/util/Map;",
        crate::native_synchronized_map,
    );
    r.register(
        coll,
        "synchronizedCollection",
        "(Ljava/util/Collection;)Ljava/util/Collection;",
        crate::native_synchronized_collection,
    );
    r.register(
        coll,
        "checkedList",
        "(Ljava/util/List;Ljava/lang/Class;)Ljava/util/List;",
        |_ctx, args| Ok(Some(args[0])),
    );
    r.register(
        coll,
        "checkedSet",
        "(Ljava/util/Set;Ljava/lang/Class;)Ljava/util/Set;",
        |_ctx, args| Ok(Some(args[0])),
    );
    r.register(
        coll,
        "checkedMap",
        "(Ljava/util/Map;Ljava/lang/Class;Ljava/lang/Class;)Ljava/util/Map;",
        |_ctx, args| Ok(Some(args[0])),
    );
    r.register(
        coll,
        "singleton",
        "(Ljava/lang/Object;)Ljava/util/Set;",
        |ctx, args| {
            let elem = args[0];
            // Pin across the set/array allocs below — a moving young GC there
            // would relocate them (native stale-local family).
            let elem_pin = pinned_object_value(ctx, elem);
            let set = try_alloc_concurrent_synthetic(ctx, "java/util/HashSet", 3)?;
            let set_pin = ctx.pin_native_root(set);
            let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 16);
            let set = ctx.read_native_pin(set_pin, set);
            let elem = read_pinned_object_value(ctx, elem_pin, elem);
            ctx.set_field(set, 0, Value::Object(Some(arr)));
            ctx.set_field(set, 1, Value::Int(0));
            ctx.set_field(set, 2, Value::Int(16));
            // Just store element — we'll use the standard HashSet native_set_add internally
            // But we can't call it directly here. Just allocate and put manually:
            ctx.set_array_element(arr, 0, elem);
            ctx.set_field(set, 1, Value::Int(1));
            ctx.unpin_native_roots(elem_pin.map(|(h, _)| h).unwrap_or(set_pin));
            Ok(Some(Value::Object(Some(set))))
        },
    );
    r.register(
        coll,
        "singletonList",
        "(Ljava/lang/Object;)Ljava/util/List;",
        |ctx, args| {
            let elem = args[0];
            // Pin across the list/array allocs below — a moving young GC there
            // would relocate them (native stale-local family).
            let elem_pin = pinned_object_value(ctx, elem);
            let list = try_alloc_concurrent_synthetic(ctx, "java/util/ArrayList", 2)?;
            let list_pin = ctx.pin_native_root(list);
            let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 1);
            let list = ctx.read_native_pin(list_pin, list);
            let elem = read_pinned_object_value(ctx, elem_pin, elem);
            ctx.set_array_element(arr, 0, elem);
            ctx.set_field(list, 0, Value::Object(Some(arr)));
            ctx.set_field(list, 1, Value::Int(1));
            ctx.unpin_native_roots(elem_pin.map(|(h, _)| h).unwrap_or(list_pin));
            Ok(Some(Value::Object(Some(list))))
        },
    );
    r.register(
        coll,
        "singletonMap",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/util/Map;",
        |ctx, args| {
            let key = args[0];
            let val = args[1];
            // Pin across the map/array/node allocs below — a moving young GC
            // there would relocate them (native stale-local family).
            let key_pin = pinned_object_value(ctx, key);
            let val_pin = pinned_object_value(ctx, val);
            let map = try_alloc_concurrent_synthetic(ctx, "java/util/HashMap", 3)?;
            let map_pin = ctx.pin_native_root(map);
            let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 16);
            let arr_pin = ctx.pin_native_root(arr);
            let map = ctx.read_native_pin(map_pin, map);
            ctx.set_field(map, 0, Value::Object(Some(arr)));
            ctx.set_field(map, 1, Value::Int(0));
            ctx.set_field(map, 2, Value::Int(16));
            // Simple: put at bucket 0
            let node = try_alloc_concurrent_synthetic(ctx, "java/util/HashMap$Node", 4)?;
            let key = read_pinned_object_value(ctx, key_pin, key);
            let val = read_pinned_object_value(ctx, val_pin, val);
            let map = ctx.read_native_pin(map_pin, map);
            let arr = ctx.read_native_pin(arr_pin, arr);
            ctx.set_field(node, 0, key);
            ctx.set_field(node, 1, val);
            ctx.set_field(node, 2, Value::Int(0)); // hash
            ctx.set_field(node, 3, Value::Object(None)); // next
            ctx.set_array_element(arr, 0, Value::Object(Some(node)));
            ctx.set_field(map, 1, Value::Int(1));
            let first_pin = key_pin
                .map(|(h, _)| h)
                .or(val_pin.map(|(h, _)| h))
                .unwrap_or(map_pin);
            ctx.unpin_native_roots(first_pin);
            Ok(Some(Value::Object(Some(map))))
        },
    );
    r.register(
        coll,
        "nCopies",
        "(ILjava/lang/Object;)Ljava/util/List;",
        |ctx, args| {
            let n = args[0].as_int().unwrap_or(0) as usize;
            let elem = args[1];
            // Pin across the list/array allocs below — a moving young GC there
            // would relocate them (native stale-local family).
            let elem_pin = pinned_object_value(ctx, elem);
            let list = try_alloc_concurrent_synthetic(ctx, "java/util/ArrayList", 2)?;
            let list_pin = ctx.pin_native_root(list);
            let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, n);
            let list = ctx.read_native_pin(list_pin, list);
            let elem = read_pinned_object_value(ctx, elem_pin, elem);
            for i in 0..n {
                ctx.set_array_element(arr, i, elem);
            }
            ctx.set_field(list, 0, Value::Object(Some(arr)));
            ctx.set_field(list, 1, Value::Int(n as i32));
            ctx.unpin_native_roots(elem_pin.map(|(h, _)| h).unwrap_or(list_pin));
            Ok(Some(Value::Object(Some(list))))
        },
    );
    r.register(
        coll,
        "frequency",
        "(Ljava/util/Collection;Ljava/lang/Object;)I",
        |ctx, args| {
            // Collection is typically an ArrayList with backing array at field 0, size at field 1
            let col = obj_arg(args, 1)?;
            let target = args.get(2).copied().unwrap_or(Value::Object(None));
            let arr_val = ctx.get_field(col, 0);
            let size = match ctx.get_field(col, 1) {
                Value::Int(n) => n as usize,
                _ => 0,
            };
            let mut count = 0i32;
            if let Value::Object(Some(arr)) = arr_val {
                let len = ctx.array_length(arr).min(size);
                for i in 0..len {
                    let elem = ctx.get_array_element(arr, i);
                    if values_equal(&elem, &target) {
                        count += 1;
                    }
                }
            }
            Ok(Some(Value::Int(count)))
        },
    );
    // NOTE: `Collections.disjoint` is deliberately NOT registered as a native.
    // It used to be stubbed here to always return `1` ("assume disjoint"), which
    // silently produced wrong answers (e.g. keycloak DisclosureRedListTest:
    // `Collections.disjoint(redList, {"vct"})` returned true even though both
    // sets share "vct", so the red-list guard never threw). The real
    // `java.util.Collections.disjoint` is a small pure-Java method (iterate one
    // collection, `contains` on the other, with the Set-size optimisation) and
    // runs correctly on CratonVM, so we let the real bytecode handle it.
    r.set_category(__prev_cat);
}

/// Helper to init a map-like object with 3 fields (buckets, size, capacity)
pub(crate) fn native_al_init_default_for_map(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let cap = 16;
    // Pin across the array alloc below — a moving young GC there would
    // relocate `this` (native stale-local family).
    let this_pin = ctx.pin_native_root(this);
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, cap);
    let this = ctx.read_native_pin(this_pin, this);
    ctx.set_field(this, 0, Value::Object(Some(arr)));
    ctx.set_field(this, 1, Value::Int(0));
    ctx.set_field(this, 2, Value::Int(cap as i32));
    ctx.unpin_native_roots(this_pin);
    Ok(Some(Value::Object(None)))
}

// =============================================================================
// AbstractMap expansion — base class methods for Map implementations
// =============================================================================

pub(crate) fn register_p60_abstract_map(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let am = "java/util/AbstractMap";
    // `AbstractMap.isEmpty()` is `return size() == 0;` in the real JDK, and
    // `size()` is ABSTRACT there — every concrete subclass supplies it. Ask for
    // it virtually, which is both the real body and layout-independent.
    //
    // This used to read raw slot 1 as the size, which assumed CratonVM's
    // synthetic 3-slot map layout. `AbstractMap` is a CLASS, so this native
    // intercepts every Map subclass that INHERITS `isEmpty` rather than
    // overriding it — `TreeMap`, `Collections$UnmodifiableMap`, and friends —
    // and on those, slot 1 is some unrelated field. When it happened to hold 0,
    // or anything that is not an `Int`, a fully populated map reported itself
    // EMPTY. That is the root cause behind the Kafka
    // `MetaPropertiesEnsemble.verify` shim ("No readable meta.properties files
    // found" — its populated `logDirProps` map looked empty here), and it is
    // live in the DEFAULT build.
    //
    // The slot read survives only as a fallback for a receiver that has no
    // reachable `size()` — i.e. a bare synthetic `AbstractMap` — so synthetic
    // behaviour is unchanged. No recursion risk: `size()` is not registered on
    // `AbstractMap`, so it can only resolve to a concrete subclass's own
    // implementation.
    r.register(am, "isEmpty", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if let Ok(Some(Value::Int(n))) = ctx.invoke_virtual(this, "size", "()I", &[]) {
            return Ok(Some(Value::Int(i32::from(n == 0))));
        }
        let size = match ctx.get_field(this, 1) {
            Value::Int(v) => v,
            _ => 0,
        };
        Ok(Some(Value::Int(if size == 0 { 1 } else { 0 })))
    });
    // W2: both used to answer a constant `false`. `AbstractMap` is a CLASS, so
    // these natives intercept every Map subclass that inherits (rather than
    // overrides) `containsKey`/`containsValue` — which is the normal case,
    // since providing them is the whole point of extending `AbstractMap`. Such
    // a map reported that it contained nothing at all, while its sibling
    // `size()`/`isEmpty()`/`toString()` natives right here read the real entry
    // count and disagreed.
    //
    // Serve them from the same bucket walk the concrete maps use. These are the
    // very helpers the `WeakHashMap` registration ~650 lines below already
    // shares, and they understand the 3-slot (buckets, size, capacity) layout
    // `native_al_init_default_for_map` above installs — the same layout the
    // neighbouring `isEmpty` reads its size from.
    r.register(
        am,
        "containsKey",
        "(Ljava/lang/Object;)Z",
        cratonvm_native_collections::native_map_contains_key_pub,
    );
    r.register(
        am,
        "containsValue",
        "(Ljava/lang/Object;)Z",
        cratonvm_native_collections::native_map_contains_value_pub,
    );
    // Same raw-slot-1 hazard as `isEmpty` above, and the same fix: ask the
    // receiver for its own `size()`. This one cannot be made fully faithful
    // here — the real `AbstractMap.toString()` renders every entry — but
    // `{size=N}` with the RIGHT N beats `{size=0}` for a populated real map,
    // which is what a slot read produced.
    r.register(am, "toString", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let size = match ctx.invoke_virtual(this, "size", "()I", &[]) {
            Ok(Some(Value::Int(n))) => n,
            _ => match ctx.get_field(this, 1) {
                Value::Int(v) => v,
                _ => 0,
            },
        };
        let s = ctx.create_string(&format!("{{size={size}}}"));
        Ok(Some(Value::Object(Some(s))))
    });
    // SHIM-AUDIT (feature-designs/native-builtins-shim-audit.md, row
    // `java/util/AbstractMap`) — `equals`/`hashCode` are DELIBERATELY NOT
    // registered here. They used to be, as:
    //
    //     hashCode -> Value::Int(this.as_ptr() as i32)
    //     equals   -> this.as_ptr() == other.as_ptr()
    //
    // Three things were wrong with that, and `AbstractMap` being an ABSTRACT
    // CLASS is what made all three reachable. The native-override hierarchy
    // walk (`vm/src/runtime/interpreter/invoke.rs`, the `walk_native_hierarchy`
    // loop) climbs a receiver's SUPERCLASS chain and, at each ancestor, looks
    // for a native BEFORE it asks whether that ancestor has bytecode. Neither
    // `java.util.HashMap` nor `TreeMap`, `LinkedHashMap`, `EnumMap`,
    // `Collections$UnmodifiableMap` nor any user `class X extends AbstractMap`
    // declares `equals`/`hashCode` — they all inherit them — so this pair
    // intercepted *every map in the VM*:
    //
    //  1. WRONG ANSWER. The real `AbstractMap.equals` is entry-wise and
    //     `AbstractMap.hashCode` is the sum of entry hashes. Identity made two
    //     maps with identical contents unequal and gave them different hashes
    //     — the JDK's map-in-a-set / map-as-a-key contract, inverted.
    //  2. UNSTABLE HASH. `this.as_ptr() as i32` is a RAW HEAP ADDRESS, not the
    //     VM's identity hash. Under a moving young collection the object
    //     relocates and its `hashCode()` silently changes, so a map used as a
    //     key is lost from its own bucket across a GC. `Object.hashCode`'s
    //     native (`native_object_hash_code`) uses `ctx.identity_hash_code`,
    //     which is stable across relocation — the two natives disagreed about
    //     what "identity hash" even means.
    //  3. IT WON OVER CORRECT BYTECODE. In a mixed run where the real
    //     `java.util.AbstractMap` is loaded, its correct entry-wise bytecode
    //     was shadowed by (1).
    //
    // Refusing is strictly better than reimplementing, in BOTH modes, which is
    // why nothing replaces them:
    //   * real `AbstractMap` bytecode present -> the walk finds no native on
    //     `AbstractMap`, sees `has_bytecode`, and stops; the real entry-wise
    //     implementation runs. Correct.
    //   * bare synthetic `AbstractMap` stub (no bytecode) -> the walk continues
    //     to `java/lang/Object` and lands on the `Object.equals`/
    //     `Object.hashCode` natives, i.e. identity — the same answer the
    //     deleted shims gave, minus the moving-GC instability of (2).
    //
    // Do not "restore" these without a receiver-driven entry walk; an identity
    // answer on an abstract collection base class is never right.
    r.set_category(__prev_cat);
}

// =============================================================================
// NavigableMap/NavigableSet completion — REMOVED, see below.
// =============================================================================

/// **Deliberately empty.** Kept only because `phases_late.rs` still calls it;
/// deleting the call site is a nomination (`G41-1` §7 N1a), not this file's to
/// make.
///
/// # What used to be here, and why it is gone
///
/// This pass registered 24 triples — `floor/ceiling/higher/lower` × `Key/Entry`
/// on `java/util/TreeMap` and `java/util/NavigableMap`, and `floor/ceiling/
/// higher/lower` on `java/util/TreeSet` and `java/util/NavigableSet` — against
/// local `p62_tm_*` / `p62_ts_*` bodies that were **linear scans over the
/// interleaved `[k0,v0,k1,v1,…]` slot-0 array using a `natural_compare_values`
/// helper that only knew the four primitive `Value` arms and answered `0` for
/// everything else.**
///
/// `native-collections/src/lib.rs` registers the SAME 24 triples
/// (`register_tree_map_natives` / `register_tree_set_natives`) against
/// `native_tm_*` / `native_ts_*`, which call `tm_sync_native_state` first, take
/// a `tm_fast_with` BTree range path, honour a user-supplied `Comparator`, and
/// deliberately re-read `data` after a comparator call because that call can
/// move the heap (the "Family-1 stale-`ObjectRef` fix" in that file).
///
/// This pass is reachable ONLY from `register_synthetic_overrides`, which is
/// `#[cfg(feature = "synthetic-jdk")]` and runs LAST. `register()` is
/// last-write-wins. So:
///
/// * a shipping build (`--jdk-only` included) ran the `native_tm_*` bodies;
/// * a `--features synthetic-jdk` build ran these `p62_*` ones.
///
/// The two answer differently for any `TreeMap`/`TreeSet` with a custom
/// `Comparator` or with non-primitive keys, and the copy every synthetic-JDK
/// test measured was the weaker one. That is the `register_pe_panama` /
/// `structLayout` defect in a second family.
///
/// # Why deleting these arms takes nothing with it — MEASURED, not reasoned
///
/// `F34-1` §5's trap is that dropping a synthetic-only pass can drop triples
/// its shipping twin never registered. It does not apply here, and this was
/// checked against `--dump-native-registry` rather than against the source:
///
/// * `cratonvm --dump-native-registry` (compatible mode, no `synthetic-jdk`):
///   all 24 triples present, `kind = bridge`, `owns_slot = true`,
///   `overwrote = null`, `registered_by = native-collections/src/lib.rs`
///   :48609–48651 (`TreeMap`), :48821–48863 (`NavigableMap`), :48980–48998
///   (`TreeSet`), :49116–49134 (`NavigableSet`).
/// * `cratonvm --jdk-only --dump-native-registry`: the identical 24 rows, same
///   kind, same owner, `synthetic-stub` count 0 for the whole registry.
///
/// `Bridge` is `allowed_in(JdkOnly)` (`native-api/src/registry.rs`
/// `allowed_in` refuses only `SyntheticStub`), and `G34-1` settled that
/// registering a `Bridge` is by itself sufficient to preempt real JDK
/// bytecode. So the shipping bodies serve all 24 triples in BOTH modes, and
/// the interface (`NavigableMap`/`NavigableSet`) triples — the ones §5's trap
/// is actually about — are among them.
///
/// The registrar's own arms were the complete set: 8 `TreeMap` + 8
/// `NavigableMap` + 4 `TreeSet` + 4 `NavigableSet`, no fifth class, no `for`
/// loop, nothing else. Removing them removes exactly 24 drifting triples and
/// no capability.
///
/// `native-builtins/tests/registrar_drift.rs` pins this: the 24 triples are a
/// vacuity-guarded NEGATIVE control (`FIXED_NOT_DRIFTING`) — they must still be
/// registered, and must no longer drift. If the twin comes back, that test
/// reddens.
pub(crate) fn register_p62_navigable_expansion(_r: &mut NativeMethodRegistry) {
    // Intentionally no registrations. Do NOT re-add a TreeMap/TreeSet body
    // here: `native-collections/src/lib.rs` owns these 24 slots in every mode,
    // and a second copy is only ever reachable in one of them. The `p62_tm_*`
    // / `p62_ts_*` bodies and their `natural_compare_values` helper were
    // deleted with the registrations rather than left behind, because a fix
    // landing in an unreachable body is a failure mode this branch has already
    // had once.
}

// =============================================================================
// AbstractMap.SimpleEntry / SimpleImmutableEntry = 2-field (key=0, value=1)
// =============================================================================

/// `String.valueOf(o)` for an entry component: `"null"`, the string itself, or
/// the object's own `toString`.
///
/// `Map.Entry`'s contract spells its rendering out — the entry's string form is
/// `getKey() + "=" + getValue()` — so the components have to be rendered the
/// way Java renders them, not summarised.
fn entry_component_string(ctx: &mut dyn NativeContext, v: Value) -> String {
    match v {
        Value::Object(Some(o)) => {
            if let Some(s) = ctx.read_string(o) {
                return s;
            }
            match ctx.invoke_virtual(o, "toString", "()Ljava/lang/String;", &[]) {
                Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s).unwrap_or_default(),
                _ => String::new(),
            }
        }
        Value::Object(None) => "null".to_string(),
        Value::Int(i) => i.to_string(),
        Value::Long(l) => l.to_string(),
        Value::Float(f) => f.to_string(),
        Value::Double(d) => d.to_string(),
        // Neither can be an entry component: they are interpreter-internal
        // operand shapes, never a Java reference or primitive value.
        Value::ReturnAddress(_) | Value::Uninitialized => String::new(),
    }
}

/// `Object.hashCode()` of an entry component, with the null-is-zero rule
/// `Map.Entry.hashCode` depends on.
fn entry_component_hash(ctx: &mut dyn NativeContext, v: Value) -> i32 {
    match v {
        Value::Object(Some(o)) => match ctx.invoke_virtual(o, "hashCode", "()I", &[]) {
            Ok(Some(Value::Int(h))) => h,
            _ => 0,
        },
        Value::Object(None) => 0,
        Value::Int(i) => i,
        Value::Long(l) => (l ^ ((l as u64) >> 32) as i64) as i32,
        _ => 0,
    }
}

/// `true` iff `obj` is a `java.util.Map.Entry` — the type half of
/// `Map.Entry.equals`'s contract.
///
/// The fabricated `AbstractMap$SimpleEntry` / `$SimpleImmutableEntry` did not
/// declare the interface at all until `jdk_interfaces` grew an arm for them,
/// which is why adding this test is safe now and would have made every entry
/// comparison answer false before.
fn entry_is_map_entry(ctx: &dyn NativeContext, obj: ObjectRef) -> bool {
    match ctx.class_id_by_name("java/util/Map$Entry") {
        Some(target) => {
            let cid = ctx.class_id_of_object(obj);
            cid == target || ctx.is_subclass(cid, target)
        }
        None => false,
    }
}

/// `Objects.equals(a, b)` for entry components.
fn entry_component_eq(ctx: &mut dyn NativeContext, a: Value, b: Value) -> bool {
    match (a, b) {
        (Value::Object(None), Value::Object(None)) => true,
        (Value::Object(Some(x)), Value::Object(Some(y))) => {
            if x == y {
                return true;
            }
            matches!(
                ctx.invoke_virtual(
                    x,
                    "equals",
                    "(Ljava/lang/Object;)Z",
                    &[Value::Object(Some(y))]
                ),
                Ok(Some(Value::Int(1)))
            )
        }
        (Value::Int(x), Value::Int(y)) => x == y,
        (Value::Long(x), Value::Long(y)) => x == y,
        _ => false,
    }
}

/// Register the three `Map.Entry` methods whose behaviour the interface
/// SPECIFIES, on a 2-field (key=0, value=1) entry class.
///
/// These were absent, and absence is not neutral here: `toString` fell back to
/// a placeholder that printed the object's address, and `equals`/`hashCode`
/// fell back to identity — so two entries with equal keys and values compared
/// unequal and hashed differently, which is precisely what `Map.Entry`'s
/// contract forbids and what any code putting entries in a set relies on.
fn register_entry_value_semantics(r: &mut NativeMethodRegistry, cls: &'static str) {
    r.register(cls, "toString", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let k = ctx.get_field(this, 0);
        let v = ctx.get_field(this, 1);
        let ks = entry_component_string(ctx, k);
        let vs = entry_component_string(ctx, v);
        let s = ctx.create_string(&format!("{ks}={vs}"));
        Ok(Some(Value::Object(Some(s))))
    });
    r.register(cls, "hashCode", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let k = ctx.get_field(this, 0);
        let v = ctx.get_field(this, 1);
        // Map.Entry.hashCode() is specified as key.hashCode() ^ value.hashCode().
        let h = entry_component_hash(ctx, k) ^ entry_component_hash(ctx, v);
        Ok(Some(Value::Int(h)))
    });
    r.register(cls, "equals", "(Ljava/lang/Object;)Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let other = match args.get(1) {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Int(0))),
        };
        if this == other {
            return Ok(Some(Value::Int(1)));
        }
        // ANY Map.Entry, and ONLY a Map.Entry. The type test is half of the
        // specified contract and it was missing here: without it an entry
        // compares equal to any object that merely answers `getKey` and
        // `getValue`. `native_entry_equals` in `native-collections` — which
        // registers the SAME triple for `SimpleEntry` and wins the
        // last-write-wins race — has always had it, so the two spellings only
        // agree with this present.
        if !entry_is_map_entry(ctx, other) {
            return Ok(Some(Value::Int(0)));
        }
        // Compare against ANY Map.Entry, as the contract requires — via the
        // interface accessors, not by reaching into the other object's slots,
        // which would assume it has this class's layout.
        // `getKey()` is arbitrary Java and can move `other` before
        // `getValue()` below dereferences it.
        let other_pin = ctx.pin_native_root(other);
        let ok = match ctx.invoke_virtual(other, "getKey", "()Ljava/lang/Object;", &[]) {
            Ok(Some(v)) => v,
            _ => return Ok(Some(Value::Int(0))),
        };
        let other = ctx.read_native_pin(other_pin, other);
        let ov = match ctx.invoke_virtual(other, "getValue", "()Ljava/lang/Object;", &[]) {
            Ok(Some(v)) => v,
            _ => return Ok(Some(Value::Int(0))),
        };
        let k = ctx.get_field(this, 0);
        let v = ctx.get_field(this, 1);
        let eq = entry_component_eq(ctx, k, ok) && entry_component_eq(ctx, v, ov);
        Ok(Some(Value::Int(i32::from(eq))))
    });
}

pub(crate) fn register_p62_abstract_map_entries(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let se = "java/util/AbstractMap$SimpleEntry";
    r.register(
        se,
        "<init>",
        "(Ljava/lang/Object;Ljava/lang/Object;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.set_field(this, 0, args.get(1).copied().unwrap_or(Value::Object(None)));
            ctx.set_field(this, 1, args.get(2).copied().unwrap_or(Value::Object(None)));
            Ok(None)
        },
    );
    r.register(se, "getKey", "()Ljava/lang/Object;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    });
    r.register(se, "getValue", "()Ljava/lang/Object;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 1)))
    });
    r.register(
        se,
        "setValue",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let old = ctx.get_field(this, 1);
            ctx.set_field(this, 1, args.get(1).copied().unwrap_or(Value::Object(None)));
            Ok(Some(old))
        },
    );
    // `entry@<address>` was a placeholder, and it is what
    // `LinkedHashMap.entrySet().toString()` printed instead of `one=1;two=2;`.
    register_entry_value_semantics(r, se);

    // SimpleImmutableEntry (same layout, setValue throws)
    let sie = "java/util/AbstractMap$SimpleImmutableEntry";
    r.register(
        sie,
        "<init>",
        "(Ljava/lang/Object;Ljava/lang/Object;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.set_field(this, 0, args.get(1).copied().unwrap_or(Value::Object(None)));
            ctx.set_field(this, 1, args.get(2).copied().unwrap_or(Value::Object(None)));
            Ok(None)
        },
    );
    r.register(sie, "getKey", "()Ljava/lang/Object;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    });
    r.register(sie, "getValue", "()Ljava/lang/Object;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 1)))
    });
    r.register(
        sie,
        "setValue",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        |_ctx, _args| {
            Err(RuntimeError::UnsupportedOperationException {
                message: "immutable entry".into(),
            }
            .into())
        },
    );
    register_entry_value_semantics(r, sie);

    // `Map.entry(k, v)` (phases_late.rs) mints an instance whose class is
    // literally `java/util/Map$Entry`, and that class had NO natives at all,
    // so `toString` fell through to `Object`'s: `java.util.Map$Entry@6c`
    // where the contract says `k=7`.
    //
    // Registering on the interface name is safe for exactly these three and
    // was checked rather than assumed. `LinkedHashMap`'s entry views are
    // `AbstractMap$SimpleEntry` instances — giving `se` its `toString` above
    // is what fixed `LinkedHashMap.entrySet().toString()` — so they resolve
    // their own class's natives. And all three read only slots 0 and 1, which
    // both shapes of this class agree on.
    //
    // `setValue` is deliberately NOT registered on this name, and that is the
    // interesting half. `java/util/Map$Entry` is also minted as a THREE-field
    // entry — `key@0, value@1, sourceMap@2` — by the entry-set views in
    // native-collections and properties_sidetable, precisely so that
    // `Entry.setValue` WRITES THROUGH to the backing map, which
    // `entrySet()` iteration requires. That is the only contract this NAME
    // carries now.
    register_entry_value_semantics(r, "java/util/Map$Entry");

    // `Map.entry(k, v)` has a class of its own — `java/util/KeyValueHolder`,
    // which is what HotSpot answers for `Map.entry(..).getClass()`.
    //
    // Until it did, `Map.entry`'s 2-field immutable entry and the entry-set
    // views' 3-field write-through entry were the same synthetic class name
    // wearing two contradictory contracts, and `setValue` was decided by
    // last-write-wins. An immutable `setValue` could only ever win that race
    // by breaking every `entrySet()` write-through, so it was not registered
    // at all and `Map.entry(..).setValue(v)` stayed permissive — the last
    // diverging line of `probes/ShadowDifferentialProbe.java` under
    // `--synthetic-jdk`. A separate class removes the race rather than
    // choosing a side of it.
    //
    // `--real-jdk` is untouched: `java.util.Map.entry` is ordinary bytecode in
    // `java.base`, so the registry drops the `Map.entry` registration and the
    // JDK mints its own `KeyValueHolder`, which never reaches these natives.
    //
    // `SyntheticStub` rather than this registrar's ambient `Bridge`: the real
    // `java.util.KeyValueHolder` in `java.base` declares no `ACC_NATIVE`
    // method, so contract §1.5 cannot call these bridges, and under
    // `--jdk-only` they must drop so the JDK's own final `key`/`value` fields
    // and its own `setValue` throw are what run.
    let kvh_prev = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::SyntheticStub);
    let kvh = "java/util/KeyValueHolder";
    r.register(kvh, "getKey", "()Ljava/lang/Object;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    });
    r.register(kvh, "getValue", "()Ljava/lang/Object;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 1)))
    });
    r.register(
        kvh,
        "setValue",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        |_ctx, _args| {
            // `KeyValueHolder.setValue` throws unconditionally in the JDK, and
            // the silent-success this replaces was the dangerous half: a caller
            // defensively mutating a copy got no signal that it had mutated
            // something nobody would read.
            Err(RuntimeError::UnsupportedOperationException {
                message: "not supported".into(),
            }
            .into())
        },
    );
    register_entry_value_semantics(r, kvh);
    r.set_category(kvh_prev);
    r.set_category(__prev_cat);
}

// =============================================================================
// WeakHashMap = 3-field (same as HashMap: buckets=0, size=1, capacity=2)
// Delegates to HashMap natives for all core operations
// =============================================================================

pub(crate) fn register_p63_weak_hash_map(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    // Same synthetic fallback as the early registration: a real JDK
    // WeakHashMap must use its bytecode-backed layout.
    r.set_category(cratonvm_native_api::NativeKind::SyntheticStub);
    let whm = "java/util/WeakHashMap";
    r.register(whm, "<init>", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let cap = 16usize;
        let buckets = ctx.new_array(cratonvm_types::ArrayElementType::Reference, cap);
        ctx.set_field(this, 0, Value::Object(Some(buckets)));
        ctx.set_field(this, 1, Value::Int(0));
        ctx.set_field(this, 2, Value::Int(cap as i32));
        Ok(None)
    });
    r.register(whm, "<init>", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let cap = match args.get(1) {
            Some(Value::Int(v)) => (*v).max(1) as usize,
            _ => 16,
        };
        let buckets = ctx.new_array(cratonvm_types::ArrayElementType::Reference, cap);
        ctx.set_field(this, 0, Value::Object(Some(buckets)));
        ctx.set_field(this, 1, Value::Int(0));
        ctx.set_field(this, 2, Value::Int(cap as i32));
        Ok(None)
    });
    r.register(whm, "size", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 1)))
    });
    r.register(whm, "isEmpty", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let size = match ctx.get_field(this, 1) {
            Value::Int(v) => v,
            _ => 0,
        };
        Ok(Some(Value::Int(if size == 0 { 1 } else { 0 })))
    });
    r.register(
        whm,
        "get",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        cratonvm_native_collections::native_map_get_pub,
    );
    r.register(
        whm,
        "containsKey",
        "(Ljava/lang/Object;)Z",
        cratonvm_native_collections::native_map_contains_key_pub,
    );
    r.register(whm, "clear", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, 0, Value::Object(None));
        ctx.set_field(this, 1, Value::Int(0));
        Ok(None)
    });
    r.register(whm, "keySet", "()Ljava/util/Set;", |ctx, _args| {
        // Return empty HashSet stub
        let set = try_alloc_concurrent_synthetic(ctx, "java/util/HashSet", 3)?;
        ctx.set_field(set, 0, Value::Object(None));
        ctx.set_field(set, 1, Value::Int(0));
        ctx.set_field(set, 2, Value::Int(16));
        Ok(Some(Value::Object(Some(set))))
    });
    r.register(whm, "values", "()Ljava/util/Collection;", |ctx, _args| {
        let al = try_alloc_concurrent_synthetic(ctx, "java/util/ArrayList", 2)?;
        ctx.set_field(al, 0, Value::Object(None));
        ctx.set_field(al, 1, Value::Int(0));
        Ok(Some(Value::Object(Some(al))))
    });
    r.set_category(__prev_cat);
}

// =============================================================================
// Enumeration — empty stub + Collections.emptyEnumeration, Collections.enumeration
// =============================================================================

pub(crate) fn register_p63_enumeration(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    // Empty enumeration
    let ee = "java/util/Collections$EmptyEnumeration";
    // KEEP — verbatim real behaviour, verified against JDK 25 source
    // (`java.base/java/util/Collections.java`):
    //
    //     private static class EmptyEnumeration<E> implements Enumeration<E> {
    //         static final EmptyEnumeration<Object> EMPTY_ENUMERATION = ...;
    //         public boolean hasMoreElements() { return false; }
    //         public E nextElement() { throw new NoSuchElementException(); }
    //     }
    //
    // The real body is `return false;`, so there is no receiver state to read:
    // the class IS the empty enumeration, and it is `private static` with no
    // subclasses, so the "natives on concrete classes intercept non-overriding
    // subclasses" hazard does not apply. (Its `nextElement` right below
    // correctly throws NoSuchElementException, matching the second line.)
    r.register(ee, "hasMoreElements", "()Z", |_ctx, _args| {
        Ok(Some(Value::Int(0)))
    });
    r.register(ee, "nextElement", "()Ljava/lang/Object;", |_ctx, _args| {
        // Was `IllegalStateException` carrying the string "NoSuchElementException"
        // — a caller's `catch (NoSuchElementException)` never matched it. Real
        // `Collections.EmptyEnumeration.nextElement()` throws the real thing.
        Err(RuntimeError::NoSuchElementException {
            message: "EmptyEnumeration has no elements".into(),
        }
        .into())
    });

    let cols = "java/util/Collections";
    r.register(
        cols,
        "emptyEnumeration",
        "()Ljava/util/Enumeration;",
        |ctx, _args| {
            let e = try_alloc_concurrent_synthetic(ctx, "java/util/Collections$EmptyEnumeration", 0)?;
            Ok(Some(Value::Object(Some(e))))
        },
    );
    // synthetic-stub removed: defers to real JDK bytecode
    // removed — VERIFY real bytecode covers it
    // Collections.enumeration(Collection) was a divergent fake: it ignored its
    // Collection argument and returned an EMPTY enumeration regardless of input.
    // Real java/util/Collections.enumeration is plain bytecode that wraps the
    // collection's iterator (hasMoreElements/nextElement delegate to it), and
    // CratonVM already models Collection/Iterator, so the real bytecode runs.

    // Enumeration interface
    //
    // NEW-14: the previous stub hardcoded `hasMoreElements` to false and
    // `nextElement` to throw. Our synthetic Enumeration is a 2-field
    // struct (`array: Object[]`, `pos: int`) populated by
    // `NetworkInterface.getNetworkInterfaces`, `DriverManager.getDrivers`,
    // and the other enumerators in native-builtins. The real impl
    // simply walks the array until `pos >= array.length`.
    let en = "java/util/Enumeration";
    r.register(en, "hasMoreElements", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let pos = ctx.get_field(this, 1).as_int().unwrap_or(0) as usize;
        let len = match ctx.get_field(this, 0) {
            Value::Object(Some(arr)) => ctx.array_length(arr),
            _ => 0,
        };
        Ok(Some(Value::Int(if pos < len { 1 } else { 0 })))
    });
    r.register(en, "nextElement", "()Ljava/lang/Object;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let pos = ctx.get_field(this, 1).as_int().unwrap_or(0) as usize;
        let arr = match ctx.get_field(this, 0) {
            Value::Object(Some(a)) => a,
            _ => {
                return Err(RuntimeError::NoSuchElementException {
                    message: "empty Enumeration".into(),
                }
                .into());
            }
        };
        let len = ctx.array_length(arr);
        if pos >= len {
            return Err(RuntimeError::NoSuchElementException {
                message: "Enumeration exhausted".into(),
            }
            .into());
        }
        let elem = ctx.get_array_element(arr, pos);
        ctx.set_field(this, 1, Value::Int((pos + 1) as i32));
        Ok(Some(elem))
    });
    r.register(en, "asIterator", "()Ljava/util/Iterator;", |ctx, _args| {
        let itr = try_alloc_concurrent_synthetic(ctx, "java/util/Collections$EmptyItr", 2)?;
        ctx.set_field(itr, 0, Value::Object(None));
        ctx.set_field(itr, 1, Value::Int(0));
        Ok(Some(Value::Object(Some(itr))))
    });
    r.set_category(__prev_cat);
}

// =============================================================================
// SequencedCollection / SequencedSet / SequencedMap — Java 21 interfaces
// Register default methods for ArrayList, LinkedList, ArrayDeque, TreeSet,
// LinkedHashMap, TreeMap etc.
// =============================================================================

pub(crate) fn register_p64_sequenced_collections(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    // SequencedCollection interface
    let sc = "java/util/SequencedCollection";
    r.register(
        sc,
        "getFirst",
        "()Ljava/lang/Object;",
        native_p64_seq_get_first,
    );
    r.register(
        sc,
        "getLast",
        "()Ljava/lang/Object;",
        native_p64_seq_get_last,
    );
    r.register(
        sc,
        "reversed",
        "()Ljava/util/SequencedCollection;",
        native_p64_seq_reversed,
    );
    r.register(sc, "addFirst", "(Ljava/lang/Object;)V", |ctx, args| {
        // Default impl: add to start of the underlying list/deque
        let this = obj_arg(args, 0)?;
        let elem = args.get(1).copied().unwrap_or(Value::Object(None));
        // Try add(int, Object) with index 0 for List-like collections
        let _ = ctx.invoke_virtual(
            this,
            "add",
            "(ILjava/lang/Object;)V",
            &[Value::Int(0), elem],
        );
        Ok(None)
    });
    r.register(sc, "addLast", "(Ljava/lang/Object;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let elem = args.get(1).copied().unwrap_or(Value::Object(None));
        // Try add(Object) which appends
        let _ = ctx.invoke_virtual(this, "add", "(Ljava/lang/Object;)Z", &[elem]);
        Ok(None)
    });
    r.register(
        sc,
        "removeFirst",
        "()Ljava/lang/Object;",
        native_p64_seq_get_first,
    );
    r.register(
        sc,
        "removeLast",
        "()Ljava/lang/Object;",
        native_p64_seq_get_last,
    );

    // Register getFirst/getLast + reversed for ArrayList
    let al = "java/util/ArrayList";
    r.register(
        al,
        "getFirst",
        "()Ljava/lang/Object;",
        native_p64_al_get_first,
    );
    r.register(
        al,
        "getLast",
        "()Ljava/lang/Object;",
        native_p64_al_get_last,
    );
    r.register(al, "reversed", "()Ljava/util/List;", native_p64_al_reversed);

    // Register getFirst/getLast + reversed for LinkedList
    let ll = "java/util/LinkedList";
    r.register(
        ll,
        "getFirst",
        "()Ljava/lang/Object;",
        native_p64_ll_get_first,
    );
    r.register(
        ll,
        "getLast",
        "()Ljava/lang/Object;",
        native_p64_ll_get_last,
    );
    r.register(ll, "reversed", "()Ljava/util/List;", native_p64_ll_reversed);

    // SequencedSet interface
    let ss = "java/util/SequencedSet";
    r.register(
        ss,
        "getFirst",
        "()Ljava/lang/Object;",
        native_p64_seq_get_first,
    );
    r.register(
        ss,
        "getLast",
        "()Ljava/lang/Object;",
        native_p64_seq_get_last,
    );
    r.register(
        ss,
        "reversed",
        "()Ljava/util/SequencedSet;",
        native_p64_seq_reversed,
    );

    // SequencedMap interface
    let sm = "java/util/SequencedMap";
    r.register(
        sm,
        "firstEntry",
        "()Ljava/util/Map$Entry;",
        native_p64_sm_first_entry,
    );
    r.register(
        sm,
        "lastEntry",
        "()Ljava/util/Map$Entry;",
        native_p64_sm_last_entry,
    );
    r.register(
        sm,
        "pollFirstEntry",
        "()Ljava/util/Map$Entry;",
        native_p64_sm_first_entry,
    );
    r.register(
        sm,
        "pollLastEntry",
        "()Ljava/util/Map$Entry;",
        native_p64_sm_last_entry,
    );
    r.register(
        sm,
        "reversed",
        "()Ljava/util/SequencedMap;",
        |_ctx, args| {
            // Return self for now — full reversed map view is complex
            Ok(Some(args.first().copied().unwrap_or(Value::Object(None))))
        },
    );
    r.register(
        sm,
        "sequencedKeySet",
        "()Ljava/util/SequencedSet;",
        native_p64_sm_seq_key_set,
    );
    r.register(
        sm,
        "sequencedValues",
        "()Ljava/util/SequencedCollection;",
        native_p64_sm_seq_values,
    );
    r.register(
        sm,
        "sequencedEntrySet",
        "()Ljava/util/SequencedSet;",
        native_p64_sm_seq_entry_set,
    );
    // W2: both used to DROP the mapping and return null. That is the worst of
    // the three possible behaviours: the real `SequencedMap` declares them as
    // default methods that THROW `UnsupportedOperationException`, and
    // `LinkedHashMap` overrides them to actually insert — so a caller either
    // learns the map is read-only or gets its entry stored. Silently accepting
    // the call and storing nothing meant a later `get(k)` returned null with no
    // hint of where the value went.
    //
    // Store the mapping through the shared bucket-walking `put`, which is what
    // the only sequenced map in this tree (`LinkedHashMap`) does. `putLast` is
    // then exactly right — a fresh key goes to the end of insertion order.
    // Residual for `putFirst`: the entry is stored but NOT moved to the front,
    // so iteration order can differ from the real JDK. That is a strictly
    // smaller error than losing the entry, and it is visible (the value is
    // there) rather than silent.
    r.register(
        sm,
        "putFirst",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        cratonvm_native_collections::native_map_put_pub,
    );
    r.register(
        sm,
        "putLast",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        cratonvm_native_collections::native_map_put_pub,
    );

    // LinkedHashMap-specific SequencedMap methods
    let lhm = "java/util/LinkedHashMap";
    r.register(
        lhm,
        "firstEntry",
        "()Ljava/util/Map$Entry;",
        native_p64_lhm_first_entry,
    );
    r.register(
        lhm,
        "lastEntry",
        "()Ljava/util/Map$Entry;",
        native_p64_lhm_last_entry,
    );
    r.register(
        lhm,
        "sequencedKeySet",
        "()Ljava/util/SequencedSet;",
        native_p64_lhm_seq_key_set,
    );
    r.register(
        lhm,
        "sequencedValues",
        "()Ljava/util/SequencedCollection;",
        native_p64_lhm_seq_values,
    );
    r.register(
        lhm,
        "sequencedEntrySet",
        "()Ljava/util/SequencedSet;",
        native_p64_lhm_seq_entry_set,
    );
    r.register(
        lhm,
        "reversed",
        "()Ljava/util/SequencedMap;",
        native_p64_lhm_reversed,
    );
    r.set_category(__prev_cat);
}

pub(crate) fn native_p64_seq_get_first(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    // Fallback: for unknown types return null
    Ok(Some(Value::Object(None)))
}

pub(crate) fn native_p64_seq_get_last(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(Some(Value::Object(None)))
}

pub(crate) fn native_p64_seq_reversed(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // Fallback: return self for unknown types
    Ok(Some(args.first().copied().unwrap_or(Value::Object(None))))
}

// --- ArrayList reversed() → new ArrayList with elements in reverse order ---
pub(crate) fn native_p64_al_reversed(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let size = match ctx.get_field(this, 1) {
        Value::Int(v) => v as usize,
        _ => 0,
    };
    let arr = match ctx.get_field(this, 0) {
        Value::Object(Some(a)) => a,
        _ => {
            let new_al = try_alloc_concurrent_synthetic(ctx, "java/util/ArrayList", 2)?;
            let new_arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 0);
            ctx.set_field(new_al, 0, Value::Object(Some(new_arr)));
            ctx.set_field(new_al, 1, Value::Int(0));
            return Ok(Some(Value::Object(Some(new_al))));
        }
    };
    let new_al = try_alloc_concurrent_synthetic(ctx, "java/util/ArrayList", 2)?;
    let new_arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, size);
    for i in 0..size {
        let elem = ctx.get_array_element(arr, size - 1 - i);
        ctx.set_array_element(new_arr, i, elem);
    }
    ctx.set_field(new_al, 0, Value::Object(Some(new_arr)));
    ctx.set_field(new_al, 1, Value::Int(size as i32));
    Ok(Some(Value::Object(Some(new_al))))
}

// --- LinkedList reversed() → new ArrayList with elements in reverse order ---
pub(crate) fn native_p64_ll_reversed(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // Read through the LIST SURFACE, not raw slots.
    //
    // This used to walk the node chain directly, assuming `size` at slot 2,
    // `tail` at slot 1, and each node's element at field 2 with `prev` at
    // field 0. The live `LinkedList` in `cratonvm-native-collections` uses the
    // opposite node layout — element 0, next 1, prev 2 — and keeps head/tail
    // behind name-keyed accessors rather than those slots. So this read a
    // node's `prev` as its element and walked off the chain immediately:
    // `reversed()` returned an EMPTY list for any list built by `add`.
    //
    // `size()`/`get(i)` are layout-independent and work for every List
    // implementation, which is what a default method on `SequencedCollection`
    // should rely on anyway.
    let size = match ctx.invoke_virtual(this, "size", "()I", &[])? {
        Some(Value::Int(v)) if v > 0 => v as usize,
        _ => 0,
    };
    let mut elements = Vec::with_capacity(size);
    for i in (0..size).rev() {
        let elem = ctx
            .invoke_virtual(
                this,
                "get",
                "(I)Ljava/lang/Object;",
                &[Value::Int(i as i32)],
            )?
            .unwrap_or(Value::Object(None));
        elements.push(elem);
    }
    // Build new ArrayList with reversed elements
    let new_al = try_alloc_concurrent_synthetic(ctx, "java/util/ArrayList", 2)?;
    let new_arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, elements.len());
    for (i, elem) in elements.iter().enumerate() {
        ctx.set_array_element(new_arr, i, *elem);
    }
    ctx.set_field(new_al, 0, Value::Object(Some(new_arr)));
    ctx.set_field(new_al, 1, Value::Int(elements.len() as i32));
    Ok(Some(Value::Object(Some(new_al))))
}

pub(crate) fn native_p64_al_get_first(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let size = match ctx.get_field(this, 1) {
        Value::Int(v) => v,
        _ => 0,
    };
    if size == 0 {
        return Err(RuntimeError::IllegalStateException {
            message: "NoSuchElementException".into(),
        }
        .into());
    }
    let arr = match ctx.get_field(this, 0) {
        Value::Object(Some(a)) => a,
        _ => return Ok(Some(Value::Object(None))),
    };
    Ok(Some(ctx.get_array_element(arr, 0)))
}

pub(crate) fn native_p64_al_get_last(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let size = match ctx.get_field(this, 1) {
        Value::Int(v) => v,
        _ => 0,
    };
    if size == 0 {
        return Err(RuntimeError::IllegalStateException {
            message: "NoSuchElementException".into(),
        }
        .into());
    }
    let arr = match ctx.get_field(this, 0) {
        Value::Object(Some(a)) => a,
        _ => return Ok(Some(Value::Object(None))),
    };
    Ok(Some(ctx.get_array_element(arr, (size - 1) as usize)))
}

pub(crate) fn native_p64_ll_get_first(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let size = match ctx.get_field(this, 2) {
        Value::Int(v) => v,
        _ => 0,
    };
    if size == 0 {
        return Err(RuntimeError::IllegalStateException {
            message: "NoSuchElementException".into(),
        }
        .into());
    }
    // Read through the list surface. The raw-slot walk this replaced assumed
    // head at 0 / tail at 1 and the node's element at field 2; the live
    // LinkedList uses element 0, next 1, prev 2 and keeps head/tail behind
    // name-keyed accessors — the same mismatch that made `reversed()` return
    // an empty list. These two are currently shadowed by
    // `cratonvm-native-collections`' own registrations, so the bug was latent
    // rather than observable; fixed so a change in registration order cannot
    // silently surface it.
    let idx = if size > 0 { 0 } else { 0 };
    ctx.invoke_virtual(this, "get", "(I)Ljava/lang/Object;", &[Value::Int(idx)])
}

pub(crate) fn native_p64_ll_get_last(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let size = match ctx.get_field(this, 2) {
        Value::Int(v) => v,
        _ => 0,
    };
    if size == 0 {
        return Err(RuntimeError::IllegalStateException {
            message: "NoSuchElementException".into(),
        }
        .into());
    }
    // Read through the list surface. The raw-slot walk this replaced assumed
    // head at 0 / tail at 1 and the node's element at field 2; the live
    // LinkedList uses element 0, next 1, prev 2 and keeps head/tail behind
    // name-keyed accessors — the same mismatch that made `reversed()` return
    // an empty list. These two are currently shadowed by
    // `cratonvm-native-collections`' own registrations, so the bug was latent
    // rather than observable; fixed so a change in registration order cannot
    // silently surface it.
    let idx = if size > 0 { size - 1 } else { 0 };
    ctx.invoke_virtual(this, "get", "(I)Ljava/lang/Object;", &[Value::Int(idx)])
}

// --- SequencedMap helpers ---

// Helper: create Map$Entry from key + value
pub(crate) fn p64_make_entry(ctx: &mut dyn NativeContext, key: Value, value: Value) -> Result<ObjectRef, MethodCallFailed> {
    // Pin across the entry alloc below — a moving young GC there would
    // relocate the key/value (native stale-local family).
    let key_pin = pinned_object_value(ctx, key);
    let value_pin = pinned_object_value(ctx, value);
    let entry = try_alloc_concurrent_synthetic(ctx, "java/util/HashMap$Entry", 2)?;
    let key = read_pinned_object_value(ctx, key_pin, key);
    let value = read_pinned_object_value(ctx, value_pin, value);
    ctx.set_field(entry, 0, key);
    ctx.set_field(entry, 1, value);
    if let Some((h, _)) = key_pin.or(value_pin) {
        ctx.unpin_native_roots(h);
    }
    Ok(entry)
}

// SequencedMap interface fallbacks (return null for non-LinkedHashMap)
pub(crate) fn native_p64_sm_first_entry(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(Some(Value::Object(None)))
}

pub(crate) fn native_p64_sm_last_entry(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(Some(Value::Object(None)))
}

pub(crate) fn native_p64_sm_seq_key_set(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(Some(Value::Object(None)))
}

pub(crate) fn native_p64_sm_seq_values(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(Some(Value::Object(None)))
}

pub(crate) fn native_p64_sm_seq_entry_set(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(Some(Value::Object(None)))
}

// --- LinkedHashMap SequencedMap methods ---
// LHM layout: buckets=0, size=1, capacity=2, head=3, tail=4
// LHM node: key=0, value=1, hash=2, next=3, before=4, after=5

/// First or last entry of a `SequencedMap`, read through the PUBLIC surface.
///
/// The `firstEntry`/`lastEntry` accessors used to read raw slots — head at 3,
/// tail at 4, and each node's key/value at fields 0 and 1. The live
/// `LinkedHashMap` in `cratonvm-native-collections` matches none of that: its
/// nodes are `hash, key, value` (so field 0 is the HASH, not the key) and its
/// head/tail live in a name-keyed overlay rather than those slots. Both
/// accessors therefore returned null for every map built through `put`.
///
/// Iterating `entrySet()` is layout-independent, preserves the map's
/// insertion order, and returns the map's own `Map.Entry` objects.
fn p64_seq_map_edge_entry(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    want_last: bool,
) -> MethodCallResult {
    let entries = match ctx.invoke_virtual(this, "entrySet", "()Ljava/util/Set;", &[])? {
        Some(Value::Object(Some(s))) => s,
        _ => return Ok(Some(Value::Object(None))),
    };
    let it = match ctx.invoke_virtual(entries, "iterator", "()Ljava/util/Iterator;", &[])? {
        Some(Value::Object(Some(i))) => i,
        _ => return Ok(Some(Value::Object(None))),
    };
    let mut found = Value::Object(None);
    loop {
        match ctx.invoke_virtual(it, "hasNext", "()Z", &[])? {
            Some(Value::Int(1)) => {}
            _ => break,
        }
        let next = ctx
            .invoke_virtual(it, "next", "()Ljava/lang/Object;", &[])?
            .unwrap_or(Value::Object(None));
        if matches!(next, Value::Object(None)) {
            break;
        }
        found = next;
        if !want_last {
            break;
        }
    }
    Ok(Some(found))
}

pub(crate) fn native_p64_lhm_first_entry(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    p64_seq_map_edge_entry(ctx, this, false)
}

pub(crate) fn native_p64_lhm_last_entry(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    p64_seq_map_edge_entry(ctx, this, true)
}

pub(crate) fn native_p64_lhm_seq_key_set(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // Walk insertion order: head → ... → tail via field 5 (after)
    let mut keys = Vec::new();
    let mut cur = ctx.get_field(this, 3); // head
    while let Value::Object(Some(node)) = cur {
        keys.push(ctx.get_field(node, 0)); // key
        cur = ctx.get_field(node, 5); // after
    }
    // Return as ArrayList (simplification — real Java returns a Set view)
    let al = try_alloc_concurrent_synthetic(ctx, "java/util/ArrayList", 2)?;
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, keys.len());
    for (i, k) in keys.iter().enumerate() {
        ctx.set_array_element(arr, i, *k);
    }
    ctx.set_field(al, 0, Value::Object(Some(arr)));
    ctx.set_field(al, 1, Value::Int(keys.len() as i32));
    Ok(Some(Value::Object(Some(al))))
}

pub(crate) fn native_p64_lhm_seq_values(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let mut vals = Vec::new();
    let mut cur = ctx.get_field(this, 3);
    while let Value::Object(Some(node)) = cur {
        vals.push(ctx.get_field(node, 1)); // value
        cur = ctx.get_field(node, 5);
    }
    let al = try_alloc_concurrent_synthetic(ctx, "java/util/ArrayList", 2)?;
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, vals.len());
    for (i, v) in vals.iter().enumerate() {
        ctx.set_array_element(arr, i, *v);
    }
    ctx.set_field(al, 0, Value::Object(Some(arr)));
    ctx.set_field(al, 1, Value::Int(vals.len() as i32));
    Ok(Some(Value::Object(Some(al))))
}

pub(crate) fn native_p64_lhm_seq_entry_set(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let mut entries = Vec::new();
    let mut cur = ctx.get_field(this, 3);
    while let Value::Object(Some(node)) = cur {
        let key = ctx.get_field(node, 0);
        let val = ctx.get_field(node, 1);
        entries.push(p64_make_entry(ctx, key, val)?);
        cur = ctx.get_field(node, 5);
    }
    let al = try_alloc_concurrent_synthetic(ctx, "java/util/ArrayList", 2)?;
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, entries.len());
    for (i, e) in entries.iter().enumerate() {
        ctx.set_array_element(arr, i, Value::Object(Some(*e)));
    }
    ctx.set_field(al, 0, Value::Object(Some(arr)));
    ctx.set_field(al, 1, Value::Int(entries.len() as i32));
    Ok(Some(Value::Object(Some(al))))
}

pub(crate) fn native_p64_lhm_reversed(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // Collect entries in reverse insertion order (tail → head via before=4)
    let mut entries = Vec::new();
    let mut cur = ctx.get_field(this, 4); // tail
    while let Value::Object(Some(node)) = cur {
        let key = ctx.get_field(node, 0);
        let val = ctx.get_field(node, 1);
        entries.push((key, val));
        cur = ctx.get_field(node, 4); // before
    }
    // Build new LinkedHashMap with reversed insertion order
    // Use native put to insert each entry
    let new_lhm = try_alloc_concurrent_synthetic(ctx, "java/util/LinkedHashMap", 5)?;
    let init_cap = 16i32;
    let buckets = ctx.new_array(
        cratonvm_types::ArrayElementType::Reference,
        init_cap as usize,
    );
    ctx.set_field(new_lhm, 0, Value::Object(Some(buckets)));
    ctx.set_field(new_lhm, 1, Value::Int(0));
    ctx.set_field(new_lhm, 2, Value::Int(init_cap));
    ctx.set_field(new_lhm, 3, Value::Object(None)); // head
    ctx.set_field(new_lhm, 4, Value::Object(None)); // tail
                                                    // Insert each entry via invoke_virtual
    for (key, val) in &entries {
        let _ = ctx.invoke_virtual(
            new_lhm,
            "put",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            &[Value::Object(Some(new_lhm)), *key, *val],
        );
    }
    Ok(Some(Value::Object(Some(new_lhm))))
}

// =============================================================================
// Collections checked wrappers — delegate to underlying collection
// =============================================================================

pub(crate) fn register_p65_checked_collections(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let cols = "java/util/Collections";
    // checkedList — return the list itself (simplified, no runtime type checking)
    r.register(
        cols,
        "checkedList",
        "(Ljava/util/List;Ljava/lang/Class;)Ljava/util/List;",
        |_ctx, args| Ok(Some(args.first().copied().unwrap_or(Value::Object(None)))),
    );
    r.register(
        cols,
        "checkedSet",
        "(Ljava/util/Set;Ljava/lang/Class;)Ljava/util/Set;",
        |_ctx, args| Ok(Some(args.first().copied().unwrap_or(Value::Object(None)))),
    );
    r.register(
        cols,
        "checkedMap",
        "(Ljava/util/Map;Ljava/lang/Class;Ljava/lang/Class;)Ljava/util/Map;",
        |_ctx, args| Ok(Some(args.first().copied().unwrap_or(Value::Object(None)))),
    );
    r.register(
        cols,
        "checkedCollection",
        "(Ljava/util/Collection;Ljava/lang/Class;)Ljava/util/Collection;",
        |_ctx, args| Ok(Some(args.first().copied().unwrap_or(Value::Object(None)))),
    );
    r.register(
        cols,
        "checkedSortedSet",
        "(Ljava/util/SortedSet;Ljava/lang/Class;)Ljava/util/SortedSet;",
        |_ctx, args| Ok(Some(args.first().copied().unwrap_or(Value::Object(None)))),
    );
    r.register(
        cols,
        "checkedSortedMap",
        "(Ljava/util/SortedMap;Ljava/lang/Class;Ljava/lang/Class;)Ljava/util/SortedMap;",
        |_ctx, args| Ok(Some(args.first().copied().unwrap_or(Value::Object(None)))),
    );
    r.register(
        cols,
        "checkedNavigableSet",
        "(Ljava/util/NavigableSet;Ljava/lang/Class;)Ljava/util/NavigableSet;",
        |_ctx, args| Ok(Some(args.first().copied().unwrap_or(Value::Object(None)))),
    );
    r.register(
        cols,
        "checkedNavigableMap",
        "(Ljava/util/NavigableMap;Ljava/lang/Class;Ljava/lang/Class;)Ljava/util/NavigableMap;",
        |_ctx, args| Ok(Some(args.first().copied().unwrap_or(Value::Object(None)))),
    );
    // unmodifiableSequencedCollection — Java 21
    r.register(
        cols,
        "unmodifiableSequencedCollection",
        "(Ljava/util/SequencedCollection;)Ljava/util/SequencedCollection;",
        |_ctx, args| Ok(Some(args.first().copied().unwrap_or(Value::Object(None)))),
    );
    r.register(
        cols,
        "unmodifiableSequencedSet",
        "(Ljava/util/SequencedSet;)Ljava/util/SequencedSet;",
        |_ctx, args| Ok(Some(args.first().copied().unwrap_or(Value::Object(None)))),
    );
    r.register(
        cols,
        "unmodifiableSequencedMap",
        "(Ljava/util/SequencedMap;)Ljava/util/SequencedMap;",
        |_ctx, args| Ok(Some(args.first().copied().unwrap_or(Value::Object(None)))),
    );
    r.set_category(__prev_cat);
}

// =============================================================================
// Misc: java.util.EnumMap, java.util.EnumSet, java.lang.Iterable additions
// =============================================================================

/// `Ordering` → the -1/0/1 an `int compareTo` must return (W2).
fn ordering_to_int(o: std::cmp::Ordering) -> i32 {
    match o {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    }
}

/// Unbox a BOXED PRIMITIVE to an `f64` for the `java.lang.Comparable.compareTo`
/// fallback (W2).
///
/// Gated on the wrapper CLASS NAME, deliberately: the sibling
/// `natural_compare` in native-collections documents how a raw slot-0 probe
/// mis-fires on any POJO whose first declared field happens to be a primitive
/// (two unsaved JPA entities both read `id == 0` and compared "equal", which
/// silently dropped an element from a natural-order TreeSet). Only the eight
/// wrapper classes may be unboxed here.
fn comparable_boxed_number(ctx: &dyn NativeContext, obj: ObjectRef) -> Option<f64> {
    let name = ctx.class_name_of_id(ctx.class_id_of_object(obj))?;
    match name.as_str() {
        "java/lang/Integer"
        | "java/lang/Long"
        | "java/lang/Short"
        | "java/lang/Byte"
        | "java/lang/Character"
        | "java/lang/Boolean"
        | "java/lang/Float"
        | "java/lang/Double" => {}
        _ => return None,
    }
    if ctx.object_num_fields(obj) == 0 {
        return None;
    }
    match ctx.get_field(obj, 0) {
        Value::Int(v) => Some(v as f64),
        Value::Long(v) => Some(v as f64),
        Value::Float(v) => Some(v as f64),
        Value::Double(v) => Some(v),
        _ => None,
    }
}

pub(crate) fn register_p70_misc(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    // EnumMap and EnumSet core methods already registered in earlier phases — only add NEW methods here

    // EnumSet: complementOf and range (not in earlier phases)
    let es = "java/util/EnumSet";
    r.register(
        es,
        "complementOf",
        "(Ljava/util/EnumSet;)Ljava/util/EnumSet;",
        |ctx, _args| {
            // Use same layout as existing EnumSet: field 0 = ArrayList backing, field 1 = type
            let set = try_alloc_concurrent_synthetic(ctx, "java/util/EnumSet", 2)?;
            // Pin across the array/backing allocs below — a moving young GC
            // there would relocate them (native stale-local family).
            let set_pin = ctx.pin_native_root(set);
            let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 0);
            let arr_pin = ctx.pin_native_root(arr);
            let backing = try_alloc_concurrent_synthetic(ctx, "java/util/ArrayList", 2)?;
            let set = ctx.read_native_pin(set_pin, set);
            let arr = ctx.read_native_pin(arr_pin, arr);
            ctx.set_field(backing, 0, Value::Object(Some(arr)));
            ctx.set_field(backing, 1, Value::Int(0));
            ctx.set_field(set, 0, Value::Object(Some(backing)));
            ctx.set_field(set, 1, Value::Object(None));
            ctx.unpin_native_roots(set_pin);
            Ok(Some(Value::Object(Some(set))))
        },
    );
    r.register(
        es,
        "range",
        "(Ljava/lang/Enum;Ljava/lang/Enum;)Ljava/util/EnumSet;",
        crate::phases_early::native_es_range,
    );

    // java.io.Serializable — marker interface (no methods, but sometimes referenced)
    //
    // KEEP. `java.io.Serializable` genuinely declares no members, so this key
    // is only ever reached by a static-field lookup that walked all the way up
    // to the interface — i.e. by a class with no explicit
    // `static final long serialVersionUID` — and 0 is the correct "not
    // declared" answer there. The serialization machinery does not go through
    // this native at all: `serialization.rs`'s `ObjectStreamClass` builder
    // resolves the field with `static_field_index_by_name` on the CONCRETE
    // class and falls back to `compute_default_svuid`, so no stream ever
    // carries this 0.
    r.register(
        "java/io/Serializable",
        "serialVersionUID",
        "J",
        |_ctx, _args| Ok(Some(Value::Long(0))),
    );

    // java.lang.Comparable — compareTo for String already exists; add for wrappers
    //
    // W2: this used to answer a constant 0, i.e. "every object compares equal".
    // `compareTo` is ABSTRACT, so the interface-native guard does not apply to
    // it (an abstract method takes the `check_override` branch and is cached by
    // RECEIVER class), which means any receiver whose class declares no
    // `compareTo` landed here — and a comparison that always says "equal"
    // silently collapses a sort into a no-op and makes a TreeMap/TreeSet
    // dedup-drop everything after the first element. That is precisely the B2
    // failure `natural_compare` in native-collections was fixed for.
    //
    // Decide the cases we can decide (String content, then the boxed
    // primitives this registration was added for), and raise ClassCastException
    // for the rest rather than inventing an ordering. The JDK's own
    // natural-order paths throw ClassCastException when they cannot order two
    // values, so callers already handle it.
    r.register(
        "java/lang/Comparable",
        "compareTo",
        "(Ljava/lang/Object;)I",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let other = match args.get(1) {
                Some(Value::Object(Some(o))) => *o,
                // `x.compareTo(null)` throws NPE on every JDK Comparable.
                _ => {
                    return Err(RuntimeError::NullPointerException {
                        message: Some("Comparable.compareTo: null argument".to_string()),
                    }
                    .into())
                }
            };
            if this == other {
                return Ok(Some(Value::Int(0)));
            }
            if let (Some(a), Some(b)) = (ctx.read_string(this), ctx.read_string(other)) {
                return Ok(Some(Value::Int(ordering_to_int(a.cmp(&b)))));
            }
            if let (Some(a), Some(b)) = (
                comparable_boxed_number(ctx, this),
                comparable_boxed_number(ctx, other),
            ) {
                // `Double.compare` semantics, not IEEE totalOrder — the two
                // disagree on a negatively-signed or payload-carrying NaN. See
                // `cratonvm_types::jfp`. (`comparable_boxed_number` widens
                // every boxed number to `f64`, so the integral receivers that
                // dominate this path are unaffected either way.)
                return Ok(Some(Value::Int(cratonvm_types::jfp::double_compare(a, b))));
            }
            Err(RuntimeError::ClassCastException {
                message:
                    "Comparable.compareTo: receiver declares no compareTo and is neither a String \
                     nor a boxed primitive"
                        .to_string(),
            }
            .into())
        },
    );

    // STUB-REMOVAL (wave 3) — DELETED: `java/lang/AutoCloseable.close()V` -> no-op.
    //
    // Proof it was dead in BOTH modes: `native-io`'s `register_scanner_natives`
    // (native-io/src/lib.rs ~6061) registers the SAME triple, pointing at
    // `native_scanner_close`, and `register_io_natives` always runs AFTER
    // `register_builtins` (vm_init.rs 1252-1254 / 1523 / 1953, and the two test
    // helpers in vm.rs) — last registration wins, so this no-op never served a
    // single call. Removing it changes no behaviour and stops the next sweep
    // re-litigating a registration that cannot fire.
    //
    // NOTE for the owner of native-io: the surviving winner is Scanner-specific.
    // `native_scanner_close` writes `Int(1)` into field index 4 of WHATEVER
    // receiver reaches `AutoCloseable.close()` / `Closeable.close()`, which is
    // only meaningful for a synthetic Scanner. That is a separate bug and is
    // reported upstream rather than papered over here.
    r.set_category(__prev_cat);
}
