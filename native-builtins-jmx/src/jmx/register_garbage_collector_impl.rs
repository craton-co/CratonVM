#[allow(unused_imports)]
use super::*;

/// `sun.management.GarbageCollectorImpl` — per-collector counters.
///
/// Each bean answers its own collector's count and time when the running
/// backend describes its beans (Generational, G1, ZGC), and the VM-wide
/// totals otherwise. (gci i14/mp: this doc said the counters were a flat 0;
/// they have been real since gen r4w2/obs.)
pub fn register_garbage_collector_impl(r: &mut NativeMethodRegistry) {
            let cls = "sun/management/GarbageCollectorImpl";

    // getCollectionCount — REAL: cumulative GC count from the VM's own
    // counter (`gc_collection_count`), the same source the Bridge
    // GarbageCollectorMXBean.getCollectionCount uses. Previously a
    // fabricated 0, which made H2's collectGarbage() delta-loop spin.
    //
    // gen r4w2/obs (2026-09-23): PER BEAN when the collector describes its
    // beans (`NativeContext::gc_collectors`): the generational backend's
    // `Copy` bean counts the collections that did not reclaim old gen and
    // `MarkSweepCompact` the ones that did, so the two sum to the collections
    // run and a full collection is no longer counted under a young bean.
    r.register(
        cls,
        "getCollectionCount",
        "()J",
        |ctx, args| {
            if let Ok(this) = obj_arg(args, 0) {
                if let Some(c) = gc_collector_info_for(ctx, this) {
                    return Ok(Some(Value::Long(
                        i64::try_from(c.count).unwrap_or(i64::MAX),
                    )));
                }
            }
            Ok(Some(Value::Long(ctx.gc_collection_count() as i64)))
        },
        cratonvm_native_api::NativeKind::Bridge,
    );
    // getCollectionTime — accumulated pause milliseconds where the backend
    // keeps them (generational, since 2026-09-23: each pause rounded UP to a
    // whole millisecond, so delta-based callers such as H2 still see it move
    // on every collection; per bean since round 4 wave 2), and the historical
    // count-as-time stand-in where it does not. See
    // `NativeContext::gc_collection_time_ms`.
    r.register(
        cls,
        "getCollectionTime",
        "()J",
        |ctx, args| {
            if let Ok(this) = obj_arg(args, 0) {
                if let Some(c) = gc_collector_info_for(ctx, this) {
                    return Ok(Some(Value::Long(
                        i64::try_from(c.time_ms).unwrap_or(i64::MAX),
                    )));
                }
            }
            Ok(Some(Value::Long(ctx.gc_collection_time_ms() as i64)))
        },
        cratonvm_native_api::NativeKind::Bridge,
    );

    // <init>(Ljava/lang/String;Lsun/management/VMManagement;)V — the `name`
    // argument is exactly what `getName()` below reads back, so discarding it
    // made every bean built through the real constructor answer `getName() ==
    // null` while only the ones built by `alloc_garbage_collector_impl`
    // reported a name. Store both arguments, mirroring the Java constructor.
    r.register(
        cls,
        "<init>",
        "(Ljava/lang/String;Lsun/management/VMManagement;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            init_memory_manager_fields(
                ctx,
                this,
                args.get(1).copied().unwrap_or(Value::Object(None)),
            );
            let jvm = args.get(2).copied().unwrap_or(Value::Object(None));
            ctx.set_field_by_name(this, "jvm", jvm);
            Ok(None)
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // JDK 25 also has the public `<init>(Ljava/lang/String;)V` form (used
    // by ManagementFactoryHelper internals) — same `name` handling, no
    // VMManagement to record.
    //
    // gcd d1/e (2026-09-27, gen r5w4/obs8): this body is only the stand-in for
    // a class with NO bytes, exactly as `MemoryPoolImpl.<init>` has been since
    // gen r5w4/obs8. With the JDK's `GarbageCollectorImpl` loaded, its own
    // constructor runs (`super(name)` -> `MemoryManagerImpl.<init>` ->
    // `NotificationEmitterSupport.<init>`, whose field initialisers build
    // `listenerLock` / `listenerList`); this body skipped that chain, so a
    // collector bean built by Java code (`GarbageCollectorExtImpl
    // .createGarbageCollector`, or user reflection) had a null `listenerLock`
    // until `init_notification_emitter_support` patched the VM-built ones.
    // (The `(String, VMManagement)` form above has no bytes in JDK 25, so it
    // stays a plain bridge.)
    r.register(cls, "<init>", "(Ljava/lang/String;)V", |ctx, args| {
        if !ctx.is_class_synthetic_stub("sun/management/GarbageCollectorImpl") {
            return ctx.invoke_special_bytecode_only(
                "sun/management/GarbageCollectorImpl",
                "<init>",
                "(Ljava/lang/String;)V",
                args,
            );
        }
        let this = obj_arg(args, 0)?;
        init_memory_manager_fields(
            ctx,
            this,
            args.get(1).copied().unwrap_or(Value::Object(None)),
        );
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);

    // Wave 1 / Task A: getName()Ljava/lang/String; — read the synthetic
    // `name` slot we populate in `alloc_garbage_collector_impl`. The
    // bytecode `MemoryManagerImpl.getName` does `getfield name` directly;
    // overriding natively keeps us robust against field-layout drift.
    r.register(cls, "getName", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field_by_name(this, "name")))
    }, cratonvm_native_api::NativeKind::Bridge);

    // gc()V — GarbageCollectorImpl's manual-trigger entry point, the same
    // request `MemoryMXBean.gc()` makes. Run the collector instead of
    // silently doing nothing: a caller asking for a collection and getting a
    // no-op has no way to tell, and the sibling `MemoryMXBean.gc()` native
    // already calls `force_gc()`.
    r.register(cls, "gc", "()V", |ctx, _args| {
        if let Some(h) = cratonvm_native_builtins::hooks::JFR_HOOKS.get() { (h.force_gc_as_system_gc)(ctx); };
        Ok(None)
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    // gen r4w3/obs2: the `com.sun.management` half of the same beans. Here
    // rather than at a new `vm_init` call site so both registration arms that
    // call this function pick it up. Sets and restores its own category.
    register_gc_notification_natives(r);
}

/// `sun.management.MemoryManagerImpl` — base class for memory managers.
///
/// Wave 1 / Task A: register `getName()` so the synthetic manager
/// instances returned by `MemoryImpl.getMemoryManagers0()` answer with
/// their populated `name` field. (`isValid()` defaults to `false` via
/// the synthetic field initialisation; we override it to `true` so
/// `ManagementFactoryHelper` doesn't filter the bean out.)
pub fn register_memory_manager_impl(r: &mut NativeMethodRegistry) {
            let cls = "sun/management/MemoryManagerImpl";

    // <init>(Ljava/lang/String;)V — the argument IS the manager name that
    // `getName()` below reads; the previous no-op threw it away, so a manager
    // constructed by real bytecode had a null name.
    //
    // gcd d1/e (2026-09-27, gen r5w4/obs8): only the stand-in for a class
    // with NO bytes. With the JDK's `MemoryManagerImpl` loaded, its own
    // constructor runs: `NotificationEmitterSupport.<init>` (the
    // `listenerLock` / `listenerList` initialisers, without which
    // `addNotificationListener` throws `NullPointerException`), then `name`,
    // `isValid = true`, `pools = null` -- what HotSpot builds.
    r.register(cls, "<init>", "(Ljava/lang/String;)V", |ctx, args| {
        if !ctx.is_class_synthetic_stub("sun/management/MemoryManagerImpl") {
            return ctx.invoke_special_bytecode_only(
                "sun/management/MemoryManagerImpl",
                "<init>",
                "(Ljava/lang/String;)V",
                args,
            );
        }
        let this = obj_arg(args, 0)?;
        init_memory_manager_fields(
            ctx,
            this,
            args.get(1).copied().unwrap_or(Value::Object(None)),
        );
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);

    r.register(cls, "getName", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field_by_name(this, "name")))
    }, cratonvm_native_api::NativeKind::Bridge);

    r.register(cls, "isValid", "()Z", native_manager_is_valid, cratonvm_native_api::NativeKind::Bridge);

    // getMemoryPools0()[Ljava/lang/management/MemoryPoolMXBean; — return
    // an empty array so the synthetic accessor doesn't trip on a missing
    // native.
    //
    // gen r4w2/obs (2026-09-23): a collector the running backend describes
    // answers with its real pools. This edge backs the pure-Java
    // `getMemoryPoolNames()` (`GarbageCollectorImpl` inherits it), so the
    // empty array made every collector report NO pools — the "pools=" column
    // of `GenR4MxbeanProbe.java` was blank.
    r.register(
        cls,
        "getMemoryPools0",
        "()[Ljava/lang/management/MemoryPoolMXBean;",
        |ctx, args| {
            let this = obj_arg(args, 0).ok();
            let collector = this.and_then(|this| gc_collector_info_for(&*ctx, this));
            // i7-L2: the empty answer carries the same component as the
            // populated one, not `Object`.
            let plain: Vec<&'static str> = match (&collector, this) {
                (None, Some(this)) => match bean_name(&*ctx, this) {
                    Some(m) => non_heap_manager_pools_of(&*ctx, &m),
                    None => Vec::new(),                },
                _ => Vec::new(),
            };
            let names: &[&'static str] = match collector {
                Some(c) => c.pool_names,
                // gci i15/nh: `CodeCacheManager` / `Metaspace Manager` manage
                // the code heaps / the metaspace pools. zs s3/ob2: on ZGC, the
                // VM-modelled pools that name the manager.
                None => &plain,
            };
            // gen r5w2/obs6: the SAME pool beans `MemoryImpl.getMemoryPools()`
            // handed out, when it has built them; fresh ones otherwise.
            let canonical = if names.is_empty() {
                None
            } else {
                registered_bean_array(ctx, "pools", "sun/management/MemoryPoolImpl", names)
            };
            let arr = match canonical {
                Some(arr) => arr,
                None => alloc_pool_bean_array(ctx, names)?,            };
            Ok(Some(Value::Object(Some(arr))))
        },
        cratonvm_native_api::NativeKind::Bridge,
    );
    }

/// `sun.management.MemoryPoolImpl` — per-pool metadata + usage.
///
/// Wave 1 / Task A: synthetic instances are built by
/// `alloc_memory_pool_impl` with `name` (string) + `isHeap` (boolean)
/// fields populated. Native overrides for `getName()` and `getType()`
/// short-circuit the bytecode so the probe sees the right values
/// regardless of field-layout drift.
pub fn register_memory_pool_impl(r: &mut NativeMethodRegistry) {
            let cls = "sun/management/MemoryPoolImpl";

    // <init>(Ljava/lang/String;ZJJ)V — the real
    // `MemoryPoolImpl(String name, boolean isHeap, long usageThreshold,
    // long gcThreshold)`. All four arguments are read back by accessors:
    // `name` by `getName()`, `isHeap` by `getType()` (both registered just
    // below), and the two thresholds by the pure-Java `getUsageThreshold()` /
    // `getCollectionUsageThreshold()`. The previous no-op discarded every one
    // of them, so a pool constructed through this ctor reported a null name
    // and a NON_HEAP type regardless of what it was created as. The two
    // `*Supported` flags are derived exactly the way the JDK derives them: a
    // negative threshold means the pool does not support that threshold.
    //
    // gen r5w4/obs8 (2026-09-26): this body is only the stand-in for a class
    // with NO bytes. Real class bytes are authoritative (AGENTS.md, contract
    // §1.4): when `MemoryPoolImpl` was loaded from the JDK image, the bridge
    // hands the call to the class's own constructor through the sanctioned
    // "a native that IS the registration for this triple runs its class's
    // bytecode" seam (`invoke_special_bytecode_only`). Without that, every
    // compatible-mode door that prefers a registered `Bridge` over concrete
    // bytecode (`invoke_on_class_shared_inner`'s pre-execute override, which
    // `new_object_initialized` goes through) ran this body instead, so the
    // two sensors (`usageSensor`, `gcSensor`) the JDK constructor builds were
    // never built, and `jmx.rs::alloc_heap_pool_impl` had to force both
    // `*Supported` flags back to `false`: GenR5W3PoolThresholdProbe measured
    // `false/false` on all three Generational pools at `897210f83`. The only
    // callers of this constructor are that function and user reflection
    // (HotSpot calls `ManagementFactoryHelper.createMemoryPool` from the VM);
    // both now get exactly what HotSpot's constructor builds.
    r.register(cls, "<init>", "(Ljava/lang/String;ZJJ)V", |ctx, args| {
        if !ctx.is_class_synthetic_stub("sun/management/MemoryPoolImpl") {
            return ctx.invoke_special_bytecode_only(
                "sun/management/MemoryPoolImpl",
                "<init>",
                "(Ljava/lang/String;ZJJ)V",
                args,
            );
        }
        let this = obj_arg(args, 0)?;
        let name = args.get(1).copied().unwrap_or(Value::Object(None));
        let is_heap = match args.get(2) {
            Some(Value::Int(v)) => *v,
            _ => 0,
        };
        let usage_threshold = long_arg(args, 3);
        let collection_threshold = long_arg(args, 4);
        ctx.set_field_by_name(this, "name", name);
        ctx.set_field_by_name(this, "isHeap", Value::Int(is_heap));
        ctx.set_field_by_name(this, "isValid", Value::Int(1));
        ctx.set_field_by_name(this, "usageThreshold", Value::Long(usage_threshold));
        ctx.set_field_by_name(
            this,
            "collectionThreshold",
            Value::Long(collection_threshold),
        );
        ctx.set_field_by_name(
            this,
            "usageThresholdSupported",
            Value::Int(i32::from(usage_threshold >= 0)),
        );
        ctx.set_field_by_name(
            this,
            "collectionThresholdSupported",
            Value::Int(i32::from(collection_threshold >= 0)),
        );
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);

    r.register(cls, "getName", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field_by_name(this, "name")))
    }, cratonvm_native_api::NativeKind::Bridge);

    r.register(cls, "isValid", "()Z", native_manager_is_valid, cratonvm_native_api::NativeKind::Bridge);

    r.register(
        cls,
        "getType",
        "()Ljava/lang/management/MemoryType;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let is_heap = matches!(
                ctx.get_field_by_name(this, "isHeap"),
                Value::Int(v) if v != 0
            );
            // MemoryType is an enum — fetch the static field by name.
            let cid = ctx
                .ensure_class_initialized("java/lang/management/MemoryType")
                .unwrap_or(ClassId::new(0));
            let field = if is_heap { "HEAP" } else { "NON_HEAP" };
            if let Some(idx) = ctx.static_field_index_by_name(cid, field) {
                Ok(Some(ctx.get_static_field(cid, idx)))
            } else {
                // Fall back to a freshly allocated synthetic enum object —
                // the probe only stringifies via toString(), which on
                // enums reads the `name` field at slot 0.
                //
                // H6-1 N4: this carrier is fabricated fresh right here, under
                // a class name (`java/lang/management/MemoryType`) whose real
                // class initialization / static lookup already failed on this
                // arm — so it mints with NO named fields
                // (`synthetic_stub_field_model` gives unnamed carriers
                // `_f0.._fN`), and `set_field_by_name("name", ..)` is a
                // documented no-op when the name does not resolve: it used to
                // drop this write silently. Slot 0 is an index into a carrier
                // this function itself just allocated, which H6-1 §3's own
                // discriminator calls legitimate (unlike indexing into a REAL
                // JDK layout) — write it directly.
                let e = try_alloc_concurrent_synthetic(ctx, "java/lang/management/MemoryType", 2)?;
                // gen r4w2/obs: `create_string` allocates; re-read the carrier
                // through a pin before writing into it.
                let pin = ctx.pin_native_root(e);
                let label = ctx.create_string(field);
                let e = ctx.read_native_pin(pin, e);
                ctx.unpin_native_roots(pin);
                ctx.set_field(e, 0, Value::Object(Some(label)));
                Ok(Some(Value::Object(Some(e))))
            }
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // getUsage / getCollectionUsage — the pool's real usage when the collector
    // describes its pools (gen r4w2/obs, 2026-09-23: the generational backend's
    // `Eden Space` / `Survivor Space` / `Tenured Gen`, which printed
    // `used=-1 committed=-1 max=-1` in `GenR4MxbeanProbe.java`),
    // and otherwise the JMM "metric unavailable" sentinel; see
    // `undefined_memory_usage` for why that is the honest answer there.
    r.register(
        cls,
        "getUsage0",
        "()Ljava/lang/management/MemoryUsage;",
        |ctx, args| {
            // gci i15/nh: a non-heap pool too.
            let pool = obj_arg(args, 0)
                .ok()
                .and_then(|this| any_pool_info_for(&*ctx, this));
            let mu = match pool {
                Some(p) => gc_pool_usage(ctx, &p)?,
                None => undefined_memory_usage(ctx)?,
            };
            Ok(Some(Value::Object(Some(mu))))
        },
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        cls,
        "getCollectionUsage0",
        "()Ljava/lang/management/MemoryUsage;",
        |ctx, args| {
            let pool = obj_arg(args, 0)
                .ok()
                .and_then(|this| any_pool_info_for(&*ctx, this));
            let mu = match pool {
                // gci i15/nh: no collector manages a non-heap pool, and
                // HotSpot's `jmm_GetPoolCollectionUsage` answers null for a
                // pool that is not `is_collected_pool()`.
                Some(p) if is_non_heap_pool(&p) => return Ok(Some(Value::Object(None))),
                Some(p) => gc_pool_collection_usage(ctx, &p)?,
                None => undefined_memory_usage(ctx)?,
            };
            Ok(Some(Value::Object(Some(mu))))
        },
        cratonvm_native_api::NativeKind::Bridge,
    );

    // getPeakUsage0 — answer with the peak this pool has actually recorded
    // (`peakUsage`), falling back to the current usage when nothing has been
    // recorded yet. That fallback is what makes the JMM invariant
    // "peak >= current, and peak == current immediately after a reset" hold
    // rather than being asserted by two independent constants.
    r.register(
        cls,
        "getPeakUsage0",
        "()Ljava/lang/management/MemoryUsage;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let recorded = match ctx.get_field_by_name(this, "peakUsage") {
                Value::Object(Some(peak)) => Some(peak),
                _ => None,
            };
            // gen r4w2/obs: a pool with real usage keeps a real peak — the
            // larger of the recorded one and the current usage, re-recorded
            // when the current one is larger.
            //
            // gen r5w2/obs6 (2026-09-26): and of the collector's own samples,
            // taken at both edges of every collection
            // (`NativeContext::gc_pool_peak`) — HotSpot's
            // `record_peak_memory_usage` at `gc_begin` / `gc_end`. Eden peaks
            // exactly then (it is full at the pause), so a query-time sample
            // alone reported roughly the query-time occupancy
            // (`gengc-r4w2-obs-jmx-residuals-on-the-generational-backend`
            // item 1). `used` and `committed` are maximised separately, as
            // HotSpot does; `max` stays the pool's.
            //
            // gci i14/mp: the JDK's own `MemoryPoolImpl` declares no
            // `peakUsage` field, so on a real class library `recorded` is
            // always `None` and the write below is a no-op: the peak lives in
            // the VM (`record_gc_pool_peak`, which records this query's sample).
            // The field is the synthetic-JDK carrier's only.
            //
            // gci i15/nh: a non-heap pool too; its peak lives in the same VM
            // queue, raised only by these queries (no collection edge samples
            // it), so it never falls below an earlier answer.
            if let Some(pool) = any_pool_info_for(ctx, this) {
                let (mut used, mut committed) = (pool.used, pool.committed);
                // gci i14/mp finding 13: hand the VM this sample instead of
                // letting it take a second one.
                if let Some((u, c)) = ctx.record_gc_pool_peak(pool.name, pool.used, pool.committed) {
                    used = used.max(u);
                    committed = committed.max(c);
                }
                if let Some(peak) = recorded {
                    let [_, s_used, s_committed, _] = memory_usage_slots(ctx, peak);
                    let long = |v: Value| match v {
                        Value::Long(l) => l,
                        _ => -1,
                    };
                    let rec_used = long(ctx.get_field(peak, s_used));
                    let rec_committed = long(ctx.get_field(peak, s_committed));
                    if rec_used >= used && rec_committed >= committed {
                        return Ok(Some(Value::Object(Some(peak))));
                    }
                    used = used.max(rec_used);
                    committed = committed.max(rec_committed);
                }
                // `MemoryUsage` rejects `used > committed` and, with a defined
                // max, `committed > max`.
                let committed = committed.max(used);
                let max = if pool.max >= 0 {
                    pool.max.max(committed)
                } else {
                    pool.max
                };
                let pin = ctx.pin_native_root(this);
                let now = alloc_memory_usage(ctx, pool.init, used, committed, max);
                let this = ctx.read_native_pin(pin, this);
                ctx.unpin_native_roots(pin);
                let now = now?;
                ctx.set_field_by_name(this, "peakUsage", Value::Object(Some(now)));
                return Ok(Some(Value::Object(Some(now))));
            }
            if let Some(peak) = recorded {
                return Ok(Some(Value::Object(Some(peak))));
            }
            let mu = undefined_memory_usage(ctx)?;
            Ok(Some(Value::Object(Some(mu))))
        },
        cratonvm_native_api::NativeKind::Bridge,
    );

    // resetPeakUsage0()V — the JMM contract is "reset the recorded peak to
    // the pool's CURRENT usage", so record the current usage on the instance
    // instead of ignoring the call; `getPeakUsage0` above reads it back.
    //
    // gen r4w2/obs (2026-09-23): a pool the collector describes now records
    // its REAL current usage, so a reset is externally observable there. A
    // pool it does not describe still records the UNDEFINED sentinel (the
    // pre- and post-reset answers are then equal, as before).
    r.register(
        cls,
        "resetPeakUsage0",
        "()V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // gci i15/nh: a non-heap pool too.
            let pool = any_pool_info_for(ctx, this);
            // gen r5w2/obs6: the collector-sampled peak resets too, or the
            // next `getPeakUsage()` would bring the pre-reset peak back.
            if let Some(p) = pool.as_ref() {
                ctx.reset_gc_pool_peak(p.name);
            }
            // Building the snapshot allocates, which can relocate the receiver —
            // keep it rooted and re-read it before the field write.
            let pin = ctx.pin_native_root(this);
            let now = match pool {
                Some(p) => gc_pool_usage(ctx, &p),
                None => undefined_memory_usage(ctx),
            };
            let this = ctx.read_native_pin(pin, this);
            ctx.unpin_native_roots(pin);
            let now = now?;
            ctx.set_field_by_name(this, "peakUsage", Value::Object(Some(now)));
            Ok(None)
        },
        cratonvm_native_api::NativeKind::Bridge,
    );

    // gen r5w3/obs7 (2026-09-26;
    // `gengc-r5w2-obs6-heap-pools-support-no-usage-thresholds`): the four
    // `ACC_NATIVE` threshold methods of the JDK's `MemoryPoolImpl`, which
    // had no native at all (a pool reporting support would have thrown
    // `UnsatisfiedLinkError` from `setUsageThreshold`).
    //
    // `setUsageThreshold0(long current, long newThreshold)` /
    // `setCollectionThreshold0(long current, long newThreshold)` arm the
    // collector's check (`NativeContext::set_gc_pool_threshold`); the Java
    // side has already refused an unsupported pool, a negative value and one
    // above `max`, and stores the new value in its own field afterwards.
    r.register(
        cls,
        "setUsageThreshold0",
        "(JJ)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            if let Some(name) = bean_name(&*ctx, this) {
                let _ = ctx.set_gc_pool_threshold(&name, false, long_arg(args, 2));            }
            Ok(None)
        },
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        cls,
        "setCollectionThreshold0",
        "(JJ)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            if let Some(name) = bean_name(&*ctx, this) {
                let _ = ctx.set_gc_pool_threshold(&name, true, long_arg(args, 2));            }
            Ok(None)
        },
        cratonvm_native_api::NativeKind::Bridge,
    );
    // `setPoolUsageSensor(Sensor)` / `setPoolCollectionSensor(Sensor)` hand
    // HotSpot the sensor objects to keep as JNI global handles. This VM keeps
    // no Java reference outside the heap: the delivery
    // (`deliver_pool_sensor`) reads the sensor back from the pool bean's own
    // `usageSensor` / `gcSensor` field, which the JDK constructor set to the
    // very objects passed here. So these only have to exist.
    r.register(
        cls,
        "setPoolUsageSensor",
        "(Lsun/management/Sensor;)V",
        |_ctx, _args| Ok(None),
        cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        cls,
        "setPoolCollectionSensor",
        "(Lsun/management/Sensor;)V",
        |_ctx, _args| Ok(None),
        cratonvm_native_api::NativeKind::Bridge,
    );

    // getMemoryManagers0()[Ljava/lang/management/MemoryManagerMXBean; --
    // backs the pure-Java getMemoryManagerNames(), which Tomcat's
    // Diagnostics.getVMInfo() calls unprotected (no try/catch). Return an
    // empty array so the accessor doesn't trip on a missing native, mirroring
    // MemoryManagerImpl.getMemoryPools0's same-shape stub above.
    //
    // gen r4w2/obs: a pool the collector describes answers with its real
    // managers (Serial: `Eden Space` -> `Copy`, `MarkSweepCompact`), so
    // `getMemoryManagerNames()` joins against the collector list.
    r.register(
        cls,
        "getMemoryManagers0",
        "()[Ljava/lang/management/MemoryManagerMXBean;",
        |ctx, args| {
            // gci i15/nh: a non-heap pool answers its non-heap manager.
            let pool = obj_arg(args, 0)
                .ok()
                .and_then(|this| any_pool_info_for(&*ctx, this));
            let non_heap = pool.as_ref().is_some_and(is_non_heap_pool);
            // i7-L2: the empty answer carries the same component as the
            // populated one, not `Object`.
            let names = pool.map_or(&[][..], |p| p.manager_names);
            // gen r5w2/obs6: the SAME collector beans
            // `MemoryImpl.getMemoryManagers()` handed out (the ones a
            // `NotificationListener` is registered on), when it has built
            // them; fresh ones otherwise.
            let canonical = if names.is_empty() {
                None
            } else {
                registered_bean_array(ctx, "mgrs", "sun/management/MemoryManagerImpl", names)
            };
            let arr = match canonical {
                Some(arr) => arr,
                None if non_heap => alloc_bean_array(
                    ctx,
                    "sun/management/MemoryManagerImpl",
                    names,
                    alloc_memory_manager_impl,
                )?,
                None => alloc_collector_bean_array(ctx, names)?,            };
            Ok(Some(Value::Object(Some(arr))))
        },
        cratonvm_native_api::NativeKind::Bridge,
    );

    }

/// Allocate a synthetic `sun.management.MemoryPoolImpl` with `name` +
/// `isHeap` populated.  The remaining fields default-initialise to zero
/// (longs) / null (refs) which matches a "no-threshold" pool — fine for
/// JConsole-style enumeration.
pub fn alloc_memory_pool_impl(
    ctx: &mut dyn NativeContext,
    name: &str,
    is_heap: bool,
) -> Result<ObjectRef, MethodCallFailed> {
    let obj = try_alloc_concurrent_synthetic(ctx, "sun/management/MemoryPoolImpl", 12)?;
    // gen r4w2/obs: `create_string` allocates; re-read the bean through a pin
    // before writing its fields (see `alloc_garbage_collector_impl`).
    let pin = ctx.pin_native_root(obj);
    let n = jmx_fresh_string(ctx, name);
    let obj = ctx.read_native_pin(pin, obj);
    ctx.unpin_native_roots(pin);
    ctx.set_field_by_name(obj, "name", Value::Object(Some(n)));
    ctx.set_field_by_name(obj, "isHeap", Value::Int(if is_heap { 1 } else { 0 }));
    ctx.set_field_by_name(obj, "isValid", Value::Int(1));
    Ok(obj)
}

/// Allocate a synthetic `sun.management.GarbageCollectorImpl` with the
/// inherited `name` field populated.  The bean implements both
/// `MemoryManagerMXBean` (so it shows up in
/// `getMemoryManagerMXBeans()`) and `GarbageCollectorMXBean` (so it
/// passes the `instanceof` filter in
/// `ManagementFactoryHelper.getGarbageCollectorMXBeans`).
pub fn alloc_garbage_collector_impl(
    ctx: &mut dyn NativeContext,
    name: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    let obj = try_alloc_concurrent_synthetic(ctx, "sun/management/GarbageCollectorImpl", 4)?;
    // gen r4w2/obs: `create_string` allocates, so the bean is re-read through
    // a pin before its fields are written (the MemoryPoolImpl twin above had
    // the same gap). This factory is now called in a loop, once per collector.
    let pin = ctx.pin_native_root(obj);
    let n = jmx_fresh_string(ctx, name);
    let obj = ctx.read_native_pin(pin, obj);
    init_memory_manager_fields(ctx, obj, Value::Object(Some(n)));
    ctx.unpin_native_roots(pin);
    // gc-common w6-d (`common-w6d-gc-bean-add-notification-listener-npe-on-g1-and-zgc`):
    // the constructors never ran, so neither did `NotificationEmitterSupport`'s
    // field initialisers, and `addNotificationListener` synchronizes on
    // `listenerLock` -- a `NullPointerException` under `--real-jdk` on every
    // backend whose beans come from here (G1, ZGC). `alloc_gc_ext_bean` and
    // `alloc_memory_mxbean` already do this. Fills only null fields, and
    // returns the bean's current address (it pins and re-reads it itself).
    init_notification_emitter_support(ctx, obj)
}

// ---------------------------------------------------------------------------
// Per-generation beans (gen r4w2/obs, 2026-09-23)
// ---------------------------------------------------------------------------
//
// When the running collector describes its beans
// (`NativeContext::gc_collectors` / `gc_memory_pools` non-empty — the
// generational backend, with HotSpot Serial's `Copy` / `MarkSweepCompact`
// collectors and `Eden Space` / `Survivor Space` / `Tenured Gen` pools; since
// gc-common w7-f G1, with `G1 Young Generation` / `G1 Old Generation` /
// `G1 Concurrent GC` and the three `G1 ...` pools, and ZGC's four
// `ZGC Minor/Major Cycles/Pauses` over `ZGC Young Generation` /
// `ZGC Old Generation` since the gcd d10/o retirement), every bean list,
// every per-bean counter and every pool usage below answers from those
// descriptors, looked up by the bean's own `name`. When it does not (test
// contexts, and ZGC until `handoff-w7f-zgc-backend-gc-beans` lands), each site
// keeps its historical answer byte-for-byte.
// See `docs/feature-designs/generative-gc-architecture.md`.

/// The collector's description of the pool bean `this`, looked up by name.
pub(in crate::jmx) fn gc_pool_info_for(
    ctx: &dyn NativeContext,
    this: ObjectRef,
) -> Option<cratonvm_native_api::GcMemoryPoolInfo> {
    let pools = ctx.gc_memory_pools();
    if pools.is_empty() {
        return None;
    }
    let name = match ctx.get_field_by_name(this, "name") {
        Value::Object(Some(s)) => ctx.read_string(s)?,
        _ => return None,
    };
    pools.into_iter().find(|p| p.name == name)}

/// zs s3/ob2: the non-heap pools the VM models itself
/// ([`NativeContext::non_heap_memory_pools`]: ZGC's `Metaspace` under
/// `CRATONVM_ZGC_JMX_METASPACE_POOL`; empty on every other backend), by name.
/// Non-empty, they replace gci i15/nh's five (`JMX_NON_HEAP_POOL_NAMES`) on
/// that VM: its pool list, its plain managers and the figures of a non-heap
/// pool bean (see `any_pool_info_for`).
///
/// zt t1/ob (the merge decision): HotSpot 25 ZGC lists the five, so ZGC
/// models none of its own by default and this is empty everywhere; only the
/// kill arm `CRATONVM_ZGC_JMX_HOTSPOT_NON_HEAP_POOLS=0` brings zs s3/ob2's
/// `Metaspace`-only set back.
pub(in crate::jmx) fn zgc_non_heap_pool_names(ctx: &dyn NativeContext) -> Vec<&'static str> {
    ctx.non_heap_memory_pools().iter().map(|p| p.name).collect()
}

/// The pools plain manager `manager` manages (`getMemoryPoolNames()`): zs
/// s3/ob2's VM-modelled pools that name it when the VM models any (ZGC:
/// `Metaspace Manager` -> `Metaspace`), else gci i15/nh's
/// [`non_heap_manager_pools`].
pub(in crate::jmx) fn non_heap_manager_pools_of(ctx: &dyn NativeContext, manager: &str) -> Vec<&'static str> {
    let modelled = ctx.non_heap_memory_pools();
    if modelled.is_empty() {
        // zt t30/nh: an unsegmented code cache's manager manages `CodeCache`.
        if manager == "CodeCacheManager" {
            return code_pool_names(ctx.code_cache_layout().as_ref()).to_vec();
        }
        return non_heap_manager_pools(manager).to_vec();
    }
    modelled
        .iter()
        .filter(|p| p.manager_names.contains(&manager))
        .map(|p| p.name)
        .collect()
}

/// zs s3/ob2: the plain (non-GC) managers of the VM-modelled non-heap pools,
/// each once, in pool order (ZGC: `Metaspace Manager`); empty when the VM
/// models none (gci i15/nh's `JMX_NON_HEAP_MANAGER_NAMES` then).
pub(in crate::jmx) fn non_heap_manager_names(ctx: &dyn NativeContext) -> Vec<&'static str> {
    let mut out: Vec<&'static str> = Vec::new();
    for p in ctx.non_heap_memory_pools() {
        for &m in p.manager_names {
            if !out.contains(&m) {
                out.push(m);
            }
        }
    }
    out
}

/// The collector's description of the manager / collector bean `this`.
pub(in crate::jmx) fn gc_collector_info_for(
    ctx: &dyn NativeContext,
    this: ObjectRef,
) -> Option<cratonvm_native_api::GcCollectorInfo> {
    let collectors = ctx.gc_collectors();
    if collectors.is_empty() {
        return None;
    }
    let name = match ctx.get_field_by_name(this, "name") {
        Value::Object(Some(s)) => ctx.read_string(s)?,
        _ => return None,
    };
    collectors.into_iter().find(|c| c.name == name)}

// ---------------------------------------------------------------------------
// Non-heap pools and managers (gci i15/nh, 2026-10-14)
// ---------------------------------------------------------------------------
//
// `gci-i14mp-memory-pool-and-gc-mxbean-review-findings-FIXED-20261016` item 7.
// HotSpot 25 reports five non-heap pools ahead of the heap's
// (`cratonvm_native_api::registry::JMX_NON_HEAP_POOL_NAMES`) and two
// non-collecting managers ahead of the collectors (`CodeCacheManager` over
// the three code heaps, `Metaspace Manager` over `Metaspace` and
// `Compressed Class Space`); `getNonHeapMemoryUsage()` is their sum
// (`jmm_GetMemoryUsage(false)`). Here, whenever the collector describes its
// heap pools (every real backend), the same lists in the same order, with
// what this VM really holds:
//
// * `Metaspace`: the measured class-metadata bytes (`class_metadata_bytes`,
//   the heap summary's `metaspace_used`, the figure `getNonHeapMemoryUsage()`
//   reported alone until now); `committed == used` (no reservation stands
//   behind it); `max` is `-XX:MaxMetaspaceSize` or -1, as HotSpot. gci i33/ms:
//   with the VM's HotSpot figures (`NativeContext::metaspace_space_figures`)
//   `committed` is the 64 KiB-granule figure and `Compressed Class Space`
//   carries the class space (used, committed, `max` CompressedClassSpaceSize)
//   instead of being empty ([`MetaspaceSplit`]).
// * `CodeHeap 'non-profiled nmethods'`: every byte of JIT code mapped now
//   (`jit_code_cache_bytes`: method bodies, OSR trampolines and deopt stubs
//   are one count, the one the code-cache cap bounds); `max` the cap or -1.
//   Compiled code here carries no profile (profiling is the interpreter's), so
//   its HotSpot counterpart is the non-profiled heap. `init` keeps the 2 MiB the
//   non-heap aggregate always reported (`NON_HEAP_INIT_BYTES`), so the
//   aggregate's `init` is unchanged.
// * NO COUNTERPART, reported empty (`0` bytes, `max` undefined): `CodeHeap
//   'non-nmethods'` (the interpreter, adapters and runtime stubs are Rust code
//   in the binary; the JIT's own stubs are in the count above, not separable),
//   `CodeHeap 'profiled nmethods'` (no profiled tier), `Compressed Class Space`
//   (gci i33/ms: only in the no-class-space shape, `CRATONVM_METASPACE_HOTSPOT_SPACES=0`;
//   `Metaspace` includes the class space either way, as on HotSpot).
//
// Usage thresholds: built as HotSpot builds them (usage threshold supported,
// collection threshold not). gci i16/nt: checked as HotSpot's low-memory
// detector checks them -- `setUsageThreshold0` arms the VM's per-heap table
// (`set_gc_pool_threshold` accepts these names) and samples at once (HotSpot's
// `jmm_SetPoolThreshold` -> `detect_low_memory(pool)`), and the VM samples
// again on class definition (HotSpot: per metaspace expansion; here on a
// class-file-bytes cadence, see `PoolThresholds::non_heap_walk_due`) and at
// each collection's end; a crossing is the gauge sensor's one trigger,
// delivered as `MEMORY_THRESHOLD_EXCEEDED` through `deliver_pool_sensor` at
// the next drain point, and counted in `getUsageThresholdCount()`. The code
// heaps are sampled at the same points, not at JIT install (no VM hook there;
// see the page). Samples come from [`non_heap_threshold_samples`], the
// figures `getUsage()` answers. `getCollectionUsage()` is null, as on HotSpot
// (no collector manages them), and their `GcInfo` entries are sampled at
// delivery.

/// The code heaps, in `JMX_NON_HEAP_POOL_NAMES` order.
pub(in crate::jmx) const CODE_HEAP_POOLS: &[&str] = &[
    "CodeHeap 'non-nmethods'",
    "CodeHeap 'profiled nmethods'",
    "CodeHeap 'non-profiled nmethods'",
];
/// zt t30/nh: the one code pool of an unsegmented code cache.
pub(in crate::jmx) const CODE_CACHE_POOL: &[&str] = &[cratonvm_types::code_cache_layout::UNSEGMENTED_CODE_CACHE_POOL];

/// zt t30/nh: the code pools `layout` lists (the three heaps, or `CodeCache`).
pub(in crate::jmx) fn code_pool_names(
    layout: Option<&cratonvm_types::code_cache_layout::CodeCacheLayout>,
) -> &'static [&'static str] {
    match layout {
        Some(l) if !l.segmented => CODE_CACHE_POOL,
        _ => CODE_HEAP_POOLS,
    }
}

/// The metaspace pools, in `JMX_NON_HEAP_POOL_NAMES` order.
pub(in crate::jmx) const METASPACE_POOLS: &[&str] = &["Metaspace", "Compressed Class Space"];
pub(in crate::jmx) const CODE_CACHE_MANAGERS: &[&str] = &["CodeCacheManager"];
pub(in crate::jmx) const METASPACE_MANAGERS: &[&str] = &["Metaspace Manager"];

/// gci i33/ms (`gci-i33ms-metaspace-spaces-and-limit-FIXED-20261016`): the part of
/// HotSpot 25's metaspace figures the two metaspace pools need besides the
/// used total -- the total committed (`Metaspace`: committed by 64 KiB
/// granule, not `== used`) and the compressed class space's used, committed
/// and max (`Compressed Class Space`: `CompressedClassSpaceSize`, the pool
/// HotSpot fills with the narrow-klass `Klass`es; it read used 0). From the
/// VM's `NativeContext::metaspace_space_figures`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MetaspaceSplit {
    pub committed: u64,
    pub class_used: u64,
    pub class_committed: u64,
    pub class_max: u64,
}

/// What the non-heap pools are sampled from. Pure input of
/// [`non_heap_pool_from`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(in crate::jmx) struct NonHeapSources {
    /// Class-metadata bytes ([`non_heap_used_bytes`]: positive).
    pub(in crate::jmx) metadata_used: i64,
    /// `-XX:MaxMetaspaceSize`, `None` unbounded.
    pub(in crate::jmx) max_metaspace: Option<u64>,
    /// gci i33/ms: HotSpot's committed and class-space figures; `None` = the
    /// no-class-space shape (`committed == used`, `Compressed Class Space`
    /// empty), as before.
    pub(in crate::jmx) metaspace_split: Option<MetaspaceSplit>,
    /// JIT code mapped now and its cap (`None` unbounded); `None`: no code
    /// cache (mocks).
    pub(in crate::jmx) code: Option<(u64, Option<u64>)>,
}

/// The `&'static` spelling of non-heap pool `name`, if it is one.
pub(in crate::jmx) fn non_heap_pool_name(name: &str) -> Option<&'static str> {
    // zt t30/nh: either layout's names (`CodeCache` too).
    cratonvm_native_api::registry::JMX_ANY_NON_HEAP_POOL_NAMES
        .iter()
        .copied()
        .find(|n| *n == name)
}

/// The pools non-heap manager `name` manages (`getMemoryPoolNames()`); empty
/// for any other name.
pub(in crate::jmx) fn non_heap_manager_pools(name: &str) -> &'static [&'static str] {
    match name {
        "CodeCacheManager" => CODE_HEAP_POOLS,
        "Metaspace Manager" => METASPACE_POOLS,
        _ => &[],
    }
}

/// Non-heap pool `name` as [`NonHeapSources`] describe it (see the section
/// note for each figure). Every quadruple is a valid `MemoryUsage`:
/// `committed == used`, and a defined `max` is never below `used`. `None` for
/// a name that is not a non-heap pool. Pure.
pub(in crate::jmx) fn non_heap_pool_from(
    name: &'static str,
    s: &NonHeapSources,
) -> Option<cratonvm_native_api::GcMemoryPoolInfo> {
    let to_i64 = |b: u64| i64::try_from(b).unwrap_or(i64::MAX);
    // gci i33/ms: `committed` is its own figure (never below `used`);
    // `max` is never below `committed`.
    let pool_committed = |init: i64,
                          used: i64,
                          committed: i64,
                          max: Option<u64>,
                          managers: &'static [&'static str]|
     -> cratonvm_native_api::GcMemoryPoolInfo {
        let committed = committed.max(used);
        cratonvm_native_api::GcMemoryPoolInfo {
            name,
            init,
            used,
            committed,
            max: max.map_or(-1, |m| to_i64(m).max(committed)),
            // Never collected: HotSpot's `getCollectionUsage()` is null.
            collection_used: -1,
            collection_committed: -1,
            manager_names: managers,
        }
    };
    let pool = |init: i64, used: i64, max: Option<u64>, managers: &'static [&'static str]| {
        pool_committed(init, used, used, max, managers)
    };
    Some(match name {
        // zt t30/nh: an unsegmented code cache's one pool carries the same
        // code (its `init` and `max` are the layout's: `with_code_cache_layout`).
        "CodeHeap 'non-profiled nmethods'" | "CodeCache" => {
            let (used, cap) = s.code.unwrap_or((0, None));
            pool(NON_HEAP_INIT_BYTES, to_i64(used), cap, CODE_CACHE_MANAGERS)
        }
        "CodeHeap 'non-nmethods'" | "CodeHeap 'profiled nmethods'" => {
            pool(0, 0, None, CODE_CACHE_MANAGERS)
        }
        "Metaspace" => {
            let used = s.metadata_used.max(0);
            let committed = s.metaspace_split.map_or(used, |m| to_i64(m.committed));
            pool_committed(0, used, committed, s.max_metaspace, METASPACE_MANAGERS)
        }
        "Compressed Class Space" => match s.metaspace_split {
            Some(m) => pool_committed(
                0,
                to_i64(m.class_used),
                to_i64(m.class_committed),
                Some(m.class_max),
                METASPACE_MANAGERS,
            ),
            None => pool(0, 0, None, METASPACE_MANAGERS),
        },
        _ => return None,
    })
}

/// Sample the sources: the class-metadata walk only when `metaspace`, the
/// two relaxed JIT loads only when `code`.
pub(in crate::jmx) fn non_heap_sources(ctx: &dyn NativeContext, metaspace: bool, code: bool) -> NonHeapSources {
    // gci i33/ms: HotSpot's figures in one walk when the VM states them,
    // else the measured total (`class_metadata_bytes`), as before.
    let figures = if metaspace {
        ctx.metaspace_space_figures()
    } else {
        None
    };
    NonHeapSources {
        metadata_used: match figures {
            Some([used, ..]) => non_heap_used_bytes(Some(used), 0),
            None if metaspace => {
                non_heap_used_bytes(ctx.class_metadata_bytes(), ctx.loaded_class_count())
            }
            None => 0,
        },
        max_metaspace: if metaspace {
            ctx.max_metaspace_bytes()
        } else {
            None
        },
        metaspace_split: figures.map(metaspace_split_of),
        code: if code { ctx.jit_code_cache_bytes() } else { None },
    }
}

/// gci i33/ms: [`MetaspaceSplit`] from `NativeContext::metaspace_space_figures`'
/// `[used, committed, class used, class committed, class max]`. Pure.
pub(in crate::jmx) fn metaspace_split_of(figures: [u64; 5]) -> MetaspaceSplit {
    let [_, committed, class_used, class_committed, class_max] = figures;
    MetaspaceSplit {
        committed,
        class_used,
        class_committed,
        class_max,
    }
}

/// Non-heap pool `name` now, sampling only its own source.
pub(in crate::jmx) fn non_heap_pool_info(
    ctx: &dyn NativeContext,
    name: &str,
) -> Option<cratonvm_native_api::GcMemoryPoolInfo> {
    let name = non_heap_pool_name(name)?;
    let metaspace = METASPACE_POOLS.iter().any(|n| *n == name);
    let layout = if metaspace {
        None
    } else {
        // zt t30/nh: only the code pools this VM's layout lists.
        let layout = ctx.code_cache_layout();
        if !code_pool_names(layout.as_ref()).contains(&name) {
            return None;
        }
        layout
    };
    let mut pool = non_heap_pool_from(name, &non_heap_sources(ctx, metaspace, !metaspace))?;
    if !metaspace {
        with_code_cache_layout(std::slice::from_mut(&mut pool), layout.as_ref());
    }
    Some(pool)
}

/// All five non-heap pools now, in HotSpot's order (one sample of each source).
pub(in crate::jmx) fn non_heap_pool_infos(ctx: &dyn NativeContext) -> Vec<cratonvm_native_api::GcMemoryPoolInfo> {
    let s = non_heap_sources(ctx, true, true);
    let layout = ctx.code_cache_layout();
    let mut pools: Vec<cratonvm_native_api::GcMemoryPoolInfo> =
        cratonvm_native_api::registry::jmx_non_heap_pool_names(layout.as_ref())
            .iter()
            .copied()
            .filter_map(|n| non_heap_pool_from(n, &s))
            .collect();
    with_code_cache_layout(&mut pools, layout.as_ref());
    pools
}

/// zt t3/ob (`zgc-zt1ob-non-heap-pool-residuals-CLOSED-20261019.md` item 1): give
/// each `CodeHeap '...'` pool among `pools` HotSpot's `init` when the VM
/// states one (`NativeContext::code_cache_layout`; ZGC by
/// default), so `getNonHeapMemoryUsage().getInit()` -- the pools' sum -- is
/// HotSpot's (3 x 2555904 = 7667712 on 25.0.3) instead of gci i15/nh's 2 MiB.
/// `None` leaves the pools as they are. Also the threshold samples' `init`
/// (`vm_exec.rs` `jmx_check_non_heap_thresholds`), so a sample is the
/// quadruple `getUsage()` answers. Pure.
///
/// zt t30/nh (`g1-and-generational-items-carried-from-the-zgc-zt-round-FIXED-20261023.md`
/// item 2): the VM states a whole [`CodeCacheLayout`] (every collector under
/// `CRATONVM_JMX_CODE_CACHE_HOTSPOT_LAYOUT`): each code pool's `init` is the
/// layout's (`InitialCodeCacheSize` moves it), and an unsegmented code
/// cache's `CodeCache` pool's `max` is its reserved size
/// (`ReservedCodeCacheSize`, HotSpot's `CodeHeap::max_capacity`), never below
/// `committed`.
///
/// [`CodeCacheLayout`]: cratonvm_types::code_cache_layout::CodeCacheLayout
pub fn with_code_cache_layout(
    pools: &mut [cratonvm_native_api::GcMemoryPoolInfo],
    layout: Option<&cratonvm_types::code_cache_layout::CodeCacheLayout>,
) {
    let Some(layout) = layout else {
        return;
    };
    let to_i64 = |b: u64| i64::try_from(b).unwrap_or(i64::MAX);
    for p in pools.iter_mut() {
        let Some(init) = layout.init_of(p.name) else {
            continue;
        };
        p.init = to_i64(init);
        if !layout.segmented {
            p.max = to_i64(layout.reserved).max(p.committed);
        }
    }
}

/// zt t1/ob: the non-heap pools of the sources asked for only (`metaspace`:
/// `Metaspace` and `Compressed Class Space`, one class-store walk; `code`:
/// the three code heaps, two relaxed loads), in HotSpot's order; empty, and
/// nothing sampled, when neither is.
pub(in crate::jmx) fn non_heap_pool_infos_sampled(
    ctx: &dyn NativeContext,
    metaspace: bool,
    code: bool,
) -> Vec<cratonvm_native_api::GcMemoryPoolInfo> {
    if !(metaspace || code) {
        return Vec::new();
    }
    let s = non_heap_sources(ctx, metaspace, code);
    let layout = if code { ctx.code_cache_layout() } else { None };
    let mut pools: Vec<cratonvm_native_api::GcMemoryPoolInfo> =
        cratonvm_native_api::registry::jmx_non_heap_pool_names(layout.as_ref())
            .iter()
            .copied()
            .filter(|n| {
                if non_heap_pool_is_metaspace(n) {
                    metaspace
                } else {
                    code
                }
            })
            .filter_map(|n| non_heap_pool_from(n, &s))
            .collect();
    if code {
        with_code_cache_layout(&mut pools, layout.as_ref());
    }
    pools
}

/// zt t1/ob: which non-heap sources a `GcInfo` over `pool_names` (the
/// `GcInfoBuilder`'s) needs sampled, `(metaspace, code)`: those of a
/// non-heap pool that `before` or `after` does not carry
/// ([`gc_info_pool_usage`] reads a carried entry and never the sample).
/// Pure.
pub(in crate::jmx) fn gc_info_non_heap_wanted(
    pool_names: &[String],
    before: &[cratonvm_native_api::GcPoolUsageSnapshot],
    after: &[cratonvm_native_api::GcPoolUsageSnapshot],
) -> (bool, bool) {
    let carried = |pool: &str| {
        before.iter().any(|p| p.name == pool) && after.iter().any(|p| p.name == pool)
    };
    let mut want = (false, false);
    for pool in pool_names {
        let Some(name) = non_heap_pool_name(pool) else {
            continue;
        };
        if carried(name) {
            continue;
        }
        if non_heap_pool_is_metaspace(name) {
            want.0 = true;
        } else {
            want.1 = true;
        }
    }
    want
}

/// gci i16/nt: is non-heap pool `name` sampled from the class metadata (the
/// two metaspace pools) rather than the JIT's code? The VM's threshold check
/// samples only the sources an armed threshold needs.
pub fn non_heap_pool_is_metaspace(name: &str) -> bool {
    METASPACE_POOLS.iter().any(|n| *n == name)
}

/// gci i16/nt: the non-heap pools the VM's usage-threshold check compares
/// (`vm_exec.rs` `jmx_check_non_heap_thresholds`), built by the very function
/// `getUsage()` answers from ([`non_heap_pool_from`]), so a notification's
/// `MemoryUsage` and the threshold comparison see the figures the bean
/// reports. `metaspace`: `(measured class-metadata bytes, -XX:MaxMetaspaceSize)`,
/// `None` = not sampled (its two pools are left out); `code`: the JIT's
/// mapped code and cap, `None` = not sampled (the three code heaps are left
/// out). HotSpot's order. Pure.
pub fn non_heap_threshold_samples(
    metaspace: Option<(u64, Option<u64>)>,
    code: Option<(u64, Option<u64>)>,
) -> Vec<cratonvm_native_api::GcMemoryPoolInfo> {
    non_heap_threshold_samples_split(metaspace, None, code)
}

/// gci i33/ms: [`non_heap_threshold_samples`] with HotSpot's committed and
/// class-space figures (`split`, sampled with `metaspace` in the same walk;
/// `None` = the no-class-space shape), so a `Compressed Class Space`
/// threshold compares the class space's used bytes, as HotSpot's low-memory
/// detector does, and the samples are the figures `getUsage()` answers.
pub fn non_heap_threshold_samples_split(
    metaspace: Option<(u64, Option<u64>)>,
    split: Option<MetaspaceSplit>,
    code: Option<(u64, Option<u64>)>,
) -> Vec<cratonvm_native_api::GcMemoryPoolInfo> {
    non_heap_threshold_samples_in(
        cratonvm_native_api::registry::JMX_NON_HEAP_POOL_NAMES,
        metaspace,
        split,
        code,
    )
}

/// zt t30/nh: [`non_heap_threshold_samples_split`] over the pools `names`
/// lists (`registry::jmx_non_heap_pool_names` of the VM's layout: an
/// unsegmented code cache samples its one `CodeCache` pool). Pure.
pub fn non_heap_threshold_samples_in(
    names: &[&'static str],
    metaspace: Option<(u64, Option<u64>)>,
    split: Option<MetaspaceSplit>,
    code: Option<(u64, Option<u64>)>,
) -> Vec<cratonvm_native_api::GcMemoryPoolInfo> {
    let s = NonHeapSources {
        metadata_used: metaspace.map_or(0, |(used, _)| non_heap_used_bytes(Some(used), 0)),
        max_metaspace: metaspace.and_then(|(_, max)| max),
        metaspace_split: metaspace.and(split),
        code,
    };
    names
        .iter()
        .copied()
        .filter(|n| {
            if non_heap_pool_is_metaspace(n) {
                metaspace.is_some()
            } else {
                code.is_some()
            }
        })
        .filter_map(|n| non_heap_pool_from(n, &s))
        .collect()
}

/// Is `pool` one of the non-heap pools?
pub(in crate::jmx) fn is_non_heap_pool(pool: &cratonvm_native_api::GcMemoryPoolInfo) -> bool {
    non_heap_pool_name(pool.name).is_some()
}

/// The pool bean `this` described: a non-heap pool by its name, else a heap
/// pool by the collector ([`gc_pool_info_for`]). The non-heap check comes
/// first so a non-heap query never pays for a heap census.
pub(in crate::jmx) fn any_pool_info_for(
    ctx: &dyn NativeContext,
    this: ObjectRef,
) -> Option<cratonvm_native_api::GcMemoryPoolInfo> {
    if let Some(n) = bean_name(ctx, this) {
        // zs s3/ob2: a VM that models its non-heap pools (ZGC's `Metaspace`)
        // answers from its own figures. Asked only for a non-heap name: the
        // measure takes the class manager's read lock and may walk the class
        // store, which no heap-pool query should pay (zs s4/rv3).
        if non_heap_pool_name(&n).is_some() {
            if let Some(p) = ctx.non_heap_memory_pools().into_iter().find(|p| p.name == n) {
                return Some(p);
            }
        }
        if let Some(p) = non_heap_pool_info(ctx, &n) {
            return Some(p);
        }
    }
    gc_pool_info_for(ctx, this)
}

/// `jmm_GetMemoryUsage(false)` over `pools`: the sums, with `init` / `max`
/// undefined (-1) as soon as one pool's is. Pure.
pub(in crate::jmx) fn non_heap_usage_sum(pools: &[cratonvm_native_api::GcMemoryPoolInfo]) -> [i64; 4] {
    type Field = fn(&cratonvm_native_api::GcMemoryPoolInfo) -> i64;
    let undefined = |f: Field| pools.iter().any(|p| f(p) < 0);
    let sum = |f: Field| {
        pools
            .iter()
            .fold(0i64, |acc, p| acc.saturating_add(f(p).max(0)))
    };
    let init_of: Field = |p| p.init;
    let max_of: Field = |p| p.max;
    let used_of: Field = |p| p.used;
    let committed_of: Field = |p| p.committed;
    let init = if undefined(init_of) { -1 } else { sum(init_of) };
    let max = if undefined(max_of) { -1 } else { sum(max_of) };
    [init, sum(used_of), sum(committed_of), max]
}

/// A `GcInfo` usage entry for `pool`: the collection's own sample when it has
/// one, else the non-heap pool's current usage (`non_heap`), else
/// [`pool_usage_in`]'s undefined entry. Pure.
pub(in crate::jmx) fn gc_info_pool_usage(
    snapshot: &[cratonvm_native_api::GcPoolUsageSnapshot],
    non_heap: &[cratonvm_native_api::GcMemoryPoolInfo],
    pool: &str,
) -> [i64; 4] {
    if !snapshot.iter().any(|p| p.name == pool) {
        if let Some(p) = non_heap.iter().find(|p| p.name == pool) {
            return [p.init, p.used, p.committed, p.max];
        }
    }
    pool_usage_in(snapshot, pool)
}

/// One plain `sun.management.MemoryManagerImpl` (a non-collecting manager),
/// as HotSpot builds it: `ManagementFactoryHelper.createMemoryManager(name)`,
/// i.e. `new MemoryManagerImpl(name)` -- the registered `<init>` bridge hands
/// the call to the JDK constructor when the class has bytes. A constructor
/// that did not run (no JDK bytes) is completed as the synthetic beans are.
pub(in crate::jmx) fn alloc_memory_manager_impl(
    ctx: &mut dyn NativeContext,
    name: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    let built = {
        // gci i15a: fresh, as HotSpot's `create_from_str` (not interned).
        let n = ctx.create_string_uninterned(name);
        let mut scope = NativeHandleScope::new(ctx);
        let n_h = scope.root(n);
        let args = [Value::Object(Some(scope.get(&n_h)))];
        // Roots the new object and the argument across the constructor.
        match scope.new_object_initialized(
            "sun/management/MemoryManagerImpl",
            "(Ljava/lang/String;)V",
            &args,
        )? {
            Some(Value::Object(Some(m))) => Some(m),
            _ => None,
        }
    };
    // Unconstructed-carrier gate (`native-builtins/tests/unconstructed_carrier_gate.rs`,
    // baselined gci i15a): the fallback below MINTS a `MemoryManagerImpl`
    // whose `<init>` / `getName` / `isValid` are retired on a real JDK. Its
    // JDK 25 declared state (src.zip read): `NotificationEmitterSupport`
    // `listenerLock = new Object()`, `listenerList = Collections.emptyList()`;
    // `MemoryManagerImpl(String)` `name = name`, `isValid = true`,
    // `pools = null`, `notifInfo = null`. Every non-zero one is written BY NAME
    // before the bean escapes: `listenerLock` / `listenerList` (an empty
    // `ArrayList`, which the JDK only sizes, iterates and copies) by
    // `init_notification_emitter_support`, `name` / `isValid` by
    // `init_memory_manager_fields` below (a minted bean has no name yet).
    let bean = match built {
        Some(b) => b,
        None => try_alloc_concurrent_synthetic(ctx, "sun/management/MemoryManagerImpl", 4)?,
    };
    // Only null fields are filled: a no-op where the JDK constructor ran.
    let bean = init_notification_emitter_support(ctx, bean)?;
    if bean_name(&*ctx, bean).is_some() {
        return Ok(bean);
    }
    let pin = ctx.pin_native_root(bean);
    let n = ctx.create_string_uninterned(name);
    let bean = ctx.read_native_pin(pin, bean);
    init_memory_manager_fields(ctx, bean, Value::Object(Some(n)));
    ctx.unpin_native_roots(pin);
    Ok(bean)
}

/// One manager bean: a plain one for a non-heap manager, the collector bean
/// ([`alloc_gc_ext_bean`]) for every other name.
pub(in crate::jmx) fn alloc_any_manager_impl(
    ctx: &mut dyn NativeContext,
    name: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    if cratonvm_native_api::registry::JMX_NON_HEAP_MANAGER_NAMES
        .iter()
        .any(|n| *n == name)
    {
        alloc_memory_manager_impl(ctx, name)
    } else {
        alloc_gc_ext_bean(ctx, name)
    }
}

/// `MemoryImpl.getMemoryManagers0`'s array for a collector that describes its
/// beans: HotSpot's `_managers_list`, the two non-heap managers and then the
/// collectors.
pub(in crate::jmx) fn alloc_manager_bean_array(
    ctx: &mut dyn NativeContext,
    collectors: &[&'static str],
) -> Result<ObjectRef, MethodCallFailed> {
    // zs s3/ob2: the VM-modelled pools' managers (ZGC: `Metaspace Manager`)
    // when it models any.
    let modelled = non_heap_manager_names(&*ctx);
    let plain: &[&'static str] = if modelled.is_empty() {
        cratonvm_native_api::registry::JMX_NON_HEAP_MANAGER_NAMES
    } else {
        &modelled
    };
    let names: Vec<&'static str> = plain
        .iter()
        .copied()
        .chain(collectors.iter().copied())
        .collect();
    alloc_bean_array(
        ctx,
        "sun/management/MemoryManagerImpl",
        &names,
        alloc_any_manager_impl,
    )
}

/// `MemoryImpl.getMemoryPools0`'s names for a collector that describes its
/// heap pools: the non-heap pools first, as HotSpot's `_pools_list`. zs
/// s3/ob2: the VM-modelled ones ([`zgc_non_heap_pool_names`], ZGC:
/// `Metaspace`) when it models any.
pub(in crate::jmx) fn pool_names_with_non_heap(ctx: &dyn NativeContext, heap: &[&'static str]) -> Vec<&'static str> {
    let modelled = zgc_non_heap_pool_names(ctx);
    let non_heap: &[&'static str] = if modelled.is_empty() {
        // zt t30/nh: this VM's layout's list (`CodeCache` when unsegmented).
        cratonvm_native_api::registry::jmx_non_heap_pool_names(ctx.code_cache_layout().as_ref())
    } else {
        &modelled
    };
    non_heap.iter().copied().chain(heap.iter().copied()).collect()
}

/// A `MemoryUsage` for `pool`'s current usage.
pub(in crate::jmx) fn gc_pool_usage(
    ctx: &mut dyn NativeContext,
    pool: &cratonvm_native_api::GcMemoryPoolInfo,
) -> Result<ObjectRef, MethodCallFailed> {
    alloc_memory_usage(ctx, pool.init, pool.used, pool.committed, pool.max)
}

/// A `MemoryUsage` for `pool`'s usage right after the last collection. Before
/// the first collection `used` is 0, which is what HotSpot's pools report then
/// (the JMM's "undefined" is reserved for `init`/`max`).
///
/// gci i14/mp (`gci-i14mp-memory-pool-and-gc-mxbean-review-findings-FIXED-20261016`
/// finding 2): and so is `committed`. HotSpot builds every pool with
/// `_after_gc_usage(init_size, 0, 0, max_size)` and only `gc_end` replaces it,
/// so before the pool's first collection `getCollectionUsage()` is
/// `(init, 0, 0, max)`; this answered the pool's CURRENT committed there.
pub(in crate::jmx) fn gc_pool_collection_usage(
    ctx: &mut dyn NativeContext,
    pool: &cratonvm_native_api::GcMemoryPoolInfo,
) -> Result<ObjectRef, MethodCallFailed> {
    let [init, used, committed, max] = collection_usage_quad(pool);
    alloc_memory_usage(ctx, init, used, committed, max)
}

/// `[init, used, committed, max]` of [`gc_pool_collection_usage`]. Pure.
pub(in crate::jmx) fn collection_usage_quad(pool: &cratonvm_native_api::GcMemoryPoolInfo) -> [i64; 4] {
    // gci i14/mp finding 3: `collection_committed < 0` is "no collection that
    // affected this pool yet" -- also after OTHER collections (`Tenured Gen`
    // after young ones only, `G1 Old Gen` after young-only pauses), where
    // HotSpot's `_after_gc_usage` is still its initial `(init, 0, 0, max)`.
    if pool.collection_used < 0 || pool.collection_committed < 0 {
        return [pool.init, 0, 0, pool.max];
    }
    // The used and committed recorded at the end of that collection
    // (finding 3: the committed was the pool's CURRENT one until gci i14/mp),
    // widened so `used <= committed <= max` holds as `MemoryUsage` requires.
    let used = pool.collection_used.max(0);
    let committed = pool.collection_committed.max(used);
    let max = if pool.max >= 0 {
        pool.max.max(committed)
    } else {
        pool.max
    };
    [pool.init, used, committed, max]
}

/// Allocate one bean per name (via `alloc`) into a fresh reference array whose
/// component class is `elem_class`. The array is rooted across every bean
/// allocation, and each bean is stored into it before the next one is
/// allocated, so no reference is held unrooted across an allocation.
pub(in crate::jmx) fn alloc_bean_array<F>(
    ctx: &mut dyn NativeContext,
    elem_class: &str,
    names: &[&'static str],
    mut alloc: F,
) -> Result<ObjectRef, MethodCallFailed>
where
    F: FnMut(&mut dyn NativeContext, &str) -> Result<ObjectRef, MethodCallFailed>,
{
    let cid = ctx
        .ensure_class_initialized(elem_class)
        .unwrap_or(ClassId::new(0));
    let mut scope = NativeHandleScope::new(ctx);
    let arr = scope.new_ref_array(cid, names.len());
    let arr_h = scope.root(arr);
    for (i, &name) in names.iter().enumerate() {
        let bean = alloc(&mut *scope, name)?;
        let arr = scope.get(&arr_h);
        scope.set_array_element(arr, i, Value::Object(Some(bean)));
    }
    Ok(scope.get(&arr_h))
}

/// `MemoryPoolImpl` beans for `names`, as an array.
pub(in crate::jmx) fn alloc_pool_bean_array(
    ctx: &mut dyn NativeContext,
    names: &[&'static str],
) -> Result<ObjectRef, MethodCallFailed> {
    // gci i15/nh: a non-heap name gets a non-heap bean.
    alloc_bean_array(
        ctx,
        "sun/management/MemoryPoolImpl",
        names,
        alloc_any_pool_impl,
    )
}

/// One pool bean: [`alloc_non_heap_pool_impl`] for a non-heap pool name,
/// [`alloc_heap_pool_impl`] for every other. gci i15/nh.
pub(in crate::jmx) fn alloc_any_pool_impl(
    ctx: &mut dyn NativeContext,
    name: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    if non_heap_pool_name(name).is_some() {
        alloc_non_heap_pool_impl(ctx, name)
    } else {
        alloc_heap_pool_impl(ctx, name)
    }
}

/// One non-heap pool bean, built as HotSpot builds `Metaspace`, `Compressed
/// Class Space` and the code heaps: `createMemoryPool(name, false, 0, -1)` --
/// a NON_HEAP pool with the usage threshold supported and the collection
/// threshold not. gci i15/nh (see "Non-heap pools and managers" for what
/// the usage threshold does and does not deliver).
pub(in crate::jmx) fn alloc_non_heap_pool_impl(
    ctx: &mut dyn NativeContext,
    name: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    construct_pool_impl(ctx, name, false, true, false)
}

/// One heap pool bean. A pool whose collector checks a threshold
/// (`NativeContext::gc_pool_threshold_support`: the generational backend's
/// Serial pools, gen r5w3/obs7) is built by the JDK's own constructor
/// `MemoryPoolImpl(String name, boolean isHeap, long usageThreshold, long
/// gcThreshold)` — as HotSpot builds it through
/// `ManagementFactoryHelper.createMemoryPool` — with `0` for a supported
/// threshold and `-1` for an unsupported one, so the bean carries the two
/// sensors (`usageSensor`, `gcSensor`) that `Sensor.trigger` is delivered to.
///
/// The registered `<init>` bridge hands the call to the JDK constructor
/// whenever the class has real bytes (gen r5w4/obs8: before that it ran its
/// own field-only body in their place, so every generational pool fell back
/// to `false/false`).
///
/// A constructor that did not build the sensor a supported threshold needs
/// (no JDK class bytes: the `<init>` bridge's own body ran) leaves that
/// threshold UNSUPPORTED on the bean: `setUsageThreshold` would dereference
/// the null sensor (`getUsageThresholdCount()` does), and an honest `false` is
/// what every pool answered before. Every other pool is the synthetic bean
/// ([`alloc_memory_pool_impl`]), unchanged.
pub(in crate::jmx) fn alloc_heap_pool_impl(
    ctx: &mut dyn NativeContext,
    name: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    let (usage, collection) = ctx.gc_pool_threshold_support(name);
    if !usage && !collection {
        return alloc_memory_pool_impl(ctx, name, true);
    }
    construct_pool_impl(ctx, name, true, usage, collection)
}

/// The constructor path of [`alloc_heap_pool_impl`] (gci i15/nh: shared with
/// [`alloc_non_heap_pool_impl`]): `MemoryPoolImpl(name, isHeap, usage ? 0 :
/// -1, collection ? 0 : -1)`, falling back to the synthetic bean when no
/// object came back, and leaving a supported threshold UNSUPPORTED when the
/// constructor did not build its sensor. A constructor that did not run at
/// all (no JDK bytes and no bridge body: the bean has no name) gets the
/// synthetic bean's fields.
pub(in crate::jmx) fn construct_pool_impl(
    ctx: &mut dyn NativeContext,
    name: &str,
    is_heap: bool,
    usage: bool,
    collection: bool,
) -> Result<ObjectRef, MethodCallFailed> {
    let pool = {
        let n = jmx_fresh_string(ctx, name);
        let mut scope = NativeHandleScope::new(ctx);
        let n_h = scope.root(n);
        let threshold = |supported: bool| Value::Long(if supported { 0 } else { -1 });
        let args = [
            Value::Object(Some(scope.get(&n_h))),
            Value::Int(i32::from(is_heap)),
            threshold(usage),
            threshold(collection),
        ];
        // Roots the new object and the arguments across the constructor.
        match scope.new_object_initialized(
            "sun/management/MemoryPoolImpl",
            "(Ljava/lang/String;ZJJ)V",
            &args,
        )? {
            Some(Value::Object(Some(pool))) => Some(pool),
            _ => None,
        }
    };
    let Some(pool) = pool else {
        return alloc_memory_pool_impl(ctx, name, is_heap);
    };
    // No allocation in this loop: `pool` stays valid.
    for (supported, sensor, flag) in [
        (usage, "usageSensor", "usageThresholdSupported"),
        (collection, "gcSensor", "collectionThresholdSupported"),
    ] {
        let has_sensor = matches!(ctx.get_field_by_name(pool, sensor), Value::Object(Some(_)));
        if supported && !has_sensor {
            ctx.set_field_by_name(pool, flag, Value::Int(0));
        }
    }
    if bean_name(&*ctx, pool).is_some() {
        return Ok(pool);
    }
    // `create_string` allocates: re-read the bean through a pin.
    let pin = ctx.pin_native_root(pool);
    let n = ctx.create_string_uninterned(name);
    let pool = ctx.read_native_pin(pin, pool);
    ctx.unpin_native_roots(pin);
    ctx.set_field_by_name(pool, "name", Value::Object(Some(n)));
    ctx.set_field_by_name(pool, "isHeap", Value::Int(i32::from(is_heap)));
    ctx.set_field_by_name(pool, "isValid", Value::Int(1));
    Ok(pool)
}

/// `GarbageCollectorImpl` beans for `names`, as a `MemoryManagerImpl[]` (the
/// component type `getMemoryManagers0` has always used).
pub(in crate::jmx) fn alloc_collector_bean_array(
    ctx: &mut dyn NativeContext,
    names: &[&'static str],
) -> Result<ObjectRef, MethodCallFailed> {
    alloc_bean_array(
        ctx,
        "sun/management/MemoryManagerImpl",
        names,
        alloc_garbage_collector_impl,
    )
}

/// Read a bean array back as a `Vec`, for `getPlatformMXBeans`'s list builder
/// (which roots each element again before it allocates the list).
pub(in crate::jmx) fn bean_array_to_vec(ctx: &dyn NativeContext, arr: ObjectRef) -> Vec<ObjectRef> {
    (0..ctx.array_length(arr))
        .filter_map(|i| match ctx.get_array_element(arr, i) {
            Value::Object(Some(o)) => Some(o),
            _ => None,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// GC notifications and `getLastGcInfo()` (gen r4w3/obs2, 2026-09-23)
// ---------------------------------------------------------------------------
//
// HotSpot's bean for a collector is `com.sun.management.internal
// .GarbageCollectorExtImpl`, built by the VM through that class's static
// `createGarbageCollector` (`MemoryManager::get_memory_manager_instance`). It is
// the `com.sun.management.GarbageCollectorMXBean` that answers
// `getLastGcInfo()` and emits `GarbageCollectionNotificationInfo`
// notifications — what Micrometer's `jvm.gc.pause` and Spring Boot's GC
// metrics listen to.
//
// When the running collector describes its beans (the generational backend:
// `Copy`, `MarkSweepCompact`; G1 and ZGC since gc-common w7-f — see
// "Per-generation beans"), `MemoryImpl.getMemoryManagers0` now builds
// exactly that bean, and every route that hands collector beans out hands out
// THE SAME ones: the JDK's own `MemoryImpl.mgrs` cache. The VM's drain
// (`vm/src/runtime/interpreter/gc_events.rs::run_gc_notifications`) then calls
// [`deliver_gc_notification`], which is HotSpot's
// `GCNotifier::sendNotificationInternal`: build the `GcInfo` from the bean's
// `GcInfoBuilder` and call `createGCNotification` on the bean. Nothing here
// is per backend: the drain hands over G1's and ZGC's records exactly as the
// generational ones (gc-common w7-f).

pub(in crate::jmx) const GC_EXT_IMPL: &str = "com/sun/management/internal/GarbageCollectorExtImpl";
pub(in crate::jmx) const GC_INFO_BUILDER: &str = "com/sun/management/internal/GcInfoBuilder";
pub(in crate::jmx) const GC_INFO: &str = "com/sun/management/GcInfo";
/// `private GcInfo(GcInfoBuilder, long index, long startTime, long endTime,
/// MemoryUsage[] before, MemoryUsage[] after, Object[] extAttributes)` —
/// HotSpot constructs it from native code too (`createGcInfo`,
/// `GcInfoBuilder.getLastGcInfo0`).
pub(in crate::jmx) const GC_INFO_CTOR: &str = "(Lcom/sun/management/internal/GcInfoBuilder;JJJ\
[Ljava/lang/management/MemoryUsage;[Ljava/lang/management/MemoryUsage;[Ljava/lang/Object;)V";
/// HotSpot's only GC extension attribute (`jmm_GetGCExtAttributeInfo`).
pub(in crate::jmx) const GC_EXT_ATTRIBUTE_NAME: &str = "GcThreadCount";
pub(in crate::jmx) const GC_EXT_ATTRIBUTE_DESC: &str = "Number of GC threads";
/// Its value for Serial's managers (`GCMemoryManager::_num_gc_threads`, never
/// raised by `SerialHeap`) — the shape these beans report.
///
/// Deliberate on every backend that reaches this builder except G1 (gci
/// i19/ob: [`G1_GC_EXT_THREAD_COUNT`]), and it is Serial's number, not a
/// measurement (gen r5w2/obs6 records it here, as
/// `gengc-r4w2-obs-jmx-residuals-on-the-generational-backend` asked): the
/// generational young collection may evacuate with several workers
/// (`CRATONVM_GC_PAR_THREADS`), and its `Copy` bean still says `1`, because
/// the beans report HotSpot Serial's shape and Serial's young collector is
/// single-threaded. The worker count the evacuation used has no JMX answer;
/// it is whatever `CRATONVM_GC_PAR_THREADS` configured.
pub(in crate::jmx) const GC_EXT_THREAD_COUNT: i32 = 1;
/// gci i19/ob (`gci-i14mp-memory-pool-and-gc-mxbean-review-findings-FIXED-20261016`
/// item 6): HotSpot 25's value for G1's managers. `MemoryService::
/// set_universe_heap` counts the collector's threads that exist when the heap
/// is published (`heap->gc_threads_do` with `GcThreadCountClosure`) and gives
/// every manager that count; Serial has none, so its managers keep the
/// constructor's 1. G1 with its default `UseDynamicNumberOfGCThreads` has
/// five then: one parallel worker, the concurrent mark thread, one concurrent
/// mark worker, the primary refinement thread and the service thread. Read
/// from HotSpot 25.0.3's `GciMp14PoolNumbersProbe info` (`GcThreadCount=5` on
/// `G1 Young Generation` and `G1 Old Generation`, a 32-way host, so not the
/// CPU count). Non-default GC thread flags are not modelled.
pub(in crate::jmx) const G1_GC_EXT_THREAD_COUNT: i32 = 5;

/// The `GcThreadCount` a collector bean's `GcInfo` carries: G1's managers
/// (`G1 Young Generation`, `G1 Old Generation`, `G1 Concurrent GC`) report
/// [`G1_GC_EXT_THREAD_COUNT`], every other bean Serial's 1 (ZGC unchanged,
/// out of the gci round's scope).
pub(in crate::jmx) fn gc_ext_thread_count(gc_name: &str) -> i32 {
    if gc_name.starts_with("G1 ") {
        G1_GC_EXT_THREAD_COUNT
    } else {
        GC_EXT_THREAD_COUNT
    }
}

/// `args[idx]` as a non-null reference, without `obj_arg`'s null diagnostics:
/// the callers below treat a null as "not supplied".
pub(in crate::jmx) fn opt_obj(args: &[Value], idx: usize) -> Option<ObjectRef> {
    match args.get(idx) {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    }
}

/// The `name` a memory manager / pool bean carries.
pub(in crate::jmx) fn bean_name(ctx: &dyn NativeContext, bean: ObjectRef) -> Option<String> {
    match ctx.get_field_by_name(bean, "name") {
        Value::Object(Some(s)) => ctx.read_string(s),
        _ => None,
    }
}

/// `getPlatformMXBeans(GarbageCollectorMXBean.class)`'s filter over the
/// manager array (gci i15/nh): keep the beans whose name is one of
/// `collectors`, so the plain non-heap managers (`CodeCacheManager`,
/// `Metaspace Manager`) that lead the array are left out. Out of line for
/// the zt t6/ob4 test (zs s4/rv3 item 13).
pub(in crate::jmx) fn retain_collector_beans(
    ctx: &dyn NativeContext,
    beans: &mut Vec<ObjectRef>,
    collectors: &[&str],
) {
    beans.retain(|b| bean_name(ctx, *b).is_some_and(|n| collectors.iter().any(|c| *c == n)));
}

/// gci i46/hc (`gci-i46hc-bean-and-gc-notification-strings-are-interned-FIXED-20261016`):
/// a String HotSpot 25 builds fresh -- a pool's or collector's name
/// (`MemoryPool::get_memory_pool_instance` / `MemoryManager::
/// get_memory_manager_instance`: `java_lang_String::create_from_str`), a GC
/// notification's name, action and cause (`GCNotifier::sendNotificationInternal`,
/// the same), the `GcThreadCount` attribute's name and description
/// (`GcInfoBuilder.c`'s `NewStringUTF`). None is interned there, so
/// `bean.getName() == "Copy"` and `info.getGcAction() == "end of minor GC"`
/// are `false` on HotSpot; [`NativeContext::create_string`] interned every one
/// here and made them `true`. `CRATONVM_JMX_FRESH_BEAN_STRINGS=0` interns
/// them again.
pub(in crate::jmx) fn jmx_fresh_string(ctx: &mut dyn NativeContext, text: &str) -> ObjectRef {
    {
        ctx.create_string_uninterned(text)
    }
}

/// Is `bean` a `com.sun.management.internal.GarbageCollectorExtImpl`?
pub(in crate::jmx) fn is_gc_ext_bean(ctx: &dyn NativeContext, bean: ObjectRef) -> bool {
    ctx.class_assignable_to_name(ctx.class_id_of_object(bean), GC_EXT_IMPL) == Some(true)
}

/// Milliseconds from `origin` -- the calling VM's start, the origin
/// `RuntimeMXBean.getUptime()` counts from ([`vm_clock`]) -- as
/// `GcInfo.getStartTime()` / `getEndTime()` are. A collection before that
/// origin reads 0. With the process-wide fallback origin (a context that does
/// not report its VM's start; see [`PROCESS_START`]) only a collection in a
/// process that never registered the management natives can predate it, and
/// the two natives that lead to a `GcInfo` still force it.
pub(in crate::jmx) fn ms_since_vm_start(origin: Instant, t: Instant) -> i64 {
    i64::try_from(t.saturating_duration_since(origin).as_millis()).unwrap_or(i64::MAX)
}

/// One collector bean as HotSpot builds it: `GarbageCollectorExtImpl
/// .createGarbageCollector(name, null)`. Falls back to the plain
/// `sun.management.GarbageCollectorImpl` when `jdk.management` is not there
/// to build it (synthetic JDK).
pub(in crate::jmx) fn alloc_gc_ext_bean(
    ctx: &mut dyn NativeContext,
    name: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    let s = jmx_fresh_string(ctx, name);
    // `invoke` roots its arguments across the call.
    let built = ctx.invoke(
        GC_EXT_IMPL,
        "createGarbageCollector",
        "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/management/GarbageCollectorMXBean;",
        &[Value::Object(Some(s)), Value::Object(None)],
    );
    let bean = match built {
        Ok(Some(Value::Object(Some(b)))) if is_gc_ext_bean(&*ctx, b) => b,
        _ => alloc_garbage_collector_impl(ctx, name)?,
    };
    // The constructor chain passes through `GarbageCollectorImpl.<init>
    // (String)`, for which this file registers a bridge (see
    // `register_garbage_collector_impl`). Where that bridge answers, it stores
    // the name but never runs `NotificationEmitterSupport`'s field
    // initialisers, and `addNotificationListener` synchronizes on
    // `listenerLock`. Only null fields are filled, so this is a no-op where the
    // real constructors ran.
    let bean = init_notification_emitter_support(ctx, bean)?;
    if bean_name(&*ctx, bean).is_some() {
        return Ok(bean);
    }
    let pin = ctx.pin_native_root(bean);
    let n = jmx_fresh_string(ctx, name);
    let bean = ctx.read_native_pin(pin, bean);
    init_memory_manager_fields(ctx, bean, Value::Object(Some(n)));
    ctx.unpin_native_roots(pin);
    Ok(bean)
}

/// `MemoryImpl.<getter>()`: the JDK's cached bean array (built once, by the
/// `*0` natives above, under `MemoryImpl`'s class lock). `None` when it cannot
/// be read (no `java.management`, a failing call) or is empty.
pub(in crate::jmx) fn canonical_bean_array(
    ctx: &mut dyn NativeContext,
    getter: &str,
    desc: &str,
) -> Option<ObjectRef> {
    match ctx.invoke("sun/management/MemoryImpl", getter, desc, &[]) {
        Ok(Some(Value::Object(Some(arr)))) if ctx.array_length(arr) > 0 => Some(arr),
        _ => None,
    }
}

/// The collector (manager) beans every route should hand out.
pub(in crate::jmx) fn canonical_collector_beans(ctx: &mut dyn NativeContext) -> Option<ObjectRef> {
    canonical_bean_array(
        ctx,
        "getMemoryManagers",
        "()[Ljava/lang/management/MemoryManagerMXBean;",
    )
}

/// The pool beans every route should hand out.
pub(in crate::jmx) fn canonical_pool_beans(ctx: &mut dyn NativeContext) -> Option<ObjectRef> {
    canonical_bean_array(
        ctx,
        "getMemoryPools",
        "()[Ljava/lang/management/MemoryPoolMXBean;",
    )
}

/// [`canonical_collector_beans`] as a real `java.util.ArrayList`, for
/// `ManagementFactory.getGarbageCollectorMXBeans()`: the collectors only.
/// gci i15/nh: the manager array starts with the non-heap managers
/// (`CodeCacheManager`, `Metaspace Manager`; zs s3/ob2: ZGC's `Metaspace
/// Manager` alone), which are no collectors, so beans are kept by the
/// collector names (it said no filter was needed).
pub(in crate::jmx) fn canonical_collector_list(
    ctx: &mut dyn NativeContext,
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    let Some(arr) = canonical_collector_beans(ctx) else {
        return Ok(None);
    };
    let collectors: Vec<&'static str> = ctx.gc_collectors().iter().map(|c| c.name).collect();
    let mut scope = NativeHandleScope::new(ctx);
    let arr_h = scope.root(arr);
    let list = match scope.new_object_initialized("java/util/ArrayList", "()V", &[])? {
        Some(Value::Object(Some(list))) => list,
        _ => return Ok(None),
    };
    let list_h = scope.root(list);
    let len = {
        let arr = scope.get(&arr_h);
        scope.array_length(arr)
    };
    // The collector beans' slots in the array, in its order, with their names.
    // Nothing allocates in this pass.
    let mut slots: Vec<(usize, String)> = Vec::new();
    for i in 0..len {
        let arr = scope.get(&arr_h);
        if let Value::Object(Some(b)) = scope.get_array_element(arr, i) {
            if let Some(n) = bean_name(&*scope, b) {
                if collectors.iter().any(|c| *c == n.as_str()) {
                    slots.push((i, n));
                }
            }
        }
    }
    // gci i46/hc: HotSpot's list order (see `hotspot_gc_bean_list_order`).
    {
        let order = {
            let names: Vec<&str> = slots.iter().map(|(_, n)| n.as_str()).collect();
            hotspot_gc_bean_list_order(&names)
        };
        let mut ordered = Vec::with_capacity(slots.len());
        for k in order {
            ordered.push(slots[k].clone());
        }
        slots = ordered;
    }
    for (i, _) in slots {
        let arr = scope.get(&arr_h);
        let bean = scope.get_array_element(arr, i);
        let list = scope.get(&list_h);
        scope.invoke(
            "java/util/ArrayList",
            "add",
            "(Ljava/lang/Object;)Z",
            &[Value::Object(Some(list)), bean],
        )?;
    }
    Ok(Some(scope.get(&list_h)))
}



/// Java's `String.hashCode()` of `s` (over its UTF-16 units). Pure.
pub(in crate::jmx) fn java_string_hash_code(s: &str) -> i32 {
    s.encode_utf16()
        .fold(0i32, |h, c| h.wrapping_mul(31).wrapping_add(i32::from(c)))
}

/// gci i46/hc (`gci-i46hc-compatible-gc-bean-list-order-is-not-hotspots-FIXED-20261016`):
/// the order HotSpot 25's `ManagementFactory.getGarbageCollectorMXBeans()`
/// lists collectors `names` in, given in `MemoryImpl.getMemoryManagers()`
/// order: indices into `names`. The JDK answers from its platform component's
/// `nameToMBeanMap()` -- `new HashMap<>(list.size())` keyed by each bean's
/// canonical `ObjectName` (`java.lang:name=<name>,type=GarbageCollector`),
/// filled in manager order -- and lists `values()`: bucket by bucket, each
/// bucket in insertion order. So on G1 (`G1 Young Generation`, `G1 Old
/// Generation`, `G1 Concurrent GC`) the list is Young, Concurrent, Old (the
/// order HotSpot's `getMemoryManagerMXBeans()` lists them in, `vi15-nh`
/// `nh15_list_g1*.hs`); Serial's `Copy`, `MarkSweepCompact` keep theirs.
/// Models `HashMap`'s sizing (`tableSizeFor`, load factor 0.75, a resize
/// when the size passes the threshold, order kept within a bucket across a
/// split) and hash spreading (`h ^ (h >>> 16)`). Pure.
pub(in crate::jmx) fn hotspot_gc_bean_list_order(names: &[&str]) -> Vec<usize> {
    let n = names.len();
    if n == 0 {
        return Vec::new();
    }
    // `new HashMap<>(n)`: capacity `tableSizeFor(n)`, threshold
    // `(int) (capacity * 0.75)` once the table exists; `put` resizes
    // (doubles) when the new size passes it.
    let mut cap = n.next_power_of_two();
    let mut threshold = cap * 3 / 4;
    for size in 1..=n {
        if size > threshold {
            cap <<= 1;
            threshold = cap * 3 / 4;
        }
    }
    let bucket = |name: &str| {
        let canonical = format!("java.lang:name={name},type=GarbageCollector");
        let h = java_string_hash_code(&canonical) as u32;
        ((h ^ (h >> 16)) as usize) & (cap - 1)
    };
    let buckets: Vec<usize> = names.iter().map(|&name| bucket(name)).collect();
    let mut order: Vec<usize> = (0..n).collect();
    // Stable: equal buckets keep insertion order.
    order.sort_by_key(|&i| buckets[i]);
    order
}

/// The bean `name` that `MemoryImpl` handed out, if it has handed any out.
/// Reads the `mgrs` static only — never builds the beans: nobody can have a
/// listener on a bean that was never handed out. Does not allocate.
pub(in crate::jmx) fn registered_gc_ext_bean(ctx: &dyn NativeContext, name: &str) -> Option<ObjectRef> {
    let cid = ctx.class_id_by_name("sun/management/MemoryImpl")?;
    let idx = ctx.static_field_index_by_name(cid, "mgrs")?;
    let Value::Object(Some(mgrs)) = ctx.get_static_field(cid, idx) else {
        return None;
    };
    (0..ctx.array_length(mgrs)).find_map(|i| match ctx.get_array_element(mgrs, i) {
        Value::Object(Some(b))
            if is_gc_ext_bean(ctx, b) && bean_name(ctx, b).as_deref() == Some(name) =>
        {
            Some(b)
        }
        _ => None,
    })
}

/// The beans `names`, in that order, taken from `MemoryImpl`'s cached static
/// array `field` (`mgrs` for the managers, `pools` for the pools) and
/// returned as a fresh `elem_class[]` of those SAME instances. gen r5w2/obs6
/// (`gengc-r4w2-obs-jmx-residuals-on-the-generational-backend`, notification
/// residual): `MemoryPoolImpl.getMemoryManagers0` and
/// `MemoryManagerImpl.getMemoryPools0` built fresh beans with the right names
/// but not the identity `ManagementFactory` hands out, so a listener added to
/// a manager reached from a pool was on a bean no collection notifies.
///
/// `None` — the caller then builds fresh beans, as before — unless `MemoryImpl`
/// has already built the cache and every name is in it. Never builds the cache
/// and never runs Java: this runs inside the JDK's own bean construction,
/// where `MemoryImpl`'s class lock may be held.
pub(in crate::jmx) fn registered_bean_array(
    ctx: &mut dyn NativeContext,
    field: &str,
    elem_class: &str,
    names: &[&'static str],
) -> Option<ObjectRef> {
    let found: Vec<ObjectRef> = {
        let c: &dyn NativeContext = &*ctx;
        let cid = c.class_id_by_name("sun/management/MemoryImpl")?;
        let idx = c.static_field_index_by_name(cid, field)?;
        let Value::Object(Some(cache)) = c.get_static_field(cid, idx) else {
            return None;
        };
        let len = c.array_length(cache);
        let mut beans = Vec::with_capacity(names.len());
        for name in names {
            let bean = (0..len).find_map(|i| match c.get_array_element(cache, i) {
                Value::Object(Some(b)) if bean_name(c, b).as_deref() == Some(*name) => Some(b),
                _ => None,
            })?;
            beans.push(bean);
        }
        beans
    };
    // Rooted before anything allocates: the array below (and a first-use
    // class lookup) can collect and move the beans.
    let mut scope = NativeHandleScope::new(ctx);
    let handles: Vec<_> = found.into_iter().map(|b| scope.root(b)).collect();
    let cid = scope.class_id_by_name(elem_class)?;
    let arr = scope.new_ref_array(cid, handles.len());
    for (i, h) in handles.iter().enumerate() {
        let bean = scope.get(h);
        scope.set_array_element(arr, i, Value::Object(Some(bean)));
    }
    Some(arr)
}

/// The usage `snapshot` records for `pool`; a pool it does not describe is
/// reported with the JMM's undefined `init`/`max` and zero bytes (HotSpot
/// records every pool; a null here would reach the JDK's `MemoryUsage`
/// open-type mapping). Today the builder's pool names come from this same
/// collector's `getMemoryPools0`, so every name is described.
pub(in crate::jmx) fn pool_usage_in(snapshot: &[cratonvm_native_api::GcPoolUsageSnapshot], pool: &str) -> [i64; 4] {
    snapshot
        .iter()
        .find(|p| p.name == pool)
        .map_or([-1, 0, 0, -1], |p| [p.init, p.used, p.committed, p.max])
}

/// Build the `com.sun.management.GcInfo` for `info` against `builder` (a
/// `GcInfoBuilder`), in the builder's pool order — the order `GcInfo`'s
/// constructor zips the two usage arrays against. `given` are
/// `[before, after, extValues]` arrays to fill in place (what
/// `getLastGcInfo0` is handed); each missing or too-short one is allocated.
/// `Ok(None)` if the constructor produced nothing.
pub(in crate::jmx) fn build_gc_info(
    ctx: &mut dyn NativeContext,
    builder: ObjectRef,
    info: &cratonvm_native_api::GcNotificationInfo,
    given: [Option<ObjectRef>; 3],
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    // Read before anything allocates.
    let pool_names: Vec<String> = match ctx.get_field_by_name(builder, "poolNames") {
        Value::Object(Some(arr)) => (0..ctx.array_length(arr))
            .map(|i| match ctx.get_array_element(arr, i) {
                Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                _ => String::new(),
            })
            .collect(),
        _ => Vec::new(),
    };
    // gci i15/nh: the builder's names are `MemoryImpl`'s whole pool list, so
    // the non-heap pools too. No collection samples them; their entry (before
    // and after alike) is their usage now -- HotSpot samples them at both
    // edges, which differ only when the collection unloaded classes.
    //
    // zt t1/ob: only the sources of a non-heap pool the record does not
    // carry at both edges are sampled -- a ZGC cycle record carries
    // `Metaspace` (and `Compressed Class Space`) from its edge walks, a ZGC
    // pause record all five as zeros, and (zt t6/ob4) a ZGC cycle record
    // the three code heaps from its edges too -- so such a `GcInfo` no longer pays a
    // class-store walk whose figures `gc_info_pool_usage` would not read.
    // Off ZGC no record carries one: both sources, as before.
    let (metaspace, code) = gc_info_non_heap_wanted(&pool_names, &info.before, &info.after);
    let non_heap = non_heap_pool_infos_sampled(&*ctx, metaspace, code);
    let n = pool_names.len();
    let mut scope = NativeHandleScope::new(ctx);
    let builder_h = scope.root(builder);
    let given_h: Vec<Option<NativeHandle>> = given
        .into_iter()
        .map(|g| g.map(|o| scope.root(o)))
        .collect();
    let usage_cid = scope
        .ensure_class_initialized("java/lang/management/MemoryUsage")
        .unwrap_or(ClassId::new(0));
    let mut usage_arrays: Vec<NativeHandle> = Vec::with_capacity(2);
    for (k, snapshot) in [&info.before, &info.after].into_iter().enumerate() {
        let supplied = given_h[k]
            .as_ref()
            .map(|h| scope.get(h))
            .filter(|a| scope.array_length(*a) >= n);
        let arr = match supplied {
            Some(arr) => arr,
            None => scope.new_ref_array(usage_cid, n),
        };
        let arr_h = scope.root(arr);
        for (i, pool) in pool_names.iter().enumerate() {
            let [init, used, committed, max] = gc_info_pool_usage(snapshot, &non_heap, pool);
            let mu = alloc_memory_usage(&mut *scope, init, used, committed, max)?;
            let arr = scope.get(&arr_h);
            scope.set_array_element(arr, i, Value::Object(Some(mu)));
        }
        usage_arrays.push(arr_h);
    }
    // `extAttributes`: one `Integer` (`GcThreadCount`), matching
    // `getNumGcExtAttributes` below.
    let supplied_ext = given_h[2]
        .as_ref()
        .map(|h| scope.get(h))
        .filter(|a| scope.array_length(*a) >= 1);
    let ext = match supplied_ext {
        Some(ext) => ext,
        None => {
            let object_cid = scope
                .ensure_class_initialized("java/lang/Object")
                .unwrap_or(ClassId::new(0));
            scope.new_ref_array(object_cid, 1)
        }
    };
    let ext_h = scope.root(ext);
    let count = match scope.invoke(
        "java/lang/Integer",
        "valueOf",
        "(I)Ljava/lang/Integer;",
        &[Value::Int(gc_ext_thread_count(info.gc_name))],
    ) {
        Ok(Some(v @ Value::Object(Some(_)))) => v,
        _ => Value::Object(None),
    };
    let ext = scope.get(&ext_h);
    scope.set_array_element(ext, 0, count);

    // The calling VM's origin (gc-common w17-e), read once for both ends.
    let origin = vm_clock(&*scope).start;
    let start_ms = ms_since_vm_start(origin, info.start);
    let end_ms = ms_since_vm_start(origin, info.end).max(start_ms);
    let args = [
        Value::Object(Some(scope.get(&builder_h))),
        Value::Long(i64::try_from(info.id).unwrap_or(i64::MAX)),
        Value::Long(start_ms),
        Value::Long(end_ms),
        Value::Object(Some(scope.get(&usage_arrays[0]))),
        Value::Object(Some(scope.get(&usage_arrays[1]))),
        Value::Object(Some(scope.get(&ext_h))),
    ];
    // `new_object_initialized` roots the new object and these arguments
    // across the constructor.
    let built = scope.new_object_initialized(GC_INFO, GC_INFO_CTOR, &args)?;
    Ok(match built {
        Some(Value::Object(Some(gc_info))) => Some(gc_info),
        _ => None,
    })
}

/// Deliver one collection's `GarbageCollectionNotificationInfo`: what
/// HotSpot's `GCNotifier::sendNotificationInternal` does on its Notification
/// Thread. Build the `GcInfo` from the bean's own `getGcInfoBuilder()`, then
/// `bean.createGCNotification(timestamp, gcName, gcAction, gcCause, gcInfo)`,
/// which returns at once when the bean has no listener and otherwise sends the
/// JDK's own notification through the JDK's own listener list.
///
/// `Ok(true)` delivered; `Ok(false)` no bean to deliver to (the beans were
/// never handed out, or this is not a `GarbageCollectorExtImpl`); `Err` the
/// Java side threw — for a listener's exception, the JDK's
/// `AssertionError("Error in invoking listener")`.
///
/// Must run where Java may run: never inside a collection. The caller is the
/// VM's post-collection drain.
pub fn deliver_gc_notification(
    ctx: &mut dyn NativeContext,
    info: &cratonvm_native_api::GcNotificationInfo,
) -> Result<bool, MethodCallFailed> {
    // Pin the `GcInfo` time origin no later than the first delivery.
    let _ = process_start();
    let Some(bean) = registered_gc_ext_bean(&*ctx, info.gc_name) else {
        return Ok(false);
    };
    let mut scope = NativeHandleScope::new(ctx);
    let bean_h = scope.root(bean);
    let builder = match scope.invoke(
        GC_EXT_IMPL,
        "getGcInfoBuilder",
        "()Lcom/sun/management/internal/GcInfoBuilder;",
        &[Value::Object(Some(bean))],
    )? {
        Some(Value::Object(Some(builder))) => builder,
        _ => return Ok(false),
    };
    let Some(gc_info) = build_gc_info(&mut *scope, builder, info, [None, None, None])? else {
        return Ok(false);
    };
    let gc_info_h = scope.root(gc_info);
    let name = jmx_fresh_string(&mut *scope, info.gc_name);
    let name_h = scope.root(name);
    let action = jmx_fresh_string(&mut *scope, info.gc_action);
    let action_h = scope.root(action);
    let cause = jmx_fresh_string(&mut *scope, info.gc_cause);
    let args = [
        Value::Object(Some(scope.get(&bean_h))),
        Value::Long(info.timestamp_ms),
        Value::Object(Some(scope.get(&name_h))),
        Value::Object(Some(scope.get(&action_h))),
        Value::Object(Some(cause)),
        Value::Object(Some(scope.get(&gc_info_h))),
    ];
    scope.invoke(
        GC_EXT_IMPL,
        "createGCNotification",
        "(JLjava/lang/String;Ljava/lang/String;Ljava/lang/String;Lcom/sun/management/GcInfo;)V",
        &args,
    )?;
    Ok(true)
}

/// `sun.management.Sensor` — the JDK class whose `trigger(int, MemoryUsage)`
/// / `clear(int)` HotSpot's `SensorInfo` calls.
pub(in crate::jmx) const SENSOR_CLASS: &str = "sun/management/Sensor";

/// The pool bean `name` that `MemoryImpl` handed out (its cached `pools`
/// static), if it has handed any out. Never builds the beans: nobody can have
/// set a threshold on a pool that was never handed out. Does not allocate.
/// gen r5w3/obs7.
pub(in crate::jmx) fn registered_pool_bean(ctx: &dyn NativeContext, name: &str) -> Option<ObjectRef> {
    let cid = ctx.class_id_by_name("sun/management/MemoryImpl")?;
    let idx = ctx.static_field_index_by_name(cid, "pools")?;
    let Value::Object(Some(pools)) = ctx.get_static_field(cid, idx) else {
        return None;
    };
    (0..ctx.array_length(pools)).find_map(|i| match ctx.get_array_element(pools, i) {
        Value::Object(Some(b)) if bean_name(ctx, b).as_deref() == Some(name) => Some(b),
        _ => None,
    })
}

/// Deliver one pool-threshold sensor request (gen r5w3/obs7;
/// `gengc-r5w2-obs6-heap-pools-support-no-usage-thresholds`): what HotSpot's
/// Service Thread does in `SensorInfo::trigger` / `SensorInfo::clear`. The
/// pool bean `MemoryImpl` handed out carries the two sensors its constructor
/// built (`usageSensor`, `gcSensor`); `trigger_usage` `Some` calls
/// `Sensor.trigger(count, new MemoryUsage(..))`, whose `triggerAction` sends
/// the `MEMORY_THRESHOLD_EXCEEDED` / `MEMORY_COLLECTION_THRESHOLD_EXCEEDED`
/// notification through `MemoryImpl.createNotification`; `None` calls
/// `Sensor.clear(count)`.
///
/// `Ok(false)`: no pool bean or no sensor to call (the beans were never handed
/// out, or the pool was built without the JDK's constructor). Must run where
/// Java may run: the VM's post-collection drain.
pub fn deliver_pool_sensor(
    ctx: &mut dyn NativeContext,
    pool: &str,
    collection: bool,
    count: i32,
    trigger_usage: Option<[i64; 4]>,
) -> Result<bool, MethodCallFailed> {
    let Some(bean) = registered_pool_bean(&*ctx, pool) else {
        return Ok(false);
    };
    let field = if collection {
        "gcSensor"
    } else {
        "usageSensor"
    };
    let Value::Object(Some(sensor)) = ctx.get_field_by_name(bean, field) else {
        return Ok(false);
    };
    match trigger_usage {
        Some([init, used, committed, max]) => {
            // The `MemoryUsage` allocation can move the sensor: root it.
            let mut scope = NativeHandleScope::new(ctx);
            let sensor_h = scope.root(sensor);
            let usage = alloc_memory_usage(&mut *scope, init, used, committed, max)?;
            let args = [
                Value::Object(Some(scope.get(&sensor_h))),
                Value::Int(count),
                Value::Object(Some(usage)),
            ];
            scope.invoke(
                SENSOR_CLASS,
                "trigger",
                "(ILjava/lang/management/MemoryUsage;)V",
                &args,
            )?;
        }
        None => {
            ctx.invoke(
                SENSOR_CLASS,
                "clear",
                "(I)V",
                &[Value::Object(Some(sensor)), Value::Int(count)],
            )?;
        }
    }
    Ok(true)
}

/// gci i48b/oe47d: the `gcName` the warm-up's discarded notification carries
/// ([`warm_gc_notification_build`]); never seen by a listener.
pub(in crate::jmx) const GC_NOTIFICATION_WARM_UP_NAME: &str = "warm-up";

/// gci i48b/oe47d (`gci-i47w-ob47-g1-oome-escapes-the-probes-catch-FIXED-20261016`):
/// when a GC bean gets its first listener (`setNotificationEnabled(gc,
/// true)`), build ONE notification for `bean` exactly as the drain's
/// [`deliver_gc_notification`] and the JDK's `createGCNotification` do --
/// `getGcInfoBuilder()`, the `GcInfo`, `GarbageCollectionNotificationInfo`,
/// `GarbageCollectionNotifInfoCompositeData.toCompositeData` (eager:
/// `CompositeDataSupport`, the `GcInfo` view, `TabularDataSupport`, ...), the
/// bean's `getObjectName()` and a `javax.management.Notification` with that
/// user data -- and discard it without sending it. On the registering thread;
/// every failure ignored (the first real build then initializes, or rethrows
/// for, whatever is left, as before).
///
/// Why: the first build initializes the whole delivery closure (51 classes on
/// a normal G1 run: `GcInfoBuilder`, the pool sensors, stream `Nodes`,
/// `MatchOps`, `ArrayType`, `Notification`, ...), and here that cold path is
/// interpreted and slow (0.25-0.7 s on the delivery thread) while the mutator
/// keeps allocating. `GciGo11G1NamesProbe oome` on G1 ran those `<clinit>`s on
/// a full heap: one threw `OutOfMemoryError`, the class became erroneous (JVMS
/// 5.5) and every later notification failed with `NoClassDefFoundError: Could
/// not initialize class ...` (`GcInfoCompositeData` on i48a, `Notification` on
/// i48b once a hand list covered the first). HotSpot runs the same closure on
/// its Notification Thread right after the first pause and never meets a full
/// heap there. No Java-visible state changes: no sequence number is taken
/// (`getNextSeqNumber` is not called), nothing is sent, the bean's builder is
/// the one its first notification would create. Known difference: under
/// `-verbose:class` / `-Xlog:class+load` the closure loads at the first
/// `addNotificationListener` instead of after the first collection.
pub(in crate::jmx) fn warm_gc_notification_build(ctx: &mut dyn NativeContext, bean: ObjectRef) {
    let now = std::time::Instant::now();
    let info = cratonvm_native_api::GcNotificationInfo {
        gc_name: GC_NOTIFICATION_WARM_UP_NAME,
        gc_action: GC_NOTIFICATION_WARM_UP_NAME,
        gc_cause: GC_NOTIFICATION_WARM_UP_NAME,
        id: 0,
        start: now,
        end: now,
        timestamp_ms: 0,
        before: Vec::new(),
        after: Vec::new(),
    };
    let _ = build_discarded_gc_notification(ctx, bean, &info);
}

/// The body of [`warm_gc_notification_build`]: `Ok(true)` when the whole
/// notification was built (and dropped), `Ok(false)` when a step produced
/// nothing, `Err` when one threw.
pub(in crate::jmx) fn build_discarded_gc_notification(
    ctx: &mut dyn NativeContext,
    bean: ObjectRef,
    info: &cratonvm_native_api::GcNotificationInfo,
) -> Result<bool, MethodCallFailed> {
    let mut scope = NativeHandleScope::new(ctx);
    let bean_h = scope.root(bean);
    let builder = match scope.invoke(
        GC_EXT_IMPL,
        "getGcInfoBuilder",
        "()Lcom/sun/management/internal/GcInfoBuilder;",
        &[Value::Object(Some(bean))],
    )? {
        Some(Value::Object(Some(builder))) => builder,
        _ => return Ok(false),
    };
    let Some(gc_info) = build_gc_info(&mut *scope, builder, info, [None, None, None])? else {
        return Ok(false);
    };
    let gc_info_h = scope.root(gc_info);
    let text = jmx_fresh_string(&mut *scope, info.gc_name);
    let text_h = scope.root(text);
    let text = Value::Object(Some(scope.get(&text_h)));
    let args = [text, text, text, Value::Object(Some(scope.get(&gc_info_h)))];
    let built = scope.new_object_initialized(
        "com/sun/management/GarbageCollectionNotificationInfo",
        "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;Lcom/sun/management/GcInfo;)V",
        &args,
    )?;
    let Some(Value::Object(Some(notif_info))) = built else {
        return Ok(false);
    };
    let cd = match scope.invoke(
        "com/sun/management/internal/GarbageCollectionNotifInfoCompositeData",
        "toCompositeData",
        "(Lcom/sun/management/GarbageCollectionNotificationInfo;)\
Ljavax/management/openmbean/CompositeData;",
        &[Value::Object(Some(notif_info))],
    )? {
        Some(Value::Object(Some(cd))) => cd,
        _ => return Ok(false),
    };
    let cd_h = scope.root(cd);
    let receiver = scope.get(&bean_h);
    let source = match scope.invoke_virtual(
        receiver,
        "getObjectName",
        "()Ljavax/management/ObjectName;",
        &[],
    )? {
        Some(Value::Object(Some(name))) => name,
        _ => scope.get(&bean_h),
    };
    let source_h = scope.root(source);
    let kind = jmx_fresh_string(&mut *scope, "com.sun.management.gc.notification");
    let kind_h = scope.root(kind);
    let args = [
        Value::Object(Some(scope.get(&kind_h))),
        Value::Object(Some(scope.get(&source_h))),
        Value::Long(0),
        Value::Long(info.timestamp_ms),
        Value::Object(Some(scope.get(&text_h))),
    ];
    let built = scope.new_object_initialized(
        "javax/management/Notification",
        "(Ljava/lang/String;Ljava/lang/Object;JJLjava/lang/String;)V",
        &args,
    )?;
    let Some(Value::Object(Some(notif))) = built else {
        return Ok(false);
    };
    let cd = scope.get(&cd_h);
    scope.invoke_virtual(
        notif,
        "setUserData",
        "(Ljava/lang/Object;)V",
        &[Value::Object(Some(cd))],
    )?;
    Ok(true)
}

/// The `native` methods of `GarbageCollectorExtImpl` and `GcInfoBuilder`
/// (`libmanagement_ext` on HotSpot). All four are `ACC_NATIVE` in the JDK, so
/// each registration is the implementation, not a shadow: explicit `Bridge`.
pub(in crate::jmx) fn register_gc_notification_natives(r: &mut NativeMethodRegistry) {

    // `setNotificationEnabled(GarbageCollectorMXBean gc, boolean enabled)` —
    // called by `addNotificationListener` on the first listener and by
    // `removeNotificationListener` on the last. Until this existed the call
    // had no native at all.
    r.register(
        GC_EXT_IMPL,
        "setNotificationEnabled",
        "(Lcom/sun/management/GarbageCollectorMXBean;Z)V",
        |ctx, args| {
            let _ = process_start();
            let enabled = matches!(args.get(2), Some(Value::Int(v)) if *v != 0);
            let bean = opt_obj(args, 1).or_else(|| opt_obj(args, 0));
            if let Some(name) = bean.and_then(|b| bean_name(&*ctx, b)) {
                ctx.set_gc_notification_enabled(&name, enabled);
            }
            // gci i48b/oe47d: nothing above is held past this point.
            if enabled {
                if let Some(bean) = bean {
                    warm_gc_notification_build(ctx, bean);                }            }
            Ok(None)
        },
        cratonvm_native_api::NativeKind::Bridge,
    );

    // `GcInfoBuilder.getNumGcExtAttributes(GarbageCollectorMXBean)` — one,
    // `GcThreadCount`, as HotSpot.
    r.register(
        GC_INFO_BUILDER,
        "getNumGcExtAttributes",
        "(Ljava/lang/management/GarbageCollectorMXBean;)I",
        |_ctx, _args| Ok(Some(Value::Int(1))),
        cratonvm_native_api::NativeKind::Bridge,
    );

    // `fillGcAttributeInfo(gc, numAttributes, String[] names, char[] types,
    // String[] descriptions)`.
    r.register(
        GC_INFO_BUILDER,
        "fillGcAttributeInfo",
        "(Ljava/lang/management/GarbageCollectorMXBean;I[Ljava/lang/String;[C[Ljava/lang/String;)V",
        |ctx, args| {
            let n = match args.get(2) {
                Some(Value::Int(n)) => *n,
                _ => 0,
            };
            let (Some(names), Some(types), Some(descs)) =
                (opt_obj(args, 3), opt_obj(args, 4), opt_obj(args, 5))
            else {
                return Ok(None);
            };
            if n < 1 {
                return Ok(None);
            }
            // The char write first: nothing has allocated yet.
            ctx.set_array_element(types, 0, Value::Int(i32::from(b'I')));
            let mut scope = NativeHandleScope::new(ctx);
            let names_h = scope.root(names);
            let descs_h = scope.root(descs);
            let name = jmx_fresh_string(&mut *scope, GC_EXT_ATTRIBUTE_NAME);
            let names = scope.get(&names_h);
            scope.set_array_element(names, 0, Value::Object(Some(name)));
            let desc = jmx_fresh_string(&mut *scope, GC_EXT_ATTRIBUTE_DESC);
            let descs = scope.get(&descs_h);
            scope.set_array_element(descs, 0, Value::Object(Some(desc)));
            Ok(None)
        },
        cratonvm_native_api::NativeKind::Bridge,
    );

    // `getLastGcInfo0(gc, numExtAtts, Object[] extAttValues, char[]
    // extAttTypes, MemoryUsage[] before, MemoryUsage[] after)` — behind
    // `com.sun.management.GarbageCollectorMXBean.getLastGcInfo()`. Null
    // before the bean's first collection (and on backends without a per-bean
    // model), as HotSpot; otherwise the arrays are filled in place and the
    // `GcInfo` built over them, as `libmanagement_ext` does.
    r.register(
        GC_INFO_BUILDER,
        "getLastGcInfo0",
        "(Ljava/lang/management/GarbageCollectorMXBean;I[Ljava/lang/Object;[C\
[Ljava/lang/management/MemoryUsage;[Ljava/lang/management/MemoryUsage;)Lcom/sun/management/GcInfo;",
        |ctx, args| {
            let _ = process_start();
            let (Some(builder), Some(gc)) = (opt_obj(args, 0), opt_obj(args, 1)) else {
                return Ok(Some(Value::Object(None)));
            };
            let Some(info) = bean_name(&*ctx, gc).and_then(|n| ctx.gc_last_collection(&n)) else {
                return Ok(Some(Value::Object(None)));
            };
            let given = [opt_obj(args, 5), opt_obj(args, 6), opt_obj(args, 3)];
            let gc_info = build_gc_info(ctx, builder, &info, given)?;
            Ok(Some(Value::Object(gc_info)))
        },
        cratonvm_native_api::NativeKind::Bridge,
    );

    }

/// `sun.management.OperatingSystemImpl` — process / OS metrics.
///
/// The eight `()J` counters are REAL on Linux, read from `/proc` by the
/// [`os_metrics`] module — the same place `libmanagement` reads them. They
/// were a flat `-1` until 2026-08-13, justified by "CratonVM has no portable
/// in-VM source": true of a portable source, and false of the platform this VM
/// is measured on, where the sibling `getSystemLoadAverage` had been reading
/// `/proc/loadavg` all along. Off Linux they are still the OpenJDK "metric
/// unavailable" sentinel, so a caller can distinguish unavailable from
/// measured.
///
/// The two CPU-*load* doubles remain -1.0, and that is a different question,
/// not an unfinished half of the same one: a load is a fraction over an
/// interval and needs a previous sample, which this bean does not keep.
///
/// (Available-processor count, OS name and arch are real too, and come from
/// the `OperatingSystemMXBean` alloc above rather than from this class.)
pub fn register_operating_system_impl(r: &mut NativeMethodRegistry) {
            let cls = "sun/management/OperatingSystemImpl";

    // REAL on Linux, `-1` elsewhere — see the `os_metrics` module, which is
    // the single implementation these `*0` natives, the JDK 9+ `*0` natives
    // below, and the `com.sun.management.OperatingSystemMXBean` interface
    // methods in `register_operating_system_mxbean` all read. The whole family
    // was a flat `-1` on the stated premise that CratonVM "has no portable
    // in-VM source" for them; that is true of a *portable* source and false of
    // this host, where `/proc` publishes every one of them and the sibling
    // `getSystemLoadAverage` was already reading `/proc/loadavg`.
    for (name, metric) in os_metric_long_natives() {
        r.register(cls, name, "()J", metric, cratonvm_native_api::NativeKind::Bridge);
    }

    // KEEP -1.0: unlike the counters above, a CPU *load* is a fraction over an
    // interval, so answering it needs a previous sample. This bean keeps none,
    // and a single-shot `/proc/stat` read would be a number with no defined
    // meaning rather than a measurement. `-1.0` is the JMM's documented
    // "not available".
    let neg_one_double: fn(&mut dyn NativeContext, &[Value]) -> MethodCallResult =
        |_ctx, _args| Ok(Some(Value::Double(-1.0)));
    for name in ["getSystemCpuLoad0", "getProcessCpuLoad0"] {
        r.register(cls, name, "()D", neg_one_double, cratonvm_native_api::NativeKind::Bridge);
    }

    // KEEP: nothing to establish, provably. The real `initialize0()` opens the
    // platform perf-counter handles the `get*0()` natives then read through;
    // every one of ours answers independently (from the `-1` "metric
    // unavailable" sentinel), so there is no state a body here could set up
    // that anything would later read. It exists purely so `<clinit>` links.

    // JDK 9+ moved the platform OS-bean implementation to
    // `com.sun.management.internal.OperatingSystemImpl` (the old
    // `sun.management.OperatingSystemImpl` triples above are kept for any
    // legacy caller). JDK 25 also renamed several natives:
    //   getFreePhysicalMemorySize0  -> getFreeMemorySize0
    //   getTotalPhysicalMemorySize0 -> getTotalMemorySize0
    //   getSystemCpuLoad0           -> getCpuLoad0
    // `<clinit>` calls the static `initialize0()`; without it the class
    // fails to initialize with UnsatisfiedLinkError, which aborts any
    // `ManagementFactory.getPlatformMBeanServer()` caller. Apache Derby's
    // embedded boot does exactly that (JMXManagementService.boot ->
    // getOperatingSystemMXBean -> OperatingSystemImpl.<clinit>), so the
    // missing native left the whole database service unbooted and the
    // embedded driver unregistered. Same "metric unavailable" -1 / -1.0
    // sentinels as the legacy class.
    let mcls = "com/sun/management/internal/OperatingSystemImpl";
    for (name, metric) in os_metric_long_natives_jdk9() {
        r.register(mcls, name, "()J", metric, cratonvm_native_api::NativeKind::Bridge);
    }
    for name in ["getCpuLoad0", "getProcessCpuLoad0"] {
        r.register(mcls, name, "()D", neg_one_double, cratonvm_native_api::NativeKind::Bridge);
    }
    // KEEP: same reasoning as the legacy class's `initialize0` above — no
    // counter handles for it to open, and no getter here reads any state it
    // could establish.
    r.register(
        mcls,
        "initialize0",
        "()V",
        |_ctx, _args| Ok(None),
        cratonvm_native_api::NativeKind::Bridge,
    );

    // `jdk.internal.platform.CgroupMetrics.isUseContainerSupport()Z` gates
    // `Metrics.getInstance()`: when it returns `false`, `getInstance()`
    // returns `null` immediately, without ever calling
    // `CgroupSubsystemFactory.create()` (which would need real
    // `/sys/fs/cgroup` file parsing we don't implement). Real JDK takes
    // this exact path under `-XX:-UseContainerSupport` or outside a
    // container, and `ManagementFactory`'s callers already handle a null
    // `Metrics` instance. It was an unregistered native (UnsatisfiedLinkError)
    // that aborted `ManagementFactory.getPlatformMBeanServer()` before this
    // fix.
    //
    // KEEP, with two things stated precisely rather than hidden.
    //
    // (a) CratonVM's *launcher* does honour a cgroup CPU quota
    // (`available_processor_count()` prefers
    // `VmConfig::container_effective_processors`) and `VmConfig` carries a real
    // `use_container_support` toggle (default true, `vm/src/config.rs:482`), so
    // "no cgroup awareness at all" would be too strong, and that toggle is not
    // exposed on `NativeContext` — an accessor would be the mechanical fix.
    //
    // (b) But an accessor alone would NOT make `true` correct, which is why
    // this is a KEEP and not an escalation. What this method gates is the
    // `CgroupSubsystem` SPI: answering `true` sends `Metrics.getInstance()`
    // into `CgroupSubsystemFactory.create()`, a `/proc/self/mountinfo` +
    // `/sys/fs/cgroup` walk that is meaningless off Linux and outside a
    // container and that this VM has never exercised. The honest predicate is
    // "the toggle is on AND a cgroup hierarchy is actually mounted", and it has
    // to be validated on a containerised Linux run before
    // `ManagementFactory.getPlatformMBeanServer()` — which is what reaches this
    // — is made to depend on it. `false` is the reachable truth today; a `true`
    // here would be a claim we cannot back.
    r.register(
        "jdk/internal/platform/CgroupMetrics",
        "isUseContainerSupport",
        "()Z",
        |_ctx, _args| Ok(Some(Value::Int(0))),
        cratonvm_native_api::NativeKind::Bridge,
    );

    }

/// gci i47w/dh47: the class JDK 25's `HotSpotDiagnosticMXBean` platform bean
/// is (`jdk.management`). JDK 8's was `sun/management/HotSpotDiagnostic`.
pub(crate) const HOTSPOT_DIAGNOSTIC: &str = "com/sun/management/internal/HotSpotDiagnostic";

/// `com.sun.management.internal.HotSpotDiagnostic` -- the native half of
/// `HotSpotDiagnosticMXBean.dumpHeap`. Its other methods are real JDK
/// bytecode (`getVMOption` / `getDiagnosticOptions` read `Flag.getFlags`,
/// [`register_flag_impl`]).
///
/// gci i47w/dh47 (`gci-i47w-j45-hotspotdiagnostic-dumpheap0-unsatisfied-link-FIXED-20261016`):
/// this used to target JDK 8's `sun/management/HotSpotDiagnostic` and
/// register nothing ("CratonVM has no HPROF writer", stale since
/// `vm/src/runtime/hprof.rs` serves `-XX:+HeapDumpOnOutOfMemoryError` and
/// `jcmd GC.heap_dump`), so `dumpHeap` died with `UnsatisfiedLinkError` in
/// both modes. Now `dumpHeap0(String, Z)V` is a stated `Bridge` (registered in
/// both arms of `vm_init`, kept under `--real-jdk`: it is the only
/// implementation of a `native` method of a real JDK class) answering as
/// HotSpot's `jmm_DumpHeap0`:
///  * the Java side (`HotSpotDiagnostic.dumpHeap`, real bytecode) has already
///    refused a name without `.hprof` (`IllegalArgumentException`, unless
///    `jdk.management.heapdump.allowAnyFileSuffix`) and NPE'd on `null`;
///  * a `null` reaching the native anyway is
///    `NullPointerException("Output file name cannot be null.")`;
///  * the file is opened exclusively before anything else: an existing file
///    is `java.io.IOException` with the bare `os::strerror` text
///    (`File exists`), nothing collected;
///  * `live == true` runs one full collection (`Heap Dump Initiated GC`)
///    first; the walk runs at the dump's own pause;
///  * success returns normally and prints nothing (HotSpot passes no
///    `outputStream` here, unlike `jcmd GC.heap_dump`).
pub fn register_hotspot_diagnostic(r: &mut NativeMethodRegistry) {
            r.register(
        HOTSPOT_DIAGNOSTIC,
        "dumpHeap0",
        "(Ljava/lang/String;Z)V",
        |ctx, args| {
            let path = match args.get(1) {
                Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                _ => {
                    return Err(RuntimeError::NullPointerException {
                        message: Some("Output file name cannot be null.".to_string()),
                    }
                    .into())
                }
            };
            let live = matches!(args.get(2), Some(Value::Int(v)) if *v != 0);
            match ctx.dump_heap_for_mxbean(&path, live) {
                Ok(()) => Ok(None),
                Err(message) => Err(RuntimeError::IOException { message }.into()),
            }
        },
        cratonvm_native_api::NativeKind::Bridge,
    );
    }

/// Fill `out` with `Flag` objects for the CratonVM flags this process actually
/// has a value for — the body of
/// `com.sun.management.internal.Flag.getFlags(String[], Flag[], int)`.
///
/// `names == null` means "every flag" (that is how `Flag.getAllFlags()` calls
/// it); otherwise only the named ones, and unknown names are simply skipped,
/// which is what makes `HotSpotDiagnostic.getVMOption("nope")` raise the JDK's
/// own `IllegalArgumentException` from real bytecode. Returns the number of
/// slots written, as the JDK's caller expects (`for (i = 0; i < count; i++)`).
///
/// Modelled HotSpot rows (cabdb9f4ae) carry their typed value (`Boolean` /
/// `Long`) and their own origin; the manageable `SoftMaxHeapSize` row is
/// writeable (zt t6/tc). Every other reported flag is `writeable = false` /
/// `external = false` with origin
/// `ENVIRON_VAR`: CratonVM's configuration is latched on first read
/// (`cratonvm_types::flags`), so no VM option is settable at runtime and every
/// value that is set came from the environment. That is a measurement, not a
/// placeholder — and it is what makes the four `set*Value` natives below
/// unreachable for a real reason rather than by accident.
///
/// Degrades to 0 written — i.e. exactly the previous behaviour — when there is
/// no real `Flag` class to construct (synthetic-JDK mode) or when its
/// constructor does not have the shape [`FLAG_INIT_DESC`] describes, so a JDK
/// that reshapes it cannot make this misreport.
pub(in crate::jmx) fn native_flag_get_flags(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut objects = args.iter().filter_map(|v| match v {
        Value::Object(o) => Some(*o),
        _ => None,
    });
    let requested = objects.next().flatten();
    let Some(out) = objects.next().flatten() else {
        return Ok(Some(Value::Int(0)));
    };
    let limit = args
        .iter()
        .find_map(|v| match v {
            Value::Int(n) => Some(*n),
            _ => None,
        })
        .unwrap_or(0);
    if limit <= 0 {
        return Ok(Some(Value::Int(0)));
    }
    if ctx.would_fabricate_synthetic_stub(FLAG_CLASS) {
        return Ok(Some(Value::Int(0)));
    }
    // zt t7/tc2: the `int`-origin constructor (the shape these rows were
    // written for) and the `VMOption.Origin` one JDK 9+ really declares. The
    // CratonVM rows keep their previous answer -- listed only where the `int`
    // shape exists, i.e. never on JDK 25 -- and the manageable
    // `SoftMaxHeapSize` row takes whichever exists, the enum shape first.
    let int_ctor = ctx.method_exists(FLAG_CLASS, "<init>", FLAG_INIT_DESC);
    let origin_ctor = ctx.method_exists(FLAG_CLASS, "<init>", FLAG_INIT_DESC_ORIGIN);
    if !int_ctor && !origin_ctor {
        return Ok(Some(Value::Int(0)));
    }

    let soft_max = soft_max_flag_row(&*ctx);
    let all = resolved_vm_flag_settings(&*ctx);
    let hotspot_rows = ctx.hotspot_vm_flags();
    // zt t24/x20: manageable rows are writeable and external (the switch's doc).
    let manageable_rows = jmx_manageable_heap_free_ratios();
    // zt t26/x21: whether this VM's heap is ZGC (its soft-max door exists).
    let zgc_heap = ctx.zgc_soft_max_heap_flag().is_some();
    let wanted: Vec<(String, String)> = match requested {
        None => all,
        Some(names) => {
            let count = ctx.array_length(names);
            let mut picked = Vec::new();
            for i in 0..count {
                let Value::Object(Some(element)) = ctx.get_array_element(names, i) else {
                    continue;
                };
                let Some(name) = ctx.read_string(element) else {
                    continue;
                };
                if let Some(hit) = all.iter().find(|(flag, _)| *flag == name) {
                    picked.push(hit.clone());
                }
            }
            picked
        }
    };

    // `create_string` and `new_object_initialized` both allocate, so `out` (and
    // the name string, across the second allocation) are pinned and re-read
    // rather than held in a bare Rust local across a possible moving GC.
    let capacity = ctx.array_length(out).min(limit as usize);
    let out_pin = ctx.pin_native_root(out);
    let mut out = out;
    let mut written = 0usize;
    for (name, value) in wanted.into_iter().take(capacity) {
        // zt t6/tc: the manageable `SoftMaxHeapSize` row carries a
        // `java.lang.Long` (a `size_t` flag), is writeable and reports its
        // own origin; every CratonVM row is a latched `String`.
        let managed = soft_max.filter(|_| name == SOFT_MAX_FLAG);
        // cabdb9f4ae: a modelled HotSpot row (the same rows PrintFlagsFinal
        // and VM.flags print) carries its typed value and its own origin.
        let hotspot = match managed {
            Some(_) => None,
            None => hotspot_rows.iter().find(|row| row.name == name),
        };
        if managed.is_none() && hotspot.is_none() && !int_ctor {
            // zt t7/tc2: a CratonVM row on a JDK without the `int` shape --
            // the pre-t7 answer (not listed; HotSpot has no such flag).
            continue;
        }
        let name_obj = ctx.create_string(&name);
        let name_pin = ctx.pin_native_root(name_obj);
        let (value_obj, writeable) = match (managed, hotspot) {
            (Some(row), _) => {
                let boxed = ctx.new_object_initialized(
                    "java/lang/Long",
                    "(J)V",
                    &[Value::Long(row.value as i64)], // Cast: a heap size, below i64::MAX
                );
                let Ok(Some(Value::Object(Some(boxed)))) = boxed else {
                    ctx.unpin_native_roots(name_pin);
                    break;
                };
                (boxed, 1)
            }
            (None, Some(row)) if row.ty == "ccstr" => {
                // zt t27/lg1: a `ccstr` row (`HeapDumpPath`) carries a
                // `java.lang.String`, as jmm's `GetVMGlobals` gives it (an
                // unset path reads `""`).
                let text = ctx.create_string(&value);
                (text, i32::from(manageable_rows && row.is_manageable()))
            }
            (None, Some(row)) => {
                // cabdb9f4ae: `bool` rows box as `java.lang.Boolean`, every
                // other modelled row as `java.lang.Long`. zt t28/mf: a
                // `double` row as `java.lang.Double`, as jmm's `GetVMGlobals`.
                let (class, signature, arg) = if row.ty == "bool" {
                    (
                        "java/lang/Boolean",
                        "(Z)V",
                        Value::Int(i32::from(value == "true")),
                    )
                } else if row.ty == "double" {
                    ("java/lang/Double", "(D)V", Value::Double(value.parse().unwrap_or(0.0)))
                } else {
                    (
                        "java/lang/Long",
                        "(J)V",
                        Value::Long(
                            value
                                .parse::<u64>()
                                .map(|v| v as i64)
                                .unwrap_or_else(|_| value.parse().unwrap_or(0)),
                        ),
                    )
                };
                let Ok(Some(Value::Object(Some(boxed)))) =
                    ctx.new_object_initialized(class, signature, &[arg])
                else {
                    ctx.unpin_native_roots(name_pin);
                    break;
                };
                // zt t24/x20: a `{manageable}` row is writeable on HotSpot.
                // zt t26/x21: except the table's `SoftMaxHeapSize` row on a
                // ZGC heap, reached only with ZGC's own row switched off
                // (`CRATONVM_ZGC_SOFT_MAX_MANAGEABLE=0`): read-only there,
                // as that switch's `=0` arm keeps it.
                let zgc_soft_max_off = row.name == SOFT_MAX_FLAG && zgc_heap;
                (
                    boxed,
                    i32::from(manageable_rows && row.is_manageable() && !zgc_soft_max_off),
                )
            }
            (None, None) => (
                ctx.create_string(&value),
                0, // the configuration is latched
            ),
        };
        // The origin as the `VMOption.Origin` constant name (JDK 9+'s
        // constructor) and as `jmm.h`'s code (the `int` shape).
        let (origin_name, jmm_origin) = match (managed, hotspot) {
            (Some(row), _) => (row.origin_name(), row.jmm_origin()),
            (None, Some(row)) => (row.jmx_origin(), row.jmm_origin()),
            (None, None) => ("ENVIRON_VAR", JMM_VMGLOBAL_ORIGIN_ENVIRON_VAR),
        };
        // zt t24/x20: HotSpot's `is_external()` is `is_manageable()`: every
        // writeable row here (`SoftMaxHeapSize`, the manageable modelled
        // rows) under the switch; none with it off, as before.
        let external = if manageable_rows { writeable } else { 0 };
        let enum_origin = origin_ctor && (managed.is_some() || hotspot.is_some());
        let value_pin = ctx.pin_native_root(value_obj);
        let name_obj = ctx.read_native_pin(name_pin, name_obj);
        let value_obj = ctx.read_native_pin(value_pin, value_obj);
        let flag = if enum_origin {
            // zt t7/tc2: the JDK 9+ constructor, with the `VMOption.Origin`
            // constant itself (reaching it may initialise the enum, so the
            // pinned name and value are re-read after it).
            match vm_option_origin(ctx, origin_name) {
                Some(origin_obj) => {
                    let name_obj = ctx.read_native_pin(name_pin, name_obj);
                    let value_obj = ctx.read_native_pin(value_pin, value_obj);
                    ctx.new_object_initialized(
                        FLAG_CLASS,
                        FLAG_INIT_DESC_ORIGIN,
                        &[
                            Value::Object(Some(name_obj)),
                            Value::Object(Some(value_obj)),
                            Value::Int(writeable),
                            Value::Int(external),
                            Value::Object(Some(origin_obj)),
                        ],
                    )
                }
                None => Ok(None),
            }
        } else if int_ctor {
            ctx.new_object_initialized(
                FLAG_CLASS,
                FLAG_INIT_DESC,
                &[
                    Value::Object(Some(name_obj)),
                    Value::Object(Some(value_obj)),
                    Value::Int(writeable),
                    Value::Int(external),
                    Value::Int(jmm_origin),
                ],
            )
        } else {
            Ok(None)
        };
        out = ctx.read_native_pin(out_pin, out);
        ctx.unpin_native_roots(value_pin);
        ctx.unpin_native_roots(name_pin);
        match flag {
            Ok(Some(Value::Object(Some(flag)))) => {
                ctx.set_array_element(out, written, Value::Object(Some(flag)));
                written += 1;
            }
            // Constructing one `Flag` failed; report what was written rather
            // than turning a diagnostic call into a throw.
            _ => break,
        }
    }
    ctx.unpin_native_roots(out_pin);
    Ok(Some(Value::Int(written as i32)))
}

/// zt t27/lg1: `Flag.setBooleanValue(name, value)` -- a modelled manageable
/// `bool` row (`HeapDumpOnOutOfMemoryError`, `PrintClassHistogram`,
/// `ShowCodeDetailsInExceptionMessages`, ...) is written through the VM
/// (`NativeSystemAccess::set_manageable_hotspot_text_flag`); anything else is
/// [`native_flag_set_value_unsupported`].
pub(in crate::jmx) fn native_flag_set_boolean_value(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let name = args.iter().find_map(|arg| match arg {
        Value::Object(Some(obj)) => ctx.read_string(*obj),
        _ => None,
    });
    let value = args.iter().rev().find_map(|v| match v {
        Value::Int(n) => Some(*n != 0),
        _ => None,
    });
    match (name.as_deref(), value) {
        (Some(flag), Some(on)) => {
            match ctx.set_manageable_hotspot_text_flag(flag, if on { "true" } else { "false" }) {
                Some(Ok(())) => Ok(None),
                Some(Err(message)) => {
                    Err(RuntimeError::IllegalArgumentException { message }.into())
                }
                None => native_flag_set_value_unsupported(ctx, args),
            }
        }
        _ => native_flag_set_value_unsupported(ctx, args),
    }
}

/// zt t28/mf: `Flag.setDoubleValue(name, value)` -- a modelled manageable
/// `double` row (`G1PeriodicGCSystemLoadThreshold` on ZGC), written through
/// the VM as [`native_flag_set_boolean_value`] (its text is the `double`'s).
pub(in crate::jmx) fn native_flag_set_double_value(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let name = args.iter().find_map(|arg| match arg {
        Value::Object(Some(obj)) => ctx.read_string(*obj),
        _ => None,
    });
    let value = args.iter().rev().find_map(|v| match v {
        Value::Double(d) => Some(*d),
        _ => None,
    });
    match (name.as_deref(), value) {
        (Some(flag), Some(number)) => {
            match ctx.set_manageable_hotspot_text_flag(flag, &number.to_string()) {
                Some(Ok(())) => Ok(None),
                Some(Err(message)) => {
                    Err(RuntimeError::IllegalArgumentException { message }.into())
                }
                None => native_flag_set_value_unsupported(ctx, args),
            }
        }
        _ => native_flag_set_value_unsupported(ctx, args),
    }
}

/// zt t27/lg1: `Flag.setStringValue(name, value)` -- a modelled manageable
/// `ccstr` row (`HeapDumpPath`), as [`native_flag_set_boolean_value`].
pub(in crate::jmx) fn native_flag_set_string_value(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut texts = args.iter().filter_map(|arg| match arg {
        Value::Object(Some(obj)) => ctx.read_string(*obj),
        _ => None,
    });
    let name = texts.next();
    let value = texts.next();
    match (name.as_deref(), value.as_deref()) {
        (Some(flag), Some(text)) => match ctx.set_manageable_hotspot_text_flag(flag, text) {
            Some(Ok(())) => Ok(None),
            Some(Err(message)) => Err(RuntimeError::IllegalArgumentException { message }.into()),
            None => native_flag_set_value_unsupported(ctx, args),
        },
        _ => native_flag_set_value_unsupported(ctx, args),
    }
}

/// `Flag.set{Long,Double,Boolean,String}Value` — CratonVM's configuration is
/// latched on first read, so no VM option is writeable at runtime.
///
/// This is the same `IllegalArgumentException` HotSpot's own native raises for
/// a non-writeable flag, and the same one `HotSpotDiagnostic.setVMOption`
/// already raises from bytecode after seeing `Flag.isWriteable() == false` —
/// so in practice it is a backstop, not the message a caller normally sees. The
/// previous no-op returned success for a write that never happened.
pub(in crate::jmx) fn native_flag_set_value_unsupported(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // The flag name is the first reference argument under either calling
    // convention (these are static in the JDK, but a synthetic receiver would
    // shift them by one).
    let mut name = String::from("<unknown>");
    for arg in args {
        if let Value::Object(Some(obj)) = arg {
            if let Some(text) = ctx.read_string(*obj) {
                name = text;
                break;
            }
        }
    }
    Err(RuntimeError::IllegalArgumentException {
        message: format!("VM Option \"{name}\" is not writeable"),
    }
    .into())
}

/// `sun.management.Flag` (JDK 8) and `com.sun.management.internal.Flag`
/// (JDK 9+) — VM flag enumeration.
///
/// The JDK 9+ surface is REAL: it reports the CratonVM flags this process
/// actually has a value for, read out of the one latched `VmFlags` snapshot via
/// [`craton_vm_flag_settings`]. `getVMOption("CRATONVM_JIT")` on a run that set
/// it now answers with the value the VM is really running with instead of
/// "does not exist".
///
/// The legacy `sun.management.Flag` triples below stay empty on purpose: that
/// class was removed in JDK 9, so in real-JDK mode (JDK 25) nothing resolves to
/// it at all, and there is no `getFlags` counterpart registered for it — a
/// non-zero `getInternalFlagCount()` there would promise rows that its
/// `getAllFlags()` could not produce. Empty is the self-consistent answer for a
/// class that does not exist.
pub fn register_flag_impl(r: &mut NativeMethodRegistry) {
            let cls = "sun/management/Flag";

    // Empty Flag[] — same reference-array pattern as getAllFlagNames; the
    // element type is `Lsun/management/Flag;` but our synthetic ref-array
    // doesn't carry the element class beyond ClassId::new(0).

    // JDK 9+ exposes the management-flag backend as
    // `com.sun.management.internal.Flag`. This is the live surface, and it is
    // wired to CratonVM's real flag inventory (`cratonvm_types::flag_groups`)
    // rather than to a zero.
    let internal_cls = "com/sun/management/internal/Flag";
    // KEEP: the real `initialize()` caches the jfieldIDs the other natives
    // write `Flag`'s fields through. `native_flag_get_flags` constructs each
    // `Flag` through its constructor and resolves nothing by field id, so there
    // is no cache to fill and nothing that reads one; the registration exists
    // so `Flag.<clinit>` links.
    r.register(
        internal_cls,
        "initialize",
        "()V",
        |_ctx, _args| Ok(None),
        cratonvm_native_api::NativeKind::Bridge,
    );
    // REAL: the number of flags `getFlags(null, …)` will produce, so the two
    // agree. `Flag.getAllFlags()` sizes its `Flag[]` from this value and then
    // trusts `getFlags`'s return, so a count that over- or under-states the
    // enumeration is the one way to break that caller.
    r.register(
        internal_cls,
        "getInternalFlagCount",
        "()I",
        |ctx, _args| {
            // zt t6/tc: plus the manageable `SoftMaxHeapSize` row, if any.
            // zt t24/x20: only the rows `getFlags(null, ..)` lists.
            let rows = listed_vm_flag_settings(&*ctx);
            Ok(Some(Value::Int(rows.len() as i32)))
        },
        cratonvm_native_api::NativeKind::Bridge,
    );
    // REAL: the names behind that count, same order (sorted).
    r.register(
        internal_cls,
        "getAllFlagNames",
        "()[Ljava/lang/String;",
        |ctx, _args| {
            // `String[]`, the declared type (i7-L2), through the one rooted
            // builder.
            // zt t24/x20: the rows `getFlags(null, ..)` lists.
            let names: Vec<String> = listed_vm_flag_settings(&*ctx)
                .into_iter()
                .map(|(name, _)| name)
                .collect();
            let arr = cratonvm_native_builtins::t27_tls::materialize_java_string_array(ctx, &names);
            Ok(Some(Value::Object(Some(arr))))
        },
        NativeKind::Bridge,
    );
    r.register(
        internal_cls,
        "getFlags",
        "([Ljava/lang/String;[Lcom/sun/management/internal/Flag;I)I",
        native_flag_get_flags,
        NativeKind::Bridge,
    );
    // THROW: CratonVM's configuration latches on first read, so no VM option is
    // writeable at runtime — see `native_flag_set_value_unsupported`. The
    // previous no-ops reported success for writes that never happened.
    r.register(
        internal_cls,
        "setLongValue",
        "(Ljava/lang/String;J)V",
        native_flag_set_long_value,
        NativeKind::Bridge,
    );
    r.register(
        internal_cls,
        "setDoubleValue",
        "(Ljava/lang/String;D)V",
        native_flag_set_double_value,
        NativeKind::Bridge,
    );
    // zt t27/lg1 (`CRATONVM_JMX_MANAGEABLE_EFFECT_FLAGS`): a modelled
    // manageable `bool` / `ccstr` row is written through the VM
    // (`set_manageable_hotspot_text_flag`); every other name still throws.
    r.register(
        internal_cls,
        "setBooleanValue",
        "(Ljava/lang/String;Z)V",
        native_flag_set_boolean_value,
        NativeKind::Bridge,
    );
    r.register(
        internal_cls,
        "setStringValue",
        "(Ljava/lang/String;Ljava/lang/String;)V",
        native_flag_set_string_value,
        NativeKind::Bridge,
    );
}

// ---------------------------------------------------------------------------
// 1. ManagementFactory
// ---------------------------------------------------------------------------

pub(in crate::jmx) fn register_management_factory(r: &mut NativeMethodRegistry) {
    let cls = "java/lang/management/ManagementFactory";
    // KEEP: spec-correct. `ManagementFactory` is a final all-static factory
    // whose only constructor is `private ManagementFactory() {}` — an empty
    // body that exists solely to suppress the default one. The class has no
    // instance state, so an empty native is exactly the real behaviour.
    r.register(cls, "<init>", "()V", native_noop_with_this, cratonvm_native_api::NativeKind::Bridge);

    // KAFKA-MBEAN: do NOT register a native for
    // `ManagementFactory.getPlatformMBeanServer()`. The previous synthetic
    // here allocated `try_alloc_concurrent_synthetic("javax/management/MBeanServer", 2)?`,
    // whose Class metadata is the *interface* `javax/management/MBeanServer`.
    // Any subsequent `invokeinterface MBeanServer.registerMBean(...)`
    // (e.g. Kafka's `kafka.utils.CoreUtils$.registerMBean` at
    // `CoreUtils.scala:125`) walks the interface as the receiver class,
    // finds the abstract `registerMBean` declaration with no Code attribute,
    // and throws `AbstractMethodError: ... has no Code attribute` —
    // exactly the failure the no-synthetic-stubs policy
    // (`docs/feature-designs/native-dispatch-architecture.md`) forbids.
    //
    // The real JDK bytecode for `getPlatformMBeanServer()` calls
    // `MBeanServerFactory.createMBeanServer()` which constructs a real
    // `com.sun.jmx.mbeanserver.JmxMBeanServer`. That concrete class declares
    // `registerMBean` with a Code attribute, so the interface dispatch
    // resolves correctly. Leaving this method unregistered lets the real
    // JDK code path run end-to-end.

    // getRuntimeMXBean()
    r.register(
        cls,
        "getRuntimeMXBean",
        "()Ljava/lang/management/RuntimeMXBean;",
        |ctx, _args| {
            let obj = alloc_runtime_mxbean(ctx)?;
            Ok(Some(Value::Object(Some(obj))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // getThreadMXBean()
    r.register(
        cls,
        "getThreadMXBean",
        "()Ljava/lang/management/ThreadMXBean;",
        |ctx, _args| {
            let obj = alloc_thread_mxbean(ctx)?;
            Ok(Some(Value::Object(Some(obj))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // getClassLoadingMXBean()
    r.register(
        cls,
        "getClassLoadingMXBean",
        "()Ljava/lang/management/ClassLoadingMXBean;",
        |ctx, _args| {
            let obj = alloc_class_loading_mxbean(ctx)?;
            Ok(Some(Value::Object(Some(obj))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // getOperatingSystemMXBean()
    r.register(
        cls,
        "getOperatingSystemMXBean",
        "()Ljava/lang/management/OperatingSystemMXBean;",
        |ctx, _args| {
            let obj = alloc_os_mxbean(ctx)?;
            Ok(Some(Value::Object(Some(obj))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // getCompilationMXBean()
    r.register(
        cls,
        "getCompilationMXBean",
        "()Ljava/lang/management/CompilationMXBean;",
        |ctx, _args| {
            let obj = alloc_compilation_mxbean(ctx)?;
            Ok(Some(Value::Object(Some(obj))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // getGarbageCollectorMXBeans() -> List<GarbageCollectorMXBean>
    //
    // Construct a real `java.util.ArrayList` via its `<init>()` + `add()` so
    // the returned List has the JDK's exact field layout (elementData, size,
    // modCount inherited from AbstractList). Writing fields by raw slot to a
    // synthetic ArrayList shadow made callers see `size() == 0` because the
    // synthetic class only had 2 declared slots while the real ArrayList's
    // `size` field lives at a different layout offset — H2's
    // `Utils.getGarbageCollectionCount()` then iterated an apparently-empty
    // list and returned 0, leaving `collectGarbage()`'s
    // `while(count == getCount())` loop spinning indefinitely.
    r.register(
        cls,
        "getGarbageCollectorMXBeans",
        "()Ljava/util/List;",
        |ctx, _args| {
            // gen r4w3/obs2: a collector that describes its beans answers with
            // THE beans (`MemoryImpl`'s cache — see `getPlatformMXBeans`), not
            // one interface-stamped stand-in that is no `NotificationEmitter`.
            if !ctx.gc_collectors().is_empty() {
                if let Some(list) = canonical_collector_list(ctx)? {
                    return Ok(Some(Value::Object(Some(list))));
                }
            }
            let list = match ctx.new_object("java/util/ArrayList")? {
                Some(Value::Object(Some(o))) => o,
                _ => return Ok(Some(Value::Object(None))),
            };
            // gc-common w19-e: the list was held raw across its own
            // constructor (Java) and the bean's allocation, then filled and
            // returned at its address from before them. Rooted.
            let mut scope = NativeHandleScope::new(ctx);
            let list_h = scope.root(list);
            let _ = scope.invoke(
                "java/util/ArrayList",
                "<init>",
                "()V",
                &[Value::Object(Some(list))],
            );
            // gen r5w2/obs6: one synthetic bean per descriptor when the
            // collector describes its beans but the canonical list could not
            // be built (it used to be the one `CratonVM GC` bean, whose count
            // counted a full cycle twice); the historical single bean
            // otherwise.
            let descriptors = scope.gc_collectors();
            let beans: Vec<Option<&cratonvm_native_api::GcCollectorInfo>> =
                if descriptors.is_empty() {
                    vec![None]
                } else {
                    descriptors.iter().map(Some).collect()
                };
            for descriptor in beans {
                let gc = alloc_gc_mxbean(&mut *scope, descriptor)?;
                let list = scope.get(&list_h);
                let _ = scope.invoke(
                    "java/util/ArrayList",
                    "add",
                    "(Ljava/lang/Object;)Z",
                    &[Value::Object(Some(list)), Value::Object(Some(gc))],
                );
            }
            let list = scope.get(&list_h);
            Ok(Some(Value::Object(Some(list))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    }

// ---------------------------------------------------------------------------
// PlatformManagedObject.getObjectName() for the interface-stamped beans
//
// Every `alloc_*_mxbean` below hands back an object stamped with the MXBean
// *interface* (`java/lang/management/RuntimeMXBean`, ...), so the only methods
// that can run on it are the ones registered against that interface name.
// `getObjectName()` is inherited from `java.lang.management.
// PlatformManagedObject` and was never registered, so
// `ManagementFactory.getRuntimeMXBean().getObjectName()` resolved the abstract
// declaration and threw
//
//   AbstractMethodError: method java/lang/management/PlatformManagedObject
//                        .getObjectName()Ljavax/management/ObjectName;
//                        has no Code attribute
//
// — the same failure family this file already documents for
// `RuntimeMXBean.getSystemProperties()`. `javac` emits the *qualifying* type in
// the constant pool (verified: `invokeinterface java/lang/management/
// RuntimeMXBean.getObjectName`), not the declaring interface, which is why
// these registrations go on each concrete MXBean interface and deliberately
// NOT on `PlatformManagedObject` itself: a native on the root interface would
// outrank the exact-class registrations and intercept receivers stamped with
// the concrete `sun.management.*Impl` classes, whose real bytecode already
// answers this correctly.
// ---------------------------------------------------------------------------

/// The canonical platform `ObjectName` text for a bean, keyed by the class the
/// receiver is stamped with.
///
/// `None` means "not one of the beans this file fabricates" — the callback
/// then answers `null`, which is what `PlatformManagedObject` specifies for a
/// bean that is not registered in the platform MBeanServer.
pub(in crate::jmx) fn platform_mxbean_object_name_text(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
) -> Option<String> {
    let class_id = ctx.class_id_of_object(this);
    let class_name = ctx.class_name_of_id(class_id)?;
    let fixed = match class_name.as_str() {
        "java/lang/management/RuntimeMXBean" => "java.lang:type=Runtime",
        "java/lang/management/ThreadMXBean" => "java.lang:type=Threading",
        "java/lang/management/ClassLoadingMXBean" => "java.lang:type=ClassLoading",
        "java/lang/management/OperatingSystemMXBean" => "java.lang:type=OperatingSystem",
        "java/lang/management/CompilationMXBean" => "java.lang:type=Compilation",
        "java/lang/management/PlatformLoggingMXBean" => "java.util.logging:type=Logging",
        "jdk/management/VirtualThreadSchedulerMXBean" => {
            "jdk.management:type=VirtualThreadScheduler"
        }
        // The collector bean is a NAMED platform bean: its ObjectName carries
        // the collector's own name, which `init_gc_mxbean_fields` puts in
        // slot 0. `java.lang:type=GarbageCollector,name=<getName()>` is the
        // key order the JDK builds, i.e. the SOURCE order -- which is what
        // this file's text model wants (see `object_name_set_text`).
        //
        // CORRECTED, lane H6 (2026-08-20): this used to end "...and the order
        // `getCanonicalName()` sorts to, so the text is already canonical."
        // That is false, and it is the only multi-property text this function
        // produces, so it was the one case the claim had to get right.
        // Measured, HotSpot 25.0.3+9 on this host:
        //
        //   new ObjectName("java.lang:type=GarbageCollector,name=G1 Young Generation")
        //     getCanonicalName()         = java.lang:name=G1 Young Generation,type=GarbageCollector
        //     getKeyPropertyListString() = type=GarbageCollector,name=G1 Young Generation
        //
        // Canonical sorts by key, and `name` < `type`. The text below is
        // therefore source order, NOT canonical. Nothing here needs changing --
        // `native_object_name_get_canonical_name` canonicalises on read -- but
        // a future lane that "stops fabricating" must not carry this sentence
        // forward as a licence to treat the two orders as interchangeable.
        "java/lang/management/GarbageCollectorMXBean" => {
            let name = match ctx.get_field(this, 0) {
                Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                _ => String::new(),
            };
            return Some(format!("java.lang:type=GarbageCollector,name={name}"));        }
        _ => return None,
    };
    Some(fixed.to_string())}

pub(in crate::jmx) fn native_platform_managed_object_name(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    match platform_mxbean_object_name_text(ctx, this) {
        Some(text) => {
            // SBR wave 1 (lane JMX): built through `ObjectName.getInstance`,
            // not `object_name_new`. Under `--real-jdk` the ObjectName family
            // is retired (`RETIRED_SHADOW_SBR_JMX_TRIPLES`), so this is the
            // real parser and the name carries real `_canonicalName` /
            // `_kp_array` / `_ca_array` / pattern flags; `object_name_new`
            // wrote only this file's source-order text into `_canonicalName`,
            // and real `toString()` then NPE'd on the null `_kp_array` (the
            // H6-1 §2.5 blocker). Under `--compatible` the call dispatches to
            // `native_object_name_get_instance_string`, which is
            // `object_name_new` on the same text -- the object is identical
            // to the one this site built before. The shared
            // `shared_secrets_bridge.rs` BufferPool producer already took
            // this route.
            let arg = ctx.create_string_uninterned(&text);
            ctx.invoke(
                "javax/management/ObjectName",
                "getInstance",
                "(Ljava/lang/String;)Ljavax/management/ObjectName;",
                &[Value::Object(Some(arg))],
            )
        }
        None => Ok(Some(Value::Object(None))),
    }
}

/// Register `getObjectName()` on every MXBean interface this file stamps its
/// fabricated beans with.
///
/// Exactly the eight interfaces an `alloc_*` helper above passes to
/// `alloc_concurrent_synthetic`, and no more. `MemoryMXBean` is deliberately
/// absent: `alloc_memory_mxbean` stamps the concrete `sun/management/MemoryImpl`
/// instead, whose real bytecode answers `getObjectName()` on its own. Each row
/// is `Bridge` and binds an abstract interface method, so each one adds to the
/// `bridge.abstract_method` census bucket — the slack-free
/// `bridge_without_acc_native` ratchet has to be re-frozen with this change.
pub(in crate::jmx) fn register_platform_managed_object_names(r: &mut NativeMethodRegistry) {
    for cls in [
        "java/lang/management/RuntimeMXBean",
        "java/lang/management/ThreadMXBean",
        "java/lang/management/ClassLoadingMXBean",
        "java/lang/management/OperatingSystemMXBean",
        "java/lang/management/CompilationMXBean",
        "java/lang/management/PlatformLoggingMXBean",
        "java/lang/management/GarbageCollectorMXBean",
        "jdk/management/VirtualThreadSchedulerMXBean",
    ] {
        r.register(
            cls,
            "getObjectName",
            "()Ljavax/management/ObjectName;",
            native_platform_managed_object_name, cratonvm_native_api::NativeKind::Bridge,
        );
    }
    }

// ---------------------------------------------------------------------------
// 2. RuntimeMXBean — 10-field synthetic
// ---------------------------------------------------------------------------

pub(in crate::jmx) fn alloc_runtime_mxbean(ctx: &mut dyn NativeContext) -> Result<ObjectRef, MethodCallFailed> {
    let mut obj = try_alloc_concurrent_synthetic(ctx, "java/lang/management/RuntimeMXBean", 10)?;
    init_runtime_mxbean_fields(ctx, &mut obj)?;
    Ok(obj)
}

/// Pins `obj` across [`init_runtime_mxbean_fields_body`] and hands the refreshed reference back.
///
/// The receiver is `&mut` on purpose. The body ALLOCATES and returns no
/// reference, so a moving collector could relocate `obj` inside the call and
/// every caller was left holding a pre-move address -- the shape
/// `WORKER-5-NOTE-10` traced `TreeMap.size()` returning 0 to. `&mut` makes
/// forgetting the refresh a COMPILE ERROR instead of an audit finding.
pub(in crate::jmx) fn init_runtime_mxbean_fields(
    ctx: &mut dyn NativeContext,
    obj: &mut ObjectRef,
) -> Result<(), MethodCallFailed> {
    let w5_pin = ctx.pin_native_root(*obj);
    let w5_out = init_runtime_mxbean_fields_body(ctx, *obj);
    *obj = ctx.read_native_pin(w5_pin, *obj);
    ctx.unpin_native_roots(w5_pin);
    w5_out
}

/// Populate the 10 synthetic `RuntimeMXBean` slots the getters below read by
/// index. Shared by the factory path (`alloc_runtime_mxbean`) and by the
/// `<init>` native, so a bean carries the same state however it was built —
/// without this, a directly-constructed bean answers `getName() == null` and
/// hands back an untyped default slot for the `long` getters.
pub(in crate::jmx) fn init_runtime_mxbean_fields_body(
    ctx: &mut dyn NativeContext,
    obj: ObjectRef,
) -> Result<(), MethodCallFailed> {
    let pid = std::process::id();
    // `RuntimeMXBean.getName()` is specified only as "a name representing the
    // running VM", but every JDK implements it as `pid + "@" + hostname`
    // (`VMManagementImpl.getVmId()`), and — the part that matters here — the
    // platform MBeanServer's `java.lang:type=Runtime` `Name` attribute is
    // answered by that REAL JDK code even when `getRuntimeMXBean()` hands back
    // this synthetic. `"cratonvm@<pid>"` therefore made one VM report two
    // different names for itself depending on which accessor you asked
    // (`RJdkJmx.platformBeans` pins them equal), and it was not even the
    // shape any JDK uses. Build `pid@host` instead.
    //
    // Host name source, stated exactly because it is NOT the same function the
    // JDK's `InetAddress.getLocalHost()` ends up in: that goes to
    // `Inet{4,6}AddressImpl.getLocalHostName`, i.e. `inet_address.rs`'s
    // `local_host_name()` (a raw `gethostname`), which is private to that
    // module. `resolve_real_hostname()` is this crate's other, cached resolver
    // (`HOSTNAME` env, then `COMPUTERNAME`, then the `hostname` binary,
    // FQDN-trimmed to the short form for the same reason). The two answer the
    // same short host name on every supported platform in the normal case, and
    // were MEASURED equal on the Windows gate host; if a host is ever found
    // where they differ, the fix is to make `local_host_name()` `pub(crate)`
    // and call it here, not to widen the assertion.
    //
    // gc-common w16-f: every String below is an allocation, and each slot --
    // and the argument list's two, after the empty array -- used to be written
    // through the address from before it. The wrapper's pin hands the CALLER
    // the current address; this scope does the same for every store in here.
    let mut scope = NativeHandleScope::new(ctx);
    let obj_h = scope.root(obj);
    // Interpreter round i6 wave 118, lane L1 (i61-L1 stage 2): the VM's
    // name, version and vendor and the specification's, as
    // `System.getProperty` answers them (HotSpot's `RuntimeImpl` reads the
    // same `java.vm.*` properties). The literals here (`CratonVM`, `0.1.0`,
    // `Craton`) disagreed with the property map (`cratonvm`, `25.0.1+8`).
    let identity =
        |key: &str| cratonvm_native_builtins::phases_late::management::vm_identity_property(&*scope, key);
    let texts = [
        format!("{}@{}", pid, cratonvm_native_builtins::resolve_real_hostname()),
        identity("java.vm.name"),
        identity("java.vm.version"),
        identity("java.vm.vendor"),
        identity("java.vm.specification.name"),
        identity("java.vm.specification.version"),
        identity("java.vm.specification.vendor"),
    ];
    for (slot, text) in texts.iter().enumerate() {
        let s = scope.create_string(text);
        let obj = scope.get(&obj_h);
        scope.set_field(obj, slot, Value::Object(Some(s)));
    }
    let obj = scope.get(&obj_h);
    let start_ms = vm_start_epoch_ms(&*scope);
    let up_ms = uptime_ms(&*scope);
    scope.set_field(obj, 7, Value::Long(start_ms as i64));
    scope.set_field(obj, 8, Value::Long(up_ms as i64));
    // field 9 = inputArguments (empty ArrayList)
    //
    // Slots 0..=9 above are a CratonVM-fabricated carrier and the indices are
    // legitimate: `java.lang.management.RuntimeMXBean` is an INTERFACE, so the
    // real JDK layout it is stamped with has ZERO instance fields and there is
    // nothing to collide with. `java.util.ArrayList` is the opposite case.
    //
    // H6-B, 2026-08-20. `javap -p java.util.ArrayList` / `java.util.AbstractList`
    // on JDK 25.0.3+9, instance fields in declaration order:
    //
    //     0  protected transient int      modCount      (AbstractList)
    //     1  transient java.lang.Object[] elementData   (ArrayList)
    //     2  private int                  size          (ArrayList)
    //
    // The fixed `0 = array, 1 = size` used here is the SYNTHETIC carrier's
    // convention. Against the real layout -- which is what the widening in
    // `try_alloc_concurrent_synthetic` hands back in real-JDK mode -- it put an
    // OOP into `modCount` (an int) and an `Int` into `elementData` (a
    // reference the GC scans as an oop). That is not a wrong answer, it is the
    // heap-corruption species of `docs/architecture/natives-over-real-jdk-classes.md`
    // §5: `Int(0)` in a reference slot is a bogus pointer for the collector to
    // mark and move.
    //
    // The other two `java/util/ArrayList` allocations in this file (the
    // component-list registration and `init_notification_emitter_support`)
    // already write by NAME. This was the one call site that did not -- the
    // "correct helper exists but only one call site uses it" shape, inverted.
    //
    // Resolve the INDEX by name and keep the fixed indices only as the
    // fallback, rather than switching to `set_field_by_name`: that setter is a
    // documented NO-OP when the field is absent, and the synthetic carrier
    // mints fields with no names, so a blind switch would silently drop both
    // writes there.
    let args_list: ObjectRef =
        try_alloc_concurrent_synthetic(&mut *scope, "java/util/ArrayList", 2)?;
    let list_h = scope.root(args_list);
    let empty_arr = scope.new_ref_array(ClassId::new(0), 0);
    let args_list = scope.get(&list_h);
    let list_cid = scope.class_id_of_object(args_list);
    let elem_slot = scope
        .resolve_field_index_by_class_id(list_cid, "elementData")
        .unwrap_or(0);
    let size_slot = scope
        .resolve_field_index_by_class_id(list_cid, "size")
        .unwrap_or(1);
    scope.set_field(args_list, elem_slot, Value::Object(Some(empty_arr)));
    scope.set_field(args_list, size_slot, Value::Int(0));
    let obj = scope.get(&obj_h);
    scope.set_field(obj, 9, Value::Object(Some(args_list)));
    Ok(())
}

pub(in crate::jmx) fn register_runtime_mxbean(r: &mut NativeMethodRegistry) {
    let cls = "java/lang/management/RuntimeMXBean";
    // `RuntimeMXBean` is an interface, so this can only ever run for a
    // CratonVM synthetic receiver — but a synthetic receiver arrives with
    // every slot at its untyped default, which is precisely the state the
    // index-based getters below cannot read. Establish the same state the
    // factory does.
    r.register(cls, "<init>", "()V", |ctx, args| {
        let mut this = obj_arg(args, 0)?;
        init_runtime_mxbean_fields(ctx, &mut this)?;
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);

    r.register(cls, "getName", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(cls, "getVmName", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 1)))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(cls, "getVmVersion", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 2)))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(cls, "getVmVendor", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 3)))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(cls, "getSpecName", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 4)))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(
        cls,
        "getSpecVersion",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 5)))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(cls, "getSpecVendor", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 6)))
    }, cratonvm_native_api::NativeKind::Bridge);
    // getManagementSpecVersion() -- the JMX Management Interface spec
    // version (distinct from getSpecVersion(), which is the JVM Language
    // Spec version). Not one of the 10 synthetic fields; a plain constant
    // is enough since callers (e.g. Tomcat's ManagerServlet vminfo command)
    // just print it.
    r.register(
        cls,
        "getManagementSpecVersion",
        "()Ljava/lang/String;",
        |ctx, _args| {
            let s = ctx.create_string("1.2");
            Ok(Some(Value::Object(Some(s))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(cls, "getStartTime", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 7)))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(cls, "getUptime", "()J", |ctx, _args| {
        Ok(Some(Value::Long(uptime_ms(&*ctx) as i64)))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(
        cls,
        "getInputArguments",
        "()Ljava/util/List;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 9)))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(cls, "getClassPath", "()Ljava/lang/String;", |ctx, _args| {
        let cp = ctx
            .get_system_property("java.class.path")
            .unwrap_or_default();
        let s = ctx.create_string(&cp);
        Ok(Some(Value::Object(Some(s))))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(
        cls,
        "getLibraryPath",
        "()Ljava/lang/String;",
        |ctx, _args| {
            let lp = ctx
                .get_system_property("java.library.path")
                .unwrap_or_default();
            let s = ctx.create_string(&lp);
            Ok(Some(Value::Object(Some(s))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // REAL: the boot class path the VM published, not a hard-coded "". Read
    // through the same `boot_class_path` helper as the support query below so
    // the pair can never contradict each other.
    r.register(
        cls,
        "getBootClassPath",
        "()Ljava/lang/String;",
        |ctx, _args| {
            let cp = boot_class_path(ctx).unwrap_or_default();
            let s = ctx.create_string(&cp);
            Ok(Some(Value::Object(Some(s))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // REAL: whether the VM established a boot class path at all — i.e. whether
    // `sun.boot.class.path` was populated — rather than a flat `false`.
    //
    // The previous justification for that `false` was wrong on its own terms:
    // it claimed CratonVM "has no boot class path to report", but the VM does
    // establish one (`VmConfig::boot_classpath` / `discover_boot_classpath`,
    // `-Xbootclasspath`) and publishes the property at every boot
    // (`system_bootstrap.rs`). On a modular image the published value is the
    // EMPTY string, which is a real answer — exactly what HotSpot reports with
    // nothing appended via `-Xbootclasspath/a` — and not the "no answer" the
    // old `false` implied. Absence of the property is still reported as
    // unsupported, which is the case the spec's guard actually exists for.
    r.register(cls, "isBootClassPathSupported", "()Z", |ctx, _args| {
        Ok(Some(Value::Int(i32::from(boot_class_path(ctx).is_some()))))
    }, cratonvm_native_api::NativeKind::Bridge);
    // getSystemProperties() -> Map<String,String>. Used by Elasticsearch's
    // `JvmInfo.<clinit>` (and many frameworks) to snapshot the system props.
    // The synthetic `RuntimeMXBean` is an interface object, so an unregistered
    // method falls through to the abstract interface method and raises
    // `AbstractMethodError: ... has no Code attribute`. Return the live
    // `System.getProperties()` (a `Properties`, i.e. a `Map` whose values are
    // all `String`s) which satisfies the `Map<String,String>` contract callers
    // use (get / containsKey / entrySet iteration).
    r.register(
        cls,
        "getSystemProperties",
        "()Ljava/util/Map;",
        |ctx, _args| {
            ctx.invoke(
                "java/lang/System",
                "getProperties",
                "()Ljava/util/Properties;",
                &[],
            )
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    }

// ---------------------------------------------------------------------------
// PlatformLoggingMXBean -- 0-field synthetic (no per-instance state; the
// real java.util.logging.LogManager backs all queries directly).
//
// Needed by Tomcat's Diagnostics.getVMInfo() (manager vminfo command),
// via ManagementFactory.getPlatformMXBean(PlatformLoggingMXBean.class).
// Before this, that lookup fell through getPlatformMXBean's "_ => None"
// arm, leaving Diagnostics' loggingMXBean field null and turning its
// final getLoggerNames() call into a NullPointerException instead of the
// AbstractMethodError family this whole MXBean surface otherwise hits.
// ---------------------------------------------------------------------------

pub(in crate::jmx) fn alloc_logging_mxbean(ctx: &mut dyn NativeContext) -> Result<ObjectRef, MethodCallFailed> {
    Ok(try_alloc_concurrent_synthetic(
        ctx,
        "java/lang/management/PlatformLoggingMXBean",
        0,
    )?)
}

/// The live `java.util.logging.LogManager` singleton.
///
/// `logmanager.rs` registers `LogManager.getLogManager()` for both run modes,
/// so this is the same object real bytecode would obtain. `None` when that
/// surface is unreachable — every caller below then falls back to its previous
/// answer rather than throwing, so wiring these up cannot make a currently
/// working call start failing.
pub(in crate::jmx) fn jul_log_manager(ctx: &mut dyn NativeContext) -> Option<ObjectRef> {
    match ctx.invoke(
        "java/util/logging/LogManager",
        "getLogManager",
        "()Ljava/util/logging/LogManager;",
        &[],
    ) {
        Ok(Some(Value::Object(Some(mgr)))) => Some(mgr),
        _ => None,
    }
}

/// Resolve a logger by name through the live `LogManager`.
///
/// `None` is JMX's "no such logger" case, which the `PlatformLoggingMXBean`
/// accessors map to a `null` return — deliberately distinct from the empty
/// string, which means "this logger exists but has no level of its own".
pub(in crate::jmx) fn jul_logger_by_name(ctx: &mut dyn NativeContext, name: &str) -> Option<ObjectRef> {
    let mgr = jul_log_manager(ctx)?;
    // `create_string` allocates, which can relocate the manager.
    let pin = ctx.pin_native_root(mgr);
    let key = ctx.create_string(name);
    let mgr = ctx.read_native_pin(pin, mgr);
    let logger = ctx.invoke_virtual(
        mgr,
        "getLogger",
        "(Ljava/lang/String;)Ljava/util/logging/Logger;",
        &[Value::Object(Some(key))],
    );
    ctx.unpin_native_roots(pin);
    match logger {
        Ok(Some(Value::Object(Some(logger)))) => Some(logger),
        _ => None,
    }
}

/// Read a `String` argument, or `None` when the slot is absent/null.
pub(in crate::jmx) fn opt_string_arg(ctx: &dyn NativeContext, args: &[Value], index: usize) -> Option<String> {
    match args.get(index) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s),
        _ => None,
    }
}

pub(in crate::jmx) fn register_platform_logging_mxbean(r: &mut NativeMethodRegistry) {
    let cls = "java/lang/management/PlatformLoggingMXBean";
    // KEEP: all-defaults is provably the correct fresh state. This bean is
    // the file's one genuinely stateless MXBean — `alloc_logging_mxbean`
    // allocates it with ZERO fields, and every method below answers from the
    // live `LogManager` rather than from per-instance state, so there is
    // nothing a constructor could establish.
    r.register(cls, "<init>", "()V", native_noop_with_this, cratonvm_native_api::NativeKind::Bridge);
    // REAL: the live LogManager's logger names. `Collections.list` does the
    // Enumeration walk in one call, so no unpinned reference is held across
    // the allocations that walk performs. Falls back to the previous empty
    // list when either surface is unreachable (synthetic-JDK mode).
    r.register(cls, "getLoggerNames", "()Ljava/util/List;", |ctx, _args| {
        if let Some(mgr) = jul_log_manager(ctx) {
            let names = ctx.invoke_virtual(mgr, "getLoggerNames", "()Ljava/util/Enumeration;", &[]);
            if let Ok(Some(Value::Object(Some(enumeration)))) = names {
                let listed = ctx.invoke(
                    "java/util/Collections",
                    "list",
                    "(Ljava/util/Enumeration;)Ljava/util/ArrayList;",
                    &[Value::Object(Some(enumeration))],
                );
                if let Ok(Some(v @ Value::Object(Some(_)))) = listed {
                    return Ok(Some(v));
                }
            }
        }
        match ctx.new_object_initialized("java/util/ArrayList", "()V", &[]) {
            Ok(Some(v @ Value::Object(Some(_)))) => Ok(Some(v)),
            _ => Ok(Some(Value::Object(None))),
        }
    }, cratonvm_native_api::NativeKind::Bridge);
    // REAL: the named logger's own level, read through the live LogManager.
    // The three answers are distinct and callers branch on all three:
    //   null   — no such logger
    //   ""     — the logger exists but inherits its level from its parent
    //   "INFO" — the level's name
    // The previous flat `null` told every caller that no logger in the whole
    // process existed, which is the one answer that is never true.
    r.register(
        cls,
        "getLoggerLevel",
        "(Ljava/lang/String;)Ljava/lang/String;",
        |ctx, args| {
            let Some(name) = opt_string_arg(ctx, args, 1) else {
                return Ok(Some(Value::Object(None)));
            };
            let Some(logger) = jul_logger_by_name(ctx, &name) else {
                return Ok(Some(Value::Object(None)));
            };
            // `getLevel()` / `getName()` allocate; keep the logger rooted.
            let pin = ctx.pin_native_root(logger);
            let level = ctx.invoke_virtual(logger, "getLevel", "()Ljava/util/logging/Level;", &[]);
            let named = match level {
                Ok(Some(Value::Object(Some(level)))) => ctx
                    .invoke_virtual(level, "getName", "()Ljava/lang/String;", &[])
                    .ok()
                    .flatten(),
                _ => None,
            };
            ctx.unpin_native_roots(pin);
            Ok(Some(match named {
                Some(v @ Value::Object(Some(_))) => v,
                _ => Value::Object(Some(ctx.create_string(""))),
            }))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // REAL: apply the level to the named logger. Dropping the call made every
    // JMX-driven log-level change silently ineffective — the caller has no
    // way to detect that, and `getLoggerLevel` above would have reported the
    // change as applied once it started answering truthfully.
    // A null/empty level name clears the logger's level (inherit), matching
    // the MXBean contract.
    r.register(
        cls,
        "setLoggerLevel",
        "(Ljava/lang/String;Ljava/lang/String;)V",
        |ctx, args| {
            let Some(name) = opt_string_arg(ctx, args, 1) else {
                return Ok(None);
            };
            let level_name = opt_string_arg(ctx, args, 2);
            let Some(logger) = jul_logger_by_name(ctx, &name) else {
                // The spec's answer is IllegalArgumentException, but a logger
                // this VM has not materialised yet is a CratonVM gap rather
                // than a caller error — stay quiet rather than fail a call a
                // real JVM would have accepted.
                return Ok(None);
            };
            // `Level.parse` allocates and can relocate the logger.
            let pin = ctx.pin_native_root(logger);
            let level = match level_name {
                Some(ref text) if !text.is_empty() => {
                    let key = ctx.create_string(text);
                    ctx.invoke(
                        "java/util/logging/Level",
                        "parse",
                        "(Ljava/lang/String;)Ljava/util/logging/Level;",
                        &[Value::Object(Some(key))],
                    )
                    .ok()
                    .flatten()
                    .unwrap_or(Value::Object(None))
                }
                _ => Value::Object(None),
            };
            let logger = ctx.read_native_pin(pin, logger);
            let _ =
                ctx.invoke_virtual(logger, "setLevel", "(Ljava/util/logging/Level;)V", &[level]);
            ctx.unpin_native_roots(pin);
            Ok(None)
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // REAL: the named logger's parent name.
    //   null — no such logger
    //   ""   — this IS the root logger (JUL's root logger is itself named "")
    // The previous body answered `""` for every name, telling anything that
    // builds a logger tree that every logger was the root.
    r.register(
        cls,
        "getParentLoggerName",
        "(Ljava/lang/String;)Ljava/lang/String;",
        |ctx, args| {
            let Some(name) = opt_string_arg(ctx, args, 1) else {
                return Ok(Some(Value::Object(None)));
            };
            let Some(logger) = jul_logger_by_name(ctx, &name) else {
                return Ok(Some(Value::Object(None)));
            };
            let pin = ctx.pin_native_root(logger);
            let parent =
                ctx.invoke_virtual(logger, "getParent", "()Ljava/util/logging/Logger;", &[]);
            let parent_name = match parent {
                Ok(Some(Value::Object(Some(parent)))) => ctx
                    .invoke_virtual(parent, "getName", "()Ljava/lang/String;", &[])
                    .ok()
                    .flatten(),
                _ => None,
            };
            ctx.unpin_native_roots(pin);
            Ok(Some(match parent_name {
                Some(v @ Value::Object(Some(_))) => v,
                _ => Value::Object(Some(ctx.create_string(""))),
            }))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    }

// ---------------------------------------------------------------------------
// 3. MemoryMXBean — concrete real-JDK MemoryImpl
// ---------------------------------------------------------------------------

/// Populate the inherited state that `NotificationEmitterSupport` normally
/// establishes in its Java constructor.
///
/// Synthetic management beans are allocated without running Java constructors.
/// `sun.management.MemoryImpl` inherits `NotificationEmitterSupport`, whose
/// `addNotificationListener` synchronizes on `listenerLock` and then mutates
/// `listenerList`. Leaving either field null makes a real JMX client (notably
/// Micrometer's `JvmHeapPressureMetrics`) fail during bootstrap.
pub(in crate::jmx) fn init_notification_emitter_support(
    ctx: &mut dyn NativeContext,
    emitter: ObjectRef,
) -> Result<ObjectRef, MethodCallFailed> {
    // Both construction paths can allocate and relocate the receiver, so keep
    // it rooted and re-read it before every field access.
    let pin = ctx.pin_native_root(emitter);
    let current = ctx.read_native_pin(pin, emitter);
    if !matches!(
        ctx.get_field_by_name(current, "listenerLock"),
        Value::Object(Some(_))
    ) {
        let lock = match ctx.new_object("java/lang/Object") {
            Ok(Some(Value::Object(Some(lock)))) => lock,
            _ => try_alloc_concurrent_synthetic(ctx, "java/lang/Object", 0)?,
        };
        let current = ctx.read_native_pin(pin, emitter);
        ctx.set_field_by_name(current, "listenerLock", Value::Object(Some(lock)));
    }

    let current = ctx.read_native_pin(pin, emitter);
    if !matches!(
        ctx.get_field_by_name(current, "listenerList"),
        Value::Object(Some(_))
    ) {
        let list = match ctx.new_object_initialized("java/util/ArrayList", "()V", &[]) {
            Ok(Some(Value::Object(Some(list)))) => list,
            // Synthetic-mode fallback. The real initialized ArrayList above
            // is required for real-JDK mode and covered by the probe.
            _ => try_alloc_concurrent_synthetic(ctx, "java/util/ArrayList", 2)?,
        };
        let current = ctx.read_native_pin(pin, emitter);
        ctx.set_field_by_name(current, "listenerList", Value::Object(Some(list)));
    }

    let result = ctx.read_native_pin(pin, emitter);
    ctx.unpin_native_roots(pin);
    Ok(result)
}

pub(in crate::jmx) fn alloc_memory_mxbean(ctx: &mut dyn NativeContext) -> Result<ObjectRef, MethodCallFailed> {
    // `MemoryMXBean` is an interface. Returning an object stamped with that
    // interface makes `instanceof NotificationEmitter` false and hides
    // MemoryImpl's inherited listener implementation. The concrete class's
    // registered `getMemoryUsage0` bridge still supplies live heap values.
    let obj = try_alloc_concurrent_synthetic(ctx, "sun/management/MemoryImpl", 1)?;
    let obj = init_notification_emitter_support(ctx, obj)?;
    // Slot 0 = heapUsed, the same snapshot the `MemoryMXBean` *interface*
    // `<init>` native writes to ITS slot 0 (see `register_memory_mxbean`).
    // Without this the bean handed back by `ManagementFactory.getMemoryMXBean`
    // carried an untyped default in slot 0 — a reader by index got `Int(0)`
    // where every other path in this module produces a `Long` of real bytes
    // (the classic write-by-name / read-by-slot mismatch: the only writes this
    // bean received were `init_notification_emitter_support`'s
    // `set_field_by_name` calls, which are silent no-ops on a synthetic stamp
    // because `ensure_synthetic_class` mints fields with NO names).
    //
    // Guarded on the class actually being a synthetic stamp: in real-JDK mode
    // `sun/management/MemoryImpl` is loaded from real bytes and slot 0 is a
    // genuine declared reference field (the NotificationBroadcasterSupport
    // state above), which a raw `Long` write would corrupt. There, nothing
    // reads a heap snapshot out of a slot anyway — `getMemoryUsage0` and the
    // real bytecode answer from the live accessors.
    if ctx.is_class_synthetic_stub("sun/management/MemoryImpl") {
        let heap_used = ctx.heap_allocated_bytes() as i64;
        ctx.set_field(obj, 0, Value::Long(heap_used));
    }
    Ok(obj)
}

/// The four `java.lang.management.MemoryUsage` slots, resolved by NAME.
///
/// **H6-B, 2026-08-20.** `javap -p java.lang.management.MemoryUsage` on JDK
/// 25.0.3+9, this host, instance fields in declaration order:
///
/// ```text
///   0  private final long init
///   1  private final long used
///   2  private final long committed
///   3  private final long max
/// ```
///
/// `MemoryUsage` is a REAL, concrete JDK class, so the hard-coded `0..=3` this
/// file used were right BY COINCIDENCE with nothing in the tree pinning the
/// coincidence -- the same species as `ObjectName`'s canonical-name slot
/// (H0-1 §3). The fallback below is the identity `[0, 1, 2, 3]` because that is
/// also the synthetic carrier's convention, so this conversion is
/// behaviour-neutral on BOTH carriers today and diverges only if the real
/// declaration order ever moves. That is the point of doing it.
///
/// One thing worth stating rather than assuming, because it sets how urgent
/// this site is next to its neighbours: **`MemoryUsage` has no reference
/// fields.** All four are `long`. A drifted index here is a wrong NUMBER, not
/// the bogus oop the same mistake produces on `java.util.ArrayList` or
/// `javax.management.ObjectName`. It is converted anyway -- the object-layout
/// audit's order is "convert, verify, unpad, then drop" and it has no "unless
/// the fields happen to be primitives" arm.
pub(in crate::jmx) fn memory_usage_slots(ctx: &dyn NativeContext, obj: ObjectRef) -> [usize; 4] {
    let cid = ctx.class_id_of_object(obj);
    let slot = |name: &str, fallback: usize| {
        ctx.resolve_field_index_by_class_id(cid, name)
            .unwrap_or(fallback)
    };
    [
        slot("init", 0),
        slot("used", 1),
        slot("committed", 2),
        slot("max", 3),
    ]
}

/// The non-heap `MemoryUsage.init` `MemoryImpl.getMemoryUsage0(false)`
/// reports: a constant, as HotSpot's aggregate non-heap `init` is (the sum of
/// the pools' initial sizes, fixed at startup). gci i15/nh: carried by the
/// `CodeHeap 'non-profiled nmethods'` pool's `init` (the aggregate is the
/// pools' sum), so the aggregate is unchanged.
pub(in crate::jmx) const NON_HEAP_INIT_BYTES: i64 = 2 * 1024 * 1024;

/// Per-class estimate used ONLY by a context that cannot measure class
/// metadata (`class_metadata_bytes() == None`: mocks, embedders with no class
/// store).
pub(in crate::jmx) const AVG_CLASS_METADATA_BYTES: i64 = 4096;

/// The non-heap `MemoryUsage.used` (gc-common w3-f, 2026-09-23): the VM's
/// measured class-metadata bytes when it has them, else the class-count
/// estimate — never below 1, because Spring Boot's
/// `ProcessInfoTests.memoryInfoIsAvailable` (and every real JVM) has a
/// positive non-heap footprint.
pub(in crate::jmx) fn non_heap_used_bytes(measured: Option<u64>, loaded_classes: usize) -> i64 {
    let used = match measured {
        Some(b) => i64::try_from(b).unwrap_or(i64::MAX),
        None => i64::try_from(loaded_classes)
            .unwrap_or(i64::MAX)
            .saturating_mul(AVG_CLASS_METADATA_BYTES),
    };
    used.max(1)
}

/// `[init, used, committed, max]` of the heap `MemoryUsage`, read LIVE — the
/// one derivation `MemoryImpl.getMemoryUsage0(true)` and the synthetic
/// `MemoryMXBean.getHeapMemoryUsage` share (gc-common w3-f: the synthetic
/// getter used to answer the values its `<init>` had snapshotted, frozen for
/// the life of the bean).
///
/// `init` is `-Xms`, `max` `Runtime.maxMemory()` (`max_heap_bytes`: `-Xmx`,
/// less the Generational copy reserve since gen r5w2/obs6, as HotSpot's
/// `max_capacity()`), `used` the heap's occupancy, and `committed` the same
/// source as `Runtime.totalMemory()` (`committed_heap_bytes`), clamped into
/// `[used, max]` so a racing allocation can never produce the
/// `used > committed` pair `MemoryUsage`'s own constructor rejects.
///
/// zr z3/t2 (rv3 finding 2, decided on the host by `Zr3T2HeapVsPools`): on
/// HotSpot 25 ZGC the heap's `used` and `committed` EQUAL the heap pools'
/// sums (`-Xmx256m`: Old `used=committed=4194304`, Young `used=2097152
/// committed=264241152`, heap `committed=268435456`) -- young committed is
/// the heap's commit less old's. This function keeps answering the heap's
/// own figures; it is the ZGC pool sampler (`JmxPoolSampler::sample`, under
/// `CRATONVM_ZGC_YOUNG_POOL_SINCE_CYCLE`) that sizes young committed so the
/// pools sum to them (`max(heap committed, used)`, the clamp below). An
/// earlier note here claimed HotSpot does not sum heap pools; the host row
/// refuted it.
pub(in crate::jmx) fn heap_memory_usage(ctx: &dyn NativeContext) -> [i64; 4] {
    let to_i64 = |b: usize| i64::try_from(b).unwrap_or(i64::MAX);
    let used = to_i64(ctx.heap_allocated_bytes());
    let max = ctx.max_heap_bytes();
    let committed = to_i64(ctx.committed_heap_bytes())
        .max(used)
        .min(max.max(used));
    [ctx.initial_heap_bytes(), used, committed, max]
}

/// `[init, used, committed, max]` of the non-heap `MemoryUsage` — see
/// [`non_heap_used_bytes`]. No reservation stands behind class metadata (it
/// is Rust heap owned by `ClassStore`), so `committed == used`, the heap
/// summary's convention for the same figure; `max` is -1 ("undefined"), as on
/// a HotSpot run without `-XX:MaxMetaspaceSize` (and kept with it: see the
/// note at `register_memory_mxbean`).
///
/// gci i15/nh: the SUM of the five non-heap pools, as HotSpot's
/// `jmm_GetMemoryUsage(false)` (`non_heap_usage_sum`): the class metadata
/// above PLUS the JIT's mapped code (`CodeHeap 'non-profiled nmethods'`).
/// `init` is still `NON_HEAP_INIT_BYTES` (the only pool with a defined
/// non-zero `init` carries it) and `max` still -1 (the empty code heaps'
/// `max` is undefined, so the sum is, with or without `-XX:MaxMetaspaceSize`).
pub(in crate::jmx) fn non_heap_memory_usage(ctx: &dyn NativeContext) -> [i64; 4] {
    non_heap_usage_sum(&non_heap_pool_infos(ctx))
}

pub(in crate::jmx) fn alloc_memory_usage(
    ctx: &mut dyn NativeContext,
    init: i64,
    used: i64,
    committed: i64,
    max: i64,
) -> Result<ObjectRef, MethodCallFailed> {
    let obj = try_alloc_concurrent_synthetic(ctx, "java/lang/management/MemoryUsage", 4)?;
    let [s_init, s_used, s_committed, s_max] = memory_usage_slots(ctx, obj);
    ctx.set_field(obj, s_init, Value::Long(init));
    ctx.set_field(obj, s_used, Value::Long(used));
    ctx.set_field(obj, s_committed, Value::Long(committed));
    ctx.set_field(obj, s_max, Value::Long(max));
    Ok(obj)
}

pub(in crate::jmx) fn register_memory_mxbean(r: &mut NativeMethodRegistry) {
    let cls = "java/lang/management/MemoryMXBean";
    // `MemoryMXBean` is an interface; the platform bean is allocated as a
    // concrete `sun/management/MemoryImpl` by `alloc_memory_mxbean`, so this
    // only runs for a synthetic receiver stamped with the interface name. Such
    // a receiver reaches the two usage getters below with all five slots at
    // their untyped default, and each getter then falls back to a hard-coded
    // 16MB/256MB/64MB guess. Seed the slots from the same live accessors
    // `MemoryImpl.getMemoryUsage0` uses so the fallbacks stay unreached.
    r.register(cls, "<init>", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let heap_used = ctx.heap_allocated_bytes() as i64;
        // Same derivation as `MemoryImpl.getMemoryUsage0`. The two getters
        // below no longer read these slots (they answer live since gc-common
        // w3-f); the seeding is kept for any bytecode that reads the fields.
        let non_heap_used = non_heap_memory_usage(ctx)[1];
        ctx.set_field(this, 0, Value::Long(heap_used));
        ctx.set_field(this, 1, Value::Long(ctx.max_heap_bytes()));
        ctx.set_field(
            this,
            2,
            Value::Long(heap_used.max(ctx.initial_heap_bytes())),
        );
        ctx.set_field(this, 3, Value::Long(non_heap_used));
        ctx.set_field(this, 4, Value::Long(-1));
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);

    r.register(
        cls,
        "getHeapMemoryUsage",
        "()Ljava/lang/management/MemoryUsage;",
        |ctx, _args| {
            // LIVE, like `MemoryImpl.getMemoryUsage0(true)` (gc-common w3-f,
            // 2026-09-23). This answered the three values `<init>` had seeded
            // into the receiver's slots -- the heap as it was when the bean was
            // built, for the life of the bean -- with `init` 0.
            let [init, used, committed, max] = heap_memory_usage(ctx);
            let mu = alloc_memory_usage(ctx, init, used, committed, max)?;
            Ok(Some(Value::Object(Some(mu))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    r.register(
        cls,
        "getNonHeapMemoryUsage",
        "()Ljava/lang/management/MemoryUsage;",
        |ctx, _args| {
            // LIVE, like `MemoryImpl.getMemoryUsage0(false)` (gc-common w3-f,
            // 2026-09-23). This answered `<init>`'s snapshot with slot 4 (the
            // `-1` max sentinel) as `committed` too, i.e. `used > 0` with
            // `committed = -1` -- a pair `MemoryUsage`'s constructor rejects.
            let [init, used, committed, max] = non_heap_memory_usage(ctx);
            let mu = alloc_memory_usage(ctx, init, used, committed, max)?;
            Ok(Some(Value::Object(Some(mu))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // REAL: the GC's own finalization backlog, through the `NativeContext`
    // accessor that now exposes `ReferenceProcessor::pending_finalization_count()`
    // (`gc/src/reference.rs`).
    //
    // CratonVM does finalize — `SharedVm::register_finalizable` discovers
    // finalizable objects into `ref_processor.finalization_queue` and
    // `drain_finalizers` hands them to a real `FinalizerThread`
    // (`vm/src/vm/vm_init.rs`) — so the previous flat 0 reported "no
    // finalization backlog" even while the queue was growing, which is exactly
    // the condition an operator queries this bean to detect. The accessor is
    // `try_lock`-based and answers 0 rather than blocking behind a GC pass, so
    // a monitoring read can never stall its caller. The sibling registration in
    // `phases_late::management` reads the same accessor.
    r.register(
        cls,
        "getObjectPendingFinalizationCount",
        "()I",
        |ctx, _args| Ok(Some(Value::Int(ctx.pending_finalization_count()))),
    cratonvm_native_api::NativeKind::SyntheticStub);

    r.register(cls, "gc", "()V", |ctx, _args| {
        if let Some(h) = cratonvm_native_builtins::hooks::JFR_HOOKS.get() { (h.force_gc_as_system_gc)(ctx); };
        Ok(None)
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    // isVerbose() -- not one of the 6 synthetic fields, so it answers from the
    // per-VM `-verbose:gc` flag rather than per-instance state. This
    // registration WINS over the sibling `MemoryMXBean.isVerbose` in
    // `phases_late::management` (jmx.rs registers later — see
    // `register_synthetic_overrides`), so while it returned a fixed 0 the
    // `setVerbose` half of that pair had nothing reading its writes and the
    // JMM's setVerbose/isVerbose round-trip was dead in both run modes.
    r.register(cls, "isVerbose", "()Z", |ctx, _args| {
        Ok(Some(Value::Int(i32::from(verbose_gc_get(&*ctx)))))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
}

// ---------------------------------------------------------------------------
// 4. MemoryUsage — 4-field synthetic
// ---------------------------------------------------------------------------

pub(in crate::jmx) fn register_memory_usage(r: &mut NativeMethodRegistry) {
    let cls = "java/lang/management/MemoryUsage";
    // The real `MemoryUsage` is a concrete final class with no no-arg
    // constructor, so this descriptor only exists for CratonVM synthetic
    // receivers (the 4-arg form below is the one real bytecode calls, and it
    // already round-trips). A synthetic receiver's four slots hold untyped
    // defaults, which the `()J` getters would hand straight back; give it the
    // same UNDEFINED shape `MemoryPoolImpl` uses for "no measurement taken".
    r.register(cls, "<init>", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        for slot in memory_usage_slots(ctx, this) {
            ctx.set_field(this, slot, Value::Long(-1));        }
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);
    // gcd d1/e (2026-09-27, gen r5w4/obs8): only the stand-in for a class
    // with NO bytes. With the JDK's `MemoryUsage` loaded, its own constructor
    // runs, and with it the argument checks this body skipped: HotSpot throws
    // `IllegalArgumentException` for `init < -1`, `used < 0`,
    // `committed < 0`, `max < -1`, `used > committed` and (for a defined
    // `max`) `committed > max`. The VM's own usages never come through here
    // (`alloc_memory_usage` writes the slots directly), so this changes only
    // what a Java `new MemoryUsage(init, used, committed, max)` sees. (The
    // page's `MemoryUsage.from(CompositeData)` goes through the private
    // `MemoryUsage(CompositeData)` constructor, which has no bridge.)
    r.register(cls, "<init>", "(JJJJ)V", |ctx, args| {
        if !ctx.is_class_synthetic_stub("java/lang/management/MemoryUsage") {
            return ctx.invoke_special_bytecode_only(
                "java/lang/management/MemoryUsage",
                "<init>",
                "(JJJJ)V",
                args,
            );
        }
        let this = obj_arg(args, 0)?;
        // args: this, init, used, committed, max (longs take 2 slots each in JVM
        // but our Value model uses one slot per Long)
        let init_val = match args.get(1) {
            Some(Value::Long(v)) => *v,
            _ => 0,
        };
        let used_val = match args.get(2) {
            Some(Value::Long(v)) => *v,
            _ => 0,
        };
        let committed_val = match args.get(3) {
            Some(Value::Long(v)) => *v,
            _ => 0,
        };
        let max_val = match args.get(4) {
            Some(Value::Long(v)) => *v,
            _ => 0,
        };
        let [s_init, s_used, s_committed, s_max] = memory_usage_slots(ctx, this);
        ctx.set_field(this, s_init, Value::Long(init_val));
        ctx.set_field(this, s_used, Value::Long(used_val));
        ctx.set_field(this, s_committed, Value::Long(committed_val));
        ctx.set_field(this, s_max, Value::Long(max_val));
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);

    r.register(cls, "getInit", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, memory_usage_slots(ctx, this)[0])))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(cls, "getUsed", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, memory_usage_slots(ctx, this)[1])))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(cls, "getCommitted", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, memory_usage_slots(ctx, this)[2])))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(cls, "getMax", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, memory_usage_slots(ctx, this)[3])))
    }, cratonvm_native_api::NativeKind::Bridge);
    // `toString` is NOT overridden on a real JDK: the real bytecode reads the
    // same four slots this file writes by index (`init`, `used`, `committed`,
    // `max`, declared in that order) and renders HotSpot's exact
    // `init = N(NK) used = N(NK) committed = N(NK) max = N(NK)` — including the
    // `>> 10` kibibyte column and its `-1(-1K)` for an undefined value. The
    // shim rendered `init=N, used=N, committed=N, max=N`, a format that exists
    // on no real JVM, so anything logging or scraping a MemoryUsage read
    // differently here. See `memoryusage_tostring_shim_enabled`.
    if memoryusage_tostring_shim_enabled() {
        r.register(cls, "toString", "()Ljava/lang/String;", |ctx, args| {
            let this = obj_arg(args, 0)?;
            let [s_init, s_used, s_committed, s_max] = memory_usage_slots(ctx, this);
            let init_v = match ctx.get_field(this, s_init) {
                Value::Long(v) => v,
                _ => 0,
            };
            let used_v = match ctx.get_field(this, s_used) {
                Value::Long(v) => v,
                _ => 0,
            };
            let committed_v = match ctx.get_field(this, s_committed) {
                Value::Long(v) => v,
                _ => 0,
            };
            let max_v = match ctx.get_field(this, s_max) {
                Value::Long(v) => v,
                _ => 0,
            };
            // Kept byte-for-byte identical to the real `MemoryUsage.toString()`
            // so a synthetic-jdk run is not a second, different divergence.
            let text = format!(
                "init = {}({}K) used = {}({}K) committed = {}({}K) max = {}({}K)",
                init_v,
                init_v >> 10,
                used_v,
                used_v >> 10,
                committed_v,
                committed_v >> 10,
                max_v,
                max_v >> 10
            );
            let s = ctx.create_string(&text);
            Ok(Some(Value::Object(Some(s))))
        }, cratonvm_native_api::NativeKind::Bridge);
    }
    }

// ---------------------------------------------------------------------------
// 5. ThreadMXBean — 6-field synthetic
// ---------------------------------------------------------------------------

/// Should `java.lang.management.MemoryUsage.toString()` be answered by the
/// shim rather than the class's own bytecode?
///
/// **Default: no.** `MemoryUsage` is a real, self-contained JDK class whose
/// `toString` needs nothing this VM cannot already provide — the four fields it
/// reads (`init`, `used`, `committed`, `max`) are declared in exactly the order
/// this file writes them by index, so the real bytecode sees the values the
/// sibling `getInit`/`getUsed`/`getCommitted`/`getMax` natives hand back.
///
/// `synthetic-jdk` builds have no such bytecode and keep the shim;
/// `CRATONVM_SYNTHETIC_MEMORYUSAGE_TOSTRING=1` restores it on a real-JDK run.
pub(in crate::jmx) fn memoryusage_tostring_shim_enabled() -> bool {
    if cfg!(feature = "synthetic-jdk") {
        return true;
    }
    // The latched `VmFlags` snapshot, not a live `getenv`. An undeclared flag
    // read straight from `std::env` is unreachable from
    // `CRATONVM_REAL=-memoryusage-tostring` and invisible to
    // `flags::with_thread_overrides`, so a test that arranges it through the
    // supported hook silently measures the developer's ambient environment
    // instead. `one_true_yes_exact` also accepts `yes`, which the previous
    // `Ok("1") | Ok("true")` did not — a strict widening of an opt-out escape
    // hatch. Same treatment as `jmx_openmbean`'s sibling gate.
    cratonvm_native_builtins::nbflags().synthetic_memoryusage_tostring
}

pub(in crate::jmx) fn jmx_class_id_or_object(ctx: &mut dyn NativeContext, class_name: &str) -> ClassId {
    ctx.ensure_class_initialized(class_name)
        .unwrap_or(ClassId::new(0))
}

