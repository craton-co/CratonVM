//! System, Runtime, ProcessBuilder, and Thread native method implementations.

use rustjvm_native_api::{NativeContext, NativeMethodRegistry};
use rustjvm_types::{ObjectRef, Value};
use rustjvm_types::error::{MethodCallResult, RuntimeError};

use crate::{alloc_concurrent_synthetic, obj_arg, platform_lib_name};

// ---------------------------------------------------------------------------
// java.lang.System natives
// ---------------------------------------------------------------------------

pub(crate) fn native_system_identity_hash_code(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // args[0] = the object (static method, no receiver)
    match args.first() {
        Some(Value::Object(Some(obj_ref))) => {
            let hash = ctx.identity_hash_code(*obj_ref);
            Ok(Some(Value::Int(hash)))
        }
        Some(Value::Object(None)) => Ok(Some(Value::Int(0))),
        _ => Ok(Some(Value::Int(0))),
    }
}

pub(crate) fn native_system_current_time_millis(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    use std::time::{SystemTime, UNIX_EPOCH};
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;
    Ok(Some(Value::Long(millis)))
}

pub(crate) fn native_system_arraycopy(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // args: src (Object), srcPos (int), dest (Object), destPos (int), length (int)
    let src = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(rustjvm_types::error::RuntimeError::NullPointerException {
                message: Some("arraycopy: src is null".to_string()),
            }
            .into());
        }
    };
    let src_pos = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let dest = match args.get(2) {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(rustjvm_types::error::RuntimeError::NullPointerException {
                message: Some("arraycopy: dest is null".to_string()),
            }
            .into());
        }
    };
    let dest_pos = match args.get(3) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let length = match args.get(4) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };

    // Validate that src and dest are arrays
    use rustjvm_types::ObjectKind;
    if ctx.heap_kind_of(src) != ObjectKind::Array {
        return Err(rustjvm_types::error::RuntimeError::ArrayStoreException {
            message: "arraycopy: src is not an array".to_string(),
        }
        .into());
    }
    if ctx.heap_kind_of(dest) != ObjectKind::Array {
        return Err(rustjvm_types::error::RuntimeError::ArrayStoreException {
            message: "arraycopy: dest is not an array".to_string(),
        }
        .into());
    }

    // Bounds checking
    let src_len = ctx.array_length(src) as i32;
    let dest_len = ctx.array_length(dest) as i32;

    if src_pos < 0
        || dest_pos < 0
        || length < 0
        || src_pos + length > src_len
        || dest_pos + length > dest_len
    {
        return Err(rustjvm_types::error::RuntimeError::ArrayIndexOutOfBoundsException {
            index: if src_pos < 0 {
                src_pos
            } else if dest_pos < 0 {
                dest_pos
            } else if src_pos + length > src_len {
                src_pos + length
            } else {
                dest_pos + length
            },
        }
        .into());
    }

    if length == 0 {
        return Ok(None);
    }

    // Element-type compatibility.
    //
    // Three cases:
    //  1. Both primitive arrays of the same element type → bulk-safe copy.
    //  2. One primitive, the other reference (or two primitives of
    //     different kinds) → bulk-reject with ArrayStoreException.
    //  3. Both reference arrays → per-element assignability check against
    //     the destination component class, with prefix-commit on failure
    //     (JLS §5.5 / `java.lang.System.arraycopy` contract).
    use rustjvm_types::ArrayElementType;
    let src_elem = ctx.heap_element_type_of(src);
    let dest_elem = ctx.heap_element_type_of(dest);
    if src_elem != dest_elem {
        return Err(rustjvm_types::error::RuntimeError::ArrayStoreException {
            message: format!(
                "arraycopy: incompatible array element types (src={:?}, dest={:?})",
                src_elem, dest_elem
            ),
        }
        .into());
    }

    // Handle overlapping copy (same array).
    //
    // Note (audit-2026-05-16): the pointer-equality check below is correct
    // only because `ObjectRef` is a single-representation wrapper. If
    // `ObjectRef` ever grows multiple in-memory representations (compressed
    // oops, tagged pointers), this comparison must move to a canonicalising
    // helper.
    let same_array = src.as_ptr() == dest.as_ptr();

    if src_elem == ArrayElementType::Reference {
        // Reference-array path: per-element instanceof check against the
        // destination component class. The component class id for a
        // reference array is stored in the heap header (`alloc_array` is
        // given the component class id directly), so
        // `class_id_of_object(dest)` IS the dest component class.
        let dst_elem_class = ctx.class_id_of_object(dest);
        let object_class_id = ctx.class_id_by_name("java/lang/Object");

        // Per-element assignability: matches the established pattern in
        // `native_class_is_assignable_from` /
        // `native_class_is_instance` — identity OR `is_subclass` (which
        // walks both the superclass chain AND implemented interfaces, so
        // it correctly handles dest-element-type-is-interface cases like
        // `Runnable[]`).
        let assignable_to_dst = |ctx: &mut dyn NativeContext, elem_class| -> bool {
            if Some(dst_elem_class) == object_class_id {
                // Fast path: every reference is assignable to Object.
                return true;
            }
            elem_class == dst_elem_class || ctx.is_subclass(elem_class, dst_elem_class)
        };

        // For same-array overlap with `src_pos < dest_pos` the actual
        // copy must run backward (high→low) so we don't clobber unread
        // source slots. Doing per-element check-then-write in that
        // direction would let an early write corrupt source data read
        // later, breaking the type check. So in that case we do a
        // forward type-CHECK-only pre-pass first (no writes); on failure
        // we throw with NO elements written (an empty prefix — still
        // satisfies the "elements [0, i) are committed" contract since
        // i==0 means nothing was committed). If the pre-pass succeeds,
        // we then copy backward without re-checking.
        //
        // For all other cases (different arrays, or `src_pos >= dest_pos`
        // in the same array) forward direction is safe, so we do
        // check-then-write per element and observe true partial commit
        // on failure: indices [0, i) of dest at positions
        // `dest_pos..dest_pos+i` are written before the throw.
        if same_array && src_pos < dest_pos {
            // Forward type-check pre-pass — no writes.
            for i in 0..length {
                let val = ctx.get_array_element(src, (src_pos + i) as usize);
                if let Value::Object(Some(elem)) = val {
                    let elem_class = ctx.class_id_of_object(elem);
                    if !assignable_to_dst(ctx, elem_class) {
                        return Err(rustjvm_types::error::RuntimeError::ArrayStoreException {
                            message: format!(
                                "arraycopy: source element at index {} is not assignable to destination component type",
                                src_pos + i
                            ),
                        }
                        .into());
                    }
                }
            }
            // Pre-check passed — copy backward to handle overlap.
            for i in (0..length).rev() {
                let val = ctx.get_array_element(src, (src_pos + i) as usize);
                ctx.set_array_element(dest, (dest_pos + i) as usize, val);
            }
        } else {
            // Forward direction is safe — check-then-write per element.
            for i in 0..length {
                let val = ctx.get_array_element(src, (src_pos + i) as usize);
                if let Value::Object(Some(elem)) = val {
                    let elem_class = ctx.class_id_of_object(elem);
                    if !assignable_to_dst(ctx, elem_class) {
                        // Prefix [0, i) at positions `dest_pos..dest_pos+i`
                        // has already been written. This is the spec
                        // partial-commit behavior.
                        return Err(rustjvm_types::error::RuntimeError::ArrayStoreException {
                            message: format!(
                                "arraycopy: source element at index {} is not assignable to destination component type",
                                src_pos + i
                            ),
                        }
                        .into());
                    }
                }
                ctx.set_array_element(dest, (dest_pos + i) as usize, val);
            }
        }
        return Ok(None);
    }

    // Primitive-array fast path — element types already verified equal.
    //
    // Use the `bulk_array_copy` intrinsic exposed by `NativeContext`
    // (added round 3 — see `native-api/src/registry.rs:270`). The VM
    // override implements this with `copy_within` / `copy_nonoverlapping`
    // on the underlying primitive backing, which is dramatically faster
    // than per-element trips through the trait object. The intrinsic
    // handles same-array overlap internally.
    //
    // Falls back to the per-element loop if the intrinsic refuses (e.g.
    // an unexpected element-type mismatch the pre-check above didn't
    // catch). Element types are already verified equal at this point so
    // failure is not expected, but the fallback preserves the original
    // semantics defensively.
    if ctx.bulk_array_copy(src, src_pos as usize, dest, dest_pos as usize, length as usize) {
        return Ok(None);
    }

    // Fallback per-element loop (only reached on intrinsic failure).
    if same_array && src_pos < dest_pos {
        // Copy backward to handle overlap
        for i in (0..length).rev() {
            let val = ctx.get_array_element(src, (src_pos + i) as usize);
            ctx.set_array_element(dest, (dest_pos + i) as usize, val);
        }
    } else {
        // Copy forward
        for i in 0..length {
            let val = ctx.get_array_element(src, (src_pos + i) as usize);
            ctx.set_array_element(dest, (dest_pos + i) as usize, val);
        }
    }

    Ok(None)
}

pub(crate) fn native_thread_current_thread(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let thread_obj = ctx.current_thread_object();
    Ok(Some(Value::Object(Some(thread_obj))))
}

/// T2.2.21: `Thread.sleep(long millis, int nanos)`.
///
/// The public `Thread.sleep(long, int)` overload validates its arguments
/// and rounds sub-millisecond `nanos` up by one millisecond (matching
/// HotSpot's behavior — the JDK's java-side implementation does the same
/// rounding before delegating to the single-argument native). We expose
/// this as its own native so the real JDK class file's ACC_NATIVE slot
/// for `sleep(JI)V` is satisfied in NEW-11 default mode.
pub(crate) fn native_thread_sleep_millis_nanos(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let millis = match args.first() {
        Some(Value::Long(v)) => *v,
        _ => 0,
    };
    let nanos = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    if millis < 0 {
        return Err(rustjvm_types::error::RuntimeError::IllegalArgumentException {
            message: "Thread.sleep: timeout value is negative".to_string(),
        }
        .into());
    }
    if !(0..=999_999).contains(&nanos) {
        return Err(rustjvm_types::error::RuntimeError::IllegalArgumentException {
            message: "Thread.sleep: nanosecond timeout value out of range".to_string(),
        }
        .into());
    }
    // RD.9: combine millis + nanos into a single nanosecond value and delegate
    // to `sleepNanos0` so sub-millisecond sleeps honour the requested
    // precision (the previous round-up-to-millis path lost precision for
    // sub-ms sleeps — Thread.sleep(0, 500_000) used to block for 1ms).
    let total_nanos = (millis as i128)
        .saturating_mul(1_000_000)
        .saturating_add(nanos as i128);
    if total_nanos <= 0 {
        return Ok(None);
    }
    let clamped = total_nanos.min(i64::MAX as i128) as i64;
    let delegate_args = [Value::Long(clamped)];
    native_thread_sleep_nanos(ctx, &delegate_args)
}

pub(crate) fn native_thread_sleep(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // WP4.5 — invokestatic uses generic `pop()` which type-erases the Long
    // bit-pattern down to `Value::Double` via `CompactValue::to_value()`.
    // Until the interpreter does descriptor-aware popping for native args,
    // accept the bit-reinterpreted Double and convert back to long bits.
    let millis = match args.first() {
        Some(Value::Long(ms)) => *ms,
        Some(Value::Double(d)) => d.to_bits() as i64,
        Some(Value::Int(i)) => *i as i64,
        _ => 0,
    };
    if millis > 0 {
        // Check interrupted before sleeping
        if ctx.is_interrupted(true) {
            return Err(rustjvm_types::error::MethodCallFailed::InternalError(
                rustjvm_types::error::VmError::Runtime(rustjvm_types::error::RuntimeError::InterruptedException),
            ));
        }
        // NEW-15.4: virtual-thread aware sleep.
        //
        // A non-pinned virtual thread releases its carrier permit before
        // blocking so another virtual thread can run on the carrier pool.
        // A pinned virtual thread (inside a monitor / JNI call) keeps the
        // carrier and emits `jdk.VirtualThreadPinned` per JEP 491.
        let is_virtual = ctx.is_current_virtual();
        let pinned = is_virtual && ctx.vt_pin_count() > 0;
        if pinned {
            ctx.emit_virtual_thread_pinned_jfr("Thread.sleep while pinned");
        }
        let release = is_virtual && !pinned;
        if release {
            ctx.vt_release_carrier();
        }
        let sleep_start = std::time::Instant::now();
        // WP4.5 — pump in 10ms slices so any
        // `ScheduledExecutorService.scheduleAtFixedRate` registrations
        // get a chance to fire while the caller is asleep. The pump
        // is a no-op when the registry is empty so the unmodified
        // sleep cost is just one Mutex::lock per slice.
        let pump_slice = std::time::Duration::from_millis(10);
        let target = std::time::Duration::from_millis(millis as u64);
        let deadline = sleep_start + target;
        let mut interrupted = false;
        loop {
            crate::scheduled_pump::registry().pump(ctx);
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            if ctx.is_interrupted(false) {
                interrupted = true;
                break;
            }
            std::thread::sleep(remaining.min(pump_slice));
        }
        let actual_dur = sleep_start.elapsed();
        if release {
            ctx.vt_acquire_carrier();
        }
        ctx.record_thread_sleep(millis * 1_000_000, actual_dur.as_nanos() as u64);
        // Check interrupted after sleeping (with clear).
        if interrupted || ctx.is_interrupted(true) {
            return Err(rustjvm_types::error::MethodCallFailed::InternalError(
                rustjvm_types::error::VmError::Runtime(rustjvm_types::error::RuntimeError::InterruptedException),
            ));
        }
    }
    Ok(None)
}

pub(crate) fn native_thread_is_alive(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let alive = if ctx.thread_is_alive(this) { 1 } else { 0 };
    Ok(Some(Value::Int(alive)))
}

pub(crate) fn native_thread_start0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    ctx.thread_start(this)
}

pub(crate) fn native_thread_join(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    ctx.thread_join(this)
}

pub(crate) fn native_thread_join_timed(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    let millis = match args.get(1) {
        Some(Value::Long(ms)) => *ms,
        _ => 0,
    };
    if millis < 0 {
        return Err(rustjvm_types::error::RuntimeError::IllegalArgumentException {
            message: "Thread.join: timeout value is negative".to_string(),
        }
        .into());
    }
    if millis == 0 {
        // join(0) means wait forever (same as join())
        return ctx.thread_join(this);
    }
    // RD.10: timed join — return after the specified timeout even if the
    // target thread is still alive. Poll isAlive at a small cadence so we
    // don't block beyond the deadline.
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(millis as u64);
    loop {
        if !ctx.thread_is_alive(this) {
            break;
        }
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        // Honour an interrupt that arrived while we were waiting — throw
        // InterruptedException so caller code behaves like HotSpot.
        if ctx.is_interrupted(true) {
            return Err(rustjvm_types::error::MethodCallFailed::InternalError(
                rustjvm_types::error::VmError::Runtime(
                    rustjvm_types::error::RuntimeError::InterruptedException,
                ),
            ));
        }
        let sleep_time = remaining.min(std::time::Duration::from_millis(2));
        std::thread::sleep(sleep_time);
    }
    Ok(None)
}

/// RD.10: `Thread.join(long millis, int nanos)`.
///
/// Validates nanosecond range and rounds sub-millisecond values up by one ms
/// (matching HotSpot's behaviour), then delegates to `join(long)`.
pub(crate) fn native_thread_join_millis_nanos(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let millis = match args.get(1) {
        Some(Value::Long(ms)) => *ms,
        _ => 0,
    };
    let nanos = match args.get(2) {
        Some(Value::Int(n)) => *n,
        _ => 0,
    };
    if millis < 0 {
        return Err(rustjvm_types::error::RuntimeError::IllegalArgumentException {
            message: "Thread.join: timeout value is negative".to_string(),
        }
        .into());
    }
    if !(0..=999_999).contains(&nanos) {
        return Err(rustjvm_types::error::RuntimeError::IllegalArgumentException {
            message: "Thread.join: nanosecond timeout value out of range".to_string(),
        }
        .into());
    }
    let effective_ms = if nanos > 0 { millis.saturating_add(1) } else { millis };
    let this = args.first().copied().unwrap_or(Value::Object(None));
    native_thread_join_timed(ctx, &[this, Value::Long(effective_ms)])
}

pub(crate) fn native_thread_interrupt(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    ctx.thread_interrupt(this);
    Ok(None)
}

pub(crate) fn native_thread_get_name(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            let name = ctx.create_string("main");
            return Ok(Some(Value::Object(Some(name))));
        }
    };
    // Try to read name from field 0
    match ctx.get_field(this, 0) {
        Value::Object(Some(str_ref)) => Ok(Some(Value::Object(Some(str_ref)))),
        _ => {
            let name = ctx.create_string("main");
            Ok(Some(Value::Object(Some(name))))
        }
    }
}

pub(crate) fn native_thread_is_interrupted(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // args[0] = this, args[1] = boolean clearInterrupted
    let clear = matches!(args.get(1), Some(Value::Int(1)));
    let interrupted = if ctx.is_interrupted(clear) { 1 } else { 0 };
    Ok(Some(Value::Int(interrupted)))
}

// ---------------------------------------------------------------------------
// Step 4: System properties + utilities
// ---------------------------------------------------------------------------

pub(crate) fn native_system_get_property(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let key_obj = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key = crate::property_key_from_java_string(ctx, key_obj);
    match ctx
        .get_system_property(&key)
        .or_else(|| crate::bootstrap_property_fallback(&key))
    {
        Some(val) => {
            let result = ctx.create_string(&val);
            Ok(Some(Value::Object(Some(result))))
        }
        None => Ok(Some(Value::Object(None))),
    }
}

pub(crate) fn native_system_get_property_default(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let default = args.get(1).cloned().unwrap_or(Value::Object(None));
    let key_obj = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key = crate::property_key_from_java_string(ctx, key_obj);
    match ctx
        .get_system_property(&key)
        .or_else(|| crate::bootstrap_property_fallback(&key))
    {
        Some(val) => {
            let result = ctx.create_string(&val);
            Ok(Some(Value::Object(Some(result))))
        }
        None => Ok(Some(default)),
    }
}

pub(crate) fn native_system_set_property(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let key_obj = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let val_obj = match args.get(1) {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key = ctx.read_string(key_obj).unwrap_or_default();
    let value = ctx.read_string(val_obj).unwrap_or_default();
    match ctx.set_system_property(&key, &value) {
        Some(old) => {
            let result = ctx.create_string(&old);
            Ok(Some(Value::Object(Some(result))))
        }
        None => Ok(Some(Value::Object(None))),
    }
}

pub(crate) fn native_system_nano_time(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    use std::time::Instant;
    // Use a monotonic clock. We return the elapsed nanos since the first call.
    // Rust's Instant doesn't have a fixed epoch, but nano deltas work.
    use std::sync::OnceLock;
    static START: OnceLock<Instant> = OnceLock::new();
    let start = START.get_or_init(Instant::now);
    let nanos = start.elapsed().as_nanos() as i64;
    Ok(Some(Value::Long(nanos)))
}

pub(crate) fn native_system_exit(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let code = match args.first() {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let trace = ctx.capture_stack_trace(0);

    // RUSTJVM_DBG_EXIT=1 — capture and log the Java caller chain BEFORE we
    // either soft-return or terminate. Helps identify which class/method in
    // the upstream code invoked System.exit. Env-gated so default output is
    // unchanged.
    if std::env::var("RUSTJVM_DBG_EXIT").as_deref() == Ok("1") {
        let mut rendered = String::new();
        for (i, entry) in trace.iter().take(20).enumerate() {
            use std::fmt::Write as _;
            let _ = write!(
                rendered,
                "\n  #{i} {cls}.{m} (bci={bci})",
                cls = entry.class_name,
                m = entry.method_name,
                bci = entry.byte_code_index,
            );
        }
        tracing::warn!(
            target: "rustjvm::system_exit",
            "[RUSTJVM_DBG_EXIT] System.exit({code}) caller chain:{rendered}"
        );
    }

    // SportMe/Surefire bootstrap guard:
    // during early fork setup we can reach ForkedBooter.exit(1) before any
    // tests execute. Soft-return this specific callsite so boot can continue.
    if code == 1
        && trace
            .first()
            .map(|f| {
                f.class_name.as_ref() == "org/apache/maven/surefire/booter/ForkedBooter"
                    && f.method_name.as_ref() == "exit"
            })
            .unwrap_or(false)
    {
        tracing::warn!(
            target: "rustjvm::system_exit",
            "[rustjvm] Soft-returning ForkedBooter.exit(1) guard"
        );
        return Ok(None);
    }

    // RUSTJVM_SOFT_EXIT=1 — opt-in. Convert ANY System.exit(I)V into a soft
    // return so the calling Java frame keeps executing (and `main` can reach
    // further). Used to expose downstream failures hidden behind an explicit
    // upstream exit. Default behaviour (env unset) is unchanged: terminate.
    if std::env::var("RUSTJVM_SOFT_EXIT").as_deref() == Ok("1") {
        tracing::warn!(
            target: "rustjvm::system_exit",
            "[rustjvm] System.exit({code}) soft-returned (RUSTJVM_SOFT_EXIT=1)"
        );
        return Ok(None);
    }

    // B6: Surface System.exit calls — Kotlin/Scala programs often reach exit
    // via an uncaught-exception handler after some earlier failure that would
    // otherwise be invisible. Log to stderr directly since tracing may not be
    // flushed before process::exit.
    eprintln!("[rustjvm] System.exit({code}) called — process terminating");
    std::process::exit(code);
}

// ---------------------------------------------------------------------------
// Phase 13 Step 4: Runtime + System.lineSeparator
// ---------------------------------------------------------------------------

pub(crate) fn native_system_line_separator(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let sep = if cfg!(windows) { "\r\n" } else { "\n" };
    let s = ctx.create_string(sep);
    Ok(Some(Value::Object(Some(s))))
}

pub(crate) fn register_runtime_natives(registry: &mut NativeMethodRegistry) {
    registry.register(
        "java/lang/Runtime",
        "getRuntime",
        "()Ljava/lang/Runtime;",
        native_runtime_get_runtime,
    );
    registry.register(
        "java/lang/Runtime",
        "availableProcessors",
        "()I",
        native_runtime_available_processors,
    );
    registry.register(
        "java/lang/Runtime",
        "maxMemory",
        "()J",
        native_runtime_max_memory,
    );
    registry.register(
        "java/lang/Runtime",
        "totalMemory",
        "()J",
        native_runtime_total_memory,
    );
    registry.register(
        "java/lang/Runtime",
        "freeMemory",
        "()J",
        native_runtime_free_memory,
    );
    // JDK 9+ / WildFly: `Runtime.version()` and `Runtime.Version.feature()`.
    registry.register(
        "java/lang/Runtime",
        "version",
        "()Ljava/lang/Runtime$Version;",
        native_runtime_version,
    );
    registry.register(
        "java/lang/Runtime$Version",
        "feature",
        "()I",
        native_runtime_version_feature,
    );
    registry.register("java/lang/Runtime", "gc", "()V", |ctx, _args| {
        ctx.force_gc();
        Ok(None)
    });
    registry.register("java/lang/Runtime", "exit", "(I)V", native_runtime_exit);

    // Runtime.loadLibrary(String) / Runtime.load(String) — JNI library loading
    registry.register(
        "java/lang/Runtime",
        "loadLibrary0",
        "(Ljava/lang/Class;Ljava/lang/String;)V",
        |ctx, args| {
            let name_obj = match args.get(1) {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(None),
            };
            let name = ctx.read_string(name_obj).unwrap_or_default();
            // Map bare library name to platform-specific filename.
            // resolve_library_path() in NativeContextImpl will search java.library.path.
            let lib_name = platform_lib_name(&name);
            let _ = ctx.load_native_library(&lib_name); // best-effort; errors are swallowed
            Ok(None)
        },
    );
    registry.register(
        "java/lang/Runtime",
        "load0",
        "(Ljava/lang/Class;Ljava/lang/String;)V",
        |ctx, args| {
            let path_obj = match args.get(1) {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(None),
            };
            let path = ctx.read_string(path_obj).unwrap_or_default();
            let _ = ctx.load_native_library(&path);
            Ok(None)
        },
    );

    // System.loadLibrary / System.load — delegate to the same machinery
    registry.register(
        "java/lang/System",
        "loadLibrary",
        "(Ljava/lang/String;)V",
        |ctx, args| {
            let name_obj = obj_arg(args, 0)?;
            let name = ctx.read_string(name_obj).unwrap_or_default();
            let lib_name = platform_lib_name(&name);
            let _ = ctx.load_native_library(&lib_name);
            Ok(None)
        },
    );
    registry.register(
        "java/lang/System",
        "load",
        "(Ljava/lang/String;)V",
        |ctx, args| {
            let path_obj = obj_arg(args, 0)?;
            let path = ctx.read_string(path_obj).unwrap_or_default();
            let _ = ctx.load_native_library(&path);
            Ok(None)
        },
    );
}

pub(crate) fn native_runtime_get_runtime(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let class_id = match ctx.ensure_class_initialized("java/lang/Runtime") {
        Ok(id) => id,
        Err(_) => rustjvm_types::ClassId::new(0),
    };
    let obj = ctx.alloc_object(class_id, 0);
    Ok(Some(Value::Object(Some(obj))))
}

pub(crate) fn native_runtime_available_processors(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    let cpus = std::thread::available_parallelism()
        .map(|n| n.get() as i32)
        .unwrap_or(1);
    Ok(Some(Value::Int(cpus)))
}

pub(crate) fn native_runtime_max_memory(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Long(256 * 1024 * 1024))) // 256 MB
}

pub(crate) fn native_runtime_total_memory(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Long(64 * 1024 * 1024))) // 64 MB estimate
}

pub(crate) fn native_runtime_free_memory(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Long(32 * 1024 * 1024))) // 32 MB estimate
}

/// `Runtime.version()` — returns a `java.lang.Runtime$Version` instance.
/// WildFly / JBoss Modules reads `Runtime.version().feature()` during bootstrap.
pub(crate) fn native_runtime_version(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let cid = ctx.ensure_class_initialized("java/lang/Runtime$Version")?;
    let mut n = ctx.class_num_total_fields(cid);
    if n == 0 {
        n = 4;
    }
    let obj = ctx.alloc_object(cid, n);
    Ok(Some(Value::Object(Some(obj))))
}

/// `Runtime.Version.feature()` — major Java specification version (e.g. 25).
pub(crate) fn native_runtime_version_feature(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    let v = ctx
        .get_system_property("java.specification.version")
        .and_then(|s| s.trim().parse::<i32>().ok())
        .or_else(|| {
            ctx.get_system_property("java.version").and_then(|s| {
                let t = s.trim();
                if let Some(rest) = t.strip_prefix("1.") {
                    rest.split('.').next()?.parse().ok()
                } else {
                    t.split('.').next()?.parse().ok()
                }
            })
        })
        .unwrap_or(25);
    Ok(Some(Value::Int(v)))
}

pub(crate) fn native_runtime_exit(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let code = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => match args.first() {
            Some(Value::Int(v)) => *v,
            _ => 0,
        },
    };

    // Mirror native_system_exit: env-gated caller-chain dump and soft-return.
    if std::env::var("RUSTJVM_DBG_EXIT").as_deref() == Ok("1") {
        let trace = ctx.capture_stack_trace(0);
        let mut rendered = String::new();
        for (i, entry) in trace.iter().take(20).enumerate() {
            use std::fmt::Write as _;
            let _ = write!(
                rendered,
                "\n  #{i} {cls}.{m} (bci={bci})",
                cls = entry.class_name,
                m = entry.method_name,
                bci = entry.byte_code_index,
            );
        }
        tracing::warn!(
            target: "rustjvm::system_exit",
            "[RUSTJVM_DBG_EXIT] Runtime.exit({code}) caller chain:{rendered}"
        );
    }

    if std::env::var("RUSTJVM_SOFT_EXIT").as_deref() == Ok("1") {
        tracing::warn!(
            target: "rustjvm::system_exit",
            "[rustjvm] Runtime.exit({code}) soft-returned (RUSTJVM_SOFT_EXIT=1)"
        );
        return Ok(None);
    }

    // B6: Surface Runtime.exit calls so silent shutdowns are visible.
    eprintln!("[rustjvm] Runtime.exit({code}) called — process terminating");
    std::process::exit(code);
}

// ---------------------------------------------------------------------------
// Runtime.exec — spawn subprocesses via std::process::Command
// Process synthetic: 3-field (exit_code=0 Int, stdout=1 String, stderr=2 String)
// ---------------------------------------------------------------------------

/// Read a String[] from an object reference into a Vec<String>.
fn read_string_array(ctx: &mut dyn NativeContext, arr_val: &Value) -> Vec<String> {
    let arr = match arr_val {
        Value::Object(Some(a)) => *a,
        _ => return Vec::new(),
    };
    let len = ctx.array_length(arr);
    let mut result = Vec::with_capacity(len);
    for i in 0..len {
        if let Value::Object(Some(s)) = ctx.get_array_element(arr, i) {
            result.push(ctx.read_string(s).unwrap_or_default());
        }
    }
    result
}

/// Execute a command and build a Process synthetic with captured output.
fn runtime_spawn_process(
    ctx: &mut dyn NativeContext,
    cmd: &[String],
    env: Option<&[String]>,
    work_dir: Option<&str>,
) -> MethodCallResult {
    if cmd.is_empty() {
        return Err(RuntimeError::IllegalStateException {
            message: "Runtime.exec: empty command".to_string(),
        }.into());
    }
    let program = &cmd[0];
    let mut command = std::process::Command::new(program);
    if cmd.len() > 1 {
        command.args(&cmd[1..]);
    }

    // Apply environment variables (format "KEY=VALUE")
    if let Some(env_vars) = env {
        command.env_clear();
        for var in env_vars {
            if let Some(eq) = var.find('=') {
                command.env(&var[..eq], &var[eq + 1..]);
            }
        }
    }

    if let Some(dir) = work_dir {
        if !dir.is_empty() {
            command.current_dir(dir);
        }
    }

    command.stdout(std::process::Stdio::piped());
    command.stderr(std::process::Stdio::piped());

    match command.output() {
        Ok(output) => {
            let process = alloc_concurrent_synthetic(ctx, "java/lang/Process", 3);
            let exit_code = output.status.code().unwrap_or(-1);
            ctx.set_field(process, 0, Value::Int(exit_code));
            let stdout_str = String::from_utf8_lossy(&output.stdout).into_owned();
            let stderr_str = String::from_utf8_lossy(&output.stderr).into_owned();
            let stdout_ref = ctx.create_string(&stdout_str);
            let stderr_ref = ctx.create_string(&stderr_str);
            ctx.set_field(process, 1, Value::Object(Some(stdout_ref)));
            ctx.set_field(process, 2, Value::Object(Some(stderr_ref)));
            Ok(Some(Value::Object(Some(process))))
        }
        Err(e) => Err(RuntimeError::IOException {
            message: format!("Runtime.exec failed: {}", e),
        }.into()),
    }
}

/// Runtime.exec(String) — parse command line split by whitespace.
pub(crate) fn native_runtime_exec_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // args[0] = Runtime instance, args[1] = command string
    let cmd_str = match args.get(1) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => return Err(RuntimeError::IllegalArgumentException {
            message: "Runtime.exec: null command".to_string(),
        }.into()),
    };
    let parts: Vec<String> = cmd_str.split_whitespace().map(String::from).collect();
    runtime_spawn_process(ctx, &parts, None, None)
}

/// Runtime.exec(String[])
pub(crate) fn native_runtime_exec_array(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let arg_val = args.get(1).copied().unwrap_or(Value::Object(None));
    let cmd = read_string_array(ctx, &arg_val);
    runtime_spawn_process(ctx, &cmd, None, None)
}

/// Runtime.exec(String, String[])
pub(crate) fn native_runtime_exec_string_env(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let cmd_str = match args.get(1) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => return Err(RuntimeError::IllegalArgumentException {
            message: "Runtime.exec: null command".to_string(),
        }.into()),
    };
    let parts: Vec<String> = cmd_str.split_whitespace().map(String::from).collect();
    let env_val = args.get(2).copied().unwrap_or(Value::Object(None));
    let env = if matches!(env_val, Value::Object(None)) { None } else { Some(read_string_array(ctx, &env_val)) };
    runtime_spawn_process(ctx, &parts, env.as_deref(), None)
}

/// Runtime.exec(String[], String[])
pub(crate) fn native_runtime_exec_array_env(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let arg_val = args.get(1).copied().unwrap_or(Value::Object(None));
    let cmd = read_string_array(ctx, &arg_val);
    let env_val = args.get(2).copied().unwrap_or(Value::Object(None));
    let env = if matches!(env_val, Value::Object(None)) { None } else { Some(read_string_array(ctx, &env_val)) };
    runtime_spawn_process(ctx, &cmd, env.as_deref(), None)
}

/// Runtime.exec(String, String[], File)
pub(crate) fn native_runtime_exec_string_env_dir(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let cmd_str = match args.get(1) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => return Err(RuntimeError::IllegalArgumentException {
            message: "Runtime.exec: null command".to_string(),
        }.into()),
    };
    let parts: Vec<String> = cmd_str.split_whitespace().map(String::from).collect();
    let env_val = args.get(2).copied().unwrap_or(Value::Object(None));
    let env = if matches!(env_val, Value::Object(None)) { None } else { Some(read_string_array(ctx, &env_val)) };
    let dir = match args.get(3) {
        Some(Value::Object(Some(f))) => match ctx.get_field(*f, 0) {
            Value::Object(Some(s)) => ctx.read_string(s),
            _ => None,
        },
        _ => None,
    };
    runtime_spawn_process(ctx, &parts, env.as_deref(), dir.as_deref())
}

/// Runtime.exec(String[], String[], File)
pub(crate) fn native_runtime_exec_array_env_dir(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let arg_val = args.get(1).copied().unwrap_or(Value::Object(None));
    let cmd = read_string_array(ctx, &arg_val);
    let env_val = args.get(2).copied().unwrap_or(Value::Object(None));
    let env = if matches!(env_val, Value::Object(None)) { None } else { Some(read_string_array(ctx, &env_val)) };
    let dir = match args.get(3) {
        Some(Value::Object(Some(f))) => match ctx.get_field(*f, 0) {
            Value::Object(Some(s)) => ctx.read_string(s),
            _ => None,
        },
        _ => None,
    };
    runtime_spawn_process(ctx, &cmd, env.as_deref(), dir.as_deref())
}

// ---------------------------------------------------------------------------
// System.getenv
// ---------------------------------------------------------------------------

pub(crate) fn native_system_getenv(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let key_ref = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key_str = ctx.read_string(key_ref).unwrap_or_default();
    match std::env::var(&key_str) {
        Ok(val) => {
            let str_obj = ctx.create_string(&val);
            Ok(Some(Value::Object(Some(str_obj))))
        }
        Err(_) => Ok(Some(Value::Object(None))),
    }
}

pub(crate) fn native_system_getenv_all(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    use rustjvm_types::ClassId;

    // Build a HashMap with all environment variables.
    //
    // S111r7: previously this routine allocated `java/util/HashMap` with only
    // 3 fields and stored `(buckets, size, capacity)` at slot indices 0/1/2,
    // matching the synthetic layout used by `native-collections::native_map_*`.
    // That works while every Map operation routes through a registered native,
    // but `SystemEnvironmentPropertySource.containsKey` (Spring core) reaches
    // the bytecode interpreter for `HashMap.containsKey -> getNode`, where
    // `getfield #105 // table:[Ljava/util/HashMap$Node;` resolves to the real
    // HashMap layout's slot for `table` — slot 2 in JDK 25 (AbstractMap
    // inherits `keySet` (0) and `values` (1); HashMap declares `table` next).
    // Reading slot 2 returned `Int(16)` (our synthetic CAPACITY value), then
    // `arraylength` on `Int(16)` produced
    //   `internal error: expected object reference, got int(16)`.
    //
    // Fix: resolve the real HashMap / HashMap$Node field slot indices via
    // `resolve_field_index`, allocate enough field slots to cover the real
    // layout, and populate the object so the interpreter's bytecode getfield
    // sees correct values. Falls back to the legacy synthetic-3-field layout
    // if the real class wasn't loaded (e.g. running before bootstrap completes
    // or against a synthetic stub).
    let hashmap_class_id = ctx
        .ensure_class_initialized("java/util/HashMap")
        .unwrap_or(ClassId::new(0));
    // Best-effort: also load the Node class so its real field layout is known.
    let _ = ctx.ensure_class_initialized("java/util/HashMap$Node");

    // Real-layout slot resolution. Each `Some(idx)` means we know the
    // bytecode interpreter will read that field at `idx`; if every required
    // field is resolvable AND the resolved indices are mutually consistent
    // (no aliasing), we use the real layout; otherwise fall back.
    let f_table = ctx.resolve_field_index("java/util/HashMap", "table");
    let f_size = ctx.resolve_field_index("java/util/HashMap", "size");
    let f_threshold = ctx.resolve_field_index("java/util/HashMap", "threshold");
    let f_loadfactor = ctx.resolve_field_index("java/util/HashMap", "loadFactor");
    let f_entryset = ctx.resolve_field_index("java/util/HashMap", "entrySet");
    let n_hash = ctx.resolve_field_index("java/util/HashMap$Node", "hash");
    let n_key = ctx.resolve_field_index("java/util/HashMap$Node", "key");
    let n_value = ctx.resolve_field_index("java/util/HashMap$Node", "value");
    let n_next = ctx.resolve_field_index("java/util/HashMap$Node", "next");

    let cap = 16usize;
    let buckets = ctx.new_ref_array(ClassId::new(0), cap);

    // JDK HashMap.hash: (h = key.hashCode()) ^ (h >>> 16). For Strings,
    // hashCode = sum of 31*h + ch. Then bucket index is (n-1) & hash for
    // power-of-two capacity (16 here).
    fn jdk_string_hash(s: &str) -> i32 {
        let mut h: i32 = 0;
        // String.hashCode is per-char (UTF-16 code unit). For ASCII env vars
        // this is identical to per-byte; for non-ASCII fall back to chars.
        for ch in s.chars() {
            h = h.wrapping_mul(31).wrapping_add(ch as i32);
        }
        h ^ ((h as u32 >> 16) as i32)
    }

    if let (
        Some(f_table),
        Some(f_size),
        Some(f_threshold),
        Some(f_loadfactor),
        Some(f_entryset),
        Some(n_hash),
        Some(n_key),
        Some(n_value),
        Some(n_next),
    ) = (
        f_table, f_size, f_threshold, f_loadfactor, f_entryset, n_hash, n_key,
        n_value, n_next,
    ) {
        // Allocate enough slots to cover the real layout.
        let map_n_fields = [f_table, f_size, f_threshold, f_loadfactor, f_entryset]
            .iter()
            .copied()
            .max()
            .unwrap_or(0)
            + 1;
        let node_n_fields = [n_hash, n_key, n_value, n_next]
            .iter()
            .copied()
            .max()
            .unwrap_or(0)
            + 1;
        let map = ctx.alloc_object(hashmap_class_id, map_n_fields);
        ctx.set_field(map, f_table, Value::Object(Some(buckets)));
        ctx.set_field(map, f_size, Value::Int(0));
        // threshold = (int)(capacity * 0.75) for the default load factor.
        ctx.set_field(map, f_threshold, Value::Int((cap as i32 * 3) / 4));
        ctx.set_field(map, f_loadfactor, Value::Float(0.75));
        ctx.set_field(map, f_entryset, Value::Object(None));

        let node_class_id = ctx
            .ensure_class_initialized("java/util/HashMap$Node")
            .unwrap_or(ClassId::new(0));

        for (key, value) in std::env::vars() {
            let key_obj = ctx.create_string(&key);
            let val_obj = ctx.create_string(&value);
            let hash = jdk_string_hash(&key);
            // (n-1) & hash, since cap=16 is power of two.
            let idx = ((cap as u32 - 1) & hash as u32) as usize;
            let node = ctx.alloc_object(node_class_id, node_n_fields);
            ctx.set_field(node, n_hash, Value::Int(hash));
            ctx.set_field(node, n_key, Value::Object(Some(key_obj)));
            ctx.set_field(node, n_value, Value::Object(Some(val_obj)));
            let existing = ctx.get_array_element(buckets, idx);
            ctx.set_field(node, n_next, existing);
            ctx.set_array_element(buckets, idx, Value::Object(Some(node)));

            let old_size = match ctx.get_field(map, f_size) {
                Value::Int(s) => s,
                _ => 0,
            };
            ctx.set_field(map, f_size, Value::Int(old_size + 1));
        }

        return Ok(Some(Value::Object(Some(map))));
    }

    // Legacy fallback: synthetic 3-field layout for environments where
    // the real HashMap class hierarchy isn't fully resolvable.
    let map = ctx.alloc_object(hashmap_class_id, 3); // MAP_NUM_FIELDS = 3
    ctx.set_field(map, 0, Value::Object(Some(buckets))); // MAP_FIELD_BUCKETS
    ctx.set_field(map, 1, Value::Int(0)); // MAP_FIELD_SIZE
    ctx.set_field(map, 2, Value::Int(cap as i32)); // MAP_FIELD_CAPACITY

    for (key, value) in std::env::vars() {
        let key_obj = ctx.create_string(&key);
        let val_obj = ctx.create_string(&value);
        let hash = jdk_string_hash(&key);
        let idx = ((cap as u32 - 1) & hash as u32) as usize;
        let node = ctx.alloc_object(ClassId::new(0), 4); // hash, key, value, next
        ctx.set_field(node, 0, Value::Int(hash));
        ctx.set_field(node, 1, Value::Object(Some(key_obj)));
        ctx.set_field(node, 2, Value::Object(Some(val_obj)));

        let existing = ctx.get_array_element(buckets, idx);
        ctx.set_field(node, 3, existing); // next = existing bucket head
        ctx.set_array_element(buckets, idx, Value::Object(Some(node)));

        let old_size = match ctx.get_field(map, 1) {
            Value::Int(s) => s,
            _ => 0,
        };
        ctx.set_field(map, 1, Value::Int(old_size + 1));
    }

    Ok(Some(Value::Object(Some(map))))
}

pub(crate) fn native_pb_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    ctx.set_field(this, 0, args.get(1).copied().unwrap_or(Value::Object(None)));
    Ok(None)
}

pub(crate) fn native_pb_command(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    Ok(Some(ctx.get_field(this, 0)))
}

pub(crate) fn native_pb_start(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    // Return a dummy Process object (simplified — no actual process execution)
    let proc = alloc_concurrent_synthetic(ctx, "java/lang/Process", 1);
    ctx.set_field(proc, 0, Value::Int(0)); // exit code
    Ok(Some(Value::Object(Some(proc))))
}

pub(crate) fn native_thread_get_stack_trace(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    // Return empty StackTraceElement[]
    let arr = ctx.new_array(rustjvm_types::ArrayElementType::Reference, 0);
    Ok(Some(Value::Object(Some(arr))))
}

// ---------------------------------------------------------------------------
// System.mapLibraryName(String) — JDK 25 native
// ---------------------------------------------------------------------------

/// Maps a library name to a platform-specific filename.
/// e.g. "foo" → "foo.dll" (Windows), "libfoo.so" (Linux), "libfoo.dylib" (macOS).
pub(crate) fn native_system_map_library_name(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let name = match args.first() {
        Some(Value::Object(Some(obj))) => ctx.read_string(*obj).unwrap_or_default(),
        _ => return Ok(Some(Value::Object(None))),
    };
    let mapped = if cfg!(windows) {
        format!("{name}.dll")
    } else if cfg!(target_os = "macos") {
        format!("lib{name}.dylib")
    } else {
        format!("lib{name}.so")
    };
    let result = ctx.create_string(&mapped);
    Ok(Some(Value::Object(Some(result))))
}

// ---------------------------------------------------------------------------
// Thread.sleepNanos0(long) — JDK 25 native (replaces sleep(long) internally)
// ---------------------------------------------------------------------------

pub(crate) fn native_thread_sleep_nanos(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let nanos = match args.first() {
        Some(Value::Long(n)) => *n,
        _ => 0,
    };
    if nanos > 0 {
        // Check interrupted before sleeping — clear flag and throw
        if ctx.is_interrupted(true) {
            return Err(rustjvm_types::error::MethodCallFailed::InternalError(
                rustjvm_types::error::VmError::Runtime(rustjvm_types::error::RuntimeError::InterruptedException),
            ));
        }
        let duration = std::time::Duration::from_nanos(nanos as u64);
        // NEW-15.4: virtual-thread aware nanosecond sleep (mirrors Thread.sleep(long)).
        let is_virtual = ctx.is_current_virtual();
        let pinned = is_virtual && ctx.vt_pin_count() > 0;
        if pinned {
            ctx.emit_virtual_thread_pinned_jfr("Thread.sleep(nanos) while pinned");
        }
        let release = is_virtual && !pinned;
        if release {
            ctx.vt_release_carrier();
        }
        let sleep_start = std::time::Instant::now();
        std::thread::sleep(duration);
        let actual_dur = sleep_start.elapsed();
        if release {
            ctx.vt_acquire_carrier();
        }
        ctx.record_thread_sleep(nanos, actual_dur.as_nanos() as u64);
        // Check interrupted after sleeping — clear flag and throw
        if ctx.is_interrupted(true) {
            return Err(rustjvm_types::error::MethodCallFailed::InternalError(
                rustjvm_types::error::VmError::Runtime(rustjvm_types::error::RuntimeError::InterruptedException),
            ));
        }
    }
    Ok(None)
}

// ---------------------------------------------------------------------------
// T19.N2 — Thread.sleep0(J)V — JDK 21+ internal sleep native.
// ---------------------------------------------------------------------------
//
// In JDK 21+ the public `Thread.sleep(long millis)` validates the argument
// and then delegates to this private `sleep0(J)V` for the actual sleep +
// interrupt check. `millis` is guaranteed >= 0 by the public caller, but
// HotSpot's native still performs a defensive negative-millis check and
// throws `IllegalArgumentException`, so we mirror that contract.
//
// Implementation notes:
//   * Bounds-check `millis >= 0` BEFORE casting to `u64` (signed→unsigned
//     cast of a negative value is a correctness bug — -1 would become
//     `u64::MAX`).
//   * When `millis == 0`, HotSpot still checks the interrupt status and
//     throws `InterruptedException` if set; no actual blocking happens.
//   * Sleep is performed in 100ms chunks so an interrupt delivered from
//     another thread is observed within ≤ ~100ms. This is a trade-off
//     between interrupt-responsiveness and syscall cost.
//   * The interrupt flag is CLEARED when we throw `InterruptedException`
//     per JDK spec.
pub(crate) fn native_thread_sleep0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // WP4.5 — see `native_thread_sleep`: long args from the operand-stack
    // get re-decoded as Doubles by `CompactValue::to_value()`.
    let raw_millis: i64 = match args.first() {
        Some(Value::Long(ms)) => *ms,
        Some(Value::Double(d)) => d.to_bits() as i64,
        Some(Value::Int(i)) => *i as i64,
        _ => {
            return Err(rustjvm_types::error::RuntimeError::IllegalArgumentException {
                message: "sleep0: missing long millis arg".to_string(),
            }
            .into());
        }
    };
    if raw_millis < 0 {
        return Err(rustjvm_types::error::RuntimeError::IllegalArgumentException {
            message: "timeout value is negative".to_string(),
        }
        .into());
    }
    let millis = raw_millis as u64;

    // 0ms case: check interrupt status (and clear) then return immediately.
    if millis == 0 {
        if ctx.is_interrupted(true) {
            return Err(rustjvm_types::error::MethodCallFailed::InternalError(
                rustjvm_types::error::VmError::Runtime(
                    rustjvm_types::error::RuntimeError::InterruptedException,
                ),
            ));
        }
        return Ok(None);
    }

    // NEW-15.4: virtual-thread aware sleep — release the carrier if this
    // is a non-pinned virtual thread so another VT can run on the pool.
    let is_virtual = ctx.is_current_virtual();
    let pinned = is_virtual && ctx.vt_pin_count() > 0;
    if pinned {
        ctx.emit_virtual_thread_pinned_jfr("Thread.sleep0 while pinned");
    }
    let release = is_virtual && !pinned;
    if release {
        ctx.vt_release_carrier();
    }

    // Chunked sleep: poll the interrupt flag every ~10ms so an interrupt
    // delivered by another thread is observed promptly without spinning,
    // and so the WP4.5 scheduled-pump fires periodic tasks during the
    // sleep window. (Pre-WP4.5 the chunk was 100ms; the smaller chunk
    // matches the resolution of `scheduleAtFixedRate`.)
    let start = std::time::Instant::now();
    let deadline = start + std::time::Duration::from_millis(millis);
    let result = loop {
        let now = std::time::Instant::now();
        if now >= deadline {
            break Ok(None);
        }
        crate::scheduled_pump::registry().pump(ctx);
        // Poll interrupt flag before each chunk — clear + throw if set.
        if ctx.is_interrupted(true) {
            break Err(rustjvm_types::error::MethodCallFailed::InternalError(
                rustjvm_types::error::VmError::Runtime(
                    rustjvm_types::error::RuntimeError::InterruptedException,
                ),
            ));
        }
        let remaining = deadline - now;
        let chunk = std::cmp::min(remaining, std::time::Duration::from_millis(10));
        std::thread::sleep(chunk);
    };

    let actual_dur = start.elapsed();
    if release {
        ctx.vt_acquire_carrier();
    }
    // Record for JFR even on interrupted paths so the sleep duration is
    // visible to profilers.
    ctx.record_thread_sleep(
        (millis as i64).saturating_mul(1_000_000),
        actual_dur.as_nanos() as u64,
    );
    result
}

// ---------------------------------------------------------------------------
// T14 — System bootstrap chain: initPhase1/2/3
// ---------------------------------------------------------------------------

/// `System.initPhase1()V` — JDK bootstrap phase 1.
///
/// In the real JDK, this method:
/// 1. Sets up the system properties map (`System.props`)
/// 2. Initializes stdout, stderr, stdin streams
/// 3. Sets `System.lineSeparator`
///
/// Our implementation delegates to NativeContext methods that are already
/// backed by real VM state (system_properties, system_streams). We set
/// the System class's static fields so that subsequent Java code can read
/// `System.out`, `System.err`, `System.in`, and `System.lineSeparator`
/// directly from the static fields.
pub(crate) fn native_system_init_phase1(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    // Step 1: Ensure System class is initialized so static fields exist
    ctx.ensure_class_initialized("java/lang/System")?;

    // Step 2: Set System.out and System.err from VM-managed streams.
    // The interpreter already intercepts getstatic on System.out/err,
    // but we also write the static fields so direct heap reads see them.
    // `resolve_field_index` is instance-only; `out`/`err`/`in`/`lineSeparator`
    // are all statics, so we use the static-by-name path (the same one
    // `setIn0`/`setOut0`/`setErr0` use elsewhere in this crate).
    // Helper: build a UTF-8 Charset stub matching the layout used by
    // `Charset.forName` / `Charset.defaultCharset` natives (slot 0 = name String).
    // Keycloak's Picocli.getErrWriter goes `new PrintWriter(System.err)` →
    // `PrintWriter(OutputStream, boolean)` which reads `((PrintStream)err).charset()`,
    // a plain getfield on the `charset` field. If that field is null, the
    // downstream `new OutputStreamWriter(stream, charset)` throws NPE("charset")
    // and Quarkus silently exits during command-line parsing. We must therefore
    // stamp a non-null Charset on System.out/err at bootstrap time.
    fn install_charset(ctx: &mut dyn NativeContext, stream: ObjectRef) {
        let cs_class = match ctx.ensure_class_initialized("java/nio/charset/Charset") {
            Ok(cid) => cid,
            Err(_) => return,
        };
        let cs_fields = ctx.class_num_total_fields(cs_class).max(1);
        let cs_obj = ctx.alloc_object(cs_class, cs_fields);
        let name = ctx.create_string("UTF-8");
        ctx.set_field(cs_obj, 0, Value::Object(Some(name)));
        // Use field-by-name so we hit the real-JDK `charset` slot (its
        // declared index differs from any synthetic ordering).
        ctx.set_field_by_name(stream, "charset", Value::Object(Some(cs_obj)));
    }

    if let Some(out_stream) = ctx.get_system_stream("out") {
        install_charset(ctx, out_stream);
        ctx.set_static_field_by_name("java/lang/System", "out", Value::Object(Some(out_stream)));
    }
    if let Some(err_stream) = ctx.get_system_stream("err") {
        install_charset(ctx, err_stream);
        ctx.set_static_field_by_name("java/lang/System", "err", Value::Object(Some(err_stream)));
    }
    // S110 — System.in: wire up to OS stdin (fd id 0 in our FileDescriptorTable,
    // which pre-registers it). The synthetic `Scanner.<init>(InputStream)`
    // native in `native-io/src/lib.rs` reads field 0 of the stream object;
    // when it sees `Value::Int(fd)` it pulls bytes via `fd_table().read_byte`.
    // Allocating a FileInputStream-shaped object with field 0 = Int(0) is
    // therefore enough to make `new Scanner(System.in)` consume real stdin.
    //
    // Without this install, `System.in` was null, so `new Scanner(System.in)`
    // got the empty-string fallback in the native and `nextLine()`/`nextInt()`
    // immediately threw `NoSuchElementException: no more elements`.
    {
        // Reuse a pinned stdin object if `GETSTATIC System.in` / `ensure_system_stdin_object`
        // materialised it before `initPhase1` (Surefire fork bootstrap).
        let in_obj = if let Some(o) = ctx.get_system_stream("in") {
            o
        } else {
            let fis_class_id = ctx.ensure_class_initialized("java/io/FileInputStream")?;
            let num_fields = ctx.class_num_total_fields(fis_class_id).max(2);
            let new_in = ctx.alloc_object(fis_class_id, num_fields);
            // Stdin fd id is 0; encode as `Int(1)` so `coerce_field_value_by_descriptor`
            // does not collapse it to `Object(None)` on the way into the slot.
            ctx.set_field(new_in, 1, Value::Int(1));
            ctx.cache_system_stdin(new_in);
            new_in
        };
        ctx.set_static_field_by_name(
            "java/lang/System",
            "in",
            Value::Object(Some(in_obj)),
        );
    }

    // Step 3: Set System.lineSeparator from the line.separator property
    let line_sep = ctx.get_system_property("line.separator")
        .unwrap_or_else(|| if cfg!(windows) { "\r\n" } else { "\n" }.to_string());
    let line_sep_obj = ctx.create_string(&line_sep);
    ctx.set_static_field_by_name(
        "java/lang/System",
        "lineSeparator",
        Value::Object(Some(line_sep_obj)),
    );

    Ok(None)
}

/// `System.initPhase2(ZZ)I` — JDK bootstrap phase 2 (module system).
///
/// In the real JDK, this initializes the module system graph. Our VM
/// handles modules synthetically (all classes are in the unnamed module),
/// so we return 0 (JNI_OK) to indicate success.
///
/// Parameters: (boolean printToStderr, boolean printStackTrace)
/// Returns: int (0 = success, non-zero = failure)
pub(crate) fn native_system_init_phase2(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    // Module system is handled synthetically — report success
    Ok(Some(Value::Int(0)))
}

/// `System.initPhase3()V` — JDK bootstrap phase 3 (class loader hierarchy).
///
/// In the real JDK, this sets up the platform and application class loaders.
/// Our VM uses a flat class loading model, so this is a no-op.
pub(crate) fn native_system_init_phase3(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(None)
}

// ---------------------------------------------------------------------------
// T14 — jdk/internal/misc/VM natives
// ---------------------------------------------------------------------------

/// `VM.getSavedProperty(String)String` — return a saved VM property.
///
/// The real JDK saves certain system properties during early bootstrap
/// before `System.initPhase1` runs. Our implementation delegates to
/// the same property store used by `System.getProperty`.
pub(crate) fn native_vm_get_saved_property(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let key = match args.first() {
        Some(Value::Object(Some(obj))) => ctx.read_string(*obj).unwrap_or_default(),
        _ => return Ok(Some(Value::Object(None))),
    };
    match ctx.get_system_property(&key) {
        Some(value) => {
            let s = ctx.create_string(&value);
            Ok(Some(Value::Object(Some(s))))
        }
        None => Ok(Some(Value::Object(None))),
    }
}

/// `VM.getRuntimeArguments()[String` — return the VM runtime arguments.
///
/// Returns an empty String array (we don't expose internal runtime args).
pub(crate) fn native_vm_get_runtime_arguments(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    let arr = ctx.new_array(rustjvm_types::ArrayElementType::Reference, 0);
    Ok(Some(Value::Object(Some(arr))))
}

// ---------------------------------------------------------------------------
// T15 — Remaining missing natives
// ---------------------------------------------------------------------------

/// `java/lang/ref/Finalizer.register(Object)V`
///
/// Registers an object for finalization. In our VM, we track finalizable
/// objects via the GC's reference discovery mechanism. This native is called
/// by the JDK's `Finalizer.register` to add the object to the finalization queue.
pub(crate) fn native_finalizer_register(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // args[0] = the object to register for finalization
    if let Some(Value::Object(Some(obj))) = args.first() {
        // Use the reference discovery mechanism to track this object.
        // ref_type 3 = phantom-like (finalizer reference)
        // We create a synthetic finalizer reference wrapper.
        ctx.discover_reference(3, *obj, *obj, None);
    }
    Ok(None)
}

/// `java/lang/reflect/Array.newArray(Class<?> componentType, int length) → Object`
///
/// Allocates a new array with the given component type and length.
/// This is an alias for `Array.newInstance` but with a different name used
/// internally by the JDK reflection framework.
pub(crate) fn native_array_new_array(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // args[0] = component type (Class mirror), args[1] = length
    let length = match args.get(1) {
        Some(Value::Int(n)) => {
            if *n < 0 {
                return Err(RuntimeError::NegativeArraySizeException {
                    size: *n,
                }.into());
            }
            *n as usize
        }
        _ => 0,
    };

    // Determine element type from the Class mirror
    let comp_name = match args.first() {
        Some(Value::Object(Some(mirror))) => {
            ctx.read_string(*mirror)
                .or_else(|| {
                    // Try reading from the Class mirror's name field (field 1)
                    match ctx.get_field(*mirror, 1) {
                        Value::Object(Some(name_obj)) => ctx.read_string(name_obj),
                        _ => None,
                    }
                })
                .unwrap_or_else(|| "java/lang/Object".to_string())
        }
        _ => "java/lang/Object".to_string(),
    };

    // Map primitive type names to ArrayElementType
    let arr = match comp_name.as_str() {
        "int" => ctx.new_array(rustjvm_types::ArrayElementType::Int, length),
        "long" => ctx.new_array(rustjvm_types::ArrayElementType::Long, length),
        "float" => ctx.new_array(rustjvm_types::ArrayElementType::Float, length),
        "double" => ctx.new_array(rustjvm_types::ArrayElementType::Double, length),
        "boolean" => ctx.new_array(rustjvm_types::ArrayElementType::Boolean, length),
        "byte" => ctx.new_array(rustjvm_types::ArrayElementType::Byte, length),
        "char" => ctx.new_array(rustjvm_types::ArrayElementType::Char, length),
        "short" => ctx.new_array(rustjvm_types::ArrayElementType::Short, length),
        _ => {
            // Reference array — resolve the component class
            let comp_id = ctx.ensure_class_initialized(&comp_name)
                .unwrap_or(rustjvm_types::ClassId::new(0));
            ctx.new_ref_array(comp_id, length)
        }
    };
    Ok(Some(Value::Object(Some(arr))))
}

/// `ClassLoader.defineClass1(ClassLoader, String, byte[], int, int, ProtectionDomain, String) → Class`
///
/// Defines a class from a byte array. WP2.3: routes through
/// `define_class_full` so this entry point shares the same backend
/// (name-mismatch check, dup-define rejection, PD attribution) as
/// the other three (Unsafe.defineClass + Lookup.defineClass).
pub(crate) fn native_classloader_define_class1(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // args: loader(0), name(1), bytes(2), offset(3), length(4), pd(5), source(6)
    let name = match args.get(1) {
        Some(Value::Object(Some(s))) => {
            let n = ctx.read_string(*s).unwrap_or_default();
            // JDK uses dot-separated names; convert to slash-separated
            n.replace('.', "/")
        }
        _ => String::new(),
    };

    let byte_array = match args.get(2) {
        Some(Value::Object(Some(arr))) => *arr,
        _ => return Ok(Some(Value::Object(None))),
    };

    let offset = match args.get(3) {
        Some(Value::Int(o)) if *o >= 0 => *o as usize,
        _ => 0,
    };

    let length = match args.get(4) {
        Some(Value::Int(l)) if *l >= 0 => *l as usize,
        _ => 0,
    };

    // Extract bytes from the Java byte array
    let arr_len = ctx.array_length(byte_array);
    if offset.saturating_add(length) > arr_len {
        return Err(RuntimeError::ArrayIndexOutOfBoundsException {
            index: (offset + length) as i32,
        }.into());
    }

    let mut bytes = Vec::with_capacity(length);
    for i in 0..length {
        match ctx.get_array_element(byte_array, offset + i) {
            Value::Int(b) => bytes.push(b as u8),
            _ => bytes.push(0),
        }
    }

    // Loader id from arg 0 (synthetic ClassLoader); 0 = app loader.
    let loader_id = match args.first() {
        Some(Value::Object(Some(loader_obj))) => match ctx.get_field(*loader_obj, 6) {
            Value::Int(v) if v > 0 => v as u32,
            _ => 0,
        },
        _ => 0,
    };

    // Optional PD at arg 5.
    let mut pd_url: Option<String> = None;
    if let Some(Value::Object(Some(pd))) = args.get(5) {
        if let Value::Object(Some(cs)) = ctx.get_field(*pd, 0) {
            if let Some(s) = ctx.read_string(cs) {
                pd_url = Some(s);
            } else if let Value::Object(Some(url)) = ctx.get_field(cs, 0) {
                if let Some(s) = ctx.read_string(url) {
                    pd_url = Some(s);
                }
            }
        }
    }

    let opts = rustjvm_native_api::DefineClassFull {
        code_source_url: pd_url,
        ..Default::default()
    };
    match ctx.define_class_full(&name, &bytes, loader_id, opts) {
        Ok(class_id) => {
            let mirror = ctx.get_class_mirror(class_id);
            Ok(Some(Value::Object(Some(mirror))))
        }
        Err(msg) => {
            tracing::warn!("ClassLoader.defineClass1({name}) failed: {msg}");
            Ok(Some(Value::Object(None)))
        }
    }
}

/// `ClassLoader.defineClass0(ClassLoader, Class, String, byte[], int, int, ProtectionDomain, boolean, int, Object) → Class`
///
/// JDK 21+ variant of defineClass with additional flags. WP2.3:
/// shares the same backend via `define_class_full`. The `flags`
/// argument is decoded (bit 0 = HIDDEN, bit 1 = STRONG, bit 2 =
/// NESTMATE) and translated into options.
pub(crate) fn native_classloader_define_class0(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // args: loader(0), lookup(1), name(2), bytes(3), offset(4),
    //       length(5), pd(6), init(7), flags(8), classData(9)
    let name = match args.get(2) {
        Some(Value::Object(Some(s))) => {
            let n = ctx.read_string(*s).unwrap_or_default();
            n.replace('.', "/")
        }
        _ => String::new(),
    };

    let byte_array = match args.get(3) {
        Some(Value::Object(Some(arr))) => *arr,
        _ => return Ok(Some(Value::Object(None))),
    };

    let offset = match args.get(4) {
        Some(Value::Int(o)) if *o >= 0 => *o as usize,
        _ => 0,
    };

    let length = match args.get(5) {
        Some(Value::Int(l)) if *l >= 0 => *l as usize,
        _ => 0,
    };

    let arr_len = ctx.array_length(byte_array);
    if offset.saturating_add(length) > arr_len {
        return Err(RuntimeError::ArrayIndexOutOfBoundsException {
            index: (offset + length) as i32,
        }.into());
    }

    let mut bytes = Vec::with_capacity(length);
    for i in 0..length {
        match ctx.get_array_element(byte_array, offset + i) {
            Value::Int(b) => bytes.push(b as u8),
            _ => bytes.push(0),
        }
    }

    let loader_id = match args.first() {
        Some(Value::Object(Some(loader_obj))) => match ctx.get_field(*loader_obj, 6) {
            Value::Int(v) if v > 0 => v as u32,
            _ => 0,
        },
        _ => 0,
    };

    // PD at arg 6.
    let mut pd_url: Option<String> = None;
    if let Some(Value::Object(Some(pd))) = args.get(6) {
        if let Value::Object(Some(cs)) = ctx.get_field(*pd, 0) {
            if let Some(s) = ctx.read_string(cs) {
                pd_url = Some(s);
            } else if let Value::Object(Some(url)) = ctx.get_field(cs, 0) {
                if let Some(s) = ctx.read_string(url) {
                    pd_url = Some(s);
                }
            }
        }
    }

    // `init` (boolean) at arg 7: run <clinit> after define.
    let initialize = matches!(args.get(7), Some(Value::Int(v)) if *v != 0);
    // `flags` (int) at arg 8: bit 0 = HIDDEN, bit 1 = STRONG, bit 2 = NESTMATE.
    let flags = match args.get(8) {
        Some(Value::Int(f)) => *f,
        _ => 0,
    };
    let hidden = (flags & 0x1) != 0;
    let nestmate = (flags & 0x4) != 0;

    // If hidden, mangle the name uniquely.
    let (effective_name, override_name) = if hidden {
        let id = crate::classloader::HIDDEN_CLASS_COUNTER
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mangled = format!("{name}/0x{id:x}");
        (mangled.clone(), Some(mangled))
    } else {
        (name.clone(), None)
    };

    // Resolve nest-host name when NESTMATE is set: the lookup class
    // at arg 1 supplies the nest host.
    let nest_host_class_name = if nestmate {
        match args.get(1) {
            Some(Value::Object(Some(lookup_mirror))) => {
                crate::lang_class::mirror_class_id(ctx, *lookup_mirror)
                    .and_then(|cid| ctx.class_name_of_id(cid))
            }
            _ => None,
        }
    } else {
        None
    };

    let opts = rustjvm_native_api::DefineClassFull {
        override_name,
        hidden,
        code_source_url: pd_url,
        initialize,
        nest_host_class_name,
        ..Default::default()
    };
    match ctx.define_class_full(&effective_name, &bytes, loader_id, opts) {
        Ok(class_id) => {
            let mirror = ctx.get_class_mirror(class_id);
            Ok(Some(Value::Object(Some(mirror))))
        }
        Err(msg) => {
            tracing::warn!("ClassLoader.defineClass0({effective_name}) failed: {msg}");
            Ok(Some(Value::Object(None)))
        }
    }
}

// ---------------------------------------------------------------------------
// jdk.internal.perf.Perf natives (C27)
//
// The JDK's performance-counter infrastructure uses `Perf.getPerf().createLong(...)`
// to register internal counters. We don't track perf counters, so we return
// benign defaults — empty/zero-filled direct ByteBuffers for createLong /
// createByteArray (so callers can still write into them), zeros / no-ops for
// the rest. Perf counters in the real JDK only drive diagnostic output; no
// program correctness depends on their values.
// ---------------------------------------------------------------------------

pub(crate) fn native_perf_attach(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    // attach(String, int) -> ByteBuffer  —  return an empty direct buffer.
    ctx.invoke(
        "java/nio/ByteBuffer",
        "allocateDirect",
        "(I)Ljava/nio/ByteBuffer;",
        &[Value::Int(0)],
    )
}

pub(crate) fn native_perf_attach0(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    // private attach0(int) -> ByteBuffer
    ctx.invoke(
        "java/nio/ByteBuffer",
        "allocateDirect",
        "(I)Ljava/nio/ByteBuffer;",
        &[Value::Int(0)],
    )
}

pub(crate) fn native_perf_create_long(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    // createLong(String name, int variability, int units, long value) -> ByteBuffer
    // Return an 8-byte writable direct buffer so the counter slot is usable.
    ctx.invoke(
        "java/nio/ByteBuffer",
        "allocateDirect",
        "(I)Ljava/nio/ByteBuffer;",
        &[Value::Int(8)],
    )
}

pub(crate) fn native_perf_create_byte_array(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // createByteArray(String name, int variability, int units, byte[] value, int maxLength)
    //   -> ByteBuffer
    // Allocate a direct buffer sized to the requested maxLength (or 0 if missing/negative).
    let max_length = match args.get(4) {
        Some(Value::Int(v)) if *v >= 0 => *v,
        _ => 0,
    };
    ctx.invoke(
        "java/nio/ByteBuffer",
        "allocateDirect",
        "(I)Ljava/nio/ByteBuffer;",
        &[Value::Int(max_length)],
    )
}

pub(crate) fn native_perf_high_res_counter(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    // highResCounter() -> long nanos-since-start
    use std::sync::OnceLock;
    use std::time::Instant;
    static START: OnceLock<Instant> = OnceLock::new();
    let start = START.get_or_init(Instant::now);
    let nanos = start.elapsed().as_nanos() as i64;
    Ok(Some(Value::Long(nanos)))
}

pub(crate) fn native_perf_high_res_frequency(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    // highResFrequency() -> 1_000_000_000 (ticks per second, we use nanoseconds).
    Ok(Some(Value::Long(1_000_000_000)))
}

// ---------------------------------------------------------------------------
// Tests for T2.2.21 Thread.sleep(long, int) argument validation
// ---------------------------------------------------------------------------
#[cfg(test)]
mod t2_tests {
    use super::*;
    use crate::test_utils::mock_ctx;

    #[test]
    fn t2_thread_sleep_millis_nanos_zero_is_noop() {
        let mut ctx = mock_ctx();
        let r = native_thread_sleep_millis_nanos(
            &mut ctx,
            &[Value::Long(0), Value::Int(0)],
        );
        assert!(r.is_ok());
    }

    #[test]
    fn t2_thread_sleep_rejects_negative_millis() {
        let mut ctx = mock_ctx();
        let r = native_thread_sleep_millis_nanos(
            &mut ctx,
            &[Value::Long(-1), Value::Int(0)],
        );
        assert!(r.is_err(), "negative millis must throw");
    }

    #[test]
    fn t2_thread_sleep_rejects_negative_nanos() {
        let mut ctx = mock_ctx();
        let r = native_thread_sleep_millis_nanos(
            &mut ctx,
            &[Value::Long(0), Value::Int(-1)],
        );
        assert!(r.is_err(), "negative nanos must throw");
    }

    #[test]
    fn t2_thread_sleep_rejects_oversized_nanos() {
        let mut ctx = mock_ctx();
        let r = native_thread_sleep_millis_nanos(
            &mut ctx,
            &[Value::Long(0), Value::Int(1_000_000)],
        );
        assert!(r.is_err(), "nanos >= 1_000_000 must throw");
    }

    #[test]
    fn t2_thread_sleep_accepts_max_nanos() {
        let mut ctx = mock_ctx();
        let r = native_thread_sleep_millis_nanos(
            &mut ctx,
            &[Value::Long(0), Value::Int(999_999)],
        );
        assert!(r.is_ok(), "nanos = 999_999 must be accepted");
    }
}

// ---------------------------------------------------------------------------
// Tests for T19.N2 — Thread.sleep0(J)V
// ---------------------------------------------------------------------------
#[cfg(test)]
mod t19_n2_thread_sleep0_tests {
    use super::*;
    use crate::test_utils::mock_ctx;

    /// 0ms must return essentially immediately (no actual sleep).
    #[test]
    fn t19_n2_thread_sleep0_zero_millis_returns_immediately() {
        let mut ctx = mock_ctx();
        let start = std::time::Instant::now();
        let r = native_thread_sleep0(&mut ctx, &[Value::Long(0)]);
        let elapsed = start.elapsed();
        assert!(r.is_ok(), "sleep0(0) should succeed, got {:?}", r);
        assert!(
            elapsed < std::time::Duration::from_millis(5),
            "sleep0(0) should return in < 5ms, took {:?}",
            elapsed
        );
    }

    /// 10ms must actually block for at least ~10ms (chunk size is 100ms,
    /// so a 10ms request sleeps for the full 10ms in one partial chunk).
    #[test]
    fn t19_n2_thread_sleep0_10ms_actually_sleeps() {
        let mut ctx = mock_ctx();
        let start = std::time::Instant::now();
        let r = native_thread_sleep0(&mut ctx, &[Value::Long(10)]);
        let elapsed = start.elapsed();
        assert!(r.is_ok(), "sleep0(10) should succeed, got {:?}", r);
        // Must have slept at least the requested amount.
        assert!(
            elapsed >= std::time::Duration::from_millis(10),
            "sleep0(10) should sleep ≥ 10ms, took {:?}",
            elapsed
        );
        // Must not have wildly overslept (generous upper bound for CI).
        assert!(
            elapsed <= std::time::Duration::from_millis(200),
            "sleep0(10) should return within ~200ms upper bound, took {:?}",
            elapsed
        );
    }

    /// Negative millis → IllegalArgumentException (defensive native check).
    #[test]
    fn t19_n2_thread_sleep0_negative_throws_illegal_argument() {
        let mut ctx = mock_ctx();
        let r = native_thread_sleep0(&mut ctx, &[Value::Long(-1)]);
        assert!(r.is_err(), "negative millis must throw");
        match r {
            Err(rustjvm_types::error::MethodCallFailed::InternalError(
                rustjvm_types::error::VmError::Runtime(
                    rustjvm_types::error::RuntimeError::IllegalArgumentException { message },
                ),
            )) => {
                assert!(
                    message.contains("negative"),
                    "expected 'negative' in message, got: {}",
                    message
                );
            }
            other => panic!(
                "expected IllegalArgumentException for negative millis, got {:?}",
                other
            ),
        }
    }

    /// When the interrupt flag is pre-set, sleep0(0) throws immediately.
    #[test]
    fn t19_n2_thread_sleep0_zero_millis_with_interrupt_throws() {
        let ctx = mock_ctx();
        ctx.set_interrupted(true);
        let mut ctx = ctx;
        let r = native_thread_sleep0(&mut ctx, &[Value::Long(0)]);
        assert!(r.is_err(), "sleep0(0) with interrupt flag must throw");
        // And the flag must be CLEARED per JDK spec.
        assert!(
            !ctx.is_interrupted(false),
            "interrupt flag must be cleared after InterruptedException",
        );
    }

    /// Interrupt delivered mid-sleep: set the flag on the context, then
    /// run sleep0 for a longer-than-chunk duration and verify that it
    /// returns within one chunk (≤ 110ms) with InterruptedException, and
    /// that the interrupt flag is cleared per JDK spec.
    ///
    /// We pre-set the flag because `MockNativeContext` is `!Sync`; the
    /// chunked poll at loop-top runs BEFORE the first `std::thread::sleep`,
    /// so a pre-set flag exercises the same code path as a flag that
    /// arrives during an earlier chunk.
    #[test]
    fn t19_n2_thread_sleep0_interrupt_during_sleep() {
        let ctx = mock_ctx();
        ctx.set_interrupted(true);
        let mut ctx = ctx;
        let start = std::time::Instant::now();
        // Request a 2-second sleep — if interrupt polling is broken, the
        // test will hang for ~2s (still fail, but visibly).
        let r = native_thread_sleep0(&mut ctx, &[Value::Long(2000)]);
        let elapsed = start.elapsed();
        assert!(r.is_err(), "sleep0 with interrupt must throw");
        match r {
            Err(rustjvm_types::error::MethodCallFailed::InternalError(
                rustjvm_types::error::VmError::Runtime(
                    rustjvm_types::error::RuntimeError::InterruptedException,
                ),
            )) => { /* expected */ }
            other => panic!("expected InterruptedException, got {:?}", other),
        }
        // Interrupt flag must be cleared per JDK spec.
        assert!(
            !ctx.is_interrupted(false),
            "interrupt flag must be cleared after InterruptedException",
        );
        // Poll happens at top of loop (before first chunk sleep), so the
        // flag is detected within well under one chunk (100ms). Allow
        // 110ms for CI jitter.
        assert!(
            elapsed <= std::time::Duration::from_millis(110),
            "interrupt should be detected within ≤110ms, took {:?}",
            elapsed
        );
    }
}

#[cfg(test)]
mod t14_tests {
    use super::*;
    use crate::test_utils::mock_ctx;

    // -----------------------------------------------------------------------
    // T14.1 — initPhase1
    // -----------------------------------------------------------------------

    #[test]
    fn init_phase1_succeeds() {
        let mut ctx = mock_ctx();
        // Ensure System class exists so ensure_class_initialized works
        let _ = ctx.ensure_class_initialized("java/lang/System").unwrap();
        let r = native_system_init_phase1(&mut ctx, &[]);
        assert!(r.is_ok());
        assert_eq!(r.unwrap(), None); // void return
    }

    // -----------------------------------------------------------------------
    // T14.2 — initPhase2
    // -----------------------------------------------------------------------

    #[test]
    fn init_phase2_returns_zero() {
        let mut ctx = mock_ctx();
        let r = native_system_init_phase2(&mut ctx, &[Value::Int(0), Value::Int(0)]);
        assert_eq!(r.unwrap(), Some(Value::Int(0)));
    }

    // -----------------------------------------------------------------------
    // T14.3 — initPhase3
    // -----------------------------------------------------------------------

    #[test]
    fn init_phase3_succeeds() {
        let mut ctx = mock_ctx();
        let r = native_system_init_phase3(&mut ctx, &[]);
        assert!(r.is_ok());
        assert_eq!(r.unwrap(), None); // void return
    }

    // -----------------------------------------------------------------------
    // T14.4 — VM.getSavedProperty
    // -----------------------------------------------------------------------

    #[test]
    fn vm_get_saved_property_returns_null_for_missing() {
        let mut ctx = mock_ctx();
        let key = ctx.create_string("nonexistent.property");
        let r = native_vm_get_saved_property(&mut ctx, &[Value::Object(Some(key))]);
        assert_eq!(r.unwrap(), Some(Value::Object(None)));
    }

    #[test]
    fn vm_get_saved_property_returns_value() {
        let mut ctx = mock_ctx();
        ctx.set_system_property("test.key", "test.value");
        let key = ctx.create_string("test.key");
        let r = native_vm_get_saved_property(&mut ctx, &[Value::Object(Some(key))]);
        let obj = match r.unwrap() {
            Some(Value::Object(Some(o))) => o,
            other => panic!("expected string Object, got {other:?}"),
        };
        assert_eq!(ctx.read_string(obj).unwrap(), "test.value");
    }

    #[test]
    fn vm_get_saved_property_null_key() {
        let mut ctx = mock_ctx();
        let r = native_vm_get_saved_property(&mut ctx, &[Value::Object(None)]);
        assert_eq!(r.unwrap(), Some(Value::Object(None)));
    }

    // -----------------------------------------------------------------------
    // T14.5 — VM.getRuntimeArguments
    // -----------------------------------------------------------------------

    #[test]
    fn vm_get_runtime_arguments_returns_empty_array() {
        let mut ctx = mock_ctx();
        let r = native_vm_get_runtime_arguments(&mut ctx, &[]);
        let arr = match r.unwrap() {
            Some(Value::Object(Some(a))) => a,
            other => panic!("expected array, got {other:?}"),
        };
        assert_eq!(ctx.array_length(arr), 0);
    }
}

#[cfg(test)]
mod t15_tests {
    use super::*;
    use crate::test_utils::mock_ctx;

    // -----------------------------------------------------------------------
    // T15.1.5 — Finalizer.register
    // -----------------------------------------------------------------------

    #[test]
    fn finalizer_register_with_object() {
        let mut ctx = mock_ctx();
        let obj = ctx.alloc_object(rustjvm_types::ClassId::new(0), 2);
        let r = native_finalizer_register(&mut ctx, &[Value::Object(Some(obj))]);
        assert!(r.is_ok());
        assert_eq!(r.unwrap(), None); // void
    }

    #[test]
    fn finalizer_register_with_null() {
        let mut ctx = mock_ctx();
        let r = native_finalizer_register(&mut ctx, &[Value::Object(None)]);
        assert!(r.is_ok()); // null is silently ignored
    }

    // -----------------------------------------------------------------------
    // T15.1.6 — Array.newArray
    // -----------------------------------------------------------------------

    #[test]
    fn array_new_array_int() {
        let mut ctx = mock_ctx();
        let mirror = ctx.create_string("int");
        let r = native_array_new_array(&mut ctx, &[Value::Object(Some(mirror)), Value::Int(5)]);
        let arr = match r.unwrap() {
            Some(Value::Object(Some(a))) => a,
            other => panic!("expected array, got {other:?}"),
        };
        assert_eq!(ctx.array_length(arr), 5);
    }

    #[test]
    fn array_new_array_negative_size() {
        let mut ctx = mock_ctx();
        let mirror = ctx.create_string("int");
        let r = native_array_new_array(&mut ctx, &[Value::Object(Some(mirror)), Value::Int(-1)]);
        assert!(r.is_err(), "negative size should throw NegativeArraySizeException");
    }

    // -----------------------------------------------------------------------
    // T15.1.3 — ClassLoader.defineClass1
    // -----------------------------------------------------------------------

    #[test]
    fn define_class1_empty_bytes_returns_null() {
        let mut ctx = mock_ctx();
        // Create a byte array with non-CAFEBABE bytes
        let arr = ctx.new_array(rustjvm_types::ArrayElementType::Byte, 4);
        ctx.set_array_element(arr, 0, Value::Int(0));
        ctx.set_array_element(arr, 1, Value::Int(0));
        ctx.set_array_element(arr, 2, Value::Int(0));
        ctx.set_array_element(arr, 3, Value::Int(0));
        let name = ctx.create_string("com/example/Foo");
        let r = native_classloader_define_class1(
            &mut ctx,
            &[
                Value::Object(None), // loader
                Value::Object(Some(name)),
                Value::Object(Some(arr)),
                Value::Int(0), // offset
                Value::Int(4), // length
                Value::Object(None), // pd
                Value::Object(None), // source
            ],
        );
        // Non-CAFEBABE bytes → define_class_from_bytes returns None → null
        assert_eq!(r.unwrap(), Some(Value::Object(None)));
    }

    #[test]
    fn define_class1_out_of_bounds() {
        let mut ctx = mock_ctx();
        let arr = ctx.new_array(rustjvm_types::ArrayElementType::Byte, 2);
        let name = ctx.create_string("com/example/Foo");
        let r = native_classloader_define_class1(
            &mut ctx,
            &[
                Value::Object(None),
                Value::Object(Some(name)),
                Value::Object(Some(arr)),
                Value::Int(0),
                Value::Int(10), // length > array size
                Value::Object(None),
                Value::Object(None),
            ],
        );
        assert!(r.is_err(), "should throw ArrayIndexOutOfBoundsException");
    }
}
