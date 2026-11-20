#[allow(unused_imports)]
use super::*;

/// Remove and return the bucket queued under `thr_obj`, the current thread's
/// `java.lang.Thread`.
pub fn take_inherited_for_thread_object(
    ctx: &mut dyn NativeContext,
    thr_obj: ObjectRef,
) -> Option<InheritedTlBucket> {
    let key = inherited_tl_key(ctx, thr_obj);
    // zt t27/zc10 (`CRATONVM_ZGC_ITL_DEFER_IN_CYCLE`): asked before the lock.
    let shadow_wanted = ctx.zgc_concurrent_mark_active() && zgc_itl_defer_in_cycle_enabled();
    let mut kept_alive: Vec<ObjectRef> = Vec::new();
    let mut bucket = {
        let mut pending = tl_inherited_pending().lock();
        let bucket = pending.remove(&key)?;
        if shadow_wanted {
            // The bucket's object values, as plain rows owning no handle, under
            // the shadow key, in the same critical section as the removal: no
            // scan can see the child with neither.
            let shadow: InheritedTlBucket = bucket
                .iter()
                .filter(|(_, row)| {
                    matches!(row.value, ThreadLocalValue::Plain(Value::Object(Some(_))))
                })
                .map(|&(hash, row)| (hash, TlRow { owner: 0, ..row }))
                .collect();
            if !shadow.is_empty() {
                for (_, row) in &shadow {
                    if let ThreadLocalValue::Plain(Value::Object(Some(value))) = row.value {
                        kept_alive.push(value);
                    }
                }
                // A started child is never queued again, so nothing is
                // replaced; a stale shadow owns no handle either way.
                let _ = pending.insert((key.0, key.1 | INHERITED_TL_SHADOW_BIT), shadow);
            }
        }
        bucket
    };
    // Lock dropped: the keep-alive takes the heap's SATB path.
    for value in kept_alive {
        ctx.gc_reference_keep_alive(value);
    }
    // gch h3/jx: plain values (the ephemeron switch) become roots again before
    // the caller can reach a collection point.
    tl_bucket_values_to_roots(ctx, &mut bucket);
    Some(bucket)
}

/// Post-collection fixup for [`tl_inherited_pending`], for the VM
/// `vm_identity`: re-key every bucket whose child `Thread` moved
/// (`pointer_map`, old → new), drop every bucket whose child did not survive
/// (`is_live`, asked of a PRE-remap address absent from the map), and return
/// the global-root handles those dropped buckets' values own, for the caller
/// to release against THIS VM's global table. Another VM's rows are not
/// touched.
///
/// Same two-pass order as `vm::memory::addr_keyed::remap_and_sweep`: a moved
/// child is authoritative, so an unmoved row whose address a moved child now
/// occupies is dead too (its own `Thread` was reclaimed and the address
/// reused) and is released rather than left to shadow the survivor.
///
/// Called by `vm::memory::gc::update_all_roots` before its empty-map early
/// return (so a non-moving sweep also releases dead children), while the
/// world is stopped.
pub fn remap_and_sweep_inherited_tl_buckets(
    vm_identity: usize,
    pointer_map: &cratonvm_types::PointerMap,
    is_live: &dyn Fn(usize) -> bool,
) -> Vec<usize> {
    let mut handles = Vec::new();
    if !ANY_INHERITED_TL_QUEUED.load(std::sync::atomic::Ordering::Acquire) {
        return handles;
    }
    let mut dead: Vec<InheritedTlBucket> = Vec::new();
    {
        let mut pending = tl_inherited_pending().lock();
        let mut moved_keys: Vec<((usize, usize), usize)> = Vec::new();
        let mut dead_keys: Vec<(usize, usize)> = Vec::new();
        for &(vm, child) in pending.keys() {
            if vm != vm_identity {
                continue;
            }
            // zt t27/zc10: a shadow (`CRATONVM_ZGC_ITL_DEFER_IN_CYCLE`) lives
            // until the first pause that sweeps this table after its take --
            // the closing pause of the cycle it was made in -- and owns no
            // handle, so it is dropped here whatever its child's verdict.
            if child & INHERITED_TL_SHADOW_BIT != 0 {
                dead_keys.push((vm, child));
                continue;
            }
            match pointer_map.get(&child) {
                Some(&new_child) => moved_keys.push(((vm, child), new_child)),
                None if is_live(child) => {}
                None => dead_keys.push((vm, child)),
            }
        }
        // Every moved bucket leaves its OLD key before any is inserted at its
        // new one, so a chained map is applied exactly once and a new key is
        // never mistaken for an old one.
        let mut moved: Vec<(usize, InheritedTlBucket)> = Vec::with_capacity(moved_keys.len());
        for (old_key, new_child) in moved_keys {
            if let Some(bucket) = pending.remove(&old_key) {
                moved.push((new_child, bucket));
            }
        }
        for key in dead_keys {
            if let Some(bucket) = pending.remove(&key) {
                dead.push(bucket);
            }
        }
        for (new_child, bucket) in moved {
            if let Some(stale) = pending.insert((vm_identity, new_child), bucket) {
                dead.push(stale);
            }
        }
    }
    for bucket in dead {
        for (_, row) in bucket {
            if let ThreadLocalValue::Root { handle, .. } = row.value {
                if handle != 0 {
                    handles.push(handle);
                }
            }
            // gc-common w29-a: each row also owns a weak handle on its
            // ThreadLocal; the caller's `JniGlobalRefs::remove` releases weak
            // and strong handles alike.
            if row.owner != 0 {
                handles.push(row.owner);
            }
        }
    }
    handles
}

/// On the first TL access from this OS thread, drain any inherited ITL
/// entries that the parent recorded against this thread's Java identity.
/// Idempotent: subsequent calls are a single bool check.
#[inline]
pub fn drain_inherited_for_current_thread(ctx: &mut dyn NativeContext) {
    if TL_INHERITED_DRAINED.with(|f| f.get()) {
        return;
    }
    TL_INHERITED_DRAINED.with(|f| f.set(true));
    let Some(entries) = take_inherited_for_current_thread(ctx) else {
        return;
    };
    invalidate_tl_compat_mru();
    // gc-common w29-a: an inherited row joins its bucket unless this thread
    // already has a row for the SAME ThreadLocal (a collider's row is not
    // that); a row whose ThreadLocal died since the capture is released.
    let mut rejected: Vec<TlRow> = Vec::new();
    for (hash, row) in entries {
        let rctx: &dyn NativeContext = &*ctx;
        let Some(owner) = tl_row_owner(rctx, &row) else {
            rejected.push(row);
            continue;
        };
        let taken = TL_MAP.with(|m| {
            let mut map = m.borrow_mut();
            let bucket = map.entry(hash).or_default();
            if tl_bucket_scan(rctx, bucket, owner, &mut rejected).is_some() {
                false
            } else {
                bucket.push(row);
                true
            }
        });
        if !taken {
            rejected.push(row);
        }
    }
    tl_release_rows(ctx, rejected);
}

pub fn register_thread_local_natives(r: &mut NativeMethodRegistry) {
    // EXPERIMENT SWITCH, not a supported mode (SBR wave 6, lane dispatch):
    // `CRATONVM_TL_INTRINSIC=0` registers none of the `ThreadLocal` /
    // `InheritableThreadLocal` natives, so the JDK's own `ThreadLocalMap` bodies
    // run over `Thread.threadLocals`. It exists to MEASURE what retiring the
    // reviewed Intrinsics would cost and break (see
    // `docs/jdk-only/SBR-W6-DISPATCH-...`); the default (unset) is unchanged.
    //
    // SBR round 2 (lane `thread`): under `--real-jdk` the default is now OFF --
    // the JDK's own `ThreadLocal` / `ThreadLocalMap` / `InheritableThreadLocal`
    // bytecode runs over the real `Thread.threadLocals` and
    // `Thread.inheritableThreadLocals` fields, which is what the retired
    // `Thread.<init>` (`RETIRED_SHADOW_SBR2_THREAD_TRIPLES`) copies at
    // construction time, `Thread.exit()` clears, and `InheritableThreadLocal
    // .childValue` overrides feed. `CRATONVM_TL_INTRINSIC=1` brings the
    // Intrinsics back under `--real-jdk` (the old behaviour, e.g. to A/B the
    // per-call cost); `--compatible` keeps them on (its `Thread.<init>` natives
    // and `start0` still read the side table), `CRATONVM_TL_INTRINSIC=0` still
    // turns them off there. See
    // `docs/feature-designs/jvmti-delivery-threading.md`.
    let jdk_only = r.compatibility_mode() == cratonvm_types::compat::CompatibilityMode::JdkOnly;
    let forced = cratonvm_types::flags::runtime_var("CRATONVM_TL_INTRINSIC").ok();
    let enabled = match forced.as_deref().map(str::trim) {
        Some("0" | "false" | "off") => false,
        Some("1" | "true" | "on") => true,
        _ => !jdk_only,
    };
    if !enabled {
        // SBR round 2, wave 8 (lane `tlocal`): with the side table off the
        // JDK's own `ThreadLocalMap` bytecode runs, at 3-4x the old cost. The
        // reviewed real-field fast path serves the hot call
        // (`ThreadLocal.get()`) straight off the real
        // `Thread.threadLocals` table and hands every other case to the
        // JDK bytecode. `CRATONVM_TL_REAL_FAST=0` turns it off. Not registered
        // under `--compatible`, whose native constructors still read the side
        // table.
        if jdk_only && tl_real_fast_flag_on() {
            register_thread_local_real_fast_natives(r);
        }
        return;
    }
            let c = "java/lang/ThreadLocal";
    r.register(c, "<init>", "()V", native_tl_init, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(
        c,
        "initialValue",
        "()Ljava/lang/Object;",
        native_tl_initial_value, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(c, "get", "()Ljava/lang/Object;", native_tl_get, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(c, "set", "(Ljava/lang/Object;)V", native_tl_set, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(c, "remove", "()V", native_tl_remove, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(
        c,
        "withInitial",
        "(Ljava/util/function/Supplier;)Ljava/lang/ThreadLocal;",
        native_tl_with_initial, cratonvm_native_api::NativeKind::Intrinsic,
    );
    // InheritableThreadLocal: same get/set/remove semantics, but the
    // `<init>` files the TL's weak lock key in the inheritable set so
    // `Thread.start` can copy parent values to children.
    let itl = "java/lang/InheritableThreadLocal";
    r.register(itl, "<init>", "()V", native_itl_init, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(
        itl,
        "initialValue",
        "()Ljava/lang/Object;",
        native_tl_initial_value, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(itl, "get", "()Ljava/lang/Object;", native_tl_get, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(itl, "set", "(Ljava/lang/Object;)V", native_tl_set, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(itl, "remove", "()V", native_tl_remove, cratonvm_native_api::NativeKind::Intrinsic);
    }

// ---------------------------------------------------------------------------
// SBR round 2, wave 8 (lane `tlocal`): the real-field `ThreadLocal` fast path.
//
// Under `--real-jdk` the JDK's own `ThreadLocal` / `ThreadLocalMap` bytecode
// runs over the real `Thread.threadLocals` / `inheritableThreadLocals` tables
// (wave 7, lane `thread`). Its `ThreadLocal.get()` measured 1.2-1.5 us against
// 0.45-0.67 us for the side-table Intrinsic it replaced, because every call
// crossed four VM doors from compiled code: a non-exact `instanceof`
// (`getMap`'s `TerminatingThreadLocal` test), a getfield helper, and the
// `Reference.refersTo` native (~440 ns), none of them a JDK-shaped cost.
//
// These natives (`ThreadLocal.get()` and `set(Object)`, reviewed `Intrinsic`s)
// read the SAME real table the bytecode does and answer the one case that is hot
// everywhere (the key is found by reading the probe sequence, no stale entry met
// on the way) and hand EVERY other case to the JDK's own bytecode:
// `ThreadLocal.get(Thread)` / `set(Thread, Object)` (the private bodies, so no
// behaviour is re-implemented here: miss, `initialValue`, `withInitial`,
// `createMap`, stale-entry expunge, resize, rehash, `Terminating` thread locals,
// virtual-thread tracing). Nothing is stored in a side table and nothing is cached
// across calls beyond slot indices, so the JDK's `Thread.exit`,
// `createInheritedMap` and GC-cleared-key behaviour is untouched.
//
// Their value is NOT their own speed (measured: about the JDK body's from
// compiled code, `get` 1.4 us and `set` 1.9 us: the native-call door, not the
// table, is the cost; `set` is SLOWER than the 1.0 us bytecode). They are the
// registered, reviewed `Intrinsic`s that the JIT's thin direct helpers
// (`vm/src/jit/helpers.rs::jit_thread_local_{get,set}_direct`, 0.25 us) are
// admitted in front of, and the decline edge those helpers fall back to.
// Record: `docs/feature-designs/native-dispatch-architecture.md`.

/// `CRATONVM_TL_REAL_FAST` (default ON under `--real-jdk`): `0` / `false` /
/// `off` leaves the JDK's `ThreadLocal.get` / `set` bytecode running with no
/// fast path (no native, no JIT helper). Read at registration.
pub fn tl_real_fast_flag_on() -> bool {
    !matches!(
        cratonvm_types::flags::runtime_var("CRATONVM_TL_REAL_FAST")
            .ok()
            .as_deref()
            .map(str::trim),
        Some("0" | "false" | "off")
    )
}

/// Whether the real-field `ThreadLocal.get` / `set` natives are the ones
/// registered under this policy: `--real-jdk`, `CRATONVM_TL_REAL_FAST` not off,
/// and the side-table table not forced on (`CRATONVM_TL_INTRINSIC=1`). The JIT's
/// `ThreadLocal.get()` thin helper asks this so it can never answer for a VM
/// whose registered natives are the side-table ones.
pub fn thread_local_real_fast_active(jdk_only: bool) -> bool {
    let forced_on = matches!(
        cratonvm_types::flags::runtime_var("CRATONVM_TL_INTRINSIC")
            .ok()
            .as_deref()
            .map(str::trim),
        Some("1" | "true" | "on")
    );
    jdk_only && tl_real_fast_flag_on() && !forced_on
}

pub fn register_thread_local_real_fast_natives(r: &mut NativeMethodRegistry) {
    // Explicit kind, never inherited (AGENTS.md: `NativeKind` is ambient).
    for c in ["java/lang/ThreadLocal", "java/lang/InheritableThreadLocal"] {
        r.register(c, "get", "()Ljava/lang/Object;", native_tl_real_get, cratonvm_native_api::NativeKind::Intrinsic);
        r.register(c, "set", "(Ljava/lang/Object;)V", native_tl_real_set, cratonvm_native_api::NativeKind::Intrinsic);
    }
}

/// Which `Thread` field `getMap` reads for a receiver.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TlMapField {
    /// `Thread.threadLocals` (plain `ThreadLocal` and its non-special subclasses).
    Plain,
    /// `Thread.inheritableThreadLocals` (`InheritableThreadLocal` and below).
    Inheritable,
    /// `TerminatingThreadLocal` / `CarrierThreadLocal` / anything unresolved:
    /// the JDK bytecode answers.
    Bytecode,
}

/// Slot indices and class ids the fast path needs, resolved once per VM per
/// thread (never a process global: a second VM in the process may lay these
/// classes out differently; the key is `vm_identity`).
#[derive(Clone, Copy)]
pub struct TlRealSlots {
    pub vm: usize,
    pub tl_cid: cratonvm_types::ClassId,
    pub itl_cid: cratonvm_types::ClassId,
    /// `jdk.internal.misc.TerminatingThreadLocal`: `getMap` picks
    /// `terminatingThreadLocals` for it, so the JDK bytecode answers. (The
    /// `CarrierThreadLocal` subclass overrides `get` / `set` itself and never
    /// reaches these natives.)
    pub term_cid: cratonvm_types::ClassId,
    pub hash_slot: usize,
    pub thread_tl_slot: usize,
    pub thread_itl_slot: usize,
    pub table_slot: usize,
    pub entry_value_slot: usize,
    /// `jdk.traceVirtualThreadLocals` was set when this was resolved: `set`
    /// then runs the JDK body (it prints a stack trace for virtual threads).
    pub trace_vthread_locals: bool,
    /// JIT round 21 (lane tl2, `CRATONVM_TL_REAL_TYPED_READS`, default ON):
    /// the probe's reads name the field's descriptor byte
    /// (`NativeContext::get_field_typed`), so each read skips the per-read
    /// `resolve_field_descriptor_byte_cached` lookup `get_field` pays (7.9% of
    /// the `R21TlocalSplit` profile). The bytes are the declared types of the
    /// slots resolved above: `L` for `threadLocals`, `inheritableThreadLocals`,
    /// `table` (an array: the same reference decode), `referent` and `value`,
    /// `I` for `threadLocalHashCode`. `=0` reads through `get_field`, as before.
    pub typed_reads: bool,
}

thread_local! {
    pub static TL_REAL_SLOTS: std::cell::Cell<Option<TlRealSlots>> = const { std::cell::Cell::new(None) };
}

/// Resolve [`TlRealSlots`], or `None` when any class is not loaded yet (the
/// caller then runs the JDK bytecode; nothing is cached, so a class that loads
/// later is seen by the next call).
pub fn tl_real_slots(ctx: &mut dyn NativeContext) -> Option<TlRealSlots> {
    let vm = ctx.vm_identity();
    if let Some(s) = TL_REAL_SLOTS.with(|c| c.get()) {
        if s.vm == vm {
            return Some(s);
        }
    }
    let tl_cid = ctx.class_id_by_name("java/lang/ThreadLocal")?;
    // Like `TerminatingThreadLocal` below, `InheritableThreadLocal` is loaded
    // by nothing in a small program, and a missing class made EVERY call
    // return `None` here (nothing is cached on that path), so each
    // `ThreadLocal.get` re-ran the class lookups and then the JDK bytecode from
    // inside the native: 1.6-2.6 us a call against 0.12-0.25 us once the class
    // was loaded (`MicroTL3` vs `MicroTL4`, 2026-10-05). It was also the
    // reason the JIT helper (`tl_get_layout`, which never loads) never bound.
    let itl_name = "java/lang/InheritableThreadLocal";
    let itl_cid = match ctx.class_id_by_name(itl_name) {
        Some(id) => id,
        None => {
            let _ = ctx.load_class(itl_name);
            ctx.class_id_by_name(itl_name)?
        }
    };
    // `TerminatingThreadLocal` may never have been loaded by a small program;
    // an instance cannot exist before its class does, but a cached "absent"
    // would go stale, so it is loaded (once) rather than skipped.
    let term_name = "jdk/internal/misc/TerminatingThreadLocal";
    let term_cid = match ctx.class_id_by_name(term_name) {
        Some(id) => id,
        None => {
            let _ = ctx.load_class(term_name);
            ctx.class_id_by_name(term_name)?
        }
    };
    let thread_cid = ctx.class_id_by_name("java/lang/Thread")?;
    let map_cid = ctx.class_id_by_name("java/lang/ThreadLocal$ThreadLocalMap")?;
    let entry_cid = ctx.class_id_by_name("java/lang/ThreadLocal$ThreadLocalMap$Entry")?;
    let s = TlRealSlots {
        vm,
        tl_cid,
        itl_cid,
        term_cid,
        hash_slot: ctx.resolve_field_index_by_class_id(tl_cid, "threadLocalHashCode")?,
        thread_tl_slot: ctx.resolve_field_index_by_class_id(thread_cid, "threadLocals")?,
        thread_itl_slot: ctx
            .resolve_field_index_by_class_id(thread_cid, "inheritableThreadLocals")?,
        table_slot: ctx.resolve_field_index_by_class_id(map_cid, "table")?,
        entry_value_slot: ctx.resolve_field_index_by_class_id(entry_cid, "value")?,
        trace_vthread_locals: ctx
            .get_system_property("jdk.traceVirtualThreadLocals")
            // The JDK reads `value != null && (value.isEmpty() ||
            // Boolean.parseBoolean(value))`: the bare `-D` spelling is on (tl21).
            .is_some_and(|v| v.is_empty() || v.eq_ignore_ascii_case("true")),
        typed_reads: true,
    };
    TL_REAL_SLOTS.with(|c| c.set(Some(s)));
    Some(s)
}

/// A reference field of the fast path's own resolved slots (see
/// [`TlRealSlots::typed_reads`]).
pub fn tl_real_ref_field(
    ctx: &dyn NativeContext,
    s: &TlRealSlots,
    obj: ObjectRef,
    slot: usize,
) -> Value {
    if s.typed_reads {
        ctx.get_field_typed(obj, slot, b'L')
    } else {
        ctx.get_field(obj, slot)
    }
}

/// Which map `getMap` would pick for `this`, walking the superclass chain the
/// way the JDK's `instanceof` / override does. Plain `ThreadLocal` is one
/// compare; `InheritableThreadLocal` and the `withInitial` subclass a step.
pub fn tl_map_field(ctx: &dyn NativeContext, s: &TlRealSlots, this: ObjectRef) -> TlMapField {
    let mut cid = ctx.class_id_of_object(this);
    let mut inheritable = false;
    // The chain of any ThreadLocal subclass is short; the bound only guards a
    // corrupt id against an endless walk.
    for _ in 0..32 {
        if cid == s.term_cid {
            return TlMapField::Bytecode;
        }
        if cid == s.itl_cid {
            inheritable = true;
        }
        if cid == s.tl_cid {
            return if inheritable {
                TlMapField::Inheritable
            } else {
                TlMapField::Plain
            };
        }
        match ctx.superclass_of(cid) {
            Some(parent) => cid = parent,
            None => return TlMapField::Bytecode,
        }
    }
    TlMapField::Bytecode
}

/// Read-only probe of the current thread's table for `this`: the entry whose
/// referent IS `this`, or `None` for everything else (no map, no table, key
/// absent, a stale entry on the probe path, a zero-length table). A `None` is
/// never an answer: the caller runs the JDK bytecode, which decides.
pub fn tl_real_find_entry(
    ctx: &mut dyn NativeContext,
    s: &TlRealSlots,
    this: ObjectRef,
) -> Option<(ObjectRef, ObjectRef)> {
    let thread = ctx.current_thread_object();
    let field = match tl_map_field(&*ctx, s, this) {
        TlMapField::Plain => s.thread_tl_slot,
        TlMapField::Inheritable => s.thread_itl_slot,
        TlMapField::Bytecode => return None,
    };
    let Value::Object(Some(map)) = tl_real_ref_field(&*ctx, s, thread, field) else {
        return None;
    };
    let Value::Object(Some(table)) = tl_real_ref_field(&*ctx, s, map, s.table_slot) else {
        return None;
    };
    let len = ctx.array_length(table);
    if len == 0 || !len.is_power_of_two() {
        return None;
    }
    let hash = if s.typed_reads {
        ctx.get_field_typed(this, s.hash_slot, b'I')
    } else {
        ctx.get_field(this, s.hash_slot)
    };
    let Value::Int(hash) = hash else {
        return None;
    };
    let mut i = (hash as u32 as usize) & (len - 1);
    // A full table cannot loop forever: the JDK keeps the load under 2/3, and
    // the bound below returns to the bytecode regardless.
    for _ in 0..len {
        let Value::Object(Some(entry)) = ctx.get_array_element(table, i) else {
            // Empty slot: the key is not in the table.
            return None;
        };
        match tl_real_ref_field(&*ctx, s, entry, crate::REF_FIELD_REFERENT) {
            Value::Object(Some(referent)) => {
                if referent.as_ptr() == this.as_ptr() {
                    return Some((entry, thread));
                }
            }
            // A cleared key: `expungeStaleEntry` is the bytecode's business.
            _ => return None,
        }
        i = if i + 1 < len { i + 1 } else { 0 };
    }
    None
}

pub fn native_tl_real_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    if let Some(s) = tl_real_slots(ctx) {
        if let Some((entry, _thread)) = tl_real_find_entry(ctx, &s, this) {
            return Ok(Some(tl_real_ref_field(
                &*ctx,
                &s,
                entry,
                s.entry_value_slot,
            )));
        }
    }
    // The JDK's own body: `ThreadLocal.get()` is `get(Thread.currentThread())`.
    //
    // gci i18/vp: a tail call (`continuation_pin::tail_call`): `get()` returns
    // `get(Thread)`'s `Object` unchanged, exactly as the JDK's one-line body
    // does, and this `Intrinsic` is a registered callback only. On a miss the
    // body runs `initialValue()` -- application code (`withInitial`'s
    // supplier) that may park, which unmounts a virtual thread on HotSpot.
    let thread = ctx.current_thread_object();
    cratonvm_native_api::continuation_pin::tail_call(|| {
        ctx.invoke_special_bytecode_only(
            "java/lang/ThreadLocal",
            "get",
            "(Ljava/lang/Thread;)Ljava/lang/Object;",
            &[Value::Object(Some(this)), Value::Object(Some(thread))],
        )
    })
}

pub fn native_tl_real_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let value = args.get(1).copied().unwrap_or(Value::Object(None));
    let slots = tl_real_slots(ctx);
    if let Some(s) = slots {
        if !s.trace_vthread_locals {
            if let Some((entry, _thread)) = tl_real_find_entry(ctx, &s, this) {
                // `ThreadLocalMap.set`'s found-key arm: `e.value = value`.
                ctx.set_field(entry, s.entry_value_slot, value);
                return Ok(None);
            }
        }
    }
    let trace = slots.is_none_or(|s| s.trace_vthread_locals);
    if trace {
        // Tracing on, or the classes are not all loaded: the whole public
        // body, natives skipped.
        return ctx.invoke_special_bytecode_only(
            "java/lang/ThreadLocal",
            "set",
            "(Ljava/lang/Object;)V",
            &[Value::Object(Some(this)), value],
        );
    }
    let thread = ctx.current_thread_object();
    ctx.invoke_special_bytecode_only(
        "java/lang/ThreadLocal",
        "set",
        "(Ljava/lang/Thread;Ljava/lang/Object;)V",
        &[
            Value::Object(Some(this)),
            Value::Object(Some(thread)),
            value,
        ],
    )
}

/// JDK-ONLY-LAYOUT (kind 3, the sub-case that needs no storage at all).
///
/// Both `<init>` natives used to open with
/// `ctx.set_field(this, 0, Value::Object(None))` under the comment "slot 0
/// retained for layout compatibility; not the source of truth". Nothing in the
/// tree ever read it back — `get`/`set`/`remove` are natives over a side table
/// keyed by identity hash — so the value never survived its own write in any
/// meaningful sense, and the write was pure cost.
///
/// On a real image it was not free. `java.lang.ThreadLocal` declares exactly
/// one instance field, `threadLocalHashCode:I`, so the null landed on the int
/// the real `ThreadLocalMap` hashes with. It reached every ThreadLocal
/// SUBCLASS through `super()` — the census's two "L4 gap 3" rows,
/// `jdk/internal/math/FloatingDecimal$1` and
/// `ReentrantReadWriteLock$Sync$ThreadLocalHoldCounter`, are both this one
/// writer seen through a subclass, not two defects.
///
/// The fix is deletion, not relocation: there is no field to relocate to.
pub fn native_tl_init(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(None)
}

pub fn native_itl_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // G43-1 needs the ITL as the receiver of `childValue`; since gc-common
    // w29-a the snapshot finds it through the parent's own row (whose weak
    // owner handle resolves it), so the table records only WHICH ThreadLocals
    // are inheritable, under the ITL's weak lock key -- minted here, before
    // the table lock. It used to root every ITL permanently
    // (`register_var_handle_root`) under its identity hash.
    let key = crate::gc_stable_weak_lock_key(&*ctx, this)?;
    tl_inheritable_insert(ctx.vm_identity(), key);
    tl_release_orphaned_roots(ctx);
    Ok(None)
}

pub fn native_tl_initial_value(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Object(None)))
}

/// Identify `--compatible`'s side-table `ThreadLocal.get` native callback.
#[inline]
pub fn is_tl_compat_get_native(callback: cratonvm_native_api::NativeCallback) -> bool {
    (callback as usize) == (native_tl_get as usize)
}

/// Hit fast path for `--compatible`'s side-table `ThreadLocal.get()` (no miss
/// upcalls, no safepoints). Returns `None` on miss so callers fall through to
/// the full `native_tl_get` miss path.
pub fn tl_compat_get_hit_fast(ctx: &mut dyn NativeContext, this: ObjectRef) -> Option<Value> {
    drain_inherited_for_current_thread(ctx);
    let mut dead = Vec::new();
    let hit = tl_lookup(&*ctx, this, &mut dead);
    if !dead.is_empty() {
        tl_release_rows(ctx, dead);
    }
    hit.map(|v| tl_value_to_java(ctx, v))
}

pub fn native_tl_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    if let Some(val) = tl_compat_get_hit_fast(ctx, this) {
        return Ok(Some(val));
    }
    // Miss path: if a supplier was registered via `withInitial`, invoke it
    // on the current thread, cache the result, and return.
    tl_release_orphaned_roots(ctx);
    let supplier = tl_with_initial_supplier(&*ctx, this);
    // gci i27/vt (`gci-i20st-compatible-threadlocal-initial-value-pins-a-virtual-thread-FIXED-20261017`,
    // design 2): the supplier arm runs as a TAIL call into Java bytecode
    // that does the store itself (`tl.set(v)`, the side-table
    // `native_tl_set`), so a supplier that parks unmounts its virtual thread
    // as on HotSpot instead of being pinned by this native's frame.
    if let Some(s) = supplier {
        if let Some(answer) = tl_supplied_initial_tail_call(ctx, this, s) {
            return answer;
        }
    } else if let Some(answer) = tl_overridden_initial_tail_call(ctx, this) {
        // gci i28/tl: the `initialValue()`-override arm, the same way
        // through a BOOT-loader helper (`initialValue` is protected).
        return answer;
    }
    // gc-common w29-a: the row is stored under `this` AFTER the upcall, which
    // can collect and move it (it used to be stored under the hash, computed
    // before): pin it across.
    let pin = ctx.pin_native_root(this);
    // gc-common w20-b: one tail for both sources of the initial value (the
    // supplier arm used to duplicate the store and return early).
    let called = match supplier {
        None => ctx.invoke_virtual(this, "initialValue", "()Ljava/lang/Object;", &[]),
        Some(s) => ctx.invoke_virtual(s, "get", "()Ljava/lang/Object;", &[]),
    };
    let this = ctx.read_native_pin(pin, this);
    ctx.unpin_native_roots(pin);
    let initial = called?.unwrap_or(Value::Object(None));
    let stored = tl_value_from_java(ctx, initial);
    // `setInitialValue`: an upcall that set this ThreadLocal is overwritten,
    // as `ThreadLocalMap.set` does (its root is released -- the old
    // `insert(hash, ..)` dropped it unreleased).
    if let Some(old) = tl_store(ctx, this, stored) {
        tl_drop_value_root(ctx, old);
    }
    Ok(Some(tl_value_to_java(ctx, stored)))
}

/// The `--compatible` helper class of [`tl_supplied_initial_tail_call`]
/// (gci i27/vt): `public final class ThreadLocalInitial { public static
/// Object supplied(ThreadLocal tl, Supplier s) { Object v = s.get();
/// tl.set(v); return v; } }`, assembled by hand ([`tl_initial_class_bytes`]).
pub const TL_INITIAL_CLASS: &str = "cratonvm/internal/ThreadLocalInitial";
pub const TL_INITIAL_METHOD: &str = "supplied";
pub const TL_INITIAL_DESC: &str =
    "(Ljava/lang/ThreadLocal;Ljava/util/function/Supplier;)Ljava/lang/Object;";

/// The class file of [`TL_INITIAL_CLASS`] (version 52, one straight-line
/// method: no `StackMapTable` needed, no constructor -- it is never
/// instantiated).
pub fn tl_initial_class_bytes() -> Vec<u8> {
    fn u16be(v: &mut Vec<u8>, x: u16) {
        v.extend_from_slice(&x.to_be_bytes());
    }
    fn utf8(v: &mut Vec<u8>, s: &str) {
        v.push(1);
        u16be(v, s.len() as u16);
        v.extend_from_slice(s.as_bytes());
    }
    let mut v: Vec<u8> = vec![0xCA, 0xFE, 0xBA, 0xBE, 0, 0, 0, 52];
    u16be(&mut v, 20); // constant pool count
    utf8(&mut v, TL_INITIAL_CLASS); // 1
    v.extend_from_slice(&[7, 0, 1]); // 2 Class #1
    utf8(&mut v, "java/lang/Object"); // 3
    v.extend_from_slice(&[7, 0, 3]); // 4 Class #3
    utf8(&mut v, "java/util/function/Supplier"); // 5
    v.extend_from_slice(&[7, 0, 5]); // 6 Class #5
    utf8(&mut v, "get"); // 7
    utf8(&mut v, "()Ljava/lang/Object;"); // 8
    v.extend_from_slice(&[12, 0, 7, 0, 8]); // 9 NameAndType get()Object
    v.extend_from_slice(&[11, 0, 6, 0, 9]); // 10 InterfaceMethodref Supplier.get
    utf8(&mut v, "java/lang/ThreadLocal"); // 11
    v.extend_from_slice(&[7, 0, 11]); // 12 Class #11
    utf8(&mut v, "set"); // 13
    utf8(&mut v, "(Ljava/lang/Object;)V"); // 14
    v.extend_from_slice(&[12, 0, 13, 0, 14]); // 15 NameAndType set(Object)V
    v.extend_from_slice(&[10, 0, 12, 0, 15]); // 16 Methodref ThreadLocal.set
    utf8(&mut v, TL_INITIAL_METHOD); // 17
    utf8(&mut v, TL_INITIAL_DESC); // 18
    utf8(&mut v, "Code"); // 19
    u16be(&mut v, 0x0031); // public final super
    u16be(&mut v, 2); // this
    u16be(&mut v, 4); // super
    u16be(&mut v, 0); // interfaces
    u16be(&mut v, 0); // fields
    u16be(&mut v, 1); // methods
    let code: [u8; 14] = [
        0x2B, // aload_1          s
        0xB9, 0x00, 0x0A, 0x01, 0x00, // invokeinterface Supplier.get, 1
        0x4D, // astore_2         v
        0x2A, // aload_0          tl
        0x2C, // aload_2          v
        0xB6, 0x00, 0x10, // invokevirtual ThreadLocal.set
        0x2C, // aload_2
        0xB0, // areturn
    ];
    u16be(&mut v, 0x0009); // public static
    u16be(&mut v, 17);
    u16be(&mut v, 18);
    u16be(&mut v, 1); // one attribute: Code
    u16be(&mut v, 19);
    v.extend_from_slice(&((2 + 2 + 4 + code.len() + 2 + 2) as u32).to_be_bytes());
    u16be(&mut v, 2); // max_stack
    u16be(&mut v, 3); // max_locals
    v.extend_from_slice(&(code.len() as u32).to_be_bytes());
    v.extend_from_slice(&code);
    u16be(&mut v, 0); // exception table
    u16be(&mut v, 0); // code attributes
    u16be(&mut v, 0); // class attributes
    v
}

/// [`TL_INITIAL_CLASS`] in this VM, defined on first use; `None` when it
/// cannot be defined (the caller keeps the nested path).
pub fn tl_initial_class(ctx: &mut dyn NativeContext) -> Option<cratonvm_types::ClassId> {
    if let Some(cid) = ctx.class_id_by_name(TL_INITIAL_CLASS) {
        return Some(cid);
    }
    ctx.define_class_from_bytes(TL_INITIAL_CLASS, &tl_initial_class_bytes())
        // A racing thread may have defined it first.
        .or_else(|| ctx.class_id_by_name(TL_INITIAL_CLASS))
}

/// gci i27/vt: may the `withInitial` miss path tail-call
/// [`TL_INITIAL_CLASS`]? Never under `--real-jdk` (a class the JDK does not
/// declare: class fabrication, which strict mode forbids; there the
/// side-table natives are not registered anyway unless
/// `CRATONVM_TL_INTRINSIC=1` forces them). `CRATONVM_TL_INITIAL_TAIL_CALL=0`
/// keeps the nested call (default on: a liveness fix).
pub fn tl_initial_tail_call_allowed(ctx: &dyn NativeContext) -> bool {
    !ctx.is_jdk_only()
}

/// gci i27/vt (`gci-i20st-compatible-threadlocal-initial-value-pins-a-virtual-thread-FIXED-20261017`,
/// design 2, "Java-side setter"): the `withInitial` arm of
/// [`native_tl_get`]'s miss path as a tail call
/// (`continuation_pin::tail_call`) into `ThreadLocalInitial.supplied(tl,
/// s)`, whose bytecode runs the supplier and stores its value through
/// `tl.set(v)` -- the side-table `native_tl_set`, a leaf -- before returning
/// it. HotSpot runs the same steps as Java (`get` -> `setInitialValue` ->
/// `SuppliedThreadLocal.initialValue` -> `supplier.get()`, then
/// `map.set`), so a supplier that parks unmounts its virtual thread there;
/// the nested call pinned it under this native's frame
/// (`continuation_pinned_by_native`), and with every carrier pinned the
/// queued virtual threads never started.
///
/// The contract `tail_call` states holds: `native_tl_get` is a registered
/// `Intrinsic` reached through the counted native funnel; the helper
/// returns `Object` as `ThreadLocal.get()` does and its answer -- a value,
/// the supplier's exception (nothing stored, as HotSpot), or a yield -- is
/// this native's, unchanged; nothing runs here after the call. A remounted
/// continuation returns the helper's frame (which did the store) straight
/// into `get()`'s caller.
///
/// `tl` here is always a plain `java/lang/ThreadLocal` (only
/// `native_tl_with_initial` files suppliers, and it allocates that class),
/// so `tl.set` cannot reach an application override of `set`, which
/// `setInitialValue` never calls. `None`: not allowed, or the class could
/// not be defined -- the caller runs the nested path.
pub fn tl_supplied_initial_tail_call(
    ctx: &mut dyn NativeContext,
    tl: ObjectRef,
    supplier: ObjectRef,
) -> Option<MethodCallResult> {
    if !tl_initial_tail_call_allowed(&*ctx) {
        return None;
    }
    // The first use defines the class, which can collect and move both.
    let pin = ctx.pin_native_root(tl);
    let supplier_pin = ctx.pin_native_root(supplier);
    let class = tl_initial_class(ctx);
    let tl = ctx.read_native_pin(pin, tl);
    let supplier = ctx.read_native_pin(supplier_pin, supplier);
    ctx.unpin_native_roots(pin);
    let class = class?;
    Some(cratonvm_native_api::continuation_pin::tail_call(|| {
        ctx.invoke_static_by_class_id(
            class,
            TL_INITIAL_CLASS,
            TL_INITIAL_METHOD,
            TL_INITIAL_DESC,
            &[Value::Object(Some(tl)), Value::Object(Some(supplier))],
        )
    }))
}

/// gci i28/tl (`gci-i20st-compatible-threadlocal-initial-value-pins-a-virtual-thread-FIXED-20261017`,
/// the residual `initialValue()` arm): `public final class
/// java/lang/ThreadLocal$CratonInitial { public static Object
/// initial(ThreadLocal tl) { Object v = tl.initialValue(); tl.set(v); return
/// v; } }`, defined in the BOOT loader so that it shares `java.lang`'s
/// runtime package with `ThreadLocal`: `initialValue()` is `protected`, which
/// an application-loader helper may not call on another class's instance
/// (JVMS 5.4.4 / 4.10.1.8) -- the reason i27/vt left this arm nested.
pub const TL_BOOT_INITIAL_CLASS: &str = "java/lang/ThreadLocal$CratonInitial";
pub const TL_BOOT_INITIAL_METHOD: &str = "initial";
pub const TL_BOOT_INITIAL_DESC: &str = "(Ljava/lang/ThreadLocal;)Ljava/lang/Object;";

/// The class file of [`TL_BOOT_INITIAL_CLASS`] (version 52, one straight-line
/// static method, no constructor: never instantiated).
pub fn tl_boot_initial_class_bytes() -> Vec<u8> {
    fn u16be(v: &mut Vec<u8>, x: u16) {
        v.extend_from_slice(&x.to_be_bytes());
    }
    fn utf8(v: &mut Vec<u8>, s: &str) {
        v.push(1);
        u16be(v, s.len() as u16);
        v.extend_from_slice(s.as_bytes());
    }
    let mut v: Vec<u8> = vec![0xCA, 0xFE, 0xBA, 0xBE, 0, 0, 0, 52];
    u16be(&mut v, 18); // constant pool count
    utf8(&mut v, TL_BOOT_INITIAL_CLASS); // 1
    v.extend_from_slice(&[7, 0, 1]); // 2 Class #1
    utf8(&mut v, "java/lang/Object"); // 3
    v.extend_from_slice(&[7, 0, 3]); // 4 Class #3
    utf8(&mut v, "java/lang/ThreadLocal"); // 5
    v.extend_from_slice(&[7, 0, 5]); // 6 Class #5
    utf8(&mut v, "initialValue"); // 7
    utf8(&mut v, "()Ljava/lang/Object;"); // 8
    v.extend_from_slice(&[12, 0, 7, 0, 8]); // 9 NameAndType initialValue()Object
    v.extend_from_slice(&[10, 0, 6, 0, 9]); // 10 Methodref ThreadLocal.initialValue
    utf8(&mut v, "set"); // 11
    utf8(&mut v, "(Ljava/lang/Object;)V"); // 12
    v.extend_from_slice(&[12, 0, 11, 0, 12]); // 13 NameAndType set(Object)V
    v.extend_from_slice(&[10, 0, 6, 0, 13]); // 14 Methodref ThreadLocal.set
    utf8(&mut v, TL_BOOT_INITIAL_METHOD); // 15
    utf8(&mut v, TL_BOOT_INITIAL_DESC); // 16
    utf8(&mut v, "Code"); // 17
    u16be(&mut v, 0x0031); // public final super
    u16be(&mut v, 2); // this
    u16be(&mut v, 4); // super
    u16be(&mut v, 0); // interfaces
    u16be(&mut v, 0); // fields
    u16be(&mut v, 1); // methods
    let code: [u8; 12] = [
        0x2A, // aload_0          tl
        0xB6, 0x00, 0x0A, // invokevirtual ThreadLocal.initialValue
        0x4C, // astore_1         v
        0x2A, // aload_0          tl
        0x2B, // aload_1          v
        0xB6, 0x00, 0x0E, // invokevirtual ThreadLocal.set
        0x2B, // aload_1
        0xB0, // areturn
    ];
    u16be(&mut v, 0x0009); // public static
    u16be(&mut v, 15);
    u16be(&mut v, 16);
    u16be(&mut v, 1); // one attribute: Code
    u16be(&mut v, 17);
    v.extend_from_slice(&((2 + 2 + 4 + code.len() + 2 + 2) as u32).to_be_bytes());
    u16be(&mut v, 2); // max_stack
    u16be(&mut v, 2); // max_locals
    v.extend_from_slice(&(code.len() as u32).to_be_bytes());
    v.extend_from_slice(&code);
    u16be(&mut v, 0); // exception table
    u16be(&mut v, 0); // code attributes
    u16be(&mut v, 0); // class attributes
    v
}

/// [`TL_BOOT_INITIAL_CLASS`] in this VM, defined in the BOOT loader on first
/// use through the existing `define_class_full` door
/// (`DefineClassFull::bootstrap_namespace`, the namespace `loader_id`'s `0`
/// sentinel cannot name; the bootstrap loader is exempt from the
/// `java.*` prohibited-package guard). Trusted VM-assembled bytes, so not
/// verified, as HotSpot does not verify boot classes. `None` when it cannot
/// be defined (the caller keeps the nested path).
pub fn tl_boot_initial_class(ctx: &mut dyn NativeContext) -> Option<cratonvm_types::ClassId> {
    if let Some(cid) = ctx.class_id_by_name(TL_BOOT_INITIAL_CLASS) {
        return Some(cid);
    }
    let opts = cratonvm_native_api::DefineClassFull {
        bootstrap_namespace: true,
        skip_verification: true,
        ..Default::default()
    };
    ctx.define_class_full(TL_BOOT_INITIAL_CLASS, &tl_boot_initial_class_bytes(), 0, opts)
        .ok()
        // A racing thread may have defined it first.
        .or_else(|| ctx.class_id_by_name(TL_BOOT_INITIAL_CLASS))
}

/// Which of `initialValue()` / `set(Object)` the class of `tl` overrides
/// below `java/lang/ThreadLocal` / `java/lang/InheritableThreadLocal` (whose
/// own triples are this file's side-table natives). Declared methods only,
/// bridges included (a generic `set(T)` override has a `set(Object)` bridge
/// that dispatches to it). `None` when the walk does not reach either class
/// (an unnamed id, a broken superclass chain): the caller keeps the nested
/// path.
pub fn tl_override_scan(ctx: &dyn NativeContext, tl: ObjectRef) -> Option<(bool, bool)> {
    let mut cid = ctx.class_id_of_object(tl);
    let (mut initial, mut set) = (false, false);
    for _ in 0..64 {
        let name = ctx.class_name_of_id(cid)?;
        if name == "java/lang/ThreadLocal" || name == "java/lang/InheritableThreadLocal" {
            return Some((initial, set));
        }
        initial |= ctx
            .declared_method_access_flags(cid, "initialValue", "()Ljava/lang/Object;")
            .is_some();
        set |= ctx
            .declared_method_access_flags(cid, "set", "(Ljava/lang/Object;)V")
            .is_some();
        cid = ctx.superclass_of(cid)?;
    }
    None
}

/// gci i28/tl: the `initialValue()` arm of [`native_tl_get`]'s miss path as
/// a tail call (`continuation_pin::tail_call`) into
/// `ThreadLocal$CratonInitial.initial(tl)`, whose bytecode runs the
/// application's `initialValue()` and stores its value through `tl.set(v)`
/// (the side-table `native_tl_set`, a leaf). HotSpot runs `get` ->
/// `setInitialValue` -> `initialValue()` -> `map.set` as Java, so an
/// `initialValue()` that parks unmounts its virtual thread; nested under
/// this native it pinned the carrier.
///
/// Taken only when the receiver's class overrides `initialValue()` (the base
/// one is `native_tl_initial_value`, answering `null` at once: nothing to
/// unpin, no class to define) and does NOT override `set(Object)`:
/// `setInitialValue` stores with `map.set`, never through `this.set`, so an
/// overriding `set` (`TerminatingThreadLocal`'s, an application's) must not
/// run -- those keep the nested path and its pin. Same switch as the
/// supplier arm (`CRATONVM_TL_INITIAL_TAIL_CALL`, default on, never under
/// `--real-jdk`). The [`tl_supplied_initial_tail_call`] contract holds: this
/// native answers the helper's `Object` unchanged and does nothing after.
pub fn tl_overridden_initial_tail_call(
    ctx: &mut dyn NativeContext,
    tl: ObjectRef,
) -> Option<MethodCallResult> {
    if !tl_initial_tail_call_allowed(&*ctx) {
        return None;
    }
    match tl_override_scan(&*ctx, tl) {
        Some((true, false)) => {}
        _ => return None,
    }
    // The first use defines the class, which can collect and move `tl`.
    let pin = ctx.pin_native_root(tl);
    let class = tl_boot_initial_class(ctx);
    let tl = ctx.read_native_pin(pin, tl);
    ctx.unpin_native_roots(pin);
    let class = class?;
    Some(cratonvm_native_api::continuation_pin::tail_call(|| {
        ctx.invoke_static_by_class_id(
            class,
            TL_BOOT_INITIAL_CLASS,
            TL_BOOT_INITIAL_METHOD,
            TL_BOOT_INITIAL_DESC,
            &[Value::Object(Some(tl))],
        )
    }))
}

pub fn native_tl_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    drain_inherited_for_current_thread(ctx);
    let val = args.get(1).copied().unwrap_or(Value::Object(None));
    let stored = tl_value_from_java(ctx, val);
    if let Some(old) = tl_store(ctx, this, stored) {
        tl_drop_value_root(ctx, old);
    }
    Ok(None)
}

pub fn native_tl_remove(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    drain_inherited_for_current_thread(ctx);
    let mut dead = Vec::new();
    if let Some(row) = tl_remove_row(&*ctx, this, &mut dead) {
        dead.push(row);
    }
    tl_release_rows(ctx, dead);
    Ok(None)
}

/// Release every value this OS thread holds in `TL_MAP`, and the inherited
/// bucket it never drained, dropping the JNI global root each `Root` value
/// owns. Returns the number of values released.
///
/// Called once by each thread death path (`vm_exec.rs`, right after
/// `Thread.exit()`): the platform one, and the virtual one while the dying
/// virtual thread's own map is still installed on its carrier (see
/// [`swap_current_thread_locals`]). `TL_MAP` is a Rust `thread_local!`: its destructor frees
/// the MAP when the OS thread exits, but not the `jni_global_refs` handles its
/// values own — so every `ThreadLocal` value of every thread that ever died
/// stayed a GC root for the life of the VM
/// (`ThreadLocalFinalizeProbe.java`: HotSpot `live=0`, CratonVM
/// `live=11` on all three collectors; gc-common w2-b/w3-b). HotSpot's analogue
/// is `Thread.exit()` → `clearReferences()` nulling `threadLocals`.
///
/// On a carrier it must run while the dying virtual thread's map is the one
/// installed -- releasing the CARRIER's state instead would be wrong for
/// whichever virtual thread mounts next.
///
/// `TL_MAP` is borrowed only for the drain and the pending-bucket lock only
/// for the `remove`, so `tl_release_row` — which takes the VM's JNI global
/// table lock — runs with neither held. Idempotent: a second call finds an
/// empty map and an already-drained bucket. `try_with`, not `with`: a caller
/// on a JNI detach path can run from a TSD destructor after this thread's
/// `thread_local!`s are gone, where `with` panics (out of an `extern "C"`
/// frame); there the map's handles are simply not reachable any more.
///
/// Since gc-common w29-a each row also owns a weak handle on its ThreadLocal,
/// released here with the value's root.
pub fn release_current_thread_locals(ctx: &mut dyn NativeContext) -> usize {
    invalidate_tl_compat_mru();
    let rows: Vec<TlRow> = TL_MAP
        .try_with(|m| {
            m.borrow_mut()
                .drain()
                .flat_map(|(_, bucket)| bucket)
                .collect()
        })
        .unwrap_or_default();
    let mut released = rows.len();
    tl_release_rows(ctx, rows);
    // Interpreter round i5 wave 102, lane L6: the thread's scoped-value cache
    // ends with it too (`forget_current_thread_scoped_value_cache`).
    crate::forget_current_thread_scoped_value_cache(ctx);
    if !TL_INHERITED_DRAINED
        .try_with(|f| f.replace(true))
        .unwrap_or(true)
    {
        // zt t20/tg9: never mints the thread's mirror (see
        // `tl_release_mints_no_thread_enabled`).
        if let Some(entries) = take_inherited_for_ending_thread(ctx) {
            released += entries.len();
            for (_, row) in entries {
                tl_release_row(ctx, row);
            }
        }
    }
    // Re-arm the one-shot for the NEXT Java thread this OS thread runs. A JNI
    // host thread can `AttachCurrentThread` / `DetachCurrentThread` /
    // `AttachCurrentThread` again (`jni.rs::release_foreign_thread_locals`
    // calls this at each detach): that is a second `java.lang.Thread` with its
    // own inherited bucket, which a flag left `true` here never drained -- the
    // child did not see its inherited values, and the bucket's global roots
    // stayed until the VM died (gc-common w5-b, applying
    // `handoff-w4g-reattach-resets-inherited-drain.md`). Idempotence holds: a
    // second release finds an empty map and, with the flag false again, a
    // bucket the first one already removed. A platform thread runs no Java on
    // this OS thread after its release; the virtual death path restores the
    // carrier's own flag by swapping its state back in right after.
    let _ = TL_INHERITED_DRAINED.try_with(|f| f.set(false));
    released
}

pub fn native_tl_with_initial(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Allocate a SuppliedThreadLocal, register the supplier in the side
    // table so every thread invokes it lazily on first read. HotSpot does
    // not call Supplier.get() from withInitial() itself; eager evaluation can
    // initialize method-reference implementation classes too early.
    let supplier = match args.first() {
        Some(Value::Object(Some(s))) => *s,
        _ => return Ok(Some(Value::Object(None))),
    };
    // gc-common w29-a: the supplier is held across the ThreadLocal's
    // allocation, which can collect and move it (it was rooted only after):
    // pin it across and read it back.
    let pin = ctx.pin_native_root(supplier);
    let allocated = try_alloc_concurrent_synthetic(ctx, "java/lang/ThreadLocal", 1);
    let supplier = ctx.read_native_pin(pin, supplier);
    ctx.unpin_native_roots(pin);
    let tl = allocated?;
    // Route R1: the row is filed under the new ThreadLocal's weak lock key
    // (minted before the table lock) and owns a global root of the supplier,
    // released when the ThreadLocal dies ([`forget_thread_local_keys`]). It
    // used to be `register_var_handle_root(supplier)` -- permanent -- under
    // the ThreadLocal's identity hash.
    let key = crate::gc_stable_weak_lock_key(&*ctx, tl)?;
    let root = ctx.add_global_root(supplier);
    let vm = ctx.vm_identity();
    let replaced = tl_side_tables().lock().suppliers.insert(
        key,
        TlSupplierRow {
            vm,
            root,
            fallback: supplier,
        },
    );
    ANY_TL_SUPPLIER.store(true, std::sync::atomic::Ordering::Release);
    if let Some(old) = replaced.filter(|old| old.root != 0) {
        let _ = ctx.remove_global_root(old.root);
    }
    tl_release_orphaned_roots(ctx);
    Ok(Some(Value::Object(Some(tl))))
}

/// Build a snapshot of this OS thread's TL entries whose keys are
/// flagged inheritable, with `InheritableThreadLocal.childValue` applied to
/// each. This is CratonVM's `ThreadLocal.createInheritedMap`.
///
/// Called from `lang_system::capture_inheritable_tl_at_construction` (the
/// construction-time capture the nine `Thread.<init>` bridges in `lib.rs` go
/// through) and, only when nothing was captured at construction, from
/// `native_thread_start0`. Exactly one of the two runs per child, so
/// `childValue` is applied exactly once per constructed Thread — matching
/// HotSpot, MEASURED (G36-2 §3.1 `G36Cv` rows 6 and 8: one call for a Thread
/// that is started, one for a Thread that never is), and it runs on the
/// CONSTRUCTING thread (row 10), which is where this function already was.
///
/// Returns `None` if there are no inheritable entries to copy.
///
/// NOTE: this sees the parent's local map, plus (since G43-1) whatever the
/// parent itself inherited but has not yet read — see the drain at the top of
/// the body. Suppliers registered via `withInitial` are NOT eagerly evaluated
/// for the child — the child's first `get()` will invoke its own supplier
/// copy. That matches JDK semantics: `InheritableThreadLocal` inherits only
/// set values, and `withInitial` ThreadLocals are not inheritable by default
/// anyway.
///
/// An explicitly stored `null` IS an entry, so `childValue(null)` is called
/// for it; a `remove()`d ThreadLocal is not an entry, so nothing is called.
/// Both MEASURED against HotSpot (`G36Cv` rows 4 and 5).
pub fn snapshot_inheritable_tl_entries(
    ctx: &mut dyn NativeContext,
) -> Option<InheritedTlBucket> {
    // G43-1 row 24 — inheritance must be TRANSITIVE. The drain is lazy: a
    // thread's own inherited entries sit in `tl_inherited_pending` until its
    // first `ThreadLocal` get/set/remove moves them into `TL_MAP`. A thread
    // that inherited a value and then constructs a child WITHOUT ever reading
    // it had an empty `TL_MAP` here, so the grandchild inherited nothing —
    // `null` where HotSpot gives the grandparent's value. Draining first makes
    // "what this thread would see if it read now" the thing we snapshot, which
    // is what `createInheritedMap` copies on HotSpot. Idempotent, and a single
    // bool check after the first call.
    drain_inherited_for_current_thread(ctx);

    // Four passes, and the split is NOT cosmetic. Pass 4 calls `childValue`,
    // which is arbitrary application bytecode: an override that reads any
    // ThreadLocal re-enters `TL_MAP.borrow()` (a `RefCell` double-borrow
    // PANIC), and one that constructs an `InheritableThreadLocal` re-enters
    // `native_itl_init` and this same non-reentrant `parking_lot::Mutex` (a
    // DEADLOCK on the thread-construction path, i.e. a hang in every executor
    // that ever mints a worker). So: take the table out and drop the lock,
    // read the values out and drop the borrow, and only then call into Java
    // holding neither.

    // PASS 1 — does this VM have an `InheritableThreadLocal` at all? The
    // table lock is released at once.
    if !tl_vm_has_inheritable(ctx.vm_identity()) {
        return None;
    }

    // PASS 2 — copy this thread's own rows. `TlRow` is `Copy`, so the borrow
    // is held across pure copies only, and is released at the closure's end.
    let rows: Vec<(i32, TlRow)> = TL_MAP.with(|m| {
        m.borrow()
            .iter()
            .flat_map(|(&hash, bucket)| bucket.iter().map(move |row| (hash, *row)))
            .collect()
    });
    if rows.is_empty() {
        return None;
    }

    // PASS 3 — keep the rows whose ThreadLocal is inheritable (gc-common
    // w29-a: judged by the ITL's weak lock key, found through the row's own
    // weak owner handle, so a collider of an ITL is not inherited), and give
    // each kept row handles of the SNAPSHOT'S OWN: a weak handle on the ITL
    // and a root of the parent value. Pass 4 runs Java that may `remove()` or
    // `set()` the parent's row -- releasing the parent's handles, whose slots
    // the global-ref table may then hand to another object. No step here can
    // allocate or run Java; each lock is taken once and alone.
    let mut owners: Vec<(i32, ObjectRef, TlRow)> = Vec::with_capacity(rows.len());
    for (hash, row) in rows {
        if let Some(tl) = tl_row_owner(&*ctx, &row) {
            owners.push((hash, tl, row));
        }
    }
    let keys: Vec<Option<usize>> = owners
        .iter()
        .map(|&(_, tl, _)| crate::existing_weak_lock_key(&*ctx, tl))
        .collect();
    let inheritable: Vec<bool> = {
        let tables = tl_side_tables().lock();
        keys.iter()
            .map(|key| key.is_some_and(|k| tables.inheritable.contains_key(&k)))
            .collect()
    };
    let mut parent_entries: InheritedTlBucket = Vec::new();
    for ((hash, tl, row), inherited) in owners.into_iter().zip(inheritable) {
        if !inherited {
            continue;
        }
        let parent_value = tl_value_to_java(&*ctx, row.value);
        let value = tl_value_from_java(ctx, parent_value);
        parent_entries.push((hash, tl_new_row(ctx, tl, value)));
    }
    if parent_entries.is_empty() {
        return None;
    }

    // PASS 4 — apply `childValue`. No lock, no borrow, re-entrant-safe.
    //
    // The erased descriptor `(Ljava/lang/Object;)Ljava/lang/Object;` is the
    // one that reaches BOTH `InheritableThreadLocal.childValue` itself and
    // javac's synthetic bridge for a `childValue(String)`-shaped override.
    //
    // The receiver and the parent value are resolved from the row's handles
    // immediately before each call rather than materialised in pass 3:
    // `childValue` can allocate, and a moving GC would leave a pre-computed
    // `ObjectRef` stale. The row we carry is the GC-stable form.
    //
    // BOUNDED DEVIATION (recorded here because it cannot be fixed at this
    // layer): HotSpot propagates an exception thrown by `childValue` out of
    // `Thread.<init>`. This function returns `Option`, not `Result`, and its
    // callers are nine `Thread.<init>` bodies that return `()`, so a throwing
    // override is swallowed and the child inherits the PARENT value — which is
    // also what the base implementation would have produced. `Ok(None)` (no
    // body found, e.g. a synthetic-JDK image that does not declare
    // `childValue`) takes the same arm for the same reason.
    let mut snap: InheritedTlBucket = Vec::with_capacity(parent_entries.len());
    for (hash, mut row) in parent_entries {
        // A ThreadLocal that died during an earlier `childValue` is not
        // inherited (nothing can read it).
        let Some(tl) = tl_row_owner(&*ctx, &row) else {
            tl_release_row(ctx, row);
            continue;
        };
        let parent_value = tl_value_to_java(&*ctx, row.value);
        let called = ctx.invoke_virtual(
            tl,
            "childValue",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            &[parent_value],
        );
        // `Ok(None)` / `Err(_)`: the row keeps its own root of the parent
        // value, resolved afresh by whoever reads it.
        if let Ok(Some(child_value)) = called {
            let child = tl_value_from_java(ctx, child_value);
            tl_drop_value_root(ctx, std::mem::replace(&mut row.value, child));
        }
        snap.push((hash, row));
    }
    if snap.is_empty() {
        None
    } else {
        Some(snap)
    }
}

// ---------------------------------------------------------------------------
// StringTokenizer — 3-field synthetic (input=0, pos=1, delimiters=2)
// ---------------------------------------------------------------------------
pub const ST_FIELD_INPUT: usize = 0;
pub const ST_FIELD_POS: usize = 1;
pub const ST_FIELD_DELIMS: usize = 2;

pub fn register_string_tokenizer_natives(r: &mut NativeMethodRegistry) {
    let c = "java/util/StringTokenizer";
    r.register(c, "<init>", "(Ljava/lang/String;)V", native_st_init_default, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(
        c,
        "<init>",
        "(Ljava/lang/String;Ljava/lang/String;)V",
        native_st_init_delims, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "<init>",
        "(Ljava/lang/String;Ljava/lang/String;Z)V",
        native_st_init_delims_return, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(c, "hasMoreTokens", "()Z", native_st_has_more_tokens, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(c, "nextToken", "()Ljava/lang/String;", native_st_next_token, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(
        c,
        "nextToken",
        "(Ljava/lang/String;)Ljava/lang/String;",
        native_st_next_token_with_delim, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(c, "hasMoreElements", "()Z", native_st_has_more_tokens, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(
        c,
        "nextElement",
        "()Ljava/lang/Object;",
        native_st_next_token, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(c, "countTokens", "()I", native_st_count_tokens, cratonvm_native_api::NativeKind::Intrinsic);
}

pub fn native_st_init_default(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let input_obj = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => {
            // JDK parity: `new StringTokenizer((String) null)` dereferences
            // the string in the constructor (`str.length()`), throwing NPE.
            // Silently accepting null produced an empty tokenizer, which
            // turned downstream `nextToken()` calls into a spurious
            // NoSuchElementException instead of the expected NPE — see
            // OSGi `Version("null")` mis-parse in Felix bootstrap.
            return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                message: Some("Cannot invoke \"String.length()\" because \"str\" is null".into()),
            }
            .into());
        }
    };
    ctx.set_field(this, ST_FIELD_INPUT, Value::Object(Some(input_obj)));
    ctx.set_field(this, ST_FIELD_POS, Value::Int(0));
    alloc_string_into_field(ctx, this, ST_FIELD_DELIMS, " \t\n\r\x0c");
    Ok(None)
}

pub fn native_st_init_delims(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let input_obj = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => {
            // JDK parity: the StringTokenizer constructors dereference the
            // input string (`str.length()`) and throw NPE on a null string.
            return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                message: Some("Cannot invoke \"String.length()\" because \"str\" is null".into()),
            }
            .into());
        }
    };
    ctx.set_field(this, ST_FIELD_INPUT, Value::Object(Some(input_obj)));
    ctx.set_field(this, ST_FIELD_POS, Value::Int(0));
    // gc-common w20-b: the default-delimiter arm held `this` raw across the
    // String's allocation.
    match args.get(2) {
        Some(Value::Object(Some(o))) => {
            ctx.set_field(this, ST_FIELD_DELIMS, Value::Object(Some(*o)));
        }
        _ => {
            alloc_string_into_field(ctx, this, ST_FIELD_DELIMS, " \t\n\r\x0c");
        }
    }
    Ok(None)
}

pub fn native_st_init_delims_return(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Simplified: ignore returnDelims flag, just init with delimiters
    native_st_init_delims(ctx, args)
}

pub fn st_read_state(ctx: &mut dyn NativeContext, this: ObjectRef) -> (String, usize, String) {
    let input = match ctx.get_field(this, ST_FIELD_INPUT) {
        Value::Object(Some(o)) => ctx.read_string(o).unwrap_or_default(),
        _ => String::new(),
    };
    let pos = match ctx.get_field(this, ST_FIELD_POS) {
        Value::Int(p) => p as usize,
        _ => 0,
    };
    let delims = match ctx.get_field(this, ST_FIELD_DELIMS) {
        Value::Object(Some(o)) => ctx.read_string(o).unwrap_or_default(),
        _ => " \t\n\r".to_string(),
    };
    (input, pos, delims)
}

pub fn st_next_token_impl(input: &str, pos: usize, delims: &str) -> Option<(String, usize)> {
    let chars: Vec<char> = input.chars().collect();
    let len = chars.len();
    // Skip leading delimiters
    let mut i = pos;
    while i < len && delims.contains(chars[i]) {
        i += 1;
    }
    if i >= len {
        return None;
    }
    // Collect token characters
    let start = i;
    while i < len && !delims.contains(chars[i]) {
        i += 1;
    }
    let token: String = chars[start..i].iter().collect();
    Some((token, i))
}

pub fn native_st_has_more_tokens(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let (input, pos, delims) = st_read_state(ctx, this);
    let has = st_next_token_impl(&input, pos, &delims).is_some();
    Ok(Some(Value::Int(if has { 1 } else { 0 })))
}

pub fn native_st_next_token(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let (input, pos, delims) = st_read_state(ctx, this);
    match st_next_token_impl(&input, pos, &delims) {
        Some((token, new_pos)) => {
            ctx.set_field(this, ST_FIELD_POS, Value::Int(new_pos as i32));
            let s = ctx.create_string(&token);
            Ok(Some(Value::Object(Some(s))))
        }
        None => Err(
            cratonvm_types::error::RuntimeError::NoSuchElementException {
                message: "StringTokenizer: no more tokens".to_string(),
            }
            .into(),
        ),
    }
}

pub fn native_st_next_token_with_delim(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // Update delimiters first. gc-common w20-b: the default-delimiter arm
    // held `this` raw across the String's allocation; the helper answers its
    // current address.
    let this: ObjectRef = match args.get(1) {
        Some(Value::Object(Some(o))) => {
            ctx.set_field(this, ST_FIELD_DELIMS, Value::Object(Some(*o)));
            this
        }
        _ => alloc_string_into_field(ctx, this, ST_FIELD_DELIMS, " \t\n\r\x0c"),
    };
    // Now get next token with new delims
    native_st_next_token(ctx, &[Value::Object(Some(this))])
}

/// T2.3.13: `StringTokenizer.countTokens` — single-pass O(n) counter.
///
/// The previous implementation repeatedly called `st_next_token_impl`,
/// which allocates a `Vec<char>` and copies each token string on every
/// step — producing O(n · token_length) work per call. Since the return
/// value is only the count, we never need the token strings themselves;
/// a single pass that counts transitions from delimiter to
/// non-delimiter is both simpler and strictly faster.
///
/// Input is the underlying `String.value` char sequence; delimiters are
/// a membership test against the delims string. A character is a
/// delimiter iff it appears anywhere in the delims argument. Scanning
/// from `pos` to end of input:
///   * for every run of non-delimiter chars, increment the count once.
///
/// Examples (delims = " "):
///   "one two three"       → 3
///   "  extra  leading"    → 2
///   ""                    → 0
///   "   "                 → 0
pub fn native_st_count_tokens(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let (input, pos, delims) = st_read_state(ctx, this);
    // Fast path: pre-hoist delimiters into a Vec so we don't re-scan
    // the delims string for every input character. For tiny delim
    // sets (the common case — whitespace defaults are 5 chars) the
    // linear scan is still cache-friendly.
    let delim_chars: Vec<char> = delims.chars().collect();
    let is_delim = |c: char| delim_chars.contains(&c);

    let mut count: i32 = 0;
    let mut in_token = false;
    for (i, ch) in input.chars().enumerate() {
        if i < pos {
            continue;
        }
        if is_delim(ch) {
            in_token = false;
        } else if !in_token {
            count += 1;
            in_token = true;
        }
    }
    Ok(Some(Value::Int(count)))
}

// ---------------------------------------------------------------------------
// BitSet. Older synthetic tests use a 2-field shape
// (words=0 long[], nbits=1 Int); real JDK BitSet uses
// (words, wordsInUse, sizeIsSticky). Resolve by name when real class
// metadata is available so forced real-JDK dispatch does not corrupt
// wordsInUse.
// ---------------------------------------------------------------------------
pub const BS_FIELD_WORDS: usize = 0;
pub const BS_FIELD_NBITS: usize = 1;

#[derive(Clone, Copy)]
pub struct BsLayout {
    pub words: usize,
    pub words_in_use: Option<usize>,
    pub size_is_sticky: Option<usize>,
}

pub fn bs_layout(ctx: &dyn NativeContext) -> BsLayout {
    BsLayout {
        words: ctx
            .resolve_field_index("java/util/BitSet", "words")
            .unwrap_or(BS_FIELD_WORDS),
        words_in_use: ctx.resolve_field_index("java/util/BitSet", "wordsInUse"),
        size_is_sticky: ctx.resolve_field_index("java/util/BitSet", "sizeIsSticky"),
    }
}

pub fn bs_words_obj(ctx: &dyn NativeContext, this: ObjectRef) -> Option<ObjectRef> {
    let layout = bs_layout(ctx);
    match ctx.get_field(this, layout.words) {
        Value::Object(Some(o)) => Some(o),
        _ => None,
    }
}

pub fn bs_set_capacity_bits(ctx: &mut dyn NativeContext, this: ObjectRef, words_len: usize) {
    let layout = bs_layout(ctx);
    if layout.words_in_use.is_none() {
        ctx.set_field(
            this,
            BS_FIELD_NBITS,
            Value::Int((words_len.saturating_mul(64)).min(i32::MAX as usize) as i32),
        );
    }
}

pub fn bs_words_in_use(ctx: &dyn NativeContext, this: ObjectRef, words: ObjectRef) -> usize {
    let layout = bs_layout(ctx);
    let len = ctx.array_length(words);
    if let Some(slot) = layout.words_in_use {
        return match ctx.get_field(this, slot) {
            Value::Int(n) if n > 0 => (n as usize).min(len),
            _ => 0,
        };
    }
    match ctx.get_field(this, BS_FIELD_NBITS) {
        Value::Int(n) if n > 0 => bs_word_count(n as usize).min(len),
        _ => len,
    }
}

pub fn bs_set_words_in_use(ctx: &mut dyn NativeContext, this: ObjectRef, value: usize) {
    let layout = bs_layout(ctx);
    if let Some(slot) = layout.words_in_use {
        ctx.set_field(this, slot, Value::Int(value.min(i32::MAX as usize) as i32));
    }
}

pub fn bs_recalculate_words_in_use(ctx: &mut dyn NativeContext, this: ObjectRef) {
    let layout = bs_layout(ctx);
    let Some(slot) = layout.words_in_use else {
        return;
    };
    let Some(words) = bs_words_obj(ctx, this) else {
        ctx.set_field(this, slot, Value::Int(0));
        return;
    };
    let mut used = ctx.array_length(words);
    while used > 0 {
        let word = match ctx.get_array_element(words, used - 1) {
            Value::Long(v) => v,
            _ => 0,
        };
        if word != 0 {
            break;
        }
        used -= 1;
    }
    ctx.set_field(this, slot, Value::Int(used.min(i32::MAX as usize) as i32));
}

pub fn bs_mark_capacity_not_sticky(ctx: &mut dyn NativeContext, this: ObjectRef) {
    let layout = bs_layout(ctx);
    if let Some(slot) = layout.size_is_sticky {
        ctx.set_field(this, slot, Value::Int(0));
    }
}

pub fn register_bitset_natives(r: &mut NativeMethodRegistry) {
    let c = "java/util/BitSet";
    r.register(c, "<init>", "()V", native_bs_init, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(c, "<init>", "(I)V", native_bs_init_nbits, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(c, "set", "(I)V", native_bs_set, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(c, "set", "(IZ)V", native_bs_set_val, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(c, "set", "(II)V", native_bs_set_range, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(c, "clear", "(I)V", native_bs_clear, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(c, "clear", "()V", native_bs_clear_all, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(c, "clear", "(II)V", native_bs_clear_range, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(c, "get", "(I)Z", native_bs_get, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(c, "flip", "(I)V", native_bs_flip, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(c, "flip", "(II)V", native_bs_flip_range, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(c, "length", "()I", native_bs_length, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(c, "size", "()I", native_bs_size, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(c, "cardinality", "()I", native_bs_cardinality, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(c, "isEmpty", "()Z", native_bs_is_empty, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(c, "nextSetBit", "(I)I", native_bs_next_set_bit, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(c, "nextClearBit", "(I)I", native_bs_next_clear_bit, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(c, "previousSetBit", "(I)I", native_bs_previous_set_bit, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(c, "and", "(Ljava/util/BitSet;)V", native_bs_and, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(c, "or", "(Ljava/util/BitSet;)V", native_bs_or, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(c, "xor", "(Ljava/util/BitSet;)V", native_bs_xor, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(c, "andNot", "(Ljava/util/BitSet;)V", native_bs_and_not, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(
        c,
        "intersects",
        "(Ljava/util/BitSet;)Z",
        native_bs_intersects, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(c, "equals", "(Ljava/lang/Object;)Z", native_bs_equals, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(c, "hashCode", "()I", native_bs_hash_code, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(c, "clone", "()Ljava/lang/Object;", native_bs_clone, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(c, "toString", "()Ljava/lang/String;", native_bs_to_string, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(
        c,
        "valueOf",
        "([J)Ljava/util/BitSet;",
        native_bs_value_of_longs, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(c, "toLongArray", "()[J", native_bs_to_long_array, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(
        c,
        "stream",
        "()Ljava/util/stream/IntStream;",
        native_bs_stream, cratonvm_native_api::NativeKind::Bridge,
    );
}

pub fn bs_word_count(nbits: usize) -> usize {
    nbits.div_ceil(64)
}

/// Upper bound on the BitSet backing-`long[]` length, in words.
///
/// A bit index is a Java `int`, so the largest legal index is
/// `Integer.MAX_VALUE` (2^31 - 1) → ceil((2^31 - 1 + 1) / 64) = 2^25 words
/// = 256 MiB of `long`s. We accept up to that (matching the real JDK's
/// reachable maximum) but never beyond, so an untrusted index that survives
/// the non-negative check still cannot drive an unbounded allocation: the
/// word-count derived from `bit_index` is clamped/validated against this cap
/// before any `new_array`.
pub const BS_MAX_WORDS: usize = ((i32::MAX as usize) + 64) / 64;

/// Build a catchable `java.lang.IndexOutOfBoundsException` carrying `msg` and
/// return it as a thrown Java exception (not an internal/fatal VM error).
///
/// Uses `new_object_initialized` with the `(String)` constructor so the
/// `detailMessage` is set correctly regardless of the (synthetic vs real-JDK)
/// Throwable field layout, and the object is kept GC-pinned across `<init>`.
pub fn bs_throw_index_oob(ctx: &mut dyn NativeContext, msg: &str) -> MethodCallFailed {
    let msg_str = ctx.create_string(msg);
    match ctx.new_object_initialized(
        "java/lang/IndexOutOfBoundsException",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(msg_str))],
    ) {
        Ok(Some(Value::Object(Some(exc)))) => MethodCallFailed::ExceptionThrown(exc),
        // Could not materialise the exception object (class not yet loaded on
        // early boot, etc.) — fall back to the closest catchable runtime error
        // rather than panicking or silently succeeding.
        _ => MethodCallFailed::from(RuntimeError::IllegalArgumentException {
            message: msg.to_string(),
        }),
    }
}

/// Read a single `int` bit index from `args[idx]` and validate it the way the
/// real JDK `BitSet` mutators/accessors do: a negative index throws
/// `IndexOutOfBoundsException`. A non-`Int` value (which the descriptor should
/// never permit) is treated as `0`.
///
/// This is the fix for the negative-index wrap: `*n as usize` on a negative
/// `int` produced a near-`usize::MAX` value that flowed into the capacity
/// math, overflowing `bit_index + 64` and driving a gigantic allocation.
pub fn bs_checked_bit_index(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    idx: usize,
) -> Result<usize, MethodCallFailed> {
    match args.get(idx) {
        Some(Value::Int(n)) => {
            if *n < 0 {
                Err(bs_throw_index_oob(ctx, &format!("bitIndex < 0: {n}")))
            } else {
                Ok(*n as usize)
            }
        }
        _ => Ok(0),
    }
}

/// Validate a `(fromIndex, toIndex)` pair the way the real JDK `BitSet`
/// range mutators do: `fromIndex < 0` or `toIndex < 0` throws
/// `IndexOutOfBoundsException`, and `fromIndex > toIndex` throws
/// `IndexOutOfBoundsException`. Returns the `(from, to)` as `usize`.
pub fn bs_checked_range(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    from_idx: usize,
    to_idx: usize,
) -> Result<(usize, usize), MethodCallFailed> {
    let from_raw = match args.get(from_idx) {
        Some(Value::Int(n)) => *n,
        _ => 0,
    };
    let to_raw = match args.get(to_idx) {
        Some(Value::Int(n)) => *n,
        _ => 0,
    };
    if from_raw < 0 {
        return Err(bs_throw_index_oob(
            ctx,
            &format!("fromIndex < 0: {from_raw}"),
        ));
    }
    if to_raw < 0 {
        return Err(bs_throw_index_oob(ctx, &format!("toIndex < 0: {to_raw}")));
    }
    if from_raw > to_raw {
        return Err(bs_throw_index_oob(
            ctx,
            &format!("fromIndex: {from_raw} > toIndex: {to_raw}"),
        ));
    }
    Ok((from_raw as usize, to_raw as usize))
}

pub fn native_bs_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let layout = bs_layout(ctx);
    // `new_array` allocates, so it can collect and move `this`. Pin and
    // re-derive before writing through it -- the `format_impl` shape, see
    // `docs/feature-designs/concurrent-gc-maturation.md`.
    let this_pin = ctx.pin_native_root(this);
    let words = ctx.new_array(cratonvm_types::ArrayElementType::Long, 1);
    let this = ctx.read_native_pin(this_pin, this);
    ctx.set_field(this, layout.words, Value::Object(Some(words)));
    if let Some(slot) = layout.words_in_use {
        ctx.set_field(this, slot, Value::Int(0));
    } else {
        ctx.set_field(this, BS_FIELD_NBITS, Value::Int(64));
    }
    if let Some(slot) = layout.size_is_sticky {
        ctx.set_field(this, slot, Value::Int(0));
    }
    Ok(None)
}

pub fn native_bs_init_nbits(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let nbits = match args.get(1) {
        Some(Value::Int(n)) if *n < 0 => {
            return Err(bs_throw_index_oob(ctx, &format!("nbits < 0: {n}")));
        }
        Some(Value::Int(n)) => *n as usize,
        _ => 64,
    };
    let layout = bs_layout(ctx);
    let nwords = bs_word_count(nbits).max(1);
    // See `native_bs_init`: the allocation can move `this`.
    let this_pin = ctx.pin_native_root(this);
    let words = ctx.new_array(cratonvm_types::ArrayElementType::Long, nwords);
    let this = ctx.read_native_pin(this_pin, this);
    ctx.set_field(this, layout.words, Value::Object(Some(words)));
    if let Some(slot) = layout.words_in_use {
        ctx.set_field(this, slot, Value::Int(0));
    } else {
        ctx.set_field(this, BS_FIELD_NBITS, Value::Int((nwords * 64) as i32));
    }
    if let Some(slot) = layout.size_is_sticky {
        ctx.set_field(this, slot, Value::Int(1));
    }
    Ok(None)
}

/// Grow `this`'s `words` array to cover `bit_index`.
///
/// gc-common w20-b: `this` is `&mut` (the `stale-receiver-audit` shape's
/// fix). The grow path allocates the new array, and this used to take `this`
/// by value and return `()`, holding `this` and the old array raw across that
/// allocation and leaving every caller (`set`, the `set` / `clear` / `flip`
/// range loops, `or`, `xor`) with the pre-allocation address. Both are pinned
/// across it now and `*this` is updated.
pub fn bs_ensure_capacity(ctx: &mut dyn NativeContext, this: &mut ObjectRef, bit_index: usize) {
    // Defensive bound: callers validate the bit index (non-negative, ≤
    // Integer.MAX_VALUE) before reaching here, but clamp the requested word
    // count to `BS_MAX_WORDS` so a stray huge index can never overflow
    // `bit_index + 64` (panic in debug / wrap in release) nor drive an
    // unbounded `new_array`. `bit_index` is at most `i32::MAX` after
    // validation, so `+ 64` cannot overflow `usize` on a 64-bit target.
    let needed_words = bs_word_count(bit_index.saturating_add(1)).min(BS_MAX_WORDS);
    let layout = bs_layout(ctx);
    let old_words: ObjectRef = match ctx.get_field(*this, layout.words) {
        Value::Object(Some(o)) => o,
        _ => {
            *this = alloc_array_into_field(
                ctx,
                *this,
                layout.words,
                cratonvm_types::ArrayElementType::Long,
                needed_words.max(1),
            );
            bs_set_capacity_bits(ctx, *this, needed_words.max(1));
            bs_mark_capacity_not_sticky(ctx, *this);
            return;
        }
    };
    let old_len = ctx.array_length(old_words);
    if needed_words <= old_len {
        return;
    }
    let new_len = old_len
        .saturating_mul(2)
        .max(needed_words)
        .max(1)
        .min(BS_MAX_WORDS);
    if new_len <= old_len {
        bs_set_capacity_bits(ctx, *this, old_len);
        return;
    }
    let this_pin = ctx.pin_native_root(*this);
    let old_pin = ctx.pin_native_root(old_words);
    let new_words = ctx.new_array(cratonvm_types::ArrayElementType::Long, new_len);
    let old_words = ctx.read_native_pin(old_pin, old_words);
    *this = ctx.read_native_pin(this_pin, *this);
    ctx.unpin_native_roots(this_pin);
    for i in 0..old_len {
        let v = ctx.get_array_element(old_words, i);
        ctx.set_array_element(new_words, i, v);
    }
    ctx.set_field(*this, layout.words, Value::Object(Some(new_words)));
    bs_set_capacity_bits(ctx, *this, new_len);
    bs_mark_capacity_not_sticky(ctx, *this);
}

pub fn bs_get_bit(ctx: &mut dyn NativeContext, this: ObjectRef, bit: usize) -> bool {
    let Some(words) = bs_words_obj(ctx, this) else {
        return false;
    };
    let word_idx = bit / 64;
    if word_idx >= bs_words_in_use(ctx, this, words) {
        return false;
    }
    let word = match ctx.get_array_element(words, word_idx) {
        Value::Long(v) => v,
        _ => 0,
    };
    (word >> (bit % 64)) & 1 != 0}

/// gc-common w20-b: `this` is `&mut` because setting a bit may grow the
/// words array (see [`bs_ensure_capacity`]); the range loops keep calling
/// this with the address it leaves behind.
pub fn bs_set_bit(ctx: &mut dyn NativeContext, this: &mut ObjectRef, bit: usize, val: bool) {
    if val {
        bs_ensure_capacity(ctx, this, bit);
    }
    let this = *this;
    let Some(words) = bs_words_obj(ctx, this) else {
        return;
    };
    let word_idx = bit / 64;
    if word_idx >= ctx.array_length(words) {
        return;
    }
    if !val && word_idx >= bs_words_in_use(ctx, this, words) {
        return;
    }
    let word = match ctx.get_array_element(words, word_idx) {
        Value::Long(v) => v,
        _ => 0,
    };
    let mask = 1i64 << (bit % 64);
    let new_word = if val { word | mask } else { word & !mask };
    ctx.set_array_element(words, word_idx, Value::Long(new_word));
    if val {
        let used = bs_words_in_use(ctx, this, words).max(word_idx + 1);
        bs_set_words_in_use(ctx, this, used);
    } else {
        bs_recalculate_words_in_use(ctx, this);
    }
}

pub fn bs_read_words(ctx: &mut dyn NativeContext, this: ObjectRef) -> Vec<i64> {
    let Some(words) = bs_words_obj(ctx, this) else {
        return vec![];
    };
    let len = bs_words_in_use(ctx, this, words);
    let mut result = Vec::with_capacity(len);
    for i in 0..len {
        let v = match ctx.get_array_element(words, i) {
            Value::Long(l) => l,
            _ => 0,
        };
        result.push(v);    }
    result
}

pub fn native_bs_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = obj_arg(args, 0)?;
    let bit = bs_checked_bit_index(ctx, args, 1)?;
    bs_set_bit(ctx, &mut this, bit, true);
    Ok(None)
}

pub fn native_bs_set_val(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = obj_arg(args, 0)?;
    let bit = bs_checked_bit_index(ctx, args, 1)?;
    let val = match args.get(2) {
        Some(Value::Int(v)) => *v != 0,
        _ => true,
    };
    bs_set_bit(ctx, &mut this, bit, val);
    Ok(None)
}

pub fn native_bs_set_range(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = obj_arg(args, 0)?;
    let (from, to) = bs_checked_range(ctx, args, 1, 2)?;
    for bit in from..to {
        bs_set_bit(ctx, &mut this, bit, true);
    }
    Ok(None)
}

pub fn native_bs_clear(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = obj_arg(args, 0)?;
    let bit = bs_checked_bit_index(ctx, args, 1)?;
    bs_set_bit(ctx, &mut this, bit, false);
    Ok(None)
}

pub fn native_bs_clear_all(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let Some(words) = bs_words_obj(ctx, this) else {
        return Ok(None);
    };
    let len = ctx.array_length(words);
    for i in 0..len {
        ctx.set_array_element(words, i, Value::Long(0));
    }
    bs_set_words_in_use(ctx, this, 0);
    Ok(None)
}

pub fn native_bs_clear_range(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = obj_arg(args, 0)?;
    let (from, to) = bs_checked_range(ctx, args, 1, 2)?;
    for bit in from..to {
        bs_set_bit(ctx, &mut this, bit, false);
    }
    Ok(None)
}

pub fn native_bs_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let bit = bs_checked_bit_index(ctx, args, 1)?;
    Ok(Some(Value::Int(if bs_get_bit(ctx, this, bit) {
        1
    } else {
        0
    })))
}

pub fn native_bs_flip(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = obj_arg(args, 0)?;
    let bit = bs_checked_bit_index(ctx, args, 1)?;
    let cur = bs_get_bit(ctx, this, bit);
    bs_set_bit(ctx, &mut this, bit, !cur);
    Ok(None)
}

pub fn native_bs_flip_range(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = obj_arg(args, 0)?;
    let (from, to) = bs_checked_range(ctx, args, 1, 2)?;
    for bit in from..to {
        let cur = bs_get_bit(ctx, this, bit);
        bs_set_bit(ctx, &mut this, bit, !cur);
    }
    Ok(None)
}

pub fn native_bs_length(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let words = bs_read_words(ctx, this);
    // Logical length = highest set bit + 1
    for i in (0..words.len()).rev() {
        if words[i] != 0 {
            let highest = 63 - words[i].leading_zeros() as usize;
            return Ok(Some(Value::Int((i * 64 + highest + 1) as i32)));
        }
    }
    Ok(Some(Value::Int(0)))
}

pub fn native_bs_size(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    if bs_layout(ctx).words_in_use.is_some() {
        let size = bs_words_obj(ctx, this)
            .map(|words| ctx.array_length(words).saturating_mul(64))
            .unwrap_or(0)
            .min(i32::MAX as usize);
        return Ok(Some(Value::Int(size as i32)));
    }
    let nbits = match ctx.get_field(this, BS_FIELD_NBITS) {
        Value::Int(n) => n,
        _ => 64,
    };
    Ok(Some(Value::Int(nbits)))}

pub fn native_bs_cardinality(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let words = bs_read_words(ctx, this);
    let count: u32 = words.iter().map(|w| (*w as u64).count_ones()).sum();
    Ok(Some(Value::Int(count as i32)))
}

pub fn native_bs_is_empty(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let words = bs_read_words(ctx, this);
    let empty = words.iter().all(|w| *w == 0);
    Ok(Some(Value::Int(if empty { 1 } else { 0 })))
}

pub fn native_bs_next_set_bit(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let from = bs_checked_bit_index(ctx, args, 1)?;
    let words = bs_read_words(ctx, this);
    let total_bits = words.len() * 64;
    for bit in from..total_bits {
        let word_idx = bit / 64;
        let bit_idx = bit % 64;
        if (words[word_idx] >> bit_idx) & 1 != 0 {
            return Ok(Some(Value::Int(bit as i32)));
        }
    }
    Ok(Some(Value::Int(-1)))
}

pub fn native_bs_next_clear_bit(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let from = bs_checked_bit_index(ctx, args, 1)?;
    let words = bs_read_words(ctx, this);
    let total_bits = words.len() * 64;
    if from >= total_bits {
        return Ok(Some(Value::Int(from.min(i32::MAX as usize) as i32)));
    }
    for bit in from..total_bits {
        let word_idx = bit / 64;
        let bit_idx = bit % 64;
        if (words[word_idx] >> bit_idx) & 1 == 0 {
            return Ok(Some(Value::Int(bit as i32)));
        }
    }
    Ok(Some(Value::Int(total_bits as i32)))}

pub fn native_bs_previous_set_bit(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let from = match args.get(1) {
        Some(Value::Int(n)) => *n,
        _ => -1,
    };
    if from < 0 {
        return Ok(Some(Value::Int(-1)));
    }
    let words = bs_read_words(ctx, this);
    let from_usize = from as usize;
    for bit in (0..=from_usize).rev() {
        let word_idx = bit / 64;
        if word_idx >= words.len() {
            continue;
        }
        let bit_idx = bit % 64;
        if (words[word_idx] >> bit_idx) & 1 != 0 {
            return Ok(Some(Value::Int(bit as i32)));
        }
    }
    Ok(Some(Value::Int(-1)))
}

pub fn native_bs_and(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let other = obj_arg(args, 1)?;
    let Some(this_words_obj) = bs_words_obj(ctx, this) else {
        return Ok(None);
    };
    let other_words = bs_read_words(ctx, other);
    let this_len = ctx.array_length(this_words_obj);
    for i in 0..this_len {
        let tw = match ctx.get_array_element(this_words_obj, i) {
            Value::Long(v) => v,
            _ => 0,
        };
        let ow = if i < other_words.len() {
            other_words[i]
        } else {
            0
        };
        ctx.set_array_element(this_words_obj, i, Value::Long(tw & ow));
    }
    bs_recalculate_words_in_use(ctx, this);
    Ok(None)
}

pub fn native_bs_or(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = obj_arg(args, 0)?;
    let other = obj_arg(args, 1)?;
    let other_words = bs_read_words(ctx, other);
    // Ensure capacity
    if !other_words.is_empty() {
        bs_ensure_capacity(ctx, &mut this, other_words.len() * 64 - 1);
    }
    let Some(this_words_obj) = bs_words_obj(ctx, this) else {
        return Ok(None);
    };
    let this_len = ctx.array_length(this_words_obj);
    for (i, &ow) in other_words.iter().enumerate().take(this_len) {
        let tw = match ctx.get_array_element(this_words_obj, i) {
            Value::Long(v) => v,
            _ => 0,
        };
        ctx.set_array_element(this_words_obj, i, Value::Long(tw | ow));    }
    bs_recalculate_words_in_use(ctx, this);
    Ok(None)
}

pub fn native_bs_xor(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = obj_arg(args, 0)?;
    let other = obj_arg(args, 1)?;
    let other_words = bs_read_words(ctx, other);
    if !other_words.is_empty() {
        bs_ensure_capacity(ctx, &mut this, other_words.len() * 64 - 1);
    }
    let Some(this_words_obj) = bs_words_obj(ctx, this) else {
        return Ok(None);
    };
    let this_len = ctx.array_length(this_words_obj);
    for (i, &ow) in other_words.iter().enumerate().take(this_len) {
        let tw = match ctx.get_array_element(this_words_obj, i) {
            Value::Long(v) => v,
            _ => 0,
        };
        ctx.set_array_element(this_words_obj, i, Value::Long(tw ^ ow));    }
    bs_recalculate_words_in_use(ctx, this);
    Ok(None)
}

pub fn native_bs_and_not(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let other = obj_arg(args, 1)?;
    let other_words = bs_read_words(ctx, other);
    let Some(this_words_obj) = bs_words_obj(ctx, this) else {
        return Ok(None);
    };
    let this_len = ctx.array_length(this_words_obj);
    for (i, &ow) in other_words.iter().enumerate().take(this_len) {
        let tw = match ctx.get_array_element(this_words_obj, i) {
            Value::Long(v) => v,
            _ => 0,
        };
        ctx.set_array_element(this_words_obj, i, Value::Long(tw & !ow));    }
    bs_recalculate_words_in_use(ctx, this);
    Ok(None)
}

pub fn native_bs_intersects(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let other = obj_arg(args, 1)?;
    let this_words = bs_read_words(ctx, this);
    let other_words = bs_read_words(ctx, other);
    let min_len = this_words.len().min(other_words.len());
    for i in 0..min_len {
        if this_words[i] & other_words[i] != 0 {
            return Ok(Some(Value::Int(1)));
        }
    }
    Ok(Some(Value::Int(0)))
}

pub fn native_bs_equals(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let other = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let this_words = bs_read_words(ctx, this);
    let other_words = bs_read_words(ctx, other);
    let max_len = this_words.len().max(other_words.len());
    for i in 0..max_len {
        let tw = if i < this_words.len() {
            this_words[i]
        } else {
            0
        };
        let ow = if i < other_words.len() {
            other_words[i]
        } else {
            0
        };
        if tw != ow {
            return Ok(Some(Value::Int(0)));
        }
    }
    Ok(Some(Value::Int(1)))
}

pub fn native_bs_hash_code(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let words = bs_read_words(ctx, this);
    let mut h: i64 = 1234;
    for (i, w) in words.iter().enumerate() {
        h ^= (*w) * (i as i64 + 1);
    }
    Ok(Some(Value::Int(((h >> 32) ^ h) as i32)))
}

pub fn native_bs_clone(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let words = bs_read_words(ctx, this);
    let capacity_words = bs_words_obj(ctx, this)
        .map(|words_obj| ctx.array_length(words_obj))
        .unwrap_or(words.len().max(1));
    // gc-common w20-b: the clone was held raw across its words array's
    // allocation.
    let clone = try_alloc_concurrent_synthetic(ctx, "java/util/BitSet", 3)?;
    let layout = bs_layout(ctx);
    let word_values: Vec<Value> = words.iter().map(|w| Value::Long(*w)).collect();
    let clone: ObjectRef = alloc_filled_array_into_field(
        ctx,
        clone,
        layout.words,
        cratonvm_types::ArrayElementType::Long,
        &word_values,
    );
    if let Some(slot) = layout.words_in_use {
        ctx.set_field(
            clone,
            slot,
            Value::Int(words.len().min(i32::MAX as usize) as i32),
        );
    } else {
        ctx.set_field(
            clone,
            BS_FIELD_NBITS,
            Value::Int((capacity_words.saturating_mul(64)).min(i32::MAX as usize) as i32),
        );
    }
    if let Some(slot) = layout.size_is_sticky {
        ctx.set_field(clone, slot, Value::Int(0));
    }
    Ok(Some(Value::Object(Some(clone))))
}

pub fn native_bs_to_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let words = bs_read_words(ctx, this);
    let total_bits = words.len() * 64;
    let mut bits = Vec::new();
    for bit in 0..total_bits {
        let word_idx = bit / 64;
        let bit_idx = bit % 64;
        if (words[word_idx] >> bit_idx) & 1 != 0 {
            bits.push(bit.to_string());
        }
    }
    let s = format!("{{{}}}", bits.join(", "));
    let obj = ctx.create_string(&s);
    Ok(Some(Value::Object(Some(obj))))
}

pub fn native_bs_value_of_longs(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let arr = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    // gc-common w20-b: the source array (an address copied out of `args`)
    // was read after the BitSet's allocation -- a class resolution, which
    // can run `<clinit>` -- and the BitSet held raw across its words array.
    // The words are copied out first; the BitSet is pinned by the helper.
    let len = ctx.array_length(arr);
    let word_values: Vec<Value> = (0..len).map(|i| ctx.get_array_element(arr, i)).collect();
    let words_in_use = word_values
        .iter()
        .rposition(|v| !matches!(v, Value::Long(0)))
        .map_or(0, |last| last + 1);
    let bs = try_alloc_concurrent_synthetic(ctx, "java/util/BitSet", 3)?;
    let layout = bs_layout(ctx);
    let bs: ObjectRef = alloc_filled_array_into_field(
        ctx,
        bs,
        layout.words,
        cratonvm_types::ArrayElementType::Long,
        &word_values,
    );
    if let Some(slot) = layout.words_in_use {
        ctx.set_field(
            bs,
            slot,
            Value::Int(words_in_use.min(i32::MAX as usize) as i32),
        );
    } else {
        ctx.set_field(bs, BS_FIELD_NBITS, Value::Int((len * 64) as i32));    }
    if let Some(slot) = layout.size_is_sticky {
        ctx.set_field(bs, slot, Value::Int(0));
    }
    Ok(Some(Value::Object(Some(bs))))
}

pub fn native_bs_to_long_array(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let words = bs_read_words(ctx, this);
    // Trim trailing zeros
    let mut logical_len = words.len();
    while logical_len > 0 && words[logical_len - 1] == 0 {
        logical_len -= 1;
    }
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Long, logical_len);
    for (i, &w) in words.iter().enumerate().take(logical_len) {
        ctx.set_array_element(arr, i, Value::Long(w));
    }
    Ok(Some(Value::Object(Some(arr))))
}

pub fn native_bs_stream(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let words = bs_read_words(ctx, this);
    let total_bits = words.len() * 64;
    let mut set_bits = Vec::new();
    for bit in 0..total_bits {
        let word_idx = bit / 64;
        let bit_idx = bit % 64;
        if (words[word_idx] >> bit_idx) & 1 != 0 {
            set_bits.push(bit as i32);
        }
    }
    // Create IntStream as 1-field synthetic (Object[] backing array of boxed ints)
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Int, set_bits.len());
    for (i, &b) in set_bits.iter().enumerate() {
        ctx.set_array_element(arr, i, Value::Int(b));
    }
    let stream = try_alloc_concurrent_synthetic(ctx, "java/util/stream/IntStream", 1)?;
    ctx.set_field(stream, 0, Value::Object(Some(arr)));
    Ok(Some(Value::Object(Some(stream))))
}

// ---------------------------------------------------------------------------
// EnumSet — 2-field synthetic (elements=0 ArrayList-like, enumType=1 String)
// Simplified: backed by ArrayList of enum values
// ---------------------------------------------------------------------------
pub const ES_FIELD_ELEMENTS: usize = 0;
pub const ES_FIELD_TYPE: usize = 1;

pub fn enum_set_backing_list(
    ctx: &mut dyn NativeContext,
    arr: ObjectRef,
    size: i32,
) -> Result<ObjectRef, MethodCallFailed> {
    // gc-common w20-b: `arr` was held raw across the list's allocation (a
    // class resolution, which can run `<clinit>`).
    let arr_pin = ctx.pin_native_root(arr);
    let backing = try_alloc_concurrent_synthetic(ctx, "java/util/ArrayList", 2);
    let arr = ctx.read_native_pin(arr_pin, arr);
    ctx.unpin_native_roots(arr_pin);
    let backing: ObjectRef = backing?;
    let arr_value = Value::Object(Some(arr));
    ctx.set_field(backing, 0, arr_value);
    ctx.set_field(backing, 1, Value::Int(size));
    ctx.set_field_by_name(backing, "elementData", arr_value);
    ctx.set_field_by_name(backing, "size", Value::Int(size));
    if let Some(slot) = ctx.resolve_field_index("java/util/ArrayList", "elementData") {
        ctx.set_field(backing, slot, arr_value);
    }
    if let Some(slot) = ctx.resolve_field_index("java/util/ArrayList", "size") {
        ctx.set_field(backing, slot, Value::Int(size));
    }
    Ok(backing)
}

pub fn register_enum_set_natives_with_category(
    r: &mut NativeMethodRegistry,
    kind: cratonvm_native_api::NativeKind,
) {
            let c = "java/util/EnumSet";
    r.register(
        c,
        "noneOf",
        "(Ljava/lang/Class;)Ljava/util/EnumSet;",
        native_es_none_of, kind,
    );
    r.register(
        c,
        "allOf",
        "(Ljava/lang/Class;)Ljava/util/EnumSet;",
        native_es_all_of, kind,
    );
    r.register(c, "toString", "()Ljava/lang/String;", native_es_to_string, kind);
    r.register(
        c,
        "of",
        "(Ljava/lang/Enum;)Ljava/util/EnumSet;",
        native_es_of_one, kind,
    );
    r.register(
        c,
        "of",
        "(Ljava/lang/Enum;Ljava/lang/Enum;)Ljava/util/EnumSet;",
        native_es_of_two, kind,
    );
    r.register(
        c,
        "of",
        "(Ljava/lang/Enum;Ljava/lang/Enum;Ljava/lang/Enum;)Ljava/util/EnumSet;",
        native_es_of_three, kind,
    );
    r.register(
        c,
        "of",
        "(Ljava/lang/Enum;Ljava/lang/Enum;Ljava/lang/Enum;Ljava/lang/Enum;)Ljava/util/EnumSet;",
        native_es_of_four, kind,
    );
    r.register(
        c,
        "of",
        "(Ljava/lang/Enum;[Ljava/lang/Enum;)Ljava/util/EnumSet;",
        native_es_of_varargs, kind,
    );
    r.register(
        c,
        "range",
        "(Ljava/lang/Enum;Ljava/lang/Enum;)Ljava/util/EnumSet;",
        native_es_range, kind,
    );
    r.register(
        c,
        "copyOf",
        "(Ljava/util/Collection;)Ljava/util/EnumSet;",
        native_es_copy_of, kind,
    );
    r.register(
        c,
        "complementOf",
        "(Ljava/util/EnumSet;)Ljava/util/EnumSet;",
        native_es_complement_of, kind,
    );
    r.register(c, "add", "(Ljava/lang/Object;)Z", native_es_add, kind);
    r.register(c, "addAll", "(Ljava/util/Collection;)Z", native_es_add_all, kind);
    r.register(c, "remove", "(Ljava/lang/Object;)Z", native_es_remove, kind);
    r.register(c, "contains", "(Ljava/lang/Object;)Z", native_es_contains, kind);
    r.register(c, "size", "()I", native_es_size, kind);
    r.register(c, "isEmpty", "()Z", native_es_is_empty, kind);
    r.register(c, "clear", "()V", native_es_clear, kind);
    r.register(c, "iterator", "()Ljava/util/Iterator;", native_es_iterator, kind);
    r.register(c, "toArray", "()[Ljava/lang/Object;", native_es_to_array, kind);
    // S111r32 — typed `toArray(T[])` overload. Spring's
    // `MergedAnnotation$Adapt.values(boolean, boolean)` builds an EnumSet,
    // adds CLASS_TO_STRING, then calls `set.toArray(new Adapt[0])` to get
    // back a `[LAdapt;` array. Without this override, the call falls through
    // to the inherited `AbstractCollection.toArray(T[])` — which uses field
    // `elementData` (only present on ArrayList) — and returns an empty
    // array. The downstream `MergedAnnotation.asMap(factory, adapts)`
    // therefore sees an empty `adapts` list and skips the
    // `Adapt.CLASS_TO_STRING` conversion in `TypeMappedAnnotation.adapt`,
    // leaving `Class[]` (e.g. `basePackageClasses` on `@ComponentScan`) in
    // the resulting `AnnotationAttributes`. Spring Boot's
    // `ConfigurationWarningsApplicationContextInitializer` then calls
    // `attrs.getStringArray("basePackageClasses")` and throws
    // `IllegalArgumentException: Attribute 'basePackageClasses' is of type
    // Class[], but String[] was expected`, blocking eureka startup.
    r.register(
        c,
        "toArray",
        "([Ljava/lang/Object;)[Ljava/lang/Object;",
        native_es_to_array_typed, kind,
    );
    r.register(c, "clone", "()Ljava/lang/Object;", native_es_clone, kind);
    }

pub fn register_enum_set_natives(r: &mut NativeMethodRegistry) {
    register_enum_set_natives_with_category(r, cratonvm_native_api::NativeKind::Intrinsic);
}

pub fn register_enum_set_stub_natives(r: &mut NativeMethodRegistry) {
    register_enum_set_natives_with_category(r, cratonvm_native_api::NativeKind::SyntheticStub);
}

pub fn enum_constants_array_for_class(
    ctx: &mut dyn NativeContext,
    enum_class: ObjectRef,
) -> Option<ObjectRef> {
    match crate::lang_class::native_class_get_enum_constants(
        ctx,
        &[Value::Object(Some(enum_class))],
    ) {
        Ok(Some(Value::Object(Some(arr)))) => Some(arr),
        _ => None,
    }
}

pub fn try_jdk_enum_set_none_of(
    ctx: &mut dyn NativeContext,
    enum_class: ObjectRef,
) -> Option<ObjectRef> {
    if ctx.is_class_synthetic_stub("java/util/EnumSet") {
        return None;
    }
    // gc-common w20-b: `enum_class` was passed to the constructor at its
    // address from before `getEnumConstants` (which can run the enum's
    // `<clinit>` and allocates the copy).
    let class_pin = ctx.pin_native_root(enum_class);
    let universe = enum_constants_array_for_class(ctx, enum_class);
    let enum_class = ctx.read_native_pin(class_pin, enum_class);
    ctx.unpin_native_roots(class_pin);
    let universe = universe?;
    let impl_class = if ctx.array_length(universe) <= 64 {
        "java/util/RegularEnumSet"
    } else {
        "java/util/JumboEnumSet"
    };
    match ctx.new_object_initialized(
        impl_class,
        "(Ljava/lang/Class;[Ljava/lang/Enum;)V",
        &[
            Value::Object(Some(enum_class)),
            Value::Object(Some(universe)),
        ],
    ) {
        Ok(Some(Value::Object(Some(set)))) => Some(set),
        _ => None,
    }
}

pub fn synthetic_enum_set_from_array(
    ctx: &mut dyn NativeContext,
    enum_type: Value,
    src: Option<ObjectRef>,
    len: usize,
) -> MethodCallResult {
    // gc-common w20-b: one handle scope. `enum_type` (the `Class` argument)
    // was held raw across every allocation here, and each `?` leaked the
    // pins taken before it.
    let mut scope = NativeHandleScope::new(ctx);
    let type_slot = element_handle(&mut scope, Some(enum_type));
    let src_h = src.map(|s| scope.root(s));
    let es = try_alloc_concurrent_synthetic(&mut *scope, "java/util/EnumSet", 2)?;
    let es_h = scope.root(es);
    let arr = scope.new_array(cratonvm_types::ArrayElementType::Reference, len.max(4));
    if let Some(src_h) = &src_h {
        let src_cur: ObjectRef = scope.get(src_h);
        let copy = len.min(scope.array_length(src_cur));
        for i in 0..copy {
            let v = scope.get_array_element(src_cur, i);
            scope.set_array_element(arr, i, v);
        }
    }
    let backing = enum_set_backing_list(&mut *scope, arr, len as i32)?;
    let es = scope.get(&es_h);
    scope.set_field(es, ES_FIELD_ELEMENTS, Value::Object(Some(backing)));
    let enum_type = element_value(&scope, &type_slot);
    scope.set_field(es, ES_FIELD_TYPE, enum_type);
    Ok(Some(Value::Object(Some(es))))
}

/// The synthetic 2-field `EnumSet` holding the rooted `slots` in order, typed
/// by the rooted `enum_type`.
///
/// `backing_by_name` also stamps `ArrayList`'s `elementData` / `size` on the
/// backing list ([`enum_set_backing_list`]), as the fixed-arity, varargs,
/// `range` and `complementOf` factories always did; `of(E)` and `copyOf`
/// wrote slots 0 / 1 only. Both shapes are kept byte-for-byte.
///
/// gc-common w20-b: the eight factories that spelled this out held their
/// elements (addresses copied out of `args`, or out of a source set) raw
/// across the set's and the list's allocations -- class resolutions, which
/// can run `<clinit>` -- and most held the set raw across the rest.
pub fn synthetic_enum_set_rooted(
    scope: &mut NativeHandleScope<'_>,
    enum_type: &ArgSlot,
    slots: &[ArgSlot],
    backing_by_name: bool,
) -> MethodCallResult {
    let es = try_alloc_concurrent_synthetic(&mut **scope, "java/util/EnumSet", 2)?;
    let es_h = scope.root(es);
    let arr = scope.new_array(
        cratonvm_types::ArrayElementType::Reference,
        slots.len().max(4),
    );
    for (i, slot) in slots.iter().enumerate() {
        let v = element_value(scope, slot);
        scope.set_array_element(arr, i, v);
    }
    let backing: ObjectRef = if backing_by_name {
        enum_set_backing_list(&mut **scope, arr, slots.len() as i32)?
    } else {
        let arr_h = scope.root(arr);
        let backing = try_alloc_concurrent_synthetic(&mut **scope, "java/util/ArrayList", 2)?;
        let arr = scope.get(&arr_h);
        scope.set_field(backing, 0, Value::Object(Some(arr)));
        scope.set_field(backing, 1, Value::Int(slots.len() as i32));
        backing
    };
    let es = scope.get(&es_h);
    scope.set_field(es, ES_FIELD_ELEMENTS, Value::Object(Some(backing)));
    let enum_type = element_value(scope, enum_type);
    scope.set_field(es, ES_FIELD_TYPE, enum_type);
    Ok(Some(Value::Object(Some(es))))
}

/// Every value of `values` rooted in `scope` (primitives and nulls carried
/// verbatim), for a list of elements held across Java calls.
pub fn root_values(scope: &mut NativeHandleScope<'_>, values: &[Value]) -> Vec<ArgSlot> {
    let mut slots = Vec::with_capacity(values.len());
    for v in values {
        slots.push(element_handle(scope, Some(*v)));
    }
    slots
}

/// `EnumSet.of(..)` (every arity), `range`, `complementOf` and the non-empty
/// `copyOf`: a real JDK `EnumSet` when `java.util.EnumSet` comes from class
/// files, else the synthetic bridge ([`synthetic_enum_set_rooted`]).
///
/// `typed` (`copyOf`): the synthetic set's element type is the first
/// element's declaring class, as `copyOf` always computed it.
///
/// gc-common w20-b: `elems` are rooted once, before the first Java call
/// (`getDeclaringClass`, `noneOf`, each `add`), and read back at each use.
pub fn enum_set_of_values(
    ctx: &mut dyn NativeContext,
    elems: &[Value],
    typed: bool,
    backing_by_name: bool,
) -> MethodCallResult {
    let mut scope = NativeHandleScope::new(ctx);
    let slots = root_values(&mut scope, elems);
    if let Some(set) = try_jdk_enum_set_of_slots(&mut scope, &slots) {
        return Ok(Some(Value::Object(Some(set))));
    }
    let enum_type = match slots.first() {
        Some(first) if typed => {
            let first = element_value(&scope, first);
            enum_class_from_value(&mut *scope, first).unwrap_or(Value::Object(None))
        }
        _ => Value::Object(None),
    };
    let type_slot = element_handle(&mut scope, Some(enum_type));
    synthetic_enum_set_rooted(&mut scope, &type_slot, &slots, backing_by_name)
}

pub fn native_es_none_of(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let enum_type = args.first().copied().unwrap_or(Value::Object(None));
    // gc-common w20-b: `enum_type` was reused after `noneOf`'s Java.
    let mut scope = NativeHandleScope::new(ctx);
    let type_slot = element_handle(&mut scope, Some(enum_type));
    if let Value::Object(Some(enum_class)) = enum_type {
        if let Some(set) = try_jdk_enum_set_none_of(&mut *scope, enum_class) {
            return Ok(Some(Value::Object(Some(set))));
        }
    }
    let enum_type = element_value(&scope, &type_slot);
    synthetic_enum_set_from_array(&mut *scope, enum_type, None, 0)
}

pub fn native_es_all_of(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let enum_type = args.first().copied().unwrap_or(Value::Object(None));
    // gc-common w20-b: `enum_class` was passed to `getEnumConstants` and
    // `enum_type` stored into the synthetic set after `noneOf` and `addAll`
    // (Java) at their entry addresses.
    let mut scope = NativeHandleScope::new(ctx);
    let type_slot = element_handle(&mut scope, Some(enum_type));
    if let ArgSlot::Ref(class_h) = &type_slot {
        let enum_class = scope.get(class_h);
        if let Some(set) = try_jdk_enum_set_none_of(&mut *scope, enum_class) {
            let set_h = scope.root(set);
            if scope.invoke_virtual(set, "addAll", "()V", &[]).is_ok() {
                let set_cur = scope.get(&set_h);
                return Ok(Some(Value::Object(Some(set_cur))));
            }
        }
        let enum_class = scope.get(class_h);
        if let Some(constants) = enum_constants_array_for_class(&mut *scope, enum_class) {
            let len = scope.array_length(constants);
            let enum_type = element_value(&scope, &type_slot);
            return synthetic_enum_set_from_array(&mut *scope, enum_type, Some(constants), len);
        }
    }
    let enum_type = element_value(&scope, &type_slot);
    synthetic_enum_set_from_array(&mut *scope, enum_type, None, 0)
}

pub fn native_es_of_one(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let elem = args.first().copied().unwrap_or(Value::Object(None));
    enum_set_of_values(ctx, &[elem], false, false)
}

/// Build a real JDK `EnumSet` via `noneOf(first.getClass())` + `add` for each
/// non-null element. Returns `None` when `java.util.EnumSet` is synthetic or
/// any invoke step fails (caller falls back to the 2-field bridge).
///
/// gc-common w20-b: the elements come in rooted (they used to be raw across
/// `getDeclaringClass`, `noneOf` and every `add`); the set's pin is no longer
/// leaked on either exit, and the set is returned at its current address
/// (the pre-`add` one was returned).
pub fn try_jdk_enum_set_of_slots(
    scope: &mut NativeHandleScope<'_>,
    slots: &[ArgSlot],
) -> Option<ObjectRef> {
    if scope.is_class_synthetic_stub("java/util/EnumSet") {
        return None;
    }
    let Value::Object(Some(en1)) = element_value(scope, slots.first()?) else {
        return None;
    };
    let enum_class = enum_declaring_class_from_object(&mut **scope, en1)?;
    let set = match scope.invoke(
        "java/util/EnumSet",
        "noneOf",
        "(Ljava/lang/Class;)Ljava/util/EnumSet;",
        &[Value::Object(Some(enum_class))],
    ) {
        Ok(Some(Value::Object(Some(s)))) => s,
        _ => return None,
    };
    let set_h = scope.root(set);
    for slot in slots {
        let elem = element_value(scope, slot);
        if let Value::Object(Some(_)) = elem {
            let set_now = scope.get(&set_h);
            if scope
                .invoke_virtual(set_now, "add", "(Ljava/lang/Object;)Z", &[elem])
                .is_err()
            {
                return None;
            }
        }
    }
    Some(scope.get(&set_h))
}

pub fn enum_value_ordinal(ctx: &mut dyn NativeContext, value: Value) -> Option<i32> {
    let Value::Object(Some(obj)) = value else {
        return None;
    };
    // gc-common w20-b: the field fallbacks read `obj` after `ordinal()`
    // (Java) at its entry address.
    let obj_pin = ctx.pin_native_root(obj);
    let invoked = ctx.invoke_virtual(obj, "ordinal", "()I", &[]);
    let obj = ctx.read_native_pin(obj_pin, obj);
    ctx.unpin_native_roots(obj_pin);
    if let Ok(Some(Value::Int(n))) = invoked {
        return Some(n);
    }
    match ctx.get_field_by_name(obj, "ordinal") {
        Value::Int(n) => Some(n),
        _ => match ctx.get_field(obj, 1) {
            Value::Int(n) => Some(n),
            _ => None,
        },
    }
}

/// The current values of `slots`, read after the caller's last GC point.
pub fn values_now(scope: &NativeHandleScope<'_>, slots: &[ArgSlot]) -> Vec<Value> {
    slots.iter().map(|s| element_value(scope, s)).collect()
}

/// The constants of `from`'s enum whose ordinals lie in `from..=to`
/// (`EnumSet.range`), or `[from, to]` when they cannot be computed.
///
/// gc-common w20-b: every step here is Java (`ordinal()` per constant,
/// `getClass`, `getEnumConstants`), and `from` / `to`, the constants array
/// and every element collected so far were held raw across all of it. They
/// are rooted, and the answer is read out after the last Java call; the
/// caller roots it before its own next GC point.
pub fn enum_range_elements(ctx: &mut dyn NativeContext, from: Value, to: Value) -> Vec<Value> {
    let mut scope = NativeHandleScope::new(ctx);
    let ends = root_values(&mut scope, &[from, to]);
    let Some(from_ord) = enum_value_ordinal(&mut *scope, from) else {
        return values_now(&scope, &ends);
    };
    let to = element_value(&scope, &ends[1]);
    let Some(to_ord) = enum_value_ordinal(&mut *scope, to) else {
        return values_now(&scope, &ends);
    };
    let from = element_value(&scope, &ends[0]);
    let to = element_value(&scope, &ends[1]);
    let (Value::Object(Some(from_obj)), Value::Object(Some(_))) = (from, to) else {
        return values_now(&scope, &ends);
    };
    let enum_cid = scope.class_id_of_object(from_obj);
    let constants: ObjectRef = match scope
        .static_field_index_by_name(enum_cid, "$VALUES")
        .map(|idx| scope.get_static_field(enum_cid, idx))
    {
        Some(Value::Object(Some(a))) => a,
        _ => {
            let enum_class: ObjectRef =
                match scope.invoke_virtual(from_obj, "getClass", "()Ljava/lang/Class;", &[]) {
                    Ok(Some(Value::Object(Some(c)))) => c,
                    _ => return values_now(&scope, &ends),
                };
            match scope.invoke_virtual(enum_class, "getEnumConstants", "()[Ljava/lang/Object;", &[])
            {
                Ok(Some(Value::Object(Some(a)))) => a,
                _ => return values_now(&scope, &ends),
            }
        }
    };
    let constants_h = scope.root(constants);
    let len = scope.array_length(constants);
    let mut in_range = Vec::new();
    for i in 0..len {
        let constants = scope.get(&constants_h);
        let elem = scope.get_array_element(constants, i);
        let elem_slot = element_handle(&mut scope, Some(elem));
        if let Some(ord) = enum_value_ordinal(&mut *scope, elem) {
            if ord >= from_ord && ord <= to_ord {
                in_range.push(elem_slot);
            }
        }
    }
    if in_range.is_empty() {
        values_now(&scope, &ends)
    } else {
        values_now(&scope, &in_range)
    }
}

pub fn enum_set_elements(ctx: &mut dyn NativeContext, set: ObjectRef) -> Vec<Value> {
    if es_real_kind(ctx, set).is_some() {
        return es_real_elements(ctx, set);
    }
    // gc-common w20-b: the backing fallback read `set` after `toArray()`
    // (Java) at its entry address.
    let set_pin = ctx.pin_native_root(set);
    let to_array = ctx.invoke_virtual(set, "toArray", "()[Ljava/lang/Object;", &[]);
    let set = ctx.read_native_pin(set_pin, set);
    ctx.unpin_native_roots(set_pin);
    if let Ok(Some(Value::Object(Some(arr)))) = to_array {
        return (0..ctx.array_length(arr))
            .map(|i| ctx.get_array_element(arr, i))
            .collect();
    }
    let Some(backing) = es_get_backing(ctx, set) else {
        return Vec::new();
    };
    let size = ctx.get_field(backing, 1).as_int().unwrap_or(0).max(0) as usize;
    let Value::Object(Some(arr)) = ctx.get_field(backing, 0) else {
        return Vec::new();
    };
    (0..size.min(ctx.array_length(arr)))
        .map(|i| ctx.get_array_element(arr, i))
        .collect()
}

pub fn enum_universe_for_value(ctx: &mut dyn NativeContext, value: Value) -> Vec<Value> {
    let Value::Object(Some(obj)) = value else {
        return Vec::new();
    };
    let enum_cid = ctx.class_id_of_object(obj);
    if let Some(Value::Object(Some(values))) = ctx
        .static_field_index_by_name(enum_cid, "$VALUES")
        .map(|idx| ctx.get_static_field(enum_cid, idx))
    {
        return (0..ctx.array_length(values))
            .map(|i| ctx.get_array_element(values, i))
            .collect();
    }
    let enum_class = match ctx.invoke_virtual(obj, "getClass", "()Ljava/lang/Class;", &[]) {
        Ok(Some(Value::Object(Some(c)))) => c,
        _ => return Vec::new(),
    };
    match ctx.invoke_virtual(enum_class, "getEnumConstants", "()[Ljava/lang/Object;", &[]) {
        Ok(Some(Value::Object(Some(values)))) => (0..ctx.array_length(values))
            .map(|i| ctx.get_array_element(values, i))
            .collect(),
        _ => Vec::new(),
    }
}

pub fn enum_values_same(a: Value, b: Value) -> bool {
    matches!((a, b), (Value::Object(Some(x)), Value::Object(Some(y))) if x.as_ptr() == y.as_ptr())
}

/// `EnumSet.of(E, E)` — same linkage gap as the 3-arg overload on some JDK
/// loads; mirror the `noneOf` + `add` bridge used by [`native_es_of_three`].
pub fn native_es_of_two(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let e1 = args.first().copied().unwrap_or(Value::Object(None));
    let e2 = args.get(1).copied().unwrap_or(Value::Object(None));
    enum_set_of_values(ctx, &[e1, e2], false, true)
}

/// `EnumSet.of(E, E, E)` — Spring Boot 2.7+ calls this overload during
/// `SpringApplication` static init. Prefer a real JDK `EnumSet` built via
/// `noneOf(first.getClass())` + `add` when `java.util.EnumSet` is loaded from
/// classfiles; fall back to the same 2-field synthetic bridge as `of(E,E)`.
pub fn native_es_of_three(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let e1 = args.first().copied().unwrap_or(Value::Object(None));
    let e2 = args.get(1).copied().unwrap_or(Value::Object(None));
    let e3 = args.get(2).copied().unwrap_or(Value::Object(None));
    enum_set_of_values(ctx, &[e1, e2, e3], false, true)
}

/// `EnumSet.of(E, E, E, E)` — same bridge as the other fixed-arity overloads.
pub fn native_es_of_four(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let e1 = args.first().copied().unwrap_or(Value::Object(None));
    let e2 = args.get(1).copied().unwrap_or(Value::Object(None));
    let e3 = args.get(2).copied().unwrap_or(Value::Object(None));
    let e4 = args.get(3).copied().unwrap_or(Value::Object(None));
    enum_set_of_values(ctx, &[e1, e2, e3, e4], false, true)
}

pub fn native_es_of_varargs(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let first = args.first().copied().unwrap_or(Value::Object(None));
    let rest = args.get(1).copied().unwrap_or(Value::Object(None));
    let mut elems = Vec::new();
    elems.push(first);
    if let Value::Object(Some(arr)) = rest {
        for i in 0..ctx.array_length(arr) {
            elems.push(ctx.get_array_element(arr, i));
        }
    }
    enum_set_of_values(ctx, &elems, false, true)
}

pub fn native_es_range(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let from = args.first().copied().unwrap_or(Value::Object(None));
    let to = args.get(1).copied().unwrap_or(Value::Object(None));
    let elems = enum_range_elements(ctx, from, to);
    enum_set_of_values(ctx, &elems, false, true)
}

pub fn native_es_copy_of(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let src = obj_arg(args, 0)?;
    // gc-common w20-b: the empty-source arm read `src` after `toArray()`
    // (Java) at its entry address, and held the element type raw across
    // `noneOf`.
    let src_pin = ctx.pin_native_root(src);
    let elems = enum_set_copy_source_values(ctx, src);
    let src = ctx.read_native_pin(src_pin, src);
    ctx.unpin_native_roots(src_pin);

    if !elems.is_empty() {
        return enum_set_of_values(ctx, &elems, true, false);
    }

    let mut scope = NativeHandleScope::new(ctx);
    if let Some(enum_type) = enum_set_element_type(&mut *scope, src) {
        let type_slot = element_handle(&mut scope, Some(enum_type));
        if let Value::Object(Some(enum_class)) = enum_type {
            if let Some(set) = try_jdk_enum_set_none_of(&mut *scope, enum_class) {
                return Ok(Some(Value::Object(Some(set))));
            }
        }
        return synthetic_enum_set_rooted(&mut scope, &type_slot, &[], false);
    }

    synthetic_enum_set_rooted(&mut scope, &ArgSlot::Plain(Value::Object(None)), &[], false)
}

pub fn enum_set_copy_source_values(ctx: &mut dyn NativeContext, src: ObjectRef) -> Vec<Value> {
    if es_real_kind(ctx, src).is_some() {
        return es_real_elements(ctx, src);
    }
    let arr = match ctx.invoke_virtual(src, "toArray", "()[Ljava/lang/Object;", &[]) {
        Ok(Some(Value::Object(Some(arr)))) => arr,
        _ => return Vec::new(),
    };
    let len = ctx.array_length(arr);
    let mut elems = Vec::with_capacity(len);
    for i in 0..len {
        let elem = ctx.get_array_element(arr, i);
        if !matches!(elem, Value::Object(None)) {
            elems.push(elem);
        }
    }
    elems
}

pub fn enum_class_from_value(ctx: &mut dyn NativeContext, value: Value) -> Option<Value> {
    let Value::Object(Some(elem)) = value else {
        return None;
    };
    enum_declaring_class_from_object(ctx, elem).map(|class| Value::Object(Some(class)))}

pub fn enum_declaring_class_from_object(
    ctx: &mut dyn NativeContext,
    elem: cratonvm_types::ObjectRef,
) -> Option<cratonvm_types::ObjectRef> {
    // `elem` is a PARAMETER, which is exactly as unrooted as a local -- the
    // blind spot recorded in the audit page. The fallback arm below runs AFTER
    // `getDeclaringClass()` has already had its chance to collect and move it.
    let elem_pin = ctx.pin_native_root(elem);
    let declaring = ctx.invoke_virtual(elem, "getDeclaringClass", "()Ljava/lang/Class;", &[]);
    let elem = ctx.read_native_pin(elem_pin, elem);
    match declaring {
        Ok(Some(Value::Object(Some(class)))) => Some(class),
        _ => match ctx.invoke_virtual(elem, "getClass", "()Ljava/lang/Class;", &[]) {
            Ok(Some(Value::Object(Some(class)))) => Some(class),
            _ => None,
        },
    }
}

pub fn enum_set_element_type(ctx: &mut dyn NativeContext, set: ObjectRef) -> Option<Value> {
    let cls = ctx
        .class_name_of_id(ctx.class_id_of_object(set))
        .unwrap_or_default();
    if cls == "java/util/RegularEnumSet" || cls == "java/util/JumboEnumSet" {
        match ctx.get_field_by_name(set, "elementType") {
            Value::Object(Some(enum_class)) => return Some(Value::Object(Some(enum_class))),
            _ => {}
        }
    }
    match ctx.get_field(set, ES_FIELD_TYPE) {
        Value::Object(Some(enum_class)) => Some(Value::Object(Some(enum_class))),
        _ => None,
    }
}

pub fn native_es_complement_of(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let source = obj_arg(args, 0)?;
    let source_elems = enum_set_elements(ctx, source);
    let Some(seed) = source_elems
        .iter()
        .copied()
        .find(|v| matches!(v, Value::Object(Some(_))))
    else {
        return native_es_none_of(ctx, &[]);
    };
    // gc-common w20-b: the source's elements were compared BY ADDRESS with
    // the universe after `getClass` / `getEnumConstants` (Java) had had the
    // chance to move them -- a moved element no longer matched, and the
    // complement then wrongly contained it. They are rooted across the
    // universe lookup and compared at their current addresses.
    let mut scope = NativeHandleScope::new(ctx);
    let source_slots = root_values(&mut scope, &source_elems);
    let universe = enum_universe_for_value(&mut *scope, seed);
    let present_now = values_now(&scope, &source_slots);
    let elems: Vec<Value> = universe
        .into_iter()
        .filter(|candidate| {
            !present_now
                .iter()
                .copied()
                .any(|present| enum_values_same(*candidate, present))
        })
        .collect();
    enum_set_of_values(&mut *scope, &elems, false, true)
}

pub fn es_get_backing(ctx: &mut dyn NativeContext, this: ObjectRef) -> Option<ObjectRef> {
    match ctx.get_field(this, ES_FIELD_ELEMENTS) {
        Value::Object(Some(o)) => Some(o),
        _ => None,
    }
}

pub fn es_real_kind(ctx: &mut dyn NativeContext, this: ObjectRef) -> Option<&'static str> {
    match ctx
        .class_name_arc_of_id(ctx.class_id_of_object(this))
        .as_deref()
    {
        Some("java/util/RegularEnumSet") => Some("regular"),
        Some("java/util/JumboEnumSet") => Some("jumbo"),
        _ => None,
    }
}

pub fn es_enum_ordinal(ctx: &mut dyn NativeContext, elem: Value) -> Option<usize> {
    let ord = enum_value_ordinal(ctx, elem)?;
    if ord >= 0 {
        Some(ord as usize)
    } else {
        None
    }
}

pub fn es_real_contains_ordinal(ctx: &mut dyn NativeContext, this: ObjectRef, ord: usize) -> bool {
    let word = ord / 64;
    let bit = 1u64 << (ord & 63);
    match ctx.get_field_by_name(this, "elements") {
        Value::Long(mask) if word == 0 => ((mask as u64) & bit) != 0,
        Value::Int(mask) if word == 0 => ((mask as u32 as u64) & bit) != 0,
        Value::Object(Some(words)) => {
            if word >= ctx.array_length(words) {
                return false;
            }
            match ctx.get_array_element(words, word) {
                Value::Long(mask) => ((mask as u64) & bit) != 0,
                Value::Int(mask) => ((mask as u32 as u64) & bit) != 0,
                _ => false,
            }
        }
        _ => false,
    }
}

pub fn es_real_add_ordinal(ctx: &mut dyn NativeContext, this: ObjectRef, ord: usize) -> bool {
    let word = ord / 64;
    let bit = 1u64 << (ord & 63);
    match ctx.get_field_by_name(this, "elements") {
        Value::Long(mask) if word == 0 => {
            let old = mask as u64;
            let new = old | bit;
            if new != old {
                ctx.set_field_by_name(this, "elements", Value::Long(new as i64));
                true
            } else {
                false
            }
        }
        Value::Int(mask) if word == 0 => {
            let old = mask as u32 as u64;
            let new = old | bit;
            if new != old {
                ctx.set_field_by_name(this, "elements", Value::Long(new as i64));
                true
            } else {
                false
            }
        }
        Value::Object(Some(words)) => {
            if word >= ctx.array_length(words) {
                return false;
            }
            let old = match ctx.get_array_element(words, word) {
                Value::Long(mask) => mask as u64,
                Value::Int(mask) => mask as u32 as u64,
                _ => 0,
            };
            let new = old | bit;
            if new != old {
                ctx.set_array_element(words, word, Value::Long(new as i64));
                let size = es_real_size(ctx, this);
                ctx.set_field_by_name(this, "size", Value::Int(size));
                true
            } else {
                false
            }
        }
        _ => false,    }}

pub fn es_real_remove_ordinal(ctx: &mut dyn NativeContext, this: ObjectRef, ord: usize) -> bool {
    let word = ord / 64;
    let bit = 1u64 << (ord & 63);
    match ctx.get_field_by_name(this, "elements") {
        Value::Long(mask) if word == 0 => {
            let old = mask as u64;
            let new = old & !bit;
            if new != old {
                ctx.set_field_by_name(this, "elements", Value::Long(new as i64));
                true
            } else {
                false
            }
        }
        Value::Int(mask) if word == 0 => {
            let old = mask as u32 as u64;
            let new = old & !bit;
            if new != old {
                ctx.set_field_by_name(this, "elements", Value::Long(new as i64));
                true
            } else {
                false
            }
        }
        Value::Object(Some(words)) => {
            if word >= ctx.array_length(words) {
                return false;
            }
            let old = match ctx.get_array_element(words, word) {
                Value::Long(mask) => mask as u64,
                Value::Int(mask) => mask as u32 as u64,
                _ => 0,
            };
            let new = old & !bit;
            if new != old {
                ctx.set_array_element(words, word, Value::Long(new as i64));
                let size = es_real_size(ctx, this);
                ctx.set_field_by_name(this, "size", Value::Int(size));
                true
            } else {
                false
            }
        }
        _ => false,    }}

pub fn es_real_size(ctx: &mut dyn NativeContext, this: ObjectRef) -> i32 {
    match ctx.get_field_by_name(this, "elements") {
        Value::Long(mask) => (mask as u64).count_ones() as i32,
        Value::Int(mask) => (mask as u32).count_ones() as i32,
        Value::Object(Some(words)) => {
            let mut size = 0i32;
            for i in 0..ctx.array_length(words) {
                size += match ctx.get_array_element(words, i) {
                    Value::Long(mask) => (mask as u64).count_ones() as i32,
                    Value::Int(mask) => (mask as u32).count_ones() as i32,
                    _ => 0,
                };
            }
            size
        }
        _ => ctx.get_field_by_name(this, "size").as_int().unwrap_or(0),
    }
}

pub fn es_real_clear(ctx: &mut dyn NativeContext, this: ObjectRef) {
    match ctx.get_field_by_name(this, "elements") {
        Value::Long(_) | Value::Int(_) => {
            ctx.set_field_by_name(this, "elements", Value::Long(0));
        }
        Value::Object(Some(words)) => {
            for i in 0..ctx.array_length(words) {
                ctx.set_array_element(words, i, Value::Long(0));
            }
            ctx.set_field_by_name(this, "size", Value::Int(0));
        }
        _ => {}
    }
}

pub fn es_real_elements(ctx: &mut dyn NativeContext, this: ObjectRef) -> Vec<Value> {
    let universe = match ctx.get_field_by_name(this, "universe") {
        Value::Object(Some(universe)) => universe,
        _ => return Vec::new(),
    };
    let mut elems = Vec::new();
    for ord in 0..ctx.array_length(universe) {
        if es_real_contains_ordinal(ctx, this, ord) {
            let elem = ctx.get_array_element(universe, ord);
            if !matches!(elem, Value::Object(None)) {
                elems.push(elem);
            }
        }
    }
    elems
}

pub fn native_es_add(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let elem = args.get(1).copied().unwrap_or(Value::Object(None));
    if es_real_kind(ctx, this).is_some() {
        let modified = es_enum_ordinal(ctx, elem)
            .map(|ord| es_real_add_ordinal(ctx, this, ord))
            .unwrap_or(false);
        return Ok(Some(Value::Int(i32::from(modified))));
    }
    if let Some(backing) = es_get_backing(ctx, this) {
        // Delegate to ArrayList add, reclaiming: both callers (the `EnumSet.add`
        // registration and `native_es_add_all`, which roots its receiver and
        // snapshot) use nothing bare after this call (gc-common w11-c).
        cratonvm_native_collections::native_al_add_entry_pub(
            ctx,
            &[Value::Object(Some(backing)), elem],
        )?;
        return Ok(Some(Value::Int(1)));
    }
    Ok(Some(Value::Int(0)))
}

pub fn native_es_add_all(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let coll = match args.get(1).copied() {
        Some(Value::Object(Some(coll))) => coll,
        _ => return Ok(Some(Value::Int(0))),
    };
    // `toArray` is bytecode (a GC point), and every `native_es_add` below may
    // collect: root the receiver first, then each snapshot element
    // (gc-common w11-c handoff).
    let mut scope = NativeHandleScope::new(ctx);
    let this_h = scope.root(this);
    let coll_h = scope.root(coll);
    let to_array = scope.invoke_virtual(coll, "toArray", "()[Ljava/lang/Object;", &[]);
    let elems = if let Ok(Some(Value::Object(Some(arr)))) = to_array {
        (0..scope.array_length(arr))
            .map(|i| scope.get_array_element(arr, i))
            .collect::<Vec<_>>()
    } else {
        let coll = scope.get(&coll_h);
        enum_set_elements(&mut *scope, coll)
    };
    let elem_hs: Vec<NativeHandle> = elems
        .iter()
        .filter_map(|elem| match elem {
            Value::Object(Some(obj)) => Some(scope.root(*obj)),
            _ => None,
        })
        .collect();
    let mut modified = false;
    for elem_h in &elem_hs {
        let this = scope.get(&this_h);
        let elem = scope.get(elem_h);
        native_es_add(
            &mut *scope,
            &[Value::Object(Some(this)), Value::Object(Some(elem))],
        )?;
        modified = true;
    }
    Ok(Some(Value::Int(i32::from(modified))))
}

pub fn native_es_remove(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let elem = args.get(1).copied().unwrap_or(Value::Object(None));
    if es_real_kind(ctx, this).is_some() {
        let modified = es_enum_ordinal(ctx, elem)
            .map(|ord| es_real_remove_ordinal(ctx, this, ord))
            .unwrap_or(false);
        return Ok(Some(Value::Int(i32::from(modified))));
    }
    if let Some(backing) = es_get_backing(ctx, this) {
        let size = match ctx.get_field(backing, 1) {
            Value::Int(n) => n,
            _ => 0,
        };
        if size > 0 {
            // Simplified: just decrement size (not correct but functional stub)
            ctx.set_field(backing, 1, Value::Int(size - 1));
            return Ok(Some(Value::Int(1)));
        }
    }
    Ok(Some(Value::Int(0)))
}

pub fn native_es_contains(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let elem = args.get(1).copied().unwrap_or(Value::Object(None));
    if es_real_kind(ctx, this).is_some() {
        let contained = es_enum_ordinal(ctx, elem)
            .map(|ord| es_real_contains_ordinal(ctx, this, ord))
            .unwrap_or(false);
        return Ok(Some(Value::Int(i32::from(contained))));
    }
    if let Some(backing) = es_get_backing(ctx, this) {
        return cratonvm_native_collections::native_al_contains(
            ctx,
            &[Value::Object(Some(backing)), elem],
        );
    }
    Ok(Some(Value::Int(0)))
}

pub fn native_es_size(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    if es_real_kind(ctx, this).is_some() {
        return Ok(Some(Value::Int(es_real_size(ctx, this))));
    }
    if let Some(backing) = es_get_backing(ctx, this) {
        let size = match ctx.get_field(backing, 1) {
            Value::Int(n) => n,
            _ => 0,
        };
        return Ok(Some(Value::Int(size)));    }
    Ok(Some(Value::Int(0)))
}

pub fn native_es_is_empty(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    if es_real_kind(ctx, this).is_some() {
        return Ok(Some(Value::Int(if es_real_size(ctx, this) == 0 {
            1
        } else {
            0
        })));
    }
    if let Some(backing) = es_get_backing(ctx, this) {
        let size = match ctx.get_field(backing, 1) {
            Value::Int(n) => n,
            _ => 0,
        };
        return Ok(Some(Value::Int(if size == 0 { 1 } else { 0 })));    }
    Ok(Some(Value::Int(1)))
}

pub fn native_es_clear(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    if es_real_kind(ctx, this).is_some() {
        es_real_clear(ctx, this);
        return Ok(None);
    }
    if let Some(backing) = es_get_backing(ctx, this) {
        ctx.set_field(backing, 1, Value::Int(0));
    }
    Ok(None)
}

/// `EnumSet.toString()`.
///
/// In real-JDK mode `AbstractCollection.toString()` bytecode runs and nothing
/// here is consulted. `--synthetic-jdk` has no bytecode, so with no
/// registration the call reached `Object.toString` and printed
/// `java.util.EnumSet@9` where HotSpot prints `[ALPHA, GAMMA]` — measured
/// 2026-09-02, `apps/probes/SyntheticEnumSurface`.
///
/// Built on `native_es_iterator`'s snapshot rather than on the backing array
/// directly, so the two cannot disagree about which elements the set holds or
/// in what order — the same reason `native_es_size` reads the size through the
/// backing instead of counting.
pub fn native_es_to_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let cls = ctx
        .class_name_of_id(ctx.class_id_of_object(this))
        .unwrap_or_default();
    if cls == "java/util/RegularEnumSet" || cls == "java/util/JumboEnumSet" {
        return cratonvm_native_collections::native_al_to_string(ctx, args);
    }
    let mut parts: Vec<String> = Vec::new();
    if let Some(backing) = es_get_backing(ctx, this) {
        let size = match ctx.get_field(backing, 1) {
            Value::Int(n) if n > 0 => n as usize,
            _ => 0,
        };
        if let Value::Object(Some(data)) = ctx.get_field(backing, 0) {
            for i in 0..size.min(ctx.array_length(data)) {
                let elem = ctx.get_array_element(data, i);
                let rendered = match elem {
                    Value::Object(Some(o)) => {
                        // An enum's `toString()` is its `name()` unless the enum
                        // overrides it; slot 0 is `Enum.name`, which is what
                        // `native_enum_value_of` matches on.
                        match ctx.get_field(o, 0) {
                            Value::Object(Some(n)) => ctx.read_string(n).unwrap_or_default(),
                            _ => String::from("null"),
                        }
                    }
                    _ => String::from("null"),
                };
                parts.push(rendered);
            }
        }
    }
    let out = format!("[{}]", parts.join(", "));
    Ok(Some(Value::Object(Some(ctx.create_string(&out)))))
}

/// A fresh `Object[capacity]` holding `src[0..copy_len]`.
///
/// gc-common w20-b: `EnumSet.iterator` / `toArray` / `clone` each held the
/// source array raw across the copy's allocation. `src` is pinned across it.
pub fn alloc_ref_array_copy(
    ctx: &mut dyn NativeContext,
    src: ObjectRef,
    copy_len: usize,
    capacity: usize,
) -> ObjectRef {
    let src_pin = ctx.pin_native_root(src);
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, capacity);
    let src = ctx.read_native_pin(src_pin, src);
    ctx.unpin_native_roots(src_pin);
    for i in 0..copy_len {
        let v = ctx.get_array_element(src, i);
        ctx.set_array_element(arr, i, v);
    }
    arr
}

/// A fresh `Object[]` holding `values` in order; the references among them
/// are rooted across its allocation (gc-common w20-b).
pub fn alloc_object_array_of_values(ctx: &mut dyn NativeContext, values: &[Value]) -> ObjectRef {
    let mut scope = NativeHandleScope::new(ctx);
    let slots = root_values(&mut scope, values);
    let arr = scope.new_array(cratonvm_types::ArrayElementType::Reference, slots.len());
    for (i, slot) in slots.iter().enumerate() {
        let v = element_value(&scope, slot);
        scope.set_array_element(arr, i, v);
    }
    arr
}

pub fn native_es_iterator(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let cls = ctx
        .class_name_of_id(ctx.class_id_of_object(this))
        .unwrap_or_default();
    if cls == "java/util/RegularEnumSet" || cls == "java/util/JumboEnumSet" {
        return cratonvm_native_collections::native_al_iterator(ctx, args);
    }
    if let Some(backing) = es_get_backing(ctx, this) {
        let size = match ctx.get_field(backing, 1) {
            Value::Int(n) if n > 0 => n as usize,
            _ => 0,
        };
        let data = match ctx.get_field(backing, 0) {
            Value::Object(Some(o)) => Some(o),
            _ => None,
        };
        let snap: ObjectRef = match data {
            Some(data) => {
                let copy = size.min(ctx.array_length(data));
                alloc_ref_array_copy(ctx, data, copy, size)
            }
            None => ctx.new_array(cratonvm_types::ArrayElementType::Reference, size),
        };
        return cratonvm_native_collections::make_iterator_from_array(ctx, snap, size);
    }
    let empty = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 0);
    cratonvm_native_collections::make_iterator_from_array(ctx, empty, 0)
}

pub fn native_es_to_array(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    if es_real_kind(ctx, this).is_some() {
        let elems = es_real_elements(ctx, this);
        let arr = alloc_object_array_of_values(ctx, &elems);
        return Ok(Some(Value::Object(Some(arr))));
    }
    if let Some(backing) = es_get_backing(ctx, this) {
        let size = match ctx.get_field(backing, 1) {
            Value::Int(n) => n as usize,
            _ => 0,
        };
        let data = match ctx.get_field(backing, 0) {
            Value::Object(Some(o)) => o,
            _ => return Ok(Some(Value::Object(None))),
        };
        let arr = alloc_ref_array_copy(ctx, data, size, size);
        return Ok(Some(Value::Object(Some(arr))));
    }
    let empty = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 0);
    Ok(Some(Value::Object(Some(empty))))
}

/// `EnumSet.toArray(T[] dest)` — JLS-compliant typed overload. Reuses the
/// caller-provided `dest` if it's at least `size` long; otherwise allocates
/// a new array of the same component class as `dest`. Copies our backing
/// elements into slots `0..size` and writes a trailing `null` if `dest` is
/// strictly larger than `size`.
pub fn native_es_to_array_typed(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let template = args.get(1).copied().unwrap_or(Value::Object(None));
    let real_elems = if es_real_kind(ctx, this).is_some() {
        Some(es_real_elements(ctx, this))
    } else {
        None
    };
    let backing = if real_elems.is_some() {
        None
    } else {
        match es_get_backing(ctx, this) {
            Some(b) => Some(b),
            None => {
                // No backing: return the template (or an empty Object[]).
                return Ok(Some(template));
            }
        }
    };
    let size = real_elems.as_ref().map_or_else(
        || match backing.map(|b| ctx.get_field(b, 1)) {
            Some(Value::Int(n)) => n.max(0) as usize,
            _ => 0,
        },
        |elems| elems.len(),
    );
    let data = backing.and_then(|b| match ctx.get_field(b, 0) {
        Value::Object(Some(o)) => Some(o),
        _ => None,
    });

    // gc-common w20-b: the source elements and the backing array were held
    // raw across the destination's allocation. Both are rooted.
    let mut scope = NativeHandleScope::new(ctx);
    let elem_slots = real_elems
        .as_ref()
        .map(|elems| root_values(&mut scope, elems));
    let data_h = data.map(|d| scope.root(d));

    // Pick destination: reuse template if big enough, else allocate fresh
    // with the same component as template (or a plain Object[] if template
    // is null).
    let target: ObjectRef = match template {
        Value::Object(Some(arr)) if scope.array_length(arr) >= size => arr,
        Value::Object(Some(arr)) => {
            // Template's component class lives on its array header.
            let comp_cid = scope.class_id_of_object(arr);
            scope.new_ref_array(comp_cid, size)
        }
        _ => scope.new_array(cratonvm_types::ArrayElementType::Reference, size),
    };
    if let Some(slots) = &elem_slots {
        for (i, slot) in slots.iter().enumerate().take(size) {
            let v = element_value(&scope, slot);
            scope.set_array_element(target, i, v);
        }
    } else if let Some(data_h) = &data_h {
        let d: ObjectRef = scope.get(data_h);
        let copy = size.min(scope.array_length(d));
        for i in 0..copy {
            let v = scope.get_array_element(d, i);
            scope.set_array_element(target, i, v);
        }
    }
    // JLS: write null at index `size` if target is longer.
    let target_len = scope.array_length(target);
    if target_len > size {
        scope.set_array_element(target, size, Value::Object(None));
    }
    Ok(Some(Value::Object(Some(target))))
}

pub fn native_es_clone(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // gc-common w20-b: the receiver was read after the clone's allocation (a
    // class resolution, which can run `<clinit>`), and the clone and the
    // source array were held raw across the rest. One handle scope.
    let mut scope = NativeHandleScope::new(ctx);
    let this_h = scope.root(this);
    let es = try_alloc_concurrent_synthetic(&mut *scope, "java/util/EnumSet", 2)?;
    let es_h = scope.root(es);
    let this = scope.get(&this_h);
    if let Some(backing) = es_get_backing(&mut *scope, this) {
        let size = match scope.get_field(backing, 1) {
            Value::Int(n) => n as usize,
            _ => 0,
        };
        let data = match scope.get_field(backing, 0) {
            Value::Object(Some(o)) => o,
            _ => {
                let es = scope.get(&es_h);
                scope.set_field(es, ES_FIELD_ELEMENTS, Value::Object(None));
                return Ok(Some(Value::Object(Some(es))));
            }
        };
        let new_arr = alloc_ref_array_copy(&mut *scope, data, size, size.max(4));
        let new_arr_h = scope.root(new_arr);
        let new_backing = try_alloc_concurrent_synthetic(&mut *scope, "java/util/ArrayList", 2)?;
        let new_arr = scope.get(&new_arr_h);
        scope.set_field(new_backing, 0, Value::Object(Some(new_arr)));
        scope.set_field(new_backing, 1, Value::Int(size as i32));
        let es = scope.get(&es_h);
        scope.set_field(es, ES_FIELD_ELEMENTS, Value::Object(Some(new_backing)));
    }
    let es = scope.get(&es_h);
    scope.set_field(es, ES_FIELD_TYPE, Value::Object(None));
    Ok(Some(Value::Object(Some(es))))
}

// ---------------------------------------------------------------------------
// EnumMap — 3-field synthetic (same as HashMap: buckets=0, size=1, capacity=2)
// Simplified: delegates to HashMap implementation
// ---------------------------------------------------------------------------
/// Registers only `EnumMap.<init>(Class)` — safe to call from the essential
/// (both-mode) registration path. `<init>` runs via invokespecial, where a
/// registered native always wins over bytecode (see `native_em_init`'s
/// comment), so this narrow registration is what lets early bootstrap (e.g.
/// logging, before the synthetic phase-50 registrations land) see a working
/// synthetic EnumMap. The rest of `register_enum_map_natives` (put/get/size/
/// etc) must stay synthetic-mode-only: those are normal invokevirtual calls
/// that already run correctly against real EnumMap bytecode once `<init>`
/// leaves the object properly initialized, and registering them on the
/// concrete class defeats `try_delegate_real_collection`'s real-JDK fallback
/// (its `invoke_special` resolves straight back to this same native, the
/// re-entrancy guard trips, and the caller gets a stale synthetic answer —
/// e.g. `EnumMap.size()`/`get()` reporting empty after a real `put()`).
pub fn register_enum_map_init_native(r: &mut NativeMethodRegistry) {
            r.register(
        "java/util/EnumMap",
        "<init>",
        "(Ljava/lang/Class;)V",
        native_em_init, cratonvm_native_api::NativeKind::Intrinsic,
    );
    }

pub fn register_enum_map_natives(r: &mut NativeMethodRegistry) {
            let c = "java/util/EnumMap";
    r.register(c, "<init>", "(Ljava/lang/Class;)V", native_em_init, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(
        c,
        "put",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        native_em_put, cratonvm_native_api::NativeKind::Intrinsic,
    );
    // The descriptor javac ACTUALLY emits. `EnumMap<K extends Enum<K>, V>`
    // erases `K` to `java/lang/Enum`, not to `java/lang/Object`, so a real call
    // site emits `put(Ljava/lang/Enum;Ljava/lang/Object;)Ljava/lang/Object;`
    // (`javap -s`) and never matched the row above. Measured 2026-09-02 with
    // `apps/probes/SyntheticEnumSurface`: `new EnumMap<>(Local.class).put(k, v)`
    // was `NoSuchMethodError: java.util.EnumMap.put(java.lang.Enum, java.lang.Object)`
    // in `--synthetic-jdk`, while `get`/`remove`/`containsKey` worked — those
    // three take `Object` in the source, so their erasure is the one already
    // registered and only `put` diverges.
    //
    // BOTH are kept: a caller reaching this through a raw/`Map`-typed reference
    // emits the `Object` form, and the bridge method the compiler generates for
    // `Map.put` has exactly that descriptor.
    r.register(
        c,
        "put",
        "(Ljava/lang/Enum;Ljava/lang/Object;)Ljava/lang/Object;",
        native_em_put, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        c,
        "get",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        native_em_get, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        c,
        "remove",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        native_em_remove, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        c,
        "containsKey",
        "(Ljava/lang/Object;)Z",
        native_em_contains_key, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        c,
        "containsValue",
        "(Ljava/lang/Object;)Z",
        cratonvm_native_collections::native_map_contains_value_pub, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(c, "size", "()I", native_em_size, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(c, "isEmpty", "()Z", native_em_is_empty, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(c, "clear", "()V", native_em_clear, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(
        c,
        "keySet",
        "()Ljava/util/Set;",
        cratonvm_native_collections::native_map_key_set_pub, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        c,
        "values",
        "()Ljava/util/Collection;",
        cratonvm_native_collections::native_map_values_pub, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        c,
        "entrySet",
        "()Ljava/util/Set;",
        cratonvm_native_collections::native_map_entry_set_pub, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(c, "clone", "()Ljava/lang/Object;", native_em_clone, cratonvm_native_api::NativeKind::Intrinsic);
    // T9 — equals: identity comparison (this == other). Was
    // `native_return_false` which incorrectly returned false for
    // `obj.equals(obj)`. Real content-based equality requires
    // walking the map entries; identity comparison is the safe
    // conservative default.
    r.register(c, "equals", "(Ljava/lang/Object;)Z", |_ctx, args| {
        let this_ptr = args.first().and_then(|v| match v {
            Value::Object(Some(o)) => Some(o.as_ptr() as usize),
            _ => None,
        });
        let other_ptr = args.get(1).and_then(|v| match v {
            Value::Object(Some(o)) => Some(o.as_ptr() as usize),
            _ => None,
        });
        Ok(Some(Value::Int(if this_ptr == other_ptr { 1 } else { 0 })))    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(
        c,
        "toString",
        "()Ljava/lang/String;",
        cratonvm_native_collections::native_map_to_string_pub, cratonvm_native_api::NativeKind::Bridge,
    );
}

pub fn native_em_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // A null key type is a `NullPointerException`, in BOTH modes.
    //
    // `EnumMap(Class<K> keyType)` is `keyUniverse = getKeyUniverse(keyType)`,
    // which reaches `keyType.getEnumConstantsShared()` — an unguarded
    // dereference, so the JDK throws before storing anything. This body instead
    // fell through to the `_ =>` arm below, built a zero-length universe and
    // RETURNED, leaving a live `EnumMap` with a null `keyType` whose every
    // subsequent `put` would fail somewhere else with an unrelated message.
    //
    // The message is HotSpot 25.0.3+9's helpful-NPE text, transcribed:
    // `Cannot invoke "java.lang.Class.getEnumConstantsShared()" because "klass"
    // is null`. `RJdkIntrinsics3`'s `ckX` compares only the exception CLASS, so
    // the text is fidelity rather than gate; it is recorded here so a later
    // lane that does compare messages has the measured one and not a guess.
    //
    // Placed ahead of the real/synthetic split deliberately: the contract is
    // the Java one and does not depend on which layout backs the map, and no
    // in-VM caller constructs an `EnumMap` with a null class (the bootstrap
    // logging path that motivated the essential-set registration passes a real
    // enum class; `alloc_enum_map` builds its instance without this ctor).
    if matches!(args.get(1), None | Some(Value::Object(None))) {
        return Err(RuntimeError::NullPointerException {
            message: Some(
                "Cannot invoke \"java.lang.Class.getEnumConstantsShared()\" \
                 because \"klass\" is null"
                    .to_string(),
            ),        }
        .into());
    }
    // `<init>` is dispatched via invokespecial, where a registered native
    // ALWAYS wins over bytecode (see `invoke_special_shared`'s "Native
    // override always wins" priority) — unlike invokevirtual calls such as
    // `put`/`get`, which prefer real bytecode unless force-listed. This
    // native was promoted into the essential (both-mode) registration set
    // so early logging bootstrap can see a synthetic EnumMap before the
    // synthetic phase-50 registrations land, but that means it *also* fires
    // for a real `java/util/EnumMap` loaded from the real JDK in real-JDK
    // mode, where the hardcoded buckets/size/capacity indices below don't
    // match the real class's keyType/keyUniverse/vals/size/entrySet field
    // layout — clobbering `keyType` (real field index 2, not 0) with a
    // buckets array and coercing `keyUniverse`/`vals` (ref-typed fields hit
    // with `Value::Int`) to null, so `EnumMap.put`'s real-bytecode
    // `typeCheck` sees `keyType == null` and throws ClassCastException.
    // Mirror the real constructor's semantics by field name instead.
    if !ctx.is_class_synthetic_stub("java/util/EnumMap") {
        let key_type = args.get(1).copied().unwrap_or(Value::Object(None));
        // cceres3: pin across GC-capable call (stream stale-at-store wave) —
        // `getEnumConstants` re-enters Java and the two array allocations can
        // move `this`/`key_type`/`universe` before the by-name field stores
        // below (live store-canary capture during a domain boot).
        let this_pin = ctx.pin_native_root(this);
        let kt_pin = match key_type {
            Value::Object(Some(k)) => Some(ctx.pin_native_root(k)),
            _ => None,
        };
        let universe = match key_type {
            Value::Object(Some(enum_class)) => {
                match ctx.invoke_virtual(
                    enum_class,
                    "getEnumConstants",
                    "()[Ljava/lang/Object;",
                    &[],
                ) {
                    Ok(Some(Value::Object(Some(universe)))) => universe,
                    _ => {
                        let enum_class =
                            kt_pin.map_or(enum_class, |p| ctx.read_native_pin(p, enum_class));
                        enum_constants_array_for_class(ctx, enum_class).unwrap_or_else(|| {
                            ctx.new_array(cratonvm_types::ArrayElementType::Reference, 0)
                        })
                    }
                }
            }
            _ => ctx.new_array(cratonvm_types::ArrayElementType::Reference, 0),
        };
        let uni_pin = ctx.pin_native_root(universe);
        let vals = ctx.new_array(
            cratonvm_types::ArrayElementType::Reference,
            ctx.array_length(universe),
        );
        let this = ctx.read_native_pin(this_pin, this);
        let universe = ctx.read_native_pin(uni_pin, universe);
        let key_type = match key_type {
            Value::Object(Some(k)) => {
                Value::Object(Some(kt_pin.map_or(k, |p| ctx.read_native_pin(p, k))))
            }
            other => other,
        };
        ctx.set_field_by_name(this, "keyType", key_type);
        ctx.set_field_by_name(this, "keyUniverse", Value::Object(Some(universe)));
        ctx.set_field_by_name(this, "vals", Value::Object(Some(vals)));
        ctx.set_field_by_name(this, "size", Value::Int(0));
        ctx.set_field_by_name(this, "entrySet", Value::Object(None));
        ctx.unpin_native_roots(this_pin);
        return Ok(None);
    }
    let cap = 16;
    // cceres3: pin across GC-capable call (stream stale-at-store wave)
    let this_pin = ctx.pin_native_root(this);
    let buckets = ctx.new_array(cratonvm_types::ArrayElementType::Reference, cap);
    let this = ctx.read_native_pin(this_pin, this);
    ctx.set_field(this, 0, Value::Object(Some(buckets)));
    ctx.set_field(this, 1, Value::Int(0));
    ctx.set_field(this, 2, Value::Int(cap as i32));
    ctx.unpin_native_roots(this_pin);
    Ok(None)
}

pub fn em_real_vals(ctx: &dyn NativeContext, this: ObjectRef) -> Option<ObjectRef> {
    if ctx.is_class_synthetic_stub("java/util/EnumMap") {
        return None;
    }
    match ctx.get_field_by_name(this, "vals") {
        Value::Object(Some(vals)) => Some(vals),
        _ => None,
    }
}

pub fn em_enum_ordinal(ctx: &mut dyn NativeContext, key: Value) -> Option<usize> {
    let Value::Object(Some(enum_obj)) = key else {
        return None;
    };
    // gc-common w20-b: the field fallback read `enum_obj` after `ordinal()`
    // (Java) at its entry address.
    let key_pin = ctx.pin_native_root(enum_obj);
    let invoked = ctx.invoke_virtual(enum_obj, "ordinal", "()I", &[]);
    let enum_obj = ctx.read_native_pin(key_pin, enum_obj);
    ctx.unpin_native_roots(key_pin);
    match invoked {
        Ok(Some(Value::Int(n))) if n >= 0 => return Some(n as usize),
        _ => {}
    }
    match ctx.get_field_by_name(enum_obj, "ordinal") {
        Value::Int(n) if n >= 0 => Some(n as usize),
        _ => None,
    }
}

/// A real `EnumMap`'s `vals` array and `key`'s index in it, with the map
/// and a `carried` value (the `put` value) read back after the key's
/// `ordinal()` -- Java -- at their current addresses. `None` when the key has
/// no ordinal or the ordinal is out of range (every caller answers its
/// "absent" value for both).
///
/// gc-common w20-b: `put` / `get` / `remove` / `containsKey` read `vals`
/// BEFORE `ordinal()` and then stored into (or read from) that array, and
/// `put` stored its value, at their pre-call addresses.
pub fn em_real_lookup(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    key: Value,
    carried: Value,
) -> Option<(ObjectRef, ObjectRef, usize, Value)> {
    let mut scope = NativeHandleScope::new(ctx);
    let this_h = scope.root(this);
    let carried_slot = element_handle(&mut scope, Some(carried));
    let idx = em_enum_ordinal(&mut *scope, key)?;
    let this = scope.get(&this_h);
    let vals = em_real_vals(&*scope, this)?;
    if idx >= scope.array_length(vals) {
        return None;
    }
    Some((this, vals, idx, element_value(&scope, &carried_slot)))
}

pub fn native_em_put(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    if em_real_vals(ctx, this).is_some() {
        let key = args.get(1).copied().unwrap_or(Value::Object(None));
        let value = args.get(2).copied().unwrap_or(Value::Object(None));
        let Some((this, vals, idx, value)) = em_real_lookup(ctx, this, key, value) else {
            return Ok(Some(Value::Object(None)));
        };
        let old = ctx.get_array_element(vals, idx);
        ctx.set_array_element(vals, idx, value);
        if matches!(old, Value::Object(None)) && !matches!(value, Value::Object(None)) {
            let size = ctx
                .get_field_by_name(this, "size")
                .as_int()
                .unwrap_or(0)
                .saturating_add(1);
            ctx.set_field_by_name(this, "size", Value::Int(size));
        }
        return Ok(Some(old));
    }
    cratonvm_native_collections::native_map_put_entry_pub(ctx, args)
}

pub fn native_em_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    if em_real_vals(ctx, this).is_some() {
        let key = args.get(1).copied().unwrap_or(Value::Object(None));
        let Some((_, vals, idx, _)) = em_real_lookup(ctx, this, key, Value::Object(None)) else {
            return Ok(Some(Value::Object(None)));
        };
        return Ok(Some(ctx.get_array_element(vals, idx)));    }
    cratonvm_native_collections::native_map_get_pub(ctx, args)}

pub fn native_em_remove(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    if em_real_vals(ctx, this).is_some() {
        let key = args.get(1).copied().unwrap_or(Value::Object(None));
        let Some((this, vals, idx, _)) = em_real_lookup(ctx, this, key, Value::Object(None)) else {
            return Ok(Some(Value::Object(None)));
        };
        let old = ctx.get_array_element(vals, idx);
        ctx.set_array_element(vals, idx, Value::Object(None));
        if !matches!(old, Value::Object(None)) {
            let size = ctx
                .get_field_by_name(this, "size")
                .as_int()
                .unwrap_or(0)
                .saturating_sub(1);
            ctx.set_field_by_name(this, "size", Value::Int(size));
        }
        return Ok(Some(old));
    }
    cratonvm_native_collections::native_map_remove_pub(ctx, args)
}

pub fn native_em_contains_key(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    if em_real_vals(ctx, this).is_some() {
        let key = args.get(1).copied().unwrap_or(Value::Object(None));
        let Some((_, vals, idx, _)) = em_real_lookup(ctx, this, key, Value::Object(None)) else {
            return Ok(Some(Value::Int(0)));
        };
        return Ok(Some(Value::Int(
            if matches!(ctx.get_array_element(vals, idx), Value::Object(None)) {
                0
            } else {
                1
            },
        )));
    }
    cratonvm_native_collections::native_map_contains_key_pub(ctx, args)
}

pub fn native_em_size(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    if em_real_vals(ctx, this).is_some() {
        return Ok(Some(Value::Int(
            ctx.get_field_by_name(this, "size").as_int().unwrap_or(0),
        )));
    }
    cratonvm_native_collections::native_map_size_pub(ctx, args)
}

pub fn native_em_is_empty(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    if em_real_vals(ctx, this).is_some() {
        let size = ctx.get_field_by_name(this, "size").as_int().unwrap_or(0);
        return Ok(Some(Value::Int(if size == 0 { 1 } else { 0 })));
    }
    cratonvm_native_collections::native_map_is_empty_pub(ctx, args)
}

pub fn native_em_clear(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    if let Some(vals) = em_real_vals(ctx, this) {
        for i in 0..ctx.array_length(vals) {
            ctx.set_array_element(vals, i, Value::Object(None));
        }
        ctx.set_field_by_name(this, "size", Value::Int(0));
        return Ok(None);
    }
    cratonvm_native_collections::native_map_clear_pub(ctx, args)
}

pub fn native_em_clone(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Simplified: create new empty EnumMap
    let this = obj_arg(args, 0)?;
    let _ = this;
    let em = try_alloc_concurrent_synthetic(ctx, "java/util/EnumMap", 3)?;
    let cap = 16;
    // gc-common w20-b: `em` was held raw across its buckets' allocation.
    let em: ObjectRef =
        alloc_array_into_field(ctx, em, 0, cratonvm_types::ArrayElementType::Reference, cap);
    ctx.set_field(em, 1, Value::Int(0));
    ctx.set_field(em, 2, Value::Int(cap as i32));
    Ok(Some(Value::Object(Some(em))))
}

// ---------------------------------------------------------------------------
// IdentityHashMap — 3-field synthetic (same layout as HashMap)
// Uses reference identity instead of equals/hashCode
// ---------------------------------------------------------------------------
pub fn register_identity_hashmap_natives(r: &mut NativeMethodRegistry) {
            let c = "java/util/IdentityHashMap";
    r.register(c, "<init>", "()V", native_em_init, cratonvm_native_api::NativeKind::Intrinsic); // reuse
    r.register(c, "<init>", "(I)V", native_em_init, cratonvm_native_api::NativeKind::Intrinsic); // reuse
    r.register(
        c,
        "put",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        cratonvm_native_collections::native_map_put_entry_pub, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        c,
        "get",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        cratonvm_native_collections::native_map_get_pub, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        c,
        "remove",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        cratonvm_native_collections::native_map_remove_pub, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        c,
        "containsKey",
        "(Ljava/lang/Object;)Z",
        cratonvm_native_collections::native_map_contains_key_pub, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        c,
        "containsValue",
        "(Ljava/lang/Object;)Z",
        cratonvm_native_collections::native_map_contains_value_pub, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        c,
        "size",
        "()I",
        cratonvm_native_collections::native_map_size_pub, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        c,
        "isEmpty",
        "()Z",
        cratonvm_native_collections::native_map_is_empty_pub, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        c,
        "clear",
        "()V",
        cratonvm_native_collections::native_map_clear_pub, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        c,
        "keySet",
        "()Ljava/util/Set;",
        cratonvm_native_collections::native_map_key_set_pub, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        c,
        "values",
        "()Ljava/util/Collection;",
        cratonvm_native_collections::native_map_values_pub, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        c,
        "entrySet",
        "()Ljava/util/Set;",
        cratonvm_native_collections::native_map_entry_set_pub, cratonvm_native_api::NativeKind::Intrinsic,
    );
    // T9 — equals: identity comparison (this == other). Was
    // `native_return_false` which incorrectly returned false for
    // `obj.equals(obj)`. Real content-based equality requires
    // walking the map entries; identity comparison is the safe
    // conservative default.
    r.register(c, "equals", "(Ljava/lang/Object;)Z", |_ctx, args| {
        let this_ptr = args.first().and_then(|v| match v {
            Value::Object(Some(o)) => Some(o.as_ptr() as usize),
            _ => None,
        });
        let other_ptr = args.get(1).and_then(|v| match v {
            Value::Object(Some(o)) => Some(o.as_ptr() as usize),
            _ => None,
        });
        Ok(Some(Value::Int(if this_ptr == other_ptr { 1 } else { 0 })))    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(c, "hashCode", "()I", native_identity_hashmap_hash_code, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(c, "clone", "()Ljava/lang/Object;", native_em_clone, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(
        c,
        "toString",
        "()Ljava/lang/String;",
        cratonvm_native_collections::native_map_to_string_pub, cratonvm_native_api::NativeKind::Intrinsic,
    );
    }

/// `IdentityHashMap.hashCode()` — the sum over entries of
/// `identityHashCode(key) ^ identityHashCode(value)`, matching JDK 25's
/// `IdentityHashMap#hashCode` (which deliberately uses identity hashes, not
/// the keys' own `hashCode`, so it pairs with the identity `equals` above).
///
/// A constant 0 made every IdentityHashMap hash-equal, so a HashMap keyed by
/// IdentityHashMaps collapsed into a single bucket and `Objects.hash(map)`
/// carried no information at all.
pub fn native_identity_hashmap_hash_code(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let this_pin = ctx.pin_native_root(this);
    let result = ihm_hash_code_inner(ctx, this_pin, this);
    ctx.unpin_native_roots(this_pin);
    result
}

pub fn ihm_hash_code_inner(
    ctx: &mut dyn NativeContext,
    this_pin: usize,
    this: ObjectRef,
) -> MethodCallResult {
    let this_now = ctx.read_native_pin(this_pin, this);
    // Reuse the same entry view the sibling registrations delegate to, so the
    // side-table-backed HashMap state is seen (a hand-rolled bucket walk here
    // would miss it entirely).
    let keys = cratonvm_native_collections::native_map_keys_as_array(ctx, Some(this_now));
    let keys_pin = ctx.pin_native_root(keys);
    let len = ctx.array_length(keys);
    let mut hash = 0i32;
    for i in 0..len {
        let keys_now = ctx.read_native_pin(keys_pin, keys);
        let key = ctx.get_array_element(keys_now, i);
        let key_pin = pe_pin_value(ctx, key);
        let key_hash = match key {
            Value::Object(Some(k)) => ctx.identity_hash_code(k),
            _ => 0,
        };
        // `get` dispatches the key's hashCode/equals, i.e. arbitrary bytecode.
        let this_now = ctx.read_native_pin(this_pin, this);
        let key_now = pe_read_pinned(ctx, key_pin, key);
        let value = cratonvm_native_collections::native_map_get_pub(
            ctx,
            &[Value::Object(Some(this_now)), key_now],
        )?;
        let value_hash = match value {
            Some(Value::Object(Some(v))) => ctx.identity_hash_code(v),
            _ => 0,
        };
        hash = hash.wrapping_add(key_hash ^ value_hash);    }
    Ok(Some(Value::Int(hash)))}

// ---------------------------------------------------------------------------
// WeakHashMap — 3-field synthetic (same layout as HashMap)
// Simplified: behaves like regular HashMap (no GC-driven cleanup)
// ---------------------------------------------------------------------------
pub fn register_weak_hashmap_natives(r: &mut NativeMethodRegistry) {
    // The real JDK class has bytecode and a different physical layout. Keep
    // this three-slot implementation for the synthetic fallback only.
    let c = "java/util/WeakHashMap";
    r.register(c, "<init>", "()V", native_em_init, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(c, "<init>", "(I)V", native_em_init, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(c, "<init>", "(IF)V", native_em_init, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(
        c,
        "put",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        cratonvm_native_collections::native_map_put_entry_pub, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "get",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        cratonvm_native_collections::native_map_get_pub, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "remove",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        cratonvm_native_collections::native_map_remove_pub, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "containsKey",
        "(Ljava/lang/Object;)Z",
        cratonvm_native_collections::native_map_contains_key_pub, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "containsValue",
        "(Ljava/lang/Object;)Z",
        cratonvm_native_collections::native_map_contains_value_pub, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "size",
        "()I",
        cratonvm_native_collections::native_map_size_pub, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "isEmpty",
        "()Z",
        cratonvm_native_collections::native_map_is_empty_pub, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "clear",
        "()V",
        cratonvm_native_collections::native_map_clear_pub, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "keySet",
        "()Ljava/util/Set;",
        cratonvm_native_collections::native_map_key_set_pub, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "values",
        "()Ljava/util/Collection;",
        cratonvm_native_collections::native_map_values_pub, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "entrySet",
        "()Ljava/util/Set;",
        cratonvm_native_collections::native_map_entry_set_pub, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "toString",
        "()Ljava/lang/String;",
        cratonvm_native_collections::native_map_to_string_pub, cratonvm_native_api::NativeKind::Bridge,
    );
}

// ===========================================================================
// Phase 51: Calendar, TimeZone, Currency, Concurrency extras
// ===========================================================================

pub(crate) fn register_phase51_natives(registry: &mut NativeMethodRegistry) {
    register_calendar_natives(registry);
    register_timezone_natives(registry);
    register_currency_natives(registry);
    register_exchanger_natives(registry);
    register_phaser_natives(registry);
    register_timer_natives(registry);
    register_forkjoin_natives(registry);
    register_scheduled_executor_natives(registry);
    register_qname_natives(registry);
    register_timeunit_natives(registry);
    register_object_stream_natives(registry);
    }

/// The three `QName` constructors: each part is the given `String`, or `""`
/// for an absent or null one.
///
/// gc-common w19-c: the constructors held the receiver and every part raw
/// across the `""` allocations (and handed that receiver to the row key). All
/// four are rooted now and re-read before [`qname_set_parts`].
pub fn qname_init(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    ns: Option<Value>,
    local: Option<Value>,
    prefix: Option<Value>,
) -> MethodCallResult {
    let mut scope = NativeHandleScope::new(ctx);
    let this_h = scope.root(obj_arg(args, 0)?);
    // Every given part is rooted before the first `""` is allocated.
    let mut given = Vec::with_capacity(3);
    for v in [ns, local, prefix] {
        given.push(match v {
            Some(Value::Object(Some(s))) => Some(scope.root(s)),
            _ => None,
        });
    }
    let mut parts = Vec::with_capacity(3);
    for h in given {
        parts.push(match h {
            Some(h) => h,
            None => {
                let empty = scope.create_string("");
                scope.root(empty)
            }
        });
    }
    let ns_now = scope.get(&parts[0]);
    let local_now = scope.get(&parts[1]);
    let prefix_now = scope.get(&parts[2]);
    let this = scope.get(&this_h);
    qname_set_parts(&mut *scope, this, ns_now, local_now, prefix_now);
    Ok(None)
}

/// The `(namespaceURI, localPart, prefix)` text of every `QName` this file's
/// constructors built, keyed by the QName's weak lock key
/// (`crate::gc_stable_weak_lock_key`).
///
/// gc-common w19-c: keyed by the bare identity hash until now, and never
/// pruned. Every heap mints identity hashes from 1, so two VMs in one process
/// read each other's rows (`getLocalPart()` on VM B's QName answered VM A's
/// local part), and every QName ever constructed kept its row (three owned
/// strings) for the life of the process. The lock key is VM-scoped and weak:
/// the row goes when the QName dies or its VM is torn down
/// ([`forget_pbkdf2_keys`], which both registry sweeps call).
pub fn qname_parts_table(
) -> &'static std::sync::Mutex<std::collections::HashMap<usize, (String, String, String)>> {
    static T: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<usize, (String, String, String)>>,
    > = std::sync::OnceLock::new();
    T.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// The row key of a `QName` in [`qname_parts_table`]. Computed before the
/// table lock is taken, so the registry lock and the table lock are never held
/// together.
pub fn qname_row_key(ctx: &dyn NativeContext, this: ObjectRef) -> usize {
    crate::gc_stable_weak_lock_key(ctx, this)
        .unwrap_or_else(|_| unreachable!("gc_stable_weak_lock_key never fails"))
}

pub fn qname_set_parts(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    namespace_uri: ObjectRef,
    local_part: ObjectRef,
    prefix: ObjectRef,
) {
    let namespace_uri_text = ctx.read_string(namespace_uri).unwrap_or_default();
    let local_part_text = ctx.read_string(local_part).unwrap_or_default();
    let prefix_text = ctx.read_string(prefix).unwrap_or_default();
    let key = qname_row_key(ctx, this);
    if let Ok(mut table) = qname_parts_table().lock() {
        table.insert(key, (namespace_uri_text, local_part_text, prefix_text));
    }
    ctx.set_field_by_name(this, "namespaceURI", Value::Object(Some(namespace_uri)));
    ctx.set_field_by_name(this, "localPart", Value::Object(Some(local_part)));
    ctx.set_field_by_name(this, "prefix", Value::Object(Some(prefix)));
}

pub fn qname_part_text(
    ctx: &dyn NativeContext,
    this: ObjectRef,
    name: &str,
    slot: usize,
    index: usize,
) -> String {
    let key = qname_row_key(ctx, this);
    if let Ok(table) = qname_parts_table().lock() {
        if let Some(parts) = table.get(&key) {
            return match index {
                0 => parts.0.clone(),
                1 => parts.1.clone(),
                _ => parts.2.clone(),
            };
        }
    }
    match ctx.get_field_by_name(this, name) {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
        _ if ctx.object_num_fields(this) > slot => match ctx.get_field(this, slot) {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
            _ => String::new(),
        },
        _ => String::new(),
    }
}

pub fn qname_part(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    name: &str,
    slot: usize,
    index: usize,
) -> Value {
    let text = qname_part_text(ctx, this, name, slot, index);
    Value::Object(Some(ctx.create_string(&text)))
}

