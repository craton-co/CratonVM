#[allow(unused_imports)]
use super::*;

/// `StampedLock.unlock(long stamp)` — the stamp-dispatching release.
///
/// Added 2026-07-28. `unlock(J)V` was the one method of the real
/// `StampedLock` surface that NO registrar provided — it threw
/// `NoSuchMethodError` in both run modes. Added here alongside disabling
/// `native-collections`' rival StampedLock block (see the comment at its
/// call site), which was silently destroying mutual exclusion.
///
/// For anyone tracing history: `register_p62_stamped_lock` in
/// `phases_late/concurrent.rs` looks like the culprit and is NOT — it is
/// dead code, never called from anywhere in the tree. The live shadower was
/// `native-collections`, which wins by running after `register_builtins`.
///
/// Spec, transcribed from JDK 25 `StampedLock.unlock`:
///
/// ```java
/// public void unlock(long stamp) {
///     if ((stamp & WBIT) != 0L) unlockWrite(stamp);
///     else                      unlockRead(stamp);
/// }
/// ```
///
/// so it is a pure re-dispatch, and the `IllegalMonitorStateException` comes
/// from the callee. Note this arm tests `(stamp & WBIT) != 0`, NOT
/// `isWriteLockStamp`'s `(stamp & ABITS) == WBIT`.
///
/// Until 2026-08-07 this read the write flag as `stamp & 1` and the read
/// flag as `stamp & 2` — a private encoding this backend no longer uses; see
/// the constant block in `stamped_lock` for why the JDK's own layout is now
/// mandatory. It also released the hold WITHOUT checking the stamp's
/// version, so a stale stamp released a live hold belonging to someone else.
pub fn native_stamped_unlock_by_stamp(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(obj) = stamped_obj(args) else {
        return Ok(None);
    };
    let Some(stamp) = stamped_arg(args) else {
        // The verifier guarantees a long here; anything else means the call
        // did not come through `unlock(J)V`, so refuse rather than guess.
        return Err(RuntimeError::IllegalMonitorStateException {
            message: "unlock: missing stamp argument".to_string(),
        }
        .into());
    };
    let addr = stamped_addr_for_obj(ctx, obj)?;
    let released = if stamp & crate::stamped_lock::JDK_WBIT != 0 {
        crate::stamped_lock::stamped_unlock_write(addr, stamp)
    } else {
        // Everything else -- read stamps, optimistic stamps and the zero
        // sentinel -- goes to unlockRead, which rejects the last two.
        crate::stamped_lock::stamped_unlock_read(addr, stamp)
    };
    if !released {
        return Err(RuntimeError::IllegalMonitorStateException {
            // No message: HotSpot's is null here (measured, 3 runs). The stamp
            // is still in the caller's hand, so nothing diagnosable is lost.
            message: String::new(),
        }
        .into());
    }
    mirror_stamped_state(ctx, obj, addr);
    Ok(None)
}

pub fn native_stamped_unstamped_unlock_read(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    if let Some(obj) = stamped_obj(args) {
        let addr = stamped_addr_for_obj(ctx, obj)?;
        if !crate::stamped_lock::stamped_try_unstamped_unlock_read(addr) {
            return Err(RuntimeError::IllegalMonitorStateException {
                message: "StampedLock read lock not held".to_string(),            }
            .into());        }
        mirror_stamped_state(ctx, obj, addr);
    }
    Ok(None)
}

pub fn native_stamped_unstamped_unlock_write(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    if let Some(obj) = stamped_obj(args) {
        let addr = stamped_addr_for_obj(ctx, obj)?;
        if !crate::stamped_lock::stamped_try_unstamped_unlock_write(addr) {
            return Err(RuntimeError::IllegalMonitorStateException {
                message: "StampedLock write lock not held".to_string(),            }
            .into());        }
        mirror_stamped_state(ctx, obj, addr);
    }
    Ok(None)
}

pub fn native_stamped_try_unlock_read(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(obj) = stamped_obj(args) else {
        return Ok(Some(Value::Int(0)));
    };
    let addr = stamped_addr_for_obj(ctx, obj)?;
    let unlocked = crate::stamped_lock::stamped_try_unstamped_unlock_read(addr);
    mirror_stamped_state(ctx, obj, addr);
    Ok(Some(Value::Int(i32::from(unlocked))))
}

pub fn native_stamped_try_unlock_write(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let Some(obj) = stamped_obj(args) else {
        return Ok(Some(Value::Int(0)));
    };
    let addr = stamped_addr_for_obj(ctx, obj)?;
    let unlocked = crate::stamped_lock::stamped_try_unstamped_unlock_write(addr);
    mirror_stamped_state(ctx, obj, addr);
    Ok(Some(Value::Int(i32::from(unlocked))))
}

pub fn native_stamped_try_convert_to_write(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let stamp = match args.get(1) {
        Some(Value::Long(v)) => *v,
        _ => return Ok(Some(Value::Long(0))),
    };
    let obj = match stamped_obj(args) {
        Some(o) => o,
        None => return Ok(Some(Value::Long(0))),
    };
    let addr = stamped_addr_for_obj(ctx, obj)?;
    let converted = crate::stamped_lock::stamped_try_convert_to_write(addr, stamp);
    mirror_stamped_state(ctx, obj, addr);
    Ok(Some(Value::Long(converted)))
}

pub fn native_stamped_try_convert_to_read(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let stamp = match args.get(1) {
        Some(Value::Long(v)) => *v,
        _ => return Ok(Some(Value::Long(0))),
    };
    let obj = match stamped_obj(args) {
        Some(o) => o,
        None => return Ok(Some(Value::Long(0))),
    };
    let addr = stamped_addr_for_obj(ctx, obj)?;
    let converted = crate::stamped_lock::stamped_try_convert_to_read(addr, stamp);
    mirror_stamped_state(ctx, obj, addr);
    Ok(Some(Value::Long(converted)))
}

/// `tryConvertToOptimisticRead(long stamp)` -- releases the hold `stamp`
/// names and returns an observation stamp (0 when the stamp is stale).
pub fn native_stamped_try_convert_to_optimistic(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let stamp = match args.get(1) {
        Some(Value::Long(v)) => *v,
        _ => return Ok(Some(Value::Long(0))),
    };
    let obj = match stamped_obj(args) {
        Some(o) => o,
        None => return Ok(Some(Value::Long(0))),
    };
    let addr = stamped_addr_for_obj(ctx, obj)?;
    let converted = crate::stamped_lock::stamped_try_convert_to_optimistic(addr, stamp);
    mirror_stamped_state(ctx, obj, addr);
    if crate::nbflags().dbg_stamped {
        eprintln!(
            "[SL-DBG] tryConvertToOptimisticRead addr={addr:#x} stamp={stamp} -> {converted}"
        );
    }
    Ok(Some(Value::Long(converted)))
}

/// `readLockInterruptibly()` / `writeLockInterruptibly()`.
///
/// This backend parks on a `parking_lot::Condvar`, which has no interrupt
/// channel, so these behave as the uninterruptible acquire. That is a
/// liveness-preserving approximation; leaving them to real JDK bytecode is
/// not, because that bytecode queues on a `state` word this backend never
/// writes and would never be released.
pub fn native_stamped_read_lock_interruptibly(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    native_stamped_read_lock(ctx, args)
}

pub fn native_stamped_write_lock_interruptibly(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    native_stamped_write_lock(ctx, args)
}

/// Convert a `(long time, TimeUnit unit)` argument pair to nanoseconds by
/// asking the unit itself, so any TimeUnit constant works without a table.
pub fn stamped_timeout_nanos(ctx: &mut dyn NativeContext, args: &[Value]) -> i64 {
    let time = match args.get(1) {
        Some(Value::Long(v)) => *v,
        _ => return 0,
    };
    let unit = match args.get(2) {
        Some(Value::Object(Some(u))) => *u,
        _ => return 0,
    };
    match ctx.invoke_virtual(unit, "toNanos", "(J)J", &[Value::Long(time)]) {
        Ok(Some(Value::Long(n))) => n,
        _ => 0,
    }
}

pub fn native_stamped_try_read_lock_timed(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let obj = match stamped_obj(args) {
        Some(o) => o,
        None => return Ok(Some(Value::Long(0))),
    };
    // Wave 68 (lane L1): `TimeUnit.toNanos` runs Java, which can collect, and
    // `obj` is now also stored as the thread's `parkBlocker`: re-read it from
    // a pin.
    let obj_pin = ctx.pin_native_root(obj);
    let nanos = stamped_timeout_nanos(ctx, args);
    let obj = ctx.read_native_pin(obj_pin, obj);
    ctx.unpin_native_roots(obj_pin);
    let addr = stamped_addr_for_obj(ctx, obj)?;
    // Interpreter round i2 wave 60, lane L6: a timed acquire parks with a
    // deadline, TIMED_WAITING on HotSpot (`L6W60TimedStandInStates`).
    // Wave 65 (lane L1): a park with a deadline, JVMTI PARKED.
    // Wave 68 (lane L1, `CRATONVM_STAND_IN_WAIT_OBJECTS`): the JDK's acquire
    // parks with the lock itself as the blocker (`LockSupport.park(this)`),
    // which JMX names.
    ctx.begin_blocking_region_as_on(obj, cratonvm_native_api::BlockKind::TimedParked);
    let stamp = crate::stamped_lock::stamped_try_read_lock_timed(addr, nanos);
    let obj = stamped_end_region_reread(ctx, obj);
    mirror_stamped_state(ctx, obj, addr);
    Ok(Some(Value::Long(stamp)))
}

pub fn native_stamped_try_write_lock_timed(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let obj = match stamped_obj(args) {
        Some(o) => o,
        None => return Ok(Some(Value::Long(0))),
    };
    // Wave 68 (lane L1): `TimeUnit.toNanos` runs Java, which can collect, and
    // `obj` is now also stored as the thread's `parkBlocker`: re-read it from
    // a pin.
    let obj_pin = ctx.pin_native_root(obj);
    let nanos = stamped_timeout_nanos(ctx, args);
    let obj = ctx.read_native_pin(obj_pin, obj);
    ctx.unpin_native_roots(obj_pin);
    let addr = stamped_addr_for_obj(ctx, obj)?;
    // Wave 60 (lane L6): TIMED_WAITING, as the read acquire above; wave 65
    // (lane L1): a park, JVMTI PARKED.
    // Wave 68 (lane L1, `CRATONVM_STAND_IN_WAIT_OBJECTS`): the JDK's acquire
    // parks with the lock itself as the blocker (`LockSupport.park(this)`),
    // which JMX names.
    ctx.begin_blocking_region_as_on(obj, cratonvm_native_api::BlockKind::TimedParked);
    let stamp = crate::stamped_lock::stamped_try_write_lock_timed(addr, nanos);
    let obj = stamped_end_region_reread(ctx, obj);
    mirror_stamped_state(ctx, obj, addr);
    Ok(Some(Value::Long(stamp)))
}

pub fn native_stamped_is_locked(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let addr = match stamped_addr(ctx, args)? {
        Some(a) => a,
        None => return Ok(Some(Value::Int(0))),
    };
    Ok(Some(Value::Int(i32::from(
        crate::stamped_lock::stamped_is_locked(addr),
    ))))
}

pub fn native_stamped_is_write_locked(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let addr = match stamped_addr(ctx, args)? {
        Some(a) => a,
        None => return Ok(Some(Value::Int(0))),
    };
    let held = crate::stamped_lock::stamped_is_write_locked(addr);
    if crate::nbflags().dbg_stamped {
        eprintln!("[SL-DBG] isWriteLocked addr={addr:#x} held={held}");
    }
    Ok(Some(Value::Int(i32::from(held))))
}

pub fn native_stamped_is_read_locked(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let addr = match stamped_addr(ctx, args)? {
        Some(a) => a,
        None => return Ok(Some(Value::Int(0))),
    };
    Ok(Some(Value::Int(i32::from(
        crate::stamped_lock::stamped_is_read_locked(addr),
    ))))
}

pub fn native_stamped_get_read_lock_count(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let addr = match stamped_addr(ctx, args)? {
        Some(a) => a,
        None => return Ok(Some(Value::Int(0))),
    };
    Ok(Some(Value::Int(
        crate::stamped_lock::stamped_get_read_lock_count(addr),
    )))
}

pub fn register_atomic_extras_natives(registry: &mut NativeMethodRegistry) {
    // census-tag: LongAdder / DoubleAdder / Atomic*Array are VM-internal atomic
    // primitives with spec-correct semantics → Bridge.
            // LongAdder = 1-field synthetic (sum=0 Long)
    let la = "java/util/concurrent/atomic/LongAdder";
    registry.register(la, "<init>", "()V", native_long_adder_init, cratonvm_native_api::NativeKind::Bridge);
    registry.register(la, "add", "(J)V", native_long_adder_add, cratonvm_native_api::NativeKind::Bridge);
    registry.register(la, "increment", "()V", native_long_adder_increment, cratonvm_native_api::NativeKind::Bridge);
    registry.register(la, "decrement", "()V", native_long_adder_decrement, cratonvm_native_api::NativeKind::Bridge);
    registry.register(la, "sum", "()J", native_long_adder_sum, cratonvm_native_api::NativeKind::Bridge);
    registry.register(la, "longValue", "()J", native_long_adder_sum, cratonvm_native_api::NativeKind::Bridge);
    registry.register(la, "intValue", "()I", native_long_adder_int_value, cratonvm_native_api::NativeKind::Bridge);
    registry.register(la, "reset", "()V", native_long_adder_reset, cratonvm_native_api::NativeKind::Bridge);
    registry.register(la, "sumThenReset", "()J", native_long_adder_sum_then_reset, cratonvm_native_api::NativeKind::Bridge);
    registry.register(
        la,
        "toString",
        "()Ljava/lang/String;",
        native_long_adder_to_string, cratonvm_native_api::NativeKind::Bridge,
    );

    // DoubleAdder = 1-field synthetic (sum=0 Double)
    let da = "java/util/concurrent/atomic/DoubleAdder";
    registry.register(da, "<init>", "()V", native_double_adder_init, cratonvm_native_api::NativeKind::Bridge);
    registry.register(da, "add", "(D)V", native_double_adder_add, cratonvm_native_api::NativeKind::Bridge);
    registry.register(da, "sum", "()D", native_double_adder_sum, cratonvm_native_api::NativeKind::Bridge);
    registry.register(da, "doubleValue", "()D", native_double_adder_sum, cratonvm_native_api::NativeKind::Bridge);
    registry.register(da, "reset", "()V", native_double_adder_reset, cratonvm_native_api::NativeKind::Bridge);

    // AtomicIntegerArray = 1-field synthetic (backing int[])
    let aia = "java/util/concurrent/atomic/AtomicIntegerArray";
    registry.register(aia, "<init>", "(I)V", native_aia_init, cratonvm_native_api::NativeKind::Bridge);
    registry.register(aia, "get", "(I)I", native_aia_get, cratonvm_native_api::NativeKind::Bridge);
    registry.register(aia, "set", "(II)V", native_aia_set, cratonvm_native_api::NativeKind::Bridge);
    registry.register(aia, "getAndSet", "(II)I", native_aia_get_and_set, cratonvm_native_api::NativeKind::Bridge);
    registry.register(aia, "compareAndSet", "(III)Z", native_aia_cas, cratonvm_native_api::NativeKind::Bridge);
    registry.register(aia, "getAndIncrement", "(I)I", native_aia_get_and_inc, cratonvm_native_api::NativeKind::Bridge);
    registry.register(aia, "getAndDecrement", "(I)I", native_aia_get_and_dec, cratonvm_native_api::NativeKind::Bridge);
    registry.register(aia, "getAndAdd", "(II)I", native_aia_get_and_add, cratonvm_native_api::NativeKind::Bridge);
    registry.register(aia, "incrementAndGet", "(I)I", native_aia_inc_and_get, cratonvm_native_api::NativeKind::Bridge);
    registry.register(aia, "decrementAndGet", "(I)I", native_aia_dec_and_get, cratonvm_native_api::NativeKind::Bridge);
    registry.register(aia, "length", "()I", native_aia_length, cratonvm_native_api::NativeKind::Bridge);
    // The interpreter force-routes this whole method set to a native (see the
    // "C23" block in vm_exec.rs): the real JDK bodies go through
    // `VarHandles$Array$*`, which cannot CAS an array element here. Anything
    // on that list without a registration silently falls back to exactly the
    // bytecode the force-route exists to avoid, so register the full set.
    registry.register(aia, "addAndGet", "(II)I", native_aia_add_and_get, cratonvm_native_api::NativeKind::Bridge);
    registry.register(aia, "lazySet", "(II)V", native_aia_set, cratonvm_native_api::NativeKind::Bridge);
    registry.register(aia, "setPlain", "(II)V", native_aia_set, cratonvm_native_api::NativeKind::Bridge);
    registry.register(aia, "setOpaque", "(II)V", native_aia_set, cratonvm_native_api::NativeKind::Bridge);
    registry.register(aia, "setRelease", "(II)V", native_aia_set, cratonvm_native_api::NativeKind::Bridge);
    registry.register(aia, "getPlain", "(I)I", native_aia_get, cratonvm_native_api::NativeKind::Bridge);
    registry.register(aia, "getOpaque", "(I)I", native_aia_get, cratonvm_native_api::NativeKind::Bridge);
    registry.register(aia, "getAcquire", "(I)I", native_aia_get, cratonvm_native_api::NativeKind::Bridge);
    registry.register(aia, "weakCompareAndSet", "(III)Z", native_aia_cas, cratonvm_native_api::NativeKind::Bridge);
    registry.register(aia, "weakCompareAndSetPlain", "(III)Z", native_aia_cas, cratonvm_native_api::NativeKind::Bridge);
    registry.register(aia, "weakCompareAndSetAcquire", "(III)Z", native_aia_cas, cratonvm_native_api::NativeKind::Bridge);
    registry.register(aia, "weakCompareAndSetRelease", "(III)Z", native_aia_cas, cratonvm_native_api::NativeKind::Bridge);
    registry.register(aia, "compareAndExchange", "(III)I", native_aia_cae, cratonvm_native_api::NativeKind::Bridge);
    registry.register(aia, "compareAndExchangeAcquire", "(III)I", native_aia_cae, cratonvm_native_api::NativeKind::Bridge);
    registry.register(aia, "compareAndExchangeRelease", "(III)I", native_aia_cae, cratonvm_native_api::NativeKind::Bridge);

    // AtomicLongArray = 1-field synthetic (backing long[])
    let ala = "java/util/concurrent/atomic/AtomicLongArray";
    registry.register(ala, "<init>", "(I)V", native_ala_init, cratonvm_native_api::NativeKind::Bridge);
    registry.register(ala, "get", "(I)J", native_ala_get, cratonvm_native_api::NativeKind::Bridge);
    registry.register(ala, "set", "(IJ)V", native_ala_set, cratonvm_native_api::NativeKind::Bridge);
    registry.register(ala, "getAndSet", "(IJ)J", native_ala_get_and_set, cratonvm_native_api::NativeKind::Bridge);
    registry.register(ala, "compareAndSet", "(IJJ)Z", native_ala_cas, cratonvm_native_api::NativeKind::Bridge);
    registry.register(ala, "getAndIncrement", "(I)J", native_ala_get_and_inc, cratonvm_native_api::NativeKind::Bridge);
    registry.register(ala, "incrementAndGet", "(I)J", native_ala_inc_and_get, cratonvm_native_api::NativeKind::Bridge);
    registry.register(ala, "length", "()I", native_ala_length, cratonvm_native_api::NativeKind::Bridge);
    registry.register(ala, "getAndAdd", "(IJ)J", native_ala_get_and_add, cratonvm_native_api::NativeKind::Bridge);
    registry.register(ala, "addAndGet", "(IJ)J", native_ala_add_and_get, cratonvm_native_api::NativeKind::Bridge);
    registry.register(ala, "getAndDecrement", "(I)J", native_ala_get_and_dec, cratonvm_native_api::NativeKind::Bridge);
    registry.register(ala, "decrementAndGet", "(I)J", native_ala_dec_and_get, cratonvm_native_api::NativeKind::Bridge);
    registry.register(ala, "lazySet", "(IJ)V", native_ala_set, cratonvm_native_api::NativeKind::Bridge);
    registry.register(ala, "setPlain", "(IJ)V", native_ala_set, cratonvm_native_api::NativeKind::Bridge);
    registry.register(ala, "setOpaque", "(IJ)V", native_ala_set, cratonvm_native_api::NativeKind::Bridge);
    registry.register(ala, "setRelease", "(IJ)V", native_ala_set, cratonvm_native_api::NativeKind::Bridge);
    registry.register(ala, "getPlain", "(I)J", native_ala_get, cratonvm_native_api::NativeKind::Bridge);
    registry.register(ala, "getOpaque", "(I)J", native_ala_get, cratonvm_native_api::NativeKind::Bridge);
    registry.register(ala, "getAcquire", "(I)J", native_ala_get, cratonvm_native_api::NativeKind::Bridge);
    registry.register(ala, "weakCompareAndSet", "(IJJ)Z", native_ala_cas, cratonvm_native_api::NativeKind::Bridge);
    registry.register(ala, "weakCompareAndSetPlain", "(IJJ)Z", native_ala_cas, cratonvm_native_api::NativeKind::Bridge);
    registry.register(ala, "weakCompareAndSetAcquire", "(IJJ)Z", native_ala_cas, cratonvm_native_api::NativeKind::Bridge);
    registry.register(ala, "weakCompareAndSetRelease", "(IJJ)Z", native_ala_cas, cratonvm_native_api::NativeKind::Bridge);
    registry.register(ala, "compareAndExchange", "(IJJ)J", native_ala_cae, cratonvm_native_api::NativeKind::Bridge);
    registry.register(ala, "compareAndExchangeAcquire", "(IJJ)J", native_ala_cae, cratonvm_native_api::NativeKind::Bridge);
    registry.register(ala, "compareAndExchangeRelease", "(IJJ)J", native_ala_cae, cratonvm_native_api::NativeKind::Bridge);
    }

// ---------------------------------------------------------------------------
// Atomic array element read-modify-write
// ---------------------------------------------------------------------------
//
// `AtomicIntegerArray` / `AtomicLongArray` / `AtomicReferenceArray` are backed
// by a plain Java array here, and every read-modify-write native used to be a
// bare `get_array_element` + `set_array_element` pair with nothing in between.
// That is not a compare-and-swap: two threads can both read the expected value
// and both report success, so `compareAndSet` did not provide mutual exclusion
// at all.
//
// H2's `TestFileSystem.testConcurrent` uses exactly that idiom as a spin lock
// (`while (!locks.compareAndSet(pos, 0, 1)) {}`), so a writer and a reader
// could hold the same "lock" simultaneously and the reader observed a stale
// `expected` against freshly written file contents -- surfacing as
// `AssertionError: Expected: 3900 actual: 3897`. It was previously attributed
// to compiled `org/h2` code reordering across the atomic; the atomic simply
// was not one. (2026-07-27)
//
// `NativeContext::compare_and_swap_field` already special-cases array
// receivers and performs the read/compare/write under the per-object CAS lock,
// which is what the scalar `AtomicInteger` natives have always used. Route
// every array RMW through it.

/// Range-check an atomic-array element index, throwing HotSpot's
/// `ArrayIndexOutOfBoundsException` when it is out of range.
///
/// **Why this has to exist at all.** The heap DOES range-check --
/// `VmHeap::get_array_element` returns `Err(index)` past the end and
/// `set_array_element` writes nothing -- but `vm_exec.rs`'s `NativeContext`
/// impl deliberately swallows both results (`get` ends in a typed default,
/// `set` in `let _ = ...`), and its own doc comment says so: *"The caller
/// range-checks."* Every `AtomicIntegerArray` / `AtomicLongArray` /
/// `AtomicReferenceArray` native below was a caller that never did. Measured
/// on a 3-element array:
///
/// ```text
///                            HotSpot 25.0.3+9        CratonVM (before)
/// ARA.get(-1)                AIOOBE                  null
/// ARA.get(3) / get(100)      AIOOBE                  null
/// AIA.get(-1) / get(3)       AIOOBE                  0
/// ARA.set(-1, "x")           AIOOBE                  silently no-op
/// AIA.compareAndSet(-1,0,9)  AIOOBE                  returned true, wrote nothing
/// ```
///
/// This is a *missing-exception* defect, not a memory-safety one: the heap's
/// own check still suppresses the access, so no out-of-range read or write
/// reaches adjacent memory (verified with a sentinel neighbour swept over
/// -32..63). It is still serious -- an off-by-one gets a plausible `null`/`0`
/// and the program keeps running, and a *write* that silently does nothing is
/// worse than a read that answers wrong, because the loss surfaces arbitrarily
/// far away. A `compareAndSet` that reports success without storing is the
/// sharpest form: it is the same idiom H2's `TestFileSystem.testConcurrent`
/// uses as a spin lock.
///
/// The check lives on a shared funnel rather than on each accessor for the
/// reason `lang_invoke`'s `vh_array_index` gives for the identical decision on
/// the VarHandle side: there are ~26 registered triples per class here, and a
/// check added to twenty-five of them is a silent hole in the twenty-sixth.
/// `RuntimeError::aioobe` produces HotSpot's exact text
/// (`Index -1 out of bounds for length 3`).
///
/// The negative case is the one that mattered most: the old code did
/// `*v as usize`, so `-1` became `usize::MAX` and only the heap's own
/// `index >= len` test stopped it from being a wild read.
pub fn atomic_array_index(
    ctx: &dyn NativeContext,
    arr: ObjectRef,
    idx: i32,
) -> Result<usize, MethodCallFailed> {
    let len = ctx.array_length(arr);
    if idx < 0 || (idx as usize) >= len {
        return Err(RuntimeError::aioobe(idx, len as i32).into());
    }
    Ok(idx as usize)
}

/// Read the `int` index argument of an atomic-array native WITHOUT widening it
/// to `usize`. The widening is what erased the sign; keep it `i32` until
/// [`atomic_array_index`] has passed judgement on it.
pub fn atomic_array_raw_index(args: &[Value]) -> i32 {
    match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    }
}

/// Validate the `length` argument of an atomic-array constructor.
///
/// `new AtomicIntegerArray(-1)` throws `NegativeArraySizeException: -1` on
/// HotSpot (measured; same for the `Long` and `Reference` twins). The old code
/// did `as usize`, handing `18446744073709551615` to `new_array` -- which is
/// either a catchable `OutOfMemoryError` or a hard abort depending on whether
/// a JIT frame is in between, and is the wrong exception either way.
pub fn atomic_array_new_length(len: i32) -> Result<usize, MethodCallFailed> {
    if len < 0 {
        return Err(RuntimeError::NegativeArraySizeException { size: len }.into());
    }
    Ok(len as usize)
}

/// Atomically apply `f` to element `idx` of `arr`, retrying until the CAS wins.
/// Returns `(previous, new)`. `f` must be side-effect free -- it can run more
/// than once.
pub fn atomic_array_rmw<F>(
    ctx: &mut dyn NativeContext,
    arr: ObjectRef,
    idx: usize,
    mut f: F,
) -> (Value, Value)
where
    F: FnMut(Value) -> Value,
{
    loop {
        let current = ctx.get_array_element(arr, idx);
        let updated = f(current);
        if ctx.compare_and_swap_field(arr, idx, current, updated) {
            return (current, updated);
        }
    }
}

/// Atomic `compareAndSet` on element `idx` of `arr`.
pub fn atomic_array_cas(
    ctx: &mut dyn NativeContext,
    arr: ObjectRef,
    idx: usize,
    expected: Value,
    update: Value,
) -> bool {
    ctx.compare_and_swap_field(arr, idx, expected, update)
}

/// Resolve `(backing array, RANGE-CHECKED index)` for an atomic-array element
/// native whose backing array lives in slot 0 (`AtomicIntegerArray` /
/// `AtomicLongArray`).
///
/// `Ok(None)` means the receiver or the backing array is missing, which keeps
/// each caller's historical typed default. `Err` means the index was out of
/// range and HotSpot would have thrown -- see [`atomic_array_index`].
pub fn atomic_array_slot(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> Result<Option<(ObjectRef, usize)>, MethodCallFailed> {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let raw = atomic_array_raw_index(args);
    let arr = match ctx.get_field(this, 0) {
        Value::Object(Some(o)) => o,
        _ => return Ok(None),
    };
    Ok(Some((arr, atomic_array_index(ctx, arr, raw)?)))}

/// A fresh `elem` array of `len` stored into field `index` of `this`; answers
/// `this`'s current address.
///
/// gc-common w19-b: the synthetic constructors (`LinkedBlockingQueue`,
/// `ArrayBlockingQueue`, `LinkedTransferQueue`, `Atomic{Integer,Long}Array`)
/// held their receiver -- a raw copy out of `args` -- across the backing
/// array's allocation, which can collect, and stored the array into the
/// receiver's earlier address. `this` is pinned across that allocation.
pub fn uce_store_fresh_array(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    index: usize,
    elem: cratonvm_types::ArrayElementType,
    len: usize,
) -> ObjectRef {
    let pin = ctx.pin_native_root(this);
    let arr = ctx.new_array(elem, len);
    let this_now = ctx.read_native_pin(pin, this);
    ctx.unpin_native_roots(pin);
    ctx.set_field(this_now, index, Value::Object(Some(arr)));
    this_now
}

/// [`uce_store_fresh_array`] for the synthetic queues' reference backing
/// array in field 0.
pub fn uce_store_fresh_ref_array(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    len: usize,
) -> ObjectRef {
    uce_store_fresh_array(
        ctx,
        this,
        0,
        cratonvm_types::ArrayElementType::Reference,
        len,
    )
}

// --- AtomicIntegerArray ---
pub fn native_aia_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let len = atomic_array_new_length(atomic_array_raw_index(args))?;
    uce_store_fresh_array(ctx, this, 0, cratonvm_types::ArrayElementType::Int, len);
    Ok(None)
}

pub fn native_aia_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let (arr, idx) = match atomic_array_slot(ctx, args)? {
        Some(t) => t,
        None => return Ok(Some(Value::Int(0))),
    };
    Ok(Some(ctx.get_array_element(arr, idx)))}

pub fn native_aia_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let val = match args.get(2) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let (arr, idx) = match atomic_array_slot(ctx, args)? {
        Some(t) => t,
        None => return Ok(None),
    };
    ctx.set_array_element(arr, idx, Value::Int(val));
    Ok(None)
}

pub fn native_aia_get_and_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let (arr, idx) = match atomic_array_slot(ctx, args)? {
        Some(t) => t,
        None => return Ok(Some(Value::Int(0))),
    };
    let new_val = match args.get(2) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let (old, _) = atomic_array_rmw(ctx, arr, idx, |_| Value::Int(new_val));
    Ok(Some(old))
}

pub fn native_aia_cas(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let (arr, idx) = match atomic_array_slot(ctx, args)? {
        Some(t) => t,
        None => return Ok(Some(Value::Int(0))),
    };
    let expected = match args.get(2) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let update = match args.get(3) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let ok = atomic_array_cas(ctx, arr, idx, Value::Int(expected), Value::Int(update));
    Ok(Some(Value::Int(i32::from(ok))))
}

pub fn native_aia_get_and_inc(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let (arr, idx) = match atomic_array_slot(ctx, args)? {
        Some(t) => t,
        None => return Ok(Some(Value::Int(0))),
    };
    let (old, new_val) = atomic_array_rmw(ctx, arr, idx, |cur| {
        Value::Int(cur.as_int().unwrap_or(0).wrapping_add(1))
    });
    let _ = (old, new_val);
    Ok(Some(old))
}

pub fn native_aia_get_and_dec(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let (arr, idx) = match atomic_array_slot(ctx, args)? {
        Some(t) => t,
        None => return Ok(Some(Value::Int(0))),
    };
    let (old, new_val) = atomic_array_rmw(ctx, arr, idx, |cur| {
        Value::Int(cur.as_int().unwrap_or(0).wrapping_sub(1))
    });
    let _ = (old, new_val);
    Ok(Some(old))
}

pub fn native_aia_get_and_add(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let (arr, idx) = match atomic_array_slot(ctx, args)? {
        Some(t) => t,
        None => return Ok(Some(Value::Int(0))),
    };
    let delta = match args.get(2) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let (old, _) = atomic_array_rmw(ctx, arr, idx, |cur| {
        Value::Int(cur.as_int().unwrap_or(0).wrapping_add(delta))
    });
    Ok(Some(old))
}

pub fn native_aia_inc_and_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let (arr, idx) = match atomic_array_slot(ctx, args)? {
        Some(t) => t,
        None => return Ok(Some(Value::Int(0))),
    };
    let (old, new_val) = atomic_array_rmw(ctx, arr, idx, |cur| {
        Value::Int(cur.as_int().unwrap_or(0).wrapping_add(1))
    });
    let _ = (old, new_val);
    Ok(Some(new_val))
}

pub fn native_aia_dec_and_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let (arr, idx) = match atomic_array_slot(ctx, args)? {
        Some(t) => t,
        None => return Ok(Some(Value::Int(0))),
    };
    let (old, new_val) = atomic_array_rmw(ctx, arr, idx, |cur| {
        Value::Int(cur.as_int().unwrap_or(0).wrapping_sub(1))
    });
    let _ = (old, new_val);
    Ok(Some(new_val))
}

pub fn native_aia_length(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let arr = match ctx.get_field(this, 0) {
        Value::Object(Some(o)) => o,
        _ => return Ok(Some(Value::Int(0))),
    };
    Ok(Some(Value::Int(ctx.array_length(arr) as i32)))}

pub fn native_aia_add_and_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let delta = match args.get(2) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let (arr, idx) = match atomic_array_slot(ctx, args)? {
        Some(t) => t,
        None => return Ok(Some(Value::Int(0))),
    };
    let (_, new_val) = atomic_array_rmw(ctx, arr, idx, |cur| {
        Value::Int(cur.as_int().unwrap_or(0).wrapping_add(delta))
    });
    Ok(Some(new_val))
}

/// `compareAndExchange`: like `compareAndSet` but returns the WITNESS value
/// (the value actually found), not a boolean.
pub fn native_aia_cae(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let expected = match args.get(2) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let update = match args.get(3) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let (arr, idx) = match atomic_array_slot(ctx, args)? {
        Some(t) => t,
        None => return Ok(Some(Value::Int(0))),
    };
    loop {
        let current = ctx.get_array_element(arr, idx);
        if current.as_int().unwrap_or(0) != expected {
            return Ok(Some(current));
        }
        if atomic_array_cas(ctx, arr, idx, current, Value::Int(update)) {
            return Ok(Some(current));
        }
    }
}

// --- AtomicLongArray ---
pub fn native_ala_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let len = atomic_array_new_length(atomic_array_raw_index(args))?;
    uce_store_fresh_array(ctx, this, 0, cratonvm_types::ArrayElementType::Long, len);
    Ok(None)
}

pub fn native_ala_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let (arr, idx) = match atomic_array_slot(ctx, args)? {
        Some(t) => t,
        None => return Ok(Some(Value::Long(0))),
    };
    Ok(Some(ctx.get_array_element(arr, idx)))}

pub fn native_ala_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let val = match args.get(2) {
        Some(Value::Long(v)) => *v,
        _ => 0,
    };
    let (arr, idx) = match atomic_array_slot(ctx, args)? {
        Some(t) => t,
        None => return Ok(None),
    };
    ctx.set_array_element(arr, idx, Value::Long(val));
    Ok(None)
}

pub fn native_ala_get_and_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let (arr, idx) = match atomic_array_slot(ctx, args)? {
        Some(t) => t,
        None => return Ok(Some(Value::Long(0))),
    };
    let new_val = match args.get(2) {
        Some(Value::Long(v)) => *v,
        _ => 0,
    };
    let (old, _) = atomic_array_rmw(ctx, arr, idx, |_| Value::Long(new_val));
    Ok(Some(old))
}

pub fn native_ala_cas(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let (arr, idx) = match atomic_array_slot(ctx, args)? {
        Some(t) => t,
        None => return Ok(Some(Value::Int(0))),
    };
    let expected = match args.get(2) {
        Some(Value::Long(v)) => *v,
        _ => 0,
    };
    let update = match args.get(3) {
        Some(Value::Long(v)) => *v,
        _ => 0,
    };
    let ok = atomic_array_cas(ctx, arr, idx, Value::Long(expected), Value::Long(update));
    Ok(Some(Value::Int(i32::from(ok))))
}

pub fn native_ala_get_and_inc(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let (arr, idx) = match atomic_array_slot(ctx, args)? {
        Some(t) => t,
        None => return Ok(Some(Value::Long(0))),
    };
    let (old, _) = atomic_array_rmw(ctx, arr, idx, |cur| {
        Value::Long(cur.as_long().unwrap_or(0).wrapping_add(1))
    });
    Ok(Some(old))
}

pub fn native_ala_inc_and_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let (arr, idx) = match atomic_array_slot(ctx, args)? {
        Some(t) => t,
        None => return Ok(Some(Value::Long(0))),
    };
    let (_, new_val) = atomic_array_rmw(ctx, arr, idx, |cur| {
        Value::Long(cur.as_long().unwrap_or(0).wrapping_add(1))
    });
    Ok(Some(new_val))
}

/// Resolve `(backing array, RANGE-CHECKED index)` for an `AtomicLongArray`
/// native call. Was an unchecked `Option`-returning helper that widened the
/// index with `as usize`; it is now a thin alias for [`atomic_array_slot`] so
/// the five callers below get the bounds check for free rather than each
/// needing its own.
pub fn ala_target(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> Result<Option<(ObjectRef, usize)>, MethodCallFailed> {
    atomic_array_slot(ctx, args)
}

pub fn native_ala_get_and_add(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let (arr, idx) = match ala_target(ctx, args)? {
        Some(t) => t,
        None => return Ok(Some(Value::Long(0))),
    };
    let delta = match args.get(2) {
        Some(Value::Long(v)) => *v,
        _ => 0,
    };
    let (old, _) = atomic_array_rmw(ctx, arr, idx, |cur| {
        Value::Long(cur.as_long().unwrap_or(0).wrapping_add(delta))
    });
    Ok(Some(old))
}

pub fn native_ala_add_and_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let (arr, idx) = match ala_target(ctx, args)? {
        Some(t) => t,
        None => return Ok(Some(Value::Long(0))),
    };
    let delta = match args.get(2) {
        Some(Value::Long(v)) => *v,
        _ => 0,
    };
    let (_, new_val) = atomic_array_rmw(ctx, arr, idx, |cur| {
        Value::Long(cur.as_long().unwrap_or(0).wrapping_add(delta))
    });
    Ok(Some(new_val))
}

pub fn native_ala_get_and_dec(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let (arr, idx) = match ala_target(ctx, args)? {
        Some(t) => t,
        None => return Ok(Some(Value::Long(0))),
    };
    let (old, _) = atomic_array_rmw(ctx, arr, idx, |cur| {
        Value::Long(cur.as_long().unwrap_or(0).wrapping_sub(1))
    });
    Ok(Some(old))
}

pub fn native_ala_dec_and_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let (arr, idx) = match ala_target(ctx, args)? {
        Some(t) => t,
        None => return Ok(Some(Value::Long(0))),
    };
    let (_, new_val) = atomic_array_rmw(ctx, arr, idx, |cur| {
        Value::Long(cur.as_long().unwrap_or(0).wrapping_sub(1))
    });
    Ok(Some(new_val))
}

pub fn native_ala_cae(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let (arr, idx) = match ala_target(ctx, args)? {
        Some(t) => t,
        None => return Ok(Some(Value::Long(0))),
    };
    let expected = match args.get(2) {
        Some(Value::Long(v)) => *v,
        _ => 0,
    };
    let update = match args.get(3) {
        Some(Value::Long(v)) => *v,
        _ => 0,
    };
    loop {
        let current = ctx.get_array_element(arr, idx);
        if current.as_long().unwrap_or(0) != expected {
            return Ok(Some(current));
        }
        if atomic_array_cas(ctx, arr, idx, current, Value::Long(update)) {
            return Ok(Some(current));
        }
    }
}

pub fn native_ala_length(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let arr = match ctx.get_field(this, 0) {
        Value::Object(Some(o)) => o,
        _ => return Ok(Some(Value::Int(0))),
    };
    Ok(Some(Value::Int(ctx.array_length(arr) as i32)))}

/// `Collections.synchronizedMap(m)` — return a REAL `Collections$SynchronizedMap`
/// wrapper, not the bare map. The historical identity stub left every method
/// unsynchronized, so `synchronizedMap(...).computeIfAbsent(...)` was not atomic
/// under concurrency: Spring's `ConcurrencyLimitBeanPostProcessor` keeps its
/// per-proxy throttle holders in a `synchronizedMap(IdentityHashMap)` and races
/// created DUPLICATE holders → duplicate throttles → the @ConcurrencyLimit cap
/// was exceeded (flaky `IllegalStateException` in ConcurrencyLimitTests). The
/// real wrapper's `computeIfAbsent` runs `synchronized (mutex) { m.compute… }`,
/// which CratonVM executes correctly. Null passes through.
pub fn native_synchronized_map(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    match args.first() {
        Some(v @ Value::Object(Some(_))) => ctx.new_object_initialized(
            "java/util/Collections$SynchronizedMap",
            "(Ljava/util/Map;)V",
            &[v.clone()],
        ),
        other => Ok(Some(other.cloned().unwrap_or(Value::Object(None)))),
    }
}

/// `Collections.synchronizedList(l)` — wrap in the real `SynchronizedList` (or
/// `SynchronizedRandomAccessList` when the backing list implements `RandomAccess`,
/// matching `Collections.synchronizedList`), not the raw list. Same rationale as
/// [`native_synchronized_map`]: the identity stub left mutation unsynchronized.
pub fn native_synchronized_list(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    match args.first() {
        Some(v @ Value::Object(Some(list))) => {
            let is_random_access = match ctx.class_id_by_name("java/util/RandomAccess") {
                Some(ra) => ctx.is_subclass(ctx.class_id_of_object(*list), ra),
                None => false,
            };
            let cls = if is_random_access {
                "java/util/Collections$SynchronizedRandomAccessList"
            } else {
                "java/util/Collections$SynchronizedList"
            };
            ctx.new_object_initialized(cls, "(Ljava/util/List;)V", &[v.clone()])
        }
        other => Ok(Some(other.cloned().unwrap_or(Value::Object(None)))),
    }
}

/// `Collections.synchronizedSet(s)` — wrap in the real `SynchronizedSet`.
pub fn native_synchronized_set(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    match args.first() {
        Some(v @ Value::Object(Some(_))) => ctx.new_object_initialized(
            "java/util/Collections$SynchronizedSet",
            "(Ljava/util/Set;)V",
            &[v.clone()],
        ),
        other => Ok(Some(other.cloned().unwrap_or(Value::Object(None)))),
    }
}

/// `Collections.synchronizedCollection(c)` — wrap in the real `SynchronizedCollection`.
pub fn native_synchronized_collection(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    match args.first() {
        Some(v @ Value::Object(Some(_))) => ctx.new_object_initialized(
            "java/util/Collections$SynchronizedCollection",
            "(Ljava/util/Collection;)V",
            &[v.clone()],
        ),
        other => Ok(Some(other.cloned().unwrap_or(Value::Object(None)))),
    }
}

/// Hold the wrapper's `mutex` across `body`, exactly as the JDK's
/// `Collections$Synchronized{Collection,Set,List,Map}` bytecode does
/// (`synchronized (mutex) { c.add(e); }`).
///
/// Every one of those methods was a bare forward to the backing collection --
/// the `mutex` field was written by the constructor and then read by nobody.
/// The wrapper's whole contract is the lock, so what
/// `Collections.synchronizedSet(new HashSet<>())` actually handed out was an
/// unsynchronized `HashSet` behind a class name that promises otherwise: eight
/// threads adding and then removing 4000 distinct elements each ended the run
/// at `size() == 3716` where HotSpot ends at `0` (`SyncSetProbe`, 2026-08-16;
/// the H2 `TestMultiThread` write-up saw the same shape as a NEGATIVE size).
/// A registered native shadows the class's own bytecode at every dispatch
/// site, so the real JDK implementation could not compensate -- including for
/// `SynchronizedList`, which has no natives of its own but inherits `add` /
/// `remove` / `size` from `SynchronizedCollection`, and so lost elements too
/// (26540 of 32000).
///
/// **Contended entry is GC-safe on purpose.** These wrappers are contended by
/// construction, and the owner is inside `HashMap.put`, which allocates and can
/// therefore be parked at a collection safepoint while it holds the mutex. A
/// plain `monitor_enter` leaves the waiter counted in the STW barrier's
/// `expected` set -- the three-way wedge `Monitor::block_enter`'s own doc names
/// (owner waits for GC, contender waits for owner, GC waits for contender). The
/// price of the GC-safe wait is that a moving collection CAN run inside it, so
/// `this`, the mutex and every reference argument are pinned across it and
/// re-read afterwards; nothing below may use a pre-wait `ObjectRef`.
pub fn with_sync_mutex<F>(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    args: &[Value],
    body: F,
) -> MethodCallResult
where
    F: FnOnce(&mut dyn NativeContext, ObjectRef, &[Value]) -> MethodCallResult,
{
    let mutex = match ctx.get_field_by_name(this, "mutex") {
        Value::Object(Some(m)) => m,
        // A wrapper that reached us without going through `native_sync_*_init`
        // has no `mutex` yet; the JDK's one-argument constructor uses `this`,
        // which is also the only lock that can be correct for such an object.
        _ => this,
    };
    let base = ctx.pin_native_root(this);
    let mutex_h = ctx.pin_native_root(mutex);
    let arg_h: Vec<Option<usize>> = args
        .iter()
        .map(|a| match a {
            Value::Object(Some(o)) => Some(ctx.pin_native_root(*o)),
            _ => None,
        })
        .collect();
    ctx.monitor_enter_gc_safe(mutex);
    // Post-wait addresses only, for every reference we still name.
    let mutex = ctx.read_native_pin(mutex_h, mutex);
    let this = ctx.read_native_pin(base, this);
    let mut fixed: Vec<Value> = Vec::with_capacity(args.len());
    for (a, h) in args.iter().zip(arg_h.iter()) {
        fixed.push(match (a, h) {
            (Value::Object(Some(o)), Some(h)) => Value::Object(Some(ctx.read_native_pin(*h, *o))),
            _ => *a,
        });
    }
    let out = body(ctx, this, &fixed);
    // `body` RE-ENTERS JAVA (`invoke_virtual` into `HashMap.get`,
    // `ArrayList.set`, ...), so a collection can run inside it and move the
    // mutex just as the contended entry above can. The refresh after
    // `monitor_enter_gc_safe` covers only the wait; the exit needs its own,
    // and it has to happen BEFORE `unpin_native_roots(base)` truncates the
    // stack that owns `mutex_h`.
    //
    // Without it `monitor_exit` dereferenced the PRE-BODY address:
    // `MonitorTable::exit` opens with `header_of(obj_ref)`, so a mutex whose
    // young slot the collection had reclaimed faulted there —
    // `EXCEPTION_ACCESS_VIOLATION` inside `native_sync_map_get` /
    // `sync_collection_delegate`, reproduced in seconds by
    // `probes/OldToYoungBarrierSweep.java` under
    // `--XX:UseGc Generational` with and without the JIT, and absent under
    // ZGC only because its wrapper objects had not been relocated yet.
    // The quiet face is worse than the loud one: a mutex that merely MOVED
    // exits a monitor at its old address, so the real one is never released
    // and every later waiter on that wrapper blocks forever.
    let mutex = ctx.read_native_pin(mutex_h, mutex);
    ctx.monitor_exit(mutex);
    // Unpin LAST: `monitor_exit` is the final use of a pinned reference, and
    // the previous order released the pins while one was still live.
    ctx.unpin_native_roots(base);
    out
}

pub fn native_sync_collection_init(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let backing = args.get(1).copied().unwrap_or(Value::Object(None));
    let mutex = match args.get(2).copied() {
        Some(Value::Object(Some(mutex))) => Value::Object(Some(mutex)),
        _ => Value::Object(Some(this)),
    };
    ctx.set_field_by_name(this, "c", backing);
    ctx.set_field(this, 0, backing);
    ctx.set_field_by_name(this, "mutex", mutex);
    Ok(None)
}

pub fn native_sync_collection_add(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let elem = args.get(1).copied().unwrap_or(Value::Object(None));
    with_sync_mutex(
        ctx,
        this,
        &[elem],
        |ctx, this, a| match sync_collection_backing(ctx, this) {
            Some(c) => ctx.invoke_virtual(c, "add", "(Ljava/lang/Object;)Z", &a[..1]),
            None => Ok(Some(Value::Int(0))),
        },
    )
}

pub fn native_sync_collection_contains(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let elem = args.get(1).copied().unwrap_or(Value::Object(None));
    with_sync_mutex(
        ctx,
        this,
        &[elem],
        |ctx, this, a| match sync_collection_backing(ctx, this) {
            Some(c) => ctx.invoke_virtual(c, "contains", "(Ljava/lang/Object;)Z", &a[..1]),
            None => Ok(Some(Value::Int(0))),
        },
    )
}

pub fn native_sync_collection_remove(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let elem = args.get(1).copied().unwrap_or(Value::Object(None));
    with_sync_mutex(
        ctx,
        this,
        &[elem],
        |ctx, this, a| match sync_collection_backing(ctx, this) {
            Some(c) => ctx.invoke_virtual(c, "remove", "(Ljava/lang/Object;)Z", &a[..1]),
            None => Ok(Some(Value::Int(0))),
        },
    )
}

pub fn native_sync_collection_size(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    with_sync_mutex(
        ctx,
        this,
        &[],
        |ctx, this, _a| match sync_collection_backing(ctx, this) {
            Some(c) => ctx.invoke_virtual(c, "size", "()I", &[]),
            None => Ok(Some(Value::Int(0))),
        },
    )
}

pub fn native_sync_collection_is_empty(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    with_sync_mutex(
        ctx,
        this,
        &[],
        |ctx, this, _a| match sync_collection_backing(ctx, this) {
            Some(c) => ctx.invoke_virtual(c, "isEmpty", "()Z", &[]),
            None => Ok(Some(Value::Int(1))),
        },
    )
}

/// NOT synchronized, deliberately: `SynchronizedCollection.iterator()` is the
/// one mutating-surface method the JDK forwards outside the lock, because the
/// returned iterator is used outside it too ("it is imperative that the user
/// manually synchronize on the returned collection when traversing"). Locking
/// here would diverge from the JDK without making traversal safe.
pub fn native_sync_collection_iterator(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    match sync_collection_backing(ctx, this) {
        Some(c) => ctx.invoke_virtual(c, "iterator", "()Ljava/util/Iterator;", &[]),
        None => Ok(Some(Value::Object(None))),
    }
}

pub fn native_sync_collection_to_array(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    with_sync_mutex(
        ctx,
        this,
        &[],
        |ctx, this, _a| match sync_collection_backing(ctx, this) {
            Some(c) => ctx.invoke_virtual(c, "toArray", "()[Ljava/lang/Object;", &[]),
            None => {
                let empty = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 0);
                Ok(Some(Value::Object(Some(empty))))
            }
        },
    )
}

/// Forward one `Collections$Synchronized{Collection,Set}` method to the wrapped
/// collection, unchanged.
///
/// The named-method stubs above cover the surface the wrapper needed while
/// `Collections.synchronizedCollection(…)` was its only source. Since
/// 2026-08-13 `Hashtable`/`Properties` views are wrapped too — the JDK's own
/// `Hashtable.keySet()` is `Collections.synchronizedSet(new KeySet(), this)`, so
/// matching `getClass()` means returning the wrapper — and those views are asked
/// for the whole `Collection` contract (`toString`, `stream`, `forEach`,
/// `containsAll`, …). In real-JDK mode the class's own bytecode answers all of
/// it and these are dropped as `SyntheticStub`s; in synthetic-JDK mode they are
/// the only implementation there is, and a MISSING one is worse than a slow one:
/// the call falls through to an interface-level native that reads the wrapper as
/// if it were the collection and reports it EMPTY.
///
/// `fallback` is what to answer when the wrapper has no `c` — a shape that
/// cannot arise from either constructor, so it is chosen per method only to keep
/// the return type honest rather than to encode behaviour.
/// Forward one `Collections$Synchronized{Collection,Set,List}` method to the
/// wrapped collection, under the wrapper's `mutex`.
///
/// The named-method stubs above cover the surface the wrapper needed while
/// `Collections.synchronizedCollection(...)` was its only source. Since
/// 2026-08-13 `Hashtable`/`Properties` views are wrapped too -- the JDK's own
/// `Hashtable.keySet()` is `Collections.synchronizedSet(new KeySet(), this)`, so
/// matching `getClass()` means returning the wrapper -- and those views are asked
/// for the whole `Collection` contract (`toString`, `stream`, `forEach`,
/// `containsAll`, ...). In real-JDK mode the class's own bytecode answers all of
/// it and these are dropped as `SyntheticStub`s; in synthetic-JDK mode they are
/// the only implementation there is, and a MISSING one is worse than a slow one:
/// the call falls through to an interface-level native that reads the wrapper as
/// if it were the collection and reports it EMPTY.
///
/// `fallback` is what to answer when the wrapper has no `c` -- a shape that
/// cannot arise from either constructor, so it is chosen per method only to keep
/// the return type honest rather than to encode behaviour.
///
/// `stream` / `parallelStream` / `spliterator` are forwarded WITHOUT the lock,
/// matching the JDK, for the same reason `iterator()` is.
pub fn sync_collection_delegate(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    method: &str,
    descriptor: &str,
    fallback: Option<Value>,
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    if matches!(
        method,
        "iterator" | "stream" | "parallelStream" | "spliterator"
    ) {
        return match sync_collection_backing(ctx, this) {
            Some(c) => ctx.invoke_virtual(c, method, descriptor, &args[1..]),
            None => Ok(fallback),
        };
    }
    let rest: Vec<Value> = args[1..].to_vec();
    with_sync_mutex(
        ctx,
        this,
        &rest,
        |ctx, this, a| match sync_collection_backing(ctx, this) {
            Some(c) => ctx.invoke_virtual(c, method, descriptor, a),
            None => Ok(fallback),
        },
    )
}

pub fn native_sync_map_init(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let backing = args.get(1).copied().unwrap_or(Value::Object(None));
    let mutex = match args.get(2).copied() {
        Some(Value::Object(Some(mutex))) => Value::Object(Some(mutex)),
        _ => Value::Object(Some(this)),
    };
    ctx.set_field_by_name(this, "m", backing);
    ctx.set_field(this, 0, backing);
    ctx.set_field_by_name(this, "mutex", mutex);
    Ok(None)
}

pub fn native_sync_map_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let key = args.get(1).copied().unwrap_or(Value::Object(None));
    with_sync_mutex(ctx, this, &[key], |ctx, this, a| {
        match sync_map_backing(ctx, this) {
            Some(m) => {
                ctx.invoke_virtual(m, "get", "(Ljava/lang/Object;)Ljava/lang/Object;", &a[..1])
            }
            None => Ok(Some(Value::Object(None))),
        }
    })
}

pub fn native_sync_map_put(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let key = args.get(1).copied().unwrap_or(Value::Object(None));
    let value = args.get(2).copied().unwrap_or(Value::Object(None));
    with_sync_mutex(
        ctx,
        this,
        &[key, value],
        |ctx, this, a| match sync_map_backing(ctx, this) {
            Some(m) => ctx.invoke_virtual(
                m,
                "put",
                "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
                &a[..2],
            ),
            None => Ok(Some(Value::Object(None))),
        },
    )
}

pub fn native_sync_map_contains_key(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let key = args.get(1).copied().unwrap_or(Value::Object(None));
    with_sync_mutex(ctx, this, &[key], |ctx, this, a| {
        match sync_map_backing(ctx, this) {
            Some(m) => ctx.invoke_virtual(m, "containsKey", "(Ljava/lang/Object;)Z", &a[..1]),
            None => Ok(Some(Value::Int(0))),
        }
    })
}

pub fn native_sync_map_remove(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let key = args.get(1).copied().unwrap_or(Value::Object(None));
    with_sync_mutex(ctx, this, &[key], |ctx, this, a| {
        match sync_map_backing(ctx, this) {
            Some(m) => ctx.invoke_virtual(
                m,
                "remove",
                "(Ljava/lang/Object;)Ljava/lang/Object;",
                &a[..1],
            ),
            None => Ok(Some(Value::Object(None))),
        }
    })
}

pub fn native_sync_map_size(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    with_sync_mutex(ctx, this, &[], |ctx, this, _a| {
        match sync_map_backing(ctx, this) {
            Some(m) => ctx.invoke_virtual(m, "size", "()I", &[]),
            None => Ok(Some(Value::Int(0))),
        }
    })
}

pub fn native_sync_map_is_empty(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    with_sync_mutex(ctx, this, &[], |ctx, this, _a| {
        match sync_map_backing(ctx, this) {
            Some(m) => ctx.invoke_virtual(m, "isEmpty", "()Z", &[]),
            None => Ok(Some(Value::Int(1))),
        }
    })
}

pub fn native_sync_map_entry_set(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    with_sync_mutex(ctx, this, &[], |ctx, this, _a| {
        match sync_map_backing(ctx, this) {
            Some(m) => ctx.invoke_virtual(m, "entrySet", "()Ljava/util/Set;", &[]),
            None => Ok(Some(Value::Object(None))),
        }
    })
}

pub fn native_sync_map_key_set(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    with_sync_mutex(ctx, this, &[], |ctx, this, _a| {
        match sync_map_backing(ctx, this) {
            Some(m) => ctx.invoke_virtual(m, "keySet", "()Ljava/util/Set;", &[]),
            None => Ok(Some(Value::Object(None))),
        }
    })
}

pub fn native_sync_map_values(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    with_sync_mutex(ctx, this, &[], |ctx, this, _a| {
        match sync_map_backing(ctx, this) {
            Some(m) => ctx.invoke_virtual(m, "values", "()Ljava/util/Collection;", &[]),
            None => Ok(Some(Value::Object(None))),
        }
    })
}

pub fn native_sync_map_clear(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    with_sync_mutex(ctx, this, &[], |ctx, this, _a| {
        if let Some(m) = sync_map_backing(ctx, this) {
            ctx.invoke_virtual(m, "clear", "()V", &[])?;
        }
        Ok(None)
    })
}

pub fn native_sync_map_compute_if_absent(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let key = args.get(1).copied().unwrap_or(Value::Object(None));
    let mapper_arg = args.get(2).copied().unwrap_or(Value::Object(None));
    // The get/apply/put trio is ONE critical section, exactly as the real
    // wrapper's `synchronized (mutex) { m.computeIfAbsent(...) }` is. Running it
    // unlocked is what let two threads both miss the `get` and both `put` --
    // the duplicate-holder race this method's own doc above describes.
    with_sync_mutex(ctx, this, &[key, mapper_arg], |ctx, this, a| {
        let key = a[0];
        let mapper = match a[1] {
            Value::Object(Some(f)) => Some(f),
            _ => None,
        };
        let Some(m) = sync_map_backing(ctx, this) else {
            return Ok(Some(Value::Object(None)));
        };
        if let Some(v @ Value::Object(Some(_))) =
            ctx.invoke_virtual(m, "get", "(Ljava/lang/Object;)Ljava/lang/Object;", &[key])?
        {
            return Ok(Some(v));
        }
        let Some(mapper) = mapper else {
            return Ok(Some(Value::Object(None)));
        };
        let computed = ctx
            .invoke_virtual(
                mapper,
                "apply",
                "(Ljava/lang/Object;)Ljava/lang/Object;",
                &[key],
            )?
            .unwrap_or(Value::Object(None));
        if matches!(computed, Value::Object(Some(_))) {
            let _ = ctx.invoke_virtual(
                m,
                "put",
                "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
                &[key, computed],
            );
        }
        Ok(Some(computed))    })}

pub fn native_iss_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    ctx.set_field(this, ISS_FIELD_COUNT, Value::Long(0));
    ctx.set_field(this, ISS_FIELD_SUM, Value::Long(0));
    ctx.set_field(this, ISS_FIELD_MIN, Value::Int(i32::MAX));
    ctx.set_field(this, ISS_FIELD_MAX, Value::Int(i32::MIN));
    Ok(None)
}

pub fn native_iss_accept(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let val = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let count = match ctx.get_field(this, ISS_FIELD_COUNT) {
        Value::Long(v) => v,
        _ => 0,
    };
    let sum = match ctx.get_field(this, ISS_FIELD_SUM) {
        Value::Long(v) => v,
        _ => 0,
    };
    let min = match ctx.get_field(this, ISS_FIELD_MIN) {
        Value::Int(v) => v,
        _ => i32::MAX,
    };
    let max = match ctx.get_field(this, ISS_FIELD_MAX) {
        Value::Int(v) => v,
        _ => i32::MIN,
    };
    ctx.set_field(this, ISS_FIELD_COUNT, Value::Long(count + 1));
    ctx.set_field(this, ISS_FIELD_SUM, Value::Long(sum + val as i64));
    ctx.set_field(this, ISS_FIELD_MIN, Value::Int(min.min(val)));
    ctx.set_field(this, ISS_FIELD_MAX, Value::Int(max.max(val)));
    Ok(None)
}

pub fn native_iss_get_count(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Long(0))),
    };
    match ctx.get_field(this, ISS_FIELD_COUNT) {
        Value::Long(v) => Ok(Some(Value::Long(v))),
        _ => Ok(Some(Value::Long(0))),
    }
}

pub fn native_iss_get_sum(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Long(0))),
    };
    match ctx.get_field(this, ISS_FIELD_SUM) {
        Value::Long(v) => Ok(Some(Value::Long(v))),
        _ => Ok(Some(Value::Long(0))),
    }
}

pub fn native_iss_get_min(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(i32::MAX))),
    };
    match ctx.get_field(this, ISS_FIELD_MIN) {
        Value::Int(v) => Ok(Some(Value::Int(v))),
        _ => Ok(Some(Value::Int(i32::MAX))),
    }
}

pub fn native_iss_get_max(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(i32::MIN))),
    };
    match ctx.get_field(this, ISS_FIELD_MAX) {
        Value::Int(v) => Ok(Some(Value::Int(v))),
        _ => Ok(Some(Value::Int(i32::MIN))),
    }
}

pub fn native_iss_get_average(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Double(0.0))),
    };
    let count = match ctx.get_field(this, ISS_FIELD_COUNT) {
        Value::Long(v) => v,
        _ => 0,
    };
    let sum = match ctx.get_field(this, ISS_FIELD_SUM) {
        Value::Long(v) => v,
        _ => 0,
    };
    if count == 0 {
        Ok(Some(Value::Double(0.0)))
    } else {
        Ok(Some(Value::Double(sum as f64 / count as f64)))
    }
}

pub fn native_iss_to_string(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let count = match ctx.get_field(this, ISS_FIELD_COUNT) {
        Value::Long(v) => v,
        _ => 0,
    };
    let sum = match ctx.get_field(this, ISS_FIELD_SUM) {
        Value::Long(v) => v,
        _ => 0,
    };
    let min = match ctx.get_field(this, ISS_FIELD_MIN) {
        Value::Int(v) => v,
        _ => 0,
    };
    let max = match ctx.get_field(this, ISS_FIELD_MAX) {
        Value::Int(v) => v,
        _ => 0,
    };
    let avg = if count == 0 {
        0.0
    } else {
        sum as f64 / count as f64
    };
    let text = format!(
        "IntSummaryStatistics{{count={}, sum={}, min={}, average={:.6}, max={}}}",
        count, sum, min, avg, max
    );
    Ok(Some(Value::Object(Some(ctx.create_string(&text)))))
}

pub fn register_atomic_boolean_natives(r: &mut NativeMethodRegistry) {
    // census-tag: AtomicBoolean atomic primitive (Unsafe-equivalent) → Bridge.
            let c = "java/util/concurrent/atomic/AtomicBoolean";
    r.register(c, "<init>", "()V", native_ab_init_default, cratonvm_native_api::NativeKind::Bridge);
    r.register(c, "<init>", "(Z)V", native_ab_init_value, cratonvm_native_api::NativeKind::Bridge);
    r.register(c, "get", "()Z", native_ab_get, cratonvm_native_api::NativeKind::Bridge);
    r.register(c, "set", "(Z)V", native_ab_set, cratonvm_native_api::NativeKind::Bridge);
    r.register(c, "lazySet", "(Z)V", native_ab_set, cratonvm_native_api::NativeKind::Bridge);
    r.register(c, "compareAndSet", "(ZZ)Z", native_ab_cas, cratonvm_native_api::NativeKind::Bridge);
    r.register(c, "getAndSet", "(Z)Z", native_ab_get_and_set, cratonvm_native_api::NativeKind::Bridge);
    r.register(c, "toString", "()Ljava/lang/String;", native_ab_to_string, cratonvm_native_api::NativeKind::Bridge);
    }

pub fn native_ab_init_default(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    ctx.set_field(this, AB_FIELD_VALUE, Value::Int(0));
    Ok(None)
}

pub fn native_ab_init_value(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let val = match args.get(1) {
        Some(Value::Int(v)) => {
            if *v != 0 {
                1            } else {
                0            }        }
        _ => 0,
    };
    ctx.set_field(this, AB_FIELD_VALUE, Value::Int(val));
    Ok(None)
}

pub fn native_ab_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let v = match ctx.get_field(this, AB_FIELD_VALUE) {
        Value::Int(i) => i,
        _ => 0,
    };
    Ok(Some(Value::Int(if v != 0 { 1 } else { 0 })))}

pub fn native_ab_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let val = match args.get(1) {
        Some(Value::Int(v)) => {
            if *v != 0 {
                1            } else {
                0            }        }
        _ => 0,
    };
    ctx.set_field(this, AB_FIELD_VALUE, Value::Int(val));
    Ok(None)
}

pub fn native_ab_cas(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let expected = match args.get(1) {
        Some(Value::Int(v)) => {
            if *v != 0 {
                1            } else {
                0            }        }
        _ => 0,
    };
    let update = match args.get(2) {
        Some(Value::Int(v)) => {
            if *v != 0 {
                1            } else {
                0            }        }
        _ => 0,
    };
    let current = match ctx.get_field(this, AB_FIELD_VALUE) {
        Value::Int(i) => i,
        _ => 0,
    };
    if current == expected {
        ctx.set_field(this, AB_FIELD_VALUE, Value::Int(update));
        Ok(Some(Value::Int(1)))
    } else {
        Ok(Some(Value::Int(0)))
    }
}

pub fn native_ab_get_and_set(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let new_val = match args.get(1) {
        Some(Value::Int(v)) => {
            if *v != 0 {
                1            } else {
                0            }        }
        _ => 0,
    };
    let old = match ctx.get_field(this, AB_FIELD_VALUE) {
        Value::Int(i) => i,
        _ => 0,
    };
    ctx.set_field(this, AB_FIELD_VALUE, Value::Int(new_val));
    Ok(Some(Value::Int(if old != 0 { 1 } else { 0 })))
}

pub fn native_ab_to_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let v = match ctx.get_field(this, AB_FIELD_VALUE) {
        Value::Int(i) => i != 0,
        _ => false,
    };
    let s = ctx.create_string(if v { "true" } else { "false" });
    Ok(Some(Value::Object(Some(s))))
}

pub fn register_atomic_stamped_ref_natives(r: &mut NativeMethodRegistry) {
    // census-tag: AtomicStampedReference atomic primitive → Bridge.
    let c = "java/util/concurrent/atomic/AtomicStampedReference";
    r.register(c, "<init>", "(Ljava/lang/Object;I)V", native_asr_init, cratonvm_native_api::NativeKind::Bridge);
    r.register(
        c,
        "getReference",
        "()Ljava/lang/Object;",
        native_asr_get_ref, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(c, "getStamp", "()I", native_asr_get_stamp, cratonvm_native_api::NativeKind::Bridge);
    // get([I)Ljava/lang/Object; — reads stamp into stampHolder[0] and
    // returns the reference. Faithful to the JDK two-result accessor.
    r.register(
        c,
        "get",
        "([I)Ljava/lang/Object;",
        native_asr_get_with_holder, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(c, "set", "(Ljava/lang/Object;I)V", native_asr_set, cratonvm_native_api::NativeKind::Bridge);
    r.register(
        c,
        "attemptStamp",
        "(Ljava/lang/Object;I)Z",
        native_asr_attempt_stamp, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "compareAndSet",
        "(Ljava/lang/Object;Ljava/lang/Object;II)Z",
        native_asr_cas, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "weakCompareAndSet",
        "(Ljava/lang/Object;Ljava/lang/Object;II)Z",
        native_asr_cas, cratonvm_native_api::NativeKind::Bridge,
    );
}

pub fn native_asr_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let r = args.get(1).copied().unwrap_or(Value::Object(None));
    let stamp = match args.get(2) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    // Store a real Pair object in the single `pair` field (slot 0). No write to
    // slot 1 — the ASR object itself has only one slot.
    asr_install_pair(ctx, this, r, stamp)?;
    Ok(None)
}

pub fn native_asr_get_ref(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let (reference, _stamp) = asr_read_pair(ctx, this);
    Ok(Some(reference))
}

pub fn native_asr_get_stamp(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let (_reference, stamp) = asr_read_pair(ctx, this);
    Ok(Some(Value::Int(stamp)))
}

/// `V get(int[] stampHolder)` — store the current stamp into `stampHolder[0]`
/// and return the current reference (matches JDK semantics).
pub fn native_asr_get_with_holder(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let (reference, stamp) = asr_read_pair(ctx, this);
    if let Some(Value::Object(Some(holder))) = args.get(1) {
        ctx.set_array_element(*holder, 0, Value::Int(stamp));
    }
    Ok(Some(reference))
}

pub fn native_asr_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let r = args.get(1).copied().unwrap_or(Value::Object(None));
    let stamp = match args.get(2) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    // JDK `set` allocates a fresh Pair only when reference or stamp differ; we
    // always install a fresh Pair (semantically identical, slightly simpler).
    asr_install_pair(ctx, this, r, stamp)?;
    Ok(None)
}

pub fn native_asr_attempt_stamp(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let expected_ref = args.get(1).copied().unwrap_or(Value::Object(None));
    let new_stamp = match args.get(2) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let (current_ref, _current_stamp) = asr_read_pair(ctx, this);
    if values_ref_equal(current_ref, expected_ref) {
        // Swap in a new Pair carrying the (unchanged) reference + new stamp.
        asr_install_pair(ctx, this, current_ref, new_stamp)?;
        Ok(Some(Value::Int(1)))
    } else {
        Ok(Some(Value::Int(0)))
    }
}

pub fn native_asr_cas(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let expected_ref = args.get(1).copied().unwrap_or(Value::Object(None));
    let new_ref = args.get(2).copied().unwrap_or(Value::Object(None));
    let expected_stamp = match args.get(3) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let new_stamp = match args.get(4) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let (current_ref, current_stamp) = asr_read_pair(ctx, this);
    if values_ref_equal(current_ref, expected_ref) && current_stamp == expected_stamp {
        // Install a new Pair only if reference or stamp actually changes (the
        // JDK fast-path: when both are identical it skips the casPair entirely).
        if !values_ref_equal(current_ref, new_ref) || current_stamp != new_stamp {
            asr_install_pair(ctx, this, new_ref, new_stamp)?;        }
        Ok(Some(Value::Int(1)))
    } else {
        Ok(Some(Value::Int(0)))
    }
}

pub fn register_atomic_markable_ref_natives(r: &mut NativeMethodRegistry) {
    // census-tag: AtomicMarkableReference atomic primitive → Bridge.
            let c = "java/util/concurrent/atomic/AtomicMarkableReference";
    r.register(c, "<init>", "(Ljava/lang/Object;Z)V", native_amr_init, cratonvm_native_api::NativeKind::Bridge);
    r.register(
        c,
        "getReference",
        "()Ljava/lang/Object;",
        native_amr_get_ref, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(c, "isMarked", "()Z", native_amr_is_marked, cratonvm_native_api::NativeKind::Bridge);
    // get(boolean[]) — reads mark into markHolder[0] and returns the
    // reference. Must be registered explicitly (see comment above): without
    // it, real bytecode ran unintercepted against a corrupted slot 0.
    r.register(
        c,
        "get",
        "([Z)Ljava/lang/Object;",
        native_amr_get_with_holder, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(c, "set", "(Ljava/lang/Object;Z)V", native_amr_set, cratonvm_native_api::NativeKind::Bridge);
    r.register(
        c,
        "attemptMark",
        "(Ljava/lang/Object;Z)Z",
        native_amr_attempt_mark, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "compareAndSet",
        "(Ljava/lang/Object;Ljava/lang/Object;ZZ)Z",
        native_amr_cas, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "weakCompareAndSet",
        "(Ljava/lang/Object;Ljava/lang/Object;ZZ)Z",
        native_amr_cas, cratonvm_native_api::NativeKind::Bridge,
    );
    }

/// RETIRED 2026-08-12 (W7-18 patch B). Registers nothing, on purpose, and the
/// function is kept only because its call site is in another lane's file
/// (`lib.rs`'s `register_phase_d_natives`).
///
/// This was the *third* registrar of `java/util/concurrent/StructuredTaskScope`,
/// and every triple it held was either dead or a landmine:
///
/// * **Provably inert.** All three registrars sit inside
///   `register_synthetic_overrides`, and the call order there is
///   `register_phase67_natives` (`phases_late/concurrent.rs`), then
///   `register_phase_d_natives` (this one), then
///   `register_jdk25_concurrency_natives`. `register()` is
///   last-registration-wins (docs/architecture/natives-over-real-jdk-classes.md
///   §3), and `jdk25_concurrency.rs` registers a superset of this function's
///   `StructuredTaskScope` and `$Subtask` triples — so every one of them was
///   overwritten before boot finished. The only registrations here that ever
///   *won* were the two covariant-return `join()`s,
///   `$ShutdownOnFailure.join()L…$ShutdownOnFailure;` and
///   `$ShutdownOnSuccess.join()L…$ShutdownOnSuccess;`, which `jdk25_concurrency`
///   spelled with the base `L…StructuredTaskScope;` return and therefore did not
///   collide with. Both are on classes **JEP 505 deleted**: `javap` and
///   `Class.forName` answer "not found" on Adoptium 25.0.3.9, `--fast-boot` and
///   `--real-jdk` alike (docs/feature-designs/native-dispatch-architecture.md §3).
///
/// * **A landmine, which is why it is retired rather than left alone.** These
///   bodies used a DIFFERENT `$Subtask` slot convention from the registrar that
///   owns the readers. Here, `state` was read and written at **slot 3** (`1` =
///   success, `2` = failed) and the result at slot 1. In
///   `jdk25_concurrency.rs`, which wins `Subtask.get/state/exception`, the
///   layout is `SUBTASK_FIELD_STATE = 0`, `RESULT = 1`, `EXCEPTION = 2`,
///   **`CALLABLE = 3`** — so this file's `state` write landed on the callable
///   REFERENCE slot: an `Int` in a slot the collector scans as an oop, which is
///   heap corruption rather than a wrong answer
///   (docs/architecture/natives-over-real-jdk-classes.md §5). It never fired
///   only because every one of those triples was overwritten. The day anyone
///   deletes a JDK-21-shaped triple from the winning registrar, this becomes
///   live. `t3_impl.rs::register_t31_structured_concurrency` is already a
///   tombstone for exactly this defect ("registering them here caused the
///   canonical 8-field layout to be overridden with the earlier 2-field stubs,
///   silently breaking `close()`, `result()`, and `throwIfFailed()`"); this is
///   the copy that pass missed.
///
/// Scope of the change: **synthetic-JDK mode only**, and structurally so. All
/// three registrars are reachable only from `register_synthetic_overrides`,
/// which is `#[cfg(feature = "synthetic-jdk")]` and called only on the
/// `use_synthetic_jdk` arm of `vm_init`. In `--fast-boot` and `--real-jdk` real
/// JDK bytecode serves this API end to end, measured at
/// `compatibility_classes: 0` / `synthetic_stub_invocations: 0` with zero
/// StructuredTaskScope violations (W7-18 §5), so nothing here can move either
/// shipping mode by any amount, and nothing here can move a ratchet taken in
/// Compatible mode.
pub fn register_pd_structured_concurrency(_r: &mut NativeMethodRegistry) {}

/// The legacy synthetic `ReentrantLock` / `Lock` / `Condition` / `Semaphore`
/// natives, split out of [`register_concurrent_natives`] so they have a name.
///
/// Production behaviour is unchanged: `register_concurrent_natives` calls this
/// under exactly the `!real_aqs` condition the two inline blocks used to test,
/// so the default real-AQS build still registers none of them.
///
/// It is public because otherwise this code is untestable. Real AQS became the
/// default, which is right for the VM, but it left the synthetic implementation
/// shipped and unreachable from any test: `cratonvm-vm`'s inline suite builds
/// hand-rolled receivers with no real `java.util.concurrent` bytecode behind
/// them, so it cannot exercise the real path, and the synthetic path was no
/// longer registered for it to exercise either. Its tests were left asserting
/// natives that the default build deliberately omits, and simply failed. A
/// test registry can now opt in the same way `CRATONVM_SYNTHETIC_AQS=1` does
/// at runtime.
///
/// **Kind: `SyntheticStub`, stated here** (interpreter round i3 wave 79, lane
/// L1; `i77-L1-synthetic-aqs-natives-take-the-ambient-kind`). It used to
/// register with whatever `current_category` its caller left, so the kind of
/// `ReentrantLock.lock()V` & co. was a property of the call chain (and it
/// overwrote, last-write-wins, `lib.rs`'s explicitly `SyntheticStub` copy of
/// the same triples). Every caller reached it unscoped, so the effective kind
/// was already the registry's unscoped default, `SyntheticStub`; stating it
/// changes no kind, only who decides it. Over a real JDK (`CRATONVM_SYNTHETIC_AQS=1`)
/// these are stand-ins in front of AQS bytecode, which is what
/// `SyntheticStub` means. On the synthetic library they are the only
/// implementation, and `CRATONVM_NO_STUBS` drops them there exactly as it did
/// before this change.
pub fn register_synthetic_aqs_natives(registry: &mut NativeMethodRegistry) {
    register_synthetic_aqs_natives_body(registry);
}

/// The registrations of [`register_synthetic_aqs_natives`], run inside its
/// `SyntheticStub` scope. Registrations only: no nested registrar, so the
/// scope covers exactly these rows.
pub fn register_synthetic_aqs_natives_body(registry: &mut NativeMethodRegistry) {
    // --- ReentrantLock ---
    let rl = "java/util/concurrent/locks/ReentrantLock";
    registry.register(rl, "<init>", "()V", native_rl_init, NativeKind::SyntheticStub);
    registry.register(rl, "<init>", "(Z)V", native_rl_init_fair, NativeKind::SyntheticStub);
    registry.register(rl, "lock", "()V", native_rl_lock, NativeKind::SyntheticStub);
    registry.register(rl, "lockInterruptibly", "()V", native_rl_lock, NativeKind::SyntheticStub); // simplified
    registry.register(rl, "unlock", "()V", native_rl_unlock, NativeKind::SyntheticStub);
    registry.register(rl, "tryLock", "()Z", native_rl_try_lock, NativeKind::SyntheticStub);
    registry.register(
        rl,
        "tryLock",
        "(JLjava/util/concurrent/TimeUnit;)Z",
        native_rl_try_lock_timeout, NativeKind::SyntheticStub,
    );
    registry.register(rl, "isLocked", "()Z", native_rl_is_locked, NativeKind::SyntheticStub);
    registry.register(
        rl,
        "isHeldByCurrentThread",
        "()Z",
        native_rl_is_held_by_current_thread, NativeKind::SyntheticStub,
    );
    registry.register(rl, "getHoldCount", "()I", native_rl_get_hold_count, NativeKind::SyntheticStub);
    registry.register(rl, "isFair", "()Z", native_rl_is_fair, NativeKind::SyntheticStub);
    registry.register(
        rl,
        "newCondition",
        "()Ljava/util/concurrent/locks/Condition;",
        native_rl_new_condition, NativeKind::SyntheticStub,
    );
    registry.register(rl, "toString", "()Ljava/lang/String;", native_rl_to_string, NativeKind::SyntheticStub);

    // Also register under Lock interface
    let lock = "java/util/concurrent/locks/Lock";
    registry.register(lock, "lock", "()V", native_rl_lock, NativeKind::SyntheticStub);
    registry.register(lock, "unlock", "()V", native_rl_unlock, NativeKind::SyntheticStub);
    registry.register(lock, "tryLock", "()Z", native_rl_try_lock, NativeKind::SyntheticStub);
    registry.register(
        lock,
        "newCondition",
        "()Ljava/util/concurrent/locks/Condition;",
        native_rl_new_condition, NativeKind::SyntheticStub,
    );

    // --- Condition ---
    let cond = "java/util/concurrent/locks/Condition";
    registry.register(cond, "await", "()V", native_cond_await, NativeKind::SyntheticStub);
    registry.register(cond, "awaitUninterruptibly", "()V", native_cond_await, NativeKind::SyntheticStub);
    registry.register(
        cond,
        "await",
        "(JLjava/util/concurrent/TimeUnit;)Z",
        native_cond_await_timeout, NativeKind::SyntheticStub,
    );
    registry.register(cond, "awaitNanos", "(J)J", native_cond_await_nanos, NativeKind::SyntheticStub);
    registry.register(
        cond,
        "awaitUntil",
        "(Ljava/util/Date;)Z",
        native_cond_await_until, NativeKind::SyntheticStub,
    );
    registry.register(cond, "signal", "()V", native_cond_signal, NativeKind::SyntheticStub);
    registry.register(cond, "signalAll", "()V", native_cond_signal_all, NativeKind::SyntheticStub);

    let sem = "java/util/concurrent/Semaphore";
    registry.register(sem, "<init>", "(I)V", native_sem_init, NativeKind::SyntheticStub);
    registry.register(sem, "<init>", "(IZ)V", native_sem_init_fair, NativeKind::SyntheticStub);
    registry.register(sem, "acquire", "()V", native_sem_acquire, NativeKind::SyntheticStub);
    registry.register(sem, "acquire", "(I)V", native_sem_acquire_n, NativeKind::SyntheticStub);
    registry.register(sem, "acquireUninterruptibly", "()V", native_sem_acquire, NativeKind::SyntheticStub);
    // HikariCP's SuspendResumeLock may acquire all 10,000 permits at
    // once; retain this synthetic-mode overload.
    registry.register(sem, "acquireUninterruptibly", "(I)V", native_sem_acquire_n, NativeKind::SyntheticStub);
    registry.register(sem, "release", "()V", native_sem_release, NativeKind::SyntheticStub);
    registry.register(sem, "release", "(I)V", native_sem_release_n, NativeKind::SyntheticStub);
    registry.register(sem, "tryAcquire", "()Z", native_sem_try_acquire, NativeKind::SyntheticStub);
    registry.register(sem, "tryAcquire", "(I)Z", native_sem_try_acquire_n, NativeKind::SyntheticStub);
    registry.register(
        sem,
        "tryAcquire",
        "(JLjava/util/concurrent/TimeUnit;)Z",
        native_sem_try_acquire_timeout, NativeKind::SyntheticStub,
    );
    registry.register(sem, "availablePermits", "()I", native_sem_available_permits, NativeKind::SyntheticStub);
    registry.register(sem, "drainPermits", "()I", native_sem_drain_permits, NativeKind::SyntheticStub);
    registry.register(sem, "isFair", "()Z", native_sem_is_fair, NativeKind::SyntheticStub);
    registry.register(
        sem,
        "toString",
        "()Ljava/lang/String;",
        native_sem_to_string, NativeKind::SyntheticStub,
    );

    // --- ReentrantReadWriteLock ---
    //
    // Never had natives at all: `ClassManager` declares the layout
    // (`ReentrantReadWriteLock` = 3 fields, `$ReadLock`/`$WriteLock` = 1)
    // but nothing implemented it, so synthetic mode answered
    // `NoSuchMethodError: ReentrantReadWriteLock.readLock()` — the last of
    // the `JucComplete` lock failures.
    let rwl = RWL_CLASS;
    registry.register(rwl, "<init>", "()V", native_rwl_init, NativeKind::SyntheticStub);
    registry.register(rwl, "<init>", "(Z)V", native_rwl_init_fair, NativeKind::SyntheticStub);
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
    registry.register(rwl, "isFair", "()Z", native_rwl_is_fair, NativeKind::SyntheticStub);
    registry.register(rwl, "getReadLockCount", "()I", native_rwl_read_count, NativeKind::SyntheticStub);
    registry.register(rwl, "isWriteLocked", "()Z", native_rwl_is_write_locked, NativeKind::SyntheticStub);
    registry.register(rwl, "getWriteHoldCount", "()I", native_rwl_write_hold_count, NativeKind::SyntheticStub);
    for view in [RWL_READ_VIEW, RWL_WRITE_VIEW] {
        registry.register(view, "lock", "()V", native_rwl_view_lock, NativeKind::SyntheticStub);
        registry.register(view, "lockInterruptibly", "()V", native_rwl_view_lock, NativeKind::SyntheticStub);
        registry.register(view, "unlock", "()V", native_rwl_view_unlock, NativeKind::SyntheticStub);
        registry.register(view, "tryLock", "()Z", native_rwl_view_try_lock, NativeKind::SyntheticStub);
    }
}

// ---------------------------------------------------------------------------
// ReentrantReadWriteLock — a reader/writer lock over the same monitor-parking
// scheme the `ReentrantLock` natives above use.
//
// State lives in a side table keyed by the lock's weak lock key (`RlKey`,
// the identity hash until gc-common w29-b), not in the
// object's fields, for the reason spelled out on `rl_state_table`: the real
// class's slots hold `Sync` references and writing scalars there does not
// round-trip. The two *view* objects (`$ReadLock` / `$WriteLock`) ARE cached
// in the owner's slots 0 and 1 — `rwl.readLock()` must return the same
// instance every time, as HotSpot's final fields do — with slot 2 the `fair`
// flag, matching `ClassManager`'s declared 3-field layout.
//
// Semantics implemented: multiple concurrent readers, one writer excluding all
// readers, reentrant write holds, and write-while-holding-read refused (real
// AQS throws no error but deadlocks; refusing is the honest approximation and
// no fixture depends on it). Lock downgrading (write → read) works because a
// writer may always take a read lock.
// ---------------------------------------------------------------------------

pub const RWL_CLASS: &str = "java/util/concurrent/locks/ReentrantReadWriteLock";
pub const RWL_READ_VIEW: &str = "java/util/concurrent/locks/ReentrantReadWriteLock$ReadLock";
pub const RWL_WRITE_VIEW: &str = "java/util/concurrent/locks/ReentrantReadWriteLock$WriteLock";

/// Slot 0/1 cache the `$ReadLock` / `$WriteLock` views; slot 2 is `fair`.
pub const RWL_FIELD_READ_VIEW: usize = 0;
pub const RWL_FIELD_WRITE_VIEW: usize = 1;
pub const RWL_FIELD_FAIR: usize = 2;
/// A view's single slot points back at the owning `ReentrantReadWriteLock`.
pub const RWL_VIEW_FIELD_OWNER: usize = 0;

#[derive(Clone, Copy, Default)]
pub struct RwlState {
    /// Total read holds across all threads.
    pub readers: i32,
    /// Owning thread of the write lock, or `RL_UNOWNED`.
    pub writer: i64,
    /// Reentrant write hold count.
    pub write_hold: i32,
}

pub fn rwl_state_table() -> &'static std::sync::Mutex<std::collections::HashMap<RlKey, RwlState>> {
    static T: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<RlKey, RwlState>>> =
        std::sync::OnceLock::new();
    T.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// Drop every read/write-lock row belonging to `vm_identity`; the companion of
/// `forget_vm_lock_state`, which calls it.
pub fn forget_vm_rwl_state(vm_identity: usize) {
    rwl_state_table()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|(vm, _), _| *vm != vm_identity);
}

/// Drop the read/write-lock rows filed under `keys`, weak lock keys the
/// registry has just freed (the lock died, or its VM is going); the companion
/// of `lib.rs::forget_rl_state_keys`, which calls it. gc-common w29-b: the
/// rows were keyed by the lock's identity hash and stayed until VM teardown,
/// so a lock dropped while held left a held row for the next lock with its
/// hash. The table lock is a leaf.
pub fn forget_rwl_state_keys(keys: &[usize]) {
    if keys.is_empty() {
        return;
    }
    let mut table = rwl_state_table().lock().unwrap_or_else(|e| e.into_inner());
    if table.is_empty() {
        return;
    }
    let freed: rustc_hash::FxHashSet<usize> = keys.iter().copied().collect();
    table.retain(|&(_, key), _| !freed.contains(&key));
}

pub fn rwl_with<R>(key: RlKey, f: impl FnOnce(&mut RwlState) -> R) -> R {
    let mut table = rwl_state_table().lock().unwrap_or_else(|e| e.into_inner());
    f(table.entry(key).or_default())
}

pub fn rwl_get(key: RlKey) -> RwlState {
    rwl_state_table()
        .lock()
        .ok()
        .and_then(|t| t.get(&key).copied())
        .unwrap_or_default()
}

pub fn native_rwl_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    rwl_init_common(ctx, args, false)
}

pub fn native_rwl_init_fair(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let fair = matches!(args.get(1), Some(Value::Int(v)) if *v != 0);
    rwl_init_common(ctx, args, fair)
}

pub fn rwl_init_common(ctx: &mut dyn NativeContext, args: &[Value], fair: bool) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let key = rl_key(ctx, this);
    rwl_with(key, |st| *st = RwlState::default());
    if ctx.object_num_fields(this) > RWL_FIELD_FAIR {
        ctx.set_field(this, RWL_FIELD_READ_VIEW, Value::Object(None));
        ctx.set_field(this, RWL_FIELD_WRITE_VIEW, Value::Object(None));
        ctx.set_field(this, RWL_FIELD_FAIR, Value::Int(i32::from(fair)));
    }
    Ok(None)
}

/// Return (creating on first call) the cached view object for `slot`.
pub fn rwl_view(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    slot: usize,
    class_name: &str,
) -> MethodCallResult {
    if ctx.object_num_fields(this) > slot {
        if let Value::Object(Some(existing)) = ctx.get_field(this, slot) {
            return Ok(Some(Value::Object(Some(existing))));
        }
    }
    // Allocate first, publish afterwards: `alloc_concurrent_synthetic` can
    // trigger a collection that moves `this`, and `this` is pinned as a native
    // ARG — the pin is remapped, this Rust local is not. Re-read it through a
    // pin around the allocation.
    let this_pin = ctx.pin_native_root(this);
    let view = try_alloc_concurrent_synthetic(ctx, class_name, 1)?;
    let view_pin = ctx.pin_native_root(view);
    let this = ctx.read_native_pin(this_pin, this);
    let view = ctx.read_native_pin(view_pin, view);
    ctx.set_field(view, RWL_VIEW_FIELD_OWNER, Value::Object(Some(this)));
    if ctx.object_num_fields(this) > slot {
        ctx.set_field(this, slot, Value::Object(Some(view)));
    }
    ctx.unpin_native_roots(this_pin);
    Ok(Some(Value::Object(Some(view))))
}

pub fn native_rwl_read_lock(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    rwl_view(ctx, this, RWL_FIELD_READ_VIEW, RWL_READ_VIEW)}

pub fn native_rwl_write_lock(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    rwl_view(ctx, this, RWL_FIELD_WRITE_VIEW, RWL_WRITE_VIEW)}

pub fn native_rwl_is_fair(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    if ctx.object_num_fields(this) > RWL_FIELD_FAIR {
        if let Value::Int(v) = ctx.get_field(this, RWL_FIELD_FAIR) {
            return Ok(Some(Value::Int(if v != 0 { 1 } else { 0 })));
        }
    }
    Ok(Some(Value::Int(0)))
}

pub fn native_rwl_read_count(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let key = rl_key(ctx, this);
    Ok(Some(Value::Int(rwl_get(key).readers)))
}

pub fn native_rwl_is_write_locked(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let key = rl_key(ctx, this);
    Ok(Some(Value::Int(i32::from(rwl_get(key).write_hold > 0))))
}

pub fn native_rwl_write_hold_count(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let tid = ctx.thread_id() as i64;
    let key = rl_key(ctx, this);
    let st = rwl_get(key);
    Ok(Some(Value::Int(if st.writer == tid {
        st.write_hold
    } else {
        0
    })))
}

/// Resolve a `$ReadLock`/`$WriteLock` view to `(owner, is_write)`.
pub fn rwl_view_target(ctx: &mut dyn NativeContext, view: ObjectRef) -> Option<(ObjectRef, bool)> {
    let owner = match ctx.get_field(view, RWL_VIEW_FIELD_OWNER) {
        Value::Object(Some(o)) => o,
        _ => return None,
    };
    let is_write = ctx
        .class_name_of_id(ctx.class_id_of_object(view))
        .is_some_and(|name| name == RWL_WRITE_VIEW);
    Some((owner, is_write))
}

/// Try to take the lock once. `None` means "would block".
pub fn rwl_try_acquire(key: RlKey, tid: i64, is_write: bool) -> bool {
    rwl_with(key, |st| {
        if is_write {
            let free = st.writer == RL_UNOWNED && st.readers == 0;
            let reentrant = st.writer == tid;
            if free || reentrant {
                st.writer = tid;
                st.write_hold += 1;
                return true;
            }
            false
        } else {
            // A writer excludes readers, except itself (lock downgrading).
            if st.writer == RL_UNOWNED || st.writer == tid {
                st.readers += 1;
                return true;
            }
            false
        }
    })
}

pub fn native_rwl_view_lock(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let view = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let Some((mut owner, is_write)) = rwl_view_target(ctx, view) else {
        return Ok(None);
    };
    let tid = ctx.thread_id() as i64;
    let key = rl_key(ctx, owner);
    loop {
        if rwl_try_acquire(key, tid, is_write) {
            return Ok(None);
        }
        // Park on the owner lock's monitor until a releaser notifies. The 5ms
        // timeout re-checks defensively, exactly as `native_rl_lock` does.
        ctx.monitor_enter(owner);
        let still_blocked = {
            let st = rwl_get(key);
            if is_write {
                (st.writer != RL_UNOWNED && st.writer != tid) || st.readers > 0
            } else {
                st.writer != RL_UNOWNED && st.writer != tid
            }
        };
        if still_blocked {
            if let Err(e) = monitor_wait_release(ctx, &mut owner, Some(5)) {
                if is_interrupted_exception(&e) {
                    let self_thread = ctx.current_thread_object();
                    ctx.thread_interrupt(self_thread);
                    continue;
                }
                return Err(e);
            }
        } else {
            ctx.monitor_exit(owner);
        }
    }
}

pub fn native_rwl_view_try_lock(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let view = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let Some((owner, is_write)) = rwl_view_target(ctx, view) else {
        return Ok(Some(Value::Int(0)));
    };
    let tid = ctx.thread_id() as i64;
    let key = rl_key(ctx, owner);
    Ok(Some(Value::Int(i32::from(rwl_try_acquire(
        key, tid, is_write,
    )))))
}

pub fn native_rwl_view_unlock(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let view = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let Some((owner, is_write)) = rwl_view_target(ctx, view) else {
        return Ok(None);
    };
    let tid = ctx.thread_id() as i64;
    let key = rl_key(ctx, owner);
    let released = rwl_with(key, |st| {
        if is_write {
            if st.writer != tid {
                return false;
            }
            st.write_hold -= 1;
            if st.write_hold <= 0 {
                st.write_hold = 0;
                st.writer = RL_UNOWNED;
                return true;
            }
            false
        } else {
            if st.readers > 0 {
                st.readers -= 1;
            }
            st.readers == 0
        }
    });
    if released {
        // Wake anyone parked in `native_rwl_view_lock`.
        ctx.monitor_enter(owner);
        let _ = ctx.monitor_notify_all(owner);
        ctx.monitor_exit(owner);
    }
    Ok(None)
}

