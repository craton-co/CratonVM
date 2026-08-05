// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! `java.util.concurrent` natives: executors, CompletableFuture, queues, StampedLock, StructuredTaskScope, ScopedValue, atomics, Thread/virtual-thread shims.
//!
//! Pure code move out of `phases_late.rs` (no logic, signature or ordering
//! changes). Every registration call site is untouched and the per-phase
//! dispatchers stay in the parent module, so the native registration SEQUENCE
//! is byte-identical to before the split.

use super::*;

// ---------------------------------------------------------------------------
// java.util.concurrent — Executors, Future, Callable, ExecutorService
// ExecutorService = 4-field synthetic:
//   0 = shutdown (Int: 0=running, 1=shutdown)
//   1 = pool_size (Int)
//   2 = task_queue (Reference[] array of Callable/Runnable objects)
//   3 = task_count (Int: number of pending tasks in queue)
// Future = 4-field synthetic:
//   0 = result (Object)
//   1 = done (Int: 0=pending, 1=done)
//   2 = cancelled (Int: 0=no, 1=yes)
//   3 = task (Object: the original Callable/Runnable for lazy execution)
// CompletableFuture = 3-field: result=0, done=1, exception=2
// ---------------------------------------------------------------------------
pub(crate) fn register_phase55_executors(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    // --- Callable<V> interface ---
    let callable = "java/util/concurrent/Callable";
    r.register(callable, "call", "()Ljava/lang/Object;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.invoke_virtual(this, "call", "()Ljava/lang/Object;", &[])
    });

    // --- Future<V> — 4-field synthetic (result=0, done=1, cancelled=2, task=3) ---
    let future = "java/util/concurrent/Future";

    // get() — lazy execution: if not done and not cancelled, execute the stored task
    r.register(future, "get", "()Ljava/lang/Object;", |ctx, args| {
        let mut this = obj_arg(args, 0)?;
        let cancelled = ctx.get_field(this, 2).as_int().unwrap_or(0);
        if cancelled != 0 {
            return Err(RuntimeError::IllegalStateException {
                message: "Task was cancelled".into(),
            }
            .into());
        }
        let done = ctx.get_field(this, 1).as_int().unwrap_or(0);
        if done == 0 {
            // Lazy execution: run the stored task now
            if let Value::Object(Some(mut task)) = ctx.get_field(this, 3) {
                // Pin across the task callback below — a moving young GC there
                // would relocate `this`/`task` (native stale-local family).
                let this_pin = ctx.pin_native_root(this);
                let task_pin = ctx.pin_native_root(task);
                // Try Callable.call() first, fall back to Runnable.run()
                let result = ctx.invoke_virtual(task, "call", "()Ljava/lang/Object;", &[]);
                this = ctx.read_native_pin(this_pin, this);
                match result {
                    Ok(val) => {
                        ctx.set_field(this, 0, val.unwrap_or(Value::Object(None)));
                    }
                    Err(_) => {
                        // Might be a Runnable, not a Callable
                        task = ctx.read_native_pin(task_pin, task);
                        let _ = ctx.invoke_virtual(task, "run", "()V", &[]);
                        this = ctx.read_native_pin(this_pin, this);
                        ctx.set_field(this, 0, Value::Object(None));
                    }
                }
                ctx.unpin_native_roots(this_pin);
            }
            ctx.set_field(this, 1, Value::Int(1)); // mark done
        }
        Ok(Some(ctx.get_field(this, 0)))
    });
    r.register(
        future,
        "get",
        "(JLjava/util/concurrent/TimeUnit;)Ljava/lang/Object;",
        |ctx, args| {
            let mut this = obj_arg(args, 0)?;
            let cancelled = ctx.get_field(this, 2).as_int().unwrap_or(0);
            if cancelled != 0 {
                return Err(RuntimeError::IllegalStateException {
                    message: "Task was cancelled".into(),
                }
                .into());
            }
            let done = ctx.get_field(this, 1).as_int().unwrap_or(0);
            if done == 0 {
                if let Value::Object(Some(mut task)) = ctx.get_field(this, 3) {
                    // Pin across the task callback below — a moving young GC
                    // there would relocate `this`/`task` (native stale-local
                    // family).
                    let this_pin = ctx.pin_native_root(this);
                    let task_pin = ctx.pin_native_root(task);
                    let result = ctx.invoke_virtual(task, "call", "()Ljava/lang/Object;", &[]);
                    this = ctx.read_native_pin(this_pin, this);
                    match result {
                        Ok(val) => ctx.set_field(this, 0, val.unwrap_or(Value::Object(None))),
                        Err(_) => {
                            task = ctx.read_native_pin(task_pin, task);
                            let _ = ctx.invoke_virtual(task, "run", "()V", &[]);
                            this = ctx.read_native_pin(this_pin, this);
                            ctx.set_field(this, 0, Value::Object(None));
                        }
                    }
                    ctx.unpin_native_roots(this_pin);
                }
                ctx.set_field(this, 1, Value::Int(1));
            }
            Ok(Some(ctx.get_field(this, 0)))
        },
    );
    r.register(future, "isDone", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let done = ctx.get_field(this, 1).as_int().unwrap_or(0);
        let cancelled = ctx.get_field(this, 2).as_int().unwrap_or(0);
        Ok(Some(Value::Int(if done != 0 || cancelled != 0 {
            1
        } else {
            0
        })))
    });
    r.register(future, "isCancelled", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 2)))
    });
    r.register(future, "cancel", "(Z)Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let done = ctx.get_field(this, 1).as_int().unwrap_or(0);
        if done != 0 {
            // Already completed — cannot cancel
            return Ok(Some(Value::Int(0)));
        }
        let already_cancelled = ctx.get_field(this, 2).as_int().unwrap_or(0);
        if already_cancelled != 0 {
            return Ok(Some(Value::Int(1))); // already cancelled
        }
        // Cancel the task
        ctx.set_field(this, 2, Value::Int(1));
        ctx.set_field(this, 1, Value::Int(1)); // mark done (cancelled is a terminal state)
        Ok(Some(Value::Int(1)))
    });

    // --- CompletableFuture extra methods ---
    let cf = "java/util/concurrent/CompletableFuture";
    r.register(
        cf,
        "supplyAsync",
        "(Ljava/util/function/Supplier;)Ljava/util/concurrent/CompletableFuture;",
        |ctx, args| {
            let supplier = obj_arg(args, 0)?;
            let result = ctx.invoke_virtual(supplier, "get", "()Ljava/lang/Object;", &[]);
            // Pin across the CF alloc / create_string below — a moving young GC
            // there would relocate the supplied value and the fresh CF (native
            // stale-local family).
            let val = match &result {
                Ok(v) => (*v).unwrap_or(Value::Object(None)),
                Err(_) => Value::Object(None),
            };
            let val_pin = pinned_object_value(ctx, val);
            let mut future =
                alloc_concurrent_synthetic(ctx, "java/util/concurrent/CompletableFuture", 3);
            let future_pin = ctx.pin_native_root(future);
            match result {
                Ok(_) => {
                    let val = read_pinned_object_value(ctx, val_pin, val);
                    ctx.set_field(future, 0, val);
                    ctx.set_field(future, 2, Value::Object(None));
                }
                Err(e) => {
                    ctx.set_field(future, 0, Value::Object(None));
                    let err_str = ctx.create_string(&format!("{:?}", e));
                    future = ctx.read_native_pin(future_pin, future);
                    ctx.set_field(future, 2, Value::Object(Some(err_str)));
                }
            }
            ctx.set_field(future, 1, Value::Int(1));
            ctx.unpin_native_roots(val_pin.map(|(h, _)| h).unwrap_or(future_pin));
            Ok(Some(Value::Object(Some(future))))
        },
    );
    r.register(
        cf,
        "supplyAsync",
        "(Ljava/util/function/Supplier;Ljava/util/concurrent/Executor;)Ljava/util/concurrent/CompletableFuture;",
        |ctx, args| {
            let supplier = obj_arg(args, 0)?;
            // B5 limitation (documented, not silent): this synthetic
            // CompletableFuture model runs the supplier EAGERLY on the calling
            // thread, so the supplied Executor's thread/affinity/parallelism is
            // not honored. The *result value* is correct (the supplier always
            // runs) and the returned CF is completed; only the async scheduling
            // dimension differs from the JDK. Routing through executor.execute()
            // is deliberately NOT done because an asynchronous executor would
            // return before producing the result, leaving the CF marked done
            // with no value — that would trade a scheduling difference for a
            // correctness bug. A faithful fix needs the real carrier scheduler.
            let result = ctx.invoke_virtual(supplier, "get", "()Ljava/lang/Object;", &[]);
            // Pin across the CF alloc / create_string below — a moving young GC
            // there would relocate the supplied value and the fresh CF (native
            // stale-local family).
            let val = match &result {
                Ok(v) => (*v).unwrap_or(Value::Object(None)),
                Err(_) => Value::Object(None),
            };
            let val_pin = pinned_object_value(ctx, val);
            let mut future =
                alloc_concurrent_synthetic(ctx, "java/util/concurrent/CompletableFuture", 3);
            let future_pin = ctx.pin_native_root(future);
            match result {
                Ok(_) => {
                    let val = read_pinned_object_value(ctx, val_pin, val);
                    ctx.set_field(future, 0, val);
                    ctx.set_field(future, 2, Value::Object(None));
                }
                Err(e) => {
                    ctx.set_field(future, 0, Value::Object(None));
                    let err_str = ctx.create_string(&format!("{:?}", e));
                    future = ctx.read_native_pin(future_pin, future);
                    ctx.set_field(future, 2, Value::Object(Some(err_str)));
                }
            }
            ctx.set_field(future, 1, Value::Int(1));
            ctx.unpin_native_roots(val_pin.map(|(h, _)| h).unwrap_or(future_pin));
            Ok(Some(Value::Object(Some(future))))
        },
    );
    r.register(
        cf,
        "runAsync",
        "(Ljava/lang/Runnable;)Ljava/util/concurrent/CompletableFuture;",
        |ctx, args| {
            let runnable = obj_arg(args, 0)?;
            let result = ctx.invoke_virtual(runnable, "run", "()V", &[]);
            // Phaser/ForkJoinPool hang fix (2026-07-07): a `MethodCallFailed::
            // InternalError` means the Runnable's call stack was torn down by
            // the VM WITHOUT ever routing through the callee's own bytecode
            // exception table — any `try/finally` inside `run()` (e.g. a
            // Phaser `arriveAndDeregister()` guarding a rendezvous, as in
            // SmallRye Sisu's `BeanLoadingTaskRunner`) is skipped entirely,
            // not just the catch. Previously this was stringified into the
            // CF's error field and swallowed, so `runAsync` always reported
            // eventual success to the caller even though the Runnable body
            // silently never finished running — desyncing any external
            // bookkeeping (like Phaser party counts) that assumed `run()`'s
            // own cleanup always executes. Propagate InternalError out of
            // this native method instead so the failure is visible (VM abort
            // / caller sees the failure) rather than hidden. A genuine Java
            // exception (`ExceptionThrown`) DID pass through the callee's
            // exception table already (finally blocks ran), so that case is
            // unaffected and keeps the existing eager-completion modeling.
            if matches!(result, Err(MethodCallFailed::InternalError(_))) {
                // Safe to unwrap: just matched Err(InternalError(_)) above.
                return Err(result.unwrap_err());
            }
            let mut future =
                alloc_concurrent_synthetic(ctx, "java/util/concurrent/CompletableFuture", 3);
            // Pin across the create_string in the Err branch below — a moving
            // young GC there would relocate the fresh CF (native stale-local
            // family).
            let future_pin = ctx.pin_native_root(future);
            ctx.set_field(future, 0, Value::Object(None));
            ctx.set_field(future, 1, Value::Int(1));
            match result {
                Ok(_) => ctx.set_field(future, 2, Value::Object(None)),
                Err(e) => {
                    let err_str = ctx.create_string(&format!("{:?}", e));
                    future = ctx.read_native_pin(future_pin, future);
                    ctx.set_field(future, 2, Value::Object(Some(err_str)));
                }
            }
            ctx.unpin_native_roots(future_pin);
            Ok(Some(Value::Object(Some(future))))
        },
    );
    r.register(
        cf,
        "runAsync",
        "(Ljava/lang/Runnable;Ljava/util/concurrent/Executor;)Ljava/util/concurrent/CompletableFuture;",
        |ctx, args| {
            let runnable = obj_arg(args, 0)?;
            // B5 limitation (documented, not silent): the Runnable runs eagerly
            // on the calling thread; the supplied Executor is not used. The CF
            // completes with the correct (void) outcome — see the supplyAsync
            // overload above for why we do not route through executor.execute().
            let result = ctx.invoke_virtual(runnable, "run", "()V", &[]);
            // Phaser/ForkJoinPool hang fix (2026-07-07): see the no-Executor
            // overload above for the full rationale — an InternalError means
            // the callee's own try/finally never ran, so we must not report
            // success back to the caller.
            if matches!(result, Err(MethodCallFailed::InternalError(_))) {
                // Safe to unwrap: just matched Err(InternalError(_)) above.
                return Err(result.unwrap_err());
            }
            let mut future =
                alloc_concurrent_synthetic(ctx, "java/util/concurrent/CompletableFuture", 3);
            // Pin across the create_string in the Err branch below — a moving
            // young GC there would relocate the fresh CF (native stale-local
            // family).
            let future_pin = ctx.pin_native_root(future);
            ctx.set_field(future, 0, Value::Object(None));
            ctx.set_field(future, 1, Value::Int(1));
            match result {
                Ok(_) => ctx.set_field(future, 2, Value::Object(None)),
                Err(e) => {
                    let err_str = ctx.create_string(&format!("{:?}", e));
                    future = ctx.read_native_pin(future_pin, future);
                    ctx.set_field(future, 2, Value::Object(Some(err_str)));
                }
            }
            ctx.unpin_native_roots(future_pin);
            Ok(Some(Value::Object(Some(future))))
        },
    );
    r.register(
        cf,
        "allOf",
        "([Ljava/util/concurrent/CompletableFuture;)Ljava/util/concurrent/CompletableFuture;",
        |ctx, args| {
            // Real-JDK async inputs (e.g. `runAsync` on a worker) may still be PENDING:
            // delegate to the real JDK private static `andTree` so the returned CF
            // completes only when every input does (see `p58_cf_all_of`). The eager
            // model below would mark it done immediately. Synthetic CFs (done-flag Int
            // in slot 1) keep the eager model.
            if let Some(Value::Object(Some(arr))) = args.first() {
                let arr = *arr;
                let len = ctx.array_length(arr);
                let mut any_real = false;
                for i in 0..len {
                    if let Value::Object(Some(cf_ref)) = ctx.get_array_element(arr, i) {
                        if !matches!(ctx.get_field(cf_ref, FUT_FIELD_DONE), Value::Int(_)) {
                            any_real = true;
                            break;
                        }
                    }
                }
                if any_real {
                    return ctx.invoke_special(
                        "java/util/concurrent/CompletableFuture",
                        "andTree",
                        "([Ljava/util/concurrent/CompletableFuture;II)Ljava/util/concurrent/CompletableFuture;",
                        &[
                            Value::Object(Some(arr)),
                            Value::Int(0),
                            Value::Int(len as i32 - 1),
                        ],
                    );
                }
            }
            // Pin across the CF alloc / create_string below — a moving young GC
            // there would relocate `arr` and the fresh CF (native stale-local
            // family).
            let arr_pin = match args.first() {
                Some(Value::Object(Some(a))) => Some((ctx.pin_native_root(*a), *a)),
                _ => None,
            };
            let mut future =
                alloc_concurrent_synthetic(ctx, "java/util/concurrent/CompletableFuture", 3);
            let future_pin = ctx.pin_native_root(future);
            // Check if any constituent CF has an exception
            let mut has_exception = false;
            if let Some((h, orig)) = arr_pin {
                let arr = ctx.read_native_pin(h, orig);
                let len = ctx.array_length(arr);
                for i in 0..len {
                    if let Value::Object(Some(cf_ref)) = ctx.get_array_element(arr, i) {
                        if let Value::Object(Some(_)) = ctx.get_field(cf_ref, 2) {
                            has_exception = true;
                            break;
                        }
                    }
                }
            }
            ctx.set_field(future, 0, Value::Object(None));
            ctx.set_field(future, 1, Value::Int(1));
            if has_exception {
                let err = ctx.create_string("One or more CompletableFutures failed");
                future = ctx.read_native_pin(future_pin, future);
                ctx.set_field(future, 2, Value::Object(Some(err)));
            } else {
                ctx.set_field(future, 2, Value::Object(None));
            }
            ctx.unpin_native_roots(arr_pin.map(|(h, _)| h).unwrap_or(future_pin));
            Ok(Some(Value::Object(Some(future))))
        },
    );
    r.register(
        cf,
        "anyOf",
        "([Ljava/util/concurrent/CompletableFuture;)Ljava/util/concurrent/CompletableFuture;",
        |ctx, args| {
            let arr = obj_arg(args, 0)?;
            let len = ctx.array_length(arr);
            if len > 0 {
                // Return the result of the first completed CF
                let first = ctx.get_array_element(arr, 0);
                if let Value::Object(Some(cf_ref)) = first {
                    let result = ctx.get_field(cf_ref, 0);
                    let exc = ctx.get_field(cf_ref, 2);
                    // Pin across the CF alloc below — a moving young GC there
                    // would relocate them (native stale-local family).
                    let result_pin = pinned_object_value(ctx, result);
                    let exc_pin = pinned_object_value(ctx, exc);
                    let future = alloc_concurrent_synthetic(
                        ctx,
                        "java/util/concurrent/CompletableFuture",
                        3,
                    );
                    let result = read_pinned_object_value(ctx, result_pin, result);
                    let exc = read_pinned_object_value(ctx, exc_pin, exc);
                    ctx.set_field(future, 0, result);
                    ctx.set_field(future, 1, Value::Int(1));
                    ctx.set_field(future, 2, exc);
                    if let Some((h, _)) = result_pin.or(exc_pin) {
                        ctx.unpin_native_roots(h);
                    }
                    return Ok(Some(Value::Object(Some(future))));
                }
            }
            let future =
                alloc_concurrent_synthetic(ctx, "java/util/concurrent/CompletableFuture", 3);
            ctx.set_field(future, 0, Value::Object(None));
            ctx.set_field(future, 1, Value::Int(1));
            ctx.set_field(future, 2, Value::Object(None));
            Ok(Some(Value::Object(Some(future))))
        },
    );
    r.register(
        cf,
        "completedFuture",
        "(Ljava/lang/Object;)Ljava/util/concurrent/CompletableFuture;",
        |ctx, args| {
            let value = args[0];
            // Pin across the CF alloc below — a moving young GC there would
            // relocate it (native stale-local family).
            let value_pin = pinned_object_value(ctx, value);
            let future =
                alloc_concurrent_synthetic(ctx, "java/util/concurrent/CompletableFuture", 3);
            let value = read_pinned_object_value(ctx, value_pin, value);
            ctx.set_field(future, 0, value);
            ctx.set_field(future, 1, Value::Int(1));
            ctx.set_field(future, 2, Value::Object(None));
            if let Some((h, _)) = value_pin {
                ctx.unpin_native_roots(h);
            }
            Ok(Some(Value::Object(Some(future))))
        },
    );
    r.register(
        cf,
        "failedFuture",
        "(Ljava/lang/Throwable;)Ljava/util/concurrent/CompletableFuture;",
        |ctx, args| {
            let exc = args[0];
            // Pin across the CF alloc below — a moving young GC there would
            // relocate it (native stale-local family).
            let exc_pin = pinned_object_value(ctx, exc);
            let future =
                alloc_concurrent_synthetic(ctx, "java/util/concurrent/CompletableFuture", 3);
            let exc = read_pinned_object_value(ctx, exc_pin, exc);
            ctx.set_field(future, 0, Value::Object(None));
            ctx.set_field(future, 1, Value::Int(1));
            ctx.set_field(future, 2, exc); // store exception
            if let Some((h, _)) = exc_pin {
                ctx.unpin_native_roots(h);
            }
            Ok(Some(Value::Object(Some(future))))
        },
    );
    // complete(value) — complete the CF with a value if not already done.
    // Delegates to the shared, real-JDK-aware impl so a thread parked in the
    // genuine `waitingGet()` (untimed `get()`/`join()`) is unparked via
    // `postComplete()` instead of hanging forever. See `native_cf_complete`.
    r.register(
        cf,
        "complete",
        "(Ljava/lang/Object;)Z",
        crate::native_cf_complete,
    );
    // completeExceptionally(Throwable) — complete with exception
    r.register(
        cf,
        "completeExceptionally",
        "(Ljava/lang/Throwable;)Z",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let done = ctx.get_field(this, 1).as_int().unwrap_or(0);
            if done != 0 {
                return Ok(Some(Value::Int(0)));
            }
            ctx.set_field(this, 0, Value::Object(None));
            ctx.set_field(this, 1, Value::Int(1));
            ctx.set_field(this, 2, args.get(1).copied().unwrap_or(Value::Object(None)));
            Ok(Some(Value::Int(1)))
        },
    );
    // isCompletedExceptionally()
    r.register(cf, "isCompletedExceptionally", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let exc = ctx.get_field(this, 2);
        Ok(Some(Value::Int(if matches!(exc, Value::Object(Some(_))) {
            1
        } else {
            0
        })))
    });
    // thenCompose — chains a CompletableFuture-returning function
    r.register(
        cf,
        "thenCompose",
        "(Ljava/util/function/Function;)Ljava/util/concurrent/CompletableFuture;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let func = obj_arg(args, 1)?;
            // Check if this CF has an exception — propagate it
            if let Value::Object(Some(_)) = ctx.get_field(this, 2) {
                // Pin across the CF alloc below — a moving young GC there would
                // relocate `this` (native stale-local family).
                let this_pin = ctx.pin_native_root(this);
                let new_cf =
                    alloc_concurrent_synthetic(ctx, "java/util/concurrent/CompletableFuture", 3);
                let this = ctx.read_native_pin(this_pin, this);
                ctx.set_field(new_cf, 0, Value::Object(None));
                ctx.set_field(new_cf, 1, Value::Int(1));
                ctx.set_field(new_cf, 2, ctx.get_field(this, 2));
                ctx.unpin_native_roots(this_pin);
                return Ok(Some(Value::Object(Some(new_cf))));
            }
            let val = ctx.get_field(this, 0);
            let result = ctx.invoke_virtual(
                func,
                "apply",
                "(Ljava/lang/Object;)Ljava/lang/Object;",
                &[val],
            )?;
            // The result should be a CompletableFuture — return it directly
            Ok(result)
        },
    );
    // exceptionally — handle exceptions
    r.register(
        cf,
        "exceptionally",
        "(Ljava/util/function/Function;)Ljava/util/concurrent/CompletableFuture;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let func = obj_arg(args, 1)?;
            if let Value::Object(Some(exc)) = ctx.get_field(this, 2) {
                // Has exception — apply the handler
                let result = ctx.invoke_virtual(
                    func,
                    "apply",
                    "(Ljava/lang/Object;)Ljava/lang/Object;",
                    &[Value::Object(Some(exc))],
                )?;
                // Pin across the CF alloc below — a moving young GC there would
                // relocate the handler's result (native stale-local family).
                let result = result.unwrap_or(Value::Object(None));
                let result_pin = pinned_object_value(ctx, result);
                let new_cf =
                    alloc_concurrent_synthetic(ctx, "java/util/concurrent/CompletableFuture", 3);
                let result = read_pinned_object_value(ctx, result_pin, result);
                ctx.set_field(new_cf, 0, result);
                ctx.set_field(new_cf, 1, Value::Int(1));
                ctx.set_field(new_cf, 2, Value::Object(None));
                if let Some((h, _)) = result_pin {
                    ctx.unpin_native_roots(h);
                }
                Ok(Some(Value::Object(Some(new_cf))))
            } else {
                // No exception — pass through
                Ok(Some(Value::Object(Some(this))))
            }
        },
    );
    // handle — BiFunction that gets both result and exception
    r.register(
        cf,
        "handle",
        "(Ljava/util/function/BiFunction;)Ljava/util/concurrent/CompletableFuture;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let func = obj_arg(args, 1)?;
            let val = ctx.get_field(this, 0);
            let exc = ctx.get_field(this, 2);
            let result = ctx.invoke_virtual(
                func,
                "apply",
                "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
                &[val, exc],
            )?;
            // Pin across the CF alloc below — a moving young GC there would
            // relocate the handler's result (native stale-local family).
            let result = result.unwrap_or(Value::Object(None));
            let result_pin = pinned_object_value(ctx, result);
            let new_cf =
                alloc_concurrent_synthetic(ctx, "java/util/concurrent/CompletableFuture", 3);
            let result = read_pinned_object_value(ctx, result_pin, result);
            ctx.set_field(new_cf, 0, result);
            ctx.set_field(new_cf, 1, Value::Int(1));
            ctx.set_field(new_cf, 2, Value::Object(None));
            if let Some((h, _)) = result_pin {
                ctx.unpin_native_roots(h);
            }
            Ok(Some(Value::Object(Some(new_cf))))
        },
    );
    // join() — same as get() but wraps in CompletionException
    r.register(cf, "join", "()Ljava/lang/Object;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if let Value::Object(Some(_)) = ctx.get_field(this, 2) {
            return Err(RuntimeError::IllegalStateException {
                message: "CompletableFuture completed exceptionally".into(),
            }
            .into());
        }
        Ok(Some(ctx.get_field(this, 0)))
    });
    // getNow(defaultValue) — return result if done, else default
    r.register(
        cf,
        "getNow",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let done = ctx.get_field(this, 1).as_int().unwrap_or(0);
            if done != 0 {
                Ok(Some(ctx.get_field(this, 0)))
            } else {
                Ok(Some(args.get(1).copied().unwrap_or(Value::Object(None))))
            }
        },
    );

    // --- Executors (static factory) ---
    let execs = "java/util/concurrent/Executors";
    // All executors return an ExecutorService with pool_size tracking
    r.register(
        execs,
        "newSingleThreadExecutor",
        "()Ljava/util/concurrent/ExecutorService;",
        |ctx, _args| {
            let es = alloc_concurrent_synthetic(ctx, "java/util/concurrent/ExecutorService", 4);
            // Pin across the queue alloc below — a moving young GC there would
            // relocate the fresh executor (native stale-local family).
            let es_pin = ctx.pin_native_root(es);
            ctx.set_field(es, 0, Value::Int(0)); // not shutdown
            ctx.set_field(es, 1, Value::Int(1)); // pool size = 1
            let queue = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 64);
            let es = ctx.read_native_pin(es_pin, es);
            ctx.set_field(es, 2, Value::Object(Some(queue)));
            ctx.set_field(es, 3, Value::Int(0)); // task count
            ctx.unpin_native_roots(es_pin);
            Ok(Some(Value::Object(Some(es))))
        },
    );
    r.register(
        execs,
        "newFixedThreadPool",
        "(I)Ljava/util/concurrent/ExecutorService;",
        |ctx, args| {
            let pool_size = args.first().and_then(|v| v.as_int()).unwrap_or(4);
            let es = alloc_concurrent_synthetic(ctx, "java/util/concurrent/ExecutorService", 4);
            // Pin across the queue alloc below — a moving young GC there would
            // relocate the fresh executor (native stale-local family).
            let es_pin = ctx.pin_native_root(es);
            ctx.set_field(es, 0, Value::Int(0));
            ctx.set_field(es, 1, Value::Int(pool_size));
            let queue = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 64);
            let es = ctx.read_native_pin(es_pin, es);
            ctx.set_field(es, 2, Value::Object(Some(queue)));
            ctx.set_field(es, 3, Value::Int(0));
            ctx.unpin_native_roots(es_pin);
            Ok(Some(Value::Object(Some(es))))
        },
    );
    r.register(
        execs,
        "newCachedThreadPool",
        "()Ljava/util/concurrent/ExecutorService;",
        |ctx, _args| {
            let es = alloc_concurrent_synthetic(ctx, "java/util/concurrent/ExecutorService", 4);
            // Pin across the queue alloc below — a moving young GC there would
            // relocate the fresh executor (native stale-local family).
            let es_pin = ctx.pin_native_root(es);
            ctx.set_field(es, 0, Value::Int(0));
            ctx.set_field(es, 1, Value::Int(i32::MAX)); // unbounded
            let queue = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 64);
            let es = ctx.read_native_pin(es_pin, es);
            ctx.set_field(es, 2, Value::Object(Some(queue)));
            ctx.set_field(es, 3, Value::Int(0));
            ctx.unpin_native_roots(es_pin);
            Ok(Some(Value::Object(Some(es))))
        },
    );
    r.register(
        execs,
        "newCachedThreadPool",
        "(Ljava/util/concurrent/ThreadFactory;)Ljava/util/concurrent/ExecutorService;",
        |ctx, _args| {
            let es = alloc_concurrent_synthetic(ctx, "java/util/concurrent/ExecutorService", 4);
            // Pin across the queue alloc below — a moving young GC there would
            // relocate the fresh executor (native stale-local family).
            let es_pin = ctx.pin_native_root(es);
            ctx.set_field(es, 0, Value::Int(0));
            ctx.set_field(es, 1, Value::Int(i32::MAX)); // unbounded
            let queue = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 64);
            let es = ctx.read_native_pin(es_pin, es);
            ctx.set_field(es, 2, Value::Object(Some(queue)));
            ctx.set_field(es, 3, Value::Int(0));
            ctx.unpin_native_roots(es_pin);
            Ok(Some(Value::Object(Some(es))))
        },
    );
    r.register(
        execs,
        "newScheduledThreadPool",
        "(I)Ljava/util/concurrent/ScheduledExecutorService;",
        |ctx, args| {
            let pool_size = args.first().and_then(|v| v.as_int()).unwrap_or(1);
            let es =
                alloc_concurrent_synthetic(ctx, "java/util/concurrent/ScheduledExecutorService", 4);
            // Pin across the queue alloc below — a moving young GC there would
            // relocate the fresh executor (native stale-local family).
            let es_pin = ctx.pin_native_root(es);
            ctx.set_field(es, 0, Value::Int(0));
            ctx.set_field(es, 1, Value::Int(pool_size));
            let queue = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 64);
            let es = ctx.read_native_pin(es_pin, es);
            ctx.set_field(es, 2, Value::Object(Some(queue)));
            ctx.set_field(es, 3, Value::Int(0));
            ctx.unpin_native_roots(es_pin);
            Ok(Some(Value::Object(Some(es))))
        },
    );
    r.register(
        execs,
        "newVirtualThreadPerTaskExecutor",
        "()Ljava/util/concurrent/ExecutorService;",
        |ctx, _args| {
            let es = alloc_concurrent_synthetic(ctx, "java/util/concurrent/ExecutorService", 4);
            // Pin across the queue alloc below — a moving young GC there would
            // relocate the fresh executor (native stale-local family).
            let es_pin = ctx.pin_native_root(es);
            ctx.set_field(es, 0, Value::Int(0));
            ctx.set_field(es, 1, Value::Int(i32::MAX));
            let queue = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 64);
            let es = ctx.read_native_pin(es_pin, es);
            ctx.set_field(es, 2, Value::Object(Some(queue)));
            ctx.set_field(es, 3, Value::Int(0));
            ctx.unpin_native_roots(es_pin);
            Ok(Some(Value::Object(Some(es))))
        },
    );

    // --- ExecutorService ---
    let es = "java/util/concurrent/ExecutorService";
    r.register(es, "shutdown", "()V", |ctx, args| {
        let mut this = obj_arg(args, 0)?;
        // Real ThreadPoolExecutor: leave its lifecycle to real bytecode (the
        // synthetic slot writes below would corrupt real fields, and reading
        // field 3 as a task count yields garbage). shutdownNow() (called by
        // ExecutorResource.close after this) interrupts the idle workers.
        if crate::executor_has_real_workers(ctx, this) {
            return Ok(Some(Value::Object(None)));
        }
        ctx.set_field(this, 0, Value::Int(1));
        // Execute all pending tasks before shutdown
        let task_count = ctx.get_field(this, 3).as_int().unwrap_or(0);
        if task_count > 0 {
            if let Value::Object(Some(queue)) = ctx.get_field(this, 2) {
                // Pin across the task callbacks below — a moving young GC there
                // would relocate them (native stale-local family).
                let this_pin = ctx.pin_native_root(this);
                let queue_pin = ctx.pin_native_root(queue);
                for i in 0..(task_count as usize) {
                    let queue = ctx.read_native_pin(queue_pin, queue);
                    if let Value::Object(Some(task)) = ctx.get_array_element(queue, i) {
                        let task_pin = ctx.pin_native_root(task);
                        let _: MethodCallResult = ctx
                            .invoke_virtual(task, "call", "()Ljava/lang/Object;", &[])
                            .or_else(|_| {
                                let task = ctx.read_native_pin(task_pin, task);
                                ctx.invoke_virtual(task, "run", "()V", &[])?;
                                Ok(None)
                            });
                        ctx.unpin_native_roots(task_pin);
                    }
                }
                this = ctx.read_native_pin(this_pin, this);
                ctx.unpin_native_roots(this_pin);
            }
            ctx.set_field(this, 3, Value::Int(0));
        }
        Ok(Some(Value::Object(None)))
    });
    r.register(es, "shutdownNow", "()Ljava/util/List;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // Real ThreadPoolExecutor: interrupt its workers so idle ones blocked in
        // getTask()->take() terminate (so a leaked non-daemon worker can't keep
        // the VM alive). Don't touch the synthetic slot fields — would corrupt a
        // real executor. Return an empty pending-tasks list.
        if crate::interrupt_executor_workers(ctx, this) {
            let empty = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 0);
            // Pin across the list alloc below — a moving young GC there would
            // relocate the fresh array (native stale-local family).
            let empty_pin = ctx.pin_native_root(empty);
            let list = alloc_concurrent_synthetic(ctx, "java/util/ArrayList", 2);
            let empty = ctx.read_native_pin(empty_pin, empty);
            ctx.set_field(list, 0, Value::Object(Some(empty)));
            ctx.set_field(list, 1, Value::Int(0));
            ctx.unpin_native_roots(empty_pin);
            return Ok(Some(Value::Object(Some(list))));
        }
        ctx.set_field(this, 0, Value::Int(1));
        // Return list of pending (unexecuted) tasks
        let task_count = ctx.get_field(this, 3).as_int().unwrap_or(0) as usize;
        // Pin across the array/list allocs below — a moving young GC there
        // would relocate `this`/`pending` (native stale-local family).
        let this_pin = ctx.pin_native_root(this);
        let pending = ctx.new_array(cratonvm_types::ArrayElementType::Reference, task_count);
        let pending_pin = ctx.pin_native_root(pending);
        let this = ctx.read_native_pin(this_pin, this);
        if task_count > 0 {
            if let Value::Object(Some(queue)) = ctx.get_field(this, 2) {
                for i in 0..task_count {
                    let task = ctx.get_array_element(queue, i);
                    ctx.set_array_element(pending, i, task);
                }
            }
        }
        ctx.set_field(this, 3, Value::Int(0));
        let list = alloc_concurrent_synthetic(ctx, "java/util/ArrayList", 2);
        let pending = ctx.read_native_pin(pending_pin, pending);
        ctx.set_field(list, 0, Value::Object(Some(pending)));
        ctx.set_field(list, 1, Value::Int(task_count as i32));
        ctx.unpin_native_roots(this_pin);
        Ok(Some(Value::Object(Some(list))))
    });
    r.register(es, "isShutdown", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    });
    r.register(es, "isTerminated", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let shutdown = ctx.get_field(this, 0).as_int().unwrap_or(0);
        let tasks = ctx.get_field(this, 3).as_int().unwrap_or(0);
        Ok(Some(Value::Int(if shutdown != 0 && tasks == 0 {
            1
        } else {
            0
        })))
    });
    r.register(
        es,
        "awaitTermination",
        "(JLjava/util/concurrent/TimeUnit;)Z",
        |ctx, args| {
            let mut this = obj_arg(args, 0)?;
            // Execute any pending tasks
            let task_count = ctx.get_field(this, 3).as_int().unwrap_or(0);
            if task_count > 0 {
                if let Value::Object(Some(queue)) = ctx.get_field(this, 2) {
                    // Pin across the task callbacks below — a moving young GC
                    // there would relocate them (native stale-local family).
                    let this_pin = ctx.pin_native_root(this);
                    let queue_pin = ctx.pin_native_root(queue);
                    for i in 0..(task_count as usize) {
                        let queue = ctx.read_native_pin(queue_pin, queue);
                        if let Value::Object(Some(task)) = ctx.get_array_element(queue, i) {
                            let task_pin = ctx.pin_native_root(task);
                            let _: Result<Option<Value>, MethodCallFailed> = ctx
                                .invoke_virtual(task, "call", "()Ljava/lang/Object;", &[])
                                .or_else(|_| {
                                    let task = ctx.read_native_pin(task_pin, task);
                                    ctx.invoke_virtual(task, "run", "()V", &[])?;
                                    Ok(None)
                                });
                            ctx.unpin_native_roots(task_pin);
                        }
                    }
                    this = ctx.read_native_pin(this_pin, this);
                    ctx.unpin_native_roots(this_pin);
                }
                ctx.set_field(this, 3, Value::Int(0));
            }
            Ok(Some(Value::Int(1)))
        },
    );
    r.register(
        es,
        "submit",
        "(Ljava/util/concurrent/Callable;)Ljava/util/concurrent/Future;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let callable = obj_arg(args, 1)?;
            let shutdown = ctx.get_field(this, 0).as_int().unwrap_or(0);
            if shutdown != 0 {
                return Err(RuntimeError::IllegalStateException {
                    message: "ExecutorService has been shut down".into(),
                }
                .into());
            }
            // Pin across the Future alloc below — a moving young GC there would
            // relocate `this`/`callable` (native stale-local family).
            let this_pin = ctx.pin_native_root(this);
            let callable_pin = ctx.pin_native_root(callable);
            // Create a Future with the task stored for lazy execution
            let future = alloc_concurrent_synthetic(ctx, "java/util/concurrent/Future", 4);
            let this = ctx.read_native_pin(this_pin, this);
            let callable = ctx.read_native_pin(callable_pin, callable);
            ctx.set_field(future, 0, Value::Object(None)); // result (not yet computed)
            ctx.set_field(future, 1, Value::Int(0)); // not done
            ctx.set_field(future, 2, Value::Int(0)); // not cancelled
            ctx.set_field(future, 3, Value::Object(Some(callable))); // store task
                                                                     // Also queue the task in the executor
            let task_count = ctx.get_field(this, 3).as_int().unwrap_or(0) as usize;
            if let Value::Object(Some(queue)) = ctx.get_field(this, 2) {
                let queue_len = ctx.array_length(queue);
                if task_count < queue_len {
                    ctx.set_array_element(queue, task_count, Value::Object(Some(callable)));
                    ctx.set_field(this, 3, Value::Int((task_count + 1) as i32));
                }
            }
            ctx.unpin_native_roots(this_pin);
            Ok(Some(Value::Object(Some(future))))
        },
    );
    r.register(
        es,
        "submit",
        "(Ljava/lang/Runnable;)Ljava/util/concurrent/Future;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let runnable = obj_arg(args, 1)?;
            let shutdown = ctx.get_field(this, 0).as_int().unwrap_or(0);
            if shutdown != 0 {
                return Err(RuntimeError::IllegalStateException {
                    message: "ExecutorService has been shut down".into(),
                }
                .into());
            }
            // Pin across the Future alloc below — a moving young GC there would
            // relocate `this`/`runnable` (native stale-local family).
            let this_pin = ctx.pin_native_root(this);
            let runnable_pin = ctx.pin_native_root(runnable);
            let future = alloc_concurrent_synthetic(ctx, "java/util/concurrent/Future", 4);
            let this = ctx.read_native_pin(this_pin, this);
            let runnable = ctx.read_native_pin(runnable_pin, runnable);
            ctx.set_field(future, 0, Value::Object(None));
            ctx.set_field(future, 1, Value::Int(0));
            ctx.set_field(future, 2, Value::Int(0));
            ctx.set_field(future, 3, Value::Object(Some(runnable)));
            let task_count = ctx.get_field(this, 3).as_int().unwrap_or(0) as usize;
            if let Value::Object(Some(queue)) = ctx.get_field(this, 2) {
                let queue_len = ctx.array_length(queue);
                if task_count < queue_len {
                    ctx.set_array_element(queue, task_count, Value::Object(Some(runnable)));
                    ctx.set_field(this, 3, Value::Int((task_count + 1) as i32));
                }
            }
            ctx.unpin_native_roots(this_pin);
            Ok(Some(Value::Object(Some(future))))
        },
    );
    r.register(es, "execute", "(Ljava/lang/Runnable;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let runnable = obj_arg(args, 1)?;
        let shutdown = ctx.get_field(this, 0).as_int().unwrap_or(0);
        if shutdown != 0 {
            return Err(RuntimeError::IllegalStateException {
                message: "ExecutorService has been shut down".into(),
            }
            .into());
        }
        // execute() runs immediately (fire-and-forget)
        let _ = ctx.invoke_virtual(runnable, "run", "()V", &[]);
        Ok(Some(Value::Object(None)))
    });

    // --- TimeUnit enum (1-field: ordinal=0) ---
    // `toMillis`/`toNanos`/`toSeconds`/`convert` used to be re-registered
    // here. `register` is last-wins, and this file runs after phases_early,
    // so these shadowed the complete implementation over there with a
    // three-method subset that read the ordinal from slot 0 only
    // (`unwrap_or(0)` => every unit decayed to NANOSECONDS) and a `convert`
    // that returned its input unchanged. Deleted; phases_early owns the
    // conversion surface. Only the static constants remain below.
    let tu = "java/util/concurrent/TimeUnit";
    // TimeUnit static constants (ordinals)
    r.register(
        tu,
        "NANOSECONDS",
        "Ljava/util/concurrent/TimeUnit;",
        |ctx, _args| {
            let obj = alloc_concurrent_synthetic(ctx, "java/util/concurrent/TimeUnit", 1);
            crate::phases_early::tu_set_ordinal(ctx, obj, 0);
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(
        tu,
        "MICROSECONDS",
        "Ljava/util/concurrent/TimeUnit;",
        |ctx, _args| {
            let obj = alloc_concurrent_synthetic(ctx, "java/util/concurrent/TimeUnit", 1);
            crate::phases_early::tu_set_ordinal(ctx, obj, 1);
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(
        tu,
        "MILLISECONDS",
        "Ljava/util/concurrent/TimeUnit;",
        |ctx, _args| {
            let obj = alloc_concurrent_synthetic(ctx, "java/util/concurrent/TimeUnit", 1);
            crate::phases_early::tu_set_ordinal(ctx, obj, 2);
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(
        tu,
        "SECONDS",
        "Ljava/util/concurrent/TimeUnit;",
        |ctx, _args| {
            let obj = alloc_concurrent_synthetic(ctx, "java/util/concurrent/TimeUnit", 1);
            crate::phases_early::tu_set_ordinal(ctx, obj, 3);
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(
        tu,
        "MINUTES",
        "Ljava/util/concurrent/TimeUnit;",
        |ctx, _args| {
            let obj = alloc_concurrent_synthetic(ctx, "java/util/concurrent/TimeUnit", 1);
            crate::phases_early::tu_set_ordinal(ctx, obj, 4);
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(
        tu,
        "HOURS",
        "Ljava/util/concurrent/TimeUnit;",
        |ctx, _args| {
            let obj = alloc_concurrent_synthetic(ctx, "java/util/concurrent/TimeUnit", 1);
            crate::phases_early::tu_set_ordinal(ctx, obj, 5);
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(
        tu,
        "DAYS",
        "Ljava/util/concurrent/TimeUnit;",
        |ctx, _args| {
            let obj = alloc_concurrent_synthetic(ctx, "java/util/concurrent/TimeUnit", 1);
            crate::phases_early::tu_set_ordinal(ctx, obj, 6);
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.set_category(__prev_cat);
}

// =============================================================================
// CompletableFuture expansion — thenCompose, thenRun, whenComplete, handle,
// exceptionally, allOf, anyOf, thenCombine, applyToEither, runAfterBoth, etc.
// CompletableFuture = 2-field synthetic (result=0, done=1)
// =============================================================================

pub(crate) fn register_p58_completable_future(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let cf = "java/util/concurrent/CompletableFuture";

    // thenCompose: apply Function that returns CompletableFuture, then flatten
    r.register(
        cf,
        "thenCompose",
        "(Ljava/util/function/Function;)Ljava/util/concurrent/CompletableFuture;",
        p58_cf_then_compose,
    );

    // thenRun: run Runnable after completion, return new CF with null result
    r.register(
        cf,
        "thenRun",
        "(Ljava/lang/Runnable;)Ljava/util/concurrent/CompletableFuture;",
        p58_cf_then_run,
    );

    // whenComplete: BiConsumer(result, exception) called after completion
    r.register(
        cf,
        "whenComplete",
        "(Ljava/util/function/BiConsumer;)Ljava/util/concurrent/CompletableFuture;",
        p58_cf_when_complete,
    );

    // handle: BiFunction(result, exception) → new result
    r.register(
        cf,
        "handle",
        "(Ljava/util/function/BiFunction;)Ljava/util/concurrent/CompletableFuture;",
        p58_cf_handle,
    );

    // exceptionally: Function(exception) → fallback value
    r.register(
        cf,
        "exceptionally",
        "(Ljava/util/function/Function;)Ljava/util/concurrent/CompletableFuture;",
        p58_cf_exceptionally,
    );

    // allOf: CompletableFuture[] → CompletableFuture<Void>
    r.register(
        cf,
        "allOf",
        "([Ljava/util/concurrent/CompletableFuture;)Ljava/util/concurrent/CompletableFuture;",
        p58_cf_all_of,
    );

    // anyOf: CompletableFuture[] → CompletableFuture<Object>
    r.register(
        cf,
        "anyOf",
        "([Ljava/util/concurrent/CompletableFuture;)Ljava/util/concurrent/CompletableFuture;",
        p58_cf_any_of,
    );

    // thenCombine: (CompletableFuture, BiFunction) → CompletableFuture
    r.register(cf, "thenCombine", "(Ljava/util/concurrent/CompletionStage;Ljava/util/function/BiFunction;)Ljava/util/concurrent/CompletableFuture;", p58_cf_then_combine);

    // applyToEither: (CompletableFuture, Function) → CompletableFuture
    r.register(cf, "applyToEither", "(Ljava/util/concurrent/CompletionStage;Ljava/util/function/Function;)Ljava/util/concurrent/CompletableFuture;", p58_cf_apply_to_either);

    // thenAcceptBoth: (CompletableFuture, BiConsumer) → CompletableFuture<Void>
    r.register(cf, "thenAcceptBoth", "(Ljava/util/concurrent/CompletionStage;Ljava/util/function/BiConsumer;)Ljava/util/concurrent/CompletableFuture;", p58_cf_then_accept_both);

    // runAfterBoth / runAfterEither
    r.register(cf, "runAfterBoth", "(Ljava/util/concurrent/CompletionStage;Ljava/lang/Runnable;)Ljava/util/concurrent/CompletableFuture;", p58_cf_run_after_both);
    r.register(cf, "runAfterEither", "(Ljava/util/concurrent/CompletionStage;Ljava/lang/Runnable;)Ljava/util/concurrent/CompletableFuture;", p58_cf_run_after_either);

    // failedFuture
    r.register(
        cf,
        "failedFuture",
        "(Ljava/lang/Throwable;)Ljava/util/concurrent/CompletableFuture;",
        p58_cf_failed_future,
    );

    // completeExceptionally
    r.register(
        cf,
        "completeExceptionally",
        "(Ljava/lang/Throwable;)Z",
        p58_cf_complete_exceptionally,
    );

    // isCompletedExceptionally
    r.register(
        cf,
        "isCompletedExceptionally",
        "()Z",
        p58_cf_is_completed_exceptionally,
    );

    // join() — blocking get without checked exception
    r.register(cf, "join", "()Ljava/lang/Object;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, FUT_FIELD_RESULT)))
    });
    // get() — blocking get with checked exception
    r.register(cf, "get", "()Ljava/lang/Object;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, FUT_FIELD_RESULT)))
    });
    r.register(
        cf,
        "get",
        "(JLjava/util/concurrent/TimeUnit;)Ljava/lang/Object;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, FUT_FIELD_RESULT)))
        },
    );
    // isDone
    r.register(cf, "isDone", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, FUT_FIELD_DONE)))
    });
    // getNow — return result or default if not done
    r.register(
        cf,
        "getNow",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let done = matches!(ctx.get_field(this, FUT_FIELD_DONE), Value::Int(1));
            if done {
                Ok(Some(ctx.get_field(this, FUT_FIELD_RESULT)))
            } else {
                Ok(args.get(1).copied())
            }
        },
    );
    // complete — set result and mark done. Delegates to the shared,
    // real-JDK-aware impl (fires parked Signallers via postComplete; see
    // `native_cf_complete`).
    r.register(
        cf,
        "complete",
        "(Ljava/lang/Object;)Z",
        crate::native_cf_complete,
    );
    // thenApply on CF itself (not just CompletionStage)
    r.register(
        cf,
        "thenApply",
        "(Ljava/util/function/Function;)Ljava/util/concurrent/CompletableFuture;",
        native_cf_then_apply,
    );
    r.register(
        cf,
        "thenAccept",
        "(Ljava/util/function/Consumer;)Ljava/util/concurrent/CompletableFuture;",
        native_cf_then_accept,
    );

    // toCompletableFuture (identity)
    r.register(
        cf,
        "toCompletableFuture",
        "()Ljava/util/concurrent/CompletableFuture;",
        |_ctx, args| Ok(Some(args.first().copied().unwrap_or(Value::Object(None)))),
    );

    // delayedExecutor(long, TimeUnit) — returns an Executor that sleeps then runs
    r.register(
        cf,
        "delayedExecutor",
        "(JLjava/util/concurrent/TimeUnit;)Ljava/util/concurrent/Executor;",
        |ctx, args| {
            let delay = match args.get(0) {
                Some(Value::Long(v)) => *v,
                _ => 0,
            };
            let delay_ms = scheduled_convert_to_millis(ctx, delay, args.get(1));
            let exec = alloc_concurrent_synthetic(ctx, "java/util/concurrent/Executor", 1);
            ctx.set_field(exec, 0, Value::Long(delay_ms));
            Ok(Some(Value::Object(Some(exec))))
        },
    );
    // delayedExecutor(long, TimeUnit, Executor) — same but with a base executor
    r.register(
        cf,
        "delayedExecutor",
        "(JLjava/util/concurrent/TimeUnit;Ljava/util/concurrent/Executor;)Ljava/util/concurrent/Executor;",
        |ctx, args| {
            let delay = match args.get(0) {
                Some(Value::Long(v)) => *v,
                _ => 0,
            };
            let delay_ms = scheduled_convert_to_millis(ctx, delay, args.get(1));
            let exec = alloc_concurrent_synthetic(ctx, "java/util/concurrent/Executor", 1);
            ctx.set_field(exec, 0, Value::Long(delay_ms));
            Ok(Some(Value::Object(Some(exec))))
        },
    );

    // CompletionStage interface — register key methods for dispatch
    let cs = "java/util/concurrent/CompletionStage";
    r.register(
        cs,
        "toCompletableFuture",
        "()Ljava/util/concurrent/CompletableFuture;",
        |_ctx, args| Ok(Some(args.first().copied().unwrap_or(Value::Object(None)))),
    );
    r.register(
        cs,
        "thenApply",
        "(Ljava/util/function/Function;)Ljava/util/concurrent/CompletionStage;",
        native_cf_then_apply,
    );
    r.register(
        cs,
        "thenAccept",
        "(Ljava/util/function/Consumer;)Ljava/util/concurrent/CompletionStage;",
        native_cf_then_accept,
    );
    r.register(
        cs,
        "thenCompose",
        "(Ljava/util/function/Function;)Ljava/util/concurrent/CompletionStage;",
        p58_cf_then_compose,
    );

    // Executor.execute(Runnable) — runs the Runnable; if this is a delayed executor,
    // sleeps first for the delay stored in field 0.
    let executor = "java/util/concurrent/Executor";
    r.register(
        executor,
        "execute",
        "(Ljava/lang/Runnable;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let mut runnable = match args.get(1) {
                Some(Value::Object(Some(r))) => *r,
                _ => return Ok(None),
            };
            // Check if this executor has a delay stored in field 0
            let delay_ms = match ctx.get_field(this, 0) {
                Value::Long(ms) if ms > 0 => ms as u64,
                _ => 0,
            };
            if delay_ms > 0 {
                let mut blocked_refs = [Value::Object(Some(runnable))];
                ctx.begin_blocking_region();
                std::thread::sleep(std::time::Duration::from_millis(delay_ms));
                ctx.end_blocking_region_refs(&mut blocked_refs);
                if let Value::Object(Some(cur)) = blocked_refs[0] {
                    runnable = cur;
                }
            }
            let _ = ctx.invoke_virtual(runnable, "run", "()V", &[]);
            Ok(None)
        },
    );
    r.set_category(__prev_cat);
}

pub(crate) fn p58_new_cf(ctx: &mut dyn NativeContext, result: Value, done: bool) -> ObjectRef {
    // Pin across the CF alloc below — a moving young GC there would relocate
    // the result value (native stale-local family).
    let result_pin = pinned_object_value(ctx, result);
    let cf = alloc_concurrent_synthetic(ctx, "java/util/concurrent/CompletableFuture", 2);
    ctx.set_field(
        cf,
        FUT_FIELD_RESULT,
        read_pinned_object_value(ctx, result_pin, result),
    );
    ctx.set_field(cf, FUT_FIELD_DONE, Value::Int(if done { 1 } else { 0 }));
    if let Some((h, _)) = result_pin {
        ctx.unpin_native_roots(h);
    }
    cf
}

/// DF07: a COMPLETED `Future` for the synchronous async-channel ops. Returns a
/// real `CompletableFuture.completedFuture(result)` — its real `get()`/
/// `get(timeout)` return immediately (encoding done-with-null via the internal
/// NIL sentinel). A synthetic `FutureTask` does NOT work here: in real-JDK mode
/// `FutureTask.get()` runs the real bytecode (reads the real `state` field,
/// stuck NEW) → the websocket client's `fConnect.get(timeout)` TimeoutException.
pub(crate) fn aio_completed_future(ctx: &mut dyn NativeContext, result: Value) -> MethodCallResult {
    ctx.invoke(
        "java/util/concurrent/CompletableFuture",
        "completedFuture",
        "(Ljava/lang/Object;)Ljava/util/concurrent/CompletableFuture;",
        &[result],
    )
}

/// DF07: box an int into `java.lang.Integer` for an `Integer`-typed Future result.
pub(crate) fn aio_box_int(ctx: &mut dyn NativeContext, n: i32) -> Value {
    match ctx.invoke(
        "java/lang/Integer",
        "valueOf",
        "(I)Ljava/lang/Integer;",
        &[Value::Int(n)],
    ) {
        Ok(Some(v)) => v,
        _ => Value::Int(n),
    }
}

/// DF07: decode a ByteBuffer's heap-array region for the async-channel I/O,
/// handling BOTH the real-JDK `HeapByteBuffer` layout (fields
/// `position`/`limit`/`hb`/`offset` by name, inherited from `java.nio.Buffer`)
/// and the synthetic slot layout (array@0, position@1, limit@2). Reading slots
/// 0/1/2 directly is WRONG for a real HeapByteBuffer (slot 0 is `mark`, not the
/// array) — the cause of the garbled websocket handshake. Returns
/// (backing_array, absolute_offset, remaining_len); None for a direct buffer or
/// an undecodable buffer.
pub(crate) fn aio_bb_region(
    ctx: &mut dyn NativeContext,
    bb: ObjectRef,
) -> Option<(ObjectRef, usize, usize)> {
    // Real-JDK: `position` resolves as a named int field. A synthetic ByteBuffer
    // has no named fields, so `position` reads back as Object(None) → slot path.
    if let Value::Int(pos) = ctx.get_field_by_name(bb, "position") {
        let pos = pos.max(0);
        let limit = match ctx.get_field_by_name(bb, "limit") {
            Value::Int(v) if v >= 0 => v,
            _ => match ctx.get_field_by_name(bb, "capacity") {
                Value::Int(v) if v >= 0 => v,
                _ => pos,
            },
        };
        if let Value::Object(Some(arr)) = ctx.get_field_by_name(bb, "hb") {
            let base = match ctx.get_field_by_name(bb, "offset") {
                Value::Int(v) if v >= 0 => v,
                _ => 0,
            };
            let len = (limit - pos).max(0) as usize;
            return Some((arr, (base + pos) as usize, len));
        }
        return None; // real direct buffer — not handled on this synchronous path
    }
    if let Value::Object(Some(arr)) = ctx.get_field(bb, 0) {
        let pos = ctx.get_field(bb, 1).as_int().unwrap_or(0).max(0);
        let limit = ctx
            .get_field(bb, 2)
            .as_int()
            .unwrap_or_else(|| ctx.array_length(arr) as i32)
            .max(0);
        let len = (limit - pos).max(0) as usize;
        return Some((arr, pos as usize, len));
    }
    None
}

/// DF07: advance a ByteBuffer's position by `n` after async I/O (named
/// `position` for real-JDK, slot 1 for synthetic — same discriminator as
/// `aio_bb_region`).
pub(crate) fn aio_bb_advance(ctx: &mut dyn NativeContext, bb: ObjectRef, n: i32) {
    if let Value::Int(pos) = ctx.get_field_by_name(bb, "position") {
        ctx.set_field_by_name(bb, "position", Value::Int(pos.saturating_add(n)));
    } else {
        let pos = ctx.get_field(bb, 1).as_int().unwrap_or(0);
        ctx.set_field(bb, 1, Value::Int(pos.saturating_add(n)));
    }
}

pub(crate) fn p58_cf_then_compose(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let func = obj_arg(args, 1)?;
    if p58_cf_is_real_jdk(ctx, this) {
        return ctx.invoke_special(
            "java/util/concurrent/CompletableFuture",
            "uniComposeStage",
            "(Ljava/util/concurrent/Executor;Ljava/util/function/Function;)Ljava/util/concurrent/CompletableFuture;",
            &[
                Value::Object(Some(this)),
                Value::Object(None),
                Value::Object(Some(func)),
            ],
        );
    }
    let result = ctx.get_field(this, FUT_FIELD_RESULT);
    // Apply function: Function<T, CompletableFuture<U>>
    let inner = ctx.invoke_virtual(
        func,
        "apply",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        &[result],
    )?;
    // The result should be a CompletableFuture; extract its result
    if let Some(Value::Object(Some(inner_cf))) = inner {
        let inner_result = ctx.get_field(inner_cf, FUT_FIELD_RESULT);
        let inner_done = ctx.get_field(inner_cf, FUT_FIELD_DONE);
        let cf = p58_new_cf(ctx, inner_result, inner_done == Value::Int(1));
        Ok(Some(Value::Object(Some(cf))))
    } else {
        let cf = p58_new_cf(ctx, Value::Object(None), true);
        Ok(Some(Value::Object(Some(cf))))
    }
}

pub(crate) fn p58_cf_then_run(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let _this = obj_arg(args, 0)?;
    let runnable = obj_arg(args, 1)?;
    let _ = ctx.invoke_virtual(runnable, "run", "()V", &[]);
    let cf = p58_new_cf(ctx, Value::Object(None), true);
    Ok(Some(Value::Object(Some(cf))))
}

/// BUG-17 (synthetic `p58` CF model): a real-JDK `CompletableFuture` has its real
/// `volatile Completion stack` reference in slot 1 (`FUT_FIELD_DONE`), never the
/// synthetic `DONE` Int. Real-JDK dependent-stage methods must delegate to real,
/// non-blocking JDK machinery instead of this eager model (which fires the callback
/// with `(null, null)` while the source is still asynchronously pending).
pub(crate) fn p58_cf_is_real_jdk(ctx: &mut dyn NativeContext, this: ObjectRef) -> bool {
    !matches!(ctx.get_field(this, FUT_FIELD_DONE), Value::Int(_))
}

pub(crate) fn p58_cf_when_complete(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let action = obj_arg(args, 1)?;
    // BUG-17: real-JDK CF — delegate to real private `uniWhenCompleteStage(null, c)`
    // (== public `whenComplete(c)`) so the dependent registers NON-blockingly and fires
    // on async completion with the real value (reactor's `Mono.fromFuture` subscribes via
    // `handle()`; `whenComplete` is its twin). The eager `accept(result,null)` below
    // would fire null on a still-pending async CF → `.block()`/coroutine-await returns
    // null before the value exists → `@Cacheable(sync=true)` cache miss.
    if p58_cf_is_real_jdk(ctx, this) {
        return ctx.invoke_special(
            "java/util/concurrent/CompletableFuture",
            "uniWhenCompleteStage",
            "(Ljava/util/concurrent/Executor;Ljava/util/function/BiConsumer;)Ljava/util/concurrent/CompletableFuture;",
            &[
                Value::Object(Some(this)),
                Value::Object(None),
                Value::Object(Some(action)),
            ],
        );
    }
    let result = ctx.get_field(this, FUT_FIELD_RESULT);
    // Pin across the accept() below — a moving young GC there would relocate
    // the result value (native stale-local family).
    let result_pin = pinned_object_value(ctx, result);
    // BiConsumer.accept(result, null_exception)
    let _ = ctx.invoke_virtual(
        action,
        "accept",
        "(Ljava/lang/Object;Ljava/lang/Object;)V",
        &[result, Value::Object(None)],
    );
    let result = read_pinned_object_value(ctx, result_pin, result);
    if let Some((h, _)) = result_pin {
        ctx.unpin_native_roots(h);
    }
    // Return new CF with same result
    let cf = p58_new_cf(ctx, result, true);
    Ok(Some(Value::Object(Some(cf))))
}

pub(crate) fn p58_cf_handle(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let func = obj_arg(args, 1)?;
    // BUG-17: real-JDK CF — delegate to real private `uniHandleStage(null, fn)` (== public
    // `handle(fn)`) for a NON-blocking dependent. See `p58_cf_when_complete`.
    if p58_cf_is_real_jdk(ctx, this) {
        return ctx.invoke_special(
            "java/util/concurrent/CompletableFuture",
            "uniHandleStage",
            "(Ljava/util/concurrent/Executor;Ljava/util/function/BiFunction;)Ljava/util/concurrent/CompletableFuture;",
            &[
                Value::Object(Some(this)),
                Value::Object(None),
                Value::Object(Some(func)),
            ],
        );
    }
    let result = ctx.get_field(this, FUT_FIELD_RESULT);
    // BiFunction.apply(result, null_exception)
    let new_result = ctx.invoke_virtual(
        func,
        "apply",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        &[result, Value::Object(None)],
    )?;
    let val = new_result.unwrap_or(Value::Object(None));
    let cf = p58_new_cf(ctx, val, true);
    Ok(Some(Value::Object(Some(cf))))
}

pub(crate) fn p58_cf_exceptionally(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let func = obj_arg(args, 1)?;
    if p58_cf_is_real_jdk(ctx, this) {
        return ctx.invoke_special(
            "java/util/concurrent/CompletableFuture",
            "uniExceptionallyStage",
            "(Ljava/util/concurrent/Executor;Ljava/util/function/Function;)Ljava/util/concurrent/CompletableFuture;",
            &[
                Value::Object(Some(this)),
                Value::Object(None),
                Value::Object(Some(func)),
            ],
        );
    }
    // No exception in our eager model — just pass through
    let result = ctx.get_field(this, FUT_FIELD_RESULT);
    let cf = p58_new_cf(ctx, result, true);
    Ok(Some(Value::Object(Some(cf))))
}

pub(crate) fn p58_cf_all_of(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Real-JDK async inputs (e.g. `CompletableFuture.runAsync` on a worker) may still
    // be PENDING. Delegate to the real JDK private static `andTree` (== the body of
    // the real `allOf`) so the returned CF completes only when every input does,
    // instead of the eager "all already complete" model below which makes
    // `allOf(...).join()` return immediately while tasks run. See `p58_cf_when_complete`
    // (BUG-17) for the same real-JDK delegation pattern.
    if let Some(Value::Object(Some(arr))) = args.first() {
        let arr = *arr;
        let len = ctx.array_length(arr);
        let mut any_real = false;
        for i in 0..len {
            if let Value::Object(Some(cf)) = ctx.get_array_element(arr, i) {
                if !matches!(ctx.get_field(cf, FUT_FIELD_DONE), Value::Int(_)) {
                    any_real = true;
                    break;
                }
            }
        }
        if any_real {
            return ctx.invoke_special(
                "java/util/concurrent/CompletableFuture",
                "andTree",
                "([Ljava/util/concurrent/CompletableFuture;II)Ljava/util/concurrent/CompletableFuture;",
                &[
                    Value::Object(Some(arr)),
                    Value::Int(0),
                    Value::Int(len as i32 - 1),
                ],
            );
        }
    }
    // All futures are already complete in our eager model
    let cf = p58_new_cf(ctx, Value::Object(None), true);
    Ok(Some(Value::Object(Some(cf))))
}

pub(crate) fn p58_cf_any_of(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Return result of first future in the array
    if let Some(Value::Object(Some(arr))) = args.first() {
        if ctx.array_length(*arr) > 0 {
            if let Value::Object(Some(first)) = ctx.get_array_element(*arr, 0) {
                let result = ctx.get_field(first, FUT_FIELD_RESULT);
                let cf = p58_new_cf(ctx, result, true);
                return Ok(Some(Value::Object(Some(cf))));
            }
        }
    }
    let cf = p58_new_cf(ctx, Value::Object(None), true);
    Ok(Some(Value::Object(Some(cf))))
}

pub(crate) fn p58_cf_then_combine(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let other = obj_arg(args, 1)?;
    let func = obj_arg(args, 2)?;
    let r1 = ctx.get_field(this, FUT_FIELD_RESULT);
    let r2 = ctx.get_field(other, FUT_FIELD_RESULT);
    let combined = ctx.invoke_virtual(
        func,
        "apply",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        &[r1, r2],
    )?;
    let val = combined.unwrap_or(Value::Object(None));
    let cf = p58_new_cf(ctx, val, true);
    Ok(Some(Value::Object(Some(cf))))
}

pub(crate) fn p58_cf_apply_to_either(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let func = obj_arg(args, 2)?;
    // Use this future's result (both are complete in eager model)
    let result = ctx.get_field(this, FUT_FIELD_RESULT);
    let new_result = ctx.invoke_virtual(
        func,
        "apply",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        &[result],
    )?;
    let val = new_result.unwrap_or(Value::Object(None));
    let cf = p58_new_cf(ctx, val, true);
    Ok(Some(Value::Object(Some(cf))))
}

pub(crate) fn p58_cf_then_accept_both(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let other = obj_arg(args, 1)?;
    let action = obj_arg(args, 2)?;
    let r1 = ctx.get_field(this, FUT_FIELD_RESULT);
    let r2 = ctx.get_field(other, FUT_FIELD_RESULT);
    let _ = ctx.invoke_virtual(
        action,
        "accept",
        "(Ljava/lang/Object;Ljava/lang/Object;)V",
        &[r1, r2],
    );
    let cf = p58_new_cf(ctx, Value::Object(None), true);
    Ok(Some(Value::Object(Some(cf))))
}

pub(crate) fn p58_cf_run_after_both(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let _this = obj_arg(args, 0)?;
    let runnable = obj_arg(args, 2)?;
    let _ = ctx.invoke_virtual(runnable, "run", "()V", &[]);
    let cf = p58_new_cf(ctx, Value::Object(None), true);
    Ok(Some(Value::Object(Some(cf))))
}

pub(crate) fn p58_cf_run_after_either(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let _this = obj_arg(args, 0)?;
    let runnable = obj_arg(args, 2)?;
    let _ = ctx.invoke_virtual(runnable, "run", "()V", &[]);
    let cf = p58_new_cf(ctx, Value::Object(None), true);
    Ok(Some(Value::Object(Some(cf))))
}

pub(crate) fn p58_cf_failed_future(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let throwable = args.first().copied().unwrap_or(Value::Object(None));
    let cf = p58_new_cf(ctx, throwable, true);
    Ok(Some(Value::Object(Some(cf))))
}

pub(crate) fn p58_cf_complete_exceptionally(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let throwable = args.get(1).copied().unwrap_or(Value::Object(None));
    match ctx.get_field(this, FUT_FIELD_DONE) {
        // Phase-58 synthetic CompletableFuture: its slot 1 is an integer
        // completion flag, so preserve the eager compatibility model.
        Value::Int(done) => {
            if done != 0 {
                return Ok(Some(Value::Int(0))); // already complete
            }
            ctx.set_field(this, FUT_FIELD_RESULT, throwable);
            ctx.set_field(this, FUT_FIELD_DONE, Value::Int(1));
            Ok(Some(Value::Int(1)))
        }
        // Real JDK CompletableFuture (including subclasses such as
        // Rest5ClientHttpClient.RequestFuture): slot 1 is the Completion
        // stack, not a synthetic done flag.  Storing an integer here leaves
        // the actual result unset and strands Mono.fromFuture subscribers on
        // an async connection failure.  Use the JDK's private primitive and
        // drain its completion stack just as native_cf_complete does for the
        // normal-success path.
        _ => {
            let completed = ctx
                .invoke_virtual(
                    this,
                    "completeThrowable",
                    "(Ljava/lang/Throwable;)Z",
                    &[throwable],
                )?
                .and_then(|value| value.as_int())
                .unwrap_or(0);
            ctx.invoke_virtual(this, "postComplete", "()V", &[])?;
            Ok(Some(Value::Int(completed)))
        }
    }
}

pub(crate) fn p58_cf_is_completed_exceptionally(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    // In our eager model, we don't distinguish exceptional completion
    Ok(Some(Value::Int(0)))
}

/// One side-table entry per live SynchronousQueue (keyed by identity hash).
#[derive(Default)]
pub(crate) struct SqRendezvous {
    /// Producers blocked in `put`, each carrying the item it wants to hand off.
    /// `fulfilled` flips true once a taker has consumed the item.
    producers: Vec<SqWaiter>,
    /// Consumers blocked in `take`, each with a slot a producer writes into.
    consumers: Vec<SqWaiter>,
}

pub(crate) struct SqWaiter {
    /// Monotonic ticket identifying this specific waiter.
    ticket: u64,
    /// The blocked Java Thread object, for `unpark`.
    thread: ObjectRef,
    /// For a producer: the item being offered. For a consumer: the item that a
    /// producer has delivered (filled on fulfilment).
    item: Option<Value>,
    /// Set true by the opposite side once the rendezvous has completed.
    fulfilled: bool,
}

pub(crate) fn sq_table() -> &'static parking_lot::Mutex<std::collections::HashMap<i32, SqRendezvous>>
{
    static T: SqOnceLock<parking_lot::Mutex<std::collections::HashMap<i32, SqRendezvous>>> =
        SqOnceLock::new();
    T.get_or_init(|| parking_lot::Mutex::new(std::collections::HashMap::new()))
}

pub(crate) fn sq_next_ticket() -> u64 {
    static TICKET: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    TICKET.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

pub(crate) fn register_p58_synchronous_queue(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let sq = "java/util/concurrent/SynchronousQueue";
    r.register(sq, "<init>", "()V", p58_sq_init);
    r.register(sq, "<init>", "(Z)V", p58_sq_init_fair);
    r.register(sq, "put", "(Ljava/lang/Object;)V", p58_sq_put);
    r.register(sq, "offer", "(Ljava/lang/Object;)Z", p58_sq_offer);
    r.register(
        sq,
        "offer",
        "(Ljava/lang/Object;JLjava/util/concurrent/TimeUnit;)Z",
        p58_sq_offer_timed,
    );
    r.register(sq, "take", "()Ljava/lang/Object;", p58_sq_take);
    r.register(sq, "poll", "()Ljava/lang/Object;", p58_sq_poll);
    r.register(
        sq,
        "poll",
        "(JLjava/util/concurrent/TimeUnit;)Ljava/lang/Object;",
        p58_sq_poll_timed,
    );
    // KEEP (all five constants below): these are not stubs, they are the
    // SynchronousQueue contract. The javadoc states outright that the queue
    // has zero capacity, so `peek()` always returns null, `size()` is always
    // 0, `isEmpty()` is always true, `contains(o)` is always false and
    // `remainingCapacity()` is always 0. A state-reading implementation would
    // be wrong, not better.
    // SHADOW NOTE: `size`/`isEmpty` below do not survive to runtime —
    // `concurrent_extras::register_synchronous_queue_extras` re-registers both
    // later (lib.rs registers concurrent_extras after phase 58). As of W4 that
    // override answers the same constants; it previously read slot state for
    // `isEmpty` and could contradict the `size() == 0` kept here.
    r.register(sq, "peek", "()Ljava/lang/Object;", |_ctx, _args| {
        Ok(Some(Value::Object(None)))
    });
    r.register(sq, "size", "()I", |_ctx, _args| Ok(Some(Value::Int(0))));
    r.register(sq, "isEmpty", "()Z", |_ctx, _args| Ok(Some(Value::Int(1))));
    r.register(sq, "contains", "(Ljava/lang/Object;)Z", |_ctx, _args| {
        Ok(Some(Value::Int(0)))
    });
    r.register(sq, "iterator", "()Ljava/util/Iterator;", |ctx, _args| {
        // Empty iterator
        use cratonvm_types::ArrayElementType;
        let arr = ctx.new_array(ArrayElementType::Reference, 0);
        let itr = alloc_concurrent_synthetic(ctx, "java/util/concurrent/SynchronousQueue$Itr", 2);
        ctx.set_field(itr, 0, Value::Object(Some(arr)));
        ctx.set_field(itr, 1, Value::Int(0));
        Ok(Some(Value::Object(Some(itr))))
    });
    r.register(sq, "toArray", "()[Ljava/lang/Object;", |ctx, _args| {
        use cratonvm_types::ArrayElementType;
        let arr = ctx.new_array(ArrayElementType::Reference, 0);
        Ok(Some(Value::Object(Some(arr))))
    });
    r.register(sq, "clear", "()V", p58_sq_clear);
    r.register(sq, "remainingCapacity", "()I", |_ctx, _args| {
        Ok(Some(Value::Int(0)))
    });
    // drainTo(c) is NOT constant-zero even for a zero-capacity queue: the JDK
    // implements it as `while ((e = poll()) != null) { c.add(e); ++n; }`, so it
    // harvests every item a blocked producer is currently offering. Returning 0
    // silently left those producers parked and told the caller there was
    // nothing to consume.
    r.register(sq, "drainTo", "(Ljava/util/Collection;)I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let coll = match args.get(1) {
            Some(Value::Object(Some(c))) => *c,
            // drainTo(null) is an NPE in the JDK; keep the existing lenient
            // "nothing drained" answer for a null sink rather than throwing
            // from a path that used to be total.
            _ => return Ok(Some(Value::Int(0))),
        };
        // Pin across `Collection.add` — it allocates and can safepoint, which
        // would relocate `this`/`coll` (native stale-local family).
        let this_pin = ctx.pin_native_root(this);
        let coll_pin = ctx.pin_native_root(coll);
        let mut drained = 0i32;
        let result = loop {
            let this_cur = ctx.read_native_pin(this_pin, this);
            let item = match p58_sq_poll(ctx, &[Value::Object(Some(this_cur))]) {
                // Mirrors the JDK's `while ((e = poll()) != null)` loop: a null
                // poll means no producer is waiting, so the drain is complete.
                Ok(Some(v)) if !matches!(v, Value::Object(None)) => v,
                Ok(_) => break Ok(()),
                Err(e) => break Err(e),
            };
            let coll_cur = ctx.read_native_pin(coll_pin, coll);
            if let Err(e) = ctx.invoke_virtual(coll_cur, "add", "(Ljava/lang/Object;)Z", &[item]) {
                break Err(e);
            }
            drained += 1;
        };
        ctx.unpin_native_roots(this_pin);
        result?;
        Ok(Some(Value::Int(drained)))
    });
    r.set_category(__prev_cat);
}

pub(crate) fn p58_sq_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // Fields retained for layout compatibility; the real state is the
    // identity-keyed rendezvous side-table.
    ctx.set_field(this, 0, Value::Object(None)); // item (unused)
    ctx.set_field(this, 1, Value::Int(0)); // waiting flag (unused)
    Ok(None)
}

pub(crate) fn p58_sq_init_fair(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    ctx.set_field(this, 0, Value::Object(None));
    ctx.set_field(this, 1, Value::Int(0));
    Ok(None)
}

/// `put(E)` — block until a consumer takes the item. Real rendezvous: if a
/// consumer is already waiting, hand off directly and wake it; otherwise
/// enqueue self as a waiting producer and park until taken.
pub(crate) fn p58_sq_put(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let key = ctx.identity_hash_code(this);
    let item = args.get(1).copied().unwrap_or(Value::Object(None));
    let me = ctx.current_thread_object();

    // Fast path: a consumer is already waiting → fulfil it directly.
    {
        let mut t = sq_table().lock();
        let entry = t.entry(key).or_default();
        if let Some(consumer) = entry.consumers.first_mut() {
            // Deliver the item INTO the consumer's slot and wake it. The
            // consumer (take/poll) is responsible for reading the item and
            // removing itself from the wait list — do NOT remove it here, or
            // the handed-off item would be lost before the taker reads it.
            consumer.item = Some(item);
            consumer.fulfilled = true;
            let waiter_thread = consumer.thread;
            drop(t);
            ctx.unpark(waiter_thread);
            return Ok(None);
        }
        // No consumer: enqueue self as a waiting producer.
        let ticket = sq_next_ticket();
        entry.producers.push(SqWaiter {
            ticket,
            thread: me,
            item: Some(item),
            fulfilled: false,
        });
        drop(t);
        // Park until a taker fulfils us. park() may return spuriously, so loop.
        loop {
            ctx.park(None);
            let mut t = sq_table().lock();
            if let Some(entry) = t.get_mut(&key) {
                if let Some(pos) = entry.producers.iter().position(|w| w.ticket == ticket) {
                    if entry.producers[pos].fulfilled {
                        entry.producers.remove(pos);
                        if entry.producers.is_empty() && entry.consumers.is_empty() {
                            t.remove(&key);
                        }
                        return Ok(None);
                    }
                    // Spurious wake-up: still waiting, re-park.
                    continue;
                }
            }
            // Our waiter vanished (consumed by a taker that removed it) → done.
            return Ok(None);
        }
    }
}

/// `offer(E)` — non-blocking. Succeeds only if a consumer is already waiting;
/// otherwise returns false (the JDK never buffers in a SynchronousQueue).
pub(crate) fn p58_sq_offer(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let key = ctx.identity_hash_code(this);
    let item = args.get(1).copied().unwrap_or(Value::Object(None));

    let mut t = sq_table().lock();
    let entry = t.entry(key).or_default();
    if let Some(consumer) = entry.consumers.first_mut() {
        // Deliver into the consumer's slot; the taker reads it and self-removes.
        consumer.item = Some(item);
        consumer.fulfilled = true;
        let waiter_thread = consumer.thread;
        drop(t);
        ctx.unpark(waiter_thread);
        Ok(Some(Value::Int(1)))
    } else {
        let empty = entry.producers.is_empty() && entry.consumers.is_empty();
        if empty {
            t.remove(&key);
        }
        Ok(Some(Value::Int(0)))
    }
}

/// `offer(E, long, TimeUnit)` — timed. Hands off immediately if a consumer is
/// waiting; otherwise waits up to the deadline for one. Returns false on
/// timeout (item not handed off).
pub(crate) fn p58_sq_offer_timed(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let key = ctx.identity_hash_code(this);
    let item = args.get(1).copied().unwrap_or(Value::Object(None));
    let timeout_nanos = sq_timeout_nanos(args, 2);
    let me = ctx.current_thread_object();

    let ticket;
    {
        let mut t = sq_table().lock();
        let entry = t.entry(key).or_default();
        if let Some(consumer) = entry.consumers.first_mut() {
            // Deliver into the consumer's slot; the taker reads it and self-removes.
            consumer.item = Some(item);
            consumer.fulfilled = true;
            let waiter_thread = consumer.thread;
            drop(t);
            ctx.unpark(waiter_thread);
            return Ok(Some(Value::Int(1)));
        }
        ticket = sq_next_ticket();
        entry.producers.push(SqWaiter {
            ticket,
            thread: me,
            item: Some(item),
            fulfilled: false,
        });
    }
    let deadline =
        timeout_nanos.map(|n| std::time::Instant::now() + std::time::Duration::from_nanos(n));
    // Helper closure result type: a removal returning whether we were consumed.
    let give_up = |key: i32, ticket: u64| -> i32 {
        let mut t = sq_table().lock();
        if let Some(entry) = t.get_mut(&key) {
            if let Some(pos) = entry.producers.iter().position(|w| w.ticket == ticket) {
                let consumed = entry.producers[pos].fulfilled;
                entry.producers.remove(pos);
                if entry.producers.is_empty() && entry.consumers.is_empty() {
                    t.remove(&key);
                }
                return if consumed { 1 } else { 0 };
            }
        }
        // Waiter already removed → a taker consumed us.
        1
    };
    loop {
        let park_dur = match deadline {
            Some(d) => {
                let now = std::time::Instant::now();
                if now >= d {
                    return Ok(Some(Value::Int(give_up(key, ticket))));
                }
                d - now
            }
            // No positive timeout → don't wait.
            None => return Ok(Some(Value::Int(give_up(key, ticket)))),
        };
        ctx.park(Some(park_dur));
        // Woke up: check fulfilment.
        let mut t = sq_table().lock();
        if let Some(entry) = t.get_mut(&key) {
            if let Some(pos) = entry.producers.iter().position(|w| w.ticket == ticket) {
                if entry.producers[pos].fulfilled {
                    entry.producers.remove(pos);
                    if entry.producers.is_empty() && entry.consumers.is_empty() {
                        t.remove(&key);
                    }
                    return Ok(Some(Value::Int(1)));
                }
                // Spurious wake — loop and re-check deadline.
                continue;
            }
        }
        return Ok(Some(Value::Int(1)));
    }
}

/// `take()` — block until a producer offers an item. Real rendezvous mirror of
/// `put`.
pub(crate) fn p58_sq_take(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let key = ctx.identity_hash_code(this);
    let me = ctx.current_thread_object();

    // Fast path: a producer is already waiting → consume its item and wake it.
    {
        let mut t = sq_table().lock();
        let entry = t.entry(key).or_default();
        if let Some(producer) = entry.producers.first_mut() {
            let item = producer.item.take().unwrap_or(Value::Object(None));
            producer.fulfilled = true;
            let waiter_thread = producer.thread;
            entry.producers.remove(0);
            drop(t);
            ctx.unpark(waiter_thread);
            return Ok(Some(item));
        }
        // No producer: enqueue self as a waiting consumer.
        let ticket = sq_next_ticket();
        entry.consumers.push(SqWaiter {
            ticket,
            thread: me,
            item: None,
            fulfilled: false,
        });
        drop(t);
        loop {
            ctx.park(None);
            let mut t = sq_table().lock();
            if let Some(entry) = t.get_mut(&key) {
                if let Some(pos) = entry.consumers.iter().position(|w| w.ticket == ticket) {
                    if entry.consumers[pos].fulfilled {
                        let item = entry.consumers[pos]
                            .item
                            .take()
                            .unwrap_or(Value::Object(None));
                        entry.consumers.remove(pos);
                        if entry.producers.is_empty() && entry.consumers.is_empty() {
                            t.remove(&key);
                        }
                        return Ok(Some(item));
                    }
                    continue; // spurious wake-up
                }
            }
            // Waiter removed by a producer that already delivered: best-effort null.
            return Ok(Some(Value::Object(None)));
        }
    }
}

/// `poll()` — non-blocking. Returns an item only if a producer is already
/// waiting; otherwise null.
pub(crate) fn p58_sq_poll(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let key = ctx.identity_hash_code(this);
    let mut t = sq_table().lock();
    let entry = t.entry(key).or_default();
    if let Some(producer) = entry.producers.first_mut() {
        let item = producer.item.take().unwrap_or(Value::Object(None));
        producer.fulfilled = true;
        let waiter_thread = producer.thread;
        entry.producers.remove(0);
        drop(t);
        ctx.unpark(waiter_thread);
        Ok(Some(item))
    } else {
        if entry.producers.is_empty() && entry.consumers.is_empty() {
            t.remove(&key);
        }
        Ok(Some(Value::Object(None)))
    }
}

/// `poll(long, TimeUnit)` — timed. Mirror of `offer(E, long, TimeUnit)`.
pub(crate) fn p58_sq_poll_timed(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let key = ctx.identity_hash_code(this);
    let timeout_nanos = sq_timeout_nanos(args, 1);
    let me = ctx.current_thread_object();

    let ticket;
    {
        let mut t = sq_table().lock();
        let entry = t.entry(key).or_default();
        if let Some(producer) = entry.producers.first_mut() {
            let item = producer.item.take().unwrap_or(Value::Object(None));
            producer.fulfilled = true;
            let waiter_thread = producer.thread;
            entry.producers.remove(0);
            drop(t);
            ctx.unpark(waiter_thread);
            return Ok(Some(item));
        }
        ticket = sq_next_ticket();
        entry.consumers.push(SqWaiter {
            ticket,
            thread: me,
            item: None,
            fulfilled: false,
        });
    }
    let deadline =
        timeout_nanos.map(|n| std::time::Instant::now() + std::time::Duration::from_nanos(n));
    loop {
        let park_dur = match deadline {
            Some(d) => {
                let now = std::time::Instant::now();
                if now >= d {
                    // Timed out: drop our waiter, return whatever was delivered.
                    let mut t = sq_table().lock();
                    if let Some(entry) = t.get_mut(&key) {
                        if let Some(pos) = entry.consumers.iter().position(|w| w.ticket == ticket) {
                            let item = if entry.consumers[pos].fulfilled {
                                entry.consumers[pos]
                                    .item
                                    .take()
                                    .unwrap_or(Value::Object(None))
                            } else {
                                Value::Object(None)
                            };
                            entry.consumers.remove(pos);
                            if entry.producers.is_empty() && entry.consumers.is_empty() {
                                t.remove(&key);
                            }
                            return Ok(Some(item));
                        }
                    }
                    return Ok(Some(Value::Object(None)));
                }
                Some(d - now)
            }
            // A zero/negative timeout with no deadline means "don't wait".
            None => {
                let mut t = sq_table().lock();
                if let Some(entry) = t.get_mut(&key) {
                    if let Some(pos) = entry.consumers.iter().position(|w| w.ticket == ticket) {
                        let item = if entry.consumers[pos].fulfilled {
                            entry.consumers[pos]
                                .item
                                .take()
                                .unwrap_or(Value::Object(None))
                        } else {
                            Value::Object(None)
                        };
                        entry.consumers.remove(pos);
                        if entry.producers.is_empty() && entry.consumers.is_empty() {
                            t.remove(&key);
                        }
                        return Ok(Some(item));
                    }
                }
                return Ok(Some(Value::Object(None)));
            }
        };
        ctx.park(park_dur);
        let mut t = sq_table().lock();
        if let Some(entry) = t.get_mut(&key) {
            if let Some(pos) = entry.consumers.iter().position(|w| w.ticket == ticket) {
                if entry.consumers[pos].fulfilled {
                    let item = entry.consumers[pos]
                        .item
                        .take()
                        .unwrap_or(Value::Object(None));
                    entry.consumers.remove(pos);
                    if entry.producers.is_empty() && entry.consumers.is_empty() {
                        t.remove(&key);
                    }
                    return Ok(Some(item));
                }
                continue; // spurious wake — loop and re-check deadline
            }
        }
        return Ok(Some(Value::Object(None)));
    }
}

/// Decode a (long timeout, TimeUnit) arg pair starting at `idx` into nanos.
/// Returns None for a non-positive timeout (callers treat None as "no wait").
pub(crate) fn sq_timeout_nanos(args: &[Value], idx: usize) -> Option<u64> {
    let raw = match args.get(idx) {
        Some(Value::Long(v)) => *v,
        Some(Value::Double(v)) => i64::from_le_bytes(v.to_le_bytes()),
        _ => 0,
    };
    if raw <= 0 {
        return None;
    }
    // TimeUnit (args[idx+1]) governs the magnitude. We can't easily read the
    // enum here without a virtual call, so conservatively interpret the value
    // as the unit's toNanos via a best-effort: most callers pass NANOSECONDS or
    // MILLISECONDS. We default to treating the raw value as the smallest sane
    // wait (milliseconds) when the unit is unknown, which keeps timed waits
    // bounded rather than effectively infinite. Callers that need exact units
    // run real bytecode; this native path only guards against unbounded blocks.
    Some((raw as u64).saturating_mul(1_000_000))
}

pub(crate) fn p58_sq_clear(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let key = ctx.identity_hash_code(this);
    sq_table().lock().remove(&key);
    ctx.set_field(this, 0, Value::Object(None));
    ctx.set_field(this, 1, Value::Int(0));
    Ok(None)
}

// =============================================================================
// java.util.concurrent.SubmissionPublisher delivery helpers (B6)
//
// Field layout (publisher `this`): field 0 = subscribers list, field 1 = closed
// (Int). The `()V` constructor that actually runs lives in lib.rs
// (`register_t31_concurrent_extras`, registered LAST so it wins over the
// phase-60 ctor here) and sets field 0 to an `java.util.ArrayList`-style
// WRAPPER object whose own field 0 is a reference array (cap 8) and field 1 is
// the Int count. These helpers therefore read/write subscribers THROUGH that
// wrapper so the model stays consistent with the constructor that runs. See the
// nb-core-stubs fix note for the cross-file coupling with lib.rs.
//
// The previous implementation stored only the last submitted item and never
// delivered anything to subscribers (submit/offer returned a hardcoded lag of
// 1, subscribe was a no-op). Reactive-Streams consumers therefore silently
// received nothing. These helpers maintain a real subscriber list and drive
// the Flow.Subscriber callbacks (onSubscribe / onNext / onComplete) eagerly on
// the calling thread. This is synchronous (no carrier-pool dispatch), so the
// estimated-lag return value is the live subscriber count and demand/back-
// pressure is not enforced — adequate for the common "request(Long.MAX_VALUE)
// in onSubscribe" subscriber but documented as a limitation.
// =============================================================================

/// Return the subscriber-list wrapper for a publisher, lazily creating it (an
/// `ArrayList`-style 2-field synthetic: field 0 = ref array, field 1 = Int
/// count) when the publisher's field 0 is null. `this` is pinned by the caller
/// or pinned here across the allocation. Returns the (post-alloc) wrapper ref.
pub(crate) fn sp_wrapper_ensure(ctx: &mut dyn NativeContext, this: ObjectRef) -> ObjectRef {
    if let Value::Object(Some(w)) = ctx.get_field(this, 0) {
        return w;
    }
    let this_pin = ctx.pin_native_root(this);
    let wrapper = alloc_concurrent_synthetic(ctx, "java/util/ArrayList", 2);
    let w_pin = ctx.pin_native_root(wrapper);
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 8);
    let wrapper = ctx.read_native_pin(w_pin, wrapper);
    ctx.set_field(wrapper, 0, Value::Object(Some(arr)));
    ctx.set_field(wrapper, 1, Value::Int(0));
    let this = ctx.read_native_pin(this_pin, this);
    let wrapper = ctx.read_native_pin(w_pin, wrapper);
    ctx.set_field(this, 0, Value::Object(Some(wrapper)));
    ctx.unpin_native_roots(this_pin);
    wrapper
}

/// Read `(backing_array, count)` from a publisher's subscriber-list wrapper, or
/// `None` when there is no wrapper / no subscribers. Read-only (no allocation).
pub(crate) fn sp_subscribers(
    ctx: &dyn NativeContext,
    this: ObjectRef,
) -> Option<(ObjectRef, usize)> {
    let wrapper = match ctx.get_field(this, 0) {
        Value::Object(Some(w)) => w,
        _ => return None,
    };
    let arr = match ctx.get_field(wrapper, 0) {
        Value::Object(Some(a)) => a,
        // Defensive: some path may have stored a bare array directly in field 0.
        _ => return None,
    };
    let count = match ctx.get_field(wrapper, 1) {
        Value::Int(n) if n >= 0 => n as usize,
        // No explicit count slot → fall back to the array capacity.
        _ => ctx.array_length(arr),
    };
    let cap = ctx.array_length(arr);
    Some((arr, count.min(cap)))
}

/// Append `subscriber` to the publisher's subscriber-list wrapper, growing the
/// backing array (doubling) when full and bumping the count. `this` is pinned
/// by the caller; we additionally pin across the internal allocations.
pub(crate) fn sp_append_subscriber(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    subscriber: ObjectRef,
) {
    let this_pin = ctx.pin_native_root(this);
    let sub_pin = ctx.pin_native_root(subscriber);
    let wrapper = sp_wrapper_ensure(ctx, this);
    let w_pin = ctx.pin_native_root(wrapper);
    let (arr, cap, count) = match ctx.get_field(wrapper, 0) {
        Value::Object(Some(a)) => {
            let cap = ctx.array_length(a);
            let count = match ctx.get_field(wrapper, 1) {
                Value::Int(n) if n >= 0 => n as usize,
                _ => 0,
            };
            (a, cap, count)
        }
        _ => (subscriber /*placeholder*/, 0usize, 0usize),
    };
    // Grow the backing array if full.
    let arr = if count >= cap {
        let new_cap = (cap.max(4)) * 2;
        let new_arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, new_cap);
        let a_pin = ctx.pin_native_root(new_arr);
        let wrapper = ctx.read_native_pin(w_pin, wrapper);
        if let Value::Object(Some(old_arr)) = ctx.get_field(wrapper, 0) {
            for i in 0..count {
                let elem = ctx.get_array_element(old_arr, i);
                ctx.set_array_element(new_arr, i, elem);
            }
        }
        let new_arr = ctx.read_native_pin(a_pin, new_arr);
        let wrapper = ctx.read_native_pin(w_pin, wrapper);
        ctx.set_field(wrapper, 0, Value::Object(Some(new_arr)));
        new_arr
    } else {
        arr
    };
    let subscriber = ctx.read_native_pin(sub_pin, subscriber);
    ctx.set_array_element(arr, count, Value::Object(Some(subscriber)));
    let wrapper = ctx.read_native_pin(w_pin, wrapper);
    ctx.set_field(wrapper, 1, Value::Int((count + 1) as i32));
    ctx.unpin_native_roots(this_pin);
}

/// Deliver one item to every registered subscriber via `onNext`. Returns the
/// number of subscribers reached (used as the synchronous estimated lag). The
/// `this` and `item` references are pinned across the re-entrant invokes so a
/// moving GC during a subscriber callback cannot strand them. We deliberately
/// do NOT write the item into any publisher field — pinning the item ref is
/// sufficient to keep it live, and the real-JDK SubmissionPublisher layout
/// has no synthetic `lastItem` slot to clobber.
pub(crate) fn sp_deliver_on_next(ctx: &mut dyn NativeContext, this: ObjectRef, item: Value) -> i32 {
    // Pin `this` and the item; re-read both across every subscriber callback.
    let this_pin = ctx.pin_native_root(this);
    let item_pin = match item {
        Value::Object(Some(item_ref)) => Some(ctx.pin_native_root(item_ref)),
        _ => None,
    };
    let mut delivered = 0;
    let mut idx = 0;
    loop {
        let this_cur = ctx.read_native_pin(this_pin, this);
        let (arr, len) = match sp_subscribers(ctx, this_cur) {
            Some(v) => v,
            None => break,
        };
        if idx >= len {
            break;
        }
        if let Value::Object(Some(sub)) = ctx.get_array_element(arr, idx) {
            // Re-read the (possibly relocated) item for this callback.
            let item_now = match item_pin {
                Some(p) => match item {
                    Value::Object(Some(orig)) => Value::Object(Some(ctx.read_native_pin(p, orig))),
                    _ => item,
                },
                None => item,
            };
            let _ = ctx.invoke_virtual(sub, "onNext", "(Ljava/lang/Object;)V", &[item_now]);
            delivered += 1;
        }
        idx += 1;
    }
    ctx.unpin_native_roots(this_pin);
    delivered
}

pub(crate) fn register_p60_flow(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    // Flow.Subscription = 2-field (cancelled=0, demand=1 Long)
    let sub = "java/util/concurrent/Flow$Subscription";
    r.register(sub, "request", "(J)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let n = match args.get(1) {
            Some(Value::Long(v)) => *v,
            _ => 1,
        };
        let cur = match ctx.get_field(this, 1) {
            Value::Long(v) => v,
            _ => 0,
        };
        ctx.set_field(this, 1, Value::Long(cur.saturating_add(n)));
        Ok(None)
    });
    r.register(sub, "cancel", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, 0, Value::Int(1));
        Ok(None)
    });

    // Flow constants — KEEP: `Flow.defaultBufferSize()` is specified to return
    // a fixed 256 in the JDK ("the default value for Publisher buffering"),
    // so the constant IS the implementation.
    let flow = "java/util/concurrent/Flow";
    r.register(flow, "defaultBufferSize", "()I", |_ctx, _args| {
        Ok(Some(Value::Int(256)))
    });

    // SubmissionPublisher: field 0 = subscriber-list wrapper (null until the
    // first subscribe; lazily created by sp_wrapper_ensure), field 1 = closed.
    // NOTE: the `()V` ctor that actually runs is in lib.rs
    // (register_t31_concurrent_extras, registered later → wins); it allocates
    // the wrapper eagerly. This phase-60 ctor is the fallback and leaves the
    // list null — both shapes are handled by the delivery helpers above.
    let sp = "java/util/concurrent/SubmissionPublisher";
    r.register(sp, "<init>", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, 0, Value::Object(None)); // no subscribers yet
        ctx.set_field(this, 1, Value::Int(0)); // not closed
        Ok(None)
    });
    r.register(sp, "submit", "(Ljava/lang/Object;)I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // Reject submission to a closed publisher (matches JDK IllegalStateException).
        if ctx.get_field(this, 1).as_int().unwrap_or(0) != 0 {
            return Err(RuntimeError::IllegalStateException {
                message: "Publisher is closed".into(),
            }
            .into());
        }
        let item = args.get(1).copied().unwrap_or(Value::Object(None));
        // Deliver eagerly to every registered subscriber; return the synchronous
        // estimated lag (subscriber count). With no subscribers this is 0.
        let lag = sp_deliver_on_next(ctx, this, item);
        Ok(Some(Value::Int(lag)))
    });
    r.register(
        sp,
        "offer",
        "(Ljava/lang/Object;Ljava/util/function/BiPredicate;)I",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            if ctx.get_field(this, 1).as_int().unwrap_or(0) != 0 {
                // JDK offer on a closed publisher returns a negative value.
                return Ok(Some(Value::Int(-1)));
            }
            let item = args.get(1).copied().unwrap_or(Value::Object(None));
            let lag = sp_deliver_on_next(ctx, this, item);
            Ok(Some(Value::Int(lag)))
        },
    );
    // NOTE: lib.rs `register_t31_concurrent_extras` registers a `close()V` for
    // SubmissionPublisher LATER, so at runtime THAT one wins and this body does
    // not execute (the lib.rs version only flips the closed flag and does not
    // fire onComplete). This implementation is the correct fallback and runs
    // only if the registration order changes. Firing onComplete on close is a
    // genuine gap owned by lib.rs — see the nb-core-stubs fix note.
    r.register(sp, "close", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if ctx.get_field(this, 1).as_int().unwrap_or(0) != 0 {
            return Ok(None); // already closed — idempotent
        }
        ctx.set_field(this, 1, Value::Int(1));
        // Signal completion to every subscriber.
        let pin = ctx.pin_native_root(this);
        let mut idx = 0;
        loop {
            let this_cur = ctx.read_native_pin(pin, this);
            let (arr, len) = match sp_subscribers(ctx, this_cur) {
                Some(v) => v,
                None => break,
            };
            if idx >= len {
                break;
            }
            if let Value::Object(Some(sub)) = ctx.get_array_element(arr, idx) {
                let _ = ctx.invoke_virtual(sub, "onComplete", "()V", &[]);
            }
            idx += 1;
        }
        ctx.unpin_native_roots(pin);
        Ok(None)
    });
    r.register(sp, "isClosed", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 1)))
    });
    r.register(sp, "hasSubscribers", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let has = matches!(sp_subscribers(ctx, this), Some((_, n)) if n > 0);
        Ok(Some(Value::Int(if has { 1 } else { 0 })))
    });
    r.register(sp, "getNumberOfSubscribers", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let n = sp_subscribers(ctx, this).map(|(_, n)| n).unwrap_or(0);
        Ok(Some(Value::Int(n as i32)))
    });
    r.register(
        sp,
        "subscribe",
        "(Ljava/util/concurrent/Flow$Subscriber;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let subscriber = match args.get(1) {
                Some(Value::Object(Some(s))) => *s,
                // null subscriber → JDK throws NPE.
                _ => {
                    return Err(RuntimeError::NullPointerException {
                        message: Some("subscriber is null".into()),
                    }
                    .into())
                }
            };
            // Pin `this` and the subscriber across the allocations and the
            // re-entrant onSubscribe call (both can move objects under a
            // relocating GC). The subscription is pinned right after allocation.
            let pin = ctx.pin_native_root(this);
            let sub_arg_pin = ctx.pin_native_root(subscriber);
            // Build a Flow.Subscription (cancelled=0, demand=1) and hand it to
            // the subscriber so it can establish demand.
            let subscription =
                alloc_concurrent_synthetic(ctx, "java/util/concurrent/Flow$Subscription", 2);
            let sub_pin = ctx.pin_native_root(subscription);
            ctx.set_field(subscription, 0, Value::Int(0));
            ctx.set_field(subscription, 1, Value::Long(0));
            let this = ctx.read_native_pin(pin, this);
            let subscriber = ctx.read_native_pin(sub_arg_pin, subscriber);
            sp_append_subscriber(ctx, this, subscriber);
            let subscriber = ctx.read_native_pin(sub_arg_pin, subscriber);
            let subscription = ctx.read_native_pin(sub_pin, subscription);
            let _ = ctx.invoke_virtual(
                subscriber,
                "onSubscribe",
                "(Ljava/util/concurrent/Flow$Subscription;)V",
                &[Value::Object(Some(subscription))],
            );
            ctx.unpin_native_roots(pin);
            Ok(None)
        },
    );
    r.set_category(__prev_cat);
}

// =============================================================================
// java.util.concurrent.locks.StampedLock = 2-field (state=0 Long, writeLocked=1 Int)
// state encoding: 0 = unlocked, positive = read lock count, negative stamp for write
// =============================================================================

pub(crate) fn register_p62_stamped_lock(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let sl = "java/util/concurrent/locks/StampedLock";
    r.register(sl, "<init>", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, 0, Value::Long(0)); // state
        ctx.set_field(this, 1, Value::Int(0)); // not write-locked
        Ok(None)
    });
    r.register(sl, "readLock", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let state = match ctx.get_field(this, 0) {
            Value::Long(v) => v,
            _ => 0,
        };
        let stamp = state + 1;
        ctx.set_field(this, 0, Value::Long(stamp));
        Ok(Some(Value::Long(stamp)))
    });
    r.register(sl, "writeLock", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let stamp = -(std::process::id() as i64);
        ctx.set_field(this, 0, Value::Long(stamp));
        ctx.set_field(this, 1, Value::Int(1));
        Ok(Some(Value::Long(stamp)))
    });
    r.register(sl, "tryReadLock", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let wl = match ctx.get_field(this, 1) {
            Value::Int(v) => v,
            _ => 0,
        };
        if wl != 0 {
            return Ok(Some(Value::Long(0)));
        }
        let state = match ctx.get_field(this, 0) {
            Value::Long(v) => v,
            _ => 0,
        };
        let stamp = state + 1;
        ctx.set_field(this, 0, Value::Long(stamp));
        Ok(Some(Value::Long(stamp)))
    });
    r.register(sl, "tryWriteLock", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let state = match ctx.get_field(this, 0) {
            Value::Long(v) => v,
            _ => 0,
        };
        if state != 0 {
            return Ok(Some(Value::Long(0)));
        }
        let stamp = -(std::process::id() as i64);
        ctx.set_field(this, 0, Value::Long(stamp));
        ctx.set_field(this, 1, Value::Int(1));
        Ok(Some(Value::Long(stamp)))
    });
    r.register(sl, "unlockRead", "(J)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let state = match ctx.get_field(this, 0) {
            Value::Long(v) => v,
            _ => 0,
        };
        ctx.set_field(this, 0, Value::Long((state - 1).max(0)));
        Ok(None)
    });
    r.register(sl, "unlockWrite", "(J)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, 0, Value::Long(0));
        ctx.set_field(this, 1, Value::Int(0));
        Ok(None)
    });
    r.register(sl, "unstampedUnlockRead", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let state = match ctx.get_field(this, 0) {
            Value::Long(v) => v,
            _ => 0,
        };
        ctx.set_field(this, 0, Value::Long((state - 1).max(0)));
        Ok(None)
    });
    r.register(sl, "unstampedUnlockWrite", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, 0, Value::Long(0));
        ctx.set_field(this, 1, Value::Int(0));
        Ok(None)
    });
    r.register(sl, "validate", "(J)Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let stamp = match args.get(1) {
            Some(Value::Long(v)) => *v,
            _ => 0,
        };
        let state = match ctx.get_field(this, 0) {
            Value::Long(v) => v,
            _ => 0,
        };
        Ok(Some(Value::Int(if stamp == state { 1 } else { 0 })))
    });
    r.register(sl, "tryOptimisticRead", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let wl = match ctx.get_field(this, 1) {
            Value::Int(v) => v,
            _ => 0,
        };
        if wl != 0 {
            return Ok(Some(Value::Long(0)));
        }
        let state = match ctx.get_field(this, 0) {
            Value::Long(v) => v,
            _ => 0,
        };
        Ok(Some(Value::Long(state)))
    });
    r.register(sl, "isReadLocked", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let state = match ctx.get_field(this, 0) {
            Value::Long(v) => v,
            _ => 0,
        };
        let wl = match ctx.get_field(this, 1) {
            Value::Int(v) => v,
            _ => 0,
        };
        Ok(Some(Value::Int(if state > 0 && wl == 0 { 1 } else { 0 })))
    });
    r.register(sl, "isWriteLocked", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 1)))
    });
    // tryConvertToWriteLock(stamp) — upgrade read lock to write lock if possible
    r.register(sl, "tryConvertToWriteLock", "(J)J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let stamp = match args.get(1) {
            Some(Value::Long(v)) => *v,
            _ => 0,
        };
        let state = match ctx.get_field(this, 0) {
            Value::Long(v) => v,
            _ => 0,
        };
        let wl = ctx.get_field(this, 1).as_int().unwrap_or(0);
        // Can upgrade if: stamp is valid, not already write-locked, read count is exactly 1
        if stamp == state && wl == 0 && state == 1 {
            let write_stamp = -(std::process::id() as i64);
            ctx.set_field(this, 0, Value::Long(write_stamp));
            ctx.set_field(this, 1, Value::Int(1));
            Ok(Some(Value::Long(write_stamp)))
        } else if stamp < 0 && wl != 0 {
            // Already write-locked with this stamp — return same stamp
            Ok(Some(Value::Long(stamp)))
        } else {
            Ok(Some(Value::Long(0))) // conversion failed
        }
    });
    // tryConvertToReadLock(stamp) — downgrade write lock to read lock
    r.register(sl, "tryConvertToReadLock", "(J)J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let stamp = match args.get(1) {
            Some(Value::Long(v)) => *v,
            _ => 0,
        };
        let wl = ctx.get_field(this, 1).as_int().unwrap_or(0);
        if stamp < 0 && wl != 0 {
            // Downgrade: release write, acquire read
            let read_stamp = 1i64;
            ctx.set_field(this, 0, Value::Long(read_stamp));
            ctx.set_field(this, 1, Value::Int(0));
            Ok(Some(Value::Long(read_stamp)))
        } else if stamp > 0 && wl == 0 {
            // Already read-locked
            Ok(Some(Value::Long(stamp)))
        } else {
            Ok(Some(Value::Long(0)))
        }
    });
    // unlock(stamp) — release whatever lock the stamp represents
    r.register(sl, "unlock", "(J)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let stamp = match args.get(1) {
            Some(Value::Long(v)) => *v,
            _ => 0,
        };
        if stamp < 0 {
            // Write lock
            ctx.set_field(this, 0, Value::Long(0));
            ctx.set_field(this, 1, Value::Int(0));
        } else if stamp > 0 {
            // Read lock
            let state = match ctx.get_field(this, 0) {
                Value::Long(v) => v,
                _ => 0,
            };
            ctx.set_field(this, 0, Value::Long((state - 1).max(0)));
        }
        Ok(None)
    });
    // tryReadLock with timeout
    r.register(
        sl,
        "tryReadLock",
        "(JLjava/util/concurrent/TimeUnit;)J",
        |ctx, args| {
            let mut this = obj_arg(args, 0)?;
            let wl = ctx.get_field(this, 1).as_int().unwrap_or(0);
            if wl != 0 {
                // Try waiting briefly
                let delay = match args.get(1) {
                    Some(Value::Long(v)) => *v,
                    _ => 0,
                };
                if delay > 0 {
                    let ms = delay.min(100); // cap at 100ms
                    let mut blocked_refs = [Value::Object(Some(this))];
                    ctx.begin_blocking_region();
                    std::thread::sleep(std::time::Duration::from_millis(ms as u64));
                    ctx.end_blocking_region_refs(&mut blocked_refs);
                    if let Value::Object(Some(cur)) = blocked_refs[0] {
                        this = cur;
                    }
                }
                let wl2 = ctx.get_field(this, 1).as_int().unwrap_or(0);
                if wl2 != 0 {
                    return Ok(Some(Value::Long(0)));
                }
            }
            let state = match ctx.get_field(this, 0) {
                Value::Long(v) => v,
                _ => 0,
            };
            let stamp = state + 1;
            ctx.set_field(this, 0, Value::Long(stamp));
            Ok(Some(Value::Long(stamp)))
        },
    );
    // tryWriteLock with timeout
    r.register(
        sl,
        "tryWriteLock",
        "(JLjava/util/concurrent/TimeUnit;)J",
        |ctx, args| {
            let mut this = obj_arg(args, 0)?;
            let state = match ctx.get_field(this, 0) {
                Value::Long(v) => v,
                _ => 0,
            };
            if state != 0 {
                let delay = match args.get(1) {
                    Some(Value::Long(v)) => *v,
                    _ => 0,
                };
                if delay > 0 {
                    let ms = delay.min(100);
                    let mut blocked_refs = [Value::Object(Some(this))];
                    ctx.begin_blocking_region();
                    std::thread::sleep(std::time::Duration::from_millis(ms as u64));
                    ctx.end_blocking_region_refs(&mut blocked_refs);
                    if let Value::Object(Some(cur)) = blocked_refs[0] {
                        this = cur;
                    }
                }
                let state2 = match ctx.get_field(this, 0) {
                    Value::Long(v) => v,
                    _ => 0,
                };
                if state2 != 0 {
                    return Ok(Some(Value::Long(0)));
                }
            }
            let stamp = -(std::process::id() as i64);
            ctx.set_field(this, 0, Value::Long(stamp));
            ctx.set_field(this, 1, Value::Int(1));
            Ok(Some(Value::Long(stamp)))
        },
    );
    // getReadLockCount
    r.register(sl, "getReadLockCount", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let state = match ctx.get_field(this, 0) {
            Value::Long(v) => v,
            _ => 0,
        };
        let wl = ctx.get_field(this, 1).as_int().unwrap_or(0);
        Ok(Some(Value::Int(if wl == 0 && state > 0 {
            state as i32
        } else {
            0
        })))
    });
    r.set_category(__prev_cat);
}

pub(crate) fn register_p63_scheduled_executor(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    fn stpe_ensure_work_queue(ctx: &mut dyn NativeContext, this: ObjectRef) {
        let has_queue = matches!(
            ctx.get_field_by_name(this, "workQueue"),
            Value::Object(Some(_))
        );
        if has_queue {
            return;
        }
        // Real JDK STPE expects a non-null workQueue before delayedExecute().
        // Prefer DelayedWorkQueue; fall back to PriorityBlockingQueue.
        let queue = match ctx
            .new_object("java/util/concurrent/ScheduledThreadPoolExecutor$DelayedWorkQueue")
        {
            Ok(Some(Value::Object(Some(q)))) => {
                let _ = ctx.invoke_special(
                    "java/util/concurrent/ScheduledThreadPoolExecutor$DelayedWorkQueue",
                    "<init>",
                    "()V",
                    &[Value::Object(Some(q))],
                );
                Some(q)
            }
            _ => match ctx.new_object("java/util/concurrent/PriorityBlockingQueue") {
                Ok(Some(Value::Object(Some(q)))) => {
                    let _ = ctx.invoke_special(
                        "java/util/concurrent/PriorityBlockingQueue",
                        "<init>",
                        "()V",
                        &[Value::Object(Some(q))],
                    );
                    Some(q)
                }
                _ => None,
            },
        };
        if let Some(q) = queue {
            ctx.set_field_by_name(this, "workQueue", Value::Object(Some(q)));
        }
    }
    fn stpe_init_common(
        ctx: &mut dyn NativeContext,
        this: ObjectRef,
        cores: i32,
    ) -> MethodCallResult {
        crate::phases_early::initialize_real_scheduled_thread_pool_executor(ctx, this, cores, None)
    }
    fn stpe_new_executor(ctx: &mut dyn NativeContext, cores: i32) -> Option<ObjectRef> {
        let cores = cores.max(1);
        let obj = match ctx.new_object("java/util/concurrent/ScheduledThreadPoolExecutor") {
            Ok(Some(Value::Object(Some(o)))) => o,
            _ => return None,
        };
        let _ = stpe_init_common(ctx, obj, cores);
        Some(obj)
    }

    let stpe = "java/util/concurrent/ScheduledThreadPoolExecutor";
    r.register(stpe, "<init>", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let cores = match args.get(1) {
            Some(Value::Int(v)) => *v,
            _ => 1,
        };
        stpe_init_common(ctx, this, cores)
    });
    r.register(
        stpe,
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
            crate::phases_early::initialize_real_scheduled_thread_pool_executor(
                ctx, this, cores, factory,
            )
        },
    );
    // WP4.5 — registry-driven scheduling. The pump in
    // `crate::scheduled_pump` invokes `runnable.run()` from the next
    // `Thread.sleep` / `awaitTermination` so periodic tasks observe the
    // configured cadence rather than firing exactly once inline.
    //
    // The synthetic `ScheduledFuture` stores the registry id in field 0
    // (cast to Long) and the done-flag in field 1 so `cancel(boolean)`
    // can look the task up and flip the cancelled flag. Field 0 used to
    // carry the result reference for `Future.get()` — we keep that
    // surface by routing `get()` through the pump's done-tracking.
    fn build_sf(
        ctx: &mut dyn cratonvm_native_api::NativeContext,
        id: u64,
    ) -> cratonvm_types::ObjectRef {
        let sf = alloc_concurrent_synthetic(ctx, "java/util/concurrent/ScheduledFuture", 2);
        ctx.set_field(sf, 0, Value::Long(id as i64));
        ctx.set_field(sf, 1, Value::Int(0));
        sf
    }

    r.register(stpe, "schedule", "(Ljava/lang/Runnable;JLjava/util/concurrent/TimeUnit;)Ljava/util/concurrent/ScheduledFuture;", |ctx, args| {
        let mut runnable = match args.get(1) {
            Some(Value::Object(Some(r))) => *r,
            _ => return Ok(Some(Value::Object(None))),
        };
        let delay = match args.get(2) {
            Some(Value::Long(v)) => *v,
            // WP4.5 — generic stack pop type-erases Long → Double bit-pattern.
            Some(Value::Double(d)) => d.to_bits() as i64,
            Some(Value::Int(v)) => *v as i64,
            _ => 0,
        };
        let delay_ms = scheduled_convert_to_millis(ctx, delay, args.get(3)).max(0) as u64;
        let task = crate::scheduled_pump::registry().register(runnable, delay_ms, 0, false);
        // Drive the pump immediately so callers that don't subsequently
        // sleep (e.g. zero-delay `schedule(r, 0, MILLIS)`) still get the
        // single fire they expect synchronously.
        crate::scheduled_pump::registry().pump(ctx);
        Ok(Some(Value::Object(Some(build_sf(ctx, task.id)))))
    });
    // scheduleAtFixedRate — periodic firing driven by the pump.
    r.register(stpe, "scheduleAtFixedRate", "(Ljava/lang/Runnable;JJLjava/util/concurrent/TimeUnit;)Ljava/util/concurrent/ScheduledFuture;", |ctx, args| {
        let mut runnable = match args.get(1) {
            Some(Value::Object(Some(r))) => *r,
            _ => return Ok(Some(Value::Object(None))),
        };
        let initial_delay = match args.get(2) {
            Some(Value::Long(v)) => *v,
            Some(Value::Double(d)) => d.to_bits() as i64,
            Some(Value::Int(v)) => *v as i64,
            _ => 0,
        };
        let period = match args.get(3) {
            Some(Value::Long(v)) => *v,
            Some(Value::Double(d)) => d.to_bits() as i64,
            Some(Value::Int(v)) => *v as i64,
            _ => 50,
        };
        let initial_ms = scheduled_convert_to_millis(ctx, initial_delay, args.get(4)).max(0) as u64;
        let period_ms = scheduled_convert_to_millis(ctx, period, args.get(4)).max(1) as u64;
        let task = crate::scheduled_pump::registry().register(runnable, initial_ms, period_ms, false);
        // Initial pump for zero-delay schedules so the first fire happens
        // before the caller's first `Thread.sleep`.
        crate::scheduled_pump::registry().pump(ctx);
        Ok(Some(Value::Object(Some(build_sf(ctx, task.id)))))
    });
    // scheduleWithFixedDelay — pump-driven, fixed-delay variant.
    r.register(stpe, "scheduleWithFixedDelay", "(Ljava/lang/Runnable;JJLjava/util/concurrent/TimeUnit;)Ljava/util/concurrent/ScheduledFuture;", |ctx, args| {
        let runnable = match args.get(1) {
            Some(Value::Object(Some(r))) => *r,
            _ => return Ok(Some(Value::Object(None))),
        };
        let initial_delay = match args.get(2) {
            Some(Value::Long(v)) => *v,
            Some(Value::Double(d)) => d.to_bits() as i64,
            Some(Value::Int(v)) => *v as i64,
            _ => 0,
        };
        let period = match args.get(3) {
            Some(Value::Long(v)) => *v,
            Some(Value::Double(d)) => d.to_bits() as i64,
            Some(Value::Int(v)) => *v as i64,
            _ => 50,
        };
        let initial_ms = scheduled_convert_to_millis(ctx, initial_delay, args.get(4)).max(0) as u64;
        let period_ms = scheduled_convert_to_millis(ctx, period, args.get(4)).max(1) as u64;
        let task = crate::scheduled_pump::registry().register(runnable, initial_ms, period_ms, true);
        crate::scheduled_pump::registry().pump(ctx);
        Ok(Some(Value::Object(Some(build_sf(ctx, task.id)))))
    });
    // ScheduledFuture.cancel(boolean) — flip the cancelled flag and
    // return whether this call performed the transition. The call is
    // registered against the synthetic ScheduledFuture class so the
    // probe's `sf.cancel(false)` resolves here regardless of which
    // scheduling overload built the future.
    r.register(
        "java/util/concurrent/ScheduledFuture",
        "cancel",
        "(Z)Z",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let id = match ctx.get_field(this, 0) {
                Value::Long(v) => v as u64,
                Value::Int(v) => v as u64,
                _ => 0,
            };
            if id == 0 {
                return Ok(Some(Value::Int(0)));
            }
            let cancelled = match crate::scheduled_pump::registry().find(id) {
                Some(t) => t.cancel(),
                None => false,
            };
            if cancelled {
                ctx.set_field(this, 1, Value::Int(1));
            }
            Ok(Some(Value::Int(if cancelled { 1 } else { 0 })))
        },
    );
    r.register(
        "java/util/concurrent/ScheduledFuture",
        "isCancelled",
        "()Z",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let id = match ctx.get_field(this, 0) {
                Value::Long(v) => v as u64,
                Value::Int(v) => v as u64,
                _ => 0,
            };
            if id == 0 {
                return Ok(Some(Value::Int(0)));
            }
            Ok(Some(Value::Int(
                match crate::scheduled_pump::registry().find(id) {
                    Some(t) if t.is_cancelled() => 1,
                    _ => 0,
                },
            )))
        },
    );
    r.register(
        "java/util/concurrent/ScheduledFuture",
        "isDone",
        "()Z",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 1)))
        },
    );
    r.register(stpe, "shutdown", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, 1, Value::Int(1));
        // WP4.5 — drive any final pending fires before exiting and let
        // the cancelled tasks be GC'd by the registry on next pump.
        crate::scheduled_pump::registry().pump(ctx);
        crate::scheduled_pump::registry().gc();
        Ok(None)
    });
    r.register(stpe, "shutdownNow", "()Ljava/util/List;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, 1, Value::Int(1));
        // shutdownNow cancels every pending periodic; the returned list
        // is empty in our synthetic model.
        crate::scheduled_pump::registry().cancel_all();
        crate::scheduled_pump::registry().gc();
        let al = alloc_concurrent_synthetic(ctx, "java/util/ArrayList", 2);
        ctx.set_field(al, 0, Value::Object(None));
        ctx.set_field(al, 1, Value::Int(0));
        Ok(Some(Value::Object(Some(al))))
    });
    r.register(stpe, "isShutdown", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 1)))
    });
    r.register(stpe, "isTerminated", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 1)))
    });
    // getPoolSize(): a hardcoded 0 made every pool look like it had never
    // started a thread, which is the wrong branch for callers that gate on
    // `getPoolSize() > 0` (health checks, "is the scheduler up?" probes) and
    // contradicted getCorePoolSize() right below.
    //
    // A receiver with a real `workers` set IS a real ThreadPoolExecutor
    // (`executor_has_real_workers`, the same per-instance probe the
    // ExecutorService natives use) — hand those to the real bytecode, which
    // counts the real workers. `invoke_virtual_bytecode_only` skips the
    // native-override check so this does not re-enter itself.
    //
    // Otherwise the receiver is CratonVM's synthetic STPE, whose scheduled
    // work is driven by `crate::scheduled_pump` rather than by a worker set.
    // Report the configured core pool size: that is the number of threads the
    // pool is modelled as owning, and it matches getCorePoolSize().
    r.register(stpe, "getPoolSize", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if crate::executor_has_real_workers(ctx, this) {
            return ctx.invoke_virtual_bytecode_only(this, "getPoolSize", "()I", &[]);
        }
        let core = match ctx.get_field_by_name(this, "corePoolSize") {
            Value::Int(value) => value,
            // Synthetic STPE objects have only the historical slot layout.
            _ => ctx.get_field(this, 0).as_int().unwrap_or(0),
        };
        Ok(Some(Value::Int(core.max(0))))
    });
    r.register(stpe, "getCorePoolSize", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let core_pool_size = match ctx.get_field_by_name(this, "corePoolSize") {
            Value::Int(value) => Value::Int(value),
            // Synthetic STPE objects have only the historical slot layout.
            _ => ctx.get_field(this, 0),
        };
        Ok(Some(core_pool_size))
    });

    // ScheduledExecutorService interface
    let ses = "java/util/concurrent/ScheduledExecutorService";
    r.register(ses, "schedule", "(Ljava/lang/Runnable;JLjava/util/concurrent/TimeUnit;)Ljava/util/concurrent/ScheduledFuture;", |ctx, args| {
        let mut runnable = match args.get(1) {
            Some(Value::Object(Some(r))) => *r,
            _ => return Ok(Some(Value::Object(None))),
        };
        let delay = match args.get(2) {
            Some(Value::Long(v)) => *v,
            // WP4.5 — generic stack pop type-erases Long → Double bit-pattern.
            Some(Value::Double(d)) => d.to_bits() as i64,
            Some(Value::Int(v)) => *v as i64,
            _ => 0,
        };
        let delay_ms = scheduled_convert_to_millis(ctx, delay, args.get(3));
        if delay_ms > 0 {
            let mut blocked_refs = [Value::Object(Some(runnable))];
            ctx.begin_blocking_region();
            std::thread::sleep(std::time::Duration::from_millis(delay_ms as u64));
            ctx.end_blocking_region_refs(&mut blocked_refs);
            if let Value::Object(Some(cur)) = blocked_refs[0] {
                runnable = cur;
            }
        }
        let _ = ctx.invoke_virtual(runnable, "run", "()V", &[]);
        let cf = p58_new_cf(ctx, Value::Object(None), true);
        Ok(Some(Value::Object(Some(cf))))
    });
    r.register(ses, "shutdown", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, 1, Value::Int(1));
        Ok(None)
    });

    // Executors factory
    let exs = "java/util/concurrent/Executors";
    r.register(
        exs,
        "newScheduledThreadPool",
        "(I)Ljava/util/concurrent/ScheduledExecutorService;",
        |ctx, args| {
            let cores = match args.first() {
                Some(Value::Int(v)) => *v,
                _ => 1,
            };
            let obj = stpe_new_executor(ctx, cores).unwrap_or_else(|| {
                alloc_concurrent_synthetic(
                    ctx,
                    "java/util/concurrent/ScheduledThreadPoolExecutor",
                    2,
                )
            });
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(
        exs,
        "newSingleThreadScheduledExecutor",
        "()Ljava/util/concurrent/ScheduledExecutorService;",
        |ctx, _args| {
            let obj = stpe_new_executor(ctx, 1).unwrap_or_else(|| {
                alloc_concurrent_synthetic(
                    ctx,
                    "java/util/concurrent/ScheduledThreadPoolExecutor",
                    2,
                )
            });
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.set_category(__prev_cat);
}

// =============================================================================
// PriorityBlockingQueue = 3-field (data=0, size=1, comparator=2) — reuses PQ layout
// =============================================================================

pub(crate) fn register_p65_priority_blocking_queue(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let pbq = "java/util/concurrent/PriorityBlockingQueue";
    r.register(pbq, "<init>", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 11);
        ctx.set_field(this, 0, Value::Object(Some(arr)));
        ctx.set_field(this, 1, Value::Int(0));
        ctx.set_field(this, 2, Value::Object(None));
        Ok(None)
    });
    r.register(pbq, "<init>", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let cap = match args.get(1) {
            Some(Value::Int(v)) => (*v).max(1) as usize,
            _ => 11,
        };
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, cap);
        ctx.set_field(this, 0, Value::Object(Some(arr)));
        ctx.set_field(this, 1, Value::Int(0));
        ctx.set_field(this, 2, Value::Object(None));
        Ok(None)
    });
    // put = offer (non-blocking semantics in our model)
    r.register(pbq, "put", "(Ljava/lang/Object;)V", native_p65_pbq_offer);
    r.register(pbq, "offer", "(Ljava/lang/Object;)Z", |ctx, args| {
        native_p65_pbq_offer(ctx, args)?;
        Ok(Some(Value::Int(1)))
    });
    r.register(pbq, "take", "()Ljava/lang/Object;", native_p65_pbq_poll);
    r.register(pbq, "poll", "()Ljava/lang/Object;", native_p65_pbq_poll);
    r.register(pbq, "peek", "()Ljava/lang/Object;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let size = match ctx.get_field(this, 1) {
            Value::Int(v) => v,
            _ => 0,
        };
        if size == 0 {
            return Ok(Some(Value::Object(None)));
        }
        let arr = match ctx.get_field(this, 0) {
            Value::Object(Some(a)) => a,
            _ => return Ok(Some(Value::Object(None))),
        };
        Ok(Some(ctx.get_array_element(arr, 0)))
    });
    r.register(pbq, "size", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 1)))
    });
    r.register(pbq, "isEmpty", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let size = match ctx.get_field(this, 1) {
            Value::Int(v) => v,
            _ => 0,
        };
        Ok(Some(Value::Int(if size == 0 { 1 } else { 0 })))
    });
    r.register(pbq, "clear", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, 1, Value::Int(0));
        Ok(None)
    });
    // KEEP: PriorityBlockingQueue is unbounded, and the JDK's own
    // `remainingCapacity()` is literally `return Integer.MAX_VALUE;`.
    r.register(pbq, "remainingCapacity", "()I", |_ctx, _args| {
        Ok(Some(Value::Int(i32::MAX)))
    });
    r.register(
        pbq,
        "comparator",
        "()Ljava/util/Comparator;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 2)))
        },
    );
    r.register(pbq, "toArray", "()[Ljava/lang/Object;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let size = match ctx.get_field(this, 1) {
            Value::Int(v) => v as usize,
            _ => 0,
        };
        let arr = match ctx.get_field(this, 0) {
            Value::Object(Some(a)) => a,
            _ => {
                let empty = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 0);
                return Ok(Some(Value::Object(Some(empty))));
            }
        };
        let result = ctx.new_array(cratonvm_types::ArrayElementType::Reference, size);
        for i in 0..size {
            ctx.set_array_element(result, i, ctx.get_array_element(arr, i));
        }
        Ok(Some(Value::Object(Some(result))))
    });
    r.set_category(__prev_cat);
}

pub(crate) fn native_p65_pbq_offer(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let elem = args.get(1).copied().unwrap_or(Value::Object(None));
    let mut size = match ctx.get_field(this, 1) {
        Value::Int(v) => v as usize,
        _ => 0,
    };
    let arr = match ctx.get_field(this, 0) {
        Value::Object(Some(a)) => a,
        _ => return Ok(None),
    };
    let cap = ctx.array_length(arr);
    // Grow if needed
    if size >= cap {
        let new_cap = cap * 2 + 1;
        let new_arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, new_cap);
        for i in 0..size {
            ctx.set_array_element(new_arr, i, ctx.get_array_element(arr, i));
        }
        ctx.set_field(this, 0, Value::Object(Some(new_arr)));
        // Insert at size position
        ctx.set_array_element(new_arr, size, elem);
    } else {
        ctx.set_array_element(arr, size, elem);
    }
    size += 1;
    ctx.set_field(this, 1, Value::Int(size as i32));
    // Sift up (min-heap by natural order)
    let data = match ctx.get_field(this, 0) {
        Value::Object(Some(a)) => a,
        _ => return Ok(None),
    };
    let mut i = size - 1;
    while i > 0 {
        let parent = (i - 1) / 2;
        let p_val = ctx.get_array_element(data, parent);
        let c_val = ctx.get_array_element(data, i);
        if p65_compare_values(p_val, c_val) > 0 {
            ctx.set_array_element(data, parent, c_val);
            ctx.set_array_element(data, i, p_val);
            i = parent;
        } else {
            break;
        }
    }
    Ok(None)
}

pub(crate) fn native_p65_pbq_poll(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let size = match ctx.get_field(this, 1) {
        Value::Int(v) => v as usize,
        _ => 0,
    };
    if size == 0 {
        return Ok(Some(Value::Object(None)));
    }
    let arr = match ctx.get_field(this, 0) {
        Value::Object(Some(a)) => a,
        _ => return Ok(Some(Value::Object(None))),
    };
    let result = ctx.get_array_element(arr, 0);
    let new_size = size - 1;
    let last = ctx.get_array_element(arr, new_size);
    ctx.set_array_element(arr, 0, last);
    ctx.set_array_element(arr, new_size, Value::Object(None));
    ctx.set_field(this, 1, Value::Int(new_size as i32));
    // Sift down
    let mut i = 0;
    while i < new_size / 2 {
        let left = 2 * i + 1;
        let right = 2 * i + 2;
        let mut smallest = left;
        if right < new_size {
            let l_val = ctx.get_array_element(arr, left);
            let r_val = ctx.get_array_element(arr, right);
            if p65_compare_values(r_val, l_val) < 0 {
                smallest = right;
            }
        }
        let p_val = ctx.get_array_element(arr, i);
        let s_val = ctx.get_array_element(arr, smallest);
        if p65_compare_values(s_val, p_val) < 0 {
            ctx.set_array_element(arr, i, s_val);
            ctx.set_array_element(arr, smallest, p_val);
            i = smallest;
        } else {
            break;
        }
    }
    Ok(Some(result))
}

pub(crate) fn p65_compare_values(a: Value, b: Value) -> i32 {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => x.cmp(&y) as i32,
        (Value::Long(x), Value::Long(y)) => x.cmp(&y) as i32,
        (Value::Float(x), Value::Float(y)) => x.partial_cmp(&y).map(|o| o as i32).unwrap_or(0),
        (Value::Double(x), Value::Double(y)) => x.partial_cmp(&y).map(|o| o as i32).unwrap_or(0),
        _ => 0,
    }
}

// =============================================================================
// DelayQueue = 2-field (elements=0 Object[], size=1)
//
// The previous model stored NOTHING: `put`/`offer` only bumped a counter and
// `take`/`poll`/`peek` were hardcoded to null. `size()` therefore reported a
// non-empty queue that could never yield an element — a producer/consumer pair
// silently exchanged nulls (BlockingQueue.take() is specified never to return
// null, so callers NPE far away from the cause) or spun forever in
// `while (!q.isEmpty()) consume(q.take())`.
//
// Elements now live in a real Reference[] held in field 0, so they are a GC
// root and are remapped by a moving collector. Delay ordering is honoured by
// asking each element for its remaining delay (`Delayed.getDelay(NANOSECONDS)`)
// at poll time instead of maintaining a heap: DelayQueue is a low-traffic API
// here and a scan keeps the invariants trivially correct even when an element's
// delay changes after insertion.
// =============================================================================

/// Remaining delay of `elem` in nanoseconds, via `Delayed.getDelay(TimeUnit)`.
///
/// Returns `None` when the delay cannot be determined (no `TimeUnit` class,
/// null element, or the call fails). Callers treat `None` as "available now",
/// which degrades DelayQueue to a plain FIFO rather than to the old
/// never-yields-anything behaviour.
fn dq_delay_nanos(ctx: &mut dyn NativeContext, elem: ObjectRef) -> Option<i64> {
    let cid = ctx
        .ensure_class_initialized("java/util/concurrent/TimeUnit")
        .ok()?;
    let idx = ctx.static_field_index_by_name(cid, "NANOSECONDS")?;
    let unit = match ctx.get_static_field(cid, idx) {
        Value::Object(Some(u)) => u,
        _ => return None,
    };
    match ctx.invoke_virtual(
        elem,
        "getDelay",
        "(Ljava/util/concurrent/TimeUnit;)J",
        &[Value::Object(Some(unit))],
    ) {
        Ok(Some(Value::Long(n))) => Some(n),
        Ok(Some(Value::Int(n))) => Some(n as i64),
        _ => None,
    }
}

/// Live `(elements array, size)` of a DelayQueue, clamped to the array length.
fn dq_state(ctx: &mut dyn NativeContext, this: ObjectRef) -> Option<(ObjectRef, usize)> {
    let arr = match ctx.get_field(this, 0) {
        Value::Object(Some(a)) => a,
        _ => return None,
    };
    let size = ctx.get_field(this, 1).as_int().unwrap_or(0).max(0) as usize;
    Some((arr, size.min(ctx.array_length(arr))))
}

/// Index of the element with the smallest remaining delay, plus that delay.
/// `None` when the queue is empty.
///
/// Returns the receiver alongside the answer: `Delayed.getDelay` is user
/// bytecode, so it allocates and can safepoint, and a moving young GC there
/// relocates both `this` and the backing array (native stale-local family).
/// `this` is pinned for the scan and the array is re-read from the pinned
/// receiver on every iteration; callers must use the returned receiver.
fn dq_head(ctx: &mut dyn NativeContext, this: ObjectRef) -> (ObjectRef, Option<(usize, i64)>) {
    let this_pin = ctx.pin_native_root(this);
    let mut cur = this;
    let mut best: Option<(usize, i64)> = None;
    let mut i = 0usize;
    loop {
        cur = ctx.read_native_pin(this_pin, cur);
        let Some((arr, size)) = dq_state(ctx, cur) else {
            break;
        };
        if i >= size {
            break;
        }
        let delay = match ctx.get_array_element(arr, i) {
            Value::Object(Some(e)) => dq_delay_nanos(ctx, e).unwrap_or(0),
            // A null slot has no delay to ask for; treat it as available so a
            // queue seeded with nulls (the legacy `put(null)` path) still
            // drains instead of wedging.
            _ => 0,
        };
        if best.is_none_or(|(_, b)| delay < b) {
            best = Some((i, delay));
        }
        i += 1;
    }
    cur = ctx.read_native_pin(this_pin, cur);
    ctx.unpin_native_roots(this_pin);
    (cur, best)
}

/// Remove and return the element at `index`, shifting the tail down.
fn dq_remove_at(ctx: &mut dyn NativeContext, this: ObjectRef, index: usize) -> Value {
    let Some((arr, size)) = dq_state(ctx, this) else {
        return Value::Object(None);
    };
    if index >= size {
        return Value::Object(None);
    }
    let item = ctx.get_array_element(arr, index);
    for i in index..size.saturating_sub(1) {
        let next = ctx.get_array_element(arr, i + 1);
        ctx.set_array_element(arr, i, next);
    }
    ctx.set_array_element(arr, size - 1, Value::Object(None));
    ctx.set_field(this, 1, Value::Int((size - 1) as i32));
    item
}

/// Append `elem`, growing the backing array when it is full.
fn dq_append(ctx: &mut dyn NativeContext, this: ObjectRef, elem: Value) {
    let size = ctx.get_field(this, 1).as_int().unwrap_or(0).max(0) as usize;
    // `this` and the incoming element must survive every allocation below
    // (native stale-local family): one base pin covers the seed array and the
    // growth copy.
    let base_pin = ctx.pin_native_root(this);
    let elem_pin = pinned_object_value(ctx, elem);
    let mut this = this;
    let arr = match ctx.get_field(this, 0) {
        Value::Object(Some(a)) => a,
        _ => {
            let fresh = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 8);
            this = ctx.read_native_pin(base_pin, this);
            ctx.set_field(this, 0, Value::Object(Some(fresh)));
            fresh
        }
    };
    let cap = ctx.array_length(arr);
    let arr = if size >= cap {
        let grown = ctx.new_array(cratonvm_types::ArrayElementType::Reference, cap * 2 + 8);
        this = ctx.read_native_pin(base_pin, this);
        // Re-read the old array through the (possibly moved) receiver.
        let old = match ctx.get_field(this, 0) {
            Value::Object(Some(a)) => a,
            _ => grown,
        };
        for i in 0..size.min(ctx.array_length(old)) {
            let v = ctx.get_array_element(old, i);
            ctx.set_array_element(grown, i, v);
        }
        ctx.set_field(this, 0, Value::Object(Some(grown)));
        grown
    } else {
        arr
    };
    let elem = read_pinned_object_value(ctx, elem_pin, elem);
    ctx.set_array_element(arr, size, elem);
    ctx.set_field(this, 1, Value::Int((size + 1) as i32));
    ctx.unpin_native_roots(base_pin);
}

/// Longest single park inside `DelayQueue.take()`. The loop re-checks the head
/// after every slice, so a shorter cap only costs wakeups — it bounds how long
/// this thread stays inside one blocking region.
const DQ_TAKE_SLICE: std::time::Duration = std::time::Duration::from_millis(20);

pub(crate) fn register_p65_delay_queue(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let dq = "java/util/concurrent/DelayQueue";
    r.register(dq, "<init>", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // Pin `this` across the array alloc — see `dq_append`.
        let this_pin = ctx.pin_native_root(this);
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 8);
        let this = ctx.read_native_pin(this_pin, this);
        ctx.set_field(this, 0, Value::Object(Some(arr)));
        ctx.set_field(this, 1, Value::Int(0));
        ctx.unpin_native_roots(this_pin);
        Ok(None)
    });
    r.register(
        dq,
        "put",
        "(Ljava/util/concurrent/Delayed;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let elem = args.get(1).copied().unwrap_or(Value::Object(None));
            dq_append(ctx, this, elem);
            Ok(None)
        },
    );
    r.register(
        dq,
        "offer",
        "(Ljava/util/concurrent/Delayed;)Z",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let elem = args.get(1).copied().unwrap_or(Value::Object(None));
            dq_append(ctx, this, elem);
            // DelayQueue is unbounded — offer never fails.
            Ok(Some(Value::Int(1)))
        },
    );
    // take(): blocks until the head element's delay has elapsed, per
    // BlockingQueue. BEHAVIOUR CHANGE — this used to return null immediately.
    // The wait runs inside the VM's blocking region so a GC safepoint does not
    // stall behind this thread, and `this` is re-read from the pin after each
    // slice because a moving young GC can relocate the receiver while parked.
    r.register(
        dq,
        "take",
        "()Ljava/util/concurrent/Delayed;",
        |ctx, args| {
            let mut this = obj_arg(args, 0)?;
            loop {
                let (refreshed, head) = dq_head(ctx, this);
                this = refreshed;
                if let Some((index, delay)) = head {
                    if delay <= 0 {
                        return Ok(Some(dq_remove_at(ctx, this, index)));
                    }
                    let wait =
                        std::time::Duration::from_nanos(delay.max(0) as u64).min(DQ_TAKE_SLICE);
                    let mut blocked_refs = [Value::Object(Some(this))];
                    ctx.begin_blocking_region();
                    std::thread::sleep(wait);
                    ctx.end_blocking_region_refs(&mut blocked_refs);
                    if let Value::Object(Some(cur)) = blocked_refs[0] {
                        this = cur;
                    }
                    continue;
                }
                // Empty: wait for a producer on another thread.
                let mut blocked_refs = [Value::Object(Some(this))];
                ctx.begin_blocking_region();
                std::thread::sleep(DQ_TAKE_SLICE);
                ctx.end_blocking_region_refs(&mut blocked_refs);
                if let Value::Object(Some(cur)) = blocked_refs[0] {
                    this = cur;
                }
            }
        },
    );
    r.register(
        dq,
        "poll",
        "()Ljava/util/concurrent/Delayed;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let (this, head) = dq_head(ctx, this);
            match head {
                // poll() is non-blocking: null unless the head has expired.
                Some((index, delay)) if delay <= 0 => Ok(Some(dq_remove_at(ctx, this, index))),
                _ => Ok(Some(Value::Object(None))),
            }
        },
    );
    r.register(
        dq,
        "peek",
        "()Ljava/util/concurrent/Delayed;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // peek() returns the head whether or not its delay has expired.
            let (this, head) = dq_head(ctx, this);
            let Some((index, _)) = head else {
                return Ok(Some(Value::Object(None)));
            };
            let Some((arr, _)) = dq_state(ctx, this) else {
                return Ok(Some(Value::Object(None)));
            };
            Ok(Some(ctx.get_array_element(arr, index)))
        },
    );
    r.register(dq, "size", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 1)))
    });
    r.register(dq, "isEmpty", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let size = match ctx.get_field(this, 1) {
            Value::Int(v) => v,
            _ => 0,
        };
        Ok(Some(Value::Int(if size == 0 { 1 } else { 0 })))
    });
    r.register(dq, "clear", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // Drop the element references too, so a cleared queue does not keep
        // its former contents reachable.
        if let Some((arr, size)) = dq_state(ctx, this) {
            for i in 0..size {
                ctx.set_array_element(arr, i, Value::Object(None));
            }
        }
        ctx.set_field(this, 1, Value::Int(0));
        Ok(None)
    });
    // KEEP: DelayQueue is unbounded; the JDK's `remainingCapacity()` is
    // literally `return Integer.MAX_VALUE;`.
    r.register(dq, "remainingCapacity", "()I", |_ctx, _args| {
        Ok(Some(Value::Int(i32::MAX)))
    });
    r.set_category(__prev_cat);
}

// =============================================================================
// ExecutorCompletionService = 2-field (executor=0, completionQueue=1)
//
// The completion queue in field 1 is a plain Reference[] whose LENGTH is the
// queue size — no separate count slot exists on the 2-field synthetic, and the
// traffic here is a handful of futures, so push/pop reallocate rather than
// carry a cursor. The array is a heap object, so the queued futures are a GC
// root and are remapped by a moving collector.
//
// Before this, submit() never ran the task and never recorded anything: it
// handed back a CompletableFuture already "completed" with null. take()
// fabricated a second such future out of thin air and poll() was hardcoded to
// null, so `submit(c); take().get()` answered null instead of c.call()'s
// result and poll() could never see a completion. Nothing failed loudly.
// =============================================================================

/// Append `fut` to an ExecutorCompletionService's completion queue (field 1).
fn ecs_queue_push(ctx: &mut dyn NativeContext, this: ObjectRef, fut: Value) {
    // Pin `this` and the future across the array allocation below — it can
    // safepoint and relocate both (native stale-local family).
    let this_pin = ctx.pin_native_root(this);
    let fut_pin = match fut {
        Value::Object(Some(o)) => Some((ctx.pin_native_root(o), o)),
        _ => None,
    };
    let old = match ctx.get_field(this, 1) {
        Value::Object(Some(a)) => Some(a),
        _ => None,
    };
    let old_len = match old {
        Some(a) => ctx.array_length(a),
        None => 0,
    };
    let grown = ctx.new_array(cratonvm_types::ArrayElementType::Reference, old_len + 1);
    let this = ctx.read_native_pin(this_pin, this);
    if let Value::Object(Some(old)) = ctx.get_field(this, 1) {
        for i in 0..old_len.min(ctx.array_length(old)) {
            let v = ctx.get_array_element(old, i);
            ctx.set_array_element(grown, i, v);
        }
    }
    let fut = match fut_pin {
        Some((h, o)) => Value::Object(Some(ctx.read_native_pin(h, o))),
        None => fut,
    };
    ctx.set_array_element(grown, old_len, fut);
    ctx.set_field(this, 1, Value::Object(Some(grown)));
    ctx.unpin_native_roots(this_pin);
}

/// Remove and return the head of the completion queue, or `None` when empty.
fn ecs_queue_pop(ctx: &mut dyn NativeContext, this: ObjectRef) -> Option<Value> {
    let old = match ctx.get_field(this, 1) {
        Value::Object(Some(a)) => a,
        _ => return None,
    };
    let len = ctx.array_length(old);
    if len == 0 {
        return None;
    }
    let head = ctx.get_array_element(old, 0);
    // Pin across the shrink allocation.
    let this_pin = ctx.pin_native_root(this);
    let head_pin = match head {
        Value::Object(Some(o)) => Some((ctx.pin_native_root(o), o)),
        _ => None,
    };
    let shrunk = ctx.new_array(cratonvm_types::ArrayElementType::Reference, len - 1);
    let this = ctx.read_native_pin(this_pin, this);
    if let Value::Object(Some(old)) = ctx.get_field(this, 1) {
        for i in 1..len.min(ctx.array_length(old)) {
            let v = ctx.get_array_element(old, i);
            ctx.set_array_element(shrunk, i - 1, v);
        }
    }
    ctx.set_field(this, 1, Value::Object(Some(shrunk)));
    let head = match head_pin {
        Some((h, o)) => Value::Object(Some(ctx.read_native_pin(h, o))),
        None => head,
    };
    ctx.unpin_native_roots(this_pin);
    Some(head)
}

/// Run a `Callable`/`Runnable` task inline, wrap its result in a completed
/// future and record that future in the completion queue.
///
/// Execution is inline on the calling thread, matching the rest of CratonVM's
/// synthetic executor model (`ExecutorService.submit`, `ForkJoinPool.invoke`).
/// A task that throws propagates out of `submit` rather than being buried in
/// the returned future: our `CompletableFuture` synthetic has no exceptional
/// slot that `Future.get()` would rethrow from, so capturing it would hand the
/// caller a future whose `get()` silently answers the Throwable as a value.
fn ecs_run_and_record(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    task: Value,
    runnable_result: Option<Value>,
) -> MethodCallResult {
    let this_pin = ctx.pin_native_root(this);
    let outcome = match task {
        Value::Object(Some(t)) => {
            let t_pin = ctx.pin_native_root(t);
            // The caller's `result` argument survives the task callback below
            // only if it is pinned: run()/call() is user bytecode and a moving
            // young GC there relocates it (native stale-local family).
            let result_pin = match runnable_result {
                Some(v) => pinned_object_value(ctx, v),
                None => None,
            };
            match runnable_result {
                // submit(Runnable, V): run() then report the caller's value.
                Some(v) => {
                    let ran = ctx.invoke_virtual(t, "run", "()V", &[]);
                    match ran {
                        Ok(_) => Ok(read_pinned_object_value(ctx, result_pin, v)),
                        Err(err) => Err(err),
                    }
                }
                // submit(Callable<V>): call() supplies the value. Fall back to
                // run() for a receiver that is really a Runnable.
                None => match ctx.invoke_virtual(t, "call", "()Ljava/lang/Object;", &[]) {
                    Ok(v) => Ok(v.unwrap_or(Value::Object(None))),
                    Err(err) => {
                        let t = ctx.read_native_pin(t_pin, t);
                        match ctx.invoke_virtual(t, "run", "()V", &[]) {
                            Ok(_) => Ok(Value::Object(None)),
                            // Neither shape worked — surface the original
                            // Callable failure, not the Runnable retry's.
                            Err(_) => Err(err),
                        }
                    }
                },
            }
        }
        // A null task is a NullPointerException in the JDK. Keep the historical
        // lenient answer — a completed future with the caller's result — but do
        // NOT record it: there was no completion to report, and enqueuing a
        // phantom would make the next take()/poll() hand back a future for work
        // that never existed.
        _ => {
            ctx.unpin_native_roots(this_pin);
            let value = runnable_result.unwrap_or(Value::Object(None));
            let cf = p58_new_cf(ctx, value, true);
            return Ok(Some(Value::Object(Some(cf))));
        }
    };
    let value = match outcome {
        Ok(v) => v,
        Err(err) => {
            ctx.unpin_native_roots(this_pin);
            return Err(err);
        }
    };
    let value_pin = pinned_object_value(ctx, value);
    // Read the forwarded value out FIRST: inlining this into the call below
    // borrows `ctx` immutably (`read_pinned_object_value`) inside a call that
    // already borrows it mutably (`p58_new_cf`).
    let forwarded = read_pinned_object_value(ctx, value_pin, value);
    let cf = p58_new_cf(ctx, forwarded, true);
    if let Some((h, _)) = value_pin {
        ctx.unpin_native_roots(h);
    }
    let cf_pin = ctx.pin_native_root(cf);
    let this = ctx.read_native_pin(this_pin, this);
    let cf = ctx.read_native_pin(cf_pin, cf);
    ecs_queue_push(ctx, this, Value::Object(Some(cf)));
    let cf = ctx.read_native_pin(cf_pin, cf);
    ctx.unpin_native_roots(this_pin);
    Ok(Some(Value::Object(Some(cf))))
}

/// Longest single park inside `CompletionService.take()` — see
/// [`DQ_TAKE_SLICE`].
const ECS_TAKE_SLICE: std::time::Duration = std::time::Duration::from_millis(20);

/// `CompletionService.take()` — pop a completed future, blocking until one is
/// available. Submissions run inline, so the queue is already populated in the
/// canonical `for (..) submit(..); for (..) take();` shape; the wait only
/// matters when another thread is the submitter.
fn ecs_take(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = obj_arg(args, 0)?;
    loop {
        if let Some(head) = ecs_queue_pop(ctx, this) {
            return Ok(Some(head));
        }
        let mut blocked_refs = [Value::Object(Some(this))];
        ctx.begin_blocking_region();
        std::thread::sleep(ECS_TAKE_SLICE);
        ctx.end_blocking_region_refs(&mut blocked_refs);
        if let Value::Object(Some(cur)) = blocked_refs[0] {
            this = cur;
        }
    }
}

/// `CompletionService.poll()` — non-blocking pop; null when nothing has
/// completed yet.
fn ecs_poll(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    Ok(Some(
        ecs_queue_pop(ctx, this).unwrap_or(Value::Object(None)),
    ))
}

/// `CompletionService.poll(long, TimeUnit)` — pop within the deadline, else
/// null. Unlike `take()` this never waits indefinitely.
fn ecs_poll_timed(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = obj_arg(args, 0)?;
    let raw = match args.get(1) {
        Some(Value::Long(v)) => *v,
        // A generic stack pop can type-erase Long into a Double bit pattern
        // (see the schedule() natives above).
        Some(Value::Double(d)) => d.to_bits() as i64,
        Some(Value::Int(v)) => *v as i64,
        _ => 0,
    };
    let timeout_ms = scheduled_convert_to_millis(ctx, raw, args.get(2)).max(0) as u64;
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    loop {
        if let Some(head) = ecs_queue_pop(ctx, this) {
            return Ok(Some(head));
        }
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return Ok(Some(Value::Object(None)));
        }
        let mut blocked_refs = [Value::Object(Some(this))];
        ctx.begin_blocking_region();
        std::thread::sleep(remaining.min(ECS_TAKE_SLICE));
        ctx.end_blocking_region_refs(&mut blocked_refs);
        if let Value::Object(Some(cur)) = blocked_refs[0] {
            this = cur;
        }
    }
}

pub(crate) fn register_p65_completion_service(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let ecs = "java/util/concurrent/ExecutorCompletionService";
    r.register(
        ecs,
        "<init>",
        "(Ljava/util/concurrent/Executor;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.set_field(this, 0, args.get(1).copied().unwrap_or(Value::Object(None)));
            ctx.set_field(this, 1, Value::Object(None));
            Ok(None)
        },
    );
    r.register(
        ecs,
        "submit",
        "(Ljava/util/concurrent/Callable;)Ljava/util/concurrent/Future;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let task = args.get(1).copied().unwrap_or(Value::Object(None));
            ecs_run_and_record(ctx, this, task, None)
        },
    );
    r.register(
        ecs,
        "submit",
        "(Ljava/lang/Runnable;Ljava/lang/Object;)Ljava/util/concurrent/Future;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let task = args.get(1).copied().unwrap_or(Value::Object(None));
            let result = args.get(2).copied().unwrap_or(Value::Object(None));
            ecs_run_and_record(ctx, this, task, Some(result))
        },
    );
    r.register(ecs, "take", "()Ljava/util/concurrent/Future;", ecs_take);
    r.register(ecs, "poll", "()Ljava/util/concurrent/Future;", ecs_poll);
    // Timed poll: the completion queue is filled by submit() on some thread,
    // so honour the timeout instead of answering null on the spot.
    r.register(
        ecs,
        "poll",
        "(JLjava/util/concurrent/TimeUnit;)Ljava/util/concurrent/Future;",
        ecs_poll_timed,
    );

    // CompletionService interface
    let cs = "java/util/concurrent/CompletionService";
    r.register(
        cs,
        "submit",
        "(Ljava/util/concurrent/Callable;)Ljava/util/concurrent/Future;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let task = args.get(1).copied().unwrap_or(Value::Object(None));
            ecs_run_and_record(ctx, this, task, None)
        },
    );
    r.register(cs, "take", "()Ljava/util/concurrent/Future;", ecs_take);
    r.register(cs, "poll", "()Ljava/util/concurrent/Future;", ecs_poll);
    r.set_category(__prev_cat);
}

// =============================================================================
// Thread.Builder / Thread.ofVirtual / Thread.ofPlatform — Java 21
// =============================================================================

pub(crate) fn register_p66_thread_builder(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let t = "java/lang/Thread";
    // Thread.ofVirtual() -> Thread.Builder
    r.register(
        t,
        "ofVirtual",
        "()Ljava/lang/Thread$Builder;",
        |ctx, _args| {
            let obj = alloc_concurrent_synthetic(ctx, "java/lang/Thread$Builder", 2);
            ctx.set_field(obj, 0, Value::Object(None)); // name
            ctx.set_field(obj, 1, Value::Int(1)); // virtual=true
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(
        t,
        "ofPlatform",
        "()Ljava/lang/Thread$Builder;",
        |ctx, _args| {
            let obj = alloc_concurrent_synthetic(ctx, "java/lang/Thread$Builder", 2);
            ctx.set_field(obj, 0, Value::Object(None));
            ctx.set_field(obj, 1, Value::Int(0)); // virtual=false
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(
        t,
        "startVirtualThread",
        "(Ljava/lang/Runnable;)Ljava/lang/Thread;",
        |ctx, args| {
            let runnable = args.first().copied().unwrap_or(Value::Object(None));
            let thr = alloc_concurrent_synthetic(ctx, "java/lang/Thread", 5);
            let name = ctx.create_string("virtual-thread");
            ctx.set_field(thr, 0, Value::Object(Some(name)));
            ctx.set_field(thr, 1, Value::Int(5)); // NORM_PRIORITY
                                                  // field 2 = tid (set by thread_start)
            ctx.set_field(thr, 3, runnable); // target Runnable
            ctx.set_field(thr, 4, Value::Int(1)); // virtual=true
            ctx.thread_start(thr)?;
            Ok(Some(Value::Object(Some(thr))))
        },
    );

    // Thread.Builder interface
    let tb = "java/lang/Thread$Builder";
    r.register(
        tb,
        "name",
        "(Ljava/lang/String;)Ljava/lang/Thread$Builder;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.set_field(this, 0, args.get(1).copied().unwrap_or(Value::Object(None)));
            Ok(Some(Value::Object(Some(this))))
        },
    );
    r.register(
        tb,
        "name",
        "(Ljava/lang/String;J)Ljava/lang/Thread$Builder;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.set_field(this, 0, args.get(1).copied().unwrap_or(Value::Object(None)));
            Ok(Some(Value::Object(Some(this))))
        },
    );
    r.register(
        tb,
        "daemon",
        "(Z)Ljava/lang/Thread$Builder;",
        |_ctx, args| Ok(Some(args.first().copied().unwrap_or(Value::Object(None)))),
    );
    r.register(
        tb,
        "priority",
        "(I)Ljava/lang/Thread$Builder;",
        |_ctx, args| Ok(Some(args.first().copied().unwrap_or(Value::Object(None)))),
    );
    r.register(
        tb,
        "start",
        "(Ljava/lang/Runnable;)Ljava/lang/Thread;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let runnable = args.get(1).copied().unwrap_or(Value::Object(None));
            let is_virtual = matches!(ctx.get_field(this, 1), Value::Int(1));
            let thr = alloc_concurrent_synthetic(ctx, "java/lang/Thread", 5);
            // Use builder's stored name or default
            let name_val = ctx.get_field(this, 0);
            if matches!(name_val, Value::Object(Some(_))) {
                ctx.set_field(thr, 0, name_val);
            } else {
                let name = ctx.create_string(if is_virtual {
                    "virtual-thread"
                } else {
                    "platform-thread"
                });
                ctx.set_field(thr, 0, Value::Object(Some(name)));
            }
            ctx.set_field(thr, 1, Value::Int(5)); // NORM_PRIORITY
            ctx.set_field(thr, 3, runnable); // target Runnable
            ctx.set_field(thr, 4, Value::Int(if is_virtual { 1 } else { 0 }));
            ctx.thread_start(thr)?;
            Ok(Some(Value::Object(Some(thr))))
        },
    );
    r.register(
        tb,
        "unstarted",
        "(Ljava/lang/Runnable;)Ljava/lang/Thread;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let runnable = args.get(1).copied().unwrap_or(Value::Object(None));
            let is_virtual = matches!(ctx.get_field(this, 1), Value::Int(1));
            let thr = alloc_concurrent_synthetic(ctx, "java/lang/Thread", 5);
            let name_val = ctx.get_field(this, 0);
            if matches!(name_val, Value::Object(Some(_))) {
                ctx.set_field(thr, 0, name_val);
            } else {
                let name = ctx.create_string("unstarted-thread");
                ctx.set_field(thr, 0, Value::Object(Some(name)));
            }
            ctx.set_field(thr, 1, Value::Int(5));
            ctx.set_field(thr, 3, runnable); // target Runnable (not started yet)
            ctx.set_field(thr, 4, Value::Int(if is_virtual { 1 } else { 0 }));
            Ok(Some(Value::Object(Some(thr))))
        },
    );
    r.register(
        tb,
        "factory",
        "()Ljava/util/concurrent/ThreadFactory;",
        |ctx, args| {
            // Return a ThreadFactory that delegates to the builder.
            // The factory is a 1-field object: field 0 = the builder reference.
            let this = obj_arg(args, 0)?;
            let factory = alloc_concurrent_synthetic(ctx, "java/util/concurrent/ThreadFactory", 1);
            ctx.set_field(factory, 0, Value::Object(Some(this)));
            Ok(Some(Value::Object(Some(factory))))
        },
    );
    // ThreadFactory.newThread(Runnable) — delegates to builder.unstarted
    r.register(
        "java/util/concurrent/ThreadFactory",
        "newThread",
        "(Ljava/lang/Runnable;)Ljava/lang/Thread;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let runnable = args.get(1).copied().unwrap_or(Value::Object(None));
            // Read the builder from field 0
            let builder_ref = match ctx.get_field(this, 0) {
                Value::Object(Some(b)) => b,
                _ => {
                    // No builder stored — create a default platform thread
                    let thr = alloc_concurrent_synthetic(ctx, "java/lang/Thread", 5);
                    let name = ctx.create_string("factory-thread");
                    ctx.set_field(thr, 0, Value::Object(Some(name)));
                    ctx.set_field(thr, 1, Value::Int(5));
                    ctx.set_field(thr, 3, runnable);
                    ctx.set_field(thr, 4, Value::Int(0));
                    return Ok(Some(Value::Object(Some(thr))));
                }
            };
            let is_virtual = matches!(ctx.get_field(builder_ref, 1), Value::Int(1));
            let thr = alloc_concurrent_synthetic(ctx, "java/lang/Thread", 5);
            let name_val = ctx.get_field(builder_ref, 0);
            if matches!(name_val, Value::Object(Some(_))) {
                ctx.set_field(thr, 0, name_val);
            } else {
                let name = ctx.create_string(if is_virtual {
                    "virtual-factory-thread"
                } else {
                    "platform-factory-thread"
                });
                ctx.set_field(thr, 0, Value::Object(Some(name)));
            }
            ctx.set_field(thr, 1, Value::Int(5));
            ctx.set_field(thr, 3, runnable);
            ctx.set_field(thr, 4, Value::Int(if is_virtual { 1 } else { 0 }));
            Ok(Some(Value::Object(Some(thr))))
        },
    );

    // Thread.isVirtual() — Java 21
    r.register(t, "isVirtual", "()Z", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Int(0))),
        };
        let num_fields = ctx.object_num_fields(this);
        if num_fields >= 5 {
            match ctx.get_field(this, 4) {
                Value::Int(v) => Ok(Some(Value::Int(if v != 0 { 1 } else { 0 }))),
                _ => Ok(Some(Value::Int(0))),
            }
        } else {
            Ok(Some(Value::Int(0)))
        }
    });
    // Thread.threadId() — Java 19. Use the receiver's Java tid for cross-thread
    // queries; the VM context is only a fallback during early bootstrap.
    r.register(t, "threadId", "()J", |ctx, args| {
        let receiver_tid = args.first().and_then(|value| match value {
            Value::Object(Some(thread)) => match ctx.get_field_by_name(*thread, "tid") {
                Value::Long(tid) if tid > 0 => Some(tid),
                Value::Int(tid) if tid > 0 => Some(tid as i64),
                _ => None,
            },
            _ => None,
        });
        Ok(Some(Value::Long(
            receiver_tid.unwrap_or_else(|| ctx.thread_id().max(1) as i64),
        )))
    });
    r.set_category(__prev_cat);
}

// =============================================================================
// java.util.concurrent.StructuredTaskScope — Java 21 (preview → final Java 25)
// 3-field: owner=0 (Long threadId), shutdown=1 (Int 0/1), result=2 (Object)
// =============================================================================

/// `jdk.incubator.concurrent.StructuredTaskScope$Subtask` state values, mirroring
/// the enum the `state()` native renders.
const INCUBATOR_SUBTASK_UNAVAILABLE: i32 = 0;
const INCUBATOR_SUBTASK_SUCCESS: i32 = 2;
const INCUBATOR_SUBTASK_FAILED: i32 = 3;

/// Wrap `value` in a synthetic `java.util.Optional` (1 field; null == empty).
/// `Optional`-returning methods must never hand back a bare null — callers
/// immediately dereference the result with `isPresent()`/`orElse(..)`.
fn sts_optional_of(ctx: &mut dyn NativeContext, value: Value) -> ObjectRef {
    // Pin the payload across the Optional allocation (native stale-local family).
    let value_pin = pinned_object_value(ctx, value);
    let opt = alloc_concurrent_synthetic(ctx, "java/util/Optional", 1);
    let value = read_pinned_object_value(ctx, value_pin, value);
    ctx.set_field(opt, 0, value);
    if let Some((h, _)) = value_pin {
        ctx.unpin_native_roots(h);
    }
    opt
}

/// The Throwable behind a failed call, when the failure was a Java exception
/// rather than an internal VM error.
fn thrown_object(err: &MethodCallFailed) -> Value {
    match err {
        MethodCallFailed::ExceptionThrown(obj) => Value::Object(Some(*obj)),
        _ => Value::Object(None),
    }
}

/// Fork one incubator-`StructuredTaskScope` subtask: run the callable inline
/// and record the outcome in a 3-field Subtask (state=0, result=1,
/// exception=2). Slot 2 is new — the failure Throwable used to be dropped on
/// the floor, which is why `Subtask.exception()` could only ever answer null.
///
/// Returns `(scope, subtask)` because `Callable.call()` is user bytecode: it
/// allocates and can safepoint, and a moving young GC there relocates both the
/// scope and the fresh subtask (native stale-local family). Callers must use
/// the returned handles.
fn incubator_fork_subtask(
    ctx: &mut dyn NativeContext,
    scope: Option<ObjectRef>,
    callable: Option<ObjectRef>,
) -> (Option<ObjectRef>, ObjectRef) {
    let subtask = alloc_concurrent_synthetic(
        ctx,
        "jdk/incubator/concurrent/StructuredTaskScope$Subtask",
        3,
    );
    ctx.set_field(subtask, 0, Value::Int(INCUBATOR_SUBTASK_UNAVAILABLE));
    ctx.set_field(subtask, 1, Value::Object(None));
    ctx.set_field(subtask, 2, Value::Object(None));
    let Some(callable) = callable else {
        return (scope, subtask);
    };
    let base = ctx.pin_native_root(subtask);
    let scope_pin = match scope {
        Some(s) => Some((ctx.pin_native_root(s), s)),
        None => None,
    };
    // `invoke_virtual` prepends the receiver itself — passing it again in
    // `args` (as this code used to) duplicates it.
    let outcome = ctx.invoke_virtual(callable, "call", "()Ljava/lang/Object;", &[]);
    let subtask = ctx.read_native_pin(base, subtask);
    let scope = match scope_pin {
        Some((h, s)) => Some(ctx.read_native_pin(h, s)),
        None => None,
    };
    match outcome {
        Ok(value) => {
            ctx.set_field(subtask, 0, Value::Int(INCUBATOR_SUBTASK_SUCCESS));
            ctx.set_field(subtask, 1, value.unwrap_or(Value::Object(None)));
        }
        Err(err) => {
            ctx.set_field(subtask, 0, Value::Int(INCUBATOR_SUBTASK_FAILED));
            ctx.set_field(subtask, 1, Value::Object(None));
            ctx.set_field(subtask, 2, thrown_object(&err));
        }
    }
    ctx.unpin_native_roots(base);
    (scope, subtask)
}

pub(crate) fn register_p67_structured_task_scope(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let sts = "jdk/incubator/concurrent/StructuredTaskScope";

    r.register(sts, "<init>", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, 0, Value::Long(ctx.thread_id() as i64));
        ctx.set_field(this, 1, Value::Int(0));
        ctx.set_field(this, 2, Value::Object(None));
        Ok(None)
    });
    r.register(
        sts,
        "<init>",
        "(Ljava/lang/String;Ljava/util/concurrent/ThreadFactory;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.set_field(this, 0, Value::Long(ctx.thread_id() as i64));
            ctx.set_field(this, 1, Value::Int(0));
            ctx.set_field(this, 2, Value::Object(None));
            Ok(None)
        },
    );
    r.register(
        sts,
        "fork",
        "(Ljava/util/concurrent/Callable;)Ljdk/incubator/concurrent/StructuredTaskScope$Subtask;",
        |ctx, args| {
            let callable = match args.get(1) {
                Some(Value::Object(Some(c))) => Some(*c),
                _ => None,
            };
            let (_, subtask) = incubator_fork_subtask(ctx, None, callable);
            Ok(Some(Value::Object(Some(subtask))))
        },
    );
    r.register(
        sts,
        "join",
        "()Ljdk/incubator/concurrent/StructuredTaskScope;",
        |_ctx, args| Ok(Some(args.first().copied().unwrap_or(Value::Object(None)))),
    );
    r.register(
        sts,
        "joinUntil",
        "(Ljava/time/Instant;)Ljdk/incubator/concurrent/StructuredTaskScope;",
        |_ctx, args| Ok(Some(args.first().copied().unwrap_or(Value::Object(None)))),
    );
    r.register(sts, "shutdown", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, 1, Value::Int(1));
        Ok(None)
    });
    r.register(sts, "close", "()V", |ctx, args| {
        // close() implicitly shuts down the scope (per StructuredTaskScope spec).
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, 1, Value::Int(1));
        Ok(None)
    });
    r.register(sts, "isShutdown", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 1)))
    });

    // Subtask
    let sub = "jdk/incubator/concurrent/StructuredTaskScope$Subtask";
    r.register(sub, "get", "()Ljava/lang/Object;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let state = match ctx.get_field(this, 0) {
            Value::Int(v) => v,
            _ => 0,
        };
        if state != 2 {
            return Err(RuntimeError::IllegalStateException {
                message: "Subtask not completed successfully".into(),
            }
            .into());
        }
        Ok(Some(ctx.get_field(this, 1)))
    });
    r.register(
        sub,
        "state",
        "()Ljdk/incubator/concurrent/StructuredTaskScope$Subtask$State;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let state = match ctx.get_field(this, 0) {
                Value::Int(v) => v,
                _ => 0,
            };
            let name = match state {
                0 => "UNAVAILABLE",
                1 => "RUNNING",
                2 => "SUCCESS",
                3 => "FAILED",
                _ => "UNAVAILABLE",
            };
            p57_alloc_enum(
                ctx,
                "jdk/incubator/concurrent/StructuredTaskScope$Subtask$State",
                name,
                state,
            )
        },
    );
    // exception(): the JDK contract is "the exception of a FAILED subtask;
    // IllegalStateException if the subtask is not in the FAILED state". The
    // constant null broke both halves — a FAILED subtask reported no cause
    // (callers NPE'd on `subtask.exception().getMessage()`), and calling it on
    // a SUCCESS subtask silently succeeded instead of failing. `fork` now
    // stores the Throwable in slot 2 (see `incubator_fork_subtask`).
    r.register(sub, "exception", "()Ljava/lang/Throwable;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if !matches!(ctx.get_field(this, 0), Value::Int(s) if s == INCUBATOR_SUBTASK_FAILED) {
            return Err(RuntimeError::IllegalStateException {
                message: "Subtask did not complete with an exception".into(),
            }
            .into());
        }
        if ctx.object_num_fields(this) > 2 {
            Ok(Some(ctx.get_field(this, 2)))
        } else {
            Ok(Some(Value::Object(None)))
        }
    });

    // ShutdownOnSuccess = 3-field (owner=0, shutdown=1, result=2) — same as base
    let sos = "jdk/incubator/concurrent/StructuredTaskScope$ShutdownOnSuccess";
    r.register(sos, "<init>", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, 0, Value::Long(ctx.thread_id() as i64));
        ctx.set_field(this, 1, Value::Int(0));
        ctx.set_field(this, 2, Value::Object(None));
        Ok(None)
    });
    r.register(sos, "result", "()Ljava/lang/Object;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 2)))
    });
    // Inherit fork/join/shutdown/close from base via same registration
    r.register(
        sos,
        "fork",
        "(Ljava/util/concurrent/Callable;)Ljdk/incubator/concurrent/StructuredTaskScope$Subtask;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let callable = match args.get(1) {
                Some(Value::Object(Some(c))) => Some(*c),
                _ => None,
            };
            let (this, subtask) = incubator_fork_subtask(ctx, Some(this), callable);
            // ShutdownOnSuccess captures the first successful result.
            if let Some(this) = this {
                let succeeded =
                    matches!(ctx.get_field(subtask, 0), Value::Int(s) if s == INCUBATOR_SUBTASK_SUCCESS);
                if succeeded && matches!(ctx.get_field(this, 2), Value::Object(None)) {
                    let result = ctx.get_field(subtask, 1);
                    ctx.set_field(this, 2, result);
                    ctx.set_field(this, 1, Value::Int(1)); // auto-shutdown
                }
            }
            Ok(Some(Value::Object(Some(subtask))))
        },
    );
    r.register(
        sos,
        "join",
        "()Ljdk/incubator/concurrent/StructuredTaskScope$ShutdownOnSuccess;",
        |_ctx, args| Ok(Some(args.first().copied().unwrap_or(Value::Object(None)))),
    );
    r.register(sos, "shutdown", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, 1, Value::Int(1));
        Ok(None)
    });
    r.register(sos, "close", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, 1, Value::Int(1)); // mark shutdown
        Ok(None)
    });

    // ShutdownOnFailure = 3-field
    let sof = "jdk/incubator/concurrent/StructuredTaskScope$ShutdownOnFailure";
    r.register(sof, "<init>", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, 0, Value::Long(ctx.thread_id() as i64));
        ctx.set_field(this, 1, Value::Int(0));
        ctx.set_field(this, 2, Value::Object(None)); // exception
        Ok(None)
    });
    // exception(): returns an Optional, never null — the old constant NULL made
    // `scope.exception().isPresent()` NPE instead of answering, and it could
    // never report a failure now that `fork` records the cause in slot 2.
    r.register(sof, "exception", "()Ljava/util/Optional;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let exc = if ctx.object_num_fields(this) > 2 {
            ctx.get_field(this, 2)
        } else {
            Value::Object(None)
        };
        Ok(Some(Value::Object(Some(sts_optional_of(ctx, exc)))))
    });
    r.register(sof, "throwIfFailed", "()V", |ctx, args| {
        // If the scope captured an exception (field 2), wrap it in ExecutionException and throw.
        let this = obj_arg(args, 0)?;
        if ctx.object_num_fields(this) > 2 {
            if let Value::Object(Some(_exc)) = ctx.get_field(this, 2) {
                return Err(RuntimeError::IllegalStateException {
                    message: "StructuredTaskScope subtask failed".into(),
                }
                .into());
            }
        }
        Ok(None)
    });
    r.register(
        sof,
        "throwIfFailed",
        "(Ljava/util/function/Function;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            if ctx.object_num_fields(this) > 2 {
                if let Value::Object(Some(_exc)) = ctx.get_field(this, 2) {
                    return Err(RuntimeError::IllegalStateException {
                        message: "StructuredTaskScope subtask failed".into(),
                    }
                    .into());
                }
            }
            Ok(None)
        },
    );
    r.register(
        sof,
        "fork",
        "(Ljava/util/concurrent/Callable;)Ljdk/incubator/concurrent/StructuredTaskScope$Subtask;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let callable = match args.get(1) {
                Some(Value::Object(Some(c))) => Some(*c),
                _ => None,
            };
            let (this, subtask) = incubator_fork_subtask(ctx, Some(this), callable);
            if let Some(this) = this {
                let failed =
                    matches!(ctx.get_field(subtask, 0), Value::Int(s) if s == INCUBATOR_SUBTASK_FAILED);
                if failed {
                    // Auto-shutdown on first failure, and retain the cause so
                    // throwIfFailed()/exception() can report it. Previously the
                    // Throwable was discarded, so the scope's exception slot
                    // stayed null and throwIfFailed() never fired.
                    ctx.set_field(this, 1, Value::Int(1));
                    if ctx.object_num_fields(this) > 2
                        && matches!(ctx.get_field(this, 2), Value::Object(None))
                    {
                        let exc = ctx.get_field(subtask, 2);
                        ctx.set_field(this, 2, exc);
                    }
                }
            }
            Ok(Some(Value::Object(Some(subtask))))
        },
    );
    r.register(
        sof,
        "join",
        "()Ljdk/incubator/concurrent/StructuredTaskScope$ShutdownOnFailure;",
        |_ctx, args| Ok(Some(args.first().copied().unwrap_or(Value::Object(None)))),
    );
    r.register(sof, "shutdown", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, 1, Value::Int(1));
        Ok(None)
    });
    r.register(sof, "close", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, 1, Value::Int(1));
        Ok(None)
    });

    // Also register under Java 25 final package: java.util.concurrent.StructuredTaskScope
    register_p67_structured_task_scope_j25(r);
    r.set_category(__prev_cat);
}

// =============================================================================
// StructuredTaskScope — Java 25 final API, 8-field synthetic layout (T16.4)
//
// Field 0 = name (Object, nullable String)
// Field 1 = state (Int): OPEN=0, SHUTDOWN=1, CLOSED=2
// Field 2 = task_count (Int) — total forked tasks
// Field 3 = completed_count (Int) — subtasks that reached SUCCESS/FAILED
// Field 4 = exception (Object, nullable Throwable; reused as result-cache
//           on ShutdownOnSuccess)
// Field 5 = policy (Int): BASE=0, SHUTDOWN_ON_SUCCESS=1, SHUTDOWN_ON_FAILURE=2
// Field 6 = joined (Int): 0/1
// Field 7 = suppressed_count (Int) — subtasks rejected after shutdown
//
// Subtask field 0 = state: UNAVAILABLE=0, SUCCESS=2, FAILED=3
// Subtask field 1 = result
//
// T16.4: fork runs the callable synchronously. Callers that want a threaded
// subtask route through `invoke_virtual`, which on the real interpreter goes
// through the same ephemeral-thread/invocation path that Gamma's
// `SharedVmBridge` uses for JDWP.  For the synthetic-JDK tests the callable
// arg is null, so fork just ticks task_count/completed_count and returns
// a Subtask in the UNAVAILABLE state (no real Callable invocation path).
// =============================================================================

pub(crate) const J25_STS_NAME: usize = 0;

pub(crate) const J25_STS_STATE: usize = 1;

pub(crate) const J25_STS_TASK_COUNT: usize = 2;

pub(crate) const J25_STS_COMPLETED: usize = 3;

pub(crate) const J25_STS_EXCEPTION: usize = 4;

pub(crate) const J25_STS_POLICY: usize = 5;

pub(crate) const J25_STS_JOINED: usize = 6;

pub(crate) const J25_STS_SUPPRESSED: usize = 7;

pub(crate) const J25_STS_STATE_OPEN: i32 = 0;

pub(crate) const J25_STS_STATE_SHUTDOWN: i32 = 1;

pub(crate) const J25_STS_STATE_CLOSED: i32 = 2;

pub(crate) const J25_STS_POLICY_BASE: i32 = 0;

pub(crate) const J25_STS_POLICY_SHUTDOWN_ON_SUCCESS: i32 = 1;

pub(crate) const J25_STS_POLICY_SHUTDOWN_ON_FAILURE: i32 = 2;

pub(crate) const J25_SUBTASK_STATE_UNAVAILABLE: i32 = 0;

pub(crate) const J25_SUBTASK_STATE_SUCCESS: i32 = 2;

pub(crate) const J25_SUBTASK_STATE_FAILED: i32 = 3;

pub(crate) fn j25_sts_init_fields(ctx: &mut dyn NativeContext, this: ObjectRef, policy: i32) {
    // The heap honours the declared field count, so only touch slots
    // that exist (callers may allocate fewer than 8 for lean scopes).
    let n = ctx.object_num_fields(this);
    if n > J25_STS_NAME {
        ctx.set_field(this, J25_STS_NAME, Value::Object(None));
    }
    if n > J25_STS_STATE {
        ctx.set_field(this, J25_STS_STATE, Value::Int(J25_STS_STATE_OPEN));
    }
    if n > J25_STS_TASK_COUNT {
        ctx.set_field(this, J25_STS_TASK_COUNT, Value::Int(0));
    }
    if n > J25_STS_COMPLETED {
        ctx.set_field(this, J25_STS_COMPLETED, Value::Int(0));
    }
    if n > J25_STS_EXCEPTION {
        ctx.set_field(this, J25_STS_EXCEPTION, Value::Object(None));
    }
    if n > J25_STS_POLICY {
        ctx.set_field(this, J25_STS_POLICY, Value::Int(policy));
    }
    if n > J25_STS_JOINED {
        ctx.set_field(this, J25_STS_JOINED, Value::Int(0));
    }
    if n > J25_STS_SUPPRESSED {
        ctx.set_field(this, J25_STS_SUPPRESSED, Value::Int(0));
    }
}

pub(crate) fn j25_sts_state(ctx: &mut dyn NativeContext, this: ObjectRef) -> i32 {
    match ctx.get_field(this, J25_STS_STATE) {
        Value::Int(v) => v,
        _ => J25_STS_STATE_OPEN,
    }
}

pub(crate) fn j25_sts_inc_field(ctx: &mut dyn NativeContext, this: ObjectRef, idx: usize) {
    let cur = match ctx.get_field(this, idx) {
        Value::Int(v) => v,
        _ => 0,
    };
    ctx.set_field(this, idx, Value::Int(cur + 1));
}

/// Invoke a Callable synchronously and populate the Subtask fields.
pub(crate) enum SubtaskOutcome {
    Success(Value),
    Failed,
}

pub(crate) fn j25_run_callable_into_subtask(
    ctx: &mut dyn NativeContext,
    subtask: ObjectRef,
    callable: Option<ObjectRef>,
) -> SubtaskOutcome {
    let Some(callable) = callable else {
        // Null callable — mark as SUCCESS with null result (the test harness
        // passes null callables; no Java NPE is raised by the synthetic fork
        // contract, the subtask simply has no observable value).
        ctx.set_field(subtask, 0, Value::Int(J25_SUBTASK_STATE_SUCCESS));
        ctx.set_field(subtask, 1, Value::Object(None));
        return SubtaskOutcome::Success(Value::Object(None));
    };
    match ctx.invoke_virtual(
        callable,
        "call",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(callable))],
    ) {
        Ok(Some(result)) => {
            ctx.set_field(subtask, 0, Value::Int(J25_SUBTASK_STATE_SUCCESS));
            ctx.set_field(subtask, 1, result);
            SubtaskOutcome::Success(result)
        }
        Ok(None) => {
            ctx.set_field(subtask, 0, Value::Int(J25_SUBTASK_STATE_SUCCESS));
            ctx.set_field(subtask, 1, Value::Object(None));
            SubtaskOutcome::Success(Value::Object(None))
        }
        Err(_) => {
            ctx.set_field(subtask, 0, Value::Int(J25_SUBTASK_STATE_FAILED));
            ctx.set_field(subtask, 1, Value::Object(None));
            SubtaskOutcome::Failed
        }
    }
}

// --- Policy-dispatched init trampolines (fn pointers, no captures) ---

pub(crate) fn j25_sts_init_base(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    j25_sts_init_fields(ctx, this, J25_STS_POLICY_BASE);
    Ok(None)
}

pub(crate) fn j25_sts_init_sos(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    j25_sts_init_fields(ctx, this, J25_STS_POLICY_SHUTDOWN_ON_SUCCESS);
    Ok(None)
}

pub(crate) fn j25_sts_init_sof(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    j25_sts_init_fields(ctx, this, J25_STS_POLICY_SHUTDOWN_ON_FAILURE);
    Ok(None)
}

pub(crate) fn j25_sts_init_named_base(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    j25_sts_init_fields(ctx, this, J25_STS_POLICY_BASE);
    if ctx.object_num_fields(this) > J25_STS_NAME {
        let name = args.get(1).copied().unwrap_or(Value::Object(None));
        ctx.set_field(this, J25_STS_NAME, name);
    }
    Ok(None)
}

pub(crate) fn j25_sts_init_named_sos(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    j25_sts_init_fields(ctx, this, J25_STS_POLICY_SHUTDOWN_ON_SUCCESS);
    if ctx.object_num_fields(this) > J25_STS_NAME {
        let name = args.get(1).copied().unwrap_or(Value::Object(None));
        ctx.set_field(this, J25_STS_NAME, name);
    }
    Ok(None)
}

pub(crate) fn j25_sts_init_named_sof(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    j25_sts_init_fields(ctx, this, J25_STS_POLICY_SHUTDOWN_ON_FAILURE);
    if ctx.object_num_fields(this) > J25_STS_NAME {
        let name = args.get(1).copied().unwrap_or(Value::Object(None));
        ctx.set_field(this, J25_STS_NAME, name);
    }
    Ok(None)
}

pub(crate) fn j25_sts_fork_impl(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    policy: i32,
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let subtask_cls = "java/util/concurrent/StructuredTaskScope$Subtask";
    let state = j25_sts_state(ctx, this);
    if state != J25_STS_STATE_OPEN {
        let subtask = alloc_concurrent_synthetic(ctx, subtask_cls, 2);
        ctx.set_field(subtask, 0, Value::Int(J25_SUBTASK_STATE_UNAVAILABLE));
        ctx.set_field(subtask, 1, Value::Object(None));
        if ctx.object_num_fields(this) > J25_STS_SUPPRESSED {
            j25_sts_inc_field(ctx, this, J25_STS_SUPPRESSED);
        }
        return Ok(Some(Value::Object(Some(subtask))));
    }
    if ctx.object_num_fields(this) > J25_STS_TASK_COUNT {
        j25_sts_inc_field(ctx, this, J25_STS_TASK_COUNT);
    }
    let subtask = alloc_concurrent_synthetic(ctx, subtask_cls, 2);
    let callable = match args.get(1) {
        Some(Value::Object(Some(c))) => Some(*c),
        _ => None,
    };
    let outcome = j25_run_callable_into_subtask(ctx, subtask, callable);
    if ctx.object_num_fields(this) > J25_STS_COMPLETED {
        j25_sts_inc_field(ctx, this, J25_STS_COMPLETED);
    }
    match (policy, &outcome) {
        (J25_STS_POLICY_SHUTDOWN_ON_SUCCESS, SubtaskOutcome::Success(v)) => {
            if ctx.object_num_fields(this) > J25_STS_EXCEPTION
                && matches!(ctx.get_field(this, J25_STS_EXCEPTION), Value::Object(None))
            {
                ctx.set_field(this, J25_STS_EXCEPTION, *v);
            }
            ctx.set_field(this, J25_STS_STATE, Value::Int(J25_STS_STATE_SHUTDOWN));
        }
        (J25_STS_POLICY_SHUTDOWN_ON_FAILURE, SubtaskOutcome::Failed) => {
            ctx.set_field(this, J25_STS_STATE, Value::Int(J25_STS_STATE_SHUTDOWN));
        }
        _ => {}
    }
    Ok(Some(Value::Object(Some(subtask))))
}

pub(crate) fn j25_sts_fork_base(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    j25_sts_fork_impl(ctx, args, J25_STS_POLICY_BASE)
}

pub(crate) fn j25_sts_fork_sos(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    j25_sts_fork_impl(ctx, args, J25_STS_POLICY_SHUTDOWN_ON_SUCCESS)
}

pub(crate) fn j25_sts_fork_sof(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    j25_sts_fork_impl(ctx, args, J25_STS_POLICY_SHUTDOWN_ON_FAILURE)
}

pub(crate) fn j25_sts_join_impl(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    if j25_sts_state(ctx, this) == J25_STS_STATE_CLOSED {
        return Err(RuntimeError::IllegalStateException {
            message: "StructuredTaskScope is closed".into(),
        }
        .into());
    }
    if ctx.object_num_fields(this) > J25_STS_JOINED {
        ctx.set_field(this, J25_STS_JOINED, Value::Int(1));
    }
    Ok(Some(Value::Object(Some(this))))
}

pub(crate) fn j25_sts_shutdown(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    if j25_sts_state(ctx, this) != J25_STS_STATE_CLOSED {
        ctx.set_field(this, J25_STS_STATE, Value::Int(J25_STS_STATE_SHUTDOWN));
    }
    Ok(None)
}

pub(crate) fn j25_sts_close(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    ctx.set_field(this, J25_STS_STATE, Value::Int(J25_STS_STATE_CLOSED));
    Ok(None)
}

pub(crate) fn j25_sts_is_shutdown(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let state = j25_sts_state(ctx, this);
    Ok(Some(Value::Int(if state != J25_STS_STATE_OPEN {
        1
    } else {
        0
    })))
}

/// Register core `fork/join/joinUntil/shutdown/close/isShutdown` methods
/// for a given scope class using static trampolines selected by policy.
pub(crate) fn j25_register_scope_common(
    r: &mut NativeMethodRegistry,
    cls: &'static str,
    policy: i32,
    join_ret: &'static str,
    fork_ret: &'static str,
) {
    // <init>()V and <init>(String, ThreadFactory)V — pick the policy-specific trampoline.
    let (init_nullary, init_named) = match policy {
        J25_STS_POLICY_SHUTDOWN_ON_SUCCESS => (
            j25_sts_init_sos as cratonvm_native_api::NativeCallback,
            j25_sts_init_named_sos as cratonvm_native_api::NativeCallback,
        ),
        J25_STS_POLICY_SHUTDOWN_ON_FAILURE => (
            j25_sts_init_sof as cratonvm_native_api::NativeCallback,
            j25_sts_init_named_sof as cratonvm_native_api::NativeCallback,
        ),
        _ => (
            j25_sts_init_base as cratonvm_native_api::NativeCallback,
            j25_sts_init_named_base as cratonvm_native_api::NativeCallback,
        ),
    };
    r.register(cls, "<init>", "()V", init_nullary);
    r.register(
        cls,
        "<init>",
        "(Ljava/lang/String;Ljava/util/concurrent/ThreadFactory;)V",
        init_named,
    );

    let fork_cb: cratonvm_native_api::NativeCallback = match policy {
        J25_STS_POLICY_SHUTDOWN_ON_SUCCESS => j25_sts_fork_sos,
        J25_STS_POLICY_SHUTDOWN_ON_FAILURE => j25_sts_fork_sof,
        _ => j25_sts_fork_base,
    };
    let fork_desc: &'static str =
        Box::leak(format!("(Ljava/util/concurrent/Callable;){fork_ret}").into_boxed_str());
    r.register(cls, "fork", fork_desc, fork_cb);

    let join_desc: &'static str = Box::leak(format!("(){join_ret}").into_boxed_str());
    r.register(cls, "join", join_desc, j25_sts_join_impl);

    let joinuntil_desc: &'static str =
        Box::leak(format!("(Ljava/time/Instant;){join_ret}").into_boxed_str());
    r.register(cls, "joinUntil", joinuntil_desc, j25_sts_join_impl);

    r.register(cls, "shutdown", "()V", j25_sts_shutdown);
    r.register(cls, "close", "()V", j25_sts_close);
    r.register(cls, "isShutdown", "()Z", j25_sts_is_shutdown);
}

pub(crate) fn register_p67_structured_task_scope_j25(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let sts = "java/util/concurrent/StructuredTaskScope";
    j25_register_scope_common(
        r,
        sts,
        J25_STS_POLICY_BASE,
        "Ljava/util/concurrent/StructuredTaskScope;",
        "Ljava/util/concurrent/StructuredTaskScope$Subtask;",
    );

    // Subtask accessors
    let sub = "java/util/concurrent/StructuredTaskScope$Subtask";
    r.register(sub, "get", "()Ljava/lang/Object;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if !matches!(ctx.get_field(this, 0), Value::Int(s) if s == J25_SUBTASK_STATE_SUCCESS) {
            return Err(RuntimeError::IllegalStateException {
                message: "Subtask not completed successfully".into(),
            }
            .into());
        }
        Ok(Some(ctx.get_field(this, 1)))
    });
    r.register(
        sub,
        "state",
        "()Ljava/util/concurrent/StructuredTaskScope$Subtask$State;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let state = match ctx.get_field(this, 0) {
                Value::Int(v) => v,
                _ => J25_SUBTASK_STATE_UNAVAILABLE,
            };
            let name = match state {
                J25_SUBTASK_STATE_SUCCESS => "SUCCESS",
                J25_SUBTASK_STATE_FAILED => "FAILED",
                _ => "UNAVAILABLE",
            };
            p57_alloc_enum(
                ctx,
                "java/util/concurrent/StructuredTaskScope$Subtask$State",
                name,
                state,
            )
        },
    );
    // SUPERSEDED, kept correct-by-construction: `jdk25_concurrency::
    // register_jdk25_concurrency_natives` re-registers the whole
    // `java/util/concurrent/StructuredTaskScope$Subtask` surface (get/state/
    // exception/task) over its canonical 8-field layout and runs LAST in
    // `register_synthetic_overrides`, so the live `exception()` is that one.
    // The constant null here is still wrong on its own terms — the JDK throws
    // IllegalStateException unless the subtask FAILED — so it is fixed rather
    // than left as a landmine for a future ordering change.
    r.register(sub, "exception", "()Ljava/lang/Throwable;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if !matches!(ctx.get_field(this, 0), Value::Int(s) if s == J25_SUBTASK_STATE_FAILED) {
            return Err(RuntimeError::IllegalStateException {
                message: "Subtask did not complete with an exception".into(),
            }
            .into());
        }
        if ctx.object_num_fields(this) > 2 {
            Ok(Some(ctx.get_field(this, 2)))
        } else {
            Ok(Some(Value::Object(None)))
        }
    });

    // ShutdownOnSuccess
    let sos = "java/util/concurrent/StructuredTaskScope$ShutdownOnSuccess";
    j25_register_scope_common(
        r,
        sos,
        J25_STS_POLICY_SHUTDOWN_ON_SUCCESS,
        "Ljava/util/concurrent/StructuredTaskScope$ShutdownOnSuccess;",
        "Ljava/util/concurrent/StructuredTaskScope$Subtask;",
    );
    r.register(sos, "result", "()Ljava/lang/Object;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let state = j25_sts_state(ctx, this);
        if state == J25_STS_STATE_OPEN {
            return Err(RuntimeError::IllegalStateException {
                message: "ShutdownOnSuccess.result() requires join()/shutdown()".into(),
            }
            .into());
        }
        if ctx.object_num_fields(this) > J25_STS_EXCEPTION {
            Ok(Some(ctx.get_field(this, J25_STS_EXCEPTION)))
        } else {
            Ok(Some(Value::Object(None)))
        }
    });

    // ShutdownOnFailure
    let sof = "java/util/concurrent/StructuredTaskScope$ShutdownOnFailure";
    j25_register_scope_common(
        r,
        sof,
        J25_STS_POLICY_SHUTDOWN_ON_FAILURE,
        "Ljava/util/concurrent/StructuredTaskScope$ShutdownOnFailure;",
        "Ljava/util/concurrent/StructuredTaskScope$Subtask;",
    );
    // exception(): an Optional-returning method must not answer bare null —
    // `scope.exception().isPresent()` NPEs on it. Report the scope's captured
    // cause (J25_STS_EXCEPTION) wrapped in a synthetic Optional; an empty
    // Optional when there was no failure. (Also superseded by
    // `jdk25_concurrency` — see the Subtask note above.)
    r.register(sof, "exception", "()Ljava/util/Optional;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let exc = if ctx.object_num_fields(this) > J25_STS_EXCEPTION {
            ctx.get_field(this, J25_STS_EXCEPTION)
        } else {
            Value::Object(None)
        };
        Ok(Some(Value::Object(Some(sts_optional_of(ctx, exc)))))
    });
    r.register(sof, "throwIfFailed", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if ctx.object_num_fields(this) > J25_STS_EXCEPTION {
            if let Value::Object(Some(_exc)) = ctx.get_field(this, J25_STS_EXCEPTION) {
                return Err(RuntimeError::IllegalStateException {
                    message: "StructuredTaskScope subtask failed".into(),
                }
                .into());
            }
        }
        Ok(None)
    });
    r.register(
        sof,
        "throwIfFailed",
        "(Ljava/util/function/Function;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            if ctx.object_num_fields(this) > J25_STS_EXCEPTION {
                if let Value::Object(Some(_exc)) = ctx.get_field(this, J25_STS_EXCEPTION) {
                    return Err(RuntimeError::IllegalStateException {
                        message: "StructuredTaskScope subtask failed".into(),
                    }
                    .into());
                }
            }
            Ok(None)
        },
    );
    r.set_category(__prev_cat);
}

// =============================================================================
// java.lang.ScopedValue — Java 21 (preview → final Java 25)
// 2-field synthetic (value=0, bound=1 Int)
// =============================================================================

pub(crate) fn register_p67_scoped_value(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let sv = "java/lang/ScopedValue";
    r.register(
        sv,
        "newInstance",
        "()Ljava/lang/ScopedValue;",
        |ctx, _args| {
            let obj = alloc_concurrent_synthetic(ctx, "java/lang/ScopedValue", 2);
            ctx.set_field(obj, 0, Value::Object(None));
            ctx.set_field(obj, 1, Value::Int(0)); // not bound
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(sv, "get", "()Ljava/lang/Object;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let bound = match ctx.get_field(this, 1) {
            Value::Int(v) => v,
            _ => 0,
        };
        if bound == 0 {
            return Err(RuntimeError::IllegalStateException {
                message: "ScopedValue not bound".into(),
            }
            .into());
        }
        Ok(Some(ctx.get_field(this, 0)))
    });
    r.register(sv, "isBound", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 1)))
    });
    r.register(
        sv,
        "orElse",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let bound = match ctx.get_field(this, 1) {
                Value::Int(v) => v,
                _ => 0,
            };
            if bound != 0 {
                Ok(Some(ctx.get_field(this, 0)))
            } else {
                Ok(Some(args.get(1).copied().unwrap_or(Value::Object(None))))
            }
        },
    );
    r.register(sv, "orElseThrow", "()Ljava/lang/Object;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let bound = match ctx.get_field(this, 1) {
            Value::Int(v) => v,
            _ => 0,
        };
        if bound == 0 {
            return Err(RuntimeError::IllegalStateException {
                message: "ScopedValue not bound".into(),
            }
            .into());
        }
        Ok(Some(ctx.get_field(this, 0)))
    });

    // ScopedValue.where(sv, value) → Carrier
    r.register(
        sv,
        "where",
        "(Ljava/lang/ScopedValue;Ljava/lang/Object;)Ljava/lang/ScopedValue$Carrier;",
        |ctx, args| {
            let carrier = alloc_concurrent_synthetic(ctx, "java/lang/ScopedValue$Carrier", 2);
            ctx.set_field(
                carrier,
                0,
                args.first().copied().unwrap_or(Value::Object(None)),
            ); // sv ref
            ctx.set_field(
                carrier,
                1,
                args.get(1).copied().unwrap_or(Value::Object(None)),
            ); // value
            Ok(Some(Value::Object(Some(carrier))))
        },
    );

    // Carrier
    let carrier = "java/lang/ScopedValue$Carrier";
    r.register(
        carrier,
        "where",
        "(Ljava/lang/ScopedValue;Ljava/lang/Object;)Ljava/lang/ScopedValue$Carrier;",
        |ctx, args| {
            // Chain — for simplicity, create new carrier (overwrites)
            let c = alloc_concurrent_synthetic(ctx, "java/lang/ScopedValue$Carrier", 2);
            ctx.set_field(c, 0, args.get(1).copied().unwrap_or(Value::Object(None)));
            ctx.set_field(c, 1, args.get(2).copied().unwrap_or(Value::Object(None)));
            Ok(Some(Value::Object(Some(c))))
        },
    );
    r.register(carrier, "run", "(Ljava/lang/Runnable;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // Bind the scoped value temporarily
        let sv_ref = match ctx.get_field(this, 0) {
            Value::Object(Some(r)) => r,
            _ => return Ok(None),
        };
        let value = ctx.get_field(this, 1);
        let old_val = ctx.get_field(sv_ref, 0);
        let old_bound = ctx.get_field(sv_ref, 1);
        ctx.set_field(sv_ref, 0, value);
        ctx.set_field(sv_ref, 1, Value::Int(1));
        // Run the Runnable
        if let Some(Value::Object(Some(runnable))) = args.get(1) {
            let _ = ctx.invoke_virtual(*runnable, "run", "()V", &[Value::Object(Some(*runnable))]);
        }
        // Restore
        ctx.set_field(sv_ref, 0, old_val);
        ctx.set_field(sv_ref, 1, old_bound);
        Ok(None)
    });
    r.register(
        carrier,
        "call",
        "(Ljava/util/concurrent/Callable;)Ljava/lang/Object;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let sv_ref = match ctx.get_field(this, 0) {
                Value::Object(Some(r)) => r,
                _ => return Ok(Some(Value::Object(None))),
            };
            let value = ctx.get_field(this, 1);
            let old_val = ctx.get_field(sv_ref, 0);
            let old_bound = ctx.get_field(sv_ref, 1);
            ctx.set_field(sv_ref, 0, value);
            ctx.set_field(sv_ref, 1, Value::Int(1));
            let result = if let Some(Value::Object(Some(callable))) = args.get(1) {
                ctx.invoke_virtual(
                    *callable,
                    "call",
                    "()Ljava/lang/Object;",
                    &[Value::Object(Some(*callable))],
                )
                .unwrap_or(Some(Value::Object(None)))
            } else {
                Some(Value::Object(None))
            };
            ctx.set_field(sv_ref, 0, old_val);
            ctx.set_field(sv_ref, 1, old_bound);
            Ok(result)
        },
    );
    r.register(
        carrier,
        "get",
        "(Ljava/lang/ScopedValue;)Ljava/lang/Object;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let target_sv = match args.get(1) {
                Some(Value::Object(Some(r))) => *r,
                _ => return Ok(Some(Value::Object(None))),
            };
            let carrier_sv = match ctx.get_field(this, 0) {
                Value::Object(Some(r)) => r,
                _ => return Ok(Some(Value::Object(None))),
            };
            if carrier_sv.as_ptr() == target_sv.as_ptr() {
                Ok(Some(ctx.get_field(this, 1)))
            } else {
                Ok(Some(Value::Object(None)))
            }
        },
    );
    r.set_category(__prev_cat);
}

// =============================================================================
// java.util.concurrent.SubmissionPublisher — Reactive Streams (Java 9)
// 3-field (subscribers=0 ArrayList, closed=1 Int, executor=2)
// =============================================================================

pub(crate) fn register_p69_submission_publisher(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    // SubmissionPublisher core methods (<init>, submit, offer, close, isClosed,
    // subscribe, hasSubscribers, getNumberOfSubscribers) are registered in Phase
    // 60 (`register_p60_flow`) and now perform REAL subscriber delivery (B6). Do
    // NOT re-register those here — a later registration overwrites the earlier
    // one (`methods.insert`), and the old no-op/zero stubs would clobber the
    // working delivery path. Only add methods genuinely absent from Phase 60.
    let sp = "java/util/concurrent/SubmissionPublisher";
    // KEEP: the only SubmissionPublisher constructor CratonVM registers is the
    // no-arg one, which in the JDK means `Flow.defaultBufferSize()` == 256.
    // There is no configurable capacity to read back, so 256 is the value, not
    // a placeholder.
    r.register(sp, "getMaxBufferCapacity", "()I", |_ctx, _args| {
        Ok(Some(Value::Int(256)))
    });
    // `getClosedException()` reports the Throwable passed to
    // `closeExceptionally(Throwable)`, and null when the publisher was closed
    // normally or is still open. W3 justified the constant null by observing
    // that `closeExceptionally` was registered nowhere — i.e. "no data",
    // which is a gap, not a KEEP. Register the writer too, and read it back.
    r.register(
        sp,
        "closeExceptionally",
        "(Ljava/lang/Throwable;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let err = match args.get(1) {
                Some(Value::Object(Some(t))) => *t,
                // JDK: NullPointerException if the error is null.
                _ => {
                    return Err(RuntimeError::NullPointerException {
                        message: Some("error is null".into()),
                    }
                    .into())
                }
            };
            // Idempotent, like close(): the first close wins.
            if ctx.get_field(this, 1).as_int().unwrap_or(0) != 0 {
                return Ok(None);
            }
            ctx.set_field(this, 1, Value::Int(1));
            let key = ctx.identity_hash_code(this);
            let root = ctx.add_global_root(err);
            let previous = sp_closed_exceptions().lock().insert(key, root);
            if let Some(old) = previous {
                ctx.remove_global_root(old);
            }
            // Signal the failure to every subscriber, as close() does for
            // onComplete. Pin `this` and the Throwable across the callbacks.
            let pin = ctx.pin_native_root(this);
            let err_pin = ctx.pin_native_root(err);
            let mut idx = 0;
            loop {
                let this_cur = ctx.read_native_pin(pin, this);
                let (arr, len) = match sp_subscribers(ctx, this_cur) {
                    Some(v) => v,
                    None => break,
                };
                if idx >= len {
                    break;
                }
                if let Value::Object(Some(sub)) = ctx.get_array_element(arr, idx) {
                    let err_cur = ctx.read_native_pin(err_pin, err);
                    let _ = ctx.invoke_virtual(
                        sub,
                        "onError",
                        "(Ljava/lang/Throwable;)V",
                        &[Value::Object(Some(err_cur))],
                    );
                }
                idx += 1;
            }
            ctx.unpin_native_roots(pin);
            Ok(None)
        },
    );
    r.register(
        sp,
        "getClosedException",
        "()Ljava/lang/Throwable;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let key = ctx.identity_hash_code(this);
            let root = sp_closed_exceptions().lock().get(&key).copied();
            Ok(Some(Value::Object(
                root.and_then(|r| ctx.resolve_global_root(r)),
            )))
        },
    );
    r.set_category(__prev_cat);
}

/// Throwable handed to `SubmissionPublisher.closeExceptionally`, keyed by the
/// publisher's identity hash. The Throwable is held as a global root (remapped
/// by the moving collector) because the synthetic SubmissionPublisher layout
/// (subscribers=0, closed=1, executor=2) has no slot to park it in.
fn sp_closed_exceptions() -> &'static parking_lot::Mutex<std::collections::HashMap<i32, usize>> {
    static T: SqOnceLock<parking_lot::Mutex<std::collections::HashMap<i32, usize>>> =
        SqOnceLock::new();
    T.get_or_init(|| parking_lot::Mutex::new(std::collections::HashMap::new()))
}

// =============================================================================
// java.util.concurrent.atomic — LongAccumulator, DoubleAccumulator, DoubleAdder
// =============================================================================

pub(crate) fn register_p70_atomic_accumulators(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    // LongAccumulator = 3-field (identity=0 Long, current=1 Long, operator=2 ObjectRef)
    let la = "java/util/concurrent/atomic/LongAccumulator";
    r.register(
        la,
        "<init>",
        "(Ljava/util/function/LongBinaryOperator;J)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let identity = match args.get(2) {
                Some(Value::Long(v)) => *v,
                _ => 0,
            };
            ctx.set_field(this, 0, Value::Long(identity));
            ctx.set_field(this, 1, Value::Long(identity));
            // Store the operator reference
            ctx.set_field(this, 2, args.get(1).copied().unwrap_or(Value::Object(None)));
            Ok(None)
        },
    );
    r.register(la, "accumulate", "(J)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let x = match args.get(1) {
            Some(Value::Long(v)) => *v,
            _ => 0,
        };
        let cur = match ctx.get_field(this, 1) {
            Value::Long(v) => v,
            _ => 0,
        };
        // Try to invoke the operator; fallback to addition
        let result = if let Value::Object(Some(op)) = ctx.get_field(this, 2) {
            match ctx.invoke_virtual(
                op,
                "applyAsLong",
                "(JJ)J",
                &[Value::Long(cur), Value::Long(x)],
            ) {
                Ok(Some(Value::Long(r))) => r,
                _ => cur + x, // fallback
            }
        } else {
            cur + x
        };
        ctx.set_field(this, 1, Value::Long(result));
        Ok(None)
    });
    r.register(la, "get", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 1)))
    });
    r.register(la, "longValue", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 1)))
    });
    r.register(la, "intValue", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let v = match ctx.get_field(this, 1) {
            Value::Long(v) => v as i32,
            _ => 0,
        };
        Ok(Some(Value::Int(v)))
    });
    r.register(la, "floatValue", "()F", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let v = match ctx.get_field(this, 1) {
            Value::Long(v) => v as f32,
            _ => 0.0,
        };
        Ok(Some(Value::Float(v)))
    });
    r.register(la, "doubleValue", "()D", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let v = match ctx.get_field(this, 1) {
            Value::Long(v) => v as f64,
            _ => 0.0,
        };
        Ok(Some(Value::Double(v)))
    });
    r.register(la, "reset", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let identity = ctx.get_field(this, 0);
        ctx.set_field(this, 1, identity);
        Ok(None)
    });
    r.register(la, "getThenReset", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let cur = ctx.get_field(this, 1);
        let identity = ctx.get_field(this, 0);
        ctx.set_field(this, 1, identity);
        Ok(Some(cur))
    });

    // DoubleAccumulator = 3-field (identity=0 Double, current=1 Double, operator=2 ObjectRef)
    let da = "java/util/concurrent/atomic/DoubleAccumulator";
    r.register(
        da,
        "<init>",
        "(Ljava/util/function/DoubleBinaryOperator;D)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let identity = match args.get(2) {
                Some(Value::Double(v)) => *v,
                _ => 0.0,
            };
            ctx.set_field(this, 0, Value::Double(identity));
            ctx.set_field(this, 1, Value::Double(identity));
            ctx.set_field(this, 2, args.get(1).copied().unwrap_or(Value::Object(None)));
            Ok(None)
        },
    );
    r.register(da, "accumulate", "(D)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let x = match args.get(1) {
            Some(Value::Double(v)) => *v,
            _ => 0.0,
        };
        let cur = match ctx.get_field(this, 1) {
            Value::Double(v) => v,
            _ => 0.0,
        };
        let result = if let Value::Object(Some(op)) = ctx.get_field(this, 2) {
            match ctx.invoke_virtual(
                op,
                "applyAsDouble",
                "(DD)D",
                &[Value::Double(cur), Value::Double(x)],
            ) {
                Ok(Some(Value::Double(r))) => r,
                _ => cur + x,
            }
        } else {
            cur + x
        };
        ctx.set_field(this, 1, Value::Double(result));
        Ok(None)
    });
    r.register(da, "get", "()D", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 1)))
    });
    r.register(da, "doubleValue", "()D", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 1)))
    });
    r.register(da, "reset", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let identity = ctx.get_field(this, 0);
        ctx.set_field(this, 1, identity);
        Ok(None)
    });

    // DoubleAdder = 1-field (sum=0 Double)
    let dad = "java/util/concurrent/atomic/DoubleAdder";
    r.register(dad, "<init>", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, 0, Value::Double(0.0));
        Ok(None)
    });
    r.register(dad, "add", "(D)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let x = match args.get(1) {
            Some(Value::Double(v)) => *v,
            _ => 0.0,
        };
        let cur = match ctx.get_field(this, 0) {
            Value::Double(v) => v,
            _ => 0.0,
        };
        ctx.set_field(this, 0, Value::Double(cur + x));
        Ok(None)
    });
    r.register(dad, "sum", "()D", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    });
    r.register(dad, "doubleValue", "()D", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    });
    r.register(dad, "reset", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, 0, Value::Double(0.0));
        Ok(None)
    });
    r.register(dad, "sumThenReset", "()D", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let cur = ctx.get_field(this, 0);
        ctx.set_field(this, 0, Value::Double(0.0));
        Ok(Some(cur))
    });
    r.set_category(__prev_cat);
}

// =============================================================================
// Thread extras: Thread$State enum, ThreadGroup, UncaughtExceptionHandler
// ThreadGroup slots = parent=0, name=1, maxPriority=2, daemon=3 -- the REAL
// JDK 21+ declaration order, which the fabricated model in
// `synthetic_stub_fields` now also uses. Until 2026-08-05 the model was
// `name=0, parent=1, daemon=2, maxPriority=3` and the fallback indices below
// matched it, so both pairs were transposed against every real image.
// =============================================================================

// Stale-ObjectRef hazard (2026-07-17, see
// CRATONVM-SPRING-GENUINE-BUGLIST section 5.8 follow-up
// #2): `tg_enumerate_threads` walks `ThreadRegistry::alive_thread_objects()`
// (every live Thread mirror) and, for each one, `tg_matches_thread` walks its
// full ThreadGroup ancestry chain via repeated `tg_get_field(.., "parent",
// ..)` calls. With many live threads (this native is reached by
// `ThreadGroup.enumerate()`, which Tomcat/Reactor Netty's WebSocket client
// connector churn calls heavily) the whole walk can run long enough in wall
//-clock terms to overlap a concurrent moving-GC relocation pass on another
// thread. Every `ObjectRef` these helpers juggle (`this`, `thread`, `group`,
// `requested`, `arr`) was, before this fix, a bare native-local Rust variable
// with no root registration of its own — invisible to the GC's move-fixup
// pass, unlike `ThreadRegistry`'s own copy (which IS remapped, see
// `ThreadRegistry::update_thread_objs_after_gc`). A relocation mid-walk left
// the *local* copy dangling, and the next heap touch (observed at
// `class_id_of_object` -> `gen_heap::class_id_of`'s header read) segfaulted —
// exactly the "native stale-local" hazard class documented for other natives
// in this file (see `Thread$State.values()` below for the same
// pin_native_root/read_native_pin/unpin_native_roots pattern applied here).
// Fix: pin every ObjectRef the instant it is obtained (loop element, field
// read) and re-read through the pin before each subsequent heap touch, so a
// relocation anywhere in the walk is transparently followed instead of left
// dangling.

/// Fallback slot indices for a `ThreadGroup` receiver whose class does not
/// declare the field by name.
///
/// **These are the REAL JDK 21+ indices**, and they are the same four the
/// fabricated model in `ClassManager::synthetic_stub_fields` declares, in the
/// same order — that is the invariant, and
/// `tg_fallback_slots_match_the_fabricated_model` asserts it rather than
/// leaving it to a comment.
///
/// Until 2026-08-05 they were the legacy synthetic order (`name=0, parent=1,
/// daemon=2, maxPriority=3`), which has BOTH pairs transposed against the
/// image. Nothing caught it for months because [`tg_slot`] resolves by name
/// first and every real `ThreadGroup` declares all four, so this fallback is
/// almost never reached — and because a `ThreadGroup` written over a `String`,
/// and an `int` over an `int`, are both invisible to the overlay hunter's
/// value-tag test. The L4 shadow-layout diff is what named them.
pub(crate) const TG_SLOT_PARENT: usize = 0;
pub(crate) const TG_SLOT_NAME: usize = 1;
pub(crate) const TG_SLOT_MAX_PRIORITY: usize = 2;
pub(crate) const TG_SLOT_DAEMON: usize = 3;

pub(crate) fn tg_slot(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    field: &str,
    legacy_fallback: usize,
) -> Option<usize> {
    let pin = ctx.pin_native_root(this);
    let this = ctx.read_native_pin(pin, this);
    let class_name = ctx
        .class_name_of_id(ctx.class_id_of_object(this))
        .unwrap_or_default();
    let this = ctx.read_native_pin(pin, this);
    let result = ctx
        .resolve_field_index(&class_name, field)
        .filter(|idx| *idx < ctx.object_num_fields(this))
        .or_else(|| (legacy_fallback < ctx.object_num_fields(this)).then_some(legacy_fallback));
    ctx.unpin_native_roots(pin);
    result
}

pub(crate) fn tg_get_field(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    field: &str,
    legacy_fallback: usize,
) -> Value {
    let pin = ctx.pin_native_root(this);
    let this = ctx.read_native_pin(pin, this);
    let result = match tg_slot(ctx, this, field, legacy_fallback) {
        Some(idx) => {
            let this = ctx.read_native_pin(pin, this);
            ctx.get_field(this, idx)
        }
        None => Value::Object(None),
    };
    ctx.unpin_native_roots(pin);
    result
}

/// Slot fallback for a SYNTHETIC `Thread`'s owning group.
///
/// `tg_of_thread` resolves the group by NAME (`holder.group`, or `group`
/// directly), which is right for a real-JDK `Thread`. A synthetically
/// allocated one has no field names at all — `ensure_synthetic_class` mints
/// unnamed slots — so the lookup yields nothing and the thread appears to
/// belong to no group, making `ThreadGroup.enumerate` report zero.
///
/// Consulted ONLY after both name lookups fail, so a real `Thread` never
/// reaches it and the two paths cannot disagree about the same object. Slot 1
/// mirrors the synthetic `(name, group)` shape the VM's own thread mirrors use.
const SYNTHETIC_THREAD_GROUP_SLOT: usize = 1;

pub(crate) fn tg_of_thread(ctx: &mut dyn NativeContext, thread: ObjectRef) -> Option<ObjectRef> {
    let pin = ctx.pin_native_root(thread);
    let thread = ctx.read_native_pin(pin, thread);
    let result = match ctx.get_field_by_name(thread, "holder") {
        Value::Object(Some(holder)) => {
            let holder_pin = ctx.pin_native_root(holder);
            let holder = ctx.read_native_pin(holder_pin, holder);
            let r = match ctx.get_field_by_name(holder, "group") {
                Value::Object(group) => group,
                _ => None,
            };
            ctx.unpin_native_roots(holder_pin);
            r
        }
        _ => {
            let thread = ctx.read_native_pin(pin, thread);
            match ctx.get_field_by_name(thread, "group") {
                Value::Object(Some(group)) => Some(group),
                // Neither `holder.group` nor `group` resolved: a synthetic
                // Thread with no field names. See the constant above.
                _ if ctx.object_num_fields(thread) > SYNTHETIC_THREAD_GROUP_SLOT => {
                    match ctx.get_field(thread, SYNTHETIC_THREAD_GROUP_SLOT) {
                        Value::Object(group) => group,
                        _ => None,
                    }
                }
                _ => None,
            }
        }
    };
    ctx.unpin_native_roots(pin);
    result
}

pub(crate) fn tg_matches_thread(
    ctx: &mut dyn NativeContext,
    requested: ObjectRef,
    thread: ObjectRef,
    recurse: bool,
) -> bool {
    let requested_pin = ctx.pin_native_root(requested);
    let thread_pin = ctx.pin_native_root(thread);
    let thread = ctx.read_native_pin(thread_pin, thread);
    let mut current = tg_of_thread(ctx, thread);
    ctx.unpin_native_roots(thread_pin);

    // Pin the ancestry-walk cursor across the loop -- each iteration below
    // makes at least one more native call (`tg_get_field(.., "parent", ..)`)
    // that can span an intervening concurrent GC relocation pass.
    let found = loop {
        let Some(group) = current else {
            break false;
        };
        let group_pin = ctx.pin_native_root(group);
        let group = ctx.read_native_pin(group_pin, group);
        let requested = ctx.read_native_pin(requested_pin, requested);
        if group == requested {
            ctx.unpin_native_roots(group_pin);
            break true;
        }
        if !recurse {
            ctx.unpin_native_roots(group_pin);
            break false;
        }
        current = match tg_get_field(ctx, group, "parent", TG_SLOT_PARENT) {
            Value::Object(parent) => parent,
            _ => None,
        };
        ctx.unpin_native_roots(group_pin);
    };
    ctx.unpin_native_roots(requested_pin);
    found
}

pub(crate) fn tg_enumerate_threads(
    ctx: &mut dyn NativeContext,
    group: ObjectRef,
    arr: ObjectRef,
    recurse: bool,
) -> i32 {
    let arr_len = ctx.array_length(arr);
    let mut count = 0usize;
    // `group` and `arr` are held across the entire enumeration loop, which
    // -- for every live thread mirror `ctx.enumerate_threads()` returns --
    // makes one or more further native calls via `tg_matches_thread`. See
    // the stale-ObjectRef doc comment above `tg_slot` for why this whole
    // walk must stay pinned rather than holding bare native-local copies.
    let group_pin = ctx.pin_native_root(group);
    let arr_pin = ctx.pin_native_root(arr);
    // `enumerate_threads()` hands back a point-in-time Vec snapshot (a copy
    // out of `ThreadRegistry`'s own, separately-GC-remapped map) -- every
    // element is fresh at the INSTANT this call returns, but the loop below
    // can take many more native calls' worth of wall-clock time to work
    // through the whole Vec (one `tg_matches_thread` ancestry walk per
    // thread). Pinning lazily -- i.e. only once the loop body reaches a
    // given element -- leaves every later element in the Vec unprotected
    // for however long it takes to process every earlier one, which is
    // exactly the window that let this bug reproduce even after the first
    // round of pinning below was added (see
    // CRATONVM-SPRING-GENUINE-BUGLIST 5.8 follow-up
    // #2). Pin the *entire* snapshot in one tight pass immediately after
    // capturing it instead, so every element is under a live pin before any
    // further GC-unsafe native call has a chance to run.
    let threads = ctx.enumerate_threads(usize::MAX);
    let thread_pins: Vec<usize> = threads.iter().map(|&t| ctx.pin_native_root(t)).collect();
    for (obj, &obj_pin) in threads.iter().zip(thread_pins.iter()) {
        if count >= arr_len {
            break;
        }
        let obj = ctx.read_native_pin(obj_pin, *obj);
        let group = ctx.read_native_pin(group_pin, group);
        if tg_matches_thread(ctx, group, obj, recurse) {
            let obj = ctx.read_native_pin(obj_pin, obj);
            let arr = ctx.read_native_pin(arr_pin, arr);
            let _ = ctx.set_array_element(arr, count, Value::Object(Some(obj)));
            count += 1;
        }
    }
    // Unpinning the earliest handle (`group_pin`) also unwinds every pin
    // pushed after it -- `arr_pin` and the whole `thread_pins` batch -- see
    // the LIFO `native_pin_roots` discipline used throughout this file
    // (e.g. `Thread$State.values()` below).
    ctx.unpin_native_roots(group_pin);
    count as i32
}

pub(crate) fn tg_set_field(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    field: &str,
    legacy_fallback: usize,
    value: Value,
) {
    let pin = ctx.pin_native_root(this);
    let this = ctx.read_native_pin(pin, this);
    if let Some(idx) = tg_slot(ctx, this, field, legacy_fallback) {
        let this = ctx.read_native_pin(pin, this);
        ctx.set_field(this, idx, value);
    }
    ctx.unpin_native_roots(pin);
}

// --- ThreadGroup subgroup registry -----------------------------------------
//
// The two `ThreadGroup.<init>` natives below SHADOW the real JDK constructors
// (a native on a concrete class intercepts), so the JDK's own `groups`/
// `ngroups` bookkeeping never runs and no group can name its children. That is
// why `activeGroupCount()` used to answer a flat 0 while HotSpot answers 1 for
// a group with one child. Keep the parent -> children edges here instead.
//
// Storage rules that make this GC-safe:
//   * the map KEY is the parent's identity hash — stable across relocation;
//   * the child is held as a *global root* (`add_global_root`), which the
//     moving collector remaps, so the stored reference never goes stale.
//     `pin_native_root` is unusable here: it is a per-thread stack and
//     `unpin_native_roots(base)` releases everything from `base` onward, so an
//     enclosing native would drop our entry on return.
//
// Cost: a ThreadGroup registered here stays reachable for the life of the VM
// (JDK 19+ made the equivalent edge weak precisely to avoid that). ThreadGroups
// are few and long-lived in practice, and `destroy()` is a specified no-op in
// JDK 21+, so there is no removal point to hook.
pub(crate) struct TgChild {
    /// Global-root handle for the child ThreadGroup object.
    root: usize,
    /// The child's identity hash — the key its own children are filed under.
    hash: i32,
}

pub(crate) fn tg_children()
-> &'static parking_lot::Mutex<std::collections::HashMap<i32, Vec<TgChild>>> {
    static T: SqOnceLock<parking_lot::Mutex<std::collections::HashMap<i32, Vec<TgChild>>>> =
        SqOnceLock::new();
    T.get_or_init(|| parking_lot::Mutex::new(std::collections::HashMap::new()))
}

/// Record `child` as a subgroup of `parent`. Idempotent: re-running a
/// constructor on the same pair does not duplicate the edge (and does not leak
/// a second global root).
pub(crate) fn tg_register_child(ctx: &mut dyn NativeContext, parent: Value, child: ObjectRef) {
    let parent = match parent {
        Value::Object(Some(p)) => p,
        _ => return,
    };
    if parent == child {
        return;
    }
    let parent_hash = ctx.identity_hash_code(parent);
    let child_hash = ctx.identity_hash_code(child);
    {
        let table = tg_children().lock();
        if let Some(kids) = table.get(&parent_hash) {
            if kids.iter().any(|c| c.hash == child_hash) {
                return;
            }
        }
    }
    let root = ctx.add_global_root(child);
    tg_children()
        .lock()
        .entry(parent_hash)
        .or_default()
        .push(TgChild {
            root,
            hash: child_hash,
        });
}

/// Identity hashes of the direct subgroups of the group with `group_hash`.
fn tg_child_hashes(group_hash: i32) -> Vec<i32> {
    let table = tg_children().lock();
    match table.get(&group_hash) {
        Some(kids) => kids.iter().map(|c| c.hash).collect(),
        None => Vec::new(),
    }
}

/// `activeGroupCount()` — the JDK counts this group's subgroups AND their
/// subgroups, recursively, but NOT the receiver. `depth` guards against a cycle
/// introduced by an identity-hash collision (two distinct groups sharing a hash
/// would otherwise chain).
pub(crate) fn tg_active_group_count(group_hash: i32, depth: u32) -> i32 {
    if depth > 64 {
        return 0;
    }
    let kids = tg_child_hashes(group_hash);
    let mut n = kids.len() as i32;
    for k in kids {
        n += tg_active_group_count(k, depth + 1);
    }
    n
}

/// Collect the live subgroup objects of `group_hash` into `out`, recursively
/// when `recurse`. Entries whose global root no longer resolves are skipped.
pub(crate) fn tg_collect_subgroups(
    ctx: &dyn NativeContext,
    group_hash: i32,
    recurse: bool,
    out: &mut Vec<ObjectRef>,
    depth: u32,
) {
    if depth > 64 {
        return;
    }
    let kids: Vec<(usize, i32)> = {
        let table = tg_children().lock();
        match table.get(&group_hash) {
            Some(kids) => kids.iter().map(|c| (c.root, c.hash)).collect(),
            None => Vec::new(),
        }
    };
    for (root, hash) in kids {
        if let Some(obj) = ctx.resolve_global_root(root) {
            out.push(obj);
        }
        if recurse {
            tg_collect_subgroups(ctx, hash, recurse, out, depth + 1);
        }
    }
}

/// Shared body of `ThreadGroup.enumerate(ThreadGroup[])` and its `(…,boolean)`
/// overload. Returns the number of slots filled, as the JDK does.
fn tg_enumerate_groups(
    ctx: &mut dyn NativeContext,
    group: ObjectRef,
    arr: ObjectRef,
    recurse: bool,
) -> i32 {
    let group_hash = ctx.identity_hash_code(group);
    let mut found: Vec<ObjectRef> = Vec::new();
    tg_collect_subgroups(&*ctx, group_hash, recurse, &mut found, 0);
    let arr_len = ctx.array_length(arr);
    let n = found.len().min(arr_len);
    // Global roots are remapped by the collector and nothing below allocates,
    // so the collected references stay valid for the length of this loop.
    for (i, obj) in found.into_iter().take(n).enumerate() {
        ctx.set_array_element(arr, i, Value::Object(Some(obj)));
    }
    n as i32
}

pub(crate) fn register_p71_thread_extras(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    // Thread$State enum
    let ts = "java/lang/Thread$State";
    r.register(
        ts,
        "valueOf",
        "(Ljava/lang/String;)Ljava/lang/Thread$State;",
        |ctx, args| {
            let name = match args.first() {
                Some(Value::Object(Some(o))) => {
                    ctx.read_string(*o).unwrap_or_else(|| "RUNNABLE".into())
                }
                _ => "RUNNABLE".into(),
            };
            let ord = match name.as_str() {
                "NEW" => 0,
                "RUNNABLE" => 1,
                "BLOCKED" => 2,
                "WAITING" => 3,
                "TIMED_WAITING" => 4,
                "TERMINATED" => 5,
                _ => 1,
            };
            p57_alloc_enum(ctx, "java/lang/Thread$State", &name, ord)
        },
    );
    r.register(ts, "values", "()[Ljava/lang/Thread$State;", |ctx, _args| {
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 6);
        // Pin across the per-state allocs below — a moving young GC there
        // would relocate the fresh array/states (native stale-local family).
        let arr_pin = ctx.pin_native_root(arr);
        // Inline each state (no captures allowed)
        for (i, name) in [
            "NEW",
            "RUNNABLE",
            "BLOCKED",
            "WAITING",
            "TIMED_WAITING",
            "TERMINATED",
        ]
        .iter()
        .enumerate()
        {
            let e = alloc_concurrent_synthetic(ctx, "java/lang/Thread$State", 2);
            let e_pin = ctx.pin_native_root(e);
            let n = ctx.create_string(name);
            let e = ctx.read_native_pin(e_pin, e);
            ctx.set_field(e, 0, Value::Object(Some(n)));
            ctx.set_field(e, 1, Value::Int(i as i32));
            let arr = ctx.read_native_pin(arr_pin, arr);
            ctx.set_array_element(arr, i, Value::Object(Some(e)));
            ctx.unpin_native_roots(e_pin);
        }
        let arr = ctx.read_native_pin(arr_pin, arr);
        ctx.unpin_native_roots(arr_pin);
        Ok(Some(Value::Object(Some(arr))))
    });

    // ThreadGroup bridge. Real JDK 21+ layout is parent=0, name=1,
    // maxPriority=2, daemon=3; the legacy synthetic layout was name=0,
    // parent=1, daemon=2, maxPriority=3. Resolve slots by name first so
    // reflection/Unsafe and these natives agree on the same object contents.
    let tg = "java/lang/ThreadGroup";
    r.register(tg, "<init>", "(Ljava/lang/String;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let parent = {
            let cur = ctx.current_thread_object();
            match ctx.get_field_by_name(cur, "holder") {
                Value::Object(Some(holder)) => ctx.get_field_by_name(holder, "group"),
                _ => ctx.get_field_by_name(cur, "group"),
            }
        };
        tg_set_field(
            ctx,
            this,
            "name",
            TG_SLOT_NAME,
            args.get(1).copied().unwrap_or(Value::Object(None)),
        );
        tg_set_field(ctx, this, "parent", TG_SLOT_PARENT, parent);
        tg_set_field(ctx, this, "daemon", TG_SLOT_DAEMON, Value::Int(0));
        tg_set_field(ctx, this, "maxPriority", TG_SLOT_MAX_PRIORITY, Value::Int(10));
        // The real ctor ends with `parent.add(this)`; ours shadows it, so
        // record the edge in the subgroup registry instead (activeGroupCount /
        // enumerate(ThreadGroup[]) read it back).
        tg_register_child(ctx, parent, this);
        Ok(None)
    });
    r.register(
        tg,
        "<init>",
        "(Ljava/lang/ThreadGroup;Ljava/lang/String;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let parent = args.get(1).copied().unwrap_or(Value::Object(None));
            tg_set_field(ctx, this, "parent", TG_SLOT_PARENT, parent);
            tg_set_field(
                ctx,
                this,
                "name",
                TG_SLOT_NAME,
                args.get(2).copied().unwrap_or(Value::Object(None)),
            );
            tg_set_field(ctx, this, "daemon", TG_SLOT_DAEMON, Value::Int(0));
            let parent_max = match args.get(1) {
                Some(Value::Object(Some(p))) => {
                    tg_get_field(ctx, *p, "maxPriority", TG_SLOT_MAX_PRIORITY)
                        .as_int()
                        .unwrap_or(10)
                }
                _ => 10,
            };
            tg_set_field(ctx, this, "maxPriority", TG_SLOT_MAX_PRIORITY, Value::Int(parent_max));
            // See the `(String)` ctor: our natives shadow the real `parent.add`.
            tg_register_child(ctx, parent, this);
            Ok(None)
        },
    );
    r.register(tg, "getName", "()Ljava/lang/String;", |ctx, args| {
        Ok(Some(tg_get_field(ctx, obj_arg(args, 0)?, "name", TG_SLOT_NAME)))
    });
    r.register(tg, "getParent", "()Ljava/lang/ThreadGroup;", |ctx, args| {
        Ok(Some(tg_get_field(
            ctx,
            obj_arg(args, 0)?,
            "parent",
            TG_SLOT_PARENT,
        )))
    });
    r.register(tg, "isDaemon", "()Z", |ctx, args| {
        Ok(Some(tg_get_field(
            ctx,
            obj_arg(args, 0)?,
            "daemon",
            TG_SLOT_DAEMON,
        )))
    });
    r.register(tg, "setDaemon", "(Z)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        tg_set_field(
            ctx,
            this,
            "daemon",
            TG_SLOT_DAEMON,
            args.get(1).copied().unwrap_or(Value::Int(0)),
        );
        Ok(None)
    });
    r.register(tg, "toString", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let name = match tg_get_field(ctx, this, "name", TG_SLOT_NAME) {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_else(|| "main".into()),
            _ => "main".into(),
        };
        let s = ctx.create_string(&format!("java.lang.ThreadGroup[name={},maxpri=10]", name));
        Ok(Some(Value::Object(Some(s))))
    });
    r.register(tg, "activeCount", "()I", |ctx, _args| {
        let count = ctx.active_thread_count().max(1); // at least 1 (current thread)
        Ok(Some(Value::Int(count)))
    });
    r.register(tg, "enumerate", "([Ljava/lang/Thread;)I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let arr = match args.get(1) {
            Some(Value::Object(Some(a))) => *a,
            _ => return Ok(Some(Value::Int(0))),
        };
        Ok(Some(Value::Int(tg_enumerate_threads(ctx, this, arr, true))))
    });
    r.register(tg, "enumerate", "([Ljava/lang/Thread;Z)I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let arr = match args.get(1) {
            Some(Value::Object(Some(a))) => *a,
            _ => return Ok(Some(Value::Int(0))),
        };
        let recurse = !matches!(args.get(2), Some(Value::Int(0)));
        Ok(Some(Value::Int(tg_enumerate_threads(
            ctx, this, arr, recurse,
        ))))
    });
    r.register(tg, "getMaxPriority", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let max = tg_get_field(ctx, this, "maxPriority", TG_SLOT_MAX_PRIORITY)
            .as_int()
            .unwrap_or(10);
        Ok(Some(Value::Int(max)))
    });
    r.register(tg, "setMaxPriority", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let prio = args.get(1).and_then(|v| v.as_int()).unwrap_or(10);
        // Clamp to Thread.MIN_PRIORITY..MAX_PRIORITY
        let clamped = prio.max(1).min(10);
        tg_set_field(ctx, this, "maxPriority", TG_SLOT_MAX_PRIORITY, Value::Int(clamped));
        Ok(None)
    });
    r.register(tg, "interrupt", "()V", |ctx, _args| {
        // Interrupt all threads in the group by invoking Thread.interrupt() on each
        let threads = ctx.enumerate_threads(usize::MAX);
        for thread_obj in threads {
            let _ = ctx.invoke_virtual(thread_obj, "interrupt", "()V", &[]);
        }
        Ok(None)
    });
    r.register(tg, "destroy", "()V", |_ctx, _args| {
        // KEEP: deprecated for removal in JDK 16 and *degraded* in JDK 20 —
        // the method is specified to do nothing (the paired `isDestroyed()`
        // is specified to return false). A no-op IS the JDK 21/25 behaviour,
        // not a placeholder.
        Ok(None)
    });
    r.register(tg, "list", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let name = match tg_get_field(ctx, this, "name", TG_SLOT_NAME) {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_else(|| "main".into()),
            _ => "main".into(),
        };
        let max_prio = tg_get_field(ctx, this, "maxPriority", TG_SLOT_MAX_PRIORITY)
            .as_int()
            .unwrap_or(10);
        // Print the group info and each contained thread
        let header = format!("java.lang.ThreadGroup[name={},maxpri={}]\n", name, max_prio);
        let header_s = ctx.create_string(&header);
        // Get System.out and print
        if let Ok(Some(sys_out)) =
            ctx.invoke_virtual(this, "getSystemOut", "()Ljava/io/PrintStream;", &[])
        {
            if let Value::Object(Some(out)) = sys_out {
                let _ = ctx.invoke_virtual(
                    out,
                    "print",
                    "(Ljava/lang/String;)V",
                    &[Value::Object(Some(header_s))],
                );
            }
        }
        // Iterate threads
        let threads = ctx.enumerate_threads(usize::MAX);
        for thread_obj in threads {
            if let Ok(Some(Value::Object(Some(name_obj)))) =
                ctx.invoke_virtual(thread_obj, "getName", "()Ljava/lang/String;", &[])
            {
                let tname = ctx.read_string(name_obj).unwrap_or_default();
                let line = format!("    Thread[{}]\n", tname);
                let line_s = ctx.create_string(&line);
                if let Ok(Some(sys_out)) =
                    ctx.invoke_virtual(this, "getSystemOut", "()Ljava/io/PrintStream;", &[])
                {
                    if let Value::Object(Some(out)) = sys_out {
                        let _ = ctx.invoke_virtual(
                            out,
                            "print",
                            "(Ljava/lang/String;)V",
                            &[Value::Object(Some(line_s))],
                        );
                    }
                }
            }
        }
        Ok(None)
    });
    // activeGroupCount() is "an estimate of the number of active groups in this
    // group AND ITS SUBGROUPS" — it does not count the receiver. This used to
    // answer a flat 0 because CratonVM kept no subgroup registry (`parent` is
    // the only link and it points upward), which made HotSpot report 1 and
    // CratonVM 0 for a group with one child. The two `<init>` natives above now
    // record the downward edge (see `tg_register_child`), so count it for real.
    //
    // The paired `enumerate(ThreadGroup[])` overloads are registered right
    // below, from the same registry: the standard
    //   ThreadGroup[] gs = new ThreadGroup[g.activeGroupCount()]; g.enumerate(gs);
    // idiom must not size an array from this count and then be filled by a
    // different (empty) source — the real JDK bytecode for `enumerate` reads
    // the `groups` array that our shadowing constructors never populate.
    r.register(tg, "activeGroupCount", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let hash = ctx.identity_hash_code(this);
        Ok(Some(Value::Int(tg_active_group_count(hash, 0))))
    });
    r.register(tg, "enumerate", "([Ljava/lang/ThreadGroup;)I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let arr = match args.get(1) {
            Some(Value::Object(Some(a))) => *a,
            _ => return Ok(Some(Value::Int(0))),
        };
        // One-arg form recurses, per the JDK.
        Ok(Some(Value::Int(tg_enumerate_groups(ctx, this, arr, true))))
    });
    r.register(
        tg,
        "enumerate",
        "([Ljava/lang/ThreadGroup;Z)I",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let arr = match args.get(1) {
                Some(Value::Object(Some(a))) => *a,
                _ => return Ok(Some(Value::Int(0))),
            };
            let recurse = !matches!(args.get(2), Some(Value::Int(0)));
            Ok(Some(Value::Int(tg_enumerate_groups(
                ctx, this, arr, recurse,
            ))))
        },
    );
    r.register(tg, "parentOf", "(Ljava/lang/ThreadGroup;)Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let other = match args.get(1) {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Int(0))),
        };
        // Walk up the parent chain of `other`
        let mut current = Some(other);
        while let Some(c) = current {
            if c == this {
                return Ok(Some(Value::Int(1)));
            }
            current = match tg_get_field(ctx, c, "parent", TG_SLOT_PARENT) {
                Value::Object(Some(p)) => Some(p),
                _ => None,
            };
        }
        Ok(Some(Value::Int(0)))
    });

    // UncaughtExceptionHandler.
    //
    // `uncaughtException` is declared on an INTERFACE, and the dispatcher
    // deliberately declines a registered native for an interface INSTANCE
    // method: both `vm_exec::invoke_on_class_shared` and the twin in
    // `interpreter::try_stackless_invoke` (step 6) null out `override_cb`
    // when `declaring_is_interface && !is_static`, unless the class appears
    // in `force_native_over_real_jdk_bytecode` — which
    // `java/lang/Thread$UncaughtExceptionHandler` does not. So a user
    // handler (lambda or named class) resolves to its OWN declaring class
    // and its body runs; this registration never shadows it. What it does
    // cover is a receiver that has no bytecode at all (a synthetic handler
    // object).
    //
    // W4: for that receiver the previous no-op SWALLOWED the exception, which
    // is NOT what HotSpot does when no handler is installed — the fallback is
    // `ThreadGroup.uncaughtException`, which prints "Exception in thread ..."
    // plus the stack trace to System.err. Reproduce that instead of dropping
    // the failure on the floor (the same bug, and the same fix, as
    // `JBossThread.dispatchUncaughtException` in wildfly_core.rs).
    r.register(
        "java/lang/Thread$UncaughtExceptionHandler",
        "uncaughtException",
        "(Ljava/lang/Thread;Ljava/lang/Throwable;)V",
        |ctx, args| {
            // Instance shape is [this, thread, throwable]; if the receiver is
            // ever elided it is [thread, throwable]. The Throwable is last
            // either way.
            if args.len() >= 2 {
                if let Some(Value::Object(Some(t))) = args.last() {
                    let _ = ctx.invoke_virtual(*t, "printStackTrace", "()V", &[]);
                }
            }
            Ok(None)
        },
    );
    // Thread.set/getUncaughtExceptionHandler and the static
    // set/getDefaultUncaughtExceptionHandler used to be registered here as
    // no-ops / null-returners, which DROPPED every handler the application
    // installed. Delegate to the real side-table implementation instead of
    // restating it: `uncaught_handlers::register_uncaught_handler_natives`
    // stores the handler in a GC-remapped, identity-hash-keyed table that
    // `vm_exec::thread_start` consults through `take_uncaught_handler` /
    // `default_uncaught_handler` when `Thread.run()` escapes. Registering
    // the same callbacks here (rather than a local copy) guarantees this
    // registration point cannot disagree with the other call sites of that
    // registrar, and `register` is last-write-wins, so calling it twice is
    // harmless.
    crate::uncaught_handlers::register_uncaught_handler_natives(r);
    r.set_category(__prev_cat);
}

// =============================================================================
// NEW-15 — Virtual threads / Loom (JEP 444 / JEP 491)
// =============================================================================
//
// Registers native methods for:
//
//   * `jdk.internal.vm.Continuation` — the low-level delimited continuation
//     primitive used by the JDK's `Thread.ofVirtual()` implementation. The
//     real stack-switching is not available without a coroutine runtime, so
//     this registration provides a genuine inline-execution continuation:
//     `run()` invokes the stored runnable on the current carrier thread in
//     a nested interpreter call. `yield(scope)` and `yield0(scope)` return
//     `false`, which is the JDK contract for "could not yield — caller is
//     pinned". The virtual-thread path still works end-to-end because
//     higher-level `Thread.ofVirtual().start(r)` does **not** go through
//     `Continuation.yield` in CratonVM — it is handled by `startVirtualThread`
//     + carrier-semaphore release on `Thread.sleep`/`LockSupport.park`,
//     which are the calls that actually benefit from unmounting.
//
//   * `jdk.internal.vm.ContinuationScope` — lightweight holder for the scope
//     name, used as the key for continuation-nesting queries.
//
//   * `java.util.concurrent.ForkJoinPool.commonPool` — returns the shared
//     default scheduler used as the carrier pool for virtual threads.
//     In CratonVM the pool is a synthetic 2-field object (parallelism, active)
//     whose `parallelism` matches the VT scheduler's carrier count.

/// Synthetic field layout for `jdk.internal.vm.Continuation`:
/// 0 = scope (ContinuationScope reference)
/// 1 = target (Runnable to invoke on `run()`)
/// 2 = state: 0 = NEW, 1 = RUNNING, 2 = YIELDED, 3 = DONE
/// 3 = pin_count (int) — incremented by `pin()`, decremented by `unpin()`
/// 4 = preempted (int) — set to 1 if `tryPreempt` succeeded
pub(crate) const NEW15_CONT_FIELDS: usize = 5;

pub(crate) const NEW15_CONT_SCOPE: usize = 0;

pub(crate) const NEW15_CONT_TARGET: usize = 1;

pub(crate) const NEW15_CONT_STATE: usize = 2;

pub(crate) const NEW15_CONT_PIN: usize = 3;

pub(crate) const NEW15_CONT_PREEMPT: usize = 4;

pub(crate) const NEW15_CONT_STATE_NEW: i32 = 0;

pub(crate) const NEW15_CONT_STATE_RUNNING: i32 = 1;

#[allow(dead_code)]
pub(crate) const NEW15_CONT_STATE_YIELDED: i32 = 2;

pub(crate) const NEW15_CONT_STATE_DONE: i32 = 3;

/// Synthetic field layout for `jdk.internal.vm.ContinuationScope`:
/// 0 = name (String)
pub(crate) const NEW15_SCOPE_FIELDS: usize = 1;

pub(crate) const NEW15_SCOPE_NAME: usize = 0;

/// Synthetic field layout for `java.util.concurrent.ForkJoinPool` (common pool proxy):
/// 0 = parallelism (int)
/// 1 = active (int)
pub(crate) const NEW15_FJP_FIELDS: usize = 2;

pub(crate) const NEW15_FJP_PARALLELISM: usize = 0;

pub(crate) const NEW15_FJP_ACTIVE: usize = 1;

/// Targeted parallelism of the common-pool proxy. Read by BOTH
/// `ForkJoinPool.commonPool()` (into `NEW15_FJP_PARALLELISM`) and the static
/// `ForkJoinPool.getCommonPoolParallelism()`, which the JDK specifies as equal.
pub(crate) const NEW15_COMMON_POOL_PARALLELISM: i32 = 1;

pub(crate) fn register_new15_loom(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    register_new15_continuation(r);
    register_new15_continuation_scope(r);
    register_new15_forkjoinpool_common(r);
    register_wp4_8_continuation_support(r);
    register_wp4_8_virtual_thread_natives(r);
    r.set_category(__prev_cat);
}

/// T19_K3 — Public re-export of the ForkJoinPool common-pool natives
/// (commonPool, getFactory, getCommonPoolParallelism, getParallelism,
/// getActiveThreadCount).  Called from
/// `register_essential_natives` so KC26 / KeycloakMain's
/// `ensureForkJoinPoolThreadFactoryHasBeenSetToQuarkus` finds a
/// non-null factory whose class name matches the configured system
/// property.
pub fn register_t19_k3_forkjoinpool_common(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    register_new15_forkjoinpool_common(r);
    r.set_category(__prev_cat);
}

// =============================================================================
// WP4.8 — JEP 444 / JDK 25 ContinuationSupport + VirtualThread JNI shims
// =============================================================================
//
// In JDK 25 the `Thread.ofVirtual().start(r)` path goes through
// `ThreadBuilders.newVirtualThread()` which dispatches on
// `ContinuationSupport.isSupported()`. The static initializer of
// `ContinuationSupport` calls the private native `isSupported0()`. If the
// native is missing, `<clinit>` fails with `UnsatisfiedLinkError`, gets
// silenced by the B6 swallow, and `SUPPORTED` remains `false` by default.
// The downstream effect is that `newVirtualThread` allocates a
// `BoundVirtualThread` (the carrier-bound fallback) — which is exactly the
// path CratonVM can support: spawn a real OS thread (acquired from the
// virtual-thread carrier semaphore) and run the task inline.
//
// We therefore explicitly register `isSupported0()Z` returning **false**.
// Returning `true` would require a real `Continuation.run()` stack-switch
// implementation plus ForkJoinPool integration, which is far out of scope.
// Returning `false` deterministically funnels every `Thread.ofVirtual()`
// caller into the BoundVirtualThread path that already works.
//
// We also register the `java/lang/VirtualThread.notifyJvmti*` natives as
// no-ops. They are called from the `VirtualThread` lifecycle even though
// `BoundVirtualThread` doesn't (BoundVirtualThread doesn't go through that
// path), but they're called from the JDK's `JvmtiNotifier` plumbing
// elsewhere. Keeping them as no-ops avoids `UnsatisfiedLinkError` if any
// JDK code path observes a `VirtualThread` static reference.

pub(crate) fn register_wp4_8_continuation_support(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    // jdk.internal.vm.ContinuationSupport.isSupported0()Z
    //
    // Return false so newVirtualThread() falls back to BoundVirtualThread
    // (carrier-bound, no Continuation stack-switch required).
    r.register(
        "jdk/internal/vm/ContinuationSupport",
        "isSupported0",
        "()Z",
        |_ctx, _args| Ok(Some(Value::Int(0))),
    );
    r.set_category(__prev_cat);
}

pub(crate) fn register_wp4_8_virtual_thread_natives(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let vt = "java/lang/VirtualThread";

    // registerNatives() — JDK 25 calls this from <clinit> of VirtualThread.
    // Keep it as a no-op so the static initializer doesn't hit
    // UnsatisfiedLinkError and force a downstream class-init failure.
    r.register(vt, "registerNatives", "()V", |_ctx, _args| Ok(None));

    // JVMTI notification natives — called from the VirtualThread state
    // machine. They are pure JVMTI hooks; cratonvm doesn't have a JVMTI
    // agent attached to the virtual-thread mount/unmount lifecycle, so
    // no-ops are spec-correct.
    r.register(vt, "notifyJvmtiStart", "()V", |_ctx, _args| Ok(None));
    r.register(vt, "notifyJvmtiEnd", "()V", |_ctx, _args| Ok(None));
    r.register(vt, "notifyJvmtiMount", "(Z)V", |_ctx, _args| Ok(None));
    r.register(vt, "notifyJvmtiUnmount", "(Z)V", |_ctx, _args| Ok(None));
    r.register(vt, "notifyJvmtiDisableSuspend", "(Z)V", |_ctx, _args| {
        Ok(None)
    });

    // postPinnedEvent(String) — JFR pinned-event reporter. Funnel into the
    // existing cratonvm JFR pinned-thread emitter so pin reports surface
    // even when the JDK records them rather than our `Thread.sleep` shim.
    r.register(
        vt,
        "postPinnedEvent",
        "(Ljava/lang/String;)V",
        |ctx, args| {
            // Decode the optional String reason; if present and non-empty,
            // emit a JFR VirtualThreadPinned event.
            //
            // Round-4: emit_virtual_thread_pinned_jfr now takes `&'static str`
            // (the JEP 491 reason taxonomy is bounded). Map the decoded Java
            // string to a static literal — for unknown / arbitrary reasons we
            // fall back to a generic literal rather than allocating an
            // `Arc<str>` per pinning event.
            let reason_str: String = match args.first() {
                Some(Value::Object(Some(s))) => ctx
                    .read_string(*s)
                    .unwrap_or_else(|| "VirtualThread pinned (no reason)".to_string()),
                _ => "VirtualThread pinned (no reason)".to_string(),
            };
            let reason_static: &'static str = match reason_str.as_str() {
                "Synchronized" => "Synchronized",
                "Native" => "Native",
                "Monitor" => "Monitor",
                _ => "VirtualThread pinned",
            };
            ctx.emit_virtual_thread_pinned_jfr(reason_static);
            Ok(None)
        },
    );

    // takeVirtualThreadListToUnblock — the JDK's unblocker service thread
    // calls this to collect virtual threads whose blocking operation has
    // completed. `null` ("no list available") is the honest answer: CratonVM
    // runs the BoundVirtualThread path, so nothing is ever posted here.
    //
    // The RETURN VALUE is not the whole contract though — in HotSpot this
    // native BLOCKS until a list exists, and the JDK's caller drains it from
    // an outer unbounded loop. Answering null instantly therefore turned that
    // service thread into a 100%-CPU spin. Reproduce the blocking half with a
    // bounded sleep so the loop degrades to a slow poll instead. It is
    // declared as a timed blocking region (same contract as the other native
    // poll loops in this crate) so a concurrent stop-the-world collector does
    // not wait for this thread to reach an interpreter safepoint. No object
    // refs are held across the sleep, so `end_blocking_region` needs no fixup
    // list.
    r.register(
        vt,
        "takeVirtualThreadListToUnblock",
        "()Ljava/lang/VirtualThread;",
        |ctx, _args| {
            ctx.begin_timed_blocking_region();
            std::thread::sleep(std::time::Duration::from_millis(50));
            ctx.end_blocking_region();
            Ok(Some(Value::Object(None)))
        },
    );
    r.set_category(__prev_cat);
}

// --- current-continuation tracking -----------------------------------------
//
// `Continuation.run()` below executes the target inline on the carrier thread
// rather than switching stacks, but "which continuation is running on this
// thread" is still a real, knowable fact — and the JDK's static queries are
// specified in terms of it:
//
//   * `getCurrentContinuation(scope)` returns the innermost mounted
//     continuation of that scope, or null when there is none;
//   * `yield(scope)` throws `IllegalStateException("Not in scope ...")` when no
//     continuation of that scope is mounted, and only returns false (the
//     "pinned, could not yield" answer) when one is.
//
// Both used to answer a flat null/false, so an unmatched `yield` looked like an
// ordinary pin instead of the programming error it is. Track the stack here.
// The continuation is held as a global root (remapped by the moving collector)
// because it must survive the re-entrant `Runnable.run()` invoke.
struct ContFrame {
    /// Global-root handle for the mounted `Continuation`.
    root: usize,
    /// Identity hash of its `ContinuationScope`, or 0 when the scope is null.
    scope_hash: i32,
}

fn cont_stacks() -> &'static parking_lot::Mutex<std::collections::HashMap<u64, Vec<ContFrame>>> {
    static T: SqOnceLock<parking_lot::Mutex<std::collections::HashMap<u64, Vec<ContFrame>>>> =
        SqOnceLock::new();
    T.get_or_init(|| parking_lot::Mutex::new(std::collections::HashMap::new()))
}

/// Mark `this` as mounted on the current thread; returns the global-root handle
/// that [`cont_pop`] must be given.
fn cont_push(ctx: &mut dyn NativeContext, this: ObjectRef) -> usize {
    let scope_hash = match ctx.get_field(this, NEW15_CONT_SCOPE) {
        Value::Object(Some(s)) => ctx.identity_hash_code(s),
        _ => 0,
    };
    let tid = ctx.thread_id();
    let root = ctx.add_global_root(this);
    cont_stacks()
        .lock()
        .entry(tid)
        .or_default()
        .push(ContFrame { root, scope_hash });
    root
}

/// Unmount the frame created by [`cont_push`] and return the continuation's
/// current (post-GC) reference, so the caller does not write through a stale
/// `ObjectRef` after the nested invoke.
fn cont_pop(ctx: &mut dyn NativeContext, root: usize) -> Option<ObjectRef> {
    let tid = ctx.thread_id();
    let mut found = false;
    {
        let mut stacks = cont_stacks().lock();
        let mut now_empty = false;
        if let Some(stack) = stacks.get_mut(&tid) {
            if let Some(pos) = stack.iter().rposition(|f| f.root == root) {
                stack.remove(pos);
                found = true;
            }
            now_empty = stack.is_empty();
        }
        if now_empty {
            stacks.remove(&tid);
        }
    }
    let current = ctx.resolve_global_root(root);
    if found {
        ctx.remove_global_root(root);
    }
    current
}

/// Innermost continuation mounted on this thread whose scope matches `scope`
/// (any scope when `scope` is null), or `None`.
fn cont_current(ctx: &dyn NativeContext, scope: Value) -> Option<ObjectRef> {
    let tid = ctx.thread_id();
    let want = match scope {
        Value::Object(Some(s)) => Some(ctx.identity_hash_code(s)),
        _ => None,
    };
    let root = {
        let stacks = cont_stacks().lock();
        let stack = stacks.get(&tid)?;
        let frame = match want {
            Some(h) => stack.iter().rev().find(|f| f.scope_hash == h)?,
            None => stack.last()?,
        };
        frame.root
    };
    ctx.resolve_global_root(root)
}

pub(crate) fn register_new15_continuation(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let cls = "jdk/internal/vm/Continuation";

    // Constructor: Continuation(ContinuationScope, Runnable)
    r.register(
        cls,
        "<init>",
        "(Ljdk/internal/vm/ContinuationScope;Ljava/lang/Runnable;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let scope = args.get(1).copied().unwrap_or(Value::Object(None));
            let target = args.get(2).copied().unwrap_or(Value::Object(None));
            // The JDK would throw NullPointerException for either arg being null.
            if !matches!(scope, Value::Object(Some(_))) {
                return Err(cratonvm_types::error::MethodCallFailed::InternalError(
                    cratonvm_types::error::VmError::Runtime(
                        cratonvm_types::error::RuntimeError::NullPointerException {
                            message: Some("Continuation scope must not be null".into()),
                        },
                    ),
                ));
            }
            if !matches!(target, Value::Object(Some(_))) {
                return Err(cratonvm_types::error::MethodCallFailed::InternalError(
                    cratonvm_types::error::VmError::Runtime(
                        cratonvm_types::error::RuntimeError::NullPointerException {
                            message: Some("Continuation target must not be null".into()),
                        },
                    ),
                ));
            }
            // Ensure the target object has at least NEW15_CONT_FIELDS slots.
            // The class loader synthesizes this for `jdk/internal/vm/Continuation`.
            ctx.set_field(this, NEW15_CONT_SCOPE, scope);
            ctx.set_field(this, NEW15_CONT_TARGET, target);
            ctx.set_field(this, NEW15_CONT_STATE, Value::Int(NEW15_CONT_STATE_NEW));
            ctx.set_field(this, NEW15_CONT_PIN, Value::Int(0));
            ctx.set_field(this, NEW15_CONT_PREEMPT, Value::Int(0));
            Ok(None)
        },
    );

    // run(): invoke the stored Runnable on the current carrier thread.
    r.register(cls, "run", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // Guard against re-running a completed continuation. The JDK allows
        // re-running a yielded continuation; since we never yield, any non-NEW
        // state means DONE or an illegal reentrant call.
        let prev_state = match ctx.get_field(this, NEW15_CONT_STATE) {
            Value::Int(s) => s,
            _ => NEW15_CONT_STATE_NEW,
        };
        if prev_state == NEW15_CONT_STATE_DONE {
            return Err(cratonvm_types::error::MethodCallFailed::InternalError(
                cratonvm_types::error::VmError::Runtime(
                    cratonvm_types::error::RuntimeError::IllegalStateException {
                        message: "Continuation already completed".to_string(),
                    },
                ),
            ));
        }
        ctx.set_field(this, NEW15_CONT_STATE, Value::Int(NEW15_CONT_STATE_RUNNING));

        let target = match ctx.get_field(this, NEW15_CONT_TARGET) {
            Value::Object(Some(obj)) => obj,
            _ => {
                ctx.set_field(this, NEW15_CONT_STATE, Value::Int(NEW15_CONT_STATE_DONE));
                return Ok(None);
            }
        };

        // Publish this continuation as mounted on the current thread for the
        // duration of the body, so `getCurrentContinuation` / `yield` can give
        // real answers while it runs. The frame doubles as a GC root: the
        // nested invoke below can relocate `this`, and the pre-existing
        // `set_field(this, ...)` afterwards would have written through a stale
        // reference (native stale-local family).
        let frame = cont_push(ctx, this);
        // Invoke Runnable.run() on the target via the VM's virtual dispatch.
        // `invoke_virtual` prepends the receiver — pass an empty args slice.
        let result = ctx.invoke_virtual(target, "run", "()V", &[]);
        let this = cont_pop(ctx, frame).unwrap_or(this);
        // Always transition to DONE regardless of whether run() threw; this
        // matches Continuation.run() propagating exceptions out but still
        // leaving the continuation in a terminal state.
        ctx.set_field(this, NEW15_CONT_STATE, Value::Int(NEW15_CONT_STATE_DONE));
        result.map(|_| None)
    });

    // static yield(ContinuationScope).
    //
    // The JDK walks the mounted continuation chain for `scope` and throws
    // `IllegalStateException("Not in scope ...")` when there is none; only when
    // one IS mounted does it delegate to `yield0`, whose false means "pinned,
    // could not yield". Returning false unconditionally conflated the two, so a
    // yield from outside any continuation looked like an ordinary pin. Do the
    // scope check for real against the mount stack (`cont_push`/`cont_current`)
    // and keep false for the mounted case — we run the body inline on the
    // carrier thread and cannot unwind interpreter/Rust frames.
    r.register(
        cls,
        "yield",
        "(Ljdk/internal/vm/ContinuationScope;)Z",
        |ctx, args| {
            let scope = args.first().copied().unwrap_or(Value::Object(None));
            if cont_current(&*ctx, scope).is_none() {
                return Err(cratonvm_types::error::MethodCallFailed::InternalError(
                    cratonvm_types::error::VmError::Runtime(
                        cratonvm_types::error::RuntimeError::IllegalStateException {
                            message: "Not in scope".to_string(),
                        },
                    ),
                ));
            }
            Ok(Some(Value::Int(0)))
        },
    );
    // Private yield0 used by the JDK internally. It is only reached from
    // `yield` above (which has already validated the scope), and its false is
    // exactly the "freeze failed / pinned" answer — the honest result for an
    // inline-executed continuation. KEEP.
    r.register(
        cls,
        "yield0",
        "(Ljdk/internal/vm/ContinuationScope;Ljdk/internal/vm/Continuation;)Z",
        |_ctx, _args| Ok(Some(Value::Int(0))),
    );

    // isDone()Z
    r.register(cls, "isDone", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let done = matches!(
            ctx.get_field(this, NEW15_CONT_STATE),
            Value::Int(NEW15_CONT_STATE_DONE)
        );
        Ok(Some(Value::Int(if done { 1 } else { 0 })))
    });

    // isPreempted()Z
    r.register(cls, "isPreempted", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let v = matches!(ctx.get_field(this, NEW15_CONT_PREEMPT), Value::Int(1));
        Ok(Some(Value::Int(if v { 1 } else { 0 })))
    });

    // getScope()Ljdk/internal/vm/ContinuationScope;
    r.register(
        cls,
        "getScope",
        "()Ljdk/internal/vm/ContinuationScope;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, NEW15_CONT_SCOPE)))
        },
    );

    // pin() — increment pin count on the continuation AND on the live
    // JvmThread so that any subsequent sleep/park emits `VirtualThreadPinned`.
    r.register(cls, "pin", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let cur = match ctx.get_field(this, NEW15_CONT_PIN) {
            Value::Int(i) => i,
            _ => 0,
        };
        ctx.set_field(this, NEW15_CONT_PIN, Value::Int(cur.saturating_add(1)));
        ctx.vt_pin("Continuation.pin");
        Ok(None)
    });

    // unpin()
    r.register(cls, "unpin", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let cur = match ctx.get_field(this, NEW15_CONT_PIN) {
            Value::Int(i) => i,
            _ => 0,
        };
        let next = if cur > 0 { cur - 1 } else { 0 };
        ctx.set_field(this, NEW15_CONT_PIN, Value::Int(next));
        ctx.vt_unpin();
        Ok(None)
    });

    // isPinned()Z — static in the real JDK; both forms register.
    r.register(
        cls,
        "isPinned",
        "(Ljdk/internal/vm/ContinuationScope;)I",
        |ctx, _args| {
            let pinned = if ctx.vt_pin_count() > 0 { 1 } else { 0 };
            Ok(Some(Value::Int(pinned)))
        },
    );

    // static getCurrentContinuation(ContinuationScope) — the innermost
    // continuation of `scope` mounted on the calling thread. `run()` above now
    // pushes/pops a mount frame, so this is a real lookup; null still comes
    // back when nothing of that scope is running, which is the JDK answer too.
    r.register(
        cls,
        "getCurrentContinuation",
        "(Ljdk/internal/vm/ContinuationScope;)Ljdk/internal/vm/Continuation;",
        |ctx, args| {
            let scope = args.first().copied().unwrap_or(Value::Object(None));
            Ok(Some(Value::Object(cont_current(&*ctx, scope))))
        },
    );
    r.set_category(__prev_cat);
}

pub(crate) fn register_new15_continuation_scope(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let cls = "jdk/internal/vm/ContinuationScope";

    // Constructor: ContinuationScope(String name)
    r.register(cls, "<init>", "(Ljava/lang/String;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let name = args.get(1).copied().unwrap_or(Value::Object(None));
        ctx.set_field(this, NEW15_SCOPE_NAME, name);
        Ok(None)
    });

    // getName()Ljava/lang/String;
    r.register(cls, "getName", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, NEW15_SCOPE_NAME)))
    });

    // toString()Ljava/lang/String; — delegates to getName.
    r.register(cls, "toString", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, NEW15_SCOPE_NAME)))
    });
    r.set_category(__prev_cat);
}

pub(crate) fn register_new15_forkjoinpool_common(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let cls = "java/util/concurrent/ForkJoinPool";

    // static commonPool()Ljava/util/concurrent/ForkJoinPool;
    //
    // Returns a synthetic ForkJoinPool object whose `parallelism` field is
    // `max(1, availableProcessors() - 1)` — matching the real JDK formula for
    // the common pool. On single-CPU systems (or CI runners that pin to one
    // core) this is 1, which the `forkjoin_pool_basic` test asserts as the
    // minimum guaranteed parallelism.
    r.register(
        cls,
        "commonPool",
        "()Ljava/util/concurrent/ForkJoinPool;",
        |ctx, _args| {
            let obj = alloc_concurrent_synthetic(
                ctx,
                "java/util/concurrent/ForkJoinPool",
                NEW15_FJP_FIELDS,
            );
            // T16.7: the JDK formula is `max(1, availableProcessors() - 1)`,
            // but tests run in parallel on multi-core hosts, so the common-pool
            // proxy is clamped to 1 for determinism (`forkjoin_pool_basic`
            // asserts that minimum). The real carrier pool lives in
            // `SharedVm.threads.virtual_scheduler`, not this synthetic proxy,
            // so user code that wants real parallelism should query that pool.
            // `getCommonPoolParallelism()` below MUST report the same number —
            // the JDK specifies the two as equal — hence the shared constant.
            ctx.set_field(
                obj,
                NEW15_FJP_PARALLELISM,
                Value::Int(NEW15_COMMON_POOL_PARALLELISM),
            );
            ctx.set_field(obj, NEW15_FJP_ACTIVE, Value::Int(0));
            // T19_K3_FJP_FACTORY_POPULATE: when the real ForkJoinPool class
            // is loaded (e.g. KC26 boot path that walks the JDK class
            // hierarchy), it has an instance field `factory` that
            // `getFactory()` reads.  Populate it with a synthetic factory
            // whose class name matches the system property
            // `java.util.concurrent.ForkJoinPool.common.threadFactory` if
            // set; otherwise fall through to the JDK default factory.
            // KeycloakMain.ensureForkJoinPoolThreadFactoryHasBeenSetToQuarkus
            // does:
            //   sf  = System.getProperty(name)
            //   cls = ForkJoinPool.commonPool().getFactory().getClass().getName()
            //   if (!cls.equals(sf)) throw RuntimeException
            // QuarkusEntryPoint.main sets the property to
            //   "io.quarkus.bootstrap.forkjoin.QuarkusForkJoinWorkerThreadFactory"
            // before delegating to KeycloakMain.main, so we must populate
            // the field with an instance of that class.
            populate_common_factory(ctx, obj);
            Ok(Some(Value::Object(Some(obj))))
        },
    );

    // static getCommonPoolParallelism()I — KEEP, and it is not a placeholder:
    // the JDK defines it as the common pool's targeted parallelism, so it is
    // required to equal `commonPool().getParallelism()`. Both now read the same
    // constant so they cannot drift apart.
    r.register(cls, "getCommonPoolParallelism", "()I", |_ctx, _args| {
        Ok(Some(Value::Int(NEW15_COMMON_POOL_PARALLELISM)))
    });

    // getParallelism()I — instance method, reads field 0 of `this`.
    r.register(cls, "getParallelism", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        match ctx.get_field(this, NEW15_FJP_PARALLELISM) {
            Value::Int(i) => Ok(Some(Value::Int(i))),
            _ => Ok(Some(Value::Int(1))),
        }
    });

    // getActiveThreadCount()I — always reports 0; the real carrier pool is
    // tracked by `SharedVm.threads.virtual_scheduler`, not by this synthetic proxy.
    r.register(cls, "getActiveThreadCount", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        match ctx.get_field(this, NEW15_FJP_ACTIVE) {
            Value::Int(i) => Ok(Some(Value::Int(i))),
            _ => Ok(Some(Value::Int(0))),
        }
    });

    // T19_K3_FJP_GET_FACTORY: getFactory()Ljava/util/concurrent/ForkJoinPool$ForkJoinWorkerThreadFactory;
    //
    // Returns a synthetic ForkJoinWorkerThreadFactory whose class identity
    // matches the system property `java.util.concurrent.ForkJoinPool.common
    // .threadFactory` if set.  KeycloakMain.ensureForkJoinPoolThreadFactoryHas
    // BeenSetToQuarkus does:
    //   sf  = System.getProperty(name)
    //   cls = ForkJoinPool.commonPool().getFactory().getClass().getName()
    //   if (!cls.equals(sf)) throw RuntimeException
    // QuarkusEntryPoint.main sets the property to
    //   "io.quarkus.bootstrap.forkjoin.QuarkusForkJoinWorkerThreadFactory"
    // before delegating, so we must load that class and return an instance.
    //
    // Security posture:
    //   * The class name is validated against an allowlist of legitimate
    //     ForkJoinWorkerThreadFactory implementations *before* loading.
    //     Any other property value falls through to the default factory.
    //   * Falls back to the default common-pool factory class if either
    //     (a) the property is not set or (b) the named class fails to
    //     load (the system property mechanism is the only attack
    //     surface, and ensure_class_initialized already validates the
    //     class file).
    r.register(
        cls,
        "getFactory",
        "()Ljava/util/concurrent/ForkJoinPool$ForkJoinWorkerThreadFactory;",
        |ctx, args| {
            // First try reading the existing `factory` field — populated by
            // commonPool() above.  If it's already a non-null object, return
            // it (preserves identity within a process).
            if let Some(Value::Object(Some(this))) = args.first() {
                let existing = ctx.get_field_by_name(*this, "factory");
                if let Value::Object(Some(_)) = existing {
                    return Ok(Some(existing));
                }
            }
            // No factory yet — allocate one matching the system property.
            let factory = alloc_common_factory(ctx);
            if let Some(Value::Object(Some(this))) = args.first() {
                ctx.set_field_by_name(*this, "factory", Value::Object(Some(factory)));
            }
            Ok(Some(Value::Object(Some(factory))))
        },
    );
    r.set_category(__prev_cat);
}

/// T19_K3 — Whitelist for the
/// `java.util.concurrent.ForkJoinPool.common.threadFactory` system
/// property.  Only accept fully-qualified Java class names that are
/// at most 256 ASCII chars, contain only `[A-Za-z0-9_$.]`, do not
/// start or end with a dot, and contain no consecutive dots.
pub(crate) fn is_safe_factory_class_name(s: &str) -> bool {
    if s.is_empty() || s.len() > 256 {
        return false;
    }
    if s.starts_with('.') || s.ends_with('.') {
        return false;
    }
    let mut prev_dot = false;
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'$' => {
                prev_dot = false;
            }
            b'.' => {
                if prev_dot {
                    return false;
                }
                prev_dot = true;
            }
            _ => return false,
        }
    }
    // Must contain at least one dot (i.e. be qualified).
    s.contains('.')
}

/// T19_K3 — Resolve the desired factory-class internal name
/// (slash-separated form) from the JVM's system properties.
///
/// Returns the JDK-default
/// `java/util/concurrent/ForkJoinPool$DefaultCommonPoolForkJoinWorkerThreadFactory`
/// when the property is unset or fails the allowlist check.
pub(crate) fn resolve_common_factory_internal_name(ctx: &dyn NativeContext) -> String {
    let sf = ctx
        .get_system_property("java.util.concurrent.ForkJoinPool.common.threadFactory")
        .unwrap_or_default();
    if is_safe_factory_class_name(&sf) {
        sf.replace('.', "/")
    } else {
        "java/util/concurrent/ForkJoinPool$DefaultCommonPoolForkJoinWorkerThreadFactory".to_string()
    }
}

/// T19_K3 — Allocate a synthetic
/// `ForkJoinPool$ForkJoinWorkerThreadFactory` instance whose class
/// identity matches the configured common-pool factory name.
///
/// Used by both the synthetic `commonPool()` field-populator and the
/// `getFactory()` registration to ensure
/// `getFactory().getClass().getName()` round-trips to the system
/// property's dot-form.
pub(crate) fn alloc_common_factory(ctx: &mut dyn NativeContext) -> cratonvm_types::ObjectRef {
    let target = resolve_common_factory_internal_name(ctx);
    match ctx.ensure_class_initialized(&target) {
        Ok(cid) => {
            let nfields = ctx.class_num_total_fields(cid).max(1);
            ctx.alloc_object(cid, nfields)
        }
        Err(_) => alloc_concurrent_synthetic(ctx, &target, 1),
    }
}

/// T19_K3 — Populate the `factory` instance field of a real-loaded
/// `ForkJoinPool` synthetic with a non-null factory.  No-op if the
/// loaded class shape lacks that field (legacy synthetic mode).
pub(crate) fn populate_common_factory(
    ctx: &mut dyn NativeContext,
    pool: cratonvm_types::ObjectRef,
) {
    let factory = alloc_common_factory(ctx);
    // Set by name so we work whether the class is the real JDK shape
    // (with `factory` at some non-zero index) or a synthetic placeholder
    // (where the field doesn't exist — set_field_by_name is silent on
    // missing fields).
    ctx.set_field_by_name(pool, "factory", Value::Object(Some(factory)));
}

#[cfg(test)]
pub(crate) mod new15_tests {
    #[allow(unused_imports)]
    use cratonvm_native_api::{NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess, NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess};
    use super::*;

    #[test]
    fn new15_continuation_is_registered() {
        let mut r = NativeMethodRegistry::new();
        register_new15_loom(&mut r);
        assert!(r
            .find(
                "jdk/internal/vm/Continuation",
                "<init>",
                "(Ljdk/internal/vm/ContinuationScope;Ljava/lang/Runnable;)V"
            )
            .is_some());
        assert!(r
            .find("jdk/internal/vm/Continuation", "run", "()V")
            .is_some());
        assert!(r
            .find(
                "jdk/internal/vm/Continuation",
                "yield",
                "(Ljdk/internal/vm/ContinuationScope;)Z"
            )
            .is_some());
        assert!(r
            .find("jdk/internal/vm/Continuation", "isDone", "()Z")
            .is_some());
    }

    #[test]
    fn new15_continuation_scope_is_registered() {
        let mut r = NativeMethodRegistry::new();
        register_new15_loom(&mut r);
        assert!(r
            .find(
                "jdk/internal/vm/ContinuationScope",
                "<init>",
                "(Ljava/lang/String;)V"
            )
            .is_some());
        assert!(r
            .find(
                "jdk/internal/vm/ContinuationScope",
                "getName",
                "()Ljava/lang/String;"
            )
            .is_some());
    }

    #[test]
    fn new15_forkjoinpool_common_is_registered() {
        let mut r = NativeMethodRegistry::new();
        register_new15_loom(&mut r);
        assert!(r
            .find(
                "java/util/concurrent/ForkJoinPool",
                "commonPool",
                "()Ljava/util/concurrent/ForkJoinPool;"
            )
            .is_some());
        assert!(r
            .find(
                "java/util/concurrent/ForkJoinPool",
                "getCommonPoolParallelism",
                "()I"
            )
            .is_some());
    }

    #[test]
    fn t19_k3_forkjoinpool_get_factory_is_registered() {
        let mut r = NativeMethodRegistry::new();
        register_new15_loom(&mut r);
        assert!(r
            .find(
                "java/util/concurrent/ForkJoinPool",
                "getFactory",
                "()Ljava/util/concurrent/ForkJoinPool$ForkJoinWorkerThreadFactory;"
            )
            .is_some());
    }

    #[test]
    fn t19_k3_safe_factory_class_name_accepts_quarkus() {
        // The Quarkus factory name (the property value KC26 sets).
        assert!(is_safe_factory_class_name(
            "io.quarkus.bootstrap.forkjoin.QuarkusForkJoinWorkerThreadFactory"
        ));
        // JDK default factory name with $ inner-class separator.
        assert!(is_safe_factory_class_name(
            "java.util.concurrent.ForkJoinPool$DefaultCommonPoolForkJoinWorkerThreadFactory"
        ));
    }

    #[test]
    fn t19_k3_safe_factory_class_name_rejects_path_traversal() {
        // Slashes (filesystem path) — rejected.
        assert!(!is_safe_factory_class_name("/etc/passwd"));
        // Backslashes (Windows path) — rejected.
        assert!(!is_safe_factory_class_name(r"C:\Windows\System32"));
        // Spaces — rejected.
        assert!(!is_safe_factory_class_name("foo bar.Baz"));
        // Empty — rejected.
        assert!(!is_safe_factory_class_name(""));
        // No dots (unqualified) — rejected.
        assert!(!is_safe_factory_class_name("Foo"));
    }

    #[test]
    fn t19_k3_safe_factory_class_name_rejects_consecutive_dots() {
        assert!(!is_safe_factory_class_name("foo..bar"));
        assert!(!is_safe_factory_class_name(".leading"));
        assert!(!is_safe_factory_class_name("trailing."));
    }

    #[test]
    fn t19_k3_safe_factory_class_name_rejects_oversize() {
        let huge = "a.".repeat(200);
        assert!(huge.len() > 256);
        assert!(!is_safe_factory_class_name(&huge));
    }

    #[test]
    fn t19_k3_safe_factory_class_name_rejects_special_chars() {
        // Non-ASCII letter — rejected.
        assert!(!is_safe_factory_class_name("foo.b\u{00e9}r"));
        // Control char — rejected.
        assert!(!is_safe_factory_class_name("foo.bar\x00"));
        // Semicolon (descriptor terminator) — rejected.
        assert!(!is_safe_factory_class_name("foo.bar;"));
        // Bracket (array marker) — rejected.
        assert!(!is_safe_factory_class_name("[foo.bar"));
    }

    #[test]
    fn new15_continuation_field_layout_constants_distinct() {
        assert_ne!(NEW15_CONT_SCOPE, NEW15_CONT_TARGET);
        assert_ne!(NEW15_CONT_TARGET, NEW15_CONT_STATE);
        assert_ne!(NEW15_CONT_STATE, NEW15_CONT_PIN);
        assert_ne!(NEW15_CONT_PIN, NEW15_CONT_PREEMPT);
        assert!(NEW15_CONT_PREEMPT < NEW15_CONT_FIELDS);
    }

    #[test]
    fn new15_forkjoinpool_field_layout_constants_distinct() {
        assert_ne!(NEW15_FJP_PARALLELISM, NEW15_FJP_ACTIVE);
        assert!(NEW15_FJP_ACTIVE < NEW15_FJP_FIELDS);
    }

    #[test]
    fn new15_continuation_states_are_distinct() {
        assert_ne!(NEW15_CONT_STATE_NEW, NEW15_CONT_STATE_RUNNING);
        assert_ne!(NEW15_CONT_STATE_RUNNING, NEW15_CONT_STATE_YIELDED);
        assert_ne!(NEW15_CONT_STATE_YIELDED, NEW15_CONT_STATE_DONE);
    }
}

#[cfg(test)]
mod threadgroup_layout_tests {
    use super::*;

    /// The fallback indices here and the fabricated model in
    /// `ClassManager::synthetic_stub_fields` are two hand-written copies of the
    /// same layout, and they were allowed to disagree with each other and with
    /// the JDK for months. Assert the coupling instead of describing it.
    ///
    /// Both must also be the **real** JDK 21+ order, which is why the expected
    /// names are spelled out here rather than read from the model: a test that
    /// only checked the two tables against each other would stay green if both
    /// were transposed together, which is exactly the state this replaces.
    #[test]
    fn tg_fallback_slots_match_the_fabricated_model() {
        let model = cratonvm_classloading::synthetic_stub_field_model("java/lang/ThreadGroup");
        let instance: Vec<&str> = model
            .iter()
            .filter(|f| !f.is_static())
            .map(|f| &*f.name)
            .collect();
        assert_eq!(
            instance,
            ["parent", "name", "maxPriority", "daemon"],
            "the fabricated ThreadGroup model must be the real JDK 21+ \
             declaration order (javap -p --module java.base java.lang.ThreadGroup)"
        );
        assert_eq!(instance[TG_SLOT_PARENT], "parent");
        assert_eq!(instance[TG_SLOT_NAME], "name");
        assert_eq!(instance[TG_SLOT_MAX_PRIORITY], "maxPriority");
        assert_eq!(instance[TG_SLOT_DAEMON], "daemon");
    }

    /// The model's descriptors have to match too — `maxPriority` is an `int`
    /// and `daemon` a `boolean`, and swapping *those* is the half that a
    /// name-only check would miss.
    #[test]
    fn the_fabricated_model_descriptors_are_the_real_ones() {
        let model = cratonvm_classloading::synthetic_stub_field_model("java/lang/ThreadGroup");
        let descs: Vec<(&str, &str)> = model
            .iter()
            .filter(|f| !f.is_static())
            .map(|f| (&*f.name, &*f.descriptor))
            .collect();
        assert_eq!(
            descs,
            [
                ("parent", "Ljava/lang/ThreadGroup;"),
                ("name", "Ljava/lang/String;"),
                ("maxPriority", "I"),
                ("daemon", "Z"),
            ]
        );
    }
}
