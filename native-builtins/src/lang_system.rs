// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! System, Runtime, ProcessBuilder, and Thread native method implementations.

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::{LinkageError, MethodCallFailed, MethodCallResult, RuntimeError};
use cratonvm_types::{ObjectRef, Value};

use crate::{alloc_concurrent_synthetic, obj_arg, platform_lib_name};

// ---------------------------------------------------------------------------
// System.exit / Runtime.exit pre-termination hook.
//
// `native_system_exit` and `native_runtime_exit` both `std::process::exit`
// after printing the `[cratonvm] System.exit(N) called` line. Once the
// process exits, downstream observers (dispatch_trace ring, watchdog
// printers) lose their chance to dump state. Crates higher up the
// dependency stack (e.g. `cratonvm-vm` / `vm-cli`) can register a pre-exit
// hook here so they get one last shot at printing diagnostics before
// we terminate.
//
// Wired by `vm-cli::main::run()` so a silent `System.exit(0)` during real
// app boot (e.g. Cassandra NodeTool's airline NPE catch path) at least
// dumps the dispatch_trace ring when `CRATONVM_DBG_EXIT=1` is set.
// ---------------------------------------------------------------------------
type PreExitHook = fn(code: i32);
static PRE_EXIT_HOOK: std::sync::OnceLock<PreExitHook> = std::sync::OnceLock::new();

/// Install a pre-`std::process::exit` callback that fires from
/// `native_system_exit` / `native_runtime_exit` immediately before the
/// process is torn down. Intended for diagnostic dumps (dispatch_trace
/// ring, last-N-bytecodes printout). First-installer wins вЂ” subsequent
/// calls are silent no-ops, matching `OnceLock` semantics.
pub fn set_pre_exit_hook(hook: PreExitHook) {
    let _ = PRE_EXIT_HOOK.set(hook);
}

fn invoke_pre_exit_hook(code: i32) {
    if let Some(hook) = PRE_EXIT_HOOK.get() {
        hook(code);
    }
}

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

pub(crate) fn native_system_arraycopy(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // args: src (Object), srcPos (int), dest (Object), destPos (int), length (int)
    let src = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NullPointerException {
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
            return Err(cratonvm_types::error::RuntimeError::NullPointerException {
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
    use cratonvm_types::ObjectKind;
    if ctx.heap_kind_of(src) != ObjectKind::Array {
        return Err(cratonvm_types::error::RuntimeError::ArrayStoreException {
            message: "arraycopy: src is not an array".to_string(),
        }
        .into());
    }
    if ctx.heap_kind_of(dest) != ObjectKind::Array {
        return Err(cratonvm_types::error::RuntimeError::ArrayStoreException {
            message: "arraycopy: dest is not an array".to_string(),
        }
        .into());
    }

    // Bounds checking
    let src_len = ctx.array_length(src) as i32;
    let dest_len = ctx.array_length(dest) as i32;

    // SECURITY FIX (V14): widen the end-offset additions to i64 so that
    // `src_pos + length` / `dest_pos + length` cannot wrap to a negative
    // value (Rust `+` wraps in release builds) and silently defeat the
    // `> len` bounds check. The individual >= 0 checks and the
    // ArrayIndexOutOfBoundsException semantics are preserved.
    let src_end = src_pos as i64 + length as i64;
    let dest_end = dest_pos as i64 + length as i64;
    if src_pos < 0
        || dest_pos < 0
        || length < 0
        || src_end > src_len as i64
        || dest_end > dest_len as i64
    {
        return Err(
            cratonvm_types::error::RuntimeError::ArrayIndexOutOfBoundsException {
                index: if src_pos < 0 {
                    src_pos
                } else if dest_pos < 0 {
                    dest_pos
                } else if src_end > src_len as i64 {
                    src_end as i32
                } else {
                    dest_end as i32
                },
            }
            .into(),
        );
    }

    if length == 0 {
        return Ok(None);
    }

    // Element-type compatibility.
    //
    // Three cases:
    //  1. Both primitive arrays of the same element type в†’ bulk-safe copy.
    //  2. One primitive, the other reference (or two primitives of
    //     different kinds) в†’ bulk-reject with ArrayStoreException.
    //  3. Both reference arrays в†’ per-element assignability check against
    //     the destination component class, with prefix-commit on failure
    //     (JLS В§5.5 / `java.lang.System.arraycopy` contract).
    use cratonvm_types::ArrayElementType;
    let src_elem = ctx.heap_element_type_of(src);
    let dest_elem = ctx.heap_element_type_of(dest);
    if src_elem != dest_elem {
        if src_elem == ArrayElementType::Char
            && dest_elem == ArrayElementType::Byte
            && is_abstract_string_builder_capacity_copy(ctx)
        {
            for i in 0..length {
                let val = ctx.get_array_element(src, (src_pos + i) as usize);
                let byte = match val {
                    Value::Int(v) => v & 0xff,
                    _ => 0,
                };
                ctx.set_array_element(dest, (dest_pos + i) as usize, Value::Int(byte));
            }
            return Ok(None);
        }

        // CRATONVM_DBG_ARRAYCOPY=1 вЂ” dump the Java caller chain + array
        // identities for the element-type mismatch. Env-gated; default output
        // unchanged. Used to localize the Hibernate/H2 "src=Char, dest=Byte"
        // cluster (an array mislabeled at its allocation site).
        if matches!(src_elem, ArrayElementType::Byte | ArrayElementType::Boolean)
            && dest_elem == ArrayElementType::Char
            && is_abstract_string_builder_append_copy(ctx)
        {
            for i in 0..length {
                let val = ctx.get_array_element(src, (src_pos + i) as usize);
                let ch = match val {
                    Value::Int(v) => v & 0xff,
                    _ => 0,
                };
                ctx.set_array_element(dest, (dest_pos + i) as usize, Value::Int(ch));
            }
            return Ok(None);
        }
        if std::env::var("CRATONVM_DBG_ARRAYCOPY").as_deref() == Ok("1") {
            let src_cls = ctx.class_id_of_object(src);
            let dest_cls = ctx.class_id_of_object(dest);
            let src_name = ctx.class_name_of_id(src_cls).unwrap_or_default();
            let dest_name = ctx.class_name_of_id(dest_cls).unwrap_or_default();
            let trace = ctx.capture_stack_trace(0);
            let total = trace.len();
            // Show the INNERMOST ~40 frames (closest to the arraycopy call
            // site); the trace is outermost-first so the tail is what matters.
            let skip = total.saturating_sub(40);
            let mut rendered = String::new();
            for (i, entry) in trace.iter().enumerate().skip(skip) {
                use std::fmt::Write as _;
                let _ = write!(
                    rendered,
                    "\n  #{i}/{total} {cls}.{m} (bci={bci})",
                    cls = entry.class_name,
                    m = entry.method_name,
                    bci = entry.byte_code_index,
                );
            }
            tracing::warn!(
                target: "cratonvm::arraycopy",
                "[DBG_ARRAYCOPY] mismatch src={src_elem:?}({src_name}) dest={dest_elem:?}({dest_name}) \
                 srcLen={src_len} destLen={dest_len} len={length} caller chain:{rendered}"
            );
        }
        return Err(cratonvm_types::error::RuntimeError::ArrayStoreException {
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
        let src_elem_class = ctx.class_id_of_object(src);
        let object_class_id = ctx.class_id_by_name("java/lang/Object");

        // Fast path: src and dest reference arrays have the *same* component
        // class. Every element already stored in `src` is, by construction,
        // a valid value for a `src` component slot, hence also valid for an
        // identically-typed `dst` slot. No per-element check is needed.
        //
        // This is the structural fix for arrays-of-arrays. For `int[][]`
        // the element objects are primitive `int[]` arrays which all carry
        // the synthetic `ClassId::new(0)` (primitive arrays are allocated
        // with class id 0 вЂ” see `Newarray`/`alloc_multi_array` in the
        // interpreter), while the dest array's stored component class id is
        // the real `[I` class. The old per-element check compared the
        // element's class id (0) against the dest component class id (`[I`)
        // and wrongly threw `ArrayStoreException`. Comparing the *array*
        // component class ids (`class_id_of_object(src/dest)`) sidesteps
        // that mismatch: identical component class id в‡’ assignable.
        if src_elem_class == dst_elem_class {
            // Same component type вЂ” copy without per-element checks.
            if same_array && src_pos < dest_pos {
                for i in (0..length).rev() {
                    let val = ctx.get_array_element(src, (src_pos + i) as usize);
                    ctx.set_array_element(dest, (dest_pos + i) as usize, val);
                }
            } else {
                for i in 0..length {
                    let val = ctx.get_array_element(src, (src_pos + i) as usize);
                    ctx.set_array_element(dest, (dest_pos + i) as usize, val);
                }
            }
            return Ok(None);
        }

        // Per-element assignability: matches the established pattern in
        // `native_class_is_assignable_from` /
        // `native_class_is_instance` вЂ” identity OR `is_subclass` (which
        // walks both the superclass chain AND implemented interfaces, so
        // it correctly handles dest-element-type-is-interface cases like
        // `Runnable[]`).
        let assignable_to_dst = |ctx: &mut dyn NativeContext, elem: ObjectRef| -> bool {
            if Some(dst_elem_class) == object_class_id {
                // Fast path: every reference is assignable to Object.
                return true;
            }
            let elem_class = ctx.class_id_of_object(elem);
            if elem_class == dst_elem_class || ctx.is_subclass(elem_class, dst_elem_class) {
                return true;
            }
            // Array-typed elements: a primitive array (`int[]`, `byte[]`,
            // вЂ¦) carries the synthetic `ClassId::new(0)`, so the class-id
            // comparison above can never match a real array component
            // class. Fall back to a structural descriptor comparison so
            // e.g. an `int[]` element is accepted into an `int[][]` whose
            // component class is the loaded `[I` class.
            if ctx.heap_kind_of(elem) == ObjectKind::Array {
                if let Some(dst_name) = ctx.class_name_of_id(dst_elem_class) {
                    // dst component is itself an array type.
                    if dst_name.starts_with('[') {
                        return true;
                    }
                }
            }
            false
        };

        // For same-array overlap with `src_pos < dest_pos` the actual
        // copy must run backward (highв†’low) so we don't clobber unread
        // source slots. Doing per-element check-then-write in that
        // direction would let an early write corrupt source data read
        // later, breaking the type check. So in that case we do a
        // forward type-CHECK-only pre-pass first (no writes); on failure
        // we throw with NO elements written (an empty prefix вЂ” still
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
            // Forward type-check pre-pass вЂ” no writes.
            for i in 0..length {
                let val = ctx.get_array_element(src, (src_pos + i) as usize);
                if let Value::Object(Some(elem)) = val {
                    if !assignable_to_dst(ctx, elem) {
                        return Err(cratonvm_types::error::RuntimeError::ArrayStoreException {
                            message: format!(
                                "arraycopy: source element at index {} is not assignable to destination component type",
                                src_pos + i
                            ),
                        }
                        .into());
                    }
                }
            }
            // Pre-check passed вЂ” copy backward to handle overlap.
            for i in (0..length).rev() {
                let val = ctx.get_array_element(src, (src_pos + i) as usize);
                ctx.set_array_element(dest, (dest_pos + i) as usize, val);
            }
        } else {
            // Forward direction is safe вЂ” check-then-write per element.
            for i in 0..length {
                let val = ctx.get_array_element(src, (src_pos + i) as usize);
                if let Value::Object(Some(elem)) = val {
                    if !assignable_to_dst(ctx, elem) {
                        // Prefix [0, i) at positions `dest_pos..dest_pos+i`
                        // has already been written. This is the spec
                        // partial-commit behavior.
                        return Err(cratonvm_types::error::RuntimeError::ArrayStoreException {
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

    // Primitive-array fast path вЂ” element types already verified equal.
    //
    // Use the `bulk_array_copy` intrinsic exposed by `NativeContext`
    // (added round 3 вЂ” see `native-api/src/registry.rs:270`). The VM
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
    if ctx.bulk_array_copy(
        src,
        src_pos as usize,
        dest,
        dest_pos as usize,
        length as usize,
    ) {
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

fn is_abstract_string_builder_capacity_copy(ctx: &mut dyn NativeContext) -> bool {
    let trace = ctx.capture_stack_trace(0);
    let mut string_ctor = false;
    let mut string_builder_to_string = false;

    for entry in &trace {
        let class_name = entry.class_name.as_ref();
        let method_name = entry.method_name.as_ref();
        if class_name == "java/lang/AbstractStringBuilder"
            && method_name == "ensureCapacityNewCoder"
        {
            return true;
        }
        if class_name == "java/lang/String" && method_name == "<init>" {
            string_ctor = true;
        }
        if class_name == "java/lang/StringBuilder" && method_name == "toString" {
            string_builder_to_string = true;
        }
    }

    string_ctor && string_builder_to_string
}

fn is_abstract_string_builder_append_copy(ctx: &mut dyn NativeContext) -> bool {
    let trace = ctx.capture_stack_trace(0);
    let mut string_get_bytes = false;
    let mut asb_append = false;

    for entry in &trace {
        let class_name = entry.class_name.as_ref();
        let method_name = entry.method_name.as_ref();
        if class_name == "java/lang/String" && method_name == "getBytes" {
            string_get_bytes = true;
        }
        if class_name == "java/lang/AbstractStringBuilder" && method_name == "append" {
            asb_append = true;
        }
    }

    string_get_bytes && asb_append
}

pub(crate) fn native_thread_current_thread(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    let thread_obj = ctx.current_thread_object();
    Ok(Some(Value::Object(Some(thread_obj))))
}

/// T2.2.21: `Thread.sleep(long millis, int nanos)`.
///
/// The public `Thread.sleep(long, int)` overload validates its arguments
/// and rounds sub-millisecond `nanos` up by one millisecond (matching
/// HotSpot's behavior вЂ” the JDK's java-side implementation does the same
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
        return Err(
            cratonvm_types::error::RuntimeError::IllegalArgumentException {
                message: "Thread.sleep: timeout value is negative".to_string(),
            }
            .into(),
        );
    }
    if !(0..=999_999).contains(&nanos) {
        return Err(
            cratonvm_types::error::RuntimeError::IllegalArgumentException {
                message: "Thread.sleep: nanosecond timeout value out of range".to_string(),
            }
            .into(),
        );
    }
    // RD.9: combine millis + nanos into a single nanosecond value and delegate
    // to `sleepNanos0` so sub-millisecond sleeps honour the requested
    // precision (the previous round-up-to-millis path lost precision for
    // sub-ms sleeps вЂ” Thread.sleep(0, 500_000) used to block for 1ms).
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
    // WP4.5 вЂ” invokestatic uses generic `pop()` which type-erases the Long
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
        // Keycloak Gap 9 localization (CRATONVM_DBG_SLEEP_TRACE): a worker is
        // stuck in a Thread.sleep poll-loop; sample the Java caller chain so we
        // can identify which loop and what it polls. Sampled + capped to avoid
        // flooding; off by default (one env check per real sleep call).
        if std::env::var_os("CRATONVM_DBG_SLEEP_TRACE").is_some() {
            use std::sync::atomic::{AtomicUsize, Ordering};
            static N: AtomicUsize = AtomicUsize::new(0);
            static PRINTED: AtomicUsize = AtomicUsize::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            if n % 16 == 0 && PRINTED.fetch_add(1, Ordering::Relaxed) < 60 {
                let st = ctx.capture_stack_trace(0);
                let frames: Vec<String> = st
                    .iter()
                    .take(12)
                    .map(|e| format!("{}.{}:{}", e.class_name, e.method_name, e.line_number))
                    .collect();
                eprintln!("[SLEEP-TRACE #{n} millis={millis}] {}", frames.join(" <- "));
            }
        }
        // Check interrupted before sleeping
        if ctx.is_interrupted(true) {
            return Err(cratonvm_types::error::MethodCallFailed::InternalError(
                cratonvm_types::error::VmError::Runtime(
                    cratonvm_types::error::RuntimeError::InterruptedException,
                ),
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
        // WP4.5 вЂ” pump in 10ms slices so any
        // `ScheduledExecutorService.scheduleAtFixedRate` registrations
        // get a chance to fire while the caller is asleep. The pump
        // is a no-op when the registry is empty so the unmodified
        // sleep cost is just one Mutex::lock per slice.
        let pump_slice = std::time::Duration::from_millis(10);
        // CompletableFuture.runAsync users commonly use short sleeps as
        // scheduling barriers. CratonVM's workers start quickly, but cold Java
        // proxy/linkage and real CompletableFuture submission overhead can make
        // 10 ms handoff sleeps and 100 ms worker sleeps too tight. When a sleep
        // immediately follows async submission, give those short sleeps a small
        // scheduling floor; Thread.sleep only promises to sleep at least the
        // requested duration.
        let effective_millis = crate::async_handoff_sleep_millis(millis);
        let target = std::time::Duration::from_millis(effective_millis as u64);
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
            ctx.begin_blocking_region();
            std::thread::sleep(remaining.min(pump_slice));
            ctx.end_blocking_region();
        }
        let actual_dur = sleep_start.elapsed();
        if release {
            ctx.vt_acquire_carrier();
        }
        ctx.record_thread_sleep(millis * 1_000_000, actual_dur.as_nanos() as u64);
        // Check interrupted after sleeping (with clear).
        let interrupted_after_sleep = ctx.is_interrupted(true);
        if interrupted || interrupted_after_sleep {
            return Err(cratonvm_types::error::MethodCallFailed::InternalError(
                cratonvm_types::error::VmError::Runtime(
                    cratonvm_types::error::RuntimeError::InterruptedException,
                ),
            ));
        }
    }
    Ok(None)
}

/// `Thread.getState()` / `Thread.threadState()` for real-JDK mode.
///
/// The JDK bytecode computes the state from `holder.threadStatus`, but the VM
/// never advances that field past 0 (NEW) вЂ” so the real bytecode reports NEW
/// for every thread, including ones that have finished. Strict thread-leak
/// detectors (randomizedtesting's `ThreadLeakControl`, used by the Elasticsearch
/// RestClient suite) then see a finished worker as a live NEW thread and fail
/// with a spurious `ThreadLeakError`. We compute the state from the
/// authoritative VM thread registry instead and return the **canonical**
/// `Thread$State` enum constant (callers compare it with `==`).
pub(crate) fn native_thread_get_state(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let name = match ctx.thread_run_state(this) {
        1 => "RUNNABLE",
        2 => "TERMINATED",
        _ => "NEW",
    };
    let cid = match ctx.ensure_class_initialized("java/lang/Thread$State") {
        Ok(c) => c,
        Err(_) => return Ok(Some(Value::Object(None))),
    };
    if let Some(idx) = ctx.static_field_index_by_name(cid, name) {
        return Ok(Some(ctx.get_static_field(cid, idx)));
    }
    Ok(Some(Value::Object(None)))
}

pub(crate) fn native_thread_is_alive(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let alive = if ctx.thread_is_alive(this) { 1 } else { 0 };
    Ok(Some(Value::Int(alive)))
}

pub(crate) fn native_thread_start0(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    // Round-7 CRIT fix #3: snapshot the parent's `InheritableThreadLocal`
    // entries and queue them against the child's Java Thread identity
    // hash. The child's first `ThreadLocal.get/set/remove` will drain
    // the snapshot into its own TL_MAP (see
    // `drain_inherited_for_current_thread` in phases_early.rs). We do
    // this *before* spawning so there's no race between parent's
    // post-start mutations and the child's drain.
    if let Some(snap) = crate::phases_early::snapshot_inheritable_tl_entries(ctx) {
        let child_hash = ctx.identity_hash_code(this);
        crate::phases_early::queue_inherited_tl_for_child(child_hash, snap);
    }
    // TC0622: inherit the parent (creating) thread's context classloader into
    // the child, mirroring real JDK's `Thread.<init>`, which assigns
    // `this.contextClassLoader = parent.getContextClassLoader()`. CratonVM's
    // construction path does not propagate it: the synthetic `<init>` overrides
    // (synthetic-JDK mode) only set name/priority/target/group, and the real
    // `Thread.<init>` bytecode (real-JDK mode) leaves the child's field null.
    // A null `contextClassLoader` makes `Thread.getContextClassLoader()` fall
    // back to the app loader, so Tomcat's
    // `WebappClassLoaderBase.clearReferencesThreads` вЂ” which only stops a thread
    // when `thread.getContextClassLoader() == webappLoader` вЂ” skips leaked
    // app-spawned threads (e.g. `java.util.TimerThread`) and they stay alive.
    // Do this here, on the parent thread, before the child runs, and only when
    // the child has no CCL of its own (preserve an explicit
    // `setContextClassLoader` issued before `start()`). Opt-out:
    // `CRATONVM_INHERIT_THREAD_CCL=0` restores the prior (no-inherit) behavior.
    let inherit_ccl = match std::env::var("CRATONVM_INHERIT_THREAD_CCL") {
        Ok(v) => v != "0" && !v.eq_ignore_ascii_case("false"),
        Err(_) => true,
    };
    if inherit_ccl
        && !matches!(
            ctx.get_field_by_name(this, "contextClassLoader"),
            Value::Object(Some(_))
        )
    {
        let parent = ctx.current_thread_object();
        if let Value::Object(Some(parent_ccl)) = ctx.get_field_by_name(parent, "contextClassLoader")
        {
            ctx.set_field_by_name(this, "contextClassLoader", Value::Object(Some(parent_ccl)));
        }
    }
    // test-context-round2: real JDK `Thread.<init>`'s InheritableThreadLocal
    // copy (`this.inheritableThreadLocals = ThreadLocal.createInheritedMap(
    // parent.inheritableThreadLocals)`) silently doesn't take effect for a
    // still-unexplained interpreter reason specifically when BOTH the
    // ThreadGroup and name constructor arguments are explicitly non-null at
    // the same time — confirmed via minimal repro: `Thread(Runnable)` and
    // `Thread(ThreadGroup, Runnable)` alone each correctly propagate;
    // `Thread(ThreadGroup, Runnable, String[, long])` — exactly what
    // `Executors.defaultThreadFactory()` uses for every pooled worker —
    // does not, losing the parent's InheritableThreadLocal values entirely
    // (name/group/priority/contextClassLoader are all unaffected; only this
    // one field is dropped). The two-condition trigger rules out a native
    // registration gap (the constructors aren't natively overridden at all;
    // this is real bytecode misbehaving) — root-causing it further needs
    // interpreter-level bytecode tracing, out of scope here. Apply the copy
    // here at start0-time instead, mirroring the TC0622 CCL fix above.
    //
    // Known trade-off: `characteristics` (which flags an explicit
    // `Thread(group, target, name, stackSize, false)` opt-out of
    // inheritance) isn't retained anywhere observable post-construction, so
    // this can't distinguish "buggy" from "intentionally opted out". A
    // legitimate opt-out combined with non-null group+name would incorrectly
    // regain inheritance. That combination is rare in practice (the 5-arg
    // opt-out constructor itself is rarely used); documented pending a real
    // interpreter-level fix. Opt-out: `CRATONVM_INHERIT_TL_WORKAROUND=0`.
    let apply_itl_workaround = match std::env::var("CRATONVM_INHERIT_TL_WORKAROUND") {
        Ok(v) => v != "0" && !v.eq_ignore_ascii_case("false"),
        Err(_) => true,
    };
    // The buggy path doesn't leave this field as `Object(None)` (the normal
    // "never written" value real bytecode `getfield` would observe) — a raw
    // native heap read here sees `Int(0)` instead, matching CratonVM's
    // zero-fill representation for a slot the constructor's `putfield`
    // (offset 201 in the real master constructor) never actually reached.
    // Treat either as "not yet inherited".
    let child_itl_unset = matches!(
        ctx.get_field_by_name(this, "inheritableThreadLocals"),
        Value::Object(None) | Value::Int(0)
    );
    if apply_itl_workaround && child_itl_unset {
        let parent = ctx.current_thread_object();
        if let Value::Object(Some(parent_map)) =
            ctx.get_field_by_name(parent, "inheritableThreadLocals")
        {
            if let Ok(Some(new_map)) = ctx.invoke(
                "java/lang/ThreadLocal",
                "createInheritedMap",
                "(Ljava/lang/ThreadLocal$ThreadLocalMap;)Ljava/lang/ThreadLocal$ThreadLocalMap;",
                &[Value::Object(Some(parent_map))],
            ) {
                ctx.set_field_by_name(this, "inheritableThreadLocals", new_map);
            }
        }
    }
    ctx.thread_start(this)
}

pub(crate) fn native_thread_join(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    ctx.thread_join(this)
}

pub(crate) fn native_thread_join_timed(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    let millis = match args.get(1) {
        Some(Value::Long(ms)) => *ms,
        _ => 0,
    };
    if millis < 0 {
        return Err(
            cratonvm_types::error::RuntimeError::IllegalArgumentException {
                message: "Thread.join: timeout value is negative".to_string(),
            }
            .into(),
        );
    }
    if millis == 0 {
        // join(0) means wait forever (same as join())
        return ctx.thread_join(this);
    }
    // RD.10: timed join вЂ” return after the specified timeout even if the
    // target thread is still alive. Poll isAlive at a small cadence so we
    // don't block beyond the deadline.
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(millis as u64);
    if std::env::var_os("CRATONVM_DBG_SLEEP_TRACE").is_some() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static N: AtomicUsize = AtomicUsize::new(0);
        if N.fetch_add(1, Ordering::Relaxed) % 64 == 0 {
            let st = ctx.capture_stack_trace(0);
            let frames: Vec<String> = st
                .iter()
                .take(10)
                .map(|e| format!("{}.{}:{}", e.class_name, e.method_name, e.line_number))
                .collect();
            eprintln!("[JOIN-TIMED-TRACE millis={millis}] {}", frames.join(" <- "));
        }
    }
    loop {
        if !ctx.thread_is_alive(this) {
            break;
        }
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        // Honour an interrupt that arrived while we were waiting вЂ” throw
        // InterruptedException so caller code behaves like HotSpot.
        if ctx.is_interrupted(true) {
            return Err(cratonvm_types::error::MethodCallFailed::InternalError(
                cratonvm_types::error::VmError::Runtime(
                    cratonvm_types::error::RuntimeError::InterruptedException,
                ),
            ));
        }
        let sleep_time = remaining.min(std::time::Duration::from_millis(2));
        let mut blocked_refs = [Value::Object(Some(this))];
        ctx.begin_blocking_region();
        std::thread::sleep(sleep_time);
        ctx.end_blocking_region_refs(&mut blocked_refs);
        if let Value::Object(Some(cur)) = blocked_refs[0] {
            this = cur;
        }
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
        return Err(
            cratonvm_types::error::RuntimeError::IllegalArgumentException {
                message: "Thread.join: timeout value is negative".to_string(),
            }
            .into(),
        );
    }
    if !(0..=999_999).contains(&nanos) {
        return Err(
            cratonvm_types::error::RuntimeError::IllegalArgumentException {
                message: "Thread.join: nanosecond timeout value out of range".to_string(),
            }
            .into(),
        );
    }
    let effective_ms = if nanos > 0 {
        millis.saturating_add(1)
    } else {
        millis
    };
    let this = args.first().copied().unwrap_or(Value::Object(None));
    native_thread_join_timed(ctx, &[this, Value::Long(effective_ms)])
}

pub(crate) fn native_thread_interrupt(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    // Set the VM-side interrupt atomic (what LockSupport.park / Object.wait /
    // Condition.await poll to wake).
    ctx.thread_interrupt(this);
    // Mirror onto the real `java.lang.Thread.interrupted` boolean FIELD. In
    // real-JDK mode `Thread.isInterrupted()` and the static `Thread.interrupted()`
    // run real bytecode that reads this field (not the VM atomic) вЂ” and AQS's
    // ConditionObject.checkInterruptWhileWaiting calls the static
    // `Thread.interrupted()`. Without mirroring, a thread woken from
    // LockSupport.park by an interrupt sees `interrupted == false`, so the AQS
    // await loop never detects the interrupt and re-parks forever
    // (LinkedBlockingQueue.take inside ThreadPoolExecutor.getTask в†’ shutdownNow
    // can't stop the worker в†’ leaked non-daemon thread hangs the VM). The
    // clear side is handled by `clearInterruptEvent` (which static
    // `Thread.interrupted()` calls right after clearing the field). A synthetic
    // Thread without this field resolves to a no-op set.
    ctx.set_field_by_name(this, "interrupted", Value::Int(1));
    Ok(None)
}

pub(crate) fn native_thread_get_name(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            let name = ctx.create_string("main");
            return Ok(Some(Value::Object(Some(name))));
        }
    };
    match ctx.get_field_by_name(this, "name") {
        Value::Object(Some(str_ref)) => Ok(Some(Value::Object(Some(str_ref)))),
        _ => match ctx.get_field(this, 0) {
            Value::Object(Some(str_ref)) => Ok(Some(Value::Object(Some(str_ref)))),
            _ => {
                let name = ctx.create_string("main");
                Ok(Some(Value::Object(Some(name))))
            }
        },
    }
}

pub(crate) fn native_thread_is_interrupted(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // args[0] = this, args[1] = boolean clearInterrupted
    let clear = matches!(args.get(1), Some(Value::Int(1)));
    let interrupted = if ctx.is_interrupted(clear) { 1 } else { 0 };
    Ok(Some(Value::Int(interrupted)))
}

// ---------------------------------------------------------------------------
// Step 4: System properties + utilities
// ---------------------------------------------------------------------------

pub(crate) fn native_system_get_property(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let key_obj = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key = crate::property_key_from_java_string(ctx, key_obj);
    match ctx
        .get_system_property(&key)
        .or_else(|| crate::system_property_fallback(ctx, &key))
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
        .or_else(|| crate::system_property_fallback(ctx, &key))
    {
        Some(val) => {
            let result = ctx.create_string(&val);
            Ok(Some(Value::Object(Some(result))))
        }
        None => Ok(Some(default)),
    }
}

pub(crate) fn native_system_set_property(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
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

pub(crate) fn native_system_nano_time(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
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

    // CRATONVM_DBG_EXIT=1 вЂ” capture and log the Java caller chain BEFORE we
    // either soft-return or terminate. Helps identify which class/method in
    // the upstream code invoked System.exit. Env-gated so default output is
    // unchanged.
    if std::env::var("CRATONVM_DBG_EXIT").as_deref() == Ok("1") {
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
            target: "cratonvm::system_exit",
            "[CRATONVM_DBG_EXIT] System.exit({code}) caller chain:{rendered}"
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
            target: "cratonvm::system_exit",
            "[cratonvm] Soft-returning ForkedBooter.exit(1) guard"
        );
        return Ok(None);
    }

    // CRATONVM_SOFT_EXIT=1 вЂ” opt-in. Convert ANY System.exit(I)V into a soft
    // return so the calling Java frame keeps executing (and `main` can reach
    // further). Used to expose downstream failures hidden behind an explicit
    // upstream exit. Default behaviour (env unset) is unchanged: terminate.
    if std::env::var("CRATONVM_SOFT_EXIT").as_deref() == Ok("1") {
        tracing::warn!(
            target: "cratonvm::system_exit",
            "[cratonvm] System.exit({code}) soft-returned (CRATONVM_SOFT_EXIT=1)"
        );
        return Ok(None);
    }

    // B6: Surface System.exit calls вЂ” Kotlin/Scala programs often reach exit
    // via an uncaught-exception handler after some earlier failure that would
    // otherwise be invisible. Log to stderr directly since tracing may not be
    // flushed before process::exit.
    eprintln!("[cratonvm] System.exit({code}) called вЂ” process terminating");
    invoke_pre_exit_hook(code);
    std::process::exit(code);
}

// ---------------------------------------------------------------------------
// Phase 13 Step 4: Runtime + System.lineSeparator
// ---------------------------------------------------------------------------

pub(crate) fn native_system_line_separator(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
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
    registry.register(
        "java/lang/Runtime$Version",
        "build",
        "()Ljava/util/Optional;",
        native_runtime_version_build,
    );
    registry.register(
        "java/lang/Runtime",
        "addShutdownHook",
        "(Ljava/lang/Thread;)V",
        |_ctx, _args| Ok(None),
    );
    registry.register(
        "java/lang/Runtime",
        "removeShutdownHook",
        "(Ljava/lang/Thread;)Z",
        |_ctx, _args| Ok(Some(Value::Int(1))),
    );
    registry.register("java/lang/Runtime", "gc", "()V", |ctx, _args| {
        ctx.force_gc();
        Ok(None)
    });
    registry.register("java/lang/Runtime", "exit", "(I)V", native_runtime_exit);

    // Runtime.loadLibrary(String) / Runtime.load(String) вЂ” JNI library loading
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

    // System.loadLibrary / System.load вЂ” delegate to the same machinery
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

pub(crate) fn native_runtime_get_runtime(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    let class_id = match ctx.ensure_class_initialized("java/lang/Runtime") {
        Ok(id) => id,
        Err(_) => ctx.ensure_synthetic_class("java/lang/Runtime", 8),
    };
    let obj = ctx.alloc_object(class_id, 0);
    Ok(Some(Value::Object(Some(obj))))
}

pub(crate) fn native_runtime_available_processors(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    // Container-aware: respects the cgroup CPU quota under
    // `-XX:+UseContainerSupport` (falls back to the host thread count).
    Ok(Some(Value::Int(ctx.available_processor_count())))
}

pub(crate) fn native_runtime_max_memory(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    // Report the configured `-Xmx` (container-aware once sized from a cgroup
    // limit) instead of a hardcoded 256 MB, matching HotSpot's contract that
    // `maxMemory()` reflects the actual heap ceiling.
    Ok(Some(Value::Long(ctx.max_heap_bytes())))
}

pub(crate) fn native_runtime_total_memory(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(Some(Value::Long(64 * 1024 * 1024))) // 64 MB estimate
}

pub(crate) fn native_runtime_free_memory(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(Some(Value::Long(32 * 1024 * 1024))) // 32 MB estimate
}

/// `Runtime.version()` returns a real initialized `Runtime$Version`.
///
/// The old lightweight object only satisfied native `feature()`/`build()` calls.
/// Real bytecode such as `Runtime$Version.toString()` reads the private final
/// `version` list, so construct via the JDK parser instead of returning raw
/// zeroed fields.
pub(crate) fn native_runtime_version(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    let version = ctx
        .get_system_property("java.version")
        .or_else(|| ctx.get_system_property("java.specification.version"))
        .unwrap_or_else(|| "25".to_string());
    let version_obj = ctx.create_string(version.trim());
    let pin = ctx.pin_native_root(version_obj);
    let arg = Value::Object(Some(ctx.read_native_pin(pin, version_obj)));
    let result = ctx.invoke(
        "java/lang/Runtime$Version",
        "parse",
        "(Ljava/lang/String;)Ljava/lang/Runtime$Version;",
        &[arg],
    );
    ctx.unpin_native_roots(pin);
    result
}

/// `Runtime.Version.feature()` вЂ” major Java specification version (e.g. 25).
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

/// `Runtime.Version.build()` - optional build number.
///
/// CratonVM's lightweight `Runtime.version()` object does not populate the real
/// JDK `build` field, so the real accessor would return null. Returning
/// `Optional.empty()` matches a valid version object and keeps callers from
/// treating the VM metadata as malformed.
pub(crate) fn native_runtime_version_build(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    ctx.invoke("java/util/Optional", "empty", "()Ljava/util/Optional;", &[])
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
    if std::env::var("CRATONVM_DBG_EXIT").as_deref() == Ok("1") {
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
            target: "cratonvm::system_exit",
            "[CRATONVM_DBG_EXIT] Runtime.exit({code}) caller chain:{rendered}"
        );
    }

    if std::env::var("CRATONVM_SOFT_EXIT").as_deref() == Ok("1") {
        tracing::warn!(
            target: "cratonvm::system_exit",
            "[cratonvm] Runtime.exit({code}) soft-returned (CRATONVM_SOFT_EXIT=1)"
        );
        return Ok(None);
    }

    // B6: Surface Runtime.exit calls so silent shutdowns are visible.
    eprintln!("[cratonvm] Runtime.exit({code}) called вЂ” process terminating");
    invoke_pre_exit_hook(code);
    std::process::exit(code);
}

// ---------------------------------------------------------------------------
// Runtime.exec вЂ” spawn subprocesses via std::process::Command
// Process synthetic: 3-field (exit_code=0 Int, stdout=1 String, stderr=2 String)
// ---------------------------------------------------------------------------

/// Consult the installed `java.lang.SecurityManager`, if any, before
/// spawning a host process. Mirrors HotSpot's behaviour: every
/// `ProcessBuilder.start` and `Runtime.exec*` overload must call
/// `SecurityManager.checkExec(command[0])` before the spawn syscall
/// (fork/exec/CreateProcess) is issued. If the SM throws
/// `SecurityException`, the error is propagated to the Java caller and
/// the spawn MUST NOT happen.
///
/// `command_first` is the program path (`command[0]`) as it will be
/// handed to `std::process::Command::new`. An empty string is rejected
/// up-front so a misuse on the SM side (treating `""` as "allow
/// nothing") can't be bypassed by passing an empty argv.
///
/// With no SecurityManager installed this is a no-op вЂ” matching JDK
/// behaviour where `Runtime.exec` is unrestricted until `System.setSecurityManager`
/// is called.
///
/// Audit TODO (Panama): host-call sites that go through `jdk.internal.foreign`
/// / `java.lang.foreign.Linker` can invoke `execve`/`CreateProcessW`
/// without ever transiting `ProcessBuilder.start` or `Runtime.exec`.
/// That bypass is not addressed here вЂ” gating it requires intercepting
/// every Panama downcall, tracked as a separate task. See
/// `native-builtins::panama` for the FFI entry points.
pub(crate) fn check_exec_or_throw(
    ctx: &mut dyn NativeContext,
    command_first: &str,
) -> Result<(), cratonvm_types::error::MethodCallFailed> {
    // Pass ctx so the singleton read re-fetches the CURRENT (post-GC) address
    // via the var-handle-root registry (raw static copies are never remapped).
    let sm = crate::security_manager::get_security_manager(&*ctx);
    let Some(sm_ref) = sm else { return Ok(()) };

    // Allocate a Java String for command[0] and call sm.checkExec(String).
    // The synthetic SecurityManager.checkExec native (security_manager.rs)
    // routes through checkPermission в†’ policy_allows_full_generic; a
    // denial surfaces as RuntimeError::SecurityException, which we
    // propagate verbatim so the Java caller observes a SecurityException
    // and the spawn does NOT happen.
    //
    // `invoke_virtual` takes (receiver, method, descriptor, args) where
    // `args` lists ONLY the explicit method parameters вЂ” the receiver is
    // not duplicated in the args slice (see other call sites such as
    // `AccessController.doPrivileged` in security_manager.rs).
    let cmd_obj = ctx.create_string(command_first);
    let args = [Value::Object(Some(cmd_obj))];
    ctx.invoke_virtual(sm_ref, "checkExec", "(Ljava/lang/String;)V", &args)?;
    Ok(())
}

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
        }
        .into());
    }
    let program = &cmd[0];

    // SECURITY: consult SecurityManager.checkExec(command[0]) BEFORE
    // touching std::process::Command. A SecurityException here must
    // prevent the spawn syscall entirely вЂ” see check_exec_or_throw doc.
    check_exec_or_throw(ctx, program)?;

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
        }
        .into()),
    }
}

/// Runtime.exec(String) вЂ” parse command line split by whitespace.
pub(crate) fn native_runtime_exec_string(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // args[0] = Runtime instance, args[1] = command string
    let cmd_str = match args.get(1) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => {
            return Err(RuntimeError::IllegalArgumentException {
                message: "Runtime.exec: null command".to_string(),
            }
            .into())
        }
    };
    let parts: Vec<String> = cmd_str.split_whitespace().map(String::from).collect();
    runtime_spawn_process(ctx, &parts, None, None)
}

/// Runtime.exec(String[])
pub(crate) fn native_runtime_exec_array(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let arg_val = args.get(1).copied().unwrap_or(Value::Object(None));
    let cmd = read_string_array(ctx, &arg_val);
    runtime_spawn_process(ctx, &cmd, None, None)
}

/// Runtime.exec(String, String[])
pub(crate) fn native_runtime_exec_string_env(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let cmd_str = match args.get(1) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => {
            return Err(RuntimeError::IllegalArgumentException {
                message: "Runtime.exec: null command".to_string(),
            }
            .into())
        }
    };
    let parts: Vec<String> = cmd_str.split_whitespace().map(String::from).collect();
    let env_val = args.get(2).copied().unwrap_or(Value::Object(None));
    let env = if matches!(env_val, Value::Object(None)) {
        None
    } else {
        Some(read_string_array(ctx, &env_val))
    };
    runtime_spawn_process(ctx, &parts, env.as_deref(), None)
}

/// Runtime.exec(String[], String[])
pub(crate) fn native_runtime_exec_array_env(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let arg_val = args.get(1).copied().unwrap_or(Value::Object(None));
    let cmd = read_string_array(ctx, &arg_val);
    let env_val = args.get(2).copied().unwrap_or(Value::Object(None));
    let env = if matches!(env_val, Value::Object(None)) {
        None
    } else {
        Some(read_string_array(ctx, &env_val))
    };
    runtime_spawn_process(ctx, &cmd, env.as_deref(), None)
}

/// Runtime.exec(String, String[], File)
pub(crate) fn native_runtime_exec_string_env_dir(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let cmd_str = match args.get(1) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => {
            return Err(RuntimeError::IllegalArgumentException {
                message: "Runtime.exec: null command".to_string(),
            }
            .into())
        }
    };
    let parts: Vec<String> = cmd_str.split_whitespace().map(String::from).collect();
    let env_val = args.get(2).copied().unwrap_or(Value::Object(None));
    let env = if matches!(env_val, Value::Object(None)) {
        None
    } else {
        Some(read_string_array(ctx, &env_val))
    };
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
pub(crate) fn native_runtime_exec_array_env_dir(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let arg_val = args.get(1).copied().unwrap_or(Value::Object(None));
    let cmd = read_string_array(ctx, &arg_val);
    let env_val = args.get(2).copied().unwrap_or(Value::Object(None));
    let env = if matches!(env_val, Value::Object(None)) {
        None
    } else {
        Some(read_string_array(ctx, &env_val))
    };
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

pub(crate) fn native_system_getenv(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
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

// ---------------------------------------------------------------------------
// Process-global singletons for `System.getenv()` (no-arg) and
// `System.getProperties()`.
//
// HotSpot returns the SAME object on every call:
//   - `System.getenv()` в†’ the cached unmodifiable
//     `ProcessEnvironment.theUnmodifiableEnvironment` Map, and
//   - `System.getProperties()` в†’ the `System.props` singleton `Properties`.
// so `System.getenv() == System.getenv()` and
// `System.getProperties() == System.getProperties()` hold (Spring's
// `StandardEnvironmentTests.getSystemEnvironment` / `.getSystemProperties`
// assert this via `isSameAs`). CratonVM allocated a fresh object on every call,
// so identity failed (SC-env-classreading RC-A).
//
// Cache the built object the first time and return it thereafter. The cached
// `ObjectRef`s live ONLY in these process-global mutexes (a Rust side-table,
// invisible to the field/stack/static root scans), so they must be reported as
// GC roots and remapped after a moving collection вЂ” exactly like the singleton
// class loaders (`classloader::gc_scan_loader_singleton_roots`). The matching
// hooks are `gc_scan_system_singleton_roots` (wired into `roots.rs`) and
// `gc_update_system_singleton_refs` (wired into `gc.rs`); `reset_system_singletons`
// clears them when a new VM is created (mirrors `reset_loader_singletons`).
// ---------------------------------------------------------------------------
use std::sync::{Mutex, OnceLock};

fn system_env_store() -> &'static Mutex<Option<ObjectRef>> {
    static INSTANCE: OnceLock<Mutex<Option<ObjectRef>>> = OnceLock::new();
    INSTANCE.get_or_init(|| Mutex::new(None))
}

fn system_props_store() -> &'static Mutex<Option<ObjectRef>> {
    static INSTANCE: OnceLock<Mutex<Option<ObjectRef>>> = OnceLock::new();
    INSTANCE.get_or_init(|| Mutex::new(None))
}

/// The cached no-arg `System.getenv()` Map singleton, if already built.
fn system_env_singleton() -> Option<ObjectRef> {
    *system_env_store().lock().unwrap_or_else(|e| e.into_inner())
}

/// Publish `obj` as the `System.getenv()` singleton (double-checked, like
/// [`set_system_props_singleton`]); returns the canonical singleton.
fn set_system_env_singleton(obj: ObjectRef) -> ObjectRef {
    let mut g = system_env_store().lock().unwrap_or_else(|e| e.into_inner());
    match *g {
        Some(existing) => existing,
        None => {
            *g = Some(obj);
            obj
        }
    }
}

/// Return the OpenJDK-shaped read-only wrapper used by `System.getenv()`.
///
/// The backing object is the real-layout `java/util/HashMap` built below.
/// HotSpot exposes the no-arg environment as a
/// `java.util.Collections$UnmodifiableMap` whose private field `m` points at
/// that backing map. System Rules reflects on that field by name, so the
/// existing CratonVM unmodifiable-map wrapper deliberately keeps the backing in
/// slot 0, matching the JDK's `m` field slot.
fn wrap_system_env_map(ctx: &mut dyn NativeContext, map: ObjectRef) -> ObjectRef {
    let pin = ctx.pin_native_root(map);
    let wrapper_class = ctx.ensure_synthetic_class("cratonvm/internal/UnmodifiableMap", 2);
    let map = ctx.read_native_pin(pin, map);
    let wrapper = ctx.alloc_object(wrapper_class, 2);
    let map = ctx.read_native_pin(pin, map);
    ctx.set_field(wrapper, 0, Value::Object(Some(map)));
    ctx.unpin_native_roots(pin);
    wrapper
}

/// The cached `System.getProperties()` `Properties` singleton, if already built.
pub fn system_props_singleton() -> Option<ObjectRef> {
    *system_props_store()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// Publish `obj` as the `System.getProperties()` singleton, unless another
/// thread already won the race вЂ” in which case the existing one is returned and
/// `obj` is discarded (it becomes unreachable and is collected). Returns the
/// canonical singleton so all callers converge on one identity.
pub fn set_system_props_singleton(obj: ObjectRef) -> ObjectRef {
    let mut g = system_props_store()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    match *g {
        Some(existing) => existing,
        None => {
            *g = Some(obj);
            obj
        }
    }
}

/// Replace the cached `System.getProperties()` singleton. Used by
/// `System.setProperties(Properties)`, which HotSpot implements as a global
/// swap of `System.props`, not as a mutation of the previous Properties object.
/// Returns the previous singleton so callers can drop any system-props marker.
pub fn replace_system_props_singleton(obj: Option<ObjectRef>) -> Option<ObjectRef> {
    let mut g = system_props_store()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    std::mem::replace(&mut *g, obj)
}

/// GC root scan for the `System.getenv()` / `System.getProperties()` singletons
/// (companion to [`gc_update_system_singleton_refs`]). Mirrors
/// `classloader::gc_scan_loader_singleton_roots`.
pub fn gc_scan_system_singleton_roots(out: &mut Vec<ObjectRef>) {
    if let Some(o) = *system_env_store().lock().unwrap_or_else(|e| e.into_inner()) {
        out.push(o);
    }
    if let Some(o) = *system_props_store()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
    {
        out.push(o);
    }
}

/// Post-GC remap for the system singletons (companion to
/// [`gc_scan_system_singleton_roots`]). Repoints the cached `ObjectRef`s to
/// their relocated addresses after a moving collection.
pub fn gc_update_system_singleton_refs(pointer_map: &std::collections::HashMap<usize, usize>) {
    if pointer_map.is_empty() {
        return;
    }
    let remap = |slot: &mut Option<ObjectRef>| {
        if let Some(obj_ref) = slot.as_mut() {
            let old_addr = obj_ref.as_ptr() as usize;
            if let Some(&new_addr) = pointer_map.get(&old_addr) {
                debug_assert!(new_addr != 0, "GC pointer map contains null address");
                *obj_ref = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
            }
        }
    };
    remap(&mut system_env_store().lock().unwrap_or_else(|e| e.into_inner()));
    remap(
        &mut system_props_store()
            .lock()
            .unwrap_or_else(|e| e.into_inner()),
    );
}

/// Reset the cached system singletons. Called when creating a new VM so a stale
/// `ObjectRef` from a previous VM instance is never returned (mirrors
/// `classloader::reset_loader_singletons`).
pub fn reset_system_singletons() {
    *system_env_store().lock().unwrap_or_else(|e| e.into_inner()) = None;
    *system_props_store()
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = None;
}

pub(crate) fn native_system_getenv_all(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    use cratonvm_types::ClassId;

    // Identity: return the cached singleton so `System.getenv() ==
    // System.getenv()` holds (SC-env-classreading RC-A). The process
    // environment is immutable for a running JVM, so the cached snapshot stays
    // correct. Only the real-layout path below caches; the legacy 3-field
    // fallback still returns the OpenJDK-shaped unmodifiable wrapper but is left
    // uncached so a later call retries once the real `java/util/HashMap` layout
    // is resolvable.
    if let Some(cached) = system_env_singleton() {
        return Ok(Some(Value::Object(Some(cached))));
    }

    // Build a HashMap with all environment variables.
    //
    // S111r7: previously this routine allocated `java/util/HashMap` with only
    // 3 fields and stored `(buckets, size, capacity)` at slot indices 0/1/2,
    // matching the synthetic layout used by `native-collections::native_map_*`.
    // That works while every Map operation routes through a registered native,
    // but `SystemEnvironmentPropertySource.containsKey` (Spring core) reaches
    // the bytecode interpreter for `HashMap.containsKey -> getNode`, where
    // `getfield #105 // table:[Ljava/util/HashMap$Node;` resolves to the real
    // HashMap layout's slot for `table` вЂ” slot 2 in JDK 25 (AbstractMap
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
        f_table,
        f_size,
        f_threshold,
        f_loadfactor,
        f_entryset,
        n_hash,
        n_key,
        n_value,
        n_next,
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

        // Cache the OpenJDK-shaped process-wide singleton (double-checked
        // publish). The wrapper's field 0 is the private `m` backing field that
        // libraries such as System Rules reach via reflection.
        let env = wrap_system_env_map(ctx, map);
        let env = set_system_env_singleton(env);
        return Ok(Some(Value::Object(Some(env))));
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

    let env = wrap_system_env_map(ctx, map);
    Ok(Some(Value::Object(Some(env))))
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

pub(crate) fn native_pb_start(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    eprintln!("[PB-START-OLD] called! args.len={}", args.len());
    // SECURITY: this is the "simplified" ProcessBuilder.start stub that
    // never actually spawns вЂ” it returns a dummy Process with exit_code=0.
    // The real spawning path is `phases_late::register_phase57_process`,
    // which last-write-wins overrides this registration. Even so we
    // funnel through check_exec_or_throw as defense-in-depth: if a future
    // refactor ever wires this stub up to std::process::Command, the
    // SecurityManager gate stays in place.
    //
    // Best-effort extraction of command[0] from the ProcessBuilder's
    // command field (slot 0). If we can't recover a program string we
    // still consult the SM with an empty argument so a deny-all policy
    // surfaces a SecurityException вЂ” matching the "empty argv is
    // suspicious" stance taken in `check_exec_or_throw`.
    let program: String = match args.first() {
        Some(Value::Object(Some(this))) => {
            let cmd_val = ctx.get_field(*this, 0);
            match cmd_val {
                Value::Object(Some(cmd_obj)) => {
                    // `ProcessBuilder.command` is a `List<String>` (typically an
                    // ArrayList), but `Runtime.exec`/legacy paths may hand us a
                    // raw `String[]`. Decide which by the object's RUNTIME CLASS,
                    // then read it layout-independently:
                    //   * List: read `size` / `elementData` BY FIELD NAME (the
                    //     real-JDK ArrayList carries `AbstractList.modCount` ahead
                    //     of `elementData`/`size`, so the old hard-coded
                    //     `size = field1` assumption failed and fell through to
                    //     `array_length(list)` вЂ” illegal on a non-array, which
                    //     tripped the array-length guard during picocli's
                    //     `getTerminalWidth()` ProcessBuilder probe).
                    //   * Array: only THEN is `array_length` legal.
                    let cname = ctx
                        .class_name_of_id(ctx.class_id_of_object(cmd_obj))
                        .unwrap_or_default();
                    let read_elem0 = |ctx: &mut dyn NativeContext, arr: ObjectRef| -> String {
                        match ctx.get_array_element(arr, 0) {
                            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                            _ => String::new(),
                        }
                    };
                    if cname.starts_with('[') {
                        // Genuine array (e.g. String[]): array_length is legal.
                        if ctx.array_length(cmd_obj) > 0 {
                            read_elem0(ctx, cmd_obj)
                        } else {
                            String::new()
                        }
                    } else {
                        // A List: read `size` + `elementData` by name.
                        let size = match ctx.get_field_by_name(cmd_obj, "size") {
                            Value::Int(n) => n,
                            _ => 0,
                        };
                        if size > 0 {
                            if let Value::Object(Some(data_arr)) =
                                ctx.get_field_by_name(cmd_obj, "elementData")
                            {
                                read_elem0(ctx, data_arr)
                            } else {
                                String::new()
                            }
                        } else {
                            String::new()
                        }
                    }
                }
                _ => String::new(),
            }
        }
        _ => String::new(),
    };
    check_exec_or_throw(ctx, &program)?;

    // Return a dummy Process object (simplified вЂ” no actual process execution)
    let proc = alloc_concurrent_synthetic(ctx, "java/lang/Process", 1);
    ctx.set_field(proc, 0, Value::Int(0)); // exit code
    Ok(Some(Value::Object(Some(proc))))
}

/// Materialize a `java/lang/StackTraceElement[]` from a captured frame trace
/// (innermost frame first, as `getStackTrace()` expects index 0 = current
/// call). Shared by `Thread.getStackTrace0()` and `Thread.dumpThreads()`.
pub(crate) fn build_stack_trace_element_array(
    ctx: &mut dyn NativeContext,
    trace: &[cratonvm_native_api::StackTraceEntry],
) -> cratonvm_types::ObjectRef {
    let arr = ctx.new_ref_array(cratonvm_types::ClassId::new(0), trace.len());
    // The captured trace is outermost-first (frame[0] = bottom of stack);
    // getStackTrace()/getAllStackTraces() want index 0 = the innermost (current)
    // call, so materialize reversed вЂ” matching HotSpot ordering.
    for (i, e) in trace.iter().rev().enumerate() {
        let ste = crate::alloc_concurrent_synthetic(ctx, "java/lang/StackTraceElement", 4);
        let cls_dotted = match ctx.class_id_by_name(&e.class_name) {
            Some(cid) => crate::lang_class::dotted_class_name(cid, &e.class_name),
            None => std::sync::Arc::from(e.class_name.replace('/', ".")),
        };
        crate::lang_misc::fill_stack_trace_element(
            ctx,
            ste,
            &e.class_name,
            &cls_dotted,
            &e.method_name,
            e.source_file.as_deref(),
            e.line_number,
        );
        ctx.set_array_element(arr, i, Value::Object(Some(ste)));
    }
    arr
}

/// `Thread.getStackTrace0()` вЂ” the live stack of the receiver thread (or, for
/// another thread, its last-published blocking-deposit snapshot). Returns a
/// `StackTraceElement[]`. Previously stubbed to an empty array.
pub(crate) fn native_thread_get_stack_trace(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => {
            let arr = ctx.new_ref_array(cratonvm_types::ClassId::new(0), 0);
            return Ok(Some(Value::Object(Some(arr))));
        }
    };
    let trace = ctx.thread_stack_trace(this);
    let arr = build_stack_trace_element_array(ctx, &trace);
    Ok(Some(Value::Object(Some(arr))))
}

// ---------------------------------------------------------------------------
// System.mapLibraryName(String) вЂ” JDK 25 native
// ---------------------------------------------------------------------------

/// Maps a library name to a platform-specific filename.
/// e.g. "foo" в†’ "foo.dll" (Windows), "libfoo.so" (Linux), "libfoo.dylib" (macOS).
pub(crate) fn native_system_map_library_name(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
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
// Thread.sleepNanos0(long) вЂ” JDK 25 native (replaces sleep(long) internally)
// ---------------------------------------------------------------------------

pub(crate) fn native_thread_sleep_nanos(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let nanos = match args.first() {
        Some(Value::Long(n)) => *n,
        _ => 0,
    };
    if nanos > 0 {
        // Keycloak Gap 9 localization (CRATONVM_DBG_SLEEP_TRACE): JDK25
        // Thread.sleep(millis) routes through Thread.sleepNanos -> here, so the
        // worker's poll-loop sleeps land in THIS native (not the millis one).
        if std::env::var_os("CRATONVM_DBG_SLEEP_TRACE").is_some() {
            use std::sync::atomic::{AtomicUsize, Ordering};
            static N: AtomicUsize = AtomicUsize::new(0);
            static PRINTED: AtomicUsize = AtomicUsize::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            if n % 16 == 0 && PRINTED.fetch_add(1, Ordering::Relaxed) < 80 {
                let st = ctx.capture_stack_trace(0);
                let frames: Vec<String> = st
                    .iter()
                    .take(12)
                    .map(|e| format!("{}.{}:{}", e.class_name, e.method_name, e.line_number))
                    .collect();
                eprintln!(
                    "[SLEEP-NANOS-TRACE #{n} nanos={nanos}] {}",
                    frames.join(" <- ")
                );
            }
        }
        // Check interrupted before sleeping вЂ” clear flag and throw
        if ctx.is_interrupted(true) {
            return Err(cratonvm_types::error::MethodCallFailed::InternalError(
                cratonvm_types::error::VmError::Runtime(
                    cratonvm_types::error::RuntimeError::InterruptedException,
                ),
            ));
        }
        let requested_millis = ((nanos as u128) + 999_999) / 1_000_000;
        let effective_millis =
            crate::async_handoff_sleep_millis(requested_millis.min(i64::MAX as u128) as i64);
        let effective_nanos = (effective_millis as u128)
            .saturating_mul(1_000_000)
            .max(nanos as u128)
            .min(u64::MAX as u128) as u64;
        let duration = std::time::Duration::from_nanos(effective_nanos);
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
        ctx.begin_blocking_region();
        std::thread::sleep(duration);
        ctx.end_blocking_region();
        let actual_dur = sleep_start.elapsed();
        if release {
            ctx.vt_acquire_carrier();
        }
        ctx.record_thread_sleep(nanos, actual_dur.as_nanos() as u64);
        // Check interrupted after sleeping вЂ” clear flag and throw
        if ctx.is_interrupted(true) {
            return Err(cratonvm_types::error::MethodCallFailed::InternalError(
                cratonvm_types::error::VmError::Runtime(
                    cratonvm_types::error::RuntimeError::InterruptedException,
                ),
            ));
        }
    }
    Ok(None)
}

// ---------------------------------------------------------------------------
// T19.N2 вЂ” Thread.sleep0(J)V вЂ” JDK 21+ internal sleep native.
// ---------------------------------------------------------------------------
//
// In JDK 21+ the public `Thread.sleep(long millis)` validates the argument
// and then delegates to this private `sleep0(J)V` for the actual sleep +
// interrupt check. `millis` is guaranteed >= 0 by the public caller, but
// HotSpot's native still performs a defensive negative-millis check and
// throws `IllegalArgumentException`, so we mirror that contract.
//
// Implementation notes:
//   * Bounds-check `millis >= 0` BEFORE casting to `u64` (signedв†’unsigned
//     cast of a negative value is a correctness bug вЂ” -1 would become
//     `u64::MAX`).
//   * When `millis == 0`, HotSpot still checks the interrupt status and
//     throws `InterruptedException` if set; no actual blocking happens.
//   * Sleep is performed in 100ms chunks so an interrupt delivered from
//     another thread is observed within в‰¤ ~100ms. This is a trade-off
//     between interrupt-responsiveness and syscall cost.
//   * The interrupt flag is CLEARED when we throw `InterruptedException`
//     per JDK spec.
pub(crate) fn native_thread_sleep0(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // WP4.5 вЂ” see `native_thread_sleep`: long args from the operand-stack
    // get re-decoded as Doubles by `CompactValue::to_value()`.
    let raw_millis: i64 = match args.first() {
        Some(Value::Long(ms)) => *ms,
        Some(Value::Double(d)) => d.to_bits() as i64,
        Some(Value::Int(i)) => *i as i64,
        _ => {
            return Err(
                cratonvm_types::error::RuntimeError::IllegalArgumentException {
                    message: "sleep0: missing long millis arg".to_string(),
                }
                .into(),
            );
        }
    };
    if raw_millis < 0 {
        return Err(
            cratonvm_types::error::RuntimeError::IllegalArgumentException {
                message: "timeout value is negative".to_string(),
            }
            .into(),
        );
    }
    let millis = raw_millis as u64;

    // 0ms case: check interrupt status (and clear) then return immediately.
    if millis == 0 {
        if ctx.is_interrupted(true) {
            return Err(cratonvm_types::error::MethodCallFailed::InternalError(
                cratonvm_types::error::VmError::Runtime(
                    cratonvm_types::error::RuntimeError::InterruptedException,
                ),
            ));
        }
        return Ok(None);
    }

    // NEW-15.4: virtual-thread aware sleep вЂ” release the carrier if this
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
    let effective_millis = crate::async_handoff_sleep_millis(millis as i64) as u64;
    let deadline = start + std::time::Duration::from_millis(effective_millis);
    let result = loop {
        let now = std::time::Instant::now();
        if now >= deadline {
            break Ok(None);
        }
        crate::scheduled_pump::registry().pump(ctx);
        // Poll interrupt flag before each chunk вЂ” clear + throw if set.
        if ctx.is_interrupted(true) {
            break Err(cratonvm_types::error::MethodCallFailed::InternalError(
                cratonvm_types::error::VmError::Runtime(
                    cratonvm_types::error::RuntimeError::InterruptedException,
                ),
            ));
        }
        let remaining = deadline - now;
        let chunk = std::cmp::min(remaining, std::time::Duration::from_millis(10));
        ctx.begin_blocking_region();
        std::thread::sleep(chunk);
        ctx.end_blocking_region();
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
// T14 вЂ” System bootstrap chain: initPhase1/2/3
// ---------------------------------------------------------------------------

/// `System.initPhase1()V` вЂ” JDK bootstrap phase 1.
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
    // Keycloak's Picocli.getErrWriter goes `new PrintWriter(System.err)` в†’
    // `PrintWriter(OutputStream, boolean)` which reads `((PrintStream)err).charset()`,
    // a plain getfield on the `charset` field. If that field is null, the
    // downstream `new OutputStreamWriter(stream, charset)` throws NPE("charset")
    // and Quarkus silently exits during command-line parsing. We must therefore
    // stamp a non-null Charset on System.out/err at bootstrap time.
    //
    // charset-NPE fix (2026-05-21): `set_field_by_name` resolves the
    // `charset` slot by walking the receiver's class hierarchy. If the
    // System.out/err object was allocated against the 1-field synthetic
    // `java/io/PrintStream` stub (created before the real class was
    // loaded), that walk finds no `charset` field and silently drops
    // the write вЂ” leaving the field null. `ensure_system_streams` now
    // force-loads the real PrintStream class first, but we also make
    // this helper defensive: it ensures the real `java/io/PrintStream`
    // class is loaded so the `charset` field is resolvable, and it
    // verifies the write actually landed.
    fn install_charset(ctx: &mut dyn NativeContext, stream: ObjectRef) {
        // Ensure the real PrintStream class is loaded so `charset` is a
        // resolvable field name. (No-op in pure synthetic-jdk mode.)
        let _ = ctx.ensure_class_initialized("java/io/PrintStream");
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
        // Verify the write landed. If `charset` did not resolve (e.g. the
        // receiver is still a fieldless synthetic stub), the bare
        // `set_field_by_name` above was a no-op and `PrintStream.charset()`
        // would return null в†’ NPE("charset") on the first `new
        // PrintWriter(System.err)`. Fall back to resolving the slot
        // index explicitly against `java/io/PrintStream` and, as a last
        // resort, scan the object's reference slots is unsafe (could
        // clobber `out`), so we only retry the explicit-index path.
        if let Some(idx) = ctx.resolve_field_index("java/io/PrintStream", "charset") {
            if idx < ctx.object_num_fields(stream) {
                ctx.set_field(stream, idx, Value::Object(Some(cs_obj)));
            }
        }
    }

    if let Some(out_stream) = ctx.get_system_stream("out") {
        install_charset(ctx, out_stream);
        ctx.set_static_field_by_name("java/lang/System", "out", Value::Object(Some(out_stream)));
    }
    if let Some(err_stream) = ctx.get_system_stream("err") {
        install_charset(ctx, err_stream);
        ctx.set_static_field_by_name("java/lang/System", "err", Value::Object(Some(err_stream)));
    }
    // S110 вЂ” System.in: wire up to OS stdin (fd id 0 in our FileDescriptorTable,
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
            let fd_class_id = ctx.ensure_class_initialized("java/io/FileDescriptor")?;
            let fd_fields = ctx.class_num_total_fields(fd_class_id).max(2);
            let fd_obj = ctx.alloc_object(fd_class_id, fd_fields);
            ctx.set_field_by_name(fd_obj, "fd", Value::Int(0));
            ctx.set_field_by_name(fd_obj, "handle", Value::Long(0));
            ctx.set_field_by_name(new_in, "fd", Value::Object(Some(fd_obj)));
            if !matches!(ctx.get_field_by_name(new_in, "fd"), Value::Object(Some(_))) {
                // Legacy synthetic fallback: encode stdin as `fd + 1` so the
                // reference-slot coercion path cannot collapse fd 0 to null.
                ctx.set_field(new_in, 1, Value::Int(1));
            }
            ctx.cache_system_stdin(new_in);
            new_in
        };
        let fd_obj = match ctx.get_field_by_name(in_obj, "fd") {
            Value::Object(Some(fd_obj)) => fd_obj,
            _ => {
                let fd_class_id = ctx.ensure_class_initialized("java/io/FileDescriptor")?;
                let fd_fields = ctx.class_num_total_fields(fd_class_id).max(2);
                let fd_obj = ctx.alloc_object(fd_class_id, fd_fields);
                ctx.set_field_by_name(in_obj, "fd", Value::Object(Some(fd_obj)));
                fd_obj
            }
        };
        ctx.set_field_by_name(fd_obj, "fd", Value::Int(0));
        ctx.set_field_by_name(fd_obj, "handle", Value::Long(0));
        ctx.cache_system_stdin(in_obj);
        ctx.set_static_field_by_name("java/lang/System", "in", Value::Object(Some(in_obj)));
    }

    // Step 3: Set System.lineSeparator from the line.separator property
    let line_sep = ctx
        .get_system_property("line.separator")
        .unwrap_or_else(|| if cfg!(windows) { "\r\n" } else { "\n" }.to_string());
    let line_sep_obj = ctx.create_string(&line_sep);
    ctx.set_static_field_by_name(
        "java/lang/System",
        "lineSeparator",
        Value::Object(Some(line_sep_obj)),
    );

    Ok(None)
}

/// `System.initPhase2(ZZ)I` вЂ” JDK bootstrap phase 2 (module system).
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
    // Module system is handled synthetically вЂ” report success
    Ok(Some(Value::Int(0)))
}

/// `System.initPhase3()V` вЂ” JDK bootstrap phase 3 (class loader hierarchy).
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
// T14 вЂ” jdk/internal/misc/VM natives
// ---------------------------------------------------------------------------

/// `VM.getSavedProperty(String)String` вЂ” return a saved VM property.
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

/// `VM.getRuntimeArguments()[String` вЂ” return the VM runtime arguments.
///
/// Returns an empty String array (we don't expose internal runtime args).
pub(crate) fn native_vm_get_runtime_arguments(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 0);
    Ok(Some(Value::Object(Some(arr))))
}

// ---------------------------------------------------------------------------
// T15 вЂ” Remaining missing natives
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

/// `java/lang/reflect/Array.newArray(Class<?> componentType, int length) в†’ Object`
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
                return Err(RuntimeError::NegativeArraySizeException { size: *n }.into());
            }
            *n as usize
        }
        _ => 0,
    };

    // Determine element type from the Class mirror.
    //
    // `Array.newInstance(Class,int)` / `Arrays.copyOf(.., Class)` delegate here
    // (`reflect/Array.newArray`). The component Class is frequently a
    // *synthesized array-class mirror* (e.g. `[Ljava/lang/String;` from
    // `String[][].class.getComponentType()`), whose name is only recoverable
    // via the VM reverse-map. The previous `read_string`/slot-1 read returned
    // nothing for those mirrors and defaulted to `java/lang/Object`, so
    // `rows.toArray(new Value[0][])` (H2 SortOrder.sort) allocated a bare
    // `Object[]` and CCE'd on the `(Value[][])` checkcast. Resolve via
    // `mirror_class_name` first (handles array + ordinary classes), keeping the
    // old readers as a fallback for legacy/unit-test mirrors.
    let comp_name = match args.first() {
        Some(Value::Object(Some(mirror))) => crate::lang_class::mirror_class_name(&*ctx, *mirror)
            .filter(|s| !s.is_empty())
            .or_else(|| ctx.read_string(*mirror))
            .or_else(|| match ctx.get_field(*mirror, 1) {
                Value::Object(Some(name_obj)) => ctx.read_string(name_obj),
                _ => None,
            })
            .map(|s| s.replace('.', "/"))
            .unwrap_or_else(|| "java/lang/Object".to_string()),
        _ => "java/lang/Object".to_string(),
    };

    if std::env::var("CRATONVM_DBG_TOARRAY").is_ok() {
        eprintln!(
            "[DBG_TOARRAY] newArray comp_name={:?} len={}",
            comp_name, length
        );
    }

    // Map primitive type names to ArrayElementType. Accept both the human name
    // (`int`) and the JVM descriptor (`I`) вЂ” `mirror_class_name` may return
    // either depending on how the primitive mirror was registered.
    let arr = match comp_name.as_str() {
        "int" | "I" => ctx.new_array(cratonvm_types::ArrayElementType::Int, length),
        "long" | "J" => ctx.new_array(cratonvm_types::ArrayElementType::Long, length),
        "float" | "F" => ctx.new_array(cratonvm_types::ArrayElementType::Float, length),
        "double" | "D" => ctx.new_array(cratonvm_types::ArrayElementType::Double, length),
        "boolean" | "Z" => ctx.new_array(cratonvm_types::ArrayElementType::Boolean, length),
        "byte" | "B" => ctx.new_array(cratonvm_types::ArrayElementType::Byte, length),
        "char" | "C" => ctx.new_array(cratonvm_types::ArrayElementType::Char, length),
        "short" | "S" => ctx.new_array(cratonvm_types::ArrayElementType::Short, length),
        _ => {
            // Reference array вЂ” resolve the component class. `ensure_class_initialized`
            // synthesizes array-descriptor components (`[L...;`) on demand, so a
            // multi-dimensional template yields the correct nested array type.
            let comp_id = ctx
                .ensure_class_initialized(&comp_name)
                .unwrap_or(cratonvm_types::ClassId::new(0));
            ctx.new_ref_array(comp_id, length)
        }
    };
    Ok(Some(Value::Object(Some(arr))))
}

/// `java/lang/reflect/Array.multiNewArray(Class componentType, int[] dimensions)`
///
/// Allocates a fully-materialized multi-dimensional array. Distinct from the
/// single-dim `newArray`: the result's runtime class must be the *precise*
/// nested array type вЂ” `Array.newInstance(String.class, {2,2})` в†’
/// `[[Ljava/lang/String;`, not `[Ljava/lang/String;` (SpEL
/// `ArrayConstructorTests.multiDimensionalArrays` asserts this exactly).
///
/// Each non-leaf level is therefore allocated with the resolved nested-array
/// component `ClassId` (via `ensure_class_initialized` on the `[вЂ¦` descriptor)
/// rather than the loose `ClassId(0)` the `multianewarray` *bytecode* path
/// uses вЂ” bytecode-built multiarrays are rarely inspected via `getClass()`,
/// reflective ones are.
///
/// Previously this descriptor (and `Array.newInstance(Class, int[])`) was wired
/// to a single-dim allocator that read only `dims[0]`, collapsing the result to
/// one dimension.
pub(crate) fn native_array_multi_new_array(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    use cratonvm_types::error::MethodCallFailed;

    // args[1] = int[] of per-dimension lengths.
    let dims_arr = match args.get(1) {
        Some(Value::Object(Some(a))) => *a,
        // Defensive: a single Int (1-D shape) вЂ” defer to the single-dim path.
        _ => return native_array_new_array(ctx, args),
    };
    let ndims = ctx.array_length(dims_arr);
    if ndims <= 1 {
        // A single dimension behaves exactly like `newArray`; reuse it so the
        // (already well-tested) Class-mirror element-type resolution applies.
        let len = if ndims == 1 {
            ctx.get_array_element(dims_arr, 0)
        } else {
            Value::Int(0)
        };
        let one_dim = [args.first().copied().unwrap_or(Value::Object(None)), len];
        return native_array_new_array(ctx, &one_dim);
    }

    let mut sizes: Vec<usize> = Vec::with_capacity(ndims);
    for i in 0..ndims {
        match ctx.get_array_element(dims_arr, i) {
            Value::Int(n) => {
                if n < 0 {
                    return Err(RuntimeError::NegativeArraySizeException { size: n }.into());
                }
                sizes.push(n as usize);
            }
            _ => sizes.push(0),
        }
    }

    // Resolve the leaf component type from the Class mirror (mirrors the
    // resolution in `native_array_new_array`).
    let comp_name = match args.first() {
        Some(Value::Object(Some(mirror))) => crate::lang_class::mirror_class_name(&*ctx, *mirror)
            .filter(|s| !s.is_empty())
            .or_else(|| ctx.read_string(*mirror))
            .map(|s| s.replace('.', "/"))
            .unwrap_or_else(|| "java/lang/Object".to_string()),
        _ => "java/lang/Object".to_string(),
    };

    // Either a primitive leaf (single-letter descriptor) or a reference leaf
    // (`LвЂ¦;`). The descriptor letter is what the nested array-class names are
    // built from.
    let (prim_et, leaf_desc) = match comp_name.as_str() {
        "int" | "I" => (Some(cratonvm_types::ArrayElementType::Int), "I".to_string()),
        "long" | "J" => (
            Some(cratonvm_types::ArrayElementType::Long),
            "J".to_string(),
        ),
        "float" | "F" => (
            Some(cratonvm_types::ArrayElementType::Float),
            "F".to_string(),
        ),
        "double" | "D" => (
            Some(cratonvm_types::ArrayElementType::Double),
            "D".to_string(),
        ),
        "boolean" | "Z" => (
            Some(cratonvm_types::ArrayElementType::Boolean),
            "Z".to_string(),
        ),
        "byte" | "B" => (
            Some(cratonvm_types::ArrayElementType::Byte),
            "B".to_string(),
        ),
        "char" | "C" => (
            Some(cratonvm_types::ArrayElementType::Char),
            "C".to_string(),
        ),
        "short" | "S" => (
            Some(cratonvm_types::ArrayElementType::Short),
            "S".to_string(),
        ),
        other => (None, format!("L{other};")),
    };

    // Reference leaf: resolve the base component class id once (used for the
    // innermost `String[]`-style array). Primitives ignore it.
    let base_ref_id = if prim_et.is_none() {
        ctx.ensure_class_initialized(&comp_name)
            .unwrap_or(cratonvm_types::ClassId::new(0))
    } else {
        cratonvm_types::ClassId::new(0)
    };

    fn build(
        ctx: &mut dyn NativeContext,
        sizes: &[usize],
        level: usize,
        prim_et: Option<cratonvm_types::ArrayElementType>,
        base_ref_id: cratonvm_types::ClassId,
        leaf_desc: &str,
    ) -> Result<ObjectRef, MethodCallFailed> {
        let ndims = sizes.len();
        let len = sizes[level];
        if level == ndims - 1 {
            // Innermost specified dimension: leaf array of the base type.
            let arr = match prim_et {
                Some(et) => ctx.new_array(et, len),
                None => ctx.new_ref_array(base_ref_id, len),
            };
            return Ok(arr);
        }
        // Intermediate level: a reference array whose component is the nested
        // array type one level down вЂ” descriptor `'['*(ndims-1-level) + leaf`.
        let comp_desc: String = "[".repeat(ndims - 1 - level) + leaf_desc;
        let comp_id = ctx
            .ensure_class_initialized(&comp_desc)
            .unwrap_or(cratonvm_types::ClassId::new(0));
        let mut arr = ctx.new_ref_array(comp_id, len);
        // Pin the parent across each sub-array allocation: under a moving
        // collector `arr` may relocate while `build` allocates.
        let pin = ctx.pin_native_root(arr);
        for i in 0..len {
            arr = ctx.read_native_pin(pin, arr);
            let sub = build(ctx, sizes, level + 1, prim_et, base_ref_id, leaf_desc)?;
            arr = ctx.read_native_pin(pin, arr);
            ctx.set_array_element(arr, i, Value::Object(Some(sub)));
        }
        ctx.unpin_native_roots(pin);
        Ok(arr)
    }

    let arr = build(ctx, &sizes, 0, prim_et, base_ref_id, &leaf_desc)?;
    Ok(Some(Value::Object(Some(arr))))
}

const CLASS_FILE_MAGIC: [u8; 4] = [0xCA, 0xFE, 0xBA, 0xBE];

fn define_class_format_error(class_name: &str, method: &str, message: String) -> MethodCallFailed {
    LinkageError::ClassFormatError {
        class_name: class_name.to_string(),
        message: format!("{method}: {message}"),
    }
    .into()
}

fn validate_classfile_header(
    class_name: &str,
    method: &str,
    bytes: &[u8],
) -> Result<(), MethodCallFailed> {
    if bytes.len() < 8 || bytes[0..4] != CLASS_FILE_MAGIC {
        return Err(define_class_format_error(
            class_name,
            method,
            "not a valid class file (bad magic)".to_string(),
        ));
    }
    Ok(())
}

fn read_define_class_nonnegative_int(
    args: &[Value],
    idx: usize,
) -> Result<usize, MethodCallFailed> {
    match args.get(idx) {
        Some(Value::Int(v)) if *v >= 0 => Ok(*v as usize),
        Some(Value::Int(v)) => {
            Err(RuntimeError::ArrayIndexOutOfBoundsException { index: *v }.into())
        }
        _ => Ok(0),
    }
}

fn read_byte_array_define_class_slice(
    ctx: &dyn NativeContext,
    array: ObjectRef,
    offset: usize,
    length: usize,
) -> Result<Vec<u8>, MethodCallFailed> {
    let arr_len = ctx.array_length(array);
    let end = offset
        .checked_add(length)
        .ok_or_else(|| RuntimeError::ArrayIndexOutOfBoundsException { index: i32::MAX })?;
    if end > arr_len {
        return Err(RuntimeError::ArrayIndexOutOfBoundsException {
            index: end.min(i32::MAX as usize) as i32,
        }
        .into());
    }

    let mut bytes = Vec::with_capacity(length);
    for i in 0..length {
        match ctx.get_array_element(array, offset + i) {
            Value::Int(b) => bytes.push((b & 0xFF) as u8),
            _ => bytes.push(0),
        }
    }
    Ok(bytes)
}

fn read_byte_buffer_define_class_slice(
    ctx: &dyn NativeContext,
    bb: ObjectRef,
    offset: usize,
    length: usize,
    class_name: &str,
) -> Result<Vec<u8>, MethodCallFailed> {
    let pos_slot = 1;
    let limit_slot = 2;
    let capacity_slot = 3;

    if let Value::Object(Some(array)) = ctx.get_field(bb, 0) {
        let pos = ctx.get_field(bb, pos_slot).as_int().unwrap_or(0).max(0) as usize;
        let limit = ctx
            .get_field(bb, limit_slot)
            .as_int()
            .unwrap_or_else(|| ctx.array_length(array) as i32)
            .max(0) as usize;
        let cap = ctx.array_length(array);
        let absolute_off = pos
            .checked_add(offset)
            .ok_or_else(|| RuntimeError::ArrayIndexOutOfBoundsException { index: i32::MAX })?;
        let upper = limit.min(cap);
        let end = absolute_off
            .checked_add(length)
            .ok_or_else(|| RuntimeError::ArrayIndexOutOfBoundsException { index: i32::MAX })?;
        if end > upper {
            return Err(RuntimeError::ArrayIndexOutOfBoundsException {
                index: end.min(i32::MAX as usize) as i32,
            }
            .into());
        }
        return read_byte_array_define_class_slice(ctx, array, absolute_off, length);
    }

    let addr = match ctx.get_field_by_name(bb, "address") {
        Value::Long(v) => v,
        _ => 0,
    };
    let pos = ctx.get_field(bb, pos_slot).as_int().unwrap_or(0).max(0) as usize;
    let cap = ctx
        .get_field(bb, capacity_slot)
        .as_int()
        .unwrap_or(0)
        .max(0) as usize;
    let limit = ctx
        .get_field(bb, limit_slot)
        .as_int()
        .unwrap_or(cap as i32)
        .max(0) as usize;
    if addr == 0 {
        return Err(define_class_format_error(
            class_name,
            "defineClass2",
            "direct ByteBuffer has no native address".to_string(),
        ));
    }
    let absolute_off = pos
        .checked_add(offset)
        .ok_or_else(|| RuntimeError::ArrayIndexOutOfBoundsException { index: i32::MAX })?;
    let upper = limit.min(cap);
    let end = absolute_off
        .checked_add(length)
        .ok_or_else(|| RuntimeError::ArrayIndexOutOfBoundsException { index: i32::MAX })?;
    if end > upper {
        return Err(RuntimeError::ArrayIndexOutOfBoundsException {
            index: end.min(i32::MAX as usize) as i32,
        }
        .into());
    }
    let mut out = vec![0u8; length];
    if length > 0 {
        let src = addr.wrapping_add(absolute_off as i64);
        if !ctx.copy_from_native_memory(src, &mut out) {
            return Err(define_class_format_error(
                class_name,
                "defineClass2",
                format!("direct ByteBuffer copy failed (addr={src:#x}, len={length})"),
            ));
        }
    }
    Ok(out)
}

/// `ClassLoader.defineClass1(ClassLoader, String, byte[], int, int, ProtectionDomain, String) в†’ Class`
///
/// Defines a class from a byte array. WP2.3: routes through
/// `define_class_full` so this entry point shares the same backend
/// (name-mismatch check, dup-define rejection, PD attribution) as
/// the other three (Unsafe.defineClass + Lookup.defineClass).
/// Pre-resolve a class's direct supertypes (superclass + interfaces) through the
/// DEFINING loader before `define_class_full` links them.
///
/// `class_manager::define_class` resolves a class's superclass/interfaces only
/// through CratonVM's global classpath (`load_class`) вЂ” it never calls back into
/// the user `ClassLoader` that is defining the class. That is wrong for a loader
/// whose classes live somewhere the global classpath cannot see: Tomcat's
/// `WebappClassLoader` serves `/WEB-INF/lib` jars from its `WebResourceRoot`, so
/// when it defines `org.apache.taglibs.standard.tlv.JstlCoreTLV` (a JSTL
/// `TagLibraryValidator`) the superclass `JstlBaseTLV` вЂ” in the SAME jar вЂ” is
/// invisible to the global store and the define fails (`ClassNotFound:
/// JstlBaseTLV`), 500-ing every JSP that triggers TLD validation
/// (`TestScopedAttributeELResolver`). JVMS В§5.3.5 makes the defining loader the
/// *initiating* loader for supertype resolution, so load each not-yet-loaded
/// supertype through it first; once present in the store, `define_class_full`
/// links cleanly.
///
/// Only fires for USER-DEFINED loaders and only for supertypes not already
/// loaded, so built-in/app-loader defines (ByteBuddy, cglib, the bootstrap
/// chain) вЂ” whose supertypes resolve from the classpath вЂ” are unaffected.
fn preload_supertypes_via_loader(ctx: &mut dyn NativeContext, loader_obj: ObjectRef, bytes: &[u8]) {
    if !crate::classloader::is_user_defined_loader(ctx, loader_obj) {
        return;
    }
    let cf = match cratonvm_reader::read_class(bytes) {
        Ok(c) => c,
        Err(_) => return, // malformed bytes вЂ” let define_class_full report it
    };
    let mut supertypes: Vec<String> = Vec::new();
    if let Some(s) = &cf.super_class {
        let n: &str = s;
        if !n.is_empty() && n != "java/lang/Object" {
            supertypes.push(n.to_string());
        }
    }
    for iface in &cf.interfaces {
        let n: &str = iface;
        if !n.is_empty() {
            supertypes.push(n.to_string());
        }
    }
    if supertypes.is_empty() {
        return;
    }
    let p_loader = ctx.pin_native_root(loader_obj);
    let mut loader = loader_obj;
    // Loader-faithful gate: under CRATONVM_LOADER_AWARE_RESOLUTION the
    // "already loaded" short-circuit must be keyed on THIS loader's namespace,
    // not the global name index. An isolating/enhancing loader (Hibernate's
    // package-scoped `EnhancingClassLoader`) defines its OWN enhanced copy of an
    // in-package supertype; if a different loader already loaded the un-enhanced
    // copy, the global `class_id_by_name` probe would wrongly skip the drive and
    // the subclass would link the un-enhanced super (NoSuchMethodError on the
    // enhanced `$$_hibernate_*` accessors). Driving `loadClass` here defines the
    // loader's enhanced copy first, which `define_class_full` then prefers via
    // the (loader,name) exact link. Gate-off keeps the global short-circuit в†’
    // byte-identical.
    let loader_faithful = crate::classloader::loader_aware_resolution();
    let loader_ns = if loader_faithful {
        let ns = crate::classloader::loader_namespace_id(ctx, loader);
        loader = ctx.read_native_pin(p_loader, loader);
        ns
    } else {
        0
    };
    for internal in &supertypes {
        loader = ctx.read_native_pin(p_loader, loader);
        let already = if loader_faithful {
            ctx.class_id_defined_by_loader_exact(internal, loader_ns)
                .is_some()
        } else {
            ctx.class_id_by_name(internal).is_some()
        };
        if already {
            continue; // already present for this loader вЂ” define_class_full links it
        }
        let dotted = internal.replace('/', ".");
        let name_str = ctx.create_string(&dotted); // may relocate `loader`
        loader = ctx.read_native_pin(p_loader, loader);
        let p_name = ctx.pin_native_root(name_str);
        loader = ctx.read_native_pin(p_loader, loader);
        let name_str = ctx.read_native_pin(p_name, name_str);
        // Best-effort: a genuine miss/throw is swallowed here and left for
        // `define_class_full` to surface as the spec-mandated linkage error.
        let _ = ctx.invoke_virtual(
            loader,
            "loadClass",
            "(Ljava/lang/String;)Ljava/lang/Class;",
            &[Value::Object(Some(name_str))],
        );
        loader = ctx.read_native_pin(p_loader, loader); // invoke may relocate
        ctx.unpin_native_roots(p_name);
    }
    ctx.unpin_native_roots(p_loader);
}

fn same_loader_already_defined_mirror(
    ctx: &mut dyn NativeContext,
    loader_obj: ObjectRef,
    internal_name: &str,
    loader_id: u32,
    msg: &str,
) -> Option<ObjectRef> {
    if internal_name.is_empty() || !msg.contains("already defined") {
        return None;
    }
    if let Some(mirror) =
        crate::classloader::find_loaded_class_for_loader(ctx, loader_obj, internal_name)
    {
        return Some(mirror);
    }
    if loader_id != 0 {
        if let Some(class_id) = ctx.class_id_defined_by_loader_exact(internal_name, loader_id) {
            crate::classloader::register_defining_loader(class_id.as_u32(), loader_obj);
            return Some(ctx.get_class_mirror(class_id));
        }
    }
    None
}

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
        _ => {
            return Err(RuntimeError::NullPointerException {
                message: Some("defineClass1: byte[] is null".to_string()),
            }
            .into());
        }
    };

    let offset = read_define_class_nonnegative_int(args, 3)?;
    let length = read_define_class_nonnegative_int(args, 4)?;
    let bytes = read_byte_array_define_class_slice(ctx, byte_array, offset, length)?;
    validate_classfile_header(&name, "defineClass1", &bytes)?;

    // Loader id from arg 0 (synthetic ClassLoader); 0 = app loader.
    let loader_id = match args.first() {
        Some(Value::Object(Some(loader_obj))) => {
            let mut lid = match ctx.get_field(*loader_obj, 6) {
                Value::Int(v) if v > 0 => v as u32,
                _ => 0,
            };
            // Override-first redefinition: when a USER-DEFINED loader (e.g.
            // Spring's OverridingClassLoader) defines a class whose name is
            // ALREADY loaded by another loader, defining it under the
            // Application namespace (id 0) would collide ("already defined by
            // application loader"). Give this loader its own namespace so the
            // redefinition succeeds and `Class.getClassLoader()` reports it.
            // (Real-JDK mode reaches here with lid == 0 because the synthetic
            // CL_LOADER_ID slot is a real ClassLoader field there.) Legacy
            // behavior only kicks in on an actual name collision, so
            // ByteBuddy/cglib's fresh-name defines keep their existing
            // Application-namespace behavior.
            //
            // Loader-faithful gate (CRATONVM_LOADER_AWARE_RESOLUTION): when on,
            // EVERY user-loader define gets its own stable namespace вЂ” not just
            // on collision вЂ” so the *first* definer of a name (an isolating
            // loader) is isolated too instead of landing in the shared
            // Application namespace (the ProxyClassReuseTest / IsoProbe bug).
            if lid == 0 && crate::classloader::is_user_defined_loader(ctx, *loader_obj) {
                if crate::classloader::loader_aware_resolution()
                    || name.is_empty()
                    || ctx.class_id_by_name(&name).is_some()
                {
                    lid = crate::classloader::loader_namespace_id(ctx, *loader_obj);
                }
            }
            lid
        }
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

    // JVMS В§5.3.5 вЂ” resolve direct supertypes through the DEFINING loader before
    // linking. `define_class_full` resolves the superclass/interfaces only via the
    // global classpath; a user loader whose classes are invisible there (Tomcat's
    // `WebappClassLoader` serving `/WEB-INF/lib` jars) would otherwise fail to
    // define a class whose super lives in the same jar (JSTL `JstlCoreTLV` в†’
    // `JstlBaseTLV`). No-op for built-in/app-loader defines.
    if let Some(Value::Object(Some(loader_obj))) = args.first() {
        preload_supertypes_via_loader(ctx, *loader_obj, &bytes);
    }

    let opts = cratonvm_native_api::DefineClassFull {
        code_source_url: pd_url,
        ..Default::default()
    };
    match ctx.define_class_full(&name, &bytes, loader_id, opts) {
        Ok(class_id) => {
            // Record the exact defining ClassLoader instance so
            // `Class.getClassLoader()` returns it (not the app-loader fallback).
            // ByteBuddy's `ByteArrayClassLoader.load` asserts
            // `Class.forName(name, false, this).getClassLoader() == this` and
            // throws "Class already loaded" otherwise вЂ” the blocker for
            // Hibernate's ByteBuddy proxy generation.
            if let Some(Value::Object(Some(loader_obj))) = args.first() {
                crate::classloader::register_defining_loader(class_id.as_u32(), *loader_obj);
            }
            let mirror = ctx.get_class_mirror(class_id);
            Ok(Some(Value::Object(Some(mirror))))
        }
        Err(msg) => {
            if let Some(Value::Object(Some(loader_obj))) = args.first() {
                if let Some(mirror) =
                    same_loader_already_defined_mirror(ctx, *loader_obj, &name, loader_id, &msg)
                {
                    return Ok(Some(Value::Object(Some(mirror))));
                }
            }
            tracing::warn!("ClassLoader.defineClass1({name}) failed: {msg}");
            Err(define_class_format_error(&name, "defineClass1", msg))
        }
    }
}

/// `ClassLoader.defineClass2(ClassLoader, String, ByteBuffer, int, int, ProtectionDomain, String) -> Class`
pub(crate) fn native_classloader_define_class2(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let name = match args.get(1) {
        Some(Value::Object(Some(s))) => {
            let n = ctx.read_string(*s).unwrap_or_default();
            n.replace('.', "/")
        }
        _ => String::new(),
    };

    let byte_buffer = match args.get(2) {
        Some(Value::Object(Some(bb))) => *bb,
        _ => {
            return Err(RuntimeError::NullPointerException {
                message: Some("defineClass2: ByteBuffer is null".to_string()),
            }
            .into());
        }
    };

    let offset = read_define_class_nonnegative_int(args, 3)?;
    let length = read_define_class_nonnegative_int(args, 4)?;
    let bytes = read_byte_buffer_define_class_slice(ctx, byte_buffer, offset, length, &name)?;
    validate_classfile_header(&name, "defineClass2", &bytes)?;

    let loader_id = match args.first() {
        Some(Value::Object(Some(loader_obj))) => {
            let mut lid = match ctx.get_field(*loader_obj, 6) {
                Value::Int(v) if v > 0 => v as u32,
                _ => 0,
            };
            if lid == 0 && crate::classloader::is_user_defined_loader(ctx, *loader_obj) {
                if crate::classloader::loader_aware_resolution()
                    || name.is_empty()
                    || ctx.class_id_by_name(&name).is_some()
                {
                    lid = crate::classloader::loader_namespace_id(ctx, *loader_obj);
                }
            }
            lid
        }
        _ => 0,
    };

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

    if let Some(Value::Object(Some(loader_obj))) = args.first() {
        preload_supertypes_via_loader(ctx, *loader_obj, &bytes);
    }

    let opts = cratonvm_native_api::DefineClassFull {
        code_source_url: pd_url,
        ..Default::default()
    };
    match ctx.define_class_full(&name, &bytes, loader_id, opts) {
        Ok(class_id) => {
            if let Some(Value::Object(Some(loader_obj))) = args.first() {
                crate::classloader::register_defining_loader(class_id.as_u32(), *loader_obj);
            }
            let mirror = ctx.get_class_mirror(class_id);
            Ok(Some(Value::Object(Some(mirror))))
        }
        Err(msg) => {
            if let Some(Value::Object(Some(loader_obj))) = args.first() {
                if let Some(mirror) =
                    same_loader_already_defined_mirror(ctx, *loader_obj, &name, loader_id, &msg)
                {
                    return Ok(Some(Value::Object(Some(mirror))));
                }
            }
            tracing::warn!("ClassLoader.defineClass2({name}) failed: {msg}");
            Err(define_class_format_error(&name, "defineClass2", msg))
        }
    }
}

/// `ClassLoader.defineClass0(ClassLoader, Class, String, byte[], int, int, ProtectionDomain, boolean, int, Object) в†’ Class`
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
        _ => {
            return Err(RuntimeError::NullPointerException {
                message: Some("defineClass0: byte[] is null".to_string()),
            }
            .into());
        }
    };

    let offset = read_define_class_nonnegative_int(args, 4)?;
    let length = read_define_class_nonnegative_int(args, 5)?;
    let bytes = read_byte_array_define_class_slice(ctx, byte_array, offset, length)?;
    validate_classfile_header(&name, "defineClass0", &bytes)?;

    let loader_id = match args.first() {
        Some(Value::Object(Some(loader_obj))) => {
            let mut lid = match ctx.get_field(*loader_obj, 6) {
                Value::Int(v) if v > 0 => v as u32,
                _ => 0,
            };
            // Same override-first / name-collision handling as defineClass1,
            // plus the loader-faithful gate: when
            // CRATONVM_LOADER_AWARE_RESOLUTION is on, every user-loader define
            // gets its own namespace (not just on collision) so the first
            // definer is isolated too.
            if lid == 0 && crate::classloader::is_user_defined_loader(ctx, *loader_obj) {
                if crate::classloader::loader_aware_resolution()
                    || name.is_empty()
                    || ctx.class_id_by_name(&name).is_some()
                {
                    lid = crate::classloader::loader_namespace_id(ctx, *loader_obj);
                }
            }
            lid
        }
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

    let opts = cratonvm_native_api::DefineClassFull {
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
            if !hidden {
                if let Some(Value::Object(Some(loader_obj))) = args.first() {
                    if let Some(mirror) = same_loader_already_defined_mirror(
                        ctx,
                        *loader_obj,
                        &effective_name,
                        loader_id,
                        &msg,
                    ) {
                        return Ok(Some(Value::Object(Some(mirror))));
                    }
                }
            }
            tracing::warn!("ClassLoader.defineClass0({effective_name}) failed: {msg}");
            Err(define_class_format_error(
                &effective_name,
                "defineClass0",
                msg,
            ))
        }
    }
}

// ---------------------------------------------------------------------------
// jdk.internal.perf.Perf natives (C27)
//
// The JDK's performance-counter infrastructure uses `Perf.getPerf().createLong(...)`
// to register internal counters. We don't track perf counters, so we return
// benign defaults вЂ” empty/zero-filled direct ByteBuffers for createLong /
// createByteArray (so callers can still write into them), zeros / no-ops for
// the rest. Perf counters in the real JDK only drive diagnostic output; no
// program correctness depends on their values.
// ---------------------------------------------------------------------------

pub(crate) fn native_perf_attach(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    // attach(String, int) -> ByteBuffer  вЂ”  return an empty direct buffer.
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
        let r = native_thread_sleep_millis_nanos(&mut ctx, &[Value::Long(0), Value::Int(0)]);
        assert!(r.is_ok());
    }

    #[test]
    fn t2_thread_sleep_rejects_negative_millis() {
        let mut ctx = mock_ctx();
        let r = native_thread_sleep_millis_nanos(&mut ctx, &[Value::Long(-1), Value::Int(0)]);
        assert!(r.is_err(), "negative millis must throw");
    }

    #[test]
    fn t2_thread_sleep_rejects_negative_nanos() {
        let mut ctx = mock_ctx();
        let r = native_thread_sleep_millis_nanos(&mut ctx, &[Value::Long(0), Value::Int(-1)]);
        assert!(r.is_err(), "negative nanos must throw");
    }

    #[test]
    fn t2_thread_sleep_rejects_oversized_nanos() {
        let mut ctx = mock_ctx();
        let r =
            native_thread_sleep_millis_nanos(&mut ctx, &[Value::Long(0), Value::Int(1_000_000)]);
        assert!(r.is_err(), "nanos >= 1_000_000 must throw");
    }

    #[test]
    fn t2_thread_sleep_accepts_max_nanos() {
        let mut ctx = mock_ctx();
        let r = native_thread_sleep_millis_nanos(&mut ctx, &[Value::Long(0), Value::Int(999_999)]);
        assert!(r.is_ok(), "nanos = 999_999 must be accepted");
    }
}

// ---------------------------------------------------------------------------
// Tests for T19.N2 вЂ” Thread.sleep0(J)V
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
            "sleep0(10) should sleep в‰Ґ 10ms, took {:?}",
            elapsed
        );
        // Must not have wildly overslept (generous upper bound for CI).
        assert!(
            elapsed <= std::time::Duration::from_millis(200),
            "sleep0(10) should return within ~200ms upper bound, took {:?}",
            elapsed
        );
    }

    /// Negative millis в†’ IllegalArgumentException (defensive native check).
    #[test]
    fn t19_n2_thread_sleep0_negative_throws_illegal_argument() {
        let mut ctx = mock_ctx();
        let r = native_thread_sleep0(&mut ctx, &[Value::Long(-1)]);
        assert!(r.is_err(), "negative millis must throw");
        match r {
            Err(cratonvm_types::error::MethodCallFailed::InternalError(
                cratonvm_types::error::VmError::Runtime(
                    cratonvm_types::error::RuntimeError::IllegalArgumentException { message },
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
    /// returns within one chunk (в‰¤ 110ms) with InterruptedException, and
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
        // Request a 2-second sleep вЂ” if interrupt polling is broken, the
        // test will hang for ~2s (still fail, but visibly).
        let r = native_thread_sleep0(&mut ctx, &[Value::Long(2000)]);
        let elapsed = start.elapsed();
        assert!(r.is_err(), "sleep0 with interrupt must throw");
        match r {
            Err(cratonvm_types::error::MethodCallFailed::InternalError(
                cratonvm_types::error::VmError::Runtime(
                    cratonvm_types::error::RuntimeError::InterruptedException,
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
            "interrupt should be detected within в‰¤110ms, took {:?}",
            elapsed
        );
    }
}

#[cfg(test)]
mod t14_tests {
    use super::*;
    use crate::test_utils::mock_ctx;

    // -----------------------------------------------------------------------
    // T14.1 вЂ” initPhase1
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
    // T14.2 вЂ” initPhase2
    // -----------------------------------------------------------------------

    #[test]
    fn init_phase2_returns_zero() {
        let mut ctx = mock_ctx();
        let r = native_system_init_phase2(&mut ctx, &[Value::Int(0), Value::Int(0)]);
        assert_eq!(r.unwrap(), Some(Value::Int(0)));
    }

    // -----------------------------------------------------------------------
    // T14.3 вЂ” initPhase3
    // -----------------------------------------------------------------------

    #[test]
    fn init_phase3_succeeds() {
        let mut ctx = mock_ctx();
        let r = native_system_init_phase3(&mut ctx, &[]);
        assert!(r.is_ok());
        assert_eq!(r.unwrap(), None); // void return
    }

    // -----------------------------------------------------------------------
    // T14.4 вЂ” VM.getSavedProperty
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
    // T14.5 вЂ” VM.getRuntimeArguments
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
    // T15.1.5 вЂ” Finalizer.register
    // -----------------------------------------------------------------------

    #[test]
    fn finalizer_register_with_object() {
        let mut ctx = mock_ctx();
        let obj = ctx.alloc_object(cratonvm_types::ClassId::new(0), 2);
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
    // T15.1.6 вЂ” Array.newArray
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
        assert!(
            r.is_err(),
            "negative size should throw NegativeArraySizeException"
        );
    }

    // -----------------------------------------------------------------------
    // T15.1.3 вЂ” ClassLoader.defineClass0/1/2
    // -----------------------------------------------------------------------

    #[test]
    fn define_class1_empty_bytes_throws() {
        let mut ctx = mock_ctx();
        // Create a byte array with non-CAFEBABE bytes
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 4);
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
                Value::Int(0),       // offset
                Value::Int(4),       // length
                Value::Object(None), // pd
                Value::Object(None), // source
            ],
        );
        assert!(
            r.is_err(),
            "invalid class bytes must throw, not return null"
        );
    }

    #[test]
    fn define_class2_reads_bytebuffer_backing_array() {
        let mut ctx = mock_ctx();
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 4);
        for i in 0..4 {
            ctx.set_array_element(arr, i, Value::Int(0));
        }
        let bb = ctx.alloc_object(cratonvm_types::ClassId::new(0), 4);
        ctx.set_field(bb, 0, Value::Object(Some(arr)));
        ctx.set_field(bb, 1, Value::Int(0));
        ctx.set_field(bb, 2, Value::Int(4));
        ctx.set_field(bb, 3, Value::Int(4));
        let name = ctx.create_string("com/example/Foo");
        let r = native_classloader_define_class2(
            &mut ctx,
            &[
                Value::Object(None),
                Value::Object(Some(name)),
                Value::Object(Some(bb)),
                Value::Int(0),
                Value::Int(4),
                Value::Object(None),
                Value::Object(None),
            ],
        );
        match r {
            Err(cratonvm_types::error::MethodCallFailed::InternalError(
                cratonvm_types::error::VmError::Linkage(
                    cratonvm_types::error::LinkageError::ClassFormatError { message, .. },
                ),
            )) => assert!(
                message.contains("defineClass2"),
                "expected defineClass2 class-format failure, got {message}"
            ),
            other => panic!("expected ClassFormatError from ByteBuffer handler, got {other:?}"),
        }
    }

    #[test]
    fn define_class0_empty_bytes_throws() {
        let mut ctx = mock_ctx();
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 4);
        let name = ctx.create_string("com/example/Foo");
        let r = native_classloader_define_class0(
            &mut ctx,
            &[
                Value::Object(None),
                Value::Object(None),
                Value::Object(Some(name)),
                Value::Object(Some(arr)),
                Value::Int(0),
                Value::Int(4),
                Value::Object(None),
                Value::Int(0),
                Value::Int(0),
                Value::Object(None),
            ],
        );
        assert!(
            r.is_err(),
            "invalid class bytes must throw, not return null"
        );
    }

    #[test]
    fn define_class1_out_of_bounds() {
        let mut ctx = mock_ctx();
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 2);
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

// ---------------------------------------------------------------------------
// SecurityManager.checkExec gating for process spawn (HIGH security)
// ---------------------------------------------------------------------------
//
// These tests verify the contract documented on `check_exec_or_throw`:
//
//   1. With no SecurityManager installed, the gate is a no-op and the
//      spawn proceeds (existing behaviour, preserves backwards compat).
//   2. With a SecurityManager that denies any exec, the gate propagates
//      `SecurityException` and `std::process::Command` is NEVER touched.
//   3. With a SecurityManager that allows only specific paths, only the
//      allowed paths reach the spawn syscall.
//
// We exercise the integration through `native_runtime_exec_string` and
// `native_pb_start` so any future refactor that bypasses
// `check_exec_or_throw` regresses these tests.
//
// MockNativeContext.invoke_virtual returns whatever's pre-armed in
// `invoke_virtual_result` (taken once), defaulting to `Ok(None)` вЂ”
// matching JDK's "no exception thrown == allowed" semantics. This lets us
// simulate both deny (pre-arm an Err) and allow (default).
#[cfg(test)]
mod checkexec_security_tests {
    use super::*;
    use crate::security_manager::set_security_manager_for_test;
    use crate::test_utils::mock_ctx;
    use cratonvm_types::error::{MethodCallFailed, RuntimeError, VmError};

    /// Helper: assert the failure is a SecurityException (regardless of
    /// the exact message вЂ” the wrapping is `MethodCallFailed::InternalError(
    /// VmError::Runtime(RuntimeError::SecurityException { .. }))`).
    fn assert_security_exception(err: &MethodCallFailed) {
        match err {
            MethodCallFailed::InternalError(VmError::Runtime(
                RuntimeError::SecurityException { .. },
            )) => {}
            other => panic!("expected RuntimeError::SecurityException, got {other:?}",),
        }
    }

    // -----------------------------------------------------------------------
    // (1) No SecurityManager вЂ” spawn gate is a no-op.
    // -----------------------------------------------------------------------

    #[test]
    fn no_security_manager_allows_check_exec() {
        // Ensure no SM is installed (defensive вЂ” other tests may have set one).
        let prev = set_security_manager_for_test(None);

        let mut ctx = mock_ctx();
        let result = check_exec_or_throw(&mut ctx, "/usr/bin/ls");
        assert!(
            result.is_ok(),
            "check_exec_or_throw must be a no-op with no SecurityManager, got {result:?}",
        );

        let _ = set_security_manager_for_test(prev);
    }

    // -----------------------------------------------------------------------
    // (2) SecurityManager that denies every checkExec вЂ” SecurityException
    //     propagates AND the spawn does NOT happen.
    // -----------------------------------------------------------------------

    #[test]
    fn denying_security_manager_blocks_check_exec() {
        let mut ctx = mock_ctx();
        let sm = alloc_concurrent_synthetic(&mut ctx, "java/lang/SecurityManager", 0);
        let prev = set_security_manager_for_test(Some(sm));

        // Pre-arm the mock so the next invoke_virtual returns a SecurityException.
        // This simulates a SecurityManager whose checkExec(String) denies.
        unsafe {
            *ctx.invoke_virtual_result.get() = Some(Err(MethodCallFailed::InternalError(
                VmError::Runtime(RuntimeError::SecurityException {
                    message: "access denied (test policy denies all exec)".to_string(),
                }),
            )));
        }

        let result = check_exec_or_throw(&mut ctx, "/usr/bin/evil");
        let err = result.expect_err("denying SM must surface SecurityException");
        assert_security_exception(&err);

        let _ = set_security_manager_for_test(prev);
    }

    #[test]
    fn denying_sm_blocks_runtime_exec_before_spawn() {
        // End-to-end check: a deny-all SM must short-circuit
        // native_runtime_exec_string with SecurityException вЂ” std::process::Command
        // is never invoked. Using a bogus program path proves no fallback
        // "Runtime.exec failed: ..." IOException can leak through, because
        // if the SM check is skipped the spawn would attempt the path and
        // surface IOException, not SecurityException.
        let mut ctx = mock_ctx();
        let sm = alloc_concurrent_synthetic(&mut ctx, "java/lang/SecurityManager", 0);
        let prev = set_security_manager_for_test(Some(sm));
        unsafe {
            *ctx.invoke_virtual_result.get() = Some(Err(MethodCallFailed::InternalError(
                VmError::Runtime(RuntimeError::SecurityException {
                    message: "deny".to_string(),
                }),
            )));
        }

        // args[0] = Runtime instance (irrelevant here), args[1] = command.
        let runtime_instance = alloc_concurrent_synthetic(&mut ctx, "java/lang/Runtime", 0);
        let cmd = ctx.create_string("/path/to/definitely-nonexistent-binary-xyz");
        let result = native_runtime_exec_string(
            &mut ctx,
            &[
                Value::Object(Some(runtime_instance)),
                Value::Object(Some(cmd)),
            ],
        );

        let err = result.expect_err("deny-all SM must block Runtime.exec spawn");
        assert_security_exception(&err);

        let _ = set_security_manager_for_test(prev);
    }

    #[test]
    fn denying_sm_blocks_processbuilder_start_stub() {
        // Same coverage for the simplified `native_pb_start` stub. Even
        // though this stub doesn't actually spawn, the SM gate runs first
        // so a future refactor that wires it to std::process::Command can
        // not silently bypass policy.
        let mut ctx = mock_ctx();
        let sm = alloc_concurrent_synthetic(&mut ctx, "java/lang/SecurityManager", 0);
        let prev = set_security_manager_for_test(Some(sm));
        unsafe {
            *ctx.invoke_virtual_result.get() = Some(Err(MethodCallFailed::InternalError(
                VmError::Runtime(RuntimeError::SecurityException {
                    message: "deny".to_string(),
                }),
            )));
        }

        // Build a ProcessBuilder synthetic with a 4-slot layout and a
        // command list. The native_pb_start stub reads slot 0; we plant a
        // String[] there with command[0] = "/bin/anything".
        let pb = alloc_concurrent_synthetic(&mut ctx, "java/lang/ProcessBuilder", 4);
        let cmd_arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 1);
        let prog = ctx.create_string("/bin/anything");
        ctx.set_array_element(cmd_arr, 0, Value::Object(Some(prog)));
        ctx.set_field(pb, 0, Value::Object(Some(cmd_arr)));

        let result = native_pb_start(&mut ctx, &[Value::Object(Some(pb))]);
        let err = result.expect_err("deny-all SM must block ProcessBuilder.start");
        assert_security_exception(&err);

        let _ = set_security_manager_for_test(prev);
    }

    // -----------------------------------------------------------------------
    // (3) Allow-specific-path SecurityManager вЂ” only listed paths spawn.
    // -----------------------------------------------------------------------
    //
    // The MockNativeContext's `invoke_virtual_result` is consumed by
    // `take()` per call, so for two back-to-back checkExec invocations we
    // pre-arm the mock once with Err (deny) for the first call, then leave
    // it unset so the second call falls through to the default `Ok(None)`
    // (allow). This mirrors a real SM that allows the second program but
    // denies the first.

    #[test]
    fn allow_listed_sm_lets_specific_paths_through() {
        let mut ctx = mock_ctx();
        let sm = alloc_concurrent_synthetic(&mut ctx, "java/lang/SecurityManager", 0);
        let prev = set_security_manager_for_test(Some(sm));

        // First call: simulate a denial for the disallowed binary.
        unsafe {
            *ctx.invoke_virtual_result.get() = Some(Err(MethodCallFailed::InternalError(
                VmError::Runtime(RuntimeError::SecurityException {
                    message: "deny /usr/bin/danger".to_string(),
                }),
            )));
        }
        let denied = check_exec_or_throw(&mut ctx, "/usr/bin/danger");
        let err = denied.expect_err("disallowed path must surface SecurityException");
        assert_security_exception(&err);

        // Second call: invoke_virtual_result was take()n on the previous
        // call, so the mock now falls back to its default Ok(None) вЂ”
        // simulating the allow-list permitting this program.
        let allowed = check_exec_or_throw(&mut ctx, "/bin/allowed-program");
        assert!(
            allowed.is_ok(),
            "allow-listed path must pass the gate, got {allowed:?}",
        );

        let _ = set_security_manager_for_test(prev);
    }
}
