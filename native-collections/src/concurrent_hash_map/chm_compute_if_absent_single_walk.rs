#[allow(unused_imports)]
use super::*;

/// `ConcurrentHashMap.computeIfAbsent` in the JDK's walk
/// (`CRATONVM_COMPAT_CHM_CIA_SINGLE_WALK`), with this VM's reservation so the
/// mapper runs with no monitor held:
///
/// 1. lock-free, over a snapshot of the bin: `computeIfAbsent`'s first-node
///    test (`fh == h && key.equals(fk) && fv != null`: return `fv`), then the
///    walk from that first node (`e.hash == h && key.equals(ek)`). A present
///    key answers here, without the segment monitor, as before;
/// 2. under the monitor: the bin is walked by IDENTITY, and only a node the
///    snapshot did not hold (one linked since) is compared by `equals`; the
///    reservation node is then linked at the chain's end with no comparison
///    (keys are final, so a snapshot node that did not match still does not);
/// 3. the mapper, with no monitor held;
/// 4. under the monitor: the reservation NODE, still linked and still holding
///    the marker, takes the value (or is unlinked for a null / thrown
///    result), and same-key waiters are woken.
///
/// Anything the snapshot cannot settle -- a bin longer than
/// [`CHM_CIA_SNAPSHOT_CAP`], another thread's reservation for the key, a
/// segment that changed identity -- takes the old body
/// ([`chm_compute_if_absent_absent_path`]). A reservation node a racing `put`
/// wrote over, or a racing `remove` unlinked, leaves the raced-in state, as
/// the old commit did.
#[allow(clippy::too_many_arguments)]
pub(crate) fn chm_compute_if_absent_single_walk(
    ctx: &mut dyn NativeContext,
    roots_base: usize,
    this: ObjectRef,
    seg: ObjectRef,
    hash: i32,
    key_pin: usize,
    key: Value,
    func_pin: usize,
    func: Value,
) -> MethodCallResult {
    let null = Value::Object(None);
    // The snapshot waits for the stripe lock GC-safely, so the segment is
    // rooted across it.
    let seg_pin = ctx.pin_native_root(seg);
    let snapshot = chm_seg_bin_snapshot(ctx, seg, hash, CHM_CIA_SNAPSHOT_CAP);
    let seg = ctx.read_native_pin(seg_pin, seg);
    let Some(snapshot) = snapshot else {
        let old = chm_compute_if_absent_probe_then_reserve(
            ctx, roots_base, this, seg, hash, key_pin, key, func_pin, func,
        );
        ctx.unpin_native_roots(seg_pin);
        return old;
    };
    // Nothing has run since the snapshot's nodes were read: pin them.
    let seen: Vec<(usize, ObjectRef)> = snapshot
        .iter()
        .map(|n| (ctx.pin_native_root(*n), *n))
        .collect();
    // Every pin above `seg_pin` is released with it, on every exit.
    let result = (|| -> MethodCallResult {
        // ---- 1. lock-free: the first-node test, then the walk. ----
        let mut found: Option<(usize, ObjectRef)> = None;
        if let Some(&(first_pin, first)) = seen.first() {
            let first = ctx.read_native_pin(first_pin, first);
            if get_node_hash(&*ctx, first) == hash {
                if let (Value::Object(Some(fk)), Value::Object(Some(k))) = (
                    get_node_key(&*ctx, first),
                    read_pinned_elem(ctx, key_pin, key),
                ) {
                    if map_keys_equal(ctx, k, fk)? {
                        let first = ctx.read_native_pin(first_pin, first);
                        let seg_now = ctx.read_native_pin(seg_pin, seg);
                        let fv = get_node_value(&*ctx, first);
                        if !matches!(fv, Value::Object(None))
                            && !chm_node_holds_marker(&*ctx, first, seg_now)
                        {
                            return Ok(Some(fv));
                        }
                    }
                }
            }
        }
        for &(pin, node) in &seen {
            let node = ctx.read_native_pin(pin, node);
            if get_node_hash(&*ctx, node) != hash {
                continue;
            }
            let (Value::Object(Some(nk)), Value::Object(Some(k))) = (
                get_node_key(&*ctx, node),
                read_pinned_elem(ctx, key_pin, key),
            ) else {
                continue;
            };
            if map_keys_equal(ctx, k, nk)? {
                found = Some((pin, node));
                break;
            }
        }
        if let Some((pin, node)) = found {
            let node = ctx.read_native_pin(pin, node);
            let seg_now = ctx.read_native_pin(seg_pin, seg);
            let v = get_node_value(&*ctx, node);
            if !matches!(v, Value::Object(None)) && !chm_node_holds_marker(&*ctx, node, seg_now) {
                return Ok(Some(v));
            }
            // Another thread's mapper is in flight for this key: the old body
            // waits for it.
            return chm_compute_if_absent_absent_path(
                ctx, roots_base, this, hash, key_pin, key, func_pin, func,
            );
        }

        // ---- 2. reserve under the monitor, comparing only new nodes. ----
        let this_now = ctx.read_native_pin(roots_base, this);
        let seg_now = ctx.read_native_pin(seg_pin, seg);
        match chm_segment_for(&*ctx, this_now, hash) {
            Some(s) if std::ptr::eq(s.as_ptr(), seg_now.as_ptr()) => {}
            _ => {
                return chm_compute_if_absent_absent_path(
                    ctx, roots_base, this, hash, key_pin, key, func_pin, func,
                )
            }
        }
        let reserved: Option<Option<(usize, ObjectRef)>> = {
            let _resize_flag = ChmResizeLockGuard::enter();
            let (guard, segc) = ChmMonitorGuard::acquire_gc_safe(ctx, seg_now);
            let (buckets, size, cap) = map_state(&*ctx, segc);
            let mut cur = match buckets {
                Some(b) => ctx.get_array_element(b, map_bucket_index(hash, cap)),
                None => null,
            };
            let mut raced = false;
            let mut steps = 0usize;
            while let Value::Object(Some(n)) = cur {
                steps += 1;
                if steps > chain_walk_limit(size) {
                    raced = true;
                    break;
                }
                let is_seen = seen
                    .iter()
                    .any(|&(p, s)| std::ptr::eq(ctx.read_native_pin(p, s).as_ptr(), n.as_ptr()));
                if !is_seen && get_node_hash(&*ctx, n) == hash {
                    // Linked since the snapshot: `equals` decides, as the
                    // JDK's walk would. A match of any kind (a value, or a
                    // reservation) is settled by the old body.
                    if let (Value::Object(Some(nk)), Value::Object(Some(k))) =
                        (get_node_key(&*ctx, n), read_pinned_elem(ctx, key_pin, key))
                    {
                        let n_pin = ctx.pin_native_root(n);
                        let eq = map_keys_equal(ctx, k, nk);
                        let n_now = ctx.read_native_pin(n_pin, n);
                        ctx.unpin_native_roots(n_pin);
                        match eq {
                            Ok(true) => {
                                raced = true;
                                break;
                            }
                            Ok(false) => {}
                            Err(e) => {
                                drop(guard);
                                return Err(e);
                            }
                        }
                        cur = ctx.get_field(n_now, NODE_FIELD_NEXT);
                        continue;
                    }
                }
                cur = ctx.get_field(n, NODE_FIELD_NEXT);
            }
            if raced || buckets.is_none() {
                drop(guard);
                None
            } else {
                // `pred.next = new Node(..)`, holding the marker: no key
                // comparison (`PutMode::assume_absent`), no second hash.
                let segc = ctx.read_native_pin(seg_pin, segc);
                let key_now = read_pinned_elem(ctx, key_pin, key);
                let marker = Value::Object(Some(segc));
                let mut seg_mut = segc;
                let put_base = ctx.pin_native_root(seg_mut);
                let k_pin = pin_value(ctx, key_now);
                let m_pin = pin_value(ctx, marker);
                let put = native_map_put_evict_pinned_inner(
                    ctx,
                    &mut seg_mut,
                    key_now,
                    marker,
                    true,
                    put_base,
                    k_pin,
                    m_pin,
                    GrowAlloc::SingleAttempt,
                    Some(hash),
                    false,
                    PutMode {
                        no_grow: false,
                        assume_absent: true,
                    },
                );
                ctx.unpin_native_roots(put_base);
                if let Err(e) = put {
                    drop(guard);
                    return Err(e);
                }
                // The node just linked: the one holding this key OBJECT and
                // the marker (identity reads; a resize may have moved it).
                let segc = ctx.read_native_pin(seg_pin, segc);
                let key_now = read_pinned_elem(ctx, key_pin, key);
                let (buckets, size, cap) = map_state(&*ctx, segc);
                let mut mine: Option<ObjectRef> = None;
                if let (Some(b), Value::Object(Some(k))) = (buckets, key_now) {
                    let mut cur = ctx.get_array_element(b, map_bucket_index(hash, cap));
                    for _ in 0..chain_walk_limit(size) {
                        let Value::Object(Some(n)) = cur else {
                            break;
                        };
                        if matches!(get_node_key(&*ctx, n), Value::Object(Some(nk)) if std::ptr::eq(nk.as_ptr(), k.as_ptr()))
                            && chm_node_holds_marker(&*ctx, n, segc)
                        {
                            mine = Some(n);
                            break;
                        }
                        cur = ctx.get_field(n, NODE_FIELD_NEXT);
                    }
                }
                // Rooted before the monitor is released (no allocation since
                // the walk that found it), by a GLOBAL root: the guard's drop
                // releases every native pin taken above its own. Removed
                // after the commit. `Some(None)`: reserved, but the node was
                // not found again; the commit then goes by key, as the old
                // one did.
                let mine = mine.map(|n| (ctx.add_global_root(n), n));
                drop(guard);
                Some(mine)
            }
        };
        let Some(node_ref) = reserved else {
            return chm_compute_if_absent_absent_path(
                ctx, roots_base, this, hash, key_pin, key, func_pin, func,
            );
        };

        // ---- 3. the mapper, with no monitor held. ----
        let func_now = read_pinned_elem(ctx, func_pin, func);
        let key_now = read_pinned_elem(ctx, key_pin, key);
        let mapped: Result<Option<Value>, MethodCallFailed> = match func_now {
            Value::Object(Some(f)) => ctx.invoke_virtual(
                f,
                "apply",
                "(Ljava/lang/Object;)Ljava/lang/Object;",
                &[key_now],
            ),
            _ => Ok(Some(null)),
        };
        let mapped_val = match &mapped {
            Ok(Some(v)) => *v,
            _ => null,
        };
        let mapped_pin = pin_value(ctx, mapped_val);

        // ---- 4. commit the reservation node under the monitor. ----
        let this_now = ctx.read_native_pin(roots_base, this);
        let commit: Result<Value, MethodCallFailed> = match chm_segment_for(&*ctx, this_now, hash) {
            None => Ok(null),
            Some(seg3) => {
                let _resize_flag = ChmResizeLockGuard::enter();
                let (guard, segc) = ChmMonitorGuard::acquire_gc_safe(ctx, seg3);
                // The segment the monitor is held on (not necessarily the
                // one reserved in, `seg_pin`), rooted for the key lookups
                // below, which run Java.
                let seg3_pin = ctx.pin_native_root(segc);
                let node = node_ref.map(|(root, n)| ctx.resolve_global_root(root).unwrap_or(n));
                let mv = read_pinned_elem(ctx, mapped_pin, mapped_val);
                let result = match node {
                    Some(n) if chm_seg_bin_links(&*ctx, segc, hash, n) => {
                        if chm_node_holds_marker(&*ctx, n, segc) {
                            if mapped.is_ok() && !matches!(mv, Value::Object(None)) {
                                // `pred.next = new Node<K,V>(h, key, val)`.
                                set_node_value(ctx, n, mv);
                                Ok(read_pinned_elem(ctx, mapped_pin, mv))
                            } else {
                                map_unlink_node(ctx, segc, n);
                                Ok(null)
                            }
                        } else {
                            // A racing `put` wrote over the reservation: its
                            // value stands.
                            Ok(get_node_value(&*ctx, n))
                        }
                    }
                    _ => {
                        // Unlinked by a racing mutator (or not found again):
                        // surface what the map holds for the key now, as the
                        // old commit did; a reservation of ours still in
                        // place is committed by key.
                        let key_now = read_pinned_elem(ctx, key_pin, key);
                        let cur = chm_seg_get_raw(ctx, segc, key_now, hash);
                        let segc = ctx.read_native_pin(seg3_pin, segc);
                        match cur {
                            Err(e) => Err(e),
                            Ok(Some(Value::Object(Some(v))))
                                if std::ptr::eq(v.as_ptr(), segc.as_ptr()) =>
                            {
                                let key_now = read_pinned_elem(ctx, key_pin, key);
                                let mv = read_pinned_elem(ctx, mapped_pin, mapped_val);
                                if mapped.is_ok() && !matches!(mv, Value::Object(None)) {
                                    match chm_seg_put_hashed(ctx, segc, key_now, mv, hash, false) {
                                        Ok(_) => Ok(read_pinned_elem(ctx, mapped_pin, mv)),
                                        Err(e) => Err(e),
                                    }
                                } else {
                                    match chm_seg_remove_hashed(ctx, segc, key_now, hash) {
                                        Ok(_) => Ok(null),
                                        Err(e) => Err(e),
                                    }
                                }
                            }
                            Ok(Some(v)) => Ok(v),
                            Ok(None) => Ok(null),
                        }
                    }
                };
                let segc = ctx.read_native_pin(seg3_pin, segc);
                let _ = ctx.monitor_notify_all(segc);
                ctx.unpin_native_roots(seg3_pin);
                drop(guard);
                result
            }
        };
        if let Some((root, _)) = node_ref {
            ctx.remove_global_root(root);
        }
        match mapped {
            Err(e) => Err(e),
            Ok(_) => commit.map(Some),
        }
    })();
    ctx.unpin_native_roots(seg_pin);
    result
}

thread_local! {
    /// The `(pin handle of the map, key hash)` of every `ConcurrentHashMap`
    /// mutating callback THIS THREAD is currently inside.
    ///
    /// The handle, not the `ObjectRef`: a moving collection can run inside the
    /// callback, so a raw pointer stored here would be stale by the time the
    /// re-entrant call reads it. A pin handle is stable for as long as the
    /// frame that took it is alive, and the outer frame is by construction
    /// alive whenever an inner one is looking — so
    /// `read_native_pin(handle, ..)` always answers the map's CURRENT location.
    pub(crate) static CHM_CALLBACK_FRAMES: std::cell::RefCell<Vec<(usize, i32)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Pushes one entry onto [`CHM_CALLBACK_FRAMES`] and pops it on every exit
/// path, `?` and panic included.
pub(crate) struct ChmCallbackFrame;

impl ChmCallbackFrame {
    pub(in crate::concurrent_hash_map) fn enter(map_pin: usize, hash: i32) -> Self {
        CHM_CALLBACK_FRAMES.with(|f| f.borrow_mut().push((map_pin, hash)));
        ChmCallbackFrame
    }
}

impl Drop for ChmCallbackFrame {
    fn drop(&mut self) {
        CHM_CALLBACK_FRAMES.with(|f| {
            f.borrow_mut().pop();
        });
    }
}

/// The JDK's `IllegalStateException("Recursive update")`, or `Ok(())`.
///
/// # What the JDK does, and what this VM did instead
///
/// `computeIfAbsent`/`compute`/`merge` lock ONE BIN and run the user function
/// under it. A function that calls back into the same map on the same bin
/// re-enters that lock — reentrant, so it does not deadlock — finds the
/// reservation it placed itself, and throws:
///
/// ```text
///   throw new IllegalStateException("Recursive update");
/// ```
///
/// CratonVM's `computeIfAbsent` runs its mapper with NO monitor held and
/// marks the key with a reservation instead (`chm_compute_if_absent_absent_path`).
/// Phase 1 of the re-entrant call finds that reservation, cannot tell that the
/// thread waiting for it to clear is ITSELF, and waits for it in a 50 ms loop
/// that nothing can ever end. MEASURED, both modes, no flags:
///
/// ```text
///   chm.computeIfAbsent("q", k -> chm.computeIfAbsent("q", k2 -> 1))
///     HotSpot   IllegalStateException: Recursive update
///     CratonVM  never returns
/// ```
///
/// `compute` is the other polarity of one bug: it holds a REENTRANT segment
/// monitor, so the inner call simply succeeded and the map came back holding a
/// mapping the JDK refuses to make.
///
/// # Why the hash and not the key
///
/// The JDK's rule is per-BIN, i.e. `hash & (n - 1)`, which is COARSER than
/// per-key: two unequal keys that collide in one bin also throw. Keying the
/// guard on the full hash is therefore strictly narrower than the JDK — it
/// never throws where the JDK would not — and it needs no `equals` call, which
/// on this path would be more user code re-entering the same map.
pub(crate) fn chm_reject_recursive_update(
    ctx: &dyn NativeContext,
    map: ObjectRef,
    hash: i32,
) -> Result<(), MethodCallFailed> {
    let frames: Vec<(usize, i32)> = CHM_CALLBACK_FRAMES.with(|f| f.borrow().clone());
    let recursive = frames.iter().any(|(pin, h)| {
        *h == hash && std::ptr::eq(ctx.read_native_pin(*pin, map).as_ptr(), map.as_ptr())
    });
    if recursive {
        return Err(RuntimeError::IllegalStateException {
            message: "Recursive update".to_string(),        }
        .into());    }
    Ok(())
}

pub(crate) fn native_chm_compute_if_absent(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key = args.get(1).copied().unwrap_or(Value::Object(None));
    let func = args.get(2).copied().unwrap_or(Value::Object(None));
    // C24 (HIGH): JDK rejects null key and null mappingFunction. MEASURED
    // message-less on the oracle — see `chm_bare_npe_on_null`.
    if matches!(key, Value::Object(None)) || matches!(func, Value::Object(None)) {
        return Err(bare_npe());
    }
    let roots_base = ctx.pin_native_root(this);
    let key_pin = pin_value(ctx, key);
    let func_pin = pin_value(ctx, func);
    let key = read_pinned_elem(ctx, key_pin, key);
    let hash = chm_key_hash(ctx, &key)?;
    let this = ctx.read_native_pin(roots_base, this);
    // See `chm_reject_recursive_update`: without this the re-entrant call waits
    // forever for a reservation only it can clear.
    if let Err(e) = chm_reject_recursive_update(&*ctx, this, hash) {
        ctx.unpin_native_roots(roots_base);
        return Err(e);
    }
    let _frame = ChmCallbackFrame::enter(roots_base, hash);
    let result = match chm_segment_for_mut(ctx, this, hash) {
        Some(seg) => {
            // JDK-exact lock-free fast path: `computeIfAbsent` on a PRESENT
            // key returns the existing value WITHOUT locking. Real CHM locks
            // a single bin, so a mapping callback that takes unrelated locks
            // (WildFly's registry callbacks take the management-registry
            // write lock) never orders against other keys' operations; our
            // segment monitor is far coarser, and locking it just to discover
            // the key already exists created a segment-monitor ↔ registry-
            // RWLock cycle that wedged `parallel-extension-add` boots (the
            // mode-2 wedge: gdb showed writers starving at
            // stamped_lock::rw_write_lock while a read-lock holder waited in
            // acquire_gc_safe on the same segment).
            if chm_cia_single_walk_enabled() {
                chm_compute_if_absent_single_walk(
                    ctx, roots_base, this, seg, hash, key_pin, key, func_pin, func,
                )
            } else {
                chm_compute_if_absent_probe_then_reserve(
                    ctx, roots_base, this, seg, hash, key_pin, key, func_pin, func,
                )
            }
        }
        None => Ok(Some(Value::Object(None))),
    };
    if result.is_ok() {
        let this_now = ctx.read_native_pin(roots_base, this);
        chm_note_table_high_water(ctx, this_now, false);
    }
    ctx.unpin_native_roots(roots_base);
    result
}

pub(crate) fn native_chm_compute(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key = args.get(1).copied().unwrap_or(Value::Object(None));
    let func = args.get(2).copied().unwrap_or(Value::Object(None));
    // C24 (HIGH): JDK rejects null key and null remappingFunction. MEASURED
    // message-less on the oracle — see `chm_bare_npe_on_null`.
    if matches!(key, Value::Object(None)) || matches!(func, Value::Object(None)) {
        return Err(bare_npe());
    }
    let roots_base = ctx.pin_native_root(this);
    let key_pin = pin_value(ctx, key);
    let func_pin = pin_value(ctx, func);
    let key = read_pinned_elem(ctx, key_pin, key);
    let hash = chm_key_hash(ctx, &key)?;
    let this = ctx.read_native_pin(roots_base, this);
    // `compute`'s segment monitor is REENTRANT, so a re-entrant call did not
    // hang here — it succeeded, and left the map holding a mapping the JDK
    // refuses to make. See `chm_reject_recursive_update`.
    if let Err(e) = chm_reject_recursive_update(&*ctx, this, hash) {
        ctx.unpin_native_roots(roots_base);
        return Err(e);
    }
    let _frame = ChmCallbackFrame::enter(roots_base, hash);
    let result = match chm_segment_for_mut(ctx, this, hash) {
        Some(seg) => {
            let _resize_flag = ChmResizeLockGuard::enter();
            // GC-pausable contended wait — re-read the pinned locals AFTER
            // acquiring (see `acquire_gc_safe`'s doc).
            let (_guard, seg) = ChmMonitorGuard::acquire_gc_safe(ctx, seg);
            let key = read_pinned_elem(ctx, key_pin, key);
            let func = read_pinned_elem(ctx, func_pin, func);
            // `CRATONVM_COMPAT_CHM_REMAP_SINGLE_HASH`: no second hash.
            if chm_remap_single_hash_enabled() {
                let null = Value::Object(None);
                chm_seg_remap_hashed(ctx, seg, key, null, func, hash, RemapOp::Compute)            } else {
                native_map_compute(ctx, &[Value::Object(Some(seg)), key, func])            }        }
        None => Ok(Some(Value::Object(None))),
    };
    if result.is_ok() {
        let this_now = ctx.read_native_pin(roots_base, this);
        chm_note_table_high_water(ctx, this_now, false);
    }
    ctx.unpin_native_roots(roots_base);
    result
}

pub(crate) fn native_chm_compute_if_present(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key = args.get(1).copied().unwrap_or(Value::Object(None));
    let func = args.get(2).copied().unwrap_or(Value::Object(None));
    // JDK ConcurrentHashMap rejects null key and null remappingFunction.
    // MEASURED message-less on the oracle — see `chm_bare_npe_on_null`.
    if matches!(key, Value::Object(None)) || matches!(func, Value::Object(None)) {
        return Err(bare_npe());
    }
    let roots_base = ctx.pin_native_root(this);
    let key_pin = pin_value(ctx, key);
    let func_pin = pin_value(ctx, func);
    let key = read_pinned_elem(ctx, key_pin, key);
    let hash = chm_key_hash(ctx, &key)?;
    let this = ctx.read_native_pin(roots_base, this);
    let result = match chm_segment_for(ctx, this, hash) {
        Some(seg) => {
            // JDK-exact lock-free fast path: `computeIfPresent` on an ABSENT
            // key returns null WITHOUT locking (real CHM's lock-free tabAt
            // probe). See `native_chm_compute_if_absent` for why avoiding the
            // segment monitor here matters (callback-under-coarse-lock
            // ordering cycles).
            let key_now = read_pinned_elem(ctx, key_pin, key);
            // `CRATONVM_COMPAT_CHM_REMAP_SINGLE_WALK`: the lock-free question
            // runs no `equals` (the JDK's is "is the bin empty"); the one
            // walk that does is `chm_seg_remap_single_walk`'s, locked.
            let maybe_present =
                if chm_remap_single_walk_enabled() && chm_remap_single_hash_enabled() {
                    chm_seg_bin_holds_hash(ctx, seg, hash)
                } else {
                    chm_seg_get(ctx, seg, key_now, hash)?.is_some()
                };
            match maybe_present {
                false => Ok(Some(Value::Object(None))),
                true => {
                    let this = ctx.read_native_pin(roots_base, this);
                    match chm_segment_for(ctx, this, hash) {
                        Some(seg) => {
                            let _resize_flag = ChmResizeLockGuard::enter();
                            // GC-pausable contended wait — re-read the pinned
                            // locals AFTER acquiring (see `acquire_gc_safe`).
                            let (_guard, seg) = ChmMonitorGuard::acquire_gc_safe(ctx, seg);
                            let key = read_pinned_elem(ctx, key_pin, key);
                            let func = read_pinned_elem(ctx, func_pin, func);
                            // `CRATONVM_COMPAT_CHM_REMAP_SINGLE_HASH`.
                            if chm_remap_single_hash_enabled() {
                                let null = Value::Object(None);
                                chm_seg_remap_hashed(
                                    ctx,
                                    seg,
                                    key,
                                    null,
                                    func,
                                    hash,
                                    RemapOp::IfPresent,
                                )
                            } else {
                                native_map_compute_if_present(
                                    ctx,
                                    &[Value::Object(Some(seg)), key, func],
                                )                            }                        }
                        None => Ok(Some(Value::Object(None))),                    }                }            }        }
        None => Ok(Some(Value::Object(None))),
    };
    ctx.unpin_native_roots(roots_base);
    result
}

pub(crate) fn native_chm_merge(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key = args.get(1).copied().unwrap_or(Value::Object(None));
    let value = args.get(2).copied().unwrap_or(Value::Object(None));
    let func = args.get(3).copied().unwrap_or(Value::Object(None));
    // C24 (HIGH): JDK rejects null key, null value, and null remappingFunction.
    // MEASURED message-less on the oracle — see `chm_bare_npe_on_null`.
    if matches!(key, Value::Object(None))
        || matches!(value, Value::Object(None))
        || matches!(func, Value::Object(None))
    {
        return Err(bare_npe());
    }
    let roots_base = ctx.pin_native_root(this);
    let key_pin = pin_value(ctx, key);
    let value_pin = pin_value(ctx, value);
    let func_pin = pin_value(ctx, func);
    let key = read_pinned_elem(ctx, key_pin, key);
    let hash = chm_key_hash(ctx, &key)?;
    let this = ctx.read_native_pin(roots_base, this);
    let result = match chm_segment_for_mut(ctx, this, hash) {
        Some(seg) => {
            let _resize_flag = ChmResizeLockGuard::enter();
            // GC-pausable contended wait — re-read the pinned locals AFTER
            // acquiring (see `acquire_gc_safe`'s doc).
            let (_guard, seg) = ChmMonitorGuard::acquire_gc_safe(ctx, seg);
            let key = read_pinned_elem(ctx, key_pin, key);
            let value = read_pinned_elem(ctx, value_pin, value);
            let func = read_pinned_elem(ctx, func_pin, func);
            // `CRATONVM_COMPAT_CHM_REMAP_SINGLE_HASH`.
            if chm_remap_single_hash_enabled() {
                chm_seg_remap_hashed(ctx, seg, key, value, func, hash, RemapOp::Merge)            } else {
                native_map_merge(ctx, &[Value::Object(Some(seg)), key, value, func])            }        }
        None => Ok(Some(Value::Object(None))),
    };
    if result.is_ok() {
        let this_now = ctx.read_native_pin(roots_base, this);
        chm_note_table_high_water(ctx, this_now, false);
    }
    ctx.unpin_native_roots(roots_base);
    result
}

// --- Bulk operations (iterate all segments) ---

pub(crate) fn native_chm_size(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let mut total = 0i32;
    for seg in chm_all_segments(ctx, this) {
        if let Value::Int(s) = ctx.get_field(seg, MAP_FIELD_SIZE) {
            total += s;
        }
    }
    Ok(Some(Value::Int(total)))
}

pub(crate) fn native_chm_is_empty(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let size = native_chm_size(ctx, args)?.unwrap_or(Value::Int(0));
    match size {
        Value::Int(0) => Ok(Some(Value::Int(1))),
        _ => Ok(Some(Value::Int(0))),
    }
}

pub(crate) fn native_chm_clear(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let _resize_flag = ChmResizeLockGuard::enter();
    // GC-safety: each per-segment lock wait is GC-pausable, so the segments
    // collected up front must survive (and be re-read) across iterations.
    // `this_pin` doubles as the unconditional unpin base.
    let this_pin = ctx.pin_native_root(this);
    let segs: Vec<Value> = chm_all_segments(ctx, this)
        .into_iter()
        .map(|s| Value::Object(Some(s)))
        .collect();
    let (_, seg_pins) = pin_value_slice(ctx, &segs);
    let result = (|| -> MethodCallResult {
        for i in 0..segs.len() {
            let seg = match read_pinned_elem(ctx, seg_pins[i], segs[i]) {
                Value::Object(Some(s)) => s,
                _ => continue,
            };
            let (_guard, seg) = ChmMonitorGuard::acquire_gc_safe(ctx, seg);
            native_map_clear(ctx, &[Value::Object(Some(seg))])?;        }
        Ok(None)
    })();
    ctx.unpin_native_roots(this_pin);
    result
}

pub(crate) fn native_chm_put_all(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    // `putAll(null)` dereferences its argument, so it is an NPE and not a
    // silent no-op. A missing argument is a dispatch defect rather than a
    // Java-visible null and keeps the old return — the distinction
    // `reject_null_functional` draws.
    //
    // G1-1 RECONCILED (retirement pass): the message is NOT a constant.
    // `ConcurrentHashMap.putAll`'s bytecode is `if (table != null) { ...
    // m.size() ... } m.entrySet()...` — MEASURED on HotSpot 25 both ways:
    // `new ConcurrentHashMap<>().putAll(null)` (never populated, `table`
    // still null) throws `entrySet()`/`m`; `m.put("k","v"); m.putAll(null)`
    // (table already allocated by the prior put) throws `size()`/`m`. G1-1's
    // own probe used the populated shape and recorded `size()/m`; an earlier
    // pass here measured only the empty shape and hardcoded `entrySet()/m`
    // for every receiver, which is right for a never-touched map and wrong
    // for the far more common already-populated one. `native_chm_size` > 0
    // approximates "table allocated" — it is not identical (a map emptied by
    // `remove` keeps its table but reads 0 here), but it is the only signal
    // this backend tracks and it matches both MEASURED states exactly.
    if matches!(args.get(1), Some(Value::Object(None))) {
        let table_allocated =
            matches!(native_chm_size(ctx, args), Ok(Some(Value::Int(n))) if n > 0);
        let message = if table_allocated {
            "Cannot invoke \"java.util.Map.size()\" because \"m\" is null"
        } else {
            "Cannot invoke \"java.util.Map.entrySet()\" because \"m\" is null"
        };
        return Err(RuntimeError::NullPointerException {
            message: Some(message.to_string()),
        }
        .into());
    }
    let source = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let _resize_flag = ChmResizeLockGuard::enter();
    // `collect_entries_any` (not the HashMap-only `map_collect_entries`) so a
    // TreeMap / LinkedHashMap / real-JDK unmodifiable-map source is copied in
    // full rather than read as an empty bucket table.
    //
    // GC-safety: the collected entries are raw `Value`s reused across
    // GC-triggering hashing/puts and GC-pausable lock waits — pin the whole
    // flattened slice (plus `this`, pinned BEFORE the GC-triggering
    // collection) and re-read each element right before use.
    let this_pin = ctx.pin_native_root(this);
    // C3W3-2 (`CRATONVM_COMPAT_MAP_COPY_JDK_QUESTIONS`, round 15 wave 4, lane
    // compat4): a program's map is asked `m.size()` only once the table exists
    // (`if (table != null) tryPresize(size() + m.size())`, approximated by a
    // non-zero count as the NPE message above is), then walked, one `putVal`
    // per entry as it arrives.
    if let Some(prog) = foreign_map_source(&*ctx, source) {
        let prog_pin = ctx.pin_native_root(prog);
        let walked = (|| -> Result<(), MethodCallFailed> {
            let this_now = ctx.read_native_pin(this_pin, this);
            if chm_entry_count(&*ctx, this_now) > 0 {
                let prog_now = ctx.read_native_pin(prog_pin, prog);
                let answer = ctx.invoke_virtual(prog_now, "size", "()I", &[])?;
                let this_now = ctx.read_native_pin(this_pin, this);
                if let Some(Value::Int(n)) = answer {
                    chm_note_presize(ctx, this_now, n.max(0) as usize);
                }
            }
            map_for_each_entry(
                ctx,
                (prog_pin, prog),
                MapSizeAsk::Skip,
                |ctx, key, value| chm_put_all_entry(ctx, (this_pin, this), key, value),
            )
        })();
        let this_now = ctx.read_native_pin(this_pin, this);
        chm_note_table_high_water(ctx, this_now, true);
        ctx.unpin_native_roots(this_pin);
        return walked.map(|()| None);
    }
    let collected = collect_entries_any(ctx, source);
    let entries = unpin_on_err(ctx, this_pin, collected)?;
    // `tryPresize(m.size())` before the first `putVal`; no Java runs in it.
    let this_now = ctx.read_native_pin(this_pin, this);
    chm_note_presize(ctx, this_now, entries.len());
    let flat: Vec<Value> = entries.iter().flat_map(|(k, v)| [*k, *v]).collect();
    let (_, flat_pins) = pin_value_slice(ctx, &flat);
    let result = (|| -> MethodCallResult {
        for i in 0..entries.len() {
            let key = read_pinned_elem(ctx, flat_pins[i * 2], flat[i * 2]);
            let value = read_pinned_elem(ctx, flat_pins[i * 2 + 1], flat[i * 2 + 1]);
            // `putAll` reaches `putVal` once per entry, so a null key or value
            // ANYWHERE in the source is the same message-less NPE a direct
            // `put` would raise — the source map's own null tolerance is not
            // inherited. `native_map_put` below is the HashMap body and accepts
            // both, which is why the check cannot be left to the callee.
            chm_bare_npe_on_null(&key, &value)?;
            let hash = chm_key_hash(ctx, &key)?;
            let this = ctx.read_native_pin(this_pin, this);
            if let Some(seg) = chm_segment_for_mut(ctx, this, hash) {
                let (_guard, seg) = ChmMonitorGuard::acquire_gc_safe(ctx, seg);
                let key = read_pinned_elem(ctx, flat_pins[i * 2], flat[i * 2]);
                let value = read_pinned_elem(ctx, flat_pins[i * 2 + 1], flat[i * 2 + 1]);
                // One `hashCode()` per entry, as `putVal` (`CRATONVM_COMPAT_CHM_SINGLE_HASH`).
                chm_seg_put_hashed(ctx, seg, key, value, hash, false)?;            }        }
        Ok(None)
    })();
    let this_now = ctx.read_native_pin(this_pin, this);
    chm_note_table_high_water(ctx, this_now, true);
    ctx.unpin_native_roots(this_pin);
    result
}

pub(crate) fn native_chm_replace_all(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    // RULE F. Without this the null function reached `native_map_replace_all`
    // once per segment, which returned its own silent no-op — so an empty CHM
    // and a populated one both answered "done". MEASURED bare NPE for both.
    // See `reject_null_functional`.
    reject_null_functional(args.get(1))?;
    let func = args.get(1).copied().unwrap_or(Value::Object(None));
    // NOT `native_map_replace_all` per segment any more. That body is the
    // `HashMap` one, where a function returning null is a legal way to map a
    // key to null; on a `ConcurrentHashMap` it is a
    // `NullPointerException` — the JDK's `replaceAll` re-checks its own
    // function's OUTPUT (`if (newValue == null) throw new
    // NullPointerException();`) precisely because the map cannot hold it.
    // MEASURED: `chm.replaceAll((k, v) -> null)` answered no-throw and left
    // the map with its old values, so the caller's mistake was invisible.
    //
    // The entry-collect-then-apply shape is `native_chm_put_all`'s, for its
    // GC discipline: the mapper is arbitrary Java and can move everything.
    // Applying through `native_chm_put` also gets the write-side null refusal
    // for free, but the explicit check above it is what names the CONTRACT
    // rather than relying on a callee to notice.
    let this_pin = ctx.pin_native_root(this);
    let func_pin = pin_value(ctx, func);
    let entries = chm_collect_all_entries(ctx, this);
    let flat: Vec<Value> = entries.iter().flat_map(|(k, v)| [*k, *v]).collect();
    let (_, flat_pins) = pin_value_slice(ctx, &flat);
    let result = (|| -> MethodCallResult {
        for i in 0..entries.len() {
            let key = read_pinned_elem(ctx, flat_pins[i * 2], flat[i * 2]);
            let value = read_pinned_elem(ctx, flat_pins[i * 2 + 1], flat[i * 2 + 1]);
            let f = match read_pinned_elem(ctx, func_pin, func) {
                Value::Object(Some(f)) => f,
                _ => return Ok(None),
            };
            let mapped = ctx.invoke_virtual(
                f,
                "apply",
                "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
                &[key, value],
            )?;
            let mapped = match mapped {
                Some(v) => v,
                None => Value::Object(None),
            };
            if matches!(mapped, Value::Object(None)) {
                return Err(bare_npe());
            }
            let this = ctx.read_native_pin(this_pin, this);
            let key = read_pinned_elem(ctx, flat_pins[i * 2], flat[i * 2]);
            native_chm_put(ctx, &[Value::Object(Some(this)), key, mapped])?;        }
        Ok(None)
    })();
    ctx.unpin_native_roots(this_pin);
    result
}

pub(crate) fn native_chm_for_each(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    // RULE F. G1-1 gave the CHM write path `chm_bare_npe_on_null` but did not
    // reach `forEach`/`replaceAll`, which take no key or value to test.
    // MEASURED bare NPE, empty receiver included. See `reject_null_functional`.
    reject_null_functional(args.get(1))?;
    let action = match args.get(1) {
        Some(Value::Object(Some(a))) => *a,
        _ => return Ok(None),
    };
    let entries = chm_collect_all_entries(ctx, this);
    let action_pin = ctx.pin_native_root(action);
    let flat: Vec<Value> = entries.iter().flat_map(|(k, v)| [*k, *v]).collect();
    let (_, flat_pins) = pin_value_slice(ctx, &flat);
    for i in 0..entries.len() {
        let action = ctx.read_native_pin(action_pin, action);
        let key = read_pinned_elem(ctx, flat_pins[i * 2], flat[i * 2]);
        let value = read_pinned_elem(ctx, flat_pins[i * 2 + 1], flat[i * 2 + 1]);
        if let Err(e) = ctx.invoke_virtual(
            action,
            "accept",
            "(Ljava/lang/Object;Ljava/lang/Object;)V",
            &[key, value],
        ) {
            ctx.unpin_native_roots(action_pin);
            return Err(e);
        }
    }
    ctx.unpin_native_roots(action_pin);
    Ok(None)
}

/// `ConcurrentHashMap.keySet()` — the JDK's `new KeySetView<>(this, null)`.
///
/// This used to build a `java/util/HashSet` carrying a view-backing that
/// remembered the source map, so reads resynced and `remove` wrote through.
/// That got the common paths right and three things wrong, all of which the
/// real `KeySetView` gets right for free now that it has its own natives:
///
/// * `(ConcurrentHashMap.KeySetView<K,?>) map.keySet()` threw
///   `ClassCastException` — the object was a `HashSet`.
/// * `add`/`addAll` SUCCEEDED, mutating the view's private snapshot and
///   silently losing the element at the next resync. A `keySet()` view has no
///   mapped value, so the JDK throws `UnsupportedOperationException`.
/// * `retainAll` did not write through at all (the map kept every element the
///   caller asked to drop), and `removeAll` returned `false` after removing.
///
/// The iteration order is unchanged: the view-backing read
/// `collect_keys_any` → `map_collect_keys` → `chm_collect_all_keys`, which is
/// exactly what the `KeySetView` natives read, so the HotSpot-faithful bucket
/// order `chm_reorder_by_virtual_bucket` reconstructs (and which Spring's
/// `SimpleAliasRegistry.getAliases` depends on) is preserved.
pub(crate) fn native_chm_key_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    // Refresh the real `table` mirror: this native already walks every entry,
    // so the rebuild is a constant factor and not a change of order. See
    // `chm_refresh_real_table`.
    chm_refresh_real_table(ctx, &mut this);
    // A null mapped value is what makes this view read-only, per the JDK.
    let view = make_key_set_view(ctx, this, Value::Object(None))?;
    Ok(Some(Value::Object(Some(view))))
}

pub(crate) fn native_chm_values(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    // Refresh the real `table` mirror: this native already walks every entry,
    // so the rebuild is a constant factor and not a change of order. See
    // `chm_refresh_real_table`.
    chm_refresh_real_table(ctx, &mut this);
    let vals = chm_collect_all_values(ctx, this);
    // Use the layout-aware ArrayList helpers: in real-JDK mode `elementData`
    // and `size` are NOT at slots 0/1 (`AbstractList.modCount` occupies an
    // earlier slot), so the previous hardcoded `set_field(list, 0/1, ...)`
    // wrote the backing array into the wrong slots and `values()` iterated
    // empty even though `size()` was correct.
    //
    // Live view (2026-07-22): reserve one extra trailing slot and stash the
    // source map there, exactly like the generic `native_map_values`
    // (HashMap) does — `values_view_source`/`resync_values_view` (see
    // `native_al_size`/`native_al_get`/`native_al_contains`/
    // `native_al_iterator`/etc.) detect that marker and re-collect the
    // current values from the live map before every read. Without it, this
    // ArrayList was a dead one-time snapshot: `map.values()` captured before
    // any entries existed (or before later puts) never grew, e.g. H2's
    // `Schema.getAllSequences()` (a raw `ConcurrentHashMap.values()` captured
    // once) permanently reported 0 regardless of how many sequences were
    // created afterward (`TestAlter.testAlterTableDropIdentityColumn`).
    //
    // cceres5: the list/array allocations can move the just-collected values
    // (and `this`, the source map stashed below) — pin + re-read before the
    // stores (see make_static_entry_set / native_chm_entry_set).
    let (elem_base, val_handles) = pin_value_slice(ctx, &vals);
    let this_pin = ctx.pin_native_root(this);
    let first_pin = if elem_base == usize::MAX {
        this_pin
    } else {
        elem_base
    };
    // Not `?` (gc-common w20-a): a refused carrier releases this frame's pins.
    let list = match alloc_view_carrier(ctx, "java/util/concurrent/ConcurrentHashMap$ValuesView") {
        Ok(l) => l,
        Err(e) => {
            ctx.unpin_native_roots(first_pin);
            return Err(e);
        }
    };
    let list_pin = ctx.pin_native_root(list);
    let cap = std::cmp::max(vals.len(), AL_DEFAULT_CAPACITY) + 1;
    let arr: ObjectRef = alloc_ref_array(ctx, cap);
    for (i, v) in vals.iter().enumerate() {
        let v = read_pinned_elem(ctx, val_handles[i], *v);
        let _ = ctx.set_array_element(arr, i, v);
    }
    let this = ctx.read_native_pin(this_pin, this);
    ctx.set_array_element(arr, cap - 1, Value::Object(Some(this)));
    let list = ctx.read_native_pin(list_pin, list);
    store_view_carrier_backref(ctx, list, this);
    al_set_data(ctx, list, arr);
    al_set_size(ctx, list, vals.len() as i32);
    ctx.unpin_native_roots(first_pin);
    Ok(Some(Value::Object(Some(list))))
}

pub(crate) fn native_chm_entry_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    // Refresh the real `table` mirror: this native already walks every entry,
    // so the rebuild is a constant factor and not a change of order. See
    // `chm_refresh_real_table`.
    chm_refresh_real_table(ctx, &mut this);
    let entries = chm_collect_all_entries(ctx, this);
    // cceres5: pin the snapshot + receiver before the first allocation below.
    let flat: Vec<Value> = entries.iter().flat_map(|(k, v)| [*k, *v]).collect();
    let (elem_base, flat_pins) = pin_value_slice(ctx, &flat);
    let this_pin = ctx.pin_native_root(this);
    let first_pin = if elem_base == usize::MAX {
        this_pin
    } else {
        elem_base
    };
    // Build an entrySet carrier holding Map.Entry objects. Each entry must be a
    // real `java/util/Map$Entry` (not raw Object cid=0) so user-bytecode
    // `checkcast Map$Entry` succeeds after iterating entrySet() — see
    // Spring DefaultSingletonBeanRegistry.destroyBean iterating the
    // dependentBeanMap (a ConcurrentHashMap) entrySet.
    let carrier = set_view_carrier_for(&*ctx, this, VIEW_KIND_ENTRYSET);
    let set = alloc_set_view_carrier(ctx, carrier)?;
    let set_pin = ctx.pin_native_root(set);
    let cap = std::cmp::max(entries.len().next_power_of_two(), MAP_DEFAULT_CAPACITY);
    // Live entrySet view: back the HashSet with a view-backing that remembers the
    // source ConcurrentHashMap (+ VIEW_KIND_ENTRYSET), exactly like
    // `native_chm_key_set` (keySet) and the generic `native_map_entry_set`
    // (HashMap). This is what makes removing THROUGH the set write through to the
    // map — `entrySet().remove(e)` / `entrySet().iterator().remove()` /
    // `entrySet().removeIf(...)` (see `native_hs_remove`, which pulls the source
    // off the backing and deletes `e.getKey()` from it). The previous plain
    // snapshot backing (`alloc_backing_map`, no source marker) made
    // `view_backing_source` return `None`, so those removals silently vanished:
    // Narayana's `TransactionReaper` drains its `pendingInsertions` CHM via
    // `entrySet().remove(entry)` in `ReaperElementManager.flushPending`, so with a
    // dead snapshot the timed-out transaction was never moved into the reaper's
    // sorted queue and never rolled back (Hibernate `TransactionTimeoutTest`).
    // Reads resync from the live map via `collect_entries_any`, which routes a
    // segmented CHM through `chm_collect_all_entries`.
    let mut this_now = ctx.read_native_pin(this_pin, this);
    let backing = alloc_view_backing(ctx, &mut this_now, VIEW_KIND_ENTRYSET, cap)?;
    let backing_pin = ctx.pin_native_root(backing);
    let set_now = ctx.read_native_pin(set_pin, set);
    hs_set_backing_map(ctx, set_now, backing);
    // cceres5: `alloc_live_entry`/`map_alloc_node` below can move the raw
    // snapshot refs still pending in `entries` (and `set`/`backing`/`this`
    // themselves); pin + re-read everything (see make_static_entry_set).
    let sentinel = Value::Int(1);
    for i in 0..entries.len() {
        let key = read_pinned_elem(ctx, flat_pins[i * 2], flat[i * 2]);
        let value = read_pinned_elem(ctx, flat_pins[i * 2 + 1], flat[i * 2 + 1]);
        let this_now = ctx.read_native_pin(this_pin, this);
        let entry_obj = alloc_live_entry(ctx, "java/util/Map$Entry", key, value, this_now)?;
        let entry_pin = ctx.pin_native_root(entry_obj);

        let hash = ctx.identity_hash_code(entry_obj);
        let backing_now = ctx.read_native_pin(backing_pin, backing);
        let (b, size, c) = map_state(ctx, backing_now);
        let b = match b {
            Some(b) => b,
            None => {
                ctx.unpin_native_roots(entry_pin);
                continue;
            }
        };
        let b_pin = ctx.pin_native_root(b);
        let idx = map_bucket_index(hash, c);
        let existing = ctx.get_array_element(b, idx);
        let head = match existing {
            Value::Object(obj_opt) => obj_opt,
            _ => None,
        };
        let entry_now = ctx.read_native_pin(entry_pin, entry_obj);
        let node = map_alloc_node(ctx, entry_now, sentinel, hash, head)?;
        let b_now = ctx.read_native_pin(b_pin, b);
        ctx.set_array_element(b_now, idx, Value::Object(Some(node)));
        let backing_now = ctx.read_native_pin(backing_pin, backing);
        set_map_size(ctx, backing_now, size + 1);
        ctx.unpin_native_roots(entry_pin);
    }
    let set_now = ctx.read_native_pin(set_pin, set);
    ctx.unpin_native_roots(first_pin);
    Ok(Some(Value::Object(Some(set_now))))
}

pub(crate) fn native_chm_to_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    // gen r4w4/rooting2: `obj_to_display_units` dispatches each key's and
    // value's `toString()`, so the collected entries are pinned for the whole
    // walk and each one is re-read at its use -- the `native_ksv_to_string`
    // shape. The first pin is the unpin base; with no object entries there is
    // nothing pinned and nothing to unpin.
    let entries = chm_collect_all_entries(ctx, this);
    let flat: Vec<Value> = entries.iter().flat_map(|(k, v)| [*k, *v]).collect();
    let (pin_base, pins) = pin_value_slice(ctx, &flat);
    let result = (|| -> MethodCallResult {
        let mut parts = Vec::with_capacity(entries.len());
        for i in 0..entries.len() {
            let key = read_pinned_elem(ctx, pins[2 * i], flat[2 * i]);
            let k = crate::obj_to_display_units(ctx, &key)?;
            let value = read_pinned_elem(ctx, pins[2 * i + 1], flat[2 * i + 1]);
            let v = crate::obj_to_display_units(ctx, &value)?;
            parts.push(crate::entry_units(&k, &v));
        }
        let s = crate::wrap_join("{", &parts, ", ", "}");
        let sref = ctx.create_string_from_units(&s);
        Ok(Some(Value::Object(Some(sref))))
    })();
    if pin_base != usize::MAX {
        ctx.unpin_native_roots(pin_base);
    }
    result
}

pub(crate) fn native_chm_hash_code(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    // gen r4w4/rooting2: each segment's hash runs its keys' and values'
    // `hashCode()`, so every segment after the first used to be hashed at a
    // vacated address (contributing 0). Pin the segment list across the walk,
    // as `native_chm_clear` does.
    let segs: Vec<Value> = chm_all_segments(ctx, this)
        .into_iter()
        .map(|s| Value::Object(Some(s)))
        .collect();
    let (pin_base, seg_pins) = pin_value_slice(ctx, &segs);
    let result = (|| -> MethodCallResult {
        let mut total = 0i32;
        for i in 0..segs.len() {
            let seg = read_pinned_elem(ctx, seg_pins[i], segs[i]);
            if let Some(Value::Int(h)) = native_map_hash_code(ctx, &[seg])? {
                total = total.wrapping_add(h);
            }
        }
        Ok(Some(Value::Int(total)))
    })();
    if pin_base != usize::MAX {
        ctx.unpin_native_roots(pin_base);
    }
    result
}

pub(crate) fn native_chm_equals(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let other = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    if std::ptr::eq(this.as_ptr(), other.as_ptr()) {
        return Ok(Some(Value::Int(1)));
    }
    // `ConcurrentHashMap.equals(o)` is `false` for a non-Map argument
    // (`if (!(o instanceof Map)) return false`).
    if !object_is_map(ctx, other) {
        return Ok(Some(Value::Int(0)));
    }
    // GC-safety: every `invoke_virtual` below runs arbitrary Java (`size`,
    // `get`, and the values' `equals`) and can complete a moving young GC.
    // `other` and every key/value in `our_entries` are bare Rust locals — the
    // Vec is not a GC root at all, so its contents would be pre-move addresses
    // (and an entry reachable only from it can be reclaimed outright). Root
    // them for the whole comparison and re-read each element at its use.
    let this_pin = ctx.pin_native_root(this);
    let other_h = ctx.pin_native_root(other);
    let our_entries = chm_collect_all_entries(ctx, this);
    let flat: Vec<Value> = our_entries.iter().flat_map(|(k, v)| [*k, *v]).collect();
    let (_flat_base, flat_pins) = pin_value_slice(ctx, &flat);
    let result = native_chm_equals_pinned(ctx, other, other_h, &flat, &flat_pins);
    ctx.unpin_native_roots(this_pin);
    result
}

pub(crate) fn native_chm_equals_pinned(
    ctx: &mut dyn NativeContext,
    other: ObjectRef,
    other_h: usize,
    flat: &[Value],
    flat_pins: &[usize],
) -> MethodCallResult {
    let other = ctx.read_native_pin(other_h, other);
    // `other` may be ANY `Map`, not just a (segmented) ConcurrentHashMap —
    // HotSpot's `CHM.equals` does `m.get(p.key)` / iterates `m.entrySet()` via
    // the `Map` interface. Reading `other` through the CHM-specific
    // `native_chm_size` / `native_chm_get` reported size 0 / no entries for a
    // plain HashMap argument, so `chm.equals(hashMap)` wrongly returned false.
    // Route `other` access through virtual dispatch (`other.size()` /
    // `other.get(k)`) so it resolves to whatever natives/bytecode `other`'s real
    // class provides — same cross-implementation fix as `native_map_equals`.
    let other_size = match ctx.invoke_virtual(other, "size", "()I", &[])? {
        Some(Value::Int(s)) => s,
        _ => {
            // Defensive: a Map whose size() didn't yield an int — fall back to
            // the CHM-native read rather than mis-comparing. `size()` above ran
            // Java, so `other` is re-read from its pin (gen r4w4/rooting2).
            let other = ctx.read_native_pin(other_h, other);
            match native_chm_size(ctx, &[Value::Object(Some(other))])? {
                Some(Value::Int(s)) => s,
                _ => 0,
            }
        }
    };
    if other_size != (flat.len() / 2) as i32 {
        return Ok(Some(Value::Int(0)));
    }
    // Compare each value with the `Map.equals` contract: `v.equals(otherVal)`.
    // `values_equal` alone only knew identity / String contents / enum identity,
    // so it wrongly reported unequal for equal-but-not-identical object values
    // (e.g. two maps holding equal `List`/bean values). Route object/object
    // pairs through `map_keys_equal` (which dispatches the value's real Java
    // `equals`); keep `values_equal` for primitive/null/mixed cases.
    //
    // NOTE: unlike `AbstractMap.equals` (the HashMap family, see
    // `native_map_equals`), `ConcurrentHashMap.equals` does NOT wrap this in
    // `catch (ClassCastException | NullPointerException) { return false; }` — it
    // dispatches `v.equals(val)` directly and lets such an exception propagate.
    // So we propagate here too (the `?`), matching HotSpot.
    for i in 0..(flat.len() / 2) {
        let other = ctx.read_native_pin(other_h, other);
        let key = read_pinned_elem(ctx, flat_pins[i * 2], flat[i * 2]);
        let other_val = ctx
            .invoke_virtual(
                other,
                "get",
                "(Ljava/lang/Object;)Ljava/lang/Object;",
                &[key],
            )?
            .unwrap_or(Value::Object(None));
        // `get` above ran arbitrary Java; refresh our value before comparing,
        // and root `other_val` across `map_keys_equal` (which dispatches the
        // value's own `equals`).
        let value = read_pinned_elem(ctx, flat_pins[i * 2 + 1], flat[i * 2 + 1]);
        let other_val_pin = pin_value(ctx, other_val);
        let eq = match (value, other_val) {
            (Value::Object(Some(va)), Value::Object(Some(vb))) => map_keys_equal(ctx, va, vb)?,
            _ => values_equal(ctx, &value, &other_val),
        };
        if other_val_pin != usize::MAX {
            ctx.unpin_native_roots(other_val_pin);
        }
        if !eq {
            return Ok(Some(Value::Int(0)));
        }
    }
    Ok(Some(Value::Int(1)))
}

// --- ConcurrentHashMap-specific compound operations ---

pub(crate) fn native_chm_remove_kv(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let key = args.get(1).copied().unwrap_or(Value::Object(None));
    let expected_val = args.get(2).copied().unwrap_or(Value::Object(None));
    // WRITE path, so the explicit refusal, not the helpful-NPE one:
    // `ConcurrentHashMap.remove(Object,Object)` opens with
    // `if (key == null) throw new NullPointerException();` and then answers
    // `false` for a null value rather than throwing. MEASURED:
    // `remove(null,v)` and `remove(null,null)` are `<NO-MESSAGE>`,
    // `remove(k,null)` is `OK -> false`.
    if matches!(key, Value::Object(None)) {
        return Err(bare_npe());
    }
    // G1-1 RECONCILED (retirement pass): the real body is
    // `return value != null && replaceNode(key, null, value) != null;` — the
    // null-value short circuit comes BEFORE any lookup, so an ABSENT key with
    // a null expected value must answer `false` without ever comparing
    // "current is absent" (`null`) to "expected is null" and calling that a
    // match. MEASURED: `chm.remove("zz", null)` on an empty-of-"zz" map is
    // `false`, not `true`. The old code below reached `values_equal(current,
    // expected_val)` with both sides `Value::Object(None)` in exactly that
    // case and treated it as equal.
    if matches!(expected_val, Value::Object(None)) {
        return Ok(Some(Value::Int(0)));
    }
    // GC-safety: pin across hashCode()/equals() dispatch and the
    // GC-pausable segment-lock wait — see `native_chm_put`.
    let this_pin = ctx.pin_native_root(this);
    let key_pin = pin_value(ctx, key);
    let expected_pin = pin_value(ctx, expected_val);
    let result = (|| -> MethodCallResult {
        let key = read_pinned_elem(ctx, key_pin, key);
        let hash = chm_key_hash(ctx, &key)?;
        let this = ctx.read_native_pin(this_pin, this);
        match chm_segment_for(ctx, this, hash) {
            Some(seg) => {
                let _resize_flag = ChmResizeLockGuard::enter();
                let (_guard, seg) = ChmMonitorGuard::acquire_gc_safe(ctx, seg);
                let seg_pin = ctx.pin_native_root(seg);
                let key = read_pinned_elem(ctx, key_pin, key);
                let current = native_map_get(ctx, &[Value::Object(Some(seg)), key])?
                    .unwrap_or(Value::Object(None));
                let current_pin = pin_value(ctx, current);
                let expected_val = read_pinned_elem(ctx, expected_pin, expected_val);
                let current = read_pinned_elem(ctx, current_pin, current);
                // `replaceNode`: `cv == ev || (ev != null && cv.equals(ev))`,
                // the ARGUMENT's `equals` (`CRATONVM_COMPAT_CHM_VALUE_EQUALS`).
                // It may run Java; `seg`/`key` are re-read from pins below.
                let eq = if chm_value_equals_enabled() {
                    values_equal_deep(ctx, &expected_val, &current)?
                } else {
                    values_equal(ctx, &current, &expected_val)
                };
                let r = if eq {
                    let seg = ctx.read_native_pin(seg_pin, seg);
                    let key = read_pinned_elem(ctx, key_pin, key);
                    native_map_remove(ctx, &[Value::Object(Some(seg)), key])?;
                    Ok(Some(Value::Int(1)))
                } else {
                    Ok(Some(Value::Int(0)))
                };
                ctx.unpin_native_roots(seg_pin);
                r
            }
            None => Ok(Some(Value::Int(0))),
        }
    })();
    ctx.unpin_native_roots(this_pin);
    result
}

pub(crate) fn native_chm_replace(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key = args.get(1).copied().unwrap_or(Value::Object(None));
    let new_val = args.get(2).copied().unwrap_or(Value::Object(None));
    // C24 (HIGH): JDK rejects null key/value. MEASURED message-less on the
    // oracle — see `chm_bare_npe_on_null`.
    chm_bare_npe_on_null(&key, &new_val)?;
    // GC-safety: pin across hashCode() dispatch, the GC-pausable segment-lock
    // wait, and the GC-capable put (the returned `current` must be the
    // relocated address, not the pre-put one) — see `native_chm_put`.
    let this_pin = ctx.pin_native_root(this);
    let key_pin = pin_value(ctx, key);
    let new_val_pin = pin_value(ctx, new_val);
    let result = (|| -> MethodCallResult {
        let key = read_pinned_elem(ctx, key_pin, key);
        let hash = chm_key_hash(ctx, &key)?;
        let this = ctx.read_native_pin(this_pin, this);
        match chm_segment_for(ctx, this, hash) {
            Some(seg) => {
                let _resize_flag = ChmResizeLockGuard::enter();
                let (_guard, seg) = ChmMonitorGuard::acquire_gc_safe(ctx, seg);
                let seg_pin = ctx.pin_native_root(seg);
                let key = read_pinned_elem(ctx, key_pin, key);
                let current = native_map_get(ctx, &[Value::Object(Some(seg)), key])?
                    .unwrap_or(Value::Object(None));
                let current_pin = pin_value(ctx, current);
                let r = match current {
                    Value::Object(None) => Ok(Some(Value::Object(None))),
                    _ => {
                        let seg = ctx.read_native_pin(seg_pin, seg);
                        let key = read_pinned_elem(ctx, key_pin, key);
                        let new_val = read_pinned_elem(ctx, new_val_pin, new_val);
                        native_map_put(ctx, &[Value::Object(Some(seg)), key, new_val])?;
                        Ok(Some(read_pinned_elem(ctx, current_pin, current)))
                    }
                };
                ctx.unpin_native_roots(seg_pin);
                r
            }
            None => Ok(Some(Value::Object(None))),
        }
    })();
    ctx.unpin_native_roots(this_pin);
    result
}

pub(crate) fn native_chm_replace_kv(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let key = args.get(1).copied().unwrap_or(Value::Object(None));
    let old_val = args.get(2).copied().unwrap_or(Value::Object(None));
    let new_val = args.get(3).copied().unwrap_or(Value::Object(None));
    // C24 (HIGH): JDK rejects null key/old/new. MEASURED message-less on the
    // oracle — see `chm_bare_npe_on_null`.
    if matches!(key, Value::Object(None))
        || matches!(old_val, Value::Object(None))
        || matches!(new_val, Value::Object(None))
    {
        return Err(bare_npe());
    }
    // GC-safety: pin across hashCode()/equals() dispatch and the
    // GC-pausable segment-lock wait — see `native_chm_put`.
    let this_pin = ctx.pin_native_root(this);
    let key_pin = pin_value(ctx, key);
    let old_pin = pin_value(ctx, old_val);
    let new_pin = pin_value(ctx, new_val);
    let result = (|| -> MethodCallResult {
        let key = read_pinned_elem(ctx, key_pin, key);
        let hash = chm_key_hash(ctx, &key)?;
        let this = ctx.read_native_pin(this_pin, this);
        match chm_segment_for(ctx, this, hash) {
            Some(seg) => {
                let _resize_flag = ChmResizeLockGuard::enter();
                let (_guard, seg) = ChmMonitorGuard::acquire_gc_safe(ctx, seg);
                let seg_pin = ctx.pin_native_root(seg);
                let key = read_pinned_elem(ctx, key_pin, key);
                let current = native_map_get(ctx, &[Value::Object(Some(seg)), key])?
                    .unwrap_or(Value::Object(None));
                let current_pin = pin_value(ctx, current);
                let old_val = read_pinned_elem(ctx, old_pin, old_val);
                let current = read_pinned_elem(ctx, current_pin, current);
                // `replaceNode`: `cv == ev || (ev != null && cv.equals(ev))`,
                // the ARGUMENT's `equals` (`CRATONVM_COMPAT_CHM_VALUE_EQUALS`).
                // It may run Java; `seg`/`key`/`new_val` are re-read below.
                let eq = if chm_value_equals_enabled() {
                    values_equal_deep(ctx, &old_val, &current)?
                } else {
                    values_equal(ctx, &current, &old_val)
                };
                let r = if eq {
                    let seg = ctx.read_native_pin(seg_pin, seg);
                    let key = read_pinned_elem(ctx, key_pin, key);
                    let new_val = read_pinned_elem(ctx, new_pin, new_val);
                    native_map_put(ctx, &[Value::Object(Some(seg)), key, new_val])?;
                    Ok(Some(Value::Int(1)))
                } else {
                    Ok(Some(Value::Int(0)))
                };
                ctx.unpin_native_roots(seg_pin);
                r
            }
            None => Ok(Some(Value::Int(0))),
        }
    })();
    ctx.unpin_native_roots(this_pin);
    result
}

pub(crate) fn native_chm_for_each_parallel(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    // RULE F on argument **2**, not 1: this is
    // `forEach(long parallelismThreshold, BiConsumer)`, whose first parameter
    // is a `long`. Confirming which argument is the function before copying the
    // guard is the point — the sibling `native_chm_for_each` checks arg 1.
    reject_null_functional(args.get(2))?;
    let action = match args.get(2) {
        Some(Value::Object(Some(a))) => *a,
        _ => return Ok(None),
    };
    let entries = chm_collect_all_entries(ctx, this);
    let action_pin = ctx.pin_native_root(action);
    let flat: Vec<Value> = entries.iter().flat_map(|(k, v)| [*k, *v]).collect();
    let (_, flat_pins) = pin_value_slice(ctx, &flat);
    for i in 0..entries.len() {
        let action = ctx.read_native_pin(action_pin, action);
        let key = read_pinned_elem(ctx, flat_pins[i * 2], flat[i * 2]);
        let value = read_pinned_elem(ctx, flat_pins[i * 2 + 1], flat[i * 2 + 1]);
        if let Err(e) = ctx.invoke_virtual(
            action,
            "accept",
            "(Ljava/lang/Object;Ljava/lang/Object;)V",
            &[key, value],
        ) {
            ctx.unpin_native_roots(action_pin);
            return Err(e);
        }
    }
    ctx.unpin_native_roots(action_pin);
    Ok(None)
}

/// `ConcurrentHashMap.reduceValues(long parallelismThreshold, BiFunction reducer)`
/// — "the result of accumulating all values using the given reducer to combine
/// values, or null if none".
///
/// Measured `CHM.reduceValues=null` against HotSpot's `6`, on a map whose
/// ordinary `entrySet` iteration answered correctly in the SAME run
/// (`CHM.sortedContent={a=6}` matches, and that is `new TreeMap<>(chm)` walking
/// `entrySet`). The split is the `Traverser`: every method in the bulk-operation
/// family walks the real `table` field, and a natively-backed CHM keeps its
/// entries in the segmented side layout that never populates `table` — so the
/// real bytecode traverses an empty tree and reports "no values".
///
/// That is the worst answer available. `null` from `reduceValues` is exactly
/// what an empty map returns, so a caller cannot tell a lost traversal from a
/// legitimately empty reduction — the quiet shape the round-2 widening was
/// written to catch. `forEach(long, BiConsumer)` above was registered for the
/// same reason.
///
/// The fold matches `ReduceValuesTask`: seed with the first value, then
/// `reducer.apply(acc, next)`. The `parallelismThreshold` argument is read and
/// ignored, as it may be: it is a *hint*, and a sequential evaluation is a legal
/// answer for any value of it.
pub(crate) fn native_chm_reduce_values(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    // `if (reducer == null) throw new NullPointerException();` — the first
    // statement of the real method.
    let reducer = match args.get(2) {
        Some(Value::Object(Some(f))) => *f,
        Some(Value::Object(None)) => {
            return Err(RuntimeError::NullPointerException { message: None }.into())        }
        _ => return Ok(Some(Value::Object(None))),
    };
    let values = chm_collect_all_values(ctx, this);
    // GC-SAFETY: `apply` is arbitrary interpreted bytecode. It can move the
    // reducer, every value still held only in this Rust Vec, and the accumulator
    // — which is the one that matters most, because it is fed back in as an
    // argument on the very next iteration.
    let reducer_pin = ctx.pin_native_root(reducer);
    let (_, value_pins) = pin_value_slice(ctx, &values);
    let mut acc: Option<Value> = None;
    let mut acc_pin = usize::MAX;
    let mut result: Result<Option<Value>, MethodCallFailed> = Ok(None);
    for i in 0..values.len() {
        let next = read_pinned_elem(ctx, value_pins[i], values[i]);
        let Some(prev) = acc else {
            acc = Some(next);
            acc_pin = pin_value(ctx, next);
            continue;
        };
        let reducer = ctx.read_native_pin(reducer_pin, reducer);
        let prev = read_pinned_elem(ctx, acc_pin, prev);
        match ctx.invoke_virtual(
            reducer,
            "apply",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            &[prev, next],
        ) {
            Ok(r) => {
                let combined = r.unwrap_or(Value::Object(None));
                acc = Some(combined);
                acc_pin = pin_value(ctx, combined);
            }
            Err(e) => {
                result = Err(e);
                break;
            }
        }
    }
    if let Ok(ref mut out) = result {
        *out = Some(match acc {
            Some(v) => read_pinned_elem(ctx, acc_pin, v),
            None => Value::Object(None),
        });
    }
    ctx.unpin_native_roots(reducer_pin);
    result
}

/// `ConcurrentHashMap.searchKeys(long parallelismThreshold, Function searchFunction)`
/// — "a non-null result from applying the given search function on each key, or
/// null if none".
///
/// Same cause and same silence as [`native_chm_reduce_values`]: measured
/// `CHM.searchKeys=null` against HotSpot's `found`, where `null` is also the
/// legitimate "nothing matched" answer. Short-circuits on the first non-null
/// result, as `SearchKeysTask` does.
pub(crate) fn native_chm_search_keys(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let search_fn = match args.get(2) {
        Some(Value::Object(Some(f))) => *f,
        Some(Value::Object(None)) => {
            return Err(RuntimeError::NullPointerException { message: None }.into())        }
        _ => return Ok(Some(Value::Object(None))),
    };
    let keys = chm_collect_all_keys(ctx, this);
    let fn_pin = ctx.pin_native_root(search_fn);
    let (_, key_pins) = pin_value_slice(ctx, &keys);
    let mut result: MethodCallResult = Ok(Some(Value::Object(None)));
    for i in 0..keys.len() {
        let search_fn = ctx.read_native_pin(fn_pin, search_fn);
        let key = read_pinned_elem(ctx, key_pins[i], keys[i]);
        match ctx.invoke_virtual(
            search_fn,
            "apply",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            &[key],
        ) {
            Ok(Some(v @ Value::Object(Some(_)))) => {
                result = Ok(Some(v));
                break;
            }
            Ok(_) => {}
            Err(e) => {
                result = Err(e);
                break;
            }
        }
    }
    ctx.unpin_native_roots(fn_pin);
    result
}

pub(crate) fn native_chm_mapping_count(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let size = native_chm_size(ctx, args)?.unwrap_or(Value::Int(0));
    match size {
        Value::Int(v) => Ok(Some(Value::Long(v as i64))),
        _ => Ok(Some(Value::Long(0))),
    }
}

/// `ConcurrentHashMap.newKeySet()` — a real `KeySetView` over a real
/// native-backed `ConcurrentHashMap`, mapped value `Boolean.TRUE`.
///
/// This used to return a `java/util/HashSet` because a real `KeySetView`'s
/// Java-side `add()` runs `CHM.putVal`, which walks the `table` field
/// CratonVM's segmented layout never populates. The answer to that is not to
/// hand back a different class — a `HashSet` takes no lock, so the JDK's
/// canonical concurrent set corrupted under concurrent mutation and its
/// `size()` could go negative — it is to give `KeySetView` its own natives, so
/// no method of the view is left running `table`-walking bytecode. See the
/// module comment above `KSV_CLASS`.
pub(crate) fn native_chm_new_key_set(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let map = alloc_backing_chm(ctx, None)?;
    // `Boolean.valueOf` is arbitrary Java and can complete a moving GC, so the
    // fresh map — which nothing points at yet — has to stay rooted across it.
    let (map, mapped) = rooted_across1(ctx, map, ksv_boxed_true);
    let view = make_key_set_view(ctx, map, mapped)?;
    Ok(Some(Value::Object(Some(view))))
}

/// `ConcurrentHashMap.newKeySet(int initialCapacity)`. Static, so the capacity
/// is the first argument — but the dispatch path can shift argument positions
/// (see `newSetFromMap`), hence the search rather than a fixed index.
pub(crate) fn native_chm_new_key_set_cap(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // `newKeySet(int)` is `new KeySetView<>(new ConcurrentHashMap<>(cap), TRUE)`,
    // so it inherits the constructor's refusal — and the `find_map` below,
    // which skips any non-positive argument, was exactly the clamp that hid it.
    if let Some(Value::Int(c)) = args.iter().find(|v| matches!(v, Value::Int(_))) {
        chm_reject_bad_ctor_args(Some(&Value::Int(*c)), None, None)?;
    }
    let cap = args.iter().find_map(|v| match v {
        Value::Int(n) if *n > 0 => Some(*n),
        _ => None,
    });
    let map = alloc_backing_chm(ctx, cap)?;
    let (map, mapped) = rooted_across1(ctx, map, ksv_boxed_true);
    let view = make_key_set_view(ctx, map, mapped)?;
    Ok(Some(Value::Object(Some(view))))
}

/// `ConcurrentHashMap.keySet(V mappedValue)` — a LIVE, add-able view of this
/// map's keys, exactly as the JDK's `new KeySetView<>(this, mappedValue)`.
///
/// Previously this delegated to `keySet()`, whose product has no `add` (a
/// `keySet()` view must not be add-able) and is not a `KeySetView`, so the one
/// thing this overload exists for did not work.
pub(crate) fn native_chm_key_set_view(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let mapped = args.get(1).copied().unwrap_or(Value::Object(None));
    // MEASURED 2026-08-16: `keySet(null)` is `msg=<NO-MESSAGE>`. The JDK body
    // opens `0: aload_1 / 1: ifnonnull 12 / 4: new NullPointerException /
    // 8: invokespecial <init>()V / 11: athrow` — the no-arg constructor again.
    // `"ConcurrentHashMap.keySet(null)"` was the tenth fabricated string in
    // this family; see G1-1.
    if matches!(mapped, Value::Object(None)) {
        return Err(bare_npe());
    }
    let view = make_key_set_view(ctx, this, mapped)?;
    Ok(Some(Value::Object(Some(view))))
}

// ===========================================================================
// ConcurrentHashMap$KeySetView — the object `newKeySet()` / `keySet(V)` returns
// ===========================================================================
//
// `newKeySet()` used to hand back a plain `java/util/HashSet`. Two things were
// wrong with that. It is not the class the JDK promises, so
// `(ConcurrentHashMap.KeySetView<K,?>) set` threw `ClassCastException` and
// `getMappedValue()` was a `NoSuchMethodError`. And, far worse, a `HashSet`'s
// `add`/`remove`/`size` run the `HashMap` natives, which take no lock — so
// `ConcurrentHashMap.newKeySet()`, the JDK's canonical "give me a concurrent
// set", was answered with a structure that corrupts under concurrent mutation.
// Balanced churn (every `add` followed by its own `remove`, correct final size
// 0 on any interleaving) left the size at 18, at -3, at 131; a real
// `ConcurrentHashMap.size()` cannot even go negative. `isEmpty()` is
// `size() == 0`, and `ThreadPerTaskExecutor.tryTerminate()` only advances
// SHUTDOWN → TERMINATED when its `newKeySet()` of live threads reports empty —
// so `Executors.newVirtualThreadPerTaskExecutor()` in a try-with-resources hung
// forever on the closing brace. See the retired
// `concurrenthashmap-newkeyset-returns-a-plain-hashset` write-up.
//
// The view is now a real `ConcurrentHashMap$KeySetView` over a real
// native-backed `ConcurrentHashMap`, so every mutation inherits the per-segment
// monitor that already makes `native_chm_put` / `native_chm_remove` safe, and
// `size()` is the sum of per-segment counts that a writer only ever updates
// under that segment's lock.
//
// The surface below is deliberately COMPLETE rather than "the methods the
// reproducer needs". In real-JDK mode the methods `CollectionView` declares
// (`size` / `isEmpty` / `clear` / `toArray` / `toString` / `containsAll` /
// `removeAll` / `retainAll`) are written in terms of `map` — already native —
// or of `iterator()`, native below, so their real bytecode is correct and
// these registrations are simply never consulted for them. In synthetic-jdk
// mode there is no `CollectionView` bytecode at all and the same registrations
// are the only implementation. Leaving any of them out would reproduce the
// partial-surface trap the known-issue doc warns about: a method left to real
// bytecode walks the `table` field CratonVM's segmented layout never
// populates, and quietly answers empty.

pub(crate) const KSV_CLASS: &str = "java/util/concurrent/ConcurrentHashMap$KeySetView";
pub(crate) const CHM_CLASS: &str = "java/util/concurrent/ConcurrentHashMap";
/// `CollectionView.map`, slot 0 in the JDK's layout. Also the fallback when the
/// field cannot be resolved by name — synthetic-jdk mode fabricates the class,
/// so there is no declared field list to resolve against.
pub(crate) const KSV_FIELD_MAP: usize = 0;
/// `KeySetView.value` — the value `add(e)` maps `e` to.
pub(crate) const KSV_FIELD_VALUE: usize = 1;
pub(crate) const KSV_NUM_FIELDS: usize = 2;

/// Slot index of `name` on `this`, preferring the class's own declared layout
/// and falling back to the synthetic slot. Resolved by `ClassId` rather than by
/// class name so a redefined/duplicate-loader `ConcurrentHashMap` still
/// resolves (see `resolve_field_index_by_class_id`).
pub(crate) fn ksv_slot(ctx: &dyn NativeContext, this: ObjectRef, name: &str, fallback: usize) -> usize {
    let n = ctx.object_num_fields(this);
    let cid = ctx.class_id_of_object(this);
    match ctx.resolve_field_index_by_class_id(cid, name) {
        Some(i) if i < n => i,
        _ => fallback,
    }
}

/// The backing `ConcurrentHashMap` of a `KeySetView`.
pub(crate) fn ksv_map(ctx: &dyn NativeContext, this: ObjectRef) -> Option<ObjectRef> {
    let slot = ksv_slot(ctx, this, "map", KSV_FIELD_MAP);
    if slot >= ctx.object_num_fields(this) {
        return None;
    }
    match ctx.get_field(this, slot) {
        Value::Object(Some(m)) => Some(m),
        _ => None,
    }
}

/// `this`'s mapped value — what `add(e)` stores against `e`. `Object(None)` for
/// a view that has none, which is exactly the JDK's `add` → `UnsupportedOperation`
/// condition. Deliberately allocation-free: callers read it next to a
/// `ksv_map` read and must not have a GC in between.
pub(crate) fn ksv_mapped_value(ctx: &dyn NativeContext, this: ObjectRef) -> Value {
    let slot = ksv_slot(ctx, this, "value", KSV_FIELD_VALUE);
    if slot >= ctx.object_num_fields(this) {
        return Value::Object(None);
    }
    ctx.get_field(this, slot)
}

/// True when `obj`'s runtime class is `ConcurrentHashMap$KeySetView`.
#[inline]
pub(crate) fn is_key_set_view(ctx: &dyn NativeContext, obj: ObjectRef) -> bool {
    receiver_facts(ctx, obj).has(CF_KEY_SET_VIEW)
}

/// Reroute a `KeySetView` receiver that arrived through a generic
/// `HashSet`/`ArrayList`-shaped native to the view's own implementation.
///
/// Needed because the *interface* registrations — `java/util/Set.size`,
/// `java/util/Collection.iterator`, `java/util/AbstractSet.hashCode`, … — are
/// what a call site resolves to when the receiver's class declares no such
/// method, which in synthetic-jdk mode is every method of the fabricated
/// `KeySetView` class. Those natives win over the exact-class registrations,
/// and a `KeySetView` matches neither layout they read, so `size()` answered 0
/// and `iterator()` answered null (a `for (x : set)` NPE) while `add` /
/// `contains` / `remove` — which have no interface-level registration and so
/// fall through to the exact-class natives — worked. Guarding at the generic
/// entry points is what keeps that split from existing at all.
#[inline]
pub(crate) fn ksv_route(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    imp: fn(&mut dyn NativeContext, &[Value]) -> MethodCallResult,
) -> Option<MethodCallResult> {
    match args.first() {
        Some(Value::Object(Some(o))) if is_key_set_view(ctx, *o) => Some(imp(ctx, args)),
        _ => None,
    }
}

/// `Boolean.TRUE` — `newKeySet()`'s mapped value in the JDK. Falls back to the
/// unboxed truth so a fabricated view still has a non-null mapped value (the
/// CHM natives reject a null value per JDK spec, so `add` would otherwise NPE).
pub(crate) fn ksv_boxed_true(ctx: &mut dyn NativeContext) -> Value {
    match ctx.invoke(
        "java/lang/Boolean",
        "valueOf",
        "(Z)Ljava/lang/Boolean;",
        &[Value::Int(1)],
    ) {
        Ok(Some(v @ Value::Object(Some(_)))) => v,
        _ => Value::Int(1),
    }
}

/// Allocate a `KeySetView` instance with no constructor run.
///
/// In real-JDK mode allocate with the class's OWN field count rather than the
/// hard-coded two: `set_field` past the end of an under-sized object is dropped
/// silently, so a JDK that ever adds a field would leave `map` unset and every
/// method below answering empty.
pub(crate) fn alloc_key_set_view_object(ctx: &mut dyn NativeContext) -> Result<ObjectRef, MethodCallFailed> {
    // `ensure_class_initialized` can report success having FABRICATED a
    // stand-in, so check the resolved name rather than trusting `Ok`.
    let mut real_cid = None;
    if let Ok(cid) = ctx.ensure_class_initialized(KSV_CLASS) {
        if ctx.class_name_arc_of_id(cid).as_deref() == Some(KSV_CLASS) {
            real_cid = Some(cid);
        }
    }
    match real_cid {
        Some(cid) => {
            let n = ctx.class_num_total_fields(cid).max(KSV_NUM_FIELDS);
            Ok(ctx.alloc_object(cid, n))
        }
        None => try_alloc_synthetic(ctx, KSV_CLASS, KSV_NUM_FIELDS),
    }
}

/// Allocate a fresh, fully segmented native `ConcurrentHashMap` with no
/// constructor run — the backing store for a `newKeySet()` view.
///
/// `initial_capacity` mirrors `newKeySet(int)`; `None` reproduces the no-arg
/// `ConcurrentHashMap()` shape so iteration order matches a default map.
pub(crate) fn alloc_backing_chm(
    ctx: &mut dyn NativeContext,
    initial_capacity: Option<i32>,
) -> Result<ObjectRef, MethodCallFailed> {
    let mut real_cid = None;
    if let Ok(cid) = ctx.ensure_class_initialized(CHM_CLASS) {
        if ctx.class_name_arc_of_id(cid).as_deref() == Some(CHM_CLASS) {
            real_cid = Some(cid);
        }
    }
    let chm = match real_cid {
        Some(cid) => {
            let n = ctx.class_num_total_fields(cid).max(_CHM_NUM_FIELDS);
            ctx.alloc_object(cid, n)
        }
        None => try_alloc_synthetic(ctx, CHM_CLASS, _CHM_NUM_FIELDS)?,
    };
    // Segment construction is 2N+1 allocations deep; keep the fresh map rooted
    // across it, since nothing points at it yet.
    let chm_pin = ctx.pin_native_root(chm);
    match initial_capacity {
        Some(cap) => {
            let _ = native_chm_init_capacity(ctx, &[Value::Object(Some(chm)), Value::Int(cap)]);
        }
        None => chm_init_segments(ctx, chm, CHM_DEFAULT_INIT_SEGMENTS, CHM_DEFAULT_SEGMENT_CAP),
    }
    let chm = ctx.read_native_pin(chm_pin, chm);
    ctx.unpin_native_roots(chm_pin);
    Ok(chm)
}

/// Build a `KeySetView` over `map` with mapped value `mapped`.
pub(crate) fn make_key_set_view(
    ctx: &mut dyn NativeContext,
    map: ObjectRef,
    mapped: Value,
) -> Result<ObjectRef, MethodCallFailed> {
    // The view allocation can move both operands; `map_pin` is the unpin base.
    let map_pin = ctx.pin_native_root(map);
    let mapped_pin = pin_value(ctx, mapped);
    let view = alloc_key_set_view_object(ctx)?;
    let view_pin = ctx.pin_native_root(view);
    let map = ctx.read_native_pin(map_pin, map);
    let mapped = read_pinned_elem(ctx, mapped_pin, mapped);
    let view = ctx.read_native_pin(view_pin, view);
    let map_slot = ksv_slot(ctx, view, "map", KSV_FIELD_MAP);
    let value_slot = ksv_slot(ctx, view, "value", KSV_FIELD_VALUE);
    ctx.set_field(view, map_slot, Value::Object(Some(map)));
    ctx.set_field(view, value_slot, mapped);
    ctx.unpin_native_roots(map_pin);
    Ok(view)
}

/// The view's current elements. Weakly consistent, exactly like the JDK's own
/// `KeySetView` iterator: a concurrent writer's effect may or may not be seen.
pub(crate) fn ksv_collect(ctx: &dyn NativeContext, this: ObjectRef) -> Vec<Value> {
    match ksv_map(ctx, this) {
        Some(m) => chm_collect_all_keys(ctx, m),
        None => Vec::new(),
    }
}

/// `ksv_collect` into a fresh `Object[]`, GC-safely.
///
/// The array allocation can move every ref the first collection returned, so
/// count first, allocate, then RE-collect from the live map and store with
/// nothing allocating in between — the `native_hs_iterator` / `native_hs_to_array`
/// idiom. Returns the array and its live element count; a concurrent removal
/// between the two passes trims the array rather than leaving null holes, which
/// is what `CollectionView.toArray`'s own `Arrays.copyOf(r, i)` does.
pub(crate) fn ksv_snapshot_array(ctx: &mut dyn NativeContext, this: ObjectRef) -> (ObjectRef, usize) {
    let this_pin = ctx.pin_native_root(this);
    let len = ksv_collect(ctx, this).len();
    let this = ctx.read_native_pin(this_pin, this);
    let arr = alloc_ref_array(ctx, len);
    let arr_pin = ctx.pin_native_root(arr);
    let this = ctx.read_native_pin(this_pin, this);
    let elems = ksv_collect(ctx, this);
    let arr = ctx.read_native_pin(arr_pin, arr);
    let n = len.min(elems.len());
    for (i, e) in elems.iter().enumerate().take(n) {
        ctx.set_array_element(arr, i, *e);
    }
    ctx.unpin_native_roots(this_pin);
    if n == len {
        return (arr, n);
    }
    let arr_pin = ctx.pin_native_root(arr);
    let trimmed = alloc_ref_array(ctx, n);
    let trimmed_pin = ctx.pin_native_root(trimmed);
    let arr = ctx.read_native_pin(arr_pin, arr);
    let trimmed = ctx.read_native_pin(trimmed_pin, trimmed);
    for i in 0..n {
        let v = ctx.get_array_element(arr, i);
        ctx.set_array_element(trimmed, i, v);
    }
    ctx.unpin_native_roots(arr_pin);
    (trimmed, n)
}

pub(crate) fn native_ksv_get_map(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    Ok(Some(match ksv_map(ctx, this) {
        Some(m) => Value::Object(Some(m)),
        None => Value::Object(None),
    }))
}

pub(crate) fn native_ksv_get_mapped_value(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    Ok(Some(ksv_mapped_value(ctx, this)))}

pub(crate) fn native_ksv_size(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    match ksv_map(ctx, this) {
        Some(m) => native_chm_size(ctx, &[Value::Object(Some(m))]),
        None => Ok(Some(Value::Int(0))),
    }
}

pub(crate) fn native_ksv_is_empty(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let size = match native_ksv_size(ctx, args)? {
        Some(Value::Int(n)) => n,
        _ => 0,
    };
    Ok(Some(Value::Int(i32::from(size == 0))))
}

pub(crate) fn native_ksv_clear(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    match ksv_map(ctx, this) {
        Some(m) => native_chm_clear(ctx, &[Value::Object(Some(m))]),
        None => Ok(None),
    }
}

pub(crate) fn native_ksv_contains(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let elem = args.get(1).copied().unwrap_or(Value::Object(None));
    match ksv_map(ctx, this) {
        // `containsKey` carries the JDK's null-key NPE, which is also
        // `KeySetView.contains(null)`'s contract.
        Some(m) => native_chm_contains_key(ctx, &[Value::Object(Some(m)), elem]),
        None => Ok(Some(Value::Int(0))),
    }
}

pub(crate) fn native_ksv_add(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let elem = args.get(1).copied().unwrap_or(Value::Object(None));
    // JDK: `if ((v = value) == null) throw new UnsupportedOperationException();`
    // — a view from `keySet()` (no mapped value) cannot be added to.
    let mapped = match ksv_mapped_value(ctx, this) {
        Value::Object(None) => return Err(unsupported_op()),
        v => v,
    };
    let map = match ksv_map(ctx, this) {
        Some(m) => m,
        None => return Ok(Some(Value::Int(0))),
    };
    // `putIfAbsent` takes the target segment's monitor, so concurrent adds are
    // safe and the per-segment count stays exact. Its null-key guard is
    // `add(null)`'s NPE.
    let prior = native_chm_put_if_absent(ctx, &[Value::Object(Some(map)), elem, mapped])?;
    Ok(Some(Value::Int(i32::from(matches!(
        prior,
        None | Some(Value::Object(None))
    )))))
}

pub(crate) fn native_ksv_remove(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let elem = args.get(1).copied().unwrap_or(Value::Object(None));
    let map = match ksv_map(ctx, this) {
        Some(m) => m,
        None => return Ok(Some(Value::Int(0))),
    };
    // JDK: `return map.remove(o) != null;` — a CHM never holds a null value, so
    // the returned value doubles as the membership answer.
    let prior = native_chm_remove(ctx, &[Value::Object(Some(map)), elem])?;
    Ok(Some(Value::Int(i32::from(matches!(
        prior,
        Some(Value::Object(Some(_)))
    )))))
}

pub(crate) fn native_ksv_iterator(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let this_pin = ctx.pin_native_root(this);
    let (arr, total) = ksv_snapshot_array(ctx, this);
    let arr_pin = ctx.pin_native_root(arr);
    // Fallible since 2026-08-10; the refusal ARM below was added 2026-08-11,
    // and the reason the arm was missing is worth keeping because it was a
    // correct argument that its own file had already outgrown.
    //
    // The 2026-08-10 note said this site must refuse rather than land on a real
    // `Arrays$ArrayItr`, because HotSpot's `KeySetView.iterator()` returns a
    // `KeyIterator` whose `remove()` writes through to the map, `RChmKeySetView`
    // exercises exactly that, and a fixed-size list's iterator answers
    // `UnsupportedOperationException: remove`. Every clause of that was true
    // when it was written and the last one no longer is: `real_snapshot_iterator`
    // takes a `backing` argument, [`snapshot_itr_backing_table`] carries the
    // pointer the real two-field class has no slot for, and
    // `native_snapshot_itr_remove`'s `SetLike` arm ALREADY discriminates a key-set
    // view (`if is_key_set_view(ctx, backing) { native_ksv_remove(..) }`) — the
    // write-through this site was held back for was built for this receiver and
    // then not wired to it. Measured on the pre-fix binary: `--real-jdk`
    // `RChmKeySetView` died at `surface():140` with
    // `NoClassDefFoundError: java/util/HashMap$KeyItr` on the plain `for (String
    // x : s)`, i.e. the refusal cost the whole iteration, not just `remove()`.
    //
    // What is NOT available here, and why the refusal cannot simply be dropped:
    // real `KeySetView.iterator()` bytecode reads `ConcurrentHashMap.table`, and
    // on a CratonVM-native CHM that field is null — measured reflectively under
    // `--add-opens java.base/java.util.concurrent=ALL-UNNAMED`: `table = null`,
    // `baseCount = 0` for a map with one entry, against `Node[16]`/`1` on
    // HotSpot. Letting the real iterator run would iterate EMPTY, which is the
    // failure mode that reads as a pass. The snapshot keeps the natives the one
    // owner of the state; only the class name becomes honest.
    //
    // `Compatible` is untouched: `try_alloc_synthetic` succeeds there, so this
    // arm never runs and the fabricated `HashMap$KeyItr` is still what
    // `--real-jdk` hands back.
    //
    // `alloc_key_itr`, not the fabricated `HashMap$KeyItr` shape: HotSpot
    // answers `ConcurrentHashMap$KeyIterator` here and this VM answered a
    // fabrication in compatible mode and `Arrays$ArrayItr` under `--real-jdk`.
    // `alloc_key_itr` still falls back to the fabricated shape when the image
    // has no real class, so a stripped image keeps the refusal arm below.
    // gen r4w4/rooting2: `ksv_snapshot_array` allocated, so the receiver whose
    // class picks the carrier is re-read from its pin first.
    let this_now = ctx.read_native_pin(this_pin, this);
    let carrier = key_itr_carrier_for(&*ctx, this_now, false);
    let itr = match alloc_key_itr(ctx, carrier) {
        Ok(itr) => itr,
        Err(_refused) => {
            let arr = ctx.read_native_pin(arr_pin, arr);
            let this = ctx.read_native_pin(this_pin, this);
            let real =
                real_snapshot_iterator(ctx, arr, total, Some((this, SnapshotItrRoute::SetLike)));
            ctx.unpin_native_roots(this_pin);
            return real;
        }
    };
    // `base + FIELD`, not `FIELD`. These five writes were ABSOLUTE, which was
    // right for exactly one carrier: the fabricated `java/util/HashMap$KeyItr`
    // declares no fields of its own, so its `key_itr_base` is 0 and the
    // distinction was invisible. Minting the real
    // `ConcurrentHashMap$KeyIterator` — which declares ten — made it visible at
    // once: every READER in this file (`native_map_key_itr_has_next`, `_next`,
    // `_remove`, `map_itr_comod_source`) offsets by `key_itr_base`, so the
    // snapshot landed ten slots below where they look. MEASURED: the first
    // `next()` on a three-element key set threw `NoSuchElementException`, and
    // `remove()` before `next()` answered `UnsupportedOperationException` where
    // HotSpot says `IllegalStateException` — a cursor reading a total of zero
    // and a backing of null.
    let base = key_itr_base(&*ctx, itr);
    let arr = ctx.read_native_pin(arr_pin, arr);
    let this = ctx.read_native_pin(this_pin, this);
    ctx.set_field(itr, base + MAP_KEY_ITR_FIELD_KEYS, Value::Object(Some(arr)));
    ctx.set_field(itr, base + MAP_KEY_ITR_FIELD_CURSOR, Value::Int(0));
    ctx.set_field(
        itr,
        base + MAP_KEY_ITR_FIELD_TOTAL,
        Value::Int(total as i32),
    );
    // Wire the view itself as the removal target: `CollectionView.retainAll`
    // and `removeAll` are written in terms of `it.remove()`, so an iterator
    // without a backing silently makes both no-ops.
    ctx.set_field(
        itr,
        base + MAP_KEY_ITR_FIELD_BACKING,
        Value::Object(Some(this)),
    );
    ctx.set_field(itr, base + MAP_KEY_ITR_FIELD_LAST_RET, Value::Int(-1));
    // Seed the fail-fast generation LAST, exactly as `native_hs_iterator` does:
    // `MAP_KEY_ITR_FIELD_BACKING` above is how `map_itr_comod_source` finds the
    // collection to watch. A `ConcurrentHashMap` iterator is weakly consistent
    // and must never throw `ConcurrentModificationException`, and it does not:
    // the comodification check reads a generation the CHM natives do not bump.
    map_itr_seed_expected(ctx, itr);
    ctx.unpin_native_roots(this_pin);
    Ok(Some(Value::Object(Some(itr))))
}

pub(crate) fn native_ksv_to_array(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let (arr, _) = ksv_snapshot_array(ctx, this);
    Ok(Some(Value::Object(Some(arr))))
}

/// `toArray(T[])` — fill the caller's array when it is large enough (writing
/// the spec's null terminator), otherwise allocate a fresh array of the
/// template's runtime component type so `Set<String>.toArray(new String[0])`
/// stays a `String[]`.
pub(crate) fn native_ksv_to_array_typed(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    reject_null_to_array_template(args.get(1))?;
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let target = match args.get(1).copied() {
        Some(Value::Object(Some(t))) => Some(t),
        _ => None,
    };
    let target_pin = match target {
        Some(t) => ctx.pin_native_root(t),
        None => usize::MAX,
    };
    let this_pin = ctx.pin_native_root(this);
    let (src, n) = ksv_snapshot_array(ctx, this);
    let src_pin = ctx.pin_native_root(src);
    let target = target.map(|t| ctx.read_native_pin(target_pin, t));
    if let Some(t) = target {
        if ctx.array_length(t) >= n {
            let len = ctx.array_length(t);
            let src = ctx.read_native_pin(src_pin, src);
            for i in 0..n {
                let v = ctx.get_array_element(src, i);
                ctx.set_array_element(t, i, v);
            }
            if len > n {
                ctx.set_array_element(t, n, Value::Object(None));
            }
            ctx.unpin_native_roots(if target_pin == usize::MAX {
                this_pin
            } else {
                target_pin
            });
            return Ok(Some(Value::Object(Some(t))));
        }
    }
    let out = match target {
        Some(t) => {
            let comp = ctx.class_id_of_object(t);
            ctx.new_ref_array(comp, n)
        }
        None => alloc_ref_array(ctx, n),
    };
    let out_pin = ctx.pin_native_root(out);
    let src = ctx.read_native_pin(src_pin, src);
    let out = ctx.read_native_pin(out_pin, out);
    for i in 0..n {
        let v = ctx.get_array_element(src, i);
        ctx.set_array_element(out, i, v);
    }
    ctx.unpin_native_roots(if target_pin == usize::MAX {
        this_pin
    } else {
        target_pin
    });
    Ok(Some(Value::Object(Some(out))))
}

pub(crate) fn native_ksv_to_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let elems = ksv_collect(ctx, this);
    // `obj_to_display_units` dispatches `toString()`, which allocates — pin
    // every element and re-read it at the point of use.
    let (elems_pin, handles) = pin_value_slice(ctx, &elems);
    let mut parts = Vec::with_capacity(elems.len());
    for i in 0..elems.len() {
        let e = read_pinned_elem(ctx, handles[i], elems[i]);
        parts.push(obj_to_display_units(ctx, &e)?);
    }
    if elems_pin != usize::MAX {
        ctx.unpin_native_roots(elems_pin);
    }
    let text = wrap_join("[", &parts, ", ", "]");
    let s = ctx.create_string_from_units(&text);
    Ok(Some(Value::Object(Some(s))))
}

pub(crate) fn native_ksv_hash_code(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let elems = ksv_collect(ctx, this);
    let (elems_pin, handles) = pin_value_slice(ctx, &elems);
    let mut h: i32 = 0;
    for i in 0..elems.len() {
        let e = read_pinned_elem(ctx, handles[i], elems[i]);
        // `Set.hashCode` contract: the sum of the elements' hash codes.
        match element_hash_code(ctx, &e) {
            Ok(eh) => h = h.wrapping_add(eh),
            Err(err) => {
                if elems_pin != usize::MAX {
                    ctx.unpin_native_roots(elems_pin);
                }
                return Err(err);
            }
        }
    }
    if elems_pin != usize::MAX {
        ctx.unpin_native_roots(elems_pin);
    }
    Ok(Some(Value::Int(h)))
}

pub(crate) fn native_ksv_equals(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let other = match args.get(1).copied() {
        Some(Value::Object(Some(o))) => o,
        _ => return Ok(Some(Value::Int(0))),
    };
    if std::ptr::eq(this.as_ptr(), other.as_ptr()) {
        return Ok(Some(Value::Int(1)));
    }
    // `AbstractSet.equals`: a Set equals only another Set. The guard must
    // precede the `size()` dispatch, or `set.equals("text")` raises
    // NoSuchMethodError instead of answering false.
    if !obj_is_instance_of(ctx, other, "java/util/Set") {
        return Ok(Some(Value::Int(0)));
    }
    let this_pin = ctx.pin_native_root(this);
    let other_pin = ctx.pin_native_root(other);
    let result = (|| -> MethodCallResult {
        let other = ctx.read_native_pin(other_pin, other);
        let other_size = match ctx.invoke_virtual(other, "size", "()I", &[])? {
            Some(Value::Int(n)) => n,
            _ => return Ok(Some(Value::Int(0))),
        };
        let this = ctx.read_native_pin(this_pin, this);
        let elems = ksv_collect(ctx, this);
        if elems.len() as i32 != other_size {
            return Ok(Some(Value::Int(0)));
        }
        let (_, handles) = pin_value_slice(ctx, &elems);
        for i in 0..elems.len() {
            let other = ctx.read_native_pin(other_pin, other);
            let e = read_pinned_elem(ctx, handles[i], elems[i]);
            let contains = ctx.invoke_virtual(other, "contains", "(Ljava/lang/Object;)Z", &[e])?;
            if !matches!(contains, Some(Value::Int(1))) {
                return Ok(Some(Value::Int(0)));
            }
        }
        Ok(Some(Value::Int(1)))
    })();
    ctx.unpin_native_roots(this_pin);
    result
}

pub(crate) fn native_ksv_for_each(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    // RULE F. MEASURED on `ConcurrentHashMap.keySet()`, empty or not. Also
    // reached via `ksv_route` from `native_al_for_each` / `native_hs_for_each`,
    // which already check — the duplicate is idempotent and keeps this body
    // correct when it is entered directly by its own registration.
    reject_null_functional(args.get(1))?;
    let action = match args.get(1) {
        Some(Value::Object(Some(a))) => *a,
        _ => return Ok(None),
    };
    let elems = ksv_collect(ctx, this);
    // `accept` is arbitrary Java: it allocates, so `action` and every element
    // must be pinned and re-read per dispatch.
    let action_pin = ctx.pin_native_root(action);
    let (_, handles) = pin_value_slice(ctx, &elems);
    let mut result = Ok(None);
    for i in 0..elems.len() {
        let a = ctx.read_native_pin(action_pin, action);
        let e = read_pinned_elem(ctx, handles[i], elems[i]);
        if let Err(err) = ctx.invoke_virtual(a, "accept", "(Ljava/lang/Object;)V", &[e]) {
            result = Err(err);
            break;
        }
    }
    ctx.unpin_native_roots(action_pin);
    result
}

pub(crate) fn native_ksv_stream(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return make_stream(ctx, &[]),
    };
    let (arr, _) = ksv_snapshot_array(ctx, this);
    let arr_pin = ctx.pin_native_root(arr);
    let stream = try_alloc_synthetic(ctx, "java/util/stream/Stream", STREAM_NUM_FIELDS)?;
    let arr = ctx.read_native_pin(arr_pin, arr);
    ctx.set_field(stream, STREAM_FIELD_ELEMENTS, Value::Object(Some(arr)));
    ctx.set_field(stream, STREAM_FIELD_CLOSE_HANDLERS, Value::Object(None));
    ctx.unpin_native_roots(arr_pin);
    Ok(Some(Value::Object(Some(stream))))
}

/// `ConcurrentHashMap.keySet().spliterator()`.
///
/// FOUR fields, not three: the fourth carries this spliterator's own
/// CHARACTERISTICS, which [`native_spliterator_characteristics`] reads when it
/// is present. Every synthetic spliterator in the VM shared one constant
/// `SIZED | SUBSIZED | ORDERED`, and for this receiver all three of those bits
/// are wrong while the two that matter are missing. MEASURED against HotSpot
/// 25.0.4+7, both modes:
///
/// ```text
///   chm.keySet().spliterator().characteristics() & Spliterator.CONCURRENT
///     HotSpot  non-zero      CratonVM  0
/// ```
///
/// `CONCURRENT` is the load-bearing one and it is not cosmetic: it is how a
/// stream learns that its source can change under it, so a stream pipeline told
/// `SIZED | SUBSIZED` instead is entitled to pre-size its result array to a
/// count that may already be stale. `DISTINCT` and `NONNULL` come with it
/// because `ConcurrentHashMap$KeySpliterator` reports exactly those three, and
/// `ORDERED` is dropped because a hash container has no encounter order to
/// promise.
pub(crate) fn native_ksv_spliterator(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let (arr, n) = ksv_snapshot_array(ctx, this);
    let arr_pin = ctx.pin_native_root(arr);
    let spl = try_alloc_synthetic(ctx, "java/util/Spliterator", 4)?;
    let arr = ctx.read_native_pin(arr_pin, arr);
    ctx.set_field(spl, 0, Value::Object(Some(arr)));
    ctx.set_field(spl, 1, Value::Int(0));
    ctx.set_field(spl, 2, Value::Int(n as i32));
    ctx.set_field(spl, 3, Value::Int(SPLITERATOR_CHM_KEYSET_CHARACTERISTICS));
    ctx.unpin_native_roots(arr_pin);
    Ok(Some(Value::Object(Some(spl))))
}

/// `DISTINCT | NONNULL | CONCURRENT` — what
/// `ConcurrentHashMap$KeySpliterator.characteristics()` returns.
pub(crate) const SPLITERATOR_CHM_KEYSET_CHARACTERISTICS: i32 = 0x1 | 0x100 | 0x1000;

pub(crate) fn native_ksv_contains_all(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // C4W4-6: `CollectionView.containsAll`'s `for (Object e : c)` on null.
    reject_null_contains_all_arg(args.get(1))?;
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let coll = match args.get(1) {
        Some(Value::Object(Some(c))) => *c,
        _ => return Ok(Some(Value::Int(0))),
    };
    let this_pin = ctx.pin_native_root(this);
    // JIT round 16 (lane compatdrain), r15w4-compat5 item 6: see
    // [`contains_all_by_iterator`].
    if bulk_add_jdk_questions_enabled() && copy_source_is_foreign(&*ctx, coll) {
        let walked = contains_all_by_iterator(ctx, (this_pin, this), coll, native_ksv_contains);
        ctx.unpin_native_roots(this_pin);
        return walked.map(|all| Some(Value::Int(i32::from(all))));
    }
    // A throwing walk releases `this_pin` (round 15 wave 4, lane compat5).
    let collected = collect_collection_elements_or_real(ctx, coll);
    let elems = unpin_on_err(ctx, this_pin, collected)?;
    let (_, handles) = pin_value_slice(ctx, &elems);
    let mut answer = 1;
    let mut failure = None;
    for i in 0..elems.len() {
        let this = ctx.read_native_pin(this_pin, this);
        let e = read_pinned_elem(ctx, handles[i], elems[i]);
        match native_ksv_contains(ctx, &[Value::Object(Some(this)), e]) {
            Ok(r) => {
                if !matches!(r, Some(Value::Int(1))) {
                    answer = 0;
                    break;
                }
            }
            Err(err) => {
                failure = Some(err);
                break;
            }
        }
    }
    ctx.unpin_native_roots(this_pin);
    match failure {
        Some(err) => Err(err),
        None => Ok(Some(Value::Int(answer))),
    }
}

pub(crate) fn native_ksv_add_all(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let coll = match args.get(1) {
        Some(Value::Object(Some(c))) => *c,
        _ => return Ok(Some(Value::Int(0))),
    };
    // JDK: `addAll` throws before looking at the collection, so an EMPTY
    // collection throws too. Deferring to the per-element `add` below would
    // quietly return `false` for that case.
    if matches!(ksv_mapped_value(ctx, this), Value::Object(None)) {
        return Err(unsupported_op());
    }
    let this_pin = ctx.pin_native_root(this);
    let elems = collect_collection_elements_or_real(ctx, coll)?;
    let (_, handles) = pin_value_slice(ctx, &elems);
    let mut modified = false;
    let mut failure = None;
    for i in 0..elems.len() {
        let this = ctx.read_native_pin(this_pin, this);
        let e = read_pinned_elem(ctx, handles[i], elems[i]);
        match native_ksv_add(ctx, &[Value::Object(Some(this)), e]) {
            Ok(Some(Value::Int(1))) => modified = true,
            Ok(_) => {}
            Err(err) => {
                failure = Some(err);
                break;
            }
        }
    }
    ctx.unpin_native_roots(this_pin);
    match failure {
        Some(err) => Err(err),
        None => Ok(Some(Value::Int(i32::from(modified)))),
    }
}

pub(crate) fn native_ksv_remove_all(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let coll = match args.get(1) {
        Some(Value::Object(Some(c))) => *c,
        _ => return Ok(Some(Value::Int(0))),
    };
    let this_pin = ctx.pin_native_root(this);
    let elems = collect_collection_elements_or_real(ctx, coll)?;
    let (_, handles) = pin_value_slice(ctx, &elems);
    let mut modified = false;
    let mut failure = None;
    for i in 0..elems.len() {
        let this = ctx.read_native_pin(this_pin, this);
        let e = read_pinned_elem(ctx, handles[i], elems[i]);
        match native_ksv_remove(ctx, &[Value::Object(Some(this)), e]) {
            Ok(Some(Value::Int(1))) => modified = true,
            Ok(_) => {}
            Err(err) => {
                failure = Some(err);
                break;
            }
        }
    }
    ctx.unpin_native_roots(this_pin);
    match failure {
        Some(err) => Err(err),
        None => Ok(Some(Value::Int(i32::from(modified)))),
    }
}

pub(crate) fn native_ksv_retain_all(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let coll = match args.get(1) {
        Some(Value::Object(Some(c))) => *c,
        _ => return Ok(Some(Value::Int(0))),
    };
    let this_pin = ctx.pin_native_root(this);
    let keep = collect_collection_elements_or_real(ctx, coll)?;
    let this_now = ctx.read_native_pin(this_pin, this);
    let current = ksv_collect(ctx, this_now);
    let (_, keep_handles) = pin_value_slice(ctx, &keep);
    let (_, cur_handles) = pin_value_slice(ctx, &current);
    let mut modified = false;
    let mut failure = None;
    for i in 0..current.len() {
        let mut e = read_pinned_elem(ctx, cur_handles[i], current[i]);
        let mut should_keep = false;
        for j in 0..keep.len() {
            let k = read_pinned_elem(ctx, keep_handles[j], keep[j]);
            let matched = list_element_matches(ctx, &k, &e)?;
            e = read_pinned_elem(ctx, cur_handles[i], e);
            if matched {
                should_keep = true;
                break;
            }
        }
        if should_keep {
            continue;
        }
        let this = ctx.read_native_pin(this_pin, this);
        match native_ksv_remove(ctx, &[Value::Object(Some(this)), e]) {
            Ok(Some(Value::Int(1))) => modified = true,
            Ok(_) => {}
            Err(err) => {
                failure = Some(err);
                break;
            }
        }
    }
    ctx.unpin_native_roots(this_pin);
    match failure {
        Some(err) => Err(err),
        None => Ok(Some(Value::Int(i32::from(modified)))),
    }
}

pub(crate) fn native_ksv_remove_if(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    // RULE F, empty receiver included. See `reject_null_functional`.
    reject_null_functional(args.get(1))?;
    let pred = match args.get(1) {
        Some(Value::Object(Some(p))) => *p,
        _ => return Ok(Some(Value::Int(0))),
    };
    let this_pin = ctx.pin_native_root(this);
    let pred_pin = ctx.pin_native_root(pred);
    let this_now = ctx.read_native_pin(this_pin, this);
    let elems = ksv_collect(ctx, this_now);
    let (_, handles) = pin_value_slice(ctx, &elems);
    let mut modified = false;
    let mut failure = None;
    for i in 0..elems.len() {
        let p = ctx.read_native_pin(pred_pin, pred);
        let e = read_pinned_elem(ctx, handles[i], elems[i]);
        let verdict = match ctx.invoke_virtual(p, "test", "(Ljava/lang/Object;)Z", &[e]) {
            Ok(v) => matches!(v, Some(Value::Int(1))),
            Err(err) => {
                failure = Some(err);
                break;
            }
        };
        if !verdict {
            continue;
        }
        let this = ctx.read_native_pin(this_pin, this);
        let e = read_pinned_elem(ctx, handles[i], elems[i]);
        match native_ksv_remove(ctx, &[Value::Object(Some(this)), e]) {
            Ok(Some(Value::Int(1))) => modified = true,
            Ok(_) => {}
            Err(err) => {
                failure = Some(err);
                break;
            }
        }
    }
    ctx.unpin_native_roots(this_pin);
    match failure {
        Some(err) => Err(err),
        None => Ok(Some(Value::Int(i32::from(modified)))),
    }
}

/// Public builder for a live, add-able, concurrency-safe `Set` view over an
/// existing native-backed `ConcurrentHashMap`, mapped value `Boolean.TRUE`.
///
/// `Collections.newSetFromMap(new ConcurrentHashMap<>())` — the other canonical
/// spelling of "give me a concurrent set" — used to answer with a plain
/// `HashSet` for exactly the same reason `newKeySet()` did, and with exactly
/// the same consequence: `add`/`remove`/`size` on the result ran the unlocked
/// `HashMap` natives. A `newSetFromMap` over an empty map is definitionally the
/// same object as `map.keySet(Boolean.TRUE)`, so it is built here rather than
/// duplicated. See `native-builtins`' `newSetFromMap` registration.
pub fn make_concurrent_key_set_view(
    ctx: &mut dyn NativeContext,
    map: ObjectRef,
) -> Result<ObjectRef, MethodCallFailed> {
    let (map, mapped) = rooted_across1(ctx, map, ksv_boxed_true);
    Ok(make_key_set_view(ctx, map, mapped)?)
}

pub(crate) fn register_chm_key_set_view_natives(r: &mut NativeMethodRegistry) {
            let c = KSV_CLASS;
    r.register(c, "size", "()I", native_ksv_size, cratonvm_native_api::NativeKind::Bridge);
    r.register(c, "isEmpty", "()Z", native_ksv_is_empty, cratonvm_native_api::NativeKind::Bridge);
    r.register(c, "clear", "()V", native_ksv_clear, cratonvm_native_api::NativeKind::Bridge);
    r.register(c, "contains", "(Ljava/lang/Object;)Z", native_ksv_contains, cratonvm_native_api::NativeKind::Bridge);
    r.register(c, "add", "(Ljava/lang/Object;)Z", native_ksv_add, cratonvm_native_api::NativeKind::Bridge);
    r.register(c, "remove", "(Ljava/lang/Object;)Z", native_ksv_remove, cratonvm_native_api::NativeKind::Bridge);
    r.register(c, "iterator", "()Ljava/util/Iterator;", native_ksv_iterator, cratonvm_native_api::NativeKind::Bridge);
    r.register(c, "toArray", "()[Ljava/lang/Object;", native_ksv_to_array, cratonvm_native_api::NativeKind::Bridge);
    r.register(
        c,
        "toArray",
        "([Ljava/lang/Object;)[Ljava/lang/Object;",
        native_ksv_to_array_typed, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "toArray",
        "(Ljava/util/function/IntFunction;)[Ljava/lang/Object;",
        native_collection_to_array_generator, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(c, "toString", "()Ljava/lang/String;", native_ksv_to_string, cratonvm_native_api::NativeKind::Bridge);
    r.register(c, "hashCode", "()I", native_ksv_hash_code, cratonvm_native_api::NativeKind::Bridge);
    r.register(c, "equals", "(Ljava/lang/Object;)Z", native_ksv_equals, cratonvm_native_api::NativeKind::Bridge);
    r.register(
        c,
        "forEach",
        "(Ljava/util/function/Consumer;)V",
        native_ksv_for_each, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "stream",
        "()Ljava/util/stream/Stream;",
        native_ksv_stream, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "spliterator",
        "()Ljava/util/Spliterator;",
        native_ksv_spliterator, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "containsAll",
        "(Ljava/util/Collection;)Z",
        native_ksv_contains_all, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(c, "addAll", "(Ljava/util/Collection;)Z", native_ksv_add_all, cratonvm_native_api::NativeKind::Bridge);
    r.register(
        c,
        "removeAll",
        "(Ljava/util/Collection;)Z",
        native_ksv_remove_all, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "retainAll",
        "(Ljava/util/Collection;)Z",
        native_ksv_retain_all, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "removeIf",
        "(Ljava/util/function/Predicate;)Z",
        native_ksv_remove_if, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "getMappedValue",
        "()Ljava/lang/Object;",
        native_ksv_get_mapped_value, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "getMap",
        "()Ljava/util/concurrent/ConcurrentHashMap;",
        native_ksv_get_map, cratonvm_native_api::NativeKind::Bridge,
    );
    }

/// A real `java.util.Enumeration` over `array`, or `None` when this image
/// cannot build one.
///
/// The `Enumeration` form of `real_snapshot_iterator` (spelled without an
/// intra-doc link because that helper is private and this item is not), and the
/// same rule in its strongest form: prefer a class the JDK BUILDS ITSELF over one whose
/// fields we fill. The snapshot is already an `Object[]`,
/// `java.util.Arrays$ArrayList` is the JDK's own fixed-size list over exactly
/// that shape, and `Collections.enumeration(Collection)` turns one into a real
/// `Enumeration` — real bytecode the whole way, so nothing here has to know
/// what the resulting anonymous class is CALLED (it is `java.util.Collections$3`
/// on JDK 25, and an anonymous class's number must not be written down).
///
/// Deliberately a duplicate of `native-builtins`' `classloader::
/// real_snapshot_enumeration`, not a call to it: `native-builtins` depends on
/// this crate, so the dependency cannot run the other way. This is the copy the
/// crate that owns `cratonvm/internal/SnapshotEnumeration` can reach; it is
/// `pub` so the `native-builtins` copies can converge on it later.
///
/// `hasMoreElements`/`nextElement` on the result are real JDK bytecode: neither
/// name is in `vm_exec`'s `check_override` chain, and the anonymous class
/// declares both concretely, so the interface-level
/// `java/util/Enumeration.hasMoreElements` native registered by
/// `register_collections_natives` never intercepts it (that native is reachable
/// only through the C25 *abstract*-method arm).
pub fn real_snapshot_enumeration(
    ctx: &mut dyn NativeContext,
    array: ObjectRef,
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    let list = match ctx.new_object_initialized(
        "java/util/Arrays$ArrayList",
        "([Ljava/lang/Object;)V",
        &[Value::Object(Some(array))],
    )? {
        Some(Value::Object(Some(list))) => list,
        _ => return Ok(None),
    };
    match ctx.invoke(
        "java/util/Collections",
        "enumeration",
        "(Ljava/util/Collection;)Ljava/util/Enumeration;",
        &[Value::Object(Some(list))],
    )? {
        Some(Value::Object(Some(enm))) => Ok(Some(enm)),
        _ => Ok(None),
    }
}

/// The snapshot enumeration over `elems`: the fabricated
/// `cratonvm/internal/SnapshotEnumeration` in `Compatible` mode, and a real
/// `java.util.Enumeration` when `--real-jdk` refuses that fabrication.
///
/// WHY A FALLBACK AND NOT A DROPPED SHADOW. The two callers that reach strict
/// mode are `ConcurrentHashMap.keys()`/`elements()`, and a natively-backed CHM
/// keeps its entries in a SEGMENTED layout that never populates `table` — the
/// same reason `native_chm_reduce_values` and `native_chm_search_keys` are
/// registered right beside them. Real `ConcurrentHashMap.keys()` bytecode
/// builds a `KeyIterator` over `table`, so deleting these natives would answer
/// an EMPTY enumeration over a populated map, which is strictly worse than the
/// refusal it replaced. Only the CARRIER was fabricated, so only the carrier is
/// replaced.
///
/// `Compatible` mode is byte-for-byte what it was: the fallback is reached only
/// from the refusal arm, which only strict mode takes.
pub(crate) fn make_snapshot_enumeration(
    ctx: &mut dyn NativeContext,
    elems: &[Value],
) -> Result<ObjectRef, MethodCallFailed> {
    let (elem_base, elem_handles) = pin_value_slice(ctx, elems);
    let arr = alloc_ref_array(ctx, elems.len());
    let arr_pin = ctx.pin_native_root(arr);
    for (i, val) in elems.iter().enumerate() {
        let arr = ctx.read_native_pin(arr_pin, arr);
        let val = read_pinned_elem(ctx, elem_handles[i], *val);
        ctx.set_array_element(arr, i, val);
    }
    // Unwind to this base on EVERY path. The old `?` on the allocation leaked
    // both pin sets on the refusal it is now the whole point of handling.
    let unpin_base = if elem_base == usize::MAX {
        arr_pin
    } else {
        elem_base
    };
    let out = match try_alloc_synthetic(ctx, "cratonvm/internal/SnapshotEnumeration", 2) {
        Ok(en) => {
            let en_pin = ctx.pin_native_root(en);
            let arr = ctx.read_native_pin(arr_pin, arr);
            let en = ctx.read_native_pin(en_pin, en);
            ctx.set_field(en, 0, Value::Object(Some(arr)));
            ctx.set_field(en, 1, Value::Int(0));
            Ok(en)
        }
        Err(refusal) => {
            let arr = ctx.read_native_pin(arr_pin, arr);
            match real_snapshot_enumeration(ctx, arr) {
                Ok(Some(en)) => Ok(en),
                // Nothing real to stand in — an image with no
                // `Arrays$ArrayList` — so the refusal stands rather than
                // silently becoming a fabrication again.
                Ok(None) => Err(refusal),
                Err(err) => Err(err),
            }
        }
    };
    ctx.unpin_native_roots(unpin_base);
    out
}

/// The real JDK bin-node class a `ConcurrentHashMap.table` is an array of.
pub(crate) const CHM_NODE_CLASS: &str = "java/util/concurrent/ConcurrentHashMap$Node";
/// The JDK's own dual `Enumeration`/`Iterator` carriers.
pub(crate) const CHM_KEY_ITER_CLASS: &str = "java/util/concurrent/ConcurrentHashMap$KeyIterator";
pub(crate) const CHM_VALUE_ITER_CLASS: &str = "java/util/concurrent/ConcurrentHashMap$ValueIterator";

/// Resolve a REAL image class, never a fabrication.
///
/// `ensure_class_initialized` fabricates a stand-in rather than failing, so an
/// `Ok` from it is not evidence the image has the class (the recurring
/// `ensure-class-initialized-fabricates-instead-of-failing` shape). Both the
/// name the id resolves BACK to and the synthetic-stub predicate are checked,
/// and either mismatch is reported as "not available" so the caller falls back
/// rather than building a carrier out of a stub.
pub(crate) fn chm_real_class(ctx: &mut dyn NativeContext, name: &str) -> Option<ClassId> {
    if ctx.is_class_synthetic_stub(name) {
        return None;
    }
    if let Some(cid) = ctx.class_id_by_name(name) {
        return Some(cid);
    }
    let cid = ctx.ensure_class_initialized(name).ok()?;
    if ctx.class_name_of_id(cid).as_deref() == Some(name) {
        Some(cid)
    } else {
        None
    }
}

/// Materialise the segmented store into a REAL `ConcurrentHashMap$Node[]` and
/// publish it in the receiver's real `table` field, returning the array.
///
/// # Why this exists
///
/// `Hashtable`'s precedent is the whole argument: its bucket array lives in the
/// receiver's own `table` field, and java.base's own `Hashtable$Enumerator`
/// walks it correctly even though the NODES are ours. CHM was the opposite —
/// measured on this binary, `--real-jdk`, JDK 25.0.3+9, for an empty map, a
/// 6-entry map and a 200-entry map alike:
///
/// ```text
///                        HotSpot 25                    CratonVM --real-jdk
///   table                len=16 used=6 $Node           null
///   baseCount / sizeCtl  6 / 12                        0 / 0
///   slot 0 (AbstractMap.keySet)  null                  Object[4] of segments
///   keys()               $KeyIterator (dual)           Collections$3 (NOT an Iterator)
/// ```
///
/// so `keys()`/`elements()` could not hand back the JDK's dual
/// `Enumeration`+`Iterator` carrier: there was nothing for a `KeyIterator` to
/// walk. That is `RJdkEnumerations`' `carriersAreTheJdksOwnDualInterface\
/// Enumerators` vector.
///
/// The feasibility question was settled by measurement BEFORE this was written
/// (`probes/ChmDrive.java`): a `Node[]` built by hand at the Java level, handed
/// to java.base's own `KeyIterator`/`ValueIterator`, iterates CORRECTLY on
/// CratonVM `--real-jdk` — both faces, one shared cursor, the right multiset.
/// `Traverser.advance()`, `tabAt()`'s `U.getReferenceAcquire` and its checkcast
/// to `$Node` all run. So the shape is walkable; only the field was missing.
///
/// # What this is and is NOT
///
/// It is a **refresh-on-read mirror**, not a move of the authority. The
/// segmented store rooted at slot 0 remains the one writable copy; every
/// mutator (`put`/`remove`/`compute*`/…) is unchanged and never runs this. The
/// mirror is rebuilt from scratch on each `keys()`/`elements()`, so `table` is
/// exactly as fresh as the most recent iteration and can never drift silently
/// behind the store the way a build-once cache would.
///
/// **Concurrency.** This adds no new sharing and takes no segment lock. It
/// reads the segments exactly as `chm_collect_all_keys` already does, so it
/// inherits that walk's existing weak consistency, and it publishes a private,
/// freshly-allocated array that no other thread can reach until the store to
/// `table` makes it visible. A concurrent mutation during or after the walk is
/// not reflected in an iterator already handed out — which is precisely what
/// `ConcurrentHashMap`'s spec permits of its weakly-consistent iterators
/// ("may or may not reflect" post-construction modifications). Two threads
/// calling `keys()` at once each build their own array; the last store to
/// `table` wins, and both iterators still hold their own. What this does NOT
/// give is atomicity of the snapshot against a concurrent writer — no more and
/// no less than the snapshot enumeration it replaces.
///
/// **It does not unblock retiring the CHM shadows.** Retirement needs `table`
/// to be the AUTHORITY that real `putVal`/`get` bytecode CASes into, which a
/// mirror is not. See `W7-96-chm-table-never-populated.md`.
///
/// # Why the fallback is total
///
/// Any surprise — no real `$Node` in the image, an over-large table, a
/// primitive in a slot a real `$Node` declares as a reference (which
/// descriptor coercion would silently turn into a DIFFERENT value on store) —
/// returns `None` and the caller keeps today's snapshot carrier verbatim. A
/// partially-materialised `table` would be a populated-looking lie, which is
/// the one outcome worse than the null it replaces.
//
// ---------------------------------------------------------------------------

/// Refresh [`chm_publish_real_table`]'s mirror for its side effect, discarding
/// the array.
///
/// # Why the O(N) read natives, and not `put`
///
/// `H23-2` §7.3 nominated *"`ConcurrentHashMap.table` is null for an ordinary
/// map"*, and MEASURED on this host it was: a two-entry `new
/// ConcurrentHashMap<>()` reported `table = null` where HotSpot 25.0.3+9
/// reports `[Ljava.util.concurrent.ConcurrentHashMap$Node; len=16`, and a
/// 40-entry one reported null against HotSpot's `len=64`. The mirror was
/// correct wherever it ran; it ran from exactly ONE caller,
/// `chm_real_dual_iterator`, i.e. only from `keys()`/`elements()`.
///
/// The mirror is a rebuild-from-scratch of the whole segmented store, so it is
/// O(N). Hanging it on `put` would make every insert O(N) and hanging it on
/// `size()` would make an O(1) query O(N) — both are worse than the defect.
/// Every caller added here is a bulk operation that ALREADY walks every entry
/// (`chm_collect_all_keys` / `_values` / `_entries` do the same walk the mirror
/// does), so the cost is a constant factor on an already-linear operation and
/// the asymptotics of no operation change.
///
/// What that buys, and what it does not: after any bulk read of a CHM its
/// `table` is a fresh, correctly-typed `$Node[]` that java.base's own
/// `Traverser` walks. A map that has been WRITTEN and never bulk-read still
/// presents `table = null` — the residual, stated rather than hidden, because
/// closing it needs `table` to be the authority a real `putVal` CASes into
/// rather than a mirror (`W7-96-chm-table-never-populated.md`), which is a
/// different change from this one.
///
/// The total-fallback contract is inherited unchanged: any surprise leaves
/// `table` exactly as it was, so a failed refresh can never be worse than not
/// refreshing.
/// # `this` is `&mut` — `WORKER-5-NOTE-12` N3 (the depth-2 pass)
///
/// `chm_publish_real_table` pins `this` for its whole body, but the pin is
/// released before it returns and the CALLER's copy is never updated — and the
/// body allocates (class resolution can LOAD a class, as its own comment says).
/// All three call sites then dereference the receiver immediately
/// (`make_key_set_view`, `chm_collect_all_values`, `chm_collect_all_entries`).
///
/// Found only at `--depth 2`: the allocation is two helper hops down, so the
/// depth-1 sweep that produced NOTE-12's baseline could not see it. That is
/// exactly the limit NOTE-12 §5 flagged, and it is why the baseline moved to
/// depth 2.
pub(crate) fn chm_refresh_real_table(ctx: &mut dyn NativeContext, this: &mut ObjectRef) {
    let pin = ctx.pin_native_root(*this);
    let _ = chm_publish_real_table(ctx, *this);
    *this = ctx.read_native_pin(pin, *this);
    ctx.unpin_native_roots(pin);
}

pub(crate) fn chm_publish_real_table(ctx: &mut dyn NativeContext, this: ObjectRef) -> Option<ObjectRef> {
    // `this` is pinned for the WHOLE body and every early exit unwinds through
    // the single `unpin_native_roots` below, so the many `return None` arms
    // inside cannot leak a pin. The array and key/value pins taken later are
    // all above this handle and are released by the same call.
    let this_pin = ctx.pin_native_root(this);
    let out = chm_publish_real_table_pinned(ctx, this, this_pin);
    ctx.unpin_native_roots(this_pin);
    out
}

/// [`chm_publish_real_table`]'s body, with `this` already pinned as `this_pin`
/// and the unpin left to the caller.
pub(crate) fn chm_publish_real_table_pinned(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    this_pin: usize,
) -> Option<ObjectRef> {
    // Class resolution can LOAD a class, which allocates, so this runs before
    // anything raw is read off `this` and `this` is re-read straight after.
    let node_cid = chm_real_class(ctx, CHM_NODE_CLASS)?;
    let this = ctx.read_native_pin(this_pin, this);
    let node_width = ctx.class_num_total_fields(node_cid).max(NODE_NUM_FIELDS);
    let table_slot = receiver_table_slot(ctx, this)?;

    // Snapshot the segmented store. `get_field`/`get_array_element`/
    // `array_length` never allocate, so there is no GC point in this walk and
    // the raw refs it collects are still live when it ends. Allocation starts
    // below, behind pins.
    let mut entries: Vec<(i32, Value, Value)> = Vec::new();
    for seg in chm_all_segments(ctx, this) {
        let (buckets, _size, cap) = map_state(ctx, seg);
        if let Some(b) = buckets {
            for i in 0..(cap as usize) {
                let mut node_val = ctx.get_array_element(b, i);
                while let Value::Object(Some(node)) = node_val {
                    // The JDK's `spread()` masks with `HASH_BITS`, and
                    // `Traverser.advance()` reads `e.hash < 0` as "this bin
                    // head is a Forwarding/Tree/Reservation node, not a
                    // mapping". A negative hash here would make java.base
                    // treat our plain node as a control node and walk off it.
                    let hash = match ctx.get_field(node, NODE_FIELD_HASH) {
                        Value::Int(h) => h & 0x7fff_ffff,
                        _ => return None,
                    };
                    let key = get_node_key(ctx, node);
                    let value = get_node_value(ctx, node);
                    // A value that IS the segment object is an in-flight
                    // `computeIfAbsent` reservation, not a mapping — invisible
                    // to iteration, exactly as the JDK's ReservationNode is.
                    // See `chm_collect_all_entries`.
                    let reserved = matches!(value, Value::Object(Some(v))
                        if std::ptr::eq(v.as_ptr(), seg.as_ptr()));
                    if !reserved {
                        // A real `$Node` declares `key` and `val` as
                        // references. A primitive stored into one would be
                        // descriptor-coerced on the way in and read back as a
                        // different value, so abandon rather than publish it.
                        if !matches!(key, Value::Object(Some(_)))
                            || !matches!(value, Value::Object(Some(_)))
                        {
                            return None;
                        }
                        entries.push((hash, key, value));
                    }
                    node_val = ctx.get_field(node, NODE_FIELD_NEXT);
                }
            }
        }
    }

    // HotSpot's own sizing, so the published table matches it bin-for-bin:
    // start at DEFAULT_CAPACITY and double while the count exceeds the 0.75
    // load factor. Measured against HotSpot: 6 entries -> 16, 200 -> 512.
    //
    // ...but DEFAULT_CAPACITY is only the floor for a map built by the no-arg
    // constructor. `new ConcurrentHashMap<>(16)` ends with
    // `sizeCtl = tableSizeFor(16 + (16 >>> 1) + 1) = 32`, and HotSpot's first
    // `put` allocates 32 buckets and keeps them — a 3-entry map built that way
    // has `table.length == 32`, not 16. [`chm_initial_table`] is where this
    // implementation already records that, so take it as the floor.
    //
    // TWO PRODUCERS, ONE SLOT, and this is where they reconcile. `sizeCtl` is
    // written by [`chm_record_initial_table`] as the requested TABLE SIZE and
    // by the tail of this function as the 0.75 THRESHOLD — both faithful to
    // HotSpot, which reuses the field for both meanings at different points in
    // a map's life, and indistinguishable to [`chm_initial_table`], which
    // applies `next_power_of_two` to whatever it finds. That round trip is
    // exact once the floor below is in place, and not by luck: the threshold
    // this function writes is `cap - (cap >> 2)`, i.e. `0.75 * cap`, which is
    // strictly greater than `cap / 2`, so `next_power_of_two` of it is `cap`
    // for every `cap >= 4`. The mirror therefore reports the SAME virtual table
    // size to `chm_reorder_by_virtual_bucket` after a refresh as before one,
    // and repeated refreshes are idempotent.
    //
    // MEASURED, and this is why the floor is not cosmetic: without it, a
    // refresh of the `new ConcurrentHashMap<>(16)` in `RChmKeySetView` wrote
    // `sizeCtl = 12`, `chm_initial_table` then answered 16 where the
    // constructor had recorded 32, and `keySet()` came back
    // `[youralias, thirdalias, myalias]` against HotSpot's
    // `[thirdalias, myalias, youralias]` — a real regression, caught by that
    // vector, from a mirror that only ran on `keys()` before.
    let n = entries.len();
    // Round 15 wave 5, lane compat6 ([`chm_jdk25_sizing_enabled`]): the SAME
    // table `chm_reorder_by_virtual_bucket` models, so the published array and
    // the native iteration agree. The 16 floor was wrong for a table HotSpot
    // sizes smaller (`ConcurrentHashMap(2)` -> 4, a copy of a 3-entry map ->
    // 8), and `n > threshold` kept 16 buckets at exactly 12 entries, where
    // HotSpot has already resized to 32 (`addCount`: `s >= sizeCtl`).
    let mut cap = if chm_jdk25_sizing_enabled() {
        chm_initial_table(ctx, this).max(chm_jdk_table_size_for(n))
    } else {
        chm_initial_table(ctx, this).max(16)
    };
    while n > cap - (cap >> 2) {
        if cap > (1usize << 29) {
            return None;
        }
        cap <<= 1;
    }

    // GC discipline: every collected key/value must be pinned before the first
    // allocation and re-read after each one. `this` is already pinned by the
    // caller, whose handle is the base that unwinds this whole batch.
    let flat: Vec<Value> = entries.iter().flat_map(|(_, k, v)| [*k, *v]).collect();
    let (_flat_base, flat_pins) = pin_value_slice(ctx, &flat);
    let table = ctx.try_new_ref_array(node_cid, cap)?;
    let table_pin = ctx.pin_native_root(table);

    for (i, (hash, _, _)) in entries.iter().enumerate() {
        // The only GC point in the loop. Everything raw is stale after it.
        let node = ctx.alloc_object(node_cid, node_width);
        let table = ctx.read_native_pin(table_pin, table);
        let key = read_pinned_elem(ctx, flat_pins[i * 2], flat[i * 2]);
        let value = read_pinned_elem(ctx, flat_pins[i * 2 + 1], flat[i * 2 + 1]);
        // Read the current bin head straight out of the array rather than
        // carrying it across the allocation above.
        let idx = (*hash as usize) & (cap - 1);
        let head = ctx.get_array_element(table, idx);
        ctx.set_field(node, NODE_FIELD_HASH, Value::Int(*hash));
        ctx.set_field(node, NODE_FIELD_KEY, key);
        ctx.set_field(node, NODE_FIELD_VALUE, value);
        ctx.set_field(node, NODE_FIELD_NEXT, head);
        ctx.set_array_element(table, idx, Value::Object(Some(node)));
    }

    let this = ctx.read_native_pin(this_pin, this);
    let table = ctx.read_native_pin(table_pin, table);
    // The bookkeeping HotSpot carries alongside the array. `baseCount` is a
    // `long` and `sizeCtl` the 0.75 threshold; both are resolved by NAME, so a
    // receiver that does not declare them (a subclass with a shadowing layout)
    // simply gets the array and no bookkeeping rather than a stray store.
    ctx.set_field(this, table_slot, Value::Object(Some(table)));
    let cid = ctx.class_id_of_object(this);
    let width = ctx.object_num_fields(this);
    if let Some(slot) = ctx.resolve_field_index_by_class_id(cid, "baseCount") {
        if slot < width {
            ctx.set_field(this, slot, Value::Long(n as i64));
        }
    }
    if let Some(slot) = ctx.resolve_field_index_by_class_id(cid, "sizeCtl") {
        if slot < width {
            ctx.set_field(this, slot, Value::Int((cap - (cap >> 2)) as i32));
        }
    }
    Some(table)
}

// ===========================================================================
// ConcurrentHashMap's bulk-operation family.
//
// Every method here walks `table` through a `Traverser` in the real JDK, and a
// natively-backed CHM keeps its entries in a SEGMENTED layout that never
// populates `table` (`W7-96-chm-table-never-populated`). Unregistered, they
// therefore run real bytecode over an EMPTY tree and answer the identity
// element — which is the worst shape a wrong answer can take here, because
// every one of these methods has a perfectly ordinary-looking result for an
// empty map. MEASURED against HotSpot 25.0.4+7, both modes, on a three-entry
// map (`probes/ChmShadowSweep.java`):
//
// ```text
//   reduceKeys(1, (a,b) -> a+b)                      HotSpot "abc"  CratonVM null
//   reduceValuesToInt(1, Integer::intValue, 0, sum)  HotSpot 6      CratonVM 0
//   reduceValuesToLong(1, Integer::longValue, 0, +)  HotSpot 6      CratonVM 0
//   reduceEntriesToInt(1, e -> 1, 0, sum)            HotSpot 3      CratonVM 0
//   searchValues(1, v -> v == 2 ? "found" : null)    HotSpot found  CratonVM null
//   reduceKeys(1, String::toUpperCase, min)          HotSpot "A"    CratonVM null
// ```
//
// `reduceValues`, `searchKeys` and `search` were registered — the three the
// round-2 differential happened to ask — and answered correctly, which is why
// the family read as working. The rest is registered here as one block so the
// half-applied state cannot come back: a partly-registered family is worse
// than either endpoint, because the registered half is evidence for the
// unregistered half.
//
// PARALLELISM THRESHOLD. Every signature's leading `long` is ignored, exactly
// as `forEachEntry`/`search` already ignore it. It is documented as a hint —
// "the (estimated) number of elements needed for this operation to be executed
// in parallel" — and a sequential walk satisfies every one of these
// specifications; `Long.MAX_VALUE` (never parallel) is the value that makes
// the JDK behave identically.

/// Which of the three views a bulk operation walks.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChmBulkOver {
    Keys,
    Values,
    /// A `Map.Entry` per mapping. `AbstractMap$SimpleEntry` with the source map
    /// as its live backing, exactly what `forEachEntry` already mints and for
    /// the reason recorded there: a real `Map.Entry` so `checkcast` and
    /// reflection both work, rather than the class-less carrier that made
    /// `forEachEntry` unusable for any consumer that touched its argument.
    Entries,
}

/// Walk every mapping under the snapshot discipline the rest of this family
/// uses, calling `step` with the CURRENT receiver and the pinned key/value.
///
/// `step` answers `false` to stop early — which is what `search*` needs and
/// what makes it one walker rather than two.
pub(crate) fn chm_bulk_walk(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    step: &mut dyn FnMut(
        &mut dyn NativeContext,
        ObjectRef,
        Value,
        Value,
    ) -> Result<bool, MethodCallFailed>,
) -> Result<(), MethodCallFailed> {
    let this_pin = ctx.pin_native_root(this);
    let entries = chm_collect_all_entries(ctx, this);
    let flat: Vec<Value> = entries.iter().flat_map(|(k, v)| [*k, *v]).collect();
    let (_, flat_pins) = pin_value_slice(ctx, &flat);
    let out = (|| -> Result<(), MethodCallFailed> {
        for i in 0..entries.len() {
            let key = read_pinned_elem(ctx, flat_pins[i * 2], flat[i * 2]);
            let value = read_pinned_elem(ctx, flat_pins[i * 2 + 1], flat[i * 2 + 1]);
            let source = ctx.read_native_pin(this_pin, this);
            if !step(ctx, source, key, value)? {
                break;
            }
        }
        Ok(())
    })();
    ctx.unpin_native_roots(this_pin);
    out
}

/// The element a `Keys`/`Values`/`Entries` operation sees for one mapping.
pub(crate) fn chm_bulk_element(
    ctx: &mut dyn NativeContext,
    over: ChmBulkOver,
    source: ObjectRef,
    key: Value,
    value: Value,
) -> Result<Value, MethodCallFailed> {
    Ok(match over {
        ChmBulkOver::Keys => key,
        ChmBulkOver::Values => value,
        ChmBulkOver::Entries => Value::Object(Some(alloc_live_entry(
            ctx,
            "java/util/AbstractMap$SimpleEntry",
            key,
            value,
            source,
        )?)),
    })
}

/// `func` as an object, or `None` when the slot holds a null / is absent.
pub(crate) fn chm_fn_arg(args: &[Value], index: usize) -> Option<ObjectRef> {
    match args.get(index) {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    }
}

/// The bulk family's null contract.
///
/// The first cut of these registrations answered the identity element for a
/// null functional argument, on the theory that the JDK's own NPE comes from
/// dereferencing the argument inside a parallel task and a sequential walk
/// cannot reproduce that faithfully. MEASURED, and the theory was wrong:
///
/// ```text
///   chm.searchValues(1, null)   HotSpot NullPointerException   CratonVM no-throw
/// ```
///
/// The JDK checks explicitly — every one of these methods opens with
/// `if (searchFunction == null) throw new NullPointerException();` or the
/// equivalent `Objects.requireNonNull`, BEFORE it decides whether to go
/// parallel. Message-less, like the rest of this family. `reject_null_functional`
/// keeps the "a MISSING argument is a dispatch defect, not a Java-visible null"
/// distinction the rest of the file draws.
pub(crate) fn chm_reject_null_bulk_args(args: &[Value], indices: &[usize]) -> Result<(), MethodCallFailed> {
    for i in indices {
        reject_null_functional(args.get(*i))?;    }
    Ok(())
}

/// `reduceKeys(par, reducer)` / `reduceValues(..)` / `reduceEntries(..)`, and
/// their three-argument transformer forms.
///
/// The JDK's shape, which the fold below reproduces exactly: the transformer
/// (when present) may answer null, and a null transform is SKIPPED rather than
/// folded — so a transformer that filters is the idiom, and folding its null
/// would poison the accumulator.
pub(crate) fn chm_bulk_reduce(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    over: ChmBulkOver,
    transformer: Option<ObjectRef>,
    reducer: ObjectRef,
) -> MethodCallResult {
    let reducer_pin = ctx.pin_native_root(reducer);
    let transformer_pin = transformer.map(|t| ctx.pin_native_root(t));
    // THE ACCUMULATOR LIVES IN A ONE-SLOT REFERENCE ARRAY, not in a pin taken
    // inside the walk. `chm_bulk_walk` releases its own pin base on the way
    // out, and every pin the loop body took sits ABOVE that base -- so an
    // accumulator pinned per iteration would be dangling by the time this
    // function read it back. An array slot is remapped by the collector like
    // any other heap reference, and it is pinned once, HERE, below the walk.
    // gen r4w4/rooting2: `this` is pinned across the cell allocation below and
    // re-read for the walk, which would otherwise pin a pre-move receiver.
    let this_pin = ctx.pin_native_root(this);
    let cell = alloc_ref_array(ctx, 1);
    let cell_pin = ctx.pin_native_root(cell);
    let this = ctx.read_native_pin(this_pin, this);
    let mut seen = false;
    let walk = chm_bulk_walk(ctx, this, &mut |ctx, source, key, value| {
        let elem = chm_bulk_element(ctx, over, source, key, value)?;
        let elem = match (transformer, transformer_pin) {
            (Some(t), Some(pin)) => {
                let t = ctx.read_native_pin(pin, t);
                match ctx.invoke_virtual(
                    t,
                    "apply",
                    "(Ljava/lang/Object;)Ljava/lang/Object;",
                    &[elem],
                )? {
                    Some(v @ Value::Object(Some(_))) => v,
                    // A null transform is skipped, not folded.
                    _ => return Ok(true),
                }
            }
            _ => elem,
        };
        if !seen {
            let cell = ctx.read_native_pin(cell_pin, cell);
            ctx.set_array_element(cell, 0, elem);
            seen = true;
            return Ok(true);
        }
        let cell_now = ctx.read_native_pin(cell_pin, cell);
        let left = ctx.get_array_element(cell_now, 0);
        let f = ctx.read_native_pin(reducer_pin, reducer);
        let folded = ctx.invoke_virtual(
            f,
            "apply",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            &[left, elem],
        )?;
        let cell_now = ctx.read_native_pin(cell_pin, cell);
        ctx.set_array_element(cell_now, 0, folded.unwrap_or(Value::Object(None)));
        Ok(true)
    });
    let cell_now = ctx.read_native_pin(cell_pin, cell);
    let answer = if seen {
        ctx.get_array_element(cell_now, 0)
    } else {
        Value::Object(None)
    };
    ctx.unpin_native_roots(reducer_pin);
    walk?;
    Ok(Some(answer))
}

/// The pair-shaped `reduce(par, BiFunction transformer, BiFunction reducer)`,
/// whose transformer takes the KEY and the VALUE rather than one element.
pub(crate) fn chm_bulk_reduce_pairs(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    transformer: ObjectRef,
    reducer: ObjectRef,
) -> MethodCallResult {
    let transformer_pin = ctx.pin_native_root(transformer);
    let reducer_pin = ctx.pin_native_root(reducer);
    // See `chm_bulk_reduce` for why the accumulator is an array slot, and why
    // `this` is pinned across its allocation (gen r4w4/rooting2).
    let this_pin = ctx.pin_native_root(this);
    let cell = alloc_ref_array(ctx, 1);
    let cell_pin = ctx.pin_native_root(cell);
    let this = ctx.read_native_pin(this_pin, this);
    let mut seen = false;
    let walk = chm_bulk_walk(ctx, this, &mut |ctx, _source, key, value| {
        let t = ctx.read_native_pin(transformer_pin, transformer);
        let elem = match ctx.invoke_virtual(
            t,
            "apply",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            &[key, value],
        )? {
            Some(v @ Value::Object(Some(_))) => v,
            _ => return Ok(true),
        };
        if !seen {
            let cell = ctx.read_native_pin(cell_pin, cell);
            ctx.set_array_element(cell, 0, elem);
            seen = true;
            return Ok(true);
        }
        let cell_now = ctx.read_native_pin(cell_pin, cell);
        let left = ctx.get_array_element(cell_now, 0);
        let f = ctx.read_native_pin(reducer_pin, reducer);
        let folded = ctx.invoke_virtual(
            f,
            "apply",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            &[left, elem],
        )?;
        let cell_now = ctx.read_native_pin(cell_pin, cell);
        ctx.set_array_element(cell_now, 0, folded.unwrap_or(Value::Object(None)));
        Ok(true)
    });
    let cell_now = ctx.read_native_pin(cell_pin, cell);
    let answer = if seen {
        ctx.get_array_element(cell_now, 0)
    } else {
        Value::Object(None)
    };
    ctx.unpin_native_roots(transformer_pin);
    walk?;
    Ok(Some(answer))
}

/// The primitive width a `reduce*To{Int,Long,Double}` family member folds in.
///
/// Each carries its own method NAME and DESCRIPTOR pair because the functional
/// interfaces differ per width — `ToIntFunction.applyAsInt` against
/// `IntBinaryOperator.applyAsInt`, and so on. Getting one of those descriptors
/// wrong is a `NoSuchMethodError` at the first element, not a wrong number, so
/// they are spelled out rather than derived.
#[derive(Clone, Copy)]
pub(crate) enum ChmPrimWidth {
    Int,
    Long,
    Double,
}

impl ChmPrimWidth {
    pub(in crate::concurrent_hash_map) fn transform_method(self) -> (&'static str, &'static str, &'static str) {
        match self {
            // (name, one-arg descriptor, two-arg descriptor)
            ChmPrimWidth::Int => (
                "applyAsInt",
                "(Ljava/lang/Object;)I",
                "(Ljava/lang/Object;Ljava/lang/Object;)I",
            ),
            ChmPrimWidth::Long => (
                "applyAsLong",
                "(Ljava/lang/Object;)J",
                "(Ljava/lang/Object;Ljava/lang/Object;)J",
            ),
            ChmPrimWidth::Double => (
                "applyAsDouble",
                "(Ljava/lang/Object;)D",
                "(Ljava/lang/Object;Ljava/lang/Object;)D",
            ),
        }
    }
    pub(in crate::concurrent_hash_map) fn reduce_method(self) -> (&'static str, &'static str) {
        match self {
            ChmPrimWidth::Int => ("applyAsInt", "(II)I"),
            ChmPrimWidth::Long => ("applyAsLong", "(JJ)J"),
            ChmPrimWidth::Double => ("applyAsDouble", "(DD)D"),
        }
    }
    pub(in crate::concurrent_hash_map) fn zero(self) -> Value {
        match self {
            ChmPrimWidth::Int => Value::Int(0),
            ChmPrimWidth::Long => Value::Long(0),
            ChmPrimWidth::Double => Value::Double(0.0),
        }
    }
}

/// `reduce{Keys,Values,Entries}To{Int,Long,Double}(par, transformer, basis,
/// reducer)`, and — with `pairs` — the un-suffixed `reduceTo*` whose
/// transformer takes the key and the value.
///
/// Unlike the object-shaped reduces there is no "skip" case and no empty
/// answer: the BASIS is the identity and an empty map answers it, which is why
/// an unregistered `reduceValuesToInt` answering `0` looked exactly like a
/// correct answer for a map that happened to sum to zero.
pub(crate) fn chm_bulk_reduce_prim(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    over: ChmBulkOver,
    pairs: bool,
    transformer: ObjectRef,
    basis: Value,
    reducer: ObjectRef,
    width: ChmPrimWidth,
) -> MethodCallResult {
    let transformer_pin = ctx.pin_native_root(transformer);
    let reducer_pin = ctx.pin_native_root(reducer);
    let mut acc = basis;
    let (tname, tdesc1, tdesc2) = width.transform_method();
    let (rname, rdesc) = width.reduce_method();
    let walk = chm_bulk_walk(ctx, this, &mut |ctx, source, key, value| {
        let t = ctx.read_native_pin(transformer_pin, transformer);
        let mapped = if pairs {
            ctx.invoke_virtual(t, tname, tdesc2, &[key, value])?
        } else {
            let elem = chm_bulk_element(ctx, over, source, key, value)?;
            let t = ctx.read_native_pin(transformer_pin, transformer);
            ctx.invoke_virtual(t, tname, tdesc1, &[elem])?
        };
        let mapped = mapped.unwrap_or_else(|| width.zero());
        let f = ctx.read_native_pin(reducer_pin, reducer);
        let folded = ctx.invoke_virtual(f, rname, rdesc, &[acc, mapped])?;
        acc = folded.unwrap_or_else(|| width.zero());
        Ok(true)
    });
    ctx.unpin_native_roots(transformer_pin);
    walk?;
    Ok(Some(acc))
}

/// `search{Keys,Values,Entries}(par, fn)` — the first NON-NULL answer wins and
/// the walk stops there, which is the whole contract: a search function is
/// allowed to have side effects and must not see the rest of the map.
pub(crate) fn chm_bulk_search(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    over: ChmBulkOver,
    func: ObjectRef,
) -> MethodCallResult {
    let func_pin = ctx.pin_native_root(func);
    // See `chm_bulk_reduce` for why the answer is parked in an array slot, and
    // why `this` is pinned across its allocation (gen r4w4/rooting2).
    let this_pin = ctx.pin_native_root(this);
    let cell = alloc_ref_array(ctx, 1);
    let cell_pin = ctx.pin_native_root(cell);
    let this = ctx.read_native_pin(this_pin, this);
    let walk = chm_bulk_walk(ctx, this, &mut |ctx, source, key, value| {
        let elem = chm_bulk_element(ctx, over, source, key, value)?;
        let f = ctx.read_native_pin(func_pin, func);
        let hit = ctx.invoke_virtual(
            f,
            "apply",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            &[elem],
        )?;
        if let Some(v @ Value::Object(Some(_))) = hit {
            let cell = ctx.read_native_pin(cell_pin, cell);
            ctx.set_array_element(cell, 0, v);
            return Ok(false);        }
        Ok(true)
    });
    let cell_now = ctx.read_native_pin(cell_pin, cell);
    let answer = ctx.get_array_element(cell_now, 0);
    ctx.unpin_native_roots(func_pin);
    walk?;
    Ok(Some(answer))
}

/// `forEach{Key,Value,Entry}(par, transformer, action)` — the transformer form.
/// A null transform is SKIPPED, exactly as in the reduces.
pub(crate) fn chm_bulk_foreach_transformed(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    over: ChmBulkOver,
    pairs: bool,
    transformer: ObjectRef,
    action: ObjectRef,
) -> MethodCallResult {
    let transformer_pin = ctx.pin_native_root(transformer);
    let action_pin = ctx.pin_native_root(action);
    let walk = chm_bulk_walk(ctx, this, &mut |ctx, source, key, value| {
        let t = ctx.read_native_pin(transformer_pin, transformer);
        let mapped = if pairs {
            ctx.invoke_virtual(
                t,
                "apply",
                "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
                &[key, value],
            )?
        } else {
            let elem = chm_bulk_element(ctx, over, source, key, value)?;
            let t = ctx.read_native_pin(transformer_pin, transformer);
            ctx.invoke_virtual(
                t,
                "apply",
                "(Ljava/lang/Object;)Ljava/lang/Object;",
                &[elem],
            )?
        };
        let mapped = match mapped {
            Some(v @ Value::Object(Some(_))) => v,
            _ => return Ok(true),
        };
        let a = ctx.read_native_pin(action_pin, action);
        ctx.invoke_virtual(a, "accept", "(Ljava/lang/Object;)V", &[mapped])?;
        Ok(true)
    });
    ctx.unpin_native_roots(transformer_pin);
    walk?;
    Ok(None)
}

pub(crate) fn native_chm_elements(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => {
            return Ok(Some(Value::Object(Some(make_snapshot_enumeration(
                ctx,
                &[],
            )?))))
        }
    };
    let this_pin = ctx.pin_native_root(this);
    let out = (|| -> MethodCallResult {
        let this_now = ctx.read_native_pin(this_pin, this);
        let view = match native_chm_values(ctx, &[Value::Object(Some(this_now))])? {
            Some(Value::Object(Some(v))) => v,
            _ => return Ok(None),
        };
        let view_pin = ctx.pin_native_root(view);
        let view = ctx.read_native_pin(view_pin, view);
        let built = native_al_iterator(ctx, &[Value::Object(Some(view))])?;
        Ok(match built {
            Some(Value::Object(Some(it)))
                if ctx
                    .class_name_arc_of_id(ctx.class_id_of_object(it))
                    .is_some_and(|nm| &*nm == CHM_VALUE_ITER_CLASS) =>
            {
                Some(Value::Object(Some(it)))
            }
            _ => None,
        })
    })();
    let this = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
    if let Some(v) = out? {
        return Ok(Some(v));
    }
    let vals = chm_collect_all_values(ctx, this);
    Ok(Some(Value::Object(Some(make_snapshot_enumeration(
        ctx, &vals,
    )?))))
}

/// `ConcurrentHashMap.keys()` — the key-side twin of [`native_chm_elements`],
/// and it moves for the same reason: there may only be ONE producer of
/// `ConcurrentHashMap$KeyIterator`.
///
/// This body used to build a REAL cursor over the published mirror table while
/// `keySet().iterator()` built a snapshot under a different (fabricated) class.
/// That was survivable only while the two classes were different. Now that
/// `keySet().iterator()` mints the real `ConcurrentHashMap$KeyIterator` — which
/// is what HotSpot answers and what removes a Phase-1 fabrication from the
/// collection surface — a real cursor of the same class would meet the snapshot
/// natives registered on it, which is precisely how `elements()` came to hand
/// back `null` forever.
///
/// So `keys()` is `keySet().iterator()`, exactly as the JDK's is: both are a
/// `KeyIterator`, and that class implements `Enumeration` as well as
/// `Iterator`.
pub(crate) fn native_chm_keys(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => {
            return Ok(Some(Value::Object(Some(make_snapshot_enumeration(
                ctx,
                &[],
            )?))))
        }
    };
    let this_pin = ctx.pin_native_root(this);
    let out = (|| -> MethodCallResult {
        let this_now = ctx.read_native_pin(this_pin, this);
        let view = match native_chm_key_set(ctx, &[Value::Object(Some(this_now))])? {
            Some(Value::Object(Some(v))) => v,
            _ => return Ok(None),
        };
        let view_pin = ctx.pin_native_root(view);
        let view = ctx.read_native_pin(view_pin, view);
        let built = native_ksv_iterator(ctx, &[Value::Object(Some(view))])?;
        Ok(match built {
            Some(Value::Object(Some(it)))
                if ctx
                    .class_name_arc_of_id(ctx.class_id_of_object(it))
                    .is_some_and(|nm| &*nm == CHM_KEY_ITER_CLASS) =>
            {
                Some(Value::Object(Some(it)))
            }
            // A stripped image with no real `KeyIterator` lands on
            // `Arrays$ArrayItr`, which is NOT an `Enumeration`; the snapshot
            // enumeration below is the honest answer there.
            _ => None,
        })
    })();
    let this = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
    if let Some(v) = out? {
        return Ok(Some(v));
    }
    let keys = chm_collect_all_keys(ctx, this);
    Ok(Some(Value::Object(Some(make_snapshot_enumeration(
        ctx, &keys,
    )?))))
}

// ===========================================================================
// Collections$SetFromMap — the view returned by `Collections.newSetFromMap`.
//
// `SetFromMap` captures `s = m.keySet()` *once* in its constructor and routes
// `iterator()` / `toArray()` through that captured `s`. CratonVM's `keySet()`
// natives return an eager snapshot, so when `newSetFromMap` runs over a fresh
// (empty) map, `s` is permanently empty — every later `set.add(...)` mutates
// the live map `m` but not the stale `s`. `iterator()`/`toArray()` then come
// back empty even though `size()` (which delegates to `m.size()`) is correct.
//
// Felix's `CapabilitySet.match` returns `Collections.newSetFromMap(new
// ConcurrentHashMap())`, fills it via `addAll`, and `Felix.getServiceReferences`
// then does `new ArrayList<>(set)` — which calls `set.toArray()` and came back
// empty, so `getServiceReference("...StartLevel")` returned null and Felix's
// `AutoProcessor.processAutoProperties` NPE'd.
//
// Fix: intercept the affected `SetFromMap` view methods and route them through
// the *live* backing map `m` instead of the captured `s`.
// ===========================================================================

pub(crate) const SET_FROM_MAP_CLASS: &str = "java/util/Collections$SetFromMap";

/// Resolve the live backing `Map` field (`m`) of a `Collections$SetFromMap`.
pub(crate) fn set_from_map_backing(ctx: &dyn NativeContext, this: ObjectRef) -> Option<ObjectRef> {
    let slot = ctx.resolve_field_index(SET_FROM_MAP_CLASS, "m")?;
    match ctx.get_field(this, slot) {
        Value::Object(Some(m)) => Some(m),
        _ => None,
    }
}

pub(crate) fn native_set_from_map_size(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    // Delegate to the real backing map's `size()` via virtual dispatch, same
    // as `native_set_from_map_contains`/`_to_array`/`_iterator` below — NOT
    // through `native_map_size`, which has its own synthetic-layout
    // heuristic (`map_state`'s "is slot 0 an array" probe) that can misfire
    // on a REAL map whose own `table`/backing array field happens to land at
    // the same slot index CratonVM's synthetic HashMap uses for `buckets`.
    // For a real `LinkedCaseInsensitiveMap` (extends `LinkedHashMap`) this
    // silently read the wrong field as "size" and returned 0, breaking
    // `HeadersAdaptersTests.sizeWithMultipleValuesForHeaderShouldCountHeaders`
    // once `newSetFromMap` started returning a real `SetFromMap` for it.
    match set_from_map_backing(ctx, this) {
        Some(m) => ctx.invoke_virtual(m, "size", "()I", &[]),
        None => Ok(Some(Value::Int(0))),
    }
}

pub(crate) fn native_set_from_map_is_empty(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let size = match native_set_from_map_size(ctx, args)? {
        Some(Value::Int(n)) => n,
        _ => 0,
    };
    Ok(Some(Value::Int(if size == 0 { 1 } else { 0 })))
}

pub(crate) fn native_set_from_map_contains(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let key = args.get(1).copied().unwrap_or(Value::Object(None));
    // Delegate to the real backing map's `containsKey` via virtual dispatch so
    // an `IdentityHashMap` backing uses reference identity (and the correct
    // layout) rather than the synthetic-layout `native_map_contains_key`.
    match set_from_map_backing(ctx, this) {
        Some(m) => ctx.invoke_virtual(m, "containsKey", "(Ljava/lang/Object;)Z", &[key]),
        None => Ok(Some(Value::Int(0))),
    }
}

pub(crate) fn native_set_from_map_to_array(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    // Delegate to the real backing map's `keySet().toArray()` via virtual
    // dispatch (see `native_set_from_map_iterator` — `map_collect_keys` assumes
    // a synthetic map layout and mis-reads a real `IdentityHashMap`).
    let backing = match set_from_map_backing(ctx, this) {
        Some(m) => m,
        None => return Ok(Some(Value::Object(None))),
    };
    let set = match ctx.invoke_virtual(backing, "keySet", "()Ljava/util/Set;", &[])? {
        Some(Value::Object(Some(s))) => s,
        _ => return Ok(Some(Value::Object(None))),
    };
    ctx.invoke_virtual(set, "toArray", "()[Ljava/lang/Object;", &[])}

pub(crate) fn native_set_from_map_iterator(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    // Route through the live backing map's *real* `keySet().iterator()` via
    // virtual dispatch so the correct per-map-type view is used. The
    // `native_map_key_set` helper assumes CratonVM's synthetic HashMap field
    // layout (buckets/size/capacity) and reads garbage from a real
    // `IdentityHashMap` (whose backing is a flat alternating key/value table).
    // Since `newSetFromMap` only returns a real `Collections$SetFromMap` for an
    // `IdentityHashMap` backing (other backings get the synthetic HashSet), the
    // virtual call dispatches to the real map's keySet, which iterates the live
    // keys correctly. The snapshot is taken now, after every add().
    let backing = match set_from_map_backing(ctx, this) {
        Some(m) => m,
        None => return Ok(Some(Value::Object(None))),
    };
    let set = match ctx.invoke_virtual(backing, "keySet", "()Ljava/util/Set;", &[])? {
        Some(Value::Object(Some(s))) => s,
        _ => return Ok(Some(Value::Object(None))),
    };
    ctx.invoke_virtual(set, "iterator", "()Ljava/util/Iterator;", &[])}

pub(crate) fn register_set_from_map_natives(r: &mut NativeMethodRegistry) {
            let c = SET_FROM_MAP_CLASS;
    r.register(c, "size", "()I", native_set_from_map_size, cratonvm_native_api::NativeKind::Bridge);
    r.register(c, "isEmpty", "()Z", native_set_from_map_is_empty, cratonvm_native_api::NativeKind::Bridge);
    r.register(
        c,
        "contains",
        "(Ljava/lang/Object;)Z",
        native_set_from_map_contains, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "toArray",
        "()[Ljava/lang/Object;",
        native_set_from_map_to_array, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "iterator",
        "()Ljava/util/Iterator;",
        native_set_from_map_iterator, cratonvm_native_api::NativeKind::Bridge,
    );
    }

// ===========================================================================
// Properties — delegates to HashMap backing store (same 3-field layout)
// Field 0,1,2 = HashMap fields (buckets, size, capacity)
// Field 3 = defaults Properties reference (or null)
// ===========================================================================

pub(crate) const PROPS_FIELD_DEFAULTS: usize = 3;

/// Where this `Properties` receiver's `defaults` chain link actually lives.
///
/// JDK-ONLY-LAYOUT (L2, step 3): `PROPS_FIELD_DEFAULTS` (3) is a slot of the
/// fabricated four-field `Properties` model. On a real
/// `java.util.Properties` the inherited `Hashtable` fields occupy 0..=7 and
/// absolute slot 3 is `float loadFactor`, so `native_props_init`'s
/// `Object(None)` landed on a primitive — and the overlay hunter did NOT
/// report it, because its predicate only flagged `Object(Some(_))` over a
/// primitive descriptor. It was invisible in the census for that reason, not
/// absent from the defect. **Closed 2026-08-05 as L4 gap 3**:
/// `overlay_access_is_cross_type` now flags `Object(_)`, so a write of this
/// exact shape appears in the census, and the shadow-layout diff reports the
/// slot as a `TYPE` disagreement whether or not anything writes it.
///
/// Reading the same slot back was the other half: `getProperty`'s fallback
/// walk read slot 3, got a `Float`, and stopped — so on a real layout a
/// `Properties(defaults)` chain never resolved a single inherited key, even
/// though `native_props_init_defaults` had already learned to mirror the
/// reference into the real `defaults` field by name.
///
/// The real field is resolved on the RECEIVER's class (a `Properties`
/// subclass — `java.security.Provider`, Ant's `PropertySet` — inherits it),
/// and only a receiver that does not declare `defaults` at all, i.e. the
/// fabricated stub, falls back to the raw model slot.
pub(crate) fn props_defaults_slot(ctx: &dyn NativeContext, this: ObjectRef) -> usize {
    let class_id = ctx.class_id_of_object(this);
    if let Some(idx) = ctx.resolve_field_index_by_class_id(class_id, "defaults") {
        if idx < ctx.object_num_fields(this) {
            return idx;
        }
    }
    PROPS_FIELD_DEFAULTS
}

/// Read the `defaults` link of a `Properties`-shaped receiver.
pub(crate) fn props_get_defaults(ctx: &dyn NativeContext, this: ObjectRef) -> Value {
    ctx.get_field(this, props_defaults_slot(ctx, this))
}

/// Write the `defaults` link of a `Properties`-shaped receiver.
pub(crate) fn props_set_defaults(ctx: &mut dyn NativeContext, this: ObjectRef, value: Value) {
    let slot = props_defaults_slot(ctx, this);
    ctx.set_field(this, slot, value);
}

// JDK-ONLY-CLASSIFY: unknown — needs census. HIGHEST-RISK GROUP IN THIS CRATE.
// All 43 registrations target `java.util.Properties` methods that have concrete
// bytecode in JDK 25 and none that is ACC_NATIVE, which reads as "stub" on the
// static evidence alone. Do not act on that reading. `Properties` is on the
// real-JDK bootstrap path (`java.home` and friends are read out of it before
// most of the class library is usable), and this repo has already shipped a
// bootstrap failure — `InternalError: null property: java.home` — caused by a
// whole function's worth of `Properties` registrations picking up the wrong
// ambient category at a single call site. Anything that changes what survives
// here must be proven with a full `--real-jdk` boot, not with `javap`.
// Evidence needed before any retag: schema-v2 `invocations` for every triple in
// this group across a real-JDK boot, plus `overwrote` to see which of these are
// already being replaced by `native-builtins`' `register_properties_sidetable`.
pub(crate) fn register_properties_natives(registry: &mut NativeMethodRegistry) {
            let p = "java/util/Properties";

    // <init>()V — empty properties
    registry.register(p, "<init>", "()V", native_props_init, cratonvm_native_api::NativeKind::Bridge);

    // <init>(Properties)V — with defaults
    registry.register(
        p,
        "<init>",
        "(Ljava/util/Properties;)V",
        native_props_init_defaults, cratonvm_native_api::NativeKind::Bridge,
    );

    // setProperty(String, String) -> String (old value)
    registry.register(
        p,
        "setProperty",
        "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/Object;",
        native_map_put_entry, cratonvm_native_api::NativeKind::Bridge,
    );

    // getProperty(String) -> String
    registry.register(
        p,
        "getProperty",
        "(Ljava/lang/String;)Ljava/lang/String;",
        native_props_get_property, cratonvm_native_api::NativeKind::Bridge,
    );

    // getProperty(String, String) -> String (with default value)
    registry.register(
        p,
        "getProperty",
        "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;",
        native_props_get_property_default, cratonvm_native_api::NativeKind::Bridge,
    );

    // size() -> int
    registry.register(p, "size", "()I", native_map_size, cratonvm_native_api::NativeKind::Bridge);

    // isEmpty() -> boolean
    registry.register(p, "isEmpty", "()Z", native_map_is_empty, cratonvm_native_api::NativeKind::Bridge);

    // containsKey(Object) -> boolean
    registry.register(
        p,
        "containsKey",
        "(Ljava/lang/Object;)Z",
        native_map_contains_key, cratonvm_native_api::NativeKind::Bridge,
    );

    // containsValue(Object) -> boolean
    registry.register(
        p,
        "containsValue",
        "(Ljava/lang/Object;)Z",
        native_map_contains_value, cratonvm_native_api::NativeKind::Bridge,
    );

    // put(Object, Object) -> Object
    registry.register(
        p,
        "put",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        native_map_put_entry, cratonvm_native_api::NativeKind::Bridge,
    );

    // get(Object) -> Object
    registry.register(
        p,
        "get",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        native_map_get, cratonvm_native_api::NativeKind::Bridge,
    );

    // remove(Object) -> Object
    registry.register(
        p,
        "remove",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        native_map_remove, cratonvm_native_api::NativeKind::Bridge,
    );

    // clear()
    registry.register(p, "clear", "()V", native_map_clear, cratonvm_native_api::NativeKind::Bridge);

    // keySet() -> Set
    registry.register(p, "keySet", "()Ljava/util/Set;", native_map_key_set, cratonvm_native_api::NativeKind::Bridge);

    // values() -> Collection
    registry.register(p, "values", "()Ljava/util/Collection;", native_map_values, cratonvm_native_api::NativeKind::Bridge);

    // entrySet() -> Set
    registry.register(p, "entrySet", "()Ljava/util/Set;", native_map_entry_set, cratonvm_native_api::NativeKind::Bridge);

    // toString()
    registry.register(p, "toString", "()Ljava/lang/String;", native_map_to_string, cratonvm_native_api::NativeKind::Bridge);

    // putAll(Map)
    registry.register(p, "putAll", "(Ljava/util/Map;)V", native_map_put_all, cratonvm_native_api::NativeKind::Bridge);

    // load(InputStream) — parse key=value lines
    registry.register(p, "load", "(Ljava/io/InputStream;)V", native_props_load, cratonvm_native_api::NativeKind::Bridge);

    // load(Reader) — parse key=value lines
    registry.register(p, "load", "(Ljava/io/Reader;)V", native_props_load, cratonvm_native_api::NativeKind::Bridge);

    // store(OutputStream, String) — write key=value lines
    registry.register(
        p,
        "store",
        "(Ljava/io/OutputStream;Ljava/lang/String;)V",
        native_props_store, cratonvm_native_api::NativeKind::Bridge,
    );

    // propertyNames() -> Enumeration (returns keys as ArrayList iterator)
    registry.register(
        p,
        "propertyNames",
        "()Ljava/util/Enumeration;",
        native_props_property_names, cratonvm_native_api::NativeKind::Bridge,
    );

    // stringPropertyNames() -> Set<String>
    registry.register(
        p,
        "stringPropertyNames",
        "()Ljava/util/Set;",
        native_props_string_property_names, cratonvm_native_api::NativeKind::Bridge,
    );

    // getOrDefault(Object, Object) -> Object
    registry.register(
        p,
        "getOrDefault",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        native_map_get_or_default, cratonvm_native_api::NativeKind::Bridge,
    );

    // forEach(BiConsumer)
    registry.register(
        p,
        "forEach",
        "(Ljava/util/function/BiConsumer;)V",
        native_map_for_each, cratonvm_native_api::NativeKind::Bridge,
    );

    // hashCode
    registry.register(p, "hashCode", "()I", native_map_hash_code, cratonvm_native_api::NativeKind::Bridge);

    // equals
    registry.register(p, "equals", "(Ljava/lang/Object;)Z", native_map_equals, cratonvm_native_api::NativeKind::Bridge);

    // Also register under Hashtable (Properties extends Hashtable)
    let ht = "java/util/Hashtable";
    registry.register(ht, "<init>", "()V", native_map_init, cratonvm_native_api::NativeKind::Bridge);
    registry.register(ht, "<init>", "(I)V", native_hashtable_init_capacity, cratonvm_native_api::NativeKind::Bridge);
    registry.register(ht, "<init>", "(IF)V", native_hashtable_init_capacity_load, cratonvm_native_api::NativeKind::Bridge);
    registry.register(
        ht,
        "put",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        native_map_put_entry, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        ht,
        "get",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        native_map_get, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        ht,
        "remove",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        native_map_remove, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(ht, "size", "()I", native_map_size, cratonvm_native_api::NativeKind::Bridge);
    registry.register(ht, "isEmpty", "()Z", native_map_is_empty, cratonvm_native_api::NativeKind::Bridge);
    registry.register(
        ht,
        "containsKey",
        "(Ljava/lang/Object;)Z",
        native_map_contains_key, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        ht,
        "containsValue",
        "(Ljava/lang/Object;)Z",
        native_map_contains_value, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(ht, "clear", "()V", native_map_clear, cratonvm_native_api::NativeKind::Bridge);
    registry.register(ht, "keySet", "()Ljava/util/Set;", native_map_key_set, cratonvm_native_api::NativeKind::Bridge);
    registry.register(ht, "values", "()Ljava/util/Collection;", native_map_values, cratonvm_native_api::NativeKind::Bridge);
    registry.register(ht, "entrySet", "()Ljava/util/Set;", native_map_entry_set, cratonvm_native_api::NativeKind::Bridge);
    registry.register(ht, "toString", "()Ljava/lang/String;", native_map_to_string, cratonvm_native_api::NativeKind::Bridge);
    registry.register(ht, "putAll", "(Ljava/util/Map;)V", native_map_put_all, cratonvm_native_api::NativeKind::Bridge);
    // `Hashtable` (and `Properties`, which overrides them again) declare their
    // own bucket-walking `remove(k,v)` / `replace(k,v)` / `replace(k,old,new)`.
    // Same gap, same fix — see `register_map_conditional_mutators`. The null
    // rejections those overrides open with are honoured by
    // `map_remove_kv_null_contract` / `map_replace_kv_null_contract` /
    // `map_replace3_null_contract`, which route `Hashtable` and `Properties`
    // differently because the JDK does.
    register_map_conditional_mutators(registry, ht);
    register_map_conditional_mutators(registry, p);
    // Note: `keys()` / `elements()` are registered by
    // `cratonvm-native-builtins::deprecated_io_util::register_*_natives`,
    // which runs AFTER us in `vm_init`'s real-JDK arm. The force-native
    // override in `interpreter.rs::force_native_over_real_jdk_bytecode`
    // routes dispatch to that registration instead of the real-JDK
    // Hashtable.keys() body (which walks Hashtable's own internal
    // `table[]` — empty in our impl because `put` writes to a side-store).
    // Without the force-native entry, BC's
    // `AbstractX500NameStyle.copyHashTable(BCStyle.DefaultLookUp)` produced
    // an empty per-instance `defaultLookUp` and every
    // `BCStyle.attrNameToOID("cn"/...)` threw "Unknown object id".
    }

/// The `Hashtable`-inherited half of both `Properties` constructors: allocate
/// NOTHING, which is what HotSpot's own `Properties()` does.
///
/// # What used to happen, and what it cost
///
/// Both constructors called `map_init_eager`. `Properties` is deliberately
/// EXCLUDED from `CF_HASHTABLE_LAYOUT` (JDK 25 backs it with a side
/// `ConcurrentHashMap`, not with `Hashtable`'s buckets), so that call took the
/// GENERIC arm of [`map_init_inner`] and allocated an `Object[16]` into the
/// receiver's `table`. On a real-layout `Properties` that is 144 bytes of the
/// 224 an empty one retained, against HotSpot's 120.3.
///
/// # Why null is safe here, which is a MEASUREMENT and not an argument
///
/// The comment this replaces named one risk — "leaving it null would make
/// `map_state` report no buckets on a receiver whose native put path does not
/// go through `map_resize`" — and the risk is real in the abstract. It does not
/// apply to this class, on three independent counts:
///
///   * **Nothing writes that array.** `register_properties_sidetable`
///     (`native-builtins`) runs AFTER `register_collections_natives` in BOTH
///     `vm_init` arms and OVERWRITES every Map method on `java/util/Properties`
///     — `put`, `get`, `remove`, `clear`, `size`, `isEmpty`, `containsKey`,
///     `keySet`, `values`, `entrySet`, `keys`, `elements`, `contains`. Not one
///     of those bodies reads or writes a bucket table: a String->String pair
///     goes to the Rust side-table, anything else to the real `map` CHM, which
///     `put_non_string_into_chm` CREATES ON DEMAND. `map_carrier_class_for_receiver`
///     already records the consequence, measured: a `Properties` bucket table
///     reports `occupied=0` with `size=2`.
///   * **The one path that COULD reach the buckets already handles null.**
///     `native_map_put_evict_pinned` opens with
///     `if initial_buckets.is_none() || size + 1 > (cap * 3) / 4 { map_resize(..) }`
///     — the branch a JDK-bytecode-constructed `LinkedHashMap` has always taken
///     — so even a receiver that somehow reached the generic put would
///     materialise its table on first insert rather than drop the write.
///     `map_state`'s capacity fallback reads `threshold` when there is no
///     table, and the lazy arm sets it to 0, so that path answers
///     `MAP_DEFAULT_CAPACITY` — the same 16 the eager array had.
///   * **The busiest `Properties` in the VM has ALWAYS had a null table.**
///     `system_properties_object` allocates the `System.getProperties()`
///     singleton and writes NONE of its slots; `java.home` and every other
///     bootstrap property has been read out of a bucket-less `Properties`
///     since that factory was written. This change makes every other
///     `Properties` the same shape as the one that was already proving it.
///
/// HotSpot agrees, and that is the oracle rather than the reasoning above:
/// `probes/CollectionShapeCause.java`'s field dump on a real JDK 25 shows
/// `new Properties()` leaving `table` null, `loadFactor` 0.0 and `threshold` 0,
/// with the entries in a `ConcurrentHashMap map`.
pub(crate) fn props_init_map_half(ctx: &mut dyn NativeContext, this: ObjectRef) -> MethodCallResult {
    native_map_init(ctx, &[Value::Object(Some(this))])
}

pub(crate) fn native_props_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    // LAZY, like the JDK. See [`props_init_map_half`] for why the receiver's
    // bucket table stays null.
    props_init_map_half(ctx, this)?;
    props_set_defaults(ctx, this, Value::Object(None));
    Ok(None)
}

pub(crate) fn native_props_init_defaults(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    // LAZY, like the JDK. See [`props_init_map_half`] for why the receiver's
    // bucket table stays null.
    props_init_map_half(ctx, this)?;
    // Store the defaults reference in the REAL `defaults` field, resolved by
    // name on the receiver's class. On a real-layout `java.util.Properties` the
    // inherited Hashtable fields push `defaults` well past this native model's
    // `PROPS_FIELD_DEFAULTS` (3) — which there lands on `loadFactor`, a float —
    // so the raw slot both misplaces the reference and destroys a live field.
    // The side-table `getProperty` reads the by-name `defaults` slot to walk
    // the fallback chain (TC0622 follow-up).
    //
    // JDK-ONLY-LAYOUT (L2, step 3): this used to write BOTH slots, so a real
    // receiver still took the destructive write; `props_set_defaults` writes
    // the one slot that is right for this receiver, and every reader now
    // resolves the same way.
    props_set_defaults(ctx, this, args[1]);
    Ok(None)
}

/// `(oval instanceof String) ? (String)oval : null` — JDK 25
/// `Properties.getProperty(String)`'s own type test. A `Properties`/`Hashtable`
/// slot is `Object`-typed, so `p.put("k", someInteger)` is legal and MEASURED
/// (W8-E29-1) to answer that `Integer` back out of a `()Ljava/lang/String;`
/// method unless this filter runs before every value is handed to the caller.
pub(crate) fn props_value_is_string(ctx: &dyn NativeContext, o: ObjectRef) -> bool {
    ctx.class_name_of_id(ctx.class_id_of_object(o)).as_deref() == Some("java/lang/String")
}

pub(crate) fn native_props_get_property(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    // First check this map
    let result = native_map_get(ctx, args)?;
    if let Some(Value::Object(Some(o))) = result {
        if props_value_is_string(ctx, o) {
            return Ok(result);
        }
    }
    // Fall through to defaults chain
    let mut defaults_val = props_get_defaults(ctx, this);
    while let Value::Object(Some(defs)) = defaults_val {
        let def_result = native_map_get(ctx, &[Value::Object(Some(defs)), args[1]])?;
        if let Some(Value::Object(Some(o))) = def_result {
            if props_value_is_string(ctx, o) {
                return Ok(def_result);
            }
        }
        defaults_val = props_get_defaults(ctx, defs);
    }
    Ok(Some(Value::Object(None)))
}

pub(crate) fn native_props_get_property_default(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(args[2])),
    };
    // First check this map
    let result = native_map_get(ctx, &[args[0], args[1]])?;
    if let Some(Value::Object(Some(o))) = result {
        if props_value_is_string(ctx, o) {
            return Ok(result);
        }
    }
    // Fall through to defaults chain
    let mut defaults_val = props_get_defaults(ctx, this);
    while let Value::Object(Some(defs)) = defaults_val {
        let def_result = native_map_get(ctx, &[Value::Object(Some(defs)), args[1]])?;
        if let Some(Value::Object(Some(o))) = def_result {
            if props_value_is_string(ctx, o) {
                return Ok(def_result);
            }
        }
        defaults_val = props_get_defaults(ctx, defs);
    }
    // Return the default value arg
    Ok(Some(args[2]))
}

/// Load properties from a stream-like input: parse "key=value" or "key:value" lines.
/// Simplified: reads all bytes from the stream arg (field 0 = data for BAIS),
/// then parses lines.
pub(crate) fn native_props_load(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let stream = match args[1] {
        Value::Object(Some(s)) => s,
        _ => return Ok(None),
    };

    // Try to read the stream contents: if it's a BAIS, read data directly
    // Otherwise try to read it as a string field
    let text = props_read_input(ctx, stream);

    // MED fix: implement the JDK `Properties.load` spec (close to it,
    // anyway):
    //   - lines starting with `#` or `!` (after leading whitespace) are
    //     comments and skipped entirely;
    //   - blank lines (whitespace only) are skipped;
    //   - leading whitespace before a key is trimmed;
    //   - a line that ends with an *unescaped* trailing `\` continues
    //     onto the next line (the `\` is dropped and leading whitespace
    //     on the continuation line is trimmed);
    //   - the key terminates at the first unescaped whitespace, `=`, or
    //     `:`; subsequent whitespace and one optional `=`/`:` are then
    //     consumed; the rest of the logical line is the value;
    //   - inside both key and value the JDK escapes are honoured:
    //       \t \n \r \f \\ \" \'  → the corresponding ASCII char,
    //       \= \: \space          → the literal char (allows them in keys),
    //       \uXXXX               → the BMP codepoint XXXX,
    //       \<anything else>      → the trailing char itself (per JDK).
    //
    // This is intentionally implemented in pure Rust against the in-memory
    // text rather than dispatching back to JDK Properties for parity —
    // the previous implementation silently truncated/corrupted any
    // real-world `.properties` file with escapes or continuations.
    let this_pin = ctx.pin_native_root(this);
    for (key, value) in props_parse_logical_lines(&text) {
        let this = ctx.read_native_pin(this_pin, this);
        if key.is_empty() {
            continue;
        }
        let key_obj = ctx.create_string(&key);
        let val_obj = ctx.create_string(&value);
        native_map_put(
            ctx,
            &[
                Value::Object(Some(this)),
                Value::Object(Some(key_obj)),
                Value::Object(Some(val_obj)),
            ],
        )?;
    }
    Ok(None)
}

/// Parse a `.properties` text into `(key, value)` pairs per the JDK
/// `Properties.load` spec — handles comments, blank lines, leading-
/// whitespace trim, trailing-backslash continuations, the
/// `\t\n\r\f\\\"\'\=\:` and `\uXXXX` escape sequences in both keys and
/// values, and the natural key/value separator (the first unescaped
/// whitespace, `=`, or `:`).
pub(crate) fn props_parse_logical_lines(text: &str) -> Vec<(String, String)> {
    let raw_lines: Vec<&str> = text.split('\n').collect();
    let mut out: Vec<(String, String)> = Vec::new();
    let mut i = 0;
    while i < raw_lines.len() {
        // Strip a trailing `\r` (CRLF inputs) without losing escapes.
        let mut line = raw_lines[i].trim_end_matches('\r').to_string();
        // Trim leading whitespace per spec.
        let trimmed_start = line.trim_start();
        let leading_skipped = line.len() - trimmed_start.len();
        // Comment / blank — skipped entirely (continuations do not apply
        // to comment lines).
        if trimmed_start.is_empty()
            || trimmed_start.starts_with('#')
            || trimmed_start.starts_with('!')
        {
            i += 1;
            continue;
        }
        line.drain(..leading_skipped);

        // Apply continuation: if the line ends with an *odd* number of
        // trailing backslashes, the last `\` is a continuation marker
        // and the following physical line is appended (after trimming
        // its leading whitespace).
        while ends_with_unescaped_backslash(&line) {
            line.pop(); // remove the trailing backslash
            i += 1;
            if i >= raw_lines.len() {
                break;
            }
            let next = raw_lines[i].trim_end_matches('\r');
            line.push_str(next.trim_start());
        }
        i += 1;

        // Walk the logical line to find the key/value boundary. The key
        // ends at the first unescaped whitespace, `=`, or `:`.
        let bytes = line.as_bytes();
        let mut idx = 0;
        let mut key_buf = String::new();
        while idx < bytes.len() {
            let b = bytes[idx];
            if b == b'\\' {
                // Pull one escape into the key.
                let (decoded, consumed) = decode_escape(&bytes[idx..]);
                key_buf.push_str(&decoded);
                idx += consumed;
                continue;
            }
            if b == b' ' || b == b'\t' || b == b'\x0c' || b == b'=' || b == b':' {
                break;
            }
            // Multi-byte UTF-8: copy through.
            let ch_end = utf8_char_end(bytes, idx);
            key_buf.push_str(std::str::from_utf8(&bytes[idx..ch_end]).unwrap_or(""));
            idx = ch_end;
        }

        // Skip whitespace, then at most one `=` or `:`, then more whitespace.
        while idx < bytes.len() && matches!(bytes[idx], b' ' | b'\t' | b'\x0c') {
            idx += 1;
        }
        if idx < bytes.len() && (bytes[idx] == b'=' || bytes[idx] == b':') {
            idx += 1;
            while idx < bytes.len() && matches!(bytes[idx], b' ' | b'\t' | b'\x0c') {
                idx += 1;
            }
        }

        // Remainder is the value (with escapes decoded).
        let mut val_buf = String::new();
        while idx < bytes.len() {
            let b = bytes[idx];
            if b == b'\\' {
                let (decoded, consumed) = decode_escape(&bytes[idx..]);
                val_buf.push_str(&decoded);
                idx += consumed;
                continue;
            }
            let ch_end = utf8_char_end(bytes, idx);
            val_buf.push_str(std::str::from_utf8(&bytes[idx..ch_end]).unwrap_or(""));
            idx = ch_end;
        }

        out.push((key_buf, val_buf));
    }
    out
}

/// True when the line ends with a backslash that is *not* itself
/// escaped (i.e. odd run of trailing backslashes).
pub(crate) fn ends_with_unescaped_backslash(line: &str) -> bool {
    let bytes = line.as_bytes();
    let mut n = 0usize;
    for &b in bytes.iter().rev() {
        if b == b'\\' {
            n += 1;
        } else {
            break;
        }
    }
    n % 2 == 1
}

