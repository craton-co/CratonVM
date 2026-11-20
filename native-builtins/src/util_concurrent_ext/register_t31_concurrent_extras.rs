#[allow(unused_imports)]
use super::*;

/// ConcurrentSkipListMap — backed by a sorted array of key-value pairs.
/// Fields: 0=keys_array, 1=values_array, 2=size
/// Uses binary search on keys (via compareTo) for NavigableMap operations.
pub fn register_t31_concurrent_extras(registry: &mut NativeMethodRegistry) {
    let cslm = "java/util/concurrent/ConcurrentSkipListMap";

    registry.register(cslm, "<init>", "()V", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(None),
        };
        // `this` and the first array both predate the second allocation.
        let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
        let this_h = scope.root(this);
        let keys_obj = scope.new_array(cratonvm_types::ArrayElementType::Reference, 16);
        let keys_h = scope.root(keys_obj);
        let vals = scope.new_array(cratonvm_types::ArrayElementType::Reference, 16);
        let this = scope.get(&this_h);
        let keys = scope.get(&keys_h);
        scope.set_field(this, 0, Value::Object(Some(keys)));
        scope.set_field(this, 1, Value::Object(Some(vals)));
        scope.set_field(this, 2, Value::Int(0));
        Ok(None)
    }, NativeKind::SyntheticStub);

    // put(K, V) -> V — sorted insert
    registry.register(
        cslm,
        "put",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            // `compareTo` below runs arbitrary Java on every probe of the
            // search loop, and the grow arm allocates twice. The receiver, both
            // arrays and both arguments cross all of that, so the whole body
            // works through the scope and re-reads each address after anything
            // that can collect.
            let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
            let this_h = scope.root(this);
            let key_h = match args.get(1) {
                Some(Value::Object(Some(o))) => Some(scope.root(*o)),
                _ => None,
            };
            let val_h = match args.get(2) {
                Some(Value::Object(Some(o))) => Some(scope.root(*o)),
                _ => None,
            };
            let key_plain = args.get(1).copied().unwrap_or(Value::Object(None));
            let val_plain = args.get(2).copied().unwrap_or(Value::Object(None));
            let key = |scope: &cratonvm_native_api::NativeHandleScope| match &key_h {
                Some(h) => Value::Object(Some(scope.get(h))),
                None => key_plain,
            };
            let val = |scope: &cratonvm_native_api::NativeHandleScope| match &val_h {
                Some(h) => Value::Object(Some(scope.get(h))),
                None => val_plain,
            };
            scope.monitor_enter(this);
            let size = match scope.get_field(this, 2) {
                Value::Int(n) => n as usize,
                _ => 0,
            };
            let keys_src_h = match scope.get_field(this, 0) {
                Value::Object(Some(a)) => scope.root(a),
                _ => {
                    scope.monitor_exit(this);
                    return Ok(Some(Value::Object(None)));
                }
            };
            let vals_src_h = match scope.get_field(this, 1) {
                Value::Object(Some(a)) => scope.root(a),
                _ => {
                    scope.monitor_exit(this);
                    return Ok(Some(Value::Object(None)));
                }
            };

            // Find insertion point via linear scan (compareTo)
            let mut pos = size;
            for i in 0..size {
                let keys_arr = scope.get(&keys_src_h);
                let existing_key = scope.get_array_element(keys_arr, i);
                if let Value::Object(Some(ek)) = existing_key {
                    let probe = key(&scope);
                    if let Ok(Some(Value::Int(cmp))) =
                        scope.invoke_virtual(ek, "compareTo", "(Ljava/lang/Object;)I", &[probe])
                    {
                        if cmp == 0 {
                            // Key exists — replace value
                            let vals_arr = scope.get(&vals_src_h);
                            let old = scope.get_array_element(vals_arr, i);
                            let v = val(&scope);
                            scope.set_array_element(vals_arr, i, v);
                            let this = scope.get(&this_h);
                            scope.monitor_exit(this);
                            return Ok(Some(old));
                        } else if cmp > 0 {
                            pos = i;
                            break;
                        }
                    }
                }
            }

            // Grow if needed
            let keys_arr = scope.get(&keys_src_h);
            let cap = scope.array_length(keys_arr);
            if size >= cap {
                let new_cap = cap * 2;
                let new_keys_obj =
                    scope.new_array(cratonvm_types::ArrayElementType::Reference, new_cap);
                let new_keys_h = scope.root(new_keys_obj);
                // The last GC point of this arm: `new_vals` is used as returned.
                let new_vals: ObjectRef =
                    scope.new_array(cratonvm_types::ArrayElementType::Reference, new_cap);
                let new_keys = scope.get(&new_keys_h);
                let keys_arr = scope.get(&keys_src_h);
                let vals_arr = scope.get(&vals_src_h);
                let this = scope.get(&this_h);
                let key = key(&scope);
                let val = val(&scope);
                for i in 0..size {
                    let k = scope.get_array_element(keys_arr, i);
                    scope.set_array_element(new_keys, i, k);
                    let v = scope.get_array_element(vals_arr, i);
                    scope.set_array_element(new_vals, i, v);
                }
                scope.set_field(this, 0, Value::Object(Some(new_keys)));
                scope.set_field(this, 1, Value::Object(Some(new_vals)));
                // Re-fetch after resize
                let keys_arr = new_keys;
                let vals_arr = new_vals;
                // Shift right from pos
                for i in (pos..size).rev() {
                    let k = scope.get_array_element(keys_arr, i);
                    scope.set_array_element(keys_arr, i + 1, k);
                    let v = scope.get_array_element(vals_arr, i);
                    scope.set_array_element(vals_arr, i + 1, v);
                }
                scope.set_array_element(keys_arr, pos, key);
                scope.set_array_element(vals_arr, pos, val);
            } else {
                // Shift right from pos
                let keys_arr = scope.get(&keys_src_h);
                let vals_arr = scope.get(&vals_src_h);
                for i in (pos..size).rev() {
                    let k = scope.get_array_element(keys_arr, i);
                    scope.set_array_element(keys_arr, i + 1, k);
                    let v = scope.get_array_element(vals_arr, i);
                    scope.set_array_element(vals_arr, i + 1, v);
                }
                let k = key(&scope);
                let v = val(&scope);
                scope.set_array_element(keys_arr, pos, k);
                scope.set_array_element(vals_arr, pos, v);
            }
            let this = scope.get(&this_h);
            scope.set_field(this, 2, Value::Int((size + 1) as i32));
            scope.monitor_exit(this);
            Ok(Some(Value::Object(None)))
        }, NativeKind::SyntheticStub,
    );

    // get(Object) -> V
    registry.register(
        cslm,
        "get",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            let key = args.get(1).copied().unwrap_or(Value::Object(None));
            let size = match ctx.get_field(this, 2) {
                Value::Int(n) => n as usize,
                _ => 0,
            };
            let keys_arr = match ctx.get_field(this, 0) {
                Value::Object(Some(a)) => a,
                _ => return Ok(Some(Value::Object(None))),
            };
            let vals_arr = match ctx.get_field(this, 1) {
                Value::Object(Some(a)) => a,
                _ => return Ok(Some(Value::Object(None))),
            };
            // gc-common w13-f: every probe runs `compareTo` (Java). Both
            // arrays and the key were read before the first probe and used
            // after it; they are rooted and re-read per probe.
            let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
            let keys_h = scope.root(keys_arr);
            let vals_h = scope.root(vals_arr);
            let key_h = cslm_root_key(&mut scope, key);
            for i in 0..size {
                let keys_now = scope.get(&keys_h);
                if let Value::Object(Some(ek)) = scope.get_array_element(keys_now, i) {
                    let probe = cslm_key_now(&scope, key_h.as_ref(), key);
                    if let Ok(Some(Value::Int(cmp))) =
                        scope.invoke_virtual(ek, "compareTo", "(Ljava/lang/Object;)I", &[probe])
                    {
                        if cmp == 0 {
                            let vals_now = scope.get(&vals_h);
                            return Ok(Some(scope.get_array_element(vals_now, i)));
                        }
                        if cmp > 0 {
                            break;
                        }
                    }
                }
            }
            Ok(Some(Value::Object(None)))
        }, NativeKind::SyntheticStub,
    );

    // size() -> int
    registry.register(cslm, "size", "()I", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Int(0))),
        };
        Ok(Some(ctx.get_field(this, 2)))
    }, NativeKind::SyntheticStub);

    // isEmpty() -> boolean
    registry.register(cslm, "isEmpty", "()Z", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Int(1))),
        };
        let size = match ctx.get_field(this, 2) {
            Value::Int(n) => n,
            _ => 0,
        };
        Ok(Some(Value::Int(if size == 0 { 1 } else { 0 })))    }, NativeKind::SyntheticStub);

    // containsKey(Object) -> boolean
    registry.register(cslm, "containsKey", "(Ljava/lang/Object;)Z", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Int(0))),
        };
        let key = args.get(1).copied().unwrap_or(Value::Object(None));
        let size = match ctx.get_field(this, 2) {
            Value::Int(n) => n as usize,
            _ => 0,
        };
        let keys_arr = match ctx.get_field(this, 0) {
            Value::Object(Some(a)) => a,
            _ => return Ok(Some(Value::Int(0))),
        };
        // gc-common w13-f: rooted across each probe's `compareTo`, as in `get`.
        let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
        let keys_h = scope.root(keys_arr);
        let key_h = cslm_root_key(&mut scope, key);
        for i in 0..size {
            let keys_now = scope.get(&keys_h);
            if let Value::Object(Some(ek)) = scope.get_array_element(keys_now, i) {
                let probe = cslm_key_now(&scope, key_h.as_ref(), key);
                if let Ok(Some(Value::Int(0))) =
                    scope.invoke_virtual(ek, "compareTo", "(Ljava/lang/Object;)I", &[probe])
                {
                    return Ok(Some(Value::Int(1)));
                }
            }
        }
        Ok(Some(Value::Int(0)))
    }, NativeKind::SyntheticStub);

    // remove(Object) -> V
    registry.register(
        cslm,
        "remove",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            let key = args.get(1).copied().unwrap_or(Value::Object(None));
            // gc-common w13-f: every probe runs `compareTo` (Java), after
            // which the matching arm shifted both arrays and wrote the size
            // through the addresses read before the first probe. The
            // receiver, both arrays and the key are rooted and re-read after
            // each probe, the way `put` above already does it.
            let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
            let this_h = scope.root(this);
            let key_h = cslm_root_key(&mut scope, key);
            scope.monitor_enter(this);
            let size = match scope.get_field(this, 2) {
                Value::Int(n) => n as usize,
                _ => 0,
            };
            let keys_h = match scope.get_field(this, 0) {
                Value::Object(Some(a)) => scope.root(a),
                _ => {
                    scope.monitor_exit(this);
                    return Ok(Some(Value::Object(None)));
                }
            };
            let vals_h = match scope.get_field(this, 1) {
                Value::Object(Some(a)) => scope.root(a),
                _ => {
                    scope.monitor_exit(this);
                    return Ok(Some(Value::Object(None)));
                }
            };
            for i in 0..size {
                let keys_now = scope.get(&keys_h);
                if let Value::Object(Some(ek)) = scope.get_array_element(keys_now, i) {
                    let probe = cslm_key_now(&scope, key_h.as_ref(), key);
                    if let Ok(Some(Value::Int(0))) =
                        scope.invoke_virtual(ek, "compareTo", "(Ljava/lang/Object;)I", &[probe])
                    {
                        // Nothing below can collect.
                        let keys_arr = scope.get(&keys_h);
                        let vals_arr = scope.get(&vals_h);
                        let old = scope.get_array_element(vals_arr, i);
                        // Shift left
                        for j in i..size - 1 {
                            let k = scope.get_array_element(keys_arr, j + 1);
                            scope.set_array_element(keys_arr, j, k);
                            let v = scope.get_array_element(vals_arr, j + 1);
                            scope.set_array_element(vals_arr, j, v);
                        }
                        scope.set_array_element(keys_arr, size - 1, Value::Object(None));
                        scope.set_array_element(vals_arr, size - 1, Value::Object(None));
                        let this = scope.get(&this_h);
                        scope.set_field(this, 2, Value::Int((size - 1) as i32));
                        scope.monitor_exit(this);
                        return Ok(Some(old));
                    }
                }
            }
            let this = scope.get(&this_h);
            scope.monitor_exit(this);
            Ok(Some(Value::Object(None)))
        }, NativeKind::SyntheticStub,
    );

    // firstKey() -> K
    registry.register(cslm, "firstKey", "()Ljava/lang/Object;", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Object(None))),
        };
        let size = match ctx.get_field(this, 2) {
            Value::Int(n) => n,
            _ => 0,
        };
        if size == 0 {
            return Err(RuntimeError::IllegalStateException {
                message: "NoSuchElementException".to_string(),
            }
            .into());
        }
        let keys_arr = match ctx.get_field(this, 0) {
            Value::Object(Some(a)) => a,
            _ => return Ok(Some(Value::Object(None))),
        };
        Ok(Some(ctx.get_array_element(keys_arr, 0)))    }, NativeKind::SyntheticStub);

    // lastKey() -> K
    registry.register(cslm, "lastKey", "()Ljava/lang/Object;", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Object(None))),
        };
        let size = match ctx.get_field(this, 2) {
            Value::Int(n) => n as usize,
            _ => 0,
        };
        if size == 0 {
            return Err(RuntimeError::IllegalStateException {
                message: "NoSuchElementException".to_string(),
            }
            .into());
        }
        let keys_arr = match ctx.get_field(this, 0) {
            Value::Object(Some(a)) => a,
            _ => return Ok(Some(Value::Object(None))),
        };
        Ok(Some(ctx.get_array_element(keys_arr, size - 1)))    }, NativeKind::SyntheticStub);

    // subMap(fromKey, toKey) -> SortedMap (returns self-type as simplified view)
    // For a real JVM this would return a view; we return a new ConcurrentSkipListMap with the subrange
    registry.register(
        cslm,
        "subMap",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/util/concurrent/ConcurrentNavigableMap;",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            let from = args.get(1).copied().unwrap_or(Value::Object(None));
            let to = args.get(2).copied().unwrap_or(Value::Object(None));
            cslm_subrange(ctx, this, Some(from), Some(to))
        }, NativeKind::SyntheticStub,
    );

    // headMap(toKey) -> SortedMap
    registry.register(
        cslm,
        "headMap",
        "(Ljava/lang/Object;)Ljava/util/concurrent/ConcurrentNavigableMap;",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            let to = args.get(1).copied().unwrap_or(Value::Object(None));
            cslm_subrange(ctx, this, None, Some(to))
        }, NativeKind::SyntheticStub,
    );

    // tailMap(fromKey) -> SortedMap
    registry.register(
        cslm,
        "tailMap",
        "(Ljava/lang/Object;)Ljava/util/concurrent/ConcurrentNavigableMap;",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            let from = args.get(1).copied().unwrap_or(Value::Object(None));
            cslm_subrange(ctx, this, Some(from), None)
        }, NativeKind::SyntheticStub,
    );

    // --- LinkedTransferQueue ---
    // Backed by same array structure as LinkedBlockingQueue: 0=array, 1=size, 2=capacity.
    // Real JDK 25 LTQ has a totally different layout (head/tail Node refs etc.) so the
    // synthetic <init> would leave real fields null and break later real bytecode.
    // Gate behind synthetic-jdk so real-JDK boot uses the real constructor.
    #[cfg(feature = "synthetic-jdk")]
    {
        let ltq = "java/util/concurrent/LinkedTransferQueue";
        registry.register(ltq, "<init>", "()V", |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(None),
            };
            let this = uce_store_fresh_ref_array(ctx, this, 16);
            ctx.set_field(this, 1, Value::Int(0));
            ctx.set_field(this, 2, Value::Int(i32::MAX)); // unbounded
            Ok(None)
        }, NativeKind::SyntheticStub);
        // add/offer/put — same as LBQ
        registry.register(ltq, "add", "(Ljava/lang/Object;)Z", |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Int(0))),
            };
            let elem = args.get(1).copied().unwrap_or(Value::Object(None));
            ctx.monitor_enter(this);
            let this = m18_lbq_add_internal(ctx, this, elem);
            ctx.monitor_notify_all(this)?;
            ctx.monitor_exit(this);
            Ok(Some(Value::Int(1)))
        }, NativeKind::SyntheticStub);
        registry.register(ltq, "offer", "(Ljava/lang/Object;)Z", |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Int(0))),
            };
            let elem = args.get(1).copied().unwrap_or(Value::Object(None));
            ctx.monitor_enter(this);
            let this = m18_lbq_add_internal(ctx, this, elem);
            ctx.monitor_notify_all(this)?;
            ctx.monitor_exit(this);
            Ok(Some(Value::Int(1)))
        }, NativeKind::SyntheticStub);
        registry.register(ltq, "put", "(Ljava/lang/Object;)V", |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(None),
            };
            let elem = args.get(1).copied().unwrap_or(Value::Object(None));
            ctx.monitor_enter(this);
            let this = m18_lbq_add_internal(ctx, this, elem);
            ctx.monitor_notify_all(this)?;
            ctx.monitor_exit(this);
            Ok(None)
        }, NativeKind::SyntheticStub);
        // transfer(E) — blocking: adds element and waits until it is consumed
        registry.register(ltq, "transfer", "(Ljava/lang/Object;)V", |ctx, args| {
            let mut this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(None),
            };
            let elem = args.get(1).copied().unwrap_or(Value::Object(None));
            ctx.monitor_enter(this);
            this = m18_lbq_add_internal(ctx, this, elem);
            ctx.monitor_notify_all(this)?;
            // Wait until the item is consumed (size decreases)
            let size_before = match ctx.get_field(this, 1) {
                Value::Int(n) => n,
                _ => 0,
            };
            ctx.monitor_exit(this);
            // GC-safety: parks on the monitor until a consumer takes the
            // element, and dereferences `this` on the next turn.
            let this_pin = ctx.pin_native_root(this);
            loop {
                let mut this = ctx.read_native_pin(this_pin, this);
                let size_now = match ctx.get_field(this, 1) {
                    Value::Int(n) => n,
                    _ => 0,
                };
                if size_now < size_before {
                    return Ok(None);
                }
                ctx.monitor_enter(this);
                monitor_wait_release(ctx, &mut this, Some(5))?;
            }
        }, NativeKind::SyntheticStub);
        // tryTransfer(E) — non-blocking: add if there's a waiting consumer
        registry.register(ltq, "tryTransfer", "(Ljava/lang/Object;)Z", |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Int(0))),
            };
            let elem = args.get(1).copied().unwrap_or(Value::Object(None));
            ctx.monitor_enter(this);
            let this = m18_lbq_add_internal(ctx, this, elem);
            ctx.monitor_notify_all(this)?;
            ctx.monitor_exit(this);
            Ok(Some(Value::Int(1)))
        }, NativeKind::SyntheticStub);
        // tryTransfer(E, long, TimeUnit) — timed
        registry.register(
            ltq,
            "tryTransfer",
            "(Ljava/lang/Object;JLjava/util/concurrent/TimeUnit;)Z",
            |ctx, args| {
                let this = match args.first() {
                    Some(Value::Object(Some(o))) => *o,
                    _ => return Ok(Some(Value::Int(0))),
                };
                let elem = args.get(1).copied().unwrap_or(Value::Object(None));
                ctx.monitor_enter(this);
                let this = m18_lbq_add_internal(ctx, this, elem);
                ctx.monitor_notify_all(this)?;
                ctx.monitor_exit(this);
                Ok(Some(Value::Int(1)))
            }, NativeKind::SyntheticStub,
        );
        // poll() — non-blocking
        registry.register(ltq, "poll", "()Ljava/lang/Object;", |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            ctx.monitor_enter(this);
            let size = match ctx.get_field(this, 1) {
                Value::Int(n) => n,
                _ => 0,
            };
            let result = if size > 0 {
                let r = m18_lbq_remove_head(ctx, this);
                ctx.monitor_notify_all(this)?;
                r
            } else {
                Value::Object(None)
            };
            ctx.monitor_exit(this);
            Ok(Some(result))
        }, NativeKind::SyntheticStub);
        // poll(long, TimeUnit) — timed blocking
        registry.register(
            ltq,
            "poll",
            "(JLjava/util/concurrent/TimeUnit;)Ljava/lang/Object;",
            |ctx, args| {
                let mut this = match args.first() {
                    Some(Value::Object(Some(o))) => *o,
                    _ => return Ok(Some(Value::Object(None))),
                };
                let timeout_val = match args.get(1) {
                    Some(Value::Long(v)) => *v,
                    _ => 0,
                };
                let unit_ordinal = match args.get(2) {
                    Some(Value::Object(Some(u))) => time_unit_ordinal(ctx, *u),
                    _ => 2,
                };
                let timeout_ms = convert_time_unit_to_millis(timeout_val, unit_ordinal);
                let deadline =
                    std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms as u64);
                // GC-safety: parks on the monitor -- the widest collection
                // window there is -- and dereferences `this` on the next turn.
                let this_pin = ctx.pin_native_root(this);
                loop {
                    let mut this = ctx.read_native_pin(this_pin, this);
                    ctx.monitor_enter(this);
                    let size = match ctx.get_field(this, 1) {
                        Value::Int(n) => n,
                        _ => 0,
                    };
                    if size > 0 {
                        let result = m18_lbq_remove_head(ctx, this);
                        ctx.monitor_notify_all(this)?;
                        ctx.monitor_exit(this);
                        return Ok(Some(result));
                    }
                    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                    if remaining.is_zero() {
                        ctx.monitor_exit(this);
                        return Ok(Some(Value::Object(None)));
                    }
                    let wait_ms = bounded_monitor_wait_ms(remaining, 10);
                    monitor_wait_release(ctx, &mut this, Some(wait_ms))?;
                }
            }, NativeKind::SyntheticStub,
        );
        // take() — blocking
        registry.register(ltq, "take", "()Ljava/lang/Object;", |ctx, args| {
            let mut this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            // GC-safety: parks on the monitor and dereferences `this` on
            // the next turn.
            let this_pin = ctx.pin_native_root(this);
            loop {
                let mut this = ctx.read_native_pin(this_pin, this);
                ctx.monitor_enter(this);
                let size = match ctx.get_field(this, 1) {
                    Value::Int(n) => n,
                    _ => 0,
                };
                if size > 0 {
                    let result = m18_lbq_remove_head(ctx, this);
                    ctx.monitor_notify_all(this)?;
                    ctx.monitor_exit(this);
                    return Ok(Some(result));
                }
                monitor_wait_release(ctx, &mut this, Some(10))?;
            }
        }, NativeKind::SyntheticStub);
        // peek() — non-blocking
        registry.register(ltq, "peek", "()Ljava/lang/Object;", |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            let size = match ctx.get_field(this, 1) {
                Value::Int(n) => n,
                _ => 0,
            };
            if size == 0 {
                return Ok(Some(Value::Object(None)));
            }
            let arr = match ctx.get_field(this, 0) {
                Value::Object(Some(a)) => a,
                _ => return Ok(Some(Value::Object(None))),
            };
            Ok(Some(ctx.get_array_element(arr, 0)))        }, NativeKind::SyntheticStub);
        // size()
        registry.register(ltq, "size", "()I", |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Int(0))),
            };
            Ok(Some(ctx.get_field(this, 1)))        }, NativeKind::SyntheticStub);
        // isEmpty()
        registry.register(ltq, "isEmpty", "()Z", |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Int(1))),
            };
            let size = match ctx.get_field(this, 1) {
                Value::Int(n) => n,
                _ => 0,
            };
            Ok(Some(Value::Int(if size == 0 { 1 } else { 0 })))        }, NativeKind::SyntheticStub);    } // end of #[cfg(feature = "synthetic-jdk")] block for LinkedTransferQueue

    // --- Flow.Publisher / Flow.Subscriber / Flow.Subscription ---
    // These are interfaces in Java; register minimal natives for the reactive-streams bridge.
    let flow_pub = "java/util/concurrent/Flow$Publisher";
    registry.register(
        flow_pub,
        "subscribe",
        "(Ljava/util/concurrent/Flow$Subscriber;)V",
        |ctx, args| {
            // Default: immediately call onSubscribe then onComplete
            if let Some(Value::Object(Some(subscriber))) = args.get(1) {
                // gc-common w21-a: the subscriber (a raw copy out of `args`) is
                // rooted across the Subscription's allocation and
                // `onSubscribe` (Java); `onComplete` was dispatched through
                // its pre-call address.
                let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
                let subscriber_h = scope.root(*subscriber);
                // 2-field layout, matching phase 60 / SubmissionPublisher.subscribe:
                // 0 = cancelled flag, 1 = accumulated demand (Long). The second
                // slot is what makes `request(n)` above observable.
                let sub = try_alloc_concurrent_synthetic(
                    &mut *scope,
                    "java/util/concurrent/Flow$Subscription",
                    2,
                )?;
                scope.set_field(sub, 0, Value::Int(0)); // cancelled flag
                scope.set_field(sub, 1, Value::Long(0)); // demand
                let subscriber = scope.get(&subscriber_h);
                let _ = scope.invoke_virtual(
                    subscriber,
                    "onSubscribe",
                    "(Ljava/util/concurrent/Flow$Subscription;)V",
                    &[Value::Object(Some(sub))],
                );
                let subscriber = scope.get(&subscriber_h);
                let _ = scope.invoke_virtual(subscriber, "onComplete", "()V", &[]);
            }
            Ok(None)
        }, NativeKind::SyntheticStub,
    );

    let flow_sub = "java/util/concurrent/Flow$Subscription";
    // W4: the "SUPERSEDED, never runs" comment that used to sit here was WRONG
    // for the default (real-JDK) build. `register_stream_overrides` only wins
    // when it is registered LAST, and it is not: in real-JDK mode
    // `vm_init.rs` calls `register_essential_natives_with_shims` (which reaches
    // `register_stream_overrides` via `register_annotation_overrides`) and only
    // THEN calls `register_concurrent_natives`, i.e. this module. So this
    // registration is the live one in the default build; only in
    // `--synthetic-jdk` mode does lib.rs order the stream overrides (and, later
    // still, `register_p60_flow`) after it.
    //
    // Accumulate the demand for real instead of dropping it, using the same
    // layout phase 60 uses and that `SubmissionPublisher.subscribe` allocates
    // (field 0 = cancelled flag, field 1 = Long demand). Objects with fewer
    // slots (older 1-field synthetic subscriptions) keep the no-op behaviour
    // rather than writing out of bounds.
    registry.register(flow_sub, "request", "(J)V", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(None),
        };
        let n = match args.get(1) {
            Some(Value::Long(v)) => *v,
            Some(Value::Int(v)) => *v as i64,
            _ => 0,
        };
        // Reactive-streams: a non-positive request is a protocol error; with no
        // subscriber reference to call `onError` on, ignore it (same choice as
        // `streams::native_flow_request`).
        if n <= 0 || ctx.object_num_fields(this) < 2 {
            return Ok(None);
        }
        let cur = match ctx.get_field(this, 1) {
            Value::Long(v) => v,
            Value::Int(v) => v as i64,
            _ => 0,
        };
        ctx.set_field(this, 1, Value::Long(cur.saturating_add(n)));
        Ok(None)
    }, NativeKind::SyntheticStub);
    registry.register(flow_sub, "cancel", "()V", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(None),
        };
        ctx.set_field(this, 0, Value::Int(1));
        Ok(None)
    }, NativeKind::SyntheticStub);

    // SubmissionPublisher — B6 dedup (nb-core-mediums, fable-2026-06-10):
    // the stale `<init>`/`close`/`isClosed` registrations that used to live here
    // were registered LAST (register_t31_concurrent_extras runs after
    // register_phase60_natives), so they SHADOWED the working phase-60
    // (`register_p60_flow`) versions in phases_late.rs that Round 5 wired up for
    // real subscriber delivery. In particular this `close()V` only flipped the
    // closed flag and never fired `onComplete()`, defeating the fix. The block
    // is deleted so the delivering phase-60 registrations win. See
    // phases_late.rs:16549+ for the canonical implementations.
}

// --- ReentrantLock ---

pub fn native_rl_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let key = rl_key(ctx, this);
    rl_with(key, |st| {
        *st = RlState {
            owner: RL_UNOWNED,
            hold: 0,
            fair: false,
        };
    });
    Ok(None)
}

pub fn native_rl_init_fair(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let fair = matches!(args.get(1), Some(Value::Int(v)) if *v != 0);
    let key = rl_key(ctx, this);
    rl_with(key, |st| {
        *st = RlState {
            owner: RL_UNOWNED,
            hold: 0,
            fair,
        };
    });
    Ok(None)
}

pub fn native_rl_lock(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let tid = ctx.thread_id() as i64;
    let key = rl_key(ctx, this);
    let this_pin = ctx.pin_native_root(this);
    loop {
        let mut this = ctx.read_native_pin(this_pin, this);
        // Atomically claim (or reentrantly re-claim) the lock under the
        // side-table mutex. `claimed` is true iff this call now holds it.
        let claimed = rl_with(key, |st| {
            if st.owner == RL_UNOWNED || st.owner == tid {
                st.owner = tid;
                st.hold += 1;
                true
            } else {
                false
            }
        });
        if claimed {
            return Ok(None);
        }
        // Owned by another thread — park on the lock object's monitor until
        // a releaser notifies. The 5ms timeout re-checks defensively.
        ctx.monitor_enter(this);
        let still_owned = {
            let st = rl_get(key);
            st.owner != RL_UNOWNED && st.owner != tid
        };
        if still_owned {
            // `ReentrantLock.lock()` (unlike `lockInterruptibly()` /
            // `tryLock(timeout, unit)`) is specified to never throw on
            // interrupt -- it defers: per the `Lock#lock()` javadoc, "If the
            // current thread... is interrupted while acquiring the lock...
            // it will continue to wait... but upon acquiring the lock its
            // interrupted status will be set." `monitor_wait` throws
            // `InterruptedException` just like `Object.wait()` does (and
            // already consumes the thread's interrupt flag when it does) --
            // absorb that here, restore the flag via `thread_interrupt` on
            // our own thread object, and keep retrying instead of
            // propagating the exception out of a method that must not throw.
            if let Err(e) = monitor_wait_release(ctx, &mut this, Some(5)) {
                if is_interrupted_exception(&e) {
                    let self_thread = ctx.current_thread_object();
                    ctx.thread_interrupt(self_thread);
                    continue;
                }
                return Err(e);
            }
        } else {
            ctx.monitor_exit(this);
        }
    }
}

pub fn native_rl_unlock(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let tid = ctx.thread_id() as i64;
    let key = rl_key(ctx, this);
    // Atomically verify ownership and decrement; report whether the lock is
    // now fully released so we can notify a waiter outside the mutex.
    let outcome = rl_with(key, |st| {
        if st.owner != tid {
            return None;
        }
        st.hold -= 1;
        if st.hold <= 0 {
            st.hold = 0;
            st.owner = RL_UNOWNED;
            Some(true)
        } else {
            Some(false)
        }
    });
    match outcome {
        None => Err(RuntimeError::IllegalMonitorStateException {
            message: "current thread is not owner".to_string(),
        }
        .into()),
        Some(fully_released) => {
            if fully_released {
                // Wake one thread waiting in `native_rl_lock` to acquire.
                ctx.monitor_enter(this);
                ctx.monitor_notify(this)?;
                ctx.monitor_exit(this);
            }
            Ok(None)
        }
    }
}

pub fn native_rl_try_lock(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let tid = ctx.thread_id() as i64;
    let key = rl_key(ctx, this);
    let claimed = rl_with(key, |st| {
        if st.owner == RL_UNOWNED || st.owner == tid {
            st.owner = tid;
            st.hold += 1;
            true
        } else {
            false
        }
    });
    Ok(Some(Value::Int(if claimed { 1 } else { 0 })))
}

pub fn native_rl_try_lock_timeout(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // args: [this, long timeout, TimeUnit unit]
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let tid = ctx.thread_id() as i64;
    let timeout_val = match args.get(1) {
        Some(Value::Long(v)) => *v,
        _ => 0,
    };
    let unit_ord = match args.get(2) {
        Some(Value::Object(Some(u))) => time_unit_ordinal(ctx, *u),
        _ => 2,
    };
    let timeout_ms = convert_time_unit_to_millis(timeout_val, unit_ord);
    if timeout_ms <= 0 {
        return native_rl_try_lock(ctx, args);
    }
    let key = rl_key(ctx, this);
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms as u64);
    let this_pin = ctx.pin_native_root(this);
    loop {
        let mut this = ctx.read_native_pin(this_pin, this);
        let claimed = rl_with(key, |st| {
            if st.owner == RL_UNOWNED || st.owner == tid {
                st.owner = tid;
                st.hold += 1;
                true
            } else {
                false
            }
        });
        if claimed {
            return Ok(Some(Value::Int(1)));
        }
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return Ok(Some(Value::Int(0)));
        }
        let wait_ms = remaining.as_millis().min(5).max(1) as u64;
        ctx.monitor_enter(this);
        monitor_wait_release(ctx, &mut this, Some(wait_ms))?;
        if ctx.is_interrupted(true) {
            return Err(cratonvm_types::error::MethodCallFailed::InternalError(
                cratonvm_types::error::VmError::Runtime(
                    cratonvm_types::error::RuntimeError::InterruptedException,
                ),
            ));
        }
    }
}

pub fn native_rl_is_locked(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let key = rl_key(ctx, this);
    Ok(Some(Value::Int(if rl_get(key).owner != RL_UNOWNED {
        1
    } else {
        0
    })))
}

pub fn native_rl_is_held_by_current_thread(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let tid = ctx.thread_id() as i64;
    let key = rl_key(ctx, this);
    Ok(Some(Value::Int(if rl_get(key).owner == tid {
        1
    } else {
        0
    })))
}

pub fn native_rl_get_hold_count(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let tid = ctx.thread_id() as i64;
    let key = rl_key(ctx, this);
    let st = rl_get(key);
    // `getHoldCount()` reports the count for the *current* thread only.
    Ok(Some(Value::Int(if st.owner == tid { st.hold } else { 0 })))
}

pub fn native_rl_is_fair(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let key = rl_key(ctx, this);
    Ok(Some(Value::Int(if rl_get(key).fair { 1 } else { 0 })))
}

pub fn native_rl_new_condition(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let cond = try_alloc_concurrent_synthetic(ctx, "java/util/concurrent/locks/Condition", 1)?;
    ctx.set_field(cond, COND_FIELD_LOCK, Value::Object(Some(this)));
    Ok(Some(Value::Object(Some(cond))))
}

pub fn native_rl_to_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key = rl_key(ctx, this);
    let state = if rl_get(key).owner != RL_UNOWNED {
        "locked"
    } else {
        "unlocked"
    };
    // `Object.toString()`'s `@` part is `Integer.toHexString(hashCode())`, and
    // `ReentrantLock` does not override `hashCode`: the identity hash. It was
    // the heap ADDRESS (gc-common w29-b), which HotSpot never prints and which
    // a moving collection changes, so one lock rendered two ways across a GC.
    let s = format!(
        "java.util.concurrent.locks.ReentrantLock@{:x}[{state}]",
        ctx.identity_hash_code(this) as u32
    );
    let obj = ctx.create_string(&s);
    Ok(Some(Value::Object(Some(obj))))
}

// --- Condition ---

pub fn native_cond_await(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    // Get the associated lock and verify ownership
    let lock_ref = match ctx.get_field(this, COND_FIELD_LOCK) {
        Value::Object(Some(o)) => o,
        _ => {
            return Err(RuntimeError::IllegalMonitorStateException {
                message: "condition not associated with a lock".to_string(),
            }
            .into())
        }
    };
    let tid = ctx.thread_id() as i64;
    let lock_key = rl_key(ctx, lock_ref);
    // FELIX FIX (lost-wakeup race): acquire the *condition* monitor BEFORE
    // releasing the ReentrantLock. `signal`/`signalAll` must `monitor_enter`
    // this same monitor to notify, so holding it here forces a concurrent
    // signaller to block until we reach `monitor_wait` (which atomically
    // releases the monitor and waits). Previously the lock was released and
    // lock-waiters notified BEFORE `monitor_enter(this)`, leaving a window
    // where a `signalAll()` from the lock's new owner ran to completion and
    // its `notify_all` reached no waiter — the awaiter then blocked on
    // `monitor_wait` forever. This is exactly Felix's `acquireBundleLock`/
    // `releaseBundleLock` handoff, which otherwise deadlocks `Felix.start()`.
    ctx.monitor_enter(this);
    let saved_hold = match rl_release_for_await(lock_key, tid) {
        Some(h) => h,
        None => {
            ctx.monitor_exit(this);
            return Err(RuntimeError::IllegalMonitorStateException {
                message: "current thread is not owner".to_string(),
            }
            .into());
        }
    };
    // Notify any threads waiting to acquire the now-free lock.
    ctx.monitor_enter(lock_ref);
    ctx.monitor_notify(lock_ref)?;
    ctx.monitor_exit(lock_ref);
    // Atomically release the condition monitor and wait for a signal.
    //
    // GC-SAFEPOINT FIX: `monitor_wait` parks this thread, so a collection can
    // run while we are blocked — the moving collector, or the non-moving
    // young sweep's selective promotion — and relocate both `this` and
    // `lock_ref`. The raw ObjectRefs captured at entry would then be stale, and
    // the `monitor_exit` / `reacquire` below would `header_of` a dead address →
    // EXCEPTION_ACCESS_VIOLATION (TestSwallowAbortedUploads SIGSEGV in
    // MonitorTable::exit). Pin both across the wait and read back their post-GC
    // addresses; `native_pin_roots` is remapped by the collector (see
    // vm/src/memory/gc.rs `update_all_roots`). Unpin even on the error path.
    let this_pin = ctx.pin_native_root(this);
    let lock_pin = ctx.pin_native_root(lock_ref);
    let wr = ctx.monitor_wait(this, None);
    let this = ctx.read_native_pin(this_pin, this);
    let lock_ref = ctx.read_native_pin(lock_pin, lock_ref);
    ctx.unpin_native_roots(this_pin);
    ctx.monitor_exit(this);
    // Condition contract (java.util.concurrent.locks.Condition#await): "In all
    // cases, before this method can return the current thread must re-acquire
    // the lock associated with this condition." That includes the interrupted
    // case — so re-acquire the lock BEFORE propagating the interrupt. Otherwise
    // the caller's `lock.unlock()` (e.g. `LinkedBlockingQueue.take`'s `finally`)
    // runs without holding the lock and throws IllegalMonitorStateException,
    // which corrupts a ThreadPoolExecutor worker's shutdown under takeLock
    // contention (multiple idle workers) → workers never terminate.
    reacquire_lock_after_await(ctx, lock_ref, lock_key, tid, saved_hold)?;
    wr?;
    Ok(None)
}

pub fn native_cond_await_timeout(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // RD.5: args = [this, long timeout, TimeUnit]. Convert via TimeUnit ordinal.
    let timeout_raw = match args.get(1) {
        Some(Value::Long(v)) => *v,
        _ => 0,
    };
    let unit_ordinal = match args.get(2) {
        Some(Value::Object(Some(u))) => time_unit_ordinal(ctx, *u),
        _ => 2,
    };
    let timeout_ms = convert_time_unit_to_millis(timeout_raw, unit_ordinal).max(0) as u64;
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(1))),
    };
    cond_await_millis(ctx, this, timeout_ms)
}

/// `Condition.awaitUntil(Date deadline)` — wait until the absolute deadline.
/// Returns Z: false (0) if the deadline has already elapsed, otherwise the
/// timed-wait result. Without this the synthetic `Condition` had no body for
/// `awaitUntil` and the abstract interface method raised `AbstractMethodError`
/// (keycloak `WaitConditionShutdownListenerTest`).
pub fn native_cond_await_until(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(1))),
    };
    let deadline_ms = match args.get(1) {
        Some(Value::Object(Some(d))) => match ctx.invoke_virtual(*d, "getTime", "()J", &[])? {
            Some(Value::Long(t)) => t,
            _ => 0,
        },
        // null deadline → behave as an immediate timeout (false).
        _ => return Ok(Some(Value::Int(0))),
    };
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let remaining = deadline_ms - now_ms;
    if remaining <= 0 {
        // Deadline already passed — never wait (Some(0) means "wait forever"
        // to monitor_wait), just report the timeout.
        return Ok(Some(Value::Int(0)));
    }
    cond_await_millis(ctx, this, remaining as u64)
}

pub fn native_cond_await_nanos(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // Extract nanos timeout from args[1] (Long)
    let nanos = match args.get(1) {
        Some(Value::Long(v)) => (*v).max(0),
        _ => 0,
    };
    let timeout_ms = (nanos / 1_000_000).max(1) as u64;
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Long(0))),
    };
    let lock_ref = match ctx.get_field(this, COND_FIELD_LOCK) {
        Value::Object(Some(o)) => o,
        _ => return Ok(Some(Value::Long(0))),
    };
    let tid = ctx.thread_id() as i64;
    let lock_key = rl_key(ctx, lock_ref);
    // FELIX FIX (lost-wakeup race): hold the condition monitor across the
    // lock release — see `native_cond_await` for the full rationale.
    ctx.monitor_enter(this);
    let saved_hold = match rl_release_for_await(lock_key, tid) {
        Some(h) => h,
        None => {
            ctx.monitor_exit(this);
            return Err(RuntimeError::IllegalMonitorStateException {
                message: "current thread is not owner".to_string(),
            }
            .into());
        }
    };
    ctx.monitor_enter(lock_ref);
    ctx.monitor_notify(lock_ref)?;
    ctx.monitor_exit(lock_ref);
    let start = std::time::Instant::now();
    // GC-SAFEPOINT FIX (see native_cond_await): pin `this`/`lock_ref` across
    // the blocking wait so a relocating GC can't leave them stale.
    let this_pin = ctx.pin_native_root(this);
    let lock_pin = ctx.pin_native_root(lock_ref);
    let wr = ctx.monitor_wait(this, Some(timeout_ms));
    let this = ctx.read_native_pin(this_pin, this);
    let lock_ref = ctx.read_native_pin(lock_pin, lock_ref);
    ctx.unpin_native_roots(this_pin);
    ctx.monitor_exit(this);
    let elapsed_nanos = start.elapsed().as_nanos() as i64;
    let remaining = nanos.saturating_sub(elapsed_nanos).max(0);
    // Condition contract: re-acquire the lock before returning, even on
    // interrupt (see `native_cond_await`).
    reacquire_lock_after_await(ctx, lock_ref, lock_key, tid, saved_hold)?;
    wr?;
    Ok(Some(Value::Long(remaining)))
}

pub fn native_cond_signal(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    // Verify current thread owns the associated lock
    let lock_ref = match ctx.get_field(this, COND_FIELD_LOCK) {
        Value::Object(Some(o)) => o,
        _ => {
            return Err(RuntimeError::IllegalMonitorStateException {
                message: "condition not associated with a lock".to_string(),
            }
            .into())
        }
    };
    let tid = ctx.thread_id() as i64;
    let lock_key = rl_key(ctx, lock_ref);
    if rl_get(lock_key).owner != tid {
        return Err(RuntimeError::IllegalMonitorStateException {
            message: "current thread does not hold the lock".to_string(),
        }
        .into());
    }
    // Wake one thread waiting on this condition
    ctx.monitor_enter(this);
    ctx.monitor_notify(this)?;
    ctx.monitor_exit(this);
    Ok(None)
}

pub fn native_cond_signal_all(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    // Verify current thread owns the associated lock
    let lock_ref = match ctx.get_field(this, COND_FIELD_LOCK) {
        Value::Object(Some(o)) => o,
        _ => {
            return Err(RuntimeError::IllegalMonitorStateException {
                message: "condition not associated with a lock".to_string(),
            }
            .into())
        }
    };
    let tid = ctx.thread_id() as i64;
    let lock_key = rl_key(ctx, lock_ref);
    if rl_get(lock_key).owner != tid {
        return Err(RuntimeError::IllegalMonitorStateException {
            message: "current thread does not hold the lock".to_string(),
        }
        .into());
    }
    // Wake all threads waiting on this condition
    ctx.monitor_enter(this);
    ctx.monitor_notify_all(this)?;
    ctx.monitor_exit(this);
    Ok(None)
}

pub fn native_cdl_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let count = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    if count < 0 {
        return Err(RuntimeError::IllegalArgumentException {
            message: "count < 0".to_string(),
        }
        .into());
    }
    cdl_set_count(ctx, this, count);
    Ok(None)
}

pub fn native_cdl_count_down(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    // Serialize the read-modify-write against concurrent countDown() calls —
    // two racing decrements must not lose one (the boot-thread/test-thread
    // handshake counts on exactly-N decrements releasing the latch).
    // GC-SAFEPOINT FIX: monitor_enter_gc_safe's contended wait can span a
    // completing GC pause; use its returned reference so a relocated `this`
    // doesn't go stale under the calls below.
    let this = ctx.monitor_enter_gc_safe(this);
    let count = cdl_count(ctx, this);
    if count > 0 {
        cdl_set_count(ctx, this, count - 1);
        if count - 1 == 0 {
            // Count reached zero — wake all waiting threads
            let wait_key = cdl_wait_key(ctx, this);
            ctx.vt_wake_waiters(wait_key);
            let notify_result = ctx.monitor_notify_all(this);
            ctx.monitor_exit(this);
            notify_result?;
            return Ok(None);
        }
    }
    ctx.monitor_exit(this);
    Ok(None)
}

pub fn native_cdl_await(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    // JDK 25 `await()` is `acquireSharedInterruptibly(1)`, which tests
    // `Thread.interrupted()` BEFORE the count: a pending interrupt raises (and
    // is cleared) even on an open latch. Interpreter round i3 wave 79, lane L1
    // (`i71-L1-countdownlatch-stand-in-ignores-a-pending-interrupt`).
    if ctx.is_interrupted(true) {
        return Err(cdl_interrupted());
    }
    if ctx.is_current_virtual() && ctx.vt_pin_count() == 0 {
        if cdl_count(ctx, this) <= 0 {
            return Ok(None);
        }
        let wait_key = cdl_wait_key(ctx, this);
        if ctx.vt_wait_on_key(wait_key) {
            // Close the registration-vs-countDown race. If count reached zero
            // first, cancel locally and complete without unmounting.
            if cdl_count(ctx, this) <= 0 {
                ctx.vt_cancel_wait_on_key(wait_key);
                return Ok(None);
            }
            return Err(MethodCallFailed::InternalError(
                cratonvm_types::error::VmError::ContinuationYield {
                    wake_after_nanos: 0,
                },
            ));
        }
    }
    // Block on the monitor instead of spinning. The bounded wait (10ms)
    // covers the lost-wakeup window between the count read and the wait.
    loop {
        if cdl_count(ctx, this) <= 0 {
            return Ok(None);
        }
        // GC-SAFEPOINT FIX: monitor_enter_gc_safe's contended wait can span a
        // completing GC pause; use its returned reference so a relocated
        // `this` doesn't go stale under the wait below.
        this = ctx.monitor_enter_gc_safe(this);
        // GC-SAFEPOINT FIX: the wait can relocate `this`; pin + read back.
        // Interpreter round i2 wave 65, lane L1 (`CRATONVM_SERVED_WAIT_KINDS`):
        // the JDK's `await()` is an AQS shared acquire that parks: WAITING,
        // JVMTI PARKED (not the IN_OBJECT_WAIT of the slice's monitor wait).
        this = monitor_wait_keepalive_and_exit_as(
            ctx,
            this,
            Some(10),
            cratonvm_native_api::BlockKind::Parked,
        )?;
    }
}

pub fn native_cdl_await_timeout(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    // args: this, timeout(long), TimeUnit
    let timeout_val = match args.get(1) {
        Some(Value::Long(v)) => *v,
        _ => 0,
    };
    // A null `TimeUnit` is the JDK's FIRST action, not a defaulting decision:
    // (this guard is shared by `native_cdl_await_timeout`, which IS registered
    // and was measurably wrong, and `native_sem_try_acquire_timeout`, which the
    // registry says is registered NOWHERE -- its copy of the defect is latent,
    // and is fixed here so it cannot arrive with the registration.)
    // `await` opens with `unit.toNanos(timeout)`. Defaulting to MILLISECONDS
    // turned `await(1, null)` into a one-millisecond wait returning `false`,
    // where HotSpot (and this VM's own strict arm, which refuses this native
    // and runs the bytecode) raises NPE. Message transcribed from the oracle.
    let unit_ordinal = match args.get(2) {
        Some(Value::Object(Some(u))) => time_unit_ordinal(ctx, *u),
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                message: Some(
                    "Cannot invoke \"java.util.concurrent.TimeUnit.toNanos(long)\" because \"unit\" is null"
                        .to_string(),
                ),
            }
            .into())
        }
    };
    // `tryAcquireSharedNanos`, in the JDK's order (interpreter round i3 wave
    // 79, lane L1): `Thread.interrupted()` first (raise, and clear it), then
    // the count, then `nanosTimeout <= 0L` answers `false` without waiting.
    // The timeout is kept in nanoseconds: the old whole-millisecond cast made
    // a negative timeout a ~1.8e19 ms deadline (wait forever,
    // `i71-L1-countdownlatch-stand-in-negative-timeout-waits-forever`) and a
    // sub-millisecond one an immediate `false`.
    if ctx.is_interrupted(true) {
        return Err(cdl_interrupted());
    }
    if cdl_count(ctx, this) == 0 {
        return Ok(Some(Value::Int(1)));
    }
    let timeout_nanos = cdl_timeout_nanos(timeout_val, unit_ordinal);
    if timeout_nanos <= 0 {
        return Ok(Some(Value::Int(0)));
    }
    // A deadline past what `Instant` can hold is no deadline at all.
    let deadline = std::time::Instant::now()
        .checked_add(std::time::Duration::from_nanos(timeout_nanos as u64));

    loop {
        let count = cdl_count(ctx, this);
        if count == 0 {
            return Ok(Some(Value::Int(1))); // true — count reached zero
        }
        let remaining = match deadline {
            Some(d) => d.saturating_duration_since(std::time::Instant::now()),
            None => std::time::Duration::from_millis(10),
        };
        if remaining.is_zero() {
            return Ok(Some(Value::Int(0))); // false — timed out
        }
        let wait_ms = bounded_monitor_wait_ms(remaining, 10);
        // GC-SAFEPOINT FIX: monitor_enter_gc_safe's contended wait can span a
        // completing GC pause; use its returned reference so a relocated
        // `this` doesn't go stale under the wait below.
        this = ctx.monitor_enter_gc_safe(this);
        // GC-SAFEPOINT FIX: the wait can relocate `this`; pin + read back.
        // Interpreter round i2 wave 65, lane L1 (`CRATONVM_SERVED_WAIT_KINDS`):
        // the JDK's `await(long, TimeUnit)` is `tryAcquireSharedNanos`, which
        // parks with a deadline: TIMED_WAITING, JVMTI PARKED, as HotSpot reads
        // it (0x2a1). A plain slice read WAITING / IN_OBJECT_WAIT (0x191),
        // `i64-orch-compatible-timed-countdownlatch-await-reads-waiting-20261028.md`.
        this = monitor_wait_keepalive_and_exit_as(
            ctx,
            this,
            Some(wait_ms),
            cratonvm_native_api::BlockKind::TimedParked,
        )?;
    }
}

/// The `InterruptedException` (no message, as AQS throws it) the latch
/// stand-ins raise for a pending interrupt.
pub fn cdl_interrupted() -> MethodCallFailed {
    MethodCallFailed::InternalError(cratonvm_types::error::VmError::Runtime(
        RuntimeError::InterruptedException,
    ))
}

/// `unit.toNanos(timeout)` for a `TimeUnit` ordinal (`NANOSECONDS` = 0 ..
/// `DAYS` = 6), saturating at `Long.MAX_VALUE` / `Long.MIN_VALUE` as
/// `TimeUnit.cvt` does. An unknown ordinal reads as nanoseconds.
pub fn cdl_timeout_nanos(value: i64, ordinal: i32) -> i64 {
    let scale: i64 = match ordinal {
        1 => 1_000,
        2 => 1_000_000,
        3 => 1_000_000_000,
        4 => 60_000_000_000,
        5 => 3_600_000_000_000,
        6 => 86_400_000_000_000,
        _ => 1,
    };
    value.saturating_mul(scale)
}

pub fn native_cdl_get_count(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Long(0))),
    };
    let count = cdl_count(ctx, this) as i64;
    Ok(Some(Value::Long(count)))
}

pub fn native_cdl_to_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let count = cdl_count(ctx, this);
    // The identity hash, as `Object.toString()` renders it -- not the heap
    // address (gc-common w29-b; see `native_rl_to_string`).
    let s = format!(
        "java.util.concurrent.CountDownLatch@{:x}[Count = {count}]",
        ctx.identity_hash_code(this) as u32
    );
    let obj = ctx.create_string(&s);
    Ok(Some(Value::Object(Some(obj))))
}

/// GC-stable key for a holder-less `Semaphore`'s side state, from the lib.rs
/// lock-key registry (`gc_stable_weak_lock_key`).
///
/// gc-common w16-b (`common-w15b-three-more-identity-key-registry-copies`):
/// this used to be a private copy of the identity-hash + generation scheme
/// with no VM in its entries, never swept or remapped, whose "lone occupant"
/// rule rebound any slot -- a dead `Semaphore`'s, or another VM's -- to the
/// next object with its identity hash. Two VMs number hashes from the same
/// seed, so VM B's `Semaphore` could share VM A's permits. The shared
/// registry is VM-scoped, never re-mints a key, and drops a dead object's
/// slot at the first sweep that finds it dead; the sweep then calls
/// [`forget_sem_state_keys`] with the freed keys.
pub fn sem_obj_key_for(ctx: &dyn NativeContext, obj: ObjectRef) -> usize {
    gc_stable_weak_lock_key(ctx, obj)
        .unwrap_or_else(|_| unreachable!("gc_stable_weak_lock_key never fails"))
}

pub fn sem_states_by_obj() -> &'static parking_lot::Mutex<std::collections::HashMap<usize, SemState>> {
    static H: std::sync::OnceLock<parking_lot::Mutex<std::collections::HashMap<usize, SemState>>> =
        std::sync::OnceLock::new();
    H.get_or_init(|| parking_lot::Mutex::new(std::collections::HashMap::new()))
}

/// Drop the `Semaphore` side states filed under `keys`, which the lock-key
/// registry has just freed (the semaphore died, or its VM is being torn down)
/// and will never mint again. Called by `lib.rs::sweep_lock_keys` and
/// `lib.rs::forget_vm_lock_keys` with the registry lock released; the table
/// lock is a leaf.
pub fn forget_sem_state_keys(keys: &[usize]) {
    if keys.is_empty() {
        return;
    }
    let mut states = sem_states_by_obj().lock();
    if states.is_empty() {
        return;
    }
    for k in keys {
        states.remove(k);
    }
}

pub fn sem_side_state(ctx: &dyn NativeContext, this: ObjectRef) -> SemState {
    let key = sem_obj_key_for(ctx, this);
    let mut states = sem_states_by_obj().lock();
    *states.entry(key).or_insert(SemState {
        permits: 0,
        fair: 0,
    })
}

pub fn sem_set_side_state(ctx: &dyn NativeContext, this: ObjectRef, state: SemState) {
    let key = sem_obj_key_for(ctx, this);
    sem_states_by_obj().lock().insert(key, state);
}

pub fn sem_has_holder_slot(ctx: &dyn NativeContext, this: ObjectRef) -> bool {
    ctx.object_num_fields(this) > SEM_FIELD_PERMITS
}

pub fn sem_prepare(ctx: &mut dyn NativeContext, this: ObjectRef) -> ObjectRef {
    if sem_has_holder_slot(ctx, this) {
        sem_holder(ctx, this).0
    } else {
        let _ = sem_side_state(ctx, this);
        this
    }
}

pub fn sem_holder(ctx: &mut dyn NativeContext, this: ObjectRef) -> (ObjectRef, ObjectRef) {
    let has_permits_slot = sem_has_holder_slot(ctx, this);
    if !has_permits_slot {
        let _ = sem_side_state(ctx, this);
        return (this, this);
    }
    if let Value::Object(Some(h)) = ctx.get_field(this, SEM_FIELD_PERMITS) {
        return (this, h);
    }
    // Install the holder, migrating any legacy raw-Int values.
    let legacy_permits = if has_permits_slot {
        match ctx.get_field(this, SEM_FIELD_PERMITS) {
            Value::Int(v) => v,
            _ => 0,
        }
    } else {
        0
    };
    let legacy_fair = if ctx.object_num_fields(this) > SEM_FIELD_FAIR {
        match ctx.get_field(this, SEM_FIELD_FAIR) {
            Value::Int(v) => v,
            _ => 0,
        }
    } else {
        0
    };
    // Pin `this` across the allocation — a moving GC during `new_array` would
    // relocate the receiver and leave the Rust-local copy stale (the
    // StringBuffer-GC-hazard pattern).
    let this_pin = ctx.pin_native_root(this);
    let h = ctx.new_array(cratonvm_types::ArrayElementType::Int, 2);
    let this = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
    ctx.set_array_element(h, 0, Value::Int(legacy_permits));
    ctx.set_array_element(h, 1, Value::Int(legacy_fair));
    if ctx.object_num_fields(this) > SEM_FIELD_PERMITS {
        ctx.set_field(this, SEM_FIELD_PERMITS, Value::Object(Some(h)));
    } else {
        sem_set_side_state(
            ctx,
            this,
            SemState {
                permits: legacy_permits,
                fair: legacy_fair,
            },
        );
    }
    (this, h)
}

pub fn sem_permits(ctx: &mut dyn NativeContext, this: ObjectRef) -> i32 {
    if !sem_has_holder_slot(ctx, this) {
        return sem_side_state(ctx, this).permits;
    }
    let (_, h) = sem_holder(ctx, this);
    match ctx.get_array_element(h, 0) {
        Value::Int(v) => v,
        _ => 0,
    }
}

pub fn sem_set_permits(ctx: &mut dyn NativeContext, this: ObjectRef, permits: i32) {
    if !sem_has_holder_slot(ctx, this) {
        let mut state = sem_side_state(ctx, this);
        state.permits = permits;
        sem_set_side_state(ctx, this, state);
        return;
    }
    let (_, h) = sem_holder(ctx, this);
    ctx.set_array_element(h, 0, Value::Int(permits));
}

pub fn native_sem_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let permits = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    if sem_has_holder_slot(ctx, this) {
        let (this, h) = sem_holder(ctx, this);
        let _ = this;
        ctx.set_array_element(h, 0, Value::Int(permits));
        ctx.set_array_element(h, 1, Value::Int(0));
    } else {
        sem_set_side_state(ctx, this, SemState { permits, fair: 0 });
    }
    Ok(None)
}

pub fn native_sem_init_fair(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let permits = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let fair = match args.get(2) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    if sem_has_holder_slot(ctx, this) {
        let (this, h) = sem_holder(ctx, this);
        let _ = this;
        ctx.set_array_element(h, 0, Value::Int(permits));
        ctx.set_array_element(h, 1, Value::Int(fair));
    } else {
        sem_set_side_state(ctx, this, SemState { permits, fair });
    }
    Ok(None)
}

/// Shared blocking-acquire: take `n` permits, waiting until enough are
/// available. The read-modify-write runs under the object monitor so racing
/// acquirers can't both observe and consume the same permit; waiting uses
/// bounded monitor-waits (woken by release's notify) instead of a busy spin.
pub fn sem_acquire_blocking(ctx: &mut dyn NativeContext, this: ObjectRef, n: i32) -> MethodCallResult {
    // Install the holder up-front (re-binding `this` across the possible
    // allocation) so no allocation happens inside the monitor section.
    let mut this = sem_prepare(ctx, this);
    let this_pin = ctx.pin_native_root(this);
    loop {
        let mut this = ctx.read_native_pin(this_pin, this);
        // GC-SAFEPOINT FIX (STW cross-thread JIT-takeover barrier deadlock,
        // same class as native_cdl_await's fix in commit 51a508e1): a raw
        // `ctx.monitor_enter` never marks this thread GC-blocked, so a
        // contended wait here stays counted in the barrier's `expected` set
        // forever, deadlocking any pause requested while we wait on another
        // thread's held lock (a 3-way wedge: us waiting on the owner, the
        // owner GC-blocked waiting for the pause, the pause waiting on us).
        // `monitor_enter_gc_safe` wraps the wait in the proper GC-blocked
        // protocol and returns the possibly-relocated reference — use it for
        // every subsequent access.
        this = ctx.monitor_enter_gc_safe(this);
        let permits = sem_permits(ctx, this);
        if permits >= n {
            sem_set_permits(ctx, this, permits - n);
            ctx.monitor_exit(this);
            ctx.unpin_native_roots(this_pin);
            return Ok(None);
        }
        // GC-SAFEPOINT FIX: the wait itself can also span a relocating GC;
        // the helper exits the monitor at the post-wait address on both arms
        // (wave 25, lane L7: the error arm used to exit at the PRE-wait one),
        // and the next iteration re-reads `this` from `this_pin`.
        // Interpreter round i2 wave 65, lane L1 (`CRATONVM_SERVED_WAIT_KINDS`):
        // the JDK's `acquire` parks in an AQS acquire: WAITING, PARKED.
        if let Err(e) = monitor_wait_keepalive_and_exit_as(
            ctx,
            this,
            Some(10),
            cratonvm_native_api::BlockKind::Parked,
        ) {
            ctx.unpin_native_roots(this_pin);
            return Err(e);
        }
    }
}

pub fn native_sem_acquire(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    sem_acquire_blocking(ctx, this, 1)}

pub fn native_sem_acquire_n(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let n = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 1,
    };
    sem_acquire_blocking(ctx, this, n.max(0))
}

pub fn sem_release_n_inner(ctx: &mut dyn NativeContext, this: ObjectRef, n: i32) -> MethodCallResult {
    let this = sem_prepare(ctx, this);
    // GC-SAFEPOINT FIX: see sem_acquire_blocking above for the full story —
    // this is the exact call site live-gdb-confirmed to hang
    // CrossOriginAnnotationIntegrationTests/RequestMappingMessageConversionIntegrationTests
    // (main-vm parked forever in MonitorTable::enter's contended wait,
    // uncounted-as-blocked, while the true owner is itself GC-blocked
    // waiting on a pause that waits on us).
    let this = ctx.monitor_enter_gc_safe(this);
    let permits = sem_permits(ctx, this);
    sem_set_permits(ctx, this, permits + n);
    let notify_result = ctx.monitor_notify_all(this);
    ctx.monitor_exit(this);
    notify_result?;
    Ok(None)
}

pub fn native_sem_release(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    sem_release_n_inner(ctx, this, 1)}

pub fn native_sem_release_n(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let n = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 1,
    };
    sem_release_n_inner(ctx, this, n.max(0))
}

pub fn sem_try_acquire_inner(ctx: &mut dyn NativeContext, this: ObjectRef, n: i32) -> bool {
    let this = sem_prepare(ctx, this);
    // GC-SAFEPOINT FIX: see sem_acquire_blocking above.
    let this = ctx.monitor_enter_gc_safe(this);
    let permits = sem_permits(ctx, this);
    let ok = permits >= n;
    if ok {
        sem_set_permits(ctx, this, permits - n);
    }
    ctx.monitor_exit(this);
    ok
}

pub fn native_sem_try_acquire(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    Ok(Some(Value::Int(if sem_try_acquire_inner(ctx, this, 1) {
        1
    } else {
        0
    })))
}

pub fn native_sem_try_acquire_n(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let n = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 1,
    };
    Ok(Some(Value::Int(if sem_try_acquire_inner(ctx, this, n) {
        1
    } else {
        0
    })))
}

pub fn native_sem_try_acquire_timeout(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // args: this, timeout(long), TimeUnit — honor the timeout with bounded
    // monitor-waits (was: ignored the timeout and returned tryAcquire()).
    let mut this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let timeout_val = match args.get(1) {
        Some(Value::Long(v)) => *v,
        _ => 0,
    };
    // A null `TimeUnit` is the JDK's FIRST action, not a defaulting decision:
    // (this guard is shared by `native_cdl_await_timeout`, which IS registered
    // and was measurably wrong, and `native_sem_try_acquire_timeout`, which the
    // registry says is registered NOWHERE -- its copy of the defect is latent,
    // and is fixed here so it cannot arrive with the registration.)
    // `await` opens with `unit.toNanos(timeout)`. Defaulting to MILLISECONDS
    // turned `await(1, null)` into a one-millisecond wait returning `false`,
    // where HotSpot (and this VM's own strict arm, which refuses this native
    // and runs the bytecode) raises NPE. Message transcribed from the oracle.
    let unit_ordinal = match args.get(2) {
        Some(Value::Object(Some(u))) => time_unit_ordinal(ctx, *u),
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                message: Some(
                    "Cannot invoke \"java.util.concurrent.TimeUnit.toNanos(long)\" because \"unit\" is null"
                        .to_string(),
                ),
            }
            .into())
        }
    };
    let timeout_ms = convert_time_unit_to_millis(timeout_val, unit_ordinal);
    let deadline =
        std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms.max(0) as u64);
    loop {
        if sem_try_acquire_inner(ctx, this, 1) {
            return Ok(Some(Value::Int(1)));
        }
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return Ok(Some(Value::Int(0)));
        }
        let wait_ms = bounded_monitor_wait_ms(remaining, 10);
        // GC-SAFEPOINT FIX: see sem_acquire_blocking above; `this` must stay
        // fresh across both calls below AND into the next loop iteration's
        // `sem_try_acquire_inner(ctx, this, 1)`.
        this = ctx.monitor_enter_gc_safe(this);
        // Interpreter round i2 wave 65, lane L1 (`CRATONVM_SERVED_WAIT_KINDS`):
        // the JDK's timed `tryAcquire` parks with a deadline: TIMED_WAITING,
        // PARKED.
        this = monitor_wait_keepalive_and_exit_as(
            ctx,
            this,
            Some(wait_ms),
            cratonvm_native_api::BlockKind::TimedParked,
        )?;
    }
}

pub fn native_sem_available_permits(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let permits = sem_permits(ctx, this);
    Ok(Some(Value::Int(permits)))
}

pub fn native_sem_drain_permits(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let this = sem_prepare(ctx, this);
    // GC-SAFEPOINT FIX: see sem_acquire_blocking above.
    let this = ctx.monitor_enter_gc_safe(this);
    let permits = sem_permits(ctx, this);
    sem_set_permits(ctx, this, 0);
    ctx.monitor_exit(this);
    Ok(Some(Value::Int(permits)))
}

pub fn native_sem_is_fair(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    if !sem_has_holder_slot(ctx, this) {
        return Ok(Some(Value::Int(sem_side_state(ctx, this).fair)));
    }
    let (_, h) = sem_holder(ctx, this);
    let fair = match ctx.get_array_element(h, 1) {
        Value::Int(v) => v,
        _ => 0,
    };
    Ok(Some(Value::Int(fair)))}

pub fn native_sem_to_string(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let permits = sem_permits(ctx, this);
    // The identity hash, as `Object.toString()` renders it -- not the heap
    // address (gc-common w29-b; see `native_rl_to_string`).
    let s = format!(
        "java.util.concurrent.Semaphore@{:x}[Permits = {permits}]",
        ctx.identity_hash_code(this) as u32
    );
    let obj = ctx.create_string(&s);
    Ok(Some(Value::Object(Some(obj))))
}

// ===========================================================================
// Phase 28: java.util.concurrent — Executors, Future, CompletableFuture
// ===========================================================================

pub fn register_executor_natives(registry: &mut NativeMethodRegistry) {
    // Interpreter round i3 wave 82, lane L1: over a real JDK with the
    // `JdkThreadPools` HotSpot-alignment row (`CRATONVM_COMPAT_JDK_THREAD_POOLS`,
    // default off) none of the `ThreadPoolExecutor` registrations below is made
    // (`retired_shadow::COMPAT_FAMILIES`: its retired rows and `execute`'s
    // stated stub), as under `--real-jdk`; for a real pool each was a router to
    // the JDK's own body. The `ExecutorService` interface rows stay.
    let es = "java/util/concurrent/ExecutorService";
    let exec = "java/util/concurrent/Executors";
    let tp = "java/util/concurrent/ThreadPoolExecutor";
    let tf = "java/util/concurrent/ThreadFactory";

    // Executors factory methods — return synthetic executor objects
    registry.register(
        exec,
        "newSingleThreadExecutor",
        "()Ljava/util/concurrent/ExecutorService;",
        native_new_single_thread, NativeKind::SyntheticStub,
    );
    registry.register(
        exec,
        "newFixedThreadPool",
        "(I)Ljava/util/concurrent/ExecutorService;",
        native_new_fixed_pool, NativeKind::SyntheticStub,
    );
    registry.register(
        exec,
        "newCachedThreadPool",
        "()Ljava/util/concurrent/ExecutorService;",
        native_new_cached_pool, NativeKind::SyntheticStub,
    );
    registry.register(
        exec,
        "newCachedThreadPool",
        "(Ljava/util/concurrent/ThreadFactory;)Ljava/util/concurrent/ExecutorService;",
        native_new_cached_pool, NativeKind::SyntheticStub,
    );

    // ExecutorService methods
    registry.register(
        es,
        "submit",
        "(Ljava/lang/Runnable;)Ljava/util/concurrent/Future;",
        native_es_submit_runnable, NativeKind::SyntheticStub,
    );
    registry.register(
        es,
        "submit",
        "(Ljava/util/concurrent/Callable;)Ljava/util/concurrent/Future;",
        native_es_submit_callable, NativeKind::SyntheticStub,
    );
    // 2026-09-11, lane 5 residual §9.3 — the four
    // `java/util/concurrent/AbstractExecutorService` registrations that stood
    // here are DELETED, not retired.
    //
    // `submit(Runnable)`, `submit(Callable)`, `submit(Runnable, T)` and
    // `invokeAny(Collection)` were registered on an ABSTRACT class. A dispatch
    // door asks the registry about the DECLARING class of the resolved method,
    // and every concrete executor in the image — `ThreadPoolExecutor`,
    // `ForkJoinPool` — carries its own registration of the same names, so the
    // abstract one is never the answer. `L5ExecutorSweep.java`
    // builds the one receiver shape that could reach it (a direct subclass
    // declaring only `execute`) and the rows still read `invocations: 0`, on
    // every probe run and in all 132 `--real-jdk` corpus reports.
    //
    // Lane 0 §1: a door that never opens is dead weight, not a §1.4 shadow, so
    // this is a deletion and the triples are deliberately NOT in
    // `RETIRED_SHADOW_L5_TRIPLES` — a retirement table entry would claim a
    // dispatch nobody has ever observed.
    //
    // What still names this class: `native_es_submit_runnable` and
    // `native_es_submit_callable` (`lucene_es.rs`) and the two closures below
    // pass it to `invoke_special_bytecode_only` as the class whose BYTECODE to
    // run for a genuinely-real executor. That call does not consult the
    // registry, so it is unaffected by these deletions — and with them gone it
    // is now the only way this class name reaches dispatch, which is the state
    // those guards were written assuming.
    // JDK-ONLY-WAVE2 (2026-08-06): `native_es_execute` is adjudicated a
    // `SyntheticStub`, not the ambient kind this registrar would otherwise give
    // it. It is a compatibility stand-in for CratonVM's synthetic 2-field
    // `Executors.new*ThreadPool()` receiver shape, and once the real
    // `ThreadPoolExecutor.<init>` runs for every factory shortcut
    // (`initialize_real_thread_pool_executor`) there is no receiver left for it
    // to stand in for on a real-JDK image. The tag is what lets
    // `real_protected_stub_class` yield it to the real `execute()` bytecode
    // structurally, for every receiver, which is what replaced the eight
    // hand-written receiver-shape probes in `vm`. See
    // `jdk-only-wave2-threadpoolexecutor-execute-receiver-shape-RETIRED-20260806.md`.
    registry.register(
        es,
        "execute",
        "(Ljava/lang/Runnable;)V",
        native_es_execute,
        NativeKind::SyntheticStub,
    );
    registry.register(es, "shutdown", "()V", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(None),
        };
        // Real ThreadPoolExecutor: run the real bytecode so the pool's own
        // shutdown state machine transitions correctly, instead of writing
        // the synthetic 2-field placeholder's shutdown slot (would corrupt a
        // real field) or silently no-op'ing. See
        // `NativeContext::invoke_virtual_bytecode_only`'s doc for why this
        // can't just re-call `execute`/`shutdown` via `invoke_virtual` (would
        // recurse back into this same native).
        // Round 15 wave 7 (lane drainC): the receiver's own answer first
        // (`real_pool_shutdown`; a pool subclass declaring its own `e` was
        // taken for the placeholder and never shut down).
        if let Some(shut) = real_pool_shutdown(ctx, this) {
            // NOT invoke_virtual_bytecode_only: this native is ALSO reached
            // via ScheduledThreadPoolExecutor.shutdown()'s super.shutdown()
            // (invokespecial always checks the native registry first -- see
            // populate_invoke_cache), and that dynamic-receiver-class helper
            // would just re-find STPE's own overriding shutdown() again --
            // infinite recursion. invoke_special_bytecode_only resolves
            // statically on the NAMED class instead of the receiver's
            // dynamic class, so it lands on ThreadPoolExecutor's own body.
            shut?;        } else {
            ctx.set_field(this, EXEC_FIELD_SHUTDOWN, Value::Int(1));        }
        Ok(None)
    }, NativeKind::SyntheticStub);
    // Round 15 wave 6 (lane sweepC, `r15w5-threadvm2-review-findings` R1): a
    // real pool's `shutdownNow()` returns its drained tasks; the synthetic
    // body answers an empty list after a bare interrupt.
    registry.register(es, "shutdownNow", "()Ljava/util/List;", |ctx, args| {
        if let Some(Value::Object(Some(this))) = args.first() {
            if let Some(drained) = real_executor_shutdown_now(ctx, *this) {
                return drained;
            }
        }
        native_es_shutdown_now(ctx, args)
    }, NativeKind::SyntheticStub);
    registry.register(es, "isShutdown", "()Z", native_es_is_shutdown, NativeKind::SyntheticStub);
    registry.register(es, "isTerminated", "()Z", native_es_is_shutdown, NativeKind::SyntheticStub);
    registry.register(
        es,
        "awaitTermination",
        "(JLjava/util/concurrent/TimeUnit;)Z",
        native_es_await_termination, NativeKind::SyntheticStub,
    );

    // Also register on ThreadPoolExecutor
    registry.register(
        tp,
        "submit",
        "(Ljava/lang/Runnable;)Ljava/util/concurrent/Future;",
        native_es_submit_runnable, NativeKind::SyntheticStub,
    );
    registry.register(
        tp,
        "submit",
        "(Ljava/util/concurrent/Callable;)Ljava/util/concurrent/Future;",
        native_es_submit_callable, NativeKind::SyntheticStub,
    );
    // Same adjudication as the `ExecutorService` registration above — this is
    // the copy that matters, because `java/util/concurrent/ThreadPoolExecutor`
    // is the class whose real bytecode is always loaded and which
    // `real_protected_stub_class` therefore protects.
    registry.register(
        tp,
        "execute",
        "(Ljava/lang/Runnable;)V",
        native_es_execute,
        NativeKind::SyntheticStub,
    );
    registry.register(tp, "shutdown", "()V", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(None),
        };
        // Real ThreadPoolExecutor: run the real bytecode so the pool's own
        // shutdown state machine transitions correctly, instead of writing
        // the synthetic 2-field placeholder's shutdown slot (would corrupt a
        // real field) or silently no-op'ing. See
        // `NativeContext::invoke_virtual_bytecode_only`'s doc for why this
        // can't just re-call `execute`/`shutdown` via `invoke_virtual` (would
        // recurse back into this same native).
        // Round 15 wave 7 (lane drainC): the receiver's own answer first
        // (`real_pool_shutdown`; a pool subclass declaring its own `e` was
        // taken for the placeholder and never shut down).
        if let Some(shut) = real_pool_shutdown(ctx, this) {
            // NOT invoke_virtual_bytecode_only: this native is ALSO reached
            // via ScheduledThreadPoolExecutor.shutdown()'s super.shutdown()
            // (invokespecial always checks the native registry first -- see
            // populate_invoke_cache), and that dynamic-receiver-class helper
            // would just re-find STPE's own overriding shutdown() again --
            // infinite recursion. invoke_special_bytecode_only resolves
            // statically on the NAMED class instead of the receiver's
            // dynamic class, so it lands on ThreadPoolExecutor's own body.
            shut?;        } else {
            ctx.set_field(this, EXEC_FIELD_SHUTDOWN, Value::Int(1));        }
        Ok(None)
    }, NativeKind::SyntheticStub);

    // RD.6: submit(Runnable, T) — bind the given result.
    let submit_rt_closure = |ctx: &mut dyn NativeContext, args: &[Value]| -> MethodCallResult {
        // BUG-CCE-0716, and this overload was the one it MISSED. Its two
        // siblings -- `native_es_submit_runnable` and
        // `native_es_submit_callable` -- both delegate a genuinely-real
        // receiver to `AbstractExecutorService`'s own bytecode; this one ran
        // the task inline on the CALLING thread and handed back an
        // already-completed future, so a real `ThreadPoolExecutor` never saw
        // the task at all.
        //
        // The tell is the pool's own accounting, and `L5TpeCount.java`
        // reads it per submission shape on a 2-worker pool given 3 tasks and
        // then shut down and joined -- at which point every count is
        // determined:
        //
        //   shape                 HotSpot            CratonVM (before)
        //   execute               completed=3        completed=3
        //   submit(Callable)      completed=3        completed=3
        //   submit(Runnable)      completed=3        completed=3
        //   submit(Runnable, T)   completed=3        completed=0   <-- here
        //   invokeAll             completed=3        completed=3
        //   invokeAny             completed=1        completed=0   <-- and here
        //
        // A count is the mild half of it: the task also ran on the wrong
        // THREAD, which is the difference a start-gate `CountDownLatch`
        // deadlocks on (see `spawn_runnable_on_real_thread`'s note on the
        // eager-inline policy this is the last of).
        if let Some(Value::Object(Some(this))) = args.first() {
            if crate::executor_is_real(ctx, *this) {
                // gci i18/vp: a tail call of the same `submit` descriptor
                // (`continuation_pin::tail_call`): the body's `execute` can
                // park a virtual thread on the work queue's lock, which
                // unmounts on HotSpot (no native frame there).
                return cratonvm_native_api::continuation_pin::tail_call(|| {
                    ctx.invoke_special_bytecode_only(
                        "java/util/concurrent/AbstractExecutorService",
                        "submit",
                        "(Ljava/lang/Runnable;Ljava/lang/Object;)Ljava/util/concurrent/Future;",
                        args,
                    )
                });
            }
        }
        if let Some(Value::Object(Some(runnable))) = args.get(1) {
            ctx.invoke_virtual(*runnable, "run", "()V", &[])?;
        }
        let result_val = args.get(2).copied().unwrap_or(Value::Object(None));
        let future = completed_executor_future(ctx, result_val)?;
        Ok(Some(Value::Object(Some(future))))
    };
    registry.register(
        es,
        "submit",
        "(Ljava/lang/Runnable;Ljava/lang/Object;)Ljava/util/concurrent/Future;",
        submit_rt_closure, NativeKind::SyntheticStub,
    );
    registry.register(
        tp,
        "submit",
        "(Ljava/lang/Runnable;Ljava/lang/Object;)Ljava/util/concurrent/Future;",
        submit_rt_closure, NativeKind::SyntheticStub,
    );
    // See the §9.3 note above: the `AbstractExecutorService` copy of this
    // overload was deleted with the other three.

    // RD.6's `invokeAll(Collection<Callable>)` closure is gone (gc-common
    // w11-c). Nothing registered it -- `invokeAll` runs the real
    // `AbstractExecutorService` bytecode, which `L5TpeCount` measures equal to
    // HotSpot -- and it held its result list and the task collection as bare
    // copies across every `call()` and `CompletableFuture` construction, so it
    // could not have been registered as written.

    // RD.6: invokeAny(Collection<Callable>) — returns the first successful result.
    let invoke_any_closure = |ctx: &mut dyn NativeContext, args: &[Value]| -> MethodCallResult {
        let coll = match args.get(1) {
            Some(Value::Object(Some(c))) => *c,
            _ => return Ok(Some(Value::Object(None))),
        };
        // Same omission as `submit_rt_closure` above, and the same remedy: a
        // genuinely-real executor runs `AbstractExecutorService.invokeAny`,
        // so the task is submitted to the pool rather than called inline on
        // the caller. `L5TpeCount` reads `completed=0` against HotSpot's `1`
        // without this.
        if let Some(Value::Object(Some(this))) = args.first() {
            if crate::executor_is_real(ctx, *this) {
                // gci i18/vp: a tail call of the same `invokeAny` descriptor;
                // the body waits for the first result (an
                // `ExecutorCompletionService` poll / take, a park).
                return cratonvm_native_api::continuation_pin::tail_call(|| {
                    ctx.invoke_special_bytecode_only(
                        "java/util/concurrent/AbstractExecutorService",
                        "invokeAny",
                        "(Ljava/util/Collection;)Ljava/lang/Object;",
                        args,
                    )
                });
            }
        }
        let iter_val = ctx.invoke_virtual(coll, "iterator", "()Ljava/util/Iterator;", &[])?;
        let mut last_err: Option<cratonvm_types::error::MethodCallFailed> = None;
        if let Some(Value::Object(Some(iter))) = iter_val {
            // gc-common w11-c: `hasNext`, `next` and `call` each run bytecode (a
            // GC point), and the iterator is used across all of them. It was a
            // bare copy, so a collection inside one task's `call()` left the
            // next `hasNext` on a stale address. Root it; re-read per step.
            let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
            let iter_h = scope.root(iter);
            loop {
                let iter = scope.get(&iter_h);
                let has_next = scope.invoke_virtual(iter, "hasNext", "()Z", &[])?;
                if has_next != Some(Value::Int(1)) {
                    break;
                }
                let iter = scope.get(&iter_h);
                let callable_val = scope
                    .invoke_virtual(iter, "next", "()Ljava/lang/Object;", &[])?
                    .unwrap_or(Value::Object(None));
                if let Value::Object(Some(c)) = callable_val {
                    match scope.invoke_virtual(c, "call", "()Ljava/lang/Object;", &[]) {
                        Ok(r) => return Ok(Some(r.unwrap_or(Value::Object(None)))),
                        Err(e) => {
                            last_err = Some(e);
                        }
                    }
                }
            }
        }
        if let Some(e) = last_err {
            Err(e)
        } else {
            Err(RuntimeError::IllegalStateException {
                message: "ExecutionException: no task succeeded in invokeAny".to_string(),
            }
            .into())
        }
    };
    registry.register(
        es,
        "invokeAny",
        "(Ljava/util/Collection;)Ljava/lang/Object;",
        invoke_any_closure, NativeKind::SyntheticStub,
    );
    registry.register(
        tp,
        "invokeAny",
        "(Ljava/util/Collection;)Ljava/lang/Object;",
        invoke_any_closure, NativeKind::SyntheticStub,
    );
    // See the §9.3 note above: the `AbstractExecutorService` copy of
    // `invokeAny` was deleted with the other three.
}

/// Global-root handles of the `FutureTask`s [`spawn_runnable_on_real_thread`]
/// handed to the async worker pool, per VM, for
/// `ForkJoinPool.awaitQuiescence` ([`async_tasks_quiescent`]).
///
/// gc-common w19-b: this was ONE process-wide `Mutex<Vec<usize>>`. A handle
/// is an index into ITS VM's global-root table, so a second VM's
/// `awaitQuiescence` resolved the first VM's handles in its own table (another
/// object, or none) and, finding "it" done, RELEASED an unrelated global root
/// of its own. The lock was also held across `isDone()`, a Java call that can
/// reach a safepoint while another thread blocks on the same lock outside a
/// blocking region. Rows are per VM now, dropped at teardown
/// ([`forget_vm_async_future_roots`]), and `isDone()` runs with no lock held.
pub static ASYNC_FUTURE_ROOTS: cratonvm_native_api::vm_scoped::VmScoped<Vec<usize>> =
    cratonvm_native_api::vm_scoped::VmScoped::new();

/// The tracked-future count at which [`track_async_future`] first prunes the
/// completed ones; it prunes again each time the count reaches the next power
/// of two, so the pass is amortised O(1) per submission.
pub const ASYNC_FUTURE_PRUNE_MIN: usize = 64;

pub fn track_async_future(ctx: &mut dyn NativeContext, future: ObjectRef) {
    let handle = ctx.add_global_root(future);
    if handle == 0 {
        return;
    }
    let vm = ctx.vm_identity();
    let tracked = ASYNC_FUTURE_ROOTS.with(vm, |roots| {
        roots.push(handle);
        roots.len()
    });
    if tracked >= ASYNC_FUTURE_PRUNE_MIN && tracked.is_power_of_two() {
        prune_completed_async_futures(ctx, vm);
    }
}

/// Release the handles of tracked `FutureTask`s that have completed.
///
/// gc-common w21-a: the handles were released ONLY by
/// [`async_tasks_quiescent`] (`ForkJoinPool.awaitQuiescence`), which almost no
/// program calls, so every task this VM ever handed to the async worker pool
/// (`ForkJoinPool.execute(Runnable)`, `CompletableFuture.*Async` through it) --
/// and everything its `Runnable` captured -- stayed a global root for the life
/// of the VM. A completed task needs no tracking: `awaitQuiescence` only waits
/// for pending ones. "Completed" is `FutureTask.isDone()`'s own definition,
/// `state != NEW` (`NEW == 0`), read from the field so this runs no Java; a
/// task whose `state` cannot be read as an `int` is kept. Nothing is held
/// across a `ctx` call, and a handle is released only by the caller that took
/// it out of the row (a racing `awaitQuiescence` never releases one twice).
pub fn prune_completed_async_futures(ctx: &mut dyn NativeContext, vm: usize) {
    let handles: Vec<usize> = ASYNC_FUTURE_ROOTS
        .peek(vm, |roots| roots.clone())
        .unwrap_or_default();
    let done: rustc_hash::FxHashSet<usize> = handles
        .into_iter()
        .filter(|&handle| {
            ctx.resolve_global_root(handle).is_some_and(|future| {
                matches!(ctx.get_field_by_name(future, "state"), Value::Int(state) if state != 0)
            })
        })
        .collect();
    if done.is_empty() {
        return;
    }
    let released = ASYNC_FUTURE_ROOTS.with(vm, |roots| {
        let mut released = Vec::new();
        roots.retain(|h| {
            if done.contains(h) {
                released.push(*h);
                false
            } else {
                true
            }
        });
        released
    });
    for handle in released {
        ctx.remove_global_root(handle);
    }
}

/// Drop `vm`'s tracked `FutureTask` handles (VM teardown, from
/// `phases_late::concurrent::forget_vm_concurrent_tables`). The roots die with
/// the VM's own root table, so there is nothing to release.
pub fn forget_vm_async_future_roots(vm: usize) {
    ASYNC_FUTURE_ROOTS.forget(vm);
}

/// Whether every `FutureTask` this VM handed to a worker has completed; the
/// completed ones are released.
pub fn async_tasks_quiescent(ctx: &mut dyn NativeContext) -> bool {
    let vm = ctx.vm_identity();
    let handles: Vec<usize> = ASYNC_FUTURE_ROOTS
        .peek(vm, |roots| roots.clone())
        .unwrap_or_default();
    if handles.is_empty() {
        return true;
    }
    let mut done = Vec::new();
    for handle in handles {
        let is_done = ctx.resolve_global_root(handle).and_then(|future| {
            match ctx.invoke_virtual(future, "isDone", "()Z", &[]) {
                Ok(Some(Value::Int(d))) => Some(d != 0),
                _ => None,
            }
        });
        if is_done == Some(true) {
            done.push(handle);
        }
    }
    // Take out exactly the handles this call found done, so two concurrent
    // callers never release one root twice; anything still listed (including
    // a task tracked meanwhile) is pending.
    let (released, pending) = ASYNC_FUTURE_ROOTS.with(vm, |roots| {
        let mut released = Vec::new();
        roots.retain(|h| {
            if done.contains(h) {
                released.push(*h);
                false
            } else {
                true
            }
        });
        (released, !roots.is_empty())
    });
    for handle in released {
        ctx.remove_global_root(handle);
    }
    !pending
}

/// Bug D (kafka-suite-0617) — submit a `Runnable` to a real, **bounded** pool of
/// Java worker threads instead of running it inline on the calling thread.
///
/// The historical eager-inline policy (`runnable.run()` on the caller) was a
/// stopgap because CratonVM's *Rust* ForkJoinPool workers cannot run Java
/// bytecode. But it **deadlocks** whenever the submitted task blocks waiting for
/// a signal the *submitting* thread sends later — the canonical case is a
/// start-gate `CountDownLatch.await()` (the submitter `countDown()`s only after
/// submitting), and any `CompletableFuture.*Async` stage whose completer is the
/// submitter. The whole Kafka consumer/producer `internals` TIMEOUT cluster is
/// this pattern.
///
/// A naive "spawn one real thread per task" fix trades deadlock for **thread
/// explosion** (RecordHeadersTest spawned ~477 OS threads → starvation). The
/// robust fix mirrors HotSpot's common pool: a single bounded
/// `ThreadPoolExecutor` (cached-style — grows on demand to a cap, reuses idle
/// workers, retires them after a keep-alive so the VM still exits cleanly past
/// `main()` without daemon threads). `submit()` on a *real* TPE runs real JDK
/// bytecode on a real worker thread (only the synthetic-executor stubs and the
/// force-gated `ForkJoinPool.execute` are intercepted), so blocking tasks park a
/// worker while the submitter stays free.
///
/// This covers the executor-`execute(Runnable)` path only; the recursive
/// `ForkJoinTask.fork/invoke/submit(ForkJoinTask)` compute path stays
/// eager-inline (CPU-bound, must not spawn threads). If the pool can't be
/// created we fall back to inline so the completion contract still holds.
pub fn spawn_runnable_on_real_thread(
    ctx: &mut dyn NativeContext,
    runnable: ObjectRef,
) -> MethodCallResult {
    // Pin across the pool get-or-create below — the first call runs
    // <clinit>/<init> bytecode and allocates, and a moving young GC there
    // would relocate the runnable out from under this raw Rust local
    // (native stale-local family). Re-read via the pin before every use.
    let pin = ctx.pin_native_root(runnable);
    if let Some(pool) = async_worker_pool(ctx) {
        // Record before handing the task to the executor: a fast worker can
        // enter user code (and its short scheduling sleeps) before execute()
        // returns to the submitting thread.
        note_async_runnable_submitted();
        // gc-common w21-a: the pool is pinned too. `FutureTask.<init>` below
        // is Java, and `execute` used to be dispatched on the pool's pre-call
        // address; the inline fallback likewise re-reads the runnable after it.
        let pool_pin = ctx.pin_native_root(pool);
        // execute() is the ForkJoinPool.execute contract we are emulating here;
        // avoid wrapping every CompletableFuture async task in a FutureTask.
        let runnable_cur = ctx.read_native_pin(pin, runnable);
        let future = match ctx.new_object_initialized(
            "java/util/concurrent/FutureTask",
            "(Ljava/lang/Runnable;Ljava/lang/Object;)V",
            &[Value::Object(Some(runnable_cur)), Value::Object(None)],
        )? {
            Some(Value::Object(Some(future))) => future,
            _ => {
                let runnable_cur = ctx.read_native_pin(pin, runnable);
                ctx.unpin_native_roots(pin);
                return ctx.invoke_virtual(runnable_cur, "run", "()V", &[]);
            }
        };
        let future_pin = ctx.pin_native_root(future);
        let pool = ctx.read_native_pin(pool_pin, pool);
        let submitted = ctx.invoke_virtual(
            pool,
            "execute",
            "(Ljava/lang/Runnable;)V",
            &[Value::Object(Some(ctx.read_native_pin(future_pin, future)))],
        );
        let future = ctx.read_native_pin(future_pin, future);
        ctx.unpin_native_roots(future_pin);
        ctx.unpin_native_roots(pin);
        submitted?;
        track_async_future(ctx, future);
        let grace = async_submit_handoff_grace();
        if grace.is_zero() {
            std::thread::yield_now();
        } else {
            // Interpreter round i3 wave 81, lane L6: a hand-off grace HotSpot
            // does not take (its submitter is RUNNABLE): the VM's own wait.
            ctx.begin_vm_wait_region();
            std::thread::sleep(grace);
            ctx.end_blocking_region();
        }
        return Ok(None);
    }
    // Fallback: pool creation failed — preserve the completion contract inline.
    let runnable_cur = ctx.read_native_pin(pin, runnable);
    ctx.unpin_native_roots(pin);
    ctx.invoke_virtual(runnable_cur, "run", "()V", &[])?;
    Ok(None)
}

pub fn register_completable_future_natives(registry: &mut NativeMethodRegistry) {
    let cf = "java/util/concurrent/CompletableFuture";
    let fut = "java/util/concurrent/Future";
    let ft = "java/util/concurrent/FutureTask";

    // CompletableFuture constructors and factory
    registry.register(cf, "<init>", "()V", native_cf_init, NativeKind::SyntheticStub);
    registry.register(
        cf,
        "completedFuture",
        "(Ljava/lang/Object;)Ljava/util/concurrent/CompletableFuture;",
        native_cf_completed, NativeKind::SyntheticStub,
    );
    registry.register(
        cf,
        "supplyAsync",
        "(Ljava/util/function/Supplier;)Ljava/util/concurrent/CompletableFuture;",
        native_cf_supply_async, NativeKind::SyntheticStub,
    );

    // CompletableFuture instance methods
    registry.register(cf, "get", "()Ljava/lang/Object;", native_fut_get, NativeKind::SyntheticStub);
    registry.register(cf, "join", "()Ljava/lang/Object;", native_fut_get, NativeKind::SyntheticStub);
    registry.register(cf, "isDone", "()Z", native_fut_is_done, NativeKind::SyntheticStub);
    registry.register(cf, "complete", "(Ljava/lang/Object;)Z", native_cf_complete, NativeKind::SyntheticStub);
    registry.register(
        cf,
        "thenApply",
        "(Ljava/util/function/Function;)Ljava/util/concurrent/CompletableFuture;",
        native_cf_then_apply, NativeKind::SyntheticStub,
    );
    registry.register(
        cf,
        "thenAccept",
        "(Ljava/util/function/Consumer;)Ljava/util/concurrent/CompletableFuture;",
        native_cf_then_accept, NativeKind::SyntheticStub,
    );
    registry.register(
        cf,
        "getNow",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        native_cf_get_now, NativeKind::SyntheticStub,
    );

    // CompletableFuture cancel/isCancelled
    // done field: 0=pending, 1=normal, 2=exceptional, 3=cancelled
    registry.register(cf, "cancel", "(Z)Z", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Int(0))),
        };
        let done = match ctx.get_field(this, 1) {
            Value::Int(d) => d,
            _ => 0,
        };
        if done != 0 {
            return Ok(Some(Value::Int(0))); // already completed
        }
        ctx.set_field(this, 1, Value::Int(3)); // 3 = cancelled
        Ok(Some(Value::Int(1)))
    }, NativeKind::SyntheticStub);
    registry.register(cf, "isCancelled", "()Z", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Int(0))),
        };
        let done = match ctx.get_field(this, 1) {
            Value::Int(3) => 1,
            _ => 0,
        };
        Ok(Some(Value::Int(done)))    }, NativeKind::SyntheticStub);

    // Future interface methods
    registry.register(fut, "get", "()Ljava/lang/Object;", native_fut_get, NativeKind::SyntheticStub);
    registry.register(
        fut,
        "get",
        "(JLjava/util/concurrent/TimeUnit;)Ljava/lang/Object;",
        native_fut_get_timed, NativeKind::SyntheticStub,
    );
    registry.register(fut, "isDone", "()Z", native_fut_is_done, NativeKind::SyntheticStub);
    // Real cancel/isCancelled over the shared future layout (field
    // FUT_FIELD_DONE: 0=pending, 1=normal, 2=exceptional, 3=cancelled) — the
    // same encoding `cf.cancel`/`cf.isCancelled` above use. The previous
    // constant `false` made every `Future.cancel(..)` fail and every
    // `isCancelled()` deny a cancellation that had in fact happened, so
    // `while (!f.isCancelled())` loops never terminated.
    //
    // NOTE (registration order): `register_phase55_executors`
    // (phases_late/concurrent.rs) re-registers all three of
    // `java/util/concurrent/Future`'s isDone/isCancelled/cancel LATER in
    // `register_synthetic_overrides`, so on the interface triple that one wins.
    // These stay correct-by-construction in case that order changes.
    registry.register(fut, "cancel", "(Z)Z", native_fut_cancel, NativeKind::SyntheticStub);
    registry.register(fut, "isCancelled", "()Z", native_fut_is_cancelled, NativeKind::SyntheticStub);

    // FutureTask
    registry.register(ft, "get", "()Ljava/lang/Object;", native_fut_get, NativeKind::SyntheticStub);
    registry.register(
        ft,
        "get",
        "(JLjava/util/concurrent/TimeUnit;)Ljava/lang/Object;",
        native_fut_get_timed, NativeKind::SyntheticStub,
    );
    registry.register(ft, "isDone", "()Z", native_fut_is_done, NativeKind::SyntheticStub);
    // Synthetic FutureTask uses the same (result=0, done=1) layout — see the
    // `try_alloc_concurrent_synthetic(.., "java/util/concurrent/FutureTask", 2)?`
    // call sites in phases_late/net_channels.rs. Nothing else registers
    // FutureTask.cancel/isCancelled, so the old constant `false` pair was the
    // live answer: `FutureTask.cancel(true)` could never succeed and a
    // cancelled task still reported `isCancelled() == false`.
    registry.register(ft, "cancel", "(Z)Z", native_fut_cancel, NativeKind::SyntheticStub);
    registry.register(ft, "isCancelled", "()Z", native_fut_is_cancelled, NativeKind::SyntheticStub);

    // CompletableFuture timed get
    registry.register(
        cf,
        "get",
        "(JLjava/util/concurrent/TimeUnit;)Ljava/lang/Object;",
        native_fut_get_timed, NativeKind::SyntheticStub,
    );
}

/// `Future.cancel(boolean)` / `FutureTask.cancel(boolean)` over the shared
/// synthetic future layout. `FUT_FIELD_DONE` encodes 0=pending, 1=completed
/// normally, 2=completed exceptionally, 3=cancelled (the encoding
/// `CompletableFuture.cancel` already used). Per the `Future` contract this
/// returns false when the task has already completed and true once the task is
/// (or already was) cancelled.
pub fn native_fut_cancel(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let done = match ctx.get_field(this, FUT_FIELD_DONE) {
        Value::Int(d) => d,
        _ => 0,
    };
    if done == 3 {
        // Already cancelled — the JDK returns true for repeated cancel().
        return Ok(Some(Value::Int(1)));
    }
    if done != 0 {
        // Already completed normally/exceptionally: cannot cancel.
        return Ok(Some(Value::Int(0)));
    }
    ctx.set_field(this, FUT_FIELD_DONE, Value::Int(3));
    Ok(Some(Value::Int(1)))
}

/// `Future.isCancelled()` / `FutureTask.isCancelled()` — see
/// [`native_fut_cancel`] for the `FUT_FIELD_DONE` encoding.
pub fn native_fut_is_cancelled(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let cancelled = matches!(ctx.get_field(this, FUT_FIELD_DONE), Value::Int(3));
    Ok(Some(Value::Int(if cancelled { 1 } else { 0 })))
}

pub fn native_fut_get_timed(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // RD.6: timed Future.get. Our executors run synchronously so the future is
    // already done by the time .get is called. Still, we honour the timeout
    // contract and throw TimeoutException if it is somehow unset.
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let timeout_val = match args.get(1) {
        Some(Value::Long(v)) => *v,
        _ => 0,
    };
    let unit_ord = match args.get(2) {
        Some(Value::Object(Some(u))) => time_unit_ordinal(ctx, *u),
        _ => 2,
    };
    let timeout_ms = convert_time_unit_to_millis(timeout_val, unit_ord).max(0) as u64;
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    let this_pin = ctx.pin_native_root(this);
    loop {
        let mut this = ctx.read_native_pin(this_pin, this);
        let done = match ctx.get_field(this, FUT_FIELD_DONE) {
            Value::Int(d) => d,
            _ => 0,
        };
        if done != 0 && done != -1 {
            return native_fut_get(ctx, args);
        }
        if std::time::Instant::now() >= deadline {
            return Err(RuntimeError::IllegalStateException {
                message: "TimeoutException: future did not complete in time".to_string(),
            }
            .into());
        }
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        let wait_ms = remaining.as_millis().min(5).max(1) as u64;
        ctx.monitor_enter(this);
        monitor_wait_release(ctx, &mut this, Some(wait_ms))?;
    }
}

pub fn native_cf_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    ctx.set_field(this, FUT_FIELD_RESULT, Value::Object(None));
    ctx.set_field(this, FUT_FIELD_DONE, Value::Int(0));
    Ok(None)
}

/// Which fabricated `CompletableFuture` slot model a factory writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CfModel {
    /// `phases_late::concurrent`'s model: `result`@0, `done`@1, `exception`@2.
    Three,
    /// This file's model: `result`@0, `done`@1, and the deferred
    /// `exceptionally` stage's `source`@2 / `handler`@3.
    Four,
}

/// JIT round 23 wave 8 (lane W8-L2): the one door for a `CompletableFuture`
/// factory that writes a FABRICATED slot model past the real class's two
/// fields.
///
/// The real JDK 25 class declares exactly `result` and `stack`, and it is
/// floor-exempt (`classloading::FLOOR_EXEMPT_CLASSES`,
/// `CRATONVM_COMPLETABLE_FUTURE_COMPACT_LAYOUT`, default ON) so it keeps the
/// compact layout both JIT tiers need to inline `RESULT` / `STACK`. The wide
/// model is therefore allocated only on the fabricated stub, where it IS the
/// layout. Over a real class the object is the real width: the factories
/// that call this are `Bridge`s on `CompletableFuture` itself, which the
/// class's own bytecode outranks in both modes, so their slot-2/3 writes have
/// no real-class reader. With the switch at `0` the padded real class is four
/// wide again and the allocator's `max` gives these factories their old
/// object back.
#[track_caller]
pub fn try_alloc_completable_future_model(
    ctx: &mut dyn NativeContext,
    model: CfModel,
) -> Result<ObjectRef, MethodCallFailed> {
    if ctx.is_class_synthetic_stub("java/util/concurrent/CompletableFuture") {
        return match model {
            CfModel::Three => {
                try_alloc_concurrent_synthetic(ctx, "java/util/concurrent/CompletableFuture", 3)
            }
            CfModel::Four => {
                try_alloc_concurrent_synthetic(ctx, "java/util/concurrent/CompletableFuture", 4)
            }
        };
    }
    try_alloc_concurrent_synthetic(ctx, "java/util/concurrent/CompletableFuture", 2)
}

pub fn native_cf_completed(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let val = args.first().copied().unwrap_or(Value::Object(None));
    // JIT tier2 wave 4 (page agent `cf21`): over the REAL class (the
    // `--compatible` arm with `CRATONVM_COMPAT_JDK_COMPLETABLE_FUTURE=0`)
    // `result == null` means pending and the `done` write below lands on the
    // reference slot `stack` and is dropped, so `completedFuture(null)` owes
    // the JDK's `NIL`. Resolved before the value is pinned.
    let val = if crate::phases_late::concurrent::p58_cf_owes_nil(
        &val,
        true,
        ctx.is_class_synthetic_stub("java/util/concurrent/CompletableFuture"),
    ) {
        crate::phases_late::concurrent::p58_real_cf_nil(ctx)
    } else {
        val
    };
    // gc-common w20-g: the value is pinned across the future's allocation (it
    // was stored through its entry address).
    let val_pin = match val {
        Value::Object(Some(o)) => Some((ctx.pin_native_root(o), o)),
        _ => None,
    };
    let cf = try_alloc_completable_future_model(ctx, CfModel::Four);
    let val = match val_pin {
        Some((pin, o)) => Value::Object(Some(ctx.read_native_pin(pin, o))),
        None => val,
    };
    if let Some((pin, _)) = val_pin {
        ctx.unpin_native_roots(pin);
    }
    let cf = cf?;
    ctx.set_field(cf, FUT_FIELD_RESULT, val);
    ctx.set_field(cf, FUT_FIELD_DONE, Value::Int(1));
    Ok(Some(Value::Object(Some(cf))))
}

pub fn native_cf_supply_async(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // gc-common w20-g: `supplier.get()` is Java and can collect; the result
    // was stored through the future's pre-call address. The supplier (read
    // before the future's allocation, the receiver of `get` after it) and the
    // future are both pinned.
    let supplier = match args.first() {
        Some(Value::Object(Some(s))) => Some(*s),
        _ => None,
    };
    let supplier_pin = supplier.map(|s| (ctx.pin_native_root(s), s));
    let cf = try_alloc_completable_future_model(ctx, CfModel::Four);
    let supplier = supplier_pin.map(|(pin, s)| ctx.read_native_pin(pin, s));
    let cf = match cf {
        Ok(cf) => cf,
        Err(e) => {
            if let Some((pin, _)) = supplier_pin {
                ctx.unpin_native_roots(pin);
            }
            return Err(e);
        }
    };
    let cf_pin = ctx.pin_native_root(cf);
    let result = match supplier {
        Some(supplier) => ctx.invoke_virtual(supplier, "get", "()Ljava/lang/Object;", &[]),
        None => Ok(None),
    };
    let cf = ctx.read_native_pin(cf_pin, cf);
    match supplier_pin {
        Some((pin, _)) => ctx.unpin_native_roots(pin),
        None => ctx.unpin_native_roots(cf_pin),
    }
    if supplier.is_some() {
        let result = result?;
        ctx.set_field(cf, FUT_FIELD_RESULT, result.unwrap_or(Value::Object(None)));
    }
    ctx.set_field(cf, FUT_FIELD_DONE, Value::Int(1));
    Ok(Some(Value::Object(Some(cf))))
}

pub fn native_fut_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    // Check for deferred exceptionally stage (done == -1)
    let done = match ctx.get_field(this, FUT_FIELD_DONE) {
        Value::Int(d) => d,
        _ => 0,
    };
    if done == -1 {
        // Deferred: check source CF and invoke handler if source is exceptionally completed
        let source = match ctx.get_field(this, 2) {
            // CF_FIELD_SOURCE
            Value::Object(Some(s)) => s,
            _ => return Ok(Some(Value::Object(None))),
        };
        let source_done = match ctx.get_field(source, FUT_FIELD_DONE) {
            Value::Int(d) => d,
            _ => 0,
        };
        if source_done == 2 {
            // Exceptionally completed — invoke the handler
            let handler = match ctx.get_field(this, 3) {
                // CF_FIELD_HANDLER
                Value::Object(Some(h)) => h,
                _ => return Ok(Some(Value::Object(None))),
            };
            let exc = ctx.get_field(source, FUT_FIELD_RESULT);
            // gc-common w20-g: the handler is Java and can collect; its result
            // was cached through `this`'s entry address. Pinned across it.
            let this_pin = ctx.pin_native_root(this);
            let result = ctx.invoke_virtual(
                handler,
                "apply",
                "(Ljava/lang/Object;)Ljava/lang/Object;",
                &[exc],
            );
            let this = ctx.read_native_pin(this_pin, this);
            ctx.unpin_native_roots(this_pin);
            let val = result?.unwrap_or(Value::Object(None));
            // Cache the result
            ctx.set_field(this, FUT_FIELD_RESULT, val.clone());
            ctx.set_field(this, FUT_FIELD_DONE, Value::Int(1));
            return Ok(Some(val));
        } else if source_done != 0 {
            // Source completed normally — pass through result
            let val = ctx.get_field(source, FUT_FIELD_RESULT);
            ctx.set_field(this, FUT_FIELD_RESULT, val.clone());
            ctx.set_field(this, FUT_FIELD_DONE, Value::Int(source_done));
            return Ok(Some(val));
        }
        // Source still pending — return null for now
        return Ok(Some(Value::Object(None)));
    }
    Ok(Some(ctx.get_field(this, FUT_FIELD_RESULT)))
}

pub fn native_fut_is_done(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let done = match ctx.get_field(this, FUT_FIELD_DONE) {
        Value::Int(d) => d,
        _ => 0,
    };
    if done == -1 {
        // Deferred: check if source is done
        let source_done = match ctx.get_field(this, 2) {
            Value::Object(Some(s)) => match ctx.get_field(s, FUT_FIELD_DONE) {
                Value::Int(d) if d != 0 => 1,
                _ => 0,            },
            _ => 0,
        };
        return Ok(Some(Value::Int(source_done)));    }
    Ok(Some(Value::Int(if done != 0 { 1 } else { 0 })))}

/// `CompletableFuture.complete(value)` — handles BOTH the synthetic side-field
/// model and a real-JDK `java.util.concurrent.CompletableFuture`.
///
/// CRITICAL: the synthetic override only flips the side fields (`result`@0,
/// `done`@1) and historically never ran `postComplete()`. For a *real-JDK*
/// `CompletableFuture` (allocated by the genuine bytecode `<init>`, whose
/// instance layout is `result`@0 + the lock-free `stack`@1), a thread blocked in
/// the real `waitingGet()` (untimed `get()`/`join()`) pushes a `Signaller` onto
/// `stack`@1 and parks via `LockSupport.park`. The synthetic override (a) never
/// fired that Signaller (so the parked thread was never `unpark`ed → indefinite
/// hang on every cross-thread `complete()` after the waiter blocks — the timed
/// `get(...)` only "self-heals" because `parkNanos` re-polls `result`), and
/// (b) clobbered `stack`@1 with the `done` int. Distinguish the two layouts by
/// slot-1's value type: a real-JDK CF has an Object `stack` (null or a Signaller
/// chain), a synthetic CF has an Int `done`. For the real-JDK object delegate to
/// the genuine `completeValue` + `postComplete` so waiters are released.
pub fn native_cf_complete(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let val = args.get(1).copied().unwrap_or(Value::Object(None));
    match ctx.get_field(this, FUT_FIELD_DONE) {
        // Synthetic CF model — `done` flag lives in slot 1.
        Value::Int(done) => {
            if done != 0 {
                return Ok(Some(Value::Int(0))); // already completed
            }
            ctx.set_field(this, FUT_FIELD_RESULT, val);
            ctx.set_field(this, FUT_FIELD_DONE, Value::Int(1));
            Ok(Some(Value::Int(1)))
        }
        // Real-JDK CompletableFuture — slot 1 is the `stack` (Completion) field.
        // Run the genuine completion path so `postComplete()` fires any parked
        // `Signaller` (i.e. `LockSupport.unpark` the thread blocked in `get()`).
        _ => {
            // gc-common w19-b: `completeValue` is Java and can collect; the
            // future is pinned across it and re-read for `postComplete`,
            // which was called on its earlier address.
            let this_pin = ctx.pin_native_root(this);
            let triggered = ctx
                .invoke_virtual(this, "completeValue", "(Ljava/lang/Object;)Z", &[val])?
                .and_then(|v| v.as_int())
                .unwrap_or(0);
            let this = ctx.read_native_pin(this_pin, this);
            ctx.unpin_native_roots(this_pin);
            // `invoke_virtual_bytecode_only`, not `invoke_virtual`:
            // `postComplete` is ordinary JDK bytecode and can never be a
            // native, so asking the by-NAME native resolver is two hashes of
            // the 53-byte triple plus the cold descriptor-quirk rewrite, all
            // of them misses, on every completion. Measured on the sibling
            // call in `native_cf_complete`: 1.154x of the whole composition
            // workload, and it takes the registry's own miss census from
            // 100 008 rows for this one triple to zero. This site is not on
            // that measured path, and is changed with it because a fix that
            // lands for one of five identical call sites is at the wrong
            // level.
            ctx.invoke_virtual_bytecode_only(this, "postComplete", "()V", &[])?;
            Ok(Some(Value::Int(triggered)))
        }
    }
}

pub fn native_cf_then_apply(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let func = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    if !matches!(ctx.get_field(this, FUT_FIELD_DONE), Value::Int(_)) {
        return ctx.invoke_special(
            "java/util/concurrent/CompletableFuture",
            "uniApplyStage",
            "(Ljava/util/concurrent/Executor;Ljava/util/function/Function;)Ljava/util/concurrent/CompletableFuture;",
            &[
                Value::Object(Some(this)),
                Value::Object(None),
                Value::Object(Some(func)),
            ],
        );
    }
    let done = match ctx.get_field(this, FUT_FIELD_DONE) {
        Value::Int(d) => d,
        _ => 0,
    };
    // gc-common w13-f: the new stage's allocation is a GC point after which
    // the source future (`this`) and the Function were used through their
    // `args` addresses, and `apply` is Java after which the new stage itself
    // was written through the address its allocation returned. All three are
    // rooted and re-read after each GC point.
    let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
    let this_h = scope.root(this);
    let func_h = scope.root(func);
    let cf_new = try_alloc_completable_future_model(&mut *scope, CfModel::Four)?;
    let cf_h = scope.root(cf_new);
    let this = scope.get(&this_h);
    let val = scope.get_field(this, FUT_FIELD_RESULT);
    // RD.8: propagate exceptional/cancelled state without invoking the Function.
    if done == 2 || done == 3 {
        let cf = scope.get(&cf_h);
        scope.set_field(cf, FUT_FIELD_RESULT, val);
        scope.set_field(cf, FUT_FIELD_DONE, Value::Int(done));
        return Ok(Some(Value::Object(Some(cf))));
    }
    let func = scope.get(&func_h);
    let applied = scope.invoke_virtual(
        func,
        "apply",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        &[val],
    );
    let (result, state) = match applied {
        Ok(result) => (result.unwrap_or(Value::Object(None)), 1),
        Err(e) => (cf_capture_throwable(&mut *scope, &e), 2),
    };
    let cf = scope.get(&cf_h);
    scope.set_field(cf, FUT_FIELD_RESULT, result);
    scope.set_field(cf, FUT_FIELD_DONE, Value::Int(state));
    Ok(Some(Value::Object(Some(cf))))
}

pub fn native_cf_then_accept(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let consumer = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    if !matches!(ctx.get_field(this, FUT_FIELD_DONE), Value::Int(_)) {
        return ctx.invoke_special(
            "java/util/concurrent/CompletableFuture",
            "uniAcceptStage",
            "(Ljava/util/concurrent/Executor;Ljava/util/function/Consumer;)Ljava/util/concurrent/CompletableFuture;",
            &[
                Value::Object(Some(this)),
                Value::Object(None),
                Value::Object(Some(consumer)),
            ],
        );
    }
    let done = match ctx.get_field(this, FUT_FIELD_DONE) {
        Value::Int(d) => d,
        _ => 0,
    };
    // gc-common w13-f: rooted across the allocation and `accept`, exactly as
    // in `native_cf_then_apply`.
    let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
    let this_h = scope.root(this);
    let consumer_h = scope.root(consumer);
    let cf_new = try_alloc_completable_future_model(&mut *scope, CfModel::Four)?;
    let cf_h = scope.root(cf_new);
    let this = scope.get(&this_h);
    let val = scope.get_field(this, FUT_FIELD_RESULT);
    if done == 2 || done == 3 {
        let cf = scope.get(&cf_h);
        scope.set_field(cf, FUT_FIELD_RESULT, val);
        scope.set_field(cf, FUT_FIELD_DONE, Value::Int(done));
        return Ok(Some(Value::Object(Some(cf))));
    }
    let consumer = scope.get(&consumer_h);
    let accepted = scope.invoke_virtual(consumer, "accept", "(Ljava/lang/Object;)V", &[val]);
    let (result, state) = match accepted {
        Ok(_) => (Value::Object(None), 1),
        Err(e) => (cf_capture_throwable(&mut *scope, &e), 2),
    };
    let cf = scope.get(&cf_h);
    scope.set_field(cf, FUT_FIELD_RESULT, result);
    scope.set_field(cf, FUT_FIELD_DONE, Value::Int(state));
    Ok(Some(Value::Object(Some(cf))))
}

pub fn native_cf_get_now(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let done = match ctx.get_field(this, FUT_FIELD_DONE) {
        Value::Int(d) => d,
        _ => 0,
    };
    if done != 0 {
        Ok(Some(ctx.get_field(this, FUT_FIELD_RESULT)))
    } else {
        Ok(Some(args.get(1).copied().unwrap_or(Value::Object(None))))
    }
}

/// Is `executor` ITSELF a real `ThreadPoolExecutor`: its own `workers` set,
/// and a class assignable to the JDK's? `executor_has_real_workers` also
/// answers yes for a delegate wrapper through its `e` field, and the
/// synthetic placeholders carry the `ThreadPoolExecutor` class name with no
/// `workers`; running `ThreadPoolExecutor`'s bytecode on either would read
/// fields of the wrong layout, so the real-body doors below ask this
/// narrower question (round 15 wave 6, lane sweepC).
pub fn is_real_thread_pool_executor(
    ctx: &mut dyn NativeContext,
    executor: ObjectRef,
) -> bool {
    if !matches!(
        ctx.get_field_by_name(executor, "workers"),
        Value::Object(Some(_))
    ) {
        return false;
    }
    let class_id = ctx.class_id_of_object(executor);
    ctx.class_assignable_to_name(class_id, "java/util/concurrent/ThreadPoolExecutor") == Some(true)
}

/// What a real-pool door runs on ([`real_pool_target`]).
pub enum RealPoolTarget {
    /// The receiver is a real pool: `ThreadPoolExecutor`'s own body on it.
    Own,
    /// A delegate wrapper whose `e` is a real pool: the wrapper's JDK body,
    /// `e.m(..)`, a virtual call on the pool.
    Delegate(ObjectRef),
    /// CratonVM's synthetic placeholder: its slots.
    Synthetic,
}

/// Round 15 wave 7 (lane drainC; `r15w6-sweepC-review-findings` S1):
/// `executor_has_real_workers` follows a field named `e` BEFORE looking at the
/// receiver, so a real pool subclass declaring its own `e` that holds no pool
/// was taken for the synthetic placeholder (its `shutdown()` wrote the
/// synthetic flag into a real field and never shut the pool down, so its idle
/// non-daemon workers held the VM's exit), and a wrapper whose `e` is a pool
/// had `ThreadPoolExecutor` bytecode run on the WRAPPER. The receiver's own
/// answer ([`is_real_thread_pool_executor`]) now comes first, and a wrapper
/// delegates. Without a non-null `e` the answer is exactly
/// `executor_has_real_workers`'s; `CRATONVM_TPE_DOORS_SELF_FIRST=0` (read only
/// when the receiver has one) restores it everywhere. `native-collections`'
/// `tp_target` is the same rule (that crate cannot see this one).
pub fn real_pool_target(ctx: &mut dyn NativeContext, executor: ObjectRef) -> RealPoolTarget {
    match ctx.get_field_by_name(executor, "e") {
        Value::Object(Some(pool))
            if crate::nbflags().tpe_doors_self_first =>
        {
            if is_real_thread_pool_executor(ctx, executor) {
                RealPoolTarget::Own
            } else if is_real_thread_pool_executor(ctx, pool) {
                RealPoolTarget::Delegate(pool)
            } else {
                RealPoolTarget::Synthetic
            }
        }
        _ if crate::executor_has_real_workers(ctx, executor) => RealPoolTarget::Own,
        _ => RealPoolTarget::Synthetic,
    }
}

/// `shutdown()` behind the `ThreadPoolExecutor` / `ExecutorService` /
/// JULI bridges ([`real_pool_target`]): a real pool's own shutdown
/// ([`transition_real_executor_to_shutdown`]), a wrapper's `e.shutdown()`, or
/// `None` for the synthetic placeholder (the caller writes its flag).
pub fn real_pool_shutdown(
    ctx: &mut dyn NativeContext,
    executor: ObjectRef,
) -> Option<MethodCallResult> {
    match real_pool_target(ctx, executor) {
        RealPoolTarget::Own => Some(transition_real_executor_to_shutdown(ctx, executor)),
        // gci i18/vp: every caller (the `ExecutorService` / `ThreadPoolExecutor`
        // / JULI `shutdown()V` bridges) answers this `()V` call's result as
        // its own (`shut?; Ok(None)`), so the wrapper's `shutdown()`, which
        // takes the pool's `mainLock` (a park when contended), is a tail call.
        RealPoolTarget::Delegate(pool) => Some(cratonvm_native_api::continuation_pin::tail_call(
            || ctx.invoke_virtual(pool, "shutdown", "()V", &[]),
        )),
        RealPoolTarget::Synthetic => None,
    }
}

/// `shutdownNow()` of a real `ThreadPoolExecutor`: its own body (`mainLock`,
/// `interruptWorkers`, `drainQueue`), which returns the drained tasks, as
/// `native-collections`' `native_tp_shutdown_now` already does. `None` for
/// any other receiver, or under `CRATONVM_TPE_SHUTDOWN_REAL_BODY=0`.
/// Static binding on the named class, so an STPE's `super.shutdownNow()`
/// cannot re-enter the native that called this.
pub fn real_executor_shutdown_now(
    ctx: &mut dyn NativeContext,
    executor: ObjectRef,
) -> Option<MethodCallResult> {
    if !crate::nbflags().tpe_shutdown_real_body
        || !is_real_thread_pool_executor(ctx, executor)
    {
        return None;
    }
    // gci i18/vp: both callers (the `ExecutorService` and `ThreadPoolExecutor`
    // `shutdownNow()` bridges) return this answer as their own, same
    // descriptor: a tail call. The body takes `mainLock` (a park when
    // contended), which unmounts on HotSpot.
    Some(cratonvm_native_api::continuation_pin::tail_call(|| {
        ctx.invoke_special_bytecode_only(
            "java/util/concurrent/ThreadPoolExecutor",
            "shutdownNow",
            "()Ljava/util/List;",
            &[Value::Object(Some(executor))],
        )
    }))
}

pub fn transition_real_executor_to_shutdown(
    ctx: &mut dyn NativeContext,
    executor: ObjectRef,
) -> MethodCallResult {
    // Round 15 wave 6 (lane sweepC, TV2-1 of `jit-r15w5-threadvm2-proposals-RETIRED-20261003.md`):
    // run `ThreadPoolExecutor.shutdown()`'s own body. JDK 25's is `mainLock`
    // { advanceRunState(SHUTDOWN); interruptIdleWorkers(); onShutdown(); }
    // then `tryTerminate()`; the step-by-step copy below walked `workers`
    // without `mainLock` (`r15w5-threadvm2-review-findings` R2: a worker
    // exiting concurrently made the walk stop early) and ran `onShutdown`
    // unlocked. `invoke_special_bytecode_only` binds statically on the named
    // class and skips the native registry, so neither this bridge nor an
    // STPE's `super.shutdown()` is re-entered; `--real-jdk` already runs this
    // body for every pool. `CRATONVM_TPE_SHUTDOWN_REAL_BODY=0` keeps the copy.
    if crate::nbflags().tpe_shutdown_real_body
        && is_real_thread_pool_executor(ctx, executor)
    {
        // gci i18/vp: a tail call. Every caller is a `shutdown()V` bridge
        // that answers `Ok(None)` after it, or propagates its `Err` (a yield
        // included) with `?` -- for a `void` method the identity. The body
        // takes `mainLock`, a park when contended.
        return cratonvm_native_api::continuation_pin::tail_call(|| {
            ctx.invoke_special_bytecode_only(
                "java/util/concurrent/ThreadPoolExecutor",
                "shutdown",
                "()V",
                &[Value::Object(Some(executor))],
            )
        });
    }
    // This bridge calls into Java several times (AtomicInteger, the executor
    // hooks, and finally ThreadPoolExecutor.tryTerminate).  Keep both the
    // receiver and `ctl` rooted across those calls; otherwise a moving
    // collection can leave the final invokespecial targeting a stale
    // executor, observed as an intermittent `this.ctl == null` while JUnit
    // closes its ScheduledThreadPoolExecutor timeout resource.
    let executor_pin = ctx.pin_native_root(executor);
    let executor = ctx.read_native_pin(executor_pin, executor);
    let Value::Object(Some(ctl)) = ctx.get_field_by_name(executor, "ctl") else {
        ctx.unpin_native_roots(executor_pin);
        return Ok(None);
    };
    let ctl_pin = ctx.pin_native_root(ctl);
    let result = (|| {
        // Round 15 wave 5 (lane threadvm2): `advanceRunState(SHUTDOWN)`, a CAS
        // loop that only moves the state up. A plain `get` then `set` lost a
        // worker-count change racing it (an `addWorker` increment or an idle
        // timeout's decrement: a count left too low wraps into the run-state
        // bits on the last exit and reads RUNNING again, so the remaining
        // workers never leave `getTask()`), and on a pool already STOP /
        // TERMINATED it moved the state back to SHUTDOWN, so a second
        // `shutdown()` ran `terminated()` again. `CRATONVM_TPE_SHUTDOWN_CAS_ADVANCE=0`
        // restores the plain write.
        let cas_advance = crate::nbflags().tpe_shutdown_cas_advance;
        for _ in 0..4096 {
            let ctl = ctx.read_native_pin(ctl_pin, ctl);
            let current = match ctx.invoke_virtual(ctl, "get", "()I", &[])? {
                Some(Value::Int(value)) => value,
                _ => return Ok(None),
            };
            // ThreadPoolExecutor packs run state in the high 3 bits and worker
            // count below. RUNNING is the only negative state and SHUTDOWN is
            // run-state 0, so retain only the worker count bits.
            if cas_advance && current >= 0 {
                break;
            }
            let shutdown = current & 0x1fff_ffff;
            let ctl = ctx.read_native_pin(ctl_pin, ctl);
            if !cas_advance {
                let _ = ctx.invoke_virtual(ctl, "set", "(I)V", &[Value::Int(shutdown)]);
                break;
            }
            if let Some(Value::Int(1)) = ctx.invoke_virtual(
                ctl,
                "compareAndSet",
                "(II)Z",
                &[Value::Int(current), Value::Int(shutdown)],
            )? {
                break;
            }
        }
        // A graceful ThreadPoolExecutor shutdown must wake IDLE workers so
        // they observe SHUTDOWN and leave getTask().  Merely updating ctl
        // leaks every worker blocked in LinkedBlockingQueue.take().
        //
        // Only the idle ones: this is `shutdown()`, not `shutdownNow()`, and a
        // worker in the middle of a task must be allowed to finish it. See
        // `interrupt_executor_workers_filtered`.
        let executor = ctx.read_native_pin(executor_pin, executor);
        let _ =
            crate::interrupt_executor_workers_filtered(ctx, executor, /* only_idle */ true);
        // A ScheduledThreadPoolExecutor owns delayed tasks in its work queue.
        // Its real `onShutdown()` removes cancelled delayed tasks (including
        // JUnit's cancelled timeout watchdog); without it, the queue stays
        // nonempty until the original timeout expires and awaitTermination()
        // cannot finish.
        let executor = ctx.read_native_pin(executor_pin, executor);
        let _ = ctx.invoke_virtual_bytecode_only(executor, "onShutdown", "()V", &[])?;
        // A zero-worker pool has no worker-exit path to call tryTerminate(),
        // so finalize it explicitly after the shutdown hook.
        let executor = ctx.read_native_pin(executor_pin, executor);
        let _ = ctx.invoke_special_bytecode_only(
            "java/util/concurrent/ThreadPoolExecutor",
            "tryTerminate",
            "()V",
            &[Value::Object(Some(executor))],
        )?;
        Ok(None)
    })();
    ctx.unpin_native_roots(executor_pin);
    result
}

pub fn native_sr_next_int(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let seed = sr_next_seed(ctx, this);
    Ok(Some(Value::Int((seed >> 16) as i32)))
}

pub fn native_sr_next_int_bound(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let bound = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 1,
    };
    if bound <= 0 {
        return Ok(Some(Value::Int(0)));
    }
    let seed = sr_next_seed(ctx, this);
    Ok(Some(Value::Int(
        ((seed >> 16) as i32).unsigned_abs() as i32 % bound,
    )))
}

pub fn native_sr_next_long(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Long(0))),
    };
    let seed = sr_next_seed(ctx, this);
    Ok(Some(Value::Long(seed)))
}

pub fn native_sr_next_bytes(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let _this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let arr = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let len = ctx.array_length(arr);
    if len == 0 {
        return Ok(None);
    }
    // Fill the buffer in a single bulk call to the OS CSPRNG instead of
    // burning a 64-bit random draw per byte (which discarded 7/8 of each
    // call's entropy). If the CSPRNG is unavailable we fall back to
    // per-byte `sr_os_random_u64` mixing so we never silently produce
    // deterministic bytes.
    let mut buf = vec![0u8; len];
    if !sr_os_random_bytes(&mut buf) {
        for chunk in buf.chunks_mut(8) {
            let v = sr_os_random_u64().to_le_bytes();
            for (dst, src) in chunk.iter_mut().zip(v.iter()) {
                *dst = *src;
            }
        }
    }
    for (i, &b) in buf.iter().enumerate() {
        ctx.set_array_element(arr, i, Value::Int((b as i8) as i32));
    }
    // Zero the intermediate buffer — cheap defence in depth even though
    // the heap copy in `set_array_element` is already the authoritative
    // destination.
    for b in buf.iter_mut() {
        *b = 0;
    }
    Ok(None)
}

pub fn native_sr_next_double(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Double(0.0))),
    };
    let seed = sr_next_seed(ctx, this);
    let d = ((seed >> 16) as u32 as f64) / (u32::MAX as f64);
    Ok(Some(Value::Double(d)))
}

pub fn native_sr_next_boolean(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let seed = sr_next_seed(ctx, this);
    Ok(Some(Value::Int(if seed & 1 == 0 { 0 } else { 1 })))
}

pub fn native_sr_generate_seed(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let num_bytes = match args.get(1) {
        Some(Value::Int(v)) => *v as usize,
        _ => 0,
    };
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, num_bytes);
    // Use OS entropy directly (CSPRNG) for seed generation.
    let mut buf = vec![0u8; num_bytes];
    if !sr_os_random_bytes(&mut buf) {
        // Fallback: use system_entropy_seed per-byte
        for (i, b) in buf.iter_mut().enumerate() {
            *b = (system_entropy_seed() >> (i % 8 * 8)) as u8;
        }
    }
    for (i, &b) in buf.iter().enumerate() {
        ctx.set_array_element(arr, i, Value::Int(b as i8 as i32));
    }
    Ok(Some(Value::Object(Some(arr))))
}

// ===========================================================================
// Phase 34: ReadWriteLock, Atomic extras, LongAdder
// ===========================================================================

// ---------------------------------------------------------------------------
// AbstractQueuedLongSynchronizer state stand-ins (the `JdkAqls` `=0` arm)
// ---------------------------------------------------------------------------
//
// These three natives keep the synchronizer's word in the object's own
// `private volatile long state` field, through the VM's field accessors, which
// is where the JDK 25 bodies keep it. Until interpreter round i3 wave 73
// (lane L1, `i72-L1-proposal-retire-the-aqls-side-table-20261105`) they kept
// it in a process-global `Mutex<HashMap<weak lock key, Arc<AtomicI64>>>`:
// compatibility state shared by every VM in the process (AGENTS.md: "No
// process globals for compatibility state"), and a word that a reflective,
// `VarHandle` or serialization read of `state` never saw. The premise that
// table was built on (the generic `Unsafe` CAS cannot hold a 64-bit word) does
// not hold for the field CAS: `compare_and_swap_field` with `Value::Long` is
// the path `AtomicLongFieldUpdater.compareAndSet` takes
// (`atomic_updater::native_alfu_compare_and_set`).
pub const AQLS_CLASS: &str = "java/util/concurrent/locks/AbstractQueuedLongSynchronizer";

/// The slot of AQLS's own `state` field. Resolved against the DECLARING
/// class, not the receiver's, so a subclass that declares a field of the same
/// name cannot capture the lookup (a superclass's instance slots are a prefix
/// of every subclass's layout). Resolved per call and never cached: the
/// stand-ins serve only the `CRATONVM_COMPAT_JDK_AQLS=0` arm, and a slot index
/// kept in a process global is what `NativeContext::throwable_field_index`'s
/// contract forbids (another VM can have another layout).
pub fn aqls_state_field_slot(ctx: &dyn NativeContext) -> Result<usize, MethodCallFailed> {
    ctx.resolve_field_index(AQLS_CLASS, "state").ok_or_else(|| {
        MethodCallFailed::InternalError(cratonvm_types::error::VmError::Linkage(
            cratonvm_types::error::LinkageError::NoSuchFieldError {
                class_name: AQLS_CLASS.to_string(),
                field_name: "state".to_string(),
                field_descriptor: "J".to_string(),
            },
        ))
    })
}

/// A `long` slot read as a `long`. A slot nothing has typed yet (an
/// `Int(0)`-initialised mock slot) reads as the field's default, zero.
pub fn aqls_long_of(value: Value) -> i64 {
    match value {
        Value::Long(v) => v,
        Value::Int(v) => i64::from(v),
        _ => 0,
    }
}

pub fn native_aqls_get_state(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(Value::Object(Some(synchronizer))) = args.first() else {
        return Ok(Some(Value::Long(0)));
    };
    let slot = aqls_state_field_slot(ctx)?;
    let state = aqls_long_of(ctx.get_field_volatile(*synchronizer, slot));
    Ok(Some(Value::Long(state)))
}

pub fn native_aqls_set_state(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let (Some(Value::Object(Some(synchronizer))), Some(Value::Long(state))) =
        (args.first(), args.get(1))
    else {
        return Ok(None);
    };
    let slot = aqls_state_field_slot(ctx)?;
    ctx.set_field_volatile(*synchronizer, slot, Value::Long(*state));
    Ok(None)
}

pub fn native_aqls_compare_and_set_state(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let (
        Some(Value::Object(Some(synchronizer))),
        Some(Value::Long(expected)),
        Some(Value::Long(new_state)),
    ) = (args.first(), args.get(1), args.get(2))
    else {
        return Ok(Some(Value::Int(0)));
    };
    let slot = aqls_state_field_slot(ctx)?;
    let swapped = ctx.compare_and_swap_field(
        *synchronizer,
        slot,
        Value::Long(*expected),
        Value::Long(*new_state),
    );
    Ok(Some(Value::Int(i32::from(swapped))))
}

/// OVER A REAL JDK, `--compatible` REGISTERS NONE OF THESE by default since
/// interpreter round i3 wave 72, lane L1: the three triples are retired
/// shadows, and the `JdkAqls` HotSpot-alignment row
/// (`CRATONVM_COMPAT_JDK_AQLS`, `=0` kill switch) names the class in a
/// `retired_shadow::COMPAT_FAMILIES` entry, so `NativeMethodRegistry::register`
/// drops them as `--real-jdk` refuses them, and the JDK 25 bodies keep the
/// word in the real `state` field. (These stand-ins were once written for
/// `ReentrantReadWriteLock`; in JDK 25 its `Sync` extends the INT
/// synchronizer, and no `java.base` class extends this one;
/// `i71-L1-compatible-aqls-state-lives-in-a-process-global-side-table-20261104`.)
/// What is left for them is that row's `=0` arm (the synthetic class library
/// defines no AQLS, so they serve nothing there), and since wave 73 that arm
/// keeps the word in the same `state` field the JDK bodies use, so the two
/// arms differ only in who runs the three accessors. The kind is inherited
/// (`register_essential_natives_with_shims`' ambient `Bridge`, re-tagged
/// `SyntheticStub` as retired rows).
pub fn register_aqls_state_natives(registry: &mut NativeMethodRegistry) {
    let aqls = AQLS_CLASS;
    registry.register(aqls, "getState", "()J", native_aqls_get_state, cratonvm_native_api::NativeKind::Bridge);
    registry.register(aqls, "setState", "(J)V", native_aqls_set_state, cratonvm_native_api::NativeKind::Bridge);
    registry.register(
        aqls,
        "compareAndSetState",
        "(JJ)Z",
        native_aqls_compare_and_set_state, cratonvm_native_api::NativeKind::Bridge,
    );
}

pub fn register_rwlock_natives(registry: &mut NativeMethodRegistry) {
    register_aqls_state_natives(registry);
    // Real AQS is the default (and is explicitly enabled by the Spring Boot
    // runner).  Its `ReentrantReadWriteLock` constructor creates real
    // ReadLock/WriteLock views whose `sync` field points at the real nested
    // `Sync`.  The legacy callbacks below instead manufacture a one-field
    // synthetic view with its parent RWL in slot zero.  That happens to work
    // for the callbacks they replace, but not for any unoverridden real-JDK
    // method: `WriteLock.newCondition()` reads that slot as `Sync` and then
    // tries to invoke `Sync.newCondition()` on the outer RWL, yielding the
    // misleading `ReentrantReadWriteLock.newCondition` NoSuchMethodError.
    // Keep the complete RWL family on real bytecode in this mode; the JIT
    // skip list already protects the AQS family.  StampedLock remains an
    // independent native implementation and must stay registered.
    let real_aqs = !crate::nbflags().synthetic_aqs || crate::nbflags().real_aqs;
    if real_aqs {
        register_stamped_lock_natives(registry);
        return;
    }
    register_synthetic_rwlock_natives(registry);
}

/// The legacy synthetic `ReentrantReadWriteLock` surface, split out of
/// [`register_rwlock_natives`] for the same reason as
/// [`register_synthetic_aqs_natives`]: under the default real-AQS build it is
/// never registered, and `cratonvm-vm`'s inline tests cannot drive the real
/// path either, so its tests had nothing to call.
///
/// Production behaviour is unchanged — the caller above still returns early
/// under real AQS without reaching this.
pub fn register_synthetic_rwlock_natives(registry: &mut NativeMethodRegistry) {
    let rwl = "java/util/concurrent/locks/ReentrantReadWriteLock";
    let rl = "java/util/concurrent/locks/ReentrantReadWriteLock$ReadLock";
    let wl = "java/util/concurrent/locks/ReentrantReadWriteLock$WriteLock";

    // Do not intercept the real-JDK constructors. Their three reference
    // fields are { readerLock, writerLock, sync }; the historical synthetic
    // initializer wrote integer state into those slots, so a method reference
    // such as ReentrantReadWriteLock::readLock received null. Let genuine
    // bytecode initialize the layout, while keeping the native lock-operation
    // backend below for the returned lock views.
    registry.register(
        rwl,
        "readLock",
        "()Ljava/util/concurrent/locks/ReentrantReadWriteLock$ReadLock;",
        native_rwl_read_lock, NativeKind::SyntheticStub,
    );
    registry.register(
        rwl,
        "writeLock",
        "()Ljava/util/concurrent/locks/ReentrantReadWriteLock$WriteLock;",
        native_rwl_write_lock, NativeKind::SyntheticStub,
    );

    // ReadLock = 1-field synthetic (parent=0 → ReentrantReadWriteLock).
    //
    // WP4.7: The lock/unlock/tryLock callbacks now delegate to the
    // process-wide RwLock backend in `crate::stamped_lock` instead of
    // toggling a per-object field. The old field-toggle was effectively
    // unsynchronized (two threads racing on `lock()` would each read
    // readers=0, both bump it to 1, neither would exclude a writer).
    // Real exclusion is now via a Mutex+Condvar slot keyed on the parent
    // RWL's address.
    // Pattern-A fix (bug nb-lib-gckeys §1): key the ReentrantReadWriteLock
    // state table by a GC-stable identity of the parent RWL object (see
    // `gc_stable_lock_key`) rather than its raw heap address. A read/write
    // view obtained before a GC must unlock against the SAME slot afterwards;
    // `parent.as_ptr()` shifts under a moving collector and the lock/unlock
    // pair would diverge, deadlocking the lock or corrupting a neighbour slot.
    fn rwl_parent_addr(
        ctx: &mut dyn NativeContext,
        this: ObjectRef,
    ) -> Result<Option<usize>, MethodCallFailed> {
        match ctx.get_field(this, 0) {
            Value::Object(Some(parent)) => Ok(Some(gc_stable_weak_lock_key(ctx, parent)?)),
            _ => Ok(None),
        }
    }

    registry.register(rl, "lock", "()V", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(None),
        };
        if let Ok(Some(addr)) = rwl_parent_addr(ctx, this) {
            // GC-blocking audit (STW takeover 5-class cluster, 2026-07-13):
            // rw_read_lock's internal contended wait is a raw
            // parking_lot::Condvar::wait with NO GC-blocking-region bracket
            // and no Java-heap touch inside — a thread contending for this
            // lock while a writer holds it stays counted in the STW
            // barrier's `expected` forever (not in JIT, never reaches a
            // safepoint), livelocking any cross-thread STW pause requested
            // while it waits.
            // Interpreter round i2 wave 65, lane L1: the JDK's acquire parks
            // (AQS / `StampedLock.acquire*`): WAITING, JVMTI PARKED.
            ctx.begin_blocking_region_as(cratonvm_native_api::BlockKind::Parked);
            crate::stamped_lock::rw_read_lock(addr, ctx.thread_id());
            ctx.end_blocking_region();
        }
        Ok(None)
    }, NativeKind::SyntheticStub);
    registry.register(rl, "unlock", "()V", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(None),
        };
        if let Ok(Some(addr)) = rwl_parent_addr(ctx, this) {
            crate::stamped_lock::rw_read_unlock(addr, ctx.thread_id());        }
        Ok(None)
    }, NativeKind::SyntheticStub);
    registry.register(rl, "tryLock", "()Z", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Int(0))),
        };
        let ok = match rwl_parent_addr(ctx, this)? {
            Some(a) => crate::stamped_lock::rw_try_read_lock(a, ctx.thread_id()),
            None => false,
        };
        Ok(Some(Value::Int(i32::from(ok))))
    }, NativeKind::SyntheticStub);
    registry.register(
        rl,
        "tryLock",
        "(JLjava/util/concurrent/TimeUnit;)Z",
        |ctx, args| {
            // Timed variant — caller already pre-rounded the timeout, and
            // the contention ratio in real apps doesn't justify a pdqueue
            // here. Treat as fast-path tryLock.
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Int(0))),
            };
            let ok = match rwl_parent_addr(ctx, this)? {
                Some(a) => crate::stamped_lock::rw_try_read_lock(a, ctx.thread_id()),
                None => false,
            };
            Ok(Some(Value::Int(i32::from(ok))))
        }, NativeKind::SyntheticStub,
    );
    registry.register(rl, "lockInterruptibly", "()V", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(None),
        };
        if let Ok(Some(addr)) = rwl_parent_addr(ctx, this) {
            // GC-blocking audit — see the plain `lock()` registration above.
            // Interpreter round i2 wave 65, lane L1: the JDK's acquire parks
            // (AQS / `StampedLock.acquire*`): WAITING, JVMTI PARKED.
            ctx.begin_blocking_region_as(cratonvm_native_api::BlockKind::Parked);
            crate::stamped_lock::rw_read_lock(addr, ctx.thread_id());
            ctx.end_blocking_region();
        }
        Ok(None)
    }, NativeKind::SyntheticStub);

    // WriteLock = 1-field synthetic (parent=0). WP4.7: delegate to the
    // process-wide RwLock backend (see comment on the read lock above).
    registry.register(wl, "lock", "()V", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(None),
        };
        if let Ok(Some(addr)) = rwl_parent_addr(ctx, this) {
            // GC-blocking audit (STW takeover 5-class cluster, 2026-07-13):
            // rw_write_lock's internal contended wait is a raw
            // parking_lot::Condvar::wait with NO GC-blocking-region bracket
            // and no Java-heap touch inside — see the read-lock `lock()`
            // registration above for the full rationale. Confirmed via
            // symbolicated cdb stacks: TestOrderInterceptor's stuck
            // ForkJoinPool worker threads were parked exactly here, not in
            // LockSupport.park (which IS correctly bracketed) — this was the
            // actual root cause of the STW takeover livelock, not a
            // ForkJoinPool/AQS-specific issue.
            // Interpreter round i2 wave 65, lane L1: the JDK's acquire parks
            // (AQS / `StampedLock.acquire*`): WAITING, JVMTI PARKED.
            ctx.begin_blocking_region_as(cratonvm_native_api::BlockKind::Parked);
            crate::stamped_lock::rw_write_lock(addr, ctx.thread_id());
            ctx.end_blocking_region();
        }
        Ok(None)
    }, NativeKind::SyntheticStub);
    registry.register(wl, "unlock", "()V", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(None),
        };
        if let Ok(Some(addr)) = rwl_parent_addr(ctx, this) {
            crate::stamped_lock::rw_write_unlock(addr, ctx.thread_id());        }
        Ok(None)
    }, NativeKind::SyntheticStub);
    registry.register(wl, "tryLock", "()Z", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Int(0))),
        };
        let ok = match rwl_parent_addr(ctx, this)? {
            Some(a) => crate::stamped_lock::rw_try_write_lock(a, ctx.thread_id()),
            None => false,
        };
        Ok(Some(Value::Int(i32::from(ok))))
    }, NativeKind::SyntheticStub);
    registry.register(
        wl,
        "tryLock",
        "(JLjava/util/concurrent/TimeUnit;)Z",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Int(0))),
            };
            let ok = match rwl_parent_addr(ctx, this)? {
                Some(a) => crate::stamped_lock::rw_try_write_lock(a, ctx.thread_id()),
                None => false,
            };
            Ok(Some(Value::Int(i32::from(ok))))
        }, NativeKind::SyntheticStub,
    );
    registry.register(wl, "lockInterruptibly", "()V", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(None),
        };
        if let Ok(Some(addr)) = rwl_parent_addr(ctx, this) {
            // GC-blocking audit — see the plain `lock()` registration above.
            // Interpreter round i2 wave 65, lane L1: the JDK's acquire parks
            // (AQS / `StampedLock.acquire*`): WAITING, JVMTI PARKED.
            ctx.begin_blocking_region_as(cratonvm_native_api::BlockKind::Parked);
            crate::stamped_lock::rw_write_lock(addr, ctx.thread_id());
            ctx.end_blocking_region();
        }
        Ok(None)
    }, NativeKind::SyntheticStub);
    registry.register(wl, "isHeldByCurrentThread", "()Z", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Int(0))),
        };
        let held = match rwl_parent_addr(ctx, this)? {
            Some(a) => crate::stamped_lock::rw_write_is_held(a, ctx.thread_id()),
            None => false,
        };
        Ok(Some(Value::Int(i32::from(held))))
    }, NativeKind::SyntheticStub);

    // StampedLock — real optimistic read / exclusive write lock with stamp validation.
    // Uses a global state map keyed by object address for per-lock state.
    register_stamped_lock_natives(registry);
}

/// JDK-ONLY-CLASSIFY: stub — all 31 registrations, and the reason is a
/// measurement rather than a reading of the class.
///
/// This function sets its own category now. It did not, and the consequence was
/// the ambient-category defect in its purest form: **the same registration site
/// produced a `Bridge` row and a `SyntheticStub` row in one boot**, because
/// this function is called from three places and the callers disagreed about
/// what was in effect. `--dump-native-registry` on JDK 25 / linux, 2026-08-06:
/// every one of the 25 `StampedLock` triples appears three times, twice
/// `bridge` and once `synthetic-stub`, and registration is last-write-wins, so
/// what actually shipped was decided by call ORDER.
///
/// `SyntheticStub` is what shipped, and it is also the right tag on the merits:
/// `java.util.concurrent.locks.StampedLock` is pure Java and JDK 25 declares no
/// `ACC_NATIVE` method on it or on its two view classes, so contract §1.5
/// cannot call these bridges. Under `--real-jdk` the whole surface is refused
/// together and the real class runs — which is the shape
/// `stampedlock-surface-must-be-complete-not-partial` asks for; a *partial*
/// surface is the failure mode there, and an ambient tag that depends on call
/// order is exactly how you get one.
///
/// CALLED THREE TIMES PER BOOT, AND THAT IS FINE — verified W7-15, 2026-08-07.
/// The census will show every triple below repeated; the repeats are these:
///
///   1. `lib.rs::register_essential_natives_with_shims` calls it directly;
///   2. the very next line there calls [`register_rwlock_natives`], which
///      calls it again on BOTH of its arms (real-AQS returns early through it,
///      synthetic AQS reaches it at the end of `register_synthetic_rwlock_natives`);
///   3. `vm/src/vm/vm_init.rs` calls it directly, in both mode arms.
///
/// A `synthetic-jdk` feature build adds a fourth via
/// `register_synthetic_overrides` -> [`register_rwlock_natives`].
///
/// OVER A REAL JDK, `--compatible` REGISTERS NONE OF THEM by default since
/// interpreter round i2 wave 70, lane L1: the `JdkStampedLock`
/// HotSpot-alignment row (`CRATONVM_COMPAT_JDK_STAMPED_LOCK`, `=0` kill
/// switch) names the class prefix in a `retired_shadow::COMPAT_FAMILIES`
/// entry's `stub_classes`, so `NativeMethodRegistry::register` drops every
/// `SyntheticStub` row here, as `--real-jdk` refuses them, and the JDK 25
/// bodies run (the real `state` word, `LockSupport.park(this)` on the JDK's
/// wait queue). What is left for these stand-ins is the synthetic class
/// library and that row's `=0` arm.
///
/// IDEMPOTENT FOR DISPATCH. Each repeat re-registers the identical triples with
/// the identical named-`fn` callbacks; this function states its own category, so
/// unlike the pre-2026-08-06 behaviour described above there is no ambient-kind
/// disagreement between callers, and `current_leaf` is `false` at all of them.
/// `NativeMethodRegistry::register` updates the EXISTING slot in place for a key
/// it has already seen — same callback, same `SyntheticStub` kind, same leaf
/// claim — and does not append a slot or a `slot_invocations` counter, so
/// already-issued `NativeMethodId`s stay valid and nothing observable changes.
///
/// NOT idempotent for the two per-registration LOGS, which is where the repeats
/// become visible and why they must not be read as a defect:
///
///   * `registrations` / `categories` / `kind_stated` / `category_chosen_log` /
///     `provenance` each gain a row per repeat, with
///     `provenance.overwrote == Some(SyntheticStub)` — i.e. this surface shadows
///     ITSELF. Those are the ~62 self-shadowed `StampedLock` rows in the
///     duplicate-registration census, not 62 lost registrations.
///   * under `CompatibilityMode::JdkOnly` the refusal arm returns *before*
///     inserting, so all 31 triples are pushed onto `refused` once per call.
///     A `--real-jdk` gate that counts refusal ROWS therefore counts this
///     surface three times; count distinct triples instead.
///
/// EARLY OUT WHERE THE ROW IS IN FORCE (interpreter round i3 wave 71, lane L1;
/// step 1 of `i70-L1-proposal-retire-the-stamped-lock-stand-ins-from-the-real-jdk-boot`
/// in its behaviour-preserving form): under `Compatible` with `JdkStampedLock`
/// aligned (`NativeMethodRegistry::aligned`: a real JDK, the row on) every
/// registration below is a `SyntheticStub` on the row's stub class, which
/// `register` drops, so the three calls per boot return before making 93
/// dropped calls. The `=0` arm and the synthetic class library still register
/// all 31; `JdkOnly` still reaches `register` (its refusal log is a gate's
/// input).
pub fn register_stamped_lock_natives(registry: &mut NativeMethodRegistry) {
    if registry.compatibility_mode() == cratonvm_types::compat::CompatibilityMode::Compatible
        && registry.aligned(cratonvm_types::HotSpotAlignment::JdkStampedLock)
    {
        return;
    }
            let sl = "java/util/concurrent/locks/StampedLock";
    registry.register(sl, "<init>", "()V", native_stamped_init, cratonvm_native_api::NativeKind::SyntheticStub);
    registry.register(sl, "readLock", "()J", native_stamped_read_lock, cratonvm_native_api::NativeKind::SyntheticStub);
    registry.register(sl, "writeLock", "()J", native_stamped_write_lock, cratonvm_native_api::NativeKind::SyntheticStub);
    registry.register(sl, "tryOptimisticRead", "()J", native_stamped_optimistic, cratonvm_native_api::NativeKind::SyntheticStub);
    registry.register(sl, "unlockRead", "(J)V", native_stamped_unlock_read, cratonvm_native_api::NativeKind::SyntheticStub);
    registry.register(sl, "unlockWrite", "(J)V", native_stamped_unlock_write, cratonvm_native_api::NativeKind::SyntheticStub);
    // See `native_stamped_unlock_by_stamp` — ported from the phase-62
    // imitation so that block can be deleted without losing `unlock(J)V`.
    registry.register(sl, "unlock", "(J)V", native_stamped_unlock_by_stamp, cratonvm_native_api::NativeKind::SyntheticStub);
    registry.register(
        sl,
        "unstampedUnlockRead",
        "()V",
        native_stamped_unstamped_unlock_read, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    registry.register(
        sl,
        "unstampedUnlockWrite",
        "()V",
        native_stamped_unstamped_unlock_write, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    registry.register(sl, "tryUnlockRead", "()Z", native_stamped_try_unlock_read, cratonvm_native_api::NativeKind::SyntheticStub);
    registry.register(sl, "tryUnlockWrite", "()Z", native_stamped_try_unlock_write, cratonvm_native_api::NativeKind::SyntheticStub);
    registry.register(sl, "validate", "(J)Z", native_stamped_validate, cratonvm_native_api::NativeKind::SyntheticStub);
    registry.register(sl, "tryReadLock", "()J", native_stamped_try_read_lock, cratonvm_native_api::NativeKind::SyntheticStub);
    registry.register(sl, "tryWriteLock", "()J", native_stamped_try_write_lock, cratonvm_native_api::NativeKind::SyntheticStub);
    registry.register(
        sl,
        "tryConvertToWriteLock",
        "(J)J",
        native_stamped_try_convert_to_write, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    registry.register(
        sl,
        "tryConvertToReadLock",
        "(J)J",
        native_stamped_try_convert_to_read, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    // The release path Agroal's `StampedCopyOnWriteArrayList` uses INSTEAD of
    // `unlockWrite` -- see `stamped_lock::stamped_try_convert_to_optimistic`.
    // While this was missing the call ran real JDK bytecode over a `state`
    // field this backend does not drive, released nothing, and deadlocked the
    // Keycloak boot.
    registry.register(
        sl,
        "tryConvertToOptimisticRead",
        "(J)J",
        native_stamped_try_convert_to_optimistic, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    // The rest of the blocking surface. Leaving ANY of these to real JDK
    // bytecode reintroduces exactly the same class of bug: the bytecode spins
    // on a `state` word nothing maintains.
    registry.register(
        sl,
        "readLockInterruptibly",
        "()J",
        native_stamped_read_lock_interruptibly, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    registry.register(
        sl,
        "writeLockInterruptibly",
        "()J",
        native_stamped_write_lock_interruptibly, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    registry.register(
        sl,
        "tryReadLock",
        "(JLjava/util/concurrent/TimeUnit;)J",
        native_stamped_try_read_lock_timed, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    registry.register(
        sl,
        "tryWriteLock",
        "(JLjava/util/concurrent/TimeUnit;)J",
        native_stamped_try_write_lock_timed, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    // `isLocked()` is a DELIBERATE completion of the synthetic surface: JDK 25's
    // `StampedLock` declares `isReadLocked`/`isWriteLocked` and no `isLocked`,
    // so the dead-everywhere sweep scores this row "method nowhere on any
    // image" and listed it for deletion. It must NOT be deleted —
    // `native-builtins/tests/registry_contracts.rs` pins it, and under
    // `--synthetic-jdk` it is the only implementation there is. A row a contract
    // test pins is gated by that fact alone, which is what the sweep's new
    // `--pinned` input carries.
    registry.register(sl, "isLocked", "()Z", native_stamped_is_locked, cratonvm_native_api::NativeKind::SyntheticStub);
    registry.register(sl, "isWriteLocked", "()Z", native_stamped_is_write_locked, cratonvm_native_api::NativeKind::SyntheticStub);
    registry.register(sl, "isReadLocked", "()Z", native_stamped_is_read_locked, cratonvm_native_api::NativeKind::SyntheticStub);
    registry.register(
        sl,
        "getReadLockCount",
        "()I",
        native_stamped_get_read_lock_count, cratonvm_native_api::NativeKind::SyntheticStub,
    );

    let sl_wv = "java/util/concurrent/locks/StampedLock$WriteLockView";
    registry.register(sl_wv, "lock", "()V", native_stamped_write_view_lock, cratonvm_native_api::NativeKind::SyntheticStub);
    registry.register(sl_wv, "tryLock", "()Z", native_stamped_write_view_try_lock, cratonvm_native_api::NativeKind::SyntheticStub);
    registry.register(sl_wv, "unlock", "()V", native_stamped_write_view_unlock, cratonvm_native_api::NativeKind::SyntheticStub);

    let sl_rv = "java/util/concurrent/locks/StampedLock$ReadLockView";
    registry.register(sl_rv, "lock", "()V", native_stamped_read_view_lock, cratonvm_native_api::NativeKind::SyntheticStub);
    registry.register(sl_rv, "tryLock", "()Z", native_stamped_read_view_try_lock, cratonvm_native_api::NativeKind::SyntheticStub);
    registry.register(sl_rv, "unlock", "()V", native_stamped_read_view_unlock, cratonvm_native_api::NativeKind::SyntheticStub);
    }

// The four `native_rwl_*` stubs that used to sit here were dead code: nothing
// registered them, they stored the reader/writer counts in the object's own
// slots (which the real class uses for its `Sync`/view references), and there
// was no `lock`/`unlock` for the views they handed out. The working
// implementation lives at the bottom of this file next to
// `register_synthetic_aqs_natives`, which registers it.

// Pattern-A fix (bug nb-lib-gckeys §1): key the StampedLock state table by a
// GC-stable identity (see `gc_stable_lock_key`) instead of the raw, moving
// heap address that the lock/unlock pair could disagree on across a GC.
pub fn stamped_addr(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> Result<Option<usize>, MethodCallFailed> {
    match args.first() {
        Some(Value::Object(Some(o))) => Ok(Some(gc_stable_weak_lock_key(ctx, *o)?)),
        _ => Ok(None),
    }
}

pub fn stamped_obj(args: &[Value]) -> Option<ObjectRef> {
    match args.first() {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    }
}

pub fn stamped_addr_for_obj(
    ctx: &mut dyn NativeContext,
    obj: ObjectRef,
) -> Result<usize, MethodCallFailed> {
    Ok(gc_stable_weak_lock_key(ctx, obj)?)
}

pub fn stamped_view_parent(ctx: &mut dyn NativeContext, args: &[Value]) -> Option<ObjectRef> {
    let view = stamped_obj(args)?;
    match ctx.get_field_by_name(view, "this$0") {
        Value::Object(Some(parent)) => Some(parent),
        _ => None,
    }
}

/// End a `StampedLock` acquire's blocking region and hand back `obj` (the lock,
/// or a view's owning lock) as it is AFTER the region.
///
/// Interpreter round i3 wave 79, lane L1
/// (`i68-L1-stamped-lock-stand-ins-reuse-a-pre-region-receiver`): a contended
/// acquire can stay in the region across a moving collection, and
/// `end_blocking_region` repairs the frames, not this native's locals, so the
/// following `mirror_stamped_state` wrote the `state` mirror through the
/// pre-collection address. `end_blocking_region_refs` rewrites `obj` through
/// the same fixup, as the `ReferenceQueue` poll and `phases_late/concurrent.rs`'s
/// timed `tryReadLock` do.
pub fn stamped_end_region_reread(ctx: &mut dyn NativeContext, obj: ObjectRef) -> ObjectRef {
    let mut refs = [Value::Object(Some(obj))];
    ctx.end_blocking_region_refs(&mut refs);
    match refs[0] {
        Value::Object(Some(o)) => o,
        _ => obj,
    }
}

pub fn native_stamped_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let Some(obj) = stamped_obj(args) {
        let addr = stamped_addr_for_obj(ctx, obj)?;
        crate::stamped_lock::stamped_init(addr);
        mirror_stamped_state(ctx, obj, addr);
    }
    Ok(None)
}

pub fn native_stamped_write_lock(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let obj = match stamped_obj(args) {
        Some(o) => o,
        None => return Ok(Some(Value::Long(0))),
    };
    let addr = stamped_addr_for_obj(ctx, obj)?;
    // GC-blocking audit (STW takeover 5-class cluster, 2026-07-13):
    // stamped_write_lock's contended wait is a raw parking_lot::Condvar::wait
    // with NO GC-blocking-region bracket — same missing-bracket bug as
    // ReentrantReadWriteLock's rw_write_lock (see that registration's
    // comment for the full rationale and how this was diagnosed).
    // Interpreter round i2 wave 65, lane L1: the JDK's acquire parks
    // (AQS / `StampedLock.acquire*`): WAITING, JVMTI PARKED.
    // Wave 68 (lane L1, `CRATONVM_STAND_IN_WAIT_OBJECTS`): the JDK's acquire
    // parks with the lock itself as the blocker (`LockSupport.park(this)`),
    // which JMX names.
    ctx.begin_blocking_region_as_on(obj, cratonvm_native_api::BlockKind::Parked);
    let stamp = crate::stamped_lock::stamped_write_lock(addr);
    let obj = stamped_end_region_reread(ctx, obj);
    mirror_stamped_state(ctx, obj, addr);
    if crate::nbflags().dbg_stamped {
        eprintln!("[SL-DBG] writeLock addr={addr:#x} stamp={stamp}");
    }
    Ok(Some(Value::Long(stamp)))
}

pub fn native_stamped_read_lock(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let obj = match stamped_obj(args) {
        Some(o) => o,
        None => return Ok(Some(Value::Long(0))),
    };
    let addr = stamped_addr_for_obj(ctx, obj)?;
    // GC-blocking audit — see `native_stamped_write_lock` above.
    // Interpreter round i2 wave 65, lane L1: the JDK's acquire parks
    // (AQS / `StampedLock.acquire*`): WAITING, JVMTI PARKED.
    // Wave 68 (lane L1, `CRATONVM_STAND_IN_WAIT_OBJECTS`): the JDK's acquire
    // parks with the lock itself as the blocker (`LockSupport.park(this)`),
    // which JMX names.
    ctx.begin_blocking_region_as_on(obj, cratonvm_native_api::BlockKind::Parked);
    let stamp = crate::stamped_lock::stamped_read_lock(addr);
    let obj = stamped_end_region_reread(ctx, obj);
    mirror_stamped_state(ctx, obj, addr);
    Ok(Some(Value::Long(stamp)))
}

pub fn native_stamped_try_read_lock(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let obj = match stamped_obj(args) {
        Some(o) => o,
        None => return Ok(Some(Value::Long(0))),
    };
    let addr = stamped_addr_for_obj(ctx, obj)?;
    let stamp = crate::stamped_lock::stamped_try_read_lock(addr);
    mirror_stamped_state(ctx, obj, addr);
    Ok(Some(Value::Long(stamp)))
}

pub fn native_stamped_try_write_lock(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let obj = match stamped_obj(args) {
        Some(o) => o,
        None => return Ok(Some(Value::Long(0))),
    };
    let addr = stamped_addr_for_obj(ctx, obj)?;
    let stamp = crate::stamped_lock::stamped_try_write_lock(addr);
    mirror_stamped_state(ctx, obj, addr);
    Ok(Some(Value::Long(stamp)))
}

pub fn native_stamped_write_view_lock(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(parent) = stamped_view_parent(ctx, args) else {
        return Ok(None);
    };
    let addr = stamped_addr_for_obj(ctx, parent)?;
    // GC-blocking audit — see `native_stamped_write_lock` above.
    // Interpreter round i2 wave 65, lane L1: the JDK's acquire parks
    // (AQS / `StampedLock.acquire*`): WAITING, JVMTI PARKED.
    // Wave 68 (lane L1, `CRATONVM_STAND_IN_WAIT_OBJECTS`): the JDK's acquire
    // parks with the lock itself as the blocker (`LockSupport.park(this)`),
    // which JMX names.
    ctx.begin_blocking_region_as_on(parent, cratonvm_native_api::BlockKind::Parked);
    crate::stamped_lock::stamped_write_lock(addr);
    let parent = stamped_end_region_reread(ctx, parent);
    mirror_stamped_state(ctx, parent, addr);
    Ok(None)
}

pub fn native_stamped_write_view_try_lock(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let Some(parent) = stamped_view_parent(ctx, args) else {
        return Ok(Some(Value::Int(0)));
    };
    let addr = stamped_addr_for_obj(ctx, parent)?;
    let stamp = crate::stamped_lock::stamped_try_write_lock(addr);
    mirror_stamped_state(ctx, parent, addr);
    Ok(Some(Value::Int(i32::from(stamp != 0))))
}

pub fn native_stamped_write_view_unlock(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let Some(parent) = stamped_view_parent(ctx, args) else {
        return Ok(None);
    };
    let addr = stamped_addr_for_obj(ctx, parent)?;
    if !crate::stamped_lock::stamped_try_unstamped_unlock_write(addr) {
        return Err(RuntimeError::IllegalMonitorStateException {
            message: "StampedLock write lock not held".to_string(),
        }
        .into());
    }
    mirror_stamped_state(ctx, parent, addr);
    Ok(None)
}

pub fn native_stamped_read_view_lock(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(parent) = stamped_view_parent(ctx, args) else {
        return Ok(None);
    };
    let addr = stamped_addr_for_obj(ctx, parent)?;
    // GC-blocking audit — see `native_stamped_write_lock` above.
    // Interpreter round i2 wave 65, lane L1: the JDK's acquire parks
    // (AQS / `StampedLock.acquire*`): WAITING, JVMTI PARKED.
    // Wave 68 (lane L1, `CRATONVM_STAND_IN_WAIT_OBJECTS`): the JDK's acquire
    // parks with the lock itself as the blocker (`LockSupport.park(this)`),
    // which JMX names.
    ctx.begin_blocking_region_as_on(parent, cratonvm_native_api::BlockKind::Parked);
    crate::stamped_lock::stamped_read_lock(addr);
    let parent = stamped_end_region_reread(ctx, parent);
    mirror_stamped_state(ctx, parent, addr);
    Ok(None)
}

pub fn native_stamped_read_view_try_lock(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let Some(parent) = stamped_view_parent(ctx, args) else {
        return Ok(Some(Value::Int(0)));
    };
    let addr = stamped_addr_for_obj(ctx, parent)?;
    let stamp = crate::stamped_lock::stamped_try_read_lock(addr);
    mirror_stamped_state(ctx, parent, addr);
    Ok(Some(Value::Int(i32::from(stamp != 0))))
}

pub fn native_stamped_read_view_unlock(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let Some(parent) = stamped_view_parent(ctx, args) else {
        return Ok(None);
    };
    let addr = stamped_addr_for_obj(ctx, parent)?;
    if !crate::stamped_lock::stamped_try_unstamped_unlock_read(addr) {
        return Err(RuntimeError::IllegalMonitorStateException {
            message: "StampedLock read lock not held".to_string(),
        }
        .into());
    }
    mirror_stamped_state(ctx, parent, addr);
    Ok(None)
}

pub fn native_stamped_optimistic(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let addr = match stamped_addr(ctx, args)? {
        Some(a) => a,
        None => return Ok(Some(Value::Long(STAMPED_ORIGIN))),
    };
    Ok(Some(Value::Long(
        crate::stamped_lock::stamped_try_optimistic_read(addr),
    )))
}

pub fn native_stamped_validate(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let stamp = match args.get(1) {
        Some(Value::Long(v)) => *v,
        _ => return Ok(Some(Value::Int(0))),
    };
    let addr = match stamped_addr(ctx, args)? {
        Some(a) => a,
        None => return Ok(Some(Value::Int(0))),
    };
    let valid = crate::stamped_lock::stamped_validate(addr, stamp);
    Ok(Some(Value::Int(i32::from(valid))))
}

/// The stamp argument of a `(J)V` / `(J)Z` StampedLock native, or `None`
/// when the frame did not carry one.
pub fn stamped_arg(args: &[Value]) -> Option<i64> {
    match args.get(1) {
        Some(Value::Long(v)) => Some(*v),
        _ => None,
    }
}

/// `unlockRead(long stamp)`.
///
/// The stamp is CHECKED, not ignored: the JDK throws
/// `IllegalMonitorStateException` when the stamp is not a read stamp, when
/// its version has moved on, or when no read hold is outstanding. Verified
/// against HotSpot 25.0.3 (`SLBits.java` rows `J.unlockReadBogus = IMSE`).
pub fn native_stamped_unlock_read(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(obj) = stamped_obj(args) else {
        return Ok(None);
    };
    let stamp = stamped_arg(args).unwrap_or(0);
    let addr = stamped_addr_for_obj(ctx, obj)?;
    let released = crate::stamped_lock::stamped_unlock_read(addr, stamp);
    mirror_stamped_state(ctx, obj, addr);
    if !released {
        return Err(RuntimeError::IllegalMonitorStateException {
            // No message: HotSpot's is null here (measured, 3 runs). The stamp
            // is still in the caller's hand, so nothing diagnosable is lost.
            message: String::new(),
        }
        .into());
    }
    Ok(None)
}

/// `unlockWrite(long stamp)`.
///
/// The JDK guard is `if (state != stamp || (stamp & WBIT) == 0L) throw` — an
/// EXACT state-word match, so a stale stamp cannot release a later writer's
/// hold. This used to ignore the stamp and release whatever was held, which
/// silently broke mutual exclusion whenever a caller passed the wrong stamp
/// (which the `isWriteLockStamp` encoding bug made routine). Verified
/// against HotSpot (`J.doubleUnlockWrite = IMSE`).
pub fn native_stamped_unlock_write(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(obj) = stamped_obj(args) else {
        return Ok(None);
    };
    let stamp = stamped_arg(args).unwrap_or(0);
    let addr = stamped_addr_for_obj(ctx, obj)?;
    let released = crate::stamped_lock::stamped_unlock_write(addr, stamp);
    mirror_stamped_state(ctx, obj, addr);
    if !released {
        return Err(RuntimeError::IllegalMonitorStateException {
            // No message: HotSpot's is null here (measured, 3 runs). The stamp
            // is still in the caller's hand, so nothing diagnosable is lost.
            message: String::new(),
        }
        .into());
    }
    Ok(None)
}

