#[allow(unused_imports)]
use super::*;

/// BUG FIX (2026-07-10, ES executors-factory mainlock NPE, layer 2): drive a
/// genuinely real `ThreadPoolExecutor` construction for
/// `Executors.newFixedThreadPool`/`newCachedThreadPool`/`newSingleThreadExecutor`,
/// the same pattern `initialize_real_scheduled_thread_pool_executor` already
/// uses for `ScheduledThreadPoolExecutor`. `this` must already be allocated
/// with the real class's field layout (via `alloc_concurrent_synthetic`);
/// this invokes the real `ThreadPoolExecutor(int, int, long, TimeUnit,
/// BlockingQueue[, ThreadFactory])V` constructor via `invoke_special`, which
/// itself fills in `ctl`/`workQueue`/`mainLock`/`workers`/`termination` (and,
/// when no `ThreadFactory` is supplied, calls the real JDK
/// `Executors.defaultThreadFactory()`/`defaultHandler` internally — no
/// different from what happens when user bytecode calls `new
/// ThreadPoolExecutor(...)` directly). This is what makes
/// `drop_real_layout_synthetic`'s "every ThreadPoolExecutor-tagged object is
/// real" assumption actually hold for these factory shortcuts, instead of
/// just for direct `new ThreadPoolExecutor(...)` calls.
/// Falls back to the old two-slot legacy write if queue/unit construction
/// fails, so callers never see a fully-uninitialized object.
/// See ES-FAIL-20260710-executors-factory-synthetic-mainlock-npe-FIXED.md.
pub fn initialize_real_thread_pool_executor(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    core_pool_size: i32,
    max_pool_size: i32,
    keep_alive: i64,
    keep_alive_unit_name: &str,
    queue_kind: TpeQueueKind,
    thread_factory: Option<ObjectRef>,
) -> MethodCallResult {
    let core_pool_size = core_pool_size.max(0);
    let max_pool_size = max_pool_size.max(core_pool_size).max(1);
    let pin_base = ctx.pin_native_root(this);
    let factory_pin = thread_factory.map(|factory| ctx.pin_native_root(factory));

    let Some(queue) = tpe_new_queue(ctx, queue_kind) else {
        let this = ctx.read_native_pin(pin_base, this);
        stpe_legacy_slot_init(ctx, this, core_pool_size.max(1));
        ctx.unpin_native_roots(pin_base);
        // Return the (possibly GC-relocated) object so factory-style callers
        // (see below) don't hand back a stale pre-relocation address.
        return Ok(Some(Value::Object(Some(this))));
    };
    let queue_pin = ctx.pin_native_root(queue);

    let Some(unit) = tpe_time_unit_by_name(ctx, keep_alive_unit_name) else {
        let this = ctx.read_native_pin(pin_base, this);
        let queue = ctx.read_native_pin(queue_pin, queue);
        stpe_legacy_slot_init(ctx, this, core_pool_size.max(1));
        ctx.set_field_by_name(this, "workQueue", Value::Object(Some(queue)));
        ctx.unpin_native_roots(pin_base);
        return Ok(Some(Value::Object(Some(this))));
    };
    let unit_pin = ctx.pin_native_root(unit);

    let this_arg = ctx.read_native_pin(pin_base, this);
    let queue_arg = ctx.read_native_pin(queue_pin, queue);
    let unit_arg = ctx.read_native_pin(unit_pin, unit);

    let result = if let Some(factory) = thread_factory {
        let factory_arg = ctx.read_native_pin(factory_pin.unwrap_or(pin_base), factory);
        ctx.invoke_special(
            "java/util/concurrent/ThreadPoolExecutor",
            "<init>",
            "(IIJLjava/util/concurrent/TimeUnit;Ljava/util/concurrent/BlockingQueue;Ljava/util/concurrent/ThreadFactory;)V",
            &[
                Value::Object(Some(this_arg)),
                Value::Int(core_pool_size),
                Value::Int(max_pool_size),
                Value::Long(keep_alive),
                Value::Object(Some(unit_arg)),
                Value::Object(Some(queue_arg)),
                Value::Object(Some(factory_arg)),
            ],
        )
    } else {
        ctx.invoke_special(
            "java/util/concurrent/ThreadPoolExecutor",
            "<init>",
            "(IIJLjava/util/concurrent/TimeUnit;Ljava/util/concurrent/BlockingQueue;)V",
            &[
                Value::Object(Some(this_arg)),
                Value::Int(core_pool_size),
                Value::Int(max_pool_size),
                Value::Long(keep_alive),
                Value::Object(Some(unit_arg)),
                Value::Object(Some(queue_arg)),
            ],
        )
    };

    // BUG FIX (GC-relocation-during-construction, 2026-07-16): `this` may have
    // been relocated by a moving-GC collection triggered by one of the
    // allocations above (queue/unit/ThreadFactory construction, or the real
    // <init> itself allocating mainLock/workers/a Worker+Thread). This
    // function's OWN uses of `this` are safe (always re-fetched via
    // `read_native_pin`), but factory-style callers below (newFixedThreadPool
    // etc.) hold their OWN pre-construction copy of the object with no Java
    // frame slot for the GC to fix up (the object exists only as a native Rust
    // value until the interpreter stores the returned Value into a local) --
    // if this function discarded the relocated address, those callers
    // returned a stale, already-freed from-space pointer that the interpreter
    // would read as an all-zero header on first use (AbstractMethodError /
    // stale-pointer livelock under java/util/concurrent/ExecutorService
    // dispatch; see
    // hib-misc-residuals-20260716-FIXED.md,
    // ZonedDateTimeTest/LocalDateTimeTest). Always hand back the CURRENT
    // address so every caller (both the <init> dispatch, which ignores this
    // for void methods, and the factory shims, which do not) sees the live
    // object.
    let this = ctx.read_native_pin(pin_base, this);
    ctx.unpin_native_roots(pin_base);
    result.map(|_| Some(Value::Object(Some(this))))
}

pub fn initialize_real_scheduled_thread_pool_executor(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    cores: i32,
    thread_factory: Option<ObjectRef>,
) -> MethodCallResult {
    let cores = cores.max(1);
    let pin_base = ctx.pin_native_root(this);
    let factory_pin = thread_factory.map(|factory| ctx.pin_native_root(factory));

    let Some(queue) = stpe_new_delayed_work_queue(ctx) else {
        let this = ctx.read_native_pin(pin_base, this);
        stpe_legacy_slot_init(ctx, this, cores);
        ctx.unpin_native_roots(pin_base);
        return Ok(None);
    };
    let queue_pin = ctx.pin_native_root(queue);

    let Some(unit) = stpe_time_unit_millis(ctx) else {
        let this = ctx.read_native_pin(pin_base, this);
        let queue = ctx.read_native_pin(queue_pin, queue);
        stpe_legacy_slot_init(ctx, this, cores);
        ctx.set_field_by_name(this, "workQueue", Value::Object(Some(queue)));
        ctx.unpin_native_roots(pin_base);
        return Ok(None);
    };
    let unit_pin = ctx.pin_native_root(unit);

    let this_arg = ctx.read_native_pin(pin_base, this);
    let queue_arg = ctx.read_native_pin(queue_pin, queue);
    let unit_arg = ctx.read_native_pin(unit_pin, unit);
    let result = if let Some(factory) = thread_factory {
        let factory_arg = ctx.read_native_pin(factory_pin.unwrap_or(pin_base), factory);
        ctx.invoke_special(
            "java/util/concurrent/ThreadPoolExecutor",
            "<init>",
            "(IIJLjava/util/concurrent/TimeUnit;Ljava/util/concurrent/BlockingQueue;Ljava/util/concurrent/ThreadFactory;)V",
            &[
                Value::Object(Some(this_arg)),
                Value::Int(cores),
                Value::Int(i32::MAX),
                Value::Long(10),
                Value::Object(Some(unit_arg)),
                Value::Object(Some(queue_arg)),
                Value::Object(Some(factory_arg)),
            ],
        )
    } else {
        ctx.invoke_special(
            "java/util/concurrent/ThreadPoolExecutor",
            "<init>",
            "(IIJLjava/util/concurrent/TimeUnit;Ljava/util/concurrent/BlockingQueue;)V",
            &[
                Value::Object(Some(this_arg)),
                Value::Int(cores),
                Value::Int(i32::MAX),
                Value::Long(10),
                Value::Object(Some(unit_arg)),
                Value::Object(Some(queue_arg)),
            ],
        )
    };

    let this = ctx.read_native_pin(pin_base, this);
    if result.is_ok() {
        // The real parent constructor is responsible for this assignment, but
        // the interpreter's constructor fast path can leave the inherited
        // field at its default value when the receiver is a user subclass.
        // Preserve the argument supplied to the real STPE constructor so a
        // ThreadPoolTaskScheduler subclass has the requested pool size.
        ctx.set_field_by_name(this, "corePoolSize", Value::Int(cores));
        ctx.set_field_by_name(
            this,
            "continueExistingPeriodicTasksAfterShutdown",
            Value::Int(0),
        );
        ctx.set_field_by_name(
            this,
            "executeExistingDelayedTasksAfterShutdown",
            Value::Int(1),
        );
        ctx.set_field_by_name(this, "removeOnCancel", Value::Int(0));
    }
    ctx.unpin_native_roots(pin_base);
    result.map(|_| None)
}

/// Real `java.util.concurrent.ScheduledThreadPoolExecutor` field names behind
/// the three shutdown/cancel policy accessor pairs (W2).
pub const STPE_POLICY_CONTINUE_PERIODIC: &str = "continueExistingPeriodicTasksAfterShutdown";
pub const STPE_POLICY_EXECUTE_DELAYED: &str = "executeExistingDelayedTasksAfterShutdown";
pub const STPE_POLICY_REMOVE_ON_CANCEL: &str = "removeOnCancel";

pub fn stpe_set_policy(ctx: &mut dyn NativeContext, args: &[Value], field: &str) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let v = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
    ctx.set_field_by_name(this, field, Value::Int(i32::from(v != 0)));
    Ok(None)
}

pub fn stpe_get_policy(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    field: &str,
    default: i32,
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // `get_field_by_name` answers `Value::Object(None)` for an unknown field,
    // which is the synthetic carrier — fall back to the JDK default there.
    let v = match ctx.get_field_by_name(this, field) {
        Value::Int(v) => i32::from(v != 0),
        _ => default,
    };
    Ok(Some(Value::Int(v)))
}

pub fn register_scheduled_executor_natives(r: &mut NativeMethodRegistry) {
            let ses = "java/util/concurrent/ScheduledThreadPoolExecutor";
    r.register(ses, "<init>", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let cores = match args.get(1) {
            Some(Value::Int(v)) => *v,
            _ => 1,
        };
        initialize_real_scheduled_thread_pool_executor(ctx, this, cores, None)    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(
        ses,
        "<init>",
        "(ILjava/util/concurrent/ThreadFactory;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let cores = match args.get(1) {
                Some(Value::Int(v)) => *v,
                _ => 1,
            };
            let factory = match args.get(2) {
                Some(Value::Object(Some(factory))) => Some(*factory),
                _ => None,
            };
            initialize_real_scheduled_thread_pool_executor(ctx, this, cores, factory)
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    // The real-JDK registration pass drops the synthetic STPE surface because
    // its historical two-slot layout corrupts real executors. Keep only this
    // constructor as a Bridge: it initializes the real ThreadPoolExecutor
    // state and covers Spring's anonymous ThreadPoolTaskScheduler subclass.
            r.register(
        ses,
        "<init>",
        "(ILjava/util/concurrent/ThreadFactory;Ljava/util/concurrent/RejectedExecutionHandler;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let cores = match args.get(1) {
                Some(Value::Int(v)) => *v,
                _ => 1,
            };
            let factory = match args.get(2) {
                Some(Value::Object(Some(factory))) => Some(*factory),
                _ => None,
            };
            // The shared initializer delegates to ThreadPoolExecutor's real
            // constructor, which supplies the JDK default abort policy. The
            // caller-provided rejection handler is only observed by the
            // scheduled executor's own bytecode; scheduler subclasses such as
            // Spring's anonymous executor need their core-pool size preserved
            // here instead of falling through an incomplete constructor path.
            initialize_real_scheduled_thread_pool_executor(ctx, this, cores, factory)
        }, cratonvm_native_api::NativeKind::Bridge,
    );
        r.register(ses, "shutdown", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if ctx.object_num_fields(this) > 1 {
            ctx.set_field(this, 1, Value::Int(1));        }
        Ok(Some(Value::Object(None)))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(ses, "shutdownNow", "()Ljava/util/List;", |ctx, _args| {
        let list = try_alloc_concurrent_synthetic(ctx, "java/util/ArrayList", 2)?;
        // gc-common w20-b: the list was held raw across its array.
        let list: ObjectRef = alloc_array_into_field(
            ctx,
            list,
            0,
            cratonvm_types::ArrayElementType::Reference,
            10,
        );
        ctx.set_field(list, 1, Value::Int(0));
        Ok(Some(Value::Object(Some(list))))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    // W2: the three ScheduledThreadPoolExecutor policy setters were no-ops and
    // the three getters returned constants. The constants happened to be the
    // JDK defaults (false / true / false), so the pair looked right until a
    // caller actually configured the executor: `setRemoveOnCancelPolicy(true)`
    // followed by `getRemoveOnCancelPolicy()` still answered false, and a
    // caller that verifies its own configuration (Spring's
    // `ThreadPoolTaskScheduler#setRemoveOnCancelPolicy`, Micrometer's executor
    // metrics binder) saw its setting silently dropped.
    //
    // `register_scheduled_executor_natives` runs from
    // `register_essential_natives_with_shims`, i.e. in REAL-JDK mode too, so
    // these natives shadow the real STPE bytecode. Route both directions
    // through the real fields by name; the getters keep the JDK default when
    // the field is absent, which is the synthetic-mode carrier (no real STPE
    // class, so `set_field_by_name` is a documented no-op there and the
    // synthetic executor stays at its defaults, exactly as before).
    r.register(
        ses,
        "setContinueExistingPeriodicTasksAfterShutdownPolicy",
        "(Z)V",
        |ctx, args| stpe_set_policy(ctx, args, STPE_POLICY_CONTINUE_PERIODIC), cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        ses,
        "setExecuteExistingDelayedTasksAfterShutdownPolicy",
        "(Z)V",
        |ctx, args| stpe_set_policy(ctx, args, STPE_POLICY_EXECUTE_DELAYED), cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(ses, "setRemoveOnCancelPolicy", "(Z)V", |ctx, args| {
        stpe_set_policy(ctx, args, STPE_POLICY_REMOVE_ON_CANCEL)
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(
        ses,
        "getContinueExistingPeriodicTasksAfterShutdownPolicy",
        "()Z",
        |ctx, args| stpe_get_policy(ctx, args, STPE_POLICY_CONTINUE_PERIODIC, 0), cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        ses,
        "getExecuteExistingDelayedTasksAfterShutdownPolicy",
        "()Z",
        |ctx, args| stpe_get_policy(ctx, args, STPE_POLICY_EXECUTE_DELAYED, 1), cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(ses, "getRemoveOnCancelPolicy", "()Z", |ctx, args| {
        stpe_get_policy(ctx, args, STPE_POLICY_REMOVE_ON_CANCEL, 0)
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(ses, "execute", "(Ljava/lang/Runnable;)V", |ctx, args| {
        if let Some(Value::Object(Some(task))) = args.get(1).copied() {
            ctx.invoke_virtual(task, "run", "()V", &[])?;        }
        Ok(None)
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(ses, "isShutdown", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let shut = if ctx.object_num_fields(this) > 1 {
            match ctx.get_field(this, 1) {
                Value::Int(v) => v,
                _ => 0,
            }
        } else {
            0
        };
        Ok(Some(Value::Int(shut)))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(ses, "isTerminated", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let shut = if ctx.object_num_fields(this) > 1 {
            match ctx.get_field(this, 1) {
                Value::Int(v) => v,
                _ => 0,
            }
        } else {
            0
        };
        Ok(Some(Value::Int(shut)))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    // WP4.5 — drive the scheduled-pump for the requested timeout window
    // so any periodic tasks queued by `scheduleAtFixedRate` get a chance
    // to fire before the caller observes shutdown.
    r.register(
        ses,
        "awaitTermination",
        "(JLjava/util/concurrent/TimeUnit;)Z",
        |ctx, args| {
            // WP4.5 — accept Double-bit-encoded longs from the
            // type-erased operand-stack pop (see `native_thread_sleep`).
            let count = match args.get(1) {
                Some(Value::Long(v)) => *v,
                Some(Value::Double(d)) => d.to_bits() as i64,
                Some(Value::Int(v)) => *v as i64,
                _ => 0,
            };
            // Translate the (count, TimeUnit) pair to a millisecond budget.
            // Round 13 wave 9 (lane bigdec2): `ordinal` by name (a real
            // `TimeUnit`'s slot 0 is `Enum.name`); slot 0 only with
            // `CRATONVM_ENUM_ORDINAL_BY_NAME=0`.
            let ordinal = match args.get(2) {
                Some(Value::Object(Some(unit)))
                    if true =>
                {
                    crate::time_unit_ordinal(ctx, *unit)
                }
                Some(Value::Object(Some(unit))) => ctx.get_field(*unit, 0).as_int().unwrap_or(2),
                _ => 2,
            };
            let budget_ms: i64 = match ordinal {
                0 => count / 1_000_000,
                1 => count / 1_000,
                2 => count,
                3 => count.saturating_mul(1_000),
                4 => count.saturating_mul(60_000),
                5 => count.saturating_mul(3_600_000),
                6 => count.saturating_mul(86_400_000),
                _ => count,
            };
            let budget_ms = budget_ms.max(0) as u64;
            // Pump in 25ms slices so periodic tasks fire on schedule
            // even within a long await. 25ms is short enough that a
            // 50ms-period task fires roughly twice per period during
            // an awaitTermination of 250ms (matching the SchedProbe
            // 4–8 ticks acceptance window).
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(budget_ms);
            loop {
                crate::scheduled_pump::registry().pump(ctx);
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                if remaining.is_zero() {
                    break;
                }
                let chunk = remaining.min(std::time::Duration::from_millis(25));
                // Interpreter round i3 wave 79, lane L6 (page
                // `i66-L1-timed-stand-in-waits-read-waiting`): the JDK body
                // is `termination.awaitNanos(..)`, a timed park (HotSpot:
                // TIMED_WAITING, JVMTI PARKED); the plain door read WAITING.
                ctx.begin_blocking_region_as(cratonvm_native_api::BlockKind::TimedParked);
                std::thread::sleep(chunk);
                ctx.end_blocking_region();
            }
            Ok(Some(Value::Int(1)))
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    // Do not register no-op schedule* here — `register_p63_scheduled_executor`
    // (register_essential_natives, phase 63) provides delay-aware scheduling.
    // These stubs used to overwrite p63 and return null / block real Surefire
    // fork shutdown sequencing.
            r.register(ses, "getCorePoolSize", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let core_pool_size = match ctx.get_field_by_name(this, "corePoolSize") {
            Value::Int(value) => Value::Int(value),
            // Synthetic STPE objects have only the historical slot layout.
            _ if ctx.object_num_fields(this) > 0 => ctx.get_field(this, 0),
            _ => Value::Int(1),
        };
        Ok(Some(core_pool_size))
    }, cratonvm_native_api::NativeKind::Bridge);
        // W2: `getPoolSize` answered a constant 0, so a caller checking "does this
    // scheduler have any threads?" concluded it had none even right after
    // `new ScheduledThreadPoolExecutor(8)`. Serve the real `poolSize` field
    // where it exists and otherwise the configured core size, mirroring the
    // `getCorePoolSize` bridge immediately above.
    r.register(ses, "getPoolSize", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let n = match ctx.get_field_by_name(this, "poolSize") {
            Value::Int(v) => Value::Int(v),
            // Synthetic STPE objects have only the historical slot layout,
            // whose slot 0 is the core pool size.
            _ if ctx.object_num_fields(this) > 0 => ctx.get_field(this, 0),
            _ => Value::Int(1),
        };
        Ok(Some(n))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    // KEEP (all three): these are honest for the carrier that can actually
    // reach them. Re-verified for wave 4: `ScheduledThreadPoolExecutor` does
    // not override `getActiveCount`/`getTaskCount`/`getCompletedTaskCount`, so
    // a REAL receiver resolves them on `ThreadPoolExecutor` and the lookup key
    // is that class — which native-collections registers (`let tp = "java/util
    // /concurrent/ThreadPoolExecutor"`), routing real pools into the genuine
    // JDK bytecode via `invoke_special_bytecode_only`. The only receiver that
    // reaches THIS `ses`-keyed entry is the synthetic 2-slot carrier, whose
    // `execute` (above) runs each Runnable inline and returns, so it has zero
    // active tasks at every observable moment.
    //
    // Residual, not a stub: `getTaskCount`/`getCompletedTaskCount` are
    // cumulative, so 0 under-reports work the synthetic carrier has already
    // run. Counting it needs a GC-stable per-executor key (the carrier has no
    // spare slot and this table would have to be a GC-scanned side table like
    // `fjp_state`); a process-global counter would be worse than 0, since it
    // would attribute every executor's work to each of them.
    r.register(ses, "getActiveCount", "()I", |_ctx, _args| {
        Ok(Some(Value::Int(0)))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(ses, "getTaskCount", "()J", |_ctx, _args| {
        Ok(Some(Value::Long(0)))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(ses, "getCompletedTaskCount", "()J", |_ctx, _args| {
        Ok(Some(Value::Long(0)))
    }, cratonvm_native_api::NativeKind::Intrinsic);

    let svc = "java/util/concurrent/ScheduledExecutorService";
    r.register(svc, "shutdown", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, 1, Value::Int(1));
        Ok(Some(Value::Object(None)))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(svc, "isShutdown", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let shut = match ctx.get_field(this, 1) {
            Value::Int(v) => v,
            _ => 0,
        };
        Ok(Some(Value::Int(shut)))    }, cratonvm_native_api::NativeKind::Intrinsic);

    let ex = "java/util/concurrent/Executors";
    r.register(
        ex,
        "newScheduledThreadPool",
        "(I)Ljava/util/concurrent/ScheduledExecutorService;",
        |ctx, args| {
            let ps = match args.first() {
                Some(Value::Int(v)) => *v,
                _ => 1,
            };
            let sv = try_alloc_concurrent_synthetic(
                ctx,
                "java/util/concurrent/ScheduledThreadPoolExecutor",
                2,
            )?;
            ctx.set_field(sv, 0, Value::Int(ps));
            ctx.set_field(sv, 1, Value::Int(0));
            Ok(Some(Value::Object(Some(sv))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        ex,
        "newScheduledThreadPool",
        "(ILjava/util/concurrent/ThreadFactory;)Ljava/util/concurrent/ScheduledExecutorService;",
        |ctx, args| {
            let ps = match args.first() {
                Some(Value::Int(v)) => *v,
                _ => 1,
            };
            let sv = try_alloc_concurrent_synthetic(
                ctx,
                "java/util/concurrent/ScheduledThreadPoolExecutor",
                2,
            )?;
            ctx.set_field(sv, 0, Value::Int(ps));
            ctx.set_field(sv, 1, Value::Int(0));
            Ok(Some(Value::Object(Some(sv))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        ex,
        "newSingleThreadScheduledExecutor",
        "()Ljava/util/concurrent/ScheduledExecutorService;",
        |ctx, _args| {
            let sv = try_alloc_concurrent_synthetic(
                ctx,
                "java/util/concurrent/ScheduledThreadPoolExecutor",
                2,
            )?;
            ctx.set_field(sv, 0, Value::Int(1));
            ctx.set_field(sv, 1, Value::Int(0));
            Ok(Some(Value::Object(Some(sv))))
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    // BUG FIX (2026-07-10, es-storedscripts-retire): these Executors factory
    // registrations (newFixedThreadPool, newCachedThreadPool x2,
    // newSingleThreadExecutor below) were copy-pasted from the
    // newScheduledThreadPool/newSingleThreadScheduledExecutor blocks above
    // and never had their allocated class corrected: each one synthesized an
    // object tagged java/util/concurrent/ScheduledThreadPoolExecutor instead
    // of plain java/util/concurrent/ThreadPoolExecutor. Real JDK
    // newFixedThreadPool()/newCachedThreadPool()/newSingleThreadExecutor()
    // never return an STPE. Because register_executors_scheduled_natives
    // (native_stpe_*) is gated off in real-JDK mode (see
    // 11-stpe-mainlock-npe-teardown-regression.md),
    // no native shadowed these mistagged objects' methods, so real inherited
    // ScheduledThreadPoolExecutor/ThreadPoolExecutor bytecode ran against a
    // 2-field synthetic object whose ctl/workQueue/mainLock/workers fields
    // were never initialised -- NPE ("Cannot invoke ReentrantLock.lock()
    // because mainLock is null") on shutdown(), or on ctl.get() via
    // submit()/schedule() (STPE.submit delegates through schedule()).
    // Tagging the object ThreadPoolExecutor (matching the sibling
    // registration in native-builtins/src/lib.rs::native_new_fixed_pool)
    // routes shutdown()/shutdownNow()/isShutdown() through the
    // already-synthetic-aware natives registered on ThreadPoolExecutor in
    // lib.rs (executor_has_real_workers() correctly identifies this as
    // synthetic and takes the safe branch instead of touching mainLock).
    //
    // LAYER 2 FIX (2026-07-10): the above was necessary but insufficient —
    // native-api/src/registry.rs's `drop_real_layout_synthetic` gate drops
    // *every* ThreadPoolExecutor-tagged native at registration time
    // (including the synthetic-aware submit/execute/shutdown/etc. above),
    // on the assumption that any ThreadPoolExecutor-tagged object is
    // genuinely real. That assumption was false for these factory-made
    // objects (only 2 legacy slots ever got written; ctl/workQueue/mainLock
    // stayed null), so real inherited bytecode NPE'd. Fixed by actually
    // driving the real `ThreadPoolExecutor(...)` constructor below via
    // `initialize_real_thread_pool_executor` (same pattern already used for
    // ScheduledThreadPoolExecutor construction) — these objects are now
    // genuinely real, so the drop_real_layout_synthetic assumption holds and
    // real submit/execute/shutdown/shutdownNow bytecode runs end-to-end.
    // See ES-FAIL-20260710-executors-factory-synthetic-mainlock-npe-FIXED.md.
    r.register(
        ex,
        "newFixedThreadPool",
        "(I)Ljava/util/concurrent/ExecutorService;",
        |ctx, args| {
            let ps = match args.first() {
                Some(Value::Int(v)) => *v,
                _ => 1,
            };
            let sv =
                try_alloc_concurrent_synthetic(ctx, "java/util/concurrent/ThreadPoolExecutor", 2)?;
            // Use the function's returned (possibly GC-relocated) object,
            // not `sv` directly -- see the BUG FIX comment on
            // `initialize_real_thread_pool_executor`'s return path.
            let result = initialize_real_thread_pool_executor(
                ctx,
                sv,
                ps,
                ps,
                0,
                "MILLISECONDS",
                TpeQueueKind::Linked,
                None,
            )?;
            Ok(result.or(Some(Value::Object(Some(sv)))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        ex,
        "newCachedThreadPool",
        "()Ljava/util/concurrent/ExecutorService;",
        |ctx, _args| {
            let sv =
                try_alloc_concurrent_synthetic(ctx, "java/util/concurrent/ThreadPoolExecutor", 2)?;
            let result = initialize_real_thread_pool_executor(
                ctx,
                sv,
                0,
                i32::MAX,
                60,
                "SECONDS",
                TpeQueueKind::Synchronous,
                None,
            )?;
            Ok(result.or(Some(Value::Object(Some(sv)))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        ex,
        "newCachedThreadPool",
        "(Ljava/util/concurrent/ThreadFactory;)Ljava/util/concurrent/ExecutorService;",
        |ctx, args| {
            let factory = match args.first() {
                Some(Value::Object(Some(f))) => Some(*f),
                _ => None,
            };
            let sv =
                try_alloc_concurrent_synthetic(ctx, "java/util/concurrent/ThreadPoolExecutor", 2)?;
            let result = initialize_real_thread_pool_executor(
                ctx,
                sv,
                0,
                i32::MAX,
                60,
                "SECONDS",
                TpeQueueKind::Synchronous,
                factory,
            )?;
            Ok(result.or(Some(Value::Object(Some(sv)))))
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        ex,
        "newSingleThreadExecutor",
        "()Ljava/util/concurrent/ExecutorService;",
        |ctx, _args| {
            let sv =
                try_alloc_concurrent_synthetic(ctx, "java/util/concurrent/ThreadPoolExecutor", 2)?;
            let result = initialize_real_thread_pool_executor(
                ctx,
                sv,
                1,
                1,
                0,
                "MILLISECONDS",
                TpeQueueKind::Linked,
                None,
            )?;
            Ok(result.or(Some(Value::Object(Some(sv)))))
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    }

// ---------------------------------------------------------------------------
// TimeUnit — enum-like with conversion methods
// ---------------------------------------------------------------------------
pub fn tu_nanos_per(ordinal: i32) -> i64 {
    match ordinal {
        0 => 1,
        1 => 1_000,
        2 => 1_000_000,
        3 => 1_000_000_000,
        4 => 60_000_000_000,
        5 => 3_600_000_000_000,
        6 => 86_400_000_000_000,
        _ => 1,
    }
}

/// Scale `d` from a source unit (`from_nanos` per unit) into a target unit
/// (`to_nanos` per unit), saturating instead of overflowing.
///
/// The naive `d * tu_nanos_per(o) / divisor` these conversions used to run
/// overflows `i64` for perfectly ordinary arguments, because it multiplies up
/// to nanoseconds *before* dividing back down: `MILLISECONDS.toMillis(x)`
/// scales `x` by 10^6 only to divide it out again, so it blows up for any
/// `x > ~9.2e12` even though the answer is just `x`. `DAYS.toNanos(106_752)`
/// exceeds `i64::MAX` outright. In a debug build that is an "attempt to
/// multiply with overflow" panic raised from inside a timeout conversion — a
/// hard abort where the JDK would have returned a number; in release it wraps
/// and hands back a *negative* timeout, which reads as "already expired".
///
/// `TimeUnit.convert`/`toXxx` in the real JDK saturate to `Long.MAX_VALUE` /
/// `Long.MIN_VALUE`. Do the arithmetic in `i128` — `2^63 * 86_400_000_000_000`
/// is ~2^110, comfortably inside it — and clamp on the way out.
pub fn tu_scale(d: i64, from_nanos: i64, to_nanos: i64) -> i64 {
    let scaled = (d as i128) * (from_nanos as i128) / (to_nanos.max(1) as i128);
    scaled.clamp(i64::MIN as i128, i64::MAX as i128) as i64
}

/// Read a `TimeUnit`'s ordinal.
///
/// The constants are minted as synthetic objects whose single field is
/// unnamed, so `set_field_by_name(.., "ordinal", ..)` and `get_field(.., 0)`
/// do not necessarily address the same storage. Readers that consulted only
/// slot 0 saw the uninitialised default and silently decayed to ordinal 0 —
/// `NANOSECONDS` — which made every conversion off by up to 10^9 and turned
/// `SECONDS.toNanos(1)` into `1`. Always go through this pair so the name and
/// the slot cannot drift apart again.
pub fn tu_ordinal(ctx: &mut dyn NativeContext, this: ObjectRef) -> i32 {
    match ctx.get_field_by_name(this, "ordinal") {
        Value::Int(v) => v,
        _ => match ctx.get_field(this, 0) {
            Value::Int(v) => v,
            // SECONDS: the least surprising unit to attribute an
            // unreadable ordinal to, and the pre-existing default here.
            _ => 3,
        },
    }
}

/// `TimeUnit`'s constants in declaration (= ordinal) order.
pub const TIME_UNIT_NAMES: [&str; 7] = [
    "NANOSECONDS",
    "MICROSECONDS",
    "MILLISECONDS",
    "SECONDS",
    "MINUTES",
    "HOURS",
    "DAYS",
];

/// Write a `TimeUnit`'s ordinal to both the named field and slot 0 — see
/// [`tu_ordinal`] for why one of the two is not enough.
pub fn tu_set_ordinal(ctx: &mut dyn NativeContext, this: ObjectRef, ordinal: i32) {
    ctx.set_field_by_name(this, "ordinal", Value::Int(ordinal));
    ctx.set_field(this, 0, Value::Int(ordinal));
}

/// The `values()` body of a synthetic enum (i8-L2): a typed `E[]` of `count`
/// constants, `make(ctx, i)` building the i-th, with the array rooted across
/// each (allocating) call. These bodies used to fill an `Object[]` held raw.
/// The component may load, so call this before any reference is live.
pub fn synthetic_enum_values(
    ctx: &mut dyn NativeContext,
    enum_class: &str,
    count: usize,
    make: fn(&mut dyn NativeContext, usize) -> Result<ObjectRef, MethodCallFailed>,
) -> MethodCallResult {
    let component = crate::lang_class::loading_component_id(ctx, enum_class);
    let mut scope = NativeHandleScope::new(ctx);
    let arr = scope.new_ref_array(component, count);
    let arr_h = scope.root(arr);
    for i in 0..count {
        let constant = make(&mut *scope, i)?;
        let arr = scope.get(&arr_h);
        scope.set_array_element(arr, i, Value::Object(Some(constant)));
    }
    Ok(Some(Value::Object(Some(scope.get(&arr_h)))))
}

pub fn register_timeunit_natives(r: &mut NativeMethodRegistry) {
            let c = "java/util/concurrent/TimeUnit";
    // <clinit>: initialise the 7 static enum constants. Real JDK `TimeUnit`
    // declares many static `long` scalars before/after the enum refs; writing
    // by slot index `0..6` misplaces values so `GETSTATIC MINUTES` still reads
    // null. Resolve each constant by name (static-only index) and set
    // `java.lang.Enum.ordinal` on each instance for natives / mixed paths.
    r.register(c, "<clinit>", "()V", |ctx, _args| {
        const NAMES: [&str; 7] = [
            "NANOSECONDS",
            "MICROSECONDS",
            "MILLISECONDS",
            "SECONDS",
            "MINUTES",
            "HOURS",
            "DAYS",
        ];
        for (i, name) in NAMES.iter().enumerate() {
            let tu = try_alloc_concurrent_synthetic(ctx, "java/util/concurrent/TimeUnit", 1)?;
            tu_set_ordinal(ctx, tu, i as i32);
            ctx.set_static_field_by_name(
                "java/util/concurrent/TimeUnit",
                name,
                Value::Object(Some(tu)),
            );
        }
        // `$VALUES`, which `Class.getEnumConstants` and through it
        // `EnumSet.allOf` read. Without it `EnumSet.allOf(TimeUnit.class)`
        // answered 0 against HotSpot's 7 while every direct use of the enum
        // worked (`apps/probes/SyntheticEnumSurface`).
        //
        // RE-READ from the statics rather than filled from the refs minted
        // above: `new_ref_array` allocates and can move them. The re-read is
        // also what makes `values()[i] == CONSTANT`, which `Enum.valueOf`,
        // `getEnumConstants` and `EnumSet` all rely on. Same obligation
        // `publish_synthetic_enum_constants` states for the enums that use it;
        // this clinit predates that helper and keeps its own by-name writes
        // because real `TimeUnit` interleaves static `long` scalars with the
        // enum refs, so slot-index writes misplace them.
        if let Some(cid) = ctx.class_id_by_name("java/util/concurrent/TimeUnit") {
            let values = ctx.new_ref_array(cid, NAMES.len());
            for (i, name) in NAMES.iter().enumerate() {
                let published = match ctx.static_field_index_by_name(cid, name) {
                    Some(slot) => ctx.get_static_field(cid, slot),
                    None => Value::Object(None),
                };
                ctx.set_array_element(values, i, published);
            }
            ctx.set_static_field_by_name(
                "java/util/concurrent/TimeUnit",
                "$VALUES",
                Value::Object(Some(values)),
            );
        }
        Ok(None)
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(
        c,
        "valueOf",
        "(Ljava/lang/String;)Ljava/util/concurrent/TimeUnit;",
        |ctx, args| {
            // `Enum.valueOf(TimeUnit.class, name)`: NPE on null, IAE on an
            // unknown name (this answered `SECONDS` for both), and otherwise
            // the class's OWN constant -- JLS 8.9 identity, which the minted
            // copy broke (`valueOf("SECONDS") != SECONDS`; i8-L2, fixed i9-L2).
            let name = match args.first() {
                Some(Value::Object(Some(o))) => ctx.read_string(*o).unwrap_or_default(),
                _ => {
                    return Err(RuntimeError::NullPointerException {
                        message: Some("Name is null".to_string()),
                    }
                    .into())
                }
            };
            let Some(ordinal) = TIME_UNIT_NAMES.iter().position(|n| *n == name) else {
                return Err(RuntimeError::IllegalArgumentException {
                    message: crate::lang_class::no_enum_constant_message(
                        "java.util.concurrent.TimeUnit",
                        &name,
                    ),
                }
                .into());
            };
            if let Some(constant) = crate::lang_system::canonical_enum_constant(
                ctx,
                "java/util/concurrent/TimeUnit",
                &name,
            ) {
                return Ok(Some(constant));
            }
            // Only before `<clinit>` has published the constants.
            let tu = try_alloc_concurrent_synthetic(ctx, "java/util/concurrent/TimeUnit", 1)?;
            // Cast: an index into a seven-element table.
            tu_set_ordinal(ctx, tu, ordinal as i32);
            Ok(Some(Value::Object(Some(tu))))
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        c,
        "values",
        "()[Ljava/util/concurrent/TimeUnit;",
        |ctx, _args| {
            // A fresh `TimeUnit[]` of the constants `<clinit>` published, so
            // `values()[3] == SECONDS` (i8-L2, fixed i9-L2). The minting body
            // below is left for a class whose statics are not published.
            if let Some(arr) =
                crate::lang_system::canonical_enum_values(ctx, "java/util/concurrent/TimeUnit")
            {
                return Ok(Some(Value::Object(Some(arr))));
            }
            synthetic_enum_values(ctx, "java/util/concurrent/TimeUnit", 7, |ctx, i| {
                let tu = try_alloc_concurrent_synthetic(ctx, "java/util/concurrent/TimeUnit", 1)?;
                tu_set_ordinal(ctx, tu, i as i32);
                Ok(tu)
            })
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        c,
        "convert",
        "(JLjava/util/concurrent/TimeUnit;)J",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let dur = match args.get(1) {
                Some(Value::Long(v)) => *v,
                _ => 0,
            };
            let su = obj_arg(args, 2)?;
            let to = tu_ordinal(ctx, this);
            let so = tu_ordinal(ctx, su);
            Ok(Some(Value::Long(tu_scale(
                dur,
                tu_nanos_per(so),
                tu_nanos_per(to),
            ))))
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(c, "toNanos", "(J)J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let d = match args.get(1) {
            Some(Value::Long(v)) => *v,
            _ => 0,
        };
        let o = tu_ordinal(ctx, this);
        Ok(Some(Value::Long(tu_scale(d, tu_nanos_per(o), 1))))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(c, "toMicros", "(J)J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let d = match args.get(1) {
            Some(Value::Long(v)) => *v,
            _ => 0,
        };
        let o = tu_ordinal(ctx, this);
        Ok(Some(Value::Long(tu_scale(d, tu_nanos_per(o), 1_000))))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(c, "toMillis", "(J)J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let d = match args.get(1) {
            Some(Value::Long(v)) => *v,
            _ => 0,
        };
        let o = tu_ordinal(ctx, this);
        Ok(Some(Value::Long(tu_scale(d, tu_nanos_per(o), 1_000_000))))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(c, "toSeconds", "(J)J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let d = match args.get(1) {
            Some(Value::Long(v)) => *v,
            _ => 0,
        };
        let o = tu_ordinal(ctx, this);
        Ok(Some(Value::Long(tu_scale(
            d,
            tu_nanos_per(o),
            1_000_000_000,
        ))))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(c, "toMinutes", "(J)J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let d = match args.get(1) {
            Some(Value::Long(v)) => *v,
            _ => 0,
        };
        let o = tu_ordinal(ctx, this);
        Ok(Some(Value::Long(tu_scale(
            d,
            tu_nanos_per(o),
            60_000_000_000,
        ))))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(c, "toHours", "(J)J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let d = match args.get(1) {
            Some(Value::Long(v)) => *v,
            _ => 0,
        };
        let o = tu_ordinal(ctx, this);
        Ok(Some(Value::Long(tu_scale(
            d,
            tu_nanos_per(o),
            3_600_000_000_000,
        ))))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(c, "toDays", "(J)J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let d = match args.get(1) {
            Some(Value::Long(v)) => *v,
            _ => 0,
        };
        let o = tu_ordinal(ctx, this);
        Ok(Some(Value::Long(tu_scale(
            d,
            tu_nanos_per(o),
            86_400_000_000_000,
        ))))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(c, "name", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let o = tu_ordinal(ctx, this);
        let n = match o {
            0 => "NANOSECONDS",
            1 => "MICROSECONDS",
            2 => "MILLISECONDS",
            3 => "SECONDS",
            4 => "MINUTES",
            5 => "HOURS",
            6 => "DAYS",
            _ => "SECONDS",
        };
        let s = ctx.create_string(n);
        Ok(Some(Value::Object(Some(s))))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(c, "ordinal", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Int(tu_ordinal(ctx, this))))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(c, "toString", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let o = tu_ordinal(ctx, this);
        let n = match o {
            0 => "NANOSECONDS",
            1 => "MICROSECONDS",
            2 => "MILLISECONDS",
            3 => "SECONDS",
            4 => "MINUTES",
            5 => "HOURS",
            6 => "DAYS",
            _ => "SECONDS",
        };
        let s = ctx.create_string(n);
        Ok(Some(Value::Object(Some(s))))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    // W2: `TimeUnit.sleep` used to return immediately. A no-op sleep is not a
    // harmless speed-up — it is the classic cause of "works on my machine"
    // flakiness in this tree, because every `SECONDS.sleep(1)` backoff,
    // poll-loop pacer and rate limiter degenerates into a hot spin. Sleep for
    // real, in the same 25ms slices `awaitTermination` above uses, so a GC
    // safepoint is never blocked for longer than one slice.
    r.register(c, "sleep", "(J)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let d = match args.get(1) {
            Some(Value::Long(v)) => *v,
            Some(Value::Int(v)) => *v as i64,
            _ => 0,
        };
        // A non-positive duration is a no-op in the real JDK too.
        if d <= 0 {
            return Ok(None);
        }
        let o = tu_ordinal(ctx, this);
        let nanos = (d as i128) * (tu_nanos_per(o) as i128);
        // Interpreter round i1 wave 24 (L7): the real `TimeUnit.sleep` is
        // `Thread.sleep(ms, ns)`, which ends with `InterruptedException("sleep
        // interrupted")` when the thread is interrupted before or during the
        // sleep. This loop slept every 25 ms slice to the end and never
        // looked at the interrupt, so `shutdownNow()` could not stop a worker
        // in `SECONDS.sleep(60)`. `sleepNanos0` sleeps in 10 ms slices under
        // a timed blocking region, polls the interrupt, and never forms an
        // `Instant + Duration` (so `DAYS.sleep(Long.MAX_VALUE)` cannot
        // overflow); the clamp saturates at `Long.MAX_VALUE` nanoseconds, as
        // `TimeUnit.toNanos` does.
        let clamped = nanos.clamp(0, i64::MAX as i128) as i64;
        crate::lang_system::native_thread_sleep_nanos(ctx, &[Value::Long(clamped)])
    }, cratonvm_native_api::NativeKind::Intrinsic);
    }

// ---------------------------------------------------------------------------
// ObjectInputStream / ObjectOutputStream — stubs
// ---------------------------------------------------------------------------
pub fn register_object_stream_natives(_r: &mut NativeMethodRegistry) {
    // synthetic-stub removed: defers to real JDK bytecode
    // java/io/ObjectInputStream + java/io/ObjectOutputStream have real JDK
    // bytecode. Every method here was a no-op/constant fake (readObject->null,
    // writeObject->no-op, read*/write* returning 0/swallowing). Real
    // serialization bytecode runs over the underlying In/OutputStream natives.
    let _ = _r;
}

// ============================================================================
// Phase 52: java.time enhancements, java.net URL encoding, java.text MessageFormat,
//           java.util.function extras, java.math MathContext/RoundingMode
// ============================================================================

pub(crate) fn register_phase52_natives(registry: &mut NativeMethodRegistry) {
    register_phase52_time_enums(registry);
    register_phase52_offset_datetime(registry);
    register_phase52_clock(registry);
    register_phase52_chrono_unit(registry);
    register_phase52_url_encoding(registry);
    register_phase52_inet_socket_address(registry);
    register_phase52_server_socket_factory(registry);
    register_phase52_message_format(registry);
    register_phase52_date_format(registry);
    register_phase52_math_context(registry);
    register_phase52_rounding_mode(registry);
    register_phase52_function_extras(registry);
    register_phase52_string_buffer(registry);
    register_phase52_byte_order(registry);
    register_phase52_objects_extras(registry);
    }

// ---------------------------------------------------------------------------
// java.time.Month enum — 1-field synthetic (field 0 = Int ordinal 1..12)
// ---------------------------------------------------------------------------
//
// W7-77-guarded-slot-maps.md. Slot 0 is `value` ONLY on the fabricated
// synthetic `java/time/Month`. On the real JDK 25 class it is something else
// entirely: `java.time.Month` declares no instance fields of its own, and
// `javap -p java.lang.Enum` on Eclipse Adoptium 25.0.3.9 gives the whole
// transitive layout as
//
//     0 name (java.lang.String)   1 ordinal (int)   2 hash (int)
//
// so slot 0 is `Enum.name`, a String REFERENCE, and every `set_field` below
// puts a `Value::Int` into it.
//
// WHAT THAT ACTUALLY COSTS was measured for W7-77 and is *not* what
// W7-69-read-side-alias-instrument.md §6 predicted. That record called this
// "the sharpest shape in the census" and said an escape would leave "a bogus
// pointer for the collector to mark and move". It would not: both write paths
// screen the `Value` tag before anything reaches the collector.
//
//   * Compact layout (`CRATONVM_COMPACT_REF_FIELDS`, ON by default) —
//     `types::field_layout::write_compact_field`'s `FieldStorageKind::Reference`
//     arm maps every non-`Object` `Value` through `_ => 0`. The Int is DROPPED
//     and the slot reads back null.
//   * Legacy 16-byte cells — `gc::gen_heap::for_each_ref_slot`'s final arm
//     matches on the stored `Value` tag (`Value::Object(Some(_))`), not on the
//     class's declared refs, so an `Int` cell is never visited.
//
// So this is a wrong ANSWER, not heap corruption, on both layouts. The real
// consequence of an escape is quieter and still bad: the write nulls
// `Enum.name` on a SHARED enum constant, and `getValue()` then reads
// `Object(None)`, whose `.as_int()` is `None`, so `unwrap_or(1)` answers
// JANUARY for every month of the year.
//
// UPDATED 2026-08-12 — W7-84-primitive-in-reference-store.md. The compact
// bullet above was true of `zgc`, `g1` and `heap` and NOT of `gen_heap`, which
// boxed the value into an `AUTOBOX_CLASS_ID` wrapper and un-boxed it on read;
// that three-way disagreement was itself the defect, and all four have now
// converged on boxing. An escape therefore no longer nulls `Enum.name` on any
// collector, and JANUARY-for-every-month is no longer the failure mode.
//
// It has not become harmless, and the witness below is exactly as necessary as
// it was. The slot now holds a wrapper, so `getValue()` reads its `Int` back
// correctly while `Enum.name()` — real JDK bytecode reading the same slot as a
// `String` — gets an object with no class name and no methods. Only the SHAPE
// of what the guard prevents changed. This note exists so the next reader does
// not conclude from a green `getValue()` that the witness can go.
//
// `month_slot0_is_synthetic` keeps that hypothetical hypothetical. It is the
// same remedy shape as `vm_exec.rs`'s `eetop` witness for `Thread` and
// `lang_class.rs`'s `has_named_layout` for `Method`: ask for a field NAME the
// real class's hierarchy has and the fabricated stub does not. A slot COUNT
// cannot identify a layout.
//
// THERE IS A SECOND, IDENTICAL MAP: `util_time.rs`'s own `MONTH_FIELD_VALUE`
// (also 0) with its own `alloc_month`. Every one of its five registered
// triples — `of`, `getValue`, `length(Z)I`, `maxLength`, `minLength` — is
// overwritten by `register_phase52_time_enums` below, because
// `register_synthetic_overrides` calls `register_t25_natives` first and
// `register_phase52_natives` second and `register()` is last-write-wins. It is
// DEAD, but it is dead by call ORDER, not by construction: reorder those two
// lines and it becomes the winner. Renumber one of these maps without the
// other and the tree has two different answers for slot 0.
pub const MONTH_FIELD_VALUE: usize = 0;

/// This native's belief about `java/time/Month`, published so
/// `read_alias::verify_declared_slot_maps` sweeps it against whatever class is
/// actually loaded.
///
/// Deliberately states what this code BELIEVES, not the real JDK layout. A map
/// that publishes the correct answer sweeps clean and measures nothing — the
/// point of the declaration is that the sweep reports the disagreement.
pub static MONTH_SLOT_MAP: cratonvm_native_api::read_alias::SlotMap =
    cratonvm_native_api::read_alias::SlotMap {
        class: "java/time/Month",
        slots: &[(MONTH_FIELD_VALUE, "value")],
        origin: "native-builtins/src/phases_early.rs MONTH_FIELD_VALUE",
    };

/// Class-side witness: is this receiver the fabricated 1-field
/// `java/time/Month` stub, so that slot 0 really is the `int` month value?
///
/// Asked by a field name the real class's hierarchy declares and the stub does
/// not. `java.time.Month` declares no instance fields itself, so the witness
/// is `java.lang.Enum.name`; `resolve_field_index_by_class_id` searches the
/// hierarchy, so it resolves on a real `Month` and misses on the stub. Keyed
/// on the RECEIVER's `ClassId` rather than on the name `java/time/Month`,
/// because the name-based lookup answers `None` for "two loaders define it" as
/// well as for "nobody does", and this predicate must not read the first as
/// the second.
pub fn month_slot0_is_synthetic(ctx: &dyn NativeContext, obj: ObjectRef) -> bool {
    ctx.resolve_field_index_by_class_id(ctx.class_id_of_object(obj), "name")
        .is_none()
}

/// Write the synthetic month value, refusing on a real-layout receiver.
///
/// The refusal is the guard W7-77 added: without it this is `Enum.name := 0`
/// on a shared enum constant. It cannot fire today (see the registrar note
/// above — `register_phase52_time_enums` is reachable only from
/// `register_synthetic_overrides`, which `vm_init.rs` calls only under
/// `config.use_synthetic_jdk`, and that mode skips boot-classpath discovery
/// entirely) and `month_registrars_stay_synthetic_only` in
/// `native-builtins/tests/guarded_slot_maps.rs` is what keeps that true.
pub fn month_set_value(ctx: &mut dyn NativeContext, obj: ObjectRef, val: i32) {
    if cratonvm_native_api::layout_alias::enabled() {
        cratonvm_native_api::read_alias::observe_read(
            &*ctx,
            obj,
            MONTH_FIELD_VALUE,
            "value",
            "native-builtins/src/phases_early.rs::month_set_value (write)",
        );
    }
    if !month_slot0_is_synthetic(&*ctx, obj) {
        // Real `java.time.Month`: slot 0 is `Enum.name`. Dropping the write is
        // strictly louder than performing it — performing it nulls the name of
        // a shared enum constant and still loses the value.
        return;
    }
    ctx.set_field(obj, MONTH_FIELD_VALUE, Value::Int(val));
}

/// Allocate a synthetic `java/time/Month` carrying `val`.
///
/// The one funnel for all four allocation sites, so the witness above cannot
/// be applied to three of them and forgotten on the fourth — the failure mode
/// `reference_convert_the_idiom_not_the_sites` names.
pub fn month_alloc(ctx: &mut dyn NativeContext, val: i32) -> Result<ObjectRef, MethodCallFailed> {
    let obj = try_alloc_concurrent_synthetic(ctx, "java/time/Month", 1)?;
    month_set_value(ctx, obj, val);
    Ok(obj)
}

/// `Month`'s constants in ordinal (`javap`) order; the same list as the
/// synthetic class model's `enum_constant_fields` row in
/// `classloading/src/class_manager.rs`.
pub const MONTH_NAMES: [&str; 12] = [
    "JANUARY",
    "FEBRUARY",
    "MARCH",
    "APRIL",
    "MAY",
    "JUNE",
    "JULY",
    "AUGUST",
    "SEPTEMBER",
    "OCTOBER",
    "NOVEMBER",
    "DECEMBER",
];

/// The `Month` constant for `v` (1..=12): the one `<clinit>` published (JLS
/// 8.9 identity: `Month.of(1) == JANUARY`), or a minted one in the same
/// slot-0 layout when nothing is published (i10-L2; `dow_constant`'s shape).
pub fn month_constant(ctx: &mut dyn NativeContext, v: i32) -> Result<ObjectRef, MethodCallFailed> {
    if let Some(Value::Object(Some(constant))) =
        crate::lang_system::canonical_enum_constant(ctx, "java/time/Month", p52_month_name(v))
    {
        return Ok(constant);
    }
    month_alloc(ctx, v)
}

/// A synthetic enum's `<clinit>` for a class whose natives keep their OWN
/// instance layout (`Month`, `ChronoUnit`, `RoundingMode`: slot 0 is an
/// `int`; `FormatStyle`: `Enum`'s `name` / `ordinal`), so
/// `lang_class::publish_synthetic_enum_constants`, which always mints the
/// `Enum` layout, cannot serve them (i10-L2).
///
/// `mint(ctx, ordinal)` builds constant `names[ordinal]` through the class's
/// own allocator. Each constant is stored into its static (a GC root) before
/// the next one is minted, and `$VALUES` is RE-READ from the statics after
/// its array is allocated, for the reasons `publish_synthetic_enum_constants`
/// gives. Idempotent: a class whose first constant is already published is
/// left alone. A static the class model does not declare is a silent no-op
/// (`set_static_field_by_name`), so the model and `names` must agree.
pub fn publish_minted_enum_constants(
    ctx: &mut dyn NativeContext,
    class: &str,
    names: &[&str],
    mint: fn(&mut dyn NativeContext, usize) -> Result<ObjectRef, MethodCallFailed>,
) -> MethodCallResult {
    let Some(cid) = ctx.class_id_by_name(class) else {
        return Ok(None);
    };
    if let Some(first) = names.first() {
        if let Some(slot) = ctx.static_field_index_by_name(cid, first) {
            if matches!(ctx.get_static_field(cid, slot), Value::Object(Some(_))) {
                return Ok(None);
            }
        }
    }
    for (ordinal, &name) in names.iter().enumerate() {
        let constant = mint(ctx, ordinal)?;
        ctx.set_static_field_by_name(class, name, Value::Object(Some(constant)));
    }
    let values = ctx.new_ref_array(cid, names.len());
    for (idx, &name) in names.iter().enumerate() {
        let published = match ctx.static_field_index_by_name(cid, name) {
            Some(slot) => ctx.get_static_field(cid, slot),
            None => Value::Object(None),
        };
        // `set_array_element` does not allocate: `values` cannot move here.
        ctx.set_array_element(values, idx, published);
    }
    ctx.set_static_field_by_name(class, "$VALUES", Value::Object(Some(values)));
    Ok(None)
}

/// Read the synthetic month value, defaulting to January (1) exactly as every
/// call site did before W7-77 routed them through here.
///
/// On a real-layout receiver this now answers 1 *because the witness said so*
/// rather than because `Value::Object(None).as_int()` happened to be `None` —
/// the same answer, arrived at deliberately, and observed by the census.
pub fn month_value(ctx: &dyn NativeContext, this: ObjectRef) -> i32 {
    if cratonvm_native_api::layout_alias::enabled() {
        cratonvm_native_api::read_alias::observe_read(
            ctx,
            this,
            MONTH_FIELD_VALUE,
            "value",
            "native-builtins/src/phases_early.rs::month_value",
        );
    }
    if !month_slot0_is_synthetic(ctx, this) {
        return 1;
    }
    ctx.get_field(this, MONTH_FIELD_VALUE).as_int().unwrap_or(1)
}

pub fn register_phase52_time_enums(r: &mut NativeMethodRegistry) {
            // Unconditional, matching `register_io_natives`' publication of
    // `BB_SLOT_MAP`: gating it on the flag would leave a run that enables
    // `CRATONVM_DBG_LAYOUT_ALIAS` later with nothing to sweep, which is a
    // detector reporting clean because it cannot see.
    cratonvm_native_api::read_alias::declare_slot_map(&MONTH_SLOT_MAP);
    let month = "java/time/Month";
    // Every body that answers a constant goes through `month_constant`, so
    // the answer is the constant this `<clinit>` published (i10-L2).
    r.register(month, "<clinit>", "()V", |ctx, _args| {
        publish_minted_enum_constants(ctx, "java/time/Month", &MONTH_NAMES, |ctx, i| {
            // Cast: an index below twelve.
            month_alloc(ctx, i as i32 + 1)
        })
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(month, "of", "(I)Ljava/time/Month;", |ctx, args| {
        let val = args[0].as_int().unwrap_or(1);
        if !(1..=12).contains(&val) {
            return Err(RuntimeError::IllegalArgumentException {
                message: format!("Invalid value for MonthOfYear: {val}"),
            }
            .into());
        }
        let obj = month_constant(ctx, val)?;
        Ok(Some(Value::Object(Some(obj))))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(month, "values", "()[Ljava/time/Month;", |ctx, _args| {
        if let Some(arr) = crate::lang_system::canonical_enum_values(ctx, "java/time/Month") {
            return Ok(Some(Value::Object(Some(arr))));
        }
        synthetic_enum_values(ctx, "java/time/Month", 12, |ctx, i| {
            month_alloc(ctx, i as i32 + 1)
        })
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(
        month,
        "valueOf",
        "(Ljava/lang/String;)Ljava/time/Month;",
        |ctx, args| {
            // `Enum.valueOf`: exact names (this upper-cased its argument, so
            // `valueOf("january")` succeeded), NPE on null (i10-L2).
            let name = match args.first() {
                Some(Value::Object(Some(o))) => ctx.read_string(*o).unwrap_or_default(),
                _ => {
                    return Err(RuntimeError::NullPointerException {
                        message: Some("Name is null".to_string()),
                    }
                    .into())
                }
            };
            let Some(index) = MONTH_NAMES.iter().position(|n| *n == name) else {
                return Err(RuntimeError::IllegalArgumentException {
                    // Shared with `Enum.valueOf` and the three other
                    // synthetic enum `valueOf` bodies so the shape cannot
                    // drift. `java.time.Month` is top-level, so its
                    // canonical name is its binary name with dots.
                    message: crate::lang_class::no_enum_constant_message("java.time.Month", &name),
                }
                .into());
            };
            // Cast: an index below twelve.
            let obj = month_constant(ctx, index as i32 + 1)?;
            Ok(Some(Value::Object(Some(obj))))
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(month, "getValue", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Int(month_value(&*ctx, this))))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(month, "ordinal", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let v = month_value(&*ctx, this);
        Ok(Some(Value::Int(v - 1)))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(month, "name", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let v = month_value(&*ctx, this);
        let s = ctx.create_string(p52_month_name(v));
        Ok(Some(Value::Object(Some(s))))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(month, "toString", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let v = month_value(&*ctx, this);
        let s = ctx.create_string(p52_month_name(v));
        Ok(Some(Value::Object(Some(s))))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(month, "length", "(Z)I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let v = month_value(&*ctx, this);
        let leap = args[1].as_int().unwrap_or(0) != 0;
        let len = match v {
            1 => 31,
            2 => {
                if leap {
                    29
                } else {
                    28
                }
            }
            3 => 31,
            4 => 30,
            5 => 31,
            6 => 30,
            7 => 31,
            8 => 31,
            9 => 30,
            10 => 31,
            11 => 30,
            12 => 31,
            _ => 30,
        };
        Ok(Some(Value::Int(len)))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(month, "maxLength", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let v = month_value(&*ctx, this);
        let len = match v {
            2 => 29,
            4 | 6 | 9 | 11 => 30,
            _ => 31,
        };
        Ok(Some(Value::Int(len)))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(month, "minLength", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let v = month_value(&*ctx, this);
        let len = match v {
            2 => 28,
            4 | 6 | 9 | 11 => 30,
            _ => 31,
        };
        Ok(Some(Value::Int(len)))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(month, "plus", "(J)Ljava/time/Month;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let v = month_value(&*ctx, this);
        // Reduced mod 12 FIRST, as `Month.plus` does (`(int) (months % 12)`):
        // `v - 1 + add` overflowed `i64` for an amount near `Long.MAX_VALUE`.
        let add = args[1].as_long().unwrap_or(0).rem_euclid(12);
        // Cast: a value in 1..=12.
        let new_val = ((i64::from(v) - 1 + add) % 12 + 1) as i32;
        let obj = month_constant(ctx, new_val)?;
        Ok(Some(Value::Object(Some(obj))))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(month, "minus", "(J)Ljava/time/Month;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let v = month_value(&*ctx, this);
        // Reduced mod 12 first, as `Month.minus` does; `v - 1 - sub` overflowed
        // for an amount near `Long.MIN_VALUE`.
        let sub = args[1].as_long().unwrap_or(0).rem_euclid(12);
        // Cast: a value in 1..=12.
        let new_val = ((i64::from(v) - 1 - sub + 12) % 12 + 1) as i32;
        let obj = month_constant(ctx, new_val)?;
        Ok(Some(Value::Object(Some(obj))))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(
        month,
        "firstMonthOfQuarter",
        "()Ljava/time/Month;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let v = month_value(&*ctx, this);
            let first = ((v - 1) / 3) * 3 + 1;
            let obj = month_constant(ctx, first)?;
            Ok(Some(Value::Object(Some(obj))))
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );

    // --- java.time.DayOfWeek ---
    // Every body reads the value through `dow_value` and answers a constant
    // through `dow_constant`, so a published constant (`DayOfWeek.FRIDAY`) and
    // one these bodies minted behave alike, and an answered constant is the
    // published one (i9-L2).
    let dow = "java/time/DayOfWeek";
    r.register(dow, "of", "(I)Ljava/time/DayOfWeek;", |ctx, args| {
        let val = args[0].as_int().unwrap_or(1);
        if !(1..=7).contains(&val) {
            return Err(RuntimeError::IllegalArgumentException {
                message: format!("Invalid value for DayOfWeek: {val}"),
            }
            .into());
        }
        let obj = dow_constant(ctx, val)?;
        Ok(Some(Value::Object(Some(obj))))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(dow, "getValue", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Int(dow_value(ctx, this))))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(dow, "ordinal", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let v = dow_value(ctx, this);
        Ok(Some(Value::Int(v - 1)))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(dow, "name", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let v = dow_value(ctx, this);
        let s = ctx.create_string(p52_day_of_week_name(v));
        Ok(Some(Value::Object(Some(s))))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(dow, "toString", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let v = dow_value(ctx, this);
        let s = ctx.create_string(p52_day_of_week_name(v));
        Ok(Some(Value::Object(Some(s))))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(
        dow,
        "valueOf",
        "(Ljava/lang/String;)Ljava/time/DayOfWeek;",
        |ctx, args| {
            // `Enum.valueOf`: exact names (this upper-cased its argument),
            // NPE on null (i10-L2).
            let name = match args.first() {
                Some(Value::Object(Some(o))) => ctx.read_string(*o).unwrap_or_default(),
                _ => {
                    return Err(RuntimeError::NullPointerException {
                        message: Some("Name is null".to_string()),
                    }
                    .into())
                }
            };
            let val = match name.as_str() {
                "MONDAY" => 1,
                "TUESDAY" => 2,
                "WEDNESDAY" => 3,
                "THURSDAY" => 4,
                "FRIDAY" => 5,
                "SATURDAY" => 6,
                "SUNDAY" => 7,
                _ => {
                    return Err(RuntimeError::IllegalArgumentException {
                        message: crate::lang_class::no_enum_constant_message(
                            "java.time.DayOfWeek",
                            &name,
                        ),
                    }
                    .into())
                }
            };
            let obj = dow_constant(ctx, val)?;
            Ok(Some(Value::Object(Some(obj))))
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(dow, "values", "()[Ljava/time/DayOfWeek;", |ctx, _args| {
        synthetic_enum_values(ctx, "java/time/DayOfWeek", 7, |ctx, i| {
            // Cast: an index below seven.
            dow_constant(ctx, i as i32 + 1)
        })
    }, cratonvm_native_api::NativeKind::Intrinsic);
    // `DayOfWeek.plus` reduces the amount mod 7 FIRST (`(int) (days % 7)`);
    // adding the raw `long` overflowed for an amount near `Long.MAX_VALUE`.
    r.register(dow, "plus", "(J)Ljava/time/DayOfWeek;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let v = i64::from(dow_value(ctx, this));
        let add = args[1].as_long().unwrap_or(0).rem_euclid(7);
        // Cast: a value in 1..=7.
        let new_val = ((v - 1 + add) % 7 + 1) as i32;
        let obj = dow_constant(ctx, new_val)?;
        Ok(Some(Value::Object(Some(obj))))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(dow, "minus", "(J)Ljava/time/DayOfWeek;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let v = i64::from(dow_value(ctx, this));
        let sub = args[1].as_long().unwrap_or(0).rem_euclid(7);
        // Cast: a value in 1..=7.
        let new_val = ((v - 1 - sub + 7) % 7 + 1) as i32;
        let obj = dow_constant(ctx, new_val)?;
        Ok(Some(Value::Object(Some(obj))))
    }, cratonvm_native_api::NativeKind::Bridge);
}

pub fn p52_month_name(v: i32) -> &'static str {
    match v {
        1 => "JANUARY",
        2 => "FEBRUARY",
        3 => "MARCH",
        4 => "APRIL",
        5 => "MAY",
        6 => "JUNE",
        7 => "JULY",
        8 => "AUGUST",
        9 => "SEPTEMBER",
        10 => "OCTOBER",
        11 => "NOVEMBER",
        12 => "DECEMBER",
        _ => "JANUARY",
    }
}

pub fn p52_day_of_week_name(v: i32) -> &'static str {
    match v {
        1 => "MONDAY",
        2 => "TUESDAY",
        3 => "WEDNESDAY",
        4 => "THURSDAY",
        5 => "FRIDAY",
        6 => "SATURDAY",
        7 => "SUNDAY",
        _ => "MONDAY",
    }
}

/// A `DayOfWeek`'s value (1 = MONDAY .. 7 = SUNDAY), from either of the two
/// layouts a synthetic-JDK run hands these natives (i9-L2):
///
/// * the phase-52 bodies' own minted constants, whose instance slot 0 holds
///   the `int` value;
/// * the constants `lang_class::day_of_week_clinit` publishes into the
///   statics (`publish_synthetic_enum_constants`), which carry `java/lang/Enum`'s
///   `name` / `ordinal` instead -- slot 0 there is the NAME string. `getValue`
///   returned that string for an `()I` method, and every other body read
///   MONDAY for every published constant (`DayOfWeek.FRIDAY.getValue()`).
pub fn dow_value(ctx: &mut dyn NativeContext, this: ObjectRef) -> i32 {
    if let Value::Int(v) = ctx.get_field(this, 0) {
        if (1..=7).contains(&v) {
            return v;
        }
    }
    let (_, ordinal_slot) = crate::phases_late::nio_file::enum_name_ordinal_slots(ctx);
    match ctx.get_field(this, ordinal_slot) {
        Value::Int(o) if (0..7).contains(&o) => o + 1,
        _ => 1,
    }
}

/// The `DayOfWeek` constant for `v` (1..=7): the one `<clinit>` published
/// (JLS 8.9 identity: `DayOfWeek.of(5) == FRIDAY`, `values()[4] == FRIDAY`;
/// i8-L2, fixed i9-L2), or a minted one in the phase-52 layout when nothing
/// is published.
pub fn dow_constant(ctx: &mut dyn NativeContext, v: i32) -> Result<ObjectRef, MethodCallFailed> {
    if let Some(Value::Object(Some(constant))) = crate::lang_system::canonical_enum_constant(
        ctx,
        "java/time/DayOfWeek",
        p52_day_of_week_name(v),
    ) {
        return Ok(constant);
    }
    let obj = try_alloc_concurrent_synthetic(ctx, "java/time/DayOfWeek", 1)?;
    ctx.set_field(obj, 0, Value::Int(v));
    Ok(obj)
}

// ---------------------------------------------------------------------------
// java.time.OffsetDateTime — 2-field synthetic (ldt=0, offset=1)
// ---------------------------------------------------------------------------
pub const ODT_FIELD_LDT: usize = 0;
pub const ODT_FIELD_OFFSET: usize = 1;

pub fn register_phase52_offset_datetime(r: &mut NativeMethodRegistry) {
    let odt = "java/time/OffsetDateTime";

    r.register(
        odt,
        "of",
        "(Ljava/time/LocalDateTime;Ljava/time/ZoneOffset;)Ljava/time/OffsetDateTime;",
        |ctx, args| {
            let ldt = obj_arg(args, 0)?;
            let offset = obj_arg(args, 1)?;
            let obj = try_alloc_concurrent_synthetic(ctx, "java/time/OffsetDateTime", 2)?;
            ctx.set_field(obj, ODT_FIELD_LDT, Value::Object(Some(ldt)));
            ctx.set_field(obj, ODT_FIELD_OFFSET, Value::Object(Some(offset)));
            Ok(Some(Value::Object(Some(obj))))
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );

    r.register(
        odt,
        "of",
        "(IIIIIIILjava/time/ZoneOffset;)Ljava/time/OffsetDateTime;",
        |ctx, args| {
            let year = args[0].as_int().unwrap_or(2000);
            let mo = args[1].as_int().unwrap_or(1);
            let day = args[2].as_int().unwrap_or(1);
            let hour = args[3].as_int().unwrap_or(0);
            let min = args[4].as_int().unwrap_or(0);
            let sec = args[5].as_int().unwrap_or(0);
            let nano = args[6].as_int().unwrap_or(0);
            let offset = obj_arg(args, 7)?;
            let ldt = try_alloc_concurrent_synthetic(ctx, "java/time/LocalDateTime", 7)?;
            ctx.set_field(ldt, 0, Value::Int(year));
            ctx.set_field(ldt, 1, Value::Int(mo));
            ctx.set_field(ldt, 2, Value::Int(day));
            ctx.set_field(ldt, 3, Value::Int(hour));
            ctx.set_field(ldt, 4, Value::Int(min));
            ctx.set_field(ldt, 5, Value::Int(sec));
            ctx.set_field(ldt, 6, Value::Int(nano));
            let obj = try_alloc_concurrent_synthetic(ctx, "java/time/OffsetDateTime", 2)?;
            ctx.set_field(obj, ODT_FIELD_LDT, Value::Object(Some(ldt)));
            ctx.set_field(obj, ODT_FIELD_OFFSET, Value::Object(Some(offset)));
            Ok(Some(Value::Object(Some(obj))))
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );

    r.register(odt, "now", "()Ljava/time/OffsetDateTime;", |ctx, _args| {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        let epoch_sec = now.as_secs() as i64;
        let nano = now.subsec_nanos() as i32;
        let day_secs = ((epoch_sec % 86400) + 86400) % 86400;
        let epoch_day = (epoch_sec - day_secs) / 86400;
        let (year, month, day) = p52_epoch_day_to_ymd(epoch_day);
        let ldt = try_alloc_concurrent_synthetic(ctx, "java/time/LocalDateTime", 7)?;
        ctx.set_field(ldt, 0, Value::Int(year));
        ctx.set_field(ldt, 1, Value::Int(month));
        ctx.set_field(ldt, 2, Value::Int(day));
        ctx.set_field(ldt, 3, Value::Int((day_secs / 3600) as i32));
        ctx.set_field(ldt, 4, Value::Int(((day_secs % 3600) / 60) as i32));
        ctx.set_field(ldt, 5, Value::Int((day_secs % 60) as i32));
        ctx.set_field(ldt, 6, Value::Int(nano));
        let zo = try_alloc_concurrent_synthetic(ctx, "java/time/ZoneOffset", 1)?;
        ctx.set_field(zo, 0, Value::Int(0));
        let obj = try_alloc_concurrent_synthetic(ctx, "java/time/OffsetDateTime", 2)?;
        ctx.set_field(obj, ODT_FIELD_LDT, Value::Object(Some(ldt)));
        ctx.set_field(obj, ODT_FIELD_OFFSET, Value::Object(Some(zo)));
        Ok(Some(Value::Object(Some(obj))))
    }, cratonvm_native_api::NativeKind::Intrinsic);

    r.register(
        odt,
        "toLocalDateTime",
        "()Ljava/time/LocalDateTime;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, ODT_FIELD_LDT)))
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );

    r.register(
        odt,
        "toLocalDate",
        "()Ljava/time/LocalDate;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            if let Value::Object(Some(ldt_ref)) = ctx.get_field(this, ODT_FIELD_LDT) {
                let y = ctx.get_field(ldt_ref, 0).as_int().unwrap_or(2000);
                let m = ctx.get_field(ldt_ref, 1).as_int().unwrap_or(1);
                let d = ctx.get_field(ldt_ref, 2).as_int().unwrap_or(1);
                let ld = try_alloc_concurrent_synthetic(ctx, "java/time/LocalDate", 3)?;
                ctx.set_field(ld, 0, Value::Int(y));
                ctx.set_field(ld, 1, Value::Int(m));
                ctx.set_field(ld, 2, Value::Int(d));
                Ok(Some(Value::Object(Some(ld))))
            } else {
                Ok(Some(Value::Object(None)))
            }
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );

    r.register(
        odt,
        "toLocalTime",
        "()Ljava/time/LocalTime;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            if let Value::Object(Some(ldt_ref)) = ctx.get_field(this, ODT_FIELD_LDT) {
                let h = ctx.get_field(ldt_ref, 3).as_int().unwrap_or(0);
                let mi = ctx.get_field(ldt_ref, 4).as_int().unwrap_or(0);
                let s = ctx.get_field(ldt_ref, 5).as_int().unwrap_or(0);
                let n = ctx.get_field(ldt_ref, 6).as_int().unwrap_or(0);
                let lt = try_alloc_concurrent_synthetic(ctx, "java/time/LocalTime", 4)?;
                ctx.set_field(lt, 0, Value::Int(h));
                ctx.set_field(lt, 1, Value::Int(mi));
                ctx.set_field(lt, 2, Value::Int(s));
                ctx.set_field(lt, 3, Value::Int(n));
                Ok(Some(Value::Object(Some(lt))))
            } else {
                Ok(Some(Value::Object(None)))
            }
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );

    r.register(odt, "getOffset", "()Ljava/time/ZoneOffset;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, ODT_FIELD_OFFSET)))
    }, cratonvm_native_api::NativeKind::Intrinsic);

    r.register(odt, "getYear", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if let Value::Object(Some(ldt_ref)) = ctx.get_field(this, ODT_FIELD_LDT) {
            Ok(Some(ctx.get_field(ldt_ref, 0)))
        } else {
            Ok(Some(Value::Int(0)))
        }
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(odt, "getMonthValue", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if let Value::Object(Some(ldt_ref)) = ctx.get_field(this, ODT_FIELD_LDT) {
            Ok(Some(ctx.get_field(ldt_ref, 1)))
        } else {
            Ok(Some(Value::Int(0)))
        }
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(odt, "getDayOfMonth", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if let Value::Object(Some(ldt_ref)) = ctx.get_field(this, ODT_FIELD_LDT) {
            Ok(Some(ctx.get_field(ldt_ref, 2)))
        } else {
            Ok(Some(Value::Int(0)))
        }
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(odt, "getHour", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if let Value::Object(Some(ldt_ref)) = ctx.get_field(this, ODT_FIELD_LDT) {
            Ok(Some(ctx.get_field(ldt_ref, 3)))
        } else {
            Ok(Some(Value::Int(0)))
        }
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(odt, "getMinute", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if let Value::Object(Some(ldt_ref)) = ctx.get_field(this, ODT_FIELD_LDT) {
            Ok(Some(ctx.get_field(ldt_ref, 4)))
        } else {
            Ok(Some(Value::Int(0)))
        }
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(odt, "getSecond", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if let Value::Object(Some(ldt_ref)) = ctx.get_field(this, ODT_FIELD_LDT) {
            Ok(Some(ctx.get_field(ldt_ref, 5)))
        } else {
            Ok(Some(Value::Int(0)))
        }
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(odt, "getNano", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if let Value::Object(Some(ldt_ref)) = ctx.get_field(this, ODT_FIELD_LDT) {
            Ok(Some(ctx.get_field(ldt_ref, 6)))
        } else {
            Ok(Some(Value::Int(0)))
        }
    }, cratonvm_native_api::NativeKind::Intrinsic);

    r.register(odt, "getMonth", "()Ljava/time/Month;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if let Value::Object(Some(ldt_ref)) = ctx.get_field(this, ODT_FIELD_LDT) {
            let m = ctx.get_field(ldt_ref, 1).as_int().unwrap_or(1);
            let mo = month_alloc(ctx, m)?;
            Ok(Some(Value::Object(Some(mo))))
        } else {
            Ok(Some(Value::Object(None)))
        }
    }, cratonvm_native_api::NativeKind::Intrinsic);

    r.register(
        odt,
        "getDayOfWeek",
        "()Ljava/time/DayOfWeek;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            if let Value::Object(Some(ldt_ref)) = ctx.get_field(this, ODT_FIELD_LDT) {
                let y = ctx.get_field(ldt_ref, 0).as_int().unwrap_or(2000);
                let m = ctx.get_field(ldt_ref, 1).as_int().unwrap_or(1);
                let d = ctx.get_field(ldt_ref, 2).as_int().unwrap_or(1);
                let epoch = p52_ymd_to_epoch_day(y, m, d);
                let dow_val = (((epoch + 3) % 7 + 7) % 7 + 1) as i32;
                // The published constant, as `DayOfWeek.of` answers (i9-L2).
                let dw = dow_constant(ctx, dow_val)?;
                Ok(Some(Value::Object(Some(dw))))
            } else {
                Ok(Some(Value::Object(None)))
            }
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );

    r.register(odt, "toString", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let mut result = String::new();
        if let Value::Object(Some(ldt_ref)) = ctx.get_field(this, ODT_FIELD_LDT) {
            let y = ctx.get_field(ldt_ref, 0).as_int().unwrap_or(2000);
            let m = ctx.get_field(ldt_ref, 1).as_int().unwrap_or(1);
            let d = ctx.get_field(ldt_ref, 2).as_int().unwrap_or(1);
            let h = ctx.get_field(ldt_ref, 3).as_int().unwrap_or(0);
            let mi = ctx.get_field(ldt_ref, 4).as_int().unwrap_or(0);
            let s = ctx.get_field(ldt_ref, 5).as_int().unwrap_or(0);
            result = format!("{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}");
        }
        if let Value::Object(Some(off_ref)) = ctx.get_field(this, ODT_FIELD_OFFSET) {
            let off_secs = ctx.get_field(off_ref, 0).as_int().unwrap_or(0);
            if off_secs == 0 {
                result.push('Z');
            } else {
                let sign = if off_secs < 0 { '-' } else { '+' };
                let abs = off_secs.unsigned_abs();
                let hh = abs / 3600;
                let mm = (abs % 3600) / 60;
                result.push_str(&format!("{sign}{hh:02}:{mm:02}"));
            }
        }
        let s = ctx.create_string(&result);
        Ok(Some(Value::Object(Some(s))))
    }, cratonvm_native_api::NativeKind::Intrinsic);

    r.register(odt, "equals", "(Ljava/lang/Object;)Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if let Value::Object(Some(other)) = &args[1] {
            let ldt_eq = match (
                ctx.get_field(this, ODT_FIELD_LDT),
                ctx.get_field(*other, ODT_FIELD_LDT),
            ) {
                (Value::Object(Some(a)), Value::Object(Some(b))) => {
                    (0..7).all(|i| ctx.get_field(a, i) == ctx.get_field(b, i))                }
                _ => false,
            };
            let off_eq = match (
                ctx.get_field(this, ODT_FIELD_OFFSET),
                ctx.get_field(*other, ODT_FIELD_OFFSET),
            ) {
                (Value::Object(Some(a)), Value::Object(Some(b))) => {
                    ctx.get_field(a, 0) == ctx.get_field(b, 0)                }
                _ => false,
            };
            Ok(Some(Value::Int(if ldt_eq && off_eq { 1 } else { 0 })))        } else {
            Ok(Some(Value::Int(0)))        }    }, cratonvm_native_api::NativeKind::Bridge);
}

pub fn p52_epoch_day_to_ymd(epoch_day: i64) -> (i32, i32, i32) {
    let z = epoch_day + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = (yoe as i64 + era * 400) as i32;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as i32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as i32;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

pub fn p52_ymd_to_epoch_day(y: i32, m: i32, d: i32) -> i64 {
    let y = if m <= 2 { y as i64 - 1 } else { y as i64 };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let m_adj = if m > 2 { m - 3 } else { m + 9 } as u64;
    let doy = (153 * m_adj + 2) / 5 + d as u64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe as i64 - 719_468
}

// ---------------------------------------------------------------------------
// java.time.Clock — 2-field synthetic (zone=0, type=1)
// ---------------------------------------------------------------------------
/// A synthetic `Clock` in zone `zi` (`Clock.system(ZoneId)`, and the two UTC
/// factories through [`p52_utc_clock`]).
///
/// gc-common w20-b: `zi` was held raw across the clock's allocation (a class
/// resolution, which can run `<clinit>`) and stored at its old address.
pub fn p52_clock_in_zone(ctx: &mut dyn NativeContext, zi: ObjectRef) -> MethodCallResult {
    let zi_pin = ctx.pin_native_root(zi);
    let obj = try_alloc_concurrent_synthetic(ctx, "java/time/Clock", 2);
    let zi = ctx.read_native_pin(zi_pin, zi);
    ctx.unpin_native_roots(zi_pin);
    let obj: ObjectRef = obj?;
    ctx.set_field(obj, 0, Value::Object(Some(zi)));
    ctx.set_field(obj, 1, Value::Int(0));
    Ok(Some(Value::Object(Some(obj))))
}

/// `Clock.systemUTC()` / `systemDefaultZone()`: a synthetic `ZoneId` named
/// "UTC" (built rooted, gc-common w20-b) in a synthetic `Clock`.
pub fn p52_utc_clock(ctx: &mut dyn NativeContext) -> MethodCallResult {
    let zi = try_alloc_concurrent_synthetic(ctx, "java/time/ZoneId", 1)?;
    let zi: ObjectRef = alloc_string_into_field(ctx, zi, 0, "UTC");
    p52_clock_in_zone(ctx, zi)
}

/// A synthetic `RoundingMode.HALF_UP` stored into field 1 of the synthetic
/// `MathContext` `mc`; answers `mc`'s current address.
///
/// gc-common w20-b: `MathContext.<init>(int)` and the four `DECIMAL*` /
/// `UNLIMITED` factories held the context raw across the rounding mode's
/// allocation (a class resolution, which can run `<clinit>`).
pub fn p52_store_half_up(
    ctx: &mut dyn NativeContext,
    mc: ObjectRef,
) -> Result<ObjectRef, MethodCallFailed> {
    let mc_pin = ctx.pin_native_root(mc);
    let rm = try_alloc_concurrent_synthetic(ctx, "java/math/RoundingMode", 1);
    let mc = ctx.read_native_pin(mc_pin, mc);
    ctx.unpin_native_roots(mc_pin);
    let rm: ObjectRef = rm?;
    ctx.set_field(rm, 0, Value::Int(4));
    ctx.set_field(mc, 1, Value::Object(Some(rm)));
    Ok(mc)
}

pub fn register_phase52_clock(r: &mut NativeMethodRegistry) {
            let clock = "java/time/Clock";
    r.register(clock, "systemUTC", "()Ljava/time/Clock;", |ctx, _args| {
        p52_utc_clock(ctx)
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(
        clock,
        "systemDefaultZone",
        "()Ljava/time/Clock;",
        |ctx, _args| p52_utc_clock(ctx), cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        clock,
        "system",
        "(Ljava/time/ZoneId;)Ljava/time/Clock;",
        |ctx, args| {
            let zi = obj_arg(args, 0)?;
            p52_clock_in_zone(ctx, zi)
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(clock, "millis", "()J", |_ctx, _args| {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        Ok(Some(Value::Long(now.as_millis() as i64)))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(clock, "instant", "()Ljava/time/Instant;", |ctx, _args| {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        let inst = try_alloc_concurrent_synthetic(ctx, "java/time/Instant", 2)?;
        ctx.set_field(inst, 0, Value::Long(now.as_secs() as i64));
        ctx.set_field(inst, 1, Value::Int(now.subsec_nanos() as i32));
        Ok(Some(Value::Object(Some(inst))))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(clock, "getZone", "()Ljava/time/ZoneId;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(clock, "toString", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let zone_str = if let Value::Object(Some(zi)) = ctx.get_field(this, 0) {
            if let Value::Object(Some(s)) = ctx.get_field(zi, 0) {
                ctx.read_string(s).unwrap_or_else(|| "UTC".to_string())
            } else {
                "UTC".to_string()
            }
        } else {
            "UTC".to_string()
        };
        let s = ctx.create_string(&format!("SystemClock[{zone_str}]"));
        Ok(Some(Value::Object(Some(s))))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    }

// ---------------------------------------------------------------------------
// java.time.temporal.ChronoUnit — 1-field synthetic (tag=0)
// ---------------------------------------------------------------------------
pub fn register_phase52_chrono_unit(r: &mut NativeMethodRegistry) {
            let cu = "java/time/temporal/ChronoUnit";
    // Constants published once by `<clinit>` and answered through
    // `chrono_unit_constant` (JLS 8.9 identity, i10-L2).
    r.register(cu, "<clinit>", "()V", |ctx, _args| {
        publish_minted_enum_constants(ctx, CHRONO_UNIT, &CHRONO_UNIT_NAMES, |ctx, i| {
            // Cast: an index below sixteen.
            chrono_unit_alloc(ctx, i as i32)
        })
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(
        cu,
        "valueOf",
        "(Ljava/lang/String;)Ljava/time/temporal/ChronoUnit;",
        |ctx, args| {
            // `Enum.valueOf`: NPE on null (i10-L2), IAE on an unknown name.
            let name = match args.first() {
                Some(Value::Object(Some(o))) => ctx.read_string(*o).unwrap_or_default(),
                _ => {
                    return Err(RuntimeError::NullPointerException {
                        message: Some("Name is null".to_string()),
                    }
                    .into())
                }
            };
            let tag = p52_chrono_unit_tag(&name);
            if tag < 0 {
                return Err(RuntimeError::IllegalArgumentException {
                    message: crate::lang_class::no_enum_constant_message(
                        "java.time.temporal.ChronoUnit",
                        &name,
                    ),
                }
                .into());
            }
            let obj = chrono_unit_constant(ctx, tag)?;
            Ok(Some(Value::Object(Some(obj))))
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        cu,
        "values",
        "()[Ljava/time/temporal/ChronoUnit;",
        |ctx, _args| {
            if let Some(arr) = crate::lang_system::canonical_enum_values(ctx, CHRONO_UNIT) {
                return Ok(Some(Value::Object(Some(arr))));
            }
            synthetic_enum_values(ctx, CHRONO_UNIT, 16, |ctx, i| {
                // Cast: an index below sixteen.
                chrono_unit_alloc(ctx, i as i32)
            })
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    for method in ["name", "toString"] {
        r.register(cu, method, "()Ljava/lang/String;", |ctx, args| {
            let this = obj_arg(args, 0)?;
            let tag = ctx.get_field(this, 0).as_int().unwrap_or(0);
            let s = ctx.create_string(p52_chrono_unit_name(tag));
            Ok(Some(Value::Object(Some(s))))
        }, cratonvm_native_api::NativeKind::Intrinsic);
    }
    r.register(cu, "ordinal", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(cu, "getDuration", "()Ljava/time/Duration;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let tag = ctx.get_field(this, 0).as_int().unwrap_or(0);
        let nanos: i64 = match tag {
            0 => 1,
            1 => 1_000,
            2 => 1_000_000,
            3 => 1_000_000_000,
            4 => 60_000_000_000,
            5 => 3_600_000_000_000,
            6 => 43_200_000_000_000,
            7 => 86_400_000_000_000,
            8 => 604_800_000_000_000,
            _ => 86_400_000_000_000,
        };
        let secs = nanos / 1_000_000_000;
        let nano_rem = (nanos % 1_000_000_000) as i32;
        let dur = try_alloc_concurrent_synthetic(ctx, "java/time/Duration", 2)?;
        ctx.set_field(dur, 0, Value::Long(secs));
        ctx.set_field(dur, 1, Value::Int(nano_rem));
        Ok(Some(Value::Object(Some(dur))))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(cu, "isDateBased", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let tag = ctx.get_field(this, 0).as_int().unwrap_or(0);
        Ok(Some(Value::Int(if tag >= 7 { 1 } else { 0 })))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(cu, "isTimeBased", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let tag = ctx.get_field(this, 0).as_int().unwrap_or(0);
        Ok(Some(Value::Int(if tag <= 6 { 1 } else { 0 })))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    let tu = "java/time/temporal/TemporalUnit";
    r.register(tu, "getDuration", "()Ljava/time/Duration;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let tag = ctx.get_field(this, 0).as_int().unwrap_or(7);
        let secs: i64 = match tag {
            0..=2 => 0,
            3 => 1,
            4 => 60,
            5 => 3600,
            6 => 43200,
            7 => 86400,
            8 => 604800,
            _ => 86400,
        };
        let dur = try_alloc_concurrent_synthetic(ctx, "java/time/Duration", 2)?;
        ctx.set_field(dur, 0, Value::Long(secs));
        ctx.set_field(dur, 1, Value::Int(0));
        Ok(Some(Value::Object(Some(dur))))
    }, cratonvm_native_api::NativeKind::Bridge);
}

pub const CHRONO_UNIT: &str = "java/time/temporal/ChronoUnit";

/// `ChronoUnit`'s constants in ordinal (`javap`) order, which is also the
/// slot-0 tag [`p52_chrono_unit_tag`] assigns; the same list as the synthetic
/// class model's `enum_constant_fields` row in `class_manager.rs`.
pub const CHRONO_UNIT_NAMES: [&str; 16] = [
    "NANOS",
    "MICROS",
    "MILLIS",
    "SECONDS",
    "MINUTES",
    "HOURS",
    "HALF_DAYS",
    "DAYS",
    "WEEKS",
    "MONTHS",
    "YEARS",
    "DECADES",
    "CENTURIES",
    "MILLENNIA",
    "ERAS",
    "FOREVER",
];

/// A fresh synthetic `ChronoUnit` whose slot 0 is `tag`. Only `<clinit>`
/// and the no-published-constants fallbacks mint; everything else answers
/// [`chrono_unit_constant`].
pub fn chrono_unit_alloc(ctx: &mut dyn NativeContext, tag: i32) -> Result<ObjectRef, MethodCallFailed> {
    let obj = try_alloc_concurrent_synthetic(ctx, CHRONO_UNIT, 1)?;
    ctx.set_field(obj, 0, Value::Int(tag));
    Ok(obj)
}

/// The published `ChronoUnit` constant for `tag`, or a minted one when the
/// class has published none (i10-L2).
pub fn chrono_unit_constant(
    ctx: &mut dyn NativeContext,
    tag: i32,
) -> Result<ObjectRef, MethodCallFailed> {
    if let Some(Value::Object(Some(constant))) =
        crate::lang_system::canonical_enum_constant(ctx, CHRONO_UNIT, p52_chrono_unit_name(tag))
    {
        return Ok(constant);
    }
    chrono_unit_alloc(ctx, tag)
}

pub fn p52_chrono_unit_tag(name: &str) -> i32 {
    match name {
        "NANOS" => 0,
        "MICROS" => 1,
        "MILLIS" => 2,
        "SECONDS" => 3,
        "MINUTES" => 4,
        "HOURS" => 5,
        "HALF_DAYS" => 6,
        "DAYS" => 7,
        "WEEKS" => 8,
        "MONTHS" => 9,
        "YEARS" => 10,
        "DECADES" => 11,
        "CENTURIES" => 12,
        "MILLENNIA" => 13,
        "ERAS" => 14,
        "FOREVER" => 15,
        _ => -1,
    }
}

pub fn p52_chrono_unit_name(tag: i32) -> &'static str {
    match tag {
        0 => "NANOS",
        1 => "MICROS",
        2 => "MILLIS",
        3 => "SECONDS",
        4 => "MINUTES",
        5 => "HOURS",
        6 => "HALF_DAYS",
        7 => "DAYS",
        8 => "WEEKS",
        9 => "MONTHS",
        10 => "YEARS",
        11 => "DECADES",
        12 => "CENTURIES",
        13 => "MILLENNIA",
        14 => "ERAS",
        15 => "FOREVER",
        _ => "DAYS",
    }
}

// ---------------------------------------------------------------------------
// java.net.URLEncoder / URLDecoder
// ---------------------------------------------------------------------------
pub fn register_phase52_url_encoding(r: &mut NativeMethodRegistry) {
            let enc = "java/net/URLEncoder";
    let dec = "java/net/URLDecoder";
    r.register(
        enc,
        "encode",
        "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;",
        |ctx, args| {
            let input = ctx.read_string(obj_arg(args, 0)?).unwrap_or_default();
            let charset_name = match args.get(1) {
                Some(Value::Object(Some(o))) => ctx.read_string(*o).unwrap_or_default(),
                _ => String::new(),
            };
            let encoded = p52_url_encode_named(&input, &charset_name);
            let s = ctx.create_string(&encoded);
            Ok(Some(Value::Object(Some(s))))
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        enc,
        "encode",
        "(Ljava/lang/String;)Ljava/lang/String;",
        |ctx, args| {
            let input = ctx.read_string(obj_arg(args, 0)?).unwrap_or_default();
            let encoded = p52_url_encode(&input);
            let s = ctx.create_string(&encoded);
            Ok(Some(Value::Object(Some(s))))
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        dec,
        "decode",
        "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;",
        |ctx, args| {
            let input = ctx.read_string(obj_arg(args, 0)?).unwrap_or_default();
            let charset_name = match args.get(1) {
                Some(Value::Object(Some(o))) => ctx.read_string(*o).unwrap_or_default(),
                _ => String::new(),
            };
            let decoded = match p52_url_decode_named(&input, &charset_name) {
                Ok(d) => d,
                Err(message) => {
                    return Err(RuntimeError::IllegalArgumentException {
                        message: message.to_string(),                    }
                    .into())
                }
            };
            let s = ctx.create_string(&decoded);
            Ok(Some(Value::Object(Some(s))))
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        dec,
        "decode",
        "(Ljava/lang/String;)Ljava/lang/String;",
        |ctx, args| {
            let input = ctx.read_string(obj_arg(args, 0)?).unwrap_or_default();
            let decoded = match p52_url_decode(&input) {
                Ok(d) => d,
                Err(message) => {
                    return Err(RuntimeError::IllegalArgumentException {
                        message: message.to_string(),                    }
                    .into())
                }
            };
            let s = ctx.create_string(&decoded);
            Ok(Some(Value::Object(Some(s))))
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        enc,
        "encode",
        "(Ljava/lang/String;Ljava/nio/charset/Charset;)Ljava/lang/String;",
        |ctx, args| {
            let input = ctx.read_string(obj_arg(args, 0)?).unwrap_or_default();
            let charset_name = match args.get(1) {
                Some(v @ Value::Object(Some(_))) => crate::charset::charset_name_of(ctx, *v),
                _ => String::new(),
            };
            let encoded = p52_url_encode_named(&input, &charset_name);
            let s = ctx.create_string(&encoded);
            Ok(Some(Value::Object(Some(s))))
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        dec,
        "decode",
        "(Ljava/lang/String;Ljava/nio/charset/Charset;)Ljava/lang/String;",
        |ctx, args| {
            let input = ctx.read_string(obj_arg(args, 0)?).unwrap_or_default();
            let charset_name = match args.get(1) {
                Some(v @ Value::Object(Some(_))) => crate::charset::charset_name_of(ctx, *v),
                _ => String::new(),
            };
            let decoded = match p52_url_decode_named(&input, &charset_name) {
                Ok(d) => d,
                Err(message) => {
                    return Err(RuntimeError::IllegalArgumentException {
                        message: message.to_string(),                    }
                    .into())
                }
            };
            let s = ctx.create_string(&decoded);
            Ok(Some(Value::Object(Some(s))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
}

pub fn p52_url_encode(input: &str) -> String {
    let mut result = String::with_capacity(input.len() * 3);
    for b in input.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'*' => {
                result.push(b as char);
            }
            b' ' => result.push('+'),
            _ => {
                result.push('%');
                let hi = b >> 4;
                let lo = b & 0x0F;
                result.push(if hi < 10 {
                    (b'0' + hi) as char
                } else {
                    (b'A' + hi - 10) as char
                });
                result.push(if lo < 10 {
                    (b'0' + lo) as char
                } else {
                    (b'A' + lo - 10) as char
                });
            }
        }
    }
    result
}

/// `URLEncoder.encode(String, String)` / `(String, Charset)` — charset-aware
/// counterpart to `url_encode_named` in `deprecated_io_util.rs` (same defect:
/// this registration previously always percent-escaped raw UTF-8 bytes
/// regardless of the requested charset).
pub fn p52_url_encode_named(input: &str, charset_name: &str) -> String {
    let mut result = String::with_capacity(input.len() * 3);
    for ch in input.chars() {
        match ch {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '*' => {
                result.push(ch);
            }
            ' ' => result.push('+'),
            _ => {
                let mut buf = [0u8; 4];
                let s = ch.encode_utf8(&mut buf);
                for b in crate::charset::encode_str_named(charset_name, s) {
                    result.push('%');
                    result.push(if (b >> 4) < 10 {
                        (b'0' + (b >> 4)) as char
                    } else {
                        (b'A' + (b >> 4) - 10) as char
                    });
                    let lo = b & 0x0F;
                    result.push(if lo < 10 {
                        (b'0' + lo) as char
                    } else {
                        (b'A' + lo - 10) as char
                    });
                }
            }
        }
    }
    result
}

/// Percent-decode into raw bytes only, without picking a charset for the
/// bytes-to-`String` step. See `p52_url_decode_named` for the charset-aware
/// entry point that overloads accepting a charset name/`Charset` must use.
/// Real JDK's `URLDecoder.decode` throws `IllegalArgumentException` rather
/// than passing malformed escapes through.
pub fn p52_url_decode_bytes(input: &str) -> Result<Vec<u8>, &'static str> {
    let mut result = Vec::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                result.push(b' ');
                i += 1;
            }
            b'%' => {
                // "Incomplete trailing escape (%) pattern" when fewer than 2
                // chars remain, "Illegal hex characters in escape (%)
                // pattern" when they aren't valid hex digits.
                if i + 2 >= bytes.len() {
                    return Err("URLDecoder: Incomplete trailing escape (%) pattern");
                }
                let hi = p52_hex_val(bytes[i + 1]);
                let lo = p52_hex_val(bytes[i + 2]);
                match (hi, lo) {
                    (Some(h), Some(l)) => {
                        result.push(h << 4 | l);
                        i += 3;
                    }
                    _ => {
                        return Err("URLDecoder: Illegal hex characters in escape (%) pattern");
                    }
                }
            }
            b => {
                result.push(b);
                i += 1;
            }
        }
    }
    Ok(result)
}

pub fn p52_url_decode(input: &str) -> Result<String, &'static str> {
    Ok(String::from_utf8(p52_url_decode_bytes(input)?).unwrap_or_default())
}

/// `URLDecoder.decode(String, String)` / `(String, Charset)` — decode the
/// percent-escaped bytes using the requested charset rather than always
/// UTF-8 (see `deprecated_io_util::url_decode_named` for the sibling
/// registration that fixes the same gap; both must handle non-UTF-8 charsets
/// like windows-1251 correctly for
/// `ServletServerHttpRequestTests.getFormBodyWithNotUtf8Charset`).
pub fn p52_url_decode_named(input: &str, charset_name: &str) -> Result<String, &'static str> {
    Ok(crate::charset::decode_str_named(
        charset_name,
        &p52_url_decode_bytes(input)?,
    ))
}

pub fn p52_hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'A'..=b'F' => Some(b - b'A' + 10),
        b'a'..=b'f' => Some(b - b'a' + 10),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// java.net.InetSocketAddress — holder-shaped synthetic
// ---------------------------------------------------------------------------
/// `InetSocketAddress`'s `checkPort` — the JDK rejects a port outside
/// `[0, 65535]` with `IllegalArgumentException` from EVERY constructor
/// (`InetSocketAddressHolder` runs it before storing anything). Ours accepted
/// any `int` silently, so `new InetSocketAddress(-1)` produced a socket address
/// with a negative port instead of failing where the caller could see it — and
/// a bind through it then failed much later with an unrelated OS error.
pub fn p52_isa_check_port(port: i32) -> Result<i32, cratonvm_types::error::MethodCallFailed> {
    if !(0..=0xFFFF).contains(&port) {
        return Err(
            cratonvm_types::error::RuntimeError::IllegalArgumentException {
                message: format!("port out of range:{port}"),
            }
            .into(),
        );
    }
    Ok(port)
}

/// `InetSocketAddress.checkHost` — a null hostname is an
/// `IllegalArgumentException`, NOT the `NullPointerException` a native that
/// simply dereferences the argument produces.
///
/// The message is TRANSCRIBED from HotSpot 25.0.3+9, not paraphrased: it is
/// `hostname can't be null` — an apostrophe, no article, no trailing period.
///
/// **The order this runs in relative to [`p52_isa_check_port`] is different in
/// the two callers, and neither is derivable from the other.** The JDK:
///
/// ```java
/// public static InetSocketAddress createUnresolved(String host, int port) {
///     return new InetSocketAddress(checkPort(port), checkHost(host));
/// }
/// public InetSocketAddress(String hostname, int port) {
///     checkHost(hostname);
///     ...
///     holder = new InetSocketAddressHolder(host, addr, checkPort(port));
/// }
/// ```
///
/// `createUnresolved` evaluates `checkPort` FIRST because it is the first
/// *argument*; the constructor calls `checkHost` first as its first
/// *statement*. MEASURED, both wrong at once:
///
/// | call | HotSpot |
/// |---|---|
/// | `createUnresolved(null, -1)` | `IllegalArgumentException: port out of range:-1` |
/// | `new InetSocketAddress((String) null, -1)` | `IllegalArgumentException: hostname can't be null` |
///
/// Two methods that look interchangeable, opposite answers. A guard written
/// once and shared in one order gets one of these rows right and the other
/// wrong, and the wrong one is the row a reviewer would call obviously
/// equivalent. Do not "simplify" the two call sites into a common helper that
/// fixes an order.
pub fn p52_isa_check_host(host: Value) -> Result<(), MethodCallFailed> {
    if matches!(host, Value::Object(None)) {
        return Err(RuntimeError::IllegalArgumentException {
            message: "hostname can't be null".to_string(),        }
        .into());
    }
    Ok(())
}

pub fn p52_isa_set(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    host: Value,
    addr: Value,
    port: i32,
) -> Result<(), MethodCallFailed> {
    // Cross-call GC-safety (2026-08-04): `alloc_concurrent_synthetic` allocates
    // (and on a cold VM also loads + initialises the holder class), so `this`,
    // `host` and `addr` can all relocate across it. Writing them back through
    // the pre-call locals stored a VACATED from-space reference into the
    // holder's `addr` slot, which read back as **null** while `isUnresolved()`
    // still answered false — the exact pair that let `ServerSocket.bind` walk
    // past its own unresolved-address guard and NPE inside `sun.nio.ch.Net.bind`
    // (`addr.isLinkLocalAddress()`).
    let mut scope = NativeHandleScope::new(ctx);
    let this_h = scope.root(this);
    let host_h = match host {
        Value::Object(Some(o)) => Some(scope.root(o)),
        _ => None,
    };
    let addr_h = match addr {
        Value::Object(Some(o)) => Some(scope.root(o)),
        _ => None,
    };
    let holder = try_alloc_concurrent_synthetic(
        &mut *scope,
        "java/net/InetSocketAddress$InetSocketAddressHolder",
        3,
    )?;
    let holder_h = scope.root(holder);
    let host_cur = match &host_h {
        Some(h) => Value::Object(Some(scope.get(h))),
        None => host,
    };
    let addr_cur = match &addr_h {
        Some(h) => Value::Object(Some(scope.get(h))),
        None => addr,
    };
    let holder_cur = scope.get(&holder_h);
    scope.set_field(holder_cur, 0, host_cur);
    scope.set_field(holder_cur, 1, addr_cur);
    scope.set_field(holder_cur, 2, Value::Int(port));
    let this_cur = scope.get(&this_h);
    scope.set_field(this_cur, 0, Value::Object(Some(holder_cur)));
    scope.set_field(this_cur, 1, Value::Int(port));
    scope.set_field(this_cur, 2, addr_cur);
    Ok(())
}

pub fn p52_isa_host_from_addr(ctx: &mut dyn NativeContext, addr: ObjectRef) -> Value {
    if let Some((host, ip)) = crate::net_phase_e::inet_addr_resolve(ctx, addr) {
        let host = if !host.is_empty() {
            host
        } else if !ip.is_empty() {
            ip
        } else {
            "0.0.0.0".to_string()
        };
        return Value::Object(Some(ctx.create_string(&host)));
    }
    Value::Object(None)
}

/// The numeric address text of an `InetAddress`, which is what
/// `InetSocketAddress.equals`/`hashCode` must key on — `InetAddress.equals` is
/// defined purely over the address bytes, so two mirrors of `127.0.0.1` are
/// equal no matter what hostName either of them remembers.
///
/// Falls back to the empty string for anything this family did not build; two
/// such objects then compare equal on address, which is the same answer the
/// old host-text comparison gave them.
pub fn p52_isa_addr_ip(ctx: &mut dyn NativeContext, addr: ObjectRef) -> String {
    match crate::net_phase_e::inet_addr_resolve_external(ctx, addr) {
        Some((_, ip)) => ip,
        None => String::new(),
    }
}

/// The JDK 25 `InetAddress.hashCode()` of the address whose numeric text is
/// `ip` (JIT round 20 wave 19, lane yHttp): `Inet4Address.hashCode()` is the
/// address as a big-endian `int`; `Inet6Address$Inet6AddressHolder.hashCode()`
/// sums the four 32-bit groups, each built from the SIGNED bytes
/// (`(component << 8) + ipaddress[i]`). A `%scope` suffix or brackets do not
/// take part (the holder hashes the 16 bytes alone). Text that is no address
/// hashes as 0, the JDK's answer for a holder without bytes.
pub fn p52_jdk_inet_address_hash(ip: &str) -> i32 {
    let bare = ip.split('%').next().unwrap_or(ip);
    let bare = bare
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(bare);
    match bare.parse::<std::net::IpAddr>() {
        // Cast: the JDK's `int` is the same 32 bits.
        Ok(std::net::IpAddr::V4(v4)) => u32::from(v4) as i32,
        Ok(std::net::IpAddr::V6(v6)) => {
            let mut hash: i32 = 0;
            for group in v6.octets().chunks(4) {
                let mut component: i32 = 0;
                for &b in group {
                    // Cast: `ipaddress[i]` is a Java `byte`, sign-extended.
                    component = component.wrapping_shl(8).wrapping_add(i32::from(b as i8));
                }
                hash = hash.wrapping_add(component);
            }
            hash
        }
        Err(_) => 0,
    }
}

/// `java.lang.String.hashCode()` of `s`: `31 * h + c` over its UTF-16 code
/// units, wrapping.
pub fn p52_java_string_hash(s: &str) -> i32 {
    s.encode_utf16().fold(0i32, |h, unit| {
        h.wrapping_mul(31).wrapping_add(i32::from(unit))
    })
}


pub fn p52_is_isa_holder(ctx: &dyn NativeContext, obj: ObjectRef) -> bool {
    let class_id = ctx.class_id_of_object(obj);
    matches!(
        ctx.class_name_arc_of_id(class_id).as_deref(),
        Some("java/net/InetSocketAddress$InetSocketAddressHolder")
    )
}

pub fn p52_isa_host_value(ctx: &mut dyn NativeContext, this: ObjectRef) -> Value {
    match ctx.get_field(this, 0) {
        Value::Object(Some(holder)) => {
            if p52_is_isa_holder(ctx, holder) {
                if let Value::Object(Some(host)) = ctx.get_field(holder, 0) {
                    if ctx.read_string(host).is_some() {
                        return Value::Object(Some(host));
                    }
                }
                if let Value::Object(Some(addr)) = ctx.get_field(holder, 1) {
                    let from_addr = p52_isa_host_from_addr(ctx, addr);
                    if !matches!(from_addr, Value::Object(None)) {
                        return from_addr;
                    }
                }
            } else if ctx.read_string(holder).is_some() {
                return Value::Object(Some(holder));
            }
        }
        Value::Object(None) => {}
        // A PRIMITIVE in slot 0. This native is registered as
        // `()Ljava/lang/String;`, so returning it hands bytecode about to
        // `areturn`/`checkcast` a String an `Int` — unsound, and the same
        // descriptor hazard `lang_misc::enum_constant_name` documents. A
        // zeroed or out-of-range slot decodes as `Value::Int(0)`, so this arm
        // is reachable on any receiver this family did not construct.
        _ => {}
    }
    Value::Object(Some(ctx.create_string("0.0.0.0")))
}

pub fn p52_isa_port_value(ctx: &mut dyn NativeContext, this: ObjectRef) -> Value {
    // Same bound as `p52_isa_addr_value`: a real-layout `InetSocketAddress` has
    // ONE field, so the legacy slot-1 fallback is an out-of-range read there.
    // Reading it back as the port produced a plausible-looking small integer
    // from whatever the zeroed slot decodes to.
    let legacy_port = |ctx: &mut dyn NativeContext| {
        if ctx.object_num_fields(this) > 1 {
            match ctx.get_field(this, 1) {
                v @ Value::Int(_) | v @ Value::Long(_) => v,
                _ => Value::Int(0),
            }
        } else {
            Value::Int(0)
        }
    };
    match ctx.get_field(this, 0) {
        Value::Object(Some(holder)) if p52_is_isa_holder(ctx, holder) => {
            match ctx.get_field(holder, 2) {
                v @ Value::Int(_) | v @ Value::Long(_) => v,
                _ => legacy_port(ctx),
            }
        }
        _ => legacy_port(ctx),
    }
}

/// The `addr` an `InetSocketAddress` carries, or `Object(None)`.
///
/// The legacy-layout fallback (slot 2) must never manufacture a NON-null answer
/// out of a slot that cannot hold an address. A real-layout `InetSocketAddress`
/// declares exactly ONE instance field (`holder`), so slot 2 is out of range,
/// and an out-of-range/zeroed read decodes as `Value::Int(0)` — which is not
/// `Object(None)`, so `isUnresolved()` answered **false** while `getAddress()`
/// answered **null**. `ServerSocket.bind` tests exactly that pair:
///
/// ```java
/// if (epoint.isUnresolved()) throw new SocketException("Unresolved address");
/// impl.bind(epoint.getAddress(), epoint.getPort());
/// ```
///
/// so the disagreement is what let a null address past the guard and two frames
/// on into `sun.nio.ch.Net.bind`'s `addr.isLinkLocalAddress()`. Any non-reference
/// result now reads as unresolved, which is both truthful and a far better
/// diagnostic (`SocketException` at the guard, not an NPE inside the JDK).
pub fn p52_isa_addr_value(ctx: &mut dyn NativeContext, this: ObjectRef) -> Value {
    match ctx.get_field(this, 0) {
        Value::Object(Some(holder)) if p52_is_isa_holder(ctx, holder) => {
            match ctx.get_field(holder, 1) {
                v @ Value::Object(_) => v,
                _ => Value::Object(None),
            }
        }
        _ if ctx.object_num_fields(this) > 2 => match ctx.get_field(this, 2) {
            v @ Value::Object(_) => v,
            _ => Value::Object(None),
        },
        _ => Value::Object(None),
    }
}

pub fn register_phase52_inet_socket_address(r: &mut NativeMethodRegistry) {
            let isa = "java/net/InetSocketAddress";
    r.register(isa, "<init>", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let port = p52_isa_check_port(args[1].as_int().unwrap_or(0))?;
        // `new InetSocketAddress(port)` delegates to the `(InetAddress, int)`
        // constructor with a null address. The JDK substitutes the resolved
        // wildcard address rather than creating an unresolved socket address.
        // Keeping this null made `getAddress()` return null and caused
        // RecordableServerHttpRequestTests.getRemoteAddress() to NPE.
        //
        // Cross-call GC-safety: `alloc_inet_address_external` and
        // `p52_isa_host_from_addr` both allocate — on a cold VM the first also
        // initialises the whole `java.net.InetAddress` hierarchy — so `this`
        // and `addr` are rooted across them. Without this the FIRST
        // `new InetSocketAddress(0)` of a GC-stressed run answered
        // `getAddress() == null`, which is how `new ServerSocket(0)` reached
        // `Net.bind` with a null address.
        let mut scope = NativeHandleScope::new(ctx);
        let this_h = scope.root(this);
        let addr =
            crate::net_phase_e::alloc_inet_address_external(&mut *scope, "0.0.0.0", "0.0.0.0")?;
        let addr_h = scope.root(addr);
        let addr_cur = scope.get(&addr_h);
        let host = p52_isa_host_from_addr(&mut *scope, addr_cur);
        let this_cur = scope.get(&this_h);
        let addr_cur = scope.get(&addr_h);
        p52_isa_set(
            &mut *scope,
            this_cur,
            host,
            Value::Object(Some(addr_cur)),
            port,
        )?;
        Ok(Some(Value::Object(None)))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(isa, "<init>", "(Ljava/lang/String;I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // HOST FIRST, then port — see [`p52_isa_check_host`]. This is the
        // opposite order from `createUnresolved` below, and it is measured, not
        // assumed: `new InetSocketAddress((String) null, -1)` reports the
        // hostname on HotSpot while `createUnresolved(null, -1)` reports the
        // port. `obj_arg(args, 1)?` used to run here and raised
        // `NullPointerException: null object argument` for both.
        let host_val = args.get(1).copied().unwrap_or(Value::Object(None));
        p52_isa_check_host(host_val)?;
        let host = obj_arg(args, 1)?;
        let port = p52_isa_check_port(args[2].as_int().unwrap_or(0))?;
        // Real JDK resolves the hostname via `InetAddress.getByName(host)`
        // here, falling back to an unresolved address only on
        // UnknownHostException. Leaving `addr` unconditionally null made
        // EVERY `new InetSocketAddress(String,int)` permanently unresolved
        // (`getAddress()` always null, `isUnresolved()` always true), even
        // for trivially-resolvable literals like "127.0.0.1" — this broke
        // `sun.nio.ch.SocketAdaptor.getInetAddress()` (its private
        // `remoteAddress()` builds one of these from the channel's peer IP
        // string), which in turn NPE'd `RemoteCIDRValve.isAllowed` on any
        // Tomcat context configured with that valve (e.g. the examples
        // webapp's `META-INF/context.xml`).
        // Cross-call GC-safety: `alloc_inet_address_external` allocates, so
        // `this` and the host String are rooted across it.
        let mut scope = NativeHandleScope::new(ctx);
        let this_h = scope.root(this);
        let host_h = scope.root(host);
        let host_str = scope.read_string(host).unwrap_or_default();
        // The ADDRESS remembers a name only when one was supplied: HotSpot's
        // `new InetSocketAddress("127.0.0.1", p).toString()` is `/127.0.0.1:p`
        // (the address it resolved has no hostName), while `("localhost", p)`
        // is `localhost/127.0.0.1:p`. The outer `InetSocketAddress`'s own
        // `hostname` slot is left as the caller's text either way — every
        // observable of it (`getHostName`/`getHostString`) agrees with HotSpot
        // for both spellings, since HotSpot answers those from the address when
        // its own field is null.
        let addr = match crate::net_phase_e::resolve_host_external(&host_str) {
            Some(ip) => Value::Object(Some(crate::net_phase_e::alloc_inet_address_for_input(
                &mut *scope,
                &host_str,
                &ip,
            )?)),
            // A bracketed or scoped IPv6 literal (`[fe80::1%ethernet_32771]`,
            // exactly what `URL.getHost()` hands `HttpClient.openServer` for a
            // link-local address) is not something the native resolver above
            // parses, so the address used to come back permanently unresolved
            // and `Socket.connect` threw `UnknownHostException`
            // (`TestStartupIPv6Connectors.testIPv6LinkLocal`). The JDK resolves
            // these in `InetAddress.getByName` (bracket stripping, the `%scope`
            // by interface name or number); ask it, and stay unresolved only
            // when it also throws, exactly as the real constructor does.
            None if host_str.contains('%') || host_str.starts_with('[') => {
                let host_cur = scope.get(&host_h);
                match (&mut *scope).invoke(
                    "java/net/InetAddress",
                    "getByName",
                    "(Ljava/lang/String;)Ljava/net/InetAddress;",
                    &[Value::Object(Some(host_cur))],
                ) {
                    Ok(Some(Value::Object(Some(resolved)))) => Value::Object(Some(resolved)),
                    _ => Value::Object(None),                }            }
            None => Value::Object(None),
        };
        let this_cur = scope.get(&this_h);
        let host_cur = scope.get(&host_h);
        p52_isa_set(
            &mut *scope,
            this_cur,
            Value::Object(Some(host_cur)),
            addr,
            port,
        )?;
        Ok(Some(Value::Object(None)))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(isa, "<init>", "(Ljava/net/InetAddress;I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // JDK InetSocketAddress(InetAddress, int) explicitly accepts null and
        // substitutes InetAddress.anyLocalAddress(). Tomcat's NioEndpoint
        // relies on that for an unspecified bind address; rejecting it here
        // leaves the connector FAILED before it can allocate an ephemeral port.
        //
        // The port is validated BEFORE the wildcard is materialised: the JDK
        // runs `checkPort` inside the holder constructor, so an out-of-range
        // port throws `IllegalArgumentException` and no address is resolved.
        let port = p52_isa_check_port(args[2].as_int().unwrap_or(0))?;
        // Cross-call GC-safety: the wildcard branch allocates (and initialises
        // the InetAddress hierarchy on a cold VM), so `this` is rooted across
        // it. This constructor is the one `ServerSocket(int, int, InetAddress)`
        // uses with a null `bindAddr`, and it is where `new ServerSocket(0)`
        // acquired the null address that NPE'd `sun.nio.ch.Net.bind`.
        let mut scope = NativeHandleScope::new(ctx);
        let this_h = scope.root(this);
        let addr = match args.get(1) {
            Some(Value::Object(Some(addr))) => *addr,
            _ => {
                crate::net_phase_e::alloc_inet_address_external(&mut *scope, "0.0.0.0", "0.0.0.0")?
            }
        };
        let addr_h = scope.root(addr);
        let addr_cur = scope.get(&addr_h);
        let host_val = p52_isa_host_from_addr(&mut *scope, addr_cur);
        let this_cur = scope.get(&this_h);
        let addr_cur = scope.get(&addr_h);
        p52_isa_set(
            &mut *scope,
            this_cur,
            host_val,
            Value::Object(Some(addr_cur)),
            port,
        )?;
        Ok(Some(Value::Object(None)))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(
        isa,
        "createUnresolved",
        "(Ljava/lang/String;I)Ljava/net/InetSocketAddress;",
        |ctx, args| {
            // PORT FIRST, then host — the reverse of the `(String,I)V`
            // constructor above, because `createUnresolved` is
            // `new InetSocketAddress(checkPort(port), checkHost(host))` and
            // `checkPort` is the first argument evaluated. MEASURED:
            // `createUnresolved(null, -1)` is `port out of range:-1`, not
            // `hostname can't be null`. See [`p52_isa_check_host`].
            let port = p52_isa_check_port(args[1].as_int().unwrap_or(0))?;
            p52_isa_check_host(args.first().copied().unwrap_or(Value::Object(None)))?;
            let host = obj_arg(args, 0)?;
            // Cross-call GC-safety: the allocation below can move `host`.
            let mut scope = NativeHandleScope::new(ctx);
            let host_h = scope.root(host);
            let obj = try_alloc_concurrent_synthetic(&mut *scope, "java/net/InetSocketAddress", 3)?;
            let obj_h = scope.root(obj);
            let obj_cur = scope.get(&obj_h);
            let host_cur = scope.get(&host_h);
            p52_isa_set(
                &mut *scope,
                obj_cur,
                Value::Object(Some(host_cur)),
                Value::Object(None),
                port,
            )?;
            Ok(Some(Value::Object(Some(scope.get(&obj_h)))))
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(isa, "getHostName", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(p52_isa_host_value(ctx, this)))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(isa, "getHostString", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(p52_isa_host_value(ctx, this)))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(isa, "getPort", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(p52_isa_port_value(ctx, this)))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(
        isa,
        "getAddress",
        "()Ljava/net/InetAddress;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(p52_isa_addr_value(ctx, this)))
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(isa, "isUnresolved", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let addr = p52_isa_addr_value(ctx, this);
        Ok(Some(Value::Int(if matches!(addr, Value::Object(None)) {
            1
        } else {
            0
        })))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(isa, "toString", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let host_str = if let Value::Object(Some(h)) = p52_isa_host_value(ctx, this) {
            ctx.read_string(h).unwrap_or_else(|| "0.0.0.0".to_string())
        } else {
            "0.0.0.0".to_string()
        };
        let port = p52_isa_port_value(ctx, this).as_int().unwrap_or(0);
        // HotSpot renders `InetSocketAddressHolder.toString()`:
        //   unresolved → `hostname + "/<unresolved>"`
        //   resolved   → `addr.toString()`, i.e. `hostName + "/" + ip`, with an
        //                Inet6Address's numeric part bracketed
        // and then appends `":" + port`. We used to emit a bare `host:port`,
        // which loses the `/`-separated address entirely and — worse — makes an
        // UNRESOLVED address print exactly like a resolved one, so a log line
        // could not distinguish `createUnresolved("h", 80)` from a real
        // endpoint. Reference: `probes/ServerSocketNullInetAddressProbe.java`,
        // diffed against HotSpot 25.0.3+9.
        let rendered = match p52_isa_addr_value(ctx, this) {
            Value::Object(Some(addr)) => {
                let (h, ip) = crate::net_phase_e::inet_addr_resolve_external(ctx, addr)
                    .unwrap_or_else(|| (host_str.clone(), host_str.clone()));
                let ip_part = if ip.contains(':') {
                    format!("[{ip}]")
                } else {
                    ip
                };
                format!("{h}/{ip_part}")
            }
            _ => format!("{host_str}/<unresolved>"),
        };
        let s = ctx.create_string(&format!("{rendered}:{port}"));
        Ok(Some(Value::Object(Some(s))))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(isa, "equals", "(Ljava/lang/Object;)Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if let Value::Object(Some(other)) = &args[1] {
            let p1 = p52_isa_port_value(ctx, this).as_int().unwrap_or(-1);
            let p2 = p52_isa_port_value(ctx, *other).as_int().unwrap_or(-2);
            if p1 != p2 {
                return Ok(Some(Value::Int(0)));
            }
            // The JDK compares the ADDRESS when both sides have one, and only
            // falls back to the hostname for UNRESOLVED addresses:
            //
            //   if (addr != null)      return addr.equals(that.addr);
            //   else if (hostname != null)
            //                          return that.addr == null
            //                                 && hostname.equalsIgnoreCase(that.hostname);
            //   else                   return that.addr == null && that.hostname == null;
            //
            // Comparing the host TEXT instead made
            // `new InetSocketAddress(getByName("127.0.0.1"), p)` unequal to
            // `new InetSocketAddress(getByName("localhost"), p)` — the same
            // endpoint, spelled two ways — so any `Set<SocketAddress>` or
            // `Map<SocketAddress, …>` keyed by peer identity could hold both.
            // The hostname fallback is `equalsIgnoreCase`, because DNS names
            // are case-insensitive.
            let a1 = p52_isa_addr_value(ctx, this);
            let a2 = p52_isa_addr_value(ctx, *other);
            let eq = match (a1, a2) {
                (Value::Object(Some(x)), Value::Object(Some(y))) => {
                    p52_isa_addr_ip(ctx, x) == p52_isa_addr_ip(ctx, y)
                }
                // Exactly one side resolved: the JDK's `addr.equals(null)` is
                // false, and its unresolved branch demands `that.addr == null`.
                (Value::Object(Some(_)), _) | (_, Value::Object(Some(_))) => false,
                // Both unresolved — compare the hostnames case-insensitively.
                _ => match (
                    p52_isa_host_value(ctx, this),
                    p52_isa_host_value(ctx, *other),
                ) {
                    (Value::Object(Some(a)), Value::Object(Some(b))) => ctx
                        .read_string(a)
                        .unwrap_or_default()
                        .eq_ignore_ascii_case(&ctx.read_string(b).unwrap_or_default()),
                    _ => false,
                },
            };
            Ok(Some(Value::Int(if eq { 1 } else { 0 })))
        } else {
            Ok(Some(Value::Int(0)))
        }
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(isa, "hashCode", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let port = p52_isa_port_value(ctx, this).as_int().unwrap_or(0);
        // Must agree with `equals` directly above: it compares the ADDRESS when
        // one exists, so hashing the host TEXT would give two equal endpoints
        // (`127.0.0.1` and `localhost`) different hashes — the classic broken
        // pair that loses entries in a HashMap. Hash the same thing `equals`
        // compares: the numeric address, falling back to the hostname only for
        // the unresolved case, lower-cased because that comparison is
        // case-insensitive.
        //
        // JIT round 20 wave 19 (lane yHttp): and hash it with the JDK's own
        // formula, `InetSocketAddressHolder.hashCode()` -- `addr.hashCode() +
        // port`, else `hostname.toLowerCase(Locale.ROOT).hashCode() + port` --
        // not a hash of the address TEXT xor the port. The value is
        // Java-visible (`hashCode()` itself, and the iteration order of every
        // `HashMap` / `HashSet` keyed by socket addresses): HotSpot answers
        // `2130714513` for `new InetSocketAddress("127.0.0.1", 8080)`, this
        // native answered the text hash of `"127.0.0.1"` xor 8080. Correctness
        // fix, unconditional (`--compatible`; under `--real-jdk` the JDK's body
        // runs, see `CRATONVM_JDK_INET_SOCKET_ADDRESS_BODIES`).
        let hash = match p52_isa_addr_value(ctx, this) {
            Value::Object(Some(addr)) => p52_jdk_inet_address_hash(&p52_isa_addr_ip(ctx, addr)),
            _ => match p52_isa_host_value(ctx, this) {
                Value::Object(Some(s)) => {
                    p52_java_string_hash(&ctx.read_string(s).unwrap_or_default().to_lowercase())
                }
                _ => 0,
            },
        };
        Ok(Some(Value::Int(hash.wrapping_add(port))))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(
        "java/net/SocketAddress",
        "toString",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let nf = ctx.object_num_fields(this);
            if nf >= 3 {
                let host_str = if let Value::Object(Some(h)) = p52_isa_host_value(ctx, this) {
                    ctx.read_string(h).unwrap_or_else(|| "0.0.0.0".to_string())
                } else {
                    "0.0.0.0".to_string()
                };
                let port = p52_isa_port_value(ctx, this).as_int().unwrap_or(0);
                let s = ctx.create_string(&format!("{host_str}:{port}"));
                Ok(Some(Value::Object(Some(s))))
            } else {
                let s = ctx.create_string("SocketAddress");
                Ok(Some(Value::Object(Some(s))))
            }
        }, cratonvm_native_api::NativeKind::Bridge,
    );
}

pub fn phase52_alloc_socket(ctx: &mut dyn NativeContext) -> Result<ObjectRef, MethodCallFailed> {
    let sock = try_alloc_concurrent_synthetic(ctx, "java/net/Socket", 5)?;
    // FIX (jndirealmintegration-ldap-connection-npe residual): field index 0
    // (SOCK_HOST in this synthetic-mode convention) lands on the REAL
    // `java.net.Socket.impl` field whenever the real class is loaded (see
    // `alloc_concurrent_synthetic`'s doc comment -- it reuses the real class
    // identity/layout when loadable). Writing a non-null empty String there
    // (the old behavior) left `impl` non-null-but-wrong-typed, so real
    // `Socket.getImpl()` bytecode skipped `createImpl()` and virtual-dispatched
    // `create(Z)V` on a `java.lang.String` receiver -- `NoSuchMethodError:
    // java/lang/String.create(Z)V`. Write `null` instead (matching
    // net_phase_e.rs's `register_re1_socket`'s own `<init>()V`), which keeps
    // `impl` unset so real bytecode's lazy `createImpl()` runs correctly.
    ctx.set_field(sock, SOCK_HOST, Value::Object(None));
    ctx.set_field(sock, SOCK_PORT, Value::Int(0));
    ctx.set_field(sock, SOCK_LOCAL_PORT, Value::Int(0));
    ctx.set_field(sock, SOCK_CLOSED, Value::Int(0));
    ctx.set_field(sock, SOCK_STREAM_ID, Value::Int(-1));
    // This allocation skips real `java.net.Socket.<init>`, which is where
    // `socketLock`/`closeLock` (`final Object` instance-initializer fields)
    // normally get set. Any later real-bytecode `Socket` method that does
    // `synchronized (socketLock)` -- e.g. `getImpl()`, reached from
    // `connect()`/`close()`/etc. -- throws "Cannot enter synchronized block
    // because ... socketLock is null" otherwise. Hit via
    // `javax.net.SocketFactory.createSocket()` (this native, used
    // unconditionally regardless of CRATONVM_REAL_NET_SOCKETS) feeding a real
    // `Socket.connect()` call, e.g. UnboundID LDAP SDK's `ConnectThread`
    // (see jndirealmintegration-ldap-connection-npe-FIXED.md). Reuse the same
    // GC-safe helper `net_phase_e::re1_init_socket_locks` already used by the
    // synthetic `Socket.<init>`/`ServerSocket.accept()` paths, rather than
    // duplicating its pin-across-allocation logic here.
    Ok(crate::net_phase_e::re1_init_socket_locks(ctx, sock)?)
}

pub fn phase52_socket_connect(
    ctx: &mut dyn NativeContext,
    host_value: Value,
    port_value: Value,
) -> MethodCallResult {
    // gc-common w20-b: the host String (an argument, or the answer of
    // `InetAddress.getHostAddress()`) is read before the socket's allocation;
    // it was read after it, at its pre-allocation address.
    let host = match host_value {
        Value::Object(Some(s)) => ctx
            .read_string(s)
            .unwrap_or_else(|| "127.0.0.1".to_string()),
        _ => "127.0.0.1".to_string(),
    };
    let port = port_value.as_int().unwrap_or(0);
    let sock = phase52_alloc_socket(ctx)?;
    // gc-common w16-a handoff: pinned across `create_string` below and
    // re-read, so the side-table row records the socket's CURRENT address.
    // Left pinned on return, as `net_phase_e::re2_accept_into` leaves its
    // socket: the native return truncates the pin stack after publishing the
    // result.
    let sock_pin = ctx.pin_native_root(sock);
    let host_obj = ctx.create_string(&host);
    let sock = ctx.read_native_pin(sock_pin, sock);
    ctx.set_field(sock, SOCK_HOST, Value::Object(Some(host_obj)));
    ctx.set_field(sock, SOCK_PORT, Value::Int(port));

    match std::net::TcpStream::connect(format!("{host}:{port}")) {
        Ok(stream) => {
            let local_port = stream.local_addr().map(|a| a.port() as i32).unwrap_or(0);
            let id = crate::servlet::s2_alloc_stream_for_vm(ctx.vm_identity(), stream);
            ctx.set_field(sock, SOCK_STREAM_ID, Value::Int(id));
            ctx.set_field(sock, SOCK_LOCAL_PORT, Value::Int(local_port));
            // FIX (SocketFactory.createSocket "not connected" split-brain):
            // `Socket.getOutputStream`/`getInputStream`/`isConnected` (registered
            // in net_phase_e.rs) read connect state from an identity-keyed side
            // table, not these raw fields (see net_phase_e.rs's SockSide doc
            // comment) -- the same collision `sock_set_for_create` was added to
            // fix for `SSLSocketFactory.createSocket`. This factory path never
            // populated that side table, so a socket obtained via
            // `SocketFactory.getDefault().createSocket(host, port)` connected
            // for real but every later side-table-backed accessor saw the
            // default stream_id=-1 ("not connected"). Populate it here too.
            //
            // gc-common w16-a: the owning variant, so a socket dropped
            // without `close()` has this stream closed by the owner-death
            // sweep, as HotSpot's `SocketCleanable` closes it.
            crate::net_phase_e::sock_set_for_create_owning_stream(ctx, sock, port, local_port, id);
            Ok(Some(Value::Object(Some(sock))))
        }
        Err(e) => Err(RuntimeError::IOException {
            message: format!("SocketFactory.createSocket failed: {host}:{port}: {e}"),
        }
        .into()),
    }
}

pub fn register_phase52_server_socket_factory(r: &mut NativeMethodRegistry) {
            let sf = "javax/net/SocketFactory";
    r.register(
        sf,
        "getDefault",
        "()Ljavax/net/SocketFactory;",
        |ctx, _args| {
            let obj = try_alloc_concurrent_synthetic(ctx, "javax/net/SocketFactory", 0)?;
            Ok(Some(Value::Object(Some(obj))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(sf, "createSocket", "()Ljava/net/Socket;", |ctx, args| {
        crate::tls_deny::deny_plaintext_fallback(
            ctx,
            args,
            crate::tls_deny::TlsFactoryKind::Socket,
            "createSocket",
            "()Ljava/net/Socket;",
        )?;
        Ok(Some(Value::Object(Some(phase52_alloc_socket(ctx)?))))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(
        sf,
        "createSocket",
        "(Ljava/lang/String;I)Ljava/net/Socket;",
        |ctx, args| {
            crate::tls_deny::deny_plaintext_fallback(
                ctx,
                args,
                crate::tls_deny::TlsFactoryKind::Socket,
                "createSocket",
                "(Ljava/lang/String;I)Ljava/net/Socket;",
            )?;
            let host = args.get(1).copied().unwrap_or(Value::Object(None));
            let port = args.get(2).copied().unwrap_or(Value::Int(0));
            phase52_socket_connect(ctx, host, port)
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        sf,
        "createSocket",
        "(Ljava/net/InetAddress;I)Ljava/net/Socket;",
        |ctx, args| {
            crate::tls_deny::deny_plaintext_fallback(
                ctx,
                args,
                crate::tls_deny::TlsFactoryKind::Socket,
                "createSocket",
                "(Ljava/net/InetAddress;I)Ljava/net/Socket;",
            )?;
            let host = match args.get(1).copied().unwrap_or(Value::Object(None)) {
                Value::Object(Some(addr)) => ctx
                    .invoke_virtual(addr, "getHostAddress", "()Ljava/lang/String;", &[])
                    .ok()
                    .flatten()
                    .unwrap_or(Value::Object(None)),
                _ => Value::Object(None),
            };
            let port = args.get(2).copied().unwrap_or(Value::Int(0));
            phase52_socket_connect(ctx, host, port)
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        sf,
        "createSocket",
        "(Ljava/lang/String;ILjava/net/InetAddress;I)Ljava/net/Socket;",
        |ctx, args| {
            crate::tls_deny::deny_plaintext_fallback(
                ctx,
                args,
                crate::tls_deny::TlsFactoryKind::Socket,
                "createSocket",
                "(Ljava/lang/String;ILjava/net/InetAddress;I)Ljava/net/Socket;",
            )?;
            let host = args.get(1).copied().unwrap_or(Value::Object(None));
            let port = args.get(2).copied().unwrap_or(Value::Int(0));
            phase52_socket_connect(ctx, host, port)
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        sf,
        "createSocket",
        "(Ljava/net/InetAddress;ILjava/net/InetAddress;I)Ljava/net/Socket;",
        |ctx, args| {
            crate::tls_deny::deny_plaintext_fallback(
                ctx,
                args,
                crate::tls_deny::TlsFactoryKind::Socket,
                "createSocket",
                "(Ljava/net/InetAddress;ILjava/net/InetAddress;I)Ljava/net/Socket;",
            )?;
            let host = match args.get(1).copied().unwrap_or(Value::Object(None)) {
                Value::Object(Some(addr)) => ctx
                    .invoke_virtual(addr, "getHostAddress", "()Ljava/lang/String;", &[])
                    .ok()
                    .flatten()
                    .unwrap_or(Value::Object(None)),
                _ => Value::Object(None),
            };
            let port = args.get(2).copied().unwrap_or(Value::Int(0));
            phase52_socket_connect(ctx, host, port)
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    let sock = "java/net/Socket";
    r.register(
        sock,
        "getRemoteSocketAddress",
        "()Ljava/net/SocketAddress;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // A `ServerSocket.accept()`- or `Socket.connect()`-produced
            // Socket never writes the raw SOCK_HOST/SOCK_PORT fields (a
            // host String there would land on real JDK's `Socket.impl`
            // slot, corrupting it) -- the real peer address is side-tabled
            // instead (see net_phase_e::SockSide). Falling straight to
            // InetSocketAddress(String,int) with the raw (null) field threw
            // an NPE for every connected/accepted Socket (e.g. MockWebServer
            // MockWebServer's `serveConnection`, breaking every test that
            // logs/uses the peer address -- RestClientIntegrationTests et
            // al). Fall back to the side table when the raw field is unset.
            let side = crate::net_phase_e::sock_get(ctx, this);
            // gc-common w20-b: the port is read before the host string is
            // allocated (it read `this` after that allocation).
            let port = match ctx.get_field(this, SOCK_PORT) {
                p @ Value::Int(v) if v != 0 => p,
                _ => Value::Int(side.port),
            };
            let host = match ctx.get_field(this, SOCK_HOST) {
                h @ Value::Object(Some(_)) => h,
                _ if !side.host.is_empty() => Value::Object(Some(ctx.create_string(&side.host))),
                _ => Value::Object(Some(ctx.create_string("0.0.0.0"))),
            };
            // `host` may be a freshly-allocated string (the `create_string`
            // arms above) with no root holding it other than this local --
            // `new_object_initialized` runs the real InetSocketAddress
            // constructor bytecode (which calls InetAddress.getByName() and
            // can allocate/GC), so pin it across that call; otherwise a GC
            // in that window can reclaim/move it out from under the pending
            // arg, surfacing as `obj_arg` seeing a null host inside the ctor.
            let host_obj = match host {
                Value::Object(Some(o)) => o,
                _ => unreachable!("host is always Object(Some(_)) from the match above"),
            };
            let host_pin = ctx.pin_native_root(host_obj);
            let host = Value::Object(Some(ctx.read_native_pin(host_pin, host_obj)));
            let result = ctx.new_object_initialized(
                "java/net/InetSocketAddress",
                "(Ljava/lang/String;I)V",
                &[host, port],
            );
            ctx.unpin_native_roots(host_pin);
            result
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        sock,
        "getLocalSocketAddress",
        "()Ljava/net/SocketAddress;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // gc-common w20-b: the port is read before the host string is
            // allocated (it read `this` after that allocation).
            let port = ctx.get_field(this, SOCK_LOCAL_PORT);
            let host_obj = ctx.create_string("127.0.0.1");
            let host_pin = ctx.pin_native_root(host_obj);
            let host_obj = ctx.read_native_pin(host_pin, host_obj);
            let host = Value::Object(Some(host_obj));
            let result = ctx.new_object_initialized(
                "java/net/InetSocketAddress",
                "(Ljava/lang/String;I)V",
                &[host, port],
            );
            ctx.unpin_native_roots(host_pin);
            result
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    let ssf = "javax/net/ServerSocketFactory";
    r.register(
        ssf,
        "getDefault",
        "()Ljavax/net/ServerSocketFactory;",
        |ctx, _args| {
            // The class name is observable: on HotSpot
            // `ServerSocketFactory.getDefault().getClass().getName()` is
            // `javax.net.DefaultServerSocketFactory`, and this answered the
            // ABSTRACT base `javax.net.ServerSocketFactory` — a class the JDK
            // never instantiates. MEASURED, `L6TlsParamSweep` row 74.
            //
            // Minting the concrete class is safe because
            // `register_plain_server_socket_factory` below registers the four
            // `createServerSocket` bodies on it as well, so no JDK bytecode
            // runs against a carrier whose `impl` was never constructed. That
            // is also what keeps `tls_deny::deny_plaintext_fallback` in the
            // path: `DefaultServerSocketFactory`'s own bytecode would build
            // the same plain `java.net.ServerSocket` without consulting the
            // guard at all.
            let obj =
                try_alloc_concurrent_synthetic(ctx, "javax/net/DefaultServerSocketFactory", 0)?;
            Ok(Some(Value::Object(Some(obj))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // The abstract base, for a caller that holds one another way, and the
    // concrete default the getter above hands out.
    register_plain_server_socket_factory(r, ssf);
    register_plain_server_socket_factory(r, "javax/net/DefaultServerSocketFactory");
    }

/// The four `createServerSocket` overloads of a PLAINTEXT server-socket
/// factory, registered on one class.
///
/// Two classes get them: `javax.net.ServerSocketFactory` and the concrete
/// `javax.net.DefaultServerSocketFactory` its `getDefault()` returns. See that
/// getter for why the concrete class is what is minted.
pub fn register_plain_server_socket_factory(r: &mut NativeMethodRegistry, ssf: &str) {
    r.register(
        ssf,
        "createServerSocket",
        "()Ljava/net/ServerSocket;",
        |ctx, args| {
            crate::tls_deny::deny_plaintext_fallback(
                ctx,
                args,
                crate::tls_deny::TlsFactoryKind::ServerSocket,
                "createServerSocket",
                "()Ljava/net/ServerSocket;",
            )?;
            ctx.new_object_initialized("java/net/ServerSocket", "()V", &[])
        }, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        ssf,
        "createServerSocket",
        "(I)Ljava/net/ServerSocket;",
        |ctx, args| {
            crate::tls_deny::deny_plaintext_fallback(
                ctx,
                args,
                crate::tls_deny::TlsFactoryKind::ServerSocket,
                "createServerSocket",
                "(I)Ljava/net/ServerSocket;",
            )?;
            let port = args.get(1).cloned().unwrap_or(Value::Int(0));
            ctx.new_object_initialized("java/net/ServerSocket", "(I)V", &[port])
        }, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        ssf,
        "createServerSocket",
        "(II)Ljava/net/ServerSocket;",
        |ctx, args| {
            crate::tls_deny::deny_plaintext_fallback(
                ctx,
                args,
                crate::tls_deny::TlsFactoryKind::ServerSocket,
                "createServerSocket",
                "(II)Ljava/net/ServerSocket;",
            )?;
            let port = args.get(1).cloned().unwrap_or(Value::Int(0));
            let backlog = args.get(2).cloned().unwrap_or(Value::Int(50));
            ctx.new_object_initialized("java/net/ServerSocket", "(II)V", &[port, backlog])
        }, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        ssf,
        "createServerSocket",
        "(IILjava/net/InetAddress;)Ljava/net/ServerSocket;",
        |ctx, args| {
            crate::tls_deny::deny_plaintext_fallback(
                ctx,
                args,
                crate::tls_deny::TlsFactoryKind::ServerSocket,
                "createServerSocket",
                "(IILjava/net/InetAddress;)Ljava/net/ServerSocket;",
            )?;
            let port = args.get(1).cloned().unwrap_or(Value::Int(0));
            let backlog = args.get(2).cloned().unwrap_or(Value::Int(50));
            let addr = args.get(3).cloned().unwrap_or(Value::Object(None));
            ctx.new_object_initialized(
                "java/net/ServerSocket",
                "(IILjava/net/InetAddress;)V",
                &[port, backlog, addr],
            )
        }, cratonvm_native_api::NativeKind::SyntheticStub,
    );
}

// ---------------------------------------------------------------------------
// java.text.MessageFormat — 1-field synthetic (pattern=0)
// ---------------------------------------------------------------------------
pub fn register_phase52_message_format(r: &mut NativeMethodRegistry) {
            let mf = "java/text/MessageFormat";
    r.register(mf, "<init>", "(Ljava/lang/String;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let pattern = obj_arg(args, 1)?;
        ctx.set_field(this, 0, Value::Object(Some(pattern)));
        Ok(Some(Value::Object(None)))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(
        mf,
        "<init>",
        "(Ljava/lang/String;Ljava/util/Locale;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let pattern = obj_arg(args, 1)?;
            ctx.set_field(this, 0, Value::Object(Some(pattern)));
            Ok(Some(Value::Object(None)))
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        mf,
        "format",
        "(Ljava/lang/String;[Ljava/lang/Object;)Ljava/lang/String;",
        |ctx, args| {
            let pattern = ctx.read_string(obj_arg(args, 0)?).unwrap_or_default();
            let format_args = p52_read_object_array(ctx, &args[1]);
            let result = p52_message_format_apply(ctx, &pattern, &format_args);
            let s = ctx.create_string(&result);
            Ok(Some(Value::Object(Some(s))))
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        mf,
        "format",
        "([Ljava/lang/Object;)Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let pattern = if let Value::Object(Some(p)) = ctx.get_field(this, 0) {
                ctx.read_string(p).unwrap_or_default()
            } else {
                String::new()
            };
            let format_args = p52_read_object_array(ctx, &args[1]);
            let result = p52_message_format_apply(ctx, &pattern, &format_args);
            let s = ctx.create_string(&result);
            Ok(Some(Value::Object(Some(s))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(mf, "toPattern", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(mf, "applyPattern", "(Ljava/lang/String;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let pattern = obj_arg(args, 1)?;
        ctx.set_field(this, 0, Value::Object(Some(pattern)));
        Ok(Some(Value::Object(None)))
    }, cratonvm_native_api::NativeKind::Bridge);
}

pub fn p52_read_object_array(ctx: &mut dyn NativeContext, val: &Value) -> Vec<Value> {
    if let Value::Object(Some(arr)) = val {
        let len = ctx.array_length(*arr);
        (0..len).map(|i| ctx.get_array_element(*arr, i)).collect()
    } else {
        Vec::new()
    }
}

pub fn p52_message_format_apply(
    ctx: &mut dyn NativeContext,
    pattern: &str,
    format_args: &[Value],
) -> String {
    let mut result = String::with_capacity(pattern.len());
    let chars: Vec<char> = pattern.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '{' {
            let start = i + 1;
            let mut end = start;
            while end < chars.len() && chars[end] != '}' {
                end += 1;
            }
            if end < chars.len() {
                let inner: String = chars[start..end].iter().collect();
                // Split into at most 3 parts: index, format-type, and the rest
                // as the format-style (which may itself contain commas, e.g. the
                // grouping separators in a `number` subformat pattern).
                let mut it = inner.splitn(3, ',');
                let idx_str = it.next().unwrap_or("0").trim();
                let ftype = it.next().map(|s| s.trim());
                let fstyle = it.next().map(|s| s.trim());
                if let Ok(idx) = idx_str.parse::<usize>() {
                    if idx < format_args.len() {
                        // B8 fix: honor the typed format element where tractable
                        // (number styles), falling back to the plain value
                        // rendering for date/time/choice (not yet localized).
                        p52_format_typed_value(ctx, &format_args[idx], ftype, fstyle, &mut result);
                    } else {
                        result.push('{');
                        result.push_str(&inner);
                        result.push('}');
                    }
                } else {
                    result.push('{');
                    result.push_str(&inner);
                    result.push('}');
                }
                i = end + 1;
            } else {
                result.push('{');
                i += 1;
            }
        } else if chars[i] == '\'' {
            if i + 1 < chars.len() && chars[i + 1] == '\'' {
                result.push('\'');
                i += 2;
            } else {
                i += 1;
                while i < chars.len() && chars[i] != '\'' {
                    result.push(chars[i]);
                    i += 1;
                }
                if i < chars.len() {
                    i += 1;
                }
            }
        } else {
            result.push(chars[i]);
            i += 1;
        }
    }
    result
}

pub fn p52_format_value(ctx: &mut dyn NativeContext, val: &Value, out: &mut String) {
    match val {
        Value::Int(n) => out.push_str(&n.to_string()),
        Value::Long(n) => out.push_str(&n.to_string()),
        Value::Float(f) => out.push_str(&f.to_string()),
        Value::Double(d) => out.push_str(&d.to_string()),
        Value::Object(Some(r)) => {
            // String objects read directly; other reference types render via
            // toString() (B8) rather than the misleading literal "null".
            if let Some(s) = ctx.read_string(*r) {
                out.push_str(&s);
            } else {
                match ctx.invoke_virtual(*r, "toString", "()Ljava/lang/String;", &[]) {
                    Ok(Some(Value::Object(Some(s)))) => {
                        out.push_str(&ctx.read_string(s).unwrap_or_else(|| "null".to_string()));
                    }
                    _ => out.push_str("null"),
                }
            }
        }
        Value::Object(None) => out.push_str("null"),
        _ => out.push('?'),
    }
}

/// Apply a MessageFormat typed element `{idx,type,style}` (B8). For `number`
/// elements the common styles are honored (`integer`, `percent`, `currency`,
/// and explicit `#`/`0`/`.`/`,` decimal patterns); `date`/`time`/`choice` are
/// not yet localized, so they fall through to the plain value rendering. This
/// is correct-or-plain rather than silently dropping the type.
pub fn p52_format_typed_value(
    ctx: &mut dyn NativeContext,
    val: &Value,
    ftype: Option<&str>,
    fstyle: Option<&str>,
    out: &mut String,
) {
    let num = match val {
        Value::Int(n) => Some(*n as f64),
        Value::Long(n) => Some(*n as f64),
        Value::Float(f) => Some(*f as f64),
        Value::Double(d) => Some(*d),
        _ => None,
    };
    match (ftype, num) {
        (Some("number"), Some(n)) => {
            let s = p52_format_number(n, fstyle);
            out.push_str(&s);
        }
        // date/time/choice are not yet localized — render the plain value so the
        // output is at least correct in magnitude rather than dropped.
        _ => p52_format_value(ctx, val, out),
    }
}

/// Render a number for a MessageFormat `{n,number,style}` element. Supports the
/// named styles (`integer`, `percent`, `currency`) and explicit decimal
/// patterns of the form `#,##0.00` (the fraction-digit count is taken from the
/// run of `0`/`#` after the decimal point; `,` requests grouping). Defaults to
/// the JDK's general number form (grouping, up to 3 fraction digits).
pub fn p52_format_number(n: f64, style: Option<&str>) -> String {
    match style {
        Some("integer") => p52_group_int(n.round() as i64),
        Some("percent") => format!("{}%", p52_group_int((n * 100.0).round() as i64)),
        Some("currency") => {
            let cents = (n * 100.0).round() / 100.0;
            format!("${}", p52_format_fixed(cents, 2, true))
        }
        Some(pat) if pat.contains('#') || pat.contains('0') || pat.contains('.') => {
            let grouping = pat.contains(',');
            let frac = match pat.split('.').nth(1) {
                Some(after) => after.chars().filter(|c| *c == '0' || *c == '#').count(),
                None => 0,
            };
            p52_format_fixed(n, frac, grouping)
        }
        _ => {
            // General number form: group the integer part, keep up to 3 fraction
            // digits without trailing zeros.
            if n.fract() == 0.0 && n.abs() < 9.0e18 {
                p52_group_int(n as i64)
            } else {
                let s = format!("{:.3}", n);
                let trimmed = s.trim_end_matches('0').trim_end_matches('.');
                match trimmed.split_once('.') {
                    Some((int_part, frac_part)) => {
                        let neg = int_part.starts_with('-');
                        let digits = int_part.trim_start_matches('-');
                        let grouped = p52_group_digits(digits);
                        format!("{}{}.{}", if neg { "-" } else { "" }, grouped, frac_part)
                    }
                    None => p52_group_int(trimmed.parse::<i64>().unwrap_or(0)),
                }
            }
        }
    }
}

/// Format `n` with exactly `frac` fraction digits, optionally grouping the
/// integer part with commas.
pub fn p52_format_fixed(n: f64, frac: usize, grouping: bool) -> String {
    let s = format!("{:.*}", frac, n);
    let neg = s.starts_with('-');
    let body = s.trim_start_matches('-');
    let (int_part, frac_part) = match body.split_once('.') {
        Some((a, b)) => (a, Some(b)),
        None => (body, None),
    };
    let int_out = if grouping {
        p52_group_digits(int_part)
    } else {
        int_part.to_string()
    };
    let mut out = String::new();
    if neg {
        out.push('-');
    }
    out.push_str(&int_out);
    if let Some(fp) = frac_part {
        out.push('.');
        out.push_str(fp);
    }
    out
}

/// Group an `i64` with comma thousands separators.
pub fn p52_group_int(n: i64) -> String {
    let neg = n < 0;
    let digits = n.unsigned_abs().to_string();
    let grouped = p52_group_digits(&digits);
    if neg {
        format!("-{grouped}")
    } else {
        grouped
    }
}

/// Insert comma thousands separators into a string of decimal digits.
pub fn p52_group_digits(digits: &str) -> String {
    let bytes = digits.as_bytes();
    let len = bytes.len();
    let mut out = String::with_capacity(len + len / 3);
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (len - i) % 3 == 0 {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}

// ---------------------------------------------------------------------------
// java.text.DateFormat (abstract) — factory stubs
// ---------------------------------------------------------------------------
pub fn register_phase52_date_format(r: &mut NativeMethodRegistry) {
            let df = "java/text/DateFormat";
    r.register(
        df,
        "getDateInstance",
        "()Ljava/text/DateFormat;",
        |ctx, _args| {
            let obj = try_alloc_concurrent_synthetic(ctx, "java/text/SimpleDateFormat", 2)?;
            // gc-common w20-b: `obj` was held raw across its pattern.
            let obj: ObjectRef = alloc_string_into_field(ctx, obj, 0, "yyyy-MM-dd");
            ctx.set_field(obj, 1, Value::Object(None));
            Ok(Some(Value::Object(Some(obj))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        df,
        "getDateInstance",
        "(I)Ljava/text/DateFormat;",
        |ctx, _args| {
            let obj = try_alloc_concurrent_synthetic(ctx, "java/text/SimpleDateFormat", 2)?;
            // gc-common w20-b: `obj` was held raw across its pattern.
            let obj: ObjectRef = alloc_string_into_field(ctx, obj, 0, "yyyy-MM-dd");
            ctx.set_field(obj, 1, Value::Object(None));
            Ok(Some(Value::Object(Some(obj))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        df,
        "getTimeInstance",
        "()Ljava/text/DateFormat;",
        |ctx, _args| {
            let obj = try_alloc_concurrent_synthetic(ctx, "java/text/SimpleDateFormat", 2)?;
            // gc-common w20-b: `obj` was held raw across its pattern.
            let obj: ObjectRef = alloc_string_into_field(ctx, obj, 0, "HH:mm:ss");
            ctx.set_field(obj, 1, Value::Object(None));
            Ok(Some(Value::Object(Some(obj))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        df,
        "getTimeInstance",
        "(I)Ljava/text/DateFormat;",
        |ctx, _args| {
            let obj = try_alloc_concurrent_synthetic(ctx, "java/text/SimpleDateFormat", 2)?;
            // gc-common w20-b: `obj` was held raw across its pattern.
            let obj: ObjectRef = alloc_string_into_field(ctx, obj, 0, "HH:mm:ss");
            ctx.set_field(obj, 1, Value::Object(None));
            Ok(Some(Value::Object(Some(obj))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        df,
        "getDateTimeInstance",
        "()Ljava/text/DateFormat;",
        |ctx, _args| {
            let obj = try_alloc_concurrent_synthetic(ctx, "java/text/SimpleDateFormat", 2)?;
            // gc-common w20-b: `obj` was held raw across its pattern.
            let obj: ObjectRef = alloc_string_into_field(ctx, obj, 0, "yyyy-MM-dd HH:mm:ss");
            ctx.set_field(obj, 1, Value::Object(None));
            Ok(Some(Value::Object(Some(obj))))
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        df,
        "getDateTimeInstance",
        "(II)Ljava/text/DateFormat;",
        |ctx, _args| {
            let obj = try_alloc_concurrent_synthetic(ctx, "java/text/SimpleDateFormat", 2)?;
            // gc-common w20-b: `obj` was held raw across its pattern.
            let obj: ObjectRef = alloc_string_into_field(ctx, obj, 0, "yyyy-MM-dd HH:mm:ss");
            ctx.set_field(obj, 1, Value::Object(None));
            Ok(Some(Value::Object(Some(obj))))
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    }

// ---------------------------------------------------------------------------
// java.math.MathContext / RoundingMode
// ---------------------------------------------------------------------------
pub fn register_phase52_math_context(r: &mut NativeMethodRegistry) {
            let mc = "java/math/MathContext";
    r.register(mc, "<init>", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let prec = args[1].as_int().unwrap_or(0);
        // `MathContext(int)` is `this(precision, DEFAULT_ROUNDINGMODE)`: the
        // published `HALF_UP`, not a fresh copy, so `getRoundingMode() ==
        // RoundingMode.HALF_UP` (i10-L2). Resolving it may run
        // `RoundingMode.<clinit>`, so `this` is pinned across it.
        let pin = ctx.pin_native_root(this);
        let rm = rounding_mode_constant(ctx, ROUNDING_HALF_UP);
        let this = ctx.read_native_pin(pin, this);
        ctx.unpin_native_roots(pin);
        let rm = rm?;
        ctx.set_field(this, 0, Value::Int(prec));
        ctx.set_field(this, 1, Value::Object(Some(rm)));
        Ok(Some(Value::Object(None)))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(mc, "<init>", "(ILjava/math/RoundingMode;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let prec = args[1].as_int().unwrap_or(0);
        let rm = obj_arg(args, 2)?;
        ctx.set_field(this, 0, Value::Int(prec));
        ctx.set_field(this, 1, Value::Object(Some(rm)));
        Ok(Some(Value::Object(None)))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(mc, "getPrecision", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(
        mc,
        "getRoundingMode",
        "()Ljava/math/RoundingMode;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 1)))
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(mc, "toString", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let prec = ctx.get_field(this, 0).as_int().unwrap_or(0);
        let rm_name = if let Value::Object(Some(rm)) = ctx.get_field(this, 1) {
            let ord = ctx.get_field(rm, 0).as_int().unwrap_or(4);
            p52_rounding_mode_name(ord)
        } else {
            "HALF_UP"
        };
        let s = ctx.create_string(&format!("precision={prec} roundingMode={rm_name}"));
        Ok(Some(Value::Object(Some(s))))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(mc, "equals", "(Ljava/lang/Object;)Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if let Value::Object(Some(other)) = &args[1] {
            let p1 = ctx.get_field(this, 0).as_int().unwrap_or(-1);
            let p2 = ctx.get_field(*other, 0).as_int().unwrap_or(-2);
            Ok(Some(Value::Int(if p1 == p2 { 1 } else { 0 })))
        } else {
            Ok(Some(Value::Int(0)))
        }
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(mc, "hashCode", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let prec = ctx.get_field(this, 0).as_int().unwrap_or(0);
        Ok(Some(Value::Int(prec.wrapping_mul(59))))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(mc, "DECIMAL32", "Ljava/math/MathContext;", |ctx, _args| {
        let obj = try_alloc_concurrent_synthetic(ctx, "java/math/MathContext", 2)?;
        ctx.set_field(obj, 0, Value::Int(7));
        let obj: ObjectRef = p52_store_half_up(ctx, obj)?;
        Ok(Some(Value::Object(Some(obj))))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(mc, "DECIMAL64", "Ljava/math/MathContext;", |ctx, _args| {
        let obj = try_alloc_concurrent_synthetic(ctx, "java/math/MathContext", 2)?;
        ctx.set_field(obj, 0, Value::Int(16));
        let obj: ObjectRef = p52_store_half_up(ctx, obj)?;
        Ok(Some(Value::Object(Some(obj))))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(mc, "DECIMAL128", "Ljava/math/MathContext;", |ctx, _args| {
        let obj = try_alloc_concurrent_synthetic(ctx, "java/math/MathContext", 2)?;
        ctx.set_field(obj, 0, Value::Int(34));
        let obj: ObjectRef = p52_store_half_up(ctx, obj)?;
        Ok(Some(Value::Object(Some(obj))))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(mc, "UNLIMITED", "Ljava/math/MathContext;", |ctx, _args| {
        let obj = try_alloc_concurrent_synthetic(ctx, "java/math/MathContext", 2)?;
        ctx.set_field(obj, 0, Value::Int(0));
        let obj: ObjectRef = p52_store_half_up(ctx, obj)?;
        Ok(Some(Value::Object(Some(obj))))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    }

pub fn register_phase52_rounding_mode(r: &mut NativeMethodRegistry) {
            let rm = "java/math/RoundingMode";
    // Constants published once by `<clinit>` and answered through
    // `rounding_mode_constant` (JLS 8.9 identity, i10-L2).
    r.register(rm, "<clinit>", "()V", |ctx, _args| {
        publish_minted_enum_constants(ctx, ROUNDING_MODE, &ROUNDING_MODE_NAMES, |ctx, i| {
            // Cast: an index below eight.
            rounding_mode_alloc(ctx, i as i32)
        })
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(
        rm,
        "valueOf",
        "(Ljava/lang/String;)Ljava/math/RoundingMode;",
        |ctx, args| {
            // `Enum.valueOf`: NPE on null (i10-L2), IAE on an unknown name.
            let name = match args.first() {
                Some(Value::Object(Some(o))) => ctx.read_string(*o).unwrap_or_default(),
                _ => {
                    return Err(RuntimeError::NullPointerException {
                        message: Some("Name is null".to_string()),
                    }
                    .into())
                }
            };
            let ord = p52_rounding_mode_ordinal(&name);
            if ord < 0 {
                return Err(RuntimeError::IllegalArgumentException {
                    message: crate::lang_class::no_enum_constant_message(
                        "java.math.RoundingMode",
                        &name,
                    ),
                }
                .into());
            }
            let obj = rounding_mode_constant(ctx, ord)?;
            Ok(Some(Value::Object(Some(obj))))
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(rm, "values", "()[Ljava/math/RoundingMode;", |ctx, _args| {
        if let Some(arr) = crate::lang_system::canonical_enum_values(ctx, ROUNDING_MODE) {
            return Ok(Some(Value::Object(Some(arr))));
        }
        synthetic_enum_values(ctx, ROUNDING_MODE, 8, |ctx, i| {
            // Cast: an index below eight.
            rounding_mode_alloc(ctx, i as i32)
        })
    }, cratonvm_native_api::NativeKind::Bridge);
    for method in ["name", "toString"] {
        r.register(rm, method, "()Ljava/lang/String;", |ctx, args| {
            let this = obj_arg(args, 0)?;
            let ord = ctx.get_field(this, 0).as_int().unwrap_or(0);
            let s = ctx.create_string(p52_rounding_mode_name(ord));
            Ok(Some(Value::Object(Some(s))))
        }, cratonvm_native_api::NativeKind::Bridge);
    }
    r.register(rm, "ordinal", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    }, cratonvm_native_api::NativeKind::Bridge);
}

pub const ROUNDING_MODE: &str = "java/math/RoundingMode";

/// `RoundingMode`'s constants in ordinal (`javap`) order, which is also the
/// slot-0 value [`p52_rounding_mode_ordinal`] assigns (and the old
/// `BigDecimal.ROUND_*` numbering `math_bignum.rs` reads); the same list as
/// the synthetic class model's `enum_constant_fields` row in
/// `class_manager.rs`.
pub const ROUNDING_MODE_NAMES: [&str; 8] = [
    "UP",
    "DOWN",
    "CEILING",
    "FLOOR",
    "HALF_UP",
    "HALF_DOWN",
    "HALF_EVEN",
    "UNNECESSARY",
];

/// `RoundingMode.HALF_UP`'s ordinal: `MathContext(int)`'s default mode.
pub const ROUNDING_HALF_UP: i32 = 4;

/// A fresh synthetic `RoundingMode` whose slot 0 is `ord`. Only `<clinit>`
/// and the no-published-constants fallbacks mint; everything else answers
/// [`rounding_mode_constant`].
pub fn rounding_mode_alloc(
    ctx: &mut dyn NativeContext,
    ord: i32,
) -> Result<ObjectRef, MethodCallFailed> {
    let obj = try_alloc_concurrent_synthetic(ctx, ROUNDING_MODE, 1)?;
    ctx.set_field(obj, 0, Value::Int(ord));
    Ok(obj)
}

/// The published `RoundingMode` constant for `ord`, or a minted one when
/// the class has published none (i10-L2), so
/// `new MathContext(7).getRoundingMode() == RoundingMode.HALF_UP`.
pub fn rounding_mode_constant(
    ctx: &mut dyn NativeContext,
    ord: i32,
) -> Result<ObjectRef, MethodCallFailed> {
    if let Some(Value::Object(Some(constant))) =
        crate::lang_system::canonical_enum_constant(ctx, ROUNDING_MODE, p52_rounding_mode_name(ord))
    {
        return Ok(constant);
    }
    rounding_mode_alloc(ctx, ord)
}

pub fn p52_rounding_mode_ordinal(name: &str) -> i32 {
    match name {
        "UP" => 0,
        "DOWN" => 1,
        "CEILING" => 2,
        "FLOOR" => 3,
        "HALF_UP" => 4,
        "HALF_DOWN" => 5,
        "HALF_EVEN" => 6,
        "UNNECESSARY" => 7,
        _ => -1,
    }
}

pub fn p52_rounding_mode_name(ord: i32) -> &'static str {
    match ord {
        0 => "UP",
        1 => "DOWN",
        2 => "CEILING",
        3 => "FLOOR",
        4 => "HALF_UP",
        5 => "HALF_DOWN",
        6 => "HALF_EVEN",
        7 => "UNNECESSARY",
        _ => "HALF_UP",
    }
}

// ---------------------------------------------------------------------------
// java.util.function extras
// ---------------------------------------------------------------------------
pub fn register_phase52_function_extras(r: &mut NativeMethodRegistry) {
            r.register(
        "java/util/function/BinaryOperator",
        "apply",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.invoke_virtual(
                this,
                "apply",
                "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
                &args[1..],
            )
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        "java/util/function/UnaryOperator",
        "apply",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.invoke_virtual(
                this,
                "apply",
                "(Ljava/lang/Object;)Ljava/lang/Object;",
                &args[1..],
            )
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        "java/util/function/IntFunction",
        "apply",
        "(I)Ljava/lang/Object;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.invoke_virtual(this, "apply", "(I)Ljava/lang/Object;", &args[1..])
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        "java/util/function/LongFunction",
        "apply",
        "(J)Ljava/lang/Object;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.invoke_virtual(this, "apply", "(J)Ljava/lang/Object;", &args[1..])
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        "java/util/function/DoubleFunction",
        "apply",
        "(D)Ljava/lang/Object;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.invoke_virtual(this, "apply", "(D)Ljava/lang/Object;", &args[1..])
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        "java/util/function/ToIntFunction",
        "applyAsInt",
        "(Ljava/lang/Object;)I",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.invoke_virtual(this, "applyAsInt", "(Ljava/lang/Object;)I", &args[1..])
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        "java/util/function/ToLongFunction",
        "applyAsLong",
        "(Ljava/lang/Object;)J",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.invoke_virtual(this, "applyAsLong", "(Ljava/lang/Object;)J", &args[1..])
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        "java/util/function/ToDoubleFunction",
        "applyAsDouble",
        "(Ljava/lang/Object;)D",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.invoke_virtual(this, "applyAsDouble", "(Ljava/lang/Object;)D", &args[1..])
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        "java/util/function/IntToLongFunction",
        "applyAsLong",
        "(I)J",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.invoke_virtual(this, "applyAsLong", "(I)J", &args[1..])
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        "java/util/function/IntToDoubleFunction",
        "applyAsDouble",
        "(I)D",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.invoke_virtual(this, "applyAsDouble", "(I)D", &args[1..])
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        "java/util/function/LongToIntFunction",
        "applyAsInt",
        "(J)I",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.invoke_virtual(this, "applyAsInt", "(J)I", &args[1..])
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        "java/util/function/LongToDoubleFunction",
        "applyAsDouble",
        "(J)D",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.invoke_virtual(this, "applyAsDouble", "(J)D", &args[1..])
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        "java/util/function/DoubleToIntFunction",
        "applyAsInt",
        "(D)I",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.invoke_virtual(this, "applyAsInt", "(D)I", &args[1..])
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        "java/util/function/DoubleToLongFunction",
        "applyAsLong",
        "(D)J",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.invoke_virtual(this, "applyAsLong", "(D)J", &args[1..])
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        "java/util/function/ObjIntConsumer",
        "accept",
        "(Ljava/lang/Object;I)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.invoke_virtual(this, "accept", "(Ljava/lang/Object;I)V", &args[1..])
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        "java/util/function/ObjLongConsumer",
        "accept",
        "(Ljava/lang/Object;J)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.invoke_virtual(this, "accept", "(Ljava/lang/Object;J)V", &args[1..])
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        "java/util/function/ObjDoubleConsumer",
        "accept",
        "(Ljava/lang/Object;D)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.invoke_virtual(this, "accept", "(Ljava/lang/Object;D)V", &args[1..])
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        "java/util/function/IntSupplier",
        "getAsInt",
        "()I",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.invoke_virtual(this, "getAsInt", "()I", &[])
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        "java/util/function/LongSupplier",
        "getAsLong",
        "()J",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.invoke_virtual(this, "getAsLong", "()J", &[])
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        "java/util/function/DoubleSupplier",
        "getAsDouble",
        "()D",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.invoke_virtual(this, "getAsDouble", "()D", &[])
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        "java/util/function/BooleanSupplier",
        "getAsBoolean",
        "()Z",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.invoke_virtual(this, "getAsBoolean", "()Z", &[])
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        "java/util/function/IntBinaryOperator",
        "applyAsInt",
        "(II)I",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.invoke_virtual(this, "applyAsInt", "(II)I", &args[1..])
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        "java/util/function/LongBinaryOperator",
        "applyAsLong",
        "(JJ)J",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.invoke_virtual(this, "applyAsLong", "(JJ)J", &args[1..])
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        "java/util/function/DoubleBinaryOperator",
        "applyAsDouble",
        "(DD)D",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.invoke_virtual(this, "applyAsDouble", "(DD)D", &args[1..])
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        "java/util/function/IntConsumer",
        "accept",
        "(I)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.invoke_virtual(this, "accept", "(I)V", &args[1..])
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        "java/util/function/LongConsumer",
        "accept",
        "(J)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.invoke_virtual(this, "accept", "(J)V", &args[1..])
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        "java/util/function/DoubleConsumer",
        "accept",
        "(D)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.invoke_virtual(this, "accept", "(D)V", &args[1..])
        }, cratonvm_native_api::NativeKind::Bridge,
    );
}

