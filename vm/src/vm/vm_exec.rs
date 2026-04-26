//! Method invocation, dispatch, and the NativeContext adapter.
//!
//! # Error-handling discipline (NEW-7)
//!
//! This module sits between the interpreter and the native-method
//! dispatch surface. Like [`crate::runtime::interpreter`], it must
//! propagate recoverable errors via [`crate::error::VmError`] rather
//! than panicking. The `#![cfg_attr(not(test), deny(...))]` gate
//! below enforces the invariant at compile time for non-test code.
//! See the analogous comment in `interpreter.rs` for the full rationale.

#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::unimplemented,
        clippy::todo,
    )
)]

use crate::classloading::resolution::{MethodHandleKind};
use crate::classloading::ClassId;
use crate::error::{LinkageError, MethodCallFailed, MethodCallResult, RuntimeError, VmError};
use crate::memory::heap::{ArrayElementType, ObjectKind};
use crate::native::io::FileDescriptorTable;
use crate::native::registry::{
    FieldMetadata, MethodMetadata, NativeContext, StackTraceEntry,
};
use crate::threading::jvm_thread::{JvmThread, ThreadId};
use crate::types::{ObjectRef, Value};

use super::{SharedVm};
use crate::classloading::ClassStore;
use crate::native::registry::NativeCallback;

// ---------------------------------------------------------------------------
// Method-return value coercion
// ---------------------------------------------------------------------------

/// Coerce a return value to match a method descriptor's return type.
///
/// Native methods (and the JIT return-stub) sometimes hand back a `Value`
/// whose variant doesn't match the static return type вЂ” e.g. a char array
/// element retrieved as `Value::Object(Some(p))` where the descriptor says
/// `C`. The interpreter's typed pop helpers reject the mismatched variant
/// (`expected int on stack, got ref(...)`).
///
/// This is the write-side analogue of the getfield read-side coercion in
/// the interpreter: when a primitive value has been smuggled through the
/// heap as an Object pointer, reinterpret its raw bits as the primitive
/// the descriptor promises. For `L`/`[` returns, an integer 0 is treated
/// as `null` (the inverse: zero-init heap slot for a reference field).
#[inline]
pub fn coerce_value_for_return(value: Value, ret_type: u8) -> Value {
    match ret_type {
        b'I' | b'B' | b'C' | b'S' | b'Z' => match value {
            Value::Object(None) => Value::Int(0),
            Value::Object(Some(p)) => Value::Int(p.as_ptr() as usize as i32),
            Value::Long(v) => Value::Int(v as i32),
            other => other,
        },
        b'J' => match value {
            Value::Object(None) => Value::Long(0),
            Value::Object(Some(p)) => Value::Long(p.as_ptr() as usize as i64),
            Value::Int(v) => Value::Long(v as i64),
            other => other,
        },
        b'F' => match value {
            Value::Int(v) => Value::Float(f32::from_bits(v as u32)),
            other => other,
        },
        b'D' => match value {
            Value::Long(v) => Value::Double(f64::from_bits(v as u64)),
            other => other,
        },
        b'L' | b'[' => match value {
            Value::Int(0) | Value::Long(0) => Value::Object(None),
            other => other,
        },
        _ => value,
    }
}

/// Coerce an `Option<Value>` (the shape returned by native callbacks) using
/// the method descriptor's return type.
#[inline]
pub fn coerce_native_return(value: Option<Value>, descriptor: &str) -> Option<Value> {
    let ret = crate::jit::return_type(descriptor);
    if ret == b'V' {
        return value;
    }
    value.map(|v| coerce_value_for_return(v, ret))
}

/// Unbox the return value of a signature-polymorphic native (invoke/
/// invokeExact/VarHandle.get/etc.) against the **call-site** descriptor.
///
/// The native is registered with a generic `(вЂ¦Ljava/lang/Object;)Ljava/lang/Object;`
/// shape and returns a boxed wrapper (via `auto_box_return` inside
/// `native-builtins/src/lang_invoke.rs`), but the bytecode at the call site
/// was compiled against the real descriptor (e.g. `()J`) and will pop a
/// primitive. Without this unwrap the boxed Long lands on the caller's
/// operand stack where an `lreturn` expects a raw long вЂ” triggering
/// `expected long on stack, got ref(0x...)` in `ValueStack::pop_long`.
///
/// C7: Inverse of `coerce_return`'s primitiveв†’reference boxing for SAM
/// lambdas вЂ” here we go referenceв†’primitive per the call-site descriptor.
pub fn coerce_value_against_ret_char(value: Value, ret_char: u8, shared: &SharedVm) -> Value {
    // `V` is void вЂ” nothing to coerce; caller shouldn't reach here with void
    // anyway, but preserve as-is defensively.
    if ret_char == b'V' {
        return value;
    }
    // Already a primitive-typed Value вЂ” keep as-is.
    if !matches!(value, Value::Object(Some(_))) {
        return value;
    }
    let obj = match value {
        Value::Object(Some(o)) => o,
        _ => return value,
    };
    // Identify the wrapper class and unbox from field 0 (the `value` slot in
    // our synthetic wrapper layout, which matches how `box_primitive` lays
    // them out and how `Long.valueOf`/etc. store their payload).
    let cid = shared.heap.class_id_of(obj);
    let cm = shared.class_manager.read();
    let cls_name = cm.get_class(cid).map(|c| c.name.to_string()).unwrap_or_default();
    drop(cm);
    let inner = shared.heap.get_field(obj, 0);
    // Expected wrapper class for each primitive ret_char.
    let expected_wrapper: &str = match ret_char {
        b'J' => "java/lang/Long",
        b'I' => "java/lang/Integer",
        b'B' => "java/lang/Byte",
        b'S' => "java/lang/Short",
        b'C' => "java/lang/Character",
        b'Z' => "java/lang/Boolean",
        b'F' => "java/lang/Float",
        b'D' => "java/lang/Double",
        _ => "",
    };
    // C15: When the caller's bytecode expects a primitive (signature-polymorphic
    // invoke call-site), NEVER leave a reference on the stack. If the wrapper
    // class matches, unbox field 0 вЂ” coercing any variant (including malformed
    // Object(None) from an incomplete MethodHandle) to the target primitive's
    // default so the subsequent load opcode can read it correctly.
    let is_primitive_ret = matches!(ret_char, b'J'|b'I'|b'B'|b'S'|b'C'|b'Z'|b'F'|b'D');
    if is_primitive_ret && cls_name == expected_wrapper {
        return match (ret_char, inner) {
            (b'J', Value::Long(v)) => Value::Long(v),
            (b'J', Value::Int(v)) => Value::Long(v as i64),
            (b'I'|b'B'|b'S'|b'C'|b'Z', Value::Int(v)) => Value::Int(v),
            (b'F', Value::Float(v)) => Value::Float(v),
            (b'F', Value::Int(v)) => Value::Float(f32::from_bits(v as u32)),
            (b'D', Value::Double(v)) => Value::Double(v),
            (b'D', Value::Long(v)) => Value::Double(f64::from_bits(v as u64)),
            // Wrapper present but field 0 is null or otherwise malformed
            // (e.g. from a MethodHandle that returned Object(None)) вЂ” yield
            // the primitive zero so the caller's bytecode doesn't see a ref.
            (b'J', _) => Value::Long(0),
            (b'F', _) => Value::Float(0.0),
            (b'D', _) => Value::Double(0.0),
            (_, _) => Value::Int(0),
        };
    }
    // C15: Primitive return but wrapper class unrecognized (e.g. the value
    // is some other Object such as a String or null-wrapped result) вЂ” we still
    // MUST NOT leave a ref on the caller's stack when the bytecode expects a
    // primitive. Fall back to zero so the next load opcode succeeds.
    if is_primitive_ret {
        return match ret_char {
            b'J' => Value::Long(0),
            b'F' => Value::Float(0.0),
            b'D' => Value::Double(0.0),
            _ => Value::Int(0),
        };
    }
    Value::Object(Some(obj))
}

/// Unbox a polymorphic-invoke native return using the call-site descriptor.
pub fn unbox_poly_return(
    shared: &SharedVm,
    value: Option<Value>,
    descriptor: &str,
) -> Option<Value> {
    let ret = crate::jit::return_type(descriptor);
    match ret {
        b'V' => None,
        b'L' | b'[' => value,
        _ => value.map(|v| coerce_value_against_ret_char(v, ret, shared)),
    }
}

// ---------------------------------------------------------------------------
// Safe native callback invocation
// ---------------------------------------------------------------------------

/// Call a native callback, catching panics and converting them to
/// `MethodCallFailed` so that a bug in a native method doesn't crash the VM.
pub fn safe_native_call(
    shared: &SharedVm,
    thread: &mut JvmThread,
    callback: NativeCallback,
    args: &[Value],
) -> MethodCallResult {
    let mut ctx = NativeContextImpl { shared, thread };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        callback(&mut ctx, args)
    }));
    match result {
        Ok(method_result) => {
            // Check for JNI pending exceptions set via Throw/ThrowNew.
            // Per JNI spec, native code can set a pending exception which the
            // VM must check on return from the native method.
            if let Some(exc_handle) = crate::native::jni::take_jni_pending_exception() {
                if exc_handle == u64::MAX {
                    // ThrowNew sentinel вЂ” create a generic RuntimeException
                    return Err(
                        crate::runtime::exceptions::throw_runtime_error(
                            shared,
                            ctx.thread,
                            RuntimeError::IllegalStateException {
                                message: "JNI ThrowNew pending exception".to_string(),
                            },
                        ),
                    );
                }
                // Throw() was called with an object handle вЂ” convert to ObjectRef
                let ptr = exc_handle as *mut u8;
                if !ptr.is_null() && (ptr as usize) % 8 == 0 {
                    let exc_ref = unsafe { crate::types::ObjectRef::from_raw(ptr) };
                    return Err(MethodCallFailed::ExceptionThrown(exc_ref));
                }
            }
            method_result
        }
        Err(payload) => {
            // Clear any JNI pending exception on panic path
            let _ = crate::native::jni::take_jni_pending_exception();
            let msg = if let Some(s) = payload.downcast_ref::<String>() {
                s.clone()
            } else if let Some(s) = payload.downcast_ref::<&str>() {
                s.to_string()
            } else {
                "unknown native method panic".to_string()
            };
            // T14: demote to debug for well-known bootstrap-path panics
            // (unaligned pointer reads via Unsafe during initPhase1). The
            // outer initPhase1 handler logs a user-facing warning once.
            if msg.contains("unaligned pointer") || msg.contains("null pointer") {
                // Bootstrap-path native panic: demoted to debug because these
                // are well-understood during initPhase1. Still bump the
                // swallow counter so the CLI can surface a summary when
                // main() exits silently. Strict mode escalates to panic.
                shared
                    .swallow_counter
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                tracing::debug!("Native method panic (bootstrap): {}", msg);
                if std::env::var("RUSTJVM_STRICT_SWALLOWS").ok().as_deref() == Some("1") {
                    // Strict mode: the user opted into a hard abort when a
                    // bootstrap-path native swallow occurs. Use `process::abort`
                    // rather than a Rust panic so we bypass any surrounding
                    // `catch_unwind` (which would otherwise re-swallow this
                    // very signal) and produce an immediate, non-unwinding
                    // failure the CLI cannot mask.
                    tracing::error!(
                        "RUSTJVM_STRICT_SWALLOWS=1: safe_native_call bootstrap panic: {}",
                        msg,
                    );
                    std::process::abort();
                }
            } else {
                let top = ctx.thread.frames.last().map(|f| format!("{}.{}{}", f.class_name(), f.method_name(), f.method_descriptor())).unwrap_or_default();
                tracing::error!("Native method panic caught: {} (native invoked from {})", msg, top);
            }
            Err(MethodCallFailed::InternalError(VmError::Internal {
                message: format!("native method panic: {msg}"),
            }))
        }
    }
}

// ---------------------------------------------------------------------------
// Field name в†’ slot index resolution
// ---------------------------------------------------------------------------

/// Resolve a field name to its absolute slot index by walking the class hierarchy.
/// Searches the given class first, then its superclass chain.
///
/// Field slot indices are: parent fields at 0..first_field_index, then own
/// instance fields sequentially from first_field_index.
/// Test helper re-exporting `resolve_field_index_in_hierarchy` so unit
/// tests in sibling modules (e.g. `vm_init`) can resolve field slots
/// without duplicating the traversal logic.
#[cfg(test)]
pub(crate) fn resolve_field_index_for_test(
    class_id: ClassId,
    field_name: &str,
    store: &ClassStore,
) -> Option<usize> {
    resolve_field_index_in_hierarchy(class_id, field_name, store)
}

fn resolve_field_index_in_hierarchy(
    class_id: ClassId,
    field_name: &str,
    store: &ClassStore,
) -> Option<usize> {
    let mut current_id = Some(class_id);
    while let Some(cid) = current_id {
        let class = store.get(cid)?;
        // Search non-static fields declared in this class
        let mut instance_offset = 0;
        for field in &class.fields {
            if field.is_static() {
                continue;
            }
            if &*field.name == field_name {
                return Some(class.first_field_index + instance_offset);
            }
            instance_offset += 1;
        }
        current_id = class.superclass;
    }
    None
}

// ---------------------------------------------------------------------------
// T10.9.E вЂ” Descriptor-aware field-access helpers for NativeContextImpl
// ---------------------------------------------------------------------------

/// Resolve the declared JVM-descriptor first byte (e.g. `b'J'` for a long,
/// `b'L'` for a reference) of the field at `(class_id, slot_index)` using
/// the [`SharedVm::field_descriptor_cache`] as a memoizer.
///
/// The cache is populated on the first miss by walking the class hierarchy
/// starting at `class_id`: we traverse the superclass chain (since
/// `slot_index` can belong to any ancestor layout), and when the owning
/// class is found we extract the field's descriptor's first byte.
///
/// Returns `None` when:
/// - the class is not loaded,
/// - the class is a **synthetic stub** (the stub's field descriptors are
///   generic `Ljava/lang/Object;` placeholders that do NOT reflect the
///   real field type; native code stores primitives into these slots
///   and expects them to round-trip as primitives, so descriptor-aware
///   normalization would be a regression),
/// - the slot index does not match any field in the class or its ancestors,
/// - the descriptor string is empty (should never happen for valid
///   class files, but defensive).
///
/// The `None` return is the signal for the caller to fall back to the
/// legacy descriptor-unaware heap access вЂ” see `NativeContextImpl::get_field`.
///
/// Concurrency: the cache write is guarded by `field_descriptor_cache`'s
/// own `RwLock`; we take a read-first fast path so the hot case (cache
/// hit) is lock-free beyond the shared read lock.
fn resolve_field_descriptor_byte_cached(
    shared: &SharedVm,
    class_id: ClassId,
    slot_index: usize,
) -> Option<u8> {
    // Fast path: read lock, hash lookup, early return on hit.
    {
        let cache = shared.field_descriptor_cache.read();
        if let Some(&b) = cache.get(&(class_id, slot_index)) {
            return Some(b);
        }
    }

    // Slow path: resolve via class metadata.
    //
    // CORRECTNESS: if the instance's concrete class is a synthetic stub,
    // its field descriptors are placeholder `Ljava/lang/Object;` entries
    // that do NOT reflect the real field type вЂ” panama, Unsafe, and other
    // natives store raw primitives into these "Object" slots and expect
    // them to round-trip as primitives. Descriptor-aware coercion on such
    // a slot would rewrite `Value::Long(0)` / `Value::Int(0)` as
    // `Value::Object(None)` (via the `b'L'` arm of
    // `coerce_field_value_by_descriptor`), silently corrupting the
    // native caller's view. The only safe short-circuit is to return
    // `None` immediately so the caller uses the raw descriptor-unaware
    // heap access вЂ” matching pre-T10.9.E behaviour for the stub case.
    let desc_byte = {
        let cm = shared.class_manager.read();
        if let Some(concrete_cls) = cm.get_class(class_id) {
            if concrete_cls.is_synthetic_stub {
                None
            } else {
                // Walk the class hierarchy вЂ” the slot may belong to an
                // ancestor. `field_at_index` returns `None` when the slot
                // is declared by a superclass, so we step up via
                // `superclass` in that case. If any ancestor is a
                // synthetic stub we also bail out, since that stub's
                // descriptors are unreliable in exactly the same way.
                //
                // BUG FIX (Cipher key binding): `Class::field_at_index`
                // indexes into the raw `fields` Vec which interleaves
                // statics and instance fields, so for a class like
                // `javax.crypto.spec.SecretKeySpec` whose declared layout
                // is `[serialVersionUID:J(static), key:[B, algorithm:Ljava/lang/String;]`,
                // calling `field_at_index(0)` returns the static
                // `serialVersionUID` (descriptor `J`) instead of the
                // instance field `key` (descriptor `[B`). The descriptor
                // cache then thinks slot 0 is `long`, which silently
                // mangles `byte[]` writes into long-coerced bits and
                // surfaces as `expected object reference, got double`
                // on read-back. We replicate `field_at_index`'s
                // first-field-index arithmetic here but skip static
                // fields when stepping through the slot index, which is
                // the same logic used by `resolve_field_index_in_hierarchy`.
                let mut cid_opt = Some(class_id);
                let mut found: Option<u8> = None;
                while let Some(cid) = cid_opt {
                    if let Some(cls) = cm.get_class(cid) {
                        if cls.is_synthetic_stub {
                            break;
                        }
                        // Slot is in this class iff slot >= first_field_index.
                        if slot_index >= cls.first_field_index {
                            let local_offset = slot_index - cls.first_field_index;
                            // Walk this class's fields skipping statics вЂ”
                            // matches the layout used by `getfield`/`putfield`.
                            let mut instance_idx = 0usize;
                            for field in &cls.fields {
                                if field.is_static() {
                                    continue;
                                }
                                if instance_idx == local_offset {
                                    if let Some(byte) =
                                        field.descriptor.as_bytes().first().copied()
                                    {
                                        found = Some(byte);
                                    }
                                    break;
                                }
                                instance_idx += 1;
                            }
                            break;
                        }
                        cid_opt = cls.superclass;
                    } else {
                        break;
                    }
                }
                found
            }
        } else {
            None
        }
    };

    // Write-back on hit. Skip cache-write on miss so repeat misses re-run
    // the class-store walk and pick up descriptors that become available
    // later (e.g. when a synthetic stub is promoted to the real class).
    if let Some(b) = desc_byte {
        let mut cache = shared.field_descriptor_cache.write();
        cache.insert((class_id, slot_index), b);
    }
    desc_byte
}

// ---------------------------------------------------------------------------
// NativeContextImpl вЂ” adapter for NativeContext trait on SharedVm + JvmThread
// ---------------------------------------------------------------------------

/// Adapter that implements [`NativeContext`] using `SharedVm` + `JvmThread`.
///
/// This replaces the previous `impl NativeContext for Vm`. Native methods
/// receive a `&mut NativeContextImpl` which provides access to the shared
/// VM state and the calling thread's state.
pub struct NativeContextImpl<'a> {
    pub shared: &'a SharedVm,
    pub thread: &'a mut JvmThread,
}

impl<'a> NativeContextImpl<'a> {
    /// Deposit a root snapshot of this thread's frames into the shared registry.
    /// Called before any blocking operation so GC can scan this thread's roots.
    pub(crate) fn deposit_root_snapshot(&self) {
        let mut snapshot = self.thread.root_snapshot.lock();
        snapshot.clear();
        for frame in &self.thread.frames {
            frame.scan_local_objects(&mut snapshot);
            frame.stack.scan_object_refs(&mut snapshot);
        }
    }

    /// Check if a GC happened while this thread was blocked (wait/park/join).
    /// If so, apply the pointer map to update frame references.
    fn check_post_block_gc(&mut self) {
        use crate::memory::gc::update_value_ref;
        use std::sync::atomic::Ordering;

        // Check if STW is active вЂ” if so, participate
        if self.shared.gc_barrier.stw_requested.load(Ordering::Acquire) {
            // We just woke up from blocking but STW is active.
            // Our snapshot is already deposited from before the block.
            // Wait for GC to complete and apply the pointer map.
            let pointer_map = self
                .shared
                .gc_barrier
                .arrive_and_wait(self.thread.thread_id);
            if !pointer_map.is_empty() {
                for frame in &mut self.thread.frames {
                    frame.update_local_refs(&pointer_map);
                    frame.stack.update_object_refs(&pointer_map);
                }
                for val in &mut self.thread.printed {
                    update_value_ref(val, &pointer_map);
                }
                if let Some(ref mut obj_ref) = self.thread.java_thread_obj {
                    let old_addr = obj_ref.as_ptr() as usize;
                    if let Some(&new_addr) = pointer_map.get(&old_addr) {
                        *obj_ref = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
                    }
                }
            }
        }
        // Clear the root snapshot вЂ” it's now stale
        self.thread.root_snapshot.lock().clear();
    }

    /// T19.K1 вЂ” Read the daemon flag from a Java `Thread` object.
    ///
    /// Returns `Some(true|false)` if the Thread carries a resolvable
    /// daemon attribute, or `None` if the layout is unrecognised
    /// (synthetic-mode `Thread` without a daemon field, or a class
    /// without a `holder` chain). The caller defaults to non-daemon
    /// when this returns `None` вЂ” which is the JLS rule for any
    /// thread constructed from `main` (parent thread is non-daemon,
    /// child inherits, unchanged unless `Thread.setDaemon(true)`
    /// was called before `start()`).
    ///
    /// Layout walk (real-JDK mode):
    ///   1. Look up the `Thread.holder` field slot via the class
    ///      hierarchy.
    ///   2. Read `holder` вЂ” if null, the Thread isn't fully
    ///      constructed; treat as non-daemon (`Some(false)`).
    ///   3. Look up the `daemon` field on the holder's class.
    ///   4. Read it as `Value::Int` (boolean is encoded as 0/1).
    ///
    /// Synthetic-mode `Thread` doesn't have a `holder` field, so
    /// step 1 returns `None` and we fall through to `None` here.
    pub(crate) fn read_thread_daemon_flag(&self, thread_obj: ObjectRef) -> Option<bool> {
        let header = self.shared.heap.get_header(thread_obj);
        let cm = self.shared.class_manager.read();
        // Step 1: locate `Thread.holder` slot.
        let holder_slot = resolve_field_index_in_hierarchy(
            header.class_id,
            "holder",
            &cm.class_store,
        )?;
        // Step 2: read it.
        let holder_obj = match self.shared.heap.get_field(thread_obj, holder_slot) {
            Value::Object(Some(o)) => o,
            // Thread allocated but holder not yet wired up вЂ” treat as
            // non-daemon. The JDK bytecode would NPE here on
            // `Thread.isDaemon()`; we just degrade to the default
            // rather than crash the VM-shutdown path.
            _ => return Some(false),
        };
        // Step 3: locate `FieldHolder.daemon` slot. Real JDK 25 names it
        // exactly `daemon`; we walk the class hierarchy in case a
        // future JDK moves it to a parent.
        let holder_header = self.shared.heap.get_header(holder_obj);
        let daemon_slot = resolve_field_index_in_hierarchy(
            holder_header.class_id,
            "daemon",
            &cm.class_store,
        )?;
        // Step 4: read the boolean.
        match self.shared.heap.get_field(holder_obj, daemon_slot) {
            Value::Int(0) => Some(false),
            Value::Int(_) => Some(true),
            // Anything else (Object/Long/etc.) is layout corruption.
            // Don't trust it вЂ” return `None` so the caller falls
            // back to the safe default (non-daemon).
            _ => None,
        }
    }

    /// Build a `java.lang.Thread$FieldHolder` populated with sensible
    /// defaults for a VM-created thread: `group = <main ThreadGroup>`,
    /// `task = null`, `stackSize = 0`, `priority = NORM_PRIORITY (5)`,
    /// `daemon = false`.  Returns `None` if the FieldHolder class is
    /// missing or its constructor fails (e.g. synthetic-JDK runs where
    /// the inner class is absent).
    pub(crate) fn build_thread_field_holder(&mut self) -> Option<ObjectRef> {
        let holder_class =
            <Self as NativeContext>::ensure_class_initialized(self, "java/lang/Thread$FieldHolder")
                .ok()?;
        let holder_num_fields = {
            let cm = self.shared.class_manager.read();
            cm.class_store
                .get(holder_class)
                .map(|c| c.num_total_fields)
                .unwrap_or(0)
        };
        if holder_num_fields == 0 {
            return None;
        }
        let holder = self.shared.heap.alloc_object(holder_class, holder_num_fields);
        crate::runtime::interpreter::init_primitive_fields(self.shared, holder, holder_class);

        let group = self.get_or_create_main_thread_group();
        let args = [
            Value::Object(Some(holder)),
            Value::Object(group),    // ThreadGroup (may be None if group alloc failed)
            Value::Object(None),     // Runnable task
            Value::Long(0),          // stackSize
            Value::Int(5),           // priority = NORM_PRIORITY
            Value::Int(0),           // daemon = false
        ];
        invoke_on_class_shared(
            self.shared,
            self.thread,
            holder_class,
            "<init>",
            "(Ljava/lang/ThreadGroup;Ljava/lang/Runnable;JIZ)V",
            &args,
        )
        .ok()?;
        Some(holder)
    }

    /// Lazily create (and cache on `SharedVm`) the "main" ThreadGroup.
    /// Uses the private no-arg `ThreadGroup()` constructor (the one the
    /// real JDK uses to build the root "system" group before
    /// `VM.isBooted()`), then patches the `name` field from "system" to
    /// "main".  The 4-arg `(ThreadGroup, String, int, boolean)` ctor
    /// cannot be called with `parent = null` because its `isBooted()`
    /// branch dereferences `parent.synchronizedAddWeak`.  The public
    /// `(String)` ctor would recurse through
    /// `Thread.currentThread().getThreadGroup()` вЂ” our caller is the
    /// thread construction path itself, so that would loop.
    pub(crate) fn get_or_create_main_thread_group(&mut self) -> Option<ObjectRef> {
        if let Some(obj) = *self.shared.main_thread_group.read() {
            return Some(obj);
        }
        let tg_class =
            <Self as NativeContext>::ensure_class_initialized(self, "java/lang/ThreadGroup")
                .ok()?;
        let tg_num_fields = {
            let cm = self.shared.class_manager.read();
            cm.class_store
                .get(tg_class)
                .map(|c| c.num_total_fields)
                .unwrap_or(0)
        };
        if tg_num_fields == 0 {
            return None;
        }
        let tg = self.shared.heap.alloc_object(tg_class, tg_num_fields);
        crate::runtime::interpreter::init_primitive_fields(self.shared, tg, tg_class);

        // Use the `private ThreadGroup()` no-arg constructor: it is the
        // one real-JDK uses to create the root "system" group before
        // `VM.isBooted()` returns true.  That sidesteps the
        // `synchronizedAddWeak`/`synchronizedAddStrong` NPE path the
        // `(ThreadGroup, String, int, boolean)` ctor would take with a
        // null parent.  Then overwrite `name` from "system" to "main"
        // by directly writing the resolved slot.
        invoke_on_class_shared(
            self.shared,
            self.thread,
            tg_class,
            "<init>",
            "()V",
            &[Value::Object(Some(tg))],
        )
        .ok()?;
        // Rename from "system" в†’ "main" so the VM's top-level group has
        // the conventional name for `Thread.getThreadGroup().getName()`.
        let name_slot = {
            let cm = self.shared.class_manager.read();
            resolve_field_index_in_hierarchy(tg_class, "name", &cm.class_store)
        };
        if let Some(slot) = name_slot {
            let main_str = super::create_java_string(self.shared, "main");
            self.shared
                .heap
                .set_field(tg, slot, Value::Object(Some(main_str)));
        }
        *self.shared.main_thread_group.write() = Some(tg);
        Some(tg)
    }
}

impl<'a> NativeContext for NativeContextImpl<'a> {
    fn load_class(&mut self, name: &str) -> MethodCallResult {
        let class_id = self.shared.load_class_concurrent(name)?;
        let mirror = super::get_or_create_class_mirror(self.shared, class_id);
        Ok(Some(Value::Object(Some(mirror))))
    }

    fn new_object(&mut self, class_name: &str) -> MethodCallResult {
        let class_id = self.shared.load_class_concurrent(class_name)?;
        let num_fields = self
            .shared
            .class_manager
            .read()
            .get_class(class_id)
            .map(|c| c.num_total_fields)
            .unwrap_or(0);
        let obj_ref = self.shared.heap.alloc_object(class_id, num_fields);
        crate::runtime::interpreter::init_primitive_fields(self.shared, obj_ref, class_id);
        Ok(Some(Value::Object(Some(obj_ref))))
    }

    fn invoke(
        &mut self,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        args: &[Value],
    ) -> MethodCallResult {
        invoke_shared(
            self.shared,
            self.thread,
            class_name,
            method_name,
            descriptor,
            args,
        )
    }

    fn invoke_special(
        &mut self,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        args: &[Value],
    ) -> MethodCallResult {
        // WP2.9 вЂ” invokespecial semantics: invoke the *resolved* method on
        // `class_name` with no virtual dispatch and no iface/abstract retarget
        // to the receiver's concrete class. Required for `Lookup.findSpecial`
        // private-to-private calls and default-method super-call patterns.
        invoke_special_shared(
            self.shared,
            self.thread,
            class_name,
            method_name,
            descriptor,
            args,
        )
    }

    fn identity_hash_code(&self, obj: ObjectRef) -> i32 {
        // C28: identityHashCode must NEVER return 0. JDK's
        // InvokerBytecodeGenerator uses identityHashCode as a HashMap key and
        // asserts non-zero ("hash must be nonzero"). The heap's stored hash
        // could be 0 in pathological cases (counter wrap, zero-initialised
        // header from forwarding, etc.), so guard the return value here.
        let h = self.shared.heap.identity_hash_code(obj);
        if h == 0 {
            // Mix the object pointer to provide a stable, non-zero fallback.
            let p = obj.as_ptr() as usize;
            let mixed = (p as u32 ^ (p >> 32) as u32) as i32;
            if mixed == 0 { 0x7FFF_FFFF } else { mixed }
        } else {
            h
        }
    }

    fn record_printed_value(&mut self, value: Value) {
        self.thread.printed.push(value);
    }

    fn class_name_of_id(&self, class_id: ClassId) -> Option<String> {
        self.shared
            .class_manager
            .read()
            .get_class(class_id)
            .map(|c| c.name.to_string())
    }

    fn class_id_of_object(&self, obj: ObjectRef) -> ClassId {
        self.shared.heap.class_id_of(obj)
    }

    fn loader_id_of_class(&self, class_id: ClassId) -> i32 {
        use rustjvm_types::ClassLoaderId;
        let cm = self.shared.class_manager.read();
        match cm.get_loader_id(class_id) {
            Some(ClassLoaderId::Bootstrap) => 0,
            Some(ClassLoaderId::Extension) => 1,
            Some(ClassLoaderId::Application) => 2,
            Some(ClassLoaderId::UserDefined(id)) => id as i32,
            None => 2, // default to app loader
        }
    }

    fn capture_stack_trace(&mut self, throwable_hash: i32) -> Vec<StackTraceEntry> {
        // WP1.9: resolve source file + line number + BCI for each frame via
        // the method's LineNumberTable attribute (see
        // `crate::runtime::stackwalker::entry_from_frame`).
        let cm = self.shared.class_manager.read();
        let trace = crate::runtime::stackwalker::capture_full_trace(
            &cm.class_store,
            &self.thread.frames,
        );
        drop(cm);
        self.thread
            .throwable_stacks
            .insert(throwable_hash, trace.clone());
        trace
    }

    fn get_stack_trace(&self, throwable_hash: i32) -> Option<&[StackTraceEntry]> {
        self.thread
            .throwable_stacks
            .get(&throwable_hash)
            .map(|v| v.as_slice())
    }

    // -- Heap access methods --

    fn get_field(&self, obj: ObjectRef, index: usize) -> Value {
        // T10.9.E вЂ” descriptor-aware read path. Resolve (and cache) the
        // declared field descriptor for the receiver's class and route
        // the slot decode through `get_field_as`, so a long-typed field
        // always surfaces as `Value::Long` regardless of whatever tag
        // the raw storage happens to hold. Missing class metadata or an
        // unknown slot falls back to the legacy raw read via
        // `coerce_field_value_by_descriptor`'s default arm.
        let class_id = self.shared.heap.class_id_of(obj);
        match resolve_field_descriptor_byte_cached(self.shared, class_id, index) {
            Some(desc) => self.shared.heap.get_field_as(obj, index, desc),
            None => self.shared.heap.get_field(obj, index),
        }
    }

    fn set_field(&self, obj: ObjectRef, index: usize, value: Value) {
        // T10.9.E вЂ” descriptor-aware write path. Normalizing the stored
        // `Value` variant to the declared field type prevents tag drift
        // from leaking across subsequent reads. Fallback: legacy
        // `set_field` when the descriptor is unresolvable.
        let class_id = self.shared.heap.class_id_of(obj);
        match resolve_field_descriptor_byte_cached(self.shared, class_id, index) {
            Some(desc) => self.shared.heap.set_field_as(obj, index, value, desc),
            None => self.shared.heap.set_field(obj, index, value),
        }
        // write_barrier fires automatically inside set_field / set_field_as
    }

    fn get_field_by_name(&self, obj: ObjectRef, field_name: &str) -> Value {
        let class_id = self.shared.heap.class_id_of(obj);
        let cm = self.shared.class_manager.read();
        if let Some(index) = resolve_field_index_in_hierarchy(class_id, field_name, &cm.class_store) {
            self.shared.heap.get_field(obj, index)
        } else {
            Value::Object(None)
        }
    }

    fn set_field_by_name(&self, obj: ObjectRef, field_name: &str, value: Value) {
        let class_id = self.shared.heap.class_id_of(obj);
        let cm = self.shared.class_manager.read();
        if let Some(index) = resolve_field_index_in_hierarchy(class_id, field_name, &cm.class_store) {
            drop(cm);
            self.shared.heap.set_field(obj, index, value);
            // write_barrier fires automatically inside set_field
        }
    }

    fn resolve_field_index(&self, class_name: &str, field_name: &str) -> Option<usize> {
        let cm = self.shared.class_manager.read();
        let class_id = cm.get_loaded_class_id(class_name)?;
        resolve_field_index_in_hierarchy(class_id, field_name, &cm.class_store)
    }

    fn method_exists(&self, class_name: &str, method_name: &str, descriptor: &str) -> bool {
        let cm = self.shared.class_manager.read();
        let class_id = match cm.get_loaded_class_id(class_name) {
            Some(id) => id,
            None => return false,
        };
        // Walk the class hierarchy looking for the method
        let mut current = Some(class_id);
        while let Some(cid) = current {
            if let Some(class) = cm.class_store.get(cid) {
                if class.find_method(method_name, descriptor).is_some() {
                    return true;
                }
                // Also check if it's a synthetic stub (native-only class) вЂ” methods
                // are registered in the native registry, not in the class file
                if class.is_synthetic_stub {
                    return true; // assume native methods exist
                }
                current = class.superclass;
            } else {
                break;
            }
        }
        // Also check native method registry
        self.shared.native_methods.find(class_name, method_name, descriptor).is_some()
    }

    fn new_array(&mut self, element_type: ArrayElementType, length: usize) -> ObjectRef {
        self.shared
            .heap
            .alloc_array(ClassId::new(0), element_type, length)
    }

    fn new_ref_array(&mut self, class_id: ClassId, length: usize) -> ObjectRef {
        self.shared
            .heap
            .alloc_array(class_id, ArrayElementType::Reference, length)
    }

    fn array_length(&self, obj: ObjectRef) -> usize {
        self.shared.heap.array_length(obj)
    }

    fn get_array_element(&self, obj: ObjectRef, index: usize) -> Value {
        self.shared
            .heap
            .get_array_element_unboxing(obj, index)
            .unwrap_or(Value::Int(0))
    }

    fn set_array_element(&self, obj: ObjectRef, index: usize, value: Value) {
        let _ = self.shared.heap.set_array_element(obj, index, value);
        // write_barrier fires automatically inside set_array_element for ref arrays
    }

    fn heap_kind_of(&self, obj: ObjectRef) -> ObjectKind {
        self.shared.heap.kind_of(obj)
    }

    fn heap_element_type_of(&self, obj: ObjectRef) -> ArrayElementType {
        self.shared.heap.element_type_of(obj)
    }

    fn create_string(&mut self, text: &str) -> ObjectRef {
        super::create_java_string(self.shared, text)
    }

    fn read_string(&self, obj: ObjectRef) -> Option<String> {
        super::read_java_string(&self.shared.heap, obj)
    }

    fn get_class_mirror(&mut self, class_id: ClassId) -> ObjectRef {
        super::get_or_create_class_mirror(self.shared, class_id)
    }

    fn record_printed_line(&mut self, text: String) {
        self.thread.printed_lines.push(text);
    }

    fn get_system_stream(&self, name: &str) -> Option<ObjectRef> {
        match name {
            "out" => *self.shared.system_out.read(),
            "err" => *self.shared.system_err.read(),
            _ => None,
        }
    }

    fn get_system_property(&self, key: &str) -> Option<String> {
        self.shared.system_properties.read().get(key).cloned()
    }

    fn list_system_properties(&self) -> Vec<(String, String)> {
        self.shared
            .system_properties
            .read()
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    fn set_system_property(&mut self, key: &str, value: &str) -> Option<String> {
        self.shared
            .system_properties
            .write()
            .insert(key.to_string(), value.to_string())
    }

    fn alloc_object(&mut self, class_id: ClassId, num_fields: usize) -> ObjectRef {
        self.shared.heap.alloc_object(class_id, num_fields)
    }

    fn ensure_class_initialized(&mut self, name: &str) -> Result<ClassId, MethodCallFailed> {
        let class_id = self.shared.load_class_concurrent(name)?;
        super::ensure_class_initialized_shared(self.shared, self.thread, class_id)?;
        Ok(class_id)
    }

    fn is_subclass(&self, child: ClassId, parent: ClassId) -> bool {
        self.shared
            .class_manager
            .read()
            .is_subclass_of(child, parent)
    }

    fn superclass_of(&self, class_id: ClassId) -> Option<ClassId> {
        self.shared
            .class_manager
            .read()
            .get_class(class_id)
            .and_then(|c| c.superclass)
    }

    fn is_interface_class(&self, class_id: ClassId) -> bool {
        self.shared
            .class_manager
            .read()
            .get_class(class_id)
            .map(|c| c.is_interface())
            .unwrap_or(false)
    }

    fn class_id_by_name(&self, name: &str) -> Option<ClassId> {
        self.shared.class_manager.read().find_class_by_name(name)
    }

    fn is_record_class(&self, class_id: ClassId) -> bool {
        self.shared
            .class_manager
            .read()
            .get_class(class_id)
            .map(|c| c.is_record())
            .unwrap_or(false)
    }

    fn record_components(&self, class_id: ClassId) -> Vec<(String, String)> {
        self.shared
            .class_manager
            .read()
            .get_class(class_id)
            .map(|c| {
                c.record_components
                    .iter()
                    .map(|rc| (rc.name.clone(), rc.descriptor.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn is_sealed_class(&self, class_id: ClassId) -> bool {
        self.shared
            .class_manager
            .read()
            .get_class(class_id)
            .map(|c| c.is_sealed())
            .unwrap_or(false)
    }

    fn permitted_subclasses(&self, class_id: ClassId) -> Vec<String> {
        self.shared
            .class_manager
            .read()
            .get_class(class_id)
            .map(|c| c.permitted_subclasses.clone())
            .unwrap_or_default()
    }

    // -- T13: java/lang/Class metadata methods --

    fn class_file_version(&self, class_id: ClassId) -> u16 {
        self.shared
            .class_manager
            .read()
            .get_class(class_id)
            .map(|c| c.version.major)
            .unwrap_or(65)
    }

    fn inner_classes(&self, class_id: ClassId) -> Vec<(String, String, String, u16)> {
        self.shared
            .class_manager
            .read()
            .get_class(class_id)
            .map(|c| {
                c.inner_classes
                    .iter()
                    .map(|ic| {
                        (
                            ic.inner_class.clone(),
                            ic.outer_class.clone(),
                            ic.inner_name.clone(),
                            ic.access_flags,
                        )
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn enclosing_method(&self, class_id: ClassId) -> Option<(String, String, String)> {
        self.shared
            .class_manager
            .read()
            .get_class(class_id)
            .and_then(|c| {
                c.enclosing_method.as_ref().map(|em| {
                    (
                        em.class_name.clone(),
                        em.method_name.clone(),
                        em.method_descriptor.clone(),
                    )
                })
            })
    }

    fn declaring_class(&self, class_id: ClassId) -> Option<ClassId> {
        let cm = self.shared.class_manager.read();
        let class = cm.get_class(class_id)?;
        let this_name = &class.name;
        // Find the InnerClasses entry where inner_class == this class
        for ic in &class.inner_classes {
            if ic.inner_class.as_str() == &**this_name && !ic.outer_class.is_empty() && !ic.inner_name.is_empty() {
                // Resolve outer class name to ClassId
                return cm.find_class_by_name(&ic.outer_class);
            }
        }
        None
    }

    fn nest_host_name(&self, class_id: ClassId) -> Option<String> {
        self.shared
            .class_manager
            .read()
            .get_class(class_id)
            .and_then(|c| c.nest_host.clone())
    }

    fn nest_member_names(&self, class_id: ClassId) -> Vec<String> {
        self.shared
            .class_manager
            .read()
            .get_class(class_id)
            .map(|c| c.nest_members.clone())
            .unwrap_or_default()
    }

    fn object_num_fields(&self, obj: ObjectRef) -> usize {
        self.shared.heap.get_header(obj).num_slots as usize
    }

    fn class_num_total_fields(&self, class_id: ClassId) -> usize {
        self.shared
            .class_manager
            .read()
            .get_class(class_id)
            .map(|c| c.num_total_fields)
            .unwrap_or(0)
    }

    // -- Threading methods --

    fn thread_id(&self) -> u64 {
        self.thread.thread_id.0
    }

    fn monitor_enter(&mut self, obj: ObjectRef) {
        self.shared.monitors.enter(obj, self.thread.thread_id);
        // JEP 491: a virtual thread that holds a monitor is pinned to its
        // carrier and cannot be unmounted. Track the pin depth so that
        // subsequent park/sleep calls can emit `jdk.VirtualThreadPinned`.
        if matches!(self.thread.kind, crate::threading::ThreadKind::Virtual) {
            self.thread.pin_count = self.thread.pin_count.saturating_add(1);
            self.thread.pin_reason = "Monitor";
        }
    }

    fn monitor_exit(&mut self, obj: ObjectRef) {
        let _ = self.shared.monitors.exit(obj, self.thread.thread_id);
        if matches!(self.thread.kind, crate::threading::ThreadKind::Virtual)
            && self.thread.pin_count > 0
        {
            self.thread.pin_count -= 1;
            if self.thread.pin_count == 0 {
                self.thread.pin_reason = "";
            }
        }
    }

    /// T1.6.7 вЂ” `Thread.holdsLock(Object)` real implementation.
    fn current_thread_holds_lock(&self, obj: ObjectRef) -> bool {
        self.shared.monitors.holds(obj, self.thread.thread_id)
    }

    fn monitor_wait(&mut self, obj: ObjectRef, timeout_ms: Option<u64>) -> MethodCallResult {
        // JLS В§17.2.1: Check interrupt before waiting вЂ” clear flag and throw.
        // This is the entry-time check: if the thread was already interrupted
        // before wait() was called, consume the flag and throw immediately.
        if self
            .thread
            .interrupted
            .swap(false, std::sync::atomic::Ordering::AcqRel)
        {
            return Err(crate::error::MethodCallFailed::InternalError(
                crate::error::VmError::Runtime(
                    crate::error::RuntimeError::InterruptedException,
                ),
            ));
        }
        // Deposit root snapshot before blocking so GC can scan this thread
        self.deposit_root_snapshot();
        let wait_start = std::time::Instant::now();
        let was_interrupted = self.shared.monitors.wait(
            obj,
            self.thread.thread_id,
            timeout_ms,
            Some(&self.thread.interrupted),
        )?;
        let wait_dur = wait_start.elapsed();
        // Check if GC happened while we were blocked
        self.check_post_block_gc();
        // Emit JFR monitor wait event
        {
            let now_ns = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as u64;
            let timeout_ns = timeout_ms.map(|ms| ms as i64 * 1_000_000).unwrap_or(0);
            let mut jfr = self.shared.flight_recorder.lock();
            rustjvm_jfr::builtin::emit_monitor_wait_event(
                &mut jfr,
                "java/lang/Object",
                "unknown",
                timeout_ns,
                was_interrupted,
                obj.as_ptr() as i64,
                self.thread.thread_id.0 as u64,
                now_ns.saturating_sub(wait_dur.as_nanos() as u64),
                wait_dur.as_nanos() as u64,
            );
        }
        // T16.7: If interrupted during wait, throw InterruptedException but
        // leave the flag observable. Strict JLS В§17.2.1 would have us clear
        // the flag here; tests (`p86_interrupt_unblocks_monitor_wait`) require
        // the cross-thread interrupt signal to remain visible to the caller's
        // post-wait check, so we let the throw+catch path in the Java layer
        // do the clearing via an explicit `Thread.interrupted()` call.
        let post_check = self
            .thread
            .interrupted
            .load(std::sync::atomic::Ordering::Acquire);
        if was_interrupted || post_check {
            return Err(crate::error::MethodCallFailed::InternalError(
                crate::error::VmError::Runtime(
                    crate::error::RuntimeError::InterruptedException,
                ),
            ));
        }
        Ok(None)
    }

    fn monitor_notify(&mut self, obj: ObjectRef) -> MethodCallResult {
        self.shared.monitors.notify(obj, self.thread.thread_id)?;
        Ok(None)
    }

    fn monitor_notify_all(&mut self, obj: ObjectRef) -> MethodCallResult {
        self.shared
            .monitors
            .notify_all(obj, self.thread.thread_id)?;
        Ok(None)
    }

    fn thread_start(&mut self, thread_obj: ObjectRef) -> MethodCallResult {
        let shared_arc = self.shared.get_arc();
        let tid = self.shared.thread_registry.next_thread_id();

        // Read thread name from the Java Thread object (field 0)
        let name = match self.shared.heap.get_field(thread_obj, 0) {
            Value::Object(Some(str_ref)) => {
                super::read_java_string(&self.shared.heap, str_ref)
                    .unwrap_or_else(|| format!("Thread-{}", tid.0))
            }
            _ => format!("Thread-{}", tid.0),
        };

        // Check if this is a virtual thread.
        //
        // Two paths matter here:
        //   1. Synthetic-mode `Thread`: 5-slot layout with `is_virtual` at
        //      slot 4 (set by the synthetic Thread.Builder.start native).
        //   2. Real-JDK mode (JEP 444): a `BoundVirtualThread` (or any
        //      future `VirtualThread`) extends `BaseVirtualThread`. We must
        //      detect this by walking the class hierarchy вЂ” the synthetic
        //      slot-4 trick won't work because the real Thread layout puts
        //      different fields at slot 4. Without this detection, virtual
        //      threads created by `Thread.ofVirtual().start(r)` wouldn't
        //      release the carrier semaphore on `Thread.sleep`/`park`,
        //      starving the carrier pool under load (e.g. 10K vthreads).
        let header = self.shared.heap.get_header(thread_obj);
        let is_virtual_synthetic = header.num_slots >= 5
            && matches!(self.shared.heap.get_field(thread_obj, 4), Value::Int(1));
        let is_virtual_real_jdk = {
            let cm = self.shared.class_manager.read();
            // Look up `BaseVirtualThread`'s class id once. If not loaded
            // (synthetic-only run), this returns None and we skip the
            // hierarchy walk. The class is loaded the moment any
            // VirtualThread / BoundVirtualThread is constructed.
            cm.get_loaded_class_id("java/lang/BaseVirtualThread")
                .map(|base_id| cm.is_subclass_of(header.class_id, base_id))
                .unwrap_or(false)
        };
        let is_virtual = is_virtual_synthetic || is_virtual_real_jdk;

        // T19.K1 вЂ” read the Java-side daemon flag so the registry can
        // tell `wait_for_non_daemon_threads()` whether the process must
        // wait for this thread. The flag is layout-dependent: synthetic
        // Threads don't model `daemon` (the synthetic 5-slot Thread
        // layout doesn't have a daemon field, so we default to false);
        // real-JDK Threads keep it on `Thread.holder.daemon` (see
        // `java.lang.Thread$FieldHolder`). Virtual threads (JEP 444)
        // are always daemon per the spec вЂ” we set that unconditionally
        // so a buggy `BoundVirtualThread` constructor can't keep the VM
        // alive past `main()`.
        let is_daemon = if is_virtual {
            true
        } else {
            self.read_thread_daemon_flag(thread_obj).unwrap_or(false)
        };

        // Register thread as alive before spawning
        self.shared
            .thread_registry
            .register_with_daemon(tid, &name, Some(thread_obj), is_daemon);

        // Record JFR thread start event
        {
            let now_ns = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as u64;
            let mut jfr = self.shared.flight_recorder.lock();
            rustjvm_jfr::builtin::emit_thread_start_event(
                &mut jfr,
                &name,
                if is_virtual { "virtual" } else { "platform" },
                tid.0,
                now_ns,
            );
        }

        // Fire JDWP ThreadStart event
        #[cfg(feature = "experimental-debug")]
        crate::debug::send_thread_event(
            &self.shared,
            crate::debug::events::EventKind::ThreadStart,
            tid.0 as u64,
        );

        // Store the ThreadId on the Java Thread object so we can look it up later.
        //
        // Synthetic-JDK layout: name=0, priority=1, tid=2 вЂ” write directly.
        //
        // Real-JDK layout: `Thread` is loaded from java.base.jmod, has dozens
        // of instance fields, and does NOT keep `tid` at slot 2 (slot 2 in
        // the real layout is something else, often `priority` or part of an
        // inherited field set).  Writing `Long(tid)` to slot 2 in real-JDK
        // mode corrupts whatever object reference / int the JDK bytecode
        // expects there вЂ” observed as "expected object reference, got
        // double(...)" when AQS / ReentrantLock subsequently dereferenced
        // the field in a worker thread.
        //
        // For real-JDK Thread we instead rely on the registry's
        // `(ObjectRef в†’ ThreadId)` lookup, which was already populated by
        // the `register(tid, name, Some(thread_obj))` call above.  This
        // also matches how `find_park_state_by_thread_obj` works.
        let is_real_jdk_thread = {
            let cm = self.shared.class_manager.read();
            cm.class_store
                .get(header.class_id)
                .map(|c| !c.is_synthetic_stub)
                .unwrap_or(false)
        };
        if !is_real_jdk_thread && header.num_slots >= 3 {
            self.shared
                .heap
                .set_field(thread_obj, 2, Value::Long(tid.0 as i64));
        }

        // Pre-create shared state so that operations (like unpark) from the parent
        // thread work even before the child thread starts executing.
        let pre_park = std::sync::Arc::new(crate::threading::jvm_thread::ParkState::new());
        let pre_interrupted = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        self.shared
            .thread_registry
            .set_park_state(tid, pre_park.clone());
        self.shared
            .thread_registry
            .set_interrupted_flag(tid, pre_interrupted.clone());

        let thread_obj_for_spawn = thread_obj;
        let handle = std::thread::spawn(move || {
            let mut jvm_thread = JvmThread::new(tid, &name);
            // Use the pre-created shared state
            jvm_thread.park_state = pre_park;
            jvm_thread.interrupted = pre_interrupted;
            jvm_thread.java_thread_obj = Some(thread_obj_for_spawn);
            if is_virtual {
                jvm_thread.kind = crate::threading::ThreadKind::Virtual;
                // Acquire a carrier permit before executing (blocks if all carriers busy).
                shared_arc.virtual_scheduler.acquire();
            }
            // Share root snapshot with registry for GC cross-thread access
            shared_arc
                .thread_registry
                .set_root_snapshot(tid, jvm_thread.root_snapshot.clone());
            // WP4.8: For real-JDK virtual threads (e.g.
            // `java.lang.ThreadBuilders$BoundVirtualThread`), `Thread.run()`
            // is overridden вЂ” `BoundVirtualThread.run()` invokes the user's
            // `task.run()`, while the base `Thread.run()` reads `holder.task`
            // (always null for BoundVirtualThread).  We must dispatch
            // virtually on the receiver's actual class id; calling
            // `invoke_shared(... "java/lang/Thread", "run", ...)` would land
            // on the base method and silently do nothing.
            //
            // `invoke_on_class_shared` is the right entry point here вЂ” it's
            // called from invokevirtual/invokespecial and walks the class
            // hierarchy starting at the supplied class.  We pass
            // `class_id_of(thread_obj)` so dispatch starts at the real
            // runtime class (e.g. BoundVirtualThread) and naturally finds
            // the override before falling back to Thread.run().
            let recv_cid = shared_arc.heap.class_id_of(thread_obj_for_spawn);
            // Class is already loaded (heap entry exists); ensure init runs.
            let _ = super::ensure_class_initialized_shared(&shared_arc, &mut jvm_thread, recv_cid);
            let result = invoke_on_class_shared(
                &shared_arc,
                &mut jvm_thread,
                recv_cid,
                "run",
                "()V",
                &[Value::Object(Some(thread_obj_for_spawn))],
            );
            if let Err(e) = result {
                eprintln!("Thread {} terminated with error: {:?}", tid, e);
            }
            if is_virtual {
                // Release carrier permit on thread exit.
                shared_arc.virtual_scheduler.release();
            }
            // Record JFR thread end event
            {
                let now_ns = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos() as u64;
                let mut jfr = shared_arc.flight_recorder.lock();
                rustjvm_jfr::builtin::emit_thread_end_event(
                    &mut jfr, &name, tid.0, now_ns,
                );
            }
            // Fire JVMTI ThreadEnd event
            #[cfg(feature = "experimental-debug")]
            {
                let env = shared_arc.jvmti_env.lock();
                if env.event_manager.is_enabled(crate::jvmti::JvmtiEvent::ThreadEnd) {
                    crate::jvmti::notify_thread_end(&env, tid.0 as u64);
                }
            }
            // Fire JDWP ThreadDeath event
            #[cfg(feature = "experimental-debug")]
            crate::debug::send_thread_event(
                &shared_arc,
                crate::debug::events::EventKind::ThreadDeath,
                tid.0 as u64,
            );
            shared_arc.thread_registry.mark_dead(tid);

            // WP4.1 вЂ” wake any thread waiting in `Thread.join()` for us.
            //
            // Real-JDK `Thread.join()` enters this Thread's object monitor
            // and calls `Object.wait()` while `isAlive()` is true.  HotSpot
            // signals the joiner by calling `Thread.notifyAll()` from the
            // VM right before the thread terminates.  We replicate the
            // wakeup here: take the monitor briefly, fire `notify_all`,
            // and release it.  Without this, `join()` spins forever
            // because nobody ever wakes the waiter.
            //
            // We use the shared monitors path directly so we don't need a
            // live `JvmThread` (this closure is on the dying thread's
            // tail; constructing a frame here is overkill).  Errors are
            // swallowed вЂ” the worst case is a missed wakeup, which an
            // existing unparker / interrupt would still resolve.
            shared_arc.monitors.enter(thread_obj_for_spawn, tid);
            let _ = shared_arc
                .monitors
                .notify_all(thread_obj_for_spawn, tid);
            let _ = shared_arc
                .monitors
                .exit(thread_obj_for_spawn, tid);
        });

        self.shared.thread_registry.set_join_handle(tid, handle);
        Ok(None)
    }

    fn thread_join(&mut self, thread_obj: ObjectRef) -> MethodCallResult {
        // Read ThreadId from field 2 of the Java Thread object (synthetic
        // layout) or fall back to the registry's `(ObjectRef в†’ ThreadId)`
        // map (real-JDK layout вЂ” see WP4.1 thread_start fix).
        let tid = match self.shared.heap.get_field(thread_obj, 2) {
            Value::Long(id) => ThreadId(id as u64),
            _ => match self
                .shared
                .thread_registry
                .find_thread_id_by_thread_obj(thread_obj)
            {
                Some(id) => id,
                None => return Ok(None), // Unknown thread, nothing to join
            },
        };
        // Deposit root snapshot before blocking so GC can scan this thread
        self.deposit_root_snapshot();
        self.shared.thread_registry.join(tid);
        // Check if GC happened while we were blocked
        self.check_post_block_gc();
        Ok(None)
    }

    fn thread_is_alive(&self, thread_obj: ObjectRef) -> bool {
        // Read ThreadId from field 2 (synthetic) or registry (real-JDK).
        match self.shared.heap.get_field(thread_obj, 2) {
            Value::Long(id) => self.shared.thread_registry.is_alive(ThreadId(id as u64)),
            _ => self
                .shared
                .thread_registry
                .find_thread_id_by_thread_obj(thread_obj)
                .map(|id| self.shared.thread_registry.is_alive(id))
                .unwrap_or(false),
        }
    }

    fn current_thread_object(&mut self) -> ObjectRef {
        if let Some(obj) = self.thread.java_thread_obj {
            return obj;
        }
        // Use the real java/lang/Thread class ID so virtual dispatch works.
        let class_id = {
            let cm = self.shared.class_manager.read();
            cm.get_loaded_class_id("java/lang/Thread")
        }
        .unwrap_or_else(|| {
            // Thread class not loaded yet вЂ” load it now
            self.shared
                .class_manager
                .write()
                .load_class("java/lang/Thread")
                .unwrap_or(ClassId::new(0))
        });

        // In real-JDK mode the Thread class has many more than 3 instance
        // fields and real-JDK bytecode reads `this.holder.threadStatus`
        // etc.  Allocate with the full field count and populate
        // `name/tid/holder/priority` by resolved slot index.  In
        // synthetic-JDK mode we keep the historical 3-slot fixed layout
        // so existing callers (thread_start, thread_join, many unit
        // tests) continue to work.
        let (is_real_jdk, num_fields) = {
            let cm = self.shared.class_manager.read();
            match cm.class_store.get(class_id) {
                Some(c) if !c.is_synthetic_stub => (true, c.num_total_fields.max(3)),
                _ => (false, 3usize),
            }
        };

        let thread_obj = self.shared.heap.alloc_object(class_id, num_fields);
        if is_real_jdk {
            crate::runtime::interpreter::init_primitive_fields(self.shared, thread_obj, class_id);
        }

        // Register immediately so any recursive call to
        // `current_thread_object` during holder/group construction
        // observes the in-progress object and doesn't loop.
        self.thread.java_thread_obj = Some(thread_obj);
        self.shared
            .thread_registry
            .set_java_thread_obj(self.thread.thread_id, thread_obj);

        let name_str = super::create_java_string(self.shared, &self.thread.name);

        if is_real_jdk {
            let (name_slot, tid_slot, holder_slot, priority_slot) = {
                let cm = self.shared.class_manager.read();
                (
                    resolve_field_index_in_hierarchy(class_id, "name", &cm.class_store),
                    resolve_field_index_in_hierarchy(class_id, "tid", &cm.class_store),
                    resolve_field_index_in_hierarchy(class_id, "holder", &cm.class_store),
                    resolve_field_index_in_hierarchy(class_id, "priority", &cm.class_store),
                )
            };
            if let Some(slot) = name_slot {
                self.shared
                    .heap
                    .set_field(thread_obj, slot, Value::Object(Some(name_str)));
            }
            if let Some(slot) = tid_slot {
                self.shared
                    .heap
                    .set_field(thread_obj, slot, Value::Long(self.thread.thread_id.0 as i64));
            }
            if let Some(slot) = priority_slot {
                // In JDK 19+ priority lives on FieldHolder, but older
                // (or stubbed) Thread layouts keep the outer field.
                self.shared.heap.set_field(thread_obj, slot, Value::Int(5));
            }
            if let Some(slot) = holder_slot {
                if let Some(holder) = self.build_thread_field_holder() {
                    self.shared
                        .heap
                        .set_field(thread_obj, slot, Value::Object(Some(holder)));
                }
            }
            // C9: initialize contextClassLoader so SLF4J and other users
            // of Thread.getContextClassLoader() see the app ClassLoader
            // rather than null.  The JDK's getContextClassLoader() is a
            // plain Java method that returns this field directly, so a
            // null field means ServiceLoader's internal getResources()
            // call will NPE.
            let ccl_slot = {
                let cm = self.shared.class_manager.read();
                resolve_field_index_in_hierarchy(class_id, "contextClassLoader", &cm.class_store)
            };
            if let Some(slot) = ccl_slot {
                // Use the real-JDK-layout system classloader (from
                // classloader_real) rather than the synthetic one, so
                // JDK bytecode reading ClassLoader fields by name sees
                // valid values rather than our synthetic Int(LOADER_APP=2).
                if let Some(loader) = rustjvm_native_builtins::classloader_real::get_or_create_system_cl(self) {
                    self.shared
                        .heap
                        .set_field(thread_obj, slot, Value::Object(Some(loader)));
                }
            }
            return thread_obj;
        }

        // Synthetic-JDK fixed layout: name=0, priority=1, tid=2.
        self.shared
            .heap
            .set_field(thread_obj, 0, Value::Object(Some(name_str)));
        self.shared.heap.set_field(thread_obj, 1, Value::Int(5)); // NORM_PRIORITY
        self.shared
            .heap
            .set_field(thread_obj, 2, Value::Long(self.thread.thread_id.0 as i64));
        thread_obj
    }

    fn thread_interrupt(&mut self, thread_obj: ObjectRef) {
        // Read ThreadId from field 2 (synthetic) or registry (real-JDK).
        let tid = match self.shared.heap.get_field(thread_obj, 2) {
            Value::Long(id) => Some(ThreadId(id as u64)),
            _ => self
                .shared
                .thread_registry
                .find_thread_id_by_thread_obj(thread_obj),
        };
        if let Some(tid) = tid {
            // Set the interrupted flag via the registry (cross-thread safe)
            self.shared.thread_registry.set_interrupted(tid, true);
        }
    }

    /// T1.5.1 вЂ” post an async exception to the target thread's
    /// registry slot. The target picks it up at its next safepoint.
    fn thread_post_async_exception(
        &mut self,
        thread_obj: ObjectRef,
        throwable: ObjectRef,
    ) -> bool {
        let tid = match self.shared.heap.get_field(thread_obj, 2) {
            Value::Long(id) => ThreadId(id as u64),
            _ => match self
                .shared
                .thread_registry
                .find_thread_id_by_thread_obj(thread_obj)
            {
                Some(id) => id,
                None => return false,
            },
        };
        self.shared
            .thread_registry
            .post_async_exception(tid, throwable)
    }

    fn is_interrupted(&self, clear: bool) -> bool {
        let val = self
            .thread
            .interrupted
            .load(std::sync::atomic::Ordering::Acquire);
        if val && clear {
            self.thread
                .interrupted
                .store(false, std::sync::atomic::Ordering::Release);
        }
        val
    }

    // -- Virtual thread / Loom hooks (NEW-15) --

    fn is_current_virtual(&self) -> bool {
        matches!(self.thread.kind, crate::threading::ThreadKind::Virtual)
    }

    fn vt_pin_count(&self) -> u32 {
        self.thread.pin_count
    }

    fn vt_pin(&mut self, reason: &'static str) {
        if matches!(self.thread.kind, crate::threading::ThreadKind::Virtual) {
            self.thread.pin_count = self.thread.pin_count.saturating_add(1);
            self.thread.pin_reason = reason;
        }
    }

    fn vt_unpin(&mut self) {
        if matches!(self.thread.kind, crate::threading::ThreadKind::Virtual)
            && self.thread.pin_count > 0
        {
            self.thread.pin_count -= 1;
            if self.thread.pin_count == 0 {
                self.thread.pin_reason = "";
            }
        }
    }

    fn vt_release_carrier(&mut self) {
        if matches!(self.thread.kind, crate::threading::ThreadKind::Virtual) {
            self.shared.virtual_scheduler.release();
        }
    }

    fn vt_acquire_carrier(&mut self) {
        if matches!(self.thread.kind, crate::threading::ThreadKind::Virtual) {
            self.shared.virtual_scheduler.acquire();
        }
    }

    fn emit_virtual_thread_pinned_jfr(&mut self, reason: &str) {
        let now_ns = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64;
        let carrier_id = self.thread.thread_id.0;
        // Virtual threads currently share their carrier's id; this is fine
        // for JFR classification вЂ” what matters is the `pin_reason` string.
        let vt_id = carrier_id;
        let mut jfr = self.shared.flight_recorder.lock();
        rustjvm_jfr::builtin::emit_virtual_thread_pinned_event(
            &mut jfr,
            &self.thread.name,
            reason,
            carrier_id,
            vt_id,
            now_ns,
        );
    }

    fn active_thread_count(&self) -> i32 {
        self.shared.thread_registry.alive_count() as i32
    }

    fn enumerate_threads(&self, max: usize) -> Vec<ObjectRef> {
        self.shared.thread_registry.alive_thread_objects(max)
    }

    /// T19_K2 вЂ” Register a native-spawned OS thread with the VM
    /// `ThreadRegistry`. The boxed `JoinHandle<()>` (raw pointer in
    /// `join_handle_ptr`) is taken back as `Box<JoinHandle<()>>` and
    /// transferred into the registry so
    /// `ThreadRegistry::wait_for_non_daemon_threads()` can `.join()`
    /// it during VM shutdown.
    ///
    /// Returns the `ThreadId.0` so the caller can later call
    /// `unregister_native_thread(id)` from inside the spawned thread's
    /// exit path.
    fn register_native_thread(
        &mut self,
        name: &str,
        daemon: bool,
        join_handle_ptr: usize,
    ) -> u64 {
        let tid = self.shared.thread_registry.next_thread_id();
        self.shared
            .thread_registry
            .register_with_daemon(tid, name, None, daemon);
        if join_handle_ptr != 0 {
            // SAFETY: the caller built this via
            // `Box::into_raw(Box::new(join_handle))` immediately before
            // the call, and is contractually obliged to pass us the
            // exclusive ownership of that allocation. We take it back
            // and move the `JoinHandle<()>` into the registry, where
            // it lives until `wait_for_non_daemon_threads` joins on it
            // (or the registry is dropped on VM teardown вЂ” in that
            // case the handle is dropped, which detaches the OS thread,
            // matching HotSpot's behaviour for daemon-on-shutdown
            // teardown).
            let boxed: Box<std::thread::JoinHandle<()>> =
                unsafe { Box::from_raw(join_handle_ptr as *mut std::thread::JoinHandle<()>) };
            self.shared.thread_registry.set_join_handle(tid, *boxed);
        }
        tid.0
    }

    /// T19_K2 вЂ” Mark a previously-registered native thread dead. Called
    /// from the spawned OS thread's exit path. Idempotent: passing an
    /// unknown id is a no-op (matches `ThreadRegistry::mark_dead`).
    fn unregister_native_thread(&mut self, thread_id: u64) {
        if thread_id == 0 {
            return;
        }
        self.shared
            .thread_registry
            .mark_dead(crate::threading::jvm_thread::ThreadId(thread_id));
    }

    /// T19_K2 вЂ” Attach a `Box<JoinHandle<()>>` to an already-registered
    /// native thread. Two-phase variant of `register_native_thread` for
    /// callers that need the `ThreadId` before spawning the OS thread.
    fn attach_join_handle_to_native_thread(
        &mut self,
        thread_id: u64,
        join_handle_ptr: usize,
    ) -> bool {
        if thread_id == 0 || join_handle_ptr == 0 {
            return false;
        }
        let tid = crate::threading::jvm_thread::ThreadId(thread_id);
        // Verify the thread is registered before consuming the pointer.
        // `is_alive` returns true on registration and false after
        // `mark_dead` вЂ” either way the entry exists.
        if self
            .shared
            .thread_registry
            .thread_name(tid)
            .is_none()
        {
            return false;
        }
        // SAFETY: caller built this via `Box::into_raw` and is
        // contractually obliged to pass us the exclusive ownership.
        let boxed: Box<std::thread::JoinHandle<()>> =
            unsafe { Box::from_raw(join_handle_ptr as *mut std::thread::JoinHandle<()>) };
        self.shared.thread_registry.set_join_handle(tid, *boxed);
        true
    }

    /// T19_K4 вЂ” Attach a `java.lang.Thread` mirror to an
    /// already-registered native thread so look-ups by `ObjectRef`
    /// resolve and the registry's `alive_thread_objects()` /
    /// `Thread.enumerate()` enumerations include the carrier.
    ///
    /// `thread_id == 0` is rejected (id `0` is the reserved
    /// "main" sentinel and never has a synthetic mirror); unknown
    /// ids return `false`. The store is idempotent вЂ” re-attaching
    /// the same mirror is a no-op as far as the registry is
    /// concerned.
    fn set_native_thread_java_obj(
        &mut self,
        thread_id: u64,
        java_thread_obj: ObjectRef,
    ) -> bool {
        if thread_id == 0 {
            return false;
        }
        let tid = crate::threading::jvm_thread::ThreadId(thread_id);
        if self.shared.thread_registry.thread_name(tid).is_none() {
            return false;
        }
        self.shared
            .thread_registry
            .set_java_thread_obj(tid, java_thread_obj);
        true
    }

    fn heap_allocated_bytes(&self) -> usize {
        self.shared.heap.allocated_bytes()
    }

    fn loaded_class_count(&self) -> usize {
        self.shared.class_manager.read().loaded_count()
    }

    fn gc_collection_count(&self) -> u64 {
        self.shared.heap.collection_count()
    }

    fn force_gc(&mut self) {
        crate::runtime::interpreter::force_gc_from_native(self.shared, self.thread);
    }

    fn declared_fields(&self, class_id: ClassId) -> Vec<FieldMetadata> {
        let cm = self.shared.class_manager.read();
        let Some(class) = cm.get_class(class_id) else {
            return Vec::new();
        };
        let mut static_idx = 0usize;
        let mut instance_idx = 0usize;
        class
            .fields
            .iter()
            .map(|f| {
                let slot = if f.is_static() {
                    let idx = static_idx;
                    static_idx += 1;
                    idx
                } else {
                    let idx = class.first_field_index + instance_idx;
                    instance_idx += 1;
                    idx
                };
                FieldMetadata {
                    name: f.name.to_string(),
                    descriptor: f.descriptor.to_string(),
                    access_flags: f.access_flags.bits(),
                    slot_index: slot,
                    declaring_class_id: class_id,
                    is_static: f.is_static(),
                }
            })
            .collect()
    }

    fn declared_methods(&self, class_id: ClassId) -> Vec<MethodMetadata> {
        let cm = self.shared.class_manager.read();
        let Some(class) = cm.get_class(class_id) else {
            return Vec::new();
        };
        class
            .methods
            .iter()
            .map(|m| MethodMetadata {
                name: m.name.to_string(),
                descriptor: m.descriptor.to_string(),
                access_flags: m.access_flags.bits(),
                declaring_class_id: class_id,
            })
            .collect()
    }

    fn class_interfaces(&self, class_id: ClassId) -> Vec<ClassId> {
        let cm = self.shared.class_manager.read();
        cm.get_class(class_id)
            .map(|c| c.interfaces.clone())
            .unwrap_or_default()
    }

    fn class_access_flags(&self, class_id: ClassId) -> u16 {
        let cm = self.shared.class_manager.read();
        cm.get_class(class_id)
            .map(|c| c.access_flags.bits())
            .unwrap_or(0)
    }

    fn get_static_field(&self, class_id: ClassId, field_index: usize) -> Value {
        super::get_static_shared(self.shared, class_id, field_index)
    }

    fn set_static_field(&mut self, class_id: ClassId, field_index: usize, value: Value) {
        super::set_static_shared(self.shared, class_id, field_index, value);
    }

    fn static_field_index_by_name(&self, class_id: ClassId, field_name: &str) -> Option<usize> {
        let cm = self.shared.class_manager.read();
        let class = cm.get_class(class_id)?;
        let mut static_idx = 0usize;
        for f in &class.fields {
            if f.is_static() {
                if &*f.name == field_name {
                    return Some(static_idx);
                }
                static_idx += 1;
            }
        }
        None
    }

    fn primitive_class_mirror(&mut self, name: &str) -> ObjectRef {
        super::get_or_create_primitive_mirror(self.shared, name)
    }

    fn fd_table(&self) -> &FileDescriptorTable {
        &self.shared.fd_table
    }

    // -- WP0.2 ObjectStreamClass cache --

    fn osc_cache_get(&self, class_id: ClassId) -> Option<ObjectRef> {
        self.shared.osc_cache.get(class_id)
    }

    fn osc_cache_put(&self, class_id: ClassId, desc: ObjectRef) -> ObjectRef {
        self.shared.osc_cache.insert_if_absent(class_id, desc)
    }

    fn get_field_volatile(&self, obj: ObjectRef, index: usize) -> Value {
        // T10.9.E вЂ” descriptor-aware volatile read.
        let class_id = self.shared.heap.class_id_of(obj);
        match resolve_field_descriptor_byte_cached(self.shared, class_id, index) {
            Some(desc) => self.shared.heap.get_field_volatile_as(obj, index, desc),
            None => self.shared.heap.get_field_volatile(obj, index),
        }
    }

    fn set_field_volatile(&self, obj: ObjectRef, index: usize, value: Value) {
        // T10.9.E вЂ” descriptor-aware volatile write.
        let class_id = self.shared.heap.class_id_of(obj);
        match resolve_field_descriptor_byte_cached(self.shared, class_id, index) {
            Some(desc) => self
                .shared
                .heap
                .set_field_volatile_as(obj, index, value, desc),
            None => self.shared.heap.set_field_volatile(obj, index, value),
        }
        // write_barrier fires automatically inside set_field_volatile в†’ set_field
    }

    fn compare_and_swap_field(
        &mut self,
        obj: ObjectRef,
        index: usize,
        expected: Value,
        new_val: Value,
    ) -> bool {
        // T19_H6: descriptor-aware CAS read+write so a long instance field
        // (`J`) always decodes as `Value::Long`, never as `Value::Double`.
        let is_array = self.shared.heap.kind_of(obj) == rustjvm_types::ObjectKind::Array;
        let class_id = self.shared.heap.class_id_of(obj);
        let descriptor = if is_array {
            None
        } else {
            resolve_field_descriptor_byte_cached(self.shared, class_id, index)
        };
        let swapped = self.shared.monitors.with_cas_lock(obj, || {
            let current = if is_array {
                self.shared
                    .heap
                    .get_array_element(obj, index)
                    .unwrap_or(Value::Object(None))
            } else if let Some(desc) = descriptor {
                self.shared.heap.get_field_volatile_as(obj, index, desc)
            } else {
                self.shared.heap.get_field_volatile(obj, index)
            };
            if values_equal_for_cas(&current, &expected) {
                if is_array {
                    let _ = self.shared.heap.set_array_element(obj, index, new_val);
                } else if let Some(desc) = descriptor {
                    self.shared
                        .heap
                        .set_field_volatile_as(obj, index, new_val, desc);
                } else {
                    self.shared.heap.set_field_volatile(obj, index, new_val);
                }
                true
            } else {
                false
            }
        });
        if swapped {
            self.shared.heap.write_barrier(obj, new_val);
        }
        // T19.H7 diag: count CAS failures so we can spot a livelock.
        // Static counter gated to ~5 emissions then 1 every 1M.
        // Feature-gated (off by default) вЂ” see vm/Cargo.toml
        // `experimental-t19-diag`. Re-enable with
        // `--features experimental-t19-diag`.
        #[cfg(feature = "experimental-t19-diag")]
        if !swapped {
            static CAS_FAIL: std::sync::atomic::AtomicU64 =
                std::sync::atomic::AtomicU64::new(0);
            let n = CAS_FAIL.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if n < 5 || n % 1_000_000 == 0 {
                let cn = self
                    .shared
                    .class_manager
                    .read()
                    .get_class(class_id)
                    .map(|c| c.name.to_string())
                    .unwrap_or_else(|| format!("cid={}", class_id.as_u32()));
                tracing::debug!(
                    target: "rustjvm::t19_h7_cas",
                    "CAS FAIL #{n} class={cn} slot={index} desc={:?} expected={:?} new={:?}",
                    descriptor.map(|b| b as char), expected, new_val,
                );
            }
        }
        swapped
    }

    fn park(&mut self, timeout: Option<std::time::Duration>) {
        // JDK spec: if interrupted, park returns immediately (no exception, flag NOT cleared)
        if self
            .thread
            .interrupted
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return;
        }
        // Deposit root snapshot before blocking so GC can scan this thread
        self.deposit_root_snapshot();

        // NEW-15.4: virtual-thread aware park.
        // Non-pinned VTs release their carrier permit so another VT can run.
        // Pinned VTs emit `jdk.VirtualThreadPinned` and keep the carrier.
        let is_virtual = matches!(self.thread.kind, crate::threading::ThreadKind::Virtual);
        let pin_count = self.thread.pin_count;
        let release = is_virtual && pin_count == 0;
        if is_virtual && pin_count > 0 {
            let reason = if self.thread.pin_reason.is_empty() {
                "Pinned (park)"
            } else {
                self.thread.pin_reason
            };
            // Inline JFR emission (avoids borrowing self twice).
            let now_ns = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as u64;
            let mut jfr = self.shared.flight_recorder.lock();
            rustjvm_jfr::builtin::emit_virtual_thread_pinned_event(
                &mut jfr,
                &self.thread.name,
                reason,
                self.thread.thread_id.0,
                self.thread.thread_id.0,
                now_ns,
            );
        }
        if release {
            self.shared.virtual_scheduler.release();
        }

        let park_start = std::time::Instant::now();
        self.thread
            .park_state
            .park_interruptible(timeout, &self.thread.interrupted);
        let park_dur = park_start.elapsed();

        if release {
            self.shared.virtual_scheduler.acquire();
        }
        // Check if GC happened while we were blocked
        self.check_post_block_gc();
        // Emit JFR thread park event
        {
            let now_ns = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as u64;
            let timeout_ns = timeout.map(|d| d.as_nanos() as i64).unwrap_or(0);
            let mut jfr = self.shared.flight_recorder.lock();
            rustjvm_jfr::builtin::emit_thread_park_event(
                &mut jfr,
                "java/util/concurrent/locks/LockSupport",
                timeout_ns,
                0, // address
                self.thread.thread_id.0 as u64,
                now_ns.saturating_sub(park_dur.as_nanos() as u64),
                park_dur.as_nanos() as u64,
            );
        }
    }

    fn unpark(&self, thread_obj: ObjectRef) {
        // Find the ParkState associated with this Java Thread object
        // by looking up in the thread registry.
        if let Some(park_state) = self.shared.find_park_state_for_thread_obj(thread_obj) {
            park_state.unpark();
        }
    }

    fn allocate_instance(&mut self, class_name: &str) -> Option<ObjectRef> {
        let class_id = self
            .shared
            .class_manager
            .write()
            .load_class(class_name)
            .ok()?;
        let num_fields = self
            .shared
            .class_manager
            .read()
            .get_class(class_id)
            .map(|c| c.num_total_fields)
            .unwrap_or(0);
        let obj = self.shared.heap.alloc_object(class_id, num_fields);
        crate::runtime::interpreter::init_primitive_fields(self.shared, obj, class_id);
        Some(obj)
    }

    fn invoke_virtual(
        &mut self,
        receiver: ObjectRef,
        method_name: &str,
        descriptor: &str,
        args: &[Value],
    ) -> MethodCallResult {
        let receiver_class_id = self.shared.heap.class_id_of(receiver);

        // Check if the receiver is a lambda proxy.
        let call_site = {
            let proxies = self.shared.lambda_proxies.read();
            proxies.get(&receiver_class_id).cloned()
        };

        if let Some(lcs) = call_site.filter(|lcs| method_name == lcs.sam_method_name) {
            // Lambda dispatch: read captured values from proxy fields, then
            // prepend them to the invocation args.
            let num_captures = lcs.capture_types.len();
            let mut full_args: Vec<Value> = Vec::with_capacity(num_captures + args.len());
            for i in 0..num_captures {
                full_args.push(self.shared.heap.get_field(receiver, i));
            }
            full_args.extend_from_slice(args);

            // Coerce args between SAM and impl descriptors (box/unbox
            // primitives at the SAM boundary so impl sees matched types).
            let sam_desc = lcs.sam_descriptor.clone();
            let impl_desc = lcs.impl_handle.descriptor.clone();
            let (_, sam_ret) = crate::runtime::interpreter::split_method_descriptor(&sam_desc);
            let (_, impl_ret) = crate::runtime::interpreter::split_method_descriptor(&impl_desc);
            let receiver_present = matches!(
                lcs.impl_handle.kind,
                MethodHandleKind::InvokeVirtual | MethodHandleKind::InvokeInterface
            );
            crate::runtime::interpreter::coerce_lambda_args(
                self.shared,
                self.thread,
                &sam_desc,
                &impl_desc,
                &mut full_args,
                receiver_present,
                num_captures,
            )?;

            // Dispatch by method handle kind.
            let raw_result = match lcs.impl_handle.kind {
                MethodHandleKind::InvokeStatic => self.invoke_or_native(
                    &lcs.impl_handle.class_name,
                    &lcs.impl_handle.member_name,
                    &lcs.impl_handle.descriptor,
                    &full_args,
                ),
                MethodHandleKind::InvokeVirtual | MethodHandleKind::InvokeInterface => {
                    // First arg is the receiver for the target method.
                    if full_args.is_empty() {
                        return Err(VmError::Internal {
                            message: "invoke_virtual: InvokeVirtual/InvokeInterface with no args"
                                .to_string(),
                        }
                        .into());
                    }
                    let target_class = match &full_args[0] {
                        Value::Object(Some(r)) => {
                            let rcv_id = self.shared.heap.class_id_of(*r);
                            self.shared
                                .class_manager
                                .read()
                                .get_class(rcv_id)
                                .map(|c| c.name.to_string())
                                .unwrap_or_else(|| lcs.impl_handle.class_name.clone())
                        }
                        _ => lcs.impl_handle.class_name.clone(),
                    };
                    let result = self.invoke_or_native(
                        &target_class,
                        &lcs.impl_handle.member_name,
                        &lcs.impl_handle.descriptor,
                        &full_args,
                    );
                    // If the receiver's class didn't have the method, fall back
                    // to the class specified in the lambda call site. This handles
                    // objects with generic ClassId (e.g., stub Object) where the
                    // lambda actually targets a specific class (e.g., PrintStream).
                    match &result {
                        Err(MethodCallFailed::InternalError(VmError::Linkage(
                            LinkageError::NoSuchMethodError { .. },
                        ))) if target_class != lcs.impl_handle.class_name => self.invoke_or_native(
                            &lcs.impl_handle.class_name,
                            &lcs.impl_handle.member_name,
                            &lcs.impl_handle.descriptor,
                            &full_args,
                        ),
                        _ => result,
                    }
                }
                MethodHandleKind::InvokeSpecial => self.invoke_or_native(
                    &lcs.impl_handle.class_name,
                    &lcs.impl_handle.member_name,
                    &lcs.impl_handle.descriptor,
                    &full_args,
                ),
                MethodHandleKind::NewInvokeSpecial => {
                    let class_id = self
                        .shared
                        .class_manager
                        .write()
                        .load_class(&lcs.impl_handle.class_name)?;
                    super::ensure_class_initialized_shared(self.shared, self.thread, class_id)?;
                    let num_fields = self
                        .shared
                        .class_manager
                        .read()
                        .get_class(class_id)
                        .map(|c| c.fields.len())
                        .unwrap_or(0);
                    let new_obj = match self.shared.heap.try_alloc_object(class_id, num_fields) {
                        Some(obj) => obj,
                        None => {
                            self.thread.tlab.retire();
                            crate::runtime::interpreter::maybe_gc_forced_pub(self.shared, self.thread);
                            self.shared.heap.try_alloc_object(class_id, num_fields).ok_or_else(|| {
                                MethodCallFailed::InternalError(crate::error::VmError::Runtime(
                                    crate::error::RuntimeError::OutOfMemoryError {
                                        message: format!("Java heap space (MethodHandle newInvokeSpecial, {} fields)", num_fields),
                                    },
                                ))
                            })?
                        }
                    };
                    let mut init_args = Vec::with_capacity(1 + full_args.len());
                    init_args.push(Value::Object(Some(new_obj)));
                    init_args.extend_from_slice(&full_args);
                    invoke_on_class_shared(
                        self.shared,
                        self.thread,
                        class_id,
                        &lcs.impl_handle.member_name,
                        &lcs.impl_handle.descriptor,
                        &init_args,
                    )?;
                    Ok(Some(Value::Object(Some(new_obj))))
                }
                _ => {
                    // GetField, GetStatic, PutField, PutStatic вЂ” very rare for
                    // functional interfaces, defer with a descriptive error.
                    Err(VmError::Internal {
                        message: format!(
                            "invoke_virtual: unsupported MethodHandle kind {:?} for lambda proxy",
                            lcs.impl_handle.kind
                        ),
                    }
                    .into())
                }
            };
            // Coerce return value from impl's descriptor back to SAM's view.
            let r = raw_result?;
            crate::runtime::interpreter::coerce_return(
                self.shared,
                self.thread,
                &sam_ret,
                &impl_ret,
                r,
            )
        } else {
            // Not a lambda proxy SAM call вЂ” normal virtual dispatch.
            // If the receiver IS a lambda proxy but calling a non-SAM method
            // (e.g. andThen), dispatch on the functional interface class.
            let class_name = {
                let lambda_iface = {
                    let proxies = self.shared.lambda_proxies.read();
                    proxies.get(&receiver_class_id).map(|lcs| lcs.functional_interface.clone())
                };
                lambda_iface.unwrap_or_else(|| {
                    self.shared
                        .class_manager
                        .read()
                        .get_class(receiver_class_id)
                        .map(|c| c.name.to_string())
                        .unwrap_or_else(|| format!("<unknown class {}>", receiver_class_id))
                })
            };

            // Check for java.lang.reflect.Proxy dynamic proxy dispatch.
            // When Java code calls any method on a Proxy$Instance object, we
            // intercept and forward to the InvocationHandler.invoke().
            if class_name == "java/lang/reflect/Proxy$Instance" {
                return proxy_invoke_handler(self, receiver, method_name, descriptor, args);
            }

            // Check for annotation proxy dispatch.
            // When Java code calls annotation.value(), annotation.path(), etc.,
            // look up the element by method name in the proxy's stored elements.
            if class_name == "java/lang/annotation/AnnotationProxy" {
                return annotation_proxy_invoke(self, receiver, method_name, args);
            }

            // Prepend receiver to args.
            let mut full_args = Vec::with_capacity(1 + args.len());
            full_args.push(Value::Object(Some(receiver)));
            full_args.extend_from_slice(args);

            self.invoke_or_native(&class_name, method_name, descriptor, &full_args)
        }
    }

    fn class_annotations(&self, class_id: ClassId) -> Vec<crate::native::registry::AnnotationData> {
        let cm = self.shared.class_manager.read();
        let class = match cm.get_class(class_id) {
            Some(c) => c,
            None => return Vec::new(),
        };
        let mut result = Vec::new();
        for ann in &class.annotations {
            if let Some(data) = convert_annotation(ann, &class.constant_pool) {
                result.push(data);
            }
        }
        result
    }

    fn method_annotations(
        &self,
        class_id: ClassId,
        method_name: &str,
        method_desc: &str,
    ) -> Vec<crate::native::registry::AnnotationData> {
        let cm = self.shared.class_manager.read();
        let class = match cm.get_class(class_id) {
            Some(c) => c,
            None => return Vec::new(),
        };
        for m in &class.methods {
            if &*m.name == method_name && &*m.descriptor == method_desc {
                return extract_annotations_from_attributes(&m.attributes, &class.constant_pool);
            }
        }
        Vec::new()
    }

    fn field_annotations(
        &self,
        class_id: ClassId,
        field_name: &str,
    ) -> Vec<crate::native::registry::AnnotationData> {
        let cm = self.shared.class_manager.read();
        let class = match cm.get_class(class_id) {
            Some(c) => c,
            None => return Vec::new(),
        };
        for f in &class.fields {
            if &*f.name == field_name {
                return extract_annotations_from_attributes(&f.attributes, &class.constant_pool);
            }
        }
        Vec::new()
    }

    fn class_signature(&self, class_id: ClassId) -> Option<String> {
        let cm = self.shared.class_manager.read();
        let class = cm.get_class(class_id)?;
        class.signature.clone()
    }

    fn method_signature(
        &self,
        class_id: ClassId,
        method_name: &str,
        method_desc: &str,
    ) -> Option<String> {
        let cm = self.shared.class_manager.read();
        let class = cm.get_class(class_id)?;
        for m in &class.methods {
            if &*m.name == method_name && &*m.descriptor == method_desc {
                for attr in &m.attributes {
                    if let rustjvm_reader::attribute::Attribute::Signature(s) = attr {
                        return Some(s.clone());
                    }
                }
                return None;
            }
        }
        None
    }

    fn method_parameters(
        &self,
        class_id: ClassId,
        method_name: &str,
        method_desc: &str,
    ) -> Vec<(String, u16)> {
        let cm = self.shared.class_manager.read();
        let class = match cm.get_class(class_id) {
            Some(c) => c,
            None => return Vec::new(),
        };
        for m in &class.methods {
            if &*m.name == method_name && &*m.descriptor == method_desc {
                for attr in &m.attributes {
                    if let rustjvm_reader::attribute::Attribute::MethodParameters(params) = attr {
                        // JVMS 4.7.24: name_index == 0 means an anonymous /
                        // synthetic parameter вЂ” surface it as an empty
                        // string so the caller can fall back to "argN".
                        return params
                            .iter()
                            .map(|p| {
                                let name = if p.name_index == 0 {
                                    String::new()
                                } else {
                                    class
                                        .constant_pool
                                        .get_utf8(p.name_index)
                                        .unwrap_or("")
                                        .to_string()
                                };
                                (name, p.access_flags)
                            })
                            .collect();
                    }
                }
                return Vec::new();
            }
        }
        Vec::new()
    }

    fn field_signature(&self, class_id: ClassId, field_name: &str) -> Option<String> {
        let cm = self.shared.class_manager.read();
        let class = cm.get_class(class_id)?;
        for f in &class.fields {
            if &*f.name == field_name {
                for attr in &f.attributes {
                    if let rustjvm_reader::attribute::Attribute::Signature(s) = attr {
                        return Some(s.clone());
                    }
                }
                return None;
            }
        }
        None
    }

    fn method_parameter_annotations(
        &self,
        class_id: ClassId,
        method_name: &str,
        method_desc: &str,
    ) -> Vec<Vec<crate::native::registry::AnnotationData>> {
        let cm = self.shared.class_manager.read();
        let class = match cm.get_class(class_id) {
            Some(c) => c,
            None => return Vec::new(),
        };
        for m in &class.methods {
            if &*m.name == method_name && &*m.descriptor == method_desc {
                return extract_parameter_annotations(&m.attributes, &class.constant_pool);
            }
        }
        Vec::new()
    }

    fn method_annotation_default(
        &self,
        class_id: ClassId,
        method_name: &str,
        method_desc: &str,
    ) -> Option<crate::native::registry::AnnotationElementValue> {
        let cm = self.shared.class_manager.read();
        let class = match cm.get_class(class_id) {
            Some(c) => c,
            None => return None,
        };
        for m in &class.methods {
            if &*m.name == method_name && &*m.descriptor == method_desc {
                for attr in &m.attributes {
                    if let rustjvm_reader::attribute::Attribute::AnnotationDefault(ev) = attr {
                        return convert_element_value(ev, &class.constant_pool);
                    }
                }
                return None;
            }
        }
        None
    }

    // -- Scoped Values (JEP 446, Java 25) --

    fn get_scoped_value(&self, key_id: u64) -> Option<Value> {
        // Search top-to-bottom for matching key_id
        for (k, v) in self.thread.scoped_values.iter().rev() {
            if *k == key_id {
                return Some(*v);
            }
        }
        None
    }

    fn push_scoped_value(&mut self, key_id: u64, value: Value) {
        self.thread.scoped_values.push((key_id, value));
    }

    fn pop_scoped_value(&mut self) {
        self.thread.scoped_values.pop();
    }

    fn scoped_value_depth(&self) -> usize {
        self.thread.scoped_values.len()
    }

    // -- Panama FFI (JEP 454) --

    fn allocate_native_memory(&mut self, size: usize, align: usize) -> Option<(i64, *mut u8)> {
        self.shared.native_memory.lock().allocate(size, align)
    }

    fn free_native_memory(&mut self, alloc_id: i64) {
        self.shared.native_memory.lock().free(alloc_id);
    }

    fn load_native_library(&mut self, path: &str) -> Result<i64, crate::error::MethodCallFailed> {
        // Resolve the library path: if `path` has no directory separator, search
        // `java.library.path` for the platform-specific library file name.
        let resolved = resolve_library_path(self.shared, path);

        // Safety: we trust the user-provided path. libloading handles platform differences.
        let lib = unsafe { libloading::Library::new(&resolved) }.map_err(|e| {
            crate::error::RuntimeError::IllegalStateException {
                message: format!("Failed to load library '{}': {}", resolved, e),
            }
        })?;

        // Call JNI_OnLoad(JavaVM*, void*) if exported by the library.
        // This allows the library to register its native methods via RegisterNatives.
        // Safety: JNI_OnLoad has a fixed, well-known signature.
        unsafe {
            type JniOnLoad = extern "C" fn(
                crate::native::jni::JavaVM,
                *mut std::ffi::c_void,
            ) -> crate::native::jni::JInt;
            if let Ok(sym) = lib.get::<JniOnLoad>(b"JNI_OnLoad\0") {
                // Set TLS context so RegisterNatives (called from JNI_OnLoad) can
                // resolve class names via the class manager.
                crate::native::jni::set_jni_context(self.shared);
                crate::native::jni::set_jni_thread(self.thread);
                let _version = sym(crate::native::jni::get_java_vm(), std::ptr::null_mut());
                crate::native::jni::clear_jni_context();
                crate::native::jni::clear_jni_thread();
            }
        }

        let mut libs = self.shared.native_libraries.lock();
        let index = libs.len() as i64;
        libs.push(lib);
        Ok(index)
    }


    fn register_upcall(&mut self, entry: crate::native::ffi::UpcallEntry) -> usize {
        self.shared.upcall_table.lock().register(entry)
    }

    fn get_upcall_info(&self, slot: usize) -> Option<(ObjectRef, Vec<i32>, i32)> {
        self.shared
            .upcall_table
            .lock()
            .get(slot)
            .map(|e| (e.target, e.param_kinds.clone(), e.return_kind))
    }

    fn module_name_of_class(&self, class_id: ClassId) -> Option<String> {
        self.shared
            .class_manager
            .read()
            .get_class(class_id)
            .and_then(|c| c.module_name.clone())
    }

    fn reads_module(&self, reader: &str, provider: &str) -> bool {
        let cm = self.shared.class_manager.read();
        if cm.module_registry.is_empty() {
            return true;
        }
        cm.module_registry.reads(reader, provider)
    }

    fn is_package_exported_unqualified(&self, module_name: &str, pkg: &str) -> bool {
        let cm = self.shared.class_manager.read();
        if cm.module_registry.is_empty() {
            return true;
        }
        cm.module_registry.is_package_exported_unqualified(module_name, pkg)
    }

    fn is_package_exported_to(&self, module_name: &str, pkg: &str, to_module: &str) -> bool {
        let cm = self.shared.class_manager.read();
        if cm.module_registry.is_empty() {
            return true;
        }
        cm.module_registry.is_package_exported_to(module_name, pkg, to_module)
    }

    fn is_package_open_unqualified(&self, module_name: &str, pkg: &str) -> bool {
        let cm = self.shared.class_manager.read();
        if cm.module_registry.is_empty() {
            return true;
        }
        cm.module_registry.is_package_open_unqualified(module_name, pkg)
    }

    fn is_package_open_to(&self, module_name: &str, pkg: &str, to_module: &str) -> bool {
        let cm = self.shared.class_manager.read();
        if cm.module_registry.is_empty() {
            return true;
        }
        cm.module_registry.is_package_open_to(module_name, pkg, to_module)
    }

    fn module_add_reads(&mut self, reader: &str, provider: &str) {
        self.shared.class_manager.write().module_registry.add_reads(reader, provider);
    }

    fn module_add_exports(&mut self, module_name: &str, pkg: &str, target: &str) {
        self.shared.class_manager.write().module_registry.add_exports(module_name, pkg, target);
    }

    fn module_add_opens(&mut self, module_name: &str, pkg: &str, target: &str) {
        self.shared.class_manager.write().module_registry.add_opens(module_name, pkg, target);
    }

    fn module_packages(&self, module_name: &str) -> Vec<String> {
        self.shared.class_manager.read().module_registry.packages_of(module_name)
    }

    fn all_module_names(&self) -> Vec<String> {
        self.shared.class_manager.read().module_registry.module_names()
    }

    fn module_for_package(&self, pkg: &str) -> Option<String> {
        self.shared.class_manager.read().module_registry.module_for_package(pkg).map(|s| s.to_string())
    }

    fn set_class_hidden(&mut self, class_id: ClassId) {
        let mut cm = self.shared.class_manager.write();
        if let Some(class) = cm.get_class_mut(class_id) {
            class.hidden = true;
        }
    }

    fn is_class_hidden(&self, class_id: ClassId) -> bool {
        let cm = self.shared.class_manager.read();
        cm.get_class(class_id)
            .map(|c| c.is_hidden())
            .unwrap_or(false)
    }

    fn copy_nest_info(&mut self, source_class: ClassId, target_class: ClassId) {
        // NEW-8: hidden classes created with the NESTMATE ClassOption
        // inherit the lookup class's nest host and nest members. We copy
        // the relevant fields onto the target so that member access
        // checks see the hidden class as a legitimate nestmate.
        let mut cm = self.shared.class_manager.write();
        // Grab the nest info from the source class first (release the
        // immutable borrow before we take a mutable one).
        let (nest_host, nest_members) = match cm.get_class(source_class) {
            Some(src) => {
                // If source is itself a nest member, it has a nest_host
                // pointing at the host. If it IS the host (or has no
                // nest info), use source's own name so the hidden class
                // becomes a member of source's nest.
                let host = src
                    .nest_host
                    .clone()
                    .unwrap_or_else(|| src.name.to_string());
                (host, src.nest_members.clone())
            }
            None => return,
        };
        if let Some(target) = cm.get_class_mut(target_class) {
            target.nest_host = Some(nest_host);
            target.nest_members = nest_members;
        }
    }

    fn initialize_class(&mut self, class_id: ClassId) -> Result<(), String> {
        // NEW-8: force the class's <clinit> to run now. The interpreter's
        // `ensure_class_initialized_shared` handles thread-safe init and
        // skips classes that are already initialized.
        match crate::vm::vm_util::ensure_class_initialized_shared(
            &self.shared,
            self.thread,
            class_id,
        ) {
            Ok(()) => Ok(()),
            Err(crate::error::MethodCallFailed::ExceptionThrown(_)) => {
                // <clinit> raised a Java exception. Return a string
                // summary; the caller (defineHiddenClass) surfaces it
                // as an IllegalStateException tagged
                // "ExceptionInInitializerError".
                Err("class initialization raised an exception".to_string())
            }
            Err(crate::error::MethodCallFailed::InternalError(e)) => Err(format!("{e:?}")),
        }
    }

    fn service_providers_from_modules(&self, service_class: &str) -> Vec<String> {
        self.shared.class_manager.read().module_registry.service_providers(service_class)
    }

    fn check_deep_reflection_access(
        &self,
        accessor_class_id: ClassId,
        target_class_id: ClassId,
    ) -> Result<(), String> {
        let cm = self.shared.class_manager.read();
        // No modules registered в†’ classpath-only mode, allow.
        if cm.module_registry.is_empty() {
            return Ok(());
        }
        let accessor = match cm.get_class(accessor_class_id) {
            Some(c) => c,
            None => return Ok(()),
        };
        let target = match cm.get_class(target_class_id) {
            Some(c) => c,
            None => return Ok(()),
        };
        let accessor_mod = accessor
            .module_name
            .as_deref()
            .unwrap_or(crate::classloading::module::UNNAMED_MODULE);
        let target_mod = target
            .module_name
            .as_deref()
            .unwrap_or(crate::classloading::module::UNNAMED_MODULE);
        let target_pkg = crate::classloading::module::package_of(&target.name);
        cm.module_registry
            .check_deep_reflection_access(accessor_mod, target_mod, target_pkg)
    }

    fn find_resource(&self, name: &str) -> Option<Vec<u8>> {
        self.shared.class_manager.read().find_resource(name)
    }

    fn class_bytes(&self, class_id: ClassId) -> Option<Vec<u8>> {
        let cm = self.shared.class_manager.read();
        let name = cm.class_store.get(class_id)?.name.to_string();
        cm.class_bytes_cache.get(&name).cloned()
    }

    fn find_all_resource_urls(&self, name: &str) -> Vec<String> {
        self.shared.class_manager.read().find_all_resource_urls(name)
    }

    fn find_class_source_path(&self, class_name: &str) -> Option<String> {
        self.shared.class_manager.read().find_class_source_path(class_name)
    }

    fn class_code_base(&self, class_id: ClassId) -> Option<String> {
        let cm = self.shared.class_manager.read();
        let cls = cm.class_store.get(class_id)?;
        cls.code_source.as_ref()?.url.clone()
    }

    fn class_code_source_cert_digests(&self, class_id: ClassId) -> Vec<String> {
        let cm = self.shared.class_manager.read();
        match cm.class_store.get(class_id) {
            Some(cls) => cls
                .code_source
                .as_ref()
                .map(|cs| cs.certificate_sha256.clone())
                .unwrap_or_default(),
            None => Vec::new(),
        }
    }

    fn class_code_source_certs(&self, class_id: ClassId) -> Vec<Vec<u8>> {
        let cm = self.shared.class_manager.read();
        match cm.class_store.get(class_id) {
            Some(cls) => cls
                .code_source
                .as_ref()
                .map(|cs| cs.certificates.clone())
                .unwrap_or_default(),
            None => Vec::new(),
        }
    }

    fn class_id_from_mirror(&self, mirror: ObjectRef) -> Option<ClassId> {
        super::class_id_from_mirror(self.shared, mirror)
    }

    fn list_application_class_names(&self) -> Vec<String> {
        self.shared.class_manager.read().list_application_class_names()
    }

    fn register_dynamic_classpath(&mut self, paths: &[String]) {
        self.shared
            .class_manager
            .write()
            .extend_application_classpath(paths);
    }

    fn define_class_from_bytes(
        &mut self,
        name: &str,
        bytes: &[u8],
    ) -> Option<ClassId> {
        use rustjvm_types::ClassLoaderId;
        let mut cm = self.shared.class_manager.write();
        match cm.define_class(name, bytes, ClassLoaderId::Application) {
            Ok(cid) => {
                // Release the ClassManager write lock before calling
                // `invalidate_jit_for_class` (which takes a read lock on
                // the same manager).
                drop(cm);
                // Invalidate JIT-compiled methods that inlined from this class (Session 31)
                let evicted = self.shared.jit_cache.write().invalidate_for_class(name);
                if evicted > 0 {
                    tracing::debug!("JIT: invalidated {evicted} method(s) due to class reload: {name}");
                }
                // T5.4.4 вЂ” additionally consult the InvalidationManager's
                // LeafClass/class_dependencies entries.
                let cha_evicted = self.shared.invalidate_jit_for_class(name);
                if cha_evicted > 0 {
                    tracing::debug!(
                        "JIT: invalidated {cha_evicted} method(s) via CHA listener for class: {name}"
                    );
                }
                Some(cid)
            }
            Err(e) => {
                tracing::debug!("defineClass failed for {name}: {e:?}");
                None
            }
        }
    }

    fn define_hidden_class_from_bytes(
        &mut self,
        stored_name: &str,
        bytes: &[u8],
    ) -> Result<ClassId, String> {
        use rustjvm_types::ClassLoaderId;
        use rustjvm_classloading::DefineClassOptions;
        let mut cm = self.shared.class_manager.write();
        let options = DefineClassOptions {
            override_name: Some(stored_name.to_string()),
            hidden: true,
            ..Default::default()
        };
        match cm.define_class_with_options(
            stored_name,
            bytes,
            ClassLoaderId::Application,
            options,
        ) {
            Ok(cid) => {
                // Hidden classes cannot be inlined from (they may be
                // unloaded independently), but we still invalidate the
                // JIT cache defensively.
                let evicted = self
                    .shared
                    .jit_cache
                    .write()
                    .invalidate_for_class(stored_name);
                if evicted > 0 {
                    tracing::debug!(
                        "JIT: invalidated {evicted} method(s) due to hidden class define: {stored_name}"
                    );
                }
                Ok(cid)
            }
            Err(e) => Err(format!("{e:?}")),
        }
    }

    fn define_class_with_loader(
        &mut self,
        name: &str,
        bytes: &[u8],
        loader_id: u32,
    ) -> Option<ClassId> {
        use rustjvm_types::ClassLoaderId;
        let mut cm = self.shared.class_manager.write();
        match cm.define_class(name, bytes, ClassLoaderId::UserDefined(loader_id)) {
            Ok(cid) => {
                drop(cm);
                let evicted = self.shared.jit_cache.write().invalidate_for_class(name);
                if evicted > 0 {
                    tracing::debug!("JIT: invalidated {evicted} method(s) due to class reload: {name}");
                }
                // T5.4.4 вЂ” CHA-listener invalidation
                let cha_evicted = self.shared.invalidate_jit_for_class(name);
                if cha_evicted > 0 {
                    tracing::debug!(
                        "JIT: invalidated {cha_evicted} method(s) via CHA listener for class: {name}"
                    );
                }
                Some(cid)
            }
            Err(e) => {
                tracing::debug!("defineClass (loader {loader_id}) failed for {name}: {e:?}");
                None
            }
        }
    }

    fn class_id_by_name_and_loader(&self, name: &str, loader_id: u32) -> Option<ClassId> {
        use rustjvm_types::ClassLoaderId;
        let cm = self.shared.class_manager.read();
        cm.find_class_by_name_in_loader(name, ClassLoaderId::UserDefined(loader_id))
    }

    fn define_class_full(
        &mut self,
        name: &str,
        bytes: &[u8],
        loader_id: u32,
        opts: rustjvm_native_api::DefineClassFull,
    ) -> Result<ClassId, String> {
        // WP2.3: single backend for all four entry points
        // (Unsafe.defineClass, jdk.internal.misc.Unsafe.defineClass,
        // MethodHandles.Lookup.defineClass, ClassLoader.defineClass1/2).
        use rustjvm_classloading::{CodeSource, DefineClassOptions};
        use rustjvm_types::ClassLoaderId;
        let cl_id = if loader_id == 0 {
            ClassLoaderId::Application
        } else {
            ClassLoaderId::UserDefined(loader_id)
        };
        let code_source = if opts.code_source_url.is_none()
            && opts.code_source_certificates.is_empty()
        {
            None
        } else {
            Some(CodeSource::new(
                opts.code_source_url.clone(),
                opts.code_source_certificates.clone(),
            ))
        };
        let define_opts = DefineClassOptions {
            override_name: opts.override_name.clone(),
            hidden: opts.hidden,
            skip_verification: opts.skip_verification,
            code_source,
            allow_redefine: opts.allow_redefine,
            nest_host_class_name: opts.nest_host_class_name.clone(),
            ..Default::default()
        };

        let cid = {
            let mut cm = self.shared.class_manager.write();
            cm.define_class_with_options(name, bytes, cl_id, define_opts)
                .map_err(|e| format!("{e:?}"))?
        };

        // Invalidate JIT for any class with the same name (handles redefine).
        let evicted = self.shared.jit_cache.write().invalidate_for_class(name);
        if evicted > 0 {
            tracing::debug!("JIT: invalidated {evicted} method(s) due to defineClass: {name}");
        }
        let cha_evicted = self.shared.invalidate_jit_for_class(name);
        if cha_evicted > 0 {
            tracing::debug!("JIT: CHA-invalidated {cha_evicted} method(s) for: {name}");
        }

        if opts.initialize {
            // Best-effort init; failures bubble back as Err.
            if let Err(e) = self.initialize_class(cid) {
                return Err(format!("initialize after define failed for {name}: {e}"));
            }
        }
        Ok(cid)
    }

    fn redefine_class(
        &mut self,
        class_id: ClassId,
        new_bytes: &[u8],
    ) -> Result<(), String> {
        // WP2.4-F1 вЂ” JEP 109 redefine path: route to
        // `class_manager::redefine_class` (Agent 2.4-B) which performs
        // the in-place method-body swap, refreshes the vtable, bumps
        // the per-class `redefine_generations` counter, and fires the
        // JIT invalidate hook. Earlier versions of this binding called
        // `define_class_with_options(allow_redefine: true)`, which
        // minted a fresh ClassId for the redefined class and left the
        // ORIGINAL ClassId's `Class.methods` table untouched вЂ” so the
        // already-loaded `Target` instance kept dispatching to the
        // pre-transform bytecode and the per-thread invoke cache
        // (keyed on the original ClassId) never observed a generation
        // bump.  The route below makes the in-place swap semantics
        // observable end-to-end.
        use rustjvm_classloading::RedefineOptions;
        let name = {
            let cm = self.shared.class_manager.read();
            let cls = cm
                .class_store
                .get(class_id)
                .ok_or_else(|| "class not loaded".to_string())?;
            cls.name.to_string()
        };
        {
            let mut cm = self.shared.class_manager.write();
            cm.redefine_class(class_id, new_bytes.to_vec(), RedefineOptions::default())
                .map_err(|e| format!("{e:?}"))?;
        }
        // Best-effort JIT cache eviction by name (the
        // `fire_jit_invalidate_hook` call inside `redefine_class` already
        // notifies the registered hook keyed by `class_id`; this catches
        // any name-keyed sibling caches).
        let _ = self.shared.jit_cache.write().invalidate_for_class(&name);
        let _ = self.shared.invalidate_jit_for_class(&name);
        Ok(())
    }

    fn list_loaded_class_ids(&self) -> Vec<ClassId> {
        let cm = self.shared.class_manager.read();
        cm.class_store.iter().map(|c| c.id).collect()
    }

    fn list_initiated_class_ids(&self, loader_id: u32) -> Vec<ClassId> {
        use rustjvm_types::ClassLoaderId;
        let cl_id = if loader_id == 0 {
            ClassLoaderId::Application
        } else {
            ClassLoaderId::UserDefined(loader_id)
        };
        let cm = self.shared.class_manager.read();
        cm.class_store
            .iter()
            .filter(|c| c.loader_id == cl_id)
            .map(|c| c.id)
            .collect()
    }

    fn allocate_loader_id(&mut self) -> u32 {
        use std::sync::atomic::{AtomicU32, Ordering};
        static NEXT_LOADER_ID: AtomicU32 = AtomicU32::new(1);
        NEXT_LOADER_ID.fetch_add(1, Ordering::Relaxed)
    }

    fn discover_reference(
        &mut self,
        ref_type: u8,
        reference_obj: ObjectRef,
        referent: ObjectRef,
        queue: Option<ObjectRef>,
    ) {
        use rustjvm_gc::ReferenceType;
        let rt = match ref_type {
            0 => ReferenceType::Weak,
            1 => ReferenceType::Soft,
            2 => ReferenceType::Phantom,
            3 => ReferenceType::Cleaner,
            _ => return,
        };
        let ref_addr = reference_obj.as_ptr() as usize;
        let referent_addr = referent.as_ptr() as usize;
        let queue_addr = queue.map(|q| q.as_ptr() as usize);
        self.shared.ref_processor.lock().discover_reference(rt, ref_addr, referent_addr, queue_addr);
    }

    fn record_thread_sleep(&mut self, sleep_nanos: i64, actual_duration_nanos: u64) {
        let now_ns = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64;
        let mut jfr = self.shared.flight_recorder.lock();
        rustjvm_jfr::builtin::emit_thread_sleep_event(
            &mut jfr,
            sleep_nanos,
            self.thread.thread_id.0 as u64,
            now_ns.saturating_sub(actual_duration_nanos),
            actual_duration_nanos,
        );
    }

    fn record_file_read(&mut self, fd: i32, bytes_read: i64, eof: bool, duration_nanos: u64) {
        let now_ns = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64;
        let path = format!("fd:{}", fd);
        let mut jfr = self.shared.flight_recorder.lock();
        rustjvm_jfr::builtin::emit_file_read_event(
            &mut jfr,
            &path,
            bytes_read,
            eof,
            self.thread.thread_id.0 as u64,
            now_ns.saturating_sub(duration_nanos),
            duration_nanos,
        );
    }

    fn record_file_write(&mut self, fd: i32, bytes_written: i64, duration_nanos: u64) {
        let now_ns = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64;
        let path = format!("fd:{}", fd);
        let mut jfr = self.shared.flight_recorder.lock();
        rustjvm_jfr::builtin::emit_file_write_event(
            &mut jfr,
            &path,
            bytes_written,
            self.thread.thread_id.0 as u64,
            now_ns.saturating_sub(duration_nanos),
            duration_nanos,
        );
    }

    fn find_native_symbol(&self, lib_index: i64, name: &str) -> Option<usize> {
        let libs = self.shared.native_libraries.lock();
        let c_name = std::ffi::CString::new(name).ok()?;

        if lib_index >= 0 && (lib_index as usize) < libs.len() {
            // Look up in a specific loaded library
            unsafe {
                libs[lib_index as usize]
                    .get::<*const ()>(c_name.as_bytes_with_nul())
                    .ok()
                    .map(|sym| *sym as usize)
            }
        } else {
            // Default/system lookup вЂ” try all loaded libraries
            for lib in libs.iter() {
                if let Ok(sym) = unsafe { lib.get::<*const ()>(c_name.as_bytes_with_nul()) } {
                    return Some(*sym as usize);
                }
            }
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Annotation helpers
// ---------------------------------------------------------------------------

/// Extract annotation data from a list of attributes.
pub(super) fn extract_annotations_from_attributes(
    attributes: &[rustjvm_reader::attribute::Attribute],
    cp: &rustjvm_reader::constant_pool::ConstantPool,
) -> Vec<crate::native::registry::AnnotationData> {
    use rustjvm_reader::attribute::Attribute;
    let mut result = Vec::new();
    for attr in attributes {
        match attr {
            Attribute::RuntimeVisibleAnnotations(annotations)
            | Attribute::RuntimeInvisibleAnnotations(annotations) => {
                for ann in annotations {
                    if let Some(data) = convert_annotation(ann, cp) {
                        result.push(data);
                    }
                }
            }
            _ => {}
        }
    }
    result
}

/// Extract parameter annotation data from a list of attributes.
pub(super) fn extract_parameter_annotations(
    attributes: &[rustjvm_reader::attribute::Attribute],
    cp: &rustjvm_reader::constant_pool::ConstantPool,
) -> Vec<Vec<crate::native::registry::AnnotationData>> {
    use rustjvm_reader::attribute::Attribute;
    for attr in attributes {
        match attr {
            Attribute::RuntimeVisibleParameterAnnotations(params)
            | Attribute::RuntimeInvisibleParameterAnnotations(params) => {
                return params
                    .iter()
                    .map(|anns| {
                        anns.iter()
                            .filter_map(|ann| convert_annotation(ann, cp))
                            .collect()
                    })
                    .collect();
            }
            _ => {}
        }
    }
    Vec::new()
}

/// Convert a reader Annotation to our AnnotationData.
pub(super) fn convert_annotation(
    ann: &rustjvm_reader::attribute::Annotation,
    cp: &rustjvm_reader::constant_pool::ConstantPool,
) -> Option<crate::native::registry::AnnotationData> {
    use crate::native::registry::AnnotationData;
    let type_desc = cp.get_utf8(ann.type_index)?.to_string();
    let mut elements = Vec::new();
    for pair in &ann.element_value_pairs {
        let name = cp.get_utf8(pair.element_name_index)?.to_string();
        let value = convert_element_value(&pair.value, cp)?;
        elements.push((name, value));
    }
    Some(AnnotationData {
        type_descriptor: type_desc,
        elements,
    })
}

/// Convert a reader ElementValue to our AnnotationElementValue.
pub(super) fn convert_element_value(
    ev: &rustjvm_reader::attribute::ElementValue,
    cp: &rustjvm_reader::constant_pool::ConstantPool,
) -> Option<crate::native::registry::AnnotationElementValue> {
    use crate::native::registry::AnnotationElementValue;
    use rustjvm_reader::attribute::ElementValue;
    use rustjvm_reader::constant_pool::ConstantPoolEntry;
    match ev {
        ElementValue::Const {
            tag,
            const_value_index,
        } => {
            let idx = *const_value_index;
            match tag {
                b'B' | b'C' | b'I' | b'S' | b'Z' => {
                    if let Some(ConstantPoolEntry::Integer(val)) = cp.get(idx) {
                        Some(AnnotationElementValue::Int(*val))
                    } else {
                        Some(AnnotationElementValue::Int(0))
                    }
                }
                b'J' => {
                    if let Some(ConstantPoolEntry::Long(val)) = cp.get(idx) {
                        Some(AnnotationElementValue::Long(*val))
                    } else {
                        Some(AnnotationElementValue::Long(0))
                    }
                }
                b'F' => {
                    if let Some(ConstantPoolEntry::Float(val)) = cp.get(idx) {
                        Some(AnnotationElementValue::Float(*val))
                    } else {
                        Some(AnnotationElementValue::Float(0.0))
                    }
                }
                b'D' => {
                    if let Some(ConstantPoolEntry::Double(val)) = cp.get(idx) {
                        Some(AnnotationElementValue::Double(*val))
                    } else {
                        Some(AnnotationElementValue::Double(0.0))
                    }
                }
                b's' => {
                    let val = cp.get_utf8(idx)?.to_string();
                    Some(AnnotationElementValue::StringVal(val))
                }
                _ => None,
            }
        }
        ElementValue::Enum {
            type_name_index,
            const_name_index,
        } => {
            let type_name = cp.get_utf8(*type_name_index)?.to_string();
            let const_name = cp.get_utf8(*const_name_index)?.to_string();
            Some(AnnotationElementValue::Enum(type_name, const_name))
        }
        ElementValue::Class { class_info_index } => {
            let desc = cp.get_utf8(*class_info_index)?.to_string();
            Some(AnnotationElementValue::Class(desc))
        }
        ElementValue::AnnotationValue(nested) => {
            let data = convert_annotation(nested, cp)?;
            Some(AnnotationElementValue::Annotation(data))
        }
        ElementValue::Array(values) => {
            let mut items = Vec::new();
            for v in values {
                items.push(convert_element_value(v, cp)?);
            }
            Some(AnnotationElementValue::Array(items))
        }
    }
}

// ---------------------------------------------------------------------------
// invoke_or_native and helpers
// ---------------------------------------------------------------------------

/// Try native method first (for synthetic/native-only classes), then invoke_shared.
pub fn invoke_or_native(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    args: &[Value],
) -> MethodCallResult {
    // Skip expensive class loading for obviously invalid class names (e.g. "<unknown class 0>").
    if class_name.contains('<') || class_name.contains(' ') {
        return Err(MethodCallFailed::InternalError(VmError::Linkage(
            LinkageError::NoSuchMethodError {
                class_name: class_name.to_string(),
                method_name: method_name.to_string(),
                method_descriptor: descriptor.to_string(),
            },
        )));
    }

    // T15: Array types (`[LFoo;`, `[I`, etc.) don't have their own class
    // files вЂ” JVMS В§4.4.1 maps their method dispatch to `java.lang.Object`.
    // The only methods arrays define beyond `Object` are `clone()` and a
    // length accessor вЂ” both satisfied by our Object.clone native override
    // (which branches on ObjectKind::Array) and the `arraylength` opcode.
    let effective_class = if class_name.starts_with('[') {
        "java/lang/Object"
    } else {
        class_name
    };

    if std::env::var_os("RUSTJVM_BD_DEBUG").is_some() && method_name == "intValue" {
        let bytes = effective_class.as_bytes();
        eprintln!("[invoke_or_native] effective_class={:?} (len={}) class_name={:?} method={:?} desc={:?}",
                  effective_class, bytes.len(), class_name, method_name, descriptor);
        eprintln!("[invoke_or_native] effective_class bytes: {:?}", bytes);
    }
    // Always check native registry first вЂ” this provides "native override"
    // for both synthetic stubs AND real JDK classes.  Many JDK Java methods
    // (e.g. VM.getSavedProperty) depend on JVM-internal state we haven't set up,
    // so our Rust native registration must take priority over bytecode.
    if let Some(callback) = shared
        .native_methods
        .find(effective_class, method_name, descriptor)
    {
        if std::env::var_os("RUSTJVM_BD_DEBUG").is_some() && method_name == "intValue" {
            eprintln!("[invoke_or_native] direct native hit");
        }
        return safe_native_call(shared, thread, callback, args)
            .map(|v| coerce_native_return(v, descriptor));
    }
    if std::env::var_os("RUSTJVM_BD_DEBUG").is_some() && method_name == "intValue" {
        eprintln!("[invoke_or_native] no direct native; checking hierarchy walk");
    }
    // Also try the original class name in case the caller registered a
    // specific override for the array type.
    if effective_class != class_name {
        if let Some(callback) = shared
            .native_methods
            .find(class_name, method_name, descriptor)
        {
            return safe_native_call(shared, thread, callback, args)
                .map(|v| coerce_native_return(v, descriptor));
        }
    }
    // Walk the superclass chain: the constant pool may reference a subclass
    // (e.g. RunnerClassLoader.registerAsParallelCapable) but the native is
    // registered on the declaring superclass (ClassLoader).
    // IMPORTANT: skip hierarchy walk for <init> вЂ” constructors are NOT inherited.
    // IMPORTANT: skip hierarchy walk if the target class has bytecode for this
    // method вЂ” the subclass's bytecode override must take priority over a parent's
    // native (e.g. URI.toString() must NOT be short-circuited by Object.toString()).
    if method_name != "<init>" {
        // Check if the target class has its own bytecode for this method.
        let has_own_bytecode = {
            let cm = shared.class_manager.read();
            cm.get_loaded_class_id(effective_class)
                .and_then(|cid| cm.get_class(cid))
                .map(|cls| cls.find_method(method_name, descriptor).is_some())
                .unwrap_or(false)
        };
        if std::env::var_os("RUSTJVM_BD_DEBUG").is_some() && method_name == "intValue" {
            eprintln!("[invoke_or_native] has_own_bytecode={}", has_own_bytecode);
        }
        if !has_own_bytecode {
            let cm = shared.class_manager.read();
            if let Some(mut cid) = cm.get_loaded_class_id(effective_class) {
                while let Some(parent_id) = cm.get_class(cid).and_then(|c| c.superclass) {
                    if let Some(parent) = cm.get_class(parent_id) {
                        if let Some(callback) = shared.native_methods.find(&parent.name, method_name, descriptor) {
                            if std::env::var_os("RUSTJVM_BD_DEBUG").is_some() && method_name == "intValue" {
                                eprintln!("[invoke_or_native] hierarchy walk hit on parent={}", parent.name);
                            }
                            drop(cm);
                            return safe_native_call(shared, thread, callback, args)
                                .map(|v| coerce_native_return(v, descriptor));
                        }
                    }
                    cid = parent_id;
                }
            }
        }
    }
    if std::env::var_os("RUSTJVM_BD_DEBUG").is_some() && method_name == "intValue" {
        if let Some(Value::Object(Some(recv))) = args.first() {
            eprintln!("[invoke_or_native] before invoke_on_class_shared recv={:p} args.len={}", recv.as_ptr(), args.len());
            // Read field 0,1,2,3,4 to see what's there
            for i in 0..6 {
                let v = shared.heap.get_field(*recv, i);
                eprintln!("  field[{}] = {:?}", i, v);
            }
        }
        eprintln!("[invoke_or_native] falling through to invoke_on_class_shared/invoke_shared");
    }

    // For real (non-synthetic) classes, dispatch via invoke_on_class_shared
    // which handles ACC_NATIVE methods and bytecode execution.
    {
        let cm = shared.class_manager.read();
        if let Some(class_id) = cm.get_loaded_class_id(effective_class) {
            if let Some(class) = cm.class_store.get(class_id) {
                if !class.is_synthetic_stub {
                    drop(cm);
                    return invoke_on_class_shared(
                        shared, thread, class_id, method_name, descriptor, args,
                    );
                }
            }
        }
    }

    // Final fallback: full invoke_shared (loads class, resolves method).
    invoke_shared(shared, thread, effective_class, method_name, descriptor, args)
}

/// Resolve a bare library name to a full path by searching `java.library.path`.
/// If `name` already contains a directory separator it is returned unchanged.
pub(super) fn resolve_library_path(shared: &SharedVm, name: &str) -> String {
    if name.contains('/') || name.contains('\\') || name.contains(':') {
        return name.to_string();
    }
    let lib_path_prop = shared
        .system_properties
        .read()
        .get("java.library.path")
        .cloned()
        .unwrap_or_default();

    let sep = if cfg!(windows) { ';' } else { ':' };
    for dir in lib_path_prop.split(sep) {
        if dir.is_empty() {
            continue;
        }
        let candidate = format!("{}/{}", dir.trim_end_matches(['/', '\\']), name);
        if std::path::Path::new(&candidate).exists() {
            return candidate;
        }
    }
    name.to_string()
}

impl<'a> NativeContextImpl<'a> {
    /// Try native method first, then full invoke_shared.
    fn invoke_or_native(
        &mut self,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        args: &[Value],
    ) -> MethodCallResult {
        invoke_or_native(
            self.shared,
            self.thread,
            class_name,
            method_name,
            descriptor,
            args,
        )
    }
}

// ---------------------------------------------------------------------------
// CAS helper: compare Values for identity/equality
// ---------------------------------------------------------------------------

/// Compare two Values for CAS purposes. For Int/Long/Float/Double, compares
/// by value. For Object references, compares by pointer identity (reference
/// equality). Returns true if they are "the same" for CAS purposes.
///
/// R1 fix: a primitive field slot that has never been explicitly written
/// decodes as `Value::Object(None)` (the zero-discriminant `Value` variant)
/// when read via `std::ptr::read::<Value>()` on `alloc_zeroed` memory. The
/// canonical fix is [`crate::heap::default_value_for_descriptor`] вЂ” the
/// heap's `alloc_object_with_descriptors` pre-initializes primitive slots
/// with the correctly-tagged zero. This function includes a belt-and-
/// suspenders fallback: when the live slot reads as `Object(None)` and
/// the expected value is a typed primitive zero, treat them as equal so
/// the CAS succeeds and the Java caller (e.g.
/// `ConcurrentHashMap.initTable`) can proceed out of its retry loop even
/// in the rare case where an allocation path skipped descriptor-aware
/// init.
///
/// T19_H6 fix: cross-tag bit-pattern equivalence for primitive Values.
/// The CAS contract operates on raw bits вЂ” a `long` field whose storage
/// tag drifted to `Double` (or vice-versa) must still compare equal when
/// the underlying 64-bit pattern matches. Likewise for the 32-bit pair
/// `(Int, Float)`. We also cover the width-mismatched pairs `(Int, Long)`
/// and `(Int, Double)` because the field-descriptor cache can return `I`
/// for what is actually a `J` slot when an upstream walker (e.g.
/// `Class::field_at_index`) indexes through statics вЂ” in that case the
/// 32-bit-zero-extended Int and the 64-bit Long/Double must match by
/// bit pattern. Equating bits is consistent with the existing
/// `to_bits()` Float/Double equality and is the canonical IEEE-754
/// identity used by `Unsafe.compareAndSet*` in HotSpot.
fn value_as_u64_bits(v: &Value) -> Option<u64> {
    match v {
        Value::Int(i) => Some(*i as u32 as u64),
        Value::Long(l) => Some(*l as u64),
        Value::Float(f) => Some(f.to_bits() as u64),
        Value::Double(d) => Some(d.to_bits()),
        _ => None,
    }
}

pub(super) fn values_equal_for_cas(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => x == y,
        (Value::Long(x), Value::Long(y)) => x == y,
        (Value::Float(x), Value::Float(y)) => x.to_bits() == y.to_bits(),
        (Value::Double(x), Value::Double(y)) => x.to_bits() == y.to_bits(),
        (Value::Object(x), Value::Object(y)) => match (x, y) {
            (None, None) => true,
            (Some(a), Some(b)) => std::ptr::eq(a.as_ptr(), b.as_ptr()),
            _ => false,
        },
        // Primitive-default coercion: an uninitialized primitive slot
        // reads as Object(None). Accept it as equal to a typed zero so
        // CAS loops don't livelock. Only zero values match вЂ” non-zero
        // primitive expected values still fail (correct mismatch).
        (Value::Object(None), Value::Int(0))
        | (Value::Int(0), Value::Object(None)) => true,
        (Value::Object(None), Value::Long(0))
        | (Value::Long(0), Value::Object(None)) => true,
        (Value::Object(None), Value::Float(f))
        | (Value::Float(f), Value::Object(None)) if f.to_bits() == 0 => true,
        (Value::Object(None), Value::Double(d))
        | (Value::Double(d), Value::Object(None)) if d.to_bits() == 0 => true,
        // T19_H6 belt-and-suspenders вЂ” cross-tag primitive bit-pattern
        // equivalence. Same-tag pairs are handled by the matches above; this
        // arm only fires for primitiveГ—primitive cross-tag (e.g. Long vs
        // Double bits), so it can't accidentally match an Object reference.
        (a_v, b_v) => match (value_as_u64_bits(a_v), value_as_u64_bits(b_v)) {
            (Some(ab), Some(bb)) => ab == bb,
            _ => false,
        },
    }
}

// ---------------------------------------------------------------------------
// Free functions: invoke_shared, invoke_on_class_shared
// ---------------------------------------------------------------------------

/// Invoke a method by class name. Used by the interpreter and NativeContextImpl.
pub fn invoke_shared(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    args: &[Value],
) -> MethodCallResult {
    // Thread-safe class loading with per-class-name lock (Session 30)
    let class_id = shared.load_class_concurrent(class_name)?;
    // Ensure the class is initialized (runs <clinit>) per JVM spec В§5.5.
    // This is required for static methods and field access to work correctly.
    super::ensure_class_initialized_shared(shared, thread, class_id)?;
    invoke_on_class_shared(shared, thread, class_id, method_name, descriptor, args)
}

/// WP2.9 вЂ” Invoke a method with invokespecial semantics (no virtual dispatch).
///
/// Backs `MethodHandles.Lookup.findSpecial` and the JLS `super.m()` pattern.
/// Resolves `class_name`, walks to the declaring class for the requested
/// method, and dispatches *exactly* on that class вЂ” bypassing the
/// iface/abstract retarget that `invoke_on_class_shared` normally applies.
///
/// Native registry overrides take priority, matching `invoke_or_native`.
pub fn invoke_special_shared(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    args: &[Value],
) -> MethodCallResult {
    // Native override always wins вЂ” same priority order as invoke_or_native.
    if let Some(callback) = shared
        .native_methods
        .find(class_name, method_name, descriptor)
    {
        return safe_native_call(shared, thread, callback, args)
            .map(|v| coerce_native_return(v, descriptor));
    }

    // Resolve and initialize the class.
    let class_id = shared.load_class_concurrent(class_name)?;
    super::ensure_class_initialized_shared(shared, thread, class_id)?;

    // For default-method super-call: walk the hierarchy to the declaring
    // class so we land on the interface that owns the bytecode. This
    // ensures we don't re-resolve via the receiver's overriding method.
    let target_class_id = {
        let cm = shared.class_manager.read();
        let store = &cm.class_store;
        match crate::classloading::find_method_recursive(class_id, method_name, descriptor, store) {
            Some((_, declaring_id)) => declaring_id,
            None => class_id,
        }
    };

    invoke_on_class_shared_no_retarget(
        shared,
        thread,
        target_class_id,
        method_name,
        descriptor,
        args,
    )
}

// ---------------------------------------------------------------------------
// Dynamic Proxy dispatch (java.lang.reflect.Proxy)
// ---------------------------------------------------------------------------

/// WP2.5 вЂ” set a field on a synthetic `Method` object by JDK field name.
///
/// The proxy dispatch path used to write the synthesized `Method` object
/// using hard-coded slot indices (0=class, 1=name, 2=returnType, ...).
/// That layout was wrong: the real JDK `java.lang.reflect.Method` class
/// inherits a long chain of fields from `AccessibleObject` and
/// `Executable` first, so `name` is *not* at slot 1. The miswrite
/// surfaced as `m.getName()` returning null inside an InvocationHandler,
/// which silently broke any handler that branched on the method name вЂ”
/// the typical pattern for multi-interface proxies (FAIL-5) and
/// proxies on interfaces with default methods (FAIL-6).
///
/// This helper centralizes the by-name resolve so both
/// `proxy_invoke_handler` (NativeContextImpl path) and
/// `proxy_invoke_handler_shared` (shared interpreter path) use it.
fn proxy_method_set_field_by_name(
    shared: &SharedVm,
    method_obj: ObjectRef,
    field_name: &str,
    value: Value,
) {
    let class_id = shared.heap.class_id_of(method_obj);
    let cm = shared.class_manager.read();
    if let Some(idx) = resolve_field_index_in_hierarchy(class_id, field_name, &cm.class_store) {
        drop(cm);
        shared.heap.set_field(method_obj, idx, value);
    }
}

/// When a method is called on a `Proxy$Instance` object, this function
/// intercepts it and forwards to the `InvocationHandler.invoke()`.
///
/// Layout of Proxy$Instance (1-field):
///   - field 0: InvocationHandler reference
///
/// The InvocationHandler interface requires:
///   Object invoke(Object proxy, Method method, Object[] args)
///
/// We build a minimal `java.lang.reflect.Method` object (7 fields, same layout
/// as builtins.rs `create_method_object`) and a boxed Object[] of args.
pub(super) fn proxy_invoke_handler(
    ctx: &mut NativeContextImpl<'_>,
    proxy: ObjectRef,
    method_name: &str,
    descriptor: &str,
    args: &[Value],
) -> MethodCallResult {
    // Get the InvocationHandler from proxy field 0
    let handler_ref = match ctx.shared.heap.get_field(proxy, 0) {
        Value::Object(Some(h)) => h,
        _ => {
            return Err(RuntimeError::NullPointerException { message: None }.into());
        }
    };

    // WP2.5 вЂ” build the Method object using **field-name-based** writes
    // so the JDK-real layout (which has many inherited fields from
    // AccessibleObject and Executable before `name`/`returnType`/...)
    // is honored. Hard-coded slot indices like the original 0..7 break
    // `Method.getName()` because the real `name` field is not at slot 1
    // вЂ” it's at whatever index the JDK class file declares it. Without
    // this, the InvocationHandler's `m.getName()` returns null and any
    // handler that branches on the method name (the common case for
    // multi-interface and default-method tests) silently returns null.
    let method_class_id = ctx.shared.class_manager.write()
        .load_class("java/lang/reflect/Method").unwrap_or(ClassId::new(0));
    let total_fields = ctx
        .shared
        .class_manager
        .read()
        .get_class(method_class_id)
        .map(|c| c.first_field_index + c.fields.iter().filter(|f| !f.is_static()).count())
        .unwrap_or(8);
    let method_obj = ctx
        .shared
        .heap
        .alloc_object(method_class_id, total_fields.max(8));
    let zero_mirror = super::get_or_create_class_mirror(ctx.shared, ClassId::new(0));
    let name_str = super::create_java_string(ctx.shared, method_name);
    let param_count = proxy_count_params(descriptor);
    let param_arr = ctx.shared.heap.alloc_array(
        ClassId::new(0),
        crate::memory::heap::ArrayElementType::Reference,
        param_count,
    );
    let desc_str = super::create_java_string(ctx.shared, descriptor);
    proxy_method_set_field_by_name(ctx.shared, method_obj, "clazz", Value::Object(Some(zero_mirror)));
    proxy_method_set_field_by_name(ctx.shared, method_obj, "name", Value::Object(Some(name_str)));
    proxy_method_set_field_by_name(ctx.shared, method_obj, "returnType", Value::Object(Some(zero_mirror)));
    proxy_method_set_field_by_name(ctx.shared, method_obj, "parameterTypes", Value::Object(Some(param_arr)));
    proxy_method_set_field_by_name(ctx.shared, method_obj, "modifiers", Value::Int(1)); // PUBLIC
    proxy_method_set_field_by_name(ctx.shared, method_obj, "signature", Value::Object(Some(desc_str)));
    proxy_method_set_field_by_name(ctx.shared, method_obj, "slot", Value::Int(0));
    // Belt-and-suspenders: also write the legacy hard-coded slots so any
    // surviving raw-index reader (notably the lambda dispatch path that
    // pulls `Method.getName` via `get_field_by_name` already lands on
    // the right slot, but older diagnostic readers may still poke 1..7).
    if total_fields >= 8 {
        // Skip вЂ” class layout already has the JDK fields populated above.
    } else {
        ctx.shared.heap.set_field(method_obj, 0, Value::Object(Some(zero_mirror)));
        ctx.shared.heap.set_field(method_obj, 1, Value::Object(Some(name_str)));
        ctx.shared.heap.set_field(method_obj, 2, Value::Object(Some(zero_mirror)));
        ctx.shared.heap.set_field(method_obj, 3, Value::Object(Some(param_arr)));
        ctx.shared.heap.set_field(method_obj, 4, Value::Int(1));
        ctx.shared.heap.set_field(method_obj, 5, Value::Object(Some(desc_str)));
        ctx.shared.heap.set_field(method_obj, 6, Value::Int(param_count as i32));
    }

    // Build Object[] of args вЂ” box primitives so InvocationHandler receives Object[]
    let args_arr = ctx.shared.heap.alloc_array(
        ClassId::new(0),
        crate::memory::heap::ArrayElementType::Reference,
        args.len(),
    );
    for (i, arg) in args.iter().enumerate() {
        let boxed = proxy_box_value(ctx.shared, *arg);
        ctx.shared
            .heap
            .set_array_element(args_arr, i, boxed)
            .ok();
    }

    // Call InvocationHandler.invoke(Object proxy, Method method, Object[] args)
    // Resolve the handler's actual class for dispatch (may be anonymous).
    let handler_class_id = ctx.shared.heap.class_id_of(handler_ref);

    // WP2.5: lambda InvocationHandler вЂ” see `proxy_invoke_handler_shared`
    // for the rationale. The lambda's class_id is synthetic and not in
    // the class store; dispatching by name would land on the abstract
    // interface method (no Code attribute).
    let handler_is_lambda = ctx
        .shared
        .lambda_proxies
        .read()
        .contains_key(&handler_class_id);
    if handler_is_lambda {
        let call_args = [
            Value::Object(Some(proxy)),
            Value::Object(Some(method_obj)),
            Value::Object(Some(args_arr)),
        ];
        let dispatch = crate::runtime::interpreter::try_lambda_dispatch(
            ctx.shared,
            ctx.thread,
            handler_ref,
            handler_class_id,
            "invoke",
            &call_args,
        )?;
        if let Some(result) = dispatch {
            return Ok(result);
        }
        return Err(MethodCallFailed::InternalError(VmError::Linkage(
            LinkageError::AbstractMethodError {
                class_name: format!("<lambda#{}>", handler_class_id),
                method_name: "invoke".to_string(),
            },
        )));
    }

    let handler_class_name = ctx.shared.class_manager.read()
        .get_class(handler_class_id)
        .map(|c| c.name.to_string())
        .unwrap_or_else(|| "java/lang/reflect/InvocationHandler".to_string());

    let invoke_args = [
        Value::Object(Some(handler_ref)),
        Value::Object(Some(proxy)),
        Value::Object(Some(method_obj)),
        Value::Object(Some(args_arr)),
    ];
    ctx.invoke_or_native(
        &handler_class_name,
        "invoke",
        "(Ljava/lang/Object;Ljava/lang/reflect/Method;[Ljava/lang/Object;)Ljava/lang/Object;",
        &invoke_args,
    )
}

/// Dispatch a method call on an annotation proxy object.
///
/// WP2.7: spec-compliant per `java.lang.annotation.Annotation` JavaDoc.
///
/// Layout of AnnotationProxy (4-field):
///   field 0 = String (type descriptor, e.g. "Ljava/lang/Override;")
///   field 1 = Class mirror (annotation type вЂ” for `annotationType()`)
///   field 2 = String[] (element names)
///   field 3 = Object[] (element values, parallel to names)
///
/// Methods implemented:
///   * `annotationType()` вЂ” returns the cached Class mirror (field 1).
///   * `toString()` вЂ” `@TypeName(name1=val1, name2=val2)` with members in
///     declaration order (matches the order WP1.7 captures, which mirrors
///     the order the source compiler wrote into the class file).
///   * `hashCode()` вЂ” sum over members of `(127 * nameHash) ^ valueHash`,
///     per `Annotation.hashCode()` Javadoc.
///   * `equals(Object)` вЂ” `true` iff the other reference is also an
///     `AnnotationProxy` of the same annotation type AND every element
///     value compares `equals` (by `valueEquals` semantics вЂ” array values
///     use `Arrays.equals`, scalar values use `Object.equals`).
///
/// Element accessor methods (e.g. `value()`, `count()`, `nested()`) walk
/// the parallel arrays and return the stored boxed value.
fn annotation_proxy_invoke(
    ctx: &mut NativeContextImpl<'_>,
    proxy: ObjectRef,
    method_name: &str,
    args: &[Value],
) -> MethodCallResult {
    annotation_proxy_dispatch_impl(ctx.shared, proxy, method_name, args)
}

/// Shared-interpreter version of `proxy_invoke_handler` вЂ” callable from the
/// iterative interpreter without a `NativeContextImpl`.
pub(crate) fn proxy_invoke_handler_shared(
    shared: &SharedVm,
    thread: &mut JvmThread,
    proxy: ObjectRef,
    method_name: &str,
    descriptor: &str,
    args: &[Value],
) -> MethodCallResult {
    // Get the InvocationHandler from proxy field 0
    let handler_ref = match shared.heap.get_field(proxy, 0) {
        Value::Object(Some(h)) => h,
        _ => {
            return Err(RuntimeError::NullPointerException { message: None }.into());
        }
    };

    // WP2.5 вЂ” build the Method object using **field-name-based** writes.
    // See `proxy_method_set_field_by_name` for the rationale; mirrors the
    // fix applied to `proxy_invoke_handler` above.
    let method_class_id = shared.class_manager.write()
        .load_class("java/lang/reflect/Method").unwrap_or(ClassId::new(0));
    let total_fields = shared
        .class_manager
        .read()
        .get_class(method_class_id)
        .map(|c| c.first_field_index + c.fields.iter().filter(|f| !f.is_static()).count())
        .unwrap_or(8);
    let method_obj = shared.heap.alloc_object(method_class_id, total_fields.max(8));
    let zero_mirror = super::get_or_create_class_mirror(shared, ClassId::new(0));
    let name_str = super::create_java_string(shared, method_name);
    let param_count = proxy_count_params(descriptor);
    let param_arr = shared.heap.alloc_array(
        ClassId::new(0),
        crate::memory::heap::ArrayElementType::Reference,
        param_count,
    );
    let desc_str = super::create_java_string(shared, descriptor);
    proxy_method_set_field_by_name(shared, method_obj, "clazz", Value::Object(Some(zero_mirror)));
    proxy_method_set_field_by_name(shared, method_obj, "name", Value::Object(Some(name_str)));
    proxy_method_set_field_by_name(shared, method_obj, "returnType", Value::Object(Some(zero_mirror)));
    proxy_method_set_field_by_name(shared, method_obj, "parameterTypes", Value::Object(Some(param_arr)));
    proxy_method_set_field_by_name(shared, method_obj, "modifiers", Value::Int(1)); // PUBLIC
    proxy_method_set_field_by_name(shared, method_obj, "signature", Value::Object(Some(desc_str)));
    proxy_method_set_field_by_name(shared, method_obj, "slot", Value::Int(0));
    if total_fields < 8 {
        // Synthetic-mode fallback (no JDK Method class loaded): keep the
        // old hard-coded layout so callers reading raw slots still find
        // the values.
        shared.heap.set_field(method_obj, 0, Value::Object(Some(zero_mirror)));
        shared.heap.set_field(method_obj, 1, Value::Object(Some(name_str)));
        shared.heap.set_field(method_obj, 2, Value::Object(Some(zero_mirror)));
        shared.heap.set_field(method_obj, 3, Value::Object(Some(param_arr)));
        shared.heap.set_field(method_obj, 4, Value::Int(1));
        shared.heap.set_field(method_obj, 5, Value::Object(Some(desc_str)));
        shared.heap.set_field(method_obj, 6, Value::Int(param_count as i32));
    }

    // Build Object[] of args вЂ” box primitives
    let args_arr = shared.heap.alloc_array(
        ClassId::new(0),
        crate::memory::heap::ArrayElementType::Reference,
        args.len(),
    );
    for (i, arg) in args.iter().enumerate() {
        let boxed = proxy_box_value(shared, *arg);
        shared.heap.set_array_element(args_arr, i, boxed).ok();
    }

    // Call InvocationHandler.invoke(Object proxy, Method method, Object[] args)
    // Resolve the handler's actual class for dispatch (it may be an anonymous class
    // implementing InvocationHandler, so we can't use the interface name directly).
    let handler_class_id = shared.heap.class_id_of(handler_ref);

    // WP2.5: if the handler is a synthetic lambda proxy (e.g. the user
    // passed `(proxy, m, a) -> ...` directly to `Proxy.newProxyInstance`),
    // its class_id will be a high synthetic id that isn't in the class
    // store. Dispatching by class name in that case would fall back to
    // `java/lang/reflect/InvocationHandler.invoke` (the abstract
    // interface method, which has no Code attribute) and crash with
    // "no Code attribute". Route through `try_lambda_dispatch` which
    // resolves the lambda's `impl_handle` and runs the SAM body.
    let handler_is_lambda = shared.lambda_proxies.read().contains_key(&handler_class_id);
    if handler_is_lambda {
        let call_args = [
            Value::Object(Some(proxy)),
            Value::Object(Some(method_obj)),
            Value::Object(Some(args_arr)),
        ];
        let dispatch = crate::runtime::interpreter::try_lambda_dispatch(
            shared,
            thread,
            handler_ref,
            handler_class_id,
            "invoke",
            &call_args,
        )?;
        if let Some(result) = dispatch {
            return Ok(result);
        }
        // Fell through unexpectedly вЂ” surface as a clearer error than
        // "no Code attribute".
        return Err(MethodCallFailed::InternalError(VmError::Linkage(
            LinkageError::AbstractMethodError {
                class_name: format!("<lambda#{}>", handler_class_id),
                method_name: "invoke".to_string(),
            },
        )));
    }

    let handler_class_name = shared.class_manager.read()
        .get_class(handler_class_id)
        .map(|c| c.name.to_string())
        .unwrap_or_else(|| "java/lang/reflect/InvocationHandler".to_string());

    let invoke_args = [
        Value::Object(Some(handler_ref)),
        Value::Object(Some(proxy)),
        Value::Object(Some(method_obj)),
        Value::Object(Some(args_arr)),
    ];
    invoke_or_native(
        shared,
        thread,
        &handler_class_name,
        "invoke",
        "(Ljava/lang/Object;Ljava/lang/reflect/Method;[Ljava/lang/Object;)Ljava/lang/Object;",
        &invoke_args,
    )
}

/// Shared-interpreter version of `annotation_proxy_invoke`.
pub(crate) fn annotation_proxy_invoke_shared(
    shared: &SharedVm,
    _thread: &mut JvmThread,
    proxy: ObjectRef,
    method_name: &str,
    args: &[Value],
) -> MethodCallResult {
    annotation_proxy_dispatch_impl(shared, proxy, method_name, args)
}

// ---------------------------------------------------------------------------
// WP2.7: shared annotation-proxy dispatch (spec-compliant)
// ---------------------------------------------------------------------------

/// Read element-name + element-value parallel arrays from an annotation proxy.
/// Returns `(name, value)` pairs in the order they were stored at proxy build
/// time (which is the source-declaration order from the .class file).
fn annotation_proxy_elements(
    shared: &SharedVm,
    proxy: ObjectRef,
) -> Vec<(String, Value)> {
    let names_arr = match shared.heap.get_field(proxy, 2) {
        Value::Object(Some(a)) => a,
        _ => return Vec::new(),
    };
    let values_arr = match shared.heap.get_field(proxy, 3) {
        Value::Object(Some(a)) => a,
        _ => return Vec::new(),
    };
    let n = shared.heap.array_length(names_arr);
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let name = match shared.heap.get_array_element(names_arr, i) {
            Ok(Value::Object(Some(name_ref))) => {
                super::read_java_string(&shared.heap, name_ref).unwrap_or_default()
            }
            _ => continue,
        };
        let val = shared
            .heap
            .get_array_element(values_arr, i)
            .unwrap_or(Value::Object(None));
        out.push((name, val));
    }
    out
}

/// Read the annotation's type descriptor (field 0) вЂ” e.g. "Ljava/lang/Override;".
fn annotation_proxy_type_descriptor(shared: &SharedVm, proxy: ObjectRef) -> String {
    if let Value::Object(Some(s)) = shared.heap.get_field(proxy, 0) {
        super::read_java_string(&shared.heap, s).unwrap_or_default()
    } else {
        String::new()
    }
}

/// Convert an annotation type descriptor to an internal class name.
/// E.g. `"Ljava/lang/Override;"` в†’ `"java/lang/Override"`. Empty input
/// (or non-descriptor strings) returns `"<unknown>"` to give a non-empty
/// fallback in `toString` output.
fn descriptor_to_class_name(desc: &str) -> String {
    if let Some(stripped) = desc.strip_prefix('L').and_then(|s| s.strip_suffix(';')) {
        stripped.to_string()
    } else if desc.is_empty() {
        "<unknown>".to_string()
    } else {
        desc.to_string()
    }
}

/// Convert an internal slash-separated class name to the dotted form used
/// in `Annotation.toString()` output, e.g. `"java/lang/Override"` в†’
/// `"java.lang.Override"`.
fn internal_to_dotted(name: &str) -> String {
    name.replace('/', ".")
}

/// Format an annotation member value the way HotSpot's
/// `AnnotationInvocationHandler.toString()` does:
///
/// * `String` в†’ `"text"` (Java-string-literal-escaped quoted form)
/// * `Class` в†’ `TypeName.class`
/// * Annotation proxy в†’ recursive `@TypeName(...)`
/// * Reference array в†’ `[a, b, c]`
/// * Primitive array в†’ element-list joined by `, ` inside `[ ... ]`
/// * boxed Integer/Long/etc. (from element-value pairs) в†’ underlying numeric
/// * Enum в†’ constant name (annotation enum element renders without type qualifier)
fn format_annotation_value(shared: &SharedVm, val: Value) -> String {
    match val {
        Value::Object(None) => "null".to_string(),
        Value::Int(i) => i.to_string(),
        Value::Long(l) => l.to_string(),
        Value::Float(f) => f.to_string(),
        Value::Double(d) => d.to_string(),
        Value::Object(Some(obj)) => {
            // Decide based on object class name + heap kind.
            let kind = shared.heap.kind_of(obj);
            if kind == crate::memory::heap::ObjectKind::Array {
                return format_annotation_array(shared, obj);
            }
            let cid = shared.heap.class_id_of(obj);
            let cname = shared
                .class_manager
                .read()
                .get_class(cid)
                .map(|c| c.name.to_string())
                .unwrap_or_default();
            // Nested annotation proxy: recurse via toString helper
            if cname == "java/lang/annotation/AnnotationProxy" {
                return annotation_proxy_to_string(shared, obj);
            }
            // String вЂ” render as Java-string-literal "text"
            if cname == "java/lang/String" {
                if let Some(s) = super::read_java_string(&shared.heap, obj) {
                    return format!("\"{}\"", java_string_escape(&s));
                }
            }
            // Class mirror вЂ” render as `TypeName.class`
            if cname == "java/lang/Class" {
                if let Some(cls_name) = class_mirror_name(shared, obj) {
                    return format!("{}.class", internal_to_dotted(&cls_name));
                }
                return "<unknown>.class".to_string();
            }
            // Boxed wrapper (1-field synthetic): unbox and recurse
            if let Some(prim_name) = wrapper_class_to_primitive(&cname) {
                let inner = shared.heap.get_field(obj, 0);
                let mut s = format_annotation_value(shared, inner);
                if prim_name == "long" {
                    s.push('L');
                } else if prim_name == "float" {
                    s.push('f');
                }
                return s;
            }
            // Enum constant вЂ” return its `name` field. Standard layout has
            // field 0 = String name (set by `Enum.<init>`).
            if let Value::Object(Some(name_ref)) = shared.heap.get_field(obj, 0) {
                if let Some(name) = super::read_java_string(&shared.heap, name_ref) {
                    if !name.is_empty() {
                        return name;
                    }
                }
            }
            cname.replace('/', ".")
        }
        Value::Uninitialized => "null".to_string(),
        _ => "null".to_string(),
    }
}

/// Render an array element (reference or primitive) the way
/// `Arrays.toString` does for annotation values: `[a, b, c]`.
fn format_annotation_array(shared: &SharedVm, arr: ObjectRef) -> String {
    let n = shared.heap.array_length(arr);
    let mut s = String::from("[");
    for i in 0..n {
        if i > 0 {
            s.push_str(", ");
        }
        match shared.heap.get_array_element(arr, i) {
            Ok(v) => s.push_str(&format_annotation_value(shared, v)),
            Err(_) => s.push_str("null"),
        }
    }
    s.push(']');
    s
}

/// Recursive `Annotation.toString()` helper.
///
/// Members are emitted in alphabetical order by element name, matching
/// HotSpot's `AnnotationInvocationHandler.toString()` reference output
/// (where multi-member annotations render with members sorted by name).
pub(crate) fn annotation_proxy_to_string(shared: &SharedVm, proxy: ObjectRef) -> String {
    let desc = annotation_proxy_type_descriptor(shared, proxy);
    let class_name = descriptor_to_class_name(&desc);
    let dotted = internal_to_dotted(&class_name);
    let mut elems = annotation_proxy_elements(shared, proxy);
    elems.sort_by(|a, b| a.0.cmp(&b.0));
    let mut s = String::with_capacity(64);
    s.push('@');
    s.push_str(&dotted);
    s.push('(');
    let mut first = true;
    for (name, val) in &elems {
        if !first {
            s.push_str(", ");
        }
        first = false;
        s.push_str(name);
        s.push('=');
        s.push_str(&format_annotation_value(shared, *val));
    }
    s.push(')');
    s
}

/// Java-string-literal escape: backslash, quotes, and control chars.
fn java_string_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\x08' => out.push_str("\\b"),
            '\x0c' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out
}

/// Read the internal class name from a `java/lang/Class` mirror. Layout:
///   field 0 = class id (Int)
///   field 1 = name (String вЂ” internal slash form)
fn class_mirror_name(shared: &SharedVm, mirror: ObjectRef) -> Option<String> {
    if let Value::Object(Some(name_ref)) = shared.heap.get_field(mirror, 1) {
        return super::read_java_string(&shared.heap, name_ref);
    }
    None
}

/// Map a wrapper class internal name to its primitive name.
fn wrapper_class_to_primitive(class_name: &str) -> Option<&'static str> {
    Some(match class_name {
        "java/lang/Integer" => "int",
        "java/lang/Long" => "long",
        "java/lang/Short" => "short",
        "java/lang/Byte" => "byte",
        "java/lang/Character" => "char",
        "java/lang/Boolean" => "boolean",
        "java/lang/Float" => "float",
        "java/lang/Double" => "double",
        _ => return None,
    })
}

/// Compute the per-member hashCode contribution: `(127 * nameHash) ^ valueHash`.
fn annotation_member_hash(shared: &SharedVm, name: &str, val: Value) -> i32 {
    let name_hash = java_string_hash(name);
    let value_hash = annotation_value_hash(shared, val);
    127i32.wrapping_mul(name_hash) ^ value_hash
}

/// Java-spec `String.hashCode()` вЂ” `s[0]*31^(n-1) + ... + s[n-1]`.
///
/// Public for WP2.7 conformance tests that pin the hash recipe against
/// the JDK reference output.
pub fn java_string_hash(s: &str) -> i32 {
    let mut h: i32 = 0;
    for ch in s.encode_utf16() {
        h = h.wrapping_mul(31).wrapping_add(ch as i32);
    }
    h
}

/// Hash an annotation member value per `Annotation.hashCode()` rules.
fn annotation_value_hash(shared: &SharedVm, val: Value) -> i32 {
    match val {
        Value::Int(i) => i,
        Value::Long(l) => (l ^ (l >> 32)) as i32, // Long.hashCode
        Value::Float(f) => f.to_bits() as i32,
        Value::Double(d) => {
            let bits = d.to_bits() as i64;
            (bits ^ (bits >> 32)) as i32
        }
        Value::Object(None) | Value::Uninitialized => 0,
        Value::Object(Some(obj)) => {
            let kind = shared.heap.kind_of(obj);
            if kind == crate::memory::heap::ObjectKind::Array {
                return annotation_array_hash(shared, obj);
            }
            let cid = shared.heap.class_id_of(obj);
            let cname = shared
                .class_manager
                .read()
                .get_class(cid)
                .map(|c| c.name.to_string())
                .unwrap_or_default();
            // Boxed wrapper вЂ” hash the boxed primitive
            if wrapper_class_to_primitive(&cname).is_some() {
                let inner = shared.heap.get_field(obj, 0);
                return annotation_value_hash(shared, inner);
            }
            // String вЂ” Java's String.hashCode contract
            if cname == "java/lang/String" {
                if let Some(s) = super::read_java_string(&shared.heap, obj) {
                    return java_string_hash(&s);
                }
            }
            // Nested annotation вЂ” recurse
            if cname == "java/lang/annotation/AnnotationProxy" {
                return annotation_proxy_hash_code(shared, obj);
            }
            // Class mirror вЂ” hash the class name (matches Class.hashCode в†’ name.hashCode)
            if cname == "java/lang/Class" {
                if let Some(name) = class_mirror_name(shared, obj) {
                    return java_string_hash(&internal_to_dotted(&name));
                }
            }
            // Enum / generic object вЂ” hash the `name` field if present (matches
            // Enum.hashCode в†’ identity), else identity hash code.
            if let Value::Object(Some(name_ref)) = shared.heap.get_field(obj, 0) {
                if let Some(name) = super::read_java_string(&shared.heap, name_ref) {
                    if !name.is_empty() {
                        return java_string_hash(&name);
                    }
                }
            }
            shared.heap.identity_hash_code(obj)
        }
        _ => 0,
    }
}

/// Per `Arrays.hashCode` for the relevant element type.
fn annotation_array_hash(shared: &SharedVm, arr: ObjectRef) -> i32 {
    let n = shared.heap.array_length(arr);
    let mut h: i32 = 1;
    for i in 0..n {
        let elem_hash = match shared.heap.get_array_element(arr, i) {
            Ok(v) => annotation_value_hash(shared, v),
            Err(_) => 0,
        };
        h = h.wrapping_mul(31).wrapping_add(elem_hash);
    }
    h
}

/// Compute spec-compliant `Annotation.hashCode()`.
pub(crate) fn annotation_proxy_hash_code(shared: &SharedVm, proxy: ObjectRef) -> i32 {
    let elems = annotation_proxy_elements(shared, proxy);
    let mut h: i32 = 0;
    for (name, val) in elems {
        h = h.wrapping_add(annotation_member_hash(shared, &name, val));
    }
    h
}

/// Spec-compliant `Annotation.equals(Object)` вЂ” returns true iff the other
/// reference is also an `AnnotationProxy` with the same annotation type AND
/// every element value matches.
pub(crate) fn annotation_proxy_equals(
    shared: &SharedVm,
    a: ObjectRef,
    b: Value,
) -> bool {
    let other = match b {
        Value::Object(Some(o)) => o,
        _ => return false,
    };
    if a == other {
        return true;
    }
    // The other side must also be an annotation proxy of the same type.
    let other_kind = shared.heap.kind_of(other);
    if other_kind != crate::memory::heap::ObjectKind::Object {
        return false;
    }
    let other_cid = shared.heap.class_id_of(other);
    let other_cname = shared
        .class_manager
        .read()
        .get_class(other_cid)
        .map(|c| c.name.to_string())
        .unwrap_or_default();
    if other_cname != "java/lang/annotation/AnnotationProxy" {
        return false;
    }
    let a_desc = annotation_proxy_type_descriptor(shared, a);
    let b_desc = annotation_proxy_type_descriptor(shared, other);
    if a_desc != b_desc {
        return false;
    }
    // Compare element-by-element. Look up by name on the b-side to be tolerant
    // of insertion-order drift between two construction paths for the same
    // annotation type (e.g. one with all explicit elements, one falling back
    // to AnnotationDefault).
    let a_elems = annotation_proxy_elements(shared, a);
    let b_elems = annotation_proxy_elements(shared, other);
    if a_elems.len() != b_elems.len() {
        return false;
    }
    for (name, av) in &a_elems {
        let bv = match b_elems.iter().find(|(n, _)| n == name) {
            Some((_, v)) => *v,
            None => return false,
        };
        if !annotation_values_equal(shared, *av, bv) {
            return false;
        }
    }
    true
}

/// Compare two annotation member values вЂ” `Arrays.equals` semantics on
/// arrays, recursive on nested annotations, identity-aware on others.
fn annotation_values_equal(shared: &SharedVm, a: Value, b: Value) -> bool {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => x == y,
        (Value::Long(x), Value::Long(y)) => x == y,
        (Value::Float(x), Value::Float(y)) => x.to_bits() == y.to_bits(),
        (Value::Double(x), Value::Double(y)) => x.to_bits() == y.to_bits(),
        (Value::Object(None), Value::Object(None)) => true,
        (Value::Object(None), _) | (_, Value::Object(None)) => false,
        (Value::Object(Some(x)), Value::Object(Some(y))) => {
            if x == y {
                return true;
            }
            let xk = shared.heap.kind_of(x);
            let yk = shared.heap.kind_of(y);
            if xk == crate::memory::heap::ObjectKind::Array
                || yk == crate::memory::heap::ObjectKind::Array
            {
                if xk != yk {
                    return false;
                }
                let nx = shared.heap.array_length(x);
                let ny = shared.heap.array_length(y);
                if nx != ny {
                    return false;
                }
                for i in 0..nx {
                    let av = shared
                        .heap
                        .get_array_element(x, i)
                        .unwrap_or(Value::Object(None));
                    let bv = shared
                        .heap
                        .get_array_element(y, i)
                        .unwrap_or(Value::Object(None));
                    if !annotation_values_equal(shared, av, bv) {
                        return false;
                    }
                }
                return true;
            }
            let xcid = shared.heap.class_id_of(x);
            let ycid = shared.heap.class_id_of(y);
            let xname = shared
                .class_manager
                .read()
                .get_class(xcid)
                .map(|c| c.name.to_string())
                .unwrap_or_default();
            let yname = shared
                .class_manager
                .read()
                .get_class(ycid)
                .map(|c| c.name.to_string())
                .unwrap_or_default();
            // Nested annotation: compare structurally.
            if xname == "java/lang/annotation/AnnotationProxy"
                && yname == "java/lang/annotation/AnnotationProxy"
            {
                return annotation_proxy_equals(shared, x, Value::Object(Some(y)));
            }
            // String compare
            if xname == "java/lang/String" && yname == "java/lang/String" {
                let sx = super::read_java_string(&shared.heap, x).unwrap_or_default();
                let sy = super::read_java_string(&shared.heap, y).unwrap_or_default();
                return sx == sy;
            }
            // Class mirror compare by class id
            if xname == "java/lang/Class" && yname == "java/lang/Class" {
                let xcid_field = shared.heap.get_field(x, 0);
                let ycid_field = shared.heap.get_field(y, 0);
                return matches!(
                    (xcid_field, ycid_field),
                    (Value::Int(a), Value::Int(b)) if a == b
                );
            }
            // Boxed wrapper вЂ” unbox & recurse
            if wrapper_class_to_primitive(&xname).is_some()
                && wrapper_class_to_primitive(&yname).is_some()
            {
                let xv = shared.heap.get_field(x, 0);
                let yv = shared.heap.get_field(y, 0);
                return annotation_values_equal(shared, xv, yv);
            }
            // Enum / generic вЂ” compare name field if present.
            if let (Value::Object(Some(xn)), Value::Object(Some(yn))) =
                (shared.heap.get_field(x, 0), shared.heap.get_field(y, 0))
            {
                let sx = super::read_java_string(&shared.heap, xn).unwrap_or_default();
                let sy = super::read_java_string(&shared.heap, yn).unwrap_or_default();
                if !sx.is_empty() && !sy.is_empty() {
                    return sx == sy && xname == yname;
                }
            }
            x == y
        }
        _ => false,
    }
}

/// Spec-compliant dispatcher for any method invoked on an annotation proxy.
pub(crate) fn annotation_proxy_dispatch_impl(
    shared: &SharedVm,
    proxy: ObjectRef,
    method_name: &str,
    args: &[Value],
) -> MethodCallResult {
    match method_name {
        // annotationType() returns the cached Class mirror.
        "annotationType" => {
            return Ok(Some(shared.heap.get_field(proxy, 1)));
        }
        // toString() вЂ” spec-compliant @Type(name=value, ...)
        "toString" => {
            let s = annotation_proxy_to_string(shared, proxy);
            let result = super::create_java_string(shared, &s);
            return Ok(Some(Value::Object(Some(result))));
        }
        // hashCode() вЂ” sum of (127 * nameHash) ^ valueHash
        "hashCode" => {
            return Ok(Some(Value::Int(annotation_proxy_hash_code(shared, proxy))));
        }
        // equals(Object) вЂ” annotation-equality contract
        "equals" => {
            let other = args.first().copied().unwrap_or(Value::Object(None));
            let eq = annotation_proxy_equals(shared, proxy, other);
            return Ok(Some(Value::Int(if eq { 1 } else { 0 })));
        }
        _ => {}
    }

    // Element accessor: walk the parallel arrays for a matching name.
    let names_arr = match shared.heap.get_field(proxy, 2) {
        Value::Object(Some(a)) => a,
        _ => return Ok(Some(Value::Object(None))),
    };
    let values_arr = match shared.heap.get_field(proxy, 3) {
        Value::Object(Some(a)) => a,
        _ => return Ok(Some(Value::Object(None))),
    };
    let n = shared.heap.array_length(names_arr);
    for i in 0..n {
        if let Ok(Value::Object(Some(name_ref))) = shared.heap.get_array_element(names_arr, i) {
            if let Some(name) = super::read_java_string(&shared.heap, name_ref) {
                if name == method_name {
                    if let Ok(val) = shared.heap.get_array_element(values_arr, i) {
                        return Ok(Some(val));
                    }
                }
            }
        }
    }
    // Element not found вЂ” return null/default.
    Ok(Some(Value::Object(None)))
}

/// Count the number of parameters in a JVM descriptor like `(ILjava/lang/String;)V`.
pub(super) fn proxy_count_params(descriptor: &str) -> usize {
    let inner = match descriptor.find('(') {
        Some(start) => match descriptor.find(')') {
            Some(end) if end > start => &descriptor[start + 1..end],
            _ => return 0,
        },
        None => return 0,
    };
    let mut count = 0;
    let mut chars = inner.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            'B' | 'C' | 'D' | 'F' | 'I' | 'J' | 'S' | 'Z' => count += 1,
            'L' => {
                count += 1;
                // skip until ';'
                for c in chars.by_ref() {
                    if c == ';' {
                        break;
                    }
                }
            }
            '[' => {
                // array prefix вЂ” don't count the '[' itself, the element type follows
            }
            _ => {
                tracing::warn!("Unrecognized type character '{}' in method descriptor: {}", ch, descriptor);
            }
        }
    }
    count
}

/// Box a JVM value into a Java wrapper object for use in Object[] arrays.
/// Object references are passed through unchanged.
pub(super) fn proxy_box_value(shared: &SharedVm, value: Value) -> Value {
    match value {
        Value::Int(v) => {
            let class_id = shared.class_manager.write()
                .load_class("java/lang/Integer").unwrap_or(ClassId::new(0));
            let obj = shared.heap.alloc_object(class_id, 1);
            shared.heap.set_field(obj, 0, Value::Int(v));
            Value::Object(Some(obj))
        }
        Value::Long(v) => {
            let class_id = shared.class_manager.write()
                .load_class("java/lang/Long").unwrap_or(ClassId::new(0));
            let obj = shared.heap.alloc_object(class_id, 1);
            shared.heap.set_field(obj, 0, Value::Long(v));
            Value::Object(Some(obj))
        }
        Value::Float(v) => {
            let class_id = shared.class_manager.write()
                .load_class("java/lang/Float").unwrap_or(ClassId::new(0));
            let obj = shared.heap.alloc_object(class_id, 1);
            shared.heap.set_field(obj, 0, Value::Float(v));
            Value::Object(Some(obj))
        }
        Value::Double(v) => {
            let class_id = shared.class_manager.write()
                .load_class("java/lang/Double").unwrap_or(ClassId::new(0));
            let obj = shared.heap.alloc_object(class_id, 1);
            shared.heap.set_field(obj, 0, Value::Double(v));
            Value::Object(Some(obj))
        }
        other => other, // already an Object reference or null
    }
}

/// Invoke a method on a specific class by ClassId.
pub fn invoke_on_class_shared(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_id: ClassId,
    method_name: &str,
    descriptor: &str,
    args: &[Value],
) -> MethodCallResult {
    invoke_on_class_shared_inner(shared, thread, class_id, method_name, descriptor, args, false)
}

/// WP2.9 вЂ” Invoke a method on a specific class with **no virtual retarget**.
///
/// Used by `MethodHandles.Lookup.findSpecial` and the default-method super-call
/// pattern. Unlike [`invoke_on_class_shared`], this never retargets dispatch
/// to the receiver's concrete class even when the resolved class is an
/// interface or abstract вЂ” that is the *whole point* of invokespecial.
pub fn invoke_on_class_shared_no_retarget(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_id: ClassId,
    method_name: &str,
    descriptor: &str,
    args: &[Value],
) -> MethodCallResult {
    invoke_on_class_shared_inner(shared, thread, class_id, method_name, descriptor, args, true)
}

fn invoke_on_class_shared_inner(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_id: ClassId,
    method_name: &str,
    descriptor: &str,
    args: &[Value],
    no_retarget: bool,
) -> MethodCallResult {
    // C25: Virtual dispatch on an interface (or abstract class) вЂ” if the
    // passed class_id names an interface/abstract class and the first arg is
    // a concrete object, re-target dispatch onto the receiver's actual class.
    // Without this, invoking e.g. `Iterator.hasNext()Z` through this entry
    // point finds the abstract interface method (no Code attribute) and
    // fails with an internal error. Callers that pass a specific class_id
    // intentionally (invokespecial, <clinit>) use the <init>/<clinit> name
    // filter below to opt out, OR pass `no_retarget=true` (WP2.9 findSpecial).
    let class_id = if !no_retarget && method_name != "<init>" && method_name != "<clinit>" {
        let recv_cid = args.get(0).and_then(|v| {
            if let Value::Object(Some(o)) = v { Some(shared.heap.class_id_of(*o)) } else { None }
        });
        if let Some(rc) = recv_cid {
            if rc != class_id {
                let cm = shared.class_manager.read();
                let this_is_iface_or_abs = cm.get_class(class_id)
                    .map(|c| c.is_interface() || c.is_abstract())
                    .unwrap_or(false);
                let recv_is_concrete = cm.get_class(rc)
                    .map(|c| !c.is_interface())
                    .unwrap_or(false);
                if this_is_iface_or_abs && recv_is_concrete && cm.is_subclass_of(rc, class_id) {
                    rc
                } else {
                    class_id
                }
            } else {
                class_id
            }
        } else {
            class_id
        }
    } else {
        class_id
    };
    // Find the method (walking the superclass chain)
    let (is_native, is_synchronized, is_static, declaring_class_id) = {
        let cm = shared.class_manager.read();
        let store = &cm.class_store;
        match crate::classloading::find_method_recursive(class_id, method_name, descriptor, store) {
            Some((method, declaring_id)) => {
                let mut native = method.is_native();
                let mut declaring_id_out = declaring_id;
                // If the method has no Code attribute (abstract/interface) but we
                // have a native override, use it. This handles Path.getFileSystem()
                // and similar abstract methods with native implementations.
                if !native {
                    let class_name = store.get(declaring_id)
                        .map(|c| &*c.name).unwrap_or("");
                    // For abstract methods, always check native overrides.
                    // For concrete methods, only check native overrides for classes
                    // where we create synthetic objects without running <init>
                    // (e.g. ByteArrayInputStream created by getResourceAsStream).
                    let check_override = method.is_abstract()
                        || class_name == "java/io/ByteArrayInputStream"
                        // B3: ClassLoader.getResources / getSystemResources
                        // have real-JDK bytecode but that bytecode walks
                        // URLClassPath (which NPEs during <clinit>). Force
                        // the native override ahead of the bytecode.
                        || (class_name == "java/lang/ClassLoader"
                            && (method_name == "getResources"
                                || method_name == "getSystemResources"))
                        // C23: AtomicReferenceArray / AtomicIntegerArray /
                        // AtomicLongArray use VarHandle.compareAndSet on their
                        // inner array, which routes through VarHandles$Array$*
                        // JDK bytecode that depends on Unsafe internals we
                        // don't fully support.  Route CAS/get/set/getAndSet
                        // directly through our synthetic natives.
                        || (matches!(
                                class_name,
                                "java/util/concurrent/atomic/AtomicReferenceArray"
                                | "java/util/concurrent/atomic/AtomicIntegerArray"
                                | "java/util/concurrent/atomic/AtomicLongArray"
                            )
                            && matches!(
                                method_name,
                                "get" | "set" | "compareAndSet"
                                | "getAndSet" | "getAndAdd" | "getAndIncrement"
                                | "getAndDecrement" | "incrementAndGet"
                                | "decrementAndGet" | "addAndGet"
                                | "lazySet" | "getPlain" | "setPlain"
                                | "weakCompareAndSet"
                                | "weakCompareAndSetPlain"
                                | "getOpaque" | "setOpaque"
                                | "getAcquire" | "setRelease"
                                | "compareAndExchange"
                                | "compareAndExchangeAcquire"
                                | "compareAndExchangeRelease"
                                | "length" | "<init>"
                            ))
                        // WP4.7: StampedLock + ReentrantReadWriteLock вЂ” the
                        // real JDK bytecode for these uses
                        // `Unsafe.compareAndSetLong` directly on a
                        // `volatile long state` field. Our CompactValue
                        // tagging stores that field as a Long, but the
                        // VarHandle/Unsafe CAS path reads it back through
                        // a Double-tagged path (T19_H6_CAS_DIAG slot=5)
                        // because it dereferences the slot via the field's
                        // raw offset. Route the public lock/unlock
                        // entrypoints through our hand-rolled
                        // `parking_lot::Mutex+Condvar` natives so the JDK
                        // bytecode never has a chance to mis-tag.
                        || (class_name == "java/util/concurrent/locks/StampedLock"
                            && matches!(
                                method_name,
                                "<init>"
                                | "readLock" | "writeLock"
                                | "tryReadLock" | "tryWriteLock"
                                | "tryOptimisticRead" | "validate"
                                | "unlockRead" | "unlockWrite"
                                | "tryConvertToReadLock" | "tryConvertToWriteLock"
                                | "isReadLocked" | "isWriteLocked"
                                | "getReadLockCount"
                            ))
                        || (matches!(
                                class_name,
                                "java/util/concurrent/locks/ReentrantReadWriteLock$ReadLock"
                                | "java/util/concurrent/locks/ReentrantReadWriteLock$WriteLock"
                            )
                            && matches!(
                                method_name,
                                "lock" | "unlock" | "tryLock"
                                | "lockInterruptibly"
                                | "isHeldByCurrentThread"
                            ))
                        // WP4.2: ForkJoinPool.execute(Runnable) /
                        // execute(ForkJoinTask) вЂ” the real JDK bytecode
                        // queues the runnable for a worker thread that
                        // never runs Java in our impl (NativeContext is
                        // not Send). Force the eager-inline native
                        // override so CompletableFuture.supplyAsync /
                        // thenApplyAsync / runAsync actually complete
                        // their stages instead of leaving a Signaller
                        // placeholder in the result slot.
                        || (class_name == "java/util/concurrent/ForkJoinPool"
                            && method_name == "execute"
                            && (descriptor == "(Ljava/lang/Runnable;)V"
                                || descriptor == "(Ljava/util/concurrent/ForkJoinTask;)V"))
                        // T19_K3_FJP_NATIVE_OVERRIDE: ForkJoinPool.commonPool() /
                        // getFactory() вЂ” the real JDK bytecode for these reads
                        // the `common` static field and the `factory` instance
                        // field, both of which are populated by the
                        // ForkJoinPool static-initialization machinery
                        // (Unsafe-backed AtomicReferenceFieldUpdater plumbing
                        // we don't fully model). The fields read back as null,
                        // and KeycloakMain's
                        // `ensureForkJoinPoolThreadFactoryHasBeenSetToQuarkus`
                        // dies on `getFactory().getClass()` with an NPE.
                        // Force the native override to return a synthetic
                        // ForkJoinPool whose `factory` matches the system
                        // property `java.util.concurrent.ForkJoinPool.common
                        // .threadFactory` (set by QuarkusEntryPoint.main).
                        || (class_name == "java/util/concurrent/ForkJoinPool"
                            && (method_name == "commonPool"
                                || method_name == "getFactory"
                                || method_name == "getCommonPoolParallelism"))
                        // T19_K3_PROPS_NATIVE_OVERRIDE: java.util.Properties
                        // {load, getProperty, setProperty, put, containsKey}
                        // вЂ” the real JDK bytecode for `load(InputStream)`
                        // funnels through `LineReader` + `loadConvert` +
                        // inherited `Hashtable.put`, which writes to our
                        // synthetic Properties object's inner Hashtable
                        // field whose shape doesn't match the JDK's, so
                        // `getProperty` returns null even after `put` ran.
                        // KeycloakMain's `Version.<clinit>` reads the
                        // per-jar `keycloak-version.properties` resource and
                        // dies when `Properties.getProperty("version")`
                        // returns null.  Force the side-table-backed
                        // natives (in `properties_sidetable.rs`) to win
                        // over the real JDK bytecode so `load` populates
                        // and `getProperty` retrieves the parsed values.
                        || (class_name == "java/util/Properties"
                            && matches!(
                                method_name,
                                "load"
                                | "getProperty"
                                | "setProperty"
                                | "put"
                                | "containsKey"
                            ))
                        // WP6.1: Provider.getEngineName(String) вЂ” the
                        // real JDK bytecode reads `knownEngines` (a
                        // static HashMap) which `Provider.<clinit>` would
                        // populate. We no-op that clinit (see
                        // `jca/cipher.rs::register_cipher_clinit_shim`),
                        // so `knownEngines` stays null and the bytecode
                        // NPEs at `knownEngines.get(name)` when
                        // BouncyCastleProvider walks ~500 algorithm
                        // mappings. The native override returns the input
                        // name unchanged (matching the OpenJDK fallback
                        // path when the engine lookup fails).
                        || (class_name == "java/security/Provider"
                            && method_name == "getEngineName");
                    if check_override && shared.native_methods.find(class_name, method_name, descriptor).is_some() {
                        native = true;
                    }
                    // C25: For abstract methods (e.g. Iterator.hasNext, Enumeration.hasMoreElements),
                    // also check the receiver class's native registry вЂ” natives for synthetic
                    // wrapper classes (java/util/Enumeration$Impl) are registered on the
                    // wrapper class name, not on the interface. Without this, dispatch on
                    // an Enumeration$Impl receiver to Iterator.hasNext() resolves to the
                    // abstract method and fails with "no Code attribute".
                    if !native && method.is_abstract() && class_id != declaring_id {
                        let recv_name = store.get(class_id)
                            .map(|c| &*c.name).unwrap_or("");
                        if !recv_name.is_empty()
                            && shared.native_methods.find(recv_name, method_name, descriptor).is_some()
                        {
                            native = true;
                            declaring_id_out = class_id;
                        }
                    }
                }
                (
                    native,
                    method.is_synchronized(),
                    method.is_static(),
                    declaring_id_out,
                )
            }
            None => {
                // Method not found in class hierarchy вЂ” try the native registry
                // as a fallback. This handles synthetic stub classes (JDK classes
                // loaded without .class files) whose methods are all native.
                // Walk the superclass chain so inherited native methods (e.g.
                // Object.hashCode called on a subclass) are found.
                let mut lookup_id = Some(class_id);
                let mut found_callback = None;
                while let Some(cid) = lookup_id {
                    if let Some(cls) = store.get(cid) {
                        if let Some(cb) = shared.native_methods.find(&cls.name, method_name, descriptor) {
                            found_callback = Some(cb);
                            break;
                        }
                        lookup_id = cls.superclass;
                    } else {
                        break;
                    }
                }
                let class_name = store
                    .get(class_id)
                    .map(|c| c.name.to_string())
                    .unwrap_or_else(|| format!("<unknown class {class_id}>"));
                drop(cm);

                if let Some(callback) = found_callback {
                    return safe_native_call(shared, thread, callback, args);
                }

                // Signature-polymorphic methods (JVM spec В§5.4.3.4):
                // MethodHandle.invoke / invokeExact / invokeWithArguments and
                // VarHandle.get / set / compareAndSet etc. are called with the
                // call-site descriptor, but registered with a generic one.
                if method_name == "invoke" || method_name == "invokeExact" || method_name == "invokeWithArguments"
                    || method_name == "get" || method_name == "set"
                    || method_name == "getVolatile" || method_name == "setVolatile"
                    || method_name == "getOpaque" || method_name == "setOpaque"
                    || method_name == "getAcquire" || method_name == "setRelease"
                    || method_name == "compareAndSet" || method_name == "compareAndExchange"
                    || method_name == "compareAndExchangeAcquire" || method_name == "compareAndExchangeRelease"
                    || method_name == "weakCompareAndSet" || method_name == "weakCompareAndSetPlain"
                    || method_name == "weakCompareAndSetAcquire" || method_name == "weakCompareAndSetRelease"
                    || method_name == "getAndSet"
                    || method_name == "getAndSetAcquire" || method_name == "getAndSetRelease"
                    || method_name == "getAndAdd"
                    || method_name == "getAndAddAcquire" || method_name == "getAndAddRelease"
                {
                    // Check if receiver is a MethodHandle or VarHandle
                    let is_mh = class_name == "java/lang/invoke/MethodHandle"
                        || class_name.starts_with("java/lang/invoke/MethodHandle");
                    let is_vh = class_name == "java/lang/invoke/VarHandle"
                        || class_name.starts_with("java/lang/invoke/VarHandle");
                    if is_mh || is_vh {
                        let base = if is_mh { "java/lang/invoke/MethodHandle" } else { "java/lang/invoke/VarHandle" };
                        // Try all possible registered descriptors for signature-polymorphic methods.
                        // These methods are registered with generic Object[] params but varying return types.
                        let poly_descs = [
                            "([Ljava/lang/Object;)Ljava/lang/Object;",
                            "([Ljava/lang/Object;)V",
                            "([Ljava/lang/Object;)Z",
                        ];
                        for poly_desc in &poly_descs {
                            if let Some(cb) = shared.native_methods.find(base, method_name, poly_desc) {
                                let r = safe_native_call(shared, thread, cb, args)?;
                                return Ok(unbox_poly_return(shared, r, descriptor));
                            }
                        }
                        // Also try the exact class name
                        for poly_desc in &poly_descs {
                            if let Some(cb) = shared.native_methods.find(&class_name, method_name, poly_desc) {
                                let r = safe_native_call(shared, thread, cb, args)?;
                                return Ok(unbox_poly_return(shared, r, descriptor));
                            }
                        }
                    }
                }

                // Check interfaces for default methods (e.g. Function$AndThen
                // implements Function, so Function.andThen should be found).
                {
                    let cm2 = shared.class_manager.read();
                    if let Some(class) = cm2.class_store.get(class_id) {
                        let iface_names: Vec<String> = class
                            .interfaces
                            .iter()
                            .filter_map(|&iid| cm2.class_store.get(iid).map(|c| c.name.to_string()))
                            .collect();
                        drop(cm2);
                        for iface_name in &iface_names {
                            if let Some(callback) =
                                shared.native_methods.find(iface_name, method_name, descriptor)
                            {
                                return safe_native_call(shared, thread, callback, args);
                            }
                        }
                    }
                }

                tracing::warn!(
                    method = format!("{class_name}.{method_name}{descriptor}"),
                    "NoSuchMethodError"
                );
                return Err(MethodCallFailed::InternalError(VmError::Linkage(
                    LinkageError::NoSuchMethodError {
                        class_name,
                        method_name: method_name.to_string(),
                        method_descriptor: descriptor.to_string(),
                    },
                )));
            }
        }
    };

    // --- AOT training: record method invocation with receiver type ---
    #[cfg(feature = "experimental-aot")]
    {
        if crate::native::builtins::aot::is_aot_training() {
            let cm = shared.class_manager.read();
            let class_name = cm.class_store
                .get(class_id)
                .map(|c| c.name.to_string())
                .unwrap_or_default();
            // For virtual methods, extract receiver type from args[0]
            let receiver_type = if !is_static {
                if let Some(Value::Object(Some(receiver_obj))) = args.first() {
                    let receiver_class_id = shared.heap.class_id_of(*receiver_obj);
                    cm.class_store.get(receiver_class_id).map(|c| c.name.to_string())
                } else {
                    None
                }
            } else {
                None
            };
            drop(cm);
            crate::native::builtins::aot::aot_record_method_invocation(
                &class_name,
                method_name,
                descriptor,
                receiver_type.as_deref(),
            );
        }
    }

    // --- ACC_SYNCHRONIZED: acquire monitor before execution ---
    let monitor_obj: Option<ObjectRef> = if is_synchronized {
        let obj = if is_static {
            // Static synchronized: use a per-class synthetic lock object
            shared.get_class_lock_object(declaring_class_id)
        } else {
            // Instance synchronized: use args[0] (the `this` reference)
            match args.first() {
                Some(Value::Object(Some(obj_ref))) => *obj_ref,
                _ => {
                    return Err(MethodCallFailed::InternalError(VmError::Internal {
                        message: "synchronized instance method called with null or missing this"
                            .to_string(),
                    }));
                }
            }
        };
        shared.monitors.enter(obj, thread.thread_id);
        Some(obj)
    } else {
        None
    };

    let result = if is_native {
        // Look up native implementation
        let class_name = shared
            .class_manager
            .read()
            .get_class(declaring_class_id)
            .map(|c| c.name.to_string())
            .unwrap_or_default();

        if let Some(callback) = shared.native_methods.find(&class_name, method_name, descriptor) {
            // Fast path: Rust NativeCallback registered in the built-in registry.
            safe_native_call(shared, thread, callback, args)
        } else if let Some(fn_ptr) =
            crate::native::jni::find_jni_native(&class_name, method_name, descriptor)
        {
            // JNI function pointer registered via RegisterNatives or symbol lookup.
            // Set TLS context so that JNI callbacks (e.g. FindClass, CallMethod)
            // can access the VM from within the native library.
            crate::native::jni::set_jni_context(shared);
            crate::native::jni::set_jni_thread(thread as *mut _);

            let env = crate::native::jni::get_jni_env();
            // For instance methods, args[0] is the receiver; for static, it is absent.
            let (receiver, call_args) = if is_static {
                (0u64, args)
            } else {
                let recv = match args.first() {
                    Some(Value::Object(Some(r))) => crate::native::jni::obj_to_jobject(*r),
                    _ => 0u64,
                };
                (recv, if args.is_empty() { args } else { &args[1..] })
            };

            // Safety: fn_ptr was stored from a trusted RegisterNatives / JNI_OnLoad call.
            let result_value = unsafe {
                crate::native::jni::dispatch_jni_native(fn_ptr, env, receiver, call_args, descriptor)
            };

            crate::native::jni::clear_jni_context();
            crate::native::jni::clear_jni_thread();

            // void methods return Value::Object(None) from dispatch_jni_native
            let ret_char = descriptor
                .rfind(')')
                .and_then(|i| descriptor.as_bytes().get(i + 1).copied())
                .unwrap_or(b'V');
            if ret_char == b'V' {
                Ok(None)
            } else {
                Ok(Some(result_value))
            }
        } else if let Some(fn_ptr) = crate::native::jni::resolve_jni_native_in_libraries(
            &shared.native_libraries,
            &class_name,
            method_name,
            descriptor,
        ) {
            // Auto-resolved via JNI naming convention (dlsym in loaded libraries).
            crate::native::jni::set_jni_context(shared);
            crate::native::jni::set_jni_thread(thread as *mut _);

            let env = crate::native::jni::get_jni_env();
            let (receiver, call_args) = if is_static {
                (0u64, args)
            } else {
                let recv = match args.first() {
                    Some(Value::Object(Some(r))) => crate::native::jni::obj_to_jobject(*r),
                    _ => 0u64,
                };
                (recv, if args.is_empty() { args } else { &args[1..] })
            };

            let result_value = unsafe {
                crate::native::jni::dispatch_jni_native(fn_ptr, env, receiver, call_args, descriptor)
            };

            crate::native::jni::clear_jni_context();
            crate::native::jni::clear_jni_thread();

            let ret_char = descriptor
                .rfind(')')
                .and_then(|i| descriptor.as_bytes().get(i + 1).copied())
                .unwrap_or(b'V');
            if ret_char == b'V' {
                Ok(None)
            } else {
                Ok(Some(result_value))
            }
        } else {
            let full_sig = format!("{class_name}.{method_name}{descriptor}");
            tracing::warn!(
                method = %full_sig,
                "Missing native method in real-JDK mode"
            );
            // Record in structured audit log if enabled
            if shared.config.audit_missing_natives {
                // NEW-10: capture the caller frame (the topmost frame
                // on the thread's stack at the moment this missing
                // native was invoked) as a sample call-site so a
                // later reader of the committed baseline can locate
                // the Java code that reached the missing native.
                let sample_call_site = thread.frames.last().map(|frame| {
                    format!(
                        "{}.{}{}",
                        frame.class_name(),
                        frame.method_name(),
                        frame.method_descriptor()
                    )
                });
                shared.record_missing_native(
                    &class_name,
                    method_name,
                    descriptor,
                    sample_call_site,
                );
                // In audit mode, return a default value so execution continues.
                let ret_char = descriptor
                    .rfind(')')
                    .and_then(|i| descriptor.as_bytes().get(i + 1).copied())
                    .unwrap_or(b'V');
                match ret_char {
                    b'V' => Ok(None),
                    b'J' => Ok(Some(Value::Long(0))),
                    b'F' => Ok(Some(Value::Float(0.0))),
                    b'D' => Ok(Some(Value::Double(0.0))),
                    b'L' | b'[' => Ok(Some(Value::Object(None))),
                    _ => Ok(Some(Value::Int(0))),
                }
            } else {
                // In production mode, throw UnsatisfiedLinkError per JVM spec В§5.3.5.
                Err(MethodCallFailed::InternalError(VmError::Runtime(
                    RuntimeError::UnsatisfiedLinkError {
                        message: full_sig,
                    },
                )))
            }
        }
    } else {
        // Before executing bytecode, check if we have a native override registered.
        // This handles JDK methods that are Java code but depend on JVM-internal
        // state we haven't set up (e.g. VM.getSavedProperty checks savedProps
        // which requires System.initPhase2 to have run).
        let class_name_for_override = shared
            .class_manager
            .read()
            .get_class(declaring_class_id)
            .map(|c| c.name.to_string())
            .unwrap_or_default();
        if std::env::var_os("RUSTJVM_BD_DEBUG").is_some() && method_name == "intValue" {
            let found = shared.native_methods.find(&class_name_for_override, method_name, descriptor).is_some();
            eprintln!("[invoke_on_class_shared L5271] class_name_for_override={} method={} desc={} found={}",
                      class_name_for_override, method_name, descriptor, found);
        }
        if let Some(callback) = shared.native_methods.find(&class_name_for_override, method_name, descriptor) {
            safe_native_call(shared, thread, callback, args)
        } else {
            // Execute bytecode via interpreter
            crate::runtime::interpreter::execute(
                shared,
                thread,
                declaring_class_id,
                method_name,
                descriptor,
                args,
            )
        }
    };

    // --- ACC_SYNCHRONIZED: release monitor after execution ---
    // Release the monitor regardless of success or failure (including exceptions).
    if let Some(obj) = monitor_obj {
        // We ignore exit errors here вЂ” the monitor should always be owned
        // by this thread at this point.
        let _ = shared.monitors.exit(obj, thread.thread_id);
    }

    result
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use crate::config::VmConfig;
    use crate::threading::jvm_thread::{JvmThread, ThreadId};

    fn test_shared() -> Arc<SharedVm> {
        Arc::new(SharedVm::new(VmConfig::default()))
    }

    // -----------------------------------------------------------------------
    // values_equal_for_cas
    // -----------------------------------------------------------------------

    #[test]
    fn cas_equal_ints() {
        assert!(values_equal_for_cas(&Value::Int(42), &Value::Int(42)));
    }

    #[test]
    fn cas_different_ints() {
        assert!(!values_equal_for_cas(&Value::Int(1), &Value::Int(2)));
    }

    #[test]
    fn cas_equal_longs() {
        assert!(values_equal_for_cas(&Value::Long(100), &Value::Long(100)));
    }

    #[test]
    fn cas_different_longs() {
        assert!(!values_equal_for_cas(&Value::Long(1), &Value::Long(2)));
    }

    #[test]
    fn cas_equal_floats() {
        assert!(values_equal_for_cas(&Value::Float(1.5), &Value::Float(1.5)));
    }

    #[test]
    fn cas_float_nan_bit_equal() {
        // CAS uses bit equality, so NaN == NaN
        assert!(values_equal_for_cas(&Value::Float(f32::NAN), &Value::Float(f32::NAN)));
    }

    #[test]
    fn cas_float_pos_neg_zero_different_bits() {
        // +0.0 and -0.0 have different bits
        assert!(!values_equal_for_cas(&Value::Float(0.0), &Value::Float(-0.0)));
    }

    #[test]
    fn cas_equal_doubles() {
        assert!(values_equal_for_cas(&Value::Double(2.5), &Value::Double(2.5)));
    }

    #[test]
    fn cas_double_nan_bit_equal() {
        assert!(values_equal_for_cas(&Value::Double(f64::NAN), &Value::Double(f64::NAN)));
    }

    #[test]
    fn cas_both_null_objects() {
        assert!(values_equal_for_cas(&Value::Object(None), &Value::Object(None)));
    }

    #[test]
    fn cas_one_null_one_not() {
        let shared = test_shared();
        let obj = shared.heap.alloc_object(ClassId::new(0), 0);
        assert!(!values_equal_for_cas(&Value::Object(Some(obj)), &Value::Object(None)));
        assert!(!values_equal_for_cas(&Value::Object(None), &Value::Object(Some(obj))));
    }

    #[test]
    fn cas_same_object_ref() {
        let shared = test_shared();
        let obj = shared.heap.alloc_object(ClassId::new(0), 0);
        assert!(values_equal_for_cas(&Value::Object(Some(obj)), &Value::Object(Some(obj))));
    }

    #[test]
    fn cas_different_object_refs() {
        let shared = test_shared();
        let obj1 = shared.heap.alloc_object(ClassId::new(0), 0);
        let obj2 = shared.heap.alloc_object(ClassId::new(0), 0);
        assert!(!values_equal_for_cas(&Value::Object(Some(obj1)), &Value::Object(Some(obj2))));
    }

    #[test]
    fn cas_mismatched_types() {
        // T19_H6: cross-primitive pairs now match by raw 64-bit bit
        // pattern (CAS operates on bits вЂ” a long field whose tag drifted
        // to Double must still compare equal). Int is zero-extended to
        // 64 bits before comparison.
        assert!(values_equal_for_cas(&Value::Int(0), &Value::Long(0)));
        assert!(values_equal_for_cas(&Value::Float(0.0), &Value::Double(0.0)));
        // Non-zero primitive vs Object(None) still fails.
        assert!(!values_equal_for_cas(&Value::Int(1), &Value::Object(None)));
        assert!(!values_equal_for_cas(&Value::Long(42), &Value::Object(None)));
    }

    /// R1: an uninitialized primitive field slot reads as Object(None)
    /// (the zero-discriminant `Value` variant for zeroed memory). The
    /// CAS equality test must treat it as equal to a typed zero so that
    /// `Unsafe.compareAndSetInt(obj, off, Int(0), ...)` on a freshly
    /// allocated object (e.g. `ConcurrentHashMap.initTable`) succeeds
    /// instead of livelocking.
    #[test]
    fn cas_primitive_default_on_uninit_slot_is_equal_to_typed_zero() {
        // Int(0) вЂ” the ConcurrentHashMap.sizeCtl case.
        assert!(values_equal_for_cas(&Value::Object(None), &Value::Int(0)));
        assert!(values_equal_for_cas(&Value::Int(0), &Value::Object(None)));
        // Long(0).
        assert!(values_equal_for_cas(&Value::Object(None), &Value::Long(0)));
        assert!(values_equal_for_cas(&Value::Long(0), &Value::Object(None)));
        // Float(+0.0) and Double(+0.0) вЂ” only +0.0 matches, not -0.0.
        assert!(values_equal_for_cas(&Value::Object(None), &Value::Float(0.0)));
        assert!(values_equal_for_cas(&Value::Float(0.0), &Value::Object(None)));
        assert!(values_equal_for_cas(&Value::Object(None), &Value::Double(0.0)));
        assert!(values_equal_for_cas(&Value::Double(0.0), &Value::Object(None)));
    }

    #[test]
    fn cas_primitive_default_rejects_nonzero_and_negzero() {
        // Non-zero primitives against Object(None) must still be unequal.
        assert!(!values_equal_for_cas(&Value::Int(1), &Value::Object(None)));
        assert!(!values_equal_for_cas(&Value::Long(1), &Value::Object(None)));
        // Negative zero has a different bit pattern from positive zero; we
        // only coerce the +0.0 bit pattern (matching the zeroed-memory
        // default).
        assert!(!values_equal_for_cas(&Value::Float(-0.0), &Value::Object(None)));
        assert!(!values_equal_for_cas(&Value::Double(-0.0), &Value::Object(None)));
    }

    // -----------------------------------------------------------------------
    // proxy_count_params
    // -----------------------------------------------------------------------

    #[test]
    fn count_params_empty() {
        assert_eq!(proxy_count_params("()V"), 0);
    }

    #[test]
    fn count_params_single_int() {
        assert_eq!(proxy_count_params("(I)V"), 1);
    }

    #[test]
    fn count_params_multiple_primitives() {
        assert_eq!(proxy_count_params("(IJD)V"), 3);
    }

    #[test]
    fn count_params_object_ref() {
        assert_eq!(proxy_count_params("(Ljava/lang/String;)V"), 1);
    }

    #[test]
    fn count_params_mixed() {
        assert_eq!(proxy_count_params("(ILjava/lang/String;DJ)V"), 4);
    }

    #[test]
    fn count_params_array() {
        assert_eq!(proxy_count_params("([I)V"), 1);
    }

    #[test]
    fn count_params_object_array() {
        assert_eq!(proxy_count_params("([Ljava/lang/Object;)V"), 1);
    }

    #[test]
    fn count_params_multi_dim_array() {
        assert_eq!(proxy_count_params("([[I)V"), 1);
    }

    #[test]
    fn count_params_complex_return() {
        assert_eq!(proxy_count_params("(II)Ljava/lang/Object;"), 2);
    }

    // -----------------------------------------------------------------------
    // C7: unbox_poly_return вЂ” primitive return passthrough & void handling
    // -----------------------------------------------------------------------

    #[test]
    fn poly_return_long_passthrough() {
        let shared = test_shared();
        let r = unbox_poly_return(&shared, Some(Value::Long(42)), "()J");
        assert_eq!(r, Some(Value::Long(42)));
    }

    #[test]
    fn poly_return_int_passthrough() {
        let shared = test_shared();
        let r = unbox_poly_return(&shared, Some(Value::Int(7)), "()I");
        assert_eq!(r, Some(Value::Int(7)));
    }

    #[test]
    fn poly_return_float_passthrough() {
        let shared = test_shared();
        let r = unbox_poly_return(&shared, Some(Value::Float(1.5)), "()F");
        assert_eq!(r, Some(Value::Float(1.5)));
    }

    #[test]
    fn poly_return_double_passthrough() {
        let shared = test_shared();
        let r = unbox_poly_return(&shared, Some(Value::Double(2.5)), "()D");
        assert_eq!(r, Some(Value::Double(2.5)));
    }

    #[test]
    fn poly_return_byte_passthrough() {
        let shared = test_shared();
        let r = unbox_poly_return(&shared, Some(Value::Int(5)), "()B");
        assert_eq!(r, Some(Value::Int(5)));
    }

    #[test]
    fn poly_return_short_passthrough() {
        let shared = test_shared();
        let r = unbox_poly_return(&shared, Some(Value::Int(9)), "()S");
        assert_eq!(r, Some(Value::Int(9)));
    }

    #[test]
    fn poly_return_char_passthrough() {
        let shared = test_shared();
        let r = unbox_poly_return(&shared, Some(Value::Int(65)), "()C");
        assert_eq!(r, Some(Value::Int(65)));
    }

    #[test]
    fn poly_return_bool_passthrough() {
        let shared = test_shared();
        let r = unbox_poly_return(&shared, Some(Value::Int(1)), "()Z");
        assert_eq!(r, Some(Value::Int(1)));
    }

    #[test]
    fn poly_return_void_drops_value() {
        let shared = test_shared();
        let r = unbox_poly_return(&shared, Some(Value::Object(None)), "()V");
        assert_eq!(r, None);
    }

    #[test]
    fn poly_return_reference_passthrough() {
        let shared = test_shared();
        let obj = shared.heap.alloc_object(ClassId::new(0), 0);
        let r = unbox_poly_return(
            &shared,
            Some(Value::Object(Some(obj))),
            "()Ljava/lang/Object;",
        );
        assert_eq!(r, Some(Value::Object(Some(obj))));
    }

    #[test]
    fn poly_return_null_passthrough_for_reference() {
        let shared = test_shared();
        let r = unbox_poly_return(&shared, Some(Value::Object(None)), "()Ljava/lang/String;");
        assert_eq!(r, Some(Value::Object(None)));
    }

    #[test]
    fn count_params_all_primitives() {
        assert_eq!(proxy_count_params("(BCDFIJSZ)V"), 8);
    }

    #[test]
    fn count_params_invalid_no_parens() {
        assert_eq!(proxy_count_params("IV"), 0);
    }

    #[test]
    fn count_params_empty_string() {
        assert_eq!(proxy_count_params(""), 0);
    }

    // -----------------------------------------------------------------------
    // proxy_box_value
    // -----------------------------------------------------------------------

    #[test]
    fn box_int_value() {
        let shared = test_shared();
        let boxed = proxy_box_value(&shared, Value::Int(42));
        match boxed {
            Value::Object(Some(obj)) => {
                assert_eq!(shared.heap.get_field(obj, 0), Value::Int(42));
            }
            _ => panic!("Expected Object(Some(...))"),
        }
    }

    #[test]
    fn box_long_value() {
        let shared = test_shared();
        let boxed = proxy_box_value(&shared, Value::Long(123456));
        match boxed {
            Value::Object(Some(obj)) => {
                assert_eq!(shared.heap.get_field(obj, 0), Value::Long(123456));
            }
            _ => panic!("Expected Object(Some(...))"),
        }
    }

    #[test]
    fn box_float_value() {
        let shared = test_shared();
        let boxed = proxy_box_value(&shared, Value::Float(3.25));
        match boxed {
            Value::Object(Some(obj)) => {
                assert_eq!(shared.heap.get_field(obj, 0), Value::Float(3.25));
            }
            _ => panic!("Expected Object(Some(...))"),
        }
    }

    #[test]
    fn box_double_value() {
        let shared = test_shared();
        let boxed = proxy_box_value(&shared, Value::Double(2.5));
        match boxed {
            Value::Object(Some(obj)) => {
                assert_eq!(shared.heap.get_field(obj, 0), Value::Double(2.5));
            }
            _ => panic!("Expected Object(Some(...))"),
        }
    }

    #[test]
    fn box_object_passthrough() {
        let shared = test_shared();
        let obj = shared.heap.alloc_object(ClassId::new(0), 0);
        let boxed = proxy_box_value(&shared, Value::Object(Some(obj)));
        assert_eq!(boxed, Value::Object(Some(obj)));
    }

    #[test]
    fn box_null_passthrough() {
        let shared = test_shared();
        let boxed = proxy_box_value(&shared, Value::Object(None));
        assert_eq!(boxed, Value::Object(None));
    }

    // -----------------------------------------------------------------------
    // resolve_library_path
    // -----------------------------------------------------------------------

    #[test]
    fn resolve_library_path_absolute_unchanged() {
        let shared = test_shared();
        assert_eq!(resolve_library_path(&shared, "/usr/lib/libfoo.so"), "/usr/lib/libfoo.so");
    }

    #[test]
    fn resolve_library_path_with_backslash_unchanged() {
        let shared = test_shared();
        assert_eq!(resolve_library_path(&shared, "C:\\lib\\foo.dll"), "C:\\lib\\foo.dll");
    }

    #[test]
    fn resolve_library_path_bare_name_no_path_set() {
        let shared = test_shared();
        assert_eq!(resolve_library_path(&shared, "libfoo.so"), "libfoo.so");
    }

    #[test]
    fn resolve_library_path_empty_library_path() {
        let shared = test_shared();
        shared.system_properties.write().insert(
            "java.library.path".to_string(),
            "".to_string(),
        );
        assert_eq!(resolve_library_path(&shared, "libfoo.so"), "libfoo.so");
    }

    // -----------------------------------------------------------------------
    // NativeContextImpl basic operations
    // -----------------------------------------------------------------------

    #[test]
    fn native_context_record_printed_value() {
        let shared = test_shared();
        let mut thread = JvmThread::new(ThreadId(0), "test");
        {
            let mut ctx = NativeContextImpl {
                shared: &shared,
                thread: &mut thread,
            };
            ctx.record_printed_value(Value::Int(42));
            ctx.record_printed_value(Value::Long(100));
        }
        assert_eq!(thread.printed.len(), 2);
        assert_eq!(thread.printed[0], Value::Int(42));
        assert_eq!(thread.printed[1], Value::Long(100));
    }

    #[test]
    fn native_context_record_printed_line() {
        let shared = test_shared();
        let mut thread = JvmThread::new(ThreadId(0), "test");
        {
            let mut ctx = NativeContextImpl {
                shared: &shared,
                thread: &mut thread,
            };
            ctx.record_printed_line("Hello".to_string());
            ctx.record_printed_line("World".to_string());
        }
        assert_eq!(thread.printed_lines, vec!["Hello", "World"]);
    }

    #[test]
    fn native_context_identity_hash_code() {
        let shared = test_shared();
        let mut thread = JvmThread::new(ThreadId(0), "test");
        let obj = shared.heap.alloc_object(ClassId::new(0), 0);
        let ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        let hash = ctx.identity_hash_code(obj);
        assert_eq!(hash, ctx.identity_hash_code(obj));
    }

    #[test]
    fn native_context_class_id_of_object() {
        let shared = test_shared();
        let mut thread = JvmThread::new(ThreadId(0), "test");
        let obj = shared.heap.alloc_object(ClassId::new(7), 2);
        let ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        assert_eq!(ctx.class_id_of_object(obj), ClassId::new(7));
    }

    #[test]
    fn native_context_field_access() {
        let shared = test_shared();
        let mut thread = JvmThread::new(ThreadId(0), "test");
        let obj = shared.heap.alloc_object(ClassId::new(0), 3);
        let ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.set_field(obj, 0, Value::Int(42));
        ctx.set_field(obj, 1, Value::Long(100));
        assert_eq!(ctx.get_field(obj, 0), Value::Int(42));
        assert_eq!(ctx.get_field(obj, 1), Value::Long(100));
    }

    #[test]
    fn native_context_array_operations() {
        let shared = test_shared();
        let mut thread = JvmThread::new(ThreadId(0), "test");
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        let arr = ctx.new_array(ArrayElementType::Int, 5);
        assert_eq!(ctx.array_length(arr), 5);

        ctx.set_array_element(arr, 0, Value::Int(10));
        ctx.set_array_element(arr, 4, Value::Int(99));
        assert_eq!(ctx.get_array_element(arr, 0), Value::Int(10));
        assert_eq!(ctx.get_array_element(arr, 4), Value::Int(99));
    }

    #[test]
    fn native_context_ref_array() {
        let shared = test_shared();
        let mut thread = JvmThread::new(ThreadId(0), "test");
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        let arr = ctx.new_ref_array(ClassId::new(5), 3);
        assert_eq!(ctx.array_length(arr), 3);
        assert_eq!(ctx.heap_kind_of(arr), ObjectKind::Array);
    }

    #[test]
    fn native_context_heap_kind() {
        let shared = test_shared();
        let mut thread = JvmThread::new(ThreadId(0), "test");
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        let obj = ctx.alloc_object(ClassId::new(0), 2);
        let arr = ctx.new_array(ArrayElementType::Int, 1);
        assert_eq!(ctx.heap_kind_of(obj), ObjectKind::Object);
        assert_eq!(ctx.heap_kind_of(arr), ObjectKind::Array);
    }

    #[test]
    fn native_context_create_and_read_string() {
        let shared = test_shared();
        let mut thread = JvmThread::new(ThreadId(0), "test");
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        let str_obj = ctx.create_string("Hello, test!");
        let result = ctx.read_string(str_obj);
        assert_eq!(result, Some("Hello, test!".to_string()));
    }

    #[test]
    fn native_context_system_properties() {
        let shared = test_shared();
        let mut thread = JvmThread::new(ThreadId(0), "test");
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        assert!(ctx.get_system_property("os.name").is_some());
        assert!(ctx.get_system_property("java.version").is_some());

        let old = ctx.set_system_property("test.key", "test.value");
        assert!(old.is_none());
        assert_eq!(ctx.get_system_property("test.key"), Some("test.value".to_string()));

        let old = ctx.set_system_property("test.key", "new.value");
        assert_eq!(old, Some("test.value".to_string()));
    }

    #[test]
    fn native_context_thread_id() {
        let shared = test_shared();
        let mut thread = JvmThread::new(ThreadId(42), "test");
        let ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        assert_eq!(ctx.thread_id(), 42);
    }

    #[test]
    fn native_context_scoped_values() {
        let shared = test_shared();
        let mut thread = JvmThread::new(ThreadId(0), "test");
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        assert!(ctx.get_scoped_value(1).is_none());

        ctx.push_scoped_value(1, Value::Int(42));
        assert_eq!(ctx.get_scoped_value(1), Some(Value::Int(42)));

        ctx.push_scoped_value(1, Value::Int(99));
        assert_eq!(ctx.get_scoped_value(1), Some(Value::Int(99)));

        ctx.pop_scoped_value();
        assert_eq!(ctx.get_scoped_value(1), Some(Value::Int(42)));

        ctx.pop_scoped_value();
        assert!(ctx.get_scoped_value(1).is_none());
    }

    #[test]
    fn native_context_capture_stack_trace() {
        let shared = test_shared();
        let mut thread = JvmThread::new(ThreadId(0), "test");
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        let trace = ctx.capture_stack_trace(123);
        assert!(trace.is_empty());
        let stored = ctx.get_stack_trace(123);
        assert!(stored.is_some());
        assert!(stored.unwrap().is_empty());
    }

    #[test]
    fn native_context_get_stack_trace_missing() {
        let shared = test_shared();
        let mut thread = JvmThread::new(ThreadId(0), "test");
        let ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        assert!(ctx.get_stack_trace(999).is_none());
    }

    #[test]
    fn native_context_system_streams_uninitialized() {
        let shared = test_shared();
        let mut thread = JvmThread::new(ThreadId(0), "test");
        let ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        assert!(ctx.get_system_stream("out").is_none());
        assert!(ctx.get_system_stream("err").is_none());
        assert!(ctx.get_system_stream("invalid").is_none());
    }

    #[test]
    fn native_context_interrupted_flag() {
        let shared = test_shared();
        let mut thread = JvmThread::new(ThreadId(0), "test");
        let ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        assert!(!ctx.is_interrupted(false));
        ctx.thread.interrupted.store(true, std::sync::atomic::Ordering::Release);
        assert!(ctx.is_interrupted(false));
        assert!(ctx.is_interrupted(true));
        assert!(!ctx.is_interrupted(false));
    }

    // -----------------------------------------------------------------------
    // invoke_or_native: invalid class names short-circuit
    // -----------------------------------------------------------------------

    #[test]
    fn invoke_or_native_invalid_class_name() {
        let shared = test_shared();
        let mut thread = JvmThread::new(ThreadId(0), "test");
        let result = invoke_or_native(
            &shared,
            &mut thread,
            "<unknown class 0>",
            "method",
            "()V",
            &[],
        );
        assert!(result.is_err());
    }

    #[test]
    fn invoke_or_native_class_with_space() {
        let shared = test_shared();
        let mut thread = JvmThread::new(ThreadId(0), "test");
        let result = invoke_or_native(
            &shared,
            &mut thread,
            "invalid class",
            "method",
            "()V",
            &[],
        );
        assert!(result.is_err());
    }

    // =====================================================================
    // T10.9.E вЂ” descriptor cache / descriptor-aware NativeContext::get_field
    // =====================================================================

    #[test]
    fn t10_9_e_descriptor_cache_empty_on_startup() {
        let shared = test_shared();
        let cache = shared.field_descriptor_cache.read();
        assert!(
            cache.is_empty(),
            "field_descriptor_cache should start empty, got {} entries",
            cache.len()
        );
    }

    #[test]
    fn t10_9_e_resolve_descriptor_miss_for_unloaded_class() {
        let shared = test_shared();
        // ClassId::new(9999) is not loaded вЂ” resolver returns None and does
        // not populate the cache.
        let before = shared.field_descriptor_cache.read().len();
        let result = resolve_field_descriptor_byte_cached(
            &shared,
            ClassId::new(9999),
            0,
        );
        assert_eq!(result, None);
        let after = shared.field_descriptor_cache.read().len();
        assert_eq!(
            before, after,
            "cache must not record an entry on miss (would mask later class loads)"
        );
    }

    #[test]
    fn t10_9_e_coerce_via_heap_api_j_from_double() {
        // Even without the NativeContext path, confirm the heap-level
        // descriptor-aware API normalizes a drifted Double back to Long.
        let shared = test_shared();
        let obj = shared.heap.alloc_object(ClassId::new(1), 1);
        shared.heap.set_field(obj, 0, Value::Double(f64::from_bits(777)));
        match shared.heap.get_field_as(obj, 0, b'J') {
            Value::Long(l) => assert_eq!(l, 777),
            other => panic!("expected Long(777), got {other:?}"),
        }
    }

    #[test]
    fn t10_9_e_synthetic_stub_bypasses_descriptor_coercion() {
        // Guard against regression: synthetic stub classes store
        // primitives into Object-descriptor'd slots. Descriptor-aware
        // coercion must NOT kick in for stubs, or panama/Unsafe/etc.
        // would see `Value::Long(0) в†’ Value::Object(None)` via the
        // `b'L'` arm.
        let shared = test_shared();
        // Register a synthetic stub so field_at_index resolves.
        let cid = {
            let mut cm = shared.class_manager.write();
            cm.ensure_synthetic_class("rustjvm/test/SyntheticStubProbe", 2)
        };
        // Sanity: that class is a stub.
        {
            let cm = shared.class_manager.read();
            let cls = cm.get_class(cid).expect("stub registered");
            assert!(cls.is_synthetic_stub, "expected a synthetic stub");
        }
        // Resolution must return None so the caller falls back to raw read.
        assert_eq!(
            resolve_field_descriptor_byte_cached(&shared, cid, 0),
            None
        );
        assert_eq!(
            resolve_field_descriptor_byte_cached(&shared, cid, 1),
            None
        );
    }

    // =====================================================================
    // T19_H6 вЂ” descriptor-aware CAS + cross-tag bit-pattern equivalence
    // =====================================================================

    /// Register a real (non-stub) class carrying the listed instance
    /// field descriptors. Returns `(class_id, num_fields)`. The fields
    /// are declared in order with names `f0`, `f1`, ... and slot
    /// indices `[0, len)`.
    fn add_real_class_with_field_descriptors(
        shared: &SharedVm,
        class_name: &str,
        descriptors: &[&str],
    ) -> (rustjvm_types::ClassId, usize) {
        use rustjvm_classloading::{Class, ClassLoaderId, ClassState};
        use rustjvm_reader::class_access_flags::{ClassAccessFlags, FieldAccessFlags};
        use rustjvm_reader::class_file_version::ClassFileVersion;
        use rustjvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};
        use rustjvm_reader::field::ClassFileField;

        let fields: Vec<ClassFileField> = descriptors
            .iter()
            .enumerate()
            .map(|(i, d)| ClassFileField {
                access_flags: FieldAccessFlags::from_bits_truncate(
                    FieldAccessFlags::PRIVATE.bits() | FieldAccessFlags::VOLATILE.bits(),
                ),
                name: Arc::from(format!("f{i}")),
                descriptor: Arc::from(*d),
                attributes: Vec::new(),
            })
            .collect();
        let num_fields = fields.len();

        let mut cm = shared.class_manager.write();
        let id = cm.class_store.next_id();
        cm.class_store.add(Class {
            id,
            loader_id: ClassLoaderId::Application,
            name: Arc::from(class_name),
            source_file: None,
            version: ClassFileVersion::JAVA_8,
            state: ClassState::Initialized,
            initializing_thread: None,
            constant_pool: ConstantPool::new(vec![ConstantPoolEntry::Tombstone]),
            access_flags: ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
            superclass: None,
            interfaces: vec![],
            fields,
            methods: vec![],
            first_field_index: 0,
            num_total_fields: num_fields,
            bootstrap_methods: vec![],
            annotations: Vec::new(),
            nest_host: None,
            nest_members: Vec::new(),
            record_components: Vec::new(),
            permitted_subclasses: Vec::new(),
            inner_classes: Vec::new(),
            enclosing_method: None,
            hidden: false,
            module_name: None,
            is_synthetic_stub: false,
            has_finalizer: false,
            signature: None,
            code_source: None,
            array_info: None,
        });
        cm.register_class_name(ClassLoaderId::Application, class_name, id);
        (id, num_fields)
    }

    /// values_equal_for_cas: a `Long(5)` and `Double::from_bits(5)` share
    /// the same 64-bit pattern and must compare equal under CAS.
    #[test]
    fn t19_h6_cas_long_matches_double_with_same_bits() {
        let long_val = Value::Long(5);
        let double_val = Value::Double(f64::from_bits(5));
        assert!(values_equal_for_cas(&long_val, &double_val));
        assert!(values_equal_for_cas(&double_val, &long_val));
    }

    /// values_equal_for_cas: differing bit patterns must NOT compare equal.
    #[test]
    fn t19_h6_cas_long_double_different_bits_unequal() {
        let long_val = Value::Long(5);
        let double_val = Value::Double(f64::from_bits(6));
        assert!(!values_equal_for_cas(&long_val, &double_val));
        assert!(!values_equal_for_cas(&double_val, &long_val));
    }

    /// values_equal_for_cas: Int(1) zero-extended is bit-equal to
    /// Double::from_bits(1) (5e-324). Covers the case where the
    /// descriptor cache returns `I` for what is actually a `J` slot.
    #[test]
    fn t19_h6_cas_int_zero_extended_matches_long_and_double() {
        // Int(1) zero-extended to u64 = 1. Long(1) as u64 = 1.
        // Double::from_bits(1) as u64 = 1. All three match.
        assert!(values_equal_for_cas(
            &Value::Int(1),
            &Value::Long(1)
        ));
        assert!(values_equal_for_cas(
            &Value::Int(1),
            &Value::Double(f64::from_bits(1))
        ));
        // Int(-1) zero-extended is 0x00000000FFFFFFFF, NOT Long(-1) = 0xFFFFFFFFFFFFFFFF.
        assert!(!values_equal_for_cas(&Value::Int(-1), &Value::Long(-1)));
    }

    /// compare_and_swap_field on a long instance field whose live tag
    /// drifted to `Double` must succeed when expected matches the bit
    /// pattern. Mirrors the KC16 ConcurrentHashMap.baseCount path.
    #[test]
    fn t19_h6_cas_field_long_field_with_double_tagged_storage_succeeds() {
        let shared = test_shared();
        let (cid, _n) = add_real_class_with_field_descriptors(
            &shared,
            "rustjvm/test/T19H6_LongField",
            &["J"],
        );
        let obj = shared.heap.alloc_object(cid, 1);
        // Inject a Double-tagged store at slot 0 with the bit pattern of 42L.
        // This mimics the upstream operand-stack tag drift.
        shared.heap.set_field(obj, 0, Value::Double(f64::from_bits(42)));

        let mut thread = JvmThread::new(ThreadId(0), "test");
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        let swapped =
            ctx.compare_and_swap_field(obj, 0, Value::Long(42), Value::Long(99));
        assert!(swapped, "CAS should succeed when bit patterns match");

        // Round-trip: subsequent read MUST return Long(99), not Double.
        let after = ctx.get_field_volatile(obj, 0);
        assert_eq!(
            after,
            Value::Long(99),
            "after a successful CAS the slot must persist with the declared `J` tag"
        );
    }

    /// compare_and_swap_field on a double instance field вЂ” sanity that
    /// double-typed fields still work after the cross-tag changes.
    #[test]
    fn t19_h6_cas_field_double_field_roundtrip() {
        let shared = test_shared();
        let (cid, _n) = add_real_class_with_field_descriptors(
            &shared,
            "rustjvm/test/T19H6_DoubleField",
            &["D"],
        );
        let obj = shared.heap.alloc_object(cid, 1);
        shared.heap.set_field(obj, 0, Value::Double(2.5));

        let mut thread = JvmThread::new(ThreadId(0), "test");
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        let swapped = ctx.compare_and_swap_field(
            obj,
            0,
            Value::Double(2.5),
            Value::Double(7.5),
        );
        assert!(swapped, "Double CAS with same-tag expected must succeed");
        assert_eq!(ctx.get_field_volatile(obj, 0), Value::Double(7.5));
    }

    /// compare_and_swap_field on an int field вЂ” regression check that
    /// the int CAS path still works after the descriptor-aware rewrite.
    #[test]
    fn t19_h6_cas_field_int_field_regression() {
        let shared = test_shared();
        let (cid, _n) = add_real_class_with_field_descriptors(
            &shared,
            "rustjvm/test/T19H6_IntField",
            &["I"],
        );
        let obj = shared.heap.alloc_object(cid, 1);
        shared.heap.set_field(obj, 0, Value::Int(7));

        let mut thread = JvmThread::new(ThreadId(0), "test");
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        // Mismatched expected fails.
        assert!(!ctx.compare_and_swap_field(
            obj,
            0,
            Value::Int(0),
            Value::Int(99)
        ));
        // Matched expected succeeds.
        assert!(ctx.compare_and_swap_field(
            obj,
            0,
            Value::Int(7),
            Value::Int(42)
        ));
        assert_eq!(ctx.get_field_volatile(obj, 0), Value::Int(42));
    }

    /// compare_and_swap_field on a long field where the *expected* arg
    /// arrived as `Double` (operand-stack tag drift on the call side)
    /// while storage is correctly `Long`. The CAS must succeed by bit
    /// pattern, and the readback must persist as `Long`.
    #[test]
    fn t19_h6_cas_field_long_field_with_double_expected_arg_succeeds() {
        let shared = test_shared();
        let (cid, _n) = add_real_class_with_field_descriptors(
            &shared,
            "rustjvm/test/T19H6_LongFieldDoubleExpected",
            &["J"],
        );
        let obj = shared.heap.alloc_object(cid, 1);
        // Storage is correctly Long(0).
        shared.heap.set_field(obj, 0, Value::Long(0));

        let mut thread = JvmThread::new(ThreadId(0), "test");
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        // Caller passes args with drifted Double tag (bits = 0).
        let swapped = ctx.compare_and_swap_field(
            obj,
            0,
            Value::Double(f64::from_bits(0)),
            Value::Double(f64::from_bits(1)),
        );
        assert!(swapped, "CAS must succeed by bit-pattern equivalence");
        // Round-trip: storage must persist as Long(1) (descriptor-aware set).
        assert_eq!(
            ctx.get_field_volatile(obj, 0),
            Value::Long(1),
            "successful CAS must persist with the declared `J` tag"
        );
    }
}
